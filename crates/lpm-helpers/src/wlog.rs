//! Diagnostic write log: one line per firmware/EC/EFI/platform-profile write.
//!
//! /var/log/legion-power-manager/writes.log (root helpers only; best effort,
//! never fails the caller; rotated to writes.log.1 at 512 KiB).
//! Line: UTC time | pid | this binary | parent < grandparent (pkexec uid) | kind | detail
//! Used to find which tool changes BIOS-side state (e.g. Legion Optimization
//! turning itself off): compare against the "boot-state"/"shutdown-state" lines.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

const DIR: &str = "/var/log/legion-power-manager";
const FILE: &str = "/var/log/legion-power-manager/writes.log";
const MAX: u64 = 512 * 1024;

fn proc_comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_owned()).unwrap_or_else(|_| "?".into())
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    s.rsplit(')').next()?.split_whitespace().nth(1)?.parse().ok()
}

fn utc_now() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&t, &mut tm);
        let mut buf = [0u8; 32];
        let n = libc::strftime(buf.as_mut_ptr() as *mut libc::c_char, buf.len(),
                               b"%Y-%m-%dT%H:%M:%SZ\0".as_ptr() as *const libc::c_char, &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

pub fn log(kind: &str, detail: &str) {
    let me = std::process::id();
    let exe = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "?".into());
    let pp = proc_ppid(me).unwrap_or(0);
    let gp = proc_ppid(pp).unwrap_or(0);
    let uid = std::env::var("PKEXEC_UID").map(|u| format!(" uid={u}")).unwrap_or_default();
    let d: String = detail.chars().map(|c| if c.is_control() { ' ' } else { c }).take(400).collect();
    let line = format!("{} | {} | {} | {}<{}{} | {} | {}\n", utc_now(), me, exe, proc_comm(pp), proc_comm(gp), uid, kind, d);
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o755).create(DIR);
    if std::fs::metadata(FILE).map_or(false, |m| m.len() > MAX) {
        let _ = std::fs::rename(FILE, format!("{FILE}.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(FILE) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Sysfs paths whose writes can change firmware/EC state.
pub fn interesting_sysfs(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    s.contains("firmware-attributes") || s.contains("platform_profile") || s.contains("platform-profile")
        || s.contains("/sys/devices/platform/") || s.contains("legion")
}
