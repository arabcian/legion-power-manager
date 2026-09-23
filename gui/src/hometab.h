#pragma once
// Home tab: power-profile switcher + hardware inventory + live sensors.
// Port of home_tab.py.
#include "platformprofile.h"
#include <QHash>
#include <QWidget>
#include <functional>
#include <optional>

class QButtonGroup;
class QCheckBox;
class QComboBox;
class QLineEdit;
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
    QGroupBox *buildDeviceBox();  // nullptr when the machine exposes none of it
    void refreshDevice();
    void setDevice(const QString &key, const QString &value);
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

    // Device box (battery charge mode, ideapad toggles, fan targets)
    QString chargeFile_, ideapadDir_, fanHwmon_;
    QComboBox *charge_ = nullptr;
    QHash<QString, QCheckBox *> toggles_;
    struct FanRow { QString key; QLabel *rpm; QLineEdit *target; QCheckBox *autoBox; QCheckBox *maxBox; QPushButton *set; int max; };
    QList<FanRow> fans_;
    // EC "Full Speed" flag: separate from fanN_target and persistent across
    // reboots (e.g. switched on in Windows). fullSpeedFile_ is empty when the
    // running kernel exposes no interface for it.
    QString fullSpeedFile_;
    bool fullSpeedPwm_ = false;          // pwm1_enable (0 = full) vs legion fan_fullspeed (1 = full)
    QCheckBox *fullSpeed_ = nullptr;
    QLabel *fanWarn_ = nullptr;
    bool fullSpeedOn_ = false;           // last known / suspected state
    int fullSpeedGuess_ = 0;             // consecutive polls that look like Full Speed
    std::optional<bool> readFullSpeed() const;
    void clearFullSpeed();
    int devicePending_ = 0;
    QList<QPair<QString, QString>> deviceQueue_;  // writes clicked while one is in flight
};
