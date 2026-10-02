//! System tuning allowlist shared by `tune-helper` (root), `lpm-gamemode` and
//! (through `describe`) the Optimizations tab.
//!
//! Every knob is a row in [`TUNABLES`]: a fixed key, a fixed set of sysfs /
//! procfs targets (or a fixed discovery rule under /sys or /proc/irq), and a
//! value domain re-read from the kernel at write time. Nothing path-like ever
//! comes from a request; requests only name keys and values.
//!
//! Semantics follow lutris-game-tune: the first write to a concrete file
//! records its original value in the baseline (write order), "restore" writes
//! the baseline back. Table order is apply order: amd_pstate/status comes
//! first (it resets the per-policy files), CPU hot-plug comes last (an offline
//! CPU's cpufreq policy rejects writes) and is restored first.
//!
//! Rows carry a [`Vendor`]: AMD-only rows (amd-pstate, CCD/X3D, amdgpu) and
//! Intel-only rows (intel_pstate, hybrid P/E-core, uncore, RAPL, TCC, i915/xe)
//! are hidden from `describe` and report "not available" on the other vendor,
//! so one preset file stays portable across both.

use crate::{canonical_in_sysfs, read_trimmed, sysfs_write};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const CPU_DIR: &str = "/sys/devices/system/cpu";
pub const X3D_DRIVER_DIR: &str = "/sys/bus/platform/drivers/amd_x3d_vcache";
pub const DEBUGFS: &str = "/sys/kernel/debug";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor { Any, Amd, Intel }

impl Vendor {
    pub fn as_str(self) -> &'static str {
        match self { Vendor::Any => "other", Vendor::Amd => "amd", Vendor::Intel => "intel" }
    }
}

/// vendor_id of the first CPU in /proc/cpuinfo.
pub fn vendor_from_cpuinfo(s: &str) -> Vendor {
    for l in s.lines() {
        let Some((k, v)) = l.split_once(':') else { continue };
        if k.trim() != "vendor_id" { continue; }
        return match v.trim() { "GenuineIntel" => Vendor::Intel, "AuthenticAMD" | "HygonGenuine" => Vendor::Amd, _ => Vendor::Any };
    }
    Vendor::Any
}

/// The running CPU's vendor (read once). `Any` if unknown.
pub fn cpu_vendor() -> Vendor {
    static V: std::sync::OnceLock<Vendor> = std::sync::OnceLock::new();
    *V.get_or_init(|| vendor_from_cpuinfo(crate::cpuinfo_head()))
}

pub fn vendor_ok(t: &Tunable) -> bool { t.vendor == Vendor::Any || t.vendor == cpu_vendor() }

/// Hybrid (Alder Lake and later) core class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoreType { P, E }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Discrete strings; the live option list comes from [`Options`].
    Choice,
    /// Integer in [min, max] (files reporting 0x.. hex are parsed too).
    Int { min: i64, max: i64 },
    /// "1"/"0" in presets; written as Y/N or 1/0, matching what the file reports.
    Bool,
}

