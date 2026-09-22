#include "vfcurvewidget.h"
#include "theme.h"
#include <QKeyEvent>
#include <QMouseEvent>
#include <QPainter>
#include <QPainterPath>
#include <cmath>

static QColor C(const char *s) { return QColor(QString::fromLatin1(s)); }

VfCurveWidget::VfCurveWidget(QWidget *parent) : QWidget(parent) {
    setFocusPolicy(Qt::StrongFocus);
    setMinimumHeight(200);
    setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Expanding);
    setMouseTracking(false);
}

static double niceStep(double range, int targetTicks) {
    const double raw = range / std::max(1, targetTicks);
    const double mag = std::pow(10.0, std::floor(std::log10(raw)));
    for (double m : {1.0, 2.0, 2.5, 5.0, 10.0}) if (m * mag >= raw) return m * mag;
    return 10 * mag;
}

void VfCurveWidget::fitAxes() {
    if (pts_.isEmpty()) return;
    double x0 = pts_[0].x(), x1 = x0, y1 = pts_[0].y();
    for (const QPointF &p : pts_) { x0 = std::min(x0, p.x()); x1 = std::max(x1, p.x()); y1 = std::max(y1, p.y()); }
    xMin_ = x0 - 50; xMax_ = x1 + 50; yMin_ = 0; yMax_ = y1 + 200;
    update();
}

void VfCurveWidget::setPoints(const QVector<QPointF> &pts) {
    const bool reshaped = pts.size() != pts_.size();
    pts_ = pts;
    if (reshaped) { sel_.clear(); cur_ = 0; fitAxes(); }
    update();
    Q_EMIT selectionChanged();
}

void VfCurveWidget::selectAll() {
    sel_.clear();
    for (int i = 0; i < pts_.size(); ++i) sel_.insert(i);
    cur_ = 0;
    update();
    Q_EMIT selectionChanged();
}

void VfCurveWidget::clearSelection() {
    sel_.clear();
    cur_ = 0;
    update();
    Q_EMIT selectionChanged();
}

QRectF VfCurveWidget::plotRect() const { return QRectF(52, 10, width() - 62, height() - 44); }

QPointF VfCurveWidget::toPixel(QPointF v) const {
    const QRectF r = plotRect();
    return {r.left() + (v.x() - xMin_) / (xMax_ - xMin_) * r.width(),
            r.bottom() - (v.y() - yMin_) / (yMax_ - yMin_) * r.height()};
}

QPointF VfCurveWidget::toValue(QPointF px) const {
    const QRectF r = plotRect();
    return {xMin_ + (px.x() - r.left()) / r.width() * (xMax_ - xMin_),
            yMin_ + (r.bottom() - px.y()) / r.height() * (yMax_ - yMin_)};
}

int VfCurveWidget::hitTest(QPointF px) const {
    int best = -1;
    double bestD = 12 * 12;  // px radius
    for (int i = 0; i < pts_.size(); ++i) {
        const QPointF d = toPixel(pts_[i]) - px;
        const double dd = d.x() * d.x() + d.y() * d.y();
        if (dd < bestD) { bestD = dd; best = i; }
    }
    return best;
}

void VfCurveWidget::paintEvent(QPaintEvent *) {
    QPainter p(this);
    p.setRenderHint(QPainter::Antialiasing);
    p.fillRect(rect(), C(theme::BG1));
    const QRectF r = plotRect();
    p.fillRect(r, C(theme::BG0));

    QFont f = font();
    f.setPointSizeF(7.5);
    p.setFont(f);

    // grid + tick labels
    p.setPen(QPen(C(theme::BORDER_SOFT), 1));
    const double xs = niceStep(xMax_ - xMin_, 10), ys = niceStep(yMax_ - yMin_, 8);
    for (double x = std::ceil(xMin_ / xs) * xs; x <= xMax_; x += xs) {
        const double px = toPixel({x, 0}).x();
        p.setPen(QPen(C(theme::BORDER_SOFT), 1));
        p.drawLine(QPointF(px, r.top()), QPointF(px, r.bottom()));
        p.setPen(C(theme::MUTED));
        p.drawText(QRectF(px - 30, r.bottom() + 2, 60, 14), Qt::AlignHCenter | Qt::AlignTop, QString::number(x, 'f', 0));
    }
    for (double y = std::ceil(yMin_ / ys) * ys; y <= yMax_; y += ys) {
        const double py = toPixel({0, y}).y();
        p.setPen(QPen(C(theme::BORDER_SOFT), 1));
        p.drawLine(QPointF(r.left(), py), QPointF(r.right(), py));
        p.setPen(C(theme::MUTED));
        p.drawText(QRectF(0, py - 7, r.left() - 4, 14), Qt::AlignRight | Qt::AlignVCenter, QString::number(y, 'f', 0));
    }
    p.setPen(C(theme::WARN));
    p.drawText(QRectF(r.left(), r.bottom() + 16, r.width(), 14), Qt::AlignHCenter, QStringLiteral("Voltage (mV)"));
    p.save();
    p.translate(10, r.center().y());
    p.rotate(-90);
    p.drawText(QRectF(-60, -8, 120, 14), Qt::AlignCenter, QStringLiteral("Frequency (MHz)"));
    p.restore();
    p.setPen(QPen(C(theme::BORDER), 1));
    p.drawRect(r);

    if (pts_.isEmpty()) {
        p.setPen(C(theme::MUTED));
        p.drawText(r, Qt::AlignCenter, QStringLiteral("No curve loaded — press “Read Current Curve”."));
        return;
    }
    p.setClipRect(r.adjusted(-6, -6, 6, 6));
    QPainterPath path(toPixel(pts_[0]));
    for (int i = 1; i < pts_.size(); ++i) path.lineTo(toPixel(pts_[i]));
    p.setPen(QPen(C(theme::ACCENT), 2.2));
    p.setBrush(Qt::NoBrush);
    p.drawPath(path);

    p.setPen(QPen(C(theme::BG0), 1.2));
    p.setBrush(C(theme::INFO));
    for (const QPointF &v : pts_) p.drawEllipse(toPixel(v), 2.6, 2.6);
    p.setPen(QPen(C(theme::DANGER), 2));
    p.setBrush(C(theme::WARN));
    for (int i : sel_) if (i < pts_.size()) p.drawEllipse(toPixel(pts_[i]), 5.5, 5.5);
    if (hasFocus() && cur_ < pts_.size()) {
        p.setPen(QPen(C(theme::FG), 1, Qt::DotLine));
        p.setBrush(Qt::NoBrush);
        p.drawEllipse(toPixel(pts_[cur_]), 8, 8);
    }
}

