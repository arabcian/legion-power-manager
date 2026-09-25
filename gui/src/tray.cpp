#include "tray.h"
#include "hometab.h"
#include "inteltab.h"
#include "mainwindow.h"
#include "nvidiatab.h"
#include "optimizetab.h"
#include "ryzentab.h"
#include "scenes.h"
#include "theme.h"
#include <QApplication>
#include <algorithm>
#include <QMenu>
#include <QPainter>
#include <QTimer>

static constexpr int APPLY_COOLDOWN_MS = 2500;

QIcon appIcon(int size) {
    // Drawn at runtime so it always matches theme::ACCENT (same glyph as tray.py).
    QPixmap pm(size, size);
    pm.fill(Qt::transparent);
    QPainter p(&pm);
    p.setRenderHint(QPainter::Antialiasing);
    const QColor accent(QString::fromLatin1(theme::ACCENT));
    p.setBrush(QColor(QString::fromLatin1(theme::BG1)));
    p.setPen(QPen(accent, std::max(2, size / 20)));
    const double in = size * 0.08;
    p.drawRoundedRect(QRectF(in, in, size - 2 * in, size - 2 * in), size * 0.18, size * 0.18);
    QPen pen(accent, std::max(3, size / 11));
    pen.setCapStyle(Qt::RoundCap);
    p.setPen(pen);
    p.setBrush(Qt::NoBrush);
    const double m = size * 0.28;
    p.drawArc(QRectF(m, m, size - 2 * m, size - 2 * m), -60 * 16, 300 * 16);
    p.drawLine(QPointF(size / 2.0, size * 0.2), QPointF(size / 2.0, size * 0.47));
    return QIcon(pm);
}

Tray::Tray(MainWindow *win) : QSystemTrayIcon(appIcon(), win), win_(win), menu_(new QMenu) {
    setToolTip("Legion Power Manager");
    // Rebuilt on every open: profiles can change behind our back.
    connect(menu_, &QMenu::aboutToShow, this, &Tray::rebuild);
    setContextMenu(menu_);
    connect(this, &QSystemTrayIcon::activated, this, [this](ActivationReason r) {
        if (r == Trigger || r == DoubleClick) toggleWindow();
    });
    rebuild();

    // Scene results: a notification when nobody is looking at the Scenes tab
    // (tray pick, charger plugged/pulled); the tab shows its own status line.
    SceneEngine *eng = win_->scenes();
    connect(eng, &SceneEngine::finished, this, [this](const QString &n, bool ok, const QStringList &log) {
        if (n.isEmpty()) return;  // informational only
        if (win_->isVisible() && win_->isActiveWindow()) return;
        QStringList bad;
        for (const QString &l : log) if (l.startsWith(QStringLiteral("✗"))) bad << l.mid(2);
        notify("Scene: " + n, ok ? QStringLiteral("Applied.") : bad.join('\n'));
    });
    connect(eng, &SceneEngine::started, this, [this](const QString &n) { setToolTip("Legion Power Manager — applying scene '" + n + "'…"); });
    connect(eng, &SceneEngine::finished, this, [this](const QString &n) { if (!n.isEmpty()) setToolTip("Legion Power Manager — scene: " + n); });
}

void Tray::toggleWindow() {
    if (win_->isVisible() && !win_->isMinimized()) { win_->hide(); return; }
    win_->showNormal();
    win_->raise();
    win_->activateWindow();
}

bool Tray::claimCooldown() {
    if (cooldown_) { notify("Legion Power Manager", "Still applying the previous change — try again in a moment."); return false; }
    cooldown_ = true;
    QTimer::singleShot(APPLY_COOLDOWN_MS, this, [this] { cooldown_ = false; });
    return true;
}

void Tray::notify(const QString &t, const QString &m) {
    if (supportsMessages()) showMessage(t, m, icon(), 2500);
}

static void disabledEntry(QMenu *m, const QString &t) { m->addAction(t)->setEnabled(false); }

