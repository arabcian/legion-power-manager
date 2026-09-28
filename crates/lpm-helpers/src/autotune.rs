//! Autotune: profiles the machine and derives an Optimizations preset for one
//! of four goals (power saving, gaming, throughput, optimal desktop).
//!
//! Three stages, kept apart so the rules are testable without sysfs:
//!   1. [`Profile::gather`]  reads hardware / kernel facts (unprivileged).
//!   2. [`decide`]           pure rules: (goal, profile) -> key, value, reason.
//!   3. [`autotune`]         drops what this machine does not offer (tune::files
//!                           empty, value not accepted by tune::validate) and
//!                           returns a preset object the GUI / lpm-autotune load.
//!
//! Every value is derived from the profile - topology (CCDs, V-Cache, hybrid),
//! cpufreq driver and EPP support, C-state exit latencies, RAM size, swap kind,
//! storage type, battery, GPUs, kernel version - never a fixed table per model.
//! The reasoning per goal is documented in docs/AUTOTUNE.md.

use crate::tune;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Goal { PowerSave, Gaming, Throughput, Desktop }

impl Goal {
    pub const ALL: [Goal; 4] = [Goal::PowerSave, Goal::Gaming, Goal::Throughput, Goal::Desktop];
    pub fn parse(s: &str) -> Option<Goal> {
        match s.to_ascii_lowercase().replace(['-', '_', ' '], "").as_str() {
            "powersave" | "powersaving" | "power" | "battery" => Some(Goal::PowerSave),
            "gaming" | "game" | "latency" => Some(Goal::Gaming),
            "throughput" | "barethroughput" | "compute" => Some(Goal::Throughput),
            "desktop" | "optimaldesktop" | "balanced" => Some(Goal::Desktop),
            _ => None,
        }
    }
    pub fn key(self) -> &'static str {
        match self { Goal::PowerSave => "powersave", Goal::Gaming => "gaming", Goal::Throughput => "throughput", Goal::Desktop => "desktop" }
    }
    pub fn label(self) -> &'static str {
        match self {
            Goal::PowerSave => "Power saving",
            Goal::Gaming => "Gaming (latency + throughput)",
            Goal::Throughput => "Bare throughput",
            Goal::Desktop => "Optimal desktop",
        }
    }
    /// Preset name (valid for lpm-gamemode / the GUI: letters, digits, space _ - .).
    pub fn preset_name(self) -> &'static str {
        match self { Goal::PowerSave => "Auto Power saving", Goal::Gaming => "Auto Gaming",
                     Goal::Throughput => "Auto Throughput", Goal::Desktop => "Auto Desktop" }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwapKind { None, Zram, ZswapDisk, Ssd, Hdd }

impl SwapKind {
    fn as_str(self) -> &'static str {
        match self { SwapKind::None => "none", SwapKind::Zram => "zram", SwapKind::ZswapDisk => "disk + zswap",
                     SwapKind::Ssd => "disk (SSD/NVMe)", SwapKind::Hdd => "disk (HDD)" }
    }
}

#[derive(Clone, Debug)]
pub struct CState { pub name: String, pub latency_us: u64 }

#[derive(Clone, Debug)]
pub struct Profile {
    pub vendor: tune::Vendor,
    pub model: String,
    pub logical: usize,
    pub cores: usize,
    pub smt_active: bool,
    /// L3 domains of online CPUs (index, cpus, l3 KiB, max kHz).
    pub ccds: Vec<tune::Ccx>,
    pub cache_ccd: Option<usize>,
    pub freq_ccd: Option<usize>,
    pub x3d_driver: bool,
    pub hybrid: bool,
    pub driver: String,
    pub epp: bool,
    pub governors: Vec<String>,
    pub idle_governors: Vec<String>,
    pub cstates: Vec<CState>,
    pub ram_kb: u64,
    pub swap: SwapKind,
    pub nvme: bool,
    pub rotational: bool,
    pub sata_hosts: bool,
    pub battery: bool,
    pub on_ac: Option<bool>,
    pub nvidia_dgpu: bool,
    pub amd_igpu: bool,
    pub intel_igpu: bool,
    pub wifi: bool,
    pub kernel: (u32, u32),
    pub numa_nodes: usize,
    pub scx: Vec<String>,
    pub dynamic_epp: bool,
    pub uncore: Option<(u64, u64)>,
    /// Live values of the rows whose rule depends on the current state.
    pub current: BTreeMap<String, String>,
}

fn rd(p: impl AsRef<Path>) -> Option<String> { crate::read_trimmed(p.as_ref()).ok() }

fn kernel_release() -> (u32, u32) {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 { return (0, 0); }
    let rel = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) }.to_string_lossy().into_owned();
    parse_release(&rel)
}

