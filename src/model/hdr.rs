// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#![allow(dead_code)]

//! HDR content: recognising it, decoding it to linear light, and the two images
//! a capture then produces.
//!
//! The working representation is scRGB-like: **linear** light in the sRGB /
//! BT.709 primaries, where 1.0 is the SDR reference white (203 cd/m², the value
//! ITU-R BT.2408 gives for a graphics white).  An HDR10 source — BT.2020
//! primaries, PQ (ST 2084) transfer — is decoded into that space, the editor's
//! marks are composited there in linear light, and two images come out:
//!
//! * `<name>.png`, 8-bit sRGB, the tone-mapped SDR view of the same content;
//! * `<name>.hdr`, Radiance RGBE, the HDR content itself.
//!
//! Both the PQ curve and the BT.2020↔BT.709 matrices are the ones a colour
//! pipeline is expected to use; the same constants appear in the reference
//! implementation this follows (Starward's `HdrToneMapEffect`/`HDR10ToScRGB`).

use crate::error::{Result, VshotError};
use crate::geometry::{Point, Rect, Size};
use crate::model::Frame;

/// The scRGB reference white: linear value 1.0 is this many cd/m².
///
/// It is the white a frame is measured against by default.  A compositor that
/// describes its own SDR white (see [`OutputColor::reference_nits`]) makes a
/// capture use that instead, so that 1.0 always stands for the content's own
/// white and a plain SDR pixel is never read as light above it.
pub const REFERENCE_WHITE_NITS: f32 = 203.0;
/// The peak the PQ curve is defined to (ST 2084).
pub const PQ_PEAK_NITS: f32 = 10_000.0;
/// The nominal peak of an HLG signal (BT.2100).
pub const HLG_PEAK_NITS: f32 = 1_000.0;

/// Content above SDR white by this much (about 2 %) counts as HDR: a flat SDR
/// frame decodes to at most 1.0, so anything over the threshold really is over.
const HDR_WHITE_EPSILON: f32 = 0.02;

/// The transfer function an HDR buffer is encoded with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transfer {
    /// Already linear light (`Linear` in the wayland/PQ sense), 1.0 = white.
    Linear,
    /// The sRGB curve, i.e. ordinary SDR pixels.
    Srgb,
    /// PQ / ST 2084, as HDR10 uses.
    Pq,
    /// HLG / ARIB STD-B67, as broadcast HDR uses.
    Hlg,
}

/// The primaries an HDR buffer carries.  Only the two the pipeline needs to
/// tell apart: BT.709 (scRGB) and BT.2020 (HDR10).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Primaries {
    Bt709,
    Bt2020,
}

/// The colour properties of one output, as the compositor describes them over
/// `wp_color_manager_v1`.
///
/// This is the Wayland analogue of the display facts Starward reads on Windows:
/// `reference_nits` is the SDR white level, the light level a code of 1.0 stands
/// for and the level above which a capture really is HDR.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputColor {
    /// The transfer function the output expects content in.
    pub transfer: Transfer,
    /// The primaries the output expects content in.
    pub primaries: Primaries,
    /// The luminance one unit of content means, in cd/m² (the reference white).
    pub reference_nits: f32,
}

impl OutputColor {
    /// Whether the output is showing HDR content (a PQ or HLG transfer).
    pub fn is_hdr(&self) -> bool {
        matches!(self.transfer, Transfer::Pq | Transfer::Hlg)
    }
}

// --- reading an unlabelled buffer -----------------------------------------
//
// A 10-bit screencopy buffer carries no transfer function: `zwlr_screencopy_v1`
// names a format and a depth, nothing more.  It must not be guessed from the
// pixels either.  PQ content on a compositor that passes a client's absolute
// HDR through — Hyprland does, it only converts between transfer functions and
// never tone-maps into the panel's range — reaches ten-bit full scale, so it
// looks exactly like sRGB white to any statistic of the buffer; reading such a
// capture as sRGB on that evidence is what turned a correct HDR capture grey.
//
// The encoding is instead fixed by the format, one layer down: a compositor
// hands a client a 10-bit buffer *only* when it means to fill it with the
// output's own HDR pixels, and offers 8-bit sRGB on an HDR output otherwise.
// Hyprland upholds that in `CMonitor::getPreferredReadFormat` — with
// `misc:screencopy_hdr` off an HDR output is read back as `XRGB8888` — so the
// capture side reads a 10-bit buffer on an output the compositor describes as
// HDR as the output's own transfer function, and never inspects the pixels to
// decide.  [`Rgb10Summary`] exists only to log what a capture held.

/// The ten-bit code from which a sample counts as near sRGB white in the debug
/// summary; just below full scale, so a dithered edge still counts.
const SUMMARY_NEAR_WHITE: u16 = 1000;

/// The percentile of the per-pixel maximum channel the summary reports, high
/// so a few dithered or blown samples do not move it.
const SUMMARY_P999_PERMILLE: u64 = 999; // 99.9 %

/// A compact reading of a 10-bit buffer's per-pixel maximum channel: a couple of
/// percentiles, the largest code seen, and how much of the frame sits at sRGB
/// white.  It drives no decision — see the section comment above — and is only
/// logged (behind `VSHOT_HDR_DEBUG`) so a capture can be checked against what
/// produced it.
#[derive(Clone, Copy, Debug)]
pub struct Rgb10Summary {
    pub median: u16,
    pub p999: u16,
    pub max: u16,
    /// Share of pixels at or above [`SUMMARY_NEAR_WHITE`].
    pub white_share: f32,
}

impl Rgb10Summary {
    /// Measures `words`, which are DRM `XRGB2101010`-packed pixels.
    pub fn of(words: &[u32]) -> Self {
        let mut histogram = [0u32; 1024];
        for word in words {
            let red = (word >> 20) & 0x3ff;
            let green = (word >> 10) & 0x3ff;
            let blue = word & 0x3ff;
            // A histogram index is a ten-bit code, always in range.
            histogram[(red.max(green).max(blue) & 0x3ff) as usize] += 1;
        }

        let total = u64::try_from(words.len()).unwrap_or(u64::MAX);
        let percentile = |permille: u64| {
            let target = (total * permille).div_ceil(1000);
            let mut seen = 0u64;
            for (code, &count) in histogram.iter().enumerate() {
                seen += u64::from(count);
                if seen >= target {
                    // The loop index is a ten-bit code, always in range.
                    return code as u16;
                }
            }
            1023
        };

        let mut max = 0u16;
        let mut white = 0u64;
        for (code, &count) in histogram.iter().enumerate() {
            if count > 0 {
                max = code as u16;
            }
            if code as u16 >= SUMMARY_NEAR_WHITE {
                white += u64::from(count);
            }
        }

        Self {
            median: percentile(500),
            p999: percentile(SUMMARY_P999_PERMILLE),
            max,
            white_share: if total == 0 {
                0.0
            } else {
                white as f32 / total as f32
            },
        }
    }
}

