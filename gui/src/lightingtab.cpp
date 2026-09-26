#include "lightingtab.h"
#include "theme.h"
#include <QCheckBox>
#include <QColorDialog>
#include <QComboBox>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QLabel>
#include <QListWidget>
#include <QMessageBox>
#include <QMouseEvent>
#include <QPainter>
#include <QPushButton>
#include <QSlider>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>
#include <cmath>

using namespace lighting;

static constexpr double KEY_ASPECT = 0.86;  // key height / column width
static constexpr double LOGO_ROWS = 1.4;    // extra space under the grid for the logo
static constexpr int BRIGHT_DEBOUNCE_MS = 300, MAX_COLORS = 8, MAX_EFFECTS = 64;

static QLabel *muted(const QString &t) {
    auto *l = new QLabel(t);
    l->setProperty("role", "muted");
    return l;
}
static QPushButton *mini(const QString &t, const QString &tip = {}) {
    auto *b = new QPushButton(t);
    b->setObjectName("btnMini");
    if (!tip.isEmpty()) b->setToolTip(tip);
    return b;
}
static QColor readable(const QColor &bg) {
    return bg.lightnessF() > 0.55 ? QColor(QString::fromLatin1(theme::BG0)) : QColor(QString::fromLatin1(theme::FG));
}
static QIcon swatch(const Effect &e, int size = 14) {
    QPixmap pm(size, size);
    pm.fill(Qt::transparent);
    QPainter p(&pm);
    p.setRenderHint(QPainter::Antialiasing);
    QLinearGradient g(0, 0, size, 0);
    if (e.colorMode == ColorList && !e.colors.isEmpty()) {
        for (int i = 0; i < e.colors.size(); ++i) g.setColorAt(e.colors.size() == 1 ? 0 : double(i) / (e.colors.size() - 1), e.colors[i]);
        if (e.colors.size() == 1) g.setColorAt(1, e.colors[0]);
    } else {
        for (int i = 0; i <= 6; ++i) g.setColorAt(i / 6.0, QColor::fromHsv(i * 60 % 360, 200, 240));
    }
    p.setBrush(g);
    p.setPen(Qt::NoPen);
    p.drawRoundedRect(QRectF(1, 1, size - 2, size - 2), 3, 3);
    return QIcon(pm);
}

// ── keyboard drawing ────────────────────────────────────────────────────────

KeyboardView::KeyboardView(QWidget *parent) : QWidget(parent) {
    setMouseTracking(false);
    QSizePolicy sp(QSizePolicy::Expanding, QSizePolicy::Preferred);
    sp.setHeightForWidth(true);
    setSizePolicy(sp);
    setToolTip("Click a key to select it · drag to select several · Ctrl/Shift-click to add or remove");
}

void KeyboardView::setKeyMap(const KeyMap &m) {
    map_ = m;
    phys_ = kblayout::forMap(m);
    legends_ = kblayout::legends(phys_);
    setToolTip(QStringLiteral("Click a key to select it · drag to select several · Ctrl/Shift-click to add or remove")
               + (phys_.isEmpty() ? QStringLiteral("\nLayout: controller matrix (unknown board)")
                                  : QStringLiteral("\nLayout: %1 · legends: %2")
                                        .arg(kblayout::formName(kblayout::formOf(m)), kblayout::activeLayoutName())));
    sel_.clear();
    layoutCells();
    updateGeometry();
    update();
}

void KeyboardView::setPreview(const QHash<int, QColor> &c, const QSet<int> &a) {
    colors_ = c;
    animated_ = a;
    update();
}

void KeyboardView::setSelection(const QSet<int> &s) {
    if (s == sel_) return;
    sel_ = s;
    update();
    Q_EMIT selectionChanged();
}

QSize KeyboardView::sizeHint() const { return {640, heightForWidth(640)}; }
QSize KeyboardView::minimumSizeHint() const { return {360, heightForWidth(360)}; }

int KeyboardView::heightForWidth(int w) const {
    if (map_.isEmpty()) return 160;
    const bool logo = map_.extra.count(0) < map_.extra.size();
    if (!phys_.isEmpty()) {
        const QRectF b = physBounds();
        const double u = double(w) / b.width();
        return int(std::ceil(u * KEY_ASPECT * (b.height() + (logo ? LOGO_ROWS : 0)))) + 2;
    }
    const double u = double(w) / map_.cols;
    return int(std::ceil(u * KEY_ASPECT * (map_.rows + (logo ? LOGO_ROWS : 0)))) + 2;
}

QRectF KeyboardView::physBounds() const {
    QRectF b;
    for (const kblayout::Key &k : phys_) b = b.isNull() ? k.rect : b.united(k.rect);
    return b.adjusted(-0.1, -0.1, 0.1, 0.1);
}

void KeyboardView::resizeEvent(QResizeEvent *e) {
    QWidget::resizeEvent(e);
    layoutCells();
}

void KeyboardView::layoutCells() {
    cells_.clear();
    if (map_.isEmpty()) return;
    if (!phys_.isEmpty()) layoutPhysical();
    else layoutMatrix();
}

