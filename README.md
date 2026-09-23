# Legion Power Manager 2 — Rust + C++/Qt6

Port of the PySide6 Legion Power Manager: privileged work in Rust, GUI in Qt6.
Five tabs: Home (power profile), Firmware Attributes, NVIDIA Curve Optimizer,
Ryzen Curve Optimizer *or* Intel Undervolt (picked by CPU vendor), Optimizations
(system tuning).

## Install

    sudo ./install.sh                   # build + install to /usr
    sudo ./install.sh --remove-legacy   # …and remove the old Python install

Needs: Rust ≥ 1.75, Qt ≥ 6.4 (Widgets, Network), CMake ≥ 3.19, polkit.
Runtime: NVIDIA proprietary driver (for the NVIDIA tab), root-owned `ryzenadj`
in `/usr/bin`, `/usr/sbin`, `/usr/local/{bin,sbin}` or `/opt/ryzenadj` (for the
Ryzen tab). Neither is required to install or use the other tabs.

Gentoo: `./make-dist.sh` produces `dist/legion-power-manager-2.0.0.tar.xz`
(crates vendored, offline build). Copy it into DISTDIR and use the ebuild in
`packaging/gentoo/sys-power/legion-power-manager/` from a local overlay.

After installing, launch `legion-power-manager` once (it starts hidden in the
tray — pass `--window` to open it immediately, or click the tray icon) to
create your config directories.

### Quick start

- **Power profile (Home tab).** Pick a profile card; it's applied immediately
  through `legion-profile-helper`, no reboot needed.
- **GPU curve (NVIDIA tab).** *Read Current Curve* first, drag points or set
  Core/Memory offsets, *Apply Offsets*. *Save As…* keeps a named profile;
  ★ *Default* auto-applies one at every boot (needs
  `systemctl enable --now nvcurve-autoload` / `rc-update add nvcurve-autoload default`,
  see below).
- **CPU curve (Ryzen tab).** Per-core Curve Optimizer offsets, or one offset
  for every core with *Apply All-Core*. *Disable* a slot if your CPU has fewer
  physical cores than SMU slots (common on cut-down/partially-populated CCDs).
- **CPU undervolt (Intel Undervolt tab, Intel CPUs only).** *Read current*,
  tick the values to write, *Apply*. Voltage offsets for core / cache / iGPU /
  system agent / analog I/O (OC mailbox MSR 0x150), IccMax, TCC offset and
  PL1/PL2 (MSR 0x610, mirrored to MCHBAR). A readback mismatch means the BIOS
  locks undervolting (Plundervolt, CVE-2019-11157). *⏻ Apply at boot & resume*
  stores the values for the `lpm-intel-uv` service; offsets are lost on S3,
  so the unit (systemd) / elogind hook (OpenRC) re-applies them after resume.
  Separate AC / battery profiles, BD PROCHOT, cTDP, PL lock, positive offsets
  (opt-in), ThrottleStop.ini import, live throttle/VCore/RAPL monitor.
  Optional daemon (`lpm-intel-uv-daemon` service): switches profile with the
  charger, re-applies after resume and every interval (the EC restores PL1/PL2),
  and runs hwphint EPP switching by load or RAPL power.
  Without the GUI: `sudo lpm-intel-uv read | apply … | reset | monitor | measure |
  throttlestop FILE | turbo on|off | daemon`.
- **System tuning (Optimizations tab).** Pick a built-in preset (top dropdown),
  *Load*, review the checked rows, *Apply checked*. ★ *Use for games* wires it
  into Lutris/Steam (see the Game launch sub-tab for the exact hooks); ⏻ *Apply
  at boot* stores it as the boot preset (needs enabling the `lpm-tune` service,
  see below). *Restore originals* always available at the top of the tab.
  **Every row's tooltip explains what it does, when to change it, and what
  numbers to enter for common scenarios — hover before asking "what do I put
  here?"**; the section further down also walks through several tuning goals
  end to end.

### Boot-time services (optional)

