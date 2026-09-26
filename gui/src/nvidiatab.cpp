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
#include <QSlider>
#include <QSpinBox>
#include <QTimer>
#include <QVBoxLayout>
#include <array>
#include <cmath>
#include <dlfcn.h>

static const QString PROFILES_DIR = QStringLiteral("/etc/nvcurve/profiles");
static const QString CONFIG_PATH = QStringLiteral("/etc/nvcurve/config.json");
static const QString RUN_DIR = QStringLiteral("/run/nvcurve-gui");
static const QString SCRATCH = QStringLiteral("_live");
static const QString STAR = QStringLiteral(" ★");
// Driver limits: +1000 MHz up, -2000 MHz down (a flattened undervolt curve
// pulls its top points well past -1000; matches nvcurve MIN/MAX_DELTA_KHZ).
static constexpr int MAX_OFFSET = 1000, MIN_OFFSET = -2000;

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
    std::optional<unsigned> architecture() { unsigned v; return arch_ && arch_(dev_, &v) == 0 ? std::optional(v) : std::nullopt; }
    std::optional<unsigned long long> eventReasons() {
        unsigned long long v; return reasons_ && reasons_(dev_, &v) == 0 ? std::optional(v) : std::nullopt;
    }
    struct PowerMizer { unsigned current, mode, supported; };  // nvmlDevicePowerMizerModes_v1_t
    std::optional<PowerMizer> powerMizer() {
        PowerMizer m{}; return pm_ && pm_(dev_, &m) == 0 ? std::optional(m) : std::nullopt;
    }
private:
    explicit Nvml(void *h) : lib_(h) {
        auto sym = [&](const char *n) { return dlsym(lib_, n); };
        auto init = reinterpret_cast<int (*)()>(sym("nvmlInit_v2"));
        auto byIdx = reinterpret_cast<int (*)(unsigned, void **)>(sym("nvmlDeviceGetHandleByIndex_v2"));
        shutdown_ = reinterpret_cast<int (*)()>(sym("nvmlShutdown"));
        temp_ = reinterpret_cast<int (*)(void *, unsigned, unsigned *)>(sym("nvmlDeviceGetTemperature"));
        power_ = reinterpret_cast<int (*)(void *, unsigned *)>(sym("nvmlDeviceGetPowerUsage"));
        clock_ = reinterpret_cast<int (*)(void *, unsigned, unsigned *)>(sym("nvmlDeviceGetClockInfo"));
        arch_ = reinterpret_cast<int (*)(void *, unsigned *)>(sym("nvmlDeviceGetArchitecture"));
        reasons_ = reinterpret_cast<int (*)(void *, unsigned long long *)>(sym("nvmlDeviceGetCurrentClocksEventReasons"));
        if (!reasons_) reasons_ = reinterpret_cast<int (*)(void *, unsigned long long *)>(sym("nvmlDeviceGetCurrentClocksThrottleReasons"));
        pm_ = reinterpret_cast<int (*)(void *, PowerMizer *)>(sym("nvmlDeviceGetPowerMizerMode_v1"));
        if (!init || !byIdx || init() != 0) { shutdown_ = nullptr; return; }
        ok_ = byIdx(0, &dev_) == 0;
    }
    void *lib_, *dev_ = nullptr;
    bool ok_ = false;
    int (*shutdown_)() = nullptr;
    int (*temp_)(void *, unsigned, unsigned *) = nullptr;
    int (*power_)(void *, unsigned *) = nullptr;
    int (*clock_)(void *, unsigned, unsigned *) = nullptr;
    int (*arch_)(void *, unsigned *) = nullptr;
    int (*reasons_)(void *, unsigned long long *) = nullptr;
    int (*pm_)(void *, PowerMizer *) = nullptr;
};

