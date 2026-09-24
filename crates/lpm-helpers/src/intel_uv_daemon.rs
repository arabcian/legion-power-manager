//! Boot config + daemon for the Intel undervolt tool.
//!
//! Replicates:
//!   * intel-undervolt `daemon`: periodic re-apply (undervolt once, power and
//!     tjoffset every interval), config reload on SIGHUP (it uses SIGUSR1 —
//!     both are accepted), hwphint EPP switching by CPU load or RAPL power.
//!   * throttled: separate AC / BATTERY profiles, power-source polling,
//!     full re-apply when the source flips, Autoreload on config mtime change.
//!   * resume: intel-undervolt/throttled rely on sleep hooks / D-Bus; here a
//!     jump of CLOCK_BOOTTIME against CLOCK_MONOTONIC (time spent suspended)
//!     triggers a full re-apply, so no hook is needed while the daemon runs.

use crate::intel_uv::{self, HwpAlgo, HwpRule, Profile};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const BOOT_FILE: &str = "/etc/legion-power-manager/intel-uv-boot.json";

#[derive(Debug, Clone)]
pub struct BootConfig { pub ac: Option<Profile>, pub battery: Option<Profile>, pub interval_ms: u64, pub reapply: bool }

/// {"ac": profile|null, "battery": profile|null, "daemon": {"interval_ms": 5000, "reapply": true}}
/// A plain profile (no ac/battery keys) is used for both sources.
pub fn parse_boot(v: &Value) -> Result<BootConfig, String> {
    let o = v.as_object().ok_or("boot config must be an object")?;
    let split = o.contains_key("ac") || o.contains_key("battery");
    let prof = |k: &str| -> Result<Option<Profile>, String> {
        match o.get(k) { None | Some(Value::Null) => Ok(None),
            Some(p) => intel_uv::parse_profile(p).map(Some).map_err(|e| format!("{k}: {e}")) }
    };
    let (ac, battery) = if split { (prof("ac")?, prof("battery")?) } else {
        let p = intel_uv::parse_profile(v)?;
        (Some(p.clone()), Some(p))
    };
    let d = o.get("daemon").and_then(Value::as_object);
    let interval_ms = d.and_then(|d| d.get("interval_ms")).and_then(Value::as_u64).unwrap_or(5000);
    if !(500..=600_000).contains(&interval_ms) { return Err("daemon.interval_ms must be 500..600000".into()); }
    let reapply = d.and_then(|d| d.get("reapply")).and_then(Value::as_bool).unwrap_or(true);
    Ok(BootConfig { ac, battery, interval_ms, reapply })
}

pub fn load_boot() -> Result<Option<BootConfig>, String> {
    if !Path::new(BOOT_FILE).exists() { return Ok(None); }
    // Root writes MSR voltages from this file at every boot/resume: only a
    // root-owned, not group/other-writable regular file is trusted.
    let s = crate::read_root_file(BOOT_FILE, 256 * 1024)
        .ok_or_else(|| format!("{BOOT_FILE}: not a root-owned, non-writable regular file (or too large) — ignored"))?;
    let v: Value = serde_json::from_str(&s).map_err(|e| format!("{BOOT_FILE}: {e}"))?;
    parse_boot(&v).map(Some).map_err(|e| format!("{BOOT_FILE}: {e}"))
}

/// AC if any Mains supply is online. No Mains supply at all (desktop) → AC,
/// like undervolt.py (throttled assumes battery, which is wrong on desktops).
pub fn on_ac() -> bool {
    let Ok(rd) = std::fs::read_dir("/sys/class/power_supply") else { return true };
    let mut found = false;
    for e in rd.flatten() {
        let p = e.path();
        if std::fs::read_to_string(p.join("type")).map(|t| t.trim() == "Mains").unwrap_or(false) {
            found = true;
            if std::fs::read_to_string(p.join("online")).map(|t| t.trim() == "1").unwrap_or(false) { return true; }
        }
    }
    !found
}

impl BootConfig {
    pub fn for_source(&self, ac: bool) -> Option<&Profile> { if ac { self.ac.as_ref() } else { self.battery.as_ref() } }
}

// ── hwphint (intel-undervolt scaling.c / stat.c / power.c) ─────────────────

#[derive(Default)]
struct CpuStat { prev: HashMap<usize, (u64, u64)>, single: f64, multi: f64 }

impl CpuStat {
    /// Per-CPU busy fraction from /proc/stat (idle = 4th field, as in
    /// stat.c). single = max over CPUs. multi = mean: intel-undervolt sums the
    /// per-CPU loads without dividing, so its multi threshold is really a
    /// "number of busy CPUs" and 0.8 triggers on almost any load — fixed here.
    fn measure(&mut self) {
        let Ok(s) = std::fs::read_to_string("/proc/stat") else { return };
        let (mut single, mut sum, mut n) = (0.0f64, 0.0f64, 0usize);
        for line in s.lines() {
            let mut it = line.split_whitespace();
            let Some(name) = it.next() else { continue };
            if !name.starts_with("cpu") || name.len() == 3 { continue; }
            let Ok(idx) = name[3..].parse::<usize>() else { continue };
            let vals: Vec<u64> = it.filter_map(|x| x.parse().ok()).collect();
            if vals.len() < 4 { continue; }
            let (total, idle) = (vals.iter().sum::<u64>(), vals[3]);
            if let Some(&(pt, pi)) = self.prev.get(&idx) {
                if total > pt {
                    let load = (total - pt).saturating_sub(idle.saturating_sub(pi)) as f64 / (total - pt) as f64;
                    single = single.max(load);
                    sum += load;
                    n += 1;
                }
            }
            self.prev.insert(idx, (total, idle));
        }
        self.single = single;
        self.multi = if n > 0 { sum / n as f64 } else { 0.0 };
    }
}

