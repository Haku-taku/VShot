// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline checks for the annotation render cache and the freehand preview.
//
// Every committed mark keeps a rasterized copy of itself and redraws it only
// when something it draws changes.  That matters most for the mosaic, which
// averages the source image block by block: recomputing it on every repaint
// (a selection drag, a pointer move) is what made a busy capture stutter.  The
// check paints the real overlay offscreen and reads the per-mark rebuild count,
// so what is asserted is the cache the editor actually uses.
//
// The in-progress freehand stroke has the same problem in a different shape:
// re-stroking the whole path on every move is quadratic over a long scribble.
// It builds up through a raster that only grows by the points added since the
// last paint, and the check proves each segment is baked once while the result
// still matches the mark that is committed on release.
//
// Needs QApplication and the offscreen platform plugin; no compositor and no
// layer shell.  `QT_QPA_PLATFORM=offscreen` supplies the one screen the overlay
// is parented to.
//
// Built only with `-DVSHOT_BUILD_CHECKS=ON`; see the README's verification
// section.

#include "capture_overlay.hpp"

#include <QApplication>
#include <QColor>
#include <QImage>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLineEdit>
#include <QPointF>
#include <QRegion>
#include <QScreen>
#include <QString>
#include <Qt>

#include <algorithm>
#include <cmath>
#include <cstdio>

namespace {

int failures = 0;

void expect(bool condition, const char *what, const QString &detail = QString())
{
    if (condition) {
        std::printf("ok    %s\n", what);
        return;
    }
    ++failures;
    if (detail.isEmpty()) {
        std::printf("FAIL  %s\n", what);
    } else {
        std::printf("FAIL  %s -- %s\n", what, qPrintable(detail));
    }
}

// A one-output region session whose selection is already made, so the editor
// opens with the toolbar up.  The output carries real pixels: the mosaic reads
// them, so an empty frame would cache an empty raster and prove nothing.
vshot::Session editingSession()
{
    vshot::Session session;
    session.mode = QStringLiteral("region");
    session.bounds = vshot::LogicalRect{0, 0, 400, 400};
    vshot::OutputSession output;
    output.id = 1;
    output.name = QStringLiteral("CHECK-1");
    output.geometry = vshot::LogicalRect{0, 0, 400, 400};
    output.surface = output.geometry;
    output.scale = 1;
    output.pixelWidth = 400;
    output.pixelHeight = 400;
    output.image = QImage(400, 400, QImage::Format_ARGB32);
    // A coarse pattern rather than a flat fill, so the mosaic and the brush have
    // something to average: a uniform frame would hide a wrong sample point.
    for (int y = 0; y < 400; ++y) {
        for (int x = 0; x < 400; ++x) {
            output.image.setPixelColor(x, y,
                                       QColor(40 + (x / 8 * 7) % 60, 30 + (y / 8 * 53) % 200,
                                              30 + ((x + y) / 8 * 29) % 200));
        }
    }
    session.outputs.push_back(output);
    session.selection = vshot::LogicalRect{0, 0, 400, 400};
    return session;
}

void paintOnce(vshot::CaptureOverlay *overlay, QImage *target)
{
    target->fill(Qt::transparent);
    overlay->render(target);
}

// Draws a rectangle drag with the current tool.
void drag(vshot::OverlayController &controller, vshot::CaptureOverlay *overlay,
          const QPointF &from, const QPointF &to)
{
    controller.press(overlay, from, Qt::LeftButton, Qt::NoModifier);
    controller.move(overlay, to, Qt::LeftButton, Qt::NoModifier);
    controller.release(overlay, to, Qt::LeftButton, Qt::NoModifier);
}

// The area the overlay's live child widgets cover, in overlay coordinates.  The
// floating toolbar and its buttons repaint themselves when their own state
// changes -- Qt drives that, not the controller's invalidated region -- so a
// difference under a child is not something the controller's rect can be blamed
// for.
QRegion childAreas(vshot::CaptureOverlay *overlay)
{
    QRegion areas;
    for (QWidget *child : overlay->findChildren<QWidget *>()) {
        if (child->isWindow() || !child->isVisible()) {
            continue;
        }
        areas += QRect(child->mapTo(overlay, QPoint()), child->size());
    }
    return areas;
}

// One interactive step: full-render the overlay, run the step, full-render it
// again, and prove every pixel the step changed lies inside the rect the step
// asked to be repainted.  A null rect means the step repainted the whole
// surface, so there is nothing to compare -- a full repaint is its own eraser
// and needs no narrow region to be correct.
void expectStepCovered(vshot::OverlayController &controller, vshot::CaptureOverlay *overlay,
                       const QPointF &to, const char *gesture)
{
    QImage before(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    paintOnce(overlay, &before);
    controller.move(overlay, to, Qt::LeftButton, Qt::NoModifier);
    QImage after(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    paintOnce(overlay, &after);
    const QRect claimed = controller.lastInteractiveUpdate();
    if (claimed.isNull()) {
        return;
    }
    const QRegion allowed = QRegion(claimed) + childAreas(overlay);
    int outside = 0;
    for (int y = 0; y < after.height(); ++y) {
        for (int x = 0; x < after.width(); ++x) {
            if (before.pixel(x, y) != after.pixel(x, y) && !allowed.contains(QPoint(x, y))) {
                ++outside;
            }
        }
    }
    expect(outside == 0, gesture,
           QStringLiteral("%1 px changed outside the invalidated region").arg(outside));
}

// A committed mark rasterizes once and a repaint that changes nothing reuses
// it; moving it or restyling it is what rebuilds it.
void checkRepaintsReuseTheRaster()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(editingSession());
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();

    controller.chooseTool(vshot::Tool::Mosaic);
    drag(controller, overlay, QPointF(40, 40), QPointF(180, 140));
    expect(controller.annotations().size() == 1, "the mosaic lands as one annotation");
    if (controller.annotations().size() != 1) {
        return;
    }

    QImage target(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    paintOnce(overlay, &target);
    expect(controller.annotations().at(0).rasterRebuilds() == 1,
           "the first paint rasterizes the mark",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(0).rasterRebuilds()));
    paintOnce(overlay, &target);
    paintOnce(overlay, &target);
    expect(controller.annotations().at(0).rasterRebuilds() == 1,
           "repaints that change nothing reuse the cached raster",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(0).rasterRebuilds()));

    // Moving the mark changes the source blocks it averages, so it must redraw.
    controller.chooseTool(vshot::Tool::Select);
    drag(controller, overlay, QPointF(110, 90), QPointF(150, 115));
    paintOnce(overlay, &target);
    expect(controller.annotations().at(0).rasterRebuilds() == 2,
           "moving the mark rebuilds its raster",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(0).rasterRebuilds()));
    paintOnce(overlay, &target);
    expect(controller.annotations().at(0).rasterRebuilds() == 2,
           "the moved mark then settles into the cache",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(0).rasterRebuilds()));

    // Restyling the selected mark changes what it draws, so it must redraw too.
    const std::uint32_t strength = controller.annotations().at(0).strength;
    controller.setMosaicStrength(strength == 3u ? 1u : 3u);
    paintOnce(overlay, &target);
    expect(controller.annotations().at(0).rasterRebuilds() == 3,
           "a style change rebuilds the raster",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(0).rasterRebuilds()));
}

