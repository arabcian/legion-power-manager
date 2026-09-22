//! ClockBoostTable snapshots (port of hal/snapshot.py).

use crate::atomicio::{ensure_dir, write_bytes, write_json};
use crate::hal::vfcurve::{get_boost_mask, read_clock_table_raw};
use crate::nvapi::*;
use crate::timefmt;
use crate::types::SnapshotInfo;
use log::{error, info};
use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};

fn offsets(raw: &[u8]) -> Vec<i32> {
    if raw.len() < CT_BASE { return Vec::new(); }
    (0..(raw.len() - CT_BASE) / CT_STRIDE)
        .map(|i| read_i32(raw, CT_BASE + i * CT_STRIDE + CT_DELTA_OFF)).collect()
}

/// Save the current table (or `raw` if the caller already has it).
/// Returns the .bin path.
pub fn save(gpu: Gpu, gpu_name: &str, dir: &str, max_snapshots: usize, raw: Option<&[u8]>)
    -> Option<PathBuf>
{
    let owned;
    let raw = match raw {
        Some(r) => r,
        None => match read_clock_table_raw(gpu) {
            Ok(b) => { owned = b; owned.bytes() }
            Err(e) => { error!("Failed to read ClockBoostTable: {e}"); return None; }
        },
    };
    let dir = Path::new(dir);
    if let Err(e) = ensure_dir(dir, 0o755) {
        error!("Cannot create snapshot dir {}: {e}", dir.display());
        return None;
    }
    let ts = timefmt::stamp();
    let bin = dir.join(format!("clock_boost_table_{ts}.bin"));
    let meta_path = dir.join(format!("clock_boost_table_{ts}.json"));
    if let Err(e) = write_bytes(&bin, raw, 0o644) {
        error!("Snapshot write failed: {e}");
        return None;
    }
    let offs = offsets(raw);
    let nonzero = offs.iter().filter(|&&o| o != 0).count();
    let meta = json!({
        "gpu": gpu_name, "timestamp": timefmt::iso_now(), "file": bin.to_string_lossy(),
        "size": raw.len(), "offsets_kHz": offs, "nonzero_offsets": nonzero,
    });
    if let Err(e) = write_json(&meta_path, &meta, 0o644) {
        error!("Snapshot metadata write failed: {e}");
    }
    info!("Snapshot saved: {} ({} bytes, {nonzero} non-zero offsets)", bin.display(), raw.len());
    if max_snapshots > 0 { prune(dir, max_snapshots); }
    Some(bin)
}

fn bins_sorted(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".bin")).collect();
    v.sort();
    v
}

fn prune(dir: &Path, max: usize) {
    let bins = bins_sorted(dir);
    if bins.len() <= max { return; }
    for name in &bins[..bins.len() - max] {
        let stem = &name[..name.len() - 4];
        for ext in [".bin", ".json"] { let _ = std::fs::remove_file(dir.join(format!("{stem}{ext}"))); }
    }
}

/// Resolve which snapshot file to restore; a caller-supplied path must
/// resolve (through symlinks) inside `dir`.
pub fn resolve_restore_path(dir: &str, filepath: Option<&str>) -> Result<PathBuf, String> {
    let d = Path::new(dir);
    let real_dir = std::fs::canonicalize(d).map_err(|e| format!("{dir}: {e}"))?;
    let cand = match filepath {
        // Newest *regular file* only: a planted symlink named e.g. "z.bin"
        // would otherwise sort last and be picked implicitly.
        None => {
            let newest = bins_sorted(d).into_iter().rev()
                .find(|n| std::fs::symlink_metadata(d.join(n)).map_or(false, |m| m.file_type().is_file()))
                .ok_or_else(|| format!("No snapshot .bin files in {dir}"))?;
            d.join(newest)
        }
        Some(fp) if Path::new(fp).is_absolute() => PathBuf::from(fp),
        Some(fp) => d.join(fp),
    };
    let shown = filepath.unwrap_or("(newest)");
    let real = std::fs::canonicalize(&cand).map_err(|_| format!("Snapshot file not found: {shown}"))?;
    // Containment is checked on every path, implicit or caller-supplied.
    if !real.starts_with(&real_dir) {
        return Err(format!("Refusing to restore from outside snapshot_dir: {shown}"));
    }
    Ok(real)
}

