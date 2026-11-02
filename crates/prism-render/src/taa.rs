//! Temporal anti-aliasing resolve (#883 Phase 1 + 2).
//!
//! Sits between `ForwardPass` and `PostPass` when `RenderSettings::taa` is on.
//! It samples the current HDR color + depth, prefers the rasterized per-object
//! velocity buffer, falls back to previous-view-projection reprojection,
//! clamps the history to the current 3×3 neighborhood, and blends with
//! `taa_strength`. Transparent draws mark `normal.w = 1`; the resolve drops
//! their history so blended surfaces never smear across frames.
//!
//! Two HDR images are ping-ponged by frame-in-flight slot: frame N writes
//! `images[N % 2]` and reads `images[(N + 1) % 2]`, exactly like GTAO. No copy
//! is needed, and the previous image is already `SHADER_READ_ONLY_OPTIMAL`.
//!
//! The outputs are published as [`TAA_COLOR_H`]; `PostPass` reads that handle
//! instead of `FORWARD_COLOR_H` while TAA is enabled. With TAA off this pass
//! returns immediately, so the legacy path is untouched.

use anyhow::Context as _;
use anyhow::Result;
use ash::vk;

use crate::context::VulkanContext;
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderPassNode,
    RenderSettings, ResourceType, ResourceUsage, FORWARD_COLOR_H, FORWARD_DEPTH_H,
    FORWARD_NORMAL_H, FORWARD_VELOCITY_H, TAA_COLOR_H,
};
use crate::render_pass::find_memory_type;
use crate::shader;
use crate::shader_bindings::taa as bindings;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct TaaParams {
    inv_view_proj: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    resolution: [f32; 2],
    strength: f32,
    valid_history: f32,
}

const _: () = assert!(std::mem::size_of::<TaaParams>() == 144);

pub struct TaaPass {
    extent: vk::Extent2D,
    images: [vk::Image; 2],
    memories: [vk::DeviceMemory; 2],
    views: [vk::ImageView; 2],
    framebuffers: [vk::Framebuffer; 2],
    render_pass: Option<vk::RenderPass>,
    pipeline: Option<GraphicsPipeline>,
    sampler: vk::Sampler,
    ds_layout: vk::DescriptorSetLayout,
    ds_pool: vk::DescriptorPool,
    sets: [vk::DescriptorSet; 2],
    ubos: [vk::Buffer; 2],
    ubo_memories: [vk::DeviceMemory; 2],
    /// Previous frame's unjittered view-projection.
    prev_view_proj: Option<[[f32; 4]; 4]>,
    /// Whether each ping-pong image contains a valid history sample.
    history_valid: [bool; 2],
    device: Option<ash::Device>,
}

