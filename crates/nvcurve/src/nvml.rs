//! Minimal NVML binding via dlopen (replaces pynvml).
//!
//! Only the entry points nvcurve uses are resolved, each lazily by name, so
//! a driver lacking one (e.g. nvmlDeviceGetClockOffsets before 555.85) just
//! reports "not available" for that call.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::{Mutex, OnceLock};

pub const CLOCK_GRAPHICS: u32 = 0;
pub const CLOCK_MEM: u32 = 2;
const TEMPERATURE_GPU: u32 = 0;
const ERROR_INSUFFICIENT_SIZE: u32 = 7;

#[derive(Debug, Clone, Copy)]
pub struct Handle(usize);

pub struct Nvml {
    dl: usize,
    initialized: Mutex<bool>,
    handles: Mutex<HashMap<u32, usize>>,
}

static NVML: OnceLock<Option<Nvml>> = OnceLock::new();

/// The process-wide NVML instance, or None if libnvidia-ml can't be loaded.
pub fn get() -> Option<&'static Nvml> {
    NVML.get_or_init(|| {
        for name in [&b"libnvidia-ml.so.1\0"[..], &b"libnvidia-ml.so\0"[..]] {
            let h = unsafe { libc::dlopen(name.as_ptr() as *const c_char, libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !h.is_null() {
                return Some(Nvml { dl: h as usize, initialized: Mutex::new(false), handles: Mutex::new(HashMap::new()) });
            }
        }
        None
    }).as_ref()
}

/// get() + ensure_init(), as a Result with a user-facing message.
pub fn ready() -> Result<&'static Nvml, String> {
    let n = get().ok_or_else(|| "NVML not available (libnvidia-ml.so.1 not found)".to_string())?;
    n.ensure_init()?;
    Ok(n)
}

/// nvmlClockOffset_v1_t. The struct carries the offset *and* the allowed
/// range; NVML derives the expected size from `version`, so a struct missing
/// the two range fields (16 instead of 24 bytes) is rejected with
/// ARGUMENT_VERSION_MISMATCH — which is what silently pushed every earlier
/// build onto the deprecated per-domain API.
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
struct ClockOffset { version: u32, kind: u32, pstate: u32, offset_mhz: i32, min_mhz: i32, max_mhz: i32 }
const CLOCK_OFFSET_V1: u32 = (1 << 24) | std::mem::size_of::<ClockOffset>() as u32;
const _: () = assert!(std::mem::size_of::<ClockOffset>() == 24);

/// Offset of one clock domain in one P-state, with the driver's range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetInfo { pub offset_mhz: i32, pub min_mhz: i32, pub max_mhz: i32 }

/// nvmlDevicePowerMizerModes_v1_t (no version field).
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct PowerMizerModes { pub current: u32, pub mode: u32, pub supported: u32 }

pub const PSTATE_UNKNOWN: u32 = 32;
const MAX_PSTATES: usize = 16;

#[repr(C)]
struct PciInfoV3 {
    bus_id_legacy: [c_char; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_subsystem_id: u32,
    bus_id: [c_char; 32],
}

#[repr(C)]
#[derive(Default)]
pub struct Memory { pub total: u64, pub free: u64, pub used: u64 }

#[repr(C)]
#[derive(Default)]
pub struct Utilization { pub gpu: u32, pub memory: u32 }

impl Nvml {
    fn sym(&self, name: &str) -> Option<usize> {
        let c = CString::new(name).ok()?;
        let p = unsafe { libc::dlsym(self.dl as *mut c_void, c.as_ptr()) };
        (!p.is_null()).then_some(p as usize)
    }

    /// Resolves `name` as function type F (must be a fn pointer type).
    fn func<F: Copy>(&self, name: &str) -> Result<F, String> {
        assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
        let p = self.sym(name).ok_or_else(|| format!("{name} not found in libnvidia-ml"))?;
        Ok(unsafe { std::mem::transmute_copy::<usize, F>(&p) })
    }

    pub fn has(&self, name: &str) -> bool { self.sym(name).is_some() }

