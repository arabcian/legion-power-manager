//! lpm-calibrate — build this machine's signature: what every CPU, scheduler,
//! memory and storage knob does here, idle, under load and on the disk,
//! accumulated across runs.
//!
//!   sudo lpm-calibrate [--budget MIN] [--depth lean|deep|max] [--all] [--phase idle|load|io|both]
//!                      [--only KEY|@thp|@mem|@cpu|@sched|@io,..] [--sessions N] [--thorough] [--dir PATH] [--seed N] [--no-confirm]
//!   sudo lpm-calibrate --oat [...]        legacy: one knob at a time (ref, every candidate, ref)
//!   lpm-calibrate --list                  the plan: keys, values, depth, estimated time, coverage
//!   lpm-calibrate --show                  the signature (weighted estimates per value, interactions)
//!   sudo lpm-calibrate --restore          put back what an interrupted run changed
//!
//! Default mode = sequential experimental design. Every run changes a balanced random set
//! of knobs together; a Bayesian model (src/model.rs) learns each knob's effect (dose-response
//! for numeric ladders) and the interactions of ALL knob pairs and, with enough data, triples,
//! from ALL runs. Depth grows with the budget (lean <= 20 min, deep <= 50, max beyond): more
//! runs, crowded runs that change many knobs at once, interaction doubts chased on purpose,
//! dose refinement between measured levels. Short sessions without --depth are progressive:
//! each spends its time on what the log still lacks (see Stage), so repeated 15-minute
//! sessions (or --sessions N, back to back) add up to a deep calibration. After the space-filling start each next batch is
//! placed where it most reduces the chance that a per-goal decision (change / leave a knob)
//! or an interaction among the chosen knobs is misjudged; it stops early when both are
//! settled. The predicted best configuration per goal is then measured (confirmation) and
//! its result feeds back. Latency, throughput, power and memory are measured on every run
//! (log-ratio to the session's reference runs, robust to outliers and drift), so any goal
//! weights can be decided from the same data. THP is measured as a family (enabled, mTHP,
//! defrag, shmem) with TLB-reach and huge-fault probes. A run that OOM-kills is bisected to
//! the smallest culprit set, which is stored as unsafe; a run disturbed by other programs
//! counts less.
//!
//! The plan comes from the tunable table itself (CPU, Scheduler, Memory, Storage),
//! minus keys that are structural or unsafe to flip (calib::EXCLUDED). The
//! budget (--budget, default 15 min) is kept by wall clock; repeated sessions
//! fill the gaps of earlier ones and refine the estimates.
//! Results accumulate in /var/lib/legion-power-manager/signature.json
//! (recency- and kernel-weighted); autotune doses every knob from it.
//!
//! Phase IO: the storage suite (bench::io) on the disk holding --dir (default: /var/tmp or
//! the first of /var/cache, /home, / that is on a local disk) for the Storage rows and the
//! dirty window - their effects are measured and weighed there only (storage weight).
//! Phase 1 (idle): quiet machine. Phase 2 (load): a ballast child holds memory
//! down to max(1 GiB, 5 % RAM) free, re-faults 64 MiB blocks and keeps half the
//! CPUs busy; part of its memory is perforated (free, but no free 2 MiB block). Brakes: the ballast dies at once if MemAvailable < 256 MiB or PSI
//! memory full > 40 %, is the OOM killer's first target and dies with this
//! process; a candidate that caused an OOM kill is stored as unsafe.
//! Every original is journaled before it is written and restored after each
//! knob, on Ctrl-C, or with --restore after a crash. Needs a clean state:
//! Optimizations → Restore originals first.

