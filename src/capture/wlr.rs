// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

use memmap2::{MmapMut, MmapOptions};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_output_v1, wp_color_manager_v1, wp_image_description_info_v1,
    wp_image_description_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::error::{Result, VshotError};
use crate::geometry::{Rect, Size};
use crate::model::hdr::{
    OutputColor, Primaries, Rgb10Summary, ToneMap, Transfer, REFERENCE_WHITE_NITS,
};
use crate::model::{Frame, HdrFrame};

use super::dmabuf::{
    is_hdr_fourcc, swap_red_blue_10, DmabufFrame, GbmBuffer, DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_ARGB2101010,
};

#[derive(Debug)]
struct CaptureBuffer {
    _file: tempfile::NamedTempFile,
    map: MmapMut,
    buffer: wl_buffer::WlBuffer,
    width: u32,
    height: u32,
    stride: usize,
    format: wl_shm::Format,
}

impl CaptureBuffer {
    fn new<State>(
        shm: &wl_shm::WlShm,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
        qh: &QueueHandle<State>,
    ) -> Result<Self>
    where
        State: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        // The four 32-bit layouts a compositor may hand over: the `…rgb…` names
        // are BGRA in memory and the `…bgr…` ones RGBA, while the fourth byte
        // is alpha in the `A…` forms and padding in the `X…` ones.  wlroots
        // with the pixman renderer — a headless or software-rendered session —
        // offers XBGR8888, which is why the `…bgr…` pair is here at all.  A
        // 10-bit offer (the `…2101010` four) is read as 8-bit sRGB in
        // `decode_10bit_shm`: the compositor renders an sRGB image into it even
        // at that depth.  Every layout is four bytes per pixel, so the buffer
        // maths below is the same for all.
        if !matches!(
            format,
            wl_shm::Format::Argb8888
                | wl_shm::Format::Xrgb8888
                | wl_shm::Format::Abgr8888
                | wl_shm::Format::Xbgr8888
        ) && !is_10bit_shm(format)
        {
            return Err(VshotError::UnsupportedOutput(format!(
                "wlr-screencopy returned unsupported wl_shm format {format:?}"
            )));
        }
        let width_usize = usize::try_from(width)
            .map_err(|_| VshotError::WaylandProtocol("capture width is too large".into()))?;
        let height_usize = usize::try_from(height)
            .map_err(|_| VshotError::WaylandProtocol("capture height is too large".into()))?;
        let stride = usize::try_from(stride)
            .map_err(|_| VshotError::WaylandProtocol("capture stride is too large".into()))?;
        let minimum_stride = width_usize
            .checked_mul(4)
            .ok_or_else(|| VshotError::WaylandProtocol("capture stride overflows".into()))?;
        if stride < minimum_stride {
            return Err(VshotError::WaylandProtocol(format!(
                "compositor returned a stride of {stride} for a {width}-pixel frame"
            )));
        }
        let bytes = stride
            .checked_mul(height_usize)
            .ok_or_else(|| VshotError::WaylandProtocol("capture buffer size overflows".into()))?;
        let pool_size = i32::try_from(bytes).map_err(|_| {
            VshotError::WaylandProtocol("capture buffer exceeds Wayland's signed size limit".into())
        })?;
        let file = tempfile::Builder::new()
            .prefix("vshot-")
            .tempfile_in("/dev/shm")
            .map_err(|source| {
                VshotError::WaylandProtocol(format!("failed to create capture SHM file: {source}"))
            })?;
        file.as_file()
            .set_len(u64::try_from(bytes).map_err(|_| {
                VshotError::WaylandProtocol("capture buffer size is invalid".into())
            })?)
            .map_err(|source| {
                VshotError::WaylandProtocol(format!("failed to size capture SHM file: {source}"))
            })?;
        let map =
            unsafe { MmapOptions::new().len(bytes).map_mut(file.as_file()) }.map_err(|source| {
                VshotError::WaylandProtocol(format!("failed to map capture SHM buffer: {source}"))
            })?;
        let pool = shm.create_pool(file.as_fd(), pool_size, qh, ());
        let buffer = pool.create_buffer(
            0,
            i32::try_from(width)
                .map_err(|_| VshotError::WaylandProtocol("capture width is too large".into()))?,
            i32::try_from(height)
                .map_err(|_| VshotError::WaylandProtocol("capture height is too large".into()))?,
            i32::try_from(stride)
                .map_err(|_| VshotError::WaylandProtocol("capture stride is too large".into()))?,
            format,
            qh,
            (),
        );
        pool.destroy();
        Ok(Self {
            _file: file,
            map,
            buffer,
            width,
            height,
            stride,
            format,
        })
    }

    fn into_frame(self, y_invert: bool) -> Result<Frame> {
        convert_shm_pixels(
            &self.map,
            self.width,
            self.height,
            self.stride,
            self.format,
            y_invert,
        )
    }

    /// Reads this 10-bit buffer's pixels as DRM `XRGB2101010`-packed words —
    /// red in bits 20..30, green in 10..20, blue in 0..10.
    ///
    /// Both the sRGB fallback ([`decode_10bit_shm`]) and the HDR decode start
    /// here; only the transfer function they apply differs.
    fn words(&self, y_invert: bool) -> Result<Vec<u32>> {
        ten_bit_words(
            &self.map,
            self.width,
            self.height,
            self.stride,
            self.format,
            y_invert,
        )
    }
}

fn convert_shm_pixels(
    map: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: wl_shm::Format,
    y_invert: bool,
) -> Result<Frame> {
    if is_10bit_shm(format) {
        return decode_10bit_shm(map, width, height, stride, format, y_invert);
    }
    if !matches!(
        format,
        wl_shm::Format::Argb8888
            | wl_shm::Format::Xrgb8888
            | wl_shm::Format::Abgr8888
            | wl_shm::Format::Xbgr8888
    ) {
        return Err(VshotError::UnsupportedOutput(format!(
            "unsupported wl_shm format {format:?}"
        )));
    }
    let width = usize::try_from(width)
        .map_err(|_| VshotError::WaylandProtocol("capture width is too large".into()))?;
    let height = usize::try_from(height)
        .map_err(|_| VshotError::WaylandProtocol("capture height is too large".into()))?;
    let minimum_stride = width
        .checked_mul(4)
        .ok_or_else(|| VshotError::WaylandProtocol("capture stride overflows".into()))?;
    if stride < minimum_stride {
        return Err(VshotError::WaylandProtocol(
            "capture stride is smaller than the frame width".into(),
        ));
    }
    let expected = stride
        .checked_mul(height)
        .ok_or_else(|| VshotError::WaylandProtocol("capture buffer size overflows".into()))?;
    if map.len() < expected {
        return Err(VshotError::WaylandProtocol(
            "capture SHM mapping is smaller than the advertised stride".into(),
        ));
    }
    let pixel_count = width
        .checked_mul(height)
        .and_then(|area| area.checked_mul(4))
        .ok_or_else(|| VshotError::WaylandProtocol("capture frame is too large".into()))?;
    let mut pixels = vec![0u8; pixel_count];
    // What the bytes mean.  The `…rgb…` names are BGRA in memory and vshot
    // works in RGBA — the same four bytes with the red and blue ends swapped —
    // while the `…bgr…` names are already RGBA.  The fourth byte is alpha in
    // the `A…` forms and padding in the `X…` ones, and padding has to be
    // written out opaque.  Swapping as whole 32-bit words instead of one byte
    // at a time is what lets the loop go several times faster: it moves the
    // same pixels with a fraction of the memory traffic, which matters because
    // this runs once per grabbed frame.
    let (swap, alpha) = match format {
        wl_shm::Format::Argb8888 => (true, None),
        wl_shm::Format::Xrgb8888 => (true, Some(0xFF00_0000)),
        wl_shm::Format::Abgr8888 => (false, None),
        wl_shm::Format::Xbgr8888 => (false, Some(0xFF00_0000)),
        _ => unreachable!("unsupported format was rejected above"),
    };
    let row_bytes = width * 4;
    for (destination_y, destination_row) in pixels.chunks_exact_mut(row_bytes).enumerate() {
        let source_y = if y_invert {
            height - 1 - destination_y
        } else {
            destination_y
        };
        let source_row = &map[source_y * stride..source_y * stride + row_bytes];
        for (destination, source) in destination_row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(source_row.as_chunks::<4>().0.iter())
        {
            let word = u32::from_le_bytes([source[0], source[1], source[2], source[3]]);
            let ordered = if swap {
                (word & 0x0000_00FF) << 16 | (word & 0x00FF_0000) >> 16 | (word & 0xFF00_FF00)
            } else {
                word
            };
            let pixel = alpha.map_or(ordered, |opaque| ordered | opaque);
            destination.copy_from_slice(&pixel.to_le_bytes());
        }
    }
    Frame::new(Size::new(width as u32, height as u32), pixels)
}

