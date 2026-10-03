#include "netpage.h"

#include "privileged.h"
#include "theme.h"

#include <QCheckBox>
#include <QCollator>
#include <QClipboard>
#include <QDateTime>
#include <QDialog>
#include <QDialogButtonBox>
#include <QDir>
#include <QFile>
#include <QFileDialog>
#include <QFileInfo>
#include <QFileSystemWatcher>
#include <QFontDatabase>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QHideEvent>
#include <QHostAddress>
#include <QHostInfo>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QListWidget>
#include <QMenu>
#include <QMap>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QPointer>
#include <QProcess>
#include <QPushButton>
#include <QRegularExpression>
#include <QScrollBar>
#include <QSet>
#include <QShowEvent>
#include <QSignalBlocker>
#include <QTabWidget>
#include <QTimer>
#include <QTreeWidget>
#include <QVBoxLayout>

#include <algorithm>
#include <memory>
#include <unistd.h>
#include <pwd.h>

static constexpr int POLL_MS = 3000, MAX_BLOCK_ROWS = 1000, ALERT_GAP_MS = 10000;
static const char *const LOG_DIR = "/var/log/legion-power-manager";
static const char *const LOG_FILE = "/var/log/legion-power-manager/netguard.log";
static const char *const CONN_LOG = "/var/log/legion-power-manager/connections.log";
static constexpr int LOG_TAB = 2, MAX_LOG_ROWS = 3000;
static constexpr qint64 LOG_TAIL = 512 * 1024;  // of an existing log, only the end is loaded
enum { IdxRole = Qt::UserRole, KeyRole, ExeRole, IpRole, SortRole, PidRole, UidRole };

static QString helper() { return privileged::helperPath(QStringLiteral("netguard-helper")); }
static QString str(const QJsonObject &o, const char *k) { return o.value(QLatin1String(k)).toString(); }
static QString endpoint(const QString &ip, int port) {
    return (ip.contains(QLatin1Char(':')) ? QLatin1Char('[') + ip + QLatin1Char(']') : ip) + QLatin1Char(':') + QString::number(port);
}
static QString userName(uint uid) {
    const passwd *p = ::getpwuid(uid);
    return p ? QString::fromLocal8Bit(p->pw_name) : QString::number(uid);
}
static QString baseName(const QString &path) {
    return path.mid(std::max(path.lastIndexOf(QLatin1Char('/')), path.lastIndexOf(QLatin1Char('\\'))) + 1);
}

// Row that sorts by what the column holds, not by its text: numbers (PID, port) numerically,
// "ip", "ip:port" and "[v6]:port" by address then port, a SortRole value (the log's time) by that value.
class SortItem : public QTreeWidgetItem {
public:
    using QTreeWidgetItem::QTreeWidgetItem;
    bool operator<(const QTreeWidgetItem &o) const override {
        const int c = treeWidget() ? treeWidget()->sortColumn() : 0;
        const QVariant a = data(c, SortRole), b = o.data(c, SortRole);
        if (a.isValid() && b.isValid()) return a.toDouble() < b.toDouble();
        const Key x = key(text(c)), y = key(o.text(c));
        if (x.rank != y.rank) return x.rank < y.rank;
        if (x.rank == 0) return x.num < y.num;
        if (x.rank == 1) {
            if (x.v6 != y.v6) return !x.v6;
            const int r = x.bytes.compare(y.bytes);
            return r ? r < 0 : x.port < y.port;
        }
        static QCollator col = [] { QCollator k; k.setNumericMode(true); k.setCaseSensitivity(Qt::CaseInsensitive); return k; }();
        return col.compare(text(c), o.text(c)) < 0;
    }

private:
    struct Key { int rank = 2; double num = 0; bool v6 = false; QByteArray bytes; int port = 0; };
    static Key key(const QString &t) {
        Key k;
        bool ok = false;
        k.num = t.toDouble(&ok);
        if (ok) { k.rank = 0; return k; }
        QString ip = t;
        if (t.startsWith(QLatin1Char('['))) {
            const int e = t.indexOf(QStringLiteral("]:"));
            if (e > 0) { ip = t.mid(1, e - 1); k.port = t.mid(e + 2).toInt(); }
        } else if (t.count(QLatin1Char(':')) == 1) {
            const int e = t.indexOf(QLatin1Char(':'));
            ip = t.left(e);
            k.port = t.mid(e + 1).toInt();
        }
        QHostAddress h;
        if (!h.setAddress(ip)) return k;
        k.rank = 1;
        k.v6 = h.protocol() == QAbstractSocket::IPv6Protocol;
        if (k.v6) {
            const Q_IPV6ADDR a = h.toIPv6Address();
            k.bytes = QByteArray(reinterpret_cast<const char *>(a.c), 16);
        } else {
            const quint32 a = h.toIPv4Address();
            for (int i = 3; i >= 0; --i) k.bytes.append(char((a >> (8 * i)) & 0xff));
        }
        return k;
    }
};

static QTreeWidget *makeTree(const QStringList &cols) {
    auto *t = new QTreeWidget;
    t->setColumnCount(cols.size());
    t->setHeaderLabels(cols);
    t->setRootIsDecorated(false);
    t->setAlternatingRowColors(true);
    t->setUniformRowHeights(true);
    t->header()->setStretchLastSection(true);
    t->setContextMenuPolicy(Qt::CustomContextMenu);
    return t;
}

