#pragma once
// Optimizations → Boot options: read-only advisor for settings that only
// exist at boot (kernel command line, NVIDIA module options). Shows what the
// running system already has, explains each option, and builds a copyable
// command-line addition and /etc/modprobe.d file. It never writes to the
// bootloader: GRUB, systemd-boot, UKIs and a built-in CONFIG_CMDLINE all
// differ, and a wrong edit there costs a rescue boot.
#include <QWidget>
#include <functional>

class QCheckBox;
class QLabel;
class QPlainTextEdit;

class BootAdvisor : public QWidget {
    Q_OBJECT
public:
    explicit BootAdvisor(QWidget *parent = nullptr);

protected:
    void showEvent(QShowEvent *e) override;

private:
    struct Item {
        QString group;          // section: Latency / Performance / Stability / Power saving / Graphics & tools / NVIDIA
        bool modprobe;          // false: kernel command line; true: /etc/modprobe.d
        QString module;         // modprobe: module name ("nvidia")
        QString text;           // "amd_pstate=active" / "NVreg_X=1"
        QString why;
        bool suggest;           // ticked by default when not yet set
        bool caution;           // trade-off worth a warning colour
        std::function<bool()> applicable;
        std::function<int()> state;   // 1 = already in effect, 0 = not set, -1 = unknown
        QString excl;           // items sharing a key are mutually exclusive (ticking one unticks the others)
        QCheckBox *box = nullptr;
        QLabel *status = nullptr;
    };
    void refresh();
    void updateOutput();
    QList<Item> items_;
    QLabel *cmdline_ = nullptr;
    QPlainTextEdit *cmdOut_ = nullptr, *modOut_ = nullptr;
};
