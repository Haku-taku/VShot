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

/// Two colours are "the same" for a component when every channel is within
/// this.  Loose enough to absorb a gradient or a subtle texture, tight enough
/// that a border line is not swallowed.
const SAME_COLOR: i32 = 12;

/// How much a line's centre must differ from its surround, in luminance, to
/// count as a line.
///
/// Measured on a real window: the separators between its panes answer 60 to 75
/// through the matched filter, while a themed background's own texture answers
/// under 4.  The lines that divide an interface are drawn to be seen, so the
/// value between those is not delicate.
const LINE_THRESHOLD: f32 = 16.0;

/// How much the two sides of a colour step must differ, in luminance.
///
/// Much lower than the line threshold, because this is measuring something
/// else: a divider is drawn to be seen, while the boundary between two panes of
/// a themed interface can be a couple of levels.  Measured on a chat client,
/// its sidebar and its conversation differ by 2, and at 6 the boundary between
/// them is missed entirely.
const BLOCK_THRESHOLD: f32 = 2.0;

/// The smallest a *control* may be and still be offered.
///
/// Much smaller than [`MIN_REGION_EDGE`], because the two measure different
/// things: a region is a pane of the interface, a control is a button inside
/// one.  A button is routinely 24 to 40 pixels; a pane is never that small.
const MIN_CONTROL_EDGE: u32 = 24;

/// How large a region has to be before its colour steps are trusted.
///
/// Below this a step is as likely to be a control's own edge, or the boundary
/// of a block of content, as a division of the interface.  Measured on a chat
/// client: at 100 the conversation list is cut into one region per row, at 200
/// the sidebar and the conversation are the two regions a person would name.
const BLOCK_MIN_SPAN: u32 = 200;

/// How far either side of a colour step the means are taken.
///
/// Wide enough that a band averages away the text and icons inside a pane, so
/// what is compared is the panes' own colours rather than their contents.
const BLOCK_REACH: u32 = 24;

/// The widest a line may be and still be a line.
///
/// A divider is one pixel wide on one toolkit and three on another, so the
/// filter is tried at each width and the best answer kept.  Past a few pixels
/// the "line" is a pane's own edge, which the cut does not need told about.
const MAX_LINE_WIDTH: u32 = 4;

/// How much of a line has to run through a region before it divides it.
///
/// Not 1.0: a divider interrupted by the content it separates is still the
/// divider, and a sidebar's own tab bar crosses only 65% of the sidebar before
/// its list of commits begins.  Not low either, or a paragraph's ragged edge
/// would read as a cut.
///
/// Measured on a real window: 0.75 leaves a sidebar whole, 0.65 divides it into
/// its tab bar and its list, and 0.55 shatters the window into 78 regions.
const CUT_FRACTION: f64 = 0.65;

