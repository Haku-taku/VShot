// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline check for the HDR backdrop the region overlay leaves to VShot.
//
// When VShot can show the frozen frame itself — on a colour-managed surface
// carrying the output's own image description, so the compositor converts
// nothing and tone-maps nothing — the overlay has to keep out of the way: it
// draws the veil with the selection cut out of it and no frame of its own.
// Drawing the SDR frame there instead would cover the better picture with a
// dimmer one, and the selection would show a tone map rather than the light
// the screen showed.
//
// The check paints the real overlay offscreen and reads the pixels back, so
// what it pins is the alpha the surface carries: transparent where the
// backdrop must show through, the veil everywhere else, and — with no
// backdrop — the frame drawn as it always was.
//
// Needs QApplication and the offscreen platform plugin; no compositor and no
// layer shell.  Built only with `-DVSHOT_BUILD_CHECKS=ON`; see the README's
// verification section.

#include "capture_overlay.hpp"

#include <QApplication>
#include <QColor>
#include <QGuiApplication>
#include <QImage>
#include <QPainter>
#include <QScreen>
#include <QString>

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

// A one-output region session with its selection already made, so the
// controller opens in editing state and the veil has a hole to leave.
vshot::Session editingSession(bool backdrop, vshot::LogicalRect selection)
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
    output.backdrop = backdrop;
    output.image = QImage(400, 400, QImage::Format_RGB32);
    output.image.fill(QColor(255, 255, 255));
    session.outputs.push_back(output);
    session.selection = selection;
    return session;
}

// Paints the overlay the way a layer surface's buffer would be written, and
// hands the pixels back.
QImage render(const vshot::Session &session)
{
    vshot::OverlayController controller(session);
    controller.beginPresetEdit();
    QScreen *screen = QGuiApplication::primaryScreen();
    QString error;
    vshot::CaptureOverlay *overlay =
        screen == nullptr ? nullptr : controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        return QImage();
    }
    QImage target(overlay->size(), QImage::Format_ARGB32_Premultiplied);
    target.fill(Qt::transparent);
    QPainter painter(&target);
    controller.paint(overlay, &painter);
    painter.end();
    return target;
}

void checkBackdropLeavesTheFrameToVShot()
{
    // A wide selection, and sample points clear of the chrome the editor draws
    // over it: the size pill hangs off the selection's top-left corner and the
    // resize handles sit on its corners and edge midpoints.
    const vshot::LogicalRect selection{100, 100, 200, 200};
    const QPoint insidePixel{200, 250};
    const QPoint outsidePixel{10, 10};

    const QImage withBackdrop = render(editingSession(true, selection));
    if (withBackdrop.isNull()) {
        expect(false, "the overlay paints offscreen");
        return;
    }
    const QColor inside = withBackdrop.pixelColor(insidePixel);
    const QColor outside = withBackdrop.pixelColor(outsidePixel);
    expect(inside.alpha() == 0,
           "the selection is left to the backdrop",
           QStringLiteral("alpha %1").arg(inside.alpha()));
    expect(outside.alpha() == 80 && outside.red() == 0 && outside.green() == 0 &&
               outside.blue() == 0,
           "the surround is the black veil",
           QStringLiteral("rgba %1,%2,%3,%4")
               .arg(outside.red())
               .arg(outside.green())
               .arg(outside.blue())
               .arg(outside.alpha()));

    const QImage withoutBackdrop = render(editingSession(false, selection));
    if (withoutBackdrop.isNull()) {
        expect(false, "the overlay paints offscreen with no backdrop");
        return;
    }
    const QColor frame = withoutBackdrop.pixelColor(insidePixel);
    expect(frame.alpha() == 255 && frame.red() > 200,
           "with no backdrop the overlay draws the frame itself",
           QStringLiteral("rgba %1,%2,%3,%4")
               .arg(frame.red())
               .arg(frame.green())
               .arg(frame.blue())
               .arg(frame.alpha()));
}

} // namespace

int main(int argc, char **argv)
{
    QApplication app(argc, argv);

    checkBackdropLeavesTheFrameToVShot();

    if (failures != 0) {
        std::printf("\n%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("\nall backdrop checks passed\n");
    return 0;
}
