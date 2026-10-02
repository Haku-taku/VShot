// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! The Rust side of the still-image shim (`ffmpeg_still.c`).
//!
//! JPEG, WebP and JPEG XL, encoded by ffmpeg's libavcodec and reached through
//! `dlopen` like the recording encoder in `record::avcodec`.  Which of them
//! exist is a question about the *machine*, not the build: a rolling release
//! can ship a libavcodec without `libjxl`, so `vshot formats` asks at runtime
//! and offers only what answered.  That is what [`jpeg_available`] and its
//! neighbours are for, and why nothing here can fail a build.
//!
//! The shim is deliberately thin.  Everything about the encoders -- the pixel
//! format each one takes, the qscale JPEG wants instead of a quality, the
//! Butteraugli distance JPEG XL takes instead of both -- lives in the C file,
//! where the libavcodec calls are.  This side hands over pixels and settings
//! and takes back bytes.

use std::ffi::{c_char, c_float, c_int, CStr};

use crate::error::{Result, VshotError};
use crate::geometry::Size;
use crate::model::hdr::{HdrFrame, Primaries};

/// The three still-image kinds the shim knows, in its own numbering.
pub const JPEG: c_int = 1;
pub const WEBP: c_int = 2;
pub const JXL: c_int = 3;

/// A ceiling on a decoded JPEG XL, so a header naming an absurd size cannot
/// make this side allocate whatever it asks for.  Eight 4K frames' worth.
const MAX_JXL_PIXELS: usize = 32 * 1024 * 1024;

unsafe extern "C" {
    fn vshot_still_available(kind: c_int) -> c_int;
    fn vshot_still_encode(
        kind: c_int,
        width: c_int,
        height: c_int,
        rgba: *const u8,
        hdr_rgba: *const c_float,
        reference_nits: c_float,
        primaries: c_int,
        custom: c_int,
        chromaticities: *const c_float,
        quality: c_int,
        lossless: c_int,
        effort: c_int,
        distance: c_float,
        out: *mut *mut u8,
        out_len: *mut usize,
        err: *mut c_char,
        err_cap: usize,
    ) -> c_int;
    fn vshot_still_decode_jxl(
        bytes: *const u8,
        len: usize,
        fallback_nits: c_float,
        rgba_out: *mut *mut c_float,
        width: *mut c_int,
        height: *mut c_int,
        primaries: *mut c_int,
        custom: *mut c_int,
        reference_nits: *mut c_float,
        chromaticities: *mut c_float,
        err: *mut c_char,
        err_cap: usize,
    ) -> c_int;
    fn vshot_still_free(bytes: *mut std::ffi::c_void);
    fn vshot_still_load_error() -> *const c_char;
}

/// Whether the machine's ffmpeg can write and read `kind`.
///
/// The first call opens two codecs to find out, and the answer cannot change
/// inside one process, so the shim caches it and this is free thereafter.
fn available(kind: c_int) -> bool {
    unsafe { vshot_still_available(kind) != 0 }
}

pub fn jpeg_available() -> bool {
    available(JPEG)
}

pub fn webp_available() -> bool {
    available(WEBP)
}

pub fn jxl_available() -> bool {
    available(JXL)
}

/// Why none of the three is available, when none is: the message the C side
/// left, which names the library that could not be loaded rather than making
/// this side guess at which one mattered.
pub fn load_error() -> String {
    unsafe {
        let pointer = vshot_still_load_error();
        if pointer.is_null() {
            String::new()
        } else {
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        }
    }
}

/// Encodes an SDR frame.  `quality` is 1-100 for both formats; `lossless`
/// matters to WebP only, since JPEG has no lossless mode at all.
pub fn encode_sdr(
    kind: c_int,
    frame: &crate::model::Frame,
    quality: i64,
    lossless: bool,
) -> Result<Vec<u8>> {
    encode(
        kind,
        frame.size(),
        Some(frame.pixels()),
        None,
        0.0,
        0,
        false,
        None,
        quality,
        lossless,
        7,
        0.0,
    )
}

/// Encodes an HDR half as JPEG XL.
///
/// `distance` is Butteraugli's, and zero is lossless: measured on a 16x16
/// linear float frame, zero kept all 1024 samples bit-exact while three wrote a
/// file a quarter the size.  It travels as a number rather than a flag because
/// it is a continuum, not a mode.
pub fn encode_jxl(
    frame: &HdrFrame,
    reference_nits: f32,
    distance: f64,
    effort: i64,
) -> Result<Vec<u8>> {
    // The primaries are named for the C side, which has to set them on the
    // frame: libjxl *assumes* BT.709 and says the colours may be wrong when
    // they are left off, and a file tagged with a guess is worse than a file
    // that says it does not know.
    let (primaries, custom) = match frame.primaries() {
        Primaries::Bt709 => (0, false),
        Primaries::DisplayP3 => (1, false),
        Primaries::Bt2020 => (2, false),
        // A gamut the compositor described by its chromaticities has no CICP
        // name, so it travels in a box of this program's own instead.
        Primaries::Custom { .. } => (3, true),
    };
    let chromaticities = frame.primaries().chromaticities();
    let chromaticities = [
        chromaticities[0].0,
        chromaticities[0].1,
        chromaticities[1].0,
        chromaticities[1].1,
        chromaticities[2].0,
        chromaticities[2].1,
    ];
    encode(
        JXL,
        frame.size(),
        None,
        Some(frame.pixels()),
        reference_nits,
        primaries,
        custom,
        Some(&chromaticities),
        90,
        distance == 0.0,
        effort,
        distance as f32,
    )
}