Both nvcurve-autoload (★ Default GPU profile) and lpm-tune (⏻ Optimizations
boot preset) are installed for either init system; enable only the one your
distro actually runs:

    # systemd
    systemctl enable --now nvcurve-autoload.service
    systemctl enable --now lpm-tune.service
    systemctl enable lpm-intel-uv.service           # Intel only

    # OpenRC
    rc-update add nvcurve-autoload default
    rc-update add lpm-tune boot
    rc-update add lpm-intel-uv boot                 # Intel only

Neither does anything until you've actually set a ★ default profile / ⏻ boot
preset from the GUI — enabling the service early is harmless.

### Layout

    /usr/bin/legion-power-manager                    GUI + tray
    /usr/bin/nvcurve                                 nvcurve CLI
    /usr/bin/lpm-gamemode                            Lutris/Steam game-mode hook (user, no setuid)
    /usr/bin/lpm-intel-uv                            standalone Intel undervolt CLI (root)
    /usr/libexec/legion-power-manager/
        legion-profile-helper  fwattr-helper  ryzen-co-helper
        tune-helper  intel-uv-helper                            (root:root 0755)
        nvcurve-root-helper                                     (root:root 0700)
    /usr/share/polkit-1/actions/com.legion-power-manager.policy
    /etc/polkit-1/rules.d/49-legion-power-manager.rules
    /etc/xdg/autostart/legion-power-manager.desktop  starts in the tray on login
    /etc/init.d/nvcurve-autoload                     OpenRC (GPU profile)
    /etc/init.d/lpm-tune                             OpenRC (tuning boot preset, runlevel boot)
    /usr/lib/systemd/system/nvcurve-autoload.service systemd (GPU profile)
    /usr/lib/systemd/system/lpm-tune.service         systemd (tuning boot preset)

Unchanged data locations: `/etc/nvcurve/{config.json,profiles/}` (GPU),
`~/.config/ryzen-curve-optimizer/profiles/` (CPU). New: `~/.config/legion-power-manager/
{tune-presets/,tune.json}` (tuning presets, ★ game preset),
`/etc/legion-power-manager/tune-boot.json` (root-owned boot preset),
`/run/legion-power-manager/tune/state.json` (saved originals; tmpfs).

## Tray & app behaviour (step 4)

- Starts hidden in the tray; `--window` opens the window. Closing hides to the
  tray, Quit exits. No tray after ~6 s → the window is shown instead.
- A second launch never starts a second copy: it asks the running one to show
  its window (per-user QLocalServer, user-only socket permissions).
- Tray menu: Power Profile, CPU Curve (Ryzen), GPU Curve (NVIDIA) with saved
  profiles, reset entries and a 2.5 s cooldown, rebuilt on every open.
- The icon is drawn at runtime (same glyph as before); an SVG is installed for menus.

## Components

Cargo workspace:

| Crate | Content | Status |
|---|---|---|
| `crates/lpm-helpers` | pkexec root helpers: `legion-profile-helper`, `fwattr-helper`, `ryzen-co-helper` | step 1 ✔ |
| `crates/lpm-helpers` `intel_uv` | Intel undervolt core, `intel-uv-helper`, `lpm-intel-uv` CLI | ✔ |
| `crates/nvcurve` | nvcurve core library: NvAPI + NVML (dlopen), HAL, safety, snapshots, profiles, autoload | step 2a ✔ |
| `crates/nvcurve` bins | `nvcurve` CLI, `nvcurve-root-helper` | step 2b ✔ |
| nvcurve REST/WS server | not ported — nothing in the GUI uses it | — |

    cargo test --release

## nvcurve core — mapping from the Python package

| Python | Rust |
|---|---|
| nvapi/bootstrap, constants, errors, types | `nvapi.rs`, `types.rs` |
| pynvml | `nvml.rs` (only the entry points used, resolved lazily by name) |
| hal/gpu, vfcurve, ranges, monitoring, limits, snapshot | `hal/*.rs` |
| safety, atomicio, config | `safety.rs`, `atomicio.rs`, `config.rs` |
| profiles/native, apply | `profiles/*.rs` |

No runtime dependency on Python, pynvml or nvidia-ml-py. Linking needs no
NVIDIA libraries; a missing driver is an `NvError::Unavailable`.

