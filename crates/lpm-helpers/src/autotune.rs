//! Autotune: profiles the machine and derives an Optimizations preset for one
//! of four goals (power saving, gaming, throughput, optimal desktop).
//!
//! Stages (pure where it matters, so every rule is testable without sysfs):
//!   1. [`Profile::gather`]  hardware / kernel facts, live values, evidence
//!                           (vmstat, PSI) and the disk's sustained write rate.
//!   2. [`decide_weighted`]  two kinds of rules:
//!        * structural rules: topology/driver facts that have one right answer
//!          (pstate mode, CCD roles, amdgpu DPM auto, bring a parked CCD back);
//!        * scored knobs: every knob with a real trade-off is a set of candidate
//!          values, each with an effect vector over five objectives
//!          (latency, throughput, power, memory footprint, stability risk);
//!          the storage weight says how much the I/O objectives measured by
//!          lpm-calibrate's IO phase count for the goal.
//!          U = sum(w_o * effect_o) - MODESTY * deviation; the reference
//!          ("leave it", or the kernel default) wins unless a candidate beats
//!          it by MARGIN. Effects are ordinal estimates from kernel docs and
//!          measured evidence, never "bigger is better": every non-reference
//!          candidate carries a documented cost.
//!   3. [`enforce`]          hard constraints no weight can buy: dirty pair
//!                           ordering, THP/mTHP/max_ptes_none, watermark and
//!                           reserve envelopes, EPP vs governor, boot params.
//!   4. [`autotune_with`]    tune::validate + [`audit`] + availability filter.
//!
//! [`audit`] also checks presets/scenes written by older versions (int32-
//! wrapped dirty limits etc.); tune-helper refuses values it rejects, and
//! [`guard_verdict`] is the rollback rule of the post-apply pressure guard.
//! Reasoning per knob: docs/AUTOTUNE.md.

use crate::calib;
use crate::model;
use crate::defaults;
use crate::iorate;
use crate::tune;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// Cost per unit of deviation from the reference ("be modest").
const MODESTY: f64 = 0.10;
/// A candidate must beat the reference by this much to be written.
pub const MARGIN: f64 = 0.03;
/// The benchmarks "see" a knob when at least one of its values moves this goal's utility by
/// this many posterior standard deviations. Below it a measured "no effect" is the benchmark's
/// blindness, not knowledge: the rule's estimate stays.
const SEEN_Z: f64 = 2.0;
/// A value a rule chose leaves the joint decision only when the model puts it this many
/// standard deviations below the reference.
const KEEP_Z: f64 = 1.0;
/// Largest integer the GUI's JSON layer (double) carries exactly.
pub const JSON_SAFE_INT: i64 = 1 << 53;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Goal { PowerSave, Gaming, Throughput, Desktop }

impl Goal {
    pub const ALL: [Goal; 4] = [Goal::PowerSave, Goal::Gaming, Goal::Throughput, Goal::Desktop];
    pub fn parse(s: &str) -> Option<Goal> {
        match s.to_ascii_lowercase().replace(['-', '_', ' '], "").as_str() {
            "powersave" | "powersaving" | "power" | "battery" => Some(Goal::PowerSave),
            "gaming" | "game" | "latency" => Some(Goal::Gaming),
            "throughput" | "barethroughput" | "compute" => Some(Goal::Throughput),
            "desktop" | "optimaldesktop" | "balanced" => Some(Goal::Desktop),
            _ => None,
        }
    }
    pub fn key(self) -> &'static str {
        match self { Goal::PowerSave => "powersave", Goal::Gaming => "gaming", Goal::Throughput => "throughput", Goal::Desktop => "desktop" }
    }
    pub fn label(self) -> &'static str {
        match self {
            Goal::PowerSave => "Power saving",
            Goal::Gaming => "Gaming (latency + throughput)",
            Goal::Throughput => "Bare throughput",
            Goal::Desktop => "Optimal desktop",
        }
    }
    /// Preset name (valid for lpm-gamemode / the GUI: letters, digits, space _ - .).
    pub fn preset_name(self) -> &'static str {
        match self { Goal::PowerSave => "Auto Power saving", Goal::Gaming => "Auto Gaming",
                     Goal::Throughput => "Auto Throughput", Goal::Desktop => "Auto Desktop" }
    }
}

/// Objective weights. The goal gives the defaults; the user (GUI / CLI /
/// scene) may override each one. Stability never drops below 0.5: weights
/// shift trade-offs, hard constraints are not for sale.
///
/// `storage` is a relevance, not an objective: the calibrated I/O knobs (Storage rows, dirty
/// window) are measured by their own benchmark suite, whose latency / throughput / power /
/// footprint are weighed with the four weights above times `storage`. 0 = storage does not
/// matter for this goal (the I/O knobs keep their rule-based values), 1 = an I/O effect counts
/// as much as the same CPU/memory effect. The defaults follow what each goal waits on: a game
/// streams assets but is mostly CPU/GPU-bound, a desktop waits on saves and launches, bulk work
/// moves data.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Weights { pub latency: f64, pub throughput: f64, pub power: f64, pub footprint: f64, pub stability: f64, pub storage: f64 }

impl Weights {
    pub const KEYS: [&'static str; 6] = ["latency", "throughput", "power", "footprint", "stability", "storage"];
    pub fn for_goal(g: Goal) -> Weights {
        let w = |latency, throughput, power, footprint, storage| Weights { latency, throughput, power, footprint, stability: 1.0, storage };
        match g {
            Goal::Gaming => w(1.0, 0.6, 0.15, 0.4, 0.5),
            Goal::Desktop => w(0.7, 0.3, 0.7, 0.6, 0.6),
            Goal::Throughput => w(0.2, 1.0, 0.2, 0.5, 0.7),
            Goal::PowerSave => w(0.2, 0.1, 1.0, 0.5, 0.4),
        }
    }
    /// {"latency": 1.2, ...}; unknown keys and non-numbers are ignored.
    pub fn with_overrides(mut self, v: &Value) -> Weights {
        let Some(o) = v.as_object() else { return self };
        for (k, x) in o {
            let Some(n) = x.as_f64().filter(|n| n.is_finite()) else { continue };
            let n = n.clamp(0.0, 3.0);
            match k.as_str() {
                "latency" => self.latency = n, "throughput" => self.throughput = n, "power" => self.power = n,
                "footprint" => self.footprint = n, "stability" => self.stability = n.max(0.5), "storage" => self.storage = n, _ => {}
            }
        }
        self
    }
    pub fn to_json(&self) -> Value {
        json!({"latency": self.latency, "throughput": self.throughput, "power": self.power,
               "footprint": self.footprint, "stability": self.stability, "storage": self.storage})
    }
}

/// Effect of one candidate relative to the reference, per objective. Positive
/// = better for that objective; `risk` is a stability cost (<= 0).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fx { pub lat: f64, pub thr: f64, pub pwr: f64, pub mem: f64, pub risk: f64 }

impl Fx {
    fn u(&self, w: &Weights) -> f64 {
        w.latency * self.lat + w.throughput * self.thr + w.power * self.pwr + w.footprint * self.mem + w.stability * self.risk
    }
    fn explain(&self, w: &Weights, dev: f64) -> String {
        let mut p = Vec::new();
        for (n, e, wt) in [("latency", self.lat, w.latency), ("throughput", self.thr, w.throughput), ("power", self.pwr, w.power),
                           ("memory", self.mem, w.footprint), ("stability", self.risk, w.stability)] {
            if e.abs() >= 0.005 { p.push(format!("{n} {:+.2}×{wt:.1}", e)); }
        }
        if dev > 0.0 { p.push(format!("modesty {:+.2}", -MODESTY * dev)); }
        format!("[U {:+.2}: {}]", self.u(w) - MODESTY * dev, p.join(", "))
    }
}

fn fx(lat: f64, thr: f64, pwr: f64, mem: f64, risk: f64) -> Fx { Fx { lat, thr, pwr, mem, risk } }

/// One candidate value of a scored knob. `value` Null = do not write.
pub struct Cand { value: Value, fx: Fx, dev: f64, why: String }

fn cand(value: impl Into<Value>, fx: Fx, dev: f64, why: impl Into<String>) -> Cand {
    Cand { value: value.into(), fx, dev, why: why.into() }
}
/// "Leave it as it is" (the reference for knobs whose kernel default varies).
fn leave() -> Cand { Cand { value: Value::Null, fx: Fx::default(), dev: 0.0, why: String::new() } }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwapKind { None, Zram, ZswapDisk, Ssd, Hdd }

impl SwapKind {
    fn as_str(self) -> &'static str {
        match self { SwapKind::None => "none", SwapKind::Zram => "zram", SwapKind::ZswapDisk => "disk + zswap",
                     SwapKind::Ssd => "disk (SSD/NVMe)", SwapKind::Hdd => "disk (HDD)" }
    }
}

#[derive(Clone, Debug)]
pub struct CState { pub name: String, pub latency_us: u64 }


/// What the running system has actually been through: cumulative reclaim /
/// compaction / swap counters since boot plus pressure-stall averages. Rules
/// use it to scale the *benefit* of a knob (a knob that fixes a problem this
/// machine does not have earns nothing) and to raise safety margins. Counters
/// from a machine that has just booted (or scanned almost nothing) carry no weight.
#[derive(Clone, Debug, Default)]
pub struct Evidence {
    pub uptime_s: u64,
    pub pgscan_direct: u64,
    pub pgscan_kswapd: u64,
    pub allocstall: u64,
    pub compact_stall: u64,
    pub thp_fault_alloc: u64,
    pub thp_fault_fallback: u64,
    pub pswpout: u64,
    pub kswapd_low_wmark_quick: u64,
    pub workingset_refault_file: u64,
    /// PSI 5-minute averages in percent.
    pub psi_mem_some: Option<f64>,
    pub psi_mem_full: Option<f64>,
    pub psi_cpu_some: Option<f64>,
    pub psi_io_some: Option<f64>,
    pub psi_io_full: Option<f64>,
}

impl Evidence {
    pub fn gather() -> Evidence {
        let mut e = Evidence::default();
        e.uptime_s = rd("/proc/uptime").and_then(|s| s.split('.').next().and_then(|n| n.parse().ok())).unwrap_or(0);
        e.parse_vmstat(&std::fs::read_to_string("/proc/vmstat").unwrap_or_default());
        let mem = rd("/proc/pressure/memory").unwrap_or_default();
        let io = rd("/proc/pressure/io").unwrap_or_default();
        e.psi_mem_some = psi_avg300(&mem, "some");
        e.psi_mem_full = psi_avg300(&mem, "full");
        e.psi_cpu_some = psi_avg300(&rd("/proc/pressure/cpu").unwrap_or_default(), "some");
        e.psi_io_some = psi_avg300(&io, "some");
        e.psi_io_full = psi_avg300(&io, "full");
        e
    }

    pub fn parse_vmstat(&mut self, text: &str) {
        for l in text.lines() {
            let Some((k, v)) = l.split_once(' ') else { continue };
            let Ok(n) = v.trim().parse::<u64>() else { continue };
            match k {
                "pgscan_direct" => self.pgscan_direct = n,
                "pgscan_kswapd" => self.pgscan_kswapd = n,
                "compact_stall" => self.compact_stall = n,
                "thp_fault_alloc" => self.thp_fault_alloc = n,
                "thp_fault_fallback" => self.thp_fault_fallback = n,
                "pswpout" => self.pswpout = n,
                "kswapd_low_wmark_hit_quickly" => self.kswapd_low_wmark_quick = n,
                "workingset_refault_file" => self.workingset_refault_file = n,
                _ if k.starts_with("allocstall_") => self.allocstall += n,
                _ => {}
            }
        }
    }

    /// Share of scanned pages that the allocating thread itself had to reclaim
    /// (direct reclaim = a stall on that thread). None without enough history.
    pub fn direct_reclaim_share(&self) -> Option<f64> {
        let total = self.pgscan_direct + self.pgscan_kswapd;
        (self.uptime_s >= 3600 && total >= 200_000).then(|| self.pgscan_direct as f64 / total as f64)
    }
    /// Direct reclaim is a regular event, not a one-off.
    pub fn reclaim_stalls(&self) -> bool {
        self.direct_reclaim_share().map_or(false, |s| s >= 0.10) && self.allocstall >= 1000
    }
    /// 0..1: how strongly this machine's history says kswapd starts too late
    /// (direct reclaim share, or kswapd hitting the low watermark right after
    /// going to sleep - the kernel doc's two symptoms).
    pub fn reclaim_strength(&self) -> f64 {
        let share = self.direct_reclaim_share().map_or(0.0, |s| (s / 0.20).min(1.0));
        let stalls = if self.allocstall >= 1000 { 1.0 } else { self.allocstall as f64 / 1000.0 };
        let quick = if self.uptime_s >= 3600 { (self.kswapd_low_wmark_quick as f64 / (self.uptime_s as f64 / 60.0)).min(1.0) } else { 0.0 };
        (share * stalls).max(quick * 0.5)
    }
    /// Memory is short right now (tasks are stalling on it).
    pub fn mem_pressure_now(&self) -> bool {
        self.psi_mem_full.map_or(false, |v| v >= 1.0) || self.psi_mem_some.map_or(false, |v| v >= 10.0)
    }
    /// Huge pages are being used a lot (THP fault volume since boot).
    pub fn thp_heavy(&self) -> bool { self.thp_fault_alloc + self.thp_fault_fallback >= 50_000 }
    /// One line for the report header; empty without any signal.
    pub fn summary(&self) -> String {
        let mut p = Vec::new();
        if let Some(s) = self.direct_reclaim_share() {
            p.push(format!("direct reclaim {:.0}% of scanned pages ({} stalls)", s * 100.0, self.allocstall));
        }
        if let (Some(a), Some(f)) = (self.psi_mem_some, self.psi_mem_full) { p.push(format!("memory pressure {a:.1}% some / {f:.1}% full (5 min)")); }
        if let (Some(a), Some(f)) = (self.psi_io_some, self.psi_io_full) { p.push(format!("I/O pressure {a:.1}% some / {f:.1}% full (5 min)")); }
        if let Some(c) = self.psi_cpu_some { p.push(format!("CPU pressure {c:.1}% (5 min)")); }
        let faults = self.thp_fault_alloc + self.thp_fault_fallback;
        if faults >= 1000 { p.push(format!("THP fault success {:.0}%", self.thp_fault_alloc as f64 * 100.0 / faults as f64)); }
        p.join(" · ")
    }
}

/// avg300 (percent) of the `some` / `full` line of a /proc/pressure file.
pub fn psi_avg300(text: &str, kind: &str) -> Option<f64> { psi_field(text, kind, "avg300=") }
pub fn psi_avg10(text: &str, kind: &str) -> Option<f64> { psi_field(text, kind, "avg10=") }
fn psi_field(text: &str, kind: &str, field: &str) -> Option<f64> {
    text.lines().find(|l| l.starts_with(kind))?.split_whitespace()
        .find_map(|w| w.strip_prefix(field)).and_then(|v| v.parse().ok())
}

// ── post-apply guard ─────────────────────────────────────────────────────────

/// One 1 Hz sample for the post-apply guard (tune-helper samples, this decides).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pressure { pub io_full10: f64, pub mem_full10: f64, pub allocstall: u64 }

impl Pressure {
    pub fn sample() -> Pressure {
        let mut e = Evidence::default();
        e.parse_vmstat(&std::fs::read_to_string("/proc/vmstat").unwrap_or_default());
        Pressure {
            io_full10: psi_avg10(&rd("/proc/pressure/io").unwrap_or_default(), "full").unwrap_or(0.0),
            mem_full10: psi_avg10(&rd("/proc/pressure/memory").unwrap_or_default(), "full").unwrap_or(0.0),
            allocstall: e.allocstall,
        }
    }
}

/// Samples the guard watches after an apply, and the streaks that trip it.
pub const GUARD_SECS: usize = 120;
const GUARD_IO_STREAK: usize = 15;
const GUARD_MEM_STREAK: usize = 10;
const GUARD_STALL_STREAK: usize = 10;

/// Keys the guard protects (those that can stall writers or reclaim).
pub fn guarded(key: &str) -> bool {
    ["vm.", "thp.", "mm.", "zswap."].iter().any(|p| key.starts_with(p)) || key == "blk.wbt_lat_usec"
}

/// Rollback rule. `base` is taken right before the apply, `window` holds the
/// 1 Hz samples since. Sustained "full" stalls (every non-idle task waiting)
/// well above what the machine showed before trip it; a busy build alone
/// shows "some", not tens of percent "full" for 10-15 s.
pub fn guard_verdict(base: &Pressure, window: &[Pressure]) -> Option<String> {
    let io_lim = (base.io_full10 * 2.0 + 10.0).max(25.0);
    let mem_lim = (base.mem_full10 + 5.0).max(10.0);
    let streak = |f: &dyn Fn(usize) -> bool, n: usize| {
        let mut run = 0;
        for i in 0..window.len() { if f(i) { run += 1; if run >= n { return true; } } else { run = 0; } }
        false
    };
    if streak(&|i| window[i].io_full10 >= io_lim, GUARD_IO_STREAK) {
        return Some(format!("I/O stall: PSI io full avg10 stayed >= {io_lim:.0}% for {GUARD_IO_STREAK} s after the apply (before: {:.1}%)", base.io_full10));
    }
    if streak(&|i| window[i].mem_full10 >= mem_lim, GUARD_MEM_STREAK) {
        return Some(format!("memory stall: PSI memory full avg10 stayed >= {mem_lim:.0}% for {GUARD_MEM_STREAK} s (before: {:.1}%)", base.mem_full10));
    }
    if streak(&|i| {
        let prev = if i == 0 { base.allocstall } else { window[i - 1].allocstall };
        window[i].allocstall.saturating_sub(prev) >= 500
    }, GUARD_STALL_STREAK) {
        return Some(format!("direct reclaim storm: >= 500 allocation stalls/s for {GUARD_STALL_STREAK} s"));
    }
    None
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub vendor: tune::Vendor,
    pub model: String,
    pub logical: usize,
    pub cores: usize,
    pub smt_active: bool,
    /// L3 domains of online CPUs (index, cpus, l3 KiB, max kHz).
    pub ccds: Vec<tune::Ccx>,
    pub cache_ccd: Option<usize>,
    pub freq_ccd: Option<usize>,
    pub x3d_driver: bool,
    pub hybrid: bool,
    pub driver: String,
    pub epp: bool,
    pub governors: Vec<String>,
    pub idle_governors: Vec<String>,
    pub cstates: Vec<CState>,
    pub ram_kb: u64,
    pub swap: SwapKind,
    pub nvme: bool,
    pub rotational: bool,
    pub sata_hosts: bool,
    pub battery: bool,
    pub on_ac: Option<bool>,
    pub nvidia_dgpu: bool,
    pub amd_igpu: bool,
    pub intel_igpu: bool,
    pub wifi: bool,
    pub kernel: (u32, u32),
    pub numa_nodes: usize,
    pub scx: Vec<String>,
    pub dynamic_epp: bool,
    pub uncore: Option<(u64, u64)>,
    pub evidence: Evidence,
    /// Sustained write rate of the disk(s) that take ordinary writes.
    pub io: iorate::IoRate,
    /// /proc/cmdline: boot parameters the user chose are hard constraints.
    pub cmdline: String,
    /// Tunable values captured early in boot (kernel + distro + user sysctl,
    /// before TLP/LPM): the anchor scored knobs adapt from.
    pub defaults: Option<defaults::Defaults>,
    /// Per key: applies / guard rollbacks on this machine.
    pub outcomes: BTreeMap<String, defaults::Outcome>,
    /// Measured effects from lpm-calibrate (replace the estimated ones).
    pub calibration: Option<calib::Calibration>,
    /// Live values of the rows whose rule depends on the current state.
    pub current: BTreeMap<String, String>,
}

fn rd(p: impl AsRef<Path>) -> Option<String> { crate::read_trimmed(p.as_ref()).ok() }

fn kernel_release() -> (u32, u32) {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 { return (0, 0); }
    let rel = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) }.to_string_lossy().into_owned();
    parse_release(&rel)
}

