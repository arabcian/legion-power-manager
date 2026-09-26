#include "lighting.h"
#include <QCoreApplication>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QJsonArray>
#include <QJsonDocument>
#include <QPointer>
#include <QProcess>
#include <QSet>
#include <QTimer>
#include <utility>

namespace lighting {

static constexpr int DIRECT_TIMEOUT_MS = 15000;

bool present() {
    // Dev aid (like LPM_FWATTR_BASE): show the tab without the hardware; the helper still decides.
    if (qEnvironmentVariableIsSet("LPM_LIGHTING_FORCE")) return true;
    const QDir d(QStringLiteral("/sys/class/hidraw"));
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System)) {
        QFile f(d.filePath(e) + QStringLiteral("/device/uevent"));
        if (!f.open(QIODevice::ReadOnly)) continue;
        for (const QByteArray &line : f.readAll().split('\n')) {
            if (!line.startsWith("HID_ID=")) continue;
            const QList<QByteArray> p = line.mid(7).split(':');
            bool ok1 = false, ok2 = false;
            if (p.size() == 3 && p[1].toUInt(&ok1, 16) == 0x048D && ok1
                && (p[2].toUInt(&ok2, 16) & 0xFF00) == 0xC100 && ok2)
                return true;
        }
    }
    return false;
}

QString typeName(int t) {
    switch (t) {
    case ScrewRainbow: return QStringLiteral("Rainbow spiral");
    case RainbowWave: return QStringLiteral("Rainbow wave");
    case ColorChange: return QStringLiteral("Colour change");
    case ColorPulse: return QStringLiteral("Colour pulse");
    case ColorWave: return QStringLiteral("Colour wave");
    case Smooth: return QStringLiteral("Smooth");
    case Rain: return QStringLiteral("Rain");
    case Ripple: return QStringLiteral("Ripple");
    case AudioBounce: return QStringLiteral("Audio bounce");
    case AudioRipple: return QStringLiteral("Audio ripple");
    case Static: return QStringLiteral("Static");
    case TypeLighting: return QStringLiteral("Type lighting");
    case AuroraSync: return QStringLiteral("Aurora sync");
    }
    return QStringLiteral("Unknown (%1)").arg(t);
}

bool needsHost(int t) { return t == AudioBounce || t == AudioRipple || t == AuroraSync || t < 1 || t > AuroraSync; }
bool hasSpeed(int t) { return t != Static && !needsHost(t); }
bool hasDirection(int t) { return t == RainbowWave || t == ColorWave; }
bool hasClockwise(int t) { return t == ScrewRainbow; }
bool hasColorMode(int t) {
    return t == Static || t == ColorChange || t == ColorPulse || t == ColorWave || t == Smooth
        || t == Rain || t == Ripple || t == TypeLighting;
}
bool multiColor(int t) { return hasColorMode(t) && t != Static; }
QList<int> editableTypes() {
    return {Static, ColorChange, ColorPulse, ColorWave, Smooth, Rain, Ripple, TypeLighting, RainbowWave, ScrewRainbow};
}

QJsonObject toJson(const Effect &e) {
    QJsonArray colors, keys;
    for (const QColor &c : e.colors) colors.append(c.name(QColor::HexRgb).mid(1));
    for (int k : e.keys) keys.append(k);
    return {{"type", e.type}, {"speed", e.speed}, {"direction", e.direction}, {"clockwise", e.clockwise},
            {"color_mode", e.colorMode}, {"colors", colors}, {"keys", keys}};
}

Effect fromJson(const QJsonObject &o) {
    Effect e;
    e.type = o.value("type").toInt();
    e.speed = o.value("speed").toInt();
    e.direction = o.value("direction").toInt();
    e.clockwise = o.value("clockwise").toInt();
    e.colorMode = o.value("color_mode").toInt();
    for (const QJsonValue &c : o.value("colors").toArray()) e.colors << QColor('#' + c.toString());
    for (const QJsonValue &k : o.value("keys").toArray()) e.keys << k.toInt();
    return e;
}

