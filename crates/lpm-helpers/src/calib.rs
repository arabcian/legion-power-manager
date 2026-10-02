//! Machine signature: measured effects of every CPU, scheduler and memory
//! knob on THIS machine, accumulated across calibration runs.
//!
//! lpm-calibrate tests one knob at a time, every candidate interleaved with
//! the knob's reference (its live = boot value), and stores per run
//!   effect = median(cand) / median(ref) - 1   (+ = better, per metric)
//! zeroed within 2x the run-to-run noise (MAD, >= 2 %), folded into four
//! objectives (latency, throughput, power, memory). Records accumulate in
//! /var/lib/legion-power-manager/signature.json, tied to a hardware
//! fingerprint; an estimate is the recency- and kernel-weighted mean of the
//! last records (half-life 90 days; other kernel versions count half) and
//! carries its evidence weight `n`, so autotune can trust it more the more
//! often it was seen. Numeric knobs get a dose-response curve (see `curve`).
//!
//! Since format 4 the signature also holds the *experiment log* of lpm-calibrate's
//! multi-knob designs (`rows`), which the joint model (model.rs) turns into
//! stand-alone effects, pair and triple interactions and posteriors; the
//! one-at-a-time records stay valid and are folded in as (down-weighted)
//! single-knob rows. Rows carry the benchmark version that measured them: rows of
//! an older bench set (other metrics per objective) count 60 %. The fitted prior
//! scales per phase and objective are kept (`hyp`), so autotune reuses them
//! instead of searching again.
//!
//! Three phases: IDLE (quiet machine), LOAD (ballast holding memory near a
//! safe headroom, churning allocations, half the CPUs busy) and IO (the storage
//! suite on a local disk, for the Storage rows and the dirty window). Each knob
//! belongs to the phases whose benchmarks can see it, and the I/O knobs only to IO:
//! storage metrics no longer dilute the CPU/memory objectives (nor their noise the
//! CPU/memory knobs), and the I/O objectives are weighed per goal by its storage
//! weight. Stability risk is never taken from a benchmark; a candidate that caused
//! an OOM kill is unsafe.

use crate::model::{self, Cfg, Factor, Fit, Hyper, Joint, Model, PhaseSet};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const FILE: &str = "/var/lib/legion-power-manager/signature.json";
/// Earlier single-run format (still read once, then superseded).
pub const OLD_FILE: &str = "/var/lib/legion-power-manager/calibration.json";
const KEEP: usize = 10;
const HALF_LIFE_DAYS: f64 = 90.0;

/// One benchmark metric and its direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Metric {
    WakeP99Us,      // timer wake-up overshoot p99 (latency, lower better)
    FaultP99Us,     // page-fault time p99 per 2 MiB of a fresh heap (latency, lower better)
    FsyncP99Ms,     // fsync() p99 while a writer streams (latency, lower better)
    RandReadP99Us,  // 4 KiB O_DIRECT random read p99 (latency, lower better)
    PingPongP99Us,  // thread-to-thread wake-up round trip p99 (latency, lower better)
    StallsPerSec,   // allocation stalls (direct reclaim) per second (latency, lower better)
    MemBwGbs,       // memcpy bandwidth (throughput, higher better)
    AllocMs,        // time to fault in a fresh heap (throughput, lower better)
    WriteMbs,       // buffered write incl. final fsync (throughput, higher better)
    ReadMbs,        // cold sequential read (throughput, higher better)
    ThpPct,         // share of the dense probe heap backed by huge pages (throughput, higher better)
    CpuSingle,      // single-thread integer work per second (throughput, higher better)
    CpuMulti,       // all-thread integer work per second (throughput, higher better)
    IdleW,          // idle power (power, lower better)
    PkgW,           // CPU package power during a loaded run (power, lower better)
    CpuEff,         // all-thread work per joule (power, higher better)
    ProbeRssMib,    // RSS of the disposable sparse-heap probe (footprint, lower better)
    TlbRandNs,      // dependent random loads over a plain (not madvised) heap: TLB reach of THP=always (throughput, lower better)
    TlbHugeNs,      // the same over a MADV_HUGEPAGE heap: what THP gives programs that opt in (throughput, lower better)
    ShmRandNs,      // the same over shared memory (memfd): THP shmem_enabled (throughput, lower better)
    FaultHugeP99Us, // fault time p99 per 2 MiB of a MADV_HUGEPAGE heap: defrag/compaction stalls (latency, lower better)
    FrameP99Us,     // 240 Hz frame loop: deadline -> fixed work done, tail (latency: wake-up + clock ramp, lower better)
    MixedReadP99Us, // 4 KiB direct random read tail while another writer streams and commits (latency, lower better)
    MmapFaultP99Us, // first-touch fault tail on a cold mmap of a file (latency, lower better)
    IopsK,          // 4 KiB direct random reads per second from several threads, thousands (throughput, higher better)
    IoCpuUs,        // busy CPU time (irq included) per random read (power, lower better)
    RaFootprintMib, // page cache brought in by sparse mmap touches: read-around waste (footprint, lower better)
    FrameMedUs,     // 240 Hz frame loop: typical deadline -> work done (latency: the clock a mostly idle core gets, lower better)
    GameFrameMs,    // game loop (60 % busy main thread + 3 job threads, 125 Hz): typical frame time (throughput = fps, lower better)
    GameTailMs,     // game loop: frame-time tail, the 1 % lows (latency, lower better)
    BusyFrameP99Us, // 240 Hz frame loop while every CPU is taken: preemption / wake-up placement tail (latency, lower better)
    JobsPerSec,     // short processes per second: exec, fresh-heap faults, 2 ms of work, exit (throughput, higher better)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Objective { Lat, Thr, Pwr, Mem }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase { Idle, Load, Io }

impl Phase {
    pub const ALL: [Phase; 3] = [Phase::Idle, Phase::Load, Phase::Io];
    pub fn idx(self) -> usize { match self { Phase::Idle => 0, Phase::Load => 1, Phase::Io => 2 } }
    pub fn from_idx(i: u64) -> Phase { match i { 0 => Phase::Idle, 2 => Phase::Io, _ => Phase::Load } }
    pub fn name(self) -> &'static str { match self { Phase::Idle => "idle", Phase::Load => "load", Phase::Io => "io" } }
}

/// Keys measured by the storage suite (IO phase only).
pub fn io_key(key: &str) -> bool { key == "vm.dirty" || key.starts_with("blk.") }

impl Metric {
    pub const ALL: [Metric; 32] = [Metric::WakeP99Us, Metric::FaultP99Us, Metric::FsyncP99Ms, Metric::RandReadP99Us,
        Metric::PingPongP99Us, Metric::StallsPerSec, Metric::MemBwGbs, Metric::AllocMs, Metric::WriteMbs, Metric::ReadMbs,
        Metric::ThpPct, Metric::CpuSingle, Metric::CpuMulti, Metric::IdleW, Metric::PkgW, Metric::CpuEff, Metric::ProbeRssMib,
        Metric::TlbRandNs, Metric::TlbHugeNs, Metric::ShmRandNs, Metric::FaultHugeP99Us, Metric::FrameP99Us, Metric::MixedReadP99Us,
        Metric::MmapFaultP99Us, Metric::IopsK, Metric::IoCpuUs, Metric::RaFootprintMib, Metric::FrameMedUs, Metric::GameFrameMs,
        Metric::GameTailMs, Metric::BusyFrameP99Us, Metric::JobsPerSec];
    pub fn objective(self) -> Objective {
        use Metric::*;
        match self {
            WakeP99Us | FaultP99Us | FsyncP99Ms | RandReadP99Us | PingPongP99Us | StallsPerSec | FaultHugeP99Us | FrameP99Us
            | MixedReadP99Us | MmapFaultP99Us | FrameMedUs | GameTailMs | BusyFrameP99Us => Objective::Lat,
            MemBwGbs | AllocMs | WriteMbs | ReadMbs | ThpPct | CpuSingle | CpuMulti | TlbRandNs | TlbHugeNs | ShmRandNs | IopsK
            | GameFrameMs | JobsPerSec => Objective::Thr,
            IdleW | PkgW | CpuEff | IoCpuUs => Objective::Pwr,
            ProbeRssMib | RaFootprintMib => Objective::Mem,
        }
    }
    /// Smallest relative change that counts as real in the one-at-a-time path. p99 tails
    /// jitter far more than means and counters.
    pub fn noise_floor(self) -> f64 {
        use Metric::*;
        match self { WakeP99Us | FaultP99Us | FsyncP99Ms | RandReadP99Us | PingPongP99Us | FaultHugeP99Us | FrameP99Us | MixedReadP99Us
                     | MmapFaultP99Us | GameTailMs | BusyFrameP99Us => 0.05, _ => 0.02 }
    }
    /// Offset that keeps log-ratios finite for counters that can be zero.
    pub fn eps(self) -> f64 { match self { Metric::StallsPerSec | Metric::ThpPct => 1.0, _ => 0.0 } }
    pub fn higher_better(self) -> bool {
        use Metric::*;
        matches!(self, MemBwGbs | WriteMbs | ReadMbs | ThpPct | CpuSingle | CpuMulti | CpuEff | IopsK | JobsPerSec)
    }
    pub fn name(self) -> &'static str {
        use Metric::*;
        match self {
            WakeP99Us => "wake p99", FaultP99Us => "fault p99", FsyncP99Ms => "fsync p99", RandReadP99Us => "rand read p99",
            PingPongP99Us => "ping-pong p99", StallsPerSec => "alloc stalls", MemBwGbs => "mem bandwidth", AllocMs => "heap fault-in",
            WriteMbs => "write", ReadMbs => "read", ThpPct => "THP coverage", CpuSingle => "1-thread work", CpuMulti => "all-thread work",
            IdleW => "idle power", PkgW => "package power", CpuEff => "work/joule", ProbeRssMib => "sparse RSS",
            TlbRandNs => "TLB random (plain)", TlbHugeNs => "TLB random (madvise)", ShmRandNs => "TLB random (shmem)",
            FaultHugeP99Us => "huge fault p99", FrameP99Us => "frame tail", MixedReadP99Us => "read tail under writes",
            MmapFaultP99Us => "mmap fault tail", IopsK => "random-read IOPS", IoCpuUs => "CPU per I/O", RaFootprintMib => "read-around cache",
            FrameMedUs => "frame typical", GameFrameMs => "game frame time", GameTailMs => "game frame tail",
            BusyFrameP99Us => "frame tail, CPUs taken", JobsPerSec => "short jobs",
        }
    }
}

