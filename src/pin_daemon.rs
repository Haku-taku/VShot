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

use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::error::{Result, VshotError};
use crate::geometry::{Point, Rect, Size};
use crate::model::picture::{self, Picture};
use crate::pin::{PinCommand, PqPin};
use crate::pin_hdr::{OutputPlacement, Pin, Surfaces};

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
    /// Device pixels per logical pixel of the picture, as the sizing decided.
    /// The zoom badge reports its factor relative to this, so a 4K capture
    /// pinned at its natural size on a 4K output reads as 100%.
    density: u32,
    visible: bool,
    /// The marks the pixels carry, as the editor reported them, when the pin
    /// came out of an editing session that had any.
    ///
    /// Kept so the pin can be opened for editing again with the user's marks
    /// still on them: they are the marks *as data*, which the flattened pixels
    /// no longer are.  Nothing reads them yet -- the editor's handoff is not
    /// wired -- but a write-back that dropped them would throw away the only
    /// copy there is.
    annotations: Option<serde_json::Value>,
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
pub(crate) fn hold_score(bounds: Rect, rect: Rect) -> i64 {
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

/// One pin as the chrome needs it: where it is, how big it is drawn, and what
/// is worth saying about it.
///
/// Deliberately not the pin itself.  The chrome draws labels and no pictures,
/// so the pixels would be a copy of something that side can never show -- and
/// the daemon is the one holding them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChromeLabel {
    pub(crate) id: u64,
    pub(crate) origin: Point,
    pub(crate) size: Size,
    /// Whether this pin is an HDR capture, whatever is showing it.
    pub(crate) captured_hdr: bool,
    /// Whether the pixels on screen are the capture's own light, as opposed to
    /// the same capture mapped down for an SDR output.  The tag is drawn in a
    /// different ink for each.
    pub(crate) shown_as_hdr: bool,
    /// Whether the pointer is over this pin, which is the only one whose tag is
    /// up.
    pub(crate) hovered: bool,
    pub(crate) visible: bool,
}

/// A pin's right-click menu, as the chrome draws it.
///
/// The rows are the daemon's -- it is the side that knows what a pin is and
/// what can be done to it -- and the drawing, the pointer and the keyboard are
/// the chrome's, which is also the side that *names* them: it has the font and
/// the translation table, and a menu spelled here would be English in a Chinese
/// session.  What comes back is one row number, or nothing.
#[derive(Clone, Debug, PartialEq)]
struct Menu {
    id: u64,
    /// Where the menu is anchored, in global logical pixels: the pointer that
    /// opened it.
    anchor: Point,
    /// How many rows the menu has, so a pick can be checked against it.
    rows: usize,
    /// The rectangle it occupies, in global logical pixels, once the chrome has
    /// drawn it.  The chrome answers with it -- it is the side with the font, so
    /// it is the side that knows how wide the rows are -- and the pins under it
    /// give their input up over it, because a click goes to the topmost surface
    /// whose region holds it.
    rect: Option<Rect>,
    /// The row the pointer is over, or `None` while it is over none.
    highlighted: Option<usize>,
}

/// How many rows every pin's menu ends with, after any the pin itself
/// contributes.
///
/// The *names* are the chrome's: it is the side with the font and the
/// translation table, and a menu spelled here would be English in a Chinese
/// session.  What travels is how many rows there are, and the row the user
/// picked.
const MENU_ACTION_ROWS: usize = 6;

/// What each of those rows does, in the order the chrome draws them.
const MENU_ACTIONS: [&str; MENU_ACTION_ROWS] = [
    "Copy image",
    "Save as…",
    "Edit",
    "Reset zoom",
    "Recognize text…",
    "Close",
];

/// One open editing session: the pin it is about, and the files it was handed.
///
/// Kept so the daemon knows an edit is running -- two editors on one stack
/// would each be drawing marks the other knows nothing about -- and so that the
/// session's directory lives exactly as long as the edit does.
struct EditSession {
    #[allow(dead_code)] // read when the editor's write-back lands
    id: u64,
    _directory: tempfile::TempDir,
}

/// The rectangle a message carries, or `None` when it carries none.
fn json_rect(value: &serde_json::Value) -> Option<Rect> {
    let x = value.get("x")?.as_i64()?;
    let y = value.get("y")?.as_i64()?;
    let width = value.get("width")?.as_u64()?;
    let height = value.get("height")?.as_u64()?;
    Some(Rect::new(
        i32::try_from(x).ok()?,
        i32::try_from(y).ok()?,
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    ))
}

/// One rectangle as the editor's session file spells it.
fn rect_json(rect: Rect) -> serde_json::Value {
    json!({
        "x": rect.origin.x,
        "y": rect.origin.y,
        "width": rect.size.width,
        "height": rect.size.height,
    })
}

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
    /// The pin the pointer is over, which is the only one whose `HDR` tag is
    /// up.  Kept here rather than in the chrome because it is a fact about the
    /// stack: a pin that goes away, or moves out from under the pointer, stops
    /// being hovered without the pointer moving at all.
    hovered: Option<u64>,
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
    /// How wide a pin's rim is drawn, in logical pixels.  The editor wants it
    /// because the stroke reaches half this far outside the image, and a drag
    /// starting on it should move the pin rather than read as a click on the
    /// canvas.
    border_width: i32,
    pointer: Pointer,
    /// Where a pin may be put, which is what keeps one from being dragged past
    /// every screen.  Read once when the daemon comes up: a pin lives as long
    /// as the daemon does, and so does the arrangement it was placed against.
    outputs: Vec<OutputPlacement>,
}