pub fn parse_release(rel: &str) -> (u32, u32) {
    let mut it = rel.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).map(|x| x.parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// "/dev/nvme0n1p3" -> rotational flag of the whole disk (None if unknown).
fn dev_rotational(dev: &str) -> Option<bool> {
    let name = Path::new(dev).file_name()?.to_string_lossy().into_owned();
    let node = std::fs::canonicalize(Path::new("/sys/class/block").join(&name)).ok()?;
    let disk = if node.join("partition").is_file() { node.parent()?.to_path_buf() } else { node };
    rd(disk.join("queue/rotational")).map(|r| r == "1")
}

/// Swap devices from /proc/swaps; the most important one decides the kind.
fn swap_kind(zswap_on: bool) -> SwapKind {
    let text = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    let mut kinds = Vec::new();
    for l in text.lines().skip(1) {
        let mut f = l.split_whitespace();
        let (Some(name), Some(ty)) = (f.next(), f.next()) else { continue };
        if name.contains("/zram") { kinds.push(SwapKind::Zram); continue; }
        let rot = if ty == "partition" { dev_rotational(name) } else {
            // Swap file: judge by the disk holding the root file system.
            let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
            mounts.lines().find(|m| m.split_whitespace().nth(1) == Some("/"))
                .and_then(|m| m.split_whitespace().next().map(str::to_owned))
                .and_then(|d| dev_rotational(&d))
        };
        kinds.push(match (zswap_on, rot) { (true, _) => SwapKind::ZswapDisk, (_, Some(true)) => SwapKind::Hdd, _ => SwapKind::Ssd });
    }
    if kinds.contains(&SwapKind::Zram) { SwapKind::Zram } else { kinds.first().copied().unwrap_or(SwapKind::None) }
}

impl Profile {
    pub fn gather() -> Profile {
        let cpu = Path::new(tune::CPU_DIR);
        let online = tune::online_cpus();
        let mut cores = std::collections::BTreeSet::new();
        for c in &online {
            if let Some(s) = rd(cpu.join(format!("cpu{c}/topology/thread_siblings_list"))) { cores.insert(s); }
        }
        let ccds = tune::ccx_groups();
        let pol0 = cpu.join("cpufreq/policy0");
        let pol = tune::policies().into_iter().next().unwrap_or(pol0);
        let list = |p: &Path| rd(p).map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
        let cstates: Vec<CState> = tune::cstate_names().into_iter().enumerate().map(|(i, name)| CState {
            name,
            latency_us: rd(cpu.join(format!("cpu0/cpuidle/state{i}/latency"))).and_then(|s| s.parse().ok()).unwrap_or(0),
        }).collect();
        let ram_kb = std::fs::read_to_string("/proc/meminfo").unwrap_or_default().lines()
            .find_map(|l| l.strip_prefix("MemTotal:").and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok()))
            .unwrap_or(0);
        let zswap_on = matches!(rd("/sys/module/zswap/parameters/enabled").as_deref(), Some("Y") | Some("1"));
        let disks = tune::block_devs();
        let rot = |d: &Path| rd(d.join("queue/rotational")).as_deref() == Some("1");
        let mut battery = false;
        let mut on_ac = None;
        for e in std::fs::read_dir("/sys/class/power_supply").into_iter().flatten().flatten() {
            let d = e.path();
            match rd(d.join("type")).as_deref() {
                Some("Battery") if rd(d.join("scope")).as_deref() != Some("Device") => battery = true,
                Some("Mains") => on_ac = Some(rd(d.join("online")).as_deref() == Some("1")),
                _ => {}
            }
        }
        let (mut nvidia, mut amd_i, mut intel_i) = (false, false, false);
        for e in std::fs::read_dir("/sys/bus/pci/devices").into_iter().flatten().flatten() {
            let d = e.path();
            if !rd(d.join("class")).map_or(false, |c| c.starts_with("0x03")) { continue; }
            match rd(d.join("vendor")).as_deref() {
                Some("0x10de") => nvidia = true,
                Some("0x1002") => amd_i = true,
                Some("0x8086") => intel_i = true,
                _ => {}
            }
        }
        let wifi = std::fs::read_dir("/sys/class/net").into_iter().flatten().flatten()
            .any(|e| e.path().join("wireless").is_dir() || e.path().join("phy80211").exists());
        let numa_nodes = rd("/sys/devices/system/node/possible").map(|s| tune::cpu_list(&s).len()).unwrap_or(1).max(1);
        let scx = tune::find("sched.ext").map(|t| tune::options(t).into_iter().map(|o| o.0).filter(|n| n != "none").collect())
            .unwrap_or_default();
        let uncore = std::fs::read_dir(cpu.join("intel_uncore_frequency")).into_iter().flatten().flatten().next().and_then(|e| {
            let d = e.path();
            Some((rd(d.join("initial_min_freq_khz"))?.parse().ok()?, rd(d.join("initial_max_freq_khz"))?.parse().ok()?))
        });
        let mut current = BTreeMap::new();
        for k in ["vm.min_free_kbytes", "cpu.ccd_park", "kernel.sched_schedstats", "wq.affinity_scope",
                  "mm.lru_gen", "pm.nvme_latency_us", "mm.ksm_run", "cpu.smt"] {
            if let Some(v) = tune::find(k).and_then(tune::current) { current.insert(k.to_owned(), v); }
        }
        Profile {
            vendor: tune::cpu_vendor(),
            model: crate::cpuinfo_head().lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == "model name").map(|(_, v)| v.trim().to_owned()))
                .unwrap_or_default(),
            logical: online.len(),
            cores: cores.len().max(1),
            smt_active: rd(cpu.join("smt/active")).as_deref() == Some("1"),
            cache_ccd: tune::resolve_ccd(&ccds, "cache").map(|g| g.index),
            freq_ccd: tune::resolve_ccd(&ccds, "frequency").map(|g| g.index),
            ccds,
            x3d_driver: tune::x3d_mode_path().is_some(),
            hybrid: tune::hybrid().is_some(),
            driver: rd(pol.join("scaling_driver")).unwrap_or_default(),
            epp: pol.join("energy_performance_preference").is_file(),
            governors: list(&pol.join("scaling_available_governors")),
            idle_governors: list(&cpu.join("cpuidle/available_governors")),
            cstates,
            ram_kb,
            swap: swap_kind(zswap_on),
            nvme: disks.iter().any(|d| d.file_name().map_or(false, |n| n.to_string_lossy().starts_with("nvme"))),
            rotational: disks.iter().any(|d| rot(d)),
            sata_hosts: std::fs::read_dir("/sys/class/scsi_host").into_iter().flatten().flatten()
                .any(|e| e.path().join("link_power_management_policy").is_file()),
            battery,
            on_ac,
            nvidia_dgpu: nvidia,
            amd_igpu: amd_i,
            intel_igpu: intel_i,
            wifi,
            kernel: kernel_release(),
            numa_nodes,
            scx,
            dynamic_epp: cpu.join("amd_pstate/dynamic_epp").is_file(),
            uncore,
            current,
        }
    }

    pub fn ram_gb(&self) -> u64 { (self.ram_kb + 512 * 1024) / (1024 * 1024) }
    fn amd(&self) -> bool { self.vendor == tune::Vendor::Amd }
    fn intel(&self) -> bool { self.vendor == tune::Vendor::Intel }
    fn multi_ccd(&self) -> bool { self.ccds.len() > 1 }
    /// Asymmetric two-die X3D part (V-Cache die and frequency die both resolvable).
    fn x3d(&self) -> bool { self.multi_ccd() && self.cache_ccd.is_some() && self.freq_ccd.is_some() && self.cache_ccd != self.freq_ccd }
    fn kernel_at_least(&self, maj: u32, min: u32) -> bool { self.kernel >= (maj, min) }
    /// Frequency is chosen by the scheduler (schedutil), not by CPPC/HWP.
    fn schedutil(&self) -> bool { !self.epp && self.governors.iter().any(|g| g == "schedutil") }
    fn cur(&self, k: &str) -> Option<&str> { self.current.get(k).map(String::as_str) }

    pub fn to_json(&self) -> Value {
        json!({
            "vendor": self.vendor.as_str(), "model": self.model, "logical_cpus": self.logical, "cores": self.cores,
            "smt": self.smt_active,
            "ccds": self.ccds.iter().map(|c| json!({"index": c.index, "cpus": tune::fmt_cpu_list(&c.cpus),
                                                     "l3_mb": c.l3_kib / 1024, "max_mhz": c.max_khz / 1000})).collect::<Vec<_>>(),
            "cache_ccd": self.cache_ccd, "frequency_ccd": self.freq_ccd, "x3d": self.x3d(), "hybrid": self.hybrid,
            "cpufreq_driver": self.driver, "epp": self.epp, "governors": self.governors,
            "idle_governors": self.idle_governors,
            "cstates": self.cstates.iter().map(|c| json!({"name": c.name, "latency_us": c.latency_us})).collect::<Vec<_>>(),
            "ram_gb": self.ram_gb(), "swap": self.swap.as_str(), "nvme": self.nvme, "rotational_disk": self.rotational,
            "sata": self.sata_hosts, "battery": self.battery, "on_ac": self.on_ac,
            "gpu": {"nvidia": self.nvidia_dgpu, "amd": self.amd_igpu, "intel": self.intel_igpu},
            "wifi": self.wifi, "kernel": format!("{}.{}", self.kernel.0, self.kernel.1), "numa_nodes": self.numa_nodes,
            "sched_ext": self.scx, "dynamic_epp": self.dynamic_epp,
        })
    }

    /// One line for the GUI / CLI header.
    pub fn summary(&self) -> String {
        let mut p = vec![if self.model.is_empty() { self.vendor.as_str().to_uppercase() } else { self.model.clone() }];
        p.push(format!("{}C/{}T", self.cores, self.logical));
        if self.x3d() {
            p.push(format!("{} CCDs, V-Cache on CCD{}", self.ccds.len(), self.cache_ccd.unwrap_or(0)));
        } else if self.multi_ccd() {
            p.push(format!("{} CCDs", self.ccds.len()));
        }
        if self.hybrid { p.push("hybrid P/E".into()); }
        p.push(format!("{} ({})", if self.driver.is_empty() { "no cpufreq" } else { &self.driver }, if self.epp { "EPP" } else { "no EPP" }));
        p.push(format!("{} GB RAM", self.ram_gb()));
        p.push(format!("swap: {}", self.swap.as_str()));
        p.push(if self.nvme && !self.rotational { "NVMe".into() } else if self.rotational { "HDD present".into() } else { "SSD".into() });
        let mut g = Vec::new();
        if self.nvidia_dgpu { g.push("NVIDIA"); }
        if self.amd_igpu { g.push("AMD"); }
        if self.intel_igpu { g.push("Intel"); }
        if !g.is_empty() { p.push(format!("GPU: {}", g.join(" + "))); }
        p.push(if self.battery { match self.on_ac { Some(true) => "laptop (on AC)", Some(false) => "laptop (on battery)", None => "laptop" } }
               else { "desktop (no battery)" }.into());
        p.push(format!("kernel {}.{}", self.kernel.0, self.kernel.1));
        p.join(" · ")
    }
}