### Behavioural differences (all deliberate)
- `snapshot::restore(None)` now picks the newest *regular* `.bin` and applies the
  same snapshot-dir containment check as an explicit path. The Python version
  would follow a planted symlink that sorted last.
- `validate_limits` reports the offending field structurally; `apply_profile`
  no longer substring-matches error text to decide what to drop.
- `apply_profile` reads the ClockBoostTable once and reuses it for both the
  auto-snapshot and the write baseline (was two reads).
- nvidia-smi fallback only runs a root-owned, non-writable binary from a fixed
  list (`/usr/bin`, `/opt/bin`, `/usr/sbin`) — never `$PATH`.
- Profile loading rejects non-integer numeric fields at load time (was: loaded,
  then failed later). One bad file is skipped with a warning, as before.
- Profile-name filter uses Rust's Unicode `is_alphanumeric`; this can differ from
  Python's `str.isalnum` only for rare combining marks.

## nvcurve root helper (step 2b)

Same ops and JSON protocol as `root_helper.py`; results still land in
`/run/nvcurve-gui/*.json` in the same shape. Differences:
- No `python3 -m nvcurve` subprocesses at all (memlock, default profile and
  autoload were still shelled out) — one process, one NvAPI session.
- `write_nvcurve_profile` now applies the same schema check as
  `apply_gpu_offsets` (was: "any valid JSON"). Root applies this file later.
- Deleting a profile clears *every* auto-load entry that references it; the
  Python helper ran `profile default --clear`, which only cleared GPU 0's key.
- `profile_dir` is pinned to `/etc/nvcurve/profiles` (where the helper writes)
  instead of following config.json, so write and apply can't diverge.
- autoload's log output is captured and returned as the op message.

## nvcurve CLI (step 2b)

Ported: `read [--full|--json|--raw]`, `inspect`, `write`, `snapshot`, `gpus`,
`profile`, `memlock`, `autoload`; global `--gpu N`. Log level via
`NVCURVE_LOG=debug`. Not yet: `serve`, `daemon` (2c), `verify`, `setup`,
`read --diag`. Dropped: `service` (systemd; OpenRC init script comes in step 4).
Snapshot/profile dirs now follow `/etc/nvcurve/config.json` in every command
(the Python CLI used built-in defaults for `snapshot`).

### First hardware test (read-only first)
    nvcurve gpus
    sudo nvcurve read
    sudo nvcurve read --json | head -40
    sudo nvcurve inspect --point 0
    sudo nvcurve write --global --delta 0 --dry-run

## Qt6 GUI (step 3a) — `gui/`

    cmake -S gui -B gui/build -DCMAKE_BUILD_TYPE=Release
    cmake --build gui/build -j
    gui/build/legion-power-manager

Requires Qt ≥ 6.4 (Widgets) and a C++20 compiler. Helper directory is
`-DLPM_HELPER_DIR=/usr/libexec/legion-power-manager` (default; must match polkit).

Done: theme (same tokens/QSS), async pkexec runner, platform-profile reader,
Home tab (profile switcher, hardware inventory, live sensors).
All four tabs are ported (3a–3d). Tray + install layout: step 4.

Differences from the PySide6 Home tab:
- pkexec runs asynchronously; the window no longer freezes while the polkit
  dialog is open, and profile buttons are disabled during the switch.
- nvidia-smi runs asynchronously with a 3 s kill timeout, never stacked; the
  Python tab blocked the GUI thread up to 3 s every 2 s poll.
- hwmon sensors are ordered numerically (temp2 before temp10).
- `LPM_SCREENSHOT=/tmp/x.png` renders the window once and exits (dev aid).

## Firmware Attributes tab (step 3b)

Port of `fwattr_tab.py`, same fwattr-helper protocol (single + batch).
Differences:
- Values are snapped to the driver's `min + k·step` grid before writing
  (the Python tab could send off-grid values; the helper only checks range).
- Writes are async; the tab is disabled while pkexec is pending, so a second
  Apply can't race the first.
- Home tab's profile switch unlocks/locks the tab immediately (signal).
- "Default" button no longer clipped (was fixed 60 px).
- `LPM_FWATTR_BASE=/path` renders against a fake tree (dev only; the helper
  still rejects anything outside `/sys/class/firmware-attributes`).

