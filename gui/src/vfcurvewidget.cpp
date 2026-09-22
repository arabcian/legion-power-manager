#include "vfcurvewidget.h"
#include "theme.h"
#include <QKeyEvent>
#include <QMouseEvent>
#include <QPainter>
#include <QPainterPath>
#include <QWheelEvent>
#include <cmath>

static QColor C(const char *s) { return QColor(QString::fromLatin1(s)); }

VfCurveWidget::VfCurveWidget(QWidget *parent) : QWidget(parent) {
    setFocusPolicy(Qt::StrongFocus);
    setMinimumHeight(200);
    setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Expanding);
    setMouseTracking(true);  // hover highlight
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
    fullX0_ = x0 - 50; fullX1_ = x1 + 50; fullY1_ = y1 + 200;
    zoom_ = 1.0;
    pan_ = 0.5;
    applyView();
}

void VfCurveWidget::applyView() {
    const double full = fullX1_ - fullX0_;
    const double span = full / zoom_;
    xMin_ = fullX0_ + pan_ * (full - span);
    xMax_ = xMin_ + span;
    if (zoom_ <= 1.0001) {
        yMin_ = 0;
        yMax_ = fullY1_;
    } else {
        // Frequency follows the visible points, otherwise a zoomed-in
        // stretch of the curve would be a flat line at the top.
        double lo = 1e9, hi = -1e9;
        for (const QPointF &p : pts_) {
            if (p.x() < xMin_ - 1 || p.x() > xMax_ + 1) continue;
            lo = std::min(lo, p.y());
            hi = std::max(hi, p.y());
        }
        if (lo > hi) { yMin_ = 0; yMax_ = fullY1_; }
        else {
            const double pad = std::max(60.0, (hi - lo) * 0.15);
            yMin_ = std::max(0.0, lo - pad);
            yMax_ = hi + pad;
        }
    }
    update();
    Q_EMIT viewChanged();
}

void VfCurveWidget::setZoom(double z) {
    z = std::clamp(z, 1.0, MAX_ZOOM);
    if (std::abs(z - zoom_) < 1e-6) return;
    zoomAt(z / zoom_, (xMin_ + xMax_) / 2);
}

void VfCurveWidget::setPan(double p) {
    p = std::clamp(p, 0.0, 1.0);
    if (std::abs(p - pan_) < 1e-6) return;
    pan_ = p;
    applyView();
}

void VfCurveWidget::zoomAt(double factor, double anchorMv) {
    const double z = std::clamp(zoom_ * factor, 1.0, MAX_ZOOM);
    const double full = fullX1_ - fullX0_;
    const double frac = (anchorMv - xMin_) / std::max(1e-9, xMax_ - xMin_);  // anchor's screen position
    const double span = full / z;
    const double newMin = anchorMv - frac * span;
    zoom_ = z;
    pan_ = full - span > 1e-9 ? std::clamp((newMin - fullX0_) / (full - span), 0.0, 1.0) : 0.5;
    applyView();
}

void VfCurveWidget::wheelEvent(QWheelEvent *e) {
    if (pts_.isEmpty()) return QWidget::wheelEvent(e);
    const double steps = e->angleDelta().y() / 120.0;
    if (steps == 0) return QWidget::wheelEvent(e);
    if (e->modifiers() & Qt::ShiftModifier) {
        setPan(pan_ - steps * 0.1 / zoom_ * 2);
    } else {
        zoomAt(std::pow(1.25, steps), toValue(e->position()).x());
    }
    e->accept();
}

void VfCurveWidget::setPoints(const QVector<QPointF> &pts) {
    const bool reshaped = pts.size() != pts_.size();
    pts_ = pts;
    if (reshaped) { sel_.clear(); cur_ = 0; fitAxes(); }
    else if (zoom_ > 1.0001 && dragIndex_ < 0 && !groupDrag_) applyView();  // keep edited points in frame
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
    double bestD = 16 * 16;  // px radius
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
    if (zoom_ > 1.0001) {
        // Overview strip: where the visible window sits on the whole curve.
        const double full = fullX1_ - fullX0_;
        const QRectF strip(r.left() + 1, r.top() + 1, r.width() - 2, 3);
        p.fillRect(strip, C(theme::BG2));
        p.fillRect(QRectF(strip.left() + (xMin_ - fullX0_) / full * strip.width(), strip.top(),
                          strip.width() / zoom_, strip.height()), C(theme::ACCENT_SOFT));
        p.setPen(C(theme::MUTED));
        p.drawText(r.adjusted(0, 6, -6, 0), Qt::AlignRight | Qt::AlignTop, QStringLiteral("%1×").arg(zoom_, 0, 'f', 1));
    }

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
    for (const QPointF &v : pts_) p.drawEllipse(toPixel(v), 4.2, 4.2);
    p.setPen(QPen(C(theme::DANGER), 2));
    p.setBrush(C(theme::WARN));
    for (int i : sel_) if (i < pts_.size()) p.drawEllipse(toPixel(pts_[i]), 6.5, 6.5);
    if (hover_ >= 0 && hover_ < pts_.size() && dragIndex_ < 0) {
        p.setPen(QPen(C(theme::FG), 2));
        p.setBrush(sel_.contains(hover_) ? C(theme::WARN) : C(theme::ACCENT));
        p.drawEllipse(toPixel(pts_[hover_]), 7.5, 7.5);
    }
    if (hasFocus() && cur_ < pts_.size()) {
        p.setPen(QPen(C(theme::FG), 1, Qt::DotLine));
        p.setBrush(Qt::NoBrush);
        p.drawEllipse(toPixel(pts_[cur_]), 10, 10);
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
    if (e->buttons() == Qt::NoButton) {
        const int h = hitTest(e->position());
        if (h != hover_) {
            hover_ = h;
            if (h >= 0) setCursor(Qt::PointingHandCursor); else unsetCursor();
            update();
        }
        return;
    }
    const double y = toValue(e->position()).y();
    if (dragIndex_ >= 0) {
        dragFreq_ = std::clamp(y, 0.0, fullY1_);
        pts_[dragIndex_].setY(std::round(dragFreq_));  // local preview; model updates on release
        update();
    } else if (groupDrag_) {
        const int total = int(std::lround(y - groupStartY_));
        if (total != groupApplied_) { Q_EMIT selectionShifted(total - groupApplied_); groupApplied_ = total; }
    }
}

void VfCurveWidget::leaveEvent(QEvent *e) {
    QWidget::leaveEvent(e);
    if (hover_ >= 0) { hover_ = -1; unsetCursor(); update(); }
}

void VfCurveWidget::mouseReleaseEvent(QMouseEvent *) {
    if (dragIndex_ >= 0) Q_EMIT pointEdited(dragIndex_, int(std::lround(dragFreq_)));
    dragIndex_ = -1;
    groupDrag_ = false;
    unsetCursor();
    if (zoom_ > 1.0001) applyView();
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
    case Qt::Key_Plus: case Qt::Key_Equal: setZoom(zoom_ * 1.25); break;
    case Qt::Key_Minus: setZoom(zoom_ / 1.25); break;
    case Qt::Key_0: fitAxes(); break;
    case Qt::Key_A:
        if (e->modifiers() & Qt::ControlModifier) { selectAll(); break; }
        [[fallthrough]];
    default: return QWidget::keyPressEvent(e);
    }
    update();
}
