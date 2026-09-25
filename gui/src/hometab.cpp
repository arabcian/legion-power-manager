#include "hometab.h"
#include "fancurvedialog.h"
#include "memorydialog.h"
#include "scenes.h"
#include "privileged.h"
#include "sysinfo.h"
#include "theme.h"

#include <QButtonGroup>
#include <QCoreApplication>
#include <QElapsedTimer>
#include <QFile>
#include <QPointer>
#include <QThreadPool>
#include <QGridLayout>
#include <QAbstractItemView>
#include <QCheckBox>
#include <QFileInfo>
#include <QComboBox>
#include <QDir>
#include <QGroupBox>
#include <QIntValidator>
#include <QJsonObject>
#include <QLineEdit>
#include <QSignalBlocker>
#include <QHBoxLayout>
#include <QJsonObject>
#include <QLabel>
#include <QFrame>
#include <QMessageBox>
#include <QProcess>
#include <QPushButton>
#include <QStandardPaths>
#include <QTimer>
#include <QShowEvent>
#include <QVBoxLayout>

static constexpr int POLL_MS = 2500, LIVE_POLL_MS = 2000, SMI_TIMEOUT_MS = 3000;
// nvidia-smi while the dGPU idles: every query resets the driver's idle timer,
// so polling it every 2 s kept the GPU out of D3cold for as long as the Home
// tab was open. Idle → one query every 15 s, long enough for it to suspend.
static constexpr int SMI_IDLE_MS = 15000;

/// Monotonic milliseconds (QDeadlineTimer-free, works on Qt 6.4).
static qint64 monoMs() {
    static QElapsedTimer t;
    if (!t.isValid()) t.start();
    return t.elapsed();
}

/// NVIDIA display-class PCI function (sysfs dir), or empty.
static QString nvidiaPciDir() {
    const QDir d(QStringLiteral("/sys/bus/pci/devices"));
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name)) {
        const QString p = d.filePath(e);
        if (pp::readText(p + "/vendor").value_or(QString()) == QLatin1String("0x10de")
            && pp::readText(p + "/class").value_or(QString()).startsWith(QLatin1String("0x03")))
            return p;
    }
    return {};
}

/// GPU model from the driver's procfs node — reading it does not touch the
/// hardware, unlike nvidia-smi, which powers the dGPU up just to print a name.
static std::optional<QString> nvidiaProcModel() {
    const QDir d(QStringLiteral("/proc/driver/nvidia/gpus"));
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot, QDir::Name)) {
        QFile f(d.filePath(e) + QStringLiteral("/information"));
        if (!f.open(QIODevice::ReadOnly)) continue;
        for (const QByteArray &line : f.read(16 * 1024).split('\n'))
            if (line.startsWith("Model:"))
                if (const QString m = QString::fromUtf8(line.mid(6)).trimmed(); !m.isEmpty()) return m;
    }
    return std::nullopt;
}

/// nvidia-smi path, resolved once ($PATH does not change under us).
static const QString &nvidiaSmiExe() {
    static const QString exe = QStandardPaths::findExecutable(QStringLiteral("nvidia-smi"));
    return exe;
}

static const QHash<QString, QString> LABELS{
    {"low-power", "Power Saver"}, {"quiet", "Quiet"}, {"cool", "Cool"}, {"balanced", "Balanced"},
    {"balanced-performance", "Balanced Performance"}, {"performance", "Performance"},
    {"max-power", "Extreme"}, {"custom", "Custom"}};

static const QHash<QString, QString> DESCRIPTIONS{
    {"low-power", "Lowest power draw, longest battery life."},
    {"quiet", "Keeps fan noise as low as possible."},
    {"cool", "Prioritises low surface and internal temperatures."},
    {"balanced", "Balanced mix of power, noise and performance."},
    {"balanced-performance", "Leans towards performance."},
    {"performance", "Maximum performance; fans run louder."},
    {"max-power", "Unrestricted BIOS limits, mains power only."},
    {"custom", "Hands the PPT and fan limits to you. Required by the Firmware Attributes tab."}};

QString HomeTab::profileLabel(const QString &p) {
    if (auto it = LABELS.find(p); it != LABELS.end()) return *it;
    QString t = p;
    t.replace('-', ' ');
    if (!t.isEmpty()) t[0] = t[0].toUpper();
    return t;
}

QString HomeTab::helperPath() { return privileged::helperPath(QStringLiteral("legion-profile-helper")); }

static QLabel *muted(const QString &text, QWidget *parent = nullptr) {
    auto *l = new QLabel(text, parent);
    l->setProperty("role", "muted");
    l->setWordWrap(true);
    return l;
}

static QPushButton *profileButton(const QString &label, const QString &accent) {
    auto *b = new QPushButton(label);
    b->setCheckable(true);
    b->setCursor(Qt::PointingHandCursor);
    b->setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Fixed);
    b->setFixedHeight(26);
    b->setStyleSheet(QStringLiteral(
        "QPushButton { background: %1; color: %2; border: 1px solid %3; border-left: 3px solid %4;"
        " border-radius: 5px; padding: 3px 10px; font-weight: 600; text-align: left; }"
        "QPushButton:hover { background: %5; color: %6; }"
        "QPushButton:checked { background: %4; color: %7; border-color: %4; }")
        .arg(theme::BG2, theme::FG_DIM, theme::BORDER_SOFT, accent, theme::BG3, theme::FG, theme::BG0));
    return b;
}