use lpm_helpers::autotune::{Goal, Weights, MARGIN};
use lpm_helpers::bench;
use lpm_helpers::calib::{self, Benches, Calibration, Metric, Objective, Phase, Row, Sample};
use lpm_helpers::model::{self, Cfg, Factor, Joint, Mix, Model, Problem, Rng};
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
    /// Puts every original back. The I/O scheduler goes first: switching the elevator resets
    /// the queue's nr_requests and (bfq) writeback throttling, so a journaled original of those
    /// must be written after it, not before.
    fn restore(&mut self) -> Vec<String> {
        let mut errs = Vec::new();
        let sched = |k: &str| k == "blk.scheduler";
        let order = self.entries.iter().filter(|e| sched(&e.0)).chain(self.entries.iter().rev().filter(|e| !sched(&e.0)));
        for (k, f, o) in order {
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

/// Live value of a group, as one of its labels. A Storage row whose disks disagree ("mixed",
/// e.g. a USB stick next to the NVMe drives) takes the value of the disk the suite measures.
fn live_label(key: &str, disk: Option<&str>) -> Option<String> {
    let cur = |k: &str| tune::find(k).and_then(tune::current);
    if let (Some(attr), Some(d)) = (key.strip_prefix("blk."), disk) {
        let v = cur(key)?;
        if v != "mixed" { return Some(v); }
        let raw = std::fs::read_to_string(format!("/sys/block/{d}/queue/{attr}")).ok()?;
        let raw = raw.trim();
        return Some(match (raw.find('['), raw.find(']')) { (Some(a), Some(b)) if b > a => raw[a + 1..b].to_owned(), _ => raw.to_owned() });
    }
    match key {
        "vm.dirty" => Some("1".into()),
        "thp" => {
            let m = cur("thp.enabled")?;
            if m != "always" && m != "madvise" && m != "never" { return None; }
            let mthp = ["thp.mthp_16k", "thp.mthp_32k", "thp.mthp_64k"].iter().any(|k| cur(k).map_or(false, |v| v != "never"));
            if m == "never" && mthp { return None; }
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
            .arg(headroom.to_string()).arg("1").stdout(std::process::Stdio::piped()).spawn().map_err(|e| format!("ballast: {e}"))?;
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
/// `battery`: power source chosen at session start (whole machine) instead of RAPL.
/// `disk`: whole disk holding `dir` (None: not a local disk - no IO phase).
struct Ctx { exe: PathBuf, dir: String, disk: Option<String>, io_size: usize, scale: f64, battery: bool }

fn ms(c: &Ctx, base: u64) -> Duration { Duration::from_millis((base as f64 * c.scale) as u64) }

/// Latency side of the CPU benchmarks, the same on a quiet and on a loaded machine: thread
/// ping-pong, the light frame loop (tail and median) and the game loop (frame time and tail).
fn cpu_suite(c: &Ctx, s: &mut Sample) {
    if let Some(p) = bench::pingpong_p99_us(2000) { s.insert(Metric::PingPongP99Us, p); }
    if let Some((t, m)) = bench::frame_us(ms(c, 600)) { s.insert(Metric::FrameP99Us, t); s.insert(Metric::FrameMedUs, m); }
    if let Some((m, t)) = bench::game_loop(ms(c, 720)) { s.insert(Metric::GameFrameMs, m); s.insert(Metric::GameTailMs, t); }
}

fn run_idle(b: Benches, c: &Ctx) -> Result<Sample, String> {
    // Page cache and fragmentation only matter to the heap and I/O probes.
    if b.io || b.cpu_mem { bench::settle(); } else { std::thread::sleep(Duration::from_millis(300)); }
    let mut s = Sample::new();
    if b.idle {
        std::thread::sleep(ms(c, 800));
        if let Some(w) = bench::idle_w(ms(c, 2000), c.battery) { s.insert(Metric::IdleW, w); }
    }
    if b.cpu || b.cpu_mem {
        if let Some(w) = bench::wake_p99_us(ms(c, 600)) { s.insert(Metric::WakeP99Us, w); }
    }
    if b.cpu {
        cpu_suite(c, &mut s);
        s.insert(Metric::CpuSingle, bench::cpu_single(ms(c, 400)));
        let k = bench::contended(ms(c, 600));
        s.insert(Metric::CpuMulti, k.rate);
        if let Some(e) = k.eff { s.insert(Metric::CpuEff, e); }
        if let Some(f) = k.frame_tail { s.insert(Metric::BusyFrameP99Us, f); }
    }
    if b.cpu || b.cpu_mem { if let Some(j) = bench::jobs_per_sec(&c.exe, ms(c, 450)) { s.insert(Metric::JobsPerSec, j); } }
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
            cpu_suite(c, &mut s);
            s.insert(Metric::CpuSingle, bench::cpu_single(ms(c, 400)));
            // The ballast's spinners hold half the CPUs already: 1.5 threads per CPU on top of them.
            let k = bench::contended(ms(c, 450));
            s.insert(Metric::CpuMulti, k.rate);
            if let Some(f) = k.frame_tail { s.insert(Metric::BusyFrameP99Us, f); }
        }
        // Short jobs under memory pressure: every fresh heap is paid for by reclaim.
        if let Some(j) = bench::jobs_per_sec(&c.exe, ms(c, 450)) { s.insert(Metric::JobsPerSec, j); }
        if b.cpu_mem { bench::probe_child(&c.exe, 256, &mut s)?; }
        if b.io { bench::io(&c.dir, c.io_size / 4, &mut s)?; }
        Ok(s)
    });
    let mut s = res?;
    s.insert(Metric::StallsPerSec, (bench::vmstat("allocstall") - stall0) as f64 / t0.elapsed().as_secs_f64());
    if let Some(w) = w { s.insert(Metric::PkgW, w); }
    Ok((s, bench::vmstat("oom_kill") > oom0))
}

/// One run of the IO phase: page cache dropped (the previous run's file pages must not serve
/// this one's reads), then the storage suite on the measured disk.
fn run_io(c: &Ctx) -> Result<Sample, String> {
    unsafe { libc::sync(); }
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
    std::thread::sleep(Duration::from_millis(400));
    let mut s = Sample::new();
    bench::io(&c.dir, c.io_size, &mut s)?;
    Ok(s)
}

/// Seconds per run (default windows; --thorough scales them).
fn est_secs(ph: Phase, b: Benches, scale: f64) -> f64 {
    let s = match ph {
        Phase::Idle => 0.3 + if b.io || b.cpu_mem { 0.8 } else { 0.0 } + if b.idle { 2.8 } else { 0.0 } + if b.cpu || b.cpu_mem { 1.1 } else { 0.0 }
            + if b.cpu { 2.5 } else { 0.0 } + if b.cpu_mem { 1.8 } else { 0.0 } + if b.io { 4.0 } else { 0.0 },
        Phase::Load => 1.9 + if b.cpu { 2.3 } else { 0.0 } + if b.cpu_mem { 1.0 } else { 0.0 } + if b.io { 2.0 } else { 0.0 },
        Phase::Io => 0.6 + 4.0,
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

/// `--only` entry: a key, or a group alias (@thp, @mem, @cpu, @sched, @io = Storage rows + dirty window).
fn selects(x: &str, key: &str, group: &str) -> bool {
    match x {
        "@io" | "@storage" => calib::io_key(key),
        "@thp" => key == "thp" || key.starts_with("thp."),
        "@mem" => group == "Memory",
        "@cpu" => group == "CPU",
        "@sched" => group == "Scheduler",
        _ => x == key,
    }
}

fn plan(rounds: usize, scale: f64, ram: u64, only: &Option<Vec<String>>, phase: &str, cal: &Calibration, disk: &Result<String, String>) -> (Vec<Item>, Vec<(String, String)>) {
    let swap = bench::meminfo_kb("SwapTotal:").unwrap_or(0) > 0;
    let zram = std::fs::read_to_string("/proc/swaps").unwrap_or_default().contains("/dev/zram");
    let numa = std::fs::read_dir("/sys/devices/system/node").map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("node")).count()).unwrap_or(1);
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let mut keys: Vec<(String, &'static str)> = vec![("vm.dirty".into(), "Memory"), ("thp".into(), "Memory")];
    for t in tune::TUNABLES {
        if !["CPU", "Scheduler", "Memory", "Storage"].contains(&t.group) { continue; }
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
        if only.as_ref().map_or(false, |o| !o.iter().any(|x| selects(x, &key, group))) { continue; }
        if calib::io_key(&key) { if let Err(e) = disk { skipped.push((key, format!("no storage suite: {e} (--dir PATH on a local disk)"))); continue; } }
        let Some(reference) = live_label(&key, disk.as_ref().ok().map(String::as_str)) else { skipped.push((key, "live value unreadable".into())); continue };
        let t = tune::find(&key);
        let opts: Vec<String> = t.map(|t| tune::options(t).into_iter().map(|(v, _)| v).collect()).unwrap_or_default();
        let raw = match key.as_str() {
            "vm.dirty" => vec!["0.25".into(), "0.5".into(), "2".into()],
            "thp" => vec!["never".into(), "madvise".into(), "always".into(), "madvise+mthp".into(), "always+mthp".into()],
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
        if key == "vm.dirty" {
            // Windows are clamped to [32 MiB, min(RAM/50, 1 GiB)]: on a fast disk several become the same limits,
            // which would be measured as different doses (or as the reference itself).
            let rate = lpm_helpers::iorate::gather().bps;
            let bytes = |w: &str| w.parse::<f64>().ok().map(|w| lpm_helpers::autotune::dirty_pair(rate, ram, w));
            let mut seen = vec![bytes(&reference)];
            values.retain(|v| { let b = bytes(v); if seen.contains(&b) { false } else { seen.push(b); true } });
        }
        if values.is_empty() { skipped.push((key, "no alternative values".into())); continue; }
        for (ph, b) in calib::phases_for(&key, group) {
            if phase != "both" && phase != ph.name() { continue; }
            let secs = est_secs(ph, b, scale) * runs_for(values.len(), rounds) as f64;
            items.push(Item { key: key.clone(), phase: ph, benches: b, reference: reference.clone(), values: values.clone(), secs });
        }
    }
    // Least measured first; phases stay together (one ballast for the whole load phase).
    items.sort_by_key(|i| (i.phase.idx(), cal.coverage(&i.key, i.phase)));
    (items, skipped)
}

// ── sequential design ────────────────────────────────────────────────────────

/// How deep a session digs, from its time budget: the shape of the space-filling runs (how
/// many knobs change together), the run cap, the share spent space-filling, the batch size,
/// confirmation repeats, how many doubtful interactions are chased on purpose, the largest
/// random experiment, and how many dose refinements (midpoints of numeric ladders) are tried.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Depth { name: &'static str, mix: Mix, cap: usize, init: f64, batch: usize, reps: usize, pairs: usize, crowd: usize, ladder: usize, explore: bool }

impl Depth {
    /// `minutes`: the whole session (infinite = --all); `nf`: most knobs in one phase.
    fn pick(minutes: f64, nf: usize, name: Option<&str>) -> Depth {
        let n = name.unwrap_or(if minutes <= 20.0 { "lean" } else if minutes <= 50.0 { "deep" } else { "max" });
        let f = nf.max(4);
        match n {
            "deep" => Depth { name: "deep", mix: Mix { cluster: 0.35, cluster_k: (2, 5), spread_k: (4, 9), crowd: 0.2, crowd_k: (8, 14.min(f)) },
                              cap: 520, init: 0.5, batch: 6, reps: 3, pairs: 8, crowd: 10, ladder: 4, explore: false },
            "max" => Depth { name: "max", mix: Mix { cluster: 0.25, cluster_k: (2, 6), spread_k: (4, 10), crowd: 0.35, crowd_k: (10.min(f), (f / 2).max(12).min(f)) },
                             cap: 1000, init: 0.45, batch: 8, reps: 4, pairs: 16, crowd: 14, ladder: 8, explore: false },
            _ => Depth { name: "lean", mix: Mix::lean(), cap: 240, init: 0.55, batch: 5, reps: 2, pairs: 0, crowd: 6, ladder: 0, explore: false },
        }
    }
}

/// Progressive lean sessions: a short session builds on what the log already holds and
/// spends its time on the next thing the data lacks, so sessions left running while the
/// machine is free add up to a deep calibration. Stages per phase, derived from the log
/// itself (no counters to go stale; runs of deep/max sessions count too):
///   base   - until every value of every knob was in >= 6 runs
///   pairs  - until 90 % of knob pairs were changed together in >= 2 runs (gap-filling)
///   crowd  - until max(24, knobs) runs changed >= 8 knobs at once
///   refine - one session of dose refinement (or a deep/max session, or refined doses in the log)
///   polish - then rotating: doubt (interactions, triples), crowd, refine - the one done least
/// Every session's strategy is noted in the signature, so the next one knows what ran.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum Stage { Base, Pairs, Crowd, Refine, Polish }

impl Stage {
    fn what(self) -> &'static str {
        match self {
            Stage::Base => "every value of every knob measured (>= 6 runs each)",
            Stage::Pairs => "every knob pair seen together (>= 2 runs), gaps first",
            Stage::Crowd => "crowded runs (many knobs at once: saturation, higher-order effects)",
            Stage::Refine => "dose refinement between measured levels",
            Stage::Polish => "rotating per session: interaction doubt, crowd, dose refinement",
        }
    }
}

/// What the log holds for one phase's knobs.
struct Progress { runs: usize, sessions: usize, main_min: usize, pairs2: f64, crowded: usize, crowd_need: usize, refined: usize, numeric: usize,
                  refine_done: bool, polish: [usize; 3] }

fn is_numeric(f: &Factor) -> bool { f.reference.parse::<f64>().is_ok() && !f.values.is_empty() && f.values.iter().all(|v| v.parse::<f64>().is_ok()) }

/// A numeric value strictly between the knob's planned levels that is not one of them.
fn is_mid(f: &Factor, v: &str) -> bool {
    let Ok(x) = v.parse::<f64>() else { return false };
    let lv: Vec<f64> = std::iter::once(&f.reference).chain(&f.values).filter_map(|s| s.parse().ok()).collect();
    let (lo, hi) = lv.iter().fold((f64::MAX, f64::MIN), |(a, b), l| (a.min(*l), b.max(*l)));
    lv.len() >= 2 && x > lo && x < hi && !lv.iter().any(|l| (l - x).abs() < 1e-9)
}

fn progress(cal: &Calibration, phase: Phase, factors: &[Factor]) -> Progress {
    let nf = factors.len();
    let idx: BTreeMap<&str, usize> = factors.iter().enumerate().map(|(i, f)| (f.key.as_str(), i)).collect();
    let mut vc: Vec<Vec<usize>> = factors.iter().map(|f| vec![0; f.values.len()]).collect();
    let (mut co, mut refined, mut sess) = (vec![0usize; nf * nf], vec![false; nf], std::collections::BTreeSet::new());
    let (mut runs, mut crowded) = (0, 0);
    let big = 8.min(nf.saturating_sub(1)).max(2);
    for r in cal.rows.iter().filter(|r| r.phase == phase) {
        runs += 1;
        sess.insert(r.sess);
        let ids: Vec<usize> = r.cfg.iter().filter_map(|(k, v)| {
            let i = *idx.get(k.as_str())?;
            match factors[i].values.iter().position(|x| x == v) { Some(j) => vc[i][j] += 1, None => if is_mid(&factors[i], v) { refined[i] = true } }
            Some(i)
        }).collect();
        if ids.len() >= big { crowded += 1; }
        for &a in &ids { for &b in &ids { if a < b { co[a * nf + b] += 1; } } }
    }
    let total = nf * nf.saturating_sub(1) / 2;
    let seen2 = (0..nf).flat_map(|a| (a + 1..nf).map(move |b| (a, b))).filter(|(a, b)| co[a * nf + b] >= 2).count();
    Progress {
        runs, sessions: sess.len(), main_min: vc.iter().flatten().min().copied().unwrap_or(0),
        pairs2: if total == 0 { 1.0 } else { seen2 as f64 / total as f64 }, crowded,
        crowd_need: if nf < 10 { 0 } else { 24.max(nf) }, refined: refined.iter().filter(|x| **x).count(),
        numeric: factors.iter().filter(|f| is_numeric(f)).count(),
        refine_done: cal.strategies_of(phase).iter().any(|s| matches!(s.as_str(), "lean/refine" | "lean/polish-refine" | "deep" | "max")),
        polish: ["lean/polish-doubt", "lean/polish-crowd", "lean/polish-refine"].map(|n| cal.strategies_of(phase).iter().filter(|s| *s == n).count()),
    }
}

impl Progress {
    fn stage(&self) -> Stage {
        if self.main_min < 6 { Stage::Base }
        else if self.pairs2 < 0.9 { Stage::Pairs }
        else if self.crowded < self.crowd_need { Stage::Crowd }
        else if !self.refine_done && self.refined == 0 && self.numeric > 0 { Stage::Refine }
        else { Stage::Polish }
    }
    fn line(&self) -> String {
        format!("{} run(s) in {} session(s); least-measured value {}/6 runs, knob pairs seen twice {:.0} %/90 %, crowded runs {}/{}, refined doses {}",
                self.runs, self.sessions, self.main_min, self.pairs2 * 100.0, self.crowded, self.crowd_need, self.refined)
    }
}

impl Depth {
    /// A lean session's shape for its stage (time decides the run count). `explore`: once the
    /// decisions are settled the rest of the time keeps gathering coverage instead of stopping.
    /// `polish`: how often each polish strategy ran (the least-run one goes next).
    fn progressive(stage: Stage, nf: usize, polish: [usize; 3]) -> Depth {
        let f = nf.max(4);
        let deep = Mix { cluster: 0.35, cluster_k: (2, 5), spread_k: (4, 9), crowd: 0.2, crowd_k: (8.min(f), 14.min(f)) };
        let crowd = Mix { cluster: 0.2, cluster_k: (2, 5), spread_k: (4, 9), crowd: 0.5, crowd_k: (8.min(f), 14.min(f)) };
        let d = |name, mix, init, batch, reps, pairs, crowd, ladder| Depth { name, mix, cap: 240, init, batch, reps, pairs, crowd, ladder, explore: true };
        match stage {
            Stage::Base => d("lean/base", Mix::lean(), 0.55, 5, 2, 0, 6, 0),
            Stage::Pairs => d("lean/pairs", Mix { cluster: 0.3, cluster_k: (2, 5), spread_k: (4, 9), crowd: 0.0, crowd_k: (8, 12) }, 0.6, 5, 2, 6, 8, 0),
            Stage::Crowd => d("lean/crowd", crowd, 0.5, 6, 2, 8, 12, 0),
            Stage::Refine => d("lean/refine", deep, 0.2, 6, 3, 8, 10, 4),
            Stage::Polish => match (0..3).min_by_key(|&i| (polish[i], i)).unwrap_or(0) {
                0 => d("lean/polish-doubt", deep, 0.15, 6, 3, 16, 14, 2),
                1 => d("lean/polish-crowd", crowd, 0.4, 6, 2, 12, 14, 0),
                _ => d("lean/polish-refine", deep, 0.15, 6, 4, 8, 10, 6),
            },
        }
    }
}

/// One executed design point (empty cfg = reference run): wall-clock time and evidence weight.
struct Run { cfg: Cfg, s: Sample, order: usize, t: u64, w: f64 }

fn obj_index(m: Metric) -> usize { match m.objective() { Objective::Lat => 0, Objective::Thr => 1, Objective::Pwr => 2, Objective::Mem => 3 } }

/// Weight of every metric inside its objective, from the scatter of the session's reference
/// runs (robust sd of the log values): w = 1 / (sd^2 + mean sd^2 of the objective), i.e. halfway
/// between equal weights and inverse variance, then kept within 1/3..3x of the equal share. A
/// tail that jumps 30 % between identical runs no longer drowns a bandwidth that moves 1 %, and
/// no single metric can take an objective over. Fewer than 4 reference runs: equal weights.
fn metric_weights(refs: &[&Run], ms: &[Metric]) -> BTreeMap<Metric, f64> {
    let sd = |m: Metric| -> Option<f64> {
        let v: Vec<f64> = refs.iter().filter_map(|r| r.s.get(&m)).filter(|x| **x + m.eps() > 0.0).map(|x| (x + m.eps()).ln()).collect();
        if v.len() < 4 { return None; }
        let mut a = v.clone();
        let med = calib::median(&mut a)?;
        let mut d: Vec<f64> = v.iter().map(|x| (x - med).abs()).collect();
        Some(1.4826 * calib::median(&mut d)?)
    };
    let mut out = BTreeMap::new();
    for o in 0..4 {
        let mo: Vec<Metric> = ms.iter().copied().filter(|m| obj_index(*m) == o).collect();
        if mo.is_empty() { continue; }
        let sds: Vec<Option<f64>> = mo.iter().map(|m| sd(*m)).collect();
        if sds.iter().any(Option::is_none) { for m in &mo { out.insert(*m, 1.0); } continue; }
        let var: Vec<f64> = sds.iter().map(|x| x.unwrap().powi(2)).collect();
        let mean = (var.iter().sum::<f64>() / var.len() as f64).max(1e-6);
        let raw: Vec<f64> = var.iter().map(|v| 1.0 / (v + mean)).collect();
        let avg = raw.iter().sum::<f64>() / raw.len() as f64;
        for (m, w) in mo.iter().zip(&raw) { out.insert(*m, (w / avg).clamp(1.0 / 3.0, 3.0)); }
    }
    out
}

/// Runs -> rows: per metric the log-ratio to the session's reference runs (all runs when there
/// are fewer than 3), sign so that + is better, each clipped to +-0.5 (a stray outlier cannot
/// dominate), averaged per objective with the metric's noise weight (`metric_weights`) and brought
/// to the objective's scale (`calib::objective_gain`: the gain summed over the objective's metrics,
/// not diluted by the ones a knob does not touch). A metric
/// counts only if at least 80 % of the runs have it, so every run is judged on the same set.
/// Each row keeps its run's time (drift model) and weight.
fn compute_rows(runs: &[Run], phase: Phase, sess: u64, kernel: &str, n_total: usize) -> Vec<Row> {
    if runs.len() < 4 { return Vec::new(); }
    let refs: Vec<&Run> = runs.iter().filter(|r| r.cfg.is_empty()).collect();
    let base_runs: Vec<&Run> = if refs.len() >= 3 { refs.clone() } else { runs.iter().collect() };
    let mut base: BTreeMap<Metric, f64> = BTreeMap::new();
    for m in Metric::ALL {
        if runs.iter().filter(|r| r.s.contains_key(&m)).count() * 5 < runs.len() * 4 { continue; }
        let mut v: Vec<f64> = base_runs.iter().filter_map(|r| r.s.get(&m).copied()).collect();
        if let Some(b) = calib::median(&mut v) { base.insert(m, b); }
    }
    // Weights over the metrics this phase actually measured (the others are not in any row).
    let wts = metric_weights(&refs, &base.keys().copied().collect::<Vec<_>>());
    let mut gain = [0usize; 4];
    for m in base.keys() { gain[obj_index(*m)] += 1; }
    let gain = gain.map(calib::objective_gain);
    runs.iter().map(|r| {
        let mut acc = [(0.0f64, 0.0f64); 4];
        for (m, b) in &base {
            let Some(&v) = r.s.get(m) else { continue };
            let e = m.eps();
            if v + e <= 0.0 || b + e <= 0.0 { continue; }
            let mut d = ((v + e) / (b + e)).ln();
            if !m.higher_better() { d = -d; }
            let (i, w) = (obj_index(*m), wts.get(m).copied().unwrap_or(1.0));
            acc[i].0 += w * d.clamp(-0.5, 0.5); acc[i].1 += w;
        }
        let mut y = [f64::NAN; 4];
        for i in 0..4 { if acc[i].1 > 0.0 { y[i] = acc[i].0 / acc[i].1 * gain[i]; } }
        Row { phase, sess, pos: (r.order as f64 / n_total.max(runs.len()) as f64).min(1.0), t: r.t, kernel: kernel.into(), cfg: r.cfg.clone(), y, w: r.w, bv: calib::BENCH_VERSION }
    }).collect()
}

/// A finished run: sample, OOM kill, evidence weight (a busy machine counts less), time.
struct Done { s: Sample, oom: bool, w: f64, t: u64 }

struct Runner<'a> { j: Journal, c: &'a Ctx, rate: u64, ram: u64, ballast: Option<Ballast>, restarts: u32, runs: usize, t0: Instant, busy: usize,
                    /// Settings every run starts from (a run that changes the same key overrides them).
                    base: Cfg }

impl Runner<'_> {
    fn need_ballast(&mut self) -> Result<(), String> {
        let dead = match self.ballast.as_mut() { None => true, Some(b) => !b.alive() };
        if !dead { return Ok(()); }
        if self.ballast.is_some() {
            self.restarts += 1;
            let _ = self.j.restore();
            if self.restarts > 2 { return Err("ballast stopped by the brakes repeatedly: load phase ends here".into()); }
        } else { eprintln!("\nbuilding memory pressure (with fragmented free memory) ..."); }
        let b = Ballast::start(&self.c.exe)?;
        eprintln!("ballast holds {} MiB; {} MiB left available", b.held_mib, bench::meminfo_kb("MemAvailable:").unwrap_or(0) / 1024);
        self.ballast = Some(b);
        Ok(())
    }
    /// One run of `cfg` (all other knobs at their reference). None = a value could not be written.
    fn run(&mut self, phase: Phase, b: Benches, cfg: &Cfg, note: &str) -> Result<Option<Done>, String> {
        if STOP.load(Ordering::SeqCst) { let _ = self.j.restore(); return Err("interrupted".into()); }
        if phase == Phase::Load { self.need_ballast()?; }
        // No memory pressure behind the storage suite: the ballast would turn it into a swap test.
        if phase != Phase::Load { if let Some(b) = self.ballast.take() { b.stop(); } }
        self.runs += 1;
        let names: Vec<String> = cfg.iter().take(3).map(|(k, v)| format!("{k}={v}")).collect();
        eprint!("\r[{:>3} runs {:>4.1} min] {:<4} {:<64}", self.runs, self.t0.elapsed().as_secs_f64() / 60.0, phase.name(),
                if cfg.is_empty() { format!("reference {note}") } else { format!("{} change(s): {}{}", cfg.len(), names.join(" "), if cfg.len() > 3 { " ..." } else { "" }) });
        let _ = std::io::stderr().flush();
        let mut todo: Vec<(String, String)> = self.base.iter().filter(|(k, _)| !cfg.iter().any(|(c, _)| c == k)).cloned().collect();
        todo.extend(cfg.iter().cloned());
        // The elevator first: switching it resets nr_requests and (bfq) writeback throttling.
        todo.sort_by_key(|(k, _)| k != "blk.scheduler");
        for (k, v) in &todo {
            if let Err(e) = apply(&mut self.j, k, v, self.rate, self.ram) {
                eprintln!("\n{k} = {v}: {e} — run skipped");
                let _ = self.j.restore();
                return Ok(None);
            }
        }
        let skip: Vec<u32> = self.ballast.as_ref().map(|b| b.child.id()).into_iter().collect();
        let (fg, t1) = (bench::Foreign::snapshot(&skip), Instant::now());
        let r = match phase { Phase::Idle => run_idle(b, self.c).map(|s| (s, false)), Phase::Load => run_load(b, self.c),
                              Phase::Io => run_io(self.c).map(|s| (s, false)) };
        let busy = fg.busy_cpus(&skip, t1.elapsed().as_secs_f64());
        for e in self.j.restore() { eprintln!("\nrestore: {e}"); }
        // Other programs keeping more than 0.6 CPU busy disturb the figures: such a run counts less.
        let w = if busy > 0.6 { (0.6 / busy).clamp(0.1, 1.0) } else { 1.0 };
        if w < 1.0 { self.busy += 1; if self.busy <= 3 || self.busy % 10 == 0 { eprintln!("\nother programs kept {busy:.1} CPU(s) busy: run counted at {:.0} % ({} such run(s))", w * 100.0, self.busy); } }
        match r { Ok((s, oom)) => Ok(Some(Done { s, oom, w, t: now() })), Err(e) => { eprintln!("\n{e}"); Ok(None) } }
    }
}

/// A run that caused an OOM kill: halve its changes until the smallest set that still does is found.
fn oom_cull(cal: &mut Calibration, rn: &mut Runner, phase: Phase, b: Benches, cfg: &Cfg, left: &mut usize, refs: &BTreeMap<String, String>, kernel: &str) -> Result<(), String> {
    if cfg.len() == 1 {
        let (k, v) = &cfg[0];
        cal.add(k, refs.get(k).map_or("", String::as_str), v, phase, None, true, now(), kernel);
        cal.add_unsafe_set(cfg.clone());
        eprintln!("\n{k} = {v} caused an OOM kill: never picked");
        return Ok(());
    }
    let mid = cfg.len() / 2;
    let mut found = false;
    for half in [cfg[..mid].to_vec(), cfg[mid..].to_vec()] {
        if *left == 0 { break; }
        *left -= 1;
        if let Some(Done { oom: true, .. }) = rn.run(phase, b, &half, "(OOM search)")? { found = true; oom_cull(cal, rn, phase, b, &half, left, refs, kernel)?; }
    }
    if !found { cal.add_unsafe_set(cfg.clone()); eprintln!("\n{} changes together caused an OOM kill: that combination is never picked", cfg.len()); }
    Ok(())
}

struct Analysis { risk: f64, doubt_pairs: usize, optima: Vec<(Goal, Cfg, f64, f64)>, picks: Vec<Cfg>, r2: f64, cover: f64 }

/// A goal's objective weights in one phase: the IO phase's objectives count with the storage
/// weight, exactly as autotune weighs them, so the margin means the same in both.
fn goal_wts(g: Goal, phase: Phase) -> [f64; 4] {
    let w = Weights::for_goal(g);
    let k = if phase == Phase::Io { w.storage } else { 1.0 };
    [w.latency * k, w.throughput * k, w.power * k, w.footprint * k]
}

/// Every goal's utility over one phase's per-objective fits (no refit per goal).
fn goal_models(space: &Arc<model::Space>, fits: &[Option<Arc<model::Fit>>; 4], phase: Phase) -> Vec<(Goal, Model)> {
    Goal::ALL.iter().filter_map(|&g| Model::new(space.clone(), fits.clone(), goal_wts(g, phase)).map(|m| (g, m))).collect()
}

/// Per goal: the best combination, how much doubt is left in its decisions (and, with
/// `pair_k`, in the interactions among the knobs that matter), and which experiments would
/// remove the most of it. `crowd`: largest random experiment in the candidate pool.
fn analyse(models: Vec<(Goal, Model)>, phase: Phase, factors: &[Factor], bad: &[Cfg], want: usize, pair_k: usize, crowd: usize, rng: &mut Rng) -> Option<Analysis> {
    let (r2, cover) = (models.first()?.1.r2(), models.first()?.1.cover90());
    let joints: Vec<(Goal, Joint)> = models.into_iter().map(|(g, m)| {
        let (mut idle, mut load, mut io) = (None, None, None);
        match phase { Phase::Idle => idle = Some(m), Phase::Load => load = Some(m), Phase::Io => io = Some(m) }
        (g, Joint { idle, load, io, share: 0.5 })
    }).collect();
    let keys: Vec<String> = factors.iter().map(|f| f.key.clone()).collect();
    let cands: Vec<Vec<(String, f64)>> = factors.iter().map(|f| {
        let mut v = vec![(f.reference.clone(), 0.0)];
        for x in &f.values { v.push((x.clone(), model::modest_cost(&f.reference, x))); }
        v
    }).collect();
    let probs: Vec<Problem> = joints.iter().map(|(_, j)| {
        let mut p = Problem::new(j, keys.clone(), cands.clone(), Vec::new(), model::RISK_Z, MARGIN, bad.to_vec());
        p.crowd = crowd;
        p
    }).collect();
    let (mut risk, mut doubt_pairs, mut optima, mut targets, mut pool) = (0.0, 0, Vec::new(), Vec::new(), Vec::<Cfg>::new());
    for ((g, j), p) in joints.iter().zip(&probs) {
        let mut sel = p.optimize(&vec![0; keys.len()], rng, 6);
        p.prune(&mut sel, model::RISK_Z, MARGIN);
        let mut t = p.targets(&sel, MARGIN);
        risk += t.iter().map(|t| t.pwrong).sum::<f64>();
        let pt = p.pair_targets(&sel, MARGIN, pair_k);
        risk += 0.5 * pt.iter().map(|t| t.pwrong).sum::<f64>();
        doubt_pairs += pt.iter().filter(|t| t.pwrong > 0.2).count();
        t.extend(pt);
        pool.extend(p.pool(&sel, &t, 40, rng));
        let cfg = p.cfg(&sel);
        let (mu, var) = j.eval(&cfg);
        optima.push((*g, cfg, mu, var.sqrt()));
        targets.push(t);
    }
    pool.retain(|c| !c.is_empty());
    pool.sort(); pool.dedup();
    let mut score = vec![0.0; pool.len()];
    for (p, t) in probs.iter().zip(&targets) { for (i, a) in p.acquisition(t, &pool).into_iter().enumerate() { score[i] += a; } }
    let mut picks = Vec::new();
    for _ in 0..want {
        let Some((bi, &bv)) = score.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else { break };
        if bv <= 0.0 { break; }
        let c = pool[bi].clone();
        for (i, p) in pool.iter().enumerate() { let sh = p.iter().filter(|kv| c.contains(kv)).count(); if sh > 0 { score[i] *= 0.5f64.powi(sh as i32); } }
        score[bi] = -1.0;
        picks.push(c);
    }
    Some(Analysis { risk, doubt_pairs, optima, picks, r2, cover })
}

fn save(cal: &Calibration) {
    let _ = lpm_helpers::secure_dir(lpm_helpers::defaults::DIR);
    let _ = lpm_helpers::write_root_file(calib::FILE, &serde_json::to_vec(&cal.to_json()).unwrap());
}

/// Benchmarks of a phase's runs: idle/load measure CPU, memory and (idle) power on every run,
/// whatever the knobs, so every goal is decided from the same data; the IO phase runs the
/// storage suite alone.
fn union_benches(phase: Phase, its: &[&Item]) -> Benches {
    if phase == Phase::Io { return Benches { io: true, ..Default::default() }; }
    let mut b = Benches { cpu_mem: true, cpu: true, ..Default::default() };
    for i in its { b.io |= i.benches.io; b.idle |= i.benches.idle; }
    b
}

/// A numeric value on a knob's own grid: kHz caps in 100 MHz steps, big counts in thousands,
/// the dirty window in hundredths; None = not a numeric knob.
fn round_dose(key: &str, x: f64) -> Option<String> {
    if key == "vm.dirty" { return Some(format!("{:.2}", x).trim_end_matches('0').trim_end_matches('.').to_owned()); }
    let t = tune::find(key)?;
    let tune::Kind::Int { min, max } = &t.kind else { return None };
    let step = if x >= 1e6 { 100_000.0 } else if x >= 1e4 { 1000.0 } else if x >= 200.0 { 10.0 } else { 1.0 };
    Some((((x / step).round() * step) as i64).clamp(*min, *max).to_string())
}

/// Dose refinement: for the numeric knobs of the predicted optima, the midpoints (geometric
/// where both ends are positive) between the chosen dose and its measured neighbours become
/// new levels, each tried inside that optimum. At most `max` experiments; `factors` grow.
fn ladder(factors: &mut [Factor], optima: &[(Goal, Cfg, f64, f64)], bad: &[Cfg], max: usize) -> Vec<Cfg> {
    let mut out: Vec<Cfg> = Vec::new();
    for (_, cfg, _, _) in optima {
        for (k, v) in cfg {
            if out.len() >= max { return out; }
            let Some(f) = factors.iter_mut().find(|f| f.key == *k) else { continue };
            let Ok(x) = v.parse::<f64>() else { continue };
            let mut lv: Vec<f64> = std::iter::once(&f.reference).chain(&f.values).filter_map(|s| s.parse().ok()).collect();
            if lv.len() < 2 { continue; }
            lv.sort_by(|a, b| a.total_cmp(b)); lv.dedup();
            let Some(i) = lv.iter().position(|l| (l - x).abs() < 1e-9) else { continue };
            for n in [i.checked_sub(1), Some(i + 1)].into_iter().flatten().filter(|&n| n < lv.len()) {
                let (a, b) = (lv[n].min(x), lv[n].max(x));
                let m = if a > 0.0 { (a * b).sqrt() } else { (a + b) / 2.0 };
                let Some(mv) = round_dose(k, m) else { continue };
                let Ok(mx) = mv.parse::<f64>() else { continue };
                if lv.iter().any(|l| (l - mx).abs() < 1e-9) || f.values.contains(&mv) || mv == f.reference { continue; }
                let c: Cfg = cfg.iter().map(|(kk, vv)| (kk.clone(), if kk == k { mv.clone() } else { vv.clone() })).collect();
                if bad.iter().any(|u| u.iter().all(|kv| c.contains(kv))) || out.contains(&c) { continue; }
                f.values.push(mv);
                out.push(c);
                if out.len() >= max { return out; }
            }
        }
    }
    out
}

/// Interactions the model is sure about, pairs and (when modelled) triples.
fn report_interactions(m: &Model, k: usize, indent: &str) {
    let orders: Vec<usize> = if m.space.order >= 3 { vec![2, 3] } else { vec![2] };
    for ord in orders {
        for i in m.interactions(ord, k).into_iter().filter(|i| i.mean.abs() > 1.5 * i.sd) {
            let po: Vec<String> = ["lat", "thr", "pwr", "mem"].iter().zip(i.per_obj).filter_map(|(n, x)| x.filter(|x| x.abs() >= 0.002).map(|x| format!("{n} {:+.1}%", x * 100.0))).collect();
            println!("{indent}{} {:<64} {:+.3} ± {:.3} {:<8} {}", if ord == 2 { "pair  " } else { "triple" }, i.label(), i.mean, i.sd, i.kind(), po.join(" "));
        }
    }
}

struct Design<'a> { phase: Phase, bs: Benches, sess: u64, n_total: usize, kernel: &'a str, refs: &'a BTreeMap<String, String>, runs: Vec<Run>, oom_left: usize,
                    t0: Instant, secs: f64, spent: f64 }

