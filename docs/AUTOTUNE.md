# Autotune

Autotune turns the Optimizations tab into a preset generator: pick a base,
LPM profiles the machine and checks every row with the value that base calls
for **on this hardware**. Nothing is written until *Apply checked*; *Save*
makes it an ordinary preset (usable as ★ game preset, boot preset or in a Scene).

GUI: Optimizations → *Autotune for* [base] → **⚙ Autotune** (report dialog with
the reason for every row). CLI:

    lpm-autotune profile [--json]
    lpm-autotune <powersave|gaming|throughput|desktop> [--json] [--save [NAME]]

Both run unprivileged (`tune-helper {"op":"autotune","goal":...}`).

## Pipeline (crates/lpm-helpers/src/autotune.rs)

1. **Profile::gather** – CPU vendor/model, cores/threads/SMT, L3 domains (CCDs),
   V-Cache and frequency die, Intel P/E split, cpufreq driver and EPP support,
   governors, cpuidle governors and C-state exit latencies, RAM, swap kind
   (zram / disk+zswap / SSD / HDD), NVMe/HDD/SATA, battery and AC, GPUs
   (NVIDIA/AMD/Intel), Wi-Fi, kernel version, NUMA nodes, installed sched_ext
   schedulers, uncore range, and live values of state-dependent rows.
2. **decide(goal, profile)** – pure rule set, one reason per decision (unit tested
   with synthetic profiles: X3D laptop, desktop, Intel hybrid).
3. **filter** – drops rows this machine/kernel does not offer (`tune::files`
   empty) or values `tune::validate` rejects; they are listed as "skipped".

Autotune never parks a CCD (it brings a parked one back): parking breaks
Wine/Proton CPU numbering and nvidia-powerd; isolation is done with launch
affinity plus IRQ/workqueue steering instead.

## The four bases

| Area | Power saving | Gaming (latency + throughput) | Bare throughput | Optimal desktop |
|---|---|---|---|---|
| EPP | `power` | X3D: V-Cache CCD `performance`, frequency CCD `balance_power`; hybrid: P `performance`, E `balance_power` | laptop `balance_performance` (package-power-bound), desktop `performance` | `balance_performance`, or dynamic EPP on a laptop |
| Turbo / floor | off / `cpuinfo_min` | on / `lowest_nonlinear` | on / `lowest_nonlinear` | on / `lowest_nonlinear` |
| X3D preference | – | `cache` | `frequency` | – |
| cpuidle | teo, all states | teo; laptop: no cap (deep idle = boost headroom); desktop: deepest state skipped via wake-latency QoS | all states | teo, all states |
| THP | madvise, khugepaged slow | `always` + defer+madvise + max_ptes_none 409 (≥16 GB and kernel ≥6.12, else madvise) | same as gaming | same as gaming |
| Dirty cache (bytes) | 10 % / 20 % of RAM, writeback 15 s, expire 60 s | 64 MB / 256 MB, writeback 15 s (CachyOS) | 10 % / 40 % of RAM, ≤16 GB (TuneD) | same as gaming |
| Swap | zram: swappiness 150 (180 power saving), page-cluster 0, zswap off; SSD swap: 100 + zswap, page-cluster 1; HDD: 60 + zswap, page-cluster 2 (CachyOS) | | | |
| Working set | – | MGLRU min_ttl 1 s (0 under 12 GB RAM or while memory pressure is high), vfs_cache_pressure 50, watermark_scale_factor sized to ~400 MiB headroom (≥16 GB, see Evidence) | watermark_scale_factor as gaming | as gaming |
| Preemption / slice | lazy (6.13+) | full, stock EEVDF slice | lazy, stock slice, migration_cost 5 ms (TuneD) | lazy, stock slice |
| Sched features (debugfs) | RUN_TO_PARITY on | NEXT_BUDDY on | RUN_TO_PARITY on | NEXT_BUDDY on |
| Floor frequency (7.1+, if present) | `cpuinfo_min` | `nominal` | `nominal` | `nominal` |
| khugepaged max_ptes_swap | 0 (with swap) | 0 (with swap) | stock | 0 (with swap) |
| Steering | – | IRQs + unbound wq on frequency die / E-cores | all CPUs | – |
| RT uclamp (schedutil only) | 0 | 1024 | 1024 | 256 |
| Devices | ASPM powersave, PCI/USB runtime PM auto, HDA 1 s, Wi-Fi PS on, APST all states | ASPM performance, runtime PM on (GPU excluded), APST off, Wi-Fi PS off, iGPU low with dGPU | iGPU low with dGPU, SATA max_performance | SATA med_power_with_dipm |
| Network | – | BBR + fq | BBR + fq | BBR + fq |
| Launch | – | nice −5, pinned to V-Cache CCD on X3D | – | – |

