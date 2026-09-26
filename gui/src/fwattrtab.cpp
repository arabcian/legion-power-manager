#include "fwattrtab.h"
#include <QFileInfo>
#include <QStandardPaths>
#include <QSaveFile>
#include <QShowEvent>
#include "platformprofile.h"
#include "privileged.h"
#include "theme.h"

#include <QDir>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QMap>
#include <QMessageBox>
#include <QPushButton>
#include <QRegularExpression>
#include <QScrollArea>
#include <QSlider>
#include <QSpinBox>
#include <QTabWidget>
#include <QTimer>
#include <QVBoxLayout>

// LPM_FWATTR_BASE: dev-only override for rendering against a fake tree. The
// helper still refuses any path outside /sys/class/firmware-attributes.
static const QString BASE = qEnvironmentVariableIsSet("LPM_FWATTR_BASE")
    ? qEnvironmentVariable("LPM_FWATTR_BASE") : QStringLiteral("/sys/class/firmware-attributes");

static QString helperPath() { return privileged::helperPath(QStringLiteral("fwattr-helper")); }
static QString gpuHelperPath() { return privileged::helperPath(QStringLiteral("legion-gpu-helper")); }

// Attributes lenovo-wmi-other lists with min=max=step=0: the kernel rejects any
// sysfs write to them (EINVAL). They are written through \_SB.GZFD.WMAE instead.
// Ranges: Dynamic Boost ceiling/floor 0..25 W (as Legion Space shows them);
// cTGP deliberately left wide (0..250 sanity cap) for testing.
// Lower bound 1, never 0: writing 0 makes the firmware treat the feature as off
// and the kernel drops the attribute from sysfs (only a Windows power-profile
// reset brought it back). The helpers refuse 0 as well.
struct WmiKnob { const char *attr, *key; int lo, hi; };
static const WmiKnob WMI_KNOBS[] = {
    {"gpu_nv_ctgp", "ctgp", 1, 250},
    {"gpu_nv_ppab", "boost_up", 1, 25},
    {"gpu_nv_cpu_boost", "boost_down", 1, 25},
};
// Sysfs-backed limits that legion-gpu-helper can also reach over WMAE; used
// only when the attribute has vanished from sysfs AND the WMAE read-back was
// seen to match sysfs on this machine (recorded in the range cache).
static const std::pair<const char *, const char *> WMAE_FALLBACK[] = {
    {"ppt_pl1_spl", "spl"}, {"ppt_pl2_sppt", "sppt"}, {"ppt_pl3_fppt", "fppt"}, {"cpu_temp", "cpu_temp"},
};

/// Last seen firmware ranges, so a row can still be drawn after its attribute
/// disappeared from sysfs. ~/.cache/legion-power-manager/fwattr-ranges.json
static QString rangeCacheFile() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericCacheLocation) + QStringLiteral("/legion-power-manager/fwattr-ranges.json");
}
static QJsonObject readRangeCache() {
    QFile f(rangeCacheFile());
    return f.open(QIODevice::ReadOnly) && f.size() < 256 * 1024 ? QJsonDocument::fromJson(f.readAll()).object() : QJsonObject();
}
static void writeRangeCache(const QJsonObject &o) {
    QDir().mkpath(QFileInfo(rangeCacheFile()).absolutePath());
    QSaveFile f(rangeCacheFile());
    if (f.open(QIODevice::WriteOnly)) { f.write(QJsonDocument(o).toJson()); f.commit(); }
}
/// The GameZone "other method" WMI interface (WMAE) is present.
static bool wmaeAvailable() {
    return !QDir(QStringLiteral("/sys/bus/wmi/devices")).entryList({"DC2A8805-3A8C-41BA-A6F7-092E0089CD3B*"},
                                                                  QDir::Dirs | QDir::System | QDir::NoDotAndDotDot).isEmpty();
}

static std::optional<int> readInt(const QString &p) {
    auto s = pp::readText(p);
    bool ok = false;
    int v = s ? s->toInt(&ok) : 0;
    return ok ? std::optional(v) : std::nullopt;
}

