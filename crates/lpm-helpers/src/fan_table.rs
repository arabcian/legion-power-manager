//! Lenovo Legion Custom-mode fan table, detected by capability rather than model.
//!
//! LENOVO_FAN_METHOD (WMI 92549549-4bde-4f06-ac04-ce8bf898dbaa) is the interface Legion
//! Space / Vantage use on Legion machines; its MOF defines
//!     method 5  Fan_Get_Table: u32 count, then count x u32 levels
//!     method 6  Fan_Set_Table: FSTM u8, FSID u8, FSTL u32, then 10 x u16 levels
//! Nothing here is tied to one model:
//!   * the ACPI method is found through the WMI bus: the block's ACPI device path
//!     (firmware_node/path) + "WM" + its object_id (e.g. \_SB.GZFD.WMAB);
//!   * the machine must answer method 5 with a sane 10-level table, or it is refused;
//!   * every write is read back through method 5, and a mismatch restores the old table.
//! Per-fan RPM/temperature steps (LENOVO_FAN_TABLE_DATA) are parsed from the DSDT
//! when the firmware stores them as static buffers; the curve works without them.
//!
//! Verified on Legion Pro 7 16AFR10H (SMCN19WW/SMCN20WW): the EC keeps the table across
//! reboots, AC changes and profile switches, and follows it in Custom mode (AC only).
//!
//! Goes through acpi_call, whose reply is cut at 256 chars: method 5's buffer prints as
//! "0xNN, " per byte, so the reply stops around byte 42 — past byte 40, the low byte of
//! the last level, which is all we need (levels are 1..10).

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const ACPI_CALL: &str = "/proc/acpi/call";
const FAN_METHOD_GUID: &str = "92549549-4BDE-4F06-AC04-CE8BF898DBAA";
const WMI_DEVICES: &str = "/sys/bus/wmi/devices";
const DSDT: &str = "/sys/firmware/acpi/tables/DSDT";
const LEVELS: usize = 10;


/// One acpi_call transaction through legion_wmi::acpi_raw: a single
/// descriptor and an exclusive flock around the write→read pair. The module has
/// one global result buffer, so an unlocked caller could read the reply of a
/// concurrent WMAE/WMAA call (Home tab polls) — or have its own reply stolen.
fn acpi_call(cmd: &str) -> Result<String, String> {
    if !Path::new(ACPI_CALL).exists() { crate::modprobe("acpi_call"); }
    if !Path::new(ACPI_CALL).exists() {
        return Err("/proc/acpi/call missing — install the acpi_call module (sys-power/acpi_call)".into());
    }
    crate::legion_wmi::acpi_raw(cmd)
}

/// ACPI path of the LENOVO_FAN_METHOD block's method, e.g. `\_SB.GZFD.WMAB`.
pub fn fan_method_path() -> Result<String, String> {
    let dev = fs::read_dir(WMI_DEVICES).map_err(|e| format!("{WMI_DEVICES}: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().and_then(|n| n.to_str())
            .is_some_and(|n| n.to_ascii_uppercase().starts_with(FAN_METHOD_GUID)))
        .ok_or("no LENOVO_FAN_METHOD WMI interface (not a Lenovo Legion, or lenovo WMI not exposed)")?;
    let oid = fs::read_to_string(dev.join("object_id")).map(|s| s.trim().to_string())
        .map_err(|_| "LENOVO_FAN_METHOD has no object_id (not a method block)".to_string())?;
    if oid.len() != 2 || !oid.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("unexpected WMI object_id '{oid}'"));
    }
    // Walk up from the WMI device to the PNP0C14 device that owns the ACPI node.
    let mut cur: Option<PathBuf> = fs::canonicalize(&dev).ok();
    while let Some(p) = cur {
        if let Ok(path) = fs::read_to_string(p.join("firmware_node/path")) {
            let path = path.trim();
            if path.starts_with('\\') && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"\\._".contains(&b)) {
                // firmware_node/path pads segments to 4 chars ("\\_SB_.GZFD"); use the
                // short form ("\\_SB.GZFD") that acpi_call was verified with.
                let path: Vec<String> = path.split('.')
                    .map(|seg| { let t = seg.trim_end_matches('_'); if t.is_empty() || t == "\\" { seg.to_string() } else { t.to_string() } })
                    .collect();
                return Ok(format!("{}.WM{oid}", path.join(".")));
            }
        }
        cur = p.parent().filter(|q| q.starts_with("/sys/devices")).map(Path::to_path_buf);
    }
    Err("cannot find the ACPI device behind LENOVO_FAN_METHOD".into())
}

