#include "backuppage.h"

#include "privileged.h"
#include "sysinfo.h"
#include "theme.h"

#include <QCheckBox>
#include <QComboBox>
#include <QCoreApplication>
#include <QDateTime>
#include <QDir>
#include <QFile>
#include <QFileDialog>
#include <QFileInfo>
#include <QFontDatabase>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QLocale>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QProcess>
#include <QProgressBar>
#include <QPushButton>
#include <QSettings>
#include <QShowEvent>
#include <QSpinBox>
#include <QStandardPaths>
#include <QStorageInfo>
#include <QTreeWidget>
#include <QVBoxLayout>

namespace {

enum { PathRole = Qt::UserRole, KindRole };

// Mirrors backup::DEFAULT_EXCLUDES. "/x/*" keeps the empty mount point in the
// archive; /home/* is the "Include /home" box.
const char *const DEFAULT_EXCLUDES =
    "/proc/*\n/sys/*\n/dev/*\n/run/*\n/tmp/*\n/var/tmp/*\n/mnt/*\n/media/*\n/lost+found\n/var/cache/*\n/usr/portage/distfiles/*";

struct Comp { const char *id, *label; int lo, hi, def; };
const Comp COMPS[] = {
    {"pigz", "gzip (pigz, all cores) — .tar.gz", 1, 9, 6},
    {"zstd", "zstd (all cores, fastest) — .tar.zst", 1, 19, 3},
    {"xz", "xz (all cores, smallest, slow) — .tar.xz", 0, 9, 6},
};

QString helper() { return privileged::helperPath(QStringLiteral("backup-helper")); }
QString settingsPath() {
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QStringLiteral("/legion-power-manager/gui.ini");
}
QString human(qint64 bytes) { return QLocale::c().formattedDataSize(bytes, 1, QLocale::DataSizeIecFormat); }
QString clock(qint64 s) {
    return QStringLiteral("%1:%2:%3").arg(s / 3600).arg(s / 60 % 60, 2, 10, QLatin1Char('0')).arg(s % 60, 2, 10, QLatin1Char('0'));
}
QString str(const QJsonObject &o, const char *k) { return o.value(QLatin1String(k)).toString(); }
qint64 num(const QJsonObject &o, const char *k) { return o.value(QLatin1String(k)).toInteger(); }

QJsonObject readJson(const QString &path) {
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly) || f.size() > (1 << 20)) return {};
    return QJsonDocument::fromJson(f.readAll()).object();
}

bool isImageName(const QString &n) {
    return n.startsWith(QLatin1String("backup-")) &&
           (n.endsWith(QLatin1String(".tar.gz")) || n.endsWith(QLatin1String(".tar.zst")) || n.endsWith(QLatin1String(".tar.xz")));
}
bool isConfigName(const QString &n) { return n.startsWith(QLatin1String("lpm-config-")) && n.endsWith(QLatin1String(".tar.gz")); }

QHBoxLayout *folderRow(const QString &label, QLineEdit *edit, QWidget *owner, const QString &title) {
    auto *h = new QHBoxLayout;
    h->addWidget(new QLabel(label));
    h->addWidget(edit, 1);
    auto *b = new QPushButton(QStringLiteral("Browse…"));
    QObject::connect(b, &QPushButton::clicked, owner, [edit, owner, title] {
        const QString d = QFileDialog::getExistingDirectory(owner, title, edit->text());
        if (!d.isEmpty()) { edit->setText(d); Q_EMIT edit->editingFinished(); }
    });
    h->addWidget(b);
    return h;
}

} // namespace

