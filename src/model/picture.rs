// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! One image, read out of whatever file it came in.
//!
//! Everything the pin side does with a file starts here, and the point of a
//! single door is that the *content* decides what the picture is rather than
//! the file it arrived in.  A PNG, a JPEG XL, an AVIF and a Radiance file all
//! come back as one of two things, and the difference between them is what the
//! bytes can say about their own light:
//!
//! * an SDR file is eight-bit sRGB codes and nothing more — [`Picture::Sdr`],
//!   carried as they were read, because decoding them to light and encoding
//!   them back would be a round trip for nothing;
//! * an HDR format states the light itself, in primaries it names and against
//!   a white it names — [`Picture::Hdr`], carried as linear light.
//!
//! "Is this HDR content" is then [`Picture::carries_hdr`], a question about the
//! picture.  It is deliberately not "which format was this", because a JPEG XL
//! may hold a picture with no light above SDR white at all, and a display has
//! to be told what the content is rather than what the file was.

use std::path::Path;

use crate::error::{Result, VshotError};
use crate::geometry::Size;
use crate::model::codec::{self, HdrImage};
use crate::model::frame::Frame;
use crate::model::hdr::{pq_encode, srgb_eotf, OutputColor, Primaries, ToneMapOptions};

/// An image as the pin side holds it, whichever format it was written in.
#[derive(Clone, Debug, PartialEq)]
pub enum Picture {
    /// Eight-bit sRGB codes, from a file that says nothing about its own light.
    /// Every PNG is this, and so is anything else this program reads that
    /// carries no colorimetry of its own.
    Sdr(Frame),
    /// Linear light, in the primaries the file named and against the white its
    /// `1.0` stands for.
    Hdr(HdrImage),
}

/// What the ten-bit codes a surface is given mean.
///
/// A surface is passed through untouched only when its pixels hold what its own
/// description says they do, and the description is the *output's*: PQ on a
/// panel showing HDR, the sRGB curve on one that is not.  So which of the two
/// the codes are written in is not a property of the picture but of the output
/// it is going onto, and the renderer has to know which it has.
#[allow(dead_code)] // consumed by the daemon that replaces the Qt one
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Encoding {
    /// PQ, over a 10 000 cd/m² peak.  What a surface described in PQ carries.
    Pq,
    /// The sRGB curve, 0..1 from black to the output's white.  What a surface
    /// described in sRGB carries.
    Srgb,
}

/// One picture's pixels as one output's surface is written with them.
#[allow(dead_code)] // consumed by the daemon that replaces the Qt one
#[derive(Clone, Debug)]
pub struct SurfacePixels {
    /// Ten-bit `ARGB2101010` codes: two bits of alpha, then three channels.
    pub words: Vec<u32>,
    /// Which curve those codes are on, which is the one the surface they are
    /// going onto is described in.
    pub encoding: Encoding,
}

impl Picture {
    /// The picture's geometry, which is the same question of either half.
    pub fn size(&self) -> Size {
        match self {
            Self::Sdr(frame) => frame.size(),
            Self::Hdr(image) => image.frame.size(),
        }
    }

    /// The codes this picture is written with on an output described by
    /// `color`.
    ///
    /// This is the whole of what "SDR or HDR" decides, and it is decided by two
    /// things at once — what the picture holds and what the output it is
    /// landing on can show — so it is four cases rather than two:
    ///
    /// * an SDR picture on an SDR output is its own codes, which is what the
    ///   file held and what the panel is described in;
    /// * an SDR picture on an HDR output is the same light re-encoded against
    ///   *that output's* white and in its gamut, because an SDR code means
    ///   "this output's white" and no two outputs have the same one;
    /// * an HDR picture on an HDR output is the captured light itself, in the
    ///   output's own gamut and at the white the file named — the case the
    ///   half-float surface exists for;
    /// * an HDR picture on an SDR output is that light mapped down, the only
    ///   one of the four that has to invent anything.
    #[allow(dead_code)] // consumed by the daemon that replaces the Qt one
    pub fn words_for(&self, color: OutputColor, options: ToneMapOptions) -> Result<SurfacePixels> {
        match (self, color.is_hdr()) {
            (Self::Sdr(frame), false) => Ok(SurfacePixels {
                words: srgb_codes(frame),
                encoding: Encoding::Srgb,
            }),
            (Self::Sdr(frame), true) => Ok(SurfacePixels {
                words: srgb_frame_words(frame, color.reference_nits, color.primaries),
                encoding: Encoding::Pq,
            }),
            (Self::Hdr(image), true) => Ok(SurfacePixels {
                words: image.frame.to_rgb10_pq_in(color.primaries, image.white()),
                encoding: Encoding::Pq,
            }),
            (Self::Hdr(image), false) => Ok(SurfacePixels {
                words: srgb_codes(&image.frame.tone_map_to_srgb_with(options)?),
                encoding: Encoding::Srgb,
            }),
        }
    }

