//! Automatic exposure metering for the tonemapper.
//!
//! Ported from RTXGI's `Samples/Shared/PostProcess/AutoExposurePass` +
//! `Samples/Shared/Shaders/AutoExposure.hlsl` (v2.9.0). The scene is shaded
//! with physically-based radiance, so one fixed exposure cannot cover both a
//! sunlit facade and a corridor lit only by indirect bounces: on the Sponza
//! scene the path-traced interior meters at 0.0038 mean luminance against a
//! 0.18 middle grey, i.e. 47x too dark. Metering adapts the tonemapper to
//! whatever is actually on screen.
//!
//! The pass runs between the scene pass and `PostPass`:
//! 1. `luminanceHistogram` builds a log2 luminance histogram, one 16x16 thread
//!    group per tile, merging into shared memory and then into the global
//!    histogram with atomics.
//! 2. `exposureAdaptation` reduces the histogram to the geometric mean of the
//!    pixels between two percentiles (dropping black pixels and fireflies) and
//!    moves the adapted luminance toward it in log space.
//!
//! The adapted luminance lives in a one-element `R32_UINT` buffer as a float
//! bit pattern. `PostPass` divides `AUTO_EXPOSURE_KEY` by it, which is exactly
//! the RTXGI tonemapper contract, and the same buffer is what a future DLSS
//! integration would consume as its exposure buffer.

use anyhow::Context as _;
use ash::vk;
use std::ffi::CString;
use std::time::Instant;

use crate::buffer::create_buffer;
use crate::compute::ComputePipeline;
use crate::context::VulkanContext;
use crate::post::select_input_target;
use crate::render_graph::{
    GraphResources, RenderContext, RenderGraphBuilder, RenderPassNode, RenderSettings,
    ResourceUsage, FORWARD_COLOR_H, PT_COLOR_H, TAA_COLOR_H,
};
use crate::shader_bindings::auto_exposure as bindings;

/// Scene luminance mapped to middle grey. Matches RTXGI `AUTO_EXPOSURE_KEY`.
pub const AUTO_EXPOSURE_KEY: f32 = 0.18;

/// Histogram bins (also the histogram buffer's element count).
pub const HISTOGRAM_BINS: u32 = 256;

const GROUP_SIZE: u32 = 16;

/// Metering range in log2 scene luminance (EV), matching RTXGI's defaults.
const MIN_LOG_LUMINANCE: f32 = -12.0;
const MAX_LOG_LUMINANCE: f32 = 12.0;
/// Fraction of the darkest pixels ignored (shadows, black pixels).
const LOW_PERCENTILE: f32 = 0.4;
/// Fraction above which pixels are ignored (emitters, fireflies).
const HIGH_PERCENTILE: f32 = 0.95;
/// Adaptation rates in EV/s; the remaining gap halves every 1/speed seconds.
const ADAPTATION_SPEED_UP: f32 = 3.0;
const ADAPTATION_SPEED_DOWN: f32 = 1.0;

/// GPU buffers the metering writes into.
struct MeteringBuffers {
    histogram: vk::Buffer,
    histogram_memory: vk::DeviceMemory,
    luminance: vk::Buffer,
    luminance_memory: vk::DeviceMemory,
}

impl MeteringBuffers {
    fn new(context: &VulkanContext) -> anyhow::Result<Self> {
        let usage = vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST;
        let props = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        let (histogram, histogram_memory) =
            create_buffer(context, HISTOGRAM_BINS as vk::DeviceSize * 4, usage, props)
                .context("auto exposure: create histogram buffer")?;
        let (luminance, luminance_memory) = create_buffer(context, 4, usage, props)
            .context("auto exposure: create luminance buffer")?;
        Ok(Self {
            histogram,
            histogram_memory,
            luminance,
            luminance_memory,
        })
    }

    fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer(self.histogram, None);
            device.free_memory(self.histogram_memory, None);
            device.destroy_buffer(self.luminance, None);
            device.free_memory(self.luminance_memory, None);
        }
    }
}

