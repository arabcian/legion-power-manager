#pragma once
// Read-only access to the platform-profile class interface — port of
// platform_profile.py. Writes go through legion-profile-helper.
//
// Custom mode only works via /sys/class/platform-profile/*/profile: the
// legacy /sys/firmware/acpi/platform_profile store rejects "custom" with
// EINVAL, and lenovo-wmi-gamezone registers it as a *hidden* choice, so it
// is writable but never listed.
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

} // namespace pp
