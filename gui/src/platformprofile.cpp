#include "platformprofile.h"
#include <QDir>
#include <QCoreApplication>
#include <QElapsedTimer>
#include <QFile>
#include <QSocketNotifier>
#include <QTimer>
#include <fcntl.h>
#include <unistd.h>
#include <algorithm>
#include <cerrno>
#include <cstring>
#include <memory>

namespace pp {

static const QString CLASS_DIR = QStringLiteral("/sys/class/platform-profile");
static const QString LEGACY_PROFILE = QStringLiteral("/sys/firmware/acpi/platform_profile");
static const QString LEGACY_CHOICES = QStringLiteral("/sys/firmware/acpi/platform_profile_choices");

// The Live box, the device rows, Scenes and the GPU tabs read a few hundred
// sysfs attributes per poll. QFile::read(64 KiB) allocated (and QFile's own
// read buffer added another) 64 KiB per attribute for what is almost always
// under 32 bytes; this is one open() and read() into the stack, and only a
// file that actually fills the first page grows a heap buffer.
std::optional<QByteArray> readRaw(const QString &path) {
    const int fd = ::open(QFile::encodeName(path).constData(), O_RDONLY | O_CLOEXEC | O_NOCTTY);
    if (fd < 0) return std::nullopt;
    constexpr qsizetype CAP = 64 * 1024;
    char stack[4096];
    QByteArray big;
    qsizetype got = 0;
    for (;;) {
        char *dst = big.isNull() ? stack + got : big.data() + got;
        const qsizetype room = (big.isNull() ? qsizetype(sizeof stack) : big.size()) - got;
        const ssize_t n = ::read(fd, dst, size_t(room));
        if (n < 0) { if (errno == EINTR) continue; got = 0; big.clear(); break; }  // read error: empty, as before
        if (n == 0) break;
        got += n;
        if (got < (big.isNull() ? qsizetype(sizeof stack) : big.size())) continue;  // short read: sysfs may still have more
        if (got >= CAP) break;
        if (big.isNull()) { big.resize(std::min<qsizetype>(CAP, 16 * 1024)); std::memcpy(big.data(), stack, size_t(got)); }
        else big.resize(std::min<qsizetype>(CAP, big.size() * 2));
    }
    ::close(fd);
    if (big.isNull()) return QByteArray(stack, got);
    big.truncate(got);
    return big;
}

std::optional<QString> readText(const QString &path) {
    const auto raw = readRaw(path);
    if (!raw) return std::nullopt;
    return QString::fromUtf8(*raw).trimmed();
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
    auto rate = std::make_shared<std::pair<QElapsedTimer, int>>();
    rate->first.start();
    connect(n, &QSocketNotifier::activated, this, [this, n, fd, rate] {
        char b[64];
        ::lseek(fd, 0, SEEK_SET);
        if (::read(fd, b, sizeof b) < 0) {  // re-arm impossible: kernfs would report POLLPRI forever
            n->setEnabled(false);           // the safety poll keeps us in sync
            check();
            return;
        }
        if (rate->first.elapsed() > 1000) { rate->first.restart(); rate->second = 0; }
        if (++rate->second > 10) {          // POLLPRI not clearing: pause, re-arm later
            n->setEnabled(false);
            QTimer::singleShot(10000, n, [n, fd, rate] {
                char c[64];
                ::lseek(fd, 0, SEEK_SET);
                if (::read(fd, c, sizeof c) >= 0) { rate->first.restart(); rate->second = 0; n->setEnabled(true); }
            });
        }
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
