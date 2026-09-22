#include "ryzentab.h"
#include "platformprofile.h"
#include "privileged.h"
#include "theme.h"

#include <QButtonGroup>
#include <QCheckBox>
#include <QComboBox>
#include <QDateTime>
#include <QDir>
#include <QFile>
#include <QFrame>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QIntValidator>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QRadioButton>
#include <QRegularExpression>
#include <QSaveFile>
#include <QScrollArea>
#include <QSet>
#include <QStandardPaths>
#include <QGridLayout>
#include <QVBoxLayout>
#include <climits>

static constexpr int SLOTS_PER_CCD = 8, CCD_FALLBACK = 2;
static constexpr int CO_MIN = -50, CO_MAX = 20;
static constexpr int PKEXEC_TIMEOUT_MS = 300000;  // covers reading/typing the polkit prompt

static QString helperPath() { return privileged::helperPath(QStringLiteral("ryzen-co-helper")); }

// ── topology ────────────────────────────────────────────────────────────────

static QList<int> expandCpuList(const QString &s) {
    QList<int> out;
    for (const QString &part : s.split(',', Qt::SkipEmptyParts)) {
        const QStringList lh = part.trimmed().split('-');
        bool a = false, b = true;
        int lo = lh.value(0).toInt(&a), hi = lh.size() > 1 ? lh[1].toInt(&b) : lo;
        if (a && b && hi >= lo && hi - lo < 4096) for (int i = lo; i <= hi; ++i) out << i;
    }
    return out;
}

static std::optional<QString> l3Shared(const QString &cpuDir) {
    const QDir cache(cpuDir + "/cache");
    for (const QString &idx : cache.entryList({"index*"}, QDir::Dirs, QDir::Name))
        if (pp::readText(cache.filePath(idx) + "/level") == QStringLiteral("3"))
            return pp::readText(cache.filePath(idx) + "/shared_cpu_list");
    return std::nullopt;
}

ryzen::Layout ryzen::detect() {
    Layout out;
    const QDir sys(QStringLiteral("/sys/devices/system/cpu"));
    QSet<QString> keys;
    for (const QString &c : sys.entryList({"cpu[0-9]*"}, QDir::Dirs))
        if (auto s = l3Shared(sys.filePath(c))) keys.insert(*s);
    QList<QList<int>> groups;
    for (const QString &k : keys) {
        QList<int> g = expandCpuList(k);
        std::sort(g.begin(), g.end());
        g.erase(std::unique(g.begin(), g.end()), g.end());
        if (!g.isEmpty()) groups << g;
    }
    std::sort(groups.begin(), groups.end(), [](auto &a, auto &b) { return a.first() < b.first(); });
    out.ccdCount = groups.size();
    for (int ccd = 0; ccd < groups.size(); ++ccd) {
        QStringList order;
        QHash<QString, QList<int>> phys;
        for (int cpu : groups[ccd]) {
            const QString t = sys.filePath(QStringLiteral("cpu%1/topology/").arg(cpu));
            QString sib = pp::readText(t + "core_cpus_list").value_or(
                pp::readText(t + "thread_siblings_list").value_or(QString::number(cpu)));
            if (!phys.contains(sib)) order << sib;
            phys[sib] << cpu;
        }
        for (const QString &sib : order) {
            QList<int> cpus = phys.value(sib);
            std::sort(cpus.begin(), cpus.end());
            bool ok = false;
            int hp = pp::readText(sys.filePath(QStringLiteral("cpu%1/acpi_cppc/highest_perf").arg(cpus.first())))
                         .value_or(QString()).toInt(&ok);
            out.cores[ccd].append({cpus, ok ? std::optional(hp) : std::nullopt});
        }
    }
    return out;
}

QString ryzen::profilesDir() {
    // Same location as the standalone applet so existing profiles carry over.
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) +
           QStringLiteral("/ryzen-curve-optimizer/profiles");
}

// ── UI ──────────────────────────────────────────────────────────────────────

static QGroupBox *box(const QString &title, const char *objName) {
    auto *b = new QGroupBox(title);
    b->setObjectName(QString::fromLatin1(objName));
    return b;
}

