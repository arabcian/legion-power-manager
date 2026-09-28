#include "theme.h"
#include <QApplication>
#include <QHash>
#include <QPalette>
#include <QProxyStyle>
#include <QSettings>
#include <QStyleFactory>

namespace theme {

namespace {
struct Palette {
    const char *id, *name;
    //          BG0  BG1  BG2  BG3  BG4  WELL BORDER BORDER_SOFT FG  FG_DIM MUTED ACCENT ACCENT_SOFT
    //          OK   WARN DANGER DANGER_SOFT INFO PURPLE
    const char *c[19];
};
const Palette PALETTES[] = {
    {"graphite", "Graphite", {"#121316", "#191a1e", "#202126", "#292a30", "#34363d", "#0e0f11", "#2f3137", "#24252a",
        "#e8e6e3", "#b4b2ae", "#86868d", "#eba55b", "#a9773f", "#8fc486", "#e6c065", "#e8776c", "#8e4841", "#7fb1d5", "#b79be2"}},
    {"gruvbox", "Gruvbox", {"#1d2021", "#282828", "#302e2d", "#3c3836", "#504945", "#161819", "#45403d", "#34302e",
        "#ebdbb2", "#d5c4a1", "#928374", "#fe8019", "#b35f1c", "#b8bb26", "#fabd2f", "#fb4934", "#8f2a22", "#83a598", "#d3869b"}},
    {"dracula", "Dracula", {"#1e1f29", "#282a36", "#2f3240", "#383a4a", "#44475a", "#191a21", "#3d4051", "#313341",
        "#f8f8f2", "#c9c9d9", "#7d86ad", "#bd93f9", "#7f62b8", "#50fa7b", "#f1fa8c", "#ff5555", "#9a3b3b", "#8be9fd", "#ff79c6"}},
    {"tokyo-night", "Tokyo Night", {"#16161e", "#1a1b26", "#1f2335", "#292e42", "#343a55", "#121218", "#2f344d", "#232639",
        "#c0caf5", "#a9b1d6", "#737aa2", "#7aa2f7", "#4b6cb7", "#9ece6a", "#e0af68", "#f7768e", "#8c4050", "#7dcfff", "#bb9af7"}},
    {"nord", "Nord", {"#242933", "#2e3440", "#353c4a", "#3b4252", "#434c5e", "#20252e", "#4c566a", "#3a4150",
        "#eceff4", "#d8dee9", "#8a93a6", "#88c0d0", "#5e8a99", "#a3be8c", "#ebcb8b", "#bf616a", "#7d4148", "#81a1c1", "#b48ead"}},
    {"catppuccin", "Catppuccin Mocha", {"#181825", "#1e1e2e", "#262637", "#313244", "#45475a", "#11111b", "#3b3d52", "#2a2b3c",
        "#cdd6f4", "#bac2de", "#7f849c", "#cba6f7", "#8a6fb3", "#a6e3a1", "#f9e2af", "#f38ba8", "#8b4a60", "#89b4fa", "#f5c2e7"}},
    {"horizon", "Horizon", {"#16171d", "#1c1e26", "#232530", "#2e303e", "#393b4d", "#121318", "#3d3f52", "#272936",
        "#fdf0ed", "#d5d8da", "#6c6f93", "#e95678", "#93384b", "#29d398", "#fab795", "#f43e5c", "#8a2536", "#26bbd9", "#ee64ac"}},
    {"oxocarbon", "Oxocarbon", {"#0f0f0f", "#161616", "#1e1e1e", "#262626", "#393939", "#0b0b0b", "#353535", "#222222",
        "#f2f4f8", "#dde1e6", "#8d8d8d", "#42be65", "#2a7a41", "#08bdba", "#f1c21b", "#ee5396", "#8a2f56", "#78a9ff", "#be95ff"}},
    {"monokai-pro", "Monokai Pro", {"#19181a", "#221f22", "#2d2a2e", "#363337", "#403e41", "#151416", "#444145", "#2f2c30",
        "#fcfcfa", "#c1c0c0", "#939293", "#ffd866", "#a88f45", "#a9dc76", "#fc9867", "#ff6188", "#9a3b55", "#78dce8", "#ab9df2"}},
    {"rose-pine", "Rosé Pine", {"#13111c", "#191724", "#1f1d2e", "#26233a", "#312e48", "#100e17", "#403d52", "#21202e",
        "#e0def4", "#908caa", "#6e6a86", "#ebbcba", "#9a7a79", "#9ccfd8", "#f6c177", "#eb6f92", "#8f4259", "#569fba", "#c4a7e7"}},
    {"kanagawa-dragon", "Kanagawa Dragon", {"#0d0c0c", "#181616", "#201d1d", "#282727", "#393836", "#0a0909", "#403e3b", "#242222",
        "#c5c9c5", "#a6a69c", "#737c73", "#b6927b", "#7a6252", "#87a987", "#c4b28a", "#c4746e", "#7a4541", "#8ba4b0", "#a292a3"}},
    {"everforest", "Everforest", {"#232a2e", "#2d353b", "#343f44", "#3d484d", "#475258", "#1e2326", "#4f585e", "#384147",
        "#d3c6aa", "#9da9a0", "#7a8478", "#a7c080", "#6d7e52", "#83c092", "#dbbc7f", "#e67e80", "#8a4b4c", "#7fbbb3", "#d699b6"}},
    {"ayu-mirage", "Ayu Mirage", {"#171b24", "#1f2430", "#242936", "#2a3040", "#343b4d", "#141820", "#3a4152", "#2a303c",
        "#cccac2", "#b3b1ad", "#707a8c", "#ffcc66", "#a88744", "#d5ff80", "#ffad66", "#f28779", "#8f4f47", "#73d0ff", "#dfbfff"}},
    {"synthwave", "Synthwave '84", {"#1b1628", "#241b2f", "#262335", "#2f2a42", "#3b3452", "#17121f", "#463d5e", "#2c2640",
        "#ffffff", "#d6cfe4", "#848bbd", "#ff7edb", "#a34f8c", "#72f1b8", "#fede5d", "#fe4450", "#8f2830", "#36f9f6", "#b893ce"}},
    // Originals: Legion black with its red, and a cold blue-green night.
    {"crimson", "Crimson", {"#111112", "#18181a", "#1f1f22", "#28282c", "#333338", "#0c0c0d", "#2e2e33", "#222226",
        "#eeeeee", "#b8b8bc", "#87878e", "#e5484d", "#9c3236", "#7fc98f", "#f0c05a", "#ff7a59", "#8f4632", "#7aa7d9", "#b39ddb"}},
    {"glacier", "Glacier", {"#0f1519", "#141c21", "#1a242a", "#213038", "#2b3d46", "#0b1013", "#26363e", "#1c282e",
        "#e2eef2", "#a9c0c8", "#6f8a94", "#5ed3c4", "#3a8a80", "#8ed49a", "#e8c86a", "#ef7b7b", "#8a4646", "#7ab8e8", "#b4a2e6"}},
};
QString g_current = QStringLiteral("graphite");
bool g_restart = false;
QSettings settings() {
    return QSettings(QSettings::IniFormat, QSettings::UserScope, QStringLiteral("legion-power-manager"), QStringLiteral("gui"));
}
} // namespace

QList<QPair<QString, QString>> themes() {
    QList<QPair<QString, QString>> out;
    for (const Palette &p : PALETTES) out.append({QString::fromLatin1(p.id), QString::fromLatin1(p.name)});
    return out;
}
QString currentTheme() { return g_current; }

void load() {
    const QString want = settings().value(QStringLiteral("theme")).toString();
    const Palette *p = &PALETTES[0];
    for (const Palette &x : PALETTES) if (want == QLatin1String(x.id)) p = &x;
    g_current = QString::fromLatin1(p->id);
    const char **tok[] = {&BG0, &BG1, &BG2, &BG3, &BG4, &WELL, &BORDER, &BORDER_SOFT, &FG, &FG_DIM, &MUTED,
                          &ACCENT, &ACCENT_SOFT, &OK, &WARN, &DANGER, &DANGER_SOFT, &INFO, &PURPLE};
    for (int i = 0; i < 19; ++i) *tok[i] = p->c[i];
}

bool saveTheme(const QString &id) {
    QSettings s = settings();
    s.setValue(QStringLiteral("theme"), id);
    s.sync();
    return s.status() == QSettings::NoError;
}

void requestRestart() { g_restart = true; QCoreApplication::quit(); }
bool restartRequested() { return g_restart; }

QString profileAccent(const QString &p) {
    static const QHash<QString, QString> m{
        {"low-power", INFO}, {"quiet", INFO}, {"cool", "#7cc6bd"}, {"balanced", FG},
        {"balanced-performance", WARN}, {"performance", DANGER}, {"max-power", "#e070b0"}, {"custom", PURPLE}};
    return m.value(p, MUTED);
}

QString rgba(const char *hex, double alpha) {
    const QColor c(QString::fromLatin1(hex));
    return QStringLiteral("rgba(%1, %2, %3, %4)").arg(c.red()).arg(c.green()).arg(c.blue()).arg(alpha, 0, 'f', 2);
}

QString banner(const char *color, const QString &selector) {
    const QString body = QStringLiteral("background: %1; border: 1px solid %2; border-radius: %3px; color: %4;")
                             .arg(rgba(color, 0.08), rgba(color, 0.34)).arg(RADIUS - 1).arg(FG);
    return selector.isEmpty() ? body : selector + QStringLiteral(" { ") + body + QStringLiteral(" }");
}

static QPalette palette() {
    QPalette p;
    auto c = [](const char *s) { return QColor(QString::fromLatin1(s)); };
    p.setColor(QPalette::Window, c(BG0));        p.setColor(QPalette::WindowText, c(FG));
    p.setColor(QPalette::Base, c(WELL));         p.setColor(QPalette::AlternateBase, c(BG1));
    p.setColor(QPalette::ToolTipBase, c(BG3));   p.setColor(QPalette::ToolTipText, c(FG));
    p.setColor(QPalette::Text, c(FG));           p.setColor(QPalette::Button, c(BG3));
    p.setColor(QPalette::ButtonText, c(FG));     p.setColor(QPalette::BrightText, c(DANGER));
    p.setColor(QPalette::Link, c(INFO));         p.setColor(QPalette::Highlight, c(ACCENT));
    p.setColor(QPalette::HighlightedText, c(BG0)); p.setColor(QPalette::PlaceholderText, c(MUTED));
    p.setColor(QPalette::Mid, c(BORDER));        p.setColor(QPalette::Light, c(BG4));
    p.setColor(QPalette::Dark, c(BG0));          p.setColor(QPalette::Shadow, c(WELL));
    for (auto r : {QPalette::WindowText, QPalette::Text, QPalette::ButtonText})
        p.setColor(QPalette::Disabled, r, c(MUTED));
    p.setColor(QPalette::Disabled, QPalette::Base, c(BG1));
    p.setColor(QPalette::Disabled, QPalette::Button, c(BG1));
    return p;
}

// Selectors (objectName / dynamic properties) are the contract with the tabs:
// btnAccent, btnDanger, btnMini, terminal, miniSlider, box_<colour>,
// role=muted/title, mainTabs. Only the look behind them changes here.
static QString stylesheet() {
    QString s = QStringLiteral(R"QSS(
QWidget { background: @BG0; color: @FG; font-size: @FONTpt; }
/* Plain layout containers (exact class QWidget, not subclasses) take their
   parent's surface instead of punching a window-coloured hole into a card. */
.QWidget { background: transparent; }
QToolTip { background: @BG3; color: @FG; border: 1px solid @BORDER; border-radius: 6px; padding: 5px 8px; }

/* ── main navigation: a header band; the current tab carries an amber rule ── */
QTabWidget#mainTabs { background: @BG1; }
QTabWidget#mainTabs::pane { border: none; border-top: 1px solid @BORDER_SOFT; background: @BG0; top: -1px; }
QTabWidget#mainTabs::tab-bar { left: 36px; }
QTabWidget#mainTabs > QTabBar { background: transparent; }
QTabWidget#mainTabs > QTabBar::tab { background: @BG2; color: @MUTED; border: 1px solid @BORDER_SOFT; border-radius: 7px;
  padding: 6px 8px; margin: 6px 2px; }
QTabWidget#mainTabs > QTabBar::tab:hover:!selected { background: @BG3; color: @FG_DIM; border-color: @BORDER; }
QTabWidget#mainTabs > QTabBar::tab:selected { background: @BG3; color: @FG; border-color: @ACCENT; }
QLabel#appMark { background: transparent; }

/* ── sub-tabs (inside a page): quieter, same grammar ── */
QTabWidget::pane { border: none; border-top: 1px solid @BORDER_SOFT; background: transparent; top: -1px; }
QTabBar { qproperty-drawBase: 0; background: transparent; }
QTabBar::tab { background: transparent; color: @MUTED; padding: 5px 11px 5px 11px; margin-right: 1px;
  border: none; border-bottom: 2px solid transparent; }
QTabBar::tab:hover:!selected { color: @FG_DIM; border-bottom-color: @BORDER; }
QTabBar::tab:selected { color: @FG; font-weight: 600; border-bottom-color: @ACCENT; }
QTabBar::tab:disabled { color: @BG4; }
QTabBar::scroller { width: 22px; }
QTabBar QToolButton { background: @BG1; border: none; border-radius: 0; }

/* ── sections: a card with its title set inside, top left ── */
QGroupBox { background: @BG1; border: 1px solid @BORDER_SOFT; border-radius: @RADpx; margin-top: 0;
  padding: 24px 6px 3px 6px; font-weight: 600; color: @FG; }
QGroupBox::title { subcontrol-origin: border; subcontrol-position: top left; left: 13px; top: 8px;
  padding: 0; color: @FG; background: transparent; }
QGroupBox#box_blue::title { color: @INFO; } QGroupBox#box_purple::title { color: @PURPLE; }
QGroupBox#box_yellow::title { color: @WARN; } QGroupBox#box_green::title { color: @OK; }
QGroupBox#box_grey::title { color: @FG_DIM; }
/* A section inside a section is not another card: a divider and a heading. */
QGroupBox QGroupBox { background: transparent; border: none; border-top: 1px solid @BORDER_SOFT;
  border-radius: 0; padding: 26px 0 0 0; }
QGroupBox QGroupBox::title { left: 2px; top: 9px; color: @FG_DIM; }

QLabel { background: transparent; }
QLabel[role="muted"] { color: @MUTED; font-size: 9pt; }
QLabel[role="title"] { color: @FG; font-size: 14pt; font-weight: 600; }
QFrame[frameShape="4"], QFrame[frameShape="5"] { color: @BORDER_SOFT; }

/* ── buttons: flat, raised one step above the card; amber = primary ── */
QPushButton { background: @BG3; color: @FG; border: 1px solid @BG3; border-radius: 6px;
  padding: 2px 11px; font-weight: 500; min-height: 18px; }
QPushButton:hover { background: @BG4; border-color: @BG4; }
QPushButton:pressed { background: @BG2; border-color: @BORDER; }
QPushButton:focus { border-color: @ACCENT_SOFT; }
QPushButton:disabled { background: transparent; color: @MUTED; border-color: @BORDER_SOFT; }
QPushButton:checked { background: @BG4; border-color: @ACCENT_SOFT; }
QPushButton#btnAccent { background: @ACCENT; border-color: @ACCENT; color: @BG0; font-weight: 600; }
QPushButton#btnAccent:hover { background: @ACCENT_HI; border-color: @ACCENT_HI; }
QPushButton#btnAccent:pressed { background: @ACCENT_SOFT; border-color: @ACCENT_SOFT; }
QPushButton#btnAccent:focus { border-color: @FG; }
QPushButton#btnDanger { background: transparent; border-color: @DANGER_SOFT; color: @DANGER; }
QPushButton#btnDanger:hover { background: @DANGER_TINT; border-color: @DANGER; }
QPushButton#btnDanger:pressed { background: @DANGER_SOFT; color: @FG; }
QPushButton#btnAccent:disabled, QPushButton#btnDanger:disabled { background: transparent; color: @MUTED; border-color: @BORDER_SOFT; }
QPushButton#btnMini { padding: 1px 8px; min-height: 14px; font-size: 9pt; }
QToolButton { background: transparent; border: 1px solid transparent; border-radius: 6px; padding: 2px; }
QToolButton:hover { background: @BG3; }

/* ── inputs: recessed wells; the frame lights up only on focus ── */
QLineEdit, QSpinBox, QDoubleSpinBox, QComboBox { background: @WELL; color: @FG; border: 1px solid @BORDER_SOFT;
  border-radius: 6px; padding: 2px 7px; min-height: 18px; selection-background-color: @ACCENT_SOFT; selection-color: @FG; }
QLineEdit:hover, QSpinBox:hover, QDoubleSpinBox:hover, QComboBox:hover { border-color: @BORDER; }
QLineEdit:focus, QSpinBox:focus, QDoubleSpinBox:focus, QComboBox:focus, QComboBox:on { border-color: @ACCENT_SOFT; }
QLineEdit:disabled, QSpinBox:disabled, QDoubleSpinBox:disabled, QComboBox:disabled { background: transparent; color: @MUTED; border-color: @BORDER_SOFT; }
QLineEdit:read-only { background: @BG1; }
QPlainTextEdit, QTextEdit { background: @WELL; color: @FG; border: 1px solid @BORDER_SOFT; border-radius: 8px;
  padding: 5px 7px; selection-background-color: @ACCENT_SOFT; }
QComboBox { padding-right: 22px; }
QComboBox::drop-down { subcontrol-origin: padding; subcontrol-position: center right; border: none; width: 20px; }
QComboBox::down-arrow { image: url(:/theme/chev-down.png); width: 10px; height: 10px; }
QComboBox::down-arrow:disabled { image: url(:/theme/chev-down-off.png); }
QComboBox QAbstractItemView { background: @BG2; color: @FG; border: 1px solid @BORDER; border-radius: 8px; padding: 4px;
  selection-background-color: @BG4; selection-color: @FG; outline: none; }
QComboBox QAbstractItemView::item { min-height: 22px; padding: 0 6px; border-radius: 5px; }
QSpinBox, QDoubleSpinBox { padding-right: 18px; }
QSpinBox::up-button, QDoubleSpinBox::up-button { subcontrol-origin: border; subcontrol-position: top right;
  width: 17px; border: none; border-top-right-radius: 6px; background: transparent; }
QSpinBox::down-button, QDoubleSpinBox::down-button { subcontrol-origin: border; subcontrol-position: bottom right;
  width: 17px; border: none; border-bottom-right-radius: 6px; background: transparent; }
QSpinBox::up-button:hover, QSpinBox::down-button:hover,
QDoubleSpinBox::up-button:hover, QDoubleSpinBox::down-button:hover { background: @BG3; }
QSpinBox::up-arrow, QDoubleSpinBox::up-arrow { image: url(:/theme/chev-up.png); width: 8px; height: 8px; }
QSpinBox::down-arrow, QDoubleSpinBox::down-arrow { image: url(:/theme/chev-down.png); width: 8px; height: 8px; }
QSpinBox::up-arrow:disabled, QSpinBox::up-arrow:off,
QDoubleSpinBox::up-arrow:disabled, QDoubleSpinBox::up-arrow:off { image: url(:/theme/chev-up-off.png); }
QSpinBox::down-arrow:disabled, QSpinBox::down-arrow:off,
QDoubleSpinBox::down-arrow:disabled, QDoubleSpinBox::down-arrow:off { image: url(:/theme/chev-down-off.png); }
QPlainTextEdit#terminal { background: @WELL; color: @FG_DIM; border: 1px solid @BORDER_SOFT; border-radius: 8px; font-family: monospace; }

/* ── lists and tables ── */
QListView, QTreeView, QTableView { background: @WELL; color: @FG; border: 1px solid @BORDER_SOFT; border-radius: 8px;
  padding: 3px; outline: none; alternate-background-color: @BG1; gridline-color: @BORDER_SOFT; }
QListView::item { padding: 4px 6px; border-radius: 5px; }
QListView::item:hover:!selected { background: @BG2; }
QListView::item:selected { background: @BG4; color: @FG; }
QTableView::item:selected { background: @BG4; color: @FG; }
QHeaderView { background: transparent; border: none; }
QHeaderView::section { background: @BG1; color: @MUTED; border: none; border-bottom: 1px solid @BORDER_SOFT;
  padding: 4px 6px; font-weight: 600; }
QTableCornerButton::section { background: @BG1; border: none; }

/* ── toggles ── */
QCheckBox, QRadioButton { background: transparent; spacing: 7px; }
QCheckBox::indicator, QRadioButton::indicator { width: 14px; height: 14px; border: 1px solid @BORDER; background: @WELL; }
QCheckBox::indicator { border-radius: 4px; } QRadioButton::indicator { border-radius: 8px; }
QCheckBox::indicator:hover, QRadioButton::indicator:hover { border-color: @ACCENT_SOFT; }
QCheckBox::indicator:checked { background: @ACCENT; border-color: @ACCENT; image: url(:/theme/check.png); }
QCheckBox::indicator:indeterminate { background: @ACCENT_SOFT; border-color: @ACCENT_SOFT; }
QRadioButton::indicator:checked { border-color: @ACCENT;
  background: qradialgradient(cx:0.5, cy:0.5, radius:0.5, fx:0.5, fy:0.5, stop:0 @ACCENT, stop:0.42 @ACCENT, stop:0.52 @WELL, stop:1 @WELL); }
QCheckBox::indicator:disabled, QRadioButton::indicator:disabled { background: transparent; border-color: @BORDER_SOFT; }
QCheckBox::indicator:checked:disabled { background: @BG4; border-color: @BG4; }
QRadioButton::indicator:checked:disabled { border-color: @BG4;
  background: qradialgradient(cx:0.5, cy:0.5, radius:0.5, fx:0.5, fy:0.5, stop:0 @BG4, stop:0.42 @BG4, stop:0.52 @BG1, stop:1 @BG1); }
QCheckBox:disabled, QRadioButton:disabled { color: @MUTED; }

/* ── sliders: thin rail, amber fill, a small solid knob ── */
QSlider { background: transparent; }
QSlider::groove:horizontal { height: 4px; background: @BG3; border-radius: 2px; margin: 0; }
QSlider::sub-page:horizontal { background: @ACCENT; border-radius: 2px; }
QSlider::handle:horizontal { background: @FG; border: 3px solid @BG1; width: 10px; height: 10px; margin: -7px 0; border-radius: 8px; }
QSlider::handle:horizontal:hover { background: #ffffff; }
QSlider::handle:horizontal:disabled { background: @MUTED; }
QSlider::sub-page:horizontal:disabled { background: @BG4; }
QSlider#miniSlider::groove:horizontal { height: 3px; border-radius: 1px; }
QSlider#miniSlider::sub-page:horizontal { border-radius: 1px; }
QSlider#miniSlider::handle:horizontal { width: 8px; height: 8px; margin: -6px 0; border-radius: 7px; }

/* ── scrolling ── */
QScrollArea { border: none; background: transparent; }
QScrollArea > QWidget > QWidget { background: transparent; }
QScrollBar:vertical { background: transparent; width: 10px; margin: 2px 1px; }
QScrollBar::handle:vertical { background: @BG4; border-radius: 4px; min-height: 28px; margin: 0 1px; }
QScrollBar::handle:vertical:hover, QScrollBar::handle:horizontal:hover { background: @MUTED; }
QScrollBar:horizontal { background: transparent; height: 10px; margin: 1px 2px; }
QScrollBar::handle:horizontal { background: @BG4; border-radius: 4px; min-width: 28px; margin: 1px 0; }
QScrollBar::add-line, QScrollBar::sub-line { height: 0; width: 0; }
QScrollBar::add-page, QScrollBar::sub-page { background: transparent; }

QStatusBar { background: @BG0; color: @MUTED; font-size: 9pt; } QStatusBar::item { border: none; }
QDialog, QMessageBox { background: @BG0; }

/* ── menus (tray) ── */
QMenu { background: @BG2; color: @FG; border: 1px solid @BORDER; border-radius: 8px; padding: 5px; }
QMenu::item { padding: 5px 24px 5px 12px; border-radius: 5px; background: transparent; }
QMenu::item:selected { background: @BG4; }
QMenu::item:disabled { color: @MUTED; }
QMenu::separator { height: 1px; background: @BORDER_SOFT; margin: 4px 8px; }
QMenu::indicator { width: 12px; height: 12px; left: 5px; }
QMenu::indicator:checked { image: url(:/theme/check.png); background: @ACCENT; border-radius: 3px; }
QMenuBar { background: @BG0; } QMenuBar::item:selected { background: @BG3; }
)QSS");
    // Longest names first so @BG doesn't eat @BG0 etc.
    const std::pair<const char *, QString> toks[] = {
        {"@BORDER_SOFT", BORDER_SOFT}, {"@ACCENT_SOFT", ACCENT_SOFT}, {"@DANGER_SOFT", DANGER_SOFT},
        {"@DANGER_TINT", rgba(DANGER, 0.14)}, {"@ACCENT_HI", QColor(QString::fromLatin1(ACCENT)).lighter(112).name()},
        {"@FG_DIM", FG_DIM}, {"@BORDER", BORDER}, {"@ACCENT", ACCENT}, {"@DANGER", DANGER},
        {"@MUTED", MUTED}, {"@PURPLE", PURPLE}, {"@INFO", INFO}, {"@WARN", WARN}, {"@WELL", WELL}, {"@OK", OK},
        {"@BG0", BG0}, {"@BG1", BG1}, {"@BG2", BG2}, {"@BG3", BG3}, {"@BG4", BG4}, {"@FG", FG},
        {"@FONT", QString::number(FONT_PT)}, {"@RAD", QString::number(RADIUS)}};
    for (const auto &[k, v] : toks) s.replace(QString::fromLatin1(k), v);
    return s;
}

namespace {
/// Fusion with one fix: a layout nested in a grid whose horizontal and vertical
/// spacing differ has no spacing of its own to inherit, and Fusion's fallback
/// is 0 — button pairs such as "Copy… / Delete" or "Fan curve… / Max all fans"
/// were drawn touching. Everything else is Fusion's.
class Style : public QProxyStyle {
public:
    using QProxyStyle::QProxyStyle;
    int pixelMetric(PixelMetric m, const QStyleOption *o = nullptr, const QWidget *w = nullptr) const override {
        if (m == PM_LayoutHorizontalSpacing || m == PM_LayoutVerticalSpacing) return 6;
        return QProxyStyle::pixelMetric(m, o, w);
    }
    // Used per widget pair when the layout has no spacing at all (the nested case).
    int layoutSpacing(QSizePolicy::ControlType, QSizePolicy::ControlType, Qt::Orientation,
                      const QStyleOption * = nullptr, const QWidget * = nullptr) const override {
        return 6;
    }
};
} // namespace

void apply(QApplication &app) {
    // Fusion honours all of the QSS; native styles each ignore a different subset.
    app.setStyle(new Style(QStyleFactory::create(QStringLiteral("Fusion"))));
    app.setPalette(palette());
    app.setStyleSheet(stylesheet());
}

} // namespace theme
