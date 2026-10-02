// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! The resident pin daemon.
//!
//! One process holds the pins and draws them: the socket the CLI talks to and
//! the surfaces the pictures go on are the same process's, so a pin is a value
//! this side owns rather than a file two processes pass back and forth.  That
//! is the whole difference from the shape it replaces -- a Qt daemon that drew
//! on SDR outputs, a Rust helper that drew on HDR ones, and a `.pq` file
//! between them carrying a pin's light in a spelling only this program knew.
//!
//! What is drawn is [`Picture`], one value per pin, decoded from whatever file
//! the pin names.  Which codes go into which surface is
//! [`Picture::words_for`], asked once per output the pin is on, because the
//! answer is a property of the picture *and* the display: light above SDR white
//! is shown as it was captured on a panel that can show it, mapped down on one
//! that cannot, and an SDR picture is written against whichever output's white
//! it lands on.
//!
//! **Not here yet.**  The pointer and the keyboard are not read, so a pin cannot
//! be dragged, clicked or closed from the screen; the chrome that needs text --
//! the corner badges, the right-click menu and the `HDR` tag -- belongs to a Qt
//! process this one does not yet call; and the editor and the save dialog are
//! not wired.  A pin made through this daemon can be listed, toggled and closed
//! by the CLI, and that is all.

use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::error::{Result, VshotError};
use crate::geometry::{Point, Rect, Size};
use crate::model::picture::{self, Picture};
use crate::pin::PinCommand;
use crate::pin_hdr::{Pin, Surfaces};

/// How long a client may take to send its request before the connection is
/// dropped.  It is one short line, and a client that stops halfway must not
/// hold the loop.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// One pinned image, as this side knows it.
struct Pinned {
    id: u64,
    /// The picture itself, which is what every output's codes are derived from.
    picture: Picture,
    /// The file it came from.  Kept because it is what a pin is *of*: the save
    /// dialog opens on it, the editor is handed it, and it is the identity a
    /// repeated request is matched against.
    path: PathBuf,
    /// Global logical top-left.
    origin: Point,
    /// Logical pixels per source pixel.
    scale: f64,
    visible: bool,
}

impl Pinned {
    /// This pin as the renderer wants it.
    fn render_pin(&self) -> Pin {
        let mut pin = Pin::new(
            self.picture.clone(),
            self.path.clone(),
            self.origin,
            self.scale,
        );
        pin.visible = self.visible;
        pin
    }
}

/// How long the daemon stays up with nothing pinned.
///
/// It is resident so that pins outlive the command that made them, and with
/// nothing pinned there is nothing to hold: the next `vshot pin` starts one
/// again.  Half a second, the same as the daemon this replaces, which is long
/// enough for a close and the next request to be one session's work.
const IDLE_QUIT: Duration = Duration::from_millis(500);

/// How long two presses on one pin have to be apart to be one gesture rather
/// than two.  Qt's own double-click interval, so the two daemons feel alike.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// How much one wheel notch scales a pin by.  Multiplicative rather than
/// additive, so a notch feels the same at any size.
const ZOOM_PER_NOTCH: f64 = 1.1;

/// The range a pin may be zoomed to, on the same reasoning as the Qt daemon's:
/// below a tenth a pin is a speck, and above eight times a screenshot is a
/// handful of pixels filling a screen.
const MIN_SCALE: f64 = 0.1;
const MAX_SCALE: f64 = 8.0;

/// What the pointer is doing with the stack.
///
/// Which pin a point lands on, what a press starts, and when a second press is
/// a double-click: all of it is decisions, so all of it is here rather than in
/// the Wayland plumbing that feeds it.
#[derive(Debug, Default)]
struct Pointer {
    /// The pin being dragged, where the pointer was when it started and where
    /// the pin was then.
    drag: Option<(u64, Point, Point)>,
    /// The pin the pointer last pressed, and when, for telling a double-click
    /// from two clicks on the same pin.
    pressed: Option<(u64, Instant)>,
}

/// What a gesture changed, so the daemon knows what to do about it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Change {
    /// The stack is not what is on screen any more.
    redraw: bool,
    /// A pin went away.
    closed: bool,
}

