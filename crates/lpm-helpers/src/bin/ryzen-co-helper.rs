//! Root helper for the Ryzen Curve Optimizer applet.
//! Port of ryzen_curve_optimizer_helper.py -- same JSON protocol:
//!   {"op": "set_coall", "params": {"value": -20}}
//!   {"op": "set_coper_batch", "params": {"entries": [{ccd,ccx,core,coper}, ...]}}
//!   {"op": "reset"}
//!
//! ryzenadj is resolved from a fixed list, must be root-owned and not
//! group/world-writable, runs with a cleared environment and a timeout.
//! Out-of-range values are rejected, never clamped.

use lpm_helpers::*;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::ffi::CString;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_STDIN_BYTES: usize = 64 * 1024;
const RYZENADJ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ENTRIES: usize = 64;
const MAX_OUTPUT: u64 = 64 * 1024;
const FIELD_MAX: i64 = 0xF;
const CO_MIN: i64 = -50;
const CO_MAX: i64 = 20;

const RYZENADJ_CANDIDATES: &[&str] = &[
    "/usr/bin/ryzenadj",
    "/usr/sbin/ryzenadj",
    "/usr/local/bin/ryzenadj",
    "/usr/local/sbin/ryzenadj",
    "/opt/ryzenadj/ryzenadj",
];

struct Invalid(String);

fn require_int(v: Option<&Value>, name: &str, lo: i64, hi: i64) -> Result<i64, Invalid> {
    let n = match v {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => n.as_i64(),
        Some(other) => {
            let t = match other {
                Value::Null => "NoneType", Value::Bool(_) => "bool", Value::Number(_) => "float",
                Value::String(_) => "str", Value::Array(_) => "list", Value::Object(_) => "dict",
            };
            return Err(Invalid(format!("{name} must be an integer, got {t}")));
        }
        None => return Err(Invalid(format!("{name} must be an integer, got NoneType"))),
    };
    match n {
        Some(n) if (lo..=hi).contains(&n) => Ok(n),
        _ => Err(Invalid(format!("{name} out of range: {} (allowed {lo}..{hi})",
            v.map(|x| x.to_string()).unwrap_or_default()))),
    }
}

fn is_safe_executable(path: &str) -> bool {
    let Ok(md) = std::fs::metadata(path) else { return false };
    if !md.is_file() || md.uid() != 0 { return false; }
    if md.permissions().mode() & 0o6022 != 0 { return false; }
    let Ok(c) = CString::new(path) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

fn find_ryzenadj() -> Option<&'static str> {
    RYZENADJ_CANDIDATES.iter().copied().find(|p| is_safe_executable(p))
}

fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(mut r) = r {
            let _ = (&mut r).take(MAX_OUTPUT).read_to_end(&mut s);
            let _ = std::io::copy(&mut r, &mut std::io::sink()); // keep the pipe drained
        }
        String::from_utf8_lossy(&s).trim().to_owned()
    })
}

fn run_ryzenadj(arg: &str) -> (bool, String) {
    let Some(bin) = find_ryzenadj() else {
        return (false, format!(
            "no usable ryzenadj binary found (searched: {}). It must exist, be root-owned \
             and not group/world-writable.", RYZENADJ_CANDIDATES.join(", ")));
    };
    let mut child = match Command::new(bin)
        .arg(arg)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, format!("failed to execute ryzenadj: {e}")),
    };
    let out_t = drain(child.stdout.take());
    let err_t = drain(child.stderr.take());

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= RYZENADJ_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return (false, format!("ryzenadj timed out after {}s", RYZENADJ_TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return (false, format!("failed to wait for ryzenadj: {e}")),
        }
    };
    let out = out_t.join().unwrap_or_default();
    let err = err_t.join().unwrap_or_default();

    if !status.success() {
        let msg = if !err.is_empty() { err } else if !out.is_empty() { out } else {
            format!("ryzenadj exited with code {}", status.code().unwrap_or(-1))
        };
        return (false, msg);
    }
    (true, if out.is_empty() { "OK".into() } else { out })
}

/// ((ccd << 4 | ccx) << 4 | core) << 20 | (coper & 0xFFFF)
fn encode_coper(ccd: i64, ccx: i64, core: i64, coper: i64) -> u64 {
    ((((ccd << 4 | ccx) << 4 | core) as u64) << 20) | ((coper as u64) & 0xFFFF)
}

