// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "pin_chrome.hpp"

#include "pin_label.hpp"


#include <LayerShellQt/Window>

#include <QFontDatabase>
#include <QGuiApplication>
#include <QWindow>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QLocalSocket>
#include <QMouseEvent>
#include <QPainter>
#include <QPainterPath>
#include <QRegion>
#include <QScreen>
#include <QTimer>

namespace vshot {
namespace {

// The right-click menu, in logical pixels.  The same numbers the pin surface's
// own menu uses, so a menu drawn here and one drawn there are the same menu.
constexpr int kMenuRowPaddingY = 5;
constexpr int kMenuPaddingX = 10;
constexpr qreal kMenuRadius = 6.0;
constexpr int kMenuCursorGap = 4;

/// How long a badge stays up.  The same as the surface's own, so a zoom step
/// reported here and a copy reported there read alike.
constexpr int kBadgeMs = 900;

/// Wayland has no "no input here" request: an unset input region means the
/// whole surface is interactive, and Qt sends no request at all for an empty
/// mask, which is exactly that default.  A region parked outside the surface is
/// the portable way to say "click straight through", which is what every
/// label-only surface wants -- the labels say what a pin is, and every gesture
/// belongs to the pin underneath.
const QRegion &clickThroughInputRegion()
{
    static const QRegion region(QRect(-8, -8, 1, 1));
    return region;
}

/// One label drawn: its translucent box, its text, and -- for the HDR tag -- a
/// hairline in the text's own ink, which is what keeps a white tag readable
/// over a white picture.
void paintLabel(QPainter &painter, const QRect &box, const QString &text, const QColor &ink)
{
    painter.save();
    painter.setRenderHint(QPainter::Antialiasing, true);
    painter.setPen(Qt::NoPen);
    painter.setBrush(kLabelBox);
    painter.drawRoundedRect(box, kLabelRadius, kLabelRadius);
    // A thin stroke in the ink, so the box has an edge of its own on a picture
    // of the same colour as the box.
    painter.setBrush(Qt::NoBrush);
    painter.setPen(QPen(ink, 1));
    painter.drawRoundedRect(QRectF(box).adjusted(0.5, 0.5, -0.5, -0.5), kLabelRadius, kLabelRadius);
    painter.setPen(ink);
    painter.drawText(box, Qt::AlignCenter, text);
    painter.restore();
}

/// One menu row drawn: its text, left-aligned, in the font the labels use.
void paintMenuRow(QPainter &painter, const QRect &box, const QString &text, bool hovered)
{
    painter.save();
    if (hovered) {
        painter.setPen(Qt::NoPen);
        painter.setBrush(QColor(255, 255, 255, 40));
        painter.drawRoundedRect(box, kMenuRadius, kMenuRadius);
    }
    painter.setPen(Qt::white);
    painter.drawText(box.adjusted(kMenuPaddingX, 0, -kMenuPaddingX, 0),
                     Qt::AlignLeft | Qt::AlignVCenter, text);
    painter.restore();
}

} // namespace

PinChrome::PinChrome(QScreen *screen)
    : QWidget(nullptr, Qt::Tool | Qt::FramelessWindowHint)
    , screen_(screen)
{
    setAttribute(Qt::WA_TranslucentBackground);
    setAttribute(Qt::WA_ShowWithoutActivating);
    // Nothing here is interactive, and nothing here should be able to take the
    // keyboard from the surface below it.
    setFocusPolicy(Qt::NoFocus);
    setMouseTracking(false);
    badgeTimer_ = new QTimer(this);
    badgeTimer_->setSingleShot(true);
    connect(badgeTimer_, &QTimer::timeout, this, [this] {
        badgeId_ = 0;
        for (Entry &entry : entries_) {
            entry.badge.clear();
        }
        update();
    });
}

bool PinChrome::showLayerSurface()
{
    winId();
    QWindow *window = windowHandle();
    if (window == nullptr) {
        return false;
    }
    auto *layer = LayerShellQt::Window::get(window);
    if (layer == nullptr) {
        return false;
    }
    layer->setLayer(LayerShellQt::Window::LayerOverlay);
    LayerShellQt::Window::Anchors anchors(LayerShellQt::Window::AnchorTop);
    anchors |= LayerShellQt::Window::AnchorBottom;
    anchors |= LayerShellQt::Window::AnchorLeft;
    anchors |= LayerShellQt::Window::AnchorRight;
    layer->setExclusiveZone(-1);
    // Never the keyboard.  Every key a pin answers to -- Space to edit, the
    // arrows, Esc -- belongs to the surface that has the pointer, and this one
    // is only ever decoration over it.
    layer->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityNone);
    layer_ = layer;
    // A scope of its own: the compositor keeps a layer's surfaces in map order
    // per scope as far as this program is concerned, and the daemon maps its
    // pictures first.
    layer->setScope(QStringLiteral("vshot-pin-chrome"));
    layer->setDesiredSize(QSize(0, 0));
    layer->setScreen(screen_);
    surfaceReady_ = true;
    applyMask();
    show();
    applyMask(); // re-issue now that the platform window exists
    return true;
}

