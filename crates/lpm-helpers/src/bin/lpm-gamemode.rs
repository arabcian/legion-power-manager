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
//!   lpm-gamemode SCENE name               apply a saved scene now (all components)
//!   lpm-gamemode RESTORE                  restore everything now
//!   lpm-gamemode STATUS                   live values and game-mode state
//!
//! Without a preset name, the game preset chosen in the GUI is used
//! (~/.config/legion-power-manager/tune.json).
//!
//! Game launch settings (Optimizations → Game launch, tune.json):
//!   "game_scene"    a saved scene the first PRE / WRAP switches to; the last
//!                   POST returns to the scene active before the game (or to
//!                   the AC / battery scene when automatic switching is on).
//!                   The scene's Optimizations part is skipped: the game preset
//!                   owns tuning while a game runs.
//!   "undervolt_cpu" / "undervolt_gpu"
//!                   with a game scene: whether its CPU / GPU curve is applied;
//!                   without one: apply the "GAMING" curve profiles (CPU first,
//!                   GPU 2 s later; a missing profile is skipped with a note).

/// stderr logging that never panics: eprintln! aborts the process on EPIPE
/// (a launcher that stopped reading), which could cut POST off before it had
/// restored anything.
macro_rules! log {
    ($($t:tt)*) => {{ use std::io::Write as _; let _ = writeln!(std::io::stderr(), $($t)*); }};
}

use lpm_helpers::{lighting, tune};
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
/// RUN: how long to look for a PRE that has not taken the start lock yet.
const START_DETECT: std::time::Duration = std::time::Duration::from_secs(3);
/// RUN: upper bound for PRE's start sequence (scene, undervolt, preset).
const START_WAIT: std::time::Duration = std::time::Duration::from_secs(90);
const PKEXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const UNDERVOLT_GAP: std::time::Duration = std::time::Duration::from_secs(2);
/// Same directory nvcurve-root-helper applies from.
const NVCURVE_PROFILES: &str = "/etc/nvcurve/profiles";

/// Everything this tool asks of root goes to tune-profile-helper (approved presets by name, restore, game
/// bookkeeping); only APPROVE talks to tune-helper.
fn helper() -> String { format!("{HELPER_DIR}/tune-profile-helper") }

const STORE_DIR: &str = "/etc/legion-power-manager/presets";

/// The approved (root-owned) copy of a preset's values. tune-profile-helper applies from it by name.
fn store_values(name: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(format!("{STORE_DIR}/{name}.json")).ok()?).ok()?;
    v["values"].is_object().then(|| v["values"].clone())
}

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
    let mut v = read_json(&p).ok_or_else(|| format!("cannot read preset {}", p.display()))?;
    if !v.is_object() { return Err(format!("preset '{name}' is not a JSON object")); }
    // Only the root-owned copy counts as the preset's values; the user's file may say anything.
    v["values"] = store_values(&name).ok_or_else(|| format!(
        "preset '{name}' is not approved — run: lpm-gamemode APPROVE '{name}' (or save it in the Optimizations tab)"))?;
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
    if let Some(m) = v["message"].as_str() { log!("lpm-gamemode {tag}: {m}"); }
    for r in v["results"].as_array().into_iter().flatten() {
        if let Some(e) = r["error"].as_str() { log!("  {}: {e}", r["key"].as_str().unwrap_or("?")); }
        for e in r["errors"].as_array().into_iter().flatten() {
            log!("  {}: {}", e["key"].as_str().unwrap_or("?"), e["error"].as_str().unwrap_or(""));
        }
    }
    if !ok { log!("lpm-gamemode {tag}: {}", v["error"].as_str().unwrap_or("failed")); }
    ok
}

fn apply(name: Option<&str>, mode: &str) -> Result<bool, String> {
    // The request names the preset; tune-profile-helper takes the values from the approved store.
    let (name, _) = load_preset(name)?;
    let mut req = json!({"op": "apply_preset", "mode": mode, "preset": name, "soft_park": mode == "game"});
    if mode == "game" { req["owner_pid"] = json!(OWNER.load(Ordering::SeqCst)); }
    let v = pkexec(&req)?;
    if let Some(r) = v["parked"].as_str() {
        log!("lpm-gamemode PRE: CCD '{r}' parked for the game without hot-unplug (game kept off it, IRQs and kernel work moved onto it)");
    }
    Ok(report(if mode == "game" { "PRE" } else { "APPLY" }, &v))
}

/// POST: release game mode; when that was the last game, leave the game scene.
fn post() -> Result<bool, String> {
    let v = pkexec(&json!({"op": "release", "owner_pid": OWNER.load(Ordering::SeqCst)}))?;
    let ok = report("POST", &v);
    if v["restored"] == true { leave_game_scene(); }
    Ok(ok)
}

