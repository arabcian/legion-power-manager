//! Higher-level operations shared by the CLI and the root helper.

use crate::atomicio::{ensure_dir, write_json};
use crate::config::{Config, PERSISTENT_CONFIG_FILE};
use crate::hal::{gpu, monitoring, vfcurve};
use crate::profiles::apply::apply_profile;
use crate::profiles::native::{load_profile, profile_path};
use crate::types::CurveState;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

/// `{gpu, current_voltage_uV, vf_curve:[{index,freq_kHz,volt_uV,freq_offset_kHz,domain}]}`
/// — the shape the GUI reads from /run/nvcurve-gui/*.json.
pub fn curve_json(gpu_name: &str, state: &CurveState, voltage: Option<u32>) -> Value {
    let pts: Vec<Value> = state.points.iter().filter(|p| p.freq_khz > 0 || p.volt_uv > 0).map(|p| json!({
        "index": p.index, "freq_kHz": p.freq_khz, "volt_uV": p.volt_uv,
        "freq_offset_kHz": p.delta_khz, "domain": p.domain,
    })).collect();
    json!({"gpu": gpu_name, "current_voltage_uV": voltage, "vf_curve": pts})
}

/// Read the curve, retrying briefly if the driver is still settling after a write.
pub fn read_curve_retry(gpu_index: usize, retries: u32) -> Result<Value, String> {
    let mut last = String::from("unknown error");
    for attempt in 0..=retries {
        let (g, name) = gpu::get_gpu(gpu_index).map_err(|e| e.to_string())?;
        match vfcurve::read_curve(g, &name) {
            Ok(s) => return Ok(curve_json(&name, &s, monitoring::read_voltage(g).ok())),
            Err(e) => last = e,
        }
        if attempt < retries { std::thread::sleep(Duration::from_millis(150)); }
    }
    Err(last)
}

/// Reset → apply → read-back verify → retry. Ok(message) / Err(reason).
pub fn apply_profile_verified(gpu_index: usize, name: &str, cfg: &Config, max_retries: u32)
    -> Result<String, String>
{
    let (g, _) = gpu::get_gpu(gpu_index).map_err(|e| e.to_string())?;
    let (rc, d) = vfcurve::reset_all_offsets(g, false);
    if rc != 0 { return Err(format!("Reset failed: {d}")); }

    let path = profile_path(&cfg.profile_dir, name).ok_or_else(|| format!("Invalid profile name: {name:?}"))?;
    if !path.is_file() { return Err(format!("Profile not found: {name}")); }
    let expected = load_profile(&path)?.deltas().map_err(|e| format!("Malformed curve_deltas in profile: {e}"))?;

    let mut errs: Vec<String> = Vec::new();
    let mut warns: Vec<String> = Vec::new();
    for attempt in 0..max_retries {
        let out = apply_profile(gpu_index, name, cfg)?;
        errs = out.errors;
        warns = out.warnings;
        if errs.is_empty() && !expected.is_empty() {
            match vfcurve::read_clock_offsets(g) {
                Err(e) => errs = vec![format!("Read-back failed: {e}")],
                Ok(offs) => {
                    let mism: Vec<String> = expected.iter().filter_map(|(&i, &v)| {
                        let got = *offs.get(usize::try_from(i).ok()?)? as i64;
                        (got != v).then(|| format!("pt{i}: expected {:+.0}MHz got {:+.0}MHz",
                                                    v as f64 / 1000.0, got as f64 / 1000.0))
                    }).collect();
                    if !mism.is_empty() { errs = vec![format!("Read-back mismatch: {}", mism.join("; "))]; }
                }
            }
        }
        if errs.is_empty() { break; }
        if attempt + 1 < max_retries { std::thread::sleep(Duration::from_millis(300)); }
    }
    if !errs.is_empty() {
        let suffix = if max_retries > 1 { format!(" (after {max_retries} attempts)") } else { String::new() };
        return Err(format!("{}{suffix}", errs.join("; ")));
    }
    let mut msg = format!("Profile '{name}' applied successfully.");
    for w in warns { msg.push('\n'); msg.push_str(&w); }
    Ok(msg)
}

// ── Persistent config (/etc/nvcurve/config.json) as a raw JSON object ──────
// Edited as a raw object so keys this version doesn't know are preserved.

pub fn persistent_load(path: &str) -> Map<String, Value> {
    let p = Path::new(path);
    if !p.exists() { return Map::new(); }
    match std::fs::read_to_string(p).map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
        Ok(Value::Object(m)) => m,
        Ok(_) => { eprintln!("nvcurve: warning: {path} is not a JSON object — ignoring"); Map::new() }
        Err(e) => { eprintln!("nvcurve: warning: could not read {path}: {e}"); Map::new() }
    }
}

pub fn persistent_save(path: &str, m: &Map<String, Value>) -> std::io::Result<()> {
    let p = Path::new(path);
    if let Some(dir) = p.parent() { ensure_dir(dir, 0o755)?; }
    write_json(p, &Value::Object(m.clone()), 0o644)
}

/// Stable key of GPU `index`. None if discovery worked but the index doesn't
/// exist; "idx:N" if discovery itself failed (can't validate).
pub fn gpu_stable_key_offline(index: usize) -> Option<String> {
    match gpu::discover_gpus() {
        Ok(v) => v.into_iter().find(|g| g.index == index).map(|g| g.stable_key()),
        Err(_) => Some(format!("idx:{index}")),
    }
}

fn migrate_legacy(m: &mut Map<String, Value>) {
    if let Some(old) = m.remove("auto_load_profile") {
        let e = m.entry("auto_load_profiles").or_insert_with(|| json!({}));
        if let Value::Object(o) = e { o.entry("idx:0").or_insert(old); }
    }
}

/// Set (Some) or clear (None) GPU `index`'s default profile.
pub fn set_default_profile(index: usize, name: Option<&str>) -> Result<(), String> {
    let mut m = persistent_load(PERSISTENT_CONFIG_FILE);
    migrate_legacy(&mut m);
    let key = gpu_stable_key_offline(index).ok_or_else(|| format!("GPU {index} not found"))?;
    let entry = m.entry("auto_load_profiles").or_insert_with(|| json!({}));
    if !entry.is_object() { *entry = json!({}); }
    let profiles = entry.as_object_mut().unwrap();
    match name {
        Some(n) => { profiles.insert(key, json!(n)); }
        None => { profiles.remove(&key); }
    }
    if profiles.is_empty() { m.remove("auto_load_profiles"); }
    persistent_save(PERSISTENT_CONFIG_FILE, &m).map_err(|e| format!("cannot write {PERSISTENT_CONFIG_FILE}: {e}"))
}

/// Remove every auto-load entry pointing at `name` (any GPU). Returns true if any.
pub fn clear_default_references(name: &str) -> Result<bool, String> {
    let mut m = persistent_load(PERSISTENT_CONFIG_FILE);
    migrate_legacy(&mut m);
    let Some(Value::Object(profiles)) = m.get_mut("auto_load_profiles") else { return Ok(false) };
    let before = profiles.len();
    profiles.retain(|_, v| v.as_str() != Some(name));
    let changed = profiles.len() != before;
    if profiles.is_empty() { m.remove("auto_load_profiles"); }
    if changed {
        persistent_save(PERSISTENT_CONFIG_FILE, &m).map_err(|e| format!("cannot write {PERSISTENT_CONFIG_FILE}: {e}"))?;
    }
    Ok(changed)
}
