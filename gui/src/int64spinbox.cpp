#include "int64spinbox.h"

#include <QLineEdit>
#include <limits>

Int64SpinBox::Int64SpinBox(QWidget *parent) : QAbstractSpinBox(parent) {
    // Typed text becomes the value on Enter / focus-out (or per keystroke with keyboard tracking).
    connect(this, &QAbstractSpinBox::editingFinished, this, &Int64SpinBox::commitText);
    connect(lineEdit(), &QLineEdit::textEdited, this, [this] { if (keyboardTracking()) commitText(); });
    showValue();
}

void Int64SpinBox::setRange(qint64 min, qint64 max) {
    if (max < min) max = min;
    min_ = min;
    max_ = max;
    setValue(value_);
    showValue();
}

void Int64SpinBox::setValue(qint64 v) {
    v = qBound(min_, v, max_);
    const bool changed = v != value_;
    value_ = v;
    showValue();
    if (changed) Q_EMIT valueChanged(value_);
}

bool Int64SpinBox::parse(const QString &text, qint64 *out) {
    bool ok = false;
    const qint64 n = text.trimmed().toLongLong(&ok);  // fails on overflow instead of wrapping
    if (ok && out) *out = n;
    return ok;
}

qint64 Int64SpinBox::stepped(qint64 v, qint64 step, int steps, qint64 min, qint64 max) {
    qint64 d = 0, r = 0;
    if (__builtin_mul_overflow(step, qint64(steps), &d) || __builtin_add_overflow(v, d, &r))
        return steps > 0 ? max : min;
    return qBound(min, r, max);
}

void Int64SpinBox::stepBy(int steps) {
    commitText();
    setValue(stepped(value_, 1, steps, min_, max_));
    selectAll();
}

QAbstractSpinBox::StepEnabled Int64SpinBox::stepEnabled() const {
    StepEnabled e = StepNone;
    if (value_ < max_) e |= StepUpEnabled;
    if (value_ > min_) e |= StepDownEnabled;
    return e;
}

QValidator::State Int64SpinBox::validate(QString &input, int &) const {
    const QString t = input.trimmed();
    if (t.isEmpty() || (t == QLatin1String("-") && min_ < 0)) return QValidator::Intermediate;
    for (int i = 0; i < t.size(); ++i)
        if (!t[i].isDigit() && !(i == 0 && t[i] == QLatin1Char('-') && min_ < 0)) return QValidator::Invalid;
    qint64 n = 0;
    if (!parse(t, &n)) return QValidator::Invalid;  // beyond 64 bits
    if (n > max_ && n >= 0) return QValidator::Invalid;  // more digits cannot bring it back
    return n >= min_ && n <= max_ ? QValidator::Acceptable : QValidator::Intermediate;
}

void Int64SpinBox::fixup(QString &input) const {
    qint64 n = value_;
    if (parse(input, &n)) n = qBound(min_, n, max_);
    input = QString::number(n);
}

void Int64SpinBox::commitText() {
    qint64 n = 0;
    if (parse(lineEdit()->text(), &n)) setValue(n);
    else showValue();
}

void Int64SpinBox::showValue() {
    const QString t = QString::number(value_);
    if (lineEdit()->text() != t) lineEdit()->setText(t);
}