BackupPage::BackupPage(QWidget *parent) : QWidget(parent) {
    QSettings st(settingsPath(), QSettings::IniFormat);
    st.beginGroup(QStringLiteral("backup"));
    auto *v = new QVBoxLayout(this);
    v->setContentsMargins(8, 6, 8, 6);
    v->setSpacing(6);
    auto action = [this](const char *text, QBoxLayout *l, const char *objName = nullptr) {
        auto *b = new QPushButton(QString::fromUtf8(text));
        if (objName) b->setObjectName(QLatin1String(objName));
        l->addWidget(b);
        actions_.append(b);
        return b;
    };

    // ── LPM configuration ──
    auto *cb = new QGroupBox(QStringLiteral("Legion Power Manager configuration"));
    auto *cv = new QVBoxLayout(cb);
    cv->setContentsMargins(8, 6, 8, 6);
    cv->setSpacing(4);
    cfgDir_ = new QLineEdit(st.value(QStringLiteral("config_dir"), QDir::homePath() + QStringLiteral("/Backups/legion-power-manager")).toString());
    cv->addLayout(folderRow(QStringLiteral("Folder"), cfgDir_, this, QStringLiteral("Folder for configuration backups")));
    auto *cr = new QHBoxLayout;
    cfgProfile_ = new QCheckBox(QStringLiteral("System profile"));
    cfgProfile_->setChecked(st.value(QStringLiteral("config_profile"), true).toBool());
    cfgProfile_->setToolTip(QStringLiteral(
        "Also saves how this installation is put together, as reference material:\n"
        "/etc/portage, the world set, the kernel .config, fstab, GRUB/dracut/kernel settings,\n"
        "modprobe.d, modules-load.d, sysctl, conf.d, runlevels, udev rules, plus the kernel\n"
        "command line, DMI, loaded modules and the installed package list.\n"
        "It is stored under system-profile/ and is never restored automatically."));
    cfgLogs_ = new QCheckBox(QStringLiteral("Logs"));
    cfgLogs_->setChecked(st.value(QStringLiteral("config_logs"), false).toBool());
    cfgLogs_->setToolTip(QStringLiteral("Health / kernel-log history, the write log and the network-guard log."));
    cr->addWidget(cfgProfile_);
    cr->addWidget(cfgLogs_);
    auto *ci = new QLabel(QStringLiteral("Scenes, presets, CPU/GPU curves, lighting, boot profiles, network-guard rules, calibration — one dated .tar.gz."));
    ci->setProperty("role", "muted");
    cr->addWidget(ci, 1);
    connect(action("Back up now", cr, "btnAccent"), &QPushButton::clicked, this, &BackupPage::configBackup);
    connect(action("Restore selected…", cr), &QPushButton::clicked, this, &BackupPage::configRestore);
    cv->addLayout(cr);
    v->addWidget(cb);

    // ── System image ──
    auto *sb = new QGroupBox(QStringLiteral("System image (tar of /)"));
    auto *sv = new QVBoxLayout(sb);
    sv->setContentsMargins(8, 6, 8, 6);
    sv->setSpacing(4);
    sysDir_ = new QLineEdit(st.value(QStringLiteral("system_dir"), QStringLiteral("/")).toString());
    auto *fr = folderRow(QStringLiteral("Folder"), sysDir_, this, QStringLiteral("Folder for system images"));
    free_ = new QLabel;
    free_->setProperty("role", "muted");
    fr->addWidget(free_);
    sv->addLayout(fr);

    auto *o = new QHBoxLayout;
    comp_ = new QComboBox;
    for (const Comp &c : COMPS) comp_->addItem(QString::fromUtf8(c.label), QLatin1String(c.id));
    comp_->setCurrentIndex(std::max(0, comp_->findData(st.value(QStringLiteral("compressor"), QStringLiteral("pigz")))));
    level_ = new QSpinBox;
    auto levelRange = [this](bool reset) {
        const Comp &c = COMPS[comp_->currentIndex()];
        level_->setRange(c.lo, c.hi);
        if (reset) level_->setValue(c.def);
    };
    levelRange(true);
    if (st.contains(QStringLiteral("level"))) level_->setValue(st.value(QStringLiteral("level")).toInt());
    connect(comp_, &QComboBox::currentIndexChanged, this, [levelRange] { levelRange(true); });
    low_ = new QCheckBox(QStringLiteral("Low priority"));
    low_->setChecked(st.value(QStringLiteral("low_priority"), true).toBool());
    low_->setToolTip(QStringLiteral("tar and the compressor run at nice 19 and the lowest I/O priority: the desktop stays responsive."));
    verify_ = new QCheckBox(QStringLiteral("Verify"));
    verify_->setChecked(st.value(QStringLiteral("verify"), true).toBool());
    verify_->setToolTip(QStringLiteral("Reads the finished archive back (decompress + tar listing) and compares the entry count."));
    home_ = new QCheckBox(QStringLiteral("Include /home"));
    home_->setChecked(st.value(QStringLiteral("include_home"), false).toBool());
    keep_ = new QSpinBox;
    keep_->setRange(0, 99);
    keep_->setSpecialValueText(QStringLiteral("all"));
    keep_->setValue(st.value(QStringLiteral("keep"), 0).toInt());
    keep_->setToolTip(QStringLiteral("After a successful image, older backup-YYYY-MM-DD images in this folder beyond this count are deleted."));
    o->addWidget(new QLabel(QStringLiteral("Compression")));
    o->addWidget(comp_);
    o->addWidget(new QLabel(QStringLiteral("Level")));
    o->addWidget(level_);
    o->addWidget(low_);
    o->addWidget(verify_);
    o->addWidget(home_);
    o->addWidget(new QLabel(QStringLiteral("Keep newest")));
    o->addWidget(keep_);
    o->addStretch(1);
    sv->addLayout(o);

    auto *er = new QHBoxLayout;
    auto *el = new QLabel(QStringLiteral("Exclude"));
    el->setAlignment(Qt::AlignTop);
    er->addWidget(el);
    excludes_ = new QPlainTextEdit(st.value(QStringLiteral("excludes"), QString::fromLatin1(DEFAULT_EXCLUDES)).toString());
    excludes_->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    excludes_->setFixedHeight(fontMetrics().lineSpacing() * 4 + 12);
    excludes_->setToolTip(QStringLiteral(
        "One tar --exclude pattern per line. \"/proc/*\" (not \"/proc\") keeps the empty mount point,\n"
        "so a restore onto a fresh disk can still mount /proc, /sys, /dev and /run.\n"
        "The archive being written and older backup-* images in the folder are always excluded."));
    er->addWidget(excludes_, 1);
    auto *ec = new QVBoxLayout;
    auto *bDef = new QPushButton(QStringLiteral("Defaults"));
    connect(bDef, &QPushButton::clicked, this, [this] { excludes_->setPlainText(QString::fromLatin1(DEFAULT_EXCLUDES)); });
    ec->addWidget(bDef);
    connect(action("Create image", ec, "btnAccent"), &QPushButton::clicked, this, &BackupPage::systemBackup);
    ec->addStretch(1);
    er->addLayout(ec);
    sv->addLayout(er);

    target_ = new QLineEdit(st.value(QStringLiteral("restore_target")).toString());
    target_->setPlaceholderText(QStringLiteral("/mnt/gentoo — or / to restore over the running system"));
    auto *rr = folderRow(QStringLiteral("Restore to"), target_, this, QStringLiteral("Directory to restore the image into"));
    connect(action("Verify selected", rr), &QPushButton::clicked, this, &BackupPage::verifySelected);
    connect(action("Restore selected…", rr, "btnDanger"), &QPushButton::clicked, this, &BackupPage::systemRestore);
    sv->addLayout(rr);
    v->addWidget(sb);

    // ── Archives ──
    list_ = new QTreeWidget;
    list_->setColumnCount(5);
    list_->setHeaderLabels({QStringLiteral("Archive"), QStringLiteral("Kind"), QStringLiteral("Size"), QStringLiteral("Written"), QStringLiteral("Notes")});
    list_->setRootIsDecorated(false);
    list_->setAlternatingRowColors(true);
    list_->setUniformRowHeights(true);
    for (int i = 0; i < 4; ++i) list_->header()->setSectionResizeMode(i, QHeaderView::ResizeToContents);
    list_->header()->setStretchLastSection(true);
    v->addWidget(list_, 1);

    auto *pr = new QHBoxLayout;
    bar_ = new QProgressBar;
    bar_->setFixedWidth(180);
    bar_->setTextVisible(false);
    bar_->hide();
    pr->addWidget(bar_);
    status_ = new QLabel(QStringLiteral("Idle."));
    status_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    pr->addWidget(status_, 1);
    auto *bAdd = action("Add file…", pr);
    bAdd->setToolTip(QStringLiteral("List an archive that lives somewhere else (to verify or restore it)."));
    connect(bAdd, &QPushButton::clicked, this, [this] {
        const QString f = QFileDialog::getOpenFileName(this, QStringLiteral("Backup archive"), sysDir_->text(),
                                                       QStringLiteral("tar archives (*.tar *.tar.gz *.tgz *.tar.zst *.tar.xz *.tar.bz2);;All files (*)"));
        if (f.isEmpty()) return;
        if (!extra_.contains(f)) extra_.append(f);
        refreshList();
        select(f);
    });
    cancel_ = new QPushButton(QStringLiteral("Cancel"));
    cancel_->setEnabled(false);
    connect(cancel_, &QPushButton::clicked, this, &BackupPage::cancel);
    pr->addWidget(cancel_);
    v->addLayout(pr);

    log_ = new QPlainTextEdit;
    log_->setReadOnly(true);
    log_->setMaximumBlockCount(400);
    log_->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    log_->setFixedHeight(fontMetrics().lineSpacing() * 5 + 12);
    v->addWidget(log_);

    for (QLineEdit *e : {cfgDir_, sysDir_})
        connect(e, &QLineEdit::editingFinished, this, [this] { saveSettings(); refreshList(); updateFree(); });
    connect(target_, &QLineEdit::editingFinished, this, &BackupPage::saveSettings);
}