// Each mark owns its own cache: drawing a second mark leaves the first one's
// raster alone.
void checkEachMarkCachesOnItsOwn()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(editingSession());
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();

    controller.chooseTool(vshot::Tool::Mosaic);
    drag(controller, overlay, QPointF(40, 40), QPointF(140, 110));
    QImage target(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    paintOnce(overlay, &target);
    expect(controller.annotations().size() == 1 &&
               controller.annotations().at(0).rasterRebuilds() == 1,
           "the first mark rasterizes once");

    controller.chooseTool(vshot::Tool::Pen);
    drag(controller, overlay, QPointF(220, 220), QPointF(300, 280));
    expect(controller.annotations().size() == 2, "the pen stroke lands as a second annotation");
    if (controller.annotations().size() != 2) {
        return;
    }
    paintOnce(overlay, &target);
    expect(controller.annotations().at(0).rasterRebuilds() == 1,
           "drawing a second mark leaves the first one's raster cached");
    expect(controller.annotations().at(1).rasterRebuilds() == 1,
           "the new mark rasterizes on its own");
    paintOnce(overlay, &target);
    expect(controller.annotations().at(0).rasterRebuilds() == 1 &&
               controller.annotations().at(1).rasterRebuilds() == 1,
           "both marks stay cached across further repaints");
}

// A cached raster still lands where the mark is: painting a bright stroke over
// a plain frame leaves bright pixels on the stroke, both on the paint that
// rasterizes it and on the repaint that blits the cache.
void checkCachedPixelsLandOnTheMark()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(editingSession());
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();

    controller.chooseTool(vshot::Tool::Pen);
    controller.setWidth(6);
    controller.setCurrentColor(QColor(255, 30, 30));
    drag(controller, overlay, QPointF(60, 200), QPointF(340, 200));
    const auto hasStrokePixels = [](const QImage &image) {
        for (int y = 180; y < 220; ++y) {
            for (int x = 40; x < 360; ++x) {
                if (x >= image.width() || y >= image.height()) {
                    continue;
                }
                const QColor pixel = image.pixelColor(x, y);
                if (pixel.red() > 180 && pixel.green() < 120 && pixel.blue() < 120) {
                    return true;
                }
            }
        }
        return false;
    };
    QImage target(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    paintOnce(overlay, &target);
    expect(hasStrokePixels(target), "the rasterized stroke paints where it was drawn");
    paintOnce(overlay, &target);
    expect(hasStrokePixels(target), "the cached stroke blits to the same place");
}

// Opens an overlay on `screen` in edit state and draws one pen stroke with the
// given colour, so both the mark's own colour and the document's spelling of it
// can be read back.  Returns false when the stroke did not land.
bool drawStrokeWithColor(vshot::OverlayController &controller, vshot::CaptureOverlay **overlay,
                         QScreen *screen, const QColor &color)
{
    QString error;
    *overlay = controller.addOverlay(0, screen, &error);
    if (*overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return false;
    }
    (*overlay)->show();
    controller.beginPresetEdit();
    controller.chooseTool(vshot::Tool::Pen);
    controller.setWidth(6);
    controller.setCurrentColor(color);
    drag(controller, *overlay, QPointF(60, 120), QPointF(340, 120));
    expect(controller.annotations().size() == 1, "the pen stroke lands as one annotation");
    return controller.annotations().size() == 1;
}