#[derive(Clone, Copy)]
pub enum Options {
    None,
    Fixed(&'static [&'static str]),
    /// Space-separated list in this file (relative to policy0).
    ListFile(&'static str),
    /// Options embedded in the value file: "always [madvise] never".
    Bracketed,
    /// Space-separated list in this absolute file (e.g. cpuidle/available_governors).
    ListAt(&'static str),
    /// Computed from hardware (min freq, C-states, CCD roles…).
    Special,
}

#[derive(Clone, Copy)]
pub enum Target {
    File(&'static str),
    /// Same file name in every cpufreq/policy*.
    PerPolicy(&'static str),
    /// Same file in the cpufreq policies covering one CCD (index into ccx_groups).
    PerCcdPolicy(&'static str, usize),
    /// Per-policy `boost` (6.11+), else global cpufreq/boost, else
    /// intel_pstate/no_turbo (inverted: the preset value stays "1 = turbo on").
    Boost,
    /// Same file in the cpufreq policies of one hybrid core class (Intel P/E).
    PerCoreType(&'static str, CoreType),
    /// Same file under every cpuN/ (e.g. power/energy_perf_bias).
    PerCpu(&'static str),
    /// intel_uncore_frequency/<domain>/<file> for every uncore domain.
    Uncore(&'static str),
    /// RAPL package-domain power limit in watts for the named constraint
    /// (long_term / short_term), on both the MSR and the MMIO interface.
    RaplWatts(&'static str),
    /// cur_state of the "TCC Offset" thermal cooling device (intel_tcc_cooling).
    TccOffset,
    /// Intel iGPU GT attribute: i915 card*/gt/gt*/<i915>, xe card*/device/tile*/gt*/freq0/<xe>.
    IntelGt { i915: &'static str, xe: &'static str },
    /// amd_x3d_vcache/<instance>/amd_x3d_mode, instance discovered at runtime.
    X3d,
    /// Per-policy scaling_min_freq from a sibling frequency file.
    MinFreq,
    /// Per-policy amd_pstate_floor_freq (kernel 7.1+, CPPC Performance Priority): the
    /// frequency firmware throttles to first when power/thermal limits bite. Same
    /// values as MinFreq (lowest_nonlinear / cpuinfo_min).
    FloorFreq,
    /// One scheduler feature bit in debugfs sched/features: written as NAME or NO_NAME.
    SchedFeature(&'static str),
    /// cpu*/cpuidle/state*/disable: keep states <= N enabled.
    CState,
    /// Same queue attribute on every whole disk (nvme, sd, mmcblk, vd).
    PerBlock(&'static str),
    /// machinecheck*/check_interval.
    Mce,
    /// power_dpm_force_performance_level of every amdgpu (vendor 0x1002) device.
    AmdgpuDpm,
    /// PCI latency-timer byte (config offset 0x0D) of every PCI function.
    PciLatency,
    /// Unbound workqueue cpumask; value = CCD role.
    WqCpumask,
    /// /proc/irq/N/smp_affinity_list for every IRQ; value = CCD role (best effort).
    Irq,
    /// cpu*/online of one CCD (never cpu0's).
    CcdPark,
    /// 802.11 power save of every wireless interface, through `iw` (no sysfs knob).
    WifiPowerSave,
    /// Per-link PCIe ASPM overrides: link/l1_aspm (+ l1_1_aspm, l1_2_aspm) of every PCI function.
    PciAspm,
    /// sched_ext BPF scheduler: starts / stops an scx_* binary (value = its name or "none").
    SchedExt,
    /// The first of these files that exists (a knob that moved between
    /// kernel versions, e.g. sched_itmt_enabled: /proc/sys -> debugfs in 6.14).
    AnyFile(&'static [&'static str]),
    /// power/control (runtime PM) of every PCI function, except display
    /// devices, their sibling functions and the bridges above them, and
    /// drivers that manage runtime PM themselves (see RPM_DRIVER_DENY).
    PciRuntimePm,
    /// power/control of every USB device except HID and audio devices.
    UsbRuntimePm,
    /// Same attribute on every SCSI/ATA host (SATA link power management).
    ScsiHost(&'static str),
    /// power/pm_qos_latency_tolerance_us of every NVMe controller (APST ceiling).
    NvmeLatency,
    /// amdgpu panel power savings (ABM) level of every eDP connector.
    AmdgpuAbm,
    /// Wake-on-LAN of every physical Ethernet port, through `ethtool` (no sysfs knob).
    EthWol,
    /// Energy-Efficient Ethernet of every physical Ethernet port, through `ethtool --set-eee`.
    EthEee,
    /// `soft` block switch of every rfkill device of one type (bluetooth / wlan / wwan).
    /// Inverted for the user: 1 = radio enabled.
    Rfkill(&'static str),
    /// power/autosuspend_delay_ms of the USB devices `UsbRuntimePm` may touch.
    UsbAutosuspendMs,
    /// power/control of every AHCI port (`<pci device>/ata*`), TLP's AHCI_RUNTIME_PM for ports.
    AhciPortRuntime,
    /// `device/power/<attr>` of every ATA disk that supports runtime PM (control / autosuspend_delay_ms).
    AhciDisk(&'static str),
    /// ATA APM level (hdparm -B) of the Nth APM-capable ATA disk (sorted by serial, so
    /// the slot follows the drive, not sdX). One slot per disk = per-disk levels.
    DiskApm(usize),
    /// vm.dirty_bytes / dirty_background_bytes. Writing bytes zeroes the ratio
    /// twin, and 0 is not a valid bytes value, so while the system is in ratio
    /// mode the baseline records "ratio:<n>" and restore writes the ratio file.
    DirtyBytes { bytes: &'static str, ratio: &'static str },
}

pub struct Tunable {
    pub key: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub options: Options,
    pub target: Target,
    /// Lives under debugfs (needs it mounted; root-only readable).
    pub debugfs: bool,
    /// Can visibly hurt stability, thermals or idle power; the GUI marks it.
    pub caution: bool,
    /// Only offered on this CPU vendor.
    pub vendor: Vendor,
}

const fn t(key: &'static str, group: &'static str, label: &'static str, help: &'static str,
           kind: Kind, options: Options, target: Target) -> Tunable {
    Tunable { key, group, label, help, kind, options, target, debugfs: false, caution: false, vendor: Vendor::Any }
}
const fn int(min: i64, max: i64) -> Kind { Kind::Int { min, max } }
const fn dbg(mut x: Tunable) -> Tunable { x.debugfs = true; x }
const fn warn(mut x: Tunable) -> Tunable { x.caution = true; x }
const fn amd(mut x: Tunable) -> Tunable { x.vendor = Vendor::Amd; x }
const fn intel(mut x: Tunable) -> Tunable { x.vendor = Vendor::Intel; x }
const BR: Options = Options::Bracketed;
const NO: Options = Options::None;

/// Keys that hot-plug CPUs: applied after every other knob, restored before them.
pub const HOTPLUG_KEYS: &[&str] = &["cpu.smt", "cpu.ccd_park"];
/// After each cpu*/online or smt/control write.
pub const HOTPLUG_SETTLE_WRITE: std::time::Duration = std::time::Duration::from_millis(5);
/// After a whole hot-plug knob, before sysfs is read again.
pub const HOTPLUG_SETTLE: std::time::Duration = std::time::Duration::from_millis(50);
/// Between two ordinary knobs.
pub const KNOB_GAP: std::time::Duration = std::time::Duration::from_millis(2);

pub const TUNABLES: &[Tunable] = &[
    // ── CPU ───────────────────────────────────────────────────────────────
    amd(t("cpu.pstate_status", "CPU", "amd-pstate mode",
      "active = EPP/CPPC decides frequency (recommended on Zen 2+); guided = kernel sets a floor, firmware the rest; passive = legacy governor control. Leave on active unless you have a specific reason: guided/passive exist mainly for older firmware or debugging odd boost behaviour. Changing it resets every per-policy governor/EPP value underneath, which is why this row is always applied first and restored last of the non-hot-plug rows.",
      Kind::Choice, Options::Fixed(&["active", "guided", "passive"]), Target::File("/sys/devices/system/cpu/amd_pstate/status"))),
    intel(t("cpu.intel_pstate_status", "CPU", "intel_pstate mode",
      "active = the CPU's own HWP (Speed Shift) logic picks the frequency and the governor/EPP rows below are hints to it - the right mode on every HWP-capable Intel CPU (6th gen and later). passive = intel_pstate becomes a plain cpufreq driver ('intel_cpufreq') driven by a kernel governor such as schedutil: useful only for comparing governors or on firmware with broken HWP. 'off' is deliberately not offered (it unloads frequency control entirely). Changing it resets every per-policy governor/EPP/limit underneath, so it is applied before any of them.",
      Kind::Choice, Options::Fixed(&["active", "passive"]), Target::File("/sys/devices/system/cpu/intel_pstate/status"))),
    amd(t("cpu.dynamic_epp", "CPU", "Dynamic EPP (amd-pstate)",
      "Kernel 7.x amd-pstate feature: switches every policy's EPP automatically with the power source (AC = performance-leaning, battery = power-leaning). While enabled the kernel owns EPP, so the global and per-CCD EPP rows below are refused (EBUSY) or overridden on the next AC/battery change - applied right after the pstate mode so 'disabled' lands before any EPP row; disable it when you tune EPP by hand or per game.",
      Kind::Choice, Options::Fixed(&["enabled", "disabled"]), Target::File("/sys/devices/system/cpu/amd_pstate/dynamic_epp"))),
    t("cpu.governor", "CPU", "Scaling governor",
      "In amd-pstate active mode this mostly gates EPP: 'performance' pins EPP to 0 regardless of the EPP row below, 'powersave' lets EPP decide. For gaming keep 'powersave' here and control behaviour with EPP instead - EPP has finer steps (5 levels vs 2) and swaps faster. Set 'performance' only for a fixed worst-case floor (e.g. Competitive preset) or on kernels/firmware where EPP is ignored. On a 2+ CCD chip this row is hidden - set Governor · CCD0 and · CCD1 instead (to the same value, if you want one governor for the whole chip); a single-CCD chip has no CCD rows, so this is the only place to set it.",
      Kind::Choice, Options::ListFile("scaling_available_governors"), Target::PerPolicy("scaling_governor")),
    t("cpu.epp", "CPU", "Energy-performance preference",
      "EPP hint to CPPC firmware (active mode), five steps from most aggressive to most efficient. Needs governor 'powersave' to take effect. Pick by scenario: competitive/latency-sensitive -> performance; general gaming on AC -> balance_performance (usually indistinguishable in fps, noticeably cooler/quieter); on battery or light desktop work -> balance_power; power -> power-priority, expect lower sustained clocks. If frame times feel spiky right after a load change, try balance_performance before touching anything else - that spikiness is often EPP being too cautious to boost. On a 2+ CCD chip this row is hidden - set EPP · CCD0 and · CCD1 instead; a single-CCD chip has no CCD rows, so this is the only place to set it.",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerPolicy("energy_performance_preference")),
    amd(t("cpu.epp_boost", "CPU", "amd-pstate epp_boost",
      "Per-core EPP boost (amd_pstate.epp_boost, global; the patch series has no per-policy knob). Only on kernels with the (not upstream) epp_boost patch; the row shows n/a otherwise. While it is on, the driver samples every core's busy share (at most every 10 ms) and gives a core that is at least half busy the performance EPP for as long as it stays busy, whatever EPP its policy has; idle and lightly used cores keep the policy's efficient EPP. That is what a game's main thread needs when the package power is shared with the GPU: better 1 % lows without raising the whole package. Autotune turns it on for the Gaming and Throughput goals as a fixed part of the goal (no benchmark overrides it) and the Gaming / Competitive / Compile throughput presets carry it; off for power saving.",
      Kind::Bool, NO, Target::File("/sys/module/amd_pstate/parameters/epp_boost"))),
    t("cpu.boost", "CPU", "Core performance boost",
      "Turbo (core performance boost). Leave it on for normal use - this is not a fps knob, it only removes the clock ceiling above base. On Intel this drives intel_pstate/no_turbo (inverted, so 1 still means turbo on). Turn it off for two specific jobs: (1) thermal/fan-curve testing where you want repeatable numbers, (2) validating undervolt/Curve Optimizer offsets, since boost clocks typically first expose an unstable core (use the CO validation preset, which also widens C-states and shortens the MCE poll).",
      Kind::Bool, NO, Target::Boost),
    t("cpu.min_freq", "CPU", "Minimum frequency",
      "(Only 'cpuinfo_min' is offered on Intel: intel_pstate has no lowest-nonlinear file and HWP already avoids the inefficient range on its own.) lowest_nonlinear raises the CPU's idle floor to amd_pstate_lowest_nonlinear_freq (typically 400-600 MHz above the hardware minimum): frequencies below that point are inefficient on Zen, disproportionate wake-up latency for negligible power savings. Safe to enable for every scenario, including battery; the lowest-risk, no-downside row on this whole tab.",
      Kind::Choice, Options::Special, Target::MinFreq),
    amd(t("cpu.floor_freq", "CPU", "Floor frequency (power-limit throttle target)",
      "Kernel 7.1+ on CPUs with AMD 'CPPC Performance Priority' (row shows n/a everywhere else, including Zen 5 mobile as of now). When a package power or thermal limit forces the platform to throttle, firmware first drops the core to this floor before going lower still. The kernel default is the nominal frequency, which is what you want while gaming or compiling on a power-limited laptop (sustained clocks stay high, only the boost above them is shed). cpuinfo_min lets a power-saving profile throttle all the way down; lowest_nonlinear keeps the efficient region. Threads of one core should share one value (SMT siblings are written together).",
      Kind::Choice, Options::Special, Target::FloorFreq)),
    // Per-CCD overrides: applied after the global rows above, so a preset can
    // set everything and then split the dies (e.g. V-Cache die performance,
    // frequency die balance_power while it only hosts IRQs and background work).
    amd(t("cpu.governor_ccd0", "CPU", "Governor · CCD0",
      "Scaling governor for CCD0's CPUs (the global 'Scaling governor' row is hidden on 2+ CCD chips - this and the CCD1 row are how you set it). Set both CCD rows the same for one governor across the whole chip, or split them for asymmetric behaviour: powersave on the CCD whose EPP row you are actually using, since 'performance' here pins EPP and makes the EPP row moot.",
      Kind::Choice, Options::ListFile("scaling_available_governors"), Target::PerCcdPolicy("scaling_governor", 0))),
    amd(t("cpu.governor_ccd1", "CPU", "Governor · CCD1",
      "Scaling governor for CCD1's CPUs (the global row is hidden on 2+ CCD chips). See the CCD0 row for how to set this.",
      Kind::Choice, Options::ListFile("scaling_available_governors"), Target::PerCcdPolicy("scaling_governor", 1))),
    amd(t("cpu.epp_ccd0", "CPU", "EPP · CCD0",
      "EPP for CCD0's CPUs (the global EPP row is hidden on 2+ CCD chips - this and the CCD1 row are how you set it). Needs governor 'powersave' on CCD0 (see the Governor · CCD0 row) - 'performance' pins EPP and this row becomes moot. Concrete split for a V-Cache part: CCD0 = V-Cache -> performance here (the game runs there); CCD1 = frequency die -> balance_power on its own row, since it is mostly idle plus background/IRQ work during a game. That is exactly what the Gaming X3D and Competitive presets set (wq/irq affinity also point at CCD1).",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerCcdPolicy("energy_performance_preference", 0))),
    amd(t("cpu.epp_ccd1", "CPU", "EPP · CCD1",
      "EPP for CCD1's CPUs (the global row is hidden on 2+ CCD chips). Needs governor 'powersave' on CCD1 (Governor · CCD1 row). See the CCD0 row for the usual split.",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerCcdPolicy("energy_performance_preference", 1))),
    amd(t("cpu.boost_ccd0", "CPU", "Boost · CCD0",
      "Turbo for CCD0's CPUs only (needs kernel 6.11+ with per-policy boost; shows n/a otherwise, use the global Boost row instead). Use this to trade one die's headroom for the other's: turn boost off on the idle/background CCD so its heat and power budget go to the CCD doing the work.",
      Kind::Bool, NO, Target::PerCcdPolicy("boost", 0))),
    amd(t("cpu.boost_ccd1", "CPU", "Boost · CCD1",
      "Turbo for CCD1's CPUs only (needs kernel 6.11+ with per-policy boost). See the CCD0 row for why you would split this.",
      Kind::Bool, NO, Target::PerCcdPolicy("boost", 1))),
    amd(t("cpu.max_freq_ccd0", "CPU", "Max frequency · CCD0 (kHz)",
      "Hard frequency ceiling (kHz) for CCD0's CPUs, independent of boost/EPP. The kernel clamps whatever you enter to the hardware's real range, so an oversized value is harmless - it just becomes the hardware max. Two uses: (1) cap the non-gaming CCD low (e.g. 3500000 = 3.5 GHz) to keep it cool and quiet while it only handles background work; (2) cap a whole CCD during thermal testing instead of disabling boost outright, for a repeatable non-zero ceiling. Leave unchecked for normal gaming.",
      int(400_000, 7_000_000), NO, Target::PerCcdPolicy("scaling_max_freq", 0))),
    amd(t("cpu.max_freq_ccd1", "CPU", "Max frequency · CCD1 (kHz)",
      "Hard frequency ceiling (kHz) for CCD1's CPUs. See the CCD0 row for the two common uses (capping the idle CCD, or repeatable thermal tests).",
      int(400_000, 7_000_000), NO, Target::PerCcdPolicy("scaling_max_freq", 1))),
    // ── Intel (intel_pstate / hybrid / uncore / RAPL / TCC) ───────────────
    // After the global governor/EPP/boost rows, so the P/E-core overrides win.
    intel(t("cpu.hwp_dynamic_boost", "CPU", "HWP dynamic boost",
      "intel_pstate raises the HWP minimum for a moment when a task wakes up after waiting on I/O, so a thread that just got its data back does not start at the idle clock. 1 = on: lower wake-up latency for bursty, I/O-bound work (asset streaming, shader compiles) at a small idle-power cost; the Intel gaming presets turn it on. 0 = the kernel default. Only takes effect in active mode with HWP.",
      Kind::Bool, NO, Target::File("/sys/devices/system/cpu/intel_pstate/hwp_dynamic_boost"))),
    intel(t("cpu.max_perf_pct", "CPU", "Max performance (%)",
      "Global intel_pstate ceiling as a percentage of the highest turbo P-state, applied on top of every policy's scaling_max_freq. 100 = no cap. A lower value (e.g. 70-80) is the simplest way to take the top, least efficient turbo bins away on battery or for a quiet profile without disabling turbo outright. Applied before the minimum row; the kernel refuses a max below the current min.",
      int(1, 100), NO, Target::File("/sys/devices/system/cpu/intel_pstate/max_perf_pct"))),
    intel(t("cpu.min_perf_pct", "CPU", "Min performance (%)",
      "Global intel_pstate floor as a percentage of the highest P-state (stock is the hardware minimum, usually 15-20%). Raising it keeps clocks up between bursts - a blunt latency tool that costs idle power and heat on every core; prefer EPP · P-cores = performance first. Cannot exceed Max performance.",
      int(1, 100), NO, Target::File("/sys/devices/system/cpu/intel_pstate/min_perf_pct"))),
    intel(t("cpu.epb", "CPU", "Energy-performance bias (EPB)",
      "Legacy IA32_ENERGY_PERF_BIAS per CPU, 0 (performance) … 15 (power saving); 6 is the normal default. With HWP active EPP is what matters and EPB mostly steers uncore/package decisions. Shows n/a when the kernel does not expose power/energy_perf_bias.",
      int(0, 15), NO, Target::PerCpu("power/energy_perf_bias"))),
    intel(t("cpu.epp_pcore", "CPU", "EPP · P-cores",
      "EPP for the performance cores only (/sys/devices/cpu_core/cpus). The game's main threads live here: performance for competitive play, balance_performance for everything else. Applied after the global EPP row, so a preset can set everything to balance_performance and then push only the P-cores. Needs governor 'powersave' (active mode).",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerCoreType("energy_performance_preference", CoreType::P))),
    intel(t("cpu.epp_ecore", "CPU", "EPP · E-cores",
      "EPP for the efficient cores only (/sys/devices/cpu_atom/cpus). During a game they mostly run background work, IRQs and helper threads: balance_power keeps them from eating the package power budget the P-cores and the dGPU could use, which on a laptop often raises the P-cores' sustained clock.",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerCoreType("energy_performance_preference", CoreType::E))),
    intel(t("cpu.max_freq_pcore", "CPU", "Max frequency · P-cores (kHz)",
      "Frequency ceiling for the P-cores, independent of turbo/EPP; the kernel clamps to the real range. Use it to cut the last turbo bins (a big share of the voltage and heat) while keeping all-core clocks, e.g. 4800000 on a 5.3 GHz part.",
      int(400_000, 7_000_000), NO, Target::PerCoreType("scaling_max_freq", CoreType::P))),
    intel(t("cpu.max_freq_ecore", "CPU", "Max frequency · E-cores (kHz)",
      "Frequency ceiling for the E-cores. Capping them (e.g. 3000000) while a game runs on the P-cores frees package power and thermal headroom for the P-cores at almost no cost to background work.",
      int(400_000, 7_000_000), NO, Target::PerCoreType("scaling_max_freq", CoreType::E))),
    intel(t("cpu.uncore_max_khz", "CPU", "Uncore max frequency (kHz)",
      "Ceiling of the uncore (ring/fabric, L3, memory path) from intel_uncore_frequency. Lowering it saves several watts of package power on battery at the cost of memory latency and L3 bandwidth - bad for games, fine for desktop work. Applied before the minimum row.",
      int(400_000, 8_000_000), NO, Target::Uncore("max_freq_khz"))),
    intel(t("cpu.uncore_min_khz", "CPU", "Uncore min frequency (kHz)",
      "Floor of the uncore clock. Raising it (up to the max) removes the ramp-up delay of the ring/memory path after idle - a small but measurable win in frame-time consistency for latency-sensitive games, paid for with idle package power. Leave at the stock minimum on battery.",
      int(400_000, 8_000_000), NO, Target::Uncore("min_freq_khz"))),
    warn(intel(t("cpu.rapl_pl1", "CPU", "RAPL PL1 · long term (W)",
      "Package sustained power limit, written to both RAPL interfaces (MSR intel-rapl and MMIO intel-rapl-mmio). On a Legion the EC also programs PL1 through the firmware attribute 'ppt_pl1_spl' and re-programs it on every platform-profile change, so prefer the Firmware Attributes tab (Custom profile) and use this row for testing or on machines without that attribute. Lower = cooler and quieter, higher = more sustained all-core clock if cooling allows.",
      int(5, 400), NO, Target::RaplWatts("long_term")))),
    warn(intel(t("cpu.rapl_pl2", "CPU", "RAPL PL2 · short term (W)",
      "Package short-term (turbo burst) power limit on both RAPL interfaces. Same caveat as PL1: the EC's 'ppt_pl2_sppt' firmware attribute rewrites it on a profile change. Keep it >= PL1.",
      int(5, 400), NO, Target::RaplWatts("short_term")))),
    intel(t("cpu.tcc_offset", "CPU", "TCC offset (°C below TjMax)",
      "Thermal Control Circuit activation offset (intel_tcc_cooling): the CPU starts throttling this many degrees below TjMax (e.g. 105 °C - 10 = 95 °C). Keeps a laptop's surface and fans calmer under sustained load and trims the hottest, least efficient turbo; 0 = stock. Not an undervolt: voltage is unchanged, the ceiling just arrives earlier.",
      int(0, 63), NO, Target::TccOffset)),
    amd(t("cpu.x3d_mode", "CPU", "3D V-Cache CCD preference",
      "amd_x3d_vcache driver hint (kernel 6.13+, X3D chips only) telling the scheduler which CCD to prefer for new threads. cache = V-Cache CCD first: right for almost every game, since large working sets (open-world titles, simulation-heavy games, emulators) benefit most from the extra L3. frequency = the higher-clocked CCD: better for single-threaded or clock-sensitive work (compiling, older/less cache-hungry engines, clock-bound benchmarks). Combine with the launch affinity in the Game launch tab to actually pin the game process, not just hint the scheduler. Written with a 3 s timeout: some BIOS/AGESA versions stall in the ACPI call; a timeout is reported as a failure for this row but does not block the rest of Apply.",
      Kind::Choice, Options::Fixed(&["frequency", "cache"]), Target::X3d)),
    t("cpu.idle_governor", "CPU", "cpuidle governor",
      "Which algorithm picks the C-state for an idle CPU. menu = the long-time default, predicts idle length from recent history and is tuned for older, deeper C-state tables. teo (timer events oriented) = looks at when the next timer is due and how often recent predictions were wrong; on modern CPUs with few C-states (Zen: C1/C2/C3) it usually picks the right state more often - fewer too-deep entries under light load (less wake latency) and fewer too-shallow ones at idle (less power). Takes effect immediately, safe to switch back.",
      Kind::Choice, Options::ListAt("/sys/devices/system/cpu/cpuidle/available_governors"), Target::File("/sys/devices/system/cpu/cpuidle/current_governor")),
    warn(t("cpu.cstate_max", "CPU", "Deepest C-state kept",
      "Disables every C-state deeper than the one you pick, on every CPU. Two reasons to touch this: (1) input-latency chasing - deep C-states add microseconds of wake-up jitter on the way back to full clock, so capping to a shallower state trims worst-case latency at the cost of idle power and heat; (2) Curve Optimizer validation - the transition out of a deep idle state back to boost clock is exactly where a marginal core first crashes, so capping C-states while dialing in offsets (paired with a short MCE poll interval) surfaces instability faster than gaming normally would. Day-to-day/battery use: leave at 'all enabled'. Meant to be temporary, not a permanent setting.",
      Kind::Choice, Options::Special, Target::CState)),
    t("cpu.wake_latency_us", "CPU", "CPU wake-up latency limit (µs)",
      "Per-CPU PM QoS resume-latency constraint (cpuN/power/pm_qos_resume_latency_us). Both cpuidle governors (menu and teo) skip every idle state whose exit latency is above this value, so it is the fine-grained, reversible sibling of 'Deepest C-state kept': instead of naming a state you give the wake-up time you can accept, and the kernel keeps every state that is fast enough. 0 = no constraint (the kernel default - every C-state allowed). Example on Zen: C1 exits in ~1 µs, C2 in tens of µs, C3/CC6 in hundreds; a limit just below the deepest state's latency trims the worst-case wake-up jitter while keeping the shallow savings. The cost is the same as capping C-states: idle power, heat and - on a laptop - less boost headroom for the busy cores, because sleeping cores no longer hand their share of the power budget back. The special 'n/a' value (poll forever, no idle at all) is deliberately not offered.",
      int(0, 100_000), NO, Target::PerCpu("power/pm_qos_resume_latency_us")),
    t("cpu.schedutil_rate_limit_us", "CPU", "schedutil rate limit (µs)",
      "Only exists while the governor is schedutil (amd-pstate guided/passive, intel_cpufreq, acpi-cpufreq). Minimum time between two frequency requests from the scheduler. Lower = the clock follows load changes sooner (snappier, more requests); higher = fewer changes and less overhead, at the price of reacting later to bursts. Has no effect in amd-pstate / intel_pstate active mode, where the hardware (CPPC/HWP) picks the frequency on its own.",
      int(0, 1_000_000), NO, Target::File("/sys/devices/system/cpu/cpufreq/schedutil/rate_limit_us")),
    // ── Memory ────────────────────────────────────────────────────────────
    t("thp.enabled", "Memory", "THP enabled",
      "Transparent HugePages: promotes small pages into 2 MB pages where possible, cutting TLB misses for large allocations. madvise = only for memory ranges the app explicitly opts into (Proton/DXVK/most game engines already do this) - the recommended default, no surprise stalls. always = the kernel tries everywhere, which can add a synchronous compaction stall the first time a large allocation needs a huge page; only worth it if you profiled a specific non-madvise-aware workload. never = off, for debugging a THP-related issue.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/enabled")),
    t("thp.shmem_enabled", "Memory", "THP shmem",
      "Same idea as THP enabled above, but for tmpfs/shmem-backed memory (shared memory segments, some Wine/Proton prefixes, /dev/shm) instead of regular anonymous memory. advise = only where explicitly requested, matching the conservative default above; deny turns it off entirely for shmem specifically if you suspect it of a stutter without wanting to disable THP everywhere.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/shmem_enabled")),
    t("thp.defrag", "Memory", "THP defrag",
      "How hard a page fault tries to obtain a huge page when one is not immediately free. defer+madvise = never block synchronously outside madvise regions, only try in the background - pairs with THP enabled=madvise for the lowest-stutter combination and is what the Gaming X3D preset uses. madvise alone still allows a synchronous compaction attempt inside madvise regions; defer+madvise removes that too. Leave 'always' as the kernel default only if you are not chasing stutter.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/defrag")),
    t("thp.khugepaged_defrag", "Memory", "khugepaged defrag",
      "khugepaged periodically scans memory and compacts pages into huge pages in the background. 1 (default) = on; 0 = off, one less source of periodic background CPU/latency jitter, at the cost of huge pages building up more slowly for long-running processes. Worth setting 0 in any latency-focused preset; the memory-efficiency loss is minor over a gaming session's timescale.",
      int(0, 1), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/defrag")),
    t("thp.mthp_16k", "Memory", "mTHP 16 KB anon",
      "Multi-size THP (kernel 6.8+): lets anonymous memory use 16 KB folios instead of only 4 KB or 2 MB pages. Mid-size folios cut TLB misses and page-fault count for mid-size allocations while wasting far less memory than 2 MB pages. inherit = follow THP enabled above; madvise = only regions that ask for huge pages; never = off (kernel default for these sizes). A reasonable trial is madvise for 64K-256K; measure before keeping it, gains are workload-dependent.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/hugepages-16kB/enabled")),
    t("thp.mthp_32k", "Memory", "mTHP 32 KB anon",
      "Multi-size THP (kernel 6.8+): lets anonymous memory use 32 KB folios instead of only 4 KB or 2 MB pages. Mid-size folios cut TLB misses and page-fault count for mid-size allocations while wasting far less memory than 2 MB pages. inherit = follow THP enabled above; madvise = only regions that ask for huge pages; never = off (kernel default for these sizes). A reasonable trial is madvise for 64K-256K; measure before keeping it, gains are workload-dependent.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/hugepages-32kB/enabled")),
    t("thp.mthp_64k", "Memory", "mTHP 64 KB anon",
      "Multi-size THP (kernel 6.8+): lets anonymous memory use 64 KB folios instead of only 4 KB or 2 MB pages. Mid-size folios cut TLB misses and page-fault count for mid-size allocations while wasting far less memory than 2 MB pages. inherit = follow THP enabled above; madvise = only regions that ask for huge pages; never = off (kernel default for these sizes). A reasonable trial is madvise for 64K-256K; measure before keeping it, gains are workload-dependent.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/hugepages-64kB/enabled")),
    t("thp.mthp_128k", "Memory", "mTHP 128 KB anon",
      "Multi-size THP (kernel 6.8+): lets anonymous memory use 128 KB folios instead of only 4 KB or 2 MB pages. Mid-size folios cut TLB misses and page-fault count for mid-size allocations while wasting far less memory than 2 MB pages. inherit = follow THP enabled above; madvise = only regions that ask for huge pages; never = off (kernel default for these sizes). A reasonable trial is madvise for 64K-256K; measure before keeping it, gains are workload-dependent.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/hugepages-128kB/enabled")),
    t("thp.mthp_256k", "Memory", "mTHP 256 KB anon",
      "Multi-size THP (kernel 6.8+): lets anonymous memory use 256 KB folios instead of only 4 KB or 2 MB pages. Mid-size folios cut TLB misses and page-fault count for mid-size allocations while wasting far less memory than 2 MB pages. inherit = follow THP enabled above; madvise = only regions that ask for huge pages; never = off (kernel default for these sizes). A reasonable trial is madvise for 64K-256K; measure before keeping it, gains are workload-dependent.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/hugepages-256kB/enabled")),
    t("thp.mthp_512k", "Memory", "mTHP 512 KB anon",
      "Multi-size THP (kernel 6.8+): lets anonymous memory use 512 KB folios instead of only 4 KB or 2 MB pages. Mid-size folios cut TLB misses and page-fault count for mid-size allocations while wasting far less memory than 2 MB pages. inherit = follow THP enabled above; madvise = only regions that ask for huge pages; never = off (kernel default for these sizes). A reasonable trial is madvise for 64K-256K; measure before keeping it, gains are workload-dependent.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/hugepages-512kB/enabled")),
    t("thp.mthp_1m", "Memory", "mTHP 1 MB anon",
      "Multi-size THP (kernel 6.8+): lets anonymous memory use 1 MB folios instead of only 4 KB or 2 MB pages. Mid-size folios cut TLB misses and page-fault count for mid-size allocations while wasting far less memory than 2 MB pages. inherit = follow THP enabled above; madvise = only regions that ask for huge pages; never = off (kernel default for these sizes). A reasonable trial is madvise for 64K-256K; measure before keeping it, gains are workload-dependent.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/hugepages-1024kB/enabled")),
    t("thp.khp_max_ptes_none", "Memory", "khugepaged max_ptes_none",
      "How many empty (never-touched) 4 KB slots khugepaged accepts when collapsing a 2 MB range into a huge page. 511 (default) = collapse even an almost empty range, which fills in memory the program never used: with THP enabled=always this is the main source of RSS bloat. 0-64 = only collapse ranges that are really in use - much less wasted memory, huge pages still form where they help. With any mTHP size enabled the kernel only honours 0 or 511 for mTHP collapse (other values log a warning and act as 0). Has no effect with THP off.",
      int(0, 511), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/max_ptes_none")),
    t("thp.khp_pages_to_scan", "Memory", "khugepaged pages_to_scan",
      "Pages khugepaged examines per wake-up. Default 4096. Lower = gentler background scanning (less periodic CPU work and lock contention), huge pages form more slowly; higher = faster promotion for long-running processes at the cost of more background work.",
      int(8, 262_144), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/pages_to_scan")),
    t("thp.khp_scan_sleep_ms", "Memory", "khugepaged scan_sleep_millisecs",
      "Pause between khugepaged scan passes. Default 10000 (10 s). Longer = fewer background scan bursts (useful while gaming or on battery); shorter = huge pages form sooner.",
      int(0, 600_000), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/scan_sleep_millisecs")),
    t("thp.khp_max_ptes_swap", "Memory", "khugepaged max_ptes_swap",
      "How many of a 2 MB range's 4 KB pages may still sit in swap when khugepaged collapses the range into a huge page (default 64): the collapse then reads them back in, synchronously, from zram/zswap or disk. 0 = only collapse ranges that are fully resident, so background collapsing never causes swap-in I/O or decompression work behind a running program's back. Only matters with THP enabled and swap in use.",
      int(0, 511), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/max_ptes_swap")),
    t("thp.khp_alloc_sleep_ms", "Memory", "khugepaged alloc_sleep_millisecs",
      "How long khugepaged backs off after failing to allocate a huge page (memory fragmented). Default 60000. Longer = less futile compaction work under memory pressure.",
      int(0, 600_000), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/alloc_sleep_millisecs")),
    t("mm.lru_gen", "Memory", "MGLRU enabled mask",
      "Multi-Gen LRU feature bitmask: 0x1 core MGLRU, 0x2 batched leaf-PTE young-bit aging (scales reclaim cost to the accessed set instead of the whole address space - what keeps reclaim cheap for a game with a huge virtual address space but a much smaller hot set), 0x4 non-leaf PMD aging. 7 (all three) is the kernel default and normally the right value; clearing 0x2 makes reclaim scan cost grow with the game's total mapped memory rather than its working set, which shows up as reclaim-related stutter under pressure. Only change this if debugging MGLRU itself.",
      int(0, 7), NO, Target::File("/sys/kernel/mm/lru_gen/enabled")),
    t("mm.lru_gen_min_ttl", "Memory", "MGLRU min_ttl_ms",
      "Pages younger than this many milliseconds are never evicted, even under memory pressure - protects whatever you touched most recently (the game's active working set) from being paged out to make room elsewhere. 1000 (1s) is the kernel docs' thrashing-prevention example: under real pressure the kernel invokes the OOM killer instead of evicting protected pages, so something gets killed instead of the system slowing down. Only sensible with ample free RAM; 0 disables the protection (kernel default).",
      int(0, 60_000), NO, Target::File("/sys/kernel/mm/lru_gen/min_ttl_ms")),
    t("mm.ksm_run", "Memory", "KSM run",
      "KSM background-scans memory for identical pages across processes and merges them to save RAM - common in VM hosts, rarely useful for a single desktop/game session and pure scanning overhead when there is little page duplication. 0 = stop the scanner (recommended while gaming: one less periodic task competing for cache/memory bandwidth). 1 = run normally. 2 = stop and eagerly unmerge already-merged pages, useful once if you disabled KSM after it had already merged things and want the memory back immediately.",
      int(0, 2), NO, Target::File("/sys/kernel/mm/ksm/run")),
    t("vm.max_map_count", "Memory", "vm.max_map_count",
      "Ceiling on mmap() regions a single process may hold. Several modern Proton/DXVK titles (large open-world games especially) map more regions than the old distro default of 65530 and crash with an out-of-memory-looking error that is actually this limit. 2147483642 is the SteamOS/Proton-recommended value (effectively unlimited) and is what every gaming preset here sets; no real downside to leaving it this high on a desktop.",
      int(65_530, 2_147_483_642), NO, Target::File("/proc/sys/vm/max_map_count")),
    t("vm.swappiness", "Memory", "vm.swappiness",
      "How aggressively the kernel swaps out anonymous memory before reclaiming cache, 0-200. With zram/zswap (compressed RAM swap, fast) higher values are cheap and free more RAM for cache: 100-150 is reasonable. With real disk swap (slow), keep this low, 10-30, so swapping only happens under genuine pressure - 10 is what the gaming presets here use. No swap configured at all: this setting has no effect either way.",
      int(0, 200), NO, Target::File("/proc/sys/vm/swappiness")),
    t("vm.compaction_proactiveness", "Memory", "vm.compaction_proactiveness",
      "How proactively the kernel compacts free memory into contiguous blocks in the background, 0-100 (roughly a percentage of one CPU). Higher keeps more contiguous free memory available for huge-page allocations at the cost of constant low-level background work; 0 disables it entirely, cheapest but means compaction only happens synchronously (as a stall) exactly when something needs it. A moderate value like 5-10 is a reasonable middle ground; avoid 0 here together with watermark_boost_factor 0, since together they remove both the proactive and reactive paths to defragmenting memory.",
      int(0, 100), NO, Target::File("/proc/sys/vm/compaction_proactiveness")),
    t("vm.watermark_boost_factor", "Memory", "vm.watermark_boost_factor",
      "After a fragmentation event (pageblocks of different mobility mixed) kswapd reclaims extra memory: up to factor/10000 of the zone's high watermark, so the default 15000 = up to 150%. This frees page cache (it never holds memory) so later huge-page/high-order allocations find free pageblocks; the cost is bursts of cache reclaim and refaults. 0 = off. Autotune never raises it above the kernel default.",
      int(0, 100_000), NO, Target::File("/proc/sys/vm/watermark_boost_factor")),
    t("vm.watermark_scale_factor", "Memory", "vm.watermark_scale_factor",
      "Sets the gap between the min/low/high memory watermarks as a fraction of total RAM (parts per 10000). A larger value makes kswapd (the background reclaim thread) start reclaiming earlier and more gradually, so allocations are less likely to hit the slow synchronous direct-reclaim path (a stall on the thread that is allocating, right when you would notice it). 50-100 is a reasonable bump from an often-low default on a system with plenty of RAM; the trade-off is slightly less RAM available for cache.",
      int(1, 3000), NO, Target::File("/proc/sys/vm/watermark_scale_factor")),
    t("vm.min_free_kbytes", "Memory", "vm.min_free_kbytes",
      "KiB of RAM the kernel always keeps free for atomic (cannot-sleep-cannot-reclaim) allocations, mainly network/interrupt-context. Too low risks allocation failures under network or interrupt load; too high wastes RAM that could be cache. 262144 (256 MB) is a sane bump from a low distro default on a 16 GB+ system; scale roughly with total RAM if pushed further.",
      int(1024, 4_194_304), NO, Target::File("/proc/sys/vm/min_free_kbytes")),
    t("vm.zone_reclaim_mode", "Memory", "vm.zone_reclaim_mode",
      "Whether the kernel reclaims from a NUMA zone before falling back to another node under pressure. Only matters on multi-node (multi-socket, or single-socket-multi-die-as-NUMA) systems; on a single-node laptop it is already inert at 0 - leave it there.",
      int(0, 7), NO, Target::File("/proc/sys/vm/zone_reclaim_mode")),
    t("vm.page_lock_unfairness", "Memory", "vm.page_lock_unfairness",
      "How many times a thread may steal a contended page lock before the kernel forces a fair FIFO hand-off to the longest waiter. Lower favours latency-fairness; higher favours the current holder's throughput. Narrow effect - the default is fine for virtually every workload.",
      int(0, 10), NO, Target::File("/proc/sys/vm/page_lock_unfairness")),
    t("vm.stat_interval", "Memory", "vm.stat_interval",
      "How often (seconds) the kernel refreshes per-CPU vmstat counters that feed watermark/reclaim decisions. Higher = fewer periodic timer wakeups (marginally less background jitter, matters more on battery than a plugged-in gaming session) but statistics used for reclaim decisions are correspondingly staler. 10 is a reasonable relaxed value versus a more frequent default; do not push much higher on a system actively under memory pressure.",
      int(1, 120), NO, Target::File("/proc/sys/vm/stat_interval")),
    t("vm.page_cluster", "Memory", "vm.page-cluster",
      "Consecutive swap pages read ahead on a swap-in, as a power of two (0 = 1 page, 3 = 8 pages, the disk-swap-era default). Readahead assumes sequential access, true for spinning disks but pointless for zram/NVMe swap where random access is just as fast - 0 avoids wasted effort on pages you will not touch next. Only raise this if you have real swap on a traditional disk.",
      int(0, 6), NO, Target::File("/proc/sys/vm/page-cluster")),
    t("vm.dirty_background_ratio", "Memory", "vm.dirty_background_ratio (%)",
      "Share of dirtyable memory that may be dirty (written but not yet on disk) before the background flusher threads start writing it out. Only shown while the system uses the ratio form: dirty_background_bytes is its counterpart and writing one zeroes the other, so on a system configured in bytes this row stays n/a instead of silently switching the unit. With lots of RAM the stock 10% is gigabytes - it all has to reach the disk eventually, and a large flush competing with game asset reads or an fsync() from the desktop is a classic source of multi-second hitches. 1-5% on a 16-64 GB machine keeps writeback in small, steady chunks. For power saving the opposite (a larger buffer) lets the disk idle longer.",
      int(1, 100), NO, Target::File("/proc/sys/vm/dirty_background_ratio")),
    t("vm.dirty_ratio", "Memory", "vm.dirty_ratio (%)",
      "Share of dirtyable memory that may be dirty before a process that writes is throttled and made to write back itself (the stall you feel). Counterpart of dirty_bytes, same ratio-only rule as the background row. Keep it above the background ratio (the kernel halves the background value otherwise). Low (2-10%) = bounded stalls and short fsync() times - desktop/gaming; high (20-40%, TuneD throughput-performance uses 40) = large write bursts are absorbed at RAM speed - throughput.",
      int(1, 100), NO, Target::File("/proc/sys/vm/dirty_ratio")),
    t("vm.dirty_background_bytes", "Memory", "vm.dirty_background_bytes",
      "Absolute form of dirty_background_ratio: background writeback starts once this many bytes are dirty. On a machine with lots of RAM even 1% is several hundred MB, so the byte form is the only way to get small, steady writeback. CachyOS ships 67108864 (64 MB). Writing it switches the system to byte mode (the ratio row then shows n/a); restoring writes the original ratio back.",
      int(8192, 68_719_476_736), NO, Target::DirtyBytes { bytes: "/proc/sys/vm/dirty_background_bytes", ratio: "/proc/sys/vm/dirty_background_ratio" }),
    t("vm.dirty_bytes", "Memory", "vm.dirty_bytes",
      "Absolute form of dirty_ratio: a writing process is throttled once this many bytes are dirty. CachyOS ships 268435456 (256 MB) so a large copy or download can never pile up gigabytes that then flush all at once and stall the compositor. Keep it above the background value. Same byte-mode switch and restore rule as the background row.",
      int(8192, 68_719_476_736), NO, Target::DirtyBytes { bytes: "/proc/sys/vm/dirty_bytes", ratio: "/proc/sys/vm/dirty_ratio" }),
    t("vm.dirty_writeback_centisecs", "Memory", "vm.dirty_writeback_centisecs",
      "Interval (1/100 s) at which the flusher threads wake up to write old dirty data. Stock 500 (5 s). Raising it to 1500 (15 s, TLP's power-saving value) batches writes so an NVMe/SSD can stay in a low-power state longer - a small but real battery win - at the cost of more data lost on a crash or power cut. 0 (never wake periodically) is not offered: dirty data would then only leave RAM under memory pressure.",
      int(100, 360_000), NO, Target::File("/proc/sys/vm/dirty_writeback_centisecs")),
    t("vm.dirty_expire_centisecs", "Memory", "vm.dirty_expire_centisecs",
      "Age (1/100 s) after which dirty data is old enough to be written by the next periodic flush. Stock 3000 (30 s). Longer on battery (fewer, bigger write bursts, disk sleeps more), shorter when you want less unwritten data at risk. Pairs with the writeback interval above.",
      int(100, 360_000), NO, Target::File("/proc/sys/vm/dirty_expire_centisecs")),
    t("vm.vfs_cache_pressure", "Memory", "vm.vfs_cache_pressure",
      "How eagerly the kernel reclaims the dentry/inode caches (the directory and file metadata cache) relative to the page cache. 100 = stock balance. Lower (e.g. 50) keeps metadata cached longer: file dialogs, shader-cache directories, Steam library scans and `emerge` dependency walks stay fast after they were touched once - cheap on a machine with plenty of RAM. Above 100 frees metadata sooner (low-RAM systems). Do not go near 0: the caches then can never be reclaimed and can end in an OOM.",
      int(1, 1000), NO, Target::File("/proc/sys/vm/vfs_cache_pressure")),
    t("zswap.enabled", "Memory", "zswap",
      "Compressed RAM cache in front of a real swap device: pages that would be written to disk are compressed and kept in RAM, and only the coldest ones go on to the swap partition/file. Turns most swap-outs into a few microseconds of compression instead of disk I/O, which is the difference between a system that slows down under memory pressure and one that stalls. Useful only with a disk swap device; with zram swap it is double compression and should be off.",
      Kind::Bool, NO, Target::File("/sys/module/zswap/parameters/enabled")),
    t("zswap.compressor", "Memory", "zswap compressor",
      "Algorithm for the zswap pool. lz4 = fastest compression and decompression (lowest latency when a swapped page is touched again), lower ratio; zstd = noticeably better ratio (more pages fit in the pool, fewer reach the disk) for more CPU per page. Offered: what the kernel has loaded or can load. A change creates a new pool; pages already stored stay in the old one until they are read back.",
      Kind::Choice, Options::Special, Target::File("/sys/module/zswap/parameters/compressor")),
    t("zswap.max_pool_percent", "Memory", "zswap max pool (% of RAM)",
      "Upper bound of the compressed pool as a share of RAM (stock 20). Larger = more swapped data stays in RAM compressed, less disk swap I/O; smaller = more RAM for everything else. The pool is not preallocated, it only grows under pressure.",
      int(1, 100), NO, Target::File("/sys/module/zswap/parameters/max_pool_percent")),
    t("zswap.shrinker_enabled", "Memory", "zswap shrinker",
      "Kernel 6.8+: under memory pressure zswap writes its coldest compressed pages on to the swap device proactively, instead of only when the pool is full. Keeps the pool from filling up with pages that will never be used again. 1 is right for almost everyone.",
      Kind::Bool, NO, Target::File("/sys/module/zswap/parameters/shrinker_enabled")),
    t("vm.defrag_mode", "Memory", "vm.defrag_mode",
      "1 = the page allocator works harder to avoid fragmentation, which keeps huge pages (THP/mTHP) and other higher-order allocations obtainable; 0 = stock. The kernel documentation recommends enabling it right after boot because fragmentation, once it has happened, can be long-lasting or even permanent - so put it in the boot preset instead of toggling it mid-session. The commit that introduced it reports that THP success rates stop declining over time, but start lower than on the stock allocator. Only pays off while THP/mTHP are in use; with 32 GB of RAM it costs little.",
      Kind::Bool, NO, Target::File("/proc/sys/vm/defrag_mode")),
    t("thp.shrink_underused", "Memory", "THP shrink underused",
      "Kernel 6.12+: every THP created at fault or collapse time is put on a deferred list, and under memory pressure the 'underused' ones (more zero-filled 4 KB pages than khugepaged max_ptes_none allows) are split so the empty part can be reclaimed. 1 = on (the way THP=always pays its RSS bloat back when RAM gets tight); 0 = huge pages are never split for that reason: they stay whole, a little more memory stays held, no splitting work under pressure.",
      Kind::Bool, NO, Target::File("/sys/kernel/mm/transparent_hugepage/shrink_underused")),
    t("zswap.accept_threshold", "Memory", "zswap accept threshold (%)",
      "Once the zswap pool has hit its maximum size, new pages are refused until it shrinks below this percentage of the maximum (stock 90): the hysteresis that stops zswap flapping between 'full' and 'accepting' at the limit. Lower = zswap resumes accepting only after it has drained further, which means more pages go straight to the disk swap in the meantime. Only matters when the pool actually fills up.",
      int(1, 100), NO, Target::File("/sys/module/zswap/parameters/accept_threshold_percent")),
    // ── Scheduler ─────────────────────────────────────────────────────────
    warn(t("sched.ext", "Scheduler", "sched_ext scheduler",
      "Runs a sched_ext BPF scheduler (kernel 6.12+ with CONFIG_SCHED_CLASS_EXT, plus the scx schedulers installed) in place of the kernel's EEVDF while the setting is active; restoring stops it and EEVDF takes over again instantly. lavd = latency-criticality aware, built for gaming and interactive loads (frame pacing, input latency) and aware of big/little and X3D core differences; bpfland = prioritises interactive tasks, good general desktop choice; rusty/flash/cosmos/p2dq are more specialised. Best used as a game-mode setting. A buggy scheduler cannot hang the system: the kernel's watchdog ejects it and falls back to EEVDF. Only scx_* binaries that are root-owned in system directories are ever started.",
      Kind::Choice, Options::Special, Target::SchedExt)),
    t("kernel.split_lock_mitigate", "Scheduler", "split_lock_mitigate",
      "The kernel detects unaligned atomic (split-lock) memory accesses and can throttle the offending core by roughly 1000x as a security/fairness mitigation. Some older x86 code compiled without alignment guarantees - a handful of Windows games under Wine/Proton, and some emulators - trigger this and get catastrophically slow for a moment. 0 disables the throttle (detection/logging still happens, it just does not slow the core down); safe to leave at 0 on a gaming system unless this machine also runs untrusted code from other users.", int(0, 1), NO,
      Target::File("/proc/sys/kernel/split_lock_mitigate")),
    t("kernel.watchdog", "Scheduler", "kernel.watchdog",
      "Kernel hang/lockup detector: periodic per-CPU timer interrupts plus an NMI watchdog, purely a diagnostic safety net. 0 removes those periodic interrupts (a small real reduction in background timer noise). Keep it enabled (1) while debugging kernel/driver issues - the CO validation preset deliberately re-enables it, since a real core failure is exactly the kind of hang you want logged.",
      int(0, 1), NO, Target::File("/proc/sys/kernel/watchdog")),
    t("kernel.numa_balancing", "Scheduler", "kernel.numa_balancing",
      "Background NUMA balancing periodically unmaps and re-faults pages to measure and improve node locality. On a single-node system (this laptop) there is no second node to migrate toward, so this is pure page-fault sampling overhead with zero possible benefit - 0 is correct here and in every preset. Only meaningful to leave non-zero on an actual multi-socket/multi-node machine.",
      int(0, 3), NO, Target::File("/proc/sys/kernel/numa_balancing")),
    t("kernel.timer_migration", "Scheduler", "kernel.timer_migration",
      "1 (default) lets the kernel migrate a timer from an idle CPU to one already awake, so the idle CPU can stay in a deep sleep state instead of waking to fire it - good for battery life. 0 keeps every timer on the CPU that armed it: more predictable latency for that timer, which matters chasing worst-case jitter on a latency-critical thread. For gaming the effect is usually too small to matter either way; battery-focused presets should leave this at 1.",
      int(0, 1), NO, Target::File("/proc/sys/kernel/timer_migration")),
    t("kernel.sched_autogroup", "Scheduler", "sched_autogroup_enabled",
      "Groups each login session's tasks into its own scheduling entity, so one session hogging CPU does not starve another. Required (1) for the Game launch tab's 'renice the autogroup' option to have any effect - that option renices the whole session group, including the game's helper threads/processes spawned outside the immediate process tree. Leave enabled; disabling it is rarely useful outside specific server/scheduling benchmarks.", int(0, 1), NO,
      Target::File("/proc/sys/kernel/sched_autogroup_enabled")),
    t("kernel.cfs_bandwidth_slice_us", "Scheduler", "sched_cfs_bandwidth_slice_us",
      "Internal time slice (µs) used when a cgroup has a CPU bandwidth quota enforced. Only matters if something on the system sets a cgroup CPU quota (some containers, systemd resource-control units); inert otherwise. Leave at default unless you run quota-limited cgroups yourself.",
      int(1, 1_000_000), NO, Target::File("/proc/sys/kernel/sched_cfs_bandwidth_slice_us")),
    t("kernel.sched_util_clamp_min_rt_default", "Scheduler", "RT tasks' default boost (uclamp)",
      "Utilization-clamp minimum given to every real-time task that did not set its own (0-1024). Stock 1024 = any RT task (PipeWire, audio threads, IRQ threads, some kernel threads) makes schedutil request the maximum frequency the moment it runs - historical behaviour, and expensive on battery. A lower value (e.g. 0-256) lets schedutil pick a frequency from the RT load instead. Only matters with the schedutil governor (and for capacity-aware placement on hybrid CPUs); in amd-pstate / intel_pstate active mode the hardware picks the clock and this has no effect on frequency.",
      int(0, 1024), NO, Target::File("/proc/sys/kernel/sched_util_clamp_min_rt_default")),
    t("kernel.sched_energy_aware", "Scheduler", "Energy Aware Scheduling",
      "EAS places each waking task on the CPU where the energy model says it costs least. It only ever runs on asymmetric-capacity CPUs without SMT, with an energy model and the schedutil governor (e.g. Intel hybrid parts without Hyper-Threading such as Lunar/Arrow Lake); the row is n/a everywhere else. 1 = efficiency first (small tasks packed onto efficient cores), 0 = classic load balancing (more throughput for bursty multi-threaded loads).",
      Kind::Bool, NO, Target::File("/proc/sys/kernel/sched_energy_aware")),
    t("kernel.sched_schedstats", "Scheduler", "sched_schedstats",
      "Scheduler statistics collection (per-task wait/sleep accounting used by perf sched, latencytop and some monitoring tools). It costs a little on every context switch; 0 turns it off. Some tools switch it on and never switch it back - leave it on only while you are actually profiling.",
      Kind::Bool, NO, Target::File("/proc/sys/kernel/sched_schedstats")),
    dbg(t("sched.itmt", "Scheduler", "Preferred-core scheduling (ITMT)",
      "Lets the scheduler prefer the cores the firmware ranks fastest (Intel Turbo Boost Max 3.0 / AMD Preferred Core, reported through amd-pstate or intel_pstate). With it on, a lightly threaded load lands on the best-binned cores first: higher single-thread clocks, and idle cores stay idle. Kernel 6.14 moved the switch from /proc/sys/kernel to debugfs (x86/sched_itmt_enabled); whichever exists is used. Almost always best left on; off is for comparing or debugging core placement.",
      Kind::Bool, NO, Target::AnyFile(&["/proc/sys/kernel/sched_itmt_enabled", "/sys/kernel/debug/x86/sched_itmt_enabled"]))),
    dbg(t("sched.preempt", "Scheduler", "Preemption model (debugfs)",
      "Live-switchable preemption model on PREEMPT_DYNAMIC kernels (shows 'root only' if the kernel was not built with it, or debugfs is not mounted - the row still activates, root just cannot read the current value from an unprivileged describe). full = a running task can be preempted almost anywhere: lowest latency, right for desktop/gaming and what the gaming presets set. voluntary = only at explicit preemption points: slightly higher latency, slightly higher throughput, a good middle ground. none = cooperative-style, maximum throughput minimum latency guarantees, essentially never wanted on a desktop. lazy (6.13+) = full's latency behaviour with some of voluntary's throughput via deferred preemption; use it for compile-heavy presets if your kernel supports it, otherwise voluntary is the fallback (see the Compile throughput preset).",
      Kind::Choice, Options::Fixed(&["none", "voluntary", "full", "lazy"]), Target::File("/sys/kernel/debug/sched/preempt"))),
    dbg(t("sched.base_slice_ns", "Scheduler", "EEVDF base slice (debugfs)",
      "Hidden on BORE kernels (CachyOS), where this file is read-only: use 'min_base_slice_ns' below there. EEVDF scheduler's base time slice in nanoseconds - roughly, how long a task runs before it becomes fair game for preemption by an equally-important task. Kernel default scales as ~3 ms times log2(CPU count), capped. Smaller (e.g. 1000000 = 1 ms, what the gaming presets use) means the scheduler re-evaluates fairness more often: lower worst-case latency for anything waiting its turn, at a small throughput cost from more frequent context switches. Larger (e.g. 3000000 = 3 ms, the Compile throughput preset) favours throughput: fewer switches, slightly higher latency for anything waiting.",
      int(100_000, 100_000_000), NO, Target::File("/sys/kernel/debug/sched/base_slice_ns"))),
    dbg(t("sched.min_base_slice_ns", "Scheduler", "min_base_slice_ns (debugfs)",
      "The writable base-slice knob of BORE kernels (CachyOS), where base_slice_ns is read-only: the effective slice is the smallest whole multiple of one scheduler tick (1/HZ, i.e. 1 ms at HZ=1000) that is >= this value, so 1000000 gives 1 ms and 2000000 (the stock minimum) gives 2 ms; values below one tick round up to a tick. Other patched kernels - including the one lutris-game-tune was written against - expose the same tunable under this filename as well. Whichever of the two files exists on your kernel is the one that is 'available'; set both rows the same and only the real one actually writes.",
      int(100_000, 100_000_000), NO, Target::File("/sys/kernel/debug/sched/min_base_slice_ns"))),
    dbg(t("sched.migration_cost_ns", "Scheduler", "migration_cost_ns (debugfs)",
      "How long (ns) a task must have run before the scheduler treats it as cache-cold and freely migratable without a locality penalty. Lower migrates more readily for better load balance; higher keeps tasks pinned longer for better cache locality. Niche - the kernel default suits almost everyone.",
      int(0, 100_000_000), NO, Target::File("/sys/kernel/debug/sched/migration_cost_ns"))),
    dbg(t("sched.nr_migrate", "Scheduler", "nr_migrate (debugfs)",
      "Maximum tasks moved in one load-balancing pass (default 32). Lower reduces burstiness per pass at the cost of correcting large imbalances more slowly; higher corrects faster but does more work per pass. Leave at default unless profiling scheduler balancing specifically.",
      int(1, 1024), NO, Target::File("/sys/kernel/debug/sched/nr_migrate"))),
    dbg(t("sched.cgroup_mode", "Scheduler", "cgroup_mode (debugfs, 7.3+)",
      "How the scheduler scales a task's weight inside a control group (every desktop session, Steam/Lutris scope and systemd/OpenRC service is one). New in 7.3 together with the single-runqueue group scheduler; the 7.3 default is concur. smp = the pre-7.3 behaviour (weight divided by how much of the group runs on that CPU: precise fairness, but tiny factors and extra latency on many-CPU machines). concur = scale by min(CPUs available, runnable tasks in the group): acts like smp under light load and approaches max as the group fills the machine. max = assume every task is fully concurrent: no numeric problems, but artificially high weights when little is running. tasks = scale only by runnable-task count: valid, but wildly different from the traditional meaning. up = no scaling at all, as on a uniprocessor: wrong weight distribution, test use only. Only present on 7.3+ kernels with debugfs.",
      Kind::Choice, Options::Fixed(&["smp", "up", "max", "concur", "tasks"]), Target::File("/sys/kernel/debug/sched/cgroup_mode"))),
    dbg(t("sched.llc_balance", "Scheduler", "Cache-aware scheduling (debugfs, 7.2+)",
      "Kernel 7.2's cache-aware load balancing (CONFIG_SCHED_CACHE): the scheduler tries to keep the threads of one process inside a single last-level-cache domain so they share cache instead of bouncing lines between dies. Only active when a node has more than one LLC - on a 2-CCD Ryzen X3D that means V-Cache CCD vs frequency CCD, so a game's threads get clustered on one CCD. 1 = on, 0 = off (what to compare against). Interacts with CCD affinity and parking: if you already pin games to a CCD it changes little; for everything not pinned it decides which CCD a multi-threaded process lands on. The row only exists when the kernel was built with CONFIG_SCHED_CACHE.",
      Kind::Bool, NO, Target::File("/sys/kernel/debug/sched/llc_balancing/enabled"))),
    dbg(t("sched.llc_aggr_tolerance", "Scheduler", "llc_aggr_tolerance (debugfs, 7.2+)",
      "How aggressively cache-aware scheduling packs a process into one LLC: it scales the cache-footprint and thread-count limits above which the process is no longer packed. 0 switches the aggregation off at runtime, higher values tolerate bigger processes on one LLC (good when the working set fits the 96 MB V-Cache, bad when it overflows and the other die's cores sit idle). Needs CONFIG_SCHED_CACHE.",
      int(0, 100), NO, Target::File("/sys/kernel/debug/sched/llc_aggr_tolerance"))),
    dbg(t("sched.llc_imb_pct", "Scheduler", "llc_imb_pct (debugfs, 7.2+)",
      "Cache-aware scheduling guard: how much busier (in percent) the preferred LLC may be than another before the scheduler stops pulling the process's threads towards it. Lower keeps load more even between the two CCDs, higher favours cache locality. Needs CONFIG_SCHED_CACHE.",
      int(0, 1000), NO, Target::File("/sys/kernel/debug/sched/llc_imb_pct"))),
    dbg(t("sched.llc_overload_pct", "Scheduler", "llc_overload_pct (debugfs, 7.2+)",
      "Cache-aware scheduling guard: utilisation of the preferred LLC (percent) above which aggregation backs off and threads spread to the other LLC again. Called llc_overaggr_pct in some later revisions - whichever file exists is the one used. Needs CONFIG_SCHED_CACHE.",
      int(1, 1000), NO, Target::AnyFile(&["/sys/kernel/debug/sched/llc_overload_pct", "/sys/kernel/debug/sched/llc_overaggr_pct"]))),
    dbg(t("sched.feat_next_buddy", "Scheduler", "NEXT_BUDDY (debugfs)",
      "Wakeup-preemption buddy: after a task wakes another, prefer running the woken task next, on the assumption that waker and wakee share cache-hot data (a game's main thread handing work to a render or audio thread, a pipe or futex hand-off). Kernel source describes it as improving cache locality; it was off by default for years and was switched on in the scheduler tree in Nov 2025 (kernels after that have it on already - the row shows the live value). On for interactive and game loads; leave at kernel default for pure throughput.",
      Kind::Bool, NO, Target::SchedFeature("NEXT_BUDDY"))),
    dbg(t("sched.feat_run_to_parity", "Scheduler", "RUN_TO_PARITY (debugfs)",
      "Wakeup preemption is inhibited until the running task has reached its zero-lag point or used up its slice (default on); tasks with a shorter slice may still cancel it (PREEMPT_SHORT). On = fewer preemptions and context switches, better throughput; off = every eligible wakeup may preempt at once, lower worst-case wake-up latency, more switches. Leave on unless you are chasing a specific latency spike.",
      Kind::Bool, NO, Target::SchedFeature("RUN_TO_PARITY"))),
    t("kernel.sched_bore", "Scheduler", "BORE scheduler",
      "BORE (Burst-Oriented Response Enhancer, in CachyOS kernels): demotes tasks by how long they ran since they last slept or yielded, so bursty CPU hogs (compiles, encoders, shader builds) lose priority to light interactive tasks (compositor, input, audio, the game's main thread). 1 = on, 0 = plain EEVDF weights; switching reweights every task at once, so it is safe at runtime. The rows below only exist while it is built in.",
      int(0, 1), NO, Target::File("/proc/sys/kernel/sched_bore")),
    t("kernel.sched_burst_inherit_type", "Scheduler", "BORE burst inheritance",
      "What a freshly forked process inherits from its relatives: 0 = nothing (every new process starts un-penalised), 1 = the average penalty of its parent's children, 2 = the average over the closest ancestor that actually fans out into several children (stock). With 2 the many short helper processes of a launcher tree (Steam, pressure-vessel, Wine, build systems) inherit the penalty of their siblings instead of starting fresh. Higher = more consistent classification of process trees; 0 = every new process gets a clean slate.",
      int(0, 2), NO, Target::File("/proc/sys/kernel/sched_burst_inherit_type")),
    t("kernel.sched_burst_smoothness", "Scheduler", "BORE smoothness",
      "How slowly the remembered penalty of a task grows when its latest burst was longer than before (0-3; stock 1): the increase is divided by 2^value. Larger = a task that suddenly turns CPU-heavy is demoted more gradually, so short spikes in a normally light task (a game frame that takes longer) do not immediately cost it priority. A shrinking penalty is always taken in one step.",
      int(0, 3), NO, Target::File("/proc/sys/kernel/sched_burst_smoothness")),
    t("kernel.sched_burst_penalty_offset", "Scheduler", "BORE penalty offset",
      "Tolerance before the penalty starts (0-63; stock 24): bursts shorter than roughly 2^offset ns are not penalised at all. Higher = more of a task's run time is tolerated before it is demoted (closer to plain EEVDF); lower = demotion starts earlier.",
      int(0, 63), NO, Target::File("/proc/sys/kernel/sched_burst_penalty_offset")),
    t("kernel.sched_burst_penalty_scale", "Scheduler", "BORE penalty scale",
      "How steeply the penalty grows with the burst length (0-4095; stock 1536 in current CachyOS patches, 1280 in older ones). Higher = hogs fall further behind interactive tasks; 0 = no penalty at all. Raising it favours latency of light tasks over the throughput of CPU-bound ones; lowering it does the opposite.",
      int(0, 4095), NO, Target::File("/proc/sys/kernel/sched_burst_penalty_scale")),
    t("kernel.sched_burst_cache_lifetime", "Scheduler", "BORE burst cache lifetime (ns)",
      "How long (ns) the averaged penalty of a process tree or thread group is cached before it is recomputed for the next fork (stock 75000000 = 75 ms). Longer = cheaper forks, staler averages; shorter = fresher averages, more scanning work per fork under heavy fork rates. Only relevant with burst inheritance on.",
      int(0, 4_294_967_295), NO, Target::File("/proc/sys/kernel/sched_burst_cache_lifetime")),
    dbg(t("sched.feat_preempt_short", "Scheduler", "PREEMPT_SHORT (debugfs)",
      "Lets a waking task with a shorter slice than the running one cancel RUN_TO_PARITY and preempt it at once (default on). It is the exception that keeps latency-sensitive tasks (the ones that asked for a short slice) responsive while RUN_TO_PARITY protects everyone else's slice; turning it off makes RUN_TO_PARITY absolute. Leave on for interactive and game loads.",
      Kind::Bool, NO, Target::SchedFeature("PREEMPT_SHORT"))),
    dbg(t("sched.feat_delay_dequeue", "Scheduler", "DELAY_DEQUEUE (debugfs)",
      "A task that goes to sleep while it still owes the CPU (negative lag) is kept in the competition until it has worked that off instead of being dequeued at once; when it is chosen it has positive lag by definition. Fairer across sleep/wake cycles, at the price of a little extra scheduling work. Default on; 0 restores the old behaviour, mainly useful to rule it out while chasing a scheduling regression.",
      Kind::Bool, NO, Target::SchedFeature("DELAY_DEQUEUE"))),
    dbg(t("sched.feat_hrtick", "Scheduler", "HRTICK (debugfs)",
      "High-resolution preemption tick: arms a one-shot hrtimer for the exact end of the running task's slice instead of waiting for the next periodic tick. Matters when the slice is shorter than a tick or not a multiple of it (e.g. a 0.5 ms base slice at HZ=1000); costs a timer programming on every context switch, so it is off by default. Try it together with a short base slice, measure, and leave off otherwise.",
      Kind::Bool, NO, Target::SchedFeature("HRTICK"))),
    t("wq.affinity_scope", "Scheduler", "Unbound workqueue affinity scope",
      "Kernel 6.6+: how widely an unbound work item may travel from the CPU that queued it. cache (stock) = within the same L3 - on a two-CCD Ryzen the work stays on the die that asked for it, no cross-CCD cache traffic; smt / cpu = even closer (better locality, less work-conservation); numa / system = anywhere (best for spreading heavy work, worst locality). Use together with 'Unbound workqueue CPUs', which is a hard CPU mask; this row is only the locality preference inside that mask.",
      Kind::Choice, Options::Fixed(&["cpu", "smt", "cache", "numa", "system"]), Target::File("/sys/module/workqueue/parameters/default_affinity_scope")),
    t("wq.cpumask", "Scheduler", "Unbound workqueue CPUs",
      "Confines every unbound kernel workqueue (writeback, crypto, most filesystem background work) to the CPUs of one CCD, keeping that background work off whichever CCD the game runs on. Typical use: set this to 'frequency' (or whichever CCD is NOT hosting the game) while the Game launch tab's affinity points the game at 'cache'/the V-Cache CCD - filesystem/crypto work then physically cannot preempt or share L2/L3 with the game's threads. Pick 'all' to undo (kernel default spreads across every CPU). Only offered with 2+ CCDs; a single-CCD chip has nothing to confine work away from.",
      Kind::Choice, Options::Special, Target::WqCpumask),
    t("irq.affinity", "Scheduler", "IRQ affinity",
      "Sets smp_affinity_list for every IRQ to one CCD's CPUs, so device interrupts (network, USB, most peripherals) fire on cores the game is not using. Same pairing idea as workqueue CPUs above: point this at the non-gaming CCD. Best-effort by design - multi-queue NVMe/network IRQs are often kernel-managed and refuse a manual affinity change (reported as 'refused', not a failure); single-CPU-only IRQs (per-CPU timers) are skipped since they say nothing about policy either way. 'all' restores the kernel's default spread.",
      Kind::Choice, Options::Special, Target::Irq),
    // ── Storage ───────────────────────────────────────────────────────────
    t("blk.scheduler", "Storage", "I/O scheduler",
      "I/O scheduler for every whole disk (nvme*, sd*, mmcblk*, vd*) that supports the option. none = no reordering/merging, lowest per-request latency - right for a fast NVMe SSD doing mostly single-queue-depth reads, which is what game loading usually looks like; all the gaming presets here set this. mq-deadline = bounds worst-case latency while still merging/reordering some, a reasonable default for a spinning disk or mixed workloads. kyber = tries to hit explicit latency targets, tunable but more complex. bfq = fairness-focused, best when several processes compete hard for the same disk (e.g. downloading while playing) at some cost to raw throughput.",
      Kind::Choice, BR, Target::PerBlock("scheduler")),
    t("blk.wbt_lat_usec", "Storage", "Writeback throttling (µs)",
      "Writeback throttling tries to keep read latency under this target (microseconds) by slowing background dirty-page writeback when it competes with reads. 0 disables throttling entirely: writeback runs full speed with no read-latency budget, so a big write burst (a shader cache compiling, a game update downloading) can measurably delay unrelated reads (textures streaming in) at the exact moment you would notice it as a stutter. If you see stutter specifically during downloads/installs while playing something else, try a target like 2000-5000 instead of 0.",
      int(0, 1_000_000), NO, Target::PerBlock("wbt_lat_usec")),
    t("blk.read_ahead_kb", "Storage", "Read-ahead (KiB)",
      "How many KiB the kernel speculatively reads ahead on sequential access patterns. Default 128. Larger (e.g. 512-1024) helps games that stream assets sequentially out of large packed files (common in open-world titles) - more of the next chunk is already in cache by the time it is needed. Smaller (e.g. 32-64) helps workloads dominated by random, non-sequential reads (databases, some emulator ROM sets) where readahead just wastes IO bandwidth. If unsure, leave at 128 - this is a workload-shape bet, not a universal win either direction.",
      int(0, 16_384), NO, Target::PerBlock("read_ahead_kb")),
    t("blk.rq_affinity", "Storage", "rq_affinity",
      "Where a finished block request is completed. 0 = on whatever CPU took the interrupt; 1 = on a CPU of the same group (cache domain) as the submitter - fewer cross-CCD cache transfers on a two-CCD Ryzen; 2 = always on the submitting CPU itself (strongest locality, costs an IPI when the interrupt landed elsewhere). Together with 'IRQ affinity' this decides which die handles NVMe completion work.",
      int(0, 2), NO, Target::PerBlock("rq_affinity")),
    t("blk.nomerges", "Storage", "nomerges",
      "Turns off the block layer's request-merge lookups: 0 = merge as usual, 1 = only the cheap one-hit merge attempt, 2 = no merging at all. On a fast NVMe SSD merging saves nothing worth the lookup, so 2 trims a little CPU per request; it does hurt sequential throughput on spinning disks and some SATA SSDs.",
      int(0, 2), NO, Target::PerBlock("nomerges")),
    t("blk.iostats", "Storage", "I/O statistics accounting",
      "Per-request accounting that feeds /proc/diskstats, iostat and the PSI I/O numbers. 0 removes it from the I/O fast path (a small saving per request on NVMe at high IOPS); the cost is that iostat, htop's disk columns and I/O statistics stop updating.",
      Kind::Bool, NO, Target::PerBlock("iostats")),
    t("blk.add_random", "Storage", "Disk entropy contribution",
      "Whether disk I/O timing is mixed into the kernel's entropy pool. 0 = off: one less per-request hook. Modern kernels do not depend on it for seeding, so 0 loses nothing on a machine with a hardware RNG (and this one has the CPU's).",
      Kind::Bool, NO, Target::PerBlock("add_random")),
    t("blk.nr_requests", "Storage", "Queue depth (nr_requests)",
      "How many requests may be allocated in the block layer per queue. Lower = shorter queues, less queuing delay behind a big write burst; higher = more requests in flight for sustained throughput. The kernel refuses values above what the device's tag set can hold, which is reported as a failure for this row only. Stock is a device-dependent value; change it only to chase latency under heavy parallel I/O.",
      int(4, 65_536), NO, Target::PerBlock("nr_requests")),
    // ── Network ───────────────────────────────────────────────────────────
    t("net.tcp_congestion", "Network", "TCP congestion control",
      "Algorithm that decides how fast TCP sends. cubic (default) backs off on packet loss, so a lossy Wi-Fi link or a busy uplink makes it swing between too fast and too slow - visible as latency spikes in online games and uneven downloads. bbr models the path's bandwidth and round-trip time instead and keeps queues short: steadier latency and better throughput on Wi-Fi and long routes. bbr is loaded on demand (tcp_bbr module). Only affects new connections.",
      Kind::Choice, Options::Special, Target::File("/proc/sys/net/ipv4/tcp_congestion_control")),
    t("net.default_qdisc", "Network", "Default queueing discipline",
      "Packet scheduler attached to network interfaces when they come up. fq = fair queueing with pacing, the pairing BBR was designed for; fq_codel = common distro default, good against bufferbloat; cake = most thorough bufferbloat control, a little more CPU. Applies to interfaces (re)initialised afterwards - reconnect Wi-Fi or the link to pick it up.",
      Kind::Choice, Options::Special, Target::File("/proc/sys/net/core/default_qdisc")),
    t("net.wifi_power_save", "Network", "Wi-Fi power save",
      "802.11 power save lets the Wi-Fi radio doze between beacons. On (the usual default) saves real power on battery but adds tens of milliseconds of latency and jitter whenever the radio has to wake - the classic cause of ping spikes in online games. Off = radio always awake: steady latency, more drain. Set per interface through iw; NetworkManager may turn it back on when it reconnects (set wifi.powersave there to make it stick).",
      Kind::Bool, NO, Target::WifiPowerSave),
    t("net.tcp_slow_start_after_idle", "Network", "TCP slow start after idle",
      "1 (stock) = a TCP connection that has been idle for about one RTO restarts from a small congestion window. 0 = it keeps the window it had, so a long-lived connection that goes quiet and bursts again (a game server link, SSH, a download resuming after a pause) gets its full speed back at once. Cheap and safe for a desktop; affects new sends on existing connections immediately.",
      int(0, 1), NO, Target::File("/proc/sys/net/ipv4/tcp_slow_start_after_idle")),
    t("net.tcp_mtu_probing", "Network", "TCP MTU probing",
      "0 = off, 1 = packetization-layer path-MTU discovery switches on when an ICMP 'black hole' is detected (a router that drops the 'too big' messages, common on VPNs and some mobile networks: connections hang after the handshake), 2 = always probe. 1 costs nothing until it is needed and rescues the connections that would otherwise stall.",
      int(0, 2), NO, Target::File("/proc/sys/net/ipv4/tcp_mtu_probing")),
    t("net.netdev_max_backlog", "Network", "Receive backlog (packets)",
      "Per-CPU queue of received packets waiting for the network stack when the NIC delivers faster than the CPU processes them (stock 1000; CachyOS ships 4096). A larger queue absorbs bursts on a fast link (2.5 GbE, Wi-Fi 7) instead of dropping packets, at the price of a little more buffering delay while it is full. Only matters under bursty receive load.",
      int(100, 1_000_000), NO, Target::File("/proc/sys/net/core/netdev_max_backlog")),
    // ── Devices ───────────────────────────────────────────────────────────
    t("pci.aspm", "Devices", "PCIe ASPM policy",
      "PCIe Active State Power Management policy. 'performance' keeps every PCIe link at full power, no link-state transitions: removes the wake-up latency that shows as GPU or NVMe micro-jitter when a link drops to a power-saving state between traffic bursts - right for a plugged-in gaming session. 'powersave'/'powersupersave' let links drop to save power (better battery life, small idle power win) but on some hardware combinations actively cause dropouts on NVMe or Wi-Fi rather than just adding latency - if you see random Wi-Fi disconnects or NVMe timeouts, try 'performance' or 'default' here even outside gaming. 'default' defers to what the BIOS/ACPI tables request per device.",
      Kind::Choice, BR, Target::File("/sys/module/pcie_aspm/parameters/policy")),
    warn(t("pci.aspm_links", "Devices", "PCIe ASPM per link",
      "Overrides ASPM on each PCIe link where a driver or the firmware turned it off - the global policy above cannot re-enable those. l1 = allow L1 on every link; l1ss = also L1.1/L1.2 substates, the ones that matter for idle power (a Wi-Fi card or NVMe drive stuck without L1.2 can cost ~0.5-1 W at idle). Some devices disable ASPM on purpose because it breaks them (MediaTek Wi-Fi drops the connection on some firmware, older NVMe controllers stall): try it, and restore originals if a device misbehaves. Links that refuse are skipped.",
      Kind::Choice, Options::Fixed(&["l1", "l1ss"]), Target::PciAspm)),
    t("pm.mem_sleep", "Devices", "Suspend mode (mem_sleep)",
      "What 'suspend' means. s2idle = modern standby: fast resume, firmware-managed, but the SoC and some devices stay partly powered, so overnight drain is higher and a laptop in a bag can warm up if something keeps waking it. deep = classic S3: everything but RAM powered off, lowest drain, slightly slower resume; only offered when the firmware advertises it. Put it in the boot preset to make it stick.",
      Kind::Choice, BR, Target::File("/sys/power/mem_sleep")),
    t("pci.latency_timer", "Devices", "PCI latency timers",
      "Sets the legacy PCI latency-timer register (config offset 0x0D) on every PCI function: 0x00 on the host bridge, 0x80 on PCI-to-PCI bridges, 0x20 on everything else - the same values lutris-game-tune's setpci calls used, written with a direct single-byte pwrite instead of shelling out. In practice this has close to zero effect on a modern system: PCIe functions hardwire this register to 0 and ignore writes, so a 'stock' vs 'tuned' difference on an all-PCIe laptop is expected and not a sign anything is wrong. Kept for completeness and the rare legacy-PCI device where it does matter.",
      Kind::Choice, Options::Special, Target::PciLatency),
    t("snd.hda_power_save", "Devices", "HDA power_save (s)",
      "Idle timeout (s) before the HDA audio codec may power down. 0 = never power it down: removes both the power-up pop some codecs make and the wake latency before the next sound plays. Worth 0 whenever audio glitches/pops are a complaint; a positive number is the idle delay in seconds.",
      int(0, 3600), NO, Target::File("/sys/module/snd_hda_intel/parameters/power_save")),
    t("snd.hda_power_save_controller", "Devices", "HDA power_save_controller",
      "Companion to power_save above, but for the HDA controller chip rather than the codec. Disable (0) alongside power_save=0 if pops/clicks persist with only the codec setting changed - some hardware needs both to fully stop the power-down cycle.",
      Kind::Bool, NO, Target::File("/sys/module/snd_hda_intel/parameters/power_save_controller")),
    t("usb.autosuspend", "Devices", "USB autosuspend delay (s)",
      "Default autosuspend delay (seconds) applied to USB devices as they are plugged in or the driver binds. -1 disables autosuspend entirely for newly-bound devices: no risk of a mouse/controller/USB DAC needing a moment to wake up right when you move it - the recommended value for gaming peripherals. This only affects devices that bind after the change; anything already plugged in keeps whatever delay it already had (replug it, or reboot with this in the boot preset, to apply retroactively). A positive number is the idle seconds before autosuspend for devices without their own override.",
      int(-1, 3600), NO, Target::File("/sys/module/usbcore/parameters/autosuspend")),
    t("usb.autosuspend_ms", "Devices", "USB autosuspend delay · connected (ms)",
      "Idle time in milliseconds before a USB device that is plugged in RIGHT NOW may autosuspend (power/autosuspend_delay_ms). The 'USB autosuspend delay' row above only reaches devices that bind later; this one reaches the ones already connected, so together they cover both. Only matters where runtime PM is 'auto' (see 'USB runtime PM'): 2000 is the kernel default, -1 = never suspend, small values save more power but wake the device more often. HID (mouse, keyboard, controller) and USB audio devices are skipped, like in the runtime PM row.",
      int(-1, 3_600_000), NO, Target::UsbAutosuspendMs),
    t("net.wol", "Devices", "Wake-on-LAN (Ethernet)",
      "Wake-on-LAN keeps the Ethernet PHY/MAC partly powered so a magic packet can wake the machine. 0 = off (TLP's WOL_DISABLE default): the NIC can power down fully in suspend and the laptop cannot be woken by network chatter in a bag; 1 = magic-packet wake (ethtool wol g), only offered when the port supports it. Set through ethtool; restoring a port that had another wake mode (e.g. 'pg') writes magic-packet wake back. n/a without ethtool or without a wired port that supports WoL.",
      Kind::Bool, NO, Target::EthWol),
    t("net.eee", "Devices", "Energy-Efficient Ethernet (EEE)",
      "EEE lets the Ethernet PHY drop into a low-power idle state between packets (LPI) and wake on demand. 1 = on: a small idle saving on the PHY; 0 = off: no wake-up delay (tens of microseconds) on the first packet after a quiet moment, and it avoids the link flapping some switch/NIC combinations show with EEE. Set through ethtool --set-eee; only offered for a wired port whose driver reports EEE settings.",
      Kind::Bool, NO, Target::EthEee),
    t("rf.bluetooth", "Devices", "Bluetooth radio",
      "Soft-blocks / unblocks every Bluetooth adapter through rfkill (1 = radio on, 0 = off). An idle but enabled adapter keeps its USB/PCIe link and firmware awake (a few hundred mW); TLP switches it off on battery when you list it in DEVICES_TO_DISABLE. Turning it off disconnects every Bluetooth device, so leave it on if a BT mouse or headset is in use.",
      Kind::Bool, NO, Target::Rfkill("bluetooth")),
    warn(t("rf.wlan", "Devices", "Wi-Fi radio",
      "Soft-blocks / unblocks every Wi-Fi adapter through rfkill (1 = radio on, 0 = off). Off drops the connection completely - only for a wired-only or offline profile. Do not put it in a boot preset unless you are sure a wired link is available.",
      Kind::Bool, NO, Target::Rfkill("wlan"))),
    t("rf.wwan", "Devices", "WWAN (mobile broadband) radio",
      "Soft-blocks / unblocks every WWAN modem through rfkill (1 = radio on, 0 = off). Only present on laptops with a cellular modem; a powered modem searching for a network is a large idle drain.",
      Kind::Bool, NO, Target::Rfkill("wwan")),
    t("gpu.amdgpu_abm", "Devices", "Panel power savings (amdgpu ABM)",
      "Adaptive backlight modulation of the internal display (amdgpu 'panel_power_savings', kernel 6.8+): the driver lowers the backlight and boosts pixel values to compensate. 0 = off (accurate colour and contrast), 1-4 = increasingly aggressive; TLP's AMDGPU_ABM_LEVEL_ON_SAV is 3 on battery and 0 otherwise. Saves a few hundred mW to over a watt on bright content but visibly changes contrast in dark scenes - keep 0 for gaming and colour work. power-profiles-daemon may override the level when it changes profile. n/a without an amdgpu-driven eDP panel or on older kernels.",
      int(0, 4), NO, Target::AmdgpuAbm),
    t("disk.apm_0", "Devices", "Disk APM level · disk 1",
      "ATA Advanced Power Management level of this disk (hdparm -B), TLP's DISK_APM_LEVEL. 1-127 = aggressive saving and the drive may spin down, 128-253 = saving without spin-down (128 is TLP's battery default), 254 = maximum performance (TLP's AC default), 255 = APM off. Only drives that report APM support are listed (mostly spinning disks; most SATA SSDs ignore it); USB and NVMe disks are never touched. Each disk has its own row, tied to the drive's serial number, so a value follows the drive even if sdX names change. Reading the live value needs root or membership of the 'disk' group; the row then shows the value LPM last applied.",
      int(1, 255), NO, Target::DiskApm(0)),
    t("disk.apm_1", "Devices", "Disk APM level · disk 2",
      "Same as 'Disk APM level · disk 1', for the second APM-capable ATA disk.",
      int(1, 255), NO, Target::DiskApm(1)),
    t("disk.apm_2", "Devices", "Disk APM level · disk 3",
      "Same as 'Disk APM level · disk 1', for the third APM-capable ATA disk.",
      int(1, 255), NO, Target::DiskApm(2)),
    t("disk.apm_3", "Devices", "Disk APM level · disk 4",
      "Same as 'Disk APM level · disk 1', for the fourth APM-capable ATA disk.",
      int(1, 255), NO, Target::DiskApm(3)),
    t("pm.ahci_runtime_timeout", "Devices", "AHCI disk runtime PM timeout (ms)",
      "Idle time in milliseconds before an ATA/SATA disk whose runtime PM is 'auto' is suspended (device/power/autosuspend_delay_ms of the disk; TLP's AHCI_RUNTIME_PM_TIMEOUT, default 15 s = 15000). Applied before the runtime PM rows below, like TLP, so the disk never runs with a stale short timeout. Only useful together with 'AHCI disk runtime PM' = auto.",
      int(0, 3_600_000), NO, Target::AhciDisk("power/autosuspend_delay_ms")),
    t("pm.ahci_disk_runtime", "Devices", "AHCI disk runtime PM",
      "Runtime power management of every ATA/SATA disk (device/power/control; TLP's AHCI_RUNTIME_PM_ON_* for disks): auto = an idle disk is suspended after the timeout above and woken on the next access, on = never suspended (TLP's AC setting). A suspended spinning disk pays a spin-up delay on the next read. NVMe and USB disks are not affected.",
      Kind::Choice, Options::Fixed(&["auto", "on"]), Target::AhciDisk("power/control")),
    t("pm.ahci_port_runtime", "Devices", "AHCI port runtime PM",
      "Runtime power management of every AHCI port (ata* under the SATA controller; TLP's AHCI_RUNTIME_PM_ON_* for ports): auto lets an idle port (nothing attached, or its disk suspended) power down, on keeps it powered. Complements 'SATA link power (ALPM)' in the Power group, which controls the link state itself. n/a on machines without an AHCI controller.",
      Kind::Choice, Options::Fixed(&["auto", "on"]), Target::AhciPortRuntime),
    // ── Power (runtime PM of devices) ─────────────────────────────────────
    t("pm.pci_runtime", "Power", "PCI runtime PM",
      "Runtime power management of every PCI function (power/control): auto = an idle device (Wi-Fi, card reader, USB/Thunderbolt controller, audio, SATA) may drop to D3 and is woken on demand - TLP's battery setting, often worth 1-3 W idle on a laptop; on = always powered (no resume delay, TLP's AC setting). GPUs are left alone, together with their audio/USB-C sibling functions and every bridge above them: the NVIDIA dGPU reaches D3cold through its own driver and its root port, and forcing 'on' there would keep it awake. Drivers that manage this themselves (nvidia, nouveau, amdgpu, radeon, i915, xe, mei_me) are skipped too.",
      Kind::Choice, Options::Fixed(&["auto", "on"]), Target::PciRuntimePm),
    t("pm.usb_runtime", "Power", "USB runtime PM (connected devices)",
      "Runtime autosuspend of the USB devices plugged in right now (power/control) - the 'USB autosuspend delay' row only affects devices that bind later. auto = idle devices (webcam, fingerprint reader, Bluetooth, hubs) suspend; on = never. HID (mouse, keyboard, controller) and USB audio devices are always skipped: suspended input devices lose the first movement, suspended DACs pop.",
      Kind::Choice, Options::Fixed(&["auto", "on"]), Target::UsbRuntimePm),
    t("pm.sata_alpm", "Power", "SATA link power (ALPM)",
      "AHCI link power management of every SATA port. med_power_with_dipm = the modern default (TLP recommends it for AC and battery): link and device may enter partial/slumber states when idle, a large idle saving with a tiny resume delay. min_power = deepest states, most saving, occasionally causes errors on older drives. max_performance = link always active, lowest latency. n/a on machines with only NVMe drives.",
      Kind::Choice, Options::Fixed(&["max_performance", "medium_power", "med_power_with_dipm", "min_power"]), Target::ScsiHost("link_power_management_policy")),
    t("pm.nvme_latency_us", "Power", "NVMe APST latency tolerance (µs)",
      "Ceiling on the entry+exit latency of the NVMe power states that Autonomous Power State Transition may use, per controller (power/pm_qos_latency_tolerance_us). The driver reprograms APST immediately. 0 = APST off: the drive never enters a non-operational state - no wake-up hitch when a game streams from an idle drive, but ~0.5-1 W more at idle and a warmer drive. 100000 (the usual default) = every state the drive offers. A middle value keeps the shallow states only. n/a when the drive has no APST.",
      int(0, 1_000_000), NO, Target::NvmeLatency),
    amd(t("gpu.amdgpu_dpm", "Devices", "iGPU DPM level (amdgpu)",
      "Forces the integrated Radeon GPU's power state. 'low' pins the iGPU to its lowest performance level, freeing shared SoC power/thermal budget for the CPU cores - worth trying specifically when gaming on the discrete GPU, since the iGPU is doing nothing but display output/compositing anyway. 'auto' (default) lets the driver manage it dynamically. 'high' forces maximum iGPU performance, only useful running GPU work on the iGPU itself (rare on a laptop with a discrete GPU) - not something a gaming preset should set.",
      Kind::Choice, Options::Fixed(&["auto", "low", "high"]), Target::AmdgpuDpm)),
    intel(t("gpu.intel_max_mhz", "Devices", "iGPU max frequency (Intel, MHz)",
      "Ceiling of the Intel integrated GPU (i915 rps_max_freq_mhz / xe max_freq). Same idea as the amdgpu 'low' level on AMD: while the game renders on the NVIDIA dGPU the iGPU only composites the desktop, so capping it hands the shared package power budget to the CPU cores. The kernel refuses values outside RPn…RP0 and a max below the current min.",
      int(100, 4000), NO, Target::IntelGt { i915: "rps_max_freq_mhz", xe: "max_freq" })),
    intel(t("gpu.intel_min_mhz", "Devices", "iGPU min frequency (Intel, MHz)",
      "Floor of the Intel iGPU clock. Only worth raising when the iGPU itself renders (hybrid mode, video playback stutter); otherwise leave it at the hardware minimum (RPn).",
      int(100, 4000), NO, Target::IntelGt { i915: "rps_min_freq_mhz", xe: "min_freq" })),
    intel(t("gpu.intel_slpc_profile", "Devices", "iGPU SLPC power profile (i915)",
      "GuC SLPC power profile of the Intel iGPU: base = stock frequency management, power_saving = the GuC keeps the iGPU clock lower and ramps more slowly. power_saving is a free battery win when the dGPU does the rendering.",
      Kind::Choice, BR, Target::IntelGt { i915: "slpc_power_profile", xe: "" })),
    // ── Stability ─────────────────────────────────────────────────────────
    t("mce.check_interval", "Stability", "MCE poll interval (s)",
      "Polling interval (seconds) for correctable machine-check errors (early-warning signs of a marginal core/memory, short of a full crash). Stock is 300s. 10s (what the CO validation preset uses) catches a marginal Curve Optimizer offset within seconds of it starting to misbehave instead of up to 5 minutes later - pair with `dmesg -w` or rasdaemon open in a terminal while stress-testing a new offset. Set it back to the stock 300 s for normal use. 0 (no polling) is not offered: correctable errors - the early warning of failing RAM or a marginal core - would then go unnoticed until something crashes.",
      int(1, 3600), NO, Target::Mce),
    // ── Hot-plug (must stay last, see HOTPLUG_KEYS) ───────────────────────
    warn(t("cpu.smt", "CPU", "SMT",
      "Turns SMT (the second logical thread per physical core) on or off system-wide. Most games are unaffected or slightly faster with SMT on (more threads available); a minority of titles - especially ones sensitive to cache contention between sibling threads, or with poor thread-count scaling - show better 1% lows with it off, since every physical core is then dedicated to one thread with no sibling contention. This is genuinely game-specific: test SMT on vs off on the specific title if chasing 1% lows. Hot-plugs half the CPUs off/online, which is why this row is always applied last and restored first - every other per-CPU setting needs the CPU online first to accept the write.",
      Kind::Choice, Options::Fixed(&["on", "off"]), Target::File("/sys/devices/system/cpu/smt/control"))),
    warn(t("cpu.ccd_park", "CPU", "Park a CCD / E-cores (offline)",
      "On a hybrid Intel CPU this parks the E-cores instead (the P-cores hold cpu0 and can never be parked): the game then only ever shares the ring with P-cores - a test tool for titles with bad hybrid scheduling, not a daily setting. Takes an entire CCD fully offline (every CPU in it): no scheduling, no IRQs, no cross-CCD cache-coherency traffic can reach it at all. The most deterministic possible setup for an X3D chip - the game gets sole, uncontested use of one die's cache and cores with zero interference from the other die under any circumstance - at the obvious cost of losing that die's cores entirely until restored. The Competitive preset parks the frequency CCD as its most aggressive step; only reach for this if affinity plus workqueue/IRQ steering (which achieve most of the isolation benefit without losing any cores) is not enough for what you are chasing. cpu0's CCD can never be parked (the kernel needs cpu0 online), so on a 2-CCD chip you can only ever park 'the other one'. While any CPU is offline (park or SMT off) nvidia-powerd is stopped and restarted afterwards: it cannot handle hot-unplugged CPUs and on Blackwell laptops that ends in a GSP hang (Xid 79/119 -> 154, reboot needed). Dynamic Boost (+25 W GPU) is therefore off while parked. Game mode (lpm-gamemode PRE/RUN/WRAP) never takes the CCD offline: Wine/Proton count only online CPUs and map them 1:1 to CPU numbers, so the hole a parked CCD leaves (0-7,16-23) breaks thread pinning and some games do not start. There the CCD is emptied instead - the game gets the other CCD as a cgroup v2 cpuset partition (everything else is moved off it; needs the unified cgroup hierarchy, OpenRC rc_cgroup_mode=\"unified\", otherwise the game is only pinned), IRQs and unbound kernel work are moved onto the parked CCD, and WINE_CPU_TOPOLOGY maps the game's CPUs - which isolates the game just as well.",
      Kind::Choice, Options::Special, Target::CcdPark)),
];

/// Rows that were removed from the table; saved presets may still carry them and must not error.
pub const RETIRED_KEYS: &[&str] = &["wq.power_efficient"];

pub fn find(key: &str) -> Option<&'static Tunable> {
    TUNABLES.iter().find(|t| t.key == key)
}

pub fn is_hotplug(key: &str) -> bool { HOTPLUG_KEYS.contains(&key) }

// ── low-level reads ────────────────────────────────────────────────────────

fn read(p: &Path) -> Option<String> { read_trimmed(p).ok() }

/// "always [madvise] never" -> ("madvise", [always, madvise, never]).
/// Also accepts sched/preempt's "none voluntary (full) lazy".
pub fn parse_bracketed(s: &str) -> (Option<String>, Vec<String>) {
    let mut sel = None;
    let mut opts = Vec::new();
    for w in s.split_whitespace() {
        let inner = w.strip_prefix('[').and_then(|w| w.strip_suffix(']'))
            .or_else(|| w.strip_prefix('(').and_then(|w| w.strip_suffix(')')));
        match inner {
            Some(i) => { sel = Some(i.to_owned()); opts.push(i.to_owned()); }
            None => opts.push(w.to_owned()),
        }
    }
    (sel, opts)
}

fn num_suffix(name: &str, prefix: &str) -> Option<u32> {
    name.strip_prefix(prefix).filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))?.parse().ok()
}

/// Sorted numbered children "prefixN" of `dir`.
fn numbered(dir: &Path, prefix: &str) -> Vec<(u32, PathBuf)> {
    let mut v: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|e| num_suffix(&e.file_name().to_string_lossy(), prefix).map(|n| (n, e.path())))
        .collect();
    v.sort_by_key(|x| x.0);
    v
}

/// Active policies only: a policy whose CPUs are all offline (parked CCD)
/// keeps its files, but every read and write on them returns EBUSY.
///
/// Cached like [`ccx_groups`] (same TTL, dropped by the same hot-plug writes): one
/// `describe` asks a dozen times, each a directory walk plus one read per policy.
pub fn policies() -> Vec<PathBuf> {
    if let Some(v) = POLICIES.with(|t| t.borrow().as_ref().filter(|(at, _)| at.elapsed() < TOPO_TTL).map(|(_, v)| v.clone())) {
        return v;
    }
    let v: Vec<PathBuf> = numbered(&Path::new(CPU_DIR).join("cpufreq"), "policy").into_iter().map(|x| x.1)
        .filter(|p| read(&p.join("affected_cpus")).map_or(false, |s| !s.trim().is_empty()))
        .collect();
    POLICIES.with(|t| *t.borrow_mut() = Some((std::time::Instant::now(), v.clone())));
    v
}

pub fn cpus() -> Vec<(u32, PathBuf)> { numbered(Path::new(CPU_DIR), "cpu") }

/// cpufreq policies whose CPUs all belong to L3 domain `ccd` (none on single-CCD parts).
fn ccd_policies(ccd: usize) -> Vec<PathBuf> {
    let groups = ccx_groups();
    if groups.len() < 2 { return vec![]; }
    let Some(g) = groups.get(ccd) else { return vec![] };
    policies_within(&g.cpus)
}

/// cpufreq policies whose CPUs all belong to `set`.
fn policies_within(set: &[usize]) -> Vec<PathBuf> {
    if set.is_empty() { return vec![]; }
    policies().into_iter().filter(|p| {
        let cpus = read(&p.join("related_cpus")).map(|s| s.split_whitespace().filter_map(|c| c.parse().ok()).collect::<Vec<usize>>())
            .unwrap_or_default();
        !cpus.is_empty() && cpus.iter().all(|c| set.contains(c))
    }).collect()
}

/// Intel hybrid core classes from the perf PMU nodes (cpu_core / cpu_atom).
/// Lists are as the kernel reports them (all present CPUs of that class).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hybrid { pub pcores: Vec<usize>, pub ecores: Vec<usize> }

pub fn hybrid() -> Option<Hybrid> {
    let list = |n: &str| read(&Path::new("/sys/devices").join(n).join("cpus")).map(|s| cpu_list(&s)).unwrap_or_default();
    let (p, e) = (list("cpu_core"), list("cpu_atom"));
    (!p.is_empty() && !e.is_empty()).then_some(Hybrid { pcores: p, ecores: e })
}

fn core_type_cpus(ct: CoreType) -> Vec<usize> {
    hybrid().map(|h| match ct { CoreType::P => h.pcores, CoreType::E => h.ecores }).unwrap_or_default()
}

/// A hybrid core class as a Ccx-shaped CPU group (online CPUs only), for the
/// same affinity / workqueue / IRQ / park role machinery the CCDs use.
fn hybrid_group(ct: CoreType) -> Option<Ccx> {
    let online = online_cpus();
    let cpus: Vec<usize> = core_type_cpus(ct).into_iter().filter(|c| online.is_empty() || online.contains(c)).collect();
    if cpus.is_empty() { return None; }
    let max_khz = cpus.iter()
        .filter_map(|c| read(&Path::new(CPU_DIR).join(format!("cpu{c}/cpufreq/cpuinfo_max_freq"))))
        .filter_map(|s| s.parse().ok()).max().unwrap_or(0);
    let l3_kib = cpus.first().and_then(|c| read(&Path::new(CPU_DIR).join(format!("cpu{c}/cache/index3/size")))).map_or(0, |s| size_kib(&s));
    Some(Ccx { index: if ct == CoreType::P { 0 } else { 1 }, cpus, l3_kib, max_khz })
}

/// More than one steerable CPU domain: 2+ CCDs, or a hybrid P/E split.
fn has_domains() -> bool { ccx_groups().len() > 1 || hybrid().is_some() }

pub fn x3d_mode_path() -> Option<PathBuf> {
    std::fs::read_dir(X3D_DRIVER_DIR).ok()?.flatten()
        .map(|e| e.path().join("amd_x3d_mode"))
        .find(|p| p.is_file())
        .and_then(|p| canonical_in_sysfs(&p))
}

pub fn cstate_names() -> Vec<String> {
    numbered(&Path::new(CPU_DIR).join("cpu0/cpuidle"), "state").into_iter()
        .map(|(i, p)| read(&p.join("name")).unwrap_or_else(|| format!("state{i}")))
        .collect()
}

pub fn debugfs_mounted() -> bool {
    std::fs::read_to_string("/proc/self/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some(DEBUGFS)))
        .unwrap_or(false)
}

/// World-readable copy of the live debugfs tunable values, written by root
/// (tune-helper) after every privileged operation. debugfs itself stays 0700:
/// unprivileged callers read this file instead, so nothing is opened up.
pub const DEBUGFS_SNAPSHOT: &str = "/run/legion-power-manager/debugfs.json";

/// Root only. Atomic (temp + rename), mode 0644, in a root-owned 0755 directory.
pub fn write_debugfs_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } != 0 || !ensure_debugfs() { return; }
    let mut m = Map::new();
    for t in TUNABLES.iter().filter(|t| t.debugfs && vendor_ok(t)) {
        if let Some(v) = current(t) { m.insert(t.key.to_owned(), Value::String(v)); }
    }
    let path = Path::new(DEBUGFS_SNAPSHOT);
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() { return; }
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    let tmp = dir.join(".debugfs.json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let body = Value::Object(m).to_string();
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Snapshot value of a debugfs tunable; ignored unless the file is root-owned and not group/world-writable.
fn debugfs_snapshot_value(key: &str) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(DEBUGFS_SNAPSHOT).ok()?;
    if md.uid() != 0 || md.mode() & 0o022 != 0 { return None; }
    let v: Value = serde_json::from_str(&std::fs::read_to_string(DEBUGFS_SNAPSHOT).ok()?).ok()?;
    v.get(key)?.as_str().map(str::to_owned)
}

/// Some(true/false) = the root-written snapshot lists / does not list this debugfs key (the
/// tunable exists on this kernel or not); None when no trustworthy snapshot exists yet.
fn debugfs_snapshot_has(key: &str) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(DEBUGFS_SNAPSHOT).ok()?;
    if md.uid() != 0 || md.mode() & 0o022 != 0 { return None; }
    let v: Value = serde_json::from_str(&std::fs::read_to_string(DEBUGFS_SNAPSHOT).ok()?).ok()?;
    Some(v.get(key).is_some())
}

/// Mounts debugfs if needed (root only). Never fails the request.
pub fn ensure_debugfs() -> bool {
    if debugfs_mounted() { return true; }
    let src = std::ffi::CString::new("debugfs").unwrap();
    let dst = std::ffi::CString::new(DEBUGFS).unwrap();
    unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), src.as_ptr(), 0, std::ptr::null()) == 0 }
}