impl Design<'_> {
    fn exec(&mut self, cal: &mut Calibration, rn: &mut Runner, cfg: &Cfg) -> Result<(), String> {
        let t = Instant::now();
        match rn.run(self.phase, self.bs, cfg, "")? {
            Some(Done { s, oom: false, w, t }) => { let order = self.runs.len(); self.runs.push(Run { cfg: cfg.clone(), s, order, t, w }); }
            Some(Done { oom: true, .. }) => {
                let start = self.oom_left.min(6);
                let mut left = start;
                oom_cull(cal, rn, self.phase, self.bs, cfg, &mut left, self.refs, self.kernel)?;
                self.oom_left -= start - left;
            }
            None => {}
        }
        self.spent += t.elapsed().as_secs_f64();
        Ok(())
    }
    /// Runs that still fit in the phase's time (measured cost per run, not the estimate).
    fn left(&self) -> usize {
        if !self.secs.is_finite() { return usize::MAX; }
        let per = if self.runs.len() >= 4 { self.spent / self.runs.len() as f64 } else { self.secs / self.n_total as f64 };
        ((self.secs - self.t0.elapsed().as_secs_f64()).max(0.0) / per.max(0.5)) as usize
    }
    fn commit(&self, cal: &mut Calibration) {
        cal.put_session(self.phase, self.sess, compute_rows(&self.runs, self.phase, self.sess, self.kernel, self.n_total));
        save(cal);
    }
}

