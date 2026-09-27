// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline check for the annotation overlay: what a drag paints, what the
// eraser takes away, what undo brings back, what a click on the toolbar does
// *not* paint, and where the toolbar lands before anyone drags it.
//
// The properties worth locking down are the ones that are invisible in the
// code and obvious to the user: a stroke has to reach the pixels (the model
// holding a stroke and the canvas showing it are two different things), the
// eraser has to be precise enough to leave the neighbouring stroke alone, an
// undo has to actually clear the canvas rather than only the list it is drawn
// from, and a stray drag across the toolbar must not leave a line across the
// screen. Every one of those fails silently if the wiring is wrong, which is
// exactly why they are driven through synthetic pointer events and read back as
// pixels instead of being asserted on the model.
//
// Built only with `-DVSHOT_BUILD_CHECKS=ON`. Needs Qt Widgets and the offscreen
// platform plugin (run it with QT_QPA_PLATFORM=offscreen), but no compositor and
// no layer shell: the surface is rendered straight into a QImage through
// QWidget::render(), and showLayerSurface() is never called.

#include "annotate_surface.hpp"
#include "i18n.hpp"

#include <QApplication>
#include <QColor>
#include <QImage>
#include <QKeyEvent>
#include <QLineEdit>
#include <QMouseEvent>
#include <QPainter>
#include <QPoint>
#include <QResizeEvent>
#include <QScreen>
#include <QString>
#include <QWidget>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <functional>
#include <limits>

