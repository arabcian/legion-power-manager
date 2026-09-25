//! `nvcurve` CLI — offline (direct HAL) subset of cli.py.
//!
//! Ported: read [--full|--json|--raw], inspect, write, snapshot, gpus,
//! profile {save,apply,list,default}, memlock {status,set,reset}, autoload.
//! Server-backed commands (serve, daemon) arrive with the daemon port (2c);
//! the systemd `service` command is intentionally dropped (OpenRC target).

use nvcurve::config::{Config, PERSISTENT_CONFIG_FILE};
use nvcurve::hal::{gpu, limits, monitoring, snapshot, vfcurve};
use nvcurve::nvapi::*;
use nvcurve::profiles::apply::{apply_profile, run_autoload};
use nvcurve::profiles::native::{list_profiles, load_profile, profile_path, save_profile, ProfileData};
use nvcurve::safety::{check_negative_freq_warnings, validate_write};
use nvcurve::{logging, ops, CurveState, Domain, NvError};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::exit;

const USAGE: &str = "\
usage: nvcurve [--gpu N] <command> [options]

commands:
  read      [--full] [--json] [--raw]           Read the V/F curve
  inspect   [--point N | --range A-B]            Raw ClockBoostTable fields
  write     (--point N | --range A-B | --global) --delta MHz [--dry-run] [--max-delta MHz]
  write     --reset | --reset-all [--dry-run]
  snapshot  save | restore [--file F] | list
  gpus                                           List NVIDIA GPUs
  profile   list | save NAME | apply NAME | default (NAME | --clear)
  memlock   status | set (--max MHz | --to-max) [--min MHz] | reset
  autoload                                       Apply configured auto-load profiles (root)
";

// ── argument handling ───────────────────────────────────────────────────────

struct Args { pos: Vec<String>, opts: BTreeMap<String, Option<String>> }

const VALUED: &[&str] = &["--gpu", "--point", "--range", "--delta", "--max-delta", "--file", "--max", "--min"];

fn die(code: i32, msg: impl AsRef<str>) -> ! {
    eprintln!("{}", msg.as_ref());
    exit(code)
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args { pos: vec![], opts: BTreeMap::new() };
    while let Some(s) = it.next() {
        if s == "-h" || s == "--help" { print!("{USAGE}"); exit(0); }
        if s == "-v" || s == "--version" { println!("nvcurve {}", env!("CARGO_PKG_VERSION")); exit(0); }
        if let Some((k, v)) = s.split_once('=').filter(|_| s.starts_with("--")) {
            a.opts.insert(k.into(), Some(v.into()));
        } else if VALUED.contains(&s.as_str()) {
            let v = it.next().unwrap_or_else(|| die(2, format!("nvcurve: {s} needs a value")));
            a.opts.insert(s, Some(v));
        } else if s.starts_with("--") {
            a.opts.insert(s, None);
        } else {
            a.pos.push(s);
        }
    }
    a
}

impl Args {
    fn flag(&self, k: &str) -> bool { self.opts.contains_key(k) }
    fn val(&self, k: &str) -> Option<&str> { self.opts.get(k).and_then(|v| v.as_deref()) }
    fn num<T: std::str::FromStr>(&self, k: &str) -> Option<T> {
        self.val(k).map(|v| v.parse().unwrap_or_else(|_| die(2, format!("nvcurve: invalid value for {k}: {v}"))))
    }
    fn mhz(&self, k: &str) -> Option<f64> {
        self.num::<f64>(k).map(|f| if f.is_finite() { f } else { die(2, format!("nvcurve: {k} must be finite")) })
    }
    fn range(&self) -> Option<(i64, i64)> {
        let s = self.val("--range")?;
        let (a, b) = s.split_once('-').unwrap_or_else(|| die(2, format!("Expected A-B format, got '{s}'")));
        let (a, b): (i64, i64) = match (a.parse(), b.parse()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => die(2, format!("Non-integer in range: '{s}'")),
        };
        if a > b { die(2, format!("Start > end in range: {a}-{b}")); }
        if a < 0 || b >= CT_POINTS as i64 { die(2, format!("Range {a}-{b} outside 0–{}", CT_POINTS - 1)); }
        Some((a, b))
    }
    fn gpu(&self) -> usize { self.num("--gpu").unwrap_or(0) }
}

