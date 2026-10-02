// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! Finding the elements inside a window by looking at what is drawn.
//!
//! The fallback source, and the last one asked.  It exists because most
//! programs that draw their own interface — a GPU-rendered editor, a QML shell,
//! a game's menu — expose no accessibility tree at all, so there is nothing to
//! read but the pixels.  It is asked last because it only ever *guesses*: it
//! recovers rectangles that look like controls, never what they are.
//!
//! Two ways of reading a window are implemented, and both are kept because
//! they fail on different interfaces:
//!
//! * [`lines`] finds the *drawn structure*.  A divider is a run of pixels that
//!   is long one way and thin the other, and the rectangles those dividers
//!   enclose are the panes of the interface.  This is what the picker uses by
//!   default: measured on an editor, it recovers the sidebar, the tab strip,
//!   the status bar and each tab, nested as a tree.
//! * [`components`] finds the *colour regions*.  A panel is an area of one
//!   colour, so connected areas of similar pixels are candidate regions.  It
//!   is what the source used to be, kept because it answers for interfaces
//!   that draw no dividers at all and separate their panes by colour alone —
//!   a chat client's sidebar against its conversation.
//!
//! Neither is a superset of the other, which is why both are here rather than
//! one replacing the other.
//!
//! What neither can do is worth stating, because it is why this is the
//! fallback: they have no names (every label is its size), and a busy region —
//! a photograph, a gradient, a page of text — is either shattered into small
//! boxes or merged into one, rather than read as the thing a person sees.

use crate::capture::window::WindowCandidate;
use crate::geometry::{Point, Rect};
use crate::model::SceneSnapshot;
use crate::selection_region::RegionNode;

use super::{ElementRequest, ElementSource, Fallback};

pub mod components;
pub mod lines;

/// The source the picker asks for elements, using the line reader.
///
/// Named for what it is rather than for how it works: `Pixels`, because from
/// the caller's side there is one pixel source and this is it.  The component
/// reader is [`components::Components`], registered beside it for the
/// interfaces this one cannot read.
pub struct Pixels;

impl ElementSource for Pixels {
    fn elements(&self, request: &ElementRequest<'_>) -> Option<Vec<RegionNode>> {
        let analysis = Analysis::of(request.scene, request.window)?;
        let roots = lines::regions(&analysis);
        if debug_enabled() {
            report(request.window, &analysis, &roots);
        }
        (!roots.is_empty()).then_some(roots)
    }
}

/// The pixel source that reads colour regions instead of lines.
///
/// Registered beside [`Pixels`] rather than instead of it: the two fail on
/// different interfaces, and neither is a superset of the other.  This one
/// answers where an interface draws no dividers and separates its panes by
/// colour alone.
pub struct Components;

impl ElementSource for Components {
    fn elements(&self, request: &ElementRequest<'_>) -> Option<Vec<RegionNode>> {
        let analysis = Analysis::of(request.scene, request.window)?;
        let roots = components::regions(&analysis);
        if debug_enabled() {
            report(request.window, &analysis, &roots);
        }
        (!roots.is_empty()).then_some(roots)
    }
}

/// Whether the pixel sources say what they found, for `VSHOT_PIXEL_DEBUG`.
///
/// The same switch the window-level pixel detector reads, so one variable turns
/// on everything that reads pixels.
pub(crate) fn debug_enabled() -> bool {
    std::env::var_os("VSHOT_PIXEL_DEBUG").is_some()
}

fn report(window: &WindowCandidate, analysis: &Analysis, roots: &[RegionNode]) {
    fn count(node: &RegionNode) -> usize {
        1 + node.children.iter().map(count).sum::<usize>()
    }
    let total: usize = roots.iter().map(count).sum();
    eprintln!(
        "vshot: element pixels on {:?} ({}x{} at {}x{}): analysis {}x{}, {} root(s), {} node(s)",
        window.label,
        window.geometry.size.width,
        window.geometry.size.height,
        window.geometry.left(),
        window.geometry.top(),
        analysis.width,
        analysis.height,
        roots.len(),
        total,
    );
    fn dump(node: &RegionNode, depth: usize) {
        eprintln!(
            "vshot:   {} {}x{}+{}+{}",
            "  ".repeat(depth),
            node.rect.size.width,
            node.rect.size.height,
            node.rect.left(),
            node.rect.top()
        );
        for child in &node.children {
            dump(child, depth + 1);
        }
    }
    for root in roots {
        dump(root, 0);
    }
}

