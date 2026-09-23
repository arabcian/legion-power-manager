#include "inteltab.h"
#include "privileged.h"
#include "theme.h"

#include <QCheckBox>
#include <QComboBox>
#include <QDir>
#include <QDoubleSpinBox>
#include <QFile>
#include <QFileDialog>
#include <QScrollArea>
#include <QTimer>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QRegularExpression>
#include <QSaveFile>
#include <QSpinBox>
#include <QStandardItemModel>
#include <cmath>
#include <utility>
#include <QStandardPaths>
#include <QTime>
#include <QVBoxLayout>
#include <QVariant>

// Limits mirror intel_uv.rs; the helper re-checks everything.
static constexpr double UV_MIN = -300.0;  // GUI floor (helper accepts down to -1000)
static constexpr double ICC_MAX = 511.75;  // 11-bit field (not throttled's 10-bit 255.75)
static constexpr double UV_MAX_POSITIVE = 250.0;  // helper cap for allow_positive
static constexpr int MONITOR_MS = 2000;
static const char *BOOT_FILE = "/etc/legion-power-manager/intel-uv-boot.json";
static constexpr int PKEXEC_TIMEOUT_MS = 300000;

static QString helperPath() { return privileged::helperPath(QStringLiteral("intel-uv-helper")); }

QString inteluv::profilesDir() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) +
           QStringLiteral("/legion-power-manager/intel-uv-profiles");
}

static QGroupBox *box(const QString &title, const char *objName) {
    auto *b = new QGroupBox(title);
    b->setObjectName(QString::fromLatin1(objName));
    return b;
}
static QLabel *muted(const QString &t) {
    auto *l = new QLabel(t);
    l->setProperty("role", "muted");
    l->setWordWrap(true);
    return l;
}
static QDoubleSpinBox *dspin(double lo, double hi, double step, int dec, const QString &suffix) {
    auto *s = new QDoubleSpinBox;
    s->setRange(lo, hi);
    s->setSingleStep(step);
    s->setDecimals(dec);
    s->setSuffix(suffix);
    s->setFixedWidth(104);
    s->setAlignment(Qt::AlignRight);
    s->setEnabled(false);
    return s;
}

