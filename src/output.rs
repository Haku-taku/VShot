// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Local};

use crate::cli::Destination;
use crate::error::{Result, VshotError};
use crate::geometry::Point;
use crate::model::codec;
use crate::model::{Frame, HdrFrame};

/// How the HDR half of a capture is written.
///
/// The set of formats is the codec registry's, not a list here: every format is
/// one module behind [`codec::HdrCodec`], and which of them this build has is
/// decided by its cargo features.  What this type adds is the *choice* — a
/// value that came from `--hdr-format` or the config file, resolved once and
/// then carried through the write path.
///
/// Both options hold the light; they differ in whether a reader can be told
/// what the light *means*.  AVIF carries its own colour description, so a
/// BT.2020 PQ image is stated as one and any reader that understands AVIF
/// shows it correctly — at the cost of being lossy.  Radiance RGBE is exact,
/// but its gamut lives in a `PRIMARIES=` header that the readers that matter
/// here (ffmpeg, ImageMagick) ignore, so a wide-gamut capture looks
/// over-saturated in them.
/// Compares by name rather than by address: the registry hands out one static
/// per format, so the addresses would work, but the name is what the value
/// means and what a failure prints.
#[derive(Clone, Copy)]
pub struct HdrFormat(&'static dyn codec::HdrCodec);

impl PartialEq for HdrFormat {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for HdrFormat {}

impl std::fmt::Debug for HdrFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl Default for HdrFormat {
    /// JPEG XL when this machine can write it, and otherwise AVIF.
    ///
    /// JPEG XL is the exact one -- it holds the light as it was captured -- so
    /// it is the default where it exists.  Whether it exists is the machine's
    /// answer, not the build's: a rolling release can ship a libavcodec whose
    /// `libjxl` was left out, and a capture must not fail over that.  AVIF is
    /// the fallback because it states its colour and every reader shows it
    /// right, and after that whatever other codec this build has.
    fn default() -> Self {
        Self(
            codec::by_name("jxl")
                .or_else(|| codec::by_name("avif"))
                .or_else(|| codec::codecs().into_iter().next())
                .expect("a build with no HDR codec cannot write an HDR half"),
        )
    }
}

impl HdrFormat {
    /// The AVIF format, when this build has the codec.
    ///
    /// Nothing in the program reaches for these by name — `parse` and `default`
    /// are what the command line and the config go through — but they are the
    /// format's own vocabulary, and the tests below are written in it.
    #[cfg(feature = "avif")]
    #[allow(non_upper_case_globals, dead_code)]
    pub const Avif: Self = Self(&codec::avif::Avif);

    /// The Radiance RGBE format, when this build has the codec.
    #[cfg(feature = "radiance")]
    #[allow(non_upper_case_globals, dead_code)]
    pub const Radiance: Self = Self(&codec::radiance::Radiance);

    /// The JPEG XL format.  Unlike the two above it is not gated on a feature
    /// but on the machine, so it is offered as a constant only where the shim
    /// says this ffmpeg can write it.
    #[allow(non_upper_case_globals, dead_code)]
    pub const Jxl: Self = Self(&codec::jxl::Jxl);

    /// Parses the `--hdr-format` value.  The names are the file extensions, so
    /// a value copied out of `vshot --help` names the file it produces.
    pub fn parse(value: &str) -> Result<Self> {
        codec::by_name(value).map(Self).ok_or_else(|| {
            VshotError::InvalidDestination(format!(
                "`--hdr-format {other}` is not one of {}",
                codec::names().join(", "),
                other = value
            ))
        })
    }

    /// The suffix the HDR half's file carries.
    pub fn extension(self) -> &'static str {
        self.0.extension()
    }

    /// The name `--hdr-format` and the config file use for it.
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// The parameters this format accepts, in the order a window should list
    /// them.  Radiance declares none.
    pub fn specs(self) -> &'static [codec::ParamSpec] {
        self.0.specs()
    }

    /// Encodes the HDR half at the settings the caller asked for.
    pub fn encode_with(
        self,
        frame: &HdrFrame,
        reference_nits: f32,
        values: &codec::ParamValues,
        density: Option<u32>,
    ) -> Result<Vec<u8>> {
        self.0.encode_with(
            &codec::HdrImage::new(frame.clone(), reference_nits),
            values,
            density,
        )
    }
}

