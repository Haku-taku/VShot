// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! The accessibility tree, as a source of selectable regions.
//!
//! A window is one rectangle; the widgets inside it are many.  Those come from
//! AT-SPI, over D-Bus.  This is the client that reads them and the conversion
//! that turns them into [`RegionNode`]s, so the picker offers a button or a
//! panel the way it already offers a window.
//!
//! Two things about AT-SPI on Wayland shape everything here.
//!
//! First, **its coordinates are window-relative**, whatever the coordinate type
//! asks for.  `Component.GetExtents` answers the same numbers for `SCREEN` and
//! for `WINDOW`, because a Wayland client cannot know where the compositor put
//! it.  Global position therefore has to come from somewhere else — the
//! compositor's own window list — and every rect here is
//! `window origin + element rect`.  See `src/capture/window.rs` for that side.
//!
//! Second, **each accessible is addressed by a bus name and a path of its
//! own**, handed back by `GetChildAtIndex` as `(so)`.  There is no one
//! destination to talk to: every node carries its own.

use zbus::blocking::connection::Builder;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use crate::error::Result;
use crate::capture::window::WindowCandidate;
use crate::geometry::{Point, Rect, Size};
use crate::selection_region::{RegionKind, RegionNode};

/// How long one query may take before the answer is given up on.
///
/// An application that registers with AT-SPI and then never answers holds a
/// D-Bus method call open indefinitely, and the tree walk is a call per node --
/// so without this a single unresponsive window stops the picker dead instead
/// of simply having no elements.  Measured against a well-behaved tree the
/// whole walk takes a few milliseconds, so this is generous.
const METHOD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// How many nodes a whole walk may visit.
///
/// A tree is not unbounded in practice, but a misreporting one -- a child
/// count read as a huge number, or a cycle -- would otherwise keep the walk
/// going for as long as the desktop is up.  The cap turns that into an
/// ordinary "no elements" answer.
const MAX_NODES: usize = 4096;

/// Well-behaved trees stop here.  A tree that nests deeper than this is not
/// offering the user anything they can point at, and walking it costs a D-Bus
/// round trip per node, so the walk is capped rather than left to run.
const MAX_DEPTH: u32 = 8;

/// An extent wider or taller than this is not a widget.  Hidden pages of a
/// notebook report their size as uninitialized memory — one measured at
/// 724708414 pixels wide — and a rect like that would be drawn across the whole
/// screen.
const MAX_EXTENT: i32 = 1 << 16;

/// One accessible, as AT-SPI addresses it: a bus name plus a path.
#[derive(Clone, Debug)]
struct Accessible {
    bus: String,
    path: String,
}

impl Accessible {
    fn root(bus: impl Into<String>) -> Self {
        Self {
            bus: bus.into(),
            path: "/org/a11y/atspi/accessible/root".to_string(),
        }
    }
}

/// A live connection to the accessibility bus.
///
/// AT-SPI does not live on the session bus.  `org.a11y.Bus` on the session bus
/// hands out the address of a second bus, and everything else happens there.
pub struct Accessibility {
    connection: Connection,
}

impl Accessibility {
    /// Opens the accessibility bus, or says why it could not.
    ///
    /// A desktop with no AT-SPI is not an error the caller should fail on —
    /// picking falls back to whole windows — so the reason is returned rather
    /// than logged here.
    pub fn connect() -> std::result::Result<Self, String> {
        let session = Connection::session().map_err(|error| format!("no session bus: {error}"))?;
        let address: String = session
            .call_method(
                Some("org.a11y.Bus"),
                "/org/a11y/bus",
                Some("org.a11y.Bus"),
                "GetAddress",
                &(),
            )
            .map_err(|error| format!("org.a11y.Bus gave no address: {error}"))?
            .body()
            .deserialize()
            .map_err(|error| format!("the a11y bus address was not a string: {error}"))?;
        let connection = Builder::address(address.as_str())
            .map_err(|error| format!("the a11y bus address was unusable: {error}"))?
            // Without this an application that registers and then never answers
            // holds a call open forever, and the tree walk is a call per node.
            .method_timeout(METHOD_TIMEOUT)
            .build()
            .map_err(|error| format!("cannot reach the a11y bus: {error}"))?;
        Ok(Self { connection })
    }

    /// The frames of every application, with the tree under each.
    ///
    /// A frame is matched to a window by its title, which is why the caller
    /// passes the windows it already has: AT-SPI knows names, the compositor
    /// knows positions, and only the pair is a region the user can point at.
    pub fn frames(&self) -> std::result::Result<Vec<AccessibilityFrame>, String> {
        let root = Accessible::root("org.a11y.atspi.Registry");
        let count = self.child_count(&root).unwrap_or(0);
        let mut frames = Vec::new();
        for index in 0..count {
            let Some(application) = self.child_at(&root, index) else {
                continue;
            };
            let application_count = self.child_count(&application).unwrap_or(0);
            for frame_index in 0..application_count {
                let Some(frame) = self.child_at(&application, frame_index) else {
                    continue;
                };
                let name = self.name(&frame).unwrap_or_default();
                let rect = self
                    .extents(&frame)
                    .and_then(|(x, y, width, height)| to_rect(x, y, width, height));
                frames.push(AccessibilityFrame {
                    accessible: frame,
                    title: name,
                    rect,
                });
            }
        }
        Ok(frames)
    }

