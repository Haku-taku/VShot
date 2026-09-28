// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline check for where the editor's floating toolbar lands.
//
// The toolbar keeps its command bar (the tool row) pinned to the selection and
// grows the style row out of the way.  That is easy to get wrong at the edges:
// when the capture nearly fills the display there is no room beyond either side
// of the command bar, and a placement that only flips the whole panel up or
// down drags the buttons around as the style row shows and hides.  This check
// drives the real controller and reads the resulting widget geometry, so what
// is asserted is the placement the user actually meets.
//
// Needs QApplication and the offscreen platform plugin; no compositor and no
// layer shell.  `QT_QPA_PLATFORM=offscreen` supplies the one screen the overlay
// is parented to.
//
// Built only with `-DVSHOT_BUILD_CHECKS=ON`; see the README's verification
// section.

#include "capture_overlay.hpp"

#include <QAbstractButton>
#include <QApplication>
#include <QCoreApplication>
#include <QHelpEvent>
#include <QLabel>
#include <QPoint>
#include <QScreen>
#include <QString>
#include <QWidget>

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

// A one-output region session with a fixed 400x400 output and an already-made
// selection, so the controller opens in editing state with the toolbar up.
vshot::Session sessionFor(const vshot::LogicalRect &selection)
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
    session.outputs.push_back(output);
    session.selection = selection;
    return session;
}

struct ToolbarParts {
    QWidget *command = nullptr;
    QWidget *style = nullptr;
};

ToolbarParts toolbarParts(vshot::CaptureOverlay *overlay)
{
    ToolbarParts parts;
    parts.command = overlay->findChild<QWidget *>(QStringLiteral("toolbarCommandSurface"));
    parts.style = overlay->findChild<QWidget *>(QStringLiteral("toolbarStyleRow"));
    return parts;
}

int globalTop(const QWidget *widget)
{
    return widget->mapToGlobal(QPoint(0, 0)).y();
}

// The capture nearly fills the display: no room above or below the command bar
// for the style row.  The style row must double back over the selection rather
// than push the command bar, which is what made the buttons jump.
void checkCrampedCaptureKeepsTheButtonsStill()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(sessionFor(vshot::LogicalRect{0, 0, 400, 400}));
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();
    const ToolbarParts parts = toolbarParts(overlay);
    expect(parts.command != nullptr && parts.style != nullptr,
           "the toolbar has a command bar and a style row");
    if (parts.command == nullptr || parts.style == nullptr) {
        return;
    }
    // The remembered tool in the user's config can open the session on another
    // tool, so pin Select before asserting on what Select's style row does.
    controller.chooseTool(vshot::Tool::Select);
    expect(!parts.style->isVisible(), "the Select tool starts with no style row");
    const int before = globalTop(parts.command);

    controller.chooseTool(vshot::Tool::Rectangle);
    expect(parts.style->isVisible(), "the Rectangle tool raises the style row");
    const int after = globalTop(parts.command);
    const int styleTop = globalTop(parts.style);
    expect(std::abs(after - before) <= 1,
           "the command bar does not move when the style row appears",
           QStringLiteral("moved from %1 to %2").arg(before).arg(after));
    expect(styleTop < after,
           "the style row doubles back above the command bar",
           QStringLiteral("style row top %1, bar top %2").arg(styleTop).arg(after));
}

// Room above the selection: the whole panel stays above it and the style row
// grows further up, away from the selection.
void checkPanelAboveKeepsTheStyleRowAbove()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(sessionFor(vshot::LogicalRect{150, 150, 100, 100}));
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();
    const ToolbarParts parts = toolbarParts(overlay);
    if (parts.command == nullptr || parts.style == nullptr) {
        expect(false, "the toolbar has a command bar and a style row");
        return;
    }
    const int before = globalTop(parts.command);
    expect(before + parts.command->height() <= 150,
           "the command bar sits above the selection",
           QStringLiteral("bar bottom %1, selection top 150")
               .arg(before + parts.command->height()));

    controller.chooseTool(vshot::Tool::Rectangle);
    const int after = globalTop(parts.command);
    expect(std::abs(after - before) <= 1,
           "the command bar stays put while the style row appears",
           QStringLiteral("moved from %1 to %2").arg(before).arg(after));
    expect(globalTop(parts.style) < after,
           "the style row grows above the command bar, away from the selection");
}

