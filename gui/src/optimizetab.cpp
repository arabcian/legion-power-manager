#include "optimizetab.h"
#include "privileged.h"
#include "inteltab.h"
#include "ryzentab.h"
#include "scenes.h"
#include "bootadvisor.h"
#include "lazywidget.h"
#include "sysinfo.h"
#include "theme.h"

#include <QApplication>
#include <QCheckBox>
#include <QClipboard>
#include <QAbstractItemView>
#include <QComboBox>
#include <QDialog>
#include <QDialogButtonBox>
#include <QHeaderView>
#include <QTableWidget>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QFrame>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPointer>
#include <QProcess>
#include <QPushButton>
#include <QRegularExpression>
#include <QSaveFile>
#include <QScrollArea>
#include <QSet>
#include <QSpacerItem>
#include <QSpinBox>
#include <QDoubleSpinBox>
#include <QFormLayout>
#include <QSettings>
#include "int64spinbox.h"
#include <QStandardPaths>
#include <QTabWidget>
#include <QTimer>
#include <QVBoxLayout>
#include <algorithm>
#include <climits>

static constexpr int POLL_MS = 4000, DESCRIBE_TIMEOUT_MS = 8000, PKEXEC_TIMEOUT_MS = 120000, AUTOTUNE_TIMEOUT_MS = 20000;
static constexpr qint64 MAX_PRESET_BYTES = 256 * 1024;
static const char *GROUPS[] = {"CPU", "Memory", "Scheduler", "Storage", "Network", "Devices", "Power", "Stability"};
static const QString GAMEMODE = QStringLiteral("/usr/bin/lpm-gamemode");
// lpm-gamemode PRE/WRAP apply these curve profiles (exact name) when enabled.
static const QString UNDERVOLT_PROFILE = QStringLiteral("GAMING");
static const QString NVCURVE_PROFILES = QStringLiteral("/etc/nvcurve/profiles");

static QString helperPath() { return privileged::helperPath(QStringLiteral("tune-helper")); }

// Ops that can only put saved originals back or apply an *approved* preset by name go to tune-profile-helper;
// raw values, the boot preset, the preset store and driver options go to tune-helper (install.sh security level 3
// asks for the password there only).
static QString helperForOp(const QString &op) {
    static const QStringList profileOps{"snapshot", "apply_preset", "release", "prune", "restore", "restore_keys", "boost", "isolate_join", "tool"};
    return privileged::helperPath(profileOps.contains(op) ? QStringLiteral("tune-profile-helper") : QStringLiteral("tune-helper"));
}

// Root-owned copy of a preset that tune-profile-helper / lpm-gamemode apply by name.
static bool storeHas(const QString &name) { return QFile::exists(QStringLiteral("/etc/legion-power-manager/presets/") + name + QStringLiteral(".json")); }

// ── built-in presets ────────────────────────────────────────────────────────
// Templates only: values a machine does not offer are skipped on load. Keep
// the names valid for lpm-gamemode (letters, digits, space, _ - .).

// `vendor`: "amd" / "intel" = only listed on that CPU vendor, nullptr = everywhere.
struct Builtin { const char *vendor, *name, *summary, *json; };
static const Builtin BUILTINS[] = {
    {"amd", "Gaming X3D",
     "Full lutris-game-tune set plus X3D placement: game on the V-Cache CCD, IRQs and kernel work on the other one.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"performance","cpu.epp_boost":"1",
        "cpu.governor_ccd0":"powersave","cpu.governor_ccd1":"powersave","cpu.epp_ccd0":"performance","cpu.epp_ccd1":"balance_power",
        "cpu.boost":"1","cpu.min_freq":"lowest_nonlinear","cpu.x3d_mode":"cache",
        "thp.enabled":"madvise","thp.shmem_enabled":"advise","thp.defrag":"defer+madvise","thp.khugepaged_defrag":0,
        "mm.lru_gen":7,"mm.lru_gen_min_ttl":1000,"mm.ksm_run":0,"vm.max_map_count":2147483642,
        "vm.swappiness":10,"vm.compaction_proactiveness":5,"vm.watermark_boost_factor":15000,
        "vm.watermark_scale_factor":50,"vm.min_free_kbytes":262144,"vm.zone_reclaim_mode":0,
        "vm.page_lock_unfairness":1,"vm.stat_interval":10,"vm.page_cluster":0,
        "kernel.split_lock_mitigate":0,"kernel.watchdog":0,"kernel.numa_balancing":0,
        "kernel.sched_autogroup":1,"kernel.cfs_bandwidth_slice_us":3000,
        "sched.preempt":"full","sched.base_slice_ns":1000000,"sched.min_base_slice_ns":1000000,"sched.migration_cost_ns":500000,"sched.nr_migrate":32,
        "wq.cpumask":"frequency","irq.affinity":"frequency",
        "blk.scheduler":"none","pci.aspm":"performance","pci.latency_timer":"tuned",
        "snd.hda_power_save":0,"snd.hda_power_save_controller":"0","usb.autosuspend":-1,"gpu.amdgpu_dpm":"low"},
      "run":{"nice":-5,"autogroup":true,"affinity":"cache"}})"},
    {"amd", "Competitive",
     "Gaming X3D taken to the limit: frequency CCD parked (in game mode emptied, not taken offline), deep C-states off. Maximum determinism, most heat.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"performance","cpu.boost":"1",
        "cpu.governor_ccd0":"powersave","cpu.governor_ccd1":"powersave","cpu.epp_ccd0":"performance","cpu.epp_ccd1":"balance_power",
        "cpu.min_freq":"lowest_nonlinear","cpu.x3d_mode":"cache","cpu.cstate_max":"1",
        "thp.enabled":"madvise","thp.defrag":"defer+madvise","thp.khugepaged_defrag":0,"mm.lru_gen_min_ttl":1000,
        "mm.ksm_run":0,"vm.max_map_count":2147483642,"vm.swappiness":10,"vm.stat_interval":10,"vm.page_cluster":0,
        "kernel.split_lock_mitigate":0,"kernel.watchdog":0,"kernel.numa_balancing":0,"kernel.timer_migration":0,
        "sched.preempt":"full","sched.base_slice_ns":1000000,"sched.min_base_slice_ns":1000000,"blk.scheduler":"none","pci.aspm":"performance","snd.hda_power_save":0,"usb.autosuspend":-1,
        "gpu.amdgpu_dpm":"low","cpu.ccd_park":"frequency"},
      "run":{"nice":-10,"autogroup":true,"affinity":"cache"}})"},
    {"amd", "Low latency desktop",
     "Everyday responsiveness without the power cost: efficient floor clock, full preemption, no compaction stalls.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"balance_performance",
        "cpu.min_freq":"lowest_nonlinear","thp.enabled":"madvise","thp.defrag":"defer+madvise",
        "mm.lru_gen":7,"mm.lru_gen_min_ttl":1000,"vm.max_map_count":2147483642,"vm.page_cluster":0,
        "kernel.split_lock_mitigate":0,"sched.preempt":"full","snd.hda_power_save":0},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    {"amd", "Compile throughput",
     "Long parallel builds (emerge, kernel): frequency CCD preferred, throughput preemption, bigger slices.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"balance_performance","cpu.boost":"1",
        "cpu.x3d_mode":"frequency","cpu.smt":"on","cpu.ccd_park":"none","cpu.cstate_max":"all",
        "thp.enabled":"always","thp.defrag":"madvise","vm.swappiness":60,
        "sched.preempt":"voluntary","sched.base_slice_ns":3000000,"sched.min_base_slice_ns":3000000,"sched.migration_cost_ns":500000,
        "wq.cpumask":"all","irq.affinity":"all","blk.scheduler":"mq-deadline"},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    {"amd", "CO validation",
     "For proving Curve Optimizer offsets: boost on, every idle state on (idle-to-boost transitions are where CO fails), MCE polled every 10 s.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"performance","cpu.boost":"1",
        "cpu.min_freq":"cpuinfo_min","cpu.cstate_max":"all","cpu.smt":"on","cpu.ccd_park":"none",
        "kernel.watchdog":1,"mce.check_interval":10},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    {"amd", "Quiet battery",
     "Unplugged: power EPP, no turbo, lowest floor clock, aggressive device power saving.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"power","cpu.boost":"0",
        "cpu.min_freq":"cpuinfo_min","cpu.cstate_max":"all","pci.aspm":"powersupersave",
        "snd.hda_power_save":1,"snd.hda_power_save_controller":"1","usb.autosuspend":2,
        "kernel.watchdog":1,"gpu.amdgpu_dpm":"auto","vm.stat_interval":10,
        "net.wol":"0","gpu.amdgpu_abm":3,
        "pm.ahci_runtime_timeout":15000,"pm.ahci_disk_runtime":"auto","pm.ahci_port_runtime":"auto","disk.apm_0":128,"disk.apm_1":128},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    // ── Intel (hybrid P/E-core) ──────────────────────────────────────────────
    {"intel", "Intel gaming hybrid",
     "lutris-game-tune set for a hybrid Intel CPU: P-cores on performance EPP, E-cores on balance_power, kernel work and IRQs steered to the E-cores.",
     R"({"values":{
        "cpu.intel_pstate_status":"active","cpu.governor":"powersave","cpu.epp":"balance_performance",
        "cpu.epp_pcore":"performance","cpu.epp_ecore":"balance_power","cpu.boost":"1","cpu.hwp_dynamic_boost":"1",
        "thp.enabled":"madvise","thp.shmem_enabled":"advise","thp.defrag":"defer+madvise","thp.khugepaged_defrag":0,
        "mm.lru_gen":7,"mm.lru_gen_min_ttl":1000,"mm.ksm_run":0,"vm.max_map_count":2147483642,
        "vm.swappiness":10,"vm.compaction_proactiveness":5,"vm.watermark_boost_factor":15000,
        "vm.watermark_scale_factor":50,"vm.min_free_kbytes":262144,"vm.zone_reclaim_mode":0,
        "vm.page_lock_unfairness":1,"vm.stat_interval":10,"vm.page_cluster":0,
        "kernel.split_lock_mitigate":0,"kernel.watchdog":0,"kernel.numa_balancing":0,
        "kernel.sched_autogroup":1,"kernel.cfs_bandwidth_slice_us":3000,
        "sched.preempt":"full","sched.base_slice_ns":1000000,"sched.min_base_slice_ns":1000000,"sched.migration_cost_ns":500000,"sched.nr_migrate":32,
        "wq.cpumask":"ecore","irq.affinity":"ecore",
        "blk.scheduler":"none","pci.aspm":"performance","pci.latency_timer":"tuned",
        "snd.hda_power_save":0,"snd.hda_power_save_controller":"0","usb.autosuspend":-1,
        "gpu.intel_slpc_profile":"power_saving"},
      "run":{"nice":-5,"autogroup":true,"affinity":"none"}})"},
    {"intel", "Intel competitive",
     "Intel gaming hybrid taken further: game pinned to the P-cores, every core on performance EPP, deep C-states off. Most deterministic, most heat.",
     R"({"values":{
        "cpu.intel_pstate_status":"active","cpu.governor":"powersave","cpu.epp":"performance",
        "cpu.epp_pcore":"performance","cpu.epp_ecore":"balance_performance","cpu.boost":"1","cpu.hwp_dynamic_boost":"1",
        "cpu.cstate_max":"1","thp.enabled":"madvise","thp.defrag":"defer+madvise","thp.khugepaged_defrag":0,
        "mm.lru_gen_min_ttl":1000,"mm.ksm_run":0,"vm.max_map_count":2147483642,"vm.swappiness":10,"vm.stat_interval":10,
        "vm.page_cluster":0,"kernel.split_lock_mitigate":0,"kernel.watchdog":0,"kernel.numa_balancing":0,
        "kernel.timer_migration":0,"sched.preempt":"full","sched.base_slice_ns":1000000,"sched.min_base_slice_ns":1000000,"wq.cpumask":"ecore","irq.affinity":"ecore","blk.scheduler":"none","pci.aspm":"performance",
        "snd.hda_power_save":0,"usb.autosuspend":-1,"gpu.intel_slpc_profile":"power_saving"},
      "run":{"nice":-10,"autogroup":true,"affinity":"pcore"}})"},
    {"intel", "Intel low latency desktop",
     "Everyday responsiveness on Intel: balance_performance EPP, HWP dynamic boost, full preemption, no compaction stalls.",
     R"({"values":{
        "cpu.intel_pstate_status":"active","cpu.governor":"powersave","cpu.epp":"balance_performance","cpu.hwp_dynamic_boost":"1",
        "thp.enabled":"madvise","thp.defrag":"defer+madvise","mm.lru_gen":7,"mm.lru_gen_min_ttl":1000,
        "vm.max_map_count":2147483642,"vm.page_cluster":0,"kernel.split_lock_mitigate":0,"sched.preempt":"full",
        "snd.hda_power_save":0},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    {"intel", "Intel compile throughput",
     "Long parallel builds: every P- and E-core busy, balance_performance EPP, throughput preemption, bigger slices.",
     R"({"values":{
        "cpu.intel_pstate_status":"active","cpu.governor":"powersave","cpu.epp":"balance_performance",
        "cpu.epp_pcore":"balance_performance","cpu.epp_ecore":"balance_performance","cpu.boost":"1",
        "cpu.ccd_park":"none","cpu.cstate_max":"all","thp.enabled":"always","thp.defrag":"madvise","vm.swappiness":60,
        "sched.preempt":"voluntary","sched.base_slice_ns":3000000,"sched.min_base_slice_ns":3000000,"sched.migration_cost_ns":500000,
        "wq.cpumask":"all","irq.affinity":"all","blk.scheduler":"mq-deadline"},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    {"intel", "Intel quiet battery",
     "Unplugged: power EPP, no turbo, E-cores capped by EPP, iGPU power saving, aggressive device power saving.",
     R"({"values":{
        "cpu.intel_pstate_status":"active","cpu.governor":"powersave","cpu.epp":"power","cpu.epp_pcore":"balance_power",
        "cpu.epp_ecore":"power","cpu.boost":"0","cpu.hwp_dynamic_boost":"0","cpu.min_freq":"cpuinfo_min",
        "cpu.cstate_max":"all","pci.aspm":"powersupersave","snd.hda_power_save":1,"snd.hda_power_save_controller":"1",
        "usb.autosuspend":2,"kernel.watchdog":1,"gpu.intel_slpc_profile":"power_saving",
        "vm.stat_interval":10,"net.wol":"0",
        "pm.ahci_runtime_timeout":15000,"pm.ahci_disk_runtime":"auto","pm.ahci_port_runtime":"auto","disk.apm_0":128,"disk.apm_1":128},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
};