namespace {

int failures = 0;

// The colour the default pen draws in, spelled out here on purpose: a check
// that imported the surface's own constant would pass for any value at all.
constexpr int kInkR = 229;
constexpr int kInkG = 57;
constexpr int kInkB = 53; // 0x35: the default red is #e53935

// The surface is output-sized, and deliberately wide enough to hold the whole
// palette on one row: the default placement below is checked against a surface
// the palette really fits in, the way every output a user has does.  1280 is a
// representative logical width -- a 1080p or 4K output is wider than this, and
// the one-row palette is about 900 logical pixels -- so the row stays on one row
// as it does on real outputs.  A narrower surface may fold the row, which is the
// separate property `checkToolbarWrapping` pins down.
constexpr int kSurfaceWidth = 1280;
constexpr int kSurfaceHeight = 600;

// The output widths the palette's one-row / wrapping behaviour is read at: a
// wide output has room for the whole row and keeps it on one row, a narrow one
// does not and makes the palette wrap.  Spelled out here like the ink below, so
// the numbers are the check's own and not the palette's.
constexpr int kWideSurfaceWidth = 1600;
constexpr int kNarrowSurfaceWidth = 480;

// One row of the palette: the button height plus the panel's own padding, the
// height every normal output shows.  Spelled out on purpose: a change that
// quietly grew the palette to two rows on every output would otherwise pass
// every assertion here.
constexpr int kSingleRowHeight = 40;

// The two segment tools' own numbers, spelled out here rather than imported
// from the surface, the way the ink colour is.  kLineWidth is the pen width the
// checks set; the amplitude and wavelength are the surface's formula
// `max(width * 2, 4)` and `max(width * 6, 18)` at that width, so a wave is
// twelve logical pixels tall and a straight line reaches its ends.
constexpr int kLineWidth = 6;
constexpr int kHalfWidth = kLineWidth / 2;
constexpr int kWaveAmplitude = 12; // max(6 * 2, 4)

// Distance from a point to the segment `a`..`b`.
double distanceToSegment(const QPointF &p, const QPointF &a, const QPointF &b)
{
    const QPointF ab = b - a;
    const double length2 = ab.x() * ab.x() + ab.y() * ab.y();
    if (length2 <= 0.0) {
        return std::hypot(p.x() - a.x(), p.y() - a.y());
    }
    const double t = std::clamp(
        ((p.x() - a.x()) * ab.x() + (p.y() - a.y()) * ab.y()) / length2, 0.0, 1.0);
    return std::hypot(p.x() - (a.x() + t * ab.x()), p.y() - (a.y() + t * ab.y()));
}

// Counts the check's ink inside `area` that lies outside `bounds`, skipping the
// rects the toolbar's children cover: the default red is also the palette's
// first colour, so a whole-image pixel test that ignored the swatch would read
// it as a stray stroke.
int strayInk(const QImage &image, const QRect &area, const QRect &bounds,
             const QVector<QRect> &children);

void expect(const char *what, bool ok, const QString &detail = QString())
{
    if (ok) {
        std::printf("ok    %s\n", what);
        return;
    }
    std::printf("FAIL  %s%s\n", what,
                detail.isEmpty()
                    ? ""
                    : qPrintable(QStringLiteral(" -- ") + detail));
    ++failures;
}

QImage renderSurface(QWidget &surface)
{
    QImage image(surface.size(), QImage::Format_ARGB32_Premultiplied);
    image.fill(Qt::transparent);
    QPainter painter(&image);
    surface.render(&painter);
    painter.end();
    return image;
}

// Whether the pixel at a logical point is the opaque default ink.  Alpha is
// checked as well as colour: a stroke that faded in or a canvas blitted at the
// wrong scale would still pass a colour-only test on a half-covered pixel.
bool isInk(const QImage &image, const QPoint &logical)
{
    const QColor got = image.pixelColor(logical);
    return got.red() == kInkR && got.green() == kInkG && got.blue() == kInkB &&
           got.alpha() == 255;
}

bool isClear(const QImage &image, const QPoint &logical)
{
    return image.pixelColor(logical).alpha() == 0;
}

// Whether any pixel in a rect carries ink.  Used where the exact pixel a tool
// paints is its own business -- the text tool's glyphs land wherever the font
// puts them -- while "it painted something, here abouts" is the property that
// matters.
bool hasInk(const QImage &image, const QRect &area)
{
    const QRect bounded = area.intersected(image.rect());
    for (int y = bounded.top(); y <= bounded.bottom(); ++y) {
        for (int x = bounded.left(); x <= bounded.right(); ++x) {
            if (image.pixelColor(x, y).alpha() != 0) {
                return true;
            }
        }
    }
    return false;
}

int strayInk(const QImage &image, const QRect &area, const QRect &bounds,
             const QVector<QRect> &children)
{
    int stray = 0;
    const QRect scan = area.intersected(image.rect());
    for (int y = scan.top(); y <= scan.bottom(); ++y) {
        for (int x = scan.left(); x <= scan.right(); ++x) {
            const QPoint at(x, y);
            if (!isInk(image, at) || bounds.contains(at)) {
                continue;
            }
            bool inChild = false;
            for (const QRect &child : children) {
                if (child.contains(at)) {
                    inChild = true;
                    break;
                }
            }
            if (!inChild) {
                ++stray;
            }
        }
    }
    return stray;
}

void press(QWidget *surface, const QPoint &local)
{
    QMouseEvent event(QEvent::MouseButtonPress, QPointF(local),
                      QPointF(surface->mapToGlobal(local)), Qt::LeftButton,
                      Qt::LeftButton, Qt::NoModifier);
    QApplication::sendEvent(surface, &event);
}

void moveTo(QWidget *surface, const QPoint &local)
{
    QMouseEvent event(QEvent::MouseMove, QPointF(local),
                      QPointF(surface->mapToGlobal(local)), Qt::NoButton,
                      Qt::LeftButton, Qt::NoModifier);
    QApplication::sendEvent(surface, &event);
}

void release(QWidget *surface, const QPoint &local)
{
    QMouseEvent event(QEvent::MouseButtonRelease, QPointF(local),
                      QPointF(surface->mapToGlobal(local)), Qt::LeftButton,
                      Qt::NoButton, Qt::NoModifier);
    QApplication::sendEvent(surface, &event);
}

// One drag, in steps: a pointer does not travel in one jump, and a surface that
// only handled the endpoints would be a different implementation.
void drag(QWidget *surface, const QPoint &from, const QPoint &to)
{
    press(surface, from);
    const int steps = std::max(1, std::abs(to.x() - from.x()));
    for (int step = 1; step <= steps; ++step) {
        const QPoint at(from.x() + (to.x() - from.x()) * step / steps,
                        from.y() + (to.y() - from.y()) * step / steps);
        moveTo(surface, at);
    }
    release(surface, to);
}

// The area the surface's live child widgets cover, in surface coordinates.  The
// toolbar and its buttons repaint themselves when their own state changes -- Qt
// drives that, not the surface's invalidated region -- so a difference under a
// child is not something the surface's rect can be blamed for.
QVector<QRect> childAreas(const QWidget &surface)
{
    QVector<QRect> areas;
    for (QWidget *child : surface.findChildren<QWidget *>()) {
        if (child->isWindow() || child->isHidden()) {
            continue;
        }
        areas.append(QRect(child->mapTo(&surface, QPoint()), child->size()));
    }
    return areas;
}

// Every pixel an interactive step changes has to fall inside the region the
// step asked to repaint: a pixel outside it is one this surface would have left
// stale on screen.  Pixels inside a child widget (the toolbar repaints itself
// through syncState) are not the surface's to repaint.
void expectStepCovered(vshot::AnnotateSurface &surface, const char *what,
                       const std::function<void()> &step)
{
    const QImage before = renderSurface(surface);
    surface.clearInvalidatedRect();
    step();
    const QImage after = renderSurface(surface);
    const QRect allowed = surface.invalidatedRect().adjusted(-1, -1, 1, 1);
    const QVector<QRect> children = childAreas(surface);
    int outside = 0;
    for (int y = 0; y < after.height(); ++y) {
        for (int x = 0; x < after.width(); ++x) {
            if (before.pixel(x, y) == after.pixel(x, y)) {
                continue;
            }
            const QPoint at(x, y);
            if (allowed.contains(at)) {
                continue;
            }
            bool inChild = false;
            for (const QRect &child : children) {
                if (child.contains(at)) {
                    inChild = true;
                    break;
                }
            }
            if (!inChild) {
                ++outside;
            }
        }
    }
    expect(what, outside == 0,
           QStringLiteral("%1 px changed outside the invalidated region").arg(outside));
}

void checkPenAndRect(vshot::AnnotateSurface &surface)
{
    surface.setTool(vshot::AnnotateSurface::Tool::Pen);
    surface.setPenWidth(6);
    drag(&surface, QPoint(40, 120), QPoint(200, 120));

    QImage image = renderSurface(surface);
    expect("a pen drag paints along its path", isInk(image, QPoint(120, 120)));
    expect("a pen drag paints nothing above it", isClear(image, QPoint(120, 60)));
    expect("the pen drag is one stroke", surface.strokeCount() == 1,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));

