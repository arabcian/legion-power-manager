//! Lenovo Legion Spectrum keyboard (ITE 8258, USB 048D:C1xx on Gen10) over Linux hidraw.
//!
//! Copyright (C) 2026 arabcian — SPDX-License-Identifier: GPL-3.0-or-later
//!
//! Protocol learned from Lenovo Legion Toolkit (GPL-3.0,
//! github.com/LenovoLegionToolkit-Team/LenovoLegionToolkit); this is an
//! independent implementation, no LLT code is included. Verified on a
//! Legion Pro 7 16AFR10H (048d:c197):
//! every transfer is a 960-byte feature report, id 7, header `07 <op> C0 03`.
//! Per-key colour works through firmware "Always" effects (one per colour group, op CB).
//! Host-driven Aurora (D0/A1) is NOT used: Gen10 firmware ignores LLT's A1 frames
//! (tested on 16AFR10H: the per-key state report stays black whatever is sent).
//!
//! Profile writes (CB) land in the controller's non-volatile store and survive
//! reboots and OS switches — never drive animations with them.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub const VENDOR_ID: u16 = 0x048D;
pub const PRODUCT_MASK: u16 = 0xFF00;
pub const PRODUCT_MATCH: u16 = 0xC100;
pub const REPORT_ID: u8 = 7;
pub const REPORT_LEN: usize = 960;
/// Marker keycode meaning "all lights" (used by AuroraSync/audio effects).
pub const ALL_KEYS: u16 = 0x65;
pub const MAX_BRIGHTNESS: u8 = 9;
pub const MAX_PROFILE: u8 = 6;

mod op {
    pub const COMPAT: u8 = 0xD1;
    pub const KEY_COUNT: u8 = 0xC4;
    pub const KEY_PAGE: u8 = 0xC5;
    pub const PROFILE_SET: u8 = 0xC8;
    pub const PROFILE_DEFAULT: u8 = 0xC9;
    pub const PROFILE_GET: u8 = 0xCA;
    pub const EFFECT_SET: u8 = 0xCB;
    pub const EFFECT_GET: u8 = 0xCC;
    pub const BRIGHT_GET: u8 = 0xCD;
    pub const BRIGHT_SET: u8 = 0xCE;
    pub const LOGO_GET: u8 = 0xA5;
    pub const LOGO_SET: u8 = 0xA6;
}

pub type Report = [u8; REPORT_LEN];

