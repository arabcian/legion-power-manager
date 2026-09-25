//! Root helper for the Home tab: set one platform profile on one handler.
//! Port of legion_profile_helper.py -- same JSON protocol:
//!   stdin:  {"profile": "<name>", "handler": "platform-profile-N" | null}
//!   stdout: {"ok": true, "profile": .., "effective": ..} | {"ok": false, "error": ..}
//!
//! Home-tab device controls (fixed, whitelisted sysfs targets only):
//!   stdin:  {"device": "<key>", "value": "<v>"}
//!   keys:   charge_type            BAT*/charge_types (value must be one it offers)
//!           fn_lock | camera_power | usb_charging   ideapad_acpi VPC*/<key>, "0"/"1"
//!           fan<N>_target          lenovo_wmi_other hwmon, 0 (= auto) .. fanN_max
//!           fan_fullspeed          "1"/"0": the EC's Full Speed flag (separate from
//!                                  fanN_target; survives reboots, e.g. set from
//!                                  Windows). Backends, first found wins:
//!                                  lenovo_wmi_other pwm1_enable (0 = full, 2 = auto),
//!                                  legion_laptop PNP0C09:*/fan_fullspeed (1/0),
//!                                  Lenovo WMAE feature 0x04020000 (EC FNST) via acpi_call
//!   stdin:  {"wmae_toggle": "get"} | {"wmae_toggle": "set", "key": k, "on": bool}
//!           keys: instant_boot_ac, instant_boot_usbpd, fnq_custom (Lenovo WMAE)
//!   stdin:  {"fan_fullspeed": "get"}
//!   stdout: {"ok": true, "on": bool, "backend": "pwm1_enable" | "fan_fullspeed" | "wmae"}
//!   stdout: {"ok": true, "device": .., "effective": ..} | {"ok": false, "error": ..}
//!
//! Custom-mode fan table (Legion 16AFR10H, via acpi_call; see lpm_helpers::fan_table):
//!   stdin:  {"fan_table": "get"} | {"fan_table": "set", "levels": [10 x 1..10, non-decreasing]}
//!   stdout: {"ok": true, "levels": [..], "fans": [{fan, sensor, rpm[10], temp[10]}]}
//!
//! Memory SPD (read-only, any DDR5 machine; see lpm_helpers::memory_spd):
//!   stdin:  {"memory": "spd"} | {"memory": "all"} (SPD + live UMC timings via ryzen_smu)
//!   stdin:  {"memory": "aod_get"}  BIOS DRAM timing overrides, read-only here (16AFR10H only);
//!           writes (aod_set / aod_restore) are in legion-firmware-helper
//!   stdout: {"ok": true, "modules": [{slot, part, speed_mts, tAA{ns,clk}, ...}]}
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

// ── device controls ─────────────────────────────────────────────────────────

const PSU_DIR: &str = "/sys/class/power_supply";
const IDEAPAD_DIR: &str = "/sys/bus/platform/drivers/ideapad_acpi";
const HWMON_DIR: &str = "/sys/class/hwmon";
const IDEAPAD_KEYS: &[&str] = &["fn_lock", "camera_power", "usb_charging"];

