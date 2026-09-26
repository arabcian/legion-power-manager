//! Read and write the V/F curve via NvAPI (port of hal/vfcurve.py).
//!
//! Point indices and deltas are bounds-checked at the sink (build_write_buffer)
//! — not only in safety::validate_write — so no caller can ever produce an
//! out-of-entry write into the SetClockBoostTable struct.

use crate::nvapi::*;
use crate::types::{now_secs, CurveState, Domain, VfPoint};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

pub type PointDeltas = BTreeMap<i64, i64>;

fn check_point(point: i64) -> Result<usize, String> {
    if (0..CT_POINTS as i64).contains(&point) { Ok(point as usize) }
    else { Err(format!("point {point} out of range (0-{})", CT_POINTS - 1)) }
}

fn check_delta(delta: i64, point: i64) -> Result<i32, String> {
    i32::try_from(delta).map_err(|_| format!(
        "delta {delta} kHz for point {point} does not fit the driver's signed 32-bit freqDelta field"))
}

// ── Boost mask + per-point info (static per GPU; cached) ────────────────────
//
// GetClockBoostMask is NvAPI's ClockClientClkVfPointsGetInfo: after the
// 32-byte mask it carries one 0x18-byte record per point — `type` (0 = a
// programmable frequency point) and `bVoltageBased`. That is the driver's own
// answer to "is this a core V/F point or a memory point", and nvcurve used to
// throw it away and guess from the ClockBoostTable flags instead.

pub const INFO_BASE: usize = 0x44;
pub const INFO_STRIDE: usize = 0x18;
pub const POINT_TYPE_PROG: u32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointInfo { pub kind: u32, pub voltage_based: bool }

#[derive(Clone)]
struct BoostInfo { mask: [u8; 32], points: Vec<PointInfo> }

static MASK_CACHE: Mutex<Option<HashMap<Gpu, BoostInfo>>> = Mutex::new(None);

pub fn parse_point_info(d: &[u8]) -> Vec<PointInfo> {
    if d.len() < INFO_BASE { return Vec::new(); }
    (0..(d.len() - INFO_BASE) / INFO_STRIDE).map(|i| {
        let off = INFO_BASE + i * INFO_STRIDE;
        PointInfo { kind: read_u32(d, off), voltage_based: d[off + 4] != 0 }
    }).collect()
}

fn boost_info(gpu: Gpu) -> Result<BoostInfo, String> {
    if let Some(m) = MASK_CACHE.lock().unwrap_or_else(|p| p.into_inner())
        .as_ref().and_then(|c| c.get(&gpu)) {
        return Ok(m.clone());
    }
    let d = nvcall(fid::GET_CLOCK_BOOST_MASK, gpu, MASK_SIZE, 1, |b| {
        b.bytes_mut()[4..36].fill(0xFF);
    })?;
    let mut mask = [0u8; 32];
    mask.copy_from_slice(&d.bytes()[4..36]);
    let info = BoostInfo { mask, points: parse_point_info(d.bytes()) };
    MASK_CACHE.lock().unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new).insert(gpu, info.clone());
    Ok(info)
}

pub fn get_boost_mask(gpu: Gpu) -> Result<[u8; 32], String> { boost_info(gpu).map(|b| b.mask) }
pub fn get_point_info(gpu: Gpu) -> Result<Vec<PointInfo>, String> { boost_info(gpu).map(|b| b.points) }

fn mask_has(mask: &[u8; 32], point: usize) -> bool { point < 256 && mask[point / 8] & (1 << (point % 8)) != 0 }

// ── V3 status: offset-free base frequency ───────────────────────────────────
//
// ClockClientClkVfPointsGetStatus v3 (same entry point as GetVFPCurve, larger
// struct): header 0x68 bytes (mask, bVfTupleBaseSupported at 0x24), then 255
// records of 348 bytes: type, freq, volt, base tuple {freq, volt, 32 rsvd},
// offset tuple, 256 rsvd. Only the base frequency is taken from it.

const V3_HEADER: usize = 0x68;
const V3_STRIDE: usize = 348;
const V3_SIZE: usize = V3_HEADER + 255 * V3_STRIDE;
const V3_BASE_SUPPORTED: usize = 0x24;
const V3_BASE_FREQ: usize = 12;

