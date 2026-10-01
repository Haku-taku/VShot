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
//! Three classical operations, coarse to fine:
//!
//! * **Run-length segmentation** of rows and columns into bands of constant
//!   colour.  A toolbar, a sidebar, a status bar and a list row are all
//!   rectangles of one colour, so a rectangle that is uniform along an edge is
//!   where a control ends.  This is the cheapest signal and the one that
//!   survives themes and scaling.
//! * **Connected components** over a colour-similarity predicate, which turns
//!   "these pixels look alike and touch" into candidate boxes.  This is what
//!   finds a button or a text field — a small uniform area sitting in a
//!   differently-coloured surround.
//! * **Containment**, which arranges the boxes into the tree the picker walks:
//!   a box inside another is its child, so the wheel can climb from a button to
//!   the panel holding it.
//!
//! What it cannot do is worth stating, because it is why this is the fallback:
//! it has no names (every label is its size), it cannot tell a button from a
//! decorative panel of the same shape, and a busy region — a photograph, a
//! gradient, a page of text — shatters into many small boxes rather than the
//! one control a person sees.

use crate::capture::window::WindowCandidate;
use crate::geometry::{Point, Rect};
use crate::model::SceneSnapshot;
use crate::selection_region::{RegionKind, RegionNode};

use super::{ElementRequest, ElementSource};

/// How many pixels of a window are analysed, at most.  The frame is downsampled
/// until it fits, which is what keeps a 4K window from costing 8M operations
/// per pass.
const MAX_ANALYSIS_PIXELS: u64 = 512 * 1024;

/// Two colours are "the same" for segmentation when every channel is within
/// this.  Loose enough to absorb a gradient or a subtle texture, tight enough
/// that a border line is not swallowed.
const SAME_COLOR: i32 = 12;

/// A candidate smaller than this is noise — an anti-aliased corner, a stray
/// glyph — rather than something a user could point at.
const MIN_ELEMENT_EDGE: u32 = 8;
const MIN_ELEMENT_AREA: u32 = 256;

/// How thick a full-width (or full-height) band has to be to count.
///
/// A band spans the window, so it is only interesting when it is a *division*
/// of it: a toolbar, a status bar, a sidebar.  A terminal's every line of text
/// is its own run, and without a floor those runs become a pile of strips that
/// look like elements on a window that has none.  Measured on a terminal, the
/// runs are one text line each — around 20 device pixels — so this sits above
/// them and below a real toolbar.
const MIN_BAND_THICKNESS: u32 = 48;

/// A candidate covering more of the window than this is the window's own
/// background, not a control inside it.
const MAX_ELEMENT_SHARE: f64 = 0.92;


/// The pixel source.
pub struct Pixels;

impl ElementSource for Pixels {
    /// The elements read out of the frame, or `None` when the window is not in
    /// the frame or nothing element-shaped was found.
    fn elements(&self, request: &ElementRequest<'_>) -> Option<Vec<RegionNode>> {
        let window = request.window;
        let analysis = Analysis::of(request.scene, window)?;
        let components = analysis.uniform_components();
        let bands = analysis.band_divisions();
        let candidates = analysis.candidates();
        if debug_enabled() {
            eprintln!(
                "vshot: element pixels on {:?} ({}x{} at {}x{}): analysis {}x{}, factor {}, \
                 {} component(s), {} band(s), {} candidate(s)",
                window.label,
                window.geometry.size.width,
                window.geometry.size.height,
                window.geometry.left(),
                window.geometry.top(),
                analysis.width,
                analysis.height,
                analysis.factor,
                components.len(),
                bands.len(),
                candidates.len(),
            );
            for rect in &candidates {
                eprintln!(
                    "vshot:   candidate {}x{}+{}+{}",
                    rect.size.width,
                    rect.size.height,
                    rect.left(),
                    rect.top()
                );
            }
        }
        if candidates.is_empty() {
            return None;
        }
        let tree = nest(candidates);
        (!tree.is_empty()).then_some(tree)
    }
}

/// Whether the element detector says what it found, for `VSHOT_PIXEL_DEBUG`.
///
/// The same switch the window-level pixel detector reads, so one variable turns
/// on everything that reads pixels.
fn debug_enabled() -> bool {
    std::env::var_os("VSHOT_PIXEL_DEBUG").is_some()
}