BackupPage::~BackupPage() {
    saveSettings();
    if (proc_) proc_->closeWriteChannel();  // the helper cancels on EOF
}

void BackupPage::saveSettings() {
    QSettings st(settingsPath(), QSettings::IniFormat);
    st.beginGroup(QStringLiteral("backup"));
    st.setValue(QStringLiteral("config_dir"), cfgDir_->text());
    st.setValue(QStringLiteral("config_profile"), cfgProfile_->isChecked());
    st.setValue(QStringLiteral("config_logs"), cfgLogs_->isChecked());
    st.setValue(QStringLiteral("system_dir"), sysDir_->text());
    st.setValue(QStringLiteral("compressor"), comp_->currentData());
    st.setValue(QStringLiteral("level"), level_->value());
    st.setValue(QStringLiteral("low_priority"), low_->isChecked());
    st.setValue(QStringLiteral("verify"), verify_->isChecked());
    st.setValue(QStringLiteral("include_home"), home_->isChecked());
    st.setValue(QStringLiteral("keep"), keep_->value());
    st.setValue(QStringLiteral("excludes"), excludes_->toPlainText());
    st.setValue(QStringLiteral("restore_target"), target_->text());
}

void BackupPage::showEvent(QShowEvent *e) {
    QWidget::showEvent(e);
    refreshList();
    updateFree();
}

