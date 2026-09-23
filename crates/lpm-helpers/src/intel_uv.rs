//! Intel undervolt core: OC mailbox voltage offsets / IccMax, TCC offset,
//! package power limits (MSR + MCHBAR mirror).
//!
//! Every mechanism here was cross-checked against three implementations:
//!   * intel-undervolt (kitsunyan, C)      undervolt.c / config.c
//!   * undervolt       (georgewhewell, Py) undervolt.py
//!   * throttled       (erpalma, Py)       throttled.py / mmio.py
//!
//! Where they disagree, the choice made here is documented at that spot.
//!
//! OC mailbox (MSR 0x150), all three agree:
//!   write offset : 0x8000_0011_0000_0000 | plane<<40 | ((ticks & 0x7FF) << 21)
//!   read request : 0x8000_0010_0000_0000 | plane<<40, then read MSR 0x150
//!   ticks        : round(mV * 1.024), signed 11 bit (1/1024 V units)
//!   planes       : 0 core, 1 iGPU, 2 cache, 3 system agent, 4 analog I/O
//!   verify       : read back, compare the low 32 bits (intel-undervolt,
//!                  throttled); undervolt.py compares decoded mV — same test.
//! IccMax (throttled only): read 0x8000_0016.., write 0x8000_0017.. | plane<<40
//!   | field, field = floor(A*4), 10 bit.
//! MSR 0x150 is package scoped: intel-undervolt uses /dev/cpu/0/msr only,
//! the Python tools broadcast to every CPU (each CPU is a separate mailbox
//! transaction carrying the same command). One transaction on CPU 0 is used.

use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

pub const MSR_PLATFORM_INFO: u64 = 0xCE;
pub const MSR_OC_MAILBOX: u64 = 0x150;
pub const MSR_TEMPERATURE_TARGET: u64 = 0x1A2;
pub const MSR_RAPL_POWER_UNIT: u64 = 0x606;
pub const MSR_PKG_POWER_LIMIT: u64 = 0x610;
pub const IA32_PERF_STATUS: u64 = 0x198;
pub const IA32_THERM_STATUS: u64 = 0x19C;
pub const MSR_POWER_CTL: u64 = 0x1FC;
pub const MSR_PKG_ENERGY_STATUS: u64 = 0x611;
pub const MSR_DRAM_ENERGY_STATUS: u64 = 0x619;
pub const MSR_PP0_ENERGY_STATUS: u64 = 0x639;
pub const MSR_PP1_ENERGY_STATUS: u64 = 0x641;
pub const MSR_CONFIG_TDP_CONTROL: u64 = 0x64B;
/// undervolt.py --force allows any positive offset; capped here (overvolting
/// beyond this has no stability use and only adds heat/wear).
pub const UV_MAX_POSITIVE_MV: f64 = 250.0;

pub const TICKS_PER_MV: f64 = 1.024;
pub const UV_MIN_MV: f64 = -1000.0; // -1024 ticks, signed 11-bit floor (throttled)
/// IccMax field width. throttled uses 10 bits (0x3FF, max 255.75 A), which
/// truncates current HX parts: a Core Ultra 7 255HX reports 0x41C = 263 A and
/// throttled/10-bit decoding shows 7 A. The field is 11 bits (max 511.75 A).
pub const ICC_MAX_FIELD: u64 = 0x7FF;

pub const MCHBAR_PKG_LIMIT_OFFSET: u64 = 0x59A0;
const HOST_BRIDGE: &str = "/sys/bus/pci/devices/0000:00:00.0";

/// (key, plane index, label). undervolt.py leaves "digitalio" (5) out as "not working?".
pub const PLANES: &[(&str, u64, &str)] = &[
    ("core", 0, "CPU Core"),
    ("gpu", 1, "Integrated GPU"),
    ("cache", 2, "CPU Cache"),
    ("uncore", 3, "System Agent"),
    ("analogio", 4, "Analog I/O"),
];
/// IccMax exists for core/gpu/cache only (throttled CURRENT_PLANES).
pub const ICC_PLANES: &[&str] = &["core", "gpu", "cache"];

pub fn plane_index(key: &str) -> Option<u64> {
    PLANES.iter().find(|p| p.0 == key).map(|p| p.1)
}

// ── pure encoding (unit-tested against the originals' vectors) ─────────────

pub fn mv_to_ticks(mv: f64) -> Result<i64, String> { mv_to_ticks_ex(mv, false) }

/// `allow_positive` = undervolt.py's --force (throttled never allows it).
pub fn mv_to_ticks_ex(mv: f64, allow_positive: bool) -> Result<i64, String> {
    let hi = if allow_positive { UV_MAX_POSITIVE_MV } else { 0.0 };
    if !mv.is_finite() || !(UV_MIN_MV..=hi).contains(&mv) {
        return Err(if allow_positive { format!("offset must be between {UV_MIN_MV} and +{hi} mV, got {mv}") }
                   else { format!("offset must be between {UV_MIN_MV} and 0 mV, got {mv} (positive needs allow_positive)") });
    }
    // f64::round = half away from zero == intel-undervolt's (|v|*1.024 + 0.5)
    // truncation. (undervolt.py's banker's round differs only on exact .5 ties.)
    Ok((mv * TICKS_PER_MV).round() as i64)
}

pub fn encode_offset(ticks: i64) -> u64 { ((ticks as u64) & 0x7FF) << 21 }

pub fn uv_write_cmd(plane: u64, ticks: i64) -> u64 {
    0x8000_0011_0000_0000 | (plane << 40) | encode_offset(ticks)
}
pub fn uv_read_cmd(plane: u64) -> u64 { 0x8000_0010_0000_0000 | (plane << 40) }

/// Offset in mV from a mailbox response. Only the low 32 bits carry data
/// (throttled masks them; undervolt.py only strips the plane bits, which is
/// wrong if the response status byte is non-zero).
pub fn decode_mv(resp: u64) -> f64 {
    let x = (((resp & 0xFFFF_FFFF) >> 21) & 0x7FF) as i64;
    let t = if x >= 0x400 { x - 0x800 } else { x };
    t as f64 / TICKS_PER_MV
}

pub fn amps_to_icc_field(a: f64) -> Result<u64, String> {
    let max = ICC_MAX_FIELD as f64 / 4.0;
    if !a.is_finite() || !(a > 0.0 && a <= max) {
        return Err(format!("IccMax must be in (0, {max}] A, got {a}"));
    }
    let f = (a * 4.0).floor() as u64; // floor: never above the requested ceiling (throttled)
    if !(1..=ICC_MAX_FIELD).contains(&f) { return Err(format!("IccMax {a} A quantises outside the 11-bit field")); }
    Ok(f)
}
pub fn icc_read_cmd(plane: u64) -> u64 { 0x8000_0016_0000_0000 | (plane << 40) }
pub fn icc_write_cmd(plane: u64, field: u64) -> u64 { 0x8000_0017_0000_0000 | (plane << 40) | (field & ICC_MAX_FIELD) }

