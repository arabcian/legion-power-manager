//! nvcurve root helper — pkexec target for the NVIDIA tab.
//! Port of apps/nvcurve/root_helper.py; same JSON protocol:
//!   stdin  {"op": "<name>", ...op fields at top level...}
//!   stdout {"ok": bool, "message"|"error": str}
//!
//! Everything runs in-process on one NvAPI session. Unlike the Python
//! version nothing is delegated to `python3 -m nvcurve` subprocesses, so
//! there is no interpreter, PYTHONPATH or child environment to harden.

use nvcurve::atomicio::{ensure_dir, write_bytes, write_json};
use nvcurve::config::{Config, PERSISTENT_CONFIG_FILE};
use nvcurve::hal::{gpu, limits, vfcurve};
use nvcurve::profiles::apply::run_autoload;
use nvcurve::profiles::native::is_canonical_profile_name;
use nvcurve::{logging, ops};
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

const PROFILES_DIR: &str = "/etc/nvcurve/profiles";
const RESULT_DIR: &str = "/run/nvcurve-gui";
const READ_RESULT: &str = "nvcurve_read.json";
const APPLY_RESULT: &str = "nvcurve_apply_result.json";
const RESET_RESULT: &str = "nvcurve_reset_result.json";

const MAX_PROFILE_BYTES: usize = 256 * 1024;
const MAX_STDIN_BYTES: usize = 1024 * 1024;
const MAX_CURVE_POINTS: i64 = 255; // == CT_POINTS

type Obj = Map<String, Value>;
type OpResult = Result<String, String>;

// ── helpers ─────────────────────────────────────────────────────────────────

fn config() -> Config {
    let mut cfg = Config::load(Path::new(PERSISTENT_CONFIG_FILE));
    // Writes always land in PROFILES_DIR; apply must read the same place.
    cfg.profile_dir = PROFILES_DIR.into();
    cfg
}

fn profile_file(name: &str) -> PathBuf { Path::new(PROFILES_DIR).join(format!("{name}.json")) }

fn name_param<'a>(p: &'a Obj, key: &str) -> Result<&'a str, String> {
    p.get(key).and_then(Value::as_str).filter(|n| is_canonical_profile_name(n))
        .ok_or_else(|| "Invalid profile name".to_string())
}

/// Result files for the unprivileged GUI: root-owned 0755 dir that must not
/// be a symlink; the file is replaced atomically (never opened through a
/// pre-planted link).
fn write_result(file: &str, v: &Value) {
    let dir = Path::new(RESULT_DIR);
    if let Ok(md) = std::fs::symlink_metadata(dir) {
        if !md.is_dir() { log::warn!("{RESULT_DIR} is not a directory — result not written"); return; }
    }
    if ensure_dir(dir, 0o755).is_err() { return; }
    if let Err(e) = write_json(&dir.join(file), v, 0o644) { log::warn!("result write failed: {e}"); }
}

fn refresh_result(file: &str) {
    if let Ok(v) = ops::read_curve_retry(0, 2) { write_result(file, &v); }
}

fn is_int(v: &Value) -> Option<i64> { v.as_i64().filter(|_| v.is_i64() || v.is_u64()) }

