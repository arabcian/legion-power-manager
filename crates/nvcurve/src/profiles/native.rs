//! Profile storage and schema (port of profiles/native.py).

use crate::atomicio::{ensure_dir, write_json};
use log::warn;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PROFILE_NAME_MAX_LEN: usize = 64;
const PROFILE_NAME_EXTRA: &str = " _-()";
pub const MAX_PROFILE_BYTES: u64 = 256 * 1024;

/// Canonical on-disk form of a profile name, or "" if unusable.
/// Same policy as native.py (alnum incl. Unicode, space, `_-()`, ≤64 chars).
pub fn safe_profile_name(name: &str) -> String {
    let cleaned: String = name.chars()
        .filter(|c| c.is_alphanumeric() || PROFILE_NAME_EXTRA.contains(*c)).collect();
    cleaned.trim().chars().take(PROFILE_NAME_MAX_LEN).collect::<String>().trim().to_owned()
}

/// Privileged-boundary check: accept only names already in canonical form.
pub fn is_canonical_profile_name(name: &str) -> bool {
    let c = safe_profile_name(name);
    !c.is_empty() && c == name
}

pub fn profile_path(dir: &str, name: &str) -> Option<PathBuf> {
    let safe = safe_profile_name(name);
    (!safe.is_empty()).then(|| Path::new(dir).join(format!("{safe}.json")))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileData {
    pub name: String,
    /// "point index" → delta kHz. Kept as parsed JSON; converted strictly by
    /// `deltas()` at apply time so one malformed key is a reported error.
    pub curve_deltas: Map<String, Value>,
    pub gpu_name: String,
    pub mem_offset_mhz: Option<i64>,
    pub power_limit_w: Option<i64>,
    pub mem_locked_min_mhz: Option<i64>,
    pub mem_locked_max_mhz: Option<i64>,
    /// NVML core clock cap. A flattened curve needs it: the driver keeps no
    /// point more than ~1000 MHz below stock, whatever freqDelta it stores, so
    /// only a cap holds the flat top. None = no cap (an active one is removed).
    pub gpu_clock_cap_mhz: Option<i64>,
}

const KNOWN: &[&str] = &["name", "curve_deltas", "gpu_name", "mem_offset_mhz", "power_limit_w",
                         "mem_locked_min_mhz", "mem_locked_max_mhz", "gpu_clock_cap_mhz"];
const OBSOLETE: &[&str] = &["gpu_locked_min_mhz", "gpu_locked_max_mhz", "vram_p0_offset_mhz"];

fn opt_int(m: &Map<String, Value>, k: &str) -> Result<Option<i64>, String> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) if n.is_i64() => Ok(n.as_i64()),
        Some(v) => Err(format!("field {k:?} must be an integer, got {v}")),
    }
}

impl ProfileData {
    pub fn from_value(v: Value, origin: &str) -> Result<Self, String> {
        let Value::Object(mut m) = v else { return Err(format!("profile {origin} is not a JSON object")) };
        if !m.contains_key("mem_offset_mhz") {
            if let Some(old) = m.remove("vram_p0_offset_mhz") { m.insert("mem_offset_mhz".into(), old); }
        }
        for k in OBSOLETE { m.remove(*k); }
        let unknown: Vec<&String> = m.keys().filter(|k| !KNOWN.contains(&k.as_str())).collect();
        if !unknown.is_empty() {
            warn!("Profile {origin} has unknown field(s) {} — ignoring",
                  unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
        }
        let name = m.get("name").and_then(Value::as_str)
            .ok_or_else(|| format!("profile {origin}: missing string field \"name\""))?.to_owned();
        let curve_deltas = match m.get("curve_deltas") {
            Some(Value::Object(c)) => c.clone(),
            None | Some(Value::Null) => Map::new(),
            Some(_) => return Err(format!("profile {origin}: curve_deltas must be an object")),
        };
        Ok(ProfileData {
            name,
            curve_deltas,
            gpu_name: m.get("gpu_name").and_then(Value::as_str).unwrap_or("").to_owned(),
            mem_offset_mhz: opt_int(&m, "mem_offset_mhz")?,
            power_limit_w: opt_int(&m, "power_limit_w")?,
            mem_locked_min_mhz: opt_int(&m, "mem_locked_min_mhz")?,
            mem_locked_max_mhz: opt_int(&m, "mem_locked_max_mhz")?,
            gpu_clock_cap_mhz: opt_int(&m, "gpu_clock_cap_mhz")?,
        })
    }

    pub fn to_value(&self) -> Value {
        json!({
            "name": self.name, "curve_deltas": self.curve_deltas, "gpu_name": self.gpu_name,
            "mem_offset_mhz": self.mem_offset_mhz, "power_limit_w": self.power_limit_w,
            "mem_locked_min_mhz": self.mem_locked_min_mhz, "mem_locked_max_mhz": self.mem_locked_max_mhz,
            "gpu_clock_cap_mhz": self.gpu_clock_cap_mhz,
        })
    }

    /// Strict conversion: every key must be an integer string, every value an
    /// integer (or an integer-valued number / string, as Python int() allowed).
    pub fn deltas(&self) -> Result<BTreeMap<i64, i64>, String> {
        self.curve_deltas.iter().map(|(k, v)| {
            let p: i64 = k.trim().parse().map_err(|_| format!("key {k:?} is not an integer"))?;
            let d = match v {
                Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            }.ok_or_else(|| format!("value for point {k} is not an integer: {v}"))?;
            Ok((p, d))
        }).collect()
    }
}

pub fn save_profile(dir: &str, data: &ProfileData) -> std::io::Result<PathBuf> {
    ensure_dir(Path::new(dir), 0o755)?;
    let mut safe = safe_profile_name(&data.name);
    if safe.is_empty() { safe = "Unnamed".into(); }
    let path = Path::new(dir).join(format!("{safe}.json"));
    let mut d = data.clone();
    d.name = safe;
    write_json(&path, &d.to_value(), 0o644)?;
    Ok(path)
}

pub fn load_profile(path: &Path) -> Result<ProfileData, String> {
    let text = crate::atomicio::read_regular(path, MAX_PROFILE_BYTES as u64)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    ProfileData::from_value(v, &path.display().to_string())
}

pub fn list_profiles(dir: &str) -> Vec<ProfileData> {
    let mut out: Vec<ProfileData> = std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |x| x == "json"))
        .filter_map(|p| load_profile(&p).map_err(|e| warn!("Skipping unreadable profile: {e}")).ok())
        .collect();
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

