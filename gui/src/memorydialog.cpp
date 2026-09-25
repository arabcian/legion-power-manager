#include "memorydialog.h"
#include "privileged.h"
#include "theme.h"
#include <QHBoxLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonObject>
#include <QLabel>
#include <QPushButton>
#include <QTableWidget>
#include <QVBoxLayout>
#include <QCheckBox>
#include <QFile>
#include <QFormLayout>
#include <QMessageBox>
#include <QSpinBox>
#include <QScrollArea>
#include <algorithm>
#include <functional>

MemoryDialog::MemoryDialog(const QString &helper, QWidget *parent) : QDialog(parent), helper_(helper) {
    setWindowTitle("Memory timings");
    setAttribute(Qt::WA_DeleteOnClose);
    resize(760, 760);
    auto *v = new QVBoxLayout(this);
    auto *intro = new QLabel("<b>SPD</b>: the JEDEC timings each module is rated for (stored on the module, never "
                             "changed by BIOS tuning). <b>Running</b>: what the memory controller actually uses now, read "
                             "from the AMD UMC through ryzen_smu. Everything here is read-only.");
    intro->setWordWrap(true);
    intro->setTextFormat(Qt::RichText);
    v->addWidget(intro);

    table_ = new QTableWidget;
    table_->setEditTriggers(QAbstractItemView::NoEditTriggers);
    table_->setSelectionMode(QAbstractItemView::NoSelection);
    table_->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
    v->addWidget(table_, 1);

    status_ = new QLabel;
    status_->setWordWrap(true);
    status_->setTextFormat(Qt::RichText);
    v->addWidget(status_);

    auto *h = new QHBoxLayout;
    auto *reload = new QPushButton("Reload");
    auto *edit = new QPushButton("Edit timings…");
    edit->setToolTip("Change BIOS memory timings (Legion Pro 7 16AFR10H only). Applied on next boot.");
    connect(edit, &QPushButton::clicked, this, &MemoryDialog::openEditor);
    // Editor is mapped for one model only; hide it elsewhere (the helper refuses too).
    auto dmi = [](const char *n) {
        QFile f(QStringLiteral("/sys/class/dmi/id/") + QLatin1String(n));
        return f.open(QIODevice::ReadOnly) ? QString::fromLatin1(f.readAll()).trimmed() : QString();
    };
    edit->setVisible((dmi("product_version") + ' ' + dmi("product_family")).contains(QLatin1String("16AFR10H"))
                     && dmi("bios_version").startsWith(QLatin1String("SMCN")));
    auto *close = new QPushButton("Close");
    h->addWidget(reload);
    h->addWidget(edit);
    h->addStretch(1);
    h->addWidget(close);
    v->addLayout(h);
    connect(reload, &QPushButton::clicked, this, &MemoryDialog::load);
    connect(close, &QPushButton::clicked, this, &QDialog::close);
    load();
}