    /// The elements inside `frame`, as regions in global logical pixels.
    ///
    /// `origin` is the frame's own global position, which AT-SPI cannot supply:
    /// every rect here is the window-relative extent shifted by it, and then
    /// clipped to the frame so a widget that hangs outside its window does not
    /// offer a region off the side of it.
    pub fn elements(
        &self,
        frame: &AccessibilityFrame,
        origin: Point,
    ) -> std::result::Result<Vec<RegionNode>, String> {
        let Some(frame_rect) = frame.rect.and_then(|rect| rect.translate(origin).ok()) else {
            // No frame geometry means nothing to clip to and nothing to place a
            // child against.
            return Ok(Vec::new());
        };
        let mut elements = Vec::new();
        let mut budget = MAX_NODES;
        let count = self.child_count(&frame.accessible).unwrap_or(0);
        for index in 0..count {
            let Some(child) = self.child_at(&frame.accessible, index) else {
                continue;
            };
            if let Some(node) = self.node(&child, origin, frame_rect, 0, &mut budget) {
                elements.push(node);
            }
            if budget == 0 {
                break;
            }
        }
        Ok(elements)
    }

    /// One accessible and its subtree, or `None` when it has no usable
    /// geometry.  A node with no rect cannot be pointed at, so it and
    /// everything under it is left out — which is what keeps the degenerate
    /// tenth of a real tree out of the picker.
    fn node(
        &self,
        accessible: &Accessible,
        origin: Point,
        frame: Rect,
        depth: u32,
        // Nodes left to visit across the whole walk.  A tree that misreports
        // its size would otherwise keep the walk going indefinitely; spending
        // the budget ends it as an ordinary short answer.
        budget: &mut usize,
    ) -> Option<RegionNode> {
        *budget = budget.saturating_sub(1);
        if *budget == 0 {
            return None;
        }
        let (x, y, width, height) = self.extents(accessible)?;
        let mut rect = to_rect(x, y, width, height)?;
        rect = rect.translate(origin).ok()?;
        rect = rect.clamp_to(frame)?;
        if rect.is_empty() {
            return None;
        }

        let role = self.role_name(accessible).unwrap_or_default();
        let name = self.name(accessible).unwrap_or_default();
        let label = match (role.as_str(), name.as_str()) {
            ("", "") => String::new(),
            ("", name) => name.to_string(),
            (role, "") => role.to_string(),
            (role, name) => format!("{role} {name}"),
        };

        let mut node = RegionNode::leaf(RegionKind::Element, rect, label);
        if depth + 1 < MAX_DEPTH {
            let count = self.child_count(accessible).unwrap_or(0);
            let mut children = Vec::new();
            for index in 0..count {
                let Some(child) = self.child_at(accessible, index) else {
                    continue;
                };
                if let Some(child_node) = self.node(&child, origin, frame, depth + 1, budget) {
                    children.push(child_node);
                }
                if *budget == 0 {
                    break;
                }
            }
            node = node.with_children(children);
        }
        Some(node)
    }

