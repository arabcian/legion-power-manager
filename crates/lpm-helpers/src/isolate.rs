//! Game CPU partition: a cgroup v2 cpuset partition ("root") that gives the
//! game's CCD to the game alone, without CPU hot-plug.
//!
//! A partition root takes its CPUs out of the parent's effective set, so every
//! other cgroup (desktop, services, user sessions) is confined to the rest of
//! the chip, while all CPUs stay online: Wine/Proton and nvidia-powerd see an
//! unchanged topology. "root" (not "isolated") keeps load balancing inside the
//! partition, which a multi-threaded game needs.
//!
//! Needs the unified hierarchy at /sys/fs/cgroup with the cpuset controller
//! (OpenRC: rc_cgroup_mode="unified"). Created by tune-helper for a running
//! game session and removed by the full restore that ends game mode.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const ROOT: &str = "/sys/fs/cgroup";
pub const CG: &str = "/sys/fs/cgroup/lpm-game";
const STATE: &str = "/run/legion-power-manager/tune/isolate.json";

fn read(p: &str) -> Option<String> { std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned()) }

fn write(p: &str, v: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(p)
        .and_then(|mut f| f.write_all(v.as_bytes()))
        .map_err(|e| format!("{p}: {e}"))
}

fn cg(file: &str) -> String { format!("{CG}/{file}") }

/// Why the partition cannot be used here, if it cannot.
pub fn unsupported() -> Option<String> {
    match read(&format!("{ROOT}/cgroup.controllers")) {
        None => Some("cgroup v2 is not mounted at /sys/fs/cgroup (OpenRC: rc_cgroup_mode=\"unified\")".into()),
        Some(c) if !c.split_whitespace().any(|x| x == "cpuset") => Some("the cpuset controller is not available in cgroup v2".into()),
        Some(_) => None,
    }
}

/// Current partition (readable by any user).
pub fn status() -> Value {
    if !Path::new(CG).is_dir() {
        return json!({"active": false, "unsupported": unsupported()});
    }
    json!({
        "active": true,
        "cpus": read(&cg("cpuset.cpus.effective")).unwrap_or_default(),
        "partition": read(&cg("cpuset.cpus.partition")).unwrap_or_default(),
        "procs": read(&cg("cgroup.procs")).map_or(0, |s| s.lines().count()),
    })
}

fn load_state() -> Value {
    crate::read_root_file(STATE, 64 * 1024).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}))
}

fn save_state(v: &Value) -> Result<(), String> { crate::write_root_file(STATE, v.to_string().as_bytes()) }

/// Creates (or keeps) the partition over `cpus`.
pub fn setup(cpus: &[usize]) -> Result<(), String> {
    if let Some(e) = unsupported() { return Err(e); }
    if cpus.is_empty() { return Err("no CPUs for the game partition".into()); }
    let list = crate::tune::fmt_cpu_list(cpus);
    if Path::new(CG).is_dir() {
        let part = read(&cg("cpuset.cpus.partition")).unwrap_or_default();
        if part == "root" && read(&cg("cpuset.cpus")).map(|c| crate::tune::cpu_list(&c)) == Some(cpus.to_vec()) { return Ok(()); }
    }
    let mut st = load_state();
    if st.get("added").is_none() {
        let sub = read(&format!("{ROOT}/cgroup.subtree_control")).unwrap_or_default();
        let added = !sub.split_whitespace().any(|c| c == "cpuset");
        st["added"] = json!(added);
        save_state(&st)?;  // recorded before the change, so a crash still undoes it
        if added { write(&format!("{ROOT}/cgroup.subtree_control"), "+cpuset")?; }
    }
    match std::fs::create_dir(CG) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{CG}: {e}")),
    }
    let r = write(&cg("cpuset.cpus"), &list).and_then(|_| write(&cg("cpuset.cpus.partition"), "root"));
    let part = read(&cg("cpuset.cpus.partition")).unwrap_or_default();
    if r.is_err() || part != "root" {
        let why = r.err().unwrap_or_else(|| format!("kernel reports '{part}'"));
        let _ = teardown();
        return Err(format!("partition over {list} refused: {why}"));
    }
    Ok(())
}

/// cgroup of `pid` as a directory under /sys/fs/cgroup.
fn cgroup_of(pid: i32) -> Option<PathBuf> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = s.lines().find_map(|l| l.strip_prefix("0::"))?;
    if rel.split('/').any(|c| c == "..") { return None; }
    let p = PathBuf::from(format!("{ROOT}{rel}"));
    p.join("cgroup.procs").is_file().then_some(p)
}

/// Moves `pid` (and its future children) into the partition.
pub fn join(pid: i32) -> Result<(), String> {
    if !Path::new(CG).is_dir() { return Err("game partition not set up".into()); }
    let mut st = load_state();
    if st.get("origin").is_none() {
        if let Some(o) = cgroup_of(pid).filter(|o| o != Path::new(CG)) {
            st["origin"] = json!(o.display().to_string());
            save_state(&st)?;
        }
    }
    write(&cg("cgroup.procs"), &pid.to_string())
}

/// Moves everything left in the partition back and removes it. Ok(true) = removed.
pub fn teardown() -> Result<bool, String> {
    let st = load_state();
    let existed = Path::new(CG).is_dir();
    if existed {
        let origin = st["origin"].as_str().map(PathBuf::from)
            .filter(|o| o.starts_with(ROOT) && o.as_path() != Path::new(CG) && o.join("cgroup.procs").is_file())
            .unwrap_or_else(|| PathBuf::from(ROOT));
        let dest = origin.join("cgroup.procs").display().to_string();
        for _ in 0..20 {
            let pids = read(&cg("cgroup.procs")).unwrap_or_default();
            if pids.is_empty() { break; }
            for pid in pids.lines() {
                if write(&dest, pid).is_err() { let _ = write(&format!("{ROOT}/cgroup.procs"), pid); }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = write(&cg("cpuset.cpus.partition"), "member");
        let mut removed = false;
        for _ in 0..20 {
            if std::fs::remove_dir(CG).is_ok() { removed = true; break; }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !removed { return Err(format!("{CG} could not be removed (tasks still exiting?)")); }
    }
    if st["added"] == true { let _ = write(&format!("{ROOT}/cgroup.subtree_control"), "-cpuset"); }
    let _ = std::fs::remove_file(STATE);
    Ok(existed)
}