    surface.setTool(vshot::AnnotateSurface::Tool::Rect);
    drag(&surface, QPoint(40, 40), QPoint(160, 90));
    image = renderSurface(surface);
    // A band rather than one pixel: where exactly the stroke sits inside the
    // dragged rectangle is the implementation's business (a path inset by half
    // the pen width puts the ink wholly inside), while "the top edge is drawn"
    // is the property that matters.
    expect("a rectangle paints its edge", hasInk(image, QRect(60, 36, 80, 10)));
    expect("a rectangle leaves its inside alone", isClear(image, QPoint(100, 65)));
    expect("the rectangle is a second stroke", surface.strokeCount() == 2);

    surface.setTool(vshot::AnnotateSurface::Tool::Arrow);
    drag(&surface, QPoint(40, 180), QPoint(200, 180));
    image = renderSurface(surface);
    expect("an arrow paints its shaft", isInk(image, QPoint(120, 180)));
    expect("an arrow paints its head near the tip",
           hasInk(image, QRect(178, 171, 22, 18)));
    expect("the arrow is a third stroke", surface.strokeCount() == 3);

    // Put the canvas back to the pen stroke alone for the checks below.
    surface.clear();
    surface.setTool(vshot::AnnotateSurface::Tool::Pen);
    drag(&surface, QPoint(40, 120), QPoint(200, 120));
}

void checkEraser(vshot::AnnotateSurface &surface)
{
    // A second stroke well away from the eraser's path: it has to survive, and
    // that is what makes "removes whole strokes" a precise claim rather than
    // "removes something".
    surface.setTool(vshot::AnnotateSurface::Tool::Pen);
    drag(&surface, QPoint(40, 60), QPoint(200, 60));
    expect("two strokes before erasing", surface.strokeCount() == 2);

    surface.setTool(vshot::AnnotateSurface::Tool::Eraser);
    drag(&surface, QPoint(120, 100), QPoint(120, 140));

    QImage image = renderSurface(surface);
    expect("the eraser took the stroke it crossed",
           isClear(image, QPoint(120, 120)));
    expect("the eraser left the other stroke alone",
           isInk(image, QPoint(120, 60)));
    expect("erasing removed one stroke", surface.strokeCount() == 1,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));
}

