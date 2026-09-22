#pragma once
// Optimizations tab — the GUI for tune-helper / lpm-gamemode.
//
// Every row comes from `tune-helper {"op":"describe"}` (run unprivileged: the
// helper only reads), so the tab never hard-codes a sysfs path. A row is
// [include] [name] [current] [editor] [↺]; "include" decides what goes into
// Apply and into a saved preset. Presets live in
// ~/.config/legion-power-manager/tune-presets/<name>.json, the same files
// lpm-gamemode reads for Lutris/Steam, so "Use for games" is the whole
// integration. Built-in presets are templates in code; they are copied to a
// user file the moment they are chosen for games or for boot.
#include <QJsonArray>
#include <QJsonObject>
#include <QWidget>
#include <functional>

class QCheckBox;
class QComboBox;
class QGridLayout;
class QLabel;
class QLineEdit;
class QPushButton;
class QSpinBox;
class QTabWidget;
class QTimer;

class OptimizeTab : public QWidget {
    Q_OBJECT
public:
    explicit OptimizeTab(QWidget *parent = nullptr);

    /// Built-in and user presets, as shown in the combo (tray menu).
    QStringList presetNames() const;
    /// Load + apply as a manual change (tray). false if busy or unknown.
    bool applyNamedPreset(const QString &name);
    void restoreAll(bool confirm = true);
    bool busy() const { return busy_; }
    bool tuningActive() const { return active_; }
    QString gamePreset() const;

    static QString presetsDir();
    static QString configFile();

protected:
    void showEvent(QShowEvent *e) override;
    void hideEvent(QHideEvent *e) override;

private:
    struct Option { QString value, label; };
    struct Row {
        QString key, group, label, help, kind, current;
        QList<Option> options;
        qint64 min = 0, max = 0;
        bool available = false, debugfs = false, caution = false, hotplug = false;
        bool touched = false;  // user edited the editor since the last sync
        QCheckBox *include = nullptr;
        QLabel *name = nullptr, *cur = nullptr;
        QComboBox *combo = nullptr;
        QSpinBox *spin = nullptr;
        QPushButton *revert = nullptr;
    };

    void buildUi();
    QWidget *buildLaunchPage();
    void refresh();
    void onDescribe(const QJsonObject &d);
    void buildRows(const QJsonArray &rows);
    void updateRow(Row &r, const QJsonObject &o);
    void updateStateBanner();
    void updateLaunchPreview();

    static bool validFor(const Row &r, const QString &v);
    QString editorValue(const Row &r) const;
    bool setEditorValue(Row &r, const QString &v);
    bool differs(const Row &r) const;
    void markRow(Row &r);
    Row *row(const QString &key);

    QJsonObject collectValues() const;
    QJsonObject collectRun() const;
    QJsonObject presetObject(const QString &name) const;
    int loadPresetObject(const QJsonObject &p, QStringList *skipped);
    bool writeUserPreset(const QString &name, const QJsonObject &p, QString *err = nullptr);

    void reloadPresets(const QString &select = {});
    void loadSelected();
    void saveAs();
    void deleteSelected();
    void useForGames();
    void setBoot();
    void clearBoot();

    void applySelected();
    void applyValues(const QJsonObject &values, const QString &preset);
    void revertRow(const QString &key);
    void runOp(const QJsonObject &req, const QString &what, std::function<void(const QJsonObject &)> then = {});
    void setBusy(bool b);
    void showStatus(const QString &msg, const char *color = nullptr, int ms = 6000);

    QList<Row> rows_;
    QJsonObject topology_, state_, boot_;
    bool active_ = false, busy_ = false, describing_ = false, helperMissing_ = false;

    QLabel *banner_ = nullptr, *bannerDetail_ = nullptr, *status_ = nullptr, *bootLabel_ = nullptr;
    QPushButton *restoreBtn_ = nullptr, *applyBtn_ = nullptr, *gameBtn_ = nullptr, *bootBtn_ = nullptr, *bootClear_ = nullptr;
    QComboBox *presetCombo_ = nullptr, *affinity_ = nullptr;
    QSpinBox *nice_ = nullptr;
    QCheckBox *autogroup_ = nullptr;
    QLineEdit *lutrisPre_ = nullptr, *lutrisPost_ = nullptr, *lutrisPrefix_ = nullptr, *steam_ = nullptr;
    QLabel *topoLabel_ = nullptr;
    QTabWidget *groups_ = nullptr;
    QTimer *poll_ = nullptr, *statusTimer_ = nullptr;
    QString loadedPreset_;
};