void BackupPage::updateFree() {
    const QStorageInfo si(sysDir_->text());
    free_->setText(si.isValid() ? QStringLiteral("%1 free on %2").arg(human(si.bytesAvailable()), si.rootPath()) : QString());
}

void BackupPage::refreshList() {
    const QString keepSel = selected();
    list_->clear();
    struct Entry { QFileInfo fi; bool image; };
    QList<Entry> found;
    QSet<QString> seen;
    auto scan = [&](const QString &dir) {
        const QDir d(dir);
        if (dir.isEmpty() || !d.exists()) return;
        for (const QFileInfo &fi : d.entryInfoList(QDir::Files | QDir::Hidden | QDir::System, QDir::NoSort)) {
            const bool img = isImageName(fi.fileName()), cfg = isConfigName(fi.fileName());
            if ((img || cfg) && !seen.contains(fi.absoluteFilePath())) { seen.insert(fi.absoluteFilePath()); found.append({fi, img}); }
        }
    };
    scan(cfgDir_->text());
    scan(sysDir_->text());
    for (const QString &f : std::as_const(extra_)) {
        const QFileInfo fi(f);
        if (fi.isFile() && !seen.contains(fi.absoluteFilePath())) { seen.insert(fi.absoluteFilePath()); found.append({fi, !isConfigName(fi.fileName())}); }
    }
    std::sort(found.begin(), found.end(), [](const Entry &a, const Entry &b) { return a.fi.lastModified() > b.fi.lastModified(); });
    for (const Entry &e : std::as_const(found)) {
        QStringList notes;
        if (e.image) {
            const QJsonObject info = readJson(e.fi.absoluteFilePath() + QStringLiteral(".info.json"));
            if (!info.isEmpty()) {
                notes << QStringLiteral("%L1 entries").arg(num(info, "files"));
                if (!str(info, "verified").isEmpty()) notes << QStringLiteral("verified");
                if (info.value(QLatin1String("include_home")).toBool()) notes << QStringLiteral("with /home");
                if (num(info, "tar_status") == 2) notes << QStringLiteral("tar reported errors");
                if (!str(info, "kernel").isEmpty()) notes << QStringLiteral("kernel ") + str(info, "kernel");
            }
        } else if (e.fi.fileName().contains(QLatin1String("pre-restore"))) {
            notes << QStringLiteral("saved automatically before a restore");
        }
        auto *it = new QTreeWidgetItem({e.fi.fileName(), e.image ? QStringLiteral("System image") : QStringLiteral("LPM configuration"),
                                        human(e.fi.size()), e.fi.lastModified().toString(QStringLiteral("yyyy-MM-dd HH:mm")),
                                        notes.join(QStringLiteral(" · "))});
        it->setData(0, PathRole, e.fi.absoluteFilePath());
        it->setData(0, KindRole, e.image ? QStringLiteral("system") : QStringLiteral("config"));
        it->setToolTip(0, e.fi.absoluteFilePath());
        it->setTextAlignment(2, Qt::AlignRight | Qt::AlignVCenter);
        list_->addTopLevelItem(it);
        if (e.fi.absoluteFilePath() == keepSel) list_->setCurrentItem(it);
    }
}

void BackupPage::select(const QString &path) {
    for (int i = 0; i < list_->topLevelItemCount(); ++i)
        if (list_->topLevelItem(i)->data(0, PathRole).toString() == path) { list_->setCurrentItem(list_->topLevelItem(i)); return; }
}

QString BackupPage::selected(QString *kind) const {
    const QTreeWidgetItem *it = list_->currentItem();
    if (!it) return {};
    if (kind) *kind = it->data(0, KindRole).toString();
    return it->data(0, PathRole).toString();
}

// ── job plumbing ────────────────────────────────────────────────────────────

void BackupPage::log(const QString &line, const char *color) {
    if (color) log_->appendHtml(QStringLiteral("<span style='color:%1'>%2</span>").arg(QLatin1String(color), line.toHtmlEscaped()));
    else log_->appendPlainText(line);
}