void checkUndoRedoClear(vshot::AnnotateSurface &surface)
{
    // A known starting point: what the earlier checks drew is history the
    // assertions below must not step back into.
    surface.clear();
    surface.setTool(vshot::AnnotateSurface::Tool::Pen);
    drag(&surface, QPoint(40, 60), QPoint(200, 60));
    expect("undo is available after drawing", surface.canUndo());

    surface.undo();
    QImage image = renderSurface(surface);
    expect("an undo clears the canvas, not only the list", isClear(image, QPoint(120, 60)));
    expect("the undo is redoable", surface.canRedo());

    surface.redo();
    image = renderSurface(surface);
    expect("a redo paints it back", isInk(image, QPoint(120, 60)));

    // A new stroke is a new branch: what was undone cannot come back.
    surface.undo();
    drag(&surface, QPoint(40, 220), QPoint(200, 220));
    expect("a new stroke drops the redo branch", !surface.canRedo());

    surface.clear();
    image = renderSurface(surface);
    expect("clear empties the canvas", isClear(image, QPoint(120, 220)));
    expect("clear forgets every stroke", surface.strokeCount() == 0,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));
    surface.undo();
    image = renderSurface(surface);
    expect("a clear is undoable", isInk(image, QPoint(120, 220)));
}

// Resizes a surface the way a real output appearing at that size would.  The
// check surfaces are rendered, never shown, so Qt defers their resize event
// until a show that never comes; the toolbar settles its place and its size
// from that event, so it has to be delivered here.  A surface that is only ever
// the offscreen screen's own width happens to need nothing, but every surface
// the check gives another width -- the main one below included -- does.
void resizeSurface(vshot::AnnotateSurface &surface, int width, int height)
{
    const QSize before = surface.size();
    surface.setGeometry(0, 0, width, height);
    QResizeEvent event(surface.size(), before);
    QApplication::sendEvent(&surface, &event);
}

void checkToolbar(vshot::AnnotateSurface &surface)
{
    const QRect bar = surface.toolbarRect();
    std::printf("      output %d: toolbar %dx%d at (%d,%d)\n", surface.width(), bar.width(),
                bar.height(), bar.x(), bar.y());
    expect("the toolbar has a rect", !bar.isEmpty(),
           QStringLiteral("%1x%2").arg(bar.width()).arg(bar.height()));
    expect("the toolbar sits along the top edge", bar.top() >= 0 && bar.top() <= 40,
           QStringLiteral("top=%1").arg(bar.top()));
    expect("the toolbar is centred horizontally",
           std::abs(bar.center().x() - surface.width() / 2) <= 2,
           QStringLiteral("centre=%1 surface=%2")
               .arg(bar.center().x())
               .arg(surface.width() / 2));
    expect("the toolbar fits inside the output", bar.right() <= surface.width());

    // A stroke to compare against, so "the click changed nothing" is a claim
    // about pixels rather than about an empty canvas.
    surface.setTool(vshot::AnnotateSurface::Tool::Pen);
    surface.setPenWidth(6);
    drag(&surface, QPoint(60, 300), QPoint(260, 300));
    const int strokesBefore = surface.strokeCount();
    const QImage before = renderSurface(surface);
    expect("the reference stroke is on the canvas", isInk(before, QPoint(160, 300)));

    // A click on the toolbar has to reach the panel and not the canvas.  Qt
    // routes a click to the child widget under the point, so the property worth
    // checking is that the panel is what sits there at all -- sending the event
    // to the surface directly would bypass the very mechanism that makes it so.
    QWidget *under = surface.childAt(bar.center());
    expect("the toolbar is what a click at its centre hits", under != nullptr);
    if (under != nullptr) {
        const QPoint local = under->mapFrom(&surface, bar.center());
        press(under, local);
        release(under, local);
    }
    expect("a click on the toolbar draws nothing", surface.strokeCount() == strokesBefore,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));
    const QImage after = renderSurface(surface);
    expect("a click on the toolbar leaves the canvas alone", isInk(after, QPoint(160, 300)));

    // Hiding it takes it off the screen without touching the drawing.
    surface.setToolbarHidden(true);
    expect("a hidden toolbar has no rect", surface.toolbarRect().isEmpty());
    const QImage hidden = renderSurface(surface);
    expect("hiding the toolbar keeps the drawing", isInk(hidden, QPoint(160, 300)));
    surface.setToolbarHidden(false);
    expect("the toolbar comes back", surface.toolbarRect() == bar);
}