HomeTab::HomeTab(QWidget *parent) : QWidget(parent), handler_(pp::primaryHandler()) {
    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(12, 10, 12, 10);
    root->setSpacing(8);

    // Header
    auto *title = new QLabel(sysinfo::dmi("product_version").value_or(sysinfo::dmi("product_name").value_or("Lenovo Legion")));
    title->setProperty("role", "title");
    QStringList sub{sysinfo::cpuModel().value_or("unknown CPU")};
    if (handler_) sub << "driver: " + handler_->name;
    root->addWidget(title);
    root->addWidget(muted(sub.join("   ·   ")));

    guardBanner_ = new QFrame;
    guardBanner_->setStyleSheet(QStringLiteral("QFrame { background: rgba(219,123,110,0.12); border: 1px solid %1; border-radius: 8px; }"
                                               "QLabel { background: transparent; border: none; }").arg(theme::DANGER_SOFT));
    auto *gb = new QHBoxLayout(guardBanner_);
    gb->setContentsMargins(10, 6, 8, 6);
    guardText_ = new QLabel;
    guardText_->setWordWrap(true);
    guardText_->setTextFormat(Qt::RichText);
    gb->addWidget(guardText_, 1);
    resumeBoot_ = new QPushButton("Resume boot presets");
    resumeBoot_->setToolTip("Apply the boot presets again from the next boot on. Fix or lower the offending\n"
                            "undervolt / curve first, or the machine may crash again.");
    resumeLogin_ = new QPushButton("Resume login scene");
    resumeLogin_->setToolTip("Apply the automatic scene at login again. Fix the scene's curves first.");
    gb->addWidget(resumeBoot_);
    gb->addWidget(resumeLogin_);
    root->addWidget(guardBanner_);
    guardBanner_->hide();
    connect(resumeLogin_, &QPushButton::clicked, this, [this] { scenes::resumeLoginGuard(); refreshGuard(); });
    connect(resumeBoot_, &QPushButton::clicked, this, [this] {
        resumeBoot_->setEnabled(false);
        privileged::run(privileged::helperPath("tune-helper"), QJsonObject{{"op", "guard_reset"}}, this,
                        [this](const privileged::Result &r) {
            resumeBoot_->setEnabled(true);
            if (!r.ok()) { showStatus("Could not resume: " + r.message(), 8000); return; }
            showStatus("Boot presets resume from the next boot.", 6000);
            refreshGuard();
        });
    });

    auto *profileBox = new QGroupBox("Power Profile");
    profileBox->setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Expanding);
    grid_ = new QGridLayout(profileBox);
    grid_->setContentsMargins(8, 4, 8, 6);
    grid_->setHorizontalSpacing(8);
    grid_->setVerticalSpacing(4);

    // One line, fixed height, elided: messages never push the boxes around.
    status_ = muted(QString());
    status_->setFixedHeight(status_->fontMetrics().height() + 2);
    status_->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Fixed);

    // 2×2 grid, every cell the same size.
    auto *cells = new QGridLayout;
    cells->setSpacing(10);
    QGroupBox *dev = buildDeviceBox();  // hosts status_ when present
    const QList<QWidget *> boxes{buildHardwareBox(), buildLiveBox(), profileBox, dev};
    for (int i = 0; i < boxes.size(); ++i) {
        if (!boxes[i]) continue;
        boxes[i]->setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Expanding);
        cells->addWidget(boxes[i], i / 2, i % 2);
    }
    cells->setRowStretch(0, 1);
    cells->setRowStretch(1, 1);
    cells->setColumnStretch(0, 1);
    cells->setColumnStretch(1, 1);
    root->addLayout(cells, 1);
    if (!dev) root->addWidget(status_);
    statusTimer_ = new QTimer(this);
    statusTimer_->setSingleShot(true);
    connect(statusTimer_, &QTimer::timeout, status_, [this] { status_->clear(); });

    root->addWidget(muted("Per-component limits and undervolt curves live in the other tabs. "
                          "Firmware Attributes only accepts writes while the profile is Custom."));

    group_ = new QButtonGroup(this);
    group_->setExclusive(true);
    connect(group_, &QButtonGroup::buttonClicked, this, [this](QAbstractButton *b) {
        applyProfile(b->property("profile").toString());
    });

    rebuild();

    auto *poll = new QTimer(this);
    connect(poll, &QTimer::timeout, this, &HomeTab::refreshSelection);
    poll->start(POLL_MS);
    auto *live = new QTimer(this);
    connect(live, &QTimer::timeout, this, &HomeTab::refreshLive);
    live->start(LIVE_POLL_MS);
    QTimer::singleShot(0, this, &HomeTab::refreshLive);
}

// ── nvidia-smi (async, bounded) ─────────────────────────────────────────────

void HomeTab::runNvidiaSmi(const QStringList &args, std::function<void(const QByteArray &)> onOk) {
    const QString &exe = nvidiaSmiExe();
    if (exe.isEmpty()) { onOk({}); return; }
    auto *p = new QProcess(this);
    auto *t = new QTimer(p);
    t->setSingleShot(true);
    connect(t, &QTimer::timeout, p, [p] { p->kill(); });
    connect(p, &QProcess::finished, this, [p, onOk](int code, QProcess::ExitStatus st) {
        onOk(code == 0 && st == QProcess::NormalExit ? p->readAllStandardOutput() : QByteArray());
        p->deleteLater();
    });
    connect(p, &QProcess::errorOccurred, this, [p, onOk](QProcess::ProcessError e) {
        if (e == QProcess::FailedToStart) { onOk({}); p->deleteLater(); }
    });
    // Mark before start(): a synchronous FailedToStart runs onOk (which
    // clears smiLive_) inside start(), and must not be overwritten after.
    if (args.contains("--query-gpu=temperature.gpu,power.draw,clocks.current.graphics,utilization.gpu"))
        smiLive_ = p;
    p->start(exe, args);
    t->start(SMI_TIMEOUT_MS);
}

// ── Hardware box ────────────────────────────────────────────────────────────

QGroupBox *HomeTab::buildHardwareBox() {
    auto *box = new QGroupBox("Hardware");
    box->setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Expanding);
    auto *g = new QGridLayout(box);
    g->setContentsMargins(8, 4, 8, 6);
    g->setHorizontalSpacing(10);
    g->setVerticalSpacing(6);

    int row = 0;
    auto add = [&](const QString &k, const std::optional<QString> &v, QLabel **keyOut = nullptr, QLabel **valOut = nullptr) {
        if (!v) return;
        auto *key = muted(k);
        auto *val = new QLabel(*v);
        val->setWordWrap(true);
        g->addWidget(key, row, 0, Qt::AlignTop);
        g->addWidget(val, row, 1);
        if (keyOut) *keyOut = key;
        if (valOut) *valOut = val;
        ++row;
    };
    add("System", sysinfo::systemInfo());
    add("Kernel", sysinfo::kernel());
    add("CPU", sysinfo::cpuModel());
    add("GPU", QStringLiteral("…"), &gpuHwKey_, &gpuHwValue_);
    {
        QLabel *memVal = nullptr;
        add("Memory", sysinfo::ramTotal(), nullptr, &memVal);
        if (memVal) {
            // Replace the plain value with value + a "Timings…" link to the SPD view.
            auto *w = new QWidget;
            auto *h = new QHBoxLayout(w);
            h->setContentsMargins(0, 0, 0, 0);
            g->removeWidget(memVal);
            h->addWidget(memVal, 1);
            auto *btn = new QPushButton("Timings…");
            btn->setToolTip("Show each module's JEDEC timings from its SPD chip (read-only).");
            connect(btn, &QPushButton::clicked, this, [this] { (new MemoryDialog(helperPath(), this))->show(); });
            h->addWidget(btn);
            g->addWidget(w, row - 1, 1);
        }
    }
    add("GPU mode", sysinfo::gpuMode());
    add("BIOS", sysinfo::biosInfo());
    add("EC Firmware", sysinfo::dmiClean("ec_firmware_release"));
    g->setColumnStretch(1, 1);
    g->setRowStretch(row, 1);

    if (auto m = nvidiaProcModel()) {
        gpuHwValue_->setText(*m);
    } else {
        runNvidiaSmi({"--query-gpu=name", "--format=csv,noheader"}, [this](const QByteArray &out) {
            if (auto n = sysinfo::parseGpuName(out)) { gpuHwValue_->setText(*n); }
            else { gpuHwKey_->hide(); gpuHwValue_->hide(); }
        });
    }
    return box;
}

