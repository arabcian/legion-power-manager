#pragma once
// Export / import of everything the user has built, as one JSON file:
// scenes (+ automatic switching), Optimizations presets and game-launch
// settings, Ryzen CO / Intel undervolt profiles and NVIDIA curve profiles.
// For backups, a reinstall, or sharing with other owners of the same model.
#include <QString>
#include <functional>

class MainWindow;
class QWidget;

namespace bundle {

/// Writes the bundle; returns a one-line summary, or empty with `err` set.
QString exportTo(const QString &path, QString *err);

/// Reads, shows a summary (and a machine-mismatch warning), asks how to treat
/// names that already exist, writes the user files and — through
/// nvcurve-root-helper, one pkexec per profile — the NVIDIA profiles.
/// `done` gets the result line (empty when the user cancelled).
void importFrom(const QString &path, QWidget *parent, std::function<void(const QString &)> done);

} // namespace bundle
