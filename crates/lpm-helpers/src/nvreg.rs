//! NVIDIA kernel module options (modprobe.d) — a curated, validated subset.
//!
//! LPM owns one file, loaded last (`zz-` sorts after the usual nvidia.conf),
//! so its values win over other modprobe.d files; the kernel command line
//! still wins over all of them. Changes apply the next time the module loads
//! (reboot); an initramfs that carries the nvidia modules must be rebuilt.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const LPM_FILE: &str = "/etc/modprobe.d/zz-legion-power-manager-nvidia.conf";
const DIRS: &[&str] = &["/etc/modprobe.d", "/run/modprobe.d", "/usr/local/lib/modprobe.d", "/usr/lib/modprobe.d", "/lib/modprobe.d"];

#[derive(Clone, Copy)]
pub enum Kind { Bool, Choice(&'static [(&'static str, &'static str)]), Int(i64, i64), Path, Dwords }

pub struct Param { pub module: &'static str, pub name: &'static str, pub kind: Kind, pub desc: &'static str }

const fn p(module: &'static str, name: &'static str, kind: Kind, desc: &'static str) -> Param { Param { module, name, kind, desc } }

pub const PARAMS: &[Param] = &[
    p("nvidia", "NVreg_DynamicPowerManagement", Kind::Choice(&[("0x00", "off (dGPU never powers down)"), ("0x01", "coarse"), ("0x02", "fine (RTD3, laptop default)"), ("0x03", "auto (driver decides)")]),
      "Runtime D3 power management of the dGPU. 'fine' lets the GPU power off when idle (biggest battery win on a hybrid laptop); 'off' keeps it powered - a test when resume from idle misbehaves (GSP timeouts right after a game starts)."),
    p("nvidia", "NVreg_DynamicPowerManagementVideoMemoryThreshold", Kind::Int(0, 1024),
      "MB of video memory in use below which the GPU may still power off (fine mode); contents are copied to system RAM. 0 = never power off with VRAM in use."),
    p("nvidia", "NVreg_PreserveVideoMemoryAllocations", Kind::Bool,
      "Save all video memory across suspend/hibernate (needs the nvidia-suspend hooks). Without it a Wayland session or a running game can lose its GPU state on resume."),
    p("nvidia", "NVreg_TemporaryFilePath", Kind::Path,
      "Where video memory is saved on suspend when PreserveVideoMemoryAllocations=1. Must be on a real disk with room for the whole VRAM (e.g. /var/tmp), not a tmpfs."),
    p("nvidia", "NVreg_EnableS0ixPowerManagement", Kind::Bool,
      "Use S0ix (modern standby) power management for suspend instead of S3. Only when the platform suspends to s2idle."),
    p("nvidia", "NVreg_EnableGpuFirmware", Kind::Bool,
      "Run the resource manager on the GPU's GSP firmware. Blackwell (RTX 50) and the open kernel modules always use GSP - 0 has no effect there."),
    p("nvidia", "NVreg_UsePageAttributeTable", Kind::Bool,
      "Use the kernel's PAT for write-combined mappings (1, recommended) instead of the driver's legacy MTRR path."),
    p("nvidia", "NVreg_InitializeSystemMemoryAllocations", Kind::Bool,
      "Zero system memory before handing it to the GPU (1 = default, safer). 0 saves a little CPU time on large allocations."),
    p("nvidia", "NVreg_EnableMSI", Kind::Bool,
      "Message-signalled interrupts (1, default). 0 falls back to legacy INTx - only for debugging interrupt problems."),
    p("nvidia", "NVreg_EnableResizableBar", Kind::Bool,
      "Let the driver resize BAR1 when the firmware supports Resizable BAR."),
    p("nvidia", "NVreg_RegistryDwords", Kind::Dwords,
      "Raw driver registry keys, 'Key=Value;Key=Value' (e.g. RMUseSwI2c=0x01). Only for keys you know - wrong ones can stop the GPU from initializing."),
    p("nvidia_drm", "modeset", Kind::Bool,
      "Kernel modesetting for nvidia-drm. Required for Wayland and for PRIME sync; recent drivers default to 1."),
    p("nvidia_drm", "fbdev", Kind::Bool,
      "nvidia-drm's own framebuffer console (needs modeset=1). Recent drivers default to 1."),
];

pub fn find(name: &str) -> Option<&'static Param> { PARAMS.iter().find(|p| p.name == name) }

fn num(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) { i64::from_str_radix(h, 16).ok() } else { s.parse().ok() }
}

/// Canonical value or an error.
pub fn validate(p: &Param, v: &str) -> Result<String, String> {
    let v = v.trim();
    match p.kind {
        Kind::Bool => match v { "0" | "N" | "n" => Ok("0".into()), "1" | "Y" | "y" => Ok("1".into()), _ => Err(format!("{}: must be 0 or 1", p.name)) },
        Kind::Choice(opts) => {
            let n = num(v).ok_or_else(|| format!("{}: not a number", p.name))?;
            opts.iter().find(|(o, _)| num(o) == Some(n)).map(|(o, _)| o.to_string()).ok_or_else(|| format!("{}: not offered", p.name))
        }
        Kind::Int(lo, hi) => {
            let n = num(v).filter(|n| (lo..=hi).contains(n)).ok_or_else(|| format!("{}: must be {lo}..{hi}", p.name))?;
            Ok(n.to_string())
        }
        Kind::Path => {
            let ok = v.starts_with('/') && v.len() <= 200 && !v.split('/').any(|c| c == "..")
                && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b));
            if ok { Ok(v.into()) } else { Err(format!("{}: absolute path of letters, digits, / _ . - only", p.name)) }
        }
        Kind::Dwords => {
            let ok = !v.is_empty() && v.len() <= 512 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"_=;,".contains(&b))
                && v.split(';').filter(|e| !e.is_empty()).all(|e| e.split_once('=').map_or(false, |(k, x)| !k.is_empty() && num(x).is_some()));
            if ok { Ok(v.into()) } else { Err(format!("{}: expected Key=Value;Key=Value with numeric values", p.name)) }
        }
    }
}