pub fn read_base_freqs(gpu: Gpu) -> Option<Vec<u32>> {
    let mask = get_boost_mask(gpu).ok()?;
    let d = nvcall(fid::GET_VFP_CURVE, gpu, V3_SIZE, 3, |b| b.bytes_mut()[4..36].copy_from_slice(&mask)).ok()?;
    parse_v3_base(d.bytes())
}

pub fn parse_v3_base(d: &[u8]) -> Option<Vec<u32>> {
    if d.len() < V3_SIZE || d[V3_BASE_SUPPORTED] == 0 { return None; }
    Some((0..255).map(|i| read_u32(d, V3_HEADER + i * V3_STRIDE + V3_BASE_FREQ)).collect())
}

fn fill_mask(mask: &[u8; 32]) -> impl FnOnce(&mut Buf) + '_ {
    move |b| b.bytes_mut()[4..36].copy_from_slice(mask)
}

fn set_mask_bit(bytes: &mut [u8], point: usize) {
    bytes[4 + point / 8] |= 1 << (point % 8);
}

// ── Readers ─────────────────────────────────────────────────────────────────

/// Base V/F curve: [(freq_kHz, volt_uV)].
pub fn read_vfp_curve(gpu: Gpu) -> Result<Vec<(u32, u32)>, String> {
    let mask = get_boost_mask(gpu).map_err(|e| format!("GetClockBoostMask failed: {e}"))?;
    let d = nvcall(fid::GET_VFP_CURVE, gpu, VFP_SIZE, 1, fill_mask(&mask))?;
    let n = (d.len() - VFP_BASE) / VFP_STRIDE;
    Ok((0..n).map(|i| {
        let off = VFP_BASE + i * VFP_STRIDE;
        (d.u32_at(off), d.u32_at(off + 4))
    }).collect())
}

pub fn read_clock_table_raw(gpu: Gpu) -> Result<Buf, String> {
    let mask = get_boost_mask(gpu).map_err(|e| format!("GetClockBoostMask failed: {e}"))?;
    nvcall(fid::GET_CLOCK_BOOST_TABLE, gpu, CT_SIZE, 1, fill_mask(&mask))
}

/// (delta_kHz, flags) per entry.
pub fn parse_ct_entries(d: &[u8]) -> Vec<(i32, u32)> {
    if d.len() < CT_BASE { return Vec::new(); }
    (0..(d.len() - CT_BASE) / CT_STRIDE).map(|i| {
        let base = CT_BASE + i * CT_STRIDE;
        (read_i32(d, base + CT_DELTA_OFF), read_u32(d, base))
    }).collect()
}

pub fn read_clock_offsets(gpu: Gpu) -> Result<Vec<i32>, String> {
    let d = read_clock_table_raw(gpu)?;
    Ok(parse_ct_entries(d.bytes()).into_iter().map(|(delta, _)| delta).collect())
}

/// All nine raw u32 fields of one entry (freqDelta signed), for diagnostics.
pub fn read_clock_entry_full(data: &[u8], point: i64) -> Result<Vec<(String, i64)>, String> {
    let p = check_point(point)?;
    let base = CT_BASE + p * CT_STRIDE;
    if base + 36 > data.len() {
        return Err(format!("ClockBoostTable buffer too short for point {point} ({} bytes)", data.len()));
    }
    let mut out: Vec<(String, i64)> = (0..9).map(|j| {
        let off = base + j * 4;
        let v = if j == 5 { read_i32(data, off) as i64 } else { read_u32(data, off) as i64 };
        (format!("field_{j:02}_0x{:02X}", j * 4), v)
    }).collect();
    let delta = out[5].1;
    out.push(("freqDelta_kHz".into(), delta));
    Ok(out)
}

