#include "amdgpu.h"
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QRegularExpression>

namespace amdgpu {

QByteArray readFile(const QString &path) {
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly)) return {};
    QByteArray b = f.read(64 * 1024);
    b.replace('\0', "");  // several SMUs pad their tables with NULs
    return b;
}
static QString text(const QString &p) { return QString::fromUtf8(readFile(p)).trimmed(); }
static std::optional<qint64> integer(const QString &p) {
    bool ok = false;
    const qint64 v = text(p).toLongLong(&ok);
    return ok ? std::optional(v) : std::nullopt;
}

/// Signed integers in a line, in order ("-450mv  0mv" → -450, 0).
static QList<int> numbers(const QString &s) {
    static const QRegularExpression re(QStringLiteral("-?\\d+"));
    QList<int> out;
    for (auto it = re.globalMatch(s); it.hasNext();) out << it.next().captured().toInt();
    return out;
}

QList<Card> cards() {
    QList<Card> out;
    // Dev aid (like LPM_FWATTR_BASE): LPM_AMDGPU_DRM=<dir with cardN/device> for screenshots and tests.
    const QDir drm(qEnvironmentVariableIsSet("LPM_AMDGPU_DRM") ? qEnvironmentVariable("LPM_AMDGPU_DRM")
                                                               : QStringLiteral("/sys/class/drm"));
    static const QRegularExpression cardRe(QStringLiteral("^card\\d+$"));
    for (const QString &n : drm.entryList(QDir::Dirs | QDir::System | QDir::NoDotAndDotDot, QDir::Name)) {
        if (!cardRe.match(n).hasMatch()) continue;
        const QString dev = drm.filePath(n) + QStringLiteral("/device");
        if (QFileInfo(QFileInfo(dev + QStringLiteral("/driver")).symLinkTarget()).fileName() != QLatin1String("amdgpu")) continue;
        Card c;
        c.name = n;
        c.dev = dev;
        for (const QString &h : QDir(dev + QStringLiteral("/hwmon")).entryList(QDir::Dirs | QDir::NoDotAndDotDot))
            if (text(dev + "/hwmon/" + h + "/name") == QLatin1String("amdgpu")) { c.hwmon = dev + "/hwmon/" + h; break; }
        c.pciId = text(dev + "/vendor").mid(2) + ':' + text(dev + "/device").mid(2);
        if (c.pciId.endsWith(':')) c.pciId.chop(1);
        const qint64 vram = integer(dev + "/mem_info_vram_total").value_or(0);
        c.integrated = vram > 0 && vram <= (qint64(2) << 30) && !QFile::exists(c.hwmon + "/power1_cap");
        QString product = text(dev + "/product_name");
        if (product.isEmpty()) product = c.integrated ? QStringLiteral("Integrated Radeon") : QStringLiteral("Radeon");
        c.label = QStringLiteral("%1  ·  %2  ·  %3").arg(product, c.pciId, n);
        out << c;
    }
    return out;
}

std::optional<quint64> featureMask() {
    const QString s = text(QStringLiteral("/sys/module/amdgpu/parameters/ppfeaturemask"));
    if (s.isEmpty()) return std::nullopt;
    bool ok = false;
    const quint64 v = s.startsWith(QLatin1String("0x")) ? s.mid(2).toULongLong(&ok, 16) : s.toULongLong(&ok, 0);
    return ok ? std::optional(v) : std::nullopt;
}

std::optional<int> Od::sclkAt(int idx) const {
    for (const State &s : sclk) if (s.index == idx) return s.mhz;
    return std::nullopt;
}
std::optional<int> Od::mclkAt(int idx) const {
    for (const State &s : mclk) if (s.index == idx) return s.mhz;
    return std::nullopt;
}

Od parseOd(const QByteArray &raw) {
    Od t;
    QString section;
    for (QString line : QString::fromUtf8(raw).split('\n')) {
        line = line.trimmed();
        if (line.isEmpty()) continue;
        if (line.startsWith(QLatin1String("OD_")) && line.endsWith(':')) { section = line.chopped(1); continue; }
        const int colon = line.indexOf(':');
        if (section == QLatin1String("OD_SCLK") || section == QLatin1String("OD_MCLK") || section == QLatin1String("OD_VDDC_CURVE")) {
            bool ok = false;
            const int idx = line.left(colon).trimmed().toInt(&ok);
            const QList<int> v = numbers(line.mid(colon + 1));
            if (colon < 0 || !ok || v.isEmpty()) continue;
            const State s{idx, v[0], v.size() > 1 ? v[1] : -1};
            if (section == QLatin1String("OD_SCLK")) t.sclk << s;
            else if (section == QLatin1String("OD_MCLK")) t.mclk << s;
            else if (s.mv >= 0) t.curve << s;
        } else if (section == QLatin1String("OD_SCLK_OFFSET") || section == QLatin1String("OD_VDDGFX_OFFSET")) {
            const QList<int> v = numbers(line);
            if (v.isEmpty()) continue;
            (section == QLatin1String("OD_SCLK_OFFSET") ? t.sclkOffset : t.voltageOffset) = v[0];
        } else if (section == QLatin1String("OD_RANGE") && colon > 0) {
            const QList<int> v = numbers(line.mid(colon + 1));
            if (v.size() >= 2) t.ranges.insert(line.left(colon).trimmed(), {v[0], v[1]});
        }
    }
    const bool perState = std::any_of(t.sclk.cbegin(), t.sclk.cend(), [](const State &s) { return s.mv >= 0; });
    t.kind = t.sclkOffset ? Od::Offset : !t.curve.isEmpty() ? Od::Curve : perState ? Od::PerState
           : !t.sclk.isEmpty() ? Od::MinMax : Od::None;
    return t;
}

