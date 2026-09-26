#pragma once
// Placeholder page that builds its real widget on first show. For tabs that
// are expensive to construct and rarely opened: nothing of theirs runs at
// login while the app sits in the tray.
#include <QShowEvent>
#include <QVBoxLayout>
#include <QWidget>
#include <functional>

class LazyWidget : public QWidget {
public:
    explicit LazyWidget(std::function<QWidget *()> make, QWidget *parent = nullptr)
        : QWidget(parent), make_(std::move(make)) {
        auto *l = new QVBoxLayout(this);
        l->setContentsMargins(0, 0, 0, 0);
    }

protected:
    void showEvent(QShowEvent *e) override {
        if (make_) {
            QWidget *w = make_();
            make_ = nullptr;
            layout()->addWidget(w);
        }
        QWidget::showEvent(e);
    }

private:
    std::function<QWidget *()> make_;
};
