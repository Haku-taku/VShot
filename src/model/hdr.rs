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
//! * `<name>.png`, 8-bit sRGB, the SDR view of the same content;
//! * `<name>.hdr`, Radiance RGBE, the HDR content itself.
//!
//! The SDR view is **the compositor's own**, not one computed here: a capture
//! reads its SDR half over a connection that is not a `wp_color_manager_v1`
//! client, and the compositor answers that with the rendition it already
//! produced for ordinary clients — tone-mapped by the component that owns the
//! display's colour management.  [`HdrFrame::tone_map_to_srgb`] is this module's
//! own conversion, kept for a frame with no compositor rendition behind it, and
//! is deliberately not what a capture saves.
//!
//! Both the PQ curve and the BT.2020↔BT.709 matrices are the ones a colour
//! pipeline is expected to use; the same constants appear in the reference
//! implementation this follows (Starward's `HdrToneMapEffect`/`HDR10ToScRGB`).

use crate::error::{Result, VshotError};
use crate::geometry::{Point, Rect, Size};
use crate::model::Frame;
use crate::parallel::{collect_rows, map_rows};

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

/// The primaries an RGB buffer carries.
///
/// The sets this pipeline has a name for are named, so a description reads as
/// what it is; a gamut it has no name for keeps the matrix its own
/// chromaticities imply.  A Display P3 or an EDID-only description therefore
/// converts correctly instead of being read as BT.709, which shifted every
/// colour it carried.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Primaries {
    Bt709,
    DisplayP3,
    Bt2020,
    /// A gamut with no name here: the matrix from its linear RGB to BT.709
    /// linear, built from the chromaticities the compositor reported.
    Custom { to_bt709: [[f32; 3]; 3] },
}

impl Primaries {
    /// Linear RGB in this gamut to BT.709 linear (both D65).
    pub fn to_bt709(&self) -> [[f32; 3]; 3] {
        match self {
            Primaries::Bt709 => IDENTITY,
            Primaries::DisplayP3 => DISPLAY_P3_TO_BT709,
            Primaries::Bt2020 => BT2020_TO_BT709,
            Primaries::Custom { to_bt709 } => *to_bt709,
        }
    }

    /// Linear RGB in this gamut to BT.2020 linear — the space a colour-managed
    /// HDR surface is described in.
    ///
    /// The named gamuts have their own matrix rather than a product of two:
    /// composing BT.709 into the path would leave off-diagonal terms of ~1e-6
    /// behind, and PQ's toe turns a linear 1e-6 into a code of a few, so a
    /// black pixel would come back faintly lit.
    pub fn to_bt2020(&self) -> [[f32; 3]; 3] {
        match self {
            Primaries::Bt709 => BT709_TO_BT2020,
            Primaries::DisplayP3 => DISPLAY_P3_TO_BT2020,
            Primaries::Bt2020 => IDENTITY,
            Primaries::Custom { .. } => multiply3(BT709_TO_BT2020, self.to_bt709()),
        }
    }

    /// BT.709 linear into this gamut: what a mark drawn in sRGB needs before it
    /// can be blended into a frame of this gamut.
    pub fn from_bt709(&self) -> [[f32; 3]; 3] {
        invert3(self.to_bt709())
    }

