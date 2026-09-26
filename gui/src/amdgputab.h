#pragma once
// AMD GPU tab — overclock, undervolt and efficiency tuning for amdgpu cards
// (discrete Radeon or the CPU's integrated graphics). The form is built from
// what the driver reports for the selected card, so every generation gets
// only the controls it really has:
//   per-state table (Polaris/Vega10) · V/F curve (Vega20/Navi1x) ·
//   min/max clocks + voltage offset (Navi2x/RDNA3/APUs) · clock offset (RDNA4)
// plus performance level, power profile, power limit and PMFW fan settings.
// Writes go through amdgpu-helper; every Apply is a trial that reverts to
// the previous settings unless confirmed within REVERT_SECONDS.
#include "amdgpu.h"
#include <QJsonObject>
#include <QWidget>

class QCheckBox;
class QComboBox;
class QFrame;
class QGridLayout;
class QLabel;
class QPushButton;
class QSlider;
class QSpinBox;
class QTimer;
class QVBoxLayout;

class AmdGpuTab : public QWidget {
    Q_OBJECT
public:
    explicit AmdGpuTab(QWidget *parent = nullptr);
    static bool present() { return !amdgpu::cards().isEmpty(); }
    QStringList savedProfileNames() const;

protected:
    void showEvent(QShowEvent *e) override;
    void hideEvent(QHideEvent *e) override;

private:
    using Pair = QPair<QSpinBox *, QSpinBox *>;
    const amdgpu::Card *card() const;
    void selectCard(int i);
    void rebuildForm();
    QJsonObject settingsOf(const amdgpu::Snapshot &s) const;
    QJsonObject collect() const;
    void fill(const QJsonObject &s);
    void updateBanner();
    void pollLive();

    void apply(const QJsonObject &settings, bool trial);
    void resetCard();
    void startTrial();
    void endTrial(bool keep);
    void undervoltStep();
    void applyPreset(int which);

    void reloadProfiles();
    void saveProfile();
    void loadProfile();
    void deleteProfile();

    void setBusy(bool b);
    void status(const QString &msg, const char *color = nullptr);

    QList<amdgpu::Card> cards_;
    amdgpu::Snapshot snap_;
    QJsonObject before_;   // settings before the trial apply (the revert target)

    QComboBox *cardCombo_, *presetCombo_, *profileCombo_;
    QLabel *kindLabel_, *banner_, *live_, *status_, *stableLabel_;
    QVBoxLayout *formHost_;
    QWidget *form_ = nullptr;
    QFrame *trialBar_;
    QLabel *trialLabel_;
    QPushButton *applyBtn_, *resetBtn_, *uvBtn_;
    QTimer *liveTimer_, *trialTimer_;
    int trialLeft_ = 0;
    bool busy_ = false;

    // Form (null when the card lacks the control)
    QComboBox *perf_ = nullptr, *profile_ = nullptr;
    QSlider *capSlider_ = nullptr;
    QSpinBox *cap_ = nullptr, *sclkMin_ = nullptr, *sclkMax_ = nullptr, *mclkMin_ = nullptr,
             *mclkMax_ = nullptr, *sclkOffset_ = nullptr, *voltOffset_ = nullptr;
    QList<Pair> curve_, sclkStates_, mclkStates_, fanCurve_;
    QMap<QString, QSpinBox *> fan_;
    QCheckBox *zeroRpm_ = nullptr;
};