static bool builtinForThisCpu(const Builtin &b) {
    if (!b.vendor) return true;
    const QLatin1String v(b.vendor);
    return (v == QLatin1String("amd") && sysinfo::isAmd()) || (v == QLatin1String("intel") && sysinfo::isIntel());
}

static const Builtin *builtin(const QString &name) {
    for (const Builtin &b : BUILTINS) if (builtinForThisCpu(b) && name == QLatin1String(b.name)) return &b;
    return nullptr;
}

static QJsonObject builtinObject(const Builtin &b) {
    QJsonObject o = QJsonDocument::fromJson(QByteArray(b.json)).object();
    o["summary"] = QString::fromUtf8(b.summary);
    return o;
}

/// Same rule as lpm-gamemode's valid_name().
static bool validPresetName(const QString &n) {
    static const QRegularExpression re(QStringLiteral(R"(^[\p{L}\p{N}][\p{L}\p{N} _.\-]{0,63}\z)"));
    return re.match(n).hasMatch() && !n.contains(QStringLiteral("..")) && n.toUtf8().size() <= 64;
}

static QJsonObject readJsonFile(const QString &path) {
    QFile f(path);
    if (f.size() > MAX_PRESET_BYTES || !f.open(QIODevice::ReadOnly)) return {};
    return QJsonDocument::fromJson(f.readAll()).object();
}

static bool writeJsonFile(const QString &path, const QJsonObject &o, QString *err) {
    QDir().mkpath(QFileInfo(path).absolutePath());
    QSaveFile f(path);
    if (!f.open(QIODevice::WriteOnly)) { if (err) *err = f.errorString(); return false; }
    f.write(QJsonDocument(o).toJson(QJsonDocument::Indented));
    if (!f.commit()) { if (err) *err = f.errorString(); return false; }
    return true;
}

QString OptimizeTab::presetsDir() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) +
           QStringLiteral("/legion-power-manager/tune-presets");
}
QString OptimizeTab::configFile() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) +
           QStringLiteral("/legion-power-manager/tune.json");
}
QString OptimizeTab::gamePreset() const {
    const QString n = readJsonFile(configFile()).value("game_preset").toString();
    return validPresetName(n) ? n : QString();
}

// ── UI ──────────────────────────────────────────────────────────────────────

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

OptimizeTab::OptimizeTab(QWidget *parent) : QWidget(parent) {
    buildUi();
    reloadPresets();
    poll_ = new QTimer(this);
    poll_->setInterval(POLL_MS);
    connect(poll_, &QTimer::timeout, this, &OptimizeTab::refresh);
    refresh();
}

void OptimizeTab::buildUi() {
    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(12, 10, 12, 10);
    root->setSpacing(6);

    // State banner: is anything changed right now, by whom, and the way back.
    auto *bannerFrame = new QFrame;
    bannerFrame->setObjectName("tuneBanner");
    auto *bl = new QHBoxLayout(bannerFrame);
    bl->setContentsMargins(10, 6, 8, 6);
    auto *btext = new QVBoxLayout;
    btext->setSpacing(0);
    banner_ = new QLabel;
    banner_->setStyleSheet("font-weight: 600; background: transparent;");
    bannerDetail_ = muted({});
    btext->addWidget(banner_);
    btext->addWidget(bannerDetail_);
    bl->addLayout(btext, 1);
    restoreBtn_ = new QPushButton("Restore originals");
    restoreBtn_->setObjectName("btnDanger");
    restoreBtn_->setToolTip("Write every saved original value back (hot-plugged CPUs first).");
    connect(restoreBtn_, &QPushButton::clicked, this, [this] { restoreAll(true); });
    bl->addWidget(restoreBtn_);
    root->addWidget(bannerFrame);

    // Presets
    auto *pbox = box("Preset", "box_purple");
    auto *pl = new QGridLayout(pbox);
    pl->setHorizontalSpacing(6);
    presetCombo_ = new QComboBox;
    presetCombo_->setMinimumWidth(220);
    auto *bLoad = new QPushButton("Load"), *bSave = new QPushButton("Save as…"), *bDel = new QPushButton("Delete");
    bDel->setObjectName("btnDanger");
    gameBtn_ = new QPushButton("★ Use for games");
    gameBtn_->setToolTip("Make the selected preset the default of lpm-gamemode (Lutris / Steam hooks).\n"
                         "A built-in preset is copied to your preset folder first.");
    bootBtn_ = new QPushButton("⏻ Apply at boot");
    bootBtn_->setToolTip("Store the checked rows as the boot preset (root-owned\n"
                         "/etc/legion-power-manager/tune-boot.json, applied by the lpm-tune OpenRC service).");
    bootClear_ = new QPushButton("Clear boot");
    connect(bLoad, &QPushButton::clicked, this, &OptimizeTab::loadSelected);
    connect(bSave, &QPushButton::clicked, this, &OptimizeTab::saveAs);
    connect(bDel, &QPushButton::clicked, this, &OptimizeTab::deleteSelected);
    connect(gameBtn_, &QPushButton::clicked, this, &OptimizeTab::useForGames);
    connect(bootBtn_, &QPushButton::clicked, this, &OptimizeTab::setBoot);
    connect(bootClear_, &QPushButton::clicked, this, &OptimizeTab::clearBoot);
    auto *summary = muted({});
    connect(presetCombo_, &QComboBox::currentIndexChanged, this, [this, summary, bDel] {
        const QString n = presetCombo_->currentData().toString();
        const bool user = QFile::exists(presetsDir() + '/' + n + ".json");
        bDel->setEnabled(user);
        const QString s = presetObject(n).value("summary").toString();
        summary->setText(s.isEmpty() ? (user ? QStringLiteral("Your preset.") : QString()) : s);
    });
    pl->addWidget(presetCombo_, 0, 0);
    pl->addWidget(bLoad, 0, 1);
    pl->addWidget(bSave, 0, 2);
    pl->addWidget(bDel, 0, 3);
    pl->addItem(new QSpacerItem(0, 0, QSizePolicy::Expanding), 0, 4);
    pl->setColumnStretch(4, 1);
    pl->addWidget(gameBtn_, 0, 5);
    pl->addWidget(bootBtn_, 0, 6);
    pl->addWidget(bootClear_, 0, 7);
    pl->addWidget(summary, 1, 0, 1, 5);
    bootLabel_ = muted({});
    bootLabel_->setAlignment(Qt::AlignRight | Qt::AlignVCenter);
    pl->addWidget(bootLabel_, 1, 5, 1, 3);

    // Autotune: pick a base, the helper profiles the hardware and fills the rows.
    auto *autoRow = new QHBoxLayout;
    autoRow->setSpacing(6);
    auto *autoLbl = new QLabel(QStringLiteral("Autotune for"));
    autoGoal_ = new QComboBox;
    autoGoal_->addItem(QStringLiteral("Power saving"), QStringLiteral("powersave"));
    autoGoal_->addItem(QStringLiteral("Gaming (latency + throughput)"), QStringLiteral("gaming"));
    autoGoal_->addItem(QStringLiteral("Bare throughput"), QStringLiteral("throughput"));
    autoGoal_->addItem(QStringLiteral("Optimal desktop"), QStringLiteral("desktop"));
    autoGoal_->setCurrentIndex(3);
    autoGoal_->setToolTip(QStringLiteral("Power saving: battery life and low heat first.\n"
                                         "Gaming: frame-time consistency and input latency, then throughput.\n"
                                         "Bare throughput: most work per second for builds, encodes, compute.\n"
                                         "Optimal desktop: responsive everyday use at sensible power."));
    autoBtn_ = new QPushButton(QStringLiteral("⚙ Autotune"));
    autoBtn_->setToolTip(QStringLiteral("Profile this machine (CPU topology, V-Cache/hybrid, cpufreq driver, C-state latencies, RAM, swap,\n"
                                        "storage, battery, GPUs, kernel) and check every row with the value the chosen base calls for.\n"
                                        "Nothing is written until you press Apply checked; Save turns it into a preset."));
    connect(autoBtn_, &QPushButton::clicked, this, &OptimizeTab::runAutotune);
    auto *weightsBtn = new QPushButton(QStringLiteral("Weights…"));
    weightsBtn->setToolTip(QStringLiteral("How much this base values latency, throughput, power, memory footprint, stability\n"
                                          "and how much the calibrated storage benchmarks count.\n"
                                          "Autotune writes a setting only when its expected gain under these weights beats leaving it alone;\n"
                                          "hard safety limits cannot be bought with weights."));
    connect(weightsBtn, &QPushButton::clicked, this, &OptimizeTab::editAutotuneWeights);
    autoRow->addWidget(autoLbl);
    autoRow->addWidget(autoGoal_);
    autoRow->addWidget(autoBtn_);
    autoRow->addWidget(weightsBtn);
    autoRow->addWidget(muted(QStringLiteral("hardware-aware preset, reviewed before anything is applied")), 1);
    pl->addLayout(autoRow, 2, 0, 1, 8);
    root->addWidget(pbox);

    groups_ = new QTabWidget;
    groups_->setDocumentMode(true);
    root->addWidget(groups_, 1);

    // Action bar
    auto *bar = new QHBoxLayout;
    auto *sel = new QLabel("Select:");
    sel->setProperty("role", "muted");
    bar->addWidget(sel);
    auto selector = [this](const QString &text, const QString &tip, std::function<bool(const Row &)> pred) {
        auto *b = new QPushButton(text);
        b->setToolTip(tip);
        connect(b, &QPushButton::clicked, this, [this, pred] {
            for (Row &r : rows_) if (r.include && r.include->isEnabled()) r.include->setChecked(pred(r));
        });
        return b;
    };
    bar->addWidget(selector("Changed", "Check exactly the rows whose editor differs from the live value",
                            [this](const Row &r) { return differs(r); }));
    bar->addWidget(selector("None", "Uncheck every row", [](const Row &) { return false; }));
    bar->addSpacing(12);
    status_ = new QLabel;
    status_->setWordWrap(true);
    bar->addWidget(status_, 1);
    auto *refreshBtn = new QPushButton("Refresh");
    connect(refreshBtn, &QPushButton::clicked, this, &OptimizeTab::refresh);
    bar->addWidget(refreshBtn);
    applyBtn_ = new QPushButton("Apply checked");
    applyBtn_->setObjectName("btnAccent");
    applyBtn_->setMinimumWidth(132);
    connect(applyBtn_, &QPushButton::clicked, this, &OptimizeTab::applySelected);
    bar->addWidget(applyBtn_);
    root->addLayout(bar);

    statusTimer_ = new QTimer(this);
    statusTimer_->setSingleShot(true);
    connect(statusTimer_, &QTimer::timeout, status_, &QLabel::clear);
    updateStateBanner();
}