// The palette has to fit an output that has room for it on one row -- the
// order, the spacing and the separators on that row are the look a user knows
// -- and may only fold onto more rows where the output is too narrow to hold
// the row.  Both ends are read off a real surface, so neither "it quietly
// wraps everywhere" nor "it can never wrap" can slip through.
void checkToolbarWrapping(QScreen *screen)
{
    {
        vshot::AnnotateSurface wide(screen);
        resizeSurface(wide, kWideSurfaceWidth, kSurfaceHeight);
        const QRect bar = wide.toolbarRect();
        std::printf("      wide output %d: toolbar %dx%d at (%d,%d)\n", kWideSurfaceWidth,
                    bar.width(), bar.height(), bar.x(), bar.y());
        expect("a wide output keeps the toolbar on one row",
               bar.height() == kSingleRowHeight,
               QStringLiteral("%1x%2").arg(bar.width()).arg(bar.height()));
        expect("the one-row toolbar fits across the wide output",
               bar.right() <= wide.width());
    }
    {
        vshot::AnnotateSurface narrow(screen);
        resizeSurface(narrow, kNarrowSurfaceWidth, kSurfaceHeight);
        const QRect bar = narrow.toolbarRect();
        std::printf("      narrow output %d: toolbar %dx%d at (%d,%d)\n", kNarrowSurfaceWidth,
                    bar.width(), bar.height(), bar.x(), bar.y());
        expect("a narrow output folds the toolbar onto more rows",
               bar.height() > kSingleRowHeight,
               QStringLiteral("%1x%2").arg(bar.width()).arg(bar.height()));
        expect("the folded toolbar still fits across the narrow output",
               bar.right() <= narrow.width());
    }
}

