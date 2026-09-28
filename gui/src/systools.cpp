#include "systools.h"

#include "privileged.h"
#include "theme.h"

#include <QApplication>
#include <QClipboard>
#include <QDir>
#include <QFile>
#include <QFontDatabase>
#include <QHBoxLayout>
#include <QLabel>
#include <QRegularExpression>
#include <QScrollBar>
#include <QSet>
#include <QTextBrowser>
#include <QPointer>
#include <QProcess>
#include <QPushButton>
#include <QStandardPaths>
#include <QTabWidget>
#include <QTimer>
#include <QVBoxLayout>

namespace {

constexpr int CMD_TIMEOUT_MS = 20000;
constexpr qsizetype MAX_OUT = 1 << 20;

// A command is argv; argv[0] may be a pseudo-command:
//   @file <path>      contents of a file
//   @glob <pattern>   every file matching (dir/*/name)
//   @pcilink          PCIe link speed/width of GPUs and NVMe drives (sysfs)
struct Spec {
    const char *title, *about;
    QList<QStringList> cmds;
    const char *rootTool = nullptr;  // tune-helper {"op":"tool"} instead of cmds
};

const QHash<QString, QString> PACKAGES = {
    {"sensors", "sys-apps/lm-sensors"}, {"lscpu", "sys-apps/util-linux"}, {"cpupower", "sys-power/cpupower"},
    {"numactl", "sys-process/numactl"}, {"lspci", "sys-apps/pciutils"}, {"lsusb", "sys-apps/usbutils"},
    {"lsblk", "sys-apps/util-linux"}, {"nvidia-smi", "x11-drivers/nvidia-drivers"},
    {"vulkaninfo", "dev-util/vulkan-tools"}, {"glxinfo", "x11-apps/mesa-progs"}, {"lsmod", "sys-apps/kmod"},
    {"swapon", "sys-apps/util-linux"}, {"free", "sys-process/procps"}, {"df", "sys-apps/coreutils"}};

QString findExe(const QString &name) {
    QString p = QStandardPaths::findExecutable(name);
    if (p.isEmpty())  // user PATH often lacks sbin; Gentoo puts nvidia-smi in /opt/bin
        p = QStandardPaths::findExecutable(name, {"/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin",
                                                  "/sbin", "/bin", "/opt/bin"});
    return p;
}

QString readFile(const QString &path) {
    QFile f(path);
    return f.open(QIODevice::ReadOnly) ? QString::fromUtf8(f.read(MAX_OUT)) : QStringLiteral("(cannot read %1)\n").arg(path);
}

QString pciLinks() {
    QString out;
    const QDir d(QStringLiteral("/sys/bus/pci/devices"));
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System)) {
        const QString p = d.filePath(e) + QLatin1Char('/');
        const QString cls = readFile(p + "class").trimmed();
        const char *what = cls.startsWith("0x03") ? "GPU" : cls.startsWith("0x0108") ? "NVMe" : nullptr;
        if (!what || !QFile::exists(p + "current_link_speed")) continue;
        auto rd = [&](const char *f) { return readFile(p + QLatin1String(f)).trimmed(); };
        out += QStringLiteral("%1  %2  now %3 x%4  (max %5 x%6)\n")
                   .arg(e, QString::fromLatin1(what).leftJustified(4), rd("current_link_speed"), rd("current_link_width"),
                        rd("max_link_speed"), rd("max_link_width"));
    }
    return out.isEmpty() ? QStringLiteral("(no PCIe link information)\n") : out;
}

// "Serial Number: X", "UUID: X", "POWER_SUPPLY_SERIAL_NUMBER=X", "GPU UUID : X"…
// (dmidecode, smartctl, nvme, nvidia-smi, battery). Placeholder values that
// identify nothing are left visible.
const QRegularExpression SERIAL_RE(
    QStringLiteral(R"(^([^:=\n]*?(?:[Ss]erial|SERIAL|UUID|[Uu]uid|Asset Tag)[^:=\n]*?\s*[:=]\s*)(\S[^\n]*?)\s*$)"),
    QRegularExpression::MultilineOption);
const QSet<QString> PLACEHOLDERS = {"Not Specified", "Not Present", "None", "Unknown", "N/A", "Not Available",
                                    "To Be Filled By O.E.M.", "Default string", "Default String", "0", "00000000", ""};

