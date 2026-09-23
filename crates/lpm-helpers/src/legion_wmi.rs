//! Experimental Legion GPU power knobs.
//!
//! Two write paths, one per knob, decided by whether the firmware exposes a
//! range for it in /sys/class/firmware-attributes:
//!
//!   ranged (min!=max)   → sysfs firmware-attributes current_value
//!     gpu_nv_ac_offset  CPU+GPU total processing power offset   (fw 10..130)
//!     gpu_temp          GPU thermal target                       (fw 75..87)
//!
//!   unranged (min==max==0, kernel refuses sysfs writes) → acpi_call \_SB.GZFD.WMAE
//!     gpu_nv_ctgp       cTGP                     feature 0x02020000 (GPL2)
//!     gpu_nv_ppab       Dynamic Boost ceiling    feature 0x02010000 (GPL1)
//!     gpu_nv_cpu_boost  Dynamic Boost floor      feature 0x020B0000 (G1PL)
//!
//! All confirmed on Cihan's Legion by decompiling the BIOS and by live reads
//! (base TGP 0x50=80, ceiling/floor 0x19=25). WMAE ABI, also confirmed live:
//!   get: \_SB.GZFD.WMAE 0x0 0x11 {id[0],id[1],id[2],id[3]}
//!   set: \_SB.GZFD.WMAE 0x0 0x12 {id..., value(dword LE)}
//! After a set the firmware does Notify(NPCF,0xC0) itself, so nvidia-powerd
//! re-reads with no /dev/mem and no daemon. Effective only in the Custom
//! platform profile (ODV1==3); other profiles let SGPS overwrite them.
//!
//! Reads always come back through the same channel that owns the knob (sysfs
//! for ranged, WMAE-get for unranged), so the tab shows the true value even
//! when the sysfs current_value of an unranged attribute is stale.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const FW_BASE: &str = "/sys/class/firmware-attributes";
const ACPI_CALL: &str = "/proc/acpi/call";

/// cTGP: no hard cap the user can't exceed — they asked to write freely for a
/// test. Kept only as a sanity ceiling so a typo can't send a wild value to
/// the EC. The GUI shows the *recommended* 5..150 envelope separately.
pub const CTGP_SANITY_MAX: i64 = 250;

pub struct Feat {
    pub key: &'static str,
    pub attr: &'static str,   // firmware-attributes name (also read source)
    pub id: u32,              // WMAE feature id (0 = ranged/sysfs only)
    pub label: &'static str,
    pub unit: &'static str,
    pub via_acpi: bool,       // unranged: write through WMAE
    pub lo: i64, pub hi: i64, // GUI range hint; the sysfs range wins when present
}

pub const FEATURES: &[Feat] = &[
    Feat { key: "ctgp",       attr: "gpu_nv_ctgp",      id: 0x0202_0000, label: "cTGP",                  unit: "W",  via_acpi: true,  lo: 5,  hi: CTGP_SANITY_MAX },
    Feat { key: "boost_up",   attr: "gpu_nv_ppab",      id: 0x0201_0000, label: "Dynamic Boost ceiling", unit: "W",  via_acpi: true,  lo: 0,  hi: 25 },
    Feat { key: "boost_down", attr: "gpu_nv_cpu_boost", id: 0x020B_0000, label: "Dynamic Boost floor",   unit: "W",  via_acpi: true,  lo: 0,  hi: 25 },
    Feat { key: "ac_offset",  attr: "gpu_nv_ac_offset", id: 0,           label: "CPU+GPU total offset",  unit: "W",  via_acpi: false, lo: 10, hi: 130 },
    Feat { key: "gpu_temp",   attr: "gpu_temp",         id: 0,           label: "GPU temp target",       unit: "°C", via_acpi: false, lo: 75, hi: 87 },
];

pub fn feat(key: &str) -> Option<&'static Feat> { FEATURES.iter().find(|f| f.key == key) }

// ── firmware-attributes (sysfs) ─────────────────────────────────────────────

fn attr_dir(attr: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(FW_BASE).ok()?;
    for e in rd.flatten() {
        let d = e.path().join("attributes").join(attr);
        if d.join("current_value").is_file() { return Some(d); }
    }
    None
}

fn read_i64(p: &Path) -> Option<i64> { std::fs::read_to_string(p).ok()?.trim().parse().ok() }

struct SysRange { cur: i64, min: i64, max: i64, def: Option<i64>, step: i64, ranged: bool }

fn sysfs_read(attr: &str) -> Option<SysRange> {
    let d = attr_dir(attr)?;
    let cur = read_i64(&d.join("current_value"))?;
    let min = read_i64(&d.join("min_value")).unwrap_or(0);
    let max = read_i64(&d.join("max_value")).unwrap_or(0);
    let step = read_i64(&d.join("scalar_increment")).unwrap_or(0);
    let def = read_i64(&d.join("default_value"));
    Some(SysRange { cur, min, max, def, step, ranged: !(min == 0 && max == 0 && step == 0) })
}