/// RAPL time window field (7 bit: Y = bits 0-4, Z = bits 5-6):
/// t = 2^Y * (1 + Z/4) * time_unit.
pub fn tw_to_seconds(field: u64, time_unit_s: f64) -> f64 {
    let y = (field & 0x1F) as i32;
    let z = ((field >> 5) & 0x3) as f64;
    2f64.powi(y) * (1.0 + z / 4.0) * time_unit_s
}

/// Nearest encodable window, as intel-undervolt / undervolt.py do (throttled
/// rounds up instead). Saturates at Y=31,Z=3: undervolt.py returns 0xFE there,
/// which in its Z<<5|Y layout spills into bit 7 → bit 56 of the MSR (bug);
/// intel-undervolt's 0xFE is in its own (Z<<6|Y<<1) layout and is correct.
pub fn seconds_to_tw(seconds: f64, time_unit_s: f64) -> Result<u64, String> {
    if !seconds.is_finite() || seconds <= 0.0 { return Err(format!("time window must be > 0 s, got {seconds}")); }
    let val = seconds / time_unit_s;
    let (mut best, mut best_err) = (0u64, f64::MAX);
    for z in 0..4u64 {
        let m = 1.0 + z as f64 / 4.0;
        let vm = val / m;
        let mut e: i32 = if vm < 1.0 { 0 } else { vm.log2().floor() as i32 };
        if vm >= 1.0 && vm - 2f64.powi(e) >= 2f64.powi(e + 1) - vm { e += 1; }
        let e = e.clamp(0, 31);
        let err = (2f64.powi(e) * m - val).abs();
        if err < best_err { best_err = err; best = (z << 5) | e as u64; }
    }
    Ok(best)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Units { pub power_w: f64, pub time_s: f64 }

/// MSR 0x606: power unit = 1/2^bits[3:0] W, time unit = 1/2^bits[19:16] s.
pub fn decode_units(raw: u64) -> Units {
    Units { power_w: 1.0 / 2f64.powi((raw & 0xF) as i32), time_s: 1.0 / 2f64.powi(((raw >> 16) & 0xF) as i32) }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Term { pub watts: f64, pub seconds: f64, pub enabled: bool, pub clamp: bool }

/// MSR_PKG_POWER_LIMIT: PL1 0-14, en 15, clamp 16, TW1 17-23,
/// PL2 32-46, en 47, clamp 48, TW2 49-55, lock 63.
pub fn decode_pl(raw: u64, u: Units) -> (Term, Term, bool) {
    let t = |sh: u32| Term {
        watts: ((raw >> sh) & 0x7FFF) as f64 * u.power_w,
        seconds: tw_to_seconds((raw >> (sh + 17)) & 0x7F, u.time_s),
        enabled: (raw >> (sh + 15)) & 1 == 1,
        clamp: (raw >> (sh + 16)) & 1 == 1,
    };
    (t(0), t(32), raw >> 63 == 1)
}

/// Read-modify-write of one term. Only that term's power, enable and window
/// fields are replaced; clamp bits and every other bit keep their current
/// value (undervolt.py keeps them via backup_rest, intel-undervolt via its
/// masks). Enable is set, as all three do when a limit is written.
pub fn encode_pl_term(raw: u64, second: bool, watts: f64, seconds: Option<f64>, u: Units) -> Result<u64, String> {
    let sh = if second { 32 } else { 0 };
    let name = if second { "PL2" } else { "PL1" };
    if !watts.is_finite() || watts <= 0.0 { return Err(format!("{name} must be > 0 W")); }
    let p = (watts / u.power_w).round() as u64;
    // intel-undervolt clamps to 0x7fff/unit but then writes the *watt* value
    // unscaled; undervolt.py/throttled reject. Rejecting is the correct one.
    if !(1..=0x7FFF).contains(&p) { return Err(format!("{name} {watts} W does not fit the 15-bit field")); }
    let mut v = raw & !(0x7FFFu64 << sh) & !(1u64 << (sh + 15));
    v |= (p << sh) | (1u64 << (sh + 15));
    if let Some(s) = seconds {
        let tw = seconds_to_tw(s, u.time_s)?;
        v = (v & !(0x7Fu64 << (sh + 17))) | (tw << (sh + 17));
    }
    Ok(v)
}

// ── MSR access ─────────────────────────────────────────────────────────────

pub struct Msr { f: File }

impl Msr {
    /// /dev/cpu/0/msr, loading the msr module first if needed.
    pub fn open(write: bool) -> io::Result<Msr> {
        let dev = Path::new("/dev/cpu/0/msr");
        if !dev.exists() { let _ = modprobe_msr(); }
        let f = OpenOptions::new().read(true).write(write)
            .custom_flags(libc::O_CLOEXEC).open(dev)?;
        Ok(Msr { f })
    }
    pub fn read(&self, addr: u64) -> io::Result<u64> {
        let mut b = [0u8; 8];
        if self.f.read_at(&mut b, addr)? != 8 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short MSR read")); }
        Ok(u64::from_ne_bytes(b))
    }
    pub fn write(&self, addr: u64, v: u64) -> io::Result<()> {
        if self.f.write_at(&v.to_ne_bytes(), addr)? != 8 { return Err(io::Error::new(io::ErrorKind::WriteZero, "short MSR write")); }
        Ok(())
    }
    pub fn mailbox(&self, cmd: u64) -> io::Result<u64> {
        self.write(MSR_OC_MAILBOX, cmd)?;
        self.read(MSR_OC_MAILBOX)
    }
}

fn modprobe_msr() -> bool {
    for p in ["/sbin/modprobe", "/usr/sbin/modprobe", "/usr/bin/modprobe", "/bin/modprobe"] {
        if Path::new(p).is_file() {
            return std::process::Command::new(p).arg("msr").env_clear()
                .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        }
    }
    false
}

/// throttled's set_msr_allow_writes(): writes stay allowed with "default"
/// but every 0x150 write then logs a kernel warning.
pub fn ensure_allow_writes() -> Option<String> {
    let p = Path::new("/sys/module/msr/parameters/allow_writes");
    let cur = std::fs::read_to_string(p).ok()?.trim().to_owned();
    if cur == "on" { return Some(cur); }
    let _ = crate::sysfs_write(p, b"on");
    Some(std::fs::read_to_string(p).ok()?.trim().to_owned())
}

pub fn lockdown() -> Option<String> {
    let s = std::fs::read_to_string("/sys/kernel/security/lockdown").ok()?;
    s.split_whitespace().find(|w| w.starts_with('[')).map(|w| w.trim_matches(|c| c == '[' || c == ']').to_owned())
}

fn err_str(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) =>
            format!("{e} — MSR writes blocked (kernel lockdown / Secure Boot, or msr.allow_writes=off)"),
        Some(libc::EIO) => format!("{e} — the CPU rejected this MSR (not implemented on this model)"),
        _ => e.to_string(),
    }
}

