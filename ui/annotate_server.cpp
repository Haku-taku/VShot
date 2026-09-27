// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "annotate_server.hpp"
#include "annotate_surface.hpp"

#include <QCoreApplication>
#include <QFileInfo>
#include <QGuiApplication>
#include <QHash>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonParseError>
#include <QLocalServer>
#include <QLocalSocket>
#include <QPointer>
#include <QRect>
#include <QScreen>
#include <QSet>
#include <QSocketNotifier>
#include <QTimer>

#include <cerrno>
#include <csignal>
#include <cstdio>
#include <functional>
#include <sys/socket.h>
#include <unistd.h>

namespace vshot {
namespace {

// Self-pipe so a termination signal can wake the Qt event loop safely; the
// handler itself only does an async-signal-safe write().
int g_terminateFd[2] = {-1, -1};

void onTerminateSignal(int)
{
    const char marker = 't';
    const ssize_t written = ::write(g_terminateFd[1], &marker, 1);
    static_cast<void>(written);
}

void installTerminateNotifier(QObject *context, std::function<void()> onTerminate)
{
    if (::socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, g_terminateFd) != 0) {
        return;
    }
    struct sigaction action {};
    action.sa_handler = onTerminateSignal;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_RESTART;
    ::sigaction(SIGTERM, &action, nullptr);
    ::sigaction(SIGINT, &action, nullptr);
    // SIGPIPE would otherwise kill the daemon when a client goes away.
    ::signal(SIGPIPE, SIG_IGN);
    auto *notifier = new QSocketNotifier(g_terminateFd[0], QSocketNotifier::Read, context);
    QObject::connect(notifier, &QSocketNotifier::activated, context,
                     [notifier, onTerminate = std::move(onTerminate)] {
                         notifier->setEnabled(false);
                         onTerminate();
                     });
}

void respond(QLocalSocket *socket, const QJsonObject &payload)
{
    const QByteArray encoded = QJsonDocument(payload).toJson(QJsonDocument::Compact);
    socket->write(encoded);
    socket->write("\n", 1);
    socket->flush();
    socket->disconnectFromServer();
}

// How long a probe waits for the daemon that owns the socket to accept. Local,
// and only reached when the file is really there, so this is a ceiling rather
// than a wait.
constexpr int kProbeMs = 500;

// Whether a live daemon already owns `socketPath`.
//
// Qt's own bind refusal cannot answer this: a socket file left behind by a
// killed daemon and one with a server behind it both fail `listen()` with
// "Address in use", so the two are told apart by connecting instead -- nothing
// answers the stale one.
bool daemonIsRunning(const QString &socketPath)
{
    if (!QFileInfo::exists(socketPath)) {
        return false;
    }
    QLocalSocket probe;
    probe.connectToServer(socketPath);
    if (!probe.waitForConnected(kProbeMs)) {
        return false;
    }
    // Left without a request: the daemon answers only complete lines, so all
    // this costs it is one socket object.
    probe.disconnectFromServer();
    return true;
}

// How often the processes a capture left behind are looked at, in
// milliseconds. A recorder that is killed rather than stopped cleanly never
// sends `capture-end`, so without this poll the toolbar would stay away for
// good; a second is slow enough to cost nothing and quick enough that the
// toolbar is back before the user reaches for it.
constexpr int kCaptureWatchdogMs = 1000;

// Whether one of those processes is still there. `0` means it is; a failure
// with ESRCH means it is gone; any other failure (EPERM for a process that is
// not ours to look at) means it is still there. The caller guarantees a pid
// above 1: `0` and negatives address a process group -- or every process --
// rather than a single one, which is never what a record request names.
bool processAlive(qint64 pid)
{
    return ::kill(static_cast<pid_t>(pid), 0) == 0 || errno != ESRCH;
}

// Owns every annotation surface and dispatches daemon commands. It inherits
// QObject only to reuse the functor-based connect() lifetime; it declares no
// signals or slots of its own, so the build stays moc-free.
class AnnotateServer final : public QObject {
public:
    explicit AnnotateServer(QLocalServer *server)
        : server_(server)
    {
        // One poll however many captures are running; it is armed by the first
        // `capture-begin` that names a pid.
        captureWatchdog_ = new QTimer(this);
        captureWatchdog_->setInterval(kCaptureWatchdogMs);
        connect(captureWatchdog_, &QTimer::timeout, this, [this] { pollCapturePids(); });
    }

    void handleNewConnection()
    {
        while (QLocalSocket *socket = server_->nextPendingConnection()) {
            connect(socket, &QLocalSocket::readyRead, this,
                    [this, socket] { readRequest(socket); });
            connect(socket, &QLocalSocket::disconnected, socket, [this, socket] {
                // A client that leaves mid-request leaves half a line behind,
                // and the buffers are keyed by the socket object: a later
                // socket can be handed the same address and would then be read
                // as carrying a request it never sent.
                buffer_.remove(socket);
                socket->deleteLater();
            });
        }
    }