void KeyboardView::layoutPhysical() {
    const bool logo = map_.extra.count(0) < map_.extra.size();
    const QRectF b = physBounds();
    const double rowsTotal = b.height() + (logo ? LOGO_ROWS : 0);
    const double u = std::min(double(width()) / b.width(), double(height()) / (rowsTotal * KEY_ASPECT));
    const double rh = u * KEY_ASPECT, gap = std::max(1.5, u * 0.06);
    const double x0 = (width() - u * b.width()) / 2.0, y0 = 1;
    auto px = [&](const QRectF &r) {
        return QRectF(x0 + (r.x() - b.x()) * u, y0 + (r.y() - b.y()) * rh, r.width() * u, r.height() * rh);
    };
    // One cell per code; a code with several rects (ISO Enter) becomes one outline.
    QHash<int, int> index;
    for (const kblayout::Key &k : phys_) {
        QRectF r = px(k.rect);
        if (!k.bar) r.adjust(gap, gap, -gap, -gap);
        if (const auto it = index.constFind(k.code); it != index.cend()) {
            Cell &c = cells_[*it];
            const double rad = std::min(5.0, r.height() * 0.18);
            QPainterPath part;
            // extend the part up into the first rect so the union has no seam
            part.addRoundedRect(r.adjusted(0, -2 * gap - 2 * rad, 0, 0), rad, rad);
            if (c.shape.isEmpty()) c.shape.addRoundedRect(c.rect, rad, rad);
            c.shape = c.shape.united(part).simplified();
            continue;
        }
        index.insert(k.code, int(cells_.size()));
        cells_.append({k.code, r, k.bar, k.bar ? QString() : legends_.value(k.code, k.label), {}});
    }
    if (logo) {
        const QList<int> codes = [&] { QList<int> v; for (int k : map_.extra) if (k) v << k; return v; }();
        const double w = u * 4, top = y0 + b.height() * rh + rh * 0.35;
        double x = x0 + (u * b.width() - codes.size() * (w + u)) / 2.0 + u / 2;
        for (int k : codes) {
            cells_.append({k, QRectF(x, top, w, rh * 0.8), false, QStringLiteral("LOGO"), {}});
            x += w + u;
        }
    }
}

void KeyboardView::layoutMatrix() {
    const bool logo = map_.extra.count(0) < map_.extra.size();
    const double rowsTotal = map_.rows + (logo ? LOGO_ROWS : 0);
    const double u = std::min(double(width()) / map_.cols, double(height()) / (rowsTotal * KEY_ASPECT));
    const double h = u * KEY_ASPECT, gap = std::max(1.5, u * 0.06);
    const double x0 = (width() - u * map_.cols) / 2.0, y0 = 1;
    for (const KeyMap::Span &s : map_.spans()) {
        QRectF r(x0 + s.col * u + gap, y0 + s.row * h + gap, s.width * u - 2 * gap, h - 2 * gap);
        const bool bar = isPerimeter(s.code);
        if (bar) {
            const bool horizontal = s.row == 0 || s.row == map_.rows - 1 || s.width > 1;
            r = horizontal ? QRectF(r.left(), r.center().y() - h * 0.14, r.width(), h * 0.28)
                           : QRectF(r.center().x() - u * 0.14, r.top(), u * 0.28, r.height());
        }
        cells_.append({s.code, r, bar, bar ? QString() : keyLabel(s.code), {}});
    }
    if (logo) {
        const QList<int> codes = [&] { QList<int> v; for (int k : map_.extra) if (k) v << k; return v; }();
        const double w = u * 4, top = y0 + map_.rows * h + h * 0.35;
        double x = x0 + (u * map_.cols - codes.size() * (w + u)) / 2.0 + u / 2;
        for (int k : codes) {
            cells_.append({k, QRectF(x, top, w, h * 0.8), false, QStringLiteral("LOGO"), {}});
            x += w + u;
        }
    }
}

void KeyboardView::paintEvent(QPaintEvent *) {
    QPainter p(this);
    p.setRenderHint(QPainter::Antialiasing);
    if (cells_.isEmpty()) {
        p.setPen(QColor(QString::fromLatin1(theme::MUTED)));
        p.drawText(rect(), Qt::AlignCenter, "No key map yet");
        return;
    }
    const QColor off(QString::fromLatin1(theme::BG3)), border(QString::fromLatin1(theme::BORDER)),
        accent(QString::fromLatin1(theme::ACCENT));
    QFont f = font();
    double keyH = 20;  // a regular 1u key sets the legend size (the F-row and arrows are shorter)
    for (const Cell &c : cells_) if (!c.bar && c.code == 0x42) { keyH = c.rect.height(); break; }
    f.setPixelSize(std::clamp(int(keyH * 0.34), 7, 13));
    p.setFont(f);
    for (const Cell &c : cells_) {
        const auto it = colors_.constFind(c.code);
        const bool lit = it != colors_.cend();
        const QColor fill = lit ? *it : off;
        const bool selected = sel_.contains(c.code);
        p.setPen(QPen(border, 1));
        p.setBrush(fill);
        const double rad = c.bar ? 2 : std::min(5.0, c.rect.height() * 0.18);
        if (!c.shape.isEmpty()) p.drawPath(c.shape);
        else p.drawRoundedRect(c.rect, rad, rad);
        if (selected) {  // two-tone ring: readable on any key colour, amber included
            p.setBrush(Qt::NoBrush);
            const QPen outer(QColor(QString::fromLatin1(theme::BG0)), 3),
                inner(fill.lightnessF() > 0.6 ? QColor(QString::fromLatin1(theme::FG)) : accent, 1.6);
            for (const QPen &pen : {outer, inner}) {
                p.setPen(pen);
                if (!c.shape.isEmpty()) {
                    p.drawPath(c.shape);
                } else {
                    p.drawRoundedRect(c.rect.adjusted(-1.5, -1.5, 1.5, 1.5), rad + 1, rad + 1);
                }
            }
        }
        if (!c.label.isEmpty()) {
            p.setPen(lit ? readable(fill) : QColor(QString::fromLatin1(theme::MUTED)));
            p.drawText(c.rect, Qt::AlignCenter, c.label);
        }
        if (animated_.contains(c.code) && !c.bar) {  // animated effect: dotted line along the bottom edge
            QPen pen(readable(fill), 1.2, Qt::DotLine);
            p.setPen(pen);
            const double y = c.rect.bottom() - 3;
            p.drawLine(QPointF(c.rect.left() + 4, y), QPointF(c.rect.right() - 4, y));
        }
    }
}

