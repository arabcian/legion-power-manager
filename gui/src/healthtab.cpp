#include "healthtab.h"

#include "privileged.h"
#include "systools.h"
#include "theme.h"

#include <QDateTime>
#include <QDesktopServices>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QMessageBox>
#include <QTextStream>
#include <QUrl>
#include <ctime>
#include <QSocketNotifier>
#include <QTabWidget>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QPointer>
#include <QProcess>
#include <QPushButton>
#include <QTimer>
#include <QTreeWidget>
#include <QVBoxLayout>

#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <unistd.h>

static constexpr int DEBOUNCE_MS = 1500;     // one scan for a burst of kernel lines
static constexpr int POLL_ROOT_MS = 300000;  // restricted kernel log: pkexec, rarely
static constexpr int MAX_ROWS = 500;
static constexpr qint64 LOG_MAX_BYTES = 8 << 20;  // then health.log -> health.log.1

static QString helperPath() { return privileged::helperPath(QStringLiteral("tune-helper")); }

static const char *levelColor(const QString &l) {
    if (l == QLatin1String("critical")) return theme::DANGER;
    if (l == QLatin1String("error")) return theme::WARN;
    return theme::FG_DIM;
}

static QString uptime(quint64 us) {
    const quint64 s = us / 1000000;
    return QStringLiteral("%1:%2:%3").arg(s / 3600).arg(s / 60 % 60, 2, 10, QLatin1Char('0')).arg(s % 60, 2, 10, QLatin1Char('0'));
}

