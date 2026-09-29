#pragma once
// 64-bit integer spin box. QSpinBox is int (32-bit): values above 2^31-1 such
// as vm.dirty_bytes on a 32 GB machine wrapped (13175391846 -> 290489958) or
// were clamped to the row minimum (8192) and were saved that way.
#include <QAbstractSpinBox>
#include <QtGlobal>

class Int64SpinBox : public QAbstractSpinBox {
    Q_OBJECT
public:
    explicit Int64SpinBox(QWidget *parent = nullptr);

    qint64 value() const { return value_; }
    qint64 minimum() const { return min_; }
    qint64 maximum() const { return max_; }
    void setRange(qint64 min, qint64 max);
    /// Clamped to [minimum, maximum]; emits valueChanged when it changes.
    void setValue(qint64 v);

    void stepBy(int steps) override;
    QValidator::State validate(QString &input, int &pos) const override;
    void fixup(QString &input) const override;

    /// Pure helpers (unit-tested): saturating step and text parsing.
    static qint64 stepped(qint64 v, qint64 step, int steps, qint64 min, qint64 max);
    static bool parse(const QString &text, qint64 *out);

Q_SIGNALS:
    void valueChanged(qint64 value);

protected:
    StepEnabled stepEnabled() const override;

private:
    void commitText();
    void showValue();
    qint64 min_ = 0, max_ = 99, value_ = 0;
};
