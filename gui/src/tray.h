#pragma once
// System tray icon + menu — port of tray.py.
#include <QSystemTrayIcon>

class MainWindow;
class QMenu;

QIcon appIcon(int size = 64);

class Tray : public QSystemTrayIcon {
    Q_OBJECT
public:
    explicit Tray(MainWindow *win);
    void toggleWindow();

private:
    void rebuild();
    bool claimCooldown();
    void notify(const QString &title, const QString &msg);

    MainWindow *win_;
    QMenu *menu_;
    bool cooldown_ = false;
};
