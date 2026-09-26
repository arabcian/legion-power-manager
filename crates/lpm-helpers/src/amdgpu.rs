//! AMD GPU tuning (amdgpu driver, `pp_od_clk_voltage` and friends).
//!
//! The request describes the *complete* wanted state of one card; the helper
//! validates all of it against the ranges the driver reports right now (never
//! against numbers from the GUI), and only then writes, in an order the
//! driver accepts:
//!
//!   1. performance level → `manual` (older SMUs refuse OD edits otherwise)
//!   2. overdrive table: `r` (back to stock), the edits, `c` (commit)
//!   3. power cap (hwmon `power1_cap`, µW)
//!   4. PMFW fan settings (`gpu_od/fan_ctrl/*`, value + `c` each)
//!   5. power profile (needs `manual` to stick)
//!   6. the wanted performance level
//!
//! A failed overdrive write restores the stock table (`r` + `c`) before the
//! error is reported, so a half-written table is never left committed.
//!
//! Table shapes the parser understands (kernel output differs per SMU):
//!   * per-state (Polaris, Vega10):  `OD_SCLK: 0: 300MHz 750mV …`, range `VDDC:`
//!   * V/F curve (Vega20, Navi1x):   `OD_VDDC_CURVE: 0: 800MHz 716mV`, ranges
//!                                    `VDDC_CURVE_SCLK[i]` / `VDDC_CURVE_VOLT[i]`
//!   * min/max (Navi2x, RDNA3, APUs): `OD_SCLK: 0: 500Mhz 1: 2500Mhz`,
//!                                    optional `OD_VDDGFX_OFFSET: -50mV`
//!   * offset (RDNA4):                `OD_SCLK_OFFSET: 0Mhz`, range `SCLK_OFFSET:`
//! The files are padded with NUL bytes on several ASICs; those are dropped.

use crate::{canonical_in_sysfs, read_trimmed, sysfs_write};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MAX_REQUEST: usize = 16 * 1024;
const PERF_LEVELS: &[&str] = &[
    "auto", "low", "high", "manual",
    "profile_standard", "profile_min_sclk", "profile_min_mclk", "profile_peak",
];
/// Hard ceilings on top of the driver's own ranges (some report 5000 MHz).
const SANE_CLOCK_MHZ: i64 = 4000;
const SANE_MV: i64 = 1300;