class ToolPage : public QWidget {
public:
    explicit ToolPage(const Spec &s) : spec_(s) {
        auto *v = new QVBoxLayout(this);
        v->setContentsMargins(8, 6, 8, 6);
        auto *top = new QHBoxLayout;
        auto *about = new QLabel(QString::fromUtf8(s.about));
        about->setWordWrap(true);
        about->setProperty("role", "muted");
        top->addWidget(about, 1);
        serialsBtn_ = new QPushButton(QStringLiteral("Show serials"));
        serialsBtn_->setToolTip(QStringLiteral("Serial numbers and UUIDs are hidden; click one in the output to show just that one."));
        serialsBtn_->setVisible(false);
        top->addWidget(serialsBtn_);
        auto *run = new QPushButton(QStringLiteral("Run again"));
        auto *copy = new QPushButton(QStringLiteral("Copy"));
        top->addWidget(run);
        top->addWidget(copy);
        v->addLayout(top);
        text_ = new QTextBrowser;
        text_->setOpenLinks(false);  // anchors are serial toggles, not links
        text_->setLineWrapMode(QTextEdit::NoWrap);
        text_->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
        v->addWidget(text_, 1);
        connect(text_, &QTextBrowser::anchorClicked, this, [this](const QUrl &u) {
            bool ok = false;
            const int i = u.toString().mid(2).toInt(&ok);  // "s:<index>"
            if (!ok) return;
            if (shown_.contains(i)) shown_.remove(i); else shown_.insert(i);
            render();
        });
        connect(serialsBtn_, &QPushButton::clicked, this, [this] {
            // Any hidden → show all; all shown → hide all.
            if (shown_.size() < serials_) { for (int i = 0; i < serials_; ++i) shown_.insert(i); }
            else shown_.clear();
            render();
        });
        connect(run, &QPushButton::clicked, this, [this] { start(); });
        connect(copy, &QPushButton::clicked, this, [this] { QApplication::clipboard()->setText(text_->toPlainText()); });
    }

protected:
    void showEvent(QShowEvent *e) override {
        QWidget::showEvent(e);
        if (!ran_) start();  // lazy: nothing runs until the page is opened
    }

private:
    void start() {
        if (busy_) return;
        ran_ = busy_ = true;
        out_.clear();
        text_->setPlainText(QStringLiteral("Running…"));
        serialsBtn_->setVisible(false);
        if (spec_.rootTool) {
            privileged::run(privileged::helperPath(QStringLiteral("tune-helper")),
                            QJsonObject{{"op", "tool"}, {"tool", QString::fromLatin1(spec_.rootTool)}}, this,
                            [this](const privileged::Result &r) {
                busy_ = false;
                setOutput(r.ok() ? r.json.value("output").toString() : QStringLiteral("Not run: ") + r.message());
            }, 120000);
            return;
        }
        step(0);
    }

    void finish() {
        busy_ = false;
        setOutput(out_);
    }

    void setOutput(const QString &raw) {
        raw_ = raw;
        shown_.clear();  // a fresh run starts hidden again
        render(false);
    }

    // Output as HTML with every serial behind a click-to-toggle anchor.
    // Copy takes the text as displayed, so hidden serials stay hidden there too.
    void render(bool keepScroll = true) {
        const int vs = text_->verticalScrollBar()->value(), hs = text_->horizontalScrollBar()->value();
        QString html = QStringLiteral("<pre style='margin:0'>");
        qsizetype at = 0;
        int n = 0;
        auto it = SERIAL_RE.globalMatch(raw_);
        while (it.hasNext()) {
            const QRegularExpressionMatch m = it.next();
            const QString val = m.captured(2);
            if (PLACEHOLDERS.contains(val) || val.startsWith(QLatin1String("Not ")) || val.startsWith(QLatin1String("To Be Filled"))) continue;
            html += raw_.mid(at, m.capturedStart(2) - at).toHtmlEscaped();
            const bool show = shown_.contains(n);
            html += QStringLiteral("<a href='s:%1' style='color:%2; text-decoration:none'>%3</a>")
                        .arg(n).arg(QString::fromLatin1(show ? theme::ACCENT : theme::MUTED))
                        .arg(show ? val.toHtmlEscaped() : QStringLiteral("••••••••  [hidden]"));
            at = m.capturedEnd(2);
            ++n;
        }
        html += raw_.mid(at).toHtmlEscaped() + QStringLiteral("</pre>");
        serials_ = n;
        text_->setHtml(html);
        serialsBtn_->setVisible(n > 0);
        serialsBtn_->setText(shown_.size() < n ? QStringLiteral("Show serials") : QStringLiteral("Hide serials"));
        if (keepScroll) { text_->verticalScrollBar()->setValue(vs); text_->horizontalScrollBar()->setValue(hs); }
    }