    // Maps the overlay onto every output before the first request arrives. The
    // client spawns this daemon because the user asked for the drawing to be
    // up, and deliberately sends no command afterwards, so a daemon that
    // started hidden would swallow the request it exists to serve. False means
    // no output could be covered, which is fatal: a resident daemon with no
    // surface has nothing to show and could only lie about it.
    bool startSurfaces()
    {
        for (QScreen *screen : QGuiApplication::screens()) {
            addSurface(screen);
        }
        return !surfaces_.isEmpty();
    }

    // Keeps one surface per output. Unlike the pin stack, a surface here is not
    // created with the content: the overlay is the daemon's whole reason to
    // run, so an output that appears gets one immediately and an output that
    // goes away takes its drawing with it -- the strokes were made against what
    // that screen was showing.
    void watchScreens()
    {
        connect(qApp, &QGuiApplication::screenAdded, this,
                [this](QScreen *screen) { addSurface(screen); });
        connect(qApp, &QGuiApplication::screenRemoved, this,
                [this](QScreen *screen) { dropSurface(screen); });
    }

    // Unmaps every surface before the process goes away. Leaving a mapped
    // layer surface behind can wedge the compositor's output frames.
    void shutdownAll() { destroySurfaces(); }

private:
    static QJsonObject okReply()
    {
        return QJsonObject{{QStringLiteral("ok"), true}};
    }

    static QJsonObject error(const QString &message)
    {
        return QJsonObject{{QStringLiteral("ok"), false},
                           {QStringLiteral("error"), message}};
    }

    // Requests are newline-terminated single JSON objects.
    void readRequest(QLocalSocket *socket)
    {
        buffer_[socket] += socket->readAll();
        const qsizetype newline = buffer_[socket].indexOf('\n');
        if (newline < 0) {
            return; // still streaming
        }
        const QByteArray line = buffer_[socket].left(newline);
        buffer_.remove(socket);

        QJsonParseError parseError;
        const QJsonDocument document = QJsonDocument::fromJson(line, &parseError);
        if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
            respond(socket, error(QStringLiteral("invalid annotate request JSON: %1")
                                      .arg(parseError.errorString())));
            return;
        }
        const QJsonObject request = document.object();
        const bool quit =
            request.value(QStringLiteral("command")).toString() == QStringLiteral("quit");
        const QJsonObject reply = dispatch(request);
        respond(socket, reply);
        // The reply is sent first: the client waits for it, and it would never
        // arrive from a process that had already left the event loop.
        if (quit) {
            quitNow();
        }
    }

    QJsonObject dispatch(const QJsonObject &request)
    {
        const QString command = request.value(QStringLiteral("command")).toString();
        if (command == QStringLiteral("toggle")) {
            setVisible(!anyVisible());
            return okReply();
        }
        if (command == QStringLiteral("show")) {
            setVisible(true);
            return okReply();
        }
        if (command == QStringLiteral("hide")) {
            setVisible(false);
            return okReply();
        }
        if (command == QStringLiteral("clear")) {
            for (const QPointer<AnnotateSurface> &surface : surfaces_) {
                if (surface != nullptr) {
                    surface->clear();
                }
            }
            return okReply();
        }
        if (command == QStringLiteral("capture-begin")) {
            beginCapture(request);
            return okReply();
        }
        if (command == QStringLiteral("capture-end")) {
            endCapture();
            return okReply();
        }
        if (command == QStringLiteral("status")) {
            QJsonObject reply = okReply();
            // `running` is what the caller asked about, and this process
            // answering at all is the whole of the answer.
            reply.insert(QStringLiteral("running"), true);
            reply.insert(QStringLiteral("visible"), anyVisible());
            reply.insert(QStringLiteral("strokes"), static_cast<qint64>(totalStrokes()));
            return reply;
        }
        if (command == QStringLiteral("quit")) {
            return okReply();
        }
        return error(QStringLiteral("unknown annotate command `%1`").arg(command));
    }

    // A capture or a recording is about to start, so the toolbar goes away: it
    // is a window like any other and would be baked into the picture, while the
    // drawing underneath is usually the reason for capturing at all.
    void beginCapture(const QJsonObject &request)
    {
        setToolbarHidden(true);
        bool ok = false;
        const qint64 pid = request.value(QStringLiteral("pid")).toVariant().toLongLong(&ok);
        // A request without a usable pid is still honoured -- the toolbar is
        // hidden -- it just has nothing to watch: the matching `capture-end`
        // remains the only way back, which is what a screenshot sends.
        if (!ok || pid <= 1) {
            return;
        }
        // Watched together rather than one at a time: a screenshot taken while
        // a recording runs is a second capture over the same toolbar, and the
        // toolbar may only come back once neither of them is running.
        capturePids_.insert(pid);
        captureWatchdog_->start();
    }

