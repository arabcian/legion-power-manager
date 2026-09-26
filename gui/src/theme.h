#pragma once
// Single source of truth for the look — port of theme.py.
#include <QColor>
#include <QString>
#include <QWidget>

class QApplication;

namespace theme {
// Graphite with an amber signal. Surfaces step up in small, even increments
// (window → card → inner panel → control → hover); text you type into sits in
// a recessed WELL, below the card, so "type here" and "press here" read apart
// at a glance. Borders are hairlines that separate, they never frame. Amber is
// reserved for what is active: the selected tab, primary actions, slider fill,
// checked state, focus.
inline constexpr const char *BG0 = "#121316", *BG1 = "#191a1e", *BG2 = "#202126", *BG3 = "#292a30",
    *BG4 = "#34363d", *WELL = "#0e0f11", *BORDER = "#2f3137", *BORDER_SOFT = "#24252a", *FG = "#e8e6e3",
    *FG_DIM = "#b4b2ae", *MUTED = "#86868d", *ACCENT = "#eba55b", *ACCENT_SOFT = "#a9773f",
    *OK = "#8fc486", *WARN = "#e6c065", *DANGER = "#e8776c", *DANGER_SOFT = "#8e4841",
    *INFO = "#7fb1d5", *PURPLE = "#b79be2";
inline constexpr int FONT_PT = 10, RADIUS = 9;

/// Per-profile colour, after the Legion power-button LED: Quiet/Power Saver
/// blue, Balanced white, Performance red, Custom purple (Extreme: magenta).
QString profileAccent(const QString &profile);
/// "rgba(r, g, b, a)" of a theme colour — tints for banners and selections.
QString rgba(const char *hex, double alpha);
/// Callout frame: a faint wash of `color` with a matching hairline. `selector`
/// scopes it (e.g. "#tuneBanner") so child labels keep their own look.
QString banner(const char *color, const QString &selector = QString());
void apply(QApplication &app);
/// setStyleSheet() re-polishes the widget (and re-resolves every style rule
/// for it) even when the sheet is identical; the tabs that restyle a label on
/// every poll go through this so an unchanged state costs a string compare.
inline void setSheet(QWidget *w, const QString &css) { if (w->styleSheet() != css) w->setStyleSheet(css); }
}
