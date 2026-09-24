#include "scenes.h"
#include "fwattrtab.h"
#include "hometab.h"
#include "inteltab.h"
#include "mainwindow.h"
#include "nvidiatab.h"
#include "optimizetab.h"
#include "platformprofile.h"
#include "privileged.h"
#include "ryzentab.h"
#include <QDir>
#include <QFile>
#include <QJsonArray>
#include <QJsonDocument>
#include <QProcess>
#include <QRegularExpression>
#include <QSaveFile>
#include <QStandardPaths>
#include <QTimer>

namespace scenes {

static constexpr qint64 MAX_FILE = 64 * 1024;

static QString configDir() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QStringLiteral("/legion-power-manager");
}
QString dir() { return configDir() + QStringLiteral("/scenes"); }
static QString autoFile() { return configDir() + QStringLiteral("/scenes.json"); }
static QString sceneFile(const QString &n) { return dir() + '/' + n + QStringLiteral(".json"); }

bool validName(const QString &n) {
    // Same rule as the Ryzen/Intel profile names: it becomes a file name.
    static const QRegularExpression re(QStringLiteral("^[A-Za-z0-9][A-Za-z0-9 _-]{0,63}$"));
    return re.match(n).hasMatch();
}

static QJsonObject readObject(const QString &path) {
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly) || f.size() > MAX_FILE) return {};
    return QJsonDocument::fromJson(f.readAll()).object();
}

static bool writeObject(const QString &path, const QJsonObject &o, QString *err) {
    if (!QDir().mkpath(QFileInfo(path).absolutePath())) { if (err) *err = "cannot create " + QFileInfo(path).absolutePath(); return false; }
    QSaveFile f(path);
    if (!f.open(QIODevice::WriteOnly) || f.write(QJsonDocument(o).toJson()) < 0 || !f.commit()) {
        if (err) *err = f.errorString();
        return false;
    }
    return true;
}

static Choice choiceFrom(const QJsonValue &v) {
    const QJsonObject o = v.toObject();
    if (o.value("reset").toBool()) return {Choice::Reset, {}};
    if (const QString n = o.value("profile").toString(); !n.isEmpty()) return {Choice::Profile, n};
    return {};
}

static QJsonValue choiceTo(const Choice &c) {
    switch (c.kind) {
    case Choice::Reset: return QJsonObject{{"reset", true}};
    case Choice::Profile: return QJsonObject{{"profile", c.name}};
    case Choice::Unchanged: break;
    }
    return QJsonValue::Undefined;
}

QStringList names() {
    QStringList out;
    for (QString f : QDir(dir()).entryList({"*.json"}, QDir::Files | QDir::Readable, QDir::Name)) {
        f.chop(5);
        if (validName(f)) out << f;
    }
    return out;
}

std::optional<Scene> load(const QString &name) {
    if (!validName(name) || !QFile::exists(sceneFile(name))) return std::nullopt;
    const QJsonObject o = readObject(sceneFile(name));
    Scene s;
    s.name = name;
    if (const QString p = o.value("platform_profile").toString(); pp::VALID_PROFILES.contains(p)) s.platformProfile = p;
    const QJsonObject fw = o.value("firmware").toObject();
    for (auto it = fw.begin(); it != fw.end(); ++it)
        if (it->isDouble()) s.firmware.insert(it.key(), it->toInt());
    s.cpu = choiceFrom(o.value("cpu_curve"));
    s.gpu = choiceFrom(o.value("gpu_curve"));
    s.tuning = choiceFrom(o.value("tuning"));
    s.command = o.value("command").toString().trimmed();
    return s;
}

bool save(const Scene &s, QString *err) {
    if (!validName(s.name)) { if (err) *err = "invalid scene name"; return false; }
    QJsonObject o{{"version", 1}};
    if (!s.platformProfile.isEmpty()) o["platform_profile"] = s.platformProfile;
    if (!s.firmware.isEmpty()) {
        QJsonObject fw;
        for (auto it = s.firmware.cbegin(); it != s.firmware.cend(); ++it) fw[it.key()] = it.value();
        o["firmware"] = fw;
    }
    for (const auto &[key, c] : {std::pair{"cpu_curve", s.cpu}, {"gpu_curve", s.gpu}, {"tuning", s.tuning}})
        if (c.kind != Choice::Unchanged) o[QLatin1String(key)] = choiceTo(c);
    if (!s.command.isEmpty()) o["command"] = s.command;
    return writeObject(sceneFile(s.name), o, err);
}

bool remove(const QString &name) { return validName(name) && QFile::remove(sceneFile(name)); }