RyzenTab::RyzenTab(QWidget *parent) : QWidget(parent), layout_(ryzen::detect()) {
    ccdCount_ = layout_.ccdCount ? layout_.ccdCount : CCD_FALLBACK;
    if (qEnvironmentVariableIsSet("LPM_RYZEN_CCDS")) ccdCount_ = qEnvironmentVariableIntValue("LPM_RYZEN_CCDS");  // dev only
    profilesReady_ = QDir().mkpath(ryzen::profilesDir());

    QList<int> mismatch;
    for (int c = 0; c < ccdCount_; ++c)
        if (layout_.cores.contains(c) && layout_.cores[c].size() != SLOTS_PER_CCD) mismatch << c;

    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(12, 10, 12, 10);
    root->setSpacing(6);

    if (!mismatch.isEmpty()) {
        QStringList d;
        for (int c : mismatch) d << QStringLiteral("CCD%1: %2/%3 cores").arg(c).arg(layout_.cores[c].size()).arg(SLOTS_PER_CCD);
        auto *b = box("Notice", "box_yellow");
        auto *l = new QVBoxLayout(b);
        auto *t = new QLabel("Detected a partially-populated CCD (" + d.join(", ") + "). On chips like this, the SMU's 8 fixed "
            "core slots per CCD and the OS's core numbering do not necessarily line up 1:1 — some slots may not correspond "
            "to a real core at all, and this cannot be auto-detected. Check Disable for any slot with no real core behind it.");
        t->setWordWrap(true);
        l->addWidget(t);
        root->addWidget(b);
    }

    // Layout (top → bottom):
    //   [ Profile ............ Load Save Del ]            [ Reset Curve Optimizer ]
    //   All-Core:  [ value ]                                         [Apply All-Core]
    //   Per-Core:  mode radios                               [Clear] [Apply Per-Core]
    //              ┌ CCD0 card ┐ ┌ CCD1 card ┐   (one aligned grid per CCD)
    // Reset clears *both* all-core and per-core offsets (--set-coall=0), so it
    // lives in the global row, not inside the all-core box. Both Apply buttons
    // share one width and the right edge, so they line up vertically.
    constexpr int APPLY_W = 132;

    auto *top = new QHBoxLayout;
    top->setSpacing(8);
    auto *profBox = box("Profile", "box_purple");
    auto *pl = new QHBoxLayout(profBox);
    profileCombo_ = new QComboBox;
    profileCombo_->setMinimumWidth(160);
    auto *bLoad = new QPushButton("Load"), *bSave = new QPushButton("Save"), *bDel = new QPushButton("Delete");
    bDel->setObjectName("btnDanger");
    connect(bLoad, &QPushButton::clicked, this, [this] { loadProfile(); });
    connect(bSave, &QPushButton::clicked, this, &RyzenTab::saveProfile);
    connect(bDel, &QPushButton::clicked, this, &RyzenTab::deleteProfile);
    pl->addWidget(profileCombo_, 1);
    pl->addWidget(bLoad);
    pl->addWidget(bSave);
    pl->addWidget(bDel);
    top->addWidget(profBox, 1);

    auto *resetBox = box("Global", "box_grey");
    auto *rl = new QHBoxLayout(resetBox);
    auto *bReset = new QPushButton("Reset Curve Optimizer");
    bReset->setObjectName("btnDanger");
    bReset->setToolTip("Send --set-coall=0: clears the all-core offset AND every per-core offset.");
    connect(bReset, &QPushButton::clicked, this, [this] {
        if (QMessageBox::question(this, "Reset Curve Optimizer",
                "Clear every Curve Optimizer offset (all-core and per-core) back to 0?",
                QMessageBox::Yes | QMessageBox::No, QMessageBox::No) == QMessageBox::Yes)
            applyReset();
    });
    rl->addWidget(bReset);
    top->addWidget(resetBox);
    root->addLayout(top);

    // All-core
    auto *acBox = box("All-Core Offset  (--set-coall)", "box_yellow");
    auto *al = new QHBoxLayout(acBox);
    al->addWidget(new QLabel("Offset:"));
    coall_ = new QLineEdit;
    coall_->setPlaceholderText(QStringLiteral("%1 … %2").arg(CO_MIN).arg(CO_MAX));
    coall_->setFixedWidth(96);
    coall_->setAlignment(Qt::AlignCenter);
    coall_->setValidator(new QIntValidator(CO_MIN, CO_MAX, coall_));
    connect(coall_, &QLineEdit::returnPressed, this, [this] { applyAllCore(); });
    al->addWidget(coall_);
    auto *acHint = new QLabel("Same offset on every core. Negative = undervolt.");
    acHint->setProperty("role", "muted");
    al->addWidget(acHint);
    al->addStretch();
    auto *bAll = new QPushButton("Apply All-Core");
    bAll->setObjectName("btnAccent");
    bAll->setFixedWidth(APPLY_W);
    connect(bAll, &QPushButton::clicked, this, [this] { applyAllCore(); });
    al->addWidget(bAll);
    root->addWidget(acBox);

    // Per-core
    auto *coreBox = box("Per-Core Offsets  (fixed SMU slot layout)", "box_green");
    auto *cl = new QVBoxLayout(coreBox);
    cl->setSpacing(8);
    auto *hdr = new QHBoxLayout;
    auto *modeBox = new QWidget;
    auto *ml = new QHBoxLayout(modeBox);
    ml->setContentsMargins(0, 0, 0, 0);
    auto *rPrimary = new QRadioButton(QStringLiteral("CCD0 only (%1 slots)").arg(SLOTS_PER_CCD));
    auto *rAll = new QRadioButton(QStringLiteral("All CCDs (%1 slots)").arg(ccdCount_ * SLOTS_PER_CCD));
    rAll->setChecked(true);
    auto *mg = new QButtonGroup(this);
    mg->addButton(rPrimary);
    mg->addButton(rAll);
    connect(rPrimary, &QRadioButton::toggled, this, [this](bool c) { if (c) setCoreMode(false); });
    connect(rAll, &QRadioButton::toggled, this, [this](bool c) { if (c) setCoreMode(true); });
    ml->addWidget(rPrimary);
    ml->addWidget(rAll);
    modeBox->setVisible(ccdCount_ >= 2);
    hdr->addWidget(modeBox);
    hdr->addStretch();
    auto *bClear = new QPushButton("Clear");
    bClear->setToolTip("Empty every per-core field and untick Disable (nothing is sent)");
    auto *bCores = new QPushButton("Apply Per-Core");
    bCores->setObjectName("btnAccent");
    bCores->setFixedWidth(APPLY_W);
    connect(bClear, &QPushButton::clicked, this, &RyzenTab::clearCores);
    connect(bCores, &QPushButton::clicked, this, [this] { applyPerCore(); });
    hdr->addWidget(bClear);
    hdr->addWidget(bCores);
    cl->addLayout(hdr);
    applyButtons_ = {bAll, bReset, bCores};

    const QString accents[] = {theme::OK, theme::ACCENT, theme::PURPLE, theme::INFO};
    auto *cards = new QHBoxLayout;
    cards->setSpacing(12);
    slots_.reserve(ccdCount_ * SLOTS_PER_CCD);
    for (int ccd = 0; ccd < ccdCount_; ++ccd) {
        const QString accent = accents[ccd % 4];
        const auto detected = layout_.cores.value(ccd);

        // Best cores on this CCD by CPPC (top two get a marker).
        QList<int> perf;
        for (const auto &c : detected) if (c.highestPerf) perf << *c.highestPerf;
        std::sort(perf.begin(), perf.end(), std::greater<>());
        const int bestCut = perf.size() >= 2 ? perf[1] : (perf.isEmpty() ? INT_MAX : perf[0]);

        auto *card = new QFrame;
        card->setObjectName("ccdCard");
        card->setStyleSheet(QStringLiteral("#ccdCard { background: %1; border: 1px solid %2; border-top: 2px solid %3; border-radius: 6px; }")
                                .arg(theme::BG2, theme::BORDER_SOFT, accent));
        auto *g = new QGridLayout(card);
        g->setContentsMargins(12, 8, 12, 10);
        g->setHorizontalSpacing(14);
        g->setVerticalSpacing(3);

        // Row 0: title + quick fill
        auto *title = new QLabel(QStringLiteral("CCD%1").arg(ccd));
        title->setStyleSheet(QStringLiteral("color:%1; font-weight:700; font-size:11pt; background:transparent;").arg(accent));
        auto *fillBar = new QHBoxLayout;
        fillBar->setSpacing(6);
        auto *fill = new QLineEdit;
        fill->setPlaceholderText(QStringLiteral("%1 … %2").arg(CO_MIN).arg(CO_MAX));
        fill->setFixedWidth(84);
        fill->setAlignment(Qt::AlignCenter);
        fill->setValidator(new QIntValidator(CO_MIN, CO_MAX, fill));
        fill->setToolTip(QStringLiteral("Fill every enabled slot of CCD%1 with this offset").arg(ccd));
        auto *fb = new QPushButton("Fill");
        connect(fill, &QLineEdit::returnPressed, this, [this, ccd] { fillCcd(ccd); });
        connect(fb, &QPushButton::clicked, this, [this, ccd] { fillCcd(ccd); });
        fillEntries_[ccd] = fill;
        fillBar->addStretch();
        fillBar->addWidget(fill);
        fillBar->addWidget(fb);
        g->addWidget(title, 0, 0, 1, 2, Qt::AlignLeft | Qt::AlignVCenter);
        g->addLayout(fillBar, 0, 2, 1, 2);

        // Row 1: column headers — same grid, so they line up with the cells.
        const char *heads[] = {"Slot", "Offset", "Disable", "CPPC"};
        for (int c = 0; c < 4; ++c) {
            auto *h = new QLabel(QString::fromLatin1(heads[c]));
            h->setProperty("role", "muted");
            h->setAlignment(Qt::AlignCenter);
            g->addWidget(h, 1, c);
        }
        auto *line = new QFrame;
        line->setFrameShape(QFrame::HLine);
        line->setStyleSheet(QStringLiteral("color:%1;").arg(theme::BORDER_SOFT));
        g->addWidget(line, 2, 0, 1, 4);

        for (int s = 0; s < SLOTS_PER_CCD; ++s) {
            const ryzen::PhysCore *pc = s < detected.size() ? &detected[s] : nullptr;
            QStringList cpus;
            if (pc) for (int c : pc->cpus) cpus << QString::number(c);
            const QString cpuHint = pc ? QStringLiteral("OS CPU%1 %2").arg(cpus.size() == 1 ? "" : "s", cpus.join(", "))
                                       : QStringLiteral("OS CPU mapping unknown (topology mismatch)");
            const int r = 3 + s;

            auto *id = new QLabel(QStringLiteral("S%1").arg(s));
            id->setStyleSheet(QStringLiteral("color:%1; font-weight:600; background:transparent;").arg(accent));
            id->setToolTip(QStringLiteral("Fixed SMU slot %1 of CCD%2 (hardware addressing, not the OS core id)\n").arg(s).arg(ccd) + cpuHint);
            g->addWidget(id, r, 0, Qt::AlignCenter);

            auto *e = new QLineEdit;
            e->setPlaceholderText("0");
            e->setFixedWidth(72);
            e->setAlignment(Qt::AlignCenter);
            e->setValidator(new QIntValidator(CO_MIN, CO_MAX, e));
            e->setToolTip(QStringLiteral("Curve Optimizer offset for this slot (%1..%2)").arg(CO_MIN).arg(CO_MAX));
            g->addWidget(e, r, 1, Qt::AlignCenter);

            auto *d = new QCheckBox;
            d->setToolTip("Disable this SMU slot.\nTick it if your CPU has no physical core here (e.g. a 6-core CCD uses "
                          "only 6 of the 8 SMU slots). Disabled slots are skipped entirely.");
            connect(d, &QCheckBox::toggled, e, [e](bool on) { e->setEnabled(!on); if (on) e->clear(); });
            g->addWidget(d, r, 2, Qt::AlignCenter);

            const bool hasHp = pc && pc->highestPerf;
            const bool best = hasHp && *pc->highestPerf >= bestCut;
            auto *cppc = new QLabel(hasHp ? QString::number(*pc->highestPerf) + (best ? QStringLiteral(" ★") : QString())
                                          : QStringLiteral("–"));
            cppc->setMinimumWidth(52);
            cppc->setAlignment(Qt::AlignCenter);
            cppc->setStyleSheet(QStringLiteral("color:%1;%2 background:transparent;")
                                    .arg(hasHp ? (best ? theme::WARN : theme::FG_DIM) : theme::MUTED, best ? " font-weight:700;" : ""));
            cppc->setToolTip(QStringLiteral("CPPC highest_perf for this physical core") +
                             (hasHp ? ": " + QString::number(*pc->highestPerf) : QStringLiteral(" (not reported on this system)")) +
                             "\nHigher = a better core on this die; ★ marks the two best on this CCD.\n" + cpuHint);
            g->addWidget(cppc, r, 3, Qt::AlignCenter);

            slots_.append({ccd, s, e, d, id});
        }
        // Spare width is shared evenly, so the four columns spread across the
        // card instead of bunching up on the left.
        for (int c = 0; c < 4; ++c) g->setColumnStretch(c, 1);
        g->setRowStretch(3 + SLOTS_PER_CCD, 1);
        ccdColumns_[ccd] = card;
        cards->addWidget(card, 1);
    }

    auto *gridHost = new QWidget;
    gridHost->setObjectName("ryzenGrid");
    gridHost->setStyleSheet("#ryzenGrid { background: transparent; }");
    gridHost->setLayout(cards);
    auto *scroll = new QScrollArea;
    scroll->setWidgetResizable(true);
    scroll->setFrameShape(QFrame::NoFrame);
    scroll->setWidget(gridHost);
    scroll->setMinimumHeight(std::min(gridHost->sizeHint().height(), 260));
    cl->addWidget(scroll, 1);
    root->addWidget(coreBox, 1);

    auto *logBox = box("Output / Log", "box_grey");
    auto *ll = new QVBoxLayout(logBox);
    log_ = new QPlainTextEdit;
    log_->setReadOnly(true);
    log_->setObjectName("terminal");
    log_->setMaximumBlockCount(2000);
    ll->addWidget(log_);
    log_->setMinimumHeight(54);
    log_->setMaximumHeight(90);
    root->addWidget(logBox, 0);

    reloadProfiles();
    for (int c = 0; c < ccdCount_; ++c) activeCcds_.insert(c);

    log(QStringLiteral("%1 CCD(s), %2 fixed SMU slots each (%3 total).").arg(ccdCount_).arg(SLOTS_PER_CCD).arg(ccdCount_ * SLOTS_PER_CCD));
    if (!layout_.ccdCount) log(QStringLiteral("CCD topology could not be read from sysfs — assuming %1 CCDs. Verify before applying.").arg(CCD_FALLBACK), "err");
    if (!mismatch.isEmpty()) log("Partially-populated CCD detected — see the notice above about SMU slot IDs vs. OS core IDs.", "cmd");
    bool cppc = false;
    for (const auto &l : std::as_const(layout_.cores)) for (const auto &c : l) cppc |= c.highestPerf.has_value();
    log(cppc ? "CPPC highest_perf read per core from sysfs — shown in the 'cppc' column."
             : "CPPC highest_perf not available from sysfs on this system — cppc column shows '–'.", cppc ? "info" : "cmd");
    if (!profilesReady_) log("Profile directory is not writable: " + ryzen::profilesDir(), "err");
}