impl Stack {
    fn new(outputs: Vec<OutputPlacement>, border_width: i32) -> Self {
        Self {
            pins: Vec::new(),
            next_id: 1,
            all_visible: true,
            border_width,
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
    /// A right-click on `point`: the pin under it, whose menu should open.
    fn menu_target(&self, point: Point) -> Option<u64> {
        self.index_at(point).map(|index| self.pins[index].id)
    }

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
            if self.pointer.hovered == Some(id) {
                self.pointer.hovered = None;
            }
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

    /// The pointer moved to `point`, with or without a button down.
    ///
    /// The hover is what the tag follows, so it is worked out on every motion:
    /// a drag that carries a pin out from under the pointer takes the tag with
    /// it, and one that brings a pin under it raises the tag.
    fn hover(&mut self, point: Point) -> Change {
        let hovered = self.index_at(point).map(|index| self.pins[index].id);
        if hovered == self.pointer.hovered {
            return Change::default();
        }
        self.pointer.hovered = hovered;
        Change {
            redraw: true,
            closed: false,
        }
    }

    /// The pointer left the pins, so nothing is hovered any more.
    fn leave(&mut self) -> Change {
        if self.pointer.hovered.is_none() {
            return Change::default();
        }
        self.pointer.hovered = None;
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
        // Away from the user -- the direction Wayland calls positive -- is
        // zoom *out*, the way a wheel behaves in every viewer: a notch down
        // makes the picture smaller, a notch up makes it bigger.  The sign is
        // inverted here rather than at the source, because the compositor's
        // convention is the one worth keeping and this is where it turns into a
        // zoom.
        let factor = ZOOM_PER_NOTCH.powi(-notches);
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

    /// Gives the pin at `index` a new picture, read from `path`.
    ///
    /// Everything else about the pin stays: where it is, how big it is drawn,
    /// whether it is hidden.  The editor replaced the *pixels*, and a
    /// write-back that also moved the pin would undo the drag the user did
    /// before opening it.
    ///
    /// The marks go with the picture they described: one that arrives with its
    /// own gets them from the request that carried it, and one that arrives
    /// without any has none to keep.
    /// The labels the chrome should draw, back to front.
    ///
    /// Every pin contributes the same two facts -- whether it is an HDR capture
    /// and whether what is on screen is its own light -- and the hover picks
    /// which of them shows a tag.
    fn labels(&self) -> Vec<ChromeLabel> {
        self.pins
            .iter()
            .map(|pin| ChromeLabel {
                id: pin.id,
                origin: pin.origin,
                size: Self::drawn(pin.picture.size(), pin.scale),
                // "Is this an HDR capture" is a question about the picture, and
                // the answer for a *pin* is whether it holds light above SDR
                // white at all -- not whether enough of it does.  The area test
                // is for a capture deciding whether a second file is worth
                // writing, and applying it here would take the tag off a small
                // pin of a bright highlight, which is exactly the pin a user
                // wants to know is HDR.
                captured_hdr: pin.picture.carries_hdr(crate::model::hdr::HdrDecision {
                    always: true,
                    ratio: 0.0,
                }),
                shown_as_hdr: pin.picture.as_hdr().is_some(),
                hovered: self.pointer.hovered == Some(pin.id),
                visible: pin.visible,
            })
            .collect()
    }

    fn replace(&mut self, index: usize, picture: Picture, path: &Path) {
        let pin = &mut self.pins[index];
        pin.picture = picture;
        pin.path = path.to_path_buf();
        pin.annotations = None;
    }

    /// Moves the pin with `id` to the front, if it is in the stack.
    fn raise_by_id(&mut self, id: u64) -> Option<u64> {
        let index = self.pins.iter().position(|pin| pin.id == id)?;
        self.raise(index);
        Some(id)
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

/// The Qt helper that can draw the labels, if this machine has one.
///
/// Resolved the way every other helper is, so a build tree and an installed
/// copy both find theirs: `VSHOT_QT_HELPER` first, then the binary beside this
/// one and the two build layouts under it.
fn helper_program() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VSHOT_QT_HELPER") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let directory = std::env::current_exe().ok()?.parent()?.to_path_buf();
    [
        directory.join("vshot-qt-ui"),
        directory.join("../build-qt/vshot-qt-ui"),
        directory.join("../../build-qt/vshot-qt-ui"),
        PathBuf::from("/usr/bin/vshot-qt-ui"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

/// The clipboard type to read an image from, out of what the clipboard offers.
///
/// The preferred types first, because a clipboard usually offers several and
/// they are not equal: PNG is lossless and is what every screenshot path here
/// writes, while the same picture as JPEG has been through a lossy encoder
/// already.  Anything else under `image/` is taken when none of those is on
/// offer, and it is up to the decoder to say whether it can read it -- a
/// refusal from there names the type, which is more use than a guess here.
fn clipboard_image_type(listed: &str) -> Option<&str> {
    const PREFERRED: [&str; 5] = [
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/bmp",
        "image/tiff",
    ];
    let offered = |wanted: &str| listed.lines().any(|line| line.trim() == wanted);
    PREFERRED
        .iter()
        .copied()
        .find(|kind| offered(kind))
        .or_else(|| {
            listed
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("image/"))
        })
}

/// The formats the save dialog may offer, as the `name/suffix,...` argument it
/// parses.
///
/// Built from the registry rather than from a list kept here: which formats a
/// build can write is the codecs' answer, and a dialog that knew names of its
/// own would eventually offer one the binary cannot write.
fn save_format_argument(hdr: bool) -> String {
    let codecs: Vec<(&str, &str, &[&str])> = if hdr {
        crate::model::codec::codecs()
            .into_iter()
            .map(|codec| (codec.name(), codec.extension(), codec.suffixes()))
            .collect()
    } else {
        crate::model::codec::sdr_codecs()
            .into_iter()
            .map(|codec| (codec.name(), codec.extension(), codec.suffixes()))
            .collect()
    };
    codecs
        .into_iter()
        .map(|(name, extension, suffixes)| {
            // A format spelled one way reports no suffixes and is named by its
            // extension -- which is what the registry's own JSON does, and the
            // dialog reads both the same way.  Reading the empty list as "no
            // suffix at all" would drop every format but JPEG.
            if suffixes.is_empty() {
                format!("{name}/{extension}")
            } else {
                format!("{name}/{}", suffixes.join("/"))
            }
        })
        .collect::<Vec<String>>()
        .join(",")
}

/// Runs the save dialog and answers what the user chose: the path, and whether
/// the SDR copy was asked for beside it.
///
/// `None` when the dialog was cancelled or could not run, which are the same
/// thing from here: nothing was asked for.  A daemon that reported a failure
/// for a closed dialog would be telling the user something went wrong when
/// they simply changed their mind.
fn run_save_dialog(
    helper: &Path,
    suggested: &str,
    output_name: &str,
    formats: &str,
    hdr: bool,
) -> Result<Option<(PathBuf, bool)>> {
    let output = std::process::Command::new(helper)
        .arg("--save-dialog")
        .arg(suggested)
        .arg(output_name)
        .arg(formats)
        .arg(if hdr { "hdr" } else { "sdr" })
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|source| VshotError::Pin(format!("cannot run the save dialog: {source}")))?;
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    if !reply
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let Some(path) = reply.get("path").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    // The SDR copy is only ever asked about for content that has an HDR half;
    // for anything else the answer is "there is nothing to copy beside".
    let copy = reply
        .get("sdrCopy")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(Some((PathBuf::from(path), copy)))
}

/// Puts `bytes` on the clipboard as a PNG, answering whether it got there.
///
/// `wl-copy` forks and the child stays alive as the selection owner, so only
/// the short-lived process started here is waited for: the copy outlives it,
/// which is what a clipboard is.
fn copy_to_clipboard(png: &[u8]) -> bool {
    use std::io::Write as _;
    let Ok(mut child) = std::process::Command::new("wl-copy")
        .arg("--type")
        .arg("image/png")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let written = child
        .stdin
        .as_mut()
        .is_some_and(|stdin| stdin.write_all(png).is_ok());
    // A `wl-copy` that could not take the bytes must not be left holding a
    // half-written selection.
    if !written {
        let _ = child.kill();
        return false;
    }
    child.wait().is_ok_and(|status| status.success())
}

/// Where the chrome process listens: beside the daemon's own socket, with a
/// name of its own, so both can live in the runtime directory a session owns
/// without either having to be told about the other.
fn chrome_socket(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(".chrome");
    PathBuf::from(name)
}

/// The Qt process that draws the pins' corner labels.
///
/// The pictures are this side's -- a half-float surface has light in it and no
/// glyphs -- so the text is somebody else's, on a transparent layer surface
/// mapped after ours.  The connection is one way and lossy on purpose: a label
/// is decoration, and a chrome process that has died or never started leaves
/// every pin drawn exactly as it was, minus its tags.
struct Chrome {
    stream: Option<UnixStream>,
    /// What was last sent, so a stack that has not changed is not sent again.
    /// A drag sends one update per motion event, and every one of them would
    /// otherwise be a line and a repaint on the other side.
    sent: Option<Vec<ChromeLabel>>,
    /// What the chrome has said back: which menu row the user picked, or that
    /// the menu was dismissed.  The menu is the one thing that side decides --
    /// it is the side with the pointer and the keyboard there.
    replies: Vec<ChromeReply>,
    /// What is left of a line that has not arrived whole yet.
    pending: Vec<u8>,
}

/// What the chrome reports back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChromeReply {
    /// The user picked a row of the open menu.
    Chosen(usize),
    /// The menu was drawn, and this is the rectangle it occupies in global
    /// logical pixels.  The pins under it have to give their input up over it,
    /// and only the chrome knows how wide the rows came out.
    Menu(Rect),
    /// The menu was dismissed without a pick: Esc, a click outside it, or the
    /// pin it belonged to going away.
    Dismissed {
        /// Whether the click that closed it was over no row and should reach
        /// whatever is underneath.  The chrome took it only because a menu
        /// closes on a click anywhere; swallowing it as well would make the
        /// first click after a menu do nothing at all.
        passthrough: bool,
    },
}

impl Chrome {
    /// Starts the Qt process that draws the labels, then connects to it.
    ///
    /// Started rather than required: the daemon owns no Qt, so a session where
    /// the helper cannot come up -- no Qt, no layer shell -- keeps every pin
    /// and loses only its tags.  The process is waited for briefly, because the
    /// first stack it is told about should be the one that is already up.
    fn start(socket: &Path, program: Option<&Path>) -> Self {
        let mut chrome = Self {
            stream: None,
            sent: None,
            replies: Vec::new(),
            pending: Vec::new(),
        };
        let Some(program) = program else {
            return chrome;
        };
        // A socket left behind by a chrome that was killed makes every later
        // connect fail; the path is this daemon's own, so clearing it is safe.
        let _ = std::fs::remove_file(socket);
        let spawned = std::process::Command::new(program)
            .arg("--pin-chrome")
            .arg(socket)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn();
        if spawned.is_err() {
            return chrome;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(stream) = UnixStream::connect(socket) {
                // Non-blocking so the daemon's own loop can read whatever the
                // chrome has said without ever waiting on it.
                let _ = stream.set_nonblocking(true);
                chrome.stream = Some(stream);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        chrome
    }

    /// Tells the chrome what to draw, if that is not what it is already drawing.
    fn sync(&mut self, labels: &[ChromeLabel], visible: bool) {
        if self.sent.as_deref() == Some(labels) {
            return;
        }
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let message = json!({
            "cmd": "labels",
            "visible": visible,
            "pins": labels
                .iter()
                .map(|label| json!({
                    "id": label.id,
                    "x": label.origin.x,
                    "y": label.origin.y,
                    "width": label.size.width,
                    "height": label.size.height,
                    "hdr": label.captured_hdr,
                    "shown": label.shown_as_hdr,
                    "hovered": label.hovered,
                }))
                .collect::<Vec<serde_json::Value>>(),
        })
        .to_string();
        // A chrome that has gone away is not an error worth reporting: the
        // labels are missing and every pin is still there.
        if stream.write_all(message.as_bytes()).is_err()
            || stream.write_all(b"\n").is_err()
            || stream.flush().is_err()
        {
            self.stream = None;
            return;
        }
        self.sent = Some(labels.to_vec());
    }

    /// A handle on the connection, for a caller that has to wait on it.
    fn fd(&self) -> Option<std::os::fd::OwnedFd> {
        use std::os::fd::AsFd;
        self.stream.as_ref()?.as_fd().try_clone_to_owned().ok()
    }

    /// Reads whatever the chrome has said since the last look.
    ///
    /// Non-blocking and never an error: a chrome that has gone away is not a
    /// failure, it is a stack without labels, and every pin is still there.
    fn poll(&mut self) -> Vec<ChromeReply> {
        let Some(stream) = self.stream.as_mut() else {
            return Vec::new();
        };
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    // The chrome closed the connection: it is gone, and nothing
                    // it says from here is worth waiting for.
                    self.stream = None;
                    break;
                }
                Ok(count) => self.pending.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.stream = None;
                    break;
                }
            }
        }
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=newline).collect::<Vec<u8>>();
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            let Ok(message) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
                continue;
            };
            match message.get("cmd").and_then(serde_json::Value::as_str) {
                Some("chosen") => {
                    if let Some(row) = message.get("row").and_then(serde_json::Value::as_u64) {
                        self.replies.push(ChromeReply::Chosen(row as usize));
                    }
                }
                Some("dismissed") => {
                    let passthrough = message
                        .get("passthrough")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    self.replies.push(ChromeReply::Dismissed { passthrough });
                }
                Some("menu") => {
                    if let Some(rect) = message.get("rect").and_then(json_rect) {
                        self.replies.push(ChromeReply::Menu(rect));
                    }
                }
                _ => {}
            }
        }
        std::mem::take(&mut self.replies)
    }

    /// Tells the chrome to draw a pin's menu, or to take it down.
    fn menu(&mut self, menu: Option<&Menu>) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let message = match menu {
            Some(menu) => json!({
                "cmd": "menu",
                "id": menu.id,
                "x": menu.anchor.x,
                "y": menu.anchor.y,
                "rows": menu.rows,
                "highlighted": menu.highlighted,
            }),
            None => json!({"cmd": "menu"}),
        }
        .to_string();
        if stream.write_all(message.as_bytes()).is_err()
            || stream.write_all(b"\n").is_err()
            || stream.flush().is_err()
        {
            self.stream = None;
        }
    }

    /// Puts `text` on one pin's corner for a moment.
    fn badge(&mut self, id: u64, text: &str) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let message = json!({"cmd": "badge", "id": id, "text": text}).to_string();
        if stream.write_all(message.as_bytes()).is_err()
            || stream.write_all(b"\n").is_err()
            || stream.flush().is_err()
        {
            self.stream = None;
        }
    }
}