// The colour the picker hands the editor has to survive into the committed mark
// and out through the result document.  Every place that writes an annotation's
// colour into that document goes through `colorText`, and the document is the
// only thing the renderer ever sees -- so a colour that lost its alpha between
// the picker and the JSON is invisible on screen and only shows up as a solid
// mark in the output PNG.  The exact string is pinned because the Rust reader
// reads `#rrggbbaa` with the alpha last: a channel dropped, swapped or put in
// the wrong place changes which colour comes out.
void checkTranslucentColorSerializesWithAlpha()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(editingSession());
    vshot::CaptureOverlay *overlay = nullptr;
    // A colour with no two channels equal and an alpha that is neither 0 nor
    // 255, so no single dropped or reordered channel can hide in the string.
    if (!drawStrokeWithColor(controller, &overlay, screen, QColor(17, 34, 204, 128))) {
        return;
    }
    const vshot::Annotation &mark = controller.annotations().at(0);
    expect(mark.color.alpha() == 128,
           "the committed stroke keeps the alpha the current colour carried",
           QStringLiteral("alpha=%1").arg(mark.color.alpha()));

    const QJsonDocument document = controller.resultDocument();
    const QJsonArray annotations =
        document.object().value(QStringLiteral("annotations")).toArray();
    expect(annotations.size() == 1, "the document carries the stroke");
    if (annotations.isEmpty()) {
        return;
    }
    const QString color = annotations.at(0).toObject().value(QStringLiteral("color")).toString();
    expect(color == QStringLiteral("#1122cc80"),
           "a translucent colour serializes as #rrggbbaa, alpha in the last two digits",
           QStringLiteral("got %1").arg(color));
}

// The other half of the same contract: an opaque colour keeps the six-digit
// spelling, so adding support for alpha did not turn every colour in the
// document into eight digits.
void checkOpaqueColorSerializesWithoutAlpha()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(editingSession());
    vshot::CaptureOverlay *overlay = nullptr;
    if (!drawStrokeWithColor(controller, &overlay, screen, QColor(17, 34, 204, 255))) {
        return;
    }
    expect(controller.annotations().at(0).color.alpha() == 255,
           "an opaque stroke stays fully opaque");

    const QJsonDocument document = controller.resultDocument();
    const QJsonArray annotations =
        document.object().value(QStringLiteral("annotations")).toArray();
    if (annotations.isEmpty()) {
        expect(false, "the document carries the stroke");
        return;
    }
    const QString color = annotations.at(0).toObject().value(QStringLiteral("color")).toString();
    expect(color == QStringLiteral("#1122cc"),
           "an opaque colour serializes as #rrggbb, not eight digits",
           QStringLiteral("got %1").arg(color));
}

// A serpentine of many moves, so the live raster is baked in many steps.
QVector<QPointF> serpentine()
{
    QVector<QPointF> path;
    for (int i = 0; i < 80; ++i) {
        const double t = i / 79.0;
        path.append(QPointF(30 + t * 340, 200 + std::sin(t * 18.0) * 70));
    }
    return path;
}

