// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! JPEG XL: the HDR half written exactly.
//!
//! Of the HDR formats this program offers, JPEG XL is the one that can hold
//! what a capture actually is: linear light, whatever gamut the output
//! described, and -- with `distance` at zero -- every sample unchanged.  AVIF
//! states its colour and is read everywhere but is lossy; Radiance is exact but
//! has no colorimetry at all.  This is both.
//!
//! The pixels go in as they are: linear, in the frame's own primaries, with
//! `1.0` meaning the output's SDR white.  Two facts make that worth doing, and
//! both were measured against the system ffmpeg rather than assumed:
//!
//! * **The colour has to be declared on the frame.**  Left off, libjxl warns
//!   that it is *assuming* BT.709 and sRGB and says the colours may be wrong;
//!   a file written that way came back tagged bt709 whatever the frame held.
//!   With the primaries, transfer and matrix set, BT.2020 and Display P3 both
//!   came back as themselves.
//!
//! * **The reference white has to travel separately.**  A JPEG XL frame says
//!   its transfer and primaries but not what `1.0` stands for in cd/m², which
//!   is the one thing that turns the numbers back into light.  It is written in
//!   a box of this program's own, appended past the container -- a place libjxl
//!   ignores without complaint, measured by decoding a file carrying one.  A
//!   file with no such box is read at `--hdr-reference-white`, the same way a
//!   Radiance file from another writer is.
//!
//! A gamut the compositor described only by its chromaticities has no CICP
//! name, so it travels in a second box and is rebuilt from the numbers on the
//! way back; a foreign file naming a gamut this side cannot read is *refused*
//! rather than read as BT.709, because a wrong gamut is a wrong picture.

use std::path::Path;

use crate::error::Result;
use crate::geometry::Size;
use crate::model::codec::params::{ParamKind, ParamSpec, ParamValues};
use crate::model::codec::scale::{self, ScaleMetadata, ScalePlace};
use crate::model::codec::{HdrCodec, HdrImage};

/// The JPEG XL codec.
pub struct Jxl;

impl ScaleMetadata for Jxl {
    /// An `Exif` box, holding `XResolution`/`YResolution`/`ResolutionUnit`.
    ///
    /// The image header states the transfer function, the primaries and the
    /// intensity the picture is meant for, and none of those is a pixel size;
    /// the box is where the format keeps everything else it has to say.
    fn scale_place(&self) -> ScalePlace {
        ScalePlace::Standard
    }

    fn with_scale(&self, mut bytes: Vec<u8>, density: u32, _size: Size) -> Result<Vec<u8>> {
        // A box is only meaningful inside a container, and a bare codestream
        // has none: appending one past the image would put bytes where a reader
        // expects the end of the file.  Every file this program writes goes
        // through libjxl, which containerises.
        if bytes.len() < 12 || &bytes[..12] != kJxlSignature {
            return Ok(bytes);
        }
        // The box's payload is the four-byte offset of the TIFF header from the
        // start of the payload, then the EXIF as a JPEG APP1 segment would carry
        // it.  Measured against libjxl's own writer rather than read off the
        // standard: a picture ImageMagick hands to `libjxl` comes back with
        // `00 00 00 06 45 78 69 66 00 00` in front of the TIFF header, and that
        // is the layout a reader here has to match.
        let exif = scale::exif_tiff(density);
        let payload = (6u32).to_be_bytes().len() + b"Exif\0\0".len() + exif.len();
        bytes.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
        bytes.extend_from_slice(kExifBox);
        bytes.extend_from_slice(&6u32.to_be_bytes());
        bytes.extend_from_slice(b"Exif\0\0");
        bytes.extend_from_slice(&exif);
        Ok(bytes)
    }

    fn declared_scale(&self, bytes: &[u8]) -> Option<u32> {
        if bytes.len() < 12 || &bytes[..12] != kJxlSignature {
            return None;
        }
        let mut at = 12;
        while at + 8 <= bytes.len() {
            let length = u32::from_be_bytes(bytes[at..at + 4].try_into().ok()?) as usize;
            if length < 8 || at + length > bytes.len() {
                return None;
            }
            if bytes.get(at + 4..at + 8)? == kExifBox.as_slice() {
                let payload = bytes.get(at + 8..at + length)?;
                // The offset counts from the end of its own four bytes, which
                // is where the EXIF data would begin in a JPEG's APP1 segment:
                // six for the `Exif\0\0` identifier, which is what libjxl
                // writes and what a reader here has to match.
                let start = u32::from_be_bytes(payload.get(..4)?.try_into().ok()?) as usize;
                return scale::density_from_exif_payload(payload.get(4 + start..)?);
            }
            at += length;
        }
        None
    }
}

/// The twelve bytes every JPEG XL container opens with.
const kJxlSignature: &[u8; 12] = b"\0\0\0\x0cJXL \r\n\x87\n";
/// The box this program records the scale in.
const kExifBox: &[u8; 4] = b"Exif";

