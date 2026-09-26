#pragma once
// Keyboard lighting — Lenovo Legion Gen10 "Spectrum" per-key RGB (ITE 8258,
// USB 048d:c1xx). Model shared by the Lighting tab, the tray and scenes, plus
// the call into lighting-helper.
//
// Access: lighting-helper runs as the user when the udev rule has tagged the
// controller's hidraw node `uaccess` (no pkexec, no prompt); only a reply
// with "denied" makes run() retry through pkexec.
//
// Effects live in the controller's non-volatile profile store (6 profiles,
// survive reboots and Windows/Linux switches). Per-key colour is one Static
// effect per colour group; the whole profile has to fit one 960-byte report.
#include "privileged.h"
#include <QColor>
#include <QHash>
#include <QJsonObject>
#include <QList>
#include <QString>
#include <QVector>

namespace lighting {

inline constexpr int REPORT_LEN = 960, MAX_BRIGHTNESS = 9, MIN_PROFILE = 1, MAX_PROFILE = 6;
inline constexpr int ALL_KEYS = 0x65;  // "every light" marker used by the audio/Aurora effects

/// Spectrum controller present (sysfs only — no device I/O, no helper).
bool present();

enum Type {
    ScrewRainbow = 1, RainbowWave, ColorChange, ColorPulse, ColorWave, Smooth, Rain, Ripple,
    AudioBounce, AudioRipple, Static, TypeLighting, AuroraSync,
};
QString typeName(int type);
/// Needs host-side audio/screen processing — not available on Linux, not offered.
bool needsHost(int type);
bool hasSpeed(int type);
bool hasDirection(int type);
bool hasClockwise(int type);
bool hasColorMode(int type);  // random colours or a colour list
bool multiColor(int type);    // colour list may hold several colours
QList<int> editableTypes();

enum ColorMode { NoColors = 0, RandomColors = 1, ColorList = 2 };

struct Effect {
    int type = Static, speed = 0, direction = 0, clockwise = 0, colorMode = ColorList;
    QList<QColor> colors;
    QList<int> keys;
    bool operator==(const Effect &) const = default;
    bool allKeys() const { return keys == QList<int>{ALL_KEYS}; }
};
QJsonObject toJson(const Effect &e);
Effect fromJson(const QJsonObject &o);
/// Bytes the list takes in the controller's report (limit REPORT_LEN).
int encodedLen(const QList<Effect> &effects);

enum class Zone { Keyboard, Perimeter, Logo };
bool isPerimeter(int kc);
/// Printed legend for a keycode (US layout), empty if unknown.
QString keyLabel(int kc);

struct KeyMap {
    int rows = 0, cols = 0;
    QVector<int> grid;   // row-major; 0 = empty; a wide key repeats its code
    QVector<int> extra;  // secondary page (lid logo)
    struct Span { int code, row, col, width; };

    static KeyMap fromJson(const QJsonObject &o);
    bool isEmpty() const { return rows == 0 || cols == 0; }
    int at(int r, int c) const { return grid.value(r * cols + c); }
    QList<int> unique() const;           // every code once, row-major, extras last
    Zone zoneOf(int kc) const;
    QList<int> zone(Zone z) const;
    QList<Span> spans() const;           // horizontal runs of one code per row
};

/// Starts lighting-helper with `req`. `elevate`: on "denied", repeat through
/// pkexec (polkit action com.legion-power-manager.lighting.write).
void run(const QJsonObject &req, QObject *ctx, privileged::Callback cb, bool elevate = true);

} // namespace lighting
