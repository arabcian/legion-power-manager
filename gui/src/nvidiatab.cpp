#include "nvidiatab.h"
#include "privileged.h"
#include "theme.h"
#include "vfcurvewidget.h"

#include <QComboBox>
#include <QDateTime>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QSpinBox>
#include <QTimer>
#include <QVBoxLayout>
#include <cmath>
#include <dlfcn.h>

static const QString PROFILES_DIR = QStringLiteral("/etc/nvcurve/profiles");
static const QString CONFIG_PATH = QStringLiteral("/etc/nvcurve/config.json");
static const QString RUN_DIR = QStringLiteral("/run/nvcurve-gui");
static const QString SCRATCH = QStringLiteral("_live");
static const QString STAR = QStringLiteral(" ★");
static constexpr int MAX_OFFSET = 1000;  // driver's ±1000 MHz hard cap

static QString helperPath() { return privileged::helperPath(QStringLiteral("nvcurve-root-helper")); }
static int floorDiv(qint64 a, int b) { return int(std::floor(double(a) / b)); }  // Python //

// ── NVML (dlopen; opened only while the tab is visible) ─────────────────────
// Holding NVML open keeps /dev/nvidia* open, which keeps the dGPU out of
// runtime D3cold. The Python tab initialised NVML at startup and never let
// go; here it is shut down whenever the tab is hidden.
class Nvml {
public:
    static Nvml *open() {
        void *h = dlopen("libnvidia-ml.so.1", RTLD_NOW | RTLD_LOCAL);
        if (!h) return nullptr;
        auto *n = new Nvml(h);
        if (!n->ok_) { delete n; return nullptr; }
        return n;
    }
    ~Nvml() { if (shutdown_) shutdown_(); dlclose(lib_); }
    std::optional<unsigned> temp() { unsigned v; return temp_ && temp_(dev_, 0, &v) == 0 ? std::optional(v) : std::nullopt; }
    std::optional<unsigned> powerMw() { unsigned v; return power_ && power_(dev_, &v) == 0 ? std::optional(v) : std::nullopt; }
    std::optional<unsigned> clock(unsigned type) { unsigned v; return clock_ && clock_(dev_, type, &v) == 0 ? std::optional(v) : std::nullopt; }
private:
    explicit Nvml(void *h) : lib_(h) {
        auto sym = [&](const char *n) { return dlsym(lib_, n); };
        auto init = reinterpret_cast<int (*)()>(sym("nvmlInit_v2"));
        auto byIdx = reinterpret_cast<int (*)(unsigned, void **)>(sym("nvmlDeviceGetHandleByIndex_v2"));
        shutdown_ = reinterpret_cast<int (*)()>(sym("nvmlShutdown"));
        temp_ = reinterpret_cast<int (*)(void *, unsigned, unsigned *)>(sym("nvmlDeviceGetTemperature"));
        power_ = reinterpret_cast<int (*)(void *, unsigned *)>(sym("nvmlDeviceGetPowerUsage"));
        clock_ = reinterpret_cast<int (*)(void *, unsigned, unsigned *)>(sym("nvmlDeviceGetClockInfo"));
        if (!init || !byIdx || init() != 0) { shutdown_ = nullptr; return; }
        ok_ = byIdx(0, &dev_) == 0;
    }
    void *lib_, *dev_ = nullptr;
    bool ok_ = false;
    int (*shutdown_)() = nullptr;
    int (*temp_)(void *, unsigned, unsigned *) = nullptr;
    int (*power_)(void *, unsigned *) = nullptr;
    int (*clock_)(void *, unsigned, unsigned *) = nullptr;
};

// ── UI ──────────────────────────────────────────────────────────────────────

static QLabel *lbl(const QString &t, const char *color, bool bold = false) {
    auto *l = new QLabel(t);
    l->setStyleSheet(QStringLiteral("color:%1;%2").arg(QString::fromLatin1(color), bold ? " font-weight:600;" : ""));
    return l;
}

static QSpinBox *spin(int lo, int hi, const QString &suffix, int w) {
    auto *s = new QSpinBox;
    s->setRange(lo, hi);
    s->setSuffix(suffix);
    s->setFixedWidth(w);
    return s;
}