/// Domain per point from the driver's point info, if it is usable: at least
/// one programmable voltage-based point, and every such point before every
/// other one (core curve first, memory after — the layout of every known
/// GPU). Anything else falls back to the flags heuristic, so a driver that
/// fills the records differently can't silently reclassify points.
fn domains_from_info(n: usize, info: &[PointInfo]) -> Option<Vec<Domain>> {
    if info.len() < n { return None; }
    let d: Vec<Domain> = info[..n].iter()
        .map(|p| if p.kind == POINT_TYPE_PROG && p.voltage_based { Domain::Gpu } else { Domain::Memory })
        .collect();
    let first_mem = d.iter().position(|x| *x == Domain::Memory).unwrap_or(n);
    let core_ok = first_mem > 0 && d[first_mem..].iter().all(|x| *x == Domain::Memory);
    core_ok.then_some(d)
}

fn merge(vfp: &[(u32, u32)], ct: &[(i32, u32)], info: Option<&[PointInfo]>, base: Option<&[u32]>,
         gpu_name: &str) -> CurveState {
    let n = vfp.iter().position(|&(f, v)| f == 0 && v == 0).unwrap_or(vfp.len());
    let from_info = info.and_then(|i| domains_from_info(n, i));
    let mut points = Vec::with_capacity(n);
    let mut in_memory = false;
    for (i, &(freq, volt)) in vfp[..n].iter().enumerate() {
        let (delta, flags) = ct.get(i).copied().unwrap_or((0, 0));
        if flags == 1 { in_memory = true; }
        let domain = match &from_info {
            Some(d) => d[i],
            None => if in_memory { Domain::Memory } else { Domain::Gpu },
        };
        let base_khz = base.and_then(|b| b.get(i).copied()).filter(|&b| b > 0);
        points.push(VfPoint { index: i, freq_khz: freq, base_khz, volt_uv: volt, delta_khz: delta, domain });
    }
    CurveState {
        points, timestamp: now_secs(), gpu_name: gpu_name.to_owned(),
        domain_source: if from_info.is_some() { "point-info" } else { "flags" },
    }
}

pub fn read_curve(gpu: Gpu, gpu_name: &str) -> Result<CurveState, String> {
    read_curve_with_raw_ct(gpu, gpu_name).map(|(s, _)| s)
}

/// read_curve() plus the raw ClockBoostTable it fetched, so writers can
/// reuse it as the write baseline (one driver round-trip instead of two).
pub fn read_curve_with_raw_ct(gpu: Gpu, gpu_name: &str) -> Result<(CurveState, Buf), String> {
    let vfp = read_vfp_curve(gpu)?;
    let raw = read_clock_table_raw(gpu)?;
    let info = get_point_info(gpu).ok();
    let base = read_base_freqs(gpu);
    let state = merge(&vfp, &parse_ct_entries(raw.bytes()), info.as_deref(), base.as_deref(), gpu_name);
    Ok((state, raw))
}

// ── Writers ─────────────────────────────────────────────────────────────────

/// Builds a SetClockBoostTable buffer from `current_raw`, changing only the
/// requested entries' freqDelta and (sparse mode) only their mask bits.
/// The whole request is validated before any byte is touched.
pub fn build_write_buffer(
    gpu: Gpu,
    deltas: &PointDeltas,
    full_mask: bool,
    current_raw: Option<&[u8]>,
) -> Result<Buf, String> {
    let mut checked = Vec::with_capacity(deltas.len());
    for (&p, &d) in deltas {
        checked.push((check_point(p).map_err(|e| format!("Rejected unsafe write: {e}"))?,
                      check_delta(d, p).map_err(|e| format!("Rejected unsafe write: {e}"))?));
    }

    let owned;
    let raw = match current_raw {
        Some(r) => r,
        None => {
            owned = read_clock_table_raw(gpu).map_err(|e| format!("Cannot read current ClockBoostTable: {e}"))?;
            owned.bytes()
        }
    };
    if raw.len() < CT_SIZE {
        return Err(format!("ClockBoostTable buffer is {} bytes, expected at least {CT_SIZE} — \
                            refusing to build a write buffer", raw.len()));
    }

    let mut buf = Buf::from_bytes(&raw[..CT_SIZE]);
    buf.put_u32(0, (1 << 16) | CT_SIZE as u32);

    if full_mask {
        let mask = get_boost_mask(gpu).map_err(|e| format!("Cannot read boost mask: {e}"))?;
        buf.bytes_mut()[4..36].copy_from_slice(&mask);
    } else {
        buf.bytes_mut()[4..36].fill(0);
        for &(p, _) in &checked { set_mask_bit(buf.bytes_mut(), p); }
    }
    for &(p, d) in &checked {
        buf.put_i32(CT_BASE + p * CT_STRIDE + CT_DELTA_OFF, d);
    }
    Ok(buf)
}

