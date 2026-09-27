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
    output.scale = 1;
    output.pixelWidth = 400;
    output.pixelHeight = 400;
    output.image = QImage(400, 400, QImage::Format_ARGB32);
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