NvidiaTab::NvidiaTab(QWidget *parent) : QWidget(parent) {
    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(12, 8, 12, 8);
    root->setSpacing(5);

    // Status + profiles
    auto *status = new QGroupBox("GPU Status");
    auto *sv = new QVBoxLayout(status);
    auto *stats = new QHBoxLayout;
    temp_ = lbl("Temp: -- °C", theme::ACCENT, true);
    power_ = lbl("Power: -- W", theme::WARN, true);
    clock_ = lbl("Clock: -- MHz", theme::INFO, true);
    memClock_ = lbl("Mem Clock: -- MHz", theme::OK, true);
    for (QLabel *l : {temp_, power_, clock_, memClock_}) { stats->addWidget(l); stats->addSpacing(10); }
    stats->addStretch();
    sv->addLayout(stats);

    auto *prow = new QHBoxLayout;
    prow->addWidget(lbl("Profile:", theme::MUTED));
    auto *bSave = new QPushButton("Save As…");
    bSave->setObjectName("btnAccent");
    connect(bSave, &QPushButton::clicked, this, &NvidiaTab::saveProfileAs);
    prow->addWidget(bSave);
    profiles_ = new QComboBox;
    profiles_->setMinimumWidth(150);
    connect(profiles_, &QComboBox::activated, this, &NvidiaTab::onProfileSelected);
    prow->addWidget(profiles_, 1);
    auto *bDef = new QPushButton("★ Default");
    bDef->setToolTip("Toggle this profile as the boot-time auto-load profile");
    connect(bDef, &QPushButton::clicked, this, &NvidiaTab::toggleDefault);
    auto *bApplyProf = new QPushButton("Apply Profile");
    bApplyProf->setToolTip("Apply the selected saved profile to the GPU as-is");
    connect(bApplyProf, &QPushButton::clicked, this, [this] {
        const QString n = profiles_->currentText().remove(STAR);
        if (!n.isEmpty()) applyNamedProfile(n);
    });
    auto *bDel = new QPushButton("Delete");
    bDel->setObjectName("btnDanger");
    connect(bDel, &QPushButton::clicked, this, &NvidiaTab::deleteProfile);
    prow->addWidget(bApplyProf);
    prow->addWidget(bDef);
    prow->addWidget(bDel);
    sv->addLayout(prow);
    root->addWidget(status);

    // Point info
    auto *ih = new QHBoxLayout;
    selLabel_ = lbl("Selected: -", theme::WARN, true);
    voltLabel_ = lbl("Voltage: - mV", theme::INFO);
    freqLabel_ = lbl("Freq: - MHz", theme::OK);
    offLabel_ = lbl("Offset: - MHz", theme::ACCENT);
    for (QLabel *l : {selLabel_, voltLabel_, freqLabel_, offLabel_}) { l->setMinimumWidth(96); ih->addWidget(l); }
    pointSpin_ = spin(-MAX_OFFSET, MAX_OFFSET, " MHz", 92);
    pointSpin_->setEnabled(false);
    pointSpin_->setToolTip("Total offset for the selected point(s)");
    connect(pointSpin_, &QSpinBox::valueChanged, this, [this](int v) {
        for (int i : vf_->selection()) setOffsetClamped(i, v);
        curveModified_ = true;
        updateCoreOffsetUi();
        recompute();
    });
    ih->addWidget(pointSpin_);
    ih->addStretch();
    ih->addWidget(lbl("Flatten after index:", theme::MUTED));
    flattenSpin_ = spin(-1, 127, "", 64);
    flattenSpin_->setSpecialValueText(" ");
    flattenSpin_->setValue(-1);
    flattenSpin_->setToolTip("Every point above this index gets the same frequency as this point (press Enter)");
    connect(flattenSpin_, &QSpinBox::editingFinished, this, [this] {
        const int s = flattenSpin_->value();
        if (s < 0 || s >= base_.size()) return;
        const int target = int(base_[s].y()) + offsetOf(s);
        for (int i = s + 1; i < base_.size(); ++i) setOffsetClamped(i, target - int(base_[i].y()));
        curveModified_ = true;
        updateCoreOffsetUi();
        recompute();
        log(QStringLiteral("Flatten applied from index %1").arg(s));
    });
    ih->addWidget(flattenSpin_);

    // Curve
    auto *curve = new QGroupBox("V/F Curve");
    auto *cv = new QVBoxLayout(curve);
    cv->setSpacing(4);
    cv->addLayout(ih);
    vf_ = new VfCurveWidget;
    vf_->setToolTip("Click a point to select it, drag a selected point to move it.\n"
                    "Ctrl+click adds to the selection; Ctrl+A selects all, then drag empty space to shift the whole curve.\n"
                    "←/→ extend the selection, ↑/↓ nudge by 1 MHz (Shift: 15), Space toggles, Esc clears.");
    connect(vf_, &VfCurveWidget::selectionChanged, this, &NvidiaTab::onSelectionChanged);
    connect(vf_, &VfCurveWidget::pointEdited, this, [this](int i, int f) {
        if (i < 0 || i >= base_.size()) return;
        setOffsetClamped(i, f - int(base_[i].y()));
        curveModified_ = true;
        updateCoreOffsetUi();
        recompute();
    });
    connect(vf_, &VfCurveWidget::selectionShifted, this, [this](int d) {
        for (int i : vf_->selection()) setOffsetClamped(i, offsetOf(i) + d);
        curveModified_ = true;
        updateCoreOffsetUi();
        recompute();
    });
    cv->addWidget(vf_, 1);
    auto *gb = new QHBoxLayout;
    gb->addWidget(lbl("Click a point to select, drag to move · Ctrl+A then drag empty space shifts the curve", theme::MUTED));
    gb->addStretch();
    auto *bAll = new QPushButton("Select All");
    connect(bAll, &QPushButton::clicked, vf_, &VfCurveWidget::selectAll);
    auto *bRg = new QPushButton("Reset Graph");
    bRg->setToolTip("Discard unapplied edits and show the last curve read from the GPU");
    connect(bRg, &QPushButton::clicked, this, [this] { profiles_->setCurrentIndex(-1); resetGraphToLastRead(); });
    gb->addWidget(bAll);
    gb->addWidget(bRg);
    cv->addLayout(gb);
    root->addWidget(curve, 1);

    // Controls
    auto *ctl = new QGroupBox("Controls");
    auto *cvl = new QVBoxLayout(ctl);
    cvl->setSpacing(4);
    auto *ch = new QHBoxLayout;
    auto *ch2 = new QHBoxLayout;
    cvl->addLayout(ch);
    cvl->addLayout(ch2);
    ch->addWidget(lbl("Core:", theme::MUTED));
    coreSpin_ = spin(-500, 500, " MHz", 96);
    connect(coreSpin_, &QSpinBox::valueChanged, this, [this](int v) { coreOffset_ = v; recompute(); });
    ch->addWidget(coreSpin_);
    ch->addSpacing(8);
    ch->addWidget(lbl("Memory:", theme::MUTED));
    memSpin_ = spin(-3000, 3000, " MHz", 96);
    ch->addWidget(memSpin_);
    ch->addSpacing(8);
    ch->addWidget(lbl("VRAM lock:", theme::MUTED));
    lockMinSpin_ = spin(0, 20000, " MHz", 92);
    lockMinSpin_->setSpecialValueText("–");
    lockMinSpin_->setToolTip("Min memory clock (MHz). 0 → uses Max for both.");
    lockMaxSpin_ = spin(0, 20000, " MHz", 92);
    lockMaxSpin_->setSpecialValueText("–");
    lockMaxSpin_->setToolTip("Max memory clock (MHz) — the actual lock target.");
    ch->addWidget(lockMinSpin_);
    ch->addWidget(lbl("–", theme::MUTED));
    ch->addWidget(lockMaxSpin_);
    auto *bLock = new QPushButton("Lock"), *bUnlock = new QPushButton("Unlock");
    bLock->setToolTip("Lock VRAM clock to Min–Max now");
    bUnlock->setToolTip("Unlock VRAM clock");
    connect(bLock, &QPushButton::clicked, this, &NvidiaTab::vramLock);
    connect(bUnlock, &QPushButton::clicked, this, &NvidiaTab::vramUnlock);
    ch->addWidget(bLock);
    ch->addWidget(bUnlock);
    ch->addStretch();
    ch2->addStretch();
    readBtn_ = new QPushButton("Read Curve");
    connect(readBtn_, &QPushButton::clicked, this, &NvidiaTab::readCurve);
    auto *bApply = new QPushButton("Apply Offsets");
    bApply->setObjectName("btnAccent");
    connect(bApply, &QPushButton::clicked, this, &NvidiaTab::applyOffsets);
    resetBtn_ = new QPushButton("Reset Curve");
    resetBtn_->setObjectName("btnDanger");
    connect(resetBtn_, &QPushButton::clicked, this, &NvidiaTab::resetCurve);
    ch2->addWidget(readBtn_);
    ch2->addWidget(bApply);
    ch2->addWidget(resetBtn_);
    root->addWidget(ctl);
    actionButtons_ = {bSave, bDef, bApplyProf, bDel, bLock, bUnlock, readBtn_, bApply, resetBtn_};

    log_ = new QPlainTextEdit;
    log_->setObjectName("terminal");
    log_->setReadOnly(true);
    log_->setFixedHeight(52);
    log_->setMaximumBlockCount(1000);
    root->addWidget(log_);

    statsTimer_ = new QTimer(this);
    statsTimer_->setInterval(2000);
    connect(statsTimer_, &QTimer::timeout, this, &NvidiaTab::pollStats);

    refreshProfiles();
    // First read happens on first show (it may raise a polkit prompt; don't
    // do that while the app is starting hidden in the tray).
}

