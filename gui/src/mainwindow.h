#pragma once
#include <QMainWindow>

class HomeTab;
class NvidiaTab;
class OptimizeTab;
class RyzenTab;
class QTabWidget;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    explicit MainWindow(QWidget *parent = nullptr);
    HomeTab *home() const { return home_; }
    RyzenTab *ryzen() const { return ryzen_; }
    NvidiaTab *nvidia() const { return nvidia_; }
    OptimizeTab *optimize() const { return optimize_; }
    /// Set by the tray's Quit: closeEvent then really closes instead of hiding.
    void setForceQuit(bool v) { forceQuit_ = v; }
    void setHideOnClose(bool v) { hideOnClose_ = v; }

protected:
    void closeEvent(QCloseEvent *e) override;

private:
    QTabWidget *tabs_;
    HomeTab *home_;
    RyzenTab *ryzen_;
    NvidiaTab *nvidia_;
    OptimizeTab *optimize_;
    bool forceQuit_ = false, hideOnClose_ = false;
};
