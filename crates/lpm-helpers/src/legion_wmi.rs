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
/// the EC. The recommended envelope is available from status on request.
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
    Feat { key: "boost_up",   attr: "gpu_nv_ppab",      id: 0x0201_0000, label: "Dynamic Boost ceiling", unit: "W",  via_acpi: true,  lo: 1,  hi: 25 },
    Feat { key: "boost_down", attr: "gpu_nv_cpu_boost", id: 0x020B_0000, label: "Dynamic Boost floor",   unit: "W",  via_acpi: true,  lo: 1,  hi: 25 },
    Feat { key: "ac_offset",  attr: "gpu_nv_ac_offset", id: 0,           label: "CPU+GPU total offset",  unit: "W",  via_acpi: false, lo: 10, hi: 130 },
    Feat { key: "gpu_temp",   attr: "gpu_temp",         id: 0,           label: "GPU temp target",       unit: "°C", via_acpi: false, lo: 75, hi: 87 },
    // CPU limits: written through sysfs; the WMAE id (same numbering as the
    // kernel's lenovo-wmi-other: SPPT 1, SPL 2, FPPT 3, TEMP 4 — matched
    // against this BIOS's DSDT) is only a fallback for when the attribute has
    // vanished from sysfs. The GUI enables that fallback only after it has seen
    // the WMAE read-back equal the sysfs value on this machine.
    Feat { key: "spl",        attr: "ppt_pl1_spl",      id: 0x0102_0000, label: "CPU sustained power (PL1)", unit: "W",  via_acpi: false, lo: 5,  hi: 200 },
    Feat { key: "sppt",       attr: "ppt_pl2_sppt",     id: 0x0101_0000, label: "CPU short-term power (PL2)", unit: "W", via_acpi: false, lo: 5,  hi: 250 },
    Feat { key: "fppt",       attr: "ppt_pl3_fppt",     id: 0x0103_0000, label: "CPU peak power (PL3)",      unit: "W",  via_acpi: false, lo: 5,  hi: 250 },
    Feat { key: "cpu_temp",   attr: "cpu_temp",         id: 0x0104_0000, label: "CPU temperature limit",     unit: "°C", via_acpi: false, lo: 60, hi: 105 },
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
    if value < 1 { return Err(ZERO_REFUSED.into()); }
    if r.ranged {
        if value < r.min || value > r.max {
            return Err(format!("{value} outside firmware range [{}, {}]", r.min, r.max));
        }
    } else if let Some(f) = FEATURES.iter().find(|f| f.attr == attr) {
        // No firmware range published: fall back to the feature's own
        // envelope instead of writing an unchecked integer to the EC.
        if value < f.lo || value > f.hi {
            return Err(format!("{value} outside {}..{} {} (firmware publishes no range)", f.lo, f.hi, f.unit));
        }
    } else {
        return Err(format!("{attr}: no range known; refusing"));
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

/// One acpi_call transaction. The module keeps a single global result
/// buffer: two concurrent callers (the GUI's status poll and an apply, or a
/// second tool) can read each other's result. The write→read pair is
/// therefore done under an exclusive flock on the proc file, and the reply
/// is bounded (the module's buffer is small; a runaway read never grows).
fn acpi_raw(expr: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    const MAX_REPLY: u64 = 64 * 1024;
    let mut f = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(ACPI_CALL)
        .map_err(|e| format!("{ACPI_CALL}: {e}"))?;
    let _lock = crate::FdLock::exclusive(&f).map_err(|e| format!("{ACPI_CALL} lock: {e}"))?;
    f.write_all(expr.as_bytes()).map_err(|e| format!("acpi_call write: {e}"))?;
    let mut raw = Vec::new();
    (&mut f).take(MAX_REPLY).read_to_end(&mut raw).map_err(|e| format!("acpi_call read: {e}"))?;
    let out = String::from_utf8_lossy(&raw);
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

// ── GPU mode (MUX) ──────────────────────────────────────────────────────────
//
// From the DSDT of BIOS N20 (WinSMCN20WW), \_SB.GZFD.WMAA = the GameZone WMI
// interface 887B54E3-DDDC-4B2C-8B88-68A26A8835D0 (what Legion Space calls):
//   0x28 IsSupportGSync  → 2 on this machine
//   0x29 GetGSyncStatus  → EC MSMF bit: 1 = dGPU direct (MUX to NVIDIA), 0 = hybrid
//   0x2A SetGSyncStatus  → SMI 0xCA via port 0xB0, sub-command 0x26 (1) / 0x25 (0)
// The firmware switches the MUX at the next boot. What runs *now* is visible
// on the PCI bus: in dGPU mode the AMD iGPU is hidden.
// WMAE feature 0x00210000 (EC GMDM, 3 states) is reported read-only: its
// meaning lives in EC firmware and is not verified yet.

const GZ_GSYNC_SUPPORTED: u8 = 0x28;
const GZ_GSYNC_GET: u8 = 0x29;
const GZ_GSYNC_SET: u8 = 0x2A;
const FEAT_GMDM: u32 = 0x0021_0000;

fn wmaa(method: u8, arg: u64) -> Result<u64, String> {
    let out = acpi_raw(&format!("\\_SB.GZFD.WMAA 0x0 0x{method:x} 0x{arg:x}"))?;
    parse_u64(&out).ok_or_else(|| format!("WMAA 0x{method:x}: unparseable reply '{out}'"))
}

/// AMD display controller visible on the PCI bus (hybrid mode running).
fn amd_igpu_present() -> bool {
    std::fs::read_dir("/sys/bus/pci/devices").into_iter().flatten().flatten().any(|e| {
        let rd = |f: &str| std::fs::read_to_string(e.path().join(f)).ok().map(|s| s.trim().to_owned());
        rd("vendor").as_deref() == Some("0x1002") && rd("class").map_or(false, |c| c.starts_with("0x03"))
    })
}

fn mode_name(dgpu: bool) -> &'static str { if dgpu { "dgpu" } else { "hybrid" } }

pub fn gpu_mode_status() -> Value {
    if !acpi_available() { modprobe_acpi_call(); }
    if !acpi_available() {
        return json!({"ok": false, "error": "acpi_call is not loaded (modprobe acpi_call)", "active": mode_name(!amd_igpu_present())});
    }
    let supported = wmaa(GZ_GSYNC_SUPPORTED, 0).map(|v| v != 0).unwrap_or(false);
    let active_dgpu = !amd_igpu_present();
    let next = wmaa(GZ_GSYNC_GET, 0).ok().map(|v| v == 1);
    json!({
        "ok": true,
        "supported": supported,
        "active": mode_name(active_dgpu),
        "next_boot": next.map(mode_name),
        "reboot_pending": next.map_or(false, |n| n != active_dgpu),
        "amdgpu_driver": crate::tune::kmod_available("amdgpu", "drivers/gpu/drm/amd/amdgpu"),
        "gmdm_raw": wmae_get(FEAT_GMDM).ok(),
    })
}

/// `mode`: "hybrid" | "dgpu". Hybrid puts the internal panel on the iGPU, so
/// it is refused when the running kernel has no amdgpu driver (black screen
/// at the next boot) unless `force` is set.
pub fn set_gpu_mode(mode: &str, force: bool) -> Value {
    let dgpu = match mode { "dgpu" => true, "hybrid" => false, _ => return json!({"ok": false, "error": "mode must be 'hybrid' or 'dgpu'"}) };
    if !acpi_available() { modprobe_acpi_call(); }
    if !acpi_available() { return json!({"ok": false, "error": "acpi_call is not loaded (modprobe acpi_call)"}); }
    if !wmaa(GZ_GSYNC_SUPPORTED, 0).map_or(false, |v| v != 0) {
        return json!({"ok": false, "error": "the firmware does not report GPU mode switching support"});
    }
    if !dgpu && !force && !crate::tune::kmod_available("amdgpu", "drivers/gpu/drm/amd/amdgpu") {
        return json!({"ok": false, "needs_force": true,
            "error": "this kernel has no amdgpu driver: in hybrid mode the internal display is driven by the AMD iGPU and would stay black. Build amdgpu (CONFIG_DRM_AMDGPU) first."});
    }
    if let Err(e) = wmaa(GZ_GSYNC_SET, dgpu as u64) { return json!({"ok": false, "error": e}); }
    let next = wmaa(GZ_GSYNC_GET, 0).ok().map(|v| v == 1);
    if next != Some(dgpu) {
        return json!({"ok": false, "error": "the firmware did not take the new mode (read-back differs)", "next_boot": next.map(mode_name)});
    }
    let active_dgpu = !amd_igpu_present();
    json!({"ok": true, "active": mode_name(active_dgpu), "next_boot": mode_name(dgpu), "reboot_pending": dgpu != active_dgpu})
}

// ── platform profile ────────────────────────────────────────────────────────

pub fn platform_profile() -> Option<String> {
    std::fs::read_to_string("/sys/firmware/acpi/platform_profile").ok().map(|s| s.trim().to_owned())
}
fn in_custom() -> bool { platform_profile().as_deref() == Some("custom") }

/// Runtime-PM state of the NVIDIA dGPU ("active", "suspended", …), if present.
fn dgpu_runtime_status() -> Option<String> {
    for e in std::fs::read_dir("/sys/bus/pci/devices").ok()?.flatten() {
        let d = e.path();
        let rd = |f: &str| std::fs::read_to_string(d.join(f)).ok().map(|s| s.trim().to_owned());
        if rd("vendor").as_deref() == Some("0x10de") && rd("class").map_or(false, |c| c.starts_with("0x03")) {
            return rd("power/runtime_status");
        }
    }
    None
}

/// nvidia-smi Max/Min Power Limit — advisory only, never a hard limit here.
///
/// Opt-in ({"op":"status","envelope":true}) and skipped while the dGPU is
/// runtime-suspended: nvidia-smi powers the GPU up just to answer, and the
/// plain status poll used to do that on every Firmware Attributes load —
/// including at login, with the app still hidden in the tray. Bounded by a
/// timeout because nvidia-smi can hang on a wedged GPU, and this runs as root
/// inside a pkexec call the GUI is waiting on.
fn nvidia_power_limits() -> Option<(i64, i64)> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    if dgpu_runtime_status().as_deref() == Some("suspended") { return None; }
    let exe = ["/usr/bin/nvidia-smi", "/bin/nvidia-smi", "/opt/bin/nvidia-smi"].iter().find(|p| crate::trusted_path(Path::new(p)))?;
    let mut child = Command::new(exe).args(["-q", "-d", "POWER"]).env_clear().env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || { let mut s = Vec::new(); let _ = (&mut out).take(256 * 1024).read_to_end(&mut s); s });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => { let _ = child.kill(); let _ = child.wait(); return None; }
        }
    }
    let text = String::from_utf8_lossy(&reader.join().ok()?).into_owned();
    let grab = |label: &str| text.lines().find(|l| l.contains(label))
        .and_then(|l| l.split(':').nth(1)).and_then(|v| v.trim().split_whitespace().next())
        .and_then(|n| n.parse::<f64>().ok()).map(|f| f as i64);
    Some((grab("Min Power Limit")?, grab("Max Power Limit")?))
}