// ── CPU identity ───────────────────────────────────────────────────────────

pub struct CpuId { pub vendor: String, pub family: u32, pub model: u32, pub stepping: u32, pub name: String }

pub fn cpu_id() -> Option<CpuId> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let mut c = CpuId { vendor: String::new(), family: 0, model: 0, stepping: 0, name: String::new() };
    for line in text.lines() {
        if line.trim().is_empty() { break; } // first processor block only
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim() {
            "vendor_id" => c.vendor = v.into(),
            "cpu family" => c.family = v.parse().unwrap_or(0),
            "model" => c.model = v.parse().unwrap_or(0),
            "stepping" => c.stepping = v.parse().unwrap_or(0),
            "model name" => c.name = v.into(),
            _ => {}
        }
    }
    Some(c)
}

/// Codename by (family 6) model, from throttled's supported_cpus table.
/// Informational only: unlike throttled, an unknown model is not refused —
/// the readback check is what decides whether a write took effect.
pub fn codename(model: u32) -> &'static str {
    match model {
        42 => "Sandy Bridge", 58 => "Ivy Bridge", 60 | 69 | 70 => "Haswell", 61 | 71 => "Broadwell",
        78 | 94 => "Skylake", 142 | 158 => "Kaby/Coffee/Whiskey Lake", 102 => "Cannon Lake",
        126 => "Ice Lake", 140 | 141 => "Tiger Lake", 165 | 166 => "Comet Lake", 167 => "Rocket Lake",
        151 | 154 => "Alder Lake", 183 | 186 | 191 => "Raptor Lake", 170 => "Meteor Lake",
        189 => "Lunar Lake", 181 | 197 | 198 => "Arrow Lake", 204 => "Panther Lake",
        _ => "unknown",
    }
}

// ── MCHBAR package power-limit mirror (throttled method) ───────────────────

const M39_15: u64 = ((1u64 << 39) - 1) & !((1u64 << 15) - 1);
const M39_17: u64 = ((1u64 << 39) - 1) & !((1u64 << 17) - 1);
const M42_17: u64 = ((1u64 << 42) - 1) & !((1u64 << 17) - 1);

/// Host bridge device id → MCHBAR address mask, verbatim from throttled.
fn mchbar_mask(dev: u32) -> Option<u64> {
    Some(match dev {
        0x5914 | 0x3EC4 | 0x9B54 => M39_15,
        0x9A14 => M39_17,
        0x4601 | 0x4602 | 0x4621 | 0x4641 | 0x4660 | 0x4668 | 0x4648 | 0xA703 | 0x4640 | 0x4630
        | 0xA700 | 0xA740 | 0xA704 | 0xA702 | 0xA706 | 0xA707 | 0xA708 | 0xA716 | 0xA718
        | 0x7D21 | 0x7D22 | 0x7D23 | 0x7D24 | 0x7D01 | 0x7D02 | 0x7D14 | 0x7D06 | 0x7D20 | 0x7D30 => M42_17,
        _ => return None,
    })
}

/// Validated MCHBAR base: Intel host bridge with a known device id, enable
/// bit set, no bit outside the per-generation mask. intel-undervolt instead
/// hardcodes 0xFED159A0 (MCHBAR 0xFED10000), which is wrong on newer
/// platforms — not replicated. Config space is read from sysfs (root sees
/// all 256 bytes) instead of spawning setpci.
pub fn mchbar_base() -> Result<u64, String> {
    let rd = |f: &str| std::fs::read_to_string(format!("{HOST_BRIDGE}/{f}")).ok()
        .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
    let (Some(ven), Some(dev)) = (rd("vendor"), rd("device")) else { return Err("PCI host bridge not readable".into()) };
    if ven != 0x8086 { return Err(format!("host bridge vendor {ven:#06x} is not Intel")); }
    let Some(mask) = mchbar_mask(dev) else { return Err(format!("unknown host bridge {dev:#06x} (not in throttled's table)")) };
    let f = File::open(format!("{HOST_BRIDGE}/config")).map_err(|e| format!("PCI config: {e}"))?;
    let mut b = [0u8; 8];
    if f.read_at(&mut b, 0x48).map_err(|e| format!("PCI config: {e}"))? != 8 { return Err("PCI config too short".into()); }
    let bar = u64::from_le_bytes(b);
    if bar & 1 == 0 || bar & !(mask | 1) != 0 { return Err(format!("MCHBAR {bar:#x} disabled or malformed")); }
    let base = bar & mask;
    if base == 0 { return Err("MCHBAR base is 0".into()); }
    Ok(base)
}

pub struct Mmio { ptr: *mut u8, len: usize, off: usize }

impl Mmio {
    pub fn open(phys: u64) -> io::Result<Mmio> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let aligned = phys & !(page - 1);
        let off = (phys - aligned) as usize;
        let len = off + 8;
        let f = OpenOptions::new().read(true).write(true)
            .custom_flags(libc::O_SYNC | libc::O_CLOEXEC).open("/dev/mem")?;
        use std::os::fd::AsRawFd;
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
                                    libc::MAP_SHARED, f.as_raw_fd(), aligned as libc::off_t) };
        if p == libc::MAP_FAILED { return Err(io::Error::last_os_error()); }
        Ok(Mmio { ptr: p as *mut u8, len, off })
    }
    pub fn read64(&self) -> u64 { unsafe { std::ptr::read_volatile(self.ptr.add(self.off) as *const u64) } }
    pub fn write64(&self, v: u64) { unsafe { std::ptr::write_volatile(self.ptr.add(self.off) as *mut u64, v) } }
}
impl Drop for Mmio {
    fn drop(&mut self) { unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len); } }
}

// ── profile ────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct PlReq { pub watts: f64, pub seconds: Option<f64> }

