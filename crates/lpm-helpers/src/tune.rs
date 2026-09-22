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
    /// Same file in the cpufreq policies covering one CCD (index into ccx_groups).
    PerCcdPolicy(&'static str, usize),
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
      "active = EPP/CPPC decides frequency (recommended on Zen 2+); guided = kernel sets a floor, firmware the rest; passive = legacy governor control. Leave on active unless you have a specific reason: guided/passive exist mainly for older firmware or debugging odd boost behaviour. Changing it resets every per-policy governor/EPP value underneath, which is why this row is always applied first and restored last of the non-hot-plug rows.",
      Kind::Choice, Options::Fixed(&["active", "guided", "passive"]), Target::File("/sys/devices/system/cpu/amd_pstate/status")),
    t("cpu.governor", "CPU", "Scaling governor",
      "In amd-pstate active mode this mostly gates EPP: 'performance' pins EPP to 0 regardless of the EPP row below, 'powersave' lets EPP decide. For gaming keep 'powersave' here and control behaviour with EPP instead - EPP has finer steps (5 levels vs 2) and swaps faster. Set 'performance' only for a fixed worst-case floor (e.g. Competitive preset) or on kernels/firmware where EPP is ignored.",
      Kind::Choice, Options::ListFile("scaling_available_governors"), Target::PerPolicy("scaling_governor")),
    t("cpu.epp", "CPU", "Energy-performance preference",
      "EPP hint to CPPC firmware (active mode), five steps from most aggressive to most efficient. Needs governor 'powersave' to take effect. Pick by scenario: competitive/latency-sensitive -> performance; general gaming on AC -> balance_performance (usually indistinguishable in fps, noticeably cooler/quieter); on battery or light desktop work -> balance_power; power -> power-priority, expect lower sustained clocks. If frame times feel spiky right after a load change, try balance_performance before touching anything else - that spikiness is often EPP being too cautious to boost.",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerPolicy("energy_performance_preference")),
    t("cpu.epp_boost", "CPU", "amd-pstate epp_boost",
      "EPP boost module parameter (global; the patch series has no per-policy knob). Only on kernels with the (not upstream) epp_boost patch. Leave off unless you specifically built a kernel with this patch and want EPP to react faster; harmless no-op otherwise, the row will show n/a.",
      Kind::Bool, NO, Target::File("/sys/module/amd_pstate/parameters/epp_boost")),
    t("cpu.boost", "CPU", "Core performance boost",
      "Turbo (core performance boost). Leave it on for normal use - this is not a fps knob, it only removes the clock ceiling above base. Turn it off for two specific jobs: (1) thermal/fan-curve testing where you want repeatable numbers, (2) validating Ryzen Curve Optimizer offsets, since boost clocks typically first expose an unstable core (use the CO validation preset, which also widens C-states and shortens the MCE poll).",
      Kind::Bool, NO, Target::Boost),
    t("cpu.min_freq", "CPU", "Minimum frequency",
      "lowest_nonlinear raises the CPU's idle floor to amd_pstate_lowest_nonlinear_freq (typically 400-600 MHz above the hardware minimum): frequencies below that point are inefficient on Zen, disproportionate wake-up latency for negligible power savings. Safe to enable for every scenario, including battery; the lowest-risk, no-downside row on this whole tab.",
      Kind::Choice, Options::Special, Target::MinFreq),
    // Per-CCD overrides: applied after the global rows above, so a preset can
    // set everything and then split the dies (e.g. V-Cache die performance,
    // frequency die balance_power while it only hosts IRQs and background work).
    t("cpu.governor_ccd0", "CPU", "Governor · CCD0",
      "Scaling governor for CCD0's CPUs only; overrides the global 'Scaling governor' row for just these cores. Typical split for X3D chips: set powersave here on whichever CCD you name in EPP, and use the global row for the rest. Leave both CCD rows unchecked to keep one governor for the whole chip - the common case; only check these for deliberately asymmetric behaviour.",
      Kind::Choice, Options::ListFile("scaling_available_governors"), Target::PerCcdPolicy("scaling_governor", 0)),
    t("cpu.governor_ccd1", "CPU", "Governor · CCD1",
      "Scaling governor for CCD1's CPUs only; overrides the global 'Scaling governor' row for just these cores. See the CCD0 row for the usual split.",
      Kind::Choice, Options::ListFile("scaling_available_governors"), Target::PerCcdPolicy("scaling_governor", 1)),
    t("cpu.epp_ccd0", "CPU", "EPP · CCD0",
      "EPP for CCD0's CPUs only; overrides the global EPP row for just these cores. Needs governor 'powersave' on CCD0 - 'performance' pins EPP and this override becomes moot. Concrete split for a V-Cache part: CCD0 = V-Cache -> performance here (the game runs there); CCD1 = frequency die -> balance_power on its own row, since it is mostly idle plus background/IRQ work during a game. That is exactly what the Gaming X3D and Competitive presets set (wq/irq affinity also point at CCD1).",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerCcdPolicy("energy_performance_preference", 0)),
    t("cpu.epp_ccd1", "CPU", "EPP · CCD1",
      "EPP for CCD1's CPUs only; overrides the global EPP row for just these cores. Needs governor 'powersave' on CCD1. See the CCD0 row for the usual split.",
      Kind::Choice, Options::ListFile("energy_performance_available_preferences"), Target::PerCcdPolicy("energy_performance_preference", 1)),
    t("cpu.boost_ccd0", "CPU", "Boost · CCD0",
      "Turbo for CCD0's CPUs only (needs kernel 6.11+ with per-policy boost; shows n/a otherwise, use the global Boost row instead). Use this to trade one die's headroom for the other's: turn boost off on the idle/background CCD so its heat and power budget go to the CCD doing the work.",
      Kind::Bool, NO, Target::PerCcdPolicy("boost", 0)),
    t("cpu.boost_ccd1", "CPU", "Boost · CCD1",
      "Turbo for CCD1's CPUs only (needs kernel 6.11+ with per-policy boost). See the CCD0 row for why you would split this.",
      Kind::Bool, NO, Target::PerCcdPolicy("boost", 1)),
    t("cpu.max_freq_ccd0", "CPU", "Max frequency · CCD0 (kHz)",
      "Hard frequency ceiling (kHz) for CCD0's CPUs, independent of boost/EPP. The kernel clamps whatever you enter to the hardware's real range, so an oversized value is harmless - it just becomes the hardware max. Two uses: (1) cap the non-gaming CCD low (e.g. 3500000 = 3.5 GHz) to keep it cool and quiet while it only handles background work; (2) cap a whole CCD during thermal testing instead of disabling boost outright, for a repeatable non-zero ceiling. Leave unchecked for normal gaming.",
      int(400_000, 7_000_000), NO, Target::PerCcdPolicy("scaling_max_freq", 0)),
    t("cpu.max_freq_ccd1", "CPU", "Max frequency · CCD1 (kHz)",
      "Hard frequency ceiling (kHz) for CCD1's CPUs. See the CCD0 row for the two common uses (capping the idle CCD, or repeatable thermal tests).",
      int(400_000, 7_000_000), NO, Target::PerCcdPolicy("scaling_max_freq", 1)),
    t("cpu.x3d_mode", "CPU", "3D V-Cache CCD preference",
      "amd_x3d_vcache driver hint (kernel 6.13+, X3D chips only) telling the scheduler which CCD to prefer for new threads. cache = V-Cache CCD first: right for almost every game, since large working sets (open-world titles, simulation-heavy games, emulators) benefit most from the extra L3. frequency = the higher-clocked CCD: better for single-threaded or clock-sensitive work (compiling, older/less cache-hungry engines, clock-bound benchmarks). Combine with the launch affinity in the Game launch tab to actually pin the game process, not just hint the scheduler. Written with a 3 s timeout: some BIOS/AGESA versions stall in the ACPI call; a timeout is reported as a failure for this row but does not block the rest of Apply.",
      Kind::Choice, Options::Fixed(&["frequency", "cache"]), Target::X3d),
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
    // ── Devices ───────────────────────────────────────────────────────────
    t("pci.aspm", "Devices", "PCIe ASPM policy",
      "PCIe Active State Power Management policy. 'performance' keeps every PCIe link at full power, no link-state transitions: removes the wake-up latency that shows as GPU or NVMe micro-jitter when a link drops to a power-saving state between traffic bursts - right for a plugged-in gaming session. 'powersave'/'powersupersave' let links drop to save power (better battery life, small idle power win) but on some hardware combinations actively cause dropouts on NVMe or Wi-Fi rather than just adding latency - if you see random Wi-Fi disconnects or NVMe timeouts, try 'performance' or 'default' here even outside gaming. 'default' defers to what the BIOS/ACPI tables request per device.",
      Kind::Choice, BR, Target::File("/sys/module/pcie_aspm/parameters/policy")),
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
    t("gpu.amdgpu_dpm", "Devices", "iGPU DPM level (amdgpu)",
      "Forces the integrated Radeon GPU's power state. 'low' pins the iGPU to its lowest performance level, freeing shared SoC power/thermal budget for the CPU cores - worth trying specifically when gaming on the discrete GPU, since the iGPU is doing nothing but display output/compositing anyway. 'auto' (default) lets the driver manage it dynamically. 'high' forces maximum iGPU performance, only useful running GPU work on the iGPU itself (rare on a laptop with a discrete GPU) - not something a gaming preset should set.",
      Kind::Choice, Options::Fixed(&["auto", "low", "high"]), Target::AmdgpuDpm),
    // ── Stability ─────────────────────────────────────────────────────────
    t("mce.check_interval", "Stability", "MCE poll interval (s)",
      "Polling interval (seconds) for correctable machine-check errors (early-warning signs of a marginal core/memory, short of a full crash). Stock is 300s. 10s (what the CO validation preset uses) catches a marginal Curve Optimizer offset within seconds of it starting to misbehave instead of up to 5 minutes later - pair with `dmesg -w` or rasdaemon open in a terminal while stress-testing a new offset. Set back to something relaxed (or 0 to stop polling) for normal use; frequent polling has a small but real overhead not worth paying permanently.",
      int(0, 3600), NO, Target::Mce),
    // ── Hot-plug (must stay last, see HOTPLUG_KEYS) ───────────────────────
    warn(t("cpu.smt", "CPU", "SMT",
      "Turns SMT (the second logical thread per physical core) on or off system-wide. Most games are unaffected or slightly faster with SMT on (more threads available); a minority of titles - especially ones sensitive to cache contention between sibling threads, or with poor thread-count scaling - show better 1% lows with it off, since every physical core is then dedicated to one thread with no sibling contention. This is genuinely game-specific: test SMT on vs off on the specific title if chasing 1% lows. Hot-plugs half the CPUs off/online, which is why this row is always applied last and restored first - every other per-CPU setting needs the CPU online first to accept the write.",
      Kind::Choice, Options::Fixed(&["on", "off"]), Target::File("/sys/devices/system/cpu/smt/control"))),
    warn(t("cpu.ccd_park", "CPU", "Park a CCD (offline)",
      "Takes an entire CCD fully offline (every CPU in it): no scheduling, no IRQs, no cross-CCD cache-coherency traffic can reach it at all. The most deterministic possible setup for an X3D chip - the game gets sole, uncontested use of one die's cache and cores with zero interference from the other die under any circumstance - at the obvious cost of losing that die's cores entirely until restored. The Competitive preset parks the frequency CCD as its most aggressive step; only reach for this if affinity plus workqueue/IRQ steering (which achieve most of the isolation benefit without losing any cores) is not enough for what you are chasing. cpu0's CCD can never be parked (the kernel needs cpu0 online), so on a 2-CCD chip you can only ever park 'the other one'.",
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
    policies().into_iter().filter(|p| {
        let cpus = read(&p.join("related_cpus")).map(|s| s.split_whitespace().filter_map(|c| c.parse().ok()).collect::<Vec<usize>>())
            .unwrap_or_default();
        !cpus.is_empty() && cpus.iter().all(|c| g.cpus.contains(c))
    }).collect()
}

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
        Target::PerCcdPolicy(f, ccd) => ccd_policies(ccd).into_iter().map(|p| p.join(f)).filter(|p| p.is_file()).collect(),
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
            // "custom" is what EPP reads back after a raw numeric write; it cannot be written as a string.
            same(p.and_then(|p| read(&p)).map(|s| s.split_whitespace().filter(|o| *o != "custom").map(str::to_owned).collect())
                .unwrap_or_default())
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