// Room below but not above: the panel drops below the selection and the style
// row grows further down, away from the selection, without moving the buttons.
void checkPanelBelowKeepsTheStyleRowBelow()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(sessionFor(vshot::LogicalRect{0, 0, 400, 120}));
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();
    const ToolbarParts parts = toolbarParts(overlay);
    if (parts.command == nullptr || parts.style == nullptr) {
        expect(false, "the toolbar has a command bar and a style row");
        return;
    }
    const int before = globalTop(parts.command);
    expect(before >= 120, "the command bar drops below the selection",
           QStringLiteral("bar top %1, selection bottom 120").arg(before));

    controller.chooseTool(vshot::Tool::Rectangle);
    const int after = globalTop(parts.command);
    expect(std::abs(after - before) <= 1,
           "the command bar stays put while the style row appears",
           QStringLiteral("moved from %1 to %2").arg(before).arg(after));
    expect(globalTop(parts.style) > after,
           "the style row grows below the command bar, away from the selection");
}

// Hovering a button: the panel draws the tip itself.
//
// Qt's `QToolTip` is a popup window, and this process's layer-shell platform
// integration cannot host one: the compositor never gets a usable popup and Qt
// paints the text into the overlay's own full-output surface, which the user
// meets as a screen-sized block of panel colour.  This must stay a small child
// of the overlay, and no Qt tooltip window may appear.
void checkHoverShowsThePanelTooltip()
{
    QScreen *screen = QGuiApplication::primaryScreen();
    if (screen == nullptr) {
        expect(false, "a screen to hang an overlay off");
        return;
    }
    vshot::OverlayController controller(sessionFor(vshot::LogicalRect{150, 150, 100, 100}));
    QString error;
    vshot::CaptureOverlay *overlay = controller.addOverlay(0, screen, &error);
    if (overlay == nullptr) {
        expect(false, "the controller accepts an overlay", error);
        return;
    }
    overlay->show();
    controller.beginPresetEdit();

    QAbstractButton *button = nullptr;
    const QList<QAbstractButton *> buttons = overlay->findChildren<QAbstractButton *>();
    for (QAbstractButton *candidate : buttons) {
        if (candidate->isVisible() && !candidate->toolTip().isEmpty()) {
            button = candidate;
            break;
        }
    }
    if (button == nullptr) {
        expect(false, "a visible toolbar button with a tooltip");
        return;
    }

    const QPoint local(button->width() / 2, button->height() / 2);
    const int windowsBefore = QApplication::topLevelWidgets().size();
    QHelpEvent hover(QEvent::ToolTip, local, button->mapToGlobal(local));
    QCoreApplication::sendEvent(button, &hover);
    const int windowsAfter = QApplication::topLevelWidgets().size();
    expect(windowsAfter == windowsBefore,
           "a hover raises no tooltip window of Qt's own",
           QStringLiteral("%1 window(s) appeared").arg(windowsAfter - windowsBefore));

    auto *tip = overlay->findChild<QLabel *>(QStringLiteral("vshotTooltip"));
    expect(tip != nullptr && tip->isVisible(), "the panel shows its own tooltip");
    if (tip == nullptr) {
        return;
    }
    expect(tip->text() == button->toolTip(), "the tip carries the hovered button's text");
    // A one-line card that fits the overlay: the failure this guards is a block
    // of panel colour covering the whole surface, not a wide line of text.
    expect(tip->height() < 60 && tip->width() < overlay->width(),
           "the tip is a small card, not a screen-sized block",
           QStringLiteral("tip %1x%2 over %3x%4")
               .arg(tip->width())
               .arg(tip->height())
               .arg(overlay->width())
               .arg(overlay->height()));
    const QPoint tipTopLeft = tip->mapTo(overlay, QPoint(0, 0));
    const QPoint buttonTopLeft = button->mapTo(overlay, QPoint(0, 0));
    expect(std::abs(tipTopLeft.y() - buttonTopLeft.y()) < 200,
           "the tip stays near the button it describes");
}

} // namespace

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);

    checkCrampedCaptureKeepsTheButtonsStill();
    checkPanelAboveKeepsTheStyleRowAbove();
    checkPanelBelowKeepsTheStyleRowBelow();
    checkHoverShowsThePanelTooltip();

    if (failures != 0) {
        std::printf("\n%d toolbar checks failed\n", failures);
        return 1;
    }
    std::printf("\nall toolbar checks passed\n");
    return 0;
}
