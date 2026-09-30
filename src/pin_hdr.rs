// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! The HDR pin surfaces: a resident helper that shows pinned HDR images on the
//! compositor.
//!
//! A pinned image is drawn by the Qt pin daemon, which is an SDR surface and
//! cannot be anything else: a window's surface description comes from the
//! compositor, Qt builds one from the window's colour space — named BT.2020 with
//! no luminances — and the compositor only passes a surface through untouched
//! when its description is *the output's own*.  Anything else is converted and,
//! for light beyond SDR white, tone-mapped; measured here, a ramp meant to run
//! from 50 to 1000 cd/m² came out as 40 to 188 cd/m², below SDR white.
//!
//! So the HDR half of a pinned capture is shown by a surface of our own: one
//! layer surface per output, holding a half-float (`ABGR16161616F`) dma-buf whose
//! description is the output's own — a passthrough, so the pixels reach the panel
//! as the light they stand for.
//!
//! The picture, its shadow and its rim are drawn into that one buffer, in one
//! commit, by [`crate::pin_hdr_fp16`].  That is the whole reason for a half-float
//! buffer: a ten-bit `wl_shm` buffer carries two bits of alpha, which cannot
//! express a soft shadow, and a picture whose rim lived on a *different* surface
//! would trail its own edge while it was dragged.
//!
//! The Qt daemon keeps what is left: the hit testing, the badges, the menus, the
//! zoom, and the `HDR` tag under the pointer.  It hands the pixels and the places
//! over a socket.
//!
//! The protocol is one JSON object per line, the daemon driving:
//!
//! ```text
//! {"cmd":"pins","style":{"radius":12,"shadow":{"size":14,"offset":3,"opacity":120},
//!  "border":{"width":2,"color":[192,192,192],"active":[255,96,96]}},
//!  "pins":[{"id":1,"path":"/dev/shm/x.pq","x":100,"y":200,"scale":1.0,
//!           "visible":true,"active":false,"white":203.0}]}
//! {"cmd":"quit"}
//! ```
//!
//! and this side answering one line per command, plus one `mapped` event as soon
//! as its surfaces are up — which the daemon waits for, because the compositor
//! stacks one layer's surfaces in the order they were mapped and the chrome has
//! to come after the picture.
//!
//! Only the region the pins have actually moved through is composed and
//! committed: the stack a drag sends once per motion event differs from the last
//! one by a few pixels, not by an output.  The helper also waits on its socket
//! and on the compositor at the same time, so a position is picked up the moment
//! it is written and a buffer coming back lets a refused compose go out — no
//! timer in between.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;

use crate::error::{Result, VshotError};
use crate::geometry::{Point, Rect, Size};
use crate::pin_hdr_fp16::{self as fp16, Buffer, Surface};
use crate::wayland::{PinBufferSpec, WaylandSession};

/// How long the helper waits for the daemon to connect before giving up, so a
/// daemon that died between spawning and connecting leaves nothing behind.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many picture buffers one output holds: two, so one can be with the
/// compositor while the next is drawn.  It has to match the shim's own count.
const PIN_SLOTS: usize = 2;

/// The masks the shim keeps are named `MASK_NAMESPACE + pin id`, far away from
/// any pin id, so dropping a pin's picture never drops its shadow by mistake.
const MASK_NAMESPACE: u64 = 1 << 63;

/// One pinned HDR image: its pixels as PQ-encoded words, and where the daemon
/// wants them, in global logical pixels.
#[derive(Clone)]
struct Pin {
    /// The file the words were read from, so a pin whose pixels did not change
    /// is not read again on every update.
    path: PathBuf,
    /// Shared, because every update rebuilds the stack and a drag sends one per
    /// motion event: copying the pixels of an output-sized pin each time would
    /// cost far more than composing it did.
    words: Arc<Vec<u32>>,
    width: u32,
    height: u32,
    origin: Point,
    /// Logical pixels per source pixel.
    scale: f64,
    visible: bool,
    /// Whether the keyboard would act on this pin, which is what picks the rim's
    /// colour.
    active: bool,
    /// The light the pin's `1.0` stands for, from the capture that made it.  The
    /// rim is a colour from the config, in sRGB, and this is what turns it into
    /// the PQ code the surface wants: that colour shown at SDR white.
    white: f32,
}

/// The magic a PQ file starts with, so a file that is not one is refused
/// instead of drawn as noise.
const PQ_MAGIC: &[u8; 8] = b"VSHTPQ01";

/// One pinned HDR image as it travels: what the header names and the words it
/// carries.
pub(crate) struct PqImage {
    pub width: u32,
    pub height: u32,
    /// The light a code of `1.0` stands for.  The helper does not strictly need
    /// it — it writes the codes onto a surface already described by the output —
    /// but the pin editor decodes the same file back to linear light and needs
    /// it.
    pub reference_nits: f32,
    /// The gamut the codes are written in: the output the pin was taken from,
    /// not BT.2020 unless that output really was BT.2020.  The editor decodes
    /// the file back to linear light with this, and a reader that ignored it
    /// would read a wide-gamut pin as if it were BT.2020 and shift its colours.
    pub primaries: Primaries,
    pub words: Vec<u32>,
}

