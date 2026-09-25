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
    *V.get_or_init(|| std::fs::read_to_string("/proc/cpuinfo").map(|s| vendor_from_cpuinfo(&s)).unwrap_or(Vendor::Any))
}

fn vendor_ok(t: &Tunable) -> bool { t.vendor == Vendor::Any || t.vendor == cpu_vendor() }

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
      "EPP boost module parameter (global; the patch series has no per-policy knob). Only on kernels with the (not upstream) epp_boost patch. Leave off unless you specifically built a kernel with this patch and want EPP to react faster; harmless no-op otherwise, the row will show n/a.",
      Kind::Bool, NO, Target::File("/sys/module/amd_pstate/parameters/epp_boost"))),
    t("cpu.boost", "CPU", "Core performance boost",
      "Turbo (core performance boost). Leave it on for normal use - this is not a fps knob, it only removes the clock ceiling above base. On Intel this drives intel_pstate/no_turbo (inverted, so 1 still means turbo on). Turn it off for two specific jobs: (1) thermal/fan-curve testing where you want repeatable numbers, (2) validating undervolt/Curve Optimizer offsets, since boost clocks typically first expose an unstable core (use the CO validation preset, which also widens C-states and shortens the MCE poll).",
      Kind::Bool, NO, Target::Boost),
    t("cpu.min_freq", "CPU", "Minimum frequency",
      "(Only 'cpuinfo_min' is offered on Intel: intel_pstate has no lowest-nonlinear file and HWP already avoids the inefficient range on its own.) lowest_nonlinear raises the CPU's idle floor to amd_pstate_lowest_nonlinear_freq (typically 400-600 MHz above the hardware minimum): frequencies below that point are inefficient on Zen, disproportionate wake-up latency for negligible power savings. Safe to enable for every scenario, including battery; the lowest-risk, no-downside row on this whole tab.",
      Kind::Choice, Options::Special, Target::MinFreq),
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
      "How many empty (never-touched) 4 KB slots khugepaged accepts when collapsing a 2 MB range into a huge page. 511 (default) = collapse even an almost empty range, which fills in memory the program never used: with THP enabled=always this is the main source of RSS bloat. 0-64 = only collapse ranges that are really in use - much less wasted memory, huge pages still form where they help. Has no effect with THP off.",
      int(0, 511), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/max_ptes_none")),
    t("thp.khp_pages_to_scan", "Memory", "khugepaged pages_to_scan",
      "Pages khugepaged examines per wake-up. Default 4096. Lower = gentler background scanning (less periodic CPU work and lock contention), huge pages form more slowly; higher = faster promotion for long-running processes at the cost of more background work.",
      int(8, 262_144), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/pages_to_scan")),
    t("thp.khp_scan_sleep_ms", "Memory", "khugepaged scan_sleep_millisecs",
      "Pause between khugepaged scan passes. Default 10000 (10 s). Longer = fewer background scan bursts (useful while gaming or on battery); shorter = huge pages form sooner.",
      int(0, 600_000), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/scan_sleep_millisecs")),
    t("thp.khp_alloc_sleep_ms", "Memory", "khugepaged alloc_sleep_millisecs",
      "How long khugepaged backs off after failing to allocate a huge page (memory fragmented). Default 60000. Longer = less futile compaction work under memory pressure.",
      int(0, 600_000), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/alloc_sleep_millisecs")),
    t("mm.lru_gen", "Memory", "MGLRU enabled mask",
      "Multi-Gen LRU feature bitmask: 0x1 core MGLRU, 0x2 batched leaf-PTE young-bit aging (scales reclaim cost to the accessed set instead of the whole address space - what keeps reclaim cheap for a game with a huge virtual address space but a much smaller hot set), 0x4 non-leaf PMD aging. 7 (all three) is the kernel default and normally the right value; clearing 0x2 makes reclaim scan cost grow with the game's total mapped memory rather than its working set, which shows up as reclaim-related stutter under pressure. Only change this if debugging MGLRU itself.",
      int(0, 7), NO, Target::File("/sys/kernel/mm/lru_gen/enabled")),
    t("mm.lru_gen_min_ttl", "Memory", "MGLRU min_ttl_ms",
      "Pages younger than this many milliseconds are never evicted, even under memory pressure - protects whatever you touched most recently (the game's active working set) from being paged out to make room elsewhere. 1000 (1s) is a common desktop value and what the Gaming X3D preset uses: under real pressure the kernel prefers invoking the OOM killer over evicting protected pages, which in practice means a background process gets killed instead of your game stalling on page faults. Push higher (e.g. 5000) with plenty of RAM if you never want the foreground app touched; 0 disables the protection (kernel default).",
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
      "How hard kswapd reacts (raises watermarks temporarily) when it detects fragmentation is about to cause a high-order allocation failure. Kernel default 15000 (roughly 1.5% of a zone). Leave at default unless debugging allocation stalls specifically; not a knob most gaming setups need to touch.",
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
    dbg(t("sched.preempt", "Scheduler", "Preemption model (debugfs)",
      "Live-switchable preemption model on PREEMPT_DYNAMIC kernels (shows 'root only' if the kernel was not built with it, or debugfs is not mounted - the row still activates, root just cannot read the current value from an unprivileged describe). full = a running task can be preempted almost anywhere: lowest latency, right for desktop/gaming and what the gaming presets set. voluntary = only at explicit preemption points: slightly higher latency, slightly higher throughput, a good middle ground. none = cooperative-style, maximum throughput minimum latency guarantees, essentially never wanted on a desktop. lazy (6.13+) = full's latency behaviour with some of voluntary's throughput via deferred preemption; use it for compile-heavy presets if your kernel supports it, otherwise voluntary is the fallback (see the Compile throughput preset).",
      Kind::Choice, Options::Fixed(&["none", "voluntary", "full", "lazy"]), Target::File("/sys/kernel/debug/sched/preempt"))),
    dbg(t("sched.base_slice_ns", "Scheduler", "EEVDF base slice (debugfs)",
      "EEVDF scheduler's base time slice in nanoseconds - roughly, how long a task runs before it becomes fair game for preemption by an equally-important task. Kernel default scales as ~3 ms times log2(CPU count), capped. Smaller (e.g. 1000000 = 1 ms, what the gaming presets use) means the scheduler re-evaluates fairness more often: lower worst-case latency for anything waiting its turn, at a small throughput cost from more frequent context switches. Larger (e.g. 3000000 = 3 ms, the Compile throughput preset) favours throughput: fewer switches, slightly higher latency for anything waiting.",
      int(100_000, 100_000_000), NO, Target::File("/sys/kernel/debug/sched/base_slice_ns"))),
    dbg(t("sched.min_base_slice_ns", "Scheduler", "min_base_slice_ns (debugfs)",
      "Identical purpose to the base_slice_ns row above; some patched kernels - including the one lutris-game-tune was written against - expose this same tunable under this alternate filename instead. Whichever of the two files exists on your kernel is the one that is 'available'; set both rows the same and only the real one actually writes.",
      int(100_000, 100_000_000), NO, Target::File("/sys/kernel/debug/sched/min_base_slice_ns"))),
    dbg(t("sched.migration_cost_ns", "Scheduler", "migration_cost_ns (debugfs)",
      "How long (ns) a task must have run before the scheduler treats it as cache-cold and freely migratable without a locality penalty. Lower migrates more readily for better load balance; higher keeps tasks pinned longer for better cache locality. Niche - the kernel default suits almost everyone.",
      int(0, 100_000_000), NO, Target::File("/sys/kernel/debug/sched/migration_cost_ns"))),
    dbg(t("sched.nr_migrate", "Scheduler", "nr_migrate (debugfs)",
      "Maximum tasks moved in one load-balancing pass (default 32). Lower reduces burstiness per pass at the cost of correcting large imbalances more slowly; higher corrects faster but does more work per pass. Leave at default unless profiling scheduler balancing specifically.",
      int(1, 1024), NO, Target::File("/sys/kernel/debug/sched/nr_migrate"))),
    t("wq.power_efficient", "Scheduler", "workqueue power_efficient",
      "When enabled (Y/1, the kernel's power-saving default), some per-CPU kernel workqueues are allowed to migrate to an unbound worker to save power. Disabling it (N/0, what the gaming presets set) keeps that work pinned to the CPU that queued it - marginally lower latency for whatever depends on that work completing promptly, at a small power-efficiency cost. Combine with the 'Unbound workqueue CPUs' row below to also steer the workqueues that are unbound by design onto a specific CCD.",
      Kind::Bool, NO, Target::File("/sys/module/workqueue/parameters/power_efficient")),
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
      "Polling interval (seconds) for correctable machine-check errors (early-warning signs of a marginal core/memory, short of a full crash). Stock is 300s. 10s (what the CO validation preset uses) catches a marginal Curve Optimizer offset within seconds of it starting to misbehave instead of up to 5 minutes later - pair with `dmesg -w` or rasdaemon open in a terminal while stress-testing a new offset. Set back to something relaxed (or 0 to stop polling) for normal use; frequent polling has a small but real overhead not worth paying permanently.",
      int(0, 3600), NO, Target::Mce),
    // ── Hot-plug (must stay last, see HOTPLUG_KEYS) ───────────────────────
    warn(t("cpu.smt", "CPU", "SMT",
      "Turns SMT (the second logical thread per physical core) on or off system-wide. Most games are unaffected or slightly faster with SMT on (more threads available); a minority of titles - especially ones sensitive to cache contention between sibling threads, or with poor thread-count scaling - show better 1% lows with it off, since every physical core is then dedicated to one thread with no sibling contention. This is genuinely game-specific: test SMT on vs off on the specific title if chasing 1% lows. Hot-plugs half the CPUs off/online, which is why this row is always applied last and restored first - every other per-CPU setting needs the CPU online first to accept the write.",
      Kind::Choice, Options::Fixed(&["on", "off"]), Target::File("/sys/devices/system/cpu/smt/control"))),
    warn(t("cpu.ccd_park", "CPU", "Park a CCD / E-cores (offline)",
      "On a hybrid Intel CPU this parks the E-cores instead (the P-cores hold cpu0 and can never be parked): the game then only ever shares the ring with P-cores - a test tool for titles with bad hybrid scheduling, not a daily setting. Takes an entire CCD fully offline (every CPU in it): no scheduling, no IRQs, no cross-CCD cache-coherency traffic can reach it at all. The most deterministic possible setup for an X3D chip - the game gets sole, uncontested use of one die's cache and cores with zero interference from the other die under any circumstance - at the obvious cost of losing that die's cores entirely until restored. The Competitive preset parks the frequency CCD as its most aggressive step; only reach for this if affinity plus workqueue/IRQ steering (which achieve most of the isolation benefit without losing any cores) is not enough for what you are chasing. cpu0's CCD can never be parked (the kernel needs cpu0 online), so on a 2-CCD chip you can only ever park 'the other one'.",
      Kind::Choice, Options::Special, Target::CcdPark)),
];

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

pub fn policies() -> Vec<PathBuf> {
    numbered(&Path::new(CPU_DIR).join("cpufreq"), "policy").into_iter().map(|x| x.1).collect()
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

fn cstate_names() -> Vec<String> {
    numbered(&Path::new(CPU_DIR).join("cpu0/cpuidle"), "state").into_iter()
        .map(|(i, p)| read(&p.join("name")).unwrap_or_else(|| format!("state{i}")))
        .collect()
}

pub fn debugfs_mounted() -> bool {
    std::fs::read_to_string("/proc/self/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some(DEBUGFS)))
        .unwrap_or(false)
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
}

/// Forget the cached topology (after writing cpu*/online or smt/control).
pub fn invalidate_topology() { TOPO.with(|t| *t.borrow_mut() = None); }

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
    let mut v = Vec::new();
    for (role, what) in [("cache", "V-Cache CCD"), ("frequency", "frequency CCD")] {
        if let Some(g) = resolve_ccd(groups, role).filter(|g| usable(g)) {
            v.push((role.to_owned(), format!("{what} (CCD{}: {})", g.index, fmt_cpu_list(&g.cpus))));
        }
    }
    if groups.len() >= 2 {
        for g in groups.iter().filter(|g| usable(g)) {
            v.push((format!("ccd{}", g.index), format!("CCD{} ({}, {} MB L3)", g.index, fmt_cpu_list(&g.cpus), g.l3_kib / 1024)));
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
    if cpus.is_empty() || offline != cpus.as_slice() { return None; }
    let role = v["role"].as_str()?.to_owned();
    let label = v["label"].as_str().map(str::to_owned).unwrap_or_else(|| format!("park {role}"));
    Some((role, label, cpus))
}

/// Called by tune-helper (root) after a successful park.
pub fn record_ccd_park(role: &str, label: Option<&str>) -> Result<(), String> {
    if role == "none" {
        let _ = std::fs::remove_file(CCD_PARK_RECORD);
        return Ok(());
    }
    let body = serde_json::json!({"role": role, "label": label, "cpus": offline_cpus()});
    crate::write_root_file(CCD_PARK_RECORD, body.to_string().as_bytes())
}

fn possible_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("possible")).map(|s| cpu_list(&s)).unwrap_or_default() }
fn present_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("present")).map(|s| cpu_list(&s)).unwrap_or_default() }
fn online_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("online")).map(|s| cpu_list(&s)).unwrap_or_default() }