/// The pins, back to front, with the pointer's business with them.
///
/// Separated from the daemon because everything about what a gesture *means*
/// lives here and nothing about Wayland does, which is what lets it be tested
/// without a compositor.  The order is the paint order: the last entry is
/// drawn last, so it is the frontmost, and it is the one a point lands on when
/// two pins overlap.
struct Stack {
    pins: Vec<Pinned>,
    next_id: u64,
    all_visible: bool,
    pointer: Pointer,
}

impl Stack {
    fn new() -> Self {
        Self {
            pins: Vec::new(),
            next_id: 1,
            all_visible: true,
            pointer: Pointer::default(),
        }
    }

    /// A pin's rectangle in global logical pixels.
    fn rect_of(pin: &Pinned) -> Rect {
        let size = pin.picture.size();
        Rect::new(
            pin.origin.x,
            pin.origin.y,
            (f64::from(size.width) * pin.scale).round().max(1.0) as u32,
            (f64::from(size.height) * pin.scale).round().max(1.0) as u32,
        )
    }

    /// The index of the frontmost pin containing `point`.
    fn index_at(&self, point: Point) -> Option<usize> {
        self.pins
            .iter()
            .rposition(|pin| Self::rect_of(pin).contains(point))
    }

    /// A press at `point`.  Brings the pin under it to the front and starts
    /// dragging it; a second press on the same pin within the double-click
    /// interval closes it instead.
    fn press(&mut self, point: Point, now: Instant) -> Change {
        let Some(index) = self.index_at(point) else {
            // A press on the desktop is not this stack's business.
            self.pointer.pressed = None;
            return Change::default();
        };
        let id = self.pins[index].id;
        // Raised before any test on it: a click that closes a pin still brings
        // it to the front on the way, which is what the user sees happen.
        let index = self.raise(index);
        let double = self
            .pointer
            .pressed
            .is_some_and(|(last, at)| last == id && now.duration_since(at) <= DOUBLE_CLICK);
        if double {
            self.pointer.pressed = None;
            self.pointer.drag = None;
            self.pins.remove(index);
            return Change {
                redraw: true,
                closed: true,
            };
        }
        self.pointer.pressed = Some((id, now));
        self.pointer.drag = Some((id, point, self.pins[index].origin));
        Change {
            redraw: true,
            closed: false,
        }
    }

    /// The pointer moved to `point` with a button down.
    fn motion(&mut self, point: Point) -> Change {
        let Some((id, from, origin)) = self.pointer.drag else {
            return Change::default();
        };
        let Some(pin) = self.pins.iter_mut().find(|pin| pin.id == id) else {
            self.pointer.drag = None;
            return Change::default();
        };
        let moved = Point::new(origin.x + (point.x - from.x), origin.y + (point.y - from.y));
        if pin.origin == moved {
            return Change::default();
        }
        pin.origin = moved;
        Change {
            redraw: true,
            closed: false,
        }
    }

    /// The button came up.  A drag ends here; a press that never moved leaves
    /// the pin where it was and keeps its place in the stack.
    fn release(&mut self) -> Change {
        self.pointer.drag = None;
        Change::default()
    }