void MemoryDialog::load() {
    status_->setText("Reading SPD and memory controller…");
    privileged::run(helper_, QJsonObject{{"memory", "all"}}, this, [this](const privileged::Result &r) {
        if (!r.ok()) {
            status_->setText(QStringLiteral("<span style='color:%1'>%2</span>").arg(theme::DANGER, r.message().toHtmlEscaped()));
            return;
        }
        const QJsonArray mods = r.json.value("modules").toArray();
        const QJsonObject umc = r.json.value("umc").toObject();
        const QJsonObject run = umc.value("timings").toObject();
        const bool haveRun = !run.isEmpty();

        // One row per timing. spd: how to show it from a module (empty = no SPD value);
        // live: key in the UMC timings (empty = not read from the controller).
        struct Row { QString label; std::function<QString(const QJsonObject &)> spd; QString live; bool clkCompare = false; };
        auto timing = [](const char *k) {
            return [k](const QJsonObject &m) {
                const QJsonObject t = m.value(k).toObject();
                return QStringLiteral("%1  (%2 ns)").arg(t.value("clk").toInt()).arg(t.value("ns").toDouble(), 0, 'f', 2);
            };
        };
        auto ns = [](const char *k) { return [k](const QJsonObject &m) { return QStringLiteral("%1 ns").arg(m.value(k).toInt()); }; };
        auto none = std::function<QString(const QJsonObject &)>();
        QList<Row> rows = {
            {"Part", [](const QJsonObject &m) { return m.value("part").toString(); }, {}},
            {"Module maker", [](const QJsonObject &m) {
                 const QString n = m.value("manufacturer").toString();
                 return n.isEmpty() ? "ID " + m.value("manufacturer_id").toString() : n; }, {}},
            {"DRAM maker", [](const QJsonObject &m) { const QString n = m.value("dram_manufacturer").toString(); return n.isEmpty() ? QStringLiteral("?") : n; }, {}},
            {"Type", [](const QJsonObject &m) {
                 return QStringLiteral("DDR5 %1, %2 Gbit dies").arg(m.value("form").toString()).arg(m.value("die_density_gbit").toInt()); }, {}},
            {"Speed", [](const QJsonObject &m) {
                 return QStringLiteral("%1 MT/s").arg(m.value("speed_mts").toInt()); }, "__speed"},
            {"tCL", timing("tAA"), "tCL", true}, {"tRCD (rd)", timing("tRCD"), "tRCDRD", true},
            {"tRCD (wr)", none, "tRCDWR"}, {"tRP", timing("tRP"), "tRP", true},
            {"tRAS", timing("tRAS"), "tRAS", true}, {"tRC", timing("tRC"), "tRC", true},
            {"tWR", timing("tWR"), "tWR", true},
            {"tRFC1", ns("tRFC1_ns"), "tRFC1"}, {"tRFC2", ns("tRFC2_ns"), "tRFC2"}, {"tRFCsb", ns("tRFCsb_ns"), {}},
            {"tCWL", none, "tCWL"}, {"tRRD_S", none, "tRRDS"}, {"tRRD_L", none, "tRRDL"}, {"tFAW", none, "tFAW"},
            {"tRTP", none, "tRTP"}, {"tWTR_S", none, "tWTRS"}, {"tWTR_L", none, "tWTRL"},
            {"tRdWr", none, "tRdWr"}, {"tWrRd", none, "tWrRd"},
            {"tRdRd SCL/SC/SD/DD", none, "__rdrd"}, {"tWrWr SCL/SC/SD/DD", none, "__wrwr"},
            {"tREFI", none, "tREFI"}, {"tMOD / tMRD", none, "__mod"}, {"tMODPDA / tMRDPDA", none, "__pda"},
            {"tSTAG", none, "tSTAG"}, {"tPHY WRD/RDL/WRL", none, "__phy"},
            {"EXPO profile", [](const QJsonObject &m) { return m.value("expo").toBool() ? QStringLiteral("present") : QStringLiteral("none"); }, {}},
        };
        auto iv = [&](const char *k) { return QString::number(run.value(k).toInt()); };
        auto liveText = [&](const QString &key) -> QString {
            if (key.isEmpty() || !haveRun) return QString();
            if (key == "__speed") return QStringLiteral("%1 MT/s").arg(umc.value("speed_mts").toInt());
            if (key == "__rdrd") return iv("tRdRdScl") + " / " + iv("tRdRdSc") + " / " + iv("tRdRdSd") + " / " + iv("tRdRdDd");
            if (key == "__wrwr") return iv("tWrWrScl") + " / " + iv("tWrWrSc") + " / " + iv("tWrWrSd") + " / " + iv("tWrWrDd");
            if (key == "__mod") return iv("tMOD") + " / " + iv("tMRD");
            if (key == "__pda") return iv("tMODPDA") + " / " + iv("tMRDPDA");
            if (key == "__phy") return iv("tPHYWRD") + " / " + iv("tPHYRDL") + " / " + iv("tPHYWRL");
            if (key == "tRFC1" || key == "tRFC2")
                return QStringLiteral("%1  (%2 ns)").arg(run.value(key).toInt()).arg(run.value(key + "_ns").toDouble(), 0, 'f', 0);
            return QString::number(run.value(key).toInt());
        };
        // Drop rows with nothing to show in any column.
        rows.erase(std::remove_if(rows.begin(), rows.end(), [&](const Row &r) {
            return (!r.spd || mods.isEmpty()) && liveText(r.live).isEmpty(); }), rows.end());

        const int liveCol = mods.size();
        table_->clear();
        table_->setRowCount(rows.size());
        table_->setColumnCount(mods.size() + (haveRun ? 1 : 0));
        QStringList hdr;
        for (const auto &m : mods) hdr << "SPD " + m.toObject().value("slot").toString();
        if (haveRun) hdr << "Running";
        table_->setHorizontalHeaderLabels(hdr);
        QStringList vh;
        const QColor tightened(theme::ACCENT), dim(theme::MUTED);
        for (int i = 0; i < rows.size(); ++i) {
            vh << rows[i].label;
            for (int c = 0; c < mods.size(); ++c) {
                auto *it = new QTableWidgetItem(rows[i].spd ? rows[i].spd(mods[c].toObject()) : QStringLiteral("—"));
                if (!rows[i].spd) it->setForeground(dim);
                table_->setItem(i, c, it);
            }
            if (haveRun) {
                const QString lt = liveText(rows[i].live);
                auto *it = new QTableWidgetItem(lt.isEmpty() ? QStringLiteral("—") : lt);
                if (lt.isEmpty()) it->setForeground(dim);
                // Highlight timings the BIOS runs tighter than the module's JEDEC profile.
                if (rows[i].clkCompare && !mods.isEmpty() && !lt.isEmpty()) {
                    const char *spdKey = rows[i].label == "tCL" ? "tAA" : rows[i].label == "tRCD (rd)" ? "tRCD" : nullptr;
                    const QString k = spdKey ? QString::fromLatin1(spdKey) : rows[i].label;
                    const int spdClk = mods[0].toObject().value(k).toObject().value("clk").toInt();
                    if (spdClk > 0 && run.value(rows[i].live).toInt() < spdClk) {
                        it->setForeground(tightened);
                        it->setToolTip(QStringLiteral("Tighter than the module's JEDEC %1").arg(spdClk));
                    }
                }
                table_->setItem(i, liveCol, it);
            }
        }
        table_->setVerticalHeaderLabels(vh);

        QStringList notes;
        for (const auto &x : r.json.value("errors").toArray()) notes << x.toString();
        if (r.json.contains("spd_error")) notes << "SPD: " + r.json.value("spd_error").toString();
        if (r.json.contains("umc_error")) notes << "Running: " + r.json.value("umc_error").toString();
        if (haveRun && umc.value("channels_match").isBool() && !umc.value("channels_match").toBool())
            notes << "Channel 1 runs different timings from channel 0 (showing channel 0).";
        status_->setText(notes.isEmpty()
            ? QStringLiteral("<span style='color:%1'>%2 module(s) read%3. Orange = tighter than the module's JEDEC profile.</span>")
                  .arg(theme::OK).arg(mods.size()).arg(haveRun ? QStringLiteral(", live timings from the memory controller") : QString())
            : QStringLiteral("<span style='color:%1'>%2</span>").arg(theme::WARN, notes.join("<br>").toHtmlEscaped().replace("&lt;br&gt;", "<br>")));
    });
}

