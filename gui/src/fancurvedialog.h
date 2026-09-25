#pragma once
// Custom-mode fan table editor (Legion 16AFR10H): one row of 10 levels (1..10) shared
// by every fan; each fan maps a level to its own RPM at its own sensor's temperature
// steps. Reads/writes through legion-profile-helper {"fan_table": ...}.
#include <QDialog>
#include <QVector>

class QLabel;
class QPushButton;
class QTableWidget;

/// LENOVO_FAN_METHOD WMI block present (unprivileged); the helper probes the firmware.
bool fanCurveSupported();

struct FanCurveFan { int id = 0, sensor = 0; QVector<int> rpm, temp; };

class FanCurveWidget : public QWidget {
    Q_OBJECT
public:
    explicit FanCurveWidget(QWidget *parent = nullptr);
    void setLevels(const QVector<int> &l);
    QVector<int> levels() const { return levels_; }
    QSize sizeHint() const override { return {560, 280}; }
Q_SIGNALS:
    void changed();
protected:
    void paintEvent(QPaintEvent *) override;
    void mousePressEvent(QMouseEvent *e) override;
    void mouseMoveEvent(QMouseEvent *e) override;
    void mouseReleaseEvent(QMouseEvent *) override { drag_ = -1; }
private:
    QRectF plot() const;
    QPointF pos(int step, int level) const;
    int levelAt(double y) const;
    void setPoint(int step, int level);
    QVector<int> levels_ = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10};
    int drag_ = -1;
};

class FanCurveDialog : public QDialog {
    Q_OBJECT
public:
    FanCurveDialog(const QString &helper, const QString &profile, QWidget *parent = nullptr);
private:
    void load();
    void apply();
    void refreshTable();
    void setBusy(bool b);
    void status(const QString &msg, const char *color = nullptr);
    void takeReply(const QJsonObject &j);

    QString helper_;
    FanCurveWidget *curve_;
    QTableWidget *table_;
    QLabel *status_;
    QPushButton *reload_, *stock_, *apply_;
    QVector<FanCurveFan> fans_;
    QVector<int> hw_;  // what the EC holds
};