/// One window's pixels, at its own resolution, in window-local coordinates.
pub(crate) struct Analysis {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The window's own global rectangle — the compositor's, which is what
    /// every local rect is shifted by and clipped to.
    pub(crate) window: Rect,
    /// Device pixels per analysis pixel.  Always 1 today: a divider is one or
    /// two pixels wide, and averaging it into a larger block turns a strong
    /// line into a weak ramp — which is the signal the whole reader depends on.
    /// Kept as a field so the coordinate conversion does not have to assume.
    pub(crate) factor: u32,
    /// Device pixels per logical pixel of the output the window is on.
    pub(crate) scale: u32,
    pub(crate) pixels: Vec<[u8; 4]>,
}

impl Analysis {
    /// Samples the part of the frozen frame the window covers.
    ///
    /// `None` when no output covers the window, or when what is left is too
    /// small to hold an element.
    pub(crate) fn of(scene: &SceneSnapshot, window: &WindowCandidate) -> Option<Self> {
        let bounds = window.geometry;
        if bounds.is_empty() {
            return None;
        }
        let output = scene
            .outputs()
            .iter()
            .find(|output| output.geometry.intersection(bounds).is_some())?;
        let scale = output.scale.max(1);

        // The window in the output's own device pixels, clipped to the frame.
        let source = output.frame.size();
        let to_device = |value: i32, base: i32| -> i64 {
            (i64::from(value) - i64::from(base)) * i64::from(scale)
        };
        let left = to_device(bounds.left(), output.geometry.left()).clamp(0, i64::from(source.width));
        let top = to_device(bounds.top(), output.geometry.top()).clamp(0, i64::from(source.height));
        let right =
            to_device(bounds.right().ok()?, output.geometry.left()).clamp(0, i64::from(source.width));
        let bottom =
            to_device(bounds.bottom().ok()?, output.geometry.top()).clamp(0, i64::from(source.height));
        let device_width = u32::try_from(right - left).ok()?;
        let device_height = u32::try_from(bottom - top).ok()?;
        // A window the compositor lists but the screen does not show — one
        // parked off the edge, or on a workspace that is not visible — has
        // almost nothing to analyse.
        if device_width < MIN_WINDOW_EDGE || device_height < MIN_WINDOW_EDGE {
            return None;
        }

        let factor = 1u32;
        let width = device_width;
        let height = device_height;
        let mut pixels = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            let row = top + i64::from(y) * i64::from(factor);
            for x in 0..width {
                let column = left + i64::from(x) * i64::from(factor);
                pixels.push(
                    output
                        .frame
                        .pixel(Point::new(
                            i32::try_from(column).ok()?,
                            i32::try_from(row).ok()?,
                        ))
                        .unwrap_or([0, 0, 0, 0]),
                );
            }
        }

        Some(Self {
            width,
            height,
            window: bounds,
            factor,
            scale,
            pixels,
        })
    }

    pub(crate) fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.pixels[(y * self.width + x) as usize]
    }

    /// A local analysis rect as a global logical one.
    ///
    /// An analysis pixel covers `factor` device pixels, and a logical pixel
    /// covers `scale` of them, so the two multiplications and the one division
    /// do not cancel: `analysis × factor ÷ scale` is the logical size.
    pub(crate) fn to_global(&self, rect: Rect) -> Option<Rect> {
        let scale = i64::from(self.scale).max(1);
        let factor = i64::from(self.factor);
        let to_logical = |value: i64| -> i64 { (value * factor + scale / 2) / scale };
        let x = to_logical(i64::from(rect.origin.x));
        let y = to_logical(i64::from(rect.origin.y));
        let width = to_logical(i64::from(rect.size.width)).max(1);
        let height = to_logical(i64::from(rect.size.height)).max(1);
        let global = Rect::new(
            self.window.origin.x + i32::try_from(x).ok()?,
            self.window.origin.y + i32::try_from(y).ok()?,
            u32::try_from(width).ok()?,
            u32::try_from(height).ok()?,
        );
        // Keep it inside the window: rounding at a scale other than 1 can push
        // an edge a pixel past where the window ends.
        global.clamp_to(self.window)
    }
}

/// A window smaller than this has nothing to analyse.
const MIN_WINDOW_EDGE: u32 = 100;

