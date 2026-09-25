#include "scenestab.h"
#include "bundle.h"
#include "fwattrtab.h"
#include "hometab.h"
#include "inteltab.h"
#include "mainwindow.h"
#include "nvidiatab.h"
#include "optimizetab.h"
#include "ryzentab.h"
#include "theme.h"
#include <QCheckBox>
#include <QDate>
#include <QDir>
#include <QFileDialog>
#include <QComboBox>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QLabel>
#include <QLineEdit>
#include <QListWidget>
#include <QMessageBox>
#include <QPushButton>
#include <QShowEvent>
#include <QTimer>
#include <QVBoxLayout>

using scenes::Choice;
using scenes::Scene;

// Combo item data: "u" unchanged, "r" reset, "p:<name>" profile.
static QString encode(const Choice &c) {
    return c.kind == Choice::Reset ? QStringLiteral("r") : c.kind == Choice::Profile ? "p:" + c.name : QStringLiteral("u");
}
static Choice decode(const QString &d) {
    if (d == QLatin1String("r")) return {Choice::Reset, {}};
    if (d.startsWith(QLatin1String("p:"))) return {Choice::Profile, d.mid(2)};
    return {};
}

/// Fills a component combo, keeping `keep` selectable even when its profile
/// has since been deleted — shown as missing instead of silently dropped.
static void fillChoice(QComboBox *c, const QStringList &names, const QString &resetLabel, const Choice &keep) {
    c->clear();
    c->addItem(QStringLiteral("Unchanged"), QStringLiteral("u"));
    if (!resetLabel.isEmpty()) c->addItem(resetLabel, QStringLiteral("r"));
    for (const QString &n : names) c->addItem(n, "p:" + n);
    if (keep.kind == Choice::Profile && !names.contains(keep.name)) c->addItem(keep.name + QStringLiteral("  (missing)"), "p:" + keep.name);
    c->setCurrentIndex(std::max(0, c->findData(encode(keep))));
}

static QLabel *muted(const QString &t) {
    auto *l = new QLabel(t);
    l->setProperty("role", "muted");
    return l;
}