/// The SDR half's format: the PNG a capture is written to, named the way
/// [`HdrFormat`] names the other half.
///
/// A wrapper around the codec registry's own trait object, so the format
/// vocabulary and the codec registry are the same list — and a build with one
/// SDR codec has a one-entry list rather than a special case.
#[derive(Clone, Copy)]
pub struct SdrFormat(&'static dyn codec::SdrCodec);

impl PartialEq for SdrFormat {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for SdrFormat {}

impl std::fmt::Debug for SdrFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl Default for SdrFormat {
    /// PNG, which is what every destination on this side has always been: a
    /// screenshot is written losslessly, and the clipboard and the pin daemon
    /// both speak it.
    fn default() -> Self {
        Self(
            codec::sdr_by_name("png")
                .or_else(|| codec::sdr_codecs().into_iter().next())
                .expect("a build with no SDR codec cannot write a capture"),
        )
    }
}

impl SdrFormat {
    /// Parses the `--sdr-format` value.
    pub fn parse(value: &str) -> Result<Self> {
        codec::sdr_by_name(value).map(Self).ok_or_else(|| {
            VshotError::InvalidDestination(format!(
                "`--sdr-format {value}` is not one of {}",
                codec::sdr_names().join(", ")
            ))
        })
    }

    /// The suffix this format's files carry.
    pub fn extension(self) -> &'static str {
        self.0.extension()
    }

    /// The name `--sdr-format` and the config file use for it.
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// Every suffix this format's files are written with.  More than one for
    /// JPEG, whose `.jpeg` is the same format as its `.jpg`.
    pub fn suffixes(self) -> &'static [&'static str] {
        self.0.suffixes()
    }

    /// The type the clipboard carries this format under.
    pub fn mime(self) -> &'static str {
        self.0.mime()
    }

    /// The parameters this format accepts.
    pub fn specs(self) -> &'static [codec::ParamSpec] {
        self.0.specs()
    }

    /// Encodes the SDR half at the settings the caller asked for.
    pub fn encode(
        self,
        frame: &Frame,
        density: Option<u32>,
        values: &codec::ParamValues,
    ) -> Result<Vec<u8>> {
        self.0.encode(frame, density, values)
    }
}

/// One capture's HDR half, with the light its `1.0` stands for: the output's own
/// SDR white.  Kept together because both consumers need both — the second file
/// is encoded from the frame, while a pinned surface takes it re-encoded at that
/// white, which is what turns the frame back into the absolute codes the panel
/// shows.
///
/// This is the codec layer's [`codec::HdrImage`] under the name the output path
/// has always used for it: the value a decode produces and the value an encode
/// consumes are the same one, which is what makes "read the file back and pin
/// it" a round trip rather than a conversion.
pub type HdrHalf = codec::HdrImage;