// The two segment tools: a straight line is the capsule of its two points and
// nothing wider, and a wave is the sine of that same segment -- crests reaching
// the amplitude on both sides, crossing the line again and again, and finishing
// back on it at both ends.  Each is drawn on its own surface so the pixel scans
// see one mark and the toolbar (whose swatch is the same red) is skipped.
void checkLineAndWave(QScreen *screen)
{
    vshot::AnnotateSurface surface(screen);
    resizeSurface(surface, kSurfaceWidth, kSurfaceHeight);
    const QVector<QRect> children = childAreas(surface);

    // --- The line. ---
    surface.setTool(vshot::AnnotateSurface::Tool::Line);
    surface.setPenWidth(kLineWidth);
    const QPoint lineStart(80, 160);
    const QPoint lineEnd(360, 260);
    drag(&surface, lineStart, lineEnd);
    expect("a line drag is one stroke", surface.strokeCount() == 1,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));

    QImage image = renderSurface(surface);
    expect("a line paints at its first point", isInk(image, lineStart));
    expect("a line paints at its last point", isInk(image, lineEnd));

    // Every inked pixel is within half the pen width of the segment, plus a
    // fringe for the antialiased edge: the ink is a capsule, not a wider shape.
    // The toolbar is skipped -- its red swatch is the same colour as the ink.
    int outsideCapsule = 0;
    for (int y = 0; y < image.height(); ++y) {
        for (int x = 0; x < image.width(); ++x) {
            const QPoint at(x, y);
            bool inChild = false;
            for (const QRect &child : children) {
                if (child.contains(at)) {
                    inChild = true;
                    break;
                }
            }
            if (inChild || !isInk(image, at)) {
                continue;
            }
            if (distanceToSegment(QPointF(x, y), QPointF(lineStart), QPointF(lineEnd)) >
                kHalfWidth + 1.5) {
                ++outsideCapsule;
            }
        }
    }
    expect("a line's ink stays inside the capsule of its two points", outsideCapsule == 0,
           QStringLiteral("%1 px outside the capsule").arg(outsideCapsule));

    // strokeBounds, spelled out: the endpoints' own box grown by half the width
    // and a pixel -- the rect the surface repaints and the eraser reaches into.
    const QRect lineBounds = QRect(lineStart, lineEnd)
                                 .normalized()
                                 .adjusted(-(kHalfWidth + 1), -(kHalfWidth + 1), kHalfWidth + 1,
                                           kHalfWidth + 1);
    const int lineStray = strayInk(image, image.rect(), lineBounds, children);
    expect("a line paints nothing outside its bounds", lineStray == 0,
           QStringLiteral("%1 px outside").arg(lineStray));

    // --- The wave. ---
    surface.clear();
    surface.setTool(vshot::AnnotateSurface::Tool::Wave);
    surface.setPenWidth(kLineWidth);
    const QPoint waveStart(80, 300);
    const QPoint waveEnd(480, 300);
    drag(&surface, waveStart, waveEnd);
    expect("a wave drag is one stroke", surface.strokeCount() == 1,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));
    image = renderSurface(surface);

    const int top = waveStart.y() - kWaveAmplitude - kHalfWidth - 3;
    const int bottom = waveStart.y() + kWaveAmplitude + kHalfWidth + 3;
    // The vertical extent of the ink in column `x`; `false` when the column has
    // no ink in the band at all.
    const auto band = [&](int x, int *minY, int *maxY) {
        *minY = std::numeric_limits<int>::max();
        *maxY = std::numeric_limits<int>::min();
        for (int y = top; y <= bottom; ++y) {
            if (isInk(image, QPoint(x, y))) {
                *minY = std::min(*minY, y);
                *maxY = std::max(*maxY, y);
            }
        }
        return *minY <= *maxY;
    };

    int mostAbove = 0;
    int mostBelow = 0;
    int transitions = 0;
    int side = 0;
    for (int x = waveStart.x() - kHalfWidth; x <= waveEnd.x() + kHalfWidth; ++x) {
        int minY = 0;
        int maxY = 0;
        if (!band(x, &minY, &maxY)) {
            continue;
        }
        mostAbove = std::max(mostAbove, waveStart.y() - minY);
        mostBelow = std::max(mostBelow, maxY - waveStart.y());
        // "Off the line" means past the pen's own half width: a straight
        // segment would put its ink exactly that far and no further.
        int here = 0;
        if (minY <= waveStart.y() - (kHalfWidth + 2)) {
            here = -1;
        } else if (maxY >= waveStart.y() + (kHalfWidth + 2)) {
            here = 1;
        }
        if (here != 0 && here != side) {
            ++transitions;
            side = here;
        }
    }

    // The outermost ink is the crest on the centre line plus the pen's half
    // width.  That it *reaches* the amplitude is the property that proves the
    // box the surface repaints covers the crest: a box over the two endpoints
    // alone would clip it, and this would come up short.  The tolerance is the
    // one pixel the antialiased edge lands on: the sampled crest sits a fraction
    // of a pixel off the true peak, so the outermost row can fall either side of
    // the edge.
    const int reach = kWaveAmplitude + kHalfWidth;
    expect("the wave reaches the amplitude on both sides of the line",
           mostAbove >= reach - 1 && mostAbove <= reach + 1 && mostBelow >= reach - 1 &&
               mostBelow <= reach + 1,
           QStringLiteral("above=%1 below=%2, wanted %3").arg(mostAbove).arg(mostBelow).arg(reach));
    // It crosses the line again and again, which a straight segment never does.
    expect("the wave swings to both sides of the line", transitions >= 4,
           QStringLiteral("transitions=%1").arg(transitions));

    // The two ends sit on the line the drag made, which is what rounding the
    // cycle count buys: a free-running phase leaves the far end wherever the
    // sine happened to be, so the ink there -- and the band's centre -- would sit
    // well off the line.  The centre is allowed a few pixels' slack because the
    // wave is already sloping as it leaves the endpoint.
    int startMin = 0;
    int startMax = 0;
    int endMin = 0;
    int endMax = 0;
    band(waveStart.x(), &startMin, &startMax);
    band(waveEnd.x(), &endMin, &endMax);
    const double startCentre = (startMin + startMax) / 2.0;
    const double endCentre = (endMin + endMax) / 2.0;
    expect("the wave's two ends sit on the line it was dragged along",
           isInk(image, waveStart) && isInk(image, waveEnd) &&
               std::abs(startCentre - waveStart.y()) <= 3.0 &&
               std::abs(endCentre - waveEnd.y()) <= 3.0,
           QStringLiteral("start centre %1, end centre %2, line %3")
               .arg(startCentre)
               .arg(endCentre)
               .arg(waveStart.y()));

    const QRect waveBounds =
        QRect(QPoint(waveStart.x(), waveStart.y() - kWaveAmplitude),
              QPoint(waveEnd.x(), waveStart.y() + kWaveAmplitude))
            .normalized()
            .adjusted(-(kHalfWidth + 1), -(kHalfWidth + 1), kHalfWidth + 1, kHalfWidth + 1);
    const int waveStray = strayInk(image, image.rect(), waveBounds, children);
    expect("a wave paints nothing outside its bounds", waveStray == 0,
           QStringLiteral("%1 px outside").arg(waveStray));
}

