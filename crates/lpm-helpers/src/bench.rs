//! Short benchmarks for lpm-calibrate. Every figure comes from a workload that
//! is gone afterwards: the heap probe is a child process (its memory - THP
//! bloat included - is returned when it exits), the I/O files are unlinked
//! O_TMPFILEs, and the caller drops caches + compacts between runs. Nothing
//! depends on how much memory the rest of the system happens to use.

use crate::calib::{Metric, Sample};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIB: usize = 1 << 20;

pub fn p99(v: &mut Vec<f64>) -> Option<f64> {
    if v.is_empty() { return None; }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[((v.len() as f64 * 0.99) as usize).min(v.len() - 1)])
}

fn anon(len: usize) -> Option<&'static mut [u8]> {
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
                                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
    (p != libc::MAP_FAILED).then(|| unsafe { std::slice::from_raw_parts_mut(p as *mut u8, len) })
}
fn unmap(s: &mut [u8]) { unsafe { libc::munmap(s.as_mut_ptr() as *mut libc::c_void, s.len()); } }

/// Timer wake-up overshoot p99 (µs) of a 500 µs sleep loop while another
/// thread churns anonymous memory (faults, THP allocation, reclaim paths).
pub fn wake_p99_us(dur: Duration) -> Option<f64> {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let churn = std::thread::spawn(move || {
        while !s2.load(Ordering::Relaxed) {
            if let Some(m) = anon(64 * MIB) {
                for i in (0..m.len()).step_by(4096) { m[i] = 1; }
                unmap(m);
            }
        }
    });
    let mut lat = Vec::with_capacity(4096);
    let t0 = Instant::now();
    while t0.elapsed() < dur {
        let a = Instant::now();
        std::thread::sleep(Duration::from_micros(500));
        lat.push((a.elapsed().as_nanos() as f64 / 1000.0 - 500.0).max(0.0));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = churn.join();
    p99(&mut lat)
}

fn smaps_kb(field: &str) -> Option<f64> {
    std::fs::read_to_string("/proc/self/smaps_rollup").ok()?.lines()
        .find_map(|l| l.strip_prefix(field)?.trim().trim_end_matches("kB").trim().parse().ok())
}

/// Runs inside the disposable child (`lpm-calibrate __probe`): sparse heap
/// footprint, dense fault latency, memcpy bandwidth. Prints one JSON line.
pub fn probe_main(heap_mib: usize) -> Value {
    let heap: usize = heap_mib.clamp(64, 1024) * MIB;
    let Some(h) = anon(heap) else { return json!({"error": "mmap failed"}) };
    // Sparse: one byte per 64 KiB - what a fragmented heap looks like. With THP
    // "always" every touched 2 MiB range may become a whole huge page.
    let base = smaps_kb("Rss:").unwrap_or(0.0);
    for i in (0..heap).step_by(64 * 1024) { h[i] = 1; }
    let sparse_rss = (smaps_kb("Rss:").unwrap_or(0.0) - base).max(0.0) / 1024.0;
    let thp_kb = smaps_kb("AnonHugePages:").unwrap_or(0.0);
    unmap(h);
    // Dense: fault a fresh heap in, timing each 2 MiB.
    let Some(d) = anon(heap) else { return json!({"error": "mmap failed"}) };
    let mut per = Vec::with_capacity(heap / (2 * MIB));
    let t0 = Instant::now();
    for chunk in (0..heap).step_by(2 * MIB) {
        let a = Instant::now();
        for i in (chunk..chunk + 2 * MIB).step_by(4096) { d[i] = 1; }
        per.push(a.elapsed().as_nanos() as f64 / 1000.0);
    }
    let alloc_ms = t0.elapsed().as_secs_f64() * 1000.0;
    // Huge-page coverage of the dense heap (THP success under fragmentation).
    let thp_pct = smaps_kb("AnonHugePages:").unwrap_or(0.0) * 1024.0 * 100.0 / heap as f64;
    // Bandwidth: copy between two 256 MiB halves.
    let (a, b) = d.split_at_mut(heap / 2);
    let t1 = Instant::now();
    let rounds = 6;
    for r in 0..rounds { if r % 2 == 0 { b.copy_from_slice(a) } else { a.copy_from_slice(b) } }
    let gbs = (rounds * heap / 2) as f64 / t1.elapsed().as_secs_f64() / 1e9;
    std::hint::black_box(&d[heap - 1]);
    json!({"probe_rss_mib": sparse_rss, "anon_huge_kb": thp_kb, "fault_p99_us": p99(&mut per), "alloc_ms": alloc_ms, "membw_gbs": gbs,
           "thp_pct": thp_pct})
}

/// Runs the probe in a child process (`exe __probe`) and folds it into `s`.
pub fn probe_child(exe: &std::path::Path, heap_mib: usize, s: &mut Sample) -> Result<(), String> {
    let out = std::process::Command::new(exe).arg("__probe").arg(heap_mib.to_string()).output().map_err(|e| format!("probe: {e}"))?;
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| "probe: bad output".to_string())?;
    if let Some(e) = v["error"].as_str() { return Err(format!("probe: {e}")); }
    for (m, k) in [(Metric::ProbeRssMib, "probe_rss_mib"), (Metric::FaultP99Us, "fault_p99_us"), (Metric::AllocMs, "alloc_ms"),
                   (Metric::MemBwGbs, "membw_gbs"), (Metric::ThpPct, "thp_pct")] {
        if let Some(x) = v[k].as_f64() { s.insert(m, x); }
    }
    Ok(())
}

