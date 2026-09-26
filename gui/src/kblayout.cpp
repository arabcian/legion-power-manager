#include "kblayout.h"
#include <QDir>
#include <QProcess>
#include <QSet>
#include <QSettings>
#include <algorithm>
#include <utility>
#ifdef LPM_HAVE_XKB
#include <xkbcommon/xkbcommon.h>
#endif

namespace kblayout {

// Legion Pro 7 16AFR10H (Gen10 16", numpad). Codes as the controller reports
// them; x/w in key units, y/h in row units. Main block is 15u, numpad starts
// at 15.5u.
namespace {

constexpr double FROW_H = 0.75, R1 = 0.9, R2 = 1.9, R3 = 2.9, R4 = 3.9, R5 = 4.9;
constexpr double NP = 15.5, WIDTH = 19.5;

struct Def { int code; double x, y, w, h; int evdev; const char *label; };

// Keys identical on ANSI and ISO boards.
const Def COMMON[] = {
    // number row
    {0x16, 0, R1, 1, 1, 41, "`"},
    {0x17, 1, R1, 1, 1, 2, "1"}, {0x18, 2, R1, 1, 1, 3, "2"}, {0x19, 3, R1, 1, 1, 4, "3"},
    {0x1a, 4, R1, 1, 1, 5, "4"}, {0x1b, 5, R1, 1, 1, 6, "5"}, {0x1c, 6, R1, 1, 1, 7, "6"},
    {0x1d, 7, R1, 1, 1, 8, "7"}, {0x1e, 8, R1, 1, 1, 9, "8"}, {0x1f, 9, R1, 1, 1, 10, "9"},
    {0x20, 10, R1, 1, 1, 11, "0"}, {0x21, 11, R1, 1, 1, 12, "-"}, {0x22, 12, R1, 1, 1, 13, "="},
    {0x38, 13, R1, 2, 1, 0, "Bksp"},
    // top letter row
    {0x40, 0, R2, 1.5, 1, 0, "Tab"},
    {0x42, 1.5, R2, 1, 1, 16, "Q"}, {0x43, 2.5, R2, 1, 1, 17, "W"}, {0x44, 3.5, R2, 1, 1, 18, "E"},
    {0x45, 4.5, R2, 1, 1, 19, "R"}, {0x46, 5.5, R2, 1, 1, 20, "T"}, {0x47, 6.5, R2, 1, 1, 21, "Y"},
    {0x48, 7.5, R2, 1, 1, 22, "U"}, {0x49, 8.5, R2, 1, 1, 23, "I"}, {0x4a, 9.5, R2, 1, 1, 24, "O"},
    {0x4b, 10.5, R2, 1, 1, 25, "P"}, {0x4c, 11.5, R2, 1, 1, 26, "["}, {0x4d, 12.5, R2, 1, 1, 27, "]"},
    // home row
    {0x55, 0, R3, 1.75, 1, 0, "Caps"},
    {0x6d, 1.75, R3, 1, 1, 30, "A"}, {0x6e, 2.75, R3, 1, 1, 31, "S"}, {0x58, 3.75, R3, 1, 1, 32, "D"},
    {0x59, 4.75, R3, 1, 1, 33, "F"}, {0x5a, 5.75, R3, 1, 1, 34, "G"}, {0x71, 6.75, R3, 1, 1, 35, "H"},
    {0x72, 7.75, R3, 1, 1, 36, "J"}, {0x5b, 8.75, R3, 1, 1, 37, "K"}, {0x5c, 9.75, R3, 1, 1, 38, "L"},
    {0x5d, 10.75, R3, 1, 1, 39, ";"}, {0x5f, 11.75, R3, 1, 1, 40, "'"},
    // bottom letter row (left shift differs, see below)
    {0x82, 2.25, R4, 1, 1, 44, "Z"}, {0x83, 3.25, R4, 1, 1, 45, "X"}, {0x6f, 4.25, R4, 1, 1, 46, "C"},
    {0x70, 5.25, R4, 1, 1, 47, "V"}, {0x87, 6.25, R4, 1, 1, 48, "B"}, {0x88, 7.25, R4, 1, 1, 49, "N"},
    {0x73, 8.25, R4, 1, 1, 50, "M"}, {0x74, 9.25, R4, 1, 1, 51, ","}, {0x75, 10.25, R4, 1, 1, 52, "."},
    {0x76, 11.25, R4, 1, 1, 53, "/"}, {0x8d, 12.25, R4, 2.75, 1, 0, "Shift"},
    // modifiers + arrows (Lenovo half-height inverted T)
    {0x7f, 0, R5, 1.25, 1, 0, "Ctrl"}, {0x80, 1.25, R5, 1, 1, 0, "Fn"}, {0x96, 2.25, R5, 1, 1, 0, "❖"},
    {0x97, 3.25, R5, 1, 1, 0, "Alt"}, {0x98, 4.25, R5, 5.75, 1, 0, ""}, {0x9a, 10, R5, 1, 1, 0, "Alt"},
    {0x9b, 11, R5, 1, 1, 0, "⬢"},
    {0x9c, 12, R5 + 0.5, 1, 0.5, 0, "←"}, {0x9d, 13, R5, 1, 0.5, 0, "↑"},
    {0x9f, 13, R5 + 0.5, 1, 0.5, 0, "↓"}, {0xa1, 14, R5 + 0.5, 1, 0.5, 0, "→"},
    // numpad (fixed legends: they depend on NumLock, not on the layout)
    {0x26, NP, R1, 1, 1, 0, "Num"}, {0x27, NP + 1, R1, 1, 1, 0, "/"}, {0x28, NP + 2, R1, 1, 1, 0, "*"},
    {0x29, NP + 3, R1, 1, 1, 0, "−"},
    {0x4f, NP, R2, 1, 1, 0, "7"}, {0x50, NP + 1, R2, 1, 1, 0, "8"}, {0x51, NP + 2, R2, 1, 1, 0, "9"},
    {0x68, NP + 3, R2, 1, 2, 0, "+"},
    {0x79, NP, R3, 1, 1, 0, "4"}, {0x7b, NP + 1, R3, 1, 1, 0, "5"}, {0x7c, NP + 2, R3, 1, 1, 0, "6"},
    {0x8e, NP, R4, 1, 1, 0, "1"}, {0x90, NP + 1, R4, 1, 1, 0, "2"}, {0x92, NP + 2, R4, 1, 1, 0, "3"},
    {0xa7, NP + 3, R4, 1, 2, 0, "Ent"},
    {0xa3, NP, R5, 2, 1, 0, "0"}, {0xa5, NP + 2, R5, 1, 1, 0, "."},
};

const Def ISO_ONLY[] = {
    {0x77, 13.5, R2, 1.5, 1, 0, "Enter"},   // ISO Enter: upper part…
    {0x77, 13.75, R3, 1.25, 1, 0, ""},      // …and lower part (one LED)
    {0xa8, 12.75, R3, 1, 1, 43, "#"},       // key left of Enter
    {0x6a, 0, R4, 1.25, 1, 0, "Shift"},
    {0x4e, 1.25, R4, 1, 1, 86, "<"},        // 102nd key
};

// ANSI: derived from the ISO board (0x4e as the backslash above Enter).
// Not verified on hardware; forMap() still requires every reported code to be here.
const Def ANSI_ONLY[] = {
    {0x4e, 13.5, R2, 1.5, 1, 43, "\\"},
    {0x77, 12.75, R3, 2.25, 1, 0, "Enter"},
    {0x6a, 0, R4, 2.25, 1, 0, "Shift"},
};

const char *const FROW[] = {"Esc", "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10",
                            "F11", "F12", "Ins", "PrtSc", "Del", "Home", "End", "PgUp", "PgDn"};

Key toKey(const Def &d) {
    Key k;
    k.code = d.code;
    k.rect = QRectF(d.x, d.y, d.w, d.h);
    k.evdev = d.evdev;
    k.label = QString::fromUtf8(d.label);
    return k;
}

// Grid row → physical y for accent bars placed from the matrix.
double rowY(int r, int rows) {
    static const double Y[] = {-0.45, 0, R1, R2, R3, R4, R5, R5 + 1};
    if (r >= rows - 1) return R5 + 1.25;  // front edge
    return r < int(std::size(Y)) ? Y[r] : R5 + 1;
}

// Accent bars: bounding box of each perimeter code in the controller matrix,
// mapped onto the physical outline (rear edge, front edge, left/right sides).
QList<Key> perimeter(const lighting::KeyMap &m) {
    struct Box { int r0 = 1 << 30, r1 = -1, c0 = 1 << 30, c1 = -1; };
    QHash<int, Box> boxes;
    QList<int> order;
    for (int r = 0; r < m.rows; ++r)
        for (int c = 0; c < m.cols; ++c) {
            const int k = m.at(r, c);
            if (!lighting::isPerimeter(k)) continue;
            if (!boxes.contains(k)) order << k;
            Box &b = boxes[k];
            b.r0 = std::min(b.r0, r); b.r1 = std::max(b.r1, r);
            b.c0 = std::min(b.c0, c); b.c1 = std::max(b.c1, c);
        }
    const int inner = std::max(1, m.cols - 2);           // columns 1..cols-2 hold keys
    const double cw = WIDTH / inner, T = 0.2;
    QList<Key> out;
    for (int k : order) {
        const Box b = boxes[k];
        Key key;
        key.code = k;
        key.bar = true;
        const bool side = b.c0 == 0 || b.c1 == m.cols - 1;
        if (side && b.c0 == b.c1) {
            const double x = b.c0 == 0 ? -0.5 : WIDTH + 0.3;
            const double y0 = std::max(0.0, rowY(b.r0, m.rows));
            const double y1 = std::min(R5 + 1.45, rowY(b.r1, m.rows) + 1);
            key.rect = QRectF(x, y0, T, std::max(0.3, y1 - y0 - 0.1));
        } else {
            const double x0 = (std::max(1, b.c0) - 1) * cw, x1 = (std::min(inner, b.c1)) * cw;
            key.rect = QRectF(x0 + 0.05, rowY(b.r0, m.rows), std::max(0.2, x1 - x0 - 0.1), T);
        }
        out << key;
    }
    return out;
}

} // namespace

Form formOf(const lighting::KeyMap &m) {
    if (m.isEmpty()) return Form::Unknown;
    const QList<int> codes = m.unique();
    if (codes.contains(0xa8)) return Form::Iso;
    if (codes.contains(0x77)) return Form::Ansi;
    return Form::Unknown;
}

QString formName(Form f) {
    switch (f) {
    case Form::Iso: return QStringLiteral("ISO");
    case Form::Ansi: return QStringLiteral("ANSI");
    case Form::Unknown: break;
    }
    return QStringLiteral("unknown");
}

QList<Key> forMap(const lighting::KeyMap &m) {
    const Form form = formOf(m);
    if (form == Form::Unknown) return {};
    QList<Key> keys;
    for (int i = 0; i < int(std::size(FROW)); ++i) {
        Key k;
        k.code = i + 1;
        k.rect = QRectF(i * (WIDTH / std::size(FROW)), 0, WIDTH / std::size(FROW), FROW_H);
        k.label = QString::fromLatin1(FROW[i]);
        keys << k;
    }
    for (const Def &d : COMMON) keys << toKey(d);
    if (form == Form::Iso) for (const Def &d : ISO_ONLY) keys << toKey(d);
    else for (const Def &d : ANSI_ONLY) keys << toKey(d);

    // Every key LED the controller reports must be placed, or the drawing
    // would silently drop keys: fall back to the matrix instead.
    QSet<int> known;
    for (const Key &k : keys) known.insert(k.code);
    for (int k : m.unique())
        if (!lighting::isPerimeter(k) && m.zoneOf(k) == lighting::Zone::Keyboard && !known.contains(k)) return {};
    // …and drop table entries this board does not have.
    const QList<int> present = m.unique();
    keys.erase(std::remove_if(keys.begin(), keys.end(), [&](const Key &k) { return !present.contains(k.code); }),
               keys.end());
    keys << perimeter(m);
    return keys;
}

// ── XKB legends ─────────────────────────────────────────────────────────────

namespace {

std::pair<QString, QString> firstOf(const QString &layouts, const QString &variants) {
    return {layouts.split(',').value(0).trimmed(), variants.split(',').value(0).trimmed()};
}

std::pair<QString, QString> detectLayout() {
    if (qEnvironmentVariableIsSet("LPM_XKB_LAYOUT"))
        return firstOf(qEnvironmentVariable("LPM_XKB_LAYOUT"), qEnvironmentVariable("LPM_XKB_VARIANT"));
    {   // KDE Plasma (X11 and Wayland)
        QSettings kx(QDir::homePath() + QStringLiteral("/.config/kxkbrc"), QSettings::IniFormat);
        kx.beginGroup(QStringLiteral("Layout"));
        const QString list = kx.value(QStringLiteral("LayoutList")).toString();
        if (!list.isEmpty() && kx.value(QStringLiteral("Use"), true).toBool())
            return firstOf(list, kx.value(QStringLiteral("VariantList")).toString());
    }
    if (qEnvironmentVariableIsSet("DISPLAY")) {  // X11 / XWayland
        QProcess p;
        p.start(QStringLiteral("setxkbmap"), {QStringLiteral("-query")});
        if (p.waitForFinished(1000) && p.exitStatus() == QProcess::NormalExit && p.exitCode() == 0) {
            QString layout, variant;
            for (const QString &line : QString::fromLocal8Bit(p.readAllStandardOutput()).split('\n')) {
                const QString v = line.section(':', 1).trimmed();
                if (line.startsWith(QLatin1String("layout:"))) layout = v;
                else if (line.startsWith(QLatin1String("variant:"))) variant = v;
            }
            if (!layout.isEmpty()) return firstOf(layout, variant);
        } else {
            p.kill();
            p.waitForFinished(200);
        }
    }
    if (qEnvironmentVariableIsSet("XKB_DEFAULT_LAYOUT"))
        return firstOf(qEnvironmentVariable("XKB_DEFAULT_LAYOUT"), qEnvironmentVariable("XKB_DEFAULT_VARIANT"));
    return {QStringLiteral("us"), QString()};
}

const std::pair<QString, QString> &layout() {
    static const auto l = detectLayout();
    return l;
}

#ifdef LPM_HAVE_XKB
QString deadLegend(xkb_keysym_t sym) {
    char name[64];
    if (xkb_keysym_get_name(sym, name, sizeof name) <= 0) return {};
    static const QHash<QString, QString> dead = {
        {"grave", "`"}, {"acute", "´"}, {"circumflex", "^"}, {"tilde", "~"}, {"diaeresis", "¨"},
        {"cedilla", "¸"}, {"abovedot", "˙"}, {"breve", "˘"}, {"caron", "ˇ"}, {"doubleacute", "˝"},
        {"ogonek", "˛"}, {"abovering", "˚"}, {"macron", "¯"},
    };
    const QString n = QString::fromLatin1(name);
    return n.startsWith(QLatin1String("dead_")) ? dead.value(n.mid(5)) : QString();
}

QString symLegend(xkb_keysym_t sym) {
    if (const uint32_t u = xkb_keysym_to_utf32(sym); u >= 0x20 && u != 0x7f)
        return QString::fromUcs4(reinterpret_cast<const char32_t *>(&u), 1);
    return deadLegend(sym);
}
#endif

} // namespace

QString activeLayoutName() {
    const auto &[l, v] = layout();
    return v.isEmpty() ? l : QStringLiteral("%1(%2)").arg(l, v);
}

QHash<int, QString> legends(const QList<Key> &keys) {
    QHash<int, QString> out;
    for (const Key &k : keys) if (!k.bar && !out.contains(k.code) && !k.label.isEmpty()) out.insert(k.code, k.label);
#ifdef LPM_HAVE_XKB
    const auto &[lay, var] = layout();
    const QByteArray l = lay.toUtf8(), v = var.toUtf8();
    xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    if (!ctx) return out;
    const xkb_rule_names names{nullptr, nullptr, l.constData(), v.isEmpty() ? nullptr : v.constData(), nullptr};
    xkb_keymap *km = xkb_keymap_new_from_names(ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
    if (!km) { xkb_context_unref(ctx); return out; }
    xkb_state *base = xkb_state_new(km), *shift = xkb_state_new(km);
    const xkb_mod_index_t si = xkb_keymap_mod_get_index(km, XKB_MOD_NAME_SHIFT);
    if (base && shift && si != XKB_MOD_INVALID) {
        xkb_state_update_mask(shift, 1u << si, 0, 0, 0, 0, 0);
        for (const Key &k : keys) {
            if (!k.evdev) continue;
            const xkb_keycode_t kc = xkb_keycode_t(k.evdev + 8);
            QString s = symLegend(xkb_state_key_get_one_sym(base, kc));
            if (s.isEmpty()) continue;
            // Letters: print the shifted form, which the layout defines (i → İ on TR, not I).
            if (s.at(0).isLetter()) {
                const QString up = symLegend(xkb_state_key_get_one_sym(shift, kc));
                s = (!up.isEmpty() && up.at(0).isLetter()) ? up : s.toUpper();
            }
            out.insert(k.code, s);
        }
    }
    if (shift) xkb_state_unref(shift);
    if (base) xkb_state_unref(base);
    xkb_keymap_unref(km);
    xkb_context_unref(ctx);
#endif
    return out;
}

} // namespace kblayout