    /// The light, when this picture has any of its own.  `None` for an SDR
    /// picture, whose light is [`crate::model::hdr::srgb_eotf`] away from its
    /// codes — and which is exactly the point: what an SDR file holds is the
    /// codes, and nothing else has to be invented for it.
    pub fn as_hdr(&self) -> Option<&HdrImage> {
        match self {
            Self::Hdr(image) => Some(image),
            Self::Sdr(_) => None,
        }
    }
}

/// Reads one image file.
///
/// `fallback_nits` is the white to read a file at when it names none of its own
/// — a Radiance file from another writer, or an HDR file with no reference
/// white in it.  It is the `--hdr-reference-white` setting, and it is ignored
/// by the formats that do name one.
pub fn decode_path(path: &Path, fallback_nits: f32) -> Result<Picture> {
    let bytes = std::fs::read(path).map_err(|source| VshotError::HdrDecode {
        path: path.to_path_buf(),
        reason: format!("cannot read the image: {source}"),
    })?;
    decode_bytes(&bytes, fallback_nits).map_err(|error| match error {
        // The reader that failed did not know which file it was looking at, so
        // the path is added here, where it is known.
        VshotError::HdrDecode { reason, .. } => VshotError::HdrDecode {
            path: path.to_path_buf(),
            reason,
        },
        other => other,
    })
}

/// The same for bytes already in memory: a clipboard payload, or a capture
/// that never went through a file.
pub fn decode_bytes(bytes: &[u8], fallback_nits: f32) -> Result<Picture> {
    // Which format this is, from the bytes: a file handed to a pin may have any
    // name at all, and the formats that state their own light all name
    // themselves in their first few bytes.
    if let Some(codec) = codec::detect(bytes) {
        return Ok(Picture::Hdr(codec.decode(bytes, fallback_nits)?));
    }
    // Everything left is a PNG, which is the one SDR format with a reader and
    // the one every capture writes.
    let frame = Frame::from_png(bytes)?;
    Ok(Picture::Sdr(frame))
}

/// Eight-bit sRGB codes in the ten-bit packing a surface buffer carries: the
/// same numbers, on a finer grid than the file had.
#[allow(dead_code)] // consumed by the daemon that replaces the Qt one
pub(crate) fn srgb_codes(frame: &Frame) -> Vec<u32> {
    let ten = |value: u8| -> u32 { (u32::from(value) * 1023 + 127) / 255 };
    frame
        .pixels()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| {
            // Two bits of alpha, and a pinned picture is normally opaque: a zero
            // would make it disappear.  One that really carries alpha keeps it,
            // so a card with a soft edge is not turned into a solid rectangle.
            let alpha = u32::from(pixel[3] >> 6);
            (alpha << 30) | (ten(pixel[0]) << 20) | (ten(pixel[1]) << 10) | ten(pixel[2])
        })
        .collect()
}

