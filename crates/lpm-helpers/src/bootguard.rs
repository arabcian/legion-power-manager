//! Boot guard: pauses the boot-time presets after a boot that ended badly.
//!
//! An undervolt / curve / tuning preset that is too aggressive can crash the
//! machine every time it is applied at boot — a crash loop the user can only
//! escape from a rescue shell. The guard gives every boot a short "armed"
//! window: `arm` at boot, `disarm` after WINDOW of stable uptime or on a clean
//! shutdown. A boot that never disarmed (panic, hang, forced power-off while
//! the presets were fresh) trips the guard, and from then on the preset
//! services skip until the user resumes them in the app — they are not
//! re-enabled automatically, so there is no crash / skip / crash cycle.
//!
//! State (root-owned, world-readable so the GUI can show it):
//!   /var/lib/legion-power-manager/boot-guard.json
//!   {"boot_id", "state": "armed"|"ok"|"paused", "tripped": bool, "reason", "tripped_at",
//!    "clean_shutdown": boot id of the last boot that shut down cleanly}

use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub const GUARD_DIR: &str = "/var/lib/legion-power-manager";
pub const GUARD_FILE: &str = "/var/lib/legion-power-manager/boot-guard.json";
/// Stable uptime after which the presets are considered good for this boot.
pub const WINDOW_SECS: u64 = 180;

pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_owned()).unwrap_or_default()
}

pub fn read() -> Value {
    crate::read_root_file(GUARD_FILE, 16 * 1024)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write(v: &Value) -> Result<(), String> {
    crate::secure_dir(GUARD_DIR)?;
    crate::write_root_file(GUARD_FILE, &serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?)
}

/// A previous boot left the guard armed (it never reached its stable window).
fn previous_boot_died(v: &Value, current: &str) -> bool {
    v["state"] == "armed" && v["boot_id"].as_str().map_or(false, |b| b != current)
}

/// Presets must be skipped: already tripped, or the previous boot died armed
/// (evaluated here too, so a preset service that starts before `arm` has run
/// still decides correctly).
pub fn should_skip() -> Option<String> {
    let v = read();
    if v["tripped"] == true {
        return Some(v["reason"].as_str().unwrap_or("boot presets are paused").to_owned());
    }
    if previous_boot_died(&v, &boot_id()) {
        return Some("the previous boot did not stay up after the presets were applied".into());
    }
    None
}

fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

/// Start of a boot: trips the guard if the previous boot died armed, else arms
/// it for this boot. Idempotent within one boot.
pub fn arm() -> Result<Value, String> {
    let mut v = read();
    let cur = boot_id();
    if cur.is_empty() { return Err("no boot_id".into()); }
    if v["boot_id"] == json!(cur) { return Ok(v); }
    if previous_boot_died(&v, &cur) {
        v["tripped"] = json!(true);
        v["tripped_at"] = json!(now());
        v["reason"] = json!(format!(
            "the previous boot ended within {} minutes of applying the boot presets without a clean shutdown \
             (crash, hang or forced power-off) — an unstable undervolt or curve is the usual cause",
            WINDOW_SECS / 60));
    }
    v["boot_id"] = json!(cur);
    v["state"] = json!(if v["tripped"] == true { "paused" } else { "armed" });
    write(&v)?;
    Ok(v)
}

/// This boot survived its window (or is shutting down cleanly).
pub fn disarm() -> Result<(), String> {
    let mut v = read();
    if v["boot_id"] == json!(boot_id()) && v["state"] == "armed" {
        v["state"] = json!("ok");
        write(&v)?;
    }
    Ok(())
}

/// Clean shutdown / reboot (OpenRC `stop`, systemd SIGTERM to `watch`):
/// disarms if still armed and records this boot id as having ended cleanly.
/// The GUI's login guard reads `clean_shutdown` so that a normal reboot soon
/// after login is not mistaken for a crash.
pub fn shutdown() -> Result<(), String> {
    let mut v = read();
    let cur = boot_id();
    if cur.is_empty() { return Err("no boot_id".into()); }
    if v["boot_id"] == json!(cur) && v["state"] == "armed" { v["state"] = json!("ok"); }
    if v["boot_id"].is_null() { v["boot_id"] = json!(cur); }
    v["clean_shutdown"] = json!(cur);
    write(&v)
}

/// User resumed the presets (GUI "Resume" / `lpm-boot-guard reset`).
/// They apply again from the next boot on (or right away via each tab).
pub fn reset() -> Result<Value, String> {
    let mut v = read();
    v["tripped"] = json!(false);
    v["reason"] = Value::Null;
    v["tripped_at"] = Value::Null;
    if v["state"] == "paused" { v["state"] = json!("ok"); }
    if v["boot_id"].is_null() { v["boot_id"] = json!(boot_id()); }
    write(&v)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn died_detection() {
        assert!(previous_boot_died(&json!({"state": "armed", "boot_id": "a"}), "b"));
        assert!(!previous_boot_died(&json!({"state": "armed", "boot_id": "b"}), "b"));  // same boot, still in window
        assert!(!previous_boot_died(&json!({"state": "ok", "boot_id": "a"}), "b"));
        assert!(!previous_boot_died(&json!({}), "b"));
    }
}
