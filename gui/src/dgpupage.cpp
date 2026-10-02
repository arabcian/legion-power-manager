#include "dgpupage.h"
#include "theme.h"

#include <QClipboard>
#include <QCoreApplication>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QHideEvent>
#include <QLabel>
#include <QPointer>
#include <QPushButton>
#include <QSet>
#include <QShowEvent>
#include <QThreadPool>
#include <QTimer>
#include <QTreeWidget>
#include <QVBoxLayout>

#include <unistd.h>

static constexpr int POLL_MS = 2000, HOLDER_EVERY = 5;  // open-handle scan every 10 s

using Row = DgpuPage::Row;
using Report = DgpuPage::Report;

static const QString PCI = QStringLiteral("/sys/bus/pci/devices");

/// Whole (small) sysfs/procfs file, trimmed; empty when unreadable.
static QString rd(const QString &p) {
    QFile f(p);
    if (!f.open(QIODevice::ReadOnly)) return {};
    return QString::fromUtf8(f.read(64 * 1024)).trimmed();
}

/// PCI address of the NVIDIA display function, or empty.
static QString gpuAddr() {
    const QDir d(PCI);
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name))
        if (rd(d.filePath(e) + "/vendor") == QLatin1String("0x10de") && rd(d.filePath(e) + "/class").startsWith(QLatin1String("0x03")))
            return e;
    return {};
}

bool DgpuPage::present() { return !gpuAddr().isEmpty(); }

static QString fmtMs(qint64 ms) {
    const qint64 s = ms / 1000;
    if (s >= 3600) return QStringLiteral("%1h %2m").arg(s / 3600).arg(s / 60 % 60, 2, 10, QLatin1Char('0'));
    return QStringLiteral("%1m %2s").arg(s / 60).arg(s % 60, 2, 10, QLatin1Char('0'));
}

static QString className(const QString &cls) {
    if (cls.startsWith(QLatin1String("0x0403"))) return QStringLiteral("HDMI/DP audio");
    if (cls.startsWith(QLatin1String("0x0c03"))) return QStringLiteral("USB controller");
    if (cls.startsWith(QLatin1String("0x0c80"))) return QStringLiteral("USB-C (UCSI)");
    return QStringLiteral("class ") + cls;
}

static QString driverOf(const QString &dev) {
    const QString t = QFileInfo(dev + "/driver").symLinkTarget();
    return t.isEmpty() ? QStringLiteral("no driver") : t.section('/', -1);
}

/// Runtime-PM / ACPI rows of one PCI function. Returns its runtime_status.
static void pmRows(const QString &section, const QString &dev, QList<Row> &rows) {
    const QString st = rd(dev + "/power/runtime_status");
    rows.append({section, QStringLiteral("Runtime status"), st.isEmpty() ? QStringLiteral("—") : st, st == QLatin1String("suspended") ? 1 : 0});
    if (const QString ps = rd(dev + "/power_state"); !ps.isEmpty())
        rows.append({section, QStringLiteral("PCI power state"), ps, ps == QLatin1String("D3cold") ? 1 : 0});
    const QString ctl = rd(dev + "/power/control");
    rows.append({section, QStringLiteral("Runtime PM (power/control)"),
                 ctl == QLatin1String("auto") ? ctl : ctl + QStringLiteral("  — must be \"auto\" for the device to sleep"),
                 ctl == QLatin1String("auto") ? 1 : 2});
    if (const QString d3 = rd(dev + "/d3cold_allowed"); !d3.isEmpty())
        rows.append({section, QStringLiteral("D3cold allowed"), d3 == QLatin1String("1") ? QStringLiteral("yes") : QStringLiteral("no"),
                     d3 == QLatin1String("1") ? 1 : 2});
    bool okA = false, okS = false;
    const qint64 act = rd(dev + "/power/runtime_active_time").toLongLong(&okA), sus = rd(dev + "/power/runtime_suspended_time").toLongLong(&okS);
    if (okA && okS && act + sus > 0)
        rows.append({section, QStringLiteral("Asleep / awake"),
                     QStringLiteral("%1 / %2  (%3 % asleep)").arg(fmtMs(sus), fmtMs(act)).arg(100 * sus / (act + sus)), 0});
    const QString fw = dev + "/firmware_node";
    if (QFileInfo::exists(fw)) {
        const QString ap = rd(fw + "/path"), aps = rd(fw + "/power_state"), real = rd(fw + "/real_power_state");
        if (!aps.isEmpty())
            rows.append({section, QStringLiteral("ACPI power state"),
                         aps + (real.isEmpty() || real == aps ? QString() : QStringLiteral("  (real: %1)").arg(real))
                             + (ap.isEmpty() ? QString() : QStringLiteral("   ") + ap), 0});
        // Power resources the device needs in D0 (the rail that D3cold switches off).
        const QDir pr(fw + "/power_resources_D0");
        for (const QString &r : pr.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name)) {
            const QString inUse = rd(pr.filePath(r) + "/resource_in_use");
            const QString name = rd(pr.filePath(r) + "/path");
            rows.append({section, QStringLiteral("Power resource ") + (name.isEmpty() ? r : name),
                         inUse.isEmpty() ? QStringLiteral("—") : inUse == QLatin1String("0") ? QStringLiteral("off") : QStringLiteral("on"), 0});
        }
    }
}

