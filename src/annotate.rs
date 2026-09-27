// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! Screen annotation: `vshot annotate`.
//!
//! The toolbar and everything drawn with it live in a resident Qt surface;
//! this module is only its client.  A request is one JSON object per line over
//! a Unix socket, exactly the shape the pin daemon speaks, so a compositor
//! keybinding can flip the toolbar with no display of its own.
//!
//! Two of the actions carry the whole feature: `toggle` and `show` start the
//! daemon when nothing is listening, and the daemon that comes up maps its
//! surfaces visible by itself.  Everything else — hiding, clearing, quitting —
//! is meaningless without a daemon and so never starts one.
//!
//! A screenshot must never fail, or even slow down, because of annotation
//! (see [`CaptureGuard`]), and a capture flow has to be able to tell the
//! daemon to get the toolbar out of the frame while it runs.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Result, VshotError};
use crate::qt_overlay::helper_program;

/// Where the socket may be redirected, for isolated instances and for tests.
const SOCKET_ENV: &str = "VSHOT_ANNOTATE_SOCKET";

/// The debug switch the Qt side reads: with it set the daemon keeps the
/// terminal's stderr instead of dropping it, which is the only way to see the
/// helper's own diagnostics.
const DAEMON_DEBUG_ENV: &str = "VSHOT_ANNOTATE_DEBUG";

/// Budget for a single request/response round trip with a live daemon.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a capture waits after the daemon has hidden the toolbar.
///
/// One compositor frame: the reply says the client has repainted, and the
/// compositor picks that commit up on its next frame, which is also when the
/// capture that follows reads the screen.  A capture is a user-visible pause
/// already, so a frame is not worth optimising away for the risk of freezing
/// the toolbar into the picture.
const SETTLE: Duration = Duration::from_millis(20);

/// Budget for everything a [`CaptureGuard`] does.  Short on purpose: a capture
/// waits for this, and the daemon is either on the local machine and fast or
/// not worth waiting for at all.
const GUARD_TIMEOUT: Duration = Duration::from_millis(300);

/// One `vshot annotate` control action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotateAction {
    /// Flip the toolbar: hide it when the daemon is showing it, show it when
    /// it is not.  Starts the daemon when none is running.
    Toggle,
    /// Show the toolbar, starting the daemon when none is running.
    Show,
    /// Hide the toolbar.  Never starts a daemon.
    Hide,
    /// Drop every stroke drawn so far.  Never starts a daemon.
    Clear,
    /// End the daemon.  Never starts a daemon.
    Quit,
    /// Report what the daemon holds.  Never starts a daemon.
    Status,
}

/// One request to the resident annotate daemon: a single JSON object per
/// connection, matching the `--annotate-server` protocol in the Qt helper.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub(crate) enum AnnotateCommand {
    Toggle,
    Show,
    Hide,
    Clear,
    Quit,
    Status,
    /// A capture is starting in the process named by `pid`, so the toolbar has
    /// to leave the screen.  The pid is what lets the daemon put the toolbar
    /// back by itself when that process dies without ever sending the matching
    /// `capture-end` — the capture client is killed, not asked to stop.
    CaptureBegin {
        pid: i32,
    },
    /// The capture that hid the toolbar is over; the toolbar may come back.
    CaptureEnd,
}

/// What the daemon answers a request with.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct AnnotateReply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// `status` only: whether annotation is switched on.  The daemon only ever
    /// answers a status while it is running, so this is `true` for any reply
    /// that arrives; it is carried because it is part of the reply.
    #[serde(default)]
    pub running: Option<bool>,
    /// `status` only: whether the toolbar is on screen right now.
    #[serde(default)]
    pub visible: Option<bool>,
    /// `status` only: how many strokes the user has drawn.
    #[serde(default)]
    pub strokes: Option<u64>,
}

impl AnnotateReply {
    /// A refused request is a failure here rather than a silently defaulted
    /// answer, so a caller that asked for something never reports success for
    /// work the daemon did not do.
    fn into_result(self) -> Result<Self> {
        if self.ok {
            return Ok(self);
        }
        Err(VshotError::Annotate(format!(
            "the screen annotation daemon rejected the request: {}",
            self.error.as_deref().unwrap_or("unknown error")
        )))
    }
}

