// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "pin_chrome.hpp"

#include "pin_label.hpp"

#include <LayerShellQt/Window>

#include <QGuiApplication>
#include <QPainter>
#include <QPainterPath>
#include <QRegion>
#include <QScreen>
#include <QTimer>

namespace vshot {
namespace {

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

void PinChrome::setPinnedVisible(bool visible)
{
    visible_ = visible;
    setVisible(visible);
}

void PinChrome::applyMask()
{
    // Click-through, always: the labels say what a pin is, and every gesture
    // belongs to the pin.
    setMask(clickThroughInputRegion());
}

void PinChrome::paintEvent(QPaintEvent *event)
{
    Q_UNUSED(event);
    QPainter painter(this);
    painter.setCompositionMode(QPainter::CompositionMode_Source);
    painter.fillRect(rect(), Qt::transparent);
    painter.setCompositionMode(QPainter::CompositionMode_SourceOver);
    if (!visible_) {
        return;
    }
    painter.setFont(tagFont());
    const QPoint origin = screen_ != nullptr ? screen_->geometry().topLeft() : QPoint(0, 0);
    for (Entry &entry : entries_) {
        const QRect target(entry.label.origin - origin, entry.label.size);
        if (!target.intersects(rect())) {
            continue;
        }
        if (entry.label.capturedHdr) {
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