// --- transfer functions ---------------------------------------------------

/// The PQ EOTF (ST 2084): a 0..1 code to linear luminance, normalised so 1.0
/// is 10 000 cd/m².
fn pq_eotf(code: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let code = code.clamp(0.0, 1.0);
    let p = code.powf(1.0 / M2);
    let numerator = (p - C1).max(0.0);
    let denominator = C2 - C3 * p;
    if denominator <= 0.0 {
        return 0.0;
    }
    (numerator / denominator).powf(1.0 / M1)
}

/// The PQ inverse EOTF (OETF): linear luminance normalised to 10 000 cd/m²
/// back to a 0..1 code.
fn pq_oetf(luminance: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let l = luminance.max(0.0);
    let p = l.powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2).clamp(0.0, 1.0)
}

/// The HLG inverse OETF (BT.2100): a 0..1 signal to scene-linear 0..1.
fn hlg_inverse_oetf(signal: f32) -> f32 {
    const A: f32 = 0.178_832_8;
    const B: f32 = 0.284_668_9;
    const C: f32 = 0.559_910_7;
    let e = signal.clamp(0.0, 1.0);
    if e <= 0.5 {
        e * e / 3.0
    } else {
        (((e - C) / A).exp() + B) / 12.0
    }
}

/// The sRGB EOTF (IEC 61966-2-1): a 0..1 code to linear light, 1.0 = white.
fn srgb_eotf(value: f32) -> f32 {
    let v = value.clamp(0.0, 1.0);
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// The sRGB OETF: linear light back to a 0..1 code.
fn srgb_oetf(linear: f32) -> f32 {
    let l = linear.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

// --- primaries ------------------------------------------------------------

/// BT.2020 to BT.709, linear light.  The rows sum to one, so the shared D65
/// white point is preserved exactly and a neutral HDR pixel stays neutral.
const BT2020_TO_BT709: [[f32; 3]; 3] = [
    [1.660_491, -0.587_641, -0.072_850],
    [-0.124_551, 1.132_9, -0.008_349],
    [-0.018_151, -0.100_579, 1.118_73],
];

/// BT.709 to BT.2020, linear light (the inverse direction, for HDR output).
const BT709_TO_BT2020: [[f32; 3]; 3] = [
    [0.627_404, 0.329_283, 0.043_313],
    [0.069_097, 0.919_540, 0.011_362],
    [0.016_391, 0.088_013, 0.895_595],
];

fn multiply(matrix: [[f32; 3]; 3], rgb: [f32; 3]) -> [f32; 3] {
    [
        matrix[0][0] * rgb[0] + matrix[0][1] * rgb[1] + matrix[0][2] * rgb[2],
        matrix[1][0] * rgb[0] + matrix[1][1] * rgb[1] + matrix[1][2] * rgb[2],
        matrix[2][0] * rgb[0] + matrix[2][1] * rgb[1] + matrix[2][2] * rgb[2],
    ]
}

// --- the frame ------------------------------------------------------------

/// A frame in linear light (scRGB-like), one `[r, g, b, a]` per pixel.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrFrame {
    size: Size,
    pixels: Vec<[f32; 4]>,
}

impl HdrFrame {
    /// Wraps already-linear scRGB pixels.
    pub fn new(size: Size, pixels: Vec<[f32; 4]>) -> Result<Self> {
        let expected = size.area()?;
        if pixels.len() != expected {
            return Err(VshotError::InvalidGeometry(format!(
                "HDR frame has {} pixels, expected {expected}",
                pixels.len()
            )));
        }
        Ok(Self { size, pixels })
    }

    pub const fn size(&self) -> Size {
        self.size
    }

    pub fn pixel(&self, x: u32, y: u32) -> Option<[f32; 4]> {
        if x >= self.size.width || y >= self.size.height {
            return None;
        }
        let index = (y as usize) * (self.size.width as usize) + (x as usize);
        self.pixels.get(index).copied()
    }

    /// The brightest linear component anywhere in the frame.  An SDR frame
    /// never exceeds 1.0, so a peak above it is what "this content is HDR"
    /// means.
    pub fn peak(&self) -> f32 {
        self.pixels
            .iter()
            .map(|pixel| pixel[0].max(pixel[1]).max(pixel[2]))
            .fold(0.0f32, f32::max)
    }

    /// Whether the frame actually carries light beyond SDR white.
    pub fn is_hdr(&self) -> bool {
        self.peak() > 1.0 + HDR_WHITE_EPSILON
    }

    /// Decodes 10-bit RGB packed the way DRM's `XRGB2101010`/`ARGB2101010`
    /// pack it (bits 20..30, 10..20, 0..10, alpha in the top two).  This is the
    /// shape a compositor's HDR screencopy buffer arrives in.  With `alpha`
    /// false the top bits are the `X` padding and the pixel is taken as opaque.
    ///
    /// `reference_nits` is the light level 1.0 stands for — the compositor's
    /// own SDR white, when it describes one.
    pub fn from_rgb10(
        words: &[u32],
        size: Size,
        transfer: Transfer,
        primaries: Primaries,
        alpha: bool,
        reference_nits: f32,
    ) -> Result<Self> {
        let expected = size.area()?;
        if words.len() != expected {
            return Err(VshotError::InvalidGeometry(format!(
                "HDR frame has {} samples, expected {expected}",
                words.len()
            )));
        }
        let mut pixels = Vec::with_capacity(expected);
        for word in words {
            let mut rgb = [
                decode_transfer(
                    ((word >> 20) & 0x3ff) as f32 / 1023.0,
                    transfer,
                    reference_nits,
                ),
                decode_transfer(
                    ((word >> 10) & 0x3ff) as f32 / 1023.0,
                    transfer,
                    reference_nits,
                ),
                decode_transfer((word & 0x3ff) as f32 / 1023.0, transfer, reference_nits),
            ];
            // HLG needs the whole triple: BT.2100's opto-optical transfer takes
            // the frame's own luma, so it cannot ride on a per-channel decode.
            if transfer == Transfer::Hlg {
                rgb = hlg_ootf(rgb, reference_nits);
            }
            if primaries == Primaries::Bt2020 {
                rgb = multiply(BT2020_TO_BT709, rgb);
            }
            let alpha = if alpha {
                ((word >> 30) & 0x3) as f32 / 3.0
            } else {
                1.0
            };
            pixels.push([rgb[0], rgb[1], rgb[2], alpha]);
        }
        Self::new(size, pixels)
    }

    /// Encodes the frame as the ten-bit pixels a colour-managed surface reads:
    /// BT.2020 primaries with the PQ (ST 2084) transfer, packed the way
    /// `wl_shm`'s `ARGB2101010` family packs them (alpha in bits 30..32, red in
    /// 20..30, green in 10..20, blue in 0..10).
    ///
    /// `reference_nits` is the light level the frame's `1.0` stands for — the
    /// output's own SDR white — so a code comes back out at the absolute
    /// luminance it was captured at.  This is the inverse of
    /// [`HdrFrame::from_rgb10`], and what puts a frozen frame onto an HDR
    /// overlay surface.
    ///
    /// The alpha bits are set: these words go into a *surface* buffer, where
    /// the two bits are a real alpha channel and a zero would make every pixel
    /// transparent.  A capture buffer reads them as the `X`/`A` padding and
    /// ignores them either way, which is why the decode side takes `alpha
    /// false` here.
    pub fn to_rgb10_pq(&self, reference_nits: f32) -> Vec<u32> {
        let reference = if reference_nits.is_finite() && reference_nits > 0.0 {
            reference_nits
        } else {
            REFERENCE_WHITE_NITS
        };
        let code = |linear: f32| -> u32 {
            (pq_oetf(linear * reference / PQ_PEAK_NITS) * 1023.0)
                .round()
                .clamp(0.0, 1023.0) as u32
        };
        self.pixels
            .iter()
            .map(|pixel| {
                let rgb = multiply(BT709_TO_BT2020, [pixel[0], pixel[1], pixel[2]]);
                (3 << 30) | (code(rgb[0]) << 20) | (code(rgb[1]) << 10) | code(rgb[2])
            })
            .collect()
    }

    /// Tone-maps the whole frame to an 8-bit sRGB frame, the SDR half of the
    /// pair, with extended Reinhard and the frame's own peak as the white point:
    /// the brightest sample lands on white and nothing clips, and hue is kept
    /// because the whole triple is scaled by one factor.  Alpha is quantised to
    /// a byte like every other channel (a capture is opaque).
    pub fn tone_map_to_srgb(&self) -> Result<Frame> {
        let white = self.peak().max(1.0);
        let mut bytes = Vec::with_capacity(self.pixels.len() * 4);
        for pixel in &self.pixels {
            let mut rgb = [pixel[0].max(0.0), pixel[1].max(0.0), pixel[2].max(0.0)];
            if white > 1.0 {
                let luminance = rgb[0].max(rgb[1]).max(rgb[2]);
                if luminance > 0.0 {
                    let mapped =
                        luminance * (1.0 + luminance / (white * white)) / (1.0 + luminance);
                    let scale = mapped / luminance;
                    rgb = [rgb[0] * scale, rgb[1] * scale, rgb[2] * scale];
                }
            }
            bytes.push(to_u8(srgb_oetf(rgb[0])));
            bytes.push(to_u8(srgb_oetf(rgb[1])));
            bytes.push(to_u8(srgb_oetf(rgb[2])));
            bytes.push(to_u8(pixel[3]));
        }
        Frame::new(self.size, bytes)
    }

    /// Composites an 8-bit sRGB layer (an annotation raster, black where it is
    /// transparent) over this frame **in linear light**: annotation colours are
    /// decoded to linear scRGB and blended there, which is what makes a mark on
    /// an HDR image keep its brightness instead of being crushed by the SDR
    /// curve.  The layer must be the same size.  This is an ordinary source-over
    /// on straight alpha, so it also holds for a frame that is not opaque.
    pub fn composite_srgb_layer(&mut self, layer: &Frame) -> Result<()> {
        if layer.size() != self.size {
            return Err(VshotError::InvalidGeometry(format!(
                "annotation layer is {}x{}, frame is {}x{}",
                layer.size().width,
                layer.size().height,
                self.size.width,
                self.size.height
            )));
        }
        for (destination, rgba) in self.pixels.iter_mut().zip(layer.pixels().chunks_exact(4)) {
            let alpha = f32::from(rgba[3]) / 255.0;
            if alpha == 0.0 {
                continue;
            }
            let source = [
                srgb_eotf(f32::from(rgba[0]) / 255.0),
                srgb_eotf(f32::from(rgba[1]) / 255.0),
                srgb_eotf(f32::from(rgba[2]) / 255.0),
            ];
            let below = destination[3];
            let out_alpha = alpha + below * (1.0 - alpha);
            if out_alpha <= 0.0 {
                continue;
            }
            for channel in 0..3 {
                destination[channel] = (source[channel] * alpha
                    + destination[channel] * below * (1.0 - alpha))
                    / out_alpha;
            }
            destination[3] = out_alpha;
        }
        Ok(())
    }

    /// Encodes the frame as Radiance RGBE (`.hdr`).  The values are linear
    /// light, which is exactly what the format holds, so an HDR viewer shows
    /// the content as captured.
    pub fn encode_radiance(&self) -> Vec<u8> {
        let width = self.size.width;
        let height = self.size.height;
        let mut out = Vec::new();
        out.extend_from_slice(b"#?RADIANCE\n");
        out.extend_from_slice(b"FORMAT=32-bit_rle_rgbe\n");
        out.extend_from_slice(b"\n");
        out.extend_from_slice(format!("-Y {height} +X {width}\n").as_bytes());
        let rle = (8..=0x7fff).contains(&width);
        for row in 0..height as usize {
            let start = row * width as usize;
            let scanline = &self.pixels[start..start + width as usize];
            if rle {
                out.extend_from_slice(&[2, 2, (width >> 8) as u8, (width & 0xff) as u8]);
                // Four component planes, each run-length encoded on its own.
                for channel in 0..4 {
                    let plane: Vec<u8> = scanline
                        .iter()
                        .map(|pixel| to_rgbe(*pixel)[channel])
                        .collect();
                    encode_rle_plane(&plane, &mut out);
                }
            } else {
                for pixel in scanline {
                    out.extend_from_slice(&to_rgbe(*pixel));
                }
            }
        }
        out
    }
}

impl HdrFrame {
    /// Crops to `requested`, clipped to the frame, the way [`Frame::crop`] is.
    pub fn crop(&self, requested: Rect) -> Result<Self> {
        let bounds = Rect::new(0, 0, self.size.width, self.size.height);
        let crop = requested.intersection(bounds).ok_or_else(|| {
            VshotError::InvalidGeometry("HDR crop does not intersect the frame".into())
        })?;
        let x = crop.origin.x as usize;
        let y = crop.origin.y as usize;
        let width = crop.size.width as usize;
        let height = crop.size.height as usize;
        let source_width = self.size.width as usize;
        let mut pixels = Vec::with_capacity(width * height);
        for row in 0..height {
            let start = (y + row) * source_width + x;
            pixels.extend_from_slice(&self.pixels[start..start + width]);
        }
        Self::new(Size::new(crop.size.width, crop.size.height), pixels)
    }

    /// Pixelates `rect` with a rect-aligned block grid, averaging the **linear**
    /// values.  This is the mosaic of an HDR capture: the same geometry as
    /// [`Frame::mosaic`], but the average is taken in light rather than in
    /// gamma-encoded bytes, so a bright block stays bright in the HDR file.
    pub fn mosaic(&mut self, rect: Rect, block_size: u32) -> Result<()> {
        let (left, top, right, bottom) = self.block_bounds(rect, block_size)?;
        let (Some((visible_left, visible_right)), Some((visible_top, visible_bottom))) = (
            clip_axis(left, right, self.size.width),
            clip_axis(top, bottom, self.size.height),
        ) else {
            return Ok(());
        };
        let block = i64::from(block_size);
        let first_x = left + (visible_left - left).div_euclid(block) * block;
        let first_y = top + (visible_top - top).div_euclid(block) * block;
        let mut block_y = first_y;
        while block_y < visible_bottom {
            let y_start = block_y.max(visible_top);
            let y_end = (block_y + block).min(visible_bottom);
            let mut block_x = first_x;
            while block_x < visible_right {
                let x_start = block_x.max(visible_left);
                let x_end = (block_x + block).min(visible_right);
                if let Some(average) = self.average_region(x_start, y_start, x_end, y_end, None) {
                    self.fill_region(x_start, y_start, x_end, y_end, average, None);
                }
                block_x += block;
            }
            block_y += block;
        }
        Ok(())
    }

    /// Pixelates the ellipse inscribed in `rect` with the same block grid.
    /// Boundary blocks are averaged over the pixels inside the ellipse and
    /// filled per pixel, matching [`Frame::mosaic_ellipse`].
    pub fn mosaic_ellipse(&mut self, rect: Rect, block_size: u32) -> Result<()> {
        let (left, top, right, bottom) = self.block_bounds(rect, block_size)?;
        let (Some((visible_left, visible_right)), Some((visible_top, visible_bottom))) = (
            clip_axis(left, right, self.size.width),
            clip_axis(top, bottom, self.size.height),
        ) else {
            return Ok(());
        };
        let block = i64::from(block_size);
        let center_x = left + (right - left) / 2;
        let center_y = top + (bottom - top) / 2;
        let a = ((right - left).max(2) / 2) as f64;
        let b = ((bottom - top).max(2) / 2) as f64;
        let threshold = a * a * b * b;
        let inside = |x: i64, y: i64| -> bool {
            let dx = (x - center_x) as f64;
            let dy = (y - center_y) as f64;
            dx * dx * b * b + dy * dy * a * a <= threshold
        };
        let first_x = left + (visible_left - left) / block * block;
        let first_y = top + (visible_top - top) / block * block;
        let mut block_y = first_y;
        while block_y < visible_bottom {
            let y_start = block_y.max(visible_top);
            let y_end = (block_y + block).min(visible_bottom);
            let mut block_x = first_x;
            while block_x < visible_right {
                let x_start = block_x.max(visible_left);
                let x_end = (block_x + block).min(visible_right);
                if let Some(average) =
                    self.average_region(x_start, y_start, x_end, y_end, Some(&inside))
                {
                    self.fill_region(x_start, y_start, x_end, y_end, average, Some(&inside));
                }
                block_x += block;
            }
            block_y += block;
        }
        Ok(())
    }

    /// Smears mosaic discs of `radius` along the path, each averaged from a
    /// pre-mosaic snapshot of the frame, mirroring [`Frame::mosaic_brush`].
    pub fn mosaic_brush(&mut self, points: &[Point], radius: u32) -> Result<()> {
        if points.is_empty() {
            return Err(VshotError::InvalidGeometry(
                "HDR mosaic brush path must contain at least one point".into(),
            ));
        }
        if radius == 0 {
            return Err(VshotError::InvalidGeometry(
                "HDR mosaic brush radius must be greater than zero".into(),
            ));
        }
        let snapshot = self.clone();
        let radius = i64::from(radius.min(512));
        let step = (radius / 2).max(1) as f64;
        let mut centers: Vec<(i64, i64)> = Vec::with_capacity(points.len());
        centers.push((i64::from(points[0].x), i64::from(points[0].y)));
        for segment in points.windows(2) {
            let (ax, ay) = (f64::from(segment[0].x), f64::from(segment[0].y));
            let (bx, by) = (f64::from(segment[1].x), f64::from(segment[1].y));
            let length = (bx - ax).hypot(by - ay);
            let count = ((length / step).ceil() as usize).max(1);
            for k in 1..=count {
                let t = k as f64 / count as f64;
                centers.push((
                    (ax + (bx - ax) * t).round() as i64,
                    (ay + (by - ay) * t).round() as i64,
                ));
            }
        }
        let radius_squared = radius * radius;
        for (center_x, center_y) in centers {
            let mut sums = [0f64; 4];
            let mut count = 0u64;
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    if dx * dx + dy * dy > radius_squared {
                        continue;
                    }
                    if let Some(index) = snapshot.index_at(center_x + dx, center_y + dy) {
                        for (channel, sum) in sums.iter_mut().enumerate() {
                            *sum += f64::from(snapshot.pixels[index][channel]);
                        }
                        count += 1;
                    }
                }
            }
            if count == 0 {
                continue;
            }
            let average = average_of(sums, count);
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    if dx * dx + dy * dy > radius_squared {
                        continue;
                    }
                    if let Some(index) = self.index_at(center_x + dx, center_y + dy) {
                        self.pixels[index] = average;
                    }
                }
            }
        }
        Ok(())
    }

    fn block_bounds(&self, rect: Rect, block_size: u32) -> Result<(i64, i64, i64, i64)> {
        if rect.is_empty() {
            return Err(VshotError::InvalidGeometry(
                "mosaic rectangle dimensions must be greater than zero".into(),
            ));
        }
        if block_size == 0 {
            return Err(VshotError::InvalidGeometry(
                "mosaic block size must be greater than zero".into(),
            ));
        }
        let left = i64::from(rect.origin.x);
        let top = i64::from(rect.origin.y);
        Ok((
            left,
            top,
            left + i64::from(rect.size.width),
            top + i64::from(rect.size.height),
        ))
    }

    fn index_at(&self, x: i64, y: i64) -> Option<usize> {
        let width = i64::from(self.size.width);
        let height = i64::from(self.size.height);
        if x < 0 || y < 0 || x >= width || y >= height {
            return None;
        }
        usize::try_from(y * width + x).ok()
    }

    fn average_region(
        &self,
        x_start: i64,
        y_start: i64,
        x_end: i64,
        y_end: i64,
        include: Option<&dyn Fn(i64, i64) -> bool>,
    ) -> Option<[f32; 4]> {
        let mut sums = [0f64; 4];
        let mut count = 0u64;
        for y in y_start..y_end {
            for x in x_start..x_end {
                if include.is_some_and(|inside| !inside(x, y)) {
                    continue;
                }
                if let Some(index) = self.index_at(x, y) {
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum += f64::from(self.pixels[index][channel]);
                    }
                    count += 1;
                }
            }
        }
        (count > 0).then(|| average_of(sums, count))
    }

    fn fill_region(
        &mut self,
        x_start: i64,
        y_start: i64,
        x_end: i64,
        y_end: i64,
        value: [f32; 4],
        include: Option<&dyn Fn(i64, i64) -> bool>,
    ) {
        for y in y_start..y_end {
            for x in x_start..x_end {
                if include.is_some_and(|inside| !inside(x, y)) {
                    continue;
                }
                if let Some(index) = self.index_at(x, y) {
                    self.pixels[index] = value;
                }
            }
        }
    }
}