/// Version of the benchmark set: rows measured by another set count less (their
/// objectives average other metrics). 2 = THP probes (TLB reach, madvised faults,
/// shmem) and a fragmented load phase. 3 = tails as expected shortfall (worst 1 %,
/// at least 5 samples), the frame-loop probe, metrics weighted by their measured
/// noise inside an objective, and the storage suite in its own IO phase (idle/load
/// objectives without I/O metrics). 4 = workloads the knobs act on: the game loop (busy,
/// unsaturated cores), the frame loop with every CPU taken (scheduler knobs), short jobs
/// (exec / fault / placement path), the frame loop's median; an objective is the gain summed
/// over its metrics (see `objective_gain`) instead of their mean. Idle/load rows of older
/// sets are not kept: they were blind to most CPU and scheduler knobs and would vote "no
/// effect" against the new measurements.
pub const BENCH_VERSION: u8 = 4;

/// Scale of an objective that averages `k` metrics. A knob moves the one or two metrics it
/// acts on and leaves the rest alone, so the plain mean shrank with every benchmark added: a
/// 10 % better frame time among eight latency metrics came out as 1.3 %, below any margin
/// autotune can use, and the more the suite measured the less every knob seemed to do. The
/// objective is therefore the gain summed over its metrics, counted as if two of them carried
/// it: mean x k/2 (never below the mean). One scale whatever else a session measured.
pub fn objective_gain(k: usize) -> f64 { (k as f64 / 2.0).max(1.0) }

/// Gains of the storage suite's objectives (4 latency, 3 throughput, 1 power, 1 footprint
/// metric): IO rows logged before version 4 hold plain means of the same metrics.
const IO_GAIN_V3: [f64; 4] = [2.0, 1.5, 1.0, 1.0];

/// Which benchmark groups a knob can influence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Benches { pub cpu_mem: bool, pub io: bool, pub idle: bool, pub cpu: bool }

/// Weight of the load phase when autotune blends both, per goal key.
pub fn load_share(goal: &str) -> f64 {
    match goal { "throughput" => 0.7, "gaming" => 0.6, "desktop" => 0.4, _ => 0.2 }
}

/// How a goal combines the phases: `load` = weight of the load phase against the idle one,
/// `io` = weight of the IO phase's objectives (the storage weight; the I/O knobs are only
/// measured there, so it scales their effects instead of blending).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blend { pub load: f64, pub io: f64 }

impl Blend {
    pub fn new(goal: &str, storage: f64) -> Blend { Blend { load: load_share(goal), io: storage } }
}

pub type Sample = BTreeMap<Metric, f64>;

pub fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() { return None; }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// Median absolute deviation relative to the median (noise level, 0..).
pub fn rel_mad(v: &[f64]) -> f64 {
    let mut a = v.to_vec();
    let Some(m) = median(&mut a) else { return 0.0 };
    if m == 0.0 { return 0.0; }
    let mut d: Vec<f64> = v.iter().map(|x| (x - m).abs()).collect();
    median(&mut d).unwrap_or(0.0) / m.abs()
}

/// Relative change per metric (+ = better), None where unmeasured,
/// 0 where the difference is within the reference's noise.
pub fn metric_effects(refs: &[Sample], cands: &[Sample]) -> BTreeMap<Metric, f64> {
    let mut out = BTreeMap::new();
    for m in Metric::ALL {
        let r: Vec<f64> = refs.iter().filter_map(|s| s.get(&m).copied()).collect();
        let c: Vec<f64> = cands.iter().filter_map(|s| s.get(&m).copied()).collect();
        let (Some(mr), Some(mc)) = (median(&mut r.clone()), median(&mut c.clone())) else { continue };
        if mr <= 0.0 || !mr.is_finite() || !mc.is_finite() { continue; }
        let raw = mc / mr - 1.0;
        let delta = if m.higher_better() { raw } else { -raw };
        let noise = (2.0 * rel_mad(&r).max(rel_mad(&c))).max(m.noise_floor());
        out.insert(m, if delta.abs() <= noise { 0.0 } else { delta.clamp(-1.0, 1.0) });
    }
    out
}

/// Metric effects folded into the four measurable objectives (mean per objective).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Measured { pub lat: Option<f64>, pub thr: Option<f64>, pub pwr: Option<f64>, pub mem: Option<f64>,
                      /// Evidence weight behind these numbers (recency- and kernel-weighted run count).
                      pub n: f64 }

pub fn fold(effects: &BTreeMap<Metric, f64>) -> Measured {
    let mean = |o: Objective| {
        let v: Vec<f64> = effects.iter().filter(|(m, _)| m.objective() == o).map(|(_, e)| *e).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64 * objective_gain(v.len()))
    };
    Measured { lat: mean(Objective::Lat), thr: mean(Objective::Thr), pwr: mean(Objective::Pwr), mem: mean(Objective::Mem), n: 1.0 }
}

impl Measured {
    fn from_json(v: &Value) -> Measured {
        Measured { lat: v["lat"].as_f64(), thr: v["thr"].as_f64(), pwr: v["pwr"].as_f64(), mem: v["mem"].as_f64(), n: v["n"].as_f64().unwrap_or(1.0) }
    }
}

// ── test plan ───────────────────────────────────────────────────────────────

/// Keys never tested, with the reason (kept in `--show`).
pub const EXCLUDED: &[(&str, &str)] = &[
    ("cpu.pstate_status", "driver switch"), ("cpu.intel_pstate_status", "driver switch"), ("cpu.smt", "takes CPUs offline"),
    ("cpu.ccd_park", "takes CPUs offline"), ("cpu.rapl_pl1", "firmware power limit"), ("cpu.rapl_pl2", "firmware power limit"),
    ("cpu.tcc_offset", "thermal limit"), ("cpu.uncore_max_khz", "Intel uncore limit"), ("cpu.uncore_min_khz", "Intel uncore limit"),
    ("cpu.governor_ccd0", "CCD role (structural)"), ("cpu.governor_ccd1", "CCD role (structural)"),
    ("cpu.epp_ccd0", "CCD role (structural)"), ("cpu.epp_ccd1", "CCD role (structural)"),
    ("cpu.boost_ccd0", "CCD role (structural)"), ("cpu.boost_ccd1", "CCD role (structural)"),
    ("cpu.epp_pcore", "core-type role (structural)"), ("cpu.epp_ecore", "core-type role (structural)"),
    ("cpu.max_freq_pcore", "core-type role (structural)"), ("cpu.max_freq_ecore", "core-type role (structural)"),
    ("cpu.x3d_mode", "V-Cache role (structural)"), ("cpu.min_freq", "floor, power only"), ("cpu.floor_freq", "floor, power only"),
    ("thp.enabled", "tested as the thp group"), ("thp.mthp_16k", "thp group"), ("thp.mthp_32k", "thp group"), ("thp.mthp_64k", "thp group"),
    ("thp.khugepaged_defrag", "khugepaged: minutes"), ("thp.khp_max_ptes_none", "khugepaged: minutes"),
    ("thp.khp_pages_to_scan", "khugepaged: minutes"), ("thp.khp_scan_sleep_ms", "khugepaged: minutes"),
    ("thp.khp_max_ptes_swap", "khugepaged: minutes"), ("thp.khp_alloc_sleep_ms", "khugepaged: minutes"),
    ("mm.lru_gen", "switching MGLRU off under load stalls"), ("mm.ksm_run", "no effect without mergeable memory"),
    ("vm.max_map_count", "a limit, not a cost"), ("vm.dirty_ratio", "tested as vm.dirty"), ("vm.dirty_background_ratio", "vm.dirty"),
    ("vm.dirty_bytes", "vm.dirty"), ("vm.dirty_background_bytes", "vm.dirty"),
    ("sched.ext", "needs a userspace scheduler"), ("kernel.watchdog", "never disabled"), ("kernel.sched_schedstats", "debug counters"),
    ("kernel.cfs_bandwidth_slice_us", "only with CPU quotas"), ("kernel.sched_util_clamp_min_rt_default", "RT tasks only"),
    ("wq.cpumask", "topology (structural)"), ("irq.affinity", "topology (structural)"),
    ("vm.dirty_writeback_centisecs", "periodic flush every 5-15 s: longer than a run, unmeasurable"),
    ("vm.dirty_expire_centisecs", "30-60 s data age: longer than a run, unmeasurable"),
];

