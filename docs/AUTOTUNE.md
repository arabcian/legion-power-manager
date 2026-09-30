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

## Anchored at boot defaults

The machine's own boot state is the reference, not a constant: `lpm-boot-guard`
snapshots every tunable after the kernel, the distro and your `sysctl.conf`
have run, but before TLP and before any preset
(`/var/lib/legion-power-manager/defaults.json`; `clean` = no TLP/LPM yet;
a clean snapshot of a kernel is never replaced by a dirty one). Enable it once:
`rc-update add lpm-boot-guard boot` / `systemctl enable lpm-boot-guard`.
Without it the lpm-tune boot op takes the snapshot, else the kernel defaults are the reference.

For every scored knob:

- the boot value is the reference; a live value that drifted goes back to it
  when no candidate wins;
- distance from it costs modesty (log2 for numbers), and a number may move at
  most **2×** away (up to 8× for evidence-driven knobs such as
  `watermark_scale_factor`, in proportion to the reclaim evidence);
- the THP group starts from the boot THP configuration when the kernel honours it;
- **learning:** every apply of a guarded knob and every guard rollback is
  counted per key (`outcomes.json`). A rollback adds a stability cost of
  0.6 × rollbacks / (applies + 1); two rollbacks retire the key's alternatives
  — autotune only offers its boot value from then on.

`vm.dirty_*` is the exception: the boot default is a share of RAM that knows
nothing about the disk, so those two stay derived from the write rate.

## Machine signature (calibration)

`sudo lpm-calibrate` builds a **signature** of this machine: what every CPU,
scheduler and memory knob does here, alone and next to the others, idle and under load. Records accumulate
across runs in `/var/lib/legion-power-manager/signature.json`, tied to a
hardware fingerprint (DMI product, CPU model, RAM ±2 %; a signature of other
hardware is set aside, not merged).

**Plan.** Generated from the tunable table (CPU, Scheduler, Memory groups),
minus keys that are structural or unsafe to flip — CCD/core-type roles,
CPU offlining, driver switches, firmware power/thermal limits, the watchdog,
khugepaged pacing (needs minutes) and the like (`lpm-calibrate --list` shows
the plan, every excluded key and why). Candidates: all options for choice
rows, the flip for switches, live ÷2 / ×2 for numbers, and per-key ladders
where that is wrong (swappiness 60…180, watermark headroom 128/256/512 MiB,
boost ≤ 15000, CCD frequency caps 100/90/80/70 %, …).

**Design, not a queue.** The old flow measured one knob at a time (`ref, c1 … cn,
ref`), so every effect rested on two or three runs, interactions were never
seen, and a change had to prove itself alone. The default mode is a
**sequential experimental design** whose depth grows with the budget:

| depth | budget | runs/phase | space-filling | runs change | batch | confirm | chased interactions | dose refinements |
|---|---|---|---|---|---|---|---|---|
| lean | ≤ 20 min | ≤ 240 | 55 % | 3–7 (45 %: 2–4 in one cluster) | 5 | 2× | — | — |
| deep | ≤ 50 min | ≤ 520 | 50 % | 4–9, 20 % crowded with 8–14 | 6 | 3× | 8 | 4 |
| max | > 50 min, `--all` | ≤ 1000 | 45 % | 4–10, 35 % crowded with 10…half of all knobs | 8 | 4× | 16 | 8 |

(`--depth lean|deep|max` fixes the shape.) The budget is kept by wall clock — the
measured cost per run, not the estimate — so fitting time and slow runs are
counted.

**Progressive lean sessions.** A short session without `--depth` does not
repeat the same lean shape: it builds on the log and spends its time on the
next thing the data lacks, per phase, so 15-minute sessions started whenever
the machine is free (or `--sessions N` back to back) add up to a deep
calibration:

| stage | until | what the session does |
|---|---|---|
| base | every value of every knob was in ≥ 6 runs | lean space-filling (knobs that are new — new kernel, new tunable — first) |
| pairs | 90 % of knob pairs changed together in ≥ 2 runs | space-filling aimed at the pairs never seen together, interaction doubt |
| crowd | max(24, knobs) runs with ≥ 8 changes | half of the runs change 8–14 knobs at once |
| refine | one refinement session ran | doubt-driven runs, dose midpoints around the optima, 3× confirmation |
| polish | — | rotating, the least-run first: interaction/triple doubt · crowd · dose refinement |

The first three stages are read from the data itself (runs of deep/max
sessions count too); every session's strategy is noted in the signature
(`strategies`). When a progressive session's decisions are settled before its
time is up it keeps filling coverage gaps instead of stopping. `--list` shows
each phase's stage and what the log holds.

1. *Space-filling start.* Every run changes a random set of knobs together,
   balanced so every knob, every value and every **pair** of knobs appears
   about equally often — counting the runs already in the log, so a new
   session fills the gaps of earlier ones. Part of the runs change several
   knobs of one coupled cluster (memory/reclaim/THP/writeback, or CPU
   frequency + scheduler); crowded runs (deep/max) change many knobs at once —
   saturation and higher-order effects live there, and so does the optimum.
   A reference run every 6th run. Idle and load phases are designed separately;
   the time is split so each knob gets a similar number of runs.