// ---------------------------------------------------------------- errors

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    NotFound,
    Range(&'static str),
    TooLarge { needed: usize },
    BadReport(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O: {e}"),
            Error::NotFound => write!(f, "Spectrum keyboard (048D:C1xx) not found"),
            Error::Range(w) => write!(f, "value out of range: {w}"),
            Error::TooLarge { needed } => {
                write!(f, "effect set needs {needed} bytes, report holds {REPORT_LEN}")
            }
            Error::BadReport(w) => write!(f, "malformed report: {w}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn parse_hex(s: &str) -> Option<Rgb> {
        let s = s.trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let v = u32::from_str_radix(s, 16).ok()?;
        Some(Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectType {
    ScrewRainbow = 1,
    RainbowWave = 2,
    ColorChange = 3,
    ColorPulse = 4,
    ColorWave = 5,
    Smooth = 6,
    Rain = 7,
    Ripple = 8,
    AudioBounce = 9,
    AudioRipple = 10,
    Always = 11,
    TypeLighting = 12,
    AuroraSync = 13,
}

impl EffectType {
    pub const ALL: [EffectType; 13] = [
        Self::ScrewRainbow, Self::RainbowWave, Self::ColorChange, Self::ColorPulse,
        Self::ColorWave, Self::Smooth, Self::Rain, Self::Ripple, Self::AudioBounce,
        Self::AudioRipple, Self::Always, Self::TypeLighting, Self::AuroraSync,
    ];

    pub fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| *e as u8 == v)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ScrewRainbow => "Screw Rainbow",
            Self::RainbowWave => "Rainbow Wave",
            Self::ColorChange => "Color Change",
            Self::ColorPulse => "Color Pulse",
            Self::ColorWave => "Color Wave",
            Self::Smooth => "Smooth",
            Self::Rain => "Rain",
            Self::Ripple => "Ripple",
            Self::AudioBounce => "Audio Bounce",
            Self::AudioRipple => "Audio Ripple",
            Self::Always => "Static",
            Self::TypeLighting => "Type Lighting",
            Self::AuroraSync => "Aurora Sync",
        }
    }

    /// Needs host-side audio/screen processing; not usable on Linux yet.
    pub fn needs_host(self) -> bool {
        matches!(self, Self::AudioBounce | Self::AudioRipple | Self::AuroraSync)
    }
    pub fn has_speed(self) -> bool {
        !matches!(self, Self::Always) && !self.needs_host()
    }
    /// Linear direction (up/down/left/right).
    pub fn has_direction(self) -> bool {
        matches!(self, Self::RainbowWave | Self::ColorWave)
    }
    pub fn has_clockwise(self) -> bool {
        matches!(self, Self::ScrewRainbow)
    }
    pub fn has_colors(self) -> bool {
        matches!(
            self,
            Self::Always | Self::ColorChange | Self::ColorPulse | Self::ColorWave
                | Self::Rain | Self::Ripple | Self::TypeLighting | Self::Smooth
        )
    }
}

/// Raw effect as stored by the firmware. Unknown type ids are kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub kind: u8,
    pub speed: u8,     // 0 none, 1..=3
    pub clockwise: u8, // 0 none, 1 cw, 2 ccw
    pub direction: u8, // 0 none, 1 up, 2 down, 3 right->left, 4 left->right
    pub color_mode: u8, // 0 none, 1 random, 2 list
    pub colors: Vec<Rgb>,
    pub keys: Vec<u16>,
}

impl Effect {
    pub fn solid(color: Rgb, keys: Vec<u16>) -> Self {
        Effect {
            kind: EffectType::Always as u8,
            speed: 0,
            clockwise: 0,
            direction: 0,
            color_mode: 2,
            colors: vec![color],
            keys,
        }
    }

    pub fn kind(&self) -> Option<EffectType> {
        EffectType::from_u8(self.kind)
    }

    fn encoded_len(&self) -> usize {
        1 + 13 + 1 + self.colors.len() * 3 + 1 + self.keys.len() * 2
    }
}

/// Bytes an effect list occupies in a CB report (incl. 4-byte header + 3-byte preamble).
pub fn encoded_len(effects: &[Effect]) -> usize {
    4 + 3 + effects.iter().map(Effect::encoded_len).sum::<usize>()
}

pub fn encode_effects(profile: u8, effects: &[Effect]) -> Result<Report> {
    let needed = encoded_len(effects);
    if needed > REPORT_LEN {
        return Err(Error::TooLarge { needed });
    }
    if effects.len() > 255 {
        return Err(Error::Range("effect count"));
    }
    let mut b = [0u8; REPORT_LEN];
    b[..4].copy_from_slice(&[REPORT_ID, op::EFFECT_SET, 0xC0, 0x03]);
    let mut p = 4;
    let mut put = |v: u8, p: &mut usize| {
        b[*p] = v;
        *p += 1;
    };
    for v in [profile, 1, 1] {
        put(v, &mut p);
    }
    for (i, e) in effects.iter().enumerate() {
        if e.colors.len() > 255 || e.keys.len() > 255 {
            return Err(Error::Range("colors/keys per effect"));
        }
        let hdr = [
            (i + 1) as u8, 0x06, 0x01, e.kind, 0x02, e.speed, 0x03, e.clockwise,
            0x04, e.direction, 0x05, e.color_mode, 0x06, 0x00,
        ];
        for v in hdr {
            put(v, &mut p);
        }
        put(e.colors.len() as u8, &mut p);
        for c in &e.colors {
            for v in [c.0, c.1, c.2] {
                put(v, &mut p);
            }
        }
        put(e.keys.len() as u8, &mut p);
        for k in &e.keys {
            for v in k.to_le_bytes() {
                put(v, &mut p);
            }
        }
    }
    Ok(b)
}

