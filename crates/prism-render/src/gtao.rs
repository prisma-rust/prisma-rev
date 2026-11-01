//! GTAO（真实环境光遮蔽）通道
//!
//! 半分辨率屏幕空间环境光遮蔽通道，在 `ForwardPass` 之后运行（ForwardPass 写入
//! D32_SFLOAT 深度和 R16G16B16A16 视图空间法线 MRT）。此通道读取深度
//!（+ 可选法线）并写入单通道 R8_UNORM 环境光遮蔽纹理。
//! `ForwardPass` 采样**上一帧的**环境光遮蔽输出（1 帧延迟）以衰减 IBL 漫反射+镜面反射项。
//!
//! ## 资源
//! - 两个 R8_UNORM 环境光遮蔽图像（通过处理中帧索引双缓冲，因此场景可以读取
//!   `ao[(frame+1)%2]`（上一帧的输出），而 GTAO 通道写入 `ao[frame]`（当前帧的输出），
//!   无处理中风险）。
//! - 四个描述符集：按帧索引，集 0 = 深度+采样器，集 1 = 法线+采样器（匹配 Slang 着色器的双集布局）。
//! - 拥有自己的渲染通道+管线（1 个颜色附件，无深度）。
//!
//! ## `execute` 中记录的布局转换
//! 1. 屏障：深度 `DEPTH_STENCIL_ATTACHMENT_OPTIMAL → DEPTH_STENCIL_READ_ONLY_OPTIMAL`
//! 2. 屏障：法线 `COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL`
//! 3. 开始渲染通道（写入 `ao[frame]`）
//! 4. 结束 渲染 pass
//! 5. 屏障 `ao[frame]` `COLOR_ATTACHMENT_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL`
//!
//! The 深度 + 法线 images return to their 附件 layouts at the start of
//! the 下一个 frame's ForwardPass (its 渲染 pass `initial_layout = UNDEFINED`
//! tolerates any incoming 布局 via `load_op = 清空 The 环境光遮蔽 图像 stays in
//! SHADER_READ_ONLY_OPTIMAL until the GTAO pass writes it again two frames
//! later (its 渲染 pass also uses `initial_layout = UNDEFINED`).

use anyhow::Context as _;
use ash::vk;
use std::time::Instant;

use crate::context::VulkanContext;
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderPassNode,
    RenderSettings, ResourceUsage, FORWARD_DEPTH_H, FORWARD_NORMAL_H,
};
use crate::render_pass::find_memory_type;
use crate::shader;
use crate::shader_bindings;

/// Per-frame-in-flight inputs the GTAO pass needs to 样本 内置 by
/// `GraphRenderer::render` from `ForwardPass` accessors and passed to
/// `GtaoPass::execute` alongside the 命令 缓冲区
pub struct GtaoFrameInputs {
    /// 深度 图像 handle (for 布局 barriers).
    pub depth_image: vk::Image,
    /// 深度 视图 (for the 集合 0 SAMPLED_IMAGE 描述符
    pub depth_view: vk::ImageView,
    /// View-space 法线 图像 handle (for 布局 barriers).
    pub normal_image: vk::Image,
    /// 法线 视图 (for the 集合 1 SAMPLED_IMAGE 描述符
    pub normal_view: vk::ImageView,
}