/// Reads one PQ file: the magic, the size, the white, the gamut, and
/// `width * height` ARGB2101010 words in little-endian order — the layout
/// `HdrFrame::to_rgb10_pq_in` produces, as `pin::PqPin::encode` writes it.
pub(crate) fn read_pq_file(path: &Path) -> Result<PqImage> {
    let bytes = std::fs::read(path).map_err(|source| VshotError::HdrPin {
        path: path.to_path_buf(),
        reason: format!("cannot read the pinned HDR image: {source}"),
    })?;
    if bytes.len() < PQ_HEADER || &bytes[..8] != PQ_MAGIC {
        // The magic names the layout, so a file carrying another one is a pin
        // written by a VShot whose format this build does not know -- an older
        // helper against a newer daemon, which is what a stale binary beside a
        // fresh one produces.  Naming both magics is what tells the two apart
        // from a truncated or foreign file.
        let found = if bytes.len() >= 8 {
            String::from_utf8_lossy(&bytes[..8]).into_owned()
        } else {
            format!("{} bytes", bytes.len())
        };
        return Err(VshotError::HdrPin {
            path: path.to_path_buf(),
            reason: format!(
                "not a vshot HDR pin image: it starts with `{found}` where this build writes \
                 `{}`; the surface helper and the pin daemon are different builds",
                String::from_utf8_lossy(PQ_MAGIC)
            ),
        });
    }
    let width = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let height = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let reference_nits = f32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let primaries = read_primaries(&bytes[20..PQ_HEADER]);
    let count =
        usize::try_from(u64::from(width) * u64::from(height)).map_err(|_| VshotError::HdrPin {
            path: path.to_path_buf(),
            reason: "image size is out of range".into(),
        })?;
    let expected = count
        .checked_mul(4)
        .and_then(|bytes| bytes.checked_add(PQ_HEADER))
        .ok_or_else(|| VshotError::HdrPin {
            path: path.to_path_buf(),
            reason: "image size is out of range".into(),
        })?;
    if bytes.len() != expected {
        return Err(VshotError::HdrPin {
            path: path.to_path_buf(),
            reason: format!(
                "image says {width}x{height} but carries {} bytes",
                bytes.len()
            ),
        });
    }
    let words = bytes[PQ_HEADER..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word))
        .collect();
    Ok(PqImage {
        width,
        height,
        reference_nits,
        words,
    })
}

/// How every pin on this output is drawn: one style for the whole stack, the way
/// the Qt side hands one to every surface.
#[derive(Clone, Debug, Default, PartialEq)]
struct Style {
    /// Corner radius in logical pixels.
    radius: i32,
    /// The soft shadow behind every pin, or none.
    shadow: Option<Shadow>,
    /// The stroke around every pin, or none.
    border: Option<Border>,
}

/// A soft shadow, in the same terms `ui/shadow.cpp` describes one.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Shadow {
    /// How far the blur reaches past the shape, in logical pixels.
    size: i32,
    /// How far the shape is dropped below its own rect, in logical pixels.
    offset: i32,
    /// Alpha of the silhouette before it is softened, 0-255.
    opacity: i32,
}

impl Shadow {
    /// How far past its own rect this shadow reaches on every side, in logical
    /// pixels.  Zero when it is off, which is what the damage region grows by.
    fn band(&self) -> i32 {
        if self.opacity <= 0 || self.size <= 0 {
            0
        } else {
            self.size + self.offset.abs() + 1
        }
    }
}

/// The stroke around a pin: how wide, and in which colour normally and while the
/// pin is the one the keyboard would act on.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Border {
    width: i32,
    colour: [u8; 3],
    active: [u8; 3],
}

/// Where one pin is drawn on one output, in that output's device pixels: the
/// top-left corner it starts at and the size it is drawn at.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    left: i64,
    top: i64,
    width: i64,
    height: i64,
}

/// How a pin lands on one output.  A pin is scaled to the display in *logical*
/// pixels and the output then scales that to its own device pixels, so both
/// factors apply; a pin that is not drawn at all has no placement.
fn placement(pin: &Pin, output: Rect, scale: u32) -> Option<Placement> {
    if pin.width == 0 || pin.height == 0 {
        return None;
    }
    let step = pin.scale * f64::from(scale);
    if step <= 0.0 {
        return None;
    }
    let device_scale = f64::from(scale);
    Some(Placement {
        left: ((i64::from(pin.origin.x - output.origin.x)) as f64 * device_scale).round() as i64,
        top: ((i64::from(pin.origin.y - output.origin.y)) as f64 * device_scale).round() as i64,
        width: ((f64::from(pin.width) * step).round() as i64).max(1),
        height: ((f64::from(pin.height) * step).round() as i64).max(1),
    })
}

/// Everything one pin paints on one output, in that output's device pixels,
/// clipped to it — the picture, the shadow that reaches past it, and the outer
/// half of its rim.
///
/// This is the region a move has to be composed over, and it is deliberately
/// larger than the picture: a shadow left out of the damage region would stay
/// behind after the pin moved, and a rim left out would keep the last frame's
/// colour.
fn pin_damage_rect(pin: &Pin, output: &OutputRect, style: &Style) -> Option<Rect> {
    if !pin.visible {
        return None;
    }
    let placed = placement(pin, output.geometry, output.scale)?;
    let mut band = style.shadow.as_ref().map(Shadow::band).unwrap_or(0);
    if let Some(border) = &style.border {
        band = band.max(border.width / 2 + 1);
    }
    let band = (f64::from(band) * f64::from(output.scale)).ceil() as i64;
    let left = placed.left.checked_sub(band)?;
    let top = placed.top.checked_sub(band)?;
    let width = placed.width.checked_add(band.checked_mul(2)?)?;
    let height = placed.height.checked_add(band.checked_mul(2)?)?;
    Rect::new(
        i32::try_from(left).ok()?,
        i32::try_from(top).ok()?,
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    )
    .clamp_to(Rect::new(
        0,
        0,
        output.pixel_size.width,
        output.pixel_size.height,
    ))
}

/// The PQ code one sRGB channel stands for, when a code of `1.0` is that
/// channel's own light at this output's white.
///
/// The rim is a colour from the config, in the 8-bit sRGB the Qt dialog draws it
/// in, so this is that colour said in the terms the surface is described in.
/// The same colour on an SDR surface and on this one then puts the same light on
/// the panel.
fn pq_code(channel: u8, white_nits: f32) -> f32 {
    let value = f32::from(channel) / 255.0;
    let linear = if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    };
    crate::model::hdr::pq_encode(linear * white_nits / 10_000.0)
}

/// Everything a shadow mask's appearance depends on, in device pixels.
type MaskKey = (i32, i32, i32, i32, i32, i32);