#[allow(clippy::too_many_arguments)]
fn encode(
    kind: c_int,
    size: Size,
    rgba: Option<&[u8]>,
    hdr_rgba: Option<&[[f32; 4]]>,
    reference_nits: f32,
    primaries: c_int,
    custom: bool,
    chromaticities: Option<&[f32; 6]>,
    quality: i64,
    lossless: bool,
    effort: i64,
    distance: f32,
) -> Result<Vec<u8>> {
    if !available(kind) {
        let reason = load_error();
        return Err(VshotError::StillEncode(if reason.is_empty() {
            "this ffmpeg build cannot write that format".into()
        } else {
            reason
        }));
    }
    let mut bytes: *mut u8 = std::ptr::null_mut();
    let mut len = 0usize;
    let mut error = [0i8; 512];
    let width = c_int::try_from(size.width)
        .map_err(|_| VshotError::StillEncode("the image is wider than ffmpeg accepts".into()))?;
    let height = c_int::try_from(size.height)
        .map_err(|_| VshotError::StillEncode("the image is taller than ffmpeg accepts".into()))?;
    let status = unsafe {
        vshot_still_encode(
            kind,
            width,
            height,
            rgba.map_or(std::ptr::null(), <[u8]>::as_ptr),
            hdr_rgba.map_or(std::ptr::null(), |pixels| pixels.as_ptr().cast()),
            reference_nits,
            primaries,
            c_int::from(custom),
            chromaticities.map_or(std::ptr::null(), |values| values.as_ptr()),
            c_int::try_from(quality).unwrap_or(0),
            c_int::from(lossless),
            c_int::try_from(effort).unwrap_or(0),
            distance,
            &mut bytes,
            &mut len,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if status != 0 || bytes.is_null() {
        return Err(VshotError::StillEncode(c_error(&error)));
    }
    let encoded = unsafe { std::slice::from_raw_parts(bytes, len).to_vec() };
    unsafe { vshot_still_free(bytes.cast()) };
    Ok(encoded)
}

/// Reads a JPEG XL file back into the HDR currency.
///
/// `fallback_nits` is the white to read a file at when it carries none of this
/// program's own -- anything written by another program.  `origin` names the
/// file in errors, the way every other codec does.
pub fn decode_jxl(
    bytes: &[u8],
    fallback_nits: f32,
    origin: &str,
) -> Result<crate::model::codec::HdrImage> {
    if !jxl_available() {
        return Err(decode_error(origin, &load_error()));
    }
    let mut rgba: *mut c_float = std::ptr::null_mut();
    let mut width = 0;
    let mut height = 0;
    let mut primaries = -1;
    let mut custom = 0;
    let mut reference_nits = fallback_nits;
    let mut chromaticities = [0.0f32; 6];
    let mut error = [0i8; 512];
    let status = unsafe {
        vshot_still_decode_jxl(
            bytes.as_ptr(),
            bytes.len(),
            fallback_nits,
            &mut rgba,
            &mut width,
            &mut height,
            &mut primaries,
            &mut custom,
            &mut reference_nits,
            chromaticities.as_mut_ptr(),
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if status != 0 || rgba.is_null() {
        return Err(decode_error(origin, &c_error(&error)));
    }
    let size = Size::new(
        u32::try_from(width).unwrap_or(u32::MAX),
        u32::try_from(height).unwrap_or(u32::MAX),
    );
    let count = match size.area() {
        Ok(count) if count <= MAX_JXL_PIXELS => count,
        Ok(_) => {
            unsafe { vshot_still_free(rgba.cast()) };
            return Err(decode_error(
                origin,
                "the image is larger than this build decodes",
            ));
        }
        Err(error) => {
            unsafe { vshot_still_free(rgba.cast()) };
            return Err(decode_error(origin, &error.to_string()));
        }
    };
    // The shim hands back one flat buffer of RGBA samples; the currency is one
    // `[r, g, b, a]` per pixel, so the four are grouped here rather than in C.
    let pixels: Vec<[f32; 4]> = unsafe { std::slice::from_raw_parts(rgba, count * 4) }
        .chunks_exact(4)
        .map(|pixel| [pixel[0], pixel[1], pixel[2], pixel[3]])
        .collect();
    unsafe { vshot_still_free(rgba.cast()) };
    let primaries = if custom != 0 {
        Primaries::from_chromaticities(
            (chromaticities[0], chromaticities[1]),
            (chromaticities[2], chromaticities[3]),
            (chromaticities[4], chromaticities[5]),
        )
    } else {
        match primaries {
            0 => Primaries::Bt709,
            1 => Primaries::DisplayP3,
            2 => Primaries::Bt2020,
            _ => {
                // The C side refuses an unnamed gamut already; reaching here
                // would mean the two disagreed about what counts as named.
                return Err(decode_error(
                    origin,
                    "the file names a gamut this build cannot read",
                ));
            }
        }
    };
    let frame = HdrFrame::in_primaries(size, pixels, primaries)
        .map_err(|error| decode_error(origin, &error.to_string()))?;
    Ok(crate::model::codec::HdrImage::new(frame, reference_nits))
}

fn decode_error(origin: &str, reason: &str) -> VshotError {
    VshotError::HdrDecode {
        path: origin.into(),
        reason: reason.into(),
    }
}

/// One of the shim's fixed-size error buffers, read as text.
fn c_error(error: &[i8]) -> String {
    let message = unsafe { CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    if message.is_empty() {
        "ffmpeg could not encode the image".into()
    } else {
        message
    }
}
