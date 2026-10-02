// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! The scale a picture declares about itself, in its own file's metadata.
//!
//! A capture on a 2× output is twice the size it should appear, so the file
//! says which scale it was taken at and the pin daemon reads it back: that is
//! what keeps a pin the size it had when it is opened on a monitor of another
//! scale, and it is a property of the *file* rather than of anything kept
//! beside it, so it survives being copied, moved, or mailed.
//!
//! Every format answers the same question through [`ScaleMetadata`], and each
//! answers it in its own container's way.  Where a format has a field of its
//! own for a physical resolution — PNG's `pHYs`, JFIF's density values — the
//! value goes there, and any reader that knows the format can see it.  Where a
//! format has none, the value goes in a marker of this program's own: nothing
//! else reads it, and nothing else needs to, but the pin behaves identically
//! whichever format the file was written in.
//!
//! Four of the five formats have somewhere standard to put it, and they use
//! it: PNG its `pHYs`, JFIF its density pair, WebP an `EXIF` chunk, JPEG XL an
//! `Exif` box.  EXIF has no tag that means "display scale" — `XResolution` is
//! a print resolution by intent — but it is the one field the industry agrees
//! on for "how large is this picture", it is the field JFIF's density and PNG's
//! `pHYs` both mirror, and the convention that reads it as a scale is the one
//! those two already carry: 96 dots per inch is 1×.
//!
//! AVIF is the exception.  Its EXIF is not a box at all but a `meta` item —
//! `infe` and `iinf` entries, and an `iloc` extent to point at it — so writing
//! one means rebuilding the item table and fixing up every offset in it, for a
//! number that would travel in the same private channel as the reference white
//! and the custom gamut, which have no standard field at all.  It keeps the
//! marker of this program's own.

use crate::error::Result;
use crate::geometry::Size;

/// Device pixels per logical pixel, as a physical resolution: 96 DPI per step.
///
/// The same convention `Frame::encode_png` has always written and
/// `ui/pin_density.cpp` reads back, so a file's scale means one thing across
/// every format here.
pub const DPI_PER_STEP: u32 = 96;

/// Metres per inch, for the one conversion the two families of unit need.
const METRES_PER_INCH: f64 = 0.0254;

/// Where a format keeps the scale it declares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScalePlace {
    /// A field the format itself defines for a physical resolution.  A reader
    /// that knows the format sees the scale, whether or not it knows VShot.
    Standard,
    /// Nowhere the format defines, so the value travels in a marker of this
    /// program's own.  Only this program reads it back, and the pin behaves
    /// the same either way.
    Private,
}

/// One format's way of recording the scale its picture is shown at.
///
/// The trait is the whole of what a format has to answer about this, so a
/// format added later answers the same two questions in the same shape rather
/// than inventing a third convention.  `ui/pin_density.cpp` is the other half:
/// it reads these places back out, and grows one branch per format.
pub trait ScaleMetadata: Sync {
    /// Where this format keeps it.  Drives the tests and the documentation;
    /// the behaviour is the two methods below.
    fn scale_place(&self) -> ScalePlace;

    /// `bytes` — a file this format's encoder just produced — with `density`
    /// recorded in the place [`Self::scale_place`] names.
    ///
    /// `size` is the picture's geometry, which a container may have to restate
    /// to make room for the marker: WebP's simple layout holds one chunk and
    /// nothing else, so adding one means writing the extended header, and that
    /// header carries the canvas.
    fn with_scale(&self, bytes: Vec<u8>, density: u32, size: Size) -> Result<Vec<u8>>;

    /// The density `bytes` record, or `None` when they record none.
    ///
    /// `None` is the ordinary answer for a file another program wrote: the pin
    /// daemon then sizes it from the output it lands on, which is what every
    /// pin did before any of this existed.
    fn declared_scale(&self, bytes: &[u8]) -> Option<u32>;
}

/// `density` as pixels per metre, the unit PNG's `pHYs` is written in.
pub fn pixels_per_meter(density: u32) -> u32 {
    let dpi = f64::from(DPI_PER_STEP) * f64::from(density.clamp(1, 4));
    (dpi / METRES_PER_INCH).round() as u32
}

