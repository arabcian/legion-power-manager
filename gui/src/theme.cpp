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

// Same QSS as theme.py, with the tokens substituted.
static QString stylesheet() {
    QString s = QStringLiteral(R"QSS(
QWidget { background: @BG0; color: @FG; font-size: @FONTpt; }
QToolTip { background: @BG2; color: @FG; border: 1px solid @BORDER; padding: 4px 6px; }
QTabWidget::pane { border: 1px solid @BORDER_SOFT; border-radius: @RADpx; background: @BG0; top: -1px; }
QTabBar { qproperty-drawBase: 0; }
QTabBar::tab { background: transparent; color: @MUTED; padding: 6px 14px; margin-right: 2px;
  border: 1px solid transparent; border-top-left-radius: @RADpx; border-top-right-radius: @RADpx; }
QTabBar::tab:hover:!selected { color: @FG_DIM; background: @BG1; }
QTabBar::tab:selected { background: @BG1; color: @FG; border-color: @BORDER_SOFT;
  border-bottom: 2px solid @ACCENT; font-weight: 600; }
QGroupBox { background: @BG1; border: 1px solid @BORDER_SOFT; border-radius: @RADpx; margin-top: 11px;
  padding: 8px 8px 6px 8px; font-weight: 600; color: @FG_DIM; }
QGroupBox::title { subcontrol-origin: margin; subcontrol-position: top left; left: 9px; padding: 0 5px; color: @ACCENT; }
QGroupBox#box_blue::title { color: @INFO; } QGroupBox#box_purple::title { color: @PURPLE; }
QGroupBox#box_yellow::title { color: @WARN; } QGroupBox#box_green::title { color: @OK; }
QGroupBox#box_grey::title { color: @MUTED; }
QLabel { background: transparent; }
QLabel[role="muted"] { color: @MUTED; font-size: 9pt; }
QLabel[role="title"] { color: @FG; font-size: 13pt; font-weight: 600; }
QPushButton { background: @BG3; color: @FG; border: 1px solid @BORDER; border-radius: 5px; padding: 4px 9px; font-weight: 600; }
QPushButton:hover { background: @BG4; border-color: @ACCENT_SOFT; }
QPushButton:pressed { background: @BG2; }
QPushButton:disabled { background: @BG1; color: @MUTED; border-color: @BORDER_SOFT; }
QPushButton#btnAccent { background: @ACCENT_SOFT; border-color: @ACCENT; color: @BG0; }
QPushButton#btnAccent:hover { background: @ACCENT; }
QPushButton#btnDanger { background: @BG2; border-color: @DANGER_SOFT; color: @DANGER; }
QPushButton#btnDanger:hover { background: @DANGER_SOFT; color: @FG; }
QLineEdit, QSpinBox, QDoubleSpinBox, QComboBox { background: @BG2; color: @FG; border: 1px solid @BORDER_SOFT;
  border-radius: 4px; padding: 2px 6px; selection-background-color: @ACCENT_SOFT; }
QPlainTextEdit, QTextEdit { background: @BG2; color: @FG; border: 1px solid @BORDER_SOFT; border-radius: 4px;
  padding: 4px 6px; selection-background-color: @ACCENT_SOFT; }
QLineEdit:focus, QSpinBox:focus, QDoubleSpinBox:focus, QComboBox:focus { border-color: @ACCENT_SOFT; }
QLineEdit:disabled, QSpinBox:disabled, QComboBox:disabled { background: @BG1; color: @MUTED; }
QComboBox::drop-down { border: none; width: 18px; }
QComboBox QAbstractItemView { background: @BG2; border: 1px solid @BORDER; selection-background-color: @ACCENT_SOFT; outline: none; }
QSpinBox::up-button, QSpinBox::down-button, QDoubleSpinBox::up-button, QDoubleSpinBox::down-button { background: @BG3; border: none; width: 14px; }
QPlainTextEdit#terminal { background: @BG0; color: @OK; border: 1px solid @BORDER_SOFT; border-radius: 4px; font-family: monospace; }
QCheckBox, QRadioButton { background: transparent; spacing: 6px; }
QCheckBox::indicator, QRadioButton::indicator { width: 14px; height: 14px; border: 1px solid @BORDER; background: @BG2; }
QCheckBox::indicator { border-radius: 4px; } QRadioButton::indicator { border-radius: 7px; }
QCheckBox::indicator:hover, QRadioButton::indicator:hover { border-color: @ACCENT; }
QCheckBox::indicator:checked, QRadioButton::indicator:checked { background: @OK; border-color: @OK; }
QSlider::groove:horizontal { height: 4px; background: @BG2; border-radius: 2px; margin: 0; }
QSlider::sub-page:horizontal { background: @ACCENT_SOFT; border-radius: 2px; }
QSlider::handle:horizontal { background: @FG_DIM; border: none; width: 12px; height: 12px; margin: -5px 0; border-radius: 6px; }
QSlider::handle:horizontal:hover { background: @ACCENT; }
QSlider::handle:horizontal:disabled { background: @MUTED; }
QScrollArea { border: none; background: transparent; }
QScrollArea > QWidget > QWidget { background: transparent; }
QScrollBar:vertical { background: transparent; width: 9px; margin: 0; }
QScrollBar::handle:vertical { background: @BG4; border-radius: 4px; min-height: 24px; }
QScrollBar::handle:vertical:hover { background: @MUTED; }
QScrollBar:horizontal { background: transparent; height: 9px; margin: 0; }
QScrollBar::handle:horizontal { background: @BG4; border-radius: 4px; min-width: 24px; }
QScrollBar::add-line, QScrollBar::sub-line { height: 0; width: 0; }
QScrollBar::add-page, QScrollBar::sub-page { background: transparent; }
QStatusBar { background: @BG0; color: @MUTED; } QStatusBar::item { border: none; }
QMenu { background: @BG2; border: 1px solid @BORDER; } QMenu::item:selected { background: @ACCENT_SOFT; }
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
