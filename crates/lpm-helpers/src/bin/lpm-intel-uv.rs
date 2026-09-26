//! lpm-intel-uv — standalone Intel undervolt tool (root).
//! Same mechanisms as the GUI tab (lpm_helpers::intel_uv).
//!
//!   lpm-intel-uv read
//!   lpm-intel-uv apply [--core MV] [--cache MV] [--gpu MV] [--uncore MV] [--analogio MV] [--force]
//!                      [--iccmax-core A] [--iccmax-gpu A] [--iccmax-cache A]
//!                      [--tjoffset C] [--pl1 W[/S]] [--pl2 W[/S]] [--no-mchbar] [--lock]
//!                      [--bdprochot on|off] [--ctdp 0|1|2]
//!   lpm-intel-uv apply-file PROFILE.json
//!   lpm-intel-uv reset                     voltage planes back to 0 mV
//!   lpm-intel-uv boot                      apply the boot profile for the current power source
//!   lpm-intel-uv daemon                    re-apply loop (AC/battery switch, resume, hwphint)
//!   lpm-intel-uv monitor [SECONDS]         throttle reasons, VCore, RAPL power (Ctrl-C quits)
//!   lpm-intel-uv measure [--csv] [--sleep S]   powercap power, coretemp, per-core MHz
//!   lpm-intel-uv throttlestop FILE.ini [INDEX] [--apply]
//!   lpm-intel-uv turbo on|off              intel_pstate/no_turbo
//!
//! Offsets are mV, ≤ 0 unless --force (max +250). On Skylake and later core
//! and cache share one plane: give both the same value (the smaller wins).

use lpm_helpers::intel_uv::{self, parse_profile};
use lpm_helpers::intel_uv_daemon;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// fn item → fn pointer → integer: the two-step cast rustc's
/// function_casts_as_integer lint asks for (same value, no behaviour change).
fn handler(f: extern "C" fn(libc::c_int)) -> libc::sighandler_t { f as *const () as libc::sighandler_t }

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn on_int(_: libc::c_int) { STOP.store(true, Ordering::SeqCst); }

fn usage() -> ! {
    eprintln!("{}", include_str!("lpm-intel-uv.rs").lines().skip(3).take_while(|l| l.starts_with("//!"))
        .map(|l| l.trim_start_matches("//!").strip_prefix(' ').unwrap_or("")).collect::<Vec<_>>().join("\n"));
    std::process::exit(2)
}

fn f(s: Option<&String>) -> f64 { s.and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()) }

fn print_status(v: &Value) {
    if let Some(c) = v.get("cpu").filter(|c| !c.is_null()) {
        println!("CPU: {} ({}; family {} model {} stepping {})", c["name"].as_str().unwrap_or("?"),
                 c["codename"].as_str().unwrap_or("?"), c["family"], c["model"], c["stepping"]);
    }
    if let Some(l) = v["lockdown"].as_str() { println!("Kernel lockdown: {l}"); }
    if let Some(e) = v["error"].as_str() { println!("Error: {e}"); return; }
    for &(k, _, label) in intel_uv::PLANES {
        let p = &v["voltage"][k];
        match p["mv"].as_f64() { Some(mv) => println!("{label:<15} {mv:>8.2} mV"),
                                 None => println!("{label:<15} {}", p["error"].as_str().unwrap_or("?")) }
    }
    for &k in intel_uv::ICC_PLANES {
        let p = &v["iccmax"][k];
        match p["amps"].as_f64() { Some(a) => println!("IccMax {k:<8} {a:>8.2} A"),
                                   None => println!("IccMax {k:<8} {}", p["error"].as_str().unwrap_or("?")) }
    }
    let t = &v["temp"];
    if t["tjmax"].is_number() {
        println!("Temperature target: TjMax {} °C - offset {} °C = {} °C (programmable: {})", t["tjmax"], t["offset"], t["target"], t["programmable"]);
    }
    let p = &v["power"];
    for k in ["pl1", "pl2"] {
        if p[k].is_object() {
            println!("{} {:>6.1} W  {:>9.4} s  {}{}", k.to_uppercase(), p[k]["watts"].as_f64().unwrap_or(0.0),
                     p[k]["seconds"].as_f64().unwrap_or(0.0),
                     if p[k]["enabled"] == true { "enabled" } else { "disabled" }, if p[k]["clamp"] == true { ", clamp" } else { "" });
        }
    }
    if p["locked"] == true { println!("Warning: package power limit is locked"); }
    if let Some(m) = p.get("mchbar") {
        if let Some(e) = m["error"].as_str() { println!("MCHBAR: {e}"); }
        else { println!("MCHBAR {}: PL1 {:.1} W, PL2 {:.1} W{}", m["base"].as_str().unwrap_or("?"),
            m["pl1"]["watts"].as_f64().unwrap_or(0.0), m["pl2"]["watts"].as_f64().unwrap_or(0.0),
            if m["matches_msr"] == true { " (matches MSR)" } else { " (differs from MSR)" }); }
    }
    if let Some(b) = v["bdprochot"]["enabled"].as_bool() { println!("BD PROCHOT: {}", if b { "enabled" } else { "disabled" }); }
    let c = &v["ctdp"];
    if c["programmable"].is_boolean() {
        println!("cTDP: level {} of {} extra (programmable: {}{})", c["current"], c["levels"], c["programmable"],
                 if c["locked"] == true { ", locked" } else { "" });
    }
    if let Ok(t) = std::fs::read_to_string("/sys/devices/system/cpu/intel_pstate/no_turbo") {
        println!("Turbo: {}", if t.trim() == "1" { "disabled" } else { "enabled" });
    }
}