/// None anywhere = leave that register untouched.
#[derive(Debug, Default, Clone)]
pub struct Profile {
    pub voltage: Vec<(&'static str, u64, f64)>,
    pub iccmax: Vec<(&'static str, u64, f64)>,
    pub tjoffset: Option<u64>,
    pub pl1: Option<PlReq>,
    pub pl2: Option<PlReq>,
    pub mchbar: bool,
    /// undervolt.py --lock-power-limit: set bit 63 after writing (until reset).
    pub lock_power: bool,
    pub allow_positive: bool,
    /// throttled Disable_BDPROCHOT: Some(true) clears MSR_POWER_CTL bit 0,
    /// Some(false) sets it back.
    pub disable_bdprochot: Option<bool>,
    /// throttled cTDP: 0 nominal, 1 down, 2 up (MSR_CONFIG_TDP_CONTROL).
    pub ctdp: Option<u64>,
    /// intel-undervolt hwphint rules (used by the daemon only).
    pub hwphint: Vec<HwpRule>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HwpAlgo {
    /// load:single|multi:threshold (0..1)
    Load { multi: bool, threshold: f64 },
    /// power:domain:gt|lt:W[:and|or]... — first term's connective is ignored
    Power(Vec<PowerTerm>),
}
#[derive(Debug, Clone, PartialEq)]
pub struct PowerTerm { pub domain: String, pub greater: bool, pub watts: f64, pub and: bool }
#[derive(Debug, Clone, PartialEq)]
pub struct HwpRule { pub force: bool, pub algo: HwpAlgo, pub load_hint: String, pub normal_hint: String }

fn num(v: &Value, what: &str) -> Result<Option<f64>, String> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_f64().map(Some).ok_or_else(|| format!("{what}: bad number")),
        _ => Err(format!("{what} must be a number or null")),
    }
}

fn obj<'a>(v: Option<&'a Value>, what: &str) -> Result<Option<&'a Map<String, Value>>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => Ok(Some(m)),
        _ => Err(format!("{what} must be an object")),
    }
}

/// Parses and range-checks a whole profile before anything is written.
pub fn parse_profile(v: &Value) -> Result<Profile, String> {
    let root = v.as_object().ok_or("profile must be an object")?;
    let mut p = Profile { mchbar: true, ..Default::default() };
    let flag = |k: &str| -> Result<bool, String> {
        match root.get(k) { None | Some(Value::Null) => Ok(false), Some(Value::Bool(b)) => Ok(*b), _ => Err(format!("{k} must be a bool")) }
    };
    p.allow_positive = flag("allow_positive")?;
    p.lock_power = flag("lock_power")?;
    match root.get("disable_bdprochot") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => p.disable_bdprochot = Some(*b),
        _ => return Err("disable_bdprochot must be a bool or null".into()),
    }
    if let Some(c) = num(root.get("ctdp").unwrap_or(&Value::Null), "ctdp")? {
        if c.fract() != 0.0 || !(0.0..=2.0).contains(&c) { return Err("ctdp must be 0 (nominal), 1 (down) or 2 (up)".into()); }
        p.ctdp = Some(c as u64);
    }
    if let Some(v) = root.get("hwphint").filter(|v| !v.is_null()) {
        let arr = v.as_array().ok_or("hwphint must be a list")?;
        if arr.len() > 8 { return Err("at most 8 hwphint rules".into()); }
        for (i, r) in arr.iter().enumerate() { p.hwphint.push(parse_hwp_rule(r).map_err(|e| format!("hwphint[{i}]: {e}"))?); }
    }
    if let Some(m) = obj(root.get("voltage"), "voltage")? {
        for k in m.keys() { if plane_index(k).is_none() { return Err(format!("unknown voltage plane '{k}'")); } }
        for &(k, idx, _) in PLANES {
            if let Some(mv) = num(m.get(k).unwrap_or(&Value::Null), &format!("voltage.{k}"))? {
                mv_to_ticks_ex(mv, p.allow_positive).map_err(|e| format!("voltage.{k}: {e}"))?;
                p.voltage.push((k, idx, mv));
            }
        }
    }
    if let Some(m) = obj(root.get("iccmax"), "iccmax")? {
        for k in m.keys() { if !ICC_PLANES.contains(&k.as_str()) { return Err(format!("unknown IccMax plane '{k}'")); } }
        for &(k, idx, _) in PLANES.iter().filter(|p| ICC_PLANES.contains(&p.0)) {
            if let Some(a) = num(m.get(k).unwrap_or(&Value::Null), &format!("iccmax.{k}"))? {
                amps_to_icc_field(a).map_err(|e| format!("iccmax.{k}: {e}"))?;
                p.iccmax.push((k, idx, a));
            }
        }
    }
    if let Some(t) = num(root.get("tjoffset").unwrap_or(&Value::Null), "tjoffset")? {
        // 6-bit field; negative input accepted like intel-undervolt ("tjoffset -20").
        let t = t.abs();
        if t.fract() != 0.0 || t > 63.0 { return Err(format!("tjoffset must be an integer 0..63 °C, got {t}")); }
        p.tjoffset = Some(t as u64);
    }
    if let Some(pw) = obj(root.get("power"), "power")? {
        for (key, slot) in [("pl1", &mut p.pl1), ("pl2", &mut p.pl2)] {
            if let Some(t) = obj(pw.get(key), &format!("power.{key}"))? {
                let w = num(t.get("watts").unwrap_or(&Value::Null), &format!("power.{key}.watts"))?
                    .ok_or_else(|| format!("power.{key}.watts is required"))?;
                if !(1.0..=1000.0).contains(&w) { return Err(format!("power.{key}.watts must be 1..1000 W")); }
                let s = num(t.get("seconds").unwrap_or(&Value::Null), &format!("power.{key}.seconds"))?;
                if let Some(s) = s { if !(0.0005..=1.0e5).contains(&s) { return Err(format!("power.{key}.seconds out of range")); } }
                *slot = Some(PlReq { watts: w, seconds: s });
            }
        }
        if let Some(b) = pw.get("mchbar") { p.mchbar = b.as_bool().ok_or("power.mchbar must be a bool")?; }
    }
    if p.lock_power && p.pl1.is_none() && p.pl2.is_none() { return Err("lock_power needs pl1 and/or pl2".into()); }
    Ok(p)
}