pub fn parse_release(rel: &str) -> (u32, u32) {
    let mut it = rel.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).map(|x| x.parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// "/dev/nvme0n1p3" -> rotational flag of the whole disk (None if unknown).
fn dev_rotational(dev: &str) -> Option<bool> {
    let name = Path::new(dev).file_name()?.to_string_lossy().into_owned();
    let node = std::fs::canonicalize(Path::new("/sys/class/block").join(&name)).ok()?;
    let disk = if node.join("partition").is_file() { node.parent()?.to_path_buf() } else { node };
    rd(disk.join("queue/rotational")).map(|r| r == "1")
}

/// Swap devices from /proc/swaps; the most important one decides the kind.
fn swap_kind(zswap_on: bool) -> SwapKind {
    let text = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    let mut kinds = Vec::new();
    for l in text.lines().skip(1) {
        let mut f = l.split_whitespace();
        let (Some(name), Some(ty)) = (f.next(), f.next()) else { continue };
        if name.contains("/zram") { kinds.push(SwapKind::Zram); continue; }
        let rot = if ty == "partition" { dev_rotational(name) } else {
            // Swap file: judge by the disk holding the root file system.
            let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
            mounts.lines().find(|m| m.split_whitespace().nth(1) == Some("/"))
                .and_then(|m| m.split_whitespace().next().map(str::to_owned))
                .and_then(|d| dev_rotational(&d))
        };
        kinds.push(match (zswap_on, rot) { (true, _) => SwapKind::ZswapDisk, (_, Some(true)) => SwapKind::Hdd, _ => SwapKind::Ssd });
    }
    if kinds.contains(&SwapKind::Zram) { SwapKind::Zram } else { kinds.first().copied().unwrap_or(SwapKind::None) }
}

impl Profile {
    pub fn gather() -> Profile {
        let cpu = Path::new(tune::CPU_DIR);
        let online = tune::online_cpus();
        let mut cores = std::collections::BTreeSet::new();
        for c in &online {
            if let Some(s) = rd(cpu.join(format!("cpu{c}/topology/thread_siblings_list"))) { cores.insert(s); }
        }
        let ccds = tune::ccx_groups();
        let pol0 = cpu.join("cpufreq/policy0");
        let pol = tune::policies().into_iter().next().unwrap_or(pol0);
        let list = |p: &Path| rd(p).map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
        let cstates: Vec<CState> = tune::cstate_names().into_iter().enumerate().map(|(i, name)| CState {
            name,
            latency_us: rd(cpu.join(format!("cpu0/cpuidle/state{i}/latency"))).and_then(|s| s.parse().ok()).unwrap_or(0),
        }).collect();
        let ram_kb = std::fs::read_to_string("/proc/meminfo").unwrap_or_default().lines()
            .find_map(|l| l.strip_prefix("MemTotal:").and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok()))
            .unwrap_or(0);
        let zswap_on = matches!(rd("/sys/module/zswap/parameters/enabled").as_deref(), Some("Y") | Some("1"));
        let disks = tune::block_devs();
        let rot = |d: &Path| rd(d.join("queue/rotational")).as_deref() == Some("1");
        let mut battery = false;
        let mut on_ac = None;
        for e in std::fs::read_dir("/sys/class/power_supply").into_iter().flatten().flatten() {
            let d = e.path();
            match rd(d.join("type")).as_deref() {
                Some("Battery") if rd(d.join("scope")).as_deref() != Some("Device") => battery = true,
                Some("Mains") => on_ac = Some(rd(d.join("online")).as_deref() == Some("1")),
                _ => {}
            }
        }
        let (mut nvidia, mut amd_i, mut intel_i) = (false, false, false);
        for e in std::fs::read_dir("/sys/bus/pci/devices").into_iter().flatten().flatten() {
            let d = e.path();
            if !rd(d.join("class")).map_or(false, |c| c.starts_with("0x03")) { continue; }
            match rd(d.join("vendor")).as_deref() {
                Some("0x10de") => nvidia = true,
                Some("0x1002") => amd_i = true,
                Some("0x8086") => intel_i = true,
                _ => {}
            }
        }
        let wifi = std::fs::read_dir("/sys/class/net").into_iter().flatten().flatten()
            .any(|e| e.path().join("wireless").is_dir() || e.path().join("phy80211").exists());
        let numa_nodes = rd("/sys/devices/system/node/possible").map(|s| tune::cpu_list(&s).len()).unwrap_or(1).max(1);
        let scx = tune::find("sched.ext").map(|t| tune::options(t).into_iter().map(|o| o.0).filter(|n| n != "none").collect())
            .unwrap_or_default();
        let uncore = std::fs::read_dir(cpu.join("intel_uncore_frequency")).into_iter().flatten().flatten().next().and_then(|e| {
            let d = e.path();
            Some((rd(d.join("initial_min_freq_khz"))?.parse().ok()?, rd(d.join("initial_max_freq_khz"))?.parse().ok()?))
        });
        let mut current = BTreeMap::new();
        for k in LIVE_KEYS {
            if let Some(v) = tune::find(k).and_then(tune::current) { current.insert((*k).to_owned(), v); }
        }
        Profile {
            vendor: tune::cpu_vendor(),
            model: crate::cpuinfo_head().lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == "model name").map(|(_, v)| v.trim().to_owned()))
                .unwrap_or_default(),
            logical: online.len(),
            cores: cores.len().max(1),
            smt_active: rd(cpu.join("smt/active")).as_deref() == Some("1"),
            cache_ccd: tune::resolve_ccd(&ccds, "cache").map(|g| g.index),
            freq_ccd: tune::resolve_ccd(&ccds, "frequency").map(|g| g.index),
            ccds,
            x3d_driver: tune::x3d_mode_path().is_some(),
            hybrid: tune::hybrid().is_some(),
            driver: rd(pol.join("scaling_driver")).unwrap_or_default(),
            epp: pol.join("energy_performance_preference").is_file(),
            governors: list(&pol.join("scaling_available_governors")),
            idle_governors: list(&cpu.join("cpuidle/available_governors")),
            cstates,
            ram_kb,
            swap: swap_kind(zswap_on),
            nvme: disks.iter().any(|d| d.file_name().map_or(false, |n| n.to_string_lossy().starts_with("nvme"))),
            rotational: disks.iter().any(|d| rot(d)),
            sata_hosts: std::fs::read_dir("/sys/class/scsi_host").into_iter().flatten().flatten()
                .any(|e| e.path().join("link_power_management_policy").is_file()),
            battery,
            on_ac,
            nvidia_dgpu: nvidia,
            amd_igpu: amd_i,
            intel_igpu: intel_i,
            wifi,
            kernel: kernel_release(),
            numa_nodes,
            scx,
            dynamic_epp: cpu.join("amd_pstate/dynamic_epp").is_file(),
            uncore,
            evidence: Evidence::gather(),
            io: iorate::gather(),
            cmdline: rd("/proc/cmdline").unwrap_or_default(),
            defaults: defaults::load(),
            outcomes: defaults::load_outcomes(),
            calibration: calib::Calibration::load(),
            current,
        }
    }

    pub fn ram_gb(&self) -> u64 { (self.ram_kb + 512 * 1024) / (1024 * 1024) }
    fn amd(&self) -> bool { self.vendor == tune::Vendor::Amd }
    fn intel(&self) -> bool { self.vendor == tune::Vendor::Intel }
    fn multi_ccd(&self) -> bool { self.ccds.len() > 1 }
    /// Asymmetric two-die X3D part (V-Cache die and frequency die both resolvable).
    fn x3d(&self) -> bool { self.multi_ccd() && self.cache_ccd.is_some() && self.freq_ccd.is_some() && self.cache_ccd != self.freq_ccd }
    fn kernel_at_least(&self, maj: u32, min: u32) -> bool { self.kernel >= (maj, min) }
    /// Frequency is chosen by the scheduler (schedutil), not by CPPC/HWP.
    fn schedutil(&self) -> bool { !self.epp && self.governors.iter().any(|g| g == "schedutil") }
    fn cur(&self, k: &str) -> Option<&str> { self.current.get(k).map(String::as_str) }
    /// Replaces a candidate's estimated latency/throughput/power/memory effects
    /// with calibrated ones (stability risk stays the model's).
    fn apply_measured(&self, b: calib::Blend, key: &str, reference: &str, value: &str, c: &mut Cand) {
        let Some(m) = self.calibration.as_ref().and_then(|cal| cal.get(key, reference, value, b)) else { return };
        overlay(&mut c.fx, m);
        c.why.push_str(" (effects measured on this machine)");
    }
    /// A candidate that caused an OOM kill during the calibration load phase.
    fn measured_unsafe(&self, key: &str, value: &str) -> bool {
        self.calibration.as_ref().map_or(false, |c| c.is_unsafe(key, value))
    }
    /// Boot-time value of a tunable, when a snapshot exists.
    pub fn boot_default(&self, k: &str) -> Option<&str> { self.defaults.as_ref()?.values.get(k).map(String::as_str) }
    /// Stability cost learned from guard rollbacks: None = key retired (>= 2 rollbacks).
    fn learned_risk(&self, k: &str) -> Option<f64> {
        let o = self.outcomes.get(k).copied().unwrap_or_default();
        if o.rollbacks >= 2 { return None; }
        Some(-0.6 * o.rollbacks as f64 / (o.applies as f64 + 1.0))
    }
    /// Value of a boot parameter (`name=value`), or "" for a bare flag.
    pub fn boot_param(&self, name: &str) -> Option<&str> {
        self.cmdline.split_whitespace().find_map(|w| {
            if w == name { return Some(""); }
            w.strip_prefix(name).and_then(|r| r.strip_prefix('='))
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "vendor": self.vendor.as_str(), "model": self.model, "logical_cpus": self.logical, "cores": self.cores,
            "smt": self.smt_active,
            "ccds": self.ccds.iter().map(|c| json!({"index": c.index, "cpus": tune::fmt_cpu_list(&c.cpus),
                                                     "l3_mb": c.l3_kib / 1024, "max_mhz": c.max_khz / 1000})).collect::<Vec<_>>(),
            "cache_ccd": self.cache_ccd, "frequency_ccd": self.freq_ccd, "x3d": self.x3d(), "hybrid": self.hybrid,
            "cpufreq_driver": self.driver, "epp": self.epp, "governors": self.governors,
            "idle_governors": self.idle_governors,
            "cstates": self.cstates.iter().map(|c| json!({"name": c.name, "latency_us": c.latency_us})).collect::<Vec<_>>(),
            "ram_gb": self.ram_gb(), "swap": self.swap.as_str(), "nvme": self.nvme, "rotational_disk": self.rotational,
            "sata": self.sata_hosts, "battery": self.battery, "on_ac": self.on_ac,
            "gpu": {"nvidia": self.nvidia_dgpu, "amd": self.amd_igpu, "intel": self.intel_igpu},
            "wifi": self.wifi, "kernel": format!("{}.{}", self.kernel.0, self.kernel.1), "numa_nodes": self.numa_nodes,
            "sched_ext": self.scx, "dynamic_epp": self.dynamic_epp,
            "evidence": {"uptime_s": self.evidence.uptime_s, "direct_reclaim_share": self.evidence.direct_reclaim_share(),
                         "allocstall": self.evidence.allocstall, "compact_stall": self.evidence.compact_stall,
                         "psi_mem_some": self.evidence.psi_mem_some, "psi_mem_full": self.evidence.psi_mem_full,
                         "psi_cpu_some": self.evidence.psi_cpu_some, "psi_io_some": self.evidence.psi_io_some,
                         "psi_io_full": self.evidence.psi_io_full},
            "storage_write": self.io.to_json(),
            "boot_defaults": self.defaults.as_ref().map(|d| json!({"clean": d.clean, "kernel": d.kernel, "keys": d.values.len()})),
        })
    }

    /// One line for the GUI / CLI header.
    pub fn summary(&self) -> String {
        let mut p = vec![if self.model.is_empty() { self.vendor.as_str().to_uppercase() } else { self.model.clone() }];
        p.push(format!("{}C/{}T", self.cores, self.logical));
        if self.x3d() {
            p.push(format!("{} CCDs, V-Cache on CCD{}", self.ccds.len(), self.cache_ccd.unwrap_or(0)));
        } else if self.multi_ccd() {
            p.push(format!("{} CCDs", self.ccds.len()));
        }
        if self.hybrid { p.push("hybrid P/E".into()); }
        p.push(format!("{} ({})", if self.driver.is_empty() { "no cpufreq" } else { &self.driver }, if self.epp { "EPP" } else { "no EPP" }));
        p.push(format!("{} GB RAM", self.ram_gb()));
        p.push(format!("swap: {}", self.swap.as_str()));
        p.push(self.io.summary());
        p.push(match &self.defaults {
            Some(d) if d.clean => "anchored at boot defaults".into(),
            Some(_) => "anchored at boot defaults (captured after TLP/LPM)".into(),
            None => "no boot-default snapshot (enable lpm-boot-guard)".into(),
        });
        let mut g = Vec::new();
        if self.nvidia_dgpu { g.push("NVIDIA"); }
        if self.amd_igpu { g.push("AMD"); }
        if self.intel_igpu { g.push("Intel"); }
        if !g.is_empty() { p.push(format!("GPU: {}", g.join(" + "))); }
        p.push(if self.battery { match self.on_ac { Some(true) => "laptop (on AC)", Some(false) => "laptop (on battery)", None => "laptop" } }
               else { "desktop (no battery)" }.into());
        p.push(format!("kernel {}.{}", self.kernel.0, self.kernel.1));
        p.join(" · ")
    }
}


/// Rows read live for rules that depend on the current state, for repairs
/// ([`audit`] on live values) and to skip writes that change nothing.
pub const LIVE_KEYS: &[&str] = &[
    "vm.min_free_kbytes", "cpu.ccd_park", "kernel.sched_schedstats", "wq.affinity_scope", "mm.lru_gen",
    "pm.nvme_latency_us", "mm.ksm_run", "cpu.smt", "thp.enabled", "thp.defrag", "thp.khp_max_ptes_none",
    "thp.khp_pages_to_scan", "thp.khp_scan_sleep_ms", "thp.mthp_16k", "thp.mthp_32k", "thp.mthp_64k",
    "thp.mthp_128k", "thp.mthp_256k", "thp.mthp_512k", "thp.mthp_1m", "vm.swappiness", "vm.page_cluster",
    "vm.watermark_scale_factor", "vm.watermark_boost_factor", "vm.compaction_proactiveness",
    "vm.zone_reclaim_mode", "vm.vfs_cache_pressure", "vm.max_map_count", "mm.lru_gen_min_ttl",
    "zswap.enabled", "vm.dirty_bytes", "vm.dirty_background_bytes", "kernel.watchdog",
    "blk.scheduler", "blk.wbt_lat_usec", "blk.read_ahead_kb", "blk.rq_affinity", "blk.nomerges", "blk.iostats",
    "blk.add_random", "blk.nr_requests",
];

const MTHP: [(&str, u32); 7] = [("thp.mthp_16k", 16), ("thp.mthp_32k", 32), ("thp.mthp_64k", 64), ("thp.mthp_128k", 128),
                                ("thp.mthp_256k", 256), ("thp.mthp_512k", 512), ("thp.mthp_1m", 1024)];

/// One rule outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision { pub key: &'static str, pub value: Value, pub why: String }

struct Rules<'a> {
    p: &'a Profile,
    g: Goal,
    w: Weights,
    out: Vec<Decision>,
    /// key -> [{value, U}] for the report / log.
    scores: Map<String, Value>,
    /// Messages of the joint pass (shown with the constraint notes).
    notes: Vec<String>,
    /// The signature's joint model for this goal's weights (None: no design rows yet).
    joint: Option<model::Joint>,
    jkeys: BTreeSet<String>,
    seen: RefCell<BTreeMap<String, bool>>,
    /// Keys no rule knows, decided from the measurements alone (signature pass).
    measured_only: BTreeSet<&'static str>,
}

fn vstr(v: &Value) -> String { match v { Value::String(s) => s.clone(), x => x.to_string() } }

impl<'a> Rules<'a> {
    fn set(&mut self, key: &'static str, value: impl Into<Value>, why: impl Into<String>) {
        self.out.retain(|d| d.key != key);
        self.out.push(Decision { key, value: value.into(), why: why.into() });
    }
    /// Like `set`, but skipped when the live value already matches.
    fn set_live(&mut self, key: &'static str, value: impl Into<Value>, why: impl Into<String>) {
        let value = value.into();
        if self.p.cur(key) == Some(vstr(&value).as_str()) { self.out.retain(|d| d.key != key); return; }
        self.set(key, value, why);
    }
    fn is(&self, g: Goal) -> bool { self.g == g }
    /// Decided by the goal itself, whatever a benchmark says: amd-pstate's per-core EPP boost
    /// is on for gaming and throughput. It acts on cores that stay more than half busy while
    /// the package is shared with a GPU - the 1 % lows of a real game, which no synthetic
    /// load of a few hundred milliseconds reproduces.
    fn pinned(&self, key: &str) -> bool { key == "cpu.epp_boost" && matches!(self.g, Goal::Gaming | Goal::Throughput) }
    /// Whether the benchmarks can see `key` on this machine at all (see SEEN_Z). Keys the
    /// joint model does not hold (one-at-a-time records only) count as seen, as before.
    fn seen(&self, key: &str) -> bool {
        if let Some(v) = self.seen.borrow().get(key) { return *v; }
        let v = match &self.joint { Some(j) if self.jkeys.contains(key) => seen_in(j, key), _ => true };
        self.seen.borrow_mut().insert(key.to_owned(), v);
        v
    }
    /// How this goal combines the calibration phases (load share, storage weight).
    fn blend(&self) -> calib::Blend { calib::Blend::new(self.g.key(), self.w.storage) }
    fn u(&self, c: &Cand) -> f64 { c.fx.u(&self.w) - MODESTY * c.dev }
    /// Index of the winning alternative, None = the reference stays.
    fn pick(&mut self, key: &str, reference: &Cand, alts: &[Cand]) -> Option<usize> {
        let base = self.u(reference);
        let mut trace = vec![json!({"value": if reference.value.is_null() { json!("(leave)") } else { reference.value.clone() }, "u": round2(base)})];
        let mut best: Option<(f64, usize)> = None;
        for (i, c) in alts.iter().enumerate() {
            let u = self.u(c);
            trace.push(json!({"value": c.value, "u": round2(u)}));
            if best.map_or(true, |(b, _)| u > b) { best = Some((u, i)); }
        }
        self.scores.insert(key.to_owned(), Value::Array(trace));
        best.filter(|(u, _)| u - base >= MARGIN).map(|(_, i)| i)
    }
    /// Scored knob: writes the winner, or the reference if it is a real value.
    /// Returns the value in effect afterwards (live value when left alone).
    /// Scored knob anchored at the boot default (when captured):
    /// * the boot value is the reference; an alternative equal to it lends it its effects;
    /// * distance from the anchor costs modesty (log2 for numbers), and numbers may
    ///   move at most 2x from it (x 2^(2*evidence) for evidence-driven knobs);
    /// * guard rollbacks add stability cost; two rollbacks retire the key's alternatives.
    fn choose(&mut self, key: &'static str, reference: Cand, alts: Vec<Cand>) -> Option<String> {
        let (reference, mut alts) = self.anchor(key, reference, alts);
        // Reference for the measurements: anchor, else live value, else what was live while calibrating.
        let ref_str = if !reference.value.is_null() { Some(vstr(&reference.value)) } else {
            self.p.cur(key).map(str::to_owned).or_else(|| self.p.calibration.as_ref().and_then(|c| c.values(key, "")).map(|(r, _)| r))
        };
        alts.retain(|c| !self.p.measured_unsafe(key, &vstr(&c.value)));
        // Measurements replace the estimates only where the benchmarks can see the knob.
        if let (Some(r), true) = (ref_str, self.seen(key)) { let b = self.blend(); for c in alts.iter_mut() { self.p.apply_measured(b, key, &r, &vstr(&c.value), c); } }
        self.choose_raw(key, reference, alts)
    }