2. *Runs chosen by doubt.* After each batch the model asks, for every goal,
   which decisions ("change this knob / leave it") are still uncertain and —
   deep/max — which **interactions** among the knobs that matter are still in
   doubt (the 2×2 contrast of two knobs in the context of the goal's optimum),
   and picks the next runs by how much they would reduce that doubt (posterior
   covariance with the doubtful contrasts, squared, over the run's own
   variance, per objective). The phase stops early when the expected number of
   wrong decisions drops below 0.35 and no interaction is left in doubt.
3. *Dose refinement* (deep/max). For numeric knobs of the predicted optima the
   geometric midpoints between the chosen dose and its measured neighbours
   (rounded to the knob's grid: 100 MHz, thousands, hundredths of the dirty
   window) become new levels and are measured inside that optimum.
4. *Confirmation.* The best configuration the models predict per goal is
   measured (2–4×) and compared with the prediction: `as predicted` or
   `SURPRISE`; either way it joins the data.

Every run measures **all four objectives** (latency, throughput, power, memory;
union of the benches the phase's knobs need), so any goal or custom weights are
decided from the same data — experiments are goal-agnostic, fits are per
objective and combined per goal without refitting.

**Phases.** Idle: quiet machine (CPU knobs: idle power, single/all-thread
work, work per joule, wake-up and thread ping-pong latency; I/O and heap
probes where relevant). Load: a ballast child holds memory down to
max(1 GiB, 5 % RAM) free, re-faults 64 MiB blocks and keeps half the CPUs busy
(wake-up and ping-pong latency, allocation stalls/s, heap probe, package power).
Every 4th ballast block is small pages with every other page freed again and is
re-made now and then (compaction heals it): free memory without a free 2 MiB
block, as on a machine that has been up for days — what THP defrag and the
compaction knobs really meet.
Single-thread work, the wake-up sleeper and the ping-pong pair run as fresh threads in
several short sub-runs, so one scheduler placement (V-Cache vs frequency CCD, same core
vs another) does not decide a whole run. Package power reads only the RAPL package
domain (not psys or the MMIO duplicate) and survives counter wrap-around; the power
source (battery or RAPL) is fixed for the whole session. CPU time used by other
programs during a run is metered (kernel threads such as kswapd/kcompactd are
not counted — they are part of what the knobs change); a run where others kept
more than 0.6 CPU busy counts proportionally less.
Brakes: the ballast dies at once if MemAvailable < 256 MiB or PSI memory
full > 40 %; it is the OOM killer's first target. A run that caused an OOM kill
is bisected (halves, at most 6 extra runs) to the smallest set of changes that
still does; a single value is stored as **unsafe**, a combination as an
**unsafe set** — neither is ever picked (subsets of an unsafe set are fine).

**THP.** Tested as its own family: the `thp` group (enabled never / madvise /
always, each with or without 16–64 KiB mTHP), `thp.defrag`, `thp.shmem_enabled`
and the 128 KiB – 1 MiB mTHP sizes as separate knobs, idle and loaded. The heap
probe (a disposable child, 2 MiB-aligned heaps) measures what THP changes:
sparse-heap footprint (bloat), fault latency and huge-page coverage of a plain
heap, dependent random loads over a plain heap (TLB reach programs get without
asking), over a `MADV_HUGEPAGE` heap (programs that opt in, with its per-2 MiB
fault p99 — defrag/compaction stalls) and over shared memory (memfd,
`shmem_enabled`). `lpm-calibrate --only @thp` runs just this family (`@mem`,
`@cpu`, `@sched` likewise).

**From runs to a model** (`src/model.rs`). Per run and metric: log-ratio to the
session's reference runs (+ = better), clipped to ±0.5, averaged per objective
— no noise thresholding, so small real effects are not thrown away; a metric
counts only if 80 % of the runs have it. The model is an additive Gaussian
process per objective (Bayesian regression in kernel form):

- main effects per value; numeric ladders use a random-walk prior over their
  ordered values (neighbouring doses share strength, doses between measured
  points are interpolated), unordered choices get one term per value;
- **every pair** of knobs, and from max(150, 4 × knobs) rows **every triple**,
  as kernel terms (elementary symmetric polynomials — no term list to
  enumerate); pairs inside a coupled cluster, pairs across clusters, triples
  inside and across each have their own prior scale; a knob that responds on
  its own carries more interaction prior (heredity, from a screening fit);
- each session has its own offset and trend, plus a slow drift over wall-clock
  time (thermal, background) that the reference runs pin down; prior scales and
  the noise level come from evidence maximisation and are kept in the signature
  (re-tuned every few batches), outlier runs are down-weighted (Student-t
  style, from leave-one-out residuals);
- diagnostics: leave-one-out R² and the share of 90 % intervals that hold;
  the strongest credible interactions — pairs and triples, per objective, as
  **synergy** (more together), **overlap** (both help, less together) or
  **conflict** (together credibly worse than the better one alone) — are
  printed after a calibration and by `--show`. Rows decay with a 90-day
  half-life, another kernel (major.minor) counts half, rows from an older
  benchmark version 0.6, at most 1600 rows kept (1100 fitted). The old
  one-at-a-time records stay valid: they join as single-knob rows worth half
  their evidence.

**Decisions (autotune).** Every quantity is a posterior. For a goal's weights:

- stand-alone effects for the per-knob rules and THP/dirty come from the model
  (`n` = how much the data narrowed the prior), replacing the estimate in
  proportion to the evidence as before;
- then a **joint pass** searches the best combination of all calibrated knobs
  together — coordinate ascent, joint moves of coupled pairs, restarts — on the
  posterior mean minus 0.5 sd, minus modesty and learned rollback risk, and a
  margin (0.03) that every change must pay by itself. The per-knob choices are
  the starting point, so the outcome is never worse under the model;
- every change is then checked *in context*: gain of the whole combination
  minus the combination without that knob, lower bound `mean − 0.5·sd ≥ margin`,
  else the knob goes back to its reference (this is what drops a redundant
  second knob and keeps a pair that only pays together);
- kept: the boot-default trust region (2×, widened by one doubling for values
  the model has evidence for), unsafe values and unsafe sets, retired keys,
  the phase blend per goal (load share: throughput 70 %, gaming 60 %,
  desktop 40 %, power saving 20 %; a knob only one phase models keeps its full
  weight there), scoped roles (cpu.epp vs cpu.epp_ccd0 — CCD roles stay
  structural), THP and the dirty window as fixed context;
- the `why` of a changed knob says what it gains alone and in context; the
  notes carry the predicted weighted gain ± sd of the whole combination.
- your weights still decide; stability risk still comes from the model and
  the guard's rollback history, never from a benchmark.

**Safety:** refuses while an Optimizations preset is active (Restore
originals first), holds the tune lock, journals every original before
writing, restores after each knob and on Ctrl-C; `sudo lpm-calibrate --restore`
after a crash. Power on AC is the CPU package only (RAPL); device power
states need a run on battery.

```sh
lpm-calibrate --list [--budget N]            # knobs, excluded keys, depth, runs and time per phase, rows logged
sudo lpm-calibrate                           # 15-minute progressive lean session (next stage of what the log lacks)
sudo lpm-calibrate --sessions 4              # four of them back to back: start it and walk away
sudo lpm-calibrate --budget 60               # max depth: crowded runs, interactions chased, doses refined
sudo lpm-calibrate --budget 30 --only @thp   # the THP family only (also @mem, @cpu, @sched)
sudo lpm-calibrate --budget 30 --phase load  # load phase only
sudo lpm-calibrate --seed 7 --no-confirm     # other random design; skip the confirmation runs
sudo lpm-calibrate --oat                     # legacy one-knob-at-a-time flow
lpm-calibrate --show                         # effects per value (n = evidence), pair/triple interactions, model quality
```

All options:

| option | effect |
|---|---|
| `--budget MIN` | time for the session (default 15), split between the idle and load phases; kept by wall clock. ≤ 20 min = lean (progressive unless `--depth`), ≤ 50 deep, more max |
| `--all` | no time limit: max depth up to its run cap (1000 per phase) |
| `--depth lean\|deep\|max` | fixed shape regardless of budget; `lean` turns progression off |
| `--sessions N` | N sessions back to back (1–48); progressive ones each take the next stage |
| `--phase idle\|load\|both` | only one phase (default both) |
| `--only K1,K2,…` | only these keys; group aliases `@thp` (thp group, defrag, shmem, mTHP sizes), `@mem` (Memory), `@cpu` (CPU), `@sched` (Scheduler) |
| `--thorough` | every measurement window ×1.6 and a 512 MiB I/O file (less noise, slower runs; `--oat`: two rounds per knob) |
| `--dir PATH` | directory for the I/O benchmark's temporary files (default `/var/tmp`; use the disk you care about) |
| `--seed N` | another random design (default fixed, so a session is reproducible) |
| `--no-confirm` | skip the confirmation runs of the predicted optima |
| `--oat` | legacy one-knob-at-a-time flow (no interactions) |
| `--list` | the plan: knobs and values, excluded keys and why, depth or progressive stage per phase, runs and time, rows logged (no root needed) |
| `--show` | the signature: effects per value and phase, model quality, pair/triple interactions (no root needed) |
| `--restore` | put back every value an interrupted run left changed (journal in `/run/legion-power-manager/calibrate.json`) |

Needs root, a clean state (no active Optimizations preset) and the tune lock;
Ctrl-C at any point restores everything and keeps what was measured. For
device-level power, run once on battery (on AC the power figure is the CPU
package only).

Run it again to add evidence: rows accumulate, and the next design starts where
the doubt is. Offline check of the design against a noisy synthetic machine
(same number of runs, same decision rule, net utility): `cargo test -p
lpm-helpers --bin lpm-calibrate sequential`; the noise/budget sweep is
`cargo test --release -p lpm-helpers --bin lpm-calibrate sweep -- --ignored --nocapture`.

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