/// The smallest region worth offering or cutting.
///
/// Below this a region is a glyph or an anti-aliased corner rather than
/// something a person would point at.
const MIN_REGION_EDGE: u32 = 200;

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
        // A window the compositor lists but the screen does not show — one
        // parked off the edge, or on a workspace that is not visible — has
        // almost nothing to analyse.  Below a couple of regions' worth there is
        // nothing here to find, and saying so is cheaper than walking a sliver.
        if device_width < MIN_REGION_EDGE || device_height < MIN_REGION_EDGE {
            return None;
        }

        // Every device pixel is looked at, at its own resolution.  A downsample
        // was tried and taken out: a divider is one or two pixels wide, and
        // averaging it into a larger block turns a strong line into a weak ramp
        // — which is the signal the whole pass depends on.  The cost is real
        // (a 4K window is 8M pixels per pass) and accepted, because a detector
        // that cannot see the lines is not worth running quickly.
        let factor = 1u32;
        let width = device_width;
        let height = device_height;

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
        // Two kinds, held apart because they are worth different sizes: a
        // region is a pane and has to be big to be worth offering, while a
        // control is a button and is *supposed* to be small.  One floor for
        // both would either lose every button or shatter every pane.
        let mut found: Vec<(Rect, bool)> = regions.iter().map(|rect| (*rect, false)).collect();
        // Controls are looked for over the *whole window*, not inside each
        // region.  A region is a pane and a control may straddle two of them —
        // a toolbar button sitting on the line between the toolbar and the
        // page — and searching region by region would find half a button in
        // each and neither half would be the button.
        for control in self.controls_in(Rect::new(0, 0, self.width, self.height)) {
            if debug_enabled() {
                eprintln!(
                    "vshot:   control {}x{}+{}+{}",
                    control.size.width,
                    control.size.height,
                    control.left(),
                    control.top()
                );
            }
            found.push((control, true));
        }
        found
            .into_iter()
            .filter(|(rect, _)| {
                // The window itself is not an element inside it.  A cut that
                // found no line anywhere leaves exactly that, and offering it
                // would give the user a box they cannot tell from the window.
                !(rect.size.width == self.width && rect.size.height == self.height)
            })
            .filter_map(|(rect, is_control)| {
                let floor = if is_control { MIN_CONTROL_EDGE } else { MIN_REGION_EDGE };
                (rect.size.width >= floor && rect.size.height >= floor)
                    .then(|| self.to_global(rect))
                    .flatten()
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
                // A component touching the region's edge is the region's own
                // background, not a control in it: whatever colour the pane is
                // painted, it runs to the pane's border.  Only what the
                // background *surrounds* is a control, and being surrounded
                // means not touching the edge.
                let touches_edge = min_x <= x0 || min_y <= y0 || max_x + 1 >= x1 || max_y + 1 >= y1;
                if touches_edge {
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
        // A colour step is only trusted in a region large enough to be a pane:
        // in a small one the step is as likely to be a control's own edge or a
        // block of content as a division of the interface.  Lines are trusted
        // at any size, because a line is drawn deliberately.
        let big = (x1 - x0) >= BLOCK_MIN_SPAN && (y1 - y0) >= BLOCK_MIN_SPAN;
        match edges.strongest(x0, y0, x1, y1, big) {
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

    /// Where a line runs, as a response map for each axis.
    ///
    /// The response is a *convolution*, not a difference between neighbouring
    /// pixels.  A divider is a run of one colour a few pixels wide with a
    /// different colour on both sides, and the kernel that answers to exactly
    /// that shape is a centre band flanked by two bands of the opposite sign —
    /// a matched filter for a line.  It responds where the centre differs from
    /// *both* flanks, which is what a drawn border, a colour step between two
    /// panes, and the edge of a toolbar all are.
    ///
    /// What that buys over an adjacent-pixel difference:
    ///
    /// * an anti-aliased line is spread over two or three pixels, each a weak
    ///   step; the centre band averages them back into one strong response;
    /// * a glyph's stroke is as sharp as a divider but has background on one
    ///   side only, so requiring *both* flanks to differ drops it;
    /// * the response is a contrast in luminance, so it means the same thing on
    ///   a light theme as on a dark one, and two colours that differ in hue but
    ///   not in brightness are not mistaken for a line.
    ///
    /// Computed once for the window and kept as a map: a cut then reads the
    /// slice belonging to its own region, which is what lets a line be found
    /// inside a pane where it does not cross the window.
    ///
    /// The kernel is separable — a band difference across the line, a box blur
    /// along it — so it is run as two passes of running sums rather than as a
    /// box lookup per pixel.  Measured on a 2560x1440 window, that is the
    /// difference between 200 ms and about 15: a summed-area table is constant
    /// time per query but every query is four scattered reads, and four million
    /// of those miss the cache far more often than a sequential sweep does.
    fn edges(&self) -> Edges {
        let luma: Vec<f32> = self
            .pixels
            .iter()
            .map(|pixel| luminance(*pixel))
            .collect();
        let width = self.width as usize;
        let height = self.height as usize;

        // Columns first: for each row, sweep across it and answer for every x.
        let mut vertical = vec![false; width * height];
        let mut block_vertical = vec![false; width * height];
        let mut row = vec![0f32; width];
        let mut prefix = vec![0f32; width + 1];
        for y in 0..height {
            row.copy_from_slice(&luma[y * width..(y + 1) * width]);
            running_sum(&row, &mut prefix);
            // The window is a few pixels either side, so the same row answers
            // for every x on it in one pass.
            for x in 0..width {
                let index = y * width + x;
                let at = x as u32;
                vertical[index] = matched_filter(&prefix, at, self.width);
                block_vertical[index] = block_step(&prefix, at, self.width);
            }
        }

        // Then rows, over the column answers just computed: the vertical map is
        // read down its own columns, which is the same sweep with the axes
        // swapped.
        let mut horizontal = vec![false; width * height];
        let mut block_horizontal = vec![false; width * height];
        let mut column = vec![0f32; height];
        let mut down = vec![0f32; height + 1];
        for x in 0..width {
            for y in 0..height {
                column[y] = luma[y * width + x];
            }
            running_sum(&column, &mut down);
            for y in 0..height {
                let index = y * width + x;
                let at = y as u32;
                horizontal[index] = matched_filter(&down, at, self.height);
                block_horizontal[index] = block_step(&down, at, self.height);
            }
        }

        Edges {
            width: self.width,
            height: self.height,
            vertical,
            horizontal,
            block_vertical,
            block_horizontal,
        }
    }
}

/// Prefix sums of one line, so a band's mean is two lookups.
fn running_sum(line: &[f32], prefix: &mut [f32]) {
    prefix[0] = 0.0;
    for (index, value) in line.iter().enumerate() {
        prefix[index + 1] = prefix[index] + value;
    }
}


/// The matched filter at one position: `min(|centre − left|, |centre − right|)`,
/// over every width a line might be.
///
/// The *minimum* of the two sides rather than the nearer one: a line has a
/// different colour on both sides, while a glyph stroke has background on one
/// side and more glyph on the other.  Taking the minimum is what tells them
/// apart, and it is the whole reason this beats an adjacent-pixel test.
fn matched_filter(prefix: &[f32], at: u32, span: u32) -> bool {
    for width in 1..=MAX_LINE_WIDTH {
        if at < 3 * width {
            // The window has not opened yet, and it only widens: no later
            // width can fit either.
            return false;
        }
        if at + 3 * width > span {
            // Too close to the far edge for this width, but a narrower one may
            // still fit.
            continue;
        }
        // The bands are contiguous, so their sums are three differences over
        // one prefix array rather than three independent lookups.
        let centre_sum = prefix[(at + width) as usize] - prefix[(at - width) as usize];
        let left_sum = prefix[(at - width) as usize] - prefix[(at - 3 * width) as usize];
        let right_sum = prefix[(at + 3 * width) as usize] - prefix[(at + width) as usize];
        let two_w = (2 * width) as f32;
        let centre = centre_sum / two_w;
        let left = left_sum / two_w;
        let right = right_sum / two_w;
        if (centre - left).abs().min((centre - right).abs()) >= LINE_THRESHOLD {
            return true;
        }
    }
    false
}

/// Whether a colour step runs through one position: the means either side
/// differ by enough.
///
/// Where the matched filter looks for a *line* — a narrow run with a different
/// colour on both sides — this looks for the boundary between two areas of
/// different colour, however wide each is.  The band is `BLOCK_REACH` pixels
/// either side, which is what makes it a comparison of areas rather than of
/// points: a chat client's sidebar and its conversation differ by a couple of
/// luminance levels, far too little for one pixel to show and plainly visible
/// across a band.
fn block_step(prefix: &[f32], at: u32, span: u32) -> bool {
    if at < BLOCK_REACH || at + BLOCK_REACH > span {
        return false;
    }
    let reach = BLOCK_REACH as f32;
    let before = (prefix[at as usize] - prefix[(at - BLOCK_REACH) as usize]) / reach;
    let after = (prefix[(at + BLOCK_REACH) as usize] - prefix[at as usize]) / reach;
    (before - after).abs() >= BLOCK_THRESHOLD
}

/// Rec. 709 luminance: the grey the eye would see.
///
/// Not a plain average of the channels — green carries most of the perceived
/// brightness, and two colours that average the same can look very different,
/// which is exactly the distinction a line detector needs.
fn luminance(pixel: [u8; 4]) -> f32 {
    0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2])
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
/// Whether two colours are the same for a component.
fn close_enough(a: [u8; 4], b: [u8; 4]) -> bool {
    let channel = |i: usize| (i32::from(a[i]) - i32::from(b[i])).abs();
    channel(0) <= SAME_COLOR
        && channel(1) <= SAME_COLOR
        && channel(2) <= SAME_COLOR
        && channel(3) <= SAME_COLOR
}

/// Where a line runs, as a response map per axis.
///
/// One entry per analysis pixel, in row-major order: `vertical[i]` is true when
/// the pixel at `i` looks like part of a vertical line, `horizontal[i]` the
/// same across.  Kept as maps rather than as per-column counts so a *region*
/// can ask about its own slice — which is the difference between finding a
/// divider that crosses a pane and one that crosses the whole window.
struct Edges {
    width: u32,
    height: u32,
    /// Drawn lines, per axis.
    vertical: Vec<bool>,
    horizontal: Vec<bool>,
    /// Colour steps between panes, per axis.  Kept apart from the lines rather
    /// than merged, because the two are trusted differently: a line is drawn
    /// deliberately and believed anywhere, a colour step is only believed in a
    /// region big enough to be a pane.
    block_vertical: Vec<bool>,
    block_horizontal: Vec<bool>,
}

impl Edges {
    /// The strongest line crossing the region, as `(axis, position)`.
    ///
    /// The position is where the region is split: the far side of the line for
    /// a vertical cut, so the line itself stays with the left-hand region
    /// rather than being lost between the two.
    ///
    /// The strength is measured *within the region*: how much of the line's
    /// length inside `(x0, y0, x1, y1)` is a line.  Measuring over the whole
    /// window instead would miss every divider that crosses a pane without
    /// crossing the window — an editor's toolbar line, for one, which stops at
    /// the sidebar.
    ///
    /// Ties go to the earlier position, which keeps the walk deterministic.
    fn strongest(&self, x0: u32, y0: u32, x1: u32, y1: u32, blocks: bool) -> Option<(Axis, u32)> {
        let height = f64::from(y1 - y0).max(1.0);
        let width = f64::from(x1 - x0).max(1.0);
        let mut best: Option<(f64, Axis, u32)> = None;

        for x in (x0 + MIN_REGION_EDGE)..(x1.saturating_sub(MIN_REGION_EDGE)) {
            let fraction = self.column_strength(x, y0, y1, blocks) / height;
            if fraction < CUT_FRACTION {
                continue;
            }
            if best.is_none_or(|(score, _, _)| fraction > score) {
                best = Some((fraction, Axis::Vertical, x));
            }
        }
        for y in (y0 + MIN_REGION_EDGE)..(y1.saturating_sub(MIN_REGION_EDGE)) {
            let fraction = self.row_strength(y, x0, x1, blocks) / width;
            if fraction < CUT_FRACTION {
                continue;
            }
            if best.is_none_or(|(score, _, _)| fraction > score) {
                best = Some((fraction, Axis::Horizontal, y));
            }
        }
        best.map(|(_, axis, at)| (axis, at))
    }

    /// How much of column `x` between `y0` and `y1` is part of a vertical line.
    fn column_strength(&self, x: u32, y0: u32, y1: u32, blocks: bool) -> f64 {
        if x >= self.width {
            return 0.0;
        }
        let mut count = 0u32;
        for y in y0..y1.min(self.height) {
            let index = (y * self.width + x) as usize;
            let hit = if blocks {
                self.vertical[index] || self.block_vertical[index]
            } else {
                self.vertical[index]
            };
            if hit {
                count += 1;
            }
        }
        f64::from(count)
    }

    /// How much of row `y` between `x0` and `x1` is part of a horizontal line.
    fn row_strength(&self, y: u32, x0: u32, x1: u32, blocks: bool) -> f64 {
        if y >= self.height {
            return 0.0;
        }
        let row = (y * self.width) as usize;
        let mut count = 0u32;
        for x in x0..x1.min(self.width) {
            let hit = if blocks {
                self.horizontal[row + x as usize] || self.block_horizontal[row + x as usize]
            } else {
                self.horizontal[row + x as usize]
            };
            if hit {
                count += 1;
            }
        }
        f64::from(count)
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
            1280,
            900,
            [30, 30, 30, 255],
            &[(Rect::new(200, 300, 400, 150), [200, 200, 200, 255])],
        );
        let elements = find(&scene, &window).expect("something found");
        let flat = flatten(&elements);
        assert!(
            flat.iter().any(|node| node.rect.size.width >= 350
                && node.rect.size.width <= 450
                && node.rect.size.height >= 120
                && node.rect.size.height <= 180),
            "no button-sized region among {:?}",
            flat.iter().map(|n| n.rect).collect::<Vec<_>>()
        );
    }

    /// A uniform window has no elements in it — the whole thing is one colour,
    /// which is the background, not a control.
    #[test]
    fn a_flat_window_yields_nothing() {
        let (scene, window) = scene_of(1280, 900, [30, 30, 30, 255], &[]);
        assert!(find(&scene, &window).is_none());
    }

    /// Two buttons side by side are two elements, not one.
    #[test]
    fn two_separated_buttons_are_two_elements() {
        let (scene, window) = scene_of(
            1280,
            900,
            [30, 30, 30, 255],
            &[
                (Rect::new(150, 300, 300, 120), [200, 200, 200, 255]),
                (Rect::new(800, 300, 300, 120), [200, 200, 200, 255]),
            ],
        );
        let elements = find(&scene, &window).expect("something found");
        let buttons = flatten(&elements)
            .into_iter()
            .filter(|node| {
                node.rect.size.width >= 250
                    && node.rect.size.width <= 350
                    && node.rect.size.height >= 100
                    && node.rect.size.height <= 150
            })
            .count();
        assert!(buttons >= 2, "expected two buttons, found {buttons}");
    }

    /// A panel with a control inside it nests: the picker can climb from the
    /// control to the panel.
    #[test]
    fn a_control_inside_a_panel_becomes_its_child() {
        let (scene, window) = scene_of(
            1280,
            900,
            [30, 30, 30, 255],
            &[
                (Rect::new(100, 100, 1080, 700), [80, 80, 90, 255]),
                (Rect::new(200, 300, 300, 120), [200, 200, 200, 255]),
            ],
        );
        let elements = find(&scene, &window).expect("something found");
        // The button has to sit under something, or the wheel could not climb
        // out of it — which is the whole point of building a tree.
        let button = flatten(&elements)
            .into_iter()
            .find(|node| {
                node.rect.size.width >= 250
                    && node.rect.size.width <= 350
                    && node.rect.size.height >= 100
                    && node.rect.size.height <= 150
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
            1280,
            900,
            [30, 30, 30, 255],
            &[(Rect::new(200, 300, 400, 150), [200, 200, 200, 255])],
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
        // An editor's shape: a tab strip across the top, a sidebar down the
        // left, a status bar along the bottom, and a control in the sidebar.
        // What is asserted is that the panes come out as panes — a sidebar
        // several hundred pixels wide and most of the window tall — and that
        // the control inside it is offered too.
        let (scene, window) = scene_of(
            2560,
            1440,
            [24, 24, 28, 255],
            &[
                (Rect::new(0, 0, 2560, 80), [72, 72, 84, 255]),
                (Rect::new(0, 80, 500, 1280), [56, 56, 66, 255]),
                (Rect::new(0, 1360, 2560, 80), [88, 88, 100, 255]),
                (Rect::new(40, 120, 400, 200), [150, 150, 165, 255]),
            ],
        );
        let elements = find(&scene, &window).expect("an editor has regions");
        let flat = flatten(&elements);
        let rects: Vec<Rect> = flat.iter().map(|node| node.rect).collect();

        let has_sidebar = flat.iter().any(|node| {
            node.rect.size.width >= 400
                && node.rect.size.width <= 600
                && node.rect.size.height >= 900
        });
        assert!(has_sidebar, "no sidebar among {rects:?}");

        let has_control = flat.iter().any(|node| {
            node.rect.size.width >= 350
                && node.rect.size.width <= 450
                && node.rect.size.height >= 150
                && node.rect.size.height <= 250
        });
        assert!(has_control, "no control inside the sidebar among {rects:?}");
    }

    /// A terminal has no controls at all — one text grid filling the window.
    /// The source has to stay quiet rather than report the text rows as
    /// elements, which is what a user sees as "it found eight things and none
    /// of them is real".
    #[test]
    fn a_terminal_yields_few_regions() {
        // A dark background with a grid of glyphs, the way a terminal looks:
        // runs of lit pixels with gaps, not full-width bars.  The gap matters —
        // a bar spanning the window would be found as a region, while a row of
        // glyphs is only as wide as its text.
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
        let found = find(&scene, &window);
        let count = found.as_ref().map(|e| flatten(e).len()).unwrap_or(0);
        // Not zero: a glyph is a small uniform block, and the detector offers
        // what it finds.  What matters is that it is not one region per
        // character — that would be thousands.
        assert!(
            count <= 60,
            "a terminal should not shatter into a region per glyph, but {count} were reported"
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

