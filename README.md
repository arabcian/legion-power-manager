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
| **Home** | Power profile (Power Saver → Extreme → Custom), live sensors (CPU per-CCD, dGPU, iGPU, fans, NVMe, power, battery), battery charge mode, GPU / iGPU mode, Fn-lock, camera, USB charging, firmware toggles (boot on AC / USB-PD, Custom in Fn+Q), memory timings viewer and BIOS timing editor. Banners for a paused boot preset, login scene or paused Scenes, each with a one-click resume |
| **Fans** *(on Home)* | Per-fan Auto / Max / target RPM, *Max all fans*, custom fan curve editor, EC Full Speed (Turbo fan). Full Speed only drives the fans in **Custom**, so in other profiles it neither shows a notice nor locks the fan controls; while Max or Full Speed is active the status and a *Disable* button sit in the fan button row, with no layout shift |
| **Scenes** | One name for the whole machine: power profile, firmware limits, CPU/GPU curves, tuning preset, keyboard lighting, a custom command. Automatic AC / battery switching, game-start scene, pause / resume, export / import |
| **Firmware Attributes** | CPU PL1/PL2/PL3, temperature targets, GPU cTGP and Dynamic Boost, including the values the kernel refuses to write (written over Lenovo WMI). BIOS CPU overclocking (PBO scalar, boost override, all-core CO) with a single *Apply* that writes every changed value in one password prompt |
| **NVIDIA Curve Optimizer** | Drag-and-edit V/F curve, per-point and global offsets, *Flatten after index*, memory offset, VRAM clock lock, PowerMizer, named profiles, apply at boot |
| **AMD GPU** | Overclock / undervolt / efficiency for any amdgpu card (Radeon dGPU or the iGPU): the form follows what the driver exposes per generation (state table, V/F curve, min/max clocks + voltage offset, RDNA4 clock offset), plus performance level, power profile, power limit, PMFW fan curve and profiles. Every Apply is a trial that reverts unless you press *Keep* |
| **Ryzen Curve Optimizer** *(AMD)* | Per-core and all-core Curve Optimizer, CPPC-ranked cores, profiles |
| **Intel Undervolt** *(Intel)* | Voltage offsets, IccMax, TCC offset, PL1/PL2, AC/battery profiles, ThrottleStop.ini import, live throttle monitor |
| **Optimizations** | ~60 documented kernel/scheduler/memory/storage knobs in striped rows, built-in presets, game launch hooks for Lutris and Steam (nice, autogroup, CCD affinity), boot-parameter advisor, one-click *Restore originals* |
| **Lighting** *(Gen10 Spectrum keyboards)* | Per-key RGB editor on a drawing of your own keyboard, firmware effects, 6 hardware profiles, brightness, lid logo, accent lights |

Everything important is also in the **tray menu**. Every setting has a tooltip
that explains what it does and what value to use.

Built to be safe to experiment with: root helpers validate every value,
a **boot guard** pauses boot-time presets after a crash, the login scene has
the same protection, and firmware-persistent changes (BIOS memory timings,
BIOS CPU OC, GPU MUX) always ask for the administrator password.

### Behaviour worth knowing

- **cTGP range follows the GPU.** 5 W up to the vBIOS max power limit minus the
  25 W Dynamic Boost headroom (RTX 5080 Laptop: 175 − 25 = 150 W). It is read
  from `nvidia-smi` while the dGPU is awake and cached in
  `/var/cache/legion-power-manager/ctgp_max`, so the range stays right while the
  dGPU sleeps. Before the first reading the ceiling is 150 W.
- **V/F curve writes are complete.** Every apply writes every GPU-domain point
  (points not in the profile are written as 0), so an earlier flatten can never
  leave stale offsets behind. There is no NVML core-clock cap: a flattened curve
  holds its top by itself, and any lock left by older versions is released on apply.
- **Flatten on a power-limited laptop** only matters above the voltage the GPU
  actually reaches. Check the *current* marker in `nvcurve read` under load; to
  save power, flatten at or below that point.
- **CCDs are selected by index.** Affinity, workqueue / IRQ steering and CCD
  parking list each CCD once, labelled with its role
  (`CCD0 V-Cache`, `CCD1 frequency`). Older presets using `cache` / `frequency`
  are mapped to the matching CCD automatically.
- **Live memory timings** wait up to 3 s for `ryzen_smu`'s SMN node after the
  module loads, and say so if the module is loaded but not bound yet.

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
hardening) and installs to `/usr`. Remove with `sudo ./uninstall.sh`.

| Option | Effect |
|---|---|
| `--no-native` | portable binaries (no `-march=native` / `target-cpu=native`) |
| `--no-lto` | no link-time optimization for the GUI (Rust always uses fat LTO) |
| `--no-pgo` | skip the profile-guided GUI build (default: instrumented build, offscreen training run over every tab, final build with the profile) |
| `--no-harden` | drop stack protector / FORTIFY=3 / CET / full RELRO / PIE on the GUI |
| `--clang` | build with clang/LLVM (same as `./install-clang.sh`) |
| `--remove-legacy` | also remove the old Python version |
| `--no-build` | install already-built files (with `DESTDIR=` for staging) |

#### clang / LLVM

```sh
sudo ./install-clang.sh            # accepts every install.sh option
```

The GUI is built with `clang++`, linked with `lld` when installed, and PGO
profiles are merged with `llvm-profdata`. The Rust helpers are linked with
clang (and lld). Tools are looked up in `PATH`, then `/usr/lib/llvm/<N>/bin`
(newest first, as on Gentoo, where `sudo` drops that directory from `PATH`),
then as `tool-<N>`. Build directories configured with the other compiler are
removed automatically, so switching between GCC and clang needs no cleanup.
Without `llvm-profdata` the build continues without PGO.

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
- **Undervolt / overclock the GPU:** NVIDIA tab → *Read Curve* → set an offset or drag
  points (optionally *Flatten after index*) → *Apply Offsets* → *Save As…* →
  ★ *Default* to apply it at boot.
- **Undervolt the CPU:** Ryzen tab → all-core or per-core offset → *Apply* → save a profile.
- **Tune for games:** Optimizations → choose a preset → *Load* → *Apply checked*;
  ★ *Use for games* and paste the shown hook into Lutris / Steam.
- **One-click setups:** Scenes → *New…* → pick a profile for each component →
  *Save*. Turn on *Switch scenes with the power source* for AC / battery.
- **Keyboard colours:** Lighting → select keys (click, drag, Ctrl-click) →
  *Paint selection* → *Apply to profile*.

Nothing is applied permanently by accident: tuning can always be undone with
*Restore originals*, curves with *Reset*, lighting with *Factory reset profile*.

## Requirements

**Build:** Rust ≥ 1.75 (cargo), a C++20 compiler (GCC or clang), CMake ≥ 3.19,
Qt ≥ 6.4 (Widgets, Network). Optional for `--clang`: `lld`, `llvm-profdata`.

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