Auto loadAuto() {
    const QJsonObject o = readObject(autoFile());
    Auto a;
    a.enabled = o.value("auto").toBool();
    a.onAc = o.value("on_ac").toString();
    a.onBattery = o.value("on_battery").toString();
    if (!validName(a.onAc)) a.onAc.clear();
    if (!validName(a.onBattery)) a.onBattery.clear();
    return a;
}

bool saveAuto(const Auto &a, QString *err) {
    return writeObject(autoFile(), {{"auto", a.enabled}, {"on_ac", a.onAc}, {"on_battery", a.onBattery}}, err);
}

std::optional<bool> onAc() {
    // Charger first: a Legion under full load can draw more than its brick
    // delivers, so the battery reads "Discharging" while plugged in — that
    // must not flip the machine into the battery scene mid-game.
    const QDir d(QStringLiteral("/sys/class/power_supply"));
    bool anySupply = false, anyBattery = false, discharging = false;
    for (const QString &e : d.entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name)) {
        const QString p = d.filePath(e);
        const QString type = pp::readText(p + "/type").value_or(QString());
        if (type == QLatin1String("Mains") || type == QLatin1String("USB")) {  // USB = UCSI / PD source
            anySupply = true;
            if (pp::readText(p + "/online").value_or(QString()) == QLatin1String("1")) return true;
        } else if (type == QLatin1String("Battery")) {
            anyBattery = true;
            discharging |= pp::readText(p + "/status").value_or(QString()) == QLatin1String("Discharging");
        }
    }
    if (anySupply) return false;              // chargers present, none online
    if (anyBattery) return !discharging;      // no charger nodes: go by the battery
    return std::nullopt;
}

} // namespace scenes

// ── engine ──────────────────────────────────────────────────────────────────

using namespace scenes;

static constexpr int POWER_POLL_MS = 3000, STABLE_READS = 2, STARTUP_DELAY_MS = 4000;

SceneEngine::SceneEngine(MainWindow *win) : QObject(win), win_(win), auto_(loadAuto()) {
    powerTimer_ = new QTimer(this);
    powerTimer_->setInterval(POWER_POLL_MS);
    connect(powerTimer_, &QTimer::timeout, this, &SceneEngine::pollPower);
    ac_ = onAc();
    powerTimer_->start();  // cheap: a handful of sysfs reads every 3 s
    // Session start: bring the machine to the scene for the current source,
    // after the tabs have finished their own startup reads.
    if (auto_.enabled && ac_)
        QTimer::singleShot(STARTUP_DELAY_MS, this, [this] { if (auto_.enabled && ac_) applyForSource(*ac_); });
}

bool SceneEngine::setAuto(const Auto &a, QString *err) {
    const bool wasOn = auto_.enabled;
    if (!saveAuto(a, err)) return false;
    auto_ = a;
    if (a.enabled && !wasOn && ac_) applyForSource(*ac_);
    return true;
}

void SceneEngine::pollPower() {
    const auto now = onAc();
    if (!now) return;
    if (!ac_) { ac_ = now; return; }
    if (*now == *ac_) { candidate_.reset(); stableReads_ = 0; return; }
    // Debounce: a loose plug or a PD renegotiation blips for a second.
    if (candidate_ != now) { candidate_ = now; stableReads_ = 1; return; }
    if (++stableReads_ < STABLE_READS) return;
    ac_ = now;
    candidate_.reset();
    stableReads_ = 0;
    Q_EMIT powerSourceChanged(*ac_);
    if (auto_.enabled) applyForSource(*ac_);
}

void SceneEngine::applyForSource(bool onAc) {
    const QString n = onAc ? auto_.onAc : auto_.onBattery;
    if (!n.isEmpty()) apply(n);
}

void SceneEngine::apply(const QString &name) {
    if (busy_) { pending_ = name; return; }
    const auto s = load(name);
    if (!s) {
        Q_EMIT finished(name, false, {QStringLiteral("✗ scene '%1' not found").arg(name)});
        return;
    }
    start(*s);
}

void SceneEngine::addStep(const QString &what, Step step) { steps_.append({what, std::move(step)}); }

void SceneEngine::helper(const QString &name, const QJsonObject &req, Done done,
                         std::function<QString(const QJsonObject &)> describe) {
    privileged::run(privileged::helperPath(name), req, this, [done, describe](const privileged::Result &r) {
        if (!r.ok()) { done(false, r.message().isEmpty() ? QStringLiteral("failed") : r.message()); return; }
        done(true, describe ? describe(r.json) : QString());
    }, 120000);
}