// Paints the stroke as the editor does -- one paint per move -- and keeps both
// the in-progress image and the committed one.
void renderFreehand(vshot::OverlayController &controller, vshot::CaptureOverlay *overlay,
                    const QVector<QPointF> &path, QImage *live, QImage *committed)
{
    *live = QImage(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    controller.press(overlay, path.constFirst(), Qt::LeftButton, Qt::NoModifier);
    for (int i = 1; i < path.size(); ++i) {
        controller.move(overlay, path.at(i), Qt::LeftButton, Qt::NoModifier);
        paintOnce(overlay, live);
    }
    controller.release(overlay, path.constLast(), Qt::LeftButton, Qt::NoModifier);
    *committed = QImage(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    paintOnce(overlay, committed);
}

int differingPixels(const QImage &first, const QImage &second)
{
    int diff = 0;
    for (int y = 0; y < first.height(); ++y) {
        for (int x = 0; x < first.width(); ++x) {
            const QColor a = first.pixelColor(x, y);
            const QColor b = second.pixelColor(x, y);
            if (std::max({std::abs(a.red() - b.red()), std::abs(a.green() - b.green()),
                          std::abs(a.blue() - b.blue())}) > 30) {
                ++diff;
            }
        }
    }
    return diff;
}

// The incremental preview must draw the same stroke as the committed mark, so
// letting go changes nothing on screen.  The live raster is baked one move at a
// time; only antialiasing at the shared joints differs, so the tolerance is a
// few pixels per vertex rather than none.
void checkLiveStrokeMatchesTheCommittedMark()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    const QVector<QPointF> path = serpentine();
    const int tolerance = 8 * path.size();

    const auto open = [&](vshot::OverlayController &controller, vshot::CaptureOverlay **overlay) {
        QString error;
        *overlay = controller.addOverlay(0, screen, &error);
        if (*overlay == nullptr) {
            expect(false, "the controller accepts an overlay", error);
            return false;
        }
        (*overlay)->show();
        controller.beginPresetEdit();
        return true;
    };

    {
        vshot::OverlayController controller(editingSession());
        vshot::CaptureOverlay *overlay = nullptr;
        if (!open(controller, &overlay)) {
            return;
        }
        controller.chooseTool(vshot::Tool::Pen);
        controller.setWidth(5);
        controller.setCurrentColor(QColor(255, 30, 30));
        QImage live;
        QImage committed;
        renderFreehand(controller, overlay, path, &live, &committed);
        const int diff = differingPixels(live, committed);
        expect(diff < tolerance, "the incremental preview matches the committed solid stroke",
               QStringLiteral("%1 pixels differ").arg(diff));
        expect(controller.liveStrokeBakes() == path.size() - 1,
               "each freehand segment is baked once, not on every paint",
               QStringLiteral("baked %1 for %2 segments")
                   .arg(controller.liveStrokeBakes())
                   .arg(path.size() - 1));
    }

    {
        vshot::OverlayController controller(editingSession());
        vshot::CaptureOverlay *overlay = nullptr;
        if (!open(controller, &overlay)) {
            return;
        }
        controller.chooseTool(vshot::Tool::Pen);
        controller.setWidth(5);
        controller.setCurrentColor(QColor(255, 30, 30));
        controller.setDash(QStringLiteral("dashed"));
        QImage live;
        QImage committed;
        renderFreehand(controller, overlay, path, &live, &committed);
        const int diff = differingPixels(live, committed);
        expect(diff < tolerance, "the incremental preview matches the committed dashed stroke",
               QStringLiteral("%1 pixels differ").arg(diff));
    }

    {
        vshot::OverlayController controller(editingSession());
        vshot::CaptureOverlay *overlay = nullptr;
        if (!open(controller, &overlay)) {
            return;
        }
        controller.chooseTool(vshot::Tool::Mosaic);
        controller.setMosaicShape(QStringLiteral("brush"));
        controller.setWidth(24);
        QImage live;
        QImage committed;
        renderFreehand(controller, overlay, path, &live, &committed);
        const int diff = differingPixels(live, committed);
        expect(diff < tolerance, "the incremental preview matches the committed mosaic brush",
               QStringLiteral("%1 pixels differ").arg(diff));
    }
}

// A pure translation must not invalidate a cached raster: the mark's pixels do
// not change, only where they are blitted.  The mosaic is the deliberate
// exception -- it averages the source image under its absolute position -- and
// checkRepaintsReuseTheRaster keeps that pinned down.  A moved mark that kept a
// stale blit position would show up here as pixels left at the old place.
void checkPureMoveReusesTheRaster()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(editingSession());
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();

    QImage target(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    // The session frame is a coarse coloured pattern that can itself contain
    // red-ish pixels, so the stroke uses pure green -- a channel combination
    // the pattern (red >= 40, blue >= 30) can never produce -- and the position
    // test looks for that.
    const auto greenIn = [](const QImage &image, int from, int to) {
        for (int y = from; y < to; ++y) {
            for (int x = 40; x < 360; ++x) {
                if (x >= image.width() || y >= image.height()) {
                    continue;
                }
                const QColor pixel = image.pixelColor(x, y);
                if (pixel.red() < 20 && pixel.green() > 200 && pixel.blue() < 20) {
                    return true;
                }
            }
        }
        return false;
    };

    // A freehand stroke: it must keep its raster when dragged, and the pixels
    // must land at the new place rather than staying behind.  The press point
    // is off the mark's edge handles so the drag translates instead of resizing.
    controller.chooseTool(vshot::Tool::Pen);
    controller.setWidth(6);
    controller.setCurrentColor(QColor(0, 255, 0));
    drag(controller, overlay, QPointF(60, 120), QPointF(340, 120));
    expect(controller.annotations().size() == 1, "the stroke lands as one annotation");
    if (controller.annotations().size() != 1) {
        return;
    }
    paintOnce(overlay, &target);
    const int strokeRebuilds = controller.annotations().at(0).rasterRebuilds();
    expect(strokeRebuilds == 1, "the stroke rasterizes once",
           QStringLiteral("rebuilds=%1").arg(strokeRebuilds));
    controller.chooseTool(vshot::Tool::Select);
    drag(controller, overlay, QPointF(150, 120), QPointF(150, 220));
    paintOnce(overlay, &target);
    expect(controller.annotations().at(0).rasterRebuilds() == strokeRebuilds,
           "translating a freehand stroke reuses its cached raster",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(0).rasterRebuilds()));
    expect(greenIn(target, 200, 240) && !greenIn(target, 100, 140),
           "the translated stroke paints at its new place, not the old one");

    // A rectangle outline: same cache, same translation.
    controller.chooseTool(vshot::Tool::Rectangle);
    controller.setWidth(4);
    controller.setCurrentColor(QColor(30, 200, 30));
    drag(controller, overlay, QPointF(40, 300), QPointF(180, 380));
    expect(controller.annotations().size() == 2, "the rectangle lands as a second annotation");
    if (controller.annotations().size() != 2) {
        return;
    }
    paintOnce(overlay, &target);
    const int rectRebuilds = controller.annotations().at(1).rasterRebuilds();
    expect(rectRebuilds == 1, "the rectangle rasterizes once",
           QStringLiteral("rebuilds=%1").arg(rectRebuilds));
    controller.chooseTool(vshot::Tool::Select);
    drag(controller, overlay, QPointF(110, 340), QPointF(210, 340));
    paintOnce(overlay, &target);
    expect(controller.annotations().at(1).rasterRebuilds() == rectRebuilds,
           "translating a rectangle reuses its cached raster",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(1).rasterRebuilds()));

    // A text label: its bitmap depends on the text and font, not on where the
    // label sits, so a move reuses it too.
    controller.chooseTool(vshot::Tool::Text);
    controller.press(overlay, QPointF(60, 60), Qt::LeftButton, Qt::NoModifier);
    QLineEdit *editor = overlay->findChild<QLineEdit *>();
    expect(editor != nullptr, "the text tool opens its inline editor");
    if (editor == nullptr) {
        return;
    }
    editor->setText(QStringLiteral("Hi"));
    controller.key(overlay, Qt::Key_Return, Qt::NoModifier);
    expect(controller.annotations().size() == 3, "the label lands as a third annotation");
    if (controller.annotations().size() != 3) {
        return;
    }
    paintOnce(overlay, &target);
    const int textRebuilds = controller.annotations().at(2).rasterRebuilds();
    expect(textRebuilds == 1, "the label rasterizes once",
           QStringLiteral("rebuilds=%1").arg(textRebuilds));
    controller.chooseTool(vshot::Tool::Select);
    drag(controller, overlay, QPointF(66, 66), QPointF(166, 66));
    paintOnce(overlay, &target);
    expect(controller.annotations().at(2).rasterRebuilds() == textRebuilds,
           "translating a label reuses its cached raster",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(2).rasterRebuilds()));
}