/// Writes a captured frame to `destination`. `density` is the frame's device
/// pixels per logical pixel (the scale of the output it came from); every PNG
/// written to a file, stdout or the clipboard carries it as its physical
/// resolution, and the pin destination states it in the request, so the
/// on-screen size and the sharpness of the capture survive wherever the image
/// goes next. `compression` is the PNG encoder's level, which costs time and
/// buys size; it does not apply to the pin destination, whose PNG only travels
/// to the daemon through a temp file.
///
/// `hdr` is the capture's HDR half when the source carried HDR content.  The
/// SDR half is the ordinary PNG at `destination`; the HDR half is written to
/// the same path with its extension replaced by `format`'s — `shot.png` beside
/// `shot.avif` — so the two images of one capture sit together and differ only
/// in their suffix.  The first is the SDR view of the content, the second the
/// HDR content itself.  Only a file destination names that second path: stdout
/// and the clipboard carry one image, and the pin daemon carries the HDR half
/// out of band, as the PQ codes a surface of its own reads (see the `Pin` arm
/// below).
///
/// `sdr_format` and `sdr_values` are the SDR half's own settings, and
/// `hdr_values` the HDR half's; both sets come from the codec registry, so a
/// format this build does not have can never be asked for here.
///
/// `sdr_copy` decides whether a capture that carries HDR content writes the SDR
/// half at all.  With it off only the HDR file is written, at the sibling path
/// its suffix already names; a capture with no HDR content writes the SDR file
/// whatever it says, because that file is then the only one there is.  stdout,
/// the clipboard and the pin carry one image each and are not affected: the
/// setting is about the files a capture leaves behind.
#[allow(clippy::too_many_arguments)]
pub fn write_frame_with_hdr(
    frame: &Frame,
    hdr: Option<&HdrHalf>,
    destination: &Destination,
    density: u32,
    sdr_format: SdrFormat,
    sdr_copy: bool,
    sdr_values: &codec::ParamValues,
    format: HdrFormat,
    hdr_values: &codec::ParamValues,
    // Where the capture came from on the desktop, in global logical pixels,
    // when it came from a place at all.  Only the pin destination uses it: the
    // daemon puts the pin back exactly there instead of in the middle of an
    // output.  Every other destination has no position to keep, and a
    // composed or synthetic image never had one.
    pin_origin: Option<Point>,
    // The marks the editing session left on the image, when it left any.  Only
    // the pin destination uses them: they are what lets the pin be reopened for
    // editing on the user's own marks rather than on the pixels they were
    // flattened into.  `None` for a capture nothing was drawn on, which is
    // every route that never opened an editor.
    pin_marks: Option<&serde_json::Value>,
    // The capture `frame` was flattened from, before any mark was drawn on it,
    // sent beside `pin_marks` so the daemon can keep the picture the marks
    // belong to.  `None` whenever `pin_marks` is.
    pin_base: Option<&[u8]>,
) -> Result<()> {
    match destination {
        Destination::File(path) => {
            let path = expand_output_path(path, Local::now());
            create_parent_directories(&path)?;
            // The clipboard is given the file that was really written, which is
            // not always the one that was named: the SDR half is written under
            // its own format's suffix (see `write_capture_files`), and a URI
            // pointing at the name the user typed would point at nothing.
            let written = write_capture_files(
                &path,
                frame,
                hdr,
                Some(density),
                sdr_format,
                sdr_copy,
                sdr_values,
                format,
                hdr_values,
            )?;
            copy_file_to_clipboard(&written)
        }
        // Both of these carry the SDR half alone, in the format it was asked
        // for: the clipboard is told the type it is, and stdout is bytes with no
        // type at all.
        Destination::Stdout => io::stdout()
            .write_all(&sdr_format.encode(frame, Some(density), sdr_values)?)
            .map_err(|source| VshotError::WriteFile {
                path: "stdout".into(),
                source,
            }),
        Destination::Clipboard => copy_to_clipboard(
            &sdr_format.encode(frame, Some(density), sdr_values)?,
            sdr_format.mime(),
        ),
        // The daemon is told the density outright, so the bytes it loads need
        // no declaration of their own, and they only have to survive the trip
        // through the temp file: the default (fastest useful) level is right.
        //
        // An HDR pinch travels in two files: the SDR PNG, which the daemon loads
        // as it always did -- for the pin's size, its fallback and whatever
        // edits it holds -- and the PQ codes, which the daemon hands to a
        // surface of its own so the image reaches the panel as the light it
        // stands for rather than as the SDR view of it.
        Destination::Pin => {
            // The codes are written in the capture's own primaries, which are
            // the output's: the surface that shows them carries that same
            // description, so what it declares and what the words hold agree.
            let pq = hdr.map(|half| crate::pin::PqPin {
                words: half
                    .frame
                    .to_rgb10_pq_in(half.frame.primaries(), half.reference_nits),
                width: half.frame.size().width,
                height: half.frame.size().height,
                reference_nits: half.reference_nits,
                primaries: half.frame.primaries(),
            });
            crate::pin::pin_png(
                &frame.to_png()?,
                density,
                pin_origin,
                pq.as_ref(),
                pin_marks,
                pin_base,
                // The editor's own handoff, and only that: a pin carrying the
                // marks of the session that just drew them.  Every other route
                // here has nobody drawing the capture, so there is no gap to
                // close and no reason to make the daemon wait for a frame.
                pin_marks.is_some(),
            )
        }
    }
}