/// Re-exec through sudo if not root (like cli.py's require_root).
fn require_root() {
    if unsafe { libc::geteuid() } == 0 { return; }
    let exe = std::env::current_exe().unwrap_or_else(|e| die(1, format!("nvcurve: cannot locate self: {e}")));
    let err = std::os::unix::process::CommandExt::exec(
        std::process::Command::new("sudo").arg("--").arg(exe).args(std::env::args_os().skip(1)));
    die(1, format!("nvcurve: sudo failed: {err}"));
}

fn open_gpu(a: &Args) -> (Gpu, String) {
    gpu::get_gpu(a.gpu()).unwrap_or_else(|e| die(if matches!(e, NvError::GpuIndex(_)) { 2 } else { 1 }, format!("nvcurve: {e}")))
}

fn mhz(khz: i64) -> f64 { khz as f64 / 1000.0 }

// ── read ────────────────────────────────────────────────────────────────────

fn hexdump(d: &[u8], start: usize, len: usize) -> String {
    let end = (start + len).min(d.len());
    (start..end).step_by(16).map(|off| {
        let chunk = &d[off..(off + 16).min(end)];
        let hx: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let asc: String = chunk.iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect();
        format!("  {off:04x}: {:<48}  {asc}", hx.join(" "))
    }).collect::<Vec<_>>().join("\n")
}

fn print_curve(s: &CurveState, voltage: Option<u32>, full: bool) {
    if let Some(v) = voltage { println!("Current voltage: {:.1} mV", v as f64 / 1000.0); }
    let gpu_n = s.points.iter().filter(|p| p.domain == Domain::Gpu).count();
    let mem_n = s.points.len() - gpu_n;
    let mut parts = vec![format!("{gpu_n} GPU core points")];
    if mem_n > 0 { parts.push(format!("{mem_n} memory points")); }
    parts.push(format!("{} total", gpu_n + mem_n));
    println!("Curve: {}\n", parts.join(", "));

    let current = voltage.and_then(|v| s.points.iter().position(|p| p.volt_uv > 0 && (p.volt_uv as i64 - v as i64).abs() < 10_000));
    let mut prev = None;
    let show: Vec<usize> = s.points.iter().enumerate().filter_map(|(i, p)| {
        let keep = full || p.domain == Domain::Memory || prev != Some(p.freq_khz) || i + 1 == s.points.len();
        prev = Some(p.freq_khz);
        keep.then_some(i)
    }).collect();

    println!("{:>3}  {:>8}  {:>8}  {:>8}  Domain", "#", "Freq", "Voltage", "Offset");
    println!("{}", "-".repeat(56));
    for i in show {
        let p = &s.points[i];
        let off = if p.delta_khz != 0 { format!("{:+.0} MHz", p.delta_mhz()) } else { String::new() };
        let dom = if p.domain == Domain::Gpu { "gpu" } else { "memory" };
        let mark = if Some(i) == current { "  <-- current" } else { "" };
        println!("{i:3}  {:>8}  {:>8}  {off:>8}  {dom}{mark}",
                 format!("{:.0} MHz", p.freq_mhz()), format!("{:.0} mV", p.volt_mv()));
    }
    println!();
    for (label, dom) in [("GPU core:", Domain::Gpu), ("Memory:  ", Domain::Memory)] {
        let act: Vec<_> = s.points.iter().filter(|p| p.domain == dom && p.freq_khz > 0).collect();
        if act.is_empty() { continue; }
        let f = act.iter().map(|p| p.freq_khz);
        let v = act.iter().map(|p| p.volt_uv);
        println!("{label} {:.0} – {:.0} MHz, {:.0} – {:.0} mV ({} points)",
                 f.clone().min().unwrap() as f64 / 1000.0, f.max().unwrap() as f64 / 1000.0,
                 v.clone().min().unwrap() as f64 / 1000.0, v.max().unwrap() as f64 / 1000.0, act.len());
    }
    let offs: Vec<i32> = s.points.iter().filter(|p| p.domain == Domain::Gpu && p.delta_khz != 0).map(|p| p.delta_khz).collect();
    if !offs.is_empty() {
        let (lo, hi) = (*offs.iter().min().unwrap(), *offs.iter().max().unwrap());
        if lo == hi {
            println!("GPU offset: {:+.0} MHz (uniform across {} points)", mhz(lo as i64), offs.len());
        } else {
            println!("GPU offsets: {} points active (range: {:+.0} to {:+.0} MHz)", offs.len(), mhz(lo as i64), mhz(hi as i64));
        }
    }
}

