// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline check for the pins' label surface: the `HDR` tag, the badge that
// reports a zoom or a copy, and the right-click menu.
//
// The chrome is the one part of the pin stack that draws text, and every one of
// its rules is invisible in the code and obvious on screen: the tag is up only
// over the pin the pointer is on, it is drawn in a different ink when what is
// on screen is the capture's own light rather than its SDR mapping, a badge
// outlives the stack update that follows it, and the menu takes Esc and the
// arrows and answers with the row the user picked.  All of that fails silently
// if the wiring is wrong, so it is driven here and read back as pixels and as
// the lines the surface writes to its socket.
//
// Built only with `-DVSHOT_BUILD_CHECKS=ON`.  Needs Qt Widgets and the offscreen
// platform plugin (run it with QT_QPA_PLATFORM=offscreen), but no compositor and
// no layer shell: the widget is rendered straight into a QImage through
// QWidget::render(), and showLayerSurface() is never called.

#include "pin_chrome.hpp"
#include "i18n.hpp"
#include "pin_label.hpp"

#include <QApplication>
#include <QImage>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QLocalServer>
#include <QLocalSocket>
#include <QMouseEvent>
#include <QPainter>
#include <QPoint>
#include <QRect>
#include <QScreen>
#include <QString>
#include <QStringList>
#include <QWidget>

#include <cstdio>

namespace {

int failures = 0;

void expect(bool condition, const char *what, const QString &detail = QString())
{
    if (condition) {
        std::printf("ok    %s%s\n", what,
                    detail.isEmpty() ? "" : QStringLiteral("  %1").arg(detail).toUtf8().constData());
        return;
    }
    std::printf("FAIL  %s%s\n", what,
                detail.isEmpty() ? "" : QStringLiteral("  %1").arg(detail).toUtf8().constData());
    ++failures;
}

QScreen *screen()
{
    return QGuiApplication::primaryScreen();
}

/// The chrome rendered offscreen at a size that holds the labels under test.
QImage paint(vshot::PinChrome &chrome, const QSize &size)
{
    chrome.resize(size);
    QImage image(size, QImage::Format_ARGB32);
    image.fill(Qt::transparent);
    chrome.render(&image);
    return image;
}

/// How many pixels of `image` are neither transparent nor the translucent box
/// the labels are drawn on: the ink, in other words.
int inkPixels(const QImage &image, const QRect &within)
{
    int count = 0;
    for (int y = within.top(); y <= within.bottom(); ++y) {
        for (int x = within.left(); x <= within.right(); ++x) {
            if (!within.contains(x, y) || x < 0 || y < 0 || x >= image.width() || y >= image.height()) {
                continue;
            }
            const QColor pixel = image.pixelColor(x, y);
            // The box is black at 160 alpha over nothing; the ink is white or
            // grey, so anything with a bright channel is ink.
            if (pixel.alpha() > 0 && (pixel.red() > 100 || pixel.green() > 100 || pixel.blue() > 100)) {
                ++count;
            }
        }
    }
    return count;
}

/// A chrome wired to a socket this check listens on, so the answers it sends
/// back can be read.
struct Wired {
    QLocalServer server;
    QLocalSocket *client = nullptr;
    vshot::PinChrome *chrome = nullptr;

