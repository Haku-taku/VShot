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
use std::time::Duration;

use serde_json::json;

use crate::error::{Result, VshotError};
use crate::geometry::{Point, Size};
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

/// The daemon: the pins, and what is showing them.
struct Daemon {
    pins: Vec<Pinned>,
    next_id: u64,
    all_visible: bool,
    surfaces: Surfaces,
}

impl Daemon {
    fn new() -> Result<Self> {
        let mut surfaces = Surfaces::new()?;
        surfaces.start()?;
        Ok(Self {
            pins: Vec::new(),
            next_id: 1,
            all_visible: true,
            surfaces,
        })
    }

    /// Hands the whole stack to the renderer and asks it to show it.
    ///
    /// Back to front, in the order the pins were added: the last one is the
    /// frontmost, which is what a pin just made should be.
    fn refresh(&mut self) -> Result<()> {
        let stack = self
            .pins
            .iter()
            .map(|pin| (pin.id, pin.render_pin()))
            .collect::<Vec<(u64, Pin)>>();
        let debug = std::env::var_os("VSHOT_PIN_DEBUG").is_some();
        if debug {
            let names = self
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
            eprintln!("vshot-pin: {} pin(s): {names}", self.pins.len());
        }
        // No style: the one this side read from the config when it came up is
        // the one to draw with, and re-reading the file on every move would
        // make the look change under a drag.
        self.surfaces.set_pins(stack, None, debug)
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
                let Some(pin) = self.pins.iter_mut().find(|pin| pin.id == id) else {
                    return Err(VshotError::Pin(format!(
                        "move names a pin that is not pinned"
                    )));
                };
                pin.origin = Point::new(x, y);
                self.refresh()?;
                Ok(json!({"ok": true}))
            }
            PinCommand::Toggle => {
                self.all_visible = !self.all_visible;
                self.apply_visibility();
                Ok(json!({"ok": true}))
            }
            PinCommand::Show => {
                self.all_visible = true;
                self.apply_visibility();
                Ok(json!({"ok": true}))
            }
            PinCommand::Hide => {
                self.all_visible = false;
                self.apply_visibility();
                Ok(json!({"ok": true}))
            }
            PinCommand::Close => {
                self.pins.clear();
                self.refresh()?;
                Ok(json!({"ok": true}))
            }
            PinCommand::List => Ok(json!({
                "ok": true,
                "count": self.pins.len(),
                "visible": self.all_visible,
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
        let id = self.next_id;
        self.next_id += 1;
        self.pins.push(Pinned {
            id,
            picture,
            path: path.to_path_buf(),
            origin,
            scale,
            visible: self.all_visible,
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

    fn apply_visibility(&mut self) {
        for pin in &mut self.pins {
            pin.visible = self.all_visible;
        }
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
    let debug = std::env::var_os("VSHOT_PIN_DEBUG").is_some();
    let mut daemon = Daemon::new()?;
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
        daemon.surfaces.wait_on(Some(wake.as_fd()))?;
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

    /// A request this daemon cannot serve is refused with a reason rather than
    /// read as something else.
    #[test]
    fn an_unknown_command_is_refused() {
        assert!(parse_request("{\"cmd\":\"dance\"}\n").is_err());
        assert!(parse_request("not json at all\n").is_err());
    }
}