/// Candidate values per key where the generic rule (Choice: its options;
/// Bool: flip; Int: live/2 and live*2) is wrong or unsafe. Values the kernel
/// or the safety audit rejects are dropped by lpm-calibrate when writing.
pub fn override_values(key: &str, live: &str, ram_kb: u64) -> Option<Vec<String>> {
    let v = |a: &[i64]| Some(a.iter().map(|x| x.to_string()).collect());
    let r: i64 = live.trim().parse().unwrap_or(0);
    match key {
        "vm.swappiness" => v(&[60, 100, 133, 150, 180]),
        "vm.page_cluster" => v(&[0, 1, 3]),
        "vm.vfs_cache_pressure" => v(&[50, 100, 200]),
        "vm.watermark_boost_factor" => v(&[0, 5000, 15000]),
        "vm.compaction_proactiveness" => v(&[0, 10, 20, 40]),
        "vm.watermark_scale_factor" => Some(vec!["@wsf".into()]),
        "vm.min_free_kbytes" => v(&[r / 2, r * 2].map(|x| x.clamp(16_384, (ram_kb / 100) as i64))),
        "mm.lru_gen_min_ttl" => v(&[0, 1000]),
        // Storage (IO phase). Doses stay inside autotune's 2x (+1 doubling with evidence) trust
        // region around the boot value: a level it could never pick is a wasted run.
        "blk.read_ahead_kb" if r > 0 => v(&[r / 4, r / 2, r * 2, r * 4].map(|x| x.clamp(4, 16_384))),
        // 0 switches throttling off (a mode, not a dose); the others bracket the target.
        "blk.wbt_lat_usec" => if r > 0 { v(&[0, r / 2, r * 2, r * 4].map(|x| x.min(1_000_000))) } else { v(&[1000, 2000, 5000]) },
        // The device's tag depth is the ceiling (writes above it fail): only shorter queues.
        "blk.nr_requests" if r > 0 => v(&[(r / 4).max(16), (r / 2).max(16)]),
        "blk.nomerges" => v(&[0, 1, 2]),
        "blk.rq_affinity" => v(&[0, 1, 2]),
        // deny/force are the kernel's emergency/testing switches, not settings.
        "thp.shmem_enabled" => Some(["always", "within_size", "advise", "never"].iter().filter(|x| **x != live).map(|x| x.to_string()).collect()),
        "vm.stat_interval" => v(&[1, 10]),
        "vm.page_lock_unfairness" => v(&[1, 5, 20]),
        "zswap.max_pool_percent" => v(&[10, 20, 30]),
        // 0/1/2 are the whole range: the generic halve/double rule would only offer 1.
        "kernel.sched_burst_inherit_type" => v(&[0, 1, 2]),
        "sched.migration_cost_ns" => v(&[r / 2, r * 2, 5_000_000]),
        "sched.nr_migrate" => v(&[8, 32, 128]),
        // up/tasks are test modes with wrong weight distribution, not candidates.
        "sched.cgroup_mode" => Some(["smp", "concur", "max"].iter().filter(|x| **x != live).map(|x| x.to_string()).collect()),
        "cpu.wake_latency_us" => v(&[0, 20, 200]),
        // Dose ladder for the CCD frequency caps: 100/90/80/70 % of the live cap (100 MHz steps).
        "cpu.max_freq_ccd0" | "cpu.max_freq_ccd1" if r > 0 => v(&[0.9, 0.8, 0.7].map(|f| ((r as f64 * f) / 100_000.0).round() as i64 * 100_000)),
        _ => None,
    }
}

/// Generic candidates for a key without an override.
pub fn generic_values(kind: &crate::tune::Kind, live: &str, options: &[String]) -> Vec<String> {
    match kind {
        crate::tune::Kind::Choice => options.iter().filter(|o| *o != live).take(4).cloned().collect(),
        crate::tune::Kind::Bool => vec![if live == "1" || live.eq_ignore_ascii_case("y") { "0".into() } else { "1".into() }],
        crate::tune::Kind::Int { min, max } => {
            let Ok(r) = live.trim().parse::<i64>() else { return Vec::new() };
            if r <= 0 { return Vec::new(); }
            [r / 2, r.saturating_mul(2)].iter().map(|x| (*x).clamp(*min, *max)).filter(|x| *x != r).map(|x| x.to_string()).collect()
        }
    }
}

/// Phases and benchmarks for a key, by what it can influence.
pub fn phases_for(key: &str, group: &str) -> Vec<(Phase, Benches)> {
    let b = |cpu_mem, io, idle, cpu| Benches { cpu_mem, io, idle, cpu };
    if io_key(key) { return vec![(Phase::Io, b(false, true, false, false))]; }
    match key {
        // THP modes change TLB reach (idle and loaded) and fault/compaction cost (loaded, fragmented).
        k if k == "thp" || k == "thp.shmem_enabled" || k.starts_with("thp.mthp_") =>
            vec![(Phase::Idle, b(true, false, false, false)), (Phase::Load, b(true, false, false, false))],
        "vm.stat_interval" => vec![(Phase::Idle, b(false, false, true, true))],
        _ => match group {
            "CPU" => vec![(Phase::Idle, b(false, false, true, true)), (Phase::Load, b(false, false, false, true))],
            "Scheduler" => vec![(Phase::Idle, b(false, false, false, true)), (Phase::Load, b(true, false, false, true))],
            "Memory" => vec![(Phase::Load, b(true, false, false, false))],
            "Storage" => vec![(Phase::Io, b(false, true, false, false))],
            _ => Vec::new(),
        },
    }
}

// ── signature store ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fingerprint { pub product: String, pub cpu: String, pub ram_mib: u64, pub bios: String }