    // The capture is over, so the toolbar comes back now instead of on the next
    // poll.
    void endCapture()
    {
        captureWatchdog_->stop();
        capturePids_.clear();
        setToolbarHidden(false);
    }

    // Forgets the captures whose process has gone. A recorder can be killed
    // instead of stopped -- a crash, the OOM killer, the user -- and no
    // `capture-end` ever arrives; a toolbar that never comes back is a tool the
    // user cannot use again, so the daemon polls for its own way out.
    void pollCapturePids()
    {
        const QSet<qint64> watched = capturePids_;
        for (const qint64 pid : watched) {
            if (!processAlive(pid)) {
                capturePids_.remove(pid);
            }
        }
        if (capturePids_.isEmpty()) {
            endCapture();
        }
    }

    // Whether the overlay is up. The widgets are asked rather than a flag kept
    // here, because a surface can leave on its own: Escape on a surface hides
    // it, and the daemon is told nothing. Every surface is shown and hidden
    // together, so one of them answers for all of them.
    bool anyVisible() const
    {
        for (const QPointer<AnnotateSurface> &surface : surfaces_) {
            if (surface != nullptr && surface->isVisible()) {
                return true;
            }
        }
        return false;
    }

    // Shows or hides every output's surface. Showing also ends a capture pause:
    // the pause exists only to keep the toolbar out of somebody else's picture,
    // and a user who asks for the overlay back is asking for all of it. Pressing
    // the key again is how a stuck toolbar is recovered without a capture end.
    void setVisible(bool visible)
    {
        for (const QPointer<AnnotateSurface> &surface : surfaces_) {
            if (surface != nullptr) {
                surface->setVisible(visible);
            }
        }
        if (visible) {
            endCapture();
        }
    }

    void setToolbarHidden(bool hidden)
    {
        toolbarHidden_ = hidden;
        for (const QPointer<AnnotateSurface> &surface : surfaces_) {
            if (surface != nullptr) {
                surface->setToolbarHidden(hidden);
            }
        }
    }

    // How much is drawn across the whole desktop. The reply carries the total
    // because the question behind it -- is there anything to save or to clear
    // -- cannot be answered by one surface.
    int totalStrokes() const
    {
        int total = 0;
        for (const QPointer<AnnotateSurface> &surface : surfaces_) {
            if (surface != nullptr) {
                total += surface->strokeCount();
            }
        }
        return total;
    }

    // Quitting looks the same from the socket and from a toolbar's quit button:
    // the surfaces are unmapped first, then the event loop is left. The daemon
    // owns the socket and every output, so a surface never quits on its own.
    void quitNow()
    {
        shutdownAll();
        QCoreApplication::quit();
    }

    // Creates the overlay on one output.
    void addSurface(QScreen *screen)
    {
        if (screen == nullptr || surfaces_.contains(screen)) {
            return;
        }
        // Read before this surface joins: it is shown by `showLayerSurface()`,
        // so it would otherwise answer for itself.
        const bool hideAtBirth = !surfaces_.isEmpty() && !anyVisible();
        auto *surface = new AnnotateSurface(screen);
        // The one gesture the surface cannot act on by itself: the socket, the
        // other outputs and the process are the daemon's to end.
        surface->setQuitCallback([this] { quitNow(); });
        if (!surface->showLayerSurface()) {
            std::fprintf(stderr, "vshot-qt-ui: cannot create an annotation surface for `%s`\n",
                         screen->name().toUtf8().constData());
            std::fflush(stderr);
            delete surface;
            return;
        }
        // An output that arrives later starts where the others are. The
        // surfaces the daemon is born with stay up -- that is the point of
        // starting it -- but one that appears while everything is hidden must
        // not pop up on its own, and one that appears during a capture must not
        // carry the toolbar into it.
        surface->setToolbarHidden(toolbarHidden_);
        if (hideAtBirth) {
            surface->setVisible(false);
        }
        surfaces_.insert(screen, surface);
        QObject::connect(surface, &QObject::destroyed, this, [this, screen] {
            surfaces_.remove(screen);
        });
        followGeometry(screen);
    }

    // An output can also change shape without ever leaving the screen list: a
    // mode change, a rotation, or a rearrangement that moves it. The surface is
    // anchored to all four edges, so the compositor is what reconfigures it;
    // asking for the new size here as well costs nothing, and it keeps the
    // surface's own resizeEvent -- where its drawing canvas is regrown -- the
    // one place a size change has to be handled.
    void followGeometry(QScreen *screen)
    {
        connect(screen, &QScreen::geometryChanged, this, [this, screen](const QRect &geometry) {
            const QPointer<AnnotateSurface> surface = surfaces_.value(screen);
            // Only a real change is passed on: a redundant resize would take
            // the surface through its own resize handling for nothing.
            if (surface != nullptr && surface->size() != geometry.size()) {
                surface->resize(geometry.size());
            }
        });
    }

