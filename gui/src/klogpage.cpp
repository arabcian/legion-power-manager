#include "klogpage.h"

#include "privileged.h"
#include "theme.h"

#include <QApplication>
#include <QClipboard>
#include <QComboBox>
#include <QDateTime>
#include <QDesktopServices>
#include <QDir>
#include <QElapsedTimer>
#include <QFile>
#include <QFileInfo>
#include <QFontDatabase>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPointer>
#include <QProcess>
#include <QPushButton>
#include <QScrollBar>
#include <QSocketNotifier>
#include <QTextBrowser>
#include <QTextStream>
#include <QTimer>
#include <QUrl>
#include <QVBoxLayout>

#include <algorithm>
#include <cerrno>
#include <cstdlib>
#include <ctime>
#include <fcntl.h>
#include <memory>
#include <unistd.h>

static constexpr int DEBOUNCE_MS = 3000;
static constexpr qint64 LOG_MAX_BYTES = 16 << 20;  // then kernel.log -> kernel.log.1
static constexpr int MAX_SHOWN_LINES = 6000;
static constexpr qint64 TAIL_BYTES = 512 << 10;

static QString helperPath() { return privileged::helperPath(QStringLiteral("tune-profile-helper")); }

static const char *const LEVELS[] = {"EMERG", "ALERT", "CRIT", "ERR", "WARN"};

KlogPage::KlogPage(QWidget *parent) : QWidget(parent) {
    auto *v = new QVBoxLayout(this);
    v->setContentsMargins(8, 6, 8, 6);

    auto *top = new QHBoxLayout;
    auto *about = new QLabel(QStringLiteral("Kernel messages of level warning or worse, saved across reboots "
                                            "(the kernel's own ring buffer is lost at every boot)."));
    about->setWordWrap(true);
    about->setProperty("role", "muted");
    top->addWidget(about, 1);
    scope_ = new QComboBox;
    scope_->addItems({QStringLiteral("This boot"), QStringLiteral("All saved boots")});
    top->addWidget(scope_);
    filter_ = new QLineEdit;
    filter_->setPlaceholderText(QStringLiteral("Filter…"));
    filter_->setClearButtonEnabled(true);
    filter_->setMaximumWidth(200);
    top->addWidget(filter_);
    auto *bRefresh = new QPushButton(QStringLiteral("Refresh"));
    auto *bCopy = new QPushButton(QStringLiteral("Copy"));
    auto *bOpen = new QPushButton(QStringLiteral("Open saved log"));
    auto *bClear = new QPushButton(QStringLiteral("Clear saved log"));
    bClear->setObjectName(QStringLiteral("btnDanger"));
    for (auto *b : {bRefresh, bCopy, bOpen, bClear}) top->addWidget(b);
    v->addLayout(top);

    text_ = new QTextBrowser;
    text_->setLineWrapMode(QTextEdit::NoWrap);
    text_->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    v->addWidget(text_, 1);
    info_ = new QLabel;
    info_->setWordWrap(true);
    info_->setProperty("role", "muted");
    v->addWidget(info_);

    connect(scope_, &QComboBox::currentIndexChanged, this, [this] { render(); });
    connect(filter_, &QLineEdit::textChanged, this, [this] { render(); });
    connect(bRefresh, &QPushButton::clicked, this, &KlogPage::capture);
    connect(bCopy, &QPushButton::clicked, this, [this] { QApplication::clipboard()->setText(text_->toPlainText()); });
    connect(bOpen, &QPushButton::clicked, this, [] { QDesktopServices::openUrl(QUrl::fromLocalFile(logPath())); });
    connect(bClear, &QPushButton::clicked, this, [this] {
        if (QMessageBox::question(this, QStringLiteral("Clear saved log"),
                QStringLiteral("Delete the saved kernel log of every boot?")) != QMessageBox::Yes) return;
        QFile::remove(logPath());
        QFile::remove(logPath() + QStringLiteral(".1"));
        // Keep savedSeq_: what this boot already showed is not written again.
        render();
    });

    QFile b(QStringLiteral("/proc/sys/kernel/random/boot_id"));
    if (b.open(QIODevice::ReadOnly)) bootId_ = QString::fromLatin1(b.readAll().trimmed());

    // Resume point: highest seq of this boot in the tail of the file, so a
    // restart of the app does not write the same records twice.
    {
        QFile f(logPath());
        const QString tag = QStringLiteral("boot=") + bootId_.left(8) + QStringLiteral(" seq=");
        if (!bootId_.isEmpty() && f.open(QIODevice::ReadOnly | QIODevice::Text)) {
            if (f.size() > TAIL_BYTES) { f.seek(f.size() - TAIL_BYTES); f.readLine(); }
            while (!f.atEnd()) {
                const QString l = QString::fromUtf8(f.readLine());
                const int i = l.indexOf(tag);
                if (i >= 0) savedSeq_ = std::max<quint64>(savedSeq_, l.mid(i + tag.size()).section(QLatin1Char(' '), 0, 0).toULongLong());
            }
        }
        lastSeq_ = savedSeq_;
    }

    debounce_ = new QTimer(this);
    debounce_->setSingleShot(true);
    debounce_->setInterval(DEBOUNCE_MS);
    connect(debounce_, &QTimer::timeout, this, &KlogPage::capture);

    // SEEK_END: the start-up capture below reads the history; the notifier
    // only wakes for new records.
    kmsgFd_ = ::open("/dev/kmsg", O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (kmsgFd_ >= 0) {
        ::lseek(kmsgFd_, 0, SEEK_END);
        auto *n = new QSocketNotifier(kmsgFd_, QSocketNotifier::Read, this);
        auto rate = std::make_shared<std::pair<QElapsedTimer, int>>();
        rate->first.start();
        connect(n, &QSocketNotifier::activated, this, [this, n, rate] {
            drainKmsg();
            if (rate->first.elapsed() > 1000) { rate->first.restart(); rate->second = 0; }
            if (++rate->second > 50) {  // flood: pause, capture once, resume later
                n->setEnabled(false);
                debounce_->start();
                QTimer::singleShot(5000, n, [this, n, rate] { drainKmsg(); rate->first.restart(); rate->second = 0; n->setEnabled(true); });
            }
        });
        QTimer::singleShot(4000, this, &KlogPage::capture);  // history of this boot, once
    } else {
        needsRoot_ = true;  // restricted: read on demand only (a pkexec prompt is never a background event)
    }
    updateInfo();
    render();
}

KlogPage::~KlogPage() { if (kmsgFd_ >= 0) ::close(kmsgFd_); }

void KlogPage::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    if (!shownOnce_) { shownOnce_ = true; if (needsRoot_) capture(); }
    render();
}

