#include "tray.h"
#include "hometab.h"
#include "mainwindow.h"
#include "nvidiatab.h"
#include "optimizetab.h"
#include "ryzentab.h"
#include "theme.h"
#include <QApplication>
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

    // Ryzen
    QMenu *rm = menu_->addMenu("CPU Curve (Ryzen)");
    RyzenTab *ryzen = win_->ryzen();
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

    menu_->addSeparator();
    connect(menu_->addAction(win_->isVisible() ? "Hide window" : "Show window"), &QAction::triggered, this, &Tray::toggleWindow);
    connect(menu_->addAction("Quit"), &QAction::triggered, this, [this] {
        win_->setForceQuit(true);
        hide();
        qApp->quit();
    });
}
