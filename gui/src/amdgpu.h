#pragma once
// AMD GPU (amdgpu driver) read side for the AMD GPU tab: card discovery and
// parsing of the driver's overdrive table, power profiles, power limit and
// PMFW fan files. Everything here only reads sysfs (world-readable); writes
// go through amdgpu-helper, which re-validates against the same files.
#include <QList>
#include <QMap>
#include <QPair>
#include <QString>
#include <optional>

namespace amdgpu {

struct Card {
    QString name;        // "card1"
    QString dev;         // /sys/class/drm/card1/device
    QString hwmon;       // its amdgpu hwmon dir, may be empty
    QString pciId;       // "1002:7480"
    QString label;       // shown in the card picker
    bool integrated = false;
};
QList<Card> cards();

/// amdgpu.ppfeaturemask; nullopt when the module is not loaded.
std::optional<quint64> featureMask();
inline constexpr quint64 OVERDRIVE_BIT = 0x4000;

struct State { int index; int mhz; int mv = -1; };  // mv -1: none
struct Od {
    enum Kind { None, PerState, Curve, MinMax, Offset } kind = None;
    QList<State> sclk, mclk, curve;
    std::optional<int> sclkOffset, voltageOffset;
    QMap<QString, QPair<int, int>> ranges;  // "SCLK", "MCLK", "VDDC", "VDDGFX_OFFSET", "SCLK_OFFSET", "VDDC_CURVE_SCLK[0]"…
    std::optional<QPair<int, int>> range(const QString &k) const {
        return ranges.contains(k) ? std::optional(ranges.value(k)) : std::nullopt;
    }
    std::optional<int> sclkAt(int idx) const;
    std::optional<int> mclkAt(int idx) const;
};
Od parseOd(const QByteArray &raw);

struct Profile { int index; QString name; bool active; };
QList<Profile> parseProfiles(const QByteArray &raw);

struct FanValue { int value; int min; int max; };
std::optional<FanValue> parseFanValue(const QByteArray &raw);
struct FanCurve { QList<QPair<int, int>> points; QPair<int, int> temp, speed; };
std::optional<FanCurve> parseFanCurve(const QByteArray &raw);

/// Everything the tab shows, read in one go.
struct Snapshot {
    Od od;
    bool odFilePresent = false;
    QString perfLevel;
    QList<Profile> profiles;
    std::optional<double> capW, capMinW, capMaxW, capDefaultW;
    QMap<QString, FanValue> fan;   // keys as in the helper: zero_rpm, min_pwm, …
    std::optional<FanCurve> fanCurve;
};
Snapshot read(const Card &c);

struct Live {
    std::optional<int> sclk, mclk, busy, fanRpm, vddgfxMv;
    std::optional<double> edgeC, junctionC, powerW;
};
Live readLive(const Card &c);

QByteArray readFile(const QString &path);

} // namespace amdgpu