QWidget *OptimizeTab::buildLaunchPage() {
    auto *scroll = new QScrollArea;
    scroll->setWidgetResizable(true);
    auto *page = new QWidget;
    auto *v = new QVBoxLayout(page);
    v->setContentsMargins(2, 6, 2, 2);
    v->setSpacing(8);

    auto *runBox = box("Launch boost  (saved in the preset's \"run\" block)", "box_green");
    auto *g = new QGridLayout(runBox);
    g->setHorizontalSpacing(10);
    g->addWidget(new QLabel("Nice"), 0, 0);
    nice_ = new QSpinBox;
    nice_->setRange(-20, 0);
    nice_->setSpecialValueText("off");
    nice_->setFixedWidth(90);
    g->addWidget(nice_, 0, 1);
    g->addWidget(muted("Priority for the game process tree. -5 is a sane start; -15…-20 can starve compositor and audio."), 0, 2);
    autogroup_ = new QCheckBox("Also renice the autogroup");
    autogroup_->setChecked(true);
    g->addWidget(autogroup_, 1, 1, 1, 2);
    g->addWidget(new QLabel("CPU affinity"), 2, 0);
    affinity_ = new QComboBox;
    affinity_->setMinimumWidth(260);
    g->addWidget(affinity_, 2, 1);
    g->addWidget(muted(sysinfo::isIntel()
        ? QStringLiteral("Pins the game to the P-cores (or E-cores). Pair it with \"Unbound workqueue CPUs\" / \"IRQ affinity\" on the E-cores.")
        : QStringLiteral("Pins the game to one CCD. Pair it with \"Unbound workqueue CPUs\" / \"IRQ affinity\" on the other CCD.")), 2, 2);
    g->setColumnStretch(2, 1);
    v->addWidget(runBox);

    // Global (tune.json), not per preset: takes effect on the next game start.
    auto *uvBox = box(QStringLiteral("System at game start"), "box_purple");
    auto *ug = new QGridLayout(uvBox);
    ug->setHorizontalSpacing(10);
    ug->setVerticalSpacing(6);
    const QJsonObject cfg = readJsonFile(configFile());
    ug->addWidget(new QLabel("Scene"), 0, 0);
    gameScene_ = new QComboBox;
    gameScene_->setMinimumWidth(220);
    gameScene_->setToolTip("The first game to start switches the machine to this scene; when the last game exits it goes "
                           "back to the scene active before (or to the AC / battery scene if automatic switching is on).\n"
                           "The scene's Optimizations preset is not used while playing — the ★ game preset handles tuning.");
    ug->addWidget(gameScene_, 0, 1, 1, 2);
    uvCpu_ = new QCheckBox("Undervolt CPU");
    uvCpu_->setChecked(cfg.value("undervolt_cpu").toBool());
    uvGpu_ = new QCheckBox("Undervolt GPU");
    uvGpu_->setChecked(cfg.value("undervolt_gpu").toBool());
    if (!sysinfo::isAmd() && !sysinfo::isIntel()) {
        uvCpu_->setChecked(false);
        uvCpu_->setEnabled(false);
        uvCpu_->setToolTip(QStringLiteral("No CPU undervolt backend for this CPU."));
    }
    ug->addWidget(uvCpu_, 1, 0);
    ug->addWidget(uvGpu_, 1, 1);
    uvInfo_ = new QLabel;
    uvInfo_->setTextFormat(Qt::RichText);
    uvInfo_->setWordWrap(true);
    ug->addWidget(uvInfo_, 1, 2);
    ug->addWidget(muted(QStringLiteral("With a scene: the undervolt boxes decide whether its CPU / GPU curve is applied — unticked, "
                                       "the scene loads without touching the curves. Without a scene: the \"%1\" curve profiles "
                                       "are applied (CPU first, GPU 2 s later). Run by lpm-gamemode PRE / WRAP.").arg(UNDERVOLT_PROFILE)),
                   2, 0, 1, 3);
    ug->setColumnStretch(2, 1);
    v->addWidget(uvBox);
    fillGameScenes();

    auto *topo = box("CPU topology", "box_blue");
    auto *tl = new QVBoxLayout(topo);
    topoLabel_ = new QLabel;
    topoLabel_->setTextFormat(Qt::RichText);
    topoLabel_->setWordWrap(true);
    tl->addWidget(topoLabel_);
    v->addWidget(topo);

    auto *hooks = box("Lutris / Steam  (replaces lutris-game-tune-wrapper — no setuid)", "box_yellow");
    auto *hg = new QGridLayout(hooks);
    hg->setHorizontalSpacing(8);
    auto line = [&](int r, const QString &label, QLineEdit *&edit) {
        hg->addWidget(new QLabel(label), r, 0);
        edit = new QLineEdit;
        edit->setReadOnly(true);
        edit->setStyleSheet("font-family: monospace;");
        hg->addWidget(edit, r, 1);
        auto *copy = new QPushButton("Copy");
        connect(copy, &QPushButton::clicked, this, [this, edit] {
            QApplication::clipboard()->setText(edit->text());
            showStatus("Copied: " + edit->text(), theme::OK, 3000);
        });
        hg->addWidget(copy, r, 2);
    };
    line(0, "Lutris pre-game script", lutrisPre_);
    line(1, "Lutris post-game script", lutrisPost_);
    line(2, "Lutris command prefix", lutrisPrefix_);
    line(3, "Steam launch options", steam_);
    hg->addWidget(muted("Lutris: Configure → System options (per game or in Preferences for all games). Without a preset "
                        "name the ★ game preset is used; append a name to pin one per game, e.g. PRE \"Competitive\". "
                        "Game mode is reference counted: the first PRE applies, the last POST restores. Launch boost "
                        "(nice, affinity) comes from the RUN prefix / WRAP."), 4, 0, 1, 3);
    hg->setColumnStretch(1, 1);
    v->addWidget(hooks);
    v->addStretch();
    scroll->setWidget(page);

    auto onRun = [this] { updateLaunchPreview(); };
    connect(nice_, &QSpinBox::valueChanged, this, onRun);
    connect(affinity_, &QComboBox::currentIndexChanged, this, onRun);
    connect(uvCpu_, &QCheckBox::toggled, this, &OptimizeTab::saveUndervolt);
    connect(uvGpu_, &QCheckBox::toggled, this, &OptimizeTab::saveUndervolt);
    connect(gameScene_, &QComboBox::activated, this, &OptimizeTab::saveUndervolt);
    return scroll;
}

void OptimizeTab::fillGameScenes() {
    if (!gameScene_) return;
    const QString sel = readJsonFile(configFile()).value("game_scene").toString();
    const QSignalBlocker b(gameScene_);
    gameScene_->clear();
    gameScene_->addItem(QStringLiteral("— none (keep the current scene)"), QString());
    const QStringList names = scenes::names();
    for (const QString &n : names) gameScene_->addItem(n, n);
    if (!sel.isEmpty() && !names.contains(sel)) gameScene_->addItem(sel + QStringLiteral("  (missing)"), sel);
    gameScene_->setCurrentIndex(std::max(0, gameScene_->findData(sel)));
}

void OptimizeTab::saveUndervolt() {
    if (!uvCpu_) return;
    QJsonObject cfg = readJsonFile(configFile());
    cfg["undervolt_cpu"] = uvCpu_->isChecked() && (sysinfo::isAmd() || sysinfo::isIntel());
    cfg["undervolt_gpu"] = uvGpu_->isChecked();
    const QString scene = gameScene_ ? gameScene_->currentData().toString() : QString();
    if (scene.isEmpty()) cfg.remove("game_scene"); else cfg["game_scene"] = scene;
    QString err;
    if (!writeJsonFile(configFile(), cfg, &err)) { showStatus("Could not save the game start setting: " + err, theme::DANGER); return; }
    updateLaunchPreview();
    QStringList on;
    if (uvCpu_->isChecked()) on << "CPU";
    if (uvGpu_->isChecked()) on << "GPU";
    const QString uv = on.isEmpty() ? QStringLiteral("no undervolt") : QStringLiteral("undervolt %1").arg(on.join(" + "));
    showStatus(scene.isEmpty() ? QStringLiteral("Game start: %1 (\"%2\" profiles)").arg(uv, UNDERVOLT_PROFILE)
                               : QStringLiteral("Game start: scene \"%1\", %2").arg(scene, uv), theme::OK, 5000);
}

