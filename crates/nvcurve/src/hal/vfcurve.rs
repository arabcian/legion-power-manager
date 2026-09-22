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

// ── Boost mask (static per GPU; cached) ─────────────────────────────────────

static MASK_CACHE: Mutex<Option<HashMap<Gpu, [u8; 32]>>> = Mutex::new(None);

pub fn get_boost_mask(gpu: Gpu) -> Result<[u8; 32], String> {
    if let Some(m) = MASK_CACHE.lock().unwrap_or_else(|p| p.into_inner())
        .as_ref().and_then(|c| c.get(&gpu)) {
        return Ok(*m);
    }
    let d = nvcall(fid::GET_CLOCK_BOOST_MASK, gpu, MASK_SIZE, 1, |b| {
        b.bytes_mut()[4..36].fill(0xFF);
    })?;
    let mut mask = [0u8; 32];
    mask.copy_from_slice(&d.bytes()[4..36]);
    MASK_CACHE.lock().unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new).insert(gpu, mask);
    Ok(mask)
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

fn merge(vfp: &[(u32, u32)], ct: &[(i32, u32)], gpu_name: &str) -> CurveState {
    let mut points = Vec::new();
    let mut in_memory = false;
    for (i, &(freq, volt)) in vfp.iter().enumerate() {
        if freq == 0 && volt == 0 { break; }
        let (delta, flags) = ct.get(i).copied().unwrap_or((0, 0));
        if flags == 1 { in_memory = true; }
        points.push(VfPoint {
            index: i, freq_khz: freq, volt_uv: volt, delta_khz: delta,
            domain: if in_memory { Domain::Memory } else { Domain::Gpu },
        });
    }
    CurveState { points, timestamp: now_secs(), gpu_name: gpu_name.to_owned() }
}

pub fn read_curve(gpu: Gpu, gpu_name: &str) -> Result<CurveState, String> {
    read_curve_with_raw_ct(gpu, gpu_name).map(|(s, _)| s)
}

/// read_curve() plus the raw ClockBoostTable it fetched, so writers can
/// reuse it as the write baseline (one driver round-trip instead of two).
pub fn read_curve_with_raw_ct(gpu: Gpu, gpu_name: &str) -> Result<(CurveState, Buf), String> {
    let vfp = read_vfp_curve(gpu)?;
    let raw = read_clock_table_raw(gpu)?;
    let state = merge(&vfp, &parse_ct_entries(raw.bytes()), gpu_name);
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

/// Returns (driver return code, description). -998 = rejected, -999 = setup failure.
pub fn write_offsets(gpu: Gpu, deltas: &PointDeltas, dry_run: bool, full_mask: bool,
                     current_raw: Option<&[u8]>) -> (i32, String) {
    let mut buf = match build_write_buffer(gpu, deltas, full_mask, current_raw) {
        Ok(b) => b,
        Err(e) if e.starts_with("Rejected") => return (-998, e),
        Err(e) => return (-999, e),
    };
    if dry_run {
        return (0, "DRY RUN — buffer built but not sent to driver".into());
    }
    nvcall_raw(fid::SET_CLOCK_BOOST_TABLE, gpu, &mut buf)
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
        let s = merge(&vfp, &ct, "x");
        assert_eq!(s.points.len(), 3);
        assert_eq!(s.points[1].domain, Domain::Gpu);
        assert_eq!(s.points[2].domain, Domain::Memory);
        assert_eq!(s.points[1].delta_khz, 5);
    }
}