/// The user's own processes holding /dev/nvidia* or the card's DRM nodes open.
/// (Other users' /proc/<pid>/fd are not readable without root.)
static QList<Row> scanHolders(const QString &dev) {
    const QString section = QStringLiteral("Open handles (your processes)");
    QSet<QString> nodes;
    for (const QString &n : QDir(dev + "/drm").entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System))
        nodes.insert(QStringLiteral("/dev/dri/") + n);
    QList<Row> out;
    const uint me = getuid();
    const QDir proc(QStringLiteral("/proc"));
    char buf[256];
    for (const QString &pid : proc.entryList(QDir::Dirs | QDir::NoDotAndDotDot, QDir::NoSort)) {
        if (!pid.at(0).isDigit()) continue;
        const QString base = QStringLiteral("/proc/") + pid;
        if (QFileInfo(base).ownerId() != me) continue;
        QStringList held;
        const QDir fds(base + QStringLiteral("/fd"));
        for (const QString &fd : fds.entryList(QDir::Files | QDir::System | QDir::NoDotAndDotDot, QDir::NoSort)) {
            const ssize_t n = ::readlink(QFile::encodeName(fds.filePath(fd)).constData(), buf, sizeof buf - 1);
            if (n <= 0) continue;
            const QString t = QString::fromUtf8(buf, int(n));
            if (!t.startsWith(QLatin1String("/dev/nvidia")) && !nodes.contains(t)) continue;
            if (const QString b = t.section('/', -1); !held.contains(b)) held << b;
        }
        if (held.isEmpty()) continue;
        held.sort();
        out.append({section, QStringLiteral("%1 (%2)").arg(rd(base + "/comm"), pid), held.join(QStringLiteral(", ")), 0});
    }
    if (out.isEmpty()) out.append({section, QStringLiteral("none"), QStringLiteral("no process of yours has the NVIDIA device open"), 1});
    out.append({section, QStringLiteral("note"), QStringLiteral("root-owned processes (nvidia-powerd, display manager) are not visible without root"), 0});
    return out;
}