QList<FwAttr> FwattrTab::discover() {
    QList<FwAttr> out;
    const QDir base(BASE);
    for (const QString &dev : base.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name)) {
        const QDir attrs(base.filePath(dev) + QStringLiteral("/attributes"));
        for (const QString &name : attrs.entryList(QDir::Dirs | QDir::NoDotAndDotDot, QDir::Name)) {
            const QString a = attrs.filePath(name) + '/';
            auto cur = readInt(a + "current_value"), def = readInt(a + "default_value"),
                 mn = readInt(a + "min_value"), mx = readInt(a + "max_value"), st = readInt(a + "scalar_increment");
            auto disp = pp::readText(a + "display_name");
            if (!cur || !def || !mn || !mx || !st || !disp) continue;
            FwAttr f;
            f.device = dev; f.name = name; f.path = a + "current_value"; f.displayName = *disp;
            f.current = *cur; f.def = *def; f.min = *mn; f.max = *mx; f.step = std::max(*st, 1);
            // Some BIOS revisions report min=max=step=0 (gpu_nv_ctgp etc.): no usable range.
            f.ranged = !(*mn == 0 && *mx == 0 && *st == 0);
            if (!f.ranged)
                for (const WmiKnob &k : WMI_KNOBS)
                    if (name == QLatin1String(k.attr)) { f.wmiKey = QString::fromLatin1(k.key); f.wmiMin = k.lo; f.wmiMax = k.hi; }
            out.append(f);
        }
    }

    // Attributes missing from sysfs that are still reachable over WMAE.
    if (!wmaeAvailable()) return out;
    auto have = [&](const char *attr) { return std::any_of(out.cbegin(), out.cend(), [&](const FwAttr &a) { return a.name == QLatin1String(attr); }); };
    const QJsonObject cache = readRangeCache();
    const QString dev = out.isEmpty() ? cache.value("_device").toString(QStringLiteral("lenovo-wmi-other-0")) : out.first().device;
    for (const WmiKnob &k : WMI_KNOBS) {
        if (have(k.attr)) continue;
        const QJsonObject c = cache.value(QLatin1String(k.attr)).toObject();
        FwAttr f;
        f.device = dev; f.name = QString::fromLatin1(k.attr);
        f.displayName = c.value("display").toString(f.name) + QStringLiteral("  (missing from sysfs — via WMI)");
        f.ranged = false; f.wmiKey = QString::fromLatin1(k.key); f.wmiMin = k.lo; f.wmiMax = k.hi;
        out.append(f);
    }
    for (const auto &[attr, key] : WMAE_FALLBACK) {
        const QJsonObject c = cache.value(QLatin1String(attr)).toObject();
        if (have(attr) || !c.value("wmae_verified").toBool()) continue;
        FwAttr f;
        f.device = dev; f.name = QString::fromLatin1(attr);
        f.displayName = c.value("display").toString(f.name) + QStringLiteral("  (missing from sysfs — via WMI)");
        f.ranged = false; f.wmiKey = QString::fromLatin1(key);
        f.wmiMin = std::max(1, c.value("min").toInt(1)); f.wmiMax = std::max(f.wmiMin, c.value("max").toInt(f.wmiMin));
        f.def = c.value("default").toInt(f.wmiMin);
        out.append(f);
    }
    return out;
}

static QString category(const QString &n) {
    if (n.startsWith("cpu") || n.startsWith("ppt_cpu") || n.startsWith("ppt_pl")) return QStringLiteral("CPU");
    if (n.startsWith("gpu") || n.startsWith("dgpu")) return QStringLiteral("GPU");
    return QStringLiteral("Other");
}

