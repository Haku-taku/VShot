// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline check for the annotation cache on a high-DPI screen.
//
// A cached mark is rasterized in logical coordinates and then blitted by a
// painter that carries the widget's device-pixel-ratio transform, while the
// overlay deliberately draws with smooth transforms off.  A cache built one
// logical pixel per pixel would therefore be magnified with nearest-neighbour:
// the mark would blur, and what the editor shows would stop matching the PNG
// the Rust side renders at full resolution.  This check runs the real editor on
// a 2x screen and reads the ratio the cache was built at, so what is asserted
// is the pixels a scaled display actually gets.
//
// The ratio's transform is the raster image's own device-pixel ratio, which
// QPainter applies by itself when it paints into that image.  A rasterizer that
// calls `scale(ratio, ratio)` on top of that multiplies by it a second time: on
// a 2x screen every committed mark comes out twice the size with whatever fell
// past its own clip cut away, while the live preview -- which never goes
// through a raster -- stays right.  The span assertion below is what tells the
// two apart; "the mark painted somewhere" is true either way.
//
// Needs QApplication and the offscreen platform plugin; no compositor and no
// layer shell.  `QT_SCALE_FACTOR` is set before the application exists so the
// one screen the overlay is parented to reports a 2x ratio.
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
#include <cstdio>
#include <cstdlib>

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
// opens with the toolbar up and a canvas to draw on.
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
    // The screen is a 2x one (`QT_SCALE_FACTOR` below), so the output it
    // carries is too: 400 logical pixels across, and 800 of them in the frame
    // the engine holds.  A real capture on such a screen reports exactly this,
    // so the check drives the editor with the numbers the editor really gets
    // rather than with a 1x output on a 2x screen.
    output.scale = 2;
    output.pixelWidth = 800;
    output.pixelHeight = 800;
    output.image = QImage(800, 800, QImage::Format_ARGB32);
    for (int y = 0; y < 800; ++y) {
        for (int x = 0; x < 800; ++x) {
            output.image.setPixelColor(x, y,
                                       QColor(40 + (x / 16 * 7) % 60, 30 + (y / 16 * 53) % 200,
                                              30 + ((x + y) / 16 * 29) % 200));
        }
    }
    session.outputs.push_back(output);
    session.selection = vshot::LogicalRect{0, 0, 400, 400};
    return session;
}

} // namespace