int KeyboardView::codeAt(const QPointF &pt) const {
    for (const Cell &c : cells_)
        if (c.shape.isEmpty() ? c.rect.adjusted(-2, -3, 2, 3).contains(pt) : c.shape.contains(pt)) return c.code;
    return 0;
}

void KeyboardView::touch(int code) {
    if (!code || code == lastCode_) return;
    lastCode_ = code;
    if (dragAdd_) sel_.insert(code); else sel_.remove(code);
    update();
    Q_EMIT selectionChanged();
}

void KeyboardView::mousePressEvent(QMouseEvent *e) {
    if (e->button() != Qt::LeftButton) return;
    const int code = codeAt(e->position());
    const bool additive = e->modifiers() & (Qt::ControlModifier | Qt::ShiftModifier);
    if (!additive) sel_.clear();
    dragAdd_ = !(additive && sel_.contains(code));
    dragging_ = true;
    lastCode_ = 0;
    if (code) touch(code);
    else { update(); Q_EMIT selectionChanged(); }
}

void KeyboardView::mouseMoveEvent(QMouseEvent *e) {
    if (dragging_) touch(codeAt(e->position()));
}

void KeyboardView::mouseReleaseEvent(QMouseEvent *) { dragging_ = false; }

// ── tab ─────────────────────────────────────────────────────────────────────

LightingTab::LightingTab(QWidget *parent) : QWidget(parent) {
    buildUi();
    brightDebounce_ = new QTimer(this);
    brightDebounce_->setSingleShot(true);
    brightDebounce_->setInterval(BRIGHT_DEBOUNCE_MS);
    connect(brightDebounce_, &QTimer::timeout, this, [this] { setBrightness(bright_->value()); });
    statusTimer_ = new QTimer(this);
    statusTimer_->setSingleShot(true);
    connect(statusTimer_, &QTimer::timeout, status_, &QLabel::clear);
    // Startup read for the tray: as the user only — never a password prompt at login.
    QTimer::singleShot(0, this, [this] { reload(false); });
}