void BackupPage::setBusy(bool busy, const QString &what) {
    for (QWidget *w : std::as_const(actions_)) w->setEnabled(!busy);
    cancel_->setEnabled(busy);
    bar_->setVisible(busy);
    if (busy) {
        bar_->setRange(0, 0);
        status_->setText(what);
        lastBytes_ = lastElapsed_ = 0;
        rate_ = 0;
        cancelling_ = false;
    }
}

void BackupPage::finished(const QString &summary, bool good) {
    status_->setText(summary);
    theme::setSheet(status_, QStringLiteral("color:%1").arg(QLatin1String(good ? theme::OK : theme::DANGER)));
    log(summary, good ? theme::OK : theme::DANGER);
    refreshList();
    updateFree();
    if (!isVisible()) Q_EMIT alert(QStringLiteral("Backup"), summary);
}

void BackupPage::run(const QJsonObject &req, bool root, const QString &what, Done done) {
    auto failNow = [&](const QString &msg) { done(QJsonObject{{QStringLiteral("ok"), false}, {QStringLiteral("error"), msg}}); };
    if (proc_) return failNow(QStringLiteral("another backup job is running"));
    if (qEnvironmentVariableIsSet("LPM_PGO_TRAIN")) return failNow(QStringLiteral("disabled during the PGO training run"));
    if (!QFileInfo(helper()).isExecutable()) return failNow(QStringLiteral("backup-helper is not installed (") + helper() + QLatin1Char(')'));
    QString pkexec;
    for (const char *c : {"/usr/bin/pkexec", "/bin/pkexec"})
        if (QFileInfo(QString::fromLatin1(c)).isFile()) { pkexec = QString::fromLatin1(c); break; }
    if (root && pkexec.isEmpty()) return failNow(QStringLiteral("pkexec was not found. Install polkit (sys-auth/polkit)."));

    // Parented to the app: a root child cannot be killed from here, and a
    // QProcess destructor would block on it. It ends when its stdin closes.
    auto *p = new QProcess(QCoreApplication::instance());
    proc_ = p;
    procRoot_ = root;
    final_ = {};
    theme::setSheet(status_, QString());
    setBusy(true, what);
    QPointer<BackupPage> self(this);
    connect(p, &QProcess::readyReadStandardOutput, this, [this, p] { while (p->canReadLine()) onLine(p->readLine()); });
    auto end = [self, p, done](const QJsonObject &fallback) {
        p->deleteLater();
        if (!self) return;
        while (p->canReadLine()) self->onLine(p->readLine());
        self->proc_ = nullptr;
        self->setBusy(false);
        done(self->final_.isEmpty() ? fallback : self->final_);
    };
    connect(p, &QProcess::finished, p, [end, p](int code, QProcess::ExitStatus) {
        const QString err = QString::fromUtf8(p->readAllStandardError().left(2048)).trimmed();
        QString msg = code == 126 ? QStringLiteral("the authorization request was dismissed.")
                    : code == 127 ? QStringLiteral("polkit did not authorize this action.")
                    : err.isEmpty() ? QStringLiteral("the helper exited with code %1.").arg(code) : err;
        end(QJsonObject{{QStringLiteral("ok"), false}, {QStringLiteral("error"), msg}});
    });
    connect(p, &QProcess::errorOccurred, p, [end, p](QProcess::ProcessError e) {
        if (e == QProcess::FailedToStart)
            end(QJsonObject{{QStringLiteral("ok"), false}, {QStringLiteral("error"), QStringLiteral("could not start: ") + p->errorString()}});
    });
    if (root) { p->setProgram(pkexec); p->setArguments({helper()}); }
    else p->setProgram(helper());
    p->start();
    p->write(QJsonDocument(req).toJson(QJsonDocument::Compact) + '\n');  // stdin stays open: EOF = cancel
}

void BackupPage::cancel() {
    if (!proc_ || cancelling_) return;
    cancelling_ = true;
    status_->setText(QStringLiteral("Cancelling…"));
    proc_->closeWriteChannel();
    // Still waiting for the password: the helper is not running yet, pkexec can be stopped.
    if (procRoot_ && final_.isEmpty() && lastElapsed_ == 0 && bar_->maximum() == 0) proc_->terminate();
}