void OptimizeTab::updateLaunchPreview() {
    if (!lutrisPre_) return;
    const QString gp = gamePreset();
    lutrisPre_->setText(GAMEMODE + " PRE");
    lutrisPost_->setText(GAMEMODE + " POST");
    lutrisPrefix_->setText(GAMEMODE + " RUN");
    steam_->setText(QStringLiteral("lpm-gamemode WRAP -- %command%"));
    const QString who = gp.isEmpty() ? QStringLiteral("⚠ no ★ game preset chosen yet — the hooks will refuse to run")
                                     : QStringLiteral("uses ★ \"%1\"").arg(gp);
    for (QLineEdit *e : {lutrisPre_, lutrisPrefix_, steam_}) e->setToolTip(who);
    autogroup_->setEnabled(nice_->value() < 0);

    if (uvInfo_ && gameScene_ && !gameScene_->currentData().toString().isEmpty()) {
        // Scene mode: say which curve the scene would load, or that it leaves it alone.
        const auto sc = scenes::load(gameScene_->currentData().toString());
        auto part = [](const char *what, const std::optional<scenes::Scene> &s, bool cpu, bool on) {
            const scenes::Choice c = !s ? scenes::Choice{} : cpu ? s->cpu : s->gpu;
            const QString v = c.kind == scenes::Choice::Profile ? "\"" + c.name + "\""
                            : c.kind == scenes::Choice::Reset ? QStringLiteral("reset") : QStringLiteral("unchanged");
            const char *col = !on || c.kind == scenes::Choice::Unchanged ? theme::MUTED : theme::OK;
            return QStringLiteral("<span style='color:%1'>%2: %3%4</span>").arg(col, what, on ? v : QStringLiteral("skipped"),
                                                                           on || c.kind == scenes::Choice::Unchanged ? QString() : " (" + v + ")");
        };
        uvInfo_->setText(!sc ? QStringLiteral("<span style='color:%1'>scene not found</span>").arg(theme::WARN)
                             : part("CPU", sc, true, uvCpu_->isChecked()) + " &nbsp;·&nbsp; " + part("GPU", sc, false, uvGpu_->isChecked()));
    } else if (uvInfo_) {
        const bool cpu = QFileInfo((sysinfo::isIntel() ? inteluv::profilesDir() : ryzen::profilesDir()) + '/' + UNDERVOLT_PROFILE + ".json").isFile();
        const bool gpu = QFileInfo(NVCURVE_PROFILES + '/' + UNDERVOLT_PROFILE + ".json").isFile();
        auto tag = [](const char *what, bool found, bool on) {
            const char *c = !on ? theme::MUTED : found ? theme::OK : theme::WARN;
            return QStringLiteral("<span style='color:%1'>%2 %3</span>").arg(c, what, found ? "✓ found" : "✗ missing");
        };
        const QString cpuTag = sysinfo::isAmd() ? tag("Ryzen", cpu, uvCpu_->isChecked())
            : sysinfo::isIntel() ? tag("Intel", cpu, uvCpu_->isChecked())
            : QStringLiteral("<span style='color:%1'>CPU: no undervolt backend</span>").arg(theme::MUTED);
        uvInfo_->setText(cpuTag + " &nbsp;·&nbsp; " + tag("NVIDIA", gpu, uvGpu_->isChecked()));
    }
}

// ── describe ────────────────────────────────────────────────────────────────

void OptimizeTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    fillGameScenes();  // scenes may have been added or deleted meanwhile
    updateLaunchPreview();
    refresh();
    poll_->start();
}
void OptimizeTab::hideEvent(QHideEvent *e) {
    QWidget::hideEvent(e);
    poll_->stop();
}

void OptimizeTab::refresh() {
    if (describing_ || busy_) return;
    // LPM_TUNE_FAKE=/path/describe.json renders canned data (dev only).
    if (const QString fake = qEnvironmentVariable("LPM_TUNE_FAKE"); !fake.isEmpty()) {
        onDescribe(readJsonFile(fake));
        return;
    }
    if (!QFileInfo(helperPath()).isExecutable()) {
        helperMissing_ = true;
        updateStateBanner();
        return;
    }
    describing_ = true;
    // describe only reads, so it runs unprivileged (no polkit round-trip).
    auto *p = new QProcess(this);
    QPointer<QProcess> guard(p);
    connect(p, &QProcess::finished, this, [this, p](int, QProcess::ExitStatus) {
        describing_ = false;
        const QByteArray out = p->readAllStandardOutput().trimmed();
        p->deleteLater();
        const QJsonObject d = QJsonDocument::fromJson(out.mid(out.lastIndexOf('\n') + 1)).object();
        if (d.value("ok").toBool()) onDescribe(d);
    });
    connect(p, &QProcess::errorOccurred, this, [this, p](QProcess::ProcessError e) {
        if (e != QProcess::FailedToStart) return;
        describing_ = false;
        helperMissing_ = true;
        updateStateBanner();
        p->deleteLater();
    });
    QTimer::singleShot(DESCRIBE_TIMEOUT_MS, p, [guard] { if (guard && guard->state() != QProcess::NotRunning) guard->kill(); });
    p->start(helperPath(), {});
    p->write(R"({"op":"describe"})");
    p->closeWriteChannel();
}

void OptimizeTab::onDescribe(const QJsonObject &d) {
    helperMissing_ = false;
    // describe runs every 4 s while the tab is shown; the topology only
    // changes on hot-plug (SMT, CCD parking), so what is built from it is
    // rebuilt only then — not a combo clear/refill and a rich-text relayout per poll.
    const QJsonObject topo = d.value("topology").toObject();
    bool topoChanged = topo != topology_;
    topology_ = topo;
    state_ = d.value("state").toObject();
    isolation_ = d.value("isolation").toObject();  // same describe poll: no extra cost
    boot_ = d.value("boot").toObject();
    const QJsonArray rows = d.value("tunables").toArray();

    QStringList keys;
    for (const auto &v : rows) keys << v.toObject().value("key").toString();
    QStringList have;
    for (const Row &r : rows_) have << r.key;
    if (keys != have) {
        buildRows(rows);
        topoChanged = true;  // new affinity combo / topology label
    } else {
        for (int i = 0; i < rows.size(); ++i) updateRow(rows_[i], rows[i].toObject());
    }

    // Affinity choices follow the live topology.
    if (affinity_ && (topoChanged || affinity_->count() == 0) && !affinity_->view()->isVisible()) {
        const QString keep = canonicalCcd(affinity_->currentData().toString());
        const QSignalBlocker b(affinity_);
        affinity_->clear();
        affinity_->addItem("none (scheduler decides)", "none");
        const QJsonArray ccds = topology_.value("ccds").toArray();
        // One entry per CCD, named by its role (no separate cache/frequency aliases).
        const int cacheCcd = topology_.value("cache_ccd").toInt(-1), freqCcd = topology_.value("frequency_ccd").toInt(-1);
        if (ccds.size() > 1) for (const auto &c : ccds) {
            const int i = c.toObject().value("index").toInt();
            const QString role = i == cacheCcd ? QStringLiteral(" V-Cache") : i == freqCcd ? QStringLiteral(" frequency") : QString();
            affinity_->addItem(QStringLiteral("CCD%1%2 (%3)").arg(i).arg(role, c.toObject().value("cpus").toString()), QStringLiteral("ccd%1").arg(i));
        }
        if (const QJsonObject h = topology_.value("hybrid").toObject(); !h.isEmpty()) {
            affinity_->addItem("P-cores (" + h.value("pcores").toString() + ")", "pcore");
            affinity_->addItem("E-cores (" + h.value("ecores").toString() + ")", "ecore");
        }
        const int k = affinity_->findData(keep);
        affinity_->setCurrentIndex(k < 0 ? 0 : k);
    }
    if (topoLabel_ && topoChanged) {
        QStringList lines;
        const int cache = topology_.value("cache_ccd").toInt(-1), freq = topology_.value("frequency_ccd").toInt(-1);
        for (const auto &cv : topology_.value("ccds").toArray()) {
            const QJsonObject c = cv.toObject();
            const int i = c.value("index").toInt();
            QString role = i == cache ? QStringLiteral(" <b style='color:%1'>V-Cache</b>").arg(theme::OK)
                         : i == freq ? QStringLiteral(" <b style='color:%1'>frequency</b>").arg(theme::ACCENT) : QString();
            lines << QStringLiteral("<b>CCD%1</b> · CPUs %2 · %3 MB L3 · max %4 MHz%5").arg(i)
                         .arg(c.value("cpus").toString()).arg(c.value("l3_kib").toInt() / 1024)
                         .arg(c.value("max_khz").toInt() / 1000).arg(role);
        }
        const QJsonObject hy = topology_.value("hybrid").toObject();
        if (!hy.isEmpty()) {
            // Intel hybrid: one shared L3, the useful split is P-cores vs E-cores.
            lines.clear();
            const QJsonArray cc = topology_.value("ccds").toArray();
            const int l3 = cc.isEmpty() ? 0 : cc.first().toObject().value("l3_kib").toInt() / 1024;
            lines << QStringLiteral("<b style='color:%1'>P-cores</b> · CPUs %2 · max %3 MHz")
                         .arg(theme::ACCENT, hy.value("pcores").toString()).arg(hy.value("pcore_max_khz").toInt() / 1000);
            lines << QStringLiteral("<b style='color:%1'>E-cores</b> · CPUs %2 · max %3 MHz")
                         .arg(theme::OK, hy.value("ecores").toString()).arg(hy.value("ecore_max_khz").toInt() / 1000);
            if (l3 > 0) lines << QStringLiteral("Shared L3 · %1 MB").arg(l3);
        }
        if (lines.isEmpty()) lines << "No L3 topology found.";
        if (topology_.value("online").toString() != topology_.value("present").toString())
            lines << QStringLiteral("<span style='color:%1'>Online CPUs: %2 of %3 — %4 parked or SMT is off.</span>")
                         .arg(theme::WARN, topology_.value("online").toString(), topology_.value("present").toString(),
                              hy.isEmpty() ? QStringLiteral("a CCD is") : QStringLiteral("E-cores are"));
        else if (hy.isEmpty() && topology_.value("ccds").toArray().size() < 2)
            lines << QStringLiteral("<span style='color:%1'>Single CCD: affinity, workqueue/IRQ steering and CCD parking do not apply.</span>").arg(theme::MUTED);
        topoLabel_->setText(lines.join("<br>"));
    }
    updateLaunchPreview();
    updateStateBanner();
    reloadPresets(presetCombo_->currentData().toString(), true);
}

