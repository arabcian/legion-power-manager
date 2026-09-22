//! Root helper for the Optimizations tab, `lpm-gamemode` and the boot service.
//!
//! One JSON object on stdin, one JSON line on stdout:
//!   {"op":"describe"}                                  any user: tunables, live values, state, topology
//!   {"op":"apply","values":{k:v,..},"mode":"manual"|"game","preset":"name"}
//!   {"op":"release"}                                   game POST: refcount-1, restore at 0
//!   {"op":"restore"}                                   write every saved original back now
//!   {"op":"restore_keys","keys":[k,..]}                restore only these knobs
//!   {"op":"boost","nice":-5,"autogroup":true}          renice the process that ran pkexec
//!   {"op":"set_boot","values":{..}|null,"preset":".."} store/clear the boot preset (root-owned file)
//!   {"op":"boot"}                                      apply the boot preset (OpenRC service)
//!
//! Keys are looked up in lpm_helpers::tune::TUNABLES; paths never come from a
//! request. The first write to a concrete file records its original value in
//! /run/legion-power-manager/tune/state.json (tmpfs: a reboot is a full
//! restore). Game mode is reference counted like lutris-game-tune: the first
//! PRE applies, the last POST restores.

use lpm_helpers::tune::{self, TUNABLES};
use lpm_helpers::*;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const MAX_STDIN_BYTES: usize = 64 * 1024;
const RUN_DIR: &str = "/run/legion-power-manager";
const STATE_DIR: &str = "/run/legion-power-manager/tune";
const STATE_FILE: &str = "/run/legion-power-manager/tune/state.json";
const LOCK_FILE: &str = "/run/legion-power-manager/tune/lock";
const ETC_DIR: &str = "/etc/legion-power-manager";
const BOOT_FILE: &str = "/etc/legion-power-manager/tune-boot.json";
const MAX_BASELINE: usize = 16_384;
const MAX_REFCOUNT: u32 = 64;

fn is_root() -> bool { unsafe { libc::geteuid() == 0 } }

/// Creates (or verifies) a root-owned, non-symlink, not group/other-writable dir.
fn secure_dir(p: &str) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o755).create(p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{p}: {e}")),
    }
    let m = fs::symlink_metadata(p).map_err(|e| format!("{p}: {e}"))?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(format!("{p} is not a root-owned private directory; refusing to use it"));
    }
    Ok(())
}

/// Reads a root-owned, not group/other-writable regular file (no symlinks).
fn read_root_file(p: &str, max: u64) -> Option<String> {
    let m = fs::symlink_metadata(p).ok()?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 || m.len() > max { return None; }
    fs::read_to_string(p).ok()
}

/// Atomic root-owned write: tmp (O_EXCL, no symlink) + fsync + rename.
fn write_root_file(p: &str, body: &[u8]) -> Result<(), String> {
    let tmp = format!("{p}.tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&tmp)
        .map_err(|e| format!("{tmp}: {e}"))?;
    f.write_all(body).and_then(|_| f.sync_all()).map_err(|e| format!("{tmp}: {e}"))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).ok();
    fs::rename(&tmp, p).map_err(|e| format!("{p}: {e}"))
}

