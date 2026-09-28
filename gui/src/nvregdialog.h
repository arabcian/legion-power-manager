#pragma once
// NVIDIA → Driver options: a curated set of nvidia / nvidia-drm module
// options (modprobe.d). Reads `tune-helper {"op":"nvreg_describe"}`
// unprivileged; Apply writes LPM's own modprobe.d file through pkexec.
#include <QDialog>
#include <QJsonObject>
#include <QList>

class QComboBox;
class QGridLayout;
class QLabel;
class QLineEdit;
class QPushButton;

class NvRegDialog : public QDialog {
    Q_OBJECT
public:
    explicit NvRegDialog(QWidget *parent = nullptr);

private:
    struct Row {
        QString name, kind;
        QComboBox *combo = nullptr;
        QLineEdit *edit = nullptr;
        QLabel *running = nullptr, *others = nullptr;
    };
    void load();
    void build(const QJsonObject &d);
    void apply();

    QGridLayout *grid_;
    QLabel *info_, *status_;
    QPushButton *apply_;
    QList<Row> rows_;
};