void BackupPage::onLine(const QByteArray &line) {
    const QJsonObject o = QJsonDocument::fromJson(line).object();
    if (o.isEmpty()) return;
    if (o.contains(QLatin1String("ok"))) { final_ = o; return; }
    const QString ev = str(o, "event"), phase = str(o, "phase");
    if (ev == QLatin1String("start")) {
        if (!str(o, "command").isEmpty()) log(QStringLiteral("$ ") + str(o, "command"), theme::MUTED);
        status_->setText((phase == QLatin1String("backup") ? QStringLiteral("Writing ") : phase == QLatin1String("restore")
                          ? QStringLiteral("Restoring ") : QStringLiteral("Verifying ")) + QFileInfo(str(o, "archive")).fileName() + QStringLiteral("…"));
        lastBytes_ = lastElapsed_ = 0;
        rate_ = 0;
        return;
    }
    if (ev != QLatin1String("progress") || cancelling_) return;
    const qint64 bytes = num(o, "bytes"), total = num(o, "total"), el = num(o, "elapsed");
    if (el > lastElapsed_ && bytes >= lastBytes_) {
        const double r = double(bytes - lastBytes_) / double(el - lastElapsed_);
        rate_ = rate_ > 0 ? rate_ * 0.7 + r * 0.3 : r;
    }
    lastBytes_ = bytes;
    lastElapsed_ = el;
    QString t;
    if (total > 0) {
        bar_->setRange(0, 1000);
        bar_->setValue(int(bytes * 1000 / total));
        t = QStringLiteral("%1 %2 of %3").arg(phase == QLatin1String("restore") ? QStringLiteral("Restoring:") : QStringLiteral("Verifying:"),
                                               human(bytes), human(total));
        if (rate_ > 0 && bytes < total) t += QStringLiteral(" · about %1 left").arg(clock(qint64(double(total - bytes) / rate_)));
    } else {
        bar_->setRange(0, 0);
        t = QStringLiteral("Writing: %1").arg(human(bytes));
    }
    t += QStringLiteral(" · %1/s · %L2 entries · %3").arg(human(qint64(rate_))).arg(num(o, "files")).arg(clock(el));
    const QString cur = str(o, "current");
    if (!cur.isEmpty()) t += QStringLiteral(" · ") + status_->fontMetrics().elidedText(cur, Qt::ElideLeft, 320);
    status_->setText(t);
}

// ── LPM configuration ───────────────────────────────────────────────────────

void BackupPage::configBackup() {
    saveSettings();
    const QString dir = cfgDir_->text().trimmed();
    if (!QDir::isAbsolutePath(dir)) { finished(QStringLiteral("Choose a folder for the configuration backups first."), false); return; }
    run({{"op", "config_backup"}, {"dest_dir", dir}, {"include_profile", cfgProfile_->isChecked()},
         {"include_logs", cfgLogs_->isChecked()}, {"lpm_version", QStringLiteral(LPM_VERSION)}},
        false, QStringLiteral("Saving the configuration…"), [this](const QJsonObject &r) {
            if (!r.value("ok").toBool()) { finished(QStringLiteral("Configuration backup failed: ") + str(r, "error"), false); return; }
            const QJsonObject c = r.value("contents").toObject();
            const QJsonArray skipped = r.value("skipped").toArray();
            for (const QJsonValue &s : skipped) log(QStringLiteral("not saved: ") + s.toString(), theme::WARN);
            finished(QStringLiteral("Saved %1 (%2): %3 user, %4 system, %5 system-profile file(s)%6.")
                         .arg(QFileInfo(str(r, "archive")).fileName(), human(num(r, "bytes")))
                         .arg(num(c, "user_config") + num(c, "user_state")).arg(num(c, "system")).arg(num(c, "system_profile"))
                         .arg(skipped.isEmpty() ? QString() : QStringLiteral(", %1 unreadable (see below)").arg(skipped.size())), true);
            select(str(r, "archive"));
        });
}

