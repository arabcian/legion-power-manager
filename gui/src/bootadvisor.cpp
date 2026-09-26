#include "bootadvisor.h"
#include "amdgpu.h"
#include "sysinfo.h"
#include "theme.h"
#include <QApplication>
#include <QCheckBox>
#include <QClipboard>
#include <QDir>
#include <QFile>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QLabel>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QScrollArea>
#include <QShowEvent>
#include <QSysInfo>
#include <QVBoxLayout>

static QString readAll(const QString &p) {
    QFile f(p);
    return f.open(QIODevice::ReadOnly) ? QString::fromUtf8(f.read(256 * 1024)).trimmed() : QString();
}

// Read once per refresh (≈45 status checks consult it), not once per check.
static QStringList g_cmdline;
static void reloadCmdline() { g_cmdline = readAll("/proc/cmdline").split(' ', Qt::SkipEmptyParts); }
static bool onCmdline(const QString &tok) { return g_cmdline.contains(tok); }

/// "Key: value" from /proc/driver/nvidia/params (the running module's options).
static QString nvParam(const QString &key) {
    for (const QString &l : readAll("/proc/driver/nvidia/params").split('\n'))
        if (l.startsWith(key + ':')) return l.section(':', 1).trimmed();
    return {};
}

static bool cpuFlag(const QString &f) {
    // First "flags" line of /proc/cpuinfo, parsed once (the file is ~100 KB on a 32-thread CPU).
    static const QStringList flags = [] {
        QFile file(QStringLiteral("/proc/cpuinfo"));
        if (!file.open(QIODevice::ReadOnly)) return QStringList();
        while (!file.atEnd()) {
            const QByteArray line = file.readLine();
            if (line.startsWith("flags")) return QString::fromLatin1(line.mid(line.indexOf(':') + 1)).split(' ', Qt::SkipEmptyParts);
        }
        return QStringList();
    }();
    return flags.contains(f);
}

static QLabel *muted(const QString &t) {
    auto *l = new QLabel(t);
    l->setProperty("role", "muted");
    l->setWordWrap(true);
    return l;
}