/// Half-resolution GTAO screen-space ambient 遮挡 pass
pub struct GtaoPass {
    /// Half-resolution extent (floor(full / 2)).
    extent: vk::Extent2D,
    /// Double-buffered 环境光遮蔽 images (one per frame-in-flight). The scene reads
    /// `ao[(frame+1)%2]` 最后一个 frame's); GTAO writes `ao[frame]`.
    ao_images: [vk::Image; 2],
    ao_memory: [vk::DeviceMemory; 2],
    ao_views: [vk::ImageView; 2],
    /// 4 描述符 sets, indexed `[frame][set]` where 集合 0 = 深度 集合 1 =
    /// 法线 Each binds one SAMPLED_IMAGE + the shared 采样器
    descriptor_sets: [[vk::DescriptorSet; 2]; 2],
    ds_layout: vk::DescriptorSetLayout,
    ds_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    render_pass: Option<vk::RenderPass>,
    /// One 帧缓冲 per 环境光遮蔽 图像 (each wraps its own `ao_views[i]`).
    framebuffers: [vk::Framebuffer; 2],
    pipeline: Option<GraphicsPipeline>,
    /// The 深度 + 法线 views currently bound to `descriptor_sets[frame]`.
    /// Tracked so we skip 冗余 描述符 rewrites when the same 交换链
    /// image_index repeats across frames-in-flight.
    bound_depth: [vk::ImageView; 2],
    bound_normal: [vk::ImageView; 2],
    device: Option<ash::Device>,
    /// 最后一个 时间 the AO_PROBE 调试 line was logged; the probe is throttled to
    /// once per 秒 so it doesn't flood the 对数 at 帧 rate.
    last_probe_log: Instant,
}