/// First PRE / WRAP of a session: the game scene (or the legacy "GAMING"
/// undervolt), then the game-mode preset. Returns whether the helper took the
/// game-mode reference (so POST must run) and the exit code.
fn game_start(name: Option<&str>) -> (bool, i32) {
    // Two games launched at the same moment must not both see "first game"
    // and both switch scenes: serialise the start sequence per user.
    let _start_lock = user_lock("gamemode-start.lock");
    let cfg = read_json(&config_dir().join("tune.json")).unwrap_or(Value::Null);
    let first = game_refcount() == 0;
    let paused = scenes_paused();
    if paused && cfg["game_scene"].as_str().is_some() { log!("lpm-gamemode: scenes are paused — game scene skipped"); }
    match cfg["game_scene"].as_str().filter(|n| valid_name(n) && !paused) {
        Some(scene) if first => enter_game_scene(scene, cfg["undervolt_cpu"] == true, cfg["undervolt_gpu"] == true),
        Some(_) => log!("lpm-gamemode: another game is running — its scene stays"),
        None if first => { undervolt(false); }
        None => {}
    }
    match apply(name, "game") {
        Ok(ok) => (true, (!ok) as i32),
        Err(e) => { log!("lpm-gamemode: {e}"); (false, 1) }
    }
}

fn pre(name: Option<&str>) -> i32 { game_start(name).1 }

// ── scenes ────────────────────────────────────────────────────────────────
// Same files and semantics as the GUI's Scenes tab (scenes.cpp). The shared
// runtime state lets the GUI and this tool agree on the active scene.

fn scenes_dir() -> PathBuf { config_dir().join("scenes") }

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
        .join("legion-power-manager")
}
fn scene_state_file() -> PathBuf { runtime_dir().join("scene.json") }
fn read_scene_state() -> Value { read_json(&scene_state_file()).filter(Value::is_object).unwrap_or_else(|| json!({})) }
fn write_scene_state(v: &Value) {
    let dir = runtime_dir();
    if std::fs::create_dir_all(&dir).is_err() { return; }
    let tmp = dir.join("scene.json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() { let _ = std::fs::rename(&tmp, scene_state_file()); }
}

/// Running game sessions, from tune-helper's world-readable state (sessions
/// whose launcher has died are not counted).
fn game_refcount() -> i64 {
    read_json(Path::new("/run/legion-power-manager/tune/state.json")).map_or(0, |v| lpm_helpers::live_game_sessions(&v))
}

/// Exclusive per-user lock in $XDG_RUNTIME_DIR, held until the file drops.
fn user_lock(name: &str) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let f = std::fs::OpenOptions::new().create(true).write(true).open(dir.join(name)).ok()?;
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 { return Some(f); }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted { return None; }
    }
}

// ── game-session owner ──────────────────────────────────────────────────────
//
// tune-helper ends a game session on its own when the owner process is gone,
// so a launcher that was killed (or a POST hook that never ran) no longer
// leaves game mode stuck until reboot. WRAP owns its session itself; PRE/POST
// are short-lived hooks, so the owner is the launcher above them (the first
// ancestor that is not a shell or a small exec wrapper).

static OWNER: AtomicI32 = AtomicI32::new(0);

const WRAPPER_COMMS: &[&str] = &["sh", "bash", "dash", "zsh", "fish", "ksh", "mksh", "busybox", "env", "timeout",
                                 "nice", "ionice", "stdbuf", "xargs", "setsid", "flock", "lpm-gamemode"];

/// (comm, ppid) of `pid` from /proc/<pid>/stat.
fn proc_comm_ppid(pid: i32) -> Option<(String, i32)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (l, r) = (s.find('(')?, s.rfind(')')?);
    let ppid = s[r + 1..].split_whitespace().nth(1)?.parse().ok()?;
    Some((s[l + 1..r].to_owned(), ppid))
}

fn launcher_pid() -> i32 {
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..16 {
        if pid <= 1 { return 0; }
        let Some((comm, ppid)) = proc_comm_ppid(pid) else { return 0 };
        if comm == "systemd" || comm == "init" { return 0; }  // no launcher above us: untracked
        if !WRAPPER_COMMS.contains(&comm.as_str()) { return pid; }
        pid = ppid;
    }
    0
}

fn enter_game_scene(scene: &str, cpu: bool, gpu: bool) {
    let mut st = read_scene_state();
    st["before_game"] = st.get("active").cloned().unwrap_or(Value::Null);
    st["game_scene"] = json!(scene);
    write_scene_state(&st);
    apply_scene(scene, SceneParts { cpu, gpu, tuning: false });  // parts that failed were reported
    let mut st = read_scene_state();
    st["active"] = json!(scene);
    write_scene_state(&st);
}

fn scenes_paused() -> bool {
    read_json(&config_dir().join("scenes.json")).map_or(false, |v| v["paused"] == true)
}

fn leave_game_scene() {
    let mut st = read_scene_state();
    if st.get("game_scene").map_or(true, Value::is_null) { return; }
    if scenes_paused() {
        st["game_scene"] = Value::Null;
        st["before_game"] = Value::Null;
        write_scene_state(&st);
        log!("lpm-gamemode: last game closed — scenes are paused, no scene change");
        return;
    }
    let auto = read_json(&config_dir().join("scenes.json")).unwrap_or(Value::Null);
    let target = if auto["auto"] == true {
        let key = if on_ac().unwrap_or(true) { "on_ac" } else { "on_battery" };
        auto[key].as_str().filter(|n| valid_name(n)).map(str::to_owned)
    } else {
        st["before_game"].as_str().map(str::to_owned)
    };
    st["game_scene"] = Value::Null;
    st["before_game"] = Value::Null;
    write_scene_state(&st);
    match target {
        Some(t) => {
            log!("lpm-gamemode: last game closed — back to scene \"{t}\"");
            apply_scene(&t, SceneParts { cpu: true, gpu: true, tuning: true });
            let mut st = read_scene_state();
            st["active"] = json!(t);
            write_scene_state(&st);
        }
        None => log!("lpm-gamemode: last game closed — no earlier scene to return to (the game scene stays)"),
    }
}

