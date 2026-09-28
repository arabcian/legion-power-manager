//! Root helper for the Optimizations tab, `lpm-gamemode` and the boot service.
//!
//! One JSON object on stdin, one JSON line on stdout:
//!   {"op":"describe"}                                  any user: tunables, live values, state, topology
//!   {"op":"autotune","goal":"gaming"}                  any user: hardware profile + derived preset
//!       (goal: powersave | gaming | throughput | desktop; see lpm_helpers::autotune)
//!   {"op":"profile"}                                   any user: the hardware profile autotune uses
//!   {"op":"apply","values":{k:v,..},"mode":"manual"|"game","preset":"name"}
//!       "replace":true (manual only): first restore every knob this request
//!       does not set, so switching presets never leaves the previous one's
//!       extra keys behind; refused while a game session holds game mode
//!   {"op":"release","owner_pid":N}                     game POST: end one session, restore at 0
//!   {"op":"prune"}                                     end sessions whose launcher died
//!   {"op":"restore"}                                   write every saved original back now
//!   {"op":"restore_keys","keys":[k,..]}                restore only these knobs
//!   {"op":"snapshot"}                                  refresh the readable copy of debugfs values (also done after every root op)
//!   {"op":"boost","nice":-5,"autogroup":true}          renice the process that ran pkexec
//!   {"op":"set_boot","values":{..}|null,"preset":".."} store/clear the boot preset (root-owned file)
//!   {"op":"boot"}                                      apply the boot preset (OpenRC service)
//!   {"op":"guard_reset"}                               resume boot presets paused by lpm-boot-guard
//!
//! Keys are looked up in lpm_helpers::tune::TUNABLES; paths never come from a
//! request. The first write to a concrete file records its original value in
//! /run/legion-power-manager/tune/state.json (tmpfs: a reboot is a full
//! restore). Game mode is reference counted like lutris-game-tune: the first
//! PRE applies, the last POST restores.
//!
//! Locking: every mutating op runs under an exclusive flock on
//! /run/legion-power-manager/tune/lock. The lock is held for the whole
//! operation (state read -> sysfs writes -> state save) so concurrent callers
//! can never interleave their baseline bookkeeping. But a single helper can
//! spend a while inside the kernel (CPU hotplug settle, nvidia-powerd pause,
//! a sysfs write stuck in D-state), so acquisition uses LOCK_NB with a short
//! retry loop and a hard deadline: a stuck helper must not freeze the GUI or
//! the game launcher forever.

use lpm_helpers::tune::{self, TUNABLES};
use lpm_helpers::*;
use serde_json::{json, Map, Value};
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_STDIN_BYTES: usize = 64 * 1024;
const RUN_DIR: &str = "/run/legion-power-manager";
const STATE_DIR: &str = "/run/legion-power-manager/tune";
const STATE_FILE: &str = "/run/legion-power-manager/tune/state.json";
const LOCK_FILE: &str = "/run/legion-power-manager/tune/lock";
const ETC_DIR: &str = "/etc/legion-power-manager";
const BOOT_FILE: &str = "/etc/legion-power-manager/tune-boot.json";
const MAX_BASELINE: usize = 16_384;
const MAX_REFCOUNT: u32 = 64;
/// How long a caller is willing to wait for a busy lock before giving up.
/// Tune ops serialise on this lock; a hung helper (D-state sysfs write,
/// nvidia-powerd pause) must not block every other caller for good.
const LOCK_TIMEOUT: Duration = Duration::from_secs(8);
/// Poll interval while waiting for the lock.
const LOCK_RETRY: Duration = Duration::from_millis(50);

fn is_root() -> bool { unsafe { libc::geteuid() == 0 } }

