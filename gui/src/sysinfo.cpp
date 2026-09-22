#include "sysinfo.h"
#include "platformprofile.h"
#include <QDir>
#include <QFile>
#include <QRegularExpression>
#include <QSet>
#include <fstream>
#include <sys/utsname.h>

namespace sysinfo {

static const QString SEP = QStringLiteral("  ·  ");
static Opt rd(const QString &p) { return pp::readText(p); }
static std::optional<long long> rdInt(const QString &p) {
    auto s = rd(p);
    bool ok = false;
    long long v = s ? s->toLongLong(&ok) : 0;
    return ok ? std::optional(v) : std::nullopt;
}
static Opt joined(const QStringList &l) { return l.isEmpty() ? Opt() : Opt(l.join(SEP)); }

Opt dmi(const QString &f) { return rd(QStringLiteral("/sys/class/dmi/id/") + f); }

Opt dmiClean(const QString &f) {
    static const QSet<QString> junk{"", "none", "not specified", "n/a", "to be filled by o.e.m.", "default string"};
    auto v = dmi(f);
    if (!v || junk.contains(v->trimmed().toLower())) return std::nullopt;
    return v->trimmed();
}

/// First line starting with `prefix` (case-insensitive). `field < 0` → the part
/// after ':', otherwise the whitespace-separated field index.
///
/// Read with std::ifstream, not QFile: procfs files report size 0, and
/// QFile::atEnd() trusts the size, so a `while (!f.atEnd())` loop never ran
/// and /proc/cpuinfo looked empty ("unknown CPU", no Memory row).
static Opt grepFile(const QString &path, const QString &prefix, int field) {
    std::ifstream in(path.toStdString());
    std::string raw;
    while (std::getline(in, raw)) {
        const QString line = QString::fromStdString(raw);
        if (!line.startsWith(prefix, Qt::CaseInsensitive)) continue;
        if (field < 0) return line.section(':', 1).trimmed();
        return line.simplified().section(' ', field, field);
    }
    return std::nullopt;
}

Opt cpuModel() { return grepFile(QStringLiteral("/proc/cpuinfo"), QStringLiteral("model name"), -1); }

Opt ramTotal() {
    auto kib = grepFile(QStringLiteral("/proc/meminfo"), QStringLiteral("MemTotal:"), 1);
    bool ok = false;
    double v = kib ? kib->toDouble(&ok) : 0;
    return ok ? Opt(QStringLiteral("%1 GiB").arg(v / (1024.0 * 1024.0), 0, 'f', 1)) : std::nullopt;
}

Opt kernel() {
    utsname u{};
    return uname(&u) == 0 ? Opt(QString::fromUtf8(u.release)) : std::nullopt;
}

Opt biosInfo() {
    QStringList bits;
    auto vendor = dmiClean("bios_vendor"), version = dmiClean("bios_version");
    if (vendor && version) bits << *vendor + ' ' + *version;
    else if (vendor || version) bits << vendor.value_or(version.value_or(QString()));
    if (auto d = dmiClean("bios_date")) bits << '(' + *d + ')';
    if (auto r = dmiClean("bios_release")) bits << '[' + *r + ']';
    const QString t = bits.join(' ').trimmed();
    return t.isEmpty() ? Opt() : Opt(t);
}

Opt systemInfo() {
    QStringList parts;
    if (auto v = dmiClean("sys_vendor")) parts << *v;
    if (auto f = dmiClean("product_family")) parts << *f;
    QString t = parts.join(' ');
    if (auto sku = dmiClean("product_sku")) t = t.isEmpty() ? "SKU " + *sku : t + " (SKU " + *sku + ')';
    return t.isEmpty() ? Opt() : Opt(t);
}

// ── hwmon ───────────────────────────────────────────────────────────────────

struct Chip { QString name, path; };

static QList<Chip> chips() {
    QList<Chip> out;
    const QDir d(QStringLiteral("/sys/class/hwmon"));
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name)) {
        const QString p = d.filePath(e);
        if (auto n = rd(p + "/name"); n && !n->isEmpty()) out.append({*n, p});
    }
    return out;
}

static int trailingIndex(const QString &file) {
    static const QRegularExpression re(QStringLiteral("(\\d+)_input$"));
    auto m = re.match(file);
    return m.hasMatch() ? m.captured(1).toInt() : 0;
}

/// Sensor files `<kind>N_input` sorted by N (not lexically: temp10 > temp2).
static QStringList inputs(const QString &chip, const QString &kind) {
    QStringList l = QDir(chip).entryList({kind + "*_input"}, QDir::Files | QDir::System);
    std::sort(l.begin(), l.end(), [](const QString &a, const QString &b) { return trailingIndex(a) < trailingIndex(b); });
    for (QString &s : l) s = chip + '/' + s;
    return l;
}