FwattrTab::FwattrTab(QWidget *parent) : QWidget(parent) {
    auto *lay = new QVBoxLayout(this);
    lay->setContentsMargins(12, 10, 12, 10);
    lay->setSpacing(6);

    banner_ = new QLabel;
    banner_->setWordWrap(true);
    banner_->setStyleSheet(QStringLiteral(
        "background: %1; border: 1px solid %2; border-left: 3px solid %3; border-radius: %4px;"
        " padding: 5px 9px; color: %5;").arg(theme::BG2, theme::ACCENT_SOFT, theme::ACCENT)
        .arg(theme::RADIUS).arg(theme::FG_DIM));
    banner_->hide();
    lay->addWidget(banner_);

    // Firmware CPU OC (LENOVO_CPU_METHOD via legion-gpu-helper). Same store as the
    // BIOS setup page; applied by the firmware at the next boot.
    {
        auto *oc = new QGroupBox(QStringLiteral("CPU overclocking (firmware, next boot)"));
        auto *ol = new QHBoxLayout(oc);
        ol->setSpacing(10);
        auto *note = new QLabel;
        note->setProperty("role", "muted");
        oc->hide();
        lay->addWidget(oc);
        privileged::run(gpuHelperPath(), QJsonObject{{"op", "fw_oc"}}, this, [this, oc, ol, note](const privileged::Result &r) {
            if (!r.ok()) {
                if (r.message().contains(QLatin1String("unknown"))) return;  // not a Legion / old helper
                ol->addWidget(note); note->setWordWrap(true);
                note->setText(QStringLiteral("Firmware OC unavailable: ") + r.message());
                oc->show();
                return;
            }
            const QJsonObject t = r.json.value("tunes").toObject();
            const int mode = r.json.value("bios_oc_mode").toInt(-1);
            static const struct { const char *key, *text, *tip; } DEFS[] = {
                {"pbo_scalar", "PBO scalar", "Precision Boost Overdrive scalar (x)."},
                {"boost_mhz", "Boost override (MHz)", "Max CPU boost clock override, added to stock fmax."},
                {"curve_optimizer", "All-core CO", "Firmware all-core Curve Optimizer; negative = undervolt.\nRuntime per-core CO stays in the Ryzen tab."},
            };
            for (const auto &d : DEFS) {
                if (!t.contains(d.key)) continue;
                const QJsonObject v = t.value(d.key).toObject();
                auto *lbl = new QLabel(d.text);
                auto *sb = new QSpinBox;
                sb->setRange(v.value("min").toInt(), v.value("max").toInt());
                sb->setValue(v.value("value").toInt());
                sb->setToolTip(d.tip);
                auto *btn = new QPushButton("Set");
                btn->setFixedWidth(46);
                btn->setToolTip("Stored in the BIOS, applied at the next boot. Asks for the administrator password.");
                const QString key = d.key;
                connect(btn, &QPushButton::clicked, this, [this, sb, btn, key, note] {
                    btn->setEnabled(false);
                    privileged::run(privileged::helperPath(privileged::FIRMWARE_HELPER),
                                    QJsonObject{{"op", "set_fw_oc"}, {"key", key}, {"value", sb->value()}}, this,
                                    [btn, note, key](const privileged::Result &r) {
                        btn->setEnabled(true);
                        note->setText(r.ok() ? key + QStringLiteral(" saved — reboot to apply") : key + QStringLiteral(" failed: ") + r.message());
                    }, privileged::FIRMWARE_TIMEOUT_MS);
                });
                ol->addWidget(lbl); ol->addWidget(sb); ol->addWidget(btn); ol->addSpacing(8);
            }
            ol->addStretch(1);
            ol->addWidget(note);
            note->setText(mode == 0 ? QStringLiteral("OC is disabled in BIOS setup — values are ignored")
                                    : QStringLiteral("Takes effect after reboot"));
            oc->show();
        });
    }

    auto *top = new QHBoxLayout;
    auto *src = new QLabel(BASE);
    src->setProperty("role", "muted");
    top->addWidget(src);
    top->addStretch();
    rescan_ = new QPushButton("Rescan");
    connect(rescan_, &QPushButton::clicked, this, &FwattrTab::rebuild);
    top->addWidget(rescan_);
    applyAll_ = new QPushButton("Apply All");
    applyAll_->setObjectName("btnAccent");
    connect(applyAll_, &QPushButton::clicked, this, &FwattrTab::applyAll);
    top->addWidget(applyAll_);
    lay->addLayout(top);

    tabs_ = new QTabWidget;
    tabs_->setDocumentMode(true);
    lay->addWidget(tabs_);

    status_ = new QLabel;
    status_->setProperty("role", "muted");
    lay->addWidget(status_);
    statusTimer_ = new QTimer(this);
    statusTimer_->setSingleShot(true);
    connect(statusTimer_, &QTimer::timeout, status_, &QLabel::clear);

    rebuild();
    // Lock state follows profile-change notifications instead of a 2 s poll.
    connect(&pp::Watcher::instance(), &pp::Watcher::changed, this, &FwattrTab::refreshLockState);
    refreshLockState();
}

