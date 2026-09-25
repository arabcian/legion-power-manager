//! Read-only DDR5 SPD decode (JESD400-5) from the kernel's spd5118 driver.
//!
//! Model-independent: any machine whose SMBus the kernel scans (i2c_piix4 on AMD)
//! exposes /sys/bus/i2c/drivers/spd5118/<bus>-<addr>/eeprom. Nothing is written.
//! These are the module's own JEDEC base timings, not what the memory controller
//! actually runs; live UMC timings are a separate, later step.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const DRV: &str = "/sys/bus/i2c/drivers/spd5118";

fn modprobe(m: &str) { crate::modprobe(m); }

fn maker(bank: u8, id: u8) -> &'static str {
    match ((bank & 0x7F) as u16) << 8 | id as u16 {
        0x00AD => "SK hynix",
        0x00CE => "Samsung",
        0x002C => "Micron",
        0x0198 => "Kingston",
        0x029E => "Corsair",
        0x04CD => "G.Skill",
        0x0325 => "Kingmax",
        0x06C1 => "ADATA",
        0x0B83 => "Crucial",
        _ => "",
    }
}

/// Cycles at tCK for a minimum time, with JEDEC's 2.5 % rounding guard.
fn cycles(t_ps: u32, tck_ps: u32) -> u32 {
    if tck_ps == 0 { return 0; }
    ((t_ps as u64 * 1000 / tck_ps as u64 + 974) / 1000) as u32
}

pub fn decode(b: &[u8]) -> Result<Value, String> {
    // Highest byte used below is 553 (DRAM manufacturer ID).
    if b.len() < 554 { return Err(format!("SPD too short ({} bytes)", b.len())); }
    if b[2] != 0x12 { return Err(format!("not DDR5 SPD (type byte {:#04x})", b[2])); }
    let w = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u32;
    let tck = w(20);
    let mt = if tck > 0 { 2_000_000 / tck } else { 0 };
    let form = match b[3] & 0x0F { 1 => "RDIMM", 2 => "UDIMM", 3 => "SODIMM", 4 => "LRDIMM", 0x0B => "CAMM2", _ => "?" };
    let die_gb = match b[4] & 0x1F { 1 => 4, 2 => 8, 3 => 12, 4 => 16, 5 => 24, 6 => 32, 7 => 48, 8 => 64, _ => 0 };
    let part = String::from_utf8_lossy(&b[521..551]).trim().to_string();
    let t = |o: usize| { let ps = w(o); json!({"ns": ps as f64 / 1000.0, "clk": cycles(ps, tck)}) };
    let expo = b.len() >= 0x344 && &b[0x340..0x344] == b"EXPO";
    Ok(json!({
        "form": form,
        "die_density_gbit": die_gb,
        "manufacturer": maker(b[512], b[513]),
        "manufacturer_id": format!("{:02X}{:02X}", b[512] & 0x7F, b[513]),
        "dram_manufacturer": maker(b[552], b[553]),
        "part": part,
        "tck_ps": tck,
        "speed_mts": mt,
        "tAA": t(30), "tRCD": t(32), "tRP": t(34), "tRAS": t(36), "tRC": t(38), "tWR": t(40),
        "tRFC1_ns": w(42), "tRFC2_ns": w(44), "tRFCsb_ns": w(46),
        "expo": expo,
    }))
}

pub fn read_all() -> Value {
    if !Path::new(DRV).is_dir() { modprobe("spd5118"); }
    let mut mods = Vec::new();
    let mut errs = Vec::new();
    if let Ok(rd) = fs::read_dir(DRV) {
        let mut devs: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path())
            .filter(|p| p.join("eeprom").is_file()).collect();
        devs.sort();
        for d in devs {
            let slot = d.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
            match fs::read(d.join("eeprom")).map_err(|e| e.to_string()).and_then(|b| decode(&b)) {
                Ok(mut v) => { v["slot"] = json!(slot); mods.push(v); }
                Err(e) => errs.push(format!("{slot}: {e}")),
            }
        }
    }
    if mods.is_empty() {
        let why = if errs.is_empty() {
            "no DDR5 SPD found — needs the spd5118 driver and a scanned SMBus (i2c_piix4 / i2c_i801)".to_string()
        } else { errs.join("; ") };
        return json!({"ok": false, "error": why});
    }
    json!({"ok": true, "modules": mods, "errors": errs})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_5600_cl46() {
        let mut b = vec![0u8; 1024];
        b[2] = 0x12; b[3] = 0x03; b[4] = 0x04;
        b[20..22].copy_from_slice(&357u16.to_le_bytes());      // DDR5-5600
        b[30..32].copy_from_slice(&16_000u16.to_le_bytes());   // tAA 16 ns at 357 ps -> 44.8 -> CL 45
        b[512] = 0x80; b[513] = 0xAD;
        b[521..529].copy_from_slice(b"HMCG88AG");
        let v = decode(&b).unwrap();
        assert_eq!(v["speed_mts"], 5602);
        assert_eq!(v["form"], "SODIMM");
        assert_eq!(v["manufacturer"], "SK hynix");
        assert_eq!(v["tAA"]["clk"], 45);
    }
}

