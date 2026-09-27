// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include <QColor>
#include <QImage>
#include <QPoint>
#include <QPointF>
#include <QRect>
#include <QString>
#include <QVector>
#include <QWidget>

#include <functional>
#include <memory>

class QLineEdit;
class QScreen;

namespace LayerShellQt {
class Window;
}

namespace vshot {

// One annotation surface: the whole of one output, drawn over a desktop that
// keeps running.
//
// This is not the capture overlay.  That one paints a frame that was grabbed
// once and stays still, so every stroke can be re-rendered from its own model
// on each repaint.  Here the desktop below is alive: the user may be pointing
// at a video, a game or a terminal, and the annotation has to appear with the
// pointer, not a frame later.  So every committed stroke keeps its own
// device-pixel ink, built on first paint and reused after that, and a repaint
// blits only the strokes that fall inside the region being repainted.  There is
// no monolithic canvas to rebuild: removing a stroke is dropping it out of the
// list, so the eraser, undo, redo and clear cost nothing beyond the repaint of
// the rect they touch.
//
// The surface owns the whole output on purpose: "draw anywhere" means every
// click has to arrive here, so no input mask is installed while it is visible.
// The toolbar is a child widget, which is what keeps its buttons apart from the
// drawing without costing the canvas a single pixel of its own.
class AnnotateSurface final : public QWidget {
public:
    // What a fresh drag draws.  The eraser is not a drawing tool: it removes
    // whole strokes, see `eraseStrokeAt`.
    enum class Tool {
        Pen,
        Eraser,
        Rect,
        Arrow,
        // The two straight, two-point tools: a plain segment and the same
        // segment turned into a sine wave.  Both are drawn from exactly the two
        // points the drag made, like the arrow, so neither accumulates points
        // as the pointer wanders.
        Line,
        Wave,
        Text,
    };

    explicit AnnotateSurface(QScreen *screen);
    ~AnnotateSurface() override;

    // Maps the widget onto its layer-shell surface.  Returns false when
    // LayerShellQt is unavailable.
    bool showLayerSurface();

    // The output this surface is mapped onto.
    QScreen *screen() const { return screen_; }

    void setTool(Tool tool);
    Tool tool() const { return tool_; }
    void setColor(const QColor &color);
    QColor color() const { return color_; }
    // Stroke width in logical pixels, clamped by the implementation.
    //
    // Named `penWidth` and not `width`: `QWidget::width` is the surface's own
    // width, and a same-named accessor here would shadow it -- every call that
    // meant the widget would silently read the stroke width instead.
    void setPenWidth(int width);
    int penWidth() const { return width_; }

    void undo();
    void redo();
    bool canUndo() const;
    bool canRedo() const;
    // Forgets every stroke.  The wipe is undoable like any other change, so a
    // stray click on the toolbar's clear button costs one undo.
    void clear();
    int strokeCount() const;
    bool isEmpty() const;

    // How many times a stroke's ink has been rasterized into its own image.
    // Each stroke is rasterized once and then reused, which is what makes the
    // eraser, an undo and a redo cheap; the checks read this to see that it
    // holds.
    int rasterBuilds() const { return rasterBuilds_; }
    // The union of the logical rects invalidated since `clearInvalidatedRect()`.
    // Every pixel an interactive step changes has to fall inside it -- a pixel
    // outside is one this surface would leave stale on screen.
    QRect invalidatedRect() const { return invalidated_; }
    void clearInvalidatedRect() { invalidated_ = QRect(); }

    // Hides or shows the toolbar without touching the drawing.  Used while a
    // capture or a recording is running: the toolbar is a window like any
    // other and would be baked into the picture, while the annotation itself is
    // usually the reason the user is capturing at all.
    void setToolbarHidden(bool hidden);
    bool toolbarHidden() const { return toolbarHidden_; }

    // The toolbar's rect in this surface's logical pixels, empty when hidden.
    QRect toolbarRect() const;

    // Invoked when the user presses the toolbar's quit button.  The daemon owns
    // the socket and the other outputs, so it is the daemon that quits.
    void setQuitCallback(std::function<void()> callback) { quit_ = std::move(callback); }

protected:
    void paintEvent(QPaintEvent *event) override;
    void mousePressEvent(QMouseEvent *event) override;
    void mouseMoveEvent(QMouseEvent *event) override;
    void mouseReleaseEvent(QMouseEvent *event) override;
    void resizeEvent(QResizeEvent *event) override;
    void keyPressEvent(QKeyEvent *event) override;

private:
    // The device-pixel ink of one committed stroke.  `image` is tightly sized to
    // the stroke and `origin` is where its top-left lands in this surface's
    // device pixels, so blitting it is a straight copy at 1:1.
    struct StrokeRaster {
        QImage image;
        // The stroke's logical box, kept so a repaint can skip the stroke
        // without walking its points again.
        QRectF logicalBounds;
        QPoint origin;
        double ratio = 1.0;
    };