/// Runs one CLI action, printing as it goes.
pub fn run(action: AnnotateAction) -> Result<()> {
    match action {
        AnnotateAction::Toggle => start_or_send(AnnotateCommand::Toggle),
        AnnotateAction::Show => start_or_send(AnnotateCommand::Show),
        AnnotateAction::Hide => send_if_running(AnnotateCommand::Hide),
        AnnotateAction::Clear => send_if_running(AnnotateCommand::Clear),
        AnnotateAction::Quit => send_if_running(AnnotateCommand::Quit),
        AnnotateAction::Status => status(),
    }
}

/// Where the daemon socket lives: `VSHOT_ANNOTATE_SOCKET` overrides everything
/// (isolated instances and tests); otherwise under `XDG_RUNTIME_DIR` when set,
/// falling back to `/tmp`. The uid suffix keeps concurrent users apart.
pub fn socket_path() -> PathBuf {
    if let Some(override_path) = std::env::var_os(SOCKET_ENV) {
        let path = PathBuf::from(override_path);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime.join(format!(
        "vshot-annotate-{}.sock",
        rustix::process::getuid().as_raw()
    ))
}

/// Whether a connect failure only means "no daemon here": nothing is bound to
/// the path at all, or the socket file outlived the daemon that left it.
fn absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Why a request produced no reply.
///
/// A daemon nobody started is the ordinary state of an optional feature, not a
/// failure, but the actions disagree on what to do about it — start one, or
/// treat the request as already satisfied — so the two cases stay apart
/// instead of collapsing into one error.
enum SendFailure {
    /// Nothing owns the socket, and the connect error that said so.
    Absent,
    Failed(VshotError),
}

/// Encodes one request as its single line of JSON: the daemon splits requests
/// on the newline terminator.
fn encode(command: &AnnotateCommand) -> Result<Vec<u8>> {
    let mut payload = serde_json::to_vec(command).map_err(|error| {
        VshotError::Annotate(format!(
            "failed to encode the screen annotation request: {error}"
        ))
    })?;
    payload.push(b'\n');
    Ok(payload)
}

/// Connects to the daemon, with both timeouts set so a wedged daemon cannot
/// hold a CLI invocation open.
fn connect(path: &Path) -> std::result::Result<UnixStream, SendFailure> {
    let stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error) if absent(&error) => return Err(SendFailure::Absent),
        Err(error) => {
            return Err(SendFailure::Failed(VshotError::Annotate(format!(
                "failed to reach the screen annotation socket {}: {error}",
                path.display()
            ))))
        }
    };
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
        .map_err(|error| {
            SendFailure::Failed(VshotError::Annotate(format!(
                "failed to set the screen annotation socket timeout: {error}"
            )))
        })?;
    Ok(stream)
}

/// Sends one request that may start the daemon when nobody is listening.
///
/// Nothing is sent to a daemon this call started: a fresh daemon maps its
/// surfaces visible as it comes up, which is exactly what `toggle` and `show`
/// were asking for, so a `toggle` sent afterwards would undo the request.
fn start_or_send(command: AnnotateCommand) -> Result<()> {
    match send(&socket_path(), &command) {
        Ok(reply) => reply.into_result().map(|_| ()),
        // Two clients racing here can both decide to start a daemon; the one
        // that loses the socket bind exits on its own, and neither sends a
        // command, so the race costs a process and changes no state.
        Err(SendFailure::Absent) => spawn_daemon(),
        Err(SendFailure::Failed(error)) => Err(error),
    }
}

/// Sends one request that only makes sense against a daemon already running.
///
/// Nothing listening is success: hiding a toolbar that does not exist, or
/// quitting a daemon that is not there, is already what the caller asked for.
/// The pin CLI treats `--quit` the same way.
fn send_if_running(command: AnnotateCommand) -> Result<()> {
    match send(&socket_path(), &command) {
        Ok(reply) => reply.into_result().map(|_| ()),
        Err(SendFailure::Absent) => Ok(()),
        Err(SendFailure::Failed(error)) => Err(error),
    }
}

