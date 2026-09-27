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
#include <QPointF>
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

} // namespace

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);

    checkRepaintsReuseTheRaster();
    checkEachMarkCachesOnItsOwn();
    checkCachedPixelsLandOnTheMark();
    checkLiveStrokeMatchesTheCommittedMark();

    if (failures != 0) {
        std::printf("\n%d annotation cache checks failed\n", failures);
        return 1;
    }
    std::printf("\nall annotation cache checks passed\n");
    return 0;
}
