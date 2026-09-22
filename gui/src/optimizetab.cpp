#include "optimizetab.h"
#include "privileged.h"
#include "theme.h"

#include <QApplication>
#include <QCheckBox>
#include <QClipboard>
#include <QComboBox>
#include <QDir>
#include <QFile>
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
#include <QStandardPaths>
#include <QTabWidget>
#include <QTimer>
#include <QVBoxLayout>
#include <algorithm>
#include <climits>

static constexpr int POLL_MS = 4000, DESCRIBE_TIMEOUT_MS = 8000, PKEXEC_TIMEOUT_MS = 120000;
static constexpr qint64 MAX_PRESET_BYTES = 256 * 1024;
static const char *GROUPS[] = {"CPU", "Memory", "Scheduler", "Storage", "Devices", "Stability"};
static const QString GAMEMODE = QStringLiteral("/usr/bin/lpm-gamemode");

static QString helperPath() { return privileged::helperPath(QStringLiteral("tune-helper")); }

// ── built-in presets ────────────────────────────────────────────────────────
// Templates only: values a machine does not offer are skipped on load. Keep
// the names valid for lpm-gamemode (letters, digits, space, _ - .).

struct Builtin { const char *name, *summary, *json; };
static const Builtin BUILTINS[] = {
    {"Gaming X3D",
     "Full lutris-game-tune set plus X3D placement: game on the V-Cache CCD, IRQs and kernel work on the other one.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"performance","cpu.epp_boost":"1",
        "cpu.boost":"1","cpu.min_freq":"lowest_nonlinear","cpu.x3d_mode":"cache",
        "thp.enabled":"madvise","thp.shmem_enabled":"advise","thp.defrag":"defer+madvise","thp.khugepaged_defrag":0,
        "mm.lru_gen":7,"mm.lru_gen_min_ttl":1000,"mm.ksm_run":0,"vm.max_map_count":2147483642,
        "vm.swappiness":10,"vm.compaction_proactiveness":5,"vm.watermark_boost_factor":15000,
        "vm.watermark_scale_factor":50,"vm.min_free_kbytes":262144,"vm.zone_reclaim_mode":0,
        "vm.page_lock_unfairness":1,"vm.stat_interval":10,"vm.page_cluster":0,
        "kernel.split_lock_mitigate":0,"kernel.watchdog":0,"kernel.numa_balancing":0,
        "kernel.sched_autogroup":1,"kernel.cfs_bandwidth_slice_us":3000,
        "sched.preempt":"full","sched.base_slice_ns":1000000,"sched.migration_cost_ns":500000,"sched.nr_migrate":32,
        "wq.power_efficient":"0","wq.cpumask":"frequency","irq.affinity":"frequency",
        "blk.scheduler":"none","pci.aspm":"performance","pci.latency_timer":"tuned",
        "snd.hda_power_save":0,"snd.hda_power_save_controller":"0","usb.autosuspend":-1,"gpu.amdgpu_dpm":"low"},
      "run":{"nice":-5,"autogroup":true,"affinity":"cache"}})"},
    {"Competitive",
     "Gaming X3D taken to the limit: frequency CCD parked, deep C-states off. Maximum determinism, most heat.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"performance","cpu.boost":"1",
        "cpu.min_freq":"lowest_nonlinear","cpu.x3d_mode":"cache","cpu.cstate_max":"1",
        "thp.enabled":"madvise","thp.defrag":"defer+madvise","thp.khugepaged_defrag":0,"mm.lru_gen_min_ttl":1000,
        "mm.ksm_run":0,"vm.max_map_count":2147483642,"vm.swappiness":10,"vm.stat_interval":10,"vm.page_cluster":0,
        "kernel.split_lock_mitigate":0,"kernel.watchdog":0,"kernel.numa_balancing":0,"kernel.timer_migration":0,
        "sched.preempt":"full","sched.base_slice_ns":1000000,"wq.power_efficient":"0",
        "blk.scheduler":"none","pci.aspm":"performance","snd.hda_power_save":0,"usb.autosuspend":-1,
        "gpu.amdgpu_dpm":"low","cpu.ccd_park":"frequency"},
      "run":{"nice":-10,"autogroup":true,"affinity":"cache"}})"},
    {"Low latency desktop",
     "Everyday responsiveness without the power cost: efficient floor clock, full preemption, no compaction stalls.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"balance_performance",
        "cpu.min_freq":"lowest_nonlinear","thp.enabled":"madvise","thp.defrag":"defer+madvise",
        "mm.lru_gen":7,"mm.lru_gen_min_ttl":1000,"vm.max_map_count":2147483642,"vm.page_cluster":0,
        "kernel.split_lock_mitigate":0,"sched.preempt":"full","wq.power_efficient":"0","snd.hda_power_save":0},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    {"Compile throughput",
     "Long parallel builds (emerge, kernel): frequency CCD preferred, throughput preemption, bigger slices.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"balance_performance","cpu.boost":"1",
        "cpu.x3d_mode":"frequency","cpu.smt":"on","cpu.ccd_park":"none","cpu.cstate_max":"all",
        "thp.enabled":"always","thp.defrag":"madvise","vm.swappiness":60,
        "sched.preempt":"voluntary","sched.base_slice_ns":3000000,"sched.migration_cost_ns":500000,
        "wq.cpumask":"all","irq.affinity":"all","blk.scheduler":"mq-deadline"},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    {"CO validation",
     "For proving Curve Optimizer offsets: boost on, every idle state on (idle-to-boost transitions are where CO fails), MCE polled every 10 s.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"performance","cpu.boost":"1",
        "cpu.min_freq":"cpuinfo_min","cpu.cstate_max":"all","cpu.smt":"on","cpu.ccd_park":"none",
        "kernel.watchdog":1,"mce.check_interval":10},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
    {"Quiet battery",
     "Unplugged: power EPP, no turbo, lowest floor clock, aggressive device power saving.",
     R"({"values":{
        "cpu.pstate_status":"active","cpu.governor":"powersave","cpu.epp":"power","cpu.boost":"0",
        "cpu.min_freq":"cpuinfo_min","cpu.cstate_max":"all","pci.aspm":"powersupersave",
        "snd.hda_power_save":1,"snd.hda_power_save_controller":"1","usb.autosuspend":2,
        "wq.power_efficient":"1","kernel.watchdog":1,"gpu.amdgpu_dpm":"auto","vm.stat_interval":10},
      "run":{"nice":0,"autogroup":true,"affinity":"none"}})"},
};

