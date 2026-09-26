#pragma once
#include <QMainWindow>

class AmdGpuTab;
class FwattrTab;
class HomeTab;
class SceneEngine;
class IntelTab;
class LightingTab;
class NvidiaTab;
class OptimizeTab;
class RyzenTab;
class QTabWidget;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    explicit MainWindow(QWidget *parent = nullptr);
    HomeTab *home() const { return home_; }
    RyzenTab *ryzen() const { return ryzen_; }  // nullptr on non-AMD CPUs
    IntelTab *intel() const { return intel_; }  // nullptr on non-Intel CPUs
    NvidiaTab *nvidia() const { return nvidia_; }
    AmdGpuTab *amdgpu() const { return amdgpu_; }  // nullptr without an amdgpu card
    OptimizeTab *optimize() const { return optimize_; }
    FwattrTab *fwattr() const { return fwattr_; }
    LightingTab *lighting() const { return lighting_; }  // nullptr without a Spectrum keyboard
    SceneEngine *scenes() const { return scenes_; }
    /// Set by the tray's Quit: closeEvent then really closes instead of hiding.
    void setForceQuit(bool v) { forceQuit_ = v; }
    void setHideOnClose(bool v) { hideOnClose_ = v; }

protected:
    void closeEvent(QCloseEvent *e) override;
    void hideEvent(QHideEvent *e) override;

private:
    QTabWidget *tabs_;
    HomeTab *home_;
    RyzenTab *ryzen_ = nullptr;
    IntelTab *intel_ = nullptr;
    NvidiaTab *nvidia_;
    AmdGpuTab *amdgpu_ = nullptr;
    OptimizeTab *optimize_;
    FwattrTab *fwattr_;
    LightingTab *lighting_ = nullptr;
    SceneEngine *scenes_;
    bool forceQuit_ = false, hideOnClose_ = false;
};