void checkText(vshot::AnnotateSurface &surface)
{
    const QPoint at(60, 200);
    surface.setTool(vshot::AnnotateSurface::Tool::Text);
    press(&surface, at);
    release(&surface, at);

    QLineEdit *editor = surface.findChild<QLineEdit *>();
    expect("the text tool opens an editor", editor != nullptr);
    if (editor == nullptr) {
        return;
    }
    editor->setText(QStringLiteral("Vshot"));
    QKeyEvent enter(QEvent::KeyPress, Qt::Key_Return, Qt::NoModifier);
    QApplication::sendEvent(editor, &enter);
    QApplication::processEvents();

    const QImage image = renderSurface(surface);
    expect("a committed text lands on the canvas",
           hasInk(image, QRect(at.x() - 8, at.y() - 48, 260, 96)));
    expect("the text is one stroke", surface.strokeCount() == 1,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));
}

// The repaint region of every interactive step has to cover the pixels the step
// changed: a pixel it changed outside that region is one the surface would have
// left stale until something else repainted it.
void checkStepCoverage(QScreen *screen)
{
    // A pen drag: the press paints the first dot, every motion extends it.
    {
        vshot::AnnotateSurface surface(screen);
        surface.setGeometry(0, 0, kSurfaceWidth, kSurfaceHeight);
        surface.setTool(vshot::AnnotateSurface::Tool::Pen);
        surface.setPenWidth(6);
        expectStepCovered(surface, "a pen press repaints the dot it starts",
                          [&] { press(&surface, QPoint(40, 120)); });
        expectStepCovered(surface, "the pen's first motion repaints what it added",
                          [&] { moveTo(&surface, QPoint(80, 120)); });
        expectStepCovered(surface, "the pen's next motion repaints what it added",
                          [&] { moveTo(&surface, QPoint(140, 120)); });
        release(&surface, QPoint(140, 120));
    }

    // The eraser: a drag step drops a whole stroke out of the list.
    {
        vshot::AnnotateSurface surface(screen);
        surface.setGeometry(0, 0, kSurfaceWidth, kSurfaceHeight);
        surface.setTool(vshot::AnnotateSurface::Tool::Pen);
        surface.setPenWidth(6);
        drag(&surface, QPoint(40, 120), QPoint(200, 120));
        drag(&surface, QPoint(40, 220), QPoint(200, 220));
        surface.setTool(vshot::AnnotateSurface::Tool::Eraser);
        press(&surface, QPoint(120, 260));
        expectStepCovered(surface, "an eraser motion repaints only the stroke it takes",
                          [&] { moveTo(&surface, QPoint(120, 180)); });
        release(&surface, QPoint(120, 180));
    }

    // A rectangle: the drag grows the preview, the release commits a shape that
    // can be smaller than the preview it replaces.
    {
        vshot::AnnotateSurface surface(screen);
        surface.setGeometry(0, 0, kSurfaceWidth, kSurfaceHeight);
        surface.setTool(vshot::AnnotateSurface::Tool::Rect);
        surface.setPenWidth(6);
        press(&surface, QPoint(40, 40));
        expectStepCovered(surface, "a rect drag repaints the shape it grew",
                          [&] { moveTo(&surface, QPoint(160, 90)); });
        expectStepCovered(surface, "a shape release repaints the old preview and the new shape",
                          [&] { release(&surface, QPoint(120, 70)); });
    }

    // Escape drops the in-progress stroke, which lives only in the preview.
    {
        vshot::AnnotateSurface surface(screen);
        surface.setGeometry(0, 0, kSurfaceWidth, kSurfaceHeight);
        surface.setTool(vshot::AnnotateSurface::Tool::Pen);
        surface.setPenWidth(6);
        press(&surface, QPoint(40, 120));
        moveTo(&surface, QPoint(200, 120));
        expectStepCovered(surface, "Escape repaints the in-progress stroke it drops", [&] {
            QKeyEvent escape(QEvent::KeyPress, Qt::Key_Escape, Qt::NoModifier);
            QApplication::sendEvent(&surface, &escape);
        });
    }
}