NetPage::NetPage(QWidget *parent) : QWidget(parent) {
    auto *v = new QVBoxLayout(this);
    v->setContentsMargins(8, 6, 8, 6);
    v->setSpacing(6);

    auto *top = new QHBoxLayout;
    status_ = new QLabel(QStringLiteral("…"));
    status_->setTextFormat(Qt::RichText);
    status_->setWordWrap(true);
    status_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    top->addWidget(status_, 1);
    guard_ = new QCheckBox("Block non-whitelisted Wine / .exe programs");
    guard_->setToolTip("A Windows program (Wine, Proton, any runner) that is not on the whitelist and opens an\n"
                       "internet connection is cut off at its first packet and written to the Blocked log.");
    lan_ = new QCheckBox("LAN exempt");
    lan_->setToolTip("Private, link-local and multicast addresses (router DNS, LAN play) are not guarded.");
    logConns_ = new QCheckBox("Log connections");
    logConns_->setToolTip("While on, the daemon writes every new connection this machine opens and every new connection\n"
                          "made to it (TCP/UDP, loopback excluded) to the Log tab, with the program behind it.\n"
                          "The same program ↔ address ↔ port is written once a minute.");
    top->addWidget(guard_);
    top->addWidget(lan_);
    top->addWidget(logConns_);
    v->addLayout(top);

    tabs_ = new QTabWidget;
    v->addWidget(tabs_, 1);
    auto button = [](const char *text, QBoxLayout *l, bool danger = false) {
        auto *b = new QPushButton(QString::fromUtf8(text));
        if (danger) b->setObjectName(QStringLiteral("btnDanger"));
        l->addWidget(b);
        return b;
    };

    // ── Connections ──
    auto *pc = new QWidget;
    auto *vc = new QVBoxLayout(pc);
    vc->setContentsMargins(6, 6, 6, 6);
    auto *rc = new QHBoxLayout;
    filter_ = new QLineEdit;
    filter_->setPlaceholderText(QStringLiteral("Filter (program, address)…"));
    filter_->setClearButtonEnabled(true);
    filter_->setMaximumWidth(240);
    rc->addWidget(filter_);
    listening_ = new QCheckBox("Listening sockets");
    rc->addWidget(listening_);
    connInfo_ = new QLabel;
    connInfo_->setProperty("role", "muted");
    rc->addWidget(connInfo_, 1);
    auto *bAll = button("All users (root)", rc);
    bAll->setToolTip("One snapshot through pkexec: also names the processes of other users (system daemons).");
    auto *bInfoC = button("IP info", rc);
    bInfoC->setToolTip("Reverse DNS and whois record (owner, network, country) of the remote address.");
    auto *bDetC = button("Details", rc);
    bDetC->setToolTip("Process (parent chain, command line, service), the address's history in the connection log, and a link to IP info.");
    auto *bWlC = button("Whitelist program", rc);
    auto *bKill = button("Kill connection", rc);
    auto *bBlC = button("Blacklist IP", rc, true);
    vc->addLayout(rc);
    conns_ = makeTree({"Program", "PID", "Proto", "Local", "Remote", "State"});
    conns_->setColumnWidth(0, 220);
    conns_->setColumnWidth(3, 190);
    conns_->setColumnWidth(4, 230);
    conns_->sortByColumn(0, Qt::AscendingOrder);
    vc->addWidget(conns_, 1);
    tabs_->addTab(pc, "Connections");

    // ── Blocked ──
    auto *pb = new QWidget;
    auto *vb = new QVBoxLayout(pb);
    vb->setContentsMargins(6, 6, 6, 6);
    auto *rb = new QHBoxLayout;
    auto *about = new QLabel("Connections the guard cut off. Whitelist the program to let it online, or blacklist the address for everything.");
    about->setProperty("role", "muted");
    about->setWordWrap(true);
    rb->addWidget(about, 1);
    auto *bWlB = button("Whitelist program", rb);
    auto *bWlDir = button("Whitelist folder", rb);
    auto *bBlB = button("Blacklist IP", rb, true);
    auto *bClear = button("Clear log", rb, true);
    vb->addLayout(rb);
    blocked_ = makeTree({"Last seen", "Program", "Destination", "DNS name", "Hits"});
    blocked_->setColumnWidth(0, 150);
    blocked_->setColumnWidth(1, 220);
    blocked_->setColumnWidth(2, 210);
    blocked_->setColumnWidth(3, 240);
    vb->addWidget(blocked_, 1);
    tabs_->addTab(pb, "Blocked");

    // ── Log ──
    auto *pl = new QWidget;
    auto *vl = new QVBoxLayout(pl);
    vl->setContentsMargins(6, 6, 6, 6);
    auto *rlog = new QHBoxLayout;
    logFilter_ = new QLineEdit;
    logFilter_->setPlaceholderText(QStringLiteral("Filter (program, address, port)…"));
    logFilter_->setClearButtonEnabled(true);
    logFilter_->setMaximumWidth(240);
    rlog->addWidget(logFilter_);
    logInfo_ = new QLabel;
    logInfo_->setProperty("role", "muted");
    rlog->addWidget(logInfo_, 1);
    auto *bDetL = button("Details", rlog);
    bDetL->setToolTip(bDetC->toolTip());
    auto *bInfoL = button("IP info", rlog);
    bInfoL->setToolTip(bInfoC->toolTip());
    auto *bBlL = button("Blacklist IP", rlog, true);
    auto *bClearL = button("Clear log", rlog, true);
    vl->addLayout(rlog);
    connLog_ = makeTree({"Time", "Dir", "Program", "Proto", "Remote address", "Port", "Note"});
    connLog_->setColumnWidth(0, 130);
    connLog_->setColumnWidth(1, 50);
    connLog_->setColumnWidth(2, 220);
    connLog_->setColumnWidth(3, 55);
    connLog_->setColumnWidth(4, 250);
    connLog_->setColumnWidth(5, 60);
    connLog_->setSortingEnabled(true);  // click a header to sort; newest first until then
    connLog_->sortByColumn(0, Qt::DescendingOrder);
    vl->addWidget(connLog_, 1);
    tabs_->addTab(pl, "Log");

    // ── Rules ──
    auto *pr = new QWidget;
    auto *hr = new QHBoxLayout(pr);
    hr->setContentsMargins(6, 6, 6, 6);
    wl_ = new QListWidget;
    wl_->setSelectionMode(QAbstractItemView::ExtendedSelection);
    bl_ = makeTree({QString()});
    bl_->setHeaderHidden(true);
    bl_->setRootIsDecorated(true);
    bl_->setSelectionMode(QAbstractItemView::ExtendedSelection);
    auto column = [&](const char *title, const char *hint, QWidget *list, QLineEdit *&edit) {
        auto *col = new QVBoxLayout;
        col->addWidget(new QLabel(QStringLiteral("<b>%1</b>").arg(QString::fromUtf8(title))));
        col->addWidget(list, 1);
        auto *row = new QHBoxLayout;
        edit = new QLineEdit;
        edit->setPlaceholderText(QString::fromUtf8(hint));
        row->addWidget(edit, 1);
        col->addLayout(row);
        hr->addLayout(col, 1);
        return row;
    };
    auto *rw = column("Whitelist — Windows programs allowed online", "/path/game.exe  ·  /games/folder/  ·  game.exe", wl_, wlEdit_);
    auto *bWlAdd = button("Add", rw);
    auto *bWlBrowse = button("Browse…", rw);
    auto *bWlDel = button("Remove", rw, true);
    auto *rl = column("Blacklist — addresses blocked for every program", "203.0.113.7  ·  198.51.100.0/24  ·  192.0.2.10 - 192.0.2.80", bl_, blEdit_);
    auto *bBlAdd = button("Add", rl);
    auto *bBlMany = button("Add many…", rl);
    bBlMany->setToolTip("Paste or load a list of addresses, CIDR blocks and ranges; they are banned as one group\n"
                        "that can be removed again with one click.");
    auto *bBlDel = button("Remove", rl, true);
    tabs_->addTab(pr, "Rules");

    // ── wiring ──
    connect(guard_, &QCheckBox::toggled, this, [this](bool on) { runRoot({{"op", "set"}, {"guard", on}}); });
    connect(lan_, &QCheckBox::toggled, this, [this](bool on) { runRoot({{"op", "set"}, {"allow_lan", on}}); });
    connect(logConns_, &QCheckBox::toggled, this, [this](bool on) { runRoot({{"op", "set"}, {"log_conns", on}}); });
    connect(logFilter_, &QLineEdit::textChanged, this, &NetPage::filterConnLog);
    auto logIp = [this] { auto *it = connLog_->currentItem(); return it ? it->data(0, IpRole).toString() : QString(); };
    auto detConn = [this](const QJsonObject &c) {
        connDetails(str(c, "remote"), c.value(QLatin1String("pid")).toInt(), str(c, "exe"), c.value(QLatin1String("uid")).toInteger(-1), str(c, "state"));
    };
    auto detLog = [this](QTreeWidgetItem *it) {
        if (it) connDetails(it->data(0, IpRole).toString(), it->data(0, PidRole).toInt(), it->data(0, ExeRole).toString(), it->data(0, UidRole).toLongLong(), QString());
    };
    connect(bDetC, &QPushButton::clicked, this, [this, detConn] { detConn(curConn()); });
    connect(bDetL, &QPushButton::clicked, this, [this, detLog] { detLog(connLog_->currentItem()); });
    connect(conns_, &QTreeWidget::itemDoubleClicked, this, [this, detConn] { detConn(curConn()); });
    connect(bInfoC, &QPushButton::clicked, this, [this] { ipInfo(str(curConn(), "remote")); });
    connect(bInfoL, &QPushButton::clicked, this, [this, logIp] { ipInfo(logIp()); });
    connect(connLog_, &QTreeWidget::itemDoubleClicked, this, [detLog](QTreeWidgetItem *it) { detLog(it); });
    connect(bBlL, &QPushButton::clicked, this, [this, logIp] { changeList("blacklist", "add", logIp()); });
    connect(bClearL, &QPushButton::clicked, this, [this] {
        if (QMessageBox::question(this, "Clear log", "Delete the connection log?") == QMessageBox::Yes) runRoot({{"op", "clear_conn_log"}}, false);
    });
    connect(filter_, &QLineEdit::textChanged, this, &NetPage::showConns);
    connect(listening_, &QCheckBox::toggled, this, &NetPage::showConns);
    connect(tabs_, &QTabWidget::currentChanged, this, &NetPage::poll);
    connect(bAll, &QPushButton::clicked, this, [this] {
        privileged::run(helper(), QJsonObject{{"op", "list"}}, this, [this](const privileged::Result &r) {
            if (!r.ok()) { QMessageBox::warning(this, "Network", r.message()); return; }
            viaRoot_ = true;  // kept until the page is hidden; the 3 s refresh would drop the names again
            connData_ = r.json.value(QLatin1String("conns")).toArray();
            showConns();
        });
    });
    auto wlConn = [this](const QJsonObject &c) {
        if (!c.value(QLatin1String("wine")).toBool()) { QMessageBox::information(this, "Network", "Select a connection of a Wine / .exe program (shown in purple)."); return; }
        changeList("whitelist", "add", str(c, "exe"));
    };
    auto killConn = [this](const QJsonObject &c) {
        if (str(c, "remote").isEmpty()) return;
        runRoot({{"op", "kill"}, {"proto", c.value(QLatin1String("proto"))}, {"lport", c.value(QLatin1String("lport"))},
                 {"remote", c.value(QLatin1String("remote"))}, {"rport", c.value(QLatin1String("rport"))}}, false);
    };
    connect(bWlC, &QPushButton::clicked, this, [this, wlConn] { wlConn(curConn()); });
    connect(bBlC, &QPushButton::clicked, this, [this] { changeList("blacklist", "add", str(curConn(), "remote")); });
    connect(bKill, &QPushButton::clicked, this, [this, killConn] { killConn(curConn()); });

    // Right-click menus. Everything an action needs is copied when the menu opens: the lists refresh under it.
    auto menuFor = [this](QTreeWidget *t, std::function<void(QMenu &, QTreeWidgetItem *)> build) {
        connect(t, &QWidget::customContextMenuRequested, this, [t, build](const QPoint &pos) {
            QTreeWidgetItem *it = t->itemAt(pos);
            if (!it) return;
            QMenu m(t);
            build(m, it);
            if (!m.isEmpty()) m.exec(t->viewport()->mapToGlobal(pos));
        });
    };
    menuFor(conns_, [this, wlConn, killConn, detConn](QMenu &m, QTreeWidgetItem *it) {
        const QJsonObject c = connData_.at(it->data(0, IdxRole).toInt()).toObject();
        const bool wine = c.value(QLatin1String("wine")).toBool();
        if (!str(c, "remote").isEmpty()) {
            m.addAction(QStringLiteral("Details…"), this, [detConn, c] { detConn(c); });
            addIpActions(m, str(c, "remote"));
            m.addSeparator();
            m.addAction(QStringLiteral("Kill connection"), this, [killConn, c] { killConn(c); });
        }
        if (wine) m.addAction(QStringLiteral("Whitelist program"), this, [wlConn, c] { wlConn(c); });
    });
    menuFor(blocked_, [this](QMenu &m, QTreeWidgetItem *it) {
        const QString exe = it->data(0, ExeRole).toString();
        addIpActions(m, it->data(0, IpRole).toString());
        m.addSeparator();
        m.addAction(QStringLiteral("Whitelist program"), this, [this, exe] { changeList("whitelist", "add", exe); });
        if (exe.startsWith(QLatin1Char('/')))
            m.addAction(QStringLiteral("Whitelist folder"), this, [this, exe] { changeList("whitelist", "add", exe.left(exe.lastIndexOf(QLatin1Char('/')) + 1)); });
    });
    menuFor(connLog_, [this, detLog](QMenu &m, QTreeWidgetItem *it) {
        m.addAction(QStringLiteral("Details…"), this, [detLog, it] { detLog(it); });
        addIpActions(m, it->data(0, IpRole).toString());
    });
    menuFor(bl_, [this](QMenu &m, QTreeWidgetItem *it) {
        const QString entry = it->data(0, IpRole).toString();
        m.addAction(it->childCount() ? QStringLiteral("Remove group") : bl_->selectedItems().size() > 1 ? QStringLiteral("Remove selected") : QStringLiteral("Remove"),
                    this, [this] { removeBlacklisted(false); });
        if (!entry.isEmpty()) {
            if (!entry.contains(QLatin1Char('/'))) m.addAction(QStringLiteral("IP info"), this, [this, entry] { ipInfo(entry); });
            m.addAction(QStringLiteral("Copy"), this, [entry] { QGuiApplication::clipboard()->setText(entry); });
        }
        m.addSeparator();
        m.addAction(QStringLiteral("Remove all…"), this, [this] { removeBlacklisted(true); });
    });
    auto blockedData = [this](int role) { auto *it = blocked_->currentItem(); return it ? it->data(0, role).toString() : QString(); };
    connect(bWlB, &QPushButton::clicked, this, [this, blockedData] { changeList("whitelist", "add", blockedData(ExeRole)); });
    connect(bWlDir, &QPushButton::clicked, this, [this, blockedData] {
        const QString exe = blockedData(ExeRole);
        if (exe.startsWith(QLatin1Char('/'))) changeList("whitelist", "add", exe.left(exe.lastIndexOf(QLatin1Char('/')) + 1));
    });
    connect(bBlB, &QPushButton::clicked, this, [this, blockedData] { changeList("blacklist", "add", blockedData(IpRole)); });
    connect(bClear, &QPushButton::clicked, this, [this] {
        if (QMessageBox::question(this, "Clear log", "Delete the log of blocked connections?") == QMessageBox::Yes) runRoot({{"op", "clear_log"}}, false);
    });
    auto addFrom = [this](const char *op, QLineEdit *e) { changeList(op, "add", e->text().trimmed()); e->clear(); };
    connect(bWlAdd, &QPushButton::clicked, this, [=, this] { addFrom("whitelist", wlEdit_); });
    connect(wlEdit_, &QLineEdit::returnPressed, this, [=, this] { addFrom("whitelist", wlEdit_); });
    connect(bBlAdd, &QPushButton::clicked, this, [=, this] { addFrom("blacklist", blEdit_); });
    connect(blEdit_, &QLineEdit::returnPressed, this, [=, this] { addFrom("blacklist", blEdit_); });
    connect(bWlBrowse, &QPushButton::clicked, this, [this] {
        const QString f = QFileDialog::getOpenFileName(this, "Windows program", QDir::homePath(), "Windows programs (*.exe *.EXE);;All files (*)");
        if (!f.isEmpty()) changeList("whitelist", "add", f);
    });
    connect(bWlDel, &QPushButton::clicked, this, [this] {
        QJsonArray a;
        for (const QListWidgetItem *it : wl_->selectedItems()) a.append(it->text());
        if (!a.isEmpty()) runRoot({{"op", "whitelist"}, {"remove", a}});
    });
    connect(bBlDel, &QPushButton::clicked, this, [this] { removeBlacklisted(false); });
    connect(bBlMany, &QPushButton::clicked, this, &NetPage::bulkBlacklist);

    timer_ = new QTimer(this);
    timer_->setInterval(POLL_MS);
    connect(timer_, &QTimer::timeout, this, [this] { if (tabs_->currentIndex() == LOG_TAB) readConnLog(); else refreshConns(); });

    // Block log: inotify, also while the window sits in the tray (tray alerts).
    watch_ = new QFileSystemWatcher(this);
    auto *settle = new QTimer(this);
    settle->setSingleShot(true);
    settle->setInterval(400);
    connect(settle, &QTimer::timeout, this, [this] { watchLog(); readLog(true); });
    connect(watch_, &QFileSystemWatcher::fileChanged, settle, qOverload<>(&QTimer::start));
    connect(watch_, &QFileSystemWatcher::directoryChanged, settle, qOverload<>(&QTimer::start));
    watchLog();
    readLog(false);
}