void BackupPage::configRestore() {
    QString kind;
    const QString archive = selected(&kind);
    if (archive.isEmpty() || kind != QLatin1String("config")) {
        finished(QStringLiteral("Select an \"LPM configuration\" archive in the list first."), false);
        return;
    }
    run({{"op", "config_inspect"}, {"archive", archive}}, false, QStringLiteral("Reading the backup…"), [this, archive](const QJsonObject &r) {
        if (!r.value("ok").toBool()) { finished(QStringLiteral("Cannot restore: ") + str(r, "error"), false); return; }
        const QJsonObject m = r.value("manifest").toObject(), c = m.value("contents").toObject(), mach = m.value("machine").toObject();
        const QString here = sysinfo::dmiClean(QStringLiteral("product_version")).value_or(QString());
        QString text = QStringLiteral("<b>%1</b><br>made %2 on %3 (%4, BIOS %5)<br><br>%6 user file(s) → ~/.config/legion-power-manager, ~/.config/ryzen-curve-optimizer<br>"
                                      "%7 system file(s) → presets, boot profiles, network-guard rules, NVIDIA profiles, calibration<br><br>"
                                      "Files in the backup replace the current ones; anything that is not in the backup stays. "
                                      "The current configuration is saved first (…-pre-restore.tar.gz). The system profile is not restored.")
                           .arg(QFileInfo(archive).fileName().toHtmlEscaped(), str(m, "created").left(16).replace(QLatin1Char('T'), QLatin1Char(' ')),
                                str(m, "host").toHtmlEscaped(), str(mach, "product").toHtmlEscaped(), str(mach, "bios").toHtmlEscaped())
                           .arg(num(c, "user_config")).arg(num(c, "system"));
        if (!str(mach, "product").isEmpty() && !here.isEmpty() && str(mach, "product") != here)
            text += QStringLiteral("<br><br><span style='color:%1'><b>This backup comes from another model (%2).</b> Firmware limits, "
                                   "curves and calibration from it may not suit this machine.</span>").arg(QLatin1String(theme::WARN), str(mach, "product").toHtmlEscaped());
        QMessageBox box(QMessageBox::Question, QStringLiteral("Restore configuration"), text, QMessageBox::Cancel, this);
        box.setTextFormat(Qt::RichText);
        box.addButton(QStringLiteral("Restore"), QMessageBox::AcceptRole);
        if (box.exec() == QMessageBox::Cancel) { status_->setText(QStringLiteral("Idle.")); return; }
        const bool hasRoot = num(c, "system") > 0;
        // 1. safety copy of what is here now, 2. root part (password; nothing changed if dismissed), 3. user part.
        run({{"op", "config_backup"}, {"dest_dir", QFileInfo(archive).absolutePath()}, {"include_profile", false},
             {"tag", "pre-restore"}, {"lpm_version", QStringLiteral(LPM_VERSION)}},
            false, QStringLiteral("Saving the current configuration…"), [this, archive, hasRoot](const QJsonObject &pre) {
                if (pre.value("ok").toBool()) log(QStringLiteral("current configuration saved as ") + QFileInfo(str(pre, "archive")).fileName());
                else if (str(pre, "error") != QLatin1String("nothing to back up yet")) {
                    finished(QStringLiteral("Restore stopped — the current configuration could not be saved first: ") + str(pre, "error"), false);
                    return;
                }
                auto userPart = [this, archive](qint64 rootFiles) {
                    run({{"op", "config_restore_user"}, {"archive", archive}}, false, QStringLiteral("Restoring user files…"),
                        [this, rootFiles](const QJsonObject &u) {
                            if (!u.value("ok").toBool()) { finished(QStringLiteral("Restoring the user files failed: ") + str(u, "error"), false); return; }
                            finished(QStringLiteral("Restored %1 user and %2 system file(s).").arg(num(u, "files")).arg(rootFiles), true);
                            QMessageBox ask(QMessageBox::Information, QStringLiteral("Configuration restored"),
                                            QStringLiteral("Legion Power Manager has to restart to load the restored configuration."),
                                            QMessageBox::NoButton, this);
                            ask.addButton(QStringLiteral("Later"), QMessageBox::RejectRole);
                            auto *now = ask.addButton(QStringLiteral("Restart now"), QMessageBox::AcceptRole);
                            ask.exec();
                            if (ask.clickedButton() == now) theme::requestRestart();
                        });
                };
                if (!hasRoot) { userPart(0); return; }
                run({{"op", "config_restore_root"}, {"archive", archive}}, true, QStringLiteral("Restoring system files (waiting for the password)…"),
                    [this, userPart](const QJsonObject &s) {
                        if (!s.value("ok").toBool()) { finished(QStringLiteral("Nothing was restored: ") + str(s, "error"), false); return; }
                        userPart(num(s, "files"));
                    });
            });
    });
}

// ── system image ────────────────────────────────────────────────────────────

void BackupPage::systemBackup() {
    saveSettings();
    const QString dir = sysDir_->text().trimmed();
    if (!QDir::isAbsolutePath(dir) || !QFileInfo(dir).isDir()) { finished(QStringLiteral("The image folder does not exist: ") + dir, false); return; }
    QJsonArray ex;
    for (const QString &l : excludes_->toPlainText().split(QLatin1Char('\n'), Qt::SkipEmptyParts))
        if (!l.trimmed().isEmpty()) ex.append(l.trimmed());
    log_->clear();
    run({{"op", "system_backup"}, {"dest_dir", dir}, {"compressor", comp_->currentData().toString()}, {"level", level_->value()},
         {"include_home", home_->isChecked()}, {"excludes", ex}, {"low_priority", low_->isChecked()},
         {"verify", verify_->isChecked()}, {"keep", keep_->value()}},
        true, QStringLiteral("Waiting for the administrator password…"), [this](const QJsonObject &r) {
            for (const QJsonValue &m : r.value("messages").toArray()) log(m.toString(), theme::WARN);
            if (!r.value("ok").toBool()) { finished(QStringLiteral("System image failed: ") + str(r, "error"), false); return; }
            for (const QJsonValue &d : r.value("removed").toArray()) log(QStringLiteral("removed old image ") + d.toString());
            QString s = QStringLiteral("%1 written: %2, %L3 entries, %4").arg(QFileInfo(str(r, "archive")).fileName(), human(num(r, "bytes")))
                            .arg(num(r, "files")).arg(clock(num(r, "elapsed")));
            if (r.value("verified").toBool()) s += QStringLiteral(", verified");
            if (r.value("degraded").toBool()) s += QStringLiteral(" — tar reported errors on some files (%1 message(s) below)").arg(num(r, "message_count"));
            else if (num(r, "message_count") > 0) s += QStringLiteral(" — %1 tar note(s), e.g. files that changed while being read").arg(num(r, "message_count"));
            finished(s, !r.value("degraded").toBool());
            select(str(r, "archive"));
        });
}

