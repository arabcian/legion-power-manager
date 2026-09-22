//! Live monitoring: voltage via NvAPI, everything else via NVML.

use crate::nvapi::{fid, nvcall, Gpu, VOLT_SIZE};
use crate::nvml;
use crate::types::{now_secs, MonitoringSample};

pub fn init_nvml() -> bool { nvml::ready().is_ok() }

pub fn shutdown_nvml() {
    if let Some(n) = nvml::get() { n.shutdown(); }
}

pub fn get_driver_version() -> Option<String> {
    nvml::ready().ok()?.driver_version().ok()
}

pub fn get_vram_total(gpu_index: u32) -> Option<u64> {
    let n = nvml::ready().ok()?;
    Some(n.memory(n.handle(gpu_index).ok()?).ok()?.total)
}

/// Current core voltage in µV.
pub fn read_voltage(gpu: Gpu) -> Result<u32, String> {
    Ok(nvcall(fid::GET_CURRENT_VOLTAGE, gpu, VOLT_SIZE, 1, |_| {})?.u32_at(0x28))
}

fn nvml_fill(s: &mut MonitoringSample, gpu_index: u32) {
    let Ok(n) = nvml::ready() else { return };
    let Ok(h) = n.handle(gpu_index) else { return };
    s.clock_mhz = n.clock(h, nvml::CLOCK_GRAPHICS).ok().map(f64::from);
    s.mem_clock_mhz = n.clock(h, nvml::CLOCK_MEM).ok().map(f64::from);
    s.temp_c = n.temperature(h).ok().map(f64::from);
    s.power_w = n.power_usage_mw(h).ok().map(|mw| mw as f64 / 1000.0);
    s.pstate = n.performance_state(h).ok();
    if let Ok(m) = n.memory(h) {
        s.mem_used_bytes = Some(m.used);
        s.mem_total_bytes = Some(m.total);
    }
    if let Ok(u) = n.utilization(h) {
        s.gpu_util_pct = Some(u.gpu as f64);
        s.mem_util_pct = Some(u.memory as f64);
    }
    s.fan_pct = n.fan_speed(h).ok().map(f64::from);
}

pub fn poll(gpu: Gpu, gpu_index: u32) -> MonitoringSample {
    let mut s = MonitoringSample { timestamp: now_secs(), voltage_uv: read_voltage(gpu).ok(), ..Default::default() };
    nvml_fill(&mut s, gpu_index);
    s
}
