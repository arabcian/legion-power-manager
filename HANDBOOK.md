# Legion Power Manager — Handbook

A short, task-by-task guide. For what each feature is, see the
[README](../README.md); for internals, [TECHNICAL.md](TECHNICAL.md) and
[AUTOTUNE.md](AUTOTUNE.md). Hover any setting in the GUI — every row has a tooltip.

**Golden rules**

1. Change one thing at a time, test, then save it as a profile.
2. Everything has an undo: *Restore originals*, *Reset*, *Factory reset*, *↺ Return to boot-guard defaults*.
3. Firmware-level changes (BIOS memory timings, BIOS CPU OC, GPU MUX) ask for the admin password — read the warning first.

---

## 1. First ten minutes

1. Install ([README → Install](../README.md#install)), log out and in once.
2. Start the app from the menu. It sits in the **tray**; closing the window hides it, *Quit* in the tray menu exits.
3. Enable the boot services you want (`lpm-boot-guard` first — it protects the rest):
   ```sh
   rc-update add lpm-boot-guard default     # OpenRC
   systemctl enable lpm-boot-guard          # systemd
   ```
4. Open **Home**, pick a power profile, look at the *Live* box to see sensors work.
5. Open **Scenes → New…**, press *Capture current*, name it, *Save*. You now have a restore point for the whole machine.
6. Health → Backup → *LPM configuration* → make a first backup.

## 2. The window and the tray

| Where | What |
|---|---|
| Tabs | Home · Scenes · Firmware · Ryzen / Intel · NVIDIA · AMD GPU · Optimizations · Lighting · Health (only those your machine supports) |
| Tray icon | Click: show / hide the window. Right-click menu: Scene, Power Profile, CPU Curve, GPU Curve, Optimizations, Keyboard Lighting, Fans, Show/Hide, Quit |
| Second launch | Just shows the running window |

Use the tray for daily switching; use the tabs when you tune.

## 3. Home

- **Power profile** — click a card. Applies instantly. *Custom* unlocks firmware limits and the fan curve.
- **Live box** — temps, fans, power, battery. Updates only while the Home tab is visible (saves power). The dGPU row shows *asleep* or its temperature without waking it.
- **Device box** — battery charge mode, GPU mode (Hybrid / dGPU only, takes effect next boot), iGPU-only mode, Panel Overdrive, Fn lock, camera, USB charging when off, Instant Boot, Custom in Fn+Q.
  - **iGPU-only mode** unloads the NVIDIA driver. If the dGPU does not return, press **Bring NVIDIA back**.
- **Fans** — per fan: *Auto*, *Max*, or set a value and press *Set*. **Turbo fan** = EC full speed. **Fan curve…** edits the table used in Custom mode (AC power).
- **Memory timings** — *Reload* reads live timings; *Edit timings…* → *Write to BIOS* (admin password; applies next boot; *Restore previous…* undoes it). Wrong timings can stop the laptop from booting — change small steps only.
- **Boot guard box** — appears when a crashed boot paused presets. Fix the cause (usually an undervolt that is too deep), then *Resume boot presets* / *Resume login scene*.

## 4. Scenes

A scene is a **name for a whole setup**. It stores power profile, firmware limits, CPU curve, NVIDIA curve, Optimizations preset, lighting, fan boost, Custom-mode fan curve and an optional command. Each part can stay *Unchanged*.

**Make one:** Scenes → *New…* → *Capture current* (copies firmware limits and the fan table) → pick profiles per component → *Save*. *Apply* tests it.

**Automatic switching:** tick *Switch scenes with the power source*, choose a scene for AC and for battery. The tray has *Pause scenes* — while paused nothing is written automatically, even on plug / unplug.

**Game start:** Optimizations → Game launch → *System at game start*: the first game switches to the chosen scene, the last exit switches back.

**Scripts:** `lpm-gamemode SCENE "Name"`. Export / import scenes from the Scenes tab.

## 5. Firmware tab

Needs **Custom** profile for most limits.

- **CPU PL1 / PL2 / PL3** — sustained / short boost / peak power. Raise in small steps; watch temperature in Home.
- **Temperature target** — where the CPU starts to throttle.
- **GPU cTGP / Dynamic Boost** — GPU power and the boost ceiling / floor. Written over WMI (needs `acpi_call`).
- **CPU overclocking (firmware, next boot)** — PBO scalar, boost override, all-core CO stored in the BIOS. Asks for the password.

## 6. Undervolting

### NVIDIA GPU (NVIDIA tab)
1. *Read Curve*.
2. Quick way: set the **Core offset** (negative = undervolt) → *Apply Offsets*. Detailed way: drag or select points on the graph (*Fit* shows all, *Reset Graph* drops unapplied edits).
3. Stress test a game. Stable → *Save As…*; add ★ **Default** to apply at boot (needs `nvcurve-autoload`).
4. Other controls: **Lock / Unlock** the VRAM clock, **Cap / Uncap** the GPU core clock, **Driver options…** (NVIDIA module parameters). *Reset Curve* / *Reset All* go back to stock.

### Ryzen CPU (Ryzen tab)
- **All-core:** enter an offset (negative = undervolt) → *Apply All-Core*.
- **Per-core:** enter offsets per core (CPPC-ranked, one column per CCD) → *Apply Per-Core*. Each CCD's *Fill* sets every enabled core of that CCD to one value; *Clear* empties the fields.
- *Save* a profile; the tray *CPU Curve* menu applies it. *Reset Curve Optimizer* clears all offsets.
- Crash or reboot after applying → lower the offsets (e.g. −5 steps).

### Intel CPU (Intel Undervolt tab)
*Read current* → tick the values to write → *Apply*. If the readback differs, the BIOS locks undervolting (*Test UV lock*). *⏻ Apply at boot && resume* stores it; separate AC / battery profiles; *Import ThrottleStop.ini…*.

### AMD GPU (AMD GPU tab)
Change values → *Apply*. It is a **trial**: unless you press **Keep** within 20 s, the old settings return. *Undervolt step −10 mV* repeats until a game crashes; keep the last step that held. Nothing is applied at boot, so a hang is fixed by rebooting. Needs `amdgpu.ppfeaturemask` bit `0x4000` (the tab shows the exact value).

## 7. Optimizations

### 7.1 Presets
Top bar: choose a preset → **Load** → review the checked rows → **Apply checked**.
**Save as…** keeps your own preset. **Restore originals** undoes everything applied this session.
**↺ Return to boot-guard defaults** pauses Scenes and rewrites all knobs to the values captured at boot — use it as a clean baseline.

Rows are grouped: **CPU · Memory · Scheduler · Storage · Network · Devices · Power · Stability**. The number on each sub-tab is how many rows your machine supports.

### 7.2 Autotune (let the program choose)
1. Optional but recommended: enable `lpm-boot-guard` and reboot once (it records the boot defaults).
2. Press **⚙ Autotune** → pick a goal: *power saving*, *gaming*, *throughput*, *desktop*.
3. Review the filled rows (changed rows are checked). **Weights…** shifts priorities (latency, throughput, power, footprint, stability).
4. **Apply checked**. A 120 s guard rolls memory settings back if the system stalls.

### 7.3 Calibration (teach the program your machine)
Optional, improves Autotune. 15 minutes, do not use the PC meanwhile.
```sh
# Pause Scenes, press "↺ Return to boot-guard defaults" (or reboot), then:
sudo lpm-calibrate --budget 15
lpm-calibrate --show        # what was learned
```
Results accumulate in `/var/lib/legion-power-manager/signature.json`. After a BIOS / hardware change the old signature is set aside automatically.

### 7.4 Games
**Game launch** sub-tab:
1. Choose a preset and press **★ Use for games**.
2. Steam → game → Launch options:
   `lpm-gamemode WRAP -- %command%`
3. Lutris → Configure → System options:
   - Pre-launch script: `/usr/bin/lpm-gamemode PRE`
   - Post-exit script: `/usr/bin/lpm-gamemode POST`
   - Command prefix (optional launch boost): `/usr/bin/lpm-gamemode RUN`
4. Pin a preset per game by appending its name, e.g. `PRE "Competitive"`.

Game mode is reference-counted: the first game applies, the last exit restores. If the preset is not approved yet: `lpm-gamemode APPROVE "Name"`.

### 7.5 Boot options
Shows kernel parameters that help (ASPM, THP, amdgpu mask…) as copy-paste text. Nothing edits your bootloader.

### 7.6 Apply at boot
**⏻ Apply at boot** makes the selected preset the boot preset (needs `lpm-tune`). **Clear boot** removes it.

## 8. Lighting (Gen10 Spectrum keyboards)

1. Pick one of the 6 hardware **profiles**.
2. Select keys: click, drag, Ctrl / Shift to add.
3. **Paint selection** (static colour) or add an **effect** (colour change, pulse, wave, rain, ripple, type lighting, rainbow…) with speed, direction, colours.
4. **Apply to profile** writes it to the keyboard (limit: 960 bytes, shown under the editor). *Revert* discards edits; *Factory reset profile* restores defaults.
5. **Brightness (0–9)** and **Lid logo** apply immediately. Tray: *Keyboard Lighting*. Scenes can set profile + brightness.

Apply when finished — do not apply in a loop (writes go to non-volatile memory).

## 9. Health

### Monitor
*Scan now* lists NVIDIA Xid / GSP, MCE, AER, lockups and guard rollbacks with uptime. Events are saved across boots — **Open saved log** / **Clear saved log**. Use it first when games crash after tuning.

### dGPU power
Why the NVIDIA GPU is awake: runtime-PM state, sibling functions, upstream port, open handles. **Copy report** for bug reports.

### Network
Needs the `lpm-netguard` service.
- **Connections** — live sockets with the program behind them. Right-click: *Details…*, *Kill connection*, *Whitelist program*, *IP info*, blacklist options.
- **Block non-whitelisted Wine / .exe programs** — a Windows program not on the whitelist is cut off at its first packet and listed in **Blocked**. Whitelist it from the right-click menu or **Rules**. **LAN exempt** leaves private addresses alone.
- **Rules** — *Whitelist* (path, folder or name) and *Blacklist* (IP, CIDR `198.51.100.0/24`, range `192.0.2.10 - 192.0.2.80`). **Add many…** bans a pasted list as one group you can remove with one click. Right-click an address: blacklist the IP, its subnet, or the owner's whole network (from whois).
- **Log connections** — on: every new connection to and from the machine goes to **Log** (and `/var/log/legion-power-manager/connections.log`). Off: nothing is logged.
- **IP info** — reverse DNS + whois.

### Tool pages
Sensors · CPU · Memory · DMI · PCI · PCIe detail · USB · Storage · SMART · NVIDIA · Graphics · Kernel · Battery. Each runs the matching system command and shows the output (DMI, PCIe detail, SMART need the admin password; NVIDIA and Graphics may wake the dGPU).

### Kernel log
Warnings or worse, kept across boots.

### Backup
- **LPM configuration** — scenes, presets, curves, lighting, boot profiles, network rules, calibration. Optional *System profile* (Portage config, kernel config, fstab, package list — reference only) and *Logs*. Saving needs no password; restoring the root-owned part does. A safety copy is made before every restore.
- **System image** — choose folder and compressor, options *Low priority*, *Verify*, *Include /home*, retention. Writes `backup-DATE.tar.gz` of `/`. **Restore** to any target path (type `RESTORE` for `/`). Admin password every time.

## 10. Command-line cheat sheet

```sh
legion-power-manager --window            # open the window
lpm-gamemode STATUS                      # game-mode state
lpm-gamemode SCENE "Gaming"              # apply a scene from a script
lpm-gamemode RESTORE                     # undo game tuning
lpm-autotune gaming                      # preview a preset
lpm-autotune gaming --save               # save as "Auto Gaming"
lpm-autotune audit --fix                 # repair unsafe saved values
lpm-autotune report 30                   # memory / PSI over 30 s
sudo lpm-calibrate --budget 15           # measure the machine
lpm-boot-guard status                    # boot-guard state (sudo … reset to clear)
lpm-netguard status | list | whois 1.2.3.4
sudo lpm-intel-uv read                   # Intel undervolt state
nvcurve sensors                          # hotspot / VRAM temps, throttle reasons
sudo nvcurve reset-all                   # everything NVIDIA back to stock
```

## 11. When something goes wrong

Go down the list until it is fixed:

1. **Odd behaviour after tuning** → Optimizations → *Restore originals*.
2. **Still odd** → *↺ Return to boot-guard defaults*, then `lpm-autotune audit --fix`.
3. **Crash on boot** → boot-guard pauses presets automatically; fix the offending undervolt / curve, then *Resume boot presets* on Home (or `sudo lpm-boot-guard reset`).
4. **Bad GPU curve** → NVIDIA tab → *Reset All*; or `sudo nvcurve reset-all` from a console.
5. **Bad CPU curve** → Ryzen → *Reset Curve Optimizer* (tray: *CPU Curve → Reset curve*).
6. **dGPU gone after iGPU-only** → Home → *Bring NVIDIA back*, or reboot.
7. **BIOS setting changes by itself** → read `/var/log/legion-power-manager/writes.log` to see what LPM wrote.
8. **Laptop will not boot after BIOS memory timings** → BIOS recovery / reseat RAM; this is why that editor asks for confirmation — change small steps.
9. **Restore a known-good setup** → Health → Backup → *LPM configuration* → restore.

## 12. Glossary

| Term | Meaning |
|---|---|
| **Scene** | Named setup of the whole machine |
| **Preset** | Named set of Optimizations rows |
| **★ / ⏻** | ★ = default (games / GPU boot curve); ⏻ = applied at boot |
| **Boot guard** | Pauses boot presets after a crashed boot |
| **CCD** | CPU chiplet; on X3D chips one CCD has the V-Cache |
| **CO** | Curve Optimizer (Ryzen undervolt) |
| **cTGP / Dynamic Boost** | GPU base power limit / extra shared with the CPU |
| **EPP** | Energy-performance preference of the CPU governor |
| **PSI** | Linux pressure-stall information (stall time for CPU / memory / I/O) |
| **D3cold** | Deepest PCIe power-off state — what lets the dGPU sleep |
| **MUX** | Hardware switch that wires the panel to the iGPU or the dGPU |
