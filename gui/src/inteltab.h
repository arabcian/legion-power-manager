#pragma once
// Intel Undervolt tab — OC mailbox voltage offsets (5 planes), IccMax,
// TCC offset and package power limits, via intel-uv-helper (pkexec).
// Created only when the CPU vendor is GenuineIntel (see MainWindow).
#include <QJsonArray>
#include <QHash>
#include <QJsonObject>
#include <QWidget>
#include <functional>

class QCheckBox;
class QComboBox;
class QDoubleSpinBox;
class QLabel;
class QPlainTextEdit;
class QPushButton;
class QSpinBox;
class QTimer;

namespace inteluv {
QString profilesDir();  // ~/.config/legion-power-manager/intel-uv-profiles
}

class IntelTab : public QWidget {
    Q_OBJECT
public:
    explicit IntelTab(QWidget *parent = nullptr);

    QStringList savedProfileNames() const;
    /// Load + apply in one step (tray / game mode).
    bool applyNamedProfile(const QString &name);
    void applyReset();

protected:
    void showEvent(QShowEvent *e) override;
    void hideEvent(QHideEvent *e) override;

private:
    struct Row { QString key; QCheckBox *on; QDoubleSpinBox *val; QLabel *cur; };
    struct PlRow { QCheckBox *on; QDoubleSpinBox *w, *s; QLabel *cur; };

    QJsonObject currentProfile() const;
    void setProfile(const QJsonObject &p);
    bool validate(const QJsonObject &p);
    void readStatus();
    void showStatus(const QJsonObject &s);
    void runOp(const QJsonObject &req, std::function<void(const QJsonObject &)> then = {});
    void setBusy(bool b);
    void setPositive(bool on);
    void importThrottleStop();
    void saveBoot();
    void pollMonitor();
    void showMonitor(const QJsonObject &s);
    void updateLimits(const QJsonObject &limits);
    void resetLimits();
    QJsonArray hwpRules() const;
    void log(const QString &msg, const QString &level = "info");

    void reloadProfiles();
    void saveProfile();
    bool loadProfile(const QString &name);
    void deleteProfile();

    QList<Row> volt_, icc_;
    QCheckBox *link_ = nullptr, *tjOn_ = nullptr, *mchbar_ = nullptr;
    QSpinBox *tj_ = nullptr;
    QLabel *tjCur_ = nullptr, *info_ = nullptr, *bootLbl_ = nullptr, *plLock_ = nullptr;
    PlRow pl1_{}, pl2_{};
    QComboBox *profileCombo_ = nullptr;
    QPlainTextEdit *log_ = nullptr;
    QList<QPushButton *> buttons_;
    // extras (undervolt.py --force/--lock, throttled BDPROCHOT/cTDP)
    QCheckBox *positive_ = nullptr, *lock_ = nullptr;
    QComboBox *bdprochot_ = nullptr, *ctdp_ = nullptr;
    // boot / daemon (throttled AC/BATTERY + Update_Rate_s, intel-undervolt daemon/hwphint)
    QComboBox *bootTarget_ = nullptr;
    QSpinBox *interval_ = nullptr;
    QCheckBox *reapply_ = nullptr, *hwpOn_ = nullptr, *hwpMulti_ = nullptr;
    QComboBox *hwpMode_ = nullptr, *hwpAlgo_ = nullptr, *hwpLoadHint_ = nullptr, *hwpNormalHint_ = nullptr, *hwpDomain_ = nullptr, *hwpCmp_ = nullptr;
    QDoubleSpinBox *hwpThreshold_ = nullptr, *hwpWatts_ = nullptr;
    // live monitor (throttled --monitor)
    QCheckBox *monOn_ = nullptr;
    QLabel *monLbl_ = nullptr;
    QTimer *monTimer_ = nullptr;
    QJsonObject monPrev_;
    // perf-limit-reason counters (MSR 0x64F / 0x6B0 / 0x6B1)
    QHash<QString, QLabel *> limCells_;   // "core:10" → cell
    QHash<QString, int> limCounts_;       // samples in which the reason was active
    QLabel *limInfo_ = nullptr;
    QPushButton *limRun_ = nullptr;
    int limSamples_ = 0;
    bool clearLogsNext_ = false;
    bool busy_ = false, readOnce_ = false, monInFlight_ = false;
};