    pub fn error_string(&self, rc: u32) -> String {
        if let Ok(f) = self.func::<unsafe extern "C" fn(u32) -> *const c_char>("nvmlErrorString") {
            let p = unsafe { f(rc) };
            if !p.is_null() {
                return format!("NVML error {rc}: {}", unsafe { CStr::from_ptr(p) }.to_string_lossy());
            }
        }
        format!("NVML error code: {rc}")
    }

    fn check(&self, rc: u32) -> Result<(), String> {
        if rc == 0 { Ok(()) } else { Err(self.error_string(rc)) }
    }

    /// nvmlInit once per process (NVML refcounts internally anyway).
    pub fn ensure_init(&self) -> Result<(), String> {
        let mut g = self.initialized.lock().unwrap_or_else(|p| p.into_inner());
        if *g { return Ok(()); }
        let f: unsafe extern "C" fn() -> u32 = self.func("nvmlInit_v2").or_else(|_| self.func("nvmlInit"))?;
        self.check(unsafe { f() })?;
        *g = true;
        self.handles.lock().unwrap_or_else(|p| p.into_inner()).clear();
        Ok(())
    }

    pub fn shutdown(&self) {
        let mut g = self.initialized.lock().unwrap_or_else(|p| p.into_inner());
        if !*g { return; }
        if let Ok(f) = self.func::<unsafe extern "C" fn() -> u32>("nvmlShutdown") { unsafe { f() }; }
        *g = false;
        // Handles are only valid within one nvmlInit session.
        self.handles.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub fn handle(&self, index: u32) -> Result<Handle, String> {
        self.ensure_init()?;
        if let Some(&h) = self.handles.lock().unwrap_or_else(|p| p.into_inner()).get(&index) {
            return Ok(Handle(h));
        }
        let f: unsafe extern "C" fn(u32, *mut usize) -> u32 =
            self.func("nvmlDeviceGetHandleByIndex_v2").or_else(|_| self.func("nvmlDeviceGetHandleByIndex"))?;
        let mut h = 0usize;
        self.check(unsafe { f(index, &mut h) })?;
        self.handles.lock().unwrap_or_else(|p| p.into_inner()).insert(index, h);
        Ok(Handle(h))
    }

    fn get1<A: Default>(&self, name: &str, h: Handle) -> Result<A, String> {
        let f: unsafe extern "C" fn(usize, *mut A) -> u32 = self.func(name)?;
        let mut a = A::default();
        self.check(unsafe { f(h.0, &mut a) })?;
        Ok(a)
    }

    fn string_out(&self, rc_fn: impl FnOnce(*mut c_char, u32) -> Result<u32, String>) -> Result<String, String> {
        let mut buf = [0 as c_char; 128];
        let rc = rc_fn(buf.as_mut_ptr(), buf.len() as u32)?;
        self.check(rc)?;
        buf[buf.len() - 1] = 0;
        Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }

    pub fn driver_version(&self) -> Result<String, String> {
        self.ensure_init()?;
        let f: unsafe extern "C" fn(*mut c_char, u32) -> u32 = self.func("nvmlSystemGetDriverVersion")?;
        self.string_out(|p, n| Ok(unsafe { f(p, n) }))
    }

    pub fn uuid(&self, h: Handle) -> Result<String, String> {
        let f: unsafe extern "C" fn(usize, *mut c_char, u32) -> u32 = self.func("nvmlDeviceGetUUID")?;
        self.string_out(|p, n| Ok(unsafe { f(h.0, p, n) }))
    }

    pub fn pci_bus(&self, h: Handle) -> Result<u32, String> {
        let f: unsafe extern "C" fn(usize, *mut PciInfoV3) -> u32 = self.func("nvmlDeviceGetPciInfo_v3")?;
        let mut info: PciInfoV3 = unsafe { std::mem::zeroed() };
        self.check(unsafe { f(h.0, &mut info) })?;
        Ok(info.bus)
    }

    pub fn clock(&self, h: Handle, kind: u32) -> Result<u32, String> {
        let f: unsafe extern "C" fn(usize, u32, *mut u32) -> u32 = self.func("nvmlDeviceGetClockInfo")?;
        let mut v = 0;
        self.check(unsafe { f(h.0, kind, &mut v) })?;
        Ok(v)
    }

    pub fn temperature(&self, h: Handle) -> Result<u32, String> {
        let f: unsafe extern "C" fn(usize, u32, *mut u32) -> u32 = self.func("nvmlDeviceGetTemperature")?;
        let mut v = 0;
        self.check(unsafe { f(h.0, TEMPERATURE_GPU, &mut v) })?;
        Ok(v)
    }

    pub fn power_usage_mw(&self, h: Handle) -> Result<u32, String> { self.get1("nvmlDeviceGetPowerUsage", h) }
    pub fn performance_state(&self, h: Handle) -> Result<u32, String> { self.get1("nvmlDeviceGetPerformanceState", h) }
    pub fn memory(&self, h: Handle) -> Result<Memory, String> { self.get1("nvmlDeviceGetMemoryInfo", h) }
    pub fn utilization(&self, h: Handle) -> Result<Utilization, String> { self.get1("nvmlDeviceGetUtilizationRates", h) }
    pub fn fan_speed(&self, h: Handle) -> Result<u32, String> { self.get1("nvmlDeviceGetFanSpeed", h) }
    pub fn power_limit_mw(&self, h: Handle) -> Result<u32, String> { self.get1("nvmlDeviceGetPowerManagementLimit", h) }
    pub fn default_power_limit_mw(&self, h: Handle) -> Result<u32, String> { self.get1("nvmlDeviceGetPowerManagementDefaultLimit", h) }

    pub fn power_limit_constraints_mw(&self, h: Handle) -> Result<(u32, u32), String> {
        let f: unsafe extern "C" fn(usize, *mut u32, *mut u32) -> u32 =
            self.func("nvmlDeviceGetPowerManagementLimitConstraints")?;
        let (mut a, mut b) = (0, 0);
        self.check(unsafe { f(h.0, &mut a, &mut b) })?;
        Ok((a, b))
    }

    pub fn set_power_limit_mw(&self, h: Handle, mw: u32) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize, u32) -> u32 = self.func("nvmlDeviceSetPowerManagementLimit")?;
        self.check(unsafe { f(h.0, mw) })
    }

