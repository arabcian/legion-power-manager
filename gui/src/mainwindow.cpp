#include "mainwindow.h"
#include "fwattrtab.h"
#include "hometab.h"
#include "nvidiatab.h"
#include "ryzentab.h"
#include "tray.h"
#include <QCloseEvent>
#include <QStatusBar>
#include <QTabWidget>

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
    auto *fw = new FwattrTab;
    tabs_->addTab(fw, "Firmware Attributes");
    // Switching to Custom on Home unlocks this tab immediately (not after its poll).
    connect(home_, &HomeTab::profileChanged, fw, &FwattrTab::refreshLockState);
    nvidia_ = new NvidiaTab;
    tabs_->addTab(nvidia_, "NVIDIA Curve Optimizer");
    ryzen_ = new RyzenTab;
    tabs_->addTab(ryzen_, "Ryzen Curve Optimizer");

    statusBar()->showMessage("Legion Power Manager " LPM_VERSION, 4000);
}

void MainWindow::closeEvent(QCloseEvent *e) {
    if (hideOnClose_ && !forceQuit_) { e->ignore(); hide(); return; }
    QMainWindow::closeEvent(e);
}
