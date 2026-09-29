// Unit test for the 64-bit editor path of the Optimizations tab:
//   cmake -DLPM_BUILD_TESTS=ON .. && make lpm-gui-tests && ctest
// Plain asserts (no QtTest dependency); runs on the offscreen platform.
#include "../src/int64spinbox.h"

#include <QApplication>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QLineEdit>
#include <cstdio>
#include <cstdlib>
#include <limits>

static int failures = 0;
#define CHECK(c) do { if (!(c)) { std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); ++failures; } } while (0)

int main(int argc, char **argv) {
    qputenv("QT_QPA_PLATFORM", "offscreen");
    QApplication app(argc, argv);
    const qint64 big = 13175391846LL;        // Throughput dirty_bytes on 32166484 kB (wrapped to 290489958 by QSpinBox)
    const qint64 bg = 3293847961LL;          // wrapped / clamped to 8192 by QSpinBox

    Int64SpinBox s;
    s.setRange(8192, 68719476736LL);         // vm.dirty_bytes row range from tune.rs
    qint64 seen = -1;
    QObject::connect(&s, &Int64SpinBox::valueChanged, [&](qint64 v) { seen = v; });
    s.setValue(big);
    CHECK(s.value() == big);
    CHECK(seen == big);
    CHECK(s.text() == QStringLiteral("13175391846"));
    s.setValue(bg);
    CHECK(s.value() == bg && s.value() != 8192);

    // Typed text is committed on editingFinished without truncation.
    s.findChild<QLineEdit *>()->setText(QStringLiteral("17179869184"));
    Q_EMIT s.editingFinished();
    CHECK(s.value() == 17179869184LL);

    // Clamping instead of wrapping.
    s.setValue(1);
    CHECK(s.value() == 8192);
    s.setValue(std::numeric_limits<qint64>::max());
    CHECK(s.value() == 68719476736LL);
    CHECK(Int64SpinBox::stepped(std::numeric_limits<qint64>::max() - 1, 1, 5, 0, std::numeric_limits<qint64>::max()) == std::numeric_limits<qint64>::max());
    CHECK(Int64SpinBox::stepped(10, 1, -20, 0, 100) == 0);

    // Validation: > 64-bit input is rejected, never wrapped.
    QString t = QStringLiteral("99999999999999999999");
    int pos = 0;
    CHECK(s.validate(t, pos) == QValidator::Invalid);
    t = QStringLiteral("100");  // below the minimum but a valid prefix
    CHECK(s.validate(t, pos) == QValidator::Intermediate);
    t = QStringLiteral("13175391846");
    CHECK(s.validate(t, pos) == QValidator::Acceptable);

    // The rest of the path: editor text -> preset JSON -> saved file -> loaded back.
    s.setValue(big);
    const QString editorText = QString::number(s.value());
    QJsonObject preset{{"vm.dirty_bytes", QJsonValue(editorText.toLongLong())}};
    const QJsonObject back = QJsonDocument::fromJson(QJsonDocument(preset).toJson()).object();
    const QJsonValue jv = back.value("vm.dirty_bytes");
    CHECK(jv.toInteger() == big);
    CHECK(QString::number(jv.toVariant().toLongLong()) == QStringLiteral("13175391846"));

    if (failures) { std::fprintf(stderr, "%d failure(s)\n", failures); return 1; }
    std::puts("int64spinbox: all checks passed");
    return 0;
}
