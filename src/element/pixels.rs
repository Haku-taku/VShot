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

/// Two colours are "the same" for a component when every channel is within
/// this.  Loose enough to absorb a gradient or a subtle texture, tight enough
/// that a border line is not swallowed.
const SAME_COLOR: i32 = 12;

/// A colour change of at least this in one channel is an edge.
///
/// Measured on a real window, the separators between its panes clear 32 and the
/// answer is the same at 8, 16 and 32 — the lines that divide an interface are
/// drawn to be seen, so the exact value is not delicate.  It is set above the
/// dithering and gradient noise of a themed background, and below a real line.
const EDGE_THRESHOLD: i32 = 16;

/// How much of a line has to show an edge before it is a division of the region
/// rather than a detail inside it.
///
/// Not 1.0: a separator interrupted by the text it separates is still the
/// separator.  Not low either, or the ragged edge of a paragraph would read as
/// a cut.
const CUT_FRACTION: f64 = 0.75;

/// The smallest region worth offering or cutting.
///
/// Below this a region is a glyph or an anti-aliased corner rather than
/// something a person would point at.
const MIN_REGION_EDGE: u32 = 24;

/// How many regions one window may be divided into.
///
/// The recursion is bounded by `MIN_REGION_EDGE` already, but a window of
/// pathological detail could still produce thousands of tiny regions, and the
/// picker has no use for them.  This stops the walk rather than the division.
const MAX_REGIONS: usize = 256;


/// The pixel source.
pub struct Pixels;

