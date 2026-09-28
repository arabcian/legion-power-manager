#include "nvregdialog.h"

#include "privileged.h"
#include "theme.h"

#include <QComboBox>
#include <QDialogButtonBox>
#include <QGridLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QProcess>
#include <QPushButton>
#include <QScrollArea>
#include <QVBoxLayout>

static QString helperPath() { return privileged::helperPath(QStringLiteral("tune-helper")); }

static QLabel *dim(const QString &t, const char *color = theme::MUTED) {
    auto *l = new QLabel(t);
    l->setWordWrap(true);
    l->setStyleSheet(QStringLiteral("color:%1").arg(QString::fromLatin1(color)));
    return l;
}

NvRegDialog::NvRegDialog(QWidget *parent) : QDialog(parent) {
    setWindowTitle(QStringLiteral("NVIDIA driver options"));
    resize(900, 640);
    auto *root = new QVBoxLayout(this);

    info_ = dim(QString());
    info_->setTextFormat(Qt::RichText);
    root->addWidget(info_);

    auto *area = new QScrollArea;
    area->setWidgetResizable(true);
    auto *inner = new QWidget;
    grid_ = new QGridLayout(inner);
    grid_->setHorizontalSpacing(12);
    grid_->setVerticalSpacing(2);
    area->setWidget(inner);
    root->addWidget(area, 1);

    status_ = dim(QString());
    root->addWidget(status_);

    auto *bb = new QDialogButtonBox;
    apply_ = bb->addButton(QStringLiteral("Apply (next boot)"), QDialogButtonBox::AcceptRole);
    apply_->setObjectName(QStringLiteral("btnAccent"));
    auto *close = bb->addButton(QDialogButtonBox::Close);
    connect(apply_, &QPushButton::clicked, this, &NvRegDialog::apply);
    connect(close, &QPushButton::clicked, this, &QDialog::reject);
    root->addWidget(bb);

    load();
}

void NvRegDialog::load() {
    auto *p = new QProcess(this);
    connect(p, &QProcess::finished, this, [this, p](int, QProcess::ExitStatus) {
        const QByteArray out = p->readAllStandardOutput().trimmed();
        p->deleteLater();
        const QJsonObject d = QJsonDocument::fromJson(out.mid(out.lastIndexOf('\n') + 1)).object();
        if (d.value(QStringLiteral("ok")).toBool()) build(d);
        else status_->setText(QStringLiteral("Cannot read driver options: ") + d.value(QStringLiteral("error")).toString());
    });
    p->start(helperPath(), {});
    p->write(R"({"op":"nvreg_describe"})");
    p->closeWriteChannel();
}

