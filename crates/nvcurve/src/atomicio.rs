//! Atomic, mode-explicit writes for root-owned state (port of atomicio.py).
//!
//! mkstemp (O_EXCL, 0600) in the target directory → write → fsync → fchmod
//! → rename → fsync(dir). A reader sees the old file or the new one, never a
//! truncated one, and the invoking user's umask never decides the mode.

use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;
use std::path::Path;

fn fsync_dir(dir: &Path) {
    if let Ok(f) = std::fs::OpenOptions::new().read(true)
        .custom_flags_dir().open(dir) { let _ = f.sync_all(); }
}

trait DirFlags { fn custom_flags_dir(&mut self) -> &mut Self; }
impl DirFlags for std::fs::OpenOptions {
    fn custom_flags_dir(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
    }
}

pub fn write_bytes(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let abs = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let dir = abs.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut tmpl = dir.join(".nvcurve-XXXXXX.tmp").as_os_str().as_bytes().to_vec();
    tmpl.push(0);
    let fd = unsafe { libc::mkstemps(tmpl.as_mut_ptr() as *mut libc::c_char, 4) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    tmpl.pop();
    let tmp_path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&tmpl));
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };

    let res = (|| {
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
        if unsafe { libc::fchmod(fd, mode as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        std::fs::rename(&tmp_path, &abs)
    })();
    drop(f);
    if res.is_err() { let _ = std::fs::remove_file(&tmp_path); }
    res?;
    fsync_dir(&dir);
    Ok(())
}

pub fn write_json(path: &Path, v: &serde_json::Value, mode: u32) -> io::Result<()> {
    let s = serde_json::to_string_pretty(v).map_err(io::Error::other)?;
    write_bytes(path, s.as_bytes(), mode)
}

/// create_dir_all + explicit chmod (umask-independent).
pub fn ensure_dir(dir: &Path, mode: u32) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let c = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
    unsafe { libc::chmod(c.as_ptr(), mode as libc::mode_t) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn atomic_replace_and_mode() {
        let d = std::env::temp_dir().join(format!("nvc-at-{}", std::process::id()));
        ensure_dir(&d, 0o755).unwrap();
        let p = d.join("x.json");
        std::fs::write(&p, b"old").unwrap();
        unsafe { libc::umask(0o000) };
        write_json(&p, &serde_json::json!({"a": 1}), 0o644).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("\"a\": 1"));
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        let leftovers: Vec<_> = std::fs::read_dir(&d).unwrap().filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".nvcurve-")).collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