int encodedLen(const QList<Effect> &effects) {
    int n = 4 + 3;  // report header + profile preamble
    for (const Effect &e : effects) n += 1 + 13 + 1 + int(e.colors.size()) * 3 + 1 + int(e.keys.size()) * 2;
    return n;
}

bool isPerimeter(int kc) { return (kc >= 0x03E9 && kc <= 0x03FA) || (kc >= 0x01F5 && kc <= 0x01FE); }

QString keyLabel(int kc) {
    // Legends by keycode, from legion-spectrum-control (MIT, Copyright (c) 2026
    // alstergee) — its license text is reproduced in NOTICE.
    static const QHash<int, QString> labels = [] {
        QHash<int, QString> h;
        const std::pair<int, const char *> t[] = {
            {1, "Esc"}, {2, "F1"}, {3, "F2"}, {4, "F3"}, {5, "F4"}, {6, "F5"}, {7, "F6"}, {8, "F7"}, {9, "F8"},
            {10, "F9"}, {11, "F10"}, {12, "F11"}, {13, "F12"}, {14, "Ins"}, {15, "PrtSc"}, {16, "Del"},
            {17, "Home"}, {18, "End"}, {19, "PgUp"}, {20, "PgDn"}, {22, "`"}, {23, "1"}, {24, "2"}, {25, "3"},
            {26, "4"}, {27, "5"}, {28, "6"}, {29, "7"}, {30, "8"}, {31, "9"}, {32, "0"}, {33, "-"}, {34, "="},
            {56, "Bksp"}, {38, "Num"}, {39, "/"}, {40, "*"}, {41, "−"}, {64, "Tab"}, {66, "Q"}, {67, "W"},
            {68, "E"}, {69, "R"}, {70, "T"}, {71, "Y"}, {72, "U"}, {73, "I"}, {74, "O"}, {75, "P"}, {76, "["},
            {77, "]"}, {78, "\\"}, {79, "7"}, {80, "8"}, {81, "9"}, {104, "+"}, {85, "Caps"}, {109, "A"},
            {110, "S"}, {88, "D"}, {89, "F"}, {90, "G"}, {113, "H"}, {114, "J"}, {91, "K"}, {92, "L"},
            {93, ";"}, {119, "Enter"}, {121, "4"}, {123, "5"}, {124, "6"}, {106, "Shift"}, {130, "Z"},
            {131, "X"}, {111, "C"}, {112, "V"}, {135, "B"}, {136, "N"}, {115, "M"}, {116, ","}, {117, "."},
            {118, "/"}, {141, "Shift"}, {142, "1"}, {144, "2"}, {146, "3"}, {167, "Ent"}, {127, "Ctrl"},
            {128, "Fn"}, {150, "❖"}, {151, "Alt"}, {152, ""}, {154, "Alt"}, {155, "⬢"}, {157, "↑"},
            {163, "0"}, {165, "."}, {156, "←"}, {159, "↓"}, {161, "→"},
        };
        for (const auto &[k, v] : t) h.insert(k, QString::fromUtf8(v));
        return h;
    }();
    return labels.value(kc);
}

KeyMap KeyMap::fromJson(const QJsonObject &o) {
    KeyMap m;
    m.rows = o.value("rows").toInt();
    m.cols = o.value("cols").toInt();
    for (const QJsonValue &v : o.value("grid").toArray()) m.grid << v.toInt();
    for (const QJsonValue &v : o.value("extra").toArray()) m.extra << v.toInt();
    if (m.rows <= 0 || m.cols <= 0 || m.grid.size() != m.rows * m.cols) return {};
    return m;
}

QList<int> KeyMap::unique() const {
    QList<int> out;
    QSet<int> seen;
    for (const QVector<int> *v : {&grid, &extra})
        for (int k : *v)
            if (k && !seen.contains(k)) { seen.insert(k); out << k; }
    return out;
}