    fn anchor(&self, key: &str, mut reference: Cand, mut alts: Vec<Cand>) -> (Cand, Vec<Cand>) {
        let learned = self.p.learned_risk(key);
        let Some(d0) = self.p.boot_default(key).map(str::to_owned) else {
            match learned { Some(l) => for c in alts.iter_mut() { c.fx.risk += l; }, None => alts.clear() }
            return (reference, alts);
        };
        let strength = if key == "vm.watermark_scale_factor" { self.p.evidence.reclaim_strength() } else { 0.0 };
        let max_dist = 1.0 + 2.0 * strength;
        let fx0 = alts.iter().find(|c| vstr(&c.value) == d0).map(|c| c.fx).unwrap_or_default();
        alts.retain(|c| vstr(&c.value) != d0);
        match learned { Some(l) => for c in alts.iter_mut() { c.fx.risk += l; }, None => alts.clear() }
        let cal = self.p.calibration.as_ref();
        let share = self.blend();
        let seen = |v: &str| cal.and_then(|c| c.get(key, &d0, v, share)).map_or(0.0, |m| m.n);
        alts.retain_mut(|c| match anchor_dist(&vstr(&c.value), &d0) {
            // Measured twice or more: one more doubling is allowed.
            Some(d) if d > max_dist && !(d <= max_dist + 1.0 && seen(&vstr(&c.value)) >= 2.0) => false,
            Some(d) => { c.dev += 0.1 * d; c.fx = sub(c.fx, fx0); true }
            None => { c.dev += 0.1; c.fx = sub(c.fx, fx0); true }
        });
        let v = d0.parse::<i64>().map(Value::from).unwrap_or_else(|_| json!(d0));
        reference = Cand { value: v, fx: Fx::default(), dev: 0.0, why: "Boot default (kernel + distro + your sysctl, before TLP/LPM).".into() };
        (reference, alts)
    }

    fn choose_raw(&mut self, key: &'static str, reference: Cand, alts: Vec<Cand>) -> Option<String> {
        match self.pick(key, &reference, &alts) {
            Some(i) => {
                let c = &alts[i];
                let why = format!("{} {}", c.why, c.fx.explain(&self.w, c.dev));
                let v = c.value.clone();
                self.set_live(key, v.clone(), why);
                Some(vstr(&v))
            }
            None if !reference.value.is_null() => {
                let v = reference.value.clone();
                self.set_live(key, v.clone(), format!("{} No candidate beats it by {MARGIN} for these weights.", reference.why));
                Some(vstr(&v))
            }
            None => self.p.cur(key).map(str::to_owned),
        }
    }
    /// When lpm-calibrate measured `key`, choose among the measured values by
    /// score (reference = boot/live value) instead of the structural rule.
    /// Numeric keys also get interpolated doses between measured values (see calib::curve).
    fn calibrated_choice(&mut self, key: &'static str) -> bool {
        let Some(cal) = self.p.calibration.as_ref() else { return false };
        let reference = self.p.boot_default(key).or_else(|| self.p.cur(key)).map(str::to_owned).unwrap_or_default();
        let Some((stored_ref, vals)) = cal.values(key, &reference) else { return false };
        let reference = if reference.is_empty() { stored_ref } else { reference };
        let share = self.blend();
        // Only values that were found unsafe are on record (nothing measured): the rule stands.
        if !vals.iter().any(|v| *v != reference && cal.get(key, &reference, v, share).is_some()) { return false; }
        let mut alts: Vec<Cand> = vals.iter().filter(|v| **v != reference).map(|v| {
            let val = v.parse::<i64>().map(Value::from).unwrap_or_else(|_| json!(v));
            cand(val, Fx::default(), 0.2, format!("{key} = {v}."))
        }).collect();
        if let Ok(r) = reference.parse::<i64>() {
            let mut pts: Vec<(i64, calib::Measured)> = vals.iter().filter_map(|v| {
                let n = v.parse::<i64>().ok()?;
                Some((n, if n == r { calib::Measured { lat: Some(0.0), thr: Some(0.0), pwr: Some(0.0), mem: Some(0.0), n: 99.0 } }
                         else { cal.get(key, &reference, v, share)? }))
            }).collect();
            if !pts.iter().any(|(n, _)| *n == r) { pts.push((r, calib::Measured { lat: Some(0.0), thr: Some(0.0), pwr: Some(0.0), mem: Some(0.0), n: 99.0 })); }
            if pts.len() >= 3 {
                for (v, m) in calib::curve(&pts) {
                    if vals.iter().any(|x| x.parse::<i64>().ok() == Some(v)) { continue; }
                    let mut c = cand(v, Fx::default(), 0.2, format!("{key} = {v} (dose interpolated between measured values)."));
                    overlay(&mut c.fx, m);
                    alts.push(c);
                }
            }
        }
        self.choose(key, leave(), alts);
        true
    }
    /// Value in effect after this rule set: decided, else live.
    fn eff(&self, key: &str) -> Option<String> {
        self.out.iter().find(|d| d.key == key).map(|d| vstr(&d.value)).or_else(|| self.p.cur(key).map(str::to_owned))
    }
}

fn round2(x: f64) -> f64 { (x * 100.0).round() / 100.0 }

/// At least one value of `key` moves the joint utility by SEEN_Z posterior standard deviations.
fn seen_in(j: &model::Joint, key: &str) -> bool {
    j.values(key).iter().any(|val| {
        let (m, var) = j.eval_diff(&[(key.to_owned(), val.clone())], &[]);
        var > 0.0 && m.abs() >= SEEN_Z * var.sqrt()
    })
}

/// log2 distance of two numeric values; None for text and for 0 against a non-zero value:
/// 0 switches the feature off (writeback throttling, boosted reclaim, background compaction,
/// APST, codec power-down), a mode change that costs like a choice. As a dose it sat ten or
/// more doublings away from any boot value, so the 2x trust region silently dropped every
/// "off" candidate whenever a boot snapshot existed.
fn anchor_dist(a: &str, b: &str) -> Option<f64> {
    let (x, y) = (a.parse::<f64>().ok()?, b.parse::<f64>().ok()?);
    if (x == 0.0) != (y == 0.0) { return None; }
    Some(((x.abs() + 1.0) / (y.abs() + 1.0)).log2().abs() + if x.signum() != y.signum() && x != 0.0 && y != 0.0 { 1.0 } else { 0.0 })
}

/// Measured effects replace the estimates in proportion to the evidence:
/// (n·measured + K·estimate) / (n + K), K = 1 run. One run halves the
/// estimate's say, five runs leave it a sixth.
fn overlay(f: &mut Fx, m: calib::Measured) {
    const K: f64 = 1.0;
    let n = m.n.max(0.0);
    let mix = |est: f64, meas: f64| (n * meas + K * est) / (n + K);
    if let Some(v) = m.lat { f.lat = mix(f.lat, v); }
    if let Some(v) = m.thr { f.thr = mix(f.thr, v); }
    if let Some(v) = m.pwr { f.pwr = mix(f.pwr, v); }
    if let Some(v) = m.mem { f.mem = mix(f.mem, v); }
}

fn sub(a: Fx, b: Fx) -> Fx { fx(a.lat - b.lat, a.thr - b.thr, a.pwr - b.pwr, a.mem - b.mem, a.risk - b.risk) }

/// Pure rule set with the goal's default weights. Keys that do not exist on this machine are filtered later.
pub fn decide(goal: Goal, p: &Profile) -> Vec<Decision> { decide_weighted(goal, p, Weights::for_goal(goal)).0 }

/// Decisions plus the score trace and the constraint notes.
pub fn decide_weighted(goal: Goal, p: &Profile, w: Weights) -> (Vec<Decision>, Map<String, Value>, Vec<String>) {
    let mut r = Rules { p, g: goal, w, out: Vec::new(), scores: Map::new(), notes: Vec::new(), joint: None, jkeys: BTreeSet::new(), seen: RefCell::new(BTreeMap::new()),
                          measured_only: BTreeSet::new() };
    r.joint = p.calibration.as_ref().and_then(|c| c.joint([w.latency, w.throughput, w.power, w.footprint], r.blend()));
    r.jkeys = r.joint.as_ref().map(|j| j.keys().into_iter().collect()).unwrap_or_default();
    cpu_rules(&mut r);
    memory_rules(&mut r);
    sched_rules(&mut r);
    io_rules(&mut r);
    device_rules(&mut r);
    signature_pass(&mut r);
    joint_pass(&mut r);
    repair_live(&mut r);
    let mut notes = std::mem::take(&mut r.notes);
    notes.extend(enforce(&mut r.out, p));
    (r.out, r.scores, notes)
}

// ── joint decision (interactions between knobs) ──────────────────────────────

impl<'a> Rules<'a> {
    /// Candidate values of a calibrated knob with their cost, index 0 = reference. Same
    /// constraints as `anchor`: boot-default trust region, unsafe values, retired keys, learned risk.
    fn level_costs(&self, key: &str, reference: &str, values: &[String]) -> Vec<(String, f64)> {
        let mut out = vec![(reference.to_owned(), 0.0)];
        let Some(learned) = self.p.learned_risk(key) else { return out };
        let d0 = self.p.boot_default(key).map(str::to_owned);
        let cal = self.p.calibration.as_ref();
        let share = self.blend();
        let strength = if key == "vm.watermark_scale_factor" { self.p.evidence.reclaim_strength() } else { 0.0 };
        let max_dist = 1.0 + 2.0 * strength;
        for v in values {
            if v == reference || self.p.measured_unsafe(key, v) { continue; }
            let mut dev = 0.2;
            if let Some(d0) = &d0 {
                let seen = cal.and_then(|c| c.get(key, d0, v, share)).map_or(0.0, |m| m.n);
                match anchor_dist(v, d0) {
                    Some(d) if d > max_dist && !(d <= max_dist + 1.0 && seen >= 2.0) => continue,
                    Some(d) => dev += 0.1 * d,
                    None => dev += 0.1,
                }
            }
            out.push((v.clone(), -MODESTY * dev + self.w.stability * learned));
        }
        out
    }
    /// Value in effect for a model knob that the rules already decided (context for the joint search).
    fn context_value(&self, key: &str) -> Option<String> {
        if key == "thp" {
            let m = self.eff("thp.enabled")?;
            if m != "always" && m != "madvise" { return None; }
            let mthp = ["thp.mthp_16k", "thp.mthp_32k", "thp.mthp_64k"].iter().any(|k| self.eff(k).map_or(false, |v| v != "never"));
            return Some(if mthp { format!("{m}+mthp") } else { m });
        }
        tune::find(key).and_then(|t| self.eff(t.key))
    }
}

/// The measured joint model decides the calibrated knobs together: it searches the best
/// combination for this goal's weights (posterior mean minus a risk term, minus modesty and
/// learned risk), then drops every change whose in-context gain does not clear the margin with
/// confidence. The rules' and the per-knob choices made before are the starting point (the
/// outcome is never worse than them under the model); THP and the dirty window, which are
/// not single knobs, and per-CCD role variants stay as fixed context.
fn joint_pass(r: &mut Rules) {
    let Some(cal) = r.p.calibration.as_ref() else { return };
    let Some(joint) = r.joint.take() else { return };
    let (mut keys, mut cands, mut fixed) = (Vec::new(), Vec::new(), Vec::new());
    for key in joint.keys() {
        let Some(reference) = cal.refs.get(&key).cloned() else { continue };
        let scoped = format!("{key}_");
        if tune::find(&key).is_none() || r.pinned(&key) || r.out.iter().any(|d| d.key.starts_with(&scoped)) {
            if let Some(v) = r.context_value(&key) { if v != reference { fixed.push((key.clone(), v)); } }
            continue;
        }
        if r.p.boot_default(&key).map_or(false, |d| d != reference) { continue; }
        let c = r.level_costs(&key, &reference, &model::with_mids(&reference, &joint.values(&key)));
        // A rule chose a value that was never measured: nothing to weigh it against, it stays.
        let ruled = r.out.iter().find(|d| d.key == key.as_str()).map(|d| vstr(&d.value));
        if ruled.map_or(false, |v| !c.iter().any(|(x, _)| *x == v)) { continue; }
        if c.len() > 1 { keys.push(key); cands.push(c); }
    }
    if keys.is_empty() { return; }
    let start: Vec<usize> = keys.iter().zip(&cands).map(|(k, c)| {
        r.out.iter().find(|d| d.key == k.as_str()).and_then(|d| c.iter().position(|(v, _)| *v == vstr(&d.value))).unwrap_or(0)
    }).collect();
    // A value a rule chose is prior knowledge (kernel documentation, this machine's evidence), not
    // a candidate that has to prove itself again: it pays neither margin nor modesty, and it goes
    // back to the reference only when the measurements put it below the reference with
    // confidence (KEEP_Z) - not when the benchmarks merely fail to see what it does. Learned
    // rollback risk still counts in full. A value picked from the measurements alone (no rule
    // behind it) has no such standing: it must pay in context like any other change.
    let ruled: Vec<bool> = keys.iter().zip(&start).map(|(k, s)| *s > 0 && !r.measured_only.contains(k.as_str())).collect();
    for (i, c) in cands.iter_mut().enumerate() {
        if !ruled[i] { continue; }
        let learned = r.w.stability * r.p.learned_risk(&keys[i]).unwrap_or(0.0);
        let sd = joint.eval_diff(&[(keys[i].clone(), c[start[i]].0.clone())], &[]).1.max(0.0).sqrt();
        c[start[i]].1 = learned + MARGIN + (KEEP_Z + model::RISK_Z) * sd;
    }
    let prob = model::Problem::new(&joint, keys.clone(), cands.clone(), fixed, model::RISK_Z, MARGIN, cal.unsafe_sets.clone());
    let mut rng = model::Rng::new(0x5EED);
    let mut sel = prob.optimize(&start, &mut rng, 8);
    if prob.score(&sel, 0.0) < prob.score(&start, 0.0) - 1e-9 { sel = start.clone(); }
    let dropped = prob.prune(&mut sel, model::RISK_Z, MARGIN);
    let cfg0 = prob.cfg(&start);
    let marg = prob.marginals(&sel);
    let mut kept = 0usize;
    for (i, key) in keys.iter().enumerate() {
        if sel[i] == start[i] { if ruled[i] { kept += 1; } continue; }
        let value = cands[i][sel[i]].0.clone();
        let why = if sel[i] == 0 && ruled[i] {
            let (m, v) = joint.eval_diff(&[(key.clone(), cands[i][start[i]].0.clone())], &[]);
            format!("Joint model: {} = {} measured {:+.3} (±{:.3}) against the reference on this machine - worse with confidence, so the rule's choice goes back to the reference.",
                    key, cands[i][start[i]].0, m, v.max(0.0).sqrt())
        } else if sel[i] == 0 {
            let d = dropped.iter().find(|d| d.0 == i);
            format!("Joint model: {} = {} looked good alone but next to the other chosen settings it adds {:+.3} (±{:.3}); back to the reference.",
                    key, cands[i][start[i]].0, d.map_or(0.0, |d| d.1), d.map_or(0.0, |d| d.2))
        } else {
            let m = marg.iter().find(|m| m.0 == i);
            let alone = joint.eval_diff(&[(key.clone(), value.clone())], &[]).0;
            format!("Joint model: {} = {}: {:+.3} alone, {:+.3} (±{:.3}) next to the other chosen settings.", key, value, alone, m.map_or(0.0, |m| m.1), m.map_or(0.0, |m| m.2))
        };
        let Some(t) = tune::find(key) else { continue };
        let v = value.parse::<i64>().map(Value::from).unwrap_or_else(|_| json!(value));
        r.set_live(t.key, v, why);
    }
    let cfg = prob.cfg(&sel);
    let (mu, var) = joint.eval(&cfg);
    let mu0 = joint.eval(&cfg0).0;
    r.notes.push(format!("Joint model: {} calibrated change(s) predicted {:+.3} ± {:.3} weighted gain for this goal (per-knob choices alone: {:+.3}).",
                         sel.iter().filter(|s| **s > 0).count(), mu, var.sqrt(), mu0));
    let blind = keys.iter().enumerate().filter(|(i, k)| ruled[*i] && sel[*i] == start[*i] && !seen_in(&joint, k)).count();
    if kept > 0 {
        r.notes.push(format!("Joint model: {kept} rule-based choice(s) kept{}; a rule's choice is dropped only when the measurements put it below the reference with confidence.",
                             if blind > 0 { format!(" ({blind} of them on knobs the benchmarks cannot see on this machine)") } else { String::new() }));
    }
}

// ── memory ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pace { Slow, Default, Fast }

/// Effects of one THP configuration relative to (madvise, no mTHP, max_ptes_none 511, default pace).
/// Memory costs come from two mechanisms the kernel docs describe: with
/// enabled=always a page fault in any 2 MB-aligned anonymous range may take a
/// whole huge page (RSS bloat of sparse heaps), and khugepaged fills up to
/// max_ptes_none empty slots per collapsed range. The deferred-split shrinker
/// only gives that back under memory pressure, so at idle it stays "used".
fn thp_fx(always: bool, mthp: bool, ptes: u32, pace: Pace) -> (Fx, f64) {
    let scope = if always { 1.0 } else { 0.25 };
    let pf = ptes as f64 / 511.0;
    let (mut f, mut dev) = (Fx::default(), 0.0);
    if always { f.thr += 0.20; f.lat += 0.05; f.mem -= 0.45; f.risk -= 0.05; dev += 0.5; }
    f.mem += 0.35 * scope * (1.0 - pf);
    f.thr -= 0.10 * scope * (1.0 - pf);
    dev += 0.1 * (1.0 - pf);
    if mthp { let g = if always { 1.0 } else { 0.4 }; f.thr += 0.06 * g; f.lat += 0.03 * g; f.mem -= 0.08 * g; dev += 0.3; }
    match pace {
        Pace::Fast => { f.thr += 0.05 * scope; f.mem -= 0.08 * scope * pf; f.pwr -= 0.05; f.lat -= 0.03; dev += 0.3; }
        Pace::Slow => { f.thr -= 0.03 * scope; f.mem += 0.04 * scope * pf; f.pwr += 0.04; f.lat += 0.01; dev += 0.15; }
        Pace::Default => {}
    }
    (f, dev)
}

type Combo = (bool, bool, u32, Pace);

/// Swaps the fault-time part of a THP combination (mode + mTHP) for the
/// calibrated one; khugepaged's slow effects (max_ptes_none, pace) stay modelled.
fn thp_measured(p: &Profile, b: calib::Blend, c: Combo, f: &mut Fx) {
    let Some(cal) = p.calibration.as_ref() else { return };
    let label = format!("{}{}", if c.0 { "always" } else { "madvise" }, if c.1 { "+mthp" } else { "" });
    let m = if label == "madvise" { calib::Measured { lat: Some(0.0), thr: Some(0.0), pwr: Some(0.0), mem: Some(0.0), n: 99.0 } }
            else { match cal.get("thp", "madvise", &label, b) { Some(m) => m, None => return } };
    let (em, _) = thp_fx(c.0, c.1, 511, Pace::Default);
    let mut model = *f;
    model.lat -= em.lat; model.thr -= em.thr; model.pwr -= em.pwr; model.mem -= em.mem;
    let mut meas = Fx::default();
    overlay(&mut meas, m);
    f.lat = model.lat + if m.lat.is_some() { meas.lat } else { em.lat };
    f.thr = model.thr + if m.thr.is_some() { meas.thr } else { em.thr };
    f.pwr = model.pwr + if m.pwr.is_some() { meas.pwr } else { em.pwr };
    f.mem = model.mem + if m.mem.is_some() { meas.mem } else { em.mem };
}

fn combo_dist(a: Combo, b: Combo) -> f64 {
    0.5 * (a.0 != b.0) as u8 as f64 + 0.3 * (a.1 != b.1) as u8 as f64
        + 0.2 * (a.2 as f64 - b.2 as f64).abs() / 511.0 + 0.15 * (a.3 != b.3) as u8 as f64
}