// ── Live box ────────────────────────────────────────────────────────────────

QGroupBox *HomeTab::buildLiveBox() {
    auto *box = new QGroupBox("Live");
    box->setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Expanding);
    auto *g = new QGridLayout(box);
    g->setContentsMargins(8, 4, 8, 6);
    g->setHorizontalSpacing(10);
    g->setVerticalSpacing(6);

    const QList<QPair<QString, std::function<std::optional<QString>()>>> spec{
        {"CPU Temp", sysinfo::cpuTemp}, {"GPU", nullptr}, {"iGPU", sysinfo::igpu},
        {"Fans", sysinfo::fans}, {"Storage", sysinfo::storage}, {"Power", sysinfo::power},
        {"CPU Power", sysinfo::cpuPackagePower}, {"USB-C in", sysinfo::usbcInputs},
        {"Battery", sysinfo::battery}};
    int row = 0;
    for (const auto &[name, getter] : spec) {
        if (!getter) {  // GPU: filled asynchronously; hidden until nvidia-smi answers
            gpuLiveKey_ = muted(name);
            gpuLiveValue_ = new QLabel;
            gpuLiveValue_->setWordWrap(true);
            gpuLiveKey_->hide();
            gpuLiveValue_->hide();
            g->addWidget(gpuLiveKey_, row, 0, Qt::AlignTop);
            g->addWidget(gpuLiveValue_, row++, 1);
            continue;
        }
        auto v = getter();
        if (!v) continue;
        auto *key = muted(name);
        auto *val = new QLabel(*v);
        val->setWordWrap(true);
        g->addWidget(key, row, 0, Qt::AlignTop);
        g->addWidget(val, row++, 1);
        liveRows_.append({key, val, getter});
    }
    if (liveRows_.isEmpty()) g->addWidget(muted("No live sensors found (hwmon, nvidia-smi, battery)."), row++, 0, 1, 2);
    g->setColumnStretch(1, 1);
    g->setRowStretch(row, 1);
    return box;
}

void HomeTab::refreshLive() {
    // Not visible (hidden to tray / other tab) → no reads. nvidia-smi in
    // particular wakes the dGPU out of D3cold.
    if (!isVisible()) return;
    refreshDevice();
    refreshGpuLive();
    if (liveBusy_ || liveRows_.isEmpty()) return;  // never stack sweeps

    // The sysfs sweep runs off the GUI thread: battery/charger attributes are
    // ACPI method calls (_BST/_PSR) answered by the EC, and an EC that is busy
    // (fan or profile change in flight) can hold a read for tens to hundreds
    // of ms — which used to stall repaint and input every 2 s.
    liveBusy_ = true;
    QList<std::function<std::optional<QString>()>> getters;
    getters.reserve(liveRows_.size());
    for (const LiveRow &r : std::as_const(liveRows_)) getters.append(r.getter);
    QPointer<HomeTab> self(this);
    QThreadPool::globalInstance()->start([self, getters] {
        QList<std::optional<QString>> vals;
        vals.reserve(getters.size());
        for (const auto &g : getters) vals.append(g());
        QMetaObject::invokeMethod(QCoreApplication::instance(), [self, vals] {
            if (!self) return;
            self->liveBusy_ = false;
            for (int i = 0; i < vals.size() && i < self->liveRows_.size(); ++i) {
                const QString t = vals[i].value_or(QStringLiteral("—"));
                if (self->liveRows_[i].value->text() != t) self->liveRows_[i].value->setText(t);
            }
        }, Qt::QueuedConnection);
    });
}

void HomeTab::refreshGpuLive() {
    if (smiLive_) return;  // previous query still running — never stack them
    if (!dgpuProbed_) {
        dgpuProbed_ = true;
        if (const QString pci = nvidiaPciDir(); !pci.isEmpty()) dgpuRuntimeStatus_ = pci + QStringLiteral("/power/runtime_status");
    }
    // Asleep (runtime PM "suspended" = D3hot/D3cold): report it, don't wake it.
    if (!dgpuRuntimeStatus_.isEmpty()
        && pp::readText(dgpuRuntimeStatus_).value_or(QString()) == QLatin1String("suspended")) {
        gpuLiveValue_->setText(QStringLiteral("asleep (runtime suspended, ~0 W)"));
        gpuLiveKey_->show();
        gpuLiveValue_->show();
        nextSmiAt_ = 0;  // query immediately once something else wakes it
        return;
    }
    if (monoMs() < nextSmiAt_) return;
    runNvidiaSmi({"--query-gpu=temperature.gpu,power.draw,clocks.current.graphics,utilization.gpu",
                  "--format=csv,noheader,nounits"},
                 [this](const QByteArray &out) {
                     smiLive_ = nullptr;
                     // Busy → normal cadence; idle (0 % util) → back off so the
                     // driver's idle timer can expire and the GPU can suspend.
                     const QStringList parts = QString::fromUtf8(out).trimmed().section('\n', 0, 0).split(',');
                     bool ok = false;
                     const int util = parts.size() >= 4 ? parts[3].trimmed().toInt(&ok) : 0;
                     nextSmiAt_ = monoMs() + (ok && util > 0 ? 0 : SMI_IDLE_MS);
                     if (auto v = sysinfo::parseGpuLive(out)) {
                         gpuLiveValue_->setText(*v);
                         gpuLiveKey_->show();
                         gpuLiveValue_->show();
                     } else if (gpuLiveValue_->isVisible()) {
                         gpuLiveValue_->setText(QStringLiteral("—"));
                     }
                 });
}

// ── Device box ──────────────────────────────────────────────────────────────

static QString rdText(const QString &p) { return pp::readText(p).value_or(QString()); }

