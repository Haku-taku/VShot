// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include <QHash>
#include <QImage>
#include <QPoint>
#include <QRect>
#include <QSize>
#include <QString>
#include <QVector>
#include <QWidget>

#include <cstdint>

class QScreen;
class QTcpSocket;
class QLocalSocket;
class QTimer;

namespace LayerShellQt {
class Window;
}

namespace vshot {

// The corner labels of the pin stack, drawn on a surface of their own.
//
// A pinned picture is painted by the daemon's half-float surfaces, which hold
// light and no glyphs: text there would mean laying out and rasterising a font
// by hand, in the middle of a renderer whose whole job is to put the captured
// light on the panel.  So the labels are Qt's, on one transparent layer surface
// per output, mapped after the pictures -- a compositor stacks a layer's
// surfaces in map order, and there is no request to restack them, so a surface
// mapped later is the one that ends up on top.
//
// Everything here is *decoration*: it never takes the pointer and never decides
// anything.  What a label says comes from the daemon, which owns the pins, and
// the only thing this surface knows that the daemon does not is where its own
// output is on the desktop.
class PinChrome final : public QWidget {
public:
    // One pin as the chrome needs it: where it is, how big it is drawn, and
    // what is worth saying about it.
    //
    // No pixels.  The chrome draws no picture -- that is the whole point of it
    // being a separate surface -- so a copy of the image would be a copy of
    // something this side can never show.
    struct Label {
        quint64 id = 0;
        /// Global logical top-left of the painted picture.
        QPoint origin;
        /// The size the picture is drawn at, in logical pixels.
        QSize size;
        /// Whether this pin is an HDR capture, whatever is showing it.  The tag
        /// belongs to the capture rather than to the surface drawing it.
        bool capturedHdr = false;
        /// Whether the pixels on screen are the capture's own HDR light, as
        /// opposed to the same capture mapped down for an SDR output.  The tag
        /// is drawn in a different ink for each.
        bool shownAsHdr = false;
    };

    explicit PinChrome(QScreen *screen);

    /// Maps the widget onto its layer-shell surface.  False when LayerShellQt
    /// is unavailable, which is a compositor that cannot show a pin at all.
    bool showLayerSurface();

    /// The output this surface is mapped onto.
    QScreen *screen() const { return screen_; }

    /// Replaces the whole stack.  Back to front, as the daemon paints it, so
    /// that two labels on one pin overlap in the order the pins do.
    void setLabels(const QVector<Label> &labels);

    /// Puts `text` on `id`'s corner for a moment: the zoom factor after a wheel
    /// step, or the outcome of a copy or a save.  A badge names a pin that is
    /// still in the stack, so one for a pin that is gone is dropped.
    void showBadge(quint64 id, const QString &text);

    /// Hides every label without forgetting them, for the daemon's `hide`.
    void setPinnedVisible(bool visible);
    bool isPinnedVisible() const { return visible_; }

private:
    // One pin's labels: the `HDR` tag while the pointer is over it, and the
    // badge for as long as it lasts.
    struct Entry {
        Label label;
        QRect tagRect;
        QRect badgeRect;
        QString badge;
    };

    void paintEvent(QPaintEvent *event) override;
    void applyMask();

    QScreen *screen_ = nullptr;
    LayerShellQt::Window *layer_ = nullptr;
    QVector<Entry> entries_;
    bool visible_ = true;
    bool surfaceReady_ = false;
    // The pin the pointer is over, which is the only one whose tag is up.
    quint64 hoverId_ = 0;
    // The pin a badge belongs to, and its text, for as long as it lasts.
    quint64 badgeId_ = 0;
    QTimer *badgeTimer_ = nullptr;
};

} // namespace vshot
