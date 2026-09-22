#include "privileged.h"
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
    auto fail = [&](const QString &msg) {
        Result r; r.error = msg;
        QTimer::singleShot(0, ctx, [cb, r] { cb(r); });
    };
    const QString pkexec = QStandardPaths::findExecutable(QStringLiteral("pkexec"));
    if (pkexec.isEmpty()) return fail(QStringLiteral("pkexec was not found. Install polkit (sys-auth/polkit)."));
    if (!QFileInfo(helper).isFile()) return fail(QStringLiteral("helper not found: ") + helper);

    auto *proc = new QProcess(ctx);
    auto *timer = new QTimer(proc);
    timer->setSingleShot(true);
    auto done = std::make_shared<bool>(false);
    QPointer<QObject> guard(ctx);

    auto finish = [=](Result r) {
        if (*done) return;
        *done = true;
        timer->stop();
        proc->deleteLater();
        if (guard) cb(r);
    };

    QObject::connect(timer, &QTimer::timeout, proc, [=] {
        proc->kill();
        Result r;
        r.error = QStringLiteral("the helper did not finish in time. If an authorization dialog is still open, close it and try again.");
        finish(r);
    });
    QObject::connect(proc, &QProcess::errorOccurred, proc, [=](QProcess::ProcessError e) {
        if (e != QProcess::FailedToStart) return;
        Result r; r.error = QStringLiteral("could not start pkexec: ") + proc->errorString();
        finish(r);
    });
    QObject::connect(proc, &QProcess::finished, proc, [=](int code, QProcess::ExitStatus) {
        Result r;
        const QByteArray out = proc->readAllStandardOutput().trimmed();
        const QString err = QString::fromUtf8(proc->readAllStandardError()).trimmed();
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

    proc->setProgram(pkexec);
    proc->setArguments({helper});
    proc->setProcessChannelMode(QProcess::SeparateChannels);
    proc->start();
    proc->write(payload);
    proc->closeWriteChannel();
    timer->start(timeoutMs);
}

} // namespace privileged