fn valid_hint(s: &str) -> bool { !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') }

/// {"mode": "switch"|"force", "load": {"multi": bool, "threshold": 0.8}
///  | "power": [{"domain": "core", "gt": true, "watts": 8, "and": false}],
///  "load_hint": "performance", "normal_hint": "balance_performance"}
pub fn parse_hwp_rule(v: &Value) -> Result<HwpRule, String> {
    let o = v.as_object().ok_or("rule must be an object")?;
    let force = match o.get("mode").and_then(Value::as_str) {
        Some("force") => true, Some("switch") | None => false, Some(m) => return Err(format!("unknown mode '{m}'")),
    };
    let hint = |k: &str| -> Result<String, String> {
        let h = o.get(k).and_then(Value::as_str).ok_or(format!("{k} missing"))?;
        if valid_hint(h) { Ok(h.into()) } else { Err(format!("{k} '{h}' is not a valid EPP name")) }
    };
    let algo = if let Some(l) = o.get("load").filter(|v| !v.is_null()) {
        let t = l.get("threshold").and_then(Value::as_f64).ok_or("load.threshold missing")?;
        if !(0.0..=1.0).contains(&t) { return Err("load.threshold must be 0..1".into()); }
        HwpAlgo::Load { multi: l.get("multi").and_then(Value::as_bool).unwrap_or(false), threshold: t }
    } else if let Some(p) = o.get("power").and_then(Value::as_array) {
        if p.is_empty() || p.len() > 8 { return Err("power needs 1..8 terms".into()); }
        let mut terms = Vec::new();
        for t in p {
            let d = t.get("domain").and_then(Value::as_str).ok_or("power term needs a domain")?;
            if d.is_empty() || d.len() > 32 || !d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Err(format!("bad domain '{d}'"));
            }
            let w = t.get("watts").and_then(Value::as_f64).ok_or("power term needs watts")?;
            if !(0.0..=1000.0).contains(&w) { return Err("watts must be 0..1000".into()); }
            terms.push(PowerTerm { domain: d.into(), greater: t.get("gt").and_then(Value::as_bool).unwrap_or(true), watts: w,
                                   and: t.get("and").and_then(Value::as_bool).unwrap_or(false) });
        }
        HwpAlgo::Power(terms)
    } else { return Err("rule needs 'load' or 'power'".into()) };
    Ok(HwpRule { force, algo, load_hint: hint("load_hint")?, normal_hint: hint("normal_hint")? })
}

/// ThrottleStop.ini → mV per plane (undervolt.py --throttlestop):
/// [ThrottleStop] FIVRVoltage{plane}{profile}=hex, same encoding as MSR 0x150.
pub fn parse_throttlestop(ini: &str, profile: u32) -> Result<Vec<(&'static str, f64)>, String> {
    let mut in_sec = false;
    let mut out = Vec::new();
    for line in ini.lines() {
        let l = line.trim();
        if l.starts_with('[') { in_sec = l.eq_ignore_ascii_case("[ThrottleStop]"); continue; }
        if !in_sec { continue; }
        let Some((k, v)) = l.split_once('=') else { continue };
        for &(key, idx, _) in PLANES {
            if k.trim().eq_ignore_ascii_case(&format!("FIVRVoltage{idx}{profile}")) {
                let raw = u64::from_str_radix(v.trim().trim_start_matches("0x").trim_start_matches("0X"), 16)
                    .map_err(|_| format!("{}: not hex", k.trim()))?;
                if raw != 0 { out.push((key, (decode_mv(raw) * 100.0).round() / 100.0)); }
            }
        }
    }
    if out.is_empty() { return Err(format!("no non-zero FIVRVoltage entries for profile {profile}")); }
    Ok(out)
}

// ── operations ─────────────────────────────────────────────────────────────

pub fn read_status() -> Value {
    let id = cpu_id();
    let mut out = json!({
        "ok": true,
        "cpu": id.as_ref().map(|c| json!({"vendor": c.vendor, "name": c.name, "family": c.family,
            "model": c.model, "stepping": c.stepping, "codename": codename(c.model)})),
        "lockdown": lockdown(),
    });
    if id.as_ref().map(|c| c.vendor.as_str()) != Some("GenuineIntel") {
        out["ok"] = json!(false);
        out["error"] = json!("not an Intel CPU");
        return out;
    }
    out["allow_writes"] = json!(ensure_allow_writes());
    let msr = match Msr::open(true) {
        Ok(m) => m,
        Err(e) => { out["ok"] = json!(false); out["error"] = json!(format!("/dev/cpu/0/msr: {}", err_str(&e))); return out; }
    };

    let mut planes = Map::new();
    for &(k, idx, label) in PLANES {
        planes.insert(k.into(), match msr.mailbox(uv_read_cmd(idx)) {
            Ok(r) => json!({"label": label, "mv": (decode_mv(r) * 100.0).round() / 100.0,
                            "raw": format!("{r:#018x}"), "status": (r >> 32) & 0xFF}),
            Err(e) => json!({"label": label, "error": err_str(&e)}),
        });
    }
    out["voltage"] = Value::Object(planes);

    let mut icc = Map::new();
    for &k in ICC_PLANES {
        icc.insert(k.into(), match msr.mailbox(icc_read_cmd(plane_index(k).unwrap())) {
            // Status byte (bits 39:32) must be 0, otherwise the low bits are not a reading.
            Ok(r) if (r >> 32) & 0xFF != 0 => json!({"error": format!("mailbox status {:#04x} (IccMax not readable on this CPU)", (r >> 32) & 0xFF),
                                                     "raw": format!("{r:#018x}")}),
            Ok(r) => json!({"amps": (r & ICC_MAX_FIELD) as f64 / 4.0, "unlimited": (r >> 31) & 1 == 1, "raw": format!("{r:#018x}")}),
            Err(e) => json!({"error": err_str(&e)}),
        });
    }
    out["iccmax"] = Value::Object(icc);

    let prog = msr.read(MSR_PLATFORM_INFO).ok().map(|v| (v >> 30) & 1 == 1);
    out["temp"] = match msr.read(MSR_TEMPERATURE_TARGET) {
        Ok(v) => {
            let tjmax = (v >> 16) & 0xFF;
            let off = (v >> 24) & 0x3F;
            json!({"tjmax": tjmax, "offset": off, "target": tjmax.saturating_sub(off), "programmable": prog})
        }
        Err(e) => json!({"error": err_str(&e), "programmable": prog}),
    };

    out["bdprochot"] = match msr.read(MSR_POWER_CTL) {
        Ok(v) => json!({"enabled": v & 1 == 1}),
        Err(e) => json!({"error": err_str(&e)}),
    };
    out["ctdp"] = match msr.read(MSR_PLATFORM_INFO) {
        Ok(pi) => {
            let prog = (pi >> 29) & 1 == 1;
            let levels = (pi >> 33) & 3;
            let cur = msr.read(MSR_CONFIG_TDP_CONTROL).ok();
            json!({"programmable": prog, "levels": levels, "current": cur.map(|c| c & 3), "locked": cur.map(|c| (c >> 31) & 1 == 1)})
        }
        Err(e) => json!({"error": err_str(&e)}),
    };
    out["power"] = match (msr.read(MSR_RAPL_POWER_UNIT), msr.read(MSR_PKG_POWER_LIMIT)) {
        (Ok(u), Ok(raw)) => {
            let units = decode_units(u);
            let (a, b, locked) = decode_pl(raw, units);
            let t = |t: Term| json!({"watts": t.watts, "seconds": t.seconds, "enabled": t.enabled, "clamp": t.clamp});
            let mut pw = json!({"pl1": t(a), "pl2": t(b), "locked": locked, "raw": format!("{raw:#018x}")});
            match mchbar_base() {
                Ok(base) => match Mmio::open(base + MCHBAR_PKG_LIMIT_OFFSET) {
                    Ok(m) => {
                        let mv = m.read64();
                        let (ma, mb, ml) = decode_pl(mv, units);
                        pw["mchbar"] = json!({"base": format!("{base:#x}"), "raw": format!("{mv:#018x}"),
                            "pl1": t(ma), "pl2": t(mb), "locked": ml, "matches_msr": mv == raw});
                    }
                    Err(e) => pw["mchbar"] = json!({"error": format!("/dev/mem: {e} (CONFIG_DEVMEM, STRICT_DEVMEM, lockdown)")}),
                },
                Err(e) => pw["mchbar"] = json!({"error": e}),
            }
            pw
        }
        (Err(e), _) | (_, Err(e)) => json!({"error": err_str(&e)}),
    };
    out
}

