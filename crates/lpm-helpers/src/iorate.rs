//! Sustained write rate of the disk(s) that receive the page cache's dirty data.
//!
//! Autotune sizes vm.dirty_bytes / dirty_background_bytes in *seconds of
//! writeback* (rate x window), so the rate has to come from this machine:
//!   1. probe   - root: a bounded O_DIRECT write (<= 512 MiB, <= 4 s) into an
//!                unnamed O_TMPFILE on the root file system, result kept in
//!                /var/lib/legion-power-manager/io-probe.json (0644, root-owned)
//!   2. passive - bytes moved / io_ticks from /sys/block/<disk>/stat: the average
//!                rate while the disk was busy since boot (a lower bound)
//!   3. class   - conservative sustained figure for the device type
//! Every figure is clamped to its device class's plausible range.

use serde_json::{json, Value};
use std::path::Path;

pub const PROBE_DIR: &str = "/var/lib/legion-power-manager";
pub const PROBE_FILE: &str = "/var/lib/legion-power-manager/io-probe.json";
const MIB: u64 = 1 << 20;
const PROBE_MAX_BYTES: u64 = 512 * MIB;
const PROBE_MAX_SECS: f64 = 4.0;
const PROBE_BLOCK: usize = 1 << 20;
/// Probe results older than this are ignored (firmware / drive changes).
const PROBE_MAX_AGE_S: u64 = 180 * 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source { Probe, Passive, Class }