    /// The wheel turned `notches` at `at`, positive away from the user.  The
    /// pin under it is scaled about its own centre, so it grows where the user
    /// is looking rather than towards a corner.
    fn scroll(&mut self, notches: i32, at: Point) -> Change {
        if notches == 0 {
            return Change::default();
        }
        let Some(index) = self.index_at(at) else {
            return Change::default();
        };
        let factor = ZOOM_PER_NOTCH.powi(notches);
        let pin = &mut self.pins[index];
        let next = (pin.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        if next == pin.scale {
            return Change::default();
        }
        // The centre is what stays put: the pin is resized about it, so the
        // point under the cursor is the point the user aimed at.
        let before = Self::rect_of(pin);
        let centre = Point::new(
            before.origin.x + before.size.width as i32 / 2,
            before.origin.y + before.size.height as i32 / 2,
        );
        pin.scale = next;
        let after = Self::rect_of(pin);
        pin.origin = Point::new(
            centre.x - after.size.width as i32 / 2,
            centre.y - after.size.height as i32 / 2,
        );
        Change {
            redraw: true,
            closed: false,
        }
    }

    /// Moves the pin at `index` to the front, answering where it ended up.
    fn raise(&mut self, index: usize) -> usize {
        if index + 1 == self.pins.len() {
            return index;
        }
        let pin = self.pins.remove(index);
        self.pins.push(pin);
        self.pins.len() - 1
    }

    fn set_visible(&mut self, visible: bool) {
        self.all_visible = visible;
        for pin in &mut self.pins {
            pin.visible = visible;
        }
    }
}

/// The daemon: the pins, and what is showing them.
struct Daemon {
    stack: Stack,
    surfaces: Surfaces,
    /// When this daemon should give up, once it has nothing to hold.
    idle: Option<Instant>,
}

impl Daemon {
    fn new() -> Result<Self> {
        let mut surfaces = Surfaces::new()?;
        surfaces.start()?;
        Ok(Self {
            stack: Stack::new(),
            surfaces,
            idle: None,
        })
    }

    /// Hands the whole stack to the renderer and asks it to show it.
    ///
    /// Back to front, in the order the pins were added: the last one is the
    /// frontmost, which is what a pin just made should be.
    fn refresh(&mut self) -> Result<()> {
        let stack = self
            .stack
            .pins
            .iter()
            .map(|pin| (pin.id, pin.render_pin()))
            .collect::<Vec<(u64, Pin)>>();
        let debug = std::env::var_os("VSHOT_PIN_DEBUG").is_some();
        if debug {
            let names = self
                .stack
                .pins
                .iter()
                .map(|pin| {
                    format!(
                        "{}@{}x{}",
                        pin.id,
                        pin.picture.size().width,
                        pin.picture.size().height
                    )
                })
                .collect::<Vec<String>>()
                .join(" ");
            eprintln!("vshot-pin: {} pin(s): {names}", self.stack.pins.len());
        }
        // No style: the one this side read from the config when it came up is
        // the one to draw with, and re-reading the file on every move would
        // make the look change under a drag.
        self.surfaces.set_pins(stack, None, debug)?;
        // And the pointer reaches exactly the pins, so a click on the desktop
        // goes to what the pin is covering rather than being swallowed here.
        let rects = self
            .stack
            .pins
            .iter()
            .filter(|pin| pin.visible)
            .map(|pin| Stack::rect_of(pin))
            .collect::<Vec<Rect>>();
        self.surfaces.set_input_rects(&rects)
    }

    /// When this daemon should give up, or `None` while it has something to
    /// hold.
    ///
    /// Arming on the first look at an empty stack and clearing on the first pin
    /// is what makes it a deadline: a pin made inside it, or a second request
    /// that arrives inside it, keeps the session going.
    fn idle_at(&mut self) -> Option<Instant> {
        if !self.stack.pins.is_empty() {
            self.idle = None;
            return None;
        }
        Some(*self.idle.get_or_insert_with(|| Instant::now() + IDLE_QUIT))
    }

    /// Applies whatever the pointer did, drawing once at the end.
    ///
    /// The compositor's own timestamps are not carried through: `Press` keeps
    /// only the order, and "when" is taken here, which is microseconds after
    /// the event was read.  Nothing else in the stack is timed.
    fn gestures(&mut self) -> Result<()> {
        let events = self.surfaces.take_events();
        if events.is_empty() {
            return Ok(());
        }
        let count = events.len();
        let now = Instant::now();
        let mut changed = Change::default();
        for event in events {
            let change = match event {
                crate::wayland::PinEvent::Press { at, .. } => self.stack.press(at, now),
                crate::wayland::PinEvent::Motion { at } => self.stack.motion(at),
                crate::wayland::PinEvent::Release => self.stack.release(),
                crate::wayland::PinEvent::Scroll { notches, at } => self.stack.scroll(notches, at),
            };
            changed.redraw |= change.redraw;
            changed.closed |= change.closed;
        }
        if std::env::var_os("VSHOT_PIN_DEBUG").is_some() {
            eprintln!(
                "vshot-pin: {count} gesture(s), {}",
                if changed.redraw {
                    "redrawn"
                } else {
                    "nothing moved"
                }
            );
        }
        if changed.redraw {
            self.refresh()?;
        }
        Ok(())
    }