ScenesTab::ScenesTab(MainWindow *win) : win_(win), eng_(win->scenes()) {
    auto *root = new QHBoxLayout(this);
    root->setContentsMargins(12, 10, 12, 10);
    root->setSpacing(10);

    // ── left: scene list ──
    auto *left = new QVBoxLayout;
    left->setSpacing(6);
    auto *listBox = new QGroupBox("Scenes");
    auto *ll = new QVBoxLayout(listBox);
    list_ = new QListWidget;
    list_->setFrameShape(QFrame::NoFrame);
    list_->setStyleSheet(QStringLiteral("QListWidget { background: transparent; outline: none; }"
                                        "QListWidget::item { padding: 4px 6px; border-radius: 5px; color: %1; }"
                                        "QListWidget::item:selected { background: %2; color: %3; }"
                                        "QListWidget::item:hover:!selected { background: %4; }")
                             .arg(theme::FG_DIM, theme::BG3, theme::FG, theme::BG2));
    ll->addWidget(list_, 1);
    auto *add = new QPushButton("New scene…");
    ll->addWidget(add);
    auto *io = new QHBoxLayout;
    io->setSpacing(4);
    auto *exp = new QPushButton("Export…");
    auto *imp = new QPushButton("Import…");
    exp->setToolTip("Save scenes, Optimizations presets, game-launch settings and every CPU / GPU curve profile to one file");
    imp->setToolTip("Load a file made with Export… (backup, reinstall, another machine of the same model)");
    for (QPushButton *b : {exp, imp}) { b->setObjectName("btnMini"); io->addWidget(b); }
    ll->addLayout(io);
    connect(exp, &QPushButton::clicked, this, [this] {
        const QString def = QDir::homePath() + "/legion-power-manager-" + QDate::currentDate().toString(Qt::ISODate) + ".lpm.json";
        const QString path = QFileDialog::getSaveFileName(this, "Export settings", def, "Legion Power Manager bundle (*.lpm.json *.json)");
        if (path.isEmpty()) return;
        QString err;
        const QString r = bundle::exportTo(path, &err);
        setStatus(r.isEmpty() ? "Export failed: " + err : r + "  →  " + path, r.isEmpty() ? theme::DANGER : theme::OK);
    });
    connect(imp, &QPushButton::clicked, this, [this] {
        const QString path = QFileDialog::getOpenFileName(this, "Import settings", QDir::homePath(), "Legion Power Manager bundle (*.lpm.json *.json)");
        if (path.isEmpty()) return;
        bundle::importFrom(path, this, [this](const QString &r) {
            if (r.isEmpty()) return;  // cancelled
            setStatus(r, r.startsWith("Import failed") ? theme::DANGER : theme::OK);
            loaded_ = {};
            reloadList();
            reloadAuto();
        });
    });
    dup_ = new QPushButton("Copy…");
    dup_->setToolTip("Duplicate this scene under a new name");
    del_ = new QPushButton("Delete");
    del_->setObjectName("btnDanger");
    left->addWidget(listBox, 1);
    auto *leftHost = new QWidget;
    leftHost->setLayout(left);
    leftHost->setFixedWidth(190);
    root->addWidget(leftHost);

    // ── right: editor + automatic switching ──
    auto *right = new QVBoxLayout;
    right->setSpacing(6);

    editor_ = new QGroupBox("Scene");
    auto *g = new QGridLayout(editor_);
    g->setHorizontalSpacing(10);
    g->setVerticalSpacing(6);
    g->setColumnStretch(1, 1);
    int row = 0;
    auto addRow = [&](const QString &label, QWidget *w, const QString &tip) {
        auto *l = muted(label);
        l->setToolTip(tip);
        w->setToolTip(tip);
        g->addWidget(l, row, 0);
        g->addWidget(w, row++, 1, 1, 3);
    };
    profile_ = new QComboBox;
    addRow("Power profile", profile_, "Platform profile set first. Firmware limits below only take effect in Custom.");

    fwLabel_ = new QLabel;
    fwLabel_->setWordWrap(true);
    fwCapture_ = new QPushButton("Capture current");
    fwCapture_->setObjectName("btnMini");
    fwCapture_->setToolTip("Store the firmware attributes as they are right now (set them in Firmware Attributes first).");
    fwClear_ = new QPushButton("Clear");
    fwClear_->setObjectName("btnMini");
    g->addWidget(muted("Firmware limits"), row, 0);
    g->addWidget(fwLabel_, row, 1);
    g->addWidget(fwCapture_, row, 2);
    g->addWidget(fwClear_, row++, 3);

    if (win_->ryzen() || win_->intel()) {
        cpu_ = new QComboBox;
        addRow(win_->ryzen() ? "CPU curve (Ryzen)" : "CPU undervolt (Intel)", cpu_,
               "A profile saved in the CPU tab. Reset sets every offset back to 0.");
    }
    gpu_ = new QComboBox;
    addRow("GPU curve (NVIDIA)", gpu_, "A profile saved in the NVIDIA tab. Reset clears the curve offsets.");
    tuning_ = new QComboBox;
    addRow("Optimizations", tuning_,
           "An Optimizations preset. Switching presets restores every knob the new one does not set, "
           "so nothing from the previous scene lingers. A running game's tuning is never touched.");
    command_ = new QLineEdit;
    command_->setPlaceholderText("optional, e.g.  kscreen-doctor output.eDP-1.mode.2560x1600@60");
    addRow("Run command", command_, "Runs as you, without a shell, after everything else — for things this app "
                                    "does not manage itself (display refresh rate, audio profile…).");

    auto *eb = new QHBoxLayout;
    eb->addWidget(dup_);
    eb->addWidget(del_);
    eb->addStretch(1);
    save_ = new QPushButton("Save");
    apply_ = new QPushButton("Apply");
    apply_->setObjectName("btnAccent");
    apply_->setToolTip("Saves the scene if changed, then applies it");
    eb->addWidget(save_);
    eb->addWidget(apply_);
    g->addLayout(eb, row++, 0, 1, 4);
    right->addWidget(editor_);

    empty_ = muted("No scenes yet. A scene bundles a power profile, firmware limits, CPU and GPU curves and an "
                   "Optimizations preset under one name — create one with New…");
    empty_->setWordWrap(true);
    right->addWidget(empty_);

    auto *autoBox = new QGroupBox("Automatic switching");
    auto *ag = new QGridLayout(autoBox);
    ag->setHorizontalSpacing(10);
    ag->setVerticalSpacing(6);
    ag->setColumnStretch(1, 1);
    ag->setColumnStretch(3, 1);
    auto_ = new QCheckBox("Switch scenes with the power source");
    auto_->setToolTip("Applies the chosen scene when the charger is plugged in or pulled (and at login). "
                      "A charger that stays connected under heavy load counts as AC even if the battery dips.");
    onAc_ = new QComboBox;
    onBattery_ = new QComboBox;
    ag->addWidget(auto_, 0, 0, 1, 4);
    ag->addWidget(muted("On AC"), 1, 0);
    ag->addWidget(onAc_, 1, 1);
    ag->addWidget(muted("On battery"), 1, 2);
    ag->addWidget(onBattery_, 1, 3);
    power_ = muted(QString());
    ag->addWidget(power_, 2, 0, 1, 4);
    right->addWidget(autoBox);

    status_ = new QLabel;
    status_->setWordWrap(true);
    right->addWidget(status_);
    right->addStretch(1);
    root->addLayout(right, 1);

    // ── wiring ──
    connect(list_, &QListWidget::currentItemChanged, this, [this](QListWidgetItem *it) {
        if (!filling_ && it) select(it->data(Qt::UserRole).toString());
    });
    connect(add, &QPushButton::clicked, this, [this] { newScene(false); });
    connect(dup_, &QPushButton::clicked, this, [this] { newScene(true); });
    connect(del_, &QPushButton::clicked, this, &ScenesTab::deleteScene);
    connect(save_, &QPushButton::clicked, this, [this] { if (saveCurrent()) setStatus("Saved.", theme::OK); });
    connect(apply_, &QPushButton::clicked, this, &ScenesTab::applyCurrent);
    connect(fwCapture_, &QPushButton::clicked, this, &ScenesTab::captureFirmware);
    connect(fwClear_, &QPushButton::clicked, this, [this] { firmware_.clear(); updateFirmwareLabel(); updateDirty(); });
    for (QComboBox *c : {profile_, cpu_, gpu_, tuning_})
        if (c) connect(c, &QComboBox::currentIndexChanged, this, [this] { if (!filling_) updateDirty(); });
    connect(command_, &QLineEdit::textChanged, this, [this] { if (!filling_) updateDirty(); });
    connect(auto_, &QCheckBox::toggled, this, [this] { if (!filling_) storeAuto(); });
    for (QComboBox *c : {onAc_, onBattery_})
        connect(c, &QComboBox::currentIndexChanged, this, [this] { if (!filling_) storeAuto(); });

    connect(eng_, &SceneEngine::started, this, [this](const QString &n) {
        apply_->setEnabled(false);
        setStatus("Applying '" + n + "'…");
    });
    connect(eng_, &SceneEngine::finished, this, [this](const QString &n, bool ok, const QStringList &log) {
        if (n.isEmpty()) { setStatus(log.join(' '), theme::FG_DIM); return; }  // informational (deferred switch)
        apply_->setEnabled(list_->count() > 0);
        setStatus((ok ? "'" + n + "' applied — " : "'" + n + "' applied with problems — ") + log.join(QStringLiteral("  ·  ")),
                  ok ? theme::OK : theme::WARN);
        reloadList(loaded_.name);  // marks the active scene
        updatePowerLabel();
    });
    connect(eng_, &SceneEngine::powerSourceChanged, this, &ScenesTab::updatePowerLabel);

    fillOptions();
    reloadAuto();
    reloadList();
    updatePowerLabel();
}

void ScenesTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    // Profiles may have been saved or deleted in other tabs meanwhile.
    const Scene cur = fromEditor();
    const Scene saved = loaded_;
    fillOptions();
    setEditor(cur);
    loaded_ = saved;
    updateDirty();
    reloadAuto();
    updatePowerLabel();
}

void ScenesTab::fillOptions() {
    filling_ = true;
    const QString keepProfile = profile_->currentData().toString();
    profile_->clear();
    profile_->addItem("Unchanged", QString());
    for (const QString &p : win_->home()->offeredProfiles()) profile_->addItem(HomeTab::profileLabel(p), p);
    profile_->setCurrentIndex(std::max(0, profile_->findData(keepProfile)));
    // Component combos are (re)filled by setEditor() with the scene's own choice kept.
    filling_ = false;
}

void ScenesTab::setEditor(const Scene &s) {
    filling_ = true;
    int pi = profile_->findData(s.platformProfile);
    if (pi < 0 && !s.platformProfile.isEmpty()) {
        profile_->addItem(HomeTab::profileLabel(s.platformProfile) + "  (not offered)", s.platformProfile);
        pi = profile_->count() - 1;
    }
    profile_->setCurrentIndex(std::max(0, pi));
    if (cpu_) {
        const QStringList names = win_->ryzen() ? win_->ryzen()->savedProfileNames() : win_->intel()->savedProfileNames();
        fillChoice(cpu_, names, win_->ryzen() ? "Reset (0 offset)" : "Reset (0 mV)", s.cpu);
    }
    fillChoice(gpu_, win_->nvidia()->profileNames(), "Reset curve", s.gpu);
    fillChoice(tuning_, win_->optimize()->presetNames(), "Restore originals", s.tuning);
    command_->setText(s.command);
    firmware_ = s.firmware;
    editor_->setTitle(s.name.isEmpty() ? QStringLiteral("Scene") : s.name);
    filling_ = false;
    loaded_ = s;
    updateFirmwareLabel();
    updateDirty();
}

