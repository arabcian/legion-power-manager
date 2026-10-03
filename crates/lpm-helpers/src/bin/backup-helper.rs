//! backup-helper — Health → Backup: system image (tar) and LPM configuration
//! backups. Engine and archive layout: lpm_helpers::backup.
//!
//! Unlike the other helpers it streams: ONE JSON request line on stdin, then
//! `{"event":"start"|"progress",…}` lines on stdout while it works and one final
//! `{"ok":…}` line. Stdin stays open for the whole run — EOF (the GUI closed it,
//! or died) cancels: tar and the compressor are terminated and the partial
//! archive is removed.
//!
//!   as the user:   config_backup  config_inspect  config_restore_user  verify (readable archives)
//!   root (pkexec): system_backup  system_restore  config_restore_root  verify
//!
//! polkit: com.legion-power-manager.backup — the administrator password every
//! time, never cached, also for wheel at the machine: a system image contains
//! /etc/shadow and a restore can replace every file on the system.

use lpm_helpers::*;
use serde_json::{json, Value};
use std::io::{BufRead, Read};

fn main() {
    init();
    let mut line = String::new();
    let req: Value = match std::io::stdin().lock().take(64 * 1024).read_line(&mut line) {
        Ok(n) if n > 0 => match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => std::process::exit(finish(json!({"ok": false, "error": format!("invalid JSON: {e}")}))),
        },
        _ => std::process::exit(finish(json!({"ok": false, "error": "no request"}))),
    };
    backup::cancel_on_stdin_eof();
    let root = unsafe { libc::geteuid() } == 0;
    let op = req["op"].as_str().unwrap_or("");
    let out = match op {
        "config_backup" => backup::config_backup(&req),
        "config_inspect" => backup::config_inspect(&req),
        "config_restore_user" => backup::config_restore_user(&req),
        "verify" => backup::verify(&req),
        "system_backup" | "system_restore" | "config_restore_root" if !root =>
            json!({"ok": false, "needs_root": true, "error": format!("{op} must run as root (via pkexec)")}),
        "system_backup" => backup::system_backup(&req),
        "system_restore" => backup::system_restore(&req),
        "config_restore_root" => backup::config_restore_root(&req),
        _ => json!({"ok": false, "error": format!("unknown op: {op}")}),
    };
    std::process::exit(finish(out));
}
