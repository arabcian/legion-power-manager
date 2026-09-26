//! Temperatures NVML does not give on GeForce: GPU hotspot and VRAM.
//!
//! Sources (all read-only):
//!   * NvAPI thermal channels (ThermChannelGetStatus v2): 40 signed values in
//!     1/256 °C behind a channel mask. Channel 9 is the hotspot up to Ada;
//!     channel 15 is the memory on GDDR6/GDDR6X boards, channel 10 on GDDR7.
//!   * Blackwell's hotspot is not in those channels any more: it is read from
//!     the aggregated hotspot register (0x00AD0AA0, 1/256 °C in the low 16 bits).
//!   * GDDR7 (and GDDR6X) memory also reports per-partition sensors through
//!     registers 0x009024C0/0x009024D0 + 0x4000·partition; the hottest one is
//!     what matters for a memory overclock.
//! The memory type is inferred from the architecture (Blackwell GeForce =
//! GDDR7); every value is range-checked, so a wrong guess shows "—", never a
//! bogus number.

use crate::nvapi::{fid, nvcall, Buf, Gpu};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

const THERM_SIZE: usize = 8 + 40 * 4;
const THERM_VER: u32 = 2;
const HOTSPOT_CHANNEL: usize = 9;
const VRAM_CHANNEL_GDDR6: usize = 15;
const VRAM_CHANNEL_GDDR7: usize = 10;

const REG_OP_SIZE: usize = 8 + 256 * 24;
const REG_FLAG_READ: u16 = 1;
const REG_FLAG_32BIT: u16 = 4;
const REG_FLAG_GLOBAL: u16 = 16;
const REG_BLACKWELL_HOTSPOT: u32 = 0x00AD_0AA0;
const REG_GDDR_TEMP_DATA: u32 = 0x0090_24C0;
const REG_GDDR_TEMP_STATUS: u32 = 0x0090_24D0;
const REG_GDDR_CLAMSHELL: u32 = 0x0090_0200;
const POISON_MASK: u64 = 0xFFFF_0000;
const POISON: u64 = 0xBADF_0000;

pub const ARCH_BLACKWELL: u32 = 10;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Sensors {
    pub hotspot_c: Option<i32>,
    /// Hottest memory sensor (per-partition when available, else the channel).
    pub vram_c: Option<i32>,
    /// ("A0", 64), ("B1 (Back)", 62) … on boards with per-partition sensors.
    pub vram_partitions: Vec<(String, i32)>,
    /// Every channel the driver reported, °C, for diagnostics (`nvcurve sensors --raw`).
    pub channels: Vec<(usize, i32)>,
    pub source: String,
}

static MASKS: Mutex<Option<HashMap<crate::nvapi::Gpu, i32>>> = Mutex::new(None);

fn therm_call(gpu: Gpu, mask: i32) -> Result<Buf, String> {
    nvcall(fid::THERM_CHANNEL_GET_STATUS, gpu, THERM_SIZE, THERM_VER, |b| b.put_i32(4, mask))
}

/// The driver rejects a mask naming a channel it doesn't have, so probe bit by
/// bit once and keep the widest accepted mask (the same set every later call).
fn channel_mask(gpu: Gpu) -> Result<i32, String> {
    if let Some(&m) = MASKS.lock().unwrap_or_else(|p| p.into_inner()).as_ref().and_then(|c| c.get(&gpu)) {
        return Ok(m);
    }
    therm_call(gpu, 1)?;
    let mut mask: i32 = 1;
    for bit in 1..31 {
        let candidate = mask | (1 << bit);
        if therm_call(gpu, candidate).is_err() { break; }
        mask = candidate;
    }
    MASKS.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert_with(HashMap::new).insert(gpu, mask);
    Ok(mask)
}

fn celsius(raw: i32) -> Option<i32> {
    let c = raw / 256;
    (c > 0 && c < 255).then_some(c)
}

pub fn read_register(gpu: Gpu, offset: u32) -> Result<u64, String> {
    let b = nvcall(fid::REGISTER_OP, gpu, REG_OP_SIZE, 1, |b| {
        b.put_u32(4, 1); // op_count
        let flags = REG_FLAG_READ | REG_FLAG_32BIT | REG_FLAG_GLOBAL;
        b.bytes_mut()[8..10].copy_from_slice(&flags.to_le_bytes());
        b.put_u32(12, offset);
    })?;
    let status = u16::from_le_bytes([b.bytes()[10], b.bytes()[11]]);
    if status != 0 { return Err(format!("register op status {status}")); }
    Ok(u64::from_le_bytes(b.bytes()[24..32].try_into().unwrap()))
}