// ── NvAPI temperatures (hotspot, VRAM) — in-process, only while visible ─────
// Same sources as `nvcurve sensors` (crates/nvcurve/src/hal/sensors.rs):
// thermal channel 9 = hotspot up to Ada, channel 15 = GDDR6/6X memory,
// channel 10 = GDDR7 memory; Blackwell's hotspot and per-partition GDDR7
// temperatures come from GPU registers. All values are range-checked.
class NvApiTemps {
public:
    static NvApiTemps *open() {
        void *h = dlopen("libnvidia-api.so.1", RTLD_NOW | RTLD_LOCAL);
        if (!h) return nullptr;
        auto *t = new NvApiTemps(h);
        if (!t->gpu_) { delete t; return nullptr; }
        return t;
    }
    ~NvApiTemps() {
        if (auto unload = fn<int (*)()>(0xD22BDD7E); unload && gpu_) unload();
        dlclose(lib_);
    }
    std::optional<int> hotspot(bool blackwell) { return blackwell ? regTemp(0x00AD0AA0) : channel(9); }
    std::optional<int> vram(bool blackwell) {
        if (!blackwell) return channel(15);
        if (auto p = partitionsMax()) return p;
        return channel(10);
    }
private:
    struct Therm { unsigned version; int mask; int values[40]; };
    struct RegOp { unsigned short flags, status; unsigned offset; unsigned long long writeMask, value; };
    struct RegOps { unsigned version, count; RegOp op[256]; };
    static_assert(sizeof(Therm) == 168 && sizeof(RegOp) == 24 && sizeof(RegOps) == 6152);

    template <typename F> F fn(unsigned id) { return qi_ ? reinterpret_cast<F>(qi_(id)) : nullptr; }
    explicit NvApiTemps(void *h) : lib_(h) {
        qi_ = reinterpret_cast<void *(*)(unsigned)>(dlsym(lib_, "nvapi_QueryInterface"));
        auto init = fn<int (*)()>(0x0150E828);
        auto enumGpus = fn<int (*)(void **, unsigned *)>(0xE5AC921F);
        if (!init || !enumGpus || init() != 0) return;
        void *gpus[64] = {};
        unsigned n = 0;
        if (enumGpus(gpus, &n) == 0 && n > 0) gpu_ = gpus[0];
        therm_ = fn<int (*)(void *, Therm *)>(0x65FE3AAD);
        reg_ = fn<int (*)(void *, RegOps *)>(0x2EB3C140);
    }
    bool thermRead(int mask, Therm &t) {
        t = Therm{};
        t.version = unsigned(sizeof(Therm)) | (2u << 16);
        t.mask = mask;
        return therm_ && therm_(gpu_, &t) == 0;
    }
    std::optional<int> channel(int i) {
        if (!therm_) return std::nullopt;
        if (mask_ == 0) {  // widest channel mask the driver accepts, probed once
            Therm t;
            if (!thermRead(1, t)) { therm_ = nullptr; return std::nullopt; }
            mask_ = 1;
            for (int b = 1; b < 31 && thermRead(mask_ | (1 << b), t); ++b) mask_ |= 1 << b;
        }
        Therm t;
        if (!thermRead(mask_, t)) return std::nullopt;
        const int c = t.values[i] / 256;
        return c > 0 && c < 255 ? std::optional(c) : std::nullopt;
    }
    std::optional<unsigned long long> reg(unsigned offset) {
        if (!reg_) return std::nullopt;
        auto *ops = new RegOps{};
        ops->version = unsigned(sizeof(RegOps)) | (1u << 16);
        ops->count = 1;
        ops->op[0].flags = 1 | 4 | 16;  // read, 32-bit, global
        ops->op[0].offset = offset;
        const bool ok = reg_(gpu_, ops) == 0 && ops->op[0].status == 0;
        const unsigned long long v = ops->op[0].value;
        delete ops;
        if (!ok) return std::nullopt;
        return v;
    }
    std::optional<int> regTemp(unsigned offset) {
        auto v = reg(offset);
        if (!v) return std::nullopt;
        const int c = int((*v & 0xFFFF) / 256);
        return c > 0 && c < 255 ? std::optional(c) : std::nullopt;
    }
    std::optional<int> partitionsMax() {
        std::optional<int> best;
        const bool clamshell = reg(0x00900200).value_or(0) >> 22 & 1;
        auto parse = [](unsigned long long v) { return 2 * int(std::min<unsigned long long>(v, 0x50)) - 40; };
        auto poisoned = [](unsigned long long v) { return (v & 0xFFFF0000ull) == 0xBADF0000ull; };
        for (unsigned p = 0; p <= 8; ++p) {
            auto st = reg(0x009024D0 + p * 0x4000);
            if (!st || poisoned(*st)) continue;
            for (const auto &pair : {std::array{std::pair{0x0u, 24u}, std::pair{0x8u, 26u}},
                                     std::array{std::pair{0x4u, 25u}, std::pair{0xCu, 27u}}}) {
                for (const auto &[slot, bit] : pair) {
                    if (!(*st >> bit & 1)) continue;
                    auto d = reg(0x009024C0 + p * 0x4000 + slot);
                    if (!d || *d == 0 || *d == 0xFFFFFFFFull || poisoned(*d)) continue;
                    const unsigned long long pc0 = *d >> 16 & 0xFF, pc1 = *d >> 24 & 0xFF;
                    if (pc0 == 0 || pc0 == 0xFF) continue;
                    for (unsigned long long raw : {pc0, clamshell ? pc1 : 0ull}) {
                        if (raw == 0 || raw == 0xFF) continue;
                        const int c = parse(raw);
                        if (c > 0 && c < 150 && (!best || c > *best)) best = c;
                    }
                    break;
                }
            }
        }
        return best;
    }
    void *lib_, *gpu_ = nullptr;
    void *(*qi_)(unsigned) = nullptr;
    int (*therm_)(void *, Therm *) = nullptr;
    int (*reg_)(void *, RegOps *) = nullptr;
    int mask_ = 0;
};

