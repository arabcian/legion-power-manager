//! lpm-autotune — profile this machine and derive an Optimizations preset.
//!
//!   lpm-autotune profile                       hardware profile (human readable)
//!   lpm-autotune <goal> [--json] [--save [NAME]]
//!
//! goal: powersave | gaming | throughput | desktop
//! --save writes ~/.config/legion-power-manager/tune-presets/<NAME>.json
//! (default name "Auto <Goal>"), the same folder the Optimizations tab and
//! lpm-gamemode read. Nothing is applied: load/apply it in the GUI, pick it as
//! the ★ game preset, or put it in a Scene. Runs unprivileged.

use lpm_helpers::autotune::{self, Goal, Profile};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!("usage: lpm-autotune profile\n       lpm-autotune <powersave|gaming|throughput|desktop> [--json] [--save [NAME]]");
    std::process::exit(2);
}

/// Same rule as the GUI's validPresetName / lpm-gamemode's valid_name.
fn valid_name(n: &str) -> bool {
    let mut ch = n.chars();
    let first_ok = ch.next().map_or(false, char::is_alphanumeric);
    first_ok && n.len() <= 64 && !n.contains("..") && n.chars().all(|c| c.is_alphanumeric() || " _.-".contains(c))
}

fn presets_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("legion-power-manager/tune-presets"))
}

fn main() {
    lpm_helpers::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(first) = args.first() else { usage() };
    if first == "profile" {
        let p = Profile::gather();
        if args.iter().any(|a| a == "--json") { println!("{}", serde_json::to_string_pretty(&p.to_json()).unwrap()); }
        else { println!("{}", p.summary()); }
        return;
    }
    let Some(goal) = Goal::parse(first) else { usage() };
    let json_out = args.iter().any(|a| a == "--json");
    let save = args.iter().position(|a| a == "--save").map(|i| {
        args.get(i + 1).filter(|n| !n.starts_with("--")).cloned().unwrap_or_else(|| goal.preset_name().to_owned())
    });

    let out = autotune::autotune(goal);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "Goal:     {}\nMachine:  {}\n", goal.label(), out["profile_summary"].as_str().unwrap_or(""));
        if let Some(e) = out["evidence_summary"].as_str().filter(|e| !e.is_empty()) { let _ = writeln!(o, "Observed: {e}\n"); }
        let values = out["preset"]["values"].as_object().cloned().unwrap_or_default();
        let why = out["rationale"].as_object().cloned().unwrap_or_default();
        let w = values.keys().map(String::len).max().unwrap_or(10);
        for (k, v) in &values {
            let val = match v { Value::String(s) => s.clone(), x => x.to_string() };
            let _ = writeln!(o, "  {k:<w$}  {val:<20}  {}", why.get(k).and_then(Value::as_str).unwrap_or(""));
        }
        let run = &out["preset"]["run"];
        let _ = writeln!(o, "\n  launch: nice {} · affinity {}  {}", run["nice"], run["affinity"].as_str().unwrap_or("none"),
                         why.get("run").and_then(Value::as_str).unwrap_or(""));
        if let Some(sk) = out["skipped"].as_array().filter(|a| !a.is_empty()) {
            let _ = writeln!(o, "\n  not offered here: {}", sk.iter().filter_map(|s| s["key"].as_str()).collect::<Vec<_>>().join(", "));
        }
    }
    if let Some(name) = save {
        if !valid_name(&name) { eprintln!("invalid preset name '{name}' (letters, digits, space, _ - .)"); std::process::exit(1); }
        let Some(dir) = presets_dir() else { eprintln!("cannot find the config directory ($HOME unset)"); std::process::exit(1) };
        let mut preset = out["preset"].clone();
        preset["version"] = Value::from(1);
        let file = dir.join(format!("{name}.json"));
        let tmp = dir.join(format!(".{name}.json.tmp"));
        let res = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&tmp, serde_json::to_vec_pretty(&preset).unwrap()))
            .and_then(|_| std::fs::rename(&tmp, &file));
        match res {
            Ok(()) => eprintln!("saved {}", file.display()),
            Err(e) => { let _ = std::fs::remove_file(&tmp); eprintln!("{}: {e}", file.display()); std::process::exit(1); }
        }
    }
}
