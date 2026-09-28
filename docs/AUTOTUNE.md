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
| Working set | – | MGLRU min_ttl 1 s, vfs_cache_pressure 50, watermark_scale_factor 125 (≥16 GB) | watermark_scale_factor 125 | as gaming |
| Preemption / slice | lazy (6.13+) | full, stock EEVDF slice | lazy, stock slice, migration_cost 5 ms (TuneD) | lazy, stock slice |
| Steering | wq power_efficient | IRQs + unbound wq on frequency die / E-cores | all CPUs | – |
| RT uclamp (schedutil only) | 0 | 1024 | 1024 | 256 |
| Devices | ASPM powersave, PCI/USB runtime PM auto, HDA 1 s, Wi-Fi PS on, APST all states | ASPM performance, runtime PM on (GPU excluded), APST off, Wi-Fi PS off, iGPU low with dGPU | iGPU low with dGPU, SATA max_performance | SATA med_power_with_dipm |
| Network | – | BBR + fq | BBR + fq | BBR + fq |
| Launch | – | nice −5, pinned to V-Cache CCD on X3D | – | – |

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