    void step(int i) {
        if (i >= spec_.cmds.size()) { finish(); return; }
        const QStringList c = spec_.cmds.at(i);
        const QString shown = c.join(QLatin1Char(' '));
        if (c.first() == QLatin1String("@file")) { out_ += "$ cat " + c.at(1) + '\n' + readFile(c.at(1)) + '\n'; step(i + 1); return; }
        if (c.first() == QLatin1String("@pcilink")) { out_ += QStringLiteral("$ PCIe links (sysfs)\n") + pciLinks() + '\n'; step(i + 1); return; }
        if (c.first() == QLatin1String("@glob")) {
            const QString pat = c.at(1);
            const QString dir = pat.section('/', 0, -3), name = pat.section('/', -1);
            for (const QString &e : QDir(dir).entryList(QDir::Dirs | QDir::NoDotAndDotDot))
                if (QFile::exists(dir + '/' + e + '/' + name)) out_ += "$ cat " + dir + '/' + e + '/' + name + '\n' + readFile(dir + '/' + e + '/' + name) + '\n';
            step(i + 1);
            return;
        }
        const QString exe = findExe(c.first());
        if (exe.isEmpty()) {
            out_ += "$ " + shown + "\n(not installed — emerge " + PACKAGES.value(c.first(), QStringLiteral("?")) + ")\n\n";
            step(i + 1);
            return;
        }
        auto *p = new QProcess(this);
        p->setProcessChannelMode(QProcess::MergedChannels);
        QPointer<QProcess> guard(p);
        connect(p, &QProcess::finished, this, [this, p, i, shown](int, QProcess::ExitStatus) {
            out_ += "$ " + shown + '\n' + QString::fromUtf8(p->readAll().left(MAX_OUT)) + '\n';
            p->deleteLater();
            step(i + 1);
        });
        connect(p, &QProcess::errorOccurred, this, [this, p, i, shown](QProcess::ProcessError e) {
            if (e != QProcess::FailedToStart) return;
            out_ += "$ " + shown + "\n(failed to start)\n\n";
            p->deleteLater();
            step(i + 1);
        });
        QTimer::singleShot(CMD_TIMEOUT_MS, p, [guard] { if (guard && guard->state() != QProcess::NotRunning) guard->kill(); });
        p->start(exe, c.mid(1));
    }

    Spec spec_;
    QTextBrowser *text_;
    QPushButton *serialsBtn_;
    QString out_, raw_;
    QSet<int> shown_;
    int serials_ = 0;
    bool ran_ = false, busy_ = false;
};

} // namespace

namespace systools {

void addPages(QTabWidget *tabs) {
    const Spec specs[] = {
        {"Sensors", "lm-sensors: temperatures, fan speeds, voltages and power of every hwmon chip.", {{"sensors"}}},
        {"CPU", "Topology, caches, frequencies and the active cpufreq driver/governor.",
         {{"lscpu"}, {"lscpu", "-e"}, {"cpupower", "frequency-info"}}},
        {"Memory", "Usage, swap/zram and NUMA layout.", {{"free", "-h"}, {"swapon", "--show"}, {"numactl", "-H"}}},
        {"DMI", "dmidecode (root): firmware/BIOS, board, and every memory module with its part number and speed.", {}, "dmidecode"},
        {"PCI", "Devices with their kernel drivers, the bus tree, and the live PCIe link of GPUs and NVMe drives.",
         {{"lspci", "-nnk"}, {"lspci", "-tv"}, {"@pcilink"}}},
        {"PCIe detail", "lspci -vv (root): link capabilities/status, ASPM and AER capability of every device.", {}, "pcie"},
        {"USB", "Connected USB devices and the port tree.", {{"lsusb"}, {"lsusb", "-t"}}},
        {"Storage", "Block devices, schedulers and mounted filesystems.",
         {{"lsblk", "-o", "NAME,MODEL,SIZE,TYPE,FSTYPE,MOUNTPOINTS,ROTA,SCHED"}, {"df", "-hT", "-x", "tmpfs", "-x", "devtmpfs"}}},
        {"SMART", "Drive health (root): smartctl -a, or nvme smart-log, for every disk.", {}, "smart"},
        {"NVIDIA", "nvidia-smi -q: the full driver/GPU report. Wakes the dGPU if it is powered down.",
         {{"@file", "/proc/driver/nvidia/version"}, {"nvidia-smi", "-q"}}},
        {"Graphics", "Vulkan and OpenGL drivers as games see them (may wake the dGPU).",
         {{"vulkaninfo", "--summary"}, {"glxinfo", "-B"}}},
        {"Kernel", "Kernel version, boot command line and loaded modules.",
         {{"uname", "-a"}, {"@file", "/proc/cmdline"}, {"lsmod"}}},
        {"Battery", "Every power supply as the kernel reports it (capacity, cycles, charge limits).",
         {{"@glob", "/sys/class/power_supply/*/uevent"}}},
    };
    for (const Spec &s : specs) tabs->addTab(new ToolPage(s), QString::fromLatin1(s.title));
}

} // namespace systools