// ── live timings from the AMD memory controller (UMC) ─────────────────────────
//
// AM5-family DDR5 UMC register map (Zen 4 / Zen 5), channel 0 at SMN 0x50000,
// channel N at +N*0x100000. Every field below was checked against ZenTimings 1.39
// on a Ryzen 9 9955HX3D (Fire Range, SMCN19WW) with hand-tuned BIOS timings.
// tRFCsb is deliberately absent: its register is not in this block.
// Read through ryzen_smu's SMN interface; nothing is written to the UMC.

const SMN: &str = "/sys/kernel/ryzen_smu_drv/smn";
const UMC0: u32 = 0x50000;
const UMC_STRIDE: u32 = 0x100000;

/// (name, register offset from UMC base + 0x200 block, low bit, width)
const UMC_FIELDS: &[(&str, u32, u32, u32)] = &[
    ("tCL", 0x204, 0, 6), ("tRAS", 0x204, 8, 7), ("tRCDRD", 0x204, 16, 6), ("tRCDWR", 0x204, 24, 6),
    ("tRC", 0x208, 0, 8), ("tRP", 0x208, 16, 6),
    ("tRRDS", 0x20C, 0, 5), ("tRRDL", 0x20C, 8, 5), ("tRTP", 0x20C, 24, 5),
    ("tFAW", 0x210, 0, 8),
    ("tCWL", 0x214, 0, 6), ("tWTRS", 0x214, 8, 5), ("tWTRL", 0x214, 16, 7),
    ("tWR", 0x218, 0, 8),
    ("tRdRdScl", 0x220, 24, 6), ("tRdRdSc", 0x220, 16, 4), ("tRdRdSd", 0x220, 8, 4), ("tRdRdDd", 0x220, 0, 4),
    ("tWrWrScl", 0x224, 24, 6), ("tWrWrSc", 0x224, 16, 4), ("tWrWrSd", 0x224, 8, 4), ("tWrWrDd", 0x224, 0, 4),
    ("tWrRd", 0x228, 0, 4), ("tRdWr", 0x228, 8, 6),
    ("tREFI", 0x230, 0, 16),
    ("tMOD", 0x234, 0, 8), ("tMRD", 0x234, 8, 8), ("tMODPDA", 0x234, 16, 8), ("tMRDPDA", 0x234, 24, 8),
    ("tSTAG", 0x250, 16, 8),
    ("tPHYWRL", 0x258, 8, 8), ("tPHYRDL", 0x258, 16, 8), ("tPHYWRD", 0x258, 24, 8),
    ("tRFC1", 0x260, 0, 11), ("tRFC2", 0x260, 16, 11),
];

/// One SMN read: address write + value read on ONE descriptor under an
/// exclusive flock. ryzen_smu keeps the SMN address in a single driver-global
/// slot, so two unsynchronised readers (two helper calls, a monitor) could read
/// each other's register — and aod_verify trusts these values before an EFI write.
fn smn_read(addr: u32) -> Result<u32, String> {
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    let f = fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(SMN)
        .map_err(|e| format!("smn open: {e}"))?;
    let _lock = crate::FdLock::exclusive(&f).map_err(|e| format!("smn lock: {e}"))?;
    if f.write_at(&addr.to_le_bytes(), 0).map_err(|e| format!("smn write: {e}"))? != 4 {
        return Err("smn write: short write".into());
    }
    let mut b = [0u8; 4];
    if f.read_at(&mut b, 0).map_err(|e| format!("smn read: {e}"))? != 4 {
        return Err("smn read: short read".into());
    }
    Ok(u32::from_le_bytes(b))
}

