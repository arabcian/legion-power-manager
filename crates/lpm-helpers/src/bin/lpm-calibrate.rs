//! lpm-calibrate — build this machine's signature: what every CPU, scheduler
//! and memory knob does here, idle and under load, accumulated across runs.
//!
//!   sudo lpm-calibrate [--budget MIN] [--all] [--phase idle|load|both] [--only KEY,..] [--thorough]
//!   lpm-calibrate --list                  the plan: keys, values, estimated time, coverage
//!   lpm-calibrate --show                  the signature (weighted estimates per value)
//!   sudo lpm-calibrate --restore          put back what an interrupted run changed
//!
//! The plan comes from the tunable table itself (CPU, Scheduler, Memory),
//! minus keys that are structural or unsafe to flip (calib::EXCLUDED). Per
//! knob: ref, every candidate once, ref (drift = reference noise). A full
//! signature is ~12-15 min; --budget (default 15) picks least-measured knobs
//! first, and repeated runs add repetitions that refine the estimates.
//! Results accumulate in /var/lib/legion-power-manager/signature.json
//! (recency- and kernel-weighted); autotune doses every knob from it.
//!
//! Phase 1 (idle): quiet machine. Phase 2 (load): a ballast child holds memory
//! down to max(1 GiB, 5 % RAM) free, re-faults 64 MiB blocks and keeps half the
//! CPUs busy. Brakes: the ballast dies at once if MemAvailable < 256 MiB or PSI
//! memory full > 40 %, is the OOM killer's first target and dies with this
//! process; a candidate that caused an OOM kill is stored as unsafe.
//! Every original is journaled before it is written and restored after each
//! knob, on Ctrl-C, or with --restore after a crash. Needs a clean state:
//! Optimizations → Restore originals first.

use lpm_helpers::bench;
use lpm_helpers::calib::{self, Benches, Calibration, Metric, Phase, Sample};
use lpm_helpers::tune;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const JOURNAL: &str = "/run/legion-power-manager/calibrate.json";
const TUNE_LOCK: &str = "/run/legion-power-manager/tune/lock";
const TUNE_STATE: &str = "/run/legion-power-manager/tune/state.json";
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) { STOP.store(true, Ordering::SeqCst); }

fn die(msg: &str) -> ! { eprintln!("lpm-calibrate: {msg}"); std::process::exit(1) }
fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

/// Originals of every file written, persisted before each write.
struct Journal { entries: Vec<(String, PathBuf, String)> }

impl Journal {
    fn load() -> Journal {
        let v: Value = std::fs::read_to_string(JOURNAL).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!([]));
        let entries = v.as_array().into_iter().flatten().filter_map(|e| {
            Some((e[0].as_str()?.to_owned(), PathBuf::from(e[1].as_str()?), e[2].as_str()?.to_owned()))
        }).collect();
        Journal { entries }
    }
    fn save(&self) -> Result<(), String> {
        let v: Vec<Value> = self.entries.iter().map(|(k, p, o)| json!([k, p, o])).collect();
        lpm_helpers::secure_dir("/run/legion-power-manager")?;
        lpm_helpers::write_root_file(JOURNAL, &serde_json::to_vec(&v).unwrap())
    }
    /// Sets a tune key, journaling each file's original first.
    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let t = tune::find(key).ok_or_else(|| format!("{key}: unknown"))?;
        let v = tune::validate(t, &Value::String(value.to_owned())).or_else(|_| {
            value.parse::<i64>().map_err(|e| e.to_string()).and_then(|n| tune::validate(t, &json!(n)))
        })?;
        for (f, data) in tune::plan(t, &v)? {
            let Some(orig) = tune::baseline_value(t, &f) else { continue };
            if !self.entries.iter().any(|(_, p, _)| *p == f) {
                self.entries.push((key.to_owned(), f.clone(), orig));
                self.save()?;
            }
            tune::write_value(t, &f, &data)?;
        }
        Ok(())
    }
    fn restore(&mut self) -> Vec<String> {
        let mut errs = Vec::new();
        for (k, f, o) in self.entries.iter().rev() {
            if let Some(t) = tune::find(k) {
                if let Err(e) = tune::write_value(t, f, o) { if f.exists() { errs.push(format!("{k} {}: {e}", f.display())); } }
            }
        }
        self.entries.clear();
        let _ = std::fs::remove_file(JOURNAL);
        errs
    }
}