HealthTab::HealthTab(QWidget *parent) : QWidget(parent) {
    // Sub-tabs: the fault monitor, then one page per system tool (lazy).
    auto *outer = new QVBoxLayout(this);
    outer->setContentsMargins(0, 0, 0, 0);
    auto *sub = new QTabWidget;
    outer->addWidget(sub);
    auto *monitor = new QWidget;
    sub->addTab(monitor, QStringLiteral("Monitor"));
    systools::addPages(sub);
    auto *root = new QVBoxLayout(monitor);
    root->setContentsMargins(10, 8, 10, 8);
    root->setSpacing(6);

    auto *top = new QHBoxLayout;
    summary_ = new QLabel;
    summary_->setTextFormat(Qt::RichText);
    top->addWidget(summary_, 1);
    auto *bScan = new QPushButton(QStringLiteral("Scan now"));
    connect(bScan, &QPushButton::clicked, this, &HealthTab::scan);
    top->addWidget(bScan);
    root->addLayout(top);

    source_ = new QLabel;
    source_->setWordWrap(true);
    source_->setStyleSheet(QStringLiteral("color:%1").arg(theme::MUTED));
    root->addWidget(source_);

    list_ = new QTreeWidget;
    list_->setColumnCount(3);
    list_->setHeaderLabels({QStringLiteral("Uptime"), QStringLiteral("Kind"), QStringLiteral("Event")});
    list_->setRootIsDecorated(false);
    list_->setAlternatingRowColors(true);
    list_->header()->setSectionResizeMode(0, QHeaderView::ResizeToContents);
    list_->header()->setSectionResizeMode(1, QHeaderView::ResizeToContents);
    list_->header()->setStretchLastSection(true);
    root->addWidget(list_, 1);

    aer_ = new QLabel;
    aer_->setWordWrap(true);
    aer_->setTextFormat(Qt::RichText);
    root->addWidget(aer_);

    // Persistent log: every event, across boots, until cleared.
    auto *logRow = new QHBoxLayout;
    logInfo_ = new QLabel;
    logInfo_->setStyleSheet(QStringLiteral("color:%1").arg(theme::MUTED));
    logRow->addWidget(logInfo_, 1);
    auto *bOpen = new QPushButton(QStringLiteral("Open saved log"));
    connect(bOpen, &QPushButton::clicked, this, [] { QDesktopServices::openUrl(QUrl::fromLocalFile(logPath())); });
    logRow->addWidget(bOpen);
    auto *bClear = new QPushButton(QStringLiteral("Clear saved log"));
    connect(bClear, &QPushButton::clicked, this, [this] {
        if (QMessageBox::question(this, QStringLiteral("Clear saved log"),
                QStringLiteral("Delete every saved kernel error (all boots)?")) != QMessageBox::Yes) return;
        QFile::remove(logPath());
        QFile::remove(logPath() + QStringLiteral(".1"));
        // Keep savedSeq_: events already seen this boot are not written again.
        updateLogInfo();
    });
    logRow->addWidget(bClear);
    root->addLayout(logRow);

    auto *hint = new QLabel(QStringLiteral(
        "Xid 154 only names the recovery the GPU needs — the cause is the Xid logged just before it. "
        "Xid 13/31/43/45 are usually the application (or an unstable GPU curve); 62/79/119/120 are "
        "firmware or power level. Machine checks and lockups point at CPU Curve Optimizer or RAM timings. "
        "Hover a row for the full kernel line."));
    hint->setWordWrap(true);
    hint->setStyleSheet(QStringLiteral("color:%1").arg(theme::MUTED));
    root->addWidget(hint);

    debounce_ = new QTimer(this);
    debounce_->setSingleShot(true);
    debounce_->setInterval(DEBOUNCE_MS);
    connect(debounce_, &QTimer::timeout, this, &HealthTab::scan);
    timer_ = new QTimer(this);
    timer_->setTimerType(Qt::VeryCoarseTimer);
    connect(timer_, &QTimer::timeout, this, &HealthTab::scan);

    // Wake only when the kernel logs something. SEEK_END: the start-up scan
    // below covers the history; the notifier only sees new records.
    kmsgFd_ = ::open("/dev/kmsg", O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (kmsgFd_ >= 0) {
        ::lseek(kmsgFd_, 0, SEEK_END);
        auto *n = new QSocketNotifier(kmsgFd_, QSocketNotifier::Read, this);
        connect(n, &QSocketNotifier::activated, this, &HealthTab::drainKmsg);
    } else {
        needsRoot_ = true;
        timer_->start(POLL_ROOT_MS);
    }
    // Resume point: highest seq of the current boot already in the file, so a
    // restart does not write this boot's history twice.
    {
        QFile b(QStringLiteral("/proc/sys/kernel/random/boot_id"));
        if (b.open(QIODevice::ReadOnly)) savedBoot_ = QString::fromLatin1(b.readAll().trimmed());
        QFile f(logPath());
        const QString tag = QStringLiteral("boot=") + savedBoot_.left(8) + QStringLiteral(" seq=");
        if (!savedBoot_.isEmpty() && f.open(QIODevice::ReadOnly | QIODevice::Text)) {
            while (!f.atEnd()) {
                const QString l = QString::fromUtf8(f.readLine());
                const int i = l.indexOf(tag);
                if (i >= 0) savedSeq_ = std::max<quint64>(savedSeq_, l.mid(i + tag.size()).section(QLatin1Char(' '), 0, 0).toULongLong());
            }
        }
    }
    updateLogInfo();
    QTimer::singleShot(3000, this, &HealthTab::scan);
    updateSummary();
}

HealthTab::~HealthTab() { if (kmsgFd_ >= 0) ::close(kmsgFd_); }

void HealthTab::drainKmsg() {
    static const char *const KEYS[] = {"NVRM", "Hardware Error", "achine check", "achine Check", "AER",
                                       "PCIe Bus Error", "lockup", "LOCKUP", "stall", "amdgpu"};
    char buf[8192];
    bool relevant = false;
    for (;;) {
        const ssize_t n = ::read(kmsgFd_, buf, sizeof buf - 1);
        if (n < 0 && errno == EPIPE) continue;  // record overwritten meanwhile
        if (n <= 0) break;                      // EAGAIN: drained
        if (relevant) continue;
        buf[n] = 0;
        for (const char *k : KEYS) if (std::strstr(buf, k)) { relevant = true; break; }
    }
    if (relevant) debounce_->start();
}

void HealthTab::scan() {
    if (busy_) return;
    if (!QFileInfo(helperPath()).isExecutable()) {
        source_->setText(QStringLiteral("tune-helper is not installed."));
        return;
    }
    busy_ = true;
    const QJsonObject req{{QStringLiteral("op"), QStringLiteral("health")}, {QStringLiteral("since"), qint64(lastSeq_)}};
    if (needsRoot_) {
        privileged::run(helperPath(), req, this, [this](const privileged::Result &r) {
            busy_ = false;
            if (!r.reached) { source_->setText(QStringLiteral("Kernel log restricted and pkexec failed: ") + r.message()); return; }
            onReply(r.json, true);
        });
        return;
    }
    auto *p = new QProcess(this);
    QPointer<QProcess> guard(p);
    connect(p, &QProcess::finished, this, [this, p](int, QProcess::ExitStatus) {
        busy_ = false;
        const QByteArray out = p->readAllStandardOutput().trimmed();
        p->deleteLater();
        onReply(QJsonDocument::fromJson(out.mid(out.lastIndexOf('\n') + 1)).object(), false);
    });
    connect(p, &QProcess::errorOccurred, this, [this, p](QProcess::ProcessError e) {
        if (e != QProcess::FailedToStart) return;
        busy_ = false;
        p->deleteLater();
    });
    QTimer::singleShot(10000, p, [guard] { if (guard && guard->state() != QProcess::NotRunning) guard->kill(); });
    p->start(helperPath(), {});
    p->write(QJsonDocument(req).toJson(QJsonDocument::Compact));
    p->closeWriteChannel();
}

void HealthTab::onReply(const QJsonObject &r, bool viaRoot) {
    if (!r.value(QStringLiteral("ok")).toBool()) {
        if (r.value(QStringLiteral("needs_root")).toBool() && !needsRoot_) {
            needsRoot_ = true;
            timer_->start(POLL_ROOT_MS);
            QTimer::singleShot(0, this, &HealthTab::scan);
            return;
        }
        source_->setText(QStringLiteral("Scan failed: ") + r.value(QStringLiteral("error")).toString());
        return;
    }
    source_->setText(viaRoot
        ? QStringLiteral("Kernel log is restricted (kernel.dmesg_restrict=1): read through pkexec every %1 min. "
                         "sysctl kernel.dmesg_restrict=0 allows direct, faster checks.").arg(POLL_ROOT_MS / 60000)
        : QStringLiteral("Watching the kernel log directly (scans only when a relevant line appears)."));

    const QJsonArray ev = r.value(QStringLiteral("events")).toArray();
    int newBad = 0;
    QString firstBad;
    for (const QJsonValue &v : ev) {
        const QJsonObject e = v.toObject();
        addEvent(e);
        const QString lvl = e.value(QStringLiteral("level")).toString();
        if (lvl == QLatin1String("critical") || lvl == QLatin1String("error")) {
            if (!newBad++) firstBad = e.value(QStringLiteral("title")).toString();
        }
    }
    persist(ev, r.value(QStringLiteral("boot_id")).toString());
    lastSeq_ = std::max<quint64>(lastSeq_, quint64(r.value(QStringLiteral("last_seq")).toDouble()));

    if (newBad) {
        if (first_) Q_EMIT alert(QStringLiteral("Health"), QStringLiteral("%1 hardware/driver error(s) since boot — see the Health tab.").arg(newBad));
        else Q_EMIT alert(QStringLiteral("Health"), newBad == 1 ? firstBad : QStringLiteral("%1 new errors — %2").arg(newBad).arg(firstBad));
    }
    first_ = false;

    QStringList aer;
    for (const QJsonValue &v : r.value(QStringLiteral("aer")).toArray()) {
        const QJsonObject a = v.toObject();
        const qint64 bad = a.value(QStringLiteral("nonfatal")).toInteger() + a.value(QStringLiteral("fatal")).toInteger();
        aer << QStringLiteral("<span style='color:%1'>%2 %3: %4 corrected, %5 uncorrectable</span>")
                   .arg(QString::fromLatin1(bad ? theme::DANGER : theme::FG_DIM))
                   .arg(a.value(QStringLiteral("what")).toString())
                   .arg(a.value(QStringLiteral("dev")).toString())
                   .arg(a.value(QStringLiteral("cor")).toInteger()).arg(bad);
    }
    aer_->setText(aer.isEmpty() ? QStringLiteral("<span style='color:%1'>PCIe AER counters: all zero.</span>").arg(theme::MUTED)
                                : QStringLiteral("PCIe AER counters (since boot):<br>") + aer.join(QStringLiteral("<br>")));
    updateSummary();
}

void HealthTab::addEvent(const QJsonObject &e) {
    const QString kind = e.value(QStringLiteral("kind")).toString();
    QString title = e.value(QStringLiteral("title")).toString();
    if (kind == QLatin1String("xid")) {
        ++xid_;
        if (e.value(QStringLiteral("code")).toInt() == 154) {
            if (!lastRootXid_.isEmpty()) title += QStringLiteral("  (cause: ") + lastRootXid_ + QLatin1Char(')');
        } else {
            lastRootXid_ = title;
        }
    } else if (kind == QLatin1String("gsp")) ++gsp_;
    else if (kind == QLatin1String("mce")) ++mce_;
    else if (kind == QLatin1String("aer")) ++aerN_;
    else if (kind == QLatin1String("lockup")) ++lockup_;
    else ++other_;

    auto *it = new QTreeWidgetItem({uptime(quint64(e.value(QStringLiteral("ts_us")).toDouble())), kind.toUpper(), title});
    it->setToolTip(2, e.value(QStringLiteral("text")).toString());
    const QColor c(levelColor(e.value(QStringLiteral("level")).toString()));
    for (int i = 0; i < 3; ++i) it->setForeground(i, c);
    list_->insertTopLevelItem(0, it);  // newest first
    while (list_->topLevelItemCount() > MAX_ROWS) delete list_->takeTopLevelItem(list_->topLevelItemCount() - 1);
}

void HealthTab::updateSummary() {
    auto part = [](const char *name, int n) {
        return QStringLiteral("<span style='color:%1'><b>%2</b> %3</span>").arg(n ? theme::DANGER : theme::OK).arg(n).arg(name);
    };
    summary_->setText(QStringLiteral("Since boot: ") + QStringList{
        part("NVIDIA Xid", xid_), part("GSP timeout", gsp_), part("machine check", mce_),
        part("PCIe AER", aerN_), part("lockup", lockup_), part("other", other_)}.join(QStringLiteral(" &nbsp;·&nbsp; ")));
}

QString HealthTab::logPath() {
    QString dir = qEnvironmentVariable("XDG_STATE_HOME");  // GenericStateLocation needs Qt 6.7
    if (dir.isEmpty()) dir = QDir::homePath() + QStringLiteral("/.local/state");
    return dir + QStringLiteral("/legion-power-manager/health.log");
}

void HealthTab::persist(const QJsonArray &ev, const QString &bootId) {
    if (!bootId.isEmpty() && bootId != savedBoot_) { savedBoot_ = bootId; savedSeq_ = 0; }
    QStringList lines;
    // Kernel timestamps count from boot; wall time = now - uptime + ts.
    timespec now{};
    ::clock_gettime(CLOCK_MONOTONIC, &now);
    const qint64 bootMs = QDateTime::currentMSecsSinceEpoch() - (qint64(now.tv_sec) * 1000 + now.tv_nsec / 1000000);
    quint64 top = savedSeq_;
    for (const QJsonValue &v : ev) {
        const QJsonObject e = v.toObject();
        const quint64 seq = quint64(e.value(QStringLiteral("seq")).toDouble());
        if (seq <= savedSeq_) continue;
        top = std::max(top, seq);
        const qint64 tsMs = qint64(e.value(QStringLiteral("ts_us")).toDouble() / 1000);
        lines << QStringLiteral("%1  boot=%2 seq=%3  %4 %5  %6  |  %7")
                     .arg(QDateTime::fromMSecsSinceEpoch(bootMs + tsMs).toString(QStringLiteral("yyyy-MM-dd HH:mm:ss")))
                     .arg(savedBoot_.left(8)).arg(seq)
                     .arg(e.value(QStringLiteral("level")).toString().toUpper(), -8)
                     .arg(e.value(QStringLiteral("kind")).toString().toUpper(), -6)
                     .arg(e.value(QStringLiteral("title")).toString(), e.value(QStringLiteral("text")).toString());
    }
    if (lines.isEmpty()) return;
    const QString path = logPath();
    QDir().mkpath(QFileInfo(path).absolutePath());
    if (QFileInfo(path).size() > LOG_MAX_BYTES) { QFile::remove(path + QStringLiteral(".1")); QFile::rename(path, path + QStringLiteral(".1")); }
    QFile f(path);
    if (!f.open(QIODevice::Append | QIODevice::Text)) { logInfo_->setText(QStringLiteral("Saved log: cannot write ") + path); return; }
    QTextStream(&f) << lines.join(QLatin1Char('\n')) << '\n';
    savedSeq_ = top;
    updateLogInfo();
}

void HealthTab::updateLogInfo() {
    const QFileInfo a(logPath()), b(logPath() + QStringLiteral(".1"));
    const qint64 sz = (a.exists() ? a.size() : 0) + (b.exists() ? b.size() : 0);
    logInfo_->setText(sz ? QStringLiteral("Saved log (all boots): %1 · %2 KiB").arg(a.absoluteFilePath()).arg((sz + 1023) / 1024)
                         : QStringLiteral("Saved log: empty — new kernel errors are appended to %1").arg(a.absoluteFilePath()));
}
