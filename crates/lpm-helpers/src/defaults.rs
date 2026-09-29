//! Boot defaults and per-key outcome history: the anchor autotune adapts from.
//!
//! * defaults.json - every tunable's value early in boot: after the kernel,
//!   the distro and the user's own sysctl.conf, before TLP and before any LPM
//!   preset. Captured by `lpm-boot-guard arm` (and by the lpm-tune boot op as
//!   a fallback). "clean" = no LPM state existed and TLP had not run yet.
//!   A clean snapshot is never replaced by a dirty one of the same kernel.
//! * outcomes.json - per key: how often a changed value was applied and how
//!   often the pressure guard rolled it back. Autotune turns rollbacks into a
//!   stability cost; two rollbacks retire every non-default candidate of that key.
//! Both files are root-owned 0644 under /var/lib/legion-power-manager.

use crate::tune;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const DIR: &str = "/var/lib/legion-power-manager";
pub const DEFAULTS_FILE: &str = "/var/lib/legion-power-manager/defaults.json";
pub const OUTCOMES_FILE: &str = "/var/lib/legion-power-manager/outcomes.json";
const TUNE_STATE: &str = "/run/legion-power-manager/tune/state.json";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Defaults { pub values: BTreeMap<String, String>, pub clean: bool, pub kernel: String }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome { pub applies: u32, pub rollbacks: u32 }

fn boot_id() -> String { crate::read_trimmed(Path::new("/proc/sys/kernel/random/boot_id")).unwrap_or_default() }
fn kernel() -> String { crate::read_trimmed(Path::new("/proc/sys/kernel/osrelease")).unwrap_or_default() }
fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

fn read_json(path: &str) -> Option<Value> {
    crate::read_root_file(path, 512 * 1024).and_then(|s| serde_json::from_str(&s).ok())
}

/// Root: snapshot every single-valued tunable once per boot.
pub fn capture() -> Result<Value, String> {
    let (id, kr) = (boot_id(), kernel());
    let old = read_json(DEFAULTS_FILE);
    if old.as_ref().map_or(false, |o| o["boot_id"] == json!(id)) {
        return Ok(json!({"captured": false, "reason": "already captured this boot"}));
    }
    let clean = !Path::new(TUNE_STATE).exists() && !Path::new("/run/tlp").exists();
    if !clean && old.as_ref().map_or(false, |o| o["clean"] == json!(true) && o["kernel"] == json!(kr)) {
        return Ok(json!({"captured": false, "reason": "keeping the clean snapshot of this kernel"}));
    }
    let mut values = Map::new();
    for t in tune::TUNABLES {
        if tune::best_effort(t) || !tune::vendor_ok(t) { continue; }
        if let Some(v) = tune::current(t) { values.insert(t.key.to_owned(), json!(v)); }
    }
    let body = json!({"boot_id": id, "kernel": kr, "time": now(), "clean": clean, "values": values});
    crate::secure_dir(DIR)?;
    crate::write_root_file(DEFAULTS_FILE, &serde_json::to_vec_pretty(&body).unwrap())?;
    Ok(json!({"captured": true, "clean": clean, "keys": body["values"].as_object().map_or(0, |m| m.len())}))
}

pub fn load() -> Option<Defaults> {
    parse_defaults(&read_json(DEFAULTS_FILE)?)
}

pub fn parse_defaults(v: &Value) -> Option<Defaults> {
    let values = v["values"].as_object()?.iter().filter_map(|(k, x)| Some((k.clone(), x.as_str()?.to_owned()))).collect();
    Some(Defaults { values, clean: v["clean"].as_bool().unwrap_or(false), kernel: v["kernel"].as_str().unwrap_or("").to_owned() })
}

pub fn load_outcomes() -> BTreeMap<String, Outcome> {
    read_json(OUTCOMES_FILE).map(|v| parse_outcomes(&v)).unwrap_or_default()
}

pub fn parse_outcomes(v: &Value) -> BTreeMap<String, Outcome> {
    v.as_object().into_iter().flatten().map(|(k, x)| (k.clone(), Outcome {
        applies: x["applies"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32,
        rollbacks: x["rollbacks"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32,
    })).collect()
}

/// Root: count applies (values that differ from the boot default) or rollbacks.
pub fn record(keys: &[String], rollback: bool) {
    if keys.is_empty() { return; }
    let mut all = load_outcomes();
    for k in keys {
        let o = all.entry(k.clone()).or_default();
        if rollback { o.rollbacks = o.rollbacks.saturating_add(1); } else { o.applies = o.applies.saturating_add(1); }
    }
    let body: Map<String, Value> = all.iter().map(|(k, o)| (k.clone(), json!({"applies": o.applies, "rollbacks": o.rollbacks}))).collect();
    if crate::secure_dir(DIR).is_ok() {
        let _ = crate::write_root_file(OUTCOMES_FILE, &serde_json::to_vec_pretty(&Value::Object(body)).unwrap());
    }
}