/// Lowest delta that keeps point `p` at ≥ 1 MHz (kHz), from the offset-free
/// base frequency. Without the V3 base tuple the base is estimated both ways
/// (reported freq, and reported freq − current delta) and the higher floor is
/// used, which is right whichever the driver reports.
pub fn floor_delta_khz(p: &VfPoint) -> i64 {
    let base = match p.base_khz {
        Some(b) => i64::from(b),
        None => (p.freq_khz as i64).min(p.freq_khz as i64 - p.delta_khz as i64),
    };
    1000 - base
}

/// Raises every delta that would drive its point to 0 MHz or below up to the
/// floor, instead of only warning (the driver's own handling of a negative
/// point frequency is undefined). Returns one note per clamped point.
pub fn clamp_to_floor(deltas: &mut PointDeltas, state: &CurveState) -> Vec<String> {
    let mut notes = Vec::new();
    for (&p, d) in deltas.iter_mut() {
        let Some(pt) = usize::try_from(p).ok().and_then(|i| state.points.get(i)) else { continue };
        if pt.freq_khz == 0 { continue; }
        let floor = floor_delta_khz(pt);
        if *d < floor {
            notes.push(format!("point {p}: {:+} MHz would reach 0 MHz — clamped to {:+} MHz", *d / 1000, floor / 1000));
            *d = floor;
        }
    }
    notes
}

/// Read-back check that tolerates the floor clamp: a negative delta that came
/// back less negative (but still ≤ 0) was raised to its point's floor on
/// purpose; anything else must match exactly.
pub fn readback_matches(expected: i64, got: i64) -> bool {
    got == expected || (expected < 0 && got > expected && got <= 0)
}

/// Returns (driver return code, description). -998 = rejected, -999 = setup failure.
/// Deltas below a point's floor are clamped first (reported in the description).
pub fn write_offsets(gpu: Gpu, deltas: &PointDeltas, dry_run: bool, full_mask: bool,
                     current_raw: Option<&[u8]>) -> (i32, String) {
    let mut deltas = deltas.clone();
    let mut notes = Vec::new();
    if deltas.values().any(|&d| d < 0) {
        match read_curve(gpu, "") {
            Ok(state) => notes = clamp_to_floor(&mut deltas, &state),
            Err(e) => log::debug!("floor clamp skipped (curve unreadable): {e}"),
        }
    }
    // Points outside the driver's own mask are not V/F points on this GPU.
    if let Ok(mask) = get_boost_mask(gpu) {
        if let Some(&p) = deltas.keys().find(|&&p| usize::try_from(p).map_or(true, |i| !mask_has(&mask, i))) {
            return (-998, format!("Rejected unsafe write: point {p} is not in this GPU's V/F point mask"));
        }
    }
    let mut buf = match build_write_buffer(gpu, &deltas, full_mask, current_raw) {
        Ok(b) => b,
        Err(e) if e.starts_with("Rejected") => return (-998, e),
        Err(e) => return (-999, e),
    };
    let suffix = if notes.is_empty() { String::new() } else { format!(" [{}]", notes.join("; ")) };
    if dry_run {
        return (0, format!("DRY RUN — buffer built but not sent to driver{suffix}"));
    }
    let (rc, d) = nvcall_raw(fid::SET_CLOCK_BOOST_TABLE, gpu, &mut buf);
    (rc, format!("{d}{suffix}"))
}