/// Mains or USB-PD online → AC; chargers present but offline → battery;
/// no chargers reported → go by the battery status. Same rule as the GUI.
fn on_ac() -> Option<bool> {
    let (mut any_supply, mut any_bat, mut discharging) = (false, false, false);
    for e in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let rd = |f: &str| std::fs::read_to_string(e.path().join(f)).ok().map(|s| s.trim().to_owned());
        match rd("type").as_deref() {
            Some("Mains") | Some("USB") => { any_supply = true; if rd("online").as_deref() == Some("1") { return Some(true); } }
            Some("Battery") => { any_bat = true; discharging |= rd("status").as_deref() == Some("Discharging"); }
            _ => {}
        }
    }
    if any_supply { Some(false) } else if any_bat { Some(!discharging) } else { None }
}

#[derive(Clone, Copy)]
struct SceneParts { cpu: bool, gpu: bool, tuning: bool }

// Firmware attributes the kernel rejects through sysfs, written by the GPU
// helper over WMI instead (same table as fwattrtab.cpp).
const WMI_KNOBS: &[(&str, &str)] = &[("gpu_nv_ctgp", "ctgp"), ("gpu_nv_ppab", "boost_up"), ("gpu_nv_cpu_boost", "boost_down")];

fn platform_profile_node() -> Option<String> {
    let mut v: Vec<String> = std::fs::read_dir("/sys/class/platform-profile").ok()?.flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v.into_iter().next()
}

