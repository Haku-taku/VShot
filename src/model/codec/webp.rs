// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! WebP: the SDR half as either a small lossy file or an exact one.
//!
//! WebP is the one format here that offers both, and the choice is the
//! parameter a user actually reaches for: `lossless` on for an exact copy of
//! the pixels, off for a file a fraction of the size.
//!
//! Unlike JPEG it carries alpha, so a capture with a transparent region keeps
//! it -- nothing is composited away, and a WebP of a region capture is still
//! see-through where the region was.
//!
//! The encoder is ffmpeg's, through [`super::ffmpeg_still`], so whether WebP is
//! offered at all is the machine's answer rather than the build's.

use crate::error::Result;
use crate::geometry::Size;
use crate::model::codec::params::{ParamKind, ParamSpec, ParamValues};
use crate::model::codec::scale::{self, ScaleMetadata, ScalePlace};
use crate::model::codec::SdrCodec;

/// The WebP codec.
pub struct Webp;

/// What `quality` is when nothing says otherwise: ffmpeg's own default.
pub const DEFAULT_QUALITY: i64 = 75;

/// Whether a capture is written exactly when nothing says otherwise.
///
/// Off: the reason to pick WebP over PNG is usually the size, and a lossless
/// WebP is a different kind of file -- smaller than a PNG for a photograph,
/// but not for the flat colour and hard edges a screenshot is made of.
pub const DEFAULT_LOSSLESS: &str = "lossy";

/// The two modes `lossless` offers, in the order the settings window lists
/// them: the default first.
pub const MODE_NAMES: &[&str] = &["lossy", "lossless"];

const SPECS: &[ParamSpec] = &[
    ParamSpec {
        name: "lossless",
        label: "Mode",
        hint: "Lossless keeps every pixel and writes a bigger file; lossy is smaller \
               and throws some away. Read by lossy too, where it sets how much",
        kind: ParamKind::Choice {
            values: MODE_NAMES,
            default: DEFAULT_LOSSLESS,
        },
    },
    ParamSpec {
        name: "quality",
        label: "Quality",
        hint: "Higher keeps more of the picture and writes a bigger file. Ignored \
               when the mode is lossless, which always keeps everything",
        kind: ParamKind::Integer {
            min: 0,
            max: 100,
            step: 1,
            default: DEFAULT_QUALITY,
        },
    },
];

impl SdrCodec for Webp {
    fn extension(&self) -> &'static str {
        "webp"
    }

    fn name(&self) -> &'static str {
        "webp"
    }

    fn mime(&self) -> &'static str {
        "image/webp"
    }

    fn specs(&self) -> &'static [ParamSpec] {
        SPECS
    }

    fn encode(
        &self,
        frame: &crate::model::Frame,
        density: Option<u32>,
        values: &ParamValues,
    ) -> Result<Vec<u8>> {
        let bytes = super::ffmpeg_still::encode_sdr(
            super::ffmpeg_still::WEBP,
            frame,
            values.integer("quality", DEFAULT_QUALITY),
            values.text("lossless", DEFAULT_LOSSLESS) == "lossless",
        )?;
        match density {
            Some(density) => self.with_scale(bytes, density, frame.size()),
            None => Ok(bytes),
        }
    }
}

impl ScaleMetadata for Webp {
    /// An `EXIF` chunk, holding `XResolution`/`YResolution`/`ResolutionUnit`:
    /// the same three numbers JFIF states its density in, and the field every
    /// reader shows as DPI.
    fn scale_place(&self) -> ScalePlace {
        ScalePlace::Standard
    }

    fn with_scale(&self, bytes: Vec<u8>, density: u32, size: Size) -> Result<Vec<u8>> {
        if bytes.len() < 16 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
            return Ok(bytes);
        }
        let mut out = bytes;
        let mut added = 0usize;
        // A simple-format file holds exactly one chunk and nothing else, so a
        // second one is only legal once the extended header is there; that
        // header is also the only place the canvas is stated.
        if &out[12..16] != b"VP8X" {
            let mut header = Vec::with_capacity(10);
            // The EXIF flag, and nothing else: a file that already carried
            // alpha or a profile would already have the extended header, so
            // reaching here means it has neither.
            header.push(kExifFlag);
            header.extend_from_slice(&[0, 0, 0]);
            // The canvas is stored one less than it is, in three bytes.
            let width = (size.width.saturating_sub(1) & 0x00ff_ffff).to_le_bytes();
            let height = (size.height.saturating_sub(1) & 0x00ff_ffff).to_le_bytes();
            header.extend_from_slice(&width[..3]);
            header.extend_from_slice(&height[..3]);
            let chunk = riff_chunk(b"VP8X", &header);
            added += chunk.len();
            let rest = out.split_off(12);
            out.extend_from_slice(&chunk);
            out.extend_from_slice(&rest);
        }
        // A file that already had the extended header -- one with alpha in it
        // -- has to be told the metadata is there too, or a reader that trusts
        // the flags skips the chunk.
        out[20] |= kExifFlag;
        let chunk = riff_chunk(b"EXIF", &scale::exif_tiff(density));
        added += chunk.len();
        out.extend_from_slice(&chunk);
        // The RIFF size counts everything after its own eight bytes.
        let declared = u32::from_le_bytes(out[4..8].try_into().unwrap_or([0; 4]));
        out[4..8].copy_from_slice(&declared.saturating_add(added as u32).to_le_bytes());
        Ok(out)
    }

    fn declared_scale(&self, bytes: &[u8]) -> Option<u32> {
        if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
            return None;
        }
        let mut at = 12;
        while at + 8 <= bytes.len() {
            let kind = bytes.get(at..at + 4)?;
            let length = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
            let body = bytes.get(at + 8..at + 8 + length)?;
            if kind == b"EXIF" {
                return scale::density_from_exif_payload(body);
            }
            // Every chunk is padded to an even length.
            at = at.checked_add(8 + length + (length & 1))?;
        }
        None
    }
}