#[derive(Default)]
struct Rapl { prev: HashMap<String, (u64, u64, Instant)>, power: HashMap<String, f64> }

impl Rapl {
    /// powercap energy_uj deltas, every zone and subzone (package-0, core,
    /// uncore, dram, psys …), as intel-undervolt's power.c.
    fn measure(&mut self) {
        let now = Instant::now();
        let Ok(rd) = std::fs::read_dir("/sys/class/powercap") else { return };
        for e in rd.flatten() {
            let p = e.path();
            let fname = e.file_name().to_string_lossy().into_owned();
            if !fname.starts_with("intel-rapl:") { continue; } // MSR interface only; -mmio mirrors package
            let (Ok(name), Ok(uj)) = (std::fs::read_to_string(p.join("name")), std::fs::read_to_string(p.join("energy_uj"))) else { continue };
            let Ok(uj) = uj.trim().parse::<u64>() else { continue };
            let range = std::fs::read_to_string(p.join("max_energy_range_uj")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(u64::MAX);
            let key = format!("{}#{fname}", name.trim());
            if let Some(&(pu, _, pt)) = self.prev.get(&key) {
                let dt = now.duration_since(pt).as_secs_f64();
                let de = if uj >= pu { uj - pu } else { uj + range.saturating_sub(pu) };
                if dt > 0.0 { self.power.insert(key.clone(), de as f64 / 1e6 / dt); }
            }
            self.prev.insert(key, (uj, range, now));
        }
    }
    /// rapl_lookup: name == domain or name starts with "domain-".
    fn get(&self, domain: &str) -> f64 {
        self.power.iter().find(|(k, _)| {
            let n = k.split('#').next().unwrap_or("");
            n == domain || n.strip_prefix(domain).map(|r| r.starts_with('-')).unwrap_or(false)
        }).map(|(_, &w)| w).unwrap_or(0.0)
    }
}

pub struct Hwp { stat: CpuStat, rapl: Rapl }

impl Hwp {
    pub fn new() -> Hwp { Hwp { stat: CpuStat::default(), rapl: Rapl::default() } }

    /// One cpu_policy_update pass. switch mode only touches policies whose
    /// current EPP is one of the rule's two hints (so a user/Optimizations
    /// choice is left alone); force writes every policy. First rule that
    /// handles a policy wins.
    pub fn update(&mut self, rules: &[HwpRule]) -> Vec<String> {
        let mut log = Vec::new();
        if rules.is_empty() { return log; }
        if rules.iter().any(|r| matches!(r.algo, HwpAlgo::Load { .. })) { self.stat.measure(); }
        if rules.iter().any(|r| matches!(r.algo, HwpAlgo::Power(_))) { self.rapl.measure(); }
        let Ok(rd) = std::fs::read_dir("/sys/devices/system/cpu/cpufreq") else { return log };
        let mut pols: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("policy")).unwrap_or(false)).collect();
        pols.sort();
        for pol in pols {
            let f = pol.join("energy_performance_preference");
            let Ok(cur) = std::fs::read_to_string(&f).map(|s| s.trim().to_owned()) else { continue };
            for r in rules {
                if !r.force && cur != r.load_hint && cur != r.normal_hint { continue; }
                let load = match &r.algo {
                    HwpAlgo::Load { multi, threshold } => (if *multi { self.stat.multi } else { self.stat.single }) >= *threshold,
                    HwpAlgo::Power(terms) => terms.iter().fold(false, |acc, t| {
                        let w = self.rapl.get(&t.domain);
                        let c = if t.greater { w > t.watts } else { w < t.watts };
                        if t.and { acc & c } else { acc | c }
                    }),
                };
                let hint = if load { &r.load_hint } else { &r.normal_hint };
                if r.force || cur != *hint {
                    if let Err(e) = crate::sysfs_write(&f, hint.as_bytes()) { log.push(format!("{}: {e}", f.display())); }
                }
                break;
            }
        }
        log
    }
}

impl Default for Hwp { fn default() -> Self { Self::new() } }

// ── daemon loop ────────────────────────────────────────────────────────────

/// fn item → fn pointer → integer: the two-step cast rustc's
/// function_casts_as_integer lint asks for (same value, no behaviour change).
fn handler(f: extern "C" fn(libc::c_int)) -> libc::sighandler_t { f as *const () as libc::sighandler_t }