/// The boot snapshot's THP configuration, if it is one the search can express
/// and the kernel honours (mTHP on needs max_ptes_none 0 or 511).
fn thp_anchor(p: &Profile, mthp_ok: bool) -> Option<Combo> {
    let e = p.boot_default("thp.enabled")?;
    let always = match e { "always" => true, "madvise" => false, _ => return None };
    let ptes: u32 = p.boot_default("thp.khp_max_ptes_none")?.parse().ok().filter(|n| *n <= 511)?;
    let scan: u32 = p.boot_default("thp.khp_pages_to_scan").and_then(|v| v.parse().ok()).unwrap_or(4096);
    let sleep: u32 = p.boot_default("thp.khp_scan_sleep_ms").and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let pace = if scan <= 2048 || sleep >= 20_000 { Pace::Slow } else if scan >= 8192 || sleep <= 5000 { Pace::Fast } else { Pace::Default };
    let mthp = mthp_ok && MTHP.iter().filter(|(_, kb)| *kb <= 64).any(|(k, _)| p.boot_default(k).map_or(false, |v| v != "never"));
    if mthp && ptes != 0 && ptes != 511 { return None; }
    if ![511, 255, 64, 0].contains(&ptes) { return None; }
    Some((always, mthp, ptes, pace))
}

fn thp_rules(r: &mut Rules) {
    let p = r.p;
    // A transparent_hugepage= boot parameter is the user's own decision.
    let boot = p.boot_param("transparent_hugepage").map(str::to_owned);
    let mthp_ok = p.kernel_at_least(6, 8);
    // Reference: the boot-time THP configuration when it is valid, else the kernel default.
    let anchor = thp_anchor(p, mthp_ok).filter(|a| boot.as_deref().map_or(true, |b| (b == "always") == a.0));
    let refc: Combo = anchor.unwrap_or((boot.as_deref() == Some("always"), false, 511, Pace::Default));
    let label = |c: Combo| json!(format!("{}{}/{}/{:?}", if c.0 { "always" } else { "madvise" }, if c.1 { "+mthp" } else { "" }, c.2, c.3));
    let (mut rf, rd0) = thp_fx(refc.0, refc.1, refc.2, refc.3);
    let thp_seen = r.seen("thp");
    if thp_seen { thp_measured(p, r.blend(), refc, &mut rf); }
    let reference = Cand { value: label(refc), fx: rf, dev: if anchor.is_some() { 0.0 } else { rd0 }, why: String::new() };
    let keys = ["thp.enabled", "thp.khp_max_ptes_none", "thp.khp_pages_to_scan", "thp.khp_scan_sleep_ms", "thp.mthp_16k", "thp.mthp_32k", "thp.mthp_64k"];
    let learned: Option<f64> = keys.iter().map(|k| p.learned_risk(k)).sum();
    let mut alts = Vec::new();
    let mut combos = Vec::new();
    for always in [false, true] {
        if let Some(b) = boot.as_deref() { if (b == "always") != always { continue; } }
        for mthp in [false, true] {
            if mthp && !mthp_ok { continue; }
            // Kernel 7.x: mTHP collapse only honours max_ptes_none 0 or 511.
            let ptes: &[u32] = if mthp { &[511, 0] } else { &[511, 255, 64, 0] };
            for &pt in ptes {
                for pace in [Pace::Slow, Pace::Default, Pace::Fast] {
                    let c: Combo = (always, mthp, pt, pace);
                    if c == refc { continue; }
                    let Some(risk) = learned else { continue };
                    let (mut f, d) = thp_fx(always, mthp, pt, pace);
                    if p.measured_unsafe("thp", &format!("{}{}", if always { "always" } else { "madvise" }, if mthp { "+mthp" } else { "" })) { continue; }
                    if thp_seen { thp_measured(p, r.blend(), c, &mut f); }
                    f.risk += risk;
                    let dev = if anchor.is_some() { combo_dist(c, refc) } else { d };
                    alts.push(Cand { value: label(c), fx: f, dev, why: String::new() });
                    combos.push((always, mthp, pt, pace));
                }
            }
        }
    }
    let win = r.pick("thp", &reference, &alts);
    let (always, mthp, ptes, pace) = win.map(|i| combos[i]).unwrap_or(refc);
    let (f, d) = thp_fx(always, mthp, ptes, pace);
    let score = f.explain(&r.w, d);
    if boot.is_none() {
        r.set_live("thp.enabled", if always { "always" } else { "madvise" }, if always {
            format!("THP everywhere: fewer TLB misses for large heaps; costs RSS bloat (a touched 2 MB range takes a whole huge page, returned only under pressure). {score}")
        } else {
            format!("Huge pages only where a program asks (madvise): no silent RSS growth at idle; Proton/DXVK, JVMs and databases already opt in. {score}")
        });
    }
    r.set_live("thp.khp_max_ptes_none", ptes as i64, match ptes {
        511 => format!("Kernel default: khugepaged collapses opted-in ranges even when mostly empty (bounded here because THP is {}). {score}", if always { "on everywhere" } else { "madvise-only" }),
        0 => format!("khugepaged only collapses ranges that are fully populated: it never fills in memory a program did not touch. {score}"),
        n => format!("khugepaged accepts at most {n} empty slots of 512 per collapse: bounded fill-in, huge pages still form in dense ranges. {score}"),
    });
    let (scan, sleep) = match pace { Pace::Slow => (1024, 30_000), Pace::Default => (4096, 10_000), Pace::Fast => (8192, 10_000) };
    let pw = match pace {
        Pace::Slow => "khugepaged scans a quarter as much, every 30 s: less background CPU/mmap-lock work; huge pages form more slowly.",
        Pace::Default => "khugepaged at the kernel's pace (4096 pages every 10 s).",
        Pace::Fast => "khugepaged scans twice as much per pass: long jobs get huge pages sooner, at more background work and fill-in.",
    };
    r.set_live("thp.khp_pages_to_scan", scan, pw);
    r.set_live("thp.khp_scan_sleep_ms", sleep, pw);
    if mthp_ok {
        for (k, kb) in MTHP {
            let on = mthp && kb <= 64;
            // Only touch sizes that are on, or that must go off.
            if !on && p.cur(k).map_or(true, |v| v == "never") { continue; }
            r.set_live(k, if on { "inherit" } else { "never" }, if on {
                format!("{kb} KB folios follow THP ({}): fewer faults for mid-size allocations at a small memory cost. {score}", if always { "always" } else { "madvise" })
            } else { format!("{kb} KB folios off (kernel default): no partially used large folios.") });
        }
    }
    // Synchronous compaction on every fault stalls the faulting thread.
    if r.calibrated_choice("thp.defrag") {
    } else if p.cur("thp.defrag") == Some("always") {
        r.set("thp.defrag", "madvise", "defrag=always stalls every faulting thread on compaction; madvise (kernel default) limits that to opted-in ranges.");
    }
    if !matches!(p.swap, SwapKind::None) {
        r.choose("thp.khp_max_ptes_swap", leave(), vec![
            cand(0, fx(0.03, -0.01, 0.0, 0.0, 0.0), 0.2, "khugepaged only collapses fully resident ranges: no swap-in / decompression behind a running program."),
        ]);
    }
}

/// Dirty limits in seconds of writeback at the measured rate, bounded by RAM.
pub fn dirty_pair(rate_bps: u64, ram_bytes: u64, window_s: f64) -> (u64, u64) {
    let lo = 32 * MIB;
    let hi = (ram_bytes / 50).min(GIB).max(lo);
    let d = ((rate_bps as f64 * window_s) as u64).clamp(lo, hi) / MIB * MIB;
    let bg = (d / 4).max(8 * MIB).min(d / 2) / MIB * MIB;
    (bg, d)
}

fn dirty_rules(r: &mut Rules) {
    let p = r.p;
    let ram = p.ram_kb * 1024;
    let rate = p.io.bps.max(1);
    let frac = |t: f64| dirty_pair(rate, ram, t).1 as f64 / ram as f64;
    // Window -> effects. Longer: bursts absorbed at RAM speed and batched (power),
    // but a bigger backlog to flush (fsync / compositor stalls) and more data at risk.
    let mk = |t: f64, lat: f64, thr: f64, pwr: f64, risk: f64, dev: f64| -> Cand {
        cand(t, fx(lat, thr, pwr, -frac(t) * 2.0, risk), dev, String::new())
    };
    let reference = mk(1.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let mut alts = vec![
        mk(0.25, 0.15, -0.15, -0.05, 0.0, 0.4),
        mk(0.5, 0.10, -0.05, -0.02, 0.0, 0.2),
        mk(2.0, -0.15, 0.08, 0.10, -0.05, 0.3),
    ];
    // Windows clamped to the same limits (fast disk) are the reference itself, not a dose with an effect.
    let mut seen = vec![dirty_pair(rate, ram, 1.0)];
    alts.retain(|c| { let b = dirty_pair(rate, ram, c.value.as_f64().unwrap_or(1.0)); if seen.contains(&b) { false } else { seen.push(b); true } });
    for c in alts.iter_mut() {
        let t = format!("{}", c.value.as_f64().unwrap_or(1.0));
        if r.seen("vm.dirty") { p.apply_measured(r.blend(), "vm.dirty", "1", &t, c); }
    }
    let win = r.pick("vm.dirty_bytes", &reference, &alts);
    let (t, c) = match win { Some(i) => (alts[i].value.as_f64().unwrap_or(1.0), &alts[i]), None => (1.0, &reference) };
    let (bg, d) = dirty_pair(rate, ram, t);
    let score = format!("{}{}", c.fx.explain(&r.w, c.dev), c.why);
    let src = p.io.summary();
    r.set("vm.dirty_background_bytes", bg as i64,
          format!("Background writeback starts at {} MiB (~{:.2} s of writes at {src}). {score}", bg >> 20, bg as f64 / rate as f64));
    r.set("vm.dirty_bytes", d as i64,
          format!("Writers are throttled at {} MiB (~{:.2} s of writes, capped at 2% of RAM / 1 GiB): the backlog a flush or fsync() has to wait for stays short. {score}",
                  d >> 20, d as f64 / rate as f64));
}

fn memory_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    let ram = p.ram_kb * 1024;
    let gb = p.ram_gb();
    let ev = p.evidence.clone();
    thp_rules(r);
    dirty_rules(r);
    let thp_scope = if r.eff("thp.enabled").as_deref() == Some("always") { 1.0 } else { 0.3 };

    // watermark_scale_factor: distance between min/low/high watermarks (default
    // 0.1% of RAM). Kernel doc: raise it when allocstall / kswapd_low_wmark_hit_quickly
    // show kswapd starts too late. The benefit therefore scales with that evidence;
    // the cost is the RAM kswapd keeps free (it leaves MemAvailable).
    let e = ev.reclaim_strength();
    let def = (ram / 1000).max(1);
    let cap = (ram / 50).min(GIB);
    let span = ((512 * MIB) as f64 / def as f64).log2().max(1.0);
    let mut alts = Vec::new();
    for h in [128 * MIB, 256 * MIB, 512 * MIB] {
        if h > cap || h <= def * 2 { continue; }
        let f = wsf_for(h, ram);
        let g = ((h as f64 / def as f64).log2() / span).clamp(0.0, 1.0);
        alts.push(cand(f, fx((0.06 + 0.30 * e) * g, 0.02 * e * g, -0.02 * g, -(h as f64 / ram as f64) * 8.0, 0.0), 0.5 * g,
            format!("kswapd keeps ~{} MiB free (factor {f}) instead of 0.1% of RAM: fewer direct-reclaim stalls (reclaim evidence {:.0}%); that RAM leaves MemAvailable.", h >> 20, e * 100.0)));
    }
    r.choose("vm.watermark_scale_factor", leave(), alts);

    // watermark_boost_factor: after a fragmentation event kswapd reclaims up to
    // factor/10000 of the high watermark *extra* (default 15000 = 150%). It frees
    // page cache - it never holds memory. 0 = no such reclaim bursts, fewer free
    // pageblocks for huge pages. File refaults are the visible cost of the bursts.
    let refault = if ev.uptime_s >= 3600 { (ev.workingset_refault_file as f64 / ev.uptime_s as f64 / 2000.0).min(1.0) } else { 0.0 };
    r.choose("vm.watermark_boost_factor", leave(), vec![
        cand(0, fx(0.04 + 0.10 * refault, -0.05 * thp_scope, 0.01, 0.0, 0.0), 0.3,
             "No boosted reclaim after fragmentation events: page cache is not dropped in bursts; huge-page allocations find fewer free pageblocks."),
    ]);
    r.choose("vm.compaction_proactiveness", leave(), vec![
        cand(0, fx(0.04, -0.06 * thp_scope, 0.02, 0.0, 0.0), 0.3,
             "kcompactd does not compact in the background: no page-migration bursts; huge pages then rely on on-demand compaction."),
    ]);
    r.choose("vm.vfs_cache_pressure", leave(), vec![
        cand(50, fx(0.05, 0.02, 0.0, -0.03, 0.0), 0.3, "Dentry/inode cache kept twice as long: faster file dialogs, library and shader-cache scans; that metadata stays in RAM."),
    ]);
    // MGLRU min_ttl: thrash protection that turns memory pressure into an OOM kill.
    let tight = gb < 16 || ev.mem_pressure_now() || ev.reclaim_stalls();
    r.choose("mm.lru_gen_min_ttl", leave(), vec![
        cand(1000, fx(0.25, 0.0, 0.0, 0.0, if tight { -0.6 } else { -0.25 }), 0.4,
             "The last second's working set is never evicted: no thrashing stutter, but real pressure ends in an OOM kill instead of slowness."),
    ]);

    if p.cur("mm.lru_gen").map_or(false, |v| v != "7") { r.set("mm.lru_gen", 7, "All MGLRU features (kernel default)."); }
    if !r.is(Desktop) && p.cur("mm.ksm_run").map_or(false, |v| v != "0") { r.set("mm.ksm_run", 0, "Stop the KSM scanner: pure background cost without VMs."); }
    if matches!(r.g, Gaming | Desktop) && p.cur("vm.max_map_count").and_then(|v| v.parse::<u64>().ok()).map_or(true, |v| v < 1_048_576) {
        r.set("vm.max_map_count", 1_048_576, "Arch/Fedora value: some Proton games exceed the old 65530 mapping limit; only a per-process count, no memory is reserved.");
    }

    // Swap cost model (kernel doc: swappiness = relative I/O cost, > 100 for in-memory swap).
    match p.swap {
        SwapKind::Zram => {
            if !r.calibrated_choice("vm.swappiness") {
                r.set_live("vm.swappiness", 150, "zram swap costs a compression, not I/O: cold anonymous pages go there before hot page cache (kernel doc: > 100 for in-memory swap).");
            }
            if !r.calibrated_choice("vm.page_cluster") { r.set_live("vm.page_cluster", 0, "zram has no seek cost: no swap readahead."); }
            r.set_live("zswap.enabled", "0", "zram is the swap device: zswap in front of it would compress twice.");
        }
        SwapKind::None => {}
        k => {
            let hdd = k == SwapKind::Hdd;
            if !r.calibrated_choice("vm.swappiness") { r.set_live("vm.swappiness", if hdd { 60 } else { 100 }, if hdd { "Swap on a spinning disk: keep swap-outs rarer than cache drops (kernel default)." }
                       else { "zswap absorbs swap-outs in RAM: equal cost for anon and file pages." }); }
            if !r.calibrated_choice("vm.page_cluster") { r.set_live("vm.page_cluster", if hdd { 3 } else { 1 }, "Swap readahead sized for the device (8 pages on HDD, 2 on flash)."); }
            r.set_live("zswap.enabled", "1", "Disk swap present: zswap keeps most swapped pages compressed in RAM.");
            r.set_live("zswap.shrinker_enabled", "1", "Cold pool pages move on to disk proactively.");
            if r.is(Gaming) { r.set_live("zswap.compressor", "lz4", "lz4: fastest decompression when a swapped page is touched again."); }
        }
    }
    let _ = (ram, gb);
}

/// watermark_scale_factor giving about `headroom` bytes between watermarks.
pub fn wsf_for(headroom: u64, ram_bytes: u64) -> i64 {
    ((headroom as u128 * 10_000 / ram_bytes.max(1) as u128) as i64).clamp(10, 300)
}

// ── I/O and devices ──────────────────────────────────────────────────────────

fn io_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    // Measured on this machine: pick by score instead of by rule.
    if r.calibrated_choice("blk.scheduler") {
    } else if p.rotational {
        r.set_live("blk.scheduler", if matches!(r.g, Gaming | Desktop) { "bfq" } else { "mq-deadline" },
              if matches!(r.g, Gaming | Desktop) { "A spinning disk is present: bfq keeps interactive reads responsive under competing I/O." }
              else { "A spinning disk is present: mq-deadline bounds latency while merging for throughput." });
    } else {
        r.set_live("blk.scheduler", "none", "Flash only: no reordering, lowest per-request latency and CPU cost.");
    }
    if !r.calibrated_choice("blk.read_ahead_kb") {
        match r.g {
            Throughput => r.set("blk.read_ahead_kb", if p.rotational { 2048 } else { 512 }, "Larger read-ahead for sequential reads (sources, archives); costs page cache on random access."),
            Gaming => r.set("blk.read_ahead_kb", 256, "Games stream assets sequentially from large packs: 256 KiB read-ahead."),
            _ => {}
        }
    }
    r.choose("blk.wbt_lat_usec", leave(), vec![
        cand(0, fx(-0.15, 0.05, 0.0, 0.0, -0.05), 0.3, "No writeback throttling: writes flush at full device speed, but reads queue behind them."),
    ]);
    if matches!(r.g, Gaming | Desktop | Throughput) {
        r.set_live("net.tcp_congestion", "bbr", "BBR models bandwidth/RTT instead of reacting to loss: steadier latency on Wi-Fi and long routes.");
        r.set_live("net.default_qdisc", "fq", "fq pacing, the qdisc BBR was designed for.");
    }
    if p.wifi && !tune::wireless_pm_locked() {
        r.choose("net.wifi_power_save", leave(), vec![
            cand("0", fx(0.12, 0.0, -0.10, 0.0, 0.0), 0.2, "Radio never dozes between beacons: no 802.11 power-save ping spikes; ~0.5 W more."),
            cand("1", fx(-0.08, 0.0, 0.08, 0.0, 0.0), 0.2, "802.11 power save: radio dozes between beacons; ping jitter under light traffic."),
        ]);
    }
}

