#include "mainwindow.h"
#include <QFile>
#include "amdgputab.h"
#include "fwattrtab.h"
#include "hometab.h"
#include "inteltab.h"
#include "lighting.h"
#include "lightingtab.h"
#include "nvidiatab.h"
#include "optimizetab.h"
#include "ryzentab.h"
#include "scenes.h"
#include "scenestab.h"
#include "sysinfo.h"
#include "tray.h"
#include "healthtab.h"
#include <QCloseEvent>
#include <QLabel>
#include <QTabBar>
#include <QTabWidget>
#include <QTimer>
#include <malloc.h>
#include <algorithm>

// glibc keeps heap freed by the window's tabs, dialogs and previews mapped;
// once it is back in the tray, hand that memory to the kernel.
static void trimHeap() { ::malloc_trim(0); }

MainWindow::MainWindow(QWidget *parent) : QMainWindow(parent) {
    setWindowTitle("Legion Power Manager");
    setWindowIcon(appIcon(128));
    resize(1040, 720);
    setMinimumSize(820, 520);

    tabs_ = new QTabWidget;
    tabs_->setObjectName("mainTabs");  // header-band styling in theme.cpp
    tabs_->setAttribute(Qt::WA_StyledBackground, true);  // paint the band behind the tab bar
    tabs_->setDocumentMode(true);
    tabs_->setUsesScrollButtons(true);
    setCentralWidget(tabs_);
    // App mark at the start of the tab band; the version lives in its tooltip
    // (it used to take a whole status-bar row for four seconds). Floated over
    // the band rather than set as a corner widget: in document mode a corner
    // widget moves the page's top rule up through the tab labels.
    mark_ = new QLabel(tabs_);
    mark_->setObjectName("appMark");
    mark_->setPixmap(appIcon(64).pixmap(QSize(18, 18)));
    mark_->setToolTip(QStringLiteral("Legion Power Manager " LPM_VERSION));
    mark_->adjustSize();

    home_ = new HomeTab;
    tabs_->addTab(home_, "Home");
    const int scenesAt = tabs_->count();  // Scenes sits right after Home; built last (it reads every tab)
    // Every tab below exists only where its hardware/firmware interface does,
    // so nobody is offered a control their machine can't honour.
    if (FwattrTab::present()) {
        fwattr_ = new FwattrTab;
        tabs_->addTab(fwattr_, "Firmware");
        // Switching to Custom on Home unlocks this tab immediately (not after its poll).
        connect(home_, &HomeTab::profileChanged, fwattr_, &FwattrTab::refreshLockState);
    }
    // One CPU voltage tab per vendor: Curve Optimizer (AMD SMU) or the
    // OC-mailbox undervolt tab (Intel). The other one is never created, so
    // nothing polls ryzen_smu on Intel or touches MSR 0x150 on AMD.
    // Curve Optimizer exists from Zen 3 mobile on (family 0x19 Zen 3/4, 0x1A Zen 5);
    // Renoir/Picasso (0x17) have no per-core CO, so no tab there.
    const int cpuFamily = [] {
        QFile f(QStringLiteral("/proc/cpuinfo"));
        if (!f.open(QIODevice::ReadOnly)) return 0;
        // procfs reports size 0, so QFile::atEnd() is true before the first
        // read — a readLine() loop never ran and the tab vanished. The field
        // is in the first processor block, well within 4 KiB.
        for (const QByteArray &l : f.read(4096).split('\n'))
            if (l.startsWith("cpu family")) return l.mid(l.indexOf(':') + 1).trimmed().toInt();
        return 0;
    }();
    if (sysinfo::isAmd() && cpuFamily >= 0x19) {
        ryzen_ = new RyzenTab;
        tabs_->addTab(ryzen_, "Ryzen Curve");
    } else if (sysinfo::isIntel()) {
        intel_ = new IntelTab;
        tabs_->addTab(intel_, "Intel Undervolt");
    }
    if (NvidiaTab::present()) {
        nvidia_ = new NvidiaTab;
        tabs_->addTab(nvidia_, "NVIDIA Curve");
    }
    // AMD GPU tuning: discrete Radeon or the CPU's integrated graphics (Hybrid mode).
    if (AmdGpuTab::present()) {
        amdgpu_ = new AmdGpuTab;
        tabs_->addTab(amdgpu_, "AMD GPU");
    }
    optimize_ = new OptimizeTab;
    tabs_->addTab(optimize_, "Optimizations");
    // Per-key RGB keyboard (Legion Gen10 Spectrum controller) — only when present.
    if (lighting::present()) {
        lighting_ = new LightingTab;
        tabs_->addTab(lighting_, "Lighting");
    }
    health_ = new HealthTab;
    tabs_->addTab(health_, "Health");

    scenes_ = new SceneEngine(this);
    home_->setSceneEngine(scenes_);
    optimize_->setSceneEngine(scenes_);
    tabs_->insertTab(scenesAt, new ScenesTab(this), "Scenes");
}

// Main tabs are equal boxes: each as wide as the widest label, or wider so
// the row fills the band. Stepped by 4 px so a window drag re-polishes the
// tab bar only now and then, not on every pixel.
void MainWindow::fitTabs() {
    QTabBar *bar = tabs_->tabBar();
    QFont f = bar->font();
    f.setWeight(QFont::DemiBold);  // the selected tab is drawn semi-bold
    const QFontMetrics fm(f);
    int text = 0;
    for (int i = 0; i < tabs_->count(); ++i) text = std::max(text, fm.horizontalAdvance(tabs_->tabText(i)));
    constexpr int CHROME = 2 * (8 + 1 + 2), BAND_LEFT = 36, SLACK = 12;  // padding+border+margin per side
    const int share = (tabs_->width() - BAND_LEFT - SLACK) / std::max(1, int(tabs_->count())) - CHROME;
    const int w = std::max(text + 4, share / 4 * 4);
    if (w == tabW_) return;
    tabW_ = w;
    bar->setStyleSheet(QStringLiteral("QTabBar::tab { min-width: %1px; max-width: %1px; }").arg(w));
}

void MainWindow::resizeEvent(QResizeEvent *e) {
    QMainWindow::resizeEvent(e);
    fitTabs();
}

void MainWindow::showEvent(QShowEvent *e) {
    QMainWindow::showEvent(e);
    fitTabs();
    // Centred on the tab band (its height is known once the style has polished it).
    mark_->move(10, std::max(0, (tabs_->tabBar()->height() - mark_->height()) / 2));
    mark_->raise();
}

void MainWindow::hideEvent(QHideEvent *e) {
    QMainWindow::hideEvent(e);
    QTimer::singleShot(2000, this, [this] { if (!isVisible()) trimHeap(); });
}

void MainWindow::closeEvent(QCloseEvent *e) {
    if (hideOnClose_ && !forceQuit_) { e->ignore(); hide(); return; }
    QMainWindow::closeEvent(e);
}