void FwattrTab::showStatus(const QString &m, int t) {
    status_->setText(m);
    if (t) statusTimer_->start(t);
}

void FwattrTab::setBusy(bool b) {
    busy_ = b;
    tabs_->setEnabled(!b && !locked_);
    applyAll_->setEnabled(!b && !locked_);
    rescan_->setEnabled(!b);
}

void FwattrTab::refreshLockState() {
    // Firmware only honours these writes in Custom. No platform-profile
    // interface at all → nothing to lock against.
    // Cached handler + value (the old code rescanned /sys/class/platform-profile every 2 s).
    pp::Watcher &w = pp::Watcher::instance();
    w.check();
    const auto profile = w.current();
    const bool locked = profile && *profile != QLatin1String("custom");
    if (locked == locked_) return;
    locked_ = locked;
    setBusy(busy_);
    if (locked) {
        QString label = *profile;
        label.replace('-', ' ');
        if (!label.isEmpty()) label[0] = label[0].toUpper();
        banner_->setText(QStringLiteral(
            "Power profile is currently \"%1\", not Custom. The firmware only accepts changes to these "
            "attributes in Custom mode, so editing is disabled here. Switch to Custom on the Home tab to unlock it.")
            .arg(label));
        banner_->show();
    } else {
        banner_->hide();
    }
}