/// The density a pixels-per-metre pair means, or `None` when the pair is not a
/// device density.
///
/// `unit` is PNG's `pHYs` unit: 0 is "aspect ratio only" and says nothing
/// about scale.  A pair off by more than a twentieth of a step is not one of
/// ours — 300 DPI is a print resolution, not a scale this program wrote — and
/// the tolerance is what absorbs a writer that rounded to whole pixels per
/// metre.
pub fn density_from_pixels_per_meter(x: u32, y: u32, unit: u8) -> Option<u32> {
    if unit != 1 || x == 0 || x != y {
        return None;
    }
    // Pixels per metre to DPI is a multiplication: an inch is 0.0254 of a metre,
    // so a metre holds 1/0.0254 of them.
    let ratio = f64::from(x) * METRES_PER_INCH / f64::from(DPI_PER_STEP);
    let rounded = ratio.round();
    if !(1.0..=4.0).contains(&rounded) || (ratio - rounded).abs() > 0.05 {
        return None;
    }
    Some(rounded as u32)
}

/// The density a dots-per-inch pair means, or `None` when it is not one of
/// ours.  JFIF states its resolution this way rather than in pixels per metre.
pub fn density_from_dpi(x: u32, y: u32) -> Option<u32> {
    if x == 0 || x != y {
        return None;
    }
    let ratio = f64::from(x) / f64::from(DPI_PER_STEP);
    let rounded = ratio.round();
    if !(1.0..=4.0).contains(&rounded) || (ratio - rounded).abs() > 0.05 {
        return None;
    }
    Some(rounded as u32)
}

/// IFD0 tags EXIF keeps a physical resolution in, and the TIFF types of the
/// values: a rational for each resolution, a short for the unit.
const TAG_X_RESOLUTION: u16 = 0x011a;
const TAG_Y_RESOLUTION: u16 = 0x011b;
const TAG_RESOLUTION_UNIT: u16 = 0x0128;
const TYPE_SHORT: u16 = 3;
const TYPE_RATIONAL: u16 = 5;
/// `ResolutionUnit` 2 is inches, which is the unit [`DPI_PER_STEP`] counts in.
const UNIT_INCHES: u16 = 2;