/// Meters the HDR scene colour and publishes the adapted luminance.
pub struct AutoExposurePass {
    histogram_pipeline: ComputePipeline,
    adapt_pipeline: ComputePipeline,
    ds_layout: vk::DescriptorSetLayout,
    ds_pool: vk::DescriptorPool,
    /// One set per swapchain image so a set is never rewritten while the frame
    /// that references it is still in flight.
    sets: Vec<vk::DescriptorSet>,
    /// Source view currently written into each set. Rewriting a binding that
    /// the GPU may still be reading needs `UPDATE_AFTER_BIND`, which this
    /// layout does not use, so skip the write unless the view actually changed.
    bound_views: Vec<vk::ImageView>,
    bound_layouts: Vec<vk::ImageLayout>,
    sampler: vk::Sampler,
    buffers: MeteringBuffers,
    devices: Option<ash::Device>,
    /// Wall-clock delta between dispatches, in seconds. The metering runs on
    /// the render thread, so this is the presentation interval the eye sees.
    last_dispatch: Option<Instant>,
}

impl AutoExposurePass {
    /// `frames_in_flight` is the swapchain image count (the offscreen target
    /// uses one), so each in-flight frame keeps its own descriptor set.
    pub fn new(context: &VulkanContext, frames_in_flight: u32) -> anyhow::Result<Self> {
        let device = &context.device;
        let buffers = MeteringBuffers::new(context)?;

        // `.Load()` needs no filtering, but the SPIR-V still declares the image
        // as a sampled image, so a sampler has to be bound.
        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .min_lod(0.0)
                    .max_lod(0.0),
                None,
            )
        }
        .context("AutoExposurePass: create sampler")?;

        let bindings_layout = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::SOURCE_COLOR_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::HISTOGRAM_OUTPUT_BINDING)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::ADAPTED_LUMINANCE_BINDING)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let ds_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings_layout),
                None,
            )
        }
        .context("AutoExposurePass: create ds layout")?;

        let fif = frames_in_flight.max(1);
        let ds_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(fif)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                            descriptor_count: fif,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_BUFFER,
                            descriptor_count: fif * 2,
                        },
                    ]),
                None,
            )
        }
        .context("AutoExposurePass: create ds pool")?;

        let layouts = vec![ds_layout; fif as usize];
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(ds_pool)
                    .set_layouts(&layouts),
            )
        }
        .context("AutoExposurePass: allocate ds")?;

        // The histogram and luminance buffers are frame-independent, so they
        // are written once here and only the source image is rewritten per frame.
        let histogram_info = [vk::DescriptorBufferInfo {
            buffer: buffers.histogram,
            offset: 0,
            range: vk::WHOLE_SIZE,
        }];
        let luminance_info = [vk::DescriptorBufferInfo {
            buffer: buffers.luminance,
            offset: 0,
            range: vk::WHOLE_SIZE,
        }];
        let static_writes: Vec<vk::WriteDescriptorSet> = sets
            .iter()
            .flat_map(|&set| {
                [
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(bindings::HISTOGRAM_OUTPUT_BINDING)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&histogram_info),
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(bindings::ADAPTED_LUMINANCE_BINDING)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&luminance_info),
                ]
            })
            .collect();
        unsafe { device.update_descriptor_sets(&static_writes, &[]) };

        let push_ranges = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::COMPUTE,
            offset: 0,
            size: bindings::PUSH_CONSTANT_SIZE,
        }];
        let set_layouts = [ds_layout];

        let histogram_spv: &[u8] =
            include_bytes!("../../../assets/shaders/auto_exposure_hist.comp.spv");
        let histogram_module = crate::shader::load_shader_module(device, histogram_spv)
            .context("AutoExposurePass: load histogram module")?;
        let entry = CString::new(bindings::ENTRY_LUMINANCE_HISTOGRAM).unwrap();
        let histogram_pipeline = ComputePipeline::new(
            device,
            histogram_module,
            entry.as_c_str(),
            &set_layouts,
            &push_ranges,
        )
        .context("AutoExposurePass: create histogram pipeline")?;
        unsafe { device.destroy_shader_module(histogram_module, None) };

        let adapt_spv: &[u8] =
            include_bytes!("../../../assets/shaders/auto_exposure_adapt.comp.spv");
        let adapt_module = crate::shader::load_shader_module(device, adapt_spv)
            .context("AutoExposurePass: load adaptation module")?;
        let entry = CString::new(bindings::ENTRY_EXPOSURE_ADAPTATION).unwrap();
        let adapt_pipeline = ComputePipeline::new(
            device,
            adapt_module,
            entry.as_c_str(),
            &set_layouts,
            &push_ranges,
        )
        .context("AutoExposurePass: create adaptation pipeline")?;
        unsafe { device.destroy_shader_module(adapt_module, None) };

        Ok(Self {
            histogram_pipeline,
            adapt_pipeline,
            ds_layout,
            ds_pool,
            bound_views: vec![vk::ImageView::null(); sets.len()],
            bound_layouts: vec![vk::ImageLayout::UNDEFINED; sets.len()],
            sets,
            sampler,
            buffers,
            devices: Some(device.clone()),
            last_dispatch: None,
        })
    }

    /// The buffer holding the adapted luminance as a float bit pattern.
    pub fn luminance_buffer(&self) -> vk::Buffer {
        self.buffers.luminance
    }

    /// Bind the metered HDR image and record both dispatches.
    fn dispatch(
        &mut self,
        ctx: &RenderContext,
        view: vk::ImageView,
        layout: vk::ImageLayout,
        extent: vk::Extent2D,
    ) {
        let device = &ctx.device;
        let set = self.sets[(ctx.image_index as usize) % self.sets.len()];

        // `AutoExposurePass` descriptor bindings do not carry UPDATE_AFTER_BIND
        // (the metering inputs are stable), so a write to a set the GPU may
        // still be reading is a validation error. The source view changes only
        // on resize or a render-mode switch, so skip the write otherwise.
        let slot = (ctx.image_index as usize) % self.sets.len();
        if self.bound_views[slot] != view || self.bound_layouts[slot] != layout {
            self.bound_views[slot] = view;
            self.bound_layouts[slot] = layout;
            let image_info = [vk::DescriptorImageInfo::default()
                .image_view(view)
                .sampler(self.sampler)
                .image_layout(layout)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(bindings::SOURCE_COLOR_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&image_info);
            unsafe { device.update_descriptor_sets(&[write], &[]) };
        }

        let now = Instant::now();
        let frame_time = self
            .last_dispatch
            .map(|last| now.duration_since(last).as_secs_f32())
            .map(|dt| dt.min(0.5))
            .unwrap_or(0.0);
        self.last_dispatch = Some(now);

        let log_range = (MAX_LOG_LUMINANCE - MIN_LOG_LUMINANCE).max(1.0);
        let push = bindings::AutoExposurePush {
            sourceSize: [extent.width, extent.height],
            minLogLuminance: MIN_LOG_LUMINANCE,
            logLuminanceRange: log_range,
            recipLogLuminanceRange: 1.0 / log_range,
            lowPercentile: LOW_PERCENTILE,
            highPercentile: HIGH_PERCENTILE,
            adaptationSpeedUp: ADAPTATION_SPEED_UP,
            adaptationSpeedDown: ADAPTATION_SPEED_DOWN,
            frameTime: frame_time,
            // Frame 0 has no history to adapt from, so snap to the metered
            // value instead of easing in from an arbitrary exposure.
            resetAdaptation: u32::from(frame_time == 0.0),
            exposureScale: ctx.frame.exposure,
        };

        unsafe {
            // Clearing the histogram is a transfer operation; the dispatch that
            // follows accumulates into it.
            device.cmd_fill_buffer(ctx.cmd, self.buffers.histogram, 0, vk::WHOLE_SIZE, 0);

            let to_compute = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::SHADER_READ);
            device.cmd_pipeline_barrier(
                ctx.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                std::slice::from_ref(&to_compute),
                &[],
                &[],
            );

            device.cmd_bind_pipeline(
                ctx.cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.histogram_pipeline.pipeline,
            );
            device.cmd_bind_descriptor_sets(
                ctx.cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.histogram_pipeline.layout,
                0,
                &[set],
                &[],
            );
            device.cmd_push_constants(
                ctx.cmd,
                self.histogram_pipeline.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(&push),
            );
            device.cmd_dispatch(
                ctx.cmd,
                extent.width.div_ceil(GROUP_SIZE),
                extent.height.div_ceil(GROUP_SIZE),
                1,
            );

            // The adaptation reads the histogram the histogram dispatch just
            // filled, and writes the luminance buffer PostPass samples.
            let hist_to_adapt = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
            device.cmd_pipeline_barrier(
                ctx.cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                std::slice::from_ref(&hist_to_adapt),
                &[],
                &[],
            );

            device.cmd_bind_pipeline(
                ctx.cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.adapt_pipeline.pipeline,
            );
            device.cmd_bind_descriptor_sets(
                ctx.cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.adapt_pipeline.layout,
                0,
                &[set],
                &[],
            );
            device.cmd_push_constants(
                ctx.cmd,
                self.adapt_pipeline.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(&push),
            );
            device.cmd_dispatch(ctx.cmd, 1, 1, 1);

            // Hand the result to PostPass, which reads it in the fragment stage.
            let to_fragment = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            device.cmd_pipeline_barrier(
                ctx.cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                std::slice::from_ref(&to_fragment),
                &[],
                &[],
            );
        }
    }

    /// Release everything this pass allocated. The pipelines destroy their own
    /// handles on drop, so only the pass-owned objects are torn down here.
    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_sampler(self.sampler, None);
            device.destroy_descriptor_pool(self.ds_pool, None);
            device.destroy_descriptor_set_layout(self.ds_layout, None);
        }
        self.buffers.destroy(device);
        self.devices = None;
    }
}

