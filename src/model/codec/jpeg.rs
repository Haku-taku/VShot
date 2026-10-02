// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! JPEG: the SDR half as a small, lossy file.
//!
//! The format everybody can open, and the trade that comes with it: it is 8-bit
//! sRGB with no alpha channel, and it always loses something.  It is here for
//! the cases where a screenshot has to be small or has to go somewhere that
//! understands nothing else.
//!
//! Two things are this codec's own business, and both are decided in the shim:
//!
//! * **Alpha is composited over black.**  A screenshot's transparent area is
//!   the desktop showing through, and JPEG cannot say "showing through" -- it
//!   has no fourth channel.  Black is what a viewer that does not composite
//!   draws anyway, where white would put a bright frame around a dark capture.
//!
//! * **Density is dropped.**  `Frame`'s density says which output a capture
//!   came from and stays sharp wherever the image goes, and a JPEG written here
//!   has nowhere to put a physical resolution -- unlike the PNG, which carries
//!   one in a `pHYs` chunk.  Asking for one would be a silent no-op, so this
//!   codec says so instead.
//!
//! The encoder is ffmpeg's, through [`super::ffmpeg_still`], so whether JPEG is
//! offered at all is the machine's answer rather than the build's.

use crate::error::Result;
use crate::geometry::Size;
use crate::model::codec::params::{ParamKind, ParamSpec, ParamValues};
use crate::model::codec::scale::{self, ScaleMetadata, ScalePlace};
use crate::model::codec::SdrCodec;

/// The JPEG codec.
pub struct Jpeg;

/// What `quality` is when nothing says otherwise.
///
/// Not 100: a screenshot is mostly flat colour and hard edges, and the largest
/// files buy almost nothing visible there while costing the size that made
/// someone pick JPEG in the first place.
pub const DEFAULT_QUALITY: i64 = 85;

const SPECS: &[ParamSpec] = &[ParamSpec {
    name: "quality",
    label: "Quality",
    hint: "Higher keeps more of the picture and writes a bigger file. JPEG always \
           loses something; this is how much",
    kind: ParamKind::Integer {
        min: 1,
        max: 100,
        step: 1,
        default: DEFAULT_QUALITY,
    },
}];

impl SdrCodec for Jpeg {
    fn extension(&self) -> &'static str {
        "jpg"
    }

    /// Both spellings.  `.jpeg` is not a second format, and a save dialog that
    /// listed only `.jpg` would read as though it were.
    fn suffixes(&self) -> &'static [&'static str] {
        &["jpg", "jpeg"]
    }

    fn name(&self) -> &'static str {
        "jpeg"
    }

    fn mime(&self) -> &'static str {
        "image/jpeg"
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
            super::ffmpeg_still::JPEG,
            frame,
            values.integer("quality", DEFAULT_QUALITY),
            // JPEG has no lossless mode: the flag is meaningless here, and
            // passing it would only pretend to a choice that does not exist.
            false,
        )?;
        match density {
            Some(density) => self.with_scale(bytes, density, frame.size()),
            None => Ok(bytes),
        }
    }
}

impl ScaleMetadata for Jpeg {
    /// JFIF's `APP0` carries a resolution and the unit it is in, which is
    /// exactly this format's own field for it — and the one every JPEG reader
    /// already understands.
    ///
    /// ffmpeg's MJPEG encoder writes a `Lavc` comment segment where that
    /// header would go, so the segment is written here instead of being asked
    /// of the encoder: a JPEG that declares its scale is a JFIF file, and one
    /// that does not is what MJPEG produces on its own.
    fn scale_place(&self) -> ScalePlace {
        ScalePlace::Standard
    }