void OptimizeTab::buildRows(const QJsonArray &rows) {
    const QJsonObject values = collectValues();  // keep the user's edits across a rebuild
    const QJsonObject run = collectRun();
    const int tabIndex = groups_->currentIndex();
    while (groups_->count()) { QWidget *w = groups_->widget(0); groups_->removeTab(0); w->deleteLater(); }
    rows_.clear();
    nice_ = nullptr; affinity_ = nullptr; autogroup_ = nullptr; topoLabel_ = nullptr;
    uvCpu_ = uvGpu_ = nullptr; uvInfo_ = nullptr;
    lutrisPre_ = lutrisPost_ = lutrisPrefix_ = steam_ = nullptr;

    for (const auto &v : rows) {
        const QJsonObject o = v.toObject();
        Row r;
        r.key = o.value("key").toString();
        r.group = o.value("group").toString();
        r.label = o.value("label").toString();
        // Per-CCD rows: name the die's role from the live topology.
        if (const auto m = QRegularExpression(QStringLiteral("_ccd(\\d+)$")).match(r.key); m.hasMatch()) {
            const int i = m.captured(1).toInt();
            if (topology_.value("cache_ccd").toInt(-1) == i) r.label += "  (V-Cache)";
            else if (topology_.value("frequency_ccd").toInt(-1) == i) r.label += "  (frequency)";
        }
        r.help = o.value("help").toString();
        r.kind = o.value("kind").toString();
        r.debugfs = o.value("debugfs").toBool();
        r.caution = o.value("caution").toBool();
        r.hotplug = o.value("hotplug").toBool();
        rows_.append(r);
    }

    for (const char *gname : GROUPS) {
        const QString group = QString::fromLatin1(gname);
        auto *scroll = new QScrollArea;
        scroll->setWidgetResizable(true);
        auto *page = new QWidget;
        auto *grid = new QGridLayout(page);
        grid->setContentsMargins(8, 8, 8, 8);
        grid->setHorizontalSpacing(12);
        grid->setVerticalSpacing(0);
        const char *heads[] = {"", "Setting", "Live value", "New value", ""};
        for (int c = 0; c < 5; ++c) {
            auto *h = new QLabel(QString::fromLatin1(heads[c]));
            h->setProperty("role", "muted");
            grid->addWidget(h, 0, c);
        }
        int line = 1, available = 0;
        for (int i = 0; i < rows_.size(); ++i) {
            Row &r = rows_[i];
            if (r.group != group) continue;
            const QString key = r.key;
            r.include = new QCheckBox;
            r.include->setToolTip("Include in Apply, Save and boot preset");
            r.name = new QLabel((r.caution ? QStringLiteral("⚠ ") : QString()) + r.label);
            // Fixed-width div: Qt wraps rich-text tooltips to the widget's width by
            // default, which for a short label would squeeze a long explanation into
            // a tall, narrow column. 420px reads as normal paragraphs instead.
            QString tip = QStringLiteral("<div style='max-width:420px;'><b>%1</b><br>%2</div>")
                              .arg(r.key.toHtmlEscaped(), r.help.toHtmlEscaped());
            if (r.debugfs) tip += "<br><i>debugfs: the live value is readable by root only.</i>";
            if (r.hotplug) tip += "<br><i>Hot-plugs CPUs: applied after every other row, restored first.</i>";
            if (r.caution) tip += QStringLiteral("<br><span style='color:%1'>Can cost stability, heat or idle power.</span>").arg(theme::WARN);
            r.name->setToolTip(tip);
            r.name->setTextFormat(Qt::PlainText);
            r.cur = new QLabel;
            r.cur->setMinimumWidth(140);
            r.cur->setTextInteractionFlags(Qt::TextSelectableByMouse);
            QWidget *editor;
            if (r.kind == "int") {
                r.spin = new Int64SpinBox;
                r.spin->setMinimumWidth(170);
                r.spin->setAccelerated(true);
                // Keyboard tracking off means valueChanged only fires on Enter/focus-out,
                // not per keystroke - correct for the wheel/arrow case, but it left a gap
                // while typing: the 4s poll could overwrite whatever partial number was
                // on screen before a signal ever told us the row was touched. hasFocus()
                // closes that gap without touching on every keystroke.
                r.spin->setKeyboardTracking(false);
                editor = r.spin;
                connect(r.spin, &Int64SpinBox::valueChanged, this, [this, key] {
                    if (Row *x = row(key)) { x->touched = true; x->include->setChecked(true); markRow(*x); }
                });
                // valueChanged doesn't fire if focus is lost without the number actually
                // changing (e.g. typed it, then re-typed the same value) - editingFinished
                // still does, and touched must be true the moment focus leaves or the next
                // poll's updateRow (midEdit now false) would treat it as never-edited.
                connect(r.spin, &Int64SpinBox::editingFinished, this, [this, key] {
                    if (Row *x = row(key)) x->touched = true;
                });
            } else {
                r.combo = new QComboBox;
                r.combo->setMinimumWidth(170);
                r.combo->setSizeAdjustPolicy(QComboBox::AdjustToContents);
                editor = r.combo;
                connect(r.combo, &QComboBox::activated, this, [this, key] {
                    if (Row *x = row(key)) { x->touched = true; x->include->setChecked(!editorValue(*x).isEmpty()); markRow(*x); }
                });
            }
            editor->setToolTip(tip);
            r.revert = new QPushButton("↺");
            r.revert->setFixedWidth(30);
            r.revert->setToolTip("Restore this setting's original value");
            connect(r.revert, &QPushButton::clicked, this, [this, key] { revertRow(key); });
            connect(r.include, &QCheckBox::toggled, this, [this, key] { if (Row *x = row(key)) markRow(*x); });
            grid->addWidget(r.include, line, 0);
            grid->addWidget(r.name, line, 1);
            grid->addWidget(r.cur, line, 2);
            // One column width for every editor, so the value controls line up.
            editor->setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Fixed);
            editor->setMaximumWidth(340);
            grid->addWidget(editor, line, 3);
            grid->addWidget(r.revert, line, 4);
            // Zebra stripe behind every other row, so a setting and its editor
            // across the wide gap read as one line. Layout itself is unchanged.
            if (line % 2 == 0) {
                auto *stripe = new QWidget;
                stripe->setAttribute(Qt::WA_StyledBackground);
                stripe->setStyleSheet(QStringLiteral("background: rgba(127,127,127,0.09); border-radius: 4px;"));
                stripe->setAttribute(Qt::WA_TransparentForMouseEvents);
                grid->addWidget(stripe, line, 0, 1, 5);
                stripe->lower();
            }
            grid->setRowMinimumHeight(line, editor->sizeHint().height() + 6);  // keeps the old 4px spacing
            ++line;
            updateRow(r, rows[i].toObject());
            if (r.available) ++available;
        }
        if (line == 1) { scroll->deleteLater(); page->deleteLater(); continue; }
        grid->setColumnStretch(1, 2);
        grid->setColumnStretch(3, 3);
        grid->setRowStretch(line, 1);
        scroll->setWidget(page);
        groups_->addTab(scroll, QStringLiteral("%1  %2").arg(group).arg(available));
    }
    groups_->addTab(buildLaunchPage(), "Game launch");
    groups_->addTab(new LazyWidget([] { return new BootAdvisor; }), "Boot options");  // built on first open
    if (tabIndex >= 0 && tabIndex < groups_->count()) groups_->setCurrentIndex(tabIndex);
    // Dev aids for screenshots (like LPM_TAB): LPM_OPT_SUBTAB=N, LPM_OPT_LOAD=<preset>.
    if (qEnvironmentVariableIsSet("LPM_OPT_SUBTAB")) groups_->setCurrentIndex(qEnvironmentVariableIntValue("LPM_OPT_SUBTAB"));
    if (const QString n = qEnvironmentVariable("LPM_OPT_LOAD"); !n.isEmpty() && values.isEmpty())
        QTimer::singleShot(0, this, [this, n] { const int i = presetCombo_->findData(n); if (i >= 0) { presetCombo_->setCurrentIndex(i); loadSelected(); } });

    if (!values.isEmpty() || !run.isEmpty()) {
        QJsonObject p{{"values", values}, {"run", run}};
        loadPresetObject(p, nullptr);
    }
}

void OptimizeTab::updateRow(Row &r, const QJsonObject &o) {
    const QString oldCurrent = r.current;
    const QString oldEditor = editorValue(r);
    r.available = o.value("available").toBool();
    r.current = o.value("current").isNull() ? QString() : o.value("current").toString();
    r.min = o.value("min").toVariant().toLongLong();
    r.max = o.value("max").toVariant().toLongLong();
    QList<Option> opts;
    for (const auto &v : o.value("options").toArray())
        opts.append({v.toObject().value("value").toString(), v.toObject().value("label").toString()});
    if (r.kind == "bool") opts = {{"1", "enabled"}, {"0", "disabled"}};

    const bool rootOnly = r.available && r.current.isEmpty() && r.debugfs;
    r.cur->setText(!r.available ? QStringLiteral("n/a") : rootOnly ? QStringLiteral("root only")
                                : r.kind == "bool" ? (r.current == "1" ? "enabled" : r.current == "0" ? "disabled" : r.current)
                                : r.current);
    theme::setSheet(r.cur, QStringLiteral("color:%1; background:transparent;").arg(r.available && !rootOnly ? theme::FG_DIM : theme::MUTED));
    r.cur->setToolTip(r.available ? QStringLiteral("%1 file(s)").arg(o.value("files").toInt()) : "Not present on this kernel/hardware");

    // Keep a user edit; otherwise follow the live value. A spin box mid-edit
    // (has focus, keyboard tracking off so no valueChanged yet) is left alone
    // outright - re-syncing its range/value while someone is still typing a
    // 7-digit kHz number is what let the periodic poll stomp on it before.
    const bool midEdit = r.spin && r.spin->hasFocus();
    const QString want = (r.touched || midEdit) ? oldEditor : r.current;
    if (r.combo) {
        bool same = r.options.size() == opts.size();
        for (int i = 0; same && i < opts.size(); ++i) same = r.options[i].value == opts[i].value;
        r.options = opts;
        const QSignalBlocker b(r.combo);
        if (!same || r.combo->count() == 0 || oldCurrent != r.current) {
            r.combo->clear();
            const bool offered = std::any_of(opts.begin(), opts.end(), [&](const Option &x) { return x.value == r.current; });
            // A live state that is not a choice (mixed, "offline 8-15", "4000 kHz"): shown, not applicable.
            if (!offered) r.combo->addItem(r.current.isEmpty() ? QStringLiteral("—") : QStringLiteral("— (%1)").arg(r.current), QString());
            for (const Option &x : opts) r.combo->addItem(x.label, x.value);
        }
        if (!setEditorValue(r, want)) {
            if (r.touched && !want.isEmpty()) {
                // The user's choice is not offered right now (e.g. the CCD it
                // parks is offline, so sysfs no longer describes it). Keep it
                // instead of silently falling back to a value-less placeholder,
                // which Save then dropped without a word.
                r.combo->addItem(QStringLiteral("%1  (not offered right now)").arg(want), want);
                setEditorValue(r, want);
            } else {
                setEditorValue(r, r.current);
            }
        }
    } else if (r.spin) {
        if (midEdit) {
            // Still typing: don't touch range or value, just let the row's
            // "current" and availability update underneath for when they blur out.
        } else {
            const QSignalBlocker b(r.spin);
            r.spin->setRange(r.min, r.max);
            if (!setEditorValue(r, want)) setEditorValue(r, r.current);
        }
    }
    const bool saved = state_.value("keys").toArray().contains(r.key);
    r.revert->setVisible(saved);
    for (QWidget *w : {static_cast<QWidget *>(r.include), static_cast<QWidget *>(r.name), static_cast<QWidget *>(r.combo),
                       static_cast<QWidget *>(r.spin)})
        if (w) w->setEnabled(r.available);
    if (!r.available) r.include->setChecked(false);
    markRow(r);
}

bool OptimizeTab::validFor(const Row &r, const QString &v) {
    if (v.isEmpty() || !r.available) return false;
    if (r.kind == "int") {
        bool ok = false;
        const qint64 n = v.toLongLong(&ok);
        return ok && n >= r.min && n <= r.max;
    }
    if (r.kind == "bool") return v == "1" || v == "0";
    // debugfs choice rows are Fixed lists; the kernel has the last word.
    return std::any_of(r.options.begin(), r.options.end(), [&](const Option &o) { return o.value == v; });
}

QString OptimizeTab::editorValue(const Row &r) const {
    if (r.spin) return QString::number(r.spin->value());
    if (r.combo) return r.combo->currentData().toString();
    return {};
}

/// Rows whose value is a CCD role (tune.rs maps the same three in validate).
/// Other rows use "cache"/"frequency" as their own values (cpu.x3d_mode) and
/// must not be rewritten to "ccdN".
static bool isCcdRoleKey(const QString &key) {
    return key == QLatin1String("wq.cpumask") || key == QLatin1String("irq.affinity") || key == QLatin1String("cpu.ccd_park");
}

/// Legacy "cache"/"frequency" (older presets, built-ins) → the CCD they are here.
QString OptimizeTab::canonicalCcd(const QString &v) const {
    if (v == QLatin1String("cache") || v == QLatin1String("frequency")) {
        const int i = topology_.value(v == QLatin1String("cache") ? "cache_ccd" : "frequency_ccd").toInt(-1);
        if (i >= 0) return QStringLiteral("ccd%1").arg(i);
    }
    return v;
}

bool OptimizeTab::setEditorValue(Row &r, const QString &v0) {
    const QString v = r.combo && isCcdRoleKey(r.key) ? canonicalCcd(v0) : v0;
    if (r.spin) {
        bool ok = false;
        const qint64 n = v.toLongLong(&ok);
        if (!ok || n < r.min || n > r.max) return false;
        const QSignalBlocker b(r.spin);
        r.spin->setValue(n);
        return true;
    }
    if (r.combo) {
        const int i = r.combo->findData(v);
        if (i < 0) return false;
        const QSignalBlocker b(r.combo);
        r.combo->setCurrentIndex(i);
        return true;
    }
    return false;
}