fn amd_family() -> Option<u32> {
    let info = fs::read_to_string("/proc/cpuinfo").ok()?;
    if !info.lines().any(|l| l.starts_with("vendor_id") && l.contains("AuthenticAMD")) { return None; }
    info.lines().find(|l| l.starts_with("cpu family"))
        .and_then(|l| l.split(':').nth(1)).and_then(|v| v.trim().parse().ok())
}

pub fn decode_umc(regs: &dyn Fn(u32) -> Result<u32, String>) -> Result<Value, String> {
    let mclk = regs(0x200)? & 0xFFFF;
    let mut t = serde_json::Map::new();
    for &(name, off, lo, w) in UMC_FIELDS {
        t.insert(name.into(), json!((regs(off)? >> lo) & ((1u32 << w) - 1)));
    }
    let g = |k: &str| t[k].as_u64().unwrap_or(0);
    let (cl, cwl, rc, ras, rp, rfc1, rfc2) = (g("tCL"), g("tCWL"), g("tRC"), g("tRAS"), g("tRP"), g("tRFC1"), g("tRFC2"));
    // Sanity: the map must describe a DDR5 controller running plausible timings.
    // Loose on purpose: tCWL and tRC are user-tunable, so only physically
    // impossible combinations are rejected.
    if !(800..=6000).contains(&mclk) || !(10..=80).contains(&cl) || cwl > cl || cwl * 2 < cl
        || rc < ras || rp == 0 || ras == 0 {
        return Err(format!("UMC registers do not look like DDR5 timings on this CPU (MCLK {mclk}, tCL {cl}, tCWL {cwl}) — \
            register map not verified here"));
    }
    let ns = |clk: u64| (clk as f64 * 1000.0 / mclk as f64 * 100.0).round() / 100.0;
    t.insert("tRFC1_ns".into(), json!(ns(rfc1)));
    t.insert("tRFC2_ns".into(), json!(ns(rfc2)));
    Ok(json!({"mclk_mhz": mclk, "speed_mts": mclk * 2, "timings": t}))
}

pub fn read_umc() -> Result<Value, String> {
    match amd_family() {
        Some(f) if f >= 0x19 => {}
        Some(f) => return Err(format!("CPU family {f:#x}: UMC map only verified for AM5-generation (Zen 4/5)")),
        None => return Err("not an AMD CPU: live timings are read from the AMD memory controller".into()),
    }
    if !Path::new(SMN).exists() { modprobe("ryzen_smu"); }
    if !Path::new(SMN).exists() { return Err("ryzen_smu not loaded (needed for live timings)".into()); }
    let ch0 = decode_umc(&|off| smn_read(UMC0 + off))?;
    // Second channel: report whether it runs the same timings.
    let ch1 = decode_umc(&|off| smn_read(UMC0 + UMC_STRIDE + off)).ok();
    let mut v = ch0.clone();
    v["channels_match"] = json!(ch1.as_ref().map(|c| c["timings"] == ch0["timings"]));
    Ok(v)
}

pub fn read_everything() -> Value {
    let mut v = read_all();
    let spd_ok = v["ok"].as_bool().unwrap_or(false);
    if !spd_ok {
        v["spd_error"] = v["error"].take();
        v.as_object_mut().map(|o| o.remove("error"));
    }
    match read_umc() {
        Ok(u) => v["umc"] = u,
        Err(e) => v["umc_error"] = json!(e),
    }
    let ok = spd_ok || v.get("umc").is_some();
    v["ok"] = json!(ok);
    if !ok {
        v["error"] = json!(format!("{}; {}", v["spd_error"].as_str().unwrap_or(""), v["umc_error"].as_str().unwrap_or("")));
    }
    v
}