void LightingTab::buildUi() {
    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(10, 8, 10, 8);
    root->setSpacing(6);

    banner_ = new QLabel;
    banner_->setWordWrap(true);
    banner_->setStyleSheet(QStringLiteral("color: %1;").arg(theme::WARN));
    elevate_ = mini("Use administrator rights", "Read and write the keyboard through pkexec this time");
    auto *bannerRow = new QHBoxLayout;
    bannerRow->addWidget(banner_, 1);
    bannerRow->addWidget(elevate_);
    root->addLayout(bannerRow);
    banner_->hide();
    elevate_->hide();

    // ── device: profile / brightness / logo ──
    auto *dev = new QGroupBox("Keyboard");
    auto *dl = new QHBoxLayout(dev);
    dl->setSpacing(8);
    profile_ = new QComboBox;
    for (int p = MIN_PROFILE; p <= MAX_PROFILE; ++p) profile_->addItem(QStringLiteral("Profile %1").arg(p), p);
    profile_->setToolTip("The controller keeps six lighting profiles (shared with Windows / Legion Space). "
                         "Choosing one activates it and loads it into the editor.");
    bright_ = new QSlider(Qt::Horizontal);
    bright_->setRange(0, MAX_BRIGHTNESS);
    bright_->setPageStep(1);
    bright_->setFixedWidth(150);
    bright_->setToolTip("Backlight brightness (0 = off). Applies immediately.");
    brightLabel_ = new QLabel("–");
    brightLabel_->setMinimumWidth(22);
    logoCheck_ = new QCheckBox("Lid logo");
    logoCheck_->setToolTip("Turns the LEGION logo on the lid on or off (its colour is set like a key below).");
    reloadBtn_ = mini("Reload", "Read everything back from the keyboard");
    dl->addWidget(muted("Profile"));
    dl->addWidget(profile_);
    dl->addSpacing(12);
    dl->addWidget(muted("Brightness"));
    dl->addWidget(bright_);
    dl->addWidget(brightLabel_);
    dl->addSpacing(12);
    dl->addWidget(logoCheck_);
    dl->addStretch(1);
    dl->addWidget(reloadBtn_);
    root->addWidget(dev);

    // ── keyboard + effects ──
    auto *mid = new QHBoxLayout;
    mid->setSpacing(8);

    auto *kbBox = new QGroupBox("Layout");
    auto *kl = new QVBoxLayout(kbBox);
    kb_ = new KeyboardView;
    kl->addWidget(kb_);
    auto *selRow = new QHBoxLayout;
    selRow->setSpacing(4);
    selRow->addWidget(muted("Select"));
    auto *selKb = mini("Keys"), *selEdge = mini("Edge", "Rear and front/side accent lights"),
         *selLogo = mini("Logo"), *selAll = mini("All"), *selNone = mini("None");
    for (QPushButton *b : {selKb, selEdge, selLogo, selAll, selNone}) selRow->addWidget(b);
    selLabel_ = muted(QString());
    selRow->addSpacing(6);
    selRow->addWidget(selLabel_);
    selRow->addStretch(1);
    kl->addLayout(selRow);
    auto *paintRow = new QHBoxLayout;
    paintRow->setSpacing(4);
    paintColor_ = new QToolButton;
    paintColor_->setToolTip("Paint colour");
    paintColor_->setFixedSize(26, 22);
    auto *paintBtn = mini("Paint selection", "Selected keys become a static colour (taken out of any other effect)");
    auto *offBtn = mini("Turn off selection", "Selected keys are removed from every effect and stay dark");
    paintRow->addWidget(paintColor_);
    paintRow->addWidget(paintBtn);
    paintRow->addWidget(offBtn);
    paintRow->addStretch(1);
    kl->addLayout(paintRow);
    kl->addStretch(1);
    mid->addWidget(kbBox, 1);

    auto *fxBox = new QGroupBox("Effects in this profile");
    fxBox->setFixedWidth(316);
    auto *fl = new QVBoxLayout(fxBox);
    fl->setSpacing(4);
    list_ = new QListWidget;
    list_->setMinimumHeight(90);
    fl->addWidget(list_, 1);
    auto *listBtns = new QHBoxLayout;
    listBtns->setSpacing(4);
    auto *addBtn = mini("Add", "New effect on the selected keys (they leave their current effect)");
    auto *delBtn = mini("Remove");
    auto *upBtn = mini("▲"), *downBtn = mini("▼");
    for (QPushButton *b : {addBtn, delBtn, upBtn, downBtn}) listBtns->addWidget(b);
    listBtns->addStretch(1);
    fl->addLayout(listBtns);

    editor_ = new QGroupBox("Effect");
    auto *eg = new QGridLayout(editor_);
    eg->setVerticalSpacing(4);
    eg->setColumnStretch(1, 1);
    int row = 0;
    auto addRow = [&](const QString &label, QWidget *w, QWidget **host) {
        auto *h = new QWidget;
        auto *hl = new QHBoxLayout(h);
        hl->setContentsMargins(0, 0, 0, 0);
        hl->addWidget(muted(label));
        hl->addWidget(w, 1);
        eg->addWidget(h, row++, 0, 1, 2);
        if (host) *host = h;
    };
    type_ = new QComboBox;
    for (int t : editableTypes()) type_->addItem(typeName(t), t);
    addRow("Type", type_, nullptr);
    speed_ = new QComboBox;
    speed_->addItem("Slow", 1);
    speed_->addItem("Medium", 2);
    speed_->addItem("Fast", 3);
    addRow("Speed", speed_, &speedRow_);
    dir_ = new QComboBox;
    dir_->addItem("Left → right", 4);
    dir_->addItem("Right → left", 3);
    dir_->addItem("Bottom → top", 1);
    dir_->addItem("Top → bottom", 2);
    addRow("Direction", dir_, &dirRow_);
    cw_ = new QComboBox;
    cw_->addItem("Clockwise", 1);
    cw_->addItem("Counter-clockwise", 2);
    addRow("Rotation", cw_, &cwRow_);
    colorRow_ = new QWidget;
    auto *cl = new QVBoxLayout(colorRow_);
    cl->setContentsMargins(0, 0, 0, 0);
    cl->setSpacing(3);
    random_ = new QCheckBox("Random colours");
    cl->addWidget(random_);
    auto *chipHost = new QWidget;
    chips_ = new QHBoxLayout(chipHost);
    chips_->setContentsMargins(0, 0, 0, 0);
    chips_->setSpacing(3);
    cl->addWidget(chipHost);
    eg->addWidget(colorRow_, row++, 0, 1, 2);
    keysLabel_ = muted(QString());
    eg->addWidget(keysLabel_, row++, 0, 1, 2);
    auto *keysRow = new QHBoxLayout;
    keysRow->setSpacing(4);
    auto *assign = mini("Use selection", "This effect takes the selected keys (they leave any other effect)");
    auto *show = mini("Select them", "Select the lights this effect uses");
    keysRow->addWidget(assign, 1);
    keysRow->addWidget(show, 1);
    eg->addLayout(keysRow, row++, 0, 1, 2);
    fl->addWidget(editor_);
    mid->addWidget(fxBox);
    root->addLayout(mid, 1);

    // ── bottom bar ──
    auto *bottom = new QHBoxLayout;
    budget_ = muted(QString());
    budget_->setToolTip("The whole profile has to fit one 960-byte report to the controller. "
                        "Each colour group costs ~19 bytes, each key 2 — roughly 36 distinct colours "
                        "when every light is used.");
    reset_ = new QPushButton("Factory reset profile");
    reset_->setObjectName("btnDanger");
    revert_ = new QPushButton("Revert");
    apply_ = new QPushButton("Apply to profile");
    apply_->setObjectName("btnAccent");
    apply_->setToolTip("Writes the effect list into the keyboard controller. It is stored there "
                       "(survives reboots and Windows), so apply when done rather than after every click.");
    bottom->addWidget(budget_);
    bottom->addStretch(1);
    bottom->addWidget(reset_);
    bottom->addWidget(revert_);
    bottom->addWidget(apply_);
    root->addLayout(bottom);
    status_ = new QLabel;
    status_->setWordWrap(true);
    root->addWidget(status_);

    // ── wiring ──
    connect(elevate_, &QPushButton::clicked, this, [this] { reload(true); });
    connect(reloadBtn_, &QPushButton::clicked, this, [this] {
        if (confirmDiscard()) reload(false);
    });
    connect(profile_, &QComboBox::activated, this, [this] {
        const int p = profile_->currentData().toInt();
        if (p == editProfile_) return;
        if (!confirmDiscard()) { filling_ = true; profile_->setCurrentIndex(profile_->findData(editProfile_)); filling_ = false; return; }
        activateProfile(p);
    });
    connect(bright_, &QSlider::valueChanged, this, [this](int v) {
        brightLabel_->setText(QString::number(v));
        if (!filling_) brightDebounce_->start();
    });
    connect(logoCheck_, &QCheckBox::toggled, this, [this](bool on) {
        if (!filling_) sendSet({{"logo", on}}, on ? "Logo on" : "Logo off");
    });
    connect(kb_, &KeyboardView::selectionChanged, this, [this] {
        selLabel_->setText(kb_->selection().isEmpty() ? QString() : QStringLiteral("%1 selected").arg(kb_->selection().size()));
    });
    connect(selKb, &QPushButton::clicked, this, [this] { kb_->setSelection(zoneKeys(Zone::Keyboard)); });
    connect(selEdge, &QPushButton::clicked, this, [this] { kb_->setSelection(zoneKeys(Zone::Perimeter)); });
    connect(selLogo, &QPushButton::clicked, this, [this] { kb_->setSelection(zoneKeys(Zone::Logo)); });
    connect(selAll, &QPushButton::clicked, this, [this] {
        const QList<int> all = map_.unique();
        kb_->setSelection(QSet<int>(all.cbegin(), all.cend()));
    });
    connect(selNone, &QPushButton::clicked, this, [this] { kb_->setSelection({}); });
    auto updatePaintIcon = [this] {
        QPixmap pm(18, 12);
        pm.fill(paint_);
        paintColor_->setIcon(QIcon(pm));
    };
    updatePaintIcon();
    connect(paintColor_, &QToolButton::clicked, this, [this, updatePaintIcon] {
        const QColor c = QColorDialog::getColor(paint_, this, "Paint colour");
        if (c.isValid()) { paint_ = c; updatePaintIcon(); }
    });
    connect(paintBtn, &QPushButton::clicked, this, [this] { paintSelection(paint_); });
    connect(offBtn, &QPushButton::clicked, this, [this] {
        if (kb_->selection().isEmpty()) { showStatus("Select some keys first."); return; }
        moveKeys(kb_->selection(), -1);
        refreshAll();
    });
    connect(list_, &QListWidget::currentRowChanged, this, [this] { if (!filling_) refreshEditor(); });
    connect(addBtn, &QPushButton::clicked, this, &LightingTab::addEffect);
    connect(delBtn, &QPushButton::clicked, this, &LightingTab::removeEffect);
    connect(upBtn, &QPushButton::clicked, this, [this] { shiftEffect(-1); });
    connect(downBtn, &QPushButton::clicked, this, [this] { shiftEffect(1); });
    connect(type_, &QComboBox::activated, this, [this] {
        const int t = type_->currentData().toInt();
        editCurrent([&](Effect &e) {
            e.type = t;
            e.speed = hasSpeed(t) ? (e.speed ? e.speed : 2) : 0;
            e.direction = hasDirection(t) ? (e.direction ? e.direction : 4) : 0;
            e.clockwise = hasClockwise(t) ? (e.clockwise ? e.clockwise : 1) : 0;
            if (!hasColorMode(t)) { e.colorMode = NoColors; e.colors.clear(); return; }
            if (e.colorMode == NoColors) e.colorMode = ColorList;
            if (e.colorMode == ColorList && e.colors.isEmpty()) e.colors = {paint_};
            if (!multiColor(t) && e.colors.size() > 1) e.colors = {e.colors.first()};
        });
    });
    connect(speed_, &QComboBox::activated, this, [this] { editCurrent([&](Effect &e) { e.speed = speed_->currentData().toInt(); }); });
    connect(dir_, &QComboBox::activated, this, [this] { editCurrent([&](Effect &e) { e.direction = dir_->currentData().toInt(); }); });
    connect(cw_, &QComboBox::activated, this, [this] { editCurrent([&](Effect &e) { e.clockwise = cw_->currentData().toInt(); }); });
    connect(random_, &QCheckBox::toggled, this, [this](bool on) {
        if (filling_) return;
        editCurrent([&](Effect &e) {
            e.colorMode = on ? RandomColors : ColorList;
            if (on) e.colors.clear();
            else if (e.colors.isEmpty()) e.colors = {paint_};
        });
    });
    connect(assign, &QPushButton::clicked, this, [this] {
        const int i = current();
        if (i < 0) return;
        if (kb_->selection().isEmpty()) { showStatus("Select some keys first."); return; }
        moveKeys(kb_->selection(), i);
        refreshAll();
    });
    connect(show, &QPushButton::clicked, this, [this] {
        const int i = current();
        if (i < 0) return;
        const QList<int> k = work_[i].allKeys() ? map_.unique() : work_[i].keys;
        kb_->setSelection(QSet<int>(k.cbegin(), k.cend()));
    });
    connect(revert_, &QPushButton::clicked, this, [this] { work_ = loaded_; refreshAll(); });
    connect(apply_, &QPushButton::clicked, this, &LightingTab::write);
    connect(reset_, &QPushButton::clicked, this, &LightingTab::factoryReset);

    setBusy(true);  // until the first read
    refreshAll();
}