struct Lock(#[allow(dead_code)] fs::File);

/// Acquires the tune lock with a bounded wait. `flock(LOCK_EX)` without
/// `LOCK_NB` blocks indefinitely; a helper stuck in an uninterruptible
/// sysfs write would then freeze every later caller (GUI, game launcher)
/// along with it. Poll `LOCK_NB` until `LOCK_TIMEOUT`, then fail with a
/// clear "busy" error so the caller can surface it instead of hanging.
fn lock() -> Result<Lock, String> {
    secure_dir(RUN_DIR)?;
    secure_dir(STATE_DIR)?;
    let f = fs::OpenOptions::new().create(true).read(true).write(true).mode(0o600)
    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(LOCK_FILE)
    .map_err(|e| format!("lock: {e}"))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Lock(f));
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // Held by another helper: retry until the deadline.
            Some(libc::EWOULDBLOCK) => {}
            // Interrupted by a signal: retry immediately.
            Some(libc::EINTR) => continue,
            _ => return Err(format!("lock: flock failed: {err}")),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "lock: another tune operation is still running (waited {}s)",
                               LOCK_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(LOCK_RETRY);
    }
}

#[derive(Default)]
struct State {
    /// (key, path, original) in first-write order.
    baseline: Vec<(String, PathBuf, String)>,
    /// Running game sessions: (owner pid, owner start time). The owner is the
    /// process that stays alive for the whole game (lpm-gamemode WRAP, or the
    /// launcher that ran PRE); None = untracked, ended only by a release.
    sessions: Vec<(Option<i32>, Option<u64>)>,
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
        let sessions: Vec<(Option<i32>, Option<u64>)> = match v["sessions"].as_array() {
            Some(a) => a.iter().take(MAX_REFCOUNT as usize).map(|e| {
                let pid = e["pid"].as_i64().and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1);
                (pid, pid.and(e["start"].as_u64()))
            }).collect(),
            // state written by an older helper: untracked sessions
            None => vec![(None, None); v["refcount"].as_u64().unwrap_or(0).min(MAX_REFCOUNT as u64) as usize],
        };
        State {
            baseline,
            sessions,
            preset: v["preset"].as_str().map(str::to_owned),
            source: v["source"].as_str().map(str::to_owned),
        }
    }

    fn save(&self) -> Result<(), String> {
        let v = json!({
            "baseline": self.baseline.iter().map(|(k, p, v)| json!([k, p, v])).collect::<Vec<_>>(),
                      "refcount": self.refcount(), "preset": self.preset, "source": self.source,
                      "sessions": self.sessions.iter().map(|(p, t)| json!({"pid": p, "start": t})).collect::<Vec<_>>(),
        });
        write_root_file(STATE_FILE, &serde_json::to_vec_pretty(&v).unwrap())
    }

    fn has(&self, p: &Path) -> bool { self.baseline.iter().any(|(_, q, _)| q == p) }

    fn refcount(&self) -> u32 { self.sessions.len() as u32 }

    /// Drops sessions whose owner has exited (launcher killed, crash, POST
    /// hook never ran). Returns how many were dropped.
    fn prune(&mut self) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|&(p, t)| session_alive(p.map(i64::from), t));
        before - self.sessions.len()
    }
}

fn summary(st: &State) -> Value {
    let mut keys: Vec<&str> = Vec::new();
    for (k, _, _) in &st.baseline { if !keys.contains(&k.as_str()) { keys.push(k); } }
    json!({"active": !st.baseline.is_empty(), "refcount": st.refcount(), "preset": st.preset,
        "source": st.source, "saved_files": st.baseline.len(), "keys": keys})
}