/// One output's picture surface and what it is holding: the shim's context, the
/// buffers it draws into, and the per-pin caches that keep a drag from
/// re-uploading anything.
struct PinTarget {
    surface: Surface,
    buffers: Vec<Buffer>,
    /// The file each pin's pixels were uploaded from.  A pin whose file changed
    /// is uploaded again; one whose file did not is left alone.
    images: HashMap<u64, PathBuf>,
    /// The shape each pin's shadow mask was built for.  A pin whose shape
    /// changed has its mask built again.
    masks: HashMap<u64, MaskKey>,
    width: u32,
    height: u32,
}

impl Drop for PinTarget {
    fn drop(&mut self) {
        for buffer in &self.buffers {
            self.surface.free_buffer(buffer);
        }
    }
}

/// One output's picture and how far the compositor has been told about it.
#[derive(Default)]
struct Stage {
    /// The rectangles the pins covered in the last commit the compositor took,
    /// so the pixels a pin has left behind are cleared by the next one.
    painted: Vec<Rect>,
    /// What is waiting for a buffer: the rectangles the pins cover now, and the
    /// region the next commit has to cover to make the compositor agree with the
    /// compose texture.  `None` when the screen already shows the picture here.
    waiting: Option<(Vec<Rect>, Rect)>,
}

/// The helper's wayland side: the session, the outputs that got a colour
/// description, and the picture each of those is showing.
struct Surfaces {
    session: WaylandSession,
    /// Outputs whose surface carries its own description.  A pin on any other
    /// output cannot be shown as HDR, and is left to the Qt daemon's SDR copy.
    hdr_outputs: Vec<String>,
    targets: HashMap<String, PinTarget>,
    stages: HashMap<String, Stage>,
    outputs: Vec<OutputRect>,
    style: Style,
    /// The stack the daemon last handed over, so a compose that found no buffer
    /// can be offered again without the daemon saying anything.
    last_pins: Vec<(u64, Pin)>,
}

/// One output's logical rectangle, as this side sees it.
#[derive(Clone, Debug)]
struct OutputRect {
    name: String,
    geometry: Rect,
    scale: u32,
    pixel_size: Size,
}

impl Surfaces {
    fn new() -> Result<Self> {
        let session = WaylandSession::connect()?;
        Ok(Self {
            session,
            hdr_outputs: Vec::new(),
            targets: HashMap::new(),
            stages: HashMap::new(),
            outputs: Vec::new(),
            style: Style::default(),
            last_pins: Vec::new(),
        })
    }

    /// Puts up one Overlay-layer surface per output, gives each its output's
    /// description — which is what makes a PQ buffer a passthrough rather than
    /// something to tone-map — and hands it the half-float buffers the pictures
    /// are drawn into.
    fn start(&mut self) -> Result<()> {
        let debug = std::env::var_os("VSHOT_PIN_DEBUG").is_some();
        let candidates = self
            .session
            .show_pin_surfaces(zwlr_layer_shell_v1::Layer::Overlay, "vshot-pin-hdr")?;
        // No output can carry a description, so there is no HDR half to show
        // anywhere: the helper stays up with nothing of its own, and every pin
        // is the Qt surface's SDR one.  That is what a compositor with no colour
        // management or no half-float dma-buf looks like, and it is not a
        // failure -- there is simply nothing here to draw.
        if candidates.is_empty() {
            return Ok(());
        }
        self.outputs = self
            .session
            .output_infos()?
            .into_iter()
            .map(|info| OutputRect {
                name: info.name,
                geometry: info.geometry,
                scale: info.scale,
                pixel_size: info.pixel_size,
            })
            .collect();
        // Only the outputs whose own description came back ready are worth a
        // buffer: on any other, a half-float picture would be read as sRGB and
        // shown far darker than the screen.
        let modifiers = self
            .session
            .dmabuf_modifier_order(fp16::FORMAT_ABGR16161616F)
            .unwrap_or_default();
        let mut node: Option<PathBuf> = None;
        let mut installed = 0usize;
        for name in &candidates {
            let Some(output) = self
                .outputs
                .iter()
                .find(|output| output.name == *name)
                .cloned()
            else {
                continue;
            };
            match PinTarget::open(&mut self.session, &output, &modifiers, &mut node, debug) {
                Ok(target) => {
                    self.targets.insert(name.clone(), target);
                    installed += 1;
                }
                Err(error) => {
                    if debug {
                        eprintln!("vshot: pin-hdr: {name}: {error}");
                    }
                }
            }
        }
        if installed == 0 {
            return Err(VshotError::PinSurface(
                "no output could be given a half-float picture surface".into(),
            ));
        }
        // The descriptions go on now that the buffers are in place: a surface's
        // description is applied on a commit, and the commit that attaches the
        // first buffer is the one that maps it.
        let colored = self.session.apply_pin_color()?;
        self.hdr_outputs = colored
            .into_iter()
            .filter(|name| self.targets.contains_key(name))
            .collect();
        Ok(())
    }