static Report gather(bool withHolders, QList<Row> holders) {
    Report r;
    const QString addr = gpuAddr();
    if (addr.isEmpty()) {
        r.verdict = QStringLiteral("NVIDIA dGPU is not on the PCI bus (iGPU-only mode, or powered off by the firmware).");
        return r;
    }
    const QString dev = QFileInfo(PCI + '/' + addr).canonicalFilePath();
    const QString gpuSec = QStringLiteral("GPU  ") + addr;
    pmRows(gpuSec, dev, r.rows);
    const QString status = rd(dev + "/power/runtime_status"), pstate = rd(dev + "/power_state"), ctl = rd(dev + "/power/control");
    QStringList blockers;
    if (ctl != QLatin1String("auto")) blockers << QStringLiteral("GPU power/control = %1 (runtime PM off — udev rule missing?)").arg(ctl);
    if (rd(dev + "/d3cold_allowed") == QLatin1String("0")) blockers << QStringLiteral("d3cold_allowed = 0 on the GPU");

    // Other functions of the same card: all of them must be able to suspend.
    const QString slot = addr.section('.', 0, 0) + '.';
    const QString sibSec = QStringLiteral("Sibling functions");
    for (const QString &e : QDir(PCI).entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name)) {
        if (!e.startsWith(slot) || e == addr) continue;
        const QString p = PCI + '/' + e, drv = driverOf(p), c = rd(p + "/power/control"), st = rd(p + "/power/runtime_status");
        const bool blocks = c != QLatin1String("auto") && QFileInfo::exists(p + "/driver");
        r.rows.append({sibSec, e + QStringLiteral("  ") + className(rd(p + "/class")),
                       QStringLiteral("%1  ·  control %2  ·  %3").arg(drv, c, st), blocks ? 2 : 0});
        if (blocks) blockers << QStringLiteral("%1 (%2) power/control = %3").arg(e, drv, c);
    }

    // Upstream port: D3cold is entered by powering the slot off through it.
    const QString port = QFileInfo(dev + "/..").canonicalFilePath();
    if (QFileInfo::exists(port + "/vendor")) {
        pmRows(QStringLiteral("Upstream port  ") + port.section('/', -1), port, r.rows);
        if (rd(port + "/power/control") != QLatin1String("auto")) blockers << QStringLiteral("upstream port power/control is not auto");
        if (rd(port + "/d3cold_allowed") == QLatin1String("0")) blockers << QStringLiteral("d3cold_allowed = 0 on the upstream port");
    }

    // Driver side.
    const QString drvSec = QStringLiteral("NVIDIA driver");
    const QString nv = QStringLiteral("/proc/driver/nvidia");
    if (!QFileInfo::exists(nv)) {
        r.rows.append({drvSec, QStringLiteral("Driver"), driverOf(dev) + QStringLiteral("  (nvidia module not loaded)"), 2});
    } else {
        for (const QString &l : rd(nv + "/version").split('\n'))
            if (l.startsWith(QLatin1String("NVRM version:"))) r.rows.append({drvSec, QStringLiteral("Version"), l.mid(13).simplified(), 0});
        for (const QString &l : rd(nv + "/gpus/" + addr + "/power").split('\n')) {
            const int i = l.indexOf(':');
            if (i <= 0) continue;
            const QString k = l.left(i).simplified(), v = l.mid(i + 1).simplified();
            if (v.isEmpty()) continue;
            int lvl = 0;
            if (k.startsWith(QLatin1String("Runtime D3"))) {
                lvl = v.startsWith(QLatin1String("Enabled")) ? 1 : 2;
                if (lvl == 2) blockers << QStringLiteral("driver reports Runtime D3 status: %1").arg(v);
            }
            r.rows.append({drvSec, k, v, lvl});
        }
        static const char *const PARAMS[] = {"DynamicPowerManagement", "DynamicPowerManagementVideoMemoryThreshold",
            "EnableS0ixPowerManagement", "S0ixPowerManagementVideoMemoryThreshold", "EnableGpuFirmware", "PreserveVideoMemoryAllocations"};
        const QStringList params = rd(nv + "/params").split('\n');
        for (const char *want : PARAMS)
            for (const QString &l : params) {
                if (l.section(':', 0, 0).trimmed() != QLatin1String(want)) continue;
                QString v = l.section(':', 1).trimmed();
                int lvl = 0;
                if (qstrcmp(want, "DynamicPowerManagement") == 0) {
                    if (v == QLatin1String("0")) { v += QStringLiteral("  — off: the GPU never sleeps"); lvl = 2; blockers << QStringLiteral("NVreg_DynamicPowerManagement=0"); }
                    else if (v == QLatin1String("1")) v += QStringLiteral("  — coarse: sleeps only while nothing has the GPU open");
                    else if (v == QLatin1String("2")) { v += QStringLiteral("  — fine-grained"); lvl = 1; }
                    else if (v == QLatin1String("3")) { v += QStringLiteral("  — driver default (fine-grained on supported notebooks)"); lvl = 1; }
                }
                r.rows.append({drvSec, QStringLiteral("NVreg_") + QLatin1String(want), v, lvl});
            }
        QStringList mods;
        for (const QString &l : rd(QStringLiteral("/proc/modules")).split('\n'))
            if (l.startsWith(QLatin1String("nvidia")))
                mods << QStringLiteral("%1 (%2)").arg(l.section(' ', 0, 0), l.section(' ', 2, 2));
        if (!mods.isEmpty()) r.rows.append({drvSec, QStringLiteral("Modules (use count)"), mods.join(QStringLiteral("  ·  ")), 0});
    }

    r.holders = withHolders ? scanHolders(dev) : holders;
    r.rows += r.holders;
    QStringList who;
    for (const Row &h : std::as_const(r.holders))
        if (h.level == 0 && h.key != QLatin1String("note")) who << h.key.section(' ', 0, 0);

    // dGPU-only (MUX): no other display function → the GPU drives the panel.
    bool igpu = false;
    for (const QString &e : QDir(PCI).entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System))
        if (rd(PCI + '/' + e + "/class").startsWith(QLatin1String("0x03")) && rd(PCI + '/' + e + "/vendor") != QLatin1String("0x10de")) igpu = true;

    if (status == QLatin1String("suspended")) {
        if (pstate.isEmpty() || pstate == QLatin1String("D3cold")) {
            r.level = 1;
            r.verdict = pstate.isEmpty() ? QStringLiteral("Asleep (runtime suspended).") : QStringLiteral("Asleep — D3cold: the GPU is powered off.");
        } else {
            r.level = 2;
            r.verdict = QStringLiteral("Runtime-suspended, but only in %1 — the power rail is still on.").arg(pstate);
            if (!blockers.isEmpty()) r.verdict += QStringLiteral(" Check: ") + blockers.join(QStringLiteral("; ")) + '.';
        }
    } else if (status == QLatin1String("active")) {
        if (!igpu) {
            r.verdict = QStringLiteral("Awake — dGPU-only (MUX) mode: the GPU drives the display and cannot sleep.");
        } else if (!blockers.isEmpty()) {
            r.level = 2;
            r.verdict = QStringLiteral("Awake — it cannot sleep: ") + blockers.join(QStringLiteral("; ")) + '.';
        } else {
            r.verdict = QStringLiteral("Awake — no setting blocks sleep; it is in use, or its idle timer is still running.");
        }
        if (igpu && !who.isEmpty()) r.verdict += QStringLiteral(" Open by: ") + who.join(QStringLiteral(", ")) + '.';
    } else {
        r.level = 2;
        r.verdict = QStringLiteral("Runtime PM state: %1.").arg(status.isEmpty() ? QStringLiteral("unknown") : status);
        if (status == QLatin1String("error")) r.verdict += QStringLiteral(" A suspend/resume of the GPU failed; it stays like this until the driver is rebound or the machine reboots.");
    }
    return r;
}