    fn proxy<'a>(
        &'a self,
        accessible: &'a Accessible,
        interface: &'static str,
    ) -> Option<Proxy<'a>> {
        let path = ObjectPath::try_from(accessible.path.as_str()).ok()?;
        Proxy::new(
            &self.connection,
            accessible.bus.as_str(),
            path,
            interface,
        )
        .ok()
    }

    /// Reads one property through an explicit `Properties.Get`.
    ///
    /// zbus's own `get_property` asks for `GetAll` first, and the AT-SPI root
    /// answers that with an empty signature, so the property never comes back.
    fn property<T>(&self, accessible: &Accessible, interface: &str, name: &str) -> Option<T>
    where
        T: TryFrom<OwnedValue>,
    {
        let proxy = self.proxy(accessible, "org.freedesktop.DBus.Properties")?;
        let reply = proxy.call_method("Get", &(interface, name)).ok()?;
        let (value,): (OwnedValue,) = reply.body().deserialize().ok()?;
        T::try_from(value).ok()
    }

    fn child_count(&self, accessible: &Accessible) -> Option<i32> {
        let count: i32 = self.property(accessible, "org.a11y.atspi.Accessible", "ChildCount")?;
        Some(count.clamp(0, 4096))
    }

    fn name(&self, accessible: &Accessible) -> Option<String> {
        self.property(accessible, "org.a11y.atspi.Accessible", "Name")
    }

    fn child_at(&self, accessible: &Accessible, index: i32) -> Option<Accessible> {
        let proxy = self.proxy(accessible, "org.a11y.atspi.Accessible")?;
        let reply = proxy.call_method("GetChildAtIndex", &(index)).ok()?;
        let (bus, path): (String, OwnedObjectPath) = reply.body().deserialize().ok()?;
        Some(Accessible {
            bus,
            path: path.to_string(),
        })
    }

    fn role_name(&self, accessible: &Accessible) -> Option<String> {
        let proxy = self.proxy(accessible, "org.a11y.atspi.Accessible")?;
        let reply = proxy.call_method("GetRoleName", &()).ok()?;
        reply.body().deserialize::<String>().ok()
    }

    /// The accessible's extent, **relative to its toplevel**.  Coordinate type
    /// 1 is `WINDOW`; `SCREEN` answers the same thing on Wayland, so asking for
    /// it would only suggest a meaning the number does not have.
    fn extents(&self, accessible: &Accessible) -> Option<(i32, i32, i32, i32)> {
        let proxy = self.proxy(accessible, "org.a11y.atspi.Component")?;
        let reply = proxy.call_method("GetExtents", &(1u32)).ok()?;
        reply.body().deserialize::<(i32, i32, i32, i32)>().ok()
    }
}

/// One toplevel the accessibility bus reports.
pub struct AccessibilityFrame {
    accessible: Accessible,
    /// The frame's name, which is the window's title.
    pub title: String,
    /// The frame's own extent, which is its size and — only by accident — a
    /// position.  Callers use the size and take the position from the
    /// compositor.
    pub rect: Option<Rect>,
}

