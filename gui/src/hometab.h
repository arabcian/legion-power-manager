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
class QFrame;
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

    // Fan control for the tray menu (same sysfs writes / EC rules as the Home tab rows).
    struct FanInfo { QString key; QString name; int min; int max; int target; };  // target 0 = Auto
    QList<FanInfo> fanInfo() const;
    bool fansMaxMode() const { return maxMode_; }
    bool fanBusy() const { return devicePending_ > 0; }
    void setAllFansMax();
    void setAllFansAuto() { exitMaxMode(); }
    void setFanTarget(const QString &key, int rpm);  // rpm 0 = Auto (EC all-or-nothing rule applies)

Q_SIGNALS:
    /// Emitted after a successful switch (Firmware Attributes tab unlocks on "custom").
    void profileChanged(const QString &profile);

protected:
    void showEvent(QShowEvent *e) override;

private:
    void rebuild();
    // GPU mode (MUX): read once on first show, switched through legion-gpu-helper.
    void readGpuMode();
    void setGpuMode(const QString &mode, bool force);
    QComboBox *gpuMode_ = nullptr;
    bool gpuModeRead_ = false;
    // Banner when lpm-boot-guard / the login guard paused presets after a crash.
    void refreshGuard();
    QFrame *guardBanner_ = nullptr;
    QLabel *guardText_ = nullptr;
    QPushButton *resumeBoot_ = nullptr, *resumeLogin_ = nullptr;
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
    bool liveBusy_ = false;            // a sensor sweep is running on the thread pool
    // dGPU runtime PM: nvidia-smi wakes the GPU, so it is skipped while the
    // GPU sleeps and throttled while it idles (see refreshGpuLive()).
    QString dgpuRuntimeStatus_;        // <pci>/power/runtime_status of the NVIDIA dGPU
    bool dgpuProbed_ = false;
    qint64 nextSmiAt_ = 0;             // monotonic ms; no nvidia-smi before this
    void refreshGpuLive();

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
    // The RPM heuristic only means "firmware Full Speed" before this session has
    // written any fan target: after our own Max → Auto the EC resets the
    // targets to 0 while the fans are still spinning down, which looks the same.
    bool fanTouched_ = false;
    bool fullSpeedAtStart_ = false;      // detected before any write; sticky until the fans actually slow
    // WMAE fallback (Legion EC FNST via acpi_call): root-only, so the state is
    // cached and re-read through the helper only when the RPMs disagree with it.
    bool fullSpeedWmae_ = false;
    std::optional<bool> wmaeFullSpeed_;
    bool fsQueryPending_ = false;
    qint64 nextFsQueryAt_ = 0;
    void queryFullSpeed();
    std::optional<bool> readFullSpeed() const;
    void clearFullSpeed();
    // The Legion EC only returns fans to its own curve when every fan target is
    // 0: with any fan still manual, a fan set to 0 just keeps its last speed.
    void setFanAuto(const QString &key);
    int autoAllChoice_ = 0;
    // "Max fans" mode: every fan at its maximum (or EC Full Speed on). The per-fan
    // controls are greyed out behind a banner; one button returns all to Auto.
    QFrame *maxBanner_ = nullptr;
    QLabel *maxBannerText_ = nullptr;
    QPushButton *maxBannerBtn_ = nullptr;
    QPushButton *maxAllBtn_ = nullptr;
    bool maxMode_ = false;
    void setMaxMode(bool on, bool ecFullSpeed);
    void exitMaxMode();              // session memory: 0 ask, 1 all fans, 2 only this one
    int devicePending_ = 0;
    QList<QPair<QString, QString>> deviceQueue_;  // writes clicked while one is in flight
};