fn print_apply(v: &Value) -> i32 {
    if let Some(e) = v["error"].as_str() { eprintln!("Error: {e}"); return 1; }
    if let Some(m) = v["message"].as_str() { println!("{m}"); }
    for r in v["results"].as_array().into_iter().flatten() {
        println!("{} {:<16} {}", if r["ok"] == true { "OK " } else { "ERR" }, r["what"].as_str().unwrap_or(""), r["message"].as_str().unwrap_or(""));
    }
    if v["ok"] == true { 0 } else { 1 }
}

/// throttled --monitor, terminal form.
fn monitor(secs: f64) -> i32 {
    let msr = match intel_uv::Msr::open(true) { Ok(m) => m, Err(e) => { eprintln!("/dev/cpu/0/msr: {e}"); return 1; } };
    let mut prev: Option<Value> = None;
    while !STOP.load(Ordering::SeqCst) {
        let s = intel_uv::monitor_sample(&msr, false);
        let t = &s["throttle"];
        let lim = |k: &str| if t[k] == true { "\x1b[93mLIM\x1b[0m" } else { "\x1b[92mOK\x1b[0m" };
        let mut line = format!("Thermal: {} - Power: {} - Current: {} - Cross-domain: {}  ||  VCore: {} mV",
            lim("thermal"), lim("power"), lim("current"), lim("cross_domain"), s["vcore_mv"]);
        if let Some(p) = &prev {
            let dt = s["t"].as_f64().unwrap_or(0.0) - p["t"].as_f64().unwrap_or(0.0);
            let wrap = s["energy"]["wrap"].as_u64().unwrap_or(1 << 32);
            for d in ["package", "core", "graphics", "dram"] {
                let (Some(a), Some(b)) = (s["energy"]["raw"][d].as_u64(), p["energy"]["raw"][d].as_u64()) else { continue };
                let unit = if d == "dram" { s["energy"]["dram_unit_j"].as_f64() } else { s["energy"]["unit_j"].as_f64() }.unwrap_or(0.0);
                if dt > 0.0 { line += &format!(" - {d}: {:.1} W", ((a + wrap - b) % wrap) as f64 * unit / dt); }
            }
        }
        print!("\r{line}\x1b[K");
        let _ = std::io::stdout().flush();
        prev = Some(s);
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < end && !STOP.load(Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(50)); }
    }
    println!();
    0
}