fn op_set_coall(p: &Map<String, Value>) -> Result<Value, Invalid> {
    let v = require_int(p.get("value"), "value", CO_MIN, CO_MAX)?;
    let (ok, msg) = run_ryzenadj(&format!("--set-coall={v}"));
    Ok(json!({"ok": ok, "message": msg, "applied": {"coall": v}}))
}

fn op_set_coper_batch(p: &Map<String, Value>) -> Result<Value, Invalid> {
    let entries = match p.get("entries") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Err(Invalid("entries must be a non-empty list".into())),
    };
    if entries.len() > MAX_ENTRIES {
        return Err(Invalid(format!("too many entries: {} (max {MAX_ENTRIES})", entries.len())));
    }

    // Validate everything before touching the CPU: no half-applied curves.
    let mut validated = Vec::with_capacity(entries.len());
    let mut seen = HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        let e = e.as_object().ok_or_else(|| Invalid(format!("entry[{i}] must be an object")))?;
        let ccd = require_int(e.get("ccd"), &format!("entry[{i}].ccd"), 0, FIELD_MAX)?;
        let ccx = require_int(e.get("ccx"), &format!("entry[{i}].ccx"), 0, FIELD_MAX)?;
        let core = require_int(e.get("core"), &format!("entry[{i}].core"), 0, FIELD_MAX)?;
        let coper = require_int(e.get("coper"), &format!("entry[{i}].coper"), CO_MIN, CO_MAX)?;
        if !seen.insert((ccd, ccx, core)) {
            return Err(Invalid(format!("duplicate slot in batch: ccd={ccd} ccx={ccx} core={core}")));
        }
        validated.push((ccd, ccx, core, coper));
    }

    let mut all_ok = true;
    let results: Vec<Value> = validated.into_iter().map(|(ccd, ccx, core, coper)| {
        let (ok, msg) = run_ryzenadj(&format!("--set-coper={}", encode_coper(ccd, ccx, core, coper)));
        all_ok &= ok;
        json!({"ok": ok, "message": msg, "ccd": ccd, "ccx": ccx, "core": core, "coper": coper})
    }).collect();
    Ok(json!({"ok": all_ok, "results": results}))
}

fn op_reset(_: &Map<String, Value>) -> Result<Value, Invalid> {
    let (ok, msg) = run_ryzenadj("--set-coall=0");
    Ok(json!({"ok": ok, "message": msg}))
}

fn run() -> Value {
    let req = match read_request(MAX_STDIN_BYTES) { Ok(v) => v, Err(e) => return e };
    let Some(obj) = req.as_object() else {
        return json!({"ok": false, "error": "payload must be a JSON object"});
    };
    let op = obj.get("op");
    let handler: fn(&Map<String, Value>) -> Result<Value, Invalid> =
        match op.and_then(Value::as_str) {
            Some("set_coall") => op_set_coall,
            Some("set_coper_batch") => op_set_coper_batch,
            Some("reset") => op_reset,
            _ => return json!({"ok": false, "error": format!("unknown op: {}",
                     op.map(|v| v.to_string()).unwrap_or_else(|| "None".into()))}),
        };
    let empty = Map::new();
    let params = match obj.get("params") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return json!({"ok": false, "error": "params must be a JSON object"}),
    };
    handler(params).unwrap_or_else(|Invalid(m)| json!({"ok": false, "error": format!("invalid request: {m}")}))
}

fn main() {
    std::process::exit(finish(run()));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoding_matches_python() {
        // Python: (((1<<4|0)<<4|3)<<20) | (-20 & 0xFFFF)
        assert_eq!(encode_coper(1, 0, 3, -20), 0x10300000 | 0xFFEC);
        assert_eq!(encode_coper(0, 0, 0, 5), 5);
        assert_eq!(encode_coper(15, 15, 15, -50), 0xFFF00000 | 0xFFCE);
    }
    #[test]
    fn ints_strict() {
        assert!(require_int(Some(&json!(true)), "v", -50, 20).is_err());
        assert!(require_int(Some(&json!(-20.0)), "v", -50, 20).is_err());
        assert!(require_int(Some(&json!(21)), "v", -50, 20).is_err());
        assert_eq!(require_int(Some(&json!(-50)), "v", -50, 20).ok(), Some(-50));
    }
    #[test]
    fn duplicate_rejected_before_exec() {
        let p = json!({"entries": [{"ccd":0,"ccx":0,"core":1,"coper":-10},
                                    {"ccd":0,"ccx":0,"core":1,"coper":-5}]});
        assert!(op_set_coper_batch(p.as_object().unwrap()).is_err());
    }
}