fn phase_analysis(cal: &Calibration, phase: Phase, factors: &[Factor], dp: &Depth, want: usize, rng: &mut Rng) -> Option<Analysis> {
    let pf = cal.phase_fit(phase)?;
    analyse(goal_models(&pf.set.space, &pf.fits, phase), phase, factors, &cal.unsafe_sets, want, dp.pairs, dp.crowd, rng)
}

fn design_phase(cal: &mut Calibration, rn: &mut Runner, phase: Phase, its: &[&Item], budget: f64, dp: &Depth, kernel: &str, seed: u64, confirm: bool,
                refs: &BTreeMap<String, String>) -> Result<(), String> {
    let name = phase.name();
    let mut factors: Vec<Factor> = its.iter().map(|i| Factor { key: i.key.clone(), reference: i.reference.clone(), values: i.values.clone() }).collect();
    let bs = union_benches(phase, its);
    // vm.dirty's reference is window "1" (what autotune reads it as), not the live limits: every run of
    // this phase starts from it, so the reference runs are what the label says.
    let dirty_ok = tune::find("vm.dirty_bytes").map_or(false, |t| !tune::files(t).is_empty());
    rn.base = factors.iter().filter(|f| f.key == "vm.dirty" && dirty_ok).map(|f| (f.key.clone(), f.reference.clone())).collect();
    let cost = est_secs(phase, bs, rn.c.scale).max(1.0);
    let n_total = ((budget / cost) as usize).clamp(12, dp.cap);
    let n_init = ((n_total as f64 * dp.init) as usize).clamp(8, n_total);
    let sess = now() + phase.idx() as u64;
    let mut rng = Rng::new(seed ^ sess);
    let mut d = Design { phase, bs, sess, n_total, kernel, refs, runs: Vec::new(), oom_left: 12, t0: Instant::now(), secs: budget, spent: 0.0 };
    let prior: Vec<Cfg> = cal.rows.iter().filter(|r| r.phase == phase).map(|r| r.cfg.clone()).collect();
    eprintln!("\n{name} design ({}): {} knob(s), up to {n_total} runs of ~{cost:.0} s ({n_init} space-filling{}, then chosen by decision doubt{})",
              dp.name, factors.len(), if dp.mix.crowd > 0.0 { format!(", {:.0} % crowded ({}-{} changes)", dp.mix.crowd * 100.0, dp.mix.crowd_k.0, dp.mix.crowd_k.1) } else { String::new() },
              if dp.pairs > 0 { " and interaction doubt" } else { "" });
    if !prior.is_empty() { eprintln!("{name}: {} earlier run(s) of this phase are in the log: the new runs fill their gaps", prior.len()); }
    let none: Cfg = Vec::new();

    // 1. Space-filling: balanced over knobs, values and knob pairs, crowded runs with the depth.
    d.exec(cal, rn, &none)?;
    let mut since = 0;
    let init: Vec<Cfg> = model::design(&factors, n_init, &mut rng, &dp.mix, &prior).into_iter().filter(|c| !cal.unsafe_cfg(c)).collect();
    for cfg in init {
        if d.secs.is_finite() && d.t0.elapsed().as_secs_f64() > d.secs * (dp.init + 0.1) { break; }
        d.exec(cal, rn, &cfg)?;
        since += 1;
        if since >= 6 { d.exec(cal, rn, &none)?; since = 0; }
    }
    d.exec(cal, rn, &none)?;
    d.commit(cal);
    cal.retune(phase, 2);
    save(cal);

    // 2. Sequential: each batch where it removes the most decision (and interaction) doubt.
    let confirm_runs = if confirm { 2 + 4 * dp.reps.min(2) } else { 0 };
    let reserve = confirm_runs + if dp.ladder > 0 { dp.ladder * 2 + 1 } else { 0 };
    let (mut batches, mut exploring) = (0, false);
    while d.runs.len() + dp.batch + 1 + reserve <= dp.cap && d.left() >= dp.batch + 1 + reserve {
        let Some(an) = phase_analysis(cal, phase, &factors, dp, dp.batch, &mut rng) else { break };
        eprintln!("\n{name}: {} runs, decision doubt {:.2}{}, model R² {:.2}, 90 % interval coverage {:.0} %", d.runs.len(), an.risk,
                  if dp.pairs > 0 { format!(" ({} doubtful interaction(s))", an.doubt_pairs) } else { String::new() }, an.r2, an.cover * 100.0);
        let settled = an.risk < 0.35 && an.doubt_pairs == 0;
        if settled && !dp.explore { eprintln!("{name}: decisions are settled, stopping early"); break; }
        // Settled (progressive) or nothing to ask: the time goes to coverage the log still lacks.
        let picks = if settled || an.picks.is_empty() {
            if settled && !exploring { eprintln!("{name}: decisions are settled; the rest of the session fills gaps ({})", dp.name); exploring = true; }
            let seen: Vec<Cfg> = prior.iter().cloned().chain(d.runs.iter().map(|r| r.cfg.clone())).collect();
            model::design(&factors, dp.batch, &mut rng, &dp.mix, &seen).into_iter().filter(|c| !cal.unsafe_cfg(c)).collect()
        } else { an.picks };
        for cfg in picks { if !cal.unsafe_cfg(&cfg) { d.exec(cal, rn, &cfg)?; } }
        d.exec(cal, rn, &none)?;
        d.commit(cal);
        batches += 1;
        if batches % 4 == 0 { cal.retune(phase, 1); save(cal); }
    }

    // 3. Dose refinement: midpoints of numeric ladders around the chosen doses.
    if dp.ladder > 0 && d.left() >= dp.ladder + confirm_runs {
        if let Some(an) = phase_analysis(cal, phase, &factors, dp, 0, &mut rng) {
            let tries = ladder(&mut factors, &an.optima, &cal.unsafe_sets, dp.ladder);
            if !tries.is_empty() {
                eprintln!("\n{name}: refining {} dose(s) between measured levels", tries.len());
                for (i, cfg) in tries.iter().enumerate() {
                    d.exec(cal, rn, cfg)?;
                    if dp.reps >= 3 { d.exec(cal, rn, cfg)?; }
                    if i % 3 == 2 { d.exec(cal, rn, &none)?; }
                }
                d.exec(cal, rn, &none)?;
                d.commit(cal);
            }
        }
    }

    // 4. Confirmation: measure what the models believe is best, per goal.
    if confirm && (d.left() >= 4 || !d.secs.is_finite()) {
        cal.retune(phase, 1);
        if let Some(an) = phase_analysis(cal, phase, &factors, dp, 0, &mut rng) {
            let mut uniq: Vec<(Cfg, Vec<(Goal, f64, f64)>)> = Vec::new();
            for (g, cfg, mu, sd) in an.optima.iter().filter(|o| !o.1.is_empty()) {
                match uniq.iter_mut().find(|(c, _)| c == cfg) { Some(u) => u.1.push((*g, *mu, *sd)), None => uniq.push((cfg.clone(), vec![(*g, *mu, *sd)])) }
            }
            let reps = if d.secs.is_finite() { dp.reps.min((d.left().saturating_sub(2) / uniq.len().max(1)).max(1)) } else { dp.reps };
            if !uniq.is_empty() {
                eprintln!("\n{name}: confirming {} predicted optimum(s), {reps} run(s) each", uniq.len());
                d.exec(cal, rn, &none)?;
                for r in 0..reps { for (cfg, _) in &uniq { d.exec(cal, rn, cfg)?; } if r % 2 == 1 { d.exec(cal, rn, &none)?; } }
                d.exec(cal, rn, &none)?;
                d.commit(cal);
                let rows = compute_rows(&d.runs, phase, sess, kernel, n_total);
                for (cfg, gs) in &uniq {
                    for (g, mu, sd) in gs {
                        let ws = goal_wts(*g, phase);
                        let obs: Vec<f64> = rows.iter().filter(|r| &r.cfg == cfg).map(|r| (0..4).map(|o| if r.y[o].is_nan() { 0.0 } else { ws[o] * r.y[o] }).sum()).collect();
                        if obs.is_empty() { continue; }
                        let m = obs.iter().sum::<f64>() / obs.len() as f64;
                        let tol = 2.0 * (sd * sd + 0.02f64.powi(2) / obs.len() as f64).sqrt();
                        eprintln!("  {:<30} {} change(s): predicted {:+.3} ± {:.3}, measured {:+.3} ({} runs) {}", g.label(), cfg.len(), mu, sd, m, obs.len(),
                                  if (m - mu).abs() <= tol { "as predicted" } else { "SURPRISE (now part of the data)" });
                    }
                }
            }
        }
    }
    cal.retune(phase, 2);
    if d.runs.len() >= 10 { cal.note_strategy(phase, dp.name); }
    save(cal);
    Ok(())
}

