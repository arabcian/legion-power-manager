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
        let mut used_new = false;
        match n.get_clock_offset(h, CLOCK_GRAPHICS) {
            Ok(v) => { out.gpc_offset_mhz = Some(v); used_new = true; }
            Err(e) => debug!("nvmlDeviceGetClockOffsets(GRAPHICS): {e}"),
        }
        match n.get_clock_offset(h, CLOCK_MEM) {
            Ok(v) => { out.mem_offset_mhz = Some(raw_mem_to_effective(v)); used_new = true; }
            Err(e) => debug!("nvmlDeviceGetClockOffsets(MEM): {e}"),
        }
        if used_new { return out; }
    }
    out.gpc_offset_mhz = n.gpc_clk_vf_offset(h).map_err(|e| debug!("GetGpcClkVfOffset: {e}")).ok();
    out.mem_offset_mhz = n.mem_clk_vf_offset(h).map_err(|e| debug!("GetMemClkVfOffset: {e}")).ok()
        .map(raw_mem_to_effective);
    out
}

/// Sets only the domains given. mem_offset_mhz is effective MHz.
pub fn set_clock_offsets(gpc_offset_mhz: Option<i32>, mem_offset_mhz: Option<i32>, gpu_index: u32)
    -> Result<(), String>
{
    if gpc_offset_mhz.is_none() && mem_offset_mhz.is_none() { return Ok(()); }
    let n = nvml::ready()?;
    let h = n.handle(gpu_index)?;
    let mem_raw = match mem_offset_mhz {
        Some(v) => Some(v.checked_mul(2).ok_or("mem_offset_mhz overflows NVML's raw domain")?),
        None => None,
    };
    let domains: Vec<(u32, i32)> = [(CLOCK_GRAPHICS, gpc_offset_mhz), (CLOCK_MEM, mem_raw)]
        .into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect();

    if n.has("nvmlDeviceSetClockOffsets") {
        let mut all_ok = true;
        for &(k, v) in &domains {
            if let Err(e) = n.set_clock_offset(h, k, v) {
                debug!("nvmlDeviceSetClockOffsets(type={k}): {e} — trying fallback");
                all_ok = false;
                break;
            }
        }
        if all_ok { return Ok(()); }
    }

    // Deprecated per-domain API (still what works on Blackwell / 590.x).
    let mut errs = Vec::new();
    if let Some(v) = gpc_offset_mhz {
        if let Err(e) = n.set_gpc_clk_vf_offset(h, v) { errs.push(format!("GPC: {e}")); }
    }
    if let Some(v) = mem_raw {
        if let Err(e) = n.set_mem_clk_vf_offset(h, v) { errs.push(format!("MEM: {e}")); }
    }
    if errs.is_empty() { Ok(()) } else { Err(errs.join("; ")) }
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

#[cfg(test)]
mod tests {
    use super::raw_mem_to_effective as f;
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