impl GtaoPass {
    /// 创建 the pass + its persistent GPU resources 环境光遮蔽 images, 采样器
    /// 描述符 sets, 渲染 pass 管线 `full_extent` is the 交换链
    /// extent; the pass operates at half 分辨率 `command_pool` is used for
    /// a one-shot 布局 过渡 on the freshly-created 环境光遮蔽 images so the
    /// scene shader's 环境光遮蔽 描述符 (written before GTAO 第一个 runs) finds
    /// them in SHADER_READ_ONLY_OPTIMAL instead of UNDEFINED.
    pub fn new(
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        full_extent: vk::Extent2D,
    ) -> anyhow::Result<Self> {
        let device = &context.device;
        // Half 分辨率 at least 1x1 to avoid zero-sized images.
        let extent = vk::Extent2D {
            width: (full_extent.width / 2).max(1),
            height: (full_extent.height / 2).max(1),
        };

        // ---- Double-buffered R8_UNORM 环境光遮蔽 images ----
        let mut ao_images = [vk::Image::null(); 2];
        let mut ao_memory = [vk::DeviceMemory::null(); 2];
        let mut ao_views = [vk::ImageView::null(); 2];
        for i in 0..2 {
            let (img, mem, view) = create_ao_image(context, extent)?;
            ao_images[i] = img;
            ao_memory[i] = mem;
            ao_views[i] = view;
        }

        // ---- 采样器 线性 clamp-to-edge; 环境光遮蔽 is low-frequency) ----
        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .min_lod(0.0)
                    .max_lod(vk::LOD_CLAMP_NONE),
                None,
            )
        }
        .context("GtaoPass: create sampler")?;

        // ---- 描述符 集合 布局 one SAMPLED_IMAGE 绑定 0) + one
        // 采样器 绑定 1). The 着色器 declares 集合 0 深度 + 集合 1
        // 法线 both with this shape, so we reuse the 布局 4x.
        let per_set_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let ds_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&per_set_bindings),
                None,
            )
        }
        .context("GtaoPass: create ds layout")?;

        // 4 sets 总计 帧 0 集合 0, 帧 0 集合 1, 帧 1 集合 0, 帧 1 集合 1].
        let ds_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(4)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::SAMPLED_IMAGE,
                            descriptor_count: 4,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::SAMPLER,
                            descriptor_count: 4,
                        },
                    ]),
                None,
            )
        }
        .context("GtaoPass: create ds pool")?;

        let layouts = [ds_layout; 4];
        let allocated = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(ds_pool)
                    .set_layouts(&layouts),
            )
        }
        .context("GtaoPass: allocate descriptor sets")?;
        let descriptor_sets = [[allocated[0], allocated[1]], [allocated[2], allocated[3]]];

        // Bind the shared 采样器 to 绑定 1 of every 集合 (the SAMPLED_IMAGE
        // at 绑定 0 is updated per-frame in `set_inputs`).
        for ds in allocated.iter() {
            let sampler_info = vk::DescriptorImageInfo::default().sampler(sampler);
            let write = vk::WriteDescriptorSet::default()
                .dst_set(*ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(std::slice::from_ref(&sampler_info));
            unsafe { device.update_descriptor_sets(&[write], &[]) };
        }

        // ---- 渲染 pass (1 R8 颜色 附件 no 深度 ----
        let render_pass = create_render_pass(device)?;

        // ---- Framebuffers (one per 环境光遮蔽 图像 ----
        let mut framebuffers = [vk::Framebuffer::null(); 2];
        for i in 0..2 {
            let attachments = [ao_views[i]];
            framebuffers[i] = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(render_pass)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
            }
            .context("GtaoPass: create framebuffer")?;
        }

        // ---- 过渡 环境光遮蔽 images to SHADER_READ_ONLY_OPTIMAL ----
        // The 环境光遮蔽 images are created with no defined initial 布局 Before the
        // GTAO pass 第一个 runs 帧 1), the scene shader's 环境光遮蔽 描述符 may
        // already be written pointing at one of these views, expecting
        // SHADER_READ_ONLY_OPTIMAL. 过渡 them up-front so the 描述符
        // 布局 matches even on 帧 0. The GTAO 渲染 pass uses
        // `initial_layout = UNDEFINED`, which tolerates any incoming 布局
        // when it transitions 后 to COLOR_ATTACHMENT_OPTIMAL to 写入
        transition_ao_images_to_shader_read(context, command_pool, [ao_images[0], ao_images[1]])?;

        Ok(Self {
            extent,
            ao_images,
            ao_memory,
            ao_views,
            descriptor_sets,
            ds_layout,
            ds_pool,
            sampler,
            render_pass: Some(render_pass),
            framebuffers,
            pipeline: None,
            bound_depth: [vk::ImageView::null(); 2],
            bound_normal: [vk::ImageView::null(); 2],
            device: Some(device.clone()),
            last_probe_log: Instant::now(),
        })
    }

    /// The half-resolution 环境光遮蔽 extent.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// 环境光遮蔽 图像 格式 (`R8_UNORM`). Exposed for the render-graph visualizer.
    pub fn ao_format() -> vk::Format {
        vk::Format::R8_UNORM
    }

    /// 借用 the 环境光遮蔽 视图 for `frame_index` (frame-in-flight, 0..2). The scene
    /// reads `ao_view((frame + 1) % 2)` to get 最后一个 frame's 输出
    pub fn ao_view(&self, frame_index: u32) -> vk::ImageView {
        self.ao_views[(frame_index as usize) % 2]
    }

    /// 更新 the 深度 + 法线 views bound to `descriptor_sets[frame_index]`.
    /// Skips the 描述符 写入 when both views 匹配 the currently-bound
    /// ones (common case: same 交换链 图像 repeats across frames-in-flight).
    /// Called every 帧 from `GraphRenderer::render` before 执行
    pub fn set_inputs(
        &mut self,
        device: &ash::Device,
        frame_index: u32,
        depth_view: vk::ImageView,
        normal_view: vk::ImageView,
    ) {
        let i = (frame_index as usize) % 2;
        if self.bound_depth[i] == depth_view && self.bound_normal[i] == normal_view {
            return;
        }
        self.bound_depth[i] = depth_view;
        self.bound_normal[i] = normal_view;

        let depth_info = vk::DescriptorImageInfo::default()
            .image_view(depth_view)
            .image_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL);
        let normal_info = vk::DescriptorImageInfo::default()
            .image_view(normal_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.descriptor_sets[i][0])
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(std::slice::from_ref(&depth_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.descriptor_sets[i][1])
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(std::slice::from_ref(&normal_info)),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    /// Rebuild the pass's swapchain-derived resources when the extent changes.
    /// The persistent resources 采样器 ds 布局 渲染 pass 管线 are
    /// kept; only the 环境光遮蔽 images + framebuffers are recreated.
    pub fn recreate_target(
        &mut self,
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        full_extent: vk::Extent2D,
    ) -> anyhow::Result<()> {
        let device = &context.device;
        unsafe { device.device_wait_idle() }.ok();

        let new_extent = half_extent(full_extent);
        if new_extent == self.extent {
            return Ok(());
        }

        // 销毁 old framebuffers + 环境光遮蔽 images.
        for fb in &self.framebuffers {
            unsafe { device.destroy_framebuffer(*fb, None) };
        }
        for i in 0..2 {
            unsafe { device.destroy_image_view(self.ao_views[i], None) };
            unsafe { device.free_memory(self.ao_memory[i], None) };
            unsafe { device.destroy_image(self.ao_images[i], None) };
            self.ao_images[i] = vk::Image::null();
            self.ao_memory[i] = vk::DeviceMemory::null();
            self.ao_views[i] = vk::ImageView::null();
            self.bound_depth[i] = vk::ImageView::null();
            self.bound_normal[i] = vk::ImageView::null();
        }

        // 创建 new 环境光遮蔽 images + framebuffers.
        for i in 0..2 {
            let (img, mem, view) = create_ao_image(context, new_extent)?;
            self.ao_images[i] = img;
            self.ao_memory[i] = mem;
            self.ao_views[i] = view;
            let attachments = [view];
            self.framebuffers[i] = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass.unwrap())
                        .attachments(&attachments)
                        .width(new_extent.width)
                        .height(new_extent.height)
                        .layers(1),
                    None,
                )
            }
            .context("GtaoPass: recreate framebuffer")?;
        }
        self.extent = new_extent;

        // 过渡 the new 环境光遮蔽 images to SHADER_READ_ONLY_OPTIMAL (same
        // rationale as in `new`: the scene's 环境光遮蔽 描述符 expects this 布局
        // before GTAO 第一个 writes the new images).
        transition_ao_images_to_shader_read(
            context,
            command_pool,
            [self.ao_images[0], self.ao_images[1]],
        )?;
        Ok(())
    }

    /// Record the GTAO pass into `cmd`. Must run AFTER `ForwardPass::execute`
    /// (which leaves 深度 in DEPTH_STENCIL_ATTACHMENT_OPTIMAL and 法线 in
    /// COLOR_ATTACHMENT_OPTIMAL).
    pub fn execute(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        frame_index: u32,
        inputs: &GtaoFrameInputs,
        push: &shader_bindings::gtao::GtaoPush,
    ) -> anyhow::Result<()> {
        self.ensure_pipeline(device)?;
        let i = (frame_index as usize) % 2;
        let render_pass = self.render_pass.unwrap();
        let pipeline = self.pipeline.as_ref().unwrap();
        let fb = self.framebuffers[i];

        // The depth/normal -> READ_ONLY barriers used to live here. They are now
        // inserted automatically by `RenderGraph::execute` from the `read_usage`
        // edges declared in `setup` (DEPTH_STENCIL_ATTACHMENT_OPTIMAL ->
        // DEPTH_STENCIL_READ_ONLY_OPTIMAL for 深度 COLOR_ATTACHMENT_OPTIMAL ->
        // SHADER_READ_ONLY_OPTIMAL for 法线 `inputs.depth_image` /
        // `inputs.normal_image` are now unused for barriers (kept for
        // 描述符 wiring in `set_inputs`, which runs in the trait 执行
        let _ = (inputs.depth_image, inputs.normal_image);

        // ---- 开始 渲染 pass (writes ao[i]) ----
        // 清空 to white (1.0 = unoccluded) so any 像素 the 着色器 doesn't
        // 写入 (there shouldn't be any - the fullscreen triangle covers the
        // whole 环境光遮蔽 目标 reads as fully lit.
        let clear_values = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [1.0, 1.0, 1.0, 1.0],
            },
        }];
        let begin_info = vk::RenderPassBeginInfo::default()
            .render_pass(render_pass)
            .framebuffer(fb)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.extent,
            })
            .clear_values(&clear_values);
        unsafe { device.cmd_begin_render_pass(cmd, &begin_info, vk::SubpassContents::INLINE) };

        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
            // 集合 0: 深度 + 采样器
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout,
                0,
                std::slice::from_ref(&self.descriptor_sets[i][0]),
                &[],
            );
            // 集合 1: 法线 + 采样器
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout,
                1,
                std::slice::from_ref(&self.descriptor_sets[i][1]),
                &[],
            );

            device.cmd_set_viewport(
                cmd,
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
            device.cmd_set_scissor(
                cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: self.extent,
                }],
            );

            // 推送 constants: shader_bindings::gtao::GtaoPush (96 字节 片元
            device.cmd_push_constants(
                cmd,
                pipeline.layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                std::slice::from_raw_parts(
                    push as *const _ as *const u8,
                    std::mem::size_of::<shader_bindings::gtao::GtaoPush>(),
                ),
            );

            // Fullscreen triangle (3 verts, no 顶点 缓冲区 - SV_VertexID).
            device.cmd_draw(cmd, 3, 1, 0, 0);
        }

        unsafe { device.cmd_end_render_pass(cmd) };

        // ---- 屏障 ao[i] -> SHADER_READ_ONLY_OPTIMAL ----
        // GRAPH-EDGE 异常 this is a cross-frame delayed edge. `ForwardPass`
        // reads this 环境光遮蔽 图像 *next frame* (1-frame 延迟 wired via
        // `GraphFrame.ao_view` -> `ForwardPass::set_ao`, NOT via a 图
        // `read_usage` edge), so the 渲染 图 cannot express or 调度
        // this 屏障 It stays manual. SHADER_READ_ONLY_OPTIMAL stays 有效
        // until the GTAO pass writes this 槽 again (2 frames later), whose
        // 渲染 pass `initial_layout = UNDEFINED` tolerates the incoming 布局
        let ao_barrier = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.ao_images[i])
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&ao_barrier),
            );
        }

        log::trace!(
            "GtaoPass: wrote AO[{}] into {}x{}",
            i,
            self.extent.width,
            self.extent.height
        );
        Ok(())
    }

    fn ensure_pipeline(&mut self, device: &ash::Device) -> anyhow::Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        let rp = self
            .render_pass
            .context("GtaoPass: render_pass not created before pipeline")?;

        const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/gtao.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/gtao.frag.spv");
        let vert_module =
            shader::load_shader_module(device, VERT_SPV).context("GtaoPass: load vert")?;
        let frag_module =
            shader::load_shader_module(device, FRAG_SPV).context("GtaoPass: load frag")?;

        let vert_entry = std::ffi::CString::new("vertexMain").unwrap();
        let frag_entry = std::ffi::CString::new("fragmentMain").unwrap();
        let vert_stage = shader::shader_stage(
            vk::ShaderStageFlags::VERTEX,
            vert_module,
            vert_entry.as_c_str(),
        );
        let frag_stage = shader::shader_stage(
            vk::ShaderStageFlags::FRAGMENT,
            frag_module,
            frag_entry.as_c_str(),
        );
        let shader_stages = [vert_stage, frag_stage];

        // No 顶点 缓冲区 (fullscreen triangle from SV_VertexID).
        let binding_descs: [vk::VertexInputBindingDescription; 0] = [];
        let attr_descs: [vk::VertexInputAttributeDescription; 0] = [];

        // 集合 0 + 集合 1 share the same 布局 (SAMPLED_IMAGE + 采样器
        let set_layouts = [self.ds_layout, self.ds_layout];

        // 推送 constants: shader_bindings::gtao::GtaoPush (96 字节 片元 only).
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<shader_bindings::gtao::GtaoPush>() as u32)];

        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(false);

        let pipeline = GraphicsPipeline::new(&PipelineDesc {
            device,
            shader_stages: &shader_stages,
            vertex_binding_desc: &binding_descs,
            vertex_attr_descs: &attr_descs,
            descriptor_set_layouts: &set_layouts,
            push_constant_ranges: &push,
            render_pass: rp,
            subpass: 0,
            cull_mode: Some(vk::CullModeFlags::NONE),
            depth_bias_enable: None,
            depth_bias_constant_factor: None,
            depth_bias_slope_factor: None,
            depth_write_enable: Some(false),
            depth_test_enable: None,
            color_attachment_count: None,
            color_blend_attachments: Some(std::slice::from_ref(&blend_attachment)),
            front_face: None,
            polygon_mode: None,
        })
        .context("GtaoPass: create pipeline")?;

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        self.pipeline = Some(pipeline);
        Ok(())
    }

    /// 销毁 all GPU resources. Called from `GraphRenderer::destroy` on
    /// shutdown. `device_wait_idle` must already have been called by the 调用者
    pub fn destroy(&mut self, device: &ash::Device) {
        for fb in &self.framebuffers {
            unsafe { device.destroy_framebuffer(*fb, None) };
        }
        for i in 0..2 {
            if self.ao_views[i] != vk::ImageView::null() {
                unsafe { device.destroy_image_view(self.ao_views[i], None) };
            }
            if self.ao_memory[i] != vk::DeviceMemory::null() {
                unsafe { device.free_memory(self.ao_memory[i], None) };
            }
            if self.ao_images[i] != vk::Image::null() {
                unsafe { device.destroy_image(self.ao_images[i], None) };
            }
        }
        if let Some(rp) = self.render_pass.take() {
            unsafe { device.destroy_render_pass(rp, None) };
        }
        self.pipeline = None;
        unsafe { device.destroy_descriptor_set_layout(self.ds_layout, None) };
        unsafe { device.destroy_descriptor_pool(self.ds_pool, None) };
        unsafe { device.destroy_sampler(self.sampler, None) };
        self.device = None;
    }
}