void KlogPage::drainKmsg() {
    char buf[8192];
    bool relevant = false;
    for (int i = 0; i < 512; ++i) {  // bounded: a flood must not pin the GUI thread
        const ssize_t n = ::read(kmsgFd_, buf, sizeof buf - 1);
        if (n < 0 && errno == EPIPE) continue;
        if (n <= 0) break;
        if (relevant) continue;
        buf[n] = 0;
        char *end = nullptr;
        const long pri = std::strtol(buf, &end, 10);
        if (end != buf && (pri & 7) <= 4) relevant = true;
    }
    if (relevant) debounce_->start();
}

void KlogPage::capture() {
    if (busy_) return;
    if (!QFileInfo(helperPath()).isExecutable()) { info_->setText(QStringLiteral("tune-helper is not installed.")); return; }
    busy_ = true;
    const QJsonObject req{{QStringLiteral("op"), QStringLiteral("klog")}, {QStringLiteral("since"), qint64(lastSeq_)}};
    if (needsRoot_) {
        privileged::run(helperPath(), req, this, [this](const privileged::Result &r) {
            busy_ = false;
            if (!r.reached) { info_->setText(QStringLiteral("Kernel log restricted and pkexec failed: ") + r.message()); return; }
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

void KlogPage::onReply(const QJsonObject &r, bool viaRoot) {
    if (!r.value(QStringLiteral("ok")).toBool()) {
        if (r.value(QStringLiteral("needs_root")).toBool() && !needsRoot_) { needsRoot_ = true; updateInfo(); return; }
        info_->setText(QStringLiteral("Capture failed: ") + r.value(QStringLiteral("error")).toString());
        return;
    }
    persist(r.value(QStringLiteral("records")).toArray(), r.value(QStringLiteral("boot_id")).toString());
    lastSeq_ = std::max<quint64>(lastSeq_, quint64(r.value(QStringLiteral("last_seq")).toDouble()));
    Q_UNUSED(viaRoot);
    updateInfo();
    render();
}

QString KlogPage::logPath() {
    QString dir = qEnvironmentVariable("XDG_STATE_HOME");  // GenericStateLocation needs Qt 6.7
    if (dir.isEmpty()) dir = QDir::homePath() + QStringLiteral("/.local/state");
    return dir + QStringLiteral("/legion-power-manager/kernel.log");
}

void KlogPage::persist(const QJsonArray &records, const QString &bootId) {
    if (!bootId.isEmpty() && bootId != bootId_) { bootId_ = bootId; savedSeq_ = 0; }
    timespec now{};
    ::clock_gettime(CLOCK_MONOTONIC, &now);  // kernel timestamps count from boot
    const qint64 bootMs = QDateTime::currentMSecsSinceEpoch() - (qint64(now.tv_sec) * 1000 + now.tv_nsec / 1000000);
    QStringList lines;
    quint64 top = savedSeq_;
    for (const QJsonValue &rv : records) {
        const QJsonObject o = rv.toObject();
        const quint64 seq = quint64(o.value(QStringLiteral("seq")).toDouble());
        if (seq <= savedSeq_) continue;
        top = std::max(top, seq);
        const int lvl = std::clamp(o.value(QStringLiteral("level")).toInt(4), 0, 4);
        const qint64 tsMs = qint64(o.value(QStringLiteral("ts_us")).toDouble() / 1000);
        lines << QStringLiteral("%1  boot=%2 seq=%3  %4  [%5.%6] %7")
                     .arg(QDateTime::fromMSecsSinceEpoch(bootMs + tsMs).toString(QStringLiteral("yyyy-MM-dd HH:mm:ss")))
                     .arg(bootId_.left(8)).arg(seq)
                     .arg(QString::fromLatin1(LEVELS[lvl]), -5)
                     .arg(tsMs / 1000).arg((tsMs % 1000) * 1000, 6, 10, QLatin1Char('0'))
                     .arg(o.value(QStringLiteral("msg")).toString());
    }
    if (lines.isEmpty()) return;
    const QString path = logPath();
    QDir().mkpath(QFileInfo(path).absolutePath());
    if (QFileInfo(path).size() > LOG_MAX_BYTES) { QFile::remove(path + QStringLiteral(".1")); QFile::rename(path, path + QStringLiteral(".1")); }
    QFile f(path);
    if (!f.open(QIODevice::Append | QIODevice::Text)) { info_->setText(QStringLiteral("Cannot write ") + path); return; }
    QTextStream(&f) << lines.join(QLatin1Char('\n')) << '\n';
    savedSeq_ = top;
}

void KlogPage::updateInfo() {
    const QFileInfo a(logPath()), b(logPath() + QStringLiteral(".1"));
    const qint64 sz = (a.exists() ? a.size() : 0) + (b.exists() ? b.size() : 0);
    QString s = QStringLiteral("Saved log: %1 · %2 KiB").arg(a.absoluteFilePath()).arg((sz + 1023) / 1024);
    if (needsRoot_) s += QStringLiteral("\nKernel log is restricted (kernel.dmesg_restrict=1): captured through pkexec when this page is opened or on Refresh, "
                                        "not in the background. sysctl kernel.dmesg_restrict=0 enables continuous capture.");
    else s += QStringLiteral("\nCapturing continuously (a scan runs only when the kernel logs a warning or error).");
    info_->setText(s);
}

void KlogPage::render() {
    const bool all = scope_->currentIndex() == 1;
    const QString tag = QStringLiteral("boot=") + bootId_.left(8) + QLatin1Char(' ');
    const QString needle = filter_->text();
    QStringList lines;
    for (const QString &path : {logPath() + QStringLiteral(".1"), logPath()}) {
        QFile f(path);
        if (!f.open(QIODevice::ReadOnly | QIODevice::Text)) continue;
        while (!f.atEnd()) {
            const QString l = QString::fromUtf8(f.readLine()).trimmed();
            if (l.isEmpty()) continue;
            if (!all && !l.contains(tag)) continue;
            if (!needle.isEmpty() && !l.contains(needle, Qt::CaseInsensitive)) continue;
            lines << l;
            if (lines.size() > MAX_SHOWN_LINES) lines.removeFirst();
        }
    }
    QString html = QStringLiteral("<pre style='margin:0'>");
    for (const QString &l : lines) {
        const char *c = l.contains(QLatin1String("  EMERG ")) || l.contains(QLatin1String("  ALERT ")) || l.contains(QLatin1String("  CRIT  ")) ? theme::DANGER
                        : l.contains(QLatin1String("  ERR   ")) ? theme::WARN : theme::FG_DIM;
        html += QStringLiteral("<span style='color:%1'>%2</span>\n").arg(QString::fromLatin1(c), l.toHtmlEscaped());
    }
    html += QStringLiteral("</pre>");
    const int vs = text_->verticalScrollBar()->value();
    const bool atEnd = vs >= text_->verticalScrollBar()->maximum() - 4;
    text_->setHtml(lines.isEmpty() ? QStringLiteral("<i>No saved kernel warnings or errors%1.</i>").arg(all ? QString() : QStringLiteral(" for this boot")) : html);
    text_->verticalScrollBar()->setValue(atEnd || vs == 0 ? text_->verticalScrollBar()->maximum() : vs);
}