/// Write a ranged attribute through fwattr-helper's own path — but we are the
/// root helper, so write directly here after re-validating against the live
/// range (never trust the GUI's bounds).
fn sysfs_write(attr: &str, value: i64) -> Result<(), String> {
    let d = attr_dir(attr).ok_or_else(|| format!("{attr}: attribute not present"))?;
    let r = sysfs_read(attr).ok_or_else(|| format!("{attr}: metadata unreadable"))?;
    if r.ranged && (value < r.min || value > r.max) {
        return Err(format!("{value} outside firmware range [{}, {}]", r.min, r.max));
    }
    crate::sysfs_write(&d.join("current_value"), value.to_string().as_bytes())
        .map_err(|e| format!("{attr}: {e}"))
}

// ── acpi_call (WMAE) ────────────────────────────────────────────────────────

fn acpi_available() -> bool { Path::new(ACPI_CALL).exists() }

fn modprobe_acpi_call() {
    for p in ["/sbin/modprobe", "/usr/sbin/modprobe", "/usr/bin/modprobe", "/bin/modprobe"] {
        if Path::new(p).is_file() {
            let _ = std::process::Command::new(p).arg("acpi_call").env_clear()
                .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
            return;
        }
    }
}

fn acpi_raw(expr: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    let mut f = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(ACPI_CALL)
        .map_err(|e| format!("{ACPI_CALL}: {e}"))?;
    f.write_all(expr.as_bytes()).map_err(|e| format!("acpi_call write: {e}"))?;
    let mut out = String::new();
    f.read_to_string(&mut out).map_err(|e| format!("acpi_call read: {e}"))?;
    let out = out.trim_matches(char::from(0)).trim().to_owned();
    if out.starts_with("Error") { return Err(format!("acpi_call: {out}")); }
    Ok(out)
}

fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) { u64::from_str_radix(h, 16).ok() } else { s.parse().ok() }
}

fn wmae_get(id: u32) -> Result<i64, String> {
    let b = id.to_le_bytes();
    let expr = format!("\\_SB.GZFD.WMAE 0x0 0x11 {{0x{:02x}, 0x{:02x}, 0x{:02x}, 0x{:02x}}}", b[0], b[1], b[2], b[3]);
    parse_u64(&acpi_raw(&expr)?).map(|v| v as i64).ok_or_else(|| "WMAE get: unparseable".into())
}

fn wmae_set(id: u32, value: i64) -> Result<(), String> {
    let a = id.to_le_bytes();
    let v = (value as u32).to_le_bytes();
    let expr = format!("\\_SB.GZFD.WMAE 0x0 0x12 {{0x{:02x},0x{:02x},0x{:02x},0x{:02x},0x{:02x},0x{:02x},0x{:02x},0x{:02x}}}",
                       a[0], a[1], a[2], a[3], v[0], v[1], v[2], v[3]);
    acpi_raw(&expr).map(|_| ())
}

// ── platform profile ────────────────────────────────────────────────────────

pub fn platform_profile() -> Option<String> {
    std::fs::read_to_string("/sys/firmware/acpi/platform_profile").ok().map(|s| s.trim().to_owned())
}
fn in_custom() -> bool { platform_profile().as_deref() == Some("custom") }

/// nvidia-smi Max/Min Power Limit, if nvidia-smi is on PATH. Used only to show
/// the recommended cTGP envelope; never a hard limit here.
fn nvidia_power_limits() -> Option<(i64, i64)> {
    let exe = ["/usr/bin/nvidia-smi", "/bin/nvidia-smi", "/usr/local/bin/nvidia-smi"].iter().find(|p| Path::new(p).is_file())?;
    let out = std::process::Command::new(exe).args(["-q", "-d", "POWER"]).env_clear().env("PATH", "/usr/bin:/bin").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let grab = |label: &str| text.lines().find(|l| l.contains(label))
        .and_then(|l| l.split(':').nth(1)).and_then(|v| v.trim().split_whitespace().next())
        .and_then(|n| n.parse::<f64>().ok()).map(|f| f as i64);
    Some((grab("Min Power Limit")?, grab("Max Power Limit")?))
}

// ── read one knob (through its owning channel) ──────────────────────────────

fn read_value(f: &Feat) -> Result<i64, String> {
    if f.via_acpi { wmae_get(f.id) } else {
        sysfs_read(f.attr).map(|r| r.cur).ok_or_else(|| format!("{}: not present", f.attr))
    }
}

// ── operations ──────────────────────────────────────────────────────────────