/// Phases with at least two testable knobs and their share of the time budget (each knob gets a similar number of runs).
fn split(items: &[Item], scale: f64, budget: f64) -> Vec<(Phase, Vec<&Item>, f64)> {
    let mut v: Vec<(Phase, Vec<&Item>, f64)> = Phase::ALL.into_iter()
        .map(|ph| (ph, items.iter().filter(|i| i.phase == ph).collect::<Vec<_>>(), 0.0)).filter(|(_, its, _)| its.len() >= 2).collect();
    let w: Vec<f64> = v.iter().map(|(ph, its, _)| its.len() as f64 * est_secs(*ph, union_benches(*ph, its), scale)).collect();
    let sum: f64 = w.iter().sum();
    for (e, w) in v.iter_mut().zip(&w) { e.2 = if budget.is_finite() { budget * w / sum } else { f64::INFINITY }; }
    v
}

fn phase_factors(its: &[&Item]) -> Vec<Factor> { its.iter().map(|i| Factor { key: i.key.clone(), reference: i.reference.clone(), values: i.values.clone() }).collect() }

fn design_main(cal: &mut Calibration, items: &[Item], ctx: &Ctx, rate: u64, ram: u64, kernel: &str, budget: f64, dp: &Depth, progressive: bool, seed: u64, confirm: bool) {
    let refs: BTreeMap<String, String> = items.iter().map(|i| (i.key.clone(), i.reference.clone())).collect();
    for (k, r) in &refs {
        if let Some(old) = cal.refs.get(k) { if old != r { eprintln!("{k}: reference changed ({old} -> {r}): earlier design rows of this knob are dropped"); cal.forget_key(k); } }
        cal.refs.insert(k.clone(), r.clone());
    }
    let phases = split(items, ctx.scale, budget);
    if phases.is_empty() { eprintln!("nothing to design: fewer than two testable knobs"); return; }
    let mut rn = Runner { j: Journal { entries: Vec::new() }, c: ctx, rate, ram, ballast: None, restarts: 0, runs: 0, t0: Instant::now(), busy: 0, base: Vec::new() };
    for (ph, its, secs) in &phases {
        let pdp = if progressive {
            let p = progress(cal, *ph, &phase_factors(its));
            eprintln!("\n{}: progressive stage {:?} - {}\n  log: {}", ph.name(), p.stage(), p.stage().what(), p.line());
            Depth::progressive(p.stage(), its.len(), p.polish)
        } else { *dp };
        if let Err(e) = design_phase(cal, &mut rn, *ph, its, *secs, &pdp, kernel, seed, confirm, &refs) { eprintln!("\n{e}"); if STOP.load(Ordering::SeqCst) { break; } }
    }
    for e in rn.j.restore() { eprintln!("restore: {e}"); }
    if let Some(b) = rn.ballast.take() { b.stop(); }
    save(cal);
    eprintln!();
    if STOP.load(Ordering::SeqCst) { println!("interrupted: measured runs are saved, everything is restored"); }
    for ph in Phase::ALL {
        let its: Vec<&Item> = items.iter().filter(|i| i.phase == ph).collect();
        let Some(pf) = cal.phase_fit(ph) else { continue };
        let factors: Vec<Factor> = its.iter().map(|i| Factor { key: i.key.clone(), reference: i.reference.clone(), values: i.values.clone() }).collect();
        let Some(an) = analyse(goal_models(&pf.set.space, &pf.fits, ph), ph, &factors, &cal.unsafe_sets, 0, dp.pairs, dp.crowd, &mut Rng::new(seed)) else { continue };
        println!("{}: {} run(s), model R² {:.2}, 90 % interval coverage {:.0} %, decision doubt {:.2}, interactions up to {}", ph.name(),
                 pf.set.nrows(), an.r2, an.cover * 100.0, an.risk, if pf.set.space.order >= 3 { "triples" } else { "pairs" });
        for (g, cfg, mu, sd) in &an.optima { println!("  {:<30} {} change(s), predicted {:+.3} ± {:.3}", g.label(), cfg.len(), mu, sd); }
        if let Some(m) = cal.phase_model(ph, [1.0; 4]) { report_interactions(&m, 5, "  "); }
    }
    if progressive {
        for (ph, its, _) in &phases {
            let p = progress(cal, *ph, &phase_factors(its));
            println!("{}: next lean session: stage {:?} ({})", ph.name(), p.stage(), p.line());
        }
    }
    println!("signature: {} design run(s) stored. `lpm-calibrate --show` for effects and interactions; run again to add evidence.", cal.rows.len());
    println!("autotune decides all calibrated knobs jointly from it on its next run (GUI Autotune or lpm-autotune <goal>).");
}

