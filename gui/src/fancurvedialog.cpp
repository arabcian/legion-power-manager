#include "fancurvedialog.h"
#include "privileged.h"
#include "theme.h"
#include <QDir>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonObject>
#include <QLabel>
#include <QMessageBox>
#include <QMouseEvent>
#include <QPainter>
#include <QPushButton>
#include <QTableWidget>
#include <QVBoxLayout>

// Unprivileged hint only: the LENOVO_FAN_METHOD WMI block exists. The helper then
// proves the machine answers Fan_Get_Table sanely before anything is written.
bool fanCurveSupported() {
    return !QDir(QStringLiteral("/sys/bus/wmi/devices"))
                .entryList({QStringLiteral("92549549-4BDE-4F06-AC04-CE8BF898DBAA*")},
                           QDir::Dirs | QDir::NoDotAndDotDot | QDir::System).isEmpty();
}

static const QVector<int> STOCK = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10};

// ── curve widget ────────────────────────────────────────────────────────────

FanCurveWidget::FanCurveWidget(QWidget *parent) : QWidget(parent) {
    setMinimumSize(420, 220);
    setMouseTracking(true);
}

void FanCurveWidget::setLevels(const QVector<int> &l) {
    if (l.size() != 10) return;
    levels_ = l;
    update();
}

QRectF FanCurveWidget::plot() const { return QRectF(rect()).adjusted(44, 14, -16, -30); }

QPointF FanCurveWidget::pos(int step, int level) const {
    const QRectF p = plot();
    return {p.left() + p.width() * step / 9.0, p.bottom() - p.height() * (level - 1) / 9.0};
}

int FanCurveWidget::levelAt(double y) const {
    const QRectF p = plot();
    return std::clamp(int(std::lround(1 + 9.0 * (p.bottom() - y) / p.height())), 1, 10);
}

// Keeps the curve non-decreasing: raising a point lifts the ones to its right,
// lowering it drops the ones to its left (the EC rejects nothing, so we must).
void FanCurveWidget::setPoint(int step, int level) {
    if (levels_[step] == level) return;
    levels_[step] = level;
    for (int j = step + 1; j < 10; ++j) levels_[j] = std::max(levels_[j], level);
    for (int j = step - 1; j >= 0; --j) levels_[j] = std::min(levels_[j], level);
    update();
    Q_EMIT changed();
}

void FanCurveWidget::mousePressEvent(QMouseEvent *e) {
    if (!isEnabled()) return;
    const QRectF p = plot();
    const int step = std::clamp(int(std::lround(9.0 * (e->position().x() - p.left()) / p.width())), 0, 9);
    drag_ = step;
    setPoint(step, levelAt(e->position().y()));
}

void FanCurveWidget::mouseMoveEvent(QMouseEvent *e) {
    if (drag_ >= 0) setPoint(drag_, levelAt(e->position().y()));
}

void FanCurveWidget::paintEvent(QPaintEvent *) {
    QPainter p(this);
    p.setRenderHint(QPainter::Antialiasing);
    const QRectF r = plot();
    p.fillRect(rect(), QColor(theme::BG1));
    p.setPen(QPen(QColor(theme::BORDER), 1));
    for (int i = 0; i < 10; ++i) {
        const QPointF a = pos(0, i + 1), b = pos(9, i + 1);
        p.drawLine(QPointF(r.left(), a.y()), QPointF(r.right(), b.y()));
        const QPointF c = pos(i, 1);
        p.drawLine(QPointF(c.x(), r.top()), QPointF(c.x(), r.bottom()));
    }
    p.setPen(QColor(theme::MUTED));
    for (int i = 1; i <= 10; ++i)
        p.drawText(QRectF(0, pos(0, i).y() - 8, 38, 16), Qt::AlignRight | Qt::AlignVCenter, QString::number(i));
    for (int s = 0; s < 10; ++s)
        p.drawText(QRectF(pos(s, 1).x() - 20, r.bottom() + 6, 40, 16), Qt::AlignCenter, QString::number(s));
    p.drawText(QRectF(r.left(), r.bottom() + 14, r.width(), 16), Qt::AlignRight, QStringLiteral("temperature step →"));

    const QColor accent(theme::ACCENT);
    QPolygonF line;
    for (int s = 0; s < 10; ++s) line << pos(s, levels_[s]);
    p.setPen(QPen(accent, 2.5));
    p.drawPolyline(line);
    p.setBrush(accent);
    p.setPen(QPen(QColor(theme::BG0), 1.5));
    for (int s = 0; s < 10; ++s) p.drawEllipse(line[s], drag_ == s ? 7 : 5.5, drag_ == s ? 7 : 5.5);
}