void LightingTab::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    // Windows / Fn+Space may have changed profile or brightness meanwhile.
    refreshIfClean();
}

QSet<int> LightingTab::zoneKeys(Zone z) const {
    const QList<int> k = map_.zone(z);
    return QSet<int>(k.cbegin(), k.cend());
}

int LightingTab::current() const {
    const int i = list_->currentRow();
    return i >= 0 && i < work_.size() ? i : -1;
}

void LightingTab::setBusy(bool b) {
    busy_ = b;
    const bool en = ready_ && !b;
    for (QWidget *w : std::initializer_list<QWidget *>{profile_, bright_, logoCheck_, reloadBtn_, reset_})
        w->setEnabled(en);
    reloadBtn_->setEnabled(!b);
    refreshBudget();
}

void LightingTab::showStatus(const QString &msg, const char *color) {
    status_->setStyleSheet(color ? QStringLiteral("color: %1;").arg(color) : QString());
    status_->setText(msg);
    statusTimer_->start(color == theme::DANGER ? 15000 : 5000);
}

void LightingTab::showBanner(const QString &msg, bool offerElevate) {
    banner_->setText(msg);
    banner_->setVisible(!msg.isEmpty());
    elevate_->setVisible(offerElevate);
}

bool LightingTab::confirmDiscard() {
    if (!dirty()) return true;
    return QMessageBox::question(this, "Lighting", "Discard the changes not applied to the keyboard yet?",
                                 QMessageBox::Discard | QMessageBox::Cancel) == QMessageBox::Discard;
}

