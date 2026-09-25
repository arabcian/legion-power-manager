#pragma once
// NVIDIA Curve Optimizer tab — port of nvcurve_gui.py (embedded mode).
// Reads/applies the V/F curve, memory offset and VRAM lock through
// nvcurve-root-helper; live stats via NVML (dlopen, only while visible).
#include <QHash>
#include <QJsonObject>
#include <QPointF>
#include <QVector>
#include <QWidget>
#include <functional>

class QComboBox;
class QLabel;
class QPlainTextEdit;
class QPushButton;
class QSpinBox;
class QTimer;
class VfCurveWidget;
class Nvml;

class NvidiaTab : public QWidget {
    Q_OBJECT
public:
    explicit NvidiaTab(QWidget *parent = nullptr);
    ~NvidiaTab() override;

    QStringList profileNames() const;
    QString defaultProfileName() const;
    bool busy() const { return busy_; }
    /// Tray quick-apply: apply a saved profile by name (no JSON rewrite).
    void applyNamedProfile(const QString &name);
    void resetCurve();

Q_SIGNALS:
    void profilesChanged();

protected:
    void showEvent(QShowEvent *) override;
    void hideEvent(QHideEvent *) override;

private:
    using Done = std::function<void(bool ok, const QJsonObject &reply)>;
    void runHelper(const QJsonObject &payload, const QString &okMsg, const QString &failMsg, Done done = {});
    void setBusy(bool b);
    void log(const QString &s);

    // model
    void recompute();
    int offsetOf(int i) const { return pointOffsets_.value(i, 0) + coreOffset_; }
    void setOffsetClamped(int i, int total);
    void updateCoreOffsetUi();
    void onSelectionChanged();
    void resetGraphToLastRead();
    bool loadCurveFile(const QString &file, qint64 notBeforeMs, QJsonArray *out);
    void applyMemOffset(const QJsonObject &result);
    void takeGpuPoints(const QJsonArray &pts, bool keepOffsets);

    // actions
    void readCurve();
    void applyOffsets();
    void vramLock();
    void vramUnlock();
    void saveProfileAs();
    void refreshProfiles();
    void onProfileSelected();
    void applyProfileToUi(const QJsonObject &data, const QString &name);
    void toggleDefault();
    void deleteProfile();
    void pollStats();

    VfCurveWidget *vf_;
    QLabel *temp_, *power_, *clock_, *memClock_;
    QLabel *selLabel_, *voltLabel_, *freqLabel_, *offLabel_;
    QSpinBox *pointSpin_, *flattenSpin_, *coreSpin_, *memSpin_, *lockMinSpin_, *lockMaxSpin_;
    QSpinBox *coreCapSpin_ = nullptr;  // NVML core clock cap (MHz, 0 = none)
    void reportClamping(const QVector<QPointF> &baseBefore, const QHash<int, int> &requested);
    QComboBox *profiles_;
    QPlainTextEdit *log_;
    QList<QPushButton *> actionButtons_;
    QPushButton *readBtn_, *resetBtn_;
    QTimer *statsTimer_;
    Nvml *nvml_ = nullptr;

    QVector<QPointF> base_;          // (mV, base MHz) of GPU-domain points
    QHash<int, int> pointOffsets_;   // per-point MHz, on top of coreOffset_
    QHash<int, int> readOffsets_;
    int coreOffset_ = 0;
    bool curveModified_ = false;
    bool busy_ = false;
    bool firstShow_ = true;
};