void PinChrome::setLabels(const QVector<Label> &labels)
{
    QVector<Entry> next;
    next.reserve(labels.size());
    for (const Label &label : labels) {
        Entry entry;
        entry.label = label;
        // The badge outlives a stack update: a wheel step sends one, and the
        // daemon's own repaint arrives right behind it.
        if (label.id == badgeId_) {
            for (const Entry &was : entries_) {
                if (was.label.id == label.id) {
                    entry.badge = was.badge;
                }
            }
        }
        next.append(entry);
    }
    entries_ = next;
    // The tag follows the pointer, and the pointer is the daemon's to track:
    // the hovered pin is the one the daemon says it is, which arrives as the
    // pin the stack marks active.
    update();
}

void PinChrome::showBadge(quint64 id, const QString &text)
{
    badgeId_ = id;
    for (Entry &entry : entries_) {
        entry.badge = entry.label.id == id ? text : QString();
    }
    badgeTimer_->start(kBadgeMs);
    update();
}

void PinChrome::setMenu(quint64 id, const QPoint &anchor, const QStringList &rows)
{
    menuId_ = id;
    menuRows_ = rows;
    menuHover_ = -1;
    const QPoint origin = screen_ != nullptr ? screen_->geometry().topLeft() : QPoint(0, 0);
    menuRect_ = menuRectFor(anchor - origin, rows);
    if (menuRect_.isEmpty()) {
        // Nothing fits on this output: leaving menuId_ set would put this
        // surface into a state no click could leave.
        dismissMenu();
        return;
    }
    // The keyboard, so Esc reaches the menu -- and so the pointer leaving the
    // surface does not take the menu with it.
    applyKeyboard();
    update();
}

QRect PinChrome::menuRectFor(const QPoint &anchor, const QStringList &rows) const
{
    const QFontMetrics metrics(tagFont());
    int width = 0;
    for (const QString &row : rows) {
        width = std::max(width, metrics.horizontalAdvance(row));
    }
    const int rowHeight = metrics.height() + 2 * kMenuRowPaddingY;
    const QSize box(width + 2 * kMenuPaddingX, rowHeight * static_cast<int>(rows.size()));
    // Down and right of the pointer, the way a menu opens; flipped when that
    // would run off the output, and clamped so a menu at the very edge is still
    // usable.
    QRect rect(anchor + QPoint(kMenuCursorGap, kMenuCursorGap), box);
    if (rect.right() > this->rect().right()) {
        rect.moveLeft(anchor.x() - kMenuCursorGap - box.width());
    }
    if (rect.bottom() > this->rect().bottom()) {
        rect.moveTop(anchor.y() - kMenuCursorGap - box.height());
    }
    rect = rect.intersected(this->rect());
    return rect.width() > 0 && rect.height() > 0 ? rect : QRect();
}

