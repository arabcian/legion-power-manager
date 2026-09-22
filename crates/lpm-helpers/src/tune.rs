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

use crate::{canonical_in_sysfs, read_trimmed, sysfs_write};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const CPU_DIR: &str = "/sys/devices/system/cpu";
pub const X3D_DRIVER_DIR: &str = "/sys/bus/platform/drivers/amd_x3d_vcache";
pub const DEBUGFS: &str = "/sys/kernel/debug";

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
    /// Computed from hardware (min freq, C-states, CCD roles…).
    Special,
}

#[derive(Clone, Copy)]
pub enum Target {
    File(&'static str),
    /// Same file name in every cpufreq/policy*.
    PerPolicy(&'static str),
    /// Per-policy `boost` (6.11+), else global cpufreq/boost.
    Boost,
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
}

const fn t(key: &'static str, group: &'static str, label: &'static str, help: &'static str,
           kind: Kind, options: Options, target: Target) -> Tunable {
    Tunable { key, group, label, help, kind, options, target, debugfs: false, caution: false }
}
const fn int(min: i64, max: i64) -> Kind { Kind::Int { min, max } }
const fn dbg(mut x: Tunable) -> Tunable { x.debugfs = true; x }
const fn warn(mut x: Tunable) -> Tunable { x.caution = true; x }
const BR: Options = Options::Bracketed;
const NO: Options = Options::None;

/// Keys that hot-plug CPUs: applied after every other knob, restored before them.
pub const HOTPLUG_KEYS: &[&str] = &["cpu.smt", "cpu.ccd_park"];

pub const TUNABLES: &[Tunable] = &[
    // ── CPU ───────────────────────────────────────────────────────────────
    t("cpu.pstate_status", "CPU", "amd-pstate mode",
      "active = EPP/CPPC decides frequency (recommended on Zen 2+); guided = kernel sets a floor, firmware the rest; passive = legacy governor control. Changing it resets per-policy governor/EPP, so it is applied first.",
      Kind::Choice, Options::Fixed(&["active", "guided", "passive"]), Target::File("/sys/devices/system/cpu/amd_pstate/status")),
    t("cpu.governor", "CPU", "Scaling governor",
      "In amd-pstate active mode, 'performance' pins EPP to performance; 'powersave' lets EPP below decide.",
      Kind::Choice, Options::ListFile("scaling_available_governors"), Target::PerPolicy("scaling_governor")),
    t("cpu.epp", "CPU", "Energy-performance preference",
      "EPP hint to CPPC firmware (active mode). performance = EPP 0, most aggressive boost; balance_performance is usually enough on AC and runs cooler.",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerPolicy("energy_performance_preference")),
    t("cpu.epp_boost", "CPU", "amd-pstate epp_boost",
      "Per-core EPP boost module parameter. Only on kernels with the (not upstream) epp_boost patch series.",
      Kind::Bool, NO, Target::File("/sys/module/amd_pstate/parameters/epp_boost")),
    t("cpu.boost", "CPU", "Core performance boost",
      "Turbo. Turn it off for thermal tests or to validate Curve Optimizer offsets at base clock.",
      Kind::Bool, NO, Target::Boost),
    t("cpu.min_freq", "CPU", "Minimum frequency",
      "lowest_nonlinear raises scaling_min_freq to the lowest efficient frequency: faster wake from idle at almost no power cost.",
      Kind::Choice, Options::Special, Target::MinFreq),
    t("cpu.x3d_mode", "CPU", "3D V-Cache CCD preference",
      "amd_x3d_vcache (6.13+): cache = schedule on the V-Cache CCD first (most games), frequency = higher-clocked CCD (compiles, ST work). Written with a 3 s timeout: some BIOSes stall in the ACPI _DSM.",
      Kind::Choice, Options::Fixed(&["frequency", "cache"]), Target::X3d),
    warn(t("cpu.cstate_max", "CPU", "Deepest C-state kept",
      "Disables every idle state deeper than the chosen one on all CPUs. Less wake-up jitter and fewer idle→boost crashes with aggressive CO offsets; costs idle power and heat.",
      Kind::Choice, Options::Special, Target::CState)),
    // ── Memory ────────────────────────────────────────────────────────────
    t("thp.enabled", "Memory", "THP enabled",
      "Transparent HugePages. madvise = only where the app asks (Proton/DXVK do); always can add compaction latency.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/enabled")),
    t("thp.shmem_enabled", "Memory", "THP shmem", "Huge pages for tmpfs/shmem mappings.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/shmem_enabled")),
    t("thp.defrag", "Memory", "THP defrag",
      "How hard a fault tries to get a huge page. defer+madvise avoids synchronous compaction stalls outside madvised regions.",
      Kind::Choice, BR, Target::File("/sys/kernel/mm/transparent_hugepage/defrag")),
    t("thp.khugepaged_defrag", "Memory", "khugepaged defrag",
      "0 = khugepaged never compacts in the background to build huge pages (one less source of periodic jitter).",
      int(0, 1), NO, Target::File("/sys/kernel/mm/transparent_hugepage/khugepaged/defrag")),
    t("mm.lru_gen", "Memory", "MGLRU enabled mask",
      "0x1 core, 0x2 batched leaf-PTE aging, 0x4 non-leaf. 7 = kernel default; clearing 0x2 makes reclaim cost grow with the game's address space.",
      int(0, 7), NO, Target::File("/sys/kernel/mm/lru_gen/enabled")),
    t("mm.lru_gen_min_ttl", "Memory", "MGLRU min_ttl_ms",
      "Working-set protection: pages younger than this are never evicted; under real pressure the OOM killer acts instead of the desktop thrashing. 1000 is a common desktop value, 0 = off.",
      int(0, 60_000), NO, Target::File("/sys/kernel/mm/lru_gen/min_ttl_ms")),
    t("mm.ksm_run", "Memory", "KSM run",
      "Kernel same-page merging. 0 = stop ksmd (no background page scanning), 1 = run, 2 = stop and unmerge.",
      int(0, 2), NO, Target::File("/sys/kernel/mm/ksm/run")),
    t("vm.max_map_count", "Memory", "vm.max_map_count",
      "Max memory mappings per process. Some Proton titles crash at the old 65530 default; SteamOS uses 2147483642.",
      int(65_530, 2_147_483_642), NO, Target::File("/proc/sys/vm/max_map_count")),
    t("vm.swappiness", "Memory", "vm.swappiness", "Swap tendency (higher with zram, lower with disk swap).",
      int(0, 200), NO, Target::File("/proc/sys/vm/swappiness")),
    t("vm.compaction_proactiveness", "Memory", "vm.compaction_proactiveness",
      "Background compaction. Keep it low but non-zero; never 0 together with watermark_boost_factor 0.",
      int(0, 100), NO, Target::File("/proc/sys/vm/compaction_proactiveness")),
    t("vm.watermark_boost_factor", "Memory", "vm.watermark_boost_factor",
      "Reaction to fragmentation events (kernel default 15000).", int(0, 100_000), NO, Target::File("/proc/sys/vm/watermark_boost_factor")),
    t("vm.watermark_scale_factor", "Memory", "vm.watermark_scale_factor",
      "Gap between min/low/high watermarks: larger = kswapd starts earlier, fewer direct-reclaim stalls.", int(1, 3000), NO,
      Target::File("/proc/sys/vm/watermark_scale_factor")),
    t("vm.min_free_kbytes", "Memory", "vm.min_free_kbytes",
      "Reserve kept free for atomic allocations.", int(1024, 4_194_304), NO, Target::File("/proc/sys/vm/min_free_kbytes")),
    t("vm.zone_reclaim_mode", "Memory", "vm.zone_reclaim_mode", "0 on single-node systems.",
      int(0, 7), NO, Target::File("/proc/sys/vm/zone_reclaim_mode")),
    t("vm.page_lock_unfairness", "Memory", "vm.page_lock_unfairness", "Page-lock steal retries before handing off fairly.",
      int(0, 10), NO, Target::File("/proc/sys/vm/page_lock_unfairness")),
    t("vm.stat_interval", "Memory", "vm.stat_interval",
      "vmstat refresh (s). Higher = fewer per-CPU timer wakeups, staler watermark statistics.", int(1, 120), NO,
      Target::File("/proc/sys/vm/stat_interval")),
    t("vm.page_cluster", "Memory", "vm.page-cluster", "Swap readahead (log2 pages). 0 for zram/SSD.",
      int(0, 6), NO, Target::File("/proc/sys/vm/page-cluster")),
    // ── Scheduler ─────────────────────────────────────────────────────────
    t("kernel.split_lock_mitigate", "Scheduler", "split_lock_mitigate",
      "0 = don't throttle split-lock offenders (~1000× core slowdown; some Windows games under Wine trigger it).", int(0, 1), NO,
      Target::File("/proc/sys/kernel/split_lock_mitigate")),
    t("kernel.watchdog", "Scheduler", "kernel.watchdog", "Soft/hard lockup watchdog (periodic per-CPU timers + NMI).",
      int(0, 1), NO, Target::File("/proc/sys/kernel/watchdog")),
    t("kernel.numa_balancing", "Scheduler", "kernel.numa_balancing",
      "Automatic NUMA page migration: pure page-fault sampling overhead on a single-node laptop.",
      int(0, 3), NO, Target::File("/proc/sys/kernel/numa_balancing")),
    t("kernel.timer_migration", "Scheduler", "kernel.timer_migration",
      "1 moves idle CPUs' timers to busy ones (fewer idle wakeups); 0 keeps timers local (more deterministic).",
      int(0, 1), NO, Target::File("/proc/sys/kernel/timer_migration")),
    t("kernel.sched_autogroup", "Scheduler", "sched_autogroup_enabled",
      "Per-session task groups. Needed for the launch boost's autogroup nice to matter.", int(0, 1), NO,
      Target::File("/proc/sys/kernel/sched_autogroup_enabled")),
    t("kernel.cfs_bandwidth_slice_us", "Scheduler", "sched_cfs_bandwidth_slice_us", "CFS bandwidth slice.",
      int(1, 1_000_000), NO, Target::File("/proc/sys/kernel/sched_cfs_bandwidth_slice_us")),
    dbg(t("sched.preempt", "Scheduler", "Preemption model (debugfs)",
      "PREEMPT_DYNAMIC kernels only. full = lowest latency for desktop/games; voluntary/lazy favour throughput (compiles). lazy needs 6.13+.",
      Kind::Choice, Options::Fixed(&["none", "voluntary", "full", "lazy"]), Target::File("/sys/kernel/debug/sched/preempt"))),
    dbg(t("sched.base_slice_ns", "Scheduler", "EEVDF base slice (debugfs)",
      "Smaller = lower latency, slightly lower throughput (default 3 ms · log2 CPUs, capped).", int(100_000, 100_000_000), NO,
      Target::File("/sys/kernel/debug/sched/base_slice_ns"))),
    dbg(t("sched.min_base_slice_ns", "Scheduler", "min_base_slice_ns (debugfs)",
      "Same knob under the name some patched kernels (and lutris-game-tune) use.", int(100_000, 100_000_000), NO,
      Target::File("/sys/kernel/debug/sched/min_base_slice_ns"))),
    dbg(t("sched.migration_cost_ns", "Scheduler", "migration_cost_ns (debugfs)", "Cache-hot migration threshold.",
      int(0, 100_000_000), NO, Target::File("/sys/kernel/debug/sched/migration_cost_ns"))),
    dbg(t("sched.nr_migrate", "Scheduler", "nr_migrate (debugfs)", "Tasks moved per load-balance pass (default 32).",
      int(1, 1024), NO, Target::File("/sys/kernel/debug/sched/nr_migrate"))),
    t("wq.power_efficient", "Scheduler", "workqueue power_efficient",
      "N keeps per-CPU workqueues local instead of pushing them to unbound (lower latency).", Kind::Bool, NO,
      Target::File("/sys/module/workqueue/parameters/power_efficient")),
    t("wq.cpumask", "Scheduler", "Unbound workqueue CPUs",
      "Confines unbound kernel workqueues (writeback, crypto, fs work) to one CCD so they stay off the game's cores. Pair with a launch affinity on the other CCD.",
      Kind::Choice, Options::Special, Target::WqCpumask),
    t("irq.affinity", "Scheduler", "IRQ affinity",
      "Steers device interrupts to one CCD. Best effort: kernel-managed IRQs (NVMe queues…) refuse and are skipped. Stock = spread over all CPUs.",
      Kind::Choice, Options::Special, Target::Irq),
    // ── Storage ───────────────────────────────────────────────────────────
    t("blk.scheduler", "Storage", "I/O scheduler",
      "none = lowest latency on NVMe; mq-deadline / kyber / bfq trade latency for fairness under mixed load. Every whole disk.",
      Kind::Choice, BR, Target::PerBlock("scheduler")),
    t("blk.wbt_lat_usec", "Storage", "Writeback throttling (µs)",
      "Target read latency for writeback throttling. 0 = off: no throttling, but a big write burst (shader cache, download) can delay reads.",
      int(0, 1_000_000), NO, Target::PerBlock("wbt_lat_usec")),
    t("blk.read_ahead_kb", "Storage", "Read-ahead (KiB)",
      "Sequential read-ahead (default 128). Larger helps streaming assets from big packed files, smaller helps random I/O.",
      int(0, 16_384), NO, Target::PerBlock("read_ahead_kb")),
    // ── Devices ───────────────────────────────────────────────────────────
    t("pci.aspm", "Devices", "PCIe ASPM policy",
      "performance cuts link wake-up latency (GPU/NVMe jitter); default/performance also cures NVMe or Wi-Fi dropouts.",
      Kind::Choice, BR, Target::File("/sys/module/pcie_aspm/parameters/policy")),
    t("pci.latency_timer", "Devices", "PCI latency timers",
      "lutris-game-tune's setpci step, done natively: host bridge 00, PCI bridges 80, other functions 20. Only legacy PCI honours it; PCIe functions hardwire 0.",
      Kind::Choice, Options::Special, Target::PciLatency),
    t("snd.hda_power_save", "Devices", "HDA power_save (s)", "0 = never power the codec down (no pops, no wake latency).",
      int(0, 3600), NO, Target::File("/sys/module/snd_hda_intel/parameters/power_save")),
    t("snd.hda_power_save_controller", "Devices", "HDA power_save_controller", "Allow the HDA controller to power down.",
      Kind::Bool, NO, Target::File("/sys/module/snd_hda_intel/parameters/power_save_controller")),
    t("usb.autosuspend", "Devices", "USB autosuspend delay (s)",
      "Default for newly bound USB devices. -1 = never suspend (no wake lag on mice, pads, DACs). Already-bound devices keep theirs.",
      int(-1, 3600), NO, Target::File("/sys/module/usbcore/parameters/autosuspend")),
    t("gpu.amdgpu_dpm", "Devices", "iGPU DPM level (amdgpu)",
      "low leaves more of the shared SoC power budget to the CPU cores while the dGPU renders; auto = driver managed.",
      Kind::Choice, Options::Fixed(&["auto", "low", "high"]), Target::AmdgpuDpm),
    // ── Stability ─────────────────────────────────────────────────────────
    t("mce.check_interval", "Stability", "MCE poll interval (s)",
      "How often correctable machine-check errors are polled (stock 300). 10 while validating Curve Optimizer offsets catches a marginal core fast; watch dmesg or rasdaemon.",
      int(0, 3600), NO, Target::Mce),
    // ── Hot-plug (must stay last, see HOTPLUG_KEYS) ───────────────────────
    warn(t("cpu.smt", "CPU", "SMT",
      "Simultaneous multithreading. Some games get better 1% lows with it off — test per game. Hot-plugs half the CPUs.",
      Kind::Choice, Options::Fixed(&["on", "off"]), Target::File("/sys/devices/system/cpu/smt/control"))),
    warn(t("cpu.ccd_park", "CPU", "Park a CCD (offline)",
      "Takes one CCD fully offline: no scheduling, no IRQs, no cross-CCD traffic. The most deterministic X3D setup, but halves the core count until restored. cpu0's CCD cannot be parked.",
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

/// L3 domains of the online CPUs, sorted by first CPU. A parked CCD has no
/// online CPU and does not appear.
pub fn ccx_groups() -> Vec<Ccx> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (_, d) in cpus() {
        let idx = d.join("cache/index3");
        if read(&idx.join("level")).as_deref() != Some("3") { continue; }
        let Some(list) = read(&idx.join("shared_cpu_list")) else { continue };
        if seen.contains(&list) { continue; }
        seen.push(list.clone());
        let cpus = cpu_list(&list);
        let max_khz = cpus.iter()
            .filter_map(|c| read(&Path::new(CPU_DIR).join(format!("cpu{c}/cpufreq/cpuinfo_max_freq"))))
            .filter_map(|s| s.parse().ok()).max().unwrap_or(0);
        out.push(Ccx { index: 0, cpus, l3_kib: read(&idx.join("size")).map_or(0, |s| size_kib(&s)), max_khz });
    }
    out.sort_by_key(|g| g.cpus.first().copied().unwrap_or(0));
    for (i, g) in out.iter_mut().enumerate() { g.index = i; }
    out
}

/// "cache" | "frequency" | "ccdN" -> that L3 domain. None on single-CCD
/// parts or when the role does not tell the dies apart.
pub fn resolve_ccd(groups: &[Ccx], role: &str) -> Option<Ccx> {
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
    v
}

fn possible_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("possible")).map(|s| cpu_list(&s)).unwrap_or_default() }
fn present_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("present")).map(|s| cpu_list(&s)).unwrap_or_default() }
fn online_cpus() -> Vec<usize> { read(&Path::new(CPU_DIR).join("online")).map(|s| cpu_list(&s)).unwrap_or_default() }