BootAdvisor::BootAdvisor(QWidget *parent) : QWidget(parent) {
    reloadCmdline();
    const bool amd = sysinfo::isAmd(), intel = sysinfo::isIntel();
    const bool nvidia = QFile::exists("/proc/driver/nvidia/params");
    const auto always = [] { return true; };
    QString grp;  // section the following cmd() items go to
    auto cmd = [&](QString text, QString why, bool suggest, bool caution, std::function<bool()> app, std::function<int()> st) {
        items_.append({grp, false, {}, text, why, suggest, caution, app, st, {}});
    };
    auto mod = [&](QString module, QString text, QString why, bool suggest, bool caution, std::function<bool()> app, std::function<int()> st) {
        items_.append({QStringLiteral("NVIDIA"), true, module, text, why, suggest, caution, app, st, {}});
    };
    auto param = [](const QString &mod, const QString &p) { return readAll("/sys/module/" + mod + "/parameters/" + p); };
    auto hasParam = [](const QString &mod, const QString &p) { return QFile::exists("/sys/module/" + mod + "/parameters/" + p); };
    auto vuln = [](const QString &name) { return readAll("/sys/devices/system/cpu/vulnerabilities/" + name); };
    auto excl = [this](const QString &key) { items_.last().excl = key; };
    // Hybrid mode = an AMD *display* controller on the PCI bus (any other AMD
    // device — USB, audio, PSP — is there on every AMD platform).
    const bool hybrid = [] {
        const QDir d(QStringLiteral("/sys/bus/pci/devices"));
        for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System))
            if (readAll(d.filePath(e) + "/vendor") == "0x1002" && readAll(d.filePath(e) + "/class").startsWith("0x03")) return true;
        return false;
    }();
    // The open kernel modules always run the GSP firmware (and Blackwell only has those).
    const bool nvOpen = readAll("/proc/driver/nvidia/version").contains("Open Kernel Module");
    auto tokenOr = [](QString tok, std::function<int()> live) { return [tok, live] { return onCmdline(tok) ? 1 : live ? live() : 0; }; };

    // ── kernel command line ──
    grp = QStringLiteral("Latency");
    cmd("preempt=full", "Fully preemptible kernel (PREEMPT_DYNAMIC kernels only): lower worst-case scheduling latency for the "
        "desktop, audio and games, at a small throughput cost.",
        true, false, [] { return QSysInfo::kernelVersion().contains("PREEMPT_DYNAMIC") || readAll("/proc/version").contains("PREEMPT_DYNAMIC"); },
        tokenOr("preempt=full", {}));
    cmd("nowatchdog", "Disables the soft/hard lockup detectors (a periodic timer interrupt on every CPU). Less background jitter; "
        "the kernel no longer reports lockups by itself.", true, false, always, tokenOr("nowatchdog", {}));
    cmd("nmi_watchdog=0", "No NMI watchdog: frees one hardware performance counter per CPU (profilers, MangoHud) and removes its "
        "periodic NMI.", true, false, always,
        tokenOr("nmi_watchdog=0", [] { return readAll("/proc/sys/kernel/nmi_watchdog") == "0" ? 1 : 0; }));
    cmd("tsc=nowatchdog", "Stops the clocksource watchdog from periodically cross-checking the TSC against HPET: one less "
        "timer interrupt source, meant for tight latency requirements. Only on a stable TSC — if dmesg ever shows "
        "\"Marking TSC unstable\", leave this out.", false, false, always, tokenOr("tsc=nowatchdog", {}));
    cmd("threadirqs", "Runs device interrupt handlers as kernel threads, so their priority can be set (rtirq, rtkit): "
        "the classic fix for audio crackle and input jitter under load. Slightly more overhead per interrupt.",
        false, false, always, tokenOr("threadirqs", {}));
    cmd("skew_tick=1", "Offsets each CPU's timer tick so they do not all fire at once and contend on the same kernel locks: "
        "smoother jitter on many-core CPUs. Costs a little power (CPUs wake at different moments).",
        false, false, always, tokenOr("skew_tick=1", {}));
    cmd("rcupdate.rcu_expedited=1", "Makes RCU grace periods expedited: kernel operations that wait for RCU (module load, "
        "network and cgroup changes, some driver paths) finish much sooner. Sends more IPIs; small power cost.",
        false, false, always, tokenOr("rcupdate.rcu_expedited=1", [param] { return param("rcupdate", "rcu_expedited") == "1" ? 1 : 0; }));
    cmd("cpuidle.governor=teo", "Boot with the teo idle governor (CPU tab switches it at runtime): better C-state choices on CPUs "
        "with few idle states, like Zen.", false, false,
        [] { return readAll("/sys/devices/system/cpu/cpuidle/available_governors").contains("teo"); },
        tokenOr("cpuidle.governor=teo", [] { return readAll("/sys/devices/system/cpu/cpuidle/current_governor") == "teo" ? 1 : 0; }));

    cmd("rcu_nocbs=all", "Offloads RCU callback processing from every CPU to rcuo kthreads the scheduler can move around: "
        "fewer softirq bursts on the CPU a game or audio thread is running on. Needs CONFIG_RCU_NOCB_CPU (no-op without it).",
        false, false, always, tokenOr("rcu_nocbs=all", {}));
    cmd("usbcore.autosuspend=-1", "Never autosuspend USB devices: no wake-up lag on the first move of a mouse / wireless dongle, "
        "and fixes devices that drop off after idle. A little idle power per device.", false, false,
        [hasParam] { return hasParam("usbcore", "autosuspend"); },
        tokenOr("usbcore.autosuspend=-1", [param] { return param("usbcore", "autosuspend") == "-1" ? 1 : 0; }));
    cmd("mce=ignore_ce", "Ignores corrected machine-check errors: removes the periodic CMCI/MCE polling timer (a real-time tuning "
        "classic). Corrected cache / memory errors are then no longer logged — you lose the early warning of failing RAM.",
        false, true, always, tokenOr("mce=ignore_ce", {}));
    cmd("tsc=reliable", "Declares the TSC reliable: no clocksource watchdog and no fallback to HPET at runtime. Stronger than "
        "tsc=nowatchdog — only when the TSC has never been marked unstable on this machine.", false, true,
        [] { return cpuFlag("constant_tsc") && cpuFlag("nonstop_tsc"); }, tokenOr("tsc=reliable", {}));

    grp = QStringLiteral("Performance");
    cmd("amd_pstate=active", "Boot straight into the amd-pstate EPP driver (the mode the CPU tab's EPP / dynamic EPP settings need). "
        "The CPU tab can switch it at runtime too; this makes it the default from the first second.",
        true, false, [amd] { return amd; },
        tokenOr("amd_pstate=active", [] { return readAll("/sys/devices/system/cpu/amd_pstate/status") == "active" ? 1 : 0; }));
    cmd("split_lock_detect=off", "Games that do split-locked atomics (some Windows titles under Proton) get throttled by the kernel's "
        "split-/bus-lock mitigation, which shows as sudden stutter. off = never throttle them.",
        true, false, [] { return cpuFlag("split_lock_detect") || cpuFlag("bus_lock_detect"); }, tokenOr("split_lock_detect=off", {}));
    cmd("transparent_hugepage=madvise", "THP only where programs ask for it — the low-stutter default for gaming (Memory tab has the "
        "runtime switch).", false, false, always,
        tokenOr("transparent_hugepage=madvise", [] { return readAll("/sys/kernel/mm/transparent_hugepage/enabled").contains("[madvise]") ? 1 : 0; }));
    cmd("zswap.enabled=1", "Compressed cache in RAM in front of swap: much smoother behaviour under memory pressure than going "
        "straight to the swap device.", true, false,
        [] { return readAll("/proc/swaps").count('\n') >= 1; },
        tokenOr("zswap.enabled=1", [] { return readAll("/sys/module/zswap/parameters/enabled") == "Y" ? 1 : 0; }));
    cmd("iommu=pt", "IOMMU in pass-through for devices the host drives itself: no DMA address translation on NVMe / GPU / "
        "network I/O. VFIO passthrough still works; only matters if you do not rely on DMA isolation.",
        false, false, always, tokenOr("iommu=pt", {}));
    cmd("audit=0", "Turns the kernel audit subsystem off: less overhead on every audited syscall. Only if nothing on the "
        "system uses auditd / SELinux audit logs.", false, false, always, tokenOr("audit=0", {}));
    cmd("nvme_core.default_ps_max_latency_us=0", "Disables NVMe autonomous power states (APST): no wake-up delay on the first "
        "access after idle. Costs idle power on battery — the opposite of a battery setup.", false, false,
        [] { return QFile::exists("/sys/module/nvme_core/parameters/default_ps_max_latency_us"); },
        tokenOr("nvme_core.default_ps_max_latency_us=0", [param] { return param("nvme_core", "default_ps_max_latency_us") == "0" ? 1 : 0; }));
    cmd("init_on_alloc=0", "Stops zeroing every page and slab object at allocation (hardened kernels turn it on): measurably "
        "faster allocation-heavy work. Leftover kernel data is no longer scrubbed.", false, true, always,
        tokenOr("init_on_alloc=0", {}));
    cmd("mitigations=off", "Turns off every CPU vulnerability mitigation. A few percent more CPU performance in some workloads, "
        "but code in a browser or a game can then read other processes' memory. Not recommended on a daily machine.",
        false, true, always, tokenOr("mitigations=off", {}));

    cmd("zswap.compressor=zstd", "zstd for zswap: noticeably better compression than the lzo default, so more pages stay in RAM "
        "instead of hitting the swap device. Slightly more CPU per page.", false, false,
        [hasParam] { return hasParam("zswap", "compressor") && readAll("/proc/swaps").count('\n') >= 1; },
        tokenOr("zswap.compressor=zstd", [param] { return param("zswap", "compressor") == "zstd" ? 1 : 0; }));
    excl("zswap-comp");
    cmd("zswap.compressor=lz4", "lz4 for zswap: the fastest compressor (lowest decompression latency on a swap-in), at a worse "
        "ratio than zstd. Needs lz4 built in or in the initramfs.", false, false,
        [hasParam] { return hasParam("zswap", "compressor") && readAll("/proc/swaps").count('\n') >= 1; },
        tokenOr("zswap.compressor=lz4", [param] { return param("zswap", "compressor") == "lz4" ? 1 : 0; }));
    excl("zswap-comp");
    cmd("zswap.shrinker_enabled=1", "Lets zswap write cold compressed pages back to swap under memory pressure instead of "
        "filling up and rejecting new pages (kernel 6.8+).", false, false,
        [hasParam] { return hasParam("zswap", "shrinker_enabled"); },
        tokenOr("zswap.shrinker_enabled=1", [param] { return param("zswap", "shrinker_enabled") == "Y" ? 1 : 0; }));
    cmd("init_on_free=0", "Stops wiping pages and slab objects when they are freed (hardened kernels enable it): less work on every "
        "free. Freed kernel data lingers until reused.", false, true, always, tokenOr("init_on_free=0", {}));
    cmd("spec_rstack_overflow=off", "Only the SRSO (Inception) mitigation off instead of all of them: it is the one with the "
        "largest cost on Zen 3/4 (every return is affected), and the rest stay on. Same risk class as mitigations=off, "
        "narrower.", false, true,
        [vuln] { const QString v = vuln("spec_rstack_overflow"); return !v.isEmpty() && !v.startsWith("Not affected"); },
        tokenOr("spec_rstack_overflow=off", [vuln] { return vuln("spec_rstack_overflow").startsWith("Vulnerable") ? 1 : 0; }));

    grp = QStringLiteral("Stability");
    cmd("amdgpu.sg_display=0", "Keeps the AMD iGPU's scanout buffer in VRAM instead of system memory: the known fix for white "
        "or flickering screens on Raphael / Phoenix iGPUs (Hybrid mode).", false, false,
        [hybrid] { return hybrid; },
        tokenOr("amdgpu.sg_display=0", [param] { return param("amdgpu", "sg_display") == "0" ? 1 : 0; }));
    cmd("amdgpu.dcdebugmask=0x10", "Disables Panel Self Refresh on the AMD iGPU: fixes freezes, stale frames and artifacts on "
        "the internal panel when the desktop is idle or scrolling. Costs a little idle power.", false, false,
        [hybrid] { return hybrid; },
        tokenOr("amdgpu.dcdebugmask=0x10", [param] { const QString v = param("amdgpu", "dcdebugmask");
            bool ok = false; return (v.toUInt(&ok, 0) & 0x10) && ok ? 1 : 0; }));
    cmd("pcie_aspm=off", "No PCIe link power management at all: the fix for NVMe / Wi-Fi / GPU links that drop out or log AER "
        "errors after idle. Costs idle power on battery.", false, false,
        [] { return QFile::exists("/sys/module/pcie_aspm"); }, tokenOr("pcie_aspm=off", {}));
    excl("aspm");
    cmd("pci=noaer", "Silences PCIe Advanced Error Reporting: stops a flood of corrected-error messages (and the interrupt "
        "load that comes with it). Hides real link problems too — fix the cause first if you can.",
        false, true, always, tokenOr("pci=noaer", {}));
    cmd("processor.max_cstate=1", "Keeps idle CPUs out of deep C-states: the workaround for random idle freezes/reboots on "
        "some Ryzen systems and the lowest wake-up latency. Large idle power and heat cost on a laptop.", false, true,
        always, tokenOr("processor.max_cstate=1", {}));
    cmd("btusb.enable_autosuspend=0", "No USB autosuspend for the Bluetooth adapter: fixes controllers, headsets and audio "
        "that drop out or reconnect slowly after idle.", false, false,
        [hasParam] { return hasParam("btusb", "enable_autosuspend"); },
        tokenOr("btusb.enable_autosuspend=0", [param] { return param("btusb", "enable_autosuspend") == "N" ? 1 : 0; }));
    cmd("sysrq_always_enabled=1", "Magic SysRq always on: Alt+SysRq+R/E/I/S/U/B syncs and reboots cleanly when the desktop "
        "or GPU is hung, instead of a hard power-off.", false, false, always,
        tokenOr("sysrq_always_enabled=1", [] { return readAll("/proc/sys/kernel/sysrq") == "1" ? 1 : 0; }));
    cmd("panic=10", "Reboot 10 s after a kernel panic instead of hanging with a frozen screen (the log survives in pstore if "
        "the platform has it).", false, false, always,
        tokenOr("panic=10", [] { return readAll("/proc/sys/kernel/panic") == "10" ? 1 : 0; }));

    grp = QStringLiteral("Power saving");
    cmd("mem_sleep_default=deep", "Suspend to S3 instead of s2idle by default: far lower overnight drain, slower resume. "
        "(Devices tab: Suspend mode switches it at runtime.)", false, false,
        [] { return readAll("/sys/power/mem_sleep").contains("deep"); },
        tokenOr("mem_sleep_default=deep", [] { return readAll("/sys/power/mem_sleep").contains("[deep]") ? 1 : 0; }));
    cmd("rcu_nocbs=all rcutree.enable_rcu_lazy=1", "Lazy RCU: batches RCU callbacks and delays them by seconds instead of waking "
        "idle CPUs for each one — a measurable idle-power saving on laptops (it is what ChromeOS and Android ship). Needs a "
        "kernel built with CONFIG_RCU_LAZY.", false, false,
        [param] { return QFile::exists("/sys/module/rcutree/parameters/enable_rcu_lazy"); },
        [param] { return onCmdline("rcu_nocbs=all") && param("rcutree", "enable_rcu_lazy") == "Y" ? 1 : 0; });
    cmd("pcie_aspm.policy=powersupersave", "Boot with the deepest PCIe link power states (L1 substates) where devices allow "
        "them. The Devices tab switches the policy at runtime; this makes it the default.", false, false,
        [] { return QFile::exists("/sys/module/pcie_aspm/parameters/policy"); },
        tokenOr("pcie_aspm.policy=powersupersave", [] { return readAll("/sys/module/pcie_aspm/parameters/policy").contains("[powersupersave]") ? 1 : 0; }));
    excl("aspm");
    cmd("workqueue.power_efficient=1", "Lets per-CPU kernel work run on whichever CPU is already awake instead of waking an "
        "idle one. Saves power; can move work away from the CPU that queued it.", false, false,
        [] { return QFile::exists("/sys/module/workqueue/parameters/power_efficient"); },
        tokenOr("workqueue.power_efficient=1", [param] { return param("workqueue", "power_efficient") == "Y" ? 1 : 0; }));

    grp = QStringLiteral("Graphics & tools");
    cmd("nvidia-drm.modeset=1", "Kernel modesetting for the NVIDIA driver: required for Wayland, PRIME offload and a flicker-free "
        "boot. Default on recent drivers.", true, false, [nvidia] { return nvidia; },
        tokenOr("nvidia-drm.modeset=1", [] { return readAll("/sys/module/nvidia_drm/parameters/modeset") == "Y" ? 1 : 0; }));
    cmd("nvidia-drm.fbdev=1", "NVIDIA's own framebuffer console (driver 545+): a real-resolution VT and no simpledrm hand-over glitch.",
        true, false, [nvidia] { return nvidia; },
        tokenOr("nvidia-drm.fbdev=1", [] { return readAll("/sys/module/nvidia_drm/parameters/fbdev") == "Y" ? 1 : 0; }));
    // AMD overdrive: the current mask with bit 0x4000 added (never a fixed
    // magic value — that would also flip whatever else this kernel disabled).
    if (const auto mask = amdgpu::featureMask()) {
        const QString tok = QStringLiteral("amdgpu.ppfeaturemask=0x%1").arg(*mask | amdgpu::OVERDRIVE_BIT, 0, 16);
        const bool on = *mask & amdgpu::OVERDRIVE_BIT;
        cmd(tok, "Enables amdgpu overdrive: clock, voltage and fan control in the AMD GPU tab (overclock / undervolt). "
            "The kernel marks itself tainted while it is on.", false, false, always,
            [on] { return on ? 1 : 0; });
    }
    cmd("msr.allow_writes=on", "Lets the Intel undervolt helper write MSRs without the kernel logging a warning on every write.",
        true, false, [intel] { return intel; }, tokenOr("msr.allow_writes=on", [] { return readAll("/sys/module/msr/parameters/allow_writes") == "on" ? 1 : 0; }));

    // ── NVIDIA module options (/etc/modprobe.d) ──
    auto nv = [](QString key, QString want) { return [key, want] { const QString v = nvParam(key); return v.isEmpty() ? -1 : v == want ? 1 : 0; }; };
    mod("nvidia", "NVreg_PreserveVideoMemoryAllocations=1", "Saves VRAM across suspend (with the nvidia-suspend / elogind "
        "hooks): no corrupted desktop or crashed games after resume.", true, false, [nvidia] { return nvidia; },
        nv("PreserveVideoMemoryAllocations", "1"));
    mod("nvidia", "NVreg_TemporaryFilePath=/var/tmp", "Where that VRAM copy goes. /tmp is often a small tmpfs, which makes "
        "suspend fail with a full GPU.", true, false, [nvidia] { return nvidia; },
        [] { const QString v = nvParam("TemporaryFilePath"); return v.isEmpty() ? -1 : v.contains("/var/tmp") ? 1 : 0; });
    mod("nvidia", "NVreg_EnableS0ixPowerManagement=1", "Lets the GPU enter its lowest state during s2idle suspend.", true, false,
        [nvidia] { return nvidia && readAll("/sys/power/mem_sleep").contains("[s2idle]"); }, nv("EnableS0ixPowerManagement", "1"));
    mod("nvidia", "NVreg_DynamicPowerManagement=0x02", "Fine-grained runtime power management: the NVIDIA GPU powers off "
        "completely when idle. Only matters in Hybrid mode (in dGPU-only mode it drives the display).", true, false,
        [nvidia, hybrid] { return nvidia && hybrid; }, nv("DynamicPowerManagement", "2"));
    mod("nvidia", "NVreg_UsePageAttributeTable=1", "Uses PAT for write-combined memory mappings: slightly faster CPU→GPU uploads.",
        true, false, [nvidia] { return nvidia; }, nv("UsePageAttributeTable", "1"));
    mod("nvidia", "NVreg_EnableResizableBar=1", "Lets the driver resize BAR1 so the CPU can map all of VRAM (Resizable BAR; "
        "off by default on Linux). Check with nvidia-smi -q | grep -A3 BAR1 — Total should equal the VRAM size. Needs "
        "Above-4G decoding / ReBAR in the firmware.", false, false, [nvidia] { return nvidia; }, nv("EnableResizableBar", "1"));
    mod("nvidia", "NVreg_EnableGpuFirmware=0", "Runs the driver on the CPU instead of the GSP firmware (proprietary module "
        "only): the known cure for periodic frame-time spikes on some 555+ drivers. The open modules ignore it.",
        false, false, [nvidia, nvOpen] { return nvidia && !nvOpen; }, nv("EnableGpuFirmware", "0"));
    mod("nvidia", "NVreg_RegistryDwords=\"RMUseSwI2c=0x01;RMI2cSpeed=100\"", "Software I²C at 100 kHz: makes DDC/CI "
        "(ddcutil — external monitor brightness/input) reliable on NVIDIA outputs.", false, false,
        [nvidia] { return nvidia; }, [] { return -1; });
    mod("nvidia", "NVreg_InitializeSystemMemoryAllocations=0", "Skips zeroing system memory the driver allocates: faster "
        "allocations, but freed data from other processes could show up in GPU buffers.", false, true,
        [nvidia] { return nvidia; }, nv("InitializeSystemMemoryAllocations", "0"));

    // ── layout ──
    auto *outer = new QVBoxLayout(this);
    outer->setContentsMargins(0, 0, 0, 0);
    auto *scroll = new QScrollArea;
    scroll->setWidgetResizable(true);
    auto *page = new QWidget;
    auto *v = new QVBoxLayout(page);
    v->setContentsMargins(8, 8, 8, 8);
    v->setSpacing(8);

    cmdline_ = muted(QString());
    cmdline_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    v->addWidget(cmdline_);

    // Same column widths in every section, so names / states line up across boxes.
    QList<QLabel *> nameLabels, statusLabels;
    auto section = [&](const QString &title, const QString &group) {
        auto *box = new QGroupBox(title);
        auto *g = new QGridLayout(box);
        g->setHorizontalSpacing(10);
        g->setVerticalSpacing(4);
        g->setColumnStretch(3, 1);
        int row = 0;
        for (Item &it : items_) {
            if (it.group != group || !it.applicable()) continue;
            it.box = new QCheckBox;
            auto *name = new QLabel(it.text);
            name->setStyleSheet(QStringLiteral("font-family: monospace; color: %1;").arg(it.caution ? theme::WARN : theme::FG));
            nameLabels << name;
            it.status = new QLabel;
            statusLabels << it.status;
            auto *why = muted(it.why);
            g->addWidget(it.box, row, 0, Qt::AlignTop);
            g->addWidget(name, row, 1, Qt::AlignTop);
            g->addWidget(it.status, row, 2, Qt::AlignTop);
            g->addWidget(why, row++, 3);
            connect(it.box, &QCheckBox::toggled, this, [this, key = it.excl, self = it.box](bool on) {
                if (on && !key.isEmpty())
                    for (Item &o : items_)
                        if (o.box && o.box != self && o.excl == key) o.box->setChecked(false);
                updateOutput();
            });
        }
        if (row) v->addWidget(box); else delete box;
    };
    section("Kernel command line — latency", "Latency");
    section("Kernel command line — performance", "Performance");
    section("Kernel command line — stability && recovery", "Stability");
    section("Kernel command line — power saving", "Power saving");
    section("Kernel command line — graphics && tools", "Graphics & tools");
    section("NVIDIA module options  (/etc/modprobe.d)", "NVIDIA");
    int nameW = 0;
    for (QLabel *l : nameLabels) nameW = std::max(nameW, l->sizeHint().width());
    for (QLabel *l : nameLabels) l->setMinimumWidth(nameW);
    const int statusW = fontMetrics().horizontalAdvance(QStringLiteral("✓ active  ")) + 8;
    for (QLabel *l : statusLabels) l->setFixedWidth(statusW);

    auto out = [&](const QString &title, QPlainTextEdit *&edit, const QString &howto) {
        auto *box = new QGroupBox(title);
        auto *l = new QVBoxLayout(box);
        edit = new QPlainTextEdit;
        edit->setReadOnly(true);
        edit->setStyleSheet("font-family: monospace;");
        edit->setMaximumHeight(64);
        auto *row = new QHBoxLayout;
        row->addWidget(muted(howto), 1);
        auto *copy = new QPushButton("Copy");
        connect(copy, &QPushButton::clicked, this, [edit] { QApplication::clipboard()->setText(edit->toPlainText()); });
        row->addWidget(copy, 0, Qt::AlignTop);
        l->addWidget(edit);
        l->addLayout(row);
        v->addWidget(box);
    };
    out("Add to the kernel command line", cmdOut_,
        "GRUB: append to GRUB_CMDLINE_LINUX_DEFAULT in /etc/default/grub, then grub-mkconfig -o /boot/grub/grub.cfg.  "
        "systemd-boot / UKI (installkernel, dracut): /etc/kernel/cmdline, then reinstall the kernel.  "
        "Built-in: CONFIG_CMDLINE.  Only options not already active are listed.");
    out("/etc/modprobe.d/nvidia-lpm.conf", modOut_,
        "Save as that file (root). If the NVIDIA modules are in your initramfs (dracut), regenerate it; takes effect at the next boot.");
    v->addStretch(1);
    scroll->setWidget(page);
    outer->addWidget(scroll);
    refresh();
}