    /// Brings every output's picture up to date and asks the session to show it.
    ///
    /// Only the region a pin was in and the region it is in now are composed and
    /// committed.  A pinned image is an output-sized surface with one picture
    /// drawn into it, so drawing all of it on every motion event is what would
    /// make a drag trail the pointer.
    fn show(&mut self) {
        if self.hdr_outputs.is_empty() {
            return;
        }
        let pins = self.last_pins.clone();
        let names = self
            .outputs
            .iter()
            .map(|output| output.name.clone())
            .collect::<Vec<_>>();
        for name in names {
            if !self.hdr_outputs.contains(&name) {
                continue;
            }
            let Some(output) = self
                .outputs
                .iter()
                .find(|output| output.name == name)
                .cloned()
            else {
                continue;
            };
            let bounds = Rect::new(0, 0, output.pixel_size.width, output.pixel_size.height);
            let now = pins
                .iter()
                .filter_map(|(_, pin)| pin_damage_rect(pin, &output, &self.style))
                .collect::<Vec<Rect>>();
            let stage = self.stages.entry(name.clone()).or_default();
            // Everything that could differ between the compose texture and the
            // pixels on the screen: what the pins covered when the compositor
            // last took a buffer, what they cover now, and anything a commit that
            // has not landed yet was going to change.
            let mut damage = Rect::default();
            for rect in stage.painted.iter().chain(&now) {
                damage = damage.union(*rect);
            }
            if let Some((_, pending)) = &stage.waiting {
                damage = damage.union(*pending);
            }
            let Some(damage) = damage.clamp_to(bounds) else {
                continue;
            };
            // A buffer the compositor has given back is what a compose needs.
            // With none free the picture stays as it is and the next pump tries
            // again.
            let Some(slot) =
                (0..PIN_SLOTS).find(|slot| self.session.pin_slot_available(&name, *slot))
            else {
                stage.waiting = Some((now, damage));
                continue;
            };
            let rendered = match self.targets.get_mut(&name) {
                Some(target) => target.render(&pins, &output, &self.style, damage, slot),
                None => continue,
            };
            if let Err(error) = rendered {
                if std::env::var_os("VSHOT_PIN_DEBUG").is_some() {
                    eprintln!("vshot: pin-hdr: {name}: {error}");
                }
                stage.waiting = Some((now, damage));
                continue;
            }
            match self.session.present_pin_buffer(&name, slot, damage) {
                Ok(true) => {
                    stage.painted = now;
                    stage.waiting = None;
                }
                Ok(false) => stage.waiting = Some((now, damage)),
                Err(error) => {
                    if std::env::var_os("VSHOT_PIN_DEBUG").is_some() {
                        eprintln!("vshot: pin-hdr: {name}: {error}");
                    }
                    stage.waiting = Some((now, damage));
                }
            }
        }
    }

    /// Offers every picture that found no buffer again, now that the compositor
    /// may have given one back.
    fn show_pending(&mut self) {
        if self.stages.values().any(|stage| stage.waiting.is_some()) {
            self.show();
        }
    }

    /// Dispatches whatever the compositor has already sent, without waiting.
    fn pump(&mut self) -> Result<()> {
        self.session.pump(Duration::ZERO)
    }

    /// Waits until the daemon speaks again or the compositor has something to
    /// say, whichever comes first.
    ///
    /// Nothing else is outstanding when this is called — the socket has been read
    /// dry and every wayland event dispatched — so blocking here waits for the
    /// next thing that matters rather than pacing an update with a sleep.  A
    /// buffer release always wakes it, which is what a refused compose needs.
    fn wait(&mut self, stream: &UnixStream) -> Result<()> {
        self.session.pump_watching(Some(stream.as_fd()), None)
    }
}

impl PinTarget {
    /// Opens a drawing context for one output and gives its surface the buffers
    /// it will draw into.
    ///
    /// The render node is the first one that will take the format: the compositor
    /// offers modifiers for `ABGR16161616F`, and only a device whose allocator
    /// knows them can hand back a buffer the compositor accepts.  `chosen` keeps
    /// the node that worked for the first output, so a second output does not
    /// probe all of them again.
    fn open(
        session: &mut WaylandSession,
        output: &OutputRect,
        modifiers: &[u64],
        chosen: &mut Option<PathBuf>,
        debug: bool,
    ) -> Result<Self> {
        let width = output.pixel_size.width;
        let height = output.pixel_size.height;
        let mut last = String::from("no render node was usable");
        let nodes = match chosen.clone() {
            Some(node) => vec![node],
            None => render_nodes(),
        };
        for node in nodes {
            let surface = match Surface::open(&node) {
                Ok(surface) => surface,
                Err(why) => {
                    last = format!("{}: {why}", node.display());
                    continue;
                }
            };
            let (buffers, specs) = match Self::allocate(&surface, modifiers, width, height, debug) {
                Ok(allocated) => allocated,
                Err(error) => {
                    last = format!("{}: {error}", node.display());
                    continue;
                }
            };
            if let Err(error) = session.install_pin_buffers(&output.name, &specs) {
                for buffer in &buffers {
                    surface.free_buffer(buffer);
                }
                return Err(error);
            }
            *chosen = Some(node);
            return Ok(Self {
                surface,
                buffers,
                images: HashMap::new(),
                masks: HashMap::new(),
                width,
                height,
            });
        }
        Err(VshotError::PinSurface(last))
    }

    /// Allocates this output's picture buffers and leaves each of them
    /// transparent, ready for the compositor.
    fn allocate(
        surface: &Surface,
        modifiers: &[u64],
        width: u32,
        height: u32,
        debug: bool,
    ) -> Result<(Vec<Buffer>, Vec<PinBufferSpec>)> {
        if modifiers.is_empty() {
            return Err(VshotError::PinSurface(
                "the compositor offers no modifier for a half-float picture".into(),
            ));
        }
        surface.begin(width, height)?;
        let mut buffers = Vec::with_capacity(PIN_SLOTS);
        for _ in 0..PIN_SLOTS {
            let mut allocated = None;
            let mut why = String::from("no modifier was accepted");
            for modifier in modifiers {
                match surface.alloc(width, height, *modifier) {
                    Ok(buffer) => {
                        allocated = Some(buffer);
                        break;
                    }
                    Err(error) => why = error.to_string(),
                }
            }
            let Some(buffer) = allocated else {
                for buffer in &buffers {
                    surface.free_buffer(buffer);
                }
                return Err(VshotError::PinSurface(why));
            };
            buffers.push(buffer);
        }
        // Both buffers start transparent: the compositor may take either of them
        // first, and only the region a commit damages is ever read, so a buffer
        // that was never drawn into would show whatever the allocator left in it.
        for buffer in &buffers {
            surface.present(buffer.slot, 0, 0, width as i32, height as i32);
        }
        if debug {
            eprintln!(
                "vshot: pin-hdr: {width}x{height} {PIN_SLOTS} half-float buffer(s) at modifier 0x{:x}",
                buffers.first().map(|buffer| buffer.modifier).unwrap_or(0)
            );
        }
        let specs = buffers
            .iter()
            .map(|buffer| PinBufferSpec {
                fd: buffer.fd,
                offset: buffer.offset,
                stride: buffer.stride,
                modifier: buffer.modifier,
                format: buffer.format,
                width,
                height,
            })
            .collect();
        Ok((buffers, specs))
    }