fn write_domain(gpu: Gpu, filter: Option<Domain>, delta: i64, dry_run: bool, empty_msg: Option<&str>)
    -> (i32, String)
{
    let (curve, raw) = match read_curve_with_raw_ct(gpu, "") {
        Ok(v) => v,
        Err(e) => return (-999, format!("Failed to read curve: {e}")),
    };
    let deltas: PointDeltas = curve.points.iter()
        .filter(|p| filter.map_or(true, |d| p.domain == d))
        .map(|p| (p.index as i64, delta)).collect();
    if deltas.is_empty() {
        if let Some(m) = empty_msg { return (-999, m.into()); }
    }
    write_offsets(gpu, &deltas, dry_run, false, Some(raw.bytes()))
}

/// Uniform offset on every GPU-core point.
pub fn write_global_offset(gpu: Gpu, delta_khz: i64, dry_run: bool) -> (i32, String) {
    write_domain(gpu, Some(Domain::Gpu), delta_khz, dry_run, None)
}

/// Uniform offset on every memory-domain point (discovered, never assumed).
pub fn write_memory_offset(gpu: Gpu, delta_khz: i64, dry_run: bool) -> (i32, String) {
    write_domain(gpu, Some(Domain::Memory), delta_khz, dry_run, Some("No memory-domain points found in curve"))
}

pub fn reset_memory_offsets(gpu: Gpu, dry_run: bool) -> (i32, String) {
    write_domain(gpu, Some(Domain::Memory), 0, dry_run, Some("No memory-domain points found in curve"))
}

pub fn reset_offsets(gpu: Gpu, dry_run: bool) -> (i32, String) {
    write_domain(gpu, Some(Domain::Gpu), 0, dry_run, None)
}

