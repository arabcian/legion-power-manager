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
    // Unranged attributes the kernel refuses to write through sysfs, handled
    // instead through the Lenovo WMI method via acpi_call (legion-gpu-helper).
    QString wmiKey;              // helper knob key; empty = normal sysfs attribute
    int wmiMin = 0, wmiMax = 0;  // range used for these (none from firmware)
    bool viaWmi() const { return !wmiKey.isEmpty(); }
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
    struct Row { FwAttr info; QSlider *slider; QSpinBox *spin; QPushButton *apply = nullptr; QPushButton *def = nullptr; };
    void applyRow(int index);
    void applyAll();
    void applyWmi(const QList<int> &indices);  // write WMI-handled rows via legion-gpu-helper
    void readWmi();                            // refresh their true values (WMAE get)
    void setBusy(bool busy);
    void showStatus(const QString &msg, int timeoutMs = 3000);
    int snapped(const Row &r) const;

    QList<Row> rows_;
    QLabel *banner_, *status_;
    QPushButton *applyAll_, *rescan_;
    QTabWidget *tabs_;
    QTimer *statusTimer_;
    bool locked_ = false, busy_ = false, wmiReady_ = false;
};