/// Which role option describes this CPU set, if any.
fn role_matching(groups: &[Ccx], set: &[usize]) -> Option<String> {
    for role in ["cache", "frequency"] {
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
pub fn best_effort(t: &Tunable) -> bool { matches!(t.target, Target::Irq | Target::PciLatency) }

// ── concrete files per tunable ────────────────────────────────────────────

/// Every concrete file the tunable writes. Empty = not available here.
pub fn files(t: &Tunable) -> Vec<PathBuf> {
    let existing = |p: PathBuf| if p.is_file() { vec![p] } else { vec![] };
    match t.target {
        // smt/control also reports notsupported / forceoff / notimplemented: not writable then.
        Target::File(p) if p.ends_with("/smt/control") =>
            if matches!(read(Path::new(p)).as_deref(), Some("on") | Some("off")) { vec![PathBuf::from(p)] } else { vec![] },
        Target::File(p) => existing(PathBuf::from(p)),
        Target::PerPolicy(f) => policies().into_iter().map(|p| p.join(f)).filter(|p| p.is_file()).collect(),
        Target::MinFreq => policies().into_iter().map(|p| p.join("scaling_min_freq")).filter(|p| p.is_file()).collect(),
        Target::Boost => {
            let per: Vec<_> = policies().into_iter().map(|p| p.join("boost")).filter(|p| p.is_file()).collect();
            if per.is_empty() { existing(Path::new(CPU_DIR).join("cpufreq/boost")) } else { per }
        }
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
        Target::WqCpumask => if ccx_groups().len() > 1 {
            existing(PathBuf::from("/sys/devices/virtual/workqueue/cpumask"))
        } else { vec![] },
        Target::Irq => if ccx_groups().len() > 1 { irq_files() } else { vec![] },
        Target::CcdPark => {
            // Stays available while a CCD is parked, so "none" can bring it back.
            let parked = online_cpus().len() < present_cpus().len();
            if ccx_groups().len() < 2 && !parked { return vec![]; }
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
            same(p.and_then(|p| read(&p)).map(|s| s.split_whitespace().map(str::to_owned).collect()).unwrap_or_default())
        }
        (Options::Bracketed, Target::File(p)) => same(read(Path::new(p)).map(|s| parse_bracketed(&s).1).unwrap_or_default()),
        (Options::Bracketed, Target::PerBlock(_)) => {
            // Options every disk supports (a preset must be writable everywhere).
            let mut common: Option<Vec<String>> = None;
            for f in files(t) {
                let o = read(&f).map(|s| parse_bracketed(&s).1).unwrap_or_default();
                common = Some(match common { None => o, Some(c) => c.into_iter().filter(|x| o.contains(x)).collect() });
            }
            same(common.unwrap_or_default())
        }
        (Options::Special, Target::MinFreq) => vec![
            ("lowest_nonlinear".into(), "lowest_nonlinear (efficient floor)".into()),
            ("cpuinfo_min".into(), "cpuinfo_min (hardware minimum)".into()),
        ],
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
            let online = online_cpus();
            let off: Vec<usize> = present_cpus().into_iter().filter(|c| !online.contains(c)).collect();
            return Some(if off.is_empty() { "none".into() } else { format!("offline {}", fmt_cpu_list(&off)) });
        }
        _ => {}
    }
    let mut vals = fs.iter().filter_map(|p| read(p)).map(|raw| match t.kind {
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
            let g = role(value)?;
            if g.cpus.contains(&0) { return Err("the CCD holding cpu0 cannot be parked".into()); }
            g.cpus.iter().map(|c| Path::new(CPU_DIR).join(format!("cpu{c}/online")))
                .map(|p| if p.is_file() { Ok((p, "0".to_owned())) } else { Err(format!("{} missing", p.display())) })
                .collect::<Result<Vec<_>, _>>()?
        }
        _ if t.kind == Kind::Bool => fs.into_iter().map(|f| {
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
    if matches!(t.target, Target::PciLatency) { return pci_latency_read(f).map(|b| format!("{b:02x}")); }
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
    })
}

/// Tunable list for the GUI.
pub fn describe() -> Value {
    let rows: Vec<Value> = TUNABLES.iter().map(|t| {
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
    fn bracketed() {
        let (s, o) = parse_bracketed("always [madvise] never");
        assert_eq!(s.as_deref(), Some("madvise"));
        assert_eq!(o, vec!["always", "madvise", "never"]);
        assert_eq!(parse_bracketed("none voluntary (full) lazy").0.as_deref(), Some("full"));
        assert_eq!(restorable("[none] mq-deadline kyber"), "none");
        assert_eq!(restorable("2000"), "2000");
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
