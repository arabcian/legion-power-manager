# Autotune

Autotune profiles the machine and proposes an Optimizations preset for one of
four goals: **Power saving**, **Gaming**, **Bare throughput**, **Optimal desktop**.
Nothing is written until you press *Apply checked* (or save it into a preset/scene).

## How a value is chosen

There are two kinds of rules.

**Structural rules** have one right answer for the hardware: amd-pstate active,
V-Cache CCD roles, game affinity / IRQ steering on X3D, amdgpu DPM `auto`,
bringing a parked CCD back, BBR + fq, swap cost model (kernel doc: swappiness
> 100 for in-memory swap).

**Scored knobs** are every setting with a real trade-off. Each candidate value
has an effect vector over five objectives relative to the reference
("leave it", or the kernel default):

| objective  | meaning |
|------------|---------|
| latency    | frame-time / input / wake-up smoothness |
| throughput | work per second |
| power      | idle and load power, heat |
| footprint  | RAM held by the setting (reserves, THP bloat, dirty cache) |
| stability  | risk of hangs, resume failures, OOM kills (always a cost) |

`U = Σ weight × effect − 0.10 × deviation`. A candidate is written only if it
beats the reference by 0.03 — otherwise the setting is left alone. Effects are
ordinal estimates taken from kernel documentation plus this machine's evidence;
a knob that fixes a problem the machine does not show earns nothing.

Default weights (latency, throughput, power, footprint, stability):

| goal       | lat | thr | pwr  | mem | stab |
|------------|-----|-----|------|-----|------|
| Gaming     | 1.0 | 0.6 | 0.15 | 0.4 | 1.0 |
| Desktop    | 0.7 | 0.3 | 0.7  | 0.6 | 1.0 |
| Throughput | 0.2 | 1.0 | 0.2  | 0.5 | 1.0 |
| Power save | 0.2 | 0.1 | 1.0  | 0.5 | 1.0 |

Change them per goal with **Weights…** next to the Autotune button, or
`lpm-autotune <goal> --weights latency=1.2,footprint=0.8`. Range 0–3;
stability cannot go below 0.5. The weights used are stored in the preset's
`autotune` block.

## Memory and writeback

| knob | rule |
|------|------|
| `vm.dirty_bytes` / `dirty_background_bytes` | sustained write rate × window (1 s / 0.25 s by default; 0.25–2 s scored), capped at 2 % of RAM and 1 GiB, floors 32 / 8 MiB, MiB-aligned. Rate: probe → `/sys/block/*/stat` → device class |
| THP group (`enabled`, `max_ptes_none`, khugepaged pace, mTHP sizes) | searched jointly. `always` costs footprint (a touched 2 MB range takes a whole huge page; the split shrinker only returns it under pressure). With any mTHP size on, `max_ptes_none` is only 0 or 511 (kernel 7.x). Pace never above 2× the default. A `transparent_hugepage=` boot parameter is left alone |
| `vm.watermark_scale_factor` | raised only with evidence (direct-reclaim share, allocstall, `kswapd_low_wmark_hit_quickly`); max 300 and 2 % of RAM / 1 GiB of headroom. That headroom leaves MemAvailable |
| `vm.watermark_boost_factor` | never above 15000. Note: boosting *frees* page cache after fragmentation events; it does not hold memory |
| `vm.min_free_kbytes` | never raised; a live value above 1 % of RAM / 256 MiB is repaired |
| `mm.lru_gen_min_ttl` | scored against OOM risk; with the default weights it stays off |
| `vm.compaction_proactiveness`, `vm.vfs_cache_pressure` | scored; at least one defrag path stays on with THP `always` |

## Devices

Runtime PM, ASPM, USB autosuspend, NVMe APST, HDA power save, Wi-Fi power save
and suspend mode are scored with a stability cost. Boot parameters win:
`usbcore.autosuspend=` → USB autosuspend is never touched; `pcie_aspm=force` →
deeper ASPM states carry a larger risk; `pcie_aspm=off` → no ASPM rows.
`kernel.watchdog` is never turned off (a hang would leave no log).

## Hard limits (audit)

`lpm-autotune audit` and tune-helper check every preset/scene/live value:

- **refused**: `dirty_background_bytes` < 1 MiB, `dirty_bytes` < 4 MiB or ≤ background,
  `watermark_scale_factor` > 1000, `min_free_kbytes` > 3 % of RAM, `vfs_cache_pressure` < 10
- **warned** (with a fix): `max_ptes_none` ≠ 0/511 with mTHP on, khugepaged faster than 2×,
  boost > 15000, reserves above 1 %, `zone_reclaim_mode` on a single node, …

tune-helper never stores or writes a refused value. Old presets/scenes (e.g. the
int32-wrapped `dirty_bytes` 8192 / 290489958 from earlier GUI versions):

    lpm-autotune audit            # list
    lpm-autotune audit --fix      # repair (keeps *.json.bak), then re-save scenes in the GUI

## Pressure guard

After an apply that wrote memory/writeback knobs, a detached helper samples PSI
once a second for 120 s. If `io full` stays ≥ max(2×before + 10, 25) % for 15 s,
`memory full` ≥ max(before + 5, 10) % for 10 s, or allocation stalls exceed
500/s for 10 s, it restores exactly those knobs, writes
`/run/legion-power-manager/tune/guard.json` and a kernel-log warning (visible
in the Health tab). It stops early when anything else changes those knobs.

## Tools

    sudo lpm-autotune probe       # ≤ 512 MiB / ≤ 4 s O_DIRECT write into an unlinked temp file
    lpm-autotune report [SEC]     # meminfo, watermarks, THP/writeback settings, PSI, vmstat deltas
    lpm-autotune <goal> --json    # full output incl. per-knob scores and constraint notes

Baseline entries whose original came from TLP are listed as `tlp_originals` in
the helper state: "restore" returns them to TLP's state, not the kernel default.
