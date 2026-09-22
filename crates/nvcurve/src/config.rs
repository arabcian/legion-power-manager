//! Runtime configuration (port of config.py + persistent-config loading).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const PERSISTENT_CONFIG_FILE: &str = "/etc/nvcurve/config.json";
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub max_delta_khz: i64,
    pub auto_snapshot: bool,
    pub max_snapshots: usize,
    pub poll_interval_s: f64,
    pub host: String,
    pub port: u16,
    pub snapshot_dir: String,
    pub profile_dir: String,
    /// GPU stable key (UUID / "pci:xxxx" / "idx:n") → profile name.
    pub auto_load_profiles: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_delta_khz: 1_000_000,
            auto_snapshot: true,
            max_snapshots: 20,
            poll_interval_s: 1.0,
            host: "127.0.0.1".into(),
            port: 8042,
            snapshot_dir: "/var/cache/nvcurve/snapshots".into(),
            profile_dir: "/etc/nvcurve/profiles".into(),
            auto_load_profiles: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load persistent config. Missing/unreadable/malformed → defaults,
    /// field-by-field: one bad field doesn't discard the rest.
    pub fn load(path: &Path) -> Config {
        let mut cfg = Config::default();
        let Ok(md) = std::fs::metadata(path) else { return cfg };
        if md.len() > MAX_CONFIG_BYTES {
            log::warn!("{} exceeds {MAX_CONFIG_BYTES} bytes — using defaults", path.display());
            return cfg;
        }
        let Ok(text) = std::fs::read_to_string(path) else { return cfg };
        let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(&text) else {
            log::warn!("{} is not a JSON object — using defaults", path.display());
            return cfg;
        };
        macro_rules! take {
            ($field:ident) => {
                if let Some(v) = m.get(stringify!($field)) {
                    match serde_json::from_value(v.clone()) {
                        Ok(x) => cfg.$field = x,
                        Err(e) => log::warn!("config field {}: {e} — keeping default", stringify!($field)),
                    }
                }
            };
        }
        take!(max_delta_khz); take!(auto_snapshot); take!(max_snapshots); take!(poll_interval_s);
        take!(host); take!(port); take!(snapshot_dir); take!(profile_dir);
        if let Some(serde_json::Value::Object(a)) = m.get("auto_load_profiles") {
            cfg.auto_load_profiles = a.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned()))).collect();
        } else if let Some(old) = m.get("auto_load_profile").and_then(|v| v.as_str()) {
            // Legacy single-string format → GPU 0.
            cfg.auto_load_profiles.insert("idx:0".into(), old.to_owned());
        }
        cfg
    }
}
