Git address : https://github.com/arabcian/legion-power-manager


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

The window has one tab per area. Tabs appear only when your hardware and
drivers support them, so you never see controls that can't work.

### Home
Your everyday dashboard.
- **Power profile** — Quiet, Balanced, Performance or Custom, switched instantly.
- **Live sensors** — CPU (per CCD), NVIDIA GPU, integrated GPU, fans, NVMe,
  power draw and battery.
- **Fans** — fan control and, on supported models, a **custom fan curve** editor
  (10 levels, each fan at its own temperature steps).
- **Battery** — charge mode (e.g. conservation / rapid charge).
- **Device** — GPU mode (Hybrid or dGPU only, takes effect after reboot),
  integrated GPU memory mode, Fn-lock, camera, USB charging while off,
  *Boot on AC* and *Boot on USB-C PD* (instant boot).
- **Memory** — view your RAM: DDR5 SPD data of every module, live timings from
  the memory controller, and (on supported BIOS) an editor for **BIOS memory
  timings** with automatic backup and restore.
- **Boot guard status** — *Resume boot presets* after a crash (see below).

### Scenes
One click (or a charger plug) sets the whole machine.
- A scene bundles: power profile, firmware limits, CPU curve, NVIDIA curve,
  Optimizations preset, keyboard lighting and an optional custom command.
  Any part can be left *Unchanged*.
- Scenes point at your saved profiles, so editing a profile updates every scene
  that uses it.
- **Automatic switching** between an AC scene and a battery scene, and at login.
- **Game-start scene** — switch when a game starts, switch back when it exits.
- **Export / import** everything you built (scenes, presets, CPU/GPU profiles)
  as one file — for backups, reinstalls or sharing with owners of the same model.

### Firmware
Firmware power and temperature limits (in the Custom profile).
- CPU PL1 / PL2 / PL3 and temperature targets.
- NVIDIA cTGP and Dynamic Boost — including the values the kernel refuses to write.
- BIOS CPU overclock settings: PBO scalar, boost clock, firmware Curve Optimizer
  (always asks for the administrator password).

### Ryzen Curve *(AMD CPUs)*
CPU undervolting with Curve Optimizer.
- One offset for all cores, or per-core offsets grouped by CCD.
- Best cores (CPPC ranking) are marked with ★.
- Named profiles, available from the tray too.

### Intel Undervolt *(Intel CPUs)*
- Voltage offsets for core, cache, iGPU, system agent and analog I/O.
- IccMax, TCC offset, PL1 / PL2, BD PROCHOT, cTDP.
- Separate AC and battery profiles, re-applied at boot and after sleep.
- ThrottleStop.ini import and a live throttle / voltage / power monitor.
- Also usable from the terminal: `lpm-intel-uv`.

### NVIDIA Curve
GPU undervolting and overclocking.
- Drag-and-edit **V/F curve**, or simple core / memory clock offsets.
- Extra sensors GeForce drivers normally hide: **hotspot** and **VRAM temperature**,
  plus the reason the GPU is currently throttling.
- PowerMizer mode and a one-click *Reset All*.
- Named profiles; mark one ★ *Default* to apply it at every boot.
- **Driver options** — a curated, validated set of NVIDIA kernel module options.
- Also usable from the terminal: `nvcurve`.

### AMD GPU *(Radeon or AMD integrated graphics)*
- Clock / voltage tuning adapted to your GPU generation (Polaris to RDNA4).
- Power limit, performance level, power profile and RDNA3+ fan settings.
- **Safe trial apply** — changes are reverted after 20 s unless you press *Keep*.
  *Undervolt step* lowers voltage 10 mV at a time until you find the stable limit.

### Optimizations
System tuning for gaming, battery life or throughput.
- About 80 documented knobs: CPU, memory, scheduler, power, storage, IRQs.
- **Autotune** — detects your hardware and builds a preset for power saving,
  gaming, throughput or desktop use (also `lpm-autotune`, see
  [docs/AUTOTUNE.md](docs/AUTOTUNE.md)).
- Built-in presets, your own presets, and a boot-time preset.
- **Game launch hooks** for Lutris and Steam: apply a preset while a game runs,
  and optionally give the game its own CCD (V-Cache or frequency CCD).
- **Boot options advisor** — shows useful kernel / NVIDIA options and builds a
  line you can copy. It never edits your bootloader.
- *Restore originals* puts every value back.

### Lighting *(Legion Gen10 Spectrum keyboards)*
- Per-key RGB editor on a drawing of your own keyboard (click, drag, Ctrl-click).
- Firmware effects: static, pulse, wave, smooth, rain, ripple, type lighting,
  rainbow wave / spiral — with speed, direction and colours.
- The keyboard's 6 hardware profiles (shared with Windows), brightness,
  lid logo and accent lights.

### Health
Find hardware and driver problems quickly.
- Watches the kernel log for faults since boot: NVIDIA Xid errors (with what
  they mean), GSP timeouts, machine checks, PCIe errors, lockups, amdgpu hangs.
- Critical events show up as a tray notification.
- **Tool pages** — the output of common system tools (sensors, lscpu, lspci,
  dmidecode, smartctl…) in one place, with an option to hide serial numbers.

### Tray and safety
- Everything important is also in the **tray menu**: power profile, scenes,
  CPU / GPU curves, keyboard lighting.
- Every setting has a tooltip explaining what it does and what value to use.
- Root helpers validate every value before writing.
- **Boot guard** pauses boot-time presets if the machine crashed while one was
  active; the login scene has the same protection.
- Firmware-persistent changes (BIOS memory timings, BIOS CPU OC, GPU MUX)
  always ask for the administrator password.

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
the old Python version. Remove with `sudo ./uninstall.sh`.

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
