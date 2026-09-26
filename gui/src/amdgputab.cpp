#include "amdgputab.h"
#include "privileged.h"
#include "theme.h"
#include <QApplication>
#include <QCheckBox>
#include <QClipboard>
#include <QComboBox>
#include <QDir>
#include <QFile>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QHideEvent>
#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QMessageBox>
#include <QPushButton>
#include <QRegularExpression>
#include <QScrollArea>
#include <QSettings>
#include <QShowEvent>
#include <QSlider>
#include <QSpinBox>
#include <QStandardPaths>
#include <QTimer>
#include <QVBoxLayout>
#include <cmath>

using namespace amdgpu;

static constexpr int LIVE_MS = 1500, REVERT_SECONDS = 20, UV_STEP_MV = 10;
static constexpr int SANE_MHZ = 4000;
static const char *const HELPER = "amdgpu-helper";

static const std::pair<const char *, const char *> PERF_LEVELS[] = {
    {"auto", "Automatic (driver decides)"},
    {"high", "High — hold the highest clocks"},
    {"low", "Low — hold the lowest clocks"},
    {"manual", "Manual — required for power profiles"},
    {"profile_standard", "Profiling: standard (fixed, for benchmarks)"},
    {"profile_peak", "Profiling: peak"},
    {"profile_min_sclk", "Profiling: minimum GPU clock"},
    {"profile_min_mclk", "Profiling: minimum memory clock"},
};
static const std::pair<const char *, const char *> FAN_FIELDS[] = {
    {"zero_rpm_stop", "Zero-RPM stop below (°C)"}, {"min_pwm", "Minimum fan speed (%)"},
    {"target_temp", "Target temperature (°C)"}, {"acoustic_limit", "Acoustic limit (RPM)"},
    {"acoustic_target", "Acoustic target (RPM)"},
};

static QLabel *muted(const QString &t) {
    auto *l = new QLabel(t);
    l->setProperty("role", "muted");
    l->setWordWrap(true);
    return l;
}
static QSpinBox *spin(int lo, int hi, int v, const QString &suffix) {
    // The current value is always reachable: some firmware reports stock
    // points outside its own OD_RANGE (the helper accepts them unchanged).
    auto *s = new QSpinBox;
    s->setRange(std::min(lo, v), std::max(hi, v));
    s->setValue(v);
    s->setSuffix(suffix);
    s->setKeyboardTracking(false);
    return s;
}
static QString profilesDir() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QStringLiteral("/legion-power-manager/amdgpu");
}
/// Stock reference + last kept summary per card (not the profiles).
static QString storePath() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation)
           + QStringLiteral("/legion-power-manager/amdgpu-state.ini");
}
static QString kindName(Od::Kind k) {
    switch (k) {
    case Od::PerState: return QStringLiteral("per-state clock/voltage table (Polaris / Vega)");
    case Od::Curve: return QStringLiteral("voltage/frequency curve (Vega 20 / RDNA1)");
    case Od::MinMax: return QStringLiteral("clock limits (+ voltage offset where the SMU has one) — RDNA2 / RDNA3 / APU");
    case Od::Offset: return QStringLiteral("clock offset + voltage offset (RDNA4)");
    case Od::None: break;
    }
    return QStringLiteral("not available");
}

// ── construction ────────────────────────────────────────────────────────────

