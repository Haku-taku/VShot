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
use crate::pin_hdr::{OutputPlacement, Pin, Surfaces};

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

/// The density a pin is shown at, and what decided it.
///
/// In the order the README gives, which is the order of how much each source
/// knows: what the caller asked for, what the file says about itself, what a
/// screenshot tool left beside it, and finally what the picture's own size
/// implies about the screen it came from.  A pin that ignored the last of those
/// would come out larger than it ever was on screen -- which is the whole
/// complaint the sizing exists to answer.
fn resolve_density(
    requested: Option<u32>,
    declared: Option<u32>,
    recorded: Option<u32>,
    size: Size,
    target: OutputPlacement,
    others: &[OutputPlacement],
) -> (u32, &'static str) {
    if let Some(value) = requested {
        return (value.clamp(1, 4), "the request");
    }
    if let Some(value) = declared {
        return (value.clamp(1, 4), "the file's own statement");
    }
    if let Some(value) = recorded {
        return (value.clamp(1, 4), "the producer's record");
    }
    (
        infer_density(size, target, others),
        "the picture's size and the output it lands on",
    )
}

/// The density to assume for a picture that states none.
///
/// A picture cannot hold more pixels than the screen it was captured on, so one
/// that does not fit the target's native resolution was not captured there: it
/// came from a bigger or denser output, and pinning it one to one would make it
/// larger than it ever was on screen.  The density of the screen that could
/// have produced it is used instead -- the smallest one that still holds every
/// pixel.  One that does fit keeps the target's own density, which is exactly
/// one picture pixel per screen pixel.
fn infer_density(size: Size, target: OutputPlacement, others: &[OutputPlacement]) -> u32 {
    let fits = |output: &OutputPlacement| {
        size.width <= output.pixel_size.width && size.height <= output.pixel_size.height
    };
    if fits(&target) {
        return target.scale.max(1);
    }
    others
        .iter()
        .filter(|output| fits(output))
        .min_by_key(|output| {
            u64::from(output.pixel_size.width) * u64::from(output.pixel_size.height)
        })
        .map(|output| output.scale.max(1))
        .unwrap_or_else(|| target.scale.max(1))
}

/// The file a screenshot tool records its captures in, unless one is named.
fn source_record_path() -> PathBuf {
    std::env::var_os("VSHOT_PIN_SOURCE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/screenshot-path"))
}

/// The density a screenshot tool recorded beside `path`, or `None`.
///
/// Two spellings, because two kinds of tool write one: a `<path>.scale` sidecar
/// whose last field is the number, and a shared record file holding one
/// `<path> <scale>` line per capture.  Both are how a program that is not vshot
/// tells a pin how dense the file it wrote is.
fn recorded_density(path: &Path, record: &Path) -> Option<u32> {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".scale");
    if let Ok(text) = std::fs::read_to_string(PathBuf::from(sidecar)) {
        if let Some(value) = text.split_whitespace().last().and_then(parse_density) {
            return Some(value);
        }
    }
    let text = std::fs::read_to_string(record).ok()?;
    let wanted = std::path::absolute(path).ok()?;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(named), Some(value)) = (fields.next(), fields.next()) else {
            // A bare number is only meaningful in a per-image file, which the
            // sidecar above is.
            continue;
        };
        if std::path::absolute(named).ok() != Some(wanted.clone()) {
            continue;
        }
        if let Some(value) = parse_density(value) {
            return Some(value);
        }
    }
    None
}

/// A density as one of those records spells it: a whole number in 1..=4, or a
/// value within a twentieth of one, so a tool that wrote `2.0` is believed and
/// one that wrote `300` -- a print resolution -- is not.
fn parse_density(token: &str) -> Option<u32> {
    let value: f64 = token.parse().ok()?;
    let rounded = value.round();
    ((1.0..=4.0).contains(&rounded) && (value - rounded).abs() <= 0.05).then_some(rounded as u32)
}

