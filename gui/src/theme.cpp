#include "theme.h"
#include <QApplication>
#include <QHash>
#include <QPalette>
#include <QStyleFactory>

namespace theme {

QString profileAccent(const QString &p) {
    static const QHash<QString, QString> m{
        {"low-power", INFO}, {"quiet", "#7fa8a0"}, {"cool", "#8fb3ad"}, {"balanced", OK},
        {"balanced-performance", WARN}, {"performance", ACCENT}, {"max-power", DANGER}, {"custom", PURPLE}};
    return m.value(p, MUTED);
}

static QPalette palette() {
    QPalette p;
    auto c = [](const char *s) { return QColor(QString::fromLatin1(s)); };
    p.setColor(QPalette::Window, c(BG0));        p.setColor(QPalette::WindowText, c(FG));
    p.setColor(QPalette::Base, c(BG2));          p.setColor(QPalette::AlternateBase, c(BG1));
    p.setColor(QPalette::ToolTipBase, c(BG2));   p.setColor(QPalette::ToolTipText, c(FG));
    p.setColor(QPalette::Text, c(FG));           p.setColor(QPalette::Button, c(BG3));
    p.setColor(QPalette::ButtonText, c(FG));     p.setColor(QPalette::BrightText, c(DANGER));
    p.setColor(QPalette::Link, c(INFO));         p.setColor(QPalette::Highlight, c(ACCENT));
    p.setColor(QPalette::HighlightedText, c(BG0)); p.setColor(QPalette::PlaceholderText, c(MUTED));
    p.setColor(QPalette::Mid, c(BORDER));        p.setColor(QPalette::Light, c(BG4));
    p.setColor(QPalette::Dark, c(BG0));
    for (auto r : {QPalette::WindowText, QPalette::Text, QPalette::ButtonText})
        p.setColor(QPalette::Disabled, r, c(MUTED));
    p.setColor(QPalette::Disabled, QPalette::Base, c(BG1));
    p.setColor(QPalette::Disabled, QPalette::Button, c(BG1));
    return p;
}

// Selectors (objectName / dynamic properties) are the contract with the tabs:
// btnAccent, btnDanger, btnMini, terminal, miniSlider, box_<colour>,
// role=muted/title. Only the look behind them changes here.
static QString stylesheet() {
    QString s = QStringLiteral(R"QSS(
QWidget { background: @BG0; color: @FG; font-size: @FONTpt; }
/* Plain layout containers (exact class QWidget, not subclasses) take their
   parent's surface instead of punching a window-coloured hole into a card. */
.QWidget { background: transparent; }
QToolTip { background: @BG3; color: @FG; border: 1px solid @BORDER; border-radius: 6px; padding: 5px 8px; }

/* ── tabs: text tabs on a hairline, amber underline marks the current one ── */
QTabWidget::pane { border: none; border-top: 1px solid @BORDER_SOFT; background: @BG0; top: -1px; }
QTabBar { qproperty-drawBase: 0; background: transparent; }
QTabBar::tab { background: transparent; color: @MUTED; padding: 6px 12px 5px 12px; margin-right: 2px;
  border: none; border-bottom: 2px solid transparent; }
QTabBar::tab:hover:!selected { color: @FG_DIM; border-bottom-color: @BG4; }
QTabBar::tab:selected { color: @FG; font-weight: 600; border-bottom-color: @ACCENT; }
QTabBar::tab:disabled { color: @BG4; }

/* ── sections: a flat card with its title set above it as a heading ── */
QGroupBox { background: @BG1; border: 1px solid @BORDER_SOFT; border-radius: @RADpx; margin-top: 15px;
  padding: 4px 8px 4px 8px; font-weight: 600; color: @FG_DIM; }
QGroupBox::title { subcontrol-origin: margin; subcontrol-position: top left; left: 3px; top: 0px;
  padding: 0; color: @FG_DIM; background: transparent; }
QGroupBox#box_blue::title { color: @INFO; } QGroupBox#box_purple::title { color: @PURPLE; }
QGroupBox#box_yellow::title { color: @WARN; } QGroupBox#box_green::title { color: @OK; }
QGroupBox#box_grey::title { color: @MUTED; }

QLabel { background: transparent; }
QLabel[role="muted"] { color: @MUTED; font-size: 9pt; }
QLabel[role="title"] { color: @FG; font-size: 13pt; font-weight: 600; }
QFrame[frameShape="4"], QFrame[frameShape="5"] { color: @BORDER_SOFT; }

/* ── buttons: flat, one step above the surface; amber = primary ── */
QPushButton { background: @BG3; color: @FG; border: 1px solid @BORDER; border-radius: 6px;
  padding: 3px 10px; font-weight: 600; min-height: 16px; }
QPushButton:hover { background: @BG4; border-color: @BG4; }
QPushButton:pressed { background: @BG2; border-color: @BORDER; }
QPushButton:focus { border-color: @ACCENT_SOFT; }
QPushButton:disabled { background: transparent; color: @MUTED; border-color: @BORDER_SOFT; }
QPushButton:checked { background: @BG4; border-color: @ACCENT_SOFT; }
QPushButton#btnAccent { background: @ACCENT; border-color: @ACCENT; color: @BG0; }
QPushButton#btnAccent:hover { background: #ebb670; border-color: #ebb670; }
QPushButton#btnAccent:pressed { background: @ACCENT_SOFT; border-color: @ACCENT_SOFT; }
QPushButton#btnDanger { background: transparent; border-color: @DANGER_SOFT; color: @DANGER; }
QPushButton#btnDanger:hover { background: @DANGER_SOFT; border-color: @DANGER_SOFT; color: @FG; }
QPushButton#btnAccent:disabled, QPushButton#btnDanger:disabled { background: transparent; color: @MUTED; border-color: @BORDER_SOFT; }
QPushButton#btnMini { padding: 1px 8px; min-height: 12px; font-size: 9pt; }
QToolButton { background: transparent; border: 1px solid transparent; border-radius: 6px; padding: 2px; }
QToolButton:hover { background: @BG3; }

/* ── inputs: filled wells, no frame until focused ── */
QLineEdit, QSpinBox, QDoubleSpinBox, QComboBox { background: @BG2; color: @FG; border: 1px solid @BORDER_SOFT;
  border-radius: 6px; padding: 1px 6px; selection-background-color: @ACCENT_SOFT; selection-color: @FG; }
QLineEdit:hover, QSpinBox:hover, QDoubleSpinBox:hover, QComboBox:hover { border-color: @BORDER; }
QLineEdit:focus, QSpinBox:focus, QDoubleSpinBox:focus, QComboBox:focus { border-color: @ACCENT_SOFT; }
QLineEdit:disabled, QSpinBox:disabled, QDoubleSpinBox:disabled, QComboBox:disabled { background: @BG1; color: @MUTED; border-color: @BORDER_SOFT; }
QLineEdit:read-only { background: @BG1; }
QPlainTextEdit, QTextEdit { background: @BG2; color: @FG; border: 1px solid @BORDER_SOFT; border-radius: 6px;
  padding: 4px 6px; selection-background-color: @ACCENT_SOFT; }
QComboBox { padding-right: 20px; }
QComboBox::drop-down { subcontrol-origin: padding; subcontrol-position: center right; border: none; width: 18px; }
QComboBox::down-arrow { image: url(:/theme/chev-down.png); width: 10px; height: 10px; }
QComboBox::down-arrow:disabled { image: url(:/theme/chev-down-off.png); }
QComboBox QAbstractItemView { background: @BG2; color: @FG; border: 1px solid @BORDER; padding: 3px;
  selection-background-color: @BG4; selection-color: @FG; outline: none; }
QSpinBox, QDoubleSpinBox { padding-right: 16px; }
QSpinBox::up-button, QDoubleSpinBox::up-button { subcontrol-origin: border; subcontrol-position: top right;
  width: 16px; border: none; border-top-right-radius: 6px; background: transparent; }
QSpinBox::down-button, QDoubleSpinBox::down-button { subcontrol-origin: border; subcontrol-position: bottom right;
  width: 16px; border: none; border-bottom-right-radius: 6px; background: transparent; }
QSpinBox::up-button:hover, QSpinBox::down-button:hover,
QDoubleSpinBox::up-button:hover, QDoubleSpinBox::down-button:hover { background: @BG4; }
QSpinBox::up-arrow, QDoubleSpinBox::up-arrow { image: url(:/theme/chev-up.png); width: 8px; height: 8px; }
QSpinBox::down-arrow, QDoubleSpinBox::down-arrow { image: url(:/theme/chev-down.png); width: 8px; height: 8px; }
QSpinBox::up-arrow:disabled, QSpinBox::up-arrow:off,
QDoubleSpinBox::up-arrow:disabled, QDoubleSpinBox::up-arrow:off { image: url(:/theme/chev-up-off.png); }
QSpinBox::down-arrow:disabled, QSpinBox::down-arrow:off,
QDoubleSpinBox::down-arrow:disabled, QDoubleSpinBox::down-arrow:off { image: url(:/theme/chev-down-off.png); }
QPlainTextEdit#terminal { background: @BG0; color: @OK; border: 1px solid @BORDER_SOFT; border-radius: 6px; font-family: monospace; }

/* ── toggles ── */
QCheckBox, QRadioButton { background: transparent; spacing: 6px; }
QCheckBox::indicator, QRadioButton::indicator { width: 14px; height: 14px; border: 1px solid @BORDER; background: @BG2; }
QCheckBox::indicator { border-radius: 4px; } QRadioButton::indicator { border-radius: 8px; }
QCheckBox::indicator:hover, QRadioButton::indicator:hover { border-color: @ACCENT_SOFT; }
QCheckBox::indicator:checked { background: @ACCENT; border-color: @ACCENT; image: url(:/theme/check.png); }
QCheckBox::indicator:indeterminate { background: @ACCENT_SOFT; border-color: @ACCENT_SOFT; }
QRadioButton::indicator:checked { border-color: @ACCENT;
  background: qradialgradient(cx:0.5, cy:0.5, radius:0.5, fx:0.5, fy:0.5, stop:0 @ACCENT, stop:0.42 @ACCENT, stop:0.52 @BG2, stop:1 @BG2); }
QCheckBox::indicator:disabled, QRadioButton::indicator:disabled { background: @BG1; border-color: @BORDER_SOFT; }
QCheckBox::indicator:checked:disabled { background: @BG4; border-color: @BG4; }
QRadioButton::indicator:checked:disabled { border-color: @BG4;
  background: qradialgradient(cx:0.5, cy:0.5, radius:0.5, fx:0.5, fy:0.5, stop:0 @BG4, stop:0.42 @BG4, stop:0.52 @BG1, stop:1 @BG1); }
QCheckBox:disabled, QRadioButton:disabled { color: @MUTED; }

/* ── sliders: thin rail, amber fill, a small solid knob ── */
QSlider::groove:horizontal { height: 4px; background: @BG3; border-radius: 2px; margin: 0; }
QSlider::sub-page:horizontal { background: @ACCENT_SOFT; border-radius: 2px; }
QSlider::handle:horizontal { background: @FG; border: 2px solid @BG1; width: 10px; height: 10px; margin: -6px 0; border-radius: 7px; }
QSlider::handle:horizontal:hover { background: @ACCENT; }
QSlider::handle:horizontal:disabled { background: @MUTED; }
QSlider#miniSlider::groove:horizontal { height: 3px; border-radius: 1px; }
QSlider#miniSlider::sub-page:horizontal { border-radius: 1px; }
QSlider::sub-page:horizontal:disabled { background: @BG4; }
QSlider#miniSlider::handle:horizontal { width: 8px; height: 8px; margin: -5px 0; border-radius: 6px; }

/* ── scrolling ── */
QScrollArea { border: none; background: transparent; }
QScrollArea > QWidget > QWidget { background: transparent; }
QScrollBar:vertical { background: transparent; width: 8px; margin: 2px 0; }
QScrollBar::handle:vertical { background: @BG4; border-radius: 4px; min-height: 28px; }
QScrollBar::handle:vertical:hover, QScrollBar::handle:horizontal:hover { background: @MUTED; }
QScrollBar:horizontal { background: transparent; height: 8px; margin: 0 2px; }
QScrollBar::handle:horizontal { background: @BG4; border-radius: 4px; min-width: 28px; }
QScrollBar::add-line, QScrollBar::sub-line { height: 0; width: 0; }
QScrollBar::add-page, QScrollBar::sub-page { background: transparent; }

QStatusBar { background: @BG0; color: @MUTED; font-size: 9pt; } QStatusBar::item { border: none; }

/* ── menus (tray) ── */
QMenu { background: @BG2; color: @FG; border: 1px solid @BORDER; padding: 4px; }
QMenu::item { padding: 4px 22px 4px 10px; border-radius: 4px; background: transparent; }
QMenu::item:selected { background: @BG4; }
QMenu::item:disabled { color: @MUTED; }
QMenu::separator { height: 1px; background: @BORDER_SOFT; margin: 4px 6px; }
QMenu::indicator { width: 12px; height: 12px; left: 4px; }
QMenu::indicator:checked { image: url(:/theme/check.png); background: @ACCENT; border-radius: 3px; }
QMenuBar { background: @BG0; } QMenuBar::item:selected { background: @BG3; }
)QSS");
    // Longest names first so @BG doesn't eat @BG0 etc.
    const std::pair<const char *, QString> toks[] = {
        {"@BORDER_SOFT", BORDER_SOFT}, {"@ACCENT_SOFT", ACCENT_SOFT}, {"@DANGER_SOFT", DANGER_SOFT},
        {"@FG_DIM", FG_DIM}, {"@BORDER", BORDER}, {"@ACCENT", ACCENT}, {"@DANGER", DANGER},
        {"@MUTED", MUTED}, {"@PURPLE", PURPLE}, {"@INFO", INFO}, {"@WARN", WARN}, {"@OK", OK},
        {"@BG0", BG0}, {"@BG1", BG1}, {"@BG2", BG2}, {"@BG3", BG3}, {"@BG4", BG4}, {"@FG", FG},
        {"@FONT", QString::number(FONT_PT)}, {"@RAD", QString::number(RADIUS)}};
    for (const auto &[k, v] : toks) s.replace(QString::fromLatin1(k), v);
    return s;
}

void apply(QApplication &app) {
    // Fusion honours all of the QSS; native styles each ignore a different subset.
    app.setStyle(QStyleFactory::create(QStringLiteral("Fusion")));
    app.setPalette(palette());
    app.setStyleSheet(stylesheet());
}

} // namespace theme
