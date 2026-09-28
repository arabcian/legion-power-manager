//! Health monitor: hardware/driver faults from the kernel log (/dev/kmsg)
//! plus the PCIe AER counters in sysfs.
//!
//! Reported: NVIDIA Xid (with what the code means; Xid 154 is only the
//! recovery action for the Xid before it), GSP timeouts, machine checks,
//! PCIe AER errors, soft/hard lockups and RCU stalls, amdgpu ring timeouts.

use serde_json::{json, Value};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

const MAX_EVENTS: usize = 300;

/// (code, level, meaning). level: critical = the GPU needs a reset/reboot.
const XIDS: &[(u32, &str, &str)] = &[
    (8, "error", "GPU stopped processing (channel timeout)"),
    (13, "error", "Graphics engine exception (usually the application or an unstable clock)"),
    (31, "error", "GPU memory page fault (application, driver or unstable clock)"),
    (32, "error", "Invalid or corrupted push buffer stream"),
    (38, "error", "Driver firmware error"),
    (43, "warn", "GPU stopped processing a channel (application fault)"),
    (45, "warn", "Channel killed by the driver (follows another error or process exit)"),
    (48, "critical", "Double-bit ECC error"),
    (56, "error", "Display engine error"),
    (57, "error", "Error programming video memory interface"),
    (61, "critical", "Internal micro-controller breakpoint/warning (PMU)"),
    (62, "critical", "Internal micro-controller halt (PMU/GSP)"),
    (63, "critical", "ECC page retirement / row remapping event"),
    (64, "critical", "ECC page retirement / row remapper failure"),
    (68, "error", "Video processor exception"),
    (69, "error", "Graphics engine class error"),
    (74, "critical", "NVLink error"),
    (79, "critical", "GPU has fallen off the bus (power, PCIe link or firmware hang)"),
    (92, "critical", "High single-bit ECC error rate"),
    (94, "critical", "Contained ECC error"),
    (95, "critical", "Uncontained ECC error"),
    (109, "error", "Context switch timeout"),
    (119, "critical", "GSP RPC timeout (GPU firmware stopped answering)"),
    (120, "critical", "GSP firmware error"),
    (121, "critical", "C2C link error"),
    (140, "critical", "Unrecovered ECC error"),
    (143, "critical", "GPU initialization failure"),
    (154, "critical", "Recovery action required — the cause is the Xid logged just before"),
    (175, "critical", "GSP RPC timeout (extended)"),
];

pub fn xid_info(code: u32) -> (&'static str, &'static str) {
    XIDS.iter().find(|x| x.0 == code).map(|x| (x.1, x.2)).unwrap_or(("error", "NVIDIA driver error (see NVIDIA's Xid catalog)"))
}

/// Classifies one kernel log message; None = not interesting.
pub fn classify(msg: &str) -> Option<Value> {
    let clip = |s: &str| s.chars().take(300).collect::<String>();
    if let Some(i) = msg.find("NVRM: Xid (") {
        let rest = &msg[i..];
        let after = rest.find("): ").map(|j| &rest[j + 3..])?;
        let code: u32 = after.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
        let (level, meaning) = xid_info(code);
        let mut title = format!("Xid {code}: {meaning}");
        if code == 154 {
            if let Some(k) = after.find(" to ") { title = format!("Xid 154: recovery action → {}", clip(&after[k + 4..])); }
        }
        return Some(json!({"kind": "xid", "code": code, "level": level, "title": title, "text": clip(msg)}));
    }
    let has = |s: &str| msg.contains(s);
    let ev = |kind: &str, level: &str, title: &str| Some(json!({"kind": kind, "level": level, "title": title, "text": clip(msg)}));
    if has("NVRM") && (has("heartbeat timed out") || has("GSP Timeout") || has("_kgspIsHeartbeatTimedOut")) {
        return ev("gsp", "critical", "NVIDIA GSP firmware timeout");
    }
    if has("[Hardware Error]") || has("Machine check events logged") || has("Machine Check Exception") {
        let level = if has("Corrected") || has("events logged") { "warn" } else { "critical" };
        return ev("mce", level, "CPU machine check (unstable Curve Optimizer / RAM timings, or hardware)");
    }
    if has("AER:") || has("PCIe Bus Error") {
        if has("Corrected error") || has("severity=Correct") { return ev("aer", "warn", "PCIe corrected error"); }
        if has("Uncorrect") || has("Fatal") || has("severity=") { return ev("aer", "critical", "PCIe uncorrectable error"); }
        return None;
    }
    if has("soft lockup") || has("hard LOCKUP") || has("rcu_preempt detected stalls") || has("rcu_sched detected stalls")
        || has("self-detected stall") {
        return ev("lockup", "critical", "CPU lockup / RCU stall");
    }
    if has("amdgpu") && ((has("ring") && has("timeout")) || has("GPU reset begin")) {
        return ev("amdgpu", "error", "AMD iGPU hang / reset");
    }
    None
}

