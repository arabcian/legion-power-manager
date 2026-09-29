# Legion Power Manager

**Legion Space / Vantage for Linux — and then some.**
One tray app for Lenovo Legion laptops: power profiles, firmware power limits,
fan curves, CPU and GPU undervolting, per-key RGB, system tuning for games,
and *Scenes* that switch all of it with one click or when you plug in the charger.

Native Qt6 GUI, privileged work in small Rust helpers, no background daemon
required, no telemetry, works on OpenRC and systemd.

> ⚠️ This tool writes low-level hardware and firmware settings.
> Read [DISCLAIMER.md](DISCLAIMER.md) before using it.

## Features

| Tab | What you get |
|---|---|
| **Home** | Power profile (Quiet → Performance → Custom), live sensors (CPU per-CCD, dGPU, iGPU, fans, NVMe, power, battery), fan control and custom fan curve, battery charge mode, GPU mode (Hybrid / dGPU only), Fn-lock, camera, USB charging, memory timings viewer |
| **Scenes** | One name for the whole machine — power profile, firmware limits, CPU/GPU curves, tuning preset, keyboard lighting, a custom command. Automatic AC / battery switching, game-start scene, export / import |
| **Firmware Attributes** | CPU PL1/PL2/PL3, temperature targets, GPU cTGP and Dynamic Boost — including the values the kernel refuses to write |
| **NVIDIA Curve Optimizer** | Drag-and-edit V/F curve, core/memory offsets, named profiles, apply at boot |
| **Ryzen Curve Optimizer** *(AMD)* | Per-core and all-core Curve Optimizer, CPPC-ranked cores, profiles |
| **Intel Undervolt** *(Intel)* | Voltage offsets, IccMax, TCC offset, PL1/PL2, AC/battery profiles, ThrottleStop.ini import, live throttle monitor |
| **Optimizations** | ~80 documented CPU/memory/scheduler/power/storage knobs, weighted, hardware-aware **Autotune** (power saving · gaming · throughput · desktop; see [Autotune](#autotune) below), built-in presets, game launch hooks for Lutris and Steam, boot-parameter advisor, one-click *Restore originals* |
| **Lighting** *(Gen10 Spectrum keyboards)* | Per-key RGB editor on a drawing of your own keyboard, firmware effects, 6 hardware profiles, brightness, lid logo, accent lights |

Everything important is also in the **tray menu**. Every setting has a tooltip
that explains what it does and what value to use.

Built to be safe to experiment with: root helpers validate every value,
a **boot guard** pauses boot-time presets after a crash, the login scene has
the same protection, and firmware-persistent changes (BIOS memory timings,
BIOS CPU OC, GPU MUX) always ask for the administrator password.

## Supported hardware

**Lenovo Legion, LOQ and IdeaPad Gaming laptops only.** The program checks the
machine at start (DMI vendor and model name) and refuses to run anywhere else —
the GUI shows why and exits, and every root helper refuses before touching
hardware, because the firmware and EC settings it writes are Lenovo-specific.

- **Developed and tested on:** Lenovo Legion Pro 7 16AFR10H (Ryzen 9 9955HX3D, RTX 5080).
- **Should work on:** other Legion / LOQ models whose kernel exposes
  `lenovo-wmi-gamezone`, `lenovo-wmi-other` and `ideapad_laptop` — tabs and
  rows only appear when the hardware and driver behind them exist.
- **Model-specific (16AFR10H BIOS):** custom fan curve, BIOS memory timings,
  GPU power limits via WMI, GPU MUX switch.
- **Lighting tab:** Legion Gen10 keyboards with the Spectrum controller
  (USB `048d:c1xx`). Other Legion keyboards are planned.
- Intel Legion support has been checked against a Core Ultra 7 255HX.

Reports from other models are very welcome — open an issue with your model
number (e.g. `83F5`) and what worked.

## Install

### From source

```sh
git clone <this repository> legion-power-manager
cd legion-power-manager
sudo ./install.sh
```

`install.sh` builds everything tuned for this machine (native CPU, LTO, PGO,
hardening) and installs to `/usr`. Options: `--no-native` for portable
binaries, `--no-lto`, `--no-pgo`, `--no-harden`, `--remove-legacy` to remove
the old Python version, `--security-level=N` (see [Security levels](#security-levels)). Remove with `sudo ./uninstall.sh`.

To build with clang/LLVM instead of GCC: `sudo ./install-clang.sh` (same options;
uses lld when installed and `llvm-profdata` for PGO).

### Gentoo

```sh
./make-dist.sh    # → dist/legion-power-manager-2.0.0.tar.xz (crates vendored, builds offline)
```

Copy the tarball into your `DISTDIR` and emerge the ebuild from
`packaging/gentoo/sys-power/legion-power-manager/` in a local overlay.

### After installing

1. Log out and back in once (the keyboard lighting udev rule and polkit rules take effect).
2. Start **Legion Power Manager** from the menu — it lives in the tray
   (`legion-power-manager --window` opens the window directly). It also starts
   automatically at login.
3. Optional boot-time services — enable only the ones you use:

   | Service | Applies at boot |
   |---|---|
   | `lpm-boot-guard` | crash protection for everything below (recommended) |
   | `nvcurve-autoload` | the ★ default GPU curve |
   | `lpm-tune` | the ⏻ Optimizations boot preset (OpenRC runlevel `boot`) |
   | `lpm-intel-uv` / `lpm-intel-uv-daemon` | Intel undervolt (Intel only) |

   ```sh
   rc-update add nvcurve-autoload default     # OpenRC
   systemctl enable nvcurve-autoload          # systemd
   ```

   Services do nothing until you set a default / boot preset in the GUI.

## Quick start

- **Change the power profile:** Home → pick a card, or tray → *Power Profile*.
- **Undervolt the GPU:** NVIDIA tab → *Read Current Curve* → set an offset or drag
  points → *Apply Offsets* → *Save As…* → ★ *Default* to apply it at boot.
- **Undervolt the CPU:** Ryzen tab → all-core or per-core offset → *Apply* → save a profile.
- **Tune for games:** Optimizations → choose a preset → *Load* → *Apply checked*;
  ★ *Use for games* and paste the shown hook into Lutris / Steam.
- **One-click setups:** Scenes → *New…* → pick a profile for each component →
  *Save*. Turn on *Switch scenes with the power source* for AC / battery.
- **Keyboard colours:** Lighting → select keys (click, drag, Ctrl-click) →
  *Paint selection* → *Apply to profile*.

Nothing is applied permanently by accident: tuning can always be undone with
*Restore originals*, curves with *Reset*, lighting with *Factory reset profile*.

## Autotune

Optimizations → **⚙ Autotune** profiles the machine (CPU topology and V-Cache,
cpufreq driver, RAM, swap, storage, kernel, boot parameters, memory and I/O
pressure history) and fills the tab with a preset for the chosen goal. Nothing
is written until you press *Apply checked*.

- **Anchored at boot defaults.** The boot-time values (kernel + distro + your
  sysctl, before TLP) are the reference; settings move at most 2× from them
  unless evidence justifies more, and settings the pressure guard had to roll
  back are penalised, then retired. Enable `lpm-boot-guard` for the snapshot.
- **Weighted, not "bigger is better".** Every setting with a trade-off is
  scored over latency, throughput, power, memory footprint and stability; a
  value is written only if it clearly beats leaving the setting alone. Each goal
  has default weights; **Weights…** changes them per goal (also
  `lpm-autotune <goal> --weights latency=1.2,footprint=0.8`).
- **Measured, bounded memory settings.** `vm.dirty_bytes` /
  `dirty_background_bytes` come from the disk's sustained write rate (about
  1 s / 0.25 s of writes, at most 2 % of RAM / 1 GiB). Watermark, reserve and
  THP/khugepaged values stay inside RAM-derived limits, never use a
  combination the kernel rejects, and rise only when reclaim evidence exists.
- **Boot parameters win** (`usbcore.autosuspend=`, `pcie_aspm=`,
  `transparent_hugepage=`); `kernel.watchdog` is never disabled.
- **Safety net.** tune-helper refuses values that fail the audit (e.g.
  `dirty_bytes` of a few KiB), and after applying memory/writeback settings a
  120 s pressure guard restores them if I/O or memory stalls persist.
- **64-bit values** are edited and stored without truncation.

```sh
lpm-autotune desktop                 # show a preset (powersave|gaming|throughput|desktop)
lpm-autotune gaming --save           # save it as "Auto Gaming"
sudo lpm-autotune probe              # measure disk write speed (<= 512 MiB, <= 4 s)
lpm-autotune audit [--fix]           # find/repair unsafe values in scenes and presets
lpm-autotune report [SECONDS]        # memory, THP, writeback, PSI, vmstat deltas
sudo lpm-calibrate [--budget MIN]    # build the machine signature: every CPU/scheduler/memory knob, idle + under load
```

After updating from an older version run `lpm-autotune audit --fix`, then
re-save your scenes so the root preset store is rewritten. Details:
[docs/AUTOTUNE.md](docs/AUTOTUNE.md).

## Security levels

Chosen at install time (`--security-level=N` or asked interactively):

| Level | Behaviour |
|---|---|
| 1 (default) | local, active `wheel` user runs helpers without a password |
| 2 | every helper asks for the admin password (polkit caches it briefly) |
| 3 | as 2, except applying an already-approved Optimizations preset by name |

Firmware-persistent changes always ask, at every level.

## Troubleshooting

- **Freezes or instability after applying a preset:** *Restore originals* in
  Optimizations; then `lpm-autotune audit` to look for unsafe values. Boot
  presets are paused automatically after a crash (`lpm-boot-guard`).
- **High idle memory use:** `lpm-autotune report` (look at `AnonHugePages`,
  watermarks, `MemAvailable`).
- **Tab or row missing:** the hardware, kernel driver or tool behind it is
  absent — see Requirements.
- **Health tab** keeps the kernel-log history (Xid, MCE, AER, lockups, guard
  rollbacks) across boots.

## Requirements

**Build:** Rust ≥ 1.75 (cargo), a C++20 compiler, CMake ≥ 3.19, Qt ≥ 6.4
(Widgets, Network).

**Runtime:**

| Needed for | Dependency |
|---|---|
| everything | polkit (`pkexec`), Linux with `platform_profile` |
| firmware limits, fans, battery, device toggles | kernel drivers `lenovo-wmi-gamezone`, `lenovo-wmi-other`, `ideapad_laptop` |
| GPU power limits, fan curve, GPU mode, instant boot | `acpi_call` kernel module |
| NVIDIA tab | proprietary NVIDIA driver (NvAPI / NVML are loaded at runtime) |
| Ryzen tab | root-owned `ryzenadj` in `/usr/bin`, `/usr/sbin`, `/usr/local/{bin,sbin}` or `/opt/ryzenadj` |
| live memory timings, extra CPU sensors *(optional)* | `ryzen_smu`, `zenpower` / `zenergy` |
| BIOS memory timings | efivarfs (`/sys/firmware/efi/efivars`) |
| Lighting tab without a password | udev + systemd-logind or elogind (`uaccess`) |

Missing pieces only disable the tab or row that needs them.

## How it works

The GUI runs as your user and never touches hardware directly. Each kind of
change goes through a small Rust helper in `/usr/libexec/legion-power-manager/`
that reads one JSON request, validates it against fixed paths and live
kernel ranges, and exits. polkit lets a `wheel` user at the machine run them
without a password; firmware-persistent changes always ask. The keyboard
lighting helper normally runs as you, through a udev `uaccess` rule.

More: [docs/TECHNICAL.md](docs/TECHNICAL.md) — every tab in detail, the
tuning guide, file locations, services and design notes.

## Credits and inspiration

- **[Lenovo Legion Toolkit](https://github.com/LenovoLegionToolkit-Team/LenovoLegionToolkit)**
  (GPL-3.0) — the reference for Legion hardware on Windows. The Spectrum
  keyboard protocol, the keyboard ID table and the lighting effect rules were
  learned from its source; the code here is an independent Rust / C++
  implementation. Thank you to Bartosz Cichecki, the LenovoLegionToolkit-Team
  and every LLT contributor.
- **[legion-spectrum-control](https://github.com/alstergee/legion-spectrum-control)**
  (MIT) — key legends for the Lighting tab; confirmed the Gen10 keyboard layout.
- **Lenovo Legion Space / Vantage** — the feature set this app brings to Linux.
- **The Linux kernel `lenovo-wmi-*` and `ideapad_laptop` drivers**, **RyzenAdj**,
  **ThrottleStop**, **intel-undervolt** and **throttled** — for the interfaces
  and ideas the CPU tabs build on.
- **[LenovoLegionLinux](https://github.com/johnfanv2/LenovoLegionLinux)** — for
  paving the way for Legion support on Linux.

Third-party license texts: [NOTICE](NOTICE).

## License

Copyright © 2026 arabcian.
Free software under the **GNU General Public License v3.0 or later** — see
[LICENSE](LICENSE). No warranty; see [DISCLAIMER.md](DISCLAIMER.md).
