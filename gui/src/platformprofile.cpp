#include "platformprofile.h"
#include <QDir>
#include <QCoreApplication>
#include <QFile>
#include <QSocketNotifier>
#include <QTimer>
#include <fcntl.h>
#include <unistd.h>
#include <algorithm>

namespace pp {

static const QString CLASS_DIR = QStringLiteral("/sys/class/platform-profile");
static const QString LEGACY_PROFILE = QStringLiteral("/sys/firmware/acpi/platform_profile");
static const QString LEGACY_CHOICES = QStringLiteral("/sys/firmware/acpi/platform_profile_choices");

std::optional<QString> readText(const QString &path) {
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly)) return std::nullopt;
    return QString::fromUtf8(f.read(64 * 1024)).trimmed();
}

QString Handler::path() const { return CLASS_DIR + '/' + node + QStringLiteral("/profile"); }

bool Handler::supportsCustom() const {
    if (choices.contains(QStringLiteral("custom"))) return true;
    const QString l = name.toLower();
    for (const char *h : {"lenovo", "gamezone", "legion", "ideapad"})
        if (l.contains(QLatin1String(h))) return true;
    return false;
}

std::optional<QString> Handler::current() const { return readText(path()); }

QList<Handler> handlers() {
    QList<Handler> out;
    const QStringList nodes = QDir(CLASS_DIR).entryList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::System, QDir::Name);
    for (const QString &n : nodes) {
        auto name = readText(CLASS_DIR + '/' + n + QStringLiteral("/name"));
        auto choices = readText(CLASS_DIR + '/' + n + QStringLiteral("/choices"));
        if (!name || !choices) continue;
        out.append({n, *name, choices->split(' ', Qt::SkipEmptyParts)});
    }
    return out;
}

std::optional<Handler> primaryHandler() {
    auto hs = handlers();
    if (hs.isEmpty()) return std::nullopt;
    // Prefer Custom-capable, then richer choice list, then lowest node.
    std::sort(hs.begin(), hs.end(), [](const Handler &a, const Handler &b) {
        if (a.supportsCustom() != b.supportsCustom()) return a.supportsCustom();
        if (a.choices.size() != b.choices.size()) return a.choices.size() > b.choices.size();
        return a.node < b.node;
    });
    return hs.first();
}

bool available() { return !handlers().isEmpty() || readText(LEGACY_PROFILE).has_value(); }

std::optional<QString> currentProfile(const std::optional<Handler> &h) {
    // Per-handler first: the legacy file says "custom" whenever handlers merely disagree.
    if (h) if (auto v = h->current(); v && !v->isEmpty()) return v;
    return readText(LEGACY_PROFILE);
}

QStringList offeredProfiles(const std::optional<Handler> &h) {
    QStringList names = h ? h->choices : readText(LEGACY_CHOICES).value_or(QString()).split(' ', Qt::SkipEmptyParts);
    if (h && h->supportsCustom()) names << QStringLiteral("custom");
    QStringList out;
    for (const QString &p : VALID_PROFILES) if (names.contains(p)) out << p;
    return out;
}

// ── change notifications ────────────────────────────────────────────────────

static constexpr int SAFETY_POLL_MS = 30000, FALLBACK_POLL_MS = 2500;

Watcher &Watcher::instance() {
    // Parented to the application: its notifiers go away before the event dispatcher does.
    static Watcher *w = new Watcher(QCoreApplication::instance());
    return *w;
}

Watcher::Watcher(QObject *parent) : QObject(parent), handler_(primaryHandler()) {
    current_ = currentProfile(handler_);
    if (handler_) watch(handler_->path());
    watch(LEGACY_PROFILE);
    auto *t = new QTimer(this);
    t->setTimerType(Qt::VeryCoarseTimer);  // may be batched with other wake-ups
    connect(t, &QTimer::timeout, this, &Watcher::check);
    t->start(fds_.isEmpty() ? FALLBACK_POLL_MS : SAFETY_POLL_MS);
    connect(this, &QObject::destroyed, [fds = fds_] { for (int fd : fds) ::close(fd); });
}

void Watcher::watch(const QString &path) {
    const int fd = ::open(QFile::encodeName(path).constData(), O_RDONLY | O_CLOEXEC);
    if (fd < 0) return;
    char buf[64];
    if (::read(fd, buf, sizeof buf) < 0) { ::close(fd); return; }  // a sysfs attribute is armed by a read
    fds_ << fd;
    auto *n = new QSocketNotifier(fd, QSocketNotifier::Exception, this);  // POLLPRI = sysfs_notify
    connect(n, &QSocketNotifier::activated, this, [this, fd] {
        char b[64];
        ::lseek(fd, 0, SEEK_SET);
        [[maybe_unused]] const auto r = ::read(fd, b, sizeof b);  // re-arm
        check();
    });
}

void Watcher::check() {
    const auto now = currentProfile(handler_);
    if (!now || now == current_) return;
    current_ = now;
    Q_EMIT changed(*now);
}

} // namespace pp