fn tmpfile(dir: &str, direct: bool) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(true).mode(0o600)
        .custom_flags(libc::O_TMPFILE | libc::O_CLOEXEC | if direct { libc::O_DIRECT } else { 0 }).open(dir)
}

/// Buffered streaming write (with an fsync prober alongside), cold sequential
/// read, 4 KiB O_DIRECT random reads. `size` bytes on `dir`'s file system.
pub fn io(dir: &str, size: usize, s: &mut Sample) -> Result<(), String> {
    let mut f = tmpfile(dir, false).map_err(|e| format!("{dir}: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let d2 = dir.to_owned();
    let prober = std::thread::spawn(move || {
        let mut lat = Vec::new();
        let Ok(mut g) = tmpfile(&d2, false) else { return lat };
        let buf = [7u8; 4096];
        while !s2.load(Ordering::Relaxed) {
            let _ = g.seek(SeekFrom::Start(0));
            let a = Instant::now();
            if g.write_all(&buf).is_err() || g.sync_data().is_err() { break; }
            lat.push(a.elapsed().as_secs_f64() * 1000.0);
            std::thread::sleep(Duration::from_millis(25));
        }
        lat
    });
    let mut buf = vec![0u8; MIB];
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for b in buf.iter_mut() { x ^= x << 13; x ^= x >> 7; x ^= x << 17; *b = x as u8; }
    let t0 = Instant::now();
    let mut res = Ok(());
    for _ in 0..size / MIB { if let Err(e) = f.write_all(&buf) { res = Err(format!("write: {e}")); break; } }
    if res.is_ok() { res = f.sync_all().map_err(|e| format!("fsync: {e}")); }
    let wsecs = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let mut fl = prober.join().unwrap_or_default();
    res?;
    s.insert(Metric::WriteMbs, size as f64 / MIB as f64 / wsecs);
    if let Some(p) = p99(&mut fl) { s.insert(Metric::FsyncP99Ms, p); }
    // Cold read: clean pages dropped from the cache, 128 KiB reads so read-ahead matters.
    unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED); }
    let _ = f.seek(SeekFrom::Start(0));
    let mut rb = vec![0u8; 128 * 1024];
    let t1 = Instant::now();
    let mut got = 0usize;
    while got < size { match f.read(&mut rb) { Ok(0) | Err(_) => break, Ok(n) => got += n } }
    s.insert(Metric::ReadMbs, got as f64 / MIB as f64 / t1.elapsed().as_secs_f64());
    // Random 4 KiB direct reads through /proc/self/fd (the file has no name).
    if let Ok(g) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT)
        .open(format!("/proc/self/fd/{}", f.as_raw_fd())) {
        let layout = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
        let p = unsafe { std::alloc::alloc(layout) };
        if !p.is_null() {
            let mut lat = Vec::with_capacity(2000);
            let pages = (size / 4096) as u64;
            for _ in 0..2000 {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                let off = (x % pages) * 4096;
                let a = Instant::now();
                if unsafe { libc::pread(g.as_raw_fd(), p as *mut libc::c_void, 4096, off as libc::off_t) } != 4096 { break; }
                lat.push(a.elapsed().as_nanos() as f64 / 1000.0);
            }
            unsafe { std::alloc::dealloc(p, layout) };
            if let Some(v) = p99(&mut lat) { s.insert(Metric::RandReadP99Us, v); }
        }
    }
    Ok(())
}