void RyzenTab::log(const QString &msg, const QString &level) {
    static const QHash<QString, QPair<QString, QString>> style{
        {"info", {theme::FG_DIM, "  "}}, {"ok", {theme::OK, "OK"}}, {"err", {theme::DANGER, "!!"}}, {"cmd", {theme::WARN, ">>"}}};
    const auto [color, prefix] = style.value(level, style.value("info"));
    log_->appendHtml(QStringLiteral("<span style=\"color:%1;\">[%2]</span> <span style=\"color:%3;\">%4 %5</span>")
        .arg(theme::MUTED, QTime::currentTime().toString("HH:mm:ss"), color, prefix,
             msg.toHtmlEscaped().replace('\n', "<br>")));
}

std::pair<RyzenTab::Parse, int> RyzenTab::parse(const Slot &s) const {
    if (s.disable->isChecked()) return {Parse::Disabled, 0};
    const QString t = s.entry->text().trimmed();
    if (t.isEmpty()) return {Parse::Empty, 0};
    bool ok = false;
    const int v = t.toInt(&ok);
    if (!ok) return {Parse::Invalid, 0};
    if (v < CO_MIN || v > CO_MAX) return {Parse::Range, v};
    return {Parse::Ok, v};
}

QList<RyzenTab::Slot *> RyzenTab::activeSlots() {
    QList<Slot *> out;
    for (Slot &s : slots_) if (activeCcds_.contains(s.ccd)) out << &s;
    return out;
}