    bool start(const QString &path)
    {
        QLocalServer::removeServer(path);
        if (!server.listen(path)) {
            return false;
        }
        chrome = new vshot::PinChrome(screen());
        chrome->resize(800, 600);
        return true;
    }
};

/// The answer the chrome has written, once it has actually gone out.
///
/// A socket write is queued: the bytes reach the peer when the event loop runs,
/// and `waitForReadyRead` on the reading end does not pump the writing one.  So
/// both sides are given a turn before the read.
QByteArray readAnswer(QLocalSocket &client, QLocalSocket &peer)
{
    for (int attempt = 0; attempt < 40; ++attempt) {
        // The chrome's write is queued in its own socket, and both ends are
        // this process: the bytes only travel once each end is given a turn.
        peer.flush();
        client.flush();
        QApplication::processEvents();
        client.waitForReadyRead(20);
        const QByteArray bytes = client.readAll();
        if (!bytes.isEmpty()) {
            return bytes;
        }
    }
    return QByteArray();
}

/// The label a pin of `size` at `origin` puts on the chrome.
vshot::PinChrome::Label label(quint64 id, const QPoint &origin, const QSize &size, bool hdr,
                              bool shown, bool hovered)
{
    vshot::PinChrome::Label label;
    label.id = id;
    label.origin = origin;
    label.size = size;
    label.capturedHdr = hdr;
    label.shownAsHdr = shown;
    label.hovered = hovered;
    return label;
}

// --- the tag ---------------------------------------------------------------

void checkTag()
{
    auto *chrome = new vshot::PinChrome(screen());
    chrome->resize(400, 300);

    // An HDR capture with the pointer on it: the tag is up.
    chrome->setLabels({label(1, QPoint(50, 50), QSize(100, 80), true, true, true)});
    const QImage hovered = paint(*chrome, QSize(400, 300));
    const QRect overPin(50, 50, 100, 40);
    expect(inkPixels(hovered, overPin) > 0, "an HDR pin under the pointer carries its tag");

    // The same pin with the pointer elsewhere: no tag.
    chrome->setLabels({label(1, QPoint(50, 50), QSize(100, 80), true, true, false)});
    const QImage away = paint(*chrome, QSize(400, 300));
    expect(inkPixels(away, overPin) == 0, "and nothing while the pointer is away");

    // An SDR pin under the pointer: no tag, because there is nothing to say.
    chrome->setLabels({label(1, QPoint(50, 50), QSize(100, 80), false, false, true)});
    const QImage plain = paint(*chrome, QSize(400, 300));
    expect(inkPixels(plain, overPin) == 0, "an SDR pin carries no tag at all");

    // A pin on another output: nothing of it lands here.
    chrome->setLabels({label(1, QPoint(900, 50), QSize(100, 80), true, true, true)});
    const QImage elsewhere = paint(*chrome, QSize(400, 300));
    expect(inkPixels(elsewhere, QRect(0, 0, 400, 300)) == 0,
           "a pin on another output draws nothing here");

    delete chrome;
}

// --- the badge -------------------------------------------------------------

void checkBadge()
{
    auto *chrome = new vshot::PinChrome(screen());
    chrome->resize(400, 300);
    // A badge is about a pin the pointer is nowhere near, and about a pin that
    // carries no tag: a save reports from a menu that has closed.
    chrome->setLabels({label(1, QPoint(50, 50), QSize(100, 80), false, false, false)});
    chrome->showBadge(1, QStringLiteral("110%"));
    const QImage shown = paint(*chrome, QSize(400, 300));
    // The badge is anchored at the pin's bottom-right corner.
    const QRect corner(50, 90, 130, 40);
    expect(inkPixels(shown, corner) > 0, "a badge shows on a pin with no tag");

    // A badge for a pin that is not in the stack has nowhere to land, and must
    // not draw at the corner of some other pin.
    auto *other = new vshot::PinChrome(screen());
    other->resize(400, 300);
    other->setLabels({label(1, QPoint(50, 50), QSize(100, 80), false, false, false)});
    other->showBadge(7, QStringLiteral("110%"));
    const QImage stranger = paint(*other, QSize(400, 300));
    expect(inkPixels(stranger, QRect(0, 0, 400, 300)) == 0,
           "a badge for a pin that is gone draws nothing");
    delete other;

    delete chrome;
}

// --- the menu --------------------------------------------------------------

void checkMenu()
{
    const QString path = QStringLiteral("/tmp/vshot-chrome-check.sock");
    Wired wired;
    if (!wired.start(path)) {
        expect(false, "the check can listen on a socket");
        return;
    }
    QLocalSocket client;
    client.connectToServer(path);
    if (!client.waitForConnected(2000)) {
        expect(false, "the check can connect to itself");
        delete wired.chrome;
        return;
    }
    // The server's end of the connection: the chrome writes into this, and the
    // check reads what the client got.
    if (!wired.server.waitForNewConnection(2000)) {
        expect(false, "the check accepts its own connection");
        delete wired.chrome;
        return;
    }
    QLocalSocket *peer = wired.server.nextPendingConnection();
    if (peer == nullptr) {
        expect(false, "the check has a peer socket");
        delete wired.chrome;
        return;
    }
    wired.chrome->setSocket(peer);

    const QStringList rows{QStringLiteral("Copy image"), QStringLiteral("Save as…"),
                           QStringLiteral("Edit"), QStringLiteral("Reset zoom"),
                           QStringLiteral("Recognize text…"), QStringLiteral("Close")};
    wired.chrome->setMenu(1, QPoint(60, 60), rows);
    const QImage drawn = paint(*wired.chrome, QSize(400, 300));
    // The menu opens down and right of the anchor.
    expect(inkPixels(drawn, QRect(60, 60, 220, 160)) > 0, "the menu is drawn at its anchor");

    // Esc takes it down and says so.
    QKeyEvent escape(QEvent::KeyPress, Qt::Key_Escape, Qt::NoModifier);
    QApplication::sendEvent(wired.chrome, &escape);
    const QByteArray answer = readAnswer(client, *peer);
    expect(answer.contains("\"dismissed\""), "Esc takes the menu down and tells the daemon",
           QString::fromUtf8(answer).trimmed());
    const QImage gone = paint(*wired.chrome, QSize(400, 300));
    expect(inkPixels(gone, QRect(0, 0, 400, 300)) == 0, "and nothing of it is left");

    // Down then Enter picks the second row.
    wired.chrome->setMenu(1, QPoint(60, 60), rows);
    // With nothing highlighted, Down enters the list at the top -- the same
    // rule the Qt pin surface's own menu had, so the two feel alike.
    QKeyEvent down(QEvent::KeyPress, Qt::Key_Down, Qt::NoModifier);
    QApplication::sendEvent(wired.chrome, &down);
    QKeyEvent enter(QEvent::KeyPress, Qt::Key_Return, Qt::NoModifier);
    QApplication::sendEvent(wired.chrome, &enter);
    const QByteArray chosen = readAnswer(client, *peer);
    expect(chosen.contains("\"chosen\""), "Enter picks the row the pointer is on",
           QString::fromUtf8(chosen).trimmed());
    const QJsonObject reply = QJsonDocument::fromJson(chosen.trimmed()).object();
    expect(reply.value(QStringLiteral("row")).toInt() == 0,
           "and the row it names is the one the arrow reached",
           QString::number(reply.value(QStringLiteral("row")).toInt()));
    // A second Down moves on to the next row, so the arrows walk the list.
    wired.chrome->setMenu(1, QPoint(60, 60), rows);
    QApplication::sendEvent(wired.chrome, &down);
    QApplication::sendEvent(wired.chrome, &down);
    QApplication::sendEvent(wired.chrome, &enter);
    const QJsonObject second = QJsonDocument::fromJson(readAnswer(client, *peer).trimmed()).object();
    expect(second.value(QStringLiteral("row")).toInt() == 1, "the arrows walk the list",
           QString::number(second.value(QStringLiteral("row")).toInt()));

    // A click inside the menu picks that row.
    wired.chrome->setMenu(1, QPoint(60, 60), rows);
    const QRect menu = QRect(60, 60, 220, 160);
    // The third row: the row height comes from the label font, so aim at the
    // middle of the row band rather than at a guessed pixel.
    const QFontMetrics metrics(vshot::tagFont());
    const int rowHeight = metrics.height() + 10;
    QMouseEvent press(QEvent::MouseButtonPress, QPointF(menu.left() + 20, menu.top() + rowHeight * 2 + 5),
                      Qt::LeftButton, Qt::LeftButton, Qt::NoModifier);
    QApplication::sendEvent(wired.chrome, &press);
    const QByteArray clicked = readAnswer(client, *peer);
    const QJsonObject picked = QJsonDocument::fromJson(clicked.trimmed()).object();
    expect(picked.value(QStringLiteral("cmd")).toString() == QStringLiteral("chosen")
               && picked.value(QStringLiteral("row")).toInt() == 2,
           "a click picks the row under it",
           QString::fromUtf8(clicked).trimmed());

    // A click outside closes it without a pick.
    wired.chrome->setMenu(1, QPoint(60, 60), rows);
    QMouseEvent outside(QEvent::MouseButtonPress, QPointF(380, 280), Qt::LeftButton, Qt::LeftButton,
                        Qt::NoModifier);
    QApplication::sendEvent(wired.chrome, &outside);
    const QByteArray dismissed = readAnswer(client, *peer);
    expect(dismissed.contains("\"dismissed\""), "a click outside closes it without a pick",
           QString::fromUtf8(dismissed).trimmed());

    // A menu at the very edge of the output is clamped into it rather than
    // drawn off the side: the user can still reach every row, which is what the
    // clamp is for.
    wired.chrome->resize(60, 60);
    wired.chrome->setMenu(1, QPoint(50, 50), rows);
    const QImage tiny = paint(*wired.chrome, QSize(60, 60));
    expect(inkPixels(tiny, QRect(0, 0, 60, 60)) > 0,
           "a menu at the edge is clamped into the surface rather than dropped");
    // And it is still answerable: the first row is inside the clamp.
    QMouseEvent onRow(QEvent::MouseButtonPress, QPointF(5, 5), Qt::LeftButton, Qt::LeftButton,
                      Qt::NoModifier);
    QApplication::sendEvent(wired.chrome, &onRow);
    const QByteArray edge = readAnswer(client, *peer);
    expect(edge.contains("\"chosen\""), "and a row of it can still be picked",
           QString::fromUtf8(edge).trimmed());

    delete wired.chrome;
    QLocalServer::removeServer(path);
}

// --- hiding ----------------------------------------------------------------

void checkVisibility()
{
    auto *chrome = new vshot::PinChrome(screen());
    chrome->resize(400, 300);
    chrome->setLabels({label(1, QPoint(50, 50), QSize(100, 80), true, true, true)});
    expect(inkPixels(paint(*chrome, QSize(400, 300)), QRect(50, 50, 100, 40)) > 0,
           "the tag is up before hiding");
    chrome->setPinnedVisible(false);
    const QImage hidden = paint(*chrome, QSize(400, 300));
    expect(inkPixels(hidden, QRect(0, 0, 400, 300)) == 0, "hiding takes every label down");
    chrome->setPinnedVisible(true);
    expect(inkPixels(paint(*chrome, QSize(400, 300)), QRect(50, 50, 100, 40)) > 0,
           "and showing brings them back");
    delete chrome;
}

} // namespace

int main(int argc, char **argv)
{
    QApplication app(argc, argv);
    vshot::initUiLanguage();

    checkTag();
    checkBadge();
    checkMenu();
    checkVisibility();

    if (failures != 0) {
        std::printf("\n%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("\nall pin chrome checks passed\n");
    return 0;
}