void NetPage::watchLog() {
    for (const char *p : {LOG_DIR, LOG_FILE})
        if (QFileInfo::exists(QLatin1String(p)) && !watch_->files().contains(QLatin1String(p)) && !watch_->directories().contains(QLatin1String(p)))
            watch_->addPath(QLatin1String(p));
}

void NetPage::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    runUser({{"op", "status"}}, [this](const QJsonObject &s) { applyStatus(s); });
    watchLog();
    readLog(false);
    poll();
}

void NetPage::hideEvent(QHideEvent *e) {
    QWidget::hideEvent(e);
    timer_->stop();
    viaRoot_ = false;
}

/// The connection list and the connection log refresh only while they are what the user is looking at.
void NetPage::poll() {
    if (isVisible() && tabs_->currentIndex() == 0) {
        if (!viaRoot_) refreshConns();
        timer_->start();
    } else if (isVisible() && tabs_->currentIndex() == LOG_TAB) {
        readConnLog();
        timer_->start();
    } else {
        timer_->stop();
    }
}

void NetPage::runUser(const QJsonObject &req, std::function<void(const QJsonObject &)> cb, int timeoutMs) {
    if (!QFileInfo(helper()).isExecutable()) { status_->setText(QStringLiteral("netguard-helper is not installed.")); return; }
    auto *p = new QProcess(this);
    QPointer<QProcess> guard(p);
    connect(p, &QProcess::finished, this, [p, cb](int, QProcess::ExitStatus) {
        const QByteArray out = p->readAllStandardOutput().trimmed();
        p->deleteLater();
        cb(QJsonDocument::fromJson(out.mid(out.lastIndexOf('\n') + 1)).object());
    });
    connect(p, &QProcess::errorOccurred, this, [p, cb](QProcess::ProcessError e) {
        if (e != QProcess::FailedToStart) return;
        p->deleteLater();
        cb({});
    });
    QTimer::singleShot(timeoutMs, p, [guard] { if (guard && guard->state() != QProcess::NotRunning) guard->kill(); });
    p->start(helper(), {});
    p->write(QJsonDocument(req).toJson(QJsonDocument::Compact));
    p->closeWriteChannel();
}