impl Fingerprint {
    pub fn current() -> Fingerprint {
        let rd = |p: &str| std::fs::read_to_string(p).map(|s| s.trim().to_owned()).unwrap_or_default();
        let cpu = rd("/proc/cpuinfo").lines().find_map(|l| l.strip_prefix("model name")?.split_once(':').map(|(_, v)| v.trim().to_owned())).unwrap_or_default();
        let ram_mib = rd("/proc/meminfo").lines().find_map(|l| l.strip_prefix("MemTotal:")?.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).unwrap_or(0) / 1024;
        Fingerprint { product: format!("{} {}", rd("/sys/class/dmi/id/product_name"), rd("/sys/class/dmi/id/product_version")).trim().to_owned(),
                      cpu, ram_mib, bios: rd("/sys/class/dmi/id/bios_version") }
    }
    /// Same machine for the purpose of the signature (BIOS updates are kept, noted by records' dates).
    pub fn matches(&self, o: &Fingerprint) -> bool {
        self.product == o.product && self.cpu == o.cpu && (self.ram_mib as i64 - o.ram_mib as i64).abs() * 50 <= self.ram_mib.max(1) as i64
    }
    fn to_json(&self) -> Value { json!({"product": self.product, "cpu": self.cpu, "ram_mib": self.ram_mib, "bios": self.bios}) }
    fn from_json(v: &Value) -> Fingerprint {
        Fingerprint { product: v["product"].as_str().unwrap_or("").into(), cpu: v["cpu"].as_str().unwrap_or("").into(),
                      ram_mib: v["ram_mib"].as_u64().unwrap_or(0), bios: v["bios"].as_str().unwrap_or("").into() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rec { pub m: Measured, pub t: u64, pub kernel: String }

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Slot { pub idle: Vec<Rec>, pub load: Vec<Rec>, pub unsafe_: bool }

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group { pub reference: String, pub values: BTreeMap<String, Slot> }

/// One measured run of a multi-knob design: which knobs were changed (all others at
/// their reference), and the effect per objective (log-ratio to the session's
/// reference runs, + = better; NaN = not measured). `t` = when the run ended,
/// `w` = its weight (disturbed runs count less), `bv` = benchmark version.
#[derive(Clone, Debug, PartialEq)]
pub struct Row { pub phase: Phase, pub sess: u64, pub pos: f64, pub t: u64, pub kernel: String, pub cfg: Cfg, pub y: [f64; 4], pub w: f64, pub bv: u8 }

/// One phase's model: its space and rows, and one fit per objective (latency, throughput, power, memory).
pub struct PhaseFit { pub set: PhaseSet, pub fits: [Option<Arc<Fit>>; 4] }

/// Fits derived from the rows, per phase (built on first use, dropped when that phase's rows change).
#[derive(Default)]
pub struct ModelCache([RefCell<Option<Option<Arc<PhaseFit>>>>; 3]);
impl Clone for ModelCache { fn clone(&self) -> Self { ModelCache::default() } }
impl PartialEq for ModelCache { fn eq(&self, _: &Self) -> bool { true } }
impl std::fmt::Debug for ModelCache { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ModelCache") } }

const MAX_ROWS: usize = 1600;

fn pidx(phase: Phase) -> usize { phase.idx() }
fn hyp_key(phase: Phase, o: usize) -> String { format!("{}{o}", phase.name()) }

/// The machine signature.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Calibration {
    pub fingerprint: Fingerprint,
    pub keys: BTreeMap<String, Vec<Group>>,
    /// Experiment log of the multi-knob designs, the reference each knob had, and
    /// combinations that caused an OOM kill.
    pub rows: Vec<Row>,
    pub refs: BTreeMap<String, String>,
    pub unsafe_sets: Vec<Cfg>,
    /// Fitted prior scales per phase and objective ("idle0" .. "load3").
    pub hyp: BTreeMap<String, Vec<f64>>,
    /// Strategy of every design session per phase ("idle"/"load" -> "lean/base", "deep", ...),
    /// oldest first: progressive lean sessions pick what has not been done yet.
    pub strategies: BTreeMap<String, Vec<String>>,
    cache: ModelCache,
    pub on_battery: bool,
    /// The file held idle/load data of an older benchmark set, which was dropped on reading.
    pub migrated: bool,
    /// Current kernel (major.minor) and time used to weight records.
    pub kernel: String,
    pub now: u64,
}

fn neg(m: Measured) -> Measured {
    let n = |x: Option<f64>| x.map(|v| -v);
    Measured { lat: n(m.lat), thr: n(m.thr), pwr: n(m.pwr), mem: n(m.mem), n: m.n }
}

fn major_minor(k: &str) -> String { k.split(['.', '-']).take(2).collect::<Vec<_>>().join(".") }

/// Recency- and kernel-weighted mean of the last records.
pub fn aggregate(recs: &[Rec], now: u64, kernel: &str) -> Option<Measured> {
    let recs = &recs[recs.len().saturating_sub(KEEP)..];
    let mm = major_minor(kernel);
    let w: Vec<f64> = recs.iter().map(|r| {
        let age = now.saturating_sub(r.t) as f64 / 86_400.0;
        0.5f64.powf(age / HALF_LIFE_DAYS) * if kernel.is_empty() || major_minor(&r.kernel) == mm { 1.0 } else { 0.5 }
    }).collect();
    let field = |f: &dyn Fn(&Measured) -> Option<f64>| {
        let (mut s, mut ws) = (0.0, 0.0);
        for (r, wt) in recs.iter().zip(&w) { if let Some(x) = f(&r.m) { s += x * wt; ws += wt; } }
        (ws > 0.0).then(|| s / ws)
    };
    let n: f64 = w.iter().sum();
    (n > 0.0).then(|| Measured { lat: field(&|m| m.lat), thr: field(&|m| m.thr), pwr: field(&|m| m.pwr), mem: field(&|m| m.mem), n })
}

impl Calibration {
    fn group(&self, key: &str, reference: &str) -> Option<&Group> {
        let gs = self.keys.get(key)?;
        gs.iter().find(|g| g.reference == reference)
            .or_else(|| gs.iter().max_by_key(|g| g.values.values().map(|s| s.idle.len() + s.load.len()).sum::<usize>()))
    }
    fn agg(&self, s: &Slot, phase: Phase) -> Option<Measured> {
        match phase { Phase::Idle => aggregate(&s.idle, self.now, &self.kernel), Phase::Load => aggregate(&s.load, self.now, &self.kernel), Phase::Io => None }
    }
    /// Effects of `value` relative to `reference` in one phase (re-based through the stored reference).
    pub fn get_phase(&self, key: &str, reference: &str, value: &str, phase: Phase) -> Option<Measured> {
        if value == reference { return None; }
        if let Some(m) = self.model_effect(key, reference, value, phase) { return Some(m); }
        let g = self.group(key, reference)?;
        let at = |v: &str| g.values.get(v).and_then(|s| self.agg(s, phase));
        if g.reference == reference { return at(value); }
        let b = at(reference)?;
        if value == g.reference { return Some(neg(b)); }
        let a = at(value)?;
        let d = |x: Option<f64>, y: Option<f64>| Some(x? - y?);
        Some(Measured { lat: d(a.lat, b.lat), thr: d(a.thr, b.thr), pwr: d(a.pwr, b.pwr), mem: d(a.mem, b.mem), n: a.n.min(b.n) })
    }
    /// The phases blended for a goal: idle and load by `b.load` (a phase without data leaves
    /// the other in full); a knob of the IO phase is measured there only and its effects are
    /// scaled by the storage weight `b.io`, as the joint model weighs that phase's objectives.
    pub fn get(&self, key: &str, reference: &str, value: &str, b: Blend) -> Option<Measured> {
        if io_key(key) {
            if let Some(m) = self.get_phase(key, reference, value, Phase::Io) {
                let sc = |x: Option<f64>| x.map(|v| v * b.io);
                return Some(Measured { lat: sc(m.lat), thr: sc(m.thr), pwr: sc(m.pwr), mem: sc(m.mem), n: m.n });
            }
        }
        let load = b.load;
        let (i, l) = (self.get_phase(key, reference, value, Phase::Idle), self.get_phase(key, reference, value, Phase::Load));
        let mix = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(x), Some(y)) => Some((1.0 - load) * x + load * y),
            (x, None) => x,
            (None, y) => y,
        };
        match (i, l) {
            (None, None) => None,
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (Some(a), Some(b)) => Some(Measured { lat: mix(a.lat, b.lat), thr: mix(a.thr, b.thr), pwr: mix(a.pwr, b.pwr),
                                                  mem: mix(a.mem, b.mem), n: a.n.min(b.n) }),
        }
    }
    pub fn new(fingerprint: Fingerprint) -> Calibration { Calibration { fingerprint, ..Default::default() } }
    // ── experiment log and joint model ──
    fn ref_of(&self, key: &str) -> Option<String> {
        self.refs.get(key).cloned().or_else(|| self.group(key, "").map(|g| g.reference.clone()))
    }
    /// Rows of one phase as model input: weights carry recency and kernel age; the one-at-a-time
    /// records join as single-knob rows worth half their evidence.
    /// The I/O knobs belong to the IO phase alone: idle/load rows of an older benchmark set that
    /// changed one are left out (their objectives mixed storage metrics in), so the two
    /// models never share a knob with the IO model.
    fn pre_rows(&self, phase: Phase) -> Vec<model::Row> {
        let mm = major_minor(&self.kernel);
        let foreign = |k: &str| (phase == Phase::Io) != io_key(k);
        let mut out: Vec<model::Row> = self.rows.iter().filter(|r| r.phase == phase && !r.cfg.iter().any(|(k, _)| foreign(k))).map(|r| {
            let age = self.now.saturating_sub(r.t) as f64 / 86_400.0;
            let k = if self.kernel.is_empty() || major_minor(&r.kernel) == mm { 1.0 } else { 0.5 };
            let b = if r.bv == BENCH_VERSION { 1.0 } else { 0.6 };
            model::Row { cfg: r.cfg.clone(), y: r.y, w: r.w * 0.5f64.powf(age / HALF_LIFE_DAYS) * k * b, sess: r.sess, pos: r.pos, t: r.t as f64 }
        }).collect();
        for (key, gs) in &self.keys {
            if phase == Phase::Io || io_key(key) { continue; }
            let Some(reference) = self.ref_of(key) else { continue };
            let Some(g) = gs.iter().find(|g| g.reference == reference) else { continue };
            for (value, slot) in &g.values {
                if slot.unsafe_ { continue; }
                let Some(m) = self.agg(slot, phase) else { continue };
                let nan = |x: Option<f64>| x.unwrap_or(f64::NAN);
                out.push(model::Row { cfg: vec![(key.clone(), value.clone())], y: [nan(m.lat), nan(m.thr), nan(m.pwr), nan(m.mem)],
                                      w: (m.n * 0.5).clamp(0.05, 3.0), sess: 0, pos: 0.5, t: 0.0 });
            }
        }
        out
    }
    fn build_set(&self, phase: Phase) -> Option<PhaseSet> {
        let rows = self.pre_rows(phase);
        let mut vals: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for r in &rows { for (k, v) in &r.cfg { vals.entry(k.clone()).or_default().insert(v.clone()); } }
        let factors: Vec<Factor> = vals.into_iter().filter_map(|(key, vs)| {
            let reference = self.ref_of(&key)?;
            let values: Vec<String> = vs.into_iter().filter(|v| *v != reference).collect();
            (!values.is_empty()).then_some(Factor { key, reference, values })
        }).collect();
        let keep: BTreeSet<&str> = factors.iter().map(|f| f.key.as_str()).collect();
        let rows: Vec<model::Row> = rows.into_iter().filter(|r| r.cfg.iter().all(|(k, _)| keep.contains(k.as_str()))).collect();
        PhaseSet::build(factors, rows)
    }
    /// Kept prior scales of one phase/objective.
    pub fn hyper(&self, phase: Phase, o: usize) -> Option<Hyper> { self.hyp.get(&hyp_key(phase, o)).and_then(|v| Hyper::from_slice(v)) }
    /// `passes`: None = kept hyperparameters as they are (a search only where none are kept).
    fn build_phase(&self, phase: Phase, passes: Option<usize>) -> Option<PhaseFit> {
        let set = self.build_set(phase)?;
        let fits = std::array::from_fn(|o| {
            let init = self.hyper(phase, o);
            set.fit_obj(o, init, passes.unwrap_or(if init.is_some() { 0 } else { 1 }))
        });
        Some(PhaseFit { set, fits })
    }
    /// The phase's model (cached until its rows change).
    pub fn phase_fit(&self, phase: Phase) -> Option<Arc<PhaseFit>> {
        let cell = &self.cache.0[pidx(phase)];
        if let Some(v) = cell.borrow().as_ref() { return v.clone(); }
        let v = self.build_phase(phase, None).map(Arc::new);
        *cell.borrow_mut() = Some(v.clone());
        v
    }
    /// Refits one phase with a hyperparameter search warm-started from the kept values, and keeps the result.
    pub fn retune(&mut self, phase: Phase, passes: usize) -> Option<Arc<PhaseFit>> {
        let v = self.build_phase(phase, Some(passes)).map(Arc::new);
        if let Some(pf) = &v {
            for (o, f) in pf.fits.iter().enumerate() { if let Some(f) = f { self.hyp.insert(hyp_key(phase, o), f.hyper.to_vec()); } }
        }
        *self.cache.0[pidx(phase)].borrow_mut() = Some(v.clone());
        v
    }
    /// Stand-alone effect of one value from the joint model (posterior mean per objective;
    /// `n` = how much the data narrowed the prior: prior variance / posterior variance - 1).
    fn model_effect(&self, key: &str, reference: &str, value: &str, phase: Phase) -> Option<Measured> {
        if self.rows.is_empty() || self.refs.get(key).map(String::as_str) != Some(reference) { return None; }
        let pf = self.phase_fit(phase)?;
        let f = pf.set.space.func(&[(key.to_owned(), value.to_owned())], true)?;
        let (mut m, mut n, mut any) = (Measured::default(), f64::MAX, false);
        for (o, fit) in pf.fits.iter().enumerate() {
            let Some(fit) = fit else { continue };
            let (mu, var, prior) = (fit.mean(&f), fit.var(&f), fit.prior(&f, &f));
            n = n.min((prior / var.max(1e-12) - 1.0).clamp(0.0, 30.0));
            match o { 0 => m.lat = Some(mu), 1 => m.thr = Some(mu), 2 => m.pwr = Some(mu), _ => m.mem = Some(mu) }
            any = true;
        }
        any.then(|| { m.n = n; m })
    }
    /// Utility model of one phase for objective weights (latency, throughput, power, footprint).
    pub fn phase_model(&self, phase: Phase, wts: [f64; 4]) -> Option<Model> {
        let pf = self.phase_fit(phase)?;
        Model::new(pf.set.space.clone(), pf.fits.clone(), wts)
    }
    /// Joint utility model for objective weights and a goal's phase blend.
    pub fn joint(&self, wts: [f64; 4], b: Blend) -> Option<Joint> {
        if self.rows.is_empty() { return None; }
        let (idle, load) = (self.phase_model(Phase::Idle, wts), self.phase_model(Phase::Load, wts));
        let io = if b.io > 0.0 { self.phase_model(Phase::Io, wts.map(|w| w * b.io)) } else { None };
        (idle.is_some() || load.is_some() || io.is_some()).then_some(Joint { idle, load, io, share: b.load })
    }
    /// Phase sets (for reports and the calibration loop).
    pub fn phase_set(&self, phase: Phase) -> Option<PhaseSet> { self.phase_fit(phase).map(|p| p.set.clone()) }
    /// Every knob the signature knows (one-at-a-time records or experiment log).
    pub fn key_names(&self) -> Vec<String> {
        let mut k: BTreeSet<String> = self.keys.keys().cloned().collect();
        for r in &self.rows { for (key, _) in &r.cfg { k.insert(key.clone()); } }
        k.into_iter().collect()
    }
    pub fn has_key(&self, key: &str) -> bool { self.keys.contains_key(key) || self.rows.iter().any(|r| r.cfg.iter().any(|(k, _)| k == key)) }
    pub fn invalidate(&mut self) { for c in &self.cache.0 { *c.borrow_mut() = None; } }
    /// Replaces the rows of one session/phase (a design in progress is re-derived after every batch).
    pub fn put_session(&mut self, phase: Phase, sess: u64, rows: Vec<Row>) {
        self.rows.retain(|r| !(r.phase == phase && r.sess == sess));
        self.rows.extend(rows);
        if self.rows.len() > MAX_ROWS {
            let drop = self.rows.len() - MAX_ROWS;
            self.rows.sort_by_key(|r| r.t);
            self.rows.drain(..drop);
            self.invalidate();
        } else {
            *self.cache.0[pidx(phase)].borrow_mut() = None;
        }
    }
    /// A knob's reference changed: rows that changed it were measured against another baseline.
    pub fn forget_key(&mut self, key: &str) { self.rows.retain(|r| !r.cfg.iter().any(|(k, _)| k == key)); self.invalidate(); }
    pub fn unsafe_cfg(&self, cfg: &[(String, String)]) -> bool { self.unsafe_sets.iter().any(|u| u.iter().all(|kv| cfg.contains(kv))) }
    pub fn add_unsafe_set(&mut self, mut set: Cfg) {
        set.sort();
        if self.unsafe_sets.iter().any(|u| u.iter().all(|kv| set.contains(kv))) { return; }
        self.unsafe_sets.retain(|u| !set.iter().all(|kv| u.contains(kv)));
        self.unsafe_sets.push(set);
        if self.unsafe_sets.len() > 50 { self.unsafe_sets.remove(0); }
    }
    pub fn is_unsafe(&self, key: &str, value: &str) -> bool {
        self.keys.get(key).map_or(false, |gs| gs.iter().any(|g| g.values.get(value).map_or(false, |s| s.unsafe_)))
    }
    /// Every value measured for `key` (any phase), with the group's reference.
    pub fn values(&self, key: &str, reference: &str) -> Option<(String, Vec<String>)> {
        let mut vals: BTreeSet<String> = BTreeSet::new();
        let mut stored = None;
        if let Some(g) = self.group(key, reference) { stored = Some(g.reference.clone()); vals.extend(g.values.keys().cloned()); }
        for r in &self.rows { for (k, v) in &r.cfg { if k == key { vals.insert(v.clone()); } } }
        let stored = stored.or_else(|| self.refs.get(key).cloned())?;
        if let Some(r) = self.refs.get(key) { vals.remove(r); }
        Some((stored, vals.into_iter().collect()))
    }
    /// Adds one run's result.
    pub fn add(&mut self, key: &str, reference: &str, value: &str, phase: Phase, m: Option<Measured>, unsafe_: bool, t: u64, kernel: &str) {
        let gs = self.keys.entry(key.to_owned()).or_default();
        let g = match gs.iter().position(|g| g.reference == reference) {
            Some(i) => &mut gs[i],
            None => { gs.push(Group { reference: reference.to_owned(), values: BTreeMap::new() }); gs.last_mut().unwrap() }
        };
        let s = g.values.entry(value.to_owned()).or_default();
        if unsafe_ { s.unsafe_ = true; }
        if let (Some(m), true) = (m, phase != Phase::Io) {
            let v = if phase == Phase::Idle { &mut s.idle } else { &mut s.load };
            v.push(Rec { m, t, kernel: kernel.to_owned() });
            let drop = v.len().saturating_sub(KEEP);
            v.drain(..drop);
        }
    }
    /// Records for `key` in `phase` (all values): drives "least covered first".
    pub fn coverage(&self, key: &str, phase: Phase) -> usize {
        self.rows.iter().filter(|r| r.phase == phase && r.cfg.iter().any(|(k, _)| k == key)).count() +
        self.keys.get(key).map_or(0, |gs| gs.iter().flat_map(|g| g.values.values())
            .map(|s| match phase { Phase::Idle => s.idle.len(), Phase::Load => s.load.len(), Phase::Io => 0 }).sum())
    }

    pub fn to_json(&self) -> Value {
        let rec = |r: &Rec| json!({"lat": r.m.lat, "thr": r.m.thr, "pwr": r.m.pwr, "mem": r.m.mem, "t": r.t, "k": r.kernel});
        let keys: Map<String, Value> = self.keys.iter().map(|(k, gs)| (k.clone(), Value::Array(gs.iter().map(|g| {
            let v: Map<String, Value> = g.values.iter().map(|(x, s)| (x.clone(), json!({
                "idle": s.idle.iter().map(rec).collect::<Vec<_>>(), "load": s.load.iter().map(rec).collect::<Vec<_>>(), "unsafe": s.unsafe_}))).collect();
            json!({"reference": g.reference, "values": v})
        }).collect()))).collect();
        let num = |x: f64| if x.is_finite() { json!(x) } else { Value::Null };
        let rows: Vec<Value> = self.rows.iter().map(|r| json!({
            "ph": r.phase.idx(), "s": r.sess, "p": r.pos, "t": r.t, "k": r.kernel, "w": r.w, "bv": r.bv,
            "c": r.cfg.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(), "y": r.y.iter().map(|v| num(*v)).collect::<Vec<_>>()})).collect();
        json!({"version": 6, "fingerprint": self.fingerprint.to_json(), "on_battery": self.on_battery, "keys": keys, "rows": rows,
               "refs": self.refs, "hyp": self.hyp, "strategies": self.strategies, "unsafe_sets": self.unsafe_sets.iter().map(|u| u.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()).collect::<Vec<_>>()})
    }
    pub fn from_json(v: &Value) -> Calibration {
        let mut c = Calibration { fingerprint: Fingerprint::from_json(&v["fingerprint"]), on_battery: v["on_battery"].as_bool().unwrap_or(false), ..Default::default() };
        let version = v["version"].as_u64().unwrap_or(1);
        let rec = |x: &Value| Rec { m: Measured::from_json(x), t: x["t"].as_u64().unwrap_or(0), kernel: x["k"].as_str().unwrap_or("").into() };
        for (k, x) in v["keys"].as_object().into_iter().flatten() {
            let groups: Vec<&Value> = if version >= 3 { x.as_array().map(|a| a.iter().collect()).unwrap_or_default() } else { vec![x] };
            for g in groups {
                let Some(r) = g["reference"].as_str() else { continue };
                let mut grp = Group { reference: r.into(), values: BTreeMap::new() };
                for (val, e) in g["values"].as_object().into_iter().flatten() {
                    let slot = match version {
                        1 => Slot { idle: vec![Rec { m: Measured::from_json(e), t: 0, kernel: String::new() }], ..Default::default() },
                        2 => Slot { idle: e.get("idle").filter(|x| x.is_object()).map(|x| vec![rec(x)]).unwrap_or_default(),
                                    load: e.get("load").filter(|x| x.is_object()).map(|x| vec![rec(x)]).unwrap_or_default(),
                                    unsafe_: e["unsafe"].as_bool().unwrap_or(false) },
                        _ => Slot { idle: e["idle"].as_array().into_iter().flatten().map(rec).collect(),
                                    load: e["load"].as_array().into_iter().flatten().map(rec).collect(),
                                    unsafe_: e["unsafe"].as_bool().unwrap_or(false) },
                    };
                    grp.values.insert(val.clone(), slot);
                }
                c.keys.entry(k.clone()).or_default().push(grp);
            }
        }
        let pair = |x: &Value| Some((x[0].as_str()?.to_owned(), x[1].as_str()?.to_owned()));
        for r in v["rows"].as_array().into_iter().flatten() {
            let mut y = [f64::NAN; 4];
            for (i, e) in r["y"].as_array().into_iter().flatten().take(4).enumerate() { y[i] = e.as_f64().unwrap_or(f64::NAN); }
            c.rows.push(Row { phase: Phase::from_idx(r["ph"].as_u64().unwrap_or(1)), sess: r["s"].as_u64().unwrap_or(0),
                              pos: r["p"].as_f64().unwrap_or(0.5), t: r["t"].as_u64().unwrap_or(0), kernel: r["k"].as_str().unwrap_or("").into(),
                              cfg: r["c"].as_array().into_iter().flatten().filter_map(pair).collect(), y, w: r["w"].as_f64().unwrap_or(1.0),
                              bv: r["bv"].as_u64().unwrap_or(1) as u8 });
        }
        // Format 4 logged vm.dirty design rows against the live limits while labelling them window "1".
        if version < 5 { c.rows.retain(|r| !r.cfg.iter().any(|(k, _)| k == "vm.dirty")); }
        // Format 6 = benchmark set 4. The storage suite is the same, so its rows are kept and
        // brought to the new objective scale; idle/load rows and one-at-a-time records of the
        // older sets go (what was found unsafe stays unsafe), with their fitted prior scales
        // and the progress of the lean sessions - those phases start over.
        let old = version < 6;
        if old {
            c.migrated = c.rows.iter().any(|r| r.phase != Phase::Io)
                || c.keys.values().flatten().flat_map(|g| g.values.values()).any(|s| !s.idle.is_empty() || !s.load.is_empty());
            c.rows.retain(|r| r.phase == Phase::Io);
            for r in c.rows.iter_mut().filter(|r| r.bv < 4) {
                for (y, g) in r.y.iter_mut().zip(IO_GAIN_V3) { *y *= g; }
                r.bv = BENCH_VERSION;
            }
            for gs in c.keys.values_mut() {
                for g in gs.iter_mut() { g.values.retain(|_, s| { s.idle.clear(); s.load.clear(); s.unsafe_ }); }
                gs.retain(|g| !g.values.is_empty());
            }
            c.keys.retain(|_, gs| !gs.is_empty());
        }
        for (k, x) in v["refs"].as_object().into_iter().flatten() { if let Some(x) = x.as_str() { c.refs.insert(k.clone(), x.to_owned()); } }
        for u in v["unsafe_sets"].as_array().into_iter().flatten() { c.unsafe_sets.push(u.as_array().into_iter().flatten().filter_map(pair).collect()); }
        for (k, x) in v["hyp"].as_object().into_iter().flatten().filter(|_| !old) {
            let h: Vec<f64> = x.as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
            if Hyper::from_slice(&h).is_some() { c.hyp.insert(k.clone(), h); }
        }
        for (k, x) in v["strategies"].as_object().into_iter().flatten().filter(|(k, _)| !old || k.as_str() == "io") {
            c.strategies.insert(k.clone(), x.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_owned)).collect());
        }
        c
    }
    /// Notes a finished design session's strategy for a phase (the last 64 are kept).
    pub fn note_strategy(&mut self, phase: Phase, name: &str) {
        let v = self.strategies.entry(phase.name().into()).or_default();
        v.push(name.to_owned());
        if v.len() > 64 { v.remove(0); }
    }
    pub fn strategies_of(&self, phase: Phase) -> &[String] {
        self.strategies.get(phase.name()).map_or(&[], |v| v.as_slice())
    }
    /// The signature of this machine (older single-run results are migrated;
    /// a signature of other hardware is ignored).
    pub fn load() -> Option<Calibration> {
        let text = crate::read_root_file(FILE, 4 << 20).or_else(|| crate::read_root_file(OLD_FILE, 512 * 1024))?;
        let mut c = Calibration::from_json(&serde_json::from_str(&text).ok()?);
        let fp = Fingerprint::current();
        if !c.fingerprint.product.is_empty() && !c.fingerprint.matches(&fp) { return None; }
        c.fingerprint = fp;
        c.kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_owned();
        c.now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        Some(c)
    }
}