impl RenderPassNode for AutoExposurePass {
    fn name(&self) -> &str {
        "AutoExposurePass"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // Declare every candidate HDR input so the graph inserts the barrier
        // into SHADER_READ_ONLY_OPTIMAL whichever one `execute` meters. The
        // same three handles `PostPass` reads, so the layouts are cache hits.
        for handle in [FORWARD_COLOR_H, PT_COLOR_H, TAA_COLOR_H] {
            graph.read_usage(ResourceUsage {
                handle,
                access: vk::AccessFlags::SHADER_READ,
                stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            });
        }
    }

    fn execute(
        &mut self,
        ctx: &RenderContext,
        resources: &mut GraphResources,
    ) -> anyhow::Result<()> {
        // Metering is opt-in. Off writes a zero adapted luminance, which the
        // tonemapper's `auto_exposure()` reads as "no metering" and turns into
        // a gain of 1.0, so the image matches the pre-auto-exposure pipeline.
        if !ctx.settings.auto_exposure {
            unsafe {
                ctx.device
                    .cmd_fill_buffer(ctx.cmd, self.buffers.luminance, 0, 4, 0);
                let to_fragment = vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ);
                ctx.device.cmd_pipeline_barrier(
                    ctx.cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    std::slice::from_ref(&to_fragment),
                    &[],
                    &[],
                );
            }
            return Ok(());
        }