IntelTab::IntelTab(QWidget *parent) : QWidget(parent) {
    QDir().mkpath(inteluv::profilesDir());
    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(12, 10, 12, 10);
    root->setSpacing(6);

    // ── top: profile + global ──
    auto *top = new QHBoxLayout;
    auto *profBox = box("Profile", "box_purple");
    auto *pl = new QHBoxLayout(profBox);
    profileCombo_ = new QComboBox;
    profileCombo_->setMinimumWidth(160);
    auto *bLoad = new QPushButton("Load"), *bSave = new QPushButton("Save"), *bDel = new QPushButton("Delete");
    bDel->setObjectName("btnDanger");
    connect(bLoad, &QPushButton::clicked, this, [this] { loadProfile(profileCombo_->currentText()); });
    connect(bSave, &QPushButton::clicked, this, &IntelTab::saveProfile);
    connect(bDel, &QPushButton::clicked, this, &IntelTab::deleteProfile);
    pl->addWidget(profileCombo_, 1);
    pl->addWidget(bLoad);
    pl->addWidget(bSave);
    pl->addWidget(bDel);
    top->addWidget(profBox, 1);

    auto *gBox = box("Global", "box_grey");
    auto *gl = new QHBoxLayout(gBox);
    auto *bRead = new QPushButton("Read current");
    bRead->setToolTip("Read every value back from the CPU (MSR 0x150 mailbox, 0x1A2, 0x610, MCHBAR).");
    connect(bRead, &QPushButton::clicked, this, &IntelTab::readStatus);
    auto *bReset = new QPushButton("Reset voltages");
    bReset->setObjectName("btnDanger");
    bReset->setToolTip("Write 0 mV to all five voltage planes. IccMax, TCC offset and power limits are left alone\n"
                       "(their stock values are set by firmware and cannot be known).");
    connect(bReset, &QPushButton::clicked, this, [this] {
        if (QMessageBox::question(this, "Reset voltages", "Set every voltage offset back to 0 mV?",
                                  QMessageBox::Yes | QMessageBox::No, QMessageBox::No) == QMessageBox::Yes)
            applyReset();
    });
    gl->addWidget(bRead);
    gl->addWidget(bReset);
    top->addWidget(gBox);
    root->addLayout(top);

    info_ = new QLabel("Press <b>Read current</b> to query the CPU.");
    info_->setTextFormat(Qt::RichText);
    info_->setWordWrap(true);
    root->addWidget(info_);

    auto *monRow = new QHBoxLayout;
    monOn_ = new QCheckBox("Live monitor");
    monOn_->setToolTip("throttled --monitor: every 2 s read the throttle reasons (IA32_THERM_STATUS), VCore\n"
                       "(IA32_PERF_STATUS) and RAPL energy counters. One pkexec call per sample, only while this tab is visible.");
    monLbl_ = new QLabel;
    monLbl_->setTextFormat(Qt::RichText);
    monRow->addWidget(monOn_);
    monRow->addWidget(monLbl_, 1);
    root->addLayout(monRow);
    monTimer_ = new QTimer(this);
    monTimer_->setInterval(MONITOR_MS);
    connect(monTimer_, &QTimer::timeout, this, &IntelTab::pollMonitor);
    connect(monOn_, &QCheckBox::toggled, this, [this](bool on) {
        monPrev_ = {};
        if (limRun_) limRun_->setText(on ? "Stop counting" : "Start counting");
        if (on && isVisible()) { monTimer_->start(); pollMonitor(); } else { monTimer_->stop(); monLbl_->clear(); }
    });

    auto *mid = new QHBoxLayout;
    mid->setSpacing(8);
    auto *left = new QVBoxLayout, *right = new QVBoxLayout;
    left->setSpacing(6);
    right->setSpacing(6);

    // ── voltage ──
    auto *vBox = box("Voltage offsets  (OC mailbox, MSR 0x150)", "box_yellow");
    auto *vg = new QGridLayout(vBox);
    vg->setVerticalSpacing(3);
    const char *heads[] = {"Plane", "Offset", "Current"};
    for (int c = 0; c < 3; ++c) vg->addWidget(muted(QString::fromLatin1(heads[c])), 0, c);
    const QList<QPair<QString, QString>> planes{{"core", "CPU Core"}, {"cache", "CPU Cache"}, {"gpu", "Integrated GPU"},
                                                {"uncore", "System Agent"}, {"analogio", "Analog I/O"}};
    for (int i = 0; i < planes.size(); ++i) {
        auto *on = new QCheckBox(planes[i].second);
        auto *sp = dspin(UV_MIN, 0.0, 1.0, 1, " mV");
        auto *cur = new QLabel("–");
        cur->setMinimumWidth(80);
        connect(on, &QCheckBox::toggled, sp, &QWidget::setEnabled);
        on->setToolTip("Unchecked = this plane is not written.");
        vg->addWidget(on, i + 1, 0);
        vg->addWidget(sp, i + 1, 1);
        vg->addWidget(cur, i + 1, 2);
        volt_.append({planes[i].first, on, sp, cur});
    }
    link_ = new QCheckBox("Link Core && Cache");
    link_->setChecked(true);
    link_->setToolTip("Skylake and later: core and cache are one voltage plane. The SMALLER of the two offsets is\n"
                      "applied to both, so they should always be written with the same value (undervolt.py warns about this).");
    auto sync = [this](int from) {
        if (!link_->isChecked()) return;
        const int to = from == 0 ? 1 : 0;
        QSignalBlocker b1(volt_[to].on), b2(volt_[to].val);
        volt_[to].on->setChecked(volt_[from].on->isChecked());
        volt_[to].val->setEnabled(volt_[from].on->isChecked());
        volt_[to].val->setValue(volt_[from].val->value());
    };
    for (int i : {0, 1}) {
        connect(volt_[i].on, &QCheckBox::toggled, this, [sync, i] { sync(i); });
        connect(volt_[i].val, &QDoubleSpinBox::valueChanged, this, [sync, i] { sync(i); });
    }
    vg->addWidget(link_, planes.size() + 1, 0, 1, 3);
    vg->addWidget(muted("Negative = undervolt. Start around −50 mV, test (stress + idle + suspend/resume), then step "
                        "down 10 mV at a time. Too far = freezes / MCEs; the offset is lost on reboot and S3, so a bad "
                        "value is recovered by a power cycle."), planes.size() + 2, 0, 1, 3);
    vg->setColumnStretch(2, 1);
    vg->setRowStretch(planes.size() + 3, 1);
    left->addWidget(vBox);

    // ── extras ──
    auto *xBox = box("Extras", "box_purple");
    auto *xg = new QGridLayout(xBox);
    xg->setVerticalSpacing(3);
    positive_ = new QCheckBox("Allow positive offsets (overvolt, max +250 mV)");
    positive_->setToolTip("undervolt.py --force. Only for stabilising a marginal chip; throttled refuses positive offsets.");
    connect(positive_, &QCheckBox::toggled, this, &IntelTab::setPositive);
    xg->addWidget(positive_, 0, 0, 1, 2);
    xg->addWidget(new QLabel("BD PROCHOT"), 1, 0);
    bdprochot_ = new QComboBox;
    bdprochot_->addItems({"untouched", "disable", "enable"});
    bdprochot_->setToolTip("throttled Disable_BDPROCHOT (MSR_POWER_CTL bit 0). Bi-directional PROCHOT lets the EC/charger\n"
                           "throttle the CPU to minimum clocks. Disabling it removes that safety net: only for machines that\n"
                           "throttle wrongly (bad charger/sensor). The EC may turn it back on; the daemon re-applies it.");
    xg->addWidget(bdprochot_, 1, 1);
    xg->addWidget(new QLabel("cTDP level"), 2, 0);
    ctdp_ = new QComboBox;
    ctdp_->addItems({"untouched", "0 · nominal", "1 · down", "2 · up"});
    ctdp_->setToolTip("throttled cTDP (MSR_CONFIG_TDP_CONTROL). Only levels the CPU advertises in PLATFORM_INFO are accepted.");
    xg->addWidget(ctdp_, 2, 1);
    lock_ = new QCheckBox("Lock PL1/PL2 until reboot");
    lock_->setToolTip("undervolt.py --lock-power-limit: sets bit 63 of MSR_PKG_POWER_LIMIT after writing, so neither\n"
                      "firmware nor software (including this tool) can change it until the next reset.");
    xg->addWidget(lock_, 3, 0, 1, 2);
    auto *bTs = new QPushButton("Import ThrottleStop.ini…");
    bTs->setToolTip("undervolt.py --throttlestop: read FIVRVoltage offsets from a ThrottleStop profile.");
    connect(bTs, &QPushButton::clicked, this, &IntelTab::importThrottleStop);
    xg->addWidget(bTs, 4, 0, 1, 2);
    xg->setColumnStretch(1, 1);
    left->addWidget(xBox);

    // ── limit reasons ──
    auto *rBox = box("Limit reasons  (MSR 0x64F / 0x6B0 / 0x6B1)", "box_blue");
    auto *rg = new QGridLayout(rBox);
    rg->setVerticalSpacing(2);
    rg->setHorizontalSpacing(14);
    rg->addWidget(muted("Reason"), 0, 0);
    const char *doms[] = {"core", "gpu", "ring"}, *domLabels[] = {"Core", "iGPU", "Ring"};
    for (int d = 0; d < 3; ++d) rg->addWidget(muted(QString::fromLatin1(domLabels[d])), 0, d + 1, Qt::AlignRight);
    // (bit, label, domains mask core|gpu|ring, tooltip) — SDM client definitions; a
    // domain without that bit shows "–".
    struct Reason { int bit; const char *label; int mask; const char *tip; };
    static const Reason reasons[] = {
        {0, "PROCHOT", 7, "External PROCHOT# (EC, charger, VRM) forced the minimum frequency."},
        {1, "Thermal", 7, "Die temperature reached the TCC target (TjMax − TCC offset)."},
        {5, "Avg thermal (RATL)", 7, "Running-average thermal limit."},
        {6, "VR thermal alert", 7, "Voltage regulator over-temperature."},
        {7, "VR TDC", 7, "VR thermal design current limit."},
        {8, "EDP / IccMax", 7, "Electrical design point: the IccMax current limit (or other electrical limit) was hit."},
        {10, "PL1", 7, "Package long-term power limit."},
        {11, "PL2", 7, "Package short-term power limit."},
        {12, "Max turbo / inefficient", 3, "Core: multi-core turbo ratio limit. iGPU: inefficient-operation limit."},
        {13, "Turbo attenuation", 1, "Turbo transition attenuation (frequent turbo changes damped)."},
        {4, "Residency regulation", 1, "Residency state regulation (C-state based)."},
    };
    int rr = 1;
    for (const Reason &r : reasons) {
        auto *name = new QLabel(QString::fromLatin1(r.label));
        name->setToolTip(QString::fromLatin1(r.tip));
        rg->addWidget(name, rr, 0);
        for (int d = 0; d < 3; ++d) {
            auto *c = new QLabel(r.mask & (1 << d) ? QStringLiteral("·") : QStringLiteral("–"));
            c->setAlignment(Qt::AlignRight | Qt::AlignVCenter);
            c->setMinimumWidth(52);
            c->setTextFormat(Qt::RichText);
            rg->addWidget(c, rr, d + 1);
            if (r.mask & (1 << d)) limCells_.insert(QStringLiteral("%1:%2").arg(QString::fromLatin1(doms[d])).arg(r.bit), c);
        }
        ++rr;
    }
    limInfo_ = muted("Counts samples (every 2 s) in which each reason limited the frequency. "
                     "Red = limiting now, amber = happened (sticky log bit or counted).");
    rg->addWidget(limInfo_, rr++, 0, 1, 4);
    auto *lr2 = new QHBoxLayout;
    limRun_ = new QPushButton("Start counting");
    connect(limRun_, &QPushButton::clicked, this, [this] { monOn_->setChecked(!monOn_->isChecked()); });
    auto *bLimReset = new QPushButton("Reset");
    bLimReset->setToolTip("Zero the counters and clear the CPU's sticky log bits.");
    connect(bLimReset, &QPushButton::clicked, this, &IntelTab::resetLimits);
    lr2->addWidget(limRun_);
    lr2->addWidget(bLimReset);
    lr2->addStretch();
    rg->addLayout(lr2, rr, 0, 1, 4);
    rg->setColumnStretch(0, 1);
    left->addWidget(rBox);
    left->addStretch();

    // ── limits ──
    auto *lBox = box("Limits", "box_blue");
    auto *lg = new QGridLayout(lBox);
    lg->setVerticalSpacing(3);
    int r = 0;
    lg->addWidget(muted("IccMax (A)"), r++, 0, 1, 4);
    for (const auto &[k, label] : QList<QPair<QString, QString>>{{"core", "Core"}, {"cache", "Cache"}, {"gpu", "iGPU"}}) {
        auto *on = new QCheckBox(label);
        auto *sp = dspin(1.0, ICC_MAX, 1.0, 2, " A");
        sp->setValue(100.0);
        auto *cur = new QLabel("–");
        connect(on, &QCheckBox::toggled, sp, &QWidget::setEnabled);
        on->setToolTip("Maximum current of this VR domain (0.25 A steps, floored, up to 511.75 A). Raising it can stop\n"
                       "current-limit throttling; lowering it caps power. Read current first: a value below the stock\n"
                       "limit shown on the right throttles the CPU. Supported by fewer CPUs than voltage offsets.");
        lg->addWidget(on, r, 0);
        lg->addWidget(sp, r, 1);
        lg->addWidget(cur, r++, 2, 1, 2);
        icc_.append({k, on, sp, cur});
    }
    lg->addWidget(muted("Thermal"), r++, 0, 1, 4);
    tjOn_ = new QCheckBox("TCC offset");
    tj_ = new QSpinBox;
    tj_->setRange(0, 63);
    tj_->setSuffix(" °C");
    tj_->setFixedWidth(104);
    tj_->setAlignment(Qt::AlignRight);
    tj_->setEnabled(false);
    tjCur_ = new QLabel("–");
    connect(tjOn_, &QCheckBox::toggled, tj_, &QWidget::setEnabled);
    tjOn_->setToolTip("MSR 0x1A2 bits 29:24: throttle at TjMax − offset (read-modify-write, target kept ≥ 40 °C).\n"
                      "Same register as Optimizations → CPU → TCC offset (intel_tcc_cooling).");
    lg->addWidget(tjOn_, r, 0);
    lg->addWidget(tj_, r, 1);
    lg->addWidget(tjCur_, r++, 2, 1, 2);

    lg->addWidget(muted("Package power (MSR 0x610)"), r++, 0, 1, 4);
    auto mkPl = [&](const QString &label, double w, double s, const QString &tip) {
        PlRow p{new QCheckBox(label), dspin(1, 1000, 1, 0, " W"), dspin(0.001, 1000, 0.5, 3, " s"), new QLabel("–")};
        p.w->setValue(w);
        p.s->setValue(s);
        p.s->setFixedWidth(96);
        p.on->setToolTip(tip);
        connect(p.on, &QCheckBox::toggled, p.w, &QWidget::setEnabled);
        connect(p.on, &QCheckBox::toggled, p.s, &QWidget::setEnabled);
        lg->addWidget(p.on, r, 0);
        lg->addWidget(p.w, r, 1);
        lg->addWidget(p.s, r, 2);
        lg->addWidget(p.cur, r++, 3);
        return p;
    };
    pl1_ = mkPl("PL1", 55, 28, "Long-term package power limit and its time window (nearest encodable window is used).");
    pl2_ = mkPl("PL2", 90, 0.002, "Short-term package power limit and its time window.");
    mchbar_ = new QCheckBox("Mirror to MCHBAR (MMIO)");
    mchbar_->setChecked(true);
    mchbar_->setToolTip("Write the same value to MCHBAR+0x59A0 (via /dev/mem) like intel-undervolt and throttled do:\n"
                        "the effective limit is the lower of the MSR and MMIO copies. Skipped automatically if the\n"
                        "host bridge is unknown or /dev/mem is blocked.");
    lg->addWidget(mchbar_, r++, 0, 1, 4);
    plLock_ = muted("On a Legion the EC re-programs PL1/PL2 on every platform-profile change — prefer the Firmware "
                    "Attributes tab (Custom) for lasting limits.");
    lg->addWidget(plLock_, r++, 0, 1, 4);
    lg->setColumnStretch(3, 1);
    lg->setRowStretch(r, 1);
    right->addWidget(lBox);

    // ── boot / daemon ──
    auto *dBox = box("Boot, resume & daemon", "box_green");
    auto *dg = new QGridLayout(dBox);
    dg->setVerticalSpacing(3);
    dg->addWidget(new QLabel("Store for"), 0, 0);
    bootTarget_ = new QComboBox;
    bootTarget_->addItems({"AC and battery", "AC only", "Battery only"});
    bootTarget_->setToolTip("throttled [AC]/[BATTERY]: separate profiles per power source. The other slot in the\n"
                            "stored file is kept; the daemon switches when the charger is plugged or unplugged.");
    dg->addWidget(bootTarget_, 0, 1, 1, 3);
    reapply_ = new QCheckBox("Re-apply every");
    reapply_->setChecked(true);
    reapply_->setToolTip("Daemon only (intel-undervolt daemon / throttled Update_Rate_s): re-write power limits, TCC,\n"
                         "BD PROCHOT and cTDP each interval — the EC restores its own PL1/PL2 on profile changes.\n"
                         "Voltages are written on start, power-source change and resume.");
    interval_ = new QSpinBox;
    interval_->setRange(500, 600000);
    interval_->setSingleStep(500);
    interval_->setValue(5000);
    interval_->setSuffix(" ms");
    dg->addWidget(reapply_, 1, 0);
    dg->addWidget(interval_, 1, 1);
    hwpOn_ = new QCheckBox("EPP switching (hwphint)");
    hwpOn_->setToolTip("intel-undervolt hwphint, run by the daemon: switch energy_performance_preference between two\n"
                       "hints by CPU load or RAPL power. 'switch' only touches CPUs whose EPP is already one of the two\n"
                       "(so Optimizations settings are respected); 'force' always writes.");
    dg->addWidget(hwpOn_, 2, 0, 1, 4);
    hwpMode_ = new QComboBox; hwpMode_->addItems({"switch", "force"});
    hwpAlgo_ = new QComboBox; hwpAlgo_->addItems({"load", "power"});
    hwpMulti_ = new QCheckBox("all cores");
    hwpMulti_->setToolTip("load: unchecked = busiest single CPU, checked = average over all CPUs.");
    hwpThreshold_ = dspin(0.05, 1.0, 0.05, 2, "");
    hwpThreshold_->setValue(0.8);
    hwpDomain_ = new QComboBox; hwpDomain_->addItems({"package", "core", "uncore", "dram", "psys"});
    hwpCmp_ = new QComboBox; hwpCmp_->addItems({">", "<"});
    hwpWatts_ = dspin(0, 1000, 1, 1, " W");
    hwpWatts_->setValue(8);
    QStringList epp{"performance", "balance_performance", "default", "balance_power", "power"};
    if (auto t = QFile("/sys/devices/system/cpu/cpufreq/policy0/energy_performance_available_preferences"); t.open(QIODevice::ReadOnly)) {
        const QStringList a = QString::fromLatin1(t.readAll()).split(' ', Qt::SkipEmptyParts);
        if (!a.isEmpty()) { epp = a; for (QString &x : epp) x = x.trimmed(); }
    }
    hwpLoadHint_ = new QComboBox; hwpLoadHint_->addItems(epp);
    hwpNormalHint_ = new QComboBox; hwpNormalHint_->addItems(epp);
    hwpLoadHint_->setCurrentText("performance");
    hwpNormalHint_->setCurrentText("balance_performance");
    dg->addWidget(new QLabel("mode"), 3, 0); dg->addWidget(hwpMode_, 3, 1);
    dg->addWidget(new QLabel("by"), 3, 2); dg->addWidget(hwpAlgo_, 3, 3);
    auto *loadRow = new QWidget; auto *lr = new QHBoxLayout(loadRow); lr->setContentsMargins(0, 0, 0, 0);
    lr->addWidget(new QLabel("load ≥")); lr->addWidget(hwpThreshold_); lr->addWidget(hwpMulti_); lr->addStretch();
    auto *powRow = new QWidget; auto *pr = new QHBoxLayout(powRow); pr->setContentsMargins(0, 0, 0, 0);
    pr->addWidget(hwpDomain_); pr->addWidget(hwpCmp_); pr->addWidget(hwpWatts_); pr->addStretch();
    dg->addWidget(loadRow, 4, 0, 1, 4);
    dg->addWidget(powRow, 5, 0, 1, 4);
    dg->addWidget(new QLabel("busy →"), 6, 0); dg->addWidget(hwpLoadHint_, 6, 1, 1, 3);
    dg->addWidget(new QLabel("idle →"), 7, 0); dg->addWidget(hwpNormalHint_, 7, 1, 1, 3);
    auto hwpSync = [this, loadRow, powRow] {
        const bool on = hwpOn_->isChecked(), load = hwpAlgo_->currentIndex() == 0;
        for (QWidget *w : std::initializer_list<QWidget *>{hwpMode_, hwpAlgo_, hwpLoadHint_, hwpNormalHint_}) w->setEnabled(on);
        loadRow->setVisible(load); powRow->setVisible(!load);
        loadRow->setEnabled(on); powRow->setEnabled(on);
        hwpThreshold_->setEnabled(on); hwpWatts_->setEnabled(on);
    };
    connect(hwpOn_, &QCheckBox::toggled, this, hwpSync);
    connect(hwpAlgo_, &QComboBox::currentIndexChanged, this, hwpSync);
    hwpSync();
    dg->addWidget(muted("Periodic re-apply and EPP switching need the daemon: rc-update add lpm-intel-uv-daemon default "
                        "(systemd: lpm-intel-uv-daemon.service). Without it, lpm-intel-uv applies once at boot and after resume."), 8, 0, 1, 4);
    dg->setColumnStretch(3, 1);
    right->addWidget(dBox);
    right->addStretch();

    mid->addLayout(left, 1);
    mid->addLayout(right, 1);
    auto *midHost = new QWidget;
    midHost->setObjectName("intelMid");
    midHost->setStyleSheet("#intelMid { background: transparent; }");
    midHost->setLayout(mid);
    auto *scroll = new QScrollArea;
    scroll->setWidgetResizable(true);
    scroll->setFrameShape(QFrame::NoFrame);
    scroll->setWidget(midHost);
    root->addWidget(scroll, 1);

    // ── actions ──
    auto *aBox = box("Apply", "box_green");
    auto *al = new QHBoxLayout(aBox);
    auto *bApply = new QPushButton("Apply");
    bApply->setObjectName("btnAccent");
    bApply->setFixedWidth(132);
    connect(bApply, &QPushButton::clicked, this, [this] {
        const QJsonObject p = currentProfile();
        if (validate(p)) runOp({{"op", "apply"}, {"profile", p}}, [this](const QJsonObject &) { readStatus(); });
    });
    auto *bBoot = new QPushButton("⏻ Apply at boot && resume");
    bBoot->setToolTip("Store these values as /etc/legion-power-manager/intel-uv-boot.json. The lpm-intel-uv service\n"
                      "applies it at boot, and the elogind / systemd sleep hook re-applies it after resume\n"
                      "(offsets are lost on S3 suspend). Test the values with Apply first.");
    connect(bBoot, &QPushButton::clicked, this, &IntelTab::saveBoot);
    auto *bClear = new QPushButton("Clear boot");
    connect(bClear, &QPushButton::clicked, this, [this] { runOp({{"op", "clear_boot"}}); });
    bootLbl_ = muted("");
    al->addWidget(bootLbl_, 1);
    al->addWidget(bClear);
    al->addWidget(bBoot);
    al->addWidget(bApply);
    root->addWidget(aBox);
    buttons_ = {bRead, bReset, bApply, bBoot, bClear, bTs};

    auto *logBox = box("Output / Log", "box_grey");
    auto *ll = new QVBoxLayout(logBox);
    log_ = new QPlainTextEdit;
    log_->setReadOnly(true);
    log_->setObjectName("terminal");
    log_->setMaximumBlockCount(2000);
    log_->setMinimumHeight(54);
    log_->setMaximumHeight(90);
    ll->addWidget(log_);
    logBox->setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Maximum);
    aBox->setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Maximum);
    root->addWidget(logBox, 0);

    reloadProfiles();
    log("Intel CPU detected. Nothing is written until you press Apply.");
}

void IntelTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    // First visit reads the CPU once (one pkexec round trip; silent for wheel via the polkit rule).
    if (!readOnce_ && !qEnvironmentVariableIsSet("LPM_PGO_TRAIN")) { readOnce_ = true; readStatus(); }
    if (monOn_->isChecked()) monTimer_->start();
}

void IntelTab::hideEvent(QHideEvent *e) {
    QWidget::hideEvent(e);
    monTimer_->stop();  // no pkexec polling while the tab (or window) is hidden
    monPrev_ = {};
}

void IntelTab::log(const QString &msg, const QString &level) {
    static const QHash<QString, QPair<QString, QString>> style{
        {"info", {theme::FG_DIM, "  "}}, {"ok", {theme::OK, "OK"}}, {"err", {theme::DANGER, "!!"}}, {"cmd", {theme::WARN, ">>"}}};
    const auto [color, prefix] = style.value(level, style.value("info"));
    log_->appendHtml(QStringLiteral("<span style=\"color:%1;\">[%2]</span> <span style=\"color:%3;\">%4 %5</span>")
        .arg(theme::MUTED, QTime::currentTime().toString("HH:mm:ss"), color, prefix,
             msg.toHtmlEscaped().replace('\n', "<br>")));
}

// ── profile <-> widgets ─────────────────────────────────────────────────────