fn output_json(name: &str, s: &CurveState, voltage: Option<u32>, total: usize) {
    let idx = |d: Domain| s.points.iter().filter(|p| p.domain == d).map(|p| p.index).collect::<Vec<_>>();
    let mut v = ops::curve_json(name, s, voltage);
    let m = v.as_object_mut().unwrap();
    m.insert("layout".into(), json!({
        "vfp_curve": {"size": VFP_SIZE, "base": VFP_BASE, "stride": VFP_STRIDE, "max_entries": total},
        "clock_table": {"size": CT_SIZE, "base": CT_BASE, "stride": CT_STRIDE, "delta_offset": CT_DELTA_OFF, "max_entries": CT_POINTS},
    }));
    m.insert("curve_info".into(), json!({"gpu_points": idx(Domain::Gpu), "mem_points": idx(Domain::Memory), "total_points": s.points.len()}));
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
}

fn cmd_read(a: &Args) {
    require_root();
    let (g, name) = open_gpu(a);
    if a.flag("--raw") {
        println!("GPU: {name}");
        let mask = vfcurve::get_boost_mask(g).ok();
        if let Ok(vfp) = nvcall(fid::GET_VFP_CURVE, g, VFP_SIZE, 1, |b| {
            if let Some(m) = mask { b.bytes_mut()[4..36].copy_from_slice(&m); }
        }) {
            println!("\n=== VFP Curve (0x21537AD4) — header + first entries ===\n{}", hexdump(vfp.bytes(), 0, 0x48));
            println!("  --- data at 0x48, stride 0x1C ---\n{}", hexdump(vfp.bytes(), 0x48, VFP_STRIDE * 5));
        }
        if let Ok(ct) = vfcurve::read_clock_table_raw(g) {
            println!("\n=== ClockBoostTable (0x23F1B133) — header + first entries ===\n{}", hexdump(ct.bytes(), 0, 0x44));
            println!("  --- data at 0x44, stride 0x24, freqDelta at +0x14 ---\n{}", hexdump(ct.bytes(), 0x44, CT_STRIDE * 5));
        }
        println!();
    }
    let vfp_total = vfcurve::read_vfp_curve(g).map(|v| v.len()).unwrap_or(VFP_POINTS);
    let s = vfcurve::read_curve(g, &name).unwrap_or_else(|e| die(1, format!("Failed to read V/F curve: {e}")));
    let v = monitoring::read_voltage(g).ok();
    if a.flag("--json") { output_json(&name, &s, v, vfp_total); } else { print_curve(&s, v, a.flag("--full")); }
}

