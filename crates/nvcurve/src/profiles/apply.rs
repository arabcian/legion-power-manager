//! Apply saved profiles to hardware (port of profiles/apply.py).
//!
//! Order (matches LACT): memory lock → memory offset → power limit → curve.

use super::native::{load_profile, profile_path};
use crate::config::Config;
use crate::hal::{gpu, limits, monitoring, snapshot, vfcurve};
use crate::safety::{validate_limits, validate_write, LimitField, Limits};
use log::{error, info, warn};
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct ApplyOutcome {
    /// Curve problems — the profile did not apply.
    pub errors: Vec<String>,
    /// Secondary settings that were rejected or failed — non-fatal.
    pub warnings: Vec<String>,
}

fn to_u32(v: i64) -> Option<u32> { u32::try_from(v).ok() }
fn to_i32(v: i64) -> Option<i32> { i32::try_from(v).ok() }

pub fn apply_profile(gpu_index: usize, name: &str, cfg: &Config) -> Result<ApplyOutcome, String> {
    let path = profile_path(&cfg.profile_dir, name).ok_or_else(|| format!("invalid profile name: {name:?}"))?;
    let mut p = load_profile(&path)?;
    let (g, gname) = gpu::get_gpu(gpu_index).map_err(|e| e.to_string())?;
    let idx = gpu_index as u32;
    let mut out = ApplyOutcome::default();

    let lim = Limits {
        power_limit_w: p.power_limit_w,
        mem_offset_mhz: p.mem_offset_mhz,
        mem_locked_min_mhz: p.mem_locked_min_mhz,
        mem_locked_max_mhz: p.mem_locked_max_mhz,
    };
    for e in validate_limits(idx, &lim) {
        out.warnings.push(format!("Rejected: {}", e.message));
        match e.field {
            LimitField::PowerLimit => p.power_limit_w = None,
            LimitField::MemOffset => p.mem_offset_mhz = None,
            LimitField::MemLocked => { p.mem_locked_min_mhz = None; p.mem_locked_max_mhz = None; }
        }
    }

    if let Some(max) = p.mem_locked_max_mhz.and_then(to_u32) {
        let min = p.mem_locked_min_mhz.and_then(to_u32).unwrap_or(max);
        if let Err(e) = limits::set_mem_locked_clocks(min, max, idx) {
            out.warnings.push(format!("Mem locked clocks: {e}"));
        }
    }
    if let Some(off) = p.mem_offset_mhz.and_then(to_i32) {
        if let Err(e) = limits::set_clock_offsets(None, Some(off), idx) {
            out.warnings.push(format!("Mem offset: {e}"));
        }
    }
    if let Some(w) = p.power_limit_w.and_then(to_u32) {
        if let Err(e) = limits::set_power_limit(w, idx) {
            out.warnings.push(format!("Power limit: {e}"));
        }
    }

    if p.curve_deltas.is_empty() {
        let (rc, d) = vfcurve::reset_offsets(g, false);
        if rc != 0 { out.warnings.push(format!("Curve reset failed ({rc}): {d}")); }
        return Ok(out);
    }
    let deltas = match p.deltas() {
        Ok(d) => d,
        Err(e) => { out.errors.push(format!("Curve: malformed curve_deltas in profile {name:?}: {e}")); return Ok(out); }
    };
    let errs = validate_write(&deltas, cfg.max_delta_khz);
    if !errs.is_empty() {
        out.errors.push(format!("Curve: {}", errs.join("; ")));
        return Ok(out);
    }
    // One table read serves both the snapshot and the write baseline.
    let raw = match vfcurve::read_clock_table_raw(g) {
        Ok(r) => r,
        Err(e) => { out.errors.push(format!("Curve write failed: cannot read ClockBoostTable: {e}")); return Ok(out); }
    };
    if cfg.auto_snapshot && snapshot::save(g, &gname, &cfg.snapshot_dir, cfg.max_snapshots, Some(raw.bytes())).is_none() {
        warn!("Auto-snapshot failed");
    }
    let (rc, d) = vfcurve::write_offsets(g, &deltas, false, false, Some(raw.bytes()));
    if rc != 0 { out.errors.push(format!("Curve write failed ({rc}): {d}")); }
    Ok(out)
}