impl Drop for GtaoPass {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            self.destroy(&device);
        }
    }
}

/// 创建 one R8_UNORM 环境光遮蔽 图像 + 视图 at the given (half-res) extent.
fn create_ao_image(
    context: &VulkanContext,
    extent: vk::Extent2D,
) -> anyhow::Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let device = &context.device;
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8_UNORM)
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
    let image =
        unsafe { device.create_image(&image_info, None) }.context("GtaoPass: create AO image")?;

    let mem_reqs = unsafe { device.get_image_memory_requirements(image) };
    let mem_type = find_memory_type(
        &context.physical_device_memory_properties,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .context("GtaoPass: no suitable memory type for AO image")?;
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mem_type);
    let memory = unsafe { device.allocate_memory(&alloc_info, None) }
        .context("GtaoPass: allocate AO image memory")?;
    unsafe { device.bind_image_memory(image, memory, 0) }
        .context("GtaoPass: bind AO image memory")?;

    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R8_UNORM)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    let view = unsafe { device.create_image_view(&view_info, None) }
        .context("GtaoPass: create AO image view")?;

    Ok((image, memory, view))
}

/// 创建 the GTAO 渲染 pass 1 R8 颜色 附件 清空 -> 存储
/// no 深度 Final 布局 COLOR_ATTACHMENT_OPTIMAL; 执行 barriers to
/// SHADER_READ_ONLY_OPTIMAL after the pass ends so the 访问 masks are
/// correct 附件 finalLayout doesn't carry srcAccessMask).
fn create_render_pass(device: &ash::Device) -> anyhow::Result<vk::RenderPass> {
    let color_attachment = vk::AttachmentDescription::default()
        .format(vk::Format::R8_UNORM)
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

    let dependency = vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

    let rp_create_info = vk::RenderPassCreateInfo::default()
        .attachments(std::slice::from_ref(&color_attachment))
        .subpasses(std::slice::from_ref(&subpass))
        .dependencies(std::slice::from_ref(&dependency));

    let rp = unsafe { device.create_render_pass(&rp_create_info, None) }
        .context("GtaoPass: create render pass")?;
    Ok(rp)
}