/// Writes the SDR image and, when there is one, the HDR image beside it, and
/// returns the path the SDR half went to.  Split out so the offline test can
/// exercise the pairing without `wl-copy`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_capture_files(
    path: &Path,
    frame: &Frame,
    hdr: Option<&HdrHalf>,
    density: Option<u32>,
    sdr_format: SdrFormat,
    sdr_copy: bool,
    sdr_values: &codec::ParamValues,
    format: HdrFormat,
    hdr_values: &codec::ParamValues,
) -> Result<PathBuf> {
    // The SDR half is the destination the user named and the HDR half sits
    // beside it.  When the destination already ends in the HDR format's own
    // suffix the two names would be the same file, so the named path is taken
    // for the HDR half and the SDR image goes to its sibling — the pair still
    // differs only in its suffix, and neither image overwrites the other.
    let (sdr_path, hdr_path) = match hdr {
        Some(_) if hdr_sibling_path(path, format) == path => {
            (sdr_sibling_path(path, sdr_format), path.to_path_buf())
        }
        _ => (
            align_sdr_suffix(path, sdr_format),
            hdr_sibling_path(path, format),
        ),
    };
    // The SDR copy is skipped only when there is an HDR half to write instead:
    // a capture that carries no HDR content has one file, and leaving it out
    // would be a capture that saved nothing at all.
    let write_sdr = sdr_copy || hdr.is_none();
    if write_sdr {
        write_file(&sdr_path, &sdr_format.encode(frame, density, sdr_values)?)?;
    }
    if let Some(hdr) = hdr {
        write_file(
            &hdr_path,
            &format.encode_with(&hdr.frame, hdr.reference_nits, hdr_values, density)?,
        )?;
    }
    // The file that exists, which is what the caller points the clipboard at:
    // with the SDR copy off the HDR half is the only one there is.
    Ok(if write_sdr { sdr_path } else { hdr_path })
}

/// The path the SDR half is really written at: the one the user named, with its
/// suffix replaced when that suffix is one this program's own SDR formats use.
///
/// `--sdr-format webp -o shot.png` would otherwise write WebP bytes under a
/// `.png` name, and everything downstream would believe the name: another
/// program opening it by extension, a later `vshot pin shot.png`, and the file
/// URI a capture puts on the clipboard would all be about a PNG that is not
/// there.  The bytes are what decides, so the name is made to agree with them.
///
/// A suffix of the format being written is already in agreement and is left
/// exactly as it was typed: `.jpeg` is JPEG, and a user whose save dialog
/// offered them `shot.jpeg` gets a file called that rather than one silently
/// renamed to `shot.jpg`.
///
/// A suffix this program never writes is left alone too.  `shot.final` and a
/// bare `shot` are names a user chose to be their own, and a strftime pattern
/// can expand into dotted parts that are not a suffix at all
/// (`shot-%Y.%m.%d`), so replacing whatever follows the last dot would rename
/// files nobody asked to have renamed.
fn align_sdr_suffix(path: &Path, format: SdrFormat) -> PathBuf {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return path.to_path_buf();
    };
    if format
        .suffixes()
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
    {
        return path.to_path_buf();
    }
    // Another format's suffix, or a name this program does not write at all.
    let ours = ["png", "jpg", "jpeg", "webp"]
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known));
    if !ours {
        return path.to_path_buf();
    }
    let mut aligned = path.to_path_buf();
    aligned.set_extension(format.extension());
    aligned
}

/// The HDR half's path: the same file name with the extension replaced, so the
/// two images of one capture sit together and differ only in their suffix.
fn hdr_sibling_path(path: &Path, format: HdrFormat) -> PathBuf {
    let mut sibling = path.to_path_buf();
    sibling.set_extension(format.extension());
    sibling
}

/// The SDR half's path, for a destination that named the HDR one outright.
fn sdr_sibling_path(path: &Path, format: SdrFormat) -> PathBuf {
    let mut sibling = path.to_path_buf();
    sibling.set_extension(format.extension());
    sibling
}

fn expand_output_path(path: &Path, now: DateTime<Local>) -> PathBuf {
    let pattern = path.to_string_lossy();
    PathBuf::from(now.format(&pattern).to_string())
}