/// Schema for profile_data (port of _validate_profile_data).
fn validate_profile_data(d: &Obj) -> Result<(), String> {
    const BOUNDS: &[(&str, i64, i64)] = &[
        ("mem_offset_mhz", -100_000, 100_000), ("power_limit_w", 1, 2_000),
        ("mem_locked_min_mhz", 1, 100_000), ("mem_locked_max_mhz", 1, 100_000),
        ("gpu_clock_cap_mhz", 210, 4_000),
    ];
    for (k, v) in d {
        if v.is_null() { continue; }
        match k.as_str() {
            "name" | "gpu_name" => if !v.as_str().is_some_and(|s| s.len() <= 256) {
                return Err(format!("field {k:?} must be a str of at most 256 bytes"));
            },
            "curve_deltas" => if !v.is_object() { return Err(format!("field {k:?} must be dict")); },
            _ => match BOUNDS.iter().find(|b| b.0 == k) {
                Some(&(_, lo, hi)) => {
                    let n = is_int(v).ok_or_else(|| format!("field {k:?} must be an integer"))?;
                    if !(lo..=hi).contains(&n) {
                        return Err(format!("field {k:?} value {n} outside plausible range ({lo}..{hi})"));
                    }
                }
                None => return Err(format!("unknown profile field: {k:?}")),
            },
        }
    }
    if let Some(n) = d.get("name").and_then(Value::as_str) {
        if !is_canonical_profile_name(n) { return Err("profile_data 'name' field has an invalid name".into()); }
    }
    if let Some(Value::Object(deltas)) = d.get("curve_deltas") {
        if deltas.len() as i64 > MAX_CURVE_POINTS {
            return Err(format!("curve_deltas has too many entries (max {MAX_CURVE_POINTS})"));
        }
        for (pk, dv) in deltas {
            let digits = pk.strip_prefix('-').unwrap_or(pk);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("curve_deltas key {pk:?} is not an integer string"));
            }
            match pk.parse::<i64>() {
                Ok(i) if (0..MAX_CURVE_POINTS).contains(&i) => {}
                _ => return Err(format!("curve_deltas point index {pk} out of range")),
            }
            let dl = is_int(dv).ok_or_else(|| format!("curve_deltas value for point {pk} must be an integer"))?;
            if !(-2_000_000..=2_000_000).contains(&dl) {
                return Err(format!("curve_deltas delta for point {pk} is implausible: {dl}"));
            }
        }
    }
    if let (Some(lo), Some(hi)) = (d.get("mem_locked_min_mhz").and_then(is_int), d.get("mem_locked_max_mhz").and_then(is_int)) {
        if lo > hi {
            return Err(format!("mem_locked_min_mhz ({lo}) is greater than mem_locked_max_mhz ({hi})"));
        }
    }
    Ok(())
}

// ── ops ─────────────────────────────────────────────────────────────────────

fn op_read_gpu_curve(_: &Obj) -> OpResult {
    let v = ops::read_curve_retry(0, 2).map_err(|e| format!("Failed to read V/F curve: {e}"))?;
    write_result(READ_RESULT, &v);
    Ok("Curve read successfully.".into())
}

fn op_apply_gpu_offsets(p: &Obj) -> OpResult {
    let name = name_param(p, "profile_name")?;
    let data = p.get("profile_data").and_then(Value::as_object).ok_or("profile_data must be an object")?;
    let text = serde_json::to_string_pretty(&Value::Object(data.clone())).map_err(|e| e.to_string())?;
    if text.len() > MAX_PROFILE_BYTES { return Err("profile_data too large".into()); }
    validate_profile_data(data).map_err(|e| format!("Invalid profile_data: {e}"))?;
    if let Some(embedded) = data.get("name").and_then(Value::as_str) {
        if embedded != name {
            return Err(format!("profile_data 'name' ({embedded:?}) does not match profile_name ({name:?})"));
        }
    }
    ensure_dir(Path::new(PROFILES_DIR), 0o755).map_err(|e| format!("Could not prepare {PROFILES_DIR}: {e}"))?;
    write_bytes(&profile_file(name), text.as_bytes(), 0o644).map_err(|e| format!("Could not write profile: {e}"))?;

    let msg = ops::apply_profile_verified(0, name, &config(), 3)?;
    refresh_result(APPLY_RESULT);
    Ok(msg)
}

fn op_apply_named_profile(p: &Obj) -> OpResult {
    let name = name_param(p, "name")?;
    if !profile_file(name).is_file() { return Err(format!("Profile not found: {name}")); }
    let msg = ops::apply_profile_verified(0, name, &config(), 3).map_err(|e| format!("Apply failed: {e}"))?;
    refresh_result(APPLY_RESULT);
    Ok(msg)
}

fn op_reset_gpu_curve(_: &Obj) -> OpResult {
    let (g, _) = gpu::get_gpu(0).map_err(|e| format!("Reset failed: {e}"))?;
    let (rc, d) = vfcurve::reset_all_offsets(g, false);
    if rc != 0 { return Err(format!("Reset write failed: {d}")); }
    let _ = limits::reset_gpu_locked_clocks(0);  // stock curve = no cap either
    refresh_result(RESET_RESULT);
    Ok("Reset successful.".into())
}