fn device_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    let force_aspm = p.boot_param("pcie_aspm") == Some("force");
    // pcie_aspm=force enables ASPM on links whose devices never advertised it:
    // every deeper link state is then a larger stability bet.
    if p.boot_param("pcie_aspm") != Some("off") {
        r.choose("pci.aspm", leave(), vec![
            cand("powersave", fx(-0.04, 0.0, 0.25, 0.0, if force_aspm { -0.20 } else { -0.08 }), 0.3,
                 "Idle PCIe links enter L0s/L1: less idle power; µs wake-up on each burst."),
            cand("performance", fx(0.06, 0.01, -0.20, 0.0, 0.0), 0.3, "PCIe links never drop to a power state: no wake-up jitter; more idle power and heat."),
        ]);
        r.choose("pci.aspm_links", leave(), vec![
            cand("l1ss", fx(-0.03, 0.0, 0.30, 0.0, if force_aspm { -0.45 } else { -0.30 }), 0.5,
                 "L1.1/L1.2 on every link: lowest idle power; some NVMe/Wi-Fi/GPU links misbehave or fail to resume."),
        ]);
    }
    r.choose("pm.pci_runtime", leave(), vec![
        cand("auto", fx(-0.03, 0.0, 0.30, 0.0, -0.10), 0.3, "Idle PCI devices drop to D3 (display devices and their bridges excluded); resume costs a few ms."),
        cand("on", fx(0.03, 0.0, -0.10, 0.0, 0.0), 0.2, "PCI devices stay in D0: no resume latency; more idle power."),
    ]);
    r.choose("pm.usb_runtime", leave(), vec![
        cand("auto", fx(-0.02, 0.0, 0.12, 0.0, -0.15), 0.3, "Idle USB devices suspend (HID/audio skipped); receivers and hubs behind them can drop input briefly."),
        cand("on", fx(0.02, 0.0, -0.05, 0.0, 0.0), 0.2, "Connected USB devices never suspend."),
    ]);
    // usbcore.autosuspend on the kernel command line is the user's own decision.
    if p.boot_param("usbcore.autosuspend").is_none() {
        r.choose("usb.autosuspend", leave(), vec![
            cand(2, fx(-0.02, 0.0, 0.08, 0.0, -0.15), 0.3, "Newly plugged USB devices suspend after 2 s idle."),
            cand(-1, fx(0.02, 0.0, -0.04, 0.0, 0.0), 0.2, "Newly plugged USB devices never autosuspend."),
        ]);
    }
    let hda = r.choose("snd.hda_power_save", leave(), vec![
        cand(1, fx(-0.04, 0.0, 0.10, 0.0, -0.02), 0.3, "Codec powers down after 1 s idle: lowest power; pops and a short delay on the next sound."),
        cand(10, fx(-0.01, 0.0, 0.08, 0.0, 0.0), 0.2, "Codec powers down after 10 s idle."),
        cand(0, fx(0.03, 0.0, -0.05, 0.0, 0.0), 0.2, "Codec always powered: no pop, no wake delay."),
    ]);
    if r.out.iter().any(|d| d.key == "snd.hda_power_save") {
        let on = hda.as_deref() != Some("0");
        r.set_live("snd.hda_power_save_controller", if on { "1" } else { "0" }, "Controller follows the codec's power saving.");
    }
    if p.nvme {
        r.choose("pm.nvme_latency_us", leave(), vec![
            cand(0, fx(0.03, 0.0, -0.25, 0.0, 0.0), 0.3, "APST off: an idle NVMe never needs to wake from a deep state; ~0.5-1 W more at idle."),
            cand(100_000, fx(-0.02, 0.0, 0.15, 0.0, -0.03), 0.3, "Every APST state allowed: deepest NVMe idle; wake cost mostly hidden by the page cache."),
        ]);
    }
    if p.battery {
        r.choose("pm.mem_sleep", leave(), vec![
            cand("deep", fx(0.0, 0.0, 0.20, 0.0, if force_aspm { -0.15 } else { -0.08 }), 0.3,
                 "S3 suspend where firmware offers it: lowest drain in the bag; resume paths are less tested than s2idle on new laptops."),
        ]);
    }
    match r.g {
        PowerSave => {
            r.set_live("net.wol", "0", "Wake-on-LAN off (TLP default): the wired NIC can power down fully in suspend.");
            r.set_live("gpu.amdgpu_abm", 3, "Panel power savings level 3 (TLP's battery level): lower backlight, compensated pixels.");
        }
        Desktop => r.set_live("net.wol", "0", "Wake-on-LAN off: the wired NIC can power down fully in suspend."),
        Gaming | Throughput => r.set_live("gpu.amdgpu_abm", 0, "Panel power savings off: no backlight/contrast modulation, accurate colour."),
    }
    if p.sata_hosts {
        match r.g {
            PowerSave => {
                for k in ["disk.apm_0", "disk.apm_1", "disk.apm_2", "disk.apm_3"] { r.set(k, 128, "APM 128 (TLP battery level): saving without spin-down on every start-stop cycle."); }
                r.set("pm.ahci_runtime_timeout", 15_000, "Idle ATA disks suspend after 15 s (TLP default).");
                r.set("pm.ahci_disk_runtime", "auto", "Idle ATA disks may suspend.");
            }
            Gaming | Throughput => {
                for k in ["disk.apm_0", "disk.apm_1", "disk.apm_2", "disk.apm_3"] { r.set(k, 254, "APM 254 (TLP AC level): no power-saving stalls."); }
                r.set("pm.ahci_disk_runtime", "on", "ATA disks never suspend: no spin-up delay mid-session.");
            }
            Desktop => {}
        }
        r.set("pm.sata_alpm", match r.g { Gaming | Throughput => "max_performance", _ => "med_power_with_dipm" },
              match r.g { Gaming | Throughput => "SATA link always active.", _ => "Modern default: partial/slumber with device-initiated PM." });
    }
    // iGPU: always driver-managed - forcing 'low' is unstable on amdgpu.
    if p.amd() && p.amd_igpu { r.set_live("gpu.amdgpu_dpm", "auto", "Driver-managed iGPU clocks ('low' is not stable on this iGPU)."); }
    if p.intel() && p.intel_igpu && (r.is(PowerSave) || (r.is(Gaming) && p.nvidia_dgpu)) {
        r.set("gpu.intel_slpc_profile", "power_saving", "iGPU clocks ramp gently: it only composites or idles here.");
    }
}

/// Every key the machine signature measured and no rule handled yet is decided
/// from the measurements. A key a structural rule decided keeps the rule's value
/// here: the joint pass weighs it against the model's posterior and takes it back
/// only on credible evidence (one-at-a-time records, which carry no posterior,
/// still replace it). A global key is left alone when a rule already set a scoped
/// variant of it (cpu.epp vs cpu.epp_ccd0: CCD roles stay structural), and so is
/// a key the goal pins (see `Rules::pinned`).
fn signature_pass(r: &mut Rules) {
    let Some(cal) = r.p.calibration.as_ref() else { return };
    let keys: Vec<&'static str> = cal.key_names().iter().filter_map(|k| tune::find(k).map(|t| t.key)).collect();
    for key in keys {
        if r.scores.contains_key(key) { continue; }
        if r.pinned(key) { continue; }
        let scoped = format!("{key}_");
        if r.out.iter().any(|d| d.key.starts_with(&scoped)) { continue; }
        let before: Vec<Decision> = r.out.iter().filter(|d| d.key == key).cloned().collect();
        if !before.is_empty() && r.jkeys.contains(key) { continue; }
        r.out.retain(|d| d.key != key);
        if r.calibrated_choice(key) { r.measured_only.insert(key); } else { r.out.extend(before); }
    }
}

/// Live values that fail [`audit`] get their fix, unless a rule already decided the key.
fn repair_live(r: &mut Rules) {
    let live: Map<String, Value> = r.p.current.iter().map(|(k, v)| {
        (k.clone(), v.parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::String(v.clone())))
    }).collect();
    let ctx = AuditCtx { ram_kb: r.p.ram_kb, cmdline: r.p.cmdline.clone(), numa_nodes: r.p.numa_nodes };
    for is in audit(&live, &ctx) {
        let (Some(fix), Some(t)) = (is.fix, tune::find(&is.key)) else { continue };
        if r.out.iter().any(|d| d.key == t.key) { continue; }
        r.set(t.key, fix, format!("Repair of the live value: {}", is.msg));
    }
}

// ── CPU and scheduler (structural rules) ─────────────────────────────────────

fn cpu_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    // Driver mode: EPP/CPPC (active) is the only mode where the per-core
    // hardware controller reacts within microseconds; every goal wants it.
    if p.amd() { r.set("cpu.pstate_status", "active", "amd-pstate active (EPP): CPPC firmware picks the clock per core in µs; every goal below is expressed through EPP."); }
    if p.intel() { r.set("cpu.intel_pstate_status", "active", "intel_pstate active (HWP): the core's own P-state logic, steered by EPP."); }

    // Dynamic EPP (kernel switches EPP with the power source) - only the
    // desktop goal hands EPP to the kernel; the others set it explicitly.
    if p.dynamic_epp {
        if r.is(Desktop) && p.battery {
            r.set("cpu.dynamic_epp", "enabled", "Laptop desktop use: let amd-pstate follow AC/battery on its own; EPP rows are left to it.");
        } else {
            r.set("cpu.dynamic_epp", "disabled", "This goal sets EPP explicitly; with dynamic EPP on the kernel would refuse or override it.");
        }
    }
    let desktop_dyn = r.is(Desktop) && p.battery && p.dynamic_epp;

    // EPP per goal. Under a laptop's package power limit, sustained all-core
    // throughput is power-bound, so balance_performance (better perf/W)
    // matches or beats performance there; without a battery it is not.
    let t_epp = if p.battery { "balance_performance" } else { "performance" };
    let (epp, why): (&str, String) = match r.g {
        PowerSave => ("power", "Most efficient EPP: lowest sustained clocks and ramp-up, biggest idle/light-load saving.".into()),
        Gaming => ("performance", "Game threads get the most aggressive boost and the fastest ramp after a stall.".into()),
        Throughput => (t_epp, if p.battery {
            "Laptop: sustained all-core load is package-power-bound, so the efficient boost curve of balance_performance gives the same or more work per second at lower heat.".into()
        } else { "No battery (no tight package limit): maximum boost for throughput.".into() }),
        Desktop => ("balance_performance", "Snappy boost for interactive bursts with much lower idle/light-load power than performance.".into()),
    };
    if p.epp && !desktop_dyn {
        if p.multi_ccd() {
            for ccd in 0..p.ccds.len().min(2) {
                let (gk, ek) = if ccd == 0 { ("cpu.governor_ccd0", "cpu.epp_ccd0") } else { ("cpu.governor_ccd1", "cpu.epp_ccd1") };
                r.set(gk, "powersave", "Governor 'powersave' hands the decision to EPP (performance would pin EPP to 0 and make the EPP row moot).");
                let (v, w) = if r.is(Gaming) && p.x3d() && Some(ccd) == p.freq_ccd {
                    ("balance_power", format!("CCD{ccd} is the frequency die: during a game it only runs IRQs, kernel work and background tasks, so it gets an efficient EPP and leaves package power to the V-Cache die."))
                } else if r.is(Gaming) && p.x3d() && Some(ccd) == p.cache_ccd {
                    ("performance", format!("CCD{ccd} carries the 3D V-Cache and hosts the game (see launch affinity): most aggressive boost."))
                } else { (epp, why.clone()) };
                r.set(ek, v, w);
            }
        } else {
            r.set("cpu.governor", "powersave", "Governor 'powersave' hands the decision to EPP.");
            r.set("cpu.epp", epp, why.clone());
        }
        if p.hybrid {
            let (pc, ec, w) = match r.g {
                PowerSave => ("balance_power", "power", "P-cores stay usable for bursts, E-cores run at the most efficient point."),
                Gaming => ("performance", "balance_power", "Game threads on the P-cores boost hardest; E-cores (background, IRQs) stay efficient and leave package power to the P-cores and the dGPU."),
                Throughput => (t_epp, t_epp, "Every core contributes to a parallel build; same EPP on both classes."),
                Desktop => ("balance_performance", "balance_performance", "Uniform, responsive EPP on both core classes."),
            };
            r.set("cpu.epp_pcore", pc, w);
            r.set("cpu.epp_ecore", ec, w);
        }
    } else if !p.epp && !p.governors.is_empty() {
        // Legacy cpufreq driver: pick a governor.
        let gov = match r.g {
            Throughput if !p.battery => "performance",
            _ if p.governors.iter().any(|g| g == "schedutil") => "schedutil",
            _ => "ondemand",
        };
        if p.governors.iter().any(|g| g == gov) {
            let key = if p.multi_ccd() { None } else { Some("cpu.governor") };
            if let Some(k) = key { r.set(k, gov, "No EPP on this driver: the governor decides the clock; schedutil follows scheduler load directly."); }
            else {
                r.set("cpu.governor_ccd0", gov, "No EPP on this driver: the governor decides the clock.");
                r.set("cpu.governor_ccd1", gov, "No EPP on this driver: the governor decides the clock.");
            }
        }
    }

    // EPP boost (patched amd-pstate): EPP reacts to load bursts faster -
    // the ramp after a stall is exactly what frame time and short compile
    // jobs pay for. Off only for power saving. The row is n/a (skipped) on
    // kernels without the patch.
    match r.g {
        PowerSave => r.set("cpu.epp_boost", "0", "No EPP boost: no short clock spikes on light loads."),
        Gaming => r.set("cpu.epp_boost", "1", "EPP boost: cores leave the efficient EPP point the moment a frame's work arrives - shorter ramp, steadier frame times."),
        Throughput => r.set("cpu.epp_boost", "1", "EPP boost: every job start (compiler, encoder chunk) runs at full clock immediately instead of after the EPP ramp."),
        Desktop => r.set("cpu.epp_boost", "1", "EPP boost: interactive bursts get full clock at once, idle stays at the efficient EPP."),
    }
    // X3D laptop gaming: the frequency die only runs IRQs / kernel work /
    // background tasks; capping it hands its share of the package limit to
    // the V-Cache die that runs the game.
    if r.is(Gaming) && p.x3d() && p.battery {
        if let Some(f) = p.freq_ccd.filter(|&f| f < 2) {
            if let Some(c) = p.ccds.iter().find(|c| c.index == f).filter(|c| c.max_khz > 0) {
                let cap = (c.max_khz * 7 / 10) as i64;
                r.set(if f == 0 { "cpu.max_freq_ccd0" } else { "cpu.max_freq_ccd1" }, cap,
                      format!("CCD{f} (frequency die) capped at {} MHz: it only serves IRQs and background work while the game runs on the V-Cache die, which gets the freed package power.", cap / 1000));
            }
        }
    }
    // X3D desktop: V-Cache die boosts (capped at 4.4 GHz - the cache, not
    // the clock, makes it snappy), frequency die runs without boost: less
    // heat and fan noise, lower power draw.
    if r.is(Desktop) && p.x3d() {
        if let (Some(c), Some(f)) = (p.cache_ccd.filter(|&c| c < 2), p.freq_ccd.filter(|&f| f < 2)) {
            let k = |i: usize, a: &'static str, b: &'static str| if i == 0 { a } else { b };
            r.set(k(c, "cpu.boost_ccd0", "cpu.boost_ccd1"), "1", format!("CCD{c} (V-Cache) keeps boost: interactive bursts stay snappy."));
            r.set(k(c, "cpu.max_freq_ccd0", "cpu.max_freq_ccd1"), 4_400_000, format!("CCD{c} capped at 4.4 GHz: the 3D V-Cache carries desktop responsiveness; the top bins only add heat and fan noise."));
            r.set(k(f, "cpu.boost_ccd0", "cpu.boost_ccd1"), "0", format!("CCD{f} (frequency die) without boost: background work at base clock, less heat and power."));
        }
        if p.x3d_driver { r.set("cpu.x3d_mode", "cache", "New threads prefer the boosted V-Cache die."); }
    }
    // schedutil only: how soon the clock follows a load change.
    if p.schedutil() {
        r.set("cpu.schedutil_rate_limit_us", match r.g { PowerSave => 10_000, Throughput => 2_000, _ => 500 },
              match r.g { PowerSave => "Fewer frequency changes: the clock does not chase every short burst.",
                          Throughput => "Moderate: sustained load, few changes needed.",
                          _ => "Clock follows a wake-up burst within 0.5 ms." });
    }

    // Floor frequency (kernel 7.1+, CPPC Performance Priority): what firmware
    // throttles to first under a power/thermal limit. Absent on most CPUs (row
    // is then filtered). Nominal is the kernel default and the right sustained
    // floor for anything that wants performance; power saving lets it fall.
    if p.amd() {
        if r.is(PowerSave) {
            r.set("cpu.floor_freq", "cpuinfo_min", "Under a power or thermal limit firmware may throttle all the way to the hardware minimum: lowest package power, and there is no sustained-performance goal to protect.");
        } else {
            r.set("cpu.floor_freq", "nominal", "Under a power or thermal limit firmware sheds boost first and holds the nominal clock (kernel default): sustained games and builds keep their base performance instead of collapsing to the idle floor.");
        }
    }

    // Turbo: the single biggest power lever; everything but power saving wants it.
    match r.g {
        PowerSave => r.set("cpu.boost", "0", "Turbo off removes the least efficient (highest-voltage) bins - the largest single power/heat saving; base clock remains."),
        _ => r.set("cpu.boost", "1", "Turbo on: full single- and multi-thread clock range."),
    }

    // Idle floor: below amd_pstate_lowest_nonlinear_freq the CPU is less
    // efficient per unit of work; power saving still wants the hardware minimum
    // for trickle loads, where absolute power matters more than efficiency.
    r.set("cpu.min_freq", if r.is(PowerSave) { "cpuinfo_min" } else { "lowest_nonlinear" },
          if r.is(PowerSave) { "Hardware minimum floor: lowest absolute power for trickle loads." }
          else { "Efficient floor: frequencies below lowest_nonlinear cost wake-up latency for almost no saving." });

    if p.intel() {
        match r.g {
            PowerSave => { r.set("cpu.hwp_dynamic_boost", "0", "No I/O-wait boost: fewer short clock spikes."); r.set("cpu.epb", 12, "EPB towards power saving (uncore/package decisions)."); }
            Gaming => { r.set("cpu.hwp_dynamic_boost", "1", "Threads waking from I/O (asset streaming, shader compiles) start at a high clock."); r.set("cpu.epb", 4, "EPB balance-performance."); }
            Throughput => r.set("cpu.epb", 4, "EPB balance-performance."),
            Desktop => { r.set("cpu.hwp_dynamic_boost", "1", "Faster ramp after I/O waits in interactive apps."); r.set("cpu.epb", 6, "EPB normal (stock)."); }
        }
        if let Some((lo, hi)) = p.uncore {
            match r.g {
                Gaming => r.set("cpu.uncore_min_khz", hi as i64, "Uncore floor raised to its maximum: no ring/L3/memory ramp-up after idle - steadier frame times."),
                PowerSave => r.set("cpu.uncore_max_khz", (lo + (hi - lo) * 2 / 5) as i64, "Uncore capped at 40% of its range: several watts less package power; costs memory latency, irrelevant on battery."),
                _ => {}
            }
        }
    }

    // X3D scheduler preference.
    if p.x3d() && p.x3d_driver {
        match r.g {
            Gaming => r.set("cpu.x3d_mode", "cache", "Games have large working sets: new threads prefer the V-Cache die."),
            Throughput => r.set("cpu.x3d_mode", "frequency", "Parallel compute (builds, encoders) benefits more from the higher-clocked die first."),
            _ => {}
        }
    }

    // cpuidle: teo matches modern short C-state tables better than menu.
    if p.idle_governors.iter().any(|g| g == "teo") && !r.is(Throughput) {
        r.set("cpu.idle_governor", "teo", "teo (timer events oriented) picks the right C-state more often on CPUs with few states: fewer too-deep entries under light load, fewer too-shallow ones at idle.");
    }
    r.set("cpu.cstate_max", "all", "All C-states enabled: sleeping cores give their share of the power budget to the busy ones (boost headroom).");

    // Wake latency. On a laptop deep idle *is* boost headroom, so no cap; on a
    // desktop gaming box the deepest state's exit latency can be traded away.
    let deep = p.cstates.iter().filter(|c| c.name != "POLL").map(|c| c.latency_us).max().unwrap_or(0);
    let second = p.cstates.iter().filter(|c| c.name != "POLL" && c.latency_us < deep).map(|c| c.latency_us).max();
    match (r.g, second) {
        (Gaming, Some(s)) if !p.battery && deep >= 50 => r.set("cpu.wake_latency_us", s as i64,
            format!("Desktop (no battery): cores may use every C-state that wakes within {s} µs, the deepest ({deep} µs) is skipped - lower worst-case wake jitter.")),
        _ => r.set("cpu.wake_latency_us", 0, if p.battery && r.is(Gaming) {
            "Laptop: no wake-latency cap - capping deep idle would steal boost headroom from the game's cores and add heat.".to_string()
        } else { "No wake-latency constraint (kernel default).".to_string() }),
    }

    // Hot-plug: autotune never parks a CCD; it brings a parked one back.
    if p.cur("cpu.ccd_park").map_or(false, |v| v != "none") {
        r.set("cpu.ccd_park", "none", "A CCD is parked: all CPUs back online (autotune isolates with affinity/steering instead; parking breaks Wine/Proton CPU numbering and nvidia-powerd).");
    }
    if r.is(Throughput) && p.cur("cpu.smt").map_or(false, |v| v == "off") {
        r.set("cpu.smt", "on", "SMT on: +20-30% throughput for parallel builds.");
    }
}

