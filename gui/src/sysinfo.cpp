#include "sysinfo.h"
#include <QElapsedTimer>
#include <QMutex>
#include <QHash>
#include "platformprofile.h"
#include <QDir>
#include <QFile>
#include <QRegularExpression>
#include <QSet>
#include <algorithm>
#include <fstream>
#include <sys/utsname.h>

namespace sysinfo {

static const QString SEP = QStringLiteral("  ·  ");
static Opt rd(const QString &p) { return pp::readText(p); }
// Numbers parse straight from the bytes: no UTF-16 round trip per attribute.
static std::optional<long long> rdInt(const QString &p) {
    const auto s = pp::readRaw(p);
    bool ok = false;
    const long long v = s ? s->trimmed().toLongLong(&ok) : 0;
    return ok ? std::optional(v) : std::nullopt;
}
static Opt joined(const QStringList &l) { return l.isEmpty() ? Opt() : Opt(l.join(SEP)); }

Opt dmi(const QString &f) { return rd(QStringLiteral("/sys/class/dmi/id/") + f); }

bool supportedMachine(QString *reason) {
    // Word-anchored: "Legion Pro 7", "LOQ 15IRH8", "IdeaPad Gaming 3" — not "Legionnaire".
    static const QRegularExpression family(QStringLiteral("\\b(legion|loq|ideapad gaming)\\b"),
                                           QRegularExpression::CaseInsensitiveOption);
    const QString vendor = dmi(QStringLiteral("sys_vendor")).value_or(QString());
    const QString version = dmi(QStringLiteral("product_version")).value_or(QString());
    const QString fam = dmi(QStringLiteral("product_family")).value_or(QString());
    const QString shown = !version.isEmpty() ? version : !fam.isEmpty() ? fam : dmi(QStringLiteral("product_name")).value_or(QString());
    const bool lenovo = vendor.compare(QLatin1String("LENOVO"), Qt::CaseInsensitive) == 0;
    // China-market names that may omit "Legion" (Y9000P, R9000K, GeekPro G5000…);
    // same list as the helpers (crates/lpm-helpers/src/machine.rs, CN_MODELS).
    static const QRegularExpression cn(QStringLiteral("(^|[^A-Za-z0-9])(Y9000|R9000|Y7000|R7000|G5000)"),
                                       QRegularExpression::CaseInsensitiveOption);
    if (lenovo && (family.match(version).hasMatch() || family.match(fam).hasMatch()
                   || cn.match(version).hasMatch() || cn.match(fam).hasMatch())) return true;
    if (reason) {
        QString what = QStringList{vendor, shown}.filter(QRegularExpression(QStringLiteral("\\S"))).join(' ');
        if (what.isEmpty()) what = QStringLiteral("no DMI information");
        *reason = QStringLiteral("This computer (%1) is not supported.\n\nLegion Power Manager runs only on "
                                 "Lenovo Legion, LOQ and IdeaPad Gaming laptops: it writes Lenovo-specific "
                                 "firmware and embedded-controller settings.").arg(what);
    }
    return false;
}

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

CpuVendor cpuVendor() {
    static const CpuVendor v = [] {
        const QString o = qEnvironmentVariable("LPM_CPU_VENDOR").toLower();
        if (o == "amd") return CpuVendor::Amd;
        if (o == "intel") return CpuVendor::Intel;
        const QString id = grepFile(QStringLiteral("/proc/cpuinfo"), QStringLiteral("vendor_id"), -1).value_or(QString());
        if (id == "GenuineIntel") return CpuVendor::Intel;
        if (id == "AuthenticAMD" || id == "HygonGenuine") return CpuVendor::Amd;
        return CpuVendor::Other;
    }();
    return v;
}

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

/// Directory entries that only change on hot-plug (power supplies, powercap
/// zones, DRM cards), filtered once by `keep` and re-listed at most every 30 s.
/// Thread-safe: the Live getters run on a worker thread.
static QStringList cachedDirs(const QString &dir, bool (*keep)(const QString &path, const QString &entry)) {
    struct Entry { QStringList paths; QElapsedTimer age; };
    static QMutex mu;
    static QHash<QString, Entry> cache;
    const QMutexLocker lock(&mu);
    const QString key = dir + QChar(0) + QString::number(quintptr(keep));
    Entry &e = cache[key];
    if (!e.age.isValid() || e.age.elapsed() > 30000) {
        e.paths.clear();
        const QDir d(dir);
        for (const QString &n : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name))
            if (const QString p = d.filePath(n); keep(p, n)) e.paths << p;
        e.age.restart();
    }
    return e.paths;
}