Scene ScenesTab::fromEditor() const {
    Scene s;
    s.name = loaded_.name;
    s.platformProfile = profile_->currentData().toString();
    s.firmware = firmware_;
    if (cpu_) s.cpu = decode(cpu_->currentData().toString());
    s.gpu = decode(gpu_->currentData().toString());
    s.tuning = decode(tuning_->currentData().toString());
    s.command = command_->text().trimmed();
    return s;
}

void ScenesTab::reloadList(const QString &selectName) {
    filling_ = true;
    const QString want = selectName.isEmpty() ? loaded_.name : selectName;
    list_->clear();
    const QStringList names = scenes::names();
    const auto a = eng_->autoConfig();
    for (const QString &n : names) {
        QStringList tags;
        if (a.enabled && n == a.onAc) tags << QStringLiteral("AC");
        if (a.enabled && n == a.onBattery) tags << QStringLiteral("battery");
        auto *it = new QListWidgetItem(tags.isEmpty() ? n : n + QStringLiteral("  ·  ") + tags.join('/'));
        it->setData(Qt::UserRole, n);
        if (n == eng_->active()) { QFont f = it->font(); f.setBold(true); it->setFont(f); it->setToolTip("Active"); }
        list_->addItem(it);
    }
    filling_ = false;
    const bool any = !names.isEmpty();
    editor_->setVisible(any);
    empty_->setVisible(!any);
    dup_->setEnabled(any);
    del_->setEnabled(any);
    if (!any) { loaded_ = {}; return; }
    const int i = names.indexOf(want);
    list_->setCurrentRow(i < 0 ? 0 : i);  // fires select(); a no-op for the scene already loaded
}