fn sched_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    if matches!(r.g, Gaming | Desktop | Throughput) {
        r.set("kernel.split_lock_mitigate", 0, "No 1000x throttle for split-lock accesses (some Windows games and emulators trigger it).");
    }
    // kernel.watchdog is never turned off: without the lockup detector a hang
    // leaves no trace in the log (the Health tab and post-mortems go blind),
    // for a saving of a few timer interrupts per second.
    if p.numa_nodes <= 1 { r.set("kernel.numa_balancing", 0, "Single NUMA node: balancing would only sample page faults for nothing."); }
    if r.is(Gaming) && !p.battery {
        r.set("kernel.timer_migration", 0, "Desktop (no battery): timers fire on the CPU that armed them - no cross-CPU timer jitter.");
    } else {
        r.set("kernel.timer_migration", 1, "Timers of idle CPUs move to awake ones: idle cores stay in deep C-states (their headroom feeds boost).");
    }
    if matches!(r.g, Gaming | Desktop) { r.set("kernel.sched_autogroup", 1, "Per-session scheduling groups: a background build cannot starve the desktop/game."); }
    if p.cur("kernel.sched_schedstats") == Some("1") && r.g != Desktop {
        r.set("kernel.sched_schedstats", "0", "Schedstats were left on by some tool: per-switch accounting cost removed.");
    }
    // Scheduler feature bits (debugfs). NEXT_BUDDY: the kernel's stated rationale is that
    // waker and wakee share cache-hot data, so the wakee runs next (game main <-> render /
    // audio thread hand-offs); it became the default in the scheduler tree in Nov 2025, so
    // on newer kernels this only pins the value. RUN_TO_PARITY (default on) is kept for
    // goals that value fewer preemptions.
    match r.g {
        Gaming | Desktop => r.set("sched.feat_next_buddy", "1", "Producer/consumer thread hand-offs (game <-> render/audio, compositor <-> client) run back-to-back on warm caches; the kernel commit enabling it gives exactly this reason. Not measured on X3D by this tool."),
        Throughput | PowerSave => r.set("sched.feat_run_to_parity", "1", "A running task is not preempted by wakeups before its slice or lag point: fewer context switches, more work per slice (kernel default, pinned in case a tool turned it off)."),
    }
    r.set("sched.itmt", "1", "Preferred cores first: light loads run on the best-binned cores.");

    // Preemption and EEVDF slice. lazy (6.13+) = full's latency, voluntary's throughput.
    let lazy = p.kernel_at_least(6, 13);
    let (pre, pw) = match r.g {
        Gaming => ("full", "Full preemption: a woken game/audio/input thread runs almost immediately."),
        Desktop => (if lazy { "lazy" } else { "full" }, "Lazy preemption keeps full's wake-up latency for RT/interactive work with fewer forced switches."),
        Throughput | PowerSave => (if lazy { "lazy" } else { "voluntary" }, "Fewer involuntary context switches: more work per slice."),
    };
    if !r.calibrated_choice("sched.preempt") { r.set("sched.preempt", pre, pw); }
    // EEVDF base slice stays at the kernel default: EEVDF already gives
    // latency-sensitive tasks earlier deadlines, and no measured source backs
    // a fixed shorter/longer global slice for these goals.
    if r.is(Throughput) {
        r.set("sched.migration_cost_ns", 5_000_000, "Tasks count as cache-hot for 5 ms (TuneD throughput value): fewer cache-destroying migrations.");
    }
    if p.cur("wq.affinity_scope").map_or(false, |v| v != "cache") {
        r.set("wq.affinity_scope", "cache", "Unbound kernel work stays inside the L3 domain that queued it (kernel default).");
    }
    // Steering: keep IRQs and unbound kernel work off the game's cores.
    if r.is(Gaming) {
        if p.x3d() {
            let f = format!("ccd{}", p.freq_ccd.unwrap_or(1));
            r.set("wq.cpumask", f.clone(), "Unbound kernel work (writeback, crypto, fs) runs on the frequency die, not beside the game.");
            r.set("irq.affinity", f, "Device interrupts land on the frequency die, not on the game's cores.");
        } else if p.hybrid {
            r.set("wq.cpumask", "ecore", "Unbound kernel work on the E-cores, P-cores stay free for the game.");
            r.set("irq.affinity", "ecore", "Device interrupts on the E-cores.");
        }
    } else if r.is(Throughput) && (p.multi_ccd() || p.hybrid) {
        r.set("wq.cpumask", "all", "Every CPU may run kernel work.");
        r.set("irq.affinity", "all", "Interrupts spread over every CPU.");
    }
    // sched_ext is not chosen automatically: scx_lavd is still in development
    // and has open reports of large fps regressions on some CPUs, so it stays a
    // per-game, user-tested choice (Scheduler tab).
    // RT boost only matters where schedutil sets the clock.
    if p.schedutil() {
        match r.g {
            PowerSave => r.set("kernel.sched_util_clamp_min_rt_default", 0, "RT threads (audio, IRQ threads) no longer force the maximum frequency."),
            Desktop => r.set("kernel.sched_util_clamp_min_rt_default", 256, "RT threads get a moderate boost instead of max clock on every wake-up."),
            _ => r.set("kernel.sched_util_clamp_min_rt_default", 1024, "RT threads run at max clock (stock)."),
        }
    }
    match r.g {
        PowerSave => r.set("kernel.sched_energy_aware", "1", "Energy-model placement (only where EAS can run)."),
        Gaming | Throughput => r.set("kernel.sched_energy_aware", "0", "Classic load balancing: spread for performance instead of packing on efficient cores."),
        Desktop => {}
    }
}

fn run_block(goal: Goal, p: &Profile) -> (Value, Option<String>) {
    match goal {
        Goal::Gaming if p.x3d() => {
            let c = p.cache_ccd.unwrap_or(0);
            (json!({"nice": -5, "autogroup": true, "affinity": format!("ccd{c}")}),
             Some(format!("Game pinned to CCD{c} (V-Cache), nice -5 for the game's autogroup.")))
        }
        Goal::Gaming => (json!({"nice": -5, "autogroup": true, "affinity": "none"}), Some("nice -5 for the game's autogroup; the scheduler places threads.".into())),
        _ => (json!({"nice": 0, "autogroup": true, "affinity": "none"}), None),
    }
}


// ── hard constraints ─────────────────────────────────────────────────────────

/// Invariants no weight can override. Fixes the decision list in place and
/// returns one note per intervention.
pub fn enforce(out: &mut Vec<Decision>, p: &Profile) -> Vec<String> {
    let mut notes = Vec::new();
    let get = |out: &Vec<Decision>, k: &str| out.iter().find(|d| d.key == k).map(|d| vstr(&d.value));
    let eff = |out: &Vec<Decision>, k: &str| get(out, k).or_else(|| p.cur(k).map(str::to_owned));
    let num = |s: Option<String>| s.and_then(|v| v.parse::<i64>().ok());

    // 1. Dirty pair: background below the throttle point, both above the floors.
    if let (Some(bg), Some(d)) = (num(get(out, "vm.dirty_background_bytes")), num(get(out, "vm.dirty_bytes"))) {
        let d2 = d.max(32 * MIB as i64);
        let bg2 = bg.clamp(8 * MIB as i64, d2 / 2);
        if (bg2, d2) != (bg, d) {
            notes.push(format!("dirty limits adjusted to bg {} / {} bytes (bg < dirty, floors 8/32 MiB)", bg2, d2));
            for x in out.iter_mut() {
                if x.key == "vm.dirty_bytes" { x.value = json!(d2); }
                if x.key == "vm.dirty_background_bytes" { x.value = json!(bg2); }
            }
        }
    }
    // 2. mTHP collapse honours max_ptes_none only at 0 or 511 (kernel 7.x warns and falls back).
    let mthp_on = MTHP.iter().any(|(k, _)| eff(out, k).map_or(false, |v| v != "never"));
    if mthp_on {
        if let Some(n) = num(eff(out, "thp.khp_max_ptes_none")).filter(|n| *n != 0 && *n != 511) {
            out.retain(|d| d.key != "thp.khp_max_ptes_none");
            out.push(Decision { key: "thp.khp_max_ptes_none", value: json!(0),
                why: format!("mTHP is enabled: the kernel only supports max_ptes_none 0 or 511 for mTHP collapse (live {n} would be ignored with a warning); 0 = no fill-in.") });
            notes.push("max_ptes_none forced to 0 (mTHP on)".into());
        }
    }
    // 3. THP everywhere needs at least one defragmentation path.
    if eff(out, "thp.enabled").as_deref() == Some("always")
        && num(eff(out, "vm.watermark_boost_factor")) == Some(0) && num(eff(out, "vm.compaction_proactiveness")) == Some(0) {
        out.retain(|d| d.key != "vm.compaction_proactiveness");
        if p.cur("vm.compaction_proactiveness") == Some("0") {
            out.push(Decision { key: "vm.compaction_proactiveness", value: json!(20), why: "THP is on everywhere and boost reclaim is off: proactive compaction (kernel default 20) is the only path left that keeps huge pages available.".into() });
        }
        notes.push("proactive compaction kept on (THP always + boost 0)".into());
    }
    // 4. EPP is meaningless (pinned) under the performance governor.
    for (g, e) in [("cpu.governor", "cpu.epp"), ("cpu.governor_ccd0", "cpu.epp_ccd0"), ("cpu.governor_ccd1", "cpu.epp_ccd1")] {
        if get(out, g).as_deref() == Some("performance") && out.iter().any(|d| d.key == e) {
            out.retain(|d| d.key != e);
            notes.push(format!("{e} dropped: governor performance pins EPP"));
        }
    }
    // 5. Boot parameters the user chose.
    if p.boot_param("usbcore.autosuspend").is_some() && out.iter().any(|d| d.key == "usb.autosuspend") {
        out.retain(|d| d.key != "usb.autosuspend");
        notes.push("usb.autosuspend left to the usbcore.autosuspend boot parameter".into());
    }
    out.retain(|d| !(d.key == "kernel.watchdog" && vstr(&d.value) == "0"));
    // 6. Integers must survive every JSON layer exactly (GUI numbers are doubles).
    out.retain(|d| match d.value.as_i64() {
        Some(n) if n.abs() > JSON_SAFE_INT => { notes.push(format!("{} dropped: {n} exceeds 2^53", d.key)); false }
        _ => true,
    });
    notes
}

// ── audit (old presets / scenes / live values) ──────────────────────────────

pub struct AuditCtx { pub ram_kb: u64, pub cmdline: String, pub numa_nodes: usize }

impl AuditCtx {
    pub fn live() -> AuditCtx {
        let ram_kb = std::fs::read_to_string("/proc/meminfo").unwrap_or_default().lines()
            .find_map(|l| l.strip_prefix("MemTotal:").and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())).unwrap_or(0);
        let numa_nodes = rd("/sys/devices/system/node/possible").map(|s| tune::cpu_list(&s).len()).unwrap_or(1).max(1);
        AuditCtx { ram_kb, cmdline: rd("/proc/cmdline").unwrap_or_default(), numa_nodes }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue { pub key: String, pub reject: bool, pub msg: String, pub fix: Option<Value> }

impl Issue {
    pub fn to_json(&self) -> Value { json!({"key": self.key, "reject": self.reject, "message": self.msg, "fix": self.fix}) }
}

/// Checks a value map (a preset, a scene's tuning block, or live values)
/// against the safe envelope. `reject` = must never be written; the rest are
/// warnings with a suggested fix. Values the map does not contain are not judged.
pub fn audit(values: &Map<String, Value>, ctx: &AuditCtx) -> Vec<Issue> {
    let ram_b = ctx.ram_kb.saturating_mul(1024);
    let n = |k: &str| values.get(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())));
    let s = |k: &str| values.get(k).map(vstr);
    let mut out = Vec::new();
    let mut add = |key: &str, reject: bool, msg: String, fix: Option<Value>| out.push(Issue { key: key.into(), reject, msg, fix });

    let bg = n("vm.dirty_background_bytes");
    let d = n("vm.dirty_bytes");
    if let Some(b) = bg {
        if b < MIB as i64 { add("vm.dirty_background_bytes", true, format!("{b} bytes: background writeback would start after a few pages (int32-wrapped value from an older LPM?)"), None); }
        else if ram_b > 0 && b as u64 > ram_b / 10 { add("vm.dirty_background_bytes", false, format!("{} MiB is more than 10% of RAM", b >> 20), None); }
    }
    if let Some(x) = d {
        if x < 4 * MIB as i64 { add("vm.dirty_bytes", true, format!("{x} bytes: every buffered writer would be throttled almost immediately (system-wide stalls)"), None); }
        else if x < 32 * MIB as i64 { add("vm.dirty_bytes", false, format!("{} MiB throttles writers very early", x >> 20), None); }
        else if ram_b > 0 && x as u64 > ram_b / 5 { add("vm.dirty_bytes", false, format!("{} MiB is more than 20% of RAM: multi-second flush stalls", x >> 20), None); }
    }
    if let (Some(b), Some(x)) = (bg, d) {
        if x <= b { add("vm.dirty_bytes", true, format!("dirty_bytes {x} <= dirty_background_bytes {b}"), None); }
    }
    let mthp_on = MTHP.iter().any(|(k, _)| s(k).map_or(false, |v| v != "never"));
    if let Some(pn) = n("thp.khp_max_ptes_none") {
        if pn != 0 && pn != 511 && mthp_on {
            add("thp.khp_max_ptes_none", false, format!("{pn} with mTHP on: the kernel ignores it for mTHP collapse (only 0 or 511)"), Some(json!(0)));
        }
    }
    if let Some(v) = n("thp.khp_pages_to_scan").filter(|v| *v > 8192) { add("thp.khp_pages_to_scan", false, format!("{v} pages per pass (default 4096): heavy background collapsing and RSS growth"), Some(json!(4096))); }
    if let Some(v) = n("thp.khp_scan_sleep_ms").filter(|v| *v < 5000) { add("thp.khp_scan_sleep_ms", false, format!("{v} ms between passes (default 10000)"), Some(json!(10_000))); }
    if let Some(v) = n("vm.watermark_scale_factor") {
        if v > 1000 { add("vm.watermark_scale_factor", true, format!("{v}: kswapd would keep {}% of RAM free", v / 100), Some(json!(10))); }
        else if v > 300 { add("vm.watermark_scale_factor", false, format!("{v}: {:.1}% of RAM held free by kswapd", v as f64 / 100.0), Some(json!(10))); }
    }
    if let Some(v) = n("vm.watermark_boost_factor").filter(|v| *v > 15_000) {
        add("vm.watermark_boost_factor", false, format!("{v}: boosted reclaim above the kernel default (15000)"), Some(json!(15_000)));
    }
    if let Some(v) = n("vm.min_free_kbytes") {
        let kb = ctx.ram_kb as i64;
        let fix = json!((kb / 400).clamp(16_384, 131_072));
        if kb > 0 && v > kb * 3 / 100 { add("vm.min_free_kbytes", true, format!("{v} KiB is over 3% of RAM (kernel doc: too high OOMs the machine)"), Some(fix)); }
        else if kb > 0 && (v > kb / 100 || v > 262_144) { add("vm.min_free_kbytes", false, format!("{v} KiB reserve is above 1% of RAM / 256 MiB"), Some(fix)); }
    }
    if let Some(v) = n("mm.lru_gen_min_ttl") {
        if v > 5000 { add("mm.lru_gen_min_ttl", false, format!("{v} ms of protected working set: pressure ends in OOM kills"), Some(json!(0))); }
        else if v > 0 && ctx.ram_kb > 0 && ctx.ram_kb < 12 << 20 { add("mm.lru_gen_min_ttl", false, "working-set protection on < 12 GB RAM trades stutter for OOM kills".into(), Some(json!(0))); }
    }
    if let Some(v) = n("vm.vfs_cache_pressure") {
        if v < 10 { add("vm.vfs_cache_pressure", true, format!("{v}: dentry/inode caches practically never reclaimed (kernel doc: OOM risk)"), Some(json!(100))); }
    }
    if let Some(v) = n("vm.zone_reclaim_mode").filter(|v| *v != 0 && ctx.numa_nodes <= 1) {
        add("vm.zone_reclaim_mode", false, format!("{v} on a single-node machine only adds reclaim"), Some(json!(0)));
    }
    if values.contains_key("usb.autosuspend") && ctx.cmdline.split_whitespace().any(|w| w.starts_with("usbcore.autosuspend=")) {
        add("usb.autosuspend", false, "overrides the usbcore.autosuspend boot parameter".into(), None);
    }
    if s("kernel.watchdog").as_deref() == Some("0") {
        add("kernel.watchdog", false, "lockup detector off: hangs leave no trace in the log".into(), None);
    }
    out
}

/// Walks any JSON (scene file, preset, preset store) and audits every object
/// that holds tunable keys. Returns (json-path, issue).
pub fn audit_tree(v: &Value, ctx: &AuditCtx) -> Vec<(String, Issue)> {
    fn walk(v: &Value, path: String, ctx: &AuditCtx, out: &mut Vec<(String, Issue)>) {
        match v {
            Value::Object(m) => {
                if m.keys().any(|k| tune::find(k).is_some()) {
                    for i in audit(m, ctx) { out.push((path.clone(), i)); }
                }
                for (k, x) in m { walk(x, format!("{path}/{k}"), ctx, out); }
            }
            Value::Array(a) => for (i, x) in a.iter().enumerate() { walk(x, format!("{path}/{i}"), ctx, out); },
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, String::new(), ctx, &mut out);
    out
}

/// Repairs every audited object in place: rejected dirty pairs are replaced
/// by the pair derived for this machine, other issues take their fix (or the
/// key is dropped when there is none and it was rejected). Returns the changes.
pub fn repair_tree(v: &mut Value, ctx: &AuditCtx, p: &Profile) -> Vec<String> {
    fn walk(v: &mut Value, path: String, ctx: &AuditCtx, p: &Profile, log: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if m.keys().any(|k| tune::find(k).is_some()) {
                    let issues = audit(m, ctx);
                    let dirty_bad = issues.iter().any(|i| i.key.starts_with("vm.dirty_") && i.reject);
                    if dirty_bad {
                        let (bg, d) = dirty_pair(p.io.bps, ctx.ram_kb * 1024, 1.0);
                        m.insert("vm.dirty_background_bytes".into(), json!(bg));
                        m.insert("vm.dirty_bytes".into(), json!(d));
                        log.push(format!("{path}: dirty limits -> {} / {} MiB ({})", bg >> 20, d >> 20, p.io.summary()));
                    }
                    for i in issues {
                        if i.key.starts_with("vm.dirty_") && dirty_bad { continue; }
                        match (i.fix, i.reject) {
                            (Some(f), _) => { log.push(format!("{path}: {} {} -> {} ({})", i.key, vstr(&m[&i.key]), vstr(&f), i.msg)); m.insert(i.key, f); }
                            (None, true) => { log.push(format!("{path}: {} removed ({})", i.key, i.msg)); m.remove(&i.key); }
                            (None, false) => log.push(format!("{path}: {} kept, note: {}", i.key, i.msg)),
                        }
                    }
                }
                for (k, x) in m.iter_mut() { walk(x, format!("{path}/{k}"), ctx, p, log); }
            }
            Value::Array(a) => for (i, x) in a.iter_mut().enumerate() { walk(x, format!("{path}/{i}"), ctx, p, log); },
            _ => {}
        }
    }
    let mut log = Vec::new();
    walk(v, String::new(), ctx, p, &mut log);
    log
}

/// Full autotune: profile, rules, availability filter. Unprivileged.
pub fn autotune(goal: Goal) -> Value { autotune_with(goal, &Profile::gather()) }

/// With user weight overrides ({"latency": 1.2, ...}; null = goal defaults).
pub fn autotune_req(goal: Goal, weights: &Value) -> Value {
    autotune_weighted(goal, &Profile::gather(), Weights::for_goal(goal).with_overrides(weights))
}

pub fn autotune_with(goal: Goal, p: &Profile) -> Value { autotune_weighted(goal, p, Weights::for_goal(goal)) }

