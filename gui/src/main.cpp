// Legion Power Manager — C++/Qt6 front end.
// Root work is done by the Rust helpers in LPM_HELPER_DIR via pkexec.
//
//   legion-power-manager            start in the tray (default)
//   legion-power-manager --window   start with the window open
//
// A second launch never starts a second copy: it asks the running one to
// show its window (or, with no --window, just exits quietly).
#include "mainwindow.h"
#include "theme.h"
#include "tray.h"
#include <QApplication>
#include <QLocalServer>
#include <QLocalSocket>
#include <QTabWidget>
#include <QTimer>
#include <cstdio>
#include <unistd.h>

static constexpr int TRAY_RETRY_MS = 400, TRAY_MAX_RETRIES = 15;

static QString instanceKey() { return QStringLiteral("legion-power-manager-%1").arg(getuid()); }

/// true if another instance answered (and was asked to show itself if `raise`).
static bool pingRunning(bool raise) {
    QLocalSocket s;
    s.connectToServer(instanceKey());
    if (!s.waitForConnected(500)) return false;
    s.write(raise ? "raise\n" : "noop\n");
    s.waitForBytesWritten(500);
    s.disconnectFromServer();
    return true;
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
    if (pingRunning(withWindow)) return 0;

    theme::apply(app);
    MainWindow win;

    QLocalServer::removeServer(instanceKey());  // stale socket from a crash
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
