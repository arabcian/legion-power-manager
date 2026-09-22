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

// nvidia-smi output parsers (the process itself is run by the caller)
Opt parseGpuName(const QByteArray &out);
Opt parseGpuLive(const QByteArray &out);

} // namespace sysinfo