/// Reports what the daemon holds, without starting one: `status` is a question,
/// and a daemon started to answer it would change what it asks about.
fn status() -> Result<()> {
    match send(&socket_path(), &AnnotateCommand::Status) {
        Ok(reply) => {
            if !reply.running.unwrap_or(true) {
                // Only reachable for a daemon that shut down between answering
                // and this line; reporting "0 stroke(s)" for it would read as a
                // toolbar in use, when there is none left to use.
                println!("vshot: no screen annotation is running");
                return Ok(());
            }
            println!(
                "{} stroke(s), {}",
                reply.strokes.unwrap_or(0),
                if reply.visible.unwrap_or(true) {
                    "visible"
                } else {
                    "hidden"
                }
            );
            Ok(())
        }
        Err(SendFailure::Absent) => {
            println!("vshot: no screen annotation is running");
            Ok(())
        }
        Err(SendFailure::Failed(error)) => Err(error),
    }
}

/// Whether the daemon should keep the terminal's stderr.
///
/// It normally drops every stream: it outlives the terminal the CLI ran in and
/// has nothing to say.  The exception is the Qt side's debug switch, whose
/// trace goes to stderr — a trace nobody can read is worse than no trace at
/// all, so the daemon then writes into the terminal that started it.
fn keep_daemon_stderr() -> bool {
    std::env::var_os(DAEMON_DEBUG_ENV).is_some()
}

/// Starts the resident daemon detached: no pipes are inherited, so the CLI
/// returns immediately while the toolbar lives on.
fn spawn_daemon() -> Result<()> {
    let helper = helper_program()?;
    Command::new(&helper.path)
        .arg("--annotate-server")
        .arg(socket_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(if keep_daemon_stderr() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .spawn()
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                VshotError::Annotate(format!(
                    "Qt helper `{}` was not found; build it with `cmake -S . -B build-qt && \
                     cmake --build build-qt` or point VSHOT_QT_HELPER at the executable",
                    helper.path.display()
                ))
            } else {
                VshotError::CommandIo {
                    program: helper.path.display().to_string(),
                    source,
                }
            }
        })?;
    Ok(())
}

/// One request and its reply, with the absent/absent-from-failure split the
/// callers above need.
fn send(path: &Path, command: &AnnotateCommand) -> std::result::Result<AnnotateReply, SendFailure> {
    let payload = encode(command).map_err(SendFailure::Failed)?;
    let mut stream = connect(path)?;
    stream.write_all(&payload).map_err(|error| {
        SendFailure::Failed(VshotError::Annotate(format!(
            "failed to send the screen annotation request: {error}"
        )))
    })?;
    read_reply(&mut stream).map_err(SendFailure::Failed)
}

/// Reads the daemon's one line of reply.  The read stops at the newline the
/// daemon terminates it with; a connection closed before that is a daemon that
/// died mid-request, and surfaces as invalid JSON.
fn read_reply(stream: &mut UnixStream) -> Result<AnnotateReply> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).map_err(|error| {
            VshotError::Annotate(format!(
                "failed to read the screen annotation reply: {error}"
            ))
        })?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.contains(&b'\n') {
            break;
        }
    }
    let reply: AnnotateReply = serde_json::from_slice(&buffer).map_err(|error| {
        VshotError::Annotate(format!(
            "the screen annotation daemon returned invalid JSON: {error}"
        ))
    })?;
    reply.into_result()
}

/// Hides the annotation toolbar for as long as this guard lives.
///
/// Annotation is an optional feature and a screenshot is not: a capture must
/// never fail, report an error, or visibly wait because its daemon is wedged
/// or absent.  So every step here is best effort and bounded by
/// [`GUARD_TIMEOUT`], and a guard that could not reach the daemon at all is
/// inert — the capture then simply happens with the toolbar still on screen,
/// which is the state the user was looking at anyway.
pub struct CaptureGuard {
    /// The connection carrying `capture-end`, kept open for the life of the
    /// guard.  `None` when the `capture-begin` round trip did not finish:
    /// there is then nothing to end, and nothing to wait for on the way out.
    stream: Option<UnixStream>,
}

impl CaptureGuard {
    /// For the screenshot path: taken for the whole capture flow, so every
    /// route in `main.rs::run` is covered.
    pub fn for_screenshot() -> Self {
        Self::begin()
    }

    /// For a recording session: held for the life of the recording.
    pub fn for_recording() -> Self {
        Self::begin()
    }

