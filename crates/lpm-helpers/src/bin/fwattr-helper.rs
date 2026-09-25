//! Privileged writer for the firmware-attributes sysfs class.
//! Port of fwattr_helper.py -- same JSON protocol:
//!   single: {"path": ".../attributes/<attr>/current_value", "value": <int>}
//!   batch:  [ {path, value}, ... ]  ->  {"ok": all_ok, "results": [...]}
//!
//! The range is re-read from the sibling min_value/max_value/scalar_increment
//! files at write time, never taken from the GUI, closing the TOCTOU window.

use lpm_helpers::*;
use serde_json::{json, Value};
use std::path::Path;

const MAX_STDIN_BYTES: usize = 64 * 1024;
const MAX_BATCH: usize = 256;
const PREFIX: &str = "/sys/class/firmware-attributes/";
/// Sanity ceiling for attributes that report no range (min==max==step==0).
const UNRANGED_HARD_CAP: i64 = 100_000;

fn seg_ok(s: &str, allow_dot_dash: bool) -> bool {
    !s.is_empty()
        && s != "." && s != ".."
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'
            || (allow_dot_dash && (b == b'.' || b == b'-')))
}

/// ^/sys/class/firmware-attributes/[A-Za-z0-9_.-]+/attributes/[A-Za-z0-9_]+/current_value$
fn path_shape_ok(p: &str) -> bool {
    let Some(rest) = p.strip_prefix(PREFIX) else { return false };
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() == 4
        && seg_ok(parts[0], true)
        && parts[1] == "attributes"
        && seg_ok(parts[2], false)
        && parts[3] == "current_value"
}

/// Accepts JSON integers and integer strings (the Python helper did int(value)).
/// Refuses bools and fractional numbers rather than truncating them.
fn parse_value(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15).map(|f| f as i64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn read_i64(p: &Path) -> Option<i64> {
    read_trimmed(p).ok()?.parse().ok()
}

fn write_one(path_v: Option<&Value>, value_v: Option<&Value>) -> Value {
    let path_json = path_v.cloned().unwrap_or(Value::Null);
    let err = |m: String| json!({"path": path_json.clone(), "ok": false, "error": m});

    let Some(path) = path_v.and_then(Value::as_str).filter(|p| path_shape_ok(p)) else {
        return err("path does not match the expected firmware-attributes shape".into());
    };
    let Some(value) = parse_value(value_v) else {
        return err("value is not an integer".into());
    };
    // Even where the firmware reports min_value 0: writing 0 makes it treat the
    // feature as off, the kernel drops the attribute from sysfs, and it only came
    // back after resetting the power profiles from Windows.
    if value < 1 {
        return err("0 is refused: the firmware treats it as \"feature off\" and the attribute disappears from sysfs".into());
    }

    let p = Path::new(path);
    if !p.exists() {
        return err("attribute does not exist".into());
    }
    // The resolved file must still be <...>/attributes/<attr>/current_value
    // of a firmware-attributes class device, not merely "somewhere in /sys".
    let Some(real) = canonical_in_sysfs(p)
        .filter(|r| r.file_name().map_or(false, |n| n == "current_value"))
        .filter(|r| r.parent().and_then(|d| d.parent()).and_then(|d| d.file_name()).map_or(false, |n| n == "attributes"))
        .filter(|r| r.components().any(|c| c.as_os_str() == "firmware-attributes")) else {
        return err("path resolves outside sysfs".into());
    };
    if !real.is_file() {
        return err("attribute does not exist".into());
    }

    let dir = real.parent().unwrap_or(Path::new("/sys"));
    let (Some(minv), Some(maxv), Some(step)) = (
        read_i64(&dir.join("min_value")),
        read_i64(&dir.join("max_value")),
        read_i64(&dir.join("scalar_increment")),
    ) else {
        return err("could not read attribute metadata".into());
    };

    let ranged = !(minv == 0 && maxv == 0 && step == 0);
    if ranged {
        if value < minv || value > maxv {
            return err(format!("value {value} outside firmware range [{minv}, {maxv}]"));
        }
    } else if !(0..=UNRANGED_HARD_CAP).contains(&value) {
        return err(format!("value {value} outside sanity range [0, {UNRANGED_HARD_CAP}]"));
    }

    // Write the canonical target we validated, not the (re-resolvable) input path.
    if let Err(e) = sysfs_write(&real, value.to_string().as_bytes()) {
        return err(format!("write failed: {e}"));
    }
    json!({"path": path, "ok": true})
}

fn run() -> Value {
    let req = match read_request(MAX_STDIN_BYTES) {
        Ok(v) => v,
        Err(_) => return json!({"ok": false, "error": "malformed request"}),
    };
    match &req {
        Value::Array(items) => {
            if items.len() > MAX_BATCH {
                return json!({"ok": false, "error": format!("batch too large (max {MAX_BATCH})")});
            }
            let results: Vec<Value> = items.iter().map(|it| match it.as_object() {
                Some(o) => write_one(o.get("path"), o.get("value")),
                None => json!({"path": null, "ok": false, "error": "malformed batch item"}),
            }).collect();
            let all_ok = results.iter().all(|r| r["ok"] == true);
            json!({"ok": all_ok, "results": results})
        }
        Value::Object(o) => write_one(o.get("path"), o.get("value")),
        _ => json!({"ok": false, "error": "malformed request"}),
    }
}

fn main() {
    init();
    std::process::exit(finish(run()));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shape() {
        assert!(path_shape_ok("/sys/class/firmware-attributes/lenovo-wmi-other-0/attributes/ppt_pl1_spl/current_value"));
        assert!(!path_shape_ok("/sys/class/firmware-attributes/../attributes/x/current_value"));
        assert!(!path_shape_ok("/sys/class/firmware-attributes/d/attributes/a.b/current_value"));
        assert!(!path_shape_ok("/sys/class/firmware-attributes/d/attributes/a/b/current_value"));
        assert!(!path_shape_ok("/sys/class/firmware-attributes/d/attributes/a/min_value"));
    }
    #[test]
    fn values() {
        assert_eq!(parse_value(Some(&json!(45))), Some(45));
        assert_eq!(parse_value(Some(&json!("45"))), Some(45));
        assert_eq!(parse_value(Some(&json!(45.0))), Some(45));
        assert_eq!(parse_value(Some(&json!(45.5))), None);
        assert_eq!(parse_value(Some(&json!(true))), None);
    }
}