pub fn autotune_weighted(goal: Goal, p: &Profile, w: Weights) -> Value {
    let (decisions, scores, notes) = decide_weighted(goal, p, w);
    let mut values = Map::new();
    let mut why = Map::new();
    let mut skipped = Vec::new();
    for d in decisions {
        let Some(t) = tune::find(d.key) else { continue };
        if !cfg!(test) {
            if !tune::vendor_ok(t) { continue; }
            if !t.debugfs && tune::files(t).is_empty() {
                skipped.push(json!({"key": d.key, "why": "not available on this machine/kernel"}));
                continue;
            }
        }
        match validate_static(t, &d.value) {
            Ok(v) => {
                let jv = match t.kind { tune::Kind::Int { .. } => v.parse::<i64>().map(Value::from).unwrap_or(Value::String(v)), _ => Value::String(v) };
                values.insert(d.key.to_owned(), jv);
                why.insert(d.key.to_owned(), Value::String(d.why));
            }
            Err(e) => skipped.push(json!({"key": d.key, "why": e})),
        }
    }
    // Invariant: nothing autotune emits may fail its own audit.
    let ctx = AuditCtx { ram_kb: p.ram_kb, cmdline: p.cmdline.clone(), numa_nodes: p.numa_nodes };
    for is in audit(&values, &ctx).into_iter().filter(|i| i.reject) {
        values.remove(&is.key);
        why.remove(&is.key);
        skipped.push(json!({"key": is.key, "why": format!("safety audit: {}", is.msg)}));
    }
    let live: Map<String, Value> = p.current.iter().map(|(k, v)| (k.clone(), v.parse::<i64>().map(Value::from).unwrap_or_else(|_| json!(v)))).collect();
    let live_issues: Vec<Value> = audit(&live, &ctx).iter().map(Issue::to_json).collect();
    let (run, run_why) = run_block(goal, p);
    if let Some(rw) = run_why { why.insert("run".into(), Value::String(rw)); }
    let summary = format!("Autotuned for {} on {}.", goal.label().to_lowercase(), p.summary());
    json!({
        "ok": true, "goal": goal.key(), "goal_label": goal.label(), "name": goal.preset_name(),
        "profile": p.to_json(), "profile_summary": p.summary(), "evidence_summary": p.evidence.summary(),
        "weights": w.to_json(), "scores": scores, "constraints": notes, "live_issues": live_issues,
        "preset": {"values": values, "run": run, "summary": summary,
                   "autotune": {"goal": goal.key(), "kernel": format!("{}.{}", p.kernel.0, p.kernel.1),
                                "weights": w.to_json(), "storage_write": p.io.to_json()}},
        "rationale": why, "skipped": skipped,
    })
}

