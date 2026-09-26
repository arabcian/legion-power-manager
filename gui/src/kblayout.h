#pragma once
// Physical keyboard geometry for the Lighting tab.
//
// The Spectrum controller reports its LEDs as a coarse 22×9 matrix: keys sit
// in whatever column their LED is wired to, unused matrix slots read 0 and
// every key is one cell wide unless its code repeats. Drawn as-is that leaves
// holes in the middle of the keyboard and loses the stagger. This module maps
// controller codes to real key positions (key units) for the chassis we know,
// and resolves legends from the active XKB layout, so one ISO geometry serves
// TR-Q, TR-F, DE, UK… and legends follow the user's layout.
//
// Anything unrecognised (a code not in the table) makes forMap() return an
// empty list and the view falls back to the controller's matrix.
#include "lighting.h"
#include <QHash>
#include <QList>
#include <QRectF>
#include <QString>

namespace kblayout {

enum class Form { Unknown, Ansi, Iso };

struct Key {
    int code = 0;
    QRectF rect;          // key units; y in row units (the view applies the key aspect)
    int evdev = 0;        // Linux KEY_* code for XKB legends, 0 = fixed legend
    QString label;        // fixed / fallback legend
    bool bar = false;     // chassis accent LED (drawn as a thin bar)
};

/// Physical form the controller's map describes (ISO has the 102nd key and the ISO Enter row).
Form formOf(const lighting::KeyMap &m);
QString formName(Form f);

/// Geometry for every code in `m` (a code may have several rects, e.g. the ISO
/// Enter); empty when the map holds a code this table does not know.
QList<Key> forMap(const lighting::KeyMap &m);

/// Legends for the keys, from the active XKB layout when available
/// (LPM_XKB_LAYOUT / LPM_XKB_VARIANT override detection). Keys without an
/// evdev code, or when xkbcommon is unavailable, keep their fixed label.
QHash<int, QString> legends(const QList<Key> &keys);

/// "tr", "tr(f)", "us"… — the layout legends() used (for a tooltip / status line).
QString activeLayoutName();

} // namespace kblayout