// ── hwmon ───────────────────────────────────────────────────────────────────

/// One `<kind>N_input` file with its N and lower-cased label (both fixed for the
/// chip's lifetime, so they are read once per scan, not once per poll).
struct Input { QString path; int index; QString label; };
struct Chip { QString name, path; QList<Input> temp, fan, power; };

static QList<Chip> scanChips();
// hwmon chips only appear/disappear on module load or hot-plug, and a chip's
// sensor files and labels never change while it exists: the directory walk,
// the three globs per chip and every *_label read used to run for each 2 s
// Live poll; now they run once per 30 s rescan and a poll only reads values.
// The Live getters run on a worker thread (HomeTab::refreshLive), so the
// shared cache is guarded.
static QList<Chip> chips() {
    static QMutex mu;
    static QList<Chip> cache;
    static QElapsedTimer age;
    const QMutexLocker lock(&mu);
    if (!age.isValid() || age.elapsed() > 30000 || cache.isEmpty()) { cache = scanChips(); age.restart(); }
    return cache;  // implicitly shared: a copy is a refcount bump
}

/// N of ".../<kind>N_input" (0 if absent). Plain scan instead of a shared
/// static QRegularExpression: runs on a worker thread, and in sort comparators.
static int trailingIndex(const QString &file) {
    static const QLatin1String suffix("_input");
    if (!file.endsWith(suffix)) return 0;
    const qsizetype end = file.size() - suffix.size();
    qsizetype b = end;
    while (b > 0 && file.at(b - 1).isDigit()) --b;
    return b < end ? QStringView(file).mid(b, end - b).toInt() : 0;
}

/// Sensor files `<kind>N_input` sorted by N (not lexically: temp10 > temp2), one
/// directory listing per chip for all three kinds.
static void scanInputs(Chip &c) {
    const QStringList all = QDir(c.path).entryList({QStringLiteral("temp*_input"), QStringLiteral("fan*_input"),
                                                    QStringLiteral("power*_input")}, QDir::Files | QDir::System);
    for (const QString &f : all) {
        Input in{c.path + '/' + f, trailingIndex(f), QString()};
        in.label = rd(in.path.chopped(6) + QStringLiteral("_label")).value_or(QString()).trimmed().toLower();
        (f.startsWith(QLatin1String("temp")) ? c.temp : f.startsWith(QLatin1String("fan")) ? c.fan : c.power).append(in);
    }
    for (QList<Input> *l : {&c.temp, &c.fan, &c.power})
        std::sort(l->begin(), l->end(), [](const Input &a, const Input &b) { return a.index < b.index; });
}

static QList<Chip> scanChips() {
    QList<Chip> out;
    const QDir d(QStringLiteral("/sys/class/hwmon"));
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name)) {
        const QString p = d.filePath(e);
        if (auto n = rd(p + "/name"); n && !n->isEmpty()) {
            Chip c{*n, p, {}, {}, {}};
            scanInputs(c);
            out.append(std::move(c));
        }
    }
    return out;
}

static std::optional<double> chipTemp(const Chip &chip) {
    if (chip.temp.isEmpty()) return std::nullopt;
    const Input *chosen = &chip.temp.first();
    for (const Input &f : chip.temp)
        if (f.label.contains(QLatin1String("tctl")) || f.label.contains(QLatin1String("tdie"))
            || f.label.contains(QLatin1String("package"))) { chosen = &f; break; }
    auto v = rdInt(chosen->path);
    return v ? std::optional(*v / 1000.0) : std::nullopt;
}