AmdGpuTab::AmdGpuTab(QWidget *parent) : QWidget(parent) {
    cards_ = cards();
    auto *outer = new QVBoxLayout(this);
    outer->setContentsMargins(10, 10, 10, 10);
    outer->setSpacing(8);

    auto *top = new QHBoxLayout;
    top->addWidget(new QLabel(QStringLiteral("GPU")));
    cardCombo_ = new QComboBox;
    for (const Card &c : cards_) cardCombo_->addItem(c.label);
    top->addWidget(cardCombo_, 1);
    kindLabel_ = muted({});
    top->addWidget(kindLabel_, 2);
    outer->addLayout(top);

    banner_ = new QLabel;
    banner_->setWordWrap(true);
    banner_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    banner_->setStyleSheet(QStringLiteral(
        "background: %1; border: 1px solid %2; border-left: 3px solid %3; border-radius: %4px; padding: 5px 9px; color: %5;")
        .arg(theme::BG2, theme::ACCENT_SOFT, theme::ACCENT).arg(theme::RADIUS).arg(theme::FG_DIM));
    banner_->hide();
    outer->addWidget(banner_);

    live_ = new QLabel(QStringLiteral("—"));
    live_->setStyleSheet(QStringLiteral("font-family: monospace; color: %1;").arg(theme::FG_DIM));
    outer->addWidget(live_);

    auto *scroll = new QScrollArea;
    scroll->setWidgetResizable(true);
    scroll->setFrameShape(QFrame::NoFrame);
    auto *host = new QWidget;
    formHost_ = new QVBoxLayout(host);
    formHost_->setContentsMargins(0, 0, 0, 0);
    scroll->setWidget(host);
    outer->addWidget(scroll, 1);

    // Trial bar: shown after Apply until the user keeps or reverts.
    trialBar_ = new QFrame;
    trialBar_->setStyleSheet(QStringLiteral("QFrame { background: %1; border: 1px solid %2; border-radius: %3px; }")
                                 .arg(theme::BG2, theme::WARN).arg(theme::RADIUS));
    auto *tb = new QHBoxLayout(trialBar_);
    trialLabel_ = new QLabel;
    tb->addWidget(trialLabel_, 1);
    auto *keep = new QPushButton(QStringLiteral("Keep"));
    keep->setObjectName("btnAccent");
    auto *revert = new QPushButton(QStringLiteral("Revert now"));
    tb->addWidget(keep);
    tb->addWidget(revert);
    trialBar_->hide();
    outer->addWidget(trialBar_);
    connect(keep, &QPushButton::clicked, this, [this] { endTrial(true); });
    connect(revert, &QPushButton::clicked, this, [this] { endTrial(false); });
    trialTimer_ = new QTimer(this);
    trialTimer_->setInterval(1000);
    connect(trialTimer_, &QTimer::timeout, this, [this] {
        if (--trialLeft_ <= 0) { endTrial(false); return; }
        trialLabel_->setText(QStringLiteral("Testing new settings — reverting in %1 s unless you keep them. "
                                            "Run your game or a stress test now.").arg(trialLeft_));
    });

    auto *bottom = new QHBoxLayout;
    bottom->addWidget(new QLabel(QStringLiteral("Preset")));
    presetCombo_ = new QComboBox;
    presetCombo_->addItems({QStringLiteral("—"), QStringLiteral("Stock"), QStringLiteral("Efficiency"), QStringLiteral("Performance")});
    presetCombo_->setToolTip(QStringLiteral(
        "Fills the form (nothing is written until Apply):\n"
        "Stock — the values the driver started with.\n"
        "Efficiency — power limit −15 %, max GPU clock −10 %, −30 mV where the card allows it.\n"
        "Performance — highest power limit and the 3D full-screen profile; clocks stay stock."));
    bottom->addWidget(presetCombo_);
    connect(presetCombo_, &QComboBox::activated, this, [this](int i) { if (i > 0) applyPreset(i); presetCombo_->setCurrentIndex(0); });
    bottom->addSpacing(12);
    bottom->addWidget(new QLabel(QStringLiteral("Profile")));
    profileCombo_ = new QComboBox;
    profileCombo_->setMinimumWidth(140);
    bottom->addWidget(profileCombo_);
    auto *bLoad = new QPushButton(QStringLiteral("Load")), *bSave = new QPushButton(QStringLiteral("Save…")),
         *bDel = new QPushButton(QStringLiteral("Delete"));
    bDel->setObjectName("btnDanger");
    for (QPushButton *b : {bLoad, bSave, bDel}) bottom->addWidget(b);
    connect(bLoad, &QPushButton::clicked, this, &AmdGpuTab::loadProfile);
    connect(bSave, &QPushButton::clicked, this, &AmdGpuTab::saveProfile);
    connect(bDel, &QPushButton::clicked, this, &AmdGpuTab::deleteProfile);
    bottom->addStretch(1);
    uvBtn_ = new QPushButton(QStringLiteral("Undervolt step −%1 mV").arg(UV_STEP_MV));
    uvBtn_->setToolTip(QStringLiteral("Lowers the voltage offset by %1 mV and applies it as a trial. Repeat until the "
                                      "game or stress test crashes, then keep the last step that held.").arg(UV_STEP_MV));
    resetBtn_ = new QPushButton(QStringLiteral("Reset to stock"));
    applyBtn_ = new QPushButton(QStringLiteral("Apply"));
    applyBtn_->setObjectName("btnAccent");
    for (QPushButton *b : {uvBtn_, resetBtn_, applyBtn_}) bottom->addWidget(b);
    connect(uvBtn_, &QPushButton::clicked, this, &AmdGpuTab::undervoltStep);
    connect(resetBtn_, &QPushButton::clicked, this, &AmdGpuTab::resetCard);
    connect(applyBtn_, &QPushButton::clicked, this, [this] { apply(collect(), true); });
    outer->addLayout(bottom);

    auto *st = new QHBoxLayout;
    status_ = muted({});
    stableLabel_ = muted({});
    stableLabel_->setAlignment(Qt::AlignRight | Qt::AlignVCenter);
    st->addWidget(status_, 2);
    st->addWidget(stableLabel_, 1);
    outer->addLayout(st);

    liveTimer_ = new QTimer(this);
    liveTimer_->setInterval(LIVE_MS);
    connect(liveTimer_, &QTimer::timeout, this, &AmdGpuTab::pollLive);
    connect(cardCombo_, &QComboBox::currentIndexChanged, this, &AmdGpuTab::selectCard);

    QDir().mkpath(profilesDir());
    reloadProfiles();
    // Prefer the discrete card when there are several.
    int pick = 0;
    for (int i = 0; i < cards_.size(); ++i) if (!cards_[i].integrated) { pick = i; break; }
    cardCombo_->setCurrentIndex(pick);
    selectCard(pick);
}

void AmdGpuTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    pollLive();
    liveTimer_->start();
}
void AmdGpuTab::hideEvent(QHideEvent *e) {
    QWidget::hideEvent(e);
    liveTimer_->stop();
}

