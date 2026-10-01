//! lpm-autotune — profile this machine and derive an Optimizations preset.
//!
//!   lpm-autotune profile [--json]              hardware profile
//!   lpm-autotune <goal> [--json] [--save [NAME]] [--weights latency=1.2,footprint=0.8,...]
//!   lpm-autotune probe                         (root) measure the disk's sustained write rate
//!   lpm-autotune audit [--fix] [FILE...]       check presets/scenes for unsafe values
//!                                              (default: the user's scenes and tune presets)
//!   lpm-autotune report [SECONDS]              memory/THP/writeback diagnostics + counter
//!                                              deltas over SECONDS (default 10), for bug reports
//!
//! goal: powersave | gaming | throughput | desktop
//! weights: latency, throughput, power, footprint, stability, storage (0..3; stability >= 0.5)
//! --save writes ~/.config/legion-power-manager/tune-presets/<NAME>.json.
//! Nothing is applied here. Runs unprivileged (except probe).

use lpm_helpers::autotune::{self, AuditCtx, Goal, Profile};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

fn usage() -> ! {
    eprintln!("usage: lpm-autotune profile [--json]\n       lpm-autotune <powersave|gaming|throughput|desktop> [--json] [--save [NAME]] [--weights k=v,..]\n       lpm-autotune probe\n       lpm-autotune audit [--fix] [FILE...]\n       lpm-autotune report [SECONDS]");
    std::process::exit(2);
}

/// Same rule as the GUI's validPresetName / lpm-gamemode's valid_name.
fn valid_name(n: &str) -> bool {
    let mut ch = n.chars();
    let first_ok = ch.next().map_or(false, char::is_alphanumeric);
    first_ok && n.len() <= 64 && !n.contains("..") && n.chars().all(|c| c.is_alphanumeric() || " _.-".contains(c))
}

fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("legion-power-manager"))
}

fn parse_weights(s: &str) -> Value {
    let mut m = Map::new();
    for kv in s.split(',') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        if let Ok(n) = v.trim().parse::<f64>() { m.insert(k.trim().to_owned(), json!(n)); }
    }
    Value::Object(m)
}

fn main() {
    lpm_helpers::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(first) = args.first() else { usage() };
    match first.as_str() {
        "profile" => {
            let p = Profile::gather();
            if args.iter().any(|a| a == "--json") { println!("{}", serde_json::to_string_pretty(&p.to_json()).unwrap()); }
            else { println!("{}", p.summary()); }
        }
        "probe" => match lpm_helpers::iorate::probe("/var/tmp") {
            Ok(v) => println!("{} MiB/s sustained write on {} ({} MiB in {} s, direct I/O: {}) - saved for autotune",
                              v["bps"].as_u64().unwrap_or(0) >> 20, v["device"].as_str().unwrap_or("?"), v["bytes"].as_u64().unwrap_or(0) >> 20,
                              v["secs"], v["direct"]),
            Err(e) => { eprintln!("probe: {e}"); std::process::exit(1); }
        },
        "audit" => audit_cmd(&args[1..]),
        "report" => report(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10).clamp(1, 120)),
        _ => goal_cmd(&args),
    }
}

fn goal_cmd(args: &[String]) {
    let Some(goal) = Goal::parse(&args[0]) else { usage() };
    let json_out = args.iter().any(|a| a == "--json");
    let save = args.iter().position(|a| a == "--save").map(|i| {
        args.get(i + 1).filter(|n| !n.starts_with("--")).cloned().unwrap_or_else(|| goal.preset_name().to_owned())
    });
    let weights = args.iter().position(|a| a == "--weights").and_then(|i| args.get(i + 1)).map(|s| parse_weights(s)).unwrap_or(Value::Null);
    let out = autotune::autotune_req(goal, &weights);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "Goal:     {}\nMachine:  {}\nWeights:  {}\n", goal.label(), out["profile_summary"].as_str().unwrap_or(""), out["weights"]);
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
        for n in out["constraints"].as_array().into_iter().flatten() { let _ = writeln!(o, "  constraint: {}", n.as_str().unwrap_or("")); }
        for i in out["live_issues"].as_array().into_iter().flatten() {
            let _ = writeln!(o, "  live value: {} - {}", i["key"].as_str().unwrap_or(""), i["message"].as_str().unwrap_or(""));
        }
        if let Some(sk) = out["skipped"].as_array().filter(|a| !a.is_empty()) {
            let _ = writeln!(o, "\n  not offered here: {}", sk.iter().filter_map(|s| s["key"].as_str()).collect::<Vec<_>>().join(", "));
        }
    }
    if let Some(name) = save {
        if !valid_name(&name) { eprintln!("invalid preset name '{name}' (letters, digits, space, _ - .)"); std::process::exit(1); }
        let Some(dir) = config_dir().map(|d| d.join("tune-presets")) else { eprintln!("cannot find the config directory ($HOME unset)"); std::process::exit(1) };
        let mut preset = out["preset"].clone();
        preset["version"] = Value::from(1);
        let file = dir.join(format!("{name}.json"));
        if let Err(e) = write_atomic(&file, &serde_json::to_vec_pretty(&preset).unwrap()) {
            eprintln!("{}: {e}", file.display());
            std::process::exit(1);
        }
        eprintln!("saved {}", file.display());
    }
}