/// Which role option describes this CPU set, if any.
fn role_matching(groups: &[Ccx], set: &[usize]) -> Option<String> {
    for role in ["cache", "frequency", "pcore", "ecore"] {
        if resolve_ccd(groups, role).map_or(false, |g| g.cpus == set) { return Some(role.into()); }
    }
    groups.iter().find(|g| g.cpus == set).map(|g| format!("ccd{}", g.index))
}

// ── PCI latency timer ────────────────────────────────────────────────────

const PCI_DEVICES: &str = "/sys/bus/pci/devices";
const PCI_LATENCY_OFFSET: u64 = 0x0D;

fn pci_config_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(PCI_DEVICES).into_iter().flatten().flatten()
        .filter_map(|e| canonical_in_sysfs(&e.path().join("config")))
        .filter(|p| is_pci_config(p))
        .collect();
    v.sort();
    v
}

fn is_pci_config(p: &Path) -> bool {
    p.file_name().map_or(false, |n| n == "config") && p.starts_with("/sys/devices")
        && p.parent().map_or(false, |d| d.join("vendor").is_file() && d.join("class").is_file())
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

fn block_devs() -> Vec<PathBuf> {
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
fn inverted(f: &Path) -> bool { f.file_name().map_or(false, |n| n == "no_turbo") }

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
pub fn best_effort(t: &Tunable) -> bool { matches!(t.target, Target::Irq | Target::PciLatency | Target::PciAspm) }

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
    let reader = std::thread::spawn(move || { let mut v = Vec::new(); let _ = (&mut out).take(64 * 1024).read_to_end(&mut v); v });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(std::time::Duration::from_millis(10)),
            _ => { let _ = child.kill(); let _ = child.wait(); return None; }
        }
    };
    Some((status.success(), String::from_utf8_lossy(&reader.join().ok()?).into_owned()))
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
    for e in std::fs::read_dir(PCI_DEVICES).into_iter().flatten().flatten() {
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

/// Writes one tunable value to one concrete target, dispatching the targets
/// that are not plain files. Everything else goes through write_checked.
pub fn write_value(t: &Tunable, f: &Path, data: &str) -> Result<(), String> {
    match t.target {
        Target::WifiPowerSave => wifi_set(f, data),
        Target::SchedExt => scx_set(data),
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
        Target::File(p) => existing(PathBuf::from(p)),
        // scaling_governor / energy_performance_preference: on a 2+ CCD chip the
        // per-CCD override rows below cover the same file set (and always run
        // after this row, so leaving both visible just invites setting one and
        // wondering why the other value stuck). Hide the global row there and
        // point people at Governor/EPP · CCDn instead; a single-CCD chip has no
        // such rows, so the global one is the only way to set this and stays.
        Target::PerPolicy(f) if ccx_groups().len() > 1 => { let _ = f; vec![] }
        Target::PerPolicy(f) => policies().into_iter().map(|p| p.join(f)).filter(|p| p.is_file()).collect(),
        Target::MinFreq => policies().into_iter().map(|p| p.join("scaling_min_freq")).filter(|p| p.is_file()).collect(),
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
        Target::PciLatency => pci_config_files(),
        Target::WqCpumask => if has_domains() {
            existing(PathBuf::from("/sys/devices/virtual/workqueue/cpumask"))
        } else { vec![] },
        Target::Irq => if has_domains() { irq_files() } else { vec![] },
        Target::WifiPowerSave => wifi_ifaces(),
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
        (Options::Special, Target::MinFreq) => {
            let mut v = Vec::new();
            // amd-pstate only; intel_pstate has no such file.
            if policies().first().map_or(false, |p| p.join("amd_pstate_lowest_nonlinear_freq").is_file()) {
                v.push(("lowest_nonlinear".into(), "lowest_nonlinear (efficient floor)".into()));
            }
            v.push(("cpuinfo_min".into(), "cpuinfo_min (hardware minimum)".into()));
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
pub fn current(t: &Tunable) -> Option<String> {
    let fs = files(t);
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
        Target::MinFreq => {
            let pol = policies().into_iter().next()?;
            let cur = read(&pol.join("scaling_min_freq"))?;
            if read(&pol.join("amd_pstate_lowest_nonlinear_freq")).as_deref() == Some(cur.as_str()) { return Some("lowest_nonlinear".into()); }
            if read(&pol.join("cpuinfo_min_freq")).as_deref() == Some(cur.as_str()) { return Some("cpuinfo_min".into()); }
            return Some(format!("{cur} kHz"));
        }
        Target::PciLatency => {
            // Hardwired-zero (PCIe) functions ignore the write; they don't make it "stock".
            let tuned = fs.iter().all(|f| pci_latency_read(f).map_or(true, |b| b == pci_latency_target(f) || b == 0));
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
            if let Some((role, _, _)) = parked_record() { return Some(role); }
            let online = online_cpus();
            let off: Vec<usize> = present_cpus().into_iter().filter(|c| !online.contains(c)).collect();
            if !off.is_empty() && hybrid().map_or(false, |h| h.ecores == off) { return Some("ecore".into()); }
            return Some(if off.is_empty() { "none".into() } else { format!("offline {}", fmt_cpu_list(&off)) });
        }
        Target::SchedExt => return scx_current(),
        Target::WifiPowerSave => {
            let mut vals = fs.iter().filter_map(|f| wifi_get(f));
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
        Target::MinFreq => fs.iter().map(|f| {
            let dir = f.parent().unwrap();
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
            if value == "none" { return Ok(fs.into_iter().map(|f| (f, "1".to_owned())).collect()); }
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
        Target::WifiPowerSave => fs.into_iter().map(|f| (f, value.to_owned())).collect(),
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
        Target::SchedExt => return scx_current(),
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
    if f.file_name().map_or(false, |n| n == "online" || n == "control") { invalidate_topology(); }
    r
}

fn write_checked_inner(f: &Path, data: &str) -> Result<(), String> {
    let s = f.to_string_lossy();
    if s.starts_with("/proc/") {
        let ok = is_irq_file(f)
            || (s.starts_with("/proc/sys/") && TUNABLES.iter().any(|t| matches!(t.target, Target::File(p) if p == s)));
        if !ok { return Err(format!("{s}: refused (outside the allowlist)")); }
        return sysfs_write(f, data.as_bytes()).map_err(|e| format!("{s}: {e}"));
    }
    let Some(real) = canonical_in_sysfs(f) else { return Err(format!("{s}: refused (outside the allowlist)")) };
    if real.file_name().map_or(false, |n| n == "config") {
        if !is_pci_config(&real) { return Err(format!("{s}: refused (not a PCI config file)")); }
        return pci_latency_write(&real, data).map_err(|e| format!("{s}: {e}"));
    }
    if real.file_name().map_or(false, |n| n == "amd_x3d_mode") {
        return write_with_timeout(real, data.to_owned()).map_err(|e| format!("{s}: {e}"));
    }
    sysfs_write(&real, data.as_bytes()).map_err(|e| format!("{s}: {e}"))
}

/// amd_x3d_mode goes through a synchronous ACPI _DSM that stalls forever on
/// some BIOS/AGESA versions (lutris-game-tune wraps it in `timeout 3`). A
/// stuck write is abandoned so the rest of the batch and the reply still happen.
fn write_with_timeout(p: PathBuf, data: String) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || { let _ = tx.send(sysfs_write(&p, data.as_bytes()).map_err(|e| e.to_string())); });
    rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|_| Err("timed out after 3 s (ACPI _DSM stall?)".into()))
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
            "key": t.key, "group": t.group, "label": t.label, "help": t.help,
            "kind": match t.kind { Kind::Choice => "choice", Kind::Int { .. } => "int", Kind::Bool => "bool" },
            "options": opts, "min": min, "max": max,
            // debugfs is root-only (0700): unprivileged callers cannot tell; root checks at write time.
            "available": !fs.is_empty() || t.debugfs,
            "debugfs": t.debugfs, "caution": t.caution, "hotplug": is_hotplug(t.key),
            "current": current(t),
            "files": fs.len(),
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
        for k in ["cpu.idle_governor", "thp.mthp_64k", "thp.khp_max_ptes_none", "net.tcp_congestion",
                  "net.default_qdisc", "net.wifi_power_save", "pci.aspm_links", "sched.ext"] {
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
        let b = find("wq.power_efficient").unwrap();
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
        assert!(role_options(&g, true).iter().all(|(k, _)| k != "cache" && k != "ccd0"));
        assert_eq!(role_matching(&g, &(8..16).chain(24..32).collect::<Vec<_>>()).as_deref(), Some("frequency"));
        assert_eq!(role_matching(&g, &[0, 1]), None);
    }
    #[test]
    fn same_values() {
        let wq = find("wq.cpumask").unwrap();
        assert!(same_value(wq, "0000ff00", "ff00"));
        let irq = find("irq.affinity").unwrap();
        assert!(same_value(irq, "0-3", "0,1,2,3"));
        let b = find("wq.power_efficient").unwrap();
        assert!(same_value(b, "Y", "1") && !same_value(b, "Y", "N"));
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
