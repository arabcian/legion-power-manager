# ⚠️ Disclaimer — read this before using Legion Power Manager

**If you do not know what a memory timing, an embedded controller, or a UEFI variable is, do not use the write features of this tool.** Use the vendor's own software, or leave your machine at its factory settings. Everything below exists for experienced users who understand the hardware and accept the consequences.

## What this tool does

Legion Power Manager changes low-level hardware settings that the manufacturer normally controls. It does not use a vendor-supported interface, because Lenovo does not publish one for Linux. Everything was found by reverse-engineering the firmware of **one** laptop model (Lenovo Legion Pro 7 16AFR10H, BIOS SMCN19WW/SMCN20WW) and tested on that machine only.

Depending on the feature, it writes to:

- **Embedded controller (EC) registers**, through ACPI/WMI methods: fan curves, fan speeds, power and thermal limits.
- **CPU and GPU power-management interfaces**: power limits, undervolt and Curve Optimizer offsets, GPU power targets.
- **UEFI firmware variables** read by the BIOS at boot: DRAM timings (memory overclocking).

## Risks

Using the write features can cause, among other things:

- **A machine that does not boot.** Wrong memory timings can make DRAM training fail at POST. Recovery may need a CMOS/EC reset, reflashing the BIOS, or a repair service.
- **Silent data corruption.** Memory that is slightly unstable can boot and seem fine while corrupting files, filesystems, and backups over time, without any crash or warning.
- **Overheating and hardware damage.** A fan curve that is too slow, or thermal and power limits set too high, can overheat the CPU, GPU, VRMs, or battery, shorten component life, or cause permanent damage.
- **System instability**: freezes, crashes, kernel panics, and lost unsaved work.
- **Loss of warranty.** Overclocking, undervolting, and changing firmware settings are generally not covered by the manufacturer's warranty. The BIOS itself states this.
- **Behaviour that changes with firmware updates.** A BIOS or EC update can move, rename, or change the meaning of any value this tool touches. A setting that is safe today may do something different after an update.

## Limits of the safety checks

The tool tries to protect you:

- It refuses to write on machines and firmware it has not been verified on.
- It checks values against ranges, and checks firmware variables against the running hardware before writing them.
- It backs up firmware variables before every change.
- It reads values back after writing them.

These checks **reduce** risk; they do not remove it. A value inside the allowed range can still be unstable on **your** particular CPU, memory modules, or cooling. Silicon differs from chip to chip. A setting that is stable on the author's machine can fail on yours.

Features marked as model-specific (memory timing editing, fan-curve control) have been confirmed on **one** machine only. On any other model, even in the same product line, their behaviour is unknown.

## Firmware protections

Some firmware versions protect their settings from being changed by the operating system, for example **AMD Variable Protection**. When that protection is on, this tool cannot change those settings, and it will not try to get around it. Disabling firmware protections is your own decision and your own responsibility. Do not use exploits or modified firmware to force changes; this project does not support that and will not help with it.

## Recovery

Before you change anything:

- Know how to reset your machine's firmware settings. On the tested Legion, holding the power button for 8–15 seconds restores the default overclocking parameters.
- Keep the firmware variable backups the tool writes to `/var/lib/legion-power-manager/`.
- Have a current backup of your data.

After a memory timing change, test stability properly before trusting the system with real work. Use memory stress tests over several hours, not just a successful boot.

## No warranty, no liability

This software is provided **"as is", without warranty of any kind**, express or implied, including but not limited to the warranties of merchantability, fitness for a particular purpose, and non-infringement. See the LICENSE file for the full terms.

In no event shall the authors or contributors be liable for any claim, damages, data loss, hardware damage, or other liability, whether in an action of contract, tort, or otherwise, arising from, out of, or in connection with the software or its use.

This project is not affiliated with, endorsed by, or supported by Lenovo, AMD, NVIDIA, or any other hardware or firmware vendor. All trademarks belong to their respective owners.

**By using the write features of this tool you confirm that you understand these risks and accept full responsibility for the result.**