// ── table parsing ───────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq)]
pub struct OdTable {
    /// (index, MHz, mV if the state carries a voltage)
    pub sclk: Vec<(u32, i64, Option<i64>)>,
    pub mclk: Vec<(u32, i64, Option<i64>)>,
    pub curve: Vec<(u32, i64, i64)>,
    pub sclk_offset: Option<i64>,
    pub voltage_offset: Option<i64>,
    /// "SCLK", "MCLK", "VDDC", "VDDGFX_OFFSET", "SCLK_OFFSET",
    /// "VDDC_CURVE_SCLK[0]", "VDDC_CURVE_VOLT[0]", …
    pub ranges: BTreeMap<String, (i64, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind { PerState, Curve, MinMax, Offset }

impl OdTable {
    pub fn kind(&self) -> Kind {
        if self.sclk_offset.is_some() { Kind::Offset }
        else if !self.curve.is_empty() { Kind::Curve }
        else if self.sclk.iter().any(|s| s.2.is_some()) { Kind::PerState }
        else { Kind::MinMax }
    }
    fn range(&self, key: &str) -> Option<(i64, i64)> { self.ranges.get(key).copied() }
}

/// First signed integer in `s` ("-450mv" → -450, "2500Mhz" → 2500).
fn num(s: &str) -> Option<i64> {
    let s = s.trim();
    let end = s.char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
        .map_or(s.len(), |(i, _)| i);
    s[..end].parse().ok()
}

/// All signed integers in a line, in order.
fn nums(s: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let neg = b[i] == b'-' && i + 1 < b.len() && b[i + 1].is_ascii_digit();
        if b[i].is_ascii_digit() || neg {
            let start = i;
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() { i += 1; }
            if let Ok(n) = s[start..i].parse() { out.push(n); }
        } else {
            i += 1;
        }
    }
    out
}

pub fn parse_od(text: &str) -> OdTable {
    let clean: String = text.chars().filter(|&c| c != '\0').collect();
    let mut t = OdTable::default();
    let mut section = String::new();
    for raw in clean.lines() {
        let line = raw.trim();
        if line.is_empty() { continue; }
        // Section headers: "OD_SCLK:", "OD_RANGE:" … (nothing after the colon)
        if line.starts_with("OD_") && line.ends_with(':') {
            section = line.trim_end_matches(':').to_owned();
            continue;
        }
        match section.as_str() {
            "OD_SCLK" | "OD_MCLK" => {
                let Some((idx, rest)) = line.split_once(':') else { continue };
                let Ok(idx) = idx.trim().parse::<u32>() else { continue };
                let v = nums(rest);
                let Some(&clk) = v.first() else { continue };
                let entry = (idx, clk, v.get(1).copied());
                if section == "OD_SCLK" { t.sclk.push(entry) } else { t.mclk.push(entry) }
            }
            "OD_VDDC_CURVE" => {
                let Some((idx, rest)) = line.split_once(':') else { continue };
                let (Ok(idx), v) = (idx.trim().parse::<u32>(), nums(rest)) else { continue };
                if v.len() >= 2 { t.curve.push((idx, v[0], v[1])); }
            }
            "OD_SCLK_OFFSET" => t.sclk_offset = num(line),
            "OD_VDDGFX_OFFSET" => t.voltage_offset = num(line),
            "OD_RANGE" => {
                let Some((key, rest)) = line.split_once(':') else { continue };
                let v = nums(rest);
                if v.len() >= 2 { t.ranges.insert(key.trim().to_owned(), (v[0], v[1])); }
            }
            _ => {}
        }
    }
    t
}

/// `NAME:\n<value>\nOD_RANGE:\nKEY: <min> <max>` (PMFW fan_ctrl files).
pub fn parse_fan_value(text: &str) -> Option<(i64, (i64, i64))> {
    let clean: String = text.chars().filter(|&c| c != '\0').collect();
    let mut lines = clean.lines().map(str::trim).filter(|l| !l.is_empty());
    lines.next()?;                       // header
    let value = num(lines.next()?)?;
    lines.find(|l| *l == "OD_RANGE:")?;
    let r = nums(lines.next()?.split_once(':')?.1);
    (r.len() >= 2).then_some((value, (r[0], r[1])))
}

/// `OD_FAN_CURVE:\n0: 45C 30%\n…\nOD_RANGE:\nFAN_CURVE(hotspot temp): 25C 100C\nFAN_CURVE(fan speed): 35% 100%`
pub fn parse_fan_curve(text: &str) -> Option<(Vec<(i64, i64)>, (i64, i64), (i64, i64))> {
    let clean: String = text.chars().filter(|&c| c != '\0').collect();
    let (mut pts, mut temp, mut speed, mut in_range) = (Vec::new(), None, None, false);
    for l in clean.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if l == "OD_RANGE:" { in_range = true; continue; }
        if l.ends_with(':') { continue; }
        let Some((k, rest)) = l.split_once(':') else { continue };
        let v = nums(rest);
        if v.len() < 2 { continue; }
        if !in_range { pts.push((v[0], v[1])); }
        else if k.contains("temp") { temp = Some((v[0], v[1])); }
        else if k.contains("speed") { speed = Some((v[0], v[1])); }
    }
    Some((pts, temp?, speed?))
}

/// Power profiles: `(index, NAME, active)` from either header style
/// (`  1 3D_FULL_SCREEN *:` or ` 1 3D_FULL_SCREEN :` / ` 0 BOOTUP_DEFAULT*:`).
pub fn parse_profiles(text: &str) -> Vec<(u32, String, bool)> {
    let clean: String = text.chars().filter(|&c| c != '\0').collect();
    let mut out = Vec::new();
    for l in clean.lines() {
        let t = l.trim();
        let Some((head, _)) = t.split_once(':') else { continue };
        let mut parts = head.split_whitespace();
        let (Some(idx), Some(name)) = (parts.next(), parts.next()) else { continue };
        let Ok(idx) = idx.parse::<u32>() else { continue };
        let active = head.contains('*');
        let name = name.trim_end_matches('*').to_owned();
        if name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && !name.is_empty() {
            out.push((idx, name, active));
        }
    }
    out
}

// ── device resolution ───────────────────────────────────────────────────────

pub struct Card { pub dev: PathBuf, pub hwmon: Option<PathBuf> }