static QString label(const QString &input) {
    return rd(input.chopped(6) + "_label").value_or(QString()).trimmed().toLower();
}

static std::optional<double> chipTemp(const QString &chip) {
    const QStringList c = inputs(chip, "temp");
    if (c.isEmpty()) return std::nullopt;
    QString chosen = c.first();
    for (const QString &f : c) {
        const QString l = label(f);
        if (l.contains("tctl") || l.contains("tdie") || l.contains("package")) { chosen = f; break; }
    }
    auto v = rdInt(chosen);
    return v ? std::optional(*v / 1000.0) : std::nullopt;
}

Opt cpuTemp() {
    const auto cs = chips();
    for (const char *want : {"k10temp", "zenpower", "coretemp"}) {
        for (const Chip &c : cs) {
            if (c.name != QLatin1String(want)) continue;
            auto main = chipTemp(c.path);
            if (!main) continue;
            QString t = QStringLiteral("%1 °C").arg(*main, 0, 'f', 0);
            for (const QString &f : inputs(c.path, "temp")) {
                QString l = label(f);
                if (!l.startsWith("tccd")) continue;
                if (auto v = rdInt(f)) t += SEP + l.replace("tccd", "CCD") + QStringLiteral(" %1°C").arg(*v / 1000.0, 0, 'f', 0);
            }
            return t;
        }
    }
    return std::nullopt;
}

Opt fans() {
    QStringList l;
    for (const Chip &c : chips())
        for (const QString &f : inputs(c.path, "fan"))
            if (auto rpm = rdInt(f); rpm && *rpm > 0 && l.size() < 4)
                l << QStringLiteral("Fan %1 %2 RPM").arg(trailingIndex(f)).arg(*rpm);
    return joined(l);
}

Opt storage() {
    QStringList l;
    int n = 0;
    for (const Chip &c : chips()) {
        if (c.name != "nvme") continue;
        ++n;
        if (auto t = chipTemp(c.path)) l << QStringLiteral("NVMe %1 %2°C").arg(n).arg(*t, 0, 'f', 0);
    }
    return joined(l);
}

Opt power() {
    QStringList l;
    for (const Chip &c : chips())
        for (const QString &f : inputs(c.path, "power"))
            if (auto uw = rdInt(f); uw && *uw > 0 && l.size() < 4)
                l << QStringLiteral("%1 %2 W").arg(c.name).arg(*uw / 1e6, 0, 'f', 1);
    return joined(l);
}

Opt igpu() {
    for (const Chip &c : chips()) {
        if (c.name != "amdgpu") continue;
        QStringList b;
        if (auto t = chipTemp(c.path)) b << QStringLiteral("%1°C").arg(*t, 0, 'f', 0);
        if (auto pw = inputs(c.path, "power"); !pw.isEmpty())
            if (auto uw = rdInt(pw.first())) b << QStringLiteral("%1W").arg(*uw / 1e6, 0, 'f', 1);
        if (auto hz = rdInt(c.path + "/freq1_input")) b << QStringLiteral("%1MHz").arg(*hz / 1000000);
        return joined(b);
    }
    return std::nullopt;
}

Opt battery() {
    const QDir d(QStringLiteral("/sys/class/power_supply"));
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name)) {
        const QString p = d.filePath(e);
        if (rd(p + "/type") != QStringLiteral("Battery")) continue;
        QStringList b;
        if (auto cap = rd(p + "/capacity")) b << *cap + '%';
        if (auto st = rd(p + "/status"); st && !st->isEmpty()) b << *st;
        auto full = rdInt(p + "/energy_full"); if (!full) full = rdInt(p + "/charge_full");
        auto design = rdInt(p + "/energy_full_design"); if (!design) design = rdInt(p + "/charge_full_design");
        if (full && design && *design > 0) b << QStringLiteral("health %1%").arg(100.0 * *full / *design, 0, 'f', 0);
        if (auto uv = rdInt(p + "/voltage_now")) b << QStringLiteral("%1V").arg(*uv / 1e6, 0, 'f', 2);
        return joined(b);
    }
    return std::nullopt;
}

Opt parseGpuName(const QByteArray &out) {
    const QString first = QString::fromUtf8(out).trimmed().section('\n', 0, 0).trimmed();
    return first.isEmpty() ? Opt() : Opt(first);
}

Opt parseGpuLive(const QByteArray &out) {
    const QStringList parts = QString::fromUtf8(out).trimmed().section('\n', 0, 0).split(',');
    if (parts.size() < 4) return std::nullopt;
    const char *suffix[] = {"°C", "W", "MHz", "% util"};
    QStringList b;
    for (int i = 0; i < 4; ++i)
        if (const QString v = parts[i].trimmed(); !v.isEmpty() && !v.contains("N/A")) b << v + QString::fromUtf8(suffix[i]);
    return joined(b);
}

} // namespace sysinfo
