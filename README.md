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
| **Optimizations** | ~60 documented kernel/scheduler/memory/storage knobs, built-in presets, game launch hooks for Lutris and Steam, boot-parameter advisor, one-click *Restore originals* |
| **Lighting** *(Gen10 Spectrum keyboards)* | Per-key RGB editor on a drawing of your own keyboard, firmware effects, 6 hardware profiles, brightness, lid logo, accent lights |

Everything important is also in the **tray menu**. Every setting has a tooltip
that explains what it does and what value to use.

Built to be safe to experiment with: root helpers validate every value,
a **boot guard** pauses boot-time presets after a crash, the login scene has
the same protection, and firmware-persistent changes (BIOS memory timings,
BIOS CPU OC, GPU MUX) always ask for the administrator password.

## Supported hardware

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