static const Builtin *builtin(const QString &name) {
    for (const Builtin &b : BUILTINS) if (name == QLatin1String(b.name)) return &b;
    return nullptr;
}

static QJsonObject builtinObject(const Builtin &b) {
    QJsonObject o = QJsonDocument::fromJson(QByteArray(b.json)).object();
    o["summary"] = QString::fromUtf8(b.summary);
    return o;
}

/// Same rule as lpm-gamemode's valid_name().
static bool validPresetName(const QString &n) {
    static const QRegularExpression re(QStringLiteral(R"(^[\p{L}\p{N}][\p{L}\p{N} _.\-]{0,63}$)"));
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
    banner_->setStyleSheet("font-weight: 700; background: transparent;");
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
    g->addWidget(muted("Pins the game to one CCD. Pair it with \"Unbound workqueue CPUs\" / \"IRQ affinity\" on the other CCD."), 2, 2);
    g->setColumnStretch(2, 1);
    v->addWidget(runBox);

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
    return scroll;
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
}

// ── describe ────────────────────────────────────────────────────────────────

void OptimizeTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
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
    topology_ = d.value("topology").toObject();
    state_ = d.value("state").toObject();
    boot_ = d.value("boot").toObject();
    const QJsonArray rows = d.value("tunables").toArray();

    QStringList keys;
    for (const auto &v : rows) keys << v.toObject().value("key").toString();
    QStringList have;
    for (const Row &r : rows_) have << r.key;
    if (keys != have) {
        buildRows(rows);
    } else {
        for (int i = 0; i < rows.size(); ++i) updateRow(rows_[i], rows[i].toObject());
    }

    // Affinity choices follow the live topology.
    if (affinity_) {
        const QString keep = affinity_->currentData().toString();
        const QSignalBlocker b(affinity_);
        affinity_->clear();
        affinity_->addItem("none (scheduler decides)", "none");
        const QJsonArray ccds = topology_.value("ccds").toArray();
        auto ccdText = [&](int i) {
            for (const auto &c : ccds) if (c.toObject().value("index").toInt() == i)
                return QStringLiteral("CCD%1: %2").arg(i).arg(c.toObject().value("cpus").toString());
            return QStringLiteral("CCD%1").arg(i);
        };
        if (!topology_.value("cache_ccd").isNull()) affinity_->addItem("V-Cache CCD (" + ccdText(topology_.value("cache_ccd").toInt()) + ")", "cache");
        if (!topology_.value("frequency_ccd").isNull()) affinity_->addItem("frequency CCD (" + ccdText(topology_.value("frequency_ccd").toInt()) + ")", "frequency");
        if (ccds.size() > 1) for (const auto &c : ccds) {
            const int i = c.toObject().value("index").toInt();
            affinity_->addItem(ccdText(i), QStringLiteral("ccd%1").arg(i));
        }
        const int k = affinity_->findData(keep);
        affinity_->setCurrentIndex(k < 0 ? 0 : k);
    }
    if (topoLabel_) {
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
        if (lines.isEmpty()) lines << "No L3 topology found.";
        if (topology_.value("online").toString() != topology_.value("present").toString())
            lines << QStringLiteral("<span style='color:%1'>Online CPUs: %2 of %3 — a CCD is parked or SMT is off.</span>")
                         .arg(theme::WARN, topology_.value("online").toString(), topology_.value("present").toString());
        else if (topology_.value("ccds").toArray().size() < 2)
            lines << QStringLiteral("<span style='color:%1'>Single CCD: affinity, workqueue/IRQ steering and CCD parking do not apply.</span>").arg(theme::MUTED);
        topoLabel_->setText(lines.join("<br>"));
    }
    updateLaunchPreview();
    updateStateBanner();
    reloadPresets(presetCombo_->currentData().toString());
}

