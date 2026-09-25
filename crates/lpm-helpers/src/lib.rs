//! Shared plumbing for the Legion Power Manager root helpers.
//!
//! Every helper follows the same contract as the Python originals:
//! one bounded JSON request on stdin, one JSON line on stdout,
//! exit 0 on success / 1 on failure. Nothing from argv or the
//! environment is ever trusted.

use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub mod bootguard;
pub mod intel_uv;
pub mod intel_uv_daemon;
pub mod legion_wmi;
pub mod tune;

/// Process-wide setup every root helper runs first.
///
/// pkexec passes the caller's umask through untouched: a user who runs
/// `umask 000; pkexec <helper>` would otherwise get world-writable
/// directories and files created by root (e.g. /etc/legion-power-manager),
/// which is a straight path to planting a boot profile. Also disables core
/// dumps, so a crash never leaves root memory (MSR/EC state, profiles) in a
/// file some other tool might pick up.
pub fn init() {
    unsafe {
        libc::umask(0o022);
        let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
    }
}

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

// ── root-owned state files ─────────────────────────────────────────────────

/// Creates (or verifies) a root-owned, non-symlink directory that is not
/// group/other-writable. Mode is set explicitly, never left to the umask.
pub fn secure_dir(p: &str) -> Result<(), String> {
    match std::fs::DirBuilder::new().mode(0o755).create(p) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{p}: {e}")),
    }
    let m = std::fs::symlink_metadata(p).map_err(|e| format!("{p}: {e}"))?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(format!("{p} is not a root-owned private directory; refusing to use it"));
    }
    Ok(())
}

/// Reads a root-owned, not group/other-writable regular file (no symlinks),
/// at most `max` bytes. The checks run on the opened descriptor, so the file
/// cannot be swapped between the check and the read.
pub fn read_root_file(p: &str, max: u64) -> Option<String> {
    let f = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(p).ok()?;
    let m = f.metadata().ok()?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 || m.len() > max { return None; }
    let mut s = String::new();
    (&f).take(max + 1).read_to_string(&mut s).ok()?;
    (s.len() as u64 <= max).then_some(s)
}

/// Atomic root-owned write: tmp (O_EXCL, no symlink, 0644) + fsync + rename
/// + fsync(dir). A reader (boot service, GUI) sees the old file or the new
/// one, never a truncated one — also across a power cut.
pub fn write_root_file(p: &str, body: &[u8]) -> Result<(), String> {
    let path = Path::new(p);
    let dir = path.parent().ok_or_else(|| format!("{p}: no parent directory"))?;
    let tmp = format!("{p}.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&tmp)
        .map_err(|e| format!("{tmp}: {e}"))?;
    let res = f.write_all(body)
        .and_then(|_| f.set_permissions(std::fs::Permissions::from_mode(0o644)))
        .and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{tmp}: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{p}: {e}"));
    }
    if let Ok(d) = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC).open(dir) { let _ = d.sync_all(); }
    Ok(())
}

/// True if `path` and every directory above it is owned by root and not
/// group/other-writable (a sticky world-writable dir like /tmp fails too),
/// and the file itself carries no setuid/setgid bit. Used before exec'ing a
/// third-party binary as root: a user-owned /opt/<tool>/ directory would
/// otherwise let the user swap the binary between the check and the exec.
pub fn trusted_path(path: &Path) -> bool {
    let Ok(real) = std::fs::canonicalize(path) else { return false };
    let Ok(md) = std::fs::metadata(&real) else { return false };
    if !md.is_file() || md.uid() != 0 || md.mode() & 0o6022 != 0 { return false; }
    let mut dir = real.parent();
    while let Some(d) = dir {
        match std::fs::metadata(d) {
            Ok(m) if m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0 => {}
            _ => return false,
        }
        dir = d.parent();
    }
    true
}

/// Exclusive advisory lock on an already-open file, released on drop.
/// Serialises multi-step hardware transactions (MSR mailbox write → read,
/// acpi_call write → read) between our own helpers and the daemon, which
/// would otherwise be able to interleave and read each other's replies.
pub struct FdLock(libc::c_int);
impl FdLock {
    pub fn exclusive<F: AsRawFd>(f: &F) -> io::Result<FdLock> {
        let fd = f.as_raw_fd();
        loop {
            if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 { return Ok(FdLock(fd)); }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted { return Err(e); }
        }
    }
}
impl Drop for FdLock {
    fn drop(&mut self) { unsafe { libc::flock(self.0, libc::LOCK_UN); } }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trusted_path_rejects_tmp() {
        let p = std::env::temp_dir().join(format!("lpm-trust-{}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        // /tmp is world-writable (sticky): never a trusted parent.
        assert!(!trusted_path(&p));
        std::fs::remove_file(&p).unwrap();
        assert!(!trusted_path(Path::new("/nonexistent/lpm")));
    }
}