    /// Draws the whole picture for this output into `slot`: the region that
    /// changed is cleared, every pin is drawn into it — shadow, picture, rim, in
    /// that order — and the whole picture is copied into the buffer.
    ///
    /// The drawing stays regional, but the copy cannot: the compositor re-reads
    /// the buffer where the damage does not reach whenever the attached buffer
    /// changes, which with two picture buffers alternated is every other commit.
    /// A slot that had only ever been handed the damaged regions would then show
    /// a pin where it used to be.  Qt's own surfaces never meet this -- copying a
    /// backing store is a whole-surface affair -- which is why only this helper
    /// ghosted.
    fn render(
        &mut self,
        pins: &[(u64, Pin)],
        output: &OutputRect,
        style: &Style,
        damage: Rect,
        slot: usize,
    ) -> Result<()> {
        self.surface.begin(self.width, self.height)?;
        // A pin that is gone leaves nothing of its own behind: its caches go with
        // it rather than sitting in video memory for the life of the helper.
        let live = pins.iter().map(|(id, _)| *id).collect::<Vec<u64>>();
        self.images.retain(|id, _| {
            let keep = live.contains(id);
            if !keep {
                self.surface.drop(*id);
            }
            keep
        });
        self.masks.retain(|id, _| {
            let keep = live.contains(id);
            if !keep {
                self.surface.drop(MASK_NAMESPACE + *id);
            }
            keep
        });
        self.surface.clear(
            damage.origin.x,
            damage.origin.y,
            damage.size.width as i32,
            damage.size.height as i32,
        );
        for (id, pin) in pins {
            self.upload(id, pin)?;
            self.draw_pin(id, pin, output, style)?;
        }
        if !self.surface.present(
            slot as i32,
            0,
            0,
            self.width as i32,
            self.height as i32,
        ) {
            return Err(VshotError::PinSurface(format!(
                "the picture could not be copied into {}'s buffer",
                output.name
            )));
        }
        Ok(())
    }

    /// Makes sure this pin's pixels are on the device, reading them once.
    fn upload(&mut self, id: &u64, pin: &Pin) -> Result<()> {
        if self.images.get(id) == Some(&pin.path) {
            return Ok(());
        }
        self.surface.drop(*id);
        self.surface.image(*id, &pin.words, pin.width, pin.height)?;
        self.images.insert(*id, pin.path.clone());
        Ok(())
    }

    /// Draws one pin: its shadow, its picture, its rim.
    fn draw_pin(&mut self, id: &u64, pin: &Pin, output: &OutputRect, style: &Style) -> Result<()> {
        if !pin.visible {
            return Ok(());
        }
        let Some(placed) = placement(pin, output.geometry, output.scale) else {
            return Ok(());
        };
        let scale = f64::from(output.scale);
        let width = i32::try_from(placed.width).unwrap_or(i32::MAX);
        let height = i32::try_from(placed.height).unwrap_or(i32::MAX);
        let left = i32::try_from(placed.left).unwrap_or(i32::MIN);
        let top = i32::try_from(placed.top).unwrap_or(i32::MIN);
        let radius = (f64::from(style.radius) * scale).round() as i32;
        if let Some(shadow) = &style.shadow {
            let spread = (f64::from(shadow.size) * scale).round() as i32;
            let offset = (f64::from(shadow.offset) * scale).round() as i32;
            if spread > 0 && shadow.opacity > 0 {
                let key = (width, height, radius, spread, offset, shadow.opacity);
                let mask_id = MASK_NAMESPACE + *id;
                if self.masks.get(id) != Some(&key) {
                    self.surface.drop(mask_id);
                    self.surface.mask(
                        mask_id,
                        width,
                        height,
                        radius,
                        spread,
                        offset,
                        shadow.opacity,
                    )?;
                    self.masks.insert(*id, key);
                }
                self.surface.draw_shadow(
                    mask_id,
                    left - spread,
                    top - spread,
                    width + 2 * spread,
                    height + 2 * spread,
                );
            }
        }
        self.surface
            .draw_image(*id, left, top, width, height, radius);
        if let Some(border) = &style.border {
            if border.width > 0 {
                let thickness = ((f64::from(border.width) * scale).round() as i32).max(1);
                let rgb = if pin.active {
                    border.active
                } else {
                    border.colour
                };
                let white = if pin.white.is_finite() && pin.white > 0.0 {
                    pin.white
                } else {
                    crate::model::hdr::REFERENCE_WHITE_NITS
                };
                self.surface.draw_rim(
                    left,
                    top,
                    width,
                    height,
                    radius,
                    thickness,
                    [
                        pq_code(rgb[0], white),
                        pq_code(rgb[1], white),
                        pq_code(rgb[2], white),
                    ],
                );
            }
        }
        Ok(())
    }
}

/// The render nodes to try, in order: an explicit override first, then every
/// `/dev/dri/renderD*`.
///
/// The device that must win is the one the compositor's own allocations come
/// from — on a machine with two GPUs, only one of them — and the honest way to
/// find out is to ask the allocator, which is why [`PinTarget::open`] probes them
/// rather than guessing from a name.
fn render_nodes() -> Vec<PathBuf> {
    let mut nodes = Vec::new();
    if let Some(value) = std::env::var_os("VSHOT_HDR_RENDER_NODE") {
        nodes.push(PathBuf::from(value));
    }
    let mut scanned = std::fs::read_dir("/dev/dri")
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("renderD"))
                })
                .collect::<Vec<PathBuf>>()
        })
        .unwrap_or_default();
    scanned.sort();
    for node in scanned {
        if !nodes.contains(&node) {
            nodes.push(node);
        }
    }
    nodes
}