#[cfg(test)]
mod umc_tests {
    use super::*;
    #[test]
    fn decodes_zentimings_capture() {
        // SMN dump from a 9955HX3D, checked against ZenTimings 1.39.
        let regs = |off: u32| -> Result<u32, String> {
            Ok(match off {
                0x200 => 0x80050af0, 0x204 => 0x26263c26, 0x208 => 0x00260062, 0x20C => 0x10000c08,
                0x210 => 0x18, 0x214 => 0x000e0824, 0x218 => 0x36, 0x220 => 0x06010707, 0x224 => 0x0c010c0c,
                0x228 => 0x1008, 0x230 => 0x00c02a8c, 0x234 => 0x20202828, 0x250 => 0x70000,
                0x258 => 0x0622181a, 0x260 => 0x01900230, _ => 0,
            })
        };
        let v = decode_umc(&regs).unwrap();
        let t = &v["timings"];
        assert_eq!(v["speed_mts"], 5600);
        for (k, want) in [("tCL", 38), ("tRCDRD", 38), ("tRCDWR", 38), ("tRP", 38), ("tRAS", 60), ("tRC", 98),
                          ("tRRDS", 8), ("tRRDL", 12), ("tFAW", 24), ("tWTRS", 8), ("tWTRL", 14), ("tWR", 54),
                          ("tRTP", 16), ("tCWL", 36), ("tRdWr", 16), ("tWrRd", 8), ("tRdRdScl", 6), ("tRdRdSc", 1),
                          ("tRdRdSd", 7), ("tRdRdDd", 7), ("tWrWrScl", 12), ("tWrWrSc", 1), ("tWrWrSd", 12),
                          ("tWrWrDd", 12), ("tREFI", 10892), ("tMOD", 40), ("tMRD", 40), ("tMODPDA", 32),
                          ("tMRDPDA", 32), ("tSTAG", 7), ("tPHYWRD", 6), ("tPHYRDL", 34), ("tPHYWRL", 24),
                          ("tRFC1", 560), ("tRFC2", 400)] {
            assert_eq!(t[k], want, "{k}");
        }
        assert_eq!(t["tRFC1_ns"], 200.0);
    }
}

// ── AOD (AMD Overclocking) setup variable: boot-time DRAM timing overrides ────
//
// Mapped on Legion Pro 7 16AFR10H (SMCN19WW) by changing one timing in the BIOS
// and diffing the variable. Data (after efivarfs' 4 attribute bytes) at 0x3C holds
// 26 records of {u8 mode (1 = manual, 0 = auto), u16 value LE}: MEMCLK speed, then
// AMD CBS's DDR5 order. Only records whose stored manual value matches the live
// UMC value were verified; only those are editable. tRCD's record holds 8 while the
// controller runs 38, so its meaning is unknown and it is left alone.
// Takes effect on the next boot; the firmware ignores it while
// AmdVariableProtection is on.

const AOD_VAR: &str = "/sys/firmware/efi/efivars/AodSetupRpl-5ed15dc0-edef-4161-9151-6014c4cc630c";
const AOD_SIZE: usize = 804;
const AOD_BASE: usize = 4 + 0x3C;          // file offset of the speed record
const BACKUP_DIR: &str = "/var/lib/legion-power-manager";

/// (name, record index, min, max). Index 0 is MEMCLK speed (not editable).
const AOD_FIELDS: &[(&str, usize, u16, u16)] = &[
    ("tCL", 1, 22, 80), ("tRP", 3, 22, 80), ("tRAS", 4, 30, 127), ("tRC", 5, 60, 255),
    ("tWR", 6, 24, 127), ("tRFC1", 7, 200, 2000), ("tRFC2", 8, 150, 1500), ("tRFCsb", 9, 150, 1500),
    ("tRTP", 10, 6, 40), ("tRRDL", 11, 4, 40), ("tRRDS", 12, 4, 40), ("tFAW", 13, 16, 80),
    ("tWTRL", 14, 6, 60), ("tWTRS", 15, 2, 30), ("tRdRdScl", 16, 1, 15), ("tWrWrScl", 20, 1, 63),
    ("tWrWrSd", 22, 1, 31), ("tWrWrDd", 23, 1, 31), ("tRdWr", 25, 4, 63),
];

fn aod_supported() -> Result<(), String> {
    let model = format!("{} {}", dmi("product_version"), dmi("product_family"));
    if !model.contains("16AFR10H") || !dmi("bios_version").starts_with("SMCN") {
        return Err("editing is only mapped for Legion Pro 7 16AFR10H (SMCN BIOS); this machine is view-only".into());
    }
    Ok(())
}

fn dmi(n: &str) -> String {
    fs::read_to_string(format!("/sys/class/dmi/id/{n}")).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn aod_load() -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut b = Vec::new();
    fs::File::open(AOD_VAR).and_then(|mut f| f.read_to_end(&mut b)).map_err(|e| format!("AodSetupRpl: {e}"))?;
    if b.len() != AOD_SIZE { return Err(format!("AodSetupRpl is {} bytes, mapped layout is {AOD_SIZE}", b.len())); }
    Ok(b)
}

fn rec(b: &[u8], i: usize) -> (u8, u16) {
    let o = AOD_BASE + 3 * i;
    (b[o], u16::from_le_bytes([b[o + 1], b[o + 2]]))
}