int PinChrome::menuRowAt(const QPoint &local) const
{
    if (menuId_ == 0 || !menuRect_.contains(local) || menuRows_.isEmpty()) {
        return -1;
    }
    const QFontMetrics metrics(tagFont());
    const int rowHeight = metrics.height() + 2 * kMenuRowPaddingY;
    const int row = (local.y() - menuRect_.top()) / rowHeight;
    return row >= 0 && row < menuRows_.size() ? row : -1;
}

void PinChrome::chooseRow(int row)
{
    if (menuId_ == 0 || row < 0) {
        return;
    }
    if (socket_ != nullptr && socket_->state() == QLocalSocket::ConnectedState) {
        QJsonObject message;
        message.insert(QStringLiteral("cmd"), QStringLiteral("chosen"));
        message.insert(QStringLiteral("row"), row);
        QByteArray line = QJsonDocument(message).toJson(QJsonDocument::Compact);
        line.append('\n');
        socket_->write(line);
        socket_->flush();
    }
    menuId_ = 0;
    menuRows_.clear();
    menuRect_ = QRect();
    applyKeyboard();
    update();
}

void PinChrome::dismissMenu()
{
    if (menuId_ == 0) {
        return;
    }
    if (socket_ != nullptr && socket_->state() == QLocalSocket::ConnectedState) {
        QJsonObject message;
        message.insert(QStringLiteral("cmd"), QStringLiteral("dismissed"));
        QByteArray line = QJsonDocument(message).toJson(QJsonDocument::Compact);
        line.append('\n');
        socket_->write(line);
        socket_->flush();
    }
    menuId_ = 0;
    menuRows_.clear();
    menuRect_ = QRect();
    applyKeyboard();
    update();
}

void PinChrome::mousePressEvent(QMouseEvent *event)
{
    if (menuId_ == 0) {
        event->ignore();
        return;
    }
    const int row = menuRowAt(event->position().toPoint());
    if (row < 0) {
        // A click outside the menu closes it, the way it does everywhere.
        dismissMenu();
    } else {
        chooseRow(row);
    }
    event->accept();
}

void PinChrome::mouseMoveEvent(QMouseEvent *event)
{
    if (menuId_ == 0) {
        event->ignore();
        return;
    }
    const int row = menuRowAt(event->position().toPoint());
    if (row != menuHover_) {
        menuHover_ = row;
        update();
    }
    event->accept();
}

void PinChrome::keyPressEvent(QKeyEvent *event)
{
    if (menuId_ == 0) {
        event->ignore();
        return;
    }
    switch (event->key()) {
    case Qt::Key_Escape:
        dismissMenu();
        event->accept();
        return;
    case Qt::Key_Down:
    case Qt::Key_Up: {
        const int step = event->key() == Qt::Key_Down ? 1 : -1;
        const int count = menuRows_.size();
        const int from = menuHover_ < 0 ? (step > 0 ? -1 : 0) : menuHover_;
        menuHover_ = (from + step + count) % count;
        update();
        event->accept();
        return;
    }
    case Qt::Key_Return:
    case Qt::Key_Enter:
        if (menuHover_ >= 0) {
            chooseRow(menuHover_);
        }
        event->accept();
        return;
    default:
        break;
    }
    event->ignore();
}

void PinChrome::applyKeyboard()
{
    if (layer_ == nullptr) {
        return;
    }
    // On demand while a menu is up and none otherwise: the surface below holds
    // the pointer, and taking the keyboard at all costs it its focus.
    layer_->setKeyboardInteractivity(menuId_ != 0
                                         ? LayerShellQt::Window::KeyboardInteractivityOnDemand
                                         : LayerShellQt::Window::KeyboardInteractivityNone);
    if (menuId_ != 0) {
        setFocus(Qt::MouseFocusReason);
    }
}