// ── CPU topology (shared with lpm-gamemode) ──────────────────────────────

/// "0-3,8-11" -> [0,1,2,3,8,9,10,11] (sorted, deduplicated).
pub fn cpu_list(s: &str) -> Vec<usize> {
    let mut v: Vec<usize> = s.split(',').flat_map(|part| {
        let mut it = part.trim().splitn(2, '-');
        let lo: Option<usize> = it.next().and_then(|x| x.parse().ok());
        let hi: Option<usize> = it.next().map_or(lo, |x| x.parse().ok());
        match (lo, hi) { (Some(a), Some(b)) if b >= a && b - a < 4096 => (a..=b).collect(), _ => vec![] }
    }).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// [0,1,2,3,8] -> "0-3,8"
pub fn fmt_cpu_list(cpus: &[usize]) -> String {
    let mut v = cpus.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 { j += 1; }
        out.push(if i == j { v[i].to_string() } else { format!("{}-{}", v[i], v[j]) });
        i = j + 1;
    }
    out.join(",")
}

/// Kernel bitmap text: 32-bit hex words, most significant first, comma separated.
pub fn cpumask_hex(cpus: &[usize]) -> String {
    let n = cpus.iter().copied().max().map_or(1, |m| m / 32 + 1);
    let mut words = vec![0u32; n];
    for &c in cpus { words[c / 32] |= 1 << (c % 32); }
    words.iter().rev().enumerate()
        .map(|(i, w)| if i == 0 { format!("{w:x}") } else { format!("{w:08x}") })
        .collect::<Vec<_>>().join(",")
}

