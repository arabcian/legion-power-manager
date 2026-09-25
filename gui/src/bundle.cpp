#include "bundle.h"
#include "inteltab.h"
#include "optimizetab.h"
#include "privileged.h"
#include "ryzentab.h"
#include "scenes.h"
#include "sysinfo.h"
#include <QCoreApplication>
#include <QDateTime>
#include <QDir>
#include <QFile>
#include <QJsonDocument>
#include <QJsonObject>
#include <QMessageBox>
#include <QPointer>
#include <QPushButton>
#include <QRegularExpression>
#include <QSaveFile>
#include <QStandardPaths>
#include <memory>

namespace bundle {

static constexpr const char *FORMAT = "legion-power-manager-bundle";
static constexpr qint64 MAX_ITEM = 256 * 1024, MAX_BUNDLE = 16 * 1024 * 1024;

struct Cat { const char *key; QString dir; const char *label; bool root; };

static QList<Cat> cats() {
    return {
        {"scenes", scenes::dir(), "scene(s)", false},
        {"tune_presets", OptimizeTab::presetsDir(), "Optimizations preset(s)", false},
        {"ryzen_profiles", ryzen::profilesDir(), "Ryzen CO profile(s)", false},
        {"intel_profiles", inteluv::profilesDir(), "Intel undervolt profile(s)", false},
        {"nvidia_profiles", QStringLiteral("/etc/nvcurve/profiles"), "NVIDIA curve profile(s)", true},
    };
}

static QString configDir() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QStringLiteral("/legion-power-manager");
}
/// Single settings files: bundle key → path.
static QList<std::pair<const char *, QString>> singles() {
    return {{"scenes_auto", configDir() + "/scenes.json"}, {"game_launch", OptimizeTab::configFile()}};
}

static bool safeName(const QString &n) {
    static const QRegularExpression re(QStringLiteral("^[A-Za-z0-9][A-Za-z0-9 _.()+-]{0,63}$"));
    return re.match(n).hasMatch() && !n.contains(QLatin1String(".."));
}

static QJsonObject readObj(const QString &path) {
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly) || f.size() > MAX_ITEM) return {};
    return QJsonDocument::fromJson(f.readAll()).object();
}

static bool writeObj(const QString &path, const QJsonObject &o) {
    if (!QDir().mkpath(QFileInfo(path).absolutePath())) return false;
    QSaveFile f(path);
    return f.open(QIODevice::WriteOnly) && f.write(QJsonDocument(o).toJson()) >= 0 && f.commit();
}

static QJsonObject machine() {
    return {{"product", sysinfo::dmiClean("product_version").value_or(sysinfo::dmiClean("product_name").value_or(QString()))},
            {"bios", sysinfo::dmiClean("bios_version").value_or(QString())}};
}

QString exportTo(const QString &path, QString *err) {
    QJsonObject b{{"format", FORMAT}, {"version", 1},
                  {"created", QDateTime::currentDateTime().toString(Qt::ISODate)}, {"machine", machine()}};
    QStringList parts;
    for (const Cat &c : cats()) {
        QJsonObject items;
        for (QString f : QDir(c.dir).entryList({"*.json"}, QDir::Files | QDir::Readable, QDir::Name)) {
            const QJsonObject o = readObj(c.dir + '/' + f);
            f.chop(5);
            if (!o.isEmpty() && safeName(f)) items[f] = o;
        }
        if (!items.isEmpty()) { b[QLatin1String(c.key)] = items; parts << QStringLiteral("%1 %2").arg(items.size()).arg(c.label); }
    }
    for (const auto &[key, file] : singles()) {
        const QJsonObject o = readObj(file);
        if (!o.isEmpty()) b[QLatin1String(key)] = o;
    }
    if (parts.isEmpty() && !b.contains("game_launch")) { if (err) *err = "nothing to export yet"; return {}; }
    QSaveFile f(path);
    if (!f.open(QIODevice::WriteOnly) || f.write(QJsonDocument(b).toJson()) < 0 || !f.commit()) {
        if (err) *err = f.errorString();
        return {};
    }
    return "Exported " + (parts.isEmpty() ? QStringLiteral("settings") : parts.join(", ")) + '.';
}