/// Layout check against the running controller: speed and tRAS must agree.
fn aod_verify(b: &[u8]) -> Result<Value, String> {
    let live = read_umc()?;
    let (_, speed) = rec(b, 0);
    let (_, ras) = rec(b, 4);
    if speed as u64 != live["speed_mts"].as_u64().unwrap_or(0) || ras as u64 != live["timings"]["tRAS"].as_u64().unwrap_or(0) {
        return Err(format!("AodSetupRpl does not match the running controller (speed {speed}, tRAS {ras}) — layout not trusted"));
    }
    Ok(live)
}

const PROT_VAR: &str = "/sys/firmware/efi/efivars/AmdVariableProtection-408f573d-65ee-49ed-8bc5-5a32bbeae745";

/// AmdVariableProtection data byte (after the 4 attribute bytes). Observed 1 while the
/// BIOS option is enabled; any non-zero is treated as "protected".
fn protection() -> Option<bool> {
    fs::read(PROT_VAR).ok().filter(|b| b.len() >= 5).map(|b| b[4] != 0)
}

pub fn aod_get() -> Value {
    if let Err(e) = aod_supported() { return json!({"ok": false, "editable": false, "error": e}); }
    let b = match aod_load() { Ok(b) => b, Err(e) => return json!({"ok": false, "editable": false, "error": e}) };
    if let Err(e) = aod_verify(&b) { return json!({"ok": false, "editable": false, "error": e}); }
    let fields: Vec<Value> = AOD_FIELDS.iter().map(|&(n, i, lo, hi)| {
        let (mode, v) = rec(&b, i);
        json!({"name": n, "manual": mode == 1, "value": v, "min": lo, "max": hi})
    }).collect();
    json!({"ok": true, "editable": true, "speed_mts": rec(&b, 0).1, "fields": fields, "protected": protection(),
           "backup": latest_backup().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))})
}

fn set_immutable(path: &str, on: bool) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;
    const GET: libc::c_ulong = 0x8008_6601;
    const SET: libc::c_ulong = 0x4008_6602;
    const IMM: libc::c_long = 0x10;
    let f = fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut flags: libc::c_long = 0;
    unsafe {
        if libc::ioctl(f.as_raw_fd(), GET as _, &mut flags) != 0 { return Err("FS_IOC_GETFLAGS failed".into()); }
        flags = if on { flags | IMM } else { flags & !IMM };
        if libc::ioctl(f.as_raw_fd(), SET as _, &flags) != 0 { return Err("FS_IOC_SETFLAGS failed".into()); }
    }
    Ok(())
}

/// Backups: AodSetupRpl-<secs>-<nanos>.bin (+ .json with the BIOS version).
fn backup_files() -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = fs::read_dir(BACKUP_DIR).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str())
            .map_or(false, |n| n.starts_with("AodSetupRpl-") && n.ends_with(".bin") && !n.contains(".prerestore")))
        .filter(|p| fs::symlink_metadata(p).map_or(false, |m| m.is_file() && m.len() == AOD_SIZE as u64))
        .collect();
    // Names sort by time: seconds are zero-padded in new names; old names
    // (AodSetupRpl-<secs>.bin) have the same digit count until 2286.
    v.sort();
    v
}

fn latest_backup() -> Option<std::path::PathBuf> { backup_files().pop() }

/// Saves `data` as a new, never-overwritten backup, synced to disk before the
/// caller touches the variable (the machine may not boot again afterwards).
fn save_backup(data: &[u8], tag: &str) -> Result<String, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    crate::secure_dir(BACKUP_DIR)?;
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let stem = format!("{BACKUP_DIR}/AodSetupRpl-{:010}-{:09}", t.as_secs(), t.subsec_nanos());
    let bin = format!("{stem}{tag}.bin");
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&bin).map_err(|e| format!("backup {bin}: {e}"))?;
    f.write_all(data).and_then(|_| f.sync_all()).map_err(|e| format!("backup {bin}: {e}"))?;
    let meta = json!({"bios_version": dmi("bios_version"), "product": dmi("product_version")});
    crate::write_root_file(&format!("{stem}{tag}.json"), meta.to_string().as_bytes())?;
    Ok(bin)
}