void SceneEngine::start(const Scene &s) {
    busy_ = true;
    ok_ = true;
    log_.clear();
    steps_.clear();
    current_ = s.name;
    Q_EMIT started(s.name);

    // 1. Platform profile — first: firmware limits depend on it being Custom.
    if (!s.platformProfile.isEmpty()) {
        addStep("Power profile", [this, p = s.platformProfile](Done done) {
            const auto h = pp::primaryHandler();
            QJsonObject req{{"profile", p}};
            if (h) req["handler"] = h->node;
            helper("legion-profile-helper", req, [this, p, done](bool ok, const QString &msg) {
                if (ok) Q_EMIT win_->home()->profileChanged(p);  // Firmware tab relocks now, not at its next poll
                done(ok, ok ? HomeTab::profileLabel(p) : msg);
            }, [](const QJsonObject &j) {
                const QString eff = j.value("effective").toString();
                return eff.isEmpty() ? QString() : HomeTab::profileLabel(eff);
            });
        });
    }

    // 2. Firmware limits (sysfs attributes + the WMI-only GPU knobs).
    if (!s.firmware.isEmpty()) {
        addStep("Firmware limits", [this, fw = s.firmware](Done done) {
            if (pp::currentProfile(pp::primaryHandler()) != QStringLiteral("custom")) {
                done(false, "needs the Custom power profile (set it in this scene)");
                return;
            }
            QJsonArray batch;
            QJsonObject wmi;
            int unknown = 0;
            QMap<QString, FwAttr> byName;
            for (const FwAttr &a : FwattrTab::discover()) byName.insert(a.name, a);
            for (auto it = fw.cbegin(); it != fw.cend(); ++it) {
                const auto a = byName.constFind(it.key());
                if (a == byName.cend()) { ++unknown; continue; }
                if (a->viaWmi()) wmi[a->wmiKey] = it.value();
                else if (a->ranged) batch.append(QJsonObject{{"path", a->path}, {"value", it.value()}});
                else ++unknown;
            }
            const QString note = unknown ? QStringLiteral(" (%1 not present here)").arg(unknown) : QString();
            auto afterSysfs = [this, wmi, note, done, n = batch.size()](bool ok, const QString &msg) {
                if (!ok) { done(false, msg); return; }
                if (wmi.isEmpty()) { done(true, QStringLiteral("%1 value(s)").arg(n) + note); return; }
                helper("legion-gpu-helper", {{"op", "apply"}, {"values", wmi}}, [done, n, note, w = wmi.size()](bool ok, const QString &m) {
                    done(ok, ok ? QStringLiteral("%1 value(s) + %2 GPU (WMI)").arg(n).arg(w) + note : "GPU (WMI): " + m);
                });
            };
            if (batch.isEmpty()) { afterSysfs(true, {}); return; }
            privileged::run(privileged::helperPath("fwattr-helper"), QJsonDocument(batch).toJson(QJsonDocument::Compact), this,
                                [afterSysfs](const privileged::Result &r) {
                                    if (r.ok()) { afterSysfs(true, {}); return; }
                                    QStringList bad;
                                    for (const auto &x : r.json.value("results").toArray())
                                        if (!x.toObject().value("ok").toBool()) bad << x.toObject().value("error").toString();
                                    afterSysfs(false, bad.isEmpty() ? r.message() : bad.join("; "));
                                }, 120000);
        });
    }

    // 3. CPU curve: Ryzen Curve Optimizer or Intel undervolt, by vendor.
    if (s.cpu.kind != Choice::Unchanged) {
        if (win_->ryzen()) {
            addStep("CPU curve", [this, c = s.cpu](Done done) {
                if (c.kind == Choice::Reset) { helper("ryzen-co-helper", {{"op", "reset"}}, [done](bool ok, const QString &m) { done(ok, ok ? "reset (0)" : m); }); return; }
                QFile f(ryzen::profilesDir() + '/' + c.name + ".json");
                if (!f.open(QIODevice::ReadOnly) || f.size() > 256 * 1024) { done(false, "profile '" + c.name + "' not found"); return; }
                const QJsonObject d = QJsonDocument::fromJson(f.readAll()).object();
                const int ccds = std::max(1, ryzen::detect().ccdCount);
                QJsonArray entries;
                for (const auto &v : d.value("cores").toArray()) {
                    const QJsonObject o = v.toObject();
                    const int ccd = o.value("ccd").toInt(), ccx = o.value("ccx").toInt();
                    const int slot = o.contains("slot") ? o.value("slot").toInt() : o.value("core").toInt();
                    if (ccx != 0 || ccd >= ccds || o.value("disabled").toBool() || !o.value("coper").isDouble()) continue;
                    entries.append(QJsonObject{{"ccd", ccd}, {"ccx", 0}, {"core", slot}, {"coper", o.value("coper").toInt()}});
                }
                const QJsonValue coall = d.value("coall");
                if (!coall.isDouble() && entries.isEmpty()) { done(false, "profile '" + c.name + "' has no offsets"); return; }
                auto perCore = [this, entries, done, name = c.name] {
                    if (entries.isEmpty()) { done(true, "'" + name + "'"); return; }
                    helper("ryzen-co-helper", {{"op", "set_coper_batch"}, {"params", QJsonObject{{"entries", entries}}}},
                           [done, name](bool ok, const QString &m) { done(ok, ok ? "'" + name + "'" : m); });
                };
                // All-core first, per-core on top — same order as the tab.
                if (coall.isDouble())
                    helper("ryzen-co-helper", {{"op", "set_coall"}, {"params", QJsonObject{{"value", coall.toInt()}}}},
                           [perCore, done](bool ok, const QString &m) { if (ok) perCore(); else done(false, m); });
                else perCore();
            });
        } else if (win_->intel()) {
            addStep("CPU undervolt", [this, c = s.cpu](Done done) {
                if (c.kind == Choice::Reset) { helper("intel-uv-helper", {{"op", "reset"}}, [done](bool ok, const QString &m) { done(ok, ok ? "reset (0 mV)" : m); }); return; }
                QFile f(inteluv::profilesDir() + '/' + c.name + ".json");
                if (!f.open(QIODevice::ReadOnly) || f.size() > 64 * 1024) { done(false, "profile '" + c.name + "' not found"); return; }
                helper("intel-uv-helper", {{"op", "apply"}, {"profile", QJsonDocument::fromJson(f.readAll()).object()}},
                       [done, name = c.name](bool ok, const QString &m) { done(ok, ok ? "'" + name + "'" : m); });
            });
        }
    }

    // 4. NVIDIA V/F curve.
    if (s.gpu.kind != Choice::Unchanged) {
        addStep("GPU curve", [this, c = s.gpu](Done done) {
            // Two NvAPI sessions writing the ClockBoostTable at once is asking for trouble.
            if (win_->nvidia()->busy()) { done(false, "the NVIDIA tab is busy; skipped"); return; }
            const QJsonObject req = c.kind == Choice::Reset ? QJsonObject{{"op", "reset_gpu_curve"}}
                                                            : QJsonObject{{"op", "apply_named_profile"}, {"name", c.name}};
            helper("nvcurve-root-helper", req, [done, c](bool ok, const QString &m) {
                done(ok, ok ? (c.kind == Choice::Reset ? QStringLiteral("reset") : "'" + c.name + "'") : m);
            });
        });
    }

    // 5. Optimizations preset — "replace": knobs the new preset does not set
    //    go back to their originals, and a running game's tuning is left alone.
    if (s.tuning.kind != Choice::Unchanged) {
        addStep("Optimizations", [this, c = s.tuning](Done done) {
            QJsonObject req{{"op", "apply"}, {"mode", "manual"}, {"replace", true}, {"values", QJsonObject()}};
            if (c.kind == Choice::Profile) {
                const QJsonObject p = win_->optimize()->presetObject(c.name);
                if (p.isEmpty()) { done(false, "preset '" + c.name + "' not found"); return; }
                req["values"] = p.value("values").toObject();
                req["preset"] = c.name;
            }
            privileged::run(privileged::helperPath("tune-helper"), req, this, [done, c](const privileged::Result &r) {
                if (r.reached && r.json.value("game_active").toBool()) { done(true, "left alone (game session active)"); return; }
                if (!r.ok()) { done(false, r.message()); return; }
                done(true, c.kind == Choice::Reset ? QStringLiteral("originals restored") : "'" + c.name + "'");
            }, 120000);
        });
    }

    // 6. User command (display mode, audio profile…) — as the user, no shell.
    if (!s.command.isEmpty()) {
        addStep("Command", [cmd = s.command](Done done) {
            QStringList argv = QProcess::splitCommand(cmd);
            if (argv.isEmpty()) { done(false, "empty command"); return; }
            const QString prog = argv.takeFirst();
            done(QProcess::startDetached(prog, argv), prog);
        });
    }

    if (steps_.isEmpty()) log_ << QStringLiteral("nothing to change (every component is 'unchanged')");
    next();
}

void SceneEngine::next() {
    if (steps_.isEmpty()) { finish(); return; }
    const auto [what, step] = steps_.takeFirst();
    step([this, what = what](bool ok, const QString &msg) {
        ok_ &= ok;
        log_ << (ok ? QString() : QStringLiteral("✗ ")) + what + (msg.isEmpty() ? QString() : QStringLiteral(": ") + msg);
        // Queued: a step may complete synchronously; never recurse through the chain.
        QTimer::singleShot(0, this, &SceneEngine::next);
    });
}

void SceneEngine::finish() {
    busy_ = false;
    active_ = current_;
    Q_EMIT finished(current_, ok_, log_);
    if (!pending_.isEmpty()) {
        const QString n = std::exchange(pending_, QString());
        apply(n);
    }
}