// ── dialog ──────────────────────────────────────────────────────────────────

FanCurveDialog::FanCurveDialog(const QString &helper, const QString &profile, QWidget *parent)
    : QDialog(parent), helper_(helper) {
    setWindowTitle("Fan curve (Custom mode)");
    setAttribute(Qt::WA_DeleteOnClose);
    resize(760, 640);
    auto *v = new QVBoxLayout(this);

    auto *intro = new QLabel("One curve for all fans: each point sets the fan <b>level</b> (1–10) for a temperature "
                             "step. Every fan turns the level into its own RPM at its own sensor's temperature — see the "
                             "table. Drag a point; the curve never goes down. The EC keeps the table across reboots and "
                             "profile changes, and follows it only in the <b>Custom</b> profile (on AC).");
    intro->setWordWrap(true);
    intro->setTextFormat(Qt::RichText);
    v->addWidget(intro);
    if (profile != QLatin1String("custom")) {
        auto *w = new QLabel(QStringLiteral("<span style='color:%1'>Current profile is <b>%2</b> — the curve is stored "
                                            "but only takes effect in Custom.</span>").arg(theme::WARN, profile.toHtmlEscaped()));
        w->setTextFormat(Qt::RichText);
        w->setWordWrap(true);
        v->addWidget(w);
    }

    curve_ = new FanCurveWidget;
    v->addWidget(curve_, 1);
    connect(curve_, &FanCurveWidget::changed, this, [this] {
        refreshTable();
        apply_->setEnabled(curve_->levels() != hw_);
    });

    table_ = new QTableWidget;
    table_->setEditTriggers(QAbstractItemView::NoEditTriggers);
    table_->setSelectionMode(QAbstractItemView::NoSelection);
    table_->verticalHeader()->setVisible(false);
    table_->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
    v->addWidget(table_, 1);

    status_ = new QLabel;
    status_->setWordWrap(true);
    status_->setTextFormat(Qt::RichText);
    v->addWidget(status_);

    auto *h = new QHBoxLayout;
    reload_ = new QPushButton("Reload");
    reload_->setToolTip("Read the table the EC holds now.");
    stock_ = new QPushButton("Stock (1…10)");
    stock_->setToolTip("Put the factory table in the editor (press Apply to write it).");
    apply_ = new QPushButton("Apply");
    apply_->setObjectName("btnAccent");
    auto *close = new QPushButton("Close");
    h->addWidget(reload_);
    h->addWidget(stock_);
    h->addStretch(1);
    h->addWidget(apply_);
    h->addWidget(close);
    v->addLayout(h);
    connect(reload_, &QPushButton::clicked, this, &FanCurveDialog::load);
    connect(stock_, &QPushButton::clicked, this, [this] {
        curve_->setLevels(STOCK);
        refreshTable();
        apply_->setEnabled(STOCK != hw_);
    });
    connect(apply_, &QPushButton::clicked, this, &FanCurveDialog::apply);
    connect(close, &QPushButton::clicked, this, &QDialog::close);

    refreshTable();
    load();
}

void FanCurveDialog::setBusy(bool b) {
    for (QWidget *w : {static_cast<QWidget *>(reload_), static_cast<QWidget *>(stock_), static_cast<QWidget *>(curve_)})
        w->setEnabled(!b);
    apply_->setEnabled(!b && !hw_.isEmpty() && curve_->levels() != hw_);
}

void FanCurveDialog::status(const QString &msg, const char *color) {
    status_->setText(color ? QStringLiteral("<span style='color:%1'>%2</span>").arg(color, msg.toHtmlEscaped())
                           : msg.toHtmlEscaped());
}