    /// Answers one request, or says why not.
    fn handle(&mut self, command: PinCommand) -> Result<serde_json::Value> {
        match command {
            PinCommand::Add {
                path,
                density,
                output_name,
                at,
                // Nothing here can use a second file yet: the sibling half is a
                // spelling of the same pin's light, and this side decodes the
                // picture itself.
                hdr: _,
                output: _,
                annotations: _,
                base: _,
                ack: _,
            } => self.add(&path, density, output_name.as_deref(), at),
            PinCommand::Move { id, x, y, .. } => {
                let Some(pin) = self.stack.pins.iter_mut().find(|pin| pin.id == id) else {
                    return Err(VshotError::Pin(format!(
                        "move names a pin that is not pinned"
                    )));
                };
                pin.origin = Point::new(x, y);
                self.refresh()?;
                Ok(json!({"ok": true}))
            }
            PinCommand::Toggle => {
                let visible = !self.stack.all_visible;
                self.stack.set_visible(visible);
                self.refresh()?;
                Ok(json!({"ok": true}))
            }
            PinCommand::Show => {
                self.stack.set_visible(true);
                self.refresh()?;
                Ok(json!({"ok": true}))
            }
            PinCommand::Hide => {
                self.stack.set_visible(false);
                self.refresh()?;
                Ok(json!({"ok": true}))
            }
            PinCommand::Close => {
                self.stack.pins.clear();
                self.refresh()?;
                Ok(json!({"ok": true}))
            }
            PinCommand::List => Ok(json!({
                "ok": true,
                "count": self.stack.pins.len(),
                "visible": self.stack.all_visible,
            })),
            // Answered by the caller, which is the loop that has to stop.
            PinCommand::Quit => Ok(json!({"ok": true})),
            PinCommand::AddClipboard { .. } => Err(VshotError::Pin(
                "this daemon cannot pin the clipboard yet; name a file instead".into(),
            )),
        }
    }

    fn add(
        &mut self,
        path: &Path,
        density: Option<u32>,
        output_name: Option<&str>,
        at: Option<crate::pin::WirePoint>,
    ) -> Result<serde_json::Value> {
        // Read once: the picture is decoded from the bytes and the scale the
        // file declares is read from the same ones.
        let bytes = std::fs::read(path).map_err(|source| {
            VshotError::Pin(format!("cannot read `{}`: {source}", path.display()))
        })?;
        let reference_nits = crate::config::hdr_reference_white_default()
            .unwrap_or(crate::model::hdr::REFERENCE_WHITE_NITS);
        let picture = picture::decode_bytes(&bytes, reference_nits)?;
        // The scale the pin is shown at: what the caller said, then what the
        // file says about itself, then the natural size.  A capture on a 2x
        // output declares 192 DPI, and a pin that ignored that would come out
        // twice the size it had on screen.
        let density = density
            .or_else(|| picture::declared_scale(&bytes))
            .unwrap_or(1)
            .clamp(1, 4);
        let scale = 1.0 / f64::from(density);
        let size = picture.size();
        let origin = match at {
            Some(point) => Point::new(point.x, point.y),
            None => self.centred(size, scale, output_name),
        };
        let id = self.stack.next_id;
        self.stack.next_id += 1;
        let visible = self.stack.all_visible;
        self.stack.pins.push(Pinned {
            id,
            picture,
            path: path.to_path_buf(),
            origin,
            scale,
            visible,
        });
        self.refresh()?;
        Ok(json!({"ok": true, "id": id}))
    }