fn register_temp(gpu: Gpu, offset: u32) -> Option<i32> {
    let raw = read_register(gpu, offset).ok()?;
    let c = ((raw & 0xFFFF) / 256) as i32;
    (c > 0 && c < 255).then_some(c)
}

/// Per-partition memory sensors: up to 9 partitions, two slot pairs each;
/// a status bit says which slot holds data; clamshell boards carry a second
/// (back-side) reading in the upper byte.
fn vram_partitions(gpu: Gpu) -> Vec<(String, i32)> {
    let parse = |v: u64| (2 * v.min(0x50) as i32) - 40;
    let clamshell = read_register(gpu, REG_GDDR_CLAMSHELL).map_or(false, |v| (v >> 22) & 1 == 1);
    let mut out = Vec::new();
    for part in 0..=8u32 {
        let Ok(status) = read_register(gpu, REG_GDDR_TEMP_STATUS + part * 0x4000) else { continue };
        if status & POISON_MASK == POISON { continue; }
        let letter = char::from(b'A' + part as u8);
        let mut i = 0;
        for pair in [[(0x0u32, 24u32), (0x8, 26)], [(0x4, 25), (0xC, 27)]] {
            for (slot, bit) in pair {
                if (status >> bit) & 1 == 0 { continue; }
                let Ok(data) = read_register(gpu, REG_GDDR_TEMP_DATA + part * 0x4000 + slot) else { continue };
                if data == 0 || data == u64::from(u32::MAX) || data & POISON_MASK == POISON { continue; }
                let (pc0, pc1) = ((data >> 16) & 0xFF, (data >> 24) & 0xFF);
                if pc0 == 0 || pc0 == 0xFF { continue; }
                let label = format!("{letter}{i}");
                if clamshell {
                    out.push((format!("{label} (Front)"), parse(pc0)));
                    if pc1 != 0 && pc1 != 0xFF { out.push((format!("{label} (Back)"), parse(pc1))); }
                } else {
                    out.push((label, parse(pc0)));
                }
                i += 1;
                break;
            }
        }
    }
    out.retain(|&(_, c)| (1..150).contains(&c));
    out
}

/// `arch`: NVML architecture id (None = unknown → pre-Blackwell rules).
pub fn read(gpu: Gpu, arch: Option<u32>) -> Sensors {
    let mut s = Sensors::default();
    let blackwell = arch.map_or(false, |a| a >= ARCH_BLACKWELL);
    let mut src = Vec::new();

    let values: Option<[i32; 40]> = channel_mask(gpu).ok().and_then(|m| therm_call(gpu, m).ok()).map(|b| {
        let mut v = [0i32; 40];
        for (i, x) in v.iter_mut().enumerate() { *x = b.i32_at(8 + i * 4); }
        v
    });
    if let Some(v) = &values {
        s.channels = v.iter().enumerate().filter_map(|(i, &r)| celsius(r).map(|c| (i, c))).collect();
    }
    let chan = |i: usize| values.as_ref().and_then(|v| celsius(v[i]));

    if blackwell {
        s.hotspot_c = register_temp(gpu, REG_BLACKWELL_HOTSPOT);
        if s.hotspot_c.is_some() { src.push("hotspot: register"); }
        s.vram_partitions = vram_partitions(gpu);
        s.vram_c = s.vram_partitions.iter().map(|p| p.1).max();
        if s.vram_c.is_some() { src.push("vram: partitions"); }
        else if let Some(c) = chan(VRAM_CHANNEL_GDDR7) { s.vram_c = Some(c); src.push("vram: channel 10"); }
    } else {
        s.hotspot_c = chan(HOTSPOT_CHANNEL);
        if s.hotspot_c.is_some() { src.push("hotspot: channel 9"); }
        s.vram_c = chan(VRAM_CHANNEL_GDDR6);
        if s.vram_c.is_some() { src.push("vram: channel 15"); }
    }
    s.source = src.join(", ");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn celsius_filters() {
        assert_eq!(celsius(65 * 256 + 128), Some(65));
        assert_eq!(celsius(0), None);
        assert_eq!(celsius(-256), None);
        assert_eq!(celsius(255 * 256), None);
    }
    #[test]
    fn struct_sizes() {
        assert_eq!(THERM_SIZE, 168);
        assert_eq!(REG_OP_SIZE, 6152);
    }
}
