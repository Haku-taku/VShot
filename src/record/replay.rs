// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! Instant replay: `vshot replay`.
//!
//! A replay is a recording that keeps its last window of history in memory
//! instead of on disk.  It runs the same frame source, the same GPU encoder
//! and the same audio side as `record`; the one difference is where the
//! encoded packets go — an in-memory ring rather than an MP4 muxer.  Nothing
//! is written until a save is triggered, and then the packets already in the
//! ring are copied straight into a fresh MP4 (a remux, no re-encode), so the
//! steady state costs one encode and one in-memory push, and the trigger costs
//! one mux.
//!
//! # Why this shape
//!
//! The alternative — keeping raw frames and encoding them when the user hits
//! the key — is what the numbers rule out: 4K60 NV12 is ~12 MB a frame, so 30
//! seconds is ~22 GB, while the same window as encoded packets at 30 Mbps is
//! ~110 MB.  A replay therefore has to encode continuously; the only real
//! choices are how much history and how cheaply.  The ring keeps the window as
//! packets (bounded, small), the encoder is bounded-GOP (so the ring is a
//! fraction of an all-intra stream and every GOP boundary is a valid start),
//! and a save is a stream copy.
//!
//! # How it is driven
//!
//! `vshot replay start` runs the session in the foreground (or detaches with
//! `--background`) and owns the pid file and the ring.  A save is triggered by
//! `vshot replay save`, which writes a one-line request into a control socket
//! the session owns; the session answers by writing the file and reporting
//! where it went.  The control channel is the same shape the pin daemon uses,
//! so a compositor keybinding can trigger a save with no display of its own.
//!
//! The session is single-threaded: the control socket is polled between
//! frames, so a save is handled at a frame boundary with the ring consistent.
//! A save that asks for more than the ring holds gets everything there is.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::capture::Capturer;
use crate::error::{Result, VshotError};
use crate::wayland::WaylandSession;

use super::avcodec::{ReplayRecorder, VideoCodec};
use super::window::{FollowPolicy, WindowEnd};
use super::{debug_enabled, resolve_source, sleep_interruptible, Source};
use crate::capture::window::{CompositorWindowProvider, ProcessWindowProvider};
use crate::capture::window_copy::{self, Follow, Name, WindowCapture};

/// How many times a second frames are taken at most, when `--fps` says
/// nothing.  A replay defaults lower than a recording: it is left running for
/// long stretches, and 30 fps halves the encoder's work while a replay of
/// motion is still smooth.
pub const DEFAULT_FPS: u32 = 30;

/// The ring's window when `--window` and the config say nothing, in seconds.
pub const DEFAULT_WINDOW: u64 = 30;

/// The key-frame distance when `--gop` and the config say nothing, in
/// seconds.  One second is the usual compromise: the ring is a fraction of an
/// all-intra stream, and a save starts within a second of the requested edge.
pub const DEFAULT_GOP: u64 = 1;

/// The longest GOP a save can still start from: with a ten-second key-frame
/// distance the ring has to hold the whole GOP past the window, which is why
/// the flag is bounded rather than free.
const MAX_GOP: u64 = 10;

/// A parsed replay request, from `vshot replay start`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayRequest {
    /// What to record: a monitor, the whole desktop, a region or a window —
    /// the same shapes `record` takes.
    pub target: super::RecordTarget,
    /// Seconds of history the ring keeps.
    pub window: u64,
    pub fps: u32,
    /// The video codec to encode with.
    pub encoder: VideoCodec,
    /// Which hardware encoder runs the session (`--encoder-backend`), as on
    /// the recording side.
    pub encoder_backend: crate::record::avcodec::EncoderBackend,
    /// Draw the cursor into the frames, like `--cursor` for screenshots.
    pub cursor: bool,
    /// Keep the microphone in the ring, and which input.
    pub mic: Option<super::MicChoice>,
    /// Keep the recorded window's own application's audio in the ring
    /// (`--app-audio`), as `record` does.
    pub app_audio: bool,
    /// Take frames from the desktop portal instead of the compositor's own
    /// protocols.
    pub portal: bool,
    /// Where a save lands, when one is triggered without its own path.
    pub save_dir: Option<PathBuf>,
    /// The key-frame distance, in seconds (1-10).
    pub gop_secs: u64,
    /// The windows a `--follow` window replay moves between, as on the
    /// recording side: the ring keeps one window at a time, switching to
    /// whichever of them the focus lands on, and stays where it is when the
    /// focus is anywhere else.
    pub follow: Vec<String>,
    /// The target bitrate in Mbit/s (`--bitrate`), or `None` to derive one
    /// from the ring's own frame size.  A ring is the one place a runaway
    /// bitrate is worst — it is held in memory, and `--window` seconds of it
    /// have to fit — so the derived default is the one a session should use
    /// unless its caller says otherwise.
    pub bitrate: Option<u32>,
    /// The encoder's level (`--quality`), on the codec's own scale and passed
    /// through as written, or `None` to let the target bitrate decide alone.
    /// See `RecordRequest::quality`.
    pub quality: Option<u16>,
}

impl ReplayRequest {
    fn frame_interval(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.fps.max(1)))
    }

    /// What this session asks of the encoder's rate control.  The ring and the
    /// file answer the same question, so this is the recording side's method
    /// with the same two fields behind it.
    pub(crate) fn rate_control(&self) -> crate::record::avcodec::RateControl {
        crate::record::avcodec::RateControl {
            bitrate: self.bitrate.map(|mbps| i64::from(mbps) * 1_000_000),
            quality: self.quality,
        }
    }

    fn gop_frames(&self) -> u32 {
        u32::try_from((self.fps.max(1) as u64).saturating_mul(self.gop_secs.clamp(1, MAX_GOP)))
            .unwrap_or(u32::MAX)
    }
}

/// Where the control socket lives: `VSHOT_REPLAY_SOCKET` overrides, else
/// `$XDG_RUNTIME_DIR/vshot-replay-<uid>.sock`, else `/tmp`.  Separate from the
/// recording's pid file so the two never collide.
pub fn socket_path() -> PathBuf {
    if let Some(override_path) = std::env::var_os("VSHOT_REPLAY_SOCKET") {
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
        "vshot-replay-{}.sock",
        rustix::process::getuid().as_raw()
    ))
}

/// The pid file the session writes, so `replay stop` can signal it.
fn pid_file() -> PathBuf {
    super::pid_file_named("replay")
}

/// Whether a replay is already running, from the pid file's point of view.
fn running_pid() -> Option<i32> {
    let text = std::fs::read_to_string(pid_file()).ok()?;
    let pid: i32 = text.trim().parse().ok()?;
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    alive.then_some(pid)
}

/// One line of the control protocol.  `Save` carries an optional path and an
/// optional number of seconds to take from the ring; `Stop` ends the session.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
enum ReplayCommand {
    /// Write the last `seconds` of the ring (all of it when `seconds` is
    /// absent or zero) to `path`, or the session's own default when absent.
    /// `save_dir` is `replay save --save-dir`: the directory the session names
    /// the file in when the request carries no path of its own.
    Save {
        #[serde(default)]
        path: Option<PathBuf>,
        #[serde(default)]
        seconds: Option<u64>,
        #[serde(default)]
        save_dir: Option<PathBuf>,
    },
    /// How much history the ring holds, and how many saves it has served.
    Status,
    /// End the session.
    Stop,
}

/// What the session answers a control line with.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reply", rename_all = "kebab-case")]
enum ReplayReply {
    /// A save finished: the file and how much it holds.
    Saved { path: PathBuf, seconds: f64 },
    /// What the ring holds right now, and how many saves the session has
    /// served.  `idle` is the session not recording a window at this moment — a
    /// window replay whose window is not there.  The two are independent: an
    /// idle session that recorded a window before still holds that window's
    /// last seconds, and a save writes them; a session that has recorded
    /// nothing yet answers a save with nothing to save.
    Status {
        span_seconds: f64,
        saves: u64,
        #[serde(default)]
        idle: bool,
    },
    /// The session is ending.
    Stopping,
    /// Something went wrong; the reason is the text.
    Error { message: String },
}

/// How many saves this replay session has served.
///
/// The session's own count rather than a ring's: a window replay opens a ring
/// per window now, and a number that went back to zero every time the focus
/// moved would be answering a question nobody asked.  One replay runs per
/// process — the pid file is what enforces that — so a counter here is the
/// session's.
static SAVES_SERVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn saves_so_far() -> u64 {
    SAVES_SERVED.load(Ordering::Relaxed)
}