fn step(results: &mut Vec<Value>, what: String, r: Result<String, String>) -> bool {
    let ok = r.is_ok();
    results.push(match r { Ok(m) => json!({"what": what, "ok": true, "message": m}),
                           Err(m) => json!({"what": what, "ok": false, "message": m}) });
    ok
}

/// Applies a pre-validated profile: voltage, IccMax, TCC offset, then power.
pub fn apply(p: &Profile) -> Value {
    let mut results = Vec::new();
    let mut all = true;
    if cpu_id().map(|c| c.vendor) != Some("GenuineIntel".into()) {
        return json!({"ok": false, "error": "not an Intel CPU"});
    }
    ensure_allow_writes();
    let msr = match Msr::open(true) {
        Ok(m) => m,
        Err(e) => return json!({"ok": false, "error": format!("/dev/cpu/0/msr: {}", err_str(&e))}),
    };

    for &(k, idx, mv) in &p.voltage {
        let r = (|| {
            let ticks = mv_to_ticks_ex(mv, p.allow_positive)?;
            let cmd = uv_write_cmd(idx, ticks);
            msr.write(MSR_OC_MAILBOX, cmd).map_err(|e| err_str(&e))?;
            let back = msr.mailbox(uv_read_cmd(idx)).map_err(|e| err_str(&e))?;
            if back & 0xFFFF_FFFF != cmd & 0xFFFF_FFFF {
                return Err(format!("readback {:.2} mV ≠ requested {:.2} mV (raw {back:#018x}) — voltage control \
                    locked (Plundervolt/CVE-2019-11157 BIOS lock, or no OC mailbox on this CPU)",
                    decode_mv(back), decode_mv(cmd)));
            }
            Ok(format!("{:.2} mV", decode_mv(back)))
        })();
        all &= step(&mut results, format!("voltage.{k}"), r);
    }

    for &(k, idx, a) in &p.iccmax {
        let r = (|| {
            let f = amps_to_icc_field(a)?;
            msr.write(MSR_OC_MAILBOX, icc_write_cmd(idx, f)).map_err(|e| err_str(&e))?;
            let resp = msr.mailbox(icc_read_cmd(idx)).map_err(|e| err_str(&e))?;
            if (resp >> 32) & 0xFF != 0 { return Err(format!("mailbox status {:#04x} after write", (resp >> 32) & 0xFF)); }
            let back = resp & ICC_MAX_FIELD;
            if back != f { return Err(format!("readback {:.2} A ≠ requested {:.2} A", back as f64 / 4.0, f as f64 / 4.0)); }
            Ok(format!("{:.2} A", back as f64 / 4.0))
        })();
        all &= step(&mut results, format!("iccmax.{k}"), r);
    }

    if let Some(off) = p.tjoffset {
        let r = (|| {
            // throttled checks PLATFORM_INFO bit 30 first; undervolt.py writes
            // (100-T)<<24 blind (assumes TjMax 100, clobbers the register);
            // intel-undervolt does a read-modify-write. RMW + the bit-30 check.
            let pi = msr.read(MSR_PLATFORM_INFO).map_err(|e| err_str(&e))?;
            if (pi >> 30) & 1 == 0 { return Err("temperature target is not programmable on this CPU".into()); }
            let cur = msr.read(MSR_TEMPERATURE_TARGET).map_err(|e| err_str(&e))?;
            let tjmax = (cur >> 16) & 0xFF;
            // throttled keeps the trip point ≥ 40 °C.
            if tjmax > 0 && tjmax.saturating_sub(off) < 40 { return Err(format!("offset {off} would put the target below 40 °C (TjMax {tjmax})")); }
            let new = (cur & !(0x3Fu64 << 24)) | (off << 24);
            msr.write(MSR_TEMPERATURE_TARGET, new).map_err(|e| err_str(&e))?;
            let back = (msr.read(MSR_TEMPERATURE_TARGET).map_err(|e| err_str(&e))? >> 24) & 0x3F;
            if back != off { return Err(format!("readback offset {back} ≠ {off}")); }
            Ok(format!("offset {off} °C → target {} °C", tjmax.saturating_sub(off)))
        })();
        all &= step(&mut results, "tjoffset".into(), r);
    }

    if p.pl1.is_some() || p.pl2.is_some() {
        let r = (|| {
            let units = decode_units(msr.read(MSR_RAPL_POWER_UNIT).map_err(|e| err_str(&e))?);
            let cur = msr.read(MSR_PKG_POWER_LIMIT).map_err(|e| err_str(&e))?;
            if cur >> 63 == 1 { return Err("MSR_PKG_POWER_LIMIT is locked by firmware until reset".into()); }
            let mut v = cur;
            if let Some(t) = &p.pl1 { v = encode_pl_term(v, false, t.watts, t.seconds, units)?; }
            if let Some(t) = &p.pl2 { v = encode_pl_term(v, true, t.watts, t.seconds, units)?; }
            let mmio_val = v;  // the MMIO copy is never locked by us
            if p.lock_power { v |= 1u64 << 63; }
            msr.write(MSR_PKG_POWER_LIMIT, v).map_err(|e| err_str(&e))?;
            let back = msr.read(MSR_PKG_POWER_LIMIT).map_err(|e| err_str(&e))?;
            if back != v { return Err(format!("MSR readback {back:#x} ≠ {v:#x}")); }
            let (a, b, _) = decode_pl(back, units);
            let mut msg = format!("MSR: PL1 {:.1} W / {:.3} s, PL2 {:.1} W / {:.4} s", a.watts, a.seconds, b.watts, b.seconds);
            if p.mchbar {
                // Both originals mirror the same value to MCHBAR+0x59A0: the
                // effective limit is the lower of the MSR and MMIO copies.
                match mchbar_base().and_then(|base| Mmio::open(base + MCHBAR_PKG_LIMIT_OFFSET).map_err(|e| format!("/dev/mem: {e}"))) {
                    Ok(m) => {
                        let old = m.read64();
                        if old >> 63 == 1 { msg += "; MCHBAR copy is locked, left alone"; }
                        else {
                            m.write64(mmio_val);
                            let mb = m.read64();
                            msg += if mb == mmio_val { "; MCHBAR mirrored" } else { "; MCHBAR write did not stick" };
                        }
                    }
                    Err(e) => msg += &format!("; MCHBAR skipped ({e})"),
                }
            }
            if p.lock_power { msg += "; MSR LOCKED until reset"; }
            Ok(msg)
        })();
        all &= step(&mut results, "power".into(), r);
    }

    if let Some(dis) = p.disable_bdprochot {
        let r = (|| {
            let cur = msr.read(MSR_POWER_CTL).map_err(|e| err_str(&e))?;
            let new = if dis { cur & !1 } else { cur | 1 };
            msr.write(MSR_POWER_CTL, new).map_err(|e| err_str(&e))?;
            let back = msr.read(MSR_POWER_CTL).map_err(|e| err_str(&e))? & 1;
            if back != new & 1 { return Err("readback differs (firmware keeps BD PROCHOT)".into()); }
            Ok(if dis { "BD PROCHOT disabled".into() } else { "BD PROCHOT enabled".into() })
        })();
        all &= step(&mut results, "bdprochot".into(), r);
    }

    if let Some(level) = p.ctdp {
        let r = (|| {
            // throttled: needs PLATFORM_INFO bit 29 and enough extra levels
            // (bits 34:33). It writes the level over the whole MSR; bit 31 is
            // the lock, so read-modify-write and refuse when locked.
            let pi = msr.read(MSR_PLATFORM_INFO).map_err(|e| err_str(&e))?;
            if (pi >> 29) & 1 == 0 { return Err("cTDP is not programmable on this CPU".into()); }
            if (pi >> 33) & 3 < level { return Err(format!("cTDP level {level} not offered (only {} extra)", (pi >> 33) & 3)); }
            let cur = msr.read(MSR_CONFIG_TDP_CONTROL).map_err(|e| err_str(&e))?;
            if (cur >> 31) & 1 == 1 { return Err("MSR_CONFIG_TDP_CONTROL is locked".into()); }
            msr.write(MSR_CONFIG_TDP_CONTROL, (cur & !3) | level).map_err(|e| err_str(&e))?;
            let back = msr.read(MSR_CONFIG_TDP_CONTROL).map_err(|e| err_str(&e))? & 3;
            if back != level { return Err(format!("readback level {back} ≠ {level}")); }
            Ok(format!("cTDP level {level} ({})", ["nominal", "down", "up"][level as usize]))
        })();
        all &= step(&mut results, "ctdp".into(), r);
    }

    json!({"ok": all, "results": results})
}