/// What `distance` is when nothing says otherwise: zero, which is lossless.
///
/// A distance of zero kept all 1024 samples of a 16x16 linear float frame
/// bit-exact, where three wrote a file a quarter the size.  An HDR half is the
/// archival copy of a capture, so exact is the default and smaller is a choice.
pub const DEFAULT_DISTANCE: f64 = 0.0;

/// What `effort` is when nothing says otherwise: ffmpeg's own default.
pub const DEFAULT_EFFORT: i64 = 7;

const SPECS: &[ParamSpec] = &[
    ParamSpec {
        name: "distance",
        label: "Distance",
        hint: "How far the encoder may drift from the original. Zero keeps every \
               sample exactly and is the default; above zero the file is smaller, \
               and the picture is not what was captured",
        kind: ParamKind::Number {
            min: 0.0,
            max: 15.0,
            step: 0.5,
            decimals: 1,
            default: DEFAULT_DISTANCE,
        },
    },
    ParamSpec {
        name: "effort",
        label: "Effort",
        hint: "How hard the encoder works, 1 (fast) to 9 (slow). Time against size; \
               it does not change what the picture holds",
        kind: ParamKind::Integer {
            min: 1,
            max: 9,
            step: 1,
            default: DEFAULT_EFFORT,
        },
    },
];