// ── read one knob (through its owning channel) ──────────────────────────────

/// The value goes through WMAE: always for the unranged GPU knobs, and as a
/// fallback for an attribute that has disappeared from sysfs.
fn use_wmae(f: &Feat) -> bool { f.via_acpi || (f.id != 0 && sysfs_read(f.attr).is_none()) }

fn read_value(f: &Feat) -> Result<i64, String> {
    if use_wmae(f) { wmae_get(f.id) } else {
        sysfs_read(f.attr).map(|r| r.cur).ok_or_else(|| format!("{}: not present", f.attr))
    }
}

/// 0 is never written: the firmware takes it as "feature off", the kernel then
/// stops exposing the attribute, and only a power-profile reset from Windows
/// brought it back.
const ZERO_REFUSED: &str = "0 is refused: the firmware treats it as \"feature off\" and the attribute disappears from sysfs";

// ── operations ──────────────────────────────────────────────────────────────

pub fn status(want_envelope: bool) -> Value {
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
        let mut o = json!({"label": f.label, "unit": f.unit, "via_acpi": f.via_acpi, "attr": f.attr,
                           "present": sys.is_some(), "wmae_fallback": f.id != 0 && sys.is_none()});
        // Cross-check for the GUI: WMAE read-back of a sysfs-backed limit.
        if !f.via_acpi && f.id != 0 && sys.is_some() && acpi_available() {
            if let Ok(w) = wmae_get(f.id) { o["wmae"] = json!(w); }
        }
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
    // cTGP recommended envelope from nvidia-smi (advisory, opt-in).
    if let Some((minp, maxp)) = want_envelope.then(nvidia_power_limits).flatten() {
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
    let need_acpi = values.keys().any(|k| feat(k).map(use_wmae).unwrap_or(false));
    if need_acpi && !acpi_available() { modprobe_acpi_call(); }

    // Validate the whole request first: a bad value anywhere means nothing
    // is written, instead of a half-applied set of EC power limits.
    let mut plan: Vec<(&String, &'static Feat, i64)> = Vec::with_capacity(values.len());
    for (key, jv) in values {
        let checked = (|| {
            let f = feat(key).ok_or_else(|| format!("unknown knob '{key}'"))?;
            let v = jv.as_i64().ok_or_else(|| format!("{key}: not an integer"))?;
            if v < 1 { return Err(format!("{}: {ZERO_REFUSED}", f.label)); }
            if use_wmae(f) {
                // No firmware range here; a sanity clamp so a typo can't send
                // something absurd to the EC. cTGP is left wide on purpose.
                let cap = if f.key == "ctgp" { CTGP_SANITY_MAX } else { f.hi };
                if v < f.lo || v > cap { return Err(format!("{} must be {}..{cap} {}", f.label, f.lo, f.unit)); }
            }
            Ok((f, v))
        })();
        match checked {
            Ok((f, v)) => plan.push((key, f, v)),
            Err(m) => { all = false; results.push(json!({"what": key, "ok": false, "message": m})); }
        }
    }
    if !all {
        return json!({"ok": false, "results": results, "error": "request rejected; nothing was written"});
    }

    for (key, f, v) in plan {
        let r = (|| {
            if use_wmae(f) {
                if !acpi_available() { return Err("acpi_call not available".into()); }
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
        assert_eq!(feat("spl").unwrap().id, 0x0102_0000);
        assert_eq!(feat("sppt").unwrap().attr, "ppt_pl2_sppt");
        assert!(FEATURES.iter().all(|f| f.lo >= 1), "no knob may allow 0");
    }
    #[test]
    fn wmae_id_le() {
        assert_eq!(0x0202_0000u32.to_le_bytes(), [0, 0, 2, 2]);
        assert_eq!(0x020B_0000u32.to_le_bytes(), [0, 0, 0x0b, 2]);
    }
}

// ── GameZone extras (DSDT of SMCN19/20WW, \_SB.GZFD.WMAA) ──────────────────────
//   0x31 IsSupportOD : 1 only if the panel has Over Drive (PANT bit 1) — the firmware
//                      itself says no on panels without it (e.g. OLED), so we never
//                      offer the toggle there
//   0x32 GetODStatus / 0x33 SetODStatus(0|1): panel GPIO 0x4A + EC 0x7F
//   0x3F IsSupportIGPUMode (3 = supported), 0x40 GetIGPUModeStatus (EC REJF),
//   0x41 SetIGPUModeStatus (EC WEJF): 0 default, 1 iGPU only (dGPU cut off), 2 auto

pub fn panel_extras() -> Value {
    if !acpi_available() { modprobe_acpi_call(); }
    if !acpi_available() { return json!({"ok": false, "error": "acpi_call is not loaded"}); }
    let od_sup = wmaa(0x31, 0).map(|v| v == 1).unwrap_or(false);
    let od = if od_sup { wmaa(0x32, 0).ok().map(|v| v == 1) } else { None };
    let ig_sup = wmaa(0x3F, 0).map(|v| v == 3).unwrap_or(false);
    let ig = if ig_sup { wmaa(0x40, 0).ok().filter(|&v| v <= 2) } else { None };
    json!({"ok": true, "od_supported": od_sup, "od": od, "igpu_supported": ig_sup, "igpu_mode": ig})
}

pub fn set_panel_od(on: bool) -> Value {
    if !acpi_available() { modprobe_acpi_call(); }
    match wmaa(0x31, 0) {
        Ok(1) => {}
        Ok(_) => return json!({"ok": false, "error": "this panel has no Over Drive (firmware reports unsupported — e.g. OLED)"}),
        Err(e) => return json!({"ok": false, "error": e}),
    }
    if let Err(e) = wmaa(0x33, on as u64) { return json!({"ok": false, "error": e}); }
    match wmaa(0x32, 0) {
        Ok(v) if (v == 1) == on => json!({"ok": true, "od": on}),
        Ok(v) => json!({"ok": false, "error": format!("read-back {v} after setting Over Drive")}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

pub fn set_igpu_mode(mode: u64) -> Value {
    if mode > 2 { return json!({"ok": false, "error": "mode must be 0 (default), 1 (iGPU only) or 2 (auto)"}); }
    if !acpi_available() { modprobe_acpi_call(); }
    match wmaa(0x3F, 0) {
        Ok(3) => {}
        Ok(_) => return json!({"ok": false, "error": "iGPU mode not supported by this firmware"}),
        Err(e) => return json!({"ok": false, "error": e}),
    }
    if let Err(e) = wmaa(0x41, mode) { return json!({"ok": false, "error": e}); }
    json!({"ok": true, "igpu_mode": wmaa(0x40, 0).ok()})
}

// ── EC Full Speed ("turbo fan") via WMAE ────────────────────────────────────
//
// WMAE feature 0x04020000 = EC field FNST (byte 0x8B bit 0, right after the
// F9F0..F9FA fan-table bytes). Get 0x11 returns 0/1; set 0x12 writes the bit
// under the firmware's own LFCM mutex. Verified on the 16AFR10H (SMCN19WW):
// 1 → fans 1800/1800/2500 → 5300/5500/7400 RPM within seconds, 0 → back to the
// EC curve. The bit survives reboots (Windows' Full Speed switch is the same
// flag), which is why a kernel without pwm1_enable/legion_laptop could see the
// fans stuck at max and not clear them. Used only when neither sysfs backend
// exists.

const FEAT_FAN_FULLSPEED: u32 = 0x0402_0000;

/// Some(on) when the firmware answers the FNST getter with 0/1, None otherwise
/// (no acpi_call, no \_SB.GZFD.WMAE, or an unexpected value → not supported).
pub fn fan_fullspeed_get() -> Result<bool, String> {
    if !acpi_available() { modprobe_acpi_call(); }
    if !acpi_available() { return Err("acpi_call is not loaded (modprobe acpi_call)".into()); }
    match wmae_get(FEAT_FAN_FULLSPEED)? {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(format!("WMAE full-speed getter returned {v}; not a Legion FNST interface")),
    }
}

/// Writes FNST and returns the read-back state.
pub fn fan_fullspeed_set(on: bool) -> Result<bool, String> {
    fan_fullspeed_get()?; // capability check before any write
    wmae_set(FEAT_FAN_FULLSPEED, on as i64)?;
    let now = fan_fullspeed_get()?;
    if now != on { return Err(format!("EC kept full speed {} after the write", if now { "on" } else { "off" })); }
    Ok(now)
}
