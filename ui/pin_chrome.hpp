// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include <QHash>
#include <QImage>
#include <QPoint>
#include <QRect>
#include <QSize>
#include <QString>
#include <QStringList>
#include <QVector>
#include <QWidget>

#include <cstdint>

class QLocalSocket;
class QMouseEvent;
class QKeyEvent;
class QPainter;
class QScreen;
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
        /// Whether the pointer is over this pin, which is the only one whose
        /// tag is up.
        bool hovered = false;
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

    /// Draws a pin's right-click menu, or takes down whatever menu is up.
    ///
    /// The rows are the daemon's -- it is the side that knows what a pin is and
    /// what can be done to it -- and the drawing, the pointer and the keyboard
    /// are this side's, because this is the surface with all three.  The answer
    /// goes back as a row number.
    void setMenu(quint64 id, const QPoint &anchor, const QStringList &rows);

    /// The socket answers are written to.  Set once, when the connection comes
    /// up; a chrome with none simply never answers.
    void setSocket(QLocalSocket *socket) { socket_ = socket; }

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
    /// Everything this surface draws, into any painter: the widget's own
    /// paintEvent and the debug dump that saves what it put on screen, which is
    /// the only way to see a layer surface at all.
    void paintInto(QPainter &painter);
    void mousePressEvent(QMouseEvent *event) override;
    void mouseMoveEvent(QMouseEvent *event) override;
    void keyPressEvent(QKeyEvent *event) override;
    void applyMask();
    /// The keyboard is taken only while a menu is up: Esc has to reach this
    /// surface for the menu to be dismissible, and nothing else here wants a
    /// key.  Taking it at all costs the surface below its pointer focus, so it
    /// is asked for and given back.
    void applyKeyboard();
    /// The rectangle a menu of `rows` occupies when anchored at `anchor`, or an
    /// empty rect when it cannot fit on this output at all.
    QRect menuRectFor(const QPoint &anchor, const QStringList &rows) const;
    /// The row the pointer is over, or -1.
    int menuRowAt(const QPoint &local) const;
    /// Tells the daemon which row was picked, and takes the menu down.
    void chooseRow(int row);
    /// Tells the daemon the menu is gone without a pick.
    void dismissMenu();

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
    /// The open menu: the pin it is about, the rows the daemon sent, where it
    /// sits and which row the pointer is over.
    quint64 menuId_ = 0;
    QStringList menuRows_;
    QRect menuRect_;
    int menuHover_ = -1;
    /// The socket the daemon is on, so an answer can be sent back.  Not owned:
    /// it belongs to the server that made it.
    QLocalSocket *socket_ = nullptr;
};

} // namespace vshot