## Ryzen Curve Optimizer tab (step 3c)

Port of `ryzen_curve_optimizer.py`: CCD/L3 topology + CPPC detection, fixed
8-slot-per-CCD SMU grid, disable checkboxes, per-CCD fill, all-core, reset,
profiles in `~/.config/ryzen-curve-optimizer/profiles` (same files as before).
Differences:
- **Bug fix:** applying a saved profile from the tray fired the per-core batch
  while the all-core pkexec call was still running; it was rejected as "busy",
  so profiles with both parts only ever got the all-core half. Now chained:
  per-core runs after all-core succeeds.
- Profiles are written with QSaveFile (atomic, fsync) and reads are capped at 256 KiB.
- Layout redesign: **Reset Curve Optimizer** is a global action (it clears
  all-core *and* per-core offsets, now with a confirmation) next to Profile;
  Apply All-Core and Apply Per-Core share one width and right edge. Each CCD is
  a card with a single aligned grid (Slot · Offset · Disable · CPPC, header in
  the same grid), columns spread evenly, ★ marks the two best CPPC cores per CCD.
  The grid scrolls instead of being squeezed on short windows.
- The tab's private Gruvbox sheet is gone; it uses the app theme.

## NVIDIA Curve Optimizer tab (step 3d)

Port of `nvcurve_gui.py` (embedded mode), same nvcurve-root-helper ops and
`/run/nvcurve-gui/*.json` result files. The V/F chart is a QPainter widget
(no QtCharts). Differences, most of them fixes:
- **Group drag clamp bug:** the Python widget clamped every *frequency* to
  800–3000 MHz while dragging, so shifting the whole curve lifted all
  sub-800 MHz idle points to 800 — large unintended positive offsets that
  then got applied. Now *offsets* are clamped to ±1000 MHz (driver cap) and
  frequencies only to ≥ 0.
- **Arrow-key edits** in the Python widget changed only the plotted points and
  were lost on the next recompute (e.g. touching the core spin box). All edits
  now go through one offset model.