/// One rule outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision { pub key: &'static str, pub value: Value, pub why: String }

struct Rules<'a> { p: &'a Profile, g: Goal, out: Vec<Decision> }

impl<'a> Rules<'a> {
    fn set(&mut self, key: &'static str, value: impl Into<Value>, why: impl Into<String>) {
        self.out.retain(|d| d.key != key);
        self.out.push(Decision { key, value: value.into(), why: why.into() });
    }
    fn is(&self, g: Goal) -> bool { self.g == g }
}

/// Pure rule set. Keys that do not exist on this machine are filtered later.
pub fn decide(goal: Goal, p: &Profile) -> Vec<Decision> {
    let mut r = Rules { p, g: goal, out: Vec::new() };
    cpu_rules(&mut r);
    memory_rules(&mut r);
    sched_rules(&mut r);
    io_rules(&mut r);
    device_rules(&mut r);
    r.out
}

fn cpu_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    // Driver mode: EPP/CPPC (active) is the only mode where the per-core
    // hardware controller reacts within microseconds; every goal wants it.
    if p.amd() { r.set("cpu.pstate_status", "active", "amd-pstate active (EPP): CPPC firmware picks the clock per core in µs; every goal below is expressed through EPP."); }
    if p.intel() { r.set("cpu.intel_pstate_status", "active", "intel_pstate active (HWP): the core's own P-state logic, steered by EPP."); }

    // Dynamic EPP (kernel switches EPP with the power source) - only the
    // desktop goal hands EPP to the kernel; the others set it explicitly.
    if p.dynamic_epp {
        if r.is(Desktop) && p.battery {
            r.set("cpu.dynamic_epp", "enabled", "Laptop desktop use: let amd-pstate follow AC/battery on its own; EPP rows are left to it.");
        } else {
            r.set("cpu.dynamic_epp", "disabled", "This goal sets EPP explicitly; with dynamic EPP on the kernel would refuse or override it.");
        }
    }
    let desktop_dyn = r.is(Desktop) && p.battery && p.dynamic_epp;

    // EPP per goal. Under a laptop's package power limit, sustained all-core
    // throughput is power-bound, so balance_performance (better perf/W)
    // matches or beats performance there; without a battery it is not.
    let t_epp = if p.battery { "balance_performance" } else { "performance" };
    let (epp, why): (&str, String) = match r.g {
        PowerSave => ("power", "Most efficient EPP: lowest sustained clocks and ramp-up, biggest idle/light-load saving.".into()),
        Gaming => ("performance", "Game threads get the most aggressive boost and the fastest ramp after a stall.".into()),
        Throughput => (t_epp, if p.battery {
            "Laptop: sustained all-core load is package-power-bound, so the efficient boost curve of balance_performance gives the same or more work per second at lower heat.".into()
        } else { "No battery (no tight package limit): maximum boost for throughput.".into() }),
        Desktop => ("balance_performance", "Snappy boost for interactive bursts with much lower idle/light-load power than performance.".into()),
    };
    if p.epp && !desktop_dyn {
        if p.multi_ccd() {
            for ccd in 0..p.ccds.len().min(2) {
                let (gk, ek) = if ccd == 0 { ("cpu.governor_ccd0", "cpu.epp_ccd0") } else { ("cpu.governor_ccd1", "cpu.epp_ccd1") };
                r.set(gk, "powersave", "Governor 'powersave' hands the decision to EPP (performance would pin EPP to 0 and make the EPP row moot).");
                let (v, w) = if r.is(Gaming) && p.x3d() && Some(ccd) == p.freq_ccd {
                    ("balance_power", format!("CCD{ccd} is the frequency die: during a game it only runs IRQs, kernel work and background tasks, so it gets an efficient EPP and leaves package power to the V-Cache die."))
                } else if r.is(Gaming) && p.x3d() && Some(ccd) == p.cache_ccd {
                    ("performance", format!("CCD{ccd} carries the 3D V-Cache and hosts the game (see launch affinity): most aggressive boost."))
                } else { (epp, why.clone()) };
                r.set(ek, v, w);
            }
        } else {
            r.set("cpu.governor", "powersave", "Governor 'powersave' hands the decision to EPP.");
            r.set("cpu.epp", epp, why.clone());
        }
        if p.hybrid {
            let (pc, ec, w) = match r.g {
                PowerSave => ("balance_power", "power", "P-cores stay usable for bursts, E-cores run at the most efficient point."),
                Gaming => ("performance", "balance_power", "Game threads on the P-cores boost hardest; E-cores (background, IRQs) stay efficient and leave package power to the P-cores and the dGPU."),
                Throughput => (t_epp, t_epp, "Every core contributes to a parallel build; same EPP on both classes."),
                Desktop => ("balance_performance", "balance_performance", "Uniform, responsive EPP on both core classes."),
            };
            r.set("cpu.epp_pcore", pc, w);
            r.set("cpu.epp_ecore", ec, w);
        }
    } else if !p.epp && !p.governors.is_empty() {
        // Legacy cpufreq driver: pick a governor.
        let gov = match r.g {
            Throughput if !p.battery => "performance",
            _ if p.governors.iter().any(|g| g == "schedutil") => "schedutil",
            _ => "ondemand",
        };
        if p.governors.iter().any(|g| g == gov) {
            let key = if p.multi_ccd() { None } else { Some("cpu.governor") };
            if let Some(k) = key { r.set(k, gov, "No EPP on this driver: the governor decides the clock; schedutil follows scheduler load directly."); }
            else {
                r.set("cpu.governor_ccd0", gov, "No EPP on this driver: the governor decides the clock.");
                r.set("cpu.governor_ccd1", gov, "No EPP on this driver: the governor decides the clock.");
            }
        }
    }

    // Turbo: the single biggest power lever; everything but power saving wants it.
    match r.g {
        PowerSave => r.set("cpu.boost", "0", "Turbo off removes the least efficient (highest-voltage) bins - the largest single power/heat saving; base clock remains."),
        _ => r.set("cpu.boost", "1", "Turbo on: full single- and multi-thread clock range."),
    }

    // Idle floor: below amd_pstate_lowest_nonlinear_freq the CPU is less
    // efficient per unit of work; power saving still wants the hardware minimum
    // for trickle loads, where absolute power matters more than efficiency.
    r.set("cpu.min_freq", if r.is(PowerSave) { "cpuinfo_min" } else { "lowest_nonlinear" },
          if r.is(PowerSave) { "Hardware minimum floor: lowest absolute power for trickle loads." }
          else { "Efficient floor: frequencies below lowest_nonlinear cost wake-up latency for almost no saving." });

    if p.intel() {
        match r.g {
            PowerSave => { r.set("cpu.hwp_dynamic_boost", "0", "No I/O-wait boost: fewer short clock spikes."); r.set("cpu.epb", 12, "EPB towards power saving (uncore/package decisions)."); }
            Gaming => { r.set("cpu.hwp_dynamic_boost", "1", "Threads waking from I/O (asset streaming, shader compiles) start at a high clock."); r.set("cpu.epb", 4, "EPB balance-performance."); }
            Throughput => r.set("cpu.epb", 4, "EPB balance-performance."),
            Desktop => { r.set("cpu.hwp_dynamic_boost", "1", "Faster ramp after I/O waits in interactive apps."); r.set("cpu.epb", 6, "EPB normal (stock)."); }
        }
        if let Some((lo, hi)) = p.uncore {
            match r.g {
                Gaming => r.set("cpu.uncore_min_khz", hi as i64, "Uncore floor raised to its maximum: no ring/L3/memory ramp-up after idle - steadier frame times."),
                PowerSave => r.set("cpu.uncore_max_khz", (lo + (hi - lo) * 2 / 5) as i64, "Uncore capped at 40% of its range: several watts less package power; costs memory latency, irrelevant on battery."),
                _ => {}
            }
        }
    }

    // X3D scheduler preference.
    if p.x3d() && p.x3d_driver {
        match r.g {
            Gaming => r.set("cpu.x3d_mode", "cache", "Games have large working sets: new threads prefer the V-Cache die."),
            Throughput => r.set("cpu.x3d_mode", "frequency", "Parallel compute (builds, encoders) benefits more from the higher-clocked die first."),
            _ => {}
        }
    }

    // cpuidle: teo matches modern short C-state tables better than menu.
    if p.idle_governors.iter().any(|g| g == "teo") && !r.is(Throughput) {
        r.set("cpu.idle_governor", "teo", "teo (timer events oriented) picks the right C-state more often on CPUs with few states: fewer too-deep entries under light load, fewer too-shallow ones at idle.");
    }
    r.set("cpu.cstate_max", "all", "All C-states enabled: sleeping cores give their share of the power budget to the busy ones (boost headroom).");

    // Wake latency. On a laptop deep idle *is* boost headroom, so no cap; on a
    // desktop gaming box the deepest state's exit latency can be traded away.
    let deep = p.cstates.iter().filter(|c| c.name != "POLL").map(|c| c.latency_us).max().unwrap_or(0);
    let second = p.cstates.iter().filter(|c| c.name != "POLL" && c.latency_us < deep).map(|c| c.latency_us).max();
    match (r.g, second) {
        (Gaming, Some(s)) if !p.battery && deep >= 50 => r.set("cpu.wake_latency_us", s as i64,
            format!("Desktop (no battery): cores may use every C-state that wakes within {s} µs, the deepest ({deep} µs) is skipped - lower worst-case wake jitter.")),
        _ => r.set("cpu.wake_latency_us", 0, if p.battery && r.is(Gaming) {
            "Laptop: no wake-latency cap - capping deep idle would steal boost headroom from the game's cores and add heat.".to_string()
        } else { "No wake-latency constraint (kernel default).".to_string() }),
    }

    // Hot-plug: autotune never parks a CCD; it brings a parked one back.
    if p.cur("cpu.ccd_park").map_or(false, |v| v != "none") {
        r.set("cpu.ccd_park", "none", "A CCD is parked: all CPUs back online (autotune isolates with affinity/steering instead; parking breaks Wine/Proton CPU numbering and nvidia-powerd).");
    }
    if r.is(Throughput) && p.cur("cpu.smt").map_or(false, |v| v == "off") {
        r.set("cpu.smt", "on", "SMT on: +20-30% throughput for parallel builds.");
    }
}