void ScenesTab::select(const QString &name) {
    if (name.isEmpty() || name == loaded_.name) return;
    if (fromEditor() != loaded_ && !loaded_.name.isEmpty()) {
        const auto r = QMessageBox::question(this, "Unsaved changes", "Save the changes to '" + loaded_.name + "' first?",
                                             QMessageBox::Save | QMessageBox::Discard | QMessageBox::Cancel);
        if (r == QMessageBox::Cancel) {
            // Put the selection back — deferred: we are inside the list's own signal.
            QTimer::singleShot(0, this, [this] { reloadList(loaded_.name); });
            return;
        }
        if (r == QMessageBox::Save && !saveCurrent()) return;
    }
    if (auto s = scenes::load(name)) setEditor(*s);
}

void ScenesTab::updateDirty() {
    const bool dirty = !loaded_.name.isEmpty() && fromEditor() != loaded_;
    save_->setEnabled(dirty);
    editor_->setTitle(loaded_.name + (dirty ? QStringLiteral("  •") : QString()));
    // Firmware limits need Custom: say so where the choice is made.
    if (!firmware_.isEmpty() && profile_->currentData().toString() != QLatin1String("custom"))
        setStatus("Firmware limits only apply in the Custom power profile — pick Custom above or they will be skipped.", theme::WARN);
}

void ScenesTab::updateFirmwareLabel() {
    fwClear_->setEnabled(!firmware_.isEmpty());
    if (firmware_.isEmpty()) {
        fwLabel_->setText(QStringLiteral("<span style='color:%1'>Unchanged</span>").arg(theme::MUTED));
        fwLabel_->setToolTip(QString());
        return;
    }
    QStringList all, head;
    for (auto it = firmware_.cbegin(); it != firmware_.cend(); ++it) all << it.key() + " = " + QString::number(it.value());
    // The three numbers people actually compare scenes by.
    for (const auto &[key, shortName] : {std::pair{"ppt_pl1_spl", "PL1"}, {"ppt_pl2_sppt", "PL2"}, {"gpu_nv_ctgp", "cTGP"}})
        if (const auto it = firmware_.constFind(QLatin1String(key)); it != firmware_.cend())
            head << QStringLiteral("%1 %2 W").arg(QLatin1String(shortName)).arg(*it);
    fwLabel_->setText(QStringLiteral("%1 value(s)%2").arg(firmware_.size()).arg(head.isEmpty() ? QString() : " — " + head.join(", ")));
    fwLabel_->setToolTip(all.join('\n'));
}

void ScenesTab::captureFirmware() {
    QMap<QString, int> fw;
    bool hasWmi = false;
    for (const FwAttr &a : FwattrTab::discover()) {
        if (a.viaWmi()) { hasWmi = true; continue; }
        if (a.ranged) fw.insert(a.name, a.current);
    }
    const QMap<QString, int> wmi = win_->fwattr()->wmiValues();
    for (auto it = wmi.cbegin(); it != wmi.cend(); ++it) fw.insert(it.key(), it.value());
    if (fw.isEmpty()) { setStatus("This machine exposes no writable firmware attributes.", theme::WARN); return; }
    firmware_ = fw;
    QString note = QStringLiteral("Captured %1 firmware value(s).").arg(fw.size());
    if (hasWmi && wmi.isEmpty()) note += " The GPU cTGP/boost values were not read yet — open Firmware Attributes once and capture again to include them.";
    if (profile_->currentData().toString() != QLatin1String("custom") && profile_->findData(QStringLiteral("custom")) >= 0) {
        profile_->setCurrentIndex(profile_->findData(QStringLiteral("custom")));
        note += " Power profile set to Custom (required for these).";
    }
    updateFirmwareLabel();
    updateDirty();
    setStatus(note, theme::OK);
}

bool ScenesTab::saveCurrent() {
    const Scene s = fromEditor();
    QString err;
    const QJsonObject values = s.tuning.kind == Choice::Profile
        ? win_->optimize()->presetObject(s.tuning.name).value("values").toObject() : QJsonObject();
    if (!scenes::save(s, &err, values)) { setStatus("Could not save: " + err, theme::DANGER); return false; }
    loaded_ = s;
    updateDirty();
    return true;
}