/// How well one output holds a pin: the area they overlap by, or the negative
/// of the gap between them when they do not meet.  Overlap beats any gap, and
/// among gaps the smallest wins -- which is what makes a pin dragged past every
/// edge come back to the nearest screen rather than to whichever happened to be
/// checked first.
fn hold_score(bounds: Rect, rect: Rect) -> i64 {
    let area = bounds.intersection(rect).map_or(0, |overlap| {
        i64::from(overlap.size.width) * i64::from(overlap.size.height)
    });
    if area > 0 {
        area
    } else {
        -i64::from(chebyshev_gap(bounds, rect))
    }
}

/// How far apart two rectangles are on their worst axis, and zero when they
/// meet: the Chebyshev distance, which is the one a rectangle's own edges are
/// measured in.
fn chebyshev_gap(a: Rect, b: Rect) -> i32 {
    let right = |rect: Rect| rect.origin.x + rect.size.width as i32 - 1;
    let bottom = |rect: Rect| rect.origin.y + rect.size.height as i32 - 1;
    let dx = (a.origin.x - right(b)).max(b.origin.x - right(a)).max(0);
    let dy = (a.origin.y - bottom(b)).max(b.origin.y - bottom(a)).max(0);
    dx.max(dy)
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

/// How much of a pin has to stay on the output it overlaps most, so that it can
/// always be grabbed back.  The same margin the daemon this replaces used.
const GRAB_MARGIN: i32 = 32;

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
    /// Where a pin may be put, which is what keeps one from being dragged past
    /// every screen.  Read once when the daemon comes up: a pin lives as long
    /// as the daemon does, and so does the arrangement it was placed against.
    outputs: Vec<OutputPlacement>,
}

impl Stack {
    fn new(outputs: Vec<OutputPlacement>) -> Self {
        Self {
            pins: Vec::new(),
            next_id: 1,
            all_visible: true,
            pointer: Pointer::default(),
            outputs,
        }
    }

    /// The size a pin of `source` pixels is drawn at, at `scale`.
    fn drawn(source: Size, scale: f64) -> Size {
        Size::new(
            (f64::from(source.width) * scale).round().max(1.0) as u32,
            (f64::from(source.height) * scale).round().max(1.0) as u32,
        )
    }

    /// A pin's rectangle in global logical pixels.
    fn rect_of(pin: &Pinned) -> Rect {
        let size = Self::drawn(pin.picture.size(), pin.scale);
        Rect::new(pin.origin.x, pin.origin.y, size.width, size.height)
    }