fn show_model(c: &Calibration) {
    if c.rows.is_empty() { return; }
    println!("\nexperiment log: {} run(s), {} unsafe combination(s)", c.rows.len(), c.unsafe_sets.len());
    for ph in Phase::ALL {
        let Some(m) = c.phase_model(ph, [1.0; 4]) else { continue };
        println!("  {:<5} {} knob(s), {} run(s), noise {:.3}, model R² {:.2}, 90 % interval coverage {:.0} %, interactions up to {}", ph.name(),
                 m.space.factors.len(), m.nobs(), m.noise(), m.r2(), m.cover90() * 100.0, if m.space.order >= 3 { "triples" } else { "pairs" });
        report_interactions(&m, 8, "        ");
    }
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
                for ph in Phase::ALL {
                    let Some(m) = c.get_phase(k, &g.reference, v, ph) else { continue };
                    println!("  {k:<30} {:<24} {:<5} {} {} {} {} {:>5.1}{}", format!("{v} vs {}", g.reference),
                             ph.name(), pct(m.lat), pct(m.thr), pct(m.pwr), pct(m.mem), m.n,
                             if s.unsafe_ { "  UNSAFE (OOM kill)" } else { "" });
                }
            }
        }
    }
    show_model(c);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Disposable children: no machine checks, just measure / hold memory.
    if args.first().map(String::as_str) == Some("__probe") {
        println!("{}", bench::probe_main(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(512)));
        return;
    }
    if args.first().map(String::as_str) == Some("__job") {
        bench::job_main(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000));
        return;
    }
    if args.first().map(String::as_str) == Some("__ballast") {
        let n = |i: usize, d: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
        bench::ballast_main(n(1, 0) as usize, n(2, 1) as usize, n(3, 1024), n(4, 0) == 1);
    }
    lpm_helpers::init();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let mut cal = Calibration::load().unwrap_or_else(|| Calibration::new(calib::Fingerprint::current()));
    if flag("--show") {
        if cal.keys.is_empty() && cal.rows.is_empty() { println!("no signature yet (sudo lpm-calibrate)"); } else { show(&cal); }
        return;
    }
    let thorough = flag("--thorough");
    let (rounds, scale) = if thorough { (2, 1.6) } else { (1, 1.0) };
    let only: Option<Vec<String>> = opt("--only").map(|s| s.split(',').map(str::to_owned).collect());
    let phase = opt("--phase").unwrap_or_else(|| "both".into());
    let budget = if flag("--all") { f64::INFINITY } else { opt("--budget").and_then(|s| s.parse::<f64>().ok()).unwrap_or(15.0) * 60.0 };
    let ram = bench::meminfo_kb("MemTotal:").unwrap_or(0) * 1024;
    // The storage suite's disk: --dir, else the first of these on a local disk (not tmpfs).
    let dir = opt("--dir").unwrap_or_else(|| ["/var/tmp", "/var/cache", "/home", "/"].iter()
        .find(|d| bench::disk_of(d).is_ok()).unwrap_or(&"/var/tmp").to_string());
    let disk = bench::disk_of(&dir);
    let (items, skipped) = plan(rounds, scale, ram, &only, &phase, &cal, &disk);
    let total_secs: f64 = items.iter().map(|i| i.secs).sum();
    let depth_opt = opt("--depth");
    if let Some(d) = &depth_opt { if !["lean", "deep", "max"].contains(&d.as_str()) { die("--depth: lean, deep or max"); } }
    let nf = Phase::ALL.iter().map(|ph| items.iter().filter(|i| i.phase == *ph).count()).max().unwrap_or(0);
    let dp = Depth::pick(budget / 60.0, nf, depth_opt.as_deref());
    // Short sessions without an explicit depth build on each other (see Stage).
    let progressive = depth_opt.is_none() && dp.name == "lean";
    if flag("--list") {
        for i in &items {
            println!("  {:<5} {:<30} ref {:<20} try {:<40} ~{:>4.0} s  measured {}x", i.phase.name(),
                     i.key, i.reference, i.values.join(","), i.secs, cal.coverage(&i.key, i.phase));
        }
        if flag("--oat") {
            println!("\n{} measurements, ~{:.0} min in total; not tested:", items.len(), total_secs / 60.0);
        } else {
            println!();
            if progressive {
                for (ph, its, _) in split(&items, scale, budget) {
                    let p = progress(&cal, ph, &phase_factors(&its));
                    let d = Depth::progressive(p.stage(), its.len(), p.polish);
                    println!("  {:<5} progressive stage {:?} ({}): {}\n        log: {}", ph.name(), p.stage(), d.name, p.stage().what(), p.line());
                }
                println!("  (lean sessions build on each other; --depth lean runs the fixed lean shape)");
            }
            if !progressive { println!("depth {}: space-filling {:.0} % of the runs ({:.0} % inside one cluster, {}-{} changes elsewhere{}), batches of {}, {} confirmation run(s) per optimum{}{}",
                     dp.name, dp.init * 100.0, dp.mix.cluster * 100.0, dp.mix.spread_k.0, dp.mix.spread_k.1,
                     if dp.mix.crowd > 0.0 { format!(", {:.0} % crowded with {}-{}", dp.mix.crowd * 100.0, dp.mix.crowd_k.0, dp.mix.crowd_k.1) } else { String::new() },
                     dp.batch, dp.reps, if dp.pairs > 0 { format!(", up to {} doubtful interactions chased", dp.pairs) } else { String::new() },
                     if dp.ladder > 0 { format!(", up to {} dose refinements", dp.ladder) } else { String::new() }); }
            for (ph, its, secs) in split(&items, scale, budget) {
                let cost = est_secs(ph, union_benches(ph, &its), scale).max(1.0);
                let logged = cal.rows.iter().filter(|r| r.phase == ph).count();
                println!("  {:<5} {} knob(s), ~{:.0} s per run, up to {} runs in {:.0} min ({} logged; interactions: all pairs{})", ph.name(), its.len(), cost,
                         ((secs / cost) as usize).clamp(12, dp.cap), secs.min(1e6) / 60.0, logged,
                         format!(", all triples from {} runs", 150.max(4 * its.len())));
            }
            match &disk { Ok(d) => println!("storage suite on {dir} ({d}); --dir PATH measures another disk"), Err(e) => println!("storage suite off: {e}") }
            println!("sequential design: every run changes several knobs; the runs stop early once the decisions are settled. Not tested:");
        }
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
    let _ = lpm_helpers::secure_dir("/run/legion-power-manager");
    if let Err(e) = lpm_helpers::secure_dir("/run/legion-power-manager/tune") { die(&format!("lock dir: {e}")); }
    let lock = std::fs::OpenOptions::new().create(true).write(true).open(TUNE_LOCK).unwrap_or_else(|e| die(&format!("lock: {e}")));
    if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 { die("Legion Power Manager is applying something; try again"); }
    // A signature of other hardware is set aside, not merged.
    if cal.keys.is_empty() && cal.rows.is_empty() && std::path::Path::new(calib::FILE).exists() {
        let _ = std::fs::rename(calib::FILE, format!("{}.other-{}", calib::FILE, now()));
    }
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    let src = bench::power_source();
    let ctx = Ctx {
        exe: std::env::current_exe().unwrap_or_else(|_| die("cannot find own executable")),
        dir: dir.clone(),
        disk: disk.clone().ok(),
        io_size: if thorough { 512 } else { 256 } << 20,
        scale,
        battery: src == "battery",
    };
    let rate = lpm_helpers::iorate::gather().bps;
    // Fixed work of the game loop and the short jobs, sized once, before any knob is touched.
    eprintln!("work unit: {} k iterations/ms at full clock (game loop: 4.8 ms main thread + 3 x 2.4 ms jobs per 8 ms frame)", bench::unit() / 1000);
    if cal.migrated { eprintln!("note: the stored idle/load measurements came from an older benchmark set that could not see most CPU and scheduler knobs; those phases start over (storage rows and unsafe values are kept)"); }
    if src != "battery" { eprintln!("note: on AC — power is RAPL (CPU package) only; device-level power needs a run on battery"); }
    match &ctx.disk {
        Some(d) => eprintln!("storage suite: {} on {d} ({} MiB written per IO run, unlinked temp files)", ctx.dir, ctx.io_size * 3 / 2 >> 20),
        None => eprintln!("storage suite off: {}", disk.as_ref().err().map_or("", String::as_str)),
    }
    cal.on_battery = src == "battery";
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_owned();

    if !flag("--oat") {
        let seed: u64 = opt("--seed").and_then(|s| s.parse().ok()).unwrap_or(0x1B_5EED);
        // --sessions N: N sessions back to back (walk away; progressive ones build on each other).
        let n = opt("--sessions").and_then(|s| s.parse::<u64>().ok()).unwrap_or(1).clamp(1, 48);
        for i in 0..n {
            if STOP.load(Ordering::SeqCst) { break; }
            if n > 1 { eprintln!("\n══ session {} of {n} ══", i + 1); }
            design_main(&mut cal, &items, &ctx, rate, ram, &kernel, budget, &dp, progressive, seed.wrapping_add(i * 0x9E37), !flag("--no-confirm"));
        }
        return;
    }
    // Budget: least-measured first until the time is used.
    let mut chosen = Vec::new();
    let mut spent = 0.0;
    for i in items {
        // The legacy flow keeps idle/load records only; storage is calibrated by the design.
        if i.phase == Phase::Io { continue; }
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
                let r = match it.phase { Phase::Idle => run_idle(it.benches, &ctx).map(|s| (s, false)), Phase::Load => run_load(it.benches, &ctx),
                                         Phase::Io => run_io(&ctx).map(|s| (s, false)) };
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
    let (left, _) = plan(rounds, scale, ram, &None, "both", &cal, &disk);
    let uncovered = left.iter().filter(|i| i.phase != Phase::Io && cal.coverage(&i.key, i.phase) == 0).count();
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
        // A CPU knob with 3 candidates: 5 runs x 6.7 s (frame loop, game loop, contended window and short jobs included).
        assert!(est_secs(Phase::Idle, cpu, 1.0) * runs_for(3, 1) as f64 <= 34.0);
        assert!(est_secs(Phase::Load, Benches { cpu: true, ..Default::default() }, 1.0) <= 4.3);
        assert!(est_secs(Phase::Io, Benches { io: true, ..Default::default() }, 1.0) < 6.0);
        assert!(selects("@io", "blk.read_ahead_kb", "Storage") && selects("@io", "vm.dirty", "Memory") && !selects("@io", "vm.swappiness", "Memory"));
        assert_eq!(union_benches(Phase::Io, &[]), Benches { io: true, ..Default::default() });
    }

    fn run(cfg: Cfg, s: &[(Metric, f64)]) -> Run { Run { cfg, s: s.iter().copied().collect(), order: 0, t: 0, w: 1.0 } }

    #[test]
    fn noisy_metrics_weigh_less_inside_their_objective() {
        // Reference runs: wake tail scatters +-30 %, frame tail +-2 %.
        let refs: Vec<Run> = [1.0, 1.3, 0.75, 1.25, 0.8, 1.0].iter().zip([1.0, 1.02, 0.98, 1.01, 0.99, 1.0])
            .map(|(a, b)| run(vec![], &[(Metric::WakeP99Us, 100.0 * a), (Metric::FrameP99Us, 500.0 * b)])).collect();
        let r: Vec<&Run> = refs.iter().collect();
        let w = metric_weights(&r, &[Metric::WakeP99Us, Metric::FrameP99Us]);
        // Two metrics, one far noisier: the shrinkage settles at 3:1 (1.5 / 0.5 of the equal share).
        assert!(w[&Metric::FrameP99Us] > 1.45 && w[&Metric::WakeP99Us] < 0.55, "{w:?}");
        // Too few references: equal weights.
        let w2 = metric_weights(&r[..3], &[Metric::WakeP99Us, Metric::FrameP99Us]);
        assert_eq!(w2[&Metric::WakeP99Us], 1.0);
        // A run 10 % better on the steady metric only now shows clearly more than half of it.
        let mut runs = refs;
        runs.push(run(vec![("k".into(), "1".into())], &[(Metric::WakeP99Us, 100.0), (Metric::FrameP99Us, 450.0)]));
        let rows = compute_rows(&runs, Phase::Idle, 1, "", 10);
        let y = rows.last().unwrap().y[0];
        assert!(y > 0.07, "{y}");
    }
    #[test]
    fn an_effect_on_one_metric_is_not_diluted_by_the_others() {
        // Eight latency metrics, a knob that makes the game's frame tail 10 % better and touches nothing else.
        let lat = [Metric::WakeP99Us, Metric::FrameP99Us, Metric::FrameMedUs, Metric::PingPongP99Us, Metric::GameTailMs,
                   Metric::BusyFrameP99Us, Metric::FaultP99Us, Metric::FaultHugeP99Us];
        let base = |game: f64| -> Vec<(Metric, f64)> { lat.iter().map(|m| (*m, if *m == Metric::GameTailMs { game } else { 100.0 })).collect() };
        let mut runs: Vec<Run> = (0..3).map(|_| run(vec![], &base(10.0))).collect();
        runs.push(run(vec![("k".into(), "1".into())], &base(9.0)));
        let y = compute_rows(&runs, Phase::Idle, 1, "", 10).last().unwrap().y[0];
        // The plain mean gave ln(1/0.9) / 8 = 1.3 %: under every margin. Summed over the metrics and halved: 5.3 %.
        assert!((y - (1.0f64 / 0.9).ln() / 2.0).abs() < 1e-9, "{y}");
        // The same knob measured in a session without the memory probes (6 metrics) lands on the same scale.
        let few = |game: f64| -> Vec<(Metric, f64)> { base(game).into_iter().filter(|(m, _)| !matches!(m, Metric::FaultP99Us | Metric::FaultHugeP99Us)).collect() };
        let mut runs: Vec<Run> = (0..3).map(|_| run(vec![], &few(10.0))).collect();
        runs.push(run(vec![("k".into(), "1".into())], &few(9.0)));
        let y2 = compute_rows(&runs, Phase::Idle, 1, "", 10).last().unwrap().y[0];
        assert!((y - y2).abs() < 1e-9, "{y} vs {y2}");
    }

    #[test]
    fn depth_grows_with_budget_and_ladders_refine_doses() {
        let (l, d, m) = (Depth::pick(15.0, 30, None), Depth::pick(40.0, 30, None), Depth::pick(f64::INFINITY, 30, None));
        assert_eq!((l.name, d.name, m.name), ("lean", "deep", "max"));
        assert!(l.cap < d.cap && d.cap < m.cap && l.mix.crowd == 0.0 && m.mix.crowd > d.mix.crowd && m.mix.crowd_k.1 <= 30);
        assert_eq!(Depth::pick(15.0, 30, Some("max")).name, "max");
        assert!(selects("@thp", "thp", "Memory") && selects("@thp", "thp.defrag", "Memory") && !selects("@thp", "vm.swappiness", "Memory"));
        assert!(selects("@sched", "sched.nr_migrate", "Scheduler") && selects("vm.swappiness", "vm.swappiness", "Memory"));
        // vm.dirty: 1 (ref) with 0.25, 0.5, 2 measured, optimum 0.5 -> geometric midpoints 0.35 and 0.71.
        let mut fs = vec![Factor { key: "vm.dirty".into(), reference: "1".into(), values: vec!["0.25".into(), "0.5".into(), "2".into()] }];
        let opt: Cfg = vec![("vm.dirty".into(), "0.5".into())];
        let tries = ladder(&mut fs, &[(Goal::Gaming, opt, 0.1, 0.01)], &[], 4);
        assert_eq!(tries, vec![vec![("vm.dirty".to_string(), "0.35".to_string())], vec![("vm.dirty".to_string(), "0.71".to_string())]]);
        assert_eq!(fs[0].values.len(), 5);
        assert!(ladder(&mut fs, &[(Goal::Gaming, vec![("vm.dirty".into(), "0.5".into())], 0.1, 0.01)], &[], 4).len() <= 2, "no level twice");
    }
    #[test]
    fn lean_sessions_progress_through_the_stages() {
        let mut fs: Vec<Factor> = (0..12).map(|i| Factor { key: format!("{}.k{i}", if i % 2 == 0 { "vm" } else { "cpu" }), reference: "0".into(), values: vec!["1".into()] }).collect();
        fs.push(Factor { key: "cpu.n2".into(), reference: "10".into(), values: vec!["5".into(), "20".into(), "40".into()] });
        fs.push(Factor { key: "vm.dirty".into(), reference: "1".into(), values: vec!["0.25".into(), "0.5".into(), "2".into()] });
        let mut cal = Calibration::default();
        let mut rng = Rng::new(3);
        let mut seen: Vec<Stage> = Vec::new();
        for sess in 1..=14u64 {
            let p = progress(&cal, Phase::Idle, &fs);
            let st = p.stage();
            assert!(seen.last().map_or(true, |l| *l <= st), "stages only move forward: {seen:?} then {st:?}");
            seen.push(st);
            let dp = Depth::progressive(st, fs.len(), p.polish);
            assert!(dp.explore && dp.cap == 240);
            let prior: Vec<Cfg> = cal.rows.iter().map(|r| r.cfg.clone()).collect();
            let mut cfgs = model::design(&fs, 40, &mut rng, &dp.mix, &prior);
            if dp.ladder > 0 { cfgs.extend(ladder(&mut fs.clone(), &[(Goal::Gaming, vec![("vm.dirty".into(), "0.5".into())], 0.1, 0.01)], &[], dp.ladder)); }
            let rows = cfgs.into_iter().map(|cfg| Row { phase: Phase::Idle, sess, pos: 0.5, t: sess * 1000, kernel: String::new(), cfg,
                                                          y: [0.0, f64::NAN, f64::NAN, f64::NAN], w: 1.0, bv: calib::BENCH_VERSION }).collect();
            cal.put_session(Phase::Idle, sess, rows);
            cal.note_strategy(Phase::Idle, dp.name);
        }
        eprintln!("stages: {seen:?}");
        for st in [Stage::Base, Stage::Pairs, Stage::Crowd, Stage::Refine, Stage::Polish] { assert!(seen.contains(&st) || st == Stage::Pairs, "{st:?} reached: {seen:?}"); }
        assert!(seen.iter().filter(|s| **s == Stage::Polish).count() >= 3, "then polish rotates: {seen:?}");
        let pol: Vec<&String> = cal.strategies_of(Phase::Idle).iter().filter(|s| s.starts_with("lean/polish")).collect();
        let kinds: std::collections::BTreeSet<&String> = pol.iter().copied().collect();
        assert!(kinds.len() == 3.min(pol.len()), "polish rotates its three strategies: {pol:?}");
    }
}

