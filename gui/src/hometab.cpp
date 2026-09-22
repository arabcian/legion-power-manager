#include "hometab.h"
#include "privileged.h"
#include "sysinfo.h"
#include "theme.h"

#include <QButtonGroup>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QJsonObject>
#include <QLabel>
#include <QMessageBox>
#include <QProcess>
#include <QPushButton>
#include <QStandardPaths>
#include <QTimer>
#include <QVBoxLayout>

static constexpr int POLL_MS = 2500, LIVE_POLL_MS = 2000, SMI_TIMEOUT_MS = 3000;

static const QHash<QString, QString> LABELS{
    {"low-power", "Power Saver"}, {"quiet", "Quiet"}, {"cool", "Cool"}, {"balanced", "Balanced"},
    {"balanced-performance", "Balanced Performance"}, {"performance", "Performance"},
    {"max-power", "Extreme"}, {"custom", "Custom"}};

static const QHash<QString, QString> DESCRIPTIONS{
    {"low-power", "Lowest power draw, longest battery life."},
    {"quiet", "Keeps fan noise as low as possible."},
    {"cool", "Prioritises low surface and internal temperatures."},
    {"balanced", "Balanced mix of power, noise and performance."},
    {"balanced-performance", "Leans towards performance."},
    {"performance", "Maximum performance; fans run louder."},
    {"max-power", "Unrestricted BIOS limits, mains power only."},
    {"custom", "Hands the PPT and fan limits to you. Required by the Firmware Attributes tab."}};

QString HomeTab::profileLabel(const QString &p) {
    if (auto it = LABELS.find(p); it != LABELS.end()) return *it;
    QString t = p;
    t.replace('-', ' ');
    if (!t.isEmpty()) t[0] = t[0].toUpper();
    return t;
}

QString HomeTab::helperPath() { return privileged::helperPath(QStringLiteral("legion-profile-helper")); }

static QLabel *muted(const QString &text, QWidget *parent = nullptr) {
    auto *l = new QLabel(text, parent);
    l->setProperty("role", "muted");
    l->setWordWrap(true);
    return l;
}

static QPushButton *profileButton(const QString &label, const QString &accent) {
    auto *b = new QPushButton(label);
    b->setCheckable(true);
    b->setCursor(Qt::PointingHandCursor);
    b->setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Fixed);
    b->setFixedHeight(26);
    b->setStyleSheet(QStringLiteral(
        "QPushButton { background: %1; color: %2; border: 1px solid %3; border-left: 3px solid %4;"
        " border-radius: 5px; padding: 3px 10px; font-weight: 600; text-align: left; }"
        "QPushButton:hover { background: %5; color: %6; }"
        "QPushButton:checked { background: %4; color: %7; border-color: %4; }")
        .arg(theme::BG2, theme::FG_DIM, theme::BORDER_SOFT, accent, theme::BG3, theme::FG, theme::BG0));
    return b;
}

HomeTab::HomeTab(QWidget *parent) : QWidget(parent), handler_(pp::primaryHandler()) {
    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(12, 10, 12, 10);
    root->setSpacing(8);

    // Header
    auto *title = new QLabel(sysinfo::dmi("product_version").value_or(sysinfo::dmi("product_name").value_or("Lenovo Legion")));
    title->setProperty("role", "title");
    QStringList sub{sysinfo::cpuModel().value_or("unknown CPU")};
    if (handler_) sub << "driver: " + handler_->name;
    root->addWidget(title);
    root->addWidget(muted(sub.join("   ·   ")));

    auto *profileBox = new QGroupBox("Power Profile");
    profileBox->setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Expanding);
    grid_ = new QGridLayout(profileBox);
    grid_->setContentsMargins(8, 4, 8, 6);
    grid_->setHorizontalSpacing(8);
    grid_->setVerticalSpacing(4);

    auto *columns = new QHBoxLayout;
    columns->setSpacing(10);
    auto *left = new QVBoxLayout;
    left->setSpacing(10);
    left->addWidget(buildHardwareBox(), 1);
    left->addWidget(profileBox, 1);
    columns->addLayout(left, 1);
    columns->addWidget(buildLiveBox(), 1);
    root->addLayout(columns, 1);

    status_ = muted(QString());
    root->addWidget(status_);
    statusTimer_ = new QTimer(this);
    statusTimer_->setSingleShot(true);
    connect(statusTimer_, &QTimer::timeout, status_, [this] { status_->clear(); });

    root->addWidget(muted("Per-component limits and undervolt curves live in the other tabs. "
                          "Firmware Attributes only accepts writes while the profile is Custom."));

    group_ = new QButtonGroup(this);
    group_->setExclusive(true);
    connect(group_, &QButtonGroup::buttonClicked, this, [this](QAbstractButton *b) {
        applyProfile(b->property("profile").toString());
    });

    rebuild();

    auto *poll = new QTimer(this);
    connect(poll, &QTimer::timeout, this, &HomeTab::refreshSelection);
    poll->start(POLL_MS);
    auto *live = new QTimer(this);
    connect(live, &QTimer::timeout, this, &HomeTab::refreshLive);
    live->start(LIVE_POLL_MS);
    QTimer::singleShot(0, this, &HomeTab::refreshLive);
}