NvidiaTab::~NvidiaTab() { delete nvml_; }

void NvidiaTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    if (firstShow_) {
        firstShow_ = false;
        // Dev aid: LPM_NVCURVE_FAKE=/path/read.json renders a canned curve.
        if (const QString fake = qEnvironmentVariable("LPM_NVCURVE_FAKE"); !fake.isEmpty()) {
            QFile f(fake);
            if (f.open(QIODevice::ReadOnly)) takeGpuPoints(QJsonDocument::fromJson(f.readAll()).object().value("vf_curve").toArray(), true);
        } else {
            QTimer::singleShot(0, this, &NvidiaTab::readCurve);
        }
    }
    if (!nvml_) nvml_ = Nvml::open();
    pollStats();
    statsTimer_->start();
}

void NvidiaTab::hideEvent(QHideEvent *e) {
    QWidget::hideEvent(e);
    statsTimer_->stop();
    delete nvml_;  // release /dev/nvidia* so the dGPU can suspend
    nvml_ = nullptr;
}

void NvidiaTab::pollStats() {
    if (!nvml_) {
        for (QLabel *l : {temp_, power_, clock_, memClock_}) l->setToolTip("NVML (libnvidia-ml.so.1) not available");
        return;
    }
    if (auto t = nvml_->temp()) temp_->setText(QStringLiteral("Temp: %1 °C").arg(*t));
    if (auto p = nvml_->powerMw()) power_->setText(QStringLiteral("Power: %1 W").arg(*p / 1000.0, 0, 'f', 1));
    if (auto c = nvml_->clock(0)) clock_->setText(QStringLiteral("Clock: %1 MHz").arg(*c));
    if (auto m = nvml_->clock(2)) memClock_->setText(QStringLiteral("Mem Clock: %1 MHz").arg(*m));
}