void MemoryDialog::openEditor() {
    privileged::run(helper_, QJsonObject{{"memory", "aod_get"}}, this, [this](const privileged::Result &r) {
        if (!r.ok()) {
            QMessageBox::information(this, "Edit timings", "Editing is not available here:\n\n" + r.message());
            return;
        }
        auto *dlg = new QDialog(this);
        dlg->setAttribute(Qt::WA_DeleteOnClose);
        dlg->setWindowTitle("Edit memory timings");
        dlg->resize(460, 680);
        auto *v = new QVBoxLayout(dlg);
        auto *warn = new QLabel(QStringLiteral(
            "<span style='color:%1'><b>Warning — memory overclocking.</b></span> These values are written to the BIOS "
            "setup variable and applied on the <b>next boot</b>. Unstable timings can prevent the machine from booting, "
            "corrupt data, or damage hardware; this is not covered by warranty and you do it at your own risk.<br><br>"
            "If it does not boot: hold the power button 8–15 s to restore default overclocking settings. A backup of the "
            "variable is saved in /var/lib/legion-power-manager before every write.<br>"
            "<b>AMD Variable Protection must be disabled in the BIOS</b>, or the change is ignored.<br>"
            "Writing asks for the administrator password every time.").arg(theme::DANGER));
        warn->setWordWrap(true);
        warn->setTextFormat(Qt::RichText);
        v->addWidget(warn);
        if (r.json.value("protected").toBool()) {
            auto *pl = new QLabel(QStringLiteral("<span style='color:%1'><b>AMD Variable Protection is ON</b> — the firmware "
                "will refuse the write. Disable it in the BIOS advanced menu first; it cannot be changed from the OS.</span>")
                .arg(theme::WARN));
            pl->setWordWrap(true);
            pl->setTextFormat(Qt::RichText);
            v->addWidget(pl);
        }
        auto *area = new QScrollArea;
        area->setWidgetResizable(true);
        auto *form = new QWidget;
        auto *fl = new QFormLayout(form);
        QList<QPair<QString, QSpinBox *>> spins;
        QHash<QString, int> orig;
        for (const auto &x : r.json.value("fields").toArray()) {
            const QJsonObject f = x.toObject();
            auto *sp = new QSpinBox;
            sp->setRange(f.value("min").toInt(), f.value("max").toInt());
            sp->setValue(f.value("value").toInt());
            if (f.value("name").toString() == "tCL") sp->setSingleStep(2);
            if (!f.value("manual").toBool()) sp->setToolTip("Currently Auto in BIOS; setting it makes it Manual.");
            fl->addRow(f.value("name").toString() + (f.value("manual").toBool() ? "" : "  (auto)"), sp);
            spins << qMakePair(f.value("name").toString(), sp);
            orig[f.value("name").toString()] = f.value("value").toInt();
        }
        area->setWidget(form);
        v->addWidget(area, 1);
        auto *ack = new QCheckBox("I understand the risks");
        v->addWidget(ack);
        auto *h = new QHBoxLayout;
        auto *apply = new QPushButton("Write to BIOS");
        apply->setObjectName("btnDanger");
        apply->setEnabled(false);
        auto *cancel = new QPushButton("Cancel");
        // Undo of the last write: puts the backup taken before it back.
        if (const QString bk = r.json.value("backup").toString(); !bk.isEmpty()) {
            auto *restore = new QPushButton("Restore previous…");
            restore->setToolTip("Write back the variable as it was before the last change (" + bk + ").\n"
                                "The current state is backed up first. Takes effect on the next boot.");
            h->addWidget(restore);
            connect(restore, &QPushButton::clicked, dlg, [dlg, restore, bk] {
                if (QMessageBox::question(dlg, "Restore timings",
                        "Write back the BIOS timing variable saved in\n" + bk + "\n\nIt takes effect on the next boot.",
                        QMessageBox::Yes | QMessageBox::No, QMessageBox::No) != QMessageBox::Yes) return;
                restore->setEnabled(false);
                privileged::run(privileged::helperPath(privileged::FIRMWARE_HELPER), QJsonObject{{"op", "aod_restore"}}, dlg,
                                [dlg, restore](const privileged::Result &w) {
                    if (w.ok()) {
                        QMessageBox::information(dlg, "Restore timings", w.json.value("changed").toBool()
                            ? QStringLiteral("Restored. Reboot to apply.")
                            : QStringLiteral("The variable already matches that backup — nothing changed."));
                        dlg->close();
                    } else {
                        QMessageBox::warning(dlg, "Restore timings", w.message());
                        restore->setEnabled(true);
                    }
                }, privileged::FIRMWARE_TIMEOUT_MS);
            });
        }
        h->addStretch(1);
        h->addWidget(apply);
        h->addWidget(cancel);
        v->addLayout(h);
        connect(ack, &QCheckBox::toggled, apply, &QPushButton::setEnabled);
        connect(cancel, &QPushButton::clicked, dlg, &QDialog::close);
        connect(apply, &QPushButton::clicked, dlg, [dlg, spins, orig, apply] {
            QJsonObject vals;
            for (const auto &[n, sp] : spins)
                if (sp->value() != orig.value(n)) vals[n] = sp->value();
            if (vals.isEmpty()) { dlg->close(); return; }
            apply->setEnabled(false);
            // BIOS variable write: legion-firmware-helper asks for the password every time.
            privileged::run(privileged::helperPath(privileged::FIRMWARE_HELPER), QJsonObject{{"op", "aod_set"}, {"values", vals}}, dlg,
                            [dlg, apply](const privileged::Result &w) {
                if (w.ok()) {
                    QMessageBox::information(dlg, "Edit timings", w.json.value("changed").toBool()
                        ? "Written. Reboot to apply, then check the Running column.\n\nBackup: " + w.json.value("backup").toString()
                        : QStringLiteral("Nothing changed."));
                    dlg->close();
                } else {
                    QMessageBox::warning(dlg, "Edit timings", w.message());
                    apply->setEnabled(true);
                }
            }, privileged::FIRMWARE_TIMEOUT_MS);
        });
        dlg->show();
    });
}