/// Whether a `wl_shm` format is one of the packed 10-bit layouts.  All four are
/// four bytes per pixel, like the 8-bit family, so a shm offer of one allocates
/// exactly the buffer the 8-bit path would; only the decode differs.
fn is_10bit_shm(format: wl_shm::Format) -> bool {
    matches!(
        format,
        wl_shm::Format::Argb2101010
            | wl_shm::Format::Xrgb2101010
            | wl_shm::Format::Abgr2101010
            | wl_shm::Format::Xbgr2101010
    )
}

/// Reads a packed 10-bit shm capture into DRM `XRGB2101010`-ordered words
/// — red in bits 20..30, green in 10..20, blue in 0..10 — undoing the red/blue
/// swap the `…bgr…` layouts carry.
///
/// Both the sRGB fallback ([`decode_10bit_shm`]) and the HDR decode (in
/// [`WlrCapture::capture`] and [`WlrCapture::capture_output_hdr`]) start from
/// these words; only the transfer function they apply differs.
fn ten_bit_words(
    map: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: wl_shm::Format,
    y_invert: bool,
) -> Result<Vec<u32>> {
    // Which way round the red and blue fields sit.
    let bgr = match format {
        wl_shm::Format::Xrgb2101010 | wl_shm::Format::Argb2101010 => false,
        wl_shm::Format::Xbgr2101010 | wl_shm::Format::Abgr2101010 => true,
        _ => unreachable!("only 10-bit formats are decoded here"),
    };
    let width = usize::try_from(width)
        .map_err(|_| VshotError::WaylandProtocol("capture width is too large".into()))?;
    let height = usize::try_from(height)
        .map_err(|_| VshotError::WaylandProtocol("capture height is too large".into()))?;
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| VshotError::WaylandProtocol("capture stride overflows".into()))?;
    if stride < row_bytes {
        return Err(VshotError::WaylandProtocol(
            "capture stride is smaller than the frame width".into(),
        ));
    }
    let expected = stride
        .checked_mul(height)
        .ok_or_else(|| VshotError::WaylandProtocol("capture buffer size overflows".into()))?;
    if map.len() < expected {
        return Err(VshotError::WaylandProtocol(
            "capture SHM mapping is smaller than the advertised stride".into(),
        ));
    }
    let mut words = Vec::with_capacity(width * height);
    for destination_y in 0..height {
        let source_y = if y_invert {
            height - 1 - destination_y
        } else {
            destination_y
        };
        let source_row = &map[source_y * stride..source_y * stride + row_bytes];
        for source in source_row.chunks_exact(4) {
            let word = u32::from_le_bytes([source[0], source[1], source[2], source[3]]);
            words.push(if bgr { swap_red_blue_10(word) } else { word });
        }
    }
    Ok(words)
}

/// Decodes a packed 10-bit shm capture into the 8-bit `Frame` the scene is
/// composed in.
///
/// This is the *SDR* reading of a 10-bit buffer: a compositor offers ten-bit
/// channels for a 10-bit SDR output too (no HDR description on it), and the
/// pixels are then ordinary sRGB at more depth.  The ten-bit channels are
/// rounded down to the eight-bit sRGB the rest of the pipeline speaks.  A
/// buffer that holds HDR is caught first — by the output's own colour
/// description, in [`WlrCapture::capture`] and
/// [`WlrCapture::capture_output_hdr`] — so the 8-bit scene never shows HDR
/// pixels read as sRGB.
fn decode_10bit_shm(
    map: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: wl_shm::Format,
    y_invert: bool,
) -> Result<Frame> {
    let words = ten_bit_words(map, width, height, stride, format, y_invert)?;
    let mut pixels = vec![0u8; words.len() * 4];
    for (destination, word) in pixels.chunks_exact_mut(4).zip(words) {
        destination[0] = ten_to_eight((word >> 20) & 0x3ff);
        destination[1] = ten_to_eight((word >> 10) & 0x3ff);
        destination[2] = ten_to_eight(word & 0x3ff);
        // A capture is opaque; the `A…` alpha is the compositor's, not a
        // transparency the screenshot should carry.
        destination[3] = 255;
    }
    if std::env::var_os("VSHOT_HDR_DEBUG").is_some() {
        eprintln!(
            "vshot: hdr: {width}x{height} {format:?} capture read as 8-bit sRGB \
             (the compositor offered no HDR to the dma-buf route)"
        );
    }
    Frame::new(Size::new(width, height), pixels)
}

/// A 10-bit channel code to the 8-bit byte nearest it, rounded rather than
/// truncated so a flat band does not drift a level darker.
fn ten_to_eight(code: u32) -> u8 {
    ((code * 255 + 511) / 1023) as u8
}

/// Logs what a 10-bit capture held, behind `VSHOT_HDR_DEBUG`.  The reading
/// decides nothing — the encoding comes from the format, not the pixels (see
/// the note in `model::hdr`) — but it lets a capture be checked against what
/// produced it.
fn trace_rgb10(name: &str, width: u32, height: u32, format: wl_shm::Format, words: &[u32]) {
    if std::env::var_os("VSHOT_HDR_DEBUG").is_none() {
        return;
    }
    let summary = Rgb10Summary::of(words);
    eprintln!(
        "vshot: hdr shm {name}: {width}x{height} {format:?} median={} p999={} max={} white={:.4} read as the output's own encoding",
        summary.median, summary.p999, summary.max, summary.white_share
    );
}

impl Drop for CaptureBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