void NvidiaTab::log(const QString &s) {
    log_->appendPlainText(QTime::currentTime().toString("[HH:mm:ss] ") + s);
}

// ── helper plumbing ─────────────────────────────────────────────────────────

void NvidiaTab::setBusy(bool b) {
    busy_ = b;
    for (QPushButton *p : std::as_const(actionButtons_)) p->setEnabled(!b);
}

void NvidiaTab::runHelper(const QJsonObject &payload, const QString &okMsg, const QString &failMsg, Done done) {
    if (busy_) { log("⏳ Another operation is still running — please wait."); return; }
    setBusy(true);
    privileged::run(helperPath(), payload, this, [=, this](const privileged::Result &r) {
        setBusy(false);
        if (r.ok()) {
            for (const QString &line : r.json.value("message").toString().split('\n', Qt::SkipEmptyParts)) log(line);
            if (!okMsg.isEmpty()) log("✔ " + okMsg);
        } else {
            log("✘ " + failMsg + ": " + r.message());
        }
        if (done) done(r.ok(), r.json);
    }, 120000);
}

/// Result files are written by root into /run/nvcurve-gui. Only accept one
/// written by *this* operation — the Python tab would happily re-read a
/// stale file from an earlier op if the helper's best-effort write failed.
bool NvidiaTab::loadCurveFile(const QString &file, qint64 notBeforeMs, QJsonArray *out) {
    const QString path = RUN_DIR + '/' + file;
    const QFileInfo fi(path);
    if (!fi.exists()) { log("❌ Result file not found: " + path); return false; }
    if (fi.lastModified().toMSecsSinceEpoch() + 1000 < notBeforeMs) { log("❌ Result file is stale: " + path); return false; }
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly) || f.size() > 1024 * 1024) { log("❌ Cannot read " + path); return false; }
    const QJsonObject o = QJsonDocument::fromJson(f.readAll()).object();
    if (!o.contains("vf_curve")) { log("❌ 'vf_curve' missing in " + file); return false; }
    *out = o.value("vf_curve").toArray();
    applyMemOffset(o);
    return true;
}