pub fn parse_cpumask(s: &str) -> Vec<usize> {
    let hex: String = s.trim().chars().filter(|c| *c != ',').collect();
    let mut out = Vec::new();
    for (i, ch) in hex.chars().rev().enumerate() {
        let Some(d) = ch.to_digit(16) else { return vec![] };
        for b in 0..4 { if d & (1 << b) != 0 { out.push(i * 4 + b as usize); } }
    }
    out.sort_unstable();
    out
}

pub fn size_kib(s: &str) -> u64 {
    let (n, mul) = match s.chars().last() { Some('K') => (&s[..s.len() - 1], 1), Some('M') => (&s[..s.len() - 1], 1024), _ => (s, 1) };
    n.parse::<u64>().unwrap_or(0) * mul
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ccx { pub index: usize, pub cpus: Vec<usize>, pub l3_kib: u64, pub max_khz: u64 }

/// Short-lived cache of [`ccx_groups`]. One `describe` asks for the topology
/// dozens of times (files/options/current of every per-CCD, affinity, IRQ
/// and park row), each a walk over ~5 sysfs files per CPU. The cache lives
/// for a fraction of a second and is dropped by every CPU hot-plug write, so
/// long-running callers (lpm-gamemode polling for SMT/CCD parking to settle)
/// still see the live topology.
const TOPO_TTL: std::time::Duration = std::time::Duration::from_millis(250);
thread_local! {
    static TOPO: std::cell::RefCell<Option<(std::time::Instant, Vec<Ccx>)>> = const { std::cell::RefCell::new(None) };
    static POLICIES: std::cell::RefCell<Option<(std::time::Instant, Vec<PathBuf>)>> = const { std::cell::RefCell::new(None) };
}

/// Forget the cached topology (after writing cpu*/online or smt/control).
pub fn invalidate_topology() {
    TOPO.with(|t| *t.borrow_mut() = None);
    POLICIES.with(|t| *t.borrow_mut() = None);
}

/// L3 domains of the online CPUs, sorted by first CPU. A parked CCD has no
/// online CPU and does not appear.
pub fn ccx_groups() -> Vec<Ccx> {
    if let Some(g) = TOPO.with(|t| t.borrow().as_ref().filter(|(at, _)| at.elapsed() < TOPO_TTL).map(|(_, g)| g.clone())) {
        return g;
    }
    let g = ccx_groups_uncached();
    TOPO.with(|t| *t.borrow_mut() = Some((std::time::Instant::now(), g.clone())));
    g
}

fn ccx_groups_uncached() -> Vec<Ccx> {
    // Built from online CPUs only, and groups whose L3 lists overlap are merged.
    // Deduplicating by the raw shared_cpu_list string is not enough: while SMT
    // or CCD parking hot-plugs CPUs, one CPU can still report "0-7,16-23" and
    // the next "0-7" for the same L3 — two "CCDs" with the same cache size, so
    // resolve_ccd("cache") saw a tie and the game ran unpinned on every CPU.
    let online = online_cpus();
    let is_online = |c: &usize| online.is_empty() || online.contains(c);
    let mut l3 = Vec::new();
    for (n, d) in cpus() {
        if !is_online(&(n as usize)) { continue; }
        let idx = d.join("cache/index3");
        if read(&idx.join("level")).as_deref() != Some("3") { continue; }
        let Some(list) = read(&idx.join("shared_cpu_list")) else { continue };
        l3.push((n as usize, cpu_list(&list), read(&idx.join("size")).map_or(0, |s| size_kib(&s))));
    }
    let mut out = group_l3(l3, &online);
    for g in &mut out {
        g.max_khz = g.cpus.iter()
            .filter_map(|c| read(&Path::new(CPU_DIR).join(format!("cpu{c}/cpufreq/cpuinfo_max_freq"))))
            .filter_map(|s| s.parse().ok()).max().unwrap_or(0);
    }
    out.sort_by_key(|g| g.cpus.first().copied().unwrap_or(0));
    for (i, g) in out.iter_mut().enumerate() { g.index = i; }
    out
}
/// (cpu, its L3 shared_cpu_list, L3 KiB) per online CPU -> merged L3 domains.
fn group_l3(entries: Vec<(usize, Vec<usize>, u64)>, online: &[usize]) -> Vec<Ccx> {
    let mut out: Vec<Ccx> = Vec::new();
    for (n, list, l3_kib) in entries {
        let mut cpus: Vec<usize> = list.into_iter().filter(|c| online.is_empty() || online.contains(c)).collect();
        if !cpus.contains(&n) { cpus.push(n); }
        // A CPU list can bridge two existing groups only transiently; absorb all it touches.
        let (hit, mut rest): (Vec<Ccx>, Vec<Ccx>) = out.into_iter().partition(|g| g.cpus.iter().any(|c| cpus.contains(c)));
        let mut merged = Ccx { index: 0, cpus, l3_kib, max_khz: 0 };
        for g in hit { merged.cpus.extend(g.cpus); merged.l3_kib = merged.l3_kib.max(g.l3_kib); }
        merged.cpus.sort_unstable();
        merged.cpus.dedup();
        rest.push(merged);
        out = rest;
    }
    out
}

pub fn resolve_ccd(groups: &[Ccx], role: &str) -> Option<Ccx> {
    match role {
        "pcore" => return hybrid_group(CoreType::P),
        "ecore" => return hybrid_group(CoreType::E),
        _ => {}
    }
    if groups.len() < 2 { return None; }
    let pick = match role {
        "cache" => {
            let best = groups.iter().max_by_key(|c| c.l3_kib)?;
            if groups.iter().filter(|c| c.l3_kib == best.l3_kib).count() > 1 { return None; }
            best
        }
        "frequency" => {
            let best = groups.iter().max_by_key(|c| c.max_khz)?;
            let tied: Vec<&Ccx> = groups.iter().filter(|c| c.max_khz == best.max_khz).collect();
            if tied.len() == 1 { best } else {
                // Same advertised max: the non-V-Cache die clocks higher in practice.
                let small = *tied.iter().min_by_key(|c| c.l3_kib)?;
                if tied.iter().filter(|c| c.l3_kib == small.l3_kib).count() > 1 { return None; }
                small
            }
        }
        m => groups.get(m.strip_prefix("ccd")?.parse::<usize>().ok()?)?,
    };
    Some(pick.clone())
}

fn role_options(groups: &[Ccx], park: bool) -> Vec<(String, String)> {
    let usable = |g: &Ccx| !park || !g.cpus.contains(&0);
    // One entry per CCD, named by its role. The old separate "cache" /
    // "frequency" entries duplicated these and re-resolved the role at every
    // apply / read-back; the CCD index is stable, so it is the value now
    // ("cache"/"frequency" are still accepted and mapped, see canonical_ccd).
    let mut v = Vec::new();
    if groups.len() >= 2 {
        let (cache, freq) = (resolve_ccd(groups, "cache").map(|g| g.index), resolve_ccd(groups, "frequency").map(|g| g.index));
        for g in groups.iter().filter(|g| usable(g)) {
            let role = if Some(g.index) == cache { " V-Cache" } else if Some(g.index) == freq { " frequency" } else { "" };
            v.push((format!("ccd{}", g.index), format!("CCD{}{role} ({}, {} MB L3)", g.index, fmt_cpu_list(&g.cpus), g.l3_kib / 1024)));
        }
    }
    for (role, what, ct) in [("pcore", "P-cores", CoreType::P), ("ecore", "E-cores", CoreType::E)] {
        if let Some(g) = hybrid_group(ct).filter(|g| usable(g)) {
            v.push((role.to_owned(), format!("{what} ({})", fmt_cpu_list(&g.cpus))));
        }
    }
    v
}

// ── CCD park record ──────────────────────────────────────────────────────
// Offline CPUs lose their cache/topology directories, so while a CCD is
// parked it cannot be recognised from sysfs any more: the option list shrank
// to "none", the current value read "offline 16-31", and a preset saved or
// loaded in that state silently lost the setting. tune-helper records which
// role it parked; the record counts only while exactly those CPUs are offline.

pub const CCD_PARK_RECORD: &str = "/run/legion-power-manager/tune/ccd-park.json";

pub fn offline_cpus() -> Vec<usize> {
    let online = online_cpus();
    present_cpus().into_iter().filter(|c| !online.contains(c)).collect()
}

/// (role, option label) of the parked CCD, if the record matches reality.
fn parked_record() -> Option<(String, String, Vec<usize>)> {
    let v: serde_json::Value = serde_json::from_str(&crate::read_root_file(CCD_PARK_RECORD, 4096)?).ok()?;
    match_park_record(&v, &offline_cpus())
}

/// The record describes the current state only if exactly its CPUs are offline.
fn match_park_record(v: &serde_json::Value, offline: &[usize]) -> Option<(String, String, Vec<usize>)> {
    let cpus: Vec<usize> = v["cpus"].as_array()?.iter().filter_map(|x| x.as_u64().map(|n| n as usize)).collect();
    // Valid while every parked CPU is still offline (SMT may add more offline CPUs).
    if cpus.is_empty() || !cpus.iter().all(|c| offline.contains(c)) { return None; }
    let role = v["role"].as_str()?.to_owned();
    let label = v["label"].as_str().map(str::to_owned).unwrap_or_else(|| format!("park {role}"));
    Some((role, label, cpus))
}

/// Called by tune-helper (root) after a successful park.
pub fn record_ccd_park(role: &str, label: Option<&str>, cpus: &[usize]) -> Result<(), String> {
    if role == "none" {
        let _ = std::fs::remove_file(CCD_PARK_RECORD);
        return Ok(());
    }
    // Only the CPUs this park took offline: with SMT off, offline_cpus() also
    // holds the other CCD's sibling threads, which "none" must not bring back.
    let cpus: Vec<usize> = if cpus.is_empty() { offline_cpus() } else { cpus.to_vec() };
    let body = serde_json::json!({"role": role, "label": label, "cpus": cpus});
    crate::write_root_file(CCD_PARK_RECORD, body.to_string().as_bytes())
}

pub fn possible_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("possible")).map(|s| cpu_list(&s)).unwrap_or_default() }
pub fn present_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("present")).map(|s| cpu_list(&s)).unwrap_or_default() }
pub fn online_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("online")).map(|s| cpu_list(&s)).unwrap_or_default() }