static QString findDir(const QString &base, const std::function<bool(const QString &)> &pred) {
    const QDir d(base);
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name))
        if (pred(d.filePath(e))) return d.filePath(e);
    return {};
}

QGroupBox *HomeTab::buildDeviceBox() {
    const QString bat = findDir("/sys/class/power_supply", [](const QString &p) {
        return rdText(p + "/type") == "Battery" && QFileInfo(p + "/charge_types").isFile(); });
    if (!bat.isEmpty()) chargeFile_ = bat + "/charge_types";
    ideapadDir_ = findDir("/sys/bus/platform/drivers/ideapad_acpi", [](const QString &p) {
        return QFileInfo(p).fileName().startsWith("VPC"); });
    fanHwmon_ = findDir("/sys/class/hwmon", [](const QString &p) { return rdText(p + "/name") == "lenovo_wmi_other"; });

    auto *box = new QGroupBox("Device");
    auto *g = new QGridLayout(box);
    g->setContentsMargins(8, 4, 8, 6);
    g->setHorizontalSpacing(8);
    g->setVerticalSpacing(4);
    int row = 0;
    // Row labels never wrap: a wrapped "Battery charge" made column 0 jump in width.
    auto label = [](const QString &t) { QLabel *l = muted(t); l->setWordWrap(false); return l; };

    if (!chargeFile_.isEmpty()) {
        charge_ = new QComboBox;
        charge_->setToolTip("Battery charge mode.\nLong_Life = stop around 80% (conservation), best while mostly plugged in.\n"
                            "Standard = full charge.  Fast = rapid charge, more heat and wear.");
        for (const QString &w : rdText(chargeFile_).split(' ', Qt::SkipEmptyParts)) {
            const QString o = QString(w).remove('[').remove(']');
            charge_->addItem(QString(o).replace('_', ' '), o);
        }
        connect(charge_, &QComboBox::activated, this, [this](int i) { setDevice("charge_type", charge_->itemData(i).toString()); });
        g->addWidget(label("Battery charge"), row, 0);
        g->addWidget(charge_, row++, 1, 1, 4);
    }

    // GPU mode (MUX) — only where the Legion GameZone WMI interface exists.
    if (!QDir(QStringLiteral("/sys/bus/wmi/devices")).entryList({"887B54E3-DDDC-4B2C-8B88-68A26A8835D0*"},
                                                               QDir::Dirs | QDir::System | QDir::NoDotAndDotDot).isEmpty()) {
        gpuMode_ = new QComboBox;
        gpuMode_->addItem("Hybrid (iGPU + NVIDIA)", "hybrid");
        gpuMode_->addItem("dGPU only (MUX → NVIDIA)", "dgpu");
        gpuMode_->setEnabled(false);
        gpuMode_->setToolTip("Which GPU drives the internal display — switched by the firmware at the next boot.\n"
                             "Hybrid: the AMD iGPU drives the panel and the NVIDIA GPU can power off (much longer\n"
                             "battery life); games render on NVIDIA through PRIME offload. Needs the amdgpu driver.\n"
                             "dGPU only: the panel is wired straight to NVIDIA (lowest latency, G-SYNC on the\n"
                             "internal panel), the iGPU is hidden and the NVIDIA GPU never sleeps.");
        const bool amdNow = [] {
            const QDir d(QStringLiteral("/sys/bus/pci/devices"));
            for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System))
                if (rdText(d.filePath(e) + "/vendor") == "0x1002" && rdText(d.filePath(e) + "/class").startsWith("0x03")) return true;
            return false;
        }();
        gpuMode_->setCurrentIndex(amdNow ? 0 : 1);
        connect(gpuMode_, &QComboBox::activated, this, [this](int i) { setGpuMode(gpuMode_->itemData(i).toString(), false); });
        g->addWidget(label("GPU mode"), row, 0);
        g->addWidget(gpuMode_, row++, 1, 1, 4);
    }

    if (!ideapadDir_.isEmpty()) {
        const QList<QPair<QString, QString>> spec{
            {"fn_lock", "Fn lock"}, {"camera_power", "Camera"}, {"usb_charging", "USB charging when off"}};
        auto *h = new QHBoxLayout;
        h->setSpacing(12);
        for (const auto &[key, label] : spec) {
            if (!QFileInfo(ideapadDir_ + '/' + key).isFile()) continue;
            auto *cb = new QCheckBox(label);
            connect(cb, &QCheckBox::clicked, this, [this, key](bool on) { setDevice(key, on ? "1" : "0"); });
            toggles_.insert(key, cb);
            h->addWidget(cb);
        }
        h->addStretch(1);
        if (!toggles_.isEmpty()) {
            g->addWidget(label("Switches"), row, 0);
            g->addLayout(h, row++, 1, 1, 4);
        }
    }


    int bannerRow = -1;
    if (!fanHwmon_.isEmpty()) bannerRow = row++;  // banner sits above the fan rows
    if (!fanHwmon_.isEmpty()) {
        const QDir d(fanHwmon_);
        for (const QString &f : d.entryList({"fan*_target"}, QDir::Files | QDir::System, QDir::Name)) {
            const QString n = f.mid(3, f.indexOf('_') - 3);
            const int lo = rdText(d.filePath("fan" + n + "_min")).toInt(), hi = rdText(d.filePath("fan" + n + "_max")).toInt();
            const int top = hi > 0 ? hi : 9999;
            auto *edit = new QLineEdit;
            edit->setValidator(new QIntValidator(1, top, edit));
            edit->setPlaceholderText(QStringLiteral("RPM"));
            edit->setMaximumWidth(80);
            edit->setToolTip(QStringLiteral("Type a target RPM (1–%1) and press Enter or Set.\n"
                                            "Firmware-reported range is %2–%1; below %2 the EC may clamp it —\n"
                                            "the RPM column shows what it really runs at.\n"
                                            "Fan targets are usually honoured only in the Custom profile.").arg(top).arg(lo));
            auto *autoBox = new QCheckBox("Auto");
            autoBox->setToolTip("Hand the fan back to the EC (writes 0). The EC resumes its own curve only when\n"
                                "every fan is on Auto; while another fan is manual this one keeps its last speed.");
            auto *set = new QPushButton("Set");
            set->setFixedWidth(46);
            auto *rpm = muted(QString());
            const QString key = f;
            auto apply = [this, key, edit] {
                if (!edit->hasAcceptableInput()) { showStatus("Enter an RPM value first", 4000); return; }
                setDevice(key, QString::number(edit->text().toInt()));
                edit->clearFocus();
            };
            connect(set, &QPushButton::clicked, this, apply);
            connect(edit, &QLineEdit::returnPressed, this, apply);
            auto *maxBox = new QCheckBox("Max");
            maxBox->setToolTip(QStringLiteral("Run this fan at its maximum, %1 RPM.").arg(top));
            connect(maxBox, &QCheckBox::clicked, this, [this, key, edit, set, autoBox, top](bool on) {
                if (on) {
                    autoBox->setChecked(false);
                    edit->setEnabled(false);
                    set->setEnabled(false);
                    setDevice(key, QString::number(top));
                    return;
                }
                // Leaving Max: back to Auto (the safe state), user can untick Auto to type.
                // If the EC's own Full Speed flag is what keeps it at max, clear that too.
                autoBox->setChecked(true);
                setFanAuto(key);
            });
            connect(autoBox, &QCheckBox::clicked, this, [this, key, edit, set, maxBox, lo](bool on) {
                maxBox->setChecked(false);
                edit->setEnabled(!on);
                set->setEnabled(!on);
                if (on) { setFanAuto(key); return; }
                // Leaving Auto: start from something sane and let the user adjust.
                if (edit->text().isEmpty()) edit->setText(QString::number(lo > 0 ? lo : 2000));
                edit->setFocus();
                edit->selectAll();
            });
            g->addWidget(label("Fan " + n), row, 0);
            g->addWidget(rpm, row, 1);
            auto *modes = new QHBoxLayout;
            modes->setSpacing(8);
            modes->addWidget(autoBox);
            modes->addWidget(maxBox);
            g->addLayout(modes, row, 2);
            g->addWidget(edit, row, 3);
            g->addWidget(set, row++, 4);
            fans_.append({key, rpm, edit, autoBox, maxBox, set, top});
        }
    }

    // EC Full Speed flag. Upstream lenovo_wmi_other exposes it as pwm1_enable only
    // where the kernel knows the feature (0 = full speed, 2 = auto); LenovoLegionLinux's
    // legion_laptop module as fan_fullspeed (1/0). Without either, Linux cannot
    // read or clear it, and fanN_target keeps reading 0 while the fans run flat out.
    if (!fanHwmon_.isEmpty() && QFileInfo(fanHwmon_ + "/pwm1_enable").isFile()) {
        fullSpeedFile_ = fanHwmon_ + "/pwm1_enable";
        fullSpeedPwm_ = true;
    } else {
        const QString d = findDir("/sys/bus/platform/drivers/legion", [](const QString &p) {
            return QFileInfo(p + "/fan_fullspeed").isFile(); });
        if (!d.isEmpty()) fullSpeedFile_ = d + "/fan_fullspeed";
    }
    if (!fullSpeedFile_.isEmpty()) {
        fullSpeed_ = new QCheckBox("Full speed (EC)");
        fullSpeed_->setToolTip("The embedded controller's own Full Speed mode (the switch in Lenovo Vantage /\n"
                               "Legion Space). It overrides every fan target and stays on across reboots and OS\n"
                               "changes until it is turned off. Source: " + fullSpeedFile_);
        connect(fullSpeed_, &QCheckBox::clicked, this, [this](bool on) { setDevice("fan_fullspeed", on ? "1" : "0"); });
        g->addWidget(label("Fans"), row, 0);
        g->addWidget(fullSpeed_, row++, 1, 1, 4);
    }
    if (bannerRow >= 0 && !fans_.isEmpty()) {
        maxBanner_ = new QFrame;
        maxBanner_->setObjectName("maxBanner");
        maxBanner_->setStyleSheet(QStringLiteral("#maxBanner { border: 1px solid %1; border-radius: 6px; background: rgba(230,160,60,0.10); }")
                                      .arg(theme::WARN));
        auto *bl = new QHBoxLayout(maxBanner_);
        bl->setContentsMargins(10, 6, 8, 6);
        maxBannerText_ = new QLabel;
        maxBannerText_->setTextFormat(Qt::RichText);
        maxBannerText_->setWordWrap(true);
        maxBannerBtn_ = new QPushButton("Disable max fans");
        maxBannerBtn_->setObjectName("btnAccent");
        maxBannerBtn_->setToolTip("Return every fan to Auto (the EC only resumes its curve when all targets are 0).");
        connect(maxBannerBtn_, &QPushButton::clicked, this, &HomeTab::exitMaxMode);
        bl->addWidget(maxBannerText_, 1);
        bl->addWidget(maxBannerBtn_);
        maxBanner_->hide();
        g->addWidget(maxBanner_, bannerRow, 0, 1, 5);

        // Entry point: one click puts every fan at max (and so into the mode above).
        maxAllBtn_ = new QPushButton("Max all fans");
        maxAllBtn_->setToolTip("Set every fan to its maximum RPM. Controls lock until you press Disable max fans.");
        connect(maxAllBtn_, &QPushButton::clicked, this, &HomeTab::setAllFansMax);
        QPushButton *curveBtn = nullptr;
        if (fanCurveSupported()) {
        curveBtn = new QPushButton("Fan curve…");
        curveBtn->setToolTip("Edit the Custom-mode fan curve the EC follows (all fans, 10 temperature steps).");
        connect(curveBtn, &QPushButton::clicked, this, [this] {
            const auto prof = currentProfile();
            auto *dlg = new FanCurveDialog(helperPath(), prof.value_or(QStringLiteral("unknown")), this);
            dlg->show();
        });
        }
        auto *mh = new QHBoxLayout;
        mh->addStretch(1);
        if (curveBtn) mh->addWidget(curveBtn);
        mh->addWidget(maxAllBtn_);
        g->addLayout(mh, row++, 0, 1, 5);
    }
    if (!fans_.isEmpty() || fullSpeed_) {
        fanWarn_ = new QLabel;
        fanWarn_->setWordWrap(true);
        fanWarn_->setTextFormat(Qt::RichText);
        fanWarn_->hide();
        g->addWidget(fanWarn_, row++, 0, 1, 5);
    }

    if (row == 0) { delete box; return nullptr; }
    g->setRowStretch(row++, 1);
    g->addWidget(status_, row++, 0, 1, 5);
    g->setColumnStretch(1, 1);
    refreshDevice();
    return box;
}