QJsonObject IntelTab::currentProfile() const {
    QJsonObject v, i;
    for (const Row &r : volt_) v[r.key] = r.on->isChecked() ? QJsonValue(r.val->value()) : QJsonValue();
    for (const Row &r : icc_) i[r.key] = r.on->isChecked() ? QJsonValue(r.val->value()) : QJsonValue();
    auto pl = [](const PlRow &p) { return p.on->isChecked() ? QJsonValue(QJsonObject{{"watts", p.w->value()}, {"seconds", p.s->value()}}) : QJsonValue(); };
    const int bd = bdprochot_->currentIndex(), ct = ctdp_->currentIndex();
    const bool anyPl = pl1_.on->isChecked() || pl2_.on->isChecked();
    return {{"voltage", v}, {"iccmax", i}, {"tjoffset", tjOn_->isChecked() ? QJsonValue(tj_->value()) : QJsonValue()},
            {"power", QJsonObject{{"pl1", pl(pl1_)}, {"pl2", pl(pl2_)}, {"mchbar", mchbar_->isChecked()}}},
            {"allow_positive", positive_->isChecked()}, {"lock_power", lock_->isChecked() && anyPl},
            {"disable_bdprochot", bd == 0 ? QJsonValue() : QJsonValue(bd == 1)},
            {"ctdp", ct == 0 ? QJsonValue() : QJsonValue(ct - 1)},
            {"hwphint", hwpRules()}};
}