pub fn rename_profile(dir: &str, old: &str, new: &str) -> bool {
    let (Some(op), Some(np)) = (profile_path(dir, old), profile_path(dir, new)) else { return false };
    if !op.exists() { return false; }
    let r = (|| -> Result<(), String> {
        let mut p = load_profile(&op)?;
        p.name = safe_profile_name(new);
        write_json(&np, &p.to_value(), 0o644).map_err(|e| e.to_string())?;
        if op != np { std::fs::remove_file(&op).map_err(|e| e.to_string())?; }
        Ok(())
    })();
    if let Err(e) = &r { warn!("rename_profile({old:?} -> {new:?}) failed: {e}"); }
    r.is_ok()
}

pub fn delete_profile(dir: &str, name: &str) -> bool {
    profile_path(dir, name).map_or(false, |p| p.exists() && std::fs::remove_file(p).is_ok())
}

#[cfg(test)]
mod tests {
    #[test]
    fn clock_cap_roundtrip() {
        let p = ProfileData { name: "flat".into(), gpu_clock_cap_mhz: Some(2047), ..Default::default() };
        let back = ProfileData::from_value(p.to_value(), "test").unwrap();
        assert_eq!(back.gpu_clock_cap_mhz, Some(2047));
        // the obsolete Python-era lock fields are still dropped, not mistaken for the cap
        let old = serde_json::json!({"name": "x", "gpu_locked_max_mhz": 1900});
        assert_eq!(ProfileData::from_value(old, "test").unwrap().gpu_clock_cap_mhz, None);
    }

    use super::*;
    #[test]
    fn names() {
        assert_eq!(safe_profile_name("Silent (UV)"), "Silent (UV)");
        assert_eq!(safe_profile_name("../../etc/passwd"), "etcpasswd");
        assert_eq!(safe_profile_name("..."), "");
        assert_eq!(safe_profile_name("  Günlük  "), "Günlük");
        assert_eq!(safe_profile_name(&"a".repeat(80)).len(), 64);
        assert!(is_canonical_profile_name("Silent (UV)"));
        assert!(!is_canonical_profile_name(" Silent"));
        assert!(!is_canonical_profile_name("a/b"));
    }
    #[test]
    fn roundtrip_and_migration() {
        let v = json!({"name":"x","curve_deltas":{"3":-15000,"4":"20000"},"vram_p0_offset_mhz":500,
                        "gpu_locked_min_mhz":1,"future":true});
        let p = ProfileData::from_value(v, "t").unwrap();
        assert_eq!(p.mem_offset_mhz, Some(500));
        let d = p.deltas().unwrap();
        assert_eq!(d[&3], -15000);
        assert_eq!(d[&4], 20000);
        let back = ProfileData::from_value(p.to_value(), "t").unwrap();
        assert_eq!(back, p);
        let bad = ProfileData::from_value(json!({"name":"x","curve_deltas":{"a":1}}), "t").unwrap();
        assert!(bad.deltas().is_err());
    }
    #[test]
    fn disk_ops() {
        let d = std::env::temp_dir().join(format!("nvc-prof-{}", std::process::id()));
        let ds = d.to_str().unwrap();
        let p = ProfileData { name: "My (OC)".into(), ..Default::default() };
        save_profile(ds, &p).unwrap();
        assert_eq!(list_profiles(ds).len(), 1);
        assert!(rename_profile(ds, "My (OC)", "Daily"));
        assert_eq!(list_profiles(ds)[0].name, "Daily");
        assert!(delete_profile(ds, "Daily"));
        assert!(list_profiles(ds).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