    /// The CIE xy chromaticities of the three primaries, for a file that has to
    /// declare them.
    pub fn chromaticities(&self) -> [(f32, f32); 3] {
        const BT709: [(f32, f32); 3] = [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)];
        const DISPLAY_P3: [(f32, f32); 3] = [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)];
        const BT2020: [(f32, f32); 3] = [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)];
        match self {
            Primaries::Bt709 => BT709,
            Primaries::DisplayP3 => DISPLAY_P3,
            Primaries::Bt2020 => BT2020,
            // Recovered from the matrix: its columns are the primaries, and
            // each column's chromaticity is the column normalised to a sum of
            // one.
            Primaries::Custom { .. } => {
                let xyz = multiply3(BT709_TO_XYZ, self.to_bt709());
                let mut found = [(0.0f32, 0.0f32); 3];
                for (index, entry) in found.iter_mut().enumerate() {
                    let column = [xyz[0][index], xyz[1][index], xyz[2][index]];
                    let sum = column[0] + column[1] + column[2];
                    if sum > 0.0 {
                        *entry = (column[0] / sum, column[1] / sum);
                    }
                }
                found
            }
        }
    }

    /// The gamut a `wp_color_manager_v1` description's chromaticities describe.
    /// The protocol carries no white point, so D65 is the one assumed; a set
    /// that matches one of the named gamuts reads as that name, and anything
    /// else keeps the matrix its coordinates imply.
    pub fn from_chromaticities(r: (f32, f32), g: (f32, f32), b: (f32, f32)) -> Self {
        let to_bt709 = multiply3(XYZ_TO_BT709, rgb_to_xyz([r, g, b], D65));
        for (known, name) in [
            (IDENTITY, Primaries::Bt709),
            (DISPLAY_P3_TO_BT709, Primaries::DisplayP3),
            (BT2020_TO_BT709, Primaries::Bt2020),
        ] {
            if matrices_close(to_bt709, known) {
                return name;
            }
        }
        Primaries::Custom { to_bt709 }
    }
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
///
/// Kept next to [`pq_eotf`] because they are two halves of one curve, and
/// `pub(crate)` because the pin surface encodes a rim colour with it: a colour
/// from the config is sRGB, and the PQ code of its own light is what a surface
/// described as this output wants.
pub(crate) fn pq_encode(luminance: f32) -> f32 {
    pq_oetf(luminance)
}

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

const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

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

/// Display P3 to BT.709, linear light.  P3's green and red sit between the two
/// BT sets, and its blue is BT.709's, so a P3 capture read as BT.709 — which is
/// what "the nearer of the two" used to do — came out too saturated.
const DISPLAY_P3_TO_BT709: [[f32; 3]; 3] = [
    [1.224_940, -0.224_940, 0.0],
    [-0.042_057, 1.042_057, 0.0],
    [-0.019_638, -0.078_636, 1.098_274],
];

/// Display P3 to BT.2020, linear light — what a P3 capture needs before it goes
/// to a BT.2020 surface.
const DISPLAY_P3_TO_BT2020: [[f32; 3]; 3] = [
    [0.753_833, 0.198_597, 0.047_570],
    [0.045_744, 0.941_777, 0.012_479],
    [-0.001_210, 0.017_602, 0.983_609],
];

/// BT.709 linear to CIE XYZ (D65): the other direction of `XYZ_TO_BT709`, and
/// what recovers a gamut's chromaticities from its conversion matrix.
const BT709_TO_XYZ: [[f32; 3]; 3] = [
    [0.412_391, 0.357_584, 0.180_481],
    [0.212_639, 0.715_169, 0.072_192],
    [0.019_331, 0.119_195, 0.950_532],
];

/// CIE XYZ (D65) to BT.709 linear: the other half of `rgb_to_xyz`.
const XYZ_TO_BT709: [[f32; 3]; 3] = [
    [3.240_970, -1.537_383, -0.498_611],
    [-0.969_244, 1.875_968, 0.041_555],
    [0.055_630, -0.203_977, 1.056_972],
];

/// The D65 white point, the one `wp_color_manager_v1` assumes.
const D65: (f32, f32) = (0.3127, 0.3290);

fn multiply(matrix: [[f32; 3]; 3], rgb: [f32; 3]) -> [f32; 3] {
    [
        matrix[0][0] * rgb[0] + matrix[0][1] * rgb[1] + matrix[0][2] * rgb[2],
        matrix[1][0] * rgb[0] + matrix[1][1] * rgb[1] + matrix[1][2] * rgb[2],
        matrix[2][0] * rgb[0] + matrix[2][1] * rgb[1] + matrix[2][2] * rgb[2],
    ]
}