const Card *AmdGpuTab::card() const {
    const int i = cardCombo_->currentIndex();
    return i >= 0 && i < cards_.size() ? &cards_[i] : nullptr;
}

void AmdGpuTab::selectCard(int) {
    if (!card()) return;
    snap_ = read(*card());
    rebuildForm();
    const QJsonObject now = settingsOf(snap_);
    fill(now);
    // First sight of this card: what the driver reports is the stock state.
    QSettings st(storePath(), QSettings::IniFormat);
    const QString key = QStringLiteral("amdgpu/stock/") + card()->pciId + '/' + card()->name;
    if (!st.contains(key)) st.setValue(key, QString::fromUtf8(QJsonDocument(now).toJson(QJsonDocument::Compact)));
    kindLabel_->setText(QStringLiteral("Overdrive: ") + kindName(snap_.od.kind));
    updateBanner();
    const QString stable = st.value(QStringLiteral("amdgpu/stable/") + card()->pciId).toString();
    stableLabel_->setText(stable.isEmpty() ? QString() : QStringLiteral("Last kept: ") + stable);
    uvBtn_->setVisible(voltOffset_ != nullptr);
    pollLive();
}

void AmdGpuTab::updateBanner() {
    QStringList lines;
    const auto mask = featureMask();
    if (snap_.od.kind == Od::None) {
        if (mask && !(*mask & OVERDRIVE_BIT)) {
            const QString param = QStringLiteral("amdgpu.ppfeaturemask=0x%1").arg(*mask | OVERDRIVE_BIT, 0, 16);
            lines << QStringLiteral("<b>Overdrive is disabled.</b> Clock and voltage control needs the overdrive bit in the "
                                    "driver's feature mask. Add <code>%1</code> to the kernel command line "
                                    "(Optimizations → Boot options lists it too) and reboot. Note: the kernel marks itself "
                                    "tainted while overdrive is on.").arg(param);
        } else if (snap_.odFilePresent) {
            lines << QStringLiteral("The driver exposes no overdrive table for this GPU (firmware-locked or unsupported "
                                    "generation). Power limit, profile and fan settings still work where shown.");
        } else {
            lines << QStringLiteral("This GPU has no overdrive interface. Performance level and power profile still work.");
        }
    }
    if (card() && card()->integrated && snap_.od.kind != Od::None)
        lines << QStringLiteral("Integrated GPU: the SMU only lets you move the clock window; there is no voltage "
                                "control and power comes out of the CPU's budget — lowering the maximum clock "
                                "leaves more power for the CPU cores.");
    banner_->setText(lines.join(QStringLiteral("<br>")));
    banner_->setVisible(!lines.isEmpty());
}

// ── form ────────────────────────────────────────────────────────────────────

