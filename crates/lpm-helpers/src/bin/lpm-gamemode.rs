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
const HOTPLUG_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
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
    let (name, p) = load_preset(name)?;
    let v = pkexec(&json!({"op": "apply", "mode": mode, "preset": name, "values": p["values"]}))?;
    Ok(report(if mode == "game" { "PRE" } else { "APPLY" }, &v))
}

/// POST: release game mode; when that was the last game, leave the game scene.
fn post() -> Result<bool, String> {
    let v = pkexec(&json!({"op": "release"}))?;
    let ok = report("POST", &v);
    if v["restored"] == true { leave_game_scene(); }
    Ok(ok)
}

/// First PRE / WRAP of a session: the game scene (or the legacy "GAMING"
/// undervolt), then the game-mode preset. Returns whether the helper took the
/// game-mode reference (so POST must run) and the exit code.
fn game_start(name: Option<&str>) -> (bool, i32) {
    let cfg = read_json(&config_dir().join("tune.json")).unwrap_or(Value::Null);
    let first = game_refcount() == 0;
    match cfg["game_scene"].as_str().filter(|n| valid_name(n)) {
        Some(scene) if first => enter_game_scene(scene, cfg["undervolt_cpu"] == true, cfg["undervolt_gpu"] == true),
        Some(_) => log!("lpm-gamemode: another game is running — its scene stays"),
        None => { undervolt(false); }
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

/// Running game sessions, from tune-helper's world-readable state.
fn game_refcount() -> i64 {
    read_json(Path::new("/run/legion-power-manager/tune/state.json")).and_then(|v| v["refcount"].as_i64()).unwrap_or(0)
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

fn leave_game_scene() {
    let mut st = read_scene_state();
    if st.get("game_scene").map_or(true, Value::is_null) { return; }
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

    // 1. power profile (firmware limits below need Custom)
    if let Some(p) = s["platform_profile"].as_str().filter(|p| !p.is_empty() && p.len() <= 32) {
        let r = pkexec_helper(&format!("{HELPER_DIR}/legion-profile-helper"),
                              &json!({"profile": p, "handler": platform_profile_node()}))
            .and_then(|v| if v["ok"] == true { Ok(v["effective"].as_str().unwrap_or(p).to_owned()) }
                          else { Err(v["error"].as_str().unwrap_or("failed").to_owned()) });
        line("power profile", r);
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
    //    that only exists built into the GUI travels in the scene as "values".
    if parts.tuning {
        if let Some(c) = scene_choice(&s["tuning"]) {
            let r = (|| -> Result<String, String> {
                let (values, preset) = match &c {
                    Err(()) => (json!({}), Value::Null),
                    Ok(n) => {
                        let vals = load_preset(Some(n)).map(|(_, p)| p["values"].clone()).ok()
                            .or_else(|| s["tuning"]["values"].as_object().map(|m| Value::Object(m.clone())))
                            .ok_or_else(|| format!("preset \"{n}\" not found"))?;
                        (vals, json!(n))
                    }
                };
                let v = pkexec(&json!({"op": "apply", "mode": "manual", "replace": true, "values": values, "preset": preset}))?;
                if v["game_active"] == true { return Ok("left alone (game session active)".into()); }
                if v["ok"] != true { return Err(v["error"].as_str().unwrap_or("failed").into()); }
                Ok(match c { Err(()) => "originals restored".into(), Ok(n) => format!("\"{n}\"") })
            })();
            line("Optimizations", r);
        }
    }

    // 6. user command — as the user, no shell, detached
    if let Some(cmd) = s["command"].as_str().map(str::trim).filter(|c| !c.is_empty()) {
        let argv: Vec<&str> = cmd.split_whitespace().collect();
        let mut c = Command::new(argv[0]);
        c.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        unsafe { c.pre_exec(|| { libc::setsid(); Ok(()) }); }
        line("command", c.spawn().map(|_| argv[0].to_owned()).map_err(|e| format!("{}: {e}", argv[0])));
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
            Ok(v) => log!("lpm-gamemode: nice boost failed: {}", v["error"].as_str().unwrap_or("?")),
            Err(e) => log!("lpm-gamemode: nice boost failed: {e}"),
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
    if cmd.is_empty() { log!("lpm-gamemode WRAP: no command"); return 2; }
    let (pname, preset) = match load_preset(name.as_deref()) { Ok(p) => p, Err(e) => { log!("lpm-gamemode: {e}"); return 2 } };
    // The refcount is taken as soon as the helper was reached, even if a knob failed.
    let (entered, _) = game_start(Some(&pname));
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
        Err(e) => { log!("lpm-gamemode: {}: {e}", cmd[0]); 127 }
    };
    if entered { if let Err(e) = post() { log!("lpm-gamemode: {e}"); } }
    code
}

/// Lutris runs the pre-launch script (PRE) and the command prefix (RUN) in
/// parallel unless "Wait for pre-launch script completion" is set. If the
/// preset hot-plugs CPUs (SMT off, CCD park), RUN must not read the CCD
/// topology — or pin the game — while CPUs are still going offline.
fn wait_for_hotplug(preset: &Value) {
    let vals = &preset["values"];
    if !tune::HOTPLUG_KEYS.iter().any(|k| !vals[*k].is_null()) { return; }
    let deadline = std::time::Instant::now() + HOTPLUG_WAIT;
    let smt_off = vals["cpu.smt"] == "off";
    let settled = || {
        let game = describe_state().map_or(false, |st| st["source"] == "game" && st["refcount"].as_u64().unwrap_or(0) > 0);
        let smt = !smt_off || read_json_str("/sys/devices/system/cpu/smt/active").as_deref() == Some("0");
        game && smt
    };
    while !settled() {
        if std::time::Instant::now() >= deadline {
            log!("lpm-gamemode RUN: game mode not applied after {} s (is PRE set, and did it succeed?) — pinning with the current topology",
                      HOTPLUG_WAIT.as_secs());
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    // CPU offlining finishes before tune-helper returns, but give cacheinfo a beat to settle.
    std::thread::sleep(std::time::Duration::from_millis(200));
}

fn read_json_str(p: &str) -> Option<String> { std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned()) }

/// tune-helper's describe, run directly as the user (no pkexec needed).
fn describe_state() -> Option<Value> {
    let out = Command::new(helper()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
        .and_then(|mut c| { c.stdin.take().unwrap().write_all(br#"{"op":"describe"}"#)?; c.wait_with_output() }).ok()?;
    serde_json::from_slice::<Value>(&out.stdout).ok().map(|v| v["state"].clone())
}

fn run_exec(args: &[String]) -> i32 {
    let (name, cmd) = split_cmd(args);
    if cmd.is_empty() { log!("lpm-gamemode RUN: no command"); return 2; }
    match load_preset(name.as_deref()) {
        Ok((_, p)) => { wait_for_hotplug(&p); prepare_run(&p) }
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
    log!("usage: lpm-gamemode PRE [preset] | POST | RUN [preset] [--] cmd… | WRAP [preset] [--] cmd…\n\
               \x20                   | APPLY preset | UNDERVOLT | SCENE name | RESTORE | STATUS");
    2
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fail = |e: String| { log!("lpm-gamemode: {e}"); 1 };
    let code = match args.first().map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("PRE") => pre(args.get(1).map(String::as_str)),
        Some("POST") => post().map_or_else(fail, |ok| (!ok) as i32),
        Some("APPLY") if args.len() == 2 => apply(Some(&args[1]), "manual").map_or_else(fail, |ok| (!ok) as i32),
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
    fn split() {
        let a: Vec<String> = ["--", "wine", "game.exe"].iter().map(|s| s.to_string()).collect();
        assert_eq!(split_cmd(&a), (None, vec!["wine".to_string(), "game.exe".to_string()]));
    }
}