fn memory_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    let gb = p.ram_gb();
    // THP. CachyOS ships enabled=always + defrag=defer+madvise + khugepaged
    // max_ptes_none=409: tcmalloc users (Proton games, Chromium) get huge pages,
    // and on 6.12+ the THP shrinker splits huge pages with more than
    // max_ptes_none zero-filled subpages, which removes the classic RSS bloat of
    // "always". Without the shrinker (older kernel) or with little RAM, madvise.
    let always = gb >= 16 && p.kernel_at_least(6, 12) && !r.is(PowerSave);
    if always {
        r.set("thp.enabled", "always", "THP everywhere (CachyOS default): tcmalloc-based apps such as Proton games and Chromium get huge pages; the 6.12+ THP shrinker keeps RSS in check.");
        r.set("thp.defrag", "defer+madvise", "Page faults never stall on compaction except in madvise regions; kswapd/kcompactd defragment in the background (CachyOS default).");
        r.set("thp.khp_max_ptes_none", 409, "A huge page with more than 409 of 512 subpages unused is split back (THP shrinker, 6.12+): the memory cost of 'always' stays small (CachyOS value).");
    } else {
        r.set("thp.enabled", "madvise", if r.is(PowerSave) { "Huge pages only on request: no background collapse work on battery." }
              else { "Huge pages only on request: kernel < 6.12 (no THP shrinker) or < 16 GB RAM, where 'always' can bloat memory." });
        r.set("thp.defrag", "defer+madvise", "Never stall a page fault for compaction outside madvise regions.");
    }
    if r.is(PowerSave) {
        r.set("thp.khugepaged_defrag", 0, "khugepaged does not compact to build huge pages.");
        r.set("thp.khp_scan_sleep_ms", 60_000, "khugepaged wakes once a minute instead of every 10 s.");
    }

    // MGLRU: min_ttl_ms protects the recently used working set from eviction
    // (kernel docs' thrashing-prevention example is 1000 ms).
    if p.cur("mm.lru_gen").map_or(true, |v| v != "7") { r.set("mm.lru_gen", 7, "All MGLRU features (kernel default)."); }
    if matches!(r.g, Gaming | Desktop) {
        r.set("mm.lru_gen_min_ttl", 1000, "Working set of the last second is never evicted: no thrashing stutter; under real pressure the OOM killer ends a process instead.");
    }
    if !r.is(Desktop) && p.cur("mm.ksm_run").map_or(true, |v| v != "0") { r.set("mm.ksm_run", 0, "Stop the KSM scanner: pure background cost without VMs."); }
    if !r.is(PowerSave) {
        r.set("vm.max_map_count", 2_147_483_642_i64, "SteamOS/Proton value: some games exceed the old 65530 mapping limit.");
    }

    // Swap (CachyOS: swappiness 100 = equal I/O cost; 150 and zswap off with
    // zram; page-cluster 0 for zram, 1 for SSD, 2 for HDD).
    match p.swap {
        SwapKind::Zram => {
            r.set("vm.swappiness", if r.is(PowerSave) { 180 } else { 150 }, "zram swap costs a compression, not I/O: cold anonymous pages go there before page cache (CachyOS 150).");
            r.set("vm.page_cluster", 0, "zram has no seek cost: no swap readahead.");
            r.set("zswap.enabled", "0", "zram is the swap device: zswap in front of it would compress twice.");
        }
        SwapKind::None => {}
        k => {
            let hdd = k == SwapKind::Hdd;
            r.set("vm.swappiness", if hdd { 60 } else { 100 }, if hdd { "Swap on a spinning disk: keep swap-outs rarer than cache drops." }
                  else { "zswap absorbs swap-outs in RAM: equal cost for anon and file pages (CachyOS 100)." });
            r.set("vm.page_cluster", if hdd { 2 } else { 1 }, "Small swap readahead for physical swap (CachyOS: 1 on SSD, 2 on HDD).");
            r.set("zswap.enabled", "1", "Disk swap present: zswap keeps most swapped pages compressed in RAM.");
            r.set("zswap.compressor", if r.is(Gaming) { "lz4" } else { "zstd" },
                  if r.is(Gaming) { "lz4: fastest decompression when a swapped page is touched again." } else { "zstd: best ratio, more pages stay in RAM." });
            r.set("zswap.shrinker_enabled", "1", "Cold pool pages move on to disk proactively.");
        }
    }

    // Reclaim: watermark_scale_factor (default 10 = 0.1% of RAM) sets how early
    // kswapd starts; a larger gap means fewer direct-reclaim stalls.
    if gb >= 16 && matches!(r.g, Gaming | Desktop | Throughput) {
        r.set("vm.watermark_scale_factor", 125, "kswapd wakes at ~1.25% free instead of 0.1%: allocations rarely hit direct reclaim (a stall on the allocating thread).");
    }
    if r.is(PowerSave) {
        r.set("vm.compaction_proactiveness", 0, "No proactive compaction: THP is madvise-only here, no background CPU work.");
        r.set("vm.stat_interval", 10, "vmstat refresh every 10 s: fewer periodic timer wakeups.");
    }

    // Dirty page cache in bytes (1% of 32 GB is already 320 MB).
    let ram_b = p.ram_kb * 1024;
    let (bg, full, w): (u64, u64, &str) = match r.g {
        Gaming | Desktop => (64 << 20, 256 << 20, "CachyOS values: writeback starts at 64 MB, writers throttle at 256 MB, so a download or copy never piles up gigabytes that flush at once and stall the compositor or asset streaming."),
        Throughput => (ram_b / 10, (ram_b * 4 / 10).min(16 << 30), "TuneD throughput-performance: 10% / 40% of RAM (capped at 16 GB) absorb write bursts at RAM speed."),
        PowerSave => (ram_b / 10, ram_b / 5, "Larger buffers so writes are batched and the drive idles longer."),
    };
    r.set("vm.dirty_background_bytes", bg as i64, w);
    r.set("vm.dirty_bytes", full.max(bg * 2) as i64, w);
    match r.g {
        PowerSave => {
            r.set("vm.dirty_writeback_centisecs", 1500, "Flusher wakes every 15 s instead of 5 s (TLP): fewer drive wake-ups.");
            r.set("vm.dirty_expire_centisecs", 6000, "Data may stay dirty up to 60 s: fewer, larger write bursts.");
        }
        Gaming | Desktop => r.set("vm.dirty_writeback_centisecs", 1500, "Flusher wakes every 15 s (CachyOS): fewer periodic writeback bursts; the byte limits above bound the backlog."),
        Throughput => {}
    }
    if matches!(r.g, Gaming | Desktop) {
        r.set("vm.vfs_cache_pressure", 50, "Directory/inode cache kept longer (CachyOS): shader caches, library scans and file dialogs stay fast.");
    }
}