// Two outputs side by side, each with its own frozen frame, so a mark can lie
// across the seam and be painted by both overlays.
vshot::Session twoOutputSession()
{
    vshot::Session session;
    session.mode = QStringLiteral("region");
    session.bounds = vshot::LogicalRect{0, 0, 800, 400};
    for (int index = 0; index < 2; ++index) {
        vshot::OutputSession output;
        output.id = static_cast<std::uint32_t>(index + 1);
        output.name = QStringLiteral("CHECK-%1").arg(index + 1);
        output.geometry = vshot::LogicalRect{index * 400, 0, 400, 400};
        output.surface = output.geometry;
        output.scale = 1;
        output.pixelWidth = 400;
        output.pixelHeight = 400;
        output.image = QImage(400, 400, QImage::Format_ARGB32);
        output.image.fill(QColor(80, 90, 100));
        session.outputs.push_back(output);
    }
    session.selection = vshot::LogicalRect{0, 0, 800, 400};
    return session;
}

// A session that spans two screens paints the same marks on both.  A single
// shared raster would be thrown away and rebuilt every time the paint moved from
// one screen to the other, so each output keeps its own: the second screen
// builds once, and both then stay cached however the repaints alternate.
void checkEachOutputKeepsItsOwnRaster()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(twoOutputSession());
    QString error;
    vshot::CaptureOverlay *first = controller.addOverlay(0, screen, &error);
    vshot::CaptureOverlay *second = controller.addOverlay(1, screen, &error);
    if (first == nullptr || second == nullptr) {
        expect(false, "the controller accepts two overlays", error);
        return;
    }
    first->show();
    second->show();
    controller.beginPresetEdit();

    controller.chooseTool(vshot::Tool::Rectangle);
    controller.setWidth(4);
    controller.setCurrentColor(QColor(255, 30, 30));
    // Straddles the seam at x = 400, so both screens paint it.
    drag(controller, first, QPointF(300, 100), QPointF(500, 300));
    expect(controller.annotations().size() == 1,
           "the straddling rectangle lands as one annotation");
    if (controller.annotations().size() != 1) {
        return;
    }

    QImage firstTarget(first->size(), QImage::Format_ARGB32_Premultiplied);
    QImage secondTarget(second->size(), QImage::Format_ARGB32_Premultiplied);
    paintOnce(first, &firstTarget);
    paintOnce(second, &secondTarget);
    const int afterBoth = controller.annotations().at(0).rasterRebuilds();
    expect(afterBoth == 2, "each output rasterizes the mark once",
           QStringLiteral("rebuilds=%1").arg(afterBoth));

    // Alternating repaints must not throw either screen's raster away.
    paintOnce(first, &firstTarget);
    paintOnce(second, &secondTarget);
    paintOnce(first, &firstTarget);
    paintOnce(second, &secondTarget);
    expect(controller.annotations().at(0).rasterRebuilds() == afterBoth,
           "repainting both screens keeps both rasters",
           QStringLiteral("rebuilds=%1").arg(controller.annotations().at(0).rasterRebuilds()));
}

// A single roomy output: the edge test has to drag a mark right up against the
// canvas boundary, and a 400x400 session clamps the drag before it gets there.
vshot::Session largeSession()
{
    vshot::Session session;
    session.mode = QStringLiteral("region");
    session.bounds = vshot::LogicalRect{0, 0, 1600, 1200};
    vshot::OutputSession output;
    output.id = 1;
    output.name = QStringLiteral("CHECK-LARGE");
    output.geometry = vshot::LogicalRect{0, 0, 1600, 1200};
    output.surface = output.geometry;
    output.scale = 1;
    output.pixelWidth = 1600;
    output.pixelHeight = 1200;
    output.image = QImage(1600, 1200, QImage::Format_ARGB32);
    output.image.fill(QColor(80, 90, 100));
    session.outputs.push_back(output);
    session.selection = vshot::LogicalRect{0, 0, 1600, 1200};
    return session;
}