QList<Profile> parseProfiles(const QByteArray &raw) {
    // "  1 3D_FULL_SCREEN *:" (Polaris) / " 0 BOOTUP_DEFAULT*:" (Navi+)
    static const QRegularExpression re(QStringLiteral("^\\s*(\\d+)\\s+([A-Z0-9_]+)\\s*(\\*?)\\s*:"));
    QList<Profile> out;
    for (const QString &l : QString::fromUtf8(raw).split('\n'))
        if (const auto m = re.match(l); m.hasMatch()) out << Profile{m.captured(1).toInt(), m.captured(2), !m.captured(3).isEmpty()};
    return out;
}

std::optional<FanValue> parseFanValue(const QByteArray &raw) {
    QStringList l;
    for (const QString &s : QString::fromUtf8(raw).split('\n')) if (!s.trimmed().isEmpty()) l << s.trimmed();
    const int r = l.indexOf(QStringLiteral("OD_RANGE:"));
    if (l.size() < 2 || r < 0 || r + 1 >= l.size()) return std::nullopt;
    const QList<int> v = numbers(l[1]), range = numbers(l[r + 1].section(':', 1));
    if (v.isEmpty() || range.size() < 2) return std::nullopt;
    return FanValue{v[0], range[0], range[1]};
}

std::optional<FanCurve> parseFanCurve(const QByteArray &raw) {
    FanCurve c;
    bool inRange = false, t = false, s = false;
    for (QString l : QString::fromUtf8(raw).split('\n')) {
        l = l.trimmed();
        if (l == QLatin1String("OD_RANGE:")) { inRange = true; continue; }
        if (l.isEmpty() || l.endsWith(':')) continue;
        const QList<int> v = numbers(l.section(':', 1));
        if (v.size() < 2) continue;
        if (!inRange) c.points << QPair<int, int>{v[0], v[1]};
        else if (l.contains(QLatin1String("temp"))) { c.temp = {v[0], v[1]}; t = true; }
        else if (l.contains(QLatin1String("speed"))) { c.speed = {v[0], v[1]}; s = true; }
    }
    return t && s && !c.points.isEmpty() ? std::optional(c) : std::nullopt;
}

static const std::pair<const char *, const char *> FAN_FILES[] = {
    {"zero_rpm", "fan_zero_rpm_enable"}, {"zero_rpm_stop", "fan_zero_rpm_stop_temperature"},
    {"min_pwm", "fan_minimum_pwm"}, {"target_temp", "fan_target_temperature"},
    {"acoustic_limit", "acoustic_limit_rpm_threshold"}, {"acoustic_target", "acoustic_target_rpm_threshold"},
};

Snapshot read(const Card &c) {
    Snapshot s;
    const QString od = c.dev + QStringLiteral("/pp_od_clk_voltage");
    s.odFilePresent = QFile::exists(od);
    s.od = parseOd(readFile(od));
    s.perfLevel = text(c.dev + "/power_dpm_force_performance_level");
    s.profiles = parseProfiles(readFile(c.dev + "/pp_power_profile_mode"));
    if (!c.hwmon.isEmpty()) {
        auto w = [&](const char *f) -> std::optional<double> {
            if (auto v = integer(c.hwmon + '/' + QLatin1String(f))) return *v / 1e6;
            return std::nullopt;
        };
        s.capW = w("power1_cap"); s.capMinW = w("power1_cap_min");
        s.capMaxW = w("power1_cap_max"); s.capDefaultW = w("power1_cap_default");
    }
    const QString fan = c.dev + QStringLiteral("/gpu_od/fan_ctrl/");
    for (const auto &[key, file] : FAN_FILES)
        if (auto v = parseFanValue(readFile(fan + QLatin1String(file)))) s.fan.insert(QLatin1String(key), *v);
    s.fanCurve = parseFanCurve(readFile(fan + QStringLiteral("fan_curve")));
    return s;
}

Live readLive(const Card &c) {
    Live l;
    auto i = [](const QString &p) -> std::optional<int> { if (auto v = integer(p)) return int(*v); return std::nullopt; };
    l.busy = i(c.dev + "/gpu_busy_percent");
    if (c.hwmon.isEmpty()) return l;
    // hwmon inputs are matched by label: the numbering differs between dGPUs and APUs.
    const QDir h(c.hwmon);
    for (const QString &f : h.entryList({QStringLiteral("*_label")}, QDir::Files | QDir::System)) {
        const QString label = text(h.filePath(f)), input = h.filePath(f.chopped(6) + QStringLiteral("_input"));
        if (label == QLatin1String("sclk")) { if (auto v = integer(input)) l.sclk = int(*v / 1000000); }
        else if (label == QLatin1String("mclk")) { if (auto v = integer(input)) l.mclk = int(*v / 1000000); }
        else if (label == QLatin1String("edge")) { if (auto v = integer(input)) l.edgeC = *v / 1000.0; }
        else if (label == QLatin1String("junction")) { if (auto v = integer(input)) l.junctionC = *v / 1000.0; }
        else if (label == QLatin1String("vddgfx")) l.vddgfxMv = i(input);
    }
    for (const char *p : {"power1_average", "power1_input"})
        if (auto v = integer(c.hwmon + '/' + QLatin1String(p))) { l.powerW = *v / 1e6; break; }
    l.fanRpm = i(c.hwmon + "/fan1_input");
    return l;
}

} // namespace amdgpu