fn sched_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    if matches!(r.g, Gaming | Desktop | Throughput) {
        r.set("kernel.split_lock_mitigate", 0, "No 1000x throttle for split-lock accesses (some Windows games and emulators trigger it).");
    }
    match r.g {
        Gaming => r.set("kernel.watchdog", 0, "No lockup-detector timer/NMI interrupts (note: the Health tab then sees no soft/hard lockup reports)."),
        PowerSave => r.set("kernel.watchdog", 0, "No periodic watchdog/NMI interrupts (TLP disables the NMI watchdog for the same reason)."),
        _ => {}
    }
    if p.numa_nodes <= 1 { r.set("kernel.numa_balancing", 0, "Single NUMA node: balancing would only sample page faults for nothing."); }
    if matches!(r.g, Gaming | Desktop) { r.set("kernel.sched_autogroup", 1, "Per-session scheduling groups: a background build cannot starve the desktop/game."); }
    if p.cur("kernel.sched_schedstats") == Some("1") && r.g != Desktop {
        r.set("kernel.sched_schedstats", "0", "Schedstats were left on by some tool: per-switch accounting cost removed.");
    }
    r.set("sched.itmt", "1", "Preferred cores first: light loads run on the best-binned cores.");

    // Preemption and EEVDF slice. lazy (6.13+) = full's latency, voluntary's throughput.
    let lazy = p.kernel_at_least(6, 13);
    let (pre, pw) = match r.g {
        Gaming => ("full", "Full preemption: a woken game/audio/input thread runs almost immediately."),
        Desktop => (if lazy { "lazy" } else { "full" }, "Lazy preemption keeps full's wake-up latency for RT/interactive work with fewer forced switches."),
        Throughput | PowerSave => (if lazy { "lazy" } else { "voluntary" }, "Fewer involuntary context switches: more work per slice."),
    };
    r.set("sched.preempt", pre, pw);
    // EEVDF base slice stays at the kernel default: EEVDF already gives
    // latency-sensitive tasks earlier deadlines, and no measured source backs
    // a fixed shorter/longer global slice for these goals.
    if r.is(Throughput) {
        r.set("sched.migration_cost_ns", 5_000_000, "Tasks count as cache-hot for 5 ms (TuneD throughput value): fewer cache-destroying migrations.");
    }
    match r.g {
        PowerSave => r.set("wq.power_efficient", "1", "Power-efficient workqueues may run on already-awake CPUs."),
        Gaming | Throughput => r.set("wq.power_efficient", "0", "Kernel work stays on the queuing CPU (latency/locality)."),
        Desktop => {}
    }
    if p.cur("wq.affinity_scope").map_or(false, |v| v != "cache") {
        r.set("wq.affinity_scope", "cache", "Unbound kernel work stays inside the L3 domain that queued it (kernel default).");
    }
    // Steering: keep IRQs and unbound kernel work off the game's cores.
    if r.is(Gaming) {
        if p.x3d() {
            let f = format!("ccd{}", p.freq_ccd.unwrap_or(1));
            r.set("wq.cpumask", f.clone(), "Unbound kernel work (writeback, crypto, fs) runs on the frequency die, not beside the game.");
            r.set("irq.affinity", f, "Device interrupts land on the frequency die, not on the game's cores.");
        } else if p.hybrid {
            r.set("wq.cpumask", "ecore", "Unbound kernel work on the E-cores, P-cores stay free for the game.");
            r.set("irq.affinity", "ecore", "Device interrupts on the E-cores.");
        }
    } else if r.is(Throughput) && (p.multi_ccd() || p.hybrid) {
        r.set("wq.cpumask", "all", "Every CPU may run kernel work.");
        r.set("irq.affinity", "all", "Interrupts spread over every CPU.");
    }
    // sched_ext is not chosen automatically: scx_lavd is still in development
    // and has open reports of large fps regressions on some CPUs, so it stays a
    // per-game, user-tested choice (Scheduler tab).
    // RT boost only matters where schedutil sets the clock.
    if p.schedutil() {
        match r.g {
            PowerSave => r.set("kernel.sched_util_clamp_min_rt_default", 0, "RT threads (audio, IRQ threads) no longer force the maximum frequency."),
            Desktop => r.set("kernel.sched_util_clamp_min_rt_default", 256, "RT threads get a moderate boost instead of max clock on every wake-up."),
            _ => r.set("kernel.sched_util_clamp_min_rt_default", 1024, "RT threads run at max clock (stock)."),
        }
    }
    match r.g {
        PowerSave => r.set("kernel.sched_energy_aware", "1", "Energy-model placement (only where EAS can run)."),
        Gaming | Throughput => r.set("kernel.sched_energy_aware", "0", "Classic load balancing: spread for performance instead of packing on efficient cores."),
        Desktop => {}
    }
}

