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
use crate::geometry::Size;
use crate::model::Frame;

/// The HDR reference white: linear value 1.0 is this many cd/m².
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

/// How an HDR frame is brought down to SDR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToneMap {
    /// Straight clip: highlights above white flatten.  Fast, and the choice
    /// when the content is barely over SDR.
    Clip,
    /// Extended Reinhard with the frame's own peak as the white point, so the
    /// brightest sample lands on white and nothing clips: hue is preserved
    /// because the whole triple is scaled by one factor.
    Reinhard,
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

    pub fn pixels(&self) -> &[[f32; 4]] {
        &self.pixels
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

    /// Decodes an ordinary 8-bit sRGB frame into linear light.
    pub fn from_srgb(frame: &Frame) -> Self {
        let size = frame.size();
        let source = frame.pixels();
        let mut pixels = Vec::with_capacity(source.len() / 4);
        for rgba in source.chunks_exact(4) {
            pixels.push([
                srgb_eotf(f32::from(rgba[0]) / 255.0),
                srgb_eotf(f32::from(rgba[1]) / 255.0),
                srgb_eotf(f32::from(rgba[2]) / 255.0),
                f32::from(rgba[3]) / 255.0,
            ]);
        }
        Self { size, pixels }
    }

    /// Decodes RGBA `u16` samples (PNG's own 16-bit order) with a declared
    /// transfer function and primaries into linear scRGB.
    ///
    /// The colour-triple samples go through their transfer function; the alpha
    /// sample never does — alpha is linear whatever the colour encoding is.
    pub fn from_samples(
        samples: &[u16],
        size: Size,
        transfer: Transfer,
        primaries: Primaries,
    ) -> Result<Self> {
        let expected = size
            .area()?
            .checked_mul(4)
            .ok_or_else(|| VshotError::InvalidGeometry("HDR sample buffer is too large".into()))?;
        if samples.len() != expected {
            return Err(VshotError::InvalidGeometry(format!(
                "HDR frame has {} samples, expected {expected}",
                samples.len()
            )));
        }
        let mut pixels = Vec::with_capacity(size.area()?);
        for rgba in samples.chunks_exact(4) {
            let mut rgb = [
                decode_transfer(f32::from(rgba[0]) / 65535.0, transfer),
                decode_transfer(f32::from(rgba[1]) / 65535.0, transfer),
                decode_transfer(f32::from(rgba[2]) / 65535.0, transfer),
            ];
            if primaries == Primaries::Bt2020 {
                rgb = multiply(BT2020_TO_BT709, rgb);
            }
            pixels.push([rgb[0], rgb[1], rgb[2], f32::from(rgba[3]) / 65535.0]);
        }
        Self::new(size, pixels)
    }

    /// Decodes a big-endian RGBA `u16` byte buffer, which is how PNG stores
    /// 16-bit samples.  HDR10 (BT.2020 + PQ) is the format this is for.
    pub fn from_hdr10_be_bytes(bytes: &[u8], size: Size) -> Result<Self> {
        if !bytes.len().is_multiple_of(2) {
            return Err(VshotError::InvalidGeometry(
                "HDR byte buffer is not a whole number of 16-bit samples".into(),
            ));
        }
        let samples: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        Self::from_samples(&samples, size, Transfer::Pq, Primaries::Bt2020)
    }

    /// Decodes 10-bit RGB packed the way DRM's `XRGB2101010`/`ARGB2101010`
    /// pack it (bits 20..30, 10..20, 0..10, alpha in the top two).  This is the
    /// shape a compositor's HDR screencopy buffer arrives in, so it is the
    /// decode the capture side will use once it negotiates such a buffer.
    /// With `alpha` false the top bits are the `X` padding and the pixel is
    /// taken as opaque.
    pub fn from_rgb10(
        words: &[u32],
        size: Size,
        transfer: Transfer,
        primaries: Primaries,
        alpha: bool,
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
                decode_transfer(((word >> 20) & 0x3ff) as f32 / 1023.0, transfer),
                decode_transfer(((word >> 10) & 0x3ff) as f32 / 1023.0, transfer),
                decode_transfer((word & 0x3ff) as f32 / 1023.0, transfer),
            ];
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

    /// Tone-maps the whole frame to an 8-bit sRGB frame, the SDR half of the
    /// pair.  Alpha is carried through unchanged (these captures are opaque).
    pub fn tone_map_to_srgb(&self, operator: ToneMap) -> Result<Frame> {
        let white = match operator {
            ToneMap::Clip => 1.0,
            ToneMap::Reinhard => self.peak().max(1.0),
        };
        let mut bytes = Vec::with_capacity(self.pixels.len() * 4);
        for pixel in &self.pixels {
            let mut rgb = [pixel[0].max(0.0), pixel[1].max(0.0), pixel[2].max(0.0)];
            if operator == ToneMap::Reinhard && white > 1.0 {
                // Extended Reinhard with `white` mapping to 1.0.  Scaling the
                // whole triple by one factor keeps the hue; only the highlights
                // roll off.
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
    /// curve.  The layer must be the same size.
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
            let keep = 1.0 - alpha;
            destination[0] = source[0] * alpha + destination[0] * keep;
            destination[1] = source[1] * alpha + destination[1] * keep;
            destination[2] = source[2] * alpha + destination[2] * keep;
            destination[3] = alpha + destination[3] * keep;
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

fn decode_transfer(value: f32, transfer: Transfer) -> f32 {
    match transfer {
        Transfer::Linear => value,
        Transfer::Srgb => srgb_eotf(value),
        // PQ and HLG are absolute: the code names a light level, so bring it
        // into the reference-white-relative scale the rest of the pipeline uses.
        Transfer::Pq => pq_eotf(value) * (PQ_PEAK_NITS / REFERENCE_WHITE_NITS),
        Transfer::Hlg => hlg_inverse_oetf(value) * (HLG_PEAK_NITS / REFERENCE_WHITE_NITS),
    }
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

/// Recognises HDR content from the facts a capture or a file provides: the
/// transfer function and primaries it declared, and whether it carried more
/// than 8 bits.  This is the metadata test a decoder can make before any pixel
/// is examined — the same one Starward uses (BT.2020 + PQ + >8 bits is HDR10).
pub const fn looks_like_hdr(transfer: Transfer, primaries: Primaries, bits_per_pixel: u32) -> bool {
    matches!(transfer, Transfer::Pq | Transfer::Hlg)
        && matches!(primaries, Primaries::Bt2020)
        && bits_per_pixel > 8
}

/// The BT.709→BT.2020 matrix, exposed for an encoder that has to write HDR10
/// back out (the reverse of what [`HdrFrame::from_samples`] applies).
pub fn scrgb_to_bt2020(rgb: [f32; 3]) -> [f32; 3] {
    multiply(BT709_TO_BT2020, rgb)
}

/// Encodes linear scRGB into a PQ code, normalised to 10 000 cd/m², which is
/// what an HDR10 output sample holds.
pub fn srgb_to_pq_code(linear: f32) -> f32 {
    pq_oetf(linear * REFERENCE_WHITE_NITS / PQ_PEAK_NITS)
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
        )
        .unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        assert!((pixel[0] - 1.0).abs() < 1e-4);
        assert!(pixel[1].abs() < 1e-4 && pixel[2].abs() < 1e-4);
    }

    #[test]
    fn a_1000_nit_pq_sample_decodes_to_scrgb_reference_white() {
        // 1000 nits is 1000 / 203 of the reference white, so scRGB 4.926.
        let code = pq_oetf(1000.0 / 10_000.0);
        let sample = (code * 65535.0).round() as u16;
        let frame = HdrFrame::from_samples(
            &[sample, sample, sample, 65535],
            Size::new(1, 1),
            Transfer::Pq,
            Primaries::Bt709,
        )
        .unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        let expected = 1000.0 / REFERENCE_WHITE_NITS;
        assert!((pixel[0] - expected).abs() < 0.01, "got {}", pixel[0]);
        assert_eq!(pixel[3], 1.0);
    }

    #[test]
    fn bt2020_white_becomes_bt709_white() {
        // The 2020->709 matrix has rows that sum to one, so a neutral stays
        // neutral; without that a white HDR pixel would come out tinted.
        let frame = HdrFrame::from_samples(
            &[65535, 65535, 65535, 65535],
            Size::new(1, 1),
            Transfer::Linear,
            Primaries::Bt2020,
        )
        .unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        assert!((pixel[0] - 1.0).abs() < 1e-4, "{}", pixel[0]);
        assert!((pixel[1] - 1.0).abs() < 1e-4, "{}", pixel[1]);
        assert!((pixel[2] - 1.0).abs() < 1e-4, "{}", pixel[2]);
    }

    #[test]
    fn detection_needs_pq_or_hlg_and_wide_primaries_and_depth() {
        assert!(looks_like_hdr(Transfer::Pq, Primaries::Bt2020, 10));
        assert!(!looks_like_hdr(Transfer::Srgb, Primaries::Bt2020, 10));
        assert!(!looks_like_hdr(Transfer::Pq, Primaries::Bt709, 10));
        assert!(!looks_like_hdr(Transfer::Pq, Primaries::Bt2020, 8));
    }

    #[test]
    fn a_sdr_white_pixel_is_not_hdr_but_a_brighter_one_is() {
        let sdr = one_pixel([1.0, 1.0, 1.0, 1.0]);
        assert!(!sdr.is_hdr());
        let hdr = one_pixel([4.0, 3.0, 2.0, 1.0]);
        assert!(hdr.is_hdr());
    }

    #[test]
    fn tone_mapping_reinhard_lands_the_peak_on_white_and_leaves_sdr_alone() {
        let frame = one_pixel([4.0, 2.0, 1.0, 1.0]);
        let sdr = frame.tone_map_to_srgb(ToneMap::Reinhard).unwrap();
        assert_eq!(sdr.pixel(Point::new(0, 0)).unwrap()[0], 255);
        // The mid 2.0 is pulled down but stays bright.
        assert!(sdr.pixel(Point::new(0, 0)).unwrap()[1] > 180);

        // A frame already inside SDR white does not get compressed.
        let flat = one_pixel([0.5, 0.5, 0.5, 1.0]);
        let sdr = flat.tone_map_to_srgb(ToneMap::Reinhard).unwrap();
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
        for (index, expected) in frame.pixels().iter().enumerate() {
            let got = decoded[index];
            let scale = expected[0].max(expected[1]).max(expected[2]).max(1e-6);
            for channel in 0..3 {
                assert!(
                    (got[channel] - expected[channel]).abs() <= scale * 0.02 + 1e-4,
                    "pixel {index} channel {channel}: {} vs {}",
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
    }

    #[test]
    fn hdr10_bytes_decode_big_endian() {
        // A neutral 0xFFFF triple is the top PQ code (10 000 nits); the matrix
        // leaves a neutral neutral, so it comes out at the reference white
        // scaled to 10 000 nits.
        let bytes = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        let frame = HdrFrame::from_hdr10_be_bytes(&bytes, Size::new(1, 1)).unwrap();
        let pixel = frame.pixel(0, 0).unwrap();
        let expected = PQ_PEAK_NITS / REFERENCE_WHITE_NITS;
        assert!((pixel[0] - expected).abs() < 0.1, "{}", pixel[0]);
        assert!((pixel[1] - expected).abs() < 0.1, "{}", pixel[1]);
        assert!((pixel[2] - expected).abs() < 0.1, "{}", pixel[2]);
        assert_eq!(pixel[3], 1.0);
        // And a black sample is black, whatever the byte order bug might do.
        let frame = HdrFrame::from_hdr10_be_bytes(&[0, 0, 0, 0, 0, 0, 0xff, 0xff], Size::new(1, 1))
            .unwrap();
        assert!(frame.pixel(0, 0).unwrap()[0].abs() < 1e-6);
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