void LightingTab::reload(bool elevate) {
    if (busy_ && ready_) return;
    setBusy(true);
    QJsonObject req{{"op", "state"}};
    if (editProfile_ >= MIN_PROFILE && editProfile_ != active_) req["profile"] = editProfile_;
    run(req, this, [this](const privileged::Result &r) {
        if (r.reached && r.json.value("ok").toBool()) {
            showBanner({}, false);
            applyState(r.json);
        } else if (r.reached && r.json.value("denied").toBool()) {
            showBanner("No access to the keyboard controller yet: the udev rule applies after the next login "
                       "(or replug). Until then lighting goes through administrator rights.", true);
        } else if (r.reached && r.json.contains("present") && !r.json.value("present").toBool()) {
            showBanner("The Spectrum keyboard controller is not responding.", false);
        } else {
            showBanner("Could not read the keyboard: " + r.message(), true);
        }
        setBusy(false);
        Q_EMIT stateChanged();
    }, elevate);
}

void LightingTab::applyState(const QJsonObject &j) {
    const KeyMap m = KeyMap::fromJson(j.value("keymap").toObject());
    if (!m.isEmpty() && (m.grid != map_.grid || m.extra != map_.extra)) {
        map_ = m;
        kb_->setKeyMap(map_);
    }
    active_ = j.value("active_profile").toInt(active_);
    editProfile_ = j.value("profile").toInt(active_);
    brightness_ = j.value("brightness").toInt();
    if (brightness_ > 0) lastOn_ = brightness_;
    logo_ = j.value("logo").toBool();
    loaded_.clear();
    for (const QJsonValue &v : j.value("effects").toArray()) loaded_ << fromJson(v.toObject());
    work_ = loaded_;
    ready_ = true;

    filling_ = true;
    if (profile_->findData(editProfile_) < 0)  // profile 0 exists on some firmware
        profile_->insertItem(0, QStringLiteral("Profile %1").arg(editProfile_), editProfile_);
    profile_->setCurrentIndex(profile_->findData(editProfile_));
    bright_->setValue(brightness_);
    brightLabel_->setText(QString::number(brightness_));
    logoCheck_->setChecked(logo_);
    filling_ = false;
    refreshAll();
}

void LightingTab::sendSet(const QJsonObject &fields, const QString &what) {
    if (!ready_) return;
    QJsonObject req = fields;
    req["op"] = "set";
    run(req, this, [this, what](const privileged::Result &r) {
        if (!r.ok()) { showStatus(what + " failed: " + r.message(), theme::DANGER); reload(false); return; }
        active_ = r.json.value("profile").toInt(active_);
        brightness_ = r.json.value("brightness").toInt(brightness_);
        if (brightness_ > 0) lastOn_ = brightness_;
        logo_ = r.json.value("logo").toBool(logo_);
        filling_ = true;
        bright_->setValue(brightness_);
        logoCheck_->setChecked(logo_);
        filling_ = false;
        showStatus(what + ".", theme::OK);
        Q_EMIT stateChanged();
    });
}

void LightingTab::activateProfile(int p) {
    if (!ready_ || p < 0 || p > MAX_PROFILE) return;
    setBusy(true);
    run({{"op", "set"}, {"profile", p}}, this, [this, p](const privileged::Result &r) {
        setBusy(false);
        if (!r.ok()) { showStatus("Switching profile failed: " + r.message(), theme::DANGER); return; }
        active_ = editProfile_ = p;
        showStatus(QStringLiteral("Profile %1 active.").arg(p), theme::OK);
        reload(false);  // loads that profile's effects (and emits stateChanged)
    });
}

