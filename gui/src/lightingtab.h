#pragma once
// Lighting tab — Legion Gen10 Spectrum per-key RGB keyboard, lid logo and
// chassis accent LEDs. Edits the effect list of one controller profile
// (1–6), previews it on a drawing of the keyboard built from the controller's
// own key map, and writes it on Apply. Brightness, logo and the active
// profile apply immediately (tray and scenes use the same calls).
#include "lighting.h"
#include <QSet>
#include <functional>
#include <QWidget>

class QCheckBox;
class QComboBox;
class QGroupBox;
class QHBoxLayout;
class QLabel;
class QListWidget;
class QPushButton;
class QSlider;
class QTimer;
class QToolButton;

class KeyboardView : public QWidget {
    Q_OBJECT
public:
    explicit KeyboardView(QWidget *parent = nullptr);
    void setKeyMap(const lighting::KeyMap &m);
    /// Colour per keycode (missing = dark); `animated` keys get a marker.
    void setPreview(const QHash<int, QColor> &colors, const QSet<int> &animated);
    const QSet<int> &selection() const { return sel_; }
    void setSelection(const QSet<int> &s);
    QSize sizeHint() const override;
    QSize minimumSizeHint() const override;
    bool hasHeightForWidth() const override { return true; }
    int heightForWidth(int w) const override;

Q_SIGNALS:
    void selectionChanged();

protected:
    void paintEvent(QPaintEvent *e) override;
    void resizeEvent(QResizeEvent *e) override;
    void mousePressEvent(QMouseEvent *e) override;
    void mouseMoveEvent(QMouseEvent *e) override;
    void mouseReleaseEvent(QMouseEvent *e) override;

private:
    struct Cell { int code; QRectF rect; bool bar; QString label; };
    void layoutCells();
    int codeAt(const QPointF &p) const;
    void touch(int code);

    lighting::KeyMap map_;
    QList<Cell> cells_;
    QHash<int, QColor> colors_;
    QSet<int> animated_, sel_;
    bool dragging_ = false, dragAdd_ = true;
    int lastCode_ = 0;
};

class LightingTab : public QWidget {
    Q_OBJECT
public:
    explicit LightingTab(QWidget *parent = nullptr);

    bool ready() const { return ready_; }
    bool busy() const { return busy_; }
    int activeProfile() const { return active_; }
    int brightness() const { return brightness_; }
    bool logo() const { return logo_; }
    /// Immediate device changes (tray). No-ops while not ready.
    void activateProfile(int p);
    void setBrightness(int b);
    void setLightsOn(bool on);  // off = brightness 0; on = last non-zero level
    void reload(bool elevate = false);
    bool hasUnappliedChanges() const { return dirty(); }
    /// Something else (scene, tray) changed the device: re-read unless edits are pending.
    void refreshIfClean() { if (ready_ && !busy_ && !dirty()) reload(false); }

Q_SIGNALS:
    void stateChanged();

protected:
    void showEvent(QShowEvent *e) override;

private:
    void buildUi();
    void setBusy(bool b);
    void showStatus(const QString &msg, const char *color = nullptr);
    void showBanner(const QString &msg, bool offerElevate);
    void applyState(const QJsonObject &j);
    void sendSet(const QJsonObject &fields, const QString &what);
    void write();
    void factoryReset();
    bool confirmDiscard();
    bool dirty() const { return work_ != loaded_; }

    // editing
    void refreshAll();          // list + editor + preview + budget
    void refreshList();
    void refreshEditor();
    void refreshPreview();
    void refreshBudget();
    void moveKeys(const QSet<int> &keys, int into);  // make `keys` exclusive to effect `into` (-1: drop them)
    void paintSelection(const QColor &c);
    void addEffect();
    void removeEffect();
    void shiftEffect(int delta);
    void editCurrent(const std::function<void(lighting::Effect &)> &f);
    void rebuildColorChips();
    int current() const;
    QSet<int> zoneKeys(lighting::Zone z) const;

    lighting::KeyMap map_;
    QList<lighting::Effect> loaded_, work_;
    int active_ = 0, editProfile_ = 0, brightness_ = 0, lastOn_ = 5;
    bool logo_ = false, ready_ = false, busy_ = false, filling_ = false;
    QColor paint_ = QColor(0xe0, 0xa4, 0x58);

    QLabel *banner_, *status_, *brightLabel_, *selLabel_, *budget_, *keysLabel_;
    QPushButton *elevate_, *apply_, *revert_, *reset_, *reloadBtn_;
    QComboBox *profile_;
    QSlider *bright_;
    QCheckBox *logoCheck_;
    KeyboardView *kb_;
    QToolButton *paintColor_;
    QListWidget *list_;
    QGroupBox *editor_;
    QComboBox *type_, *speed_, *dir_, *cw_;
    QCheckBox *random_;
    QHBoxLayout *chips_;
    QWidget *speedRow_, *dirRow_, *cwRow_, *colorRow_;
    QTimer *brightDebounce_, *statusTimer_;
};