/// Mem spin ← NVML memory offset (effective MHz) reported next to the curve.
/// Absent (older helper, NVML unavailable) → the spin is left untouched.
void NvidiaTab::applyMemOffset(const QJsonObject &result) {
    const QJsonValue m = result.value("mem_offset_mhz");
    if (!m.isDouble()) return;
    const QSignalBlocker b(memSpin_);
    memSpin_->setValue(m.toInt());
}

/// GPU-domain points become the base curve. Memory-domain points are skipped:
/// their ClockBoostTable deltas are not the NVML memory offset (different unit,
/// driver-clamped), so they must never feed the mem spin — see applyMemOffset().
void NvidiaTab::takeGpuPoints(const QJsonArray &pts, bool keepOffsets) {
    QVector<QPointF> base;
    QHash<int, int> offs;
    for (const auto &v : pts) {
        const QJsonObject p = v.toObject();
        if (p.value("domain").toString() != "gpu") continue;
        const int off = floorDiv(qint64(p.value("freq_offset_kHz").toDouble()), 1000);
        const int cur = floorDiv(qint64(p.value("freq_kHz").toDouble()), 1000);
        const int mv = floorDiv(qint64(p.value("volt_uV").toDouble()), 1000);
        offs[base.size()] = off;
        base.append(QPointF(mv, cur - off));
    }
    if (base.isEmpty()) { log("❌ No GPU points found in vf_curve."); return; }
    base_ = base;
    readOffsets_ = keepOffsets ? offs : QHash<int, int>();
    vf_->setPoints({});  // force re-fit for the new base
    resetGraphToLastRead();
    vf_->fitAxes();
}

// ── model ───────────────────────────────────────────────────────────────────

void NvidiaTab::setOffsetClamped(int i, int total) {
    // Offsets stay within the driver cap and never push a point below 0 MHz.
    // (The Python widget clamped *frequencies* to 800..3000, so dragging the
    // whole curve silently lifted idle points far below 800 MHz up to 800 —
    // a large positive offset nobody asked for.)
    total = std::clamp(total, -MAX_OFFSET, MAX_OFFSET);
    if (i < base_.size()) total = std::max(total, -int(base_[i].y()));
    pointOffsets_[i] = total - coreOffset_;
}

void NvidiaTab::recompute() {
    QVector<QPointF> pts;
    pts.reserve(base_.size());
    for (int i = 0; i < base_.size(); ++i) pts.append(QPointF(base_[i].x(), std::max(0, int(base_[i].y()) + offsetOf(i))));
    vf_->setPoints(pts);
}

void NvidiaTab::updateCoreOffsetUi() {
    // Once points diverge, a single "core offset" no longer describes the curve.
    coreSpin_->blockSignals(true);
    if (curveModified_) {
        coreSpin_->setEnabled(false);
        coreSpin_->setRange(-501, 500);
        coreSpin_->setSpecialValueText("⚠ curve");
        coreSpin_->setValue(-501);
        coreSpin_->setStyleSheet(QStringLiteral("QSpinBox { color:%1; font-weight:600; }").arg(theme::DANGER));
    } else {
        coreSpin_->setEnabled(true);
        coreSpin_->setSpecialValueText(QString());
        coreSpin_->setRange(-500, 500);
        coreSpin_->setValue(coreOffset_);
        coreSpin_->setStyleSheet(QString());
    }
    coreSpin_->blockSignals(false);
}