/// Apply with read-back verification, retrying with 1s/2s/4s backoff.
pub fn apply_with_retry(gpu_index: usize, name: &str, cfg: &Config, max_retries: u32) -> bool {
    let Some(path) = profile_path(&cfg.profile_dir, name) else {
        warn!("Auto-load profile name {name:?} is invalid — skipping GPU {gpu_index}");
        return false;
    };
    let expected: BTreeMap<i64, i64> = match load_profile(&path) {
        Ok(p) => p.deltas().unwrap_or_else(|e| { warn!("Profile {name:?} has malformed curve_deltas: {e}"); BTreeMap::new() }),
        Err(e) => { warn!("Auto-load profile {name:?} unavailable ({e}) — skipping GPU {gpu_index}"); return false; }
    };

    for attempt in 1..=max_retries {
        let (errs, warns) = match apply_profile(gpu_index, name, cfg) {
            Ok(o) => (o.errors, o.warnings),
            Err(e) => (vec![e], vec![]),
        };
        if !warns.is_empty() {
            warn!("Auto-load attempt {attempt}/{max_retries} non-fatal warnings: {}", warns.join("; "));
        }
        if !errs.is_empty() {
            warn!("Auto-load attempt {attempt}/{max_retries} errors: {}", errs.join("; "));
        } else if expected.is_empty() {
            info!("Auto-load profile {name:?} applied on GPU {gpu_index} (attempt {attempt}/{max_retries})");
            return true;
        } else {
            match gpu::get_gpu(gpu_index).map_err(|e| e.to_string()).and_then(|(g, _)| vfcurve::read_clock_offsets(g)) {
                Err(e) => warn!("Auto-load attempt {attempt}/{max_retries}: read-back failed: {e}"),
                Ok(offs) => {
                    let mism: Vec<String> = expected.iter().filter_map(|(&i, &v)| {
                        let got = *offs.get(usize::try_from(i).ok()?)? as i64;
                        (got != v).then(|| format!("pt{i}: expected {:+.0}MHz got {:+.0}MHz",
                                                    v as f64 / 1000.0, got as f64 / 1000.0))
                    }).collect();
                    if mism.is_empty() {
                        info!("Auto-load profile {name:?} verified on GPU {gpu_index} (attempt {attempt}/{max_retries})");
                        return true;
                    }
                    warn!("Auto-load attempt {attempt}/{max_retries}: read-back mismatch — {}", mism.join("; "));
                }
            }
        }
        if attempt < max_retries {
            let delay = 1u64 << (attempt - 1);
            info!("Retrying auto-load in {delay}s…");
            std::thread::sleep(std::time::Duration::from_secs(delay));
        }
    }
    warn!("Auto-load profile {name:?} failed after {max_retries} attempts on GPU {gpu_index}");
    false
}

/// Boot-time autoload. Returns the process exit code.
pub fn run_autoload(cfg: &Config) -> i32 {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("nvcurve autoload: must run as root");
        return 1;
    }
    if cfg.auto_load_profiles.is_empty() {
        info!("No auto-load profiles configured.");
        return 0;
    }
    let gpus = match gpu::discover_gpus() {
        Ok(g) => g,
        Err(e) => { error!("Failed to initialize NvAPI: {e}"); return 1; }
    };
    if gpus.is_empty() { warn!("No GPUs discovered."); }
    let key_to_idx: BTreeMap<String, usize> = gpus.iter().map(|g| (g.stable_key(), g.index)).collect();
    for (key, prof) in &cfg.auto_load_profiles {
        if prof.is_empty() { continue; }
        let Some(&idx) = key_to_idx.get(key) else {
            warn!("Auto-load: no GPU found with key {key:?} — skipping");
            continue;
        };
        info!("Auto-loading profile {prof:?} on GPU {idx} ({key})");
        apply_with_retry(idx, prof, cfg, 3);
    }
    monitoring::shutdown_nvml();
    0
}