fn parse_bytes(out: &str) -> Vec<u8> {
    out.split(|c: char| c == ',' || c == '{' || c == '}' || c.is_whitespace())
        .filter_map(|t| t.strip_prefix("0x").and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect()
}

/// Fan_Get_Table through the method; refuses anything that is not a sane 10-level table.
pub fn read_levels(method: &str) -> Result<Vec<u8>, String> {
    let mut out = acpi_call(&format!("{method} 0 0x05 0"))?;
    if !out.starts_with('{') {
        // another acpi_call user may have raced us between write and read: retry once
        out = acpi_call(&format!("{method} 0 0x05 0"))?;
    }
    let b = parse_bytes(&out);
    // count dword + 10 dwords; the tail may be cut by acpi_call, byte 40 is the last we need
    if b.len() < 4 + 4 * (LEVELS - 1) + 1 {
        let raw: String = out.chars().take(120).collect();
        return Err(format!("Fan_Get_Table via {method}: reply too short ({} bytes): '{raw}'", b.len()));
    }
    let count = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    if count as usize != LEVELS {
        return Err(format!("Fan_Get_Table reports {count} points, this editor handles {LEVELS}"));
    }
    let levels: Vec<u8> = (0..LEVELS).map(|i| b[4 + 4 * i]).collect();
    // bytes 1..3 of each dword that made it through must be zero
    for i in 0..LEVELS {
        for k in 1..4 {
            if let Some(&x) = b.get(4 + 4 * i + k) {
                if x != 0 { return Err(format!("Fan_Get_Table: level {i} is not a small integer")); }
            }
        }
    }
    if levels.iter().any(|&v| !(1..=10).contains(&v)) {
        return Err(format!("Fan_Get_Table returned out-of-range levels {levels:?}"));
    }
    Ok(levels)
}

pub fn validate(levels: &[u8]) -> Result<(), String> {
    if levels.len() != LEVELS { return Err(format!("need exactly {LEVELS} levels")); }
    if levels.iter().any(|&v| !(1..=10).contains(&v)) { return Err("levels must be 1..10".into()); }
    if levels.windows(2).any(|w| w[1] < w[0]) {
        return Err("levels must not decrease (a hotter step may never get a slower fan)".into());
    }
    Ok(())
}

fn set_raw(method: &str, levels: &[u8]) -> Result<(), String> {
    let mut buf = [0u8; 32];
    buf[0] = 1; // FSTM
    for (i, &v) in levels.iter().enumerate() { buf[6 + 2 * i] = v; }
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    acpi_call(&format!("{method} 0 0x06 b{hex}")).map(|_| ())
}

pub fn write_levels(method: &str, levels: &[u8]) -> Result<Vec<u8>, String> {
    validate(levels)?;
    let before = read_levels(method)?;
    set_raw(method, levels)?;
    let back = read_levels(method)?;
    if back != levels {
        let restored = set_raw(method, &before).is_ok() && read_levels(method).ok().as_deref() == Some(&before[..]);
        return Err(format!("the firmware did not take the table (read back {back:?}); {}. \
            Extreme mode keeps a fixed table — switch to Custom.",
            if restored { "previous table restored" } else { "could NOT restore the previous table" }));
    }
    Ok(back)
}

/// Custom-mode (0xFF) rows of LENOVO_FAN_TABLE_DATA if the DSDT holds them as static
/// buffers: {u8 mode, u8 0, u16 fan, u32 10, u16 rpm[10], u32 sensor, u32 10, u16 temp[10]}.
pub fn fan_data() -> Result<Vec<Value>, String> {
    let aml = fs::read(DSDT).map_err(|e| format!("{DSDT}: {e}"))?;
    let u16at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<(u16, u32)> = Vec::new();
    for i in 0..aml.len().saturating_sub(56) {
        let b = &aml[i..i + 56];
        if b[0] != 0xFF || b[1] != 0 || u32at(b, 4) != 10 || u32at(b, 32) != 10 { continue; }
        let (fid, sensor) = (u16at(b, 2), u32at(b, 28));
        let rpm: Vec<u16> = (0..10).map(|k| u16at(b, 8 + 2 * k)).collect();
        let temp: Vec<u16> = (0..10).map(|k| u16at(b, 36 + 2 * k)).collect();
        let sane = (1..=16).contains(&fid) && sensor < 64
            && rpm.iter().all(|&r| (300..=12000).contains(&r)) && rpm.windows(2).all(|w| w[0] <= w[1])
            && temp.iter().all(|&t| (10..=127).contains(&t)) && temp.windows(2).all(|w| w[0] <= w[1]);
        if !sane || seen.contains(&(fid, sensor)) { continue; }
        seen.push((fid, sensor));
        out.push(json!({"fan": fid, "sensor": sensor, "rpm": rpm, "temp": temp}));
    }
    out.sort_by_key(|v| v["fan"].as_u64());
    if out.is_empty() { return Err("no static fan table data in the DSDT (RPM/°C columns unavailable)".into()); }
    Ok(out)
}

pub fn handle(op: &str, levels: Option<&Value>) -> Value {
    let method = match fan_method_path() {
        Ok(m) => m,
        Err(e) => return json!({"ok": false, "unsupported": true, "error": e}),
    };
    let res = match op {
        "get" => read_levels(&method).map_err(|e| (true, e)),
        "set" => {
            let lv: Option<Vec<u8>> = levels.and_then(Value::as_array).and_then(|a| a.iter()
                .map(|v| v.as_u64().filter(|&x| x <= 255).map(|x| x as u8)).collect());
            match lv {
                // a machine that fails the read probe is unsupported, not merely failing
                Some(lv) => match read_levels(&method) {
                    Err(e) => Err((true, e)),
                    Ok(_) => write_levels(&method, &lv).map_err(|e| (false, e)),
                },
                None => Err((false, "'levels' must be 10 integers".into())),
            }
        }
        _ => Err((false, format!("unknown fan_table op '{op}'"))),
    };
    match res {
        Ok(lv) => {
            let mut v = json!({"ok": true, "levels": lv, "method": method});
            match fan_data() { Ok(f) => v["fans"] = json!(f), Err(e) => v["fans_error"] = json!(e) }
            v
        }
        Err((unsupported, e)) => json!({"ok": false, "unsupported": unsupported, "error": e}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation() {
        assert!(validate(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).is_ok());
        assert!(validate(&[1, 2, 3, 4, 6, 7, 8, 9, 10, 10]).is_ok());
        assert!(validate(&[2, 1, 3, 4, 5, 6, 7, 8, 9, 10]).is_err());
        assert!(validate(&[0, 1, 3, 4, 5, 6, 7, 8, 9, 10]).is_err());
        assert!(validate(&[1, 2, 3]).is_err());
    }
    #[test]
    fn truncated_reply() {
        // what acpi_call actually printed on the 16AFR10H (cut at 256 chars)
        let out = "{0x0a, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00, \
            0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x07, \
            0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x0a, 0x00,";
        let b = parse_bytes(out);
        assert_eq!(b.len(), 42);
        assert_eq!((0..10).map(|i| b[4 + 4 * i]).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    }
}