/// Writes the whole variable (attributes + data), clearing and restoring the
/// immutable flag efivarfs puts on it, then reads it back.
fn aod_write(b: &[u8], backup: &str) -> Result<(), String> {
    set_immutable(AOD_VAR, false)?;
    let w = fs::OpenOptions::new().write(true).open(AOD_VAR)
        .and_then(|mut f| { use std::io::Write; f.write_all(b) });
    let _ = set_immutable(AOD_VAR, true);
    w.map_err(|e| if e.raw_os_error() == Some(libc::EROFS) || e.raw_os_error() == Some(libc::EPERM) {
        "the firmware refused the write: AMD Variable Protection is on. Disable it in the BIOS \
         (advanced menu) and try again — it cannot be turned off from the OS. Nothing changed.".to_string()
    } else { format!("writing AodSetupRpl failed ({e}); nothing changed, backup at {backup}") })?;
    if aod_load()? != b { return Err(format!("read-back differs; backup at {backup}")); }
    Ok(())
}

pub fn aod_set(values: Option<&Value>) -> Value {
    let res = (|| -> Result<Value, String> {
        aod_supported()?;
        let vals = values.and_then(Value::as_object).ok_or("'values' must be an object {name: number}")?;
        let mut b = aod_load()?;
        aod_verify(&b)?;
        let orig = b.clone();
        for (name, v) in vals {
            let &(_, i, lo, hi) = AOD_FIELDS.iter().find(|f| f.0 == name).ok_or(format!("'{name}' is not editable"))?;
            let v = v.as_u64().filter(|&x| x >= lo as u64 && x <= hi as u64)
                .ok_or(format!("{name} must be {lo}..{hi}"))? as u16;
            if name == "tCL" && v % 2 != 0 { return Err("DDR5 tCL must be even".into()); }
            let o = AOD_BASE + 3 * i;
            b[o] = 1;
            b[o + 1..o + 3].copy_from_slice(&v.to_le_bytes());
        }
        let g = |n: &str| AOD_FIELDS.iter().find(|f| f.0 == n).map(|f| rec(&b, f.1).1).unwrap_or(0);
        if g("tRC") < g("tRAS") + g("tRP") { return Err("tRC must be ≥ tRAS + tRP".into()); }
        // Refresh ordering, only between records that are actually in use
        // (manual); an Auto record's stored number is not what the BIOS runs.
        let manual = |n: &str| AOD_FIELDS.iter().find(|f| f.0 == n).map_or(false, |f| rec(&b, f.1).0 == 1);
        for (hi, lo) in [("tRFC1", "tRFC2"), ("tRFC2", "tRFCsb"), ("tRFC1", "tRFCsb")] {
            if manual(hi) && manual(lo) && g(hi) < g(lo) { return Err(format!("{hi} must be ≥ {lo}")); }
        }
        if b == orig { return Ok(json!({"ok": true, "changed": false})); }
        let backup = save_backup(&orig, "")?;
        aod_write(&b, &backup)?;
        Ok(json!({"ok": true, "changed": true, "backup": backup}))
    })();
    res.unwrap_or_else(|e| json!({"ok": false, "error": e}))
}

/// Writes the newest backup back (the variable as it was before the last
/// edit). The current variable is backed up first, so this is reversible too.
pub fn aod_restore() -> Value {
    let res = (|| -> Result<Value, String> {
        aod_supported()?;
        let src = latest_backup().ok_or("no backup in /var/lib/legion-power-manager")?;
        let data = fs::read(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        let cur = aod_load()?;
        if data.len() != AOD_SIZE || data[..4] != cur[..4] {
            return Err("the backup does not match this variable's layout/attributes; refusing".into());
        }
        // A backup taken under another BIOS version may use another layout.
        let meta = fs::read_to_string(src.with_extension("json")).ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if let Some(bios) = meta.as_ref().and_then(|m| m["bios_version"].as_str()) {
            if bios != dmi("bios_version") {
                return Err(format!("the backup was taken under BIOS {bios}, this is {} — refusing", dmi("bios_version")));
            }
        }
        if data == cur {
            return Ok(json!({"ok": true, "changed": false, "restored": src.display().to_string()}));
        }
        // Kept out of the restore chain (.prerestore): a second Restore steps
        // further back instead of undoing the first one.
        let backup = save_backup(&cur, ".prerestore")?;
        aod_write(&data, &backup)?;
        // The restored image is now current: retire it so a second restore
        // steps back further instead of re-applying the same file.
        let _ = fs::rename(&src, src.with_extension("bin.restored"));
        Ok(json!({"ok": true, "changed": true, "restored": src.display().to_string(), "backup": backup}))
    })();
    res.unwrap_or_else(|e| json!({"ok": false, "error": e}))
}