fn sorted_dir(dir: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// "Fast Standard [Long_Life]" -> ["Fast", "Standard", "Long_Life"]
fn bracket_options(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(|w| w.trim_matches(|c| c == '[' || c == ']').to_owned()).filter(|w| !w.is_empty()).collect()
}

fn fan_index(key: &str) -> Option<u32> {
    let n = key.strip_prefix("fan")?.strip_suffix("_target")?;
    (!n.is_empty() && n.len() <= 2 && n.bytes().all(|b| b.is_ascii_digit())).then(|| n.parse().ok()).flatten()
}

const LEGION_DRV_DIR: &str = "/sys/bus/platform/drivers/legion";

/// Full Speed backend: (file, value to write for on, value for off).
fn fullspeed_target() -> Option<(PathBuf, &'static str, &'static str)> {
    for d in sorted_dir(HWMON_DIR) {
        if read_trimmed(&d.join("name")).ok().as_deref() == Some("lenovo_wmi_other") && d.join("pwm1_enable").is_file() {
            return Some((d.join("pwm1_enable"), "0", "2"));
        }
    }
    sorted_dir(LEGION_DRV_DIR).into_iter().map(|d| d.join("fan_fullspeed")).find(|f| f.is_file()).map(|f| (f, "1", "0"))
}

/// Resolves the target file and the literal to write (the UI-level value is
/// translated for backends with a different encoding).
fn device_write(key: &str, value: &str) -> Result<(PathBuf, String), String> {
    if key == "fan_fullspeed" {
        let on = match value { "1" => true, "0" => false, _ => return Err("fan_fullspeed takes 0 or 1".into()) };
        let (f, v_on, v_off) = fullspeed_target().ok_or(
            "no Full Speed interface: this kernel's lenovo_wmi_other has no pwm1_enable and legion_laptop is not loaded")?;
        return Ok((f, (if on { v_on } else { v_off }).to_owned()));
    }
    device_target(key, value).map(|f| (f, value.to_owned()))
}

/// Resolves the target file and validates the value against what that file accepts.
fn device_target(key: &str, value: &str) -> Result<PathBuf, String> {
    if key == "charge_type" {
        for d in sorted_dir(PSU_DIR) {
            let f = d.join("charge_types");
            if read_trimmed(&d.join("type")).ok().as_deref() != Some("Battery") || !f.is_file() { continue; }
            let opts = bracket_options(&read_trimmed(&f).map_err(|e| e.to_string())?);
            if !opts.iter().any(|o| o == value) { return Err(format!("'{value}' is not one of: {}", opts.join(", "))); }
            return Ok(f);
        }
        return Err("no battery exposes charge_types".into());
    }
    if IDEAPAD_KEYS.contains(&key) {
        if value != "0" && value != "1" { return Err(format!("{key} takes 0 or 1")); }
        return sorted_dir(IDEAPAD_DIR).into_iter()
            .filter(|d| d.file_name().map_or(false, |n| n.to_string_lossy().starts_with("VPC")))
            .map(|d| d.join(key)).find(|f| f.is_file())
            .ok_or_else(|| format!("ideapad_acpi does not expose {key}"));
    }
    if let Some(n) = fan_index(key) {
        let rpm: i64 = value.parse().map_err(|_| "fan target must be an integer RPM".to_string())?;
        for d in sorted_dir(HWMON_DIR) {
            if read_trimmed(&d.join("name")).ok().as_deref() != Some("lenovo_wmi_other") { continue; }
            let f = d.join(key);
            if !f.is_file() { return Err(format!("{key} not present")); }
            let rd = |s: &str| read_trimmed(&d.join(format!("fan{n}_{s}"))).ok().and_then(|v| v.parse::<i64>().ok());
            // fanN_min is the EC's advertised floor, not a hard limit: requests below
            // it are passed through and the EC decides (it may clamp). Only the
            // ceiling and negatives are refused.
            let hi = rd("max").unwrap_or(10_000);
            if rpm < 0 || rpm > hi { return Err(format!("{rpm} RPM is outside 0..{hi} (0 = auto)")); }
            return Ok(f);
        }
        return Err("lenovo_wmi_other hwmon not found".into());
    }
    Err(format!("unknown device control '{key}'"))
}

/// Full Speed through WMAE, for kernels without a sysfs interface.
fn fullspeed_wmae(value: &str) -> Value {
    let on = match value { "1" => true, "0" => false, _ => return json!({"ok": false, "error": "fan_fullspeed takes 0 or 1"}) };
    match lpm_helpers::legion_wmi::fan_fullspeed_set(on) {
        Ok(now) => json!({"ok": true, "device": "fan_fullspeed", "effective": if now { "1" } else { "0" }, "backend": "wmae"}),
        Err(e) => json!({"ok": false, "error": format!("no Full Speed interface in this kernel, and the WMAE fallback failed: {e}")}),
    }
}

fn fullspeed_get() -> Value {
    if let Some((f, v_on, _)) = fullspeed_target() {
        let backend = f.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
        return match read_trimmed(&f) {
            Ok(v) => json!({"ok": true, "on": v == v_on, "backend": backend}),
            Err(e) => json!({"ok": false, "error": format!("{}: {e}", f.display())}),
        };
    }
    match lpm_helpers::legion_wmi::fan_fullspeed_get() {
        Ok(on) => json!({"ok": true, "on": on, "backend": "wmae"}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

fn set_device(key: &str, value: &str) -> Value {
    if key == "fan_fullspeed" && fullspeed_target().is_none() { return fullspeed_wmae(value); }
    let (f, value) = match device_write(key, value) { Ok(x) => x, Err(e) => return json!({"ok": false, "error": e}) };
    let value = value.as_str();
    let Some(real) = canonical_in_sysfs(&f) else {
        return json!({"ok": false, "error": "target resolves outside sysfs"});
    };
    if let Err(e) = sysfs_write(&real, value.as_bytes()) {
        let why = match e.raw_os_error() {
            Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) => format!("firmware rejected '{value}' ({e}); fan targets usually need the Custom profile"),
            Some(libc::EBUSY) => format!("device busy ({e})"),
            _ => format!("write failed: {e}"),
        };
        return json!({"ok": false, "error": why});
    }
    let mut effective = read_trimmed(&real).unwrap_or_default();
    if key == "fan_fullspeed" {
        // Report in UI terms (1 = full speed) whatever the backend encoding.
        effective = match (real.file_name().and_then(|n| n.to_str()), effective.as_str()) {
            (Some("pwm1_enable"), "0") | (Some("fan_fullspeed"), "1") => "1".into(),
            (Some(_), "") => effective,
            _ => "0".into(),
        };
    }
    json!({"ok": true, "device": key, "effective": effective})
}

fn run() -> Value {
    let req = match read_request(MAX_STDIN_BYTES) { Ok(v) => v, Err(e) => return e };
    let Some(obj) = req.as_object() else {
        return json!({"ok": false, "error": "payload must be a JSON object"});
    };
    match obj.get("memory").and_then(Value::as_str) {
        Some("spd") => return lpm_helpers::memory_spd::read_all(),
        Some("all") => return lpm_helpers::memory_spd::read_everything(),
        Some("aod_get") => return lpm_helpers::memory_spd::aod_get(),
        // BIOS variable writes: legion-firmware-helper (always password-protected).
        Some(op @ ("aod_set" | "aod_restore")) => return json!({"ok": false,
            "error": format!("{op} is handled by legion-firmware-helper (administrator password required)")}),
        Some(_) => return json!({"ok": false, "error": "'memory' must be one of spd, all, aod_get"}),
        None => {}
    }
    if let Some(op) = obj.get("fan_table") {
        return match op.as_str() {
            Some(op) => lpm_helpers::fan_table::handle(op, obj.get("levels")),
            None => json!({"ok": false, "error": "'fan_table' must be a string"}),
        };
    }
    if let Some(op) = obj.get("wmae_toggle") {
        return match op.as_str() {
            Some("get") => lpm_helpers::legion_wmi::wmae_toggles_get(),
            Some("set") => match (obj.get("key").and_then(Value::as_str), obj.get("on").and_then(Value::as_bool)) {
                (Some(k), Some(on)) => lpm_helpers::legion_wmi::wmae_toggle_set(k, on),
                _ => json!({"ok": false, "error": "'key' (string) and 'on' (bool) required"}),
            },
            _ => json!({"ok": false, "error": "'wmae_toggle' must be \"get\" or \"set\""}),
        };
    }
    if let Some(op) = obj.get("fan_fullspeed") {
        return match op.as_str() {
            Some("get") => fullspeed_get(),
            _ => json!({"ok": false, "error": "'fan_fullspeed' must be \"get\" (set it through 'device')"}),
        };
    }
    if let Some(dev) = obj.get("device") {
        return match (dev.as_str(), obj.get("value").and_then(Value::as_str)) {
            (Some(k), Some(v)) if !k.is_empty() && v.len() <= 32 => set_device(k, v),
            _ => json!({"ok": false, "error": "'device' and 'value' must be strings"}),
        };
    }
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
    init();
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
    fn device_keys() {
        assert_eq!(fan_index("fan1_target"), Some(1));
        assert_eq!(fan_index("fan12_target"), Some(12));
        assert_eq!(fan_index("fan_target"), None);
        assert_eq!(fan_index("fan1/../x_target"), None);
        assert_eq!(bracket_options("Fast Standard [Long_Life]"), vec!["Fast", "Standard", "Long_Life"]);
        assert!(set_device("../../etc", "1")["ok"] == false);
        assert!(set_device("fn_lock", "2")["error"].as_str().unwrap().contains("0 or 1"));
        assert!(set_device("fan1_target", "abc")["ok"] == false);
        assert!(set_device("fan_fullspeed", "2")["error"].as_str().unwrap().contains("0 or 1"));
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
