//! Plain data types shared by HAL, profiles, CLI and daemon.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Domain {
    Gpu,
    Memory,
}

#[derive(Debug, Clone, Serialize)]
pub struct VfPoint {
    pub index: usize,
    /// Frequency reported by GetVFPCurve (already includes the current delta).
    pub freq_khz: u32,
    /// Offset-free base frequency from the V3 status (None on GPUs/drivers
    /// without the base tuple). Moves with temperature and load: use it for
    /// display and range checks only, never store it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_khz: Option<u32>,
    pub volt_uv: u32,
    /// Signed offset from the ClockBoostTable.
    pub delta_khz: i32,
    pub domain: Domain,
}

impl VfPoint {
    /// The frequency the point runs at. `freq_khz` already includes the delta
    /// (adding it again double-counted the offset).
    pub fn effective_freq_khz(&self) -> i64 { self.freq_khz as i64 }
    /// Offset-free frequency: the V3 base tuple when the driver has one,
    /// otherwise the reported frequency minus the stored delta.
    pub fn base_freq_khz(&self) -> i64 {
        self.base_khz.map_or(self.freq_khz as i64 - self.delta_khz as i64, i64::from)
    }
    pub fn freq_mhz(&self) -> f64 { self.freq_khz as f64 / 1000.0 }
    pub fn effective_freq_mhz(&self) -> f64 { self.effective_freq_khz() as f64 / 1000.0 }
    pub fn volt_mv(&self) -> f64 { self.volt_uv as f64 / 1000.0 }
    pub fn delta_mhz(&self) -> f64 { self.delta_khz as f64 / 1000.0 }
}

#[derive(Debug, Clone, Serialize)]
pub struct CurveState {
    pub points: Vec<VfPoint>,
    /// How GPU/memory points were told apart: "point-info" (the driver's
    /// per-point type + voltage-based flag) or "flags" (legacy heuristic).
    pub domain_source: &'static str,
    pub timestamp: f64,
    pub gpu_name: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MonitoringSample {
    pub timestamp: f64,
    pub voltage_uv: Option<u32>,
    pub clock_mhz: Option<f64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub fan_pct: Option<f64>,
    pub pstate: Option<u32>,
    pub mem_used_bytes: Option<u64>,
    pub mem_total_bytes: Option<u64>,
    pub gpu_util_pct: Option<f64>,
    pub mem_util_pct: Option<f64>,
    pub mem_clock_mhz: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuInfo {
    pub name: String,
    pub index: usize,
    pub uuid: Option<String>,
    pub pci_bus_id: Option<u32>,
}

impl GpuInfo {
    /// Stable identifier used as the key in `auto_load_profiles`.
    pub fn stable_key(&self) -> String {
        if let Some(u) = &self.uuid { return u.clone(); }
        if let Some(b) = self.pci_bus_id { return format!("pci:{b:04x}"); }
        format!("idx:{}", self.index)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub filepath: String,
    pub timestamp: String,
    pub gpu: String,
    pub nonzero_offsets: u64,
    pub size: u64,
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