pub fn restore(gpu: Gpu, dir: &str, filepath: Option<&str>) -> Result<(), String> {
    let path = resolve_restore_path(dir, filepath)?;
    if !path.is_file() { return Err(format!("Snapshot file not found: {}", path.display())); }
    let mut raw = Vec::with_capacity(CT_SIZE + 1);
    std::fs::File::open(&path).and_then(|f| f.take(CT_SIZE as u64 + 1).read_to_end(&mut raw))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if raw.len() != CT_SIZE {
        return Err(format!("Snapshot size mismatch: expected {CT_SIZE}, got {}", raw.len()));
    }
    let vw = read_u32(&raw, 0);
    let expected = (1u32 << 16) | CT_SIZE as u32;
    if vw != expected {
        return Err(format!("Version word mismatch: 0x{vw:08X} (expected 0x{expected:08X})"));
    }
    let mut buf = Buf::from_bytes(&raw);
    if let Ok(mask) = get_boost_mask(gpu) { buf.bytes_mut()[4..36].copy_from_slice(&mask); }
    info!("Restoring ClockBoostTable from {}", path.display());
    match nvcall_raw(fid::SET_CLOCK_BOOST_TABLE, gpu, &mut buf) {
        (0, _) => Ok(()),
        (r, d) => Err(format!("SetClockBoostTable returned {r} ({d})")),
    }
}

pub fn list_snapshots(dir: &str) -> Vec<SnapshotInfo> {
    let d = Path::new(dir);
    let mut names: Vec<String> = std::fs::read_dir(d).into_iter().flatten().filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json")).collect();
    names.sort_by(|a, b| b.cmp(a));
    names.into_iter().filter_map(|n| {
        let p = d.join(&n);
        if std::fs::metadata(&p).ok()?.len() > 64 * 1024 { return None; }
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
        let default_bin = p.with_extension("bin").to_string_lossy().into_owned();
        Some(SnapshotInfo {
            filepath: v["file"].as_str().map(str::to_owned).unwrap_or(default_bin),
            timestamp: v["timestamp"].as_str().unwrap_or("").into(),
            gpu: v["gpu"].as_str().unwrap_or("").into(),
            nonzero_offsets: v["nonzero_offsets"].as_u64().unwrap_or(0),
            size: v["size"].as_u64().unwrap_or(0),
        })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_path_containment() {
        let base = std::env::temp_dir().join(format!("nvc-snap-{}", std::process::id()));
        let snaps = base.join("snaps");
        std::fs::create_dir_all(&snaps).unwrap();
        std::fs::write(snaps.join("clock_boost_table_1.bin"), b"x").unwrap();
        std::fs::write(snaps.join("clock_boost_table_2.bin"), b"x").unwrap();
        std::fs::write(base.join("evil.bin"), b"x").unwrap();
        std::os::unix::fs::symlink(base.join("evil.bin"), snaps.join("link.bin")).unwrap();
        let d = snaps.to_str().unwrap();
        assert!(resolve_restore_path(d, None).unwrap().ends_with("clock_boost_table_2.bin"));
        assert!(resolve_restore_path(d, Some("clock_boost_table_1.bin")).is_ok());
        assert!(resolve_restore_path(d, Some("../evil.bin")).is_err());
        assert!(resolve_restore_path(d, Some("link.bin")).is_err());
        assert!(resolve_restore_path(d, Some(base.join("evil.bin").to_str().unwrap())).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn prune_keeps_newest() {
        let d = std::env::temp_dir().join(format!("nvc-prune-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..5 {
            std::fs::write(d.join(format!("clock_boost_table_{i}.bin")), b"").unwrap();
            std::fs::write(d.join(format!("clock_boost_table_{i}.json")), b"{}").unwrap();
        }
        prune(&d, 10); // under limit: nothing removed (the Python bug)
        assert_eq!(bins_sorted(&d).len(), 5);
        prune(&d, 2);
        assert_eq!(bins_sorted(&d), vec!["clock_boost_table_3.bin", "clock_boost_table_4.bin"]);
        assert!(!d.join("clock_boost_table_0.json").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