void VfCurveWidget::mousePressEvent(QMouseEvent *e) {
    setFocus();
    const int hit = hitTest(e->position());
    dragIndex_ = -1;
    groupDrag_ = false;
    if (hit >= 0) {
        if (sel_.contains(hit)) {
            dragIndex_ = hit;
            dragFreq_ = pts_[hit].y();
        } else {
            if (!(e->modifiers() & Qt::ControlModifier)) sel_.clear();
            sel_.insert(hit);
            Q_EMIT selectionChanged();
        }
        cur_ = hit;
    } else if (sel_.size() == pts_.size() && !pts_.isEmpty()) {
        groupDrag_ = true;
    } else {
        sel_.clear();
        Q_EMIT selectionChanged();
    }
    if (groupDrag_) { groupStartY_ = toValue(e->position()).y(); groupApplied_ = 0; setCursor(Qt::ClosedHandCursor); }
    if (dragIndex_ >= 0) setCursor(Qt::ClosedHandCursor);
    update();
}

void VfCurveWidget::mouseMoveEvent(QMouseEvent *e) {
    const double y = toValue(e->position()).y();
    if (dragIndex_ >= 0) {
        dragFreq_ = std::clamp(y, 0.0, yMax_);
        pts_[dragIndex_].setY(std::round(dragFreq_));  // local preview; model updates on release
        update();
    } else if (groupDrag_) {
        const int total = int(std::lround(y - groupStartY_));
        if (total != groupApplied_) { Q_EMIT selectionShifted(total - groupApplied_); groupApplied_ = total; }
    }
}

void VfCurveWidget::mouseReleaseEvent(QMouseEvent *) {
    if (dragIndex_ >= 0) Q_EMIT pointEdited(dragIndex_, int(std::lround(dragFreq_)));
    dragIndex_ = -1;
    groupDrag_ = false;
    unsetCursor();
}

void VfCurveWidget::keyPressEvent(QKeyEvent *e) {
    if (pts_.isEmpty()) return QWidget::keyPressEvent(e);
    const int step = (e->modifiers() & Qt::ShiftModifier) ? 15 : 1;
    switch (e->key()) {
    case Qt::Key_Right: cur_ = (cur_ + 1) % pts_.size(); sel_.insert(cur_); Q_EMIT selectionChanged(); break;
    case Qt::Key_Left: cur_ = (cur_ - 1 + pts_.size()) % pts_.size(); sel_.insert(cur_); Q_EMIT selectionChanged(); break;
    case Qt::Key_Up: if (!sel_.isEmpty()) Q_EMIT selectionShifted(step); break;
    case Qt::Key_Down: if (!sel_.isEmpty()) Q_EMIT selectionShifted(-step); break;
    case Qt::Key_Space:
        if (sel_.contains(cur_)) sel_.remove(cur_); else sel_.insert(cur_);
        Q_EMIT selectionChanged();
        break;
    case Qt::Key_Escape: clearSelection(); break;
    case Qt::Key_A:
        if (e->modifiers() & Qt::ControlModifier) { selectAll(); break; }
        [[fallthrough]];
    default: return QWidget::keyPressEvent(e);
    }
    update();
}