/// The daemon: the pins, and what is showing them.
struct Daemon {
    stack: Stack,
    surfaces: Surfaces,
    chrome: Chrome,
    /// The open editing session, if any.
    editing: Option<EditSession>,
    /// The clients that asked to be told which pin is the live one, each with
    /// the answer it was last given.  Only the pin editor asks; keeping the
    /// last answer is what makes a change a change, rather than a line written
    /// to every client on every sync of a drag.
    watchers: Vec<Option<UnixStream>>,
    /// What those clients were last told.
    announced_active: u64,
    /// The open menu, if any.  One at a time: a second right-click replaces it,
    /// which is what a menu that follows the pointer should do.
    menu: Option<Menu>,
    /// Where the pointer was last seen.  A click the chrome passes on arrives
    /// without coordinates -- the chrome took it on its own surface -- and this
    /// is what says where it was.
    last_pointer: Option<Point>,
    /// The socket clients reach this daemon on.  The editor is told about it
    /// because it drives the real pin over it rather than drawing a copy.
    socket: PathBuf,
    /// When this daemon should give up, once it has nothing to hold.
    idle: Option<Instant>,
}

impl Daemon {
    fn new(socket: &Path) -> Result<Self> {
        let mut surfaces = Surfaces::new()?;
        surfaces.start()?;
        let chrome = Chrome::start(&chrome_socket(socket), helper_program().as_deref());
        Ok(Self {
            stack: Stack::new(surfaces.placements(), surfaces.border_width()),
            surfaces,
            chrome,
            editing: None,
            watchers: Vec::new(),
            announced_active: 0,
            menu: None,
            last_pointer: None,
            socket: socket.to_path_buf(),
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
        //
        // A pin under an open menu gives its rect up: the menu is drawn by a
        // surface above this one, and the compositor hands a click to the
        // topmost surface whose input region holds it -- so a pin that kept its
        // own rect under the menu would take every click the menu was waiting
        // for, and no row over the pin could ever be picked.
        let covered = self.menu.as_ref().and_then(|menu| menu.rect);
        let rects = self
            .stack
            .pins
            .iter()
            .filter(|pin| pin.visible)
            .map(|pin| Stack::rect_of(pin))
            .filter(|rect| covered.is_none_or(|covered| !covered.contains(rect.origin)))
            .collect::<Vec<Rect>>();
        self.surfaces.set_input_rects(&rects)?;
        // And the keyboard, which follows the hover: a pin wants a key only
        // while the pointer is over one, and the rest of the time it belongs to
        // the window the user is actually working in.
        self.surfaces
            .set_pin_keyboard(self.stack.pointer.hovered.is_some())?;
        self.chrome
            .sync(&self.stack.labels(), self.stack.all_visible);
        Ok(())
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

    /// The edit is over: the editor's connection has gone.
    ///
    /// The session's directory goes with it, and the pin takes its input back
    /// -- while an edit is open the editor covers the whole output and every
    /// press has to reach it, which is why the pin gives its rect up at all.
    fn end_edit(&mut self) {
        if self.editing.take().is_none() {
            return;
        }
        // The editor draws its own frame, and it is gone; the pin is drawn by
        // this side again.
        self.announced_active = 0;
        if let Err(error) = self.refresh() {
            eprintln!("vshot-pin: {error}");
        }
    }

    /// The pin the editor should draw its frame around: the one the open edit
    /// is on, while that pin is still the live one.
    ///
    /// A pin is live while its surface holds the keyboard and the pointer is
    /// over it, which this side reads as the hover.  Zero is not so much "no
    /// live pin" as "no pin surface has the keyboard", which is the ordinary
    /// state of an open editor -- the editor's own surface is the one holding
    /// it -- so the answer stays as it was until another pin takes the pointer.
    fn live_pin(&self) -> u64 {
        match &self.editing {
            Some(session) => {
                if self.stack.pointer.hovered.is_none()
                    || self.stack.pointer.hovered == Some(session.id)
                {
                    session.id
                } else {
                    0
                }
            }
            None => 0,
        }
    }

    /// Tells every watcher which pin is live, when the answer changes.
    ///
    /// Only on a change: a drag syncs on every motion event, and a line per
    /// watcher per event would put the editor's move replies behind a growing
    /// queue of answers it has no use for.
    fn announce_active(&mut self) {
        let live = self.live_pin();
        if live == self.announced_active {
            return;
        }
        self.announced_active = live;
        let mut line = json!({"active": live}).to_string();
        line.push('\n');
        self.watchers.retain_mut(|watcher| {
            let Some(stream) = watcher.as_mut() else {
                return false;
            };
            stream.write_all(line.as_bytes()).is_ok() && stream.flush().is_ok()
        });
    }

    /// Opens the menu for the pin under `at`, or closes whatever menu is open
    /// when there is no pin there.
    ///
    /// The rows are this side's: it is the side that knows what a pin is and
    /// what can be done to it.  Drawing them, tracking the pointer over them
    /// and reading the keyboard are the chrome's, which is why the rows travel
    /// as text and the answer comes back as a row number.
    fn open_menu(&mut self, at: Point) {
        let Some(id) = self.stack.menu_target(at) else {
            // A right-click on the desktop is not this stack's business, and it
            // takes any open menu with it -- which is what a click outside a
            // menu does everywhere else.
            if self.menu.take().is_some() {
                self.chrome.menu(None);
            }
            return;
        };
        let menu = Menu {
            id,
            anchor: at,
            rows: MENU_ACTION_ROWS,
            highlighted: None,
            rect: None,
        };
        self.chrome.menu(Some(&menu));
        self.menu = Some(menu);
    }

    /// Does what the user picked out of an open menu.
    ///
    /// The row is a number because that is what the chrome can answer with: it
    /// draws the text it was given and knows nothing about what the rows mean.
    /// A number out of range is not an error -- the menu may have been drawn by
    /// an older daemon -- it is simply nothing to do.
    fn choose(&mut self, row: usize) -> Result<()> {
        let Some(menu) = self.menu.take() else {
            return Ok(());
        };
        self.chrome.menu(None);
        let id = menu.id;
        // The first rows are the pin's own formats, when it has any: a colour
        // card offers its values there, and picking one copies it.  They are
        // not in this side's gift -- a card is Qt's to render and its text is
        // what a copy hands over -- so they are left to the chrome's own list.
        // The rows are the chrome's names for these actions, in this order.
        let Some(action) = MENU_ACTIONS.get(row) else {
            return Ok(());
        };
        match *action {
            "Copy image" => self.copy_image(id),
            "Save as…" => {
                self.save(id)?;
            }
            "Edit" => {
                self.start_edit(id, false)?;
            }
            "Reset zoom" => {
                if let Some(index) = self.stack.pins.iter().position(|pin| pin.id == id) {
                    let pin = &mut self.stack.pins[index];
                    pin.scale = 1.0 / f64::from(pin.density);
                    self.refresh()?;
                }
            }
            "Recognize text…" => {
                self.start_edit(id, true)?;
            }
            "Close" => {
                self.stack.pins.retain(|pin| pin.id != id);
                self.refresh()?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Puts one pin's picture on the clipboard.
    ///
    /// Through `wl-copy`, the same tool the capture side writes the clipboard
    /// with, and as a PNG: a clipboard is read by every kind of program, and
    /// PNG is the one image format all of them have.
    fn copy_image(&mut self, id: u64) {
        let Some(pin) = self.stack.pins.iter().find(|pin| pin.id == id) else {
            return;
        };
        let encoded = match &pin.picture {
            Picture::Sdr(frame) => frame.to_png(),
            Picture::Hdr(image) => image
                .frame
                .tone_map_to_srgb_with(crate::config::tone_map_options())
                .and_then(|frame| frame.to_png()),
        };
        let text = match encoded {
            Ok(bytes) => copy_to_clipboard(&bytes),
            Err(error) => {
                eprintln!("vshot-pin: could not encode the pin to copy it: {error}");
                false
            }
        };
        // Reported on the pin's own corner, the way a save is: the menu that
        // started it has closed by now, and a silent failure looks like the
        // row doing nothing at all.
        self.chrome
            .badge(id, if text { "Copied image" } else { "Copy failed" });
    }

    /// Does whatever the chrome has reported since the last look.
    ///
    /// Separate from `gestures` because the two come from different places and
    /// mean different things: a gesture is the pointer on a pin, and this is
    /// the chrome's answer about a menu it drew.
    fn chrome_replies(&mut self) -> Result<()> {
        for reply in self.chrome.poll() {
            match reply {
                ChromeReply::Chosen(row) => self.choose(row)?,
                ChromeReply::Dismissed { passthrough } => {
                    // The chrome took the menu down itself -- Esc, or a click
                    // outside it -- and all this side has to do is forget it.
                    self.menu = None;
                    // The pins take their input back, now that nothing is over
                    // them.
                    self.refresh()?;
                    // A click that was aimed past the menu is handed on, so it
                    // lands where the user pointed rather than being spent on
                    // closing a menu.  It is a *click*: the button was already
                    // let go by the time this arrives -- the chrome took the
                    // press and the release -- so the gesture is started and
                    // ended here rather than left open, which would have the
                    // pin follow the pointer until the user pressed and
                    // released again to finish a drag they never began.
                    if passthrough {
                        if let Some(at) = self.last_pointer {
                            let raised = self.stack.press(at, Instant::now());
                            self.stack.release();
                            if raised.redraw {
                                self.refresh()?;
                            }
                        }
                    }
                }
                ChromeReply::Menu(rect) => {
                    if let Some(menu) = self.menu.as_mut() {
                        menu.rect = Some(rect);
                    }
                    // The pins under the menu give their input up over it.
                    self.refresh()?;
                }
            }
        }
        Ok(())
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
                crate::wayland::PinEvent::Press { at, button, .. }
                    if button == crate::wayland::input::BTN_RIGHT =>
                {
                    // The menu is the chrome's to draw and the user's to pick
                    // from; all this side does is say which pin it is about.
                    self.open_menu(at);
                    Change::default()
                }
                crate::wayland::PinEvent::Press { at, .. } => {
                    self.last_pointer = Some(at);
                    self.stack.press(at, now)
                }
                crate::wayland::PinEvent::Motion { at } => {
                    self.last_pointer = Some(at);
                    // The hover first: it is what the tag follows, and a motion
                    // with no button down still changes it.
                    let hovered = self.stack.hover(at);
                    let dragged = self.stack.motion(at);
                    Change {
                        redraw: hovered.redraw || dragged.redraw,
                        closed: dragged.closed,
                    }
                }
                crate::wayland::PinEvent::Release => self.stack.release(),
                crate::wayland::PinEvent::Scroll { notches, at } => self.stack.scroll(notches, at),
                crate::wayland::PinEvent::Leave => self.stack.leave(),
                // Space opens the editor on the pin under the pointer, which
                // is the one the tag is up on.  Everything else the pins do
                // not answer to is left to the compositor: a key swallowed
                // here would be a key no other window ever sees.
                crate::wayland::PinEvent::Key { key, pressed }
                    if pressed && key == crate::wayland::input::KEY_SPACE =>
                {
                    if std::env::var_os("VSHOT_PIN_DEBUG").is_some() {
                        eprintln!(
                            "vshot-pin: space: hovered = {:?}",
                            self.stack.pointer.hovered
                        );
                    }
                    if let Some(id) = self.stack.pointer.hovered {
                        // Reported rather than returned: a key is not a gesture
                        // that changes the stack, and a failure to start the
                        // editor must not take the daemon down.
                        if let Err(error) = self.start_edit(id, false) {
                            eprintln!("vshot-pin: {error}");
                        }
                    }
                    Change::default()
                }
                crate::wayland::PinEvent::Key { .. } => Change::default(),
            };
            changed.redraw |= change.redraw;
            changed.closed |= change.closed;
            // The wheel's own report: the factor it just applied, relative to
            // the picture's native density, on the corner of the pin it was
            // aimed at.  A drag has no such thing to say.
            if let crate::wayland::PinEvent::Scroll { at, .. } = event {
                if let Some(id) = self
                    .stack
                    .index_at(at)
                    .map(|index| self.stack.pins[index].id)
                {
                    if let Some(pin) = self.stack.pins.iter().find(|pin| pin.id == id) {
                        let percent = (pin.scale * f64::from(pin.density) * 100.0).round() as i64;
                        self.chrome.badge(id, &format!("{percent}%"));
                    }
                }
            }
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
        // The hover is what says which pin is live, so a change in it is what
        // the editor watching for its frame is waiting on.
        self.announce_active();
        Ok(())
    }

    /// Answers one request, or says why not.
    fn handle(
        &mut self,
        command: PinCommand,
        watcher: Option<&UnixStream>,
    ) -> Result<serde_json::Value> {
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
            PinCommand::Move {
                id,
                x,
                y,
                path,
                annotations,
                ..
            } => {
                let Some(index) = self.stack.pins.iter().position(|pin| pin.id == id) else {
                    return Err(VshotError::Pin(format!(
                        "move names a pin that is not pinned"
                    )));
                };
                // The pixels first: a replacement that cannot be read must not
                // leave the pin half-moved, and the position is the part a
                // retry can afford to repeat.
                if let Some(path) = path {
                    let bytes = std::fs::read(&path).map_err(|source| {
                        VshotError::Pin(format!("cannot read `{}`: {source}", path.display()))
                    })?;
                    let reference_nits = crate::config::hdr_reference_white_default()
                        .unwrap_or(crate::model::hdr::REFERENCE_WHITE_NITS);
                    let picture = picture::decode_bytes(&bytes, reference_nits)?;
                    self.stack.replace(index, picture, &path);
                }
                if let Some(marks) = annotations {
                    self.stack.pins[index].annotations = Some(marks);
                }
                // The clamp is what the pin really landed on, which is not
                // always where it was asked to go: a drag past the edge of
                // every output is pulled back.  The editor is told where it
                // ended up rather than where it asked -- it draws its frame and
                // its toolbar against that rect, and a frame tracking a
                // position the pin never took is a frame beside the picture.
                let clamped = self.stack.clamp_origin(
                    Point::new(x, y),
                    Stack::drawn(
                        self.stack.pins[index].picture.size(),
                        self.stack.pins[index].scale,
                    ),
                );
                self.stack.pins[index].origin = clamped;
                self.refresh()?;
                let pin = &self.stack.pins[index];
                let size = Stack::drawn(pin.picture.size(), pin.scale);
                Ok(json!({
                    "ok": true,
                    "x": pin.origin.x,
                    "y": pin.origin.y,
                    "width": size.width,
                    "height": size.height,
                }))
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
            // Which of this side's outputs are showing HDR.  Read from the
            // surfaces' own picture of them, which is where the colour layer's
            // answer landed: a client asking this is deciding whether to pin a
            // capture's HDR half, and only the side that holds the session can
            // answer.
            PinCommand::Outputs => Ok(json!({
                "ok": true,
                "hdr": self.surfaces.hdr_outputs(),
                "outputs": self.surfaces.surface_count(),
            })),
            // Answered by the caller, which is the loop that has to stop.
            PinCommand::Edit { id, text } => self.start_edit(id, text),
            // A client that wants the live-pin answer joins the list, and is
            // told the answer it has now.  What it does with it is its own:
            // the editor draws a frame while its pin is the live one.
            PinCommand::WatchActive => {
                if let Some(watcher) = watcher {
                    self.watchers.push(watcher.try_clone().ok());
                }
                Ok(json!({"active": self.live_pin()}))
            }
            PinCommand::Raise { id } => {
                let raised = self.stack.raise_by_id(id);
                self.refresh()?;
                self.announce_active();
                Ok(json!({"raised": raised.unwrap_or(0)}))
            }
            PinCommand::Save { id } => self.save(id),
            PinCommand::Quit => Ok(json!({"ok": true})),
            PinCommand::AddClipboard {
                density,
                output_name,
                ..
            } => self.add_clipboard(density, output_name.as_deref()),
        }
    }

    /// Writes one pin out to a file the user names.
    ///
    /// The dialog is a Qt process of its own, like the editor: this daemon is a
    /// layer-shell client and a layer surface cannot parent a popup, so the
    /// window has to be somebody else's.  It answers with a path and a format,
    /// and the writing is this side's -- it is the side holding the pixels, and
    /// the side with the codecs.
    fn save(&mut self, id: u64) -> Result<serde_json::Value> {
        let Some(index) = self.stack.pins.iter().position(|pin| pin.id == id) else {
            return Err(VshotError::Pin(
                "save names a pin that is not pinned".to_string(),
            ));
        };
        let Some(helper) = helper_program() else {
            return Err(VshotError::Pin(
                "cannot locate vshot-qt-ui for the save dialog; set VSHOT_QT_HELPER".into(),
            ));
        };
        let pin = &self.stack.pins[index];
        let size = Stack::drawn(pin.picture.size(), pin.scale);
        let rect = Rect::new(pin.origin.x, pin.origin.y, size.width, size.height);
        // The name the dialog opens with: the file the pin came from, so a
        // re-save lands beside the original, and otherwise a timestamped name
        // in the same shape the CLI writes.
        let suggested = pin
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                format!("vshot-{}.png", chrono::Local::now().format("%Y%m%d-%H%M%S"))
            });
        // The formats the dialog may offer: what this content can be written
        // as, which is the same question the CLI answers for a capture.
        let hdr = pin.picture.as_hdr().is_some();
        let formats = save_format_argument(hdr);
        let output_name = self.surfaces.output_name_at(rect).unwrap_or("").to_string();
        let Some((destination, sdr_copy)) =
            run_save_dialog(&helper, &suggested, &output_name, &formats, hdr)?
        else {
            // The user closed the dialog: nothing was asked for, and nothing is
            // wrong.
            return Ok(json!({"ok": true, "saved": false}));
        };
        self.write_pin(index, &destination, sdr_copy)?;
        Ok(json!({
            "ok": true,
            "saved": true,
            "path": destination.to_string_lossy(),
        }))
    }

    /// Writes one pin's picture out, through the CLI's own export path.
    ///
    /// Not by encoding here: `vshot pin --export` already does exactly this --
    /// it takes the SDR picture and, for a pin that has one, the `VSHTPQ02`
    /// half, and writes them the way a capture writes its own two files, at the
    /// formats and settings the config names.  A second implementation here
    /// would be a second answer to "what does saving a pin produce", and the
    /// two would drift.
    ///
    /// So the picture goes out as the two files that path expects, in a
    /// directory this daemon owns and removes when the write is done.
    fn write_pin(&self, index: usize, destination: &Path, sdr_copy: bool) -> Result<()> {
        let pin = &self.stack.pins[index];
        let directory = tempfile::Builder::new()
            .prefix("vshot-pin-save-")
            .tempdir()
            .map_err(|error| VshotError::Pin(format!("cannot make a save directory: {error}")))?;
        // The SDR picture: the codes as they are for a picture that has no
        // light of its own, and the tone map for one that has.
        let frame = match &pin.picture {
            Picture::Sdr(frame) => frame.clone(),
            Picture::Hdr(image) => image
                .frame
                .tone_map_to_srgb_with(crate::config::tone_map_options())?,
        };
        let png = directory.path().join("pin.png");
        std::fs::write(
            &png,
            frame.encode_png(crate::model::frame::PngCompression::Fast)?,
        )
        .map_err(|source| VshotError::Pin(format!("cannot write the picture to save: {source}")))?;
        let Some(program) = std::env::current_exe().ok() else {
            return Err(VshotError::Pin(
                "cannot find the vshot binary to save with".into(),
            ));
        };
        let mut command = std::process::Command::new(program);
        command
            .arg("pin")
            .arg("--export")
            .arg(&png)
            .arg(destination);
        if let Some(image) = pin.picture.as_hdr() {
            // The half the CLI reads back: the same light, in the container this
            // program hands a pin over in.
            let half = PqPin {
                words: image
                    .frame
                    .to_rgb10_pq_in(image.frame.primaries(), image.white()),
                width: image.frame.size().width,
                height: image.frame.size().height,
                reference_nits: image.white(),
                primaries: image.frame.primaries(),
            };
            let pq = directory.path().join("pin.pq");
            std::fs::write(&pq, half.encode()).map_err(|source| {
                VshotError::Pin(format!("cannot write the HDR half to save: {source}"))
            })?;
            command
                .arg("--hdr-source")
                .arg(&pq)
                .arg("--sdr-copy")
                .arg(if sdr_copy { "true" } else { "false" });
        }
        let status = command
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|source| VshotError::Pin(format!("cannot run the save: {source}")))?;
        if !status.status.success() {
            let reason = String::from_utf8_lossy(&status.stderr).trim().to_string();
            return Err(VshotError::Pin(format!(
                "could not save the picture: {reason}"
            )));
        }
        Ok(())
    }

    /// The whole desktop as the layout places it, for the editor's session.
    ///
    /// The editor's keyboard cursor walks a pointer of its own and asks the CLI
    /// to move the real one there, and a pointer position is expressed in this
    /// space -- the CLI subtracts this origin and scales to this size.  The
    /// session's own `bounds` is the pin, which is not the screen, so the
    /// desktop has to travel separately or a warp on a multi-monitor layout
    /// would land at a fraction of where it belongs.
    fn desktop_json(&self) -> serde_json::Value {
        let placements = self.surfaces.placements();
        let Some(first) = placements.first() else {
            return json!({"x": 0, "y": 0, "width": 0, "height": 0});
        };
        let mut bounds = first.geometry;
        for output in &placements[1..] {
            let other = output.geometry;
            let left = bounds.origin.x.min(other.origin.x);
            let top = bounds.origin.y.min(other.origin.y);
            let right = (bounds.origin.x + bounds.size.width as i32)
                .max(other.origin.x + other.size.width as i32);
            let bottom = (bounds.origin.y + bounds.size.height as i32)
                .max(other.origin.y + other.size.height as i32);
            bounds = Rect::new(left, top, (right - left) as u32, (bottom - top) as u32);
        }
        rect_json(bounds)
    }

    /// Opens the annotation editor on one pin.
    ///
    /// The editor is a Qt process of its own -- it draws a toolbar and a live
    /// annotation layer, which is not something this side has any of -- and it
    /// is handed a session file describing what to open on.  It draws the pin
    /// itself while it is up, so the pin's picture has to be the pristine one
    /// the marks were placed on: handing it the flattened pixels would put the
    /// editor's live marks over a baked copy of themselves.
    ///
    /// The editor writes its result back as a `Move` over this same socket,
    /// which is what `Move`'s `path` and `annotations` are for.
    fn start_edit(&mut self, id: u64, text: bool) -> Result<serde_json::Value> {
        if std::env::var_os("VSHOT_PIN_DEBUG").is_some() {
            eprintln!(
                "vshot-pin: start_edit({id}): editing={} pins={}",
                self.editing.is_some(),
                self.stack.pins.len()
            );
        }
        let Some(index) = self.stack.pins.iter().position(|pin| pin.id == id) else {
            return Err(VshotError::Pin(
                "edit names a pin that is not pinned".to_string(),
            ));
        };
        // One session at a time: two editors on one stack would each be drawing
        // marks the other knows nothing about.
        if self.editing.is_some() {
            return Err(VshotError::Pin("an edit is already open".into()));
        }
        let Some(helper) = helper_program() else {
            return Err(VshotError::Pin(
                "cannot locate vshot-qt-ui for the pin editor; set VSHOT_QT_HELPER".into(),
            ));
        };
        let pin = &self.stack.pins[index];
        let size = Stack::drawn(pin.picture.size(), pin.scale);
        let rect = Rect::new(pin.origin.x, pin.origin.y, size.width, size.height);
        // The picture the editor draws on: the pin's own, written where the
        // editor can read it.  It is a private file in a directory this daemon
        // owns, so nothing else has to be able to name it.
        let directory = tempfile::Builder::new()
            .prefix("vshot-pin-edit-")
            .tempdir()
            .map_err(|error| {
                VshotError::Pin(format!("cannot make a session directory: {error}"))
            })?;
        // The editor's session names a *raw* file -- RGBA8, four bytes a
        // pixel, no header -- because that is what a capture session hands it
        // through shared memory.  So the picture is written as the bytes the
        // editor reads rather than as a PNG it would have to decode: it is the
        // one format both sides already agree on.
        //
        // An HDR picture is flattened to the SDR view for this, which is what
        // the editor can show and draw on.  The light itself is kept beside it
        // in the pin, and a mark placed here is composited back into it when
        // the editor's result comes home -- so annotating an HDR pin does not
        // throw the light away.
        let image = directory.path().join("pin.rgba");
        let frame = match &pin.picture {
            Picture::Sdr(frame) => frame.clone(),
            Picture::Hdr(hdr) => hdr
                .frame
                .tone_map_to_srgb_with(crate::config::tone_map_options())?,
        };
        std::fs::write(&image, frame.pixels()).map_err(|source| {
            VshotError::Pin(format!("cannot write the editor's picture: {source}"))
        })?;

        let session = json!({
            "version": 1,
            "mode": "pin-edit",
            // Absent for the Space-key edit, which opens the ordinary
            // annotation editor; only the menu's `Recognize text…` asks for
            // the text mode.
            "action": if text { Some("text") } else { None },
            "id": id,
            "bounds": rect_json(rect),
            // How wide this pin's rim is drawn, so the editor can treat the
            // stroke as part of the pin: it is centred on the image's edge and
            // reaches half this far outside it, and a drag starting there
            // should move the pin rather than read as a click on the canvas.
            "border_width": self.stack.border_width,
            // The editor drives the real pin over this socket rather than
            // rendering a second copy of the picture.
            "socket": self.socket.to_string_lossy(),
            // The marks the last edit left, so the editor opens on them and
            // they stay editable.  Absent on a pin that has never been
            // annotated, which is what makes a first edit open blank.
            "annotations": pin.annotations.clone(),
            "outputs": [{
                "id": 0,
                "name": self.surfaces.output_name_at(rect).unwrap_or(""),
                "x": rect.origin.x, "y": rect.origin.y,
                "width": rect.size.width, "height": rect.size.height,
                "surface": {
                    "x": rect.origin.x, "y": rect.origin.y,
                    "width": rect.size.width, "height": rect.size.height,
                },
                "scale": 1,
                "pixel_width": pin.picture.size().width,
                "pixel_height": pin.picture.size().height,
                "path": image.to_string_lossy(),
            }],
            "desktop": self.desktop_json(),
        });
        let session_path = directory.path().join("session.json");
        std::fs::write(&session_path, session.to_string())
            .map_err(|source| VshotError::Pin(format!("cannot write the session: {source}")))?;

        let child = std::process::Command::new(&helper)
            .arg("--pin-edit")
            .arg(&session_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn();
        if child.is_err() {
            return Err(VshotError::Pin(
                "the pin editor could not be started".into(),
            ));
        }
        // The directory has to outlive the editor, and nothing here waits for
        // it: the editor writes its result back over the socket, and the
        // session's files are the editor's while it runs.  Kept so the daemon
        // can drop them when the session ends.
        self.editing = Some(EditSession {
            id,
            _directory: directory,
        });
        Ok(json!({"ok": true}))
    }

    /// Pins whatever the clipboard holds as an image.
    ///
    /// Only the image: a copied *file* is resolved to a path by the CLI, which
    /// has the same `wl-paste` and can hand this side something to decode, and
    /// text and colours become cards, which are Qt's to render.  So what is
    /// left here is bytes in one of the image types, which is the one case the
    /// CLI cannot resolve for itself -- there is no path to name.
    ///
    /// The clipboard is read with `wl-paste` rather than through anything of
    /// this program's: it is the one reader that works whatever the compositor
    /// offers, and the capture side already writes it that way.
    fn add_clipboard(
        &mut self,
        density: Option<u32>,
        output_name: Option<&str>,
    ) -> Result<serde_json::Value> {
        let Some(listed) = crate::pin::paste(&["--list-types"]) else {
            return Err(VshotError::Pin(
                "`wl-paste` was not found, so the clipboard cannot be read; it comes from the \
                 wl-clipboard package"
                    .into(),
            ));
        };
        let listed = String::from_utf8_lossy(&listed);
        let Some(wanted) = clipboard_image_type(&listed) else {
            return Err(VshotError::Pin(
                "the clipboard holds no image this build can read".into(),
            ));
        };
        let Some(bytes) = crate::pin::paste(&["--type", wanted, "--no-newline"]) else {
            return Err(VshotError::Pin(format!(
                "the clipboard offers {wanted} but would not hand it over"
            )));
        };
        self.add_bytes(&bytes, density, output_name, None, None)
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
        self.add_bytes(&bytes, density, output_name, Some(path), at)
    }

    /// The rest of an add, once the bytes are in hand: decode, size, place.
    ///
    /// Shared by the two ways a pin arrives -- a file the user named, and bytes
    /// off the clipboard -- because everything after the read is the same
    /// question, and a second copy of the sizing would be a second answer to
    /// it.
    fn add_bytes(
        &mut self,
        bytes: &[u8],
        density: Option<u32>,
        output_name: Option<&str>,
        source: Option<&Path>,
        at: Option<crate::pin::WirePoint>,
    ) -> Result<serde_json::Value> {
        let reference_nits = crate::config::hdr_reference_white_default()
            .unwrap_or(crate::model::hdr::REFERENCE_WHITE_NITS);
        let picture = picture::decode_bytes(bytes, reference_nits)?;
        let size = picture.size();
        let placements = self.surfaces.placements();
        let target = self
            .surfaces
            .placement_of(output_name)
            .or_else(|| placements.first().copied());
        let (density, decided) = match target {
            Some(target) => resolve_density(
                density,
                picture::declared_scale(bytes),
                source.and_then(|path| recorded_density(path, &source_record_path())),
                size,
                target,
                &placements,
            ),
            // No output at all: nothing to size it against, so the file's own
            // word is the only one there is.
            None => (
                density
                    .or_else(|| picture::declared_scale(bytes))
                    .unwrap_or(1),
                "the request",
            ),
        };
        if std::env::var_os("VSHOT_PIN_DEBUG").is_some() {
            eprintln!("vshot-pin: density {density} from {decided}");
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
            path: source.map(Path::to_path_buf).unwrap_or_default(),
            origin,
            scale,
            density,
            visible,
            annotations: None,
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
    let mut daemon = Daemon::new(socket)?;
    daemon.surfaces.set_pin_input(true);
    // Every client that is still connected.  A connection is kept for as long
    // as the client wants it -- the editor holds one open for the live-pin
    // answers, and a drag pipelines its positions down another -- so they are
    // all held here and polled together.  Serving one to completion is what
    // let an open editor stop the daemon from looking at anything else.
    let mut clients: Vec<Client> = Vec::new();
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
        let mut watching: Vec<OwnedFd> = Vec::new();
        watching.push(
            listener.as_fd().try_clone_to_owned().map_err(|source| {
                VshotError::Pin(format!("cannot watch the pin socket: {source}"))
            })?,
        );
        for client in &clients {
            if let Ok(fd) = client.stream.as_fd().try_clone_to_owned() {
                watching.push(fd);
            }
        }
        let chrome = daemon.chrome.fd();
        let fds: Vec<BorrowedFd<'_>> = watching
            .iter()
            .map(AsFd::as_fd)
            .chain(chrome.as_ref().map(AsFd::as_fd))
            .collect();
        match daemon.idle_at() {
            // Nothing pinned: wait out the deadline and give up when it passes.
            // Nothing the compositor or the socket says announces an empty
            // stack, so the clock is the only thing that can.
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(left) => daemon.surfaces.wait_on_many(&fds, Some(left))?,
                None => {
                    if debug {
                        eprintln!("vshot-pin: nothing pinned; giving up");
                    }
                    return Ok(());
                }
            },
            None => daemon.surfaces.wait_on_many(&fds, None)?,
        }
        // Everything every client has said, answered on the spot.  A client
        // that has gone away is dropped, and a `quit` stops the daemon.
        let mut live: Vec<Client> = Vec::with_capacity(clients.len());
        let mut dropped_watcher = false;
        for mut client in clients {
            match serve_client(&mut daemon, &mut client, debug) {
                Ok(true) => live.push(client),
                Ok(false) => {
                    // The editor holds the connection it asked to watch on for
                    // as long as its session lasts, so a watcher that has gone
                    // is an edit that has finished.
                    dropped_watcher |= client.watcher;
                }
                Err(error) => {
                    if debug {
                        eprintln!("vshot-pin: {error}");
                    }
                }
            }
        }
        clients = live;
        if dropped_watcher {
            daemon.end_edit();
        }
        // Whatever the chrome said back -- a menu row picked, a menu dismissed
        // -- before anything else looks at the stack.
        daemon.chrome_replies()?;
        // Whatever the compositor had queued, before anything else looks at the
        // stack: a drag is a stream of these, and each one is a repaint.
        daemon.gestures()?;
        loop {
            match listener.accept() {
                Ok((stream, _)) => match Client::new(stream) {
                    Ok(client) => clients.push(client),
                    Err(error) => {
                        if debug {
                            eprintln!("vshot-pin: {error}");
                        }
                    }
                },
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

/// One connected client: the socket, what has arrived but not been read yet,
/// and whether it asked to be told which pin is live.
struct Client {
    stream: UnixStream,
    pending: Vec<u8>,
    /// Whether this client asked to be told which pin is live.  Only the pin
    /// editor does, and it keeps the connection open for as long as its session
    /// lasts -- so a watcher that has gone is an edit that has finished.
    watcher: bool,
}

impl Client {
    fn new(stream: UnixStream) -> Result<Self> {
        stream.set_nonblocking(true).map_err(|source| {
            VshotError::Pin(format!("cannot make the connection non-blocking: {source}"))
        })?;
        Ok(Self {
            stream,
            pending: Vec::new(),
            watcher: false,
        })
    }
}

/// Reads whatever `client` has said and answers every complete line.
///
/// Answers `true` while the client is still connected, `false` when it has gone
/// away, and `Ok(true)` is also what a `quit` command means for the daemon --
/// the caller tells them apart by the command, not by the return.
///
/// Non-blocking, and it never waits: the daemon has other clients and the
/// compositor to serve, and a connection that is kept for later -- the editor's,
/// which stays open for the live-pin answers -- must not hold the loop.  Serving
/// one connection to completion is what made an open editor stop the daemon
/// from seeing anything else at all, including the moves the editor sent.
fn serve_client(daemon: &mut Daemon, client: &mut Client, debug: bool) -> Result<bool> {
    let mut buffer = [0u8; 8192];
    loop {
        match (&client.stream).read(&mut buffer) {
            Ok(0) => return Ok(false), // the client went away
            Ok(count) => client.pending.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return Ok(false),
        }
    }
    while let Some(newline) = client.pending.iter().position(|byte| *byte == b'\n') {
        let line = client.pending.drain(..=newline).collect::<Vec<u8>>();
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let command: PinCommand = match serde_json::from_str(text.trim()) {
            Ok(command) => command,
            Err(error) => {
                // A command this build does not know is answered rather than
                // fatal: the editor and the daemon can be different builds, and
                // a client that asked for something new should hear that it was
                // not understood.
                let reply =
                    json!({"ok": false, "error": format!("cannot read the request: {error}")});
                write_reply(&client.stream, &reply)?;
                continue;
            }
        };
        // Whether to stop is decided from the command, not from the reply: the
        // reply has to go out first.
        if matches!(command, PinCommand::Quit) {
            write_reply(&client.stream, &json!({"ok": true}))?;
            return Ok(false);
        }
        if matches!(command, PinCommand::WatchActive) {
            client.watcher = true;
        }
        let reply = match daemon.handle(command, Some(&client.stream)) {
            Ok(reply) => reply,
            Err(error) => {
                if debug {
                    eprintln!("vshot-pin: {error}");
                }
                json!({"ok": false, "error": error.to_string()})
            }
        };
        write_reply(&client.stream, &reply)?;
    }
    Ok(true)
}

/// Writes one reply, followed by the newline that ends it.
fn write_reply(stream: &UnixStream, reply: &serde_json::Value) -> Result<()> {
    let mut encoded = serde_json::to_vec(reply)
        .map_err(|error| VshotError::Pin(format!("cannot encode the reply: {error}")))?;
    encoded.push(b'\n');
    (&*stream)
        .write_all(&encoded)
        .map_err(|source| VshotError::Pin(format!("cannot answer: {source}")))?;
    let _ = (&*stream).flush();
    Ok(())
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
        let mut stack = Stack::new(vec![output(1920, 1080, 1)], 2);
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
                density: 1,
                visible: true,
                annotations: None,
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
    /// own centre, and stops at the ends of the range.  A notch towards the
    /// user is the one that makes it bigger.
    ///
    /// The pin sits in the middle of the output on purpose: one against an edge
    /// is held there by the grab margin, so a wheel at the corner moves it as
    /// well as resizing it, and that is a different test.
    #[test]
    fn the_wheel_scales_about_the_pins_centre() {
        let mut stack = stack_of(1);
        stack.pins[0].origin = Point::new(910, 515);
        // Positive is the direction Wayland calls away from the user, and a
        // wheel away from the user zooms *out* -- the way every viewer behaves.
        let change = stack.scroll(1, Point::new(960, 540));
        assert!(change.redraw);
        assert!((stack.pins[0].scale - 1.0 / ZOOM_PER_NOTCH).abs() < 1e-9);

        // One notch the other way and it is bigger than it started.
        stack.scroll(-1, Point::new(960, 540));
        assert!((stack.pins[0].scale - 1.0).abs() < 1e-9);

        // The ends of the range hold.
        for _ in 0..300 {
            stack.scroll(-1, Point::new(960, 540));
        }
        assert_eq!(stack.pins[0].scale, MAX_SCALE);
        for _ in 0..400 {
            stack.scroll(1, Point::new(960, 540));
        }
        assert_eq!(stack.pins[0].scale, MIN_SCALE);
    }

    /// A write-back replaces the pixels and nothing else: where the pin is and
    /// how big it is drawn are the user's, and a replacement that reset them
    /// would undo the drag they did before opening the editor.
    #[test]
    fn replacing_a_picture_leaves_the_rest_of_the_pin_alone() {
        let mut stack = stack_of(1);
        stack.pins[0].origin = Point::new(40, 50);
        stack.pins[0].scale = 0.5;
        stack.pins[0].annotations = Some(serde_json::json!([{"kind": "rect"}]));
        let replacement = Picture::Sdr(
            crate::model::Frame::solid(Size::new(8, 4), [1, 2, 3, 255]).expect("frame"),
        );
        stack.replace(0, replacement, Path::new("/tmp/edited.png"));

        assert_eq!(stack.pins[0].picture.size(), Size::new(8, 4));
        assert_eq!(stack.pins[0].path, PathBuf::from("/tmp/edited.png"));
        assert_eq!(stack.pins[0].origin, Point::new(40, 50));
        assert_eq!(stack.pins[0].scale, 0.5);
        // The marks described the old pixels, and a picture that arrives
        // without any of its own has none to keep.
        assert!(stack.pins[0].annotations.is_none());
    }

    /// A menu row is a number, and a number out of range is nothing to do --
    /// not an error: the menu may have been drawn by an older daemon, and a
    /// pick the daemon does not understand must not take it down.
    #[test]
    fn every_menu_row_names_an_action() {
        // The six rows the daemon offers, and the ones the Qt surface draws,
        // are one list: a menu that called them something else would be a
        // second vocabulary for one set of commands.
        // The chrome's own labels are its business; what has to agree is the
        // *order*, because a pick comes back as a row number.
        assert_eq!(MENU_ACTION_ROWS, 6);
        assert_eq!(MENU_ACTIONS[0], "Copy image");
        assert_eq!(MENU_ACTIONS[5], "Close");
        // `Close` last: the row that throws the pin away sits at the bottom of
        // a list where every other row is reversible.
        assert!(MENU_ACTIONS.contains(&"Save as…"));
        assert!(MENU_ACTIONS.contains(&"Edit"));
        assert!(MENU_ACTIONS.contains(&"Reset zoom"));
        assert!(MENU_ACTIONS.contains(&"Recognize text…"));
    }

    /// A right-click on a pin opens its menu; on the desktop it closes whatever
    /// menu is up, which is what a click outside a menu does everywhere else.
    #[test]
    fn the_menu_opens_on_the_pin_under_the_pointer() {
        let stack = stack_of(2);
        assert_eq!(stack.menu_target(Point::new(10, 10)), Some(1));
        assert_eq!(stack.menu_target(Point::new(210, 10)), Some(2));
        assert_eq!(stack.menu_target(Point::new(5000, 5000)), None);
    }

    /// The save dialog is offered what this build can write, built from the
    /// registry rather than from a list kept here: a dialog with names of its
    /// own would eventually offer one the binary cannot write.
    #[test]
    fn the_save_dialog_is_offered_the_formats_this_build_has() {
        let sdr = save_format_argument(false);
        // Every entry is `name/suffix/...`, and every format that has a suffix
        // is in there.
        for entry in sdr.split(',') {
            let mut parts = entry.split('/');
            assert!(parts.next().is_some_and(|name| !name.is_empty()), "{entry}");
            assert!(
                parts.next().is_some_and(|suffix| !suffix.is_empty()),
                "{entry}"
            );
        }
        assert!(sdr.starts_with("png/png"), "{sdr}");
        assert!(sdr.contains("jpeg/jpg/jpeg"), "{sdr}");
        // Every format in the registry is in the list, spelled one way or two.
        for name in crate::model::codec::sdr_names() {
            assert!(
                sdr.contains(&format!("{name}/")),
                "{name} missing from {sdr}"
            );
        }
        // The HDR half is a different list, and it is not the SDR one.
        let hdr = save_format_argument(true);
        assert!(!hdr.contains("png/"), "{hdr}");
        assert!(
            hdr.contains("jxl/jxl") || hdr.contains("avif/avif"),
            "{hdr}"
        );
    }

    /// The clipboard's own list decides which type is read, and the order is
    /// not arbitrary: the same picture is usually on offer as several, and PNG
    /// is the one that has not been through a lossy encoder.
    #[test]
    fn the_best_image_type_the_clipboard_offers_is_the_one_read() {
        let listed = "text/plain\nimage/jpeg\nimage/png\n";
        assert_eq!(clipboard_image_type(listed), Some("image/png"));
        // JPEG when that is all there is, rather than nothing.
        assert_eq!(
            clipboard_image_type("text/plain\nimage/jpeg\n"),
            Some("image/jpeg")
        );
        // Anything under `image/` beats refusing: the decoder names what it
        // cannot read, which is more use than a guess made here.
        assert_eq!(clipboard_image_type("image/avif\n"), Some("image/avif"));
        // Text and files are the CLI's to resolve, and this says so.
        assert_eq!(clipboard_image_type("text/plain\ntext/uri-list\n"), None);
        assert_eq!(clipboard_image_type(""), None);
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
