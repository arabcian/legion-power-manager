#pragma once
// Single source of truth for the look — port of theme.py.
#include <QColor>
#include <QString>

class QApplication;

namespace theme {
inline constexpr const char *BG0 = "#1c1a18", *BG1 = "#24211e", *BG2 = "#2d2926", *BG3 = "#3a3430",
    *BG4 = "#4a423b", *BORDER = "#4a423b", *BORDER_SOFT = "#332e2a", *FG = "#ece2d0",
    *FG_DIM = "#c8bba6", *MUTED = "#8a7f70", *ACCENT = "#e0a458", *ACCENT_SOFT = "#a8783d",
    *OK = "#a3b565", *WARN = "#e3c46a", *DANGER = "#db7b6e", *DANGER_SOFT = "#a85647",
    *INFO = "#8fb3ad", *PURPLE = "#bd8fa5";
inline constexpr int FONT_PT = 10, RADIUS = 6;

QString profileAccent(const QString &profile);
void apply(QApplication &app);
}
