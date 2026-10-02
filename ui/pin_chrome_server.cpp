// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "pin_chrome_server.hpp"

#include "i18n.hpp"
#include "pin_chrome.hpp"

#include <QGuiApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QScreen>
#include <QTimer>

#include <cstdio>

namespace vshot {
namespace {

/// How long to wait for the daemon to connect before giving up.  A chrome with
/// no daemon is a window with nothing to say, and leaving it up would be a
/// transparent surface nobody can see or close.
constexpr int kConnectMs = 10'000;

} // namespace

int runPinChrome(const QString &socketPath)
{
    QLocalServer::removeServer(socketPath);
    QLocalServer server;
    if (!server.listen(socketPath)) {
        std::fprintf(stderr, "vshot-qt-ui: cannot listen on %s: %s\n", qPrintable(socketPath),
                     qPrintable(server.errorString()));
        return 1;
    }
    // One surface per output, mapped as they come up: a label belongs on the
    // output the pin it names is on, and a pin can straddle two.
    QHash<QScreen *, PinChrome *> surfaces;
    const auto ensureSurfaces = [&surfaces] {
        for (QScreen *screen : QGuiApplication::screens()) {
            if (screen == nullptr || surfaces.contains(screen)) {
                continue;
            }
            auto *chrome = new PinChrome(screen);
            if (!chrome->showLayerSurface()) {
                delete chrome;
                continue;
            }
            surfaces.insert(screen, chrome);
        }
    };

    QLocalSocket socket;
    QObject::connect(&socket, &QLocalSocket::readyRead, &socket, [&] {
        while (socket.canReadLine()) {
            const QByteArray line = socket.readLine().trimmed();
            if (line.isEmpty()) {
                continue;
            }
            const QJsonDocument document = QJsonDocument::fromJson(line);
            if (!document.isObject()) {
                continue;
            }
            const QJsonObject message = document.object();
            const QString command = message.value(QStringLiteral("cmd")).toString();
            if (command == QStringLiteral("labels")) {
                ensureSurfaces();
                const bool visible = message.value(QStringLiteral("visible")).toBool(true);
                for (PinChrome *chrome : surfaces) {
                    QVector<PinChrome::Label> labels;
                    const QJsonArray pins = message.value(QStringLiteral("pins")).toArray();
                    for (const QJsonValue &value : pins) {
                        const QJsonObject pin = value.toObject();
                        // Only the labels that have something to say: a pin
                        // with no tag and no badge is a rect this surface would
                        // repaint for nothing.
                        const bool hovered = pin.value(QStringLiteral("hovered")).toBool();
                        const bool hdr = pin.value(QStringLiteral("hdr")).toBool();
                        if (!hovered || !hdr) {
                            continue;
                        }
                        PinChrome::Label label;
                        label.id = static_cast<quint64>(
                            pin.value(QStringLiteral("id")).toDouble());
                        label.origin = QPoint(pin.value(QStringLiteral("x")).toInt(),
                                              pin.value(QStringLiteral("y")).toInt());
                        label.size = QSize(pin.value(QStringLiteral("width")).toInt(),
                                           pin.value(QStringLiteral("height")).toInt());
                        label.capturedHdr = hdr;
                        label.shownAsHdr = pin.value(QStringLiteral("shown")).toBool();
                        labels.append(label);
                    }
                    chrome->setPinnedVisible(visible);
                    chrome->setLabels(labels);
                }
            } else if (command == QStringLiteral("badge")) {
                ensureSurfaces();
                const quint64 id = static_cast<quint64>(
                    message.value(QStringLiteral("id")).toDouble());
                const QString text = message.value(QStringLiteral("text")).toString();
                for (PinChrome *chrome : surfaces) {
                    chrome->showBadge(id, text);
                }
            } else if (command == QStringLiteral("quit")) {
                QGuiApplication::quit();
            }
        }
    });
    // A daemon that goes away takes the labels with it: they say what its pins
    // are, and there are no pins left to say it about.
    QObject::connect(&socket, &QLocalSocket::disconnected, qApp, &QGuiApplication::quit);
    // And a daemon that never arrives is not worth a window.
    auto *deadline = new QTimer(qApp);
    deadline->setSingleShot(true);
    QObject::connect(deadline, &QTimer::timeout, qApp, [&socket] {
        if (socket.state() != QLocalSocket::ConnectedState) {
            QGuiApplication::quit();
        }
    });
    deadline->start(kConnectMs);
    socket.connectToServer(socketPath);

    // A screen appearing or going away takes its labels with it.
    QObject::connect(qApp, &QGuiApplication::screenAdded, qApp,
                     [&surfaces](QScreen *screen) {
                         if (screen == nullptr || surfaces.contains(screen)) {
                             return;
                         }
                         auto *chrome = new PinChrome(screen);
                         if (chrome->showLayerSurface()) {
                             surfaces.insert(screen, chrome);
                         } else {
                             delete chrome;
                         }
                     });
    QObject::connect(qApp, &QGuiApplication::screenRemoved, qApp, [&surfaces](QScreen *screen) {
        if (PinChrome *chrome = surfaces.take(screen)) {
            chrome->hide();
            chrome->deleteLater();
        }
    });
    return QGuiApplication::exec();
}

} // namespace vshot