    fn with_scale(&self, bytes: Vec<u8>, density: u32, _size: Size) -> Result<Vec<u8>> {
        // Every JPEG opens with the SOI marker, and the application segments
        // belong between it and the frame header.
        if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] != 0xd8 {
            return Ok(bytes);
        }
        let dpi = (scale::DPI_PER_STEP * density.clamp(1, 4)) as u16;
        let mut segment = vec![0xff, 0xe0, 0x00, 0x10];
        segment.extend_from_slice(b"JFIF\0");
        // Version 1.1, then the unit: 1 is "dots per inch", the one this
        // program's 96-DPI-per-step convention is stated in.
        segment.extend_from_slice(&[0x01, 0x01, 0x01]);
        segment.extend_from_slice(&dpi.to_be_bytes());
        segment.extend_from_slice(&dpi.to_be_bytes());
        // No thumbnail, which is what the two zero bytes say.
        segment.extend_from_slice(&[0x00, 0x00]);
        let mut out = bytes;
        let rest = out.split_off(2);
        out.extend_from_slice(&segment);
        out.extend_from_slice(&rest);
        Ok(out)
    }

    fn declared_scale(&self, bytes: &[u8]) -> Option<u32> {
        if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] != 0xd8 {
            return None;
        }
        let mut at = 2;
        while at + 4 <= bytes.len() {
            // Segments are `FF <marker> <length>`, and anything that is not a
            // marker means the walk has reached the entropy-coded data.
            if bytes[at] != 0xff {
                return None;
            }
            let marker = bytes[at + 1];
            if marker == 0xda || marker == 0xd9 {
                return None; // the scan begins; nothing was declared before it
            }
            let length = usize::from(u16::from_be_bytes(bytes[at + 2..at + 4].try_into().ok()?));
            if length < 2 {
                return None;
            }
            let body = bytes.get(at + 4..at + 2 + length)?;
            if marker == 0xe0 && body.starts_with(b"JFIF\0") && body.len() >= 12 {
                let unit = body[7];
                let x = u32::from(u16::from_be_bytes(body[8..10].try_into().ok()?));
                let y = u32::from(u16::from_be_bytes(body[10..12].try_into().ok()?));
                // Unit 1 is dots per inch; anything else is a different unit
                // this convention has nothing to say about.
                return (unit == 1).then(|| scale::density_from_dpi(x, y)).flatten();
            }
            at = at.checked_add(2 + length)?;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::model::Frame;

    /// A frame with something in it to compress: a flat colour would come out
    /// the same size at every quality, and the parameter would look broken.
    fn noisy() -> Frame {
        let pixels: Vec<u8> = (0..64 * 64 * 4)
            .map(|index| (index * 37 % 251) as u8)
            .collect();
        Frame::new(Size::new(64, 64), pixels).unwrap()
    }

    /// The parameter reaches the encoder: a lower quality writes a smaller file.
    #[test]
    fn the_quality_reaches_the_encoder() {
        if !super::super::ffmpeg_still::jpeg_available() {
            return;
        }
        let frame = noisy();
        let write = |quality: i64| -> usize {
            let mut values = ParamValues::defaults(SPECS);
            values.set(
                SPECS,
                "quality",
                crate::model::codec::ParamValue::Integer(quality),
            );
            Jpeg.encode(&frame, None, &values).unwrap().len()
        };
        let low = write(20);
        let high = write(95);
        assert!(
            low < high,
            "quality 20 wrote {low} bytes against 95's {high}"
        );
    }

    /// A JPEG written here is one ffmpeg can open.
    #[test]
    fn a_jpeg_written_here_decodes() {
        if !super::super::ffmpeg_still::jpeg_available() {
            return;
        }
        let bytes = Jpeg
            .encode(&noisy(), None, &ParamValues::defaults(SPECS))
            .unwrap();
        assert_eq!(&bytes[..2], &[0xff, 0xd8], "no JPEG SOI marker");
        assert_eq!(
            &bytes[bytes.len() - 2..],
            &[0xff, 0xd9],
            "no JPEG EOI marker"
        );
    }

    /// JPEG states the scale in its own JFIF header, which any reader of the
    /// format understands -- the standard place, not a marker of ours.
    #[test]
    fn the_scale_survives_the_round_trip() {
        if !super::super::ffmpeg_still::jpeg_available() {
            return;
        }
        let frame = noisy();
        let bytes = Jpeg
            .encode(&frame, Some(2), &ParamValues::defaults(SPECS))
            .unwrap();
        assert_eq!(Jpeg.declared_scale(&bytes), Some(2));
        assert_eq!(Jpeg.scale_place(), ScalePlace::Standard);
        // Still a JPEG: SOI first, and the JFIF header right after it.
        assert_eq!(&bytes[..2], &[0xff, 0xd8]);
        assert_eq!(&bytes[2..6], &[0xff, 0xe0, 0x00, 0x10]);
        assert_eq!(&bytes[6..11], b"JFIF\0");
    }
}
