#pragma once
// Health → Network: live TCP/UDP connections with their processes, an IP
// blacklist, and the Wine/.exe guard (a non-whitelisted Windows program that
// opens an internet connection is cut off and logged by the lpm-netguard
// daemon — see crates/lpm-helpers/src/netguard.rs).
//
// Cost: the connection list is read by the unprivileged netguard-helper only
// while this page is on screen (every 3 s); the block log is watched with
// inotify, so a new block reaches the tray without any polling. Rule changes
// go through pkexec and take effect in the running daemon at once.
#include <QElapsedTimer>
#include <QHash>
#include <QJsonArray>
#include <QJsonObject>
#include <QWidget>
#include <functional>

class QCheckBox;
class QFileSystemWatcher;
class QLabel;
class QLineEdit;
class QListWidget;
class QTabWidget;
class QTimer;
class QTreeWidget;
class QTreeWidgetItem;

class NetPage : public QWidget {
    Q_OBJECT
public:
    explicit NetPage(QWidget *parent = nullptr);

Q_SIGNALS:
    /// A program was blocked just now (not the history found at start-up).
    void alert(const QString &title, const QString &message);

protected:
    void showEvent(QShowEvent *e) override;
    void hideEvent(QHideEvent *e) override;

private:
    void runUser(const QJsonObject &req, std::function<void(const QJsonObject &)> cb);
    void runRoot(const QJsonObject &req, bool isStatus = true);
    void applyStatus(const QJsonObject &s);
    void refreshConns();
    void showConns();
    void readLog(bool announce);
    void watchLog();
    void poll();
    QJsonObject curConn() const;
    void changeList(const char *op, const char *verb, const QString &entry);

    QLabel *status_, *connInfo_;
    QCheckBox *guard_, *lan_, *listening_;
    QTabWidget *tabs_;
    QLineEdit *filter_, *wlEdit_, *blEdit_;
    QTreeWidget *conns_, *blocked_;
    QListWidget *wl_, *bl_;
    QTimer *timer_;
    QFileSystemWatcher *watch_;
    QJsonArray connData_;
    QJsonObject state_;                       // last full status (kernel checks included)
    QHash<QString, QTreeWidgetItem *> rows_;  // block log: program|proto|dst|port → row
    qint64 logPos_ = 0;
    QElapsedTimer lastAlert_;
    bool busy_ = false, viaRoot_ = false;
};
