//! Root helper for the Home tab: set one platform profile on one handler.
//! Port of legion_profile_helper.py -- same JSON protocol:
//!   stdin:  {"profile": "<name>", "handler": "platform-profile-N" | null}
//!   stdout: {"ok": true, "profile": .., "effective": ..} | {"ok": false, "error": ..}
//!
//! Writes the per-handler class interface because the legacy
//! /sys/firmware/acpi/platform_profile store rejects "custom" (-EINVAL);
//! lenovo-wmi-gamezone registers it as a hidden choice on the class device.

use lpm_helpers::*;
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};

const MAX_STDIN_BYTES: usize = 4 * 1024;
const PPROF_CLASS_DIR: &str = "/sys/class/platform-profile";
const LEGACY_PROFILE_PATH: &str = "/sys/firmware/acpi/platform_profile";

/// profile_names[] from drivers/acpi/platform_profile.c
const VALID_PROFILES: &[&str] = &[
    "low-power", "cool", "quiet", "balanced",
    "balanced-performance", "performance", "max-power", "custom",
];

fn valid_node(node: &str) -> bool {
    node.strip_prefix("platform-profile-")
        .map(|n| !n.is_empty() && n.len() <= 6 && n.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

fn explain(e: &io::Error, profile: &str, target: &Path) -> String {
    let t = target.display();
    match e.raw_os_error() {
        Some(libc::EOPNOTSUPP) => format!(
            "the driver behind {t} does not accept '{profile}'. On Lenovo models Custom \
             mode is exposed only by the lenovo-wmi-gamezone driver; check that it is loaded."),
        Some(libc::EINVAL) => format!(
            "'{profile}' was rejected as invalid by {t}. Writing 'custom' to the legacy \
             /sys/firmware/acpi/platform_profile always fails this way -- it has to go to \
             the per-handler class device instead."),
        Some(libc::EACCES) | Some(libc::EPERM) =>
            format!("permission denied writing {t} (helper is not root?)"),
        Some(libc::ENODEV) =>
            format!("{t} disappeared; the platform driver may have unbound."),
        _ => format!("write failed: {e}"),
    }
}

fn set_profile(node: Option<&str>, profile: &str) -> Value {
    if !VALID_PROFILES.contains(&profile) {
        return json!({"ok": false, "error": format!("'{profile}' is not a kernel profile name")});
    }

    let target: PathBuf = match node {
        Some(node) => {
            if !valid_node(node) {
                return json!({"ok": false, "error": "handler name has an unexpected shape"});
            }
            // Path rebuilt from a fixed prefix + a digits-only name.
            let target = Path::new(PPROF_CLASS_DIR).join(node).join("profile");
            if !target.exists() {
                return json!({"ok": false, "error": format!("no such platform-profile handler: {node}")});
            }
            match canonical_in_sysfs(&target) {
                Some(real) if real.file_name().map_or(false, |n| n == "profile") => real,
                _ => return json!({"ok": false, "error": "handler path resolves outside sysfs"}),
            }
        }
        None => {
            if profile == "custom" {
                return json!({"ok": false, "error":
                    "this kernel has no /sys/class/platform-profile interface, and the legacy \
                     platform_profile file cannot be set to 'custom'. Custom mode needs kernel \
                     6.14+ with the lenovo-wmi-gamezone driver loaded."});
            }
            PathBuf::from(LEGACY_PROFILE_PATH)
        }
    };

    if let Err(e) = sysfs_write(&target, profile.as_bytes()) {
        return json!({"ok": false, "error": explain(&e, profile, &target)});
    }

    // Firmware may land elsewhere (e.g. mode unavailable on battery):
    // report what is actually in effect.
    let effective = read_trimmed(&target).unwrap_or_else(|_| profile.to_owned());
    json!({"ok": true, "profile": profile, "effective": effective})
}

fn run() -> Value {
    let req = match read_request(MAX_STDIN_BYTES) { Ok(v) => v, Err(e) => return e };
    let Some(obj) = req.as_object() else {
        return json!({"ok": false, "error": "payload must be a JSON object"});
    };
    let profile = match obj.get("profile").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p,
        _ => return json!({"ok": false, "error": "'profile' must be a non-empty string"}),
    };
    let node = match obj.get("handler") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return json!({"ok": false, "error": "'handler' must be a string or absent"}),
    };
    set_profile(node, profile)
}

fn main() {
    std::process::exit(finish(run()));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn node_shape() {
        assert!(valid_node("platform-profile-0"));
        assert!(valid_node("platform-profile-12"));
        assert!(!valid_node("platform-profile-"));
        assert!(!valid_node("platform-profile-1/../x"));
        assert!(!valid_node("../platform-profile-1"));
        assert!(!valid_node("platform-profile-1\n"));
    }
    #[test]
    fn rejects_unknown_profile() {
        let r = set_profile(None, "turbo");
        assert_eq!(r["ok"], false);
    }
    #[test]
    fn legacy_custom_refused() {
        let r = set_profile(None, "custom");
        assert!(r["error"].as_str().unwrap().contains("6.14+"));
    }
}