/// Value as the running module reports it, normalised like validate().
fn running(p: &Param) -> Option<String> {
    let raw = if p.module == "nvidia" {
        let s = std::fs::read_to_string("/proc/driver/nvidia/params").ok()?;
        let key = p.name.strip_prefix("NVreg_")?;
        s.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':').map(|v| v.trim().trim_matches('"').to_owned()))?
    } else {
        std::fs::read_to_string(format!("/sys/module/{}/parameters/{}", p.module, p.name)).ok()?.trim().to_owned()
    };
    Some(match p.kind {
        Kind::Bool => match raw.as_str() { "Y" | "1" => "1".into(), "N" | "0" => "0".into(), _ => raw },
        Kind::Choice(opts) => num(&raw).and_then(|n| opts.iter().find(|(o, _)| num(o) == Some(n))).map_or(raw, |(o, _)| o.to_string()),
        _ => raw,
    })
}

/// modprobe.d files in load order (a same-named file in an earlier directory masks the later one).
fn conf_files() -> Vec<PathBuf> {
    let mut by_name: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for d in DIRS.iter().rev() {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".conf") && e.path().is_file() { by_name.insert(n, e.path()); }
        }
    }
    by_name.into_values().collect()
}

/// (source, module, name, value) for every nvidia option set anywhere, in effect order.
fn configured() -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    let mut push = |src: &str, module: &str, kv: &str| {
        let module = module.replace('-', "_");
        if module != "nvidia" && module != "nvidia_drm" { return; }
        if let Some((k, v)) = kv.split_once('=') { out.push((src.to_owned(), module, k.to_owned(), v.trim_matches('"').to_owned())); }
    };
    for f in conf_files() {
        let Ok(s) = std::fs::read_to_string(&f) else { continue };
        for l in s.lines() {
            let l = l.split('#').next().unwrap_or("").trim();
            let mut w = l.split_whitespace();
            if w.next() != Some("options") { continue; }
            let Some(m) = w.next() else { continue };
            for kv in w { push(&f.display().to_string(), m, kv); }
        }
    }
    if let Ok(c) = std::fs::read_to_string("/proc/cmdline") {
        for t in c.split_whitespace() {
            if let Some((m, kv)) = t.split_once('.') { push("kernel command line", m, kv); }
        }
    }
    out
}