QJsonArray IntelTab::hwpRules() const {
    if (!hwpOn_->isChecked()) return {};
    QJsonObject r{{"mode", hwpMode_->currentText()}, {"load_hint", hwpLoadHint_->currentText()},
                  {"normal_hint", hwpNormalHint_->currentText()}};
    if (hwpAlgo_->currentIndex() == 0) r["load"] = QJsonObject{{"multi", hwpMulti_->isChecked()}, {"threshold", hwpThreshold_->value()}};
    else r["power"] = QJsonArray{QJsonObject{{"domain", hwpDomain_->currentText()}, {"gt", hwpCmp_->currentIndex() == 0},
                                             {"watts", hwpWatts_->value()}}};
    return {r};
}

void IntelTab::setProfile(const QJsonObject &p) {
    QSignalBlocker lb(link_);
    const bool wasLinked = link_->isChecked();
    link_->setChecked(false);  // load both planes verbatim, then re-link
    auto fill = [](QList<Row> &rows, const QJsonObject &o) {
        for (Row &r : rows) {
            const QJsonValue v = o.value(r.key);
            r.on->setChecked(v.isDouble());
            if (v.isDouble()) r.val->setValue(v.toDouble());
        }
    };
    fill(volt_, p.value("voltage").toObject());
    fill(icc_, p.value("iccmax").toObject());
    tjOn_->setChecked(p.value("tjoffset").isDouble());
    if (tjOn_->isChecked()) tj_->setValue(qAbs(p.value("tjoffset").toInt()));
    const QJsonObject pw = p.value("power").toObject();
    for (auto [row, key] : {std::pair{&pl1_, "pl1"}, std::pair{&pl2_, "pl2"}}) {
        const QJsonObject t = pw.value(key).toObject();
        row->on->setChecked(!t.isEmpty());
        if (!t.isEmpty()) { row->w->setValue(t.value("watts").toDouble()); row->s->setValue(t.value("seconds").toDouble(row->s->value())); }
    }
    mchbar_->setChecked(pw.value("mchbar").toBool(true));
    positive_->setChecked(p.value("allow_positive").toBool());
    lock_->setChecked(p.value("lock_power").toBool());
    const QJsonValue bd = p.value("disable_bdprochot");
    bdprochot_->setCurrentIndex(bd.isBool() ? (bd.toBool() ? 1 : 2) : 0);
    const QJsonValue ct = p.value("ctdp");
    ctdp_->setCurrentIndex(ct.isDouble() ? ct.toInt() + 1 : 0);
    const QJsonArray hw = p.value("hwphint").toArray();
    hwpOn_->setChecked(!hw.isEmpty());
    if (!hw.isEmpty()) {
        const QJsonObject r = hw.first().toObject();
        hwpMode_->setCurrentText(r.value("mode").toString("switch"));
        hwpLoadHint_->setCurrentText(r.value("load_hint").toString());
        hwpNormalHint_->setCurrentText(r.value("normal_hint").toString());
        if (r.contains("load")) {
            hwpAlgo_->setCurrentIndex(0);
            hwpMulti_->setChecked(r.value("load").toObject().value("multi").toBool());
            hwpThreshold_->setValue(r.value("load").toObject().value("threshold").toDouble(0.8));
        } else {
            hwpAlgo_->setCurrentIndex(1);
            const QJsonObject t = r.value("power").toArray().first().toObject();
            hwpDomain_->setCurrentText(t.value("domain").toString("package"));
            hwpCmp_->setCurrentIndex(t.value("gt").toBool(true) ? 0 : 1);
            hwpWatts_->setValue(t.value("watts").toDouble());
        }
        if (hw.size() > 1) log(QStringLiteral("Profile has %1 hwphint rules; only the first is editable here (all are kept in the file).").arg(hw.size()), "cmd");
    }
    link_->setChecked(wasLinked && volt_[0].on->isChecked() == volt_[1].on->isChecked() && volt_[0].val->value() == volt_[1].val->value());
}