void HomeTab::refreshDevice() {
    if (devicePending_ > 0) return;  // a write is in flight; its callback refreshes
    if (charge_ && !charge_->view()->isVisible()) {
        const QString raw = rdText(chargeFile_);
        const int a = raw.indexOf('['), b = raw.indexOf(']');
        if (a >= 0 && b > a) {
            QSignalBlocker blk(charge_);
            charge_->setCurrentIndex(charge_->findData(raw.mid(a + 1, b - a - 1)));
        }
    }
    for (auto it = toggles_.cbegin(); it != toggles_.cend(); ++it) {
        QSignalBlocker blk(it.value());
        it.value()->setChecked(rdText(ideapadDir_ + '/' + it.key()) == "1");
    }
    // Full Speed: read it where the kernel exposes it; otherwise infer it —
    // every target 0 (= "auto") yet every fan at ≥ 92 % of its max for two polls in a row.
    const std::optional<bool> fs = readFullSpeed();
    bool suspect = false;
    if (fs) {
        fullSpeedOn_ = *fs;
        fullSpeedGuess_ = 0;
    } else if (!fans_.isEmpty()) {
        bool looks = true;
        for (const FanRow &f : fans_) {
            const QString n = f.key.mid(3, f.key.indexOf('_') - 3);
            const int in = rdText(fanHwmon_ + "/fan" + n + "_input").toInt(), tgt = rdText(fanHwmon_ + '/' + f.key).toInt();
            looks &= tgt == 0 && f.max > 0 && f.max < 9999 && in >= f.max * 92 / 100;
        }
        fullSpeedGuess_ = looks ? fullSpeedGuess_ + 1 : 0;
        if (!fanTouched_ && fullSpeedGuess_ >= 2) fullSpeedAtStart_ = true;
        if (!looks) fullSpeedAtStart_ = false;  // fans slowed down: whatever held them is gone
        suspect = fullSpeedAtStart_;
        fullSpeedOn_ = suspect;
    }
    if (fullSpeed_) { QSignalBlocker b(fullSpeed_); fullSpeed_->setChecked(fs.value_or(false)); }
    if (fanWarn_) {
        if (fs && *fs) {
            fanWarn_->setText(QStringLiteral("<span style='color:%1'>EC Full Speed is on — fan targets are ignored until it is "
                                             "switched off (untick it, or pick Auto).</span>").arg(theme::WARN));
        } else if (suspect) {
            fanWarn_->setText(QStringLiteral("<span style='color:%1'>The fans run at maximum with no target set: the EC's Full Speed "
                "mode is on (it survives reboots, e.g. switched on in Windows). This kernel has no interface to turn it off — "
                "lenovo_wmi_other lacks pwm1_enable and legion_laptop is not loaded. Turn it off in Lenovo Vantage / Legion "
                "Space, or load LenovoLegionLinux's legion_laptop module; an EC reset (power off, hold the power button "
                "~30 s) also clears it.</span>").arg(theme::WARN));
        }
        fanWarn_->setVisible((fs && *fs) || suspect);
    }

    bool allMax = !fans_.isEmpty();
    for (const FanRow &f : std::as_const(fans_)) {
        const int t = rdText(fanHwmon_ + '/' + f.key).toInt();
        allMax &= f.max > 0 && t >= f.max;
    }
    const bool ecFs = fs && *fs;
    setMaxMode(allMax || ecFs, ecFs && !allMax);

    for (const FanRow &f : fans_) {
        const QString n = f.key.mid(3, f.key.indexOf('_') - 3);
        f.rpm->setText(rdText(fanHwmon_ + "/fan" + n + "_input") + " RPM");
        const int target = rdText(fanHwmon_ + '/' + f.key).toInt();
        if (maxMode_) { f.maxBox->setChecked(true); f.autoBox->setChecked(false); continue; }
        // Force the Max display only for a real (read) Full Speed, or an inferred one
        // the user has not overridden yet; after a click the user's choice is shown.
        if (!f.target->hasFocus() && ((fs && *fs) || (suspect && !fanTouched_))) {
            // target 0 would otherwise be shown as "Auto" while the EC holds the fans at max.
            f.maxBox->setChecked(true);
            f.autoBox->setChecked(false);
            f.target->setEnabled(false);
            f.set->setEnabled(false);
            f.maxBox->setToolTip(QStringLiteral("Held at maximum by the EC's Full Speed mode%1.")
                .arg(fullSpeed_ ? QString() : QStringLiteral(" (detected from RPM; cannot be cleared from this kernel)")));
            continue;
        }
        f.maxBox->setToolTip(QStringLiteral("Run this fan at its maximum, %1 RPM.").arg(f.max));
        if (!f.target->hasFocus()) {
            // Auto only reflects the hardware while the user is not mid-edit
            // (unticked Auto + empty box = about to type a value).
            f.maxBox->setChecked(target > 0 && target >= f.max);
            if (target > 0) {
                f.autoBox->setChecked(false);
                f.target->setText(QString::number(target));
            } else if (f.autoBox->isChecked() || f.target->text().isEmpty() || f.target->isEnabled() == false) {
                f.autoBox->setChecked(true);
            }
            const bool locked = f.autoBox->isChecked() || f.maxBox->isChecked();
            f.target->setEnabled(!locked);
            f.set->setEnabled(!locked);
        }
    }
}