void NetPage::runRoot(const QJsonObject &req, bool isStatus) {
    privileged::run(helper(), req, this, [this, isStatus](const privileged::Result &r) {
        if (!r.ok()) {
            QMessageBox::warning(this, "Network", r.message());
            const QSignalBlocker b1(guard_), b2(lan_), b3(logConns_);  // put the check boxes back
            guard_->setChecked(state_.value(QLatin1String("guard")).toBool());
            logConns_->setChecked(state_.value(QLatin1String("log_conns")).toBool());
            lan_->setChecked(state_.value(QLatin1String("allow_lan")).toBool(true));
            return;
        }
        if (isStatus) applyStatus(r.json);
        else readLog(false);
        viaRoot_ = false;
        if (isVisible() && tabs_->currentIndex() == LOG_TAB) readConnLog();
        else if (isVisible()) refreshConns();
    });
}

void NetPage::changeList(const char *op, const char *verb, const QString &entry) {
    if (entry.isEmpty()) return;
    runRoot({{"op", op}, {verb, QJsonArray{entry}}});
}

void NetPage::applyStatus(const QJsonObject &s) {
    if (!s.value(QLatin1String("ok")).toBool()) {
        if (!s.isEmpty()) status_->setText(QStringLiteral("<span style='color:%1'>%2</span>").arg(QLatin1String(theme::DANGER), str(s, "error").toHtmlEscaped()));
        return;
    }
    // Rule changes answer without the kernel checks: keep the ones from the last full status.
    for (const char *k : {"missing", "nft"})
        if (!s.contains(QLatin1String(k)) && state_.contains(QLatin1String(k))) {
            QJsonObject m = s;
            m.insert(QLatin1String(k), state_.value(QLatin1String(k)));
            return applyStatus(m);
        }
    state_ = s;
    const bool running = s.value(QLatin1String("running")).toBool(), guard = s.value(QLatin1String("guard")).toBool();
    const bool logging = s.value(QLatin1String("log_conns")).toBool();
    { const QSignalBlocker b1(guard_), b2(lan_), b3(logConns_);
      guard_->setChecked(guard);
      logConns_->setChecked(logging);
      lan_->setChecked(s.value(QLatin1String("allow_lan")).toBool(true));
      lan_->setEnabled(guard); }
    wl_->clear();
    for (const QJsonValue &x : s.value(QLatin1String("whitelist")).toArray()) wl_->addItem(x.toString());
    // Blacklist: entries banned together (same label) sit under one group row; the rest are single rows.
    QSet<QString> open;
    for (int i = 0; i < bl_->topLevelItemCount(); ++i)
        if (bl_->topLevelItem(i)->isExpanded()) open.insert(bl_->topLevelItem(i)->data(0, KeyRole).toString());
    bl_->clear();
    blacklist_.clear();
    const QJsonObject labels = s.value(QLatin1String("labels")).toObject();
    QHash<QString, QTreeWidgetItem *> groups;
    for (const QJsonValue &x : s.value(QLatin1String("blacklist")).toArray()) {
        const QString e = x.toString(), label = labels.value(e).toString();
        blacklist_ << e;
        auto *leaf = new QTreeWidgetItem({e});
        leaf->setData(0, IpRole, e);
        if (label.isEmpty()) { bl_->addTopLevelItem(leaf); continue; }
        QTreeWidgetItem *&g = groups[label];
        if (!g) {
            g = new QTreeWidgetItem(bl_);
            g->setData(0, KeyRole, label);
            QFont f = g->font(0);
            f.setBold(true);
            g->setFont(0, f);
        }
        g->addChild(leaf);
    }
    for (auto it = groups.cbegin(); it != groups.cend(); ++it) {
        it.value()->setText(0, QStringLiteral("%1  (%2)").arg(it.key()).arg(it.value()->childCount()));
        it.value()->setExpanded(open.contains(it.key()));
    }

    QStringList lines;
    if (running) {
        lines << QStringLiteral("<span style='color:%1'>●</span> Daemon running — blacklist enforced (%2), Wine guard <b>%3</b> (%4 whitelisted), connection log <b>%5</b>")
                     .arg(QLatin1String(theme::OK)).arg(blacklist_.size()).arg(QLatin1String(guard ? "on" : "off")).arg(wl_->count())
                     .arg(QLatin1String(logging ? "on" : "off"));
    } else {
        const bool systemd = QFileInfo::exists(QStringLiteral("/run/systemd/system"));
        lines << QStringLiteral("<span style='color:%1'>○ Daemon not running — rules are saved, nothing is enforced.</span> Start it: <code>%2</code>")
                     .arg(QLatin1String(theme::WARN), systemd ? QStringLiteral("systemctl enable --now lpm-netguard")
                                                              : QStringLiteral("rc-service lpm-netguard start &amp;&amp; rc-update add lpm-netguard default"));
    }
    // What the daemon reports as loaded differs from what is saved: it failed to load the rules, or it is a build
    // from before this setting existed (installed files are new, the running process is not).
    const QJsonObject applied = s.value(QLatin1String("applied")).toObject();
    if (running && !str(applied, "error").isEmpty())
        lines << QStringLiteral("<span style='color:%1'>The daemon could not load the rules: %2</span>").arg(QLatin1String(theme::DANGER), str(applied, "error").toHtmlEscaped());
    else if (running && (applied.value(QLatin1String("guard")).toBool() != guard || applied.value(QLatin1String("log_conns")).toBool() != logging))
        lines << QStringLiteral("<span style='color:%1'>The running daemon has not applied these settings — restart it: <code>%2</code></span>")
                     .arg(QLatin1String(theme::WARN), QFileInfo::exists(QStringLiteral("/run/systemd/system")) ? QStringLiteral("systemctl restart lpm-netguard")
                                                                                                             : QStringLiteral("rc-service lpm-netguard restart"));
    QStringList missing;
    for (const QJsonValue &x : s.value(QLatin1String("missing")).toArray()) missing << QStringLiteral("CONFIG_") + x.toString();
    if (!missing.isEmpty())
        lines << QStringLiteral("<span style='color:%1'>Kernel lacks: %2</span>").arg(QLatin1String(theme::DANGER), missing.join(QStringLiteral(", ")));
    if (s.contains(QLatin1String("nft")) && !s.value(QLatin1String("nft")).toBool())
        lines << QStringLiteral("<span style='color:%1'>nft not found — emerge net-firewall/nftables</span>").arg(QLatin1String(theme::DANGER));
    if (s.contains(QLatin1String("config_error")))
        lines << QStringLiteral("<span style='color:%1'>%2</span>").arg(QLatin1String(theme::DANGER), str(s, "config_error").toHtmlEscaped());
    status_->setText(lines.join(QStringLiteral("<br>")));
    showConns();  // blacklist colouring
    filterConnLog();  // "logging on/off" in the Log tab
}

