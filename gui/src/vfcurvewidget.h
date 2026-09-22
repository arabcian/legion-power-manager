#pragma once
// V/F curve plot + editor, drawn with QPainter (no QtCharts dependency).
// Replaces VFCurveWidget(QChartView) from nvcurve_gui.py.
//
// The widget only *displays* points and reports user intent; the owning
// tab keeps the offset model and pushes recomputed points back. That keeps
// every edit path (drag, group drag, arrow keys, spin box) going through
// one place, unlike the Python widget where arrow-key edits changed only
// the plotted points and were lost on the next recompute.
#include <QPointF>
#include <QSet>
#include <QVector>
#include <QWidget>

class VfCurveWidget : public QWidget {
    Q_OBJECT
public:
    explicit VfCurveWidget(QWidget *parent = nullptr);

    /// x = mV, y = MHz. Keeps the selection if the point count is unchanged.
    void setPoints(const QVector<QPointF> &pts);
    const QVector<QPointF> &points() const { return pts_; }
    const QSet<int> &selection() const { return sel_; }
    int current() const { return cur_; }
    /// Full view of the curve; also resets zoom and pan.
    void fitAxes();

    // ── zoom / pan (voltage axis; frequency auto-fits to what is visible) ──
    static constexpr double MAX_ZOOM = 12.0;
    double zoom() const { return zoom_; }
    /// 0 = left edge of the curve, 1 = right edge (meaningless at zoom 1).
    double pan() const { return pan_; }
    void setZoom(double z);
    void setPan(double p);
    /// Zooms keeping voltage `anchorMv` under the same pixel.
    void zoomAt(double factor, double anchorMv);

public Q_SLOTS:
    void selectAll();
    void clearSelection();

Q_SIGNALS:
    void selectionChanged();
    /// Zoom or pan changed (wheel, keys, or setZoom/setPan).
    void viewChanged();
    /// Single point dragged to `freqMhz` (emitted on release).
    void pointEdited(int index, int freqMhz);
    /// Whole selection shifted by `deltaMhz` (group drag / arrow keys).
    void selectionShifted(int deltaMhz);

protected:
    void paintEvent(QPaintEvent *) override;
    void mousePressEvent(QMouseEvent *) override;
    void mouseMoveEvent(QMouseEvent *) override;
    void leaveEvent(QEvent *) override;
    void mouseReleaseEvent(QMouseEvent *) override;
    void keyPressEvent(QKeyEvent *) override;
    void wheelEvent(QWheelEvent *) override;

private:
    QRectF plotRect() const;
    QPointF toPixel(QPointF v) const;
    QPointF toValue(QPointF px) const;
    int hitTest(QPointF px) const;
    void applyView();

    QVector<QPointF> pts_;
    QSet<int> sel_;
    int cur_ = 0;
    double xMin_ = 600, xMax_ = 1200, yMin_ = 0, yMax_ = 3000;
    double fullX0_ = 600, fullX1_ = 1200, fullY1_ = 3000;
    double zoom_ = 1.0, pan_ = 0.5;
    int dragIndex_ = -1;
    int hover_ = -1;  // point under the cursor (no button held)
    bool groupDrag_ = false;
    double groupStartY_ = 0;
    int groupApplied_ = 0;
    double dragFreq_ = 0;
};