// ── nvidia-powerd guard ──────────────────────────────────────────────────
// nvidia-powerd (Dynamic Boost) walks every CPU's cpuid/cpufreq data and does
// not survive hot-unplugged CPUs ("malformed CPU data" / cpuid_error, NVIDIA
// bug 4782702). Its PMGR control on Blackwell laptops then hangs the GSP as
// soon as a game loads the GPU: Xid 79/119 -> Xid 154 "reboot required".
// So it is stopped before any CPU goes offline and started again once the
// offline set is back to what it was before (SMT/park both count).

pub const POWERD_MARK: &str = "/run/legion-power-manager/tune/powerd-paused.json";

fn powerd_pids() -> Vec<i32> {
    std::fs::read_dir("/proc").into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| read(Path::new(&format!("/proc/{pid}/comm"))).as_deref() == Some("nvidia-powerd"))
        .collect()
}

fn powerd_wait(running: bool, ms: u64) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        if powerd_pids().is_empty() != running { return true; }
        if std::time::Instant::now() >= end { return false; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// `systemctl <action> nvidia-powerd` / `rc-service nvidia-powerd <action>`.
fn powerd_service(action: &str) -> bool {
    let bin = |c: &[&str]| c.iter().map(PathBuf::from).find(|p| crate::trusted_path(p));
    let (bin, args) = if Path::new("/run/systemd/system").is_dir() {
        (bin(&["/usr/bin/systemctl", "/bin/systemctl"]), [action, "nvidia-powerd.service"])
    } else if Path::new("/run/openrc").is_dir() {
        (bin(&["/sbin/rc-service", "/usr/sbin/rc-service", "/bin/rc-service", "/usr/bin/rc-service"]), ["nvidia-powerd", action])
    } else { return false };
    bin.and_then(|b| run_tool(&b, &args, std::time::Duration::from_secs(15))).map_or(false, |(ok, _)| ok)
}

/// `systemctl <action> <name>.service` / `rc-service <name> <action>` (name must be a plain daemon name).
pub fn service_ctl(name: &str, action: &str) -> bool {
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || !matches!(action, "start" | "stop") { return false; }
    let bin = |c: &[&str]| c.iter().map(PathBuf::from).find(|p| crate::trusted_path(p));
    let unit = format!("{name}.service");
    let t = std::time::Duration::from_secs(15);
    if Path::new("/run/systemd/system").is_dir() {
        bin(&["/usr/bin/systemctl", "/bin/systemctl"]).and_then(|b| run_tool(&b, &[action, &unit], t)).map_or(false, |(ok, _)| ok)
    } else if Path::new("/run/openrc").is_dir() {
        bin(&["/sbin/rc-service", "/usr/sbin/rc-service", "/bin/rc-service", "/usr/bin/rc-service"]).and_then(|b| run_tool(&b, &[name, action], t)).map_or(false, |(ok, _)| ok)
    } else { false }
}

/// True for a hot-plug write that takes CPUs offline (park "0", SMT "off").
pub fn offlines_cpus(key: &str, data: &str) -> bool {
    is_hotplug(key) && matches!(data, "0" | "off" | "forceoff")
}

/// Shutdown/reboot in progress: never (re)start a daemon then.
fn system_stopping() -> bool {
    if let Some(l) = read(Path::new("/run/openrc/softlevel")) { return l == "shutdown" || l == "reboot"; }
    if Path::new("/run/systemd/system").is_dir() {
        let bin = ["/usr/bin/systemctl", "/bin/systemctl"].iter().map(PathBuf::from).find(|p| crate::trusted_path(p));
        return bin.and_then(|b| run_tool(&b, &["is-system-running"], std::time::Duration::from_secs(5)))
            .map_or(false, |(_, out)| out.trim() == "stopping");
    }
    false
}

fn powerd_mark(exe: &str, via: &str, offline: &[usize]) -> Result<(), String> {
    crate::write_root_file(POWERD_MARK, json!({"exe": exe, "via": via, "offline": offline}).to_string().as_bytes())
}

/// Call before writes that take CPUs offline. Ok(true): it was running and is
/// stopped now. Err: still running, the caller must not offline anything.
pub fn powerd_pause() -> Result<bool, String> {
    let pids = powerd_pids();
    if pids.is_empty() { return Ok(false); }
    let exe = std::fs::read_link(format!("/proc/{}/exe", pids[0])).map(|p| p.display().to_string()).unwrap_or_default();
    let offline = offline_cpus();
    // Marked before it is touched: a powerd stopped without a mark would never come back.
    powerd_mark(&exe, "exe", &offline)?;
    // Service first, so a supervisor does not respawn it; plain signals otherwise.
    if powerd_service("stop") && powerd_wait(false, 3000) {
        powerd_mark(&exe, "service", &offline)?;
        return Ok(true);
    }
    for pid in powerd_pids() { unsafe { libc::kill(pid, libc::SIGTERM); } }
    if powerd_wait(false, 3000) { return Ok(true); }
    for pid in powerd_pids() { unsafe { libc::kill(pid, libc::SIGKILL); } }
    if powerd_wait(false, 2000) { return Ok(true); }
    let _ = std::fs::remove_file(POWERD_MARK);
    Err("nvidia-powerd could not be stopped".into())
}

/// Call after hot-plug writes. Restarts nvidia-powerd once no CPU beyond the
/// ones already offline at pause time is offline. Ok(true) = restarted.
pub fn powerd_resume() -> Result<bool, String> {
    let Some(v) = crate::read_root_file(POWERD_MARK, 4096).and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { return Ok(false) };
    let before: Vec<usize> = v["offline"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as usize)).collect()).unwrap_or_default();
    if !offline_cpus().iter().all(|c| before.contains(c)) || system_stopping() { return Ok(false); }
    let _ = std::fs::remove_file(POWERD_MARK);
    if !powerd_pids().is_empty() { return Ok(false); }
    if v["via"] == "service" {
        return if powerd_service("start") && powerd_wait(true, 3000) { Ok(true) } else { Err("nvidia-powerd service did not start again".into()) };
    }
    // Not started by a service manager we know: relaunch the same binary, detached.
    let exe = PathBuf::from(v["exe"].as_str().unwrap_or(""));
    if !exe.is_absolute() || !crate::trusted_path(&exe) { return Err("nvidia-powerd could not be restarted (untrusted or unknown binary)".into()); }
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&exe);
    cmd.env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").current_dir("/")
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe { cmd.pre_exec(|| { libc::setsid(); Ok(()) }); }
    cmd.spawn().map_err(|e| format!("nvidia-powerd: {e}"))?;
    Ok(true)
}

/// Legacy role names ("cache"/"frequency", older presets and park records)
/// mapped onto the CCD index they resolve to here; anything else unchanged.
pub fn canonical_ccd(v: &str) -> String {
    if v == "cache" || v == "frequency" {
        if let Some(g) = resolve_ccd(&ccx_groups(), v) { return format!("ccd{}", g.index); }
    }
    v.to_owned()
}

/// Which role option describes this CPU set, if any.
fn role_matching(groups: &[Ccx], set: &[usize]) -> Option<String> {
    if groups.len() >= 2 {
        if let Some(g) = groups.iter().find(|g| g.cpus == set) { return Some(format!("ccd{}", g.index)); }
    }
    for role in ["pcore", "ecore"] {
        if resolve_ccd(groups, role).map_or(false, |g| g.cpus == set) { return Some(role.into()); }
    }
    groups.iter().find(|g| g.cpus == set).map(|g| format!("ccd{}", g.index))
}

// ── PCI latency timer ────────────────────────────────────────────────────

const PCI_DEVICES: &str = "/sys/bus/pci/devices";
const PCI_LATENCY_OFFSET: u64 = 0x0D;

/// PCI devices whose config space / ASPM / power state must not be written by a
/// tuning knob: every display controller, the other functions of its slot (HDA,
/// USB-C) and every bridge above them. On a laptop whose dGPU is cut off (iGPU
/// only, firmware-disabled) or runtime-suspended, a config write resumes the
/// device through ACPI and the writing process can sit in D state for good.
fn gpu_tree() -> Vec<PathBuf> {
    let devs: Vec<PathBuf> = std::fs::read_dir(PCI_DEVICES).into_iter().flatten().flatten()
        .filter_map(|e| std::fs::canonicalize(e.path()).ok())
        .filter(|p| p.starts_with("/sys/devices"))
        .collect();
    let display: Vec<&PathBuf> = devs.iter().filter(|d| read(&d.join("class")).map_or(false, |c| c.starts_with("0x03"))).collect();
    let slot = |d: &Path| d.file_name().map(|n| n.to_string_lossy().rsplit_once('.').map_or(String::new(), |x| x.0.to_owned()));
    let slots: Vec<Option<String>> = display.iter().map(|d| slot(d)).collect();
    devs.iter()
        .filter(|d| slots.contains(&slot(d)) || display.iter().any(|g| g.starts_with(d.as_path())))
        .cloned().collect()
}

/// True if a write to a file inside `dev` may wake a sleeping device: the device
/// (or, for a bridge attribute, the device itself) is not fully active.
fn pci_unsafe_to_touch(file: &Path, tree: &[PathBuf], need_active: bool) -> bool {
    let Some(mut dev) = file.parent() else { return true };
    // <dev>/link/l1_aspm and <dev>/power/control belong to <dev>; <dev>/config is <dev>'s own.
    if dev.file_name().map_or(false, |n| n == "link" || n == "power") { dev = dev.parent().unwrap_or(dev); }
    if tree.iter().any(|t| t == dev) { return true; }
    need_active && !matches!(read(&dev.join("power/runtime_status")).as_deref(), None | Some("active"))
}

fn pci_config_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(PCI_DEVICES).into_iter().flatten().flatten()
        .filter_map(|e| canonical_in_sysfs(&e.path().join("config")))
        .filter(|p| is_pci_config(p))
        .collect();
    v.sort();
    v
}

/// Config files a *write* may touch (see gpu_tree).
fn pci_config_files_writable() -> Vec<PathBuf> {
    let tree = gpu_tree();
    pci_config_files().into_iter().filter(|p| !pci_unsafe_to_touch(p, &tree, true)).collect()
}