#[cfg(test)]
mod sim {
    //! Offline comparison on a synthetic, noisy machine: the same number of runs spent
    //! (a) one knob at a time (reference vs candidate, keep what beats the margin) and
    //! (b) by the sequential design (space-filling start, then runs chosen by decision doubt).
    use super::*;
    use lpm_helpers::model::PhaseSet;

    fn fac(k: &str, r: &str, v: &[&str]) -> Factor { Factor { key: k.into(), reference: r.into(), values: v.iter().map(|s| s.to_string()).collect() } }

    /// True utility of a configuration: mains, a synergy, a cross-cluster synergy, a redundancy, an antagonism and harmful knobs.
    /// Net utility of a configuration on a machine whose knobs matter (effects x2), minus the modesty cost of its changes.
    pub fn net(cfg: &Cfg, fs: &[Factor]) -> f64 {
        2.0 * truth(cfg) + cfg.iter().map(|(k, v)| model::modest_cost(&fs.iter().find(|f| f.key == *k).unwrap().reference, v)).sum::<f64>()
    }
    pub fn truth(cfg: &Cfg) -> f64 {
        let on = |k: &str| cfg.iter().any(|(a, _)| a == k);
        let is = |k: &str, v: &str| cfg.iter().any(|(a, b)| a == k && b == v);
        let mut y = 0.0;
        if on("vm.a") { y += 0.05 } if on("vm.b") { y += 0.04 }
        if on("vm.a") && on("vm.b") { y += 0.08 }
        if is("vm.c", "2") { y += 0.03 } if is("vm.c", "4") { y += 0.06 }
        if on("cpu.a") { y += 0.05 } if on("cpu.b") { y += 0.03 }
        if on("cpu.a") && on("cpu.c") { y += 0.06 }
        if is("vm.c", "4") && on("cpu.a") { y += 0.05 }
        if on("vm.d") { y += 0.06 } if on("vm.e") { y += 0.06 }
        if on("vm.d") && on("vm.e") { y -= 0.06 }
        if on("cpu.d") { y -= 0.05 } if on("vm.f") { y -= 0.04 }
        if on("cpu.b") && on("cpu.e") { y -= 0.08 } if on("cpu.e") { y += 0.03 }
        y
    }
    pub fn factors() -> Vec<Factor> {
        vec![fac("vm.a", "0", &["1"]), fac("vm.b", "0", &["1"]), fac("vm.c", "1", &["2", "4"]), fac("vm.d", "0", &["1"]), fac("vm.e", "0", &["1"]), fac("vm.f", "0", &["1"]),
             fac("vm.g", "0", &["1"]), fac("vm.h", "0", &["1"]), fac("cpu.a", "0", &["1"]), fac("cpu.b", "0", &["1"]), fac("cpu.c", "0", &["1"]), fac("cpu.d", "0", &["1"]),
             fac("cpu.e", "0", &["1"]), fac("cpu.f", "0", &["1"]), fac("cpu.g", "0", &["1"]), fac("cpu.h", "0", &["1"])]
    }
    pub fn noisy(cfg: &Cfg, rng: &mut Rng, sd: f64) -> f64 { 2.0 * truth(cfg) + ((rng.unit() + rng.unit() + rng.unit() + rng.unit()) - 2.0) * sd * 1.7 }