// Sliding a mark up against the edge of the canvas changes how much of it is
// visible, not the pixels it draws, so the raster is not rebuilt: the blit is
// clipped by the painter instead.  Trimming the raster to the canvas would
// change its size as the mark reached the edge, and a size change rebuilds it.
void checkEdgeOfCanvasKeepsTheRaster()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(largeSession());
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();

    QImage target(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    controller.chooseTool(vshot::Tool::Rectangle);
    controller.setWidth(4);
    controller.setCurrentColor(QColor(0, 255, 0));
    // A 70x70 outline landing 30 pixels short of the corner: one drag of thirty
    // brings its far edges exactly onto the canvas boundary, where the pen's half
    // width and the raster's padding reach past it.
    drag(controller, overlay, QPointF(1500, 1100), QPointF(1569, 1169));
    expect(controller.annotations().size() == 1, "the rectangle lands as one annotation");
    if (controller.annotations().size() != 1) {
        return;
    }
    paintOnce(overlay, &target);
    const int settled = controller.annotations().at(0).rasterRebuilds();
    expect(settled == 1, "the rectangle rasterizes once",
           QStringLiteral("rebuilds=%1").arg(settled));

    controller.chooseTool(vshot::Tool::Select);
    const vshot::LogicalRect before = controller.annotations().at(0).rect;
    drag(controller, overlay, QPointF(1535, 1135), QPointF(1565, 1165));
    // Back to the drawing tool: the select tool's white outline and handles sit
    // exactly on the edges the colour probes below look at.
    controller.chooseTool(vshot::Tool::Rectangle);
    paintOnce(overlay, &target);
    const vshot::Annotation &mark = controller.annotations().at(0);
    expect(mark.rect.x == before.x + 30 && mark.rect.y == before.y + 30 &&
               mark.rect.width == before.width && mark.rect.height == before.height,
           "the drag moved the mark thirty pixels and changed nothing else",
           QStringLiteral("before=(%1,%2 %3x%4) after=(%5,%6 %7x%8)")
               .arg(before.x)
               .arg(before.y)
               .arg(before.width)
               .arg(before.height)
               .arg(mark.rect.x)
               .arg(mark.rect.y)
               .arg(mark.rect.width)
               .arg(mark.rect.height));
    expect(mark.rect.x + static_cast<std::int32_t>(mark.rect.width) >= 1600,
           "the mark ends up against the canvas edge");
    expect(mark.rasterRebuilds() == settled,
           "a mark pushed against the canvas edge keeps its raster",
           QStringLiteral("rebuilds=%1").arg(mark.rasterRebuilds()));

    // The raster is not trimmed to the canvas, so this is what proves the blit
    // still lands where the mark is: the drag moved it, and its near edges have
    // to be painted at the place it moved to.  The session frame is a flat grey,
    // so a green pixel is the mark's own.
    const auto greenAt = [&target](int x, int y) {
        if (x < 0 || y < 0 || x >= target.width() || y >= target.height()) {
            return false;
        }
        const QColor pixel = target.pixelColor(x, y);
        return pixel.red() < 40 && pixel.green() > 150 && pixel.blue() < 40;
    };
    expect(greenAt(mark.rect.x + 20, mark.rect.y) && greenAt(mark.rect.x, mark.rect.y + 20),
           "the edges of the mark are painted where it now is",
           QStringLiteral("top=%1 left=%2 at (%3,%4)")
               .arg(greenAt(mark.rect.x + 20, mark.rect.y) ? 1 : 0)
               .arg(greenAt(mark.rect.x, mark.rect.y + 20) ? 1 : 0)
               .arg(mark.rect.x)
               .arg(mark.rect.y));
    expect(!greenAt(before.x, before.y + 20) && !greenAt(before.x + 20, before.y),
           "nothing is left where the mark came from");
}