void Tray::rebuild() {
    menu_->clear();

    // Scenes: the whole machine in one click; the component menus below stay
    // for one-off changes.
    SceneEngine *eng = win_->scenes();
    const QStringList sceneNames = scenes::names();
    if (!sceneNames.isEmpty()) {
        QMenu *sm = menu_->addMenu("Scene");
        for (const QString &n : sceneNames) {
            QAction *a = sm->addAction(n);
            a->setCheckable(true);
            a->setChecked(n == eng->active());
            connect(a, &QAction::triggered, this, [this, eng, n] {
                if (!claimCooldown()) return;
                eng->apply(n);
                notify("Scene", "Applying '" + n + "'…");
            });
        }
        sm->addSeparator();
        QAction *au = sm->addAction("Switch with power source");
        au->setCheckable(true);
        const scenes::Auto cfg = eng->autoConfig();
        au->setChecked(cfg.enabled);
        au->setEnabled(!cfg.onAc.isEmpty() || !cfg.onBattery.isEmpty());
        if (!au->isEnabled()) au->setToolTip("Choose the AC / battery scenes in the Scenes tab first");
        connect(au, &QAction::toggled, this, [eng](bool on) {
            scenes::Auto c = eng->autoConfig();
            c.enabled = on;
            eng->setAuto(c);
        });
        sm->setEnabled(!eng->busy());
        menu_->addSeparator();
    }

    // Power profile
    QMenu *pm = menu_->addMenu("Power Profile");
    HomeTab *home = win_->home();
    const QStringList profiles = home->offeredProfiles();
    if (profiles.isEmpty()) disabledEntry(pm, "(no platform-profile driver)");
    const auto current = home->currentProfile();
    for (const QString &name : profiles) {
        QAction *a = pm->addAction(HomeTab::profileLabel(name));
        a->setCheckable(true);
        a->setChecked(current == name);
        connect(a, &QAction::triggered, this, [this, home, name] {
            if (!claimCooldown()) return;
            home->applyProfile(name);  // async; Home emits profileChanged → Firmware tab relocks
        });
    }

    // Ryzen (AMD only — the window has no Ryzen tab on Intel)
    if (RyzenTab *ryzen = win_->ryzen()) {
    QMenu *rm = menu_->addMenu("CPU Curve (Ryzen)");
    const QStringList rnames = ryzen->savedProfileNames();
    if (rnames.isEmpty()) disabledEntry(rm, "(no saved profiles)");
    for (const QString &n : rnames)
        connect(rm->addAction(n), &QAction::triggered, this, [this, ryzen, n] {
            if (claimCooldown() && ryzen->applyNamedProfile(n)) notify("CPU curve", "Applying profile '" + n + "'…");
        });
    rm->addSeparator();
    connect(rm->addAction("Reset curve"), &QAction::triggered, this, [this, ryzen] {
        if (!claimCooldown()) return;
        ryzen->applyReset();
        notify("CPU curve", "Resetting Curve Optimizer (coall=0)…");
    });
    }

    // Intel undervolt (Intel only)
    if (IntelTab *intel = win_->intel()) {
        QMenu *im = menu_->addMenu("CPU Undervolt (Intel)");
        const QStringList inames = intel->savedProfileNames();
        if (inames.isEmpty()) disabledEntry(im, "(no saved profiles)");
        for (const QString &n : inames)
            connect(im->addAction(n), &QAction::triggered, this, [this, intel, n] {
                if (claimCooldown() && intel->applyNamedProfile(n)) notify("CPU undervolt", "Applying profile '" + n + "'…");
            });
        im->addSeparator();
        connect(im->addAction("Reset voltages"), &QAction::triggered, this, [this, intel] {
            if (!claimCooldown()) return;
            intel->applyReset();
            notify("CPU undervolt", "Resetting voltage offsets to 0 mV…");
        });
    }

    // NVIDIA
    QMenu *nm = menu_->addMenu("GPU Curve (NVIDIA)");
    NvidiaTab *nv = win_->nvidia();
    const QStringList nnames = nv->profileNames();
    const QString def = nv->defaultProfileName();
    if (nnames.isEmpty()) disabledEntry(nm, "(no saved profiles)");
    for (const QString &n : nnames)
        connect(nm->addAction(n == def ? n + "  ★" : n), &QAction::triggered, this, [this, nv, n] {
            if (!claimCooldown()) return;
            nv->applyNamedProfile(n);
            notify("GPU curve", "Applying profile '" + n + "'…");
        });
    nm->addSeparator();
    connect(nm->addAction("Reset curve"), &QAction::triggered, this, [this, nv] {
        if (claimCooldown()) nv->resetCurve();
    });
    nm->setEnabled(!nv->busy());  // greyed while an NVIDIA helper call is in flight

    // Optimizations
    QMenu *om = menu_->addMenu("Optimizations");
    OptimizeTab *opt = win_->optimize();
    const QString game = opt->gamePreset();
    for (const QString &n : opt->presetNames())
        connect(om->addAction(n == game ? n + "  ★" : n), &QAction::triggered, this, [this, opt, n] {
            if (!claimCooldown()) return;
            if (opt->applyNamedPreset(n)) notify("Optimizations", "Applying preset '" + n + "'…");
            else notify("Optimizations", "Preset '" + n + "' has nothing applicable here.");
        });
    om->addSeparator();
    QAction *restore = om->addAction("Restore originals");
    restore->setEnabled(opt->tuningActive());
    connect(restore, &QAction::triggered, this, [this, opt] {
        if (!claimCooldown()) return;
        opt->restoreAll(false);
        notify("Optimizations", "Restoring original values…");
    });
    om->setEnabled(!opt->busy());

    // Fans: all to max / Auto, or one fan at a fixed RPM (multiples of 100).
    const QList<HomeTab::FanInfo> fans = home->fanInfo();
    if (!fans.isEmpty()) {
        QMenu *fm = menu_->addMenu("Fans");
        const bool maxMode = home->fansMaxMode();
        QAction *mx = fm->addAction("Max all fans");
        mx->setCheckable(true);
        mx->setChecked(maxMode);
        connect(mx, &QAction::triggered, this, [this, home, maxMode] {
            if (!claimCooldown()) return;
            if (maxMode) { home->setAllFansAuto(); notify("Fans", "All fans back to Auto…"); }
            else { home->setAllFansMax(); notify("Fans", "All fans to maximum…"); }
        });
        connect(fm->addAction("All fans to Auto"), &QAction::triggered, this, [this, home] {
            if (!claimCooldown()) return;
            home->setAllFansAuto();
            notify("Fans", "All fans back to Auto…");
        });
        fm->addSeparator();
        for (const HomeTab::FanInfo &f : fans) {
            const QString cur = f.target > 0 ? QStringLiteral("%1 RPM").arg(f.target) : QStringLiteral("Auto");
            QMenu *sub = fm->addMenu(f.name + "  (" + cur + ")");
            auto add = [this, home, sub, f](const QString &text, int rpm) {
                QAction *a = sub->addAction(text);
                a->setCheckable(true);
                a->setChecked(f.target == rpm);
                const QString key = f.key, name = f.name;
                connect(a, &QAction::triggered, this, [this, home, key, name, rpm] {
                    if (!claimCooldown()) return;
                    home->setFanTarget(key, rpm);
                    notify("Fans", rpm > 0 ? QStringLiteral("%1 → %2 RPM").arg(name).arg(rpm) : name + " → Auto");
                });
            };
            add("Auto", 0);
            sub->addSeparator();
            const int top = f.max > 0 && f.max < 9999 ? f.max : 6000;
            const int start = std::max(100, (f.min + 99) / 100 * 100);
            for (int r = start; r <= top; r += 100) add(QStringLiteral("%1 RPM").arg(r), r);
            if (top % 100) add(QStringLiteral("%1 RPM (max)").arg(top), top);
        }
        fm->setEnabled(!home->fanBusy());
    }

    menu_->addSeparator();
    connect(menu_->addAction(win_->isVisible() ? "Hide window" : "Show window"), &QAction::triggered, this, &Tray::toggleWindow);
    connect(menu_->addAction("Quit"), &QAction::triggered, this, [this] {
        win_->setForceQuit(true);
        hide();
        qApp->quit();
    });
}
