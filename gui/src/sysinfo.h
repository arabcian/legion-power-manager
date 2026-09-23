#pragma once
// Static hardware inventory and live sensor readouts for the Home tab —
// port of the helper functions in home_tab.py. Everything is sysfs /
// procfs; the only external process is nvidia-smi, which callers run
// asynchronously (see HomeTab) so the GUI thread never blocks on it.
#include <QList>
#include <QPair>
#include <QString>
#include <optional>

namespace sysinfo {

using Row = QPair<QString, QString>;
using Opt = std::optional<QString>;

std::optional<QString> dmi(const QString &field);         // raw
std::optional<QString> dmiClean(const QString &field);    // placeholder strings filtered
Opt cpuModel();

enum class CpuVendor { Amd, Intel, Other };
/// vendor_id from /proc/cpuinfo (read once; LPM_CPU_VENDOR=amd|intel overrides, dev only).
CpuVendor cpuVendor();
inline bool isIntel() { return cpuVendor() == CpuVendor::Intel; }
inline bool isAmd() { return cpuVendor() == CpuVendor::Amd; }
Opt ramTotal();
Opt kernel();
Opt biosInfo();
Opt systemInfo();

// Live (cheap sysfs reads)
Opt cpuTemp();
Opt fans();
Opt storage();
Opt power();
Opt igpu();
Opt battery();
Opt cpuPackagePower();  // powercap RAPL energy delta between calls
Opt usbcInputs();       // UCSI power-delivery sources that are online
Opt gpuMode();          // "Hybrid" / "dGPU only (MUX)"

// nvidia-smi output parsers (the process itself is run by the caller)
Opt parseGpuName(const QByteArray &out);
Opt parseGpuLive(const QByteArray &out);

} // namespace sysinfo