bool IntelTab::validate(const QJsonObject &p) {
    bool any = false;
    for (const char *k : {"voltage", "iccmax"})
        for (const auto &v : p.value(k).toObject()) any |= v.isDouble();
    any |= p.value("tjoffset").isDouble();
    const QJsonObject pw = p.value("power").toObject();
    any |= pw.value("pl1").isObject() || pw.value("pl2").isObject();
    any |= p.value("disable_bdprochot").isBool() || p.value("ctdp").isDouble() || !p.value("hwphint").toArray().isEmpty();
    if (!any) { QMessageBox::information(this, "Nothing to apply", "Tick at least one value to write."); return false; }
    const QJsonObject v = p.value("voltage").toObject();
    if (v.value("core").isDouble() != v.value("cache").isDouble() || v.value("core").toDouble() != v.value("cache").toDouble())
        if (QMessageBox::warning(this, "Core / Cache differ",
                "Core and Cache are set differently. On Skylake and later they share one plane and the smaller offset "
                "is applied to both.\n\nApply anyway?", QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
            return false;
    QStringList lower;
    for (const Row &r : icc_) {
        const QVariant cur = r.cur->property("amps");
        const QJsonValue want = p.value("iccmax").toObject().value(r.key);
        if (want.isDouble() && cur.isValid() && want.toDouble() < cur.toDouble())
            lower << QStringLiteral("%1: %2 A → %3 A").arg(r.on->text()).arg(cur.toDouble(), 0, 'f', 2).arg(want.toDouble(), 0, 'f', 2);
    }
    if (!lower.isEmpty() &&
        QMessageBox::warning(this, "IccMax below stock", "These IccMax values are LOWER than the limits read from the CPU and "
            "will cause current-limit throttling:\n\n" + lower.join('\n') + "\n\nContinue?",
            QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return false;
    if (p.value("lock_power").toBool() &&
        QMessageBox::warning(this, "Lock power limits", "PL1/PL2 will be LOCKED until the next reboot. Nothing — not the EC, "
            "not this program — can change them before that.\n\nContinue?", QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return false;
    if (p.value("disable_bdprochot").toBool() &&
        QMessageBox::warning(this, "Disable BD PROCHOT", "Disabling BD PROCHOT removes the EC's emergency throttle "
            "(charger overload, battery or VRM over-temperature).\n\nContinue?", QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return false;
    return true;
}

void IntelTab::setPositive(bool on) {
    for (Row &r : volt_) r.val->setMaximum(on ? UV_MAX_POSITIVE : 0.0);
}

void IntelTab::importThrottleStop() {
    const QString path = QFileDialog::getOpenFileName(this, "ThrottleStop.ini", QDir::homePath(), "ThrottleStop (*.ini);;All files (*)");
    if (path.isEmpty()) return;
    bool ok = false;
    const int idx = QInputDialog::getInt(this, "ThrottleStop profile", "Profile index (0-3):", 0, 0, 3, 1, &ok);
    if (!ok) return;
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly) || f.size() > 1024 * 1024) { log("Cannot read " + path, "err"); return; }
    // Same parse as intel_uv::parse_throttlestop / undervolt.py: [ThrottleStop] FIVRVoltage{plane}{profile}=hex.
    const QStringList keys{"core", "gpu", "cache", "uncore", "analogio"};  // plane index order
    bool inSec = false;
    int n = 0;
    for (const QString &raw : QString::fromUtf8(f.readAll()).split('\n')) {
        const QString l = raw.trimmed();
        if (l.startsWith('[')) { inSec = l.compare("[ThrottleStop]", Qt::CaseInsensitive) == 0; continue; }
        const int eq = l.indexOf('=');
        if (!inSec || eq < 0) continue;
        const QString k = l.left(eq).trimmed();
        for (int plane = 0; plane < keys.size(); ++plane) {
            if (k.compare(QStringLiteral("FIVRVoltage%1%2").arg(plane).arg(idx), Qt::CaseInsensitive) != 0) continue;
            QString hex = l.mid(eq + 1).trimmed();
            if (hex.startsWith("0x", Qt::CaseInsensitive)) hex = hex.mid(2);
            bool okh = false;
            const quint64 v = hex.toULongLong(&okh, 16);
            if (!okh || v == 0) continue;
            int t = int(((v & 0xFFFFFFFFull) >> 21) & 0x7FF);
            if (t >= 0x400) t -= 0x800;
            const double mv = qRound(t / 1.024 * 10.0) / 10.0;
            for (Row &r : volt_) if (r.key == keys[plane]) { r.on->setChecked(true); r.val->setValue(mv); }
            log(QStringLiteral("ThrottleStop %1 → %2 mV").arg(keys[plane]).arg(mv, 0, 'f', 1));
            ++n;
        }
    }
    if (!n) log(QStringLiteral("No non-zero FIVRVoltage entries for profile %1 in %2").arg(idx).arg(path), "err");
    else log("Imported — review the values, then Apply.", "ok");
}

void IntelTab::saveBoot() {
    const QJsonObject p = currentProfile();
    if (!validate(p)) return;
    // Keep the other power-source slot of the stored file (world-readable, no root needed to read).
    QJsonObject cfg;
    if (QFile f(QString::fromLatin1(BOOT_FILE)); f.open(QIODevice::ReadOnly)) {
        const QJsonObject old = QJsonDocument::fromJson(f.readAll()).object();
        if (old.contains("ac") || old.contains("battery")) cfg = old;
        else if (!old.isEmpty()) cfg = {{"ac", old}, {"battery", old}};  // legacy single profile
    }
    const int t = bootTarget_->currentIndex();
    if (t == 0 || t == 1) cfg["ac"] = p;
    if (t == 0 || t == 2) cfg["battery"] = p;
    if (!cfg.contains("ac")) cfg["ac"] = QJsonValue();
    if (!cfg.contains("battery")) cfg["battery"] = QJsonValue();
    cfg["daemon"] = QJsonObject{{"interval_ms", interval_->value()}, {"reapply", reapply_->isChecked()}};
    if (QMessageBox::question(this, "Apply at boot", QStringLiteral("Store these values for %1 and apply them at every boot and resume?\n\n"
            "Only do this with values you have already tested.").arg(bootTarget_->currentText()),
            QMessageBox::Yes | QMessageBox::No, QMessageBox::No) == QMessageBox::Yes)
        runOp({{"op", "set_boot"}, {"config", cfg}});
}

void IntelTab::pollMonitor() {
    if (busy_ || monInFlight_ || !isVisible()) return;
    monInFlight_ = true;
    const bool clear = std::exchange(clearLogsNext_, false);
    privileged::run(helperPath(), QJsonObject{{"op", "monitor"}, {"clear_logs", clear}}, this, [this](const privileged::Result &r) {
        monInFlight_ = false;
        if (!r.ok()) { monLbl_->setText(QStringLiteral("<span style='color:%1'>%2</span>").arg(theme::DANGER, r.message().toHtmlEscaped())); return; }
        showMonitor(r.json);
    }, 15000);
}

void IntelTab::showMonitor(const QJsonObject &s) {
    const QJsonObject t = s.value("throttle").toObject();
    auto lim = [&](const char *k, const char *label) {
        const bool on = t.value(k).toBool(), logged = t.value("log").toObject().value(k).toBool();
        return QStringLiteral("%1 <span style='color:%2'>%3</span>").arg(label, on ? theme::DANGER : logged ? theme::WARN : theme::OK,
                                                                           on ? "LIM" : logged ? "was" : "OK");
    };
    QStringList parts{lim("thermal", "Thermal"), lim("power", "Power"), lim("current", "Current"), lim("cross_domain", "Cross-domain")};
    if (s.contains("vcore_mv")) parts << QStringLiteral("VCore %1 mV").arg(s.value("vcore_mv").toDouble(), 0, 'f', 0);
    const QJsonObject e = s.value("energy").toObject(), pe = monPrev_.value("energy").toObject();
    const double dt = s.value("t").toDouble() - monPrev_.value("t").toDouble();
    if (!pe.isEmpty() && dt > 0) {
        const double wrap = e.value("wrap").toDouble(4294967296.0);
        for (const char *d : {"package", "core", "graphics", "dram"}) {
            const QJsonValue a = e.value("raw").toObject().value(d), b = pe.value("raw").toObject().value(d);
            if (!a.isDouble() || !b.isDouble()) continue;
            const double unit = e.value(QString::fromLatin1(d) == "dram" ? "dram_unit_j" : "unit_j").toDouble();
            parts << QStringLiteral("%1 %2 W").arg(QString::fromLatin1(d)).arg(std::fmod(a.toDouble() - b.toDouble() + wrap, wrap) * unit / dt, 0, 'f', 1);
        }
    }
    monPrev_ = s;
    monLbl_->setText(parts.join(" · "));
    updateLimits(s.value("limits").toObject());
}

void IntelTab::updateLimits(const QJsonObject &limits) {
    if (limits.isEmpty()) {
        limInfo_->setText("This CPU returned none of the limit-reason registers.");
        return;
    }
    ++limSamples_;
    for (auto it = limCells_.cbegin(); it != limCells_.cend(); ++it) {
        const QString dom = it.key().section(':', 0, 0);
        const int bit = it.key().section(':', 1).toInt();
        if (!limits.contains(dom)) { it.value()->setText("n/a"); continue; }
        const quint64 v = quint64(limits.value(dom).toDouble());
        const bool now = (v >> bit) & 1, logged = (v >> (bit + 16)) & 1;
        int &n = limCounts_[it.key()];
        if (now) ++n;
        const QString color = now ? theme::DANGER : (logged || n) ? theme::WARN : theme::MUTED;
        it.value()->setText(QStringLiteral("<span style='color:%1'>● %2</span>").arg(color).arg(n));
        it.value()->setToolTip(now ? "limiting now" : logged ? "happened since the last reset (sticky log bit)" : QString());
    }
    limInfo_->setText(QStringLiteral("%1 samples (~%2 s). Red = limiting now, amber = happened since reset.")
                          .arg(limSamples_).arg(limSamples_ * MONITOR_MS / 1000));
}

void IntelTab::resetLimits() {
    limCounts_.clear();
    limSamples_ = 0;
    clearLogsNext_ = true;  // the next monitor sample clears the sticky log bits
    for (QLabel *c : std::as_const(limCells_)) { c->setText(QStringLiteral("·")); c->setToolTip({}); }
    limInfo_->setText(monOn_->isChecked() ? QStringLiteral("Counters reset.") : QStringLiteral("Counters reset; press Start counting."));
}

// ── helper calls ────────────────────────────────────────────────────────────

void IntelTab::setBusy(bool b) {
    busy_ = b;
    for (QPushButton *p : std::as_const(buttons_)) p->setEnabled(!b);
}

void IntelTab::runOp(const QJsonObject &req, std::function<void(const QJsonObject &)> then) {
    if (busy_) { log("Previous operation still running, please wait.", "err"); return; }
    const QString op = req.value("op").toString();
    if (op != "status") log("Sending '" + op + "' via pkexec…", "cmd");
    setBusy(true);
    privileged::run(helperPath(), req, this, [this, op, then](const privileged::Result &r) {
        setBusy(false);
        if (!r.reached) { log(r.error, "err"); return; }
        const QJsonObject j = r.json;
        if (j.contains("boot_profile"))
            bootLbl_->setText(j.value("boot_profile").toBool()
                ? QStringLiteral("<span style='color:%1'>⏻ boot/resume profile active</span>").arg(theme::OK)
                : QStringLiteral("No boot/resume profile."));
        for (const auto &v : j.value("results").toArray()) {
            const QJsonObject o = v.toObject();
            log(o.value("what").toString() + ": " + o.value("message").toString(), o.value("ok").toBool() ? "ok" : "err");
        }
        if (op == "status") { if (!r.ok()) log(r.message(), "err"); showStatus(j); }
        else if (r.ok()) log(j.value("message").toString(op + " done."), "ok");
        else log(op + " failed" + (r.message().isEmpty() ? QString() : ": " + r.message()), "err");
        if (then) then(j);
    }, PKEXEC_TIMEOUT_MS);
}

void IntelTab::readStatus() {
    if (busy_) return;
    runOp({{"op", "status"}});
}

void IntelTab::showStatus(const QJsonObject &s) {
    QStringList bits;
    const QJsonObject cpu = s.value("cpu").toObject();
    if (!cpu.isEmpty())
        bits << QStringLiteral("<b>%1</b> · %2 (model %3, stepping %4)").arg(cpu.value("name").toString().toHtmlEscaped(),
                cpu.value("codename").toString()).arg(cpu.value("model").toInt()).arg(cpu.value("stepping").toInt());
    const QString lock = s.value("lockdown").toString();
    if (!lock.isEmpty() && lock != "none")
        bits << QStringLiteral("<span style='color:%1'>kernel lockdown: %2 — MSR writes are blocked</span>").arg(theme::DANGER, lock);
    if (s.contains("allow_writes") && s.value("allow_writes").toString() != "on")
        bits << QStringLiteral("<span style='color:%1'>msr.allow_writes=%2</span>").arg(theme::WARN, s.value("allow_writes").toString());

    const QJsonObject v = s.value("voltage").toObject();
    for (Row &r : volt_) {
        const QJsonObject o = v.value(r.key).toObject();
        r.cur->setText(o.contains("mv") ? QStringLiteral("%1 mV").arg(o.value("mv").toDouble(), 0, 'f', 1) : QStringLiteral("error"));
        r.cur->setToolTip(o.contains("raw") ? "raw " + o.value("raw").toString() : o.value("error").toString());
    }
    const QJsonObject ic = s.value("iccmax").toObject();
    for (Row &r : icc_) {
        const QJsonObject o = ic.value(r.key).toObject();
        r.cur->setText(o.contains("amps") ? QStringLiteral("%1 A%2").arg(o.value("amps").toDouble(), 0, 'f', 2)
                                                .arg(o.value("unlimited").toBool() ? " (unlimited)" : "")
                                          : QStringLiteral("n/a"));
        r.cur->setToolTip(o.contains("raw") ? "raw " + o.value("raw").toString() + (o.contains("error") ? "\n" + o.value("error").toString() : QString())
                                            : o.value("error").toString());
        r.cur->setProperty("amps", o.contains("amps") ? QVariant(o.value("amps").toDouble()) : QVariant());
        // Pre-fill the editor with the stock limit so ticking the box never lowers it by accident.
        if (o.contains("amps") && !r.on->isChecked()) r.val->setValue(o.value("amps").toDouble());
    }
    const QJsonObject t = s.value("temp").toObject();
    if (t.contains("tjmax")) {
        tjCur_->setText(QStringLiteral("%1 − %2 = %3 °C").arg(t.value("tjmax").toInt()).arg(t.value("offset").toInt()).arg(t.value("target").toInt()));
        tjOn_->setEnabled(t.value("programmable").toBool(true));
        if (!tjOn_->isEnabled()) tjOn_->setChecked(false);
    } else tjCur_->setText("n/a");
    const QJsonObject pw = s.value("power").toObject();
    auto term = [](const QJsonObject &o) {
        return QStringLiteral("%1 W · %2 s%3").arg(o.value("watts").toDouble(), 0, 'f', 1).arg(o.value("seconds").toDouble(), 0, 'g', 4)
            .arg(o.value("enabled").toBool() ? "" : " (off)");
    };
    pl1_.cur->setText(pw.contains("pl1") ? term(pw.value("pl1").toObject()) : "n/a");
    pl2_.cur->setText(pw.contains("pl2") ? term(pw.value("pl2").toObject()) : "n/a");
    const bool plLocked = pw.value("locked").toBool();
    for (PlRow *p : {&pl1_, &pl2_}) { p->on->setEnabled(!plLocked); if (plLocked) p->on->setChecked(false); }
    const QJsonObject mc = pw.value("mchbar").toObject();
    QString mtext = mc.contains("error") ? "MCHBAR: " + mc.value("error").toString()
        : QStringLiteral("MCHBAR %1: PL1 %2 W / PL2 %3 W%4").arg(mc.value("base").toString())
              .arg(mc.value("pl1").toObject().value("watts").toDouble(), 0, 'f', 1)
              .arg(mc.value("pl2").toObject().value("watts").toDouble(), 0, 'f', 1)
              .arg(mc.value("matches_msr").toBool() ? " (= MSR)" : " (≠ MSR, the lower one wins)");
    plLock_->setText((plLocked ? QStringLiteral("MSR power limit is LOCKED by firmware. ") : QString()) + mtext +
                     ". The EC may re-program PL1/PL2 on profile changes.");

    const QJsonObject bd = s.value("bdprochot").toObject();
    if (bd.contains("enabled")) bdprochot_->setToolTip(bdprochot_->toolTip().section("\n\nNow:", 0, 0) +
                                                     QStringLiteral("\n\nNow: %1").arg(bd.value("enabled").toBool() ? "enabled" : "disabled"));
    const QJsonObject ct = s.value("ctdp").toObject();
    const bool ctOk = ct.value("programmable").toBool() && !ct.value("locked").toBool();
    ctdp_->setEnabled(ctOk);
    if (!ctOk) ctdp_->setCurrentIndex(0);
    else for (int lvl = 0; lvl < 3; ++lvl)
        if (auto *m = qobject_cast<QStandardItemModel *>(ctdp_->model())) m->item(lvl + 1)->setEnabled(lvl <= ct.value("levels").toInt());
    if (ct.value("current").isDouble()) ctdp_->setItemText(0, QStringLiteral("untouched (now %1)").arg(ct.value("current").toInt()));

    // Plundervolt detection is only possible by writing; say so instead of guessing.
    bits << QStringLiteral("Voltage lock can only be detected by writing: if Apply reports a readback mismatch, "
                           "undervolting is locked by the BIOS (CVE-2019-11157) or this CPU has no OC mailbox.");
    info_->setText(bits.join("<br>"));
    log("Values read from the CPU.", "ok");
}

// ── public (tray / game mode) ───────────────────────────────────────────────

void IntelTab::applyReset() { runOp({{"op", "reset"}}, [this](const QJsonObject &) { readStatus(); }); }

bool IntelTab::applyNamedProfile(const QString &name) {
    if (busy_ || !loadProfile(name)) return false;
    runOp({{"op", "apply"}, {"profile", currentProfile()}}, [this](const QJsonObject &) { readStatus(); });
    return true;
}

// ── profiles ────────────────────────────────────────────────────────────────

QStringList IntelTab::savedProfileNames() const {
    QStringList n = QDir(inteluv::profilesDir()).entryList({"*.json"}, QDir::Files, QDir::Name);
    for (QString &s : n) s.chop(5);
    return n;
}

void IntelTab::reloadProfiles() {
    const QString prev = profileCombo_->currentText();
    profileCombo_->clear();
    profileCombo_->addItems(savedProfileNames());
    if (int i = profileCombo_->findText(prev); i >= 0) profileCombo_->setCurrentIndex(i);
}

void IntelTab::saveProfile() {
    bool ok = false;
    const QString name = QInputDialog::getText(this, "Save Profile", "Profile name:", QLineEdit::Normal,
                                               profileCombo_->currentText(), &ok).trimmed();
    if (!ok) return;
    static const QRegularExpression re(QStringLiteral("^[A-Za-z0-9][A-Za-z0-9 _-]{0,63}$"));
    if (!re.match(name).hasMatch()) {
        QMessageBox::warning(this, "Invalid Name", "Use 1-64 characters: letters, digits, space, underscore or hyphen.");
        return;
    }
    const QString path = inteluv::profilesDir() + '/' + name + ".json";
    if (QFile::exists(path) && QMessageBox::question(this, "Overwrite Profile", "Profile '" + name + "' already exists. Overwrite?",
                                                     QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return;
    QSaveFile f(path);
    if (!f.open(QIODevice::WriteOnly) || f.write(QJsonDocument(currentProfile()).toJson()) < 0 || !f.commit()) {
        QMessageBox::critical(this, "Save Error", f.errorString());
        return;
    }
    reloadProfiles();
    profileCombo_->setCurrentText(name);
    log("Profile saved: " + path);
}

bool IntelTab::loadProfile(const QString &name) {
    if (name.isEmpty()) return false;
    QFile f(inteluv::profilesDir() + '/' + name + ".json");
    if (!f.open(QIODevice::ReadOnly) || f.size() > 64 * 1024) { log("Cannot read profile '" + name + "': " + f.errorString(), "err"); return false; }
    const QJsonDocument doc = QJsonDocument::fromJson(f.readAll());
    if (!doc.isObject()) { log("Profile '" + name + "' is not a JSON object.", "err"); return false; }
    setProfile(doc.object());
    profileCombo_->setCurrentText(name);
    log("Profile loaded: " + name);
    return true;
}

void IntelTab::deleteProfile() {
    const QString name = profileCombo_->currentText();
    if (name.isEmpty() || QMessageBox::question(this, "Delete Profile", "Delete profile '" + name + "'?",
                                                QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return;
    if (!QFile::remove(inteluv::profilesDir() + '/' + name + ".json")) log("Could not delete '" + name + "'.", "err");
    reloadProfiles();
}