// The narrow repaints must still leave the screen correct: whatever an
// interactive step changed has to lie inside the rect that step asked to be
// repainted.  A rect that is too small leaves stale pixels behind -- a mark at
// the place it came from, a magnifier that outlived the gesture.  The check does
// not model Qt's backing store; it compares a full render of the overlay before
// a step with one after it and counts the changed pixels the invalidated region
// does not cover.
void checkInteractiveUpdateCoversTheChange()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(editingSession());
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();

    // 1. Drawing a two-point preview: the rectangle is redrawn whole from its
    // anchor on every move, so a step has to cover the outline it drew before as
    // well as the one it draws now.
    controller.chooseTool(vshot::Tool::Rectangle);
    controller.press(overlay, QPointF(60, 60), Qt::LeftButton, Qt::NoModifier);
    expectStepCovered(controller, overlay, QPointF(140, 120),
                      "a rectangle preview move invalidates where it drew");
    expectStepCovered(controller, overlay, QPointF(160, 160),
                      "a growing rectangle preview invalidates where it drew");
    expectStepCovered(controller, overlay, QPointF(150, 140),
                      "a shrinking rectangle preview invalidates where it drew");
    controller.release(overlay, QPointF(150, 140), Qt::LeftButton, Qt::NoModifier);
    expect(controller.annotations().size() == 1, "the rectangle lands as one annotation");
    if (controller.annotations().size() != 1) {
        return;
    }

    // 2. Moving a committed mark: the mark, its white selection chrome and the
    // magnifier all travel, so each step has to cover the old and the new place
    // of all three.
    controller.chooseTool(vshot::Tool::Select);
    const vshot::LogicalRect drawn = controller.annotations().at(0).rect;
    const QPointF centre(drawn.x + static_cast<int>(drawn.width) / 2,
                         drawn.y + static_cast<int>(drawn.height) / 2);
    controller.press(overlay, centre, Qt::LeftButton, Qt::NoModifier);
    expectStepCovered(controller, overlay, centre + QPointF(10, 8),
                      "moving a mark invalidates where it drew");
    expectStepCovered(controller, overlay, centre + QPointF(24, 20),
                      "a moving mark invalidates where it draws");
    expectStepCovered(controller, overlay, centre + QPointF(30, 30),
                      "the moved mark invalidates its last place");
    controller.release(overlay, centre + QPointF(30, 30), Qt::LeftButton, Qt::NoModifier);
    const vshot::LogicalRect moved = controller.annotations().at(0).rect;
    expect(moved.x == drawn.x + 30 && moved.y == drawn.y + 30 &&
               moved.width == drawn.width && moved.height == drawn.height,
           "the drag translated the mark by thirty pixels",
           QStringLiteral("from (%1,%2) to (%3,%4)")
               .arg(drawn.x)
               .arg(drawn.y)
               .arg(moved.x)
               .arg(moved.y));

    // 3. Resizing a committed mark: press on the bottom-right handle so the
    // gesture resizes rather than moves, and cover the chrome at both sizes.
    const QPointF corner(moved.x + static_cast<int>(moved.width) - 1,
                         moved.y + static_cast<int>(moved.height) - 1);
    controller.press(overlay, corner, Qt::LeftButton, Qt::NoModifier);
    expectStepCovered(controller, overlay, corner + QPointF(10, 10),
                      "resizing a mark invalidates where it drew");
    expectStepCovered(controller, overlay, corner + QPointF(20, 20),
                      "a resizing mark invalidates where it draws");
    controller.release(overlay, corner + QPointF(20, 20), Qt::LeftButton, Qt::NoModifier);
    const vshot::LogicalRect resized = controller.annotations().at(0).rect;
    expect(resized.width > moved.width && resized.height > moved.height,
           "the drag resized the mark instead of moving it",
           QStringLiteral("%1x%2 -> %3x%4")
               .arg(moved.width)
               .arg(moved.height)
               .arg(resized.width)
               .arg(resized.height));

    // 4. A freehand pen stroke: the preview grows through a raster one segment
    // at a time, and a step may only invalidate the segment it just added.
    controller.chooseTool(vshot::Tool::Pen);
    controller.setWidth(5);
    controller.press(overlay, QPointF(40, 250), Qt::LeftButton, Qt::NoModifier);
    expectStepCovered(controller, overlay, QPointF(80, 255),
                      "a pen stroke move invalidates where it drew");
    expectStepCovered(controller, overlay, QPointF(120, 245),
                      "a growing pen stroke invalidates where it drew");
    expectStepCovered(controller, overlay, QPointF(160, 262),
                      "a turning pen stroke invalidates where it drew");
    expectStepCovered(controller, overlay, QPointF(200, 250),
                      "the released pen stroke invalidates where it drew");
    controller.release(overlay, QPointF(200, 250), Qt::LeftButton, Qt::NoModifier);
    expect(controller.annotations().size() == 2, "the pen stroke lands as a second annotation");
    expect(controller.annotations().size() == 2 &&
               controller.annotations().at(1).tool == QStringLiteral("pen"),
           "the second mark is the pen stroke");

    // 5. A mosaic brush stroke: the growing-raster path again, but each step
    // smears a disc whose radius comes from the strength, not the cursor.
    controller.chooseTool(vshot::Tool::Mosaic);
    controller.setMosaicShape(QStringLiteral("brush"));
    controller.setWidth(24);
    controller.press(overlay, QPointF(270, 60), Qt::LeftButton, Qt::NoModifier);
    expectStepCovered(controller, overlay, QPointF(310, 72),
                      "a mosaic brush move invalidates where it drew");
    expectStepCovered(controller, overlay, QPointF(350, 55),
                      "a growing mosaic brush invalidates where it drew");
    expectStepCovered(controller, overlay, QPointF(370, 80),
                      "the released mosaic brush invalidates where it drew");
    controller.release(overlay, QPointF(370, 80), Qt::LeftButton, Qt::NoModifier);
    expect(controller.annotations().size() == 3, "the mosaic brush lands as a third annotation");
    expect(controller.annotations().size() == 3 &&
               controller.annotations().at(2).tool == QStringLiteral("mosaic"),
           "the third mark is the mosaic brush stroke");

    // 6 & 7. The capture selection itself.  The session's selection is the whole
    // canvas, and `moveSelection` clamps a selection to the canvas, so moving it
    // before it is shrunk would change nothing.  The resize gesture (listed
    // seventh) therefore runs first: shrinking from the top-left corner gives
    // the move gesture (listed sixth) room to travel.  Both drag the selection
    // chrome, the magnifier and the toolbar that follows the selection.
    controller.chooseTool(vshot::Tool::Select);
    const vshot::LogicalRect canvas = *controller.selection();
    const QPointF topLeft(canvas.x, canvas.y);
    controller.press(overlay, topLeft, Qt::LeftButton, Qt::NoModifier);
    expectStepCovered(controller, overlay, topLeft + QPointF(50, 50),
                      "resizing the capture selection invalidates where it drew");
    expectStepCovered(controller, overlay, topLeft + QPointF(100, 100),
                      "a resizing capture selection invalidates where it drew");
    controller.release(overlay, topLeft + QPointF(100, 100), Qt::LeftButton, Qt::NoModifier);
    const vshot::LogicalRect shrunk = *controller.selection();
    expect(shrunk.x == canvas.x + 100 && shrunk.y == canvas.y + 100 &&
               shrunk.width + 100 == canvas.width && shrunk.height + 100 == canvas.height,
           "the drag shrank the capture selection from its top-left corner",
           QStringLiteral("(%1,%2 %3x%4) -> (%5,%6 %7x%8)")
               .arg(canvas.x)
               .arg(canvas.y)
               .arg(canvas.width)
               .arg(canvas.height)
               .arg(shrunk.x)
               .arg(shrunk.y)
               .arg(shrunk.width)
               .arg(shrunk.height));

    // A point well inside the shrunk selection, off every handle and off every
    // mark, so the gesture is a move of the selection rather than a resize or a
    // mark pick-up.
    const QPointF grip(350, 350);
    controller.press(overlay, grip, Qt::LeftButton, Qt::NoModifier);
    expectStepCovered(controller, overlay, grip - QPointF(20, 20),
                      "moving the capture selection invalidates where it drew");
    expectStepCovered(controller, overlay, grip - QPointF(40, 40),
                      "a moving capture selection invalidates where it drew");
    controller.release(overlay, grip - QPointF(40, 40), Qt::LeftButton, Qt::NoModifier);
    const vshot::LogicalRect shifted = *controller.selection();
    expect(shifted.x == shrunk.x - 40 && shifted.y == shrunk.y - 40 &&
               shifted.width == shrunk.width && shifted.height == shrunk.height,
           "the drag moved the capture selection without resizing it",
           QStringLiteral("(%1,%2) -> (%3,%4)")
               .arg(shrunk.x)
               .arg(shrunk.y)
               .arg(shifted.x)
               .arg(shifted.y));
}