    /// Where a pin of `size` at `scale` lands when the caller named no place:
    /// the middle of the output it is going onto.
    fn centred(&self, size: Size, scale: f64, output_name: Option<&str>) -> Point {
        let Some(rect) = self.surfaces.target_geometry(output_name) else {
            return Point::new(0, 0);
        };
        let width = (f64::from(size.width) * scale).round() as i32;
        let height = (f64::from(size.height) * scale).round() as i32;
        Point::new(
            rect.origin.x + (rect.size.width as i32 - width) / 2,
            rect.origin.y + (rect.size.height as i32 - height) / 2,
        )
    }
}

/// Runs the daemon until it is told to stop.
pub fn run(socket: &Path) -> Result<()> {
    // A socket left behind by a daemon that was killed makes every later
    // connect() fail; the path is this daemon's own, so clearing it is safe.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|source| {
        VshotError::Pin(format!("cannot listen on {}: {source}", socket.display()))
    })?;
    // Non-blocking, so the drain loop below can accept everything that is
    // waiting and then get back to the wait: a blocking `accept` would hold the
    // loop here and the daemon would never look at its own clock again.
    listener.set_nonblocking(true).map_err(|source| {
        VshotError::Pin(format!("cannot make the pin socket non-blocking: {source}"))
    })?;
    let debug = std::env::var_os("VSHOT_PIN_DEBUG").is_some();
    let mut daemon = Daemon::new()?;
    daemon.surfaces.set_pin_input(true);
    if debug {
        eprintln!(
            "vshot-pin: listening on `{}` with {} picture surface(s)",
            socket.display(),
            daemon.surfaces.surface_count()
        );
    }
    loop {
        // Waiting on the listener's own descriptor as well as the compositor is
        // what keeps a connection from sitting unread until a buffer happens to
        // come back: with nothing pinned there are no buffers, and the first
        // request would wait for ever.
        let wake = listener
            .as_fd()
            .try_clone_to_owned()
            .map_err(|source| VshotError::Pin(format!("cannot watch the pin socket: {source}")))?;
        match daemon.idle_at() {
            // Nothing pinned: wait out the deadline and give up when it passes.
            // Nothing the compositor or the socket says announces an empty
            // stack, so the clock is the only thing that can.
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(left) => daemon
                    .surfaces
                    .wait_on_until(Some(wake.as_fd()), Some(left))?,
                None => {
                    if debug {
                        eprintln!("vshot-pin: nothing pinned; giving up");
                    }
                    return Ok(());
                }
            },
            None => daemon.surfaces.wait_on(Some(wake.as_fd()))?,
        }
        // Whatever the compositor had queued, before anything else looks at the
        // stack: a drag is a stream of these, and each one is a repaint.
        daemon.gestures()?;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if serve(&mut daemon, stream, debug)? {
                        return Ok(());
                    }
                    // The stack may have changed; showing it is the loop's.
                    daemon.refresh()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    return Err(VshotError::Pin(format!(
                        "cannot accept on {}: {error}",
                        socket.display()
                    )))
                }
            }
        }
    }
}

/// Reads one request from `stream`, answers it, and says whether the daemon
/// should stop.
fn serve(daemon: &mut Daemon, mut stream: UnixStream, debug: bool) -> Result<bool> {
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .map_err(|source| VshotError::Pin(format!("cannot time the request: {source}")))?;
    let mut line = String::new();
    let mut reader = std::io::BufReader::new(&mut stream);
    reader
        .read_line(&mut line)
        .map_err(|source| VshotError::Pin(format!("cannot read the request: {source}")))?;
    let command: PinCommand = serde_json::from_str(line.trim())
        .map_err(|error| VshotError::Pin(format!("cannot read the request: {error}")))?;
    // Whether to stop is decided from the command, not from the reply: the
    // reply has to go out first.
    let quit = matches!(command, PinCommand::Quit);
    let reply = match daemon.handle(command) {
        Ok(reply) => reply,
        Err(error) => {
            if debug {
                eprintln!("vshot-pin: {error}");
            }
            json!({"ok": false, "error": error.to_string()})
        }
    };
    let mut encoded = serde_json::to_vec(&reply)
        .map_err(|error| VshotError::Pin(format!("cannot encode the reply: {error}")))?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .map_err(|source| VshotError::Pin(format!("cannot answer: {source}")))?;
    let _ = stream.flush();
    Ok(quit)
}