void AmdGpuTab::rebuildForm() {
    delete form_;  // at once: a deferred delete left the old card's form on screen for a frame
    form_ = nullptr;
    perf_ = profile_ = nullptr;
    capSlider_ = nullptr;
    cap_ = sclkMin_ = sclkMax_ = mclkMin_ = mclkMax_ = sclkOffset_ = voltOffset_ = nullptr;
    curve_.clear(); sclkStates_.clear(); mclkStates_.clear(); fanCurve_.clear(); fan_.clear();
    zeroRpm_ = nullptr;

    form_ = new QWidget;
    auto *v = new QVBoxLayout(form_);
    v->setContentsMargins(0, 0, 0, 0);
    auto box = [&](const QString &title) {
        auto *b = new QGroupBox(title);
        auto *g = new QGridLayout(b);
        g->setColumnStretch(2, 1);
        v->addWidget(b);
        return g;
    };
    auto row = [](QGridLayout *g, const QString &name, QWidget *w, const QString &hint = {}) {
        const int r = g->rowCount();
        g->addWidget(new QLabel(name), r, 0);
        g->addWidget(w, r, 1, Qt::AlignLeft);
        if (!hint.isEmpty()) g->addWidget(muted(hint), r, 2);
    };

    // Power
    QGridLayout *pg = box(QStringLiteral("Power && performance"));
    if (!snap_.perfLevel.isEmpty()) {
        perf_ = new QComboBox;
        for (const auto &[k, label] : PERF_LEVELS) perf_->addItem(QString::fromUtf8(label), QString::fromLatin1(k));
        row(pg, QStringLiteral("Performance level"), perf_, QStringLiteral("Automatic is right for almost everything; "
            "a power profile switches this to Manual by itself."));
    }
    if (!snap_.profiles.isEmpty()) {
        profile_ = new QComboBox;
        for (const Profile &p : snap_.profiles)
            if (p.name != QLatin1String("CUSTOM")) profile_->addItem(QString(p.name).replace('_', ' ').toLower(), p.index);
        // A non-default profile only takes effect under Manual (the helper enforces it too).
        connect(profile_, &QComboBox::activated, this, [this] {
            if (perf_ && profile_->currentText() != QLatin1String("bootup default") && perf_->currentData() == QStringLiteral("auto"))
                perf_->setCurrentIndex(perf_->findData(QStringLiteral("manual")));
        });
        row(pg, QStringLiteral("Power profile"), profile_, QStringLiteral("How eagerly the SMU raises clocks. "
            "3D full screen: games · Compute: long GPU jobs · Power saving: battery."));
    }
    if (snap_.capW && snap_.capMinW && snap_.capMaxW && *snap_.capMaxW > *snap_.capMinW) {
        auto *w = new QWidget;
        auto *h = new QHBoxLayout(w);
        h->setContentsMargins(0, 0, 0, 0);
        const int lo = int(std::ceil(*snap_.capMinW)), hi = int(std::floor(*snap_.capMaxW));
        w->setMinimumWidth(360);
        capSlider_ = new QSlider(Qt::Horizontal);
        capSlider_->setRange(lo, hi);
        cap_ = spin(lo, hi, int(*snap_.capW), QStringLiteral(" W"));
        connect(capSlider_, &QSlider::valueChanged, cap_, &QSpinBox::setValue);
        connect(cap_, &QSpinBox::valueChanged, capSlider_, &QSlider::setValue);
        capSlider_->setValue(cap_->value());
        h->addWidget(capSlider_, 1);
        h->addWidget(cap_);
        row(pg, QStringLiteral("Power limit"), w, snap_.capDefaultW
            ? QStringLiteral("Default %1 W. Lower = cooler and quieter at a small FPS cost.").arg(int(*snap_.capDefaultW)) : QString());
    }
    if (pg->rowCount() == 0) pg->parentWidget()->hide();

    // Overdrive
    const Od &od = snap_.od;
    auto rangeOf = [&](const QString &k, QPair<int, int> fallback) { return od.range(k).value_or(fallback); };
    if (od.kind != Od::None) {
        QGridLayout *og = box(QStringLiteral("Clocks && voltage (overclock / undervolt)"));
        const auto sr = rangeOf(QStringLiteral("SCLK"), {0, SANE_MHZ}), mr = rangeOf(QStringLiteral("MCLK"), {0, SANE_MHZ});
        const int shi = std::min(sr.second, SANE_MHZ), mhi = std::min(mr.second, SANE_MHZ);
        if (od.kind == Od::PerState) {
            auto table = [&](const QString &title, const QList<State> &states, QPair<int, int> r, QList<Pair> &out) {
                const auto vr = rangeOf(QStringLiteral("VDDC"), {700, 1200});
                auto *w = new QWidget;
                auto *g = new QGridLayout(w);
                g->setContentsMargins(0, 0, 0, 0);
                for (const State &s : states) {
                    const int c = out.size();
                    g->addWidget(muted(QStringLiteral("P%1").arg(s.index)), 0, c, Qt::AlignHCenter);
                    auto *mhz = spin(r.first, std::min(r.second, SANE_MHZ), s.mhz, QStringLiteral(" MHz"));
                    auto *mv = spin(vr.first, vr.second, s.mv, QStringLiteral(" mV"));
                    g->addWidget(mhz, 1, c);
                    g->addWidget(mv, 2, c);
                    out << Pair{mhz, mv};
                }
                row(og, title, w);
            };
            table(QStringLiteral("GPU states"), od.sclk, sr, sclkStates_);
            if (!od.mclk.isEmpty()) table(QStringLiteral("Memory states"), od.mclk, mr, mclkStates_);
            og->addWidget(muted(QStringLiteral("Undervolt: lower the mV of the top states in 10–25 mV steps. Overclock: raise the "
                                               "top state's MHz. Clocks must not decrease from left to right.")), og->rowCount(), 0, 1, 3);
        } else {
            if (od.kind != Od::Offset) {
                if (od.sclkAt(0)) { sclkMin_ = spin(sr.first, shi, *od.sclkAt(0), QStringLiteral(" MHz"));
                    row(og, QStringLiteral("GPU clock minimum"), sclkMin_, QStringLiteral("Raise it only to stop down-clocking stutter.")); }
                if (od.sclkAt(1)) { sclkMax_ = spin(sr.first, shi, *od.sclkAt(1), QStringLiteral(" MHz"));
                    row(og, QStringLiteral("GPU clock maximum"), sclkMax_, QStringLiteral("Lower = efficiency; higher = overclock (needs the power to back it)."));
                }
            }
            if (od.sclkOffset) {
                const auto r = rangeOf(QStringLiteral("SCLK_OFFSET"), {-500, 1000});
                sclkOffset_ = spin(r.first, r.second, *od.sclkOffset, QStringLiteral(" MHz"));
                row(og, QStringLiteral("GPU clock offset"), sclkOffset_, QStringLiteral("Shifts the whole boost curve."));
            }
            if (od.mclkAt(0) && od.kind != Od::Curve) {
                mclkMin_ = spin(mr.first, mhi, *od.mclkAt(0), QStringLiteral(" MHz"));
                row(og, QStringLiteral("Memory clock minimum"), mclkMin_);
            }
            if (od.mclkAt(1)) { mclkMax_ = spin(mr.first, mhi, *od.mclkAt(1), QStringLiteral(" MHz"));
                row(og, QStringLiteral("Memory clock maximum"), mclkMax_, QStringLiteral("VRAM overclock: +50 MHz steps; artifacts mean too far."));
            }
            if (od.kind == Od::Curve) {
                auto *w = new QWidget;
                auto *g = new QGridLayout(w);
                g->setContentsMargins(0, 0, 0, 0);
                for (int i = 0; i < od.curve.size(); ++i) {
                    const auto cr = rangeOf(QStringLiteral("VDDC_CURVE_SCLK[%1]").arg(i), sr);
                    const auto vr = rangeOf(QStringLiteral("VDDC_CURVE_VOLT[%1]").arg(i), {700, 1200});
                    auto *mhz = spin(cr.first, std::min(cr.second, SANE_MHZ), od.curve[i].mhz, QStringLiteral(" MHz"));
                    auto *mv = spin(vr.first, vr.second, od.curve[i].mv, QStringLiteral(" mV"));
                    g->addWidget(muted(QStringLiteral("Point %1").arg(i)), 0, i, Qt::AlignHCenter);
                    g->addWidget(mhz, 1, i);
                    g->addWidget(mv, 2, i);
                    curve_ << Pair{mhz, mv};
                }
                row(og, QStringLiteral("V/F curve"), w, QStringLiteral("Undervolt: lower the mV of the top point."));
            }
            if (od.voltageOffset) {
                const auto r = od.range(QStringLiteral("VDDGFX_OFFSET")).value_or(QPair<int, int>{-250, 0});
                voltOffset_ = spin(r.first, std::min(r.second, 0), *od.voltageOffset, QStringLiteral(" mV"));
                voltOffset_->setSingleStep(5);
                row(og, QStringLiteral("Voltage offset"), voltOffset_,
                    QStringLiteral("Undervolt across the whole curve: same clocks at lower power and heat. "
                                   "Typical stable range −30…−100 mV; every chip differs."));
            }
        }
    }

    // PMFW fan (RDNA3+)
    if (!snap_.fan.isEmpty() || snap_.fanCurve) {
        QGridLayout *fg = box(QStringLiteral("Fan (firmware control)"));
        if (snap_.fan.contains(QStringLiteral("zero_rpm"))) {
            zeroRpm_ = new QCheckBox(QStringLiteral("Stop the fans at low temperature"));
            row(fg, QStringLiteral("Zero RPM"), zeroRpm_);
        }
        for (const auto &[k, label] : FAN_FIELDS) {
            const QString key = QString::fromLatin1(k);
            if (!snap_.fan.contains(key)) continue;
            const FanValue f = snap_.fan.value(key);
            fan_.insert(key, spin(f.min, f.max, f.value, {}));
            row(fg, QString::fromUtf8(label), fan_.value(key));
        }
        if (snap_.fanCurve) {
            auto *w = new QWidget;
            auto *g = new QGridLayout(w);
            g->setContentsMargins(0, 0, 0, 0);
            for (int i = 0; i < snap_.fanCurve->points.size(); ++i) {
                const auto &[t, p] = snap_.fanCurve->points[i];
                auto *ts = spin(snap_.fanCurve->temp.first, snap_.fanCurve->temp.second, t, QStringLiteral(" °C"));
                auto *ps = spin(snap_.fanCurve->speed.first, snap_.fanCurve->speed.second, p, QStringLiteral(" %"));
                g->addWidget(ts, 0, i);
                g->addWidget(ps, 1, i);
                fanCurve_ << Pair{ts, ps};
            }
            row(fg, QStringLiteral("Fan curve"), w, QStringLiteral("Hotspot temperature → fan speed; must not fall."));
        }
    }
    v->addStretch(1);
    formHost_->addWidget(form_);
}

