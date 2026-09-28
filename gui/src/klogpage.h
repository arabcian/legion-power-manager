#pragma once
// Health → Kernel log: every kernel message of level warning or worse, saved
// across boots. Source: `tune-helper {"op":"klog","since":N}` (reads /dev/kmsg).
//
// Capture is event-driven: while the app runs, /dev/kmsg is watched with a
// socket notifier and the helper is started only after a warning-or-worse
// record appeared (debounced) — no periodic process spawns. If the kernel log
// is restricted (dmesg_restrict=1) it is read through pkexec only when the
// page is shown or "Refresh" is pressed, never in the background.
// The log file survives reboots until cleared; the ring buffer does not.
#include <QWidget>

class QComboBox;
class QLabel;
class QLineEdit;
class QTextBrowser;
class QTimer;

class KlogPage : public QWidget {
    Q_OBJECT
public:
    explicit KlogPage(QWidget *parent = nullptr);
    ~KlogPage() override;

protected:
    void showEvent(QShowEvent *e) override;

private:
    void capture();
    void drainKmsg();
    void onReply(const QJsonObject &r, bool viaRoot);
    void persist(const QJsonArray &records, const QString &bootId);
    void render();
    void updateInfo();
    static QString logPath();

    QComboBox *scope_;
    QLineEdit *filter_;
    QTextBrowser *text_;
    QLabel *info_;
    QTimer *debounce_;
    int kmsgFd_ = -1;
    quint64 lastSeq_ = 0, savedSeq_ = 0;
    QString bootId_;
    bool needsRoot_ = false, busy_ = false, shownOnce_ = false;
};