void RyzenTab::setCoreMode(bool all) {
    activeCcds_.clear();
    for (int c = 0; c < (all ? ccdCount_ : 1); ++c) activeCcds_.insert(c);
    for (auto it = ccdColumns_.cbegin(); it != ccdColumns_.cend(); ++it) {
        const bool vis = activeCcds_.contains(it.key());
        it.value()->setVisible(vis);
    }
    for (Slot &s : slots_) if (!activeCcds_.contains(s.ccd)) { s.disable->setChecked(false); s.entry->clear(); }
    log(all ? QStringLiteral("Core mode: All CCDs (%1 slots)").arg(ccdCount_ * SLOTS_PER_CCD)
            : QStringLiteral("Core mode: CCD0 Only (%1 slots)").arg(SLOTS_PER_CCD));
}

void RyzenTab::clearCores() {
    for (Slot *s : activeSlots()) { s->disable->setChecked(false); s->entry->clear(); }
    log("Per-core fields cleared.");
}

void RyzenTab::fillCcd(int ccd) {
    const QString t = fillEntries_.value(ccd)->text().trimmed();
    bool ok = false;
    const int v = t.toInt(&ok);
    if (t.isEmpty()) { QMessageBox::warning(this, "Missing Value", QStringLiteral("Enter an offset to fill CCD%1 with.").arg(ccd)); return; }
    if (!ok || v < CO_MIN || v > CO_MAX) {
        QMessageBox::warning(this, "Out of Range", QStringLiteral("Offset must be within %1..%2.").arg(CO_MIN).arg(CO_MAX));
        return;
    }
    int n = 0;
    for (Slot &s : slots_) if (s.ccd == ccd && !s.disable->isChecked()) { s.entry->setText(QString::number(v)); ++n; }
    if (n) log(QStringLiteral("CCD%1: filled %2 slot(s) with offset %3. Click 'Apply Per-Core' to send it.").arg(ccd).arg(n).arg(v));
    else log(QStringLiteral("CCD%1: every slot is disabled, nothing filled.").arg(ccd), "err");
}