QJsonObject AmdGpuTab::settingsOf(const Snapshot &s) const {
    QJsonObject o;
    if (!s.perfLevel.isEmpty()) o[QStringLiteral("perf_level")] = s.perfLevel;
    for (const Profile &p : s.profiles) if (p.active && p.name != QLatin1String("CUSTOM")) o[QStringLiteral("profile")] = p.index;
    if (s.capW) o[QStringLiteral("power_cap_w")] = std::round(*s.capW);
    const Od &od = s.od;
    auto pairs = [](const QList<State> &l) { QJsonArray a; for (const State &x : l) a.append(QJsonArray{x.mhz, x.mv}); return a; };
    if (od.kind == Od::PerState) {
        o[QStringLiteral("sclk_states")] = pairs(od.sclk);
        if (!od.mclk.isEmpty()) o[QStringLiteral("mclk_states")] = pairs(od.mclk);
    } else if (od.kind != Od::None) {
        if (od.kind != Od::Offset) {
            if (auto v = od.sclkAt(0)) o[QStringLiteral("sclk_min")] = *v;
            if (auto v = od.sclkAt(1)) o[QStringLiteral("sclk_max")] = *v;
        }
        if (od.kind != Od::Curve) if (auto v = od.mclkAt(0)) o[QStringLiteral("mclk_min")] = *v;
        if (auto v = od.mclkAt(1)) o[QStringLiteral("mclk_max")] = *v;
        if (od.kind == Od::Curve) o[QStringLiteral("vddc_curve")] = pairs(od.curve);
        if (od.sclkOffset) o[QStringLiteral("sclk_offset")] = *od.sclkOffset;
        if (od.voltageOffset) o[QStringLiteral("voltage_offset")] = std::min(*od.voltageOffset, 0);
    }
    QJsonObject fan;
    for (auto it = s.fan.cbegin(); it != s.fan.cend(); ++it)
        fan[it.key()] = it.key() == QLatin1String("zero_rpm") ? QJsonValue(it->value != 0) : QJsonValue(it->value);
    if (s.fanCurve) { QJsonArray a; for (const auto &[t, p] : s.fanCurve->points) a.append(QJsonArray{t, p}); fan[QStringLiteral("curve")] = a; }
    if (!fan.isEmpty()) o[QStringLiteral("fan")] = fan;
    return o;
}