fn io_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    // One scheduler for every disk (the row writes all of them).
    if p.rotational {
        r.set("blk.scheduler", if matches!(r.g, Gaming | Desktop) { "bfq" } else { "mq-deadline" },
              if matches!(r.g, Gaming | Desktop) { "A spinning disk is present: bfq keeps interactive reads responsive under competing I/O." }
              else { "A spinning disk is present: mq-deadline bounds latency while merging for throughput." });
    } else {
        r.set("blk.scheduler", "none", "Flash only: no reordering, lowest per-request latency and CPU cost.");
    }
    match r.g {
        Throughput => r.set("blk.read_ahead_kb", if p.rotational { 4096 } else { 1024 }, "Large read-ahead for sequential throughput (TuneD uses 4096 KiB)."),
        Gaming => r.set("blk.read_ahead_kb", 512, "Games stream assets sequentially from large packs: 512 KiB read-ahead."),
        _ => {}
    }
    // Network (latency under load): BBR + fq keeps queues short.
    if matches!(r.g, Gaming | Desktop | Throughput) {
        r.set("net.tcp_congestion", "bbr", "BBR models bandwidth/RTT instead of reacting to loss: steadier latency on Wi-Fi and long routes.");
        r.set("net.default_qdisc", "fq", "fq pacing, the qdisc BBR was designed for.");
    }
    if p.wifi {
        match r.g {
            Gaming => r.set("net.wifi_power_save", "0", "Radio never dozes between beacons: no 802.11 power-save ping spikes."),
            PowerSave => r.set("net.wifi_power_save", "1", "802.11 power save on."),
            _ => {}
        }
    }
}

