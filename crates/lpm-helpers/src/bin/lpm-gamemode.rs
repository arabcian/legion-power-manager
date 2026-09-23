//! lpm-gamemode — Lutris / Steam front end for the Optimizations presets.
//! Replaces lutris-game-tune-wrapper without a setuid binary: it runs as the
//! user and reaches root only through `pkexec tune-helper` (same polkit rule
//! as the other helpers). The game itself never runs with elevated rights.
//!
//!   lpm-gamemode PRE  [preset]            apply preset, refcounted game mode
//!   lpm-gamemode POST                     the last POST restores the originals
//!   lpm-gamemode RUN  [preset] [--] cmd…  nice/autogroup boost + CCD affinity, then exec
//!   lpm-gamemode WRAP [preset] [--] cmd…  PRE + RUN + POST around one command (Steam: %command%)
//!   lpm-gamemode APPLY preset             apply as a manual (non-refcounted) change
//!   lpm-gamemode UNDERVOLT                apply the "GAMING" CPU/GPU curve presets now
//!   lpm-gamemode RESTORE                  restore everything now
//!   lpm-gamemode STATUS                   live values and game-mode state
//!
//! Without a preset name, the game preset chosen in the GUI is used
//! (~/.config/legion-power-manager/tune.json).
//!
//! PRE and WRAP also undervolt when tune.json has "undervolt_cpu" /
//! "undervolt_gpu" set (Optimizations → Game launch): the Ryzen Curve
//! Optimizer profile "GAMING" goes first, then — 2 s later — the NVIDIA
//! curve profile "GAMING". A missing profile is skipped with a note.

use lpm_helpers::tune;
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

const HELPER_DIR: &str = match option_env!("LPM_HELPER_DIR") { Some(d) => d, None => "/usr/libexec/legion-power-manager" };
const PKEXEC: &[&str] = &["/usr/bin/pkexec", "/bin/pkexec"];
const MAX_PRESET_BYTES: u64 = 256 * 1024;

/// Exact, case-sensitive profile name looked up in both curve tools.
const UNDERVOLT_PROFILE: &str = "GAMING";
const PKEXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const UNDERVOLT_GAP: std::time::Duration = std::time::Duration::from_secs(2);
/// Same directory nvcurve-root-helper applies from.
const NVCURVE_PROFILES: &str = "/etc/nvcurve/profiles";

fn helper() -> String { format!("{HELPER_DIR}/tune-helper") }

fn xdg_config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
}
fn config_dir() -> PathBuf { xdg_config().join("legion-power-manager") }
/// QStandardPaths::GenericConfigLocation + the Ryzen tab's profile folder.
fn ryzen_profiles_dir() -> PathBuf { xdg_config().join("ryzen-curve-optimizer/profiles") }
fn presets_dir() -> PathBuf { config_dir().join("tune-presets") }

fn valid_name(n: &str) -> bool {
    n.chars().next().map_or(false, |c| c.is_alphanumeric()) && n.len() <= 64
        && n.chars().all(|c| c.is_alphanumeric() || " _-.".contains(c)) && !n.contains("..")
}