    fn begin() -> Self {
        Self {
            stream: capture_begin(),
        }
    }
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        // The reply is deliberately not read: this runs on the way out of a
        // capture, and waiting for an answer nobody uses is the delay the
        // short write timeout exists to bound.  Every failure is ignored for
        // the same reason the guard exists at all — annotation must stay out
        // of the screenshot's way.
        let Ok(payload) = encode(&AnnotateCommand::CaptureEnd) else {
            return;
        };
        let _ = stream.write_all(&payload);
    }
}

/// Tells the daemon a capture is starting, returning the connection that will
/// tell it the capture is over.
///
/// This never starts a daemon: a screenshot is not worth a Qt process, and
/// without one there is no toolbar to hide.  Anything outside the guard's
/// budget — a connect that hangs, a daemon that does not answer — gives `None`
/// so the capture goes ahead regardless.
fn capture_begin() -> Option<UnixStream> {
    let mut stream = UnixStream::connect(socket_path()).ok()?;
    stream.set_write_timeout(Some(GUARD_TIMEOUT)).ok()?;
    stream.set_read_timeout(Some(GUARD_TIMEOUT)).ok()?;
    let payload = encode(&AnnotateCommand::CaptureBegin { pid: current_pid() }).ok()?;
    stream.write_all(&payload).ok()?;
    // The daemon hides the toolbar before it replies, so a reply that never
    // arrives is not a daemon that did nothing: the guard gives up anyway,
    // because waiting is a delay on the capture it exists to serve, and the
    // pid it just sent is what brings the toolbar back once this process is
    // gone.
    read_reply(&mut stream).ok()?;
    // The reply says the daemon has repainted; it does not say the compositor
    // has drawn that repaint.  A commit from another client is picked up on the
    // compositor's next frame, and the capture that follows this call is a
    // frame too -- so without this pause the very frame the user is about to
    // look at can still hold the toolbar.  One frame's worth of slack, paid
    // only when a daemon is actually there to hide something.
    std::thread::sleep(SETTLE);
    Some(stream)
}

