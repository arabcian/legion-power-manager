//! Health → tool pages that need root: a fixed whitelist of read-only
//! commands with fixed arguments (nothing comes from the request but the
//! tool's name). Binaries must be root-owned in system directories.

use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const MAX_OUT: u64 = 1 << 20;
const DIRS: &[&str] = &["/usr/sbin", "/usr/bin", "/sbin", "/bin", "/usr/local/sbin", "/usr/local/bin"];

fn bin(name: &str) -> Option<PathBuf> {
    DIRS.iter().map(|d| Path::new(d).join(name)).find(|p| crate::trusted_path(p))
}

/// stdout (+ stderr appended) of `bin args`, killed after `secs`.
fn capture(bin: &Path, args: &[&str], secs: u64) -> String {
    let child = Command::new(bin).args(args).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LC_ALL", "C")
        .current_dir("/").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match child { Ok(c) => c, Err(e) => return format!("{}: {e}\n", bin.display()) };
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let tx2 = tx.clone();
    std::thread::spawn(move || { let mut v = Vec::new(); let _ = (&mut out).take(MAX_OUT).read_to_end(&mut v); let _ = tx.send((0, v)); });
    std::thread::spawn(move || { let mut v = Vec::new(); let _ = (&mut err).take(64 * 1024).read_to_end(&mut v); let _ = tx2.send((1, v)); });
    let (mut so, mut se) = (Vec::new(), Vec::new());
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((0, v)) => so = v,
            Ok((_, v)) => se = v,
            Err(_) => { let _ = child.kill(); let _ = child.wait(); return format!("{}: timed out after {secs} s\n", bin.display()); }
        }
    }
    let _ = child.wait();
    let mut s = String::from_utf8_lossy(&so).into_owned();
    if !se.is_empty() { s.push_str(&String::from_utf8_lossy(&se)); }
    s
}

fn section(out: &mut String, cmd: &str, body: &str) {
    out.push_str(&format!("$ {cmd}\n{body}"));
    if !body.ends_with('\n') { out.push('\n'); }
    out.push('\n');
}

fn missing(out: &mut String, name: &str, pkg: &str) { section(out, name, &format!("(not installed — emerge {pkg})")); }

/// Storage devices smartctl / nvme-cli can read (NVMe controllers, SATA disks).
fn disks() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/dev").into_iter().flatten().flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| (n.starts_with("nvme") && n[4..].chars().all(|c| c.is_ascii_digit()) && n.len() > 4)
            || (n.starts_with("sd") && n.len() == 3 && n.as_bytes()[2].is_ascii_lowercase()))
        .map(|n| format!("/dev/{n}")).collect();
    v.sort();
    v
}

pub fn run(tool: &str) -> Value {
    let mut out = String::new();
    match tool {
        "dmidecode" => match bin("dmidecode") {
            Some(b) => section(&mut out, "dmidecode", &capture(&b, &[], 20)),
            None => missing(&mut out, "dmidecode", "sys-apps/dmidecode"),
        },
        "smart" => {
            let (sc, nv) = (bin("smartctl"), bin("nvme"));
            if sc.is_none() && nv.is_none() { missing(&mut out, "smartctl", "sys-apps/smartmontools (or sys-apps/nvme-cli)"); }
            for d in disks() {
                if let Some(b) = &sc { section(&mut out, &format!("smartctl -a {d}"), &capture(b, &["-a", &d], 30)); }
                else if let Some(b) = nv.as_ref().filter(|_| d.starts_with("/dev/nvme")) {
                    section(&mut out, &format!("nvme smart-log {d}"), &capture(b, &["smart-log", &d], 30));
                }
            }
        }
        "dmesg" => match bin("dmesg") {
            Some(b) => section(&mut out, "dmesg --level=emerg,alert,crit,err,warn", &capture(&b, &["--level=emerg,alert,crit,err,warn"], 20)),
            None => missing(&mut out, "dmesg", "sys-apps/util-linux"),
        },
        "pcie" => match bin("lspci") {
            Some(b) => section(&mut out, "lspci -vv", &capture(&b, &["-vv"], 20)),
            None => missing(&mut out, "lspci", "sys-apps/pciutils"),
        },
        _ => return json!({"ok": false, "error": "unknown tool"}),
    }
    json!({"ok": true, "output": out})
}