fn cmd_inspect(a: &Args) {
    require_root();
    let (g, name) = open_gpu(a);
    let raw = vfcurve::read_clock_table_raw(g).unwrap_or_else(|e| die(1, format!("Failed to read ClockBoostTable: {e}")));
    let s = vfcurve::read_curve(g, &name).ok();
    let (gpu_i, mem_i): (Vec<usize>, Vec<usize>) = s.as_ref().map(|s| {
        (s.points.iter().filter(|p| p.domain == Domain::Gpu).map(|p| p.index).collect(),
         s.points.iter().filter(|p| p.domain == Domain::Memory).map(|p| p.index).collect())
    }).unwrap_or_default();

    let indices: Vec<i64> = if let Some(p) = a.num::<i64>("--point") { vec![p] }
        else if let Some((x, y)) = a.range() { (x..=y).collect() }
        else {
            let mut d = vec![0, 1, 50, 51, 80, 126];
            match (mem_i.iter().min(), mem_i.iter().max()) {
                (Some(&lo), Some(&hi)) => d.extend([lo as i64 - 1, lo as i64, lo as i64 + 1, hi as i64]),
                _ => d.push(127),
            }
            d.sort_unstable(); d.dedup(); d
        };

    println!("GPU: {name}");
    if s.is_some() {
        let mut parts = vec![format!("{} GPU core points", gpu_i.len())];
        if !mem_i.is_empty() { parts.push(format!("{} memory points", mem_i.len())); }
        parts.push(format!("{} total", gpu_i.len() + mem_i.len()));
        println!("Curve: {}", parts.join(", "));
    }
    println!("ClockBoostTable entry detail (stride=0x{CT_STRIDE:02X}, 9 fields × 4 bytes)\n");
    for p in indices {
        let Ok(fields) = vfcurve::read_clock_entry_full(raw.bytes(), p) else { continue };
        let pu = p as usize;
        let dom = if mem_i.contains(&pu) { " [MEMORY]" } else if gpu_i.contains(&pu) { " [GPU]" } else { "" };
        let vfp = s.as_ref().and_then(|s| s.points.get(pu))
            .map(|q| format!("  (VFP: {:.0} MHz @ {:.0} mV)", q.freq_mhz(), q.volt_mv())).unwrap_or_default();
        println!("Point {p:3} — buffer offset 0x{:04X}{dom}{vfp}", CT_BASE + pu * CT_STRIDE);
        for (k, v) in fields.iter().filter(|(k, _)| k != "freqDelta_kHz") {
            if k.contains("0x14") {
                println!("  {k}: {v:12}  (0x{:08X})  = {:+.0} MHz  ← freqDelta", *v as u32, mhz(*v));
            } else {
                println!("  {k}: {v:12}  (0x{v:08X})");
            }
        }
        println!();
    }
}

// ── write ───────────────────────────────────────────────────────────────────