bool OptimizeTab::differs(const Row &r) const {
    const QString v = editorValue(r);
    return r.available && !v.isEmpty() && v != r.current;
}

void OptimizeTab::markRow(Row &r) {
    if (!r.name) return;
    const bool pending = r.include->isChecked() && differs(r);
    const char *color = !r.available ? theme::MUTED : pending ? theme::ACCENT : r.caution ? theme::WARN : theme::FG;
    theme::setSheet(r.name, QStringLiteral("color:%1; background:transparent;%2").arg(color, pending ? " font-weight:600;" : ""));
}

OptimizeTab::Row *OptimizeTab::row(const QString &key) {
    for (Row &r : rows_) if (r.key == key) return &r;
    return nullptr;
}

void OptimizeTab::updateStateBanner() {
    auto *frame = banner_->parentWidget();
    const char *accent = theme::OK;
    if (helperMissing_) {
        accent = theme::DANGER;
        banner_->setText("tune-helper is not installed");
        bannerDetail_->setText("Expected at " + helperPath() + ". Run install.sh (or the ebuild) to install the helpers.");
    } else if (state_.value("active").toBool()) {
        accent = theme::WARN;
        const int games = state_.value("refcount").toInt();
        const QString src = state_.value("source").toString();
        const QString preset = state_.value("preset").toString();
        banner_->setText(games > 0 ? QStringLiteral("Game mode active — %1 game(s) running").arg(games)
                                   : src == "boot" ? QStringLiteral("Boot preset active") : QStringLiteral("Tuning active"));
        QString part;
        if (isolation_.value("active").toBool())
            part = QStringLiteral("\nGame CPU partition: CPUs %1 (%2 process(es)) — the rest of the system runs on the other CCD.")
                       .arg(isolation_.value("cpus").toString()).arg(isolation_.value("procs").toInt());
        else if (games > 0 && isolation_.value("unsupported").isString())
            part = QStringLiteral("\nNo game CPU partition: ") + isolation_.value("unsupported").toString();
        bannerDetail_->setText(QStringLiteral("%1%2 setting(s), %3 file(s) with saved originals. Restoring writes them back.%4")
            .arg(preset.isEmpty() ? QString() : "Preset \"" + preset + "\" · ")
            .arg(state_.value("keys").toArray().size()).arg(state_.value("saved_files").toInt()).arg(part));
    } else {
        banner_->setText("System at its original values");
        bannerDetail_->setText("Nothing changed by Legion Power Manager is in effect. Every change you apply is recorded and reversible.");
    }
    active_ = state_.value("active").toBool();
    theme::setSheet(frame, theme::banner(accent, QStringLiteral("#tuneBanner")));
    theme::setSheet(banner_, QStringLiteral("font-weight:600; background:transparent; color:%1;").arg(accent));
    restoreBtn_->setEnabled(active_ && !busy_);

    const QString bootName = boot_.value("preset").toString();
    const int bootCount = boot_.value("values").toObject().size();
    bootLabel_->setText(boot_.isEmpty() ? QStringLiteral("No boot preset")
                        : QStringLiteral("⏻ Boot: \"%1\" · %2 setting(s)")
                              .arg(bootName.isEmpty() ? QStringLiteral("unnamed") : bootName).arg(bootCount));
    bootLabel_->setToolTip("Applied by the lpm-tune OpenRC service. Enable it once with:\n    rc-update add lpm-tune boot");
    bootClear_->setEnabled(!boot_.isEmpty() && !busy_);
}

// ── presets ─────────────────────────────────────────────────────────────────

QStringList OptimizeTab::presetNames() const {
    QStringList out;
    for (const Builtin &b : BUILTINS) if (builtinForThisCpu(b)) out << QString::fromLatin1(b.name);
    QStringList user = QDir(presetsDir()).entryList({"*.json"}, QDir::Files, QDir::Name);
    for (QString &s : user) { s.chop(5); if (validPresetName(s) && !out.contains(s)) out << s; }
    return out;
}

QJsonObject OptimizeTab::presetObject(const QString &name) const {
    if (!validPresetName(name)) return {};
    const QString file = presetsDir() + '/' + name + ".json";
    if (QFile::exists(file)) return readJsonFile(file);
    if (const Builtin *b = builtin(name)) return builtinObject(*b);
    return {};
}

void OptimizeTab::reloadPresets(const QString &select, bool poll) {
    // The periodic describe refresh leaves an open drop-down alone (clearing
    // the model closed it under the cursor).
    if (poll && presetCombo_->view()->isVisible()) return;
    const QString keep = select.isEmpty() ? presetCombo_->currentData().toString() : select;
    const QString game = gamePreset(), bootName = boot_.value("preset").toString();
    QList<QPair<QString, QString>> items;  // (label, name)
    for (const QString &n : presetNames()) {
        const bool user = QFile::exists(presetsDir() + '/' + n + ".json");
        QString label = (builtin(n) && !user ? QStringLiteral("◆ ") : QString()) + n;
        if (builtin(n) && user) label += "  (edited)";
        if (n == game) label += "  ★";
        if (!bootName.isEmpty() && n == bootName) label += "  ⏻";
        items.append({label, n});
    }
    if (poll) {
        // Every 4 s: nothing to do unless the list itself changed (preset saved or
        // deleted elsewhere, game/boot marker moved) — the refill and the summary
        // re-read (a JSON parse per poll) used to run unconditionally.
        bool same = presetCombo_->count() == items.size();
        for (int i = 0; same && i < items.size(); ++i)
            same = presetCombo_->itemText(i) == items[i].first && presetCombo_->itemData(i).toString() == items[i].second;
        if (same) return;
    }
    {
        const QSignalBlocker b(presetCombo_);
        presetCombo_->clear();
        for (const auto &[label, n] : items) presetCombo_->addItem(label, n);
        const int i = presetCombo_->findData(keep);
        presetCombo_->setCurrentIndex(i < 0 ? 0 : i);
    }
    Q_EMIT presetCombo_->currentIndexChanged(presetCombo_->currentIndex());
}

QJsonObject OptimizeTab::collectValues() const {
    QJsonObject v;
    for (const Row &r : rows_) {
        if (!r.include || !r.include->isChecked() || !r.available) continue;
        const QString val = editorValue(r);
        if (val.isEmpty()) continue;
        v[r.key] = r.kind == "int" ? QJsonValue(val.toLongLong()) : QJsonValue(val);
    }
    return v;
}

QJsonObject OptimizeTab::collectRun() const {
    if (!nice_) return {};
    return {{"nice", nice_->value()}, {"autogroup", autogroup_->isChecked()},
            {"affinity", affinity_->currentData().toString().isEmpty() ? QStringLiteral("none") : affinity_->currentData().toString()}};
}

int OptimizeTab::loadPresetObject(const QJsonObject &p, QStringList *skipped) {
    const QJsonObject values = p.value("values").toObject();
    int n = 0;
    for (Row &r : rows_) {
        if (!r.include) continue;
        const QSignalBlocker b(r.include);
        r.include->setChecked(false);
        r.touched = false;
        setEditorValue(r, r.current);
    }
    for (auto it = values.begin(); it != values.end(); ++it) {
        Row *r = row(it.key());
        const QString raw = it->isString() ? it->toString() : QString::number(it->toVariant().toLongLong());
        const QString v = isCcdRoleKey(it.key()) ? canonicalCcd(raw) : raw;
        if (!r || !validFor(*r, v) || !setEditorValue(*r, v)) { if (skipped) *skipped << it.key(); continue; }
        r->touched = true;
        const QSignalBlocker b(r->include);
        r->include->setChecked(true);
        ++n;
    }
    for (Row &r : rows_) markRow(r);
    const QJsonObject run = p.value("run").toObject();
    if (nice_ && !run.isEmpty()) {
        nice_->setValue(std::clamp(run.value("nice").toInt(0), -20, 0));
        autogroup_->setChecked(run.value("autogroup").toBool(true));
        const int i = affinity_->findData(canonicalCcd(run.value("affinity").toString("none")));
        affinity_->setCurrentIndex(i < 0 ? 0 : i);
    }
    return n;
}

void OptimizeTab::loadSelected() {
    const QString name = presetCombo_->currentData().toString();
    const QJsonObject p = presetObject(name);
    if (p.isEmpty()) { QMessageBox::warning(this, "Load preset", "Cannot read preset \"" + name + "\"."); return; }
    QStringList skipped;
    const int n = loadPresetObject(p, &skipped);
    loadedPreset_ = name;
    QString msg = QStringLiteral("Loaded \"%1\": %2 setting(s) checked").arg(name).arg(n);
    if (!skipped.isEmpty()) msg += QStringLiteral(", %1 not offered here (%2)").arg(skipped.size()).arg(skipped.join(", "));
    showStatus(msg + ". Review, then Apply checked.", skipped.isEmpty() ? theme::OK : theme::WARN, 10000);
}

bool OptimizeTab::writeUserPreset(const QString &name, const QJsonObject &p, QString *err) {
    if (!validPresetName(name)) { if (err) *err = "invalid name"; return false; }
    QJsonObject o = p;
    o["version"] = 1;
    return writeJsonFile(presetsDir() + '/' + name + ".json", o, err);
}

void OptimizeTab::saveAs() {
    const QJsonObject values = collectValues();
    // Checked rows that have no value to save: say so instead of dropping them.
    QStringList dropped;
    for (const Row &r : rows_)
        if (r.include && r.include->isChecked() && r.available && editorValue(r).isEmpty()) dropped << r.name->text();
    if (!dropped.isEmpty() &&
        QMessageBox::warning(this, "Save preset",
            "These checked rows have no value selected and will not be saved:\n\n• " + dropped.join("\n• ") +
            "\n\nPick a value for them first, or save without them?",
            QMessageBox::Save | QMessageBox::Cancel, QMessageBox::Cancel) != QMessageBox::Save)
        return;
    if (values.isEmpty()) { QMessageBox::information(this, "Save preset", "Check at least one row first."); return; }
    bool ok = false;
    const QString name = QInputDialog::getText(this, "Save preset",
        "Preset name (letters, digits, space, _ - .):", QLineEdit::Normal, loadedPreset_, &ok).trimmed();
    if (!ok || name.isEmpty()) return;
    if (!validPresetName(name)) { QMessageBox::warning(this, "Save preset", "Invalid name."); return; }
    if (QFile::exists(presetsDir() + '/' + name + ".json") &&
        QMessageBox::question(this, "Save preset", "Overwrite \"" + name + "\"?") != QMessageBox::Yes) return;
    QString err;
    if (!writeUserPreset(name, {{"values", values}, {"run", collectRun()}}, &err)) {
        QMessageBox::critical(this, "Save preset", err);
        return;
    }
    loadedPreset_ = name;
    reloadPresets(name);
    approvePreset(name, values, [this, name, n = values.size()] {
        showStatus(QStringLiteral("Saved \"%1\" (%2 settings)").arg(name).arg(n), theme::OK);
    });
}

void OptimizeTab::deleteSelected() {
    const QString name = presetCombo_->currentData().toString();
    const QString file = presetsDir() + '/' + name + ".json";
    if (!QFile::exists(file)) return;
    const QString extra = builtin(name) ? QStringLiteral("\n\nThe built-in version comes back.") : QString();
    if (QMessageBox::question(this, "Delete preset", "Delete \"" + name + "\"?" + extra) != QMessageBox::Yes) return;
    QFile::remove(file);
    if (gamePreset() == name && !builtin(name)) {
        QJsonObject cfg = readJsonFile(configFile());
        cfg.remove("game_preset");
        writeJsonFile(configFile(), cfg, nullptr);
    }
    reloadPresets();
    updateLaunchPreview();
    if (storeHas(name)) runOp({{"op", "preset_delete"}, {"name", name}}, QStringLiteral("Remove approved copy"), nullptr);
}