impl TaaPass {
    pub fn new(context: &VulkanContext, extent: vk::Extent2D) -> Result<Self> {
        let device = &context.device;
        let extent = vk::Extent2D {
            width: extent.width.max(1),
            height: extent.height.max(1),
        };

        let mut images = [vk::Image::null(); 2];
        let mut memories = [vk::DeviceMemory::null(); 2];
        let mut views = [vk::ImageView::null(); 2];
        for i in 0..2 {
            let (image, memory, view) = create_taa_image(context, extent)?;
            images[i] = image;
            memories[i] = memory;
            views[i] = view;
        }

        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .min_lod(0.0)
                    .max_lod(vk::LOD_CLAMP_NONE),
                None,
            )
        }
        .context("TaaPass: create sampler")?;

        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::COLOR_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::DEPTH_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::HISTORY_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::PARAMS_BINDING)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::VELOCITY_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::NORMAL_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let ds_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .context("TaaPass: create ds layout")?;

        let frames = 2u32;
        let ds_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(frames)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                            descriptor_count: frames * 5,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::UNIFORM_BUFFER,
                            descriptor_count: frames,
                        },
                    ]),
                None,
            )
        }
        .context("TaaPass: create ds pool")?;
        let layouts = vec![ds_layout; frames as usize];
        let allocated = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(ds_pool)
                    .set_layouts(&layouts),
            )
        }
        .context("TaaPass: allocate ds")?;
        let sets = [allocated[0], allocated[1]];

        let ubo_size = std::mem::size_of::<TaaParams>() as vk::DeviceSize;
        let mut ubos = [vk::Buffer::null(); 2];
        let mut ubo_memories = [vk::DeviceMemory::null(); 2];
        for i in 0..2 {
            let (buffer, memory) = crate::buffer::create_buffer(
                context,
                ubo_size,
                crate::buffer::BufferUsage::UNIFORM_BUFFER,
                crate::buffer::MemoryProperties::HOST_VISIBLE
                    | crate::buffer::MemoryProperties::HOST_COHERENT,
            )
            .context("TaaPass: create params UBO")?;
            let ptr =
                unsafe { device.map_memory(memory, 0, ubo_size, vk::MemoryMapFlags::empty()) }
                    .context("TaaPass: map params UBO")?;
            unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, ubo_size as usize) };
            unsafe { device.unmap_memory(memory) };
            ubos[i] = buffer;
            ubo_memories[i] = memory;
        }

        let render_pass = create_render_pass(device)?;
        let mut framebuffers = [vk::Framebuffer::null(); 2];
        for i in 0..2 {
            framebuffers[i] = create_framebuffer(device, render_pass, views[i], extent)?;
        }
        let pipeline = create_pipeline(device, render_pass, ds_layout)?;

        Ok(Self {
            extent,
            images,
            memories,
            views,
            framebuffers,
            render_pass: Some(render_pass),
            pipeline: Some(pipeline),
            sampler,
            ds_layout,
            ds_pool,
            sets,
            ubos,
            ubo_memories,
            prev_view_proj: None,
            history_valid: [false; 2],
            device: Some(device.clone()),
        })
    }

    pub(crate) fn recreate_target(
        &mut self,
        context: &VulkanContext,
        extent: vk::Extent2D,
    ) -> Result<()> {
        let extent = vk::Extent2D {
            width: extent.width.max(1),
            height: extent.height.max(1),
        };
        if extent == self.extent {
            return Ok(());
        }
        let device = &context.device;
        unsafe {
            for i in 0..2 {
                device.destroy_framebuffer(self.framebuffers[i], None);
                device.destroy_image_view(self.views[i], None);
                device.destroy_image(self.images[i], None);
                device.free_memory(self.memories[i], None);
            }
        }
        for i in 0..2 {
            let (image, memory, view) = create_taa_image(context, extent)?;
            self.images[i] = image;
            self.memories[i] = memory;
            self.views[i] = view;
            self.framebuffers[i] =
                create_framebuffer(device, self.render_pass.unwrap(), view, extent)?;
        }
        self.extent = extent;
        self.history_valid = [false; 2];
        self.prev_view_proj = None;
        Ok(())
    }

    fn upload_params(&self, device: &ash::Device, index: usize, params: &TaaParams) -> Result<()> {
        let size = std::mem::size_of::<TaaParams>() as vk::DeviceSize;
        let ptr = unsafe {
            device.map_memory(
                self.ubo_memories[index],
                0,
                size,
                vk::MemoryMapFlags::empty(),
            )
        }
        .context("TaaPass: map params")?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                params as *const TaaParams as *const u8,
                ptr as *mut u8,
                size as usize,
            );
            device.unmap_memory(self.ubo_memories[index]);
        }
        Ok(())
    }

    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            for i in 0..2 {
                if self.framebuffers[i] != vk::Framebuffer::null() {
                    device.destroy_framebuffer(self.framebuffers[i], None);
                }
                if self.views[i] != vk::ImageView::null() {
                    device.destroy_image_view(self.views[i], None);
                }
                if self.memories[i] != vk::DeviceMemory::null() {
                    device.free_memory(self.memories[i], None);
                }
                if self.images[i] != vk::Image::null() {
                    device.destroy_image(self.images[i], None);
                }
                if self.ubos[i] != vk::Buffer::null() {
                    device.destroy_buffer(self.ubos[i], None);
                }
                if self.ubo_memories[i] != vk::DeviceMemory::null() {
                    device.free_memory(self.ubo_memories[i], None);
                }
            }
        }
        if let Some(rp) = self.render_pass.take() {
            unsafe { device.destroy_render_pass(rp, None) };
        }
        self.pipeline = None;
        unsafe {
            device.destroy_descriptor_pool(self.ds_pool, None);
            device.destroy_descriptor_set_layout(self.ds_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
        self.device = None;
    }
}

impl Drop for TaaPass {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            self.destroy(&device);
        }
    }
}