int main(int argc, char **argv)
{
    // Must be set before QApplication: the offscreen screen takes its ratio
    // from the high-DPI scaling the application reads at construction.
    qputenv("QT_SCALE_FACTOR", "2");
    QApplication app(argc, argv);

    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        std::printf("FAIL  a screen to hang an overlay off\n");
        return 1;
    }
    vshot::OverlayController controller(editingSession());
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        std::printf("FAIL  the controller accepts an overlay -- %s\n", qPrintable(error));
        return 1;
    }
    overlay->show();
    controller.beginPresetEdit();

    const qreal dpr = overlay->devicePixelRatio();
    expect(dpr == 2.0, "the screen the editor runs on reports a 2x ratio",
           QStringLiteral("ratio %1").arg(dpr));

    // A thick bright stroke across the middle, drawn the way the pen is.
    controller.chooseTool(vshot::Tool::Pen);
    controller.setWidth(6);
    controller.setCurrentColor(QColor(255, 30, 30));
    controller.press(overlay, QPointF(60, 200), Qt::LeftButton, Qt::NoModifier);
    controller.move(overlay, QPointF(200, 200), Qt::LeftButton, Qt::NoModifier);
    controller.move(overlay, QPointF(340, 200), Qt::LeftButton, Qt::NoModifier);
    controller.release(overlay, QPointF(340, 200), Qt::LeftButton, Qt::NoModifier);
    expect(controller.annotations().size() == 1, "the stroke lands as one annotation");

    // Render the real overlay at the screen's resolution: the target carries
    // the 2x ratio, so the painter the widget renders with does too.
    QImage target(overlay->size() * dpr, QImage::Format_ARGB32_Premultiplied);
    target.setDevicePixelRatio(dpr);
    target.fill(Qt::transparent);
    overlay->render(&target);

    int painted = 0;
    for (int y = 380; y < 420; ++y) {
        for (int x = 100; x < 700; ++x) {
            if (x >= target.width() || y >= target.height()) {
                continue;
            }
            const QColor pixel = target.pixelColor(x, y);
            if (pixel.red() > 180 && pixel.green() < 120 && pixel.blue() < 120) {
                ++painted;
            }
        }
    }
    expect(painted > 0, "the stroke paints where it was drawn on the 2x screen",
           QStringLiteral("%1 bright pixels").arg(painted));

    // Where it painted, not only that it did.  The drag ran along logical
    // y = 200 from logical x = 60 to 340, so on this 2x screen the ink belongs
    // at device x 120..680.  A raster that scaled by the ratio a second time
    // drew the same mark at twice that size, cut off at the raster's own
    // boundary -- still red, still "painted", and the wrong size in the wrong
    // place, which is exactly what the count above cannot see.
    const int midRow = static_cast<int>(200 * dpr);
    int minX = target.width();
    int maxX = -1;
    for (int y = midRow - 20; y <= midRow + 20 && y < target.height(); ++y) {
        for (int x = 0; x < target.width(); ++x) {
            const QColor pixel = target.pixelColor(x, y);
            if (pixel.red() > 180 && pixel.green() < 120 && pixel.blue() < 120) {
                if (x < minX) {
                    minX = x;
                }
                if (x > maxX) {
                    maxX = x;
                }
            }
        }
    }
    // The round cap reaches half the six-logical-pixel width past each end, so
    // the ink runs a few device pixels outside 120..680; further off than that
    // is the mark's size being wrong rather than the cap.
    expect(maxX >= 0 && std::abs(minX - 120) <= 8 && std::abs(maxX - 680) <= 8,
           "the committed stroke spans the drag's own pixels, scaled by the ratio once",
           QStringLiteral("device x %1..%2, expected 120..680").arg(minX).arg(maxX));

    // The thickness is where a doubled ratio shows up plainly rather than by a
    // few pixels on an end: the pen is six logical pixels wide, so a column
    // through the middle of the stroke is twelve device pixels tall.  A raster
    // scaled by the ratio on top of the image's own ratio drew it twenty-four.
    // The column is logical x = 100, well inside the stroke and far from the
    // round caps at either end.
    const int columnX = static_cast<int>(100 * dpr);
    int column = 0;
    for (int y = 0; y < target.height(); ++y) {
        const QColor pixel = target.pixelColor(columnX, y);
        if (pixel.red() > 180 && pixel.green() < 120 && pixel.blue() < 120) {
            ++column;
        }
    }
    expect(std::abs(column - 12) <= 4,
           "the committed stroke keeps the pen's own thickness, six logical pixels at 2x",
           QStringLiteral("%1 device pixels tall, expected about 12").arg(column));

    // The straight tools have rasters of their own, so the pen above cannot
    // vouch for them.  A rectangle dragged from logical (100, 60) to (300, 140)
    // covers 200x80 logical pixels -- device x 200..600, y 120..280 -- and the
    // same doubled ratio that moved the pen would move this too.
    controller.chooseTool(vshot::Tool::Rectangle);
    controller.setWidth(6);
    controller.setCurrentColor(QColor(30, 255, 30));
    controller.press(overlay, QPointF(100, 60), Qt::LeftButton, Qt::NoModifier);
    controller.move(overlay, QPointF(300, 140), Qt::LeftButton, Qt::NoModifier);
    controller.release(overlay, QPointF(300, 140), Qt::LeftButton, Qt::NoModifier);
    expect(controller.annotations().size() == 2, "the rectangle lands as a second annotation");

    QImage shapes(overlay->size() * dpr, QImage::Format_ARGB32_Premultiplied);
    shapes.setDevicePixelRatio(dpr);
    shapes.fill(Qt::transparent);
    overlay->render(&shapes);

    // Green is the rectangle's own colour; the frozen background's greens stop
    // well short of it, and the red pen is nowhere near the test.
    int shapeMinX = shapes.width();
    int shapeMaxX = -1;
    int shapeMinY = shapes.height();
    int shapeMaxY = -1;
    const int under = static_cast<int>(400 * dpr);
    for (int y = 0; y < shapes.height(); ++y) {
        for (int x = 0; x < shapes.width(); ++x) {
            const QColor pixel = shapes.pixelColor(x, y);
            if (pixel.green() > 250 && pixel.red() < 60 && pixel.blue() < 60) {
                if (x < shapeMinX) {
                    shapeMinX = x;
                }
                if (x > shapeMaxX) {
                    shapeMaxX = x;
                }
                if (y < shapeMinY) {
                    shapeMinY = y;
                }
                if (y > shapeMaxY) {
                    shapeMaxY = y;
                }
            }
        }
    }
    expect(shapeMaxX >= 0 && std::abs(shapeMinX - 200) <= 8 && std::abs(shapeMaxX - 600) <= 8 &&
               std::abs(shapeMinY - 120) <= 8 && std::abs(shapeMaxY - 280) <= 8,
           "the committed rectangle covers the rect it was dragged over, at the ratio once",
           QStringLiteral("device x %1..%2 y %3..%4, expected x 200..600 y 120..280")
               .arg(shapeMinX)
               .arg(shapeMaxX)
               .arg(shapeMinY)
               .arg(shapeMaxY));
    // A doubled ratio also shows up as an off-canvas mark: the raster is clipped
    // to its own bounds, so ink anywhere near the far edge is that clip.
    expect(shapeMaxX < under, "the rectangle stays clear of the canvas edge",
           QStringLiteral("rightmost green at x %1").arg(shapeMaxX));

    const int deviceRatio = controller.annotations().constFirst().rasterDeviceRatio();
    expect(deviceRatio == 2,
           "the cached raster is built at the screen's 2x resolution, not at one logical pixel "
           "per pixel",
           QStringLiteral("built at %1").arg(deviceRatio));

    // The cache is shared, so a repaint that changes nothing must reuse it
    // rather than rasterize the mark at a different ratio.
    QImage again(overlay->size() * dpr, QImage::Format_ARGB32_Premultiplied);
    again.setDevicePixelRatio(dpr);
    again.fill(Qt::transparent);
    overlay->render(&again);
    expect(controller.annotations().constFirst().rasterDeviceRatio() == 2,
           "a repaint keeps the cached raster at the same ratio");

    if (failures != 0) {
        std::printf("\n%d high-DPI checks failed\n", failures);
        return 1;
    }
    std::printf("\nall high-DPI checks passed\n");
    return 0;
}