/// 过渡 the two 环境光遮蔽 images from UNDEFINED -> SHADER_READ_ONLY_OPTIMAL via
/// a one-shot 命令 缓冲区 Called once at creation (and on recreate) so the
/// scene shader's 环境光遮蔽 描述符 finds the images in the 布局 it declares
/// (`SHADER_READ_ONLY_OPTIMAL`) before the GTAO pass 第一个 writes them.
///
/// GRAPH-EDGE 异常 this is a one-shot resource-creation 过渡 not
/// a per-frame 图 edge. The 渲染 图 only tracks layouts for graph-flow
/// handles (shadow / scene 深度 / 法线 / 高动态范围 颜色 the 环境光遮蔽 images are
/// `GtaoPass`-private and fed 后 to `ForwardPass` via a side-channel
/// (`GraphFrame.ao_view`), so the 图 never sees them.
fn transition_ao_images_to_shader_read(
    context: &VulkanContext,
    command_pool: vk::CommandPool,
    images: [vk::Image; 2],
) -> anyhow::Result<()> {
    let device = &context.device;
    let alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cmd = unsafe { device.allocate_command_buffers(&alloc_info) }
        .context("GtaoPass: allocate transition cmd")?[0];
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { device.begin_command_buffer(cmd, &begin) }
        .context("GtaoPass: begin transition cmd")?;

    let barriers: Vec<vk::ImageMemoryBarrier> = images
        .iter()
        .map(|&img| {
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })
        })
        .collect();
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers,
        );
    }
    unsafe { device.end_command_buffer(cmd) }.context("GtaoPass: end transition cmd")?;

    let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
    let fence = unsafe {
        device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .context("GtaoPass: create transition fence")?
    };
    unsafe {
        device
            .queue_submit(context.graphics_queue, std::slice::from_ref(&submit), fence)
            .context("GtaoPass: submit transition")?;
        device
            .wait_for_fences(&[fence], true, u64::MAX)
            .context("GtaoPass: wait transition fence")?;
        device.destroy_fence(fence, None);
        device.free_command_buffers(command_pool, std::slice::from_ref(&cmd));
    }
    Ok(())
}

