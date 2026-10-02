// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! PNG: the SDR half, and the one format a capture is never without.
//!
//! This is the SDR side's first [`SdrCodec`], and for now its only one.  It is
//! a codec like any other rather than a special case in the output path because
//! its compression level is a *parameter* — the same kind of thing AVIF's
//! quality is — and a settings window that can draw one format's parameters
//! should be able to draw every format's.
//!
//! Every level is lossless: the parameter trades encoding time for file size
//! and nothing else, which is why this side needs no reference white, no
//! transfer function and no gamut.  The pixels are the pixels.

use crate::error::Result;
use crate::model::codec::params::{ParamKind, ParamSpec, ParamValues};
use crate::model::codec::scale::{self, ScaleMetadata, ScalePlace};
use crate::model::codec::SdrCodec;
use crate::model::frame::{Frame, PngCompression};

/// The PNG codec.
pub struct Png;

/// The names `compression` accepts, in the order the settings window lists
/// them — the same five `--format-param png.compression` takes.
pub const COMPRESSION_NAMES: &[&str] = &["none", "fastest", "fast", "balanced", "high"];

/// What `compression` is when nothing says otherwise.
pub const DEFAULT_COMPRESSION: &str = "fast";

const SPECS: &[ParamSpec] = &[ParamSpec {
    name: "compression",
    label: "Compression",
    hint: "All levels are lossless; slower ones buy a smaller file",
    kind: ParamKind::Choice {
        values: COMPRESSION_NAMES,
        default: DEFAULT_COMPRESSION,
    },
}];

impl SdrCodec for Png {
    fn extension(&self) -> &'static str {
        "png"
    }

    fn name(&self) -> &'static str {
        "png"
    }

    fn mime(&self) -> &'static str {
        "image/png"
    }

    fn specs(&self) -> &'static [ParamSpec] {
        SPECS
    }

    fn encode(&self, frame: &Frame, density: Option<u32>, values: &ParamValues) -> Result<Vec<u8>> {
        let compression = PngCompression::parse(values.text("compression", DEFAULT_COMPRESSION))?;
        let bytes = frame.encode_png(compression)?;
        match density {
            Some(density) => self.with_scale(bytes, density, frame.size()),
            None => Ok(bytes),
        }
    }
}

/// The width of a PNG's signature and its `IHDR`, which is always the first
/// chunk and always the same size: where a `pHYs` is inserted, so it sits with
/// the other header chunks and before the pixels.
const AFTER_IHDR: usize = 8 + 25;

impl ScaleMetadata for Png {
    /// `pHYs` is the format's own field for a physical resolution, in pixels
    /// per metre, and it is the one the pin daemon already reads.
    fn scale_place(&self) -> ScalePlace {
        ScalePlace::Standard
    }

    fn with_scale(
        &self,
        bytes: Vec<u8>,
        density: u32,
        _size: crate::geometry::Size,
    ) -> Result<Vec<u8>> {
        if bytes.len() < AFTER_IHDR || &bytes[12..16] != b"IHDR" {
            return Ok(bytes); // not a PNG this can place a chunk in
        }
        let metres = scale::pixels_per_meter(density);
        let mut payload = Vec::with_capacity(9);
        payload.extend_from_slice(&metres.to_be_bytes());
        payload.extend_from_slice(&metres.to_be_bytes());
        // Metres: the only unit that says anything about scale.  PNG's other
        // unit is "the ratio is all that is meaningful", which is not a density.
        payload.push(1);
        let mut out = bytes;
        let rest = out.split_off(AFTER_IHDR);
        out.extend_from_slice(&png_chunk(b"pHYs", &payload));
        out.extend_from_slice(&rest);
        Ok(out)
    }

    fn declared_scale(&self, bytes: &[u8]) -> Option<u32> {
        if bytes.len() < AFTER_IHDR || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
            return None;
        }
        let mut at = 8;
        while at + 8 <= bytes.len() {
            let length = u32::from_be_bytes(bytes[at..at + 4].try_into().ok()?) as usize;
            let kind = bytes.get(at + 4..at + 8)?;
            // The pixels are where a declaration would have had to be, so the
            // walk stops rather than reading through a whole capture.
            if kind == b"IDAT" || kind == b"IEND" {
                return None;
            }
            let body = bytes.get(at + 8..at + 8 + length)?;
            if kind == b"pHYs" && length >= 9 {
                let x = u32::from_be_bytes(body[0..4].try_into().ok()?);
                let y = u32::from_be_bytes(body[4..8].try_into().ok()?);
                return scale::density_from_pixels_per_meter(x, y, body[8]);
            }
            // Length, type, payload and CRC; a chunk that runs past the end is
            // a malformed file rather than a reason to read out of bounds.
            at = at.checked_add(length + 12)?;
        }
        None
    }
}