fn is_pci_config(p: &Path) -> bool {
    p.file_name().map_or(false, |n| n == "config") && p.starts_with("/sys/devices")
        && p.parent().map_or(false, |d| d.join("vendor").is_file() && d.join("class").is_file())
}

/// A runtime-suspended function (D3hot/D3cold: the dGPU, its audio function,
/// an idle NVMe/Wi-Fi) must not be read for the status display: sysfs config
/// reads go through pci_config_pm_runtime_get(), which resumes a D3cold device
/// and its parent bridge — the 4 s describe poll kept the RTX dGPU from ever
/// staying asleep while the Optimizations tab was open.
fn pci_runtime_suspended(cfg: &Path) -> bool {
    cfg.parent().and_then(|d| read(&d.join("power/runtime_status"))).as_deref() == Some("suspended")
}

fn pci_latency_read(cfg: &Path) -> Option<u8> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(cfg).ok()?;
    let mut b = [0u8; 1];
    (f.read_at(&mut b, PCI_LATENCY_OFFSET).ok()? == 1).then_some(b[0])
}

fn pci_latency_target(cfg: &Path) -> u8 {
    let class = cfg.parent().and_then(|d| read(&d.join("class"))).unwrap_or_default();
    if class.starts_with("0x0600") { 0x00 } else if class.starts_with("0x0604") { 0x80 } else { 0x20 }
}

/// Exactly one byte at 0x0D — never a full config-space write.
fn pci_latency_write(cfg: &Path, hex: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    let v = u8::from_str_radix(hex.trim(), 16)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad latency byte"))?;
    let f = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(cfg)?;
    match f.write_at(&[v], PCI_LATENCY_OFFSET)? {
        1 => Ok(()),
        _ => Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "short write")),
    }
}

// ── discovery for the other targets ──────────────────────────────────────

pub fn block_devs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir("/sys/block").into_iter().flatten().flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            ["nvme", "sd", "mmcblk", "vd"].iter().any(|p| n.starts_with(p))
        })
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn amdgpu_dpm_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = numbered(Path::new("/sys/class/drm"), "card").into_iter()
        .map(|(_, p)| p.join("device"))
        .filter(|d| read(&d.join("vendor")).as_deref() == Some("0x1002"))
        .map(|d| d.join("power_dpm_force_performance_level"))
        .filter(|p| p.is_file())
        .filter_map(|p| canonical_in_sysfs(&p))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn uncore_files(f: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(Path::new(CPU_DIR).join("intel_uncore_frequency")).into_iter().flatten().flatten()
        .map(|e| e.path().join(f))
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v
}

const POWERCAP: &str = "/sys/class/powercap";

/// Package-domain constraint_N_power_limit_uw files whose constraint_N_name is `name`.
fn rapl_files(name: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(POWERCAP).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let top = (n.starts_with("intel-rapl:") || n.starts_with("intel-rapl-mmio:")) && n.matches(':').count() == 1;
        if !top || !read(&e.path().join("name")).map_or(false, |x| x.starts_with("package")) { continue; }
        for i in 0..8 {
            if read(&e.path().join(format!("constraint_{i}_name"))).as_deref() == Some(name) {
                let f = e.path().join(format!("constraint_{i}_power_limit_uw"));
                if f.is_file() { if let Some(c) = canonical_in_sysfs(&f) { v.push(c); } }
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

fn tcc_files() -> Vec<PathBuf> {
    numbered(Path::new("/sys/class/thermal"), "cooling_device").into_iter()
        .filter(|(_, d)| read(&d.join("type")).as_deref() == Some("TCC Offset"))
        .map(|(_, d)| d.join("cur_state"))
        .filter(|p| p.is_file())
        .filter_map(|p| canonical_in_sysfs(&p))
        .collect()
}

fn intel_gt_files(i915: &str, xe: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for (_, card) in numbered(Path::new("/sys/class/drm"), "card") {
        if read(&card.join("device/vendor")).as_deref() != Some("0x8086") { continue; }
        for (_, gt) in numbered(&card.join("gt"), "gt") { v.push(gt.join(i915)); }
        if !xe.is_empty() {
            for (_, tile) in numbered(&card.join("device"), "tile") {
                for (_, gt) in numbered(&tile, "gt") { v.push(gt.join("freq0").join(xe)); }
            }
        }
    }
    let mut v: Vec<PathBuf> = v.into_iter().filter(|p| p.is_file()).filter_map(|p| canonical_in_sysfs(&p)).collect();
    v.sort();
    v.dedup();
    v
}

/// A kernel module is usable: loaded, built in, or present as a loadable
/// module for the running kernel (writing the sysctl autoloads it).
pub fn kmod_available(module: &str, subdir: &str) -> bool {
    if Path::new("/sys/module").join(module).exists() { return true; }
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 { return false; }
    let rel = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) }.to_string_lossy().into_owned();
    let base = Path::new("/lib/modules").join(rel);
    let ko = format!("{module}.ko");
    if std::fs::read_to_string(base.join("modules.builtin")).map_or(false, |b| b.lines().any(|l| l.ends_with(&format!("/{ko}")))) {
        return true;
    }
    std::fs::read_dir(base.join("kernel").join(subdir)).into_iter().flatten().flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with(&ko))  // .ko, .ko.xz, .ko.zst
}

/// intel_pstate/no_turbo is the inverse of "boost".
/// no_turbo speaks the opposite of "boost", rfkill `soft` the opposite of "radio on".
fn inverted(f: &Path) -> bool { f.file_name().map_or(false, |n| n == "no_turbo" || n == "soft") }

fn irq_files() -> Vec<PathBuf> {
    numbered(Path::new("/proc/irq"), "").into_iter()
        .map(|(_, p)| p.join("smp_affinity_list"))
        .filter(|p| p.is_file())
        .collect()
}

fn is_irq_file(p: &Path) -> bool {
    p.to_str().and_then(|s| s.strip_prefix("/proc/irq/")).and_then(|r| r.strip_suffix("/smp_affinity_list"))
        .map_or(false, |n| !n.is_empty() && n.len() < 8 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Per-file refusals do not fail the knob: kernel-managed IRQs return EIO,
/// and some PCI functions (or a locked-down kernel) refuse config writes —
/// lutris-game-tune ran setpci with `|| true` for the same reason.
/// The kernel refuses pcie_aspm's policy with EPERM when ASPM control was
/// not granted to the OS (FADT "ASPM not supported" or _OSC denied).
pub fn firmware_owned(t: &Tunable, err: &str) -> bool {
    matches!(t.target, Target::File(p) if p.ends_with("pcie_aspm/parameters/policy")) && err.contains("os error 1)")
}

pub fn best_effort(t: &Tunable) -> bool {
    matches!(t.target, Target::Irq | Target::PciLatency | Target::PciAspm | Target::PciRuntimePm | Target::UsbRuntimePm | Target::UsbAutosuspendMs | Target::AhciPortRuntime | Target::AhciDisk(_))
}

// ── command-backed targets (Wi-Fi power save, sched_ext) ─────────────────

/// Runs a root-trusted binary with a clean environment and a hard timeout.
/// Returns (success, stdout). Never a shell; stdin/stderr closed.
fn run_tool(bin: &Path, args: &[&str], timeout: std::time::Duration) -> Option<(bool, String)> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(bin).args(args).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C").current_dir("/").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().ok()?;
    let mut out = child.stdout.take()?;
    // The reader hands over the output at stdout EOF (the tool exited or closed
    // it); the caller waits on that with a timeout instead of polling try_wait()
    // every 10 ms — `iw … get power_save` answers in ~1 ms, so every describe
    // used to pay a 10 ms floor per Wi-Fi interface.
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = (&mut out).take(64 * 1024).read_to_end(&mut v);
        let _ = tx.send(v);
    });
    let Ok(bytes) = rx.recv_timeout(timeout) else { let _ = child.kill(); let _ = child.wait(); return None; };
    // stdout closed; the exit follows at once (or it closed stdout early and hangs: bounded).
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(10)),
            _ => { let _ = child.kill(); let _ = child.wait(); return None; }
        }
    };
    Some((status.success(), String::from_utf8_lossy(&bytes).into_owned()))
}

fn iw() -> Option<PathBuf> {
    ["/usr/sbin/iw", "/sbin/iw", "/usr/bin/iw", "/bin/iw"].iter().map(PathBuf::from)
        .find(|p| crate::trusted_path(p))
}

/// Interface name from a "/sys/class/net/<if>" target path (IFNAMSIZ, no path tricks).
fn wifi_ifname(f: &Path) -> Option<String> {
    let n = f.to_str()?.strip_prefix("/sys/class/net/")?;
    (!n.is_empty() && n.len() <= 15 && n != "." && n != ".."
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))).then(|| n.to_owned())
}

fn wifi_ifaces() -> Vec<PathBuf> {
    if iw().is_none() { return vec![]; }
    let mut v: Vec<PathBuf> = std::fs::read_dir("/sys/class/net").into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| p.join("wireless").is_dir() || p.join("phy80211").exists())
        .filter(|p| wifi_ifname(p).is_some())
        .collect();
    v.sort();
    v
}

/// "1"/"0" as iw reports it ("Power save: on").
fn wifi_get(f: &Path) -> Option<String> {
    let dev = wifi_ifname(f)?;
    let (ok, out) = run_tool(&iw()?, &["dev", &dev, "get", "power_save"], std::time::Duration::from_secs(3))?;
    if !ok { return None; }
    let v = out.split(':').nth(1)?.trim().to_ascii_lowercase();
    match v.as_str() { "on" => Some("1".into()), "off" => Some("0".into()), _ => None }
}

fn wifi_set(f: &Path, data: &str) -> Result<(), String> {
    let dev = wifi_ifname(f).ok_or_else(|| format!("{}: not a network interface path", f.display()))?;
    let v = match data { "1" => "on", "0" => "off", _ => return Err(format!("power save takes 0/1, got '{data}'")) };
    let bin = iw().ok_or("iw not found (install net-wireless/iw)")?;
    match run_tool(&bin, &["dev", &dev, "set", "power_save", v], std::time::Duration::from_secs(3)) {
        Some((true, _)) => Ok(()),
        Some((false, _)) => Err(format!("{dev}: iw refused power_save {v} (interface down?)")),
        None => Err(format!("{dev}: iw timed out")),
    }
}

fn ethtool() -> Option<PathBuf> {
    ["/usr/sbin/ethtool", "/sbin/ethtool", "/usr/bin/ethtool", "/bin/ethtool"].iter().map(PathBuf::from)
        .find(|p| crate::trusted_path(p))
}

/// Physical wired interfaces (a `device` link, ARPHRD_ETHER, not wireless, not virtual).
fn eth_ifaces() -> Vec<PathBuf> {
    if ethtool().is_none() { return vec![]; }
    let mut v: Vec<PathBuf> = std::fs::read_dir("/sys/class/net").into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| p.join("device").exists() && !p.join("wireless").is_dir() && !p.join("phy80211").exists())
        .filter(|p| read(&p.join("type")).as_deref() == Some("1"))
        .filter(|p| wifi_ifname(p).is_some())
        .filter(|p| std::fs::canonicalize(p).map_or(false, |c| !c.starts_with("/sys/devices/virtual")))
        .collect();
    v.sort();
    v
}

/// ("Supports Wake-on" letters, current "Wake-on" letters) as ethtool reports them.
fn eth_wol(f: &Path) -> Option<(String, String)> {
    let dev = wifi_ifname(f)?;
    let (ok, out) = run_tool(&ethtool()?, &[&dev], std::time::Duration::from_secs(3))?;
    if !ok { return None; }
    let (mut sup, mut cur) = (None, None);
    for l in out.lines() {
        let l = l.trim();
        if let Some(v) = l.strip_prefix("Supports Wake-on:") { sup = Some(v.trim().to_owned()); }
        else if let Some(v) = l.strip_prefix("Wake-on:") { cur = Some(v.trim().to_owned()); }
    }
    Some((sup?, cur?))
}

/// "1" = any wake mode enabled, "0" = off; None when the port has no WoL at all.
fn wol_get(f: &Path) -> Option<String> {
    let (sup, cur) = eth_wol(f)?;
    if sup.chars().all(|c| c == 'd') { return None; }
    Some(if cur == "d" { "0".into() } else { "1".into() })
}

fn wol_set(f: &Path, data: &str) -> Result<(), String> {
    let dev = wifi_ifname(f).ok_or_else(|| format!("{}: not a network interface path", f.display()))?;
    let bin = ethtool().ok_or("ethtool not found (install sys-apps/ethtool)")?;
    let (sup, _) = eth_wol(f).ok_or_else(|| format!("{dev}: ethtool could not read the port"))?;
    let mode = match data {
        "0" => "d",
        "1" if sup.contains('g') => "g",
        "1" => return Err(format!("{dev}: port has no magic-packet wake")),
        _ => return Err(format!("wake-on-lan takes 0/1, got '{data}'")),
    };
    match run_tool(&bin, &["-s", &dev, "wol", mode], std::time::Duration::from_secs(3)) {
        Some((true, _)) => Ok(()),
        Some((false, _)) => Err(format!("{dev}: ethtool refused wol {mode}")),
        None => Err(format!("{dev}: ethtool timed out")),
    }
}

/// "1" = EEE enabled, "0" = disabled; None when the port/driver reports no EEE settings.
fn eee_get(f: &Path) -> Option<String> {
    let dev = wifi_ifname(f)?;
    let (ok, out) = run_tool(&ethtool()?, &["--show-eee", &dev], std::time::Duration::from_secs(3))?;
    if !ok { return None; }
    for l in out.lines() {
        if let Some(v) = l.trim().strip_prefix("EEE status:") {
            let v = v.trim();
            if v.starts_with("not supported") { return None; }
            return Some(if v.starts_with("disabled") { "0".into() } else { "1".into() });
        }
    }
    None
}

fn eee_set(f: &Path, data: &str) -> Result<(), String> {
    let dev = wifi_ifname(f).ok_or_else(|| format!("{}: not a network interface path", f.display()))?;
    let bin = ethtool().ok_or("ethtool not found (install sys-apps/ethtool)")?;
    let arg = match data {
        "0" => "off",
        "1" => "on",
        _ => return Err(format!("EEE takes 0/1, got '{data}'")),
    };
    if eee_get(f).is_none() { return Err(format!("{dev}: port reports no EEE settings")); }
    match run_tool(&bin, &["--set-eee", &dev, "eee", arg], std::time::Duration::from_secs(3)) {
        Some((true, _)) => Ok(()),
        Some((false, _)) => Err(format!("{dev}: ethtool refused eee {arg}")),
        None => Err(format!("{dev}: ethtool timed out")),
    }
}

// ── ATA disk discovery, APM (hdparm), AHCI runtime PM ───────────────────

fn hdparm() -> Option<PathBuf> {
    ["/usr/sbin/hdparm", "/sbin/hdparm", "/usr/bin/hdparm", "/bin/hdparm"].iter().map(PathBuf::from)
        .find(|p| crate::trusted_path(p))
}

/// "sda".."sdzz": the only disk names any ATA target accepts.
fn ata_disk_name_ok(n: &str) -> bool {
    n.strip_prefix("sd").map_or(false, |r| (1..=3).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_lowercase()))
}

/// The udev database entry of a block device (`E:KEY=value` lines), if readable.
fn udev_props(disk: &Path) -> Option<String> {
    let devno = read(&disk.join("dev"))?;
    std::fs::read_to_string(format!("/run/udev/data/b{devno}")).ok()
}

/// Whole ATA disks (sdX behind an ata* port; never USB, NVMe or virtio), as (name, sysfs disk dir).
fn ata_disks() -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir("/sys/block").into_iter().flatten().flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            if !ata_disk_name_ok(&n) { return None; }
            let real = canonical_in_sysfs(&e.path())?;
            let behind_ata = real.components().any(|c| {
                let c = c.as_os_str().to_string_lossy();
                c.strip_prefix("ata").map_or(false, |d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
            });
            behind_ata.then(|| (n, real))
        })
        .collect();
    v.sort();
    v
}

fn ata_disk_serial(name: &str, dir: &Path) -> String {
    udev_props(dir).and_then(|p| p.lines().find_map(|l| l.strip_prefix("E:ID_SERIAL=").map(str::to_owned)))
        .unwrap_or_else(|| name.to_owned())
}

/// APM-capable ATA disks in stable order (serial, then name).
fn apm_disks() -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, String, PathBuf)> = ata_disks().into_iter()
        .filter(|(_, d)| udev_props(d).map_or(true, |p| p.lines().any(|l| l == "E:ID_ATA_FEATURE_SET_APM=1")))
        .map(|(n, d)| (ata_disk_serial(&n, &d), n, d)).collect();
    v.sort();
    v.into_iter().map(|(_, n, d)| (n, d)).collect()
}

fn disk_apm_files(slot: usize) -> Vec<PathBuf> {
    if hdparm().is_none() { return vec![]; }
    apm_disks().into_iter().nth(slot).map(|(_, d)| d.join("dev")).filter(|p| p.is_file()).into_iter().collect()
}

/// Disk name behind a ".../block/sdX/dev" path (validated; nothing else reaches hdparm).
fn apm_dev(f: &Path) -> Option<String> {
    if f.file_name()? != "dev" || !f.starts_with("/sys/devices") { return None; }
    let n = f.parent()?.file_name()?.to_str()?;
    (ata_disk_name_ok(n) && f.parent()?.parent()?.file_name()? == "block").then(|| n.to_owned())
}

/// Live APM level (255 = off); None if unreadable (not root/disk group) or unsupported.
fn apm_get(f: &Path) -> Option<String> {
    let dev = apm_dev(f)?;
    let (ok, out) = run_tool(&hdparm()?, &["-B", &format!("/dev/{dev}")], std::time::Duration::from_secs(3))?;
    if !ok { return None; }
    let v = out.split('=').nth(1)?.trim().to_ascii_lowercase();
    if v.starts_with("off") { return Some("255".into()); }
    v.split_whitespace().next()?.parse::<u16>().ok().filter(|n| (1..=255).contains(n)).map(|n| n.to_string())
}

fn apm_set(f: &Path, data: &str) -> Result<(), String> {
    let dev = apm_dev(f).ok_or_else(|| format!("{}: not an ATA disk path", f.display()))?;
    let n: u16 = data.parse().ok().filter(|n| (1..=255).contains(n)).ok_or_else(|| format!("APM level takes 1-255, got '{data}'"))?;
    let bin = hdparm().ok_or("hdparm not found (install sys-apps/hdparm)")?;
    match run_tool(&bin, &["-B", &n.to_string(), &format!("/dev/{dev}")], std::time::Duration::from_secs(5)) {
        Some((true, _)) => Ok(()),
        Some((false, _)) => Err(format!("{dev}: hdparm refused APM level {n}")),
        None => Err(format!("{dev}: hdparm timed out")),
    }
}

fn ahci_port_files() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(PCI_DEVICES).into_iter().flatten().flatten() {
        let Ok(dev) = std::fs::canonicalize(e.path()) else { continue };
        if !dev.starts_with("/sys/devices") { continue; }
        for p in std::fs::read_dir(&dev).into_iter().flatten().flatten() {
            let n = p.file_name().to_string_lossy().into_owned();
            if !n.strip_prefix("ata").map_or(false, |d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())) { continue; }
            if let Some(c) = canonical_in_sysfs(&p.path().join("power/control")) { if c.is_file() { v.push(c); } }
        }
    }
    v.sort();
    v.dedup();
    v
}

/// device/power/<attr> of every ATA disk that exposes an autosuspend delay (TLP's runpm==0 rule).
fn ahci_disk_files(attr: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ata_disks().into_iter()
        .filter(|(_, d)| d.join("device/power/autosuspend_delay_ms").is_file())
        .filter_map(|(_, d)| canonical_in_sysfs(&d.join("device").join(attr)))
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v
}

/// Row label for the GUI: the per-disk APM rows name their drive.
pub fn row_label(t: &Tunable) -> String {
    if let Target::DiskApm(i) = t.target {
        if let Some((n, d)) = apm_disks().into_iter().nth(i) {
            let model = read(&d.join("device/model")).unwrap_or_default();
            return format!("Disk APM level · {n}{}", if model.is_empty() { String::new() } else { format!(" ({model})") });
        }
    }
    t.label.to_owned()
}

// sched_ext: only these scheduler binaries are ever started, and only from
// root-owned system directories (checked on the whole path).
const SCX_NAMES: &[&str] = &["lavd", "bpfland", "rusty", "flash", "cosmos", "p2dq", "tickless", "layered"];
const SCX_DIRS: &[&str] = &["/usr/bin", "/usr/sbin", "/usr/local/bin", "/usr/local/sbin", "/bin", "/sbin"];
const SCX_STATE: &str = "/sys/kernel/sched_ext/state";

fn scx_bin(name: &str) -> Option<PathBuf> {
    if !SCX_NAMES.contains(&name) { return None; }
    SCX_DIRS.iter().map(|d| Path::new(d).join(format!("scx_{name}"))).find(|p| crate::trusted_path(p))
}

fn scx_available() -> Vec<&'static str> {
    if !Path::new(SCX_STATE).is_file() { return vec![]; }
    SCX_NAMES.iter().copied().filter(|n| scx_bin(n).is_some()).collect()
}

/// Name of the running scheduler ("none" when EEVDF is in charge).
fn scx_current() -> Option<String> {
    let st = read(Path::new(SCX_STATE))?;
    if st != "enabled" { return Some("none".into()); }
    let ops = read(Path::new("/sys/kernel/sched_ext/root/ops")).unwrap_or_default();
    // ops is the BPF scheduler's own name ("lavd", "bpfland"…), sometimes with a suffix.
    Some(SCX_NAMES.iter().find(|n| ops == **n || ops.starts_with(&format!("{n}_"))).map_or(ops.clone(), |n| n.to_string()))
}

/// PIDs of running processes whose executable is one of our scx binaries.
fn scx_pids() -> Vec<i32> {
    let bins: Vec<PathBuf> = SCX_NAMES.iter().filter_map(|n| scx_bin(n)).filter_map(|p| std::fs::canonicalize(p).ok()).collect();
    std::fs::read_dir("/proc").into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).map_or(false, |x| bins.contains(&x)))
        .collect()
}