QJsonObject AmdGpuTab::collect() const {
    QJsonObject o;
    if (perf_) o[QStringLiteral("perf_level")] = perf_->currentData().toString();
    if (profile_ && profile_->currentIndex() >= 0) o[QStringLiteral("profile")] = profile_->currentData().toInt();
    if (cap_) o[QStringLiteral("power_cap_w")] = cap_->value();
    auto pairs = [](const QList<Pair> &l) { QJsonArray a; for (const Pair &p : l) a.append(QJsonArray{p.first->value(), p.second->value()}); return a; };
    if (!sclkStates_.isEmpty()) o[QStringLiteral("sclk_states")] = pairs(sclkStates_);
    if (!mclkStates_.isEmpty()) o[QStringLiteral("mclk_states")] = pairs(mclkStates_);
    if (!curve_.isEmpty()) o[QStringLiteral("vddc_curve")] = pairs(curve_);
    const std::pair<QSpinBox *, const char *> ints[] = {
        {sclkMin_, "sclk_min"}, {sclkMax_, "sclk_max"}, {mclkMin_, "mclk_min"}, {mclkMax_, "mclk_max"},
        {sclkOffset_, "sclk_offset"}, {voltOffset_, "voltage_offset"}};
    for (const auto &[w, k] : ints) if (w) o[QLatin1String(k)] = w->value();
    QJsonObject fan;
    if (zeroRpm_) fan[QStringLiteral("zero_rpm")] = zeroRpm_->isChecked();
    for (auto it = fan_.cbegin(); it != fan_.cend(); ++it) fan[it.key()] = (*it)->value();
    if (!fanCurve_.isEmpty()) fan[QStringLiteral("curve")] = pairs(fanCurve_);
    if (!fan.isEmpty()) o[QStringLiteral("fan")] = fan;
    return o;
}

void AmdGpuTab::fill(const QJsonObject &s) {
    auto setPairs = [](const QList<Pair> &w, const QJsonValue &v) {
        const QJsonArray a = v.toArray();
        for (int i = 0; i < w.size() && i < a.size(); ++i) {
            w[i].first->setValue(a[i].toArray().at(0).toInt());
            w[i].second->setValue(a[i].toArray().at(1).toInt());
        }
    };
    if (perf_ && s.contains(QStringLiteral("perf_level"))) {
        const int i = perf_->findData(s.value(QStringLiteral("perf_level")).toString());
        if (i >= 0) perf_->setCurrentIndex(i);
    }
    if (profile_ && s.contains(QStringLiteral("profile"))) {
        const int i = profile_->findData(s.value(QStringLiteral("profile")).toInt());
        if (i >= 0) profile_->setCurrentIndex(i);
    }
    if (cap_ && s.contains(QStringLiteral("power_cap_w"))) cap_->setValue(int(std::round(s.value(QStringLiteral("power_cap_w")).toDouble())));
    setPairs(sclkStates_, s.value(QStringLiteral("sclk_states")));
    setPairs(mclkStates_, s.value(QStringLiteral("mclk_states")));
    setPairs(curve_, s.value(QStringLiteral("vddc_curve")));
    const std::pair<QSpinBox *, const char *> ints[] = {
        {sclkMin_, "sclk_min"}, {sclkMax_, "sclk_max"}, {mclkMin_, "mclk_min"}, {mclkMax_, "mclk_max"},
        {sclkOffset_, "sclk_offset"}, {voltOffset_, "voltage_offset"}};
    for (const auto &[w, k] : ints) if (w && s.contains(QLatin1String(k))) w->setValue(s.value(QLatin1String(k)).toInt());
    const QJsonObject fan = s.value(QStringLiteral("fan")).toObject();
    if (zeroRpm_ && fan.contains(QStringLiteral("zero_rpm"))) zeroRpm_->setChecked(fan.value(QStringLiteral("zero_rpm")).toBool());
    for (auto it = fan_.cbegin(); it != fan_.cend(); ++it) if (fan.contains(it.key())) (*it)->setValue(fan.value(it.key()).toInt());
    setPairs(fanCurve_, fan.value(QStringLiteral("curve")));
}

// ── live readout (only while the tab is on screen) ──────────────────────────