void NetPage::refreshConns() {
    if (busy_) return;
    busy_ = true;
    viaRoot_ = false;
    runUser({{"op", "list"}}, [this](const QJsonObject &r) {
        busy_ = false;
        if (!r.value(QLatin1String("ok")).toBool()) { connInfo_->setText(r.isEmpty() ? QStringLiteral("helper failed") : str(r, "error")); return; }
        connData_ = r.value(QLatin1String("conns")).toArray();
        showConns();
    });
}

QJsonObject NetPage::curConn() const {
    const QTreeWidgetItem *it = conns_->currentItem();
    return it ? connData_.at(it->data(0, IdxRole).toInt()).toObject() : QJsonObject();
}

void NetPage::showConns() {
    const QString needle = filter_->text().trimmed();
    const QString keep = conns_->currentItem() ? conns_->currentItem()->data(0, KeyRole).toString() : QString();
    const int scroll = conns_->verticalScrollBar()->value();
    const QStringList &black = blacklist_;
    conns_->setUpdatesEnabled(false);
    conns_->setSortingEnabled(false);
    conns_->clear();
    QTreeWidgetItem *sel = nullptr;
    int shown = 0, wine = 0;
    for (int i = 0; i < connData_.size(); ++i) {
        const QJsonObject c = connData_.at(i).toObject();
        const QString remote = str(c, "remote"), exe = str(c, "exe"), state = str(c, "state");
        if (remote.isEmpty() && !listening_->isChecked()) continue;
        const int pid = c.value(QLatin1String("pid")).toInt();
        QString name = str(c, "name");
        if (name.isEmpty()) name = state == QLatin1String("TIME-WAIT") ? QStringLiteral("—")
                                 : QLatin1Char('(') + userName(uint(c.value(QLatin1String("uid")).toInteger())) + QLatin1Char(')');
        if (!needle.isEmpty() && !name.contains(needle, Qt::CaseInsensitive) && !exe.contains(needle, Qt::CaseInsensitive) && !remote.contains(needle))
            continue;
        const QString local = endpoint(str(c, "local"), c.value(QLatin1String("lport")).toInt());
        const QString peer = remote.isEmpty() ? QStringLiteral("*") : endpoint(remote, c.value(QLatin1String("rport")).toInt());
        auto *it = new SortItem({name, pid ? QString::number(pid) : QString(), str(c, "proto").toUpper(), local, peer, state});
        it->setData(0, IdxRole, i);
        it->setData(0, KeyRole, QString(str(c, "proto") + local + peer));
        it->setToolTip(0, exe);
        const bool isWine = c.value(QLatin1String("wine")).toBool();
        const bool bl = c.value(QLatin1String("bl")).toBool() || black.contains(remote);
        if (bl || isWine) {
            const QColor col(QLatin1String(bl ? theme::DANGER : theme::PURPLE));
            for (int k = 0; k < 6; ++k) it->setForeground(k, col);
        }
        conns_->addTopLevelItem(it);
        if (it->data(0, KeyRole).toString() == keep) sel = it;
        ++shown;
        wine += isWine;
    }
    conns_->setSortingEnabled(true);
    if (sel) conns_->setCurrentItem(sel);
    conns_->verticalScrollBar()->setValue(scroll);
    conns_->setUpdatesEnabled(true);
    connInfo_->setText(QStringLiteral("%1 shown · %2 Wine%3").arg(shown).arg(wine)
                           .arg(viaRoot_ ? QStringLiteral(" · root snapshot (not refreshing)") : QString()));
}

void NetPage::readLog(bool announce) {
    QFile f{QLatin1String(LOG_FILE)};
    if (!f.open(QIODevice::ReadOnly) || f.size() < logPos_) {  // cleared or rotated: start over
        blocked_->clear();
        rows_.clear();
        logPos_ = 0;
        if (!f.isOpen()) { tabs_->setTabText(1, QStringLiteral("Blocked")); return; }
    }
    f.seek(logPos_);
    int fresh = 0;
    QString first;
    while (!f.atEnd()) {
        const QByteArray line = f.readLine();
        if (!line.endsWith('\n')) break;  // half-written line: next time
        logPos_ += line.size();
        const QJsonObject o = QJsonDocument::fromJson(line).object();
        const QString exe = str(o, "exe"), dst = str(o, "dst");
        if (exe.isEmpty() || dst.isEmpty()) continue;
        const QString dest = str(o, "proto").toUpper() + QLatin1Char(' ') + endpoint(dst, o.value(QLatin1String("dport")).toInt());
        const QString key = exe + QLatin1Char('|') + dest;
        const QString when = QDateTime::fromSecsSinceEpoch(o.value(QLatin1String("ts")).toInteger()).toString(QStringLiteral("yyyy-MM-dd HH:mm:ss"));
        QTreeWidgetItem *it = rows_.value(key);
        if (!it) {
            it = new QTreeWidgetItem({when, baseName(exe), dest, str(o, "host"), QStringLiteral("1")});
            it->setData(0, ExeRole, exe);
            it->setData(0, IpRole, dst);
            it->setToolTip(1, exe);
            rows_.insert(key, it);
        } else {
            blocked_->takeTopLevelItem(blocked_->indexOfTopLevelItem(it));
            it->setText(0, when);
            it->setText(4, QString::number(it->text(4).toInt() + 1));
        }
        blocked_->insertTopLevelItem(0, it);  // newest first
        if (!fresh++) first = baseName(exe) + QStringLiteral(" → ") + dest;
    }
    while (blocked_->topLevelItemCount() > MAX_BLOCK_ROWS) {
        QTreeWidgetItem *old = blocked_->takeTopLevelItem(blocked_->topLevelItemCount() - 1);
        rows_.remove(rows_.key(old));
        delete old;
    }
    tabs_->setTabText(1, rows_.isEmpty() ? QStringLiteral("Blocked") : QStringLiteral("Blocked (%1)").arg(rows_.size()));
    if (announce && fresh && (!lastAlert_.isValid() || lastAlert_.elapsed() > ALERT_GAP_MS)) {
        lastAlert_.start();
        Q_EMIT alert(QStringLiteral("Network guard"),
                     QStringLiteral("Blocked ") + first + (fresh > 1 ? QStringLiteral(" (+%1 more)").arg(fresh - 1) : QString()));
    }
}