void OptimizeTab::useForGames() {
    const QString name = presetCombo_->currentData().toString();
    const QJsonObject p = presetObject(name);
    if (p.isEmpty()) return;
    QString err;
    if (!QFile::exists(presetsDir() + '/' + name + ".json") && !writeUserPreset(name, p, &err)) {
        QMessageBox::critical(this, "Use for games", err);
        return;
    }
    QJsonObject cfg = readJsonFile(configFile());
    cfg["game_preset"] = name;
    if (!writeJsonFile(configFile(), cfg, &err)) { QMessageBox::critical(this, "Use for games", err); return; }
    reloadPresets(name);
    updateLaunchPreview();
    auto announce = [this, name] {
        showStatus(QStringLiteral("★ \"%1\" is now the game preset. Hook lpm-gamemode into Lutris/Steam (Game launch tab).").arg(name), theme::OK, 10000);
    };
    // lpm-gamemode applies the root-owned approved copy, by name.
    if (storeHas(name)) announce(); else approvePreset(name, p.value("values").toObject(), announce);
}

void OptimizeTab::setBoot() {
    const QJsonObject values = collectValues();
    if (values.isEmpty()) { QMessageBox::information(this, "Apply at boot", "Check the rows to apply at boot first."); return; }
    QStringList risky;
    for (const Row &r : rows_) if (values.contains(r.key) && r.caution) risky << r.label;
    QString msg = QStringLiteral("Store %1 checked setting(s) as the boot preset?\n\nThey are applied early at every boot by the "
                                 "lpm-tune OpenRC service:\n    rc-update add lpm-tune boot").arg(values.size());
    if (!risky.isEmpty()) msg += "\n\n⚠ Includes: " + risky.join(", ") + ". A bad value here is applied before you can log in.";
    if (QMessageBox::question(this, "Apply at boot", msg) != QMessageBox::Yes) return;
    const QString name = validPresetName(loadedPreset_) ? loadedPreset_ : QStringLiteral("Custom");
    runOp({{"op", "set_boot"}, {"values", values}, {"preset", name}}, "Boot preset", [this, name](const QJsonObject &) {
        showStatus("Boot preset \"" + name + "\" stored.", theme::OK);
    });
}

void OptimizeTab::clearBoot() {
    if (QMessageBox::question(this, "Clear boot preset", "Stop applying a preset at boot?") != QMessageBox::Yes) return;
    runOp({{"op", "set_boot"}, {"values", QJsonValue::Null}}, "Clear boot preset",
          [this](const QJsonObject &) { showStatus("Boot preset cleared.", theme::OK); });
}

// ── autotune ────────────────────────────────────────────────────────────────

void OptimizeTab::runAutotune() {
    if (autoRunning_ || busy_) return;
    if (rows_.isEmpty()) { showStatus(QStringLiteral("The tunable list is not loaded yet."), theme::WARN); return; }
    if (!QFileInfo(helperPath()).isExecutable()) { helperMissing_ = true; updateStateBanner(); return; }
    const QString goal = autoGoal_->currentData().toString();
    autoRunning_ = true;
    autoBtn_->setEnabled(false);
    showStatus(QStringLiteral("Profiling the hardware…"), theme::MUTED, 0);
    // Read-only op: runs unprivileged, like describe.
    auto *p = new QProcess(this);
    QPointer<QProcess> guard(p);
    connect(p, &QProcess::finished, this, [this, p, goal](int, QProcess::ExitStatus) {
        autoRunning_ = false;
        autoBtn_->setEnabled(!busy_);
        const QByteArray out = p->readAllStandardOutput().trimmed();
        p->deleteLater();
        const QJsonObject d = QJsonDocument::fromJson(out.mid(out.lastIndexOf('\n') + 1)).object();
        if (!d.value("ok").toBool()) {
            showStatus(QStringLiteral("Autotune failed: %1").arg(d.value("error").toString(QStringLiteral("no answer from tune-helper"))), theme::DANGER, 10000);
            return;
        }
        QStringList notLoaded;
        const int n = loadPresetObject(d.value("preset").toObject(), &notLoaded);
        loadedPreset_ = d.value("name").toString();
        showStatus(QStringLiteral("Autotune · %1: %2 setting(s) checked. Review, then Apply checked or Save.")
                       .arg(d.value("goal_label").toString()).arg(n), theme::OK, 12000);
        showAutotuneReport(d, notLoaded);
    });
    connect(p, &QProcess::errorOccurred, this, [this, p](QProcess::ProcessError e) {
        if (e != QProcess::FailedToStart) return;
        autoRunning_ = false;
        autoBtn_->setEnabled(!busy_);
        showStatus(QStringLiteral("tune-helper could not be started."), theme::DANGER);
        p->deleteLater();
    });
    QTimer::singleShot(AUTOTUNE_TIMEOUT_MS, p, [guard] { if (guard && guard->state() != QProcess::NotRunning) guard->kill(); });
    p->start(helperPath(), {});
    QJsonObject req{{"op", "autotune"}, {"goal", goal}};
    if (const QJsonObject w = autotuneWeights(goal); !w.isEmpty()) req["weights"] = w;
    p->write(QJsonDocument(req).toJson(QJsonDocument::Compact));
    p->closeWriteChannel();
}

// Mirror of lpm_helpers::autotune::Weights::for_goal (only used to prefill the editor).
static QList<double> defaultWeights(const QString &goal) {
    if (goal == QLatin1String("gaming")) return {1.0, 0.6, 0.15, 0.4, 1.0, 0.5};
    if (goal == QLatin1String("throughput")) return {0.2, 1.0, 0.2, 0.5, 1.0, 0.7};
    if (goal == QLatin1String("powersave")) return {0.2, 0.1, 1.0, 0.5, 1.0, 0.4};
    return {0.7, 0.3, 0.7, 0.6, 1.0, 0.6};
}
static const char *const WEIGHT_KEYS[] = {"latency", "throughput", "power", "footprint", "stability", "storage"};
static constexpr int N_WEIGHTS = int(sizeof(WEIGHT_KEYS) / sizeof(WEIGHT_KEYS[0]));

QJsonObject OptimizeTab::autotuneWeights(const QString &goal) const {
    const QByteArray raw = QSettings().value(QStringLiteral("autotune/weights/") + goal).toByteArray();
    return QJsonDocument::fromJson(raw).object();
}

void OptimizeTab::editAutotuneWeights() {
    const QString goal = autoGoal_->currentData().toString();
    const QList<double> def = defaultWeights(goal);
    const QJsonObject cur = autotuneWeights(goal);
    QDialog dlg(this);
    dlg.setWindowTitle(QStringLiteral("Autotune weights — %1").arg(autoGoal_->currentText()));
    auto *form = new QFormLayout(&dlg);
    form->addRow(muted(QStringLiteral("0 = ignore this objective, 1 = the goal's normal emphasis, up to 3.\n"
                                      "Stability cannot go below 0.5. Saved per goal.\n"
                                      "Storage: how much the calibrated disk benchmarks count (Storage rows, dirty window);\n"
                                      "their latency/throughput/power/footprint are weighed with the weights above times this.")));
    const QStringList labels = {QStringLiteral("Latency / smoothness"), QStringLiteral("Throughput"), QStringLiteral("Power / heat"),
                                QStringLiteral("Memory footprint"), QStringLiteral("Stability"), QStringLiteral("Storage (I/O) relevance")};
    QList<QDoubleSpinBox *> boxes;
    for (int i = 0; i < N_WEIGHTS; ++i) {
        auto *b = new QDoubleSpinBox;
        b->setRange(i == 4 ? 0.5 : 0.0, 3.0);
        b->setSingleStep(0.1);
        b->setDecimals(2);
        b->setValue(cur.contains(WEIGHT_KEYS[i]) ? cur.value(WEIGHT_KEYS[i]).toDouble() : def[i]);
        form->addRow(labels[i], b);
        boxes << b;
    }
    auto *bb = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel | QDialogButtonBox::RestoreDefaults);
    connect(bb->button(QDialogButtonBox::RestoreDefaults), &QPushButton::clicked, &dlg, [&] { for (int i = 0; i < N_WEIGHTS; ++i) boxes[i]->setValue(def[i]); });
    connect(bb, &QDialogButtonBox::accepted, &dlg, &QDialog::accept);
    connect(bb, &QDialogButtonBox::rejected, &dlg, &QDialog::reject);
    form->addRow(bb);
    if (dlg.exec() != QDialog::Accepted) return;
    QJsonObject w;
    bool custom = false;
    for (int i = 0; i < N_WEIGHTS; ++i) {
        w[WEIGHT_KEYS[i]] = boxes[i]->value();
        custom |= qAbs(boxes[i]->value() - def[i]) > 1e-6;
    }
    const QString key = QStringLiteral("autotune/weights/") + goal;
    if (custom) QSettings().setValue(key, QJsonDocument(w).toJson(QJsonDocument::Compact));
    else QSettings().remove(key);
    showStatus(custom ? QStringLiteral("Custom weights saved for %1; run Autotune to use them.").arg(autoGoal_->currentText())
                      : QStringLiteral("%1 uses its default weights.").arg(autoGoal_->currentText()), theme::OK, 6000);
}