void HomeTab::setMaxMode(bool on, bool ecFullSpeed) {
    maxMode_ = on;
    if (!maxBanner_) return;
    maxBanner_->setVisible(on);
    if (maxAllBtn_) maxAllBtn_->setVisible(!on);
    if (on) {
        maxBannerText_->setText(ecFullSpeed
            ? QStringLiteral("<b>EC Full Speed is on</b> — all fans run at maximum. Fan controls are locked.")
            : QStringLiteral("<b>Max fans</b> — every fan runs at its maximum. Fan controls are locked."));
        maxBannerBtn_->setText(ecFullSpeed ? "Disable full speed" : "Disable max fans");
    }
    // Grey out every per-fan control (the RPM readout stays live).
    for (const FanRow &f : std::as_const(fans_)) {
        f.autoBox->setEnabled(!on);
        f.maxBox->setEnabled(!on);
        if (on) { f.target->setEnabled(false); f.set->setEnabled(false); }
    }
    if (fullSpeed_) fullSpeed_->setEnabled(!on || ecFullSpeed);
}

void HomeTab::exitMaxMode() {
    clearFullSpeed();
    // All targets to 0 together: the only state in which the EC takes the fans back.
    for (const FanRow &f : std::as_const(fans_)) {
        f.maxBox->setChecked(false);
        f.autoBox->setChecked(true);
        setDevice(f.key, "0");
    }
    setMaxMode(false, false);
    showStatus("All fans back to Auto — the EC curve takes over as they spin down.", 6000);
}