    /// New per-domain API (driver ≥ 555.85): offset and allowed range of
    /// `kind` in `pstate`.
    pub fn get_clock_offset(&self, h: Handle, kind: u32, pstate: u32) -> Result<OffsetInfo, String> {
        let f: unsafe extern "C" fn(usize, *mut ClockOffset) -> u32 = self.func("nvmlDeviceGetClockOffsets")?;
        let mut info = ClockOffset { version: CLOCK_OFFSET_V1, kind, pstate, ..Default::default() };
        self.check(unsafe { f(h.0, &mut info) })?;
        Ok(OffsetInfo { offset_mhz: info.offset_mhz, min_mhz: info.min_mhz, max_mhz: info.max_mhz })
    }

    pub fn set_clock_offset(&self, h: Handle, kind: u32, pstate: u32, mhz: i32) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize, *mut ClockOffset) -> u32 = self.func("nvmlDeviceSetClockOffsets")?;
        let mut info = ClockOffset { version: CLOCK_OFFSET_V1, kind, pstate, offset_mhz: mhz, ..Default::default() };
        self.check(unsafe { f(h.0, &mut info) })
    }

    /// P-states the device supports (P0 first), NVML_PSTATE_UNKNOWN entries dropped.
    pub fn supported_pstates(&self, h: Handle) -> Result<Vec<u32>, String> {
        let f: unsafe extern "C" fn(usize, *mut u32, u32) -> u32 = self.func("nvmlDeviceGetSupportedPerformanceStates")?;
        let mut buf = [PSTATE_UNKNOWN; MAX_PSTATES];
        self.check(unsafe { f(h.0, buf.as_mut_ptr(), MAX_PSTATES as u32) })?;
        Ok(buf.into_iter().filter(|&p| p < PSTATE_UNKNOWN).collect())
    }

    /// nvmlDeviceArchitecture_t: 7 Ampere, 8 Ada, 9 Hopper, 10 Blackwell.
    pub fn architecture(&self, h: Handle) -> Result<u32, String> { self.get1("nvmlDeviceGetArchitecture", h) }

    /// Clock event (throttle) reason bitmask; the old name on drivers before 535.
    pub fn clock_event_reasons(&self, h: Handle) -> Result<u64, String> {
        self.get1("nvmlDeviceGetCurrentClocksEventReasons", h)
            .or_else(|_| self.get1("nvmlDeviceGetCurrentClocksThrottleReasons", h))
    }

    /// PowerMizer (driver ≥ 580). `mode` is ignored on read.
    pub fn power_mizer(&self, h: Handle) -> Result<PowerMizerModes, String> {
        self.get1("nvmlDeviceGetPowerMizerMode_v1", h)
    }
    pub fn set_power_mizer(&self, h: Handle, mode: u32) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize, *mut PowerMizerModes) -> u32 = self.func("nvmlDeviceSetPowerMizerMode_v1")?;
        let mut m = PowerMizerModes { mode, ..Default::default() };
        self.check(unsafe { f(h.0, &mut m) })
    }

    pub fn gpc_clk_vf_offset(&self, h: Handle) -> Result<i32, String> { self.get1("nvmlDeviceGetGpcClkVfOffset", h) }
    pub fn mem_clk_vf_offset(&self, h: Handle) -> Result<i32, String> { self.get1("nvmlDeviceGetMemClkVfOffset", h) }

    pub fn set_gpc_clk_vf_offset(&self, h: Handle, v: i32) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize, i32) -> u32 = self.func("nvmlDeviceSetGpcClkVfOffset")?;
        self.check(unsafe { f(h.0, v) })
    }
    pub fn set_mem_clk_vf_offset(&self, h: Handle, v: i32) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize, i32) -> u32 = self.func("nvmlDeviceSetMemClkVfOffset")?;
        self.check(unsafe { f(h.0, v) })
    }

    pub fn mem_clk_min_max_vf_offset(&self, h: Handle) -> Result<(i32, i32), String> {
        let f: unsafe extern "C" fn(usize, *mut i32, *mut i32) -> u32 =
            self.func("nvmlDeviceGetMemClkMinMaxVfOffset")?;
        let (mut a, mut b) = (0, 0);
        self.check(unsafe { f(h.0, &mut a, &mut b) })?;
        Ok((a, b))
    }

    pub fn supported_memory_clocks(&self, h: Handle) -> Result<Vec<u32>, String> {
        let f: unsafe extern "C" fn(usize, *mut u32, *mut u32) -> u32 =
            self.func("nvmlDeviceGetSupportedMemoryClocks")?;
        let mut cap = 64u32;
        for _ in 0..3 {
            let mut buf = vec![0u32; cap as usize];
            let mut n = cap;
            let rc = unsafe { f(h.0, &mut n, buf.as_mut_ptr()) };
            if rc == ERROR_INSUFFICIENT_SIZE && n > cap && n <= 4096 { cap = n; continue; }
            self.check(rc)?;
            buf.truncate(n.min(cap) as usize);
            return Ok(buf);
        }
        Err("nvmlDeviceGetSupportedMemoryClocks: list keeps growing".into())
    }

    pub fn set_memory_locked_clocks(&self, h: Handle, min: u32, max: u32) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize, u32, u32) -> u32 = self.func("nvmlDeviceSetMemoryLockedClocks")?;
        self.check(unsafe { f(h.0, min, max) })
    }

    pub fn reset_memory_locked_clocks(&self, h: Handle) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize) -> u32 = self.func("nvmlDeviceResetMemoryLockedClocks")?;
        self.check(unsafe { f(h.0) })
    }

    /// Core clock window (MHz). With a flattened / undervolted curve, capping
    /// the max here makes the GPU run the lowest-voltage point that reaches it.
    pub fn set_gpu_locked_clocks(&self, h: Handle, min: u32, max: u32) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize, u32, u32) -> u32 = self.func("nvmlDeviceSetGpuLockedClocks")?;
        self.check(unsafe { f(h.0, min, max) })
    }

    pub fn reset_gpu_locked_clocks(&self, h: Handle) -> Result<(), String> {
        let f: unsafe extern "C" fn(usize) -> u32 = self.func("nvmlDeviceResetGpuLockedClocks")?;
        self.check(unsafe { f(h.0) })
    }
}