    // Retires one surface and breaks every connection into the daemon first:
    // the widget is destroyed asynchronously (WA_DeleteOnClose), and by then
    // the daemon may already be gone.
    void detachSurface(AnnotateSurface *surface)
    {
        if (surface == nullptr) {
            return;
        }
        surface->setQuitCallback({});
        QObject::disconnect(surface, nullptr, this, nullptr);
        surface->hide();
        surface->close();
    }

    void dropSurface(QScreen *screen)
    {
        detachSurface(surfaces_.take(screen).data());
    }

    void destroySurfaces()
    {
        const QList<QPointer<AnnotateSurface>> surfaces = surfaces_.values();
        surfaces_.clear();
        for (const QPointer<AnnotateSurface> &surface : surfaces) {
            detachSurface(surface.data());
        }
    }

    QLocalServer *server_;
    QHash<QLocalSocket *, QByteArray> buffer_;
    // One overlay per output. QPointer: a surface can be dismissed by the
    // compositor on its own (an output going away), which would leave a bare
    // pointer behind.
    QHash<QScreen *, QPointer<AnnotateSurface>> surfaces_;
    // Whether the toolbar is currently kept out of the picture. Kept here
    // because a surface created later has to start with the same state as the
    // ones already up, and there is no surface to ask when there are none.
    bool toolbarHidden_ = false;
    // The processes the captures in flight belong to, and the poll that outlives
    // them. Empty means no capture is running.
    QSet<qint64> capturePids_;
    class QTimer *captureWatchdog_ = nullptr;
};

} // namespace

int runAnnotateServer(const QString &socketPath)
{
    // Two `vshot annotate show` calls racing start two daemons. The one that
    // finds the socket already answered leaves the overlay to the daemon that
    // owns it: two daemons would map two overlays onto the same outputs, and
    // only one of them would ever hear `quit`, so the other would stay on
    // screen with no way to take it down.
    if (daemonIsRunning(socketPath)) {
        std::fprintf(stderr, "vshot-qt-ui: an annotation server is already running on `%s`\n",
                     socketPath.toUtf8().constData());
        std::fflush(stderr);
        return 0;
    }
    // A daemon that was killed leaves its socket file behind, and Qt answers
    // `listen()` on such a file exactly as it does on one with a server behind
    // it, so the stale entry has to be dropped before the bind -- otherwise
    // every later start fails and annotation is dead until someone removes the
    // file by hand. The probe above is what keeps this from stealing the socket
    // of a daemon that is still alive.
    QLocalServer::removeServer(socketPath);
    auto *server = new QLocalServer;
    if (!server->listen(socketPath)) {
        std::fprintf(stderr, "vshot-qt-ui: cannot listen on annotation socket `%s`: %s\n",
                     socketPath.toUtf8().constData(), server->errorString().toUtf8().constData());
        return 1;
    }
    std::fprintf(stderr, "vshot-qt-ui: annotation server listening on `%s`\n",
                 socketPath.toUtf8().constData());
    std::fflush(stderr);

    AnnotateServer daemon(server);
    QObject::connect(server, &QLocalServer::newConnection, &daemon,
                     &AnnotateServer::handleNewConnection);
    // close() also removes the listening socket file; otherwise the next
    // spawn would inherit a stale entry (removeServer covers that, but a
    // dangling socket is confusing to users).
    QObject::connect(qApp, &QCoreApplication::aboutToQuit, server, [server, &daemon] {
        daemon.shutdownAll();
        server->close();
    });
    daemon.watchScreens();
    // Mapping the overlay is not a request the client can retry: it spawned
    // this process because the user asked for it and sends no command
    // afterwards, so failing here has to be reported by leaving, not by
    // answering `ok` to a request that cannot be shown.
    if (!daemon.startSurfaces()) {
        const bool anyOutput = !QGuiApplication::screens().isEmpty();
        std::fprintf(stderr, "vshot-qt-ui: cannot map an annotation surface: %s\n",
                     anyOutput ? "the layer-shell protocol is not available (is LayerShellQt "
                                 "installed?)"
                               : "no output is available to draw on");
        std::fflush(stderr);
        daemon.shutdownAll();
        server->close();
        return 1;
    }
    // A signal-killed client can leave a mapped layer surface behind, which
    // is fatal for the compositor's frame loop, so SIGTERM/SIGINT unmap first.
    installTerminateNotifier(&daemon, [] { QCoreApplication::quit(); });
    return QCoreApplication::exec();
}

} // namespace vshot
