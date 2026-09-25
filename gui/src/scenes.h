#pragma once
// Scenes: one named state for the whole machine, built from the profiles the
// tabs already save — platform profile, firmware limits, CPU curve (Ryzen CO
// or Intel undervolt), NVIDIA curve, Optimizations preset, and an optional
// user command. A scene only *references* those profiles by name, so there is
// no second copy of any setting to drift out of sync.
//
// SceneEngine applies a scene step by step through the same root helpers the
// tabs use, strictly in dependency order (platform profile first: firmware
// limits only accept writes in Custom). It never opens dialogs — it runs from
// the tray and on power-source changes with the window hidden — and reports
// through finished(). Automatic switching watches the power source.
//
// Files (per user):
//   ~/.config/legion-power-manager/scenes/<name>.json
//   ~/.config/legion-power-manager/scenes.json      {"auto", "on_ac", "on_battery"}
#include <QJsonObject>
#include <QMap>
#include <QObject>
#include <QStringList>
#include <functional>
#include <optional>

class MainWindow;
class QTimer;

namespace scenes {

/// One component of a scene.
struct Choice {
    enum Kind { Unchanged, Reset, Profile } kind = Unchanged;
    QString name;  // Profile only
    bool operator==(const Choice &) const = default;
};

struct Scene {
    QString name;
    QString platformProfile;       // empty = unchanged
    QMap<QString, int> firmware;   // firmware-attribute name → value; empty = unchanged
    Choice cpu, gpu, tuning;       // tuning Reset = restore originals
    QString command;               // optional; run as the user, no shell
    bool operator==(const Scene &) const = default;
};

struct Auto {
    bool enabled = false;
    QString onAc, onBattery;       // scene names; empty = leave as is
};

QString dir();
bool validName(const QString &name);
QStringList names();
std::optional<Scene> load(const QString &name);
/// `tuningValues`: the Optimizations preset's values, stored next to its name
/// so lpm-gamemode can apply presets that only exist built into the GUI.
bool save(const Scene &s, QString *err = nullptr, const QJsonObject &tuningValues = {});
bool remove(const QString &name);
Auto loadAuto();
bool saveAuto(const Auto &a, QString *err = nullptr);

/// true = on mains (barrel or USB-C PD), false = on battery,
/// nullopt = the machine reports no power supplies (cannot tell).
std::optional<bool> onAc();

/// Shared with lpm-gamemode ($XDG_RUNTIME_DIR/legion-power-manager/scene.json):
/// the active scene, whoever applied it, and the game-launch bookkeeping.
QString activeScene();
void setActiveScene(const QString &name);
/// Game sessions currently holding game mode (tune-helper's state).
int gameSessions();

/// lpm-boot-guard state (/var/lib/legion-power-manager/boot-guard.json):
/// non-empty reason = boot presets paused after a crashed boot.
QString bootGuardReason();
/// Login guard: the automatic scene at login is paused because the last
/// login's scene apply was followed by a crash. Empty = not paused.
QString loginGuardReason();
void resumeLoginGuard();

} // namespace scenes

class SceneEngine : public QObject {
    Q_OBJECT
public:
    explicit SceneEngine(MainWindow *win);

    /// Applies a saved scene. While one is running, the latest request is
    /// queued and runs right after (an AC flip mid-apply is never lost).
    void apply(const QString &name);
    bool busy() const { return busy_; }
    /// Active scene (applied by the GUI or by lpm-gamemode at game start/exit).
    QString active() const { return scenes::activeScene(); }

    scenes::Auto autoConfig() const { return auto_; }
    /// Persists the setting; enabling it applies the matching scene at once.
    bool setAuto(const scenes::Auto &a, QString *err = nullptr);
    std::optional<bool> powerSource() const { return ac_; }

Q_SIGNALS:
    void started(const QString &name);
    /// `log` holds one line per component, failures prefixed with "✗".
    void finished(const QString &name, bool ok, const QStringList &log);
    void powerSourceChanged(bool onAc);

private:
    using Done = std::function<void(bool ok, const QString &msg)>;
    using Step = std::function<void(Done)>;

    void start(const scenes::Scene &s);
    void addStep(const QString &what, Step step);
    void next();
    void finish();
    void pollPower();
    void applyForSource(bool onAc);
    void startupApply();
    void helper(const QString &name, const QJsonObject &req, Done done,
                std::function<QString(const QJsonObject &)> describe = {});

    MainWindow *win_;
    QList<QPair<QString, Step>> steps_;
    QStringList log_;
    bool ok_ = true, busy_ = false;
    QString current_, pending_;
    bool deferred_ = false;  // a power-source switch waiting for the game to end

    scenes::Auto auto_;
    QTimer *powerTimer_;
    std::optional<bool> ac_, candidate_;
    int stableReads_ = 0;
};
