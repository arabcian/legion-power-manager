#pragma once
// Single source of truth for the look — port of theme.py.
#include <QColor>
#include <QString>

class QApplication;

namespace theme {
// Warm Gruvbox-derived dark. Surfaces step up in small, even increments
// (window → card → input → control → hover); borders are hairlines that
// only separate, they never frame. Amber is reserved for what is active:
// the selected tab, primary actions, slider fill, checked state, focus.
inline constexpr const char *BG0 = "#181614", *BG1 = "#211e1b", *BG2 = "#2a2623", *BG3 = "#342f2b",
    *BG4 = "#423b35", *BORDER = "#3b3530", *BORDER_SOFT = "#2b2724", *FG = "#eee5d5",
    *FG_DIM = "#c2b6a2", *MUTED = "#8a7f71", *ACCENT = "#e0a458", *ACCENT_SOFT = "#b07f45",
    *OK = "#a3b565", *WARN = "#e3c46a", *DANGER = "#db7b6e", *DANGER_SOFT = "#a85647",
    *INFO = "#8fb3ad", *PURPLE = "#bd8fa5";
inline constexpr int FONT_PT = 10, RADIUS = 8;

QString profileAccent(const QString &profile);
void apply(QApplication &app);
}
