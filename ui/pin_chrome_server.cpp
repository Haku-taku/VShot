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
    // The connection the daemon makes.  It is taken from the server's backlog
    // rather than dialled: the daemon is the client here, and a listening
    // socket whose connection is never accepted has a backlog nobody reads.
    QLocalSocket *peer = nullptr;

    const auto ensureSurfaces = [&surfaces, &peer] {
        for (QScreen *screen : QGuiApplication::screens()) {
            if (screen == nullptr || surfaces.contains(screen)) {
                continue;
            }
            auto *chrome = new PinChrome(screen);
            if (!chrome->showLayerSurface()) {
                delete chrome;
                continue;
            }
            // A surface made after the connection came up needs the socket too,
            // or its menu would have no way to answer.
            if (peer != nullptr) {
                chrome->setSocket(peer);
            }
            surfaces.insert(screen, chrome);
        }
    };

    QObject::connect(&server, &QLocalServer::newConnection, &server, [&] {
        if (peer != nullptr) {
            // One daemon, one connection: a second is somebody else's socket
            // file, or a stale attempt.
            server.nextPendingConnection()->deleteLater();
            return;
        }
        peer = server.nextPendingConnection();
        if (peer == nullptr) {
            return;
        }
        for (PinChrome *chrome : surfaces) {
            chrome->setSocket(peer);
        }
        // A daemon that goes away takes the labels with it: they say what its
        // pins are, and there are no pins left to say it about.
        QObject::connect(peer, &QLocalSocket::disconnected, qApp, &QGuiApplication::quit);
        QObject::connect(peer, &QLocalSocket::readyRead, peer, [&] {
            while (peer->canReadLine()) {
                const QByteArray line = peer->readLine().trimmed();
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
                    const QJsonArray pins = message.value(QStringLiteral("pins")).toArray();
                    for (PinChrome *chrome : surfaces) {
                        QVector<PinChrome::Label> labels;
                        for (const QJsonValue &value : pins) {
                            const QJsonObject pin = value.toObject();
                            // Every pin, not only the ones with something to
                            // say this instant: the badge is about a pin the
                            // pointer is nowhere near -- a save reports from a
                            // menu that has closed -- and a label dropped here
                            // has nowhere to appear.
                            PinChrome::Label label;
                            label.id = static_cast<quint64>(
                                pin.value(QStringLiteral("id")).toDouble());
                            label.origin = QPoint(pin.value(QStringLiteral("x")).toInt(),
                                                  pin.value(QStringLiteral("y")).toInt());
                            label.size = QSize(pin.value(QStringLiteral("width")).toInt(),
                                               pin.value(QStringLiteral("height")).toInt());
                            label.capturedHdr = pin.value(QStringLiteral("hdr")).toBool();
                            label.shownAsHdr = pin.value(QStringLiteral("shown")).toBool();
                            label.hovered = pin.value(QStringLiteral("hovered")).toBool();
                            labels.append(label);
                        }
                        chrome->setPinnedVisible(visible);
                        chrome->setLabels(labels);
                    }
                } else if (command == QStringLiteral("menu")) {
                    ensureSurfaces();
                    // No `id` is "take the menu down".
                    const bool open = message.contains(QStringLiteral("id"));
                    const quint64 id = static_cast<quint64>(
                        message.value(QStringLiteral("id")).toDouble());
                    const QPoint anchor(message.value(QStringLiteral("x")).toInt(),
                                        message.value(QStringLiteral("y")).toInt());
                    QStringList rows;
                    for (const QJsonValue &row : message.value(QStringLiteral("rows")).toArray()) {
                        rows.append(row.toString());
                    }
                    for (PinChrome *chrome : surfaces) {
                        if (open) {
                            chrome->setMenu(id, anchor, rows);
                        } else {
                            chrome->setMenu(0, QPoint(), QStringList());
                        }
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
    });

    // A daemon that never arrives is not worth a window.
    auto *deadline = new QTimer(qApp);
    deadline->setSingleShot(true);
    QObject::connect(deadline, &QTimer::timeout, qApp, [&peer] {
        if (peer == nullptr) {
            QGuiApplication::quit();
        }
    });
    deadline->start(kConnectMs);

    // A screen appearing or going away takes its labels with it.
    QObject::connect(qApp, &QGuiApplication::screenAdded, qApp, [&surfaces, &peer](QScreen *screen) {
        if (screen == nullptr || surfaces.contains(screen)) {
            return;
        }
        auto *chrome = new PinChrome(screen);
        if (chrome->showLayerSurface()) {
            if (peer != nullptr) {
                chrome->setSocket(peer);
            }
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
