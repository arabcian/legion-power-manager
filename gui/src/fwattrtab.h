#pragma once
// Firmware Attributes tab — port of fwattr_tab.py.
// One slider/spinbox per /sys/class/firmware-attributes/*/attributes/*
// entry, ranges taken from the driver itself; writes via fwattr-helper.
#include <QWidget>

class QLabel;
class QPushButton;
class QTabWidget;
class QTimer;
class QSlider;
class QSpinBox;

struct FwAttr {
    QString device, name, path, displayName;
    int current = 0, def = 0, min = 0, max = 0, step = 1;
    bool ranged = true;
};

class FwattrTab : public QWidget {
    Q_OBJECT
public:
    explicit FwattrTab(QWidget *parent = nullptr);
    static QList<FwAttr> discover();

public Q_SLOTS:
    void refreshLockState();
    void rebuild();

private:
    struct Row { FwAttr info; QSlider *slider; QSpinBox *spin; };
    void applyRow(int index);
    void applyAll();
    void setBusy(bool busy);
    void showStatus(const QString &msg, int timeoutMs = 3000);
    int snapped(const Row &r) const;

    QList<Row> rows_;
    QLabel *banner_, *status_;
    QPushButton *applyAll_, *rescan_;
    QTabWidget *tabs_;
    QTimer *statusTimer_;
    bool locked_ = false, busy_ = false;
};