fn write_atomic(file: &Path, body: &[u8]) -> std::io::Result<()> {
    let dir = file.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp", file.file_name().and_then(|n| n.to_str()).unwrap_or("x")));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, file)
}

fn audit_cmd(args: &[String]) {
    let fix = args.iter().any(|a| a == "--fix");
    let mut files: Vec<PathBuf> = args.iter().filter(|a| !a.starts_with("--")).map(PathBuf::from).collect();
    if files.is_empty() {
        if let Some(d) = config_dir() {
            for sub in ["scenes", "tune-presets"] {
                let mut v: Vec<PathBuf> = std::fs::read_dir(d.join(sub)).into_iter().flatten().flatten().map(|e| e.path())
                    .filter(|p| p.extension().map_or(false, |x| x == "json")).collect();
                v.sort();
                files.extend(v);
            }
        }
    }
    let ctx = AuditCtx::live();
    let profile = fix.then(Profile::gather);
    let (mut bad, mut rejected) = (0, 0);
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else { eprintln!("{}: unreadable", f.display()); continue };
        let Ok(mut v) = serde_json::from_str::<Value>(&text) else { eprintln!("{}: not JSON", f.display()); continue };
        let issues = autotune::audit_tree(&v, &ctx);
        if issues.is_empty() { continue; }
        bad += 1;
        println!("{}", f.display());
        for (path, i) in &issues {
            rejected += i.reject as usize;
            println!("  {} {}{}: {}", if i.reject { "REJECT" } else { "warn  " }, i.key, if path.is_empty() { String::new() } else { format!(" ({path})") }, i.msg);
        }
        if let Some(p) = &profile {
            let log = autotune::repair_tree(&mut v, &ctx, p);
            let bak = f.with_extension("json.bak");
            match std::fs::copy(f, &bak).and_then(|_| write_atomic(f, &serde_json::to_vec_pretty(&v).unwrap())) {
                Ok(()) => { for l in log { println!("  fixed: {l}"); } println!("  backup: {}", bak.display()); }
                Err(e) => eprintln!("  not fixed: {e}"),
            }
        }
    }
    println!("{} file(s) checked, {bad} with issues, {rejected} value(s) the helper refuses{}", files.len(),
             if rejected > 0 && !fix { " - run with --fix, then re-save the scenes/presets in the GUI so the root store is rewritten" } else { "" });
    if rejected > 0 && !fix { std::process::exit(1); }
}

// ── report ─────────────────────────────────────────────────────────────────

fn rd(p: &str) -> String { std::fs::read_to_string(p).map(|s| s.trim().to_owned()).unwrap_or_else(|_| "-".into()) }

fn kv(text: &str, sep: char) -> Map<String, Value> {
    text.lines().filter_map(|l| {
        let (k, v) = l.split_once(sep)?;
        let n: u64 = v.split_whitespace().next()?.parse().ok()?;
        Some((k.trim().to_owned(), json!(n)))
    }).collect()
}