// ── nvidia-smi (async, bounded) ─────────────────────────────────────────────

void HomeTab::runNvidiaSmi(const QStringList &args, std::function<void(const QByteArray &)> onOk) {
    const QString exe = QStandardPaths::findExecutable(QStringLiteral("nvidia-smi"));
    if (exe.isEmpty()) { onOk({}); return; }
    auto *p = new QProcess(this);
    auto *t = new QTimer(p);
    t->setSingleShot(true);
    connect(t, &QTimer::timeout, p, [p] { p->kill(); });
    connect(p, &QProcess::finished, this, [p, onOk](int code, QProcess::ExitStatus st) {
        onOk(code == 0 && st == QProcess::NormalExit ? p->readAllStandardOutput() : QByteArray());
        p->deleteLater();
    });
    connect(p, &QProcess::errorOccurred, this, [p, onOk](QProcess::ProcessError e) {
        if (e == QProcess::FailedToStart) { onOk({}); p->deleteLater(); }
    });
    p->start(exe, args);
    t->start(SMI_TIMEOUT_MS);
    if (args.contains("--query-gpu=temperature.gpu,power.draw,clocks.current.graphics,utilization.gpu"))
        smiLive_ = p;
}

// ── Hardware box ────────────────────────────────────────────────────────────

QGroupBox *HomeTab::buildHardwareBox() {
    auto *box = new QGroupBox("Hardware");
    box->setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Expanding);
    auto *g = new QGridLayout(box);
    g->setContentsMargins(8, 4, 8, 6);
    g->setHorizontalSpacing(10);
    g->setVerticalSpacing(6);

    int row = 0;
    auto add = [&](const QString &k, const std::optional<QString> &v, QLabel **keyOut = nullptr, QLabel **valOut = nullptr) {
        if (!v) return;
        auto *key = muted(k);
        auto *val = new QLabel(*v);
        val->setWordWrap(true);
        g->addWidget(key, row, 0, Qt::AlignTop);
        g->addWidget(val, row, 1);
        if (keyOut) *keyOut = key;
        if (valOut) *valOut = val;
        ++row;
    };
    add("System", sysinfo::systemInfo());
    add("Kernel", sysinfo::kernel());
    add("CPU", sysinfo::cpuModel());
    add("GPU", QStringLiteral("…"), &gpuHwKey_, &gpuHwValue_);
    add("Memory", sysinfo::ramTotal());
    add("BIOS", sysinfo::biosInfo());
    add("EC Firmware", sysinfo::dmiClean("ec_firmware_release"));
    g->setColumnStretch(1, 1);
    g->setRowStretch(row, 1);

    runNvidiaSmi({"--query-gpu=name", "--format=csv,noheader"}, [this](const QByteArray &out) {
        if (auto n = sysinfo::parseGpuName(out)) { gpuHwValue_->setText(*n); }
        else { gpuHwKey_->hide(); gpuHwValue_->hide(); }
    });
    return box;
}

// ── Live box ────────────────────────────────────────────────────────────────

QGroupBox *HomeTab::buildLiveBox() {
    auto *box = new QGroupBox("Live");
    box->setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Expanding);
    auto *g = new QGridLayout(box);
    g->setContentsMargins(8, 4, 8, 6);
    g->setHorizontalSpacing(10);
    g->setVerticalSpacing(6);

    const QList<QPair<QString, std::function<std::optional<QString>()>>> spec{
        {"CPU Temp", sysinfo::cpuTemp}, {"GPU", nullptr}, {"iGPU", sysinfo::igpu},
        {"Fans", sysinfo::fans}, {"Storage", sysinfo::storage}, {"Power", sysinfo::power},
        {"Battery", sysinfo::battery}};
    int row = 0;
    for (const auto &[name, getter] : spec) {
        if (!getter) {  // GPU: filled asynchronously; hidden until nvidia-smi answers
            gpuLiveKey_ = muted(name);
            gpuLiveValue_ = new QLabel;
            gpuLiveValue_->setWordWrap(true);
            gpuLiveKey_->hide();
            gpuLiveValue_->hide();
            g->addWidget(gpuLiveKey_, row, 0, Qt::AlignTop);
            g->addWidget(gpuLiveValue_, row++, 1);
            continue;
        }
        auto v = getter();
        if (!v) continue;
        auto *key = muted(name);
        auto *val = new QLabel(*v);
        val->setWordWrap(true);
        g->addWidget(key, row, 0, Qt::AlignTop);
        g->addWidget(val, row++, 1);
        liveRows_.append({key, val, getter});
    }
    if (liveRows_.isEmpty()) g->addWidget(muted("No live sensors found (hwmon, nvidia-smi, battery)."), row++, 0, 1, 2);
    g->setColumnStretch(1, 1);
    g->setRowStretch(row, 1);
    return box;
}