/// Runs the helper until the daemon goes away.
pub fn run(socket: &Path) -> Result<()> {
    // A stale socket from a helper that was killed leaves connect() failing for
    // ever; the daemon passes a path nothing else uses.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|source| VshotError::HdrPin {
        path: socket.to_path_buf(),
        reason: format!("cannot listen for the pin daemon: {source}"),
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|source| VshotError::HdrPin {
            path: socket.to_path_buf(),
            reason: format!("cannot make the socket non-blocking: {source}"),
        })?;
    let mut stream = accept_within(&listener, CONNECT_TIMEOUT)?;
    stream
        .set_nonblocking(true)
        .map_err(|source| VshotError::HdrPin {
            path: socket.to_path_buf(),
            reason: format!("cannot make the connection non-blocking: {source}"),
        })?;

    let mut surfaces = Surfaces::new()?;
    surfaces.start()?;
    let mut writer = stream.try_clone().map_err(|source| VshotError::HdrPin {
        path: socket.to_path_buf(),
        reason: format!("cannot duplicate the connection: {source}"),
    })?;

    let debug = std::env::var_os("VSHOT_PIN_DEBUG").is_some();
    if debug {
        eprintln!(
            "vshot: pin-hdr: {} picture surface(s); HDR outputs: {}",
            surfaces.targets.len(),
            if surfaces.hdr_outputs.is_empty() {
                "none".into()
            } else {
                surfaces.hdr_outputs.join(", ")
            }
        );
    }
    let mut reply = json!({
        "event": "mapped",
        "outputs": surfaces.hdr_outputs.clone(),
    })
    .to_string();
    reply.push('\n');
    let _ = writer.write_all(reply.as_bytes());
    let _ = writer.flush();

    let mut pending_lines: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut quit = false;
    while !quit {
        // Whatever the compositor has already said.  A buffer coming back is
        // what lets a compose that found both of them busy go out.
        surfaces.pump()?;

        // Everything the daemon has said so far.  Reading until it would block
        // is what keeps a drag from being paced by this side: the next position
        // is picked up as soon as it is written, rather than after a sleep.
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    quit = true; // the daemon closed: nothing left to show
                    break;
                }
                Ok(count) => pending_lines.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    if debug {
                        eprintln!("vshot: pin-hdr: read failed: {error}");
                    }
                    quit = true;
                    break;
                }
            }
        }

        // Whole lines only: a command that is still arriving waits for the rest
        // of itself.  Within one batch only the last position is kept: a drag
        // sends one per motion event, each replaces the whole stack, and
        // composing every one of them would redraw the output once per event for
        // a single picture.
        let mut consumed = 0;
        let mut latest: Option<serde_json::Value> = None;
        while let Some(offset) = pending_lines[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + offset;
            let text = String::from_utf8_lossy(&pending_lines[consumed..end]).to_string();
            consumed = end + 1;
            match parse_command(&text) {
                Ok(Command::Quit) => {
                    quit = true;
                    break;
                }
                Ok(Command::Pins(value)) => latest = Some(value),
                Ok(Command::Unknown) => {
                    let error = VshotError::WaylandProtocol(
                        "the pin daemon sent an unknown command".into(),
                    );
                    reject(&error, debug, &mut writer);
                }
                Err(error) => reject(&error, debug, &mut writer),
            }
        }
        if quit {
            pending_lines.clear();
        } else {
            pending_lines.drain(..consumed);
        }
        let mut composed = false;
        if let Some(value) = latest {
            match apply_pins(&value, &mut surfaces, debug) {
                Ok(()) => {
                    composed = true;
                    // One line per composed stack, so a client can wait for the
                    // update to have been committed before sending the next; the
                    // pin daemon drains these and does not wait on them.
                    let _ = writer.write_all(b"{\"ok\":true}\n");
                    let _ = writer.flush();
                }
                Err(error) => reject(&error, debug, &mut writer),
            }
        }
        if quit {
            break;
        }
        if !composed {
            // Nothing new to show.  A commit that found both buffers busy offers
            // its picture again here; otherwise this is where the helper blocks
            // until the daemon speaks or a buffer comes back.
            surfaces.show_pending();
        }

        // Nothing left to do, so wait for the next thing that matters: another
        // command, or the compositor handing a buffer back.
        surfaces.wait(&stream)?;
        surfaces.show_pending();
    }
    Ok(())
}

/// Writes one failure back to the daemon, and traces it when the debug switch
/// asks for it.  A rejected command never stops the helper: the next position
/// still gets a chance.
fn reject(error: &VshotError, debug: bool, writer: &mut UnixStream) {
    if debug {
        eprintln!("vshot: pin-hdr: {error}");
    }
    let mut message = json!({"ok": false, "error": error.to_string()}).to_string();
    message.push('\n');
    let _ = writer.write_all(message.as_bytes());
    let _ = writer.flush();
}