impl RenderPassNode for TaaPass {
    fn name(&self) -> &str {
        "TaaPass"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        graph.create_resource_at(
            TAA_COLOR_H,
            ResourceType::ColorAttachment {
                format: vk::Format::R16G16B16A16_SFLOAT,
                extent: vk::Extent2D {
                    width: 1,
                    height: 1,
                },
                sample_count: vk::SampleCountFlags::TYPE_1,
            },
        );
        graph.read_usage(ResourceUsage {
            handle: FORWARD_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        graph.read_usage(ResourceUsage {
            handle: FORWARD_DEPTH_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        });
        graph.read_usage(ResourceUsage {
            handle: FORWARD_VELOCITY_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        graph.read_usage(ResourceUsage {
            handle: FORWARD_NORMAL_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        graph.write_usage(ResourceUsage {
            handle: TAA_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
    }

    fn execute(&mut self, ctx: &RenderContext, resources: &mut GraphResources) -> Result<()> {
        eprintln!(
            "TAA exec taa={} jitter={:?}",
            ctx.settings.taa, ctx.frame.view_proj_jitter
        );
        if !ctx.settings.taa {
            return Ok(());
        }
        self.recreate_target(ctx.context, ctx.render_extent)?;
        let Some(color_view) = resources.published_view(FORWARD_COLOR_H) else {
            return Ok(());
        };
        let Some(depth_view) = resources.published_view(FORWARD_DEPTH_H) else {
            return Ok(());
        };
        let Some(velocity_view) = resources.published_view(FORWARD_VELOCITY_H) else {
            return Ok(());
        };
        let Some(normal_view) = resources.published_view(FORWARD_NORMAL_H) else {
            return Ok(());
        };

        let frame = (ctx.frame_index as usize) % 2;
        let prev = (frame + 1) % 2;

        // Recover the unjittered camera matrix (GraphRenderer applied
        // `view_proj_jitter` after building the graph frame).
        let mut view_proj = ctx.frame.view_proj;
        view_proj[3][0] -= ctx.frame.view_proj_jitter[0];
        view_proj[3][1] -= ctx.frame.view_proj_jitter[1];
        let inv_view_proj = crate::math::mat_inverse(&view_proj);
        let prev_view_proj = self.prev_view_proj.unwrap_or(view_proj);
        let valid = self.history_valid[prev] && matrix_close(prev_view_proj, view_proj);
        if !matrix_close(prev_view_proj, view_proj) {
            self.history_valid = [false; 2];
        }
        let params = TaaParams {
            inv_view_proj,
            prev_view_proj,
            resolution: [self.extent.width as f32, self.extent.height as f32],
            strength: ctx.settings.taa_strength.clamp(0.0, 0.95),
            valid_history: if valid { 1.0 } else { 0.0 },
        };
        self.upload_params(ctx.device, frame, &params)?;
        self.prev_view_proj = Some(view_proj);

        let color_info = vk::DescriptorImageInfo::default()
            .image_view(color_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let depth_info = vk::DescriptorImageInfo::default()
            .image_view(depth_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL);
        let history_info = vk::DescriptorImageInfo::default()
            .image_view(self.views[prev])
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let velocity_info = vk::DescriptorImageInfo::default()
            .image_view(velocity_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let normal_info = vk::DescriptorImageInfo::default()
            .image_view(normal_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let ubo_info = vk::DescriptorBufferInfo::default()
            .buffer(self.ubos[frame])
            .offset(0)
            .range(std::mem::size_of::<TaaParams>() as vk::DeviceSize);
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[frame])
                .dst_binding(bindings::COLOR_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&color_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[frame])
                .dst_binding(bindings::DEPTH_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&depth_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[frame])
                .dst_binding(bindings::HISTORY_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&history_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[frame])
                .dst_binding(bindings::PARAMS_BINDING)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(std::slice::from_ref(&ubo_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[frame])
                .dst_binding(bindings::VELOCITY_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&velocity_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[frame])
                .dst_binding(bindings::NORMAL_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&normal_info)),
        ];
        unsafe { ctx.device.update_descriptor_sets(&writes, &[]) };

        let old_layout = if self.history_valid[frame] {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        } else {
            vk::ImageLayout::UNDEFINED
        };
        let to_attachment = vk::ImageMemoryBarrier::default()
            .src_access_mask(if self.history_valid[frame] {
                vk::AccessFlags::SHADER_READ
            } else {
                vk::AccessFlags::empty()
            })
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(self.images[frame])
            .subresource_range(color_range());
        unsafe {
            ctx.device.cmd_pipeline_barrier(
                ctx.cmd,
                if self.history_valid[frame] {
                    vk::PipelineStageFlags::FRAGMENT_SHADER
                } else {
                    vk::PipelineStageFlags::TOP_OF_PIPE
                },
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_attachment),
            );
        }

        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass.unwrap())
            .framebuffer(self.framebuffers[frame])
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.extent,
            })
            .clear_values(std::slice::from_ref(&clear));
        unsafe {
            ctx.device
                .cmd_begin_render_pass(ctx.cmd, &begin, vk::SubpassContents::INLINE);
            ctx.device.cmd_bind_pipeline(
                ctx.cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.as_ref().unwrap().pipeline,
            );
            ctx.device.cmd_bind_descriptor_sets(
                ctx.cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.as_ref().unwrap().layout,
                0,
                std::slice::from_ref(&self.sets[frame]),
                &[],
            );
            ctx.device.cmd_set_viewport(
                ctx.cmd,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: self.extent.width as f32,
                    height: self.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            ctx.device.cmd_set_scissor(
                ctx.cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: self.extent,
                }],
            );
            ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
            ctx.device.cmd_end_render_pass(ctx.cmd);
        }

        let to_read = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(self.images[frame])
            .subresource_range(color_range());
        unsafe {
            ctx.device.cmd_pipeline_barrier(
                ctx.cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_read),
            );
        }

        resources.set_image_view(TAA_COLOR_H, self.views[frame]);
        resources.set_image(TAA_COLOR_H, self.images[frame]);
        self.history_valid[frame] = true;
        Ok(())
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Post,
            inputs: vec![
                FORWARD_COLOR_H,
                FORWARD_DEPTH_H,
                FORWARD_VELOCITY_H,
                FORWARD_NORMAL_H,
            ],
            outputs: vec![TAA_COLOR_H],
        }
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn matrix_close(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> bool {
    a.iter()
        .flatten()
        .zip(b.iter().flatten())
        .all(|(x, y)| (x - y).abs() < 1.0)
}

fn create_taa_image(
    context: &VulkanContext,
    extent: vk::Extent2D,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let device = &context.device;
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R16G16B16A16_SFLOAT)
        .extent(vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let image = unsafe { device.create_image(&info, None) }.context("TaaPass: create image")?;
    let req = unsafe { device.get_image_memory_requirements(image) };
    let memory_type = find_memory_type(
        &context.physical_device_memory_properties,
        req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .context("TaaPass: no memory type")?;
    let memory = unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(memory_type),
            None,
        )
    }
    .context("TaaPass: allocate memory")?;
    unsafe { device.bind_image_memory(image, memory, 0) }.context("TaaPass: bind memory")?;
    let view = unsafe {
        device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::R16G16B16A16_SFLOAT)
                .subresource_range(color_range()),
            None,
        )
    }
    .context("TaaPass: create view")?;
    Ok((image, memory, view))
}

fn create_framebuffer(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    view: vk::ImageView,
    extent: vk::Extent2D,
) -> Result<vk::Framebuffer> {
    unsafe {
        device.create_framebuffer(
            &vk::FramebufferCreateInfo::default()
                .render_pass(render_pass)
                .attachments(std::slice::from_ref(&view))
                .width(extent.width)
                .height(extent.height)
                .layers(1),
            None,
        )
    }
    .context("TaaPass: create framebuffer")
}

fn create_render_pass(device: &ash::Device) -> Result<vk::RenderPass> {
    let attachment = vk::AttachmentDescription::default()
        .format(vk::Format::R16G16B16A16_SFLOAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(std::slice::from_ref(&color_ref));
    unsafe {
        device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(std::slice::from_ref(&attachment))
                .subpasses(std::slice::from_ref(&subpass)),
            None,
        )
    }
    .context("TaaPass: create render pass")
}

fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    ds_layout: vk::DescriptorSetLayout,
) -> Result<GraphicsPipeline> {
    const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/taa.vert.spv");
    const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/taa.frag.spv");
    let vert_module =
        shader::load_shader_module(device, VERT_SPV).context("TaaPass: load vertex shader")?;
    let frag_module =
        shader::load_shader_module(device, FRAG_SPV).context("TaaPass: load fragment shader")?;
    let vert = std::ffi::CString::new(bindings::ENTRY_VERTEX_MAIN).unwrap();
    let frag = std::ffi::CString::new(bindings::ENTRY_FRAGMENT_MAIN).unwrap();
    let stages = [
        shader::shader_stage(vk::ShaderStageFlags::VERTEX, vert_module, vert.as_c_str()),
        shader::shader_stage(vk::ShaderStageFlags::FRAGMENT, frag_module, frag.as_c_str()),
    ];
    let binding_descs: [vk::VertexInputBindingDescription; 0] = [];
    let attr_descs: [vk::VertexInputAttributeDescription; 0] = [];
    let set_layouts = [ds_layout];
    let blend = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(false);
    let pipeline = GraphicsPipeline::new(&PipelineDesc {
        device,
        shader_stages: &stages,
        vertex_binding_desc: &binding_descs,
        vertex_attr_descs: &attr_descs,
        descriptor_set_layouts: &set_layouts,
        push_constant_ranges: &[],
        render_pass,
        subpass: 0,
        cull_mode: Some(vk::CullModeFlags::NONE),
        depth_bias_enable: None,
        depth_bias_constant_factor: None,
        depth_bias_slope_factor: None,
        depth_write_enable: Some(false),
        depth_test_enable: None,
        color_attachment_count: None,
        color_blend_attachments: Some(std::slice::from_ref(&blend)),
        front_face: None,
        polygon_mode: None,
    })
    .context("TaaPass: create pipeline")?;
    unsafe {
        device.destroy_shader_module(vert_module, None);
        device.destroy_shader_module(frag_module, None);
    }
    Ok(pipeline)
}
