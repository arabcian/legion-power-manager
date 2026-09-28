//! Centralised write validation (port of safety.py).

use crate::hal::limits::{get_mem_offset_range, get_power_limit};
use crate::nvapi::{CT_POINTS, MAX_DELTA_KHZ, MIN_DELTA_KHZ};
use std::collections::BTreeMap;

/// Upper plausibility bound for a memory locked clock (MHz).
pub const MAX_MEM_CLOCK_MHZ: i64 = 100_000;

fn mhz(khz: i64) -> f64 { khz as f64 / 1000.0 }

/// Validate a curve write. Empty = safe.
///
/// Positive deltas (overclock) are limited by `max_delta_khz`, itself clamped
/// to the driver's +1000 MHz cap. Negative deltas only lower clocks and are
/// limited by the driver floor alone (MIN_DELTA_KHZ, -2000 MHz), so a lowered
/// `--max-delta` never blocks an undervolt / flatten curve.
pub fn validate_write(deltas: &BTreeMap<i64, i64>, max_delta_khz: i64) -> Vec<String> {
    let up = max_delta_khz.min(MAX_DELTA_KHZ);
    let mut errs = Vec::new();
    for (&p, &d) in deltas {
        if p < 0 || p >= CT_POINTS as i64 {
            errs.push(format!("Point {p} out of range (0–{})", CT_POINTS - 1));
            continue;
        }
        if i32::try_from(d).is_err() {
            errs.push(format!("Delta {d} kHz for point {p} does not fit the driver's signed 32-bit \
                               freqDelta field (must be within {}..{} kHz).", i32::MIN, i32::MAX));
            continue;
        }
        if d < MIN_DELTA_KHZ {
            errs.push(format!("Delta {:+.0} MHz for point {p} is below the driver floor of {:.0} MHz.",
                              mhz(d), mhz(MIN_DELTA_KHZ)));
        } else if d > up {
            if max_delta_khz > MAX_DELTA_KHZ {
                errs.push(format!("Delta {:+.0} MHz for point {p} exceeds the driver's hard cap of \
                                   +{:.0} MHz — this cannot be raised with --max-delta.", mhz(d), mhz(MAX_DELTA_KHZ)));
            } else {
                errs.push(format!("Delta {:+.0} MHz for point {p} exceeds safety limit of +{:.0} MHz. \
                                   Use --max-delta to raise the limit if needed.", mhz(d), mhz(max_delta_khz)));
            }
        }
    }
    errs
}

/// Warn when a delta would push a point's effective frequency below 0.
pub fn check_negative_freq_warnings(deltas: &BTreeMap<i64, i64>, vfp_freqs_khz: &[i64],
                                    current_offsets_khz: Option<&[i64]>) -> Vec<String> {
    deltas.iter().filter_map(|(&p, &d)| {
        let idx = usize::try_from(p).ok()?;
        let base = *vfp_freqs_khz.get(idx)?;
        if base == 0 { return None; }
        let cur = current_offsets_khz.and_then(|c| c.get(idx).copied()).unwrap_or(0);
        let eff = base + (d - cur);
        (eff < 0).then(|| format!("Point {p}: delta {:+.0} MHz would produce effective frequency \
                                    {:.0} MHz — driver will clamp to 0.", mhz(d), mhz(eff)))
    }).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitField { PowerLimit, MemOffset, MemLocked, GpuCap }

#[derive(Debug, Clone)]
pub struct LimitError { pub field: LimitField, pub message: String }

#[derive(Debug, Clone, Default)]
pub struct Limits {
    pub power_limit_w: Option<i64>,
    pub mem_offset_mhz: Option<i64>,
    pub mem_locked_min_mhz: Option<i64>,
    pub mem_locked_max_mhz: Option<i64>,
    pub gpu_clock_cap_mhz: Option<i64>,
}

/// Validate secondary settings against driver-reported ranges. A range the
/// driver can't report is "can't validate", not "invalid".
/// Unlike the Python version this reports the offending field structurally,
/// so callers no longer substring-match error text to decide what to drop.
pub fn validate_limits(gpu_index: u32, l: &Limits) -> Vec<LimitError> {
    let mut errs = Vec::new();
    let mut push = |field, message| errs.push(LimitError { field, message });

    if let Some(w) = l.power_limit_w {
        let info = get_power_limit(gpu_index);
        if let (Some(lo), Some(hi)) = (info.min_power_limit_w, info.max_power_limit_w) {
            if !(lo as i64..=hi as i64).contains(&w) {
                push(LimitField::PowerLimit, format!(
                    "power_limit_w {w} is outside the driver's reported range ({lo}-{hi} W)"));
            }
        }
    }
    if let Some(m) = l.mem_offset_mhz {
        let r = get_mem_offset_range(gpu_index);
        if !(r.min_mem_offset_mhz as i64..=r.max_mem_offset_mhz as i64).contains(&m) {
            push(LimitField::MemOffset, format!(
                "mem_offset_mhz {m} is outside the driver's reported range ({}-{} MHz)",
                r.min_mem_offset_mhz, r.max_mem_offset_mhz));
        }
    }
    for (label, v) in [("mem_locked_min_mhz", l.mem_locked_min_mhz), ("mem_locked_max_mhz", l.mem_locked_max_mhz)] {
        if let Some(v) = v {
            if !(1..=MAX_MEM_CLOCK_MHZ).contains(&v) {
                push(LimitField::MemLocked, format!(
                    "{label} {v} is outside the plausible range (1-{MAX_MEM_CLOCK_MHZ} MHz)"));
            }
        }
    }
    if let Some(c) = l.gpu_clock_cap_mhz {
        if !(210..=4000).contains(&c) {
            push(LimitField::GpuCap, format!("gpu_clock_cap_mhz {c} is outside the plausible range (210-4000 MHz)"));
        }
    }
    if let (Some(lo), Some(hi)) = (l.mem_locked_min_mhz, l.mem_locked_max_mhz) {
        if lo > hi {
            push(LimitField::MemLocked, format!(
                "mem_locked_min_mhz ({lo}) is greater than mem_locked_max_mhz ({hi})"));
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn write_limits() {
        let mut d = BTreeMap::new();
        d.insert(0, 200_000);
        assert!(validate_write(&d, 300_000).is_empty());
        assert_eq!(validate_write(&d, 100_000).len(), 1);
        d.insert(0, 1_500_000);
        assert!(validate_write(&d, 5_000_000)[0].contains("hard cap"));
        d.clear();
        d.insert(-1, 0);
        d.insert(CT_POINTS as i64, 0);
        assert_eq!(validate_write(&d, 1).len(), 2);
    }
    #[test]
    fn negative_floor() {
        let mut d = BTreeMap::new();
        d.insert(10, -1_500_000);                        // flatten-style: fine
        assert!(validate_write(&d, 1_000_000).is_empty());
        assert!(validate_write(&d, 100_000).is_empty()); // a low overclock cap never blocks undervolt
        d.insert(10, -2_000_000);
        assert!(validate_write(&d, 1_000_000).is_empty());
        d.insert(10, -2_000_001);
        assert!(validate_write(&d, 1_000_000)[0].contains("floor"));
    }
    #[test]
    fn negative_freq() {
        let mut d = BTreeMap::new();
        d.insert(0, -600_000);
        let w = check_negative_freq_warnings(&d, &[500_000], None);
        assert_eq!(w.len(), 1);
    }
}