impl HdrCodec for Jxl {
    fn extension(&self) -> &'static str {
        "jxl"
    }

    fn name(&self) -> &'static str {
        "jxl"
    }

    fn specs(&self) -> &'static [ParamSpec] {
        SPECS
    }

    fn encode(&self, image: &HdrImage) -> Result<Vec<u8>> {
        super::ffmpeg_still::encode_jxl(
            &image.frame,
            image.white(),
            DEFAULT_DISTANCE,
            DEFAULT_EFFORT,
        )
    }

    fn encode_with(
        &self,
        image: &HdrImage,
        values: &ParamValues,
        density: Option<u32>,
    ) -> Result<Vec<u8>> {
        let bytes = super::ffmpeg_still::encode_jxl(
            &image.frame,
            image.white(),
            values.number("distance", DEFAULT_DISTANCE),
            values.integer("effort", DEFAULT_EFFORT),
        )?;
        match density {
            Some(density) => self.with_scale(bytes, density, image.frame.size()),
            None => Ok(bytes),
        }
    }

    fn decode(&self, bytes: &[u8], fallback_nits: f32) -> Result<HdrImage> {
        super::ffmpeg_still::decode_jxl(bytes, fallback_nits, "<memory>")
    }

    fn decode_path(&self, path: &Path, fallback_nits: f32) -> Result<HdrImage> {
        // The path form names the file in errors; the byte form has only
        // `<memory>` to offer, and a pin that failed on a file should say which.
        let bytes = std::fs::read(path).map_err(|source| crate::error::VshotError::HdrDecode {
            path: path.to_path_buf(),
            reason: format!("cannot read the image: {source}"),
        })?;
        self.decode(&bytes, fallback_nits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::model::codec::ParamValue;
    use crate::model::hdr::{HdrFrame, Primaries};

    /// A frame with light well above SDR white, which is the whole reason an
    /// HDR half exists: a byte format could not carry it at all.
    fn hdr_frame(primaries: Primaries) -> HdrFrame {
        let pixels: Vec<[f32; 4]> = (0..16 * 16)
            .map(|index| {
                let value = (index % 7) as f32;
                [4.0 + value, 2.0, 1.0, 1.0]
            })
            .collect();
        HdrFrame::in_primaries(Size::new(16, 16), pixels, primaries).unwrap()
    }

    /// The test the format exists for: linear light above SDR white survives a
    /// round trip exactly, and so does the reference white that scales it.
    #[test]
    fn a_linear_hdr_half_comes_back_exactly() {
        if !super::super::ffmpeg_still::jxl_available() {
            return;
        }
        let source = HdrImage::new(hdr_frame(Primaries::Bt2020), 203.0);
        let bytes = Jxl.encode(&source).expect("encode");
        let back = Jxl.decode(&bytes, 203.0).expect("decode");
        assert_eq!(back.frame.size(), source.frame.size());
        assert_eq!(back.frame.primaries(), Primaries::Bt2020, "gamut not kept");
        assert!((back.reference_nits - 203.0).abs() < 0.01, "white not kept");
        assert_eq!(back.frame.pixels(), source.frame.pixels(), "pixels changed");
    }

    /// Every gamut this side can name is written as itself and read back as
    /// itself.  A file that came back BT.709 whatever went in is a file whose
    /// colours are wrong.
    #[test]
    fn each_named_gamut_comes_back_as_itself() {
        if !super::super::ffmpeg_still::jxl_available() {
            return;
        }
        for primaries in [Primaries::Bt709, Primaries::DisplayP3, Primaries::Bt2020] {
            let source = HdrImage::new(hdr_frame(primaries), 203.0);
            let bytes = Jxl.encode(&source).expect("encode");
            let back = Jxl.decode(&bytes, 203.0).expect("decode");
            assert_eq!(
                back.frame.primaries(),
                primaries,
                "{primaries:?} came back wrong"
            );
        }
    }

    /// A gamut the compositor described by its chromaticities -- no name of its
    /// own -- is carried in this program's box and rebuilt on the way back.
    #[test]
    fn a_custom_gamut_is_carried_and_rebuilt() {
        if !super::super::ffmpeg_still::jxl_available() {
            return;
        }
        let custom = Primaries::from_chromaticities((0.700, 0.300), (0.200, 0.750), (0.140, 0.050));
        let source = HdrImage::new(hdr_frame(custom), 203.0);
        let bytes = Jxl.encode(&source).expect("encode");
        let back = Jxl.decode(&bytes, 203.0).expect("decode");
        assert!(
            matches!(back.frame.primaries(), Primaries::Custom { .. }),
            "not custom"
        );
    }

    /// The parameter reaches the encoder: a nonzero distance writes a smaller
    /// file, and zero keeps the samples exact.
    #[test]
    fn the_distance_reaches_the_encoder() {
        if !super::super::ffmpeg_still::jxl_available() {
            return;
        }
        let source = HdrImage::new(hdr_frame(Primaries::Bt2020), 203.0);
        let mut lossy = ParamValues::defaults(SPECS);
        lossy.set(SPECS, "distance", ParamValue::Number(3.0));
        let lossy = Jxl.encode_with(&source, &lossy, None).expect("encode");
        let exact = Jxl.encode(&source).expect("encode");
        assert!(
            lossy.len() < exact.len(),
            "distance 3 wrote {} bytes against 0's {}",
            lossy.len(),
            exact.len()
        );
    }

    /// A file with no reference white of this program's own -- anything another
    /// writer produced -- is read at the setting rather than guessed at.
    #[test]
    fn a_file_without_a_white_of_its_own_is_read_at_the_setting() {
        if !super::super::ffmpeg_still::jxl_available() {
            return;
        }
        let source = HdrImage::new(hdr_frame(Primaries::Bt2020), 203.0);
        let mut bytes = Jxl.encode(&source).expect("encode");
        // Strip the box rather than fabricating a file: the result is exactly
        // what a writer that never knew about it would have produced.
        let stripped = bytes
            .windows(4)
            .position(|window| window == b"vshw")
            .expect("no reference-white box was written");
        bytes.truncate(stripped - 4);
        let back = Jxl.decode(&bytes, 100.0).expect("decode");
        assert!(
            (back.reference_nits - 100.0).abs() < 0.01,
            "not read at the setting"
        );
    }

    /// A JPEG XL written here is one another reader can open, which is what
    /// makes the format worth offering at all.
    #[test]
    fn a_jxl_written_here_is_a_jpeg_xl() {
        if !super::super::ffmpeg_still::jxl_available() {
            return;
        }
        let bytes = Jxl
            .encode(&HdrImage::new(hdr_frame(Primaries::Bt2020), 203.0))
            .expect("encode");
        assert_eq!(
            &bytes[..12],
            b"\0\0\0\x0cJXL \r\n\x87\n",
            "no container signature"
        );
    }

    /// The scale goes in the container's own `Exif` box, and the container
    /// still decodes around it.
    #[test]
    fn the_scale_survives_the_round_trip() {
        if !super::super::ffmpeg_still::jxl_available() {
            return;
        }
        let source = HdrImage::new(hdr_frame(Primaries::Bt2020), 203.0);
        let bytes = Jxl
            .encode_with(&source, &ParamValues::defaults(SPECS), Some(3))
            .unwrap();
        assert_eq!(Jxl.declared_scale(&bytes), Some(3));
        assert_eq!(Jxl.scale_place(), ScalePlace::Standard);
        // The box, spelled the way libjxl spells it: the four-byte offset of
        // the TIFF header, then the EXIF as a JPEG APP1 segment carries it.
        let box_at = bytes
            .windows(4)
            .position(|window| window == kExifBox)
            .expect("the Exif box is in the container");
        assert_eq!(&bytes[box_at + 4..box_at + 8], b"\0\0\0\x06");
        assert_eq!(&bytes[box_at + 8..box_at + 14], b"Exif\0\0");
        assert_eq!(&bytes[box_at + 14..box_at + 18], b"MM\0\x2a");
        // The box did not cost the file its picture.
        let back = Jxl.decode(&bytes, 203.0).expect("decode");
        assert_eq!(back.frame.pixels(), source.frame.pixels());
    }
}