fn current_platform_profile() -> Option<String> {
    let p = match platform_profile_node() {
        Some(n) => PathBuf::from(format!("/sys/class/platform-profile/{n}/profile")),
        None => PathBuf::from("/sys/firmware/acpi/platform_profile"),
    };
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

fn scene_choice(v: &Value) -> Option<Result<String, ()>> {
    if v["reset"] == true { return Some(Err(())); }
    v["profile"].as_str().filter(|n| valid_name(n)).map(|n| Ok(n.to_owned()))
}

/// Applies a saved scene in dependency order, printing one line per part.
/// A failing part is reported and the rest still runs. Returns overall ok.
fn apply_scene(name: &str, parts: SceneParts) -> bool {
    if !valid_name(name) { log!("lpm-gamemode: invalid scene name '{name}'"); return false; }
    let Some(s) = read_json(&scenes_dir().join(format!("{name}.json"))) else {
        log!("lpm-gamemode: scene \"{name}\" not found ({})", scenes_dir().display());
        return false;
    };
    let mut ok = true;
    let mut line = |what: &str, r: Result<String, String>| match r {
        Ok(m) => log!("lpm-gamemode: scene {name}: {what}: {m}"),
        Err(e) => { ok = false; log!("lpm-gamemode: scene {name}: ✗ {what}: {e}"); }
    };

    // 0. EC fan boost (Full Speed flag) FIRST: switching it off before the profile change means
    //    the fans never spin up for the new profile and then drop again.
    let fan_want = s["fan_fullspeed"].as_bool();
    if let Some(on) = fan_want {
        let v = pkexec_helper(&format!("{HELPER_DIR}/legion-profile-helper"),
                              &json!({"device": "fan_fullspeed", "value": if on { "1" } else { "0" }}))
            .unwrap_or_else(|e| json!({"ok": false, "error": e}));
        line("Fan boost", if v["ok"] == true { Ok(if on { "turbo".into() } else { "auto".into() }) }
                          else { Err(v["error"].as_str().unwrap_or("failed").to_owned()) });
    }

    // 1. power profile (firmware limits below need Custom)
    if let Some(p) = s["platform_profile"].as_str().filter(|p| !p.is_empty() && p.len() <= 32) {
        let r = pkexec_helper(&format!("{HELPER_DIR}/legion-profile-helper"),
                              &json!({"profile": p, "handler": platform_profile_node()}))
            .and_then(|v| if v["ok"] == true { Ok(v["effective"].as_str().unwrap_or(p).to_owned()) }
                          else { Err(v["error"].as_str().unwrap_or("failed").to_owned()) });
        line("power profile", r);
    }

    // The EC may reset the flag when the profile changes: re-assert only if it no longer matches.
    if let (Some(on), Some(_)) = (fan_want, s["platform_profile"].as_str().filter(|p| !p.is_empty())) {
        let h = format!("{HELPER_DIR}/legion-profile-helper");
        if let Ok(v) = pkexec_helper(&h, &json!({"fan_fullspeed": "get"})) {
            if v["ok"] == true && v["on"].as_bool() != Some(on) {
                let r = pkexec_helper(&h, &json!({"device": "fan_fullspeed", "value": if on { "1" } else { "0" }}))
                    .and_then(|v| if v["ok"] == true { Ok("re-applied after the profile change".to_owned()) } else { Err("failed".to_owned()) });
                line("Fan boost", r);
            }
        }
    }

    // 2. firmware limits
    if let Some(fw) = s["firmware"].as_object().filter(|m| !m.is_empty()) {
        let r = (|| -> Result<String, String> {
            if current_platform_profile().as_deref() != Some("custom") {
                return Err("needs the Custom power profile (set it in this scene)".into());
            }
            let (mut batch, mut wmi, mut unknown) = (Vec::new(), serde_json::Map::new(), 0);
            for (attr, val) in fw {
                let Some(v) = val.as_i64() else { unknown += 1; continue };
                if let Some((_, key)) = WMI_KNOBS.iter().find(|(a, _)| a == attr) { wmi.insert((*key).into(), json!(v)); continue; }
                let dir = std::fs::read_dir("/sys/class/firmware-attributes").into_iter().flatten().flatten()
                    .map(|d| d.path().join("attributes").join(attr)).find(|d| d.join("current_value").is_file());
                let Some(dir) = dir else { unknown += 1; continue };
                let num = |f: &str| std::fs::read_to_string(dir.join(f)).ok().and_then(|x| x.trim().parse::<i64>().ok());
                if num("max_value").unwrap_or(0) <= num("min_value").unwrap_or(0) { unknown += 1; continue; }
                batch.push(json!({"path": dir.join("current_value"), "value": v}));
            }
            let note = if unknown > 0 { format!(" ({unknown} not present here)") } else { String::new() };
            if !batch.is_empty() {
                let v = pkexec_helper(&format!("{HELPER_DIR}/fwattr-helper"), &Value::Array(batch.clone()))?;
                if v["ok"] != true {
                    let bad: Vec<String> = v["results"].as_array().into_iter().flatten()
                        .filter(|x| x["ok"] != true).filter_map(|x| x["error"].as_str().map(str::to_owned)).collect();
                    return Err(if bad.is_empty() { v["error"].as_str().unwrap_or("failed").into() } else { bad.join("; ") });
                }
            }
            if !wmi.is_empty() {
                let v = pkexec_helper(&format!("{HELPER_DIR}/legion-gpu-helper"), &json!({"op": "apply", "values": wmi}))?;
                if v["ok"] != true { return Err(format!("GPU (WMI): {}", v["error"].as_str().unwrap_or("failed"))); }
            }
            Ok(format!("{} value(s){}{note}", batch.len(),
                       if wmi.is_empty() { String::new() } else { format!(" + {} GPU (WMI)", wmi.len()) }))
        })();
        line("firmware limits", r);
    }

    // 3. CPU curve, 4. GPU curve — CPU first, GPU a moment later, as for undervolt
    let mut did_cpu = false;
    if parts.cpu {
        if let Some(c) = scene_choice(&s["cpu_curve"]) {
            did_cpu = true;
            let r = match (tune::cpu_vendor(), c) {
                (tune::Vendor::Intel, Err(())) => pkexec_helper(&format!("{HELPER_DIR}/intel-uv-helper"), &json!({"op": "reset"}))
                    .and_then(|v| if v["ok"] == true { Ok("reset (0 mV)".into()) } else { Err(v["error"].as_str().unwrap_or("failed").into()) }),
                (tune::Vendor::Intel, Ok(n)) => apply_intel(&config_dir().join(format!("intel-uv-profiles/{n}.json"))).map(|_| format!("\"{n}\"")),
                (tune::Vendor::Amd, Err(())) => pkexec_helper(&format!("{HELPER_DIR}/ryzen-co-helper"), &json!({"op": "reset"}))
                    .and_then(|v| if v["ok"] == true { Ok("reset (0)".into()) } else { Err(v["error"].as_str().unwrap_or("failed").into()) }),
                (tune::Vendor::Amd, Ok(n)) => apply_ryzen(&ryzen_profiles_dir().join(format!("{n}.json"))).map(|_| format!("\"{n}\"")),
                _ => Err("no CPU curve backend for this CPU".into()),
            };
            line("CPU curve", r);
        }
    }
    if parts.gpu {
        if let Some(c) = scene_choice(&s["gpu_curve"]) {
            if did_cpu { std::thread::sleep(UNDERVOLT_GAP); }
            let r = match c {
                Err(()) => pkexec_helper(&format!("{HELPER_DIR}/nvcurve-root-helper"), &json!({"op": "reset_gpu_curve"}))
                    .and_then(|v| if v["ok"] == true { Ok("reset".into()) } else { Err(v["error"].as_str().unwrap_or("failed").into()) }),
                Ok(n) => apply_nvidia_named(&n).map(|_| format!("\"{n}\"")),
            };
            line("GPU curve", r);
        }
    }

    // 5. Optimizations preset — switched, not stacked ("replace"); a preset
    //    is applied by name from the approved store (tune-profile-helper).
    if parts.tuning {
        if let Some(c) = scene_choice(&s["tuning"]) {
            let r = (|| -> Result<String, String> {
                let preset = match &c { Err(()) => Value::Null, Ok(n) => json!(n) };
                let v = pkexec(&json!({"op": "apply_preset", "mode": "manual", "replace": true, "preset": preset}))?;
                if v["game_active"] == true { return Ok("left alone (game session active)".into()); }
                if v["ok"] != true { return Err(v["error"].as_str().unwrap_or("failed").into()); }
                Ok(match c { Err(()) => "originals restored".into(), Ok(n) => format!("\"{n}\"") })
            })();
            line("Optimizations", r);
        }
    }

    // 5a. Custom-mode fan curve: only while the Custom profile is active (the EC follows the table there only).
    if let Some(lv) = s["fan_table"].as_array().filter(|a| a.len() == 10 && a.iter().all(|x| x.as_u64().map_or(false, |n| (1..=10).contains(&n)))) {
        if current_platform_profile().as_deref() != Some("custom") {
            line("Fan curve", Ok("skipped (not the Custom power profile)".into()));
        } else {
            let v = pkexec_helper(&format!("{HELPER_DIR}/legion-profile-helper"), &json!({"fan_table": "set", "levels": lv}))
                .unwrap_or_else(|e| json!({"ok": false, "error": e}));
            line("Fan curve", if v["ok"] == true { Ok("table written".into()) } else { Err(v["error"].as_str().unwrap_or("failed").to_owned()) });
        }
    }

    // 6. keyboard lighting — in-process as the user (udev uaccess on the
    //    hidraw node); lighting-helper through pkexec only if that is denied.
    if let Some(req) = lighting::scene_request(&s["lighting"]) {
        let mut v = lighting::handle(&req);
        if v["denied"] == true {
            v = pkexec_helper(&format!("{HELPER_DIR}/lighting-helper"), &req)
                .unwrap_or_else(|e| json!({"ok": false, "error": e}));
        }
        line("lighting", if v["ok"] == true {
            Ok(format!("profile {} · brightness {}", v["profile"], v["brightness"]))
        } else {
            Err(v["error"].as_str().unwrap_or("failed").to_owned())
        });
    }

    // 7. user command — as the user, no shell, detached
    if let Some(cmd) = s["command"].as_str().map(str::trim).filter(|c| !c.is_empty()) {
        // Same quoting rules as the GUI (QProcess::splitCommand).
        let argv = split_command(cmd);
        if argv.is_empty() { return ok; }
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        unsafe { c.pre_exec(|| { libc::setsid(); Ok(()) }); }
        line("command", c.spawn().map(|_| argv[0].clone()).map_err(|e| format!("{}: {e}", argv[0])));
    }
    ok
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
            log!("  CCD{}/S{}: {}", r["ccd"], r["core"], r["message"].as_str().unwrap_or("failed"));
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

/// Intel undervolt profile (Intel Undervolt tab format) → intel-uv-helper.
fn apply_intel(path: &Path) -> Result<(), String> {
    let p = read_json(path).ok_or_else(|| format!("cannot read {}", path.display()))?;
    let v = pkexec_helper(&format!("{HELPER_DIR}/intel-uv-helper"), &json!({"op": "apply", "profile": p}))?;
    for r in v["results"].as_array().into_iter().flatten().filter(|r| r["ok"] != true) {
        log!("  {}: {}", r["what"].as_str().unwrap_or("?"), r["message"].as_str().unwrap_or("failed"));
    }
    if v["ok"] == true { Ok(()) } else { Err(v["error"].as_str().unwrap_or("some values failed").to_owned()) }
}

fn apply_nvidia() -> Result<(), String> { apply_nvidia_named(UNDERVOLT_PROFILE) }

fn apply_nvidia_named(name: &str) -> Result<(), String> {
    let v = pkexec_helper(&format!("{HELPER_DIR}/nvcurve-root-helper"),
        &json!({"op": "apply_named_profile", "name": name}))?;
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
    let intel = xdg_config().join(format!("legion-power-manager/intel-uv-profiles/{UNDERVOLT_PROFILE}.json"));
    if want_cpu && tune::cpu_vendor() == tune::Vendor::Intel {
        if intel.is_file() { let r = intel.clone(); steps.push(("CPU", Box::new(move || apply_intel(&r)))); }
        else { log!("lpm-gamemode: undervolt CPU skipped — no Intel undervolt profile \"{UNDERVOLT_PROFILE}\" ({})", intel.display()); }
    } else if want_cpu && tune::cpu_vendor() != tune::Vendor::Amd {
        log!("lpm-gamemode: undervolt CPU skipped — no CPU undervolt backend for this vendor");
    } else if want_cpu {
        if ryzen.is_file() { let r = ryzen.clone(); steps.push(("CPU", Box::new(move || apply_ryzen(&r)))); }
        else { log!("lpm-gamemode: undervolt CPU skipped — no Ryzen profile \"{UNDERVOLT_PROFILE}\" ({})", ryzen.display()); }
    }
    if want_gpu {
        if nvidia.is_file() { steps.push(("GPU", Box::new(apply_nvidia))); }
        else { log!("lpm-gamemode: undervolt GPU skipped — no NVIDIA profile \"{UNDERVOLT_PROFILE}\" ({})", nvidia.display()); }
    }
    let mut all_ok = true;
    for (i, (what, step)) in steps.iter().enumerate() {
        if i > 0 { std::thread::sleep(UNDERVOLT_GAP); }
        match step() {
            Ok(()) => log!("lpm-gamemode: undervolt {what}: \"{UNDERVOLT_PROFILE}\" applied"),
            Err(e) => { all_ok = false; log!("lpm-gamemode: undervolt {what} failed: {e}"); }
        }
    }
    all_ok && (!forced || !steps.is_empty())
}

// ── CCD park in game mode ─────────────────────────────────────────────────
// A hot-unplugged CCD leaves a hole in the CPU numbers (9955HX3D with CCD1
// parked: online 0-7,16-23). Wine takes the online *count* as its CPU count
// and maps logical CPU i to host CPU i (system affinity mask 0-15), while its
// processor info lists 0-7,16-23: every per-core thread pin of a game hits
// an offline or out-of-mask CPU, and games that check it do not start.
// nvidia-powerd dies on the missing CPUs too. Game mode therefore empties the
// CCD instead of taking it offline: the game is pinned to the other CCD(s),
// IRQs and unbound kernel work are moved onto the parked one. The
// Optimizations tab's manual Apply still hot-unplugs.

/// Turns a game preset's `cpu.ccd_park` into the soft form; returns the role.
/// A CCD that is already offline (parked by hand) is left as it is.
fn soft_park(preset: &mut Value) -> Option<String> {
    tune::soft_park_values(preset["values"].as_object_mut()?)
}

/// Proton's WINE_CPU_TOPOLOGY for this process's CPU set: logical CPU i ->
/// host CPU, SMT siblings as adjacent pairs ("Ns:a,b,…"). None when Wine's
/// own view (host 0..online-1) is already right. Proton rejects host ids
/// >= the online count, so above a hole only the ids below it are mapped.
fn wine_topology() -> Option<String> {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n < 1 { return None; }
    let n = n as usize;
    let mut usable = Vec::new();
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 { return None; }
        for c in 0..n { if libc::CPU_ISSET(c, &set) { usable.push(c); } }
    }
    if usable.is_empty() || usable.len() == n { return None; }
    let siblings = |c: usize| std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list"))
        .map(|s| tune::cpu_list(s.trim())).unwrap_or_default();
    let mut order: Vec<usize> = Vec::new();
    let mut pairs = true;
    for &c in &usable {
        if order.contains(&c) { continue; }
        let s: Vec<usize> = siblings(c).into_iter().filter(|x| usable.contains(x)).collect();
        if s.len() != 2 { pairs = false; break; }
        order.extend(s);
    }
    let list = |v: &[usize]| v.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
    Some(if pairs { format!("{}s:{}", order.len() / 2, list(&order)) } else { format!("{}:{}", usable.len(), list(&usable)) })
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
/// `parked`: soft-parked CCD role, the game is kept off it when the preset
/// pins nothing itself.
fn prepare_run(preset: &Value, parked: Option<&str>) {
    let run = &preset["run"];
    let nice = run["nice"].as_i64().unwrap_or(0);
    if (-20..=-1).contains(&nice) {
        match pkexec(&json!({"op": "boost", "nice": nice, "autogroup": run["autogroup"].as_bool().unwrap_or(true)})) {
            Ok(v) if v["ok"] == true => {}
            Ok(v) => log!("lpm-gamemode: nice boost failed: {}", v["error"].as_str().unwrap_or("?")),
            Err(e) => log!("lpm-gamemode: nice boost failed: {e}"),
        }
    }
    // Soft park: the game gets the other CCD(s) to itself through a cgroup
    // partition (everything else is moved off them); pinning below stays as
    // the fallback when the partition is not available.
    if let Some(pr) = parked {
        match pkexec(&json!({"op": "isolate_join", "park": pr})) {
            Ok(v) if v["ok"] == true => log!("lpm-gamemode: game CPU partition {} (rest of the system moved off it)", v["cpus"].as_str().unwrap_or("?")),
            Ok(v) => log!("lpm-gamemode: game CPU partition not used: {}", v["error"].as_str().unwrap_or("?")),
            Err(e) => log!("lpm-gamemode: game CPU partition not used: {e}"),
        }
    }
    let role = run["affinity"].as_str().unwrap_or("none");
    if role != "none" {
        match tune::resolve_ccd(&tune::ccx_groups(), role) {
            Some(g) => match set_affinity(&g.cpus) {
                Ok(()) if role == "pcore" || role == "ecore" =>
                    log!("lpm-gamemode: pinned to {} ({})", if role == "pcore" { "P-cores" } else { "E-cores" }, tune::fmt_cpu_list(&g.cpus)),
                Ok(()) => log!("lpm-gamemode: pinned to CCD{} ({}, {} MiB L3)", g.index, tune::fmt_cpu_list(&g.cpus), g.l3_kib / 1024),
                Err(e) => log!("lpm-gamemode: sched_setaffinity: {e}"),
            },
            None => log!("lpm-gamemode: affinity '{role}' does not apply to this CPU, skipped"),
        }
    } else if let Some(pr) = parked {
        let groups = tune::ccx_groups();
        if let Some(pg) = tune::resolve_ccd(&groups, pr) {
            let rest: Vec<usize> = groups.iter().flat_map(|g| g.cpus.iter().copied()).filter(|c| !pg.cpus.contains(c)).collect();
            match set_affinity(&rest) {
                Ok(()) => log!("lpm-gamemode: pinned off the parked CCD ({})", tune::fmt_cpu_list(&rest)),
                Err(e) => log!("lpm-gamemode: sched_setaffinity: {e}"),
            }
        }
    }
    // Wine/Proton must see exactly the CPUs the game may use (see soft_park).
    if std::env::var_os("WINE_CPU_TOPOLOGY").is_none() {
        if let Some(t) = wine_topology() {
            std::env::set_var("WINE_CPU_TOPOLOGY", &t);
            log!("lpm-gamemode: WINE_CPU_TOPOLOGY={t}");
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

/// Port of QProcess::splitCommand: whitespace separates, double quotes group,
/// and three consecutive quotes give one literal quote. The GUI starts scene
/// commands with the Qt function, so both must split identically.
fn split_command(cmd: &str) -> Vec<String> {
    let (mut args, mut tmp) = (Vec::new(), String::new());
    let (mut quotes, mut in_quote) = (0, false);
    for c in cmd.chars() {
        if c == '"' {
            quotes += 1;
            if quotes == 3 { quotes = 0; tmp.push(c); }
            continue;
        }
        if quotes > 0 {
            if quotes == 1 { in_quote = !in_quote; }
            quotes = 0;
        }
        if !in_quote && c.is_whitespace() {
            if !tmp.is_empty() { args.push(std::mem::take(&mut tmp)); }
        } else {
            tmp.push(c);
        }
    }
    if !tmp.is_empty() { args.push(tmp); }
    args
}

static CHILD: AtomicI32 = AtomicI32::new(0);
extern "C" fn forward(sig: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 { unsafe { libc::kill(pid, sig) }; }
}

fn wrap(args: &[String]) -> i32 {
    OWNER.store(std::process::id() as i32, Ordering::SeqCst);
    let (name, cmd) = split_cmd(args);
    if cmd.is_empty() { log!("lpm-gamemode WRAP: no command"); return 2; }
    let (pname, mut preset) = match load_preset(name.as_deref()) { Ok(p) => p, Err(e) => { log!("lpm-gamemode: {e}"); return 2 } };
    let parked = soft_park(&mut preset);
    // The refcount is taken as soon as the helper was reached, even if a knob failed.
    let (entered, _) = game_start(Some(&pname));
    prepare_run(&preset, parked.as_deref());
    // Forward termination so POST still runs when the launcher stops us.
    // `as *const ()` first: casting a function item straight to an integer type is
    // deprecated (function pointers aren't guaranteed integer-representable), even
    // though it's always fine in practice on the platforms this runs on.
    // Signals are held back until the child's pid is known: one that arrived
    // in between used to be dropped (nothing to forward to yet). The child
    // itself starts with an empty mask (std resets it before exec).
    let mut held: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut held);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] { libc::sigaddset(&mut held, s); }
        libc::sigprocmask(libc::SIG_BLOCK, &held, std::ptr::null_mut());
    }
    for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] { unsafe { libc::signal(s, forward as *const () as libc::sighandler_t) }; }
    let spawned = Command::new(&cmd[0]).args(&cmd[1..]).spawn();
    if let Ok(c) = &spawned { CHILD.store(c.id() as i32, Ordering::SeqCst); }
    unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &held, std::ptr::null_mut()); }
    let code = match spawned {
        Ok(mut c) => {
            loop {
                match c.wait() {
                    Ok(st) => break st.code().unwrap_or(1),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break 1,
                }
            }
        }
        Err(e) => { log!("lpm-gamemode: {}: {e}", cmd[0]); 127 }
    };
    if entered { if let Err(e) = post() { log!("lpm-gamemode: {e}"); } }
    code
}