fn report(secs: u64) {
    let mem = kv(&rd("/proc/meminfo"), ':');
    let g = |k: &str| mem.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mib = |kb: u64| format!("{:>8} MiB", kb >> 10);
    println!("== memory (/proc/meminfo)");
    let used = g("MemTotal").saturating_sub(g("MemAvailable"));
    println!("  total {}  available {}  used (total-available) {}", mib(g("MemTotal")), mib(g("MemAvailable")), mib(used));
    for k in ["MemFree", "Cached", "Buffers", "AnonPages", "AnonHugePages", "ShmemHugePages", "Shmem", "SReclaimable", "SUnreclaim",
              "KernelStack", "PageTables", "Zswap", "Zswapped", "SwapTotal", "SwapFree", "Committed_AS"] {
        if mem.contains_key(k) { println!("  {k:<16} {}", mib(g(k))); }
    }
    // Watermarks: kswapd keeps free what the high marks add up to; MemAvailable excludes the reserve.
    let (mut high, mut low, mut min) = (0u64, 0u64, 0u64);
    let mut in_zone = false;
    for l in rd("/proc/zoneinfo").lines() {
        let t = l.trim();
        if t.starts_with("pages free") { in_zone = true; continue; }
        if in_zone {
            let mut w = t.split_whitespace();
            match (w.next(), w.next().and_then(|n| n.parse::<u64>().ok())) {
                (Some("min"), Some(n)) => min += n,
                (Some("low"), Some(n)) => low += n,
                (Some("high"), Some(n)) => { high += n; in_zone = false; }
                _ => {}
            }
        }
    }
    let page_kb = 4;
    println!("  watermarks (sum of zones): min {} low {} high {}", mib(min * page_kb), mib(low * page_kb), mib(high * page_kb));
    println!("\n== reclaim / THP / writeback settings");
    for (k, p) in [("min_free_kbytes", "/proc/sys/vm/min_free_kbytes"), ("watermark_scale_factor", "/proc/sys/vm/watermark_scale_factor"),
                   ("watermark_boost_factor", "/proc/sys/vm/watermark_boost_factor"), ("compaction_proactiveness", "/proc/sys/vm/compaction_proactiveness"),
                   ("swappiness", "/proc/sys/vm/swappiness"), ("vfs_cache_pressure", "/proc/sys/vm/vfs_cache_pressure"),
                   ("dirty_bytes", "/proc/sys/vm/dirty_bytes"), ("dirty_background_bytes", "/proc/sys/vm/dirty_background_bytes"),
                   ("dirty_ratio", "/proc/sys/vm/dirty_ratio"), ("dirty_background_ratio", "/proc/sys/vm/dirty_background_ratio"),
                   ("dirty_writeback_centisecs", "/proc/sys/vm/dirty_writeback_centisecs"),
                   ("thp enabled", "/sys/kernel/mm/transparent_hugepage/enabled"), ("thp defrag", "/sys/kernel/mm/transparent_hugepage/defrag"),
                   ("khugepaged max_ptes_none", "/sys/kernel/mm/transparent_hugepage/khugepaged/max_ptes_none"),
                   ("khugepaged pages_to_scan", "/sys/kernel/mm/transparent_hugepage/khugepaged/pages_to_scan"),
                   ("khugepaged scan_sleep_ms", "/sys/kernel/mm/transparent_hugepage/khugepaged/scan_sleep_millisecs"),
                   ("khugepaged pages_collapsed", "/sys/kernel/mm/transparent_hugepage/khugepaged/pages_collapsed"),
                   ("lru_gen min_ttl_ms", "/sys/kernel/mm/lru_gen/min_ttl_ms")] {
        println!("  {k:<28} {}", rd(p));
    }
    let mut mthp: Vec<String> = std::fs::read_dir("/sys/kernel/mm/transparent_hugepage").into_iter().flatten().flatten()
        .filter_map(|e| { let n = e.file_name().to_string_lossy().into_owned(); n.starts_with("hugepages-").then(|| {
            let st = rd(&format!("{}/enabled", e.path().display()));
            let sel = st.split_whitespace().find(|w| w.starts_with('[')).unwrap_or("-").trim_matches(|c| c == '[' || c == ']').to_owned();
            format!("{}={sel}", n.trim_start_matches("hugepages-"))
        }) }).collect();
    mthp.sort_by_key(|s| s.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().unwrap_or(0));
    println!("  mTHP                         {}", mthp.join(" "));
    for z in std::fs::read_dir("/sys/block").into_iter().flatten().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("zram")) {
        println!("  {} mm_stat                  {}", z.file_name().to_string_lossy(), rd(&format!("{}/mm_stat", z.path().display())));
    }
    println!("  write rate                   {}", lpm_helpers::iorate::gather().summary());
    println!("  cmdline                      {}", rd("/proc/cmdline"));
    for f in ["memory", "io", "cpu"] { println!("  psi {f:<7} {}", rd(&format!("/proc/pressure/{f}")).replace('\n', " | ")); }
    const KEYS: &[&str] = &["allocstall_normal", "allocstall_movable", "pgscan_direct", "pgscan_kswapd", "pgsteal_direct", "pgsteal_kswapd",
        "compact_stall", "compact_fail", "kswapd_low_wmark_hit_quickly", "workingset_refault_file", "workingset_refault_anon",
        "thp_fault_alloc", "thp_fault_fallback", "thp_collapse_alloc", "thp_split_page", "thp_deferred_split_page",
        "pswpin", "pswpout", "nr_dirty", "nr_writeback", "nr_dirtied", "nr_written", "oom_kill"];
    let a = kv(&rd("/proc/vmstat"), ' ');
    eprintln!("sampling counters for {secs} s ...");
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let b = kv(&rd("/proc/vmstat"), ' ');
    println!("\n== /proc/vmstat: since boot / delta over {secs} s");
    for k in KEYS {
        let (x, y) = (a.get(*k).and_then(Value::as_u64), b.get(*k).and_then(Value::as_u64));
        if let (Some(x), Some(y)) = (x, y) { println!("  {k:<30} {y:>14} {:>+12}", y as i64 - x as i64); }
    }
    let ctx = AuditCtx::live();
    let p = Profile::gather();
    let live: Map<String, Value> = p.current.iter().map(|(k, v)| (k.clone(), v.parse::<i64>().map(Value::from).unwrap_or_else(|_| json!(v)))).collect();
    let issues = autotune::audit(&live, &ctx);
    if !issues.is_empty() {
        println!("\n== live values outside the safe envelope");
        for i in issues { println!("  {} {}: {}", if i.reject { "REJECT" } else { "warn  " }, i.key, i.msg); }
    }
}