static QString reportText(const Report &r) {
    QString t = r.verdict + '\n', sec;
    for (const Row &row : r.rows) {
        if (row.section != sec) { sec = row.section; t += QStringLiteral("\n[") + sec + QStringLiteral("]\n"); }
        t += row.key + QStringLiteral(": ") + row.value + '\n';
    }
    return t;
}

DgpuPage::DgpuPage(QWidget *parent) : QWidget(parent) {
    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(10, 8, 10, 8);
    root->setSpacing(6);

    auto *top = new QHBoxLayout;
    verdict_ = new QLabel(QStringLiteral("…"));
    verdict_->setWordWrap(true);
    top->addWidget(verdict_, 1);
    auto *bRefresh = new QPushButton(QStringLiteral("Refresh"));
    connect(bRefresh, &QPushButton::clicked, this, [this] { refresh(true); });
    top->addWidget(bRefresh, 0, Qt::AlignTop);
    auto *bCopy = new QPushButton(QStringLiteral("Copy report"));
    connect(bCopy, &QPushButton::clicked, this, [this] { QGuiApplication::clipboard()->setText(shown_); });
    top->addWidget(bCopy, 0, Qt::AlignTop);
    root->addLayout(top);

    tree_ = new QTreeWidget;
    tree_->setColumnCount(2);
    tree_->setHeaderHidden(true);
    tree_->setAlternatingRowColors(true);
    tree_->header()->setSectionResizeMode(0, QHeaderView::ResizeToContents);
    tree_->header()->setStretchLastSection(true);
    root->addWidget(tree_, 1);

    auto *hint = new QLabel(QStringLiteral(
        "Read-only sysfs/procfs: looking at this page does not wake the GPU. For D3cold the GPU, every sibling function and "
        "the upstream port need power/control = auto and d3cold_allowed, and the driver needs NVreg_DynamicPowerManagement "
        "2 or 3. With all of that in place a GPU that stays awake is being used — the open-handles list shows by whom."));
    hint->setWordWrap(true);
    hint->setStyleSheet(QStringLiteral("color:%1").arg(theme::MUTED));
    root->addWidget(hint);

    timer_ = new QTimer(this);
    timer_->setInterval(POLL_MS);
    connect(timer_, &QTimer::timeout, this, [this] { ++tick_; refresh(tick_ % HOLDER_EVERY == 0); });
}