void NvidiaTab::onSelectionChanged() {
    const auto &sel = vf_->selection();
    pointSpin_->blockSignals(true);
    if (sel.isEmpty() || vf_->points().isEmpty()) {
        selLabel_->setText("Selected: -");
        voltLabel_->setText("Voltage: - mV");
        freqLabel_->setText("Freq: - MHz");
        offLabel_->setText("Offset: - MHz");
        pointSpin_->setEnabled(false);
        pointSpin_->setValue(0);
    } else {
        const int idx = sel.contains(vf_->current()) ? vf_->current() : *sel.begin();
        const QPointF p = vf_->points().value(idx);
        selLabel_->setText(QStringLiteral("Selected: %1 pt%2").arg(sel.size()).arg(sel.size() == 1 ? "" : "s"));
        voltLabel_->setText(QStringLiteral("Voltage: %1 mV").arg(p.x()));
        freqLabel_->setText(QStringLiteral("Freq: %1 MHz").arg(p.y()));
        offLabel_->setText(QStringLiteral("Offset: %1 MHz").arg(offsetOf(idx)));
        pointSpin_->setEnabled(true);
        pointSpin_->setValue(offsetOf(idx));
        flattenSpin_->blockSignals(true);
        flattenSpin_->setValue(*std::max_element(sel.begin(), sel.end()));
        flattenSpin_->blockSignals(false);
    }
    pointSpin_->blockSignals(false);
}

void NvidiaTab::resetGraphToLastRead() {
    // Uniform read offsets collapse into the core offset.
    QSet<int> uniq;
    for (int i = 0; i < base_.size(); ++i) uniq.insert(readOffsets_.value(i, 0));
    pointOffsets_.clear();
    if (uniq.size() <= 1) {
        coreOffset_ = uniq.isEmpty() ? 0 : *uniq.begin();
        curveModified_ = false;
    } else {
        coreOffset_ = 0;
        pointOffsets_ = readOffsets_;
        curveModified_ = true;
    }
    flattenSpin_->blockSignals(true);
    flattenSpin_->setValue(-1);
    flattenSpin_->blockSignals(false);
    updateCoreOffsetUi();
    vf_->clearSelection();
    recompute();
}

// ── actions ─────────────────────────────────────────────────────────────────

void NvidiaTab::readCurve() {
    readBtn_->setText("Reading…");
    const qint64 t0 = QDateTime::currentMSecsSinceEpoch();
    runHelper({{"op", "read_gpu_curve"}}, QString(), "Curve read failed", [this, t0](bool ok, const QJsonObject &) {
        readBtn_->setText("Read Curve");
        QJsonArray pts;
        if (!ok || !loadCurveFile("nvcurve_read.json", t0, &pts)) return;
        takeGpuPoints(pts, true);
        log(QStringLiteral("✅ V/F curve loaded (%1 GPU points, %2).")
                .arg(base_.size()).arg(curveModified_ ? QStringLiteral("per-point offsets")
                                                      : QStringLiteral("core offset %1 MHz").arg(coreOffset_)));
    });
}

void NvidiaTab::applyOffsets() {
    if (base_.isEmpty()) { log("❌ No curve loaded yet — read the current curve or load a profile first."); return; }
    QJsonObject deltas;
    for (int i = 0; i < base_.size(); ++i)
        if (const int o = offsetOf(i)) deltas.insert(QString::number(i), qint64(o) * 1000);
    const int mem = memSpin_->value(), lmax = lockMaxSpin_->value();
    const int lmin = lockMinSpin_->value() ? lockMinSpin_->value() : lmax;
    if (deltas.isEmpty() && mem == 0 && lmax == 0) { log("No offsets to apply."); return; }
    if (lmax && lmin > lmax) { log("⚠️ VRAM lock: Min cannot be greater than Max."); return; }

    const QJsonObject data{{"name", SCRATCH}, {"curve_deltas", deltas},
                           {"mem_offset_mhz", mem ? QJsonValue(mem) : QJsonValue()}, {"power_limit_w", QJsonValue()},
                           {"mem_locked_min_mhz", lmax ? QJsonValue(lmin) : QJsonValue()},
                           {"mem_locked_max_mhz", lmax ? QJsonValue(lmax) : QJsonValue()}};
    const qint64 t0 = QDateTime::currentMSecsSinceEpoch();
    runHelper({{"op", "apply_gpu_offsets"}, {"profile_name", SCRATCH}, {"profile_data", data}},
              "Offsets applied.", "Apply failed", [this, t0](bool ok, const QJsonObject &) {
        QJsonArray pts;
        if (!ok || !loadCurveFile("nvcurve_apply_result.json", t0, &pts)) return;
        takeGpuPoints(pts, true);
        log("✅ Curve re-read from the GPU after apply.");
    });
}

