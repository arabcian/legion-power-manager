#pragma once
// Home tab: power-profile switcher + hardware inventory + live sensors.
// Port of home_tab.py.
#include "platformprofile.h"
#include <QHash>
#include <QWidget>
#include <functional>
#include <optional>

class QButtonGroup;
class QGridLayout;
class QGroupBox;
class QLabel;
class QProcess;
class QPushButton;
class QTimer;

class HomeTab : public QWidget {
    Q_OBJECT
public:
    explicit HomeTab(QWidget *parent = nullptr);

    static QString profileLabel(const QString &profile);
    static QString helperPath();

    /// Also used by the tray menu.
    void applyProfile(const QString &profile);
    QStringList offeredProfiles() const { return pp::offeredProfiles(handler_); }
    std::optional<QString> currentProfile() const { return pp::currentProfile(handler_); }

Q_SIGNALS:
    /// Emitted after a successful switch (Firmware Attributes tab unlocks on "custom").
    void profileChanged(const QString &profile);

private:
    void rebuild();
    void refreshSelection();
    void refreshLive();
    void showStatus(const QString &msg, int timeoutMs = 4000);
    void updateDescription(const std::optional<QString> &profile);
    QGroupBox *buildHardwareBox();
    QGroupBox *buildLiveBox();
    void runNvidiaSmi(const QStringList &args, std::function<void(const QByteArray &)> onOk);

    std::optional<pp::Handler> handler_;
    QHash<QString, QPushButton *> buttons_;
    QButtonGroup *group_ = nullptr;
    QGridLayout *grid_ = nullptr;
    QLabel *description_ = nullptr;
    QLabel *status_ = nullptr;
    QTimer *statusTimer_ = nullptr;
    bool applying_ = false;

    struct LiveRow { QLabel *key; QLabel *value; std::function<std::optional<QString>()> getter; };
    QList<LiveRow> liveRows_;
    QLabel *gpuLiveKey_ = nullptr, *gpuLiveValue_ = nullptr;
    QLabel *gpuHwKey_ = nullptr, *gpuHwValue_ = nullptr;
    QProcess *smiLive_ = nullptr;
};