// ── apply ───────────────────────────────────────────────────────────────────

void RyzenTab::setBusy(bool b) {
    busy_ = b;
    for (QPushButton *p : std::as_const(applyButtons_)) p->setEnabled(!b);
}

bool RyzenTab::applyAllCore(std::function<void()> then) {
    const QString t = coall_->text().trimmed();
    bool ok = false;
    const int v = t.toInt(&ok);
    if (t.isEmpty()) { QMessageBox::warning(this, "Missing Value", "Please enter an all-core offset value."); return false; }
    if (!ok || v < CO_MIN || v > CO_MAX) {
        QMessageBox::warning(this, "Out of Range", QStringLiteral("Offset must be within %1..%2.").arg(CO_MIN).arg(CO_MAX));
        return false;
    }
    runOp("set_coall", {{"value", v}}, std::move(then));
    return true;
}

void RyzenTab::applyReset() { runOp("reset", {}); }

bool RyzenTab::applyPerCore(std::function<void()> then) {
    QJsonArray entries;
    QStringList problems;
    int skipped = 0;
    for (Slot *s : activeSlots()) {
        const auto [st, v] = parse(*s);
        const QString label = QStringLiteral("CCD%1/S%2").arg(s->ccd).arg(s->slot);
        switch (st) {
        case Parse::Disabled: ++skipped; break;
        case Parse::Empty: break;
        case Parse::Invalid: problems << label + ": not a number ('" + s->entry->text().trimmed() + "')"; break;
        case Parse::Range: problems << QStringLiteral("%1: %2 is outside %3..%4").arg(label).arg(v).arg(CO_MIN).arg(CO_MAX); break;
        case Parse::Ok: entries.append(QJsonObject{{"ccd", s->ccd}, {"ccx", 0}, {"core", s->slot}, {"coper", v}}); break;
        }
    }
    if (!problems.isEmpty()) {
        QMessageBox::warning(this, "Invalid Values", "Nothing was applied. Fix these first:\n\n" + problems.join('\n'));
        for (const QString &p : problems) log(p, "err");
        return false;
    }
    if (entries.isEmpty()) { QMessageBox::information(this, "No Input", "No per-core values have been entered."); return false; }
    if (skipped) log(QStringLiteral("Skipping %1 disabled slot(s).").arg(skipped));
    runOp("set_coper_batch", {{"entries", entries}}, std::move(then));
    return true;
}