/// Knob group -> writes for one value.
fn apply(j: &mut Journal, key: &str, value: &str, rate: u64, ram: u64) -> Result<(), String> {
    match key {
        "vm.dirty" => {
            let w: f64 = value.parse().map_err(|_| "bad window")?;
            let (bg, d) = lpm_helpers::autotune::dirty_pair(rate, ram, w);
            // Background first when shrinking, limit first when growing: bg < dirty at every step.
            let cur: u64 = tune::find("vm.dirty_bytes").and_then(tune::current).and_then(|s| s.parse().ok()).unwrap_or(0);
            if d >= cur { j.set("vm.dirty_bytes", &d.to_string())?; j.set("vm.dirty_background_bytes", &bg.to_string()) }
            else { j.set("vm.dirty_background_bytes", &bg.to_string())?; j.set("vm.dirty_bytes", &d.to_string()) }
        }
        "thp" => {
            let (mode, mthp) = value.split_once('+').map_or((value, false), |(m, _)| (m, true));
            j.set("thp.enabled", mode)?;
            for k in ["thp.mthp_16k", "thp.mthp_32k", "thp.mthp_64k"] {
                if tune::find(k).map_or(false, |t| !tune::files(t).is_empty()) { j.set(k, if mthp { "inherit" } else { "never" })?; }
            }
            Ok(())
        }
        _ => j.set(key, value),
    }
}

/// Live value of a group, as one of its labels.
fn live_label(key: &str) -> Option<String> {
    let cur = |k: &str| tune::find(k).and_then(tune::current);
    match key {
        "vm.dirty" => Some("1".into()),
        "thp" => {
            let m = cur("thp.enabled")?;
            if m != "always" && m != "madvise" { return None; }
            let mthp = ["thp.mthp_16k", "thp.mthp_32k", "thp.mthp_64k"].iter().any(|k| cur(k).map_or(false, |v| v != "never"));
            Some(if mthp { format!("{m}+mthp") } else { m })
        }
        _ => cur(key),
    }
}

/// The ballast child plus a brake thread that kills it when memory gets tight.
struct Ballast { child: std::process::Child, braked: Arc<AtomicBool>, stop: Arc<AtomicBool>, held_mib: usize }