/// Dose-response curve of a numeric knob: measured points (value, effects)
/// in log2(value + 1) space, linearly interpolated between neighbours.
/// Returns interpolated candidates at the geometric midpoints between
/// measured values; their evidence weight is half the weaker neighbour's.
pub fn curve(points: &[(i64, Measured)]) -> Vec<(i64, Measured)> {
    // 0 switches a feature off: no dose lies between it and the next level.
    let mut p: Vec<&(i64, Measured)> = points.iter().filter(|(v, _)| *v >= 0).collect();
    p.sort_by_key(|(v, _)| *v);
    let mut out = Vec::new();
    for w in p.windows(2) {
        let ((a, ma), (b, mb)) = (w[0], w[1]);
        if *a <= 0 || b - a < 2 { continue; }
        let mid = (((*a as f64 + 1.0) * (*b as f64 + 1.0)).sqrt() - 1.0).round() as i64;
        if mid <= *a || mid >= *b { continue; }
        let t = ((mid as f64 + 1.0).log2() - (*a as f64 + 1.0).log2()) / ((*b as f64 + 1.0).log2() - (*a as f64 + 1.0).log2());
        let lerp = |x: Option<f64>, y: Option<f64>| Some(x? + (y? - x?) * t);
        out.push((mid, Measured { lat: lerp(ma.lat, mb.lat), thr: lerp(ma.thr, mb.thr), pwr: lerp(ma.pwr, mb.pwr),
                                  mem: lerp(ma.mem, mb.mem), n: ma.n.min(mb.n) * 0.5 }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(pairs: &[(Metric, f64)]) -> Sample { pairs.iter().copied().collect() }
    fn m(lat: f64, thr: f64) -> Measured { Measured { lat: Some(lat), thr: Some(thr), pwr: None, mem: None, n: 1.0 } }

    #[test]
    fn effects_sign_noise_and_fold() {
        let refs = vec![s(&[(Metric::MemBwGbs, 50.0), (Metric::WakeP99Us, 100.0)]), s(&[(Metric::MemBwGbs, 51.0), (Metric::WakeP99Us, 102.0)]),
                        s(&[(Metric::MemBwGbs, 49.0), (Metric::WakeP99Us, 98.0)])];
        let cands = vec![s(&[(Metric::MemBwGbs, 60.0), (Metric::WakeP99Us, 101.0)]), s(&[(Metric::MemBwGbs, 61.0), (Metric::WakeP99Us, 99.0)]),
                         s(&[(Metric::MemBwGbs, 59.0), (Metric::WakeP99Us, 100.0)])];
        let e = metric_effects(&refs, &cands);
        assert!((e[&Metric::MemBwGbs] - 0.2).abs() < 1e-9);
        assert_eq!(e[&Metric::WakeP99Us], 0.0);
        let f = fold(&e);
        assert!((f.thr.unwrap() - 0.2).abs() < 1e-9 && f.lat == Some(0.0) && f.pwr.is_none());
        let e2 = metric_effects(&[s(&[(Metric::CpuEff, 10.0)])], &[s(&[(Metric::CpuEff, 12.0)])]);
        assert!((e2[&Metric::CpuEff] - 0.2).abs() < 1e-9, "work per joule: higher is better");
    }

    #[test]
    fn signature_accumulates_weights_and_migrates() {
        let now = 400 * 86_400;
        let mut c = Calibration { kernel: "7.2.8".into(), now, ..Default::default() };
        c.add("k", "128", "1024", Phase::Idle, Some(m(0.0, 0.10)), false, now, "7.2.8");
        c.add("k", "128", "1024", Phase::Idle, Some(m(0.0, 0.30)), false, now - 90 * 86_400, "7.2.1");
        c.add("k", "128", "1024", Phase::Load, Some(m(-0.2, 0.0)), false, now, "7.2.8");
        c.add("k", "128", "256", Phase::Idle, Some(m(0.0, 0.04)), false, now, "7.2.8");
        c.add("k", "128", "4096", Phase::Load, None, true, now, "7.2.8");
        // Newest weighs 1, the 90-day-old one 0.5 -> (0.10 + 0.15) / 1.5.
        let i = c.get_phase("k", "128", "1024", Phase::Idle).unwrap();
        assert!((i.thr.unwrap() - 0.25 / 1.5).abs() < 1e-9 && (i.n - 1.5).abs() < 1e-9);
        // Other kernel counts half.
        let mut c2 = c.clone();
        c2.kernel = "7.3.0".into();
        assert!((c2.get_phase("k", "128", "256", Phase::Idle).unwrap().n - 0.5).abs() < 1e-9);
        // Blend + rebase + unsafe + roundtrip.
        let b = c.get("k", "128", "1024", Blend { load: 0.6, io: 1.0 }).unwrap();
        assert!((b.lat.unwrap() - 0.6 * -0.2).abs() < 1e-9);
        assert!((c.get_phase("k", "256", "1024", Phase::Idle).unwrap().thr.unwrap() - (0.25 / 1.5 - 0.04)).abs() < 1e-9);
        assert!(c.is_unsafe("k", "4096"));
        let mut back = Calibration::from_json(&c.to_json());
        back.kernel = c.kernel.clone(); back.now = now;
        assert_eq!(back.keys, c.keys);
        assert_eq!(c.coverage("k", Phase::Idle), 3);
        // Records beyond KEEP drop the oldest.
        for _ in 0..15 { c.add("k", "128", "256", Phase::Idle, Some(m(0.0, 0.0)), false, now, "7.2.8"); }
        assert_eq!(c.keys["k"][0].values["256"].idle.len(), KEEP);
        // Files of an older benchmark set load, but their idle/load records are not kept (the
        // old benchmarks were blind to most CPU and scheduler knobs); unsafe values stay unsafe.
        let v2 = json!({"version": 2, "keys": {"k": {"reference": "a", "values": {"b": {"idle": {"lat": 0.1}, "load": null, "unsafe": false},
                                                                                     "c": {"idle": null, "load": null, "unsafe": true}}}}});
        let c3 = Calibration::from_json(&v2);
        assert!(c3.migrated && c3.get_phase("k", "a", "b", Phase::Idle).is_none() && c3.is_unsafe("k", "c") && !c3.is_unsafe("k", "b"));
        // Version 5: idle/load rows go, IO rows are brought to the summed objective scale; the next write is format 6.
        let row = |ph: u64| json!({"ph": ph, "s": 1, "p": 0.5, "t": 1, "k": "7.2", "w": 1.0, "bv": 3, "c": [["blk.scheduler", "none"]], "y": [0.1, 0.1, 0.1, null]});
        let v5 = json!({"version": 5, "rows": [row(0), row(1), row(2)], "hyp": {"io0": [0.03, 0.03, 0.012, 0.004, 0.004, 0.01]},
                        "strategies": {"idle": ["lean/base"], "io": ["lean/base"]}});
        let c5 = Calibration::from_json(&v5);
        assert_eq!(c5.rows.len(), 1);
        assert!(c5.migrated && c5.rows[0].phase == Phase::Io && c5.rows[0].bv == BENCH_VERSION && c5.hyp.is_empty());
        assert!((c5.rows[0].y[0] - 0.2).abs() < 1e-12 && (c5.rows[0].y[1] - 0.15).abs() < 1e-12 && (c5.rows[0].y[2] - 0.1).abs() < 1e-12);
        assert!(c5.strategies_of(Phase::Idle).is_empty() && c5.strategies_of(Phase::Io).len() == 1);
        let again = Calibration::from_json(&c5.to_json());
        assert!(!again.migrated && again.rows.len() == 1 && again.rows[0].y[0] == c5.rows[0].y[0]);
        assert_eq!((objective_gain(1), objective_gain(2), objective_gain(8)), (1.0, 1.0, 4.0));
    }

    #[test]
    fn curve_and_plan_values() {
        let pts = vec![(10, m(0.0, 0.0)), (40, m(0.2, 0.0)), (160, m(0.1, 0.0))];
        let c = curve(&pts);
        assert_eq!(c.len(), 2);
        let (mid, e) = c[0];
        assert!(mid > 10 && mid < 40 && e.lat.unwrap() > 0.0 && e.lat.unwrap() < 0.2 && (e.n - 0.5).abs() < 1e-9);
        use crate::tune::Kind;
        assert_eq!(generic_values(&Kind::Int { min: 0, max: 100 }, "40", &[]), vec!["20", "80"]);
        assert_eq!(generic_values(&Kind::Int { min: 0, max: 50 }, "40", &[]), vec!["20", "50"]);
        assert!(generic_values(&Kind::Int { min: 0, max: 50 }, "0", &[]).is_empty());
        assert_eq!(generic_values(&Kind::Bool, "1", &[]), vec!["0"]);
        assert_eq!(generic_values(&Kind::Choice, "b", &["a".into(), "b".into(), "c".into()]), vec!["a", "c"]);
        assert_eq!(override_values("cpu.max_freq_ccd0", "5200000", 0).unwrap(), vec!["4700000", "4200000", "3600000"]);
        let mf = override_values("vm.min_free_kbytes", "67584", 32_000_000).unwrap();
        assert!(mf.iter().all(|x| x.parse::<u64>().unwrap() <= 320_000));
        assert!(EXCLUDED.iter().any(|(k, _)| *k == "kernel.watchdog"));
        assert_eq!(phases_for("vm.swappiness", "Memory").len(), 1);
        assert_eq!(phases_for("vm.dirty", "Memory"), vec![(Phase::Io, Benches { io: true, ..Default::default() })]);
        assert_eq!(phases_for("blk.read_ahead_kb", "Storage")[0].0, Phase::Io);
        assert!(phases_for("vm.dirty_writeback_centisecs", "Memory").iter().all(|p| p.0 == Phase::Load), "excluded from the plan, not an IO knob");
        assert_eq!(override_values("blk.read_ahead_kb", "128", 0).unwrap(), vec!["32", "64", "256", "512"]);
        assert_eq!(override_values("blk.nr_requests", "1023", 0).unwrap(), vec!["255", "511"]);
        assert!(override_values("blk.wbt_lat_usec", "2000", 0).unwrap().contains(&"0".to_string()));
        assert_eq!(phases_for("sched.preempt", "Scheduler").len(), 2);
    }

    /// Synthetic design log: `a` (dose ladder) helps a lot, `b` helps nothing; rows carry NaN for unmeasured objectives.
    fn synthetic(c: &mut Calibration) {
        let mut rng = crate::model::Rng::new(5);
        let mut rows = Vec::new();
        for i in 0..60usize {
            let cfg: Cfg = match i % 6 { 0 => vec![], 1 => vec![("vm.a".into(), "16".into())], 2 => vec![("vm.a".into(), "64".into())],
                                           3 => vec![("vm.b".into(), "1".into())], 4 => vec![("vm.a".into(), "64".into()), ("vm.b".into(), "1".into())], _ => vec![("vm.a".into(), "16".into()), ("vm.b".into(), "1".into())] };
            let a = cfg.iter().find(|(k, _)| k == "vm.a").map_or(0.0, |(_, v)| if v == "64" { 0.10 } else { 0.05 });
            let noise = ((rng.unit() + rng.unit()) - 1.0) * 0.03;
            rows.push(Row { phase: Phase::Load, sess: 9, pos: i as f64 / 60.0, t: i as u64 * 5, kernel: String::new(), cfg, y: [a + noise, f64::NAN, f64::NAN, 0.0], w: 1.0, bv: BENCH_VERSION });
        }
        c.put_session(Phase::Load, 9, rows);
        c.refs.insert("vm.a".into(), "8".into());
        c.refs.insert("vm.b".into(), "0".into());
    }

    #[test]
    fn rows_roundtrip_and_feed_standalone_effects() {
        let mut c = Calibration::default();
        synthetic(&mut c);
        c.add_unsafe_set(vec![("vm.a".into(), "64".into()), ("vm.b".into(), "1".into())]);
        c.note_strategy(Phase::Load, "lean/base");
        c.note_strategy(Phase::Load, "lean/crowd");
        let back = Calibration::from_json(&c.to_json());
        assert_eq!(back.strategies_of(Phase::Load), ["lean/base", "lean/crowd"]);
        assert!(back.strategies_of(Phase::Idle).is_empty());
        assert_eq!(back.rows.len(), 60);
        assert_eq!(back.rows[1].cfg, c.rows[1].cfg);
        assert!(back.rows[1].y[1].is_nan() && (back.rows[1].y[0] - c.rows[1].y[0]).abs() < 1e-12);
        assert_eq!(back.refs, c.refs);
        assert!(back.unsafe_cfg(&[("vm.a".into(), "64".into()), ("vm.b".into(), "1".into()), ("x".into(), "1".into())]) && !back.unsafe_cfg(&[("vm.a".into(), "64".into())]));
        // Stand-alone effects come from the joint model: the dose ladder is monotone, the inert knob ~0, the untouched objective absent.
        let e64 = back.get("vm.a", "8", "64", Blend { load: 1.0, io: 1.0 }).unwrap();
        let e16 = back.get("vm.a", "8", "16", Blend { load: 1.0, io: 1.0 }).unwrap();
        let eb = back.get("vm.b", "0", "1", Blend { load: 1.0, io: 1.0 }).unwrap();
        assert!(e64.lat.unwrap() > e16.lat.unwrap() && e16.lat.unwrap() > 0.02, "dose-response {e16:?} {e64:?}");
        assert!(e64.lat.unwrap() > 0.07 && eb.lat.unwrap().abs() < 0.03, "{e64:?} {eb:?}");
        assert!(e64.thr.is_none() && e64.n > 1.0);
        assert!(back.has_key("vm.a") && back.key_names().contains(&"vm.b".to_string()) && back.values("vm.a", "8").unwrap().1.len() == 2);
        assert!(back.get("vm.a", "999", "64", Blend { load: 1.0, io: 1.0 }).is_none(), "another reference: not comparable");
        // A changed reference invalidates the rows that used it.
        let mut c2 = back.clone();
        c2.forget_key("vm.a");
        assert!(c2.rows.iter().all(|r| !r.cfg.iter().any(|(k, _)| k == "vm.a")));
    }

    #[test]
    fn io_phase_is_separate_and_scaled_by_storage_weight() {
        let mut c = Calibration::default();
        let mut rng = crate::model::Rng::new(9);
        let mut rows = Vec::new();
        for i in 0..40usize {
            let cfg: Cfg = match i % 4 { 0 => vec![], 1 | 2 => vec![("blk.read_ahead_kb".into(), "512".into())], _ => vec![("blk.nomerges".into(), "2".into())] };
            let ra = cfg.iter().any(|(k, _)| k == "blk.read_ahead_kb");
            let noise = ((rng.unit() + rng.unit()) - 1.0) * 0.02;
            rows.push(Row { phase: Phase::Io, sess: 3, pos: i as f64 / 40.0, t: i as u64, kernel: String::new(), cfg,
                            y: [0.0 + noise, if ra { 0.2 } else { 0.0 } + noise, 0.0, if ra { -0.1 } else { 0.0 }], w: 1.0, bv: BENCH_VERSION });
        }
        c.put_session(Phase::Io, 3, rows);
        c.refs.insert("blk.read_ahead_kb".into(), "128".into());
        c.refs.insert("blk.nomerges".into(), "0".into());
        // An old idle row that changed an I/O knob stays out of the idle model.
        c.rows.push(Row { phase: Phase::Idle, sess: 1, pos: 0.0, t: 0, kernel: String::new(), cfg: vec![("blk.nomerges".into(), "2".into())],
                          y: [0.5, 0.5, 0.5, 0.5], w: 1.0, bv: 2 });
        assert!(c.phase_fit(Phase::Idle).is_none());
        let full = c.get("blk.read_ahead_kb", "128", "512", Blend { load: 0.5, io: 1.0 }).unwrap();
        let half = c.get("blk.read_ahead_kb", "128", "512", Blend { load: 0.5, io: 0.5 }).unwrap();
        assert!(full.thr.unwrap() > 0.12 && (half.thr.unwrap() - full.thr.unwrap() * 0.5).abs() < 1e-9, "{full:?} {half:?}");
        let j = c.joint([0.0, 1.0, 0.0, 0.0], Blend { load: 0.5, io: 0.5 }).unwrap();
        assert!(j.io.is_some() && j.idle.is_none());
        let (mu, _) = j.eval(&[("blk.read_ahead_kb".into(), "512".into())]);
        assert!((mu - half.thr.unwrap()).abs() < 0.03, "joint io term weighs like the per-knob effect: {mu}");
        assert!(c.joint([1.0; 4], Blend { load: 0.5, io: 0.0 }).is_none(), "storage weight 0: no IO model");
        let back = Calibration::from_json(&c.to_json());
        assert_eq!(back.rows.iter().filter(|r| r.phase == Phase::Io).count(), 40);
    }

    #[test]
    fn joint_blends_phases_by_share() {
        let mut c = Calibration::default();
        synthetic(&mut c);
        let j = c.joint([1.0, 0.0, 0.0, 0.0], Blend { load: 0.5, io: 1.0 }).unwrap();
        assert!(j.idle.is_none() && j.load.is_some());
        let with_a = vec![("vm.a".to_string(), "64".to_string())];
        let (mu, var) = j.eval(&with_a);
        assert!(mu > 0.07 && var > 0.0 && var.sqrt() < 0.05, "{mu} {var}");
        assert!(c.unsafe_cfg(&[]) == false);
    }
}