/// `width`/`height` as a rect, or `None` for the extents that are not: the
/// zero-sized and absent ones, and the ones whose size is uninitialized memory.
fn to_rect(x: i32, y: i32, width: i32, height: i32) -> Option<Rect> {
    if width <= 0 || height <= 0 || width > MAX_EXTENT || height > MAX_EXTENT {
        return None;
    }
    let size = Size::new(u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    Some(Rect::new(x, y, size.width, size.height))
}

/// The elements of one window, as a source for the picker.
///
/// Returns `None` when accessibility cannot answer at all — no bus, no frame
/// matching this window, no tree under it — so the caller keeps offering whole
/// windows rather than failing the session.
///
/// The window is the compositor's own description of it: its `title` is what
/// names the accessibility frame, and its `geometry` is the origin AT-SPI
/// cannot supply.  Both halves are needed and neither source has both.
pub fn elements_for_window(window: &WindowCandidate) -> Option<Vec<RegionNode>> {
    if window.title.is_empty() {
        return None;
    }
    let accessibility = Accessibility::connect().ok()?;
    let frames = accessibility.frames().ok()?;
    let frame = frames.iter().find(|frame| frame.title == window.title)?;
    let origin = Point::new(window.geometry.left(), window.geometry.top());
    accessibility.elements(frame, origin).ok()
}

/// Keeps [`Result`] honest: this module is best-effort and returns `Option`s.
#[allow(dead_code)]
type Unused = Result<()>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degenerate_extents_are_not_regions() {
        // A hidden notebook page measured at 724708414 pixels wide, and
        // zero-sized nodes are the commonest kind of all.
        assert!(to_rect(0, 0, 0, 10).is_none());
        assert!(to_rect(0, 0, 10, -4).is_none());
        assert!(to_rect(0, 0, 724_708_414, 0).is_none());
        assert_eq!(to_rect(3, 4, 10, 20), Some(Rect::new(3, 4, 10, 20)));
    }

    /// Needs a live desktop: `cargo test -- --ignored a11y`.
    ///
    /// Asserts the two things that only a real window can show — that the
    /// accessibility bus is reachable and that a frame's title matches a
    /// compositor window's — and prints what it found so the geometry can be
    /// eyeballed against `hyprctl clients`.
    #[test]
    #[ignore = "needs a live desktop with accessibility enabled"]
    fn the_real_desktop_has_frames_with_titles() {
        let accessibility = match Accessibility::connect() {
            Ok(accessibility) => accessibility,
            Err(reason) => panic!(
                "cannot reach the accessibility bus: {reason}\n\
                 is it on?  busctl --user set-property org.a11y.Bus /org/a11y/bus \
                 org.a11y.Status IsEnabled b true"
            ),
        };
        let frames = accessibility.frames().expect("frames");
        assert!(!frames.is_empty(), "no frame at all: is any GTK/Qt app open?");

        let named: Vec<_> = frames.iter().filter(|frame| !frame.title.is_empty()).collect();
        assert!(!named.is_empty(), "every frame is unnamed");
        for frame in &named {
            let rect = frame
                .rect
                .map(|rect| {
                    format!("{}x{}", rect.size.width, rect.size.height)
                })
                .unwrap_or_else(|| "none".into());
            println!("frame {:?} size={rect}", frame.title);
        }

        // The compositor's own list holds only the windows it is showing, and
        // a window parked on a special workspace is not one of them.  What
        // this asserts is that the two descriptions of a window name the same
        // window, so it asks `hyprctl` directly rather than going through the
        // visibility filter.
        let titles = compositor_window_titles();
        for title in &titles {
            println!("  compositor window title {:?}", title);
        }
        if titles.is_empty() {
            println!("no compositor window list; skipping the match assertion");
            return;
        }
        let matched = named.iter().any(|frame| titles.contains(&frame.title));
        assert!(
            matched,
            "no compositor window title matches an accessibility frame title"
        );
    }

    /// Needs a live desktop: `cargo test -- --ignored a11y`.
    ///
    /// The real test of the composition: element rects must come out in global
    /// coordinates, inside their window — not left at the `0,0` AT-SPI reports.
    #[test]
    #[ignore = "needs a live desktop with accessibility enabled"]
    fn element_rects_are_global_and_inside_their_window() {
        let accessibility = Accessibility::connect().expect("connect");
        let frames = accessibility.frames().expect("frames");

        // `hyprctl clients` gives every window with its global position, which
        // is exactly what AT-SPI cannot supply.  Taking the geometry straight
        // from it means the composition is checked against the compositor's own
        // answer for the window the frame belongs to.
        let windows = compositor_windows();
        let mut checked = 0;
        for (title, geometry) in &windows {
            let Some(frame) = frames.iter().find(|frame| frame.title == *title) else {
                continue;
            };
            let origin = Point::new(geometry.left(), geometry.top());
            let elements = accessibility.elements(frame, origin).expect("elements");
            let flat = Flatten::flatten(&elements);
            println!("window {title:?} at {origin:?}: {} elements", flat.len());
            for element in flat {
                assert!(
                    element.rect.intersection(*geometry).is_some(),
                    "element {:?} at {:?} is outside its window {:?}",
                    element.label,
                    element.rect,
                    geometry
                );
                assert!(
                    element.rect.left() >= geometry.left()
                        && element.rect.top() >= geometry.top(),
                    "element {:?} at {:?} was not shifted into global \
                     coordinates (window starts at {:?})",
                    element.label,
                    element.rect,
                    origin
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no element was checked: no matching window");
    }

    /// Every window `hyprctl clients` reports, as `(title, geometry)`.
    ///
    /// Deliberately not [`crate::capture::window::ProcessWindowProvider`]:
    /// that list is filtered to the windows the compositor is *showing*, and a
    /// window on a special workspace is not among them — which would leave a
    /// perfectly good window unmatched in a test that only wants to know
    /// whether the two descriptions agree.
    fn compositor_windows() -> Vec<(String, Rect)> {
        let output = match std::process::Command::new("hyprctl")
            .args(["clients", "-j"])
            .output()
        {
            Ok(output) => output,
            Err(_) => return Vec::new(),
        };
        let clients = match serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .ok()
            .and_then(|value| value.as_array().cloned())
        {
            Some(clients) => clients,
            None => return Vec::new(),
        };
        clients
            .iter()
            .filter_map(|client| {
                let title = client.get("title")?.as_str()?.to_string();
                let at = client.get("at")?.as_array()?;
                let size = client.get("size")?.as_array()?;
                let x = at.first()?.as_i64()?;
                let y = at.get(1)?.as_i64()?;
                let width = size.first()?.as_u64()?;
                let height = size.get(1)?.as_u64()?;
                let rect = Rect::new(
                    i32::try_from(x).ok()?,
                    i32::try_from(y).ok()?,
                    u32::try_from(width).ok()?,
                    u32::try_from(height).ok()?,
                );
                Some((title, rect))
            })
            .collect()
    }

    fn compositor_window_titles() -> Vec<String> {
        compositor_windows().into_iter().map(|(title, _)| title).collect()
    }
}

/// Walks a node and everything under it, for callers that want the whole tree
/// flat — a test counting every element, say.
#[cfg(test)]
trait Flatten<'a> {
    fn flatten(&'a self) -> Vec<&'a RegionNode>;
}

#[cfg(test)]
impl<'a> Flatten<'a> for Vec<RegionNode> {
    fn flatten(&'a self) -> Vec<&'a RegionNode> {
        fn walk<'a>(node: &'a RegionNode, out: &mut Vec<&'a RegionNode>) {
            out.push(node);
            for child in &node.children {
                walk(child, out);
            }
        }
        let mut all = Vec::new();
        for node in self {
            walk(node, &mut all);
        }
        all
    }
}