- **Stale result files:** a result JSON is only accepted if it was written
  during the current operation (the Python tab re-read an older file if the
  helper's best-effort write failed).
- **NVML** is opened (dlopen) only while the tab is visible and closed when
  hidden, so the dGPU can reach runtime D3cold; pynvml is no longer needed.
  Mem/hotspot temperature labels were dropped (pynvml has no such sensor
  constants, so they always read N/A).
- The first curve read (which may raise a polkit prompt) happens on first
  show, not at startup.
- "Apply Profile" button applies a saved profile directly (`apply_named_profile`);
  selecting a profile in the combo only loads it into the editor, as before.
- Ctrl+click extends the selection, Ctrl+A selects all, Shift+↑/↓ steps 15 MHz.
- `LPM_NVCURVE_FAKE=/path/read.json` renders a canned curve (dev only).

## Optimizations tab (step 5) — lutris-game-tune, integrated

Everything `lutris-game-tune.sh` does at PRE/POST, plus the tuning knobs that
came up during the sysfs review, as one tab backed by a Rust root helper.

    crates/lpm-helpers/src/tune.rs          allowlist table + sysfs logic (shared)
    crates/lpm-helpers/src/bin/tune-helper  pkexec target: apply / restore / boost / boot
    crates/lpm-helpers/src/bin/lpm-gamemode Lutris/Steam front end (user process)
    gui/src/optimizetab.{h,cpp}             the tab
    packaging/openrc/lpm-tune               boot preset service

**Rows** (59, grouped CPU · Memory · Scheduler · Storage · Devices · Stability):
all lutris-game-tune parameters (governor/EPP, epp_boost, X3D mode, ASPM, deep
C-states, VM set, MGLRU, THP ×3, split-lock, watchdog, autogroup, CFS slice,
debugfs scheduler knobs, HDA power save, PCI latency timers) and new ones:
amd-pstate mode, boost, min freq = lowest_nonlinear, **SMT**, **CCD parking**
(hot-plug), khugepaged defrag, MGLRU min_ttl_ms, KSM, `vm.max_map_count`,
`numa_balancing`, `timer_migration`, preemption model, **unbound workqueue
cpumask** and **IRQ affinity** by CCD role, I/O scheduler / WBT / read-ahead
per disk, USB autosuspend, amdgpu iGPU DPM level, MCE poll interval.
Rows the machine does not have are shown greyed out ("n/a").

**Per-CCD governor / EPP / boost / max frequency.** amd-pstate has one
cpufreq policy per CPU, so `Governor · CCDn`, `EPP · CCDn`, `Boost · CCDn`
(per-policy `boost`, 6.11+) and `Max frequency · CCDn` write only the
policies whose CPUs sit on that L3 domain; the GUI names each die's role,
e.g. "(V-Cache)". The global `Scaling governor` and `Energy-performance
preference` rows are **hidden on any 2+ CCD chip** — the CCD rows cover the
same files and always apply after where the global row would, so showing
both invited setting one and wondering why the other value stuck. Set both
CCD rows the same for one governor/EPP across the chip, or split them (that's
what Gaming X3D and Competitive do). A single-CCD chip has no CCD rows, so
the global ones are the only way to set this there and stay visible.
`epp_boost` stays global regardless: the patch series has no per-policy
file. EPP's read-only `custom` state is not offered.

**CCD roles instead of CPU lists.** Values like `cache`, `frequency`, `ccd1`
are resolved against the live L3 topology (largest L3 = V-Cache die; highest
`cpuinfo_max_freq`, or the smaller-L3 die on a tie = frequency die), so one
preset works on any X3D part. Symmetric parts only offer `ccdN`; single-CCD
parts hide these rows. cpu0's CCD is never offered for parking.

**Reversibility.** The first write to each concrete file records its
original value (before the write, so a crash mid-batch is still restorable).
"Restore originals", the per-row ↺, the last POST and `rc-service lpm-tune
stop` write them back. Order matters and is fixed by the table: amd-pstate
mode first (it resets per-policy files); SMT and CCD parking last on apply
(an offline CPU's cpufreq policy returns EBUSY) and **first** on restore.

**Presets.** Six built-ins — Gaming X3D, Competitive, Low latency desktop,
Compile throughput, CO validation, Quiet battery — plus your own (Save as…).
A preset stores the checked rows and a `run` block (nice, autogroup,
CCD affinity). Loading skips values the machine does not offer and says which.
★ *Use for games* makes it lpm-gamemode's default; ⏻ *Apply at boot* stores the
checked rows (validated by root) in `/etc/legion-power-manager/tune-boot.json`.

**Lutris** (the Game launch sub-tab has copy buttons):

| Field | Value |
|---|---|
| Pre-game script | `/usr/bin/lpm-gamemode PRE` |
| Post-game script | `/usr/bin/lpm-gamemode POST` |
| Command prefix | `/usr/bin/lpm-gamemode RUN` |
| Steam launch options | `lpm-gamemode WRAP -- %command%` |

Append a preset name to pin one per game (`PRE "Competitive"`). Game mode is
reference counted like lutris-game-tune; WRAP forwards SIGINT/TERM/HUP to the
game so POST still runs when Steam stops it.

Differences from lutris-game-tune, all deliberate:
- **No setuid wrapper.** lpm-gamemode runs as the user; root work goes through
  `pkexec tune-helper`. The RUN boost renices only the process that called
  pkexec, and only if its real uid is the authenticated user (`PKEXEC_UID`).
- **CCD isolation uses affinity + kernel steering, not cgroup moves.** The v4
  cgroup constrain/move logic (and its session watcher) is replaced by
  `sched_setaffinity` inherited by the game tree, unbound workqueue and IRQ
  steering to the other CCD, and optionally parking that CCD. No process is
  ever moved between cgroups, so the elogind/D-Bus incident class cannot recur.
- The X3D `_DSM` write keeps lutris-game-tune's 3 s timeout (thread + abandon).
- PCI latency timers are written natively (one byte at config offset 0x0D via
  pwrite; never a full config write) instead of via `setpci`; per-device
  refusals are skipped like `setpci … || true`.
- THP / shmem / defrag are ordinary rows again (v3 forced `always`).
- State lives on tmpfs: a reboot is a full restore, as before.

Security: the tune-helper request carries only keys and values. Every path is
derived from the table and re-checked before writing (realpath inside /sys,
fixed `/proc/sys` files from the table, `/proc/irq/<n>/smp_affinity_list`);
values are revalidated against live option lists and ranges. The polkit rule
grants it without a prompt like the other helpers — any process running as
the active user can therefore retune the kernel; tighten
`49-legion-power-manager.rules` as described in that file if that matters.

Dev aids: `LPM_TUNE_FAKE=/path/describe.json` renders canned data,
`LPM_OPT_SUBTAB=N` and `LPM_OPT_LOAD=<preset>` pick the sub-tab and preset
for `LPM_SCREENSHOT`.

### Deep-optimization guide

Every row's tooltip already explains what it does, when to touch it, and what
to enter — this section is for combining rows toward a goal, the reasoning a
single tooltip can't carry. All of it applies through **checking rows and
Apply checked**, or by loading/editing one of the six built-in presets
(Preset dropdown → *Load*) and saving your own variant (*Save as…*).

**Chasing 1% lows / frame-time spikes in a specific game.** Start from
*Gaming X3D* and change one thing at a time — this is empirical, not a
formula:
1. `cpu.x3d_mode` = cache, launch affinity (Game launch tab) = the V-Cache
   CCD. This alone is usually the single biggest win on an X3D chip for
   cache-hungry games (open-world, simulation-heavy, emulators).
2. `wq.cpumask` and `irq.affinity` = the *other* CCD, so filesystem/network
   work and interrupts physically cannot land on the game's cores.
3. Try `cpu.smt` = off. Some titles' 1% lows improve with SMT off (no sibling
   cache contention), most don't care — this is genuinely per-game, test it.
4. If still spiky, `cpu.cstate_max` to a shallow state (e.g. `1`) trims
   wake-from-idle jitter at the cost of idle power/heat while gaming.
5. Last resort, not first: `cpu.ccd_park` the non-gaming CCD (*Competitive*
   preset). Total isolation, but you lose those cores entirely until restored
   — only worth it if steps 1–4 didn't get you there.

**A game crashes or hangs specifically under Wine/Proton, not on native
titles.** Check `kernel.split_lock_mitigate` = 0 first (some titles trigger
the split-lock mitigation and stall ~1000× on affected instructions) and
`vm.max_map_count` = 2147483642 (some Proton titles map more regions than
old distro defaults allow and crash on it). Both are already in every gaming
preset; if you built a preset from scratch, these two are the "don't forget"
rows for Proton compatibility specifically.

**Validating a Ryzen Curve Optimizer offset (Ryzen tab) before trusting it.**
Load the *CO validation* preset here first: it keeps boost on and every
C-state enabled (idle→boost transitions are where a marginal core first
misbehaves — you want that path exercised, not avoided), re-enables the
kernel watchdog, and drops the MCE poll interval to 10s. Apply it, then apply
your CO offsets in the Ryzen tab, then stress the affected cores while
watching `dmesg -w` (or rasdaemon if installed) for correctable-error or
watchdog messages. Restore this preset once you're done — it's deliberately
not a tab to leave applied permanently.

**Compiling / long parallel builds (kernel, emerge -j).** *Compile
throughput* trades every latency-favouring row for throughput: EPP
`balance_performance` rather than `performance` (better sustained clocks
under sustained load), `cpu.x3d_mode` = frequency (compilers are usually more
clock-sensitive than cache-hungry), SMT on, no CCD parked, wider scheduler
slices (`sched.preempt` = voluntary, `sched.base_slice_ns` larger), and I/O
scheduler back to `mq-deadline` for fair multi-process disk access instead of
`none`'s raw single-queue latency.

**Battery life on the go.** *Quiet battery*: EPP `power`, boost off, min
frequency at hardware floor rather than the efficient-but-higher
`lowest_nonlinear`, `cpu.cstate_max` = all (never restrict idle depth on
battery), aggressive ASPM (`powersupersave`), HDA and USB power-saving
re-enabled. This preset is the one place a lot of the "always safe" rows from
other presets get deliberately reversed — that's intentional, they trade
latency for power savings which is exactly backwards for gaming but right
here.

**Building your own preset from scratch.** A reasonable order: (1) start
from the closest built-in preset and *Load* it, (2) uncheck what you don't
want, (3) use *Select → Changed* to see at a glance which rows currently
differ from the live system, (4) *Apply checked* and actually use the system
for a while before deciding it's right, (5) *Save as…*. Rows marked ⚠ are the
ones most likely to cost you something (stability, heat, idle power) if
misapplied — read those tooltips before checking them, and prefer testing
them individually rather than as part of a first-time bulk apply.

**Per-CCD splits in general (X3D 2-CCD chips only).** The pattern behind
`Governor · CCDn` / `EPP · CCDn` / `Boost · CCDn` / `Max frequency · CCDn`:
treat the two CCDs as two different machines with two different jobs. The
game's CCD gets the aggressive settings (performance EPP, boost on, no
frequency cap); the other CCD gets efficiency settings (balance_power EPP,
maybe boost off, maybe a frequency cap) since it's mostly idle plus whatever
background work you steered onto it with `wq.cpumask`/`irq.affinity`. None of
these four rows do anything by themselves — they only matter paired with the
launch affinity (Game launch tab) that actually puts the game process on the
CCD you're optimizing for.

