//! Global limits via NVML: power limit, clock offsets, memory locked clocks.
//! Port of hal/limits.py.
//!
//! Memory offsets are expressed in *effective* (data-rate) MHz everywhere in
//! this crate. NVML's CLOCK_MEM offset is in raw half-rate MHz, so values are
//! doubled on write and halved (round-to-nearest) on read, here and only here.

use crate::nvml::{self, CLOCK_GRAPHICS, CLOCK_MEM};
use crate::proc;
use log::{debug, warn};
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize)]
pub struct PowerLimit {
    pub power_limit_w: Option<u32>,
    pub default_power_limit_w: Option<u32>,
    pub min_power_limit_w: Option<u32>,
    pub max_power_limit_w: Option<u32>,
}

pub fn get_power_limit(gpu_index: u32) -> PowerLimit {
    let mut out = PowerLimit::default();
    let r = (|| -> Result<(), String> {
        let n = nvml::ready()?;
        let h = n.handle(gpu_index)?;
        let limit = n.power_limit_mw(h)?;
        let (lo, hi) = n.power_limit_constraints_mw(h)?;
        out.power_limit_w = Some(limit / 1000);
        out.min_power_limit_w = Some(lo / 1000);
        out.max_power_limit_w = Some(hi / 1000);
        out.default_power_limit_w = n.default_power_limit_mw(h).ok().map(|v| v / 1000);
        Ok(())
    })();
    if let Err(e) = r { warn!("get_power_limit: {e}"); }
    out
}

const NVIDIA_SMI: &[&str] = &["/usr/bin/nvidia-smi", "/opt/bin/nvidia-smi", "/usr/sbin/nvidia-smi"];