/// The EXIF a file declares `density` in, as a bare TIFF blob.
///
/// One IFD with three entries and the two rationals they point at: no camera
/// make, no date, no thumbnail.  Sixty-six bytes, and the whole of what this
/// program has to say in EXIF.
///
/// The blob is *bare* — no `Exif\0\0` identifier in front of it.  That is not
/// a choice about elegance: it is what libwebp writes.  Handed a JPEG whose
/// APP1 segment carries the identifier, `cwebp -metadata all` writes the WebP
/// `EXIF` chunk with the TIFF header at its first byte, so the bare blob is the
/// form the format has converged on.  JPEG XL's `Exif` box wants the identifier
/// back, with an offset in front of it; that is the box's business, and
/// `jxl.rs` puts it there.
pub fn exif_tiff(density: u32) -> Vec<u8> {
    let dpi = DPI_PER_STEP * density.clamp(1, 4);
    // The IFD is its entry count, the entries, and the offset of the next one;
    // the two rationals follow it, because a rational is eight bytes and an
    // entry has four to hold a value in.
    const ENTRIES: usize = 3;
    const IFD_AT: usize = 8;
    const VALUES_AT: usize = IFD_AT + 2 + ENTRIES * 12 + 4;
    let mut out = Vec::with_capacity(VALUES_AT + 16);
    // Big-endian, the classic spelling and the one a JPEG's APP1 EXIF is
    // usually written in.  Every reader takes both, so this is only taste.
    out.extend_from_slice(b"MM");
    out.extend_from_slice(&42u16.to_be_bytes());
    out.extend_from_slice(&(IFD_AT as u32).to_be_bytes());
    out.extend_from_slice(&(ENTRIES as u16).to_be_bytes());
    // Ascending tag order, which is what TIFF requires of an IFD.
    for (index, tag) in [TAG_X_RESOLUTION, TAG_Y_RESOLUTION].iter().enumerate() {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&TYPE_RATIONAL.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&((VALUES_AT + index * 8) as u32).to_be_bytes());
    }
    out.extend_from_slice(&TAG_RESOLUTION_UNIT.to_be_bytes());
    out.extend_from_slice(&TYPE_SHORT.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    // A SHORT is two bytes and the value field is four, so it goes in the first
    // two and the rest is padding.
    out.extend_from_slice(&UNIT_INCHES.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    // No next IFD.
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&dpi.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&dpi.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    debug_assert_eq!(out.len(), VALUES_AT + 16);
    out
}

/// The density a bare TIFF blob declares, or `None` when it declares none of
/// ours.
///
/// The three tags are looked up by number wherever they sit in the IFD: one a
/// camera wrote is full of others, in an order this program did not choose.
pub fn density_from_exif(tiff: &[u8]) -> Option<u32> {
    let little = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let short_at = |at: usize| -> Option<u16> {
        let bytes: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let long_at = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    if short_at(2)? != 42 {
        return None;
    }
    let ifd = long_at(4)? as usize;
    let count = short_at(ifd)? as usize;
    // A bound on the walk.  An IFD with thousands of entries is a file that is
    // not answering this question, and in every file this program or a camera
    // writes the answer is in the first few.
    let mut x = None;
    let mut y = None;
    let mut unit = 0u16;
    for index in 0..count.min(64) {
        let at = ifd.checked_add(2 + index * 12)?;
        let tag = short_at(at)?;
        let kind = short_at(at + 2)?;
        if long_at(at + 4)? == 0 {
            continue;
        }
        match tag {
            TAG_RESOLUTION_UNIT if kind == TYPE_SHORT => unit = short_at(at + 8)?,
            TAG_X_RESOLUTION | TAG_Y_RESOLUTION if kind == TYPE_RATIONAL => {
                let values = long_at(at + 8)? as usize;
                let denominator = long_at(values + 4)?;
                if denominator == 0 {
                    continue;
                }
                let value = f64::from(long_at(values)?) / f64::from(denominator);
                if tag == TAG_X_RESOLUTION {
                    x = Some(value);
                } else {
                    y = Some(value);
                }
            }
            _ => {}
        }
    }
    if unit != UNIT_INCHES {
        return None;
    }
    let (x, y) = (x?, y?);
    if (x - y).abs() > 0.5 {
        return None;
    }
    density_from_dpi(x.round() as u32, y.round() as u32)
}

/// The density an EXIF payload declares, with or without the six-byte
/// `Exif\0\0` identifier in front of it.
///
/// Both spellings are in the wild — a JPEG's APP1 segment carries the
/// identifier, WebP's `EXIF` chunk does not — and a reader that had to know
/// which container it was looking at would be one more place to get that
/// wrong.
pub fn density_from_exif_payload(payload: &[u8]) -> Option<u32> {
    density_from_exif(payload).or_else(|| {
        payload
            .strip_prefix(b"Exif\0\0")
            .and_then(density_from_exif)
    })
}

/// The four bytes of a private marker: the tag its format carries it under.
///
/// One tag across the formats that need one, so the reader has a single value
/// to look for and a format added later is one more container around the same
/// four bytes.
pub const PRIVATE_TAG: [u8; 4] = *b"vshs";

/// The payload a private marker carries: the density, little-endian.
pub fn private_payload(density: u32) -> [u8; 4] {
    density.clamp(1, 4).to_le_bytes()
}

/// The density a private payload holds, or `None` when it is not one of ours.
pub fn density_from_private(payload: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = payload.get(..4)?.try_into().ok()?;
    let density = u32::from_le_bytes(bytes);
    (1..=4).contains(&density).then_some(density)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two directions agree, which is what lets a file written here be
    /// read back as the scale it was written at.
    #[test]
    fn a_density_survives_the_round_trip() {
        for density in 1..=4u32 {
            let metres = pixels_per_meter(density);
            assert_eq!(
                density_from_pixels_per_meter(metres, metres, 1),
                Some(density),
                "{density} did not come back"
            );
        }
    }

    /// Anything that is not a scale this program writes reads as "no answer",
    /// so the daemon falls through to sizing the pin from the output it lands
    /// on rather than believing a print resolution.
    #[test]
    fn a_resolution_that_is_not_a_scale_is_refused() {
        // 300 DPI: a print resolution.
        assert_eq!(density_from_pixels_per_meter(11811, 11811, 1), None);
        // Aspect-ratio only, and a non-square declaration.
        assert_eq!(density_from_pixels_per_meter(3780, 3780, 0), None);
        assert_eq!(density_from_pixels_per_meter(3780, 1890, 1), None);
        assert_eq!(density_from_pixels_per_meter(0, 0, 1), None);
    }

    /// The private marker carries the same range and refuses anything else.
    #[test]
    fn the_private_payload_is_a_density_or_nothing() {
        for density in 1..=4u32 {
            assert_eq!(
                density_from_private(&private_payload(density)),
                Some(density)
            );
        }
        assert_eq!(density_from_private(&9u32.to_le_bytes()), None);
        assert_eq!(density_from_private(&[0, 0]), None);
    }
}