impl Source {
    pub fn as_str(self) -> &'static str {
        match self { Source::Probe => "measured (probe)", Source::Passive => "observed since boot", Source::Class => "device-class estimate" }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevClass { Nvme, Ssd, Hdd, Mmc, Other }

impl DevClass {
    /// Conservative sustained write rate (after SLC cache / on a busy disk).
    pub fn default_bps(self) -> u64 {
        match self { DevClass::Nvme => 800 * MIB, DevClass::Ssd => 300 * MIB, DevClass::Hdd => 100 * MIB,
                     DevClass::Mmc => 40 * MIB, DevClass::Other => 100 * MIB }
    }
    /// Plausible range; any measurement is clamped into it.
    pub fn range(self) -> (u64, u64) {
        match self { DevClass::Nvme => (200 * MIB, 8192 * MIB), DevClass::Ssd => (80 * MIB, 700 * MIB),
                     DevClass::Hdd => (30 * MIB, 300 * MIB), DevClass::Mmc => (10 * MIB, 300 * MIB),
                     DevClass::Other => (10 * MIB, 8192 * MIB) }
    }
    pub fn as_str(self) -> &'static str {
        match self { DevClass::Nvme => "NVMe", DevClass::Ssd => "SATA SSD", DevClass::Hdd => "HDD", DevClass::Mmc => "eMMC/SD", DevClass::Other => "disk" }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IoRate { pub bps: u64, pub source: Source, pub device: String, pub class: DevClass }

impl IoRate {
    pub fn class_default(class: DevClass, device: &str) -> IoRate {
        IoRate { bps: class.default_bps(), source: Source::Class, device: device.to_owned(), class }
    }
    pub fn to_json(&self) -> Value {
        json!({"mib_s": self.bps / MIB, "source": self.source.as_str(), "device": self.device, "class": self.class.as_str()})
    }
    pub fn summary(&self) -> String {
        format!("{} {} MiB/s write ({})", self.device, self.bps / MIB, self.source.as_str())
    }
}

fn rd(p: impl AsRef<Path>) -> Option<String> { crate::read_trimmed(p.as_ref()).ok() }

pub fn class_of(disk: &str) -> DevClass {
    if disk.starts_with("nvme") { return DevClass::Nvme; }
    if disk.starts_with("mmcblk") { return DevClass::Mmc; }
    match rd(format!("/sys/block/{disk}/queue/rotational")).as_deref() {
        Some("1") => DevClass::Hdd,
        Some("0") if disk.starts_with("sd") => DevClass::Ssd,
        _ => DevClass::Other,
    }
}

/// Whole disks under a block device node, through partitions and dm/md stacks.
pub fn whole_disks(dev: &str) -> Vec<String> {
    fn walk(name: &str, out: &mut Vec<String>, depth: u32) {
        if depth > 8 { return; }
        let Ok(node) = std::fs::canonicalize(Path::new("/sys/class/block").join(name)) else { return };
        let slaves: Vec<String> = std::fs::read_dir(node.join("slaves")).into_iter().flatten().flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        if !slaves.is_empty() {
            for s in slaves { walk(&s, out, depth + 1); }
            return;
        }
        let disk = if node.join("partition").is_file() { node.parent().map(Path::to_path_buf) } else { Some(node) };
        if let Some(n) = disk.and_then(|d| d.file_name().map(|x| x.to_string_lossy().into_owned())) {
            if !out.contains(&n) { out.push(n); }
        }
    }
    let mut out = Vec::new();
    let name = Path::new(dev).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let real = std::fs::canonicalize(dev).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    walk(real.as_deref().unwrap_or(&name), &mut out, 0);
    out
}

/// Disks behind the file systems that take ordinary writes (/, /home, /var).
pub fn target_disks() -> Vec<String> {
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let mut out = Vec::new();
    for l in mounts.lines() {
        let mut f = l.split_whitespace();
        let (Some(dev), Some(mp)) = (f.next(), f.next()) else { continue };
        if !["/", "/home", "/var"].contains(&mp) || !dev.starts_with("/dev/") { continue; }
        for d in whole_disks(dev) { if !out.contains(&d) { out.push(d); } }
    }
    out
}

/// Average rate while busy, from one /sys/block/<disk>/stat line.
/// None without enough history (>= 2 GiB written and >= 2 s busy).
pub fn passive_bps(stat: &str) -> Option<u64> {
    let f: Vec<u64> = stat.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if f.len() < 10 { return None; }
    let (rsect, wsect, io_ticks) = (f[2], f[6], f[9]);
    if wsect * 512 < 2 << 30 || io_ticks < 2000 { return None; }
    Some((rsect + wsect) * 512 * 1000 / io_ticks)
}

fn load_probe() -> Option<IoRate> {
    let text = crate::read_root_file(PROBE_FILE, 8192)?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let dev = v["device"].as_str()?.to_owned();
    let bps = v["bps"].as_u64()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if now.saturating_sub(v["time"].as_u64().unwrap_or(0)) > PROBE_MAX_AGE_S { return None; }
    if !Path::new("/sys/block").join(&dev).exists() { return None; }
    let class = class_of(&dev);
    let (lo, hi) = class.range();
    Some(IoRate { bps: bps.clamp(lo, hi), source: Source::Probe, device: dev, class })
}

/// Best available rate for the slowest target disk. Unprivileged.
pub fn gather() -> IoRate {
    let disks = target_disks();
    if let Some(p) = load_probe().filter(|p| disks.is_empty() || disks.contains(&p.device)) {
        // The probe measured one disk; a slower target disk still wins.
        let slower = disks.iter().filter(|d| **d != p.device).map(|d| rate_of(d)).min_by_key(|r| r.bps);
        return match slower { Some(s) if s.bps < p.bps => s, _ => p };
    }
    disks.iter().map(|d| rate_of(d)).min_by_key(|r| r.bps)
        .unwrap_or_else(|| IoRate::class_default(DevClass::Other, "unknown"))
}

fn rate_of(disk: &str) -> IoRate {
    let class = class_of(disk);
    let (lo, hi) = class.range();
    match rd(format!("/sys/block/{disk}/stat")).as_deref().and_then(passive_bps) {
        // Passive is a lower bound: never below the class default.
        Some(b) => IoRate { bps: b.clamp(lo, hi).max(class.default_bps()), source: Source::Passive, device: disk.to_owned(), class },
        None => IoRate::class_default(class, disk),
    }
}

/// Root only: bounded direct write into an unnamed temp file on `dir`'s file
/// system. Writes nothing that survives (O_TMPFILE is never linked).
pub fn probe(dir: &str) -> Result<Value, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if unsafe { libc::geteuid() } != 0 { return Err("the write probe needs root".into()); }
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    let cdir = std::ffi::CString::new(dir).map_err(|_| "bad directory")?;
    if unsafe { libc::statvfs(cdir.as_ptr(), &mut sv) } != 0 { return Err(format!("{dir}: statvfs failed")); }
    if (sv.f_bavail as u64) * (sv.f_frsize as u64) < 4 * PROBE_MAX_BYTES { return Err(format!("{dir}: less than 2 GiB free, probe skipped")); }
    let open = |direct: bool| std::fs::OpenOptions::new().read(true).write(true).mode(0o600)
        .custom_flags(libc::O_TMPFILE | libc::O_CLOEXEC | if direct { libc::O_DIRECT } else { 0 }).open(dir);
    let (mut f, direct) = match open(true) {
        Ok(f) => (f, true),
        Err(_) => (open(false).map_err(|e| format!("{dir}: O_TMPFILE: {e}"))?, false),
    };
    let layout = std::alloc::Layout::from_size_align(PROBE_BLOCK, 4096).unwrap();
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() { return Err("out of memory".into()); }
    let buf = unsafe { std::slice::from_raw_parts_mut(ptr, PROBE_BLOCK) };
    // Incompressible content (some controllers compress zeros).
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for b in buf.iter_mut() { x ^= x << 13; x ^= x >> 7; x ^= x << 17; *b = x as u8; }
    let t0 = std::time::Instant::now();
    let mut written: u64 = 0;
    let mut res = Ok(());
    while written < PROBE_MAX_BYTES && t0.elapsed().as_secs_f64() < PROBE_MAX_SECS {
        if let Err(e) = f.write_all(buf) { res = Err(format!("write: {e}")); break; }
        written += PROBE_BLOCK as u64;
    }
    if res.is_ok() { res = f.sync_data().map_err(|e| format!("fdatasync: {e}")); }
    let secs = t0.elapsed().as_secs_f64();
    unsafe { std::alloc::dealloc(ptr, layout) };
    drop(f);
    res?;
    if written < 64 * MIB || secs <= 0.0 { return Err("probe too short to be meaningful".into()); }
    let dev = std::fs::read_to_string("/proc/self/mounts").ok().and_then(|m| {
        // Longest mount point that prefixes dir.
        m.lines().filter_map(|l| { let mut f = l.split_whitespace(); Some((f.next()?.to_owned(), f.next()?.to_owned())) })
            .filter(|(d, mp)| d.starts_with("/dev/") && Path::new(dir).starts_with(mp))
            .max_by_key(|(_, mp)| mp.len()).map(|(d, _)| d)
    }).and_then(|d| whole_disks(&d).into_iter().next()).unwrap_or_else(|| "unknown".into());
    let bps = (written as f64 / secs) as u64;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let out = json!({"bps": bps, "device": dev, "dir": dir, "bytes": written, "secs": (secs * 1000.0).round() / 1000.0,
                     "direct": direct, "time": now});
    crate::secure_dir(PROBE_DIR)?;
    crate::write_root_file(PROBE_FILE, &serde_json::to_vec_pretty(&out).unwrap())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passive_needs_history() {
        // 4 GiB written + 1 GiB read in 5 s busy -> 1 GiB/s.
        let s = format!("100 0 {} 0 100 0 {} 0 0 5000 0", (1u64 << 30) / 512, (4u64 << 30) / 512);
        assert_eq!(passive_bps(&s), Some(5 * (1 << 30) * 1000 / 5000));
        assert_eq!(passive_bps("1 0 8 0 1 0 8 0 0 10 0"), None);
        assert_eq!(passive_bps("garbage"), None);
    }
}