void DgpuPage::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    tick_ = 0;
    refresh(true);
    timer_->start();
}

void DgpuPage::hideEvent(QHideEvent *e) {
    QWidget::hideEvent(e);
    timer_->stop();
}

void DgpuPage::refresh(bool scanHolders) {
    if (busy_) return;  // never stack sweeps
    busy_ = true;
    QPointer<DgpuPage> self(this);
    const QList<Row> prev = holders_;
    QThreadPool::globalInstance()->start([self, scanHolders, prev] {
        const Report r = gather(scanHolders || prev.isEmpty(), prev);
        QMetaObject::invokeMethod(QCoreApplication::instance(), [self, r] {
            if (!self) return;
            self->busy_ = false;
            self->apply(r);
        }, Qt::QueuedConnection);
    });
}

void DgpuPage::apply(const Report &r) {
    holders_ = r.holders;
    const QString text = reportText(r);
    if (text == shown_) return;
    shown_ = text;
    verdict_->setText(r.verdict);
    theme::setSheet(verdict_, QStringLiteral("font-weight:600; color:%1;").arg(r.level == 1 ? theme::OK : r.level == 2 ? theme::WARN : theme::FG_DIM));
    tree_->clear();
    QTreeWidgetItem *sec = nullptr;
    QString name;
    for (const Row &row : r.rows) {
        if (!sec || row.section != name) {
            name = row.section;
            sec = new QTreeWidgetItem(tree_, {name});
            sec->setFirstColumnSpanned(true);
            QFont f = sec->font(0);
            f.setBold(true);
            sec->setFont(0, f);
        }
        auto *it = new QTreeWidgetItem(sec, {row.key, row.value});
        it->setToolTip(1, row.value);
        if (row.level) it->setForeground(1, QColor(QLatin1String(row.level == 1 ? theme::OK : theme::WARN)));
    }
    tree_->expandAll();
}