fn scx_wait(enabled: bool, secs: u64) -> bool {
    let want = if enabled { "enabled" } else { "disabled" };
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        if read(Path::new(SCX_STATE)).as_deref() == Some(want) { return true; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// Game mode's "soft" CCD park: a preset's `cpu.ccd_park` becomes IRQ / workqueue routing onto that CCD
/// (nothing is hot-unplugged). Returns the CCD role. Shared by lpm-gamemode and tune-profile-helper, so the
/// helper can do it on the values it takes from the approved store.
pub fn soft_park_values(vals: &mut Map<String, Value>) -> Option<String> {
    let role = vals.get("cpu.ccd_park")?.as_str().filter(|r| *r != "none")?.to_owned();
    resolve_ccd(&ccx_groups(), &role)?;
    vals.remove("cpu.ccd_park");
    for k in ["irq.affinity", "wq.cpumask"] { vals.entry(k).or_insert_with(|| json!(role)); }
    Some(role)
}

fn scx_stop() -> Result<(), String> {
    for pid in scx_pids() { unsafe { libc::kill(pid, libc::SIGINT); } }  // scx tools detach cleanly on SIGINT
    if scx_wait(false, 3) || read(Path::new(SCX_STATE)).as_deref() != Some("enabled") { return Ok(()); }
    for pid in scx_pids() { unsafe { libc::kill(pid, libc::SIGKILL); } }
    if scx_wait(false, 3) { Ok(()) } else { Err("sched_ext scheduler did not stop".into()) }
}

fn scx_set(data: &str) -> Result<(), String> {
    if scx_current().as_deref() == Some(data) { return Ok(()); }
    scx_stop()?;
    if data == "none" { return Ok(()); }
    let bin = scx_bin(data).ok_or_else(|| format!("scx_{data} not installed in a system directory"))?;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    // Detached: its own session, no inherited pipes, so it outlives this
    // helper and pkexec. Restore (or game-mode release) stops it again.
    let mut cmd = Command::new(&bin);
    cmd.env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").current_dir("/")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe { cmd.pre_exec(|| { libc::setsid(); Ok(()) }); }
    cmd.spawn().map_err(|e| format!("scx_{data}: {e}"))?;
    if scx_wait(true, 5) { Ok(()) } else { let _ = scx_stop(); Err(format!("scx_{data} did not attach within 5 s")) }
}

// ── PCIe per-link ASPM ───────────────────────────────────────────────────

const ASPM_FILES: &[&str] = &["l1_aspm", "l1_1_aspm", "l1_2_aspm"];

fn pci_aspm_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let tree = gpu_tree();
    for e in std::fs::read_dir(PCI_DEVICES).into_iter().flatten().flatten() {
        let Ok(dev) = std::fs::canonicalize(e.path()) else { continue };
        if pci_unsafe_to_touch(&dev.join("link").join("l1_aspm"), &tree, true) { continue; }
        for f in ASPM_FILES {
            if let Some(c) = canonical_in_sysfs(&e.path().join("link").join(f)) {
                if c.starts_with("/sys/devices") && c.is_file() { v.push(c); }
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

// ── runtime PM / SATA / NVMe discovery ────────────────────────────────────

/// Drivers that run their own runtime-PM policy (GPU drivers, the ME).
const RPM_DRIVER_DENY: &[&str] = &["nvidia", "nouveau", "amdgpu", "radeon", "i915", "xe", "mei_me"];

fn driver_name(dev: &Path) -> Option<String> {
    std::fs::read_link(dev.join("driver")).ok()?.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// PCI power/control files the runtime-PM row may touch. Display devices,
/// the other functions of their slot (HDA / USB-C on the dGPU) and every
/// bridge above them are excluded: their D3cold is the GPU driver's business.
fn pci_runtime_files() -> Vec<PathBuf> {
    let devs: Vec<PathBuf> = std::fs::read_dir(PCI_DEVICES).into_iter().flatten().flatten()
        .filter_map(|e| std::fs::canonicalize(e.path()).ok())
        .filter(|p| p.starts_with("/sys/devices"))
        .collect();
    let class = |d: &Path| read(&d.join("class")).unwrap_or_default();
    let display: Vec<&PathBuf> = devs.iter().filter(|d| class(d).starts_with("0x03")).collect();
    let slot = |d: &Path| d.file_name().map(|n| n.to_string_lossy().rsplit_once('.').map_or(String::new(), |x| x.0.to_owned()));
    let display_slots: Vec<Option<String>> = display.iter().map(|d| slot(d)).collect();
    let mut v: Vec<PathBuf> = devs.iter()
        .filter(|d| !class(d).starts_with("0x03"))
        .filter(|d| !display_slots.contains(&slot(d)))
        .filter(|d| !display.iter().any(|g| g.starts_with(d.as_path())))
        .filter(|d| driver_name(d).map_or(true, |n| !RPM_DRIVER_DENY.contains(&n.as_str())))
        .map(|d| d.join("power/control"))
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// USB devices (not interfaces) without a HID (03) or audio (01) interface.
fn usb_runtime_files() -> Vec<PathBuf> { usb_pm_files("power/control") }

fn usb_pm_files(rel: &str) -> Vec<PathBuf> {
    let base = Path::new("/sys/bus/usb/devices");
    let mut v = Vec::new();
    for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.contains(':') { continue; }
        let Some(dev) = canonical_in_sysfs(&e.path()) else { continue };
        let skip = std::fs::read_dir(&dev).into_iter().flatten().flatten()
            .filter(|i| i.file_name().to_string_lossy().starts_with(&format!("{name}:")))
            .any(|i| matches!(read(&i.path().join("bInterfaceClass")).as_deref(), Some("03") | Some("01")));
        let f = dev.join(rel);
        if !skip && f.is_file() { v.push(f); }
    }
    v.sort();
    v
}

/// Samsung OLED laptop panels ("ATNA...") have no backlight for ABM to modulate;
/// the knob answers EBUSY on every write, so such connectors are not offered.
fn edid_is_oled(edid: &Path) -> bool {
    let Ok(b) = std::fs::read(edid) else { return false };
    if b.len() < 128 { return false; }
    [54usize, 72, 90, 108].iter().any(|&o| {
        b[o..o + 3] == [0, 0, 0] && b[o + 3] == 0xFC
            && String::from_utf8_lossy(&b[o + 5..o + 18]).trim_start().starts_with("ATNA")
    })
}

fn amdgpu_abm_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten()
        .filter(|e| { let n = e.file_name().to_string_lossy().into_owned(); n.starts_with("card") && n.contains("-eDP-") })
        .filter(|e| read(&e.path().join("enabled")).as_deref() == Some("enabled"))
        .filter(|e| !edid_is_oled(&e.path().join("edid")))
        .map(|e| e.path().join("amdgpu/panel_power_savings"))
        .filter(|p| p.is_file())
        .filter_map(|p| canonical_in_sysfs(&p))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn rfkill_files(kind: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir("/sys/class/rfkill").into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| read(&p.join("type")).as_deref() == Some(kind))
        .map(|p| p.join("soft"))
        .filter(|p| p.is_file())
        .filter_map(|p| canonical_in_sysfs(&p))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn scsi_host_files(attr: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir("/sys/class/scsi_host").into_iter().flatten().flatten()
        .map(|e| e.path().join(attr))
        .filter(|p| p.is_file())
        .filter_map(|p| canonical_in_sysfs(&p))
        .collect();
    v.sort();
    v
}

fn nvme_latency_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = numbered(Path::new("/sys/class/nvme"), "nvme").into_iter()
        .map(|(_, p)| p.join("power/pm_qos_latency_tolerance_us"))
        .filter(|p| p.is_file())
        .filter_map(|p| canonical_in_sysfs(&p))
        .collect();
    v.sort();
    v
}

/// Compressors zswap can use: loaded ones from /proc/crypto plus the usual
/// algorithms present as modules (setting the parameter autoloads them).
fn zswap_compressors() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let crypto = std::fs::read_to_string("/proc/crypto").unwrap_or_default();
    let mut name = String::new();
    for l in crypto.lines() {
        let Some((k, val)) = l.split_once(':') else { continue };
        match k.trim() {
            "name" => name = val.trim().to_owned(),
            "type" if matches!(val.trim(), "scomp" | "acomp" | "compression") => {
                if !name.is_empty() && !v.contains(&name) { v.push(name.clone()); }
            }
            _ => {}
        }
    }
    for a in ["lz4", "lz4hc", "zstd", "lzo", "lzo-rle", "842", "deflate"] {
        if !v.iter().any(|x| x == a) && kmod_available(a, "crypto") { v.push(a.into()); }
    }
    v.retain(|x| x.len() <= 32 && x.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)));
    v
}

/// Target::File rows that exist but must not be offered in some states.
fn file_usable(p: &str) -> bool {
    match p {
        // Ratio rows only while the system is in ratio mode (the bytes twin reads 0):
        // restoring a ratio of 0 over a bytes configuration would zero both.
        "/proc/sys/vm/dirty_ratio" => read(Path::new("/proc/sys/vm/dirty_bytes")).as_deref() == Some("0"),
        "/proc/sys/vm/dirty_background_ratio" => read(Path::new("/proc/sys/vm/dirty_background_bytes")).as_deref() == Some("0"),
        // Present but empty (and not writable) where EAS cannot run.
        "/proc/sys/kernel/sched_energy_aware" => read(Path::new(p)).map_or(false, |v| !v.is_empty()),
        // BORE kernels (CachyOS) export base_slice_ns read-only and derive it from min_base_slice_ns:
        // a row that can only fail must not be offered.
        "/sys/kernel/debug/sched/base_slice_ns" => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).map_or(true, |m| m.permissions().mode() & 0o222 != 0)
        }
        _ => true,
    }
}

/// Nominal (base) frequency in kHz of the CPU a cpufreq policy belongs to, from acpi_cppc (MHz).
fn cppc_nominal_khz(policy: &Path) -> Option<String> {
    let n = num_suffix(&policy.file_name()?.to_string_lossy(), "policy")?;
    let mhz: u64 = read(&Path::new(CPU_DIR).join(format!("cpu{n}/acpi_cppc/nominal_freq")))?.parse().ok().filter(|m| *m > 0)?;
    Some((mhz * 1000).to_string())
}

/// "1"/"0" for feature `name` in the text of debugfs sched/features (NAME or NO_NAME).
fn sched_feature_state(raw: &str, name: &str) -> Option<&'static str> {
    raw.split_whitespace().find_map(|w| if w == name { Some("1") } else if w.strip_prefix("NO_") == Some(name) { Some("0") } else { None })
}

#[cfg(test)]
pub fn sched_feature_state_for_test(raw: &str, name: &str) -> Option<&'static str> { sched_feature_state(raw, name) }

/// Writes one tunable value to one concrete target, dispatching the targets
/// that are not plain files. Everything else goes through write_checked.
pub fn write_value(t: &Tunable, f: &Path, data: &str) -> Result<(), String> {
    match t.target {
        Target::WifiPowerSave => wifi_set(f, data),
        Target::EthWol => wol_set(f, data),
        Target::EthEee => eee_set(f, data),
        Target::DiskApm(_) => apm_set(f, data),
        Target::SchedExt => scx_set(data),
        Target::SchedFeature(n) => {
            if data != n && data.strip_prefix("NO_") != Some(n) { return Err(format!("{data}: not a state of {n}")); }
            write_checked(f, data)
        }
        Target::DirtyBytes { ratio, .. } if data.starts_with("ratio:") => write_checked(Path::new(ratio), &data[6..]),
        _ => write_checked(f, data),
    }
}

// ── concrete files per tunable ────────────────────────────────────────────

/// Every concrete file the tunable writes. Empty = not available here.
pub fn files(t: &Tunable) -> Vec<PathBuf> {
    if !vendor_ok(t) { return vec![]; }
    let existing = |p: PathBuf| if p.is_file() { vec![p] } else { vec![] };
    match t.target {
        // smt/control also reports notsupported / forceoff / notimplemented: not writable then.
        Target::File(p) if p.ends_with("/smt/control") =>
            if matches!(read(Path::new(p)).as_deref(), Some("on") | Some("off")) { vec![PathBuf::from(p)] } else { vec![] },
        Target::File(p) if !file_usable(p) => vec![],
        Target::File(p) => existing(PathBuf::from(p)),
        Target::AnyFile(list) => list.iter().map(PathBuf::from).find(|p| p.is_file()).into_iter().collect(),
        Target::PciRuntimePm => pci_runtime_files(),
        Target::UsbRuntimePm => usb_runtime_files(),
        Target::UsbAutosuspendMs => usb_pm_files("power/autosuspend_delay_ms"),
        Target::AhciPortRuntime => ahci_port_files(),
        Target::AhciDisk(a) => ahci_disk_files(a),
        Target::DiskApm(i) => disk_apm_files(i),
        Target::AmdgpuAbm => amdgpu_abm_files(),
        Target::Rfkill(k) => rfkill_files(k),
        Target::ScsiHost(a) => scsi_host_files(a),
        Target::NvmeLatency => nvme_latency_files(),
        Target::DirtyBytes { bytes, .. } => existing(PathBuf::from(bytes)),
        // scaling_governor / energy_performance_preference: on a 2+ CCD chip the
        // per-CCD override rows below cover the same file set (and always run
        // after this row, so leaving both visible just invites setting one and
        // wondering why the other value stuck). Hide the global row there and
        // point people at Governor/EPP · CCDn instead; a single-CCD chip has no
        // such rows, so the global one is the only way to set this and stays.
        Target::PerPolicy(f) if ccx_groups().len() > 1 => { let _ = f; vec![] }
        Target::PerPolicy(f) => policies().into_iter().map(|p| p.join(f)).filter(|p| p.is_file()).collect(),
        Target::MinFreq => policies().into_iter().map(|p| p.join("scaling_min_freq")).filter(|p| p.is_file()).collect(),
        Target::FloorFreq => policies().into_iter().map(|p| p.join("amd_pstate_floor_freq")).filter(|p| p.is_file()).collect(),
        Target::SchedFeature(name) => {
            let f = PathBuf::from("/sys/kernel/debug/sched/features");
            // Unreadable (unprivileged) = cannot tell; root re-checks at write time.
            match read(&f) {
                Some(raw) if !sched_feature_state(&raw, name).is_some() => vec![],
                _ => existing(f),
            }
        }
        Target::PerCcdPolicy(f, ccd) => ccd_policies(ccd).into_iter().map(|p| p.join(f)).filter(|p| p.is_file()).collect(),
        Target::Boost => {
            let per: Vec<_> = policies().into_iter().map(|p| p.join("boost")).filter(|p| p.is_file()).collect();
            if !per.is_empty() { return per; }
            let global = existing(Path::new(CPU_DIR).join("cpufreq/boost"));
            if !global.is_empty() { return global; }
            existing(Path::new(CPU_DIR).join("intel_pstate/no_turbo"))
        }
        Target::PerCoreType(f, ct) => policies_within(&core_type_cpus(ct)).into_iter().map(|p| p.join(f)).filter(|p| p.is_file()).collect(),
        Target::PerCpu(f) => cpus().into_iter().map(|(_, c)| c.join(f)).filter(|p| p.is_file()).collect(),
        Target::Uncore(f) => uncore_files(f),
        Target::RaplWatts(n) => rapl_files(n),
        Target::TccOffset => tcc_files(),
        Target::IntelGt { i915, xe } => intel_gt_files(i915, xe),
        Target::X3d => x3d_mode_path().into_iter().collect(),
        Target::CState => cpus().into_iter()
            .flat_map(|(_, c)| numbered(&c.join("cpuidle"), "state"))
            .map(|(_, s)| s.join("disable"))
            .filter(|p| p.is_file())
            .collect(),
        Target::PerBlock(f) => block_devs().into_iter().map(|d| d.join("queue").join(f)).filter(|p| p.is_file()).collect(),
        Target::Mce => numbered(Path::new("/sys/devices/system/machinecheck"), "machinecheck").into_iter()
            .map(|(_, p)| p.join("check_interval")).filter(|p| p.is_file()).collect(),
        Target::AmdgpuDpm => amdgpu_dpm_files(),
        Target::PciLatency => pci_config_files_writable(),
        Target::WqCpumask => if has_domains() {
            existing(PathBuf::from("/sys/devices/virtual/workqueue/cpumask"))
        } else { vec![] },
        Target::Irq => if has_domains() { irq_files() } else { vec![] },
        Target::WifiPowerSave => wifi_ifaces(),
        Target::EthWol => eth_ifaces(),
        Target::EthEee => eth_ifaces().into_iter().filter(|p| eee_get(p).is_some()).collect(),
        Target::PciAspm => pci_aspm_files(),
        Target::SchedExt => if scx_available().is_empty() { vec![] } else { vec![PathBuf::from(SCX_STATE)] },
        Target::CcdPark => {
            // Stays available while a CCD is parked, so "none" can bring it back.
            let parked = online_cpus().len() < present_cpus().len();
            if !has_domains() && !parked { return vec![]; }
            cpus().into_iter().map(|(_, c)| c.join("online")).filter(|p| p.is_file()).collect()
        }
    }
}

/// Live option list (value, label).
pub fn options(t: &Tunable) -> Vec<(String, String)> {
    let same = |v: Vec<String>| v.into_iter().map(|s| (s.clone(), s)).collect();
    match (t.options, t.target) {
        (Options::Fixed(o), _) => o.iter().map(|s| (s.to_string(), s.to_string())).collect(),
        (Options::ListFile(f), _) => {
            let p = policies().into_iter().next().map(|p| p.join(f));
            // "custom" is what EPP reads back after a raw numeric write; it cannot be written as a string.
            same(p.and_then(|p| read(&p)).map(|s| s.split_whitespace().filter(|o| *o != "custom").map(str::to_owned).collect())
                .unwrap_or_default())
        }
        (Options::ListAt(f), _) => same(read(Path::new(f)).map(|s| s.split_whitespace().map(str::to_owned).collect()).unwrap_or_default()),
        (Options::Special, Target::SchedExt) => {
            let mut v = vec![("none".to_string(), "none (kernel EEVDF)".to_string())];
            v.extend(scx_available().into_iter().map(|n| (n.to_string(), format!("scx_{n}"))));
            v
        }
        (Options::Special, Target::File(p)) if p.ends_with("default_qdisc") => {
            // pfifo_fast is part of the core; the others are sch_* modules.
            let mut v = vec!["pfifo_fast".to_string()];
            for q in ["fq", "fq_codel", "cake"] {
                if kmod_available(&format!("sch_{q}"), "net/sched") { v.push(q.into()); }
            }
            // Keep whatever is set now selectable even if we could not see its module.
            if let Some(cur) = read(Path::new(p)) { if !v.contains(&cur) { v.push(cur); } }
            same(v)
        }
        (Options::Special, Target::File(p)) if p.ends_with("zswap/parameters/compressor") => {
            let mut v = zswap_compressors();
            if let Some(cur) = read(Path::new(p)) { if !cur.is_empty() && !v.contains(&cur) { v.push(cur); } }
            same(v)
        }
        (Options::Special, Target::File(p)) if p.ends_with("tcp_congestion_control") => {
            // Loaded algorithms, plus bbr when its module exists (writing it autoloads it).
            let mut v: Vec<String> = read(Path::new("/proc/sys/net/ipv4/tcp_available_congestion_control"))
                .map(|s| s.split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
            if !v.iter().any(|x| x == "bbr") && kmod_available("tcp_bbr", "net/ipv4") { v.push("bbr".into()); }
            same(v)
        }
        (Options::Bracketed, Target::File(p)) => same(read(Path::new(p)).map(|s| parse_bracketed(&s).1).unwrap_or_default()),
        (Options::Bracketed, Target::IntelGt { .. }) =>
            same(files(t).first().and_then(|f| read(f)).map(|s| parse_bracketed(&s).1).unwrap_or_default()),
        (Options::Bracketed, Target::PerBlock(_)) => {
            // Options every disk supports (a preset must be writable everywhere).
            let mut common: Option<Vec<String>> = None;
            for f in files(t) {
                let o = read(&f).map(|s| parse_bracketed(&s).1).unwrap_or_default();
                common = Some(match common { None => o, Some(c) => c.into_iter().filter(|x| o.contains(x)).collect() });
            }
            same(common.unwrap_or_default())
        }
        (Options::Special, Target::MinFreq) | (Options::Special, Target::FloorFreq) => {
            let mut v = Vec::new();
            // amd-pstate only; intel_pstate has no such file.
            if policies().first().map_or(false, |p| p.join("amd_pstate_lowest_nonlinear_freq").is_file()) {
                v.push(("lowest_nonlinear".into(), "lowest_nonlinear (efficient floor)".into()));
            }
            v.push(("cpuinfo_min".into(), "cpuinfo_min (hardware minimum)".into()));
            if matches!(t.target, Target::FloorFreq) && policies().first().and_then(|p| cppc_nominal_khz(p)).is_some() {
                v.insert(0, ("nominal".into(), "nominal (kernel default)".into()));
            }
            v
        }
        (Options::Special, Target::CState) => {
            let names = cstate_names();
            let mut v = vec![("all".to_string(), "all enabled".to_string())];
            for i in (0..names.len().saturating_sub(1)).rev() {
                v.push((i.to_string(), format!("≤ {} (disable deeper)", names[i])));
            }
            v
        }
        (Options::Special, Target::PciLatency) => vec![("tuned".into(), "tuned (00 / 80 / 20)".into())],
        (Options::Special, Target::WqCpumask) | (Options::Special, Target::Irq) => {
            let mut v = vec![("all".to_string(), "all CPUs (stock)".to_string())];
            v.extend(role_options(&ccx_groups(), false));
            v
        }
        (Options::Special, Target::CcdPark) => {
            let mut v = vec![("none".to_string(), "none (all CPUs online)".to_string())];
            v.extend(role_options(&ccx_groups(), true).into_iter().map(|(k, l)| (k, format!("park {l}"))));
            // The parked CCD is invisible to ccx_groups(): keep its option.
            if let Some((role, label, _)) = parked_record() {
                let role = canonical_ccd(&role);
                if !v.iter().any(|(k, _)| *k == role) { v.push((role, label)); }
            }
            v
        }
        _ => vec![],
    }
}

fn bool_norm(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        "1" | "Y" | "y" | "on" => Some("1"),
        "0" | "N" | "n" | "off" => Some("0"),
        _ => None,
    }
}

/// Raw file content -> the value that writes it back unchanged.
fn restorable(raw: &str) -> String {
    if raw.contains('[') || raw.contains('(') { parse_bracketed(raw).0.unwrap_or_else(|| raw.to_owned()) } else { raw.to_owned() }
}

fn parse_int(raw: &str) -> Option<i64> {
    let s = raw.trim();
    match s.strip_prefix("0x") { Some(h) => i64::from_str_radix(h, 16).ok(), None => s.parse().ok() }
}

/// Current value in preset terms; "mixed" if per-file values differ; None if unreadable.
pub fn current(t: &Tunable) -> Option<String> { current_with(t, files(t)) }

/// `current` for a caller that already resolved `files(t)` (describe: the
/// file walk — PCI config, IRQs, Wi-Fi via iw — then runs once per tunable).
fn current_with(t: &Tunable, fs: Vec<PathBuf>) -> Option<String> {
    if fs.is_empty() { return None; }
    match t.target {
        Target::CState => {
            let n = cstate_names().len();
            let dis: Vec<bool> = (0..n)
                .map(|i| read(&Path::new(CPU_DIR).join(format!("cpu0/cpuidle/state{i}/disable"))).as_deref() == Some("1"))
                .collect();
            return Some(match dis.iter().position(|d| *d) {
                None => "all".into(),
                Some(0) => "0".into(),
                Some(first) => (first - 1).to_string(),
            });
        }
        Target::SchedFeature(n) => return sched_feature_state(&read(fs.first()?)?, n).map(str::to_owned),
        Target::MinFreq | Target::FloorFreq => {
            let pol = policies().into_iter().next()?;
            let cur = read(&pol.join(if matches!(t.target, Target::FloorFreq) { "amd_pstate_floor_freq" } else { "scaling_min_freq" }))?;
            if matches!(t.target, Target::FloorFreq) && cppc_nominal_khz(&pol).as_deref() == Some(cur.as_str()) { return Some("nominal".into()); }
            if read(&pol.join("amd_pstate_lowest_nonlinear_freq")).as_deref() == Some(cur.as_str()) { return Some("lowest_nonlinear".into()); }
            if read(&pol.join("cpuinfo_min_freq")).as_deref() == Some(cur.as_str()) { return Some("cpuinfo_min".into()); }
            return Some(format!("{cur} kHz"));
        }
        Target::PciLatency => {
            // Hardwired-zero (PCIe) functions ignore the write; they don't make it "stock".
            // Suspended functions are skipped (not woken): the byte is restored with
            // the rest of config space on resume, so the awake ones tell the state.
            let tuned = fs.iter().filter(|f| !pci_runtime_suspended(f))
                .all(|f| pci_latency_read(f).map_or(true, |b| b == pci_latency_target(f) || b == 0));
            return Some(if tuned { "tuned".into() } else { "stock".into() });
        }
        Target::WqCpumask => {
            let set = parse_cpumask(&read(&fs[0])?);
            if possible_cpus().iter().all(|c| set.contains(c)) || online_cpus().iter().all(|c| set.contains(c)) {
                return Some("all".into());
            }
            return Some(role_matching(&ccx_groups(), &set).unwrap_or_else(|| fmt_cpu_list(&set)));
        }
        Target::Irq => {
            let (groups, online) = (ccx_groups(), online_cpus());
            let mut seen: Option<String> = None;
            for f in &fs {
                let Some(raw) = read(f) else { continue };
                let set = cpu_list(&raw);
                // Single-CPU IRQs (per-CPU timers, managed queues) say nothing about the policy.
                if set.len() <= 1 { continue; }
                let v = if online.iter().all(|c| set.contains(c)) { "all".to_string() }
                        else { role_matching(&groups, &set).unwrap_or_else(|| "custom".into()) };
                match &seen { None => seen = Some(v), Some(s) if *s == v => {}, Some(_) => return Some("mixed".into()) }
            }
            return Some(seen.unwrap_or_else(|| "all".into()));
        }
        Target::CcdPark => {
            if let Some((role, _, _)) = parked_record() { return Some(canonical_ccd(&role)); }
            let online = online_cpus();
            let off: Vec<usize> = present_cpus().into_iter().filter(|c| !online.contains(c)).collect();
            if !off.is_empty() && hybrid().map_or(false, |h| h.ecores == off) { return Some("ecore".into()); }
            return Some(if off.is_empty() { "none".into() } else { format!("offline {}", fmt_cpu_list(&off)) });
        }
        Target::SchedExt => return scx_current(),
        Target::DiskApm(_) => return fs.first().and_then(|f| apm_get(f)),
        Target::WifiPowerSave | Target::EthWol | Target::EthEee => {
            let get = |f: &PathBuf| match t.target { Target::EthWol => wol_get(f), Target::EthEee => eee_get(f), _ => wifi_get(f) };
            let mut vals = fs.iter().filter_map(get);
            let first = vals.next()?;
            return Some(if vals.all(|v| v == first) { first } else { "mixed".into() });
        }
        Target::PciAspm => {
            let on = |name: &str| fs.iter().filter(|f| f.file_name().map_or(false, |n| n == name))
                .all(|f| read(f).as_deref() == Some("1"));
            return Some(if on("l1_aspm") && on("l1_1_aspm") && on("l1_2_aspm") { "l1ss".into() }
                        else if on("l1_aspm") { "l1".into() } else { "stock (per driver)".into() });
        }
        Target::RaplWatts(_) => {
            let mut w = fs.iter().filter_map(|p| read(p)).filter_map(|r| parse_int(&r)).map(|uw| (uw / 1_000_000).to_string());
            let first = w.next()?;
            return Some(if w.all(|v| v == first) { first } else { "mixed".into() });
        }
        _ => {}
    }
    let mut vals = fs.iter().filter_map(|p| read(p).map(|r| (p, r))).map(|(p, raw)| match t.kind {
        Kind::Bool if inverted(p) => match bool_norm(&raw) { Some("1") => "0".to_owned(), Some(_) => "1".to_owned(), None => raw },
        Kind::Bool => bool_norm(&raw).map(str::to_owned).unwrap_or(raw),
        Kind::Int { .. } => parse_int(&raw).map(|n| n.to_string()).unwrap_or(raw),
        Kind::Choice => restorable(&raw),
    });
    let first = vals.next()?;
    Some(if vals.all(|v| v == first) { first } else { "mixed".into() })
}

// ── writes ────────────────────────────────────────────────────────────────

/// Validated value, still in preset terms.
pub fn validate(t: &Tunable, v: &Value) -> Result<String, String> {
    let s = match v {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => if *b { "1".into() } else { "0".into() },
        _ => return Err("value must be a string or number".into()),
    };
    if s.is_empty() || s.len() > 64 || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_+-.".contains(&b)) {
        return Err(format!("'{s}' has an unexpected shape"));
    }
    match t.kind {
        Kind::Int { min, max } => {
            let n: i64 = s.parse().map_err(|_| format!("'{s}' is not an integer"))?;
            if !(min..=max).contains(&n) { return Err(format!("{n} outside [{min}, {max}]")); }
            Ok(n.to_string())
        }
        Kind::Bool => bool_norm(&s).map(str::to_owned).ok_or_else(|| format!("'{s}' is not 0/1")),
        Kind::Choice => {
            let s = if matches!(t.target, Target::WqCpumask | Target::Irq | Target::CcdPark) { canonical_ccd(&s) } else { s };
            let opts = options(t);
            if opts.iter().any(|(v, _)| *v == s) { Ok(s) } else {
                Err(format!("'{s}' is not offered here ({})", opts.iter().map(|o| o.0.as_str()).collect::<Vec<_>>().join(", ")))
            }
        }
    }
}

