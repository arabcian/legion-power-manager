//! Safe hand-over of the NVIDIA dGPU around Lenovo's "iGPU mode".
//!
//! iGPU only (and Auto, on battery) makes the EC cut the dGPU's power and send
//! an ACPI eject for its slot. If the nvidia driver is still bound and something
//! holds /dev/nvidia* or the card's DRM node (compositor, nvidia-powerd, a game),
//! the kernel's remove path waits forever inside `nvidia` (os_delay) while it
//! holds the PCI rescan/remove lock: every later PCI rescan sits in D state and
//! only a reboot clears it.
//!
//! So: before the card is cut, unload the driver while it is still alive
//! (`release`); `restore` brings it back. Every step that touches the driver or
//! the PCI bus runs in a child with a deadline, so a hang can never take this
//! helper (or its pkexec caller) with it.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PCI: &str = "/sys/bus/pci/devices";
/// Unload order (dependants first).
const MODULES: &[&str] = &["nvidia_uvm", "nvidia_drm", "nvidia_modeset", "nvidia"];
/// Load order.
const LOAD: &[&str] = &["nvidia", "nvidia_modeset", "nvidia_uvm", "nvidia_drm"];
/// Daemons that keep the GPU open. Stopped by release, started again by restore.
const SERVICES: &[&str] = &["nvidia-powerd", "nvidia-persistenced"];
const MARK: &str = "/run/legion-power-manager/dgpu-stopped.json";

fn rd(p: &Path) -> Option<String> { std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned()) }

fn cards() -> Vec<PathBuf> {
    std::fs::read_dir(PCI).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|d| rd(&d.join("vendor")).as_deref() == Some("0x10de") && rd(&d.join("class")).map_or(false, |c| c.starts_with("0x03")))
        .collect()
}

fn module_loaded(m: &str) -> bool { Path::new("/sys/module").join(m).exists() }

/// A process stuck in a PCI rescan/remove: the bus lock is held by a wedged remove.
/// Only a reboot clears it; more attempts just pile up more D-state processes.
pub fn wedged() -> bool {
    std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| {
        e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())
            && rd(&e.path().join("wchan")).map_or(false, |w| w.contains("rescan_store") || w.contains("remove_store") || w.contains("pci_lock_rescan_remove"))
    })
}

/// Processes with the NVIDIA GPU open: /dev/nvidia* and DRM nodes whose device is NVIDIA.
pub fn holders() -> Vec<Value> {
    let me = std::process::id();
    let nv_drm = |name: &str| rd(&Path::new("/sys/class/drm").join(name).join("device/vendor")).as_deref() == Some("0x10de");
    let mut out: Vec<Value> = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        if pid == me { continue; }
        let mut what: Vec<String> = Vec::new();
        for fd in std::fs::read_dir(e.path().join("fd")).into_iter().flatten().flatten() {
            let Ok(t) = std::fs::read_link(fd.path()) else { continue };
            let t = t.to_string_lossy().into_owned();
            let hit = t.starts_with("/dev/nvidia")
                || t.strip_prefix("/dev/dri/").map_or(false, nv_drm);
            if hit && !what.contains(&t) { what.push(t); }
        }
        if what.is_empty() { continue; }
        out.push(json!({"pid": pid, "comm": rd(&e.path().join("comm")).unwrap_or_default(), "files": what}));
    }
    out
}

/// Vendor id of the first non-NVIDIA display controller (the iGPU).
fn igpu_vendor() -> Option<String> {
    std::fs::read_dir(PCI).into_iter().flatten().flatten().find_map(|e| {
        let d = e.path();
        let ven = rd(&d.join("vendor"))?;
        (rd(&d.join("class"))?.starts_with("0x03") && matches!(ven.as_str(), "0x1002" | "0x8086")).then_some(ven)
    })
}