static QString throttleText(unsigned long long m) {
    static const std::pair<unsigned long long, const char *> R[] = {
        {0x04, "power"}, {0x08, "HW slowdown"}, {0x20, "thermal"}, {0x40, "HW thermal"},
        {0x80, "power brake"}, {0x02, "app clocks"}, {0x10, "sync boost"}, {0x100, "display"}};
    QStringList out;
    for (const auto &[bit, name] : R) if (m & bit) out << QString::fromLatin1(name);
    return out.isEmpty() ? QString() : out.join(QStringLiteral(", "));
}

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
    hotspot_ = lbl("Hotspot: -- °C", theme::DANGER, true);
    vram_ = lbl("VRAM: -- °C", theme::PURPLE, true);
    throttle_ = lbl("Limit: --", theme::FG_DIM);
    throttle_->setToolTip("Why the GPU is not boosting higher right now (NVML clock event reasons).\n"
                          "power = at the power limit · thermal = temperature limit · — = boosting freely.\n"
                          "While undervolting: 'power' at the same clocks with less power means the UV works.");
    for (QLabel *l : {temp_, hotspot_, vram_, power_, clock_, memClock_, throttle_}) { stats->addWidget(l); stats->addSpacing(10); }
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
    pointSpin_ = spin(MIN_OFFSET, MAX_OFFSET, " MHz", 92);
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
        if (coreCapSpin_) coreCapSpin_->setValue(target);  // applied with Apply Offsets / saved in the profile
        // The driver never runs a point more than ~1000 MHz below stock (it
        // stores deeper offsets but ignores them): say so before applying.
        int first = -1, last = -1;
        for (int i = s + 1; i < base_.size(); ++i)
            if (target - int(base_[i].y()) < -1000) { if (first < 0) first = i; last = i; }
        if (first >= 0)
            log(QStringLiteral("ℹ️ Points %1–%2 would need more than -1000 MHz; the driver will not take them that low. "
                               "Core cap is set to %3 MHz so the top still holds.").arg(first).arg(last).arg(target));
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
                    "←/→ extend the selection, ↑/↓ nudge by 1 MHz (Shift: 15), Space toggles, Esc clears.\n"
                    "Wheel zooms at the cursor, Shift+wheel pans, +/− zoom, 0 fits.");
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
    // Compact zoom/pan bar (wheel = zoom at cursor, Shift+wheel = pan, +/−/0 keys).
    auto *gb = new QHBoxLayout;
    gb->setSpacing(6);
    auto mini = [](int lo, int hi, int w) {
        auto *sl = new QSlider(Qt::Horizontal);
        sl->setObjectName("miniSlider");
        sl->setRange(lo, hi);
        sl->setFixedWidth(w);
        sl->setFixedHeight(14);
        return sl;
    };
    auto *zoomSl = mini(100, int(VfCurveWidget::MAX_ZOOM * 100), 104);
    zoomSl->setToolTip("Zoom the voltage axis (mouse wheel over the graph zooms at the cursor)");
    auto *zoomLbl = lbl("1.0×", theme::FG_DIM);
    zoomLbl->setFixedWidth(34);
    auto *panSl = mini(0, 1000, 180);
    panSl->setToolTip("Move the zoomed window left/right (Shift+wheel over the graph)");
    auto *bFit = new QPushButton("Fit");
    bFit->setObjectName("btnMini");
    bFit->setToolTip("Show the whole curve (key: 0)");
    gb->addWidget(lbl("Zoom", theme::MUTED));
    gb->addWidget(zoomSl);
    gb->addWidget(zoomLbl);
    gb->addSpacing(4);
    gb->addWidget(lbl("◀", theme::MUTED));
    gb->addWidget(panSl);
    gb->addWidget(lbl("▶", theme::MUTED));
    gb->addWidget(bFit);
    connect(zoomSl, &QSlider::valueChanged, vf_, [this](int v) { vf_->setZoom(v / 100.0); });
    connect(panSl, &QSlider::valueChanged, vf_, [this](int v) { vf_->setPan(v / 1000.0); });
    connect(bFit, &QPushButton::clicked, vf_, &VfCurveWidget::fitAxes);
    connect(vf_, &VfCurveWidget::viewChanged, this, [this, zoomSl, panSl, zoomLbl] {
        const QSignalBlocker a(zoomSl), b(panSl);
        zoomSl->setValue(int(std::lround(vf_->zoom() * 100)));
        panSl->setValue(int(std::lround(vf_->pan() * 1000)));
        panSl->setEnabled(vf_->zoom() > 1.0001);
        zoomLbl->setText(QStringLiteral("%1×").arg(vf_->zoom(), 0, 'f', 1));
    });
    panSl->setEnabled(false);
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
    // Core clock cap (NVML locked clocks). The driver will not take per-point
    // offsets below its floor, so a flatten can stop short of its target; a cap
    // at the flatten frequency makes the GPU use the lowest-voltage point that
    // reaches it — the flat top, whatever the points above it say.
    ch2->addWidget(lbl("Core cap:", theme::MUTED));
    coreCapSpin_ = spin(0, 4000, " MHz", 96);
    coreCapSpin_->setSpecialValueText(QStringLiteral("off"));
    coreCapSpin_->setToolTip("Highest core clock the GPU may use (NVML locked clocks). The driver keeps every curve\n"
                             "point within ~1000 MHz of stock whatever offset it stores, so a flatten cannot pull the\n"
                             "top points all the way down — the cap does: the GPU then runs the lowest-voltage point\n"
                             "that reaches it. Flatten fills it in; it is saved with the profile and applied with\n"
                             "Apply Offsets. Cap / Uncap change it right away.");
    ch2->addWidget(coreCapSpin_);
    auto *bCap = new QPushButton("Cap"), *bUncap = new QPushButton("Uncap");
    connect(bCap, &QPushButton::clicked, this, [this] {
        const int mx = coreCapSpin_->value();
        if (mx < 210) { log("⚠️ Enter a core clock cap in MHz first."); return; }
        runHelper({{"op", "set_gpu_clocklock"}, {"min_mhz", 0}, {"max_mhz", mx}}, QString(), "Failed to cap the core clock");
    });
    connect(bUncap, &QPushButton::clicked, this, [this] {
        runHelper({{"op", "reset_gpu_clocklock"}}, QString(), "Failed to uncap the core clock");
    });
    ch2->addWidget(bCap);
    ch2->addWidget(bUncap);
    ch2->addSpacing(8);
    ch2->addWidget(lbl("PowerMizer:", theme::MUTED));
    powerMizer_ = new QComboBox;
    powerMizer_->setEnabled(false);
    powerMizer_->setToolTip("Auto — the driver decides (default)\n"
                            "Adaptive — clocks drop as soon as load drops\n"
                            "Prefer maximum performance — clocks stay up: no down-clock stutter, more idle power\n"
                            "Prefer consistent performance — steady clocks for benchmarking\n"
                            "Needs driver 580+. Not kept across reboots.");
    connect(powerMizer_, &QComboBox::activated, this, [this] {
        runHelper({{"op", "set_powermizer"}, {"mode", powerMizer_->currentData().toString()}}, QString(),
                  "Could not set PowerMizer", [this](bool, const QJsonObject &) { syncPowerMizer(); });
    });
    ch2->addWidget(powerMizer_);
    ch2->addStretch();
    readBtn_ = new QPushButton("Read Curve");
    connect(readBtn_, &QPushButton::clicked, this, &NvidiaTab::readCurve);
    auto *bApply = new QPushButton("Apply Offsets");
    bApply->setObjectName("btnAccent");
    connect(bApply, &QPushButton::clicked, this, &NvidiaTab::applyOffsets);
    resetBtn_ = new QPushButton("Reset Curve");
    resetBtn_->setObjectName("btnDanger");
    connect(resetBtn_, &QPushButton::clicked, this, &NvidiaTab::resetCurve);
    auto *bResetAll = new QPushButton("Reset All");
    bResetAll->setObjectName("btnDanger");
    bResetAll->setToolTip("Everything back to stock: curve offsets, NVML offsets in every P-state (other tools\n"
                          "can leave some in P2/P5), core and VRAM clock locks, power limit, PowerMizer → Auto.");
    connect(bResetAll, &QPushButton::clicked, this, [this] {
        if (QMessageBox::question(this, "Reset All", "Put every NVIDIA setting back to stock?") != QMessageBox::Yes) return;
        runHelper({{"op", "reset_all"}}, "Everything is back to stock.", "Reset All incomplete",
                  [this](bool, const QJsonObject &) { syncPowerMizer(); readCurve(); });
    });
    ch2->addWidget(readBtn_);
    ch2->addWidget(bApply);
    ch2->addWidget(resetBtn_);
    ch2->addWidget(bResetAll);
    root->addWidget(ctl);
    actionButtons_ = {bSave, bDef, bApplyProf, bDel, bLock, bUnlock, bCap, bUncap, readBtn_, bApply, resetBtn_, bResetAll};

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