/// Reads a whole request, for the tests: one line, no daemon.
#[cfg(test)]
fn parse_request(line: &str) -> Result<PinCommand> {
    serde_json::from_str(line.trim())
        .map_err(|error| VshotError::Pin(format!("cannot read the request: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon reads exactly what the CLI writes, which is the one thing
    /// that cannot be checked from either side alone.
    #[test]
    fn a_request_the_cli_can_write_is_one_this_side_can_read() {
        let written = format!(
            "{}\n",
            serde_json::to_string(&PinCommand::Add {
                path: PathBuf::from("/tmp/shot.png"),
                hdr: None,
                density: Some(2),
                output_name: Some("DP-1".into()),
                output: None,
                at: None,
                annotations: None,
                base: None,
                ack: false,
            })
            .expect("encode")
        );
        let read = parse_request(&written).expect("the daemon reads what the CLI writes");
        let PinCommand::Add { path, density, .. } = read else {
            panic!("that was an add");
        };
        assert_eq!(path, PathBuf::from("/tmp/shot.png"));
        assert_eq!(density, Some(2));
    }

    // --- what a gesture means ---------------------------------------------

    /// A stack of `count` pins, each 100x50 at 1:1, laid out in a row so that
    /// they do not overlap unless a test makes them.
    fn stack_of(count: usize) -> Stack {
        let mut stack = Stack::new();
        for index in 0..count {
            let id = stack.next_id;
            stack.next_id += 1;
            stack.pins.push(Pinned {
                id,
                picture: Picture::Sdr(
                    crate::model::Frame::solid(Size::new(100, 50), [10, 20, 30, 255])
                        .expect("frame"),
                ),
                path: PathBuf::from("(none)"),
                origin: Point::new(index as i32 * 200, 0),
                scale: 1.0,
                visible: true,
            });
        }
        stack
    }

    fn ids(stack: &Stack) -> Vec<u64> {
        stack.pins.iter().map(|pin| pin.id).collect()
    }

    /// A press brings the pin under it to the front, because that is the pin
    /// the user is now working with.
    #[test]
    fn a_press_raises_the_pin_under_it() {
        let mut stack = stack_of(3);
        let start = Instant::now();
        let change = stack.press(Point::new(10, 10), start);
        assert!(change.redraw);
        assert_eq!(ids(&stack), vec![2, 3, 1]);
    }

    /// A press on the desktop is not the stack's business, and must not
    /// disturb the order.
    #[test]
    fn a_press_on_nothing_changes_nothing() {
        let mut stack = stack_of(2);
        let change = stack.press(Point::new(5000, 5000), Instant::now());
        assert_eq!(change, Change::default());
        assert_eq!(ids(&stack), vec![1, 2]);
    }

    /// Dragging moves the pin by the pointer's own delta, so the part of the
    /// picture the user grabbed stays under the cursor.
    #[test]
    fn a_drag_moves_the_pin_by_the_pointer_delta() {
        let mut stack = stack_of(1);
        let start = Instant::now();
        stack.press(Point::new(10, 10), start);
        let change = stack.motion(Point::new(40, 25));
        assert!(change.redraw);
        assert_eq!(stack.pins[0].origin, Point::new(30, 15));
        // And a motion that lands where it already was is not a repaint.
        assert_eq!(stack.motion(Point::new(40, 25)), Change::default());
        stack.release();
        // A motion with the button up does nothing at all.
        assert_eq!(stack.motion(Point::new(90, 90)), Change::default());
        assert_eq!(stack.pins[0].origin, Point::new(30, 15));
    }

    /// Two presses on one pin inside the double-click interval are one gesture
    /// and close it; two presses further apart are two clicks and close
    /// nothing.
    #[test]
    fn a_second_press_in_time_closes_the_pin() {
        let mut stack = stack_of(2);
        let start = Instant::now();
        stack.press(Point::new(10, 10), start);
        stack.release();
        let change = stack.press(Point::new(10, 10), start + Duration::from_millis(100));
        assert_eq!(
            change,
            Change {
                redraw: true,
                closed: true
            }
        );
        assert_eq!(ids(&stack), vec![2]);

        // Slow enough to be two clicks, and the pin stays.
        let mut stack = stack_of(1);
        stack.press(Point::new(10, 10), start);
        stack.release();
        let change = stack.press(Point::new(10, 10), start + DOUBLE_CLICK * 2);
        assert!(!change.closed);
        assert_eq!(ids(&stack), vec![1]);
    }

    /// A press on one pin followed by a press on another is not a double-click,
    /// however fast it was.
    #[test]
    fn a_double_click_is_two_presses_on_the_same_pin() {
        let mut stack = stack_of(2);
        let start = Instant::now();
        stack.press(Point::new(10, 10), start);
        stack.release();
        let change = stack.press(Point::new(210, 10), start + Duration::from_millis(50));
        assert!(!change.closed);
        assert_eq!(ids(&stack).len(), 2);
    }

    /// A point where two pins overlap belongs to the front one, which is the
    /// last drawn.
    #[test]
    fn the_frontmost_pin_is_the_one_a_point_lands_on() {
        let mut stack = stack_of(2);
        stack.pins[1].origin = Point::new(50, 0);
        // (60, 10) is inside both; the front one is pin 2.
        assert_eq!(stack.index_at(Point::new(60, 10)), Some(1));
        let end = stack.press(Point::new(60, 10), Instant::now());
        assert!(end.redraw);
        // It was already in front, so the order is unchanged and it is the one
        // that would drag.
        assert_eq!(ids(&stack), vec![1, 2]);
    }

    /// The wheel scales the pin under it by a tenth a notch, about the pin's
    /// own centre, and stops at the ends of the range.
    #[test]
    fn the_wheel_scales_about_the_pins_centre() {
        let mut stack = stack_of(1);
        stack.pins[0].origin = Point::new(0, 0);
        let change = stack.scroll(1, Point::new(50, 25));
        assert!(change.redraw);
        assert!((stack.pins[0].scale - ZOOM_PER_NOTCH).abs() < 1e-9);
        // 100x50 about its centre (50, 25) at 1.1 is 110x55, so the top-left
        // moves back by five and two and a half.
        assert_eq!(stack.pins[0].origin, Point::new(-5, -2));

        // One notch back and it is where it started.
        stack.scroll(-1, Point::new(50, 25));
        assert!((stack.pins[0].scale - 1.0).abs() < 1e-9);

        // The ends of the range hold.
        for _ in 0..200 {
            stack.scroll(1, Point::new(50, 25));
        }
        assert_eq!(stack.pins[0].scale, MAX_SCALE);
        for _ in 0..300 {
            stack.scroll(-1, Point::new(50, 25));
        }
        assert_eq!(stack.pins[0].scale, MIN_SCALE);
    }

    /// The wheel over nothing, and a wheel that lands on a pin already at the
    /// end of its range, are both nothing to repaint.
    #[test]
    fn a_wheel_that_changes_nothing_is_not_a_repaint() {
        let mut stack = stack_of(1);
        assert_eq!(stack.scroll(1, Point::new(5000, 5000)), Change::default());
        assert_eq!(stack.scroll(0, Point::new(10, 10)), Change::default());
        stack.pins[0].scale = MAX_SCALE;
        assert_eq!(stack.scroll(1, Point::new(10, 10)), Change::default());
    }

    /// Hiding and showing is the whole stack at once, which is what the CLI's
    /// three commands mean.
    #[test]
    fn the_stack_is_hidden_and_shown_together() {
        let mut stack = stack_of(3);
        stack.set_visible(false);
        assert!(!stack.all_visible);
        assert!(stack.pins.iter().all(|pin| !pin.visible));
        stack.set_visible(true);
        assert!(stack.pins.iter().all(|pin| pin.visible));
    }

    /// A request this daemon cannot serve is refused with a reason rather than
    /// read as something else.
    #[test]
    fn an_unknown_command_is_refused() {
        assert!(parse_request("{\"cmd\":\"dance\"}\n").is_err());
        assert!(parse_request("not json at all\n").is_err());
    }
}