impl RenderPassNode for GtaoPass {
    fn name(&self) -> &str {
        "GtaoPass"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // Inputs (depth/normal views) are published by ForwardPass under the
        // well-known FORWARD_DEPTH_H / FORWARD_NORMAL_H handles; GTAO reads them
        // from `resources` in 执行 No graph-managed resources of its own.
        //
        // Declare the 读取 edges so the 渲染 图 inserts the
        // COLOR/DEPTH_ATTACHMENT_OPTIMAL -> *_READ_ONLY / SHADER_READ_ONLY
        // barriers automatically before this pass (replacing the hand-rolled
        // `cmd_pipeline_barrier` that used to live in `draw_ao`).
        graph.read_usage(ResourceUsage {
            handle: FORWARD_DEPTH_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        });
        graph.read_usage(ResourceUsage {
            handle: FORWARD_NORMAL_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
    }

    fn execute(
        &mut self,
        ctx: &RenderContext,
        resources: &mut GraphResources,
    ) -> anyhow::Result<()> {
        let depth_view = match resources.published_view(FORWARD_DEPTH_H) {
            Some(v) => v,
            None => {
                log::warn!("GtaoPass: no ForwardPass depth view published; skipping");
                return Ok(());
            }
        };
        let normal_view = match resources.published_view(FORWARD_NORMAL_H) {
            Some(v) => v,
            None => {
                log::warn!("GtaoPass: no ForwardPass normal view published; skipping");
                return Ok(());
            }
        };
        // TEMP PROBE: confirm GTAO 执行 runs and inputs are 有效 Throttled
        // to once per 秒 so the 对数 isn't flooded at 帧 rate; emitted at
        // 跟踪 level so it stays quiet under the 默认 信息 滤波器
        if self.last_probe_log.elapsed().as_secs_f32() >= 1.0 {
            self.last_probe_log = Instant::now();
            log::trace!(
                "AO_PROBE GTAO: frame={} image={} depth_view={:?} normal_view={:?} ao_write_slot={} ao_read_view={:?} debug_flags=0x{:x}",
                ctx.frame_index,
                ctx.image_index,
                depth_view,
                normal_view,
                ctx.frame_index % 2,
                ctx.frame.ao_view,
                ctx.frame.debug_flags
            );
        }
        // The images themselves are needed only for the 布局 barriers; reuse
        // the view's 图像 handle via the ForwardPass-published 视图 (vkImageView
        // carries the 图像 We pass the same handle for 图像 + 视图 the
        // 屏障 only needs a 有效 图像 and `vk::Image` from a 视图 is not
        // directly available here, so we use the ForwardPass-published 图像 via
        // the 资源 table's 图像 (if present) else fall 后 to the 视图
        let depth_image = resources
            .published_image(FORWARD_DEPTH_H)
            .unwrap_or(vk::Image::null());
        let normal_image = resources
            .published_image(FORWARD_NORMAL_H)
            .unwrap_or(vk::Image::null());

        // 更新 the 深度 + 法线 描述符 sets for this frame-in-flight.
        // This used to be called from `GraphRenderer::render` before PR-1; it
        // MUST happen before 执行 (which binds the 描述符 sets and
        // draws), otherwise 验证 reports `depthTex` / `normalTex` as
        // never updated via vkUpdateDescriptorSets.
        self.set_inputs(ctx.device, ctx.frame_index, depth_view, normal_view);

        let gtao_extent = self.extent();
        let inputs = GtaoFrameInputs {
            depth_image,
            depth_view,
            normal_image,
            normal_view,
        };
        let push = shader_bindings::gtao::GtaoPush {
            inv_proj: ctx.frame.inv_projection,
            viewport: [gtao_extent.width as f32, gtao_extent.height as f32],
            radius: 0.5,
            mode: 0,
            _pad0: 0,
        };
        self.execute(ctx.device, ctx.cmd, ctx.frame_index, &inputs, &push)
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Gtao,
            // 深度 + 法线 come from ForwardPass via the well-known handles.
            inputs: vec![FORWARD_DEPTH_H, FORWARD_NORMAL_H],
            // 环境光遮蔽 is consumed by ForwardPass via `set_ao` (1-frame 延迟 not a
            // 图 edge - surfaced as a 音符 by the viz instead.
            outputs: Vec::new(),
        }
    }

    fn warmup(&mut self, device: &ash::Device, _context: &VulkanContext) -> anyhow::Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        self.ensure_pipeline(device)
    }
}