### NVIDIA curve: zoom
Zoom slider (1–12×, voltage axis) and pan slider under the graph; the
frequency axis auto-fits to the visible points. Over the graph: wheel zooms at
the cursor, Shift+wheel pans, `+`/`−` zoom, `0` fits. A strip at the top of the
plot shows where the window sits on the whole curve. Dev aid:
`LPM_NVCURVE_VIEW=zoom,pan` with `LPM_NVCURVE_FAKE`.

### systemd
Both init flavours are installed; the running init picks its own files.

    systemctl enable --now nvcurve-autoload.service   # ★ Default GPU profile
    systemctl enable --now lpm-tune.service           # ⏻ Apply at boot preset
    journalctl -u lpm-tune                            # helper's JSON result

`lpm-tune.service` runs after `systemd-sysctl` (so its values win over
`/etc/sysctl.d`) and module loading, before the display manager; `systemctl
stop lpm-tune` restores the originals. `nvcurve-autoload.service` retries for
a while if the NVIDIA device is not up yet. Both are skipped by a
`ConditionPathExists` when no default profile / boot preset is set.
Service files hold `@BINDIR@`/`@LIBEXEC@`; install.sh and the ebuild fill them
in (`PREFIX`, `UNITDIR` are honoured).

## Intel CPU support (step 6)