void PinChrome::setPinnedVisible(bool visible)
{
    visible_ = visible;
    setVisible(visible);
}

void PinChrome::applyMask()
{
    // Click-through, always: the labels say what a pin is, and every gesture
    // belongs to the pin.
    //
    // The region goes on the *window*, not the widget.  `QWidget::setMask` also
    // tells Qt to stop repainting outside the mask, so a widget whose mask is
    // one pixel in the corner is a widget that never draws anything -- which is
    // exactly what happened here, and why no label ever appeared on screen
    // while every test that read the socket passed.
    QWindow *window = windowHandle();
    if (window == nullptr) {
        return;
    }
    window->setMask(clickThroughInputRegion());
}

void PinChrome::paintEvent(QPaintEvent *event)
{
    Q_UNUSED(event);
    QPainter painter(this);
    paintInto(painter);
    // A layer surface appears in no screenshot -- a compositor draws layers
    // above every window and leaves them out of a capture -- so a developer
    // asking what this actually puts on screen has only the widget itself to
    // look at.  Written where a developer looks, and only when asked.
    if (qEnvironmentVariableIsSet("VSHOT_PIN_DEBUG")) {
        QImage shot(size(), QImage::Format_ARGB32);
        shot.fill(Qt::transparent);
        QPainter into(&shot);
        paintInto(into);
        into.end();
        shot.save(QStringLiteral("/tmp/vshot-pin-chrome.png"), "PNG");
    }
}

void PinChrome::paintInto(QPainter &painter)
{
    painter.setCompositionMode(QPainter::CompositionMode_Source);
    painter.fillRect(rect(), Qt::transparent);
    painter.setCompositionMode(QPainter::CompositionMode_SourceOver);
    if (!visible_) {
        return;
    }
    painter.setFont(tagFont());
    if (menuId_ != 0 && !menuRect_.isEmpty()) {
        // The menu, above every label: it is the thing the user is looking at.
        painter.save();
        painter.setRenderHint(QPainter::Antialiasing, true);
        painter.setPen(Qt::NoPen);
        painter.setBrush(kLabelBox);
        painter.drawRoundedRect(menuRect_, kMenuRadius, kMenuRadius);
        painter.setBrush(Qt::NoBrush);
        painter.setPen(QPen(QColor(255, 255, 255, 90), 1));
        painter.drawRoundedRect(QRectF(menuRect_).adjusted(0.5, 0.5, -0.5, -0.5), kMenuRadius,
                                kMenuRadius);
        const QFontMetrics metrics(tagFont());
        const int rowHeight = metrics.height() + 2 * kMenuRowPaddingY;
        for (int row = 0; row < menuRows_.size(); ++row) {
            const QRect box(menuRect_.left(), menuRect_.top() + row * rowHeight,
                            menuRect_.width(), rowHeight);
            paintMenuRow(painter, box, menuRows_.at(row), row == menuHover_);
        }
        painter.restore();
    }
    const QPoint origin = screen_ != nullptr ? screen_->geometry().topLeft() : QPoint(0, 0);
    for (Entry &entry : entries_) {
        const QRect target(entry.label.origin - origin, entry.label.size);
        if (!target.intersects(rect())) {
            continue;
        }
        // The tag only while the pointer is over the pin: it says what the
        // thing under the cursor is, and one on every pin at once would be a
        // wall of text over the desktop.
        if (entry.label.capturedHdr && entry.label.hovered) {
            const QRect box = tagBox(kHdrTag, target.topLeft(), false, rect());
            if (!box.isEmpty()) {
                paintLabel(painter, box, kHdrTag,
                           entry.label.shownAsHdr ? kHdrTagShown : kHdrTagFallback);
            }
        }
        if (!entry.badge.isEmpty()) {
            const QRect box = tagBox(entry.badge, target.bottomRight(), true, rect());
            if (!box.isEmpty()) {
                paintLabel(painter, box, entry.badge, Qt::white);
            }
        }
    }
}

} // namespace vshot