// A stroke rasterizes once and a repaint that changes nothing reuses it;
// removing one stroke leaves every other stroke's ink cached.
void checkRasterReuse(QScreen *screen)
{
    vshot::AnnotateSurface surface(screen);
    surface.setGeometry(0, 0, kSurfaceWidth, kSurfaceHeight);
    surface.setTool(vshot::AnnotateSurface::Tool::Pen);
    surface.setPenWidth(6);
    const int rows[] = {120, 220, 320, 420};
    for (const int y : rows) {
        drag(&surface, QPoint(60, y), QPoint(220, y));
    }
    expect("four separate strokes were drawn", surface.strokeCount() == 4,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));

    // A stroke's ink is built lazily, on its first paint, so the count is only
    // meaningful after one render.
    renderSurface(surface);
    const int builds = surface.rasterBuilds();
    expect("four strokes rasterized once each", builds == 4,
           QStringLiteral("rasterBuilds=%1").arg(builds));

    // A repaint that changes nothing must not rebuild anything.
    renderSurface(surface);
    renderSurface(surface);
    expect("a repaint that changes nothing rebuilds nothing",
           surface.rasterBuilds() == builds,
           QStringLiteral("rasterBuilds=%1").arg(surface.rasterBuilds()));

    // Erase exactly one stroke, then repaint: the remaining three keep the ink
    // they already built, so the repaint adds nothing to the count.
    surface.setTool(vshot::AnnotateSurface::Tool::Eraser);
    drag(&surface, QPoint(120, 260), QPoint(120, 180));
    renderSurface(surface);
    expect("erasing removed one stroke", surface.strokeCount() == 3,
           QStringLiteral("strokeCount=%1").arg(surface.strokeCount()));
    expect("removing one stroke leaves the others' ink cached",
           surface.rasterBuilds() == builds,
           QStringLiteral("rasterBuilds=%1").arg(surface.rasterBuilds()));
}

} // namespace

int main(int argc, char **argv)
{
    QApplication app(argc, argv);
    vshot::initUiLanguage();

    QScreen *screen = app.primaryScreen();
    if (screen == nullptr) {
        std::printf("FAIL  no screen: run this with QT_QPA_PLATFORM=offscreen\n");
        return 1;
    }

    vshot::AnnotateSurface surface(screen);
    resizeSurface(surface, kSurfaceWidth, kSurfaceHeight);

    checkPenAndRect(surface);
    checkEraser(surface);
    checkUndoRedoClear(surface);
    checkToolbar(surface);
    checkToolbarWrapping(screen);
    checkLineAndWave(screen);

    checkStepCoverage(screen);
    checkRasterReuse(screen);

    vshot::AnnotateSurface text(screen);
    resizeSurface(text, kSurfaceWidth, kSurfaceHeight);
    checkText(text);

    if (failures == 0) {
        std::printf("all annotation checks passed\n");
        return 0;
    }
    std::printf("%d annotation check(s) failed\n", failures);
    return 1;
}
