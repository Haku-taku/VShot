// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

use std::collections::HashMap;

use wayland_client::protocol::wl_output;
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1;

use crate::error::{Result, VshotError};
use crate::geometry::{Point, Rect, Size};

/// `DRM_FORMAT_MOD_INVALID`, the modifier a compositor offers when it can take a
/// buffer in any layout the allocation side can produce.
pub(crate) const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// `DRM_FORMAT_MOD_LINEAR`.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Whether `modifier` is the NVIDIA block-linear family, and how tall its block
/// is.  The framing is NVIDIA's own fourcc-modifier encoding, and the height is
/// the low nibble of its value.
fn nvidia_block_height(modifier: u64) -> Option<u64> {
    const NVIDIA_VENDOR: u64 = 0x03;
    (modifier >> 56 == NVIDIA_VENDOR).then_some(modifier & 0xf)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputInfo {
    pub global_id: u32,
    pub name: String,
    pub geometry: Rect,
    pub pixel_size: Size,
    pub scale: u32,
    pub transform: wl_output::Transform,
}

impl OutputInfo {
    pub fn is_supported(&self) -> bool {
        let expected_width = self.geometry.size.width.checked_mul(self.scale);
        let expected_height = self.geometry.size.height.checked_mul(self.scale);
        self.scale > 0
            && self.transform == wl_output::Transform::Normal
            && !self.geometry.is_empty()
            && expected_width == Some(self.pixel_size.width)
            && expected_height == Some(self.pixel_size.height)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OutputData {
    pub(crate) name: Option<String>,
    pub(crate) logical_position: Option<Point>,
    pub(crate) logical_size: Option<Size>,
    pub(crate) pixel_size: Option<Size>,
    pub(crate) scale: u32,
    pub(crate) transform: Option<wl_output::Transform>,
    pub(crate) wl_done: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputUserData {
    pub(crate) global_id: u32,
}

#[derive(Debug, Default)]
pub(crate) struct TopologyState {
    pub(crate) compositor: Option<wayland_client::protocol::wl_compositor::WlCompositor>,
    pub(crate) shm: Option<wayland_client::protocol::wl_shm::WlShm>,
    pub(crate) shm_argb8888: bool,
    pub(crate) shm_xrgb8888: bool,
    // Ten-bit shm formats, what an HDR backdrop surface is written in.
    pub(crate) shm_argb2101010: bool,
    pub(crate) shm_xrgb2101010: bool,
    // `wp_color_manager_v1`, when the compositor offers it: the backdrop sets
    // an output's own image description on its surface through this.
    pub(crate) color_manager: Option<wayland_protocols::wp::color_management::v1::client::wp_color_manager_v1::WpColorManagerV1>,
    // `zwp_linux_dmabuf_v1`, and the formats it will take: the pinned HDR image
    // is a half-float dma-buf, and only a modifier the compositor lists can be
    // imported.
    pub(crate) dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    pub(crate) dmabuf_formats: HashMap<u32, Vec<u64>>,
    pub(crate) layer_shell: Option<wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    pub(crate) cursor_shape_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    pub(crate) cursor_shape_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    pub(crate) cursor_enter_serial: Option<u32>,
    pub(crate) cursor_shape: Option<wp_cursor_shape_device_v1::Shape>,
    pub(crate) xdg_output_manager: Option<wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    pub(crate) seats: Vec<wayland_client::protocol::wl_seat::WlSeat>,
    pub(crate) output_proxies: HashMap<u32, wayland_client::protocol::wl_output::WlOutput>,
    pub(crate) xdg_outputs: HashMap<u32, wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::ZxdgOutputV1>,
    pub(crate) outputs: HashMap<u32, OutputData>,
    pub(crate) pointer: Option<wayland_client::protocol::wl_pointer::WlPointer>,
    pub(crate) keyboard: Option<wayland_client::protocol::wl_keyboard::WlKeyboard>,
    pub(crate) pointer_capability: bool,
    pub(crate) keyboard_capability: bool,
    pub(crate) topology_changed: bool,
}

impl TopologyState {
    pub(crate) fn ensure_xdg_outputs<State>(&mut self, qh: &wayland_client::QueueHandle<State>)
    where
        State: wayland_client::Dispatch<
                wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::ZxdgOutputV1,
                u32,
            > + 'static,
    {
        let Some(manager) = self.xdg_output_manager.as_ref().cloned() else {
            return;
        };
        let output_ids = self.output_proxies.keys().copied().collect::<Vec<_>>();
        for global_id in output_ids {
            if self.xdg_outputs.contains_key(&global_id) {
                continue;
            }
            if let Some(output) = self.output_proxies.get(&global_id) {
                let xdg_output = manager.get_xdg_output(output, qh, global_id);
                self.xdg_outputs.insert(global_id, xdg_output);
            }
        }
    }

    pub(crate) fn shm_format(&self) -> Option<wayland_client::protocol::wl_shm::Format> {
        if self.shm_argb8888 {
            Some(wayland_client::protocol::wl_shm::Format::Argb8888)
        } else if self.shm_xrgb8888 {
            Some(wayland_client::protocol::wl_shm::Format::Xrgb8888)
        } else {
            None
        }
    }

    /// The ten-bit shm format a backdrop surface is written in.  Ten bits is
    /// what a PQ code wants: it is the depth an HDR capture arrives at, and
    /// rounding it to eight would band the very gradients the backdrop exists
    /// to show.  `None` means the compositor offers none, and the backdrop
    /// falls back to the SDR overlay.
    ///
    /// Only the `…rgb…`-ordered forms are usable: the backdrop's words are
    /// packed ``XRGB2101010``-wise (see [`crate::model::HdrFrame::to_rgb10_pq`]),
    /// and the buffer is written straight from them.  A compositor that offers
    /// only the `…bgr…` pair therefore falls back to the SDR overlay; the
    /// capture side reads all four, but it can swap the fields as it decodes.
    pub(crate) fn shm_format_10bit(&self) -> Option<wayland_client::protocol::wl_shm::Format> {
        if self.shm_argb2101010 {
            Some(wayland_client::protocol::wl_shm::Format::Argb2101010)
        } else if self.shm_xrgb2101010 {
            Some(wayland_client::protocol::wl_shm::Format::Xrgb2101010)
        } else {
            None
        }
    }

    /// The modifiers to offer GBM for `fourcc`, most preferred first, or `None`
    /// when the compositor will not take that format at all.
    ///
    /// Order matters because `gbm_bo_create_with_modifiers` takes the first
    /// modifier of the list it can allocate.  In principle the layout of a
    /// half-float picture should not change a single pixel of what the
    /// compositor shows, but measurably it does: on NVIDIA, the block-linear
    /// layouts of different heights come back with slightly different light, and
    /// the tallest block is the one that matched a software control exactly.  So
    /// the block-linear family is offered tallest block first, whichever order
    /// the compositor happened to list it in; everything else keeps the order it
    /// was given, and `INVALID` -- "any layout you like" -- goes last because a
    /// concrete layout is a stronger promise.
    pub(crate) fn dmabuf_modifier_order(&self, fourcc: u32) -> Option<Vec<u64>> {
        let modifiers = self.dmabuf_formats.get(&fourcc)?;
        let mut ordered = modifiers.clone();
        ordered.sort_by_key(|modifier| match *modifier {
            DRM_FORMAT_MOD_LINEAR | DRM_FORMAT_MOD_INVALID => (2_u8, 0_u64),
            modifier => match nvidia_block_height(modifier) {
                Some(height) => (0, u64::MAX - height),
                None => (1, modifier),
            },
        });
        Some(ordered)
    }

    /// The outputs, as the compositor describes them.
    ///
    /// This asks nothing of the seat: reading the output list is not reading a
    /// pointer.  The compositors that offer no pointer at all — a nested KWin
    /// under another session advertises a keyboard-only seat — can still
    /// describe their outputs, and every capture that only reads pixels has to
    /// keep working there.
    pub(crate) fn output_infos(&self) -> Result<Vec<OutputInfo>> {
        if self.output_proxies.is_empty() {
            return Err(VshotError::IncompleteTopology(
                "no wl_output globals were advertised".into(),
            ));
        }
        if self.xdg_output_manager.is_none() {
            return Err(VshotError::MissingCapability(
                "zxdg_output_manager_v1".into(),
            ));
        }
        if self.xdg_outputs.len() != self.output_proxies.len() {
            return Err(VshotError::IncompleteTopology(
                "not every wl_output has an xdg-output object".into(),
            ));
        }

        let mut ids = self.output_proxies.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        let mut outputs = Vec::with_capacity(ids.len());
        for global_id in ids {
            let output = self.outputs.get(&global_id).ok_or_else(|| {
                VshotError::IncompleteTopology(format!(
                    "wl_output {global_id} has no tracked metadata"
                ))
            })?;
            let name = output.name.clone().ok_or_else(|| {
                VshotError::IncompleteTopology(format!("wl_output {global_id} has no output name"))
            })?;
            let logical_position = output.logical_position.ok_or_else(|| {
                VshotError::IncompleteTopology(format!(
                    "output {name} has no xdg-output logical position"
                ))
            })?;
            let logical_size = output.logical_size.ok_or_else(|| {
                VshotError::IncompleteTopology(format!(
                    "output {name} has no xdg-output logical size"
                ))
            })?;
            let pixel_size = output.pixel_size.ok_or_else(|| {
                VshotError::IncompleteTopology(format!(
                    "output {name} has no current wl_output mode"
                ))
            })?;
            let transform = output.transform.ok_or_else(|| {
                VshotError::IncompleteTopology(format!("output {name} has no wl_output transform"))
            })?;
            let info = OutputInfo {
                global_id,
                name,
                geometry: Rect::new(
                    logical_position.x,
                    logical_position.y,
                    logical_size.width,
                    logical_size.height,
                ),
                pixel_size,
                scale: output.scale,
                transform,
            };
            if !info.is_supported() {
                return Err(VshotError::UnsupportedOutput(format!(
                    "output {} is not an unrotated integer-scale mapping (logical {}x{}, pixels {}x{}, scale {}, transform {:?})",
                    info.name,
                    info.geometry.size.width,
                    info.geometry.size.height,
                    info.pixel_size.width,
                    info.pixel_size.height,
                    info.scale,
                    info.transform
                )));
            }
            outputs.push(info);
        }
        Ok(outputs)
    }
}
