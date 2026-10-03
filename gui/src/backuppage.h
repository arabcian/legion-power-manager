#pragma once
// Health → Backup: two kinds of dated archive (engine: backup-helper).
//
//  * LPM configuration — everything Legion Power Manager stores (scenes,
//    presets, curves, lighting, boot profiles, network-guard rules, calibration)
//    plus an optional "system profile" (Portage config, world set, kernel
//    config, fstab, boot/module/sysctl configuration, package list). Made
//    without root; restoring the root-owned part asks for the password.
//  * System image — `tar --acls --xattrs -cpf - / | pigz` into a chosen folder,
//    and the matching restore into a chosen target directory. Root, the
//    administrator password every time.
//
// One job at a time; the helper streams progress lines. Cancel closes its
// stdin (a root child cannot be killed from here). Nothing runs in the
// background: the list is re-read when the page is shown and after a job.
#include <QJsonObject>
#include <QPointer>
#include <QWidget>
#include <functional>

class QCheckBox;
class QComboBox;
class QLabel;
class QLineEdit;
class QPlainTextEdit;
class QProcess;
class QProgressBar;
class QPushButton;
class QSpinBox;
class QTreeWidget;

class BackupPage : public QWidget {
    Q_OBJECT
public:
    explicit BackupPage(QWidget *parent = nullptr);
    ~BackupPage() override;

Q_SIGNALS:
    /// A job finished while the page was not on screen (tray notification).
    void alert(const QString &title, const QString &message);

protected:
    void showEvent(QShowEvent *e) override;

private:
    using Done = std::function<void(const QJsonObject &)>;
    void run(const QJsonObject &req, bool root, const QString &what, Done done);
    void onLine(const QByteArray &line);
    void cancel();
    void setBusy(bool busy, const QString &what = QString());
    void log(const QString &line, const char *color = nullptr);
    void finished(const QString &summary, bool good);

    void refreshList();
    void updateFree();
    void saveSettings();
    QString selected(QString *kind = nullptr) const;
    void select(const QString &path);

    void configBackup();
    void configRestore();
    void systemBackup();
    void systemRestore();
    void verifySelected();

    QLineEdit *cfgDir_, *sysDir_, *target_;
    QCheckBox *cfgProfile_, *cfgLogs_, *low_, *verify_, *home_;
    QComboBox *comp_;
    QSpinBox *level_, *keep_;
    QPlainTextEdit *excludes_, *log_;
    QLabel *free_, *status_;
    QTreeWidget *list_;
    QProgressBar *bar_;
    QPushButton *cancel_;
    QList<QWidget *> actions_;     // disabled while a job runs
    QStringList extra_;            // archives added with "Add file…"

    QPointer<QProcess> proc_;
    bool procRoot_ = false, cancelling_ = false;
    QJsonObject final_;
    qint64 lastBytes_ = 0, lastElapsed_ = 0;
    double rate_ = 0;
};