#[derive(Debug)]
struct PendingCapture {
    _frame: zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    buffer: Option<CaptureBuffer>,
    /// The shm offer from the `buffer` event, remembered so the shm buffer
    /// is allocated lazily — only when the dma-buf path does not take the
    /// frame.  (Version 3 defers the copy to `buffer_done`, so nothing
    /// needs the buffer before then.)
    shm_offer: Option<ShmOffer>,
    /// Set for a capture that wants a linux-dmabuf buffer.
    want_dmabuf: bool,
    /// The pool slot the compositor's `linux_dmabuf` event picked.
    dmabuf_slot: Option<usize>,
    y_invert: bool,
    copy_sent: bool,
    complete: bool,
    error: Option<String>,
}

/// The wl_shm buffer parameters of one capture, from its `buffer` event.
#[derive(Clone, Copy, Debug)]
struct ShmOffer {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

/// Size of the zero-copy buffer pool.  Each slot holds a full-size dma-buf
/// (33 MB at 4K); four slots let the compositor render into one while the
/// encoder still reads the previous ones, without the cost of a deeper pool.
const DMABUF_POOL_SLOTS: usize = 4;

/// One pooled dma-buf with the `wl_buffer` the compositor renders into.
struct DmabufSlot {
    gbm: GbmBuffer,
    wl_buffer: wl_buffer::WlBuffer,
}

impl std::fmt::Debug for DmabufSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DmabufSlot")
            .field("fd", &self.gbm.fd())
            .finish_non_exhaustive()
    }
}

impl Drop for DmabufSlot {
    fn drop(&mut self) {
        self.wl_buffer.destroy();
    }
}

/// A rotating set of dma-bufs a screencopy capture can render into,
/// together with the shape they were built for.  A capture whose offer does
/// not match the pool's shape falls back to the shm path rather than
/// rebuilding mid-recording.
#[derive(Debug)]
struct DmabufPool {
    slots: Vec<DmabufSlot>,
    next: usize,
    width: u32,
    height: u32,
    fourcc: u32,
}

#[derive(Clone, Copy, Debug)]
struct OutputUserData {
    global_id: u32,
}

/// One in-flight `wp_color_manager_v1` output query, filled by the events its
/// objects send.  The output's description and the description's information
/// arrive asynchronously, so the query's parts are collected here between
/// dispatches (see [`WlrCapture::output_color`]).
#[derive(Debug, Default)]
struct ColorQuery {
    // Held so the output object outlives the description made from it.
    _output: Option<wp_color_management_output_v1::WpColorManagementOutputV1>,
    description: Option<wp_image_description_v1::WpImageDescriptionV1>,
    info: Option<wp_image_description_info_v1::WpImageDescriptionInfoV1>,
    ready: bool,
    failed: bool,
    done: bool,
    transfer: Option<u32>,
    primaries: Option<u32>,
    min_nits: Option<f32>,
    max_nits: Option<f32>,
    reference_nits: Option<f32>,
    target_max_nits: Option<f32>,
    max_cll: Option<u32>,
}

impl ColorQuery {
    /// The collected events as an [`OutputColor`], or `None` if the description
    /// never said what its reference white is (some parametric descriptions omit
    /// the transfer or the luminances).
    fn into_output_color(self) -> Option<OutputColor> {
        let transfer = match self.transfer? {
            11 => Transfer::Pq,
            13 => Transfer::Hlg,
            5 => Transfer::Linear,
            _ => Transfer::Srgb,
        };
        let primaries = match self.primaries {
            Some(6) => Primaries::Bt2020,
            _ => Primaries::Bt709,
        };
        Some(OutputColor {
            transfer,
            primaries,
            reference_nits: self.reference_nits?,
            min_nits: self.min_nits.unwrap_or(0.0),
            max_nits: self.target_max_nits.or(self.max_nits).unwrap_or(0.0),
        })
    }
}

#[derive(Debug, Default)]
struct CaptureState {
    shm: Option<wl_shm::WlShm>,
    manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    manager_version: u32,
    dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    dmabuf_version: u32,
    /// The colour-management global, bound so this client counts as
    /// colour-management aware.  A compositor that hands HDR over through a
    /// capture only does so for such a client.  Hyprland 0.56 binds nothing to
    /// it — its capture path maps the frame to sRGB whatever the client does
    /// (see `decode_10bit_shm`) — but holding the manager costs nothing and is
    /// what a compositor that does signal HDR capture keys on.
    color_manager: Option<wp_color_manager_v1::WpColorManagerV1>,
    /// Whether an HDR capture is wanted this run, filled from the target's
    /// output colour (see [`WlrCapture::output_color`]).
    color_query: Option<ColorQuery>,
    outputs: HashMap<u32, wl_output::WlOutput>,
    output_names: HashMap<u32, String>,
    pending: Option<PendingCapture>,
    /// Zero-copy pool, built on first use and reused for the session.
    pool: Option<DmabufPool>,
    /// The last linux-dmabuf offer the compositor made, whatever capture it
    /// went by: `(fourcc, width, height)`.  A probe reads it after a plain
    /// shm capture, because every frame event carries the offer whether or
    /// not one wants the buffer.
    probe_offer: Option<(u32, u32, u32)>,
    /// The y-inversion flag of the last capture.  A probe reads it to know
    /// whether the zero-copy path (which cannot flip) is usable.
    probe_y_invert: bool,
}

impl CaptureState {
    fn fail_pending(&mut self, message: impl Into<String>) {
        if let Some(pending) = self.pending.as_mut() {
            pending.error = Some(message.into());
            pending.complete = true;
        }
    }
}

pub struct WlrCapture {
    event_queue: EventQueue<CaptureState>,
    state: CaptureState,
}