void OptimizeTab::showAutotuneReport(const QJsonObject &d, const QStringList &notLoaded) {
    const QJsonObject preset = d.value("preset").toObject();
    const QJsonObject values = preset.value("values").toObject();
    const QJsonObject why = d.value("rationale").toObject();
    const QString goal = d.value("goal").toString();

    QDialog dlg(this);
    dlg.setWindowTitle(QStringLiteral("Autotune — %1").arg(d.value("goal_label").toString()));
    dlg.resize(980, 600);
    auto *v = new QVBoxLayout(&dlg);
    v->setSpacing(6);
    auto *head = new QLabel(QStringLiteral("<b>Machine</b>&nbsp; %1").arg(d.value("profile_summary").toString().toHtmlEscaped()));
    head->setWordWrap(true);
    head->setTextFormat(Qt::RichText);
    head->setTextInteractionFlags(Qt::TextSelectableByMouse);
    v->addWidget(head);
    if (const QString ev = d.value(QStringLiteral("evidence_summary")).toString(); !ev.isEmpty()) {
        auto *evl = new QLabel(QStringLiteral("<b>Observed</b>&nbsp; %1").arg(ev.toHtmlEscaped()));
        evl->setWordWrap(true);
        evl->setTextFormat(Qt::RichText);
        evl->setTextInteractionFlags(Qt::TextSelectableByMouse);
        v->addWidget(evl);
    }

    {
        QStringList w;
        const QJsonObject wo = d.value(QStringLiteral("weights")).toObject();
        for (const char *k : WEIGHT_KEYS) w << QStringLiteral("%1 %2").arg(QLatin1String(k)).arg(wo.value(k).toDouble(), 0, 'f', 2);
        QStringList extra;
        for (const auto &c : d.value(QStringLiteral("constraints")).toArray()) extra << QStringLiteral("constraint: ") + c.toString();
        for (const auto &i : d.value(QStringLiteral("live_issues")).toArray()) {
            const QJsonObject o = i.toObject();
            extra << QStringLiteral("live %1: %2").arg(o.value("key").toString(), o.value("message").toString());
        }
        auto *wl = new QLabel(QStringLiteral("<b>Weights</b>&nbsp; %1%2").arg(w.join(QStringLiteral(" · ")).toHtmlEscaped(),
            extra.isEmpty() ? QString() : QStringLiteral("<br>") + extra.join(QStringLiteral("<br>")).toHtmlEscaped()));
        wl->setWordWrap(true);
        wl->setTextFormat(Qt::RichText);
        wl->setTextInteractionFlags(Qt::TextSelectableByMouse);
        v->addWidget(wl);
    }
    auto *table = new QTableWidget(0, 4);
    table->setHorizontalHeaderLabels({QStringLiteral("Group"), QStringLiteral("Setting"), QStringLiteral("Value"), QStringLiteral("Why (for this machine)")});
    table->verticalHeader()->hide();
    table->setEditTriggers(QAbstractItemView::NoEditTriggers);
    table->setSelectionMode(QAbstractItemView::NoSelection);
    table->setWordWrap(true);
    table->horizontalHeader()->setSectionResizeMode(0, QHeaderView::ResizeToContents);
    table->horizontalHeader()->setSectionResizeMode(1, QHeaderView::ResizeToContents);
    table->horizontalHeader()->setSectionResizeMode(2, QHeaderView::ResizeToContents);
    table->horizontalHeader()->setSectionResizeMode(3, QHeaderView::Stretch);
    auto add = [table](const QString &g, const QString &name, const QString &val, const QString &reason, bool changes) {
        const int r = table->rowCount();
        table->insertRow(r);
        auto *a = new QTableWidgetItem(g), *b = new QTableWidgetItem(name), *c = new QTableWidgetItem(val), *e = new QTableWidgetItem(reason);
        if (changes) { QFont f = c->font(); f.setBold(true); c->setFont(f); }
        else c->setToolTip(QStringLiteral("Already the live value."));
        for (auto *it : {a, b, c, e}) it->setToolTip(reason);
        table->setItem(r, 0, a); table->setItem(r, 1, b); table->setItem(r, 2, c); table->setItem(r, 3, e);
    };
    int changing = 0;
    for (const char *gname : GROUPS) {
        for (const Row &r : rows_) {
            if (r.group != QLatin1String(gname) || !values.contains(r.key) || notLoaded.contains(r.key)) continue;
            const QString shown = r.combo ? r.combo->currentText() : editorValue(r);
            const bool ch = differs(r);
            changing += ch;
            add(r.group, r.label, shown + (ch ? QString() : QStringLiteral("  (=)")), why.value(r.key).toString(), ch);
        }
    }
    const QJsonObject run = preset.value("run").toObject();
    if (why.contains("run"))
        add(QStringLiteral("Launch"), QStringLiteral("Game launch boost"),
            QStringLiteral("nice %1 · %2").arg(run.value("nice").toInt()).arg(run.value("affinity").toString()), why.value("run").toString(), true);
    table->resizeRowsToContents();
    v->addWidget(table, 1);

    QStringList off;
    for (const auto &s : d.value("skipped").toArray()) off << s.toObject().value("key").toString();
    off << notLoaded;
    off.removeDuplicates();
    auto *foot = muted(QStringLiteral("%1 row(s) checked, %2 of them change the live value (bold). Nothing has been written yet: "
                                      "review in the tab, then Apply checked.%3")
                           .arg(values.size() - notLoaded.size()).arg(changing)
                           .arg(off.isEmpty() ? QString() : QStringLiteral("\nNot offered on this machine/kernel: ") + off.join(QStringLiteral(", "))));
    v->addWidget(foot);

    auto *bb = new QDialogButtonBox;
    auto *save = bb->addButton(QStringLiteral("Save as preset…"), QDialogButtonBox::AcceptRole);
    QPushButton *saveGame = goal == QLatin1String("gaming") ? bb->addButton(QStringLiteral("Save + ★ use for games"), QDialogButtonBox::AcceptRole) : nullptr;
    bb->addButton(QStringLiteral("Close"), QDialogButtonBox::RejectRole);
    v->addWidget(bb);
    bool forGames = false;
    connect(save, &QPushButton::clicked, &dlg, &QDialog::accept);
    if (saveGame) connect(saveGame, &QPushButton::clicked, &dlg, [&] { forGames = true; dlg.accept(); });
    connect(bb, &QDialogButtonBox::rejected, &dlg, &QDialog::reject);
    if (dlg.exec() != QDialog::Accepted) return;

    bool ok = false;
    const QString name = QInputDialog::getText(this, QStringLiteral("Save autotuned preset"),
        QStringLiteral("Preset name (letters, digits, space, _ - .):"), QLineEdit::Normal, d.value("name").toString(), &ok).trimmed();
    if (!ok || name.isEmpty()) return;
    if (!validPresetName(name)) { QMessageBox::warning(this, QStringLiteral("Save preset"), QStringLiteral("Invalid name.")); return; }
    if (QFile::exists(presetsDir() + '/' + name + ".json") &&
        QMessageBox::question(this, QStringLiteral("Save preset"), "Overwrite \"" + name + "\"?") != QMessageBox::Yes) return;
    // What is saved is what the tab now holds (the user may have edited rows
    // while the report was open), plus the autotune record and summary.
    QJsonObject p{{"values", collectValues()}, {"run", collectRun()}, {"summary", preset.value("summary")}, {"autotune", preset.value("autotune")}};
    QString err;
    if (!writeUserPreset(name, p, &err)) { QMessageBox::critical(this, QStringLiteral("Save preset"), err); return; }
    loadedPreset_ = name;
    reloadPresets(name);
    approvePreset(name, p.value("values").toObject(), [this, name, forGames] {
        if (forGames) useForGames();
        else showStatus(QStringLiteral("Saved \"%1\".").arg(name), theme::OK);
    });
}

// ── apply / restore ─────────────────────────────────────────────────────────

void OptimizeTab::applySelected() {
    const QJsonObject values = collectValues();
    if (values.isEmpty()) { QMessageBox::information(this, "Apply", "No row is checked."); return; }
    QStringList risky;
    for (const Row &r : rows_) if (values.contains(r.key) && r.caution && differs(r)) risky << "• " + r.label + " → " + (r.combo ? r.combo->currentText() : editorValue(r));
    if (!risky.isEmpty() && QMessageBox::warning(this, "Apply",
            "These changes can cost stability, heat or idle power:\n\n" + risky.join('\n') +
            "\n\nEverything stays reversible with Restore originals. Continue?",
            QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes) return;
    applyValues(values, loadedPreset_);
}

void OptimizeTab::applyValues(const QJsonObject &values, const QString &preset) {
    QJsonObject req{{"op", "apply"}, {"mode", "manual"}, {"values", values}};
    if (validPresetName(preset)) req["preset"] = preset;
    runOp(req, "Apply", [this](const QJsonObject &res) {
        int written = 0, skipped = 0, refused = 0;
        QStringList errors;
        for (const auto &v : res.value("results").toArray()) {
            const QJsonObject o = v.toObject();
            written += o.value("written").toInt();
            refused += o.value("refused").toInt();
            if (o.contains("skipped")) ++skipped;
            if (!o.value("ok").toBool()) errors << o.value("key").toString() + ": " + o.value("error").toString();
        }
        QString msg = QStringLiteral("Applied: %1 file(s) written").arg(written);
        if (skipped) msg += QStringLiteral(", %1 not available").arg(skipped);
        if (refused) msg += QStringLiteral(", %1 refused by the kernel (IRQ/PCI, expected)").arg(refused);
        if (res.contains(QStringLiteral("guard")))
            msg += QStringLiteral(" · pressure guard watching %1 memory/writeback setting(s) for %2 s")
                       .arg(res.value("guard").toObject().value("keys").toArray().size()).arg(res.value("guard").toObject().value("seconds").toInt());
        showStatus(msg, errors.isEmpty() ? theme::OK : theme::WARN, 10000);
        if (!errors.isEmpty()) QMessageBox::warning(this, "Apply", "Some settings failed:\n\n" + errors.join('\n'));
        for (Row &r : rows_) r.touched = false;
    });
}

bool OptimizeTab::applyNamedPreset(const QString &name) {
    if (busy_) return false;
    const QJsonObject p = presetObject(name);
    if (p.isEmpty()) return false;
    QJsonObject values;
    const QJsonObject all = p.value("values").toObject();
    for (auto it = all.begin(); it != all.end(); ++it) {
        const Row *r = nullptr;
        for (const Row &x : rows_) if (x.key == it.key()) r = &x;
        const QString raw = it->isString() ? it->toString() : QString::number(it->toVariant().toLongLong());
        const QString v = isCcdRoleKey(it.key()) ? canonicalCcd(raw) : raw;
        if (r && validFor(*r, v)) values[it.key()] = v;
    }
    if (values.isEmpty()) return false;
    loadPresetObject(p, nullptr);
    loadedPreset_ = name;
    applyValues(values, name);
    return true;
}

void OptimizeTab::revertRow(const QString &key) {
    runOp({{"op", "restore_keys"}, {"keys", QJsonArray{key}}}, "Restore",
          [this, key](const QJsonObject &) { if (Row *r = row(key)) { r->touched = false; showStatus(r->label + " restored.", theme::OK); } });
}

void OptimizeTab::restoreAll(bool confirm) {
    if (busy_ || !active_) return;
    if (confirm) {
        QString msg = "Write every saved original value back?";
        if (state_.value("refcount").toInt() > 0)
            msg += QStringLiteral("\n\n%1 game session(s) are active; their POST hook will then have nothing left to restore.")
                       .arg(state_.value("refcount").toInt());
        if (QMessageBox::question(this, "Restore originals", msg) != QMessageBox::Yes) return;
    }
    runOp({{"op", "restore"}}, "Restore", [this](const QJsonObject &res) {
        const QJsonObject r = res.value("results").toArray().at(0).toObject();
        for (Row &x : rows_) x.touched = false;
        showStatus(QStringLiteral("Restored %1 file(s).").arg(r.value("restored").toInt()), theme::OK);
    });
}

void OptimizeTab::approvePreset(const QString &name, const QJsonObject &values, std::function<void()> then) {
    runOp({{"op", "preset_save"}, {"name", name}, {"values", values}}, QStringLiteral("Approve preset"),
          [then](const QJsonObject &res) { if (res.value(QStringLiteral("ok")).toBool() && then) then(); });
}

void OptimizeTab::runOp(const QJsonObject &req, const QString &what, std::function<void(const QJsonObject &)> then) {
    if (busy_) return;
    setBusy(true);
    showStatus(what + "…", theme::MUTED, 0);
    privileged::run(helperForOp(req.value(QStringLiteral("op")).toString()), req, this, [this, what, then](const privileged::Result &r) {
        setBusy(false);
        if (!r.reached) {
            showStatus(what + " failed.", theme::DANGER);
            QMessageBox::critical(this, what, r.error);
        } else {
            if (r.json.contains("state")) state_ = r.json.value("state").toObject();
            if (r.json.contains("boot")) boot_ = r.json.value("boot").toObject();
            if (then) then(r.json);
            if (!r.ok() && r.json.value("results").toArray().isEmpty())
                QMessageBox::critical(this, what, r.message().isEmpty() ? QStringLiteral("unknown error") : r.message());
        }
        updateStateBanner();
        reloadPresets();
        refresh();
    }, PKEXEC_TIMEOUT_MS);
}

void OptimizeTab::setBusy(bool b) {
    busy_ = b;
    groups_->setEnabled(!b);
    applyBtn_->setEnabled(!b);
    gameBtn_->setEnabled(!b);
    bootBtn_->setEnabled(!b);
    restoreBtn_->setEnabled(!b && active_);
    bootClear_->setEnabled(!b && !boot_.isEmpty());
    if (autoBtn_) autoBtn_->setEnabled(!b && !autoRunning_);
}

void OptimizeTab::showStatus(const QString &msg, const char *color, int ms) {
    status_->setText(msg);
    status_->setStyleSheet(QStringLiteral("color:%1; background:transparent;").arg(color ? color : theme::FG_DIM));
    if (ms > 0) statusTimer_->start(ms); else statusTimer_->stop();
}