/// An SDR picture's light, re-encoded for a surface described in PQ.
///
/// The decode is the sRGB EOTF and the encode is PQ against a 10 000 cd/m²
/// peak, and the white is the only thing tying the two scales together: it is
/// the light a code of `1.0` stands for on the output the picture is going
/// onto, read from that output's own description.  Codes written against
/// BT.2408's default 203 on an output whose own white is 100 would be shown
/// twice as bright as they should be.
///
/// The gamut goes with the white.  An SDR picture is BT.709, and writing its
/// values unchanged into a surface described as BT.2020 would have the panel
/// read them as a gamut they are not — every colour would come out washed out,
/// which is the same class of mistake as the wrong white and less obvious.
///
/// None of this is a tone map: it is a different encoding of the same light.
pub(crate) fn srgb_frame_words(frame: &Frame, white_nits: f32, primaries: Primaries) -> Vec<u32> {
    let white = if white_nits.is_finite() && white_nits > 0.0 {
        white_nits
    } else {
        crate::model::hdr::REFERENCE_WHITE_NITS
    };
    let to_target = primaries.from_bt709();
    let code = |linear: f32| -> u32 {
        (pq_encode(linear * white / 10_000.0) * 1023.0)
            .round()
            .clamp(0.0, 1023.0) as u32
    };
    frame
        .pixels()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| {
            let linear = [
                srgb_eotf(f32::from(pixel[0]) / 255.0),
                srgb_eotf(f32::from(pixel[1]) / 255.0),
                srgb_eotf(f32::from(pixel[2]) / 255.0),
            ];
            let converted = crate::model::hdr::multiply(to_target, linear);
            let alpha = u32::from(pixel[3] >> 6);
            (alpha << 30)
                | (code(converted[0]) << 20)
                | (code(converted[1]) << 10)
                | code(converted[2])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::model::codec::HdrCodec;
    use crate::model::hdr::Transfer;
    use crate::model::hdr::{HdrFrame, Primaries, REFERENCE_WHITE_NITS};

    /// A frame with something in it: a flat colour would not catch a channel
    /// swapped or a row order turned around.
    fn frame(width: u32, height: u32) -> Frame {
        let pixels: Vec<u8> = (0..width * height * 4)
            .map(|index| (index * 37 % 251) as u8)
            .collect();
        Frame::new(Size::new(width, height), pixels).expect("frame")
    }

    /// A PNG is read back as the codes it holds, not as light: an SDR picture
    /// has no light of its own to state, and decoding it to float and encoding
    /// it back would be a round trip that could only lose.
    #[test]
    fn a_png_reads_back_as_the_codes_it_holds() {
        let original = frame(7, 5);
        let bytes = original.to_png().expect("encode");
        let picture = decode_bytes(&bytes, REFERENCE_WHITE_NITS).expect("decode");
        let Picture::Sdr(frame) = &picture else {
            panic!("a PNG is an SDR picture, not light");
        };
        assert_eq!(frame.size(), Size::new(7, 5));
        assert_eq!(frame.pixels(), original.pixels());
    }

    /// An HDR file is read back as the light it states, with the gamut and the
    /// white it named — including light above SDR white, which is the whole
    /// reason the file was written in that format.
    #[cfg(feature = "radiance")]
    #[test]
    fn an_hdr_file_reads_back_as_light() {
        let source = HdrImage::new(
            HdrFrame::in_primaries(
                Size::new(4, 4),
                (0..16)
                    .map(|index| {
                        let value = 1.0 + index as f32 * 0.5;
                        [value, value, value, 1.0]
                    })
                    .collect(),
                Primaries::DisplayP3,
            )
            .expect("frame"),
            500.0,
        );
        let bytes = crate::model::codec::radiance::Radiance
            .encode(&source)
            .expect("encode");
        let picture = decode_bytes(&bytes, REFERENCE_WHITE_NITS).expect("decode");
        let image = picture.as_hdr().expect("an HDR format reads back as light");
        assert_eq!(image.frame.size(), Size::new(4, 4));
        assert_eq!(image.frame.primaries(), Primaries::DisplayP3);
        assert!((image.white() - 500.0).abs() < 0.5, "{}", image.white());
        // The bright corner is still bright: an SDR read would have clipped it.
        let peak = image.frame.peak();
        assert!(peak > 4.0, "{peak}");
    }

    /// A file from another writer, naming no reference white of its own, is
    /// read at the one the caller passed — which is the `--hdr-reference-white`
    /// setting, and the only thing that decides how such a file looks.
    #[cfg(feature = "radiance")]
    #[test]
    fn a_file_that_names_no_white_uses_the_fallback() {
        let header = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 1\n";
        let bytes = [header.as_slice(), &[128, 128, 128, 129]].concat();
        let picture = decode_bytes(&bytes, 250.0).expect("decode");
        let image = picture.as_hdr().expect("radiance is an HDR format");
        assert!((image.white() - 250.0).abs() < 0.01, "{}", image.white());
    }

    // --- the four cases ---------------------------------------------------

    /// An SDR picture on an SDR output is its own codes: nothing is decoded,
    /// nothing is re-encoded, and the panel's description is what the file
    /// already held.
    #[test]
    fn an_sdr_picture_on_an_sdr_output_is_its_own_codes() {
        let frame = Frame::solid(Size::new(2, 1), [10, 128, 255, 255]).expect("frame");
        let picture = Picture::Sdr(frame.clone());
        let written = picture
            .words_for(sdr_output(), ToneMapOptions::default())
            .expect("words");
        assert_eq!(written.encoding, Encoding::Srgb);
        // Ten bits from eight: the same number on a finer grid, so the byte is
        // exactly representable and nothing is lost by the trip.
        let ten = |value: u8| (u32::from(value) * 1023 + 127) / 255;
        for (index, pixel) in frame.pixels().as_chunks::<4>().0.iter().enumerate() {
            let word = written.words[index];
            assert_eq!((word >> 20) & 0x3ff, ten(pixel[0]));
            assert_eq!((word >> 10) & 0x3ff, ten(pixel[1]));
            assert_eq!(word & 0x3ff, ten(pixel[2]));
            assert_eq!(word >> 30, 3, "an opaque picture stays opaque");
        }
    }

    /// An SDR picture on an HDR output is the same *light* written against that
    /// output's own white — which is what makes it the brightness it had, not
    /// the brightness a fixed constant would have given it.
    #[test]
    fn an_sdr_picture_on_an_hdr_output_is_written_against_that_output() {
        let frame = Frame::solid(Size::new(1, 1), [128, 128, 128, 255]).expect("frame");
        let picture = Picture::Sdr(frame);
        // The light a code stands for, relative to the white it was written
        // against: the same decode the panel does.
        let light = |words: &[u32], white: f32| {
            HdrFrame::from_rgb10(
                words,
                Size::new(1, 1),
                Transfer::Pq,
                Primaries::Bt709,
                true,
                white,
            )
            .expect("decode")
            .pixel(0, 0)
            .expect("pixel")[0]
        };
        let at_203 = picture
            .words_for(
                hdr_output(Primaries::Bt709, 203.0),
                ToneMapOptions::default(),
            )
            .expect("words");
        assert_eq!(at_203.encoding, Encoding::Pq);
        // sRGB mid grey is about 0.216 of white in linear light.
        let decoded = light(&at_203.words, 203.0);
        assert!(
            (decoded - 0.2158).abs() < 0.01,
            "mid grey came out at {decoded}"
        );
        // The same byte on an output whose white is 100 cd/m² is a dimmer light,
        // so its code is lower — and reading it back against 100 gives the same
        // 0.216 of white.
        let at_100 = picture
            .words_for(
                hdr_output(Primaries::Bt709, 100.0),
                ToneMapOptions::default(),
            )
            .expect("words");
        assert!((at_100.words[0] >> 20) < (at_203.words[0] >> 20));
        let decoded_100 = light(&at_100.words, 100.0);
        assert!(
            (decoded_100 - 0.2158).abs() < 0.01,
            "at {decoded_100} of white"
        );
    }

    /// The gamut travels with the white.  An SDR picture is BT.709, and a
    /// surface described as BT.2020 would read its values as a gamut they are
    /// not — every colour washed out, which is the same class of mistake as the
    /// wrong white and much less obvious on screen.
    #[test]
    fn an_sdr_picture_is_brought_into_the_outputs_gamut() {
        let frame = Frame::solid(Size::new(1, 1), [255, 0, 0, 255]).expect("frame");
        let picture = Picture::Sdr(frame);
        let narrow = picture
            .words_for(
                hdr_output(Primaries::Bt709, 203.0),
                ToneMapOptions::default(),
            )
            .expect("words");
        let wide = picture
            .words_for(
                hdr_output(Primaries::Bt2020, 203.0),
                ToneMapOptions::default(),
            )
            .expect("words");
        // Saturated red in BT.2020 is outside BT.709, so the same light needs
        // *less* of the red channel there; writing it unchanged would have the
        // panel read BT.709 red as BT.2020 red and show a duller colour.
        assert!(
            (wide.words[0] >> 20) < (narrow.words[0] >> 20),
            "{:?} is not below {:?}",
            wide.words[0] >> 20,
            narrow.words[0] >> 20
        );
    }

    /// An HDR picture on an HDR output is the captured light itself, at the
    /// white the file named: nothing is mapped, and light above that white
    /// stays above it.
    #[cfg(feature = "radiance")]
    #[test]
    fn an_hdr_picture_on_an_hdr_output_is_the_light_itself() {
        let source = HdrImage::new(
            HdrFrame::new(
                Size::new(2, 1),
                vec![[1.0, 1.0, 1.0, 1.0], [4.0, 4.0, 4.0, 1.0]],
            )
            .expect("frame"),
            203.0,
        );
        let picture = Picture::Hdr(source);
        let written = picture
            .words_for(
                hdr_output(Primaries::Bt2020, 100.0),
                ToneMapOptions::default(),
            )
            .expect("words");
        assert_eq!(written.encoding, Encoding::Pq);
        let back = HdrFrame::from_rgb10(
            &written.words,
            Size::new(2, 1),
            Transfer::Pq,
            Primaries::Bt2020,
            true,
            203.0,
        )
        .expect("decode");
        // Four times white is four times white: the output's own SDR white has
        // nothing to do with how bright the captured light was.
        let bright = back.pixel(1, 0).expect("pixel")[0];
        assert!((bright - 4.0).abs() < 0.06, "{bright}");
    }

    /// An HDR picture on an SDR output is the one case that has to invent
    /// something: the light is mapped down rather than clipped, so the bright
    /// end keeps its ordering instead of flattening into a white rectangle.
    #[cfg(feature = "radiance")]
    #[test]
    fn an_hdr_picture_on_an_sdr_output_is_mapped_down() {
        let source = HdrImage::new(
            HdrFrame::new(
                Size::new(4, 1),
                (0..4)
                    .map(|index| {
                        let value = 1.0 + index as f32;
                        [value, value, value, 1.0]
                    })
                    .collect(),
            )
            .expect("frame"),
            203.0,
        );
        let picture = Picture::Hdr(source);
        let written = picture
            .words_for(sdr_output(), ToneMapOptions::default())
            .expect("words");
        assert_eq!(written.encoding, Encoding::Srgb);
        let codes: Vec<u32> = written
            .words
            .iter()
            .map(|word| (word >> 20) & 0x3ff)
            .collect();
        assert!(
            codes.windows(2).all(|pair| pair[0] < pair[1]),
            "the ramp came out flat, which is what clipping looks like: {codes:?}"
        );
        // The default map keeps SDR white at `ToneMapOptions::white` rather
        // than at the top, so the brightest sample has headroom above it and
        // nothing is crushed at the dim end either.
        assert!(codes[3] < 1023, "the top was clipped: {codes:?}");
        assert!(codes[0] > 512, "SDR white was crushed: {codes:?}");
    }

    /// An output showing HDR: PQ, in the gamut its description named.
    fn hdr_output(primaries: Primaries, white: f32) -> OutputColor {
        OutputColor {
            transfer: Transfer::Pq,
            primaries,
            reference_nits: white,
        }
    }

    /// An output that is not: the sRGB curve, which is what its surface is
    /// described in and what its codes mean.
    fn sdr_output() -> OutputColor {
        OutputColor {
            transfer: Transfer::Srgb,
            primaries: Primaries::Bt709,
            reference_nits: 203.0,
        }
    }

    /// Bytes that are neither an HDR format nor a PNG are refused rather than
    /// guessed at, so a pin of the wrong file says so instead of showing
    /// something that is not it.
    #[test]
    fn bytes_that_are_not_an_image_are_refused() {
        assert!(decode_bytes(b"", REFERENCE_WHITE_NITS).is_err());
        assert!(decode_bytes(b"not an image at all", REFERENCE_WHITE_NITS).is_err());
        assert!(decode_bytes(b"\x89PNG\r\n\x1a\n truncated", REFERENCE_WHITE_NITS).is_err());
    }

    /// A file that is not there is reported with its path, which is what a user
    /// who mistyped one needs to see.
    #[test]
    fn a_file_that_is_not_there_names_itself() {
        let error = decode_path(Path::new("/nonexistent/x.png"), REFERENCE_WHITE_NITS)
            .expect_err("no such file");
        assert!(error.to_string().contains("/nonexistent/x.png"), "{error}");
    }
}
