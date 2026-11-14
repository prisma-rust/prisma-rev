//! Linear HDR readback of a graph-published floating-point image.
//!
//! The offscreen path ([`crate::GraphRenderer::readback_pixels`]) hands back the
//! post-processed 8-bit swapchain image, i.e. after tonemapping and grading. That is
//! the right thing for screenshot tests, but it cannot answer the question the
//! repair spec §7.2 asks: does the transport converge to the analytically known
//! radiance? A white furnace, a pure emitter and a two-segment throughput test all
//! compare **linear** radiance, so they need the path tracer's
//! `R32G32B32A32_SFLOAT` output (`PT_COLOR_H`) sampled before `PostPass` touches it.
//!
//! # Usage
//!
//! ```text
//! renderer.request_hdr_readback();
//! // ... run one frame (begin_frame -> prepare -> execute -> present -> end_frame) ...
//! if let Some(hdr) = renderer.take_hdr_readback()? {
//!     println!("mean rgb = {:?}", hdr.mean_rgb());
//! }
//! ```
//!
//! The copy is recorded at the end of
//! [`crate::render_graph::RenderGraph::execute_with_timer`], after every pass ran, so it
//! sees the frame's final image. The staging buffer is only mapped in
//! [`crate::render_graph::RenderGraph::take_hdr_readback`], which must happen after the
//! frame fence signalled - i.e. after [`crate::GraphRenderer::present`] (headless) or
//! [`crate::GraphRenderer::end_frame`].

use anyhow::Context as _;
use ash::vk;

/// A frame's worth of linear radiance, row-major from the top-left texel.
///
/// Alpha is the value the shader wrote (the path tracer stores 1.0), so callers
/// that only care about radiance should read [`Self::mean_rgb`] / [`Self::max_rgb`]
/// rather than averaging all four channels.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HdrReadback {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Row-major RGBA texels, in render-extent order.
    pub pixels: Vec<[f32; 4]>,
}

impl HdrReadback {
    /// Texel at `(x, y)`, or `None` when out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[f32; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.pixels.get((y * self.width + x) as usize).copied()
    }

    /// Mean RGB over every texel. The analytic tests compare this against the
    /// closed-form answer (e.g. 1.0 for a white furnace).
    pub fn mean_rgb(&self) -> [f32; 3] {
        let mut sum = [0.0f32; 3];
        for px in &self.pixels {
            sum[0] += px[0];
            sum[1] += px[1];
            sum[2] += px[2];
        }
        let inv = 1.0 / self.pixels.len().max(1) as f32;
        [sum[0] * inv, sum[1] * inv, sum[2] * inv]
    }

    /// Mean luminance (Rec. 709 weights) over every texel.
    pub fn mean_luminance(&self) -> f32 {
        let [r, g, b] = self.mean_rgb();
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// Per-channel maximum. Fireflies and NaN/Inf in the reference variant
    /// (`PT_MODE_REFERENCE` skips the firefly clamp) show up here first.
    pub fn max_rgb(&self) -> [f32; 3] {
        if self.pixels.is_empty() {
            return [0.0; 3];
        }
        let mut max = [f32::NEG_INFINITY; 3];
        for px in &self.pixels {
            for (c, slot) in max.iter_mut().enumerate() {
                *slot = slot.max(px[c]);
            }
        }
        max
    }

    /// Whether any channel of any texel is NaN or infinite.
    pub fn has_non_finite(&self) -> bool {
        self.non_finite_count() > 0
    }

    /// Number of texels whose any RGB channel is NaN or infinite.
    pub fn non_finite_count(&self) -> usize {
        self.pixels
            .iter()
            .filter(|px| px[0..3].iter().any(|c| !c.is_finite()))
            .count()
    }
}

/// A recorded copy that has not been mapped yet.
///
/// Owned by [`crate::render_graph::RenderGraph`]; the buffer is freed either by
/// [`Self::take`] (after mapping) or by [`Self::destroy`] (renderer teardown).
pub(crate) struct PendingHdrReadback {
    pub(crate) buffer: vk::Buffer,
    pub(crate) memory: vk::DeviceMemory,
    pub(crate) extent: vk::Extent2D,
}

impl PendingHdrReadback {
    /// Map the staging buffer, copy the texels out, and free the Vulkan objects.
    ///
    /// Call only after the frame fence signalled.
    pub(crate) fn take(self, device: &ash::Device) -> anyhow::Result<HdrReadback> {
        let count = (self.extent.width as usize) * (self.extent.height as usize);
        let bytes = (count * std::mem::size_of::<[f32; 4]>()) as vk::DeviceSize;

        let result = (|| -> anyhow::Result<HdrReadback> {
            let mapped =
                unsafe { device.map_memory(self.memory, 0, bytes, vk::MemoryMapFlags::empty()) }
                    .context("hdr readback: map staging buffer")?;
            // The staging buffer holds tightly packed `[f32; 4]` texels, so
            // view it as an array of texels instead of a flat f32 run.
            let texels = unsafe { std::slice::from_raw_parts(mapped as *const [f32; 4], count) };
            let pixels = texels.to_vec();
            unsafe { device.unmap_memory(self.memory) };
            Ok(HdrReadback {
                width: self.extent.width,
                height: self.extent.height,
                pixels,
            })
        })();

        // Free the staging objects regardless of whether the map succeeded.
        unsafe {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
        result
    }

    /// Free the Vulkan objects without reading them back.
    pub(crate) fn destroy(self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}