/// Lutris runs the pre-launch script (PRE) and the command prefix (RUN) in
/// parallel unless "Wait for pre-launch script completion" is set. RUN must
/// not start the game while PRE is still working: the game scene / "GAMING"
/// undervolt (CPU CO, then the NVIDIA curve reset + write + read-back) would
/// otherwise run while the game is bringing the dGPU up, and a preset that
/// hot-plugs CPUs (SMT off, CCD park) must finish before RUN reads the CCD
/// topology. PRE holds gamemode-start.lock for its whole start sequence, so
/// RUN waits until that lock is free and game mode is active.
fn start_lock_held() -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(f) = std::fs::OpenOptions::new().write(true).open(runtime_dir().join("gamemode-start.lock")) else { return false };
    let busy = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
    busy  // our own lock (if taken) is released when `f` drops
}

fn wait_for_start(preset: &Value) {
    let t0 = std::time::Instant::now();
    let hotplug = tune::HOTPLUG_KEYS.iter().any(|k| !preset["values"][*k].is_null());
    let smt_off = preset["values"]["cpu.smt"] == "off";
    loop {
        let held = start_lock_held();
        // tune-helper's world-readable state file: no helper process (full sysfs describe) every 200 ms while the game starts.
        let active = !held && read_json(Path::new("/run/legion-power-manager/tune/state.json"))
            .map_or(false, |st| st["source"] == "game" && lpm_helpers::live_game_sessions(&st) > 0);
        let smt = !smt_off || read_json_str("/sys/devices/system/cpu/smt/active").as_deref() == Some("0");
        if active && smt { break; }
        let waited = t0.elapsed();
        // No PRE hook at all: nothing will ever take the lock.
        if !held && !active && waited >= START_DETECT {
            if hotplug { log!("lpm-gamemode RUN: game mode not applied (is PRE set, and did it succeed?) — pinning with the current topology"); }
            return;
        }
        if waited >= START_WAIT {
            log!("lpm-gamemode RUN: PRE still busy after {} s — starting the game anyway", START_WAIT.as_secs());
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    // CPU offlining finishes before tune-helper returns, but give cacheinfo a beat to settle.
    if hotplug { std::thread::sleep(std::time::Duration::from_millis(200)); }
}

fn read_json_str(p: &str) -> Option<String> { std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned()) }


fn run_exec(args: &[String]) -> i32 {
    let (name, cmd) = split_cmd(args);
    if cmd.is_empty() { log!("lpm-gamemode RUN: no command"); return 2; }
    match load_preset(name.as_deref()) {
        Ok((_, mut p)) => {
            let parked = soft_park(&mut p);
            wait_for_start(&p);
            prepare_run(&p, parked.as_deref())
        }
        Err(e) => log!("lpm-gamemode: {e} — starting without boost"),
    }
    let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
    log!("lpm-gamemode: {}: {err}", cmd[0]);
    127
}

fn status() -> i32 {
    let out = Command::new(helper()).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()
        .and_then(|mut c| { c.stdin.take().unwrap().write_all(br#"{"op":"describe"}"#)?; c.wait_with_output() });
    let Some(v) = out.ok().and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok()) else {
        log!("lpm-gamemode: cannot run {}", helper());
        return 1;
    };
    let st = &v["state"];
    println!("tuning   : {} (source {}, preset {}, {} game(s), {} file(s) saved)",
        if st["active"] == true { "ACTIVE" } else { "off" }, st["source"].as_str().unwrap_or("-"),
        st["preset"].as_str().unwrap_or("-"), st["refcount"], st["saved_files"]);
    println!("default  : {}", default_preset().unwrap_or_else(|| "-".into()));
    let iso = &v["isolation"];
    if iso["active"] == true {
        println!("partition: {} ({}, {} process(es))", iso["cpus"].as_str().unwrap_or(""), iso["partition"].as_str().unwrap_or(""), iso["procs"]);
    }
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

/// APPROVE [preset…]: stores the values of user presets (all of them by default) in the root-owned store the
/// helper applies from. Needs the tune-helper authorization. Migrates presets saved before the store existed.
fn approve(names: &[String]) -> i32 {
    let all: Vec<String> = if names.is_empty() {
        std::fs::read_dir(presets_dir()).into_iter().flatten().flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|s| s.strip_suffix(".json")).map(str::to_owned)).collect()
    } else { names.to_vec() };
    let mut presets = serde_json::Map::new();
    for n in all.into_iter().filter(|n| valid_name(n)) {
        if let Some(v) = read_json(&presets_dir().join(format!("{n}.json"))) {
            if v["values"].is_object() { presets.insert(n, v["values"].clone()); }
        }
    }
    if presets.is_empty() { log!("lpm-gamemode APPROVE: no preset to approve"); return 1; }
    let n = presets.len();
    match pkexec_helper(&format!("{HELPER_DIR}/tune-helper"), &json!({"op": "preset_save", "presets": presets})) {
        Ok(v) => { let ok = report("APPROVE", &v); if ok { log!("lpm-gamemode APPROVE: {n} preset(s) approved"); } (!ok) as i32 }
        Err(e) => { log!("lpm-gamemode: {e}"); 1 }
    }
}

fn usage() -> i32 {
    log!("usage: lpm-gamemode PRE [preset] | POST | RUN [preset] [--] cmd… | WRAP [preset] [--] cmd…\n\
               \x20                   | APPLY preset | UNDERVOLT | SCENE name | RESTORE | STATUS | APPROVE [preset…]");
    2
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = lpm_helpers::machine::check() {
        log!("lpm-gamemode: {e}");
        // RUN / WRAP still start the game — only without any tuning.
        if let Some(mode) = args.first().map(|s| s.to_ascii_uppercase()).filter(|m| m == "RUN" || m == "WRAP") {
            let (_, cmd) = split_cmd(&args[1..]);
            if cmd.is_empty() { log!("lpm-gamemode {mode}: no command"); std::process::exit(2); }
            let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
            log!("lpm-gamemode: {}: {err}", cmd[0]);
            std::process::exit(127);
        }
        std::process::exit(1);
    }
    let fail = |e: String| { log!("lpm-gamemode: {e}"); 1 };
    let code = match args.first().map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("PRE") => { OWNER.store(launcher_pid(), Ordering::SeqCst); pre(args.get(1).map(String::as_str)) }
        Some("POST") => { OWNER.store(launcher_pid(), Ordering::SeqCst); post().map_or_else(fail, |ok| (!ok) as i32) }
        Some("APPLY") if args.len() == 2 => apply(Some(&args[1]), "manual").map_or_else(fail, |ok| (!ok) as i32),
        Some("APPROVE") => approve(&args[1..]),
        Some("RESTORE") => pkexec(&json!({"op": "restore"})).map_or_else(fail, |v| (!report("RESTORE", &v)) as i32),
        Some("UNDERVOLT") => (!undervolt(true)) as i32,
        Some("SCENE") if args.len() == 2 => {
            let ok = apply_scene(&args[1], SceneParts { cpu: true, gpu: true, tuning: true });
            let mut st = read_scene_state();
            st["active"] = json!(args[1]);
            write_scene_state(&st);
            (!ok) as i32
        }
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
    fn split_like_qt() {
        assert_eq!(split_command(r#"kscreen-doctor output.eDP-1.mode.2560x1600@240"#),
                   vec!["kscreen-doctor", "output.eDP-1.mode.2560x1600@240"]);
        assert_eq!(split_command(r#"notify-send "Game mode" 'x'"#), vec!["notify-send", "Game mode", "'x'"]);
        assert_eq!(split_command(r#"a """b""" c"#), vec!["a", "\"b\"", "c"]);
        assert!(split_command("   ").is_empty());
    }
    #[test]
    fn split() {
        let a: Vec<String> = ["--", "wine", "game.exe"].iter().map(|s| s.to_string()).collect();
        assert_eq!(split_cmd(&a), (None, vec!["wine".to_string(), "game.exe".to_string()]));
    }
}
