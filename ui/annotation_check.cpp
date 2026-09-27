// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline check for the annotation render cache.
//
// Every committed mark keeps a rasterized copy of itself and redraws it only
// when something it draws changes.  That matters most for the mosaic, which
// averages the source image block by block: recomputing it on every repaint
// (a selection drag, a pointer move) is what made a busy capture stutter.  The
// check paints the real overlay offscreen and reads the per-mark rebuild count,
// so what is asserted is the cache the editor actually uses.
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
    output.image.fill(QColor(70, 90, 120));
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

} // namespace

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);

    checkRepaintsReuseTheRaster();
    checkEachMarkCachesOnItsOwn();
    checkCachedPixelsLandOnTheMark();

    if (failures != 0) {
        std::printf("\n%d annotation cache checks failed\n", failures);
        return 1;
    }
    std::printf("\nall annotation cache checks passed\n");
    return 0;
}