impl Ballast {
    fn start(exe: &std::path::Path) -> Result<Ballast, String> {
        use std::io::BufRead;
        let ram_mib = bench::meminfo_kb("MemTotal:").unwrap_or(0) / 1024;
        let avail = bench::meminfo_kb("MemAvailable:").unwrap_or(0) / 1024;
        let headroom = (ram_mib * 5 / 100).max(1024);
        // Leave the headroom plus room for the probe (sparse + dense heap).
        let target = avail.saturating_sub(headroom + 768).min(ram_mib * 85 / 100);
        if target < 1024 { return Err(format!("only {avail} MiB available: not enough to build pressure safely")); }
        let threads = (std::thread::available_parallelism().map_or(2, |n| n.get()) / 2).max(1);
        let mut child = std::process::Command::new(exe).arg("__ballast").arg(target.to_string()).arg(threads.to_string())
            .arg(headroom.to_string()).stdout(std::process::Stdio::piped()).spawn().map_err(|e| format!("ballast: {e}"))?;
        let mut line = String::new();
        let out = child.stdout.take().ok_or("ballast: no stdout")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || { let mut r = std::io::BufReader::new(out); let mut l = String::new(); let _ = r.read_line(&mut l); let _ = tx.send(l); });
        match rx.recv_timeout(Duration::from_secs(120)) { Ok(l) => line = l, Err(_) => {} }
        let held_mib = line.trim().strip_prefix("ready ").and_then(|n| n.parse().ok()).unwrap_or(0);
        if held_mib == 0 { let _ = child.kill(); return Err("ballast did not come up".into()); }
        let (braked, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (b2, s2, pid) = (braked.clone(), stop.clone(), child.id() as i32);
        std::thread::spawn(move || while !s2.load(Ordering::Relaxed) {
            let avail = bench::meminfo_kb("MemAvailable:").unwrap_or(u64::MAX) / 1024;
            let full = lpm_helpers::autotune::psi_avg10(&std::fs::read_to_string("/proc/pressure/memory").unwrap_or_default(), "full").unwrap_or(0.0);
            if avail < 256 || full > 40.0 {
                unsafe { libc::kill(pid, libc::SIGKILL); }
                b2.store(true, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        });
        Ok(Ballast { child, braked, stop, held_mib })
    }
    fn alive(&mut self) -> bool { !self.braked.load(Ordering::SeqCst) && matches!(self.child.try_wait(), Ok(None)) }
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}


/// `scale` stretches every measurement window (1.0 default, 1.6 with --thorough).
struct Ctx { exe: PathBuf, dir: String, io_size: usize, scale: f64 }

fn ms(c: &Ctx, base: u64) -> Duration { Duration::from_millis((base as f64 * c.scale) as u64) }

fn run_idle(b: Benches, c: &Ctx) -> Result<Sample, String> {
    // Page cache and fragmentation only matter to the heap and I/O probes.
    if b.io || b.cpu_mem { bench::settle(); } else { std::thread::sleep(Duration::from_millis(300)); }
    let mut s = Sample::new();
    if b.idle {
        std::thread::sleep(ms(c, 800));
        if let Some(w) = bench::idle_w(ms(c, 2000)) { s.insert(Metric::IdleW, w); }
    }
    if b.cpu || b.cpu_mem {
        if let Some(w) = bench::wake_p99_us(ms(c, 600)) { s.insert(Metric::WakeP99Us, w); }
    }
    if b.cpu {
        s.insert(Metric::CpuSingle, bench::cpu_single(ms(c, 400)));
        let (rate, eff) = bench::cpu_multi(ms(c, 600));
        s.insert(Metric::CpuMulti, rate);
        if let Some(e) = eff { s.insert(Metric::CpuEff, e); }
        if let Some(p) = bench::pingpong_p99_us(2000) { s.insert(Metric::PingPongP99Us, p); }
    }
    if b.cpu_mem { bench::probe_child(&c.exe, 512, &mut s)?; }
    if b.io { bench::io(&c.dir, c.io_size, &mut s)?; }
    Ok(s)
}

/// One loaded run. Returns the sample and whether an OOM kill happened.
/// One loaded run (memory is compacted once per knob, not per run).
fn run_load(b: Benches, c: &Ctx) -> Result<(Sample, bool), String> {
    std::thread::sleep(ms(c, 800));  // kswapd settles on the new values
    let (stall0, oom0, t0) = (bench::vmstat("allocstall"), bench::vmstat("oom_kill"), Instant::now());
    let (res, w) = bench::with_pkg_power(|| -> Result<Sample, String> {
        let mut s = Sample::new();
        if let Some(w) = bench::wake_p99_us(ms(c, 600)) { s.insert(Metric::WakeP99Us, w); }
        if b.cpu {
            if let Some(p) = bench::pingpong_p99_us(2000) { s.insert(Metric::PingPongP99Us, p); }
            s.insert(Metric::CpuSingle, bench::cpu_single(ms(c, 400)));
        }
        if b.cpu_mem { bench::probe_child(&c.exe, 256, &mut s)?; }
        if b.io { bench::io(&c.dir, c.io_size / 4, &mut s)?; }
        Ok(s)
    });
    let mut s = res?;
    s.insert(Metric::StallsPerSec, (bench::vmstat("allocstall") - stall0) as f64 / t0.elapsed().as_secs_f64());
    if let Some(w) = w { s.insert(Metric::PkgW, w); }
    Ok((s, bench::vmstat("oom_kill") > oom0))
}

/// Seconds per run (default windows; --thorough scales them).
fn est_secs(ph: Phase, b: Benches, scale: f64) -> f64 {
    let s = match ph {
        Phase::Idle => 0.3 + if b.io || b.cpu_mem { 0.8 } else { 0.0 } + if b.idle { 2.8 } else { 0.0 } + if b.cpu || b.cpu_mem { 0.6 } else { 0.0 }
            + if b.cpu { 1.1 } else { 0.0 } + if b.cpu_mem { 1.2 } else { 0.0 } + if b.io { 2.0 } else { 0.0 },
        Phase::Load => 1.4 + if b.cpu { 0.5 } else { 0.0 } + if b.cpu_mem { 0.6 } else { 0.0 } + if b.io { 0.8 } else { 0.0 },
    };
    s * scale
}

/// Runs for one knob: the reference opens and closes each round (drift shows
/// up as reference noise), every candidate once per round.
fn runs_for(values: usize, rounds: usize) -> usize { rounds * (values + 1) + 1 }

/// Run order: [ref, c1..cn, ref] per round, candidates rotated between rounds.
fn order(reference: &str, cands: &[String], rounds: usize) -> Vec<String> {
    let mut out = Vec::new();
    for r in 0..rounds {
        out.push(reference.to_owned());
        let mut c = cands.to_vec();
        let n = c.len();
        if n > 0 { c.rotate_left(r % n); }
        out.extend(c);
    }
    out.push(reference.to_owned());
    out
}

/// One planned measurement: a key in one phase.
struct Item { key: String, phase: Phase, benches: Benches, reference: String, values: Vec<String>, secs: f64 }

fn plan(rounds: usize, scale: f64, ram: u64, only: &Option<Vec<String>>, phase: &str, cal: &Calibration) -> (Vec<Item>, Vec<(String, String)>) {
    let swap = bench::meminfo_kb("SwapTotal:").unwrap_or(0) > 0;
    let zram = std::fs::read_to_string("/proc/swaps").unwrap_or_default().contains("/dev/zram");
    let numa = std::fs::read_dir("/sys/devices/system/node").map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("node")).count()).unwrap_or(1);
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let mut keys: Vec<(String, &'static str)> = vec![("vm.dirty".into(), "Memory"), ("thp".into(), "Memory")];
    for t in tune::TUNABLES {
        if !["CPU", "Scheduler", "Memory"].contains(&t.group) { continue; }
        if let Some((_, why)) = calib::EXCLUDED.iter().find(|(k, _)| *k == t.key) { skipped.push((t.key.to_owned(), why.to_string())); continue; }
        let why = if !tune::vendor_ok(t) { Some("other CPU vendor") }
            else if !t.debugfs && tune::files(t).is_empty() { Some("not on this machine/kernel") }
            else if t.key.starts_with("zswap.") && (zram || !swap) { Some("zram / no swap: zswap unused") }
            else if (t.key == "vm.swappiness" || t.key == "vm.page_cluster") && !swap { Some("no swap") }
            else if (t.key == "kernel.numa_balancing" || t.key == "vm.zone_reclaim_mode") && numa <= 1 { Some("single NUMA node") }
            else { None };
        if let Some(w) = why { skipped.push((t.key.to_owned(), w.to_owned())); continue; }
        keys.push((t.key.to_owned(), t.group));
    }
    for (key, group) in keys {
        if only.as_ref().map_or(false, |o| !o.iter().any(|x| *x == key)) { continue; }
        let Some(reference) = live_label(&key) else { skipped.push((key, "live value unreadable".into())); continue };
        let t = tune::find(&key);
        let opts: Vec<String> = t.map(|t| tune::options(t).into_iter().map(|(v, _)| v).collect()).unwrap_or_default();
        let raw = match key.as_str() {
            "vm.dirty" => vec!["0.25".into(), "0.5".into(), "2".into()],
            "thp" => vec!["madvise".into(), "always".into(), "madvise+mthp".into(), "always+mthp".into()],
            _ => calib::override_values(&key, &reference, ram / 1024).or_else(|| t.map(|t| calib::generic_values(&t.kind, &reference, &opts))).unwrap_or_default(),
        };
        let mut values = Vec::new();
        for v in raw {
            let vs = if v == "@wsf" {
                [128u64, 256, 512].iter().map(|h| h << 20).filter(|h| *h <= (ram / 50).min(1 << 30))
                    .map(|h| lpm_helpers::autotune::wsf_for(h, ram).to_string()).collect()
            } else { vec![v] };
            for x in vs {
                if t.map_or(false, |t| t.kind == tune::Kind::Choice) && !opts.is_empty() && !opts.contains(&x) { continue; }
                if x != reference && !values.contains(&x) { values.push(x); }
            }
        }
        if values.is_empty() { skipped.push((key, "no alternative values".into())); continue; }
        for (ph, b) in calib::phases_for(&key, group) {
            if (phase == "idle" && ph == Phase::Load) || (phase == "load" && ph == Phase::Idle) { continue; }
            let secs = est_secs(ph, b, scale) * runs_for(values.len(), rounds) as f64;
            items.push(Item { key: key.clone(), phase: ph, benches: b, reference: reference.clone(), values: values.clone(), secs });
        }
    }
    // Least measured first; phases stay together (one ballast for the whole load phase).
    items.sort_by_key(|i| (i.phase == Phase::Load, cal.coverage(&i.key, i.phase)));
    (items, skipped)
}

fn pct(x: Option<f64>) -> String { x.map_or("    n/a".into(), |v| format!("{:+6.1}%", v * 100.0)) }

fn show(c: &Calibration) {
    let fp = &c.fingerprint;
    println!("signature of {} · {} · {} MiB · BIOS {}\npower {}\n", fp.product, fp.cpu, fp.ram_mib, fp.bios,
             if c.on_battery { "last measured on battery (whole machine)" } else { "measured on AC (RAPL: CPU package only)" });
    println!("  {:<30} {:<24} {:<5} {:>8} {:>8} {:>8} {:>8} {:>5}", "knob", "value vs ref", "phase", "latency", "thruput", "power", "memory", "n");
    for (k, gs) in &c.keys {
        for g in gs {
            for (v, s) in &g.values {
                for ph in [Phase::Idle, Phase::Load] {
                    let Some(m) = c.get_phase(k, &g.reference, v, ph) else { continue };
                    println!("  {k:<30} {:<24} {:<5} {} {} {} {} {:>5.1}{}", format!("{v} vs {}", g.reference),
                             if ph == Phase::Idle { "idle" } else { "load" }, pct(m.lat), pct(m.thr), pct(m.pwr), pct(m.mem), m.n,
                             if s.unsafe_ { "  UNSAFE (OOM kill)" } else { "" });
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Disposable children: no machine checks, just measure / hold memory.
    if args.first().map(String::as_str) == Some("__probe") {
        println!("{}", bench::probe_main(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(512)));
        return;
    }
    if args.first().map(String::as_str) == Some("__ballast") {
        let n = |i: usize, d: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
        bench::ballast_main(n(1, 0) as usize, n(2, 1) as usize, n(3, 1024));
    }
    lpm_helpers::init();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let mut cal = Calibration::load().unwrap_or_else(|| Calibration { fingerprint: calib::Fingerprint::current(), ..Default::default() });
    if flag("--show") {
        if cal.keys.is_empty() { println!("no signature yet (sudo lpm-calibrate)"); } else { show(&cal); }
        return;
    }
    let thorough = flag("--thorough");
    let (rounds, scale) = if thorough { (2, 1.6) } else { (1, 1.0) };
    let only: Option<Vec<String>> = opt("--only").map(|s| s.split(',').map(str::to_owned).collect());
    let phase = opt("--phase").unwrap_or_else(|| "both".into());
    let budget = if flag("--all") { f64::INFINITY } else { opt("--budget").and_then(|s| s.parse::<f64>().ok()).unwrap_or(15.0) * 60.0 };
    let ram = bench::meminfo_kb("MemTotal:").unwrap_or(0) * 1024;
    let (items, skipped) = plan(rounds, scale, ram, &only, &phase, &cal);
    let total_secs: f64 = items.iter().map(|i| i.secs).sum();
    if flag("--list") {
        for i in &items {
            println!("  {:<5} {:<30} ref {:<20} try {:<40} ~{:>4.0} s  measured {}x", if i.phase == Phase::Idle { "idle" } else { "load" },
                     i.key, i.reference, i.values.join(","), i.secs, cal.coverage(&i.key, i.phase));
        }
        println!("\n{} measurements, ~{:.0} min in total; not tested:", items.len(), total_secs / 60.0);
        for (k, w) in &skipped { println!("  {k:<32} {w}"); }
        return;
    }
    if unsafe { libc::geteuid() } != 0 { die("needs root (sudo lpm-calibrate)"); }
    if flag("--restore") {
        let mut j = Journal::load();
        let n = j.entries.len();
        let errs = j.restore();
        println!("restored {n} file(s){}", if errs.is_empty() { String::new() } else { format!(", errors: {}", errs.join("; ")) });
        return;
    }
    if !Journal::load().entries.is_empty() { die("an interrupted run left changes: sudo lpm-calibrate --restore first"); }
    let state_active = std::fs::read_to_string(TUNE_STATE).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map_or(false, |v| v["baseline"].as_array().map_or(false, |a| !a.is_empty()));
    if state_active { die("an Optimizations preset is active: Restore originals first, so every knob is measured from the boot state"); }
    let _ = lpm_helpers::secure_dir("/run/legion-power-manager/tune");
    let lock = std::fs::OpenOptions::new().create(true).write(true).open(TUNE_LOCK).unwrap_or_else(|e| die(&format!("lock: {e}")));
    if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 { die("Legion Power Manager is applying something; try again"); }
    // A signature of other hardware is set aside, not merged.
    if cal.keys.is_empty() && std::path::Path::new(calib::FILE).exists() {
        let _ = std::fs::rename(calib::FILE, format!("{}.other-{}", calib::FILE, now()));
    }
    unsafe {
        libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
    }
    let ctx = Ctx {
        exe: std::env::current_exe().unwrap_or_else(|_| die("cannot find own executable")),
        dir: opt("--dir").unwrap_or_else(|| "/var/tmp".into()),
        io_size: if thorough { 512 } else { 256 } << 20,
        scale,
    };
    let rate = lpm_helpers::iorate::gather().bps;
    let src = bench::power_source();
    if src != "battery" { eprintln!("note: on AC — power is RAPL (CPU package) only; device-level power needs a run on battery"); }
    cal.on_battery = src == "battery";
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_owned();

    // Budget: least-measured first until the time is used.
    let mut chosen = Vec::new();
    let mut spent = 0.0;
    for i in items {
        if spent > 0.0 && spent + i.secs > budget { continue; }
        spent += i.secs;
        chosen.push(i);
    }
    let runs: usize = chosen.iter().map(|i| runs_for(i.values.len(), rounds)).sum();
    eprintln!("{} measurement(s), {runs} runs, ~{:.0} of ~{:.0} min for the full signature · Ctrl-C restores and stops",
              chosen.len(), spent / 60.0, total_secs / 60.0);

    let mut j = Journal { entries: Vec::new() };
    let mut done = 0usize;
    let mut ballast: Option<Ballast> = None;
    let mut restarts = 0;
    let t_start = Instant::now();
    'items: for it in chosen {
        if it.phase == Phase::Load && ballast.is_none() {
            eprintln!("\nphase 2: building memory pressure ...");
            match Ballast::start(&ctx.exe) {
                Ok(b) => { eprintln!("ballast holds {} MiB; {} MiB left available", b.held_mib, bench::meminfo_kb("MemAvailable:").unwrap_or(0) / 1024); ballast = Some(b); }
                Err(e) => { eprintln!("load phase skipped: {e}"); break; }
            }
        }
        let mut samples: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
        let mut unsafe_vals: Vec<String> = Vec::new();
        if it.phase == Phase::Load { let _ = std::fs::write("/proc/sys/vm/compact_memory", "1"); }
        {
            for v in &order(&it.reference, &it.values, rounds) {
                if STOP.load(Ordering::SeqCst) { break 'items; }
                if let Some(b) = ballast.as_mut() {
                    if !b.alive() {
                        restarts += 1;
                        let _ = j.restore();
                        if restarts > 2 { eprintln!("\nballast stopped by the brakes repeatedly: load phase ends here"); break 'items; }
                        match Ballast::start(&ctx.exe) { Ok(nb) => *b = nb, Err(e) => { eprintln!("\n{e}"); break 'items; } }
                    }
                }
                done += 1;
                eprint!("\r[{done}/{runs} {:>3.0} min] {:<4} {:<30} = {v:<20}", t_start.elapsed().as_secs_f64() / 60.0,
                        if it.phase == Phase::Idle { "idle" } else { "load" }, it.key);
                let _ = std::io::stderr().flush();
                if let Err(e) = apply(&mut j, &it.key, v, rate, ram) {
                    eprintln!("\n{}: {v}: {e} — value skipped", it.key);
                    let _ = j.restore();
                    continue;
                }
                let r = match it.phase { Phase::Idle => run_idle(it.benches, &ctx).map(|s| (s, false)), Phase::Load => run_load(it.benches, &ctx) };
                match r {
                    Ok((s, oom)) => { if oom && *v != it.reference { unsafe_vals.push(v.clone()); } samples.entry(v.clone()).or_default().push(s); }
                    Err(e) => eprintln!("\n{}: {v}: {e}", it.key),
                }
            }
        }
        for e in j.restore() { eprintln!("\nrestore: {e}"); }
        let refs = samples.remove(&it.reference).unwrap_or_default();
        let t = now();
        for v in &unsafe_vals { cal.add(&it.key, &it.reference, v, it.phase, None, true, t, &kernel); }
        if refs.len() < 2 { continue; }
        for (v, s) in &samples {
            let m = calib::fold(&calib::metric_effects(&refs, s));
            cal.add(&it.key, &it.reference, v, it.phase, Some(m), false, t, &kernel);
        }
        // Saved after every knob: an interrupted run keeps what it measured.
        let _ = lpm_helpers::secure_dir(lpm_helpers::defaults::DIR);
        let _ = lpm_helpers::write_root_file(calib::FILE, &serde_json::to_vec(&cal.to_json()).unwrap());
    }
    for e in j.restore() { eprintln!("\nrestore: {e}"); }
    if let Some(b) = ballast { b.stop(); }
    eprintln!();
    let _ = lpm_helpers::secure_dir(lpm_helpers::defaults::DIR);
    if let Err(e) = lpm_helpers::write_root_file(calib::FILE, &serde_json::to_vec(&cal.to_json()).unwrap()) { die(&format!("save: {e}")); }
    if STOP.load(Ordering::SeqCst) { println!("interrupted: measured knobs are saved, everything is restored"); }
    let (left, _) = plan(rounds, scale, ram, &None, "both", &cal);
    let uncovered = left.iter().filter(|i| cal.coverage(&i.key, i.phase) == 0).count();
    println!("signature: {} key(s) measured; {uncovered} measurement(s) never run yet. `lpm-calibrate --show` for details; run again to extend/refine it.",
             cal.keys.len());
    println!("autotune uses it on its next run (GUI Autotune or lpm-autotune <goal>).");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schedule() {
        let c: Vec<String> = ["a", "b", "c"].iter().map(|x| x.to_string()).collect();
        assert_eq!(order("r", &c, 1), vec!["r", "a", "b", "c", "r"]);
        assert_eq!(order("r", &c, 2), vec!["r", "a", "b", "c", "r", "b", "c", "a", "r"]);
        assert_eq!(runs_for(3, 1), 5);
        assert_eq!(runs_for(3, 2), 9);
        let cpu = Benches { cpu: true, idle: true, ..Default::default() };
        // A CPU knob with 3 candidates: was 2 x 4 runs x 8.5 s = 68 s, now 5 x 5.1 s.
        assert!(est_secs(Phase::Idle, cpu, 1.0) * runs_for(3, 1) as f64 <= 26.0);
    }
}