fn device_rules(r: &mut Rules) {
    use Goal::*;
    let p = r.p;
    match r.g {
        Gaming => r.set("pci.aspm", "performance", "PCIe links never drop to a power state between bursts: no wake-up jitter on GPU/NVMe/Wi-Fi."),
        PowerSave => r.set("pci.aspm", "powersave", "Links may enter L0s/L1 when idle (powersupersave is avoided: it breaks some devices)."),
        _ => {}
    }
    match r.g {
        Gaming => { r.set("snd.hda_power_save", 0, "Codec always powered: no pop and no wake delay."); r.set("snd.hda_power_save_controller", "0", "Controller always powered."); }
        PowerSave => { r.set("snd.hda_power_save", 1, "Codec powers down after 1 s idle."); r.set("snd.hda_power_save_controller", "1", "Controller may power down too."); }
        _ => {}
    }
    match r.g {
        Gaming => r.set("usb.autosuspend", -1, "Newly plugged peripherals never autosuspend."),
        PowerSave => r.set("usb.autosuspend", 2, "Newly plugged devices suspend after 2 s idle."),
        _ => {}
    }
    match r.g {
        Gaming => r.set("pm.pci_runtime", "on", "Devices stay in D0 during play (GPU and its bridges excluded - dGPU runtime PM is untouched)."),
        PowerSave => r.set("pm.pci_runtime", "auto", "Idle PCI devices drop to D3 (TLP battery setting)."),
        _ => {}
    }
    match r.g {
        Gaming => r.set("pm.usb_runtime", "on", "Connected USB devices never suspend (HID/audio skipped anyway)."),
        PowerSave => r.set("pm.usb_runtime", "auto", "Idle webcam/Bluetooth/readers suspend."),
        _ => {}
    }
    if p.sata_hosts {
        r.set("pm.sata_alpm", match r.g { Gaming | Throughput => "max_performance", PowerSave | Desktop => "med_power_with_dipm" },
              match r.g { Gaming | Throughput => "SATA link always active.", _ => "Modern default: partial/slumber with device-initiated PM." });
    }
    if p.nvme {
        match r.g {
            Gaming => r.set("pm.nvme_latency_us", 0, "APST off while playing: an idle NVMe never needs to wake from a deep state mid-stream (~0.5-1 W more at idle)."),
            PowerSave if p.cur("pm.nvme_latency_us").map_or(true, |v| v != "100000") =>
                r.set("pm.nvme_latency_us", 100_000, "Every APST state allowed: deepest NVMe idle."),
            _ => {}
        }
    }
    // iGPU: when a dGPU renders, the iGPU only composites; pinning it low
    // frees shared SoC power for the CPU.
    if p.amd() && p.amd_igpu {
        let low = p.nvidia_dgpu && matches!(r.g, Gaming | Throughput);
        r.set("gpu.amdgpu_dpm", if low { "low" } else { "auto" },
              if low { "dGPU renders: iGPU pinned low, its share of the SoC power budget goes to the CPU cores." } else { "Driver-managed iGPU clocks." });
    }
    if p.intel() && p.intel_igpu && (r.is(PowerSave) || (r.is(Gaming) && p.nvidia_dgpu)) {
        r.set("gpu.intel_slpc_profile", "power_saving", "iGPU clocks ramp gently: it only composites or idles here.");
    }
}

fn run_block(goal: Goal, p: &Profile) -> (Value, Option<String>) {
    match goal {
        Goal::Gaming if p.x3d() => {
            let c = p.cache_ccd.unwrap_or(0);
            (json!({"nice": -5, "autogroup": true, "affinity": format!("ccd{c}")}),
             Some(format!("Game pinned to CCD{c} (V-Cache), nice -5 for the game's autogroup.")))
        }
        Goal::Gaming => (json!({"nice": -5, "autogroup": true, "affinity": "none"}), Some("nice -5 for the game's autogroup; the scheduler places threads.".into())),
        _ => (json!({"nice": 0, "autogroup": true, "affinity": "none"}), None),
    }
}

/// Full autotune: profile, rules, availability filter. Unprivileged.
pub fn autotune(goal: Goal) -> Value { autotune_with(goal, &Profile::gather()) }