Opt cpuTemp() {
    const auto cs = chips();
    for (const char *want : {"k10temp", "zenpower", "coretemp"}) {
        for (const Chip &c : cs) {
            if (c.name != QLatin1String(want)) continue;
            auto main = chipTemp(c);
            if (!main) continue;
            QString t = QStringLiteral("%1 °C").arg(*main, 0, 'f', 0);
            long long hottest = -1;
            for (const Input &f : c.temp) {
                // coretemp: "Core N" per physical core; show the hottest next to the package.
                if (f.label.startsWith(QLatin1String("core "))) { if (auto v = rdInt(f.path)) hottest = std::max(hottest, *v); continue; }
                if (!f.label.startsWith(QLatin1String("tccd"))) continue;
                if (auto v = rdInt(f.path))
                    t += SEP + QString(f.label).replace(QLatin1String("tccd"), QLatin1String("CCD")) + QStringLiteral(" %1°C").arg(*v / 1000.0, 0, 'f', 0);
            }
            if (hottest >= 0) t += SEP + QStringLiteral("hottest core %1°C").arg(hottest / 1000.0, 0, 'f', 0);
            return t;
        }
    }
    return std::nullopt;
}

Opt fans() {
    QStringList l;
    for (const Chip &c : chips())
        for (const Input &f : c.fan) {
            if (l.size() >= 4) return joined(l);
            if (auto rpm = rdInt(f.path); rpm && *rpm > 0) l << QStringLiteral("Fan %1 %2 RPM").arg(f.index).arg(*rpm);
        }
    return joined(l);
}

Opt storage() {
    QStringList l;
    int n = 0;
    for (const Chip &c : chips()) {
        if (c.name != QLatin1String("nvme")) continue;
        ++n;
        if (auto t = chipTemp(c)) l << QStringLiteral("NVMe %1 %2°C").arg(n).arg(*t, 0, 'f', 0);
    }
    return joined(l);
}

Opt power() {
    QStringList l;
    for (const Chip &c : chips())
        for (const Input &f : c.power) {
            if (l.size() >= 4) return joined(l);
            if (auto uw = rdInt(f.path); uw && *uw > 0) l << QStringLiteral("%1 %2 W").arg(c.name).arg(*uw / 1e6, 0, 'f', 1);
        }
    return joined(l);
}

Opt igpu() {
    for (const Chip &c : chips()) {
        if (c.name != "amdgpu") continue;
        QStringList b;
        if (auto t = chipTemp(c)) b << QStringLiteral("%1°C").arg(*t, 0, 'f', 0);
        if (!c.power.isEmpty())
            if (auto uw = rdInt(c.power.first().path)) b << QStringLiteral("%1W").arg(*uw / 1e6, 0, 'f', 1);
        if (auto hz = rdInt(c.path + "/freq1_input")) b << QStringLiteral("%1MHz").arg(*hz / 1000000);
        return joined(b);
    }
    // Intel iGPU (i915 / xe): no hwmon temp on most parts, the actual GT clock is enough.
    const QStringList cards = cachedDirs(QStringLiteral("/sys/class/drm"), [](const QString &p, const QString &e) {
        return e.startsWith(QLatin1String("card")) && !e.contains('-') && rd(p + QStringLiteral("/device/vendor")) == QStringLiteral("0x8086");
    });
    for (const QString &card : cards) {
        auto mhz = rdInt(card + "/gt/gt0/rps_act_freq_mhz");                    // i915
        if (!mhz) mhz = rdInt(card + "/device/tile0/gt0/freq0/act_freq");        // xe
        if (!mhz) continue;
        QStringList b{QStringLiteral("%1MHz").arg(*mhz)};
        if (auto max = rdInt(card + "/gt/gt0/rps_max_freq_mhz")) b << QStringLiteral("max %1MHz").arg(*max);
        return joined(b);
    }
    return std::nullopt;
}