struct Lock(#[allow(dead_code)] fs::File);
fn lock() -> Result<Lock, String> {
    secure_dir(RUN_DIR)?;
    secure_dir(STATE_DIR)?;
    let f = fs::OpenOptions::new().create(true).read(true).write(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(LOCK_FILE)
        .map_err(|e| format!("lock: {e}"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("lock: flock failed".into());
    }
    Ok(Lock(f))
}

#[derive(Default)]
struct State {
    /// (key, path, original) in first-write order.
    baseline: Vec<(String, PathBuf, String)>,
    refcount: u32,
    preset: Option<String>,
    /// "manual" | "game" | "boot" — who applied last.
    source: Option<String>,
}

impl State {
    fn load() -> State {
        let Some(text) = read_root_file(STATE_FILE, 8 << 20) else { return State::default() };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { return State::default() };
        let baseline = v["baseline"].as_array().map(|a| a.iter().filter_map(|e| {
            let key = e[0].as_str()?;
            tune::find(key)?; // drop anything the table no longer knows
            Some((key.to_owned(), PathBuf::from(e[1].as_str()?), e[2].as_str()?.to_owned()))
        }).take(MAX_BASELINE).collect()).unwrap_or_default();
        State {
            baseline,
            refcount: v["refcount"].as_u64().unwrap_or(0).min(MAX_REFCOUNT as u64) as u32,
            preset: v["preset"].as_str().map(str::to_owned),
            source: v["source"].as_str().map(str::to_owned),
        }
    }

    fn save(&self) -> Result<(), String> {
        let v = json!({
            "baseline": self.baseline.iter().map(|(k, p, v)| json!([k, p, v])).collect::<Vec<_>>(),
            "refcount": self.refcount, "preset": self.preset, "source": self.source,
        });
        write_root_file(STATE_FILE, &serde_json::to_vec_pretty(&v).unwrap())
    }

    fn has(&self, p: &Path) -> bool { self.baseline.iter().any(|(_, q, _)| q == p) }
}

fn summary(st: &State) -> Value {
    let mut keys: Vec<&str> = Vec::new();
    for (k, _, _) in &st.baseline { if !keys.contains(&k.as_str()) { keys.push(k); } }
    json!({"active": !st.baseline.is_empty(), "refcount": st.refcount, "preset": st.preset,
           "source": st.source, "saved_files": st.baseline.len(), "keys": keys})
}

/// Applies `values` in table order. Returns per-key results and all-ok.
fn apply_values(st: &mut State, values: &Map<String, Value>) -> (Vec<Value>, bool) {
    let mut results = Vec::new();
    let mut all_ok = true;
    for k in values.keys() {
        if tune::find(k).is_none() {
            results.push(json!({"key": k, "ok": false, "error": "unknown key"}));
            all_ok = false;
        }
    }
    // Table order, not request order: pstate mode first, hot-plug last.
    for t in TUNABLES {
        let Some(raw) = values.get(t.key) else { continue };
        if t.debugfs && !tune::ensure_debugfs() {
            results.push(json!({"key": t.key, "ok": true, "skipped": "debugfs unavailable"}));
            continue;
        }
        if tune::files(t).is_empty() {
            // Absent hardware/kernel feature is not an error for a portable preset.
            results.push(json!({"key": t.key, "ok": true, "skipped": "not available"}));
            continue;
        }
        let v = match tune::validate(t, raw) {
            Ok(v) => v,
            Err(e) => { results.push(json!({"key": t.key, "ok": false, "error": e})); all_ok = false; continue; }
        };
        let plan = match tune::plan(t, &v) {
            Ok(p) => p,
            Err(e) => { results.push(json!({"key": t.key, "ok": false, "error": e})); all_ok = false; continue; }
        };
        let (mut written, mut refused, mut errs) = (0, 0, Vec::new());
        for (f, data) in plan {
            let Some(orig) = tune::baseline_value(t, &f) else {
                if tune::best_effort(t) { refused += 1; } else { errs.push(format!("{}: unreadable", f.display())); }
                continue;
            };
            if tune::same_value(t, &orig, &data) { continue; }
            let fresh = !st.has(&f);
            if fresh {
                if st.baseline.len() >= MAX_BASELINE { errs.push("baseline full".into()); break; }
                // Recorded and persisted *before* the write, so a crash or
                // kill mid-batch still leaves the original restorable.
                st.baseline.push((t.key.to_owned(), f.clone(), orig));
                if let Err(e) = st.save() {
                    st.baseline.pop();
                    errs.push(format!("state not saved, write skipped: {e}"));
                    break;
                }
            }
            match tune::write_checked(&f, &data) {
                Ok(()) => written += 1,
                Err(e) => {
                    // Nothing changed, so nothing to restore later.
                    if fresh { st.baseline.pop(); }
                    if tune::best_effort(t) { refused += 1; } else { errs.push(e); }
                }
            }
        }
        let mut r = json!({"key": t.key, "value": v, "written": written});
        if refused > 0 { r["refused"] = json!(refused); }
        if errs.is_empty() {
            r["ok"] = json!(true);
        } else {
            all_ok = false;
            errs.truncate(4);
            r["ok"] = json!(false);
            r["error"] = json!(errs.join("; "));
        }
        results.push(r);
    }
    (results, all_ok)
}

/// Writes saved originals back. Hot-plug knobs first (CPUs must be online
/// before their cpufreq/cpuidle files accept writes), then the rest in the
/// order they were first written (pstate mode before governor/EPP).
fn restore_entries(st: &mut State, only: Option<&[String]>) -> Value {
    let selected = |k: &str| only.map_or(true, |o| o.iter().any(|x| x == k));
    let mut order: Vec<usize> = (0..st.baseline.len()).filter(|&i| selected(&st.baseline[i].0)).collect();
    order.sort_by_key(|&i| (!tune::is_hotplug(&st.baseline[i].0), i));
    let (mut n, mut errs) = (0, Vec::new());
    for &i in &order {
        let (key, f, orig) = &st.baseline[i];
        match tune::write_checked(f, orig) {
            Ok(()) => n += 1,
            // Per-policy files vanish when the pstate mode is restored first; not an error.
            Err(_) if !f.exists() => {}
            Err(_) if tune::find(key).map_or(false, tune::best_effort) => {}
            Err(e) => errs.push(json!({"key": key, "error": e})),
        }
    }
    let drop: std::collections::HashSet<usize> = order.into_iter().collect();
    let mut i = 0;
    st.baseline.retain(|_| { let keep = !drop.contains(&i); i += 1; keep });
    if st.baseline.is_empty() { st.refcount = 0; st.preset = None; st.source = None; }
    json!({"restored": n, "errors": errs})
}

fn locked<F: FnOnce(&mut State) -> Value>(f: F) -> Value {
    let _l = match lock() { Ok(l) => l, Err(e) => return json!({"ok": false, "error": e}) };
    let mut st = State::load();
    let mut out = f(&mut st);
    if let Err(e) = st.save() {
        out["ok"] = json!(false);
        out["error"] = json!(e);
    }
    out["state"] = summary(&st);
    out
}

fn valid_preset(p: &Value) -> Option<String> {
    p.as_str().filter(|s| !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_alphanumeric() || " _-.".contains(c))).map(str::to_owned)
}

fn op_apply(req: &Value) -> Value {
    let Some(values) = req["values"].as_object() else { return json!({"ok": false, "error": "values must be an object"}) };
    if values.len() > TUNABLES.len() { return json!({"ok": false, "error": "too many values"}); }
    let mode = req["mode"].as_str().unwrap_or("manual");
    if !["manual", "game"].contains(&mode) { return json!({"ok": false, "error": "mode must be manual or game"}); }
    let preset = valid_preset(&req["preset"]);
    locked(|st| {
        if mode == "game" {
            if st.refcount >= MAX_REFCOUNT { return json!({"ok": false, "error": "too many concurrent game sessions"}); }
            st.refcount += 1;
            if st.refcount > 1 {
                return json!({"ok": true, "applied": false,
                              "message": format!("game mode already active ({} games running)", st.refcount)});
            }
        }
        st.preset = preset.clone().or(st.preset.take());
        st.source = Some(mode.to_owned());
        let (results, ok) = apply_values(st, values);
        json!({"ok": ok, "applied": true, "results": results,
               "error": (!ok).then_some("some settings could not be applied")})
    })
}

fn op_release() -> Value {
    locked(|st| {
        if st.refcount > 1 {
            st.refcount -= 1;
            return json!({"ok": true, "restored": false, "message": format!("{} game(s) still running", st.refcount)});
        }
        let r = restore_entries(st, None);
        let ok = r["errors"].as_array().map_or(true, |a| a.is_empty());
        json!({"ok": ok, "restored": true, "results": [r], "error": (!ok).then_some("some values could not be restored")})
    })
}

fn op_restore(req: &Value) -> Value {
    let keys: Option<Vec<String>> = match &req["keys"] {
        Value::Null => None,
        Value::Array(a) if a.len() <= TUNABLES.len() => Some(a.iter().filter_map(|k| k.as_str())
            .filter(|k| tune::find(k).is_some()).map(str::to_owned).collect()),
        _ => return json!({"ok": false, "error": "keys must be an array of known keys"}),
    };
    locked(|st| {
        let r = restore_entries(st, keys.as_deref());
        let ok = r["errors"].as_array().map_or(true, |a| a.is_empty());
        json!({"ok": ok, "results": [r], "error": (!ok).then_some("some values could not be restored")})
    })
}

/// Real uid of `pid` from /proc/<pid>/status.
fn proc_ruid(pid: i32) -> Option<u32> {
    let s = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().next()?.parse().ok()
}

fn op_boost(req: &Value) -> Value {
    let Some(nice) = req["nice"].as_i64().filter(|n| (-20..=-1).contains(n)) else {
        return json!({"ok": false, "error": "nice must be an integer in -20..-1"});
    };
    // pkexec execs us in place (polkit >= 0.106; it also sets PKEXEC_UID
    // after clearing the environment, so the caller cannot forge it), so our parent is the program that ran pkexec.
    // Only that process is boosted, and only if it belongs to the user pkexec
    // authenticated (PKEXEC_UID is set by pkexec itself, not by the caller).
    let ppid = unsafe { libc::getppid() };
    let Some(caller) = std::env::var("PKEXEC_UID").ok().and_then(|v| v.parse::<u32>().ok()) else {
        return json!({"ok": false, "error": "boost is only available through pkexec"});
    };
    if ppid <= 1 || proc_ruid(ppid) != Some(caller) || caller == 0 {
        return json!({"ok": false, "error": "caller process does not belong to the authenticated user"});
    }
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, ppid as libc::id_t, nice as libc::c_int) } != 0 {
        return json!({"ok": false, "error": format!("setpriority: {}", std::io::Error::last_os_error())});
    }
    let ag = req["autogroup"].as_bool().unwrap_or(false)
        .then(|| fs::write(format!("/proc/{ppid}/autogroup"), nice.to_string()).is_ok());
    json!({"ok": true, "pid": ppid, "nice": nice, "autogroup": ag})
}