/// `card` must be `cardN`; its device must be bound to amdgpu (vendor 0x1002).
pub fn resolve(card: &str) -> Result<Card, String> {
    let n = card.strip_prefix("card").ok_or("card must be cardN")?;
    if n.is_empty() || n.len() > 3 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return Err("card must be cardN".into());
    }
    let dev = canonical_in_sysfs(&Path::new("/sys/class/drm").join(card).join("device"))
        .ok_or("no such card")?;
    let driver = std::fs::read_link(dev.join("driver")).map_err(|_| "card has no driver")?;
    if driver.file_name().map_or(true, |d| d != "amdgpu") { return Err("card is not driven by amdgpu".into()); }
    if read_trimmed(&dev.join("vendor")).unwrap_or_default() != "0x1002" { return Err("not an AMD device".into()); }
    let hwmon = std::fs::read_dir(dev.join("hwmon")).ok().and_then(|rd| {
        rd.flatten().map(|e| e.path())
            .find(|p| read_trimmed(&p.join("name")).map_or(false, |n| n == "amdgpu"))
    }).and_then(|p| canonical_in_sysfs(&p)).filter(|p| p.starts_with(&dev));
    Ok(Card { dev, hwmon })
}

/// Writes one command to a file inside the card's own tree.
fn put(card: &Card, rel: &str, data: &str) -> Result<(), String> {
    let p = card.dev.join(rel);
    let real = canonical_in_sysfs(&p).filter(|r| r.starts_with(&card.dev))
        .ok_or_else(|| format!("{rel}: not available"))?;
    sysfs_write(&real, format!("{data}\n").as_bytes()).map_err(|e| format!("{rel} ← \"{data}\": {e}"))
}

fn read(card: &Card, rel: &str) -> Option<String> {
    std::fs::read_to_string(card.dev.join(rel)).ok()
}

// ── request → commands ──────────────────────────────────────────────────────

fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 1e9).map(|f| f as i64))
}

fn pairs(v: &Value) -> Result<Vec<(i64, i64)>, String> {
    let a = v.as_array().ok_or("expected a list of [a, b] pairs")?;
    if a.len() > 16 { return Err("too many points".into()); }
    a.iter().map(|p| {
        let p = p.as_array().filter(|p| p.len() == 2).ok_or("expected [a, b]")?;
        Ok((int(&p[0]).ok_or("not an integer")?, int(&p[1]).ok_or("not an integer")?))
    }).collect()
}

fn check(what: &str, v: i64, r: Option<(i64, i64)>, sane: (i64, i64)) -> Result<(), String> {
    check_cur(what, v, r, sane, None)
}

/// Like `check`, but the value the driver currently reports is always
/// accepted: some firmware ships stock points outside its own OD_RANGE
/// (an RX 5700 XT reports a 716 mV curve point with a 750–1200 mV range),
/// and re-writing an untouched value must not fail.
fn check_cur(what: &str, v: i64, r: Option<(i64, i64)>, sane: (i64, i64), current: Option<i64>) -> Result<(), String> {
    if current == Some(v) { return Ok(()); }
    let (lo, hi) = r.unwrap_or(sane);
    let (lo, hi) = (lo.max(sane.0), hi.min(sane.1));
    if v < lo || v > hi { return Err(format!("{what} {v} is outside [{lo}, {hi}]")); }
    Ok(())
}