/// Decode a CC (effect get) response. Returns (profile, effects).
pub fn decode_effects(b: &[u8]) -> Result<(u8, Vec<Effect>)> {
    if b.len() < 7 || b[0] != REPORT_ID {
        return Err(Error::BadReport("effect header"));
    }
    let profile = b[4];
    let mut p = 7usize;
    let mut last = 1u8;
    let mut out = Vec::new();
    let take = |p: &mut usize, n: usize| -> Result<&[u8]> {
        let s = b.get(*p..*p + n).ok_or(Error::BadReport("truncated effect"))?;
        *p += n;
        Ok(s)
    };
    while p < b.len() {
        let no = b[p];
        if no < last {
            break;
        }
        last = no;
        p += 1;
        let h = take(&mut p, 13)?;
        let (kind, speed, clockwise, direction, color_mode) = (h[2], h[4], h[6], h[8], h[10]);
        let nc = take(&mut p, 1)?[0] as usize;
        let colors = take(&mut p, nc * 3)?
            .chunks_exact(3)
            .map(|c| Rgb(c[0], c[1], c[2]))
            .collect();
        let nk = take(&mut p, 1)?[0] as usize;
        let keys = take(&mut p, nk * 2)?
            .chunks_exact(2)
            .map(|k| u16::from_le_bytes([k[0], k[1]]))
            .collect();
        out.push(Effect { kind, speed, clockwise, direction, color_mode, colors, keys });
    }
    Ok((profile, out))
}

/// Group a per-key colour map into "Always" effects (one per distinct colour),
/// ordered by first appearance in `order`. Keys missing from `colors` stay dark.
pub fn per_key_effects(order: &[u16], colors: &HashMap<u16, Rgb>) -> Vec<Effect> {
    let mut groups: Vec<(Rgb, Vec<u16>)> = Vec::new();
    for kc in order {
        let Some(c) = colors.get(kc) else { continue };
        match groups.iter_mut().find(|(g, _)| g == c) {
            Some((_, v)) => v.push(*kc),
            None => groups.push((*c, vec![*kc])),
        }
    }
    groups.into_iter().map(|(c, k)| Effect::solid(c, k)).collect()
}

// ---------------------------------------------------------------- key map

/// Physical layout reported by the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMap {
    pub rows: usize,
    pub cols: usize,
    /// Row-major, 0 = empty cell. A wide key repeats its code in adjacent cells.
    pub grid: Vec<u16>,
    /// Secondary page (logo etc.), 0 = empty.
    pub extra: Vec<u16>,
}

/// Chassis accent LEDs on Gen10: 18 rear-exhaust (grid row 0) + 10 front/side (grid edges).
pub const PERIMETER_REAR: std::ops::RangeInclusive<u16> = 0x03E9..=0x03FA;
pub const PERIMETER_FRONT: std::ops::RangeInclusive<u16> = 0x01F5..=0x01FE;

pub fn is_perimeter(kc: u16) -> bool {
    PERIMETER_REAR.contains(&kc) || PERIMETER_FRONT.contains(&kc)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Keyboard,
    Perimeter,
    Logo,
}

impl KeyMap {
    pub fn at(&self, row: usize, col: usize) -> u16 {
        self.grid[row * self.cols + col]
    }

    /// Every addressable code once, row-major, extras last.
    pub fn unique(&self) -> Vec<u16> {
        let mut seen = std::collections::HashSet::new();
        self.grid
            .iter()
            .chain(self.extra.iter())
            .copied()
            .filter(|&k| k != 0 && seen.insert(k))
            .collect()
    }