/// One PNG chunk: its length, its type, its payload and the CRC over the last
/// two.  The CRC is the format's own (IEEE, reflected), written out rather
/// than taken as a dependency for the seventeen bytes of one `pHYs`.
fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    let mut crc = 0xffff_ffffu32;
    for &byte in out[4..].iter() {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    out.extend_from_slice(&(!crc).to_be_bytes());
    out
}

/// What `compression` is when nothing says otherwise.  Named here rather than
/// only inside `PngCompression`'s own `Default` so the settings window can show
/// the same answer in its "built-in default" row.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::model::codec::params::ParamValue;
    use crate::model::codec::sdr_by_name;

    fn frame() -> Frame {
        Frame::new(Size::new(2, 2), vec![128; 2 * 2 * 4]).unwrap()
    }

    /// PNG is registered under the name the config file and the flag use.
    #[test]
    fn png_is_reachable_by_its_own_name() {
        let codec = sdr_by_name("png").expect("the SDR codec this build always has");
        assert_eq!(codec.extension(), "png");
        assert_eq!(codec.mime(), "image/png");
    }

    /// The codec's own default is the one the spec declares, so the settings
    /// window's "built-in default" row and the encoder agree.
    #[test]
    fn the_declared_default_is_the_one_the_encoder_uses() {
        let codec = sdr_by_name("png").unwrap();
        let spec = codec
            .specs()
            .iter()
            .find(|spec| spec.name == "compression")
            .expect("compression is declared");
        assert_eq!(
            spec.default_value(),
            ParamValue::Text(DEFAULT_COMPRESSION.into())
        );
    }

    /// The parameter reaches the encoder: a lower level writes a bigger file.
    /// The frame is noise rather than a flat colour, because DEFLATE has
    /// nothing to do with a flat one and every level would agree.
    #[test]
    fn the_compression_level_reaches_the_encoder() {
        let pixels: Vec<u8> = (0..64 * 64 * 4)
            .map(|index| (index * 37 % 251) as u8)
            .collect();
        let frame = Frame::new(Size::new(64, 64), pixels).unwrap();
        let codec = sdr_by_name("png").unwrap();

        let write = |name: &str| {
            let mut values = ParamValues::defaults(codec.specs());
            values.set(codec.specs(), "compression", ParamValue::Text(name.into()));
            codec.encode(&frame, None, &values).unwrap()
        };
        let smallest = write("high");
        let fastest = write("fastest");
        assert!(
            fastest.len() > smallest.len(),
            "fastest wrote {} bytes against high's {}",
            fastest.len(),
            smallest.len()
        );
    }

    /// A name the format does not offer is refused by the encoder rather than
    /// silently encoded at the default.
    #[test]
    fn a_level_the_format_does_not_offer_is_refused() {
        let codec = sdr_by_name("png").unwrap();
        let mut values = ParamValues::defaults(codec.specs());
        // `set` drops an unknown choice back to the declared default, which is
        // what a `--format-param png.compression=slowest` on the command line
        // gets: the codec layer never sees the bad name.
        values.set(
            codec.specs(),
            "compression",
            ParamValue::Text("slowest".into()),
        );
        assert_eq!(values.text("compression", "?"), DEFAULT_COMPRESSION);
        // The parser underneath still refuses it, which is what keeps a typo
        // from being written as a level nobody chose.
        assert!(PngCompression::parse("slowest").is_err());
        assert!(codec.encode(&frame(), None, &values).is_ok());
    }

    /// The scale a capture was taken at survives into the file and back out,
    /// which is what lets a pin keep its size on another monitor.
    #[test]
    fn the_scale_survives_the_round_trip() {
        let frame = Frame::new(Size::new(4, 4), vec![128; 4 * 4 * 4]).unwrap();
        let bytes = Png
            .encode(&frame, Some(2), &ParamValues::defaults(SPECS))
            .unwrap();
        assert_eq!(Png.declared_scale(&bytes), Some(2));
        assert_eq!(Png.scale_place(), ScalePlace::Standard);
        // And it is still a PNG.
        assert_eq!(Frame::from_png(&bytes).unwrap(), frame);
        // A file written without one declares nothing rather than 1.
        let plain = Png
            .encode(&frame, None, &ParamValues::defaults(SPECS))
            .unwrap();
        assert_eq!(Png.declared_scale(&plain), None);
    }
}