/// This process's pid, in the signed width the wire carries.  The daemon needs
/// it to restore the toolbar if this process is killed instead of exiting
/// cleanly, so it has to be the pid of the process holding the guard.
fn current_pid() -> i32 {
    // Linux pids fit in 22 bits and so cannot actually clamp; the conversion
    // is checked only because the wire type is signed.
    i32::try_from(std::process::id()).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    use super::*;

    /// The environment is process-wide and the tests in a binary run in
    /// parallel, so every test that reads or writes the socket variables
    /// holds this: one test's override must not decide another's answer.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn socket_path_prefers_the_override_and_defaults_to_the_runtime_dir() {
        let _env = env_lock();
        let saved_override = std::env::var_os(SOCKET_ENV);
        let saved_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        let runtime = tempfile::tempdir().expect("temp dir");

        std::env::remove_var(SOCKET_ENV);
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        let expected = runtime.path().join(format!(
            "vshot-annotate-{}.sock",
            rustix::process::getuid().as_raw()
        ));
        assert_eq!(socket_path(), expected);
        // An empty override is not an override: it falls back the same way.
        std::env::set_var(SOCKET_ENV, "");
        assert_eq!(socket_path(), expected);
        // A real one wins over the runtime directory.
        std::env::set_var(SOCKET_ENV, "/tmp/custom-annotate.sock");
        assert_eq!(socket_path(), PathBuf::from("/tmp/custom-annotate.sock"));

        match saved_override {
            Some(value) => std::env::set_var(SOCKET_ENV, value),
            None => std::env::remove_var(SOCKET_ENV),
        }
        match saved_runtime {
            Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn every_request_encodes_to_its_wire_line() {
        // One JSON object per line, and the line ends in a newline the daemon
        // splits on. Compared as parsed values, so key order cannot matter.
        let encoded = |command: &AnnotateCommand| -> serde_json::Value {
            let payload = encode(command).unwrap();
            assert_eq!(payload.last().copied(), Some(b'\n'), "{payload:?}");
            serde_json::from_slice(&payload[..payload.len() - 1]).unwrap()
        };

        assert_eq!(
            encoded(&AnnotateCommand::Toggle),
            serde_json::json!({"command": "toggle"})
        );
        assert_eq!(
            encoded(&AnnotateCommand::Show),
            serde_json::json!({"command": "show"})
        );
        assert_eq!(
            encoded(&AnnotateCommand::Hide),
            serde_json::json!({"command": "hide"})
        );
        assert_eq!(
            encoded(&AnnotateCommand::Clear),
            serde_json::json!({"command": "clear"})
        );
        assert_eq!(
            encoded(&AnnotateCommand::Quit),
            serde_json::json!({"command": "quit"})
        );
        assert_eq!(
            encoded(&AnnotateCommand::Status),
            serde_json::json!({"command": "status"})
        );
        assert_eq!(
            encoded(&AnnotateCommand::CaptureBegin { pid: 4711 }),
            serde_json::json!({"command": "capture-begin", "pid": 4711})
        );
        assert_eq!(
            encoded(&AnnotateCommand::CaptureEnd),
            serde_json::json!({"command": "capture-end"})
        );
    }

    #[test]
    fn a_status_reply_carries_visibility_and_strokes() {
        let reply: AnnotateReply =
            serde_json::from_str(r#"{"ok":true,"running":true,"visible":false,"strokes":7}"#)
                .unwrap();
        let reply = reply.into_result().unwrap();
        assert_eq!(reply.running, Some(true));
        assert_eq!(reply.visible, Some(false));
        assert_eq!(reply.strokes, Some(7));

        // A refused request is an error, never a silently defaulted status.
        let reply: AnnotateReply =
            serde_json::from_str(r#"{"ok":false,"error":"no such command"}"#).unwrap();
        let error = reply.into_result().unwrap_err();
        assert!(error.to_string().contains("no such command"), "{error}");
    }

    #[test]
    fn a_guard_without_a_daemon_is_inert_and_never_blocks() {
        let _env = env_lock();
        let saved_override = std::env::var_os(SOCKET_ENV);
        let directory = tempfile::tempdir().expect("temp dir");

        // Nothing at the path: the connect fails at once and there is nothing
        // to send, so the guard is inert and costs nothing.
        std::env::set_var(SOCKET_ENV, directory.path().join("missing.sock"));
        let started = Instant::now();
        let guard = CaptureGuard::for_screenshot();
        let took = Instant::now() - started;
        assert!(guard.stream.is_none(), "a missing daemon leaves no stream");
        assert!(took < Duration::from_secs(1), "the guard waited {took:?}");
        let started = Instant::now();
        drop(guard);
        let took = Instant::now() - started;
        assert!(took < Duration::from_secs(1), "the drop waited {took:?}");

        // A listener that never accepts stands in for a wedged daemon: the
        // connect succeeds and the wait is the guard's own timeout.  It still
        // returns in time, still inert, and still drops immediately.
        let socket = directory.path().join("wedged.sock");
        let _listener = UnixListener::bind(&socket).expect("listener");
        std::env::set_var(SOCKET_ENV, &socket);
        let started = Instant::now();
        let guard = CaptureGuard::for_recording();
        let took = Instant::now() - started;
        assert!(guard.stream.is_none(), "a wedged daemon leaves no stream");
        assert!(took < Duration::from_secs(3), "the guard waited {took:?}");
        let started = Instant::now();
        drop(guard);
        let took = Instant::now() - started;
        assert!(took < Duration::from_secs(1), "the drop waited {took:?}");

        match saved_override {
            Some(value) => std::env::set_var(SOCKET_ENV, value),
            None => std::env::remove_var(SOCKET_ENV),
        }
    }

    #[test]
    fn the_debug_switch_is_what_keeps_the_daemons_stderr() {
        let _env = env_lock();
        let saved_debug = std::env::var_os(DAEMON_DEBUG_ENV);

        std::env::set_var(DAEMON_DEBUG_ENV, "1");
        assert!(keep_daemon_stderr());
        std::env::remove_var(DAEMON_DEBUG_ENV);
        assert!(!keep_daemon_stderr());

        match saved_debug {
            Some(value) => std::env::set_var(DAEMON_DEBUG_ENV, value),
            None => std::env::remove_var(DAEMON_DEBUG_ENV),
        }
    }
}
