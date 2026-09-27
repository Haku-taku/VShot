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
#include <QScreen>
#include <QString>
#include <QWidget>

#include <cstdio>

namespace {

int failures = 0;

// The colour the default pen draws in, spelled out here on purpose: a check
// that imported the surface's own constant would pass for any value at all.
constexpr int kInkR = 229;
constexpr int kInkG = 57;
constexpr int kInkB = 53; // 0x35: the default red is #e53935

// The surface is output-sized, and deliberately wider than the toolbar: the
// default placement below is checked against a surface the palette really fits
// in, the way every output a user has does.
constexpr int kSurfaceWidth = 800;
constexpr int kSurfaceHeight = 600;

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

void checkToolbar(vshot::AnnotateSurface &surface)
{
    const QRect bar = surface.toolbarRect();
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
    surface.setGeometry(0, 0, kSurfaceWidth, kSurfaceHeight);

    checkPenAndRect(surface);
    checkEraser(surface);
    checkUndoRedoClear(surface);
    checkToolbar(surface);

    vshot::AnnotateSurface text(screen);
    text.setGeometry(0, 0, kSurfaceWidth, kSurfaceHeight);
    checkText(text);

    if (failures == 0) {
        std::printf("all annotation checks passed\n");
        return 0;
    }
    std::printf("%d annotation check(s) failed\n", failures);
    return 1;
}