void BackupPage::verifySelected() {
    const QString archive = selected();
    if (archive.isEmpty()) { finished(QStringLiteral("Select an archive in the list first."), false); return; }
    log_->clear();
    // A system image is 0600 root: reading it back needs the password too.
    run({{"op", "verify"}, {"archive", archive}}, !QFileInfo(archive).isReadable(), QStringLiteral("Verifying…"), [this, archive](const QJsonObject &r) {
        if (!r.value("ok").toBool()) { finished(QStringLiteral("%1 did NOT verify: %2").arg(QFileInfo(archive).fileName(), str(r, "error")), false); return; }
        finished(QStringLiteral("%1 is readable end to end: %L2 entries%3.").arg(QFileInfo(archive).fileName()).arg(num(r, "files"))
                     .arg(r.value("compared").toBool() ? QStringLiteral(", same count as when it was written") : QString()), true);
    });
}

void BackupPage::systemRestore() {
    saveSettings();
    QString kind;
    const QString archive = selected(&kind);
    if (archive.isEmpty() || kind != QLatin1String("system")) {
        finished(QStringLiteral("Select a \"System image\" in the list first (\"Add file…\" lists one from elsewhere)."), false);
        return;
    }
    const QString target = QFileInfo(target_->text().trimmed()).canonicalFilePath();
    if (target.isEmpty() || !QFileInfo(target).isDir()) {
        finished(QStringLiteral("\"Restore to\" must be an existing directory (the mounted new root, or /)."), false);
        return;
    }
    const QString name = QFileInfo(archive).fileName();
    if (target == QLatin1String("/")) {
        bool ok = false;
        const QString typed = QInputDialog::getText(this, QStringLiteral("Restore over the running system"),
            QStringLiteral("%1 will be unpacked over / — every file in the image replaces the one on this system, while it is running.\n"
                           "Files created after the image was made stay. Close other programs first and reboot afterwards.\n\n"
                           "Type RESTORE to continue:").arg(name), QLineEdit::Normal, QString(), &ok);
        if (!ok || typed != QLatin1String("RESTORE")) { status_->setText(QStringLiteral("Restore cancelled.")); return; }
    } else {
        const int n = QDir(target).entryList(QDir::AllEntries | QDir::NoDotAndDotDot | QDir::Hidden | QDir::System).size();
        if (QMessageBox::warning(this, QStringLiteral("Restore system image"),
                QStringLiteral("Unpack %1 into %2?\n\n%3").arg(name, target, n ? QStringLiteral("The directory is not empty (%1 entries): files in the image replace existing ones.").arg(n)
                                                                                 : QStringLiteral("The directory is empty.")),
                QMessageBox::Yes | QMessageBox::Cancel, QMessageBox::Cancel) != QMessageBox::Yes) { status_->setText(QStringLiteral("Restore cancelled.")); return; }
    }
    log_->clear();
    run({{"op", "system_restore"}, {"archive", archive}, {"target", target}, {"confirm", target}, {"low_priority", false}},
        true, QStringLiteral("Waiting for the administrator password…"), [this, target, name](const QJsonObject &r) {
            for (const QJsonValue &m : r.value("messages").toArray()) log(m.toString(), theme::WARN);
            if (!r.value("ok").toBool()) { finished(QStringLiteral("Restore failed: ") + str(r, "error"), false); return; }
            QStringList made;
            for (const QJsonValue &d : r.value("created_dirs").toArray()) made << d.toString();
            if (!made.isEmpty()) log(QStringLiteral("created missing mount points: ") + made.join(QLatin1Char(' ')));
            if (target != QLatin1String("/"))
                log(QStringLiteral("new disk? check the UUIDs in %1/etc/fstab and reinstall the boot loader before rebooting.").arg(target));
            QString s = QStringLiteral("%1 restored into %2: %L3 entries, %4").arg(name, target).arg(num(r, "files")).arg(clock(num(r, "elapsed")));
            if (r.value("degraded").toBool()) s += QStringLiteral(" — tar reported errors on some files (%1 message(s) below)").arg(num(r, "message_count"));
            finished(s, !r.value("degraded").toBool());
        });
}