## Evidence layer (what the machine has actually been through)

`Profile::gather` also reads `/proc/vmstat` (allocstall_*, pgscan_direct/kswapd,
compact_stall, thp_fault_*, pswpout) and `/proc/pressure/{memory,cpu}` (avg300).
Evidence never changes a goal's character; it only raises a safety margin or
withholds an aggressive setting, and counters from a machine up for less than an
hour (or with < 200k scanned pages) are ignored:

- **watermark_scale_factor** is sized in bytes (~400 MiB of free headroom →
  122 on 32 GB, 61 on 64 GB, 150 cap on 16 GB) instead of a fixed 125. If direct
  reclaim is a regular event (≥ 10 % of scanned pages *and* ≥ 1000 allocstalls)
  it is raised 1.5× and also applied below 16 GB.
- **MGLRU min_ttl** trades thrashing for an OOM kill when the working set does
  not fit, so it is switched off on < 12 GB RAM and while memory PSI is high
  (some ≥ 10 % or full ≥ 1 %).
- The report shows an "Observed" line with these numbers.

## Knobs added in this round (all verified against kernel sources)

| Row | Source | Use |
|---|---|---|
| `sched.feat_next_buddy` | kernel/sched/features.h comment; tip commit aceccac58ad7 (Nov 2025) enabling it: waker/wakee share cache-hot data | Gaming, Desktop: on. Default-on on newer kernels, so it mostly pins the value |
| `sched.feat_run_to_parity` | features.h: no wakeup preemption before 0-lag point or slice end (PREEMPT_SHORT can still cancel) | Throughput, PowerSave: on (fewer switches) |
| `cpu.floor_freq` | amd-pstate docs (7.1, `amd_pstate_floor_freq`): frequency firmware throttles to first under power/thermal limits; kernel default = nominal | PowerSave `cpuinfo_min`, others `nominal`. n/a without CPPC Performance Priority (not on Zen 5 mobile today) |
| `thp.khp_max_ptes_swap` | THP docs: swapped pages khugepaged reads back when collapsing (default 64) | 0 while swap exists, except Throughput |

Checked and deliberately **not** automated (no measured source for a fixed value,
or the effect depends on the workload): `sched_domain` imbalance_pct/intervals
(debugfs layout differs per kernel), `rcu_normal`/`rcu_expedited` (expedited
grace periods skip idle CPUs, so the power argument is weak), EEVDF base slice,
`percpu_pagelist_high_fraction`, `extfrag_threshold`, `vm.overcommit_*`,
`laptop_mode`. CachyOS's 7.2 branch carries EEVDF/cgroup-mode patches
(`cgroup_mode`, single runqueue) that are not mainline; they expose no stable knob to tune.

Deliberately **not** automated: a global EEVDF slice (no measured source for a
fixed value; EEVDF already favours short-slice tasks), `min_free_kbytes` (the
kernel scales it itself), and sched_ext (scx_lavd is still in development and
has open fps-regression reports on some CPUs – choose it per game after testing).

EPP: Phoronix measured on Zen 5 that powersave + balance_performance had the
lowest average CPU power, and on Ryzen mobile that uncapped games run at max
clock with both performance and balance_performance – hence performance only on
the game's CCD, balance_performance for desktop and power-bound throughput.

References: CachyOS-Settings (sysctl.d, THP tmpfiles), kernel docs (sysctl/vm, workqueue, sched-util-clamp, sched-energy,
PM QoS sysfs ABI, NVMe APST commit), TuneD throughput/latency profiles, TLP
defaults.
