//! legion-ec-sensors — root-only, read-only EC sensor stream for the Home tab.
//!
//! The embedded controller measures the dGPU temperature on its own (thermal
//! sensor on the board), so reading it does not go through the NVIDIA driver
//! and never wakes the GPU or resets its idle timer — unlike nvidia-smi/NVML.
//! Source: Lenovo WMI "Other Method" \_SB.GZFD.WMAE get (0x11), feature
//! 0x05050000 (GPUCurrentTemperature), via acpi_call — hence root.
//!
//! Started through pkexec while the Home tab is visible (read-only polkit
//! action, silent for the active local session); one JSON line every 2 s:
//!   {"gpu_temp_c": 47}        or {"gpu_temp_c": null} while the EC has no reading
//!
//! No arguments, no input. Stops on stdin EOF (tab hidden), when stdout goes
//! away, or when the parent dies. A firmware without the feature → one
//! {"error": …} line and exit 1.

use lpm_helpers::legion_wmi;
use std::io::{Read, Write};
use std::time::Duration;

const INTERVAL: Duration = Duration::from_secs(2);

fn main() {
    lpm_helpers::init();
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("legion-ec-sensors must run as root (via pkexec)");
        std::process::exit(1);
    }
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        if libc::getppid() == 1 { std::process::exit(0); }  // parent already gone
    }
    // stdin EOF = the tab was hidden.
    std::thread::spawn(|| {
        let mut b = [0u8; 64];
        while matches!(std::io::stdin().read(&mut b), Ok(n) if n > 0) {}
        std::process::exit(0);
    });
    let mut out = std::io::stdout().lock();
    loop {
        let line = match legion_wmi::ec_gpu_temp() {
            Ok(t) => serde_json::json!({"gpu_temp_c": t}),
            Err(e) => {
                let _ = writeln!(out, "{}", serde_json::json!({"error": e}));
                let _ = out.flush();
                std::process::exit(1);
            }
        };
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() { break; }
        std::thread::sleep(INTERVAL);
    }
}
