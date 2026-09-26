#include "mainwindow.h"
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
#include <QCloseEvent>
#include <QStatusBar>
#include <QTabWidget>
#include <QTimer>
#include <malloc.h>

// glibc keeps heap freed by the window's tabs, dialogs and previews mapped;
// once it is back in the tray, hand that memory to the kernel.
static void trimHeap() { ::malloc_trim(0); }

MainWindow::MainWindow(QWidget *parent) : QMainWindow(parent) {
    setWindowTitle("Legion Power Manager");
    setWindowIcon(appIcon(128));
    resize(980, 700);
    setMinimumSize(820, 520);

    tabs_ = new QTabWidget;
    tabs_->setDocumentMode(true);
    setCentralWidget(tabs_);

    home_ = new HomeTab;
    tabs_->addTab(home_, "Home");
    auto *fw = fwattr_ = new FwattrTab;
    const int scenesAt = tabs_->count();  // Scenes sits right after Home; built last (it reads every tab)
    tabs_->addTab(fw, "Firmware Attributes");
    // Switching to Custom on Home unlocks this tab immediately (not after its poll).
    connect(home_, &HomeTab::profileChanged, fw, &FwattrTab::refreshLockState);
    nvidia_ = new NvidiaTab;
    tabs_->addTab(nvidia_, "NVIDIA Curve Optimizer");
    // AMD GPU tuning: discrete Radeon or the CPU's integrated graphics (Hybrid mode).
    if (AmdGpuTab::present()) {
        amdgpu_ = new AmdGpuTab;
        tabs_->addTab(amdgpu_, "AMD GPU");
    }
    // One CPU voltage tab per vendor: Curve Optimizer (AMD SMU) or the
    // OC-mailbox undervolt tab (Intel). The other one is never created, so
    // nothing polls ryzen_smu on Intel or touches MSR 0x150 on AMD.
    if (sysinfo::isAmd()) {
        ryzen_ = new RyzenTab;
        tabs_->addTab(ryzen_, "Ryzen Curve Optimizer");
    } else if (sysinfo::isIntel()) {
        intel_ = new IntelTab;
        tabs_->addTab(intel_, "Intel Undervolt");
    }
    optimize_ = new OptimizeTab;
    tabs_->addTab(optimize_, "Optimizations");
    // Per-key RGB keyboard (Legion Gen10 Spectrum controller) — only when present.
    if (lighting::present()) {
        lighting_ = new LightingTab;
        tabs_->addTab(lighting_, "Lighting");
    }

    scenes_ = new SceneEngine(this);
    tabs_->insertTab(scenesAt, new ScenesTab(this), "Scenes");

    statusBar()->showMessage("Legion Power Manager " LPM_VERSION, 4000);
}

void MainWindow::hideEvent(QHideEvent *e) {
    QMainWindow::hideEvent(e);
    QTimer::singleShot(2000, this, [this] { if (!isVisible()) trimHeap(); });
}

void MainWindow::closeEvent(QCloseEvent *e) {
    if (hideOnClose_ && !forceQuit_) { e->ignore(); hide(); return; }
    QMainWindow::closeEvent(e);
}