/// What to change so the compositor stops opening the NVIDIA GPU, if one is among the holders.
/// KWIN_DRM_DEVICES is a colon-separated list, so /dev/dri/by-path names (they contain the
/// PCI address with colons) cannot be used: the script picks the iGPU's cardN at login.
fn compositor_hint(h: &[Value]) -> String {
    let has = |n: &str| h.iter().any(|x| x["comm"].as_str() == Some(n));
    let ven = igpu_vendor().unwrap_or_else(|| "0x1002".into());
    if has("kwin_wayland") || has("kwin_x11") {
        format!(" KDE: create ~/.config/plasma-workspace/env/lpm-igpu.sh containing \
`for c in /sys/class/drm/card?; do [ \"$(cat $c/device/vendor)\" = {ven} ] && export KWIN_DRM_DEVICES=/dev/dri/${{c##*/}} && break; done` and \
`export __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json` (check the file name in that directory), then log in again. \
Monitors wired to the NVIDIA GPU stay dark in that session.")
    } else if has("Xorg") {
        " Xorg: add Option \"AutoAddGPU\" \"false\" to the ServerFlags section and restart X.".into()
    } else if has("gnome-shell") || has("mutter") {
        " GNOME: start the session with only the iGPU's DRM device as primary (mutter has no simple switch for this); monitors on the NVIDIA GPU go dark.".into()
    } else { String::new() }
}

fn holders_text(h: &[Value]) -> String {
    h.iter().take(8).map(|x| format!("{} (pid {})", x["comm"].as_str().unwrap_or("?"), x["pid"])).collect::<Vec<_>>().join(", ")
}

pub fn status() -> Value {
    json!({"ok": true, "cards": cards().len(),
           "bound": cards().iter().any(|c| c.join("driver").exists()),
           "modules": MODULES.iter().filter(|m| module_loaded(m)).collect::<Vec<_>>(),
           "gpus_listed": std::fs::read_dir("/proc/driver/nvidia/gpus").map(|d| d.count()).unwrap_or(0),
           "holders": holders(), "wedged": wedged()})
}

/// Runs `f` in a forked child; Ok(exit code) or Err on timeout (child killed).
fn in_child(secs: u64, f: impl FnOnce() -> i32) -> Result<i32, String> {
    let pid = unsafe { libc::fork() };
    if pid < 0 { return Err(format!("fork: {}", std::io::Error::last_os_error())); }
    if pid == 0 { let code = f(); unsafe { libc::_exit(code & 0xFF) } }
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
        if r == pid { return if libc::WIFEXITED(st) { Ok(libc::WEXITSTATUS(st)) } else { Err("terminated abnormally".into()) }; }
        if r < 0 { return Err(format!("waitpid: {}", std::io::Error::last_os_error())); }
        if Instant::now() >= end { unsafe { libc::kill(pid, libc::SIGKILL) }; return Err(format!("timed out after {secs} s")); }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// delete_module(2) with O_NONBLOCK: refuses at once (EBUSY) instead of waiting for users.
fn unload(name: &str) -> Result<(), String> {
    if !module_loaded(name) { return Ok(()); }
    let c = std::ffi::CString::new(name).map_err(|_| "bad module name".to_string())?;
    let code = in_child(20, || {
        let r = unsafe { libc::syscall(libc::SYS_delete_module, c.as_ptr(), libc::O_NONBLOCK) };
        if r == 0 { 0 } else { unsafe { *libc::__errno_location() } }
    }).map_err(|e| format!("unloading {name}: {e} (the driver is stuck; reboot)"))?;
    match code {
        0 | libc::ENOENT => Ok(()),
        libc::EBUSY => Err(format!("{name} is in use")),
        e => Err(format!("unloading {name}: {}", std::io::Error::from_raw_os_error(e))),
    }
}

fn running(name: &str) -> bool {
    let short: String = name.chars().take(15).collect();  // comm is 15 chars
    std::fs::read_dir("/proc").into_iter().flatten().flatten()
        .any(|e| rd(&e.path().join("comm")).as_deref() == Some(short.as_str()))
}

/// Unloads the NVIDIA driver while the card is still alive. Err carries the full
/// JSON reply (with "busy" and the holders) for the GUI.
pub fn release() -> Result<Value, Value> {
    let fail = |msg: String, h: Vec<Value>| json!({"ok": false, "busy": !h.is_empty(), "holders": h, "error": msg});
    if wedged() {
        return Err(fail("a PCI rescan/remove is already stuck in the kernel (the NVIDIA driver is waiting for a dead GPU): only a reboot clears it. Nothing was changed.".into(), vec![]));
    }
    if !module_loaded("nvidia") { return Ok(json!({"released": false, "note": "nvidia driver not loaded"})); }
    let mut stopped: Vec<&str> = Vec::new();
    for s in SERVICES {
        if running(s) && crate::tune::service_ctl(s, "stop") { stopped.push(s); }
    }
    if !stopped.is_empty() {
        let _ = crate::secure_dir("/run/legion-power-manager");
        let _ = crate::write_root_file(MARK, json!({"services": stopped}).to_string().as_bytes());
    }
    // A refused release must not leave the daemons we stopped (nvidia-powerd = Dynamic Boost)
    // down: nothing was unloaded yet, so put them back and drop the marker.
    let resume = |stopped: &[&str]| {
        for s in stopped { let _ = crate::tune::service_ctl(s, "start"); }
        if !stopped.is_empty() { let _ = std::fs::remove_file(MARK); }
    };
    // Daemons can take a moment to close their handles.
    for _ in 0..20 { if holders().is_empty() { break; } std::thread::sleep(Duration::from_millis(100)); }
    let h = holders();
    if !h.is_empty() {
        resume(&stopped);
        return Err(fail(format!("{} hold the NVIDIA GPU open, so its driver cannot be unloaded. Close them first.{}", holders_text(&h), compositor_hint(&h)), h));
    }
    let mut removed = false;  // once a module is gone the state is partial: restore() finishes the job
    for m in MODULES {
        let was_loaded = module_loaded(m);
        let res = unload(m);
        if res.is_ok() { removed |= was_loaded; }
        if let Err(e) = res {
            if !removed { resume(&stopped); }
            let h = holders();
            let msg = if h.is_empty() { e } else { format!("{e}: {} still hold the GPU", holders_text(&h)) };
            return Err(fail(msg, h));
        }
    }
    Ok(json!({"released": true, "services_stopped": stopped}))
}

fn modprobe(m: &str) -> bool {
    let Some(bin) = ["/sbin/modprobe", "/usr/sbin/modprobe", "/usr/bin/modprobe", "/bin/modprobe"].iter().map(PathBuf::from).find(|p| crate::trusted_path(p)) else { return false };
    let Ok(cm) = std::ffi::CString::new(m) else { return false };
    let Ok(cb) = std::ffi::CString::new(bin.as_os_str().to_string_lossy().as_bytes()) else { return false };
    in_child(30, || unsafe {
        let devnull = libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR);
        if devnull >= 0 { libc::dup2(devnull, 0); libc::dup2(devnull, 1); libc::dup2(devnull, 2); }
        let argv = [cb.as_ptr(), cm.as_ptr(), std::ptr::null()];
        libc::execv(cb.as_ptr(), argv.as_ptr());
        127
    }) == Ok(0)
}