/// Makes the directory an `--output` path names, so a pattern can place its file
/// in a directory that only comes into being as the pattern expands.
///
/// A strftime pattern is a path *recipe*, not a path: `Screenshots/%Y%m/shot.png`
/// names a different directory every month, and the user cannot pre-create next
/// month's.  So the directory is made here rather than left to `File::create`,
/// whose "No such file or directory" says nothing about which part was missing.
/// This is the rule the recording paths already follow for vshot's own videos
/// directory, applied to every `-o` path.
pub fn create_parent_directories(path: &Path) -> Result<()> {
    // A bare file name has no directory to make; an empty parent (`/shot.png`)
    // is the root and already exists.
    let Some(directory) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    if directory.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(directory).map_err(|source| VshotError::CreateOutputDirectory {
        path: directory.to_path_buf(),
        source,
    })
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path).map_err(|source| VshotError::WriteFile {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes)
        .map_err(|source| VshotError::WriteFile {
            path: path.to_path_buf(),
            source,
        })?;
    file.flush().map_err(|source| VshotError::WriteFile {
        path: path.to_path_buf(),
        source,
    })
}

/// Puts a captured image on the clipboard, under the type its format carries.
///
/// The type is the format's own: the clipboard is one of the destinations a
/// capture can be sent to, and a format that is not PNG has to say so or every
/// paste target reads the bytes as the wrong thing.
fn copy_to_clipboard(bytes: &[u8], mime: &str) -> Result<()> {
    copy_bytes_to_clipboard(bytes, mime)
}

/// Puts recognized text on the clipboard.  `text/plain` is what a paste into
/// an editor asks for; `wl-copy` keeps serving it until the next copy.
pub fn copy_text_to_clipboard(text: &str) -> Result<()> {
    copy_bytes_to_clipboard(text.as_bytes(), "text/plain")
}

/// Puts a PNG that is already encoded on the clipboard, for a caller whose
/// result arrived as bytes rather than as a `Frame` (the translate overlay's
/// composited image, which the Qt helper wrote to a file).
pub fn copy_png_to_clipboard(bytes: &[u8]) -> Result<()> {
    copy_to_clipboard(bytes, "image/png")
}

/// Expands a `--output` path's strftime pattern with the current time, the way
/// `write_frame` does.  It exists for the translate overlay, which has to hand
/// the helper an absolute path to write to before any `Frame` exists.
pub fn expanded_output_path(path: &Path) -> PathBuf {
    expand_output_path(path, Local::now())
}

fn copy_file_to_clipboard(path: &Path) -> Result<()> {
    let absolute = path.canonicalize().map_err(|source| {
        VshotError::Clipboard(format!(
            "failed to resolve output file {} for the clipboard: {source}",
            path.display()
        ))
    })?;
    let uri = format!("{}\n", file_uri(&absolute));
    copy_bytes_to_clipboard(uri.as_bytes(), "text/uri-list")
}

