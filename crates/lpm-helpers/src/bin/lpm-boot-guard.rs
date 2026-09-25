//! lpm-boot-guard — keeps an unstable boot preset from crash-looping the machine.
//!
//!   lpm-boot-guard arm      (root) start of boot: arm, or trip if the last boot died armed
//!   lpm-boot-guard watch    (root) arm, then disarm after the stable window or on SIGTERM
//!                                  (systemd Type=simple unit; OpenRC uses arm + a sleeper)
//!   lpm-boot-guard disarm   (root) this boot is fine (window over / clean shutdown)
//!   lpm-boot-guard check    exit 0 = apply presets, 1 = skip (reason on stderr) — for
//!                           systemd ExecCondition= and the OpenRC scripts
//!   lpm-boot-guard status   print the state as JSON
//!   lpm-boot-guard reset    (root) resume the presets after a trip
//!
//! See bootguard.rs for the state machine.

use lpm_helpers::bootguard;
use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn on_term(_: libc::c_int) { STOP.store(true, Ordering::SeqCst); }

fn fail(e: String) -> i32 { eprintln!("lpm-boot-guard: {e}"); 2 }

fn arm() -> i32 {
    match bootguard::arm() {
        Ok(v) => {
            if v["tripped"] == true {
                eprintln!("lpm-boot-guard: boot presets PAUSED — {}. Resume them in Legion Power Manager (Home).",
                          v["reason"].as_str().unwrap_or("previous boot failed"));
            }
            0
        }
        Err(e) => fail(e),
    }
}

fn watch() -> i32 {
    let code = arm();
    if code != 0 || bootguard::read()["state"] != "armed" { return code; }
    for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(s, on_term as *const () as libc::sighandler_t) };
    }
    // Sleep in short steps so a shutdown within the window disarms promptly.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(bootguard::WINDOW_SECS);
    while std::time::Instant::now() < end && !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    bootguard::disarm().map_or_else(fail, |_| 0)
}

fn main() {
    lpm_helpers::init();
    let code = match std::env::args().nth(1).as_deref() {
        Some("arm") => arm(),
        Some("watch") => watch(),
        Some("disarm") => bootguard::disarm().map_or_else(fail, |_| 0),
        Some("check") => match bootguard::should_skip() {
            None => 0,
            Some(why) => { eprintln!("lpm-boot-guard: skipping boot presets — {why}"); 1 }
        },
        Some("status") => { println!("{}", bootguard::read()); 0 }
        Some("reset") => bootguard::reset().map_or_else(fail, |v| { println!("{v}"); 0 }),
        _ => { eprintln!("usage: lpm-boot-guard arm|watch|disarm|check|status|reset"); 2 }
    };
    std::process::exit(code);
}