/// tune::validate for Int/Bool rows; Choice rows are checked against the live
/// option list only outside tests (the list comes from sysfs).
fn validate_static(t: &tune::Tunable, v: &Value) -> Result<String, String> {
    if cfg!(test) && t.kind == tune::Kind::Choice {
        let s = vstr(v);
        return if s.is_empty() || s.len() > 64 { Err("bad choice".into()) } else { Ok(s) };
    }
    tune::validate(t, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legion() -> Profile {
        Profile {
            vendor: tune::Vendor::Amd, model: "AMD Ryzen 9 9955HX3D 16-Core Processor".into(), logical: 32, cores: 16, smt_active: true,
            ccds: vec![
                tune::Ccx { index: 0, cpus: (0..8).chain(16..24).collect(), l3_kib: 98304, max_khz: 5_200_000 },
                tune::Ccx { index: 1, cpus: (8..16).chain(24..32).collect(), l3_kib: 32768, max_khz: 5_450_000 },
            ],
            cache_ccd: Some(0), freq_ccd: Some(1), x3d_driver: true, hybrid: false,
            driver: "amd-pstate-epp".into(), epp: true, governors: vec!["performance".into(), "powersave".into()],
            idle_governors: vec!["menu".into(), "teo".into()],
            cstates: vec![CState { name: "POLL".into(), latency_us: 0 }, CState { name: "C1".into(), latency_us: 1 },
                          CState { name: "C2".into(), latency_us: 18 }, CState { name: "C3".into(), latency_us: 350 }],
            ram_kb: 32_166_484, swap: SwapKind::Zram, nvme: true, rotational: false, sata_hosts: false,
            battery: true, on_ac: Some(true), nvidia_dgpu: true, amd_igpu: true, intel_igpu: false, wifi: true,
            kernel: (7, 2), numa_nodes: 1, scx: vec!["lavd".into()], dynamic_epp: true, uncore: None,
            evidence: Evidence::default(),
            io: iorate::IoRate { bps: 1500 * MIB, source: iorate::Source::Probe, device: "nvme0n1".into(), class: iorate::DevClass::Nvme },
            cmdline: "root=/dev/nvme0n1p2 nowatchdog pcie_aspm=force usbcore.autosuspend=-1 processor.max_cstate=9".into(),
            defaults: None, outcomes: BTreeMap::new(), calibration: None,
            current: BTreeMap::from([
                ("vm.min_free_kbytes".into(), "67584".into()), ("cpu.ccd_park".into(), "none".into()),
                ("thp.enabled".into(), "always".into()), ("thp.khp_max_ptes_none".into(), "409".into()),
                ("thp.mthp_64k".into(), "inherit".into()), ("thp.mthp_16k".into(), "never".into()),
            ]),
        }
    }
    fn get<'a>(d: &'a [Decision], k: &str) -> Option<&'a Value> { d.iter().find(|x| x.key == k).map(|x| &x.value) }
    fn int(d: &[Decision], k: &str) -> i64 { get(d, k).and_then(Value::as_i64).unwrap_or_else(|| panic!("{k} missing")) }
    fn ctx(p: &Profile) -> AuditCtx { AuditCtx { ram_kb: p.ram_kb, cmdline: p.cmdline.clone(), numa_nodes: 1 } }

    #[test]
    fn dirty_limits_are_sane_for_every_ram_size_and_goal() {
        for gb in [8u64, 16, 32, 64, 128] {
            for g in Goal::ALL {
                for bps in [60 * MIB, 120 * MIB, 500 * MIB, 3000 * MIB] {
                    let mut p = legion();
                    p.ram_kb = gb << 20;
                    p.io.bps = bps;
                    let d = decide(g, &p);
                    let (bg, full) = (int(&d, "vm.dirty_background_bytes"), int(&d, "vm.dirty_bytes"));
                    assert!(full > bg, "{gb} GB {g:?}: {bg} !< {full}");
                    assert!(bg >= 8 << 20 && full >= 32 << 20, "{gb} GB {g:?} floors");
                    assert!(full as u64 <= ((gb << 30) / 50).min(1 << 30).max(32 << 20), "{gb} GB {g:?}: {full} over 2% of RAM / 1 GiB");
                    assert!(full.abs() < JSON_SAFE_INT);
                    for x in [bg, full] {
                        assert!(tune::validate(tune::find("vm.dirty_bytes").unwrap(), &json!(x)).is_ok());
                    }
                }
            }
        }
    }

    #[test]
    fn never_reproduces_the_wrapped_scene_values() {
        let p = legion();
        for g in Goal::ALL {
            let v = autotune_with(g, &p);
            let vals = &v["preset"]["values"];
            for k in ["vm.dirty_bytes", "vm.dirty_background_bytes"] {
                let x = vals[k].as_i64().unwrap();
                assert!(x != 8192 && x != 290_489_958 && x >= 8 << 20, "{g:?} {k} = {x}");
                // Survives a double (Qt JSON) round trip exactly.
                assert_eq!(x as f64 as i64, x);
            }
            // Every emitted int is exact through JSON.
            for (_, x) in vals.as_object().unwrap() { if let Some(n) = x.as_i64() { assert!(n.abs() <= JSON_SAFE_INT); } }
        }
    }

    #[test]
    fn dirty_follows_device_speed() {
        let mut p = legion();
        p.io = iorate::IoRate::class_default(iorate::DevClass::Hdd, "sda");
        let slow = decide(Goal::Desktop, &p);
        p.io = iorate::IoRate { bps: 2000 * MIB, source: iorate::Source::Probe, device: "nvme0n1".into(), class: iorate::DevClass::Nvme };
        let fast = decide(Goal::Desktop, &p);
        let (s, f) = (int(&slow, "vm.dirty_bytes"), int(&fast, "vm.dirty_bytes"));
        assert!(s < f, "HDD {s} vs NVMe {f}");
        assert!(s <= 128 << 20, "HDD dirty limit {s} should be about one second of 100 MiB/s");
        assert!(f >= 512 << 20);
    }

    #[test]
    fn thp_is_modest_and_kernel_consistent() {
        let p = legion();
        for g in Goal::ALL {
            let d = decide(g, &p);
            // Nobody gets THP=always or aggressive khugepaged by default.
            assert_eq!(get(&d, "thp.enabled"), Some(&json!("madvise")), "{g:?}");
            assert!(int(&d, "thp.khp_pages_to_scan") <= 8192 || get(&d, "thp.khp_pages_to_scan").is_none());
            // Live 409 with live mTHP on: resolved to a value the kernel accepts.
            let ptes = get(&d, "thp.khp_max_ptes_none").and_then(Value::as_i64).unwrap_or(409);
            let mthp_on = MTHP.iter().any(|(k, _)| get(&d, k).map(vstr).or_else(|| p.cur(k).map(str::to_owned)).map_or(false, |v| v != "never"));
            assert!(!mthp_on || ptes == 0 || ptes == 511, "{g:?}: mTHP on with max_ptes_none {ptes}");
        }
        // The combination search itself never offers an invalid pair.
        let mut p2 = legion();
        p2.current.clear();
        let w = Weights { throughput: 3.0, footprint: 0.0, ..Weights::for_goal(Goal::Throughput) };
        let (d, _, _) = decide_weighted(Goal::Throughput, &p2, w);
        assert_eq!(get(&d, "thp.enabled"), Some(&json!("always")), "weights must be able to buy THP=always");
        let ptes = get(&d, "thp.khp_max_ptes_none").and_then(Value::as_i64).unwrap_or(511);
        let on = MTHP.iter().any(|(k, _)| get(&d, k).map_or(false, |v| v != "never"));
        assert!(!on || ptes == 0 || ptes == 511);
    }

    #[test]
    fn boot_param_thp_is_respected() {
        let mut p = legion();
        p.cmdline.push_str(" transparent_hugepage=always");
        assert!(get(&decide(Goal::PowerSave, &p), "thp.enabled").is_none());
    }

    #[test]
    fn watermarks_need_evidence_and_stay_bounded() {
        let p = legion();
        for g in Goal::ALL {
            let d = decide(g, &p);
            assert!(get(&d, "vm.watermark_scale_factor").is_none(), "{g:?} raised watermarks without evidence");
            assert!(get(&d, "vm.min_free_kbytes").is_none(), "{g:?} touched min_free_kbytes");
            assert!(get(&d, "vm.watermark_boost_factor").map_or(true, |v| v.as_i64().unwrap() <= 15_000));
        }
        for gb in [8u64, 16, 32, 64, 128] {
            let mut p = legion();
            p.ram_kb = gb << 20;
            p.evidence = Evidence { uptime_s: 86_400, pgscan_direct: 400_000, pgscan_kswapd: 1_000_000, allocstall: 5_000, ..Default::default() };
            let d = decide(Goal::Gaming, &p);
            let f = int(&d, "vm.watermark_scale_factor");
            assert!(f > 10 && f <= 300, "{gb} GB: {f}");
            let headroom = (gb << 30) as f64 * f as f64 / 10_000.0;
            assert!(headroom <= ((gb << 30) / 50).min(1 << 30) as f64 * 1.01, "{gb} GB: {headroom}");
        }
    }

    #[test]
    fn oversized_reserve_is_repaired() {
        let mut p = legion();
        p.current.insert("vm.min_free_kbytes".into(), "4194304".into());
        p.current.insert("vm.watermark_scale_factor".into(), "3000".into());
        let d = decide(Goal::Throughput, &p);
        assert!(int(&d, "vm.min_free_kbytes") <= 131_072);
        assert_eq!(int(&d, "vm.watermark_scale_factor"), 10);
    }

    #[test]
    fn device_pm_respects_boot_params_and_risk() {
        let p = legion();
        for g in Goal::ALL {
            let d = decide(g, &p);
            assert!(get(&d, "usb.autosuspend").is_none(), "{g:?}: usbcore.autosuspend is on the cmdline");
            assert!(get(&d, "pm.usb_runtime") != Some(&json!("auto")), "{g:?}: USB runtime PM is a stability bet");
            assert!(get(&d, "pci.aspm_links").is_none(), "{g:?}");
            assert!(get(&d, "pci.aspm") != Some(&json!("powersave")), "{g:?}: pcie_aspm=force makes powersave risky");
            assert!(get(&d, "kernel.watchdog").is_none(), "{g:?}");
        }
        assert_eq!(get(&decide(Goal::PowerSave, &p), "pm.pci_runtime"), Some(&json!("auto")));
        assert_eq!(get(&decide(Goal::Desktop, &p), "pm.pci_runtime"), Some(&json!("auto")));
        assert!(get(&decide(Goal::Gaming, &p), "pm.pci_runtime").is_none());
        let mut q = legion();
        q.cmdline = "root=/dev/nvme0n1p2".into();
        assert_eq!(get(&decide(Goal::Desktop, &q), "pci.aspm"), Some(&json!("powersave")));
    }

    #[test]
    fn audit_catches_the_broken_scenes() {
        let p = legion();
        let c = ctx(&p);
        let power: Map<String, Value> = serde_json::from_value(json!({"vm.dirty_bytes": 8192, "vm.dirty_background_bytes": 8192})).unwrap();
        let thr: Map<String, Value> = serde_json::from_value(json!({"vm.dirty_bytes": 290_489_958, "vm.dirty_background_bytes": 8192,
            "thp.khp_max_ptes_none": 409, "thp.mthp_64k": "inherit", "thp.khp_pages_to_scan": 16384})).unwrap();
        let a = audit(&power, &c);
        assert!(a.iter().any(|i| i.key == "vm.dirty_bytes" && i.reject));
        assert!(a.iter().any(|i| i.key == "vm.dirty_background_bytes" && i.reject));
        let b = audit(&thr, &c);
        assert!(b.iter().any(|i| i.key == "vm.dirty_background_bytes" && i.reject));
        assert!(b.iter().any(|i| i.key == "thp.khp_max_ptes_none" && i.fix == Some(json!(0))));
        assert!(b.iter().any(|i| i.key == "thp.khp_pages_to_scan"));
        // A scene file: nested objects are found and repaired.
        let mut scene = json!({"name": "Daily", "optimizations": {"values": power}});
        assert!(audit_tree(&scene, &c).len() >= 2);
        let log = repair_tree(&mut scene, &c, &p);
        assert!(!log.is_empty());
        assert!(audit_tree(&scene, &c).iter().all(|(_, i)| !i.reject));
        let v = &scene["optimizations"]["values"];
        assert!(v["vm.dirty_bytes"].as_i64().unwrap() > v["vm.dirty_background_bytes"].as_i64().unwrap());
        // Autotune output passes its own audit on every RAM size.
        for gb in [8u64, 16, 32, 64, 128] {
            let mut q = legion();
            q.ram_kb = gb << 20;
            for g in Goal::ALL {
                let out = autotune_with(g, &q);
                let vals: Map<String, Value> = out["preset"]["values"].as_object().unwrap().clone();
                assert!(audit(&vals, &ctx(&q)).iter().all(|i| !i.reject), "{gb} GB {g:?}");
                assert!(out["skipped"].as_array().unwrap().iter().all(|s| !s["why"].as_str().unwrap().starts_with("safety")));
            }
        }
    }


    fn with_defaults(mut p: Profile, kv: &[(&str, &str)]) -> Profile {
        p.defaults = Some(defaults::Defaults { values: kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), clean: true, kernel: "7.2.8".into() });
        p
    }

    #[test]
    fn anchored_thp_keeps_boot_mode_but_bounds_fill_in() {
        let mut p = with_defaults(legion(), &[("thp.enabled", "always"), ("thp.khp_max_ptes_none", "511"),
            ("thp.khp_pages_to_scan", "4096"), ("thp.khp_scan_sleep_ms", "10000"), ("thp.mthp_16k", "never"),
            ("thp.mthp_32k", "never"), ("thp.mthp_64k", "never")]);
        p.current.clear();
        p.current.insert("thp.enabled".into(), "always".into());
        let g = decide(Goal::Gaming, &p);
        assert!(get(&g, "thp.enabled").is_none(), "gaming stays on the boot THP mode");
        assert_eq!(get(&g, "thp.khp_max_ptes_none"), Some(&json!(0)), "but khugepaged stops filling in");
        assert_eq!(get(&decide(Goal::Desktop, &p), "thp.enabled"), Some(&json!("madvise")), "footprint weight moves desktop");
    }

    #[test]
    fn trust_region_and_return_to_boot_default() {
        let mut p = with_defaults(legion(), &[("vm.watermark_scale_factor", "10"), ("vm.vfs_cache_pressure", "100")]);
        p.ram_kb = 32 << 20;
        p.evidence = Evidence { uptime_s: 86_400, pgscan_direct: 30_000, pgscan_kswapd: 1_000_000, allocstall: 1_000, ..Default::default() };
        // Weak evidence: 128 MiB (factor ~39) is > 2x from the boot 10 -> outside the trust region.
        let (d, sc, _) = decide_weighted(Goal::Gaming, &p, Weights::for_goal(Goal::Gaming));
        assert!(get(&d, "vm.watermark_scale_factor").map_or(true, |v| v == &json!(10)), "{:?}", sc.get("vm.watermark_scale_factor"));
        p.evidence = Evidence { uptime_s: 86_400, pgscan_direct: 400_000, pgscan_kswapd: 1_000_000, allocstall: 5_000, ..Default::default() };
        assert!(int(&decide(Goal::Gaming, &p), "vm.watermark_scale_factor") > 10);
        // A drifted live value goes back to the boot default when nothing beats it.
        p.evidence = Evidence::default();
        p.current.insert("vm.vfs_cache_pressure".into(), "50".into());
        assert_eq!(get(&decide(Goal::Throughput, &p), "vm.vfs_cache_pressure"), Some(&json!(100)));
    }

    #[test]
    fn off_is_a_mode_not_a_dose_for_the_trust_region() {
        assert_eq!(anchor_dist("0", "15000"), None);
        assert!((anchor_dist("7500", "15000").unwrap() - 1.0).abs() < 1e-3);
        // Boot snapshot with boosted reclaim on, heavy file refaults: "off" must stay reachable
        // (as a dose it was 14 doublings away and silently dropped by the 2x region).
        let mut p = with_defaults(legion(), &[("vm.watermark_boost_factor", "15000")]);
        p.evidence = Evidence { uptime_s: 86_400, workingset_refault_file: 86_400 * 4000, ..Default::default() };
        let (d, sc, _) = decide_weighted(Goal::Gaming, &p, Weights::for_goal(Goal::Gaming));
        assert_eq!(get(&d, "vm.watermark_boost_factor"), Some(&json!(0)), "{:?}", sc.get("vm.watermark_boost_factor"));
    }

    #[test]
    fn storage_weight_scales_calibrated_io_knobs() {
        let mut p = legion();
        let mut cal = calib::Calibration::default();
        let mut rng = model::Rng::new(4);
        let rows: Vec<calib::Row> = (0..48).map(|i| {
            let cfg: model::Cfg = if i % 3 == 0 { vec![] } else { vec![("blk.nomerges".into(), "2".into())] };
            let on = !cfg.is_empty();
            let n = ((rng.unit() + rng.unit()) - 1.0) * 0.01;
            calib::Row { phase: calib::Phase::Io, sess: 2, pos: i as f64 / 48.0, t: i, kernel: String::new(), cfg,
                         y: [n, if on { 0.12 } else { 0.0 } + n, n, n], w: 1.0, bv: calib::BENCH_VERSION }
        }).collect();
        cal.put_session(calib::Phase::Io, 2, rows);
        cal.refs.insert("blk.nomerges".into(), "0".into());
        p.calibration = Some(cal);
        p.current.insert("blk.nomerges".into(), "0".into());
        let w = Weights::for_goal(Goal::Throughput);
        assert_eq!(get(&decide_weighted(Goal::Throughput, &p, w).0, "blk.nomerges"), Some(&json!(2)), "measured +12 % I/O throughput");
        let off = Weights { storage: 0.0, ..w };
        assert!(get(&decide_weighted(Goal::Throughput, &p, off).0, "blk.nomerges").is_none(), "storage weight 0: the I/O gain counts for nothing");
        assert_eq!(Weights::for_goal(Goal::Gaming).with_overrides(&json!({"storage": 2.5})).storage, 2.5);
    }

    #[test]
    fn rollbacks_retire_a_key() {
        let mut p = with_defaults(legion(), &[("pm.pci_runtime", "on")]);
        p.current.insert("pm.pci_runtime".into(), "on".into());
        assert_eq!(get(&decide(Goal::Desktop, &p), "pm.pci_runtime"), Some(&json!("auto")));
        p.outcomes.insert("pm.pci_runtime".into(), defaults::Outcome { applies: 3, rollbacks: 1 });
        let one = decide(Goal::Desktop, &p);
        assert!(get(&one, "pm.pci_runtime").is_none(), "one rollback in four applies already costs more than the gain");
        p.outcomes.insert("pm.pci_runtime".into(), defaults::Outcome { applies: 50, rollbacks: 2 });
        assert!(get(&decide(Goal::PowerSave, &p), "pm.pci_runtime").is_none(), "two rollbacks retire it");
        let d = defaults::parse_defaults(&json!({"clean": true, "kernel": "7.2", "values": {"vm.swappiness": "60"}})).unwrap();
        assert_eq!(d.values["vm.swappiness"], "60");
        let o = defaults::parse_outcomes(&json!({"vm.dirty_bytes": {"applies": 4, "rollbacks": 1}}));
        assert_eq!(o["vm.dirty_bytes"], defaults::Outcome { applies: 4, rollbacks: 1 });
    }


    fn meas(lat: f64, thr: f64, mem: Option<f64>) -> calib::Measured { calib::Measured { lat: Some(lat), thr: Some(thr), pwr: None, mem, n: 1.0 } }
    /// n runs of the same result, idle or load.
    fn sig(c: &mut calib::Calibration, key: &str, r: &str, v: &str, load: bool, m: calib::Measured, runs: usize) {
        for _ in 0..runs { c.add(key, r, v, if load { calib::Phase::Load } else { calib::Phase::Idle }, Some(m), false, 0, ""); }
    }

    #[test]
    fn calibration_replaces_estimates() {
        let mut p = legion();
        p.current.clear();
        let mut cal = calib::Calibration::default();
        sig(&mut cal, "thp", "madvise", "always", false, meas(0.0, 0.01, Some(-0.6)), 5);
        sig(&mut cal, "thp", "madvise", "madvise+mthp", false, meas(0.0, 0.0, Some(0.0)), 5);
        sig(&mut cal, "thp", "madvise", "always+mthp", false, meas(0.0, 0.01, Some(-0.7)), 5);
        sig(&mut cal, "blk.read_ahead_kb", "128", "1024", false, meas(0.0, 0.30, None), 5);
        sig(&mut cal, "vm.dirty", "1", "0.5", false, meas(0.40, 0.0, None), 5);
        p.current.insert("blk.read_ahead_kb".into(), "128".into());
        p.io.bps = 400 * MIB; // windows 0.5 and 1 are different limits here (on a fast disk both clamp to the same)
        p.calibration = Some(cal);
        let w = Weights { throughput: 3.0, footprint: 0.0, ..Weights::for_goal(Goal::Throughput) };
        let (d, _, _) = decide_weighted(Goal::Throughput, &p, w);
        let u_of = |sc: &Map<String, Value>| sc["thp"].as_array().unwrap().iter()
            .find(|x| x["value"] == json!("always/511/Default")).unwrap()["u"].as_f64().unwrap();
        let (_, measured, _) = decide_weighted(Goal::Throughput, &p, w);
        let mut q = p.clone();
        q.calibration = None;
        let (_, modelled, _) = decide_weighted(Goal::Throughput, &q, w);
        assert!(u_of(&measured) < 0.0 && u_of(&modelled) > 0.3, "measured: THP=always buys ~nothing here");
        assert_eq!(get(&d, "blk.read_ahead_kb"), Some(&json!(1024)));
        let de = decide(Goal::Desktop, &p);
        assert_eq!(int(&de, "vm.dirty_bytes") as u64, dirty_pair(p.io.bps, p.ram_kb * 1024, 0.5).1);
        assert!(de.iter().find(|x| x.key == "vm.dirty_bytes").unwrap().why.contains("measured"));
    }

    #[test]
    fn load_phase_measurements_drive_pressure_knobs() {
        let mut p = legion();
        let wsf = wsf_for(256 << 20, p.ram_kb * 1024).to_string();
        let mut cal = calib::Calibration::default();
        sig(&mut cal, "vm.watermark_scale_factor", "10", &wsf, true, meas(0.30, 0.0, None), 3);
        cal.add("mm.lru_gen_min_ttl", "0", "1000", calib::Phase::Load, Some(meas(0.9, 0.0, None)), true, 0, "");
        sig(&mut cal, "sched.preempt", "full", "lazy", true, meas(-0.25, 0.0, None), 3);
        sig(&mut cal, "sched.preempt", "full", "voluntary", true, meas(-0.4, 0.0, None), 3);
        sig(&mut cal, "vm.swappiness", "150", "60", true, meas(-0.2, 0.0, None), 3);
        sig(&mut cal, "vm.swappiness", "150", "100", true, meas(0.0, 0.0, None), 3);
        p.calibration = Some(cal);
        let g = decide(Goal::Gaming, &p);
        assert_eq!(get(&g, "vm.watermark_scale_factor").map(vstr), Some(wsf));
        assert!(get(&g, "mm.lru_gen_min_ttl").is_none(), "unsafe candidate never picked");
        assert!(get(&decide(Goal::Throughput, &p), "sched.preempt").map_or(true, |v| v == "full"));
        assert!(get(&g, "vm.swappiness").map_or(true, |v| v == 150));
    }

    #[test]
    fn signature_doses_numeric_knobs_and_respects_roles() {
        let mut p = with_defaults(legion(), &[("sched.migration_cost_ns", "500000"), ("cpu.epp", "balance_performance")]);
        let mut cal = calib::Calibration::default();
        // Measured: 250 µs slightly better, 1 ms clearly better, 5 ms worse -> the optimum lies in between.
        sig(&mut cal, "sched.migration_cost_ns", "500000", "250000", true, meas(-0.05, 0.0, None), 4);
        sig(&mut cal, "sched.migration_cost_ns", "500000", "1000000", true, meas(0.20, 0.05, None), 4);
        sig(&mut cal, "sched.migration_cost_ns", "500000", "5000000", true, meas(-0.10, 0.10, None), 4);
        // Global EPP measured: must not override the per-CCD roles on an X3D part.
        sig(&mut cal, "cpu.epp", "balance_performance", "performance", false, meas(0.3, 0.3, None), 4);
        p.calibration = Some(cal);
        let (d, sc, _) = decide_weighted(Goal::Gaming, &p, Weights::for_goal(Goal::Gaming));
        let v = int(&d, "sched.migration_cost_ns");
        assert!(v >= 700_000 && v <= 2_300_000, "dose between the measured points: {v}");
        assert!(sc["sched.migration_cost_ns"].as_array().unwrap().len() > 4, "interpolated doses were scored");
        assert!(get(&d, "cpu.epp").is_none() && get(&d, "cpu.epp_ccd0").is_some());
    }

    #[test]
    fn guard_rolls_back_on_sustained_stalls_only() {
        let base = Pressure { io_full10: 1.0, mem_full10: 0.0, allocstall: 100 };
        let calm: Vec<Pressure> = (0..60).map(|i| Pressure { io_full10: 3.0 + (i % 5) as f64, mem_full10: 0.5, allocstall: 100 }).collect();
        assert!(guard_verdict(&base, &calm).is_none());
        // A short burst (a build starting) is not a stall.
        let mut burst = calm.clone();
        for s in burst.iter_mut().take(8) { s.io_full10 = 60.0; }
        assert!(guard_verdict(&base, &burst).is_none());
        // dirty_bytes = 8192: writers throttled, io full stays high.
        let mut stall = calm.clone();
        for s in stall.iter_mut().skip(5).take(20) { s.io_full10 = 70.0; }
        assert!(guard_verdict(&base, &stall).unwrap().starts_with("I/O stall"));
        let mut mem = calm.clone();
        for s in mem.iter_mut().skip(10).take(12) { s.mem_full10 = 25.0; }
        assert!(guard_verdict(&base, &mem).unwrap().starts_with("memory stall"));
        let storm: Vec<Pressure> = (0..20).map(|i| Pressure { allocstall: 100 + 1000 * (i as u64 + 1), ..Default::default() }).collect();
        assert!(guard_verdict(&base, &storm).unwrap().starts_with("direct reclaim"));
        assert!(guarded("vm.dirty_bytes") && guarded("thp.enabled") && !guarded("cpu.epp"));
    }

    #[test]
    fn weights_override_and_clamp() {
        let w = Weights::for_goal(Goal::Gaming).with_overrides(&json!({"latency": 9, "stability": 0.0, "bogus": 1, "power": "x"}));
        assert_eq!(w.latency, 3.0);
        assert_eq!(w.stability, 0.5);
        assert_eq!(w.power, Weights::for_goal(Goal::Gaming).power);
        // More power weight buys power-saving knobs in a desktop profile.
        let p = legion();
        let (d, scores, _) = decide_weighted(Goal::Desktop, &p, Weights::for_goal(Goal::Desktop).with_overrides(&json!({"power": 2.0})));
        assert_eq!(get(&d, "pm.nvme_latency_us"), Some(&json!(100_000)));
        assert!(scores.contains_key("vm.dirty_bytes") && scores.contains_key("thp"));
    }

    #[test]
    fn structural_rules_kept() {
        let p = legion();
        let d = decide(Goal::Gaming, &p);
        assert_eq!(get(&d, "cpu.epp_ccd0"), Some(&json!("performance")));
        assert_eq!(get(&d, "cpu.epp_ccd1"), Some(&json!("balance_power")));
        assert_eq!(get(&d, "irq.affinity"), Some(&json!("ccd1")));
        assert_eq!(get(&d, "gpu.amdgpu_dpm"), Some(&json!("auto")));
        assert_eq!(get(&d, "cpu.epp_boost"), Some(&json!("1")));
        assert_eq!(get(&d, "vm.swappiness"), Some(&json!(150)));
        let de = decide(Goal::Desktop, &p);
        assert_eq!(get(&de, "cpu.max_freq_ccd0"), Some(&json!(4_400_000)));
        assert_eq!(get(&de, "cpu.boost_ccd1"), Some(&json!("0")));
        let mut q = legion();
        q.current.insert("cpu.ccd_park".into(), "ccd1".into());
        assert_eq!(get(&decide(Goal::Gaming, &q), "cpu.ccd_park"), Some(&json!("none")));
        for g in Goal::ALL {
            let d = decide(g, &p);
            let mut keys: Vec<_> = d.iter().map(|x| x.key).collect();
            keys.sort();
            let n = keys.len();
            keys.dedup();
            assert_eq!(n, keys.len(), "{g:?}: duplicate keys");
            assert!(d.iter().all(|x| tune::find(x.key).is_some()), "{g:?}: unknown key");
        }
        assert_eq!(parse_release("7.2.8-cachyos"), (7, 2));
        assert_eq!(Goal::parse("bare-throughput"), Some(Goal::Throughput));
    }

    #[test]
    fn parses_vmstat_and_psi() {
        let mut e = Evidence::default();
        e.parse_vmstat("allocstall_normal 10\nallocstall_movable 5\npgscan_direct 7\npgscan_kswapd 93\nkswapd_low_wmark_hit_quickly 4\nfoo bar\n");
        assert_eq!((e.allocstall, e.pgscan_direct, e.pgscan_kswapd, e.kswapd_low_wmark_quick), (15, 7, 93, 4));
        let psi = "some avg10=0.50 avg60=1.50 avg300=3.25 total=99\nfull avg10=0.20 avg60=0.10 avg300=0.40 total=9\n";
        assert_eq!(psi_avg300(psi, "some"), Some(3.25));
        assert_eq!(psi_avg10(psi, "full"), Some(0.20));
    }

    #[test]
    fn intel_hybrid() {
        let mut p = legion();
        p.vendor = tune::Vendor::Intel;
        p.ccds.truncate(1);
        p.cache_ccd = None; p.freq_ccd = None; p.hybrid = true; p.x3d_driver = false; p.dynamic_epp = false;
        p.amd_igpu = false; p.intel_igpu = true; p.uncore = Some((800_000, 3_800_000));
        let g = decide(Goal::Gaming, &p);
        assert_eq!(get(&g, "cpu.epp_pcore"), Some(&json!("performance")));
        assert_eq!(get(&g, "irq.affinity"), Some(&json!("ecore")));
        assert!(get(&g, "cpu.pstate_status").is_none());
    }

    /// Live run: `cargo test -p lpm-helpers live_autotune -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_autotune() {
        for g in Goal::ALL {
            let v = autotune(g);
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
            assert!(v["ok"].as_bool().unwrap());
        }
    }

    /// Signature with two knobs a rule decides: split-lock mitigation (structural) and EPP boost.
    /// `harm`: latency effect of the rule's value of each on the synthetic machine.
    fn ruled_signature(harm_split: f64, harm_boost: f64) -> Profile {
        let kv = [("kernel.split_lock_mitigate", "1"), ("cpu.epp_boost", "0")];
        let mut p = with_defaults(legion(), &kv);
        for (k, v) in kv { p.current.insert(k.into(), v.into()); }
        let facs = vec![model::Factor { key: "kernel.split_lock_mitigate".into(), reference: "1".into(), values: vec!["0".into()] },
                        model::Factor { key: "cpu.epp_boost".into(), reference: "0".into(), values: vec!["1".into()] }];
        let mut rng = model::Rng::new(5);
        let mut design = model::initial_design(&facs, 60, &mut rng);
        for _ in 0..12 { design.push(Vec::new()); }
        let n = design.len();
        let rows: Vec<calib::Row> = design.iter().enumerate().map(|(i, c)| {
            let has = |k: &str| c.iter().any(|(a, _)| a == k);
            let y = if has("kernel.split_lock_mitigate") { harm_split } else { 0.0 } + if has("cpu.epp_boost") { harm_boost } else { 0.0 };
            calib::Row { phase: calib::Phase::Load, sess: 1, pos: i as f64 / n as f64, t: 0, kernel: String::new(), cfg: c.clone(),
                         y: [y + ((rng.unit() + rng.unit()) - 1.0) * 0.04, f64::NAN, f64::NAN, f64::NAN], w: 1.0, bv: calib::BENCH_VERSION }
        }).collect();
        let mut cal = calib::Calibration::default();
        cal.put_session(calib::Phase::Load, 1, rows);
        for f in &facs { cal.refs.insert(f.key.clone(), f.reference.clone()); }
        p.calibration = Some(cal);
        p
    }

    #[test]
    fn blind_benchmarks_do_not_overturn_rules_but_credible_harm_does() {
        // The benchmarks see nothing (noise around zero): the rules' choices stay. Before, "no measured
        // effect" sent every calibrated knob back to its boot default.
        let p = ruled_signature(0.0, 0.0);
        let (d, _, notes) = decide_weighted(Goal::Gaming, &p, Weights::for_goal(Goal::Gaming));
        assert_eq!(get(&d, "kernel.split_lock_mitigate"), Some(&json!(0)), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("rule-based choice(s) kept")), "{notes:?}");
        let (d, _, _) = decide_weighted(Goal::Desktop, &p, Weights::for_goal(Goal::Desktop));
        assert_eq!(get(&d, "cpu.epp_boost"), Some(&json!("1")), "desktop keeps the rule's EPP boost when nothing speaks against it");
        // Measured clearly worse than the reference on this machine: the rule's choice goes.
        let p = ruled_signature(-0.15, -0.15);
        let (d, _, _) = decide_weighted(Goal::Gaming, &p, Weights::for_goal(Goal::Gaming));
        assert_ne!(get(&d, "kernel.split_lock_mitigate"), Some(&json!(0)), "{d:?}");
        let why = |d: &[Decision], k: &str| d.iter().find(|x| x.key == k).map(|x| x.why.clone()).unwrap_or_default();
        // EPP boost is the goal's own decision for gaming and throughput - no benchmark takes it away ...
        for g in [Goal::Gaming, Goal::Throughput] {
            let (d, _, _) = decide_weighted(g, &p, Weights::for_goal(g));
            assert_eq!(get(&d, "cpu.epp_boost"), Some(&json!("1")), "{g:?}");
            assert!(why(&d, "cpu.epp_boost").starts_with("EPP boost"), "{g:?}: the rule's own reason, not a model's");
        }
        // ... while the desktop goal follows the measurement.
        let (d, _, _) = decide_weighted(Goal::Desktop, &p, Weights::for_goal(Goal::Desktop));
        assert_ne!(get(&d, "cpu.epp_boost"), Some(&json!("1")), "{d:?}");
    }

    #[test]
    fn joint_model_keeps_synergy_and_drops_redundancy() {
        let kv = [("vm.swappiness", "60"), ("vm.page_cluster", "3"), ("vm.vfs_cache_pressure", "100"), ("vm.page_lock_unfairness", "5")];
        let mut p = with_defaults(legion(), &kv);
        for (k, v) in kv { p.current.insert(k.into(), v.into()); }
        let facs = vec![model::Factor { key: "vm.swappiness".into(), reference: "60".into(), values: vec!["100".into()] },
                        model::Factor { key: "vm.page_cluster".into(), reference: "3".into(), values: vec!["1".into()] },
                        model::Factor { key: "vm.vfs_cache_pressure".into(), reference: "100".into(), values: vec!["50".into()] },
                        model::Factor { key: "vm.page_lock_unfairness".into(), reference: "5".into(), values: vec!["3".into()] }];
        // Truth (latency): swappiness and page_cluster only pay together; vfs_cache_pressure and compaction do the same job (redundant).
        let truth = |c: &model::Cfg| {
            let has = |k: &str| c.iter().any(|(a, _)| a == k);
            let mut y = 0.0;
            if has("vm.swappiness") { y += 0.02; }
            if has("vm.page_cluster") { y += 0.02; }
            if has("vm.swappiness") && has("vm.page_cluster") { y += 0.12; }
            if has("vm.vfs_cache_pressure") { y += 0.10; }
            if has("vm.page_lock_unfairness") { y += 0.10; }
            if has("vm.vfs_cache_pressure") && has("vm.page_lock_unfairness") { y -= 0.10; }
            y
        };
        let mut rng = model::Rng::new(11);
        let mut design = model::initial_design(&facs, 70, &mut rng);
        for _ in 0..12 { design.push(Vec::new()); }
        let n = design.len();
        let rows: Vec<calib::Row> = design.iter().enumerate().map(|(i, c)| calib::Row {
            phase: calib::Phase::Load, sess: 1, pos: i as f64 / n as f64, t: 0, kernel: String::new(), cfg: c.clone(),
            y: [truth(c) + ((rng.unit() + rng.unit()) - 1.0) * 0.04, f64::NAN, f64::NAN, f64::NAN], w: 1.0, bv: calib::BENCH_VERSION }).collect();
        let mut cal = calib::Calibration::default();
        cal.put_session(calib::Phase::Load, 1, rows);
        for f in &facs { cal.refs.insert(f.key.clone(), f.reference.clone()); }
        p.calibration = Some(cal);
        let (d, _, notes) = decide_weighted(Goal::Gaming, &p, Weights::for_goal(Goal::Gaming));
        assert_eq!((get(&d, "vm.swappiness").map(vstr), get(&d, "vm.page_cluster").map(vstr)), (Some("100".into()), Some("1".into())), "synergy kept: {d:?}");
        let redundant = [get(&d, "vm.vfs_cache_pressure").is_some(), get(&d, "vm.page_lock_unfairness").is_some()];
        assert_eq!(redundant.iter().filter(|x| **x).count(), 1, "exactly one of the redundant pair: {d:?}");
        assert!(notes.iter().any(|n| n.starts_with("Joint model")), "{notes:?}");
        // An unsafe combination is never picked.
        let mut q = p.clone();
        q.calibration.as_mut().unwrap().add_unsafe_set(vec![("vm.swappiness".into(), "100".into()), ("vm.page_cluster".into(), "1".into())]);
        let (d2, _, _) = decide_weighted(Goal::Gaming, &q, Weights::for_goal(Goal::Gaming));
        assert!(!(get(&d2, "vm.swappiness").is_some() && get(&d2, "vm.page_cluster").is_some()), "{d2:?}");
    }
}