fn op_set_vram_memlock(p: &Obj) -> OpResult {
    let (Some(lo), Some(hi)) = (p.get("min_mhz").and_then(is_int), p.get("max_mhz").and_then(is_int)) else {
        return Err("min_mhz/max_mhz must be integers".into());
    };
    if lo <= 0 || hi <= 0 || lo > hi || hi > nvcurve::safety::MAX_MEM_CLOCK_MHZ {
        return Err("Invalid min_mhz/max_mhz range".into());
    }
    limits::set_mem_locked_clocks(lo as u32, hi as u32, 0)?;
    Ok(match limits::get_current_mem_clock(0) {
        Some(actual) if actual as i64 != hi => format!(
            "Requested lock: {lo}–{hi} MHz — driver resolved to {actual} MHz (nearest supported stock clock)."),
        _ => format!("Memory clock locked to {lo}–{hi} MHz."),
    })
}

/// Core clock cap. min defaults to the lowest clock (0 lets the driver pick),
/// so the GPU still idles down; only the top is held.
fn op_set_gpu_clocklock(p: &Obj) -> OpResult {
    let Some(hi) = p.get("max_mhz").and_then(is_int) else { return Err("max_mhz must be an integer".into()) };
    let lo = p.get("min_mhz").and_then(is_int).unwrap_or(0);
    if !(210..=4000).contains(&hi) || lo < 0 || lo > hi {
        return Err("max_mhz must be 210–4000 and min_mhz 0–max_mhz".into());
    }
    limits::set_gpu_locked_clocks(lo as u32, hi as u32, 0)?;
    Ok(format!("GPU core clock capped at {hi} MHz (until unlocked, reboot or driver reload)."))
}

fn op_reset_gpu_clocklock(_: &Obj) -> OpResult {
    limits::reset_gpu_locked_clocks(0)?;
    Ok("GPU core clock unlocked (back to the V/F curve's own maximum).".into())
}

fn op_reset_vram_memlock(_: &Obj) -> OpResult {
    limits::reset_mem_locked_clocks(0)?;
    Ok("Memory clock unlocked (returned to driver/P-state control).".into())
}

fn op_write_nvcurve_profile(p: &Obj) -> OpResult {
    let name = name_param(p, "name")?;
    let content = p.get("content").and_then(Value::as_str).ok_or("content must be a string")?;
    if content.len() > MAX_PROFILE_BYTES { return Err("content too large".into()); }
    let parsed: Value = serde_json::from_str(content).map_err(|e| format!("content is not valid JSON: {e}"))?;
    // Stricter than the Python helper (which only checked "is JSON"): the file
    // is later read and applied by root, so it gets the same schema check as
    // apply_gpu_offsets.
    let obj = parsed.as_object().ok_or("content must be a JSON object")?;
    validate_profile_data(obj).map_err(|e| format!("Invalid profile content: {e}"))?;
    ensure_dir(Path::new(PROFILES_DIR), 0o755).map_err(|e| format!("Could not prepare {PROFILES_DIR}: {e}"))?;
    let target = profile_file(name);
    write_bytes(&target, &serde_json::to_vec_pretty(&parsed).map_err(|e| e.to_string())?, 0o644).map_err(|e| format!("Could not write profile: {e}"))?;
    Ok(format!("Profile written: {}", target.display()))
}

fn op_delete_nvcurve_profile(p: &Obj) -> OpResult {
    let name = name_param(p, "name")?;
    let target = profile_file(name);
    if !target.is_file() { return Err(format!("Profile not found: {name}")); }
    std::fs::remove_file(&target).map_err(|e| format!("Could not delete profile: {e}"))?;
    let cleared = ops::clear_default_references(name).unwrap_or(false);
    let mut msg = format!("Profile '{name}' deleted.");
    if cleared { msg.push_str(" (it was the default profile — default cleared too.)"); }
    Ok(msg)
}

fn op_set_default_gpu_profile(p: &Obj) -> OpResult {
    let clear = p.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let name = if clear { None } else {
        let n = name_param(p, "name")?;
        if !profile_file(n).is_file() { return Err(format!("Profile not found: {n}")); }
        Some(n)
    };
    ops::set_default_profile(0, name)?;
    Ok(match name {
        None => "Auto-load profile cleared for GPU 0.".into(),
        Some(n) => format!("Auto-load profile set to '{n}' for GPU 0."),
    })
}