void LightingTab::setBrightness(int b) {
    sendSet({{"brightness", std::clamp(b, 0, MAX_BRIGHTNESS)}}, b ? QStringLiteral("Brightness %1").arg(b) : QStringLiteral("Lights off"));
}

void LightingTab::setLightsOn(bool on) { setBrightness(on ? std::max(1, lastOn_) : 0); }

void LightingTab::write() {
    if (!ready_ || busy_ || !dirty()) return;
    const int p = editProfile_;
    QJsonArray fx;
    for (const Effect &e : work_) fx.append(toJson(e));
    setBusy(true);
    const QList<Effect> sent = work_;
    run({{"op", "write"}, {"profile", p}, {"effects", fx}, {"activate", true}}, this, [this, p, sent](const privileged::Result &r) {
        setBusy(false);
        if (!r.ok()) { showStatus("Writing the profile failed: " + r.message(), theme::DANGER); return; }
        loaded_ = sent;
        active_ = p;
        showStatus(QStringLiteral("Profile %1 written (%2 / %3 bytes).").arg(p).arg(r.json.value("bytes").toInt()).arg(REPORT_LEN), theme::OK);
        refreshAll();
        Q_EMIT stateChanged();
    });
}

void LightingTab::factoryReset() {
    if (!ready_ || busy_) return;
    const int p = editProfile_;
    if (QMessageBox::warning(this, "Lighting", QStringLiteral("Put profile %1 back to its factory effects?").arg(p),
                             QMessageBox::Reset | QMessageBox::Cancel) != QMessageBox::Reset)
        return;
    setBusy(true);
    run({{"op", "reset"}, {"profile", p}}, this, [this, p](const privileged::Result &r) {
        setBusy(false);
        if (!r.ok()) { showStatus("Reset failed: " + r.message(), theme::DANGER); return; }
        loaded_.clear();
        work_.clear();  // so reload() does not ask to discard
        showStatus(QStringLiteral("Profile %1 reset to factory defaults.").arg(p), theme::OK);
        reload(false);
    });
}

// ── editing ─────────────────────────────────────────────────────────────────

void LightingTab::moveKeys(const QSet<int> &keys, int into) {
    for (int i = 0; i < work_.size(); ++i) {
        if (i == into) continue;
        Effect &e = work_[i];
        if (e.allKeys()) {  // audio/Aurora "all lights": expand so single keys can leave it
            if (!needsHost(e.type)) e.keys = map_.unique();
            else continue;
        }
        e.keys.removeIf([&](int k) { return keys.contains(k); });
    }
    if (into >= 0) {
        QList<int> &k = work_[into].keys;
        if (work_[into].allKeys()) k.clear();
        for (int code : map_.unique())  // physical order, not click order
            if (keys.contains(code) && !k.contains(code)) k << code;
    }
    const Effect *keep = into >= 0 ? &work_[into] : nullptr;
    const Effect kept = keep ? *keep : Effect{};
    work_.removeIf([](const Effect &e) { return e.keys.isEmpty(); });
    if (keep) for (int i = 0; i < work_.size(); ++i) if (work_[i] == kept) { list_->setCurrentRow(i); break; }
}

void LightingTab::paintSelection(const QColor &c) {
    if (kb_->selection().isEmpty()) { showStatus("Select some keys first."); return; }
    int into = -1;
    for (int i = 0; i < work_.size(); ++i)
        if (work_[i].type == Static && work_[i].colorMode == ColorList && work_[i].colors == QList<QColor>{c}) { into = i; break; }
    if (into < 0) {
        if (work_.size() >= MAX_EFFECTS) { showStatus("Too many effects in one profile."); return; }
        work_.append(Effect{Static, 0, 0, 0, ColorList, {c}, {}});
        into = int(work_.size()) - 1;
    }
    moveKeys(kb_->selection(), into);
    refreshAll();
}

void LightingTab::addEffect() {
    if (work_.size() >= MAX_EFFECTS) { showStatus("Too many effects in one profile."); return; }
    QSet<int> keys = kb_->selection();
    if (keys.isEmpty()) keys = zoneKeys(Zone::Keyboard);
    work_.append(Effect{RainbowWave, 2, 4, 0, NoColors, {}, {}});
    moveKeys(keys, int(work_.size()) - 1);
    refreshAll();
    list_->setCurrentRow(int(work_.size()) - 1);
}

void LightingTab::removeEffect() {
    const int i = current();
    if (i < 0) return;
    work_.removeAt(i);
    refreshAll();
    list_->setCurrentRow(std::min(i, int(work_.size()) - 1));
}

void LightingTab::shiftEffect(int d) {
    const int i = current(), j = i + d;
    if (i < 0 || j < 0 || j >= work_.size()) return;
    work_.swapItemsAt(i, j);
    refreshAll();
    list_->setCurrentRow(j);
}

void LightingTab::editCurrent(const std::function<void(Effect &)> &f) {
    const int i = current();
    if (i < 0 || filling_) return;
    f(work_[i]);
    refreshAll();
}

void LightingTab::refreshAll() {
    refreshList();
    refreshEditor();
    refreshPreview();
    refreshBudget();
}

void LightingTab::refreshList() {
    filling_ = true;
    const int keep = list_->currentRow();
    list_->clear();
    for (const Effect &e : work_) {
        const QString keys = e.allKeys() ? QStringLiteral("all lights") : QStringLiteral("%1 lights").arg(e.keys.size());
        auto *it = new QListWidgetItem(swatch(e), typeName(e.type) + QStringLiteral("  ·  ") + keys);
        if (needsHost(e.type)) it->setToolTip("Needs Windows (audio or screen capture) — kept as is, not editable here");
        list_->addItem(it);
    }
    list_->setCurrentRow(std::clamp(keep, work_.isEmpty() ? -1 : 0, int(work_.size()) - 1));
    filling_ = false;
}