fn cmd_write(a: &Args, cfg: &Config) {
    let (reset, reset_all, dry) = (a.flag("--reset"), a.flag("--reset-all"), a.flag("--dry-run"));
    let delta = a.mhz("--delta");
    if !reset && !reset_all && delta.is_none() {
        die(2, "Error: --delta is required (use --reset/--reset-all to zero all offsets)");
    }
    if reset || reset_all {
        let what = if reset_all { " (gpu + memory domain)" } else { "" };
        if dry { println!("DRY RUN — would reset all offsets to 0{what}."); return; }
        require_root();
        let (g, _) = open_gpu(a);
        let (rc, d) = if reset_all { vfcurve::reset_all_offsets(g, false) } else { vfcurve::reset_offsets(g, false) };
        if rc != 0 { die(1, format!("Write failed ({rc}): {d}")); }
        println!("Reset: all offsets set to 0{what}.");
        return;
    }
    let delta = delta.unwrap();
    let dk = (delta * 1000.0) as i64;
    let max_override = a.mhz("--max-delta").map(|m| (m * 1000.0) as i64);
    let glob = a.flag("--global");
    let mut deltas = BTreeMap::new();
    let target = if let Some(p) = a.num::<i64>("--point") {
        deltas.insert(p, dk);
        format!("Target: point {p}, delta {delta:+.0} MHz ({dk:+} kHz)")
    } else if let Some((x, y)) = a.range() {
        for i in x..=y { deltas.insert(i, dk); }
        format!("Target: points {x}–{y} ({} points), delta {delta:+.0} MHz", deltas.len())
    } else if glob {
        format!("Target: all active points (global), delta {delta:+.0} MHz")
    } else {
        die(2, "Error: specify --point N, --range A-B, --global, or --reset");
    };

    if dry {
        println!("{target}\n\nDRY RUN — would send:");
        if glob { println!("  Target: Global active points"); }
        else {
            let keys: Vec<i64> = deltas.keys().copied().collect();
            let tail = if keys.len() > 5 { format!(" ...and {} more", keys.len() - 5) } else { String::new() };
            println!("  Points: {:?}{tail}", &keys[..keys.len().min(5)]);
        }
        println!("  Delta:  {dk:+} kHz ({delta:+.0} MHz)");
        if let Some(m) = a.mhz("--max-delta") { println!("  Max delta override: {m:+.0} MHz"); }
        return;
    }

    require_root();
    println!("{target}");
    let (g, name) = open_gpu(a);
    let (state, raw) = vfcurve::read_curve_with_raw_ct(g, &name).unwrap_or_else(|e| die(1, format!("Failed to read curve: {e}")));
    if glob {
        deltas = state.points.iter().filter(|p| p.domain == Domain::Gpu).map(|p| (p.index as i64, dk)).collect();
    }
    let errs = validate_write(&deltas, max_override.unwrap_or(cfg.max_delta_khz));
    if !errs.is_empty() {
        for e in errs { eprintln!("Error: {e}"); }
        exit(1);
    }
    let freqs: Vec<i64> = state.points.iter().map(|p| p.freq_khz as i64).collect();
    let cur: Vec<i64> = state.points.iter().map(|p| p.delta_khz as i64).collect();
    let warnings = check_negative_freq_warnings(&deltas, &freqs, Some(&cur));

    if cfg.auto_snapshot {
        snapshot::save(g, &name, &cfg.snapshot_dir, cfg.max_snapshots, Some(raw.bytes()));
    }
    let (rc, d) = vfcurve::write_offsets(g, &deltas, false, false, Some(raw.bytes()));
    if rc != 0 { die(1, format!("Write failed ({rc}): {d}")); }
    println!("Write OK — {} point(s) updated.", deltas.len());
    for w in warnings { println!("WARNING: {w}"); }
}

// ── snapshot / gpus ─────────────────────────────────────────────────────────

fn cmd_snapshot(a: &Args, cfg: &Config) {
    match a.pos.get(1).map(String::as_str) {
        Some("save") => {
            require_root();
            let (g, name) = open_gpu(a);
            match snapshot::save(g, &name, &cfg.snapshot_dir, cfg.max_snapshots, None) {
                Some(p) => println!("Snapshot saved: {}", p.display()),
                None => die(1, "Failed to save snapshot."),
            }
        }
        Some("restore") => {
            require_root();
            let (g, _) = open_gpu(a);
            if let Err(e) = snapshot::restore(g, &cfg.snapshot_dir, a.val("--file")) {
                die(1, format!("Restore failed — {e}"));
            }
            println!("Snapshot restored.");
        }
        Some("list") => {
            let snaps = snapshot::list_snapshots(&cfg.snapshot_dir);
            if snaps.is_empty() { println!("No snapshots found."); return; }
            println!("Snapshots:");
            for s in snaps {
                println!("  {}  {}  non-zero: {}\n    {}", s.timestamp, s.gpu, s.nonzero_offsets, s.filepath);
            }
        }
        _ => die(2, "usage: nvcurve snapshot save|restore [--file F]|list"),
    }
}