// The wave's document form is the line's: `kind=stroke` with `tool=wave` and
// exactly the two points the drag made.  The Rust reader parses the wave from
// those two points and derives the crests itself, so a third point -- or any
// name but `wave` -- would be read as a different mark and the preview would
// stop matching the baked PNG.
void checkWaveSerializesAsATwoPointStroke()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(editingSession());
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();

    controller.chooseTool(vshot::Tool::Wave);
    controller.setWidth(6);
    controller.setCurrentColor(QColor(255, 30, 30));
    const QPointF start(60, 200);
    const QPointF end(340, 200);
    drag(controller, overlay, start, end);
    expect(controller.annotations().size() == 1, "the wave lands as one annotation");
    if (controller.annotations().size() != 1) {
        return;
    }

    const QJsonDocument document = controller.resultDocument();
    const QJsonArray annotations =
        document.object().value(QStringLiteral("annotations")).toArray();
    expect(annotations.size() == 1, "the document carries the wave");
    if (annotations.isEmpty()) {
        return;
    }
    const QJsonObject mark = annotations.at(0).toObject();
    expect(mark.value(QStringLiteral("kind")).toString() == QStringLiteral("stroke"),
           "the wave serializes as a stroke",
           mark.value(QStringLiteral("kind")).toString());
    expect(mark.value(QStringLiteral("tool")).toString() == QStringLiteral("wave"),
           "the wave's tool name is `wave`",
           mark.value(QStringLiteral("tool")).toString());
    const QJsonArray points = mark.value(QStringLiteral("points")).toArray();
    expect(points.size() == 2, "the wave carries exactly its two endpoints",
           QStringLiteral("points=%1").arg(points.size()));
    if (points.size() == 2) {
        const QJsonObject first = points.at(0).toObject();
        const QJsonObject last = points.at(1).toObject();
        expect(first.value(QStringLiteral("x")).toInt() == static_cast<int>(start.x()) &&
                   first.value(QStringLiteral("y")).toInt() == static_cast<int>(start.y()) &&
                   last.value(QStringLiteral("x")).toInt() == static_cast<int>(end.x()) &&
                   last.value(QStringLiteral("y")).toInt() == static_cast<int>(end.y()),
               "the two points are the ends of the drag",
               QStringLiteral("(%1,%2)-(%3,%4)")
                   .arg(first.value(QStringLiteral("x")).toInt())
                   .arg(first.value(QStringLiteral("y")).toInt())
                   .arg(last.value(QStringLiteral("x")).toInt())
                   .arg(last.value(QStringLiteral("y")).toInt()));
    }
}

} // namespace

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);

    checkRepaintsReuseTheRaster();
    checkEachMarkCachesOnItsOwn();
    checkCachedPixelsLandOnTheMark();
    checkTranslucentColorSerializesWithAlpha();
    checkOpaqueColorSerializesWithoutAlpha();
    checkPureMoveReusesTheRaster();
    checkEachOutputKeepsItsOwnRaster();
    checkEdgeOfCanvasKeepsTheRaster();
    checkLiveStrokeMatchesTheCommittedMark();
    checkInteractiveUpdateCoversTheChange();
    checkWaveSerializesAsATwoPointStroke();

    if (failures != 0) {
        std::printf("\n%d annotation cache checks failed\n", failures);
        return 1;
    }
    std::printf("\nall annotation cache checks passed\n");
    return 0;
}