/// Rec. 709 luminance: the grey the eye would see.
///
/// Not a plain average of the channels — green carries most of the perceived
/// brightness, and two colours that average the same can look very different,
/// which is exactly the distinction a line reader needs.
pub(crate) fn luminance(pixel: [u8; 4]) -> f32 {
    0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2])
}

/// What the picker shows for a pixel-found element: its size, because the
/// pixels carry no name.  A person reads "240 × 32" and knows what they aimed
/// at; there is nothing more honest to say.
pub(crate) fn label_for(rect: Rect) -> String {
    format!("{} × {}", rect.size.width, rect.size.height)
}

/// One leaf region of the tree the picker walks.
pub(crate) fn leaf(rect: Rect) -> RegionNode {
    RegionNode::leaf(
        crate::selection_region::RegionKind::Element,
        rect,
        label_for(rect),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::window::WindowCandidate;
    use crate::geometry::Size;
    use crate::model::{Frame, OutputSnapshot, SceneSnapshot};

    /// A window filled with `background`, with each `(rect, colour)` painted on
    /// top.  Later entries are painted over earlier ones, the way a painter
    /// draws.
    pub(crate) fn scene_of(
        width: u32,
        height: u32,
        background: [u8; 4],
        painted: &[(Rect, [u8; 4])],
    ) -> (SceneSnapshot, WindowCandidate) {
        let size = Size::new(width, height);
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let point = Point::new(x as i32, y as i32);
                let color = painted
                    .iter()
                    .rev()
                    .find(|(rect, _)| rect.contains(point))
                    .map(|(_, paint)| *paint)
                    .unwrap_or(background);
                pixels.extend_from_slice(&color);
            }
        }
        let frame = Frame::new(size, pixels).expect("frame");
        let output =
            OutputSnapshot::new(1, "TEST", Rect::new(0, 0, width, height), 1, frame).expect("output");
        let scene = SceneSnapshot::from_outputs(vec![output]).expect("scene");
        let window = WindowCandidate {
            geometry: Rect::new(0, 0, width, height),
            label: String::new(),
            app_id: String::new(),
            title: "test".into(),
            pid: 0,
            handle: None,
        };
        (scene, window)
    }

    pub(crate) fn flatten(nodes: &[RegionNode]) -> Vec<&RegionNode> {
        fn walk<'a>(node: &'a RegionNode, out: &mut Vec<&'a RegionNode>) {
            out.push(node);
            for child in &node.children {
                walk(child, out);
            }
        }
        let mut all = Vec::new();
        for node in nodes {
            walk(node, &mut all);
        }
        all
    }

    fn depth(nodes: &[RegionNode]) -> usize {
        fn walk(node: &RegionNode) -> usize {
            1 + node.children.iter().map(walk).max().unwrap_or(0)
        }
        nodes.iter().map(walk).max().unwrap_or(0)
    }

    /// The line reader finds a drawn divider and the panes it separates.
    #[test]
    fn a_drawn_divider_separates_two_panes() {
        let (scene, window) = scene_of(
            1200,
            800,
            [30, 30, 30, 255],
            &[
                (Rect::new(0, 0, 600, 800), [40, 40, 40, 255]),
                // The divider itself.
                (Rect::new(598, 0, 2, 800), [200, 200, 200, 255]),
            ],
        );
        let roots = Pixels
            .elements(&ElementRequest {
                window: &window,
                scene: &scene,
                fallback: Fallback::default(),
            })
            .expect("a divider was drawn");
        let flat = flatten(&roots);
        assert!(
            flat.iter().any(|node| node.rect.size.width >= 500
                && node.rect.size.width <= 620
                && node.rect.size.height >= 700),
            "no pane among {:?}",
            flat.iter().map(|n| n.rect).collect::<Vec<_>>()
        );
    }

    /// A flat window has no lines in it, so the line reader declines.
    #[test]
    fn a_flat_window_yields_nothing() {
        let (scene, window) = scene_of(1200, 800, [30, 30, 30, 255], &[]);
        assert!(Pixels.elements(&ElementRequest {
                window: &window,
                scene: &scene,
                fallback: Fallback::default(),
            }).is_none());
    }

    /// The tree nests: a divider inside a pane makes a child, not a sibling.
    #[test]
    fn cutting_produces_a_nested_tree() {
        let (scene, window) = scene_of(
            1200,
            800,
            [30, 30, 30, 255],
            &[
                (Rect::new(0, 0, 600, 800), [40, 40, 40, 255]),
                (Rect::new(598, 0, 2, 800), [200, 200, 200, 255]),
                // A second divider, inside the left pane.
                (Rect::new(0, 398, 600, 2), [200, 200, 200, 255]),
            ],
        );
        let roots = Pixels
            .elements(&ElementRequest {
                window: &window,
                scene: &scene,
                fallback: Fallback::default(),
            })
            .expect("two dividers were drawn");
        assert!(depth(&roots) >= 2, "the cut tree is flat");
    }

    /// Text does not read as a line: a glyph stroke is long the wrong way.
    #[test]
    fn dense_text_does_not_make_dividers() {
        let mut painted = Vec::new();
        for row in 0..40u32 {
            let y = 8 + (row as i32) * 20;
            for column in 0..60u32 {
                painted.push((
                    Rect::new(8 + (column as i32) * 19, y, 12, 14),
                    [180, 180, 180, 255],
                ));
            }
        }
        let (scene, window) = scene_of(1200, 800, [16, 16, 20, 255], &painted);
        // No divider was drawn, so nothing may be found: a glyph is 12 wide and
        // 14 tall, and a line has to be long one way and thin the other.
        let found = Pixels.elements(&ElementRequest {
                window: &window,
                scene: &scene,
                fallback: Fallback::default(),
            });
        let count = found.as_ref().map(|roots| flatten(roots).len()).unwrap_or(0);
        assert!(count == 0, "text was read as structure: {count} region(s)");
    }

    /// Everything found is inside the window.
    #[test]
    fn every_region_is_inside_the_window() {
        let (scene, window) = scene_of(
            1200,
            800,
            [30, 30, 30, 255],
            &[
                (Rect::new(0, 0, 600, 800), [40, 40, 40, 255]),
                (Rect::new(598, 0, 2, 800), [200, 200, 200, 255]),
            ],
        );
        let roots = Pixels
            .elements(&ElementRequest {
                window: &window,
                scene: &scene,
                fallback: Fallback::default(),
            })
            .expect("a divider was drawn");
        for node in flatten(&roots) {
            assert!(
                window.geometry.intersection(node.rect).is_some(),
                "{:?} is outside the window",
                node.rect
            );
            assert_eq!(
                node.label,
                format!("{} × {}", node.rect.size.width, node.rect.size.height),
                "a pixel-found region is labelled by its size"
            );
        }
    }

    /// The two readers disagree about a window with no lines in it.
    ///
    /// This is the whole reason both are kept: the line reader has nothing to
    /// read, while the component reader still sees the colour step between the
    /// pane and its surround.
    #[test]
    fn the_readers_disagree_about_a_window_with_no_line() {
        let (scene, window) = scene_of(
            1280,
            900,
            [30, 30, 30, 255],
            &[
                (Rect::new(100, 100, 1080, 700), [80, 80, 90, 255]),
                // A control inside it, which is what the component reader is
                // for: a small uniform area in a differently-coloured surround.
                (Rect::new(200, 300, 300, 120), [200, 200, 200, 255]),
            ],
        );
        // The line reader may find the control's own edges — they are long and
        // thin like a divider, and that is a fair reading.  What matters here
        // is that the component reader finds the *control*, which is what it
        // is for.
        let found = Components
            .elements(&ElementRequest {
                window: &window,
                scene: &scene,
                fallback: Fallback::default(),
            })
            .expect("the component reader reads colour");
        let flat = flatten(&found);
        assert!(
            flat.iter().any(|node| node.rect.size.width >= 250
                && node.rect.size.width <= 350
                && node.rect.size.height >= 100
                && node.rect.size.height <= 150),
            "no control among {:?}",
            flat.iter().map(|n| n.rect).collect::<Vec<_>>()
        );
    }

    /// A window too small to hold anything is declined rather than guessed at.
    #[test]
    fn a_tiny_window_yields_nothing() {
        let (scene, window) = scene_of(4, 4, [30, 30, 30, 255], &[]);
        assert!(Pixels.elements(&ElementRequest {
                window: &window,
                scene: &scene,
                fallback: Fallback::default(),
            }).is_none());
        assert!(Components.elements(&ElementRequest {
                window: &window,
                scene: &scene,
                fallback: Fallback::default(),
            }).is_none());
    }
}
