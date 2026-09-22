//! Minimal `log` backend: stderr, optionally teeing into a capture buffer
//! (the root helper returns autoload's log output to the GUI as its message).

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::sync::Mutex;

struct Logger { level: Level, capture: bool }

static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool { m.level() <= self.level }
    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) { return; }
        let line = format!("{} {}: {}", r.level(), r.target(), r.args());
        if self.capture {
            CAPTURED.lock().unwrap_or_else(|p| p.into_inner()).push(line);
        } else {
            eprintln!("{line}");
        }
    }
    fn flush(&self) {}
}

/// Initialise once. `capture = true` buffers instead of printing.
pub fn init(level: Level, capture: bool) {
    let l = Box::leak(Box::new(Logger { level, capture }));
    if log::set_logger(l).is_ok() {
        log::set_max_level(LevelFilter::from(level.to_level_filter()));
    }
}

/// Drain captured lines.
pub fn take_captured() -> Vec<String> {
    std::mem::take(&mut *CAPTURED.lock().unwrap_or_else(|p| p.into_inner()))
}

/// Level from $NVCURVE_LOG (error|warn|info|debug), default `default`.
/// Only used by unprivileged binaries; the root helper ignores the env.
pub fn level_from_env(default: Level) -> Level {
    std::env::var("NVCURVE_LOG").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