/// Overdrive edits for the table's shape; every value checked against OD_RANGE.
pub fn od_commands(t: &OdTable, s: &Map<String, Value>) -> Result<Vec<String>, String> {
    let mut cmds = Vec::new();
    let clock = (0, SANE_CLOCK_MHZ);
    let get = |k: &str| s.get(k).filter(|v| !v.is_null());
    match t.kind() {
        Kind::PerState => {
            for (key, letter, states, range) in [("sclk_states", "s", &t.sclk, "SCLK"), ("mclk_states", "m", &t.mclk, "MCLK")] {
                let Some(v) = get(key) else { continue };
                let want = pairs(v)?;
                if want.len() != states.len() { return Err(format!("{key}: expected {} states", states.len())); }
                let mut prev = 0;
                for (i, (clk, mv)) in want.iter().enumerate() {
                    check_cur(&format!("{key}[{i}] clock"), *clk, t.range(range), clock, Some(states[i].1))?;
                    check_cur(&format!("{key}[{i}] voltage"), *mv, t.range("VDDC"), (400, SANE_MV), states[i].2)?;
                    if *clk < prev { return Err(format!("{key}: clocks must not decrease")); }
                    prev = *clk;
                    cmds.push(format!("{letter} {} {clk} {mv}", states[i].0));
                }
            }
        }
        Kind::Curve => {
            if let Some(v) = get("vddc_curve") {
                let want = pairs(v)?;
                if want.len() != t.curve.len() { return Err(format!("vddc_curve: expected {} points", t.curve.len())); }
                for (i, (clk, mv)) in want.iter().enumerate() {
                    check_cur(&format!("curve point {i} clock"), *clk, t.range(&format!("VDDC_CURVE_SCLK[{i}]")), clock, Some(t.curve[i].1))?;
                    check_cur(&format!("curve point {i} voltage"), *mv, t.range(&format!("VDDC_CURVE_VOLT[{i}]")), (400, SANE_MV), Some(t.curve[i].2))?;
                    if i > 0 && (*clk <= want[i - 1].0 || *mv < want[i - 1].1) {
                        return Err("curve points must rise in clock and not fall in voltage".into());
                    }
                    cmds.push(format!("vc {} {clk} {mv}", t.curve[i].0));
                }
            }
        }
        Kind::MinMax | Kind::Offset => {}
    }
    // min/max clocks (MinMax and Curve share `s 0/1`, `m 0/1`)
    if t.kind() != Kind::PerState {
        for (key, letter, idx, states, range) in [
            ("sclk_min", "s", 0u32, &t.sclk, "SCLK"), ("sclk_max", "s", 1, &t.sclk, "SCLK"),
            ("mclk_min", "m", 0, &t.mclk, "MCLK"), ("mclk_max", "m", 1, &t.mclk, "MCLK"),
        ] {
            let Some(v) = get(key) else { continue };
            let v = int(v).ok_or_else(|| format!("{key} is not an integer"))?;
            let Some(cur) = states.iter().find(|s| s.0 == idx) else { return Err(format!("{key}: this GPU has no such state")) };
            check_cur(key, v, t.range(range), clock, Some(cur.1))?;
            cmds.push(format!("{letter} {idx} {v}"));
        }
        if let (Some(lo), Some(hi)) = (get("sclk_min").and_then(int), get("sclk_max").and_then(int)) {
            if lo > hi { return Err("sclk_min is above sclk_max".into()); }
        }
    }
    if let Some(v) = get("sclk_offset") {
        let v = int(v).ok_or("sclk_offset is not an integer")?;
        if t.sclk_offset.is_none() { return Err("this GPU has no clock offset".into()); }
        check_cur("sclk_offset", v, t.range("SCLK_OFFSET"), (-1000, 1000), t.sclk_offset)?;
        cmds.push(format!("s {v}"));
    }
    if let Some(v) = get("voltage_offset") {
        let v = int(v).ok_or("voltage_offset is not an integer")?;
        if t.voltage_offset.is_none() { return Err("this GPU has no voltage offset".into()); }
        // Undervolt only: a positive offset is never needed for stability tuning.
        let sane = if t.range("VDDGFX_OFFSET").is_some() { (-500, 0) } else { (-250, 0) };
        check_cur("voltage_offset", v, t.range("VDDGFX_OFFSET"), sane, t.voltage_offset.filter(|&c| c <= 0))?;
        cmds.push(format!("vo {v}"));
    }
    Ok(cmds)
}

// ── operations ──────────────────────────────────────────────────────────────

fn od_reset(card: &Card) {
    let _ = put(card, "pp_od_clk_voltage", "r");
    let _ = put(card, "pp_od_clk_voltage", "c");
}

const FAN_DIR: &str = "gpu_od/fan_ctrl";
const FAN_VALUES: &[(&str, &str)] = &[
    ("zero_rpm", "fan_zero_rpm_enable"), ("zero_rpm_stop", "fan_zero_rpm_stop_temperature"),
    ("min_pwm", "fan_minimum_pwm"), ("target_temp", "fan_target_temperature"),
    ("acoustic_limit", "acoustic_limit_rpm_threshold"), ("acoustic_target", "acoustic_target_rpm_threshold"),
];