fn cmd_gpus() {
    let gpus = gpu::discover_gpus().unwrap_or_else(|e| die(1, format!("nvcurve: {e}")));
    if gpus.is_empty() { println!("No NVIDIA GPUs detected."); return; }
    for g in gpus {
        let pci = g.pci_bus_id.map(|b| format!("PCI 0x{b:04x}")).unwrap_or_else(|| "PCI N/A".into());
        println!("  [{}] {}  —  {}  —  {pci}", g.index, g.name, g.uuid.as_deref().unwrap_or("N/A"));
    }
}

// ── profile ─────────────────────────────────────────────────────────────────

fn cmd_profile(a: &Args, cfg: &Config) {
    let name = a.pos.get(2).map(String::as_str);
    let need = |what: &str| name.unwrap_or_else(|| die(2, format!("Error: profile name required for {what}")));
    match a.pos.get(1).map(String::as_str) {
        Some("list") => {
            let key = ops::gpu_stable_key_offline(a.gpu());
            let default = key.and_then(|k| cfg.auto_load_profiles.get(&k).cloned());
            let profiles = list_profiles(&cfg.profile_dir);
            if profiles.is_empty() { println!("No profiles found."); return; }
            println!("Profiles:");
            for p in profiles {
                let mark = if Some(&p.name) == default.as_ref() { "  [default]" } else { "" };
                println!("  - {} ({} pts){mark}", p.name, p.curve_deltas.len());
            }
        }
        Some("default") => {
            let clear = a.flag("--clear");
            if !clear && name.is_none() { die(2, "Error: profile name required (or use --clear)"); }
            require_root();
            if let Err(e) = ops::set_default_profile(a.gpu(), if clear { None } else { name }) {
                die(1, format!("Error: {e}"));
            }
            if clear { println!("Auto-load profile cleared for GPU {}.", a.gpu()); }
            else { println!("Auto-load profile set to '{}' for GPU {}.", name.unwrap(), a.gpu()); }
        }
        Some("save") => {
            let n = need("save");
            require_root();
            let idx = a.gpu();
            let (g, gname) = open_gpu(a);
            let s = vfcurve::read_curve(g, &gname).unwrap_or_else(|e| die(1, format!("Failed to read curve: {e}")));
            let curve_deltas: Map<String, Value> = s.points.iter().filter(|p| p.delta_khz != 0)
                .map(|p| (p.index.to_string(), json!(p.delta_khz))).collect();
            // NVML can't report an active mem lock; carry it over from an existing profile.
            let existing = profile_path(&cfg.profile_dir, n).filter(|p| p.exists()).and_then(|p| load_profile(&p).ok());
            let data = ProfileData {
                name: n.into(), gpu_name: gname, curve_deltas,
                mem_offset_mhz: limits::get_clock_offsets(idx as u32).mem_offset_mhz.map(i64::from),
                power_limit_w: limits::get_power_limit(idx as u32).power_limit_w.map(i64::from),
                mem_locked_min_mhz: existing.as_ref().and_then(|e| e.mem_locked_min_mhz),
                mem_locked_max_mhz: existing.as_ref().and_then(|e| e.mem_locked_max_mhz),
                gpu_clock_cap_mhz: existing.as_ref().and_then(|e| e.gpu_clock_cap_mhz),
            };
            match save_profile(&cfg.profile_dir, &data) {
                Ok(p) => println!("Saved profile '{n}' to {}", p.display()),
                Err(e) => die(1, format!("Error: {e}")),
            }
        }
        Some("apply") => {
            let n = need("apply");
            require_root();
            let out = apply_profile(a.gpu(), n, cfg).unwrap_or_else(|e| die(1, format!("Profile '{n}': {e}")));
            for w in &out.warnings { eprintln!("  warning: {w}"); }
            if !out.errors.is_empty() {
                for e in &out.errors { eprintln!("  error: {e}"); }
                die(1, format!("Profile '{n}' applied with errors."));
            }
            if out.warnings.is_empty() { println!("Applied profile '{n}'."); }
            else { println!("Applied profile '{n}' with warnings (see stderr)."); }
        }
        _ => die(2, "usage: nvcurve profile list|save NAME|apply NAME|default (NAME|--clear)"),
    }
}