/// Waits for the daemon to connect, and gives up rather than sitting on a
/// socket nobody will ever use.
fn accept_within(listener: &UnixListener, timeout: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(VshotError::HdrPin {
                    path: PathBuf::from("(listener)"),
                    reason: format!("accept failed: {error}"),
                });
            }
        }
        if Instant::now() >= deadline {
            return Err(VshotError::HdrPin {
                path: PathBuf::from("(listener)"),
                reason: "the pin daemon never connected".into(),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One command from the daemon, classified but not yet applied.
enum Command {
    Quit,
    Pins(serde_json::Value),
    Unknown,
}

/// Reads one line into a [`Command`].  Parsing is deliberately separate from
/// applying: a batch of positions is parsed whole and only its last one is
/// applied, so classifying must be cheap and free of side effects.
fn parse_command(text: &str) -> Result<Command> {
    let command: serde_json::Value = serde_json::from_str(text).map_err(|error| {
        VshotError::WaylandProtocol(format!("the pin daemon sent no JSON: {error}"))
    })?;
    match command.get("cmd").and_then(|value| value.as_str()) {
        Some("quit") => Ok(Command::Quit),
        Some("pins") => Ok(Command::Pins(command)),
        _ => Ok(Command::Unknown),
    }
}

/// Replaces the stack the helper shows with the one the command describes.
fn apply_pins(command: &serde_json::Value, surfaces: &mut Surfaces, debug: bool) -> Result<()> {
    let incoming = command.get("pins").and_then(|value| value.as_array());
    let Some(incoming) = incoming else {
        return Err(VshotError::WaylandProtocol(
            "the pin daemon sent no pin list".into(),
        ));
    };
    let style = match style_of(command) {
        Some(style) => style,
        None => surfaces.style.clone(),
    };
    let previous = std::mem::take(&mut surfaces.last_pins);
    let mut wanted = Vec::new();
    for entry in incoming {
        let id = entry.get("id").and_then(|value| value.as_u64());
        let path = entry.get("path").and_then(|value| value.as_str());
        let (Some(id), Some(path)) = (id, path) else {
            continue;
        };
        let path = PathBuf::from(path);
        let existing = previous
            .iter()
            .find(|(pin_id, pin)| *pin_id == id && pin.path == path);
        let pin = match existing {
            // Same id, same file: the pixels are the ones already read.
            Some((_, pin)) => Pin {
                path: path.clone(),
                words: Arc::clone(&pin.words),
                width: pin.width,
                height: pin.height,
                origin: point_of(entry, "x", "y"),
                scale: scale_of(entry),
                visible: visible_of(entry),
                active: entry
                    .get("active")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                white: white_of(entry, pin.white),
            },
            None => {
                let image = read_pq_file(&path)?;
                Pin {
                    path: path.clone(),
                    words: Arc::new(image.words),
                    width: image.width,
                    height: image.height,
                    origin: point_of(entry, "x", "y"),
                    scale: scale_of(entry),
                    visible: visible_of(entry),
                    active: entry
                        .get("active")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                    white: white_of(entry, image.reference_nits),
                }
            }
        };
        wanted.push((id, pin));
    }
    if debug {
        let names = wanted
            .iter()
            .map(|(id, pin)| {
                format!(
                    "{id}@{}x{}+{},{}",
                    pin.width, pin.height, pin.origin.x, pin.origin.y
                )
            })
            .collect::<Vec<String>>()
            .join(" ");
        eprintln!(
            "vshot: pin-hdr: {} pin(s): {names} [radius {} shadow {:?} border {:?}]",
            wanted.len(),
            surfaces.style.radius,
            surfaces.style.shadow,
            surfaces.style.border,
        );
    }
    // The stack the daemon repeats while nothing about it changes — a style
    // reload, a pin coming to the front, a plain SDR pin moving — is not worth
    // recomposing, and the pixels here are derived from these fields alone.
    let unchanged = previous.len() == wanted.len()
        && previous
            .iter()
            .zip(&wanted)
            .all(|((old_id, old), (new_id, new))| {
                old_id == new_id
                    && old.path == new.path
                    && old.origin == new.origin
                    && old.scale == new.scale
                    && old.visible == new.visible
                    && old.active == new.active
                    && old.white == new.white
            });
    surfaces.style = style;
    surfaces.last_pins = wanted;
    if !unchanged {
        surfaces.show();
    }
    Ok(())
}

/// Reads the style the daemon hands over with the stack, when it sends one.
fn style_of(command: &serde_json::Value) -> Option<Style> {
    let style = command.get("style")?;
    let shadow = style.get("shadow").and_then(|value| {
        if value.is_null() {
            return None;
        }
        Some(Shadow {
            size: value.get("size").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            offset: value.get("offset").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            opacity: value.get("opacity").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        })
    });
    let border = style.get("border").and_then(|value| {
        if value.is_null() {
            return None;
        }
        Some(Border {
            width: value.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            colour: colour_of(value.get("color")),
            active: colour_of(value.get("active")),
        })
    });
    Some(Style {
        radius: style
            .get("radius")
            .and_then(|value| value.as_i64())
            .unwrap_or(0) as i32,
        shadow,
        border,
    })
}

/// One `[r, g, b]` triple out of the command.
fn colour_of(value: Option<&serde_json::Value>) -> [u8; 3] {
    let mut rgb = [0u8; 3];
    if let Some(values) = value.and_then(|value| value.as_array()) {
        for (slot, value) in rgb.iter_mut().zip(values) {
            *slot = value.as_u64().unwrap_or(0).min(255) as u8;
        }
    }
    rgb
}

fn point_of(entry: &serde_json::Value, x_key: &str, y_key: &str) -> Point {
    Point::new(
        entry
            .get(x_key)
            .and_then(|value| value.as_i64())
            .unwrap_or(0) as i32,
        entry
            .get(y_key)
            .and_then(|value| value.as_i64())
            .unwrap_or(0) as i32,
    )
}

fn scale_of(entry: &serde_json::Value) -> f64 {
    entry
        .get("scale")
        .and_then(|value| value.as_f64())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(1.0)
}

fn visible_of(entry: &serde_json::Value) -> bool {
    entry
        .get("visible")
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// The light the pin's `1.0` stands for: what the daemon says, or what the pin
/// was captured at.
fn white_of(entry: &serde_json::Value, fallback: f32) -> f32 {
    entry
        .get("white")
        .and_then(|value| value.as_f64())
        .filter(|white| white.is_finite() && *white > 0.0)
        .map(|white| white as f32)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(width: u32, height: u32, origin: Point, scale: f64) -> Pin {
        Pin {
            path: PathBuf::from("(none)"),
            words: Arc::new(vec![0; (width * height) as usize]),
            width,
            height,
            origin,
            scale,
            visible: true,
            active: false,
            white: 203.0,
        }
    }

    #[test]
    fn a_pq_file_round_trips_through_the_reader() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pin.pq");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PQ_MAGIC);
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&203.0f32.to_le_bytes());
        // The gamut's own coordinates, as the protocol's millionth units.
        for (x, y) in Primaries::Bt2020.chromaticities() {
            bytes.extend_from_slice(&((x * 1_000_000.0).round() as i32).to_le_bytes());
            bytes.extend_from_slice(&((y * 1_000_000.0).round() as i32).to_le_bytes());
        }
        assert_eq!(bytes.len(), PQ_HEADER);
        for word in [0x3ff00000u32, 0x00000000u32] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();
        let image = read_pq_file(&path).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.reference_nits, 203.0);
        assert_eq!(image.primaries, Primaries::Bt2020);
        assert_eq!(image.words, vec![0x3ff00000, 0x00000000]);
    }

    #[test]
    fn a_wide_gamut_pin_reads_back_in_the_gamut_it_names() {
        // A pin taken from a P3-like output carries that output's own gamut, and
        // the editor has to decode it in the same space: reading those codes as
        // BT.2020 is what shifted every colour of a wide-gamut pin.
        let output = Primaries::from_chromaticities(
            (0.686523, 0.308594),
            (0.223633, 0.689453),
            (0.142578, 0.060547),
        );
        assert!(matches!(output, Primaries::Custom { .. }));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pin.pq");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PQ_MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&203.0f32.to_le_bytes());
        for (x, y) in output.chromaticities() {
            bytes.extend_from_slice(&((x * 1_000_000.0).round() as i32).to_le_bytes());
            bytes.extend_from_slice(&((y * 1_000_000.0).round() as i32).to_le_bytes());
        }
        bytes.extend_from_slice(&0x3ff00000u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let image = read_pq_file(&path).unwrap();
        // The coordinates survive the round trip, so the gamut is the one the
        // capture had rather than a name this pipeline happened to prefer.
        for (read, wrote) in image
            .primaries
            .chromaticities()
            .iter()
            .zip(output.chromaticities())
        {
            assert!((read.0 - wrote.0).abs() < 1e-3, "{read:?} vs {wrote:?}");
            assert!((read.1 - wrote.1).abs() < 1e-3, "{read:?} vs {wrote:?}");
        }
    }

    #[test]
    fn a_file_that_is_not_a_pq_image_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pin.pq");
        std::fs::write(&path, b"not a pin").unwrap();
        assert!(read_pq_file(&path).is_err());
    }

    /// A 4x4 output with its top-left at (10, 20), at 2x.
    fn output() -> OutputRect {
        OutputRect {
            name: "DP-1".into(),
            geometry: Rect::new(10, 20, 4, 4),
            scale: 2,
            pixel_size: Size::new(8, 8),
        }
    }

    #[test]
    fn a_damage_rect_covers_the_shadow_and_the_rim_around_a_pin() {
        // A 1x1 pin at 1x, 1 logical pixel in from the output's corner: two
        // device pixels square at (2, 2).  With no shadow and no rim that is all
        // that is damaged.
        let style = Style::default();
        let plain = pin(1, 1, Point::new(11, 21), 1.0);
        assert_eq!(
            pin_damage_rect(&plain, &output(), &style),
            Some(Rect::new(2, 2, 2, 2))
        );
        // A shadow of 14 reaches 18 logical pixels past the pin -- the blur's
        // reach, the drop below it, and the pixel the soft edge lands on -- which
        // at 2x is 36 device pixels, clipped to the output here.
        let shadowed = Style {
            shadow: Some(Shadow {
                size: 14,
                offset: 3,
                opacity: 120,
            }),
            ..Style::default()
        };
        assert_eq!(
            pin_damage_rect(&plain, &output(), &shadowed),
            Some(Rect::new(0, 0, 8, 8))
        );
    }

    #[test]
    fn a_hidden_pin_damages_nothing() {
        let hidden = Pin {
            visible: false,
            ..pin(1, 1, Point::new(11, 21), 1.0)
        };
        assert_eq!(pin_damage_rect(&hidden, &output(), &Style::default()), None);
    }

    #[test]
    fn a_pin_that_misses_the_output_entirely_damages_nothing() {
        let elsewhere = pin(1, 1, Point::new(100, 100), 1.0);
        assert_eq!(
            pin_damage_rect(&elsewhere, &output(), &Style::default()),
            None
        );
    }

    #[test]
    fn a_rim_colour_becomes_the_pq_code_of_its_own_light() {
        // Mid grey at 203 nits of white: the sRGB transfer puts it at about half
        // the white's light, and the PQ curve puts that near 0.53 of full range.
        let code = pq_code(187, 203.0);
        assert!(code > 0.5 && code < 0.58, "mid grey landed at {code}");
        assert!(pq_code(0, 203.0).abs() < 1e-6);
        assert!((pq_code(255, 203.0) - 0.5795).abs() < 0.01);
        // A brighter white puts the same colour higher up the curve.
        assert!(pq_code(187, 300.0) > pq_code(187, 203.0));
    }

    #[test]
    fn the_style_the_daemon_sends_is_read_back_whole() {
        let command = serde_json::json!({
            "cmd": "pins",
            "style": {
                "radius": 12,
                "shadow": {"size": 14, "offset": 3, "opacity": 120},
                "border": {"width": 2, "color": [192, 192, 192], "active": [255, 96, 96]},
            },
        });
        assert_eq!(
            style_of(&command),
            Some(Style {
                radius: 12,
                shadow: Some(Shadow {
                    size: 14,
                    offset: 3,
                    opacity: 120,
                }),
                border: Some(Border {
                    width: 2,
                    colour: [192, 192, 192],
                    active: [255, 96, 96],
                }),
            })
        );
        // No style at all leaves the last one in place.
        assert_eq!(style_of(&serde_json::json!({"cmd": "pins"})), None);
        // A null shadow is a shadow that is off, not a missing one.
        let off = serde_json::json!({"cmd": "pins", "style": {"radius": 0, "shadow": null}});
        assert_eq!(
            style_of(&off),
            Some(Style {
                radius: 0,
                shadow: None,
                border: None,
            })
        );
    }
}