The CPU vendor is read from `/proc/cpuinfo` (`vendor_id`) by both the GUI and
the helpers (`tune::cpu_vendor()`).

* **Ryzen Curve Optimizer tab** and the tray's *CPU Curve (Ryzen)* menu are only
  created on AMD. On Intel, *Undervolt CPU* in Game launch is disabled and
  `lpm-gamemode` skips the CPU step (an Intel undervolt tool is planned).
* Every tunable carries a vendor (`amd()` / `intel()` in `tune.rs`). Rows of the
  other vendor are left out of `describe` and reported as *not available* on
  apply, so preset files stay portable.
* Intel rows (Optimizations → CPU / Devices): `intel_pstate` mode, HWP dynamic
  boost, max/min perf %, EPB, EPP · P-cores / E-cores, max frequency · P/E,
  uncore min/max (`intel_uncore_frequency`), RAPL PL1/PL2 (MSR + MMIO, watts),
  TCC offset (`intel_tcc_cooling`), iGPU min/max MHz and SLPC profile (i915/xe).
  *Core performance boost* maps to `intel_pstate/no_turbo` (inverted).
* Hybrid topology (`/sys/devices/cpu_core|cpu_atom/cpus`) adds the `pcore` /
  `ecore` roles to launch affinity, unbound workqueue CPUs, IRQ affinity and
  parking (E-cores only; cpu0 is a P-core).
* Built-in presets are vendor-specific: *Intel gaming hybrid*, *Intel
  competitive*, *Intel low latency desktop*, *Intel compile throughput*,
  *Intel quiet battery*.
* Dev aid: `LPM_CPU_VENDOR=amd|intel` forces the GUI's vendor.