pub fn autotune_with(goal: Goal, p: &Profile) -> Value {
    let mut values = Map::new();
    let mut why = Map::new();
    let mut skipped = Vec::new();
    for d in decide(goal, p) {
        let Some(t) = tune::find(d.key) else { continue };
        if !tune::vendor_ok(t) { continue; }
        if !t.debugfs && tune::files(t).is_empty() {
            skipped.push(json!({"key": d.key, "why": "not available on this machine/kernel"}));
            continue;
        }
        match tune::validate(t, &d.value) {
            Ok(v) => {
                let jv = match t.kind { tune::Kind::Int { .. } => v.parse::<i64>().map(Value::from).unwrap_or(Value::String(v)), _ => Value::String(v) };
                values.insert(d.key.to_owned(), jv);
                why.insert(d.key.to_owned(), Value::String(d.why));
            }
            Err(e) => skipped.push(json!({"key": d.key, "why": e})),
        }
    }
    let (run, run_why) = run_block(goal, p);
    if let Some(w) = run_why { why.insert("run".into(), Value::String(w)); }
    let summary = format!("Autotuned for {} on {}.", goal.label().to_lowercase(), p.summary());
    json!({
        "ok": true, "goal": goal.key(), "goal_label": goal.label(), "name": goal.preset_name(),
        "profile": p.to_json(), "profile_summary": p.summary(),
        "preset": {"values": values, "run": run, "summary": summary,
                   "autotune": {"goal": goal.key(), "kernel": format!("{}.{}", p.kernel.0, p.kernel.1)}},
        "rationale": why, "skipped": skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legion() -> Profile {
        Profile {
            vendor: tune::Vendor::Amd, model: "AMD Ryzen 9 9955HX3D 16-Core Processor".into(), logical: 32, cores: 16, smt_active: true,
            ccds: vec![
                tune::Ccx { index: 0, cpus: (0..8).chain(16..24).collect(), l3_kib: 98304, max_khz: 5_200_000 },
                tune::Ccx { index: 1, cpus: (8..16).chain(24..32).collect(), l3_kib: 32768, max_khz: 5_450_000 },
            ],
            cache_ccd: Some(0), freq_ccd: Some(1), x3d_driver: true, hybrid: false,
            driver: "amd-pstate-epp".into(), epp: true, governors: vec!["performance".into(), "powersave".into()],
            idle_governors: vec!["menu".into(), "teo".into()],
            cstates: vec![CState { name: "POLL".into(), latency_us: 0 }, CState { name: "C1".into(), latency_us: 1 },
                          CState { name: "C2".into(), latency_us: 18 }, CState { name: "C3".into(), latency_us: 350 }],
            ram_kb: 32 * 1024 * 1024, swap: SwapKind::Zram, nvme: true, rotational: false, sata_hosts: false,
            battery: true, on_ac: Some(true), nvidia_dgpu: true, amd_igpu: true, intel_igpu: false, wifi: true,
            kernel: (7, 0), numa_nodes: 1, scx: vec!["lavd".into(), "bpfland".into()], dynamic_epp: true, uncore: None,
            current: BTreeMap::from([("vm.min_free_kbytes".into(), "67584".into()), ("cpu.ccd_park".into(), "none".into())]),
        }
    }
    fn get<'a>(d: &'a [Decision], k: &str) -> Option<&'a Value> { d.iter().find(|x| x.key == k).map(|x| &x.value) }

    #[test]
    fn gaming_x3d() {
        let p = legion();
        let d = decide(Goal::Gaming, &p);
        assert_eq!(get(&d, "cpu.epp_ccd0"), Some(&json!("performance")));
        assert_eq!(get(&d, "cpu.epp_ccd1"), Some(&json!("balance_power")));
        assert_eq!(get(&d, "irq.affinity"), Some(&json!("ccd1")));
        assert_eq!(get(&d, "cpu.x3d_mode"), Some(&json!("cache")));
        assert_eq!(get(&d, "cpu.dynamic_epp"), Some(&json!("disabled")));
        // Laptop: no wake-latency cap, no C-state cap, no parking.
        assert_eq!(get(&d, "cpu.wake_latency_us"), Some(&json!(0)));
        assert!(get(&d, "cpu.ccd_park").is_none());
        assert!(get(&d, "sched.ext").is_none());
        assert_eq!(get(&d, "vm.swappiness"), Some(&json!(150)));
        assert_eq!(get(&d, "zswap.enabled"), Some(&json!("0")));
        assert_eq!(get(&d, "gpu.amdgpu_dpm"), Some(&json!("low")));
        assert_eq!(get(&d, "vm.dirty_background_bytes"), Some(&json!(64 << 20)));
        assert_eq!(get(&d, "vm.dirty_bytes"), Some(&json!(256 << 20)));
        assert_eq!(get(&d, "thp.enabled"), Some(&json!("always")));
        assert_eq!(get(&d, "thp.khp_max_ptes_none"), Some(&json!(409)));
        assert_eq!(run_block(Goal::Gaming, &p).0["affinity"], "ccd0");
    }

    #[test]
    fn desktop_wake_cap_only_without_battery() {
        let mut p = legion();
        p.battery = false;
        let d = decide(Goal::Gaming, &p);
        assert_eq!(get(&d, "cpu.wake_latency_us"), Some(&json!(18)));
    }

    #[test]
    fn goals_differ_where_they_should() {
        let p = legion();
        let ps = decide(Goal::PowerSave, &p);
        let tp = decide(Goal::Throughput, &p);
        let de = decide(Goal::Desktop, &p);
        assert_eq!(get(&ps, "cpu.boost"), Some(&json!("0")));
        assert_eq!(get(&ps, "cpu.epp_ccd0"), Some(&json!("power")));
        assert_eq!(get(&tp, "cpu.epp_ccd1"), Some(&json!("balance_performance")));  // laptop: power-bound
        assert_eq!(get(&tp, "cpu.x3d_mode"), Some(&json!("frequency")));
        assert_eq!(get(&tp, "thp.enabled"), Some(&json!("always")));
        assert_eq!(get(&tp, "sched.preempt"), Some(&json!("lazy")));
        // Desktop on a laptop with dynamic EPP: EPP handed to the kernel.
        assert_eq!(get(&de, "cpu.dynamic_epp"), Some(&json!("enabled")));
        assert!(get(&de, "cpu.epp_ccd0").is_none());
        // Every goal: valid preset name, no duplicate keys.
        for g in Goal::ALL {
            let d = decide(g, &p);
            let mut keys: Vec<_> = d.iter().map(|x| x.key).collect();
            keys.sort();
            let n = keys.len();
            keys.dedup();
            assert_eq!(n, keys.len(), "{g:?}");
            assert!(d.iter().all(|x| tune::find(x.key).is_some()), "{g:?}: unknown key");
        }
    }

    #[test]
    fn parked_ccd_is_brought_back_and_swap_kinds() {
        let mut p = legion();
        p.current.insert("cpu.ccd_park".into(), "ccd1".into());
        assert_eq!(get(&decide(Goal::Gaming, &p), "cpu.ccd_park"), Some(&json!("none")));
        p.swap = SwapKind::Ssd;
        let d = decide(Goal::Gaming, &p);
        assert_eq!(get(&d, "zswap.enabled"), Some(&json!("1")));
        assert_eq!(get(&d, "zswap.compressor"), Some(&json!("lz4")));
        assert_eq!(get(&d, "vm.swappiness"), Some(&json!(100)));
        assert_eq!(get(&d, "vm.page_cluster"), Some(&json!(1)));
        assert_eq!(parse_release("7.0.1-gentoo"), (7, 0));
        assert_eq!(Goal::parse("Optimal desktop"), Some(Goal::Desktop));
        assert_eq!(Goal::parse("bare-throughput"), Some(Goal::Throughput));
    }

    #[test]
    fn intel_hybrid() {
        let mut p = legion();
        p.vendor = tune::Vendor::Intel;
        p.ccds.truncate(1);
        p.cache_ccd = None; p.freq_ccd = None; p.hybrid = true; p.x3d_driver = false; p.dynamic_epp = false;
        p.amd_igpu = false; p.intel_igpu = true; p.uncore = Some((800_000, 3_800_000));
        let g = decide(Goal::Gaming, &p);
        assert_eq!(get(&g, "cpu.epp_pcore"), Some(&json!("performance")));
        assert_eq!(get(&g, "irq.affinity"), Some(&json!("ecore")));
        assert_eq!(get(&g, "cpu.uncore_min_khz"), Some(&json!(3_800_000)));
        assert!(get(&g, "cpu.pstate_status").is_none());
        let ps = decide(Goal::PowerSave, &p);
        assert_eq!(get(&ps, "cpu.uncore_max_khz"), Some(&json!(2_000_000)));
    }

    /// Live run on the build machine: `cargo test -p lpm-helpers live_autotune -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_autotune() {
        for g in Goal::ALL {
            let v = autotune(g);
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
            assert!(v["ok"].as_bool().unwrap());
        }
    }
}