/// The product `first * second` of two 3×3 matrices.
fn multiply3(first: [[f32; 3]; 3], second: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let mut product = [[0.0f32; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            product[row][column] = (0..3).map(|k| first[row][k] * second[k][column]).sum();
        }
    }
    product
}

/// Whether two matrices agree to within the precision a description's
/// millionth-unit coordinates can carry.
fn matrices_close(first: [[f32; 3]; 3], second: [[f32; 3]; 3]) -> bool {
    first
        .iter()
        .flatten()
        .zip(second.iter().flatten())
        .all(|(a, b)| (a - b).abs() < 5.0e-3)
}

/// The linear RGB → CIE XYZ matrix a set of primaries and a white point imply:
/// each primary at full scale, scaled so that the three of them add up to the
/// white point.
fn rgb_to_xyz(primaries: [(f32, f32); 3], white: (f32, f32)) -> [[f32; 3]; 3] {
    let xyz = |(x, y): (f32, f32)| [x / y, 1.0, (1.0 - x - y) / y];
    let [r, g, b] = primaries.map(xyz);
    // Columns are the primaries at unit scale.
    let matrix = [
        [r[0], g[0], b[0]],
        [r[1], g[1], b[1]],
        [r[2], g[2], b[2]],
    ];
    let scale = solve3(matrix, xyz(white));
    [
        [matrix[0][0] * scale[0], matrix[0][1] * scale[1], matrix[0][2] * scale[2]],
        [matrix[1][0] * scale[0], matrix[1][1] * scale[1], matrix[1][2] * scale[2]],
        [matrix[2][0] * scale[0], matrix[2][1] * scale[1], matrix[2][2] * scale[2]],
    ]
}

/// The inverse of a 3×3 matrix, column by column.
fn invert3(matrix: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let column = |index: usize| {
        let mut unit = [0.0f32; 3];
        unit[index] = 1.0;
        solve3(matrix, unit)
    };
    let (first, second, third) = (column(0), column(1), column(2));
    [
        [first[0], second[0], third[0]],
        [first[1], second[1], third[1]],
        [first[2], second[2], third[2]],
    ]
}