static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);
extern "C" fn on_stop(_: libc::c_int) { STOP.store(true, Ordering::SeqCst); }
extern "C" fn on_reload(_: libc::c_int) { RELOAD.store(true, Ordering::SeqCst); }

fn clock(id: libc::clockid_t) -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}
fn suspended_s() -> f64 { clock(libc::CLOCK_BOOTTIME) - clock(libc::CLOCK_MONOTONIC) }

fn report(tag: &str, v: &Value, quiet_ok: bool) {
    if let Some(e) = v["error"].as_str() { eprintln!("lpm-intel-uv: {tag}: {e}"); return; }
    for r in v["results"].as_array().into_iter().flatten() {
        if r["ok"] != true || !quiet_ok {
            eprintln!("lpm-intel-uv: {tag}: {} {} {}", if r["ok"] == true { "OK " } else { "ERR" },
                      r["what"].as_str().unwrap_or(""), r["message"].as_str().unwrap_or(""));
        }
    }
}

fn mtime() -> Option<std::time::SystemTime> { std::fs::metadata(BOOT_FILE).and_then(|m| m.modified()).ok() }

pub fn run_daemon() -> i32 {
    unsafe {
        libc::signal(libc::SIGTERM, handler(on_stop));
        libc::signal(libc::SIGINT, handler(on_stop));
        libc::signal(libc::SIGHUP, handler(on_reload));
        libc::signal(libc::SIGUSR1, handler(on_reload));
    }
    let mut cfg = match load_boot() { Ok(c) => c, Err(e) => { eprintln!("lpm-intel-uv: {e}"); None } };
    let mut stamp = mtime();
    let mut applied: Option<bool> = None; // source of the last full apply
    let mut slept = suspended_s();
    let mut hwp = Hwp::new();
    eprintln!("lpm-intel-uv: daemon started ({})", if cfg.is_some() { "config loaded" } else { "no config yet" });

    while !STOP.load(Ordering::SeqCst) {
        let m = mtime();
        if RELOAD.swap(false, Ordering::SeqCst) || m != stamp {
            stamp = m;
            match load_boot() {
                Ok(c) => { cfg = c; applied = None; eprintln!("lpm-intel-uv: configuration reloaded"); }
                Err(e) => eprintln!("lpm-intel-uv: reload failed, keeping the old config: {e}"),
            }
        }
        let s = suspended_s();
        if s - slept > 1.0 { eprintln!("lpm-intel-uv: resume detected, re-applying"); applied = None; }
        slept = s;

        let interval = cfg.as_ref().map(|c| c.interval_ms).unwrap_or(5000);
        if let Some(c) = &cfg {
            let ac = on_ac();
            let src = if ac { "AC" } else { "BATTERY" };
            match c.for_source(ac) {
                Some(p) if applied != Some(ac) => {
                    if applied.is_some() { eprintln!("lpm-intel-uv: power source → {src}"); }
                    report(src, &intel_uv::apply(p), false);
                    applied = Some(ac);
                }
                Some(p) => {
                    if c.reapply {
                        let part = intel_uv::periodic_part(p);
                        if part.tjoffset.is_some() || part.pl1.is_some() || part.pl2.is_some()
                            || part.disable_bdprochot.is_some() || part.ctdp.is_some() {
                            report(src, &intel_uv::apply(&part), true);
                        }
                    }
                    for l in hwp.update(&p.hwphint) { eprintln!("lpm-intel-uv: hwphint: {l}"); }
                }
                None => {
                    if applied != Some(ac) { eprintln!("lpm-intel-uv: no profile for {src}, nothing applied"); applied = Some(ac); }
                }
            }
        }
        // Sleep in slices so SIGTERM is honoured quickly.
        let end = Instant::now() + Duration::from_millis(interval);
        while Instant::now() < end && !STOP.load(Ordering::SeqCst) && !RELOAD.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100.min(interval)));
        }
    }
    eprintln!("lpm-intel-uv: daemon stopped");
    0
}

/// One-shot boot/resume apply for the source that is active now.
pub fn apply_boot() -> Value {
    match load_boot() {
        Ok(None) => json!({"ok": true, "message": "no boot profile set"}),
        Err(e) => json!({"ok": false, "error": e}),
        Ok(Some(c)) => match c.for_source(on_ac()) {
            Some(p) => intel_uv::apply(p),
            None => json!({"ok": true, "message": format!("no profile for {}", if on_ac() { "AC" } else { "battery" })}),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boot_formats() {
        let legacy = parse_boot(&json!({"voltage": {"core": -50, "cache": -50}})).unwrap();
        assert!(legacy.ac.is_some() && legacy.battery.is_some() && legacy.reapply && legacy.interval_ms == 5000);
        let split = parse_boot(&json!({"ac": {"voltage": {"core": -50}}, "battery": null, "daemon": {"interval_ms": 2000, "reapply": false}})).unwrap();
        assert!(split.ac.is_some() && split.battery.is_none() && !split.reapply && split.interval_ms == 2000);
        assert!(parse_boot(&json!({"ac": {"voltage": {"core": 5}}})).is_err());
        assert!(parse_boot(&json!({"ac": null, "daemon": {"interval_ms": 10}})).is_err());
    }
}