/// Appends the lines the daemon wrote since the last look (newest on top).
void NetPage::readConnLog() {
    QFile f{QLatin1String(CONN_LOG)};
    if (!f.open(QIODevice::ReadOnly) || f.size() < connPos_) {  // cleared or rotated: start over
        connLog_->clear();
        connPos_ = 0;
        if (!f.isOpen()) { filterConnLog(); return; }
    }
    if (f.size() == connPos_) { filterConnLog(); return; }
    if (connPos_ == 0 && f.size() > LOG_TAIL) {
        f.seek(f.size() - LOG_TAIL);
        f.readLine();  // the line the seek landed in
        connPos_ = f.pos();
    }
    f.seek(connPos_);
    QList<QTreeWidgetItem *> fresh;
    while (!f.atEnd()) {
        const QByteArray line = f.readLine();
        if (!line.endsWith('\n')) break;  // half-written line: next time
        connPos_ += line.size();
        const QJsonObject o = QJsonDocument::fromJson(line).object();
        const QString remote = str(o, "remote"), exe = str(o, "exe");
        if (remote.isEmpty()) continue;
        const bool in = str(o, "dir") == QLatin1String("in"), blocked = o.value(QLatin1String("blocked")).toBool();
        const bool closed = in && !o.value(QLatin1String("open")).toBool(true);
        const int pid = o.value(QLatin1String("pid")).toInt();
        QString name = baseName(exe);
        if (name.isEmpty()) name = o.value(QLatin1String("uid")).isDouble() ? QLatin1Char('(') + userName(uint(o.value(QLatin1String("uid")).toInteger())) + QLatin1Char(')')
                                                                             : QStringLiteral("—");
        QString note = str(o, "host");
        if (blocked) note = QStringLiteral("blocked by the Wine guard");
        else if (o.value(QLatin1String("existing")).toBool()) note = QStringLiteral("already open when logging started");
        else if (closed) note = QStringLiteral("no listener on this port");
        else if (!note.isEmpty()) note = QStringLiteral("DNS query: ") + note;
        static quint64 seq = 0;  // keeps lines of the same second in arrival order
        auto *it = new SortItem({QDateTime::fromSecsSinceEpoch(o.value(QLatin1String("ts")).toInteger()).toString(QStringLiteral("MM-dd HH:mm:ss")),
                                        in ? QStringLiteral("← in") : QStringLiteral("→ out"), name, str(o, "proto").toUpper(), remote,
                                        QString::number(o.value(QLatin1String(in ? "lport" : "rport")).toInt()), note});
        it->setData(0, IpRole, remote);
        it->setData(0, PidRole, pid);
        it->setData(0, ExeRole, exe);
        it->setData(0, UidRole, o.value(QLatin1String("uid")).toInteger(-1));
        it->setData(0, SortRole, double(o.value(QLatin1String("ts")).toInteger()) * 1e6 + double(seq++ % 1000000));
        it->setToolTip(2, pid ? QStringLiteral("%1 (pid %2)").arg(exe).arg(pid) : exe);
        it->setToolTip(5, in ? QStringLiteral("local port; the peer used port %1").arg(o.value(QLatin1String("rport")).toInt())
                             : QStringLiteral("remote port; local port %1").arg(o.value(QLatin1String("lport")).toInt()));
        if (blocked || in) {
            const QColor col(QLatin1String(blocked ? theme::DANGER : closed ? theme::WARN : theme::OK));
            for (int k = 0; k < 7; ++k) it->setForeground(k, col);
        }
        fresh.prepend(it);
    }
    while (fresh.size() > MAX_LOG_ROWS) delete fresh.takeLast();
    if (!fresh.isEmpty()) {
        QHeaderView *h = connLog_->header();
        const int sc = h->sortIndicatorSection();
        const Qt::SortOrder so = h->sortIndicatorOrder();
        connLog_->setUpdatesEnabled(false);
        connLog_->setSortingEnabled(false);
        connLog_->insertTopLevelItems(0, fresh);
        if (connLog_->topLevelItemCount() > MAX_LOG_ROWS) {  // drop the oldest, whatever the current order is
            connLog_->sortItems(0, Qt::DescendingOrder);
            while (connLog_->topLevelItemCount() > MAX_LOG_ROWS) delete connLog_->takeTopLevelItem(connLog_->topLevelItemCount() - 1);
            h->setSortIndicator(sc, so);
        }
        connLog_->setSortingEnabled(true);  // re-sorts by the clicked column
        connLog_->setUpdatesEnabled(true);
    }
    filterConnLog();
}

void NetPage::filterConnLog() {
    const QString needle = logFilter_->text().trimmed();
    int shown = 0, inbound = 0;
    for (int i = 0; i < connLog_->topLevelItemCount(); ++i) {
        QTreeWidgetItem *it = connLog_->topLevelItem(i);
        bool hit = needle.isEmpty();
        for (int k = 1; k < 7 && !hit; ++k) hit = it->text(k).contains(needle, Qt::CaseInsensitive);
        it->setHidden(!hit);
        shown += hit;
        inbound += hit && it->text(1).startsWith(QChar(0x2190));
    }
    const bool on = state_.value(QLatin1String("log_conns")).toBool(), running = state_.value(QLatin1String("running")).toBool();
    const bool active = state_.value(QLatin1String("applied")).toObject().value(QLatin1String("log_conns")).toBool();
    logInfo_->setText(QStringLiteral("%1 shown · %2 inbound · logging %3").arg(shown).arg(inbound)
                          .arg(!on ? QStringLiteral("off") : !running ? QStringLiteral("on, but the daemon is not running")
                               : active ? QStringLiteral("on") : QStringLiteral("on, but not active in the running daemon — restart it")));
}

// ── Details: process (from /proc), the address's history in the connection log, link to IP info ──
static QByteArray procFile(int pid, const char *name) {
    QFile f(QStringLiteral("/proc/%1/%2").arg(pid).arg(QLatin1String(name)));
    return f.open(QIODevice::ReadOnly) ? f.readAll() : QByteArray();
}
static QString procCmd(int pid) {
    QByteArray b = procFile(pid, "cmdline");
    b.replace('\0', ' ');
    return QString::fromLocal8Bit(b).trimmed();
}
struct ProcStat { QString comm, state; int ppid = 0; qint64 start = 0; bool ok = false; };
static ProcStat procStat(int pid) {
    ProcStat s;
    const QByteArray b = procFile(pid, "stat");
    const int l = b.indexOf('('), r = b.lastIndexOf(')');
    if (l < 0 || r < l) return s;
    s.comm = QString::fromLocal8Bit(b.mid(l + 1, r - l - 1));
    const QList<QByteArray> f = b.mid(r + 2).split(' ');
    if (f.size() < 20) return s;
    s.state = QString::fromLatin1(f[0]);
    s.ppid = f[1].toInt();
    qint64 btime = 0;
    QFile st(QStringLiteral("/proc/stat"));
    if (st.open(QIODevice::ReadOnly))
        for (const QByteArray &ln : st.readAll().split('\n'))
            if (ln.startsWith("btime ")) btime = ln.mid(6).toLongLong();
    s.start = btime + f[19].toLongLong() / qMax(1L, sysconf(_SC_CLK_TCK));
    s.ok = true;
    return s;
}
static QString duration(qint64 s) {
    if (s < 120) return QStringLiteral("%1 s").arg(s);
    if (s < 7200) return QStringLiteral("%1 min").arg(qRound(s / 60.0));
    if (s < 172800) return QStringLiteral("%1 h").arg(qRound(s / 360.0) / 10.0);
    return QStringLiteral("%1 d").arg(qRound(s / 8640.0) / 10.0);
}