void RyzenTab::runOp(const QString &op, const QJsonObject &params, std::function<void()> then) {
    if (busy_) { log("Previous operation still running, please wait.", "err"); return; }
    log("Sending '" + op + "' via pkexec... (you may be prompted for a password)", "cmd");
    setBusy(true);
    privileged::run(helperPath(), QJsonObject{{"op", op}, {"params", params}}, this,
        [this, then](const privileged::Result &r) {
            setBusy(false);
            if (!r.reached) { log(r.error, "err"); return; }
            if (r.ok()) log("Operation applied successfully.", "ok");
            else log("Operation failed: " + (r.message().isEmpty() ? QStringLiteral("see per-core results below") : r.message()), "err");
            const QJsonArray results = r.json.value("results").toArray();
            for (const auto &v : results) {
                const QJsonObject o = v.toObject();
                log(QStringLiteral("CCD%1/S%2 -> offset %3 (%4)").arg(o.value("ccd").toInt()).arg(o.value("core").toInt())
                        .arg(o.value("coper").toInt()).arg(o.value("message").toString()),
                    o.value("ok").toBool() ? "ok" : "err");
            }
            if (results.isEmpty() && r.json.contains("message")) log(r.json.value("message").toString());
            if (r.ok() && then) then();
        }, PKEXEC_TIMEOUT_MS);
}