pub fn status() -> Value {
    if !acpi_available() { modprobe_acpi_call(); }
    let mut out = json!({
        "ok": true,
        "acpi": acpi_available(),
        "profile": platform_profile(),
        "custom": in_custom(),
    });
    let mut vals = serde_json::Map::new();
    for f in FEATURES {
        let sys = sysfs_read(f.attr);
        let mut o = json!({"label": f.label, "unit": f.unit, "via_acpi": f.via_acpi, "attr": f.attr});
        match read_value(f) {
            Ok(v) => { o["value"] = json!(v); }
            Err(e) => { o["error"] = json!(e); }
        }
        // Range: firmware's when it exposes one, else the feature's GUI hint.
        if let Some(s) = &sys {
            if s.ranged { o["min"] = json!(s.min); o["max"] = json!(s.max); o["step"] = json!(s.step.max(1)); o["ranged"] = json!(true); }
            if let Some(d) = s.def { o["default"] = json!(d); }
        }
        if o.get("ranged").is_none() { o["min"] = json!(f.lo); o["max"] = json!(f.hi); o["step"] = json!(1); o["ranged"] = json!(false); }
        vals.insert(f.key.into(), o);
    }
    // cTGP recommended envelope from nvidia-smi (advisory only).
    if let Some((minp, maxp)) = nvidia_power_limits() {
        let ceil = vals.get("boost_up").and_then(|v| v["value"].as_i64()).unwrap_or(25);
        out["envelope"] = json!({"gpu_min_w": minp, "gpu_max_w": maxp, "ctgp_max_w": (maxp - ceil).max(minp), "ctgp_min_w": minp});
    }
    out["values"] = Value::Object(vals);
    if !acpi_available() {
        out["note"] = json!("acpi_call not loaded — the cTGP / boost knobs can't be written (modprobe acpi_call).");
    }
    out
}

/// Apply pre-validated integer values by key.
pub fn apply(values: &serde_json::Map<String, Value>) -> Value {
    let mut results = Vec::new();
    let mut all = true;
    let need_acpi = values.keys().any(|k| feat(k).map(|f| f.via_acpi).unwrap_or(false));
    if need_acpi && !acpi_available() { modprobe_acpi_call(); }

    for (key, jv) in values {
        let r = (|| {
            let f = feat(key).ok_or_else(|| format!("unknown knob '{key}'"))?;
            let v = jv.as_i64().ok_or_else(|| format!("{key}: not an integer"))?;
            if f.via_acpi {
                if !acpi_available() { return Err("acpi_call not available".into()); }
                // No firmware range for these; only a sanity clamp so a typo
                // can't send something absurd to the EC. cTGP is left wide on
                // purpose (user testing); ceiling/floor keep their 0..25.
                let cap = if f.key == "ctgp" { CTGP_SANITY_MAX } else { f.hi };
                if v < 0 || v > cap { return Err(format!("{} must be 0..{cap} {}", f.label, f.unit)); }
                wmae_set(f.id, v)?;
                let back = wmae_get(f.id)?;
                if back != v { return Err(format!("readback {back} ≠ {v}")); }
                Ok(format!("{} = {} {} (WMAE)", f.label, back, f.unit))
            } else {
                sysfs_write(f.attr, v)?;
                let back = read_value(f)?;
                if back != v { return Err(format!("readback {back} ≠ {v}")); }
                Ok(format!("{} = {} {}", f.label, back, f.unit))
            }
        })();
        all &= r.is_ok();
        results.push(match r { Ok(m) => json!({"what": key, "ok": true, "message": m}),
                               Err(m) => json!({"what": key, "ok": false, "message": m}) });
    }
    json!({"ok": all, "results": results, "custom": in_custom(),
           "note": if in_custom() { Value::Null } else { json!("Not in the Custom platform profile — the firmware overwrites these on the next profile change.") }})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn feature_map() {
        assert_eq!(feat("ctgp").unwrap().id, 0x0202_0000);
        assert_eq!(feat("boost_up").unwrap().id, 0x0201_0000);
        assert_eq!(feat("boost_down").unwrap().id, 0x020B_0000);
        assert!(feat("ctgp").unwrap().via_acpi);
        assert!(!feat("ac_offset").unwrap().via_acpi);
        assert_eq!(feat("ac_offset").unwrap().attr, "gpu_nv_ac_offset");
        assert_eq!(feat("boost_down").unwrap().attr, "gpu_nv_cpu_boost");
    }
    #[test]
    fn wmae_id_le() {
        assert_eq!(0x0202_0000u32.to_le_bytes(), [0, 0, 2, 2]);
        assert_eq!(0x020B_0000u32.to_le_bytes(), [0, 0, 0x0b, 2]);
    }
}