/// Power source: battery discharge rate (whole machine) or the RAPL package
/// counter (CPU only; device power states are invisible there).
pub fn power_source() -> &'static str {
    if battery_uw().is_some() { "battery" } else if !rapl_zones().is_empty() { "rapl" } else { "none" }
}

fn battery_uw() -> Option<f64> {
    for e in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let p = e.path();
        let rd = |n: &str| std::fs::read_to_string(p.join(n)).ok().map(|s| s.trim().to_owned());
        if rd("type").as_deref() != Some("Battery") || rd("status").as_deref() != Some("Discharging") { continue; }
        if let Some(w) = rd("power_now").and_then(|v| v.parse::<f64>().ok()) { return Some(w); }
        let (i, v) = (rd("current_now")?.parse::<f64>().ok()?, rd("voltage_now")?.parse::<f64>().ok()?);
        return Some(i * v / 1e6);
    }
    None
}

fn rapl_zones() -> Vec<std::path::PathBuf> {
    std::fs::read_dir("/sys/class/powercap").into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| { let n = p.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default(); n.matches(':').count() == 1 })
        .filter(|p| p.join("energy_uj").exists()).collect()
}

/// Average idle watts over `dur` (after the caller let things settle).
pub fn idle_w(dur: Duration) -> Option<f64> {
    if battery_uw().is_some() {
        let mut v = Vec::new();
        let t0 = Instant::now();
        while t0.elapsed() < dur { if let Some(w) = battery_uw() { v.push(w / 1e6); } std::thread::sleep(Duration::from_millis(250)); }
        return (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    }
    let zones = rapl_zones();
    let read = || zones.iter().filter_map(|z| std::fs::read_to_string(z.join("energy_uj")).ok()?.trim().parse::<u64>().ok()).sum::<u64>();
    if zones.is_empty() { return None; }
    let (e0, t0) = (read(), Instant::now());
    std::thread::sleep(dur);
    let (e1, secs) = (read(), t0.elapsed().as_secs_f64());
    (e1 > e0).then(|| (e1 - e0) as f64 / 1e6 / secs)
}

/// Root: drop clean caches and compact memory so one run does not inherit
/// the previous run's page cache or fragmentation.
pub fn settle() {
    unsafe { libc::sync(); }
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
    let _ = std::fs::write("/proc/sys/vm/compact_memory", "1");
    std::thread::sleep(Duration::from_millis(500));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_benches() {
        assert!(cpu_single(Duration::from_millis(100)) > 0.0);
        assert!(cpu_multi(Duration::from_millis(100)).0 > 0.0);
        assert!(pingpong_p99_us(200).unwrap() > 0.0);
    }

    #[test]
    fn io_and_wake_produce_metrics() {
        let mut s = Sample::new();
        io("/tmp", 16 << 20, &mut s).unwrap();
        assert!(s[&Metric::WriteMbs] > 0.0 && s[&Metric::ReadMbs] > 0.0);
        assert!(wake_p99_us(Duration::from_millis(200)).is_some());
        assert_eq!(p99(&mut vec![]), None);
    }
}

// ── load phase ─────────────────────────────────────────────────────────────

pub fn meminfo_kb(field: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo").ok()?.lines()
        .find_map(|l| l.strip_prefix(field)?.trim().trim_end_matches("kB").trim().parse().ok())
}

pub fn vmstat(field: &str) -> u64 {
    std::fs::read_to_string("/proc/vmstat").unwrap_or_default().lines()
        .filter(|l| l.starts_with(field)).filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()).sum()
}