// ── profiles ────────────────────────────────────────────────────────────────

QStringList RyzenTab::savedProfileNames() const {
    QStringList n = QDir(ryzen::profilesDir()).entryList({"*.json"}, QDir::Files, QDir::Name);
    for (QString &s : n) s.chop(5);
    return n;
}

void RyzenTab::reloadProfiles() {
    const QString prev = profileCombo_->currentText();
    profileCombo_->clear();
    profileCombo_->addItems(savedProfileNames());
    if (int i = profileCombo_->findText(prev); i >= 0) profileCombo_->setCurrentIndex(i);
}

QJsonObject RyzenTab::currentState() {
    QJsonArray cores;
    for (Slot *s : activeSlots()) {
        const auto [st, v] = parse(*s);
        cores.append(QJsonObject{{"ccd", s->ccd}, {"ccx", 0}, {"slot", s->slot},
                                 {"coper", st == Parse::Ok ? QJsonValue(v) : QJsonValue()},
                                 {"disabled", s->disable->isChecked()}});
    }
    bool ok = false;
    const int coall = coall_->text().trimmed().toInt(&ok);
    return {{"version", "2.0.0"}, {"ccd_count", ccdCount_}, {"coall", ok ? QJsonValue(coall) : QJsonValue()}, {"cores", cores}};
}