/// Fan writes as (file, [commands]) after validating every value.
fn fan_commands(card: &Card, f: &Map<String, Value>) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut out = Vec::new();
    for (key, file) in FAN_VALUES {
        let Some(v) = f.get(*key).filter(|v| !v.is_null()) else { continue };
        let v = v.as_bool().map(i64::from).or_else(|| int(v)).ok_or_else(|| format!("fan.{key} is not a number"))?;
        let rel = format!("{FAN_DIR}/{file}");
        let (_, range) = read(card, &rel).as_deref().and_then(parse_fan_value)
            .ok_or_else(|| format!("fan.{key}: not supported by this GPU"))?;
        check(&format!("fan.{key}"), v, Some(range), (i64::MIN, i64::MAX))?;
        out.push((rel, vec![v.to_string(), "c".into()]));
    }
    if let Some(v) = f.get("curve").filter(|v| !v.is_null()) {
        let want = pairs(v)?;
        let rel = format!("{FAN_DIR}/fan_curve");
        let (pts, trange, srange) = read(card, &rel).as_deref().and_then(parse_fan_curve)
            .ok_or("fan.curve: not supported by this GPU")?;
        if want.len() != pts.len() { return Err(format!("fan.curve: expected {} points", pts.len())); }
        let mut cmds = Vec::new();
        for (i, (t, p)) in want.iter().enumerate() {
            check(&format!("fan.curve[{i}] temperature"), *t, Some(trange), (0, 110))?;
            check(&format!("fan.curve[{i}] speed"), *p, Some(srange), (0, 100))?;
            if i > 0 && (*t < want[i - 1].0 || *p < want[i - 1].1) {
                return Err("fan curve must not fall".into());
            }
            cmds.push(format!("{i} {t} {p}"));
        }
        cmds.push("c".into());
        out.push((rel, cmds));
    }
    Ok(out)
}

pub fn apply(card: &Card, s: &Map<String, Value>) -> Result<Vec<String>, String> {
    let has_level = card.dev.join("power_dpm_force_performance_level").exists();
    let level = match s.get("perf_level").and_then(Value::as_str) {
        None => None,
        Some(l) if PERF_LEVELS.contains(&l) => Some(l),
        Some(l) => return Err(format!("unknown performance level {l}")),
    };
    // Profile: validated against the list the driver prints; CUSTOM needs
    // heuristics parameters and is not offered.
    let list = read(card, "pp_power_profile_mode").map(|t| parse_profiles(&t)).unwrap_or_default();
    let profile = match s.get("profile").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let idx = int(v).ok_or("profile is not an integer")?;
            match list.iter().find(|p| i64::from(p.0) == idx) {
                Some(p) if p.1 != "CUSTOM" => Some(p.clone()),
                Some(_) => return Err("the CUSTOM profile is not supported".into()),
                None => return Err(format!("profile {idx} does not exist on this GPU")),
            }
        }
    };
    // A profile only sticks under `manual`. The default profile does not need
    // it, so "auto + BOOTUP_DEFAULT" (the stock state) stays auto.
    let profile_needs_manual = profile.as_ref().map_or(false, |p| p.1 != "BOOTUP_DEFAULT");
    let profile_write = profile.as_ref().filter(|p| !p.2 || profile_needs_manual).map(|p| p.0);
    let cap_uw = match s.get("power_cap_w").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let w = v.as_f64().ok_or("power_cap_w is not a number")?;
            let hw = card.hwmon.as_ref().ok_or("this GPU has no power limit")?;
            let rd = |f: &str| read_trimmed(&hw.join(f)).ok().and_then(|x| x.parse::<i64>().ok());
            let (Some(lo), Some(hi)) = (rd("power1_cap_min"), rd("power1_cap_max")) else {
                return Err("this GPU has no adjustable power limit".into());
            };
            let uw = (w * 1e6).round() as i64;
            check("power limit (µW)", uw, Some((lo, hi)), (1, 1_000_000_000))?;
            Some(uw)
        }
    };
    let od_text = read(card, "pp_od_clk_voltage").unwrap_or_default();
    let table = parse_od(&od_text);
    let od = od_commands(&table, s)?;
    if !od.is_empty() && od_text.trim_matches(|c: char| c == '\0' || c.is_whitespace()).is_empty() {
        return Err("overdrive is not enabled (amdgpu.ppfeaturemask needs bit 0x4000)".into());
    }
    let fan = match s.get("fan").and_then(Value::as_object) { Some(f) => fan_commands(card, f)?, None => Vec::new() };

    // ── everything validated: write ──
    let mut done = Vec::new();
    let od_present = !od_text.trim_matches(|c: char| c == '\0' || c.is_whitespace()).is_empty();
    if has_level && (od_present || profile_write.is_some()) {
        put(card, "power_dpm_force_performance_level", "manual")?;
    }
    if od_present {
        // Always start from stock so the request is the whole state.
        put(card, "pp_od_clk_voltage", "r")?;
        for c in &od {
            if let Err(e) = put(card, "pp_od_clk_voltage", c) { od_reset(card); return Err(e); }
        }
        if let Err(e) = put(card, "pp_od_clk_voltage", "c") { od_reset(card); return Err(e); }
        done.push(if od.is_empty() { "overdrive: stock".into() } else { format!("overdrive: {}", od.join("; ")) });
    }
    if let (Some(uw), Some(hw)) = (cap_uw, card.hwmon.as_ref()) {
        let p = canonical_in_sysfs(&hw.join("power1_cap")).filter(|r| r.starts_with(&card.dev)).ok_or("power1_cap missing")?;
        sysfs_write(&p, uw.to_string().as_bytes()).map_err(|e| format!("power1_cap: {e}"))?;
        done.push(format!("power limit {} W", uw / 1_000_000));
    }
    for (file, cmds) in &fan {
        for c in cmds { put(card, file, c)?; }
        done.push(format!("{}: {}", file.rsplit('/').next().unwrap_or(file), cmds[..cmds.len() - 1].join(", ")));
    }
    if let Some(idx) = profile_write {
        put(card, "pp_power_profile_mode", &idx.to_string())?;
        done.push(format!("power profile {}", profile.as_ref().map_or(String::new(), |p| p.1.to_lowercase())));
    }
    if has_level {
        let want = match level.unwrap_or("auto") {
            "auto" if profile_needs_manual => "manual",
            l => l,
        };
        put(card, "power_dpm_force_performance_level", want)?;
        done.push(format!("performance level {want}"));
    }
    Ok(done)
}