/// Brings the dGPU back: rescan when it is missing from the bus, load the driver,
/// start the daemons release stopped.
pub fn restore() -> Value {
    if wedged() {
        return json!({"ok": false, "error": "a PCI rescan/remove is stuck in the kernel; only a reboot clears it. Nothing was changed."});
    }
    let mut note = Vec::<String>::new();
    if cards().is_empty() {
        let r = in_child(10, || match std::fs::write("/sys/bus/pci/rescan", "1") { Ok(()) => 0, Err(_) => 1 });
        match r {
            Ok(0) => {}
            Ok(_) => return json!({"ok": false, "error": "PCI rescan was refused"}),
            Err(e) => return json!({"ok": false, "error": format!("PCI rescan {e}: the bus is locked, reboot to recover")}),
        }
        for _ in 0..50 { if !cards().is_empty() { break; } std::thread::sleep(Duration::from_millis(100)); }
        if cards().is_empty() { return json!({"ok": false, "error": "the NVIDIA GPU did not reappear on the PCI bus (still cut off by the firmware? switch iGPU mode to Default and plug in AC)"}); }
        note.push("PCI rescan brought the GPU back".into());
    }
    let mut failed = Vec::new();
    for m in LOAD {
        if !module_loaded(m) && !modprobe(m) && *m == "nvidia" { failed.push(*m); break; }
    }
    if !failed.is_empty() { return json!({"ok": false, "error": "modprobe nvidia failed (see dmesg)"}); }
    if let Some(v) = crate::read_root_file(MARK, 4096).and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        for s in v["services"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if SERVICES.contains(&s) && !running(s) { let _ = crate::tune::service_ctl(s, "start"); }
        }
        let _ = std::fs::remove_file(MARK);
    }
    json!({"ok": true, "note": note.join("; "), "gpus_listed": std::fs::read_dir("/proc/driver/nvidia/gpus").map(|d| d.count()).unwrap_or(0)})
}
