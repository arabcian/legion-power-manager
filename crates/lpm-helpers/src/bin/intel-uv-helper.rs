//! Root helper for the Intel Undervolt tab (pkexec target, same contract as
//! the other helpers: one JSON request on stdin, one JSON line on stdout).
//!   {"op": "status"}
//!   {"op": "probe_uv_lock"}   1-tick write test of the Core offset, restored
//!   {"op": "apply",     "profile": {...}}   validated whole before any write
//!   {"op": "reset"}                          all five voltage planes → 0 mV
//!   {"op": "set_boot",  "profile": {...}}   store the boot/resume profile
//!   {"op": "clear_boot"}
//!   {"op": "set_boot",  "config": {"ac": {...}|null, "battery": {...}|null, "daemon": {...}}}
//!   {"op": "boot"}                           apply the stored profile for the current power source
//!   {"op": "monitor",   "clear_logs": bool} one throttle/VCore/energy sample
//!   {"op": "monitor_stream", "interval_ms": n, "clear_logs": bool}
//!       one sample line every interval (500..10000 ms) until stdin reaches
//!       EOF or stdout goes away; a "clear" line on stdin clears the sticky
//!       log bits with the next sample. Replaces one pkexec round trip
//!       (fork, polkit D-Bus check, exec) per 2 s sample with one per session.
//!       The request line must end with '\n' so stdin can stay open.
//! Profile format: see intel_uv::parse_profile.

use lpm_helpers::intel_uv::{self, parse_profile};
use lpm_helpers::intel_uv_daemon::{apply_boot, parse_boot};
use lpm_helpers::*;
use serde_json::{json, Value};
use std::path::Path;

const MAX_STDIN_BYTES: usize = 16 * 1024;
const BOOT_DIR: &str = "/etc/legion-power-manager";
const BOOT_FILE: &str = "/etc/legion-power-manager/intel-uv-boot.json";

/// The boot profile is applied by root at every boot and resume, so it lives
/// in a verified root-owned directory and is replaced atomically (the old
/// version used create_dir_all under the caller's umask: `umask 000; pkexec`
/// left /etc/legion-power-manager world-writable).
fn write_boot(profile: &Value) -> Result<(), String> {
    secure_dir(BOOT_DIR)?;
    let body = serde_json::to_vec_pretty(profile).map_err(|e| e.to_string())?;
    write_root_file(BOOT_FILE, &body)
}

/// The request: everything up to the first newline, or to EOF (the other
/// callers write one JSON object and close stdin). Raw read(2) on fd 0 — no
/// std buffering, so bytes after the newline stay in the pipe for the stream.
fn read_request_line() -> Result<Value, Value> {
    let mut buf = Vec::with_capacity(256);
    let mut b = [0u8; 1];
    loop {
        let n = unsafe { libc::read(0, b.as_mut_ptr().cast(), 1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted { continue; }
            return Err(json!({"ok": false, "error": "failed to read stdin"}));
        }
        if n == 0 || b[0] == b'\n' { break; }
        buf.push(b[0]);
        if buf.len() > MAX_STDIN_BYTES { return Err(json!({"ok": false, "error": "payload too large"})); }
    }
    let text = std::str::from_utf8(&buf).map_err(|e| json!({"ok": false, "error": format!("invalid JSON: {e}")}))?;
    serde_json::from_str(text).map_err(|e| json!({"ok": false, "error": format!("invalid JSON: {e}")}))
}

/// One line to stdout; false once the reader is gone (EPIPE: Rust ignores SIGPIPE).
fn emit_line(v: &Value) -> bool {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").and_then(|_| out.flush()).is_ok()
}

/// Waits up to `ms` for stdin. Returns None on EOF/hang-up (stop), else
/// whether a "clear" command arrived.
fn wait_stdin(ms: u64, pending: &mut Vec<u8>) -> Option<bool> {
    let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    let mut clear = false;
    loop {
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() { return Some(clear); }
        let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut p, 1, left.as_millis() as libc::c_int) };
        if r < 0 { if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted { continue; } return None; }
        if r == 0 { return Some(clear); }
        let mut b = [0u8; 256];
        let n = unsafe { libc::read(0, b.as_mut_ptr().cast(), b.len()) };
        if n <= 0 { return None; }  // EOF: the tab was hidden or the GUI exited
        pending.extend_from_slice(&b[..n as usize]);
        while let Some(i) = pending.iter().position(|&c| c == b'\n') {
            let line: Vec<u8> = pending.drain(..=i).collect();
            clear |= line.starts_with(b"clear");
        }
        if pending.len() > 4096 { pending.clear(); }  // no newline in sight: garbage, dropped
    }
}

fn monitor_stream(obj: &serde_json::Map<String, Value>) -> Value {
    let msr = match intel_uv::Msr::open(true) {
        Ok(m) => m,
        Err(e) => return json!({"ok": false, "error": format!("/dev/cpu/0/msr: {e}")}),
    };
    let interval = obj.get("interval_ms").and_then(Value::as_u64).unwrap_or(2000).clamp(500, 10_000);
    let mut clear = obj.get("clear_logs").and_then(Value::as_bool).unwrap_or(false);
    let mut pending = Vec::new();
    // Hard ceiling: a GUI that hangs with the pipe open cannot keep a root
    // process sampling MSRs forever (the tab restarts the stream if needed).
    let stop_at = std::time::Instant::now() + std::time::Duration::from_secs(4 * 3600);
    loop {
        if !emit_line(&intel_uv::monitor_sample(&msr, std::mem::take(&mut clear))) { break; }
        if std::time::Instant::now() >= stop_at { break; }
        match wait_stdin(interval, &mut pending) {
            Some(c) => clear = c,
            None => break,
        }
    }
    json!({"ok": true, "stream_end": true})
}

fn run() -> Value {
    let req = match read_request_line() { Ok(v) => v, Err(e) => return e };
    let Some(obj) = req.as_object() else { return json!({"ok": false, "error": "payload must be a JSON object"}) };
    let profile = || -> Result<(Value, intel_uv::Profile), Value> {
        let v = obj.get("profile").cloned().unwrap_or(Value::Null);
        parse_profile(&v).map(|p| (v, p)).map_err(|e| json!({"ok": false, "error": format!("invalid profile: {e}")}))
    };
    let mut out = match obj.get("op").and_then(Value::as_str) {
        Some("status") => intel_uv::read_status(),
        Some("probe_uv_lock") => intel_uv::probe_uv_lock(),
        Some("probe_fabric") => intel_uv::probe_fabric(),
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
        Some("monitor_stream") => return monitor_stream(obj),
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

fn main() { init(); std::process::exit(finish(run())); }