NvidiaTab::~NvidiaTab() { delete temps_; delete nvml_; }

void NvidiaTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    if (firstShow_) {
        firstShow_ = false;
        // Dev aid: LPM_NVCURVE_FAKE=/path/read.json renders a canned curve.
        if (const QString fake = qEnvironmentVariable("LPM_NVCURVE_FAKE"); !fake.isEmpty()) {
            QFile f(fake);
            if (f.open(QIODevice::ReadOnly)) takeGpuPoints(QJsonDocument::fromJson(f.readAll()).object().value("vf_curve").toArray(), true);
            // LPM_NVCURVE_VIEW="zoom,pan" (e.g. "4,0.3") for zoomed screenshots.
            if (const QStringList v = qEnvironmentVariable("LPM_NVCURVE_VIEW").split(','); v.size() == 2) {
                vf_->setZoom(v[0].toDouble());
                vf_->setPan(v[1].toDouble());
            }
        } else {
            QTimer::singleShot(0, this, &NvidiaTab::readCurve);
        }
    }
    if (!nvml_) nvml_ = Nvml::open();
    syncPowerMizer();
    pollStats();
    statsTimer_->start();
}

void NvidiaTab::hideEvent(QHideEvent *e) {
    QWidget::hideEvent(e);
    statsTimer_->stop();
    delete temps_;  // NvAPI_Unload, like NVML below
    temps_ = nullptr;
    tempsTried_ = false;
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
    if (auto r = nvml_->eventReasons()) {
        const QString t = throttleText(*r);
        throttle_->setText(QStringLiteral("Limit: ") + (t.isEmpty() ? QStringLiteral("—") : t));
        throttle_->setStyleSheet(QStringLiteral("color:%1;").arg(t.isEmpty() ? theme::FG_DIM : theme::WARN));
    }
    if (!tempsTried_) {
        tempsTried_ = true;
        temps_ = NvApiTemps::open();
        blackwell_ = nvml_->architecture().value_or(0) >= 10;
        if (!temps_)
            for (QLabel *l : {hotspot_, vram_})
                l->setToolTip("NvAPI unavailable for this user — `sudo nvcurve sensors` reads it as root");
    }
    if (temps_) {
        const auto h = temps_->hotspot(blackwell_), v = temps_->vram(blackwell_);
        hotspot_->setText(h ? QStringLiteral("Hotspot: %1 °C").arg(*h) : QStringLiteral("Hotspot: — °C"));
        vram_->setText(v ? QStringLiteral("VRAM: %1 °C").arg(*v) : QStringLiteral("VRAM: — °C"));
    }
}