void OptimizeTab::buildRows(const QJsonArray &rows) {
    const QJsonObject values = collectValues();  // keep the user's edits across a rebuild
    const QJsonObject run = collectRun();
    const int tabIndex = groups_->currentIndex();
    while (groups_->count()) { QWidget *w = groups_->widget(0); groups_->removeTab(0); w->deleteLater(); }
    rows_.clear();
    nice_ = nullptr; affinity_ = nullptr; autogroup_ = nullptr; topoLabel_ = nullptr;
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
        grid->setVerticalSpacing(4);
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
                r.spin = new QSpinBox;
                r.spin->setMinimumWidth(170);
                r.spin->setAccelerated(true);
                r.spin->setKeyboardTracking(false);
                editor = r.spin;
                connect(r.spin, &QSpinBox::valueChanged, this, [this, key] {
                    if (Row *x = row(key)) { x->touched = true; x->include->setChecked(true); markRow(*x); }
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
            grid->addWidget(editor, line, 3, Qt::AlignLeft);
            grid->addWidget(r.revert, line, 4);
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
    r.cur->setStyleSheet(QStringLiteral("color:%1; background:transparent;").arg(r.available && !rootOnly ? theme::FG_DIM : theme::MUTED));
    r.cur->setToolTip(r.available ? QStringLiteral("%1 file(s)").arg(o.value("files").toInt()) : "Not present on this kernel/hardware");

    // Keep a user edit; otherwise follow the live value.
    const QString want = r.touched ? oldEditor : r.current;
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
        if (!setEditorValue(r, want)) setEditorValue(r, r.current);
    } else if (r.spin) {
        const QSignalBlocker b(r.spin);
        r.spin->setRange(int(std::clamp<qint64>(r.min, INT_MIN, INT_MAX)), int(std::clamp<qint64>(r.max, INT_MIN, INT_MAX)));
        if (!setEditorValue(r, want)) setEditorValue(r, r.current);
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

bool OptimizeTab::setEditorValue(Row &r, const QString &v) {
    if (r.spin) {
        bool ok = false;
        const qint64 n = v.toLongLong(&ok);
        if (!ok || n < r.min || n > r.max) return false;
        const QSignalBlocker b(r.spin);
        r.spin->setValue(int(n));
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
    r.name->setStyleSheet(QStringLiteral("color:%1; background:transparent;%2").arg(color, pending ? " font-weight:600;" : ""));
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
        bannerDetail_->setText(QStringLiteral("%1%2 setting(s), %3 file(s) with saved originals. Restoring writes them back.")
            .arg(preset.isEmpty() ? QString() : "Preset \"" + preset + "\" · ")
            .arg(state_.value("keys").toArray().size()).arg(state_.value("saved_files").toInt()));
    } else {
        banner_->setText("System at its original values");
        bannerDetail_->setText("Nothing changed by Legion Power Manager is in effect. Every change you apply is recorded and reversible.");
    }
    active_ = state_.value("active").toBool();
    frame->setStyleSheet(QStringLiteral("#tuneBanner { background:%1; border:1px solid %2; border-left:3px solid %3; border-radius:%4px; }")
                             .arg(theme::BG1, theme::BORDER_SOFT, accent).arg(theme::RADIUS));
    banner_->setStyleSheet(QStringLiteral("font-weight:700; background:transparent; color:%1;").arg(accent));
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
    for (const Builtin &b : BUILTINS) out << QString::fromLatin1(b.name);
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

void OptimizeTab::reloadPresets(const QString &select) {
    const QString keep = select.isEmpty() ? presetCombo_->currentData().toString() : select;
    const QString game = gamePreset(), bootName = boot_.value("preset").toString();
    {
        const QSignalBlocker b(presetCombo_);
        presetCombo_->clear();
        for (const QString &n : presetNames()) {
            const bool user = QFile::exists(presetsDir() + '/' + n + ".json");
            QString label = (builtin(n) && !user ? QStringLiteral("◆ ") : QString()) + n;
            if (builtin(n) && user) label += "  (edited)";
            if (n == game) label += "  ★";
            if (!bootName.isEmpty() && n == bootName) label += "  ⏻";
            presetCombo_->addItem(label, n);
        }
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
        const QString v = it->isString() ? it->toString() : QString::number(it->toVariant().toLongLong());
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
        const int i = affinity_->findData(run.value("affinity").toString("none"));
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
    showStatus(QStringLiteral("Saved \"%1\" (%2 settings)").arg(name).arg(values.size()), theme::OK);
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
    showStatus(QStringLiteral("★ \"%1\" is now the game preset. Hook lpm-gamemode into Lutris/Steam (Game launch tab).").arg(name), theme::OK, 10000);
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
        const QString v = it->isString() ? it->toString() : QString::number(it->toVariant().toLongLong());
        if (r && validFor(*r, v)) values[it.key()] = *it;
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

void OptimizeTab::runOp(const QJsonObject &req, const QString &what, std::function<void(const QJsonObject &)> then) {
    if (busy_) return;
    setBusy(true);
    showStatus(what + "…", theme::MUTED, 0);
    privileged::run(helperPath(), req, this, [this, what, then](const privileged::Result &r) {
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
}

void OptimizeTab::showStatus(const QString &msg, const char *color, int ms) {
    status_->setText(msg);
    status_->setStyleSheet(QStringLiteral("color:%1; background:transparent;").arg(color ? color : theme::FG_DIM));
    if (ms > 0) statusTimer_->start(ms); else statusTimer_->stop();
}