pub fn describe() -> Value {
    let conf = configured();
    let loaded = Path::new("/proc/driver/nvidia/params").is_file();
    let params: Vec<Value> = PARAMS.iter().map(|p| {
        let srcs: Vec<Value> = conf.iter().filter(|c| c.1 == p.module && c.2 == p.name)
            .map(|c| json!({"source": c.0, "value": c.3})).collect();
        let lpm = conf.iter().filter(|c| c.0 == LPM_FILE && c.1 == p.module && c.2 == p.name).last().map(|c| c.3.clone());
        let next = conf.iter().filter(|c| c.1 == p.module && c.2 == p.name).last().map(|c| c.3.clone());
        let (kind, extra) = match p.kind {
            Kind::Bool => ("bool", json!(null)),
            Kind::Choice(o) => ("choice", json!(o.iter().map(|(v, l)| json!([v, l])).collect::<Vec<_>>())),
            Kind::Int(lo, hi) => ("int", json!([lo, hi])),
            Kind::Path => ("path", json!(null)),
            Kind::Dwords => ("dwords", json!(null)),
        };
        json!({"module": p.module, "name": p.name, "kind": kind, "options": extra, "desc": p.desc,
               "running": running(p), "sources": srcs, "lpm": lpm, "next_boot": next})
    }).collect();
    json!({"ok": true, "loaded": loaded, "file": LPM_FILE, "params": params})
}

/// Rewrites LPM's file from `values` (name -> value; null or absent = not set).
pub fn set(values: &Map<String, Value>) -> Result<usize, String> {
    let mut lines: Vec<(&str, Vec<String>)> = vec![("nvidia", vec![]), ("nvidia_drm", vec![])];
    for (k, v) in values {
        let p = find(k).ok_or_else(|| format!("{k}: not a managed option"))?;
        let v = match v {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => if *b { "1".into() } else { "0".into() },
            _ => return Err(format!("{k}: bad value")),
        };
        let v = validate(p, &v)?;
        lines.iter_mut().find(|l| l.0 == p.module).unwrap().1.push(format!("{}={v}", p.name));
    }
    let n: usize = lines.iter().map(|l| l.1.len()).sum();
    if n == 0 {
        return match std::fs::remove_file(LPM_FILE) {
            Ok(()) => Ok(0),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(format!("{LPM_FILE}: {e}")),
        };
    }
    let mut body = String::from("# Written by Legion Power Manager (NVIDIA → Driver options). Edit in the app;\n\
                                 # applies the next time the module loads. Rebuild the initramfs if it carries nvidia.\n");
    for (m, opts) in &lines { if !opts.is_empty() { body.push_str(&format!("options {m} {}\n", opts.join(" "))); } }
    crate::write_root_file(LPM_FILE, body.as_bytes())?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation() {
        let d = find("NVreg_DynamicPowerManagement").unwrap();
        assert_eq!(validate(d, "2").unwrap(), "0x02");
        assert!(validate(d, "7").is_err());
        let r = find("NVreg_RegistryDwords").unwrap();
        assert!(validate(r, "RMUseSwI2c=0x01;Foo=1").is_ok());
        assert!(validate(r, "a=1 b=2").is_err());
        assert!(validate(find("NVreg_TemporaryFilePath").unwrap(), "/var/../etc").is_err());
        assert_eq!(validate(find("modeset").unwrap(), "Y").unwrap(), "1");
    }
}