/// Zero every populated point on whatever GPU is present (no hardcoded range).
pub fn reset_all_offsets(gpu: Gpu, dry_run: bool) -> (i32, String) {
    write_domain(gpu, None, 0, dry_run, Some("No populated points found in curve"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<u8> {
        let mut t = vec![0u8; CT_SIZE];
        t[..4].copy_from_slice(&((1u32 << 16) | CT_SIZE as u32).to_le_bytes());
        t[4..36].fill(0xAB);
        t
    }

    #[test]
    fn sparse_write_touches_only_target() {
        let raw = table();
        let mut d = PointDeltas::new();
        d.insert(0, 150_000);
        d.insert(9, -30_000);
        let b = build_write_buffer(Gpu(0), &d, false, Some(&raw)).unwrap();
        let bytes = b.bytes();
        assert_eq!(bytes[4], 0b0000_0001);
        assert_eq!(bytes[5], 0b0000_0010);
        assert!(bytes[6..36].iter().all(|&x| x == 0));
        assert_eq!(read_i32(bytes, CT_BASE + CT_DELTA_OFF), 150_000);
        assert_eq!(read_i32(bytes, CT_BASE + 9 * CT_STRIDE + CT_DELTA_OFF), -30_000);
        assert_eq!(read_i32(bytes, CT_BASE + CT_STRIDE + CT_DELTA_OFF), 0);
        // untouched region identical
        assert_eq!(&bytes[36..CT_BASE], &raw[36..CT_BASE]);
    }

    #[test]
    fn rejects_bad_points_and_deltas() {
        let raw = table();
        for (p, dl) in [(-1i64, 0i64), (CT_POINTS as i64, 0), (0, i64::from(i32::MAX) + 1)] {
            let mut d = PointDeltas::new();
            d.insert(p, dl);
            let (rc, _) = write_offsets(Gpu(0), &d, true, false, Some(&raw));
            assert_eq!(rc, -998);
        }
    }

    #[test]
    fn short_baseline_refused() {
        let mut d = PointDeltas::new();
        d.insert(0, 0);
        assert!(build_write_buffer(Gpu(0), &d, false, Some(&[0u8; 100])).is_err());
    }

    #[test]
    fn merge_marks_memory_domain() {
        let vfp = vec![(1000, 700), (1100, 710), (5000, 0), (0, 0), (9, 9)];
        let ct = vec![(0, 0), (5, 0), (7, 1)];
        let s = merge(&vfp, &ct, None, None, "x");
        assert_eq!(s.points.len(), 3);
        assert_eq!(s.points[1].domain, Domain::Gpu);
        assert_eq!(s.points[2].domain, Domain::Memory);
        assert_eq!(s.points[1].delta_khz, 5);
        assert_eq!(s.domain_source, "flags");
    }

    fn info(kinds: &[(u32, bool)]) -> Vec<PointInfo> {
        kinds.iter().map(|&(kind, v)| PointInfo { kind, voltage_based: v }).collect()
    }

    #[test]
    fn point_info_decides_the_domain() {
        let vfp = vec![(1000, 700), (1100, 710), (8000, 0), (8000, 0)];
        // flags would call point 1 memory (flags==1); the driver's point info says core
        let ct = vec![(0, 0), (0, 1), (0, 1), (0, 1)];
        let pi = info(&[(0, true), (0, true), (1, false), (1, false)]);
        let s = merge(&vfp, &ct, Some(&pi), None, "x");
        assert_eq!(s.domain_source, "point-info");
        assert_eq!(s.points.iter().map(|p| p.domain).collect::<Vec<_>>(),
                   vec![Domain::Gpu, Domain::Gpu, Domain::Memory, Domain::Memory]);
        // interleaved core/memory records are not trusted → flags
        let bad = info(&[(0, true), (1, false), (0, true), (1, false)]);
        assert_eq!(merge(&vfp, &ct, Some(&bad), None, "x").domain_source, "flags");
        // no core point at all → flags
        let none = info(&[(1, false); 4]);
        assert_eq!(merge(&vfp, &ct, Some(&none), None, "x").domain_source, "flags");
    }

    #[test]
    fn point_info_parsing() {
        let mut d = vec![0u8; INFO_BASE + 3 * INFO_STRIDE];
        d[INFO_BASE + 4] = 1;
        d[INFO_BASE + INFO_STRIDE..INFO_BASE + INFO_STRIDE + 4].copy_from_slice(&1u32.to_le_bytes());
        let p = parse_point_info(&d);
        assert_eq!(p[0], PointInfo { kind: 0, voltage_based: true });
        assert_eq!(p[1], PointInfo { kind: 1, voltage_based: false });
        assert_eq!(MASK_SIZE, INFO_BASE + 255 * INFO_STRIDE);  // the buffer nvcurve already fetches
    }

    #[test]
    fn v3_base() {
        let mut d = vec![0u8; V3_SIZE];
        assert!(parse_v3_base(&d).is_none());          // base tuple not supported
        d[V3_BASE_SUPPORTED] = 1;
        d[V3_HEADER + V3_STRIDE + V3_BASE_FREQ..][..4].copy_from_slice(&1_500_000u32.to_le_bytes());
        assert_eq!(parse_v3_base(&d).unwrap()[1], 1_500_000);
    }

    #[test]
    fn readback_tolerates_clamp_only() {
        assert!(readback_matches(-50_000, -50_000));
        assert!(readback_matches(-270_000, -224_000));
        assert!(!readback_matches(-50_000, 0 + 1_000));
        assert!(!readback_matches(50_000, 40_000));
    }

    #[test]
    fn floor_clamp() {
        let pt = |freq: u32, delta: i32, base: Option<u32>| VfPoint {
            index: 0, freq_khz: freq, base_khz: base, volt_uv: 700_000, delta_khz: delta, domain: Domain::Gpu };
        // base known: 225 MHz → floor −224 MHz
        let st = CurveState { points: vec![pt(200_000, -25_000, Some(225_000))], timestamp: 0.0,
                              gpu_name: String::new(), domain_source: "flags" };
        let mut d = PointDeltas::new();
        d.insert(0, -270_000);
        let notes = clamp_to_floor(&mut d, &st);
        assert_eq!(d[&0], -224_000);
        assert_eq!(notes.len(), 1);
        // no base: the stricter of the two readings wins
        assert_eq!(floor_delta_khz(&pt(300_000, 50_000, None)), 1000 - 250_000);
        assert_eq!(floor_delta_khz(&pt(300_000, -50_000, None)), 1000 - 300_000);
        // a delta above the floor is left alone
        let mut ok = PointDeltas::new();
        ok.insert(0, -100_000);
        assert!(clamp_to_floor(&mut ok, &st).is_empty());
    }
}
