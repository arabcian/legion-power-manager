// Legion Power Manager — C++/Qt6 front end.
// Root work is done by the Rust helpers in LPM_HELPER_DIR via pkexec.
//
//   legion-power-manager            start in the tray (default)
//   legion-power-manager --window   start with the window open
//
// Single instance, enforced: a per-user QLockFile (PID-checked, so a crash
// never leaves it stuck) is the authority; the local socket is only used to
// ask the running copy to show its window. A second launch with --window
// raises the running one, without it exits quietly; if the lock cannot be
// taken for any other reason the program refuses to start.
#include "mainwindow.h"
#include "theme.h"
#include "tray.h"
#include <QApplication>
#include <QDir>
#include <QElapsedTimer>
#include <QLockFile>
#include <QLocalServer>
#include <QLocalSocket>
#include <QStandardPaths>
#include <QTabWidget>
#include <QThread>
#include <QTimer>
#include <cstdio>
#include <unistd.h>

static constexpr int TRAY_RETRY_MS = 400, TRAY_MAX_RETRIES = 15;
static constexpr int PING_WINDOW_MS = 3000;  // the running copy may still be starting up

static QString instanceKey() { return QStringLiteral("legion-power-manager-%1").arg(getuid()); }

/// Per-user lock: $XDG_RUNTIME_DIR (0700, per user) or the temp dir with the uid in the name.
static QString lockPath() {
    QString dir = QStandardPaths::writableLocation(QStandardPaths::RuntimeLocation);
    if (dir.isEmpty() || !QDir(dir).exists()) dir = QDir::tempPath();
    return dir + '/' + instanceKey() + QStringLiteral(".lock");
}

/// Asks the running instance to show itself (or just pings it). Retries for a
/// short while because it may hold the lock but not be listening yet.
static bool pingRunning(bool raise) {
    QElapsedTimer t;
    t.start();
    do {
        QLocalSocket s;
        s.connectToServer(instanceKey());
        if (s.waitForConnected(300)) {
            s.write(raise ? "raise\n" : "noop\n");
            s.waitForBytesWritten(500);
            s.disconnectFromServer();
            return true;
        }
        QThread::msleep(100);
    } while (t.elapsed() < PING_WINDOW_MS);
    return false;
}

static void installTray(MainWindow *win, bool showWindow, int attempt = 0) {
    if (QSystemTrayIcon::isSystemTrayAvailable()) {
        auto *tray = new Tray(win);
        tray->show();
        win->setHideOnClose(true);
        return;
    }
    if (attempt < TRAY_MAX_RETRIES) {
        QTimer::singleShot(TRAY_RETRY_MS, win, [=] { installTray(win, showWindow, attempt + 1); });
        return;
    }
    // No tray after ~6 s: never leave the app running unreachable.
    std::fputs("legion-power-manager: no system tray available; showing the window instead.\n", stderr);
    win->show();
}

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    app.setApplicationName("Legion Power Manager");
    app.setApplicationVersion(LPM_VERSION);
    app.setDesktopFileName("legion-power-manager");
    app.setWindowIcon(appIcon(128));
    app.setQuitOnLastWindowClosed(false);  // the window is a view onto a tray app

    const QStringList args = app.arguments();
    if (args.contains("--version") || args.contains("-V")) { std::printf("legion-power-manager %s\n", LPM_VERSION); return 0; }
    const bool withWindow = args.contains("--window") || qEnvironmentVariableIsSet("LPM_SCREENSHOT");

    // Taken before any window, tray or helper exists; held until main() returns.
    QLockFile lock(lockPath());
    lock.setStaleLockTime(0);  // stale only if the owning PID is gone, never by age
    if (!lock.tryLock(0)) {
        if (lock.error() == QLockFile::LockFailedError) {
            qint64 pid = 0;
            QString host, name;
            lock.getLockInfo(&pid, &host, &name);
            if (!pingRunning(withWindow) && withWindow)
                std::fprintf(stderr, "legion-power-manager: already running (pid %lld) but it did not answer.\n", pid);
            return 0;
        }
        std::fprintf(stderr, "legion-power-manager: cannot take the instance lock %s (%s); refusing to start a second copy.\n",
                     qPrintable(lockPath()), lock.error() == QLockFile::PermissionError ? "permission denied" : "unknown error");
        return 1;
    }

    theme::apply(app);
    MainWindow win;

    // We own the lock, so any socket left behind is from a dead instance.
    QLocalServer::removeServer(instanceKey());
    QLocalServer server;
    server.setSocketOptions(QLocalServer::UserAccessOption);
    if (server.listen(instanceKey())) {
        QObject::connect(&server, &QLocalServer::newConnection, &win, [&] {
            QLocalSocket *c = server.nextPendingConnection();
            QObject::connect(c, &QLocalSocket::readyRead, &win, [c, &win] {
                if (c->readAll().startsWith("raise")) { win.showNormal(); win.raise(); win.activateWindow(); }
                c->disconnectFromServer();
                c->deleteLater();
            });
        });
    } else {
        std::fprintf(stderr, "legion-power-manager: instance socket unavailable (%s); a second launch cannot raise this window.\n",
                     qPrintable(server.errorString()));
    }

    installTray(&win, withWindow);
    if (withWindow) win.show();

    if (auto *t = win.findChild<QTabWidget *>(); t && qEnvironmentVariableIsSet("LPM_TAB"))
        t->setCurrentIndex(qEnvironmentVariableIntValue("LPM_TAB"));
    // Dev aid: LPM_SCREENSHOT=/path.png renders the window once and exits.
    if (const QByteArray shot = qgetenv("LPM_SCREENSHOT"); !shot.isEmpty())
        QTimer::singleShot(1500, &app, [&win, &app, shot] { win.grab().save(QString::fromLocal8Bit(shot)); app.quit(); });
    return app.exec();
}