    // One annotation.  A point list for every tool but the text one, which
    // carries its string and its anchor instead; the eraser never produces one.
    struct Stroke {
        Tool tool = Tool::Pen;
        QColor color;
        int width = 3;
        QVector<QPointF> points;
        QString text;
        // The ink of this stroke, rasterized into its own image on first paint.
        // A committed stroke is never modified, so the only thing that can
        // invalidate this is a change of device ratio; the shared pointer means
        // an undo or redo snapshot carries the raster with it instead of
        // rasterizing the stroke a second time.
        std::shared_ptr<StrokeRaster> raster;
    };

    // The floating palette.  Defined in the .cpp: it is a nested widget with a
    // nested button class of its own, and no other file has any business
    // knowing its shape.
    class Toolbar;

    // The eraser's reach in logical pixels.  A stroke is removed whole when the
    // eraser passes within this distance of it, which is what makes an eraser
    // drag undoable in one piece.
    static constexpr double kEraserRadius = 12.0;
    // Where the toolbar lands before the user drags it anywhere: centred
    // horizontally, this far below the output's top edge.
    static constexpr int kToolbarMargin = 12;
    // Stroke widths the toolbar offers, in logical pixels.
    static constexpr int kWidths[] = {3, 6, 12};

    // Turns one stroke into ink on a painter, in logical coordinates.  This is
    // the only path from a stroke to ink: both the live preview and the
    // per-stroke raster builder draw through it.
    void paintStroke(QPainter &painter, const Stroke &stroke) const;
    // The stroke's own cached ink, built on first use and reused until the
    // device ratio changes.  Returns null for a stroke with nothing to draw.
    // Deliberately non-const and given a mutable reference: it stores the raster
    // in the stroke it was handed.
    const StrokeRaster *rasterFor(Stroke &stroke);
    // Records the state `strokes_` is in before it is changed, so one undo can
    // step back over it.  `dirty` is the region the change about to be made can
    // affect; it travels with the snapshot so stepping back over it repaints
    // that region instead of the whole output.  Snapshots rather than per-tool
    // inverse operations: an eraser drag, a text edit and a clear all have to be
    // undoable, and a stroke list is small enough that copying it is the
    // cheapest way to make that one mechanism instead of four.
    void pushHistory(const QRect &dirty);
    // The rect a stroke can have touched, in device pixels, grown by its width
    // and the arrow head's reach.  This is the repaint region of a new stroke,
    // and the reason a drag does not repaint a 4K output sixty times a second.
    QRect deviceDirtyRect(const Stroke &stroke) const;
    // Repaints a logical rect of this surface.
    void touch(const QRect &logical);
    // Removes the frontmost stroke within the eraser's reach of `local`, if any.
    bool eraseStrokeAt(const QPointF &local);
    // Whether a stroke is close enough to `local` for the eraser to take it.
    bool strokeHits(const Stroke &stroke, const QPointF &local) const;
    // Opens the inline text editor at `local`, or moves the one already open.
    void beginText(const QPointF &local);
    // Commits (`accept`) or drops the open text editor.
    void finishText(bool accept);
    // Hands the keyboard to the compositor, or asks for it, for text entry.
    // Only the text editor needs the keyboard: drawing a stroke does not, and a
    // surface that held it would stop the user from typing anywhere else.
    void setKeyboardWanted(bool wanted);
    // Whether this surface's own device ratio is not 1, i.e. whether a logical
    // rect needs scaling before it can index a stroke's device-pixel ink.
    double deviceRatio() const;

    QVector<Stroke> strokes_;
    // One undo step: the stroke list as it was, plus the rect the change that
    // followed it touched.  The rect travels with the snapshot so stepping back
    // or forward repaints that rect instead of the whole output.
    struct HistoryEntry {
        QVector<Stroke> strokes;
        QRect dirty;
    };
    // Undo and redo as stroke-list snapshots, oldest first, newest last.  The
    // state before each change is pushed; an undo moves the current state onto
    // the redo stack and restores the top of the undo stack.
    QVector<HistoryEntry> undoStack_;
    QVector<HistoryEntry> redoStack_;
    // The stroke the pointer is dragging out, if any.  Drawn last, on top, and
    // only from the widget's own paint: it is not part of the model until the
    // button comes up.
    bool drawing_ = false;
    Stroke pending_;
    // Where the eraser last looked, so a fast drag does not step over a stroke.
    QPointF eraseFrom_;
    bool erasing_ = false;

    // The union of the logical rects `touch()` has been asked to repaint since
    // the last `clearInvalidatedRect()`.  Read by the checks to prove a step
    // repainted everything it changed.
    QRect invalidated_;
    // How many stroke inks have been rasterized, ever.  The checks read it
    // through `rasterBuilds()`.
    int rasterBuilds_ = 0;
    Tool tool_ = Tool::Pen;
    QColor color_{229, 57, 53};
    int width_ = kWidths[1];
    Toolbar *toolbar_ = nullptr;
    bool toolbarHidden_ = false;
    // Where the user dragged the toolbar to, in this surface's logical pixels;
    // null until the first drag, so the default placement survives a resize.
    QPoint toolbarOrigin_;
    QLineEdit *textEdit_ = nullptr;
    QPointF textOrigin_;

    QScreen *screen_ = nullptr;
    LayerShellQt::Window *layer_ = nullptr;
    bool surfaceReady_ = false;
    bool keyboardWanted_ = false;
    std::function<void()> quit_;
};

} // namespace vshot
