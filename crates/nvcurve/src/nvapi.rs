//! NvAPI bootstrap: dlopen, QueryInterface, versioned struct calls.
//! Port of nvapi/{bootstrap,constants,errors}.py.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::fmt;
use std::sync::{Mutex, OnceLock};

// ── Function IDs ────────────────────────────────────────────────────────────
pub mod fid {
    pub const INITIALIZE: u32 = 0x0150_E828;
    pub const ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
    pub const GET_FULL_NAME: u32 = 0xCEEE_8E9F;
    pub const GET_VFP_CURVE: u32 = 0x2153_7AD4;
    pub const GET_CLOCK_BOOST_MASK: u32 = 0x507B_4B59;
    pub const GET_CLOCK_BOOST_TABLE: u32 = 0x23F1_B133;
    pub const GET_CURRENT_VOLTAGE: u32 = 0x465F_9BCF;
    pub const GET_CLOCK_BOOST_RANGES: u32 = 0x64B4_3A6A;
    pub const GET_PERF_LIMITS: u32 = 0xE440_B867;
    pub const GET_VOLT_BOOST_PERCENT: u32 = 0x9DF2_3CA1;
    pub const SET_CLOCK_BOOST_TABLE: u32 = 0x0733_E009;
}

// ── Struct layouts (verified on GB202, driver 590.48.01) ────────────────────
pub const VFP_SIZE: usize = 0x1C28;
pub const VFP_BASE: usize = 0x48;
pub const VFP_STRIDE: usize = 0x1C;
pub const VFP_POINTS: usize = (VFP_SIZE - VFP_BASE) / VFP_STRIDE;

pub const CT_SIZE: usize = 0x2420;
pub const CT_BASE: usize = 0x44;
pub const CT_STRIDE: usize = 0x24;
pub const CT_DELTA_OFF: usize = 0x14;
pub const CT_POINTS: usize = (CT_SIZE - CT_BASE) / CT_STRIDE;

pub const MASK_SIZE: usize = 0x182C;
pub const VOLT_SIZE: usize = 0x004C;
pub const RANGES_SIZE: usize = 0x0928;
pub const PERF_SIZE: usize = 0x030C;
pub const VBOOST_SIZE: usize = 0x0028;

/// Upward cap: +1000 MHz (Blackwell driver limit for positive offsets).
pub const MAX_DELTA_KHZ: i64 = 1_000_000;
/// Downward floor: -2000 MHz. Negative offsets only lower clocks (a flattened
/// undervolt curve pulls the top points down by well over 1000 MHz), and the
/// driver accepts them; the old symmetric ±1000 clamp broke such curves.
pub const MIN_DELTA_KHZ: i64 = -2_000_000;

/// Linux NvAPI uses small negative codes.
pub fn error_name(code: i32) -> String {
    match code {
        0 => "OK", -1 => "GENERIC_ERROR", -5 => "INVALID_ARGUMENT",
        -6 => "NVIDIA_DEVICE_NOT_FOUND", -7 => "END_ENUMERATION", -8 => "INVALID_HANDLE",
        -9 => "INCOMPATIBLE_STRUCT_VERSION", -10 => "HANDLE_INVALIDATED",
        -14 => "INVALID_POINTER",
        _ => return format!("unknown ({code})"),
    }.to_owned()
}

#[derive(Debug, Clone)]
pub enum NvError {
    /// Driver library missing or NvAPI_Initialize failed.
    Unavailable(String),
    NoGpu,
    GpuIndex(String),
}

impl fmt::Display for NvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NvError::Unavailable(m) | NvError::GpuIndex(m) => f.write_str(m),
            NvError::NoGpu => f.write_str("No NVIDIA GPUs found"),
        }
    }
}
impl std::error::Error for NvError {}

/// Opaque NvPhysicalGpuHandle. Stored as usize so it is Send/Sync; the
/// driver owns the pointee and handles stay valid for the process lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Gpu(pub(crate) usize);
impl Gpu {
    pub(crate) fn ptr(self) -> *mut c_void { self.0 as *mut c_void }
}

// ── Aligned struct buffer ───────────────────────────────────────────────────

/// Byte buffer backed by u64 words so the driver always sees 8-byte
/// alignment, with little-endian field accessors.
#[derive(Clone)]
pub struct Buf {
    words: Vec<u64>,
    len: usize,
}