    /// Where a pin may be dragged to.
    ///
    /// At least [`GRAB_MARGIN`] of it has to stay on the output it overlaps
    /// most.  A drag that overshoots every screen lands back on the nearest one
    /// rather than somewhere it can never be reached from again -- which is
    /// what an unclamped drag does the first time someone flicks a pin off the
    /// edge.
    fn clamp_origin(&self, origin: Point, size: Size) -> Point {
        let rect = Rect::new(origin.x, origin.y, size.width, size.height);
        let Some(best) = self
            .outputs
            .iter()
            .map(|output| output.geometry)
            .max_by_key(|bounds| hold_score(*bounds, rect))
        else {
            return origin;
        };
        // Two ways of saying the same thing -- "the far edge is a margin inside
        // the near one" and the other way round -- taken as a range, so that a
        // pin wider than the output is still allowed to be placed somewhere.
        let across = [
            best.origin.x - size.width as i32 + GRAB_MARGIN,
            best.origin.x + best.size.width as i32 - GRAB_MARGIN,
        ];
        let down = [
            best.origin.y - size.height as i32 + GRAB_MARGIN,
            best.origin.y + best.size.height as i32 - GRAB_MARGIN,
        ];
        Point::new(
            origin
                .x
                .clamp(across[0].min(across[1]), across[0].max(across[1])),
            origin.y.clamp(down[0].min(down[1]), down[0].max(down[1])),
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
        let Some(index) = self.pins.iter().position(|pin| pin.id == id) else {
            self.pointer.drag = None;
            return Change::default();
        };
        let drawn = Self::rect_of(&self.pins[index]).size;
        let moved = self.clamp_origin(
            Point::new(origin.x + (point.x - from.x), origin.y + (point.y - from.y)),
            drawn,
        );
        if self.pins[index].origin == moved {
            return Change::default();
        }
        self.pins[index].origin = moved;
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
        let source = self.pins[index].picture.size();
        let corner = self.pins[index].origin;
        let before = Self::drawn(source, self.pins[index].scale);
        let next = (self.pins[index].scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        if next == self.pins[index].scale {
            return Change::default();
        }
        // The centre is what stays put: the pin is resized about it, so the
        // point the user aimed at is the point that grows under the cursor.  A
        // pin grown past the edge of its output is pulled back like a dragged
        // one, so the wheel cannot put a corner somewhere unreachable either.
        let centre = Point::new(
            corner.x + before.width as i32 / 2,
            corner.y + before.height as i32 / 2,
        );
        let after = Self::drawn(source, next);
        let origin = self.clamp_origin(
            Point::new(
                centre.x - after.width as i32 / 2,
                centre.y - after.height as i32 / 2,
            ),
            after,
        );
        let pin = &mut self.pins[index];
        pin.scale = next;
        pin.origin = origin;
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
            stack: Stack::new(surfaces.placements()),
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
        let size = picture.size();
        let placements = self.surfaces.placements();
        let target = self
            .surfaces
            .placement_of(output_name)
            .or_else(|| placements.first().copied());
        let (density, source) = match target {
            Some(target) => resolve_density(
                density,
                picture::declared_scale(&bytes),
                recorded_density(path, &source_record_path()),
                size,
                target,
                &placements,
            ),
            // No output at all: nothing to size it against, so the file's own
            // word is the only one there is.
            None => (
                density
                    .or_else(|| picture::declared_scale(&bytes))
                    .unwrap_or(1),
                "the request",
            ),
        };
        if std::env::var_os("VSHOT_PIN_DEBUG").is_some() {
            eprintln!("vshot-pin: density {density} from {source}");
        }
        let scale = 1.0 / f64::from(density);
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

    // --- how big a pin comes out ------------------------------------------

    fn output(width: u32, height: u32, scale: u32) -> OutputPlacement {
        OutputPlacement {
            geometry: Rect::new(0, 0, width / scale, height / scale),
            pixel_size: Size::new(width, height),
            scale,
        }
    }

    /// The order the sources are asked in: the caller's word beats the file's,
    /// the file's beats a screenshot tool's record, and all three beat a guess.
    #[test]
    fn the_density_sources_are_asked_in_order() {
        let target = output(1920, 1080, 1);
        let size = Size::new(100, 100);
        let (value, source) = resolve_density(Some(3), Some(2), Some(4), size, target, &[target]);
        assert_eq!((value, source), (3, "the request"));
        let (value, _) = resolve_density(None, Some(2), Some(4), size, target, &[target]);
        assert_eq!(value, 2);
        let (value, _) = resolve_density(None, None, Some(4), size, target, &[target]);
        assert_eq!(value, 4);
        // Nothing stated: the picture's own size decides, and one that fits the
        // target lands at exactly one picture pixel per screen pixel.
        let (value, source) = resolve_density(None, None, None, size, target, &[target]);
        assert_eq!(
            (value, source),
            (1, "the picture's size and the output it lands on")
        );
    }

    /// A picture too big for the output it is landing on came from a denser
    /// one, and is shown at the density of the smallest screen that could have
    /// produced it -- so it is never larger than it was on screen.
    #[test]
    fn a_picture_too_big_for_its_output_is_sized_from_the_one_it_came_from() {
        let small = output(1920, 1080, 1);
        let large = output(3840, 2160, 2);
        let huge = output(7680, 4320, 4);
        let arrangements = [small, large, huge];

        // 2560x1440 fits the 4K screen's native pixels but not the 1080p one's.
        let size = Size::new(2560, 1440);
        assert_eq!(infer_density(size, small, &arrangements), 2);
        // And on the screen it fits, it is one to one.
        assert_eq!(infer_density(size, large, &arrangements), 2);
        // A picture nothing can hold keeps the target's own density rather
        // than being scaled to a screen that does not exist.
        let enormous = Size::new(16_000, 16_000);
        assert_eq!(infer_density(enormous, large, &arrangements), 2);
    }

    /// A density in a record is a whole number in range, or a value close
    /// enough to one; a print resolution is not a density.
    #[test]
    fn only_a_screen_density_is_read_from_a_record() {
        assert_eq!(parse_density("2"), Some(2));
        assert_eq!(parse_density("2.0"), Some(2));
        assert_eq!(parse_density("2.05"), Some(2));
        assert_eq!(parse_density("300"), None);
        assert_eq!(parse_density("0"), None);
        assert_eq!(parse_density("5"), None);
        assert_eq!(parse_density("two"), None);
    }

    /// Both spellings of a screenshot tool's record are read: the sidecar
    /// beside the file, and the shared file with one line per capture.
    #[test]
    fn a_screenshot_tools_record_is_read_in_both_spellings() {
        let directory = tempfile::tempdir().expect("tempdir");
        let shot = directory.path().join("shot.png");
        std::fs::write(&shot, b"png").unwrap();

        // The sidecar first, which is the one that travels with the file.
        let mut sidecar = shot.as_os_str().to_os_string();
        sidecar.push(".scale");
        std::fs::write(PathBuf::from(&sidecar), b"2\n").unwrap();
        assert_eq!(
            recorded_density(&shot, &directory.path().join("none")),
            Some(2)
        );
        std::fs::remove_file(&sidecar).unwrap();

        // Then the shared record, which names the file on its line.  A line for
        // another picture is not this one's.
        let record = directory.path().join("screenshot-path");
        let other = directory.path().join("other.png");
        std::fs::write(&other, b"png").unwrap();
        std::fs::write(
            &record,
            format!("{} 3\n{} 4\n", other.display(), shot.display()),
        )
        .unwrap();
        assert_eq!(recorded_density(&shot, &record), Some(4));
        // No record at all is no answer, not a wrong one.
        assert_eq!(
            recorded_density(&shot, &directory.path().join("none")),
            None
        );
    }

    // --- what a gesture means ---------------------------------------------

    /// A stack of `count` pins on one 1920x1080 output, each 100x50 at 1:1,
    /// laid out in a row so that they do not overlap unless a test makes them.
    fn stack_of(count: usize) -> Stack {
        let mut stack = Stack::new(vec![output(1920, 1080, 1)]);
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
    ///
    /// The pin sits in the middle of the output on purpose: one against an edge
    /// is held there by the grab margin, so a wheel at the corner moves it as
    /// well as resizing it, and that is a different test.
    #[test]
    fn the_wheel_scales_about_the_pins_centre() {
        let mut stack = stack_of(1);
        stack.pins[0].origin = Point::new(910, 515);
        let change = stack.scroll(1, Point::new(960, 540));
        assert!(change.redraw);
        assert!((stack.pins[0].scale - ZOOM_PER_NOTCH).abs() < 1e-9);
        // 100x50 about its centre (960, 540) at 1.1 is 110x55, so the top-left
        // moves back by five and two and a half.
        assert_eq!(stack.pins[0].origin, Point::new(905, 513));

        // One notch back and it is where it started.
        stack.scroll(-1, Point::new(960, 540));
        assert!((stack.pins[0].scale - 1.0).abs() < 1e-9);
        assert_eq!(stack.pins[0].origin, Point::new(910, 515));

        // The ends of the range hold.
        for _ in 0..200 {
            stack.scroll(1, Point::new(960, 540));
        }
        assert_eq!(stack.pins[0].scale, MAX_SCALE);
        for _ in 0..300 {
            stack.scroll(-1, Point::new(960, 540));
        }
        assert_eq!(stack.pins[0].scale, MIN_SCALE);
    }

    /// A drag that overshoots every screen leaves a corner of the pin behind,
    /// so it can always be grabbed back.  Without this the first flick off the
    /// edge loses the pin for good.
    #[test]
    fn a_drag_off_the_edge_leaves_the_pin_reachable() {
        let mut stack = stack_of(1);
        let start = Instant::now();
        stack.press(Point::new(10, 10), start);
        // Far past the right edge, and far above the top.
        stack.motion(Point::new(9000, -9000));
        let origin = stack.pins[0].origin;
        // A 100x50 pin: a margin's worth of it is still on the 1920x1080 output
        // at 0,0, on both axes.
        assert_eq!(origin, Point::new(1920 - GRAB_MARGIN, -50 + GRAB_MARGIN));
        // And it is still hit-testable, which is the point of the margin.
        assert!(stack.index_at(Point::new(1919, 0)).is_some());
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