/// One window's pixels, downsampled, in window-local coordinates.
struct Analysis {
    width: u32,
    height: u32,
    /// The window's own global rectangle — the compositor's, which is what
    /// every local rect is shifted by and clipped to.  Kept whole rather than
    /// as an origin because the clip needs the size too: the analysis grid is
    /// the window rounded *up* to a whole number of analysis pixels, so it is
    /// regularly a pixel or two larger, and clipping to that would let a box
    /// hang over the window's edge.
    window: Rect,
    /// Device pixels per analysis pixel, so a local rect can be scaled back.
    factor: u32,
    /// Device pixels per logical pixel of the output the window is on.
    scale: u32,
    pixels: Vec<[u8; 4]>,
}

impl Analysis {
    /// Samples the part of the frozen frame the window covers.
    ///
    /// The window is cut out of the output it sits on and downsampled to
    /// `MAX_ANALYSIS_PIXELS`; `None` when no output covers it, or when what is
    /// left is too small to hold an element.
    fn of(scene: &SceneSnapshot, window: &WindowCandidate) -> Option<Self> {
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
        let right = to_device(bounds.right().ok()?, output.geometry.left())
            .clamp(0, i64::from(source.width));
        let bottom = to_device(bounds.bottom().ok()?, output.geometry.top())
            .clamp(0, i64::from(source.height));
        let device_width = u32::try_from(right - left).ok()?;
        let device_height = u32::try_from(bottom - top).ok()?;
        if device_width == 0 || device_height == 0 {
            return None;
        }

        let mut factor = 1u32;
        while u64::from(device_width.div_ceil(factor))
            * u64::from(device_height.div_ceil(factor))
            > MAX_ANALYSIS_PIXELS
        {
            factor += 1;
        }
        let width = device_width.div_ceil(factor).max(1);
        let height = device_height.div_ceil(factor).max(1);

        let mut pixels = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            let sy = top + i64::from(y * factor).min(i64::from(device_width) - 1);
            for x in 0..width {
                let sx = left + i64::from(x * factor).min(i64::from(device_width) - 1);
                pixels.push(
                    output
                        .frame
                        .pixel(Point::new(
                            i32::try_from(sx).ok()?,
                            i32::try_from(sy).ok()?,
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

    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.pixels[(y * self.width + x) as usize]
    }

    /// A local analysis rect as a global logical one.
    ///
    /// An analysis pixel covers `factor` device pixels, and a logical pixel
    /// covers `scale` of them, so the two multiplications and the one division
    /// do not cancel: `analysis × factor ÷ scale` is the logical size.
    fn to_global(&self, rect: Rect) -> Option<Rect> {
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

    /// Every element-shaped region the window's pixels yield, in global
    /// logical pixels.
    ///
    /// The passes below all work in analysis space — downsampled, window-local,
    /// in device pixels — so this is where they are scaled back to the
    /// coordinates the rest of picking uses.  A box that does not survive the
    /// conversion (it rounds away, or lands outside the window) is dropped.
    fn candidates(&self) -> Vec<Rect> {
        let mut found = Vec::new();
        found.extend(self.uniform_components());
        found.extend(self.band_divisions());
        self.filter(found)
            .into_iter()
            .filter_map(|rect| self.to_global(rect))
            .collect()
    }

    /// Connected components over a colour-similarity predicate.
    ///
    /// This is what finds a control: a button, a field, a badge is a small area
    /// of one colour inside a surround of another.  A four-way flood fill over
    /// the analysis grid, seeded from every unvisited pixel, with a neighbour
    /// joined when it is within [`SAME_COLOR`] of the seed.
    fn uniform_components(&self) -> Vec<Rect> {
        let count = (self.width * self.height) as usize;
        let mut seen = vec![false; count];
        let mut rects = Vec::new();
        let mut stack: Vec<u32> = Vec::new();

        for seed in 0..count {
            if seen[seed] {
                continue;
            }
            let seed_color = self.pixels[seed];
            seen[seed] = true;
            stack.clear();
            stack.push(seed as u32);
            let (mut min_x, mut min_y) = (self.width, self.height);
            let (mut max_x, mut max_y) = (0u32, 0u32);
            let mut members = 0u32;

            while let Some(index) = stack.pop() {
                let x = index % self.width;
                let y = index / self.width;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                members += 1;

                let mut push = |nx: u32, ny: u32, stack: &mut Vec<u32>| {
                    let at = (ny * self.width + nx) as usize;
                    if !seen[at] && close_enough(self.pixels[at], seed_color) {
                        seen[at] = true;
                        stack.push(at as u32);
                    }
                };
                if x > 0 {
                    push(x - 1, y, &mut stack);
                }
                if x + 1 < self.width {
                    push(x + 1, y, &mut stack);
                }
                if y > 0 {
                    push(x, y - 1, &mut stack);
                }
                if y + 1 < self.height {
                    push(x, y + 1, &mut stack);
                }
            }

            let width = max_x - min_x + 1;
            let height = max_y - min_y + 1;
            // A component that does not fill its own bounding box is not a
            // rectangle the user could point at — it is a scatter of similar
            // pixels, which is what a gradient or a photo produces.
            let box_area = u64::from(width) * u64::from(height);
            if u64::from(members) * 100 < box_area * 88 {
                continue;
            }
            rects.push(Rect::new(
                min_x as i32,
                min_y as i32,
                width,
                height,
            ));
        }
        rects
    }

    /// Bands the window's rows and columns divide into.
    ///
    /// A window is laid out as rectangles that span it: a toolbar across the
    /// top, a sidebar down one side, a status bar along the bottom.  Where a
    /// whole row (or column) is one colour and the next is another, a control
    /// ends.  This finds the large divisions that a per-pixel component pass
    /// misses, because a sidebar is rarely one flat colour.
    ///
    /// A band has to be *substantial* to count, in both directions:
    ///
    /// * shorter than the window along the axis it spans — a run covering every
    ///   row is the window's own background, and offering it would give the
    ///   user a box they cannot tell from the window;
    /// * not a sliver across the other axis — a run of text lines in a terminal
    ///   divides into dozens of thin bands, and none of them is a control.
    ///
    /// Without the second rule a terminal — which has no controls at all, only
    /// a grid of text — reports a handful of full-width strips that look like
    /// elements and are not.
    fn band_divisions(&self) -> Vec<Rect> {
        let mut rects = Vec::new();
        for (start, end) in self.uniform_runs(true) {
            let length = end - start;
            if length < MIN_BAND_THICKNESS || end >= self.height {
                continue;
            }
            rects.push(Rect::new(0, start as i32, self.width, length));
        }
        for (start, end) in self.uniform_runs(false) {
            let length = end - start;
            if length < MIN_BAND_THICKNESS || end >= self.width {
                continue;
            }
            rects.push(Rect::new(start as i32, 0, length, self.height));
        }
        rects
    }

    /// Runs of consecutive rows (or columns) whose *dominant* colour matches,
    /// as `(start, end)` pairs.  "Dominant" rather than "uniform": a toolbar
    /// with icons in it is still one band, and requiring every pixel to match
    /// would split it at the first glyph.
    fn uniform_runs(&self, by_row: bool) -> Vec<(u32, u32)> {
        let length = if by_row { self.height } else { self.width };
        let cross = if by_row { self.width } else { self.height };
        let mut runs = Vec::new();
        let mut start = 0u32;
        let mut previous: Option<[u8; 4]> = None;

        for index in 0..=length {
            let current = (index < length).then(|| {
                let mut counts: Vec<([u8; 4], u32)> = Vec::new();
                for other in 0..cross {
                    let color = if by_row {
                        self.at(other, index)
                    } else {
                        self.at(index, other)
                    };
                    match counts.iter_mut().find(|(known, _)| close_enough(*known, color)) {
                        Some((_, count)) => *count += 1,
                        None => counts.push((color, 1)),
                    }
                }
                counts
                    .into_iter()
                    .max_by_key(|(_, count)| *count)
                    .map(|(color, _)| color)
                    .unwrap_or([0, 0, 0, 0])
            });

            let same = match (previous, current) {
                (Some(before), Some(now)) => close_enough(before, now),
                (None, Some(_)) => true,
                _ => false,
            };
            if !same {
                if let Some(_) = previous {
                    runs.push((start, index));
                }
                start = index;
            }
            previous = current;
        }
        runs
    }

    /// Drops what cannot be a control: too small to point at, or so large it is
    /// the window's own background.
    ///
    /// Deliberately *not* a rule against a rectangle that spans the window
    /// along one axis.  A sidebar is exactly that — full height, a fraction of
    /// the width — and an editor's panes are too, so rejecting them would throw
    /// away the layout the user most wants to point at.  What makes a box the
    /// background is covering the window in *both* directions, which is what
    /// the area test already says.
    fn filter(&self, rects: Vec<Rect>) -> Vec<Rect> {
        let window_area = u64::from(self.width) * u64::from(self.height);
        let mut kept: Vec<Rect> = rects
            .into_iter()
            .filter(|rect| {
                let width = rect.size.width as u32;
                let height = rect.size.height as u32;
                if width < MIN_ELEMENT_EDGE || height < MIN_ELEMENT_EDGE {
                    return false;
                }
                if width * height < MIN_ELEMENT_AREA {
                    return false;
                }
                let area = u64::from(width) * u64::from(height);
                (area as f64) <= (window_area as f64) * MAX_ELEMENT_SHARE
            })
            .collect();
        // Overlapping boxes are the same control found twice; the larger is
        // kept, since a component split by an anti-aliased edge yields several
        // boxes inside the one real one.
        kept.sort_by_key(|rect| std::cmp::Reverse(rect.size.width * rect.size.height));
        let mut distinct: Vec<Rect> = Vec::new();
        for rect in kept {
            if distinct.iter().any(|kept| mostly_same(*kept, rect)) {
                continue;
            }
            distinct.push(rect);
        }
        distinct
    }
}

/// Whether two colours are the same for segmentation.
fn close_enough(a: [u8; 4], b: [u8; 4]) -> bool {
    (i32::from(a[0]) - i32::from(b[0])).abs() <= SAME_COLOR
        && (i32::from(a[1]) - i32::from(b[1])).abs() <= SAME_COLOR
        && (i32::from(a[2]) - i32::from(b[2])).abs() <= SAME_COLOR
        && (i32::from(a[3]) - i32::from(b[3])).abs() <= SAME_COLOR
}

/// Whether two boxes describe the same region: they cover nearly the same
/// ground, and are of comparable size.
///
/// The size test is what keeps a large box from swallowing a small one that
/// happens to sit inside it.  A panel and a button in it overlap completely —
/// by the overlap measure alone the button would look like a duplicate of the
/// panel and be dropped, which is how the first version of this lost every
/// control inside a container.
fn mostly_same(a: Rect, b: Rect) -> bool {
    let area = |rect: Rect| u64::from(rect.size.width) * u64::from(rect.size.height);
    let (small, large) = if area(a) <= area(b) { (a, b) } else { (b, a) };
    let small_area = area(small);
    let large_area = area(large);
    if small_area == 0 || large_area == 0 {
        return false;
    }
    // Comparable: neither is more than a fifth larger than the other.
    if large_area * 5 > small_area * 6 {
        return false;
    }
    let Some(overlap) = a.intersection(b) else {
        return false;
    };
    let overlap_area = u64::from(overlap.size.width) * u64::from(overlap.size.height);
    overlap_area * 100 >= small_area * 85
}

/// Arranges boxes into the tree the picker walks.
///
/// A box inside another is that box's child, so the wheel can climb from a
/// button to the panel that holds it.  Boxes are nested by area, largest
/// outward: each one is placed under the smallest box already placed that
/// contains it, or at the top when none does.
fn nest(mut rects: Vec<Rect>) -> Vec<RegionNode> {
    rects.sort_by_key(|rect| std::cmp::Reverse(u64::from(rect.size.width) * u64::from(rect.size.height)));
    rects.dedup_by(|a, b| *a == *b);

    let mut nodes: Vec<RegionNode> = rects
        .iter()
        .map(|rect| RegionNode::leaf(RegionKind::Element, *rect, label_for(*rect)))
        .collect();

    // parent[i] is the index of the smallest placed box that contains i.
    let mut parent: Vec<Option<usize>> = vec![None; nodes.len()];
    for index in 0..nodes.len() {
        let rect = nodes[index].rect;
        let mut best: Option<usize> = None;
        for other in 0..index {
            let outer = nodes[other].rect;
            if !contains(outer, rect) {
                continue;
            }
            let better = match best {
                None => true,
                Some(current) => {
                    let area = |r: Rect| u64::from(r.size.width) * u64::from(r.size.height);
                    area(outer) < area(nodes[current].rect)
                }
            };
            if better {
                best = Some(other);
            }
        }
        parent[index] = best;
    }

    // Build the tree bottom-up so each node keeps its own children in the order
    // they were found, then take the roots.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (index, owner) in parent.iter().enumerate() {
        match owner {
            Some(owner) => children[*owner].push(index),
            None => roots.push(index),
        }
    }
    fn build(index: usize, nodes: &mut Vec<RegionNode>, children: &[Vec<usize>]) -> RegionNode {
        let kids: Vec<RegionNode> = children[index]
            .iter()
            .map(|child| build(*child, nodes, children))
            .collect();
        let node = nodes[index].clone();
        node.with_children(kids)
    }
    roots
        .iter()
        .map(|root| build(*root, &mut nodes, &children))
        .collect()
}

/// Whether `outer` contains `inner` — not merely overlaps it.  A small tolerance
/// keeps a box from being its own parent's sibling over a rounding pixel.
fn contains(outer: Rect, inner: Rect) -> bool {
    const SLACK: i32 = 2;
    outer.left() - SLACK <= inner.left()
        && outer.top() - SLACK <= inner.top()
        && outer.right().map(|r| r + SLACK).unwrap_or(i32::MAX) >= inner.right().unwrap_or(i32::MIN)
        && outer.bottom().map(|b| b + SLACK).unwrap_or(i32::MAX) >= inner.bottom().unwrap_or(i32::MIN)
        && outer != inner
}

/// What the picker shows for a pixel-found element: its size, because the
/// pixels carry no name.  A person reads "240 × 32" and knows what they aimed
/// at; there is nothing more honest to say.
fn label_for(rect: Rect) -> String {
    format!("{} × {}", rect.size.width, rect.size.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::window::WindowCandidate;
    use crate::geometry::Size;
    use crate::model::{Frame, OutputSnapshot, SceneSnapshot};

    /// A window filled with `background`, with each `(rect, colour)` painted on
    /// top.  Coordinates are window-local device pixels.
    ///
    /// Later entries are painted over earlier ones, the way a painter draws:
    /// the list is walked in reverse so the last rect covering a pixel is the
    /// one that shows.  Getting this the other way round silently hides every
    /// control behind the panel that was drawn first.
    fn scene_of(
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
        let output = OutputSnapshot::new(1, "TEST", Rect::new(0, 0, width, height), 1, frame)
            .expect("output");
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

    fn find(scene: &SceneSnapshot, window: &WindowCandidate) -> Option<Vec<RegionNode>> {
        Pixels.elements(&ElementRequest { window, scene })
    }

    fn flatten(nodes: &[RegionNode]) -> Vec<&RegionNode> {
        let mut all = Vec::new();
        fn walk<'a>(node: &'a RegionNode, all: &mut Vec<&'a RegionNode>) {
            all.push(node);
            for child in &node.children {
                walk(child, all);
            }
        }
        for node in nodes {
            walk(node, &mut all);
        }
        all
    }

    /// A button is a small uniform rectangle in a differently-coloured window.
    #[test]
    fn a_button_in_a_plain_window_is_found() {
        let (scene, window) = scene_of(
            400,
            300,
            [30, 30, 30, 255],
            &[(Rect::new(40, 40, 120, 36), [200, 200, 200, 255])],
        );
        let elements = find(&scene, &window).expect("something found");
        let flat = flatten(&elements);
        assert!(
            flat.iter().any(|node| node.rect.size.width >= 100
                && node.rect.size.width <= 140
                && node.rect.size.height >= 28
                && node.rect.size.height <= 44),
            "no button-sized region among {:?}",
            flat.iter().map(|n| n.rect).collect::<Vec<_>>()
        );
    }

    /// A uniform window has no elements in it — the whole thing is one colour,
    /// which is the background, not a control.
    #[test]
    fn a_flat_window_yields_nothing() {
        let (scene, window) = scene_of(400, 300, [30, 30, 30, 255], &[]);
        assert!(find(&scene, &window).is_none());
    }

    /// Two buttons side by side are two elements, not one.
    #[test]
    fn two_separated_buttons_are_two_elements() {
        let (scene, window) = scene_of(
            400,
            300,
            [30, 30, 30, 255],
            &[
                (Rect::new(40, 40, 100, 36), [200, 200, 200, 255]),
                (Rect::new(200, 40, 100, 36), [200, 200, 200, 255]),
            ],
        );
        let elements = find(&scene, &window).expect("something found");
        let buttons = flatten(&elements)
            .into_iter()
            .filter(|node| node.rect.size.height <= 60 && node.rect.size.width <= 140)
            .count();
        assert!(buttons >= 2, "expected two buttons, found {buttons}");
    }

    /// A panel with a control inside it nests: the picker can climb from the
    /// control to the panel.
    #[test]
    fn a_control_inside_a_panel_becomes_its_child() {
        let (scene, window) = scene_of(
            400,
            300,
            [30, 30, 30, 255],
            &[
                (Rect::new(20, 20, 360, 200), [60, 60, 60, 255]),
                (Rect::new(50, 50, 100, 36), [200, 200, 200, 255]),
            ],
        );
        let elements = find(&scene, &window).expect("something found");
        // The button has to sit under something, or the wheel could not climb
        // out of it — which is the whole point of building a tree.
        let button = flatten(&elements)
            .into_iter()
            .find(|node| {
                node.rect.size.width <= 140
                    && node.rect.size.height <= 60
                    && node.rect.size.width >= 80
            })
            .expect("the button");
        let button_rect = button.rect;
        let nested_under_something = flatten(&elements).into_iter().any(|node| {
            node.children.iter().any(|child| child.rect == button_rect)
        });
        assert!(
            nested_under_something,
            "the button {:?} is not a child of any panel",
            button_rect
        );
    }

    /// Everything found is inside the window, and labelled by its size — the
    /// pixels carry no names.
    #[test]
    fn every_element_is_inside_the_window_and_sized_by_its_label() {
        let (scene, window) = scene_of(
            400,
            300,
            [30, 30, 30, 255],
            &[(Rect::new(40, 40, 120, 36), [200, 200, 200, 255])],
        );
        let elements = find(&scene, &window).expect("something found");
        for node in flatten(&elements) {
            assert!(
                window.geometry.intersection(node.rect).is_some(),
                "{:?} is outside the window",
                node.rect
            );
            assert_eq!(
                node.label,
                format!("{} × {}", node.rect.size.width, node.rect.size.height),
                "a pixel-found element is labelled by its size"
            );
        }
    }


    /// An editor-shaped window: a sidebar down one side, a tab strip and a
    /// status bar across it, and a button in the sidebar.  This is the layout
    /// the pixel source exists for — a program that draws its own UI, so there
    /// is no accessibility tree to read, but the panels are plain rectangles.
    #[test]
    fn an_editor_layout_yields_its_panels() {
        let (scene, window) = scene_of(
            1200,
            800,
            [24, 24, 28, 255],
            &[
                // Tab strip across the top.
                (Rect::new(0, 0, 1200, 40), [40, 40, 46, 255]),
                // Sidebar down the left.
                (Rect::new(0, 40, 240, 720), [32, 32, 38, 255]),
                // Status bar along the bottom.
                (Rect::new(0, 760, 1200, 40), [48, 48, 56, 255]),
                // A control inside the sidebar.
                (Rect::new(20, 60, 200, 32), [70, 70, 80, 255]),
            ],
        );
        let elements = find(&scene, &window).expect("an editor has regions");
        let flat = flatten(&elements);
        let has = |width: u32, height: u32| {
            flat.iter().any(|node| {
                node.rect.size.width >= width.saturating_sub(24)
                    && node.rect.size.width <= width + 24
                    && node.rect.size.height >= height.saturating_sub(24)
                    && node.rect.size.height <= height + 24
            })
        };
        // The sidebar's column runs the window's full height — the tab strip
        // and status bar are on it too — so what is asserted is the column, not
        // the exact extent the sidebar was painted with.
        let has_sidebar = flat.iter().any(|node| {
            node.rect.size.width >= 200
                && node.rect.size.width <= 280
                && node.rect.size.height >= 600
        });
        assert!(has_sidebar, "no sidebar column among {:?}",
                flat.iter().map(|n| n.rect).collect::<Vec<_>>());
        assert!(has(200, 32), "no control inside the sidebar");
    }

    /// A terminal has no controls at all — one text grid filling the window.
    /// The source has to stay quiet rather than report the text rows as
    /// elements, which is what a user sees as "it found eight things and none
    /// of them is real".
    #[test]
    fn a_terminal_yields_almost_nothing() {
        // A dark background with a grid of lighter glyph rows, the way a
        // terminal actually looks.
        let mut painted = Vec::new();
        for row in 0..40u32 {
            painted.push((
                Rect::new(8, 8 + (row as i32) * 20, 1180, 14),
                [180, 180, 180, 255],
            ));
        }
        let (scene, window) = scene_of(1200, 800, [16, 16, 20, 255], &painted);
        let found = find(&scene, &window);
        let count = found.as_ref().map(|e| flatten(e).len()).unwrap_or(0);
        assert!(
            count <= 2,
            "a terminal has no elements, but {count} were reported"
        );
    }

    /// A window too small to hold an element yields nothing rather than a
    /// degenerate box.
    #[test]
    fn a_tiny_window_yields_nothing() {
        let (scene, window) = scene_of(4, 4, [30, 30, 30, 255], &[]);
        assert!(find(&scene, &window).is_none());
    }
}