void NvidiaTab::resetCurve() {
    resetBtn_->setText("Resetting…");
    const qint64 t0 = QDateTime::currentMSecsSinceEpoch();
    runHelper({{"op", "reset_gpu_curve"}}, "Reset successful.", "Reset failed", [this, t0](bool ok, const QJsonObject &) {
        resetBtn_->setText("Reset Curve");
        profiles_->setCurrentIndex(-1);
        if (!ok) return;
        memSpin_->setValue(0);
        lockMinSpin_->setValue(0);
        lockMaxSpin_->setValue(0);
        QJsonArray pts;
        if (loadCurveFile("nvcurve_reset_result.json", t0, &pts)) takeGpuPoints(pts, false);
        else { readOffsets_.clear(); resetGraphToLastRead(); }
    });
}

void NvidiaTab::vramLock() {
    const int mx = lockMaxSpin_->value(), mn = lockMinSpin_->value() ? lockMinSpin_->value() : mx;
    if (!mx) { log("⚠️ Enter a Max MHz value first (Min defaults to Max if left at 0)."); return; }
    if (mn > mx) { log("⚠️ VRAM lock: Min cannot be greater than Max."); return; }
    runHelper({{"op", "set_vram_memlock"}, {"min_mhz", mn}, {"max_mhz", mx}}, QString(), "Failed to lock VRAM clock");
}

void NvidiaTab::vramUnlock() { runHelper({{"op", "reset_vram_memlock"}}, QString(), "Failed to unlock VRAM clock"); }

// ── profiles ────────────────────────────────────────────────────────────────

QStringList NvidiaTab::profileNames() const {
    QStringList out;
    for (QString f : QDir(PROFILES_DIR).entryList({"*.json"}, QDir::Files | QDir::Readable, QDir::Name)) {
        f.chop(5);
        if (f != SCRATCH) out << f;
    }
    return out;
}

QString NvidiaTab::defaultProfileName() const {
    QFile f(CONFIG_PATH);
    if (!f.open(QIODevice::ReadOnly) || f.size() > 256 * 1024) return {};
    const QJsonObject c = QJsonDocument::fromJson(f.readAll()).object();
    const QJsonObject m = c.value("auto_load_profiles").toObject();
    if (!m.isEmpty()) return m.begin().value().toString();
    return c.value("auto_load_profile").toString();
}

void NvidiaTab::refreshProfiles() {
    const QString cur = profiles_->currentText().remove(STAR), def = defaultProfileName();
    profiles_->blockSignals(true);
    profiles_->clear();
    for (const QString &n : profileNames()) profiles_->addItem(n == def ? n + STAR : n);
    int i = -1;
    for (int k = 0; k < profiles_->count(); ++k) if (profiles_->itemText(k).remove(STAR) == cur) i = k;
    profiles_->setCurrentIndex(i);
    profiles_->blockSignals(false);
    Q_EMIT profilesChanged();
}

void NvidiaTab::onProfileSelected() {
    const QString name = profiles_->currentText().remove(STAR);
    if (name.isEmpty()) return;
    QFile f(PROFILES_DIR + '/' + name + ".json");
    if (!f.open(QIODevice::ReadOnly) || f.size() > 256 * 1024) { log("❌ Cannot read profile: " + f.fileName()); return; }
    QJsonParseError pe;
    const QJsonDocument d = QJsonDocument::fromJson(f.readAll(), &pe);
    if (!d.isObject()) { log("❌ Profile is not valid JSON: " + pe.errorString()); return; }
    applyProfileToUi(d.object(), name);
}