void AmdGpuTab::pollLive() {
    if (!card() || !isVisible()) return;
    // An integrated GPU is always awake; a discrete one in runtime suspend is not woken for a readout.
    if (!card()->integrated && amdgpu::readFile(card()->dev + "/power/runtime_status").trimmed() == "suspended") {
        live_->setText(QStringLiteral("asleep (runtime suspended)"));
        return;
    }
    const Live l = readLive(*card());
    QStringList p;
    if (l.sclk) p << QStringLiteral("GPU %1 MHz").arg(*l.sclk);
    if (l.mclk) p << QStringLiteral("MEM %1 MHz").arg(*l.mclk);
    if (l.busy) p << QStringLiteral("load %1 %").arg(*l.busy);
    if (l.vddgfxMv) p << QStringLiteral("%1 mV").arg(*l.vddgfxMv);
    if (l.powerW) p << QStringLiteral("%1 W").arg(*l.powerW, 0, 'f', 1);
    if (l.edgeC) p << QStringLiteral("edge %1 °C").arg(*l.edgeC, 0, 'f', 0);
    if (l.junctionC) p << QStringLiteral("hotspot %1 °C").arg(*l.junctionC, 0, 'f', 0);
    if (l.fanRpm) p << QStringLiteral("fan %1 rpm").arg(*l.fanRpm);
    live_->setText(p.isEmpty() ? QStringLiteral("—") : p.join(QStringLiteral("   ·   ")));
}

// ── apply / trial / reset ───────────────────────────────────────────────────

void AmdGpuTab::setBusy(bool b) {
    busy_ = b;
    for (QPushButton *w : {applyBtn_, resetBtn_, uvBtn_}) w->setEnabled(!b);
    cardCombo_->setEnabled(!b && !trialBar_->isVisible());
}

void AmdGpuTab::status(const QString &msg, const char *color) {
    status_->setStyleSheet(color ? QStringLiteral("color: %1;").arg(QLatin1String(color)) : QString());
    status_->setText(msg);
}

void AmdGpuTab::apply(const QJsonObject &settings, bool trial) {
    if (busy_ || !card()) return;
    if (trial && !trialBar_->isVisible()) before_ = settingsOf(read(*card()));
    setBusy(true);
    status(QStringLiteral("Applying…"));
    const QJsonObject req{{"op", "apply"}, {"card", card()->name}, {"settings", settings}};
    privileged::run(privileged::helperPath(HELPER), req, this, [this, trial](const privileged::Result &r) {
        setBusy(false);
        snap_ = read(*card());
        if (!r.ok()) {
            status(QStringLiteral("✗ ") + r.message(), theme::DANGER);
            fill(settingsOf(snap_));
            return;
        }
        QStringList done;
        for (const QJsonValue &v : r.json.value(QStringLiteral("applied")).toArray()) done << v.toString();
        status(QStringLiteral("✓ ") + done.join(QStringLiteral(" · ")), theme::OK);
        fill(settingsOf(snap_));
        if (trial) startTrial();
    });
}

void AmdGpuTab::startTrial() {
    trialLeft_ = REVERT_SECONDS;
    trialLabel_->setText(QStringLiteral("Testing new settings — reverting in %1 s unless you keep them. "
                                        "Run your game or a stress test now.").arg(trialLeft_));
    trialBar_->show();
    cardCombo_->setEnabled(false);
    trialTimer_->start();
}

void AmdGpuTab::endTrial(bool keep) {
    trialTimer_->stop();
    trialBar_->hide();
    cardCombo_->setEnabled(!busy_);
    if (keep) {
        const QJsonObject kept = settingsOf(snap_);
        QString summary;
        if (kept.contains(QStringLiteral("voltage_offset"))) summary = QStringLiteral("%1 mV").arg(kept.value(QStringLiteral("voltage_offset")).toInt());
        if (kept.contains(QStringLiteral("sclk_max"))) summary += QStringLiteral("%1max %2 MHz").arg(summary.isEmpty() ? "" : ", ").arg(kept.value(QStringLiteral("sclk_max")).toInt());
        if (kept.contains(QStringLiteral("power_cap_w"))) summary += QStringLiteral("%1%2 W").arg(summary.isEmpty() ? "" : ", ").arg(kept.value(QStringLiteral("power_cap_w")).toInt());
        QSettings(storePath(), QSettings::IniFormat).setValue(QStringLiteral("amdgpu/stable/") + card()->pciId, summary);
        stableLabel_->setText(QStringLiteral("Last kept: ") + summary);
        status(QStringLiteral("✓ Kept. Save it as a profile to reuse it."), theme::OK);
        return;
    }
    status(QStringLiteral("Reverting to the previous settings…"), theme::WARN);
    apply(before_, false);
}

void AmdGpuTab::resetCard() {
    if (busy_ || !card()) return;
    if (trialBar_->isVisible()) { trialTimer_->stop(); trialBar_->hide(); }
    setBusy(true);
    privileged::run(privileged::helperPath(HELPER), QJsonObject{{"op", "reset"}, {"card", card()->name}}, this,
                    [this](const privileged::Result &r) {
        setBusy(false);
        snap_ = read(*card());
        fill(settingsOf(snap_));
        if (!r.ok()) { status(QStringLiteral("✗ ") + r.message(), theme::DANGER); return; }
        // Fresh stock reference for the presets.
        QSettings(storePath(), QSettings::IniFormat).setValue(QStringLiteral("amdgpu/stock/") + card()->pciId + '/' + card()->name,
                             QString::fromUtf8(QJsonDocument(settingsOf(snap_)).toJson(QJsonDocument::Compact)));
        status(QStringLiteral("✓ Back to stock"), theme::OK);
    });
}