/// Sends one control line to a running session, starting nothing (a replay is
/// long-lived; `save` never spawns it).
fn send_command(command: &ReplayCommand) -> Result<ReplayReply> {
    let path = socket_path();
    let mut payload = serde_json::to_vec(command).map_err(|error| {
        VshotError::Recording(format!("failed to encode replay request: {error}"))
    })?;
    payload.push(b'\n');
    let mut stream = UnixStream::connect(&path).map_err(|error| {
        VshotError::Recording(format!(
            "no replay is running (nothing answers on {}): {error}",
            path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| {
            VshotError::Recording(format!("could not set the replay socket timeout: {error}"))
        })?;
    stream.write_all(&payload).map_err(|error| {
        VshotError::Recording(format!("failed to send the replay request: {error}"))
    })?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).map_err(|error| {
            VshotError::Recording(format!("failed to read the replay reply: {error}"))
        })?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.contains(&b'\n') {
            break;
        }
    }
    let reply: ReplayReply = serde_json::from_slice(&buffer).map_err(|error| {
        VshotError::Recording(format!("the replay session returned invalid JSON: {error}"))
    })?;
    Ok(reply)
}

/// `vshot replay save`: asks the running session to write its history to a
/// file and reports where it went.  `save_dir` is `--save-dir`, which names
/// the directory for this one save when no path is given.
pub fn save(
    path: Option<PathBuf>,
    seconds: Option<u64>,
    save_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    match send_command(&ReplayCommand::Save {
        path,
        seconds,
        save_dir,
    })? {
        ReplayReply::Saved { path, seconds } => {
            eprintln!("vshot: saved {seconds:.1}s into {}", path.display());
            Ok(path)
        }
        // The session's own sentence is already worded for the user.
        ReplayReply::Error { message } => Err(VshotError::Bare(message)),
        other => Err(VshotError::Recording(format!(
            "the replay session answered a save with {other:?}"
        ))),
    }
}

/// `vshot replay status`: what the running session holds.
pub fn status() -> Result<()> {
    match send_command(&ReplayCommand::Status)? {
        ReplayReply::Status {
            span_seconds,
            saves,
            idle,
        } => {
            // Two columns and a word, so a script can read the numbers and a
            // person can read the state.  Idle and the length are independent:
            // an idle session is not recording a window, and the seconds are
            // whatever the last one left in the ring -- which a save can still
            // write.
            println!(
                "{span_seconds:.1}\t{saves}\t{}",
                if idle { "idle" } else { "live" }
            );
            Ok(())
        }
        ReplayReply::Error { message } => Err(VshotError::Bare(message)),
        other => Err(VshotError::Recording(format!(
            "the replay session answered a status with {other:?}"
        ))),
    }
}

/// `vshot replay stop`: asks the session to end and waits for it to leave.
pub fn stop() -> Result<()> {
    match send_command(&ReplayCommand::Stop) {
        Ok(ReplayReply::Stopping) => {}
        Ok(ReplayReply::Error { message }) => return Err(VshotError::Bare(message)),
        Ok(other) => {
            return Err(VshotError::Recording(format!(
                "the replay session answered a stop with {other:?}"
            )))
        }
        // A session that has already died but left its socket: fall through to
        // the pid file, which is the authority on whether it is really gone.
        Err(_) => {}
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if running_pid().is_none() {
            let _ = std::fs::remove_file(socket_path());
            println!("vshot: replay stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(VshotError::Recording(
        "the replay is still finishing after 10 seconds".into(),
    ))
}

/// Expands the `%`-conversions in a template the way the rest of vshot does,
/// and answers the text unchanged when it holds a `%` that is not one.
///
/// `chrono`'s own formatting panics on a stray `%`, and a filename is not a
/// place to find that out: `--save-dir /tmp/50%off` is a directory someone can
/// have, and it has to stay one.
fn expand_strftime(template: &str) -> String {
    use chrono::format::{Item, StrftimeItems};

    let items: Vec<Item<'_>> = StrftimeItems::new(template).collect();
    if items.iter().any(|item| matches!(item, Item::Error)) {
        return template.to_owned();
    }
    chrono::Local::now()
        .format_with_items(items.into_iter())
        .to_string()
}

/// The save path for a request: the session's own default when the control
/// line names none.  The directory is made either way — a `--save-dir` is a
/// strftime pattern, so it names one that may not exist yet.
///
/// Only the names vshot builds itself are templates: `--save-dir` and the
/// timestamped default are documented as strftime-expanded, and a PATH the
/// user typed is used exactly as given.  Expanding a typed path would rewrite
/// `a%m.mp4` into `a09.mp4` without saying so, and the path is the user's to
/// name.
fn resolve_save_path(
    requested: Option<&Path>,
    save_dir: Option<&Path>,
    label: &str,
) -> Result<PathBuf> {
    let path = match requested {
        Some(path) => path.to_path_buf(),
        None => {
            let base = match save_dir {
                Some(dir) => dir.to_path_buf(),
                None => super::videos_directory().ok_or_else(|| {
                    VshotError::Recording(
                        "no videos directory is known ($XDG_VIDEOS_DIR, the user-dirs file, or \
                         $HOME/Videos); name the file with `replay save PATH`"
                            .into(),
                    )
                })?,
            };
            let name = format!("{label}-%Y%m%d-%H%M%S.mp4");
            PathBuf::from(expand_strftime(&base.join(name).to_string_lossy()))
        }
    };
    let mut path = path;
    if path.extension().and_then(|ext| ext.to_str()) != Some("mp4") {
        path.set_extension("mp4");
    }
    crate::output::create_parent_directories(&path)?;
    Ok(path)
}

/// Runs a replay session to completion.  This is `vshot replay start`: it owns
/// the ring, the pid file and the control socket, and returns when the session
/// is stopped.
///
/// The portal is refused before this point, by the caller: a detached session
/// is a child process whose stderr goes nowhere, so the refusal has to come
/// from the process the user is watching.
pub fn run(request: &ReplayRequest) -> Result<()> {
    if let Some(pid) = running_pid() {
        return Err(VshotError::Recording(format!(
            "a replay is already running (pid {pid}); stop it with `vshot replay stop` first"
        )));
    }
    if request.window == 0 {
        return Err(VshotError::Recording(
            "the replay window has to be at least one second".into(),
        ));
    }

    // The control socket and the stop handler are the session's own, whatever
    // the frame source; both loops share them.
    let listener = bind_control_socket()?;
    listener.set_nonblocking(true).map_err(|error| {
        VshotError::Recording(format!(
            "could not set the replay socket non-blocking: {error}"
        ))
    })?;
    let interrupted = super::install_stop_handler()?;
    super::write_pid_file_named("replay")?;

    // A window replay has a different frame source — the compositor's own copy
    // of one window — so it runs its own loop; everything around it (the ring,
    // the control socket, the pid file) is shared.
    let outcome = match &request.target {
        super::RecordTarget::Window(target) => {
            window_session(request, target, &listener, &interrupted)
        }
        _ => screen_session(request, &listener, &interrupted),
    };

    let _ = std::fs::remove_file(pid_file());
    let _ = std::fs::remove_file(socket_path());
    outcome
}

/// The screen (monitor/all/region) replay session: the shared frame source,
/// encoded into the ring.
fn screen_session(
    request: &ReplayRequest,
    listener: &UnixListener,
    interrupted: &AtomicBool,
) -> Result<()> {
    // --- the frame source and its geometry --------------------------------
    let wayland = WaylandSession::connect()?;
    let topology = wayland.output_infos()?;
    let mut capture = Capturer::connect()?;
    let (source, (encoded_width, encoded_height)) =
        resolve_source(&request.target, &mut capture, &topology, request.cursor)?;

    // --- the microphone ---------------------------------------------------
    // A screen, the whole desktop or a region has no single application, so
    // `--app-audio` has nothing to attach to here (the CLI refuses it); the
    // soundtrack is the microphone the request asked for.
    let mut mic = super::open_soundtrack(request.mic.as_ref())?;
    let mic_format = mic.format();

    // --- the encoder and the ring -----------------------------------------
    // `auto` resolves once, here, so the probe and the open agree; NVENC has
    // no dma-buf import and always takes the software path.
    let backend = request.encoder_backend.resolve();
    let dmabuf_fourcc = if backend == crate::record::avcodec::EncoderBackend::Nvenc {
        None
    } else {
        super::probe_zero_copy(&mut capture, &source, encoded_width, encoded_height)
    };
    let gop_frames = request.gop_frames();
    let retention = request
        .window
        .saturating_add(request.gop_secs.clamp(1, MAX_GOP));
    let mut recorder = ReplayRecorder::start(
        encoded_width,
        encoded_height,
        request.encoder,
        dmabuf_fourcc,
        mic_format,
        retention,
        gop_frames,
        request.fps,
        backend,
        request.rate_control(),
    )?;
    if debug_enabled() {
        eprintln!(
            "vshot: replay {encoded_width}x{encoded_height} at {} fps with {} ({}) through \
             libavcodec {}, keeping {}s in memory (GOP {} frames)",
            request.fps,
            request.encoder.word(),
            backend.word(),
            super::avcodec::libavcodec_version(),
            request.window,
            gop_frames
        );
        if recorder.audio_channels() > 0 {
            eprintln!(
                "vshot: microphone kept in the ring: {} Hz, {} channel(s), AAC",
                recorder.audio_rate(),
                recorder.audio_channels()
            );
        }
    }

    mic.arm();
    session_loop(
        &mut capture,
        &source,
        request,
        dmabuf_fourcc.is_some(),
        &mut recorder,
        &mut mic,
        listener,
        interrupted,
    )
}

/// The window replay session: the compositor's own copy of one window,
/// encoded into the ring.  Reuses the recording window loop through the
/// [`crate::record::avcodec::VideoSink`] the ring implements, with the control
/// socket polled between frames.
///
/// Which loop depends on what the session speaks, exactly as on the recording
/// side: the wlroots route copies the window through
/// `ext_image_copy_capture_v1`, a Plasma session has no such protocol and
/// copies it through KWin's own `ScreenShot2.CaptureWindow`, and a compositor
/// with neither — niri — casts it through its own screen-cast service.
fn window_session(
    request: &ReplayRequest,
    target: &super::WindowTarget,
    listener: &UnixListener,
    interrupted: &AtomicBool,
) -> Result<()> {
    if crate::capture::active_output::Session::detect()
        == crate::capture::active_output::Session::KWin
    {
        return window_replay_session(request, target, listener, interrupted, WindowRoute::KWin);
    }
    // The same fallback a window *recording* takes, because it is the same
    // frame source; only the sink differs — a ring instead of a file.
    if !super::window_capture_supported() && super::screencast::available() {
        return window_replay_session(request, target, listener, interrupted, WindowRoute::Cast);
    }
    window_replay_session(request, target, listener, interrupted, WindowRoute::Copy)
}

/// Where a window replay's pixels come from, and with them the two answers the
/// supervisor reads between sessions: the window list and the focus.
///
/// The route is also what decides how a window is opened and closed, so the
/// supervisor's loop is one piece of code with this in it rather than three
/// near-copies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowRoute {
    /// `ext_image_copy_capture_v1`: the compositor's own copy of the window
    /// (Hyprland, Sway and the other wlroots sessions).
    Copy,
    /// KWin's `ScreenShot2.CaptureWindow`, through a scripting probe for the
    /// window list and the focus.
    KWin,
    /// The compositor's screen-cast service, where it has one and speaks
    /// neither of the other two (niri).
    Cast,
}

impl WindowRoute {
    /// The window a target names, in the shape this route's sessions want.
    fn resolve(self, target: &super::WindowTarget) -> Result<Name> {
        match self {
            WindowRoute::KWin => super::kwin_window::resolve_window_name(target),
            // A cast window is found in the same list a copy is, because the
            // cast service is aimed by the toplevel identifier the list
            // carries — see `screencast::window_id`.
            WindowRoute::Copy | WindowRoute::Cast => super::window::resolve_window_name(target),
        }
    }
}

/// A window replay: one session after another, with a standby state in
/// between.
///
/// A recording moves from window to window inside one file — the new window's
/// frames are fitted into the canvas the file was opened with — but a replay
/// cannot: its ring holds one frame size, and the second window would have to
/// be shrunk into the first one's canvas for the whole session.  So a switch of
/// window is a *new* session here: the capture, the audio and the ring of the
/// old one are dropped, and the new window gets a ring of its own at its own
/// size.  What the old ring held goes with it — the history is of the window
/// being recorded, and it is not that window any more.
///
/// Between the two is the standby state: no capture, no encoder, and a question
/// asked four times a second, while the control socket stays served.  That is
/// where a followed replay *starts* — `--follow` is a standing request to
/// record these windows, not a thing that has to be true at the moment of the
/// command — and where it returns when there is no window of its list left to
/// record at all.  The focus being on something else is not that: the session
/// stays on the window it was already on (see [`Follow`]), and only every
/// followed window being gone leaves it with nothing to record.  The ring of
/// the window recorded last is *kept* across a standby rather than dropped:
/// nothing is being recorded, but the seconds that window left behind are still
/// what `replay save` should write, and a user who closed a game to save the
/// moment just gone gets it.  A switch is the other way round — another window
/// is being recorded there, so the ring being left has no share of the next
/// session's saves.
///
/// The route decides only how a session is opened and where the window list and
/// the focus are read; everything else — the standby state, the transitions,
/// the messages — is this one loop.
fn window_replay_session(
    request: &ReplayRequest,
    target: &super::WindowTarget,
    listener: &UnixListener,
    interrupted: &AtomicBool,
    route: WindowRoute,
) -> Result<()> {
    let follow = Follow::new(request.follow.clone());
    // The window to open next, when the supervisor already knows it: a switch
    // hands the next one back from the frame loop.
    let mut pending: Option<Name> = None;
    // The ring of the window recorded last, held while there is no session.  An
    // idle replay records nothing, but it still has that window's last seconds,
    // and `replay save` during the standby is what they are kept for.
    let mut idle_ring: Option<ReplayRecorder> = None;
    // The window this replay is about.  Without `--follow` it is the one the
    // command named, resolved here and kept: the standby state waits for *that*
    // window, so a game that is closed and opened again is picked up without
    // another command.  With `--follow` there is no such window — the focus
    // names one every time — and the target was never resolved at all.
    let mut current: Option<Name> = if follow.is_empty() {
        Some(route.resolve(target)?)
    } else {
        None
    };
    // Whether a session has ever been opened.  The first one is the command:
    // if it fails, the command fails, rather than sitting in a standby state
    // over a window that cannot be recorded at all.  After that a failure is
    // the window's own and the replay waits it out.
    let mut opened_any = false;
    let mut failures = Said::new();
    let mut idle = Said::new();
    loop {
        let window = match pending.take() {
            Some(window) => window,
            None => match standby_wait(
                request,
                target,
                &follow,
                current.as_ref(),
                listener,
                interrupted,
                route,
                idle_ring.as_mut(),
            )? {
                Some(window) => window,
                None => return Ok(()),
            },
        };
        current = Some(window.clone());
        // A session is about to open, and its ring is the one a save should use
        // from here on: the idle one has been superseded, and holding it would
        // keep a second buffer in memory for the length of the new session.  An
        // attach that never opens (the window went away in between) leaves
        // nothing behind, which is the honest answer — the replay has moved on
        // to the window it was asked to record.
        idle_ring = None;
        let attached = match route {
            WindowRoute::Copy => attach_copy_session(request, &window, listener, interrupted),
            WindowRoute::KWin => attach_kwin_session(request, &window, listener, interrupted),
            WindowRoute::Cast => attach_cast_session(request, &window, listener, interrupted),
        };
        match attached {
            Ok(Detached { end, mut ring }) => {
                opened_any = true;
                // A session was opened, so whatever the replay said the last
                // time it was idle or that the last failure was is no longer
                // the latest thing to have happened.
                failures.reset();
                idle.reset();
                match end {
                    WindowEnd::Stopped => return Ok(()),
                    WindowEnd::Switch(next) => {
                        // The new window's own size is the point of restarting:
                        // its frames are not fitted into anything, and the ring
                        // being left goes with the window it recorded.
                        drop(ring.take());
                        eprintln!(
                            "vshot: the focus moved to `{}`; restarting the replay there, at its \
                             own size — what the last window's ring held goes with it",
                            next.label()
                        );
                        pending = Some(next);
                    }
                    WindowEnd::Gone(reason) => {
                        idle_ring = ring.take();
                        idle.idle(&reason, idle_ring.is_some());
                        // A window this side saw go away can still be in the
                        // compositor's list for a moment; the same pause keeps
                        // the two from chasing each other.
                        sleep_interruptible(super::window::FOCUS_POLL, interrupted);
                    }
                }
            }
            Err(error) => {
                if !opened_any {
                    // The command's own window could not be opened at all: that
                    // is the command failing, not a session to wait out.
                    return Err(error);
                }
                // The window's side of the session failed: the game closed,
                // the output it is on went away, the compositor had a moment.
                // The replay waits rather than dies — and says it once per
                // distinct failure, because a line every quarter of a second
                // is not a log.
                failures.say(&format!(
                    "the replay could not record `{}`: {error}",
                    window.label()
                ));
                sleep_interruptible(super::window::FOCUS_POLL, interrupted);
            }
        }
    }
}

/// The standby state: no capture, no encoder, and a question asked four times a
/// second — while the control socket is still served, because a replay that
/// cannot be stopped is worse than one that is idle.
///
/// `ring` is the last window's ring, when there is one: a standby records
/// nothing, but a save still writes what that window left behind.  It is `None`
/// before the first session, where a save has nothing to write and says so.
///
/// `Some` is the window to open.  `None` is "stop": the signal handler or a
/// `vshot replay stop` arrived while there was nothing to record.
#[allow(clippy::too_many_arguments)] // the standby state's own shape, plus the ring
fn standby_wait(
    request: &ReplayRequest,
    target: &super::WindowTarget,
    follow: &Follow,
    current: Option<&Name>,
    listener: &UnixListener,
    interrupted: &AtomicBool,
    route: WindowRoute,
    mut ring: Option<&mut ReplayRecorder>,
) -> Result<Option<Name>> {
    // The window list of the route, opened once: a standby state asks it four
    // times a second, and a Wayland connection per ask would be four a second
    // more than the question needs.
    let mut listing = (route != WindowRoute::KWin)
        .then(WindowCapture::connect_listing)
        .transpose()?;
    // Whether the idle sentence may promise a save: a session that has run
    // keeps its history across the standby, one that never ran has none.
    let history = ring.is_some();
    let mut idle = Said::new();
    loop {
        if interrupted.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if let Some(stream) = accept_control(listener) {
            // `handle_control` answers a save and a status out of what it is
            // handed: the ring of the window recorded last (a save writes it
            // even now), and the fact that nothing is being recorded.
            if handle_control(stream, ControlState::standby(ring.as_deref_mut()), request)? {
                return Ok(None);
            }
        }
        let found = match route {
            WindowRoute::KWin => kwin_standby(target, follow, current),
            WindowRoute::Copy | WindowRoute::Cast => match listing.as_mut() {
                Some(listing) => copy_standby(listing, target, follow, current),
                // `connect_listing` said yes when it was asked above; a
                // listing that is not there is a bug, not a state.
                None => unreachable!("the copy route opens its listing"),
            },
        };
        match found {
            Standby::Open(window) => return Ok(Some(window)),
            // Why nothing is being recorded, said once per reason: a replay
            // left running for hours should not repeat itself four times a
            // second, and the user has to be able to tell waiting from stuck.
            Standby::Idle(why) => idle.idle(&why, history),
        }
        sleep_interruptible(super::window::FOCUS_POLL, interrupted);
    }
}

/// What a standby state found: a window to open, or nothing yet and the
/// sentence that says why.
enum Standby {
    Open(Name),
    Idle(String),
}

/// What one attach left behind: why it ended, and the ring it was encoding
/// into.
///
/// The ring is handed back rather than dropped inside the attach because a
/// standby keeps it — the session is over, but what the window left in the ring
/// is not, and `replay save` during the standby writes exactly that.  `None` is
/// an attach that never got as far as opening anything (the window went away
/// between the decision and the open), which has no ring of its own to hand
/// back.
struct Detached {
    end: WindowEnd,
    ring: Option<ReplayRecorder>,
}

/// The window a standby replay should open, from the compositor's own toplevel
/// list.
///
/// The two halves of the question are the two shapes of a window replay: a
/// followed one waits for the focus, a named one for its window.
fn copy_standby(
    listing: &mut WindowCapture,
    target: &super::WindowTarget,
    follow: &Follow,
    current: Option<&Name>,
) -> Standby {
    let toplevels = match listing.toplevels() {
        Ok(toplevels) => toplevels,
        // The list is read four times a second, and a compositor between two
        // commits can fail one read: say so, and keep asking.
        Err(error) => return Standby::Idle(format!("the window list could not be read ({error})")),
    };
    let names: Vec<&Name> = toplevels.iter().map(|toplevel| &toplevel.name).collect();
    choose(&names, focus_now(), target, follow, current)
}

/// The same over KWin's scripting probes, which is where a Plasma session's
/// window list and focus come from — it speaks neither of the protocols the
/// toplevel list needs.
fn kwin_standby(target: &super::WindowTarget, follow: &Follow, current: Option<&Name>) -> Standby {
    let rows = match crate::capture::window::kwin_rows(&crate::capture::window::ProcessWindowRunner)
    {
        Ok(rows) => rows,
        Err(error) => {
            return Standby::Idle(format!("KWin's window list could not be read ({error})"))
        }
    };
    let names: Vec<Name> = rows.iter().map(super::kwin_window::name_of).collect();
    let borrowed: Vec<&Name> = names.iter().collect();
    let focus =
        match crate::capture::window::kwin_active_row(&crate::capture::window::ProcessWindowRunner)
        {
            Ok(Some(row)) => Focus::On((row.app_id, row.title)),
            // KWin lists no focused window when the focus is on the desktop,
            // and a compositor that cannot be asked answers the same way: no
            // hint, so the window list is what decides.
            Ok(None) => Focus::Unknown,
            Err(error) => {
                if debug_enabled() {
                    eprintln!("vshot: the focused window could not be read ({error}); waiting");
                }
                Focus::Unknown
            }
        };
    choose(&borrowed, focus, target, follow, current)
}

/// The focus as the shared choice wants it: the labels the compositor reports
/// for the window that has it, or nothing.
///
/// Nothing has the focus quite often — the desktop itself does, and a
/// compositor reports that as an empty answer rather than as a window — and a
/// compositor that cannot be asked at all answers the same way here.  The two
/// need not be told apart: the focus is only a hint for which window of the
/// list to open, and what leaves a replay idle is the window list, not this.
enum Focus {
    On((String, String)),
    Unknown,
}

fn focus_now() -> Focus {
    match ProcessWindowProvider.active_window() {
        Ok(focused) => Focus::On((focused.app_id, focused.title)),
        Err(error) => {
            if debug_enabled() {
                eprintln!("vshot: the focused window could not be read ({error}); waiting");
            }
            Focus::Unknown
        }
    }
}

/// The window a standby state should open, from a window list and the focus:
/// the index into `names`, or why there is nothing yet.
///
/// The two shapes of a window replay read this differently.  A followed one
/// records whenever one of its windows is on the screen: the focus picks which
/// one when it is on one of them, and otherwise the first entry the list
/// resolves — the answer a recording starts on ([`Follow::start`]), and the
/// reason a focus somewhere else does not leave this replay with nothing to
/// record.  A named one waits for its own window, found the way the command
/// found it: by the filter when it gave one — a title carrying a frame counter
/// still matches — and by the labels of the window it was on otherwise, because
/// a window that was closed and opened again is a new toplevel with a new
/// identifier.
fn choose(
    names: &[&Name],
    focus: Focus,
    target: &super::WindowTarget,
    follow: &Follow,
    current: Option<&Name>,
) -> Standby {
    if !follow.is_empty() {
        let focused = match &focus {
            Focus::On((app_id, title)) => Some((app_id.as_str(), title.as_str())),
            // Nothing having the focus and a read that failed are the same
            // answer to this question: there is no hint, so the list decides.
            Focus::Unknown => None,
        };
        return match follow.start(names, focused) {
            Ok(index) => Standby::Open(names[index].clone()),
            // The sentence already says what is missing and lists what the
            // compositor has; the idle line adds what the session is doing.
            Err(error) => Standby::Idle(relay_message(&error)),
        };
    }
    if let super::WindowTarget::Filter(filter) = target {
        if let Ok(index) = window_copy::select(names, filter) {
            return Standby::Open(names[index].clone());
        }
    }
    if let Some(current) = current {
        if let Some(index) = names
            .iter()
            .position(|name| current.same_as(name) || same_labels(current, name))
        {
            return Standby::Open(names[index].clone());
        }
        return Standby::Idle(format!(
            "the window `{}` is not on screen; this replay waits for it to come back",
            current.label()
        ));
    }
    Standby::Idle("the window to record could not be resolved".to_owned())
}

/// Whether two names have the same app id and title: the window a standby
/// replay waits for once the compositor's identifier has changed under it,
/// which is what closing and opening a window does.
fn same_labels(a: &Name, b: &Name) -> bool {
    a.app_id == b.app_id && a.title == b.title
}

/// Says a sentence once: the same one twice in a row is not news, and a replay
/// left running for hours should not repeat itself four times a second.  A
/// different sentence is said again, so the latest thing to have happened is
/// always the last line the user saw.
struct Said {
    last: String,
}

impl Said {
    fn new() -> Self {
        Self {
            last: String::new(),
        }
    }

    fn reset(&mut self) {
        self.last.clear();
    }

    fn say(&mut self, what: &str) {
        if what == self.last {
            return;
        }
        eprintln!("vshot: {what}");
        self.last = what.to_owned();
    }

    /// The sentence a standby replay has to say: nothing is being recorded, why
    /// not, whether the last window's history is still there for a save, and
    /// that the session is still running.
    fn idle(&mut self, reason: &str, history: bool) {
        let kept = if history {
            "the history of the window it recorded last is still held (`vshot replay save` writes \
             it), and "
        } else {
            ""
        };
        self.say(&format!(
            "the replay is idle: {reason}; {kept}it starts recording again when there is a window \
             to record (`vshot replay stop` to end it)"
        ));
    }
}

/// One attach of the `ext_image_copy_capture_v1` route: the capture, the audio
/// and the ring for one window, and the frame loop over them.
///
/// Everything it opens is dropped when it returns except the ring, which is
/// handed back: the ring holds one window at one size, so the next window gets
/// one of its own, and a standby keeps this one — the history is what a save
/// afterwards writes.
fn attach_copy_session(
    request: &ReplayRequest,
    window: &Name,
    listener: &UnixListener,
    interrupted: &AtomicBool,
) -> Result<Detached> {
    use super::avcodec::VideoSink;
    let Some((mut capture, shape, name)) = super::window::open_named_window_capture(window)? else {
        // The window went away between the decision and the open.  Not an
        // error: the supervisor waits for it to come back, and there is no ring
        // to hand back because none was opened.
        return Ok(Detached {
            end: WindowEnd::Gone(format!(
                "the window `{}` went away before the session could start",
                window.label()
            )),
            ring: None,
        });
    };
    // `--mic` and `--app-audio` are independent and may both be given: the
    // microphone is the room, the window's own application audio is the
    // window's sound, and the ring keeps both summed into its one track.
    let app_audio = if request.app_audio {
        super::open_app_audio(&name)?
    } else {
        None
    };
    let mic = super::open_soundtrack_with(request.mic.as_ref(), app_audio)?;
    let mic_format = mic.format();
    let gop_frames = request.gop_frames();
    let retention = request
        .window
        .saturating_add(request.gop_secs.clamp(1, MAX_GOP));
    // A window replay always captures dma-bufs; on NVENC the ring keeps them
    // as RGBA instead, because that backend cannot import one.
    let backend = request.encoder_backend.resolve();
    let mut recorder = ReplayRecorder::start(
        shape.width,
        shape.height,
        request.encoder,
        Some(shape.fourcc),
        mic_format,
        retention,
        gop_frames,
        request.fps,
        backend,
        request.rate_control(),
    )?;
    eprintln!(
        "vshot: replaying the window `{}` at {}x{}, keeping the last {}s in memory",
        name.label(),
        shape.width,
        shape.height,
        request.window
    );
    if debug_enabled() {
        eprintln!(
            "vshot: window replay {}x{} at {} fps with {} ({}) through libavcodec {}, keeping \
             {}s in memory (GOP {} frames)",
            shape.width,
            shape.height,
            request.fps,
            request.encoder.word(),
            backend.word(),
            super::avcodec::libavcodec_version(),
            request.window,
            gop_frames
        );
    }
    mic.arm();
    // The recording window loop wants a `RecordRequest`; what it reads that
    // matters here is the frame rate and the `--app-audio` flag, which tells a
    // `--follow` switch to carry the application stream across to the new
    // window while the microphone keeps running.
    let loop_request = super::RecordRequest {
        target: request.target.clone(),
        output: None,
        fps: request.fps,
        cursor: request.cursor,
        duration: None,
        encoder: request.encoder,
        encoder_backend: request.encoder_backend,
        portal: false,
        mic: None,
        // The audio side is already resolved into `mic` above; the loop takes
        // the `Soundtrack` itself.
        app_audio: request.app_audio,
        follow: request.follow.clone(),
        // The rate control belongs to the session, not to the loop: the ring
        // was already opened with it, and this request is only what the loop
        // reads its frame interval from.
        bitrate: request.bitrate,
        quality: request.quality,
    };
    let follow = Follow::new(request.follow.clone());
    let _ = VideoSink::canvas(&recorder);
    let end = super::window::loop_over(
        &mut capture,
        &mut recorder,
        &loop_request,
        &follow,
        // A switch ends this attach instead of moving it: the supervisor opens
        // the next window's session, at its own size.
        FollowPolicy::Report,
        name,
        mic,
        interrupted,
        |sink| {
            // The control socket is polled at every frame boundary, so a save
            // is served with the ring consistent and the loop keeps running.
            if let Some(stream) = accept_control(listener) {
                handle_control(stream, ControlState::recording(sink), request)
            } else {
                Ok(false)
            }
        },
    )?;
    Ok(Detached {
        end,
        ring: Some(recorder),
    })
}

/// One attach of the KWin route: KWin's own copy of one window, by its
/// `QUuid`, encoded into the ring.  The frame loop, the follow decision and the
/// per-frame control socket are exactly the ones a KWin window *recording*
/// uses; only the sink differs — a ring instead of a file — and the loop is
/// told to report a switch rather than to fit the next window into the ring.
/// The ring is handed back with the outcome, as on the other routes: a standby
/// keeps it, a switch does not.
fn attach_kwin_session(
    request: &ReplayRequest,
    window: &Name,
    listener: &UnixListener,
    interrupted: &AtomicBool,
) -> Result<Detached> {
    use super::avcodec::VideoSink;
    // The window is captured once before the ring opens, because the ring's
    // canvas comes from the window's own pixels — which only a capture
    // reports.  KWin's ScreenShot2 has no dma-buf, so the ring keeps RGBA
    // frames on every backend, not only NVENC.
    let loop_request = super::RecordRequest {
        target: request.target.clone(),
        output: None,
        fps: request.fps,
        cursor: request.cursor,
        duration: None,
        encoder: request.encoder,
        encoder_backend: request.encoder_backend,
        portal: false,
        mic: None,
        app_audio: request.app_audio,
        follow: request.follow.clone(),
        // The rate control belongs to the session, not to the loop: the ring
        // was already opened with it, and this request is only what the loop
        // reads its frame interval from.
        bitrate: request.bitrate,
        quality: request.quality,
    };
    let Some((mut capture, mut window, first)) =
        super::kwin_window::open_named_window_capture(request.cursor, window)?
    else {
        // It went away between the decision and the open: the supervisor waits
        // for it to come back rather than failing, and there is no ring to hand
        // back because none was opened.
        return Ok(Detached {
            end: WindowEnd::Gone(format!(
                "the window `{}` went away before the session could start",
                window.label()
            )),
            ring: None,
        });
    };
    let follow = Follow::new(request.follow.clone());
    // The ring's canvas, like a file's, has to be even for NV12 — see
    // `kwin_window::even_canvas`.  ScreenShot2 hands back the window's client
    // geometry, which can be odd; the fit path pads the extra column and row.
    let (width, height) = super::kwin_window::even_canvas(first.size());

    // `--mic` and `--app-audio` are independent and may both be given: the
    // microphone is the room, the window's own application audio is the
    // window's sound, and the ring keeps both summed into its one track.
    let app_audio = if request.app_audio {
        super::open_app_audio(&window.as_name())?
    } else {
        None
    };
    let mic = super::open_soundtrack_with(request.mic.as_ref(), app_audio)?;
    let mic_format = mic.format();
    let gop_frames = request.gop_frames();
    let retention = request
        .window
        .saturating_add(request.gop_secs.clamp(1, MAX_GOP));
    let backend = request.encoder_backend.resolve();
    let mut recorder = ReplayRecorder::start(
        width,
        height,
        request.encoder,
        // No dma-buf: ScreenShot2 writes pixels through a pipe, and the ring
        // keeps them as RGBA.
        None,
        mic_format,
        retention,
        gop_frames,
        request.fps,
        backend,
        request.rate_control(),
    )?;
    eprintln!(
        "vshot: replaying the window `{}` at {width}x{height}, keeping the last {}s in memory",
        window.label(),
        request.window
    );
    if debug_enabled() {
        eprintln!(
            "vshot: KWin window replay {width}x{height} at {} fps with {} ({}) through libavcodec \
             {}, keeping {}s in memory (GOP {} frames)",
            request.fps,
            request.encoder.word(),
            backend.word(),
            super::avcodec::libavcodec_version(),
            request.window,
            gop_frames
        );
    }
    let _ = VideoSink::canvas(&recorder);
    let mut mic = mic;
    mic.arm();
    let end = super::kwin_window::loop_over(
        &mut capture,
        &mut recorder,
        &loop_request,
        &follow,
        // A switch ends this attach: the supervisor opens the next window's
        // session, at its own size.
        FollowPolicy::Report,
        &mut window,
        first,
        &mut mic,
        interrupted,
        |sink| {
            if let Some(stream) = accept_control(listener) {
                handle_control(stream, ControlState::recording(sink), request)
            } else {
                Ok(false)
            }
        },
    )?;
    Ok(Detached {
        end,
        ring: Some(recorder),
    })
}

/// One attach of the screen-cast route: the compositor's own cast of one
/// window, encoded into the ring.  This is the route a compositor that speaks
/// neither `ext_image_copy_capture_v1` nor KWin's `ScreenShot2` — niri, whose
/// capture support stops at outputs and whose window pixels come from the
/// screen-cast service its portal drives.  The cast, the loop and the per-frame
/// control socket are the ones a window *recording* on that service uses; only
/// the sink differs, and the cast ending is a standby rather than the end of
/// the session — so the ring goes back to the supervisor with the outcome.
fn attach_cast_session(
    request: &ReplayRequest,
    window: &Name,
    listener: &UnixListener,
    interrupted: &AtomicBool,
) -> Result<Detached> {
    use super::avcodec::VideoSink;
    let backend = request.encoder_backend.resolve();
    let mut window = super::screencast::WindowCast::open_named(
        window,
        request.cursor,
        request.fps,
        super::screencast::allow_dmabuf(backend),
    )?;
    let geometry = window.geometry;
    let shape = super::cast::Shape::of(&geometry);
    let fourcc = super::cast::fourcc_for(geometry.spa_format)?;

    // `--mic` and `--app-audio` are independent and may both be given: the
    // microphone is the room, the window's own application audio is the
    // window's sound, and the ring keeps both summed into its one track.
    let app_audio = if request.app_audio {
        super::open_app_audio(&window.name)?
    } else {
        None
    };
    let mic = super::open_soundtrack_with(request.mic.as_ref(), app_audio)?;
    let mic_format = mic.format();
    let gop_frames = request.gop_frames();
    let retention = request
        .window
        .saturating_add(request.gop_secs.clamp(1, MAX_GOP));
    let mut recorder = ReplayRecorder::start(
        geometry.width,
        geometry.height,
        request.encoder,
        // The ring keeps dma-bufs only where the cast produced them and the
        // backend can import them; a memory cast is RGBA, exactly as on the
        // recording side.
        match shape {
            super::cast::Shape::Dmabuf => Some(fourcc),
            super::cast::Shape::Software => None,
        },
        mic_format,
        retention,
        gop_frames,
        request.fps,
        backend,
        request.rate_control(),
    )?;
    eprintln!(
        "vshot: replaying the window `{}` at {}x{}, keeping the last {}s in memory",
        window.name.label(),
        geometry.width,
        geometry.height,
        request.window
    );
    if debug_enabled() {
        eprintln!(
            "vshot: window replay {}x{} at {} fps with {} ({}) through libavcodec {}, keeping \
             {}s in memory (GOP {} frames)",
            geometry.width,
            geometry.height,
            request.fps,
            request.encoder.word(),
            backend.word(),
            super::avcodec::libavcodec_version(),
            request.window,
            gop_frames
        );
    }
    let _ = VideoSink::canvas(&recorder);
    let mut mic = mic;
    mic.arm();
    let ended = window.ended_flag();
    // Whether the loop stopped because the cast is over rather than because the
    // session was told to stop: the two both end the loop, and only one of them
    // is a standby.
    let cast_ended = std::sync::atomic::AtomicBool::new(false);
    let outcome = super::cast::loop_over(
        &mut window.stream,
        &mut recorder,
        // A replay has no `--duration`: it runs until it is stopped.
        None,
        shape,
        geometry,
        &mut mic,
        interrupted,
        // A window that is resized keeps being kept: the ring holds one
        // canvas, and the new frames are fitted into it.
        true,
        // The cast of a window sends nothing while the window sits still, and
        // this replay's history is measured on a timeline that only moves when
        // a frame is encoded.  Repeating the held frame at the session's own
        // frame rate is what keeps that timeline in step with the clock — and
        // what keeps `--gop` meaning what it says: a key-frame distance in
        // *frames* only comes to the seconds it was asked for if the frames
        // keep arriving at the rate the session was opened with.
        Some(request.frame_interval()),
        {
            let cast_ended = &cast_ended;
            move |sink| {
                // The compositor saying the cast is over ends this attach: a
                // window that was closed stops the frames without an error, and
                // the supervisor waits for it to come back.
                if ended.load(Ordering::Relaxed) {
                    cast_ended.store(true, Ordering::Relaxed);
                    return Ok(true);
                }
                // The control socket is polled at every frame boundary, so a
                // save is served with the ring consistent and the loop keeps
                // running.
                if let Some(stream) = accept_control(listener) {
                    handle_control(stream, ControlState::recording(sink), request)
                } else {
                    Ok(false)
                }
            }
        },
    );
    window.stop();
    outcome?;
    let end = if cast_ended.load(Ordering::Relaxed) {
        WindowEnd::Gone(
            "the compositor stopped casting the window; it may have been closed".to_owned(),
        )
    } else {
        WindowEnd::Stopped
    };
    Ok(Detached {
        end,
        ring: Some(recorder),
    })
}

/// Binds the control socket, replacing a stale one (a socket left by a session
/// that died without cleaning up would otherwise block the next start).
fn bind_control_socket() -> Result<UnixListener> {
    let path = socket_path();
    if path.exists() {
        // A live session is refused earlier by the pid file; anything left
        // here is stale.
        let _ = std::fs::remove_file(&path);
    }
    UnixListener::bind(&path).map_err(|error| {
        VshotError::Recording(format!(
            "could not bind the replay control socket {}: {error}",
            path.display()
        ))
    })
}

/// The replay loop: grab, encode into the ring, poll the control socket, pace.
#[allow(clippy::too_many_arguments)] // the session's own shape
fn session_loop(
    capture: &mut Capturer,
    source: &Source,
    request: &ReplayRequest,
    zero_copy: bool,
    recorder: &mut ReplayRecorder,
    mic: &mut super::pipewire_audio::Soundtrack,
    listener: &UnixListener,
    interrupted: &AtomicBool,
) -> Result<()> {
    let started = Instant::now();
    let interval = request.frame_interval();
    let mut scratch = super::SceneScratch::new();
    let mut next_frame = started;
    let mut last_frame_at = started;
    let mut consecutive_errors = 0u32;
    let mut timeline_ms = 0u64;
    let mut covered_us = 0u64;
    // The frame the loop is holding, for the slots the capture missed.  A slot
    // that passed with nothing new in it carries the frame before it, so every
    // frame on the ring is one interval long and the holds land on the frames
    // that really were not delivered.  It matters more here than anywhere
    // else: a save is asked for by *seconds*, and a timeline that stretches
    // with the capture's own slowness makes the saved clip shorter than the
    // history that was asked for.  See `super::encode_grabbed` for why
    // re-encoding a held frame is safe on both the software and dma-buf paths.
    let mut held: Option<super::Grabbed> = None;

    // A capture the compositor *refused* is retried rather than counted as a
    // dropped frame — see [`super::Refusals`].
    let mut refusals = super::Refusals::default();
    loop {
        if interrupted.load(Ordering::Relaxed) {
            break;
        }
        // The control socket is polled between frames: a save is handled at a
        // frame boundary, with the ring consistent, and the loop keeps running
        // afterwards (a replay serves many saves).
        if let Some(stream) = accept_control(listener) {
            if handle_control(stream, ControlState::recording(recorder), request)? {
                break;
            }
        }
        // The slots that have passed since the last frame.  The ring plays on
        // the clock's grid, so a slot with nothing new in it carries the frame
        // before it: that is what keeps a late copy from stretching its own
        // frame, and it is what puts the hold on the frame that really was not
        // delivered rather than on whichever one the copy happened to land in.
        let now = Instant::now();
        while next_frame + interval <= now {
            // Nothing has been grabbed yet, so there is nothing to repeat.
            let Some(frame) = held.as_ref() else {
                break;
            };
            let duration_ms = super::cast::cover(
                &mut covered_us,
                &mut timeline_ms,
                &mut last_frame_at,
                next_frame,
            );
            super::encode_grabbed(recorder, frame, duration_ms)?;
            next_frame += interval;
        }
        let now = Instant::now();
        if now < next_frame {
            sleep_interruptible(next_frame - now, interrupted);
            if interrupted.load(Ordering::Relaxed) {
                break;
            }
        }
        let grabbed = source.grab(capture, request.cursor, zero_copy, &mut scratch);
        let frame = match grabbed {
            Ok(frame) => frame,
            Err(error) => {
                // A refusal is the compositor declining to be asked, which on
                // KWin comes and goes; anything else is a hiccup.  Neither is
                // fatal on its own.
                if let VshotError::ScreenshotDenied(explanation) = &error {
                    if !refusals.refused(explanation) {
                        return Err(error);
                    }
                } else {
                    consecutive_errors += 1;
                    if consecutive_errors >= 10 {
                        return Err(VshotError::Recording(format!(
                            "giving up after {consecutive_errors} frames in a row failed: {error}"
                        )));
                    }
                    eprintln!("vshot: dropping a frame: {error}");
                }
                // The soundtrack keeps up even while the picture does not: the
                // audio ring holds four seconds (`pipewire_audio`), and a run
                // of refused frames — a refusal is forgiven for ten seconds —
                // would overflow it and leave the ring's audio behind the
                // picture it belongs to.
                super::pump_soundtrack(mic, recorder)?;
                // A dropped frame is not a hole in the timeline: the interval it
                // covered belongs to the frame before it, which was still on
                // screen.  Advancing `last_frame_at` here would cut that time
                // out of the ring, making a save shorter than the history it
                // was asked for.
                next_frame = Instant::now() + interval;
                continue;
            }
        };
        consecutive_errors = 0;
        refusals.delivered();
        // The frame belongs to the slot it was asked for, and the grid moves on
        // by one interval *from that slot*: the copy's own duration is not part
        // of what the screen showed.  Anchoring the grid on the arrival instead
        // is what let a copy that ran into the next slot stretch the ring's
        // timeline, and with it every save's length.
        let taken_at = next_frame;
        next_frame = taken_at + interval;
        let duration_ms = super::cast::cover(
            &mut covered_us,
            &mut timeline_ms,
            &mut last_frame_at,
            taken_at,
        );
        super::encode_grabbed(recorder, &frame, duration_ms)?;
        super::pump_soundtrack(mic, recorder)?;
        held = Some(frame);
    }
    Ok(())
}

/// Accepts one control connection without blocking, or `None` when none is
/// waiting.
fn accept_control(listener: &UnixListener) -> Option<UnixStream> {
    match listener.accept() {
        Ok((stream, _)) => Some(stream),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
        Err(error) => {
            if debug_enabled() {
                eprintln!("vshot: the replay control socket refused a connection: {error}");
            }
            None
        }
    }
}

/// What a control line is answered out of: the ring a save would write and a
/// status would report, and whether this session is recording a window at all.
///
/// The two are independent — a standby holds the ring of the window it recorded
/// last, so a save from it writes that, while nothing is being recorded — and
/// only the pair answers both halves of a `status`.
struct ControlState<'a> {
    ring: Option<&'a mut ReplayRecorder>,
    /// Whether a capture session is running right now.  A standby is idle with
    /// or without a ring; a live session is not idle even for the instant
    /// before its first frame arrives.
    idle: bool,
}