void FwattrTab::rebuild() {
    if (busy_) return;
    while (tabs_->count()) { QWidget *w = tabs_->widget(0); tabs_->removeTab(0); w->deleteLater(); }
    rows_.clear();

    const QList<FwAttr> attrs = discover();
    {   // Remember the firmware ranges while the attributes are there.
        QJsonObject cache = readRangeCache();
        for (const FwAttr &a : attrs) {
            if (a.path.isEmpty()) continue;  // synthesized (missing) row
            QJsonObject c = cache.value(a.name).toObject();
            c["display"] = a.displayName;
            if (a.ranged) { c["min"] = a.min; c["max"] = a.max; c["default"] = a.def; }
            cache[a.name] = c;
            cache["_device"] = a.device;
        }
        writeRangeCache(cache);
    }
    if (attrs.isEmpty()) { tabs_->addTab(new QLabel("No firmware-attributes device found."), QStringLiteral("—")); return; }

    QMap<QString, QList<FwAttr>> byDev;
    for (const FwAttr &a : attrs) byDev[a.device].append(a);

    for (auto it = byDev.cbegin(); it != byDev.cend(); ++it) {
        auto *scroll = new QScrollArea;
        scroll->setWidgetResizable(true);
        auto *page = new QWidget;
        auto *pl = new QVBoxLayout(page);
        pl->setContentsMargins(2, 4, 2, 2);
        pl->setSpacing(6);

        for (const QString &group : {QStringLiteral("CPU"), QStringLiteral("GPU"), QStringLiteral("Other")}) {
            QList<FwAttr> items;
            for (const FwAttr &a : *it) if (category(a.name) == group) items.append(a);
            if (items.isEmpty()) continue;
            auto *box = new QGroupBox(group);
            auto *g = new QGridLayout(box);
            g->setHorizontalSpacing(8);
            g->setVerticalSpacing(2);
            g->setColumnStretch(1, 1);
            int r = 0;
            for (const FwAttr &a : items) {
                const int index = rows_.size();
                auto *label = new QLabel(a.displayName);
                QString tip = QStringLiteral("%1\nmin=%2  max=%3  step=%4  default=%5")
                                  .arg(a.name).arg(a.min).arg(a.max).arg(a.step).arg(a.def);
                if (a.viaWmi()) tip += QStringLiteral("\n⚙ no firmware range — written via Lenovo WMI (acpi_call), range %1–%2").arg(a.wmiMin).arg(a.wmiMax);
                else if (!a.ranged) tip += QStringLiteral("\n⚠ firmware reports no valid range for this attribute");
                label->setToolTip(tip);
                g->addWidget(label, r, 0);

                QSlider *slider = nullptr;
                const int lo = std::max(1, a.viaWmi() ? a.wmiMin : a.min), hi = std::max(lo, a.viaWmi() ? a.wmiMax : a.max);
                if (a.ranged || a.viaWmi()) {
                    slider = new QSlider(Qt::Horizontal);
                    slider->setRange(lo, hi);
                    slider->setSingleStep(a.viaWmi() ? 1 : a.step);
                    slider->setPageStep(a.viaWmi() ? 1 : a.step);
                    slider->setValue(a.current);
                    if (a.viaWmi()) slider->setStyleSheet(QStringLiteral("QSlider::sub-page:horizontal { background: %1; }").arg(theme::PURPLE));
                    g->addWidget(slider, r, 1);
                } else {
                    auto *w = new QLabel(QStringLiteral("⚠ no range"));
                    w->setStyleSheet(QStringLiteral("color: %1;").arg(theme::WARN));
                    g->addWidget(w, r, 1);
                }
                auto *spin = new QSpinBox;
                spin->setFixedWidth(70);
                if (a.viaWmi()) spin->setRange(std::max(1, a.wmiMin), std::max(1, a.wmiMax));
                else if (a.ranged) { spin->setRange(std::max(1, a.min), std::max(1, a.max)); spin->setSingleStep(a.step); }
                else spin->setRange(1, std::max({1, a.current, a.def}));
                spin->setValue(a.current);
                g->addWidget(spin, r, 2);
                if (slider) {
                    connect(slider, &QSlider::valueChanged, spin, &QSpinBox::setValue);
                    connect(spin, &QSpinBox::valueChanged, slider, &QSlider::setValue);
                }
                auto *apply = new QPushButton("Apply");
                apply->setMinimumWidth(64);
                connect(apply, &QPushButton::clicked, this, [this, index] { applyRow(index); });
                g->addWidget(apply, r, 3);
                auto *def = new QPushButton("Default");
                def->setMinimumWidth(72);
                connect(def, &QPushButton::clicked, spin, [spin, d = a.def] { spin->setValue(d); });
                g->addWidget(def, r, 4);
                // No authoritative range → nothing safe to offer: whole row disabled.
                if (!a.ranged && !a.viaWmi()) { spin->setEnabled(false); apply->setEnabled(false); def->setEnabled(false); }
                if (a.viaWmi()) { spin->setEnabled(false); if (slider) slider->setEnabled(false); apply->setEnabled(false); }  // until readWmi confirms acpi_call
                rows_.append({a, slider, spin, apply, def});
                ++r;
            }
            // Explain the WMI rows right under the box that contains them.
            bool anyWmi = false;
            for (const FwAttr &a : items) anyWmi |= a.viaWmi();
            if (anyWmi) {
                auto *note = new QLabel(QStringLiteral(
                    "<span style='color:%1'>⚙</span> <b>cTGP</b>, <b>power performance aware boost</b> and <b>GPU to CPU dynamic "
                    "boost</b> are reported by the firmware without a valid range (min = max = 0), so the kernel refuses "
                    "to write them through sysfs. These three are written directly through the Lenovo WMI method "
                    "(<tt>\\_SB.GZFD.WMAE</tt>) with <tt>acpi_call</tt> and read back from it. Boost limits use 1–25 W (0 is never written: the firmware would drop the attribute); "
                    "cTGP is left unclamped for testing — the GPU itself caps it (150 W on this model). "
                    "Needs the <tt>acpi_call</tt> module.").arg(theme::PURPLE));
                note->setTextFormat(Qt::RichText);
                note->setWordWrap(true);
                note->setProperty("role", "muted");
                g->addWidget(note, r++, 0, 1, 5);
            }
            pl->addWidget(box);
        }
        pl->addStretch();
        scroll->setWidget(page);
        tabs_->addTab(scroll, it.key());
    }
    setBusy(false);
    // WMI rows are read through pkexec + acpi_call: only once the tab is
    // actually shown. The constructor runs at login with the app hidden in the
    // tray, where it used to cost a root helper run (and, in a session polkit
    // does not treat as active, a password prompt out of nowhere).
    wmiStale_ = true;
    if (isVisible()) readWmi();
}

void FwattrTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    if (wmiStale_) readWmi();
}

/// Value snapped onto the driver's min + k·step grid (the helper checks the
/// range but not the step; an off-grid value is rounded by the WMI method
/// in firmware-specific ways, so it's never sent).
int FwattrTab::snapped(const Row &r) const {
    const int v = r.spin->value();
    if (r.info.viaWmi() || !r.info.ranged || r.info.step <= 1) return v;
    const int k = qRound(double(v - r.info.min) / r.info.step);
    return std::clamp(r.info.min + k * r.info.step, r.info.min, r.info.max);
}

void FwattrTab::applyRow(int index) {
    if (busy_ || locked_ || index < 0 || index >= rows_.size()) return;
    if (rows_[index].info.viaWmi()) { applyWmi({index}); return; }
    if (!rows_[index].info.ranged) return;
    const int value = snapped(rows_[index]);
    rows_[index].spin->setValue(value);
    const QString name = rows_[index].info.name, path = rows_[index].info.path;
    setBusy(true);
    privileged::run(helperPath(), QJsonObject{{"path", path}, {"value", value}}, this,
        [this, index, value, name](const privileged::Result &r) {
            setBusy(false);
            if (!r.reached) { QMessageBox::critical(this, "Authorization failed", r.error); return; }
            if (!r.ok()) {
                QMessageBox::critical(this, "Error", name + " not written:\n" + (r.message().isEmpty() ? "unknown error" : r.message()));
                return;
            }
            if (index < rows_.size()) rows_[index].info.current = value;
            showStatus(QStringLiteral("%1 = %2 written").arg(name).arg(value));
        });
}

void FwattrTab::applyAll() {
    if (busy_ || locked_) return;
    QList<int> changed, wmiChanged;
    QJsonArray items;
    for (int i = 0; i < rows_.size(); ++i) {
        Row &r = rows_[i];
        if (r.info.viaWmi()) { if (wmiReady_ && r.spin->value() != r.info.current) wmiChanged << i; continue; }
        if (!r.info.ranged) continue;
        const int v = snapped(r);
        r.spin->setValue(v);
        if (v == r.info.current) continue;
        changed << i;
        items.append(QJsonObject{{"path", r.info.path}, {"value", v}});
    }
    if (changed.isEmpty() && wmiChanged.isEmpty()) { QMessageBox::information(this, "Apply All", "No changes to apply."); return; }
    if (changed.isEmpty()) { applyWmi(wmiChanged); return; }

    setBusy(true);
    privileged::run(helperPath(), QJsonDocument(items).toJson(QJsonDocument::Compact), this,
        [this, changed, wmiChanged](const privileged::Result &r) {
            setBusy(false);
            if (!r.reached) { QMessageBox::critical(this, "Authorization failed", r.error); return; }
            const QJsonArray results = r.json.value("results").toArray();
            QStringList failures;
            int applied = 0;
            for (int k = 0; k < changed.size(); ++k) {
                Row &row = rows_[changed[k]];
                const QJsonObject res = results.at(k).toObject();
                if (res.value("ok").toBool()) { row.info.current = row.spin->value(); ++applied; }
                else failures << row.info.name + ": " + res.value("error").toString(r.message().isEmpty() ? "unknown error" : r.message());
            }
            if (failures.isEmpty()) showStatus(QStringLiteral("Applied %1 attribute(s)").arg(applied));
            else QMessageBox::warning(this, "Some writes failed",
                QStringLiteral("%1/%2 attribute(s) applied.\n\nFailures:\n").arg(applied).arg(changed.size()) + failures.join('\n'));
            if (!wmiChanged.isEmpty()) applyWmi(wmiChanged);
        });
}