void NvidiaTab::syncPowerMizer() {
    if (!nvml_) return;
    static const std::pair<unsigned, const char *> MODES[] = {
        {2, "auto"}, {0, "adaptive"}, {1, "max"}, {3, "consistent"}};
    static const char *const LABELS[] = {"Adaptive", "Prefer max performance", "Auto", "Prefer consistent"};
    const auto pm = nvml_->powerMizer();
    const QSignalBlocker b(powerMizer_);
    powerMizer_->clear();
    if (!pm) { powerMizer_->addItem(QStringLiteral("n/a")); powerMizer_->setEnabled(false); return; }
    for (const auto &[id, key] : MODES)
        if (pm->supported & (1u << id)) powerMizer_->addItem(QString::fromLatin1(LABELS[id]), QString::fromLatin1(key));
    for (const auto &[id, key] : MODES)
        if (id == pm->current) powerMizer_->setCurrentIndex(powerMizer_->findData(QString::fromLatin1(key)));
    powerMizer_->setEnabled(powerMizer_->count() > 1);
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
    total = std::clamp(total, MIN_OFFSET, MAX_OFFSET);
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
    if (deltas.isEmpty() && mem == 0 && lmax == 0 && coreCapSpin_->value() < 210) { log("No offsets to apply."); return; }
    if (lmax && lmin > lmax) { log("⚠️ VRAM lock: Min cannot be greater than Max."); return; }

    const QJsonObject data{{"name", SCRATCH}, {"curve_deltas", deltas},
                           {"mem_offset_mhz", mem ? QJsonValue(mem) : QJsonValue()}, {"power_limit_w", QJsonValue()},
                           {"mem_locked_min_mhz", lmax ? QJsonValue(lmin) : QJsonValue()},
                           {"mem_locked_max_mhz", lmax ? QJsonValue(lmax) : QJsonValue()},
                           {"gpu_clock_cap_mhz", coreCapSpin_->value() >= 210 ? QJsonValue(coreCapSpin_->value()) : QJsonValue()}};
    const qint64 t0 = QDateTime::currentMSecsSinceEpoch();
    QHash<int, int> requested;
    for (int i = 0; i < base_.size(); ++i) requested.insert(i, offsetOf(i));
    const QVector<QPointF> baseBefore = base_;
    runHelper({{"op", "apply_gpu_offsets"}, {"profile_name", SCRATCH}, {"profile_data", data}},
              "Offsets applied.", "Apply failed", [this, t0, requested, baseBefore](bool ok, const QJsonObject &) {
        QJsonArray pts;
        if (!ok || !loadCurveFile("nvcurve_apply_result.json", t0, &pts)) return;
        takeGpuPoints(pts, true);
        log("✅ Curve re-read from the GPU after apply.");
        reportClamping(baseBefore, requested);
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
        coreCapSpin_->setValue(0);
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
    coreCapSpin_->setValue(data.value("gpu_clock_cap_mhz").toInt());
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
                           {"mem_locked_max_mhz", lmax ? QJsonValue(lmax) : QJsonValue()},
                           {"gpu_clock_cap_mhz", coreCapSpin_->value() >= 210 ? QJsonValue(coreCapSpin_->value()) : QJsonValue()}};
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

/// After an apply: say where the GPU did not end up where it was asked to.
/// Two different driver behaviours, both seen as "the flatten did not hold":
///  - the stored freqDelta differs from the request (the driver clamped it);
///  - the delta was stored but the point's clock did not move by it (its
///    base shifted) — the driver keeps the effective clock above a floor.
void NvidiaTab::reportClamping(const QVector<QPointF> &baseBefore, const QHash<int, int> &requested) {
    if (base_.size() != baseBefore.size()) return;
    struct Run { int from, to, want, got, kind; };
    QList<Run> runs;
    int lowestGot = 0;
    for (int i = 0; i < base_.size(); ++i) {
        const int want = requested.value(i), stored = readOffsets_.value(i);
        const int wantFreq = int(baseBefore[i].y()) + want, gotFreq = int(base_[i].y()) + stored;
        int kind = 0;
        if (std::abs(stored - want) > 2) kind = 1;               // delta clamped
        else if (std::abs(gotFreq - wantFreq) > 15) kind = 2;    // clock did not follow
        if (!kind) continue;
        lowestGot = std::min(lowestGot, stored);
        const int got = kind == 1 ? stored : gotFreq, w = kind == 1 ? want : wantFreq;
        if (!runs.isEmpty() && runs.back().kind == kind && runs.back().to == i - 1) { runs.back().to = i; runs.back().got = got; }
        else runs.append({i, i, w, got, kind});
    }
    if (runs.isEmpty()) return;
    for (const Run &r : runs) {
        const QString pts = r.from == r.to ? QStringLiteral("point %1").arg(r.from) : QStringLiteral("points %1–%2").arg(r.from).arg(r.to);
        log(r.kind == 1 ? QStringLiteral("⚠️ %1: the driver stored a smaller offset than requested (asked %2 MHz, kept %3 MHz).").arg(pts).arg(r.want).arg(r.got)
                        : QStringLiteral("⚠️ %1: offset stored, but the clock stayed above the request (asked %2 MHz, runs %3 MHz).").arg(pts).arg(r.want).arg(r.got));
    }
    log(QStringLiteral("ℹ️ The driver stores offsets down to %1 MHz but never runs a point more than ~1000 MHz below its stock "
                       "clock. Core cap holds the flat top instead (flatten fills it in; Apply Offsets and the profile include it).").arg(lowestGot));
}
