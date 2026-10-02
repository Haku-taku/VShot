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
use crate::model::codec::{self, HdrImage};
use crate::model::frame::Frame;

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

impl Picture {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::model::codec::HdrCodec;
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