/// Clips `[start, end)` to `[0, limit)`, mirroring `frame::clip_range`.
fn clip_axis(start: i64, end: i64, limit: u32) -> Option<(i64, i64)> {
    let start = start.max(0);
    let end = end.min(i64::from(limit));
    (start < end).then_some((start, end))
}

fn average_of(sums: [f64; 4], count: u64) -> [f32; 4] {
    let count = count as f64;
    [
        (sums[0] / count) as f32,
        (sums[1] / count) as f32,
        (sums[2] / count) as f32,
        (sums[3] / count) as f32,
    ]
}

fn decode_transfer(value: f32, transfer: Transfer, reference_nits: f32) -> f32 {
    let reference = if reference_nits.is_finite() && reference_nits > 0.0 {
        reference_nits
    } else {
        REFERENCE_WHITE_NITS
    };
    match transfer {
        Transfer::Linear => value,
        Transfer::Srgb => srgb_eotf(value),
        // PQ is absolute: the code names a light level, so bring it into the
        // reference-white-relative scale the rest of the pipeline uses.
        Transfer::Pq => pq_eotf(value) * (PQ_PEAK_NITS / reference),
        // HLG is not: the inverse OETF only gives the scene-linear signal, and
        // [`hlg_ootf`] has to turn it into display light.  It needs the whole
        // pixel, so it runs after the triple is decoded.
        Transfer::Hlg => hlg_inverse_oetf(value),
    }
}