fn copy_bytes_to_clipboard(bytes: &[u8], mime_type: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg(mime_type)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| VshotError::CommandIo {
            program: "wl-copy".into(),
            source,
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| VshotError::Clipboard("wl-copy stdin was not available".into()))?;
    stdin.write_all(bytes).map_err(|source| {
        VshotError::Clipboard(format!("failed to send {mime_type} to wl-copy: {source}"))
    })?;
    drop(stdin);
    let status = child.wait().map_err(|source| VshotError::CommandIo {
        program: "wl-copy".into(),
        source,
    })?;
    if !status.success() {
        return Err(VshotError::Clipboard(format!(
            "wl-copy exited with {status}"
        )));
    }
    Ok(())
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.to_string_lossy().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;

    /// A format's parameters at their own declared defaults, which is what a
    /// capture with nothing configured hands its encoder.
    fn default_params(format: impl Specs) -> codec::ParamValues {
        codec::ParamValues::defaults(format.specs())
    }

    /// The one thing both registries' format wrappers have in common, so the
    /// test helper above can take either.
    trait Specs {
        fn specs(self) -> &'static [codec::ParamSpec];
    }

    impl Specs for HdrFormat {
        fn specs(self) -> &'static [codec::ParamSpec] {
            HdrFormat::specs(self)
        }
    }

    impl Specs for SdrFormat {
        fn specs(self) -> &'static [codec::ParamSpec] {
            SdrFormat::specs(self)
        }
    }

    /// The SDR registry's outward shape: a name that parses back to the same
    /// format, an extension that is a suffix, and a MIME type for the clipboard.
    #[test]
    fn every_sdr_format_round_trips_through_its_own_name() {
        for sdr in codec::sdr_codecs() {
            let format = SdrFormat::parse(sdr.name()).expect("a name the build offers");
            assert_eq!(format.name(), sdr.name());
            assert_eq!(format.extension(), sdr.extension());
            assert!(
                format.mime().contains('/'),
                "{} has a MIME type",
                sdr.name()
            );
        }
        assert_eq!(SdrFormat::default(), SdrFormat::parse("png").unwrap());
    }

    #[test]
    fn an_sdr_format_name_no_codec_has_is_refused() {
        // A name no codec here offers, so the test holds whatever this machine
        // and this build happen to have: `tiff` is not one of them either way.
        let error = SdrFormat::parse("tiff").unwrap_err().to_string();
        assert!(error.contains("--sdr-format"), "{error}");
        assert!(
            error.contains("png"),
            "the message names what it wanted: {error}"
        );
    }

    #[test]
    fn frame_png_is_nonempty() {
        let frame = Frame::solid(Size::new(1, 1), [1, 2, 3, 255]).unwrap();
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vshot-output-test-{}-{}.png",
            std::process::id(),
            unique_suffix()
        ));
        write_file(&path, &frame.to_png().unwrap()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn output_path_keeps_prefix_and_suffix_around_time_format() {
        let now = Local::now();
        let path = expand_output_path(Path::new("shots/vshot-%Y%m%d-%H%M%S.final.png"), now);
        let expected_date = now.format("%Y%m%d-%H%M%S").to_string();
        assert_eq!(
            path,
            PathBuf::from(format!("shots/vshot-{expected_date}.final.png"))
        );
    }

    /// The directory a strftime pattern names comes into being with the pattern.
    ///
    /// `Screenshots/%Y%m/shot.png` names a different directory every month, and
    /// the user cannot pre-create next month's — so the write has to make it.
    /// Before this, such a pattern failed with a bare "No such file or
    /// directory" that named neither the missing part nor the way out.
    #[test]
    fn a_pattern_whose_directory_does_not_exist_yet_is_still_writable() {
        let mut root = std::env::temp_dir();
        root.push(format!(
            "vshot-output-dir-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        // Two levels that do not exist, the shape `Screenshots/%Y%m/` has.
        let path = expand_output_path(&root.join("%Y%m/shot.png"), Local::now());
        assert!(
            !path.parent().unwrap().exists(),
            "the fixture starts absent"
        );

        create_parent_directories(&path).unwrap();
        assert!(
            path.parent().unwrap().is_dir(),
            "the month directory is made"
        );

        let frame = Frame::solid(Size::new(1, 1), [1, 2, 3, 255]).unwrap();
        write_file(&path, &frame.to_png().unwrap()).unwrap();
        assert!(std::fs::read(&path).unwrap().starts_with(b"\x89PNG"));

        // A second write into the same month is not an error: an existing
        // directory is left alone rather than refused.
        create_parent_directories(&path).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A bare file name has no directory to make, and the root is not made
    /// either — it is already there.
    #[test]
    fn a_path_with_no_directory_is_left_alone() {
        create_parent_directories(Path::new("shot.png")).unwrap();
        create_parent_directories(Path::new("/shot.png")).unwrap();
    }

    #[test]
    fn output_path_preserves_escaped_percent() {
        let now = Local::now();
        let path = expand_output_path(Path::new("vshot-%%-%Y.png"), now);
        assert!(path.to_string_lossy().starts_with("vshot-%-"));
        assert!(path.to_string_lossy().ends_with(".png"));
    }

    /// A typo is refused rather than silently falling back to a format the user
    /// did not ask for, and the message names the flag.  Asked of a name no
    /// codec has, so it holds whatever this build's features are.
    #[test]
    fn an_hdr_format_name_no_codec_has_is_refused() {
        let error = HdrFormat::parse("avifs").unwrap_err().to_string();
        assert!(error.contains("--hdr-format"), "{error}");
    }

    /// Every name the build does have parses back to itself, and every
    /// extension resolves to the codec that owns it — which is the round trip
    /// the help, the config file and the pin path all rely on.
    #[test]
    fn every_built_format_parses_its_own_name_and_extension() {
        for codec in codec::codecs() {
            let format = HdrFormat::parse(codec.name()).expect("a name the build offers");
            assert_eq!(format.name(), codec.name());
            assert_eq!(format.extension(), codec.extension());
        }
    }

    /// The default is the exact format where the machine can write it, and AVIF
    /// where it cannot: JPEG XL holds the light as it was captured, while AVIF
    /// is the one every reader understands.
    ///
    /// Which of the two it is depends on the *machine* -- a libavcodec without
    /// `libjxl` is an ordinary thing to meet -- so the test states the rule
    /// rather than a name, and holds either way.
    #[cfg(feature = "avif")]
    #[test]
    fn the_default_hdr_format_is_the_exact_one_when_the_machine_has_it() {
        let expected = if codec::ffmpeg_still::jxl_available() {
            HdrFormat::Jxl
        } else {
            HdrFormat::Avif
        };
        assert_eq!(HdrFormat::default(), expected);
        assert_eq!(HdrFormat::parse("avif").unwrap(), HdrFormat::Avif);
        assert_eq!(HdrFormat::Avif.extension(), "avif");
    }

    #[cfg(feature = "radiance")]
    #[test]
    fn the_radiance_format_parses_its_own_name() {
        assert_eq!(HdrFormat::parse("hdr").unwrap(), HdrFormat::Radiance);
        assert_eq!(HdrFormat::Radiance.extension(), "hdr");
    }

    /// The HDR half's name is the SDR half's with the suffix swapped, and
    /// nothing else about it changes: `a.final.png` keeps its `a.final`.
    #[cfg(feature = "avif")]
    #[test]
    fn the_hdr_sibling_swaps_only_the_suffix() {
        assert_eq!(
            hdr_sibling_path(Path::new("shots/vshot-2026.png"), HdrFormat::Avif),
            PathBuf::from("shots/vshot-2026.avif")
        );
        // No extension at all: the HDR half still gets one.
        assert_eq!(
            hdr_sibling_path(Path::new("shot"), HdrFormat::Avif),
            PathBuf::from("shot.avif")
        );
    }

    /// The SDR half's suffix is made to agree with the bytes, and left alone
    /// when it already does -- including when it is the format's *other*
    /// spelling, which is the one a user may have just picked in the dialog.
    #[test]
    fn the_sdr_suffix_is_made_to_agree_with_the_bytes() {
        let png = SdrFormat::parse("png").unwrap();
        // Another format's suffix is replaced rather than appended to.
        assert_eq!(
            align_sdr_suffix(Path::new("shot.webp"), png),
            PathBuf::from("shot.png")
        );
        // A name this program never writes is the user's own and stays.
        assert_eq!(
            align_sdr_suffix(Path::new("shot.final"), png),
            PathBuf::from("shot.final")
        );
        if let Ok(jpeg) = SdrFormat::parse("jpeg") {
            // The two spellings are one format, so neither is a lie and
            // neither is renamed.
            for name in ["shot.jpg", "shot.jpeg", "shot.JPEG"] {
                assert_eq!(
                    align_sdr_suffix(Path::new(name), jpeg),
                    PathBuf::from(name),
                    "{name}"
                );
            }
            // And the other formats' suffixes still are.
            assert_eq!(
                align_sdr_suffix(Path::new("shot.png"), jpeg),
                PathBuf::from("shot.jpg")
            );
        }
    }

    /// A path with more than one dot keeps everything but the last part.
    #[cfg(feature = "radiance")]
    #[test]
    fn the_hdr_sibling_keeps_a_dotted_stem() {
        assert_eq!(
            hdr_sibling_path(Path::new("a.final.png"), HdrFormat::Radiance),
            PathBuf::from("a.final.hdr")
        );
    }

    #[cfg(feature = "radiance")]
    #[test]
    fn an_hdr_capture_writes_both_files_with_the_same_stem() {
        let frame = Frame::solid(Size::new(2, 2), [10, 20, 30, 255]).unwrap();
        let hdr = HdrHalf {
            frame: HdrFrame::new(Size::new(2, 2), vec![[4.0, 2.0, 1.0, 1.0]; 4]).unwrap(),
            reference_nits: 203.0,
        };
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vshot-hdr-output-test-{}-{}.png",
            std::process::id(),
            unique_suffix()
        ));
        write_capture_files(
            &path,
            &frame,
            Some(&hdr),
            Some(1),
            SdrFormat::default(),
            true,
            &default_params(SdrFormat::default()),
            HdrFormat::Radiance,
            &default_params(HdrFormat::Radiance),
        )
        .unwrap();
        let png = std::fs::read(&path).unwrap();
        let radiance = std::fs::read(hdr_sibling_path(&path, HdrFormat::Radiance)).unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(hdr_sibling_path(&path, HdrFormat::Radiance));
        assert!(
            png.starts_with(b"\x89PNG\r\n\x1a\n"),
            "the SDR half is a PNG"
        );
        assert!(
            radiance.starts_with(b"#?RADIANCE"),
            "the HDR half is Radiance"
        );
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(
            hdr_sibling_path(&path, HdrFormat::Radiance)
                .extension()
                .unwrap(),
            "hdr"
        );
    }

    #[cfg(feature = "avif")]
    #[test]
    fn the_default_hdr_format_writes_an_avif_beside_the_png() {
        // The pair a capture writes by default: a PNG and an AVIF of the same
        // stem, the AVIF an ISOBMFF file declaring BT.2020 PQ.
        let frame = Frame::solid(Size::new(2, 2), [10, 20, 30, 255]).unwrap();
        let hdr = HdrHalf {
            frame: HdrFrame::new(Size::new(16, 16), vec![[4.0, 2.0, 1.0, 1.0]; 256]).unwrap(),
            reference_nits: 203.0,
        };
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vshot-avif-output-test-{}-{}.png",
            std::process::id(),
            unique_suffix()
        ));
        write_capture_files(
            &path,
            &frame,
            Some(&hdr),
            Some(1),
            SdrFormat::default(),
            true,
            &default_params(SdrFormat::default()),
            HdrFormat::Avif,
            &default_params(HdrFormat::Avif),
        )
        .unwrap();
        let avif_path = hdr_sibling_path(&path, HdrFormat::Avif);
        let avif = std::fs::read(&avif_path).unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&avif_path);
        assert_eq!(&avif[4..8], b"ftyp", "the HDR half is an ISOBMFF file");
        assert!(avif.windows(4).any(|window| window == b"avif"));
    }

    #[cfg(feature = "radiance")]
    #[test]
    fn a_destination_named_for_the_hdr_half_does_not_overwrite_the_png() {
        // A destination that already ends in the HDR format's suffix names the
        // HDR half; the PNG goes beside it rather than to the same path, so
        // neither file is lost.
        let frame = Frame::solid(Size::new(2, 2), [10, 20, 30, 255]).unwrap();
        let hdr = HdrHalf {
            frame: HdrFrame::new(Size::new(2, 2), vec![[4.0, 2.0, 1.0, 1.0]; 4]).unwrap(),
            reference_nits: 203.0,
        };
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vshot-hdr-destination-test-{}-{}.hdr",
            std::process::id(),
            unique_suffix()
        ));
        write_capture_files(
            &path,
            &frame,
            Some(&hdr),
            Some(1),
            SdrFormat::default(),
            true,
            &default_params(SdrFormat::default()),
            HdrFormat::Radiance,
            &default_params(HdrFormat::Radiance),
        )
        .unwrap();
        let png_path = sdr_sibling_path(&path, SdrFormat::default());
        let png = std::fs::read(&png_path).unwrap();
        let radiance = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&png_path);
        assert!(
            png.starts_with(b"\x89PNG\r\n\x1a\n"),
            "the SDR half is a PNG"
        );
        assert!(
            radiance.starts_with(b"#?RADIANCE"),
            "the HDR half is Radiance"
        );
        assert_eq!(png_path.extension().unwrap(), "png");
    }

    #[test]
    fn a_capture_without_hdr_writes_the_sdr_file_alone() {
        let frame = Frame::solid(Size::new(1, 1), [1, 2, 3, 255]).unwrap();
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vshot-sdr-output-test-{}-{}.png",
            std::process::id(),
            unique_suffix()
        ));
        let format = HdrFormat::default();
        write_capture_files(
            &path,
            &frame,
            None,
            Some(1),
            SdrFormat::default(),
            true,
            &default_params(SdrFormat::default()),
            format,
            &default_params(format),
        )
        .unwrap();
        assert!(path.exists());
        // No HDR half was written, and asking for one under any of the build's
        // formats finds nothing.
        for codec in codec::codecs() {
            assert!(!hdr_sibling_path(&path, HdrFormat::parse(codec.name()).unwrap()).exists());
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_uri_percent_encodes_path_bytes() {
        assert_eq!(
            file_uri(Path::new("/tmp/vshot image#%.png")),
            "file:///tmp/vshot%20image%23%25.png"
        );
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