QList<HomeTab::FanInfo> HomeTab::fanInfo() const {
    QList<FanInfo> out;
    for (const FanRow &f : fans_) {
        const QString n = f.key.mid(3, f.key.indexOf('_') - 3);
        out.append({f.key, "Fan " + n, rdText(fanHwmon_ + "/fan" + n + "_min").toInt(), f.max,
                    rdText(fanHwmon_ + '/' + f.key).toInt()});
    }
    return out;
}

void HomeTab::setAllFansMax() {
    for (const FanRow &f : std::as_const(fans_)) setDevice(f.key, QString::number(f.max));
    setMaxMode(true, false);
}

void HomeTab::setFanTarget(const QString &key, int rpm) {
    if (rpm <= 0) { setFanAuto(key); return; }
    clearFullSpeed();  // EC Full Speed would ignore the target
    for (const FanRow &f : std::as_const(fans_))
        if (f.key == key) { setDevice(key, QString::number(std::min(rpm, f.max))); return; }
}

void HomeTab::setFanAuto(const QString &key) {
    QStringList manual;
    for (const FanRow &f : std::as_const(fans_))
        if (f.key != key && rdText(fanHwmon_ + '/' + f.key).toInt() > 0) manual << f.key;
    int choice = manual.isEmpty() ? 2 : autoAllChoice_;
    if (choice == 0) {
        QMessageBox mb(QMessageBox::Question, "Fan to Auto",
            "The EC hands the fans back to its own curve only when ALL fan targets are Auto.\n\n"
            "While another fan stays manual, this fan keeps running at its current speed (e.g. max) "
            "instead of slowing down. To slow only this fan, set a fixed RPM instead.",
            QMessageBox::NoButton, this);
        auto *all = mb.addButton("All fans to Auto", QMessageBox::AcceptRole);
        auto *one = mb.addButton("Only this fan", QMessageBox::ActionRole);
        mb.addButton(QMessageBox::Cancel);
        auto *remember = new QCheckBox("Remember for this session");
        mb.setCheckBox(remember);
        mb.setDefaultButton(all);
        mb.exec();
        choice = mb.clickedButton() == all ? 1 : mb.clickedButton() == one ? 2 : 0;
        if (choice && remember->isChecked()) autoAllChoice_ = choice;
        if (!choice) { refreshDevice(); return; }  // cancelled: show the real state again
    }
    clearFullSpeed();
    if (choice == 1) {
        for (const FanRow &f : std::as_const(fans_)) {
            f.autoBox->setChecked(true);
            f.maxBox->setChecked(false);
            f.target->setEnabled(false);
            f.set->setEnabled(false);
            setDevice(f.key, "0");
        }
        return;
    }
    setDevice(key, "0");
    if (!manual.isEmpty())
        showStatus("Fan set to Auto, but it keeps its last speed until every fan is on Auto (EC behaviour).", 8000);
}

std::optional<bool> HomeTab::readFullSpeed() const {
    if (fullSpeedFile_.isEmpty()) return std::nullopt;
    const QString v = rdText(fullSpeedFile_);
    if (v.isEmpty()) return std::nullopt;  // read error: fall back to the RPM heuristic
    return fullSpeedPwm_ ? v == QLatin1String("0") : v == QLatin1String("1");
}

void HomeTab::clearFullSpeed() {
    if (!fullSpeedOn_) return;
    if (fullSpeed_) { setDevice("fan_fullspeed", "0"); return; }
    showStatus("The EC's Full Speed mode is on and this kernel cannot turn it off — see the note under the fans.", 10000);
}

void HomeTab::setDevice(const QString &key, const QString &value) {
    if (key.startsWith(QLatin1String("fan"))) fanTouched_ = true;  // also for queued writes
    // One pkexec at a time: rapid clicks would otherwise stack polkit dialogs
    // and race on the same sysfs file. Later clicks on a key replace earlier ones.
    if (devicePending_ > 0) {
        for (auto &q : deviceQueue_) if (q.first == key) { q.second = value; return; }
        deviceQueue_.append({key, value});
        return;
    }
    ++devicePending_;
    const QJsonObject req{{"device", key}, {"value", value}};
    privileged::run(helperPath(), req, this, [this, key](const privileged::Result &r) {
        --devicePending_;
        if (r.ok()) showStatus(QStringLiteral("%1 → %2").arg(key, r.json.value("effective").toString()));
        else showStatus(QStringLiteral("%1 failed: %2").arg(key, r.message()), 8000);
        if (!deviceQueue_.isEmpty()) {
            const auto next = deviceQueue_.takeFirst();
            setDevice(next.first, next.second);
            return;
        }
        refreshDevice();  // shows what the hardware actually holds, success or not
    });
}

// ── Profile buttons ─────────────────────────────────────────────────────────

void HomeTab::showStatus(const QString &msg, int timeoutMs) {
    status_->setText(status_->fontMetrics().elidedText(msg, Qt::ElideRight, qMax(50, status_->width())));
    status_->setToolTip(msg);
    if (timeoutMs) statusTimer_->start(timeoutMs);
}

void HomeTab::updateDescription(const std::optional<QString> &p) {
    if (description_) description_->setText(p ? DESCRIPTIONS.value(*p) : QString());
}

void HomeTab::rebuild() {
    for (QPushButton *b : std::as_const(buttons_)) group_->removeButton(b);
    buttons_.clear();
    description_ = nullptr;
    while (QLayoutItem *it = grid_->takeAt(0)) {
        if (QWidget *w = it->widget()) w->deleteLater();
        delete it;
    }
    handler_ = pp::primaryHandler();

    if (!pp::available()) {
        grid_->addWidget(muted(
            "No power-profile interface found. This usually means the lenovo-wmi-gamezone (or legion-laptop) "
            "platform driver is not loaded. CPU and GPU limits can still be set from the Firmware Attributes tab."), 0, 0);
        return;
    }
    const QStringList profiles = pp::offeredProfiles(handler_);
    const auto current = pp::currentProfile(handler_);
    int i = 0;
    for (const QString &name : profiles) {
        auto *b = profileButton(profileLabel(name), theme::profileAccent(name));
        b->setProperty("profile", name);
        b->setToolTip(DESCRIPTIONS.value(name, name));
        b->setChecked(current == name);
        group_->addButton(b);
        buttons_.insert(name, b);
        grid_->addWidget(b, i++, 0);
    }
    grid_->setColumnStretch(0, 1);
    description_ = muted(QString());
    grid_->addWidget(description_, i, 0);
    grid_->setRowStretch(i + 1, 1);
    updateDescription(current);
    if (!current) showStatus("Could not read the current power profile.");
}