void BootAdvisor::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    refresh();
}

void BootAdvisor::refresh() {
    reloadCmdline();
    cmdline_->setText("Current command line:  " + g_cmdline.join(' '));
    for (Item &it : items_) {
        if (!it.box) continue;
        const int st = it.state();
        it.status->setText(st == 1 ? QStringLiteral("✓ active") : st == 0 ? QStringLiteral("not set") : QStringLiteral("unknown"));
        it.status->setStyleSheet(QStringLiteral("color: %1;").arg(st == 1 ? theme::OK : st == 0 ? theme::MUTED : theme::WARN));
        const QSignalBlocker b(it.box);
        it.box->setEnabled(st != 1);
        it.box->setChecked(st != 1 && it.suggest);
        it.box->setToolTip(st == 1 ? "Already in effect" : "Include in the generated text below");
    }
    updateOutput();
}

void BootAdvisor::updateOutput() {
    QStringList cmd;
    QMap<QString, QStringList> mods;
    for (const Item &it : items_) {
        if (!it.box || !it.box->isChecked() || !it.box->isEnabled()) continue;
        if (it.modprobe) mods[it.module] << it.text;
        else for (const QString &t : it.text.split(' ', Qt::SkipEmptyParts)) if (!cmd.contains(t)) cmd << t;
    }
    cmdOut_->setPlainText(cmd.isEmpty() ? QStringLiteral("# nothing to add") : cmd.join(' '));
    QString m = QStringLiteral("# Generated by Legion Power Manager (Optimizations → Boot options)\n");
    for (auto it = mods.cbegin(); it != mods.cend(); ++it) m += "options " + it.key() + ' ' + it.value().join(' ') + '\n';
    modOut_->setPlainText(mods.isEmpty() ? QStringLiteral("# nothing to add") : m.trimmed());
}