void ScenesTab::applyCurrent() {
    if (loaded_.name.isEmpty()) return;
    if (fromEditor() != loaded_ && !saveCurrent()) return;
    eng_->apply(loaded_.name);
}

void ScenesTab::newScene(bool duplicate) {
    bool ok = false;
    const QString title = duplicate ? "Duplicate scene" : "New scene";
    const QString name = QInputDialog::getText(this, title, "Scene name:", QLineEdit::Normal,
                                               duplicate ? loaded_.name + " copy" : QString(), &ok).trimmed();
    if (!ok || name.isEmpty()) return;
    if (!scenes::validName(name)) {
        QMessageBox::warning(this, title, "Use 1-64 characters: letters, digits, space, underscore or hyphen, starting with a letter or digit.");
        return;
    }
    if (scenes::names().contains(name)) { QMessageBox::warning(this, title, "A scene named '" + name + "' already exists."); return; }
    Scene s = duplicate ? fromEditor() : Scene{};
    s.name = name;
    QString err;
    const QJsonObject values = s.tuning.kind == Choice::Profile
        ? win_->optimize()->presetObject(s.tuning.name).value("values").toObject() : QJsonObject();
    if (!scenes::save(s, &err, values)) { setStatus("Could not save: " + err, theme::DANGER); return; }
    loaded_ = {};  // no unsaved-changes prompt for the scene we are leaving (duplicate carries them)
    reloadList(name);
    setStatus(duplicate ? "Duplicated as '" + name + "'." : "Created '" + name + "'. Pick what it should set, then Save.", theme::OK);
}

void ScenesTab::deleteScene() {
    const QString n = loaded_.name;
    if (n.isEmpty() || QMessageBox::question(this, "Delete scene", "Delete scene '" + n + "'?\n\nThe profiles it refers to are kept.",
                                             QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return;
    scenes::remove(n);
    auto a = eng_->autoConfig();
    if (a.onAc == n || a.onBattery == n) {
        if (a.onAc == n) a.onAc.clear();
        if (a.onBattery == n) a.onBattery.clear();
        eng_->setAuto(a);
    }
    loaded_ = {};
    reloadList();
    reloadAuto();
    setStatus("Deleted '" + n + "'.");
}

void ScenesTab::reloadAuto() {
    filling_ = true;
    const auto a = eng_->autoConfig();
    const QStringList names = scenes::names();
    for (auto [c, sel] : {std::pair{onAc_, a.onAc}, std::pair{onBattery_, a.onBattery}}) {
        c->clear();
        c->addItem(QStringLiteral("— leave as is"), QString());
        for (const QString &n : names) c->addItem(n, n);
        c->setCurrentIndex(std::max(0, c->findData(sel)));
    }
    auto_->setChecked(a.enabled);
    onAc_->setEnabled(!names.isEmpty());
    onBattery_->setEnabled(!names.isEmpty());
    filling_ = false;
}

void ScenesTab::storeAuto() {
    scenes::Auto a{auto_->isChecked(), onAc_->currentData().toString(), onBattery_->currentData().toString()};
    QString err;
    if (!eng_->setAuto(a, &err)) { setStatus("Could not save the automatic switching setting: " + err, theme::DANGER); return; }
    if (a.enabled && a.onAc.isEmpty() && a.onBattery.isEmpty()) setStatus("Pick a scene for AC and/or battery.", theme::WARN);
    reloadList(loaded_.name);
    updatePowerLabel();
}

void ScenesTab::updatePowerLabel() {
    const auto ac = eng_->powerSource();
    QString t = !ac ? QStringLiteral("Power source: unknown (no charger or battery reported)")
                    : *ac ? QStringLiteral("Now on AC power") : QStringLiteral("Now on battery");
    if (!eng_->active().isEmpty()) t += "  ·  active scene: " + eng_->active();
    power_->setText(t);
}

void ScenesTab::setStatus(const QString &msg, const char *color) {
    status_->setText(msg);
    status_->setStyleSheet(QStringLiteral("color:%1; background:transparent;").arg(color ? color : theme::FG_DIM));
}