void FanCurveDialog::takeReply(const QJsonObject &j) {
    QVector<int> lv;
    for (const auto &x : j.value("levels").toArray()) lv << x.toInt();
    if (lv.size() == 10) { hw_ = lv; curve_->setLevels(lv); }
    const QJsonArray fans = j.value("fans").toArray();
    if (!fans.isEmpty()) {
        fans_.clear();
        for (const auto &f : fans) {
            const QJsonObject o = f.toObject();
            FanCurveFan fc;
            fc.id = o.value("fan").toInt();
            fc.sensor = o.value("sensor").toInt();
            for (const auto &x : o.value("rpm").toArray()) fc.rpm << x.toInt();
            for (const auto &x : o.value("temp").toArray()) fc.temp << x.toInt();
            if (fc.rpm.size() == 10 && fc.temp.size() == 10) fans_ << fc;
        }
    }
    refreshTable();
}

void FanCurveDialog::load() {
    setBusy(true);
    status("Reading the EC fan table…");
    privileged::run(helper_, QJsonObject{{"fan_table", "get"}}, this, [this](const privileged::Result &r) {
        if (r.ok()) {
            takeReply(r.json);
            status(r.json.contains("fans_error") ? "Table read; per-fan data unavailable: " + r.json.value("fans_error").toString()
                                                 : QStringLiteral("Read from the EC."),
                   r.json.contains("fans_error") ? theme::WARN : theme::OK);
            setBusy(false);
        } else if (r.json.value("unsupported").toBool()) {
            status(r.message(), theme::DANGER);
            for (QWidget *w : {static_cast<QWidget *>(reload_), static_cast<QWidget *>(stock_),
                               static_cast<QWidget *>(curve_), static_cast<QWidget *>(apply_)})
                w->setEnabled(false);
        } else {
            status("Read failed: " + r.message(), theme::DANGER);
            setBusy(false);
        }
    });
}

void FanCurveDialog::apply() {
    const QVector<int> lv = curve_->levels();
    QJsonArray a;
    for (int x : lv) a << x;
    setBusy(true);
    status("Writing the fan table…");
    privileged::run(helper_, QJsonObject{{"fan_table", "set"}, {"levels", a}}, this, [this](const privileged::Result &r) {
        if (r.ok()) {
            takeReply(r.json);
            status("Written and verified.", theme::OK);
        } else {
            status("Write failed: " + r.message(), theme::DANGER);
        }
        setBusy(false);
    });
}

void FanCurveDialog::refreshTable() {
    const QVector<int> lv = curve_->levels();
    table_->clear();
    table_->setRowCount(10);
    table_->setColumnCount(2 + fans_.size());
    QStringList hdr{"Step", "Level"};
    for (const FanCurveFan &f : fans_) hdr << QStringLiteral("Fan %1 (sensor %2)").arg(f.id).arg(f.sensor);
    table_->setHorizontalHeaderLabels(hdr);
    const QColor dim(theme::MUTED), changed(theme::ACCENT);
    for (int s = 0; s < 10; ++s) {
        auto put = [&](int col, const QString &t, const QColor *c = nullptr) {
            auto *it = new QTableWidgetItem(t);
            it->setTextAlignment(Qt::AlignCenter);
            if (c) it->setForeground(*c);
            table_->setItem(s, col, it);
        };
        put(0, QString::number(s));
        const bool diff = hw_.size() == 10 && hw_[s] != lv[s];
        put(1, diff ? QStringLiteral("%1 → %2").arg(hw_[s]).arg(lv[s]) : QString::number(lv[s]), diff ? &changed : nullptr);
        for (int k = 0; k < fans_.size(); ++k) {
            const FanCurveFan &f = fans_[k];
            const bool never = f.temp[s] >= 127;
            put(2 + k, QStringLiteral("%1 RPM @ %2").arg(f.rpm[lv[s] - 1])
                           .arg(never ? QStringLiteral("—") : QStringLiteral("%1 °C").arg(f.temp[s])),
                never ? &dim : nullptr);
            if (never) table_->item(s, 2 + k)->setToolTip("This fan's sensor never reaches this step (127 °C).");
        }
    }
}