void NetPage::connDetails(const QString &ip, int pid, const QString &exe, qint64 uid, const QString &state) {
    if (ip.isEmpty()) return;
    QStringList out;
    const auto when = [](qint64 t) { return QDateTime::fromSecsSinceEpoch(t).toString(QStringLiteral("yyyy-MM-dd HH:mm:ss")); };

    // Process: only trusted while it is the same program (a log pid may have been reused).
    out << QStringLiteral("== Process ==");
    const ProcStat ps = pid > 0 ? procStat(pid) : ProcStat();
    const QString liveExe = pid > 0 ? QFileInfo(QStringLiteral("/proc/%1/exe").arg(pid)).symLinkTarget() : QString();
    const bool alive = ps.ok && (exe.isEmpty() || liveExe.isEmpty() || liveExe == exe);
    if (pid <= 0) out << QStringLiteral("No process known for this connection (short-lived, or owned by another user: use \"All users (root)\").");
    else if (!alive) out << QStringLiteral("pid %1 has already exited (or was reused).").arg(pid);
    if (!exe.isEmpty()) out << QStringLiteral("Program:   %1").arg(exe);
    if (uid >= 0) out << QStringLiteral("User:      %1 (uid %2)").arg(userName(uint(uid))).arg(uid);
    if (alive) {
        out << QStringLiteral("PID:       %1   state %2   name %3").arg(pid).arg(ps.state, ps.comm);
        out << QStringLiteral("Started:   %1 (%2 ago)").arg(when(ps.start), duration(QDateTime::currentSecsSinceEpoch() - ps.start));
        out << QStringLiteral("Command:   %1").arg(procCmd(pid));
        const QString cwd = QFileInfo(QStringLiteral("/proc/%1/cwd").arg(pid)).symLinkTarget();
        if (!cwd.isEmpty()) out << QStringLiteral("Work dir:  %1").arg(cwd);
        const QString cg = QString::fromLocal8Bit(procFile(pid, "cgroup")).trimmed().section(QLatin1Char('\n'), -1).section(QLatin1Char(':'), 2);
        if (!cg.isEmpty() && cg != QLatin1String("/")) out << QStringLiteral("Cgroup:    %1").arg(cg);
        out << QStringLiteral("Started by (parent chain):");
        int cur = ps.ppid;
        for (int depth = 0; cur > 0 && depth < 12; ++depth) {
            const ProcStat pp = procStat(cur);
            if (!pp.ok) break;
            out << QStringLiteral("  %1 (pid %2)  %3").arg(pp.comm).arg(cur).arg(procCmd(cur));
            cur = pp.ppid;
        }
    }
    if (!state.isEmpty() && state.at(0).isUpper()) out << QStringLiteral("TCP state: %1").arg(state);

    // History of this address in the connection log (needs "Log connections" on).
    struct Hit { qint64 ts; QString prog, dir, host, port; };
    QList<Hit> hits;
    for (const char *suffix : {".1", ""}) {
        QFile f(QLatin1String(CONN_LOG) + QLatin1String(suffix));
        if (!f.open(QIODevice::ReadOnly)) continue;
        while (!f.atEnd()) {
            const QJsonObject o = QJsonDocument::fromJson(f.readLine()).object();
            if (str(o, "remote") != ip) continue;
            const bool in = str(o, "dir") == QLatin1String("in");
            hits.append({o.value(QLatin1String("ts")).toInteger(), baseName(str(o, "exe")), str(o, "dir"), str(o, "host"),
                         QString::number(o.value(QLatin1String(in ? "lport" : "rport")).toInt())});
        }
    }
    std::sort(hits.begin(), hits.end(), [](const Hit &a, const Hit &b) { return a.ts < b.ts; });
    out << QString() << QStringLiteral("== History of %1 in the connection log ==").arg(ip);
    if (hits.isEmpty()) {
        out << QStringLiteral("No entries. Turn on \"Log connections\" (Log tab) to record new connections with their program.");
    } else {
        QMap<QString, int> progs, ports;
        QSet<QString> names;
        int outCount = 0;
        for (const Hit &h : hits) {
            ++progs[h.prog.isEmpty() ? QStringLiteral("(unknown)") : h.prog];
            ++ports[h.port];
            outCount += h.dir != QLatin1String("in");
            if (!h.host.isEmpty()) names.insert(h.host);
        }
        out << QStringLiteral("%1 connections (%2 outbound, %3 inbound), first %4, last %5").arg(hits.size()).arg(outCount).arg(hits.size() - outCount).arg(when(hits.first().ts), when(hits.last().ts));
        QList<qint64> gaps;
        for (int i = 1; i < hits.size(); ++i)
            if (hits[i].ts > hits[i - 1].ts) gaps << hits[i].ts - hits[i - 1].ts;
        if (gaps.size() >= 2) {
            std::sort(gaps.begin(), gaps.end());
            out << QStringLiteral("Rhythm:    median gap %1 (shortest %2, longest %3)").arg(duration(gaps.at(gaps.size() / 2)), duration(gaps.first()), duration(gaps.last()));
        }
        QStringList pl, ql;
        for (auto i = progs.constBegin(); i != progs.constEnd(); ++i) pl << QStringLiteral("%1 ×%2").arg(i.key()).arg(i.value());
        for (auto i = ports.constBegin(); i != ports.constEnd(); ++i) ql << QStringLiteral("%1 ×%2").arg(i.key()).arg(i.value());
        out << QStringLiteral("Programs:  %1").arg(pl.join(QStringLiteral(", ")));
        out << QStringLiteral("Ports:     %1").arg(ql.join(QStringLiteral(", ")));
        if (!names.isEmpty()) out << QStringLiteral("DNS names asked for before connecting: %1").arg(QStringList(names.values()).join(QStringLiteral(", ")));
        out << QStringLiteral("Most recent:");
        for (int i = hits.size() - 1; i >= qMax(0, hits.size() - 8); --i)
            out << QStringLiteral("  %1  %2  %3  port %4").arg(when(hits[i].ts), hits[i].dir == QLatin1String("in") ? QStringLiteral("←") : QStringLiteral("→"), hits[i].prog, hits[i].port);
    }

    auto *d = new QDialog(this);
    d->setAttribute(Qt::WA_DeleteOnClose);
    d->setWindowTitle(QStringLiteral("Connection details — ") + ip);
    d->resize(760, 560);
    auto *v = new QVBoxLayout(d);
    auto *txt = new QPlainTextEdit(out.join(QLatin1Char('\n')));
    txt->setReadOnly(true);
    txt->setLineWrapMode(QPlainTextEdit::NoWrap);
    txt->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    auto *box = new QDialogButtonBox(QDialogButtonBox::Close);
    auto *bInfo = box->addButton(QStringLiteral("IP info…"), QDialogButtonBox::ActionRole);
    auto *bCopy = box->addButton(QStringLiteral("Copy"), QDialogButtonBox::ActionRole);
    connect(bInfo, &QPushButton::clicked, this, [this, ip] { ipInfo(ip); });
    connect(bCopy, &QPushButton::clicked, d, [txt] { QGuiApplication::clipboard()->setText(txt->toPlainText()); });
    connect(box, &QDialogButtonBox::rejected, d, &QDialog::close);
    v->addWidget(txt, 1);
    v->addWidget(box);
    d->show();
}

/// Reverse DNS (system resolver) and the registry record (whois, through the unprivileged helper) of one address.
void NetPage::ipInfo(const QString &ip) {
    if (ip.isEmpty()) return;
    auto *d = new QDialog(this);
    d->setAttribute(Qt::WA_DeleteOnClose);
    d->setWindowTitle(QStringLiteral("IP info — ") + ip);
    d->resize(640, 520);
    auto *v = new QVBoxLayout(d);
    auto *head = new QLabel;
    head->setTextFormat(Qt::RichText);
    head->setTextInteractionFlags(Qt::TextSelectableByMouse);
    head->setWordWrap(true);
    auto *raw = new QPlainTextEdit;
    raw->setReadOnly(true);
    raw->setLineWrapMode(QPlainTextEdit::NoWrap);
    raw->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    auto *box = new QDialogButtonBox(QDialogButtonBox::Close);
    auto *bBl = box->addButton(QStringLiteral("Blacklist IP"), QDialogButtonBox::ActionRole);
    bBl->setObjectName(QStringLiteral("btnDanger"));
    connect(bBl, &QPushButton::clicked, this, [this, ip] { changeList("blacklist", "add", ip); });
    auto *bNet = box->addButton(QStringLiteral("Blacklist network…"), QDialogButtonBox::ActionRole);
    bNet->setObjectName(QStringLiteral("btnDanger"));
    connect(bNet, &QPushButton::clicked, this, [this, ip] { blacklistNetwork(ip); });
    connect(box, &QDialogButtonBox::rejected, d, &QDialog::close);
    v->addWidget(head);
    v->addWidget(raw, 1);
    v->addWidget(box);

    // The two lookups finish in any order; each fills its part of the header.
    auto parts = std::make_shared<QStringList>(QStringList{QStringLiteral("…"), QStringLiteral("looking up the registry record…")});
    auto render = [head, parts, ip] {
        head->setText(QStringLiteral("<b>%1</b><br>Reverse DNS: %2<br>%3").arg(ip.toHtmlEscaped(), parts->at(0), parts->at(1)));
    };
    render();
    QHostInfo::lookupHost(ip, d, [parts, render, ip](const QHostInfo &h) {
        (*parts)[0] = h.error() == QHostInfo::NoError && !h.hostName().isEmpty() && h.hostName() != ip ? h.hostName().toHtmlEscaped() : QStringLiteral("none");
        render();
    });
    QPointer<QDialog> alive(d);
    auto show = [alive, raw, parts, render](const QJsonObject &r) {
        if (!alive) return;
        if (!r.value(QLatin1String("ok")).toBool()) {
            (*parts)[1] = QStringLiteral("<span style='color:%1'>whois: %2</span>").arg(QLatin1String(theme::DANGER),
                              (r.isEmpty() ? QStringLiteral("no answer from the helper") : str(r, "error")).toHtmlEscaped());
        } else {
            QStringList rows;
            for (const QJsonValue &x : r.value(QLatin1String("summary")).toArray())
                rows << QStringLiteral("<tr><td>%1:&nbsp;&nbsp;</td><td><b>%2</b></td></tr>").arg(x.toArray().at(0).toString().toHtmlEscaped(), x.toArray().at(1).toString().toHtmlEscaped());
            const QString server = str(r, "server");
            (*parts)[1] = QStringLiteral("<table>%1</table>%2").arg(rows.join(QString()),
                              server.isEmpty() ? QString() : QStringLiteral("<span style='color:%1'>source: %2</span>").arg(QLatin1String(theme::MUTED), server.toHtmlEscaped()));
            raw->setPlainText(str(r, "raw"));
        }
        render();
    };
    d->show();
    whois(ip, show);
}