fn op_set_boot(req: &Value) -> Value {
    if let Err(e) = secure_dir(ETC_DIR) { return json!({"ok": false, "error": e}); }
    match &req["values"] {
        Value::Null => match fs::remove_file(BOOT_FILE) {
            Ok(()) => json!({"ok": true, "boot": null}),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({"ok": true, "boot": null}),
            Err(e) => json!({"ok": false, "error": format!("{BOOT_FILE}: {e}")}),
        },
        Value::Object(values) => {
            // Store only known keys with values that validate against *this* machine.
            let mut clean = Map::new();
            let mut rejected = Vec::new();
            for (k, v) in values {
                match tune::find(k) {
                    // Not on this machine: nothing to validate against, so not stored.
                    Some(t) if tune::files(t).is_empty() && !t.debugfs => {}
                    Some(t) => match tune::validate(t, v) {
                        Ok(s) => { clean.insert(k.clone(), json!(s)); }
                        Err(e) => rejected.push(json!({"key": k, "error": e})),
                    },
                    None => rejected.push(json!({"key": k, "error": "unknown key"})),
                }
            }
            if !rejected.is_empty() { return json!({"ok": false, "error": "invalid values", "results": rejected}); }
            let body = json!({"preset": valid_preset(&req["preset"]), "values": clean});
            match write_root_file(BOOT_FILE, &serde_json::to_vec_pretty(&body).unwrap()) {
                Ok(()) => json!({"ok": true, "boot": body}),
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
        _ => json!({"ok": false, "error": "values must be an object or null"}),
    }
}

fn boot_preset() -> Option<Value> {
    read_root_file(BOOT_FILE, 256 * 1024).and_then(|s| serde_json::from_str(&s).ok())
}

fn op_boot() -> Value {
    let Some(b) = boot_preset() else { return json!({"ok": true, "applied": false, "message": "no boot preset"}) };
    let Some(values) = b["values"].as_object().cloned() else { return json!({"ok": false, "error": "boot preset has no values"}) };
    locked(|st| {
        st.preset = valid_preset(&b["preset"]);
        st.source = Some("boot".into());
        let (results, ok) = apply_values(st, &values);
        json!({"ok": ok, "applied": true, "results": results, "error": (!ok).then_some("some settings could not be applied")})
    })
}

fn run() -> Value {
    let req = match read_request(MAX_STDIN_BYTES) { Ok(v) => v, Err(e) => return e };
    let op = req["op"].as_str().unwrap_or("");
    if op == "describe" {
        let mut d = tune::describe();
        d["state"] = summary(&State::load());
        d["boot"] = boot_preset().unwrap_or(Value::Null);
        d["root"] = json!(is_root());
        return d;
    }
    if !is_root() { return json!({"ok": false, "error": format!("'{op}' needs root (run through pkexec)")}); }
    match op {
        "apply" => op_apply(&req),
        "release" => op_release(),
        "restore" | "restore_keys" => op_restore(&req),
        "boost" => op_boost(&req),
        "set_boot" => op_set_boot(&req),
        "boot" => op_boot(),
        _ => json!({"ok": false, "error": "unknown op"}),
    }
}

fn main() {
    unsafe { libc::umask(0o022) };
    std::process::exit(finish(run()));
}