        // Meter whatever the tonemapper is about to display. Mirror
        // `PostPass`'s selection so the metered image and the displayed image
        // are always the same one.
        let is_pt = ctx.frame.render_mode == crate::render_graph::RenderMode::PathTrace;
        let taa_view = if ctx.settings.taa && !is_pt && ctx.frame.debug_rt == 0 {
            resources.published_view(TAA_COLOR_H)
        } else {
            None
        };
        let (handle, layout) = if taa_view.is_some() {
            (TAA_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        } else {
            select_input_target(
                ctx.frame.debug_rt,
                is_pt,
                ctx.frame.has_camera,
                resources.published_view(PT_COLOR_H).is_some(),
            )
        };
        let Some(view) = resources.published_view(handle) else {
            // Nothing rendered yet; leave the previous adapted luminance alone
            // so the tone curve does not jump on a skipped frame.
            return Ok(());
        };

        let extent = if ctx.render_extent.width > 0 && ctx.render_extent.height > 0 {
            ctx.render_extent
        } else {
            ctx.extent
        };
        if extent.width == 0 || extent.height == 0 {
            return Ok(());
        }

        self.dispatch(ctx, view, layout, extent);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_layout_matches_reflection() {
        // The struct is generated from the Slang reflection, so this only
        // guards against a stale binding file after a shader edit.
        assert_eq!(
            std::mem::size_of::<bindings::AutoExposurePush>() as u32,
            bindings::PUSH_CONSTANT_SIZE
        );
    }

    #[test]
    fn histogram_buffer_holds_every_bin() {
        assert_eq!(HISTOGRAM_BINS, 256);
        // One u32 per bin, which is what `MeteringBuffers::new` allocates.
        assert_eq!(HISTOGRAM_BINS as vk::DeviceSize * 4, 1024);
    }

    #[test]
    fn percentiles_are_ordered() {
        // `exposureAdaptation` assumes low < high; inverting them makes the
        // weighted sum zero and freezes the exposure at its previous value.
        // These are compile-time constants, so the check is a const block
        // rather than a runtime assertion clippy would flag as constant.
        const _: () = assert!(LOW_PERCENTILE < HIGH_PERCENTILE);
        const _: () = assert!(LOW_PERCENTILE >= 0.0 && LOW_PERCENTILE <= 1.0);
        const _: () = assert!(HIGH_PERCENTILE >= 0.0 && HIGH_PERCENTILE <= 1.0);
    }
}
