//! Child-process execution with a hard timeout and bounded output capture.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_OUTPUT: u64 = 64 * 1024;

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub enum RunError {
    NotFound,
    Spawn(std::io::Error),
    Timeout,
}

fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(r) = r { let _ = r.take(MAX_OUTPUT).read_to_end(&mut v); }
        String::from_utf8_lossy(&v).trim().to_owned()
    })
}

/// Runs `bin args...` with a minimal fixed environment. Never uses a shell.
pub fn run(bin: &str, args: &[String], timeout: Duration) -> Result<Output, RunError> {
    let mut child = Command::new(bin)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { RunError::NotFound } else { RunError::Spawn(e) })?;
    let o = drain(child.stdout.take());
    let e = drain(child.stderr.take());
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => return Err(RunError::Spawn(err)),
        }
    };
    Ok(Output {
        code: status.code(),
        stdout: o.join().unwrap_or_default(),
        stderr: e.join().unwrap_or_default(),
    })
}

/// First existing candidate that is root-owned, not group/world-writable
/// and executable. Used for any binary a root process executes.
pub fn find_trusted(candidates: &[&'static str]) -> Option<&'static str> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    candidates.iter().copied().find(|p| {
        let Ok(md) = std::fs::metadata(p) else { return false };
        md.is_file() && md.uid() == 0 && md.permissions().mode() & 0o022 == 0
            && md.permissions().mode() & 0o111 != 0
    })
}