impl ElementSource for Pixels {
    /// The elements read out of the frame, or `None` when the window is not in
    /// the frame or nothing element-shaped was found.
    fn elements(&self, request: &ElementRequest<'_>) -> Option<Vec<RegionNode>> {
        let window = request.window;
        let analysis = Analysis::of(request.scene, window)?;
        let regions = analysis.candidates();
        if debug_enabled() {
            eprintln!(
                "vshot: element pixels on {:?} ({}x{} at {}x{}): analysis {}x{}, factor {}, \
                 {} region(s)",
                window.label,
                window.geometry.size.width,
                window.geometry.size.height,
                window.geometry.left(),
                window.geometry.top(),
                analysis.width,
                analysis.height,
                analysis.factor,
                regions.len(),
            );
            for rect in &regions {
                eprintln!(
                    "vshot:   region {}x{}+{}+{}",
                    rect.size.width,
                    rect.size.height,
                    rect.left(),
                    rect.top()
                );
            }
        }
        if regions.is_empty() {
            return None;
        }
        let tree = nest(regions);
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

        // Box-averaged rather than point-sampled.  A separator line is one or
        // two pixels wide, and taking one pixel per block would drop it from
        // most rows — which is exactly the signal the edge pass below looks
        // for.  Averaging turns it into a ramp instead, which still reads as a
        // gradient.
        let mut pixels = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            let block_top = top + i64::from(y) * i64::from(factor);
            let block_bottom = (block_top + i64::from(factor)).min(top + i64::from(device_height));
            for x in 0..width {
                let block_left = left + i64::from(x) * i64::from(factor);
                let block_right = (block_left + i64::from(factor)).min(left + i64::from(device_width));
                let mut sum = [0u32; 4];
                let mut count = 0u32;
                for sy in block_top..block_bottom {
                    for sx in block_left..block_right {
                        let pixel = output
                            .frame
                            .pixel(Point::new(
                                i32::try_from(sx).ok()?,
                                i32::try_from(sy).ok()?,
                            ))
                            .unwrap_or([0, 0, 0, 0]);
                        for channel in 0..4 {
                            sum[channel] += u32::from(pixel[channel]);
                        }
                        count += 1;
                    }
                }
                if count == 0 {
                    pixels.push([0, 0, 0, 0]);
                    continue;
                }
                let mut averaged = [0u8; 4];
                for channel in 0..4 {
                    averaged[channel] = u8::try_from(sum[channel] / count).unwrap_or(0);
                }
                pixels.push(averaged);
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

    /// The window's regions: the panes its lines divide it into, and the
    /// controls inside those panes.
    ///
    /// Two passes, because the two things a user points at are found in
    /// different ways and neither pass finds both:
    ///
    /// * **Recursive XY-cut** finds the *structure*.  A pane is bounded by
    ///   lines that cross it, and cutting at the strongest such line, over and
    ///   over, recovers the layout: a sidebar, a toolbar, the panes of an
    ///   editor.  Measured on a real window, its dividers come out at the same
    ///   places for any threshold between 8 and 32.
    /// * **Connected components** finds the *controls*.  A button is a small
    ///   area of one colour inside a differently-coloured surround, and its
    ///   edges are only as tall as the button — they cross nothing, so the cut
    ///   pass cannot see them at all.  Run inside each region the cut produced,
    ///   which is what keeps it from drowning in the whole window's detail.
    ///
    /// A window with no lines yields nothing from the first pass and its
    /// controls from the second; a window of nothing but panes yields the panes
    /// and no controls.  Both are the right answer for what they describe.
    fn candidates(&self) -> Vec<Rect> {
        let edges = self.edges();
        let mut regions = Vec::new();
        self.cut(0, 0, self.width, self.height, &edges, 0, &mut regions);
        let regions = drop_text_flow(regions);
        let mut found = regions.clone();
        for region in &regions {
            let controls = self.controls_in(*region);
            if debug_enabled() && !controls.is_empty() {
                eprintln!(
                    "vshot:   controls in {}x{}+{}+{}: {}",
                    region.size.width, region.size.height, region.left(), region.top(),
                    controls.len()
                );
            }
            found.extend(controls);
        }
        found
            .into_iter()
            .filter_map(|rect| self.to_global(rect))
            .filter(|rect| {
                // Judged in global logical pixels, not in the analysis grid:
                // what a control is worth pointing at is a property of the
                // screen, and the grid is downsampled by a factor that differs
                // per window.  Filtering in analysis space would drop a
                // perfectly good button on a large window and keep it on a
                // small one.
                if rect.size.width < MIN_REGION_EDGE || rect.size.height < MIN_REGION_EDGE {
                    return false;
                }
                // The window itself is not an element inside it.  A cut that
                // found no line anywhere leaves exactly that, and offering it
                // would give the user a box they cannot tell from the window.
                !(rect.size.width == self.window.size.width
                    && rect.size.height == self.window.size.height)
            })
            .collect()
    }

    /// The controls inside one region: connected areas of a single colour.
    ///
    /// A four-way flood fill over the region, seeded from every unvisited
    /// pixel, joining a neighbour when its colour is within [`SAME_COLOR`] of
    /// the seed.  A component that does not fill its own bounding box is
    /// dropped — it is a scatter of similar pixels, which is what a gradient or
    /// a photograph produces, not a rectangle anyone could point at.
    fn controls_in(&self, region: Rect) -> Vec<Rect> {
        let x0 = region.origin.x.max(0) as u32;
        let y0 = region.origin.y.max(0) as u32;
        let x1 = (x0 + region.size.width).min(self.width);
        let y1 = (y0 + region.size.height).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return Vec::new();
        }
        let width = x1 - x0;
        let height = y1 - y0;
        let mut seen = vec![false; (width * height) as usize];
        let index = |x: u32, y: u32| ((y - y0) * width + (x - x0)) as usize;

        let mut rects = Vec::new();
        let mut stack: Vec<(u32, u32)> = Vec::new();
        for sy in y0..y1 {
            for sx in x0..x1 {
                if seen[index(sx, sy)] {
                    continue;
                }
                let seed = self.at(sx, sy);
                seen[index(sx, sy)] = true;
                stack.clear();
                stack.push((sx, sy));
                let (mut min_x, mut min_y) = (sx, sy);
                let (mut max_x, mut max_y) = (sx, sy);
                let mut members = 0u32;

                while let Some((x, y)) = stack.pop() {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                    members += 1;
                    let mut push = |nx: u32, ny: u32, stack: &mut Vec<(u32, u32)>| {
                        let at = index(nx, ny);
                        if !seen[at] && close_enough(self.at(nx, ny), seed) {
                            seen[at] = true;
                            stack.push((nx, ny));
                        }
                    };
                    if x > x0 {
                        push(x - 1, y, &mut stack);
                    }
                    if x + 1 < x1 {
                        push(x + 1, y, &mut stack);
                    }
                    if y > y0 {
                        push(x, y - 1, &mut stack);
                    }
                    if y + 1 < y1 {
                        push(x, y + 1, &mut stack);
                    }
                }

                let box_width = max_x - min_x + 1;
                let box_height = max_y - min_y + 1;
                let box_area = u64::from(box_width) * u64::from(box_height);
                if u64::from(members) * 100 < box_area * 88 {
                    continue;
                }
                rects.push(Rect::new(
                    min_x as i32,
                    min_y as i32,
                    box_width,
                    box_height,
                ));
            }
        }
        rects
    }

    /// Splits `(x0, y0, x1, y1)` at its strongest divider and recurses, or
    /// records the region when it has none.
    ///
    /// Depth-first and in place: the region is pushed only when it cannot be
    /// split again, so the list comes out with every region and no ancestors —
    /// the tree is rebuilt from the rectangles by `nest`, which reads the same
    /// containment the split produced.
    fn cut(
        &self,
        x0: u32,
        y0: u32,
        x1: u32,
        y1: u32,
        edges: &Edges,
        depth: u32,
        out: &mut Vec<Rect>,
    ) {
        if out.len() >= MAX_REGIONS {
            return;
        }
        // A region too small to hold a control, or to be split in two, is one.
        if x1.saturating_sub(x0) < MIN_REGION_EDGE * 2 || y1.saturating_sub(y0) < MIN_REGION_EDGE * 2 {
            out.push(Rect::new(
                x0 as i32,
                y0 as i32,
                x1 - x0,
                y1 - y0,
            ));
            return;
        }
        match edges.strongest(x0, y0, x1, y1) {
            Some((Axis::Vertical, at)) => {
                self.cut(x0, y0, at, y1, edges, depth + 1, out);
                self.cut(at, y0, x1, y1, edges, depth + 1, out);
            }
            Some((Axis::Horizontal, at)) => {
                self.cut(x0, y0, x1, at, edges, depth + 1, out);
                self.cut(x0, at, x1, y1, edges, depth + 1, out);
            }
            None => out.push(Rect::new(
                x0 as i32,
                y0 as i32,
                x1 - x0,
                y1 - y0,
            )),
        }
    }

    /// The per-column and per-row edge strengths of the whole analysis grid.
    ///
    /// Computed once for the window rather than per region: the strength of a
    /// line is a property of the pixels, and asking again inside every
    /// sub-region would walk the same pixels over and over.  A region's cut
    /// then reads a slice of this and scales it to the region's own length.
    fn edges(&self) -> Edges {
        let mut vertical = vec![0u32; self.width as usize];
        let mut horizontal = vec![0u32; self.height as usize];
        for x in 1..self.width {
            let mut count = 0;
            for y in 0..self.height {
                if differs(self.at(x, y), self.at(x - 1, y)) {
                    count += 1;
                }
            }
            vertical[x as usize] = count;
        }
        for y in 1..self.height {
            let mut count = 0;
            for x in 0..self.width {
                if differs(self.at(x, y), self.at(x, y - 1)) {
                    count += 1;
                }
            }
            horizontal[y as usize] = count;
        }
        Edges {
            vertical,
            horizontal,
        }
    }
}

/// Drops the runs of a window that are a flow of text rather than a layout.
///
/// A terminal is the clearest case: every line of text is its own run, and the
/// cut pass divides the window into one strip per line.  A file list down a
/// sidebar is the same shape turned on its side.  None of those strips is
/// something a user would point at — the region has no controls at all — and
/// reporting a dozen of them that look like elements is worse than reporting
/// nothing.
///
/// The test is shape, not content: a *run* of regions that all span the same
/// extent along one axis, sit against the same edge, and are about the same
/// thickness is a flow.  A real layout has few such strips — a toolbar, a
/// status bar — and they differ in size, so the count and the uniformity
/// together tell the two apart.
///
/// Runs are found along both axes and dropped together: a sidebar of file rows
/// is as much a text flow as a terminal's lines, and the two appear in the same
/// window.
fn drop_text_flow(regions: Vec<Rect>) -> Vec<Rect> {
    let flows = text_flows(&regions);
    if flows.is_empty() {
        return regions;
    }
    regions
        .into_iter()
        .filter(|rect| {
            !flows.iter().any(|(axis, span, edge)| match axis {
                Axis::Horizontal => rect.size.width == *span && rect.origin.x == *edge,
                Axis::Vertical => rect.size.height == *span && rect.origin.y == *edge,
            })
        })
        .collect()
}

/// Every run of strips that reads as a text flow, as `(extent, edge)` pairs to
/// drop, along both axes.
///
/// Grouped by extent rather than looking only at the widest: a terminal's lines
/// span the window, but a sidebar's file rows span only the sidebar, and both
/// are text.  A group is a flow when it holds enough strips that sit against
/// the same edge and are about the same thickness.
fn text_flows(regions: &[Rect]) -> Vec<(Axis, u32, i32)> {
    const STRIPS_BEFORE_TEXT: usize = 4;
    // A ratio rather than a pixel count: a flow's strips are all about one line
    // thick, and "about" has to hold at any scale.  A layout's few strips
    // differ from each other by far more than this.
    const SAME_THICKNESS_RATIO: f64 = 1.6;

    let mut flows = Vec::new();
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let (span_of, edge_of, thickness_of): (
            fn(&Rect) -> u32,
            fn(&Rect) -> i32,
            fn(&Rect) -> u32,
        ) = match axis {
            Axis::Horizontal => (
                |rect: &Rect| rect.size.width,
                |rect: &Rect| rect.origin.x,
                |rect: &Rect| rect.size.height,
            ),
            Axis::Vertical => (
                |rect: &Rect| rect.size.height,
                |rect: &Rect| rect.origin.y,
                |rect: &Rect| rect.size.width,
            ),
        };

        // (extent, edge) -> thicknesses of the strips there.
        let mut groups: Vec<((u32, i32), Vec<u32>)> = Vec::new();
        for rect in regions {
            let key = (span_of(rect), edge_of(rect));
            let thickness = thickness_of(rect);
            match groups.iter_mut().find(|(known, _)| *known == key) {
                Some((_, thicknesses)) => thicknesses.push(thickness),
                None => groups.push((key, vec![thickness])),
            }
        }

        for ((span, edge), mut thicknesses) in groups {
            if thicknesses.len() < STRIPS_BEFORE_TEXT {
                continue;
            }
            // The *median* thickness, not the extremes: a flow's last strip is
            // routinely thicker than the rest — it is clipped by the window's
            // edge — and a rule that read the extremes would call that a layout.
            thicknesses.sort_unstable();
            let median = thicknesses[thicknesses.len() / 2];
            if median == 0 {
                continue;
            }
            let alike = thicknesses
                .iter()
                .filter(|thickness| {
                    let ratio = f64::from(**thickness) / f64::from(median);
                    (1.0 / SAME_THICKNESS_RATIO..=SAME_THICKNESS_RATIO).contains(&ratio)
                })
                .count();
            // Most of them alike, rather than all: one clipped strip must not
            // save the rest from being recognized as text.
            if alike * 2 >= thicknesses.len() {
                flows.push((axis, span, edge));
            }
        }
    }
    flows
}