/// The `EXIF` bit of the extended header's flag byte: the third bit down, as
/// the container specifies and as `webp/mux_types.h` names it.
const kExifFlag: u8 = 0x08;

/// One RIFF chunk: its tag, its length and its payload, padded to an even
/// length the way the container requires.
fn riff_chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 9);
    out.extend_from_slice(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::model::codec::ParamValue;
    use crate::model::Frame;

    /// A frame with something in it to compress: a flat colour would come out
    /// the same size at every quality, and the parameter would look broken.
    fn noisy() -> Frame {
        let pixels: Vec<u8> = (0..64 * 64 * 4)
            .map(|index| (index * 37 % 251) as u8)
            .collect();
        Frame::new(Size::new(64, 64), pixels).unwrap()
    }

    fn write(frame: &Frame, mode: &str, quality: i64) -> Vec<u8> {
        let mut values = ParamValues::defaults(SPECS);
        values.set(SPECS, "lossless", ParamValue::Text(mode.into()));
        values.set(SPECS, "quality", ParamValue::Integer(quality));
        Webp.encode(frame, None, &values).unwrap()
    }

    /// A WebP written here is one another reader can open.
    #[test]
    fn a_webp_written_here_is_a_webp() {
        if !super::super::ffmpeg_still::webp_available() {
            return;
        }
        let bytes = write(&noisy(), "lossy", 75);
        assert_eq!(&bytes[..4], b"RIFF", "no RIFF header");
        assert_eq!(&bytes[8..12], b"WEBP", "no WEBP fourcc");
    }

    /// Lossless keeps the pixels exactly, which is the whole promise of the
    /// mode: the same frame written twice comes back byte for byte.  Sizes are
    /// deliberately not compared here -- how lossless and lossy rank depends on
    /// the picture, and a synthetic pattern ranks them the opposite way to a
    /// screenshot -- so the quality parameter is weighed against itself below.
    #[test]
    fn lossless_is_deterministic() {
        if !super::super::ffmpeg_still::webp_available() {
            return;
        }
        let frame = noisy();
        assert_eq!(
            write(&frame, "lossless", 100),
            write(&frame, "lossless", 100),
            "lossless wrote two different files"
        );
    }

    /// The quality parameter reaches the encoder: at the same mode, a lower
    /// quality writes a smaller file.
    #[test]
    fn the_quality_reaches_the_encoder() {
        if !super::super::ffmpeg_still::webp_available() {
            return;
        }
        let frame = noisy();
        let low = write(&frame, "lossy", 10).len();
        let high = write(&frame, "lossy", 90).len();
        assert!(
            low < high,
            "quality 10 wrote {low} bytes against 90's {high}"
        );
    }

    /// WebP has nowhere standard to put a scale, so the value travels in a
    /// marker of this program's own -- and the file it travels in is still a
    /// WebP, which is the part that has to hold.
    #[test]
    fn the_scale_survives_the_round_trip() {
        if !super::super::ffmpeg_still::webp_available() {
            return;
        }
        let frame = noisy();
        let bytes = Webp
            .encode(&frame, Some(3), &ParamValues::defaults(SPECS))
            .unwrap();
        assert_eq!(Webp.declared_scale(&bytes), Some(3));
        assert_eq!(Webp.scale_place(), ScalePlace::Standard);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
        // The extended header has to be there for a second chunk to be legal.
        assert_eq!(&bytes[12..16], b"VP8X");
        // And it has to say the metadata is there, or a reader that trusts the
        // flags looks right past the chunk.
        assert_eq!(bytes[20] & kExifFlag, kExifFlag, "the EXIF flag is set");
        // The chunk is named `EXIF`, and what it holds is a bare TIFF blob:
        // no `Exif\0\0`, which is the spelling libwebp itself writes.
        assert!(bytes.windows(4).any(|window| window == b"EXIF"));
        assert!(bytes
            .windows(4)
            .any(|window| window == [0x4d, 0x4d, 0x00, 0x2a]));
        // And the RIFF size still counts the file it is in.
        let declared = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(declared + 8, bytes.len(), "the RIFF size does not match");
        // A file written without a scale declares none.  What libwebp does
        // with the layout is its own business -- a frame carrying alpha gets
        // the extended header whether or not this program added anything.
        let plain = Webp
            .encode(&frame, None, &ParamValues::defaults(SPECS))
            .unwrap();
        assert_eq!(Webp.declared_scale(&plain), None);
    }
}