/// The part of a profile the daemon re-applies every interval (intel-undervolt
/// daemon defaults: undervolt once, power + tjoffset every tick; throttled
/// re-writes PL/TCC/BDPROCHOT/cTDP every Update_Rate_s because firmware and
/// the EC restore their own values). Voltage/IccMax are applied on start,
/// power-source change and resume only.
pub fn periodic_part(p: &Profile) -> Profile {
    // A locked limit cannot be rewritten until reset: skip it instead of
    // failing every interval.
    let (pl1, pl2) = if p.lock_power { (None, None) } else { (p.pl1.clone(), p.pl2.clone()) };
    Profile { voltage: vec![], iccmax: vec![], lock_power: false, hwphint: vec![], pl1, pl2, ..p.clone() }
}

/// One monitoring sample (throttled --monitor): throttle reasons from
/// IA32_THERM_STATUS (bits 0/10/12/14 status, 1/11/13/15 sticky log), VCore
/// from IA32_PERF_STATUS[47:32]/8192 V, raw RAPL energy counters for the
/// caller to difference (units: 0.5^MSR_RAPL_POWER_UNIT[12:8] J).
pub fn monitor_sample(msr: &Msr, clear_logs: bool) -> Value {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let mut out = json!({"ok": true, "t": ts});
    match msr.read(IA32_THERM_STATUS) {
        Ok(v) => {
            let b = |n: u32| (v >> n) & 1 == 1;
            out["throttle"] = json!({"thermal": b(0), "power": b(10), "current": b(12), "cross_domain": b(14),
                "log": {"thermal": b(1), "power": b(11), "current": b(13), "cross_domain": b(15), "prochot": b(3)},
                "temp_below_tjmax": (v >> 16) & 0x7F});
            // Writing 0 clears only the sticky log bits (status bits are read-only).
            if clear_logs { let _ = msr.write(IA32_THERM_STATUS, 0); }
        }
        Err(e) => out["throttle"] = json!({"error": err_str(&e)}),
    }
    if let Ok(v) = msr.read(IA32_PERF_STATUS) {
        out["vcore_mv"] = json!((((v >> 32) & 0xFFFF) as f64 / 8192.0 * 1000.0).round());
    }
    if let Ok(u) = msr.read(MSR_RAPL_POWER_UNIT) {
        let eu = 0.5f64.powi(((u >> 8) & 0x1F) as i32);
        let mut e = Map::new();
        for (name, addr) in [("package", MSR_PKG_ENERGY_STATUS), ("core", MSR_PP0_ENERGY_STATUS),
                             ("graphics", MSR_PP1_ENERGY_STATUS), ("dram", MSR_DRAM_ENERGY_STATUS)] {
            if let Ok(r) = msr.read(addr) { e.insert(name.into(), json!(r & 0xFFFF_FFFF)); }
        }
        // throttled: fixed DRAM unit on some server models (Haswell/Broadwell/Skylake-SP, KNL).
        let dram_unit = cpu_id().filter(|c| c.family == 6 && [63, 79, 85, 86, 87].contains(&c.model)).map(|_| 15.3e-6);
        out["energy"] = json!({"unit_j": eu, "dram_unit_j": dram_unit.unwrap_or(eu), "raw": e, "wrap": 1u64 << 32});
    }
    out
}