/// AO 目标是交换链尺寸的一半（且至少 1×1）。
///
/// 抽成纯函数是因为 `recreate_target` 用「新尺寸 == 当前尺寸」做早退：只要调用方
/// 在 `Swapchain::recreate` **之前**采样交换链 extent，算出来的半分辨率就
/// 与当前值相同，整个 resize 会被静默跳过。
pub(crate) fn half_extent(full: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: (full.width / 2).max(1),
        height: (full.height / 2).max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ao_target_is_half_the_swapchain_extent() {
        assert_eq!(
            half_extent(vk::Extent2D {
                width: 1280,
                height: 720,
            }),
            vk::Extent2D {
                width: 640,
                height: 360,
            }
        );
    }

    #[test]
    fn tiny_extents_clamp_to_one_pixel() {
        assert_eq!(
            half_extent(vk::Extent2D {
                width: 1,
                height: 0,
            }),
            vk::Extent2D {
                width: 1,
                height: 1,
            }
        );
    }

    /// 回归守卫：喂给 `recreate_target` 的必须是**新**交换链的 extent。
    /// 用旧 extent 算出的半分辨率等于当前值，早退会把 resize 吞掉。
    #[test]
    fn stale_swapchain_extent_would_early_return() {
        let current = half_extent(vk::Extent2D {
            width: 1920,
            height: 1080,
        });
        let stale = half_extent(vk::Extent2D {
            width: 1920,
            height: 1080,
        });
        let fresh = half_extent(vk::Extent2D {
            width: 1600,
            height: 900,
        });
        assert_eq!(stale, current, "旧 extent 会命中「尺寸未变」早退");
        assert_ne!(fresh, current, "新 extent 必须真正触发重建");
    }
}