fn op_run_gpu_autoload(_: &Obj) -> OpResult {
    let code = run_autoload(&config());
    let text = logging::take_captured().join("\n");
    if code != 0 {
        return Err(if text.is_empty() { format!("autoload failed (code {code})") } else { text });
    }
    Ok(if text.is_empty() { "No default GPU profile configured.".into() } else { text })
}

fn dispatch(op: &str) -> Option<fn(&Obj) -> OpResult> {
    Some(match op {
        "read_gpu_curve" => op_read_gpu_curve,
        "apply_gpu_offsets" => op_apply_gpu_offsets,
        "apply_named_profile" => op_apply_named_profile,
        "reset_gpu_curve" => op_reset_gpu_curve,
        "set_vram_memlock" => op_set_vram_memlock,
        "reset_vram_memlock" => op_reset_vram_memlock,
        "set_gpu_clocklock" => op_set_gpu_clocklock,
        "reset_gpu_clocklock" => op_reset_gpu_clocklock,
        "write_nvcurve_profile" => op_write_nvcurve_profile,
        "delete_nvcurve_profile" => op_delete_nvcurve_profile,
        "set_default_gpu_profile" => op_set_default_gpu_profile,
        "run_gpu_autoload" => op_run_gpu_autoload,
        _ => return None,
    })
}

fn run() -> Value {
    if unsafe { libc::geteuid() } != 0 {
        return json!({"ok": false, "error": "root_helper must run as root"});
    }
    // pkexec passes the caller's umask through; pin it once for every op.
    unsafe { libc::umask(0o022) };
    logging::init(log::Level::Info, true);

    let mut raw = Vec::new();
    if let Err(e) = std::io::stdin().lock().take(MAX_STDIN_BYTES as u64 + 1).read_to_end(&mut raw) {
        return json!({"ok": false, "error": format!("failed to read stdin: {e}")});
    }
    if raw.len() > MAX_STDIN_BYTES { return json!({"ok": false, "error": "stdin payload too large"}); }
    let text = match String::from_utf8(raw) {
        Ok(t) => t,
        Err(e) => return json!({"ok": false, "error": format!("stdin is not valid UTF-8: {e}")}),
    };
    let payload: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("Invalid JSON on stdin: {e}")}),
    };
    let Some(obj) = payload.as_object() else {
        return json!({"ok": false, "error": "Payload must be a JSON object"});
    };
    let op = obj.get("op").and_then(Value::as_str).unwrap_or("");
    let Some(handler) = dispatch(op) else {
        return json!({"ok": false, "error": format!("Unknown or disallowed op: {}", obj.get("op").cloned().unwrap_or(Value::Null))});
    };
    match handler(obj) {
        Ok(m) => json!({"ok": true, "message": m}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

fn main() {
    let v = run();
    println!("{v}");
    std::process::exit(if v["ok"] == true { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn o(v: Value) -> Obj { v.as_object().unwrap().clone() }
    #[test]
    fn schema() {
        assert!(validate_profile_data(&o(json!({"name":"A (b)","curve_deltas":{"0":-15000,"126":0},
            "mem_offset_mhz":500,"power_limit_w":175,"gpu_name":"RTX","mem_locked_min_mhz":null}))).is_ok());
        for bad in [
            json!({"evil": 1}),
            json!({"power_limit_w": true}),
            json!({"power_limit_w": 99999}),
            json!({"mem_offset_mhz": 1.5}),
            json!({"curve_deltas": {"x": 1}}),
            json!({"curve_deltas": {"600": 1}}),
            json!({"curve_deltas": {"-1": 1}}),
            json!({"curve_deltas": {"1": 3000000}}),
            json!({"curve_deltas": {"1": false}}),
            json!({"mem_locked_min_mhz": 9000, "mem_locked_max_mhz": 8000}),
            json!({"name": "../x"}),
        ] {
            assert!(validate_profile_data(&o(bad.clone())).is_err(), "{bad}");
        }
    }
}