/// Stock voltage: all five planes back to 0 mV. IccMax, TCC and power limits
/// have no knowable "stock" (firmware programs them), so reset leaves them.
pub fn reset_profile() -> Profile {
    Profile { voltage: PLANES.iter().map(|&(k, i, _)| (k, i, 0.0)).collect(), ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_vectors_match_all_three() {
        // undervolt.py doctest: convert_offset(-50) == 0xf9a00000
        assert_eq!(encode_offset(mv_to_ticks(-50.0).unwrap()), 0xF9A0_0000);
        // intel-undervolt: (0x800 - 50*1.024 + 0.5) << 21 & 0xffffffff
        assert_eq!((((0x800 as f64 - 50.0 * 1.024 + 0.5) as u64) << 21) & 0xFFFF_FFFF, 0xF9A0_0000);
        // undervolt.py: pack_offset(0, 0xecc00000) / pack_offset(1, 0xf0000000)
        assert_eq!(0x8000_0011_0000_0000u64 | 0xECC0_0000, 0x80000011ECC00000);
        assert_eq!(uv_write_cmd(1, mv_to_ticks(-125.0).unwrap()), 0x80000111F0000000);
        assert_eq!(uv_read_cmd(0), 0x8000001000000000);
        assert_eq!(uv_read_cmd(1), 0x8000011000000000);
        assert_eq!(encode_offset(0), 0);
    }

    #[test]
    fn decode_vectors() {
        assert_eq!(decode_mv(0x0), 0.0);
        assert_eq!(decode_mv(0x40000000000), 0.0);
        assert_eq!(decode_mv(0x100f3400000), -99.609375); // undervolt.py doctest
        assert_eq!(decode_mv(0xf0000000), -125.0);
        // status byte set: undervolt.py would mis-decode this, throttled/us not
        assert_eq!(decode_mv(0x0000_00FF_F9A0_0000), decode_mv(0xF9A0_0000));
        for mv in (-1000..=0).map(|x| x as f64) {
            let t = mv_to_ticks(mv).unwrap();
            assert_eq!(mv_to_ticks(decode_mv(encode_offset(t))).unwrap(), t);
        }
        assert!(mv_to_ticks(1.0).is_err() && mv_to_ticks(-1000.5).is_err());
    }

    #[test]
    fn icc_vectors_match_throttled() {
        assert_eq!(amps_to_icc_field(0.25).unwrap(), 1);
        assert_eq!(amps_to_icc_field(255.75).unwrap(), 0x3FF);
        // 11-bit field: the 255HX's stock 263 A (raw 0x41C) must round-trip.
        assert_eq!(amps_to_icc_field(263.0).unwrap(), 0x41C);
        assert_eq!((0x0000_0000_0000_041Cu64 & ICC_MAX_FIELD) as f64 / 4.0, 263.0);
        assert_eq!((0x41Cu64 & 0x3FF) as f64 / 4.0, 7.0); // what 10-bit decoding showed
        assert_eq!(amps_to_icc_field(511.75).unwrap(), 0x7FF);
        assert_eq!(amps_to_icc_field(100.0).unwrap(), 400);
        assert_eq!(amps_to_icc_field(105.4).unwrap(), 421);
        assert_eq!(amps_to_icc_field(200.9).unwrap(), 803);
        assert!(amps_to_icc_field(0.0).is_err() && amps_to_icc_field(512.0).is_err());
        assert_eq!(icc_write_cmd(0, 400) & 0x7FF, 400);
        assert_eq!(icc_write_cmd(0, 0x41C) & 0x7FF, 0x41C);
    }

    #[test]
    fn power_limit_roundtrip() {
        let u = decode_units(0x000A_0E03); // 1/8 W, 1/1024 s (common value)
        assert_eq!(u.power_w, 0.125);
        assert_eq!(u.time_s, 1.0 / 1024.0);
        let raw = 0x0042_8168_00DD_8168u64; // clamp bits + junk to be preserved
        let v = encode_pl_term(raw, false, 45.0, Some(28.0), u).unwrap();
        let v = encode_pl_term(v, true, 90.0, Some(0.002), u).unwrap();
        let (a, b, l) = decode_pl(v, u);
        assert_eq!((a.watts, b.watts, a.enabled, b.enabled, l), (45.0, 90.0, true, true, false));
        assert!((a.seconds - 28.0).abs() / 28.0 < 0.13);
        assert!((b.seconds - 0.002).abs() < 0.001);
        assert_eq!(v & (1 << 16), raw & (1 << 16)); // clamp preserved
        assert_eq!(v & (1 << 48), raw & (1 << 48));
        assert_eq!(v >> 56, raw >> 56);
        // Saturation stays inside the 7-bit field (undervolt.py's 0xFE bug)
        assert!(seconds_to_tw(1e12, u.time_s).unwrap() <= 0x7F);
        assert!(encode_pl_term(0, false, 5000.0, None, u).is_err());
    }

    #[test]
    fn tw_nearest_like_undervolt_py() {
        let ts = 1.0 / 1024.0;
        for s in [0.002, 0.01, 1.0, 8.0, 28.0, 56.0, 128.0] {
            let got = tw_to_seconds(seconds_to_tw(s, ts).unwrap(), ts);
            assert!((got - s).abs() / s < 0.13, "{s} -> {got}");
        }
    }

    #[test]
    fn extras_parse() {
        assert!(parse_profile(&json!({"voltage": {"core": 20}, "allow_positive": true})).is_ok());
        assert!(parse_profile(&json!({"voltage": {"core": 300}, "allow_positive": true})).is_err());
        assert!(parse_profile(&json!({"ctdp": 3})).is_err());
        assert!(parse_profile(&json!({"lock_power": true})).is_err());
        let p = parse_profile(&json!({"disable_bdprochot": true, "ctdp": 1, "hwphint": [
            {"mode": "switch", "load": {"multi": false, "threshold": 0.8}, "load_hint": "performance", "normal_hint": "balance_performance"},
            {"mode": "force", "power": [{"domain": "core", "gt": true, "watts": 8}], "load_hint": "performance", "normal_hint": "power"}]})).unwrap();
        assert_eq!((p.disable_bdprochot, p.ctdp, p.hwphint.len()), (Some(true), Some(1), 2));
        assert!(parse_profile(&json!({"hwphint": [{"load": {"threshold": 0.5}, "load_hint": "perf; rm", "normal_hint": "power"}]})).is_err());
    }

    #[test]
    fn throttlestop_import() {
        let ini = "[ThrottleStop]\nFIVRVoltage00=0xF9A00000\nFIVRVoltage20=F9A00000\nFIVRVoltage10=0\nFIVRVoltage01=0xF0000000\n";
        let v = parse_throttlestop(ini, 0).unwrap();
        assert_eq!(v, vec![("core", -49.8), ("cache", -49.8)]);
        assert_eq!(parse_throttlestop(ini, 1).unwrap(), vec![("core", -125.0)]);
        assert!(parse_throttlestop(ini, 3).is_err());
    }

    #[test]
    fn profile_validation() {
        assert!(parse_profile(&json!({"voltage": {"core": 5}})).is_err());
        assert!(parse_profile(&json!({"voltage": {"digitalio": -5}})).is_err());
        assert!(parse_profile(&json!({"tjoffset": 64})).is_err());
        assert!(parse_profile(&json!({"power": {"pl1": {"seconds": 28}}})).is_err());
        let p = parse_profile(&json!({"voltage": {"core": -80, "cache": -80, "gpu": null},
            "tjoffset": -10, "power": {"pl1": {"watts": 55, "seconds": 28}, "mchbar": false}})).unwrap();
        assert_eq!(p.voltage.len(), 2);
        assert_eq!(p.tjoffset, Some(10));
        assert!(!p.mchbar && p.pl1.is_some() && p.pl2.is_none());
    }
}