void HomeTab::refreshLive() {
    // Not visible (hidden to tray / other tab) → no reads. nvidia-smi in
    // particular wakes the dGPU out of D3cold.
    if (!isVisible()) return;
    for (const LiveRow &r : liveRows_) r.value->setText(r.getter().value_or(QStringLiteral("—")));
    if (smiLive_) return;  // previous query still running — never stack them
    runNvidiaSmi({"--query-gpu=temperature.gpu,power.draw,clocks.current.graphics,utilization.gpu",
                  "--format=csv,noheader,nounits"},
                 [this](const QByteArray &out) {
                     smiLive_ = nullptr;
                     if (auto v = sysinfo::parseGpuLive(out)) {
                         gpuLiveValue_->setText(*v);
                         gpuLiveKey_->show();
                         gpuLiveValue_->show();
                     } else if (gpuLiveValue_->isVisible()) {
                         gpuLiveValue_->setText(QStringLiteral("—"));
                     }
                 });
}

// ── Profile buttons ─────────────────────────────────────────────────────────

void HomeTab::showStatus(const QString &msg, int timeoutMs) {
    status_->setText(msg);
    if (timeoutMs) statusTimer_->start(timeoutMs);
}

void HomeTab::updateDescription(const std::optional<QString> &p) {
    if (description_) description_->setText(p ? DESCRIPTIONS.value(*p) : QString());
}

void HomeTab::rebuild() {
    for (QPushButton *b : std::as_const(buttons_)) group_->removeButton(b);
    buttons_.clear();
    description_ = nullptr;
    while (QLayoutItem *it = grid_->takeAt(0)) {
        if (QWidget *w = it->widget()) w->deleteLater();
        delete it;
    }
    handler_ = pp::primaryHandler();

    if (!pp::available()) {
        grid_->addWidget(muted(
            "No power-profile interface found. This usually means the lenovo-wmi-gamezone (or legion-laptop) "
            "platform driver is not loaded. CPU and GPU limits can still be set from the Firmware Attributes tab."), 0, 0);
        return;
    }
    const QStringList profiles = pp::offeredProfiles(handler_);
    const auto current = pp::currentProfile(handler_);
    int i = 0;
    for (const QString &name : profiles) {
        auto *b = profileButton(profileLabel(name), theme::profileAccent(name));
        b->setProperty("profile", name);
        b->setToolTip(DESCRIPTIONS.value(name, name));
        b->setChecked(current == name);
        group_->addButton(b);
        buttons_.insert(name, b);
        grid_->addWidget(b, i++, 0);
    }
    grid_->setColumnStretch(0, 1);
    description_ = muted(QString());
    grid_->addWidget(description_, i, 0);
    grid_->setRowStretch(i + 1, 1);
    updateDescription(current);
    if (!current) showStatus("Could not read the current power profile.");
}

void HomeTab::refreshSelection() {
    if (applying_) return;  // don't move the checkmark under a pending polkit dialog
    const auto current = pp::currentProfile(handler_);
    if (!current) return;
    if (QPushButton *b = buttons_.value(*current)) {
        if (!b->isChecked()) { b->setChecked(true); updateDescription(current); }
    } else {
        rebuild();  // a hidden mode became active (hotkey) — give it a button
    }
}

void HomeTab::applyProfile(const QString &profile) {
    if (applying_) return;
    const auto previous = pp::currentProfile(handler_);
    QJsonObject req{{"profile", profile}};
    if (handler_) req.insert("handler", handler_->node);

    applying_ = true;
    for (QPushButton *b : std::as_const(buttons_)) b->setEnabled(false);
    showStatus("Switching to " + profileLabel(profile) + "…", 0);

    privileged::run(helperPath(), req, this, [this, profile, previous](const privileged::Result &r) {
        applying_ = false;
        for (QPushButton *b : std::as_const(buttons_)) b->setEnabled(true);
        if (!r.reached) {
            showStatus(QString());
            QMessageBox::critical(this, "Authorization failed", r.error);
            refreshSelection();
            return;
        }
        if (!r.ok()) {
            showStatus(QString());
            QMessageBox::critical(this, "Could not switch profile", r.message().isEmpty() ? "unknown error" : r.message());
            rebuild();
            return;
        }
        QString effective = r.json.value("effective").toString();
        if (effective.isEmpty()) effective = profile;
        if (effective != profile) {
            showStatus("Requested " + profileLabel(profile) + ", but the firmware settled on " + profileLabel(effective) + ".");
            rebuild();
        } else {
            showStatus("Power profile set to " + profileLabel(effective) + ".");
            if (QPushButton *b = buttons_.value(effective)) b->setChecked(true);
            updateDescription(effective);
        }
        if (previous != effective) Q_EMIT profileChanged(effective);
    }, 60000);
}