void AmdGpuTab::undervoltStep() {
    if (!voltOffset_ || busy_) return;
    if (voltOffset_->value() - UV_STEP_MV < voltOffset_->minimum()) {
        status(QStringLiteral("Already at the driver's lowest offset."), theme::WARN);
        return;
    }
    // Stepping from a kept state: the trial reverts to the last step that held.
    if (trialBar_->isVisible()) endTrial(true);
    voltOffset_->setValue(voltOffset_->value() - UV_STEP_MV);
    apply(collect(), true);
}

void AmdGpuTab::applyPreset(int which) {
    const QString raw = QSettings(storePath(), QSettings::IniFormat).value(QStringLiteral("amdgpu/stock/") + card()->pciId + '/' + card()->name).toString();
    QJsonObject s = QJsonDocument::fromJson(raw.toUtf8()).object();
    if (s.isEmpty()) s = settingsOf(snap_);
    s[QStringLiteral("perf_level")] = QStringLiteral("auto");
    for (const Profile &p : snap_.profiles)
        if (p.name == QLatin1String(which == 3 ? "3D_FULL_SCREEN" : "BOOTUP_DEFAULT")) s[QStringLiteral("profile")] = p.index;
    if (which == 2) {  // Efficiency
        if (snap_.capDefaultW && snap_.capMinW) s[QStringLiteral("power_cap_w")] = std::max(*snap_.capMinW, std::round(*snap_.capDefaultW * 0.85));
        if (s.contains(QStringLiteral("sclk_max"))) s[QStringLiteral("sclk_max")] = int(s.value(QStringLiteral("sclk_max")).toInt() * 0.9);
        if (s.contains(QStringLiteral("voltage_offset"))) s[QStringLiteral("voltage_offset")] = std::min(s.value(QStringLiteral("voltage_offset")).toInt(), -30);
    } else if (which == 3 && snap_.capMaxW) {  // Performance
        s[QStringLiteral("power_cap_w")] = std::floor(*snap_.capMaxW);
    }
    fill(s);
    status(QStringLiteral("Preset loaded into the form — review it, then Apply."));
}

// ── profiles ────────────────────────────────────────────────────────────────

QStringList AmdGpuTab::savedProfileNames() const {
    QStringList out;
    for (const QString &f : QDir(profilesDir()).entryList({QStringLiteral("*.json")}, QDir::Files, QDir::Name)) out << f.chopped(5);
    return out;
}

void AmdGpuTab::reloadProfiles() {
    profileCombo_->clear();
    profileCombo_->addItems(savedProfileNames());
}

void AmdGpuTab::saveProfile() {
    if (!card()) return;
    bool ok = false;
    const QString name = QInputDialog::getText(this, QStringLiteral("Save profile"), QStringLiteral("Name:"), QLineEdit::Normal,
                                               profileCombo_->currentText(), &ok).trimmed();
    static const QRegularExpression valid(QStringLiteral("^[\\w .-]{1,40}$"));
    if (!ok || name.isEmpty()) return;
    if (!valid.match(name).hasMatch() || name.startsWith('.')) { status(QStringLiteral("✗ Use letters, digits, space, . _ -"), theme::DANGER); return; }
    QFile f(profilesDir() + '/' + name + QStringLiteral(".json"));
    const QJsonObject doc{{"pci_id", card()->pciId}, {"settings", collect()}};
    if (!f.open(QIODevice::WriteOnly | QIODevice::Truncate) || f.write(QJsonDocument(doc).toJson()) < 0) {
        status(QStringLiteral("✗ Could not write ") + f.fileName(), theme::DANGER);
        return;
    }
    reloadProfiles();
    profileCombo_->setCurrentText(name);
    status(QStringLiteral("✓ Saved \"%1\"").arg(name), theme::OK);
}

void AmdGpuTab::loadProfile() {
    const QString name = profileCombo_->currentText();
    if (name.isEmpty() || !card()) return;
    const QJsonObject doc = QJsonDocument::fromJson(readFile(profilesDir() + '/' + name + QStringLiteral(".json"))).object();
    if (doc.value(QStringLiteral("pci_id")).toString() != card()->pciId
        && QMessageBox::question(this, QStringLiteral("Different GPU"),
               QStringLiteral("\"%1\" was saved for GPU %2, this is %3. Load it anyway? Values are clamped to this card's ranges.")
                   .arg(name, doc.value(QStringLiteral("pci_id")).toString(), card()->pciId)) != QMessageBox::Yes)
        return;
    fill(doc.value(QStringLiteral("settings")).toObject());
    status(QStringLiteral("Loaded \"%1\" into the form — Apply to use it.").arg(name));
}

void AmdGpuTab::deleteProfile() {
    const QString name = profileCombo_->currentText();
    if (name.isEmpty() || QMessageBox::question(this, QStringLiteral("Delete profile"), QStringLiteral("Delete \"%1\"?").arg(name)) != QMessageBox::Yes) return;
    QFile::remove(profilesDir() + '/' + name + QStringLiteral(".json"));
    reloadProfiles();
}
