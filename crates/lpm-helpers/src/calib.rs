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
//! Two phases: IDLE (quiet machine) and LOAD (ballast holding memory near a
//! safe headroom, churning allocations, half the CPUs busy). Stability risk is
//! never taken from a benchmark; a candidate that caused an OOM kill is unsafe.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Objective { Lat, Thr, Pwr, Mem }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase { Idle, Load }

impl Metric {
    pub const ALL: [Metric; 17] = [Metric::WakeP99Us, Metric::FaultP99Us, Metric::FsyncP99Ms, Metric::RandReadP99Us,
        Metric::PingPongP99Us, Metric::StallsPerSec, Metric::MemBwGbs, Metric::AllocMs, Metric::WriteMbs, Metric::ReadMbs,
        Metric::ThpPct, Metric::CpuSingle, Metric::CpuMulti, Metric::IdleW, Metric::PkgW, Metric::CpuEff, Metric::ProbeRssMib];
    pub fn objective(self) -> Objective {
        use Metric::*;
        match self {
            WakeP99Us | FaultP99Us | FsyncP99Ms | RandReadP99Us | PingPongP99Us | StallsPerSec => Objective::Lat,
            MemBwGbs | AllocMs | WriteMbs | ReadMbs | ThpPct | CpuSingle | CpuMulti => Objective::Thr,
            IdleW | PkgW | CpuEff => Objective::Pwr,
            ProbeRssMib => Objective::Mem,
        }
    }
    pub fn higher_better(self) -> bool {
        use Metric::*;
        matches!(self, MemBwGbs | WriteMbs | ReadMbs | ThpPct | CpuSingle | CpuMulti | CpuEff)
    }
}

/// Which benchmark groups a knob can influence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Benches { pub cpu_mem: bool, pub io: bool, pub idle: bool, pub cpu: bool }

