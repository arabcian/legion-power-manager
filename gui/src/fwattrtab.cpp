#include "fwattrtab.h"
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
static constexpr int PROFILE_POLL_MS = 2000;

static QString helperPath() { return privileged::helperPath(QStringLiteral("fwattr-helper")); }

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
            out.append(f);
        }
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
    auto *poll = new QTimer(this);
    connect(poll, &QTimer::timeout, this, &FwattrTab::refreshLockState);
    poll->start(PROFILE_POLL_MS);
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
    const auto profile = pp::currentProfile(pp::primaryHandler());
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
                if (!a.ranged) tip += QStringLiteral("\n⚠ firmware reports no valid range for this attribute");
                label->setToolTip(tip);
                g->addWidget(label, r, 0);

                QSlider *slider = nullptr;
                if (a.ranged) {
                    slider = new QSlider(Qt::Horizontal);
                    slider->setRange(a.min, a.max);
                    slider->setSingleStep(a.step);
                    slider->setPageStep(a.step);
                    slider->setValue(a.current);
                    g->addWidget(slider, r, 1);
                } else {
                    auto *w = new QLabel(QStringLiteral("⚠ no range"));
                    w->setStyleSheet(QStringLiteral("color: %1;").arg(theme::WARN));
                    g->addWidget(w, r, 1);
                }
                auto *spin = new QSpinBox;
                spin->setFixedWidth(70);
                if (a.ranged) { spin->setRange(a.min, a.max); spin->setSingleStep(a.step); }
                else spin->setRange(0, std::max(a.current, a.def));
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
                if (!a.ranged) { spin->setEnabled(false); apply->setEnabled(false); def->setEnabled(false); }
                rows_.append({a, slider, spin});
                ++r;
            }
            pl->addWidget(box);
        }
        pl->addStretch();
        scroll->setWidget(page);
        tabs_->addTab(scroll, it.key());
    }
    setBusy(false);
}

/// Value snapped onto the driver's min + k·step grid (the helper checks the
/// range but not the step; an off-grid value is rounded by the WMI method
/// in firmware-specific ways, so it's never sent).
int FwattrTab::snapped(const Row &r) const {
    const int v = r.spin->value();
    if (!r.info.ranged || r.info.step <= 1) return v;
    const int k = qRound(double(v - r.info.min) / r.info.step);
    return std::clamp(r.info.min + k * r.info.step, r.info.min, r.info.max);
}

void FwattrTab::applyRow(int index) {
    if (busy_ || locked_ || index < 0 || index >= rows_.size() || !rows_[index].info.ranged) return;
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
    QList<int> changed;
    QJsonArray items;
    for (int i = 0; i < rows_.size(); ++i) {
        Row &r = rows_[i];
        if (!r.info.ranged) continue;
        const int v = snapped(r);
        r.spin->setValue(v);
        if (v == r.info.current) continue;
        changed << i;
        items.append(QJsonObject{{"path", r.info.path}, {"value", v}});
    }
    if (changed.isEmpty()) { QMessageBox::information(this, "Apply All", "No changes to apply."); return; }

    setBusy(true);
    privileged::run(helperPath(), QJsonDocument(items).toJson(QJsonDocument::Compact), this,
        [this, changed](const privileged::Result &r) {
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
        });
}
