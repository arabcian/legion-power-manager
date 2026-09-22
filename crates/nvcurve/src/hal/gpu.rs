//! GPU discovery and initialization (port of hal/gpu.py).

use crate::nvapi::{call0, call2, fid, Gpu, NvError};
use crate::nvml;
use crate::types::GpuInfo;
use std::ffi::{c_void, CStr};

/// NvAPI's documented ceiling for EnumPhysicalGPUs.
const MAX_PHYSICAL_GPUS: usize = 64;

pub fn init_nvapi() -> Result<(), NvError> {
    match call0(fid::INITIALIZE)? {
        Some(0) => Ok(()),
        _ => Err(NvError::Unavailable("NvAPI_Initialize failed".into())),
    }
}

pub fn enumerate_gpus() -> Result<Vec<Gpu>, NvError> {
    let mut handles = [std::ptr::null_mut::<c_void>(); MAX_PHYSICAL_GPUS];
    let mut n: i32 = 0;
    let rc = call2(fid::ENUM_PHYSICAL_GPUS,
                   handles.as_mut_ptr() as *mut c_void,
                   &mut n as *mut i32 as *mut c_void)?
        .ok_or_else(|| NvError::Unavailable("EnumPhysicalGPUs not exported by driver".into()))?;
    if rc != 0 {
        return Err(NvError::Unavailable(format!("EnumPhysicalGPUs failed (code {rc})")));
    }
    if n <= 0 {
        return Err(NvError::NoGpu);
    }
    // Never trust a count larger than the array we handed the driver.
    let count = (n as usize).min(MAX_PHYSICAL_GPUS);
    Ok(handles[..count].iter().map(|p| Gpu(*p as usize)).collect())
}

pub fn gpu_name(gpu: Gpu) -> String {
    let mut buf = [0u8; 256];
    let _ = call2(fid::GET_FULL_NAME, gpu.ptr(), buf.as_mut_ptr() as *mut c_void);
    *buf.last_mut().unwrap() = 0;
    CStr::from_bytes_until_nul(&buf).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Initialize NvAPI and return (handle, name) for `index`.
pub fn get_gpu(index: usize) -> Result<(Gpu, String), NvError> {
    init_nvapi()?;
    let gpus = enumerate_gpus()?;
    let gpu = *gpus.get(index).ok_or_else(|| NvError::GpuIndex(
        format!("GPU index {index} out of range (found {} GPU(s))", gpus.len())))?;
    Ok((gpu, gpu_name(gpu)))
}

/// All physical GPUs with NVML UUID / PCI bus when available.
/// Zero GPUs is Ok(vec![]); a broken driver is Err.
pub fn discover_gpus() -> Result<Vec<GpuInfo>, NvError> {
    init_nvapi()?;
    let gpus = match enumerate_gpus() {
        Ok(g) => g,
        Err(NvError::NoGpu) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let nv = nvml::ready().ok();
    Ok(gpus.iter().enumerate().map(|(i, &g)| {
        let (mut uuid, mut pci_bus_id) = (None, None);
        if let Some(nv) = nv {
            if let Ok(h) = nv.handle(i as u32) {
                uuid = nv.uuid(h).ok();
                pci_bus_id = nv.pci_bus(h).ok();
            }
        }
        GpuInfo { name: gpu_name(g), index: i, uuid, pci_bus_id }
    }).collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_driver_is_an_error_not_a_crash() {
        if std::path::Path::new("/proc/driver/nvidia").exists() { return; }
        assert!(matches!(super::discover_gpus(), Err(crate::NvError::Unavailable(_))));
        assert!(crate::hal::limits::get_power_limit(0).power_limit_w.is_none());
        assert!(crate::hal::limits::set_power_limit(100, 0).is_err());
    }
}