/// BT.2100's opto-optical transfer for HLG: the scene-linear signal
/// [`decode_transfer`] produced, to the light a display shows.
///
/// `F_D = α · Y_S^(γ−1) · E_S`, per channel, with `Y_S` the signal's own luma and
/// α the nominal peak (`HLG_PEAK_NITS`).  One factor on all three channels keeps
/// hue, and the property the standard is built around falls out: a 75 % signal —
/// HLG's reference white — lands on 203 cd/m² of a 1 000-nit display, the BT.2408
/// reference white, which the test pins.  The result is brought into the same
/// reference-white-relative scale as PQ's.
fn hlg_ootf(scene: [f32; 3], reference_nits: f32) -> [f32; 3] {
    // The BT.2020 luma weights, which is the space the signal is in here.
    let luma = 0.2627 * scene[0] + 0.6780 * scene[1] + 0.0593 * scene[2];
    // A 1 000-nit HLG system, the reference display the curve is scaled for.
    const SYSTEM_GAMMA: f32 = 1.2;
    let reference = if reference_nits.is_finite() && reference_nits > 0.0 {
        reference_nits
    } else {
        REFERENCE_WHITE_NITS
    };
    let factor = HLG_PEAK_NITS * luma.max(0.0).powf(SYSTEM_GAMMA - 1.0) / reference;
    [scene[0] * factor, scene[1] * factor, scene[2] * factor]
}