void importFrom(const QString &path, QWidget *parent, std::function<void(const QString &)> done) {
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly) || f.size() > MAX_BUNDLE) { done("Import failed: cannot read " + path); return; }
    const QJsonObject b = QJsonDocument::fromJson(f.readAll()).object();
    if (b.value("format").toString() != QLatin1String(FORMAT) || b.value("version").toInt() != 1) {
        done("Import failed: not a Legion Power Manager bundle (or a newer version).");
        return;
    }

    // Plan: what is in it, what already exists here.
    struct Entry { Cat cat; QString name; QJsonObject obj; bool exists; };
    QList<Entry> entries;
    QStringList summary;
    int conflicts = 0, rejected = 0;
    for (const Cat &c : cats()) {
        const QJsonObject items = b.value(QLatin1String(c.key)).toObject();
        int n = 0;
        for (auto it = items.begin(); it != items.end(); ++it) {
            const QJsonDocument doc(it.value().toObject());
            if (!safeName(it.key()) || !it.value().isObject() || doc.toJson(QJsonDocument::Compact).size() > MAX_ITEM) { ++rejected; continue; }
            const bool ex = QFile::exists(c.dir + '/' + it.key() + ".json");
            conflicts += ex;
            entries.append({c, it.key(), it.value().toObject(), ex});
            ++n;
        }
        if (n) summary << QStringLiteral("%1 %2").arg(n).arg(c.label);
    }
    QList<std::pair<QString, QJsonObject>> singleWrites;
    for (const auto &[key, file] : singles()) {
        const QJsonObject o = b.value(QLatin1String(key)).toObject();
        if (o.isEmpty()) continue;
        const bool ex = QFile::exists(file);
        conflicts += ex;
        singleWrites.append({file, o});
        summary << (QLatin1String(key) == QLatin1String("scenes_auto") ? QStringLiteral("automatic switching") : QStringLiteral("game launch settings"));
        if (ex) singleWrites.back().second["__exists"] = true;
    }
    if (entries.isEmpty() && singleWrites.isEmpty()) { done("Import: the bundle is empty."); return; }

    const QJsonObject from = b.value("machine").toObject(), here = machine();
    QString text = QStringLiteral("Bundle from %1 (%2):\n\n• %3")
        .arg(from.value("product").toString("unknown machine"), b.value("created").toString().left(10), summary.join("\n• "));
    if (rejected) text += QStringLiteral("\n\n%1 item(s) with invalid names were left out.").arg(rejected);
    if (from.value("product") != here.value("product") || from.value("bios") != here.value("bios"))
        text += QStringLiteral("\n\n⚠ Made on %1 / BIOS %2 — this machine is %3 / BIOS %4. Curve Optimizer offsets, undervolts and "
                               "GPU curves are specific to one chip: check them before applying.")
                    .arg(from.value("product").toString("?"), from.value("bios").toString("?"),
                         here.value("product").toString("?"), here.value("bios").toString("?"));

    QMessageBox box(QMessageBox::Question, "Import settings", text, QMessageBox::NoButton, parent);
    QPushButton *overwrite = nullptr, *keep = nullptr, *go = nullptr;
    if (conflicts) {
        box.setInformativeText(QStringLiteral("%1 item(s) already exist here.").arg(conflicts));
        overwrite = box.addButton("Overwrite existing", QMessageBox::AcceptRole);
        keep = box.addButton("Keep existing", QMessageBox::AcceptRole);
    } else {
        go = box.addButton("Import", QMessageBox::AcceptRole);
    }
    box.addButton(QMessageBox::Cancel);
    box.exec();
    if (box.clickedButton() != overwrite && box.clickedButton() != keep && box.clickedButton() != go) { done({}); return; }
    const bool replace = box.clickedButton() != keep;

    // Scene "command" fields are programs this app starts on its own (at
    // login, on every AC/battery switch, from game hooks). One arriving in a
    // file from somewhere else must be seen and accepted explicitly.
    QStringList commands;
    for (const Entry &e : entries)
        if (QLatin1String(e.cat.key) == QLatin1String("scenes") && !(e.exists && !replace)) {
            const QString c = e.obj.value("command").toString().trimmed();
            if (!c.isEmpty()) commands << QStringLiteral("%1:  %2").arg(e.name, c);
        }
    bool keepCommands = true;
    if (!commands.isEmpty()) {
        QMessageBox cb(QMessageBox::Warning, "Import settings",
                       QStringLiteral("%1 scene(s) in this bundle run a command when the scene is applied — "
                                      "automatically at login, on AC/battery changes or at game start:").arg(commands.size()),
                       QMessageBox::NoButton, parent);
        cb.setInformativeText("Only keep them if you know exactly what they do.");
        cb.setDetailedText(commands.join('\n'));
        QPushButton *strip = cb.addButton("Import without commands", QMessageBox::AcceptRole);
        QPushButton *withCmd = cb.addButton("Import with commands", QMessageBox::DestructiveRole);
        cb.addButton(QMessageBox::Cancel);
        cb.setDefaultButton(strip);
        cb.exec();
        if (cb.clickedButton() != strip && cb.clickedButton() != withCmd) { done({}); return; }
        keepCommands = cb.clickedButton() == withCmd;
    }

    int written = 0, skipped = 0, failed = 0;
    QList<Entry> nvidia;
    for (const Entry &e : entries) {
        if (e.exists && !replace) { ++skipped; continue; }
        if (e.cat.root) { nvidia.append(e); continue; }
        QJsonObject obj = e.obj;
        if (!keepCommands && QLatin1String(e.cat.key) == QLatin1String("scenes")) obj.remove("command");
        writeObj(e.cat.dir + '/' + e.name + ".json", obj) ? ++written : ++failed;
    }
    for (auto [file, o] : singleWrites) {
        const bool ex = o.take("__exists").toBool();
        if (ex && !replace) { ++skipped; continue; }
        writeObj(file, o) ? ++written : ++failed;
    }

    auto result = [=](int nvOk, const QStringList &nvErr) {
        QString r = QStringLiteral("Imported %1 item(s)").arg(written + nvOk);
        if (skipped) r += QStringLiteral(", kept %1 existing").arg(skipped);
        if (failed) r += QStringLiteral(", %1 could not be written").arg(failed);
        if (!nvErr.isEmpty()) r += QStringLiteral(". NVIDIA: ") + nvErr.join("; ");
        return r.endsWith('.') ? r : r + '.';
    };
    if (nvidia.isEmpty()) { done(result(0, {})); return; }

    // NVIDIA profiles live in /etc/nvcurve/profiles: one pkexec each, in
    // sequence; the helper validates every profile before writing it.
    auto state = std::make_shared<std::pair<int, QStringList>>(0, QStringList{});
    // The step holds itself only weakly; the pending helper callback holds the
    // strong reference, so the chain frees itself after the last profile.
    auto step = std::make_shared<std::function<void(int)>>();
    std::weak_ptr<std::function<void(int)>> weak = step;
    // Context for the helper callbacks: the dialog's parent, or the app when
    // none was given (a null QPointer would read as "parent destroyed").
    QPointer<QObject> ctx(parent ? static_cast<QObject *>(parent) : QCoreApplication::instance());
    *step = [=](int i) {
        if (i >= nvidia.size() || !ctx) { done(result(state->first, state->second)); return; }
        const Entry &e = nvidia[i];
        const QJsonObject req{{"op", "write_nvcurve_profile"}, {"name", e.name},
                              {"content", QString::fromUtf8(QJsonDocument(e.obj).toJson(QJsonDocument::Compact))}};
        auto self = weak.lock();
        privileged::run(privileged::helperPath("nvcurve-root-helper"), req, ctx, [=, name = e.name](const privileged::Result &r) {
            if (r.ok()) ++state->first; else state->second << name + ": " + r.message();
            (*self)(i + 1);
        });
    };
    (*step)(0);
}

} // namespace bundle
