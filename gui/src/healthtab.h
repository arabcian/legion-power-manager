#pragma once
// Health tab — hardware/driver faults since boot: NVIDIA Xid (with the root
// cause of an Xid 154), GSP timeouts, machine checks, PCIe AER, lockups.
//
// Source: `tune-helper {"op":"health","since":N}`, run unprivileged (reads
// /dev/kmsg). Event-driven: the tab keeps /dev/kmsg open with a socket
// notifier and starts the helper only after a kernel line that can matter
// (NVRM, MCE, AER, lockup, amdgpu) — no periodic process spawns. Only when
// the kernel log is restricted (dmesg_restrict=1) it falls back to a slow
// pkexec poll. New critical events reach the tray as a notification.
#include <QJsonObject>
#include <QWidget>

class QLabel;
class QTimer;
class QTreeWidget;

class HealthTab : public QWidget {
    Q_OBJECT
public:
    explicit HealthTab(QWidget *parent = nullptr);
    ~HealthTab() override;

Q_SIGNALS:
    /// A new error/critical event (not the history found at start-up).
    void alert(const QString &title, const QString &message);

private:
    void scan();
    void drainKmsg();
    void onReply(const QJsonObject &r, bool viaRoot);
    void addEvent(const QJsonObject &e);
    void updateSummary();

    QLabel *summary_, *source_, *aer_;
    QTreeWidget *list_;
    QTimer *timer_, *debounce_;
    int kmsgFd_ = -1;
    quint64 lastSeq_ = 0;
    bool first_ = true, needsRoot_ = false, busy_ = false;
    int xid_ = 0, gsp_ = 0, mce_ = 0, aerN_ = 0, lockup_ = 0, other_ = 0;
    QString lastRootXid_;  // title of the Xid before an Xid 154
};