impl WlrCapture {
    pub fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env()
            .map_err(|error| VshotError::WaylandConnection(error.to_string()))?;
        let mut event_queue = connection.new_event_queue::<CaptureState>();
        let qh = event_queue.handle();
        let state = CaptureState::default();
        connection.display().get_registry(&qh, ());
        let mut state = state;
        event_queue
            .roundtrip(&mut state)
            .map_err(|error| VshotError::WaylandProtocol(error.to_string()))?;
        event_queue
            .roundtrip(&mut state)
            .map_err(|error| VshotError::WaylandProtocol(error.to_string()))?;
        if state.shm.is_none() {
            return Err(VshotError::MissingCapability("wl_shm".into()));
        }
        if state.manager.is_none() {
            return Err(VshotError::MissingCapability(
                "zwlr_screencopy_manager_v1".into(),
            ));
        }
        if state.outputs.is_empty() {
            return Err(VshotError::MissingCapability(
                "at least one wl_output".into(),
            ));
        }
        Ok(Self { event_queue, state })
    }

    pub fn capture_output(&mut self, name: &str, cursor: bool) -> Result<Frame> {
        self.capture(name, None, cursor)
    }

    /// Captures one rectangle of an output, given in *output-local logical*
    /// coordinates — the space `capture_output_region` is defined in, and the
    /// space the region picker works in.
    ///
    /// Only that rectangle is rendered into the buffer and converted, so a
    /// scrolled capture of a small region costs a fraction of a full-screen
    /// grab.  That is what lets frames be taken fast enough to keep up with a
    /// page that is still moving.  The compositor clips the rectangle to the
    /// output's extents.
    pub fn capture_region(&mut self, name: &str, region: Rect, cursor: bool) -> Result<Frame> {
        self.capture(name, Some(region), cursor)
    }

    fn capture(&mut self, name: &str, region: Option<Rect>, cursor: bool) -> Result<Frame> {
        let (buffer, y_invert) = self.capture_buffer(name, region, cursor)?;
        let convert_started = Instant::now();
        // A 10-bit buffer on an output the compositor describes as HDR *is* the
        // output's own HDR pixels: the format is the contract, and the pixels
        // are never inspected to decide (see the note in `model::hdr`).  An HDR
        // buffer is tone-mapped down, so the SDR scene, and the annotation
        // overlay drawn from it, show the content as light rather than HDR read
        // as sRGB.
        let frame = if is_10bit_shm(buffer.format) {
            match self.hdr_output_color(name)? {
                Some(color) => {
                    let words = buffer.words(y_invert)?;
                    trace_rgb10(name, buffer.width, buffer.height, buffer.format, &words);
                    HdrFrame::from_rgb10(
                        &words,
                        Size::new(buffer.width, buffer.height),
                        color.transfer,
                        color.primaries,
                        false,
                        color.reference_nits,
                    )?
                    .tone_map_to_srgb(ToneMap::Reinhard)?
                }
                None => buffer.into_frame(y_invert)?,
            }
        } else {
            buffer.into_frame(y_invert)?
        };
        if std::env::var_os("VSHOT_RECORD_DEBUG").is_some() {
            eprintln!(
                "vshot:   capture: pixel-convert {:.1}ms",
                convert_started.elapsed().as_secs_f64() * 1000.0
            );
        }
        Ok(frame)
    }

    /// Runs one `zwlr_screencopy` request and returns the raw buffer the
    /// compositor filled, before it is converted to any pixel layout.
    fn capture_buffer(
        &mut self,
        name: &str,
        region: Option<Rect>,
        cursor: bool,
    ) -> Result<(CaptureBuffer, bool)> {
        if self.state.pending.is_some() {
            return Err(VshotError::WaylandProtocol(
                "a capture is already in progress".into(),
            ));
        }
        let (global_id, output) = self
            .state
            .output_names
            .iter()
            .find(|(_, output_name)| output_name.as_str() == name)
            .and_then(|(global_id, _)| {
                self.state
                    .outputs
                    .get(global_id)
                    .cloned()
                    .map(|output| (*global_id, output))
            })
            .ok_or_else(|| VshotError::IncompleteTopology(format!("unknown output `{name}`")))?;
        let manager =
            self.state.manager.as_ref().cloned().ok_or_else(|| {
                VshotError::MissingCapability("zwlr_screencopy_manager_v1".into())
            })?;
        let qh = self.event_queue.handle();
        let overlay_cursor = if cursor { 1 } else { 0 };
        let frame = match region {
            Some(region) => {
                let (x, y, width, height) = region_arguments(region)?;
                manager.capture_output_region(overlay_cursor, &output, x, y, width, height, &qh, ())
            }
            None => manager.capture_output(overlay_cursor, &output, &qh, ()),
        };
        self.state.pending = Some(PendingCapture {
            _frame: frame,
            buffer: None,
            shm_offer: None,
            want_dmabuf: false,
            dmabuf_slot: None,
            y_invert: false,
            copy_sent: false,
            complete: false,
            error: None,
        });
        let _ = global_id;

        let wait_started = Instant::now();
        self.dispatch_until(Instant::now() + Duration::from_secs(10))?;
        let wait_ms = wait_started.elapsed().as_secs_f64() * 1000.0;
        let pending = self.state.pending.take().ok_or_else(|| {
            VshotError::WaylandProtocol("capture disappeared before completion".into())
        })?;
        if let Some(error) = pending.error {
            return Err(VshotError::WaylandProtocol(error));
        }
        let buffer = pending.buffer.ok_or_else(|| {
            VshotError::WaylandProtocol("screencopy completed without a buffer".into())
        })?;
        if std::env::var_os("VSHOT_RECORD_DEBUG").is_some() {
            eprintln!("vshot:   capture: compositor+shm-copy {wait_ms:.1}ms");
        }
        Ok((buffer, pending.y_invert))
    }

    /// Captures one output straight into a dma-buf: the compositor renders
    /// into a buffer the encoder can import, and no pixel passes through
    /// the CPU.  This is the fast path the recorder runs `record monitor`
    /// through; the shm `capture_output` stays as the compatibility path.
    ///
    /// The buffer pool is built on the first call, from the compositor's
    /// own linux-dmabuf offer — that is where the fourcc and the size come
    /// from.  Later calls rotate through the pool.
    ///
    /// Fails — and the caller falls back to `capture_output` — when the
    /// session lacks linux-dmabuf, the compositor offers no dma-buf, the
    /// buffer's shape does not match the pool, or the frame needs a
    /// y-flip the zero-copy chain cannot apply.
    pub fn capture_output_dmabuf(&mut self, name: &str, cursor: bool) -> Result<DmabufFrame> {
        self.capture_dmabuf(name, None, cursor)
    }

    /// The zero-copy variant of `capture_region`: the compositor renders just
    /// that rectangle into a dma-buf, which is what makes a small region both
    /// cheap to copy and free of a CPU round trip.
    pub fn capture_region_dmabuf(
        &mut self,
        name: &str,
        region: Rect,
        cursor: bool,
    ) -> Result<DmabufFrame> {
        self.capture_dmabuf(name, Some(region), cursor)
    }

    /// Captures one output as HDR content, when the session can hand it over.
    ///
    /// Two routes carry HDR pixels.  The first is the buffer-over-`wl_shm`
    /// route: when the compositor keeps HDR in the screencopy buffer — a
    /// compositor patched so that `screencopy_hdr` fills the output's 10-bit
    /// format with the pixels it is showing — those pixels arrive over shm, and
    /// a 10-bit buffer on an HDR-described output is read as that output's own
    /// encoding.  It needs no dma-buf, so it is the route that works on a driver
    /// (NVIDIA, here) that cannot hand a 10-bit dma-buf over at all.  The second
    /// is the `linux-dmabuf` route, for a compositor that advertises the
    /// high-bit-depth format there instead, decoded as HDR10 (BT.2020 primaries,
    /// PQ transfer).
    ///
    /// `Ok(None)` means this output offers no HDR by either route — a compositor
    /// that maps the capture to sRGB, or an output the compositor does not call
    /// HDR — and the ordinary SDR path stands alone.
    pub fn capture_output_hdr(&mut self, name: &str, cursor: bool) -> Result<Option<HdrFrame>> {
        let debug = std::env::var_os("VSHOT_HDR_DEBUG").is_some();

        // The shm route: a 10-bit buffer on an output the compositor describes
        // as HDR is that output's own HDR pixels — the format is the contract,
        // the pixels are never inspected (see the note in `model::hdr`).  An
        // 8-bit offer means the compositor mapped the capture to sRGB, and the
        // caller keeps its own SDR frame.
        let color = self.hdr_output_color(name)?;
        if let Some(color) = color {
            let (buffer, y_invert) = self.capture_buffer(name, None, cursor)?;
            let format = buffer.format;
            if is_10bit_shm(format) {
                let words = buffer.words(y_invert)?;
                trace_rgb10(name, buffer.width, buffer.height, format, &words);
                let hdr = HdrFrame::from_rgb10(
                    &words,
                    Size::new(buffer.width, buffer.height),
                    color.transfer,
                    color.primaries,
                    false,
                    color.reference_nits,
                )?;
                if debug {
                    eprintln!(
                        "vshot: hdr shm {name}: peak={} is_hdr={}",
                        hdr.peak(),
                        hdr.is_hdr()
                    );
                }
                return Ok(Some(hdr));
            }
        }

        let probe = self.probe_dmabuf_offer_region(name, None);
        if std::env::var_os("VSHOT_HDR_DEBUG").is_some() {
            eprintln!("vshot: hdr probe {name}: {probe:?}");
        }
        let (fourcc, width, height, y_invert) = match probe {
            Ok(offer) => offer,
            Err(VshotError::MissingCapability(_)) | Err(VshotError::UnsupportedOutput(_)) => {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        if y_invert || !is_hdr_fourcc(fourcc) {
            if std::env::var_os("VSHOT_HDR_DEBUG").is_some() {
                eprintln!(
                    "vshot: hdr skip {name}: fourcc=0x{fourcc:08x} y_invert={y_invert} hdr={}",
                    is_hdr_fourcc(fourcc)
                );
            }
            return Ok(None);
        }
        // A pool built earlier for a different shape or format cannot serve
        // this capture; HDR outputs are walked one after another here, so the
        // pool has to follow each output's own offer.
        if self.state.pool.as_ref().is_some_and(|pool| {
            pool.fourcc != fourcc || pool.width != width || pool.height != height
        }) {
            self.state.pool = None;
        }
        self.build_dmabuf_pool(width, height, fourcc)?;
        let frame = self.capture_output_dmabuf(name, cursor)?;
        let words = frame.read_rgb10()?;
        let hdr = HdrFrame::from_rgb10(
            &words,
            Size::new(frame.width, frame.height),
            Transfer::Pq,
            Primaries::Bt2020,
            // The `A…` forms carry alpha in the top two bits; the `X…` forms
            // pad them, and a screenshot is opaque either way.
            matches!(
                frame.fourcc,
                DRM_FORMAT_ARGB2101010 | DRM_FORMAT_ABGR2101010
            ),
            // This route decodes plain HDR10 and may run for an output the
            // compositor does not describe, so it falls back to the HDR10
            // reference white (BT.2408) when there is no description.
            color.map_or(REFERENCE_WHITE_NITS, |color| color.reference_nits),
        )?;
        if std::env::var_os("VSHOT_HDR_DEBUG").is_some() {
            eprintln!(
                "vshot: hdr decoded {name}: {}x{} fourcc=0x{:08x} peak={} is_hdr={}",
                frame.width,
                frame.height,
                frame.fourcc,
                hdr.peak(),
                hdr.is_hdr()
            );
        }
        Ok(Some(hdr))
    }

    /// The colour description of an output, but only when the compositor calls
    /// it HDR — a PQ or HLG transfer.  `None` means the output is SDR, or the
    /// compositor does not describe it, so a 10-bit buffer of its pixels is not
    /// HDR content and must not be read as if it were.
    fn hdr_output_color(&mut self, name: &str) -> Result<Option<OutputColor>> {
        Ok(self.output_color(name)?.filter(OutputColor::is_hdr))
    }

    /// The colour properties the compositor describes for one output, over
    /// `wp_color_manager_v1`.
    ///
    /// This is the Wayland reading of the display facts Starward reads on
    /// Windows: the reference luminance is the SDR white level — the level above
    /// which a capture really is HDR — and whether the output is in HDR is its
    /// transfer function, not the capture buffer's depth.  `None` means the
    /// compositor does not describe its outputs, or did not answer in time, and
    /// leaves the capture side to its own defaults.
    pub fn output_color(&mut self, name: &str) -> Result<Option<OutputColor>> {
        let Some(manager) = self.state.color_manager.clone() else {
            return Ok(None);
        };
        let output = self.output_by_name(name)?;
        let qh = self.event_queue.handle();
        let cm_output = manager.get_output(&output, &qh, ());
        let description = cm_output.get_image_description(&qh, ());
        self.state.color_query = Some(ColorQuery {
            _output: Some(cm_output),
            description: Some(description),
            ..ColorQuery::default()
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let answered = self.dispatch_while(deadline, |state| {
            state
                .color_query
                .as_ref()
                .is_some_and(|query| query.ready || query.failed)
        })?;
        let ready = answered
            && self
                .state
                .color_query
                .as_ref()
                .is_some_and(|query| query.ready);
        if !ready {
            self.state.color_query = None;
            return Ok(None);
        }

        // The description is ready; ask it for its information and collect it.
        let info = {
            let query = self.state.color_query.as_mut().expect("checked above");
            let description = query.description.clone().expect("a ready description");
            let info = description.get_information(&qh, ());
            query.info = Some(info.clone());
            info
        };
        let _ = info;
        let collected = self.dispatch_while(deadline, |state| {
            state
                .color_query
                .as_ref()
                .is_some_and(|query| query.done || query.failed)
        })?;
        let query = self.state.color_query.take().unwrap_or_default();
        if !collected || query.failed {
            return Ok(None);
        }
        Ok(query.into_output_color())
    }

    /// The `wl_output` for one output name, as the registry bound it.
    fn output_by_name(&self, name: &str) -> Result<wl_output::WlOutput> {
        self.state
            .output_names
            .iter()
            .find(|(_, output_name)| output_name.as_str() == name)
            .and_then(|(global_id, _)| self.state.outputs.get(global_id))
            .cloned()
            .ok_or_else(|| VshotError::IncompleteTopology(format!("unknown output `{name}`")))
    }

    /// One dma-buf capture, of the whole output or of one rectangle inside it.
    /// The pool is built for the capture's own shape, so a region capture
    /// must have been probed with `probe_dmabuf_offer_region` first — the
    /// offer's size is what the pool was built for.
    fn capture_dmabuf(
        &mut self,
        name: &str,
        region: Option<Rect>,
        cursor: bool,
    ) -> Result<DmabufFrame> {
        if self.state.pending.is_some() {
            return Err(VshotError::WaylandProtocol(
                "a capture is already in progress".into(),
            ));
        }
        if !super::dmabuf::available() {
            return Err(VshotError::MissingCapability(format!(
                "zero-copy capture needs libgbm: {}",
                super::dmabuf::load_error()
            )));
        }
        if self.state.manager_version < 3 {
            return Err(VshotError::MissingCapability(
                "screencopy version 3 (linux-dmabuf buffers)".into(),
            ));
        }
        let (global_id, output) = self
            .state
            .output_names
            .iter()
            .find(|(_, output_name)| output_name.as_str() == name)
            .and_then(|(global_id, _)| {
                self.state
                    .outputs
                    .get(global_id)
                    .cloned()
                    .map(|output| (*global_id, output))
            })
            .ok_or_else(|| VshotError::IncompleteTopology(format!("unknown output `{name}`")))?;
        let manager =
            self.state.manager.as_ref().cloned().ok_or_else(|| {
                VshotError::MissingCapability("zwlr_screencopy_manager_v1".into())
            })?;
        let qh = self.event_queue.handle();
        let overlay_cursor = if cursor { 1 } else { 0 };
        let frame = match region {
            Some(region) => {
                let (x, y, width, height) = region_arguments(region)?;
                manager.capture_output_region(overlay_cursor, &output, x, y, width, height, &qh, ())
            }
            None => manager.capture_output(overlay_cursor, &output, &qh, ()),
        };
        self.state.pending = Some(PendingCapture {
            _frame: frame,
            buffer: None,
            shm_offer: None,
            want_dmabuf: true,
            dmabuf_slot: None,
            y_invert: false,
            copy_sent: false,
            complete: false,
            error: None,
        });
        let _ = global_id;

        let wait_started = Instant::now();
        self.dispatch_until(Instant::now() + Duration::from_secs(10))?;
        let wait_ms = wait_started.elapsed().as_secs_f64() * 1000.0;
        let pending = self.state.pending.take().ok_or_else(|| {
            VshotError::WaylandProtocol("capture disappeared before completion".into())
        })?;
        if let Some(error) = pending.error {
            return Err(VshotError::WaylandProtocol(error));
        }
        if pending.y_invert {
            // The GPU path cannot flip; the shm path can.  Say so rather
            // than reading the frame upside down.
            return Err(VshotError::UnsupportedOutput(
                "the compositor renders this output y-inverted, which the zero-copy path cannot \
                 capture; the software path handles it"
                    .into(),
            ));
        }
        let slot = pending.dmabuf_slot.ok_or_else(|| {
            VshotError::UnsupportedOutput(
                "the compositor offered no linux-dmabuf buffer for this capture".into(),
            )
        })?;
        let pool = self.state.pool.as_ref().ok_or_else(|| {
            VshotError::WaylandProtocol("the dma-buf pool disappeared mid-capture".into())
        })?;
        let slot_ref = pool.slots.get(slot).ok_or_else(|| {
            VshotError::WaylandProtocol("the dma-buf pool slot vanished mid-capture".into())
        })?;
        let gbm = &slot_ref.gbm;
        if std::env::var_os("VSHOT_RECORD_DEBUG").is_some() {
            eprintln!(
                "vshot:   capture: compositor+dmabuf-copy {wait_ms:.1}ms (zero copy, no \
                 conversion)"
            );
        }
        Ok(DmabufFrame {
            fd: gbm.fd(),
            fourcc: gbm.fourcc(),
            modifier: gbm.modifier(),
            offset: gbm.offset(),
            stride: gbm.stride(),
            width: gbm.width(),
            height: gbm.height(),
        })
    }

    /// Builds the zero-copy buffer pool for one output shape.  Called
    /// between captures (never mid-capture); the recorder does it once the
    /// first probe of the output has established the fourcc and size from
    /// the compositor's own offer.
    pub fn build_dmabuf_pool(&mut self, width: u32, height: u32, fourcc: u32) -> Result<()> {
        if self.state.pool.is_some() {
            return Ok(());
        }
        let dmabuf = self
            .state
            .dmabuf
            .as_ref()
            .cloned()
            .ok_or_else(|| VshotError::MissingCapability("zwp_linux_dmabuf_v1".into()))?;
        let qh = self.event_queue.handle();
        let mut slots = Vec::with_capacity(DMABUF_POOL_SLOTS);
        for _ in 0..DMABUF_POOL_SLOTS {
            let gbm = GbmBuffer::create(width, height, fourcc)?;
            if std::env::var_os("VSHOT_HDR_DEBUG").is_some() {
                eprintln!(
                    "vshot: hdr pool {width}x{height} fourcc=0x{fourcc:08x} modifier=0x{:x} stride={} offset={}",
                    gbm.modifier(),
                    gbm.stride(),
                    gbm.offset()
                );
            }
            let modifier = gbm.modifier();
            let params = dmabuf.create_params(&qh, ());
            let borrowed = unsafe { BorrowedFd::borrow_raw(gbm.fd()) };
            params.add(
                borrowed,
                0,
                gbm.offset(),
                gbm.stride(),
                (modifier >> 32) as u32,
                (modifier & 0xffff_ffff) as u32,
            );
            let wl_buffer = params.create_immed(
                i32::try_from(width).map_err(|_| {
                    VshotError::WaylandProtocol("capture width is too large".into())
                })?,
                i32::try_from(height).map_err(|_| {
                    VshotError::WaylandProtocol("capture height is too large".into())
                })?,
                fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &qh,
                (),
            );
            params.destroy();
            slots.push(DmabufSlot { gbm, wl_buffer });
        }
        self.state.pool = Some(DmabufPool {
            slots,
            next: 0,
            width,
            height,
            fourcc,
        });
        Ok(())
    }

    /// The dma-buf format/size the compositor offered for an output, read
    /// from a plain shm capture: every screencopy frame event carries the
    /// linux-dmabuf offer whether or not a client wants the buffer, so the
    /// offer can be sampled without disturbing the compatibility capture.
    /// The fourcc comes back as the DRM fourcc the pool must use.
    ///
    /// `region` narrows the question to one rectangle of the output.  A
    /// region capture's offer is the region's own pixel size — that is what a
    /// pool for it must be built for, and what `record region` probes before
    /// its loop starts.
    pub fn probe_dmabuf_offer_region(
        &mut self,
        name: &str,
        region: Option<Rect>,
    ) -> Result<(u32, u32, u32, bool)> {
        if self.state.manager_version < 3 {
            return Err(VshotError::MissingCapability(
                "screencopy version 3 (linux-dmabuf buffers)".into(),
            ));
        }
        if self.state.dmabuf.is_none() {
            return Err(VshotError::MissingCapability("zwp_linux_dmabuf_v1".into()));
        }
        self.state.probe_offer = None;
        let _frame = self.capture(name, region, false)?;
        let y_invert = self.state.probe_y_invert;
        self.state
            .probe_offer
            .take()
            .map(|(fourcc, width, height)| (fourcc, width, height, y_invert))
            .ok_or_else(|| {
                VshotError::UnsupportedOutput(
                    "the compositor offered no linux-dmabuf buffer for this output".into(),
                )
            })
    }

    fn dispatch_until(&mut self, deadline: Instant) -> Result<()> {
        let complete = self.dispatch_while(deadline, |state| {
            state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.complete)
        })?;
        if !complete {
            self.state.pending.take();
            return Err(VshotError::CaptureTimeout);
        }
        Ok(())
    }

    /// Dispatches until `done` holds or `deadline` passes, returning whether it
    /// held.  A timeout is not an error here: a query that never gets its answer
    /// simply has none (see [`WlrCapture::output_color`]), so `false` is told
    /// apart from a protocol failure rather than turned into one.
    fn dispatch_while<F>(&mut self, deadline: Instant, done: F) -> Result<bool>
    where
        F: Fn(&CaptureState) -> bool,
    {
        loop {
            self.event_queue
                .dispatch_pending(&mut self.state)
                .map_err(|error| VshotError::WaylandProtocol(error.to_string()))?;
            if done(&self.state) {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            self.event_queue
                .flush()
                .map_err(|error| VshotError::WaylandProtocol(error.to_string()))?;
            let Some(read_guard) = self.event_queue.prepare_read() else {
                continue;
            };
            let fd = read_guard.connection_fd();
            let mut poll_fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN | rustix::event::PollFlags::ERR,
            )];
            let timeout = rustix::event::Timespec::try_from(remaining).map_err(|_| {
                VshotError::WaylandProtocol("capture timeout is out of range".into())
            })?;
            let ready = match rustix::event::poll(&mut poll_fds, Some(&timeout)) {
                Ok(ready) => ready,
                // A signal — the stop handler's, most of all — interrupts the
                // poll; that is not a protocol failure, and returning from
                // here with a timeout error would turn a clean stop into a
                // dropped frame.
                Err(rustix::io::Errno::INTR) => {
                    drop(read_guard);
                    continue;
                }
                Err(error) => {
                    return Err(VshotError::WaylandProtocol(format!(
                        "failed to poll Wayland connection: {error}"
                    )));
                }
            };
            if ready == 0 {
                drop(read_guard);
                return Ok(false);
            }
            read_guard
                .read()
                .map_err(|error| VshotError::WaylandProtocol(error.to_string()))?;
        }
    }
}

/// The four integers `capture_output_region` takes, in its own order, checked:
/// a rectangle that will not fit them is not something to send a compositor.
fn region_arguments(region: Rect) -> Result<(i32, i32, i32, i32)> {
    let width = i32::try_from(region.size.width).map_err(|_| {
        VshotError::WaylandProtocol("the capture region is too wide to ask for".into())
    })?;
    let height = i32::try_from(region.size.height).map_err(|_| {
        VshotError::WaylandProtocol("the capture region is too tall to ask for".into())
    })?;
    Ok((region.origin.x, region.origin.y, width, height))
}

/// The numeric code of a `WEnum` argument, whether or not this build names it.
/// The protocol's named values are stable, so the numbers are what the colour
/// mapping keys on (ST 2084 PQ is 11, HLG 13, BT.2020 primaries 6).
macro_rules! named {
    ($value:expr) => {
        match $value {
            WEnum::Unknown(code) => code,
            WEnum::Value(value) => value as u32,
        }
    };
}

impl Dispatch<wl_registry::WlRegistry, ()> for CaptureState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" if state.shm.is_none() => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_screencopy_manager_v1" if state.manager.is_none() => {
                    let bind_version = version.min(3);
                    state.manager_version = bind_version;
                    state.manager = Some(registry.bind(name, bind_version, qh, ()));
                }
                "zwp_linux_dmabuf_v1" if state.dmabuf.is_none() => {
                    // Version 3 is what screencopy's dmabuf event needs;
                    // binding lower is still fine for the shm path.
                    let bind_version = version.min(3);
                    state.dmabuf_version = bind_version;
                    state.dmabuf = Some(registry.bind(name, bind_version, qh, ()));
                }
                "wp_color_manager_v1" if state.color_manager.is_none() => {
                    // Bound for its side effect on a compositor that keys HDR
                    // capture on it, not for its events (see
                    // `CaptureState::color_manager`).
                    state.color_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_output" => {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(
                        name,
                        version.min(4),
                        qh,
                        OutputUserData { global_id: name },
                    );
                    state.outputs.insert(name, output);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, OutputUserData> for CaptureState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        data: &OutputUserData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.output_names.insert(data.global_id, name);
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        _: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_color_manager_v1::WpColorManagerV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &wp_color_manager_v1::WpColorManagerV1,
        _: wp_color_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_color_management_output_v1::WpColorManagementOutputV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &wp_color_management_output_v1::WpColorManagementOutputV1,
        _: wp_color_management_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_image_description_v1::WpImageDescriptionV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &wp_image_description_v1::WpImageDescriptionV1,
        event: wp_image_description_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(query) = state.color_query.as_mut() else {
            return;
        };
        match event {
            wp_image_description_v1::Event::Ready { .. }
            | wp_image_description_v1::Event::Ready2 { .. } => query.ready = true,
            wp_image_description_v1::Event::Failed { .. } => query.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<wp_image_description_info_v1::WpImageDescriptionInfoV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &wp_image_description_info_v1::WpImageDescriptionInfoV1,
        event: wp_image_description_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(query) = state.color_query.as_mut() else {
            return;
        };
        match event {
            wp_image_description_info_v1::Event::TfNamed { tf } => {
                query.transfer = Some(named!(tf))
            }
            wp_image_description_info_v1::Event::PrimariesNamed { primaries } => {
                query.primaries = Some(named!(primaries));
            }
            wp_image_description_info_v1::Event::Luminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                query.min_nits = Some(min_lum as f32 / 10_000.0);
                query.max_nits = Some(max_lum as f32);
                query.reference_nits = Some(reference_lum as f32);
            }
            wp_image_description_info_v1::Event::TargetLuminance { max_lum, .. } => {
                query.target_max_nits = Some(max_lum as f32);
            }
            wp_image_description_info_v1::Event::TargetMaxCll { max_cll } => {
                query.max_cll = Some(max_cll);
            }
            wp_image_description_info_v1::Event::Done => query.done = true,
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        _: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        _: zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let format = match format {
                    WEnum::Value(format) => format,
                    WEnum::Unknown(value) => {
                        state.fail_pending(format!("unknown screencopy wl_shm format {value}"));
                        return;
                    }
                };
                let copy_immediately = state.manager_version < 3;
                let Some(pending) = state.pending.as_mut() else {
                    return;
                };
                if pending.shm_offer.is_some() {
                    pending.error = Some("screencopy sent more than one SHM buffer".into());
                    pending.complete = true;
                    return;
                }
                pending.shm_offer = Some(ShmOffer {
                    format,
                    width,
                    height,
                    stride,
                });
                // A pre-v3 compositor has no buffer_done: the copy has to
                // go out with the offer, so the shm buffer is built now.
                if copy_immediately && !pending.want_dmabuf {
                    let Some(shm) = state.shm.as_ref().cloned() else {
                        state.fail_pending("wl_shm disappeared during capture");
                        return;
                    };
                    match CaptureBuffer::new(&shm, width, height, stride, format, qh) {
                        Ok(buffer) => {
                            frame.copy(&buffer.buffer);
                            pending.copy_sent = true;
                            pending.buffer = Some(buffer);
                        }
                        Err(error) => {
                            pending.error = Some(error.to_string());
                            pending.complete = true;
                        }
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                // Every frame event carries the offer, wanted or not; keep
                // the newest for `probe_dmabuf_offer`.
                state.probe_offer = Some((format, width, height));
                let Some(pending) = state.pending.as_mut() else {
                    return;
                };
                if !pending.want_dmabuf {
                    return;
                }
                // The offer must match exactly what the pool was built
                // for: the same fourcc and size.  A mismatch (a scale
                // change, a rotated buffer) makes this capture fall back
                // to the shm path.
                if let Some(pool) = state.pool.as_ref() {
                    if pool.fourcc == format && pool.width == width && pool.height == height {
                        pending.dmabuf_slot = Some(pool.next);
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                // The zero-copy branch first: bind the pool's wl_buffer and
                // copy.  Nothing shm was allocated for this capture.
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.dmabuf_slot.is_some())
                {
                    let Some(pending) = state.pending.as_mut() else {
                        return;
                    };
                    if let Some(pool) = state.pool.as_mut() {
                        let slot = pending.dmabuf_slot.expect("checked above");
                        let slot_ref = &pool.slots[slot];
                        frame.copy(&slot_ref.wl_buffer);
                        pending.copy_sent = true;
                        pool.next = (slot + 1) % pool.slots.len();
                        return;
                    }
                }
                // The shm branch: allocate the buffer now (lazily) and copy.
                let Some(pending) = state.pending.as_mut() else {
                    return;
                };
                if pending.copy_sent {
                    return;
                }
                let Some(offer) = pending.shm_offer else {
                    pending.error =
                        Some("screencopy sent buffer_done without a usable buffer".into());
                    pending.complete = true;
                    return;
                };
                let Some(shm) = state.shm.as_ref().cloned() else {
                    pending.error = Some("wl_shm disappeared during capture".into());
                    pending.complete = true;
                    return;
                };
                match CaptureBuffer::new(
                    &shm,
                    offer.width,
                    offer.height,
                    offer.stride,
                    offer.format,
                    qh,
                ) {
                    Ok(buffer) => {
                        frame.copy(&buffer.buffer);
                        pending.copy_sent = true;
                        pending.buffer = Some(buffer);
                    }
                    Err(error) => {
                        pending.error = Some(error.to_string());
                        pending.complete = true;
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: WEnum::Value(flags),
            } => {
                state.probe_y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                if let Some(pending) = state.pending.as_mut() {
                    pending.y_invert = state.probe_y_invert;
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                if let Some(pending) = state.pending.as_mut() {
                    pending.complete = true;
                    frame.destroy();
                }
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.fail_pending("compositor failed to copy the requested output");
                frame.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_xrgb_with_padding_and_y_inversion() {
        let map = vec![
            3, 2, 1, 99, 0, 0, 0, 0, // source row 0
            6, 5, 4, 88, 0, 0, 0, 0, // source row 1
        ];
        let frame = convert_shm_pixels(&map, 1, 2, 8, wl_shm::Format::Xrgb8888, true).unwrap();
        assert_eq!(frame.size(), Size::new(1, 2));
        assert_eq!(
            frame.pixel(crate::geometry::Point::new(0, 0)),
            Some([4, 5, 6, 255])
        );
        assert_eq!(
            frame.pixel(crate::geometry::Point::new(0, 1)),
            Some([1, 2, 3, 255])
        );
    }

    #[test]
    fn converts_argb_alpha_without_reading_padding() {
        let map = vec![30, 20, 10, 77, 255, 255, 255, 255];
        let frame = convert_shm_pixels(&map, 1, 1, 8, wl_shm::Format::Argb8888, false).unwrap();
        assert_eq!(
            frame.pixel(crate::geometry::Point::new(0, 0)),
            Some([10, 20, 30, 77])
        );
    }

    /// The `…bgr…` layouts are already RGBA in memory, so only the padding has
    /// to be made opaque.  wlroots with the pixman renderer — a headless or
    /// software-rendered session — hands over XBGR8888, and reading it as the
    /// BGRA of the `…rgb…` names would swap every red and blue in the frame.
    #[test]
    fn converts_xbgr_without_swapping_the_colour_bytes() {
        let map = vec![1, 2, 3, 99];
        let frame = convert_shm_pixels(&map, 1, 1, 4, wl_shm::Format::Xbgr8888, false).unwrap();
        assert_eq!(
            frame.pixel(crate::geometry::Point::new(0, 0)),
            Some([1, 2, 3, 255])
        );
    }

    /// ABGR8888 keeps its alpha, and its byte order, the way ARGB8888 keeps
    /// them with the colour bytes swapped.
    #[test]
    fn converts_abgr_alpha_without_swapping_the_colour_bytes() {
        let map = vec![1, 2, 3, 77];
        let frame = convert_shm_pixels(&map, 1, 1, 4, wl_shm::Format::Abgr8888, false).unwrap();
        assert_eq!(
            frame.pixel(crate::geometry::Point::new(0, 0)),
            Some([1, 2, 3, 77])
        );
    }

    /// A 10-bit capture is a 10-bit sRGB image, not HDR: the `…rgb…` packing
    /// puts red in bits 20..30, and a full-scale red comes out as a full red.
    #[test]
    fn reads_a_10bit_xrgb_buffer_as_srgb_bytes() {
        let word: u32 = 1023 << 20;
        let map = word.to_le_bytes().to_vec();
        let frame = convert_shm_pixels(&map, 1, 1, 4, wl_shm::Format::Xrgb2101010, false).unwrap();
        assert_eq!(
            frame.pixel(crate::geometry::Point::new(0, 0)),
            Some([255, 0, 0, 255])
        );
    }

    /// The `…bgr…` packing stores the red and blue fields the other way round,
    /// so the same word is a red pixel under XBGR2101010 only if the swap is
    /// applied.
    #[test]
    fn reads_a_10bit_xbgr_buffer_swapping_red_and_blue() {
        let word: u32 = 1023; // red in the low field of a `…bgr…` word
        let map = word.to_le_bytes().to_vec();
        let frame = convert_shm_pixels(&map, 1, 1, 4, wl_shm::Format::Xbgr2101010, false).unwrap();
        assert_eq!(
            frame.pixel(crate::geometry::Point::new(0, 0)),
            Some([255, 0, 0, 255])
        );
    }

    #[test]
    fn a_ten_bit_code_rounds_to_the_nearest_byte() {
        assert_eq!(ten_to_eight(0), 0);
        assert_eq!(ten_to_eight(1023), 255);
        assert_eq!(ten_to_eight(512), 128);
    }

    /// A PQ / BT.2020 output description maps to the HDR reading, and an sRGB
    /// one does not — the colour codes the protocol names are what the mapping
    /// keys on.
    #[test]
    fn an_output_description_maps_its_transfer_and_primaries() {
        let hdr = ColorQuery {
            transfer: Some(11),
            primaries: Some(6),
            reference_nits: Some(203.0),
            max_nits: Some(417.0),
            ..ColorQuery::default()
        }
        .into_output_color()
        .unwrap();
        assert!(hdr.is_hdr());
        assert_eq!(hdr.transfer, Transfer::Pq);
        assert_eq!(hdr.primaries, Primaries::Bt2020);
        assert_eq!(hdr.reference_nits, 203.0);
        assert_eq!(hdr.max_nits, 417.0);

        let sdr = ColorQuery {
            transfer: Some(9),
            primaries: Some(1),
            reference_nits: Some(80.0),
            ..ColorQuery::default()
        }
        .into_output_color()
        .unwrap();
        assert!(!sdr.is_hdr());
        assert_eq!(sdr.transfer, Transfer::Srgb);
        assert_eq!(sdr.primaries, Primaries::Bt709);
    }

    /// A description that never stated its reference white cannot be read.
    #[test]
    fn an_output_description_without_luminances_is_not_read() {
        let query = ColorQuery {
            transfer: Some(11),
            primaries: Some(6),
            ..ColorQuery::default()
        };
        assert!(query.into_output_color().is_none());
    }
}