/// Reads every /dev/kmsg record with seq > since. Err("EPERM") when the caller
/// may not read the kernel log (kernel.dmesg_restrict).
fn kmsg_events(since: u64) -> Result<(Vec<Value>, u64), String> {
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open("/dev/kmsg").map_err(|e| if e.raw_os_error() == Some(libc::EPERM) || e.raw_os_error() == Some(libc::EACCES) { "EPERM".to_string() } else { format!("/dev/kmsg: {e}") })?;
    let (mut out, mut last) = (Vec::new(), since);
    let mut buf = vec![0u8; 8192];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let rec = String::from_utf8_lossy(&buf[..n]);
                let Some((head, body)) = rec.split_once(';') else { continue };
                let mut h = head.split(',');
                let _pri = h.next();
                let Some(seq) = h.next().and_then(|s| s.parse::<u64>().ok()) else { continue };
                let ts: u64 = h.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                last = last.max(seq);
                if seq <= since { continue; }
                let msg = body.lines().next().unwrap_or("");
                if let Some(mut e) = classify(msg) {
                    e["seq"] = json!(seq);
                    e["ts_us"] = json!(ts);
                    out.push(e);
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,  // record overwritten meanwhile
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("/dev/kmsg: {e}")),
        }
    }
    if out.len() > MAX_EVENTS { out.drain(..out.len() - MAX_EVENTS); }
    Ok((out, last))
}

/// PCIe devices with a non-zero AER total (sysfs, readable by anyone).
fn aer_counters() -> Vec<Value> {
    let total = |p: std::path::PathBuf, key: &str| -> u64 {
        std::fs::read_to_string(p).ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix(key)?.trim().parse().ok())).unwrap_or(0)
    };
    let mut v = Vec::new();
    for e in std::fs::read_dir("/sys/bus/pci/devices").into_iter().flatten().flatten() {
        let d = e.path();
        if !d.join("aer_dev_correctable").is_file() { continue; }
        let cor = total(d.join("aer_dev_correctable"), "TOTAL_ERR_COR");
        let nonfatal = total(d.join("aer_dev_nonfatal"), "TOTAL_ERR_NONFATAL");
        let fatal = total(d.join("aer_dev_fatal"), "TOTAL_ERR_FATAL");
        if cor + nonfatal + fatal == 0 { continue; }
        let class = std::fs::read_to_string(d.join("class")).unwrap_or_default();
        let vendor = std::fs::read_to_string(d.join("vendor")).unwrap_or_default();
        let what = match (vendor.trim(), class.trim().get(..6)) {
            ("0x10de", Some("0x0300")) | ("0x10de", Some("0x0302")) => "NVIDIA GPU",
            (_, Some("0x0604")) => "PCIe port",
            (_, Some("0x0108")) => "NVMe",
            (_, Some("0x0280")) => "Wi-Fi",
            _ => "device",
        };
        v.push(json!({"dev": e.file_name().to_string_lossy(), "what": what, "cor": cor, "nonfatal": nonfatal, "fatal": fatal}));
    }
    v
}

/// {"ok", "events", "last_seq", "boot_id", "aer"} or {"ok": false, "needs_root": true}.
pub fn scan(since: u64) -> Value {
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default().trim().to_owned();
    match kmsg_events(since) {
        Ok((events, last)) => json!({"ok": true, "events": events, "last_seq": last, "boot_id": boot_id, "aer": aer_counters()}),
        Err(e) if e == "EPERM" => json!({"ok": false, "needs_root": true, "error": "the kernel log is restricted (kernel.dmesg_restrict=1)"}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xid_lines() {
        let e = classify("NVRM: Xid (PCI:0000:01:00): 79, pid='<unknown>', name=<unknown>, GPU has fallen off the bus.").unwrap();
        assert_eq!(e["code"], 79);
        assert_eq!(e["level"], "critical");
        let e = classify("NVRM: Xid (PCI:0000:01:00): 154, GPU recovery action changed from 0x0 (None) to 0x2 (Node Reboot Required)").unwrap();
        assert_eq!(e["code"], 154);
        assert!(e["title"].as_str().unwrap().contains("Node Reboot Required"));
        assert!(classify("NVRM: loading NVIDIA UNIX Open Kernel Module").is_none());
        assert_eq!(classify("pcieport 0000:00:01.1: AER: Corrected error message received from 0000:01:00.0").unwrap()["kind"], "aer");
        assert_eq!(classify("mce: [Hardware Error]: Machine check events logged").unwrap()["level"], "warn");
        assert_eq!(classify("watchdog: BUG: soft lockup - CPU#3 stuck for 22s!").unwrap()["kind"], "lockup");
    }
}