/// Whether two colours are the same for a component.
fn close_enough(a: [u8; 4], b: [u8; 4]) -> bool {
    let channel = |i: usize| (i32::from(a[i]) - i32::from(b[i])).abs();
    channel(0) <= SAME_COLOR
        && channel(1) <= SAME_COLOR
        && channel(2) <= SAME_COLOR
        && channel(3) <= SAME_COLOR
}

/// Whether two pixels differ enough to be an edge between them.
fn differs(a: [u8; 4], b: [u8; 4]) -> bool {
    let channel = |i: usize| (i32::from(a[i]) - i32::from(b[i])).abs();
    // Alpha is deliberately not compared: a window's own alpha is uniform
    // where it matters, and an opaque background under a translucent overlay
    // would otherwise read as an edge everywhere.
    channel(0).max(channel(1)).max(channel(2)) >= EDGE_THRESHOLD
}

/// Which way a cut runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    /// A vertical line: the region is split left and right of it.
    Vertical,
    /// A horizontal line: split above and below.
    Horizontal,
}

/// How strongly each column and row of the window is an edge.
///
/// `vertical[x]` counts the rows where column `x` differs from `x - 1`, and
/// `horizontal[y]` the columns where row `y` differs from `y - 1`.  A divider
/// shows up as a column (or row) with a count near the region's length.
struct Edges {
    vertical: Vec<u32>,
    horizontal: Vec<u32>,
}

