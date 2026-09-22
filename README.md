                        -------------------------------------SCREENSHOTS-----------------------------------------
<img width="1220" height="690" alt="Screenshot_20260922_034609" src="https://github.com/user-attachments/assets/6b400444-672b-42dc-a9ff-a414b326f935" />
<img width="1220" height="690" alt="Screenshot_20260922_034625" src="https://github.com/user-attachments/assets/8b0541e2-ac3b-4b36-9b87-652164f3641a" />
<img width="1220" height="701" alt="Screenshot_20260922_034737" src="https://github.com/user-attachments/assets/8bfbd291-ba1c-4b4b-958a-82979f1416ed" />
<img width="1220" height="844" alt="Screenshot_20260922_034800" src="https://github.com/user-attachments/assets/d61bdcf6-6e61-4c0c-9901-6b079ce9f9fd" />


# Legion Power Manager 2 — Rust + C++/Qt6

Port of the PySide6 Legion Power Manager: privileged work in Rust, GUI in Qt6.

## Install

    sudo ./install.sh                   # build + install to /usr
    sudo ./install.sh --remove-legacy   # …and remove the old Python install
    sudo rc-update add nvcurve-autoload default   # boot-time GPU profile (optional)

Gentoo: `./make-dist.sh` produces `dist/legion-power-manager-2.0.0.tar.xz`
(crates vendored, offline build). Copy it into DISTDIR and use the ebuild in
`packaging/gentoo/sys-power/legion-power-manager/` from a local overlay.

Needs: Rust ≥ 1.75, Qt ≥ 6.4 (Widgets, Network), CMake ≥ 3.19, polkit.
Runtime: NVIDIA proprietary driver (NVIDIA tab), root-owned `ryzenadj` (Ryzen tab).

### Layout

    /usr/bin/legion-power-manager                    GUI + tray
    /usr/bin/nvcurve                                 nvcurve CLI
    /usr/libexec/legion-power-manager/
        legion-profile-helper  fwattr-helper  ryzen-co-helper   (root:root 0755)
        nvcurve-root-helper                                     (root:root 0700)
    /usr/share/polkit-1/actions/com.legion-power-manager.policy
    /etc/polkit-1/rules.d/49-legion-power-manager.rules
    /etc/xdg/autostart/legion-power-manager.desktop  starts in the tray on login
    /etc/init.d/nvcurve-autoload                     OpenRC

Unchanged data locations: `/etc/nvcurve/{config.json,profiles/}` (GPU),
`~/.config/ryzen-curve-optimizer/profiles/` (CPU).

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