/// Weight of the load phase when autotune blends both, per goal key.
pub fn load_share(goal: &str) -> f64 {
    match goal { "throughput" => 0.7, "gaming" => 0.6, "desktop" => 0.4, _ => 0.2 }
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
        // Noise floor: 2x the larger MAD, at least 2% (timers and power jitter).
        let noise = (2.0 * rel_mad(&r).max(rel_mad(&c))).max(0.02);
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
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
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
    ("thp.mthp_128k", "thp group"), ("thp.mthp_256k", "thp group"), ("thp.mthp_512k", "thp group"), ("thp.mthp_1m", "thp group"),
    ("thp.shmem_enabled", "shmem not probed"), ("thp.khugepaged_defrag", "khugepaged: minutes"), ("thp.khp_max_ptes_none", "khugepaged: minutes"),
    ("thp.khp_pages_to_scan", "khugepaged: minutes"), ("thp.khp_scan_sleep_ms", "khugepaged: minutes"),
    ("thp.khp_max_ptes_swap", "khugepaged: minutes"), ("thp.khp_alloc_sleep_ms", "khugepaged: minutes"),
    ("mm.lru_gen", "switching MGLRU off under load stalls"), ("mm.ksm_run", "no effect without mergeable memory"),
    ("vm.max_map_count", "a limit, not a cost"), ("vm.dirty_ratio", "tested as vm.dirty"), ("vm.dirty_background_ratio", "vm.dirty"),
    ("vm.dirty_bytes", "vm.dirty"), ("vm.dirty_background_bytes", "vm.dirty"),
    ("sched.ext", "needs a userspace scheduler"), ("kernel.watchdog", "never disabled"), ("kernel.sched_schedstats", "debug counters"),
    ("kernel.cfs_bandwidth_slice_us", "only with CPU quotas"), ("kernel.sched_util_clamp_min_rt_default", "RT tasks only"),
    ("wq.cpumask", "topology (structural)"), ("irq.affinity", "topology (structural)"),
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
        "vm.dirty_writeback_centisecs" => v(&[500, 1500]),
        "vm.dirty_expire_centisecs" => v(&[3000, 6000]),
        "vm.stat_interval" => v(&[1, 10]),
        "vm.page_lock_unfairness" => v(&[1, 5, 20]),
        "zswap.max_pool_percent" => v(&[10, 20, 30]),
        "sched.migration_cost_ns" => v(&[r / 2, r * 2, 5_000_000]),
        "sched.nr_migrate" => v(&[8, 32, 128]),
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
    match key {
        "vm.dirty" => vec![(Phase::Idle, b(false, true, false, false)), (Phase::Load, b(true, true, false, false))],
        "thp" => vec![(Phase::Idle, b(true, false, false, false)), (Phase::Load, b(true, false, false, false))],
        "vm.dirty_writeback_centisecs" | "vm.dirty_expire_centisecs" => vec![(Phase::Idle, b(false, true, true, false))],
        "vm.stat_interval" => vec![(Phase::Idle, b(false, false, true, true))],
        _ => match group {
            "CPU" => vec![(Phase::Idle, b(false, false, true, true)), (Phase::Load, b(false, false, false, true))],
            "Scheduler" => vec![(Phase::Idle, b(false, false, false, true)), (Phase::Load, b(true, false, false, true))],
            "Memory" => vec![(Phase::Load, b(true, false, false, false))],
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

/// The machine signature.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Calibration {
    pub fingerprint: Fingerprint,
    pub keys: BTreeMap<String, Vec<Group>>,
    pub on_battery: bool,
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
        aggregate(if phase == Phase::Idle { &s.idle } else { &s.load }, self.now, &self.kernel)
    }
    /// Effects of `value` relative to `reference` in one phase (re-based through the stored reference).
    pub fn get_phase(&self, key: &str, reference: &str, value: &str, phase: Phase) -> Option<Measured> {
        if value == reference { return None; }
        let g = self.group(key, reference)?;
        let at = |v: &str| g.values.get(v).and_then(|s| self.agg(s, phase));
        if g.reference == reference { return at(value); }
        let b = at(reference)?;
        if value == g.reference { return Some(neg(b)); }
        let a = at(value)?;
        let d = |x: Option<f64>, y: Option<f64>| Some(x? - y?);
        Some(Measured { lat: d(a.lat, b.lat), thr: d(a.thr, b.thr), pwr: d(a.pwr, b.pwr), mem: d(a.mem, b.mem), n: a.n.min(b.n) })
    }
    /// Both phases blended: `load` = weight of the load phase (0..1). A phase
    /// without data leaves the other in full.
    pub fn get(&self, key: &str, reference: &str, value: &str, load: f64) -> Option<Measured> {
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
    pub fn is_unsafe(&self, key: &str, value: &str) -> bool {
        self.keys.get(key).map_or(false, |gs| gs.iter().any(|g| g.values.get(value).map_or(false, |s| s.unsafe_)))
    }
    /// Every value measured for `key` (any phase), with the group's reference.
    pub fn values(&self, key: &str, reference: &str) -> Option<(String, Vec<String>)> {
        let g = self.group(key, reference)?;
        Some((g.reference.clone(), g.values.keys().cloned().collect()))
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
        if let Some(m) = m {
            let v = if phase == Phase::Idle { &mut s.idle } else { &mut s.load };
            v.push(Rec { m, t, kernel: kernel.to_owned() });
            let drop = v.len().saturating_sub(KEEP);
            v.drain(..drop);
        }
    }
    /// Records for `key` in `phase` (all values): drives "least covered first".
    pub fn coverage(&self, key: &str, phase: Phase) -> usize {
        self.keys.get(key).map_or(0, |gs| gs.iter().flat_map(|g| g.values.values())
            .map(|s| if phase == Phase::Idle { s.idle.len() } else { s.load.len() }).sum())
    }

    pub fn to_json(&self) -> Value {
        let rec = |r: &Rec| json!({"lat": r.m.lat, "thr": r.m.thr, "pwr": r.m.pwr, "mem": r.m.mem, "t": r.t, "k": r.kernel});
        let keys: Map<String, Value> = self.keys.iter().map(|(k, gs)| (k.clone(), Value::Array(gs.iter().map(|g| {
            let v: Map<String, Value> = g.values.iter().map(|(x, s)| (x.clone(), json!({
                "idle": s.idle.iter().map(rec).collect::<Vec<_>>(), "load": s.load.iter().map(rec).collect::<Vec<_>>(), "unsafe": s.unsafe_}))).collect();
            json!({"reference": g.reference, "values": v})
        }).collect()))).collect();
        json!({"version": 3, "fingerprint": self.fingerprint.to_json(), "on_battery": self.on_battery, "keys": keys})
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
        c
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
    let mut p: Vec<&(i64, Measured)> = points.iter().filter(|(v, _)| *v >= 0).collect();
    p.sort_by_key(|(v, _)| *v);
    let mut out = Vec::new();
    for w in p.windows(2) {
        let ((a, ma), (b, mb)) = (w[0], w[1]);
        if b - a < 2 { continue; }
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
        let b = c.get("k", "128", "1024", 0.6).unwrap();
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
        // Version 2 files load.
        let v2 = json!({"version": 2, "keys": {"k": {"reference": "a", "values": {"b": {"idle": {"lat": 0.1}, "load": null, "unsafe": false}}}}});
        let c3 = Calibration::from_json(&v2);
        assert_eq!(c3.get_phase("k", "a", "b", Phase::Idle).unwrap().lat, Some(0.1));
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
        assert_eq!(phases_for("sched.preempt", "Scheduler").len(), 2);
    }
}
