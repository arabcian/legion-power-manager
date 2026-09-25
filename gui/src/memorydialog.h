#pragma once
// Read-only memory view: DDR5 SPD (JEDEC base profile) of every module, through
// legion-profile-helper {"memory": "spd"}. Works on any machine with spd5118.
#include <QDialog>

class QLabel;
class QTableWidget;

class MemoryDialog : public QDialog {
    Q_OBJECT
public:
    MemoryDialog(const QString &helper, QWidget *parent = nullptr);
private:
    void load();
    void openEditor();
    QString helper_;
    QTableWidget *table_;
    QLabel *status_;
};