/// Solves `matrix * x = target` by Cramer's rule; the primaries always give a
/// well-conditioned matrix, and a degenerate one would fall back to zeros.
fn solve3(matrix: [[f32; 3]; 3], target: [f32; 3]) -> [f32; 3] {
    let determinant = |m: [[f32; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let base = determinant(matrix);
    if base.abs() < 1.0e-12 {
        return [0.0; 3];
    }
    let with = |column: usize| {
        let mut copy = matrix;
        for row in 0..3 {
            copy[row][column] = target[row];
        }
        determinant(copy) / base
    };
    [with(0), with(1), with(2)]
}

/// Pulls an out-of-gamut linear colour back in by moving it toward its own
/// luminance until its smallest component is zero.
///
/// The alternative — clamping each component at zero — shifts the hue of every
/// colour the target gamut cannot hold, which is what a saturated HDR colour
/// looked like after the conversion to BT.709.  Moving along the line to the
/// achromatic axis gives up only the chroma that does not fit and keeps the
/// hue, which is what a gamut map is for.
fn map_into_gamut(rgb: [f32; 3]) -> [f32; 3] {
    let smallest = rgb[0].min(rgb[1]).min(rgb[2]);
    if smallest >= 0.0 {
        return rgb;
    }
    let luma = 0.212_6 * rgb[0] + 0.715_2 * rgb[1] + 0.072_2 * rgb[2];
    let span = luma - smallest;
    if span <= 0.0 {
        let grey = luma.max(0.0);
        return [grey; 3];
    }
    let toward = (-smallest / span).clamp(0.0, 1.0);
    [
        (rgb[0] + toward * (luma - rgb[0])).max(0.0),
        (rgb[1] + toward * (luma - rgb[1])).max(0.0),
        (rgb[2] + toward * (luma - rgb[2])).max(0.0),
    ]
}

// --- the frame ------------------------------------------------------------

/// A frame in linear light, one `[r, g, b, a]` per pixel, in `primaries`.
///
/// The gamut travels with the frame because the two consumers want different
/// spaces: a colour-managed surface is described in the output's own primaries
/// and wants the frame as it was captured, while an 8-bit PNG or a Radiance
/// file carries no colorimetry and has to be BT.709.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrFrame {
    size: Size,
    pixels: Vec<[f32; 4]>,
    primaries: Primaries,
}

impl HdrFrame {
    /// Wraps already-linear pixels in BT.709, the space every test and every
    /// caller without a compositor description uses.
    pub fn new(size: Size, pixels: Vec<[f32; 4]>) -> Result<Self> {
        Self::in_primaries(size, pixels, Primaries::Bt709)
    }

    /// Wraps already-linear pixels that are in `primaries`.
    pub fn in_primaries(
        size: Size,
        pixels: Vec<[f32; 4]>,
        primaries: Primaries,
    ) -> Result<Self> {
        let expected = size.area()?;
        if pixels.len() != expected {
            return Err(VshotError::InvalidGeometry(format!(
                "HDR frame has {} pixels, expected {expected}",
                pixels.len()
            )));
        }
        Ok(Self {
            size,
            pixels,
            primaries,
        })
    }

    /// The gamut the pixels are in.
    pub const fn primaries(&self) -> Primaries {
        self.primaries
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
        let mut pixels = vec![[0.0f32; 4]; expected];
        map_rows(&mut pixels, size.width as usize, |offset, row| {
            for (index, destination) in row.iter_mut().enumerate() {
                let word = words[offset + index];
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
                // HLG needs the whole triple: BT.2100's opto-optical transfer
                // takes the frame's own luma, so it cannot ride on a per-channel
                // decode.
                if transfer == Transfer::Hlg {
                    rgb = hlg_ootf(rgb, reference_nits);
                }
                // The gamut is *kept*, not converted: a colour-managed surface
                // is described in this same space, so the HDR half reaches the
                // panel with its wide gamut intact.  The 8-bit consumers
                // convert on the way out.
                let alpha = if alpha {
                    ((word >> 30) & 0x3) as f32 / 3.0
                } else {
                    1.0
                };
                *destination = [rgb[0], rgb[1], rgb[2], alpha];
            }
        });
        Self::in_primaries(size, pixels, primaries)
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
        let mut words = vec![0u32; self.pixels.len()];
        map_rows(&mut words, self.size.width as usize, |offset, row| {
            for (index, destination) in row.iter_mut().enumerate() {
                let pixel = self.pixels[offset + index];
                let rgb = multiply(self.primaries.to_bt2020(), [pixel[0], pixel[1], pixel[2]]);
                *destination =
                    (3 << 30) | (code(rgb[0]) << 20) | (code(rgb[1]) << 10) | code(rgb[2]);
            }
        });
        words
    }

    /// Tone-maps the whole frame to an 8-bit sRGB frame, the SDR half of the
    /// pair.
    ///
    /// **The capture path does not use this.**  A capture takes its SDR half from
    /// the compositor, which hands an ordinary client the SDR rendition it
    /// produced itself — tone-mapped against that display's own peak by the
    /// component that owns its colour management — and this map cannot match
    /// that (see `capture::wlr::WlrCapture::capture` and `main::sdr_and_hdr`).
    /// It is kept as the model's own conversion, for a caller that holds an HDR
    /// frame with no compositor rendition behind it; the SDR half of a capture is
    /// never such a frame.
    ///
    /// The map is **display-referred**: linear 1.0 is the output's own SDR white
    /// (see [`OutputColor::reference_nits`]) and it lands on sRGB white, so a
    /// sample inside the SDR range keeps exactly the code its light deserves.
    /// The frame's own peak deliberately does **not** set the white point: with
    /// that, one capture of a window would come out at a different brightness
    /// from the next depending on what else shared the frame, and a pinned copy
    /// of a capture would not match the content it was taken from.
    ///
    /// Light beyond SDR white has nowhere to go in an 8-bit SDR image — the
    /// format ends at white — so it is rolled off: the whole triple is scaled by
    /// one factor until its brightest channel lands on white, which keeps hue
    /// and clips only what the format cannot hold.  That is a blunt roll-off
    /// beside a real tone map's knee, which is the other reason the compositor's
    /// own rendition is what a capture saves.  Alpha is quantised to a byte like
    /// every other channel (a capture is opaque).
    pub fn tone_map_to_srgb(&self) -> Result<Frame> {
        let mut bytes = vec![0u8; self.pixels.len() * 4];
        map_rows(&mut bytes, self.size.width as usize * 4, |offset, row| {
            for (index, destination) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let pixel = self.pixels[offset / 4 + index];
                // Into BT.709 first, and out of gamut by desaturation rather
                // than by clipping: clamping a negative component moves a
                // saturated colour's hue, which is the inaccurate colour a
                // wide-gamut capture used to come out with.
                let rgb = map_into_gamut(multiply(
                    self.primaries.to_bt709(),
                    [pixel[0], pixel[1], pixel[2]],
                ));
                // One factor for the whole triple, so hue survives the roll-off
                // of anything brighter than white.
                let peak = rgb[0].max(rgb[1]).max(rgb[2]);
                let scale = if peak > 1.0 { 1.0 / peak } else { 1.0 };
                destination[0] = to_u8(srgb_oetf(rgb[0] * scale));
                destination[1] = to_u8(srgb_oetf(rgb[1] * scale));
                destination[2] = to_u8(srgb_oetf(rgb[2] * scale));
                destination[3] = to_u8(pixel[3]);
            }
        });
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
        let layer = layer.pixels();
        map_rows(&mut self.pixels, self.size.width as usize, |offset, row| {
            for (index, destination) in row.iter_mut().enumerate() {
                let rgba = &layer[(offset + index) * 4..][..4];
                let alpha = f32::from(rgba[3]) / 255.0;
                if alpha == 0.0 {
                    continue;
                }
                // The layer is 8-bit sRGB (BT.709) and the frame is in its own
                // gamut, so the mark is brought into that gamut before the
                // blend; otherwise a mark on a BT.2020 frame would be added as
                // if its values were BT.2020, which they are not.
                let source = multiply(
                    self.primaries.from_bt709(),
                    [
                        srgb_eotf(f32::from(rgba[0]) / 255.0),
                        srgb_eotf(f32::from(rgba[1]) / 255.0),
                        srgb_eotf(f32::from(rgba[2]) / 255.0),
                    ],
                );
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
        });
        Ok(())
    }

    /// Encodes the frame as Radiance RGBE (`.hdr`).  The values are linear
    /// light, which is exactly what the format holds, so an HDR viewer shows
    /// the content as captured.
    ///
    /// The pixels go out **as captured**, in the output's own primaries: the
    /// file is the archival half, and converting it would throw away the wide
    /// gamut for good.  RGBE has no colorimetry of its own, so the gamut is
    /// declared in a `PRIMARIES=` header — the line Radiance reads.  Readers
    /// that ignore it (ffmpeg and ImageMagick both do) assume Rec.709 and show
    /// a wide gamut over-saturated; the SDR half beside it is the one that is
    /// converted for them.
    pub fn encode_radiance(&self) -> Vec<u8> {
        let width = self.size.width;
        let height = self.size.height;
        let mut out = Vec::new();
        out.extend_from_slice(b"#?RADIANCE\n");
        out.extend_from_slice(b"FORMAT=32-bit_rle_rgbe\n");
        let [r, g, b] = self.primaries.chromaticities();
        out.extend_from_slice(
            format!(
                "PRIMARIES={:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6}\n",
                r.0, r.1, g.0, g.1, b.0, b.1, D65.0, D65.1
            )
            .as_bytes(),
        );
        out.extend_from_slice(b"\n");
        out.extend_from_slice(format!("-Y {height} +X {width}\n").as_bytes());
        let rle = (8..=0x7fff).contains(&width);
        // Scanlines are independent — a run never crosses one — so each is
        // encoded on its own and they are laid down in order.  The chunks the
        // encoder is split into are whole rows, and every row in one gets its
        // own scanline: emitting a chunk as a single scanline would shift every
        // row after the first.
        for rows in collect_rows(&self.pixels, width as usize, |chunk| {
            let mut bytes = Vec::new();
            for scanline in chunk.chunks(width as usize) {
                encode_scanline(scanline, width, rle, &mut bytes);
            }
            bytes
        }) {
            out.extend_from_slice(&rows);
        }
        out
    }
}

/// Appends one Radiance scanline: the run-length form when the width allows it,
/// which is every width but a tiny one, and the flat form otherwise.
fn encode_scanline(scanline: &[[f32; 4]], width: u32, rle: bool, out: &mut Vec<u8>) {
    if rle {
        out.extend_from_slice(&[2, 2, (width >> 8) as u8, (width & 0xff) as u8]);
        // Four component planes, each run-length encoded on its own.
        for channel in 0..4 {
            let plane: Vec<u8> = scanline
                .iter()
                .map(|pixel| to_rgbe(*pixel)[channel])
                .collect();
            encode_rle_plane(&plane, out);
        }
    } else {
        for pixel in scanline {
            out.extend_from_slice(&to_rgbe(*pixel));
        }
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
        Self::in_primaries(
            Size::new(crop.size.width, crop.size.height),
            pixels,
            self.primaries,
        )
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
        // neutral; without that a white HDR pixel would come out tinted.  The
        // conversion happens when the frame is written out as SDR, not when it
        // is decoded: the frame itself keeps the output's gamut.
        let frame = HdrFrame::from_rgb10(
            &[0x3fff_ffff],
            Size::new(1, 1),
            Transfer::Linear,
            Primaries::Bt2020,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        assert_eq!(frame.primaries(), Primaries::Bt2020);
        let pixel = frame.tone_map_to_srgb().unwrap().pixel(Point::new(0, 0)).unwrap();
        for channel in &pixel[..3] {
            assert!((i32::from(*channel) - 255).abs() <= 1, "{pixel:?}");
        }
    }

    #[test]
    fn a_gamut_is_read_from_its_chromaticities() {
        assert_eq!(
            Primaries::from_chromaticities((0.708, 0.292), (0.170, 0.797), (0.131, 0.046)),
            Primaries::Bt2020
        );
        assert_eq!(
            Primaries::from_chromaticities((0.640, 0.330), (0.300, 0.600), (0.150, 0.060)),
            Primaries::Bt709
        );
        assert_eq!(
            Primaries::from_chromaticities((0.680, 0.320), (0.265, 0.690), (0.150, 0.060)),
            Primaries::DisplayP3
        );
        // A gamut that is none of them keeps its own matrix instead of being
        // read as BT.709, which is what shifted a monitor's colours.
        let custom =
            Primaries::from_chromaticities((0.700, 0.300), (0.200, 0.750), (0.140, 0.050));
        assert!(matches!(custom, Primaries::Custom { .. }));
        // It shares the D65 white point, so a neutral stays neutral through it.
        let neutral = multiply(custom.to_bt709(), [0.5, 0.5, 0.5]);
        for channel in neutral {
            assert!((channel - 0.5).abs() < 2.0e-3, "{neutral:?}");
        }
    }

    #[test]
    fn an_out_of_gamut_colour_is_mapped_not_clipped() {
        // A saturated BT.2020 green is outside sRGB.  Clamping its negative red
        // and blue leaves sRGB's own pure green, which is a different hue; the
        // map pulls the colour toward the white point until it fits, which
        // keeps the hue and gives up only the chroma that does not fit.
        let frame = HdrFrame::in_primaries(
            Size::new(1, 1),
            vec![[0.0, 1.0, 0.0, 1.0]],
            Primaries::Bt2020,
        )
        .unwrap();
        let pixel = frame.tone_map_to_srgb().unwrap().pixel(Point::new(0, 0)).unwrap();
        // Green stays the largest and red stays zero, but blue is not clamped
        // away: the result is the same hue at lower chroma, not sRGB's green.
        assert!(pixel[2] > 0, "blue was clipped away: {pixel:?}");
        assert!(pixel[1] >= pixel[0] && pixel[1] >= pixel[2], "{pixel:?}");
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
            // The saturated primaries are outside sRGB: a detour through BT.709
            // would clip them and the codes would come back desaturated.
            rgb10(1023, 0, 0),
            rgb10(0, 1023, 0),
            rgb10(0, 0, 1023),
        ];
        let frame = HdrFrame::from_rgb10(
            &words,
            Size::new(6, 1),
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
    fn tone_mapping_is_anchored_to_sdr_white_whatever_else_is_in_the_frame() {
        // A sample inside SDR white keeps the code its own light deserves — the
        // frame's peak is not a white point — so the same content tone-maps to
        // the same bytes whether or not something brighter shares the frame.
        let alone = HdrFrame::new(
            Size::new(2, 1),
            vec![[0.5, 0.25, 0.1, 1.0], [0.5, 0.25, 0.1, 1.0]],
        )
        .unwrap();
        let beside = HdrFrame::new(
            Size::new(2, 1),
            vec![[0.5, 0.25, 0.1, 1.0], [4.0, 2.0, 1.0, 1.0]],
        )
        .unwrap();
        for channel in 0..3 {
            assert_eq!(
                alone
                    .tone_map_to_srgb()
                    .unwrap()
                    .pixel(Point::new(0, 0))
                    .unwrap()[channel],
                beside
                    .tone_map_to_srgb()
                    .unwrap()
                    .pixel(Point::new(0, 0))
                    .unwrap()[channel],
                "channel {channel} moved with the frame's peak"
            );
        }
        // SDR white is sRGB white, exactly.
        let white = one_pixel([1.0, 1.0, 1.0, 1.0]);
        assert_eq!(
            white
                .tone_map_to_srgb()
                .unwrap()
                .pixel(Point::new(0, 0))
                .unwrap()[0],
            255
        );
        // And a value with no light of its own stays black.
        let black = one_pixel([0.0, 0.0, 0.0, 1.0]);
        assert_eq!(
            black
                .tone_map_to_srgb()
                .unwrap()
                .pixel(Point::new(0, 0))
                .unwrap()[0],
            0
        );
    }

    #[test]
    fn tone_mapping_rolls_light_over_white_off_and_keeps_hue() {
        // Light beyond SDR white cannot be shown by an 8-bit SDR image: the
        // brightest channel lands on white and the rest of the triple is scaled
        // with it, which clips only that light and leaves the hue alone.
        let frame = one_pixel([4.0, 2.0, 1.0, 1.0]);
        let sdr = frame.tone_map_to_srgb().unwrap();
        let pixel = sdr.pixel(Point::new(0, 0)).unwrap();
        assert_eq!(pixel[0], 255, "the brightest channel did not reach white");
        // The ratio between the channels is what the light had, so the mark on
        // the image keeps its colour instead of washing out towards white.
        assert_eq!(pixel[1], to_u8(srgb_oetf(0.5)));
        assert_eq!(pixel[2], to_u8(srgb_oetf(0.25)));
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

    /// Every per-pixel pass splits a frame into row chunks, and one whose body
    /// still assumed a chunk was a single row left the rest of the buffer as it
    /// was allocated — invisible on the few-pixel frames the other tests use,
    /// and a screenshot in black bands on a real one.  This frame is big enough
    /// to be split, which is the whole point of it.
    #[test]
    fn a_frame_big_enough_to_be_split_is_filled_row_by_row() {
        let side = 640u32;
        let (width, height) = (side, side);
        // Every row a distinct neutral grey: the BT.2020 to BT.709 matrix keeps a
        // neutral neutral, so every channel of every pixel is positive and a row
        // that was never written is unmistakable.
        let words: Vec<u32> = (0..width * height)
            .map(|index| {
                let code = 100 + (index / width) % 900;
                (code << 20) | (code << 10) | code
            })
            .collect();
        let frame = HdrFrame::from_rgb10(
            &words,
            Size::new(width, height),
            Transfer::Pq,
            Primaries::Bt2020,
            false,
            REFERENCE_WHITE_NITS,
        )
        .unwrap();
        for y in 0..height {
            for x in [0, width / 2, width - 1] {
                let pixel = frame.pixel(x, y).unwrap();
                assert!(
                    pixel[0] > 0.0 && pixel[1] > 0.0 && pixel[2] > 0.0,
                    "row {y} column {x} was left as it was allocated: {pixel:?}"
                );
            }
        }

        // The passes that go the other way have to cover the whole frame too.
        let back = frame.to_rgb10_pq(REFERENCE_WHITE_NITS);
        assert!(
            back.iter().all(|word| (word >> 20) & 0x3ff > 0),
            "a ten-bit re-encode left pixels unset"
        );
        let sdr = frame.tone_map_to_srgb().unwrap();
        assert!(
            sdr.pixels().iter().step_by(4).all(|red| *red > 0),
            "the SDR tone map left pixels unset"
        );

        // A Radiance file whose chunks were emitted as single scanlines puts
        // every row after the first of each chunk in the wrong place, so the
        // decoder has to hand back what went in.
        let encoded = frame.encode_radiance();
        let decoded = decode_radiance(&encoded, width as usize, height as usize);
        for y in [0usize, 1, 100, 639] {
            let expected = frame.pixel(0, y as u32).unwrap();
            let got = decoded[y * width as usize];
            for channel in 0..3 {
                assert!(
                    (got[channel] - expected[channel]).abs() <= expected[channel] * 0.02 + 1e-4,
                    "row {y} channel {channel}: {} vs {}",
                    got[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    fn the_radiance_file_keeps_the_captures_own_gamut() {
        // The archival half is not converted: a wide-gamut capture is written
        // as it came, with its primaries declared, so the data survives even
        // though RGBE has no colorimetry field of its own.  Converting it to
        // BT.709 would throw the wide gamut away for good.
        let frame = HdrFrame::in_primaries(
            Size::new(1, 1),
            vec![[0.0, 1.0, 0.0, 1.0]],
            Primaries::Bt2020,
        )
        .unwrap();
        let encoded = frame.encode_radiance();
        let header_end = encoded.windows(2).position(|pair| pair == b"\n\n").unwrap();
        let header = String::from_utf8_lossy(&encoded[..header_end]);
        assert!(
            header.contains("PRIMARIES=0.708000 0.292000 0.170000 0.797000 0.131000 0.046000"),
            "{header}"
        );
        // A pure BT.2020 green stays one: its mantissa is half scale, 128.  A
        // conversion to BT.709 would clip the negative red and blue and raise
        // green to 144.
        let pixels = &encoded[header_end + 2..];
        let start = pixels
            .windows(4)
            .position(|pixel| pixel[1] > 100)
            .expect("the green scanline is there");
        assert_eq!(&pixels[start..start + 4], &[0, 128, 0, 129]);
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
