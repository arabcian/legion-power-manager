//! nvcurve-sensors — root-only NVIDIA temperatures for the NVIDIA tab.
//!
//! Blackwell's hotspot and the per-partition GDDR7 sensors are GPU register
//! reads that the driver only allows a privileged client. The tab starts this
//! through pkexec once per show (a read-only polkit action, no password for
//! the active local session) and it streams one JSON line every 2 s:
//!   {"hotspot_c": 45, "vram_c": 52, "vram_partitions": [["A0", 50], …]}
//!
//! It takes no arguments and no input: the only thing it can do is read the
//! fixed sensor offsets in hal::sensors. It stops when the tab closes its
//! stdin (EOF), when stdout goes away, when its parent dies, or after an hour.

use nvcurve::hal::{gpu, sensors};
use nvcurve::nvml;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(2);
const MAX_RUNTIME: Duration = Duration::from_secs(3600);

fn main() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("nvcurve-sensors must run as root (via pkexec)");
        std::process::exit(1);
    }
    unsafe {
        let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        if libc::getppid() == 1 { std::process::exit(0); }  // parent already gone
    }
    // stdin EOF = the tab was hidden.
    std::thread::spawn(|| {
        let mut b = [0u8; 64];
        while matches!(std::io::stdin().read(&mut b), Ok(n) if n > 0) {}
        std::process::exit(0);
    });
    let Ok((g, _)) = gpu::get_gpu(0) else {
        println!("{{\"error\":\"no NVIDIA GPU\"}}");
        std::process::exit(1);
    };
    let arch = nvml::ready().ok().and_then(|n| n.architecture(n.handle(0).ok()?).ok());
    let start = Instant::now();
    let mut out = std::io::stdout().lock();
    while start.elapsed() < MAX_RUNTIME {
        let s = sensors::read(g, arch);
        let line = serde_json::json!({
            "hotspot_c": s.hotspot_c, "vram_c": s.vram_c, "vram_partitions": s.vram_partitions,
        });
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() { break; }
        std::thread::sleep(INTERVAL);
    }
}