pub fn reset(card: &Card) -> Vec<String> {
    let mut log = Vec::new();
    let has_level = card.dev.join("power_dpm_force_performance_level").exists();
    if has_level { let _ = put(card, "power_dpm_force_performance_level", "manual"); }
    if card.dev.join("pp_od_clk_voltage").exists() { od_reset(card); log.push("overdrive table reset".into()); }
    if let Some(p) = read(card, "pp_power_profile_mode").map(|t| parse_profiles(&t)).and_then(|l| l.into_iter().find(|p| p.1 == "BOOTUP_DEFAULT")) {
        if put(card, "pp_power_profile_mode", &p.0.to_string()).is_ok() { log.push("power profile default".into()); }
    }
    if let Some(hw) = card.hwmon.as_ref() {
        if let Ok(d) = read_trimmed(&hw.join("power1_cap_default")) {
            if let Some(p) = canonical_in_sysfs(&hw.join("power1_cap")).filter(|r| r.starts_with(&card.dev)) {
                if sysfs_write(&p, d.as_bytes()).is_ok() { log.push("power limit default".into()); }
            }
        }
    }
    for f in FAN_VALUES.iter().map(|f| f.1).chain(["fan_curve"]) {
        let rel = format!("{FAN_DIR}/{f}");
        if card.dev.join(&rel).exists() && put(card, &rel, "r").is_ok() {
            let _ = put(card, &rel, "c");
            log.push(format!("{f} default"));
        }
    }
    if has_level && put(card, "power_dpm_force_performance_level", "auto").is_ok() { log.push("performance level auto".into()); }
    log
}