void NvidiaTab::applyProfileToUi(const QJsonObject &data, const QString &name) {
    if (base_.isEmpty()) { log("⚠️ Read the curve first, then load a profile onto it."); return; }
    const QJsonObject deltas = data.value("curve_deltas").toObject();
    // Exactly what profiles::native loads and apply writes through NVML (incl. the legacy key).
    // Never derived from memory-domain curve_deltas: apply doesn't use them as the mem offset.
    const int mem = (data.contains("mem_offset_mhz") ? data.value("mem_offset_mhz")
                                                     : data.value("vram_p0_offset_mhz")).toInt();

    QHash<int, int> offs;
    QSet<int> uniq;
    for (int i = 0; i < base_.size(); ++i) {
        const int o = floorDiv(qint64(deltas.value(QString::number(i)).toDouble()), 1000);
        offs[i] = o;
        uniq.insert(o);
    }
    pointOffsets_.clear();
    if (uniq.size() <= 1) coreOffset_ = uniq.isEmpty() ? 0 : *uniq.begin();
    else { coreOffset_ = 0; pointOffsets_ = offs; }
    curveModified_ = uniq.size() > 1;

    memSpin_->setValue(mem);
    lockMinSpin_->setValue(data.value("mem_locked_min_mhz").toInt());
    lockMaxSpin_->setValue(data.value("mem_locked_max_mhz").toInt());
    flattenSpin_->setValue(-1);
    updateCoreOffsetUi();
    vf_->clearSelection();
    recompute();
    log(QStringLiteral("✅ Profile loaded into the editor: %1 (core %2, mem %3). Press Apply Offsets to write it.")
            .arg(name).arg(curveModified_ ? QStringLiteral("per-point") : QString::number(coreOffset_)).arg(mem));
}

void NvidiaTab::saveProfileAs() {
    bool ok = false;
    const QString name = QInputDialog::getText(this, "Profile Name", "Enter profile name:", QLineEdit::Normal,
                                               profiles_->currentText().remove(STAR), &ok).trimmed();
    if (!ok || name.isEmpty()) return;
    QJsonObject deltas;
    for (int i = 0; i < base_.size(); ++i)
        if (const int o = offsetOf(i)) deltas.insert(QString::number(i), qint64(o) * 1000);
    const int lmax = lockMaxSpin_->value(), lmin = lockMinSpin_->value() ? lockMinSpin_->value() : lmax;
    const QJsonObject data{{"name", name}, {"curve_deltas", deltas}, {"mem_offset_mhz", memSpin_->value()},
                           {"power_limit_w", QJsonValue()},
                           {"mem_locked_min_mhz", lmax ? QJsonValue(lmin) : QJsonValue()},
                           {"mem_locked_max_mhz", lmax ? QJsonValue(lmax) : QJsonValue()}};
    runHelper({{"op", "write_nvcurve_profile"}, {"name", name},
               {"content", QString::fromUtf8(QJsonDocument(data).toJson(QJsonDocument::Indented))}},
              "Profile '" + name + "' saved.", "Profile '" + name + "' could not be saved", [this, name](bool ok, const QJsonObject &) {
        if (!ok) return;
        refreshProfiles();
        for (int k = 0; k < profiles_->count(); ++k) if (profiles_->itemText(k).remove(STAR) == name) profiles_->setCurrentIndex(k);
    });
}

void NvidiaTab::toggleDefault() {
    const QString name = profiles_->currentText().remove(STAR);
    if (name.isEmpty()) { log("❌ No profile selected."); return; }
    const bool clear = defaultProfileName() == name;
    const QJsonObject payload = clear ? QJsonObject{{"op", "set_default_gpu_profile"}, {"clear", true}}
                                      : QJsonObject{{"op", "set_default_gpu_profile"}, {"name", name}};
    runHelper(payload, QString(), "Could not change the default profile", [this](bool, const QJsonObject &) { refreshProfiles(); });
}

void NvidiaTab::deleteProfile() {
    const QString name = profiles_->currentText().remove(STAR);
    if (name.isEmpty()) return;
    if (QMessageBox::question(this, "Delete Profile", "Delete profile '" + name + "'? This cannot be undone.",
                              QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes)
        return;
    runHelper({{"op", "delete_nvcurve_profile"}, {"name", name}}, QString(), "Could not delete '" + name + "'",
              [this](bool, const QJsonObject &) { refreshProfiles(); });
}

void NvidiaTab::applyNamedProfile(const QString &name) {
    const qint64 t0 = QDateTime::currentMSecsSinceEpoch();
    runHelper({{"op", "apply_named_profile"}, {"name", name}}, QString(), "Could not apply '" + name + "'",
              [this, t0](bool ok, const QJsonObject &) {
        QJsonArray pts;
        if (ok && loadCurveFile("nvcurve_apply_result.json", t0, &pts)) takeGpuPoints(pts, true);
    });
}