void FwattrTab::applyWmi(const QList<int> &indices) {
    if (busy_ || locked_ || indices.isEmpty()) return;
    if (!wmiReady_) {
        QMessageBox::warning(this, "acpi_call needed", "These attributes are written through acpi_call, which is not available.\n"
                             "Load it with: modprobe acpi_call (Gentoo: emerge sys-power/acpi_call).");
        return;
    }
    QJsonObject vals;
    for (int i : indices) vals[rows_[i].info.wmiKey] = rows_[i].spin->value();
    setBusy(true);
    privileged::run(gpuHelperPath(), QJsonObject{{"op", "apply"}, {"values", vals}}, this,
        [this, indices](const privileged::Result &r) {
            setBusy(false);
            if (!r.reached) { QMessageBox::critical(this, "Authorization failed", r.error); return; }
            QStringList fails, oks;
            for (const auto &x : r.json.value("results").toArray()) {
                const QJsonObject o = x.toObject();
                (o.value("ok").toBool() ? oks : fails) << o.value("message").toString();
            }
            if (!fails.isEmpty()) QMessageBox::warning(this, "WMI write failed", fails.join('\n'));
            if (!oks.isEmpty()) showStatus(oks.join(" · "), 5000);
            readWmi();  // show what the EC holds now
        });
}

void FwattrTab::readWmi() {
    bool any = false;
    for (const Row &r : std::as_const(rows_)) any |= r.info.viaWmi();
    if (!any || busy_) return;
    wmiStale_ = false;
    privileged::run(gpuHelperPath(), QJsonObject{{"op", "status"}}, this, [this](const privileged::Result &r) {
        wmiReady_ = r.reached && r.json.value("acpi").toBool();
        const QJsonObject vals = r.json.value("values").toObject();
        {   // WMAE id check for the CPU limits: the fallback is only offered
            // once WMAE has been seen to report exactly what sysfs reports.
            QJsonObject cache = readRangeCache();
            bool changed = false;
            for (auto it = vals.begin(); it != vals.end(); ++it) {
                const QJsonObject o = it.value().toObject();
                if (!o.contains("wmae") || !o.value("present").toBool() || !o.contains("value")) continue;
                const QString attr = o.value("attr").toString();
                const bool match = o.value("wmae").toInt() == o.value("value").toInt();
                QJsonObject c = cache.value(attr).toObject();
                if (c.value("wmae_verified").toBool() != match) { c["wmae_verified"] = match; cache[attr] = c; changed = true; }
                if (!match) showStatus(QStringLiteral("⚠ %1: WMAE reads %2 but sysfs %3 — its WMI fallback stays off.")
                                           .arg(attr).arg(o.value("wmae").toInt()).arg(o.value("value").toInt()), 10000);
            }
            if (changed) writeRangeCache(cache);
        }
        for (Row &row : rows_) {
            if (!row.info.viaWmi()) continue;
            const QJsonObject o = vals.value(row.info.wmiKey).toObject();
            const bool have = wmiReady_ && o.contains("value");
            if (have) {
                const int v = o.value("value").toInt();
                row.info.current = v;
                // cTGP can hold values above its (test) range; widen rather than clip the display.
                if (v > row.spin->maximum()) { row.spin->setMaximum(v); if (row.slider) row.slider->setMaximum(v); }
                row.spin->setValue(v);
            }
            row.spin->setEnabled(have);
            if (row.slider) row.slider->setEnabled(have);
            if (row.apply) row.apply->setEnabled(have);
            if (row.def) row.def->setEnabled(have);
            if (!have) row.spin->setToolTip(wmiReady_ ? o.value("error").toString() : QStringLiteral("acpi_call not loaded"));
        }
        if (!wmiReady_ && r.reached) showStatus("acpi_call not loaded — the three WMI-handled GPU attributes are read-only (modprobe acpi_call).", 8000);
    });
}

QMap<QString, int> FwattrTab::wmiValues() const {
    QMap<QString, int> out;
    if (!wmiReady_ || wmiStale_) return out;
    for (const Row &r : rows_) if (r.info.viaWmi()) out.insert(r.info.name, r.info.current);
    return out;
}