Opt battery() {
    const QStringList bats = cachedDirs(QStringLiteral("/sys/class/power_supply"), [](const QString &p, const QString &) {
        return rd(p + QStringLiteral("/type")) == QStringLiteral("Battery");
    });
    for (const QString &p : bats) {
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

Opt cpuPackagePower() {
    // energy_uj is a free-running counter: power = Δenergy / Δt between two polls.
    struct Sample { long long uj = -1; qint64 ms = 0; };
    static QMutex mu;
    const QMutexLocker lock(&mu);
    static QHash<QString, Sample> last;
    static QElapsedTimer clock;
    if (!clock.isValid()) clock.start();
    const QStringList zones = cachedDirs(QStringLiteral("/sys/class/powercap"), [](const QString &, const QString &e) {
        if (e.count(':') < 1 || e.count(':') > 2) return false;  // intel-rapl:0 (package), intel-rapl:0:0 (core)
        // intel-rapl-mmio mirrors the MSR package counter; showing both reads as two packages.
        return !(e.startsWith(QLatin1String("intel-rapl-mmio")) && QDir(QStringLiteral("/sys/class/powercap")).exists(QStringLiteral("intel-rapl:0")));
    });
    QStringList l;
    for (const QString &p : zones) {
        const auto uj = rdInt(p + "/energy_uj");
        const auto name = rd(p + "/name");  // zone names are short; kept per poll so a renamed zone shows up
        if (!uj || !name) continue;
        Sample &s = last[p];
        const qint64 now = clock.elapsed();
        if (s.uj >= 0 && now > s.ms && *uj >= s.uj) {
            const double w = double(*uj - s.uj) / double(now - s.ms) / 1000.0;
            l << QStringLiteral("%1 %2 W").arg(name->section('-', 0, 0), QString::number(w, 'f', 1));
        } else if (s.uj < 0) {
            l << QStringLiteral("%1 …").arg(name->section('-', 0, 0));
        }
        s = {*uj, now};
    }
    return joined(l);
}

Opt usbcInputs() {
    const QStringList ports = cachedDirs(QStringLiteral("/sys/class/power_supply"), [](const QString &, const QString &e) {
        return e.startsWith(QLatin1String("ucsi-source-psy"));
    });
    QStringList l;
    bool any = false;
    for (const QString &p : ports) {
        const QString e = p.section('/', -1);
        any = true;
        if (rdInt(p + "/online").value_or(0) != 1) continue;
        const auto uv = rdInt(p + "/voltage_now"), ua = rdInt(p + "/current_now");
        const QString port = QStringLiteral("Port %1").arg(e.section(':', -1).toInt());
        if (uv && ua && *uv > 0 && *ua > 0)
            l << QStringLiteral("%1 %2 W (%3 V)").arg(port).arg(*uv / 1e6 * (*ua / 1e6), 0, 'f', 1).arg(*uv / 1e6, 0, 'f', 0);
        else
            l << port + QStringLiteral(" connected");
    }
    if (!any) return std::nullopt;  // no UCSI at all: row hidden
    return l.isEmpty() ? Opt(QStringLiteral("none")) : joined(l);
}

Opt gpuMode() {
    // Display-class PCI functions: an AMD one next to the NVIDIA one = hybrid.
    const QDir d(QStringLiteral("/sys/bus/pci/devices"));
    // iGPU = an AMD *or* Intel display function (Legion/LOQ ship both CPU vendors).
    bool igpu = false, nv = false;
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System)) {
        const QString p = d.filePath(e);
        if (!rd(p + "/class").value_or(QString()).startsWith(QStringLiteral("0x03"))) continue;
        const QString v = rd(p + "/vendor").value_or(QString());
        igpu |= v == QStringLiteral("0x1002") || v == QStringLiteral("0x8086");
        nv |= v == QStringLiteral("0x10de");
    }
    if (igpu && nv) return QStringLiteral("Hybrid (iGPU + dGPU)");
    if (nv) return QStringLiteral("dGPU only (MUX) — iGPU disabled in firmware");
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