impl<'a> ControlState<'a> {
    /// A session that is recording: the ring it is filling, and the fact that a
    /// window is behind it.
    fn recording(ring: &'a mut ReplayRecorder) -> Self {
        Self {
            ring: Some(ring),
            idle: false,
        }
    }

    /// A session that is not: the ring of the window it recorded last, if there
    /// has been one.
    fn standby(ring: Option<&'a mut ReplayRecorder>) -> Self {
        Self { ring, idle: true }
    }
}

/// Handles one control line.  Returns `true` when the session should end.
fn handle_control(
    mut stream: UnixStream,
    state: ControlState<'_>,
    request: &ReplayRequest,
) -> Result<bool> {
    let ControlState {
        ring: recorder,
        idle,
    } = state;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let command: ReplayCommand = match serde_json::from_slice(&buffer) {
        Ok(command) => command,
        Err(error) => {
            let reply = ReplayReply::Error {
                message: format!("invalid replay control line: {error}"),
            };
            let _ = write_reply(&mut stream, &reply);
            return Ok(false);
        }
    };
    match command {
        ReplayCommand::Save {
            path,
            seconds,
            save_dir,
        } => {
            let reply = match recorder {
                None => ReplayReply::Error {
                    // No ring at all: the session is alive, and no window has
                    // been recorded yet.  A standby that *has* recorded one
                    // holds its ring, and a save from it is served below.
                    message: "the replay is idle: no window has been recorded, so there is \
                              nothing to save"
                        .to_owned(),
                },
                Some(recorder) => match do_save(
                    recorder,
                    request,
                    path.as_deref(),
                    save_dir.as_deref(),
                    seconds,
                ) {
                    Ok((path, seconds)) => {
                        SAVES_SERVED.fetch_add(1, Ordering::Relaxed);
                        ReplayReply::Saved { path, seconds }
                    }
                    Err(error) => ReplayReply::Error {
                        // The client relays this sentence verbatim, so the
                        // category prefix is stripped here: wrapping a
                        // "recording failed: ..." in another "recording failed:"
                        // reads as a stutter.
                        message: relay_message(&error),
                    },
                },
            };
            let _ = write_reply(&mut stream, &reply);
            Ok(false)
        }
        ReplayCommand::Status => {
            let reply = ReplayReply::Status {
                span_seconds: recorder
                    .as_ref()
                    .map_or(0.0, |recorder| recorder.span_seconds()),
                saves: saves_so_far(),
                // Whether a window is being recorded *now*, which is not the
                // same question as whether there is a ring: a standby answers
                // idle while still holding the last window's history.
                idle,
            };
            let _ = write_reply(&mut stream, &reply);
            Ok(false)
        }
        ReplayCommand::Stop => {
            let _ = write_reply(&mut stream, &ReplayReply::Stopping);
            Ok(true)
        }
    }
}