Zone KeyMap::zoneOf(int kc) const {
    if (isPerimeter(kc)) return Zone::Perimeter;
    if (extra.contains(kc)) return Zone::Logo;
    return Zone::Keyboard;
}

QList<int> KeyMap::zone(Zone z) const {
    QList<int> out;
    for (int k : unique()) if (zoneOf(k) == z) out << k;
    return out;
}

QList<KeyMap::Span> KeyMap::spans() const {
    QList<Span> out;
    for (int r = 0; r < rows; ++r)
        for (int c = 0; c < cols;) {
            const int k = at(r, c), start = c;
            while (c < cols && at(r, c) == k) ++c;
            if (k) out.append({k, r, start, c - start});
        }
    return out;
}

// ── helper call ─────────────────────────────────────────────────────────────

static void runDirect(const QJsonObject &req, QObject *ctx, privileged::Callback cb) {
    QPointer<QObject> guard(ctx);
    auto fail = [&](const QString &msg) {
        privileged::Result r; r.error = msg;
        QTimer::singleShot(0, ctx, [cb, r] { cb(r); });
    };
    if (qEnvironmentVariableIsSet("LPM_PGO_TRAIN")) return fail(QStringLiteral("disabled during the PGO training run"));
    const QString helper = privileged::helperPath(QStringLiteral("lighting-helper"));
    if (!QFileInfo(helper).isFile()) return fail(QStringLiteral("helper not found: ") + helper);

    // Runs as the user: killing it on timeout is allowed, so a plain parent is fine.
    auto *proc = new QProcess(QCoreApplication::instance());
    auto *timer = new QTimer(proc);
    timer->setSingleShot(true);
    auto done = std::make_shared<bool>(false);
    auto finish = [=](const privileged::Result &r) {
        if (std::exchange(*done, true)) return;
        timer->stop();
        if (guard) cb(r);
    };
    QObject::connect(timer, &QTimer::timeout, proc, [=] {
        privileged::Result r; r.error = QStringLiteral("lighting-helper did not answer in time");
        finish(r);
        proc->kill();
    });
    QObject::connect(proc, &QProcess::errorOccurred, proc, [=](QProcess::ProcessError e) {
        if (e != QProcess::FailedToStart) return;
        privileged::Result r; r.error = QStringLiteral("could not start lighting-helper: ") + proc->errorString();
        finish(r);
        proc->deleteLater();
    });
    QObject::connect(proc, &QProcess::finished, proc, [=](int code, QProcess::ExitStatus) {
        proc->deleteLater();
        const QByteArray out = proc->readAllStandardOutput().trimmed();
        const QJsonDocument doc = QJsonDocument::fromJson(out.mid(out.lastIndexOf('\n') + 1));
        privileged::Result r;
        if (doc.isObject() && !doc.object().isEmpty()) { r.reached = true; r.json = doc.object(); }
        else r.error = QStringLiteral("lighting-helper exited with code %1").arg(code);
        finish(r);
    });
    proc->setProgram(helper);
    proc->setProcessChannelMode(QProcess::SeparateChannels);
    proc->start();
    if (proc->state() == QProcess::NotRunning) return;
    proc->write(QJsonDocument(req).toJson(QJsonDocument::Compact));
    proc->closeWriteChannel();
    timer->start(DIRECT_TIMEOUT_MS);
}

void run(const QJsonObject &req, QObject *ctx, privileged::Callback cb, bool elevate) {
    QPointer<QObject> guard(ctx);
    runDirect(req, ctx, [req, guard, cb, elevate](const privileged::Result &r) {
        if (elevate && r.reached && r.json.value("denied").toBool() && guard) {
            privileged::run(privileged::helperPath(QStringLiteral("lighting-helper")), req, guard.data(), cb, 60000);
            return;
        }
        cb(r);
    });
}

} // namespace lighting