pub fn handle(req: &Value) -> Value {
    let Some(o) = req.as_object() else { return json!({"ok": false, "error": "malformed request"}) };
    let card = match o.get("card").and_then(Value::as_str).map(resolve) {
        Some(Ok(c)) => c,
        Some(Err(e)) => return json!({"ok": false, "error": e}),
        None => return json!({"ok": false, "error": "missing card"}),
    };
    match o.get("op").and_then(Value::as_str) {
        Some("apply") => {
            let empty = Map::new();
            let s = o.get("settings").and_then(Value::as_object).unwrap_or(&empty);
            match apply(&card, s) {
                Ok(done) => json!({"ok": true, "applied": done}),
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
        Some("reset") => json!({"ok": true, "applied": reset(&card)}),
        _ => json!({"ok": false, "error": "unknown op"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLARIS: &str = "OD_SCLK:\n0:        300MHz        750mV\n1:        600MHz        769mV\n2:       1366MHz       1150mV\nOD_MCLK:\n0:        300MHz        750mV\n1:       1750MHz        975mV\nOD_RANGE:\nSCLK:     300MHz       2000MHz\nMCLK:     300MHz       2250MHz\nVDDC:     750mV        1200mV\n";
    const NAVI10: &str = "OD_SCLK:\n0: 800Mhz\n1: 2064Mhz\nOD_MCLK:\n1: 875MHz\nOD_VDDC_CURVE:\n0: 800MHz 716mV\n1: 1432MHz 811mV\n2: 2064MHz 1190mV\nOD_RANGE:\nSCLK:     800Mhz       2150Mhz\nMCLK:     625Mhz        950Mhz\nVDDC_CURVE_SCLK[0]:     800Mhz       2150Mhz\nVDDC_CURVE_VOLT[0]:     750mV        1200mV\nVDDC_CURVE_SCLK[1]:     800Mhz       2150Mhz\nVDDC_CURVE_VOLT[1]:     750mV        1200mV\nVDDC_CURVE_SCLK[2]:     800Mhz       2150Mhz\nVDDC_CURVE_VOLT[2]:     750mV        1200mV\n";
    const RDNA3: &str = "OD_SCLK:\n0: 500Mhz\n1: 2660Mhz\nOD_MCLK:\n0: 97Mhz\n1: 1300MHz\n\0\0\0OD_VDDGFX_OFFSET:\n-100mV\n\0\0OD_RANGE:\nSCLK:     500Mhz       5000Mhz\nMCLK:      97Mhz       1500Mhz\nVDDGFX_OFFSET:    -450mv          0mv\n\0\0";
    const RDNA4: &str = "OD_SCLK_OFFSET:\n0Mhz\nOD_MCLK:\n0: 97Mhz\n1: 1259MHz\nOD_VDDGFX_OFFSET:\n-50mV\nOD_RANGE:\nSCLK_OFFSET:    -500Mhz       1000Mhz\nMCLK:      97Mhz       1500Mhz\nVDDGFX_OFFSET:    -200mv          0mv\n";
    const APU: &str = "OD_SCLK:\n0:        600Mhz\n1:       2200Mhz\nOD_RANGE:\nSCLK:     600Mhz       2200Mhz\n";

    fn obj(v: Value) -> Map<String, Value> { v.as_object().unwrap().clone() }

    #[test]
    fn kinds() {
        assert_eq!(parse_od(POLARIS).kind(), Kind::PerState);
        assert_eq!(parse_od(NAVI10).kind(), Kind::Curve);
        assert_eq!(parse_od(RDNA3).kind(), Kind::MinMax);
        assert_eq!(parse_od(RDNA4).kind(), Kind::Offset);
        assert_eq!(parse_od(APU).kind(), Kind::MinMax);
        let r3 = parse_od(RDNA3);
        assert_eq!(r3.voltage_offset, Some(-100));
        assert_eq!(r3.ranges["VDDGFX_OFFSET"], (-450, 0));
        assert_eq!(r3.mclk, vec![(0, 97, None), (1, 1300, None)]);
        assert_eq!(parse_od(NAVI10).curve[2], (2, 2064, 1190));
        assert_eq!(parse_od(POLARIS).sclk[2], (2, 1366, Some(1150)));
    }

    #[test]
    fn commands() {
        let t = parse_od(RDNA3);
        assert_eq!(od_commands(&t, &obj(json!({"sclk_max": 2500, "voltage_offset": -60}))).unwrap(),
                   vec!["s 1 2500", "vo -60"]);
        assert!(od_commands(&t, &obj(json!({"voltage_offset": 20}))).is_err());       // no overvolt
        assert!(od_commands(&t, &obj(json!({"sclk_max": 4500}))).is_err());           // sanity cap
        assert!(od_commands(&t, &obj(json!({"sclk_min": 2000, "sclk_max": 1000}))).is_err());
        let t = parse_od(RDNA4);
        assert_eq!(od_commands(&t, &obj(json!({"sclk_offset": -100}))).unwrap(), vec!["s -100"]);
        assert!(od_commands(&t, &obj(json!({"voltage_offset": -300}))).is_err());     // range -200
        assert!(od_commands(&t, &obj(json!({"sclk_max": 2000}))).is_err());           // no s 1 state
        let t = parse_od(NAVI10);
        assert_eq!(od_commands(&t, &obj(json!({"vddc_curve": [[800, 750], [1432, 800], [2000, 1100]]}))).unwrap(),
                   vec!["vc 0 800 750", "vc 1 1432 800", "vc 2 2000 1100"]);
        assert!(od_commands(&t, &obj(json!({"mclk_min": 700}))).is_err());            // only state 1
        // stock point below its own range (716 < 750) is accepted unchanged, not lowered further
        let navi = parse_od(&NAVI10.replace("0: 800MHz 716mV", "0: 800MHz 716mV"));
        assert!(od_commands(&navi, &obj(json!({"vddc_curve": [[800, 716], [1432, 811], [2064, 1190]]}))).is_ok());
        assert!(od_commands(&navi, &obj(json!({"vddc_curve": [[800, 700], [1432, 811], [2064, 1190]]}))).is_err());
        let t = parse_od(POLARIS);
        assert_eq!(od_commands(&t, &obj(json!({"sclk_states": [[300, 750], [600, 760], [1340, 1100]]}))).unwrap(),
                   vec!["s 0 300 750", "s 1 600 760", "s 2 1340 1100"]);
        assert!(od_commands(&t, &obj(json!({"sclk_states": [[300, 700], [600, 760], [1340, 1100]]}))).is_err());
        let t = parse_od(APU);
        assert_eq!(od_commands(&t, &obj(json!({"sclk_max": 1800}))).unwrap(), vec!["s 1 1800"]);
    }

    #[test]
    fn profiles_and_fans() {
        let old = "NUM        MODE_NAME     SCLK_UP_HYST\n  0   BOOTUP_DEFAULT:        -\n  1 3D_FULL_SCREEN *:        0\n  6           CUSTOM:        -\n";
        assert_eq!(parse_profiles(old), vec![(0, "BOOTUP_DEFAULT".into(), false), (1, "3D_FULL_SCREEN".into(), true), (6, "CUSTOM".into(), false)]);
        let new = "PROFILE_INDEX(NAME) CLOCK_TYPE(NAME) FPS\n 0 BOOTUP_DEFAULT*:\n                    0(       GFXCLK)       0       1\n 1 3D_FULL_SCREEN :\n";
        assert_eq!(parse_profiles(new), vec![(0, "BOOTUP_DEFAULT".into(), true), (1, "3D_FULL_SCREEN".into(), false)]);
        assert_eq!(parse_fan_value("FAN_MINIMUM_PWM:\n35\nOD_RANGE:\nMINIMUM_PWM: 35 100\n"), Some((35, (35, 100))));
        let c = parse_fan_curve("OD_FAN_CURVE:\n0: 0C 0%\n1: 45C 40%\nOD_RANGE:\nFAN_CURVE(hotspot temp): 25C 100C\nFAN_CURVE(fan speed): 35% 100%\n").unwrap();
        assert_eq!(c, (vec![(0, 0), (45, 40)], (25, 100), (35, 100)));
    }

    /// Real dumps: LPM_AMDGPU_SNAPSHOTS=<dir with */card*/device/pp_od_clk_voltage> cargo test
    #[test]
    fn snapshots() {
        let Ok(dir) = std::env::var("LPM_AMDGPU_SNAPSHOTS") else { return };
        for gpu in std::fs::read_dir(dir).unwrap().flatten() {
            for card in std::fs::read_dir(gpu.path()).unwrap().flatten() {
                let dev = card.path().join("device");
                if let Ok(b) = std::fs::read(dev.join("pp_od_clk_voltage")) {
                    let t = parse_od(&String::from_utf8_lossy(&b));
                    println!("{:>14}: {:?} sclk={:?} mclk={:?} curve={} vo={:?} so={:?} ranges={:?}",
                        gpu.file_name().to_string_lossy(), t.kind(), t.sclk, t.mclk, t.curve.len(),
                        t.voltage_offset, t.sclk_offset, t.ranges.keys().collect::<Vec<_>>());
                }
                if let Ok(b) = std::fs::read(dev.join("pp_power_profile_mode")) {
                    let p = parse_profiles(&String::from_utf8_lossy(&b));
                    println!("{:>14}  profiles: {:?}", "", p.iter().map(|p| format!("{}{}{}", p.0, p.1, if p.2 {"*"} else {""})).collect::<Vec<_>>());
                }
            }
        }
    }
}