pub fn set_power_limit(limit_w: u32, gpu_index: u32) -> Result<(), String> {
    let nvml_err = match nvml::ready().and_then(|n| {
        let h = n.handle(gpu_index)?;
        n.set_power_limit_mw(h, limit_w.saturating_mul(1000))
    }) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    debug!("NVML set_power_limit failed: {nvml_err} — falling back to nvidia-smi");

    // Fallback with a hard timeout: a wedged driver is exactly when this fires.
    let Some(bin) = proc::find_trusted(NVIDIA_SMI) else {
        return Err(format!("NVML rejected the power limit ({nvml_err}) and no trusted nvidia-smi was found"));
    };
    let args = vec!["-i".into(), gpu_index.to_string(), "-pl".into(), limit_w.to_string()];
    match proc::run(bin, &args, Duration::from_secs(15)) {
        Ok(o) if o.code == Some(0) => Ok(()),
        Ok(o) => Err(if !o.stderr.is_empty() { o.stderr } else { o.stdout }),
        Err(proc::RunError::Timeout) => Err("nvidia-smi timed out after 15s while setting the power limit".into()),
        Err(proc::RunError::NotFound) => Err("NVML rejected the power limit and nvidia-smi is not installed".into()),
        Err(proc::RunError::Spawn(e)) => Err(format!("failed to run nvidia-smi: {e}")),
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClockOffsets {
    pub gpc_offset_mhz: Option<i32>,
    pub mem_offset_mhz: Option<i32>,
}

fn raw_mem_to_effective(raw: i32) -> i32 {
    // Python round(): half-to-even. Raw values we write are always even.
    let h = raw as f64 / 2.0;
    let r = h.round();
    (if (h - h.trunc()).abs() == 0.5 && r as i64 % 2 != 0 { r - h.signum() } else { r }) as i32
}

pub fn get_clock_offsets(gpu_index: u32) -> ClockOffsets {
    let mut out = ClockOffsets::default();
    let Ok(n) = nvml::ready() else { return out };
    let Ok(h) = n.handle(gpu_index) else { return out };

    if n.has("nvmlDeviceGetClockOffsets") {
        match n.get_clock_offset(h, CLOCK_GRAPHICS, 0) {
            Ok(v) => out.gpc_offset_mhz = Some(v.offset_mhz),
            Err(e) => debug!("nvmlDeviceGetClockOffsets(GRAPHICS): {e}"),
        }
        match n.get_clock_offset(h, CLOCK_MEM, 0) {
            Ok(v) => out.mem_offset_mhz = Some(raw_mem_to_effective(v.offset_mhz)),
            Err(e) => debug!("nvmlDeviceGetClockOffsets(MEM): {e}"),
        }
    }
    if out.gpc_offset_mhz.is_none() {
        out.gpc_offset_mhz = n.gpc_clk_vf_offset(h).map_err(|e| debug!("GetGpcClkVfOffset: {e}")).ok();
    }
    if out.mem_offset_mhz.is_none() {
        out.mem_offset_mhz = n.mem_clk_vf_offset(h).map_err(|e| debug!("GetMemClkVfOffset: {e}")).ok()
            .map(raw_mem_to_effective);
    }
    out
}

/// Sets only the domains given. mem_offset_mhz is effective MHz.
///
/// Core: the per-P-state API first (its struct is now the right size, so it
/// actually works), the deprecated per-domain call as fallback.
/// Memory: deliberately the *other* order — the deprecated call first, which
/// is what every earlier build used and what the ×2 effective→raw scaling was
/// calibrated against on real hardware. The new API only takes over where the
/// old one is missing (see `LPM_NVML_MEM_NEW_API` to try it explicitly).
pub fn set_clock_offsets(gpc_offset_mhz: Option<i32>, mem_offset_mhz: Option<i32>, gpu_index: u32)
    -> Result<(), String>
{
    if gpc_offset_mhz.is_none() && mem_offset_mhz.is_none() { return Ok(()); }
    let n = nvml::ready()?;
    let h = n.handle(gpu_index)?;
    let has_new = n.has("nvmlDeviceSetClockOffsets");
    let mut errs = Vec::new();

    if let Some(v) = gpc_offset_mhz {
        let new = if has_new { n.set_clock_offset(h, CLOCK_GRAPHICS, 0, v) } else { Err("n/a".into()) };
        if let Err(e) = new {
            if has_new { debug!("nvmlDeviceSetClockOffsets(GRAPHICS): {e} — trying the per-domain call"); }
            if let Err(e2) = n.set_gpc_clk_vf_offset(h, v) { errs.push(format!("GPC: {e2}")); }
        }
    }
    if let Some(v) = mem_offset_mhz {
        let raw = v.checked_mul(2).ok_or("mem_offset_mhz overflows NVML's raw domain")?;
        let prefer_new = std::env::var_os("LPM_NVML_MEM_NEW_API").is_some();
        let old = |n: &nvml::Nvml| n.set_mem_clk_vf_offset(h, raw);
        let new = |n: &nvml::Nvml| n.set_clock_offset(h, CLOCK_MEM, 0, raw);
        let (first, second): (&dyn Fn(&nvml::Nvml) -> Result<(), String>, &dyn Fn(&nvml::Nvml) -> Result<(), String>) =
            if prefer_new { (&new, &old) } else { (&old, &new) };
        if let Err(e) = first(n) {
            debug!("memory offset, first API: {e} — trying the other one");
            if let Err(e2) = second(n) { errs.push(format!("MEM: {e}; {e2}")); }
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(errs.join("; ")) }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OffsetRanges {
    /// Core offset range (MHz) the driver allows in P0.
    pub gpc: Option<(i32, i32)>,
    /// Memory offset range in effective MHz.
    pub mem: Option<(i32, i32)>,
}

/// Ranges straight from nvmlDeviceGetClockOffsets (P0). None where the
/// driver can't report them.
pub fn get_offset_ranges(gpu_index: u32) -> OffsetRanges {
    let mut out = OffsetRanges::default();
    let Ok(n) = nvml::ready() else { return out };
    let Ok(h) = n.handle(gpu_index) else { return out };
    if let Ok(i) = n.get_clock_offset(h, CLOCK_GRAPHICS, 0) { out.gpc = Some((i.min_mhz, i.max_mhz)); }
    if let Ok(i) = n.get_clock_offset(h, CLOCK_MEM, 0) {
        out.mem = Some((raw_mem_to_effective(i.min_mhz), raw_mem_to_effective(i.max_mhz)));
    }
    out
}

/// Zeroes the core and memory offset of *every* supported P-state (and the
/// deprecated per-domain offsets), not only P0: another tool — or an older
/// build — may have left offsets in P2/P3/P5 that keep applying after a
/// "reset". Returns what was changed.
pub fn reset_all_clock_offsets(gpu_index: u32) -> Result<Vec<String>, String> {
    let n = nvml::ready()?;
    let h = n.handle(gpu_index)?;
    let mut done = Vec::new();
    if n.has("nvmlDeviceGetClockOffsets") {
        for p in n.supported_pstates(h).unwrap_or_else(|_| vec![0]) {
            for (kind, name) in [(CLOCK_GRAPHICS, "core"), (CLOCK_MEM, "memory")] {
                match n.get_clock_offset(h, kind, p) {
                    Ok(i) if i.offset_mhz != 0 => match n.set_clock_offset(h, kind, p, 0) {
                        Ok(()) => done.push(format!("P{p} {name} offset {:+} → 0", i.offset_mhz)),
                        Err(e) => warn!("reset P{p} {name} offset: {e}"),
                    },
                    _ => {}
                }
            }
        }
    }
    if n.gpc_clk_vf_offset(h).map_or(false, |v| v != 0) && n.set_gpc_clk_vf_offset(h, 0).is_ok() {
        done.push("core offset (per-domain) → 0".into());
    }
    if n.mem_clk_vf_offset(h).map_or(false, |v| v != 0) && n.set_mem_clk_vf_offset(h, 0).is_ok() {
        done.push("memory offset (per-domain) → 0".into());
    }
    Ok(done)
}

// ── clock event (throttle) reasons ──────────────────────────────────────────

const REASONS: &[(u64, &str, &str)] = &[
    (0x0001, "idle", "GPU idle"),
    (0x0002, "app_clocks", "application clock setting"),
    (0x0004, "sw_power_cap", "power limit"),
    (0x0008, "hw_slowdown", "hardware slowdown (power brake / thermal)"),
    (0x0010, "sync_boost", "sync boost"),
    (0x0020, "sw_thermal", "thermal limit (driver)"),
    (0x0040, "hw_thermal", "thermal limit (hardware)"),
    (0x0080, "hw_power_brake", "external power brake"),
    (0x0100, "display_clock", "display clock setting"),
];

#[derive(Debug, Clone, Default, Serialize)]
pub struct Throttle {
    pub mask: u64,
    /// Short keys ("sw_power_cap", …), idle excluded.
    pub reasons: Vec<&'static str>,
    pub descriptions: Vec<&'static str>,
}

pub fn decode_throttle(mask: u64) -> Throttle {
    let mut t = Throttle { mask, ..Default::default() };
    for &(bit, key, text) in REASONS {
        if mask & bit != 0 && bit != 0x0001 { t.reasons.push(key); t.descriptions.push(text); }
    }
    t
}

pub fn get_throttle(gpu_index: u32) -> Option<Throttle> {
    let n = nvml::ready().ok()?;
    n.clock_event_reasons(n.handle(gpu_index).ok()?).ok().map(decode_throttle)
}

// ── PowerMizer ──────────────────────────────────────────────────────────────

pub const POWER_MIZER_MODES: &[(u32, &str, &str)] = &[
    (0, "adaptive", "Adaptive — clocks drop when load drops"),
    (1, "max", "Prefer maximum performance — keeps clocks up (no down-clock stutter, more idle power)"),
    (2, "auto", "Auto — the driver decides (default)"),
    (3, "consistent", "Prefer consistent performance — steady clocks for benchmarking"),
];

pub fn power_mizer_mode_id(name: &str) -> Option<u32> {
    POWER_MIZER_MODES.iter().find(|m| m.1 == name).map(|m| m.0)
}

#[derive(Debug, Clone, Serialize)]
pub struct PowerMizer {
    pub current: &'static str,
    pub supported: Vec<&'static str>,
}

pub fn get_power_mizer(gpu_index: u32) -> Result<PowerMizer, String> {
    let n = nvml::ready()?;
    let m = n.power_mizer(n.handle(gpu_index)?)?;
    let name = |id: u32| POWER_MIZER_MODES.iter().find(|x| x.0 == id).map_or("unknown", |x| x.1);
    Ok(PowerMizer {
        current: name(m.current),
        supported: POWER_MIZER_MODES.iter().filter(|x| m.supported & (1 << x.0) != 0).map(|x| x.1).collect(),
    })
}

pub fn set_power_mizer(gpu_index: u32, name: &str) -> Result<(), String> {
    let id = power_mizer_mode_id(name).ok_or_else(|| format!("unknown PowerMizer mode {name:?}"))?;
    let n = nvml::ready()?;
    let h = n.handle(gpu_index)?;
    let m = n.power_mizer(h)?;
    if m.supported & (1 << id) == 0 { return Err(format!("PowerMizer mode {name} is not supported by this GPU")); }
    if m.current == id { return Ok(()); }
    n.set_power_mizer(h, id)?;
    let after = n.power_mizer(h)?;
    if after.current != id { return Err(format!("driver kept PowerMizer mode {} after the write", after.current)); }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct MemOffsetRange {
    pub min_mem_offset_mhz: i32,
    pub max_mem_offset_mhz: i32,
}

/// Allowed memory offset range in effective MHz; falls back to the observed
/// RTX 5090 values (-500/+1500) if NVML can't report it.
pub fn get_mem_offset_range(gpu_index: u32) -> MemOffsetRange {
    let mut out = MemOffsetRange { min_mem_offset_mhz: -500, max_mem_offset_mhz: 1500 };
    if let Ok(n) = nvml::ready() {
        if let Ok((lo, hi)) = n.handle(gpu_index).and_then(|h| n.mem_clk_min_max_vf_offset(h)) {
            out.min_mem_offset_mhz = raw_mem_to_effective(lo);
            out.max_mem_offset_mhz = raw_mem_to_effective(hi);
        }
    }
    out
}

pub fn get_supported_mem_clocks(gpu_index: u32) -> Vec<u32> {
    let r = nvml::ready().and_then(|n| n.supported_memory_clocks(n.handle(gpu_index)?));
    match r {
        Ok(mut v) => { v.sort_unstable(); v.dedup(); v }
        Err(e) => { debug!("get_supported_mem_clocks: {e}"); Vec::new() }
    }
}

pub fn get_max_mem_clock(gpu_index: u32) -> Option<u32> {
    get_supported_mem_clocks(gpu_index).last().copied()
}

/// Memory clock NVML reports right now (a lock request is snapped silently).
pub fn get_current_mem_clock(gpu_index: u32) -> Option<u32> {
    let n = nvml::ready().ok()?;
    n.clock(n.handle(gpu_index).ok()?, CLOCK_MEM).ok()
}

pub fn set_mem_locked_clocks(min_mhz: u32, max_mhz: u32, gpu_index: u32) -> Result<(), String> {
    let n = nvml::ready()?;
    n.set_memory_locked_clocks(n.handle(gpu_index)?, min_mhz, max_mhz)
}

pub fn reset_mem_locked_clocks(gpu_index: u32) -> Result<(), String> {
    let n = nvml::ready()?;
    n.reset_memory_locked_clocks(n.handle(gpu_index)?)
}

pub fn set_gpu_locked_clocks(min_mhz: u32, max_mhz: u32, gpu_index: u32) -> Result<(), String> {
    let n = nvml::ready()?;
    n.set_gpu_locked_clocks(n.handle(gpu_index)?, min_mhz, max_mhz)
}

pub fn reset_gpu_locked_clocks(gpu_index: u32) -> Result<(), String> {
    let n = nvml::ready()?;
    n.reset_gpu_locked_clocks(n.handle(gpu_index)?)
}

#[cfg(test)]
mod tests {
    use super::raw_mem_to_effective as f;
    use super::decode_throttle;
    #[test]
    fn throttle_decoding() {
        let t = decode_throttle(0x0001 | 0x0004 | 0x0040);
        assert_eq!(t.reasons, vec!["sw_power_cap", "hw_thermal"]);
        assert!(decode_throttle(0x0001).reasons.is_empty());
    }

    #[test]
    fn python_round_semantics() {
        assert_eq!(f(2000), 1000);
        assert_eq!(f(-1000), -500);
        assert_eq!(f(3), 2);   // round(1.5) == 2
        assert_eq!(f(5), 2);   // round(2.5) == 2
        assert_eq!(f(-3), -2); // round(-1.5) == -2
        assert_eq!(f(7), 4);   // round(3.5) == 4
    }
}