/// Registry record of an address, asked once per session.
void NetPage::whois(const QString &ip, std::function<void(const QJsonObject &)> cb) {
    if (whois_.contains(ip)) { cb(whois_.value(ip)); return; }
    runUser({{"op", "whois"}, {"ip", ip}}, [this, ip, cb](const QJsonObject &r) {
        if (r.value(QLatin1String("ok")).toBool()) whois_.insert(ip, r);
        cb(r);
    }, 25000);
}

/// The address entries of every right-click menu.
void NetPage::addIpActions(QMenu &m, const QString &ip) {
    if (ip.isEmpty()) return;
    const bool v6 = ip.contains(QLatin1Char(':'));
    const QString subnet = v6 ? ip + QStringLiteral("/64") : ip.section(QLatin1Char('.'), 0, 2) + QStringLiteral(".0/24");  // the helper clears the host bits
    m.addAction(QStringLiteral("IP info"), this, [this, ip] { ipInfo(ip); });
    m.addAction(QStringLiteral("Copy address"), this, [ip] { QGuiApplication::clipboard()->setText(ip); });
    m.addSeparator();
    m.addAction(QStringLiteral("Blacklist ") + ip, this, [this, ip] { changeList("blacklist", "add", ip); });
    m.addAction(v6 ? QStringLiteral("Blacklist its /64 subnet") : QStringLiteral("Blacklist subnet ") + subnet, this, [this, subnet] { changeList("blacklist", "add", subnet); });
    m.addAction(QStringLiteral("Blacklist the owner's whole network…"), this, [this, ip] { blacklistNetwork(ip); });
}

/// Bans the registered block the address sits in (whois range), as one group named after its owner.
void NetPage::blacklistNetwork(const QString &ip) {
    whois(ip, [this, ip](const QJsonObject &r) {
        if (!r.value(QLatin1String("ok")).toBool()) {
            QMessageBox::warning(this, "Blacklist network", r.isEmpty() ? QStringLiteral("No answer from the whois lookup.") : str(r, "error"));
            return;
        }
        QString range, cidr, org, net;
        for (const QJsonValue &x : r.value(QLatin1String("summary")).toArray()) {
            const QString k = x.toArray().at(0).toString(), v = x.toArray().at(1).toString();
            if (k == QLatin1String("Range")) range = v;
            else if (k == QLatin1String("CIDR")) cidr = v;
            else if (k == QLatin1String("Organisation")) org = v;
            else if (k == QLatin1String("Network")) net = v;
        }
        QStringList entries;
        if (!range.isEmpty()) entries << range;  // the registered block itself; a route can be far wider
        else for (const QString &c : cidr.split(QLatin1Char(','), Qt::SkipEmptyParts)) entries << c.trimmed();
        if (entries.isEmpty() || str(r, "server").isEmpty()) {
            QMessageBox::information(this, "Blacklist network", QStringLiteral("The registry record of %1 names no address range.").arg(ip));
            return;
        }
        const QString label = (net.isEmpty() || org.isEmpty() ? net + org : net + QStringLiteral(" — ") + org).left(64);
        if (QMessageBox::question(this, "Blacklist network", QStringLiteral("Block this whole range for every program?\n\n%1\n%2\n\nIt is added as one group; remove the group in Rules to undo it.")
                                      .arg(entries.join(QStringLiteral(", ")), label)) != QMessageBox::Yes) return;
        runRoot({{"op", "blacklist"}, {"add", QJsonArray::fromStringList(entries)}, {"label", label.isEmpty() ? ip : label}});
    });
}

/// Paste or load a list; it is banned as one named group.
void NetPage::bulkBlacklist() {
    QDialog d(this);
    d.setWindowTitle(QStringLiteral("Blacklist many addresses"));
    d.resize(520, 420);
    auto *v = new QVBoxLayout(&d);
    auto *hint = new QLabel(QStringLiteral("One entry per line (commas work too): an address, a CIDR block, or a range \"first - last\". Text after # is ignored."));
    hint->setWordWrap(true);
    v->addWidget(hint);
    auto *text = new QPlainTextEdit;
    text->setPlaceholderText(QStringLiteral("203.0.113.7\n198.51.100.0/24\n192.0.2.10 - 192.0.2.80\n2001:db8::/32"));
    v->addWidget(text, 1);
    auto *row = new QHBoxLayout;
    row->addWidget(new QLabel(QStringLiteral("Group:")));
    auto *label = new QLineEdit(QStringLiteral("Bulk ") + QDateTime::currentDateTime().toString(QStringLiteral("yyyy-MM-dd HH:mm")));
    label->setToolTip(QStringLiteral("The entries are listed under this name in Rules; removing the group removes them all."));
    row->addWidget(label, 1);
    auto *bFile = new QPushButton(QStringLiteral("Load file…"));
    row->addWidget(bFile);
    v->addLayout(row);
    auto *box = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    v->addWidget(box);
    connect(box, &QDialogButtonBox::accepted, &d, &QDialog::accept);
    connect(box, &QDialogButtonBox::rejected, &d, &QDialog::reject);
    connect(bFile, &QPushButton::clicked, &d, [&d, text, label] {
        const QString path = QFileDialog::getOpenFileName(&d, QStringLiteral("Address list"), QDir::homePath());
        QFile f(path);
        if (path.isEmpty() || !f.open(QIODevice::ReadOnly)) return;
        text->setPlainText(QString::fromUtf8(f.read(1 << 20)));
        label->setText(QFileInfo(path).completeBaseName());
    });
    if (d.exec() != QDialog::Accepted) return;
    static const QRegularExpression dash(QStringLiteral("\\s*-\\s*")), sep(QStringLiteral("[,;\\s]+"));
    QJsonArray add;
    const QStringList lines = text->toPlainText().split(QLatin1Char('\n'));
    for (const QString &raw : lines) {
        const QString line = raw.section(QLatin1Char('#'), 0, 0).replace(dash, QStringLiteral("-"));  // "a - b" stays one token
        for (const QString &tok : line.split(sep, Qt::SkipEmptyParts)) add.append(tok);
    }
    if (!add.isEmpty()) runRoot({{"op", "blacklist"}, {"add", add}, {"label", label->text().trimmed()}});
}

/// Removes the selected entries; a selected group takes all of its entries with it.
void NetPage::removeBlacklisted(bool all) {
    if (all) {
        if (blacklist_.isEmpty() || QMessageBox::question(this, "Blacklist", QStringLiteral("Remove all %1 blacklist entries?").arg(blacklist_.size())) != QMessageBox::Yes) return;
        runRoot({{"op", "blacklist"}, {"remove_all", true}});
        return;
    }
    QSet<QString> pick;
    const QList<QTreeWidgetItem *> selected = bl_->selectedItems();
    for (const QTreeWidgetItem *it : selected) {
        pick.insert(it->data(0, IpRole).toString());
        for (int i = 0; i < it->childCount(); ++i) pick.insert(it->child(i)->data(0, IpRole).toString());
    }
    pick.remove(QString());
    if (!pick.isEmpty()) runRoot({{"op", "blacklist"}, {"remove", QJsonArray::fromStringList(QStringList(pick.cbegin(), pick.cend()))}});
}
