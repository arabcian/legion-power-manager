//! Shared plumbing for the Legion Power Manager root helpers.
//!
//! Every helper follows the same contract as the Python originals:
//! one bounded JSON request on stdin, one JSON line on stdout,
//! exit 0 on success / 1 on failure. Nothing from argv or the
//! environment is ever trusted.

use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::path::Path;

pub mod intel_uv;
pub mod intel_uv_daemon;
pub mod tune;

/// Reads at most `max` bytes from stdin. Returns Err with a ready-made
/// JSON error payload if the input is too large, not UTF-8, or not JSON.
pub fn read_request(max: usize) -> Result<Value, Value> {
    let mut buf = Vec::with_capacity(1024);
    let stdin = io::stdin();
    let mut lock = stdin.lock().take(max as u64 + 1);
    if let Err(e) = lock.read_to_end(&mut buf) {
        return Err(json!({"ok": false, "error": format!("failed to read stdin: {e}")}));
    }
    if buf.len() > max {
        return Err(json!({"ok": false, "error": "payload too large"}));
    }
    let text = std::str::from_utf8(&buf)
        .map_err(|e| json!({"ok": false, "error": format!("invalid JSON: {e}")}))?;
    serde_json::from_str(text)
        .map_err(|e| json!({"ok": false, "error": format!("invalid JSON: {e}")}))
}

/// Writes one JSON line to stdout and flushes.
pub fn emit(v: &Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

/// Emits and converts `ok` into a process exit code.
pub fn finish(v: Value) -> i32 {
    emit(&v);
    if v.get("ok").and_then(Value::as_bool).unwrap_or(false) { 0 } else { 1 }
}

/// Writes `data` to a sysfs attribute with a single write() syscall.
/// sysfs store handlers parse one buffer per write and must not be fed
/// in pieces, so a short write is reported as an error instead of retried.
pub fn sysfs_write(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?; // callers pass the canonical path; a symlink here is refused

    let n = f.write(data)?;
    if n != data.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short write to sysfs"));
    }
    Ok(())
}

pub fn read_trimmed(path: &Path) -> io::Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_owned())
}

/// Canonicalises `path` and requires the result to stay inside /sys.
/// (A changed realpath is normal: /sys/class/* entries are kernel symlinks
/// into /sys/devices/*. Only leaving sysfs is an escape.)
pub fn canonical_in_sysfs(path: &Path) -> Option<std::path::PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    if real.starts_with("/sys") && real != Path::new("/sys") { Some(real) } else { None }
}