impl Edges {
    /// The strongest line crossing the region, as `(axis, position)`.
    ///
    /// The position is where the region is split: the far side of the line for
    /// a vertical cut, so the line itself stays with the left-hand region
    /// rather than being lost between the two.
    ///
    /// Ties go to the earlier position, which keeps the walk deterministic.  A
    /// line that only covers part of the region is scaled to the region's own
    /// length, so the same divider reads the same inside a pane as it does in
    /// the window.
    fn strongest(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> Option<(Axis, u32)> {
        let height = f64::from(y1 - y0).max(1.0);
        let width = f64::from(x1 - x0).max(1.0);
        let mut best: Option<(f64, Axis, u32)> = None;

        for x in (x0 + MIN_REGION_EDGE)..(x1.saturating_sub(MIN_REGION_EDGE)) {
            let count = self.vertical.get(x as usize).copied().unwrap_or(0);
            // A line has to cross the whole region, and the count is for the
            // whole window: a divider that spans the region but not the window
            // is scaled up, one that spans the window but not the region is
            // scaled down.
            let fraction = f64::from(count) / height;
            if fraction < CUT_FRACTION {
                continue;
            }
            if best.is_none_or(|(score, _, _)| fraction > score) {
                best = Some((fraction, Axis::Vertical, x));
            }
        }
        for y in (y0 + MIN_REGION_EDGE)..(y1.saturating_sub(MIN_REGION_EDGE)) {
            let count = self.horizontal.get(y as usize).copied().unwrap_or(0);
            let fraction = f64::from(count) / width;
            if fraction < CUT_FRACTION {
                continue;
            }
            if best.is_none_or(|(score, _, _)| fraction > score) {
                best = Some((fraction, Axis::Horizontal, y));
            }
        }
        best.map(|(_, axis, at)| (axis, at))
    }
}

/// Arranges regions into the tree the picker walks.
///
/// A region inside another is that region's child, so the wheel can climb from
/// a control to the pane holding it.  Regions are nested by area, largest
/// outward: each is placed under the smallest already placed that contains it,
/// or at the top when none does.
///
/// The split already produced this containment — a cut's halves are inside the
/// region it split — but the recursion flattens it away, and rebuilding it
/// here is cheaper than threading a tree through the walk.
fn nest(mut rects: Vec<Rect>) -> Vec<RegionNode> {
    rects.sort_by_key(|rect| {
        std::cmp::Reverse(u64::from(rect.size.width) * u64::from(rect.size.height))
    });
    rects.dedup();

    let nodes: Vec<RegionNode> = rects
        .iter()
        .map(|rect| RegionNode::leaf(RegionKind::Element, *rect, label_for(*rect)))
        .collect();

    let area = |rect: Rect| u64::from(rect.size.width) * u64::from(rect.size.height);
    let mut parent: Vec<Option<usize>> = vec![None; nodes.len()];
    for index in 0..nodes.len() {
        let rect = nodes[index].rect;
        let mut best: Option<usize> = None;
        for other in 0..index {
            if !contains(nodes[other].rect, rect) {
                continue;
            }
            if best.is_none_or(|current| area(nodes[other].rect) < area(nodes[current].rect)) {
                best = Some(other);
            }
        }
        parent[index] = best;
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (index, owner) in parent.iter().enumerate() {
        match owner {
            Some(owner) => children[*owner].push(index),
            None => roots.push(index),
        }
    }
    fn build(index: usize, nodes: &[RegionNode], children: &[Vec<usize>]) -> RegionNode {
        let kids: Vec<RegionNode> = children[index]
            .iter()
            .map(|child| build(*child, nodes, children))
            .collect();
        nodes[index].clone().with_children(kids)
    }
    roots
        .iter()
        .map(|root| build(*root, &nodes, &children))
        .collect()
}

/// Whether `outer` contains `inner` — not merely overlaps it.  A small
/// tolerance keeps a box from becoming its own sibling over a rounding pixel.
fn contains(outer: Rect, inner: Rect) -> bool {
    const SLACK: i32 = 2;
    outer != inner
        && outer.left() - SLACK <= inner.left()
        && outer.top() - SLACK <= inner.top()
        && outer.right().map(|r| r + SLACK).unwrap_or(i32::MAX) >= inner.right().unwrap_or(i32::MIN)
        && outer.bottom().map(|b| b + SLACK).unwrap_or(i32::MAX) >= inner.bottom().unwrap_or(i32::MIN)
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
                (Rect::new(20, 20, 360, 200), [80, 80, 90, 255]),
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
                (Rect::new(0, 0, 1200, 40), [72, 72, 84, 255]),
                // Sidebar down the left.
                (Rect::new(0, 40, 240, 720), [56, 56, 66, 255]),
                // Status bar along the bottom.
                (Rect::new(0, 760, 1200, 40), [88, 88, 100, 255]),
                // A control inside the sidebar.
                (Rect::new(20, 60, 200, 32), [150, 150, 165, 255]),
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