void HomeTab::refreshSelection() {
    if (applying_) return;  // don't move the checkmark under a pending polkit dialog
    const auto current = pp::currentProfile(handler_);
    if (!current) return;
    if (QPushButton *b = buttons_.value(*current)) {
        if (!b->isChecked()) { b->setChecked(true); updateDescription(current); }
    } else {
        rebuild();  // a hidden mode became active (hotkey) — give it a button
    }
}

void HomeTab::applyProfile(const QString &profile) {
    if (applying_) return;
    const auto previous = pp::currentProfile(handler_);
    QJsonObject req{{"profile", profile}};
    if (handler_) req.insert("handler", handler_->node);

    applying_ = true;
    for (QPushButton *b : std::as_const(buttons_)) b->setEnabled(false);
    showStatus("Switching to " + profileLabel(profile) + "…", 0);

    privileged::run(helperPath(), req, this, [this, profile, previous](const privileged::Result &r) {
        applying_ = false;
        for (QPushButton *b : std::as_const(buttons_)) b->setEnabled(true);
        if (!r.reached) {
            showStatus(QString());
            QMessageBox::critical(this, "Authorization failed", r.error);
            refreshSelection();
            return;
        }
        if (!r.ok()) {
            showStatus(QString());
            QMessageBox::critical(this, "Could not switch profile", r.message().isEmpty() ? "unknown error" : r.message());
            rebuild();
            return;
        }
        QString effective = r.json.value("effective").toString();
        if (effective.isEmpty()) effective = profile;
        if (effective != profile) {
            showStatus("Requested " + profileLabel(profile) + ", but the firmware settled on " + profileLabel(effective) + ".");
            rebuild();
        } else {
            showStatus("Power profile set to " + profileLabel(effective) + ".");
            if (QPushButton *b = buttons_.value(effective)) b->setChecked(true);
            updateDescription(effective);
        }
        if (previous != effective) Q_EMIT profileChanged(effective);
    }, 60000);
}

// ── first show: GPU mode read-back, guard banner ─────────────────────────────

void HomeTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    refreshGuard();
    if (gpuMode_ && !gpuModeRead_) { gpuModeRead_ = true; readGpuMode(); }
}

void HomeTab::refreshGuard() {
    const QString boot = scenes::bootGuardReason(), login = scenes::loginGuardReason();
    QStringList lines;
    if (!boot.isEmpty()) lines << QStringLiteral("<b>Boot presets paused</b> — %1.").arg(boot.toHtmlEscaped());
    if (!login.isEmpty()) lines << QStringLiteral("<b>Automatic login scene paused</b> — %1.").arg(login.toHtmlEscaped());
    guardText_->setText(lines.join("<br>") + (lines.isEmpty() ? QString()
        : QStringLiteral("<br><span style='color:%1'>Lower the offending undervolt / curve, then resume.</span>").arg(theme::FG_DIM)));
    resumeBoot_->setVisible(!boot.isEmpty());
    resumeLogin_->setVisible(!login.isEmpty());
    guardBanner_->setVisible(!lines.isEmpty());
}

static QString modeLabel(const QString &m) { return m == QLatin1String("dgpu") ? QStringLiteral("dGPU only") : QStringLiteral("hybrid"); }

void HomeTab::readGpuMode() {
    privileged::run(privileged::helperPath("legion-gpu-helper"), QJsonObject{{"op", "gpu_mode"}}, this,
                    [this](const privileged::Result &r) {
        const QJsonObject j = r.json;
        if (!r.ok() || !j.value("supported").toBool()) {
            gpuMode_->setToolTip((r.ok() ? QStringLiteral("Not supported by this firmware.") : r.message()) + "\n\n" + gpuMode_->toolTip());
            return;
        }
        const QString active = j.value("active").toString(), next = j.value("next_boot").toString(active);
        const bool pending = j.value("reboot_pending").toBool();
        {
            const QSignalBlocker b(gpuMode_);
            gpuMode_->setItemText(0, QStringLiteral("Hybrid (iGPU + NVIDIA)"));
            gpuMode_->setItemText(1, QStringLiteral("dGPU only (MUX → NVIDIA)"));
            const int i = std::max(0, gpuMode_->findData(next));
            if (pending) gpuMode_->setItemText(i, gpuMode_->itemText(i) + QStringLiteral("  — after reboot (running %1)").arg(modeLabel(active)));
            gpuMode_->setCurrentIndex(i);
        }
        gpuMode_->setStyleSheet(pending ? QStringLiteral("QComboBox { color: %1; }").arg(theme::WARN) : QString());
        gpuMode_->setEnabled(true);
    }, 60000);
}

void HomeTab::setGpuMode(const QString &mode, bool force) {
    if (!force) {
        const QString msg = mode == QLatin1String("hybrid")
            ? "Switch to Hybrid at the next boot?\n\nThe AMD iGPU will drive the internal display; the NVIDIA GPU can "
              "power off when idle. Games run on NVIDIA via PRIME render offload (prime-run / __NV_PRIME_RENDER_OFFLOAD=1).\n\n"
              "An X11 config that forces the NVIDIA GPU as primary can leave the desktop black in Hybrid — "
              "Wayland sessions are unaffected. If anything goes wrong, the BIOS setup (F2) has the same switch."
            : "Switch to dGPU only at the next boot?\n\nThe internal display is wired straight to the NVIDIA GPU (lowest "
              "latency, G-SYNC on the panel). The iGPU is hidden and the NVIDIA GPU never powers off — shorter battery life.";
        if (QMessageBox::question(this, "GPU mode", msg) != QMessageBox::Yes) { readGpuMode(); return; }
    }
    gpuMode_->setEnabled(false);
    privileged::run(privileged::helperPath("legion-gpu-helper"), QJsonObject{{"op", "set_gpu_mode"}, {"mode", mode}, {"force", force}}, this,
                    [this, mode](const privileged::Result &r) {
        gpuMode_->setEnabled(true);
        if (r.reached && r.json.value("needs_force").toBool()) {
            const auto a = QMessageBox::warning(this, "GPU mode — amdgpu missing", r.message() +
                "\n\nSwitch anyway? Only do this if you are about to boot a kernel that has amdgpu, "
                "or you know how to switch back in the BIOS setup (F2).", QMessageBox::Yes | QMessageBox::No, QMessageBox::No);
            if (a == QMessageBox::Yes) setGpuMode(mode, true); else readGpuMode();
            return;
        }
        if (!r.ok()) { showStatus("GPU mode: " + r.message(), 10000); readGpuMode(); return; }
        showStatus(r.json.value("reboot_pending").toBool() ? "GPU mode set — reboot to switch." : "GPU mode unchanged.", 8000);
        readGpuMode();
    }, 60000);
}