/// Applies `values` in table order. Returns per-key results and all-ok.
fn apply_values(st: &mut State, values: &Map<String, Value>) -> (Vec<Value>, bool) {
    let mut results = Vec::new();
    let mut all_ok = true;
    for k in values.keys() {
        if tune::find(k).is_none() && !tune::RETIRED_KEYS.contains(&k.as_str()) {
            results.push(json!({"key": k, "ok": false, "error": "unknown key"}));
            all_ok = false;
        }
    }
    // Table order, not request order: pstate mode first, hot-plug last.
    let mut first = true;
    for t in TUNABLES {
        let Some(raw) = values.get(t.key) else { continue };
        if !std::mem::take(&mut first) { std::thread::sleep(tune::KNOB_GAP); }
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
        // Label of the CCD being parked, taken while it is still resolvable.
        let park_label = (t.key == "cpu.ccd_park")
        .then(|| tune::options(t).into_iter().find(|(k, _)| *k == v).map(|(_, l)| l)).flatten();
        let plan = match tune::plan(t, &v) {
            Ok(p) => p,
            Err(e) => { results.push(json!({"key": t.key, "ok": false, "error": e})); all_ok = false; continue; }
        };
        let park_cpus: Vec<usize> = plan.iter().filter(|(_, d)| d == "0")
        .filter_map(|(f, _)| f.parent()?.file_name()?.to_str()?.strip_prefix("cpu")?.parse().ok()).collect();
        let (mut written, mut refused, mut errs) = (0, 0, Vec::new());
        // Pass 1: read every original, record the fresh ones. They are
        // persisted in ONE state save *before* any write, so a crash or kill
        // mid-batch still leaves every touched file restorable — without
        // re-serialising the whole state once per file (IRQ affinity and the
        // PCI latency row touch 100+ files each).
        let mut todo: Vec<(PathBuf, String, bool)> = Vec::new();
        for (f, data) in plan {
            let Some(orig) = tune::baseline_value(t, &f) else {
                if tune::best_effort(t) { refused += 1; } else { errs.push(format!("{}: unreadable", f.display())); }
                continue;
            };
            if tune::same_value(t, &orig, &data) { continue; }
            let fresh = !st.has(&f) && !todo.iter().any(|(q, _, _)| *q == f);
            if fresh {
                if st.baseline.len() >= MAX_BASELINE { errs.push("baseline full".into()); break; }
                st.baseline.push((t.key.to_owned(), f.clone(), orig));
            }
            todo.push((f, data, fresh));
        }
        let fresh_n = todo.iter().filter(|x| x.2).count();
        if fresh_n > 0 {
            if let Err(e) = st.save() {
                st.baseline.truncate(st.baseline.len() - fresh_n);
                errs.push(format!("state not saved, writes skipped: {e}"));
                todo.clear();
            }
        }
        // Pass 2: write. A failed write changed nothing, so its fresh
        // baseline entry is dropped again (saved with the rest at the end).
        let mut unchanged: Vec<PathBuf> = Vec::new();
        let mut fw_owned = false;
        // nvidia-powerd must be gone before any CPU goes offline (see tune::powerd_pause).
        let mut powerd_note = None;
        if todo.iter().any(|(_, d, _)| tune::offlines_cpus(t.key, d)) {
            match tune::powerd_pause() {
                Ok(true) => powerd_note = Some("nvidia-powerd stopped while CPUs are offline"),
                Ok(false) => {}
                Err(e) => {
                    errs.push(format!("{e}; CPUs left online"));
                    unchanged.extend(todo.drain(..).filter(|x| x.2).map(|x| x.0));
                }
            }
        }
        for (f, data, fresh) in todo {
            match tune::write_value(t, &f, &data) {
                Ok(()) => written += 1,
                Err(e) => {
                    if fresh { unchanged.push(f); }
                    if tune::firmware_owned(t, &e) { fw_owned = true; }
                    else if tune::best_effort(t) { refused += 1; } else { errs.push(e); }
                }
            }
        }
        if !unchanged.is_empty() {
            st.baseline.retain(|(k, q, _)| !(k == t.key && unchanged.contains(q)));
        }
        if t.key == "cpu.ccd_park" && errs.is_empty() {
            if let Err(e) = tune::record_ccd_park(&v, park_label.as_deref(), &park_cpus) { errs.push(format!("park record: {e}")); }
        }
        if tune::is_hotplug(t.key) {
            // Unpark / SMT on, or an offline write that failed: bring powerd back.
            match tune::powerd_resume() {
                Ok(true) => powerd_note = Some("nvidia-powerd restarted"),
                Ok(false) => {}
                Err(e) => errs.push(e),
            }
        }
        if fw_owned && errs.is_empty() && written == 0 {
            // Not changeable on this machine: same as an absent feature, not a failure.
            results.push(json!({"key": t.key, "ok": true, "skipped": "firmware owns PCIe ASPM (FADT / _OSC); the policy cannot be changed from Linux"}));
            continue;
        }
        let mut r = json!({"key": t.key, "value": v, "written": written});
        if refused > 0 { r["refused"] = json!(refused); }
        if let Some(n) = powerd_note { r["note"] = json!(n); }
        if errs.is_empty() {
            r["ok"] = json!(true);
        } else {
            all_ok = false;
            errs.truncate(4);
            r["ok"] = json!(false);
            r["error"] = json!(errs.join("; "));
        }
        results.push(r);
        if tune::is_hotplug(t.key) && written > 0 { std::thread::sleep(tune::HOTPLUG_SETTLE); tune::invalidate_topology(); }
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
    // Entries whose original could not be written back stay in the baseline:
    // dropping them would lose the only record of the original value while the
    // knob is still changed. A later restore (GUI, POST, service stop) retries.
    let mut failed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut hotplug_done = false;
    for &i in &order {
        let (key, f, orig) = &st.baseline[i];
        // CPUs come back first: give them time before their cpufreq files are written.
        if !hotplug_done && !tune::is_hotplug(key) {
            hotplug_done = true;
            if order.first().map_or(false, |&j| tune::is_hotplug(&st.baseline[j].0)) {
                std::thread::sleep(tune::HOTPLUG_SETTLE);
                tune::invalidate_topology();
            }
        }
        // Restoring an "off"/"0" original takes CPUs offline too.
        if tune::offlines_cpus(key, orig) {
            if let Err(e) = tune::powerd_pause() { failed.insert(i); errs.push(json!({"key": key, "error": e})); continue; }
        }
        let res = match tune::find(key) { Some(t) => tune::write_value(t, f, orig), None => tune::write_checked(f, orig) };
        match res {
            Ok(()) => n += 1,
            // Per-policy files vanish when the pstate mode is restored first; not an error.
            Err(_) if !f.exists() => {}
            Err(_) if tune::find(key).map_or(false, tune::best_effort) => {}
            Err(e) => { failed.insert(i); errs.push(json!({"key": key, "error": e})); }
        }
    }
    if let Err(e) = tune::powerd_resume() { errs.push(json!({"key": "nvidia-powerd", "error": e})); }
    // Game mode is over: give the game's CPUs back to the whole system.
    if only.is_none() {
        if let Err(e) = lpm_helpers::isolate::teardown() { errs.push(json!({"key": "game partition", "error": e})); }
    }
    let drop: std::collections::HashSet<usize> = order.into_iter().filter(|i| !failed.contains(i)).collect();
    let mut i = 0;
    st.baseline.retain(|_| { let keep = !drop.contains(&i); i += 1; keep });
    // A full restore ends game mode even if some originals could not be
    // written back (those stay recorded for the next restore attempt).
    if only.is_none() { st.sessions.clear(); }
    if st.baseline.is_empty() { st.sessions.clear(); st.preset = None; st.source = None; }
    json!({"restored": n, "errors": errs})
}

fn locked<F: FnOnce(&mut State) -> Value>(f: F) -> Value {
    let _l = match lock() { Ok(l) => l, Err(e) => return json!({"ok": false, "error": e}) };
    let mut st = State::load();
    // A game whose launcher was killed never sends POST: end its session here,
    // and when it was the last one, restore exactly as the last POST would.
    let mut stale = Value::Null;
    if st.prune() > 0 && st.sessions.is_empty() && st.source.as_deref() == Some("game") {
        stale = restore_entries(&mut st, None);
    }
    let mut out = f(&mut st);
    if !stale.is_null() && out.is_object() { out["stale_game_released"] = stale; }
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
    let replace = req["replace"].as_bool().unwrap_or(false);
    if replace && mode != "manual" { return json!({"ok": false, "error": "replace is only valid in manual mode"}); }
    let owner = if mode == "game" { session_owner(req) } else { (None, None) };
    locked(|st| {
        let mut restored = Value::Null;
        if replace {
            // A scene switch (e.g. AC → battery) must not yank a running game's tuning.
            if st.refcount() > 0 {
                return json!({"ok": false, "applied": false, "game_active": true,
                    "error": format!("game mode is active ({} session(s)); tuning left unchanged", st.refcount())});
            }
            let mut stale: Vec<String> = Vec::new();
            for (k, _, _) in &st.baseline {
                if !values.contains_key(k) && !stale.contains(k) { stale.push(k.clone()); }
            }
            if !stale.is_empty() { restored = restore_entries(st, Some(&stale)); }
            st.preset = preset.clone();
        }
        if mode == "game" {
            if st.refcount() >= MAX_REFCOUNT { return json!({"ok": false, "error": "too many concurrent game sessions"}); }
            st.sessions.push(owner);
            if st.refcount() > 1 {
                return json!({"ok": true, "applied": false,
                    "message": format!("game mode already active ({} games running)", st.refcount())});
            }
        }
        if !replace { st.preset = preset.clone().or(st.preset.take()); }
        st.source = Some(mode.to_owned());
        let (results, mut ok) = apply_values(st, values);
        if restored["errors"].as_array().map_or(false, |a| !a.is_empty()) { ok = false; }
        if values.is_empty() && st.baseline.is_empty() { st.source = None; }
        json!({"ok": ok, "applied": true, "results": results, "restored": restored,
            "error": (!ok).then_some("some settings could not be applied")})
    })
}

fn op_release(req: &Value) -> Value {
    let owner = session_owner(req);
    locked(|st| {
        // No game session (PRE failed / already released / pruned): a POST must
        // not restore manual or boot tuning that game mode never owned.
        if st.refcount() == 0 {
            return json!({"ok": true, "restored": false, "message": "no game session active"});
        }
        if st.refcount() > 1 {
            // Drop the caller's own session if we can identify it, else the
            // first untracked one. The previous `unwrap_or(0)` fallback
            // silently removed the *first live session* whenever the owner
            // PID was stale/recycled (session_owner returns (None, None) in
            // that case), leaving a real game's tuning active with no
            // session left to release it.
            let idx = st.sessions.iter().position(|s| owner.0.is_some() && *s == owner)
            .or_else(|| st.sessions.iter().position(|s| s.0.is_none()));
            return match idx {
                Some(i) => {
                    st.sessions.remove(i);
                    json!({"ok": true, "restored": false,
                        "message": format!("{} game(s) still running", st.refcount())})
                }
                None => json!({"ok": false, "error": "no matching game session to release"}),
            };
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

/// Owner of a game session: `owner_pid` from the request (lpm-gamemode sends
/// the process that lives as long as the game), accepted only if it belongs to
/// the user pkexec authenticated. Anything else is an untracked session, which
/// behaves exactly like the old plain refcount.
fn session_owner(req: &Value) -> (Option<i32>, Option<u64>) {
    let Some(pid) = req["owner_pid"].as_i64().and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1) else { return (None, None) };
    let caller = match std::env::var("PKEXEC_UID") {
        Ok(v) => match v.parse::<u32>() { Ok(u) => Some(u), Err(_) => return (None, None) },
        Err(_) => None, // run by root directly (tests, scripts)
    };
    if let Some(uid) = caller { if proc_ruid(pid) != Some(uid) { return (None, None); } }
    match proc_start_time(pid) { Some(t) => (Some(pid), Some(t)), None => (None, None) }
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
    // While `ppid` is still our parent its PID cannot be recycled (a dead
    // parent reparents us before its PID is freed), so re-checking getppid()
    // right before each privileged step pins the target: a caller that exits
    // mid-request can never make root renice an unrelated process that
    // happened to reuse the number.
    let still_parent = || unsafe { libc::getppid() } == ppid;
    if ppid <= 1 || caller == 0 || proc_ruid(ppid) != Some(caller) || !still_parent() {
        return json!({"ok": false, "error": "caller process does not belong to the authenticated user"});
    }
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, ppid as libc::id_t, nice as libc::c_int) } != 0 {
        return json!({"ok": false, "error": format!("setpriority: {}", std::io::Error::last_os_error())});
    }
    let ag = req["autogroup"].as_bool().unwrap_or(false).then(|| {
        still_parent() && fs::OpenOptions::new().write(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(format!("/proc/{ppid}/autogroup"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, nice.to_string().as_bytes())).is_ok()
    });
    json!({"ok": true, "pid": ppid, "nice": nice, "autogroup": ag})
}

/// Moves the process that ran pkexec (lpm-gamemode RUN/WRAP, which then
/// starts the game) into the game CPU partition: every CPU except the
/// soft-parked CCD `park`. Only while a game session holds game mode, so the
/// release that ends it also removes the partition.
fn op_isolate_join(req: &Value) -> Value {
    let Some(park) = req["park"].as_str().filter(|s| !s.is_empty() && s.len() <= 16) else {
        return json!({"ok": false, "error": "park must name a CCD"});
    };
    let ppid = unsafe { libc::getppid() };
    let Some(caller) = std::env::var("PKEXEC_UID").ok().and_then(|v| v.parse::<u32>().ok()) else {
        return json!({"ok": false, "error": "isolate_join is only available through pkexec"});
    };
    if ppid <= 1 || caller == 0 || proc_ruid(ppid) != Some(caller) {
        return json!({"ok": false, "error": "caller process does not belong to the authenticated user"});
    }
    locked(|st| {
        if st.source.as_deref() != Some("game") || st.refcount() == 0 {
            return json!({"ok": false, "error": "game mode is not active"});
        }
        let groups = tune::ccx_groups();
        let Some(pg) = tune::resolve_ccd(&groups, park) else {
            return json!({"ok": false, "error": format!("'{park}' does not resolve to an online CCD")});
        };
        let mut cpus: Vec<usize> = groups.iter().flat_map(|g| g.cpus.iter().copied()).filter(|c| !pg.cpus.contains(c)).collect();
        cpus.sort_unstable();
        if let Err(e) = lpm_helpers::isolate::setup(&cpus) { return json!({"ok": false, "error": e}); }
        // Same PID-reuse pin as boost: only while the caller is still our parent.
        if unsafe { libc::getppid() } != ppid { return json!({"ok": false, "error": "caller exited"}); }
        match lpm_helpers::isolate::join(ppid) {
            Ok(()) => json!({"ok": true, "pid": ppid, "cpus": tune::fmt_cpu_list(&cpus)}),
           Err(e) => json!({"ok": false, "error": e}),
        }
    })
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
                    None if tune::RETIRED_KEYS.contains(&k.as_str()) => {}
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
        d["isolation"] = lpm_helpers::isolate::status();
        return d;
    }
    // Read-only ops any user may run; health needs root only when the kernel log is restricted.
    match op {
        "health" => return lpm_helpers::health::scan(req["since"].as_u64().unwrap_or(0)),
        "klog" => return lpm_helpers::health::klog(req["since"].as_u64().unwrap_or(0)),
        "nvreg_describe" => return lpm_helpers::nvreg::describe(),
        "autotune" => return match req["goal"].as_str().and_then(lpm_helpers::autotune::Goal::parse) {
            Some(g) => lpm_helpers::autotune::autotune(g),
            None => json!({"ok": false, "error": "goal must be one of powersave, gaming, throughput, desktop"}),
        },
        "profile" => {
            let p = lpm_helpers::autotune::Profile::gather();
            return json!({"ok": true, "profile": p.to_json(), "summary": p.summary()});
        }
        _ => {}
    }
    if !is_root() { return json!({"ok": false, "error": format!("'{op}' needs root (run through pkexec)")}); }
    let out = match op {
        "snapshot" => json!({"ok": true}),
        "apply" => op_apply(&req),
        "release" => op_release(&req),
        // Ends game sessions whose launcher died (pruning runs in every locked op).
        "prune" => locked(|_| json!({"ok": true})),
        "restore" | "restore_keys" => op_restore(&req),
        "boost" => op_boost(&req),
        "isolate_join" => op_isolate_join(&req),
        "tool" => lpm_helpers::tools::run(req["tool"].as_str().unwrap_or("")),
        "nvreg_set" => match req["values"].as_object() {
            Some(v) => match lpm_helpers::nvreg::set(v) {
                Ok(n) => json!({"ok": true, "written": n, "file": lpm_helpers::nvreg::LPM_FILE}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            None => json!({"ok": false, "error": "values must be an object"}),
        },
        "set_boot" => op_set_boot(&req),
        "boot" => op_boot(),
        "guard_reset" => match lpm_helpers::bootguard::reset() {
            Ok(v) => json!({"ok": true, "guard": v}),
            Err(e) => json!({"ok": false, "error": e}),
        },
        _ => json!({"ok": false, "error": "unknown op"}),
    };
    // Unprivileged describe reads the debugfs values from this copy (debugfs stays root-only).
    if matches!(op, "snapshot" | "apply" | "release" | "restore" | "restore_keys" | "boot" | "prune") { tune::write_debugfs_snapshot(); }
    out
}

fn main() {
    init();
    std::process::exit(finish(run()));
}