/// Writes one reply line to the control stream.
fn write_reply(stream: &mut UnixStream, reply: &ReplayReply) -> std::io::Result<()> {
    let mut payload =
        serde_json::to_vec(reply).map_err(|error| std::io::Error::other(error.to_string()))?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    stream.flush()
}

/// The sentence a client relays for a failed control request: the error's own
/// text with any category prefix (`recording failed: `) removed, because the
/// client prints it as-is and would otherwise stutter the prefix.
fn relay_message(error: &VshotError) -> String {
    let text = error.to_string();
    match text.strip_prefix("recording failed: ") {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

/// Saves the ring to a file and reports the path and length.  This is the
/// trigger's whole cost: a remux of packets already encoded, no re-encode.
fn do_save(
    recorder: &mut ReplayRecorder,
    request: &ReplayRequest,
    requested_path: Option<&Path>,
    requested_dir: Option<&Path>,
    seconds: Option<u64>,
) -> Result<(PathBuf, f64)> {
    // The directory the request itself carries (`replay save --save-dir`) wins
    // over the session's: the file is named here, so a directory the caller
    // gave for this one save is the one that applies to it.
    let save_dir = requested_dir.or(request.save_dir.as_deref());
    let path = resolve_save_path(requested_path, save_dir, "replay")?;
    // No `--seconds` means the user's window, not the whole ring: the ring
    // holds one key-frame interval more than the window on purpose, and that
    // slack is not what a bare `replay save` asked for.
    let want = seconds.unwrap_or(request.window);
    let (_packets, saved_seconds) = recorder.save(&path, want)?;
    crate::notify::replay_saved(&path, saved_seconds);
    Ok((path, saved_seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_frame_interval_follows_the_rate() {
        let request = ReplayRequest {
            target: super::super::RecordTarget::All,
            window: 30,
            fps: 30,
            encoder: VideoCodec::H264,
            encoder_backend: crate::record::avcodec::EncoderBackend::Auto,
            cursor: false,
            mic: None,
            app_audio: false,
            follow: Vec::new(),
            portal: false,
            save_dir: None,
            gop_secs: 1,
            bitrate: None,
            quality: None,
        };
        assert_eq!(request.frame_interval(), Duration::from_nanos(33_333_333));
        assert_eq!(request.gop_frames(), 30);
        let mut request2 = request.clone();
        request2.gop_secs = 2;
        assert_eq!(request2.gop_frames(), 60);
    }

    #[test]
    fn the_socket_path_is_separate_from_the_recording_pid_file() {
        // SAFETY: single-threaded test body, and the variable is restored.
        let saved = std::env::var_os("VSHOT_REPLAY_SOCKET");
        std::env::set_var("VSHOT_REPLAY_SOCKET", "/tmp/vshot-replay-probe.sock");
        assert_eq!(socket_path(), PathBuf::from("/tmp/vshot-replay-probe.sock"));
        match saved {
            Some(value) => std::env::set_var("VSHOT_REPLAY_SOCKET", value),
            None => std::env::remove_var("VSHOT_REPLAY_SOCKET"),
        }
    }

    #[test]
    fn a_save_never_claims_more_than_the_ring_holds() {
        // The default a bare `replay save` uses is the user's window; the ring
        // holds a little more (one key-frame interval) on purpose.
        let request = ReplayRequest {
            target: super::super::RecordTarget::All,
            window: 30,
            fps: 30,
            encoder: VideoCodec::H264,
            encoder_backend: crate::record::avcodec::EncoderBackend::Auto,
            cursor: false,
            mic: None,
            app_audio: false,
            follow: Vec::new(),
            portal: false,
            save_dir: None,
            gop_secs: 1,
            bitrate: None,
            quality: None,
        };
        assert_eq!(request.window, 30);
    }

    #[test]
    fn the_control_protocol_round_trips() {
        let command = ReplayCommand::Save {
            path: Some(PathBuf::from("/tmp/a.mp4")),
            seconds: Some(10),
            save_dir: None,
        };
        let text = serde_json::to_string(&command).unwrap();
        assert_eq!(
            serde_json::from_str::<ReplayCommand>(&text).unwrap(),
            command
        );
        // A bare `replay save --save-dir DIR` travels as no path and a
        // directory; the session is what names the file.
        let command = ReplayCommand::Save {
            path: None,
            seconds: None,
            save_dir: Some(PathBuf::from("/tmp/clips")),
        };
        let text = serde_json::to_string(&command).unwrap();
        assert_eq!(
            serde_json::from_str::<ReplayCommand>(&text).unwrap(),
            command
        );
        let reply = ReplayReply::Saved {
            path: PathBuf::from("/tmp/a.mp4"),
            seconds: 10.0,
        };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<ReplayReply>(&text).unwrap(), reply);
    }

    #[test]
    fn a_save_path_gains_an_mp4_suffix() {
        let path = resolve_save_path(Some(Path::new("/tmp/replay-now")), None, "replay").unwrap();
        assert_eq!(path.extension().unwrap(), "mp4");
    }

    #[test]
    fn a_save_path_the_user_typed_is_used_verbatim() {
        // A path the user named is not a template.  Expanding it would turn
        // `a%m.mp4` into `a09.mp4` without saying so, and a stray `%` used to
        // be worse than a rename: `chrono` answers one with a panic, which
        // takes the session — and the history in its ring — down with it.
        for typed in ["/tmp/a%m.mp4", "/tmp/50%off.mp4"] {
            let path = resolve_save_path(Some(Path::new(typed)), None, "replay").unwrap();
            assert_eq!(path, PathBuf::from(typed));
        }
    }

    /// A window of the list, with the identifier the compositor gave it.
    fn window(app_id: &str, title: &str, identifier: &str) -> Name {
        Name {
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            identifier: identifier.to_owned(),
        }
    }

    const NO_FILTER: super::super::WindowTarget = super::super::WindowTarget::Active;

    #[test]
    fn a_followed_replay_records_any_of_its_windows() {
        let game = window("game", "Game A", "7");
        let chat = window("discord", "Discord", "9");
        let names = [&game, &chat];
        let follow = Follow::new(vec!["Game A".into()]);
        // The focus on the followed window: that is the one to open.
        match choose(
            &names,
            Focus::On(("game".into(), "Game A".into())),
            &NO_FILTER,
            &follow,
            None,
        ) {
            Standby::Open(name) => assert_eq!(name, game),
            Standby::Idle(why) => panic!("the followed window was not opened: {why}"),
        }
        // The focus on a window outside the list: the followed one is still on
        // the screen, so it is still what gets recorded — the focus is a hint
        // for which window of the list, not the condition for recording one.
        match choose(
            &names,
            Focus::On(("discord".into(), "Discord".into())),
            &NO_FILTER,
            &follow,
            Some(&game),
        ) {
            Standby::Open(name) => assert_eq!(name, game),
            Standby::Idle(why) => panic!("the followed window was left unopened: {why}"),
        }
        // A compositor that cannot say what has the focus changes nothing.
        match choose(&names, Focus::Unknown, &NO_FILTER, &follow, Some(&game)) {
            Standby::Open(name) => assert_eq!(name, game),
            Standby::Idle(why) => panic!("the followed window was left unopened: {why}"),
        }
        // No window of the list on the screen at all: the one idle state, and
        // the sentence has to name the windows that were wanted.
        let other = window("kitty", "~/dev", "3");
        let names = [&other];
        match choose(&names, Focus::Unknown, &NO_FILTER, &follow, Some(&game)) {
            Standby::Idle(why) => assert!(why.contains("Game A"), "{why}"),
            Standby::Open(name) => panic!("a window outside the list was opened: {name:?}"),
        }
    }

    #[test]
    fn a_named_replay_waits_for_its_window_to_come_back() {
        let named = super::super::WindowTarget::Filter("Game Two".into());
        let before = window("game", "Game Two — Main Menu", "7");
        let after = window("game", "Game Two — Main Menu", "12");
        // The same window under a new identifier, which is what closing and
        // opening it does: the labels are what it is found by.
        let names = [&after];
        match choose(
            &names,
            Focus::Unknown,
            &named,
            &Follow::default(),
            Some(&before),
        ) {
            Standby::Open(name) => assert_eq!(name, after),
            Standby::Idle(why) => panic!("the window that came back was not opened: {why}"),
        }
        // Gone: idle, and the sentence names the window being waited for.
        let other = window("kitty", "~/dev", "3");
        let names = [&other];
        match choose(
            &names,
            Focus::Unknown,
            &named,
            &Follow::default(),
            Some(&before),
        ) {
            Standby::Idle(why) => assert!(why.contains("Game Two"), "{why}"),
            Standby::Open(name) => panic!("a window that is not the one was opened: {name:?}"),
        }
        // Back, exactly as it was, under the same identifier.
        let names = [&before];
        match choose(
            &names,
            Focus::Unknown,
            &named,
            &Follow::default(),
            Some(&before),
        ) {
            Standby::Open(name) => assert_eq!(name, before),
            Standby::Idle(why) => panic!("the window itself was not opened: {why}"),
        }
    }

    #[test]
    fn the_idle_sentence_says_whether_the_history_is_still_there() {
        // A standby keeps the ring of the window recorded last, and the
        // sentence has to say so: it is what tells the user that a save still
        // writes something.  A replay that has recorded nothing has no such
        // promise to make, and says the same sentence without it.
        let mut said = Said::new();
        said.idle("the focus is on the desktop", true);
        assert!(said.last.contains("still held"), "{}", said.last);
        assert!(
            said.last.contains("the focus is on the desktop"),
            "{}",
            said.last
        );
        said.idle("the focus is on the desktop", false);
        assert!(!said.last.contains("still held"), "{}", said.last);
        assert!(
            said.last.contains("the focus is on the desktop"),
            "{}",
            said.last
        );
    }

    #[test]
    fn the_names_vshot_builds_are_expanded() {
        let expanded = expand_strftime("/tmp/clips-%Y/replay-%Y%m%d-%H%M%S.mp4");
        assert!(!expanded.contains('%'), "{expanded}");
        assert!(expanded.starts_with("/tmp/clips-2"), "{expanded}");
        assert!(expanded.ends_with(".mp4"), "{expanded}");
    }

    #[test]
    fn a_stray_percent_in_a_template_is_kept_rather_than_panicking() {
        // `--save-dir /tmp/50%off` is a directory someone can have, and it has
        // to stay one rather than panic the session.
        assert_eq!(expand_strftime("/tmp/50%off"), "/tmp/50%off");
        assert_eq!(expand_strftime("/tmp/a%"), "/tmp/a%");
        assert_eq!(expand_strftime("%Y%%%"), "%Y%%%");
    }
}