    pub fn zone_of(&self, kc: u16) -> Zone {
        if is_perimeter(kc) {
            Zone::Perimeter
        } else if self.extra.contains(&kc) {
            Zone::Logo
        } else {
            Zone::Keyboard
        }
    }

    pub fn zone(&self, z: Zone) -> Vec<u16> {
        self.unique().into_iter().filter(|&k| self.zone_of(k) == z).collect()
    }

    /// Cells covered by each key: (row, first_col, width). For drawing.
    pub fn spans(&self) -> Vec<(u16, usize, usize, usize)> {
        let mut out = Vec::new();
        for r in 0..self.rows {
            let mut c = 0;
            while c < self.cols {
                let k = self.at(r, c);
                let start = c;
                while c < self.cols && self.at(r, c) == k {
                    c += 1;
                }
                if k != 0 {
                    out.push((k, r, start, c - start));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------- device

const fn hid_ioc(nr: u64, len: usize) -> u64 {
    // _IOC(_IOC_READ|_IOC_WRITE, 'H', nr, len)
    (3u64 << 30) | ((len as u64) << 16) | ((b'H' as u64) << 8) | nr
}
const HIDIOCSFEATURE: u64 = hid_ioc(0x06, REPORT_LEN);
const HIDIOCGFEATURE: u64 = hid_ioc(0x07, REPORT_LEN);

fn request(opcode: u8, payload: &[u8]) -> Report {
    let mut b = [0u8; REPORT_LEN];
    b[..4].copy_from_slice(&[REPORT_ID, opcode, 0xC0, 0x03]);
    b[4..4 + payload.len()].copy_from_slice(payload);
    b
}

pub struct Device {
    file: File,
    path: PathBuf,
}

impl Device {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        Ok(Device { file, path })
    }

    /// Scan /sys/class/hidraw for 048D:C1xx and pick the interface that answers D1.
    pub fn find() -> Result<Self> {
        let mut last_err = None;
        let mut entries: Vec<_> = fs::read_dir("/sys/class/hidraw")
            .map_err(|_| Error::NotFound)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();
        for dir in entries {
            let Ok(uevent) = fs::read_to_string(dir.join("device/uevent")) else { continue };
            let Some(id) = uevent.lines().find_map(|l| l.strip_prefix("HID_ID=")) else { continue };
            let mut it = id.split(':').skip(1).map(|s| u32::from_str_radix(s, 16).ok());
            let (Some(Some(vid)), Some(Some(pid))) = (it.next(), it.next()) else { continue };
            if vid as u16 != VENDOR_ID || (pid as u16 & PRODUCT_MASK) != PRODUCT_MATCH {
                continue;
            }
            let node = Path::new("/dev").join(dir.file_name().unwrap());
            match Device::open(&node).and_then(|d| d.is_compatible().map(|ok| (d, ok))) {
                Ok((d, true)) => return Ok(d),
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or(Error::NotFound))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_feature(&self, buf: &Report) -> Result<()> {
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), HIDIOCSFEATURE as _, buf.as_ptr()) };
        if r < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub fn get_feature(&self) -> Result<Report> {
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = REPORT_ID;
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), HIDIOCGFEATURE as _, buf.as_mut_ptr()) };
        if r < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(buf)
    }

    fn xfer(&self, opcode: u8, payload: &[u8]) -> Result<Report> {
        self.set_feature(&request(opcode, payload))?;
        self.get_feature()
    }

    pub fn is_compatible(&self) -> Result<bool> {
        Ok(self.xfer(op::COMPAT, &[])?[4] == 0)
    }

    pub fn brightness(&self) -> Result<u8> {
        Ok(self.xfer(op::BRIGHT_GET, &[])?[4])
    }

    pub fn set_brightness(&self, v: u8) -> Result<()> {
        if v > MAX_BRIGHTNESS {
            return Err(Error::Range("brightness 0-9"));
        }
        self.set_feature(&request(op::BRIGHT_SET, &[v]))
    }

    pub fn profile(&self) -> Result<u8> {
        Ok(self.xfer(op::PROFILE_GET, &[])?[4])
    }

    pub fn set_profile(&self, p: u8) -> Result<()> {
        if p > MAX_PROFILE {
            return Err(Error::Range("profile 0-6"));
        }
        self.set_feature(&request(op::PROFILE_SET, &[p]))?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        Ok(())
    }

    /// Factory-reset one profile's effect list.
    pub fn reset_profile(&self, p: u8) -> Result<()> {
        if p > MAX_PROFILE {
            return Err(Error::Range("profile 0-6"));
        }
        self.set_feature(&request(op::PROFILE_DEFAULT, &[p]))
    }

    pub fn logo(&self) -> Result<bool> {
        Ok(self.xfer(op::LOGO_GET, &[])?[4] == 1)
    }

    pub fn set_logo(&self, on: bool) -> Result<()> {
        self.set_feature(&request(op::LOGO_SET, &[on as u8]))
    }

    pub fn keymap(&self) -> Result<KeyMap> {
        let r = self.xfer(op::KEY_COUNT, &[7])?;
        let (rows, cols) = (r[5] as usize, r[6] as usize);
        if rows == 0 || cols == 0 || 6 + cols * 3 > REPORT_LEN {
            return Err(Error::BadReport("key count"));
        }
        // Items are packed (Pack=1): u8 index, u16le keycode; array starts at byte 6.
        let page = |param: u8, idx: u8| -> Result<Vec<u16>> {
            let r = self.xfer(op::KEY_PAGE, &[param, idx])?;
            Ok((0..cols)
                .map(|x| u16::from_le_bytes([r[6 + x * 3 + 1], r[6 + x * 3 + 2]]))
                .collect())
        };
        let mut grid = Vec::with_capacity(rows * cols);
        for y in 0..rows {
            grid.extend(page(7, y as u8)?);
        }
        let extra = page(8, 0)?;
        Ok(KeyMap { rows, cols, grid, extra })
    }

    /// Raw CC response for a profile (use for backup).
    pub fn read_effects_raw(&self, profile: u8) -> Result<Report> {
        self.xfer(op::EFFECT_GET, &[profile])
    }

    pub fn read_effects(&self, profile: u8) -> Result<Vec<Effect>> {
        Ok(decode_effects(&self.read_effects_raw(profile)?)?.1)
    }

    pub fn write_effects(&self, profile: u8, effects: &[Effect]) -> Result<()> {
        if profile > MAX_PROFILE {
            return Err(Error::Range("profile 0-6"));
        }
        self.set_feature(&encode_effects(profile, effects)?)
    }

    /// Write back a CC dump produced by `read_effects_raw`.
    pub fn restore_effects_raw(&self, dump: &[u8]) -> Result<()> {
        if dump.len() != REPORT_LEN || dump[0] != REPORT_ID || dump[1] != op::EFFECT_GET {
            return Err(Error::BadReport("not a CC dump"));
        }
        let mut b = [0u8; REPORT_LEN];
        b.copy_from_slice(dump);
        b[1] = op::EFFECT_SET;
        self.set_feature(&b)
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Grid as reported by a Legion Pro 7 16AFR10H.
    fn pro7_gen10() -> KeyMap {
        const G: [[u16; 22]; 9] = [
            [0,0,0x3e9,0x3f3,0x3ea,0x3f4,0x3eb,0x3f5,0x3ec,0x3f6,0x3ed,0x3ee,0x3ef,0x3f7,0x3f0,0x3f8,0x3f1,0x3f9,0x3f2,0x3fa,0,0],
            [0,1,2,3,4,5,6,7,8,9,0xa,0xb,0xc,0xd,0xe,0xf,0x10,0x11,0x12,0x13,0x14,0],
            [0,0x16,0x17,0x18,0x19,0x1a,0x1b,0x1c,0,0x1d,0x1e,0x1f,0x20,0x21,0x22,0x38,0x38,0x26,0x27,0x28,0x29,0],
            [0,0x40,0x42,0,0x43,0x44,0x45,0x46,0x47,0x48,0x49,0x4a,0x4b,0,0x4c,0x4d,0x77,0x4f,0x50,0x51,0x68,0],
            [0,0x55,0x55,0x6d,0x6e,0x58,0x59,0x5a,0x71,0,0x72,0x5b,0x5c,0x5d,0x5f,0xa8,0x77,0x79,0x7b,0x7c,0x68,0],
            [0x1f5,0x6a,0x4e,0x82,0x83,0,0x6f,0x70,0x87,0x88,0x73,0x74,0x75,0,0x76,0x8d,0x8d,0x8e,0x90,0x92,0xa7,0x1fe],
            [0x1f5,0x7f,0x80,0x96,0x97,0x98,0x98,0x98,0x98,0x98,0x98,0x9a,0x9b,0,0,0x9d,0,0xa3,0xa3,0xa5,0xa7,0x1fe],
            [0x1f6,0,0,0,0,0,0,0,0,0,0,0,0,0,0x9c,0x9f,0,0xa1,0,0,0,0x1fd],
            [0x1f6,0x1f7,0x1f7,0x1f7,0x1f7,0x1f8,0x1f8,0x1f8,0x1f9,0x1f9,0x1f9,0x1fa,0x1fa,0x1fa,0x1fb,0x1fb,0x1fb,0x1fc,0x1fc,0x1fc,0x1fc,0x1fd],
        ];
        let mut extra = vec![0u16; 22];
        extra[0] = 0x5dd;
        KeyMap { rows: 9, cols: 22, grid: G.iter().flatten().copied().collect(), extra }
    }

    #[test]
    fn keymap_counts() {
        let m = pro7_gen10();
        assert_eq!(m.unique().len(), 131);
        assert_eq!(m.zone(Zone::Perimeter).len(), 28);
        assert_eq!(m.zone(Zone::Keyboard).len(), 102);
        assert_eq!(m.zone(Zone::Logo), vec![0x5dd]);
        let space = m.spans().into_iter().find(|s| s.0 == 0x98).unwrap();
        assert_eq!((space.1, space.2, space.3), (6, 5, 6));
    }

    #[test]
    fn roundtrip() {
        let fx = vec![
            Effect::solid(Rgb(255, 0, 64), vec![1, 2, 0x3e9]),
            Effect { kind: 2, speed: 2, clockwise: 0, direction: 3, color_mode: 0, colors: vec![], keys: vec![0x1f5] },
        ];
        let mut b = encode_effects(6, &fx).unwrap();
        b[1] = op::EFFECT_GET;
        assert_eq!(decode_effects(&b).unwrap(), (6, fx));
    }

    #[test]
    fn per_key_capacity() {
        let m = pro7_gen10();
        let order = m.unique();
        // 36 distinct colours over all 131 codes must fit, 40 must not.
        let mk = |n: usize| -> HashMap<u16, Rgb> {
            order.iter().enumerate().map(|(i, &k)| (k, Rgb((i % n) as u8, 0, 0))).collect()
        };
        assert!(encode_effects(1, &per_key_effects(&order, &mk(36))).is_ok());
        assert!(matches!(encode_effects(1, &per_key_effects(&order, &mk(40))), Err(Error::TooLarge { .. })));
    }

    #[test]
    fn ioctl_numbers() {
        assert_eq!(HIDIOCSFEATURE, 0xC3C0_4806);
        assert_eq!(HIDIOCGFEATURE, 0xC3C0_4807);
    }
}
