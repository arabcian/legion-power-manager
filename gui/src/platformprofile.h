#pragma once
// Read-only access to the platform-profile class interface — port of
// platform_profile.py. Writes go through legion-profile-helper.
//
// Custom mode only works via /sys/class/platform-profile/*/profile: the
// legacy /sys/firmware/acpi/platform_profile store rejects "custom" with
// EINVAL, and lenovo-wmi-gamezone registers it as a *hidden* choice, so it
// is writable but never listed.
#include <QObject>
#include <QString>
#include <QStringList>
#include <optional>

namespace pp {

inline const QStringList VALID_PROFILES{
    "low-power", "cool", "quiet", "balanced", "balanced-performance", "performance", "max-power", "custom"};

std::optional<QString> readText(const QString &path);

struct Handler {
    QString node;         // platform-profile-N
    QString name;         // lenovo-wmi-gamezone
    QStringList choices;
    QString path() const;
    bool supportsCustom() const;
    std::optional<QString> current() const;
};

QList<Handler> handlers();
std::optional<Handler> primaryHandler();
bool available();
std::optional<QString> currentProfile(const std::optional<Handler> &h);
QStringList offeredProfiles(const std::optional<Handler> &h);

/// Event-driven profile change notifications, shared by every tab.
/// The platform-profile core calls sysfs_notify() on the profile files for
/// every change (Fn+Q, a helper write, the firmware), so the files are watched
/// with POLLPRI instead of being re-read every 2–3 s. A slow safety re-read
/// covers a kernel that does not notify; without any watchable file it falls
/// back to the old short poll.
class Watcher : public QObject {
    Q_OBJECT
public:
    static Watcher &instance();
    const std::optional<Handler> &handler() const { return handler_; }
    std::optional<QString> current() const { return current_; }
    /// Re-read now (after our own write, when the notification may lag).
    void check();

Q_SIGNALS:
    void changed(const QString &profile);

private:
    explicit Watcher(QObject *parent);
    void watch(const QString &path);
    std::optional<Handler> handler_;
    std::optional<QString> current_;
    QList<int> fds_;
};

} // namespace pp
