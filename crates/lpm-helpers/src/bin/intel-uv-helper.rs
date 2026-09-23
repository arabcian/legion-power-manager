//! Root helper for the Intel Undervolt tab (pkexec target, same contract as
//! the other helpers: one JSON request on stdin, one JSON line on stdout).
//!   {"op": "status"}
//!   {"op": "apply",     "profile": {...}}   validated whole before any write
//!   {"op": "reset"}                          all five voltage planes → 0 mV
//!   {"op": "set_boot",  "profile": {...}}   store the boot/resume profile
//!   {"op": "clear_boot"}
//!   {"op": "set_boot",  "config": {"ac": {...}|null, "battery": {...}|null, "daemon": {...}}}
//!   {"op": "boot"}                           apply the stored profile for the current power source
//!   {"op": "monitor",   "clear_logs": bool} one throttle/VCore/energy sample
//! Profile format: see intel_uv::parse_profile.

use lpm_helpers::intel_uv::{self, parse_profile};
use lpm_helpers::intel_uv_daemon::{apply_boot, parse_boot};
use lpm_helpers::*;
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MAX_STDIN_BYTES: usize = 16 * 1024;
const BOOT_DIR: &str = "/etc/legion-power-manager";
const BOOT_FILE: &str = "/etc/legion-power-manager/intel-uv-boot.json";

fn write_boot(profile: &Value) -> Result<(), String> {
    std::fs::create_dir_all(BOOT_DIR).map_err(|e| format!("{BOOT_DIR}: {e}"))?;
    let tmp = format!("{BOOT_FILE}.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&tmp).map_err(|e| format!("{tmp}: {e}"))?;
    let body = serde_json::to_string_pretty(profile).unwrap_or_default();
    f.write_all(body.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| format!("{tmp}: {e}"))?;
    std::fs::rename(&tmp, BOOT_FILE).map_err(|e| format!("{BOOT_FILE}: {e}"))
}

fn run() -> Value {
    let req = match read_request(MAX_STDIN_BYTES) { Ok(v) => v, Err(e) => return e };
    let Some(obj) = req.as_object() else { return json!({"ok": false, "error": "payload must be a JSON object"}) };
    let profile = || -> Result<(Value, intel_uv::Profile), Value> {
        let v = obj.get("profile").cloned().unwrap_or(Value::Null);
        parse_profile(&v).map(|p| (v, p)).map_err(|e| json!({"ok": false, "error": format!("invalid profile: {e}")}))
    };
    let mut out = match obj.get("op").and_then(Value::as_str) {
        Some("status") => intel_uv::read_status(),
        Some("apply") => match profile() { Ok((_, p)) => intel_uv::apply(&p), Err(e) => e },
        Some("reset") => intel_uv::apply(&intel_uv::reset_profile()),
        Some("set_boot") => {
            let cfg = match obj.get("config") {
                Some(c) => parse_boot(c).map(|_| c.clone()).map_err(|e| json!({"ok": false, "error": format!("invalid config: {e}")})),
                None => profile().map(|(v, _)| v),
            };
            match cfg {
                Ok(v) => match write_boot(&v) { Ok(()) => json!({"ok": true, "message": format!("saved {BOOT_FILE}")}),
                                                Err(e) => json!({"ok": false, "error": e}) },
                Err(e) => e,
            }
        }
        Some("monitor") => match intel_uv::Msr::open(true) {
            Ok(m) => intel_uv::monitor_sample(&m, obj.get("clear_logs").and_then(Value::as_bool).unwrap_or(false)),
            Err(e) => json!({"ok": false, "error": format!("/dev/cpu/0/msr: {e}")}),
        },
        Some("clear_boot") => match std::fs::remove_file(BOOT_FILE) {
            Ok(()) => json!({"ok": true, "message": "boot profile removed"}),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({"ok": true, "message": "no boot profile"}),
            Err(e) => json!({"ok": false, "error": format!("{BOOT_FILE}: {e}")}),
        },
        Some("boot") => apply_boot(),
        other => json!({"ok": false, "error": format!("unknown op: {}", other.unwrap_or("None"))}),
    };
    if out.is_object() { out["boot_profile"] = json!(Path::new(BOOT_FILE).is_file()); }
    out
}

fn main() { std::process::exit(finish(run())); }
