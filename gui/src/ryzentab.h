#pragma once
// Ryzen Curve Optimizer tab — port of ryzen_curve_optimizer.py.
// Per-slot CO offsets on the fixed 8-slot-per-CCD SMU grid, all-core
// offset, reset, and per-user profiles; writes via ryzen-co-helper.
#include <QJsonObject>
#include <QMap>
#include <QSet>
#include <QWidget>
#include <functional>
#include <optional>

class QButtonGroup;
class QCheckBox;
class QComboBox;
class QLabel;
class QLineEdit;
class QPlainTextEdit;
class QPushButton;

namespace ryzen {
struct PhysCore { QList<int> cpus; std::optional<int> highestPerf; };
struct Layout {
    int ccdCount = 0;
    QMap<int, QList<PhysCore>> cores;  // per CCD, ascending logical-CPU order
    QSet<int> parked;                  // CCDs parked by Optimizations (from tune-helper's park record)
};
/// CPUs of the CCD parked by Optimizations, if the park is still in effect.
QList<int> parkedCpus();
Layout detect();
QString profilesDir();
}

class RyzenTab : public QWidget {
    Q_OBJECT
public:
    explicit RyzenTab(QWidget *parent = nullptr);

    QStringList savedProfileNames() const;
    /// Load + apply in one step (tray). All-core then per-core, chained.
    bool applyNamedProfile(const QString &name);
    void applyReset();

protected:
    void showEvent(QShowEvent *e) override;

private:
    struct Slot {
        int ccd, slot;
        QLineEdit *entry;
        QCheckBox *disable;
        QWidget *row;
        QLabel *cppc;
    };
    void updateParked();
    enum class Parse { Disabled, Empty, Ok, Invalid, Range };
    std::pair<Parse, int> parse(const Slot &s) const;
    QList<Slot *> activeSlots();

    void setCoreMode(bool allCcds);
    void fillCcd(int ccd);
    void clearCores();
    bool applyAllCore(std::function<void()> then = {});
    bool applyPerCore(std::function<void()> then = {});
    void runOp(const QString &op, const QJsonObject &params, std::function<void()> then = {});
    void setBusy(bool b);
    void log(const QString &msg, const QString &level = "info");

    void reloadProfiles();
    void saveProfile();
    bool loadProfile();
    void deleteProfile();
    QJsonObject currentState();

    ryzen::Layout layout_;
    int ccdCount_ = 2;
    QList<Slot> slots_;
    QSet<int> activeCcds_;
    QMap<int, QWidget *> ccdColumns_;
    QMap<int, QLineEdit *> fillEntries_;
    QList<QPushButton *> applyButtons_;
    QLineEdit *coall_ = nullptr;
    QMap<int, QLabel *> ccdTitles_;
    QSet<int> parkedNow_;
    class QRadioButton *rAll_ = nullptr;
    QComboBox *profileCombo_ = nullptr;
    QPlainTextEdit *log_ = nullptr;
    bool busy_ = false, profilesReady_ = true;
};