/// The ballast child (`lpm-calibrate __ballast <MiB> <threads> <floor MiB>`):
/// fills anonymous memory in 64 MiB blocks until it holds <MiB> or
/// MemAvailable falls to <floor>, prints "ready <MiB held>", then keeps
/// re-faulting one block after another (continuous reclaim / compaction /
/// fault work) and runs <threads> integer spinners. It is the OOM killer's
/// first choice (oom_score_adj 1000) and dies with its parent.
pub fn ballast_main(target_mib: usize, threads: usize, floor_mib: u64) -> ! {
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL); }
    let _ = std::fs::write("/proc/self/oom_score_adj", "1000");
    const BLOCK: usize = 64 * MIB;
    let mut blocks: Vec<&'static mut [u8]> = Vec::new();
    while blocks.len() * 64 < target_mib {
        if meminfo_kb("MemAvailable:").map_or(true, |kb| kb / 1024 <= floor_mib) { break; }
        let Some(b) = anon(BLOCK) else { break };
        for i in (0..BLOCK).step_by(4096) { b[i] = 1; }
        blocks.push(b);
    }
    println!("ready {}", blocks.len() * 64);
    let _ = std::io::stdout().flush();
    for t in 0..threads {
        std::thread::spawn(move || {
            let mut x = 0x9E37_79B9u64 ^ t as u64;
            loop { for _ in 0..1_000_000 { x ^= x << 13; x ^= x >> 7; x ^= x << 17; } std::hint::black_box(x); }
        });
    }
    let mut i = 0usize;
    loop {
        if !blocks.is_empty() {
            let k = i % blocks.len();
            unmap(blocks[k]);
            match anon(BLOCK) {
                Some(b) => { for j in (0..BLOCK).step_by(4096) { b[j] = 1; } blocks[k] = b; }
                None => { blocks.swap_remove(k); }
            }
            i += 1;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// CPU package watts over a closure's run (RAPL), and its result.
pub fn with_pkg_power<T>(f: impl FnOnce() -> T) -> (T, Option<f64>) {
    let zones = rapl_zones();
    let read = || zones.iter().filter_map(|z| std::fs::read_to_string(z.join("energy_uj")).ok()?.trim().parse::<u64>().ok()).sum::<u64>();
    let (e0, t0) = (read(), Instant::now());
    let r = f();
    let (e1, secs) = (read(), t0.elapsed().as_secs_f64());
    (r, (!zones.is_empty() && e1 > e0 && secs > 0.0).then(|| (e1 - e0) as f64 / 1e6 / secs))
}

// ── CPU / scheduler ───────────────────────────────────────────────────────────

fn spin(iters: u64, seed: u64) -> u64 {
    let mut x = seed | 1;
    for _ in 0..iters { x ^= x << 13; x ^= x >> 7; x ^= x << 17; }
    x
}

/// Integer work per second on one thread.
pub fn cpu_single(dur: Duration) -> f64 {
    let (t0, mut n) = (Instant::now(), 0u64);
    while t0.elapsed() < dur { std::hint::black_box(spin(200_000, n)); n += 200_000; }
    n as f64 / t0.elapsed().as_secs_f64()
}

/// Integer work per second on every logical CPU, and work per joule (RAPL).
pub fn cpu_multi(dur: Duration) -> (f64, Option<f64>) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let (total, w) = with_pkg_power(|| {
        let hs: Vec<_> = (0..threads).map(|i| std::thread::spawn(move || {
            let (t0, mut n) = (Instant::now(), 0u64);
            while t0.elapsed() < dur { std::hint::black_box(spin(200_000, n ^ i as u64)); n += 200_000; }
            n
        })).collect();
        hs.into_iter().map(|h| h.join().unwrap_or(0)).sum::<u64>()
    });
    let rate = total as f64 / dur.as_secs_f64();
    (rate, w.filter(|w| *w > 0.0).map(|w| rate / w))
}

/// Round trip p99 (µs) of two threads waking each other through pipes:
/// scheduler wake-up + context-switch latency.
pub fn pingpong_p99_us(rounds: usize) -> Option<f64> {
    let mut a = [0i32; 2];
    let mut b = [0i32; 2];
    unsafe { if libc::pipe(a.as_mut_ptr()) != 0 || libc::pipe(b.as_mut_ptr()) != 0 { return None; } }
    let (ar, aw, br, bw) = (a[0], a[1], b[0], b[1]);
    let echo = std::thread::spawn(move || {
        let mut c = [0u8; 1];
        for _ in 0..rounds {
            if unsafe { libc::read(ar, c.as_mut_ptr() as *mut libc::c_void, 1) } != 1 { break; }
            if unsafe { libc::write(bw, c.as_ptr() as *const libc::c_void, 1) } != 1 { break; }
        }
    });
    let mut lat = Vec::with_capacity(rounds);
    let c = [1u8; 1];
    let mut r = [0u8; 1];
    for _ in 0..rounds {
        let t = Instant::now();
        if unsafe { libc::write(aw, c.as_ptr() as *const libc::c_void, 1) } != 1 { break; }
        if unsafe { libc::read(br, r.as_mut_ptr() as *mut libc::c_void, 1) } != 1 { break; }
        lat.push(t.elapsed().as_nanos() as f64 / 1000.0);
    }
    let _ = echo.join();
    unsafe { for fd in [ar, aw, br, bw] { libc::close(fd); } }
    p99(&mut lat)
}
