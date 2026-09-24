#include "privileged.h"
#include <QCoreApplication>
#include <QFileInfo>
#include <QJsonDocument>
#include <QPointer>
#include <QProcess>
#include <QStandardPaths>
#include <QTimer>

namespace privileged {

static constexpr int EXIT_DISMISSED = 126, EXIT_NOT_AUTHORIZED = 127;

QString helperPath(const QString &relative) {
    return QStringLiteral(LPM_HELPER_DIR "/") + relative;
}

QString Result::message() const {
    if (!reached) return error;
    const QString e = json.value(QStringLiteral("error")).toString();
    return e.isEmpty() ? json.value(QStringLiteral("message")).toString() : e;
}

void run(const QString &helper, const QJsonObject &payload, QObject *ctx, Callback cb, int timeoutMs) {
    run(helper, QJsonDocument(payload).toJson(QJsonDocument::Compact), ctx, std::move(cb), timeoutMs);
}

void run(const QString &helper, const QByteArray &payload, QObject *ctx, Callback cb, int timeoutMs) {
    QPointer<QObject> guard(ctx);
    auto fail = [&](const QString &msg) {
        Result r; r.error = msg;
        QTimer::singleShot(0, ctx, [cb, r] { cb(r); });
    };
    // Fixed locations, like the Rust side: never resolve pkexec through $PATH.
    QString pkexec;
    for (const char *c : {"/usr/bin/pkexec", "/bin/pkexec"})
        if (QFileInfo(QString::fromLatin1(c)).isFile()) { pkexec = QString::fromLatin1(c); break; }
    if (pkexec.isEmpty()) return fail(QStringLiteral("pkexec was not found. Install polkit (sys-auth/polkit)."));
    if (!QFileInfo(helper).isFile()) return fail(QStringLiteral("helper not found: ") + helper);

    // Not parented to `ctx`: a QProcess destructor kills *and waits* for its
    // child. Once pkexec has exec'd the root helper, our kill() is refused
    // (EPERM), so destroying a still-running QProcess — on timeout, or when
    // the owning tab goes away — blocked the GUI thread until the helper
    // exited on its own. The process now lives until it has really finished
    // and only then deletes itself; `guard` decides whether the callback runs.
    auto *proc = new QProcess(QCoreApplication::instance());
    auto *timer = new QTimer(proc);
    timer->setSingleShot(true);
    auto done = std::make_shared<bool>(false);

    auto finish = [=](const Result &r) {
        if (*done) return;
        *done = true;
        timer->stop();
        if (guard) cb(r);
    };
    auto reap = [proc] {
        if (proc->state() == QProcess::NotRunning) proc->deleteLater();
        // else: deleted from the finished() handler once it really exits.
    };

    QObject::connect(timer, &QTimer::timeout, proc, [=] {
        proc->kill();  // works while pkexec still waits for authorization
        Result r;
        r.error = QStringLiteral("the helper did not finish in time. If an authorization dialog is still open, close it and try again.");
        finish(r);
        reap();
    });
    QObject::connect(proc, &QProcess::errorOccurred, proc, [=](QProcess::ProcessError e) {
        if (e != QProcess::FailedToStart) return;
        Result r; r.error = QStringLiteral("could not start pkexec: ") + proc->errorString();
        finish(r);
        reap();
    });
    QObject::connect(proc, &QProcess::finished, proc, [=](int code, QProcess::ExitStatus) {
        proc->deleteLater();
        if (*done) return;  // already reported (timeout); just clean up
        Result r;
        const QByteArray out = proc->readAllStandardOutput().trimmed();
        const QString err = QString::fromUtf8(proc->readAllStandardError().left(4096)).trimmed();
        // The helper prints exactly one JSON line; take the last line in case
        // anything else slipped onto stdout first.
        const QByteArray last = out.mid(out.lastIndexOf('\n') + 1);
        const QJsonDocument doc = QJsonDocument::fromJson(last);
        if (doc.isObject() && !doc.object().isEmpty()) {
            r.reached = true;
            r.json = doc.object();
        } else if (code == EXIT_DISMISSED) {
            r.error = QStringLiteral("the authorization request was dismissed.");
        } else if (code == EXIT_NOT_AUTHORIZED) {
            r.error = QStringLiteral("polkit did not authorize this action") +
                      (err.isEmpty() ? QStringLiteral(".") : QStringLiteral(": ") + err);
            if (!helper.startsWith(QStringLiteral(LPM_HELPER_DIR "/")))
                r.error += QStringLiteral("\n\nThis helper is outside " LPM_HELPER_DIR
                                          ", so the pinned polkit actions do not apply to it.");
        } else {
            r.error = err.isEmpty() ? QStringLiteral("helper exited with code %1.").arg(code) : err;
        }
        finish(r);
    });
    // A runaway helper must not grow the GUI's memory without bound: helpers
    // answer with one short JSON line, so anything past 4 MiB is garbage.
    QObject::connect(proc, &QProcess::readyReadStandardOutput, proc, [proc] {
        if (proc->bytesAvailable() > 4 * 1024 * 1024) proc->kill();
    });

    proc->setProgram(pkexec);
    proc->setArguments({helper});
    proc->setProcessChannelMode(QProcess::SeparateChannels);
    proc->start();
    if (proc->state() == QProcess::NotRunning) return;  // FailedToStart already handled
    proc->write(payload);
    proc->closeWriteChannel();
    timer->start(timeoutMs);
}

} // namespace privileged