impl Buf {
    pub fn new(len: usize) -> Self { Buf { words: vec![0; (len + 7) / 8], len } }
    pub fn from_bytes(b: &[u8]) -> Self {
        let mut s = Self::new(b.len());
        s.bytes_mut().copy_from_slice(b);
        s
    }
    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.words.as_ptr() as *const u8, self.len) }
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr() as *mut u8, self.len) }
    }
    pub(crate) fn as_mut_ptr(&mut self) -> *mut c_void { self.words.as_mut_ptr() as *mut c_void }
    pub fn u32_at(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.bytes()[off..off + 4].try_into().unwrap())
    }
    pub fn i32_at(&self, off: usize) -> i32 { self.u32_at(off) as i32 }
    pub fn put_u32(&mut self, off: usize, v: u32) {
        self.bytes_mut()[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    pub fn put_i32(&mut self, off: usize, v: i32) { self.put_u32(off, v as u32) }
}

pub fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
pub fn read_i32(b: &[u8], off: usize) -> i32 { read_u32(b, off) as i32 }

// ── Library loading ─────────────────────────────────────────────────────────

type QiFn = unsafe extern "C" fn(u32) -> *mut c_void;
type F0 = unsafe extern "C" fn() -> i32;
type F2 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;

struct Lib {
    qi: QiFn,
    cache: Mutex<HashMap<u32, usize>>,
}

static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();

fn load() -> Result<Lib, String> {
    for name in [&b"libnvidia-api.so\0"[..], &b"libnvidia-api.so.1\0"[..]] {
        let cname = CStr::from_bytes_with_nul(name).unwrap();
        let h = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if h.is_null() { continue; }
        let sym = unsafe { libc::dlsym(h, b"nvapi_QueryInterface\0".as_ptr() as *const libc::c_char) };
        if sym.is_null() { continue; }
        let qi: QiFn = unsafe { std::mem::transmute::<*mut c_void, QiFn>(sym) };
        return Ok(Lib { qi, cache: Mutex::new(HashMap::new()) });
    }
    Err("Cannot load libnvidia-api.so — ensure the NVIDIA proprietary driver is installed.".into())
}

fn lib() -> Result<&'static Lib, NvError> {
    LIB.get_or_init(load).as_ref().map_err(|e| NvError::Unavailable(e.clone()))
}

/// Resolves a function pointer by id. Ok(None) = driver does not export it.
pub fn query_interface(id: u32) -> Result<Option<usize>, NvError> {
    let l = lib()?;
    let mut cache = l.cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(&p) = cache.get(&id) { return Ok(Some(p)); }
    let p = unsafe { (l.qi)(id) } as usize;
    if p == 0 { return Ok(None); }
    cache.insert(id, p);
    Ok(Some(p))
}

pub(crate) fn call0(id: u32) -> Result<Option<i32>, NvError> {
    Ok(query_interface(id)?.map(|p| unsafe { std::mem::transmute::<usize, F0>(p)() }))
}

pub(crate) fn call2(id: u32, a: *mut c_void, b: *mut c_void) -> Result<Option<i32>, NvError> {
    Ok(query_interface(id)?.map(|p| unsafe { std::mem::transmute::<usize, F2>(p)(a, b) }))
}

const NOT_FOUND: &str = "function pointer not found (driver too old?)";

/// Versioned-struct call: allocates `size` bytes, writes `(ver<<16)|size`
/// at offset 0, lets `pre_fill` populate request fields, then calls.
pub fn nvcall(id: u32, gpu: Gpu, size: usize, ver: u32, pre_fill: impl FnOnce(&mut Buf))
    -> Result<Buf, String>
{
    let mut buf = Buf::new(size);
    buf.put_u32(0, (ver << 16) | size as u32);
    pre_fill(&mut buf);
    match call2(id, gpu.ptr(), buf.as_mut_ptr()) {
        Err(e) => Err(e.to_string()),
        Ok(None) => Err(NOT_FOUND.into()),
        Ok(Some(0)) => Ok(buf),
        Ok(Some(r)) => Err(format!("error {r} ({})", error_name(r))),
    }
}

/// Call with a caller-built buffer (writes). Returns (code, description).
pub fn nvcall_raw(id: u32, gpu: Gpu, buf: &mut Buf) -> (i32, String) {
    match call2(id, gpu.ptr(), buf.as_mut_ptr()) {
        Err(e) => (-999, e.to_string()),
        Ok(None) => (-999, NOT_FOUND.into()),
        Ok(Some(r)) => (r, error_name(r)),
    }
}