// ── memlock ─────────────────────────────────────────────────────────────────

fn cmd_memlock(a: &Args) {
    let idx = a.gpu() as u32;
    match a.pos.get(1).map(String::as_str) {
        Some("status") => {
            let c = limits::get_supported_mem_clocks(idx);
            if let (Some(lo), Some(hi)) = (c.first(), c.last()) {
                println!("Supported memory clocks: {lo}–{hi} MHz ({} steps)", c.len());
                println!("Max lock target: {hi} MHz — this is the GPU's stock/VBIOS clock table max, NOT the \
                          offset-boosted max. Requesting a lock above it silently snaps down to it.");
            } else {
                println!("Supported memory clocks: unavailable (NVML not initialized or unsupported GPU)");
            }
            println!("Note: NVML does not expose whether a lock is currently active or its range;\n\
                      this only reports what values are legal to lock to.");
        }
        Some("reset") => {
            require_root();
            limits::reset_mem_locked_clocks(idx).unwrap_or_else(|e| die(1, format!("Error: {e}")));
            println!("Memory clock unlocked (returned to driver/P-state control).");
        }
        Some("set") => {
            require_root();
            let max: u32 = if a.flag("--to-max") {
                limits::get_max_mem_clock(idx).unwrap_or_else(|| die(1,
                    "Error: could not determine max supported memory clock (NVML unavailable or unsupported GPU)"))
            } else {
                a.num("--max").unwrap_or_else(|| die(2, "Error: --max <MHz> or --to-max is required"))
            };
            let min: u32 = a.num("--min").unwrap_or(max);
            if min == 0 || min > max { die(2, "Error: need 0 < --min <= --max"); }
            limits::set_mem_locked_clocks(min, max, idx).unwrap_or_else(|e| die(1, format!("Error: {e}")));
            match limits::get_current_mem_clock(idx) {
                Some(act) if act != max => println!("Requested lock: {min}–{max} MHz — driver resolved to {act} MHz \
                    (nearest supported stock clock; see 'nvcurve memlock status' for the legal set)."),
                _ => println!("Memory clock locked to {min}–{max} MHz."),
            }
            println!("Note: a memory offset does not push the clock past this lock's ceiling — it changes the \
                      voltage used to reach it. Set the lock BEFORE the offset (`profile apply` does this).");
        }
        _ => die(2, "usage: nvcurve memlock status|set (--max MHz|--to-max) [--min MHz]|reset"),
    }
}

fn main() {
    let a = parse_args();
    let default_level = if a.pos.first().map(String::as_str) == Some("autoload") { log::Level::Info } else { log::Level::Warn };
    logging::init(logging::level_from_env(default_level), false);
    let cfg = Config::load(Path::new(PERSISTENT_CONFIG_FILE));
    match a.pos.first().map(String::as_str) {
        Some("read") => cmd_read(&a),
        Some("inspect") => cmd_inspect(&a),
        Some("write") => cmd_write(&a, &cfg),
        Some("snapshot") => cmd_snapshot(&a, &cfg),
        Some("gpus") => cmd_gpus(),
        Some("profile") => cmd_profile(&a, &cfg),
        Some("memlock") => cmd_memlock(&a),
        Some("autoload") => exit(run_autoload(&cfg)),
        Some(c @ ("serve" | "daemon" | "verify" | "setup" | "service")) =>
            die(2, format!("nvcurve: '{c}' is not ported yet in the Rust build")),
        Some(c) => die(2, format!("nvcurve: unknown command '{c}'\n\n{USAGE}")),
        None => { print!("{USAGE}"); exit(2); }
    }
}