fn read_json(p: &Path) -> Option<Value> {
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() || m.len() > MAX_PRESET_BYTES { return None; }
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn default_preset() -> Option<String> {
    read_json(&config_dir().join("tune.json"))?["game_preset"].as_str().filter(|n| valid_name(n)).map(str::to_owned)
}

fn load_preset(name: Option<&str>) -> Result<(String, Value), String> {
    let name = match name {
        Some(n) => n.to_owned(),
        None => default_preset().ok_or("no preset given and no game preset chosen in Legion Power Manager → Optimizations")?,
    };
    if !valid_name(&name) { return Err(format!("invalid preset name '{name}'")); }
    let p = presets_dir().join(format!("{name}.json"));
    let v = read_json(&p).ok_or_else(|| format!("cannot read preset {}", p.display()))?;
    if !v["values"].is_object() { return Err(format!("preset '{name}' has no values")); }
    Ok((name, v))
}

fn preset_exists(name: &str) -> bool { valid_name(name) && presets_dir().join(format!("{name}.json")).is_file() }

fn pkexec(req: &Value) -> Result<Value, String> { pkexec_helper(&helper(), req) }

fn pkexec_helper(helper: &str, req: &Value) -> Result<Value, String> {
    let short = helper.rsplit('/').next().unwrap_or(helper);
    let pk = PKEXEC.iter().find(|p| Path::new(p).is_file()).ok_or("pkexec not found (install sys-auth/polkit)")?;
    let mut child = Command::new(pk).arg(helper)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| format!("pkexec: {e}"))?;
    child.stdin.take().unwrap().write_all(req.to_string().as_bytes()).map_err(|e| format!("pkexec stdin: {e}"))?;
    // Game hooks run without a terminal and often without a polkit agent in
    // reach: never let a stuck authorization block the game launch forever.
    let pid = child.id() as libc::pid_t;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || { let _ = tx.send(child.wait_with_output()); });
    let out = match rx.recv_timeout(PKEXEC_TIMEOUT) {
        Ok(r) => r.map_err(|e| format!("pkexec: {e}"))?,
        Err(_) => {
            // Still waiting for authorization -> pkexec keeps the caller's real uid and can be signalled.
            unsafe { libc::kill(pid, libc::SIGTERM); }
            return Err(format!("{short}: no answer within {} s (authorization pending?) — skipped", PKEXEC_TIMEOUT.as_secs()));
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    match text.lines().last().and_then(|l| serde_json::from_str::<Value>(l).ok()) {
        Some(v) => Ok(v),
        None => Err(match out.status.code() {
            Some(126) => "authorization dismissed".into(),
            Some(127) => format!("polkit did not authorize {short}: {}", String::from_utf8_lossy(&out.stderr).trim()),
            _ => format!("{short} failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
        }),
    }
}

/// Prints helper messages and per-key failures to stderr; returns `ok`.
fn report(tag: &str, v: &Value) -> bool {
    let ok = v["ok"].as_bool().unwrap_or(false);
    if let Some(m) = v["message"].as_str() { eprintln!("lpm-gamemode {tag}: {m}"); }
    for r in v["results"].as_array().into_iter().flatten() {
        if let Some(e) = r["error"].as_str() { eprintln!("  {}: {e}", r["key"].as_str().unwrap_or("?")); }
        for e in r["errors"].as_array().into_iter().flatten() {
            eprintln!("  {}: {}", e["key"].as_str().unwrap_or("?"), e["error"].as_str().unwrap_or(""));
        }
    }
    if !ok { eprintln!("lpm-gamemode {tag}: {}", v["error"].as_str().unwrap_or("failed")); }
    ok
}

fn apply(name: Option<&str>, mode: &str) -> Result<bool, String> {
    let (name, p) = load_preset(name)?;
    let v = pkexec(&json!({"op": "apply", "mode": mode, "preset": name, "values": p["values"]}))?;
    Ok(report(if mode == "game" { "PRE" } else { "APPLY" }, &v))
}

fn post() -> Result<bool, String> { Ok(report("POST", &pkexec(&json!({"op": "release"}))?)) }

/// PRE: game-mode preset first, then the optional undervolt.
fn pre(name: Option<&str>) -> i32 {
    let code = apply(name, "game").map_or_else(|e| { eprintln!("lpm-gamemode: {e}"); 1 }, |ok| (!ok) as i32);
    undervolt(false);
    code
}

// ── undervolt ("GAMING" curve presets) ──────────────────────────────────────

/// Ryzen Curve Optimizer profile → ryzen-co-helper, the same order the GUI
/// uses: all-core offset first, per-core offsets only after it succeeded.
fn apply_ryzen(path: &Path) -> Result<(), String> {
    let p = read_json(path).ok_or_else(|| format!("cannot read {}", path.display()))?;
    let helper = format!("{HELPER_DIR}/ryzen-co-helper");
    let call = |op: &str, params: Value| -> Result<(), String> {
        let v = pkexec_helper(&helper, &json!({"op": op, "params": params}))?;
        for r in v["results"].as_array().into_iter().flatten().filter(|r| r["ok"] != true) {
            eprintln!("  CCD{}/S{}: {}", r["ccd"], r["core"], r["message"].as_str().unwrap_or("failed"));
        }
        if v["ok"] == true { Ok(()) } else {
            Err(v["error"].as_str().or(v["message"].as_str()).unwrap_or("failed").to_owned())
        }
    };
    // Slots on a CCD that is not there (profile from another topology) are skipped, like the GUI does.
    let ccds = tune::ccx_groups().len() as i64;
    let entries: Vec<Value> = p["cores"].as_array().into_iter().flatten()
        .filter(|c| c["disabled"] != true && c["coper"].is_i64() && c["ccx"].as_i64().unwrap_or(0) == 0)
        .filter_map(|c| {
            let ccd = c["ccd"].as_i64()?;
            let core = c["slot"].as_i64().or_else(|| c["core"].as_i64())?;
            (ccds == 0 || ccd < ccds).then(|| json!({"ccd": ccd, "ccx": 0, "core": core, "coper": c["coper"]}))
        })
        .collect();
    let coall = p["coall"].as_i64();
    if coall.is_none() && entries.is_empty() { return Err("profile contains no offsets".into()); }
    if let Some(v) = coall { call("set_coall", json!({"value": v}))?; }
    if !entries.is_empty() { call("set_coper_batch", json!({"entries": entries}))?; }
    Ok(())
}

fn apply_nvidia() -> Result<(), String> {
    let v = pkexec_helper(&format!("{HELPER_DIR}/nvcurve-root-helper"),
        &json!({"op": "apply_named_profile", "name": UNDERVOLT_PROFILE}))?;
    if v["ok"] == true { Ok(()) } else { Err(v["error"].as_str().unwrap_or("failed").to_owned()) }
}

/// Applies the "GAMING" curve presets enabled in tune.json: CPU first, then
/// GPU UNDERVOLT_GAP later. `forced` (the UNDERVOLT verb) ignores the switches.
fn undervolt(forced: bool) -> bool {
    let cfg = read_json(&config_dir().join("tune.json")).unwrap_or(Value::Null);
    let want_cpu = forced || cfg["undervolt_cpu"] == true;
    let want_gpu = forced || cfg["undervolt_gpu"] == true;
    let ryzen = ryzen_profiles_dir().join(format!("{UNDERVOLT_PROFILE}.json"));
    let nvidia = Path::new(NVCURVE_PROFILES).join(format!("{UNDERVOLT_PROFILE}.json"));

    let mut steps: Vec<(&str, Box<dyn Fn() -> Result<(), String>>)> = Vec::new();
    if want_cpu {
        if ryzen.is_file() { let r = ryzen.clone(); steps.push(("CPU", Box::new(move || apply_ryzen(&r)))); }
        else { eprintln!("lpm-gamemode: undervolt CPU skipped — no Ryzen profile \"{UNDERVOLT_PROFILE}\" ({})", ryzen.display()); }
    }
    if want_gpu {
        if nvidia.is_file() { steps.push(("GPU", Box::new(apply_nvidia))); }
        else { eprintln!("lpm-gamemode: undervolt GPU skipped — no NVIDIA profile \"{UNDERVOLT_PROFILE}\" ({})", nvidia.display()); }
    }
    let mut all_ok = true;
    for (i, (what, step)) in steps.iter().enumerate() {
        if i > 0 { std::thread::sleep(UNDERVOLT_GAP); }
        match step() {
            Ok(()) => eprintln!("lpm-gamemode: undervolt {what}: \"{UNDERVOLT_PROFILE}\" applied"),
            Err(e) => { all_ok = false; eprintln!("lpm-gamemode: undervolt {what} failed: {e}"); }
        }
    }
    all_ok && (!forced || !steps.is_empty())
}

// ── launch boost ──────────────────────────────────────────────────────────

fn set_affinity(cpus: &[usize]) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in cpus { if c < libc::CPU_SETSIZE as usize { libc::CPU_SET(c, &mut set); } }
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Applies the preset's `run` block to this process; the game inherits it.
fn prepare_run(preset: &Value) {
    let run = &preset["run"];
    let nice = run["nice"].as_i64().unwrap_or(0);
    if (-20..=-1).contains(&nice) {
        match pkexec(&json!({"op": "boost", "nice": nice, "autogroup": run["autogroup"].as_bool().unwrap_or(true)})) {
            Ok(v) if v["ok"] == true => {}
            Ok(v) => eprintln!("lpm-gamemode: nice boost failed: {}", v["error"].as_str().unwrap_or("?")),
            Err(e) => eprintln!("lpm-gamemode: nice boost failed: {e}"),
        }
    }
    let role = run["affinity"].as_str().unwrap_or("none");
    if role != "none" {
        match tune::resolve_ccd(&tune::ccx_groups(), role) {
            Some(g) => match set_affinity(&g.cpus) {
                Ok(()) => eprintln!("lpm-gamemode: pinned to CCD{} ({})", g.index, tune::fmt_cpu_list(&g.cpus)),
                Err(e) => eprintln!("lpm-gamemode: sched_setaffinity: {e}"),
            },
            None => eprintln!("lpm-gamemode: affinity '{role}' does not apply to this CPU, skipped"),
        }
    }
}

/// [preset] [--] cmd… ; a first word that is not a saved preset is the command.
fn split_cmd(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut i = 0;
    let mut name = None;
    if let Some(a) = args.first() {
        if a != "--" && preset_exists(a) { name = Some(a.clone()); i = 1; }
    }
    if args.get(i).map(String::as_str) == Some("--") { i += 1; }
    (name, args[i..].to_vec())
}

static CHILD: AtomicI32 = AtomicI32::new(0);
extern "C" fn forward(sig: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 { unsafe { libc::kill(pid, sig) }; }
}

fn wrap(args: &[String]) -> i32 {
    let (name, cmd) = split_cmd(args);
    if cmd.is_empty() { eprintln!("lpm-gamemode WRAP: no command"); return 2; }
    let (pname, preset) = match load_preset(name.as_deref()) { Ok(p) => p, Err(e) => { eprintln!("lpm-gamemode: {e}"); return 2 } };
    // The refcount is taken as soon as the helper was reached, even if a knob failed.
    let entered = match apply(Some(&pname), "game") { Ok(_) => true, Err(e) => { eprintln!("lpm-gamemode: {e}"); false } };
    undervolt(false);
    prepare_run(&preset);
    // Forward termination so POST still runs when the launcher stops us.
    // `as *const ()` first: casting a function item straight to an integer type is
    // deprecated (function pointers aren't guaranteed integer-representable), even
    // though it's always fine in practice on the platforms this runs on.
    for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] { unsafe { libc::signal(s, forward as *const () as libc::sighandler_t) }; }
    let code = match Command::new(&cmd[0]).args(&cmd[1..]).spawn() {
        Ok(mut c) => {
            CHILD.store(c.id() as i32, Ordering::SeqCst);
            loop {
                match c.wait() {
                    Ok(st) => break st.code().unwrap_or(1),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break 1,
                }
            }
        }
        Err(e) => { eprintln!("lpm-gamemode: {}: {e}", cmd[0]); 127 }
    };
    if entered { if let Err(e) = post() { eprintln!("lpm-gamemode: {e}"); } }
    code
}

fn run_exec(args: &[String]) -> i32 {
    let (name, cmd) = split_cmd(args);
    if cmd.is_empty() { eprintln!("lpm-gamemode RUN: no command"); return 2; }
    match load_preset(name.as_deref()) {
        Ok((_, p)) => prepare_run(&p),
        Err(e) => eprintln!("lpm-gamemode: {e} — starting without boost"),
    }
    let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
    eprintln!("lpm-gamemode: {}: {err}", cmd[0]);
    127
}

fn status() -> i32 {
    let out = Command::new(helper()).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()
        .and_then(|mut c| { c.stdin.take().unwrap().write_all(br#"{"op":"describe"}"#)?; c.wait_with_output() });
    let Some(v) = out.ok().and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok()) else {
        eprintln!("lpm-gamemode: cannot run {}", helper());
        return 1;
    };
    let st = &v["state"];
    println!("tuning   : {} (source {}, preset {}, {} game(s), {} file(s) saved)",
        if st["active"] == true { "ACTIVE" } else { "off" }, st["source"].as_str().unwrap_or("-"),
        st["preset"].as_str().unwrap_or("-"), st["refcount"], st["saved_files"]);
    println!("default  : {}", default_preset().unwrap_or_else(|| "-".into()));
    if let Some(b) = v["boot"].as_object() { println!("at boot  : {}", b.get("preset").and_then(Value::as_str).unwrap_or("(unnamed)")); }
    for c in v["topology"]["ccds"].as_array().into_iter().flatten() {
        println!("CCD{}     : cpus {}  L3 {} MB  max {} MHz", c["index"], c["cpus"].as_str().unwrap_or(""),
            c["l3_kib"].as_u64().unwrap_or(0) / 1024, c["max_khz"].as_u64().unwrap_or(0) / 1000);
    }
    let mut group = "";
    for t in v["tunables"].as_array().into_iter().flatten() {
        let g = t["group"].as_str().unwrap_or("");
        if g != group { println!("\n[{g}]"); group = g; }
        let cur = t["current"].as_str().unwrap_or(if t["available"] == true { "(root only)" } else { "n/a" });
        println!("  {:<30} {}", t["key"].as_str().unwrap_or(""), cur);
    }
    0
}

fn usage() -> i32 {
    eprintln!("usage: lpm-gamemode PRE [preset] | POST | RUN [preset] [--] cmd… | WRAP [preset] [--] cmd…\n\
               \x20                   | APPLY preset | UNDERVOLT | RESTORE | STATUS");
    2
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fail = |e: String| { eprintln!("lpm-gamemode: {e}"); 1 };
    let code = match args.first().map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("PRE") => pre(args.get(1).map(String::as_str)),
        Some("POST") => post().map_or_else(fail, |ok| (!ok) as i32),
        Some("APPLY") if args.len() == 2 => apply(Some(&args[1]), "manual").map_or_else(fail, |ok| (!ok) as i32),
        Some("RESTORE") => pkexec(&json!({"op": "restore"})).map_or_else(fail, |v| (!report("RESTORE", &v)) as i32),
        Some("UNDERVOLT") => (!undervolt(true)) as i32,
        Some("STATUS") => status(),
        Some("RUN") => run_exec(&args[1..]),
        Some("WRAP") => wrap(&args[1..]),
        _ => usage(),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names() {
        assert!(valid_name("Gaming X3D"));
        assert!(valid_name("low-latency_2"));
        assert!(!valid_name("../x"));
        assert!(!valid_name("/usr/bin/wine"));
        assert!(!valid_name(""));
        assert!(!valid_name("a..b"));
    }
    #[test]
    fn split() {
        let a: Vec<String> = ["--", "wine", "game.exe"].iter().map(|s| s.to_string()).collect();
        assert_eq!(split_cmd(&a), (None, vec!["wine".to_string(), "game.exe".to_string()]));
    }
}