void NvRegDialog::build(const QJsonObject &d) {
    while (QLayoutItem *it = grid_->takeAt(0)) { delete it->widget(); delete it; }
    rows_.clear();
    const QString file = d.value(QStringLiteral("file")).toString();
    info_->setText(QStringLiteral(
        "Values are written to <b>%1</b>, which loads after other modprobe.d files, so it wins over them "
        "(the kernel command line still wins over everything). They take effect the next time the module "
        "loads — reboot. If your initramfs carries the nvidia modules, rebuild it (e.g. <tt>dracut --force</tt>). "
        "Leave a row on <i>not set</i> to keep the driver default.%2")
        .arg(file, d.value(QStringLiteral("loaded")).toBool() ? QString()
                                                               : QStringLiteral("<br><b>The nvidia module is not loaded:</b> no running values.")));

    int r = 0;
    const char *heads[] = {"Option", "Running", "Also set in", "LPM value"};
    for (int c = 0; c < 4; ++c) grid_->addWidget(new QLabel(QStringLiteral("<b>%1</b>").arg(QString::fromLatin1(heads[c]))), r, c);
    ++r;
    for (const QJsonValue &v : d.value(QStringLiteral("params")).toArray()) {
        const QJsonObject o = v.toObject();
        Row row;
        row.name = o.value(QStringLiteral("name")).toString();
        row.kind = o.value(QStringLiteral("kind")).toString();
        const QString module = o.value(QStringLiteral("module")).toString();
        const QString lpm = o.value(QStringLiteral("lpm")).toString();

        auto *name = new QLabel(QStringLiteral("%1 <span style='color:%2'>(%3)</span>")
                                    .arg(row.name, QString::fromLatin1(theme::MUTED), module));
        name->setToolTip(o.value(QStringLiteral("desc")).toString());
        grid_->addWidget(name, r, 0);
        row.running = new QLabel(o.value(QStringLiteral("running")).isString() ? o.value(QStringLiteral("running")).toString()
                                                                                 : QStringLiteral("—"));
        grid_->addWidget(row.running, r, 1);
        QStringList others;
        for (const QJsonValue &s : o.value(QStringLiteral("sources")).toArray()) {
            const QJsonObject so = s.toObject();
            const QString src = so.value(QStringLiteral("source")).toString();
            if (src == file) continue;
            others << QStringLiteral("%1 = %2").arg(src.section(QLatin1Char('/'), -1), so.value(QStringLiteral("value")).toString());
        }
        row.others = dim(others.join(QStringLiteral("\n")), others.isEmpty() ? theme::MUTED : theme::WARN);
        grid_->addWidget(row.others, r, 2);

        if (row.kind == QLatin1String("bool") || row.kind == QLatin1String("choice")) {
            row.combo = new QComboBox;
            row.combo->addItem(QStringLiteral("not set"), QString());
            if (row.kind == QLatin1String("bool")) {
                row.combo->addItem(QStringLiteral("0 (off)"), QStringLiteral("0"));
                row.combo->addItem(QStringLiteral("1 (on)"), QStringLiteral("1"));
            } else {
                for (const QJsonValue &c : o.value(QStringLiteral("options")).toArray()) {
                    const QJsonArray a = c.toArray();
                    row.combo->addItem(a.at(0).toString() + QStringLiteral(" — ") + a.at(1).toString(), a.at(0).toString());
                }
            }
            const int i = row.combo->findData(lpm);
            row.combo->setCurrentIndex(lpm.isEmpty() || i < 0 ? 0 : i);
            grid_->addWidget(row.combo, r, 3);
        } else {
            row.edit = new QLineEdit(lpm);
            row.edit->setPlaceholderText(QStringLiteral("not set"));
            if (row.kind == QLatin1String("int")) {
                const QJsonArray mm = o.value(QStringLiteral("options")).toArray();
                row.edit->setPlaceholderText(QStringLiteral("not set (%1–%2)").arg(mm.at(0).toInteger()).arg(mm.at(1).toInteger()));
            }
            grid_->addWidget(row.edit, r, 3);
        }
        ++r;
        grid_->addWidget(dim(o.value(QStringLiteral("desc")).toString()), r, 0, 1, 4);
        ++r;
        rows_ << row;
    }
    grid_->setColumnStretch(0, 3);
    grid_->setColumnStretch(2, 2);
    grid_->setColumnStretch(3, 2);
    grid_->setRowStretch(r, 1);
}

void NvRegDialog::apply() {
    QJsonObject values;
    for (const Row &row : rows_) {
        const QString v = row.combo ? row.combo->currentData().toString() : row.edit->text().trimmed();
        if (!v.isEmpty()) values.insert(row.name, v);
    }
    if (values.contains(QStringLiteral("NVreg_EnableGpuFirmware")) && values.value(QStringLiteral("NVreg_EnableGpuFirmware")).toString() == QLatin1String("0")
        && QMessageBox::question(this, windowTitle(),
               QStringLiteral("GSP firmware off has no effect on RTX 50 / the open kernel modules, and on other GPUs "
                              "changes how the whole driver runs. Write it anyway?")) != QMessageBox::Yes)
        return;
    apply_->setEnabled(false);
    status_->setText(QStringLiteral("Writing…"));
    privileged::run(helperPath(), QJsonObject{{QStringLiteral("op"), QStringLiteral("nvreg_set")}, {QStringLiteral("values"), values}},
                    this, [this](const privileged::Result &r) {
        apply_->setEnabled(true);
        if (!r.ok()) { status_->setText(QStringLiteral("Not written: ") + r.message()); return; }
        const int n = r.json.value(QStringLiteral("written")).toInt();
        status_->setText(n ? QStringLiteral("%1 option(s) written. Reboot (and rebuild the initramfs if it carries nvidia) to apply.").arg(n)
                           : QStringLiteral("No options set: LPM's file was removed; driver defaults apply after reboot."));
        load();
    });
}