void LightingTab::rebuildColorChips() {
    while (QLayoutItem *it = chips_->takeAt(0)) {
        delete it->widget();
        delete it;
    }
    const int i = current();
    if (i < 0) return;
    const Effect &e = work_[i];
    if (e.colorMode != ColorList) return;
    for (int c = 0; c < e.colors.size(); ++c) {
        auto *b = new QToolButton;
        b->setFixedSize(26, 22);
        QPixmap pm(18, 12);
        pm.fill(e.colors[c]);
        b->setIcon(QIcon(pm));
        b->setToolTip(e.colors[c].name() + (multiColor(e.type) ? "  — click to change, right-click to remove" : "  — click to change"));
        b->setContextMenuPolicy(Qt::CustomContextMenu);
        connect(b, &QToolButton::clicked, this, [this, c] {
            const int i = current();
            if (i < 0 || c >= work_[i].colors.size()) return;
            const QColor n = QColorDialog::getColor(work_[i].colors[c], this, "Effect colour");
            if (n.isValid()) editCurrent([&](Effect &e) { e.colors[c] = n; });
        });
        connect(b, &QToolButton::customContextMenuRequested, this, [this, c] {
            editCurrent([&](Effect &e) { if (e.colors.size() > 1 && c < e.colors.size()) e.colors.removeAt(c); });
        });
        chips_->addWidget(b);
    }
    if (multiColor(e.type) && e.colors.size() < MAX_COLORS) {
        auto *add = new QToolButton;
        add->setText("+");
        add->setFixedSize(26, 22);
        add->setToolTip("Add a colour");
        connect(add, &QToolButton::clicked, this, [this] {
            const QColor n = QColorDialog::getColor(paint_, this, "Effect colour");
            if (n.isValid()) editCurrent([&](Effect &e) { e.colors << n; });
        });
        chips_->addWidget(add);
    }
    chips_->addStretch(1);
}

void LightingTab::refreshEditor() {
    const int i = current();
    const bool has = i >= 0;
    const bool editable = has && !needsHost(work_[i].type);
    editor_->setEnabled(editable && ready_);
    filling_ = true;
    if (has) {
        const Effect &e = work_[i];
        int ti = type_->findData(e.type);
        if (ti < 0) {  // audio/Aurora/unknown: shown, not selectable
            type_->addItem(typeName(e.type), e.type);
            ti = type_->count() - 1;
        }
        type_->setCurrentIndex(ti);
        speed_->setCurrentIndex(std::max(0, speed_->findData(e.speed)));
        dir_->setCurrentIndex(std::max(0, dir_->findData(e.direction)));
        cw_->setCurrentIndex(std::max(0, cw_->findData(e.clockwise)));
        random_->setChecked(e.colorMode == RandomColors);
        speedRow_->setVisible(hasSpeed(e.type));
        dirRow_->setVisible(hasDirection(e.type));
        cwRow_->setVisible(hasClockwise(e.type));
        colorRow_->setVisible(hasColorMode(e.type));
        keysLabel_->setText(e.allKeys() ? QStringLiteral("All lights") : QStringLiteral("%1 lights").arg(e.keys.size()));
        editor_->setTitle(QStringLiteral("Effect %1").arg(i + 1));
    } else {
        keysLabel_->clear();
        editor_->setTitle("Effect");
    }
    // Drop stale non-editable entries appended for a previous selection.
    for (int t = type_->count() - 1; t >= 0; --t)
        if (!editableTypes().contains(type_->itemData(t).toInt()) && t != type_->currentIndex()) type_->removeItem(t);
    filling_ = false;
    rebuildColorChips();
}

void LightingTab::refreshPreview() {
    QHash<int, QColor> colors;
    QSet<int> animated;
    const QList<int> order = map_.unique();
    for (const Effect &e : work_) {
        const QList<int> keys = e.allKeys() ? order : e.keys;
        for (int n = 0; n < keys.size(); ++n) {
            const int k = keys[n];
            QColor c;
            if (e.colorMode == ColorList && !e.colors.isEmpty()) c = e.colors[n % std::max<qsizetype>(1, e.type == Static ? 1 : e.colors.size())];
            else c = QColor::fromHsv(int(order.indexOf(k) * 360.0 / std::max<qsizetype>(1, order.size())) % 360, 190, 235);
            colors.insert(k, c);  // later effects draw over earlier ones
            if (e.type != Static) animated.insert(k); else animated.remove(k);
        }
    }
    kb_->setPreview(colors, animated);
}

void LightingTab::refreshBudget() {
    const int used = encodedLen(work_);
    const bool fits = used <= REPORT_LEN && work_.size() <= MAX_EFFECTS;
    budget_->setText(QStringLiteral("%1 / %2 bytes · %3 effect(s)%4").arg(used).arg(REPORT_LEN).arg(work_.size())
                         .arg(dirty() ? QStringLiteral(" · not applied") : QString()));
    budget_->setStyleSheet(fits ? QString() : QStringLiteral("color: %1;").arg(theme::DANGER));
    apply_->setEnabled(ready_ && !busy_ && dirty() && fits);
    revert_->setEnabled(ready_ && !busy_ && dirty());
}