void RyzenTab::saveProfile() {
    if (!profilesReady_) { QMessageBox::critical(this, "Save Error", "Profile directory is not writable:\n" + ryzen::profilesDir()); return; }
    bool ok = false;
    const QString name = QInputDialog::getText(this, "Save Profile", "Profile name:", QLineEdit::Normal, QString(), &ok).trimmed();
    if (!ok) return;
    static const QRegularExpression re(QStringLiteral("^[A-Za-z0-9][A-Za-z0-9 _-]{0,63}$"));
    if (!re.match(name).hasMatch()) {
        QMessageBox::warning(this, "Invalid Name", "Use 1-64 characters: letters, digits, space, underscore or hyphen, starting with a letter or digit.");
        return;
    }
    const QString path = ryzen::profilesDir() + '/' + name + ".json";
    if (QFile::exists(path) && QMessageBox::question(this, "Overwrite Profile", "Profile '" + name + "' already exists. Overwrite?",
                                                     QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return;
    QSaveFile f(path);
    if (!f.open(QIODevice::WriteOnly) || f.write(QJsonDocument(currentState()).toJson()) < 0 || !f.commit()) {
        QMessageBox::critical(this, "Save Error", f.errorString());
        return;
    }
    reloadProfiles();
    profileCombo_->setCurrentText(name);
    log("Profile saved: " + path);
}

bool RyzenTab::loadProfile() {
    const QString name = profileCombo_->currentText();
    if (name.isEmpty()) { QMessageBox::information(this, "No Profile", "No profile selected to load."); return false; }
    QFile f(ryzen::profilesDir() + '/' + name + ".json");
    if (!f.open(QIODevice::ReadOnly) || f.size() > 256 * 1024) { QMessageBox::critical(this, "Load Error", f.errorString()); return false; }
    QJsonParseError pe;
    const QJsonDocument doc = QJsonDocument::fromJson(f.readAll(), &pe);
    if (!doc.isObject()) { QMessageBox::critical(this, "Load Error", pe.error ? pe.errorString() : "Profile file is not a JSON object."); return false; }
    const QJsonObject d = doc.object();
    const QJsonValue coall = d.value("coall");
    coall_->setText(coall.isDouble() ? QString::number(coall.toInt()) : QString());
    for (Slot &s : slots_) { s.disable->setChecked(false); s.entry->clear(); }
    int missing = 0;
    for (const auto &v : d.value("cores").toArray()) {
        const QJsonObject c = v.toObject();
        const int ccd = c.value("ccd").toInt(), ccx = c.value("ccx").toInt(),
                  slot = c.contains("slot") ? c.value("slot").toInt() : c.value("core").toInt();
        Slot *hit = nullptr;
        for (Slot &s : slots_) if (ccx == 0 && s.ccd == ccd && s.slot == slot) hit = &s;
        if (!hit) { ++missing; continue; }
        const bool dis = c.value("disabled").toBool();
        hit->disable->setChecked(dis);
        if (!dis && c.value("coper").isDouble()) hit->entry->setText(QString::number(c.value("coper").toInt()));
    }
    log("Profile loaded: " + name);
    if (missing) log(QStringLiteral("%1 slot(s) in the profile do not exist on this topology (%2 CCD(s)) and were ignored.")
                         .arg(missing).arg(ccdCount_), "err");
    return true;
}

void RyzenTab::deleteProfile() {
    const QString name = profileCombo_->currentText();
    if (name.isEmpty() || QMessageBox::question(this, "Delete Profile", "Delete profile '" + name + "'?",
                                                QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return;
    if (!QFile::remove(ryzen::profilesDir() + '/' + name + ".json")) { QMessageBox::critical(this, "Delete Error", "Could not delete the profile."); return; }
    reloadProfiles();
    log("Profile deleted: " + name);
}

bool RyzenTab::applyNamedProfile(const QString &name) {
    if (busy_) { log("Previous operation still running, please wait.", "err"); return false; }
    const int i = profileCombo_->findText(name);
    if (i < 0) { reloadProfiles(); QMessageBox::warning(this, "No Profile", "Profile '" + name + "' no longer exists."); return false; }
    profileCombo_->setCurrentIndex(i);
    if (!loadProfile()) return false;

    bool anyCore = false;
    for (Slot *s : activeSlots()) anyCore |= parse(*s).first == Parse::Ok;
    const bool hasCoall = !coall_->text().trimmed().isEmpty();
    // Chained, not back-to-back: the Python version fired per-core while the
    // all-core pkexec call was still running, so it was rejected as busy
    // and a profile with both parts only ever got its all-core half applied.
    if (hasCoall) return applyAllCore(anyCore ? std::function<void()>([this] { applyPerCore(); }) : std::function<void()>());
    if (anyCore) return applyPerCore();
    log("Profile '" + name + "' contains no offsets to apply.", "err");
    return false;
}
