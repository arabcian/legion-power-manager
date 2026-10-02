#include "netpage.h"

#include "privileged.h"
#include "theme.h"

#include <QCheckBox>
#include <QDateTime>
#include <QDir>
#include <QFile>
#include <QFileDialog>
#include <QFileInfo>
#include <QFileSystemWatcher>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QHideEvent>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QListWidget>
#include <QMessageBox>
#include <QPointer>
#include <QProcess>
#include <QPushButton>
#include <QScrollBar>
#include <QShowEvent>
#include <QSignalBlocker>
#include <QTabWidget>
#include <QTimer>
#include <QTreeWidget>
#include <QVBoxLayout>

#include <pwd.h>

static constexpr int POLL_MS = 3000, MAX_BLOCK_ROWS = 1000, ALERT_GAP_MS = 10000;
static const char *const LOG_DIR = "/var/log/legion-power-manager";
static const char *const LOG_FILE = "/var/log/legion-power-manager/netguard.log";
enum { IdxRole = Qt::UserRole, KeyRole, ExeRole, IpRole };

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

static QTreeWidget *makeTree(const QStringList &cols) {
    auto *t = new QTreeWidget;
    t->setColumnCount(cols.size());
    t->setHeaderLabels(cols);
    t->setRootIsDecorated(false);
    t->setAlternatingRowColors(true);
    t->setUniformRowHeights(true);
    t->header()->setStretchLastSection(true);
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
    top->addWidget(guard_);
    top->addWidget(lan_);
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

    // ── Rules ──
    auto *pr = new QWidget;
    auto *hr = new QHBoxLayout(pr);
    hr->setContentsMargins(6, 6, 6, 6);
    auto column = [&](const char *title, const char *hint, QListWidget *&list, QLineEdit *&edit) {
        auto *col = new QVBoxLayout;
        col->addWidget(new QLabel(QStringLiteral("<b>%1</b>").arg(QLatin1String(title))));
        list = new QListWidget;
        list->setSelectionMode(QAbstractItemView::ExtendedSelection);
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
    auto *rl = column("Blacklist — addresses blocked for every program", "203.0.113.7  ·  198.51.100.0/24  ·  2001:db8::/32", bl_, blEdit_);
    auto *bBlAdd = button("Add", rl);
    auto *bBlDel = button("Remove", rl, true);
    tabs_->addTab(pr, "Rules");

    // ── wiring ──
    connect(guard_, &QCheckBox::toggled, this, [this](bool on) { runRoot({{"op", "set"}, {"guard", on}}); });
    connect(lan_, &QCheckBox::toggled, this, [this](bool on) { runRoot({{"op", "set"}, {"allow_lan", on}}); });
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
    connect(bWlC, &QPushButton::clicked, this, [this] {
        const QJsonObject c = curConn();
        if (!c.value(QLatin1String("wine")).toBool()) { QMessageBox::information(this, "Network", "Select a connection of a Wine / .exe program (shown in purple)."); return; }
        changeList("whitelist", "add", str(c, "exe"));
    });
    connect(bBlC, &QPushButton::clicked, this, [this] { changeList("blacklist", "add", str(curConn(), "remote")); });
    connect(bKill, &QPushButton::clicked, this, [this] {
        const QJsonObject c = curConn();
        if (str(c, "remote").isEmpty()) return;
        runRoot({{"op", "kill"}, {"proto", c.value(QLatin1String("proto"))}, {"lport", c.value(QLatin1String("lport"))},
                 {"remote", c.value(QLatin1String("remote"))}, {"rport", c.value(QLatin1String("rport"))}}, false);
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
    auto removeSelected = [this](const char *op, QListWidget *l) {
        QJsonArray a;
        for (const QListWidgetItem *it : l->selectedItems()) a.append(it->text());
        if (!a.isEmpty()) runRoot({{"op", op}, {"remove", a}});
    };
    connect(bWlDel, &QPushButton::clicked, this, [=, this] { removeSelected("whitelist", wl_); });
    connect(bBlDel, &QPushButton::clicked, this, [=, this] { removeSelected("blacklist", bl_); });

    timer_ = new QTimer(this);
    timer_->setInterval(POLL_MS);
    connect(timer_, &QTimer::timeout, this, &NetPage::refreshConns);

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

/// The connection list refreshes only while it is what the user is looking at.
void NetPage::poll() {
    if (isVisible() && tabs_->currentIndex() == 0) {
        if (!viaRoot_) refreshConns();
        timer_->start();
    } else {
        timer_->stop();
    }
}

void NetPage::runUser(const QJsonObject &req, std::function<void(const QJsonObject &)> cb) {
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
    QTimer::singleShot(10000, p, [guard] { if (guard && guard->state() != QProcess::NotRunning) guard->kill(); });
    p->start(helper(), {});
    p->write(QJsonDocument(req).toJson(QJsonDocument::Compact));
    p->closeWriteChannel();
}

void NetPage::runRoot(const QJsonObject &req, bool isStatus) {
    privileged::run(helper(), req, this, [this, isStatus](const privileged::Result &r) {
        if (!r.ok()) {
            QMessageBox::warning(this, "Network", r.message());
            const QSignalBlocker b1(guard_), b2(lan_);  // put the check boxes back
            guard_->setChecked(state_.value(QLatin1String("guard")).toBool());
            lan_->setChecked(state_.value(QLatin1String("allow_lan")).toBool(true));
            return;
        }
        if (isStatus) applyStatus(r.json);
        else readLog(false);
        viaRoot_ = false;
        if (isVisible()) refreshConns();
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
    { const QSignalBlocker b1(guard_), b2(lan_);
      guard_->setChecked(guard);
      lan_->setChecked(s.value(QLatin1String("allow_lan")).toBool(true));
      lan_->setEnabled(guard); }
    auto fill = [&](QListWidget *l, const char *key) {
        l->clear();
        for (const QJsonValue &x : s.value(QLatin1String(key)).toArray()) l->addItem(x.toString());
    };
    fill(wl_, "whitelist");
    fill(bl_, "blacklist");

    QStringList lines;
    if (running) {
        lines << QStringLiteral("<span style='color:%1'>●</span> Daemon running — blacklist enforced (%2), Wine guard <b>%3</b> (%4 whitelisted)")
                     .arg(QLatin1String(theme::OK)).arg(bl_->count()).arg(QLatin1String(guard ? "on" : "off")).arg(wl_->count());
    } else {
        const bool systemd = QFileInfo::exists(QStringLiteral("/run/systemd/system"));
        lines << QStringLiteral("<span style='color:%1'>○ Daemon not running — rules are saved, nothing is enforced.</span> Start it: <code>%2</code>")
                     .arg(QLatin1String(theme::WARN), systemd ? QStringLiteral("systemctl enable --now lpm-netguard")
                                                              : QStringLiteral("rc-service lpm-netguard start &amp;&amp; rc-update add lpm-netguard default"));
    }
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
    QStringList black;
    for (int i = 0; i < bl_->count(); ++i) black << bl_->item(i)->text();
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
        auto *it = new QTreeWidgetItem({name, pid ? QString::number(pid) : QString(), str(c, "proto").toUpper(), local, peer, state});
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
