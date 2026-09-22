#pragma once
// Run a root helper through pkexec, asynchronously — port of privileged.py.
//
// Unlike the Python version (blocking subprocess.run on the GUI thread, which
// froze the window while the polkit dialog was up), this is non-blocking:
// the callback fires on the GUI thread when the helper finishes.
#include <QJsonObject>
#include <QObject>
#include <QString>
#include <functional>

namespace privileged {

inline constexpr const char *INSTALL_PREFIX = LPM_HELPER_DIR;

/// Installed helper path (the one the polkit actions pin).
QString helperPath(const QString &relative);

struct Result {
    bool reached = false;   // false: pkexec/auth failure; `error` explains
    QJsonObject json;       // helper's own reply (ok/error/...) when reached
    QString error;
    bool ok() const { return reached && json.value(QStringLiteral("ok")).toBool(); }
    QString message() const;  // error text for either failure kind
};

using Callback = std::function<void(const Result &)>;

/// Starts `pkexec helper`, writes `payload` to stdin. `context` bounds the
/// callback's lifetime (not invoked if context is destroyed first).
void run(const QString &helper, const QJsonObject &payload, QObject *context, Callback cb,
         int timeoutMs = 60000);
void run(const QString &helper, const QByteArray &payload, QObject *context, Callback cb,
         int timeoutMs = 60000);

} // namespace privileged