    fn one_at_a_time(fs: &[Factor], budget: usize, rng: &mut Rng, sd: f64) -> Cfg {
        let per = (budget / fs.len()).max(3);
        let mut cfg: Cfg = Vec::new();
        for f in fs {
            let nref = (per / (f.values.len() + 1)).max(1);
            let r: f64 = (0..nref + 1).map(|_| noisy(&Vec::new(), rng, sd)).sum::<f64>() / (nref + 1) as f64;
            let mut best: Option<(f64, String)> = None;
            for v in &f.values {
                let c: Cfg = vec![(f.key.clone(), v.clone())];
                let m = (0..nref + 1).map(|_| noisy(&c, rng, sd)).sum::<f64>() / (nref + 1) as f64 - r;
                if m + model::modest_cost(&f.reference, v) >= MARGIN && best.as_ref().map_or(true, |b| m > b.0) { best = Some((m, v.clone())); }
            }
            if let Some((_, v)) = best { cfg.push((f.key.clone(), v)); }
        }
        cfg
    }

    fn sequential(fs: &[Factor], budget: usize, rng: &mut Rng, sd: f64) -> (Cfg, usize) {
        let n_init = (budget as f64 * 0.55) as usize;
        let mut runs: Vec<(Cfg, f64)> = vec![(Vec::new(), noisy(&Vec::new(), rng, sd))];
        for (i, c) in model::initial_design(fs, n_init, rng).into_iter().enumerate() {
            let y = noisy(&c, rng, sd);
            runs.push((c, y));
            if i % 6 == 5 { runs.push((Vec::new(), noisy(&Vec::new(), rng, sd))); }
        }
        let rows = |runs: &[(Cfg, f64)]| -> Vec<model::Row> { runs.iter().enumerate().map(|(i, (c, y))| model::Row { cfg: c.clone(), y: [*y, f64::NAN, f64::NAN, f64::NAN], w: 1.0, sess: 1, pos: i as f64 / budget as f64, t: i as f64 * 6.0 }).collect() };
        let an_of = |runs: &[(Cfg, f64)], want: usize, rng: &mut Rng| -> Option<Analysis> {
            let ps = PhaseSet::build(fs.to_vec(), rows(runs))?;
            let fits = std::array::from_fn(|o| ps.fit_obj(o, None, 1));
            analyse(goal_models(&ps.space, &fits, Phase::Load), Phase::Load, fs, &[], want, 0, 6, rng)
        };
        let mut last: Option<Cfg> = None;
        while runs.len() + 6 <= budget {
            let Some(an) = an_of(&runs, 5, rng) else { break };
            last = an.optima.iter().find(|o| o.0 == Goal::Gaming).map(|o| o.1.clone());
            if an.risk < 0.35 { break; }
            let picks = if an.picks.is_empty() { model::initial_design(fs, 5, rng) } else { an.picks };
            for c in picks { let y = noisy(&c, rng, sd); runs.push((c, y)); }
            runs.push((Vec::new(), noisy(&Vec::new(), rng, sd)));
        }
        let an = an_of(&runs, 0, rng).unwrap();
        (an.optima.iter().find(|o| o.0 == Goal::Gaming).map(|o| o.1.clone()).or(last).unwrap_or_default(), runs.len())
    }

    #[test]
    fn sequential_design_beats_one_at_a_time_at_equal_runs() {
        let fs = factors();
        let best = {
            // exhaustive optimum of the truth (2^15 * 3 configurations is too many to matter: coordinate search with restarts)
            let mut rng = Rng::new(1);
            let mut top = f64::MIN;
            for _ in 0..60 {
                let mut c: Cfg = Vec::new();
                for f in &fs { if rng.unit() < 0.4 { c.push((f.key.clone(), f.values[rng.below(f.values.len())].clone())); } }
                loop {
                    let mut imp = false;
                    for f in &fs { for v in std::iter::once(f.reference.clone()).chain(f.values.iter().cloned()) {
                        let mut d: Cfg = c.iter().filter(|(k, _)| *k != f.key).cloned().collect();
                        if v != f.reference { d.push((f.key.clone(), v)); }
                        if net(&d, &fs) > net(&c, &fs) + 1e-12 { c = d; imp = true; }
                    } }
                    if !imp { break; }
                }
                top = top.max(net(&c, &fs));
            }
            top
        };
        let (mut new, mut old, mut used) = (0.0, 0.0, 0);
        let seeds = 4;
        for s in 0..seeds {
            let (c, n) = sequential(&fs, 100, &mut Rng::new(100 + s), 0.05);
            new += net(&c, &fs) / seeds as f64; used += n;
            old += net(&one_at_a_time(&fs, n, &mut Rng::new(200 + s), 0.05), &fs) / seeds as f64;
        }
        eprintln!("true optimum {best:.3} | sequential design {new:.3} | one at a time {old:.3} | runs {}", used / seeds as usize);
        assert!(new > old + 0.01, "design {new:.3} vs one-at-a-time {old:.3}");
        assert!(new > 0.5 * best, "design reaches at least half of the achievable gain: {new:.3} of {best:.3}");
    }

    /// Sensitivity to noise and budget (slow: `cargo test -- --ignored --nocapture sweep`).
    #[test]
    #[ignore]
    fn sweep() {
        let fs = factors();
        for (sd, budget) in [(0.02, 60), (0.05, 60), (0.05, 100), (0.05, 160), (0.10, 100), (0.10, 160)] {
            let (mut new, mut old, mut nothing) = (0.0, 0.0, 0.0);
            let seeds = 3;
            for s in 0..seeds {
                let (c, n) = sequential(&fs, budget, &mut Rng::new(300 + s), sd);
                new += net(&c, &fs) / seeds as f64;
                old += net(&one_at_a_time(&fs, n, &mut Rng::new(400 + s), sd), &fs) / seeds as f64;
                nothing += net(&Vec::new(), &fs) / seeds as f64;
            }
            eprintln!("noise {sd:.2} budget {budget:>3}: design {new:+.3} | one at a time {old:+.3} | nothing {nothing:+.3}");
        }
    }

    #[test]
    fn rows_from_runs_are_signed_clipped_and_robust() {
        let s = |wake: f64, bw: f64, w: f64| -> Sample { [(Metric::WakeP99Us, wake), (Metric::MemBwGbs, bw), (Metric::PkgW, w)].into_iter().collect() };
        let mut runs: Vec<Run> = (0..3).map(|i| Run { cfg: Vec::new(), s: s(100.0, 50.0, 10.0), order: i, t: 100 + i as u64, w: 1.0 }).collect();
        runs.push(Run { cfg: vec![("k".into(), "1".into())], s: s(80.0, 55.0, 9.0), order: 3, t: 110, w: 1.0 });     // all better
        runs.push(Run { cfg: vec![("k".into(), "2".into())], s: s(100.0, 500.0, 10.0), order: 4, t: 120, w: 0.4 });  // absurd outlier on bandwidth, busy machine
        let rows = compute_rows(&runs, Phase::Load, 7, "7.0", 10);
        assert_eq!(rows.len(), 5);
        let ok = &rows[3];
        assert!(ok.y[0] > 0.2 && ok.y[1] > 0.05 && ok.y[2] > 0.05, "lower latency/power and higher bandwidth are +: {:?}", ok.y);
        assert!((rows[4].y[1] - 0.5).abs() < 1e-9, "one wild metric is clipped, not allowed to dominate: {:?}", rows[4].y);
        assert!(rows[0].y[0].abs() < 1e-9 && rows[3].pos > rows[0].pos);
        assert!(rows[4].t == 120 && rows[4].w == 0.4 && rows[4].bv == calib::BENCH_VERSION, "each row keeps its run's time, weight and bench version");
        assert!(compute_rows(&runs[..3], Phase::Load, 7, "", 10).is_empty(), "too few runs: nothing stored");
    }
}