/// intel-undervolt `measure`: powercap power per zone, coretemp, per-core MHz.
fn measure(csv: bool, secs: f64) -> i32 {
    let hw = std::fs::read_dir("/sys/class/hwmon").into_iter().flatten().flatten().map(|e| e.path())
        .find(|p| std::fs::read_to_string(p.join("name")).map(|n| n.trim() == "coretemp").unwrap_or(false));
    let zones: Vec<_> = std::fs::read_dir("/sys/class/powercap").into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("intel-rapl:")).unwrap_or(false)).collect();
    let read = |p: &std::path::Path| std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned());
    let mut prev: Vec<(u64, Instant)> = zones.iter().map(|z| (read(&z.join("energy_uj")).and_then(|s| s.parse().ok()).unwrap_or(0), Instant::now())).collect();
    let mut first = true;
    if !csv { print!("\x1b[?25l"); }
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs_f64(secs));
        let mut rows: Vec<(String, String)> = Vec::new();
        for (i, z) in zones.iter().enumerate() {
            let now = Instant::now();
            let e: u64 = read(&z.join("energy_uj")).and_then(|s| s.parse().ok()).unwrap_or(0);
            let range: u64 = read(&z.join("max_energy_range_uj")).and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
            let de = if e >= prev[i].0 { e - prev[i].0 } else { e + range.saturating_sub(prev[i].0) };
            let w = de as f64 / 1e6 / now.duration_since(prev[i].1).as_secs_f64();
            prev[i] = (e, now);
            rows.push((read(&z.join("name")).unwrap_or_default(), format!("{w:.3} W")));
        }
        if let Some(h) = &hw {
            for i in 1..=128 {
                let Some(t) = read(&h.join(format!("temp{i}_input"))) else { continue };
                let label = read(&h.join(format!("temp{i}_label"))).unwrap_or(format!("temp{i}"));
                rows.push((label, format!("{:.1} °C", t.parse::<f64>().unwrap_or(0.0) / 1000.0)));
            }
        }
        for c in 0..4096 {
            let Some(k) = read(std::path::Path::new(&format!("/sys/bus/cpu/devices/cpu{c}/cpufreq/scaling_cur_freq"))) else {
                if !std::path::Path::new(&format!("/sys/bus/cpu/devices/cpu{c}")).exists() { break; } continue };
            rows.push((format!("Core {c}"), format!("{:.0} MHz", k.parse::<f64>().unwrap_or(0.0) / 1000.0)));
        }
        if csv {
            if first { println!("{}", rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>().join(",")); }
            println!("{}", rows.iter().map(|r| r.1.split(' ').next().unwrap_or("").to_owned()).collect::<Vec<_>>().join(","));
        } else {
            print!("\x1b[H\x1b[J");
            for (k, v) in &rows { println!("{k:<24}{v:>14}"); }
        }
        let _ = std::io::stdout().flush();
        first = false;
    }
    if !csv { print!("\x1b[?25h"); }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { usage() };
    if let Err(e) = lpm_helpers::machine::check() { eprintln!("lpm-intel-uv: {e}"); std::process::exit(1); }
    unsafe { libc::signal(libc::SIGINT, handler(on_int)); libc::signal(libc::SIGTERM, handler(on_int)); }
    if cmd != "measure" && unsafe { libc::geteuid() } != 0 { eprintln!("lpm-intel-uv: needs root (MSR access)"); std::process::exit(1); }
    let code = match cmd.as_str() {
        "read" => { let v = intel_uv::read_status(); print_status(&v); if v["ok"] == true { 0 } else { 1 } }
        "reset" => print_apply(&intel_uv::apply(&intel_uv::reset_profile())),
        // Read-only: Arrow Lake D2D/NGU + classic mailbox read of domains 0..15.
        "probe-fabric" => { let v = intel_uv::probe_fabric(); println!("{}", serde_json::to_string_pretty(&v).unwrap()); if v["ok"] == true { 0 } else { 1 } }
        "boot" => print_apply(&intel_uv_daemon::apply_boot()),
        "daemon" => intel_uv_daemon::run_daemon(),
        "monitor" => monitor(args.get(1).map(|s| s.parse().unwrap_or_else(|_| usage())).unwrap_or(1.0f64).max(0.1)),
        "measure" => {
            let csv = args.iter().any(|a| a == "--csv");
            let secs = args.iter().position(|a| a == "--sleep").map(|i| f(args.get(i + 1))).unwrap_or(1.0);
            if secs <= 0.0 { usage() }
            measure(csv, secs)
        }
        "turbo" => {
            let v = match args.get(1).map(String::as_str) { Some("on") => "0", Some("off") => "1", _ => usage() };
            match lpm_helpers::sysfs_write(std::path::Path::new("/sys/devices/system/cpu/intel_pstate/no_turbo"), v.as_bytes()) {
                Ok(()) => { println!("turbo {}", args[1]); 0 }
                Err(e) => { eprintln!("intel_pstate/no_turbo: {e}"); 1 }
            }
        }
        "throttlestop" => {
            let path = args.get(1).unwrap_or_else(|| usage());
            let idx: u32 = args.get(2).filter(|a| !a.starts_with("--")).map(|s| s.parse().unwrap_or_else(|_| usage())).unwrap_or(0);
            match std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).map_err(|e| e.to_string())
                .and_then(|s| intel_uv::parse_throttlestop(&s, idx)) {
                Err(e) => { eprintln!("{path}: {e}"); 1 }
                Ok(v) => {
                    let mut volt = Map::new();
                    let mut cmdline = String::from("lpm-intel-uv apply");
                    for (k, mv) in &v { volt.insert((*k).into(), json!(mv)); cmdline += &format!(" --{k} {mv}"); }
                    println!("{cmdline}");
                    if args.iter().any(|a| a == "--apply") {
                        match parse_profile(&json!({"voltage": volt})) { Ok(p) => print_apply(&intel_uv::apply(&p)),
                            Err(e) => { eprintln!("invalid: {e}"); 1 } }
                    } else { 0 }
                }
            }
        }
        "apply-file" => {
            let path = args.get(1).unwrap_or_else(|| usage());
            match std::fs::read_to_string(path).map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string()))
                .and_then(|v| parse_profile(&v)) {
                Ok(p) => print_apply(&intel_uv::apply(&p)),
                Err(e) => { eprintln!("{path}: {e}"); 1 }
            }
        }
        "apply" => {
            let (mut volt, mut icc, mut power) = (Map::new(), Map::new(), Map::new());
            let mut prof = Map::new();
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                let key = a.trim_start_matches("--");
                match key {
                    "core" | "cache" | "gpu" | "uncore" | "analogio" => { volt.insert(key.into(), json!(f(it.next()))); }
                    k if k.starts_with("iccmax-") => { icc.insert(k[7..].into(), json!(f(it.next()))); }
                    "tjoffset" => { prof.insert("tjoffset".into(), json!(f(it.next()))); }
                    "ctdp" => { prof.insert("ctdp".into(), json!(f(it.next()))); }
                    "force" => { prof.insert("allow_positive".into(), json!(true)); }
                    "lock" => { prof.insert("lock_power".into(), json!(true)); }
                    "bdprochot" => {
                        let v = match it.next().map(String::as_str) { Some("off") => true, Some("on") => false, _ => usage() };
                        prof.insert("disable_bdprochot".into(), json!(v));
                    }
                    "pl1" | "pl2" => {
                        let s = it.next().unwrap_or_else(|| usage());
                        let (w, t) = s.split_once('/').map(|(w, t)| (w, Some(t))).unwrap_or((s.as_str(), None));
                        let w: f64 = w.parse().unwrap_or_else(|_| usage());
                        let t: Option<f64> = t.map(|t| t.parse().unwrap_or_else(|_| usage()));
                        power.insert(key.into(), json!({"watts": w, "seconds": t}));
                    }
                    "no-mchbar" => { power.insert("mchbar".into(), json!(false)); }
                    _ => usage(),
                }
            }
            if let (Some(c), Some(k)) = (volt.get("core").and_then(Value::as_f64), volt.get("cache").and_then(Value::as_f64)) {
                if c != k { eprintln!("warning: core and cache share one plane on Skylake+; the smaller offset applies to both"); }
            } else if volt.contains_key("core") != volt.contains_key("cache") {
                eprintln!("warning: only one of core/cache given; on Skylake+ they share a plane, set both");
            }
            prof.insert("voltage".into(), Value::Object(volt));
            prof.insert("iccmax".into(), Value::Object(icc));
            prof.insert("power".into(), Value::Object(power));
            match parse_profile(&Value::Object(prof)) {
                Ok(p) => print_apply(&intel_uv::apply(&p)),
                Err(e) => { eprintln!("invalid: {e}"); 1 }
            }
        }
        _ => usage(),
    };
    std::process::exit(code)
}