/// The (file, bytes) writes for a validated value, computed against live sysfs.
pub fn plan(t: &Tunable, value: &str) -> Result<Vec<(PathBuf, String)>, String> {
    let fs = files(t);
    if fs.is_empty() { return Err("not available on this kernel/hardware".into()); }
    let role = |v: &str| resolve_ccd(&ccx_groups(), v).ok_or_else(|| format!("'{v}' does not resolve to a CCD here"));
    let out = match t.target {
        Target::SchedFeature(n) => vec![(fs[0].clone(), if value == "1" { n.to_owned() } else { format!("NO_{n}") })],
        Target::MinFreq | Target::FloorFreq => fs.iter().map(|f| {
            let dir = f.parent().unwrap();
            if value == "nominal" {
                return cppc_nominal_khz(dir).map(|v| (f.clone(), v)).ok_or_else(|| format!("{}: nominal frequency unreadable", dir.display()));
            }
            let src = if value == "lowest_nonlinear" { "amd_pstate_lowest_nonlinear_freq" } else { "cpuinfo_min_freq" };
            read(&dir.join(src)).filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                .map(|v| (f.clone(), v)).ok_or_else(|| format!("{} unreadable", dir.join(src).display()))
        }).collect::<Result<Vec<_>, _>>()?,
        Target::CState => {
            let keep: Option<u32> = if value == "all" { None } else { value.parse().ok() };
            fs.into_iter().map(|f| {
                let idx = f.parent().and_then(|d| d.file_name())
                    .and_then(|n| num_suffix(&n.to_string_lossy(), "state")).unwrap_or(0);
                (f, if matches!(keep, Some(k) if idx > k) { "1" } else { "0" }.to_owned())
            }).collect()
        }
        Target::PciLatency => fs.into_iter().map(|f| { let v = format!("{:02x}", pci_latency_target(&f)); (f, v) }).collect(),
        Target::WqCpumask => {
            let cpus = if value == "all" { possible_cpus() } else { role(value)?.cpus };
            vec![(fs[0].clone(), cpumask_hex(&cpus))]
        }
        Target::Irq => {
            let list = fmt_cpu_list(&if value == "all" { online_cpus() } else { role(value)?.cpus });
            fs.into_iter().map(|f| (f, list.clone())).collect()
        }
        Target::CcdPark => {
            if value == "none" {
                // Bring back what the park took; onlining SMT-offline siblings fails with EPERM.
                if let Some((_, _, cpus)) = parked_record() {
                    return Ok(cpus.iter().map(|c| (Path::new(CPU_DIR).join(format!("cpu{c}/online")), "1".to_owned())).collect());
                }
                return Ok(fs.into_iter().map(|f| (f, "1".to_owned())).collect());
            }
            // Already parked with this role (its CPUs are no longer resolvable): same writes, all no-ops.
            if let Some((_, _, cpus)) = parked_record().filter(|(r, _, _)| r == value) {
                return Ok(cpus.iter().map(|c| (Path::new(CPU_DIR).join(format!("cpu{c}/online")), "0".to_owned())).collect());
            }
            let g = role(value)?;
            if g.cpus.contains(&0) { return Err("the CCD holding cpu0 cannot be parked".into()); }
            g.cpus.iter().map(|c| Path::new(CPU_DIR).join(format!("cpu{c}/online")))
                .map(|p| if p.is_file() { Ok((p, "0".to_owned())) } else { Err(format!("{} missing", p.display())) })
                .collect::<Result<Vec<_>, _>>()?
        }
        Target::PciAspm => {
            // l1: only the L1 switch; l1ss: L1 plus both substates. Enabling a
            // substate implies L1 in the kernel, so the order is only cosmetic.
            let want: &[&str] = if value == "l1ss" { ASPM_FILES } else { &["l1_aspm"] };
            fs.into_iter().filter(|f| f.file_name().and_then(|n| n.to_str()).map_or(false, |n| want.contains(&n)))
                .map(|f| (f, "1".to_owned())).collect()
        }
        Target::SchedExt => vec![(fs[0].clone(), value.to_owned())],
        Target::WifiPowerSave | Target::EthWol | Target::EthEee => fs.into_iter().map(|f| (f, value.to_owned())).collect(),
        Target::RaplWatts(_) => {
            let w: i64 = value.parse().map_err(|_| format!("'{value}' is not a wattage"))?;
            fs.into_iter().map(|f| (f, (w * 1_000_000).to_string())).collect()
        }
        _ if t.kind == Kind::Bool => fs.into_iter().map(|f| {
            // no_turbo speaks the opposite of "boost".
            let value = if inverted(&f) { if value == "1" { "0" } else { "1" } } else { value };
            // Match the file's own vocabulary (module params report Y/N).
            let yn = read(&f).map_or(false, |r| r == "Y" || r == "N");
            let w = match (yn, value) { (true, "1") => "Y", (true, _) => "N", (false, v) => v };
            (f, w.to_owned())
        }).collect(),
        _ => fs.into_iter().map(|f| (f, value.to_owned())).collect(),
    };
    Ok(out)
}

/// Value to save in the baseline for one concrete file.
pub fn baseline_value(t: &Tunable, f: &Path) -> Option<String> {
    match t.target {
        Target::PciLatency => return pci_latency_read(f).map(|b| format!("{b:02x}")),
        Target::WifiPowerSave => return wifi_get(f),
        Target::EthWol => return wol_get(f),
        Target::EthEee => return eee_get(f),
        Target::DiskApm(_) => return apm_get(f),
        Target::SchedExt => return scx_current(),
        Target::SchedFeature(n) => return read(f).and_then(|raw| sched_feature_state(&raw, n)).map(|v| if v == "1" { n.to_owned() } else { format!("NO_{n}") }),
        Target::DirtyBytes { ratio, .. } if read(f).as_deref() == Some("0") => return read(Path::new(ratio)).map(|r| format!("ratio:{r}")),
        _ => {}
    }
    let raw = read(f)?;
    Some(match t.kind {
        Kind::Int { .. } => parse_int(&raw).map(|n| n.to_string()).unwrap_or(raw),
        _ => restorable(&raw),
    })
}

/// True if writing `data` over a file whose saved value is `orig` changes nothing.
pub fn same_value(t: &Tunable, orig: &str, data: &str) -> bool {
    if orig == data { return true; }
    match t.target {
        Target::WqCpumask => parse_cpumask(orig) == parse_cpumask(data),
        Target::Irq => cpu_list(orig) == cpu_list(data),
        _ => t.kind == Kind::Bool && bool_norm(orig).is_some() && bool_norm(orig) == bool_norm(data),
    }
}

/// Final guard before a root write. Allowed: anything resolving inside /sys
/// (PCI config only as the single latency byte), fixed /proc/sys files from
/// the table, and /proc/irq/<n>/smp_affinity_list.
pub fn write_checked(f: &Path, data: &str) -> Result<(), String> {
    // CPU hot-plug changes the L3 grouping: never serve a stale topology after it.
    if f.file_name().map_or(false, |n| n == "online" || n == "control") { invalidate_topology(); }
    let r = write_checked_inner(f, data);
    if f.file_name().map_or(false, |n| n == "online" || n == "control") {
        // Let the kernel finish the hot-plug (cache/, cpufreq policy) before the next read.
        std::thread::sleep(HOTPLUG_SETTLE_WRITE);
        invalidate_topology();
    }
    r
}

fn write_checked_inner(f: &Path, data: &str) -> Result<(), String> {
    let s = f.to_string_lossy();
    if s.starts_with("/proc/") {
        let ok = is_irq_file(f)
            || (s.starts_with("/proc/sys/") && TUNABLES.iter().any(|t| match t.target {
                Target::File(p) => p == s,
                Target::AnyFile(list) => list.contains(&s.as_ref()),
                Target::DirtyBytes { bytes, ratio } => bytes == s || ratio == s,
                _ => false,
            }));
        if !ok { return Err(format!("{s}: refused (outside the allowlist)")); }
        return sysfs_write(f, data.as_bytes()).map_err(|e| format!("{s}: {e}"));
    }
    let Some(real) = canonical_in_sysfs(f) else { return Err(format!("{s}: refused (outside the allowlist)")) };
    if real.file_name().map_or(false, |n| n == "config") {
        if !is_pci_config(&real) { return Err(format!("{s}: refused (not a PCI config file)")); }
        // Never write config space of a GPU, its slot siblings or the bridges above it,
        // nor of a sleeping device: the write would wake it through ACPI (D state on a
        // firmware-disabled dGPU).
        let tree = gpu_tree();
        if pci_unsafe_to_touch(&real, &tree, true) { return Err(format!("{s}: skipped (GPU tree or suspended device)")); }
        return pci_latency_write_timeout(&real, data).map_err(|e| format!("{s}: {e}"));
    }
    if is_pci_link_or_power(&real) {
        // power/control 'on' is meant to wake a device: only the GPU tree is off limits there.
        let is_link = real.parent().and_then(|p| p.file_name()).map_or(false, |n| n == "link");
        if pci_unsafe_to_touch(&real, &gpu_tree(), is_link) { return Err(format!("{s}: skipped (GPU tree or suspended device)")); }
        return write_with_timeout(real, data.to_owned()).map_err(|e| format!("{s}: {e}"));
    }
    if real.file_name().map_or(false, |n| n == "amd_x3d_mode") {
        return write_with_timeout(real, data.to_owned()).map_err(|e| format!("{s}: {e}"));
    }
    sysfs_write(&real, data.as_bytes()).map_err(|e| format!("{s}: {e}"))
}

/// `.../link/l1*_aspm` and PCI `power/control` files: writes that can resume a device.
fn is_pci_link_or_power(real: &Path) -> bool {
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let parent = real.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("");
    let in_pci = real.starts_with("/sys/devices") && real.to_string_lossy().contains("/0000:");
    in_pci && ((parent == "link" && ASPM_FILES.contains(&name)) || (parent == "power" && name == "control" && !real.to_string_lossy().contains("/usb")))
}

/// Latency-byte write in a child with a deadline, like amd_x3d_mode: a device that
/// hangs in ACPI takes only the child with it, never this helper or its lock.
fn pci_latency_write_timeout(cfg: &Path, hex: &str) -> std::io::Result<()> {
    let v = u8::from_str_radix(hex.trim(), 16).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad latency byte"))?;
    let pid = unsafe { libc::fork() };
    if pid < 0 { return Err(std::io::Error::last_os_error()); }
    if pid == 0 {
        let ok = pci_latency_write(cfg, &format!("{v:02x}")).is_ok();
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
        if r == pid { return if libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0 { Ok(()) } else { Err(std::io::Error::new(std::io::ErrorKind::Other, "config write failed")) }; }
        if r < 0 { return Err(std::io::Error::last_os_error()); }
        if std::time::Instant::now() >= end { unsafe { libc::kill(pid, libc::SIGKILL) }; return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "config write timed out (device not responding)")); }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// amd_x3d_mode goes through a synchronous ACPI _DSM that stalls forever on
/// some BIOS/AGESA versions (lutris-game-tune wraps it in `timeout 3`).
///
/// The write runs in a forked child that closes every inherited descriptor
/// first. A thread (the old approach) that hangs in the kernel keeps the whole
/// helper from exiting — and with it the tune lock, so every later tune-helper
/// call blocked behind it. A stuck child only holds its own open of the sysfs
/// file; this process answers and exits normally.
fn write_with_timeout(p: PathBuf, data: String) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| "bad path".to_string())?;
    let bytes = data.into_bytes();
    // After fork only async-signal-safe calls: open, close, write, _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 { return Err(format!("fork: {}", std::io::Error::last_os_error())); }
    if pid == 0 {
        unsafe {
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
            if fd < 0 { libc::_exit(*libc::__errno_location() & 0xFF); }
            for other in 0..4096 { if other != fd { libc::close(other); } }
            let n = libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
            if n < 0 { libc::_exit(*libc::__errno_location() & 0xFF); }
            libc::_exit(if n as usize == bytes.len() { 0 } else { 255 });
        }
    }
    let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
        if r == pid {
            if !libc::WIFEXITED(st) { return Err("writer terminated abnormally".into()); }
            return match libc::WEXITSTATUS(st) {
                0 => Ok(()),
                255 => Err("short write".into()),
                e => Err(std::io::Error::from_raw_os_error(e).to_string()),
            };
        }
        if r < 0 { return Err(format!("waitpid: {}", std::io::Error::last_os_error())); }
        if std::time::Instant::now() >= end {
            // Left behind: init reaps it whenever the firmware call returns.
            return Err("timed out after 3 s (ACPI _DSM stall?)".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// CCD topology for the GUI and lpm-gamemode STATUS.
pub fn describe_topology() -> Value {
    let g = ccx_groups();
    let role = |r: &str| resolve_ccd(&g, r).map(|c| c.index);
    json!({
        "ccds": g.iter().map(|c| json!({"index": c.index, "cpus": fmt_cpu_list(&c.cpus),
                                          "l3_kib": c.l3_kib, "max_khz": c.max_khz})).collect::<Vec<_>>(),
        "cache_ccd": role("cache"), "frequency_ccd": role("frequency"),
        "online": fmt_cpu_list(&online_cpus()), "present": fmt_cpu_list(&present_cpus()),
        "vendor": cpu_vendor().as_str(),
        "hybrid": hybrid().map(|h| {
            let max = |set: &[usize]| set.iter()
                .filter_map(|c| read(&Path::new(CPU_DIR).join(format!("cpu{c}/cpufreq/cpuinfo_max_freq"))))
                .filter_map(|s| s.parse::<u64>().ok()).max().unwrap_or(0);
            json!({"pcores": fmt_cpu_list(&h.pcores), "ecores": fmt_cpu_list(&h.ecores),
                   "pcore_max_khz": max(&h.pcores), "ecore_max_khz": max(&h.ecores)})
        }),
    })
}

/// Tunable list for the GUI.
pub fn describe() -> Value {
    let rows: Vec<Value> = TUNABLES.iter().filter(|t| vendor_ok(t)).map(|t| {
        let fs = files(t);
        let (min, max) = match t.kind { Kind::Int { min, max } => (json!(min), json!(max)), _ => (Value::Null, Value::Null) };
        let opts: Vec<Value> = options(t).into_iter().map(|(v, l)| json!({"value": v, "label": l})).collect();
        json!({
            "key": t.key, "group": t.group, "label": row_label(t), "help": t.help,
            "kind": match t.kind { Kind::Choice => "choice", Kind::Int { .. } => "int", Kind::Bool => "bool" },
            "options": opts, "min": min, "max": max,
            // debugfs is root-only (0700): unprivileged callers cannot tell; root checks at write time.
            "available": !fs.is_empty() || (t.debugfs && debugfs_snapshot_has(t.key).unwrap_or(true)),
            "debugfs": t.debugfs, "caution": t.caution, "hotplug": is_hotplug(t.key),
            "files": fs.len(),
            "current": current_with(t, fs).or_else(|| if t.debugfs { debugfs_snapshot_value(t.key) } else { None }),
        })
    }).collect();
    let mut m = Map::new();
    m.insert("ok".into(), json!(true));
    m.insert("tunables".into(), Value::Array(rows));
    m.insert("topology".into(), describe_topology());
    m.insert("vendor".into(), json!(cpu_vendor().as_str()));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn two_ccd() -> Vec<Ccx> {
        vec![
            Ccx { index: 0, cpus: (0..8).chain(16..24).collect(), l3_kib: 98304, max_khz: 5_200_000 },
            Ccx { index: 1, cpus: (8..16).chain(24..32).collect(), l3_kib: 32768, max_khz: 5_400_000 },
        ]
    }
    #[test]
    fn park_record_matching() {
        let rec = serde_json::json!({"role": "frequency", "label": "park frequency CCD (CCD1: 8-15,24-31)",
                                     "cpus": [8, 9, 10, 11, 12, 13, 14, 15, 24, 25, 26, 27, 28, 29, 30, 31]});
        let off: Vec<usize> = (8..16).chain(24..32).collect();
        let (role, label, _) = match_park_record(&rec, &off).unwrap();
        assert_eq!(role, "frequency");
        assert!(label.contains("CCD1"));
        assert!(match_park_record(&rec, &[]).is_none());                 // everything back online: stale
        assert!(match_park_record(&rec, &off[..8]).is_none());           // partially online: not this state
        assert!(match_park_record(&serde_json::json!({"role": "x", "cpus": []}), &[]).is_none());
    }
    #[test]
    fn new_targets() {
        assert_eq!(wifi_ifname(Path::new("/sys/class/net/wlan0")).as_deref(), Some("wlan0"));
        assert_eq!(wifi_ifname(Path::new("/sys/class/net/wlp4s0")).as_deref(), Some("wlp4s0"));
        assert!(wifi_ifname(Path::new("/sys/class/net/../../etc")).is_none());
        assert!(wifi_ifname(Path::new("/sys/class/net/a/b")).is_none());
        assert!(wifi_ifname(Path::new("/sys/class/net/averyveryverylongname")).is_none());
        assert!(wifi_ifname(Path::new("/etc/passwd")).is_none());
        assert!(scx_bin("sh").is_none());           // not an allowlisted scheduler
        assert!(scx_bin("../../bin/sh").is_none());
        assert!(wifi_set(Path::new("/sys/class/net/wlan0"), "2").is_err());
        assert!(ata_disk_name_ok("sda") && ata_disk_name_ok("sdaa") && !ata_disk_name_ok("sda1") && !ata_disk_name_ok("nvme0n1"));
        assert_eq!(apm_dev(Path::new("/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda/dev")).as_deref(), Some("sda"));
        assert!(apm_dev(Path::new("/etc/passwd")).is_none());
        assert!(apm_set(Path::new("/sys/devices/x/block/sda/dev"), "0").is_err());
        for k in ["cpu.idle_governor", "thp.mthp_64k", "thp.khp_max_ptes_none", "net.tcp_congestion",
                  "net.default_qdisc", "net.wifi_power_save", "pci.aspm_links", "sched.ext",
                  "disk.apm_0", "pm.ahci_runtime_timeout", "pm.ahci_disk_runtime", "pm.ahci_port_runtime",
                  "net.wol", "rf.bluetooth", "rf.wlan", "rf.wwan", "gpu.amdgpu_abm", "usb.autosuspend_ms",
                  "vm.defrag_mode", "kernel.sched_bore", "sched.feat_preempt_short", "blk.rq_affinity", "net.eee"] {
            assert!(find(k).is_some(), "{k}");
        }
        assert!(best_effort(find("pci.aspm_links").unwrap()));
    }
    #[test]
    fn bracketed() {
        let (s, o) = parse_bracketed("always [madvise] never");
        assert_eq!(s.as_deref(), Some("madvise"));
        assert_eq!(o, vec!["always", "madvise", "never"]);
        assert_eq!(parse_bracketed("none voluntary (full) lazy").0.as_deref(), Some("full"));
        assert_eq!(restorable("[none] mq-deadline kyber"), "none");
        assert_eq!(restorable("2000"), "2000");
    }
    #[test]
    fn l3_groups_during_smt_offline() {
        // Mid-hot-plug snapshot: cpu0 still lists its (now offline) siblings, cpu1 does not.
        let online: Vec<usize> = (0..16).collect();
        let mut e = vec![(0, cpu_list("0-7,16-23"), 98304), (1, cpu_list("0-7"), 98304)];
        for c in 2..8 { e.push((c, cpu_list("0-7"), 98304)); }
        for c in 8..16 { e.push((c, cpu_list("8-15,24-31"), 32768)); }
        let mut g = group_l3(e, &online);
        g.sort_by_key(|g| g.cpus[0]);
        for (i, x) in g.iter_mut().enumerate() { x.index = i; }
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].cpus, (0..8).collect::<Vec<_>>());
        assert_eq!(g[1].cpus, (8..16).collect::<Vec<_>>());
        assert_eq!(resolve_ccd(&g, "cache").unwrap().index, 0);
    }
    #[test]
    fn table_invariants() {
        for (i, a) in TUNABLES.iter().enumerate() {
            assert!(TUNABLES[i + 1..].iter().all(|b| b.key != a.key), "duplicate {}", a.key);
            assert!(a.key.len() <= 64 && a.key.bytes().all(|b| b.is_ascii_alphanumeric() || b"._".contains(&b)));
        }
        // Hot-plug rows last, pstate mode first.
        let n = TUNABLES.len();
        let pos: Vec<usize> = HOTPLUG_KEYS.iter().map(|k| TUNABLES.iter().position(|t| t.key == *k).unwrap()).collect();
        assert_eq!(pos, vec![n - 2, n - 1]);
        assert_eq!(TUNABLES[0].key, "cpu.pstate_status");
        // Per-CCD overrides must be applied after the global rows they override.
        let pos = |k: &str| TUNABLES.iter().position(|t| t.key == k).unwrap();
        for (global, per) in [("cpu.governor", "cpu.governor_ccd0"), ("cpu.epp", "cpu.epp_ccd0"),
                              ("cpu.boost", "cpu.boost_ccd0"), ("cpu.min_freq", "cpu.max_freq_ccd0")] {
            assert!(pos(global) < pos(per), "{per} must follow {global}");
        }
    }
    #[test]
    fn validation() {
        let sw = find("vm.swappiness").unwrap();
        assert_eq!(validate(sw, &json!(10)).unwrap(), "10");
        assert!(validate(sw, &json!(999)).is_err());
        assert!(validate(sw, &json!("1; rm")).is_err());
        assert!(validate(sw, &json!("../x")).is_err());
        let b = find("kernel.sched_schedstats").unwrap();
        assert_eq!(validate(b, &json!("Y")).unwrap(), "1");
        assert_eq!(parse_int("0x0007"), Some(7));
        assert_eq!(validate(find("usb.autosuspend").unwrap(), &json!(-1)).unwrap(), "-1");
    }
    #[test]
    fn vendors() {
        assert_eq!(vendor_from_cpuinfo("processor\t: 0\nvendor_id\t: GenuineIntel\n"), Vendor::Intel);
        assert_eq!(vendor_from_cpuinfo("vendor_id : AuthenticAMD"), Vendor::Amd);
        assert_eq!(vendor_from_cpuinfo("model name : x"), Vendor::Any);
        // Vendor-specific rows are tagged; shared rows stay portable.
        assert_eq!(find("cpu.x3d_mode").unwrap().vendor, Vendor::Amd);
        assert_eq!(find("cpu.intel_pstate_status").unwrap().vendor, Vendor::Intel);
        assert_eq!(find("cpu.epp").unwrap().vendor, Vendor::Any);
        let pos = |k: &str| TUNABLES.iter().position(|t| t.key == k).unwrap();
        assert!(pos("cpu.intel_pstate_status") < pos("cpu.governor"));
        assert!(pos("cpu.epp") < pos("cpu.epp_pcore") && pos("cpu.max_perf_pct") < pos("cpu.min_perf_pct"));
        assert!(pos("cpu.uncore_max_khz") < pos("cpu.uncore_min_khz"));
        assert!(inverted(Path::new("/sys/devices/system/cpu/intel_pstate/no_turbo")));
    }
    #[test]
    fn cpu_formats() {
        assert_eq!(cpu_list("0-3,8-11"), vec![0, 1, 2, 3, 8, 9, 10, 11]);
        assert_eq!(fmt_cpu_list(&[17, 0, 1, 2, 3, 8, 16]), "0-3,8,16-17");
        let ccd1: Vec<usize> = (8..16).chain(24..32).collect();
        assert_eq!(cpumask_hex(&ccd1), "ff00ff00");
        assert_eq!(parse_cpumask("ff00ff00"), ccd1);
        assert_eq!(cpumask_hex(&[0, 40]), "100,00000001");
        assert_eq!(parse_cpumask("100,00000001"), vec![0, 40]);
        assert_eq!(parse_cpumask("zz"), Vec::<usize>::new());
        assert_eq!(size_kib("96M"), 98304);
    }
    #[test]
    fn roles() {
        let g = two_ccd();
        assert_eq!(resolve_ccd(&g, "cache").unwrap().index, 0);
        assert_eq!(resolve_ccd(&g, "frequency").unwrap().index, 1);
        assert_eq!(resolve_ccd(&g, "ccd1").unwrap().index, 1);
        assert!(resolve_ccd(&g, "ccd7").is_none());
        assert!(resolve_ccd(&g[..1], "cache").is_none());
        // Same advertised max clock: frequency = the smaller-cache die.
        let mut tied = two_ccd();
        tied[1].max_khz = tied[0].max_khz;
        assert_eq!(resolve_ccd(&tied, "frequency").unwrap().index, 1);
        // Symmetric part: neither role resolves, only ccdN.
        let mut sym = tied.clone();
        sym[1].l3_kib = sym[0].l3_kib;
        assert!(resolve_ccd(&sym, "cache").is_none() && resolve_ccd(&sym, "frequency").is_none());
        // Parking never offers cpu0's CCD.
        assert!(role_options(&g, true).iter().all(|(k, _)| k != "ccd0"));
        assert_eq!(role_matching(&g, &(8..16).chain(24..32).collect::<Vec<_>>()).as_deref(), Some("ccd1"));
        assert_eq!(role_matching(&g, &[0, 1]), None);
    }
    #[test]
    fn same_values() {
        let wq = find("wq.cpumask").unwrap();
        assert!(same_value(wq, "0000ff00", "ff00"));
        let irq = find("irq.affinity").unwrap();
        assert!(same_value(irq, "0-3", "0,1,2,3"));
        let b = find("kernel.sched_schedstats").unwrap();
        assert!(same_value(b, "Y", "1") && !same_value(b, "Y", "N"));
    }
    #[test]
    fn forked_writer() {
        let p = std::env::temp_dir().join(format!("lpm-x3d-{}", std::process::id()));
        std::fs::write(&p, b"").unwrap();
        write_with_timeout(p.clone(), "cache".into()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "cache");
        std::fs::remove_file(&p).unwrap();
        let e = write_with_timeout(p.clone(), "x".into()).unwrap_err();
        assert!(e.contains("No such file"), "{e}");
    }
    #[test]
    fn write_guard() {
        assert!(write_checked(Path::new("/proc/sys/kernel/core_pattern"), "x").is_err());
        assert!(write_checked(Path::new("/etc/passwd"), "x").is_err());
        assert!(write_checked(Path::new("/proc/irq/../sys/kernel/core_pattern"), "x").is_err());
        assert!(is_irq_file(Path::new("/proc/irq/42/smp_affinity_list")));
        assert!(!is_irq_file(Path::new("/proc/irq/default_smp_affinity")));
        assert!(!is_irq_file(Path::new("/proc/irq/1/../2/smp_affinity_list")));
    }
}