fn to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Linear RGB to Radiance RGBE: one shared exponent, three 8-bit mantissas.
fn to_rgbe(pixel: [f32; 4]) -> [u8; 4] {
    let r = pixel[0].max(0.0);
    let g = pixel[1].max(0.0);
    let b = pixel[2].max(0.0);
    let peak = r.max(g).max(b);
    if !peak.is_finite() || peak < 1.0e-32 {
        return [0, 0, 0, 0];
    }
    // v = mantissa * 2^exponent with mantissa in [0.5, 1).
    let exponent = peak.log2().floor() as i32 + 1;
    let mantissa = peak / 2f32.powi(exponent);
    let scale = mantissa * 256.0 / peak;
    [
        (r * scale).clamp(0.0, 255.0) as u8,
        (g * scale).clamp(0.0, 255.0) as u8,
        (b * scale).clamp(0.0, 255.0) as u8,
        (exponent + 128).clamp(0, 255) as u8,
    ]
}

/// One Radiance run-length plane: runs of four or more bytes are stored as
/// `(128 + count, byte)`, everything else as a literal `(count, bytes...)`.
fn encode_rle_plane(plane: &[u8], out: &mut Vec<u8>) {
    let mut index = 0;
    while index < plane.len() {
        let mut run = 1;
        while index + run < plane.len() && plane[index + run] == plane[index] && run < 127 {
            run += 1;
        }
        if run >= 4 {
            out.push(128 + run as u8);
            out.push(plane[index]);
            index += run;
            continue;
        }
        // Gather literals until a run of four starts.
        let start = index;
        while index < plane.len() {
            if index + 3 < plane.len()
                && plane[index] == plane[index + 1]
                && plane[index] == plane[index + 2]
                && plane[index] == plane[index + 3]
            {
                break;
            }
            index += 1;
            if index - start == 128 {
                break;
            }
        }
        out.push((index - start) as u8);
        out.extend_from_slice(&plane[start..index]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;

    fn one_pixel(rgba: [f32; 4]) -> HdrFrame {
        HdrFrame::new(Size::new(1, 1), vec![rgba]).unwrap()
    }

    #[test]
    fn srgb_curve_round_trips() {
        for value in [0.0, 0.02, 0.2, 0.5, 0.8, 1.0] {
            let round = srgb_oetf(srgb_eotf(value));
            assert!((round - value).abs() < 1e-4, "{value} -> {round}");
        }
    }

    #[test]
    fn pq_curve_hits_its_documented_points() {
        // 0 code is black, 1.0 code is exactly the 10 000-nit peak.
        assert!(pq_eotf(0.0).abs() < 1e-6);
        assert!((pq_eotf(1.0) - 1.0).abs() < 1e-4);
        // The 100-nit point of the PQ curve is the code 0.5081 (BT.2390 table).
        let code = pq_oetf(100.0 / 10_000.0);
        assert!((code - 0.5081).abs() < 0.002, "100 nits -> code {code}");
        assert!((pq_eotf(code) - 0.01).abs() < 1e-4);
    }

    #[test]
    fn packed_10_bit_hdr_decodes_to_scrgb() {
        // A neutral XRGB2101010 word at the top code, PQ BTC.2020: 10 000 nits,
        // i.e. the reference white scaled up, and opaque.
        let white = (0x3ffu32 << 20) | (0x3ff << 10) | 0x3ff | (3 << 30);
        let frame = HdrFrame::from_rgb10(
            &[white],
            Size::new(1, 1),
            Transfer::Pq,
            Primaries::Bt2020,
            true,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        let expected = PQ_PEAK_NITS / REFERENCE_WHITE_NITS;
        assert!((pixel[0] - expected).abs() < 0.2, "{}", pixel[0]);
        assert!((pixel[1] - expected).abs() < 0.2, "{}", pixel[1]);
        assert!((pixel[2] - expected).abs() < 0.2, "{}", pixel[2]);
        assert_eq!(pixel[3], 1.0);
        // The X form has no alpha and reads the channels from the same bits.
        let frame = HdrFrame::from_rgb10(
            &[white],
            Size::new(1, 1),
            Transfer::Pq,
            Primaries::Bt2020,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        assert_eq!(frame.pixel(0, 0).unwrap()[3], 1.0);
        // A pure-red word under a linear transfer is 10-bit red in BT.709.
        let red = 0x3ffu32 << 20;
        let frame = HdrFrame::from_rgb10(
            &[red],
            Size::new(1, 1),
            Transfer::Linear,
            Primaries::Bt709,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        assert!((pixel[0] - 1.0).abs() < 1e-4);
        assert!(pixel[1].abs() < 1e-4 && pixel[2].abs() < 1e-4);
    }

    #[test]
    fn a_1000_nit_pq_sample_decodes_to_scrgb_reference_white() {
        // 1000 nits is 1000 / 203 of the reference white, so scRGB 4.926.  The
        // word is the ten-bit code nearest the exact PQ code, so the assertion
        // carries the quantisation of a ten-bit channel.
        let code = pq_oetf(1000.0 / 10_000.0);
        let sample = (code * 1023.0).round() as u32;
        let word = (sample << 20) | (sample << 10) | sample;
        let frame = HdrFrame::from_rgb10(
            &[word],
            Size::new(1, 1),
            Transfer::Pq,
            Primaries::Bt709,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        let expected = 1000.0 / REFERENCE_WHITE_NITS;
        assert!(
            (pixel[0] - expected).abs() < expected * 0.01,
            "got {}",
            pixel[0]
        );
        assert_eq!(pixel[3], 1.0);
    }

    #[test]
    fn hlg_reference_white_and_peak_land_where_broadcast_says() {
        // BT.2408 anchors HLG on the 75 % signal: on a 1000-nit system that is
        // the 203 cd/m² reference white, i.e. 1.0 in the relative scale.  A
        // bare inverse OETF (no opto-optical transfer) would read it as 4.6 and
        // wash the whole picture out.
        let white_code = (0.75f32 * 1023.0).round() as u32;
        let word = (white_code << 20) | (white_code << 10) | white_code;
        let frame = HdrFrame::from_rgb10(
            &[word],
            Size::new(1, 1),
            Transfer::Hlg,
            Primaries::Bt2020,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        assert!((frame.pixel(0, 0).unwrap()[0] - 1.0).abs() < 0.02);
        assert!(!frame.is_hdr());

        // The top signal is the nominal 1000-nit peak, 1000 / 203 of white.
        let frame = HdrFrame::from_rgb10(
            &[0x3fff_ffff],
            Size::new(1, 1),
            Transfer::Hlg,
            Primaries::Bt2020,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        let expected = HLG_PEAK_NITS / REFERENCE_WHITE_NITS;
        assert!((frame.pixel(0, 0).unwrap()[0] - expected).abs() < 0.05);
    }

    #[test]
    fn bt2020_white_becomes_bt709_white() {
        // The 2020->709 matrix has rows that sum to one, so a neutral stays
        // neutral; without that a white HDR pixel would come out tinted.
        let frame = HdrFrame::from_rgb10(
            &[0x3fff_ffff],
            Size::new(1, 1),
            Transfer::Linear,
            Primaries::Bt2020,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        assert!((pixel[0] - 1.0).abs() < 1e-3, "{}", pixel[0]);
        assert!((pixel[1] - 1.0).abs() < 1e-3, "{}", pixel[1]);
        assert!((pixel[2] - 1.0).abs() < 1e-3, "{}", pixel[2]);
    }

    #[test]
    fn an_hdr_output_is_one_whose_transfer_is_pq_or_hlg() {
        // Detection asks the display, not the buffer: it is the transfer
        // function the compositor named, never the bit depth or the pixels.
        assert!(hdr_output().is_hdr());
        for transfer in [Transfer::Pq, Transfer::Hlg] {
            let color = OutputColor {
                transfer,
                ..hdr_output()
            };
            assert!(color.is_hdr(), "{transfer:?}");
        }
        for transfer in [Transfer::Srgb, Transfer::Linear] {
            let color = OutputColor {
                transfer,
                ..hdr_output()
            };
            assert!(!color.is_hdr(), "{transfer:?}");
        }
    }

    #[test]
    fn a_sdr_white_pixel_is_not_hdr_but_a_brighter_one_is() {
        let sdr = one_pixel([1.0, 1.0, 1.0, 1.0]);
        assert!(!sdr.is_hdr());
        let hdr = one_pixel([4.0, 3.0, 2.0, 1.0]);
        assert!(hdr.is_hdr());
    }

    #[test]
    fn a_capture_is_measured_against_the_outputs_own_white() {
        // SDR white rendered at 300 cd/m², the compositor's reference: decoded
        // against it a plain SDR pixel is 1.0 and carries no HDR, even though it
        // sits well above the scRGB reference white of 203.
        let code = pq_oetf(300.0 / 10_000.0);
        let sample = (code * 1023.0).round() as u32;
        let word = (sample << 20) | (sample << 10) | sample;
        let frame = HdrFrame::from_rgb10(
            &[word],
            Size::new(1, 1),
            Transfer::Pq,
            Primaries::Bt709,
            false,
            300.0,
        )
        .unwrap();
        assert!((frame.pixel(0, 0).unwrap()[0] - 1.0).abs() < 0.01);
        assert!(!frame.is_hdr());

        // The same word against the default reference white is light above it.
        let frame = HdrFrame::from_rgb10(
            &[word],
            Size::new(1, 1),
            Transfer::Pq,
            Primaries::Bt709,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        assert!(frame.is_hdr());
    }

    fn hdr_output() -> OutputColor {
        OutputColor {
            transfer: Transfer::Pq,
            primaries: Primaries::Bt2020,
            reference_nits: 203.0,
        }
    }

    fn rgb10(red: u32, green: u32, blue: u32) -> u32 {
        ((red & 0x3ff) << 20) | ((green & 0x3ff) << 10) | (blue & 0x3ff)
    }

    #[test]
    fn a_ten_bit_buffer_on_an_hdr_output_is_read_as_the_outputs_own_encoding() {
        // The contract the capture side relies on: a 10-bit buffer on an output
        // the compositor describes as HDR is read with that output's transfer
        // function, whatever the codes are.  Full scale is PQ's own top
        // (10 000 cd/m²), *not* sRGB white — reading it as sRGB white is what
        // turned a correct HDR capture grey.
        let color = hdr_output();
        let words = [rgb10(1023, 1023, 1023)];
        let frame = HdrFrame::from_rgb10(
            &words,
            Size::new(1, 1),
            color.transfer,
            color.primaries,
            false,
            color.reference_nits,
        )
        .unwrap();
        // 10 000 cd/m² against a 203-nit reference white is about 49 units;
        // an sRGB read of the same word would have given 1.0.
        assert!(frame.pixel(0, 0).unwrap()[0] > 45.0);
        assert!(frame.is_hdr());
    }

    #[test]
    fn the_debug_summary_measures_the_codes_a_capture_holds() {
        // The summary is diagnostics only — it decides nothing — but it has to
        // report what a capture held, so a grey result can be told apart from a
        // wrong reading.
        let words = [
            rgb10(0, 0, 0),
            rgb10(300, 300, 300),
            rgb10(1023, 1023, 1023),
        ];
        let summary = Rgb10Summary::of(&words);
        assert_eq!(summary.max, 1023);
        assert_eq!(summary.median, 300);
        assert!(summary.white_share > 0.3 && summary.white_share < 0.4);
    }

    #[test]
    fn a_frame_encodes_back_to_the_ten_bit_codes_it_came_from() {
        // The backdrop surface reads what the encoder writes, so light that
        // goes out to the compositor has to come back as the light it holds.
        let color = hdr_output();
        let words = vec![
            rgb10(0, 0, 0),
            rgb10(1023, 1023, 1023),
            rgb10(300, 500, 800),
        ];
        let frame = HdrFrame::from_rgb10(
            &words,
            Size::new(3, 1),
            color.transfer,
            color.primaries,
            false,
            color.reference_nits,
        )
        .unwrap();
        for (before, after) in words.iter().zip(&frame.to_rgb10_pq(color.reference_nits)) {
            for shift in [20, 10, 0] {
                let was = ((before >> shift) & 0x3ff) as i64;
                let now = ((after >> shift) & 0x3ff) as i64;
                assert!(
                    (was - now).abs() <= 2,
                    "{before:08x} -> {after:08x} (field {shift})"
                );
            }
        }
    }

    #[test]
    fn tone_mapping_reinhard_lands_the_peak_on_white_and_leaves_sdr_alone() {
        let frame = one_pixel([4.0, 2.0, 1.0, 1.0]);
        let sdr = frame.tone_map_to_srgb().unwrap();
        assert_eq!(sdr.pixel(Point::new(0, 0)).unwrap()[0], 255);
        // The mid 2.0 is pulled down but stays bright.
        assert!(sdr.pixel(Point::new(0, 0)).unwrap()[1] > 180);

        // A frame already inside SDR white does not get compressed.
        let flat = one_pixel([0.5, 0.5, 0.5, 1.0]);
        let sdr = flat.tone_map_to_srgb().unwrap();
        let expected = to_u8(srgb_oetf(0.5));
        assert_eq!(sdr.pixel(Point::new(0, 0)).unwrap()[0], expected);
    }

    #[test]
    fn radiance_output_round_trips_through_a_minimal_decoder() {
        let frame = HdrFrame::new(
            Size::new(16, 1),
            vec![
                [0.0, 0.0, 0.0, 1.0],
                [4.0, 2.0, 1.0, 1.0],
                [0.25, 0.5, 0.75, 1.0],
                [1.0, 1.0, 1.0, 1.0],
            ]
            .into_iter()
            .cycle()
            .take(16)
            .collect(),
        )
        .unwrap();
        let encoded = frame.encode_radiance();
        let decoded = decode_radiance(&encoded, 16, 1);
        for x in 0..16u32 {
            let expected = frame.pixel(x, 0).unwrap();
            let got = decoded[x as usize];
            let scale = expected[0].max(expected[1]).max(expected[2]).max(1e-6);
            for channel in 0..3 {
                assert!(
                    (got[channel] - expected[channel]).abs() <= scale * 0.02 + 1e-4,
                    "pixel {x} channel {channel}: {} vs {}",
                    got[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    fn an_annotation_layer_composites_in_linear_light() {
        // A mid-grey HDR pixel with a half-transparent white mark over it: the
        // blend happens in linear light, so the result is the linear mean, not
        // the sRGB mean (which would be brighter).
        let mut frame = one_pixel([0.25, 0.25, 0.25, 1.0]);
        let white = Frame::solid(Size::new(1, 1), [255, 255, 255, 128]).unwrap();
        frame.composite_srgb_layer(&white).unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        let expected = 1.0 * (128.0 / 255.0) + 0.25 * (1.0 - 128.0 / 255.0);
        assert!((pixel[0] - expected).abs() < 1e-3, "{}", pixel[0]);
        assert!((pixel[3] - 1.0).abs() < 1e-3, "{}", pixel[3]);
    }

    #[test]
    fn crop_takes_the_requested_region() {
        let frame = HdrFrame::new(
            Size::new(2, 2),
            vec![
                [0.0, 0.0, 0.0, 1.0],
                [1.0, 0.0, 0.0, 1.0],
                [2.0, 0.0, 0.0, 1.0],
                [3.0, 0.0, 0.0, 1.0],
            ],
        )
        .unwrap();
        let cropped = frame.crop(Rect::new(1, 0, 1, 2)).unwrap();
        assert_eq!(cropped.size(), Size::new(1, 2));
        assert_eq!(cropped.pixel(0, 0).unwrap()[0], 1.0);
        assert_eq!(cropped.pixel(0, 1).unwrap()[0], 3.0);
    }

    #[test]
    fn a_mosaic_averages_in_linear_light() {
        // One dark and one bright pixel in a 2-pixel block: the block average
        // is the linear mean (2.0), not the gamma-encoded mean.
        let mut frame = HdrFrame::new(
            Size::new(2, 1),
            vec![[0.0, 0.0, 0.0, 1.0], [4.0, 0.0, 0.0, 1.0]],
        )
        .unwrap();
        frame.mosaic(Rect::new(0, 0, 2, 1), 2).unwrap();
        assert!((frame.pixel(0, 0).unwrap()[0] - 2.0).abs() < 1e-5);
        assert!((frame.pixel(1, 0).unwrap()[0] - 2.0).abs() < 1e-5);
    }

    #[test]
    fn a_mosaic_brush_smears_discs_of_linear_light() {
        let mut frame = HdrFrame::new(
            Size::new(3, 3),
            (0..9).map(|index| [index as f32, 0.0, 0.0, 1.0]).collect(),
        )
        .unwrap();
        frame.mosaic_brush(&[Point::new(1, 1)], 1).unwrap();
        // The radius-1 disc covers the centre and its four neighbours; those
        // five pixels average to 4.0, and the corners outside the disc keep
        // their own values.
        for (x, y) in [(1, 1), (0, 1), (2, 1), (1, 0), (1, 2)] {
            assert!(
                (frame.pixel(x, y).unwrap()[0] - 4.0).abs() < 1e-5,
                "{x},{y} = {}",
                frame.pixel(x, y).unwrap()[0]
            );
        }
        assert_eq!(frame.pixel(0, 0).unwrap()[0], 0.0);
        assert_eq!(frame.pixel(2, 2).unwrap()[0], 8.0);
    }

    /// A tiny Radiance RGBE decoder, enough to check the encoder: header,
    /// then either flat scanlines or the 2,2,hi,lo run-length form.
    fn decode_radiance(bytes: &[u8], width: usize, height: usize) -> Vec<[f32; 3]> {
        let header_end = bytes
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .expect("header terminator")
            + 2;
        // Skip the resolution line, "-Y <height> +X <width>".
        let mut offset = header_end;
        while bytes[offset] != b'\n' {
            offset += 1;
        }
        offset += 1;
        let mut pixels = Vec::with_capacity(width * height);
        for _ in 0..height {
            let rle = width >= 8 && bytes[offset] == 2 && bytes[offset + 1] == 2;
            if rle {
                assert_eq!(
                    ((bytes[offset + 2] as usize) << 8) | bytes[offset + 3] as usize,
                    width
                );
                offset += 4;
                let mut planes: [Vec<u8>; 4] = [
                    Vec::with_capacity(width),
                    Vec::with_capacity(width),
                    Vec::with_capacity(width),
                    Vec::with_capacity(width),
                ];
                for plane in planes.iter_mut() {
                    while plane.len() < width {
                        let count = bytes[offset] as usize;
                        offset += 1;
                        if count > 128 {
                            let value = bytes[offset];
                            offset += 1;
                            for _ in 0..count - 128 {
                                plane.push(value);
                            }
                        } else {
                            plane.extend_from_slice(&bytes[offset..offset + count]);
                            offset += count;
                        }
                    }
                }
                for (((a, b), c), d) in planes[0]
                    .iter()
                    .zip(&planes[1])
                    .zip(&planes[2])
                    .zip(&planes[3])
                {
                    pixels.push(rgbe_to_rgb([*a, *b, *c, *d]));
                }
            } else {
                for _ in 0..width {
                    let rgbe = [
                        bytes[offset],
                        bytes[offset + 1],
                        bytes[offset + 2],
                        bytes[offset + 3],
                    ];
                    offset += 4;
                    pixels.push(rgbe_to_rgb(rgbe));
                }
            }
        }
        pixels
    }

    fn rgbe_to_rgb(rgbe: [u8; 4]) -> [f32; 3] {
        if rgbe[3] == 0 {
            return [0.0, 0.0, 0.0];
        }
        let scale = 2f32.powi(rgbe[3] as i32 - 128 - 8);
        [
            (rgbe[0] as f32 + 0.5) * scale,
            (rgbe[1] as f32 + 0.5) * scale,
            (rgbe[2] as f32 + 0.5) * scale,
        ]
    }
}
