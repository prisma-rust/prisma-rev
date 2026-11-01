//! 后处理通道——HDR 场景色调映射（+ 可选 FXAA）→ sRGB 交换链
//!
//! 全屏三角形片元通道，对 ForwardPass 的 HDR 中间颜色附件进行采样，
//! 应用 Reinhard 或 ACES 色调映射（根据 `tonemap_mode`），
//! 并将结果写入交换链图像。取代了之前位于 `scene_frag.slang` 中的内联色调映射，
//! 使场景输出保持线性 HDR（可被未来的后处理效果使用：泛光、时域抗锯齿等）。
//!
//! `aa_mode = 1` 时在色调映射之后再做一遍 FXAA（后处理抗锯齿，见 #494）：
//! 只在本通道内多采样几次已经色调映射过的颜色，**不改任何管线状态**，也
//! 不需要多采样目标 / resolve，因此交换链与离屏（编辑器视口）路径拿到的是
//! 同一份结果。调试图（`debug_rt != 0`）会强制关掉它。
//!
//! ## 资源
//! - 一个描述符集，将 HDR 颜色绑定为组合图像采样器
//! - 拥有自己的渲染通道+管线（1 个颜色附件 = 交换链格式，无深度）
//! - HDR 输入视图每帧通过 `set_input` 更新（ForwardPass 每交换链槽轮换一个 HDR 图像，
//!   匹配其刚写入的帧缓冲）。
//!
//! ## `execute` 中记录的布局转换
//! 1. 屏障：HDR `COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL`
//! 2. 屏障：交换链 `UNDEFINED → COLOR_ATTACHMENT_OPTIMAL`（通过渲染通道的
//! 加载 op).
//! 3. 开始 渲染 pass (writes 交换链 绘制 fullscreen triangle, 结束
//! 4. The 调用者 (GraphRenderer::render) barriers 交换链
//! `COLOR_ATTACHMENT_OPTIMAL -> PRESENT_SRC_KHR` (or the external overlay does
//! it via its own load+transition pass

use anyhow::Context as _;
use ash::vk;

use crate::context::VulkanContext;
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderMode,
    RenderPassNode, RenderSettings, ResourceHandle, ResourceUsage, FORWARD_COLOR_H,
    FORWARD_DEPTH_H, FORWARD_NORMAL_H, PT_COLOR_H,
};
use crate::render_pass::find_memory_type;
use crate::shader;
use crate::shader_bindings;

/// Fullscreen-triangle 色调映射 pass 高动态范围 scene 颜色 -> sRGB 交换链
pub struct PostPass {
    render_pass: Option<vk::RenderPass>,
    /// 交换链 颜色 格式 集合 in `new`, used to rebuild the 渲染 pass on
    /// `drop_target`/`set_target`). Stored so the visualizer can 读取 it.
    color_format: vk::Format,
    /// Layout the render pass leaves the color attachment in.
    ///
    /// Window targets end in `PRESENT_SRC_KHR` so the swapchain image can be
    /// presented; offscreen targets stay in `COLOR_ATTACHMENT_OPTIMAL` — a
    /// headless `VulkanContext` never enables `VK_KHR_swapchain` (no surface),
    /// and `PRESENT_SRC_KHR` is not a valid layout without it.
    final_layout: vk::ImageLayout,
    /// One 帧缓冲 per 交换链 图像 (each wraps its 交换链 视图
    framebuffers: Vec<Option<vk::Framebuffer>>,
    /// Cached 交换链 views the framebuffers were 内置 against (for
    /// rebuild detection, mirroring ForwardPass's 模式
    target_views: Vec<vk::ImageView>,
    extent: vk::Extent2D,
    pipeline: Option<GraphicsPipeline>,
    /// One 描述符 集合 per frame-in-flight so `set_input` can 更新 帧
    /// N's 集合 without disturbing 帧 N-1's still-in-flight 集合
    /// (VUID-vkUpdateDescriptorSets-None-03047). Each binds the 高动态范围 颜色 视图
    /// as a combined 图像 采样器
    descriptor_sets: Vec<vk::DescriptorSet>,
    ds_layout: vk::DescriptorSetLayout,
    ds_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    /// 最近邻采样器（`InternalScaleMode::Integer`，#613）：像素画放大不糊。
    sampler_nearest: vk::Sampler,
    /// The 高动态范围 视图 currently bound to each frame-in-flight's 描述符 集合
    /// Tracked so we skip 冗余 描述符 rewrites.
    bound_hdrs: Vec<vk::ImageView>,
    /// 每个 in-flight 帧当前绑定的是否是最近邻采样器（与 `bound_hdrs` 一起判缓存）。
    bound_nearest: Vec<bool>,
    device: Option<ash::Device>,
}

impl PostPass {
    /// 创建 the pass + persistent resources 采样器 ds layout/pool/sets,
    /// 渲染 pass `color_format` is the 交换链 格式 `frames_in_flight`
    /// is the number of 描述符 sets to allocate (one per 并发 帧
    /// so `set_input` doesn't disturb an in-flight 集合 The 管线 +
    /// framebuffers are created lazily once a 渲染 pass + 目标 exist.
    ///
    /// `final_layout` is where the render pass leaves the color attachment:
    /// `PRESENT_SRC_KHR` for a swapchain image, `COLOR_ATTACHMENT_OPTIMAL` for
    /// an offscreen target (see [`Self::final_layout`]).
    pub fn new(
        context: &VulkanContext,
        color_format: vk::Format,
        frames_in_flight: u32,
        final_layout: vk::ImageLayout,
    ) -> anyhow::Result<Self> {
        let device = &context.device;

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
        .context("PostPass: create sampler")?;

        let sampler_nearest = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .min_lod(0.0)
                    .max_lod(vk::LOD_CLAMP_NONE),
                None,
            )
        }
        .context("PostPass: create nearest sampler")?;

        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let ds_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .context("PostPass: create ds layout")?;

        let fif = frames_in_flight.max(1);
        let ds_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(fif)
                    .pool_sizes(&[vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        descriptor_count: fif,
                    }]),
                None,
            )
        }
        .context("PostPass: create ds pool")?;

        let layouts = vec![ds_layout; fif as usize];
        let descriptor_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(ds_pool)
                    .set_layouts(&layouts),
            )
        }
        .context("PostPass: allocate ds")?;
        let descriptor_sets: Vec<vk::DescriptorSet> = descriptor_sets;

        let render_pass = create_render_pass(device, color_format, final_layout)?;

        Ok(Self {
            render_pass: Some(render_pass),
            color_format,
            final_layout,
            framebuffers: Vec::new(),
            target_views: Vec::new(),
            extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
            pipeline: None,
            descriptor_sets,
            ds_layout,
            ds_pool,
            sampler,
            sampler_nearest,
            bound_hdrs: vec![vk::ImageView::null(); fif as usize],
            bound_nearest: vec![false; fif as usize],
            device: Some(device.clone()),
        })
    }

    /// 交换链 extent PostPass tonemaps into. Exposed for the visualizer.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// 交换链 颜色 格式 PostPass targets. Exposed for the visualizer.
    pub fn color_format(&self) -> vk::Format {
        self.color_format
    }

    /// Layout the render pass leaves the target image in.
    ///
    /// `PRESENT_SRC_KHR` for swapchain targets, `COLOR_ATTACHMENT_OPTIMAL` for
    /// offscreen targets. Exposed so tests can assert a headless renderer never
    /// asks for a swapchain-only layout.
    pub fn final_layout(&self) -> vk::ImageLayout {
        self.final_layout
    }

    /// Ensure the 帧缓冲 for `image_index` 存在 and is 内置 against the
    /// 当前 交换链 views + extent. Mirrors ForwardPass::set_target's
    /// per-slot rebuild 逻辑 (only rebuild an entry when its 视图 changes, so
    /// in-flight framebuffers are never touched).
    pub fn set_target(
        &mut self,
        device: &ash::Device,
        swapchain_views: &[vk::ImageView],
        image_index: u32,
        extent: vk::Extent2D,
    ) -> anyhow::Result<()> {
        if extent.width == 0 || extent.height == 0 {
            return Ok(());
        }
        let idx = image_index as usize;
        if idx >= swapchain_views.len() {
            return Ok(());
        }
        let view = swapchain_views[idx];

        let swapchain_changed = self.target_views.len() != swapchain_views.len()
            || self.extent != extent
            || self
                .target_views
                .iter()
                .zip(swapchain_views.iter())
                .any(|(a, b)| a != b);
        if swapchain_changed {
            self.drop_target(device);
            self.target_views = swapchain_views.to_vec();
            self.extent = extent;
            self.framebuffers = (0..swapchain_views.len()).map(|_| None).collect();
        }

        let already_current = idx < self.target_views.len()
            && self.target_views[idx] == view
            && self.framebuffers[idx].is_some();
        if !already_current {
            let rp = self
                .render_pass
                .context("PostPass: render_pass missing in set_target")?;
            if let Some(old_fb) = self.framebuffers[idx].take() {
                unsafe { device.destroy_framebuffer(old_fb, None) };
            }
            let attachments = [view];
            let fb = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(rp)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
            }
            .context("PostPass: create framebuffer")?;
            self.framebuffers[idx] = Some(fb);
            self.target_views[idx] = view;
        }
        Ok(())
    }

    /// 放置 the swapchain-derived framebuffers (called before 交换链
    /// recreate, mirroring ForwardPass::drop_target).
    pub fn drop_target(&mut self, device: &ash::Device) {
        for fb in self.framebuffers.drain(..).flatten() {
            unsafe { device.destroy_framebuffer(fb, None) };
        }
        self.target_views.clear();
        self.extent = vk::Extent2D {
            width: 0,
            height: 0,
        };
    }

    /// 更新 the 高动态范围 输入 视图 bound to the frame-in-flight's 描述符 集合
    /// Called every 帧 from `GraphRenderer::render` before 执行
    /// Bind 视图 (sampled with `image_layout`) as the 输入 纹理 for this
    /// frame-in-flight's 描述符 集合 Skips the 写入 when 视图 matches
    /// the currently-bound one. `image_layout` must 匹配 the image's actual
    /// 布局 at 绘制 时间 深度 uses `DEPTH_STENCIL_READ_ONLY_OPTIMAL`,
    /// color/normal use `SHADER_READ_ONLY_OPTIMAL`).
    pub fn set_input(
        &mut self,
        device: &ash::Device,
        frame_index: u32,
        view: vk::ImageView,
        image_layout: vk::ImageLayout,
        nearest: bool,
    ) {
        let i = (frame_index as usize) % self.descriptor_sets.len();
        if view == self.bound_hdrs[i] && nearest == self.bound_nearest[i] {
            return;
        }
        self.bound_hdrs[i] = view;
        self.bound_nearest[i] = nearest;
        let image_info = vk::DescriptorImageInfo::default()
            .image_view(view)
            .sampler(if nearest {
                self.sampler_nearest
            } else {
                self.sampler
            })
            .image_layout(image_layout);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.descriptor_sets[i])
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&image_info));
        unsafe { device.update_descriptor_sets(&[write], &[]) };
    }

    /// Record the PostPass into `cmd`. Must run AFTER ForwardPass (which leaves
    /// the 高动态范围 颜色 in COLOR_ATTACHMENT_OPTIMAL). The 调用者 barriers the
    /// 交换链 to PRESENT_SRC_KHR (or the external overlay handles it) after this.
    /// `frame_index` selects the per-frame-in-flight 描述符 集合 `image_index`
    /// selects the per-swapchain-image 帧缓冲
    pub fn execute(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        frame_index: u32,
        image_index: u32,
        hdr_image: vk::Image,
        dest: vk::Rect2D,
        push: &shader_bindings::post::PostPush,
    ) -> anyhow::Result<()> {
        self.ensure_pipeline(device)?;
        let rp = self.render_pass.unwrap();
        let pipeline = self.pipeline.as_ref().unwrap();
        let fb = self
            .framebuffers
            .get(image_index as usize)
            .copied()
            .flatten()
            .context("PostPass: no framebuffer for image_index (call set_target first)")?;
        let ds = self
            .descriptor_sets
            .get(frame_index as usize)
            .copied()
            .context("PostPass: no descriptor set for frame_index")?;

        // The 高动态范围 输入 COLOR_ATTACHMENT_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL
        // 屏障 used to live here. It is now inserted automatically by
        // `RenderGraph::execute` from the `read_usage` edge declared in
        // `setup`. `hdr_image` is therefore no longer needed in this 函数
        // (it was only referenced by the deleted 屏障
        let _ = hdr_image;

        // The 交换链 图像 transitions UNDEFINED -> COLOR_ATTACHMENT_OPTIMAL
        // via the 渲染 pass `initial_layout` (the external overlay or the caller's
        // PRESENT_SRC_KHR 屏障 handles the final 过渡 out).
        let clear_values = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        }];
        let begin_info = vk::RenderPassBeginInfo::default()
            .render_pass(rp)
            .framebuffer(fb)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.extent,
            })
            .clear_values(&clear_values);
        unsafe { device.cmd_begin_render_pass(cmd, &begin_info, vk::SubpassContents::INLINE) };

        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout,
                0,
                std::slice::from_ref(&ds),
                &[],
            );
            // #613：内部分辨率按整数倍/拉伸放到 `dest`，其余区域由清屏色填黑边。
            device.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: dest.offset.x as f32,
                    y: dest.offset.y as f32,
                    width: dest.extent.width as f32,
                    height: dest.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            device.cmd_set_scissor(cmd, 0, &[dest]);
            device.cmd_push_constants(
                cmd,
                pipeline.layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                std::slice::from_raw_parts(
                    push as *const _ as *const u8,
                    std::mem::size_of::<shader_bindings::post::PostPush>(),
                ),
            );
            // Fullscreen triangle (3 verts, no 顶点 缓冲区 - SV_VertexID).
            device.cmd_draw(cmd, 3, 1, 0, 0);
        }

        unsafe { device.cmd_end_render_pass(cmd) };

        log::trace!(
            "PostPass: tonemapped HDR -> swapchain image {} ({}x{})",
            image_index,
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
            .context("PostPass: render_pass not created before pipeline")?;

        const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/post.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/post.frag.spv");
        let vert_module =
            shader::load_shader_module(device, VERT_SPV).context("PostPass: load vert")?;
        let frag_module =
            shader::load_shader_module(device, FRAG_SPV).context("PostPass: load frag")?;

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

        let binding_descs: [vk::VertexInputBindingDescription; 0] = [];
        let attr_descs: [vk::VertexInputAttributeDescription; 0] = [];
        let set_layouts = [self.ds_layout];

        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<shader_bindings::post::PostPush>() as u32)];

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
        .context("PostPass: create pipeline")?;

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }
        self.pipeline = Some(pipeline);
        Ok(())
    }

    /// 销毁 all GPU resources. Called from GraphRenderer::destroy on
    /// shutdown. `device_wait_idle` must already have been called by the 调用者
    pub fn destroy(&mut self, device: &ash::Device) {
        self.drop_target(device);
        if let Some(rp) = self.render_pass.take() {
            unsafe { device.destroy_render_pass(rp, None) };
        }
        self.pipeline = None;
        unsafe { device.destroy_descriptor_set_layout(self.ds_layout, None) };
        unsafe { device.destroy_descriptor_pool(self.ds_pool, None) };
        unsafe { device.destroy_sampler(self.sampler, None) };
        unsafe { device.destroy_sampler(self.sampler_nearest, None) };
        self.device = None;
    }
}

impl Drop for PostPass {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            self.destroy(&device);
        }
    }
}

impl RenderPassNode for PostPass {
    fn name(&self) -> &str {
        "PostPass"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // 高动态范围 输入 is published by ForwardPass under FORWARD_COLOR_H; 读取 in
        // 执行 PostPass owns no graph-managed resources of its own.
        //
        // Declare the 读取 edge so the 渲染 图 inserts the
        // COLOR_ATTACHMENT_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL 屏障
        // automatically before this pass (replacing the hand-rolled
        // `cmd_pipeline_barrier` that used to live in 执行
        graph.read_usage(ResourceUsage {
            handle: FORWARD_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        // PT 输出 颜色 读取 in path-trace 众数 Declare unconditionally
        // so the 图 reserves the 屏障 槽 even when in 光栅化 众数
        graph.read_usage(ResourceUsage {
            handle: PT_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        // 调试 RT viewer (Tab) can also 样本 the scene 深度 众数 1) and
        // view-space 法线 众数 2). Declare these 读取 edges unconditionally
        // so the automatic 屏障 管线 keeps them in a sampled 布局
        // even when 众数 0 doesn't 读取 them (GTAO already transitions 深度
        // and 法线 to read-only layouts, so this is usually a cache hit and
        // emits no extra 屏障
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
        // Pick the 输入 RT based on the 渲染 众数 and 调试 viewer (Tab).
        // In path-trace 众数 we 读取 PT_COLOR_H instead of FORWARD_COLOR_H,
        // unless no usable 相机 存在 — in that case the PT pass was skipped
        // so fall 后 to FORWARD_COLOR_H (the gray 清空 颜色 from ForwardPass).
        // If PT_COLOR_H was not published (e.g. no RT hardware, pass skipped,
        // or geometry/materials not yet uploaded), also fall back to FORWARD_COLOR_H
        // so PostPass can tonemap and transition the swapchain image to PRESENT_SRC_KHR.
        // 调试 modes 1 深度 and 2 法线 always 读取 from scene 输出
        let is_pt = ctx.frame.render_mode == RenderMode::PathTrace;
        let has_camera = ctx.frame.has_camera;
        let has_pt_view = resources.published_view(PT_COLOR_H).is_some();
        let (handle, image_layout) =
            select_input_target(ctx.frame.debug_rt, is_pt, has_camera, has_pt_view);
        let input_view = match resources.published_view(handle) {
            Some(v) => v,
            None => {
                log::warn!("PostPass: no {:?} view published; skipping", handle);
                return Ok(());
            }
        };
        let input_image = resources
            .published_image(handle)
            .unwrap_or(vk::Image::null());

        // (Re)build this 交换链 image's 帧缓冲 if 缺少 or the
        // 交换链 changed - mirrors `ForwardPass::ensure_target`. Before PR-1
        // this was `GraphRenderer`'s 作业 (it called `set_target` every 帧
        // now the 图 drives it so the 帧缓冲 lifecycle is owned here.
        self.set_target(
            ctx.device,
            ctx.frame.swapchain_views,
            ctx.image_index,
            ctx.extent,
        )
        .context("PostPass: set_target")?;

        // Bind the selected 输入 视图 into this frame's 描述符 集合 The
        // cache (`bound_hdrs`) keys on the 视图 handle, so switching modes
        // (which changes the 视图 triggers a rewrite automatically.
        // #613：内部渲染分辨率 → 目标矩形与采样方式。
        let nearest = ctx.settings.internal_scale_mode
            == crate::render_graph::InternalScaleMode::Integer
            && ctx.render_extent != ctx.extent;
        let dest = crate::render_graph::post_destination(
            ctx.extent,
            ctx.render_extent,
            ctx.settings.internal_scale_mode,
        );
        self.set_input(
            ctx.device,
            ctx.frame_index,
            input_view,
            image_layout,
            nearest,
        );

        // The generated PostPush carries tonemapMode (Reinhard / ACES /
        // passthrough), aaMode (0 = off, 1 = FXAA) and invTexelSize (one texel in
        // UV units, used by the FXAA taps). debug_rt, proj22/proj32, near/far
        // etc. are consumed on the Rust side 输入 绑定 selection,
        // 深度 linearization) and are NOT part of the GPU 推送 常量
        // proj22/proj32/near/far 深度 linearization values are still computed
        // here as a 引用 for future 着色器 extensions.
        let _proj22 = ctx.frame.proj22;
        let _proj32 = ctx.frame.proj32;
        let _near = if (_proj22 - 1.0).abs() > 1e-6 {
            _proj32 / (_proj22 + 1.0)
        } else {
            0.1
        };
        let _far = if (_proj22 - 1.0).abs() > 1e-6 {
            _proj32 / (_proj22 - 1.0)
        } else {
            100.0
        };
        let push = shader_bindings::post::PostPush {
            tonemapMode: ctx.frame.tonemap_mode,
            aaMode: effective_aa_mode(ctx.frame.debug_rt, ctx.frame.aa_mode),
            contrast: ctx.frame.grade.contrast,
            saturation: ctx.frame.grade.saturation,
            vignette: ctx.frame.grade.vignette,
            // #613：FXAA 采样的是**内部渲染目标**，一个 texel 的 UV 尺寸按它的
            // 分辨率算；用交换链尺寸会在内部分辨率缩小时把采样步长算小。
            invTexelSize: [
                if ctx.render_extent.width > 0 {
                    1.0 / ctx.render_extent.width as f32
                } else {
                    0.0
                },
                if ctx.render_extent.height > 0 {
                    1.0 / ctx.render_extent.height as f32
                } else {
                    0.0
                },
            ],
        };
        self.execute(
            ctx.device,
            ctx.cmd,
            ctx.frame_index,
            ctx.image_index,
            input_image,
            dest,
            &push,
        )
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Post,
            // 高动态范围 颜色 comes from ForwardPass via FORWARD_COLOR_H.
            inputs: vec![FORWARD_COLOR_H],
            // PostPass writes the 交换链 (not a graph-managed 资源
            outputs: Vec::new(),
        }
    }

    fn warmup(&mut self, device: &ash::Device, _context: &VulkanContext) -> anyhow::Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        if self.render_pass.is_none() {
            self.render_pass = Some(create_render_pass(
                device,
                self.color_format,
                self.final_layout,
            )?);
        }
        self.ensure_pipeline(device)
    }
}

/// 创建 the PostPass 渲染 pass 1 color-format 颜色 附件
/// 清空 -> 存储 no 深度 `initial_layout = UNDEFINED` so the GPU
/// transitions the target 图像 from whatever 布局 it was in (typically
/// `PRESENT_SRC_KHR` from 最后一个 帧 into COLOR_ATTACHMENT_OPTIMAL as part of
/// the 加载 op.
///
/// `final_layout` is supplied by the caller: `PRESENT_SRC_KHR` for a swapchain
/// image, `COLOR_ATTACHMENT_OPTIMAL` for an offscreen image. Headless contexts
/// do not enable `VK_KHR_swapchain`, so the swapchain layout would be an
/// invalid enum value there (`VUID-VkAttachmentDescription-finalLayout-parameter`).
fn create_render_pass(
    device: &ash::Device,
    format: vk::Format,
    final_layout: vk::ImageLayout,
) -> anyhow::Result<vk::RenderPass> {
    let color_attachment = vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(final_layout);

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
        .context("PostPass: create render pass")?;
    Ok(rp)
}

// Re-export the memory-type finder so this 模块 is self-contained for
// future HDR-image helpers (currently none - the 高动态范围 图像 is owned by
// ForwardPass). Kept here as a placeholder 导入 to avoid an unused 警告
// if no callers use it.
#[allow(dead_code)]
fn _memory_type_for_hdr(context: &VulkanContext, mem_type_bits: u32) -> anyhow::Result<u32> {
    find_memory_type(
        &context.physical_device_memory_properties,
        mem_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .context("PostPass: no suitable memory type for HDR image")
}

/// Anti-aliasing mode actually handed to the shader.
///
/// The debug render-target viewer (`debug_rt` 1 = depth, 2 = view-space normal)
/// shows a diagnostic image: FXAA would blend neighbouring texels and hide
/// exactly the stair-stepping those views exist to expose, so it is forced off
/// there.
pub(crate) fn effective_aa_mode(debug_rt: u32, aa_mode: u32) -> u32 {
    if debug_rt == 0 {
        aa_mode
    } else {
        0
    }
}

/// Helper to select the input resource handle and expected image layout for PostPass.
///
/// In path-trace mode we read PT_COLOR_H instead of FORWARD_COLOR_H, unless no usable camera
/// exists or PT_COLOR_H was not published (e.g. acceleration structures unsupported, or PT pass
/// skipped). In that case, falls back to FORWARD_COLOR_H so PostPass can tonemap and transition
/// the presentation image properly.
pub(crate) fn select_input_target(
    debug_rt: u32,
    is_pt: bool,
    has_camera: bool,
    has_pt_view: bool,
) -> (ResourceHandle, vk::ImageLayout) {
    match debug_rt {
        1 => (
            FORWARD_DEPTH_H,
            vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        ),
        2 => (FORWARD_NORMAL_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        _ if is_pt && has_camera && has_pt_view => {
            (PT_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        }
        _ if is_pt && has_camera => {
            log::warn!(
                "PostPass: PT_COLOR_H requested but not published; falling back to FORWARD_COLOR_H"
            );
            (FORWARD_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        }
        _ => (FORWARD_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_input_target_raster() {
        let (handle, layout) = select_input_target(0, false, true, false);
        assert_eq!(handle, FORWARD_COLOR_H);
        assert_eq!(layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    #[test]
    fn test_select_input_target_path_trace_with_pt_view() {
        let (handle, layout) = select_input_target(0, true, true, true);
        assert_eq!(handle, PT_COLOR_H);
        assert_eq!(layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    #[test]
    fn test_select_input_target_path_trace_fallback_when_view_missing() {
        // PT requested with camera, but PT_COLOR_H was not published -> fall back to FORWARD_COLOR_H
        let (handle, layout) = select_input_target(0, true, true, false);
        assert_eq!(handle, FORWARD_COLOR_H);
        assert_eq!(layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    #[test]
    fn test_select_input_target_path_trace_no_camera() {
        // PT requested without camera -> forward color
        let (handle, layout) = select_input_target(0, true, false, false);
        assert_eq!(handle, FORWARD_COLOR_H);
        assert_eq!(layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    #[test]
    fn test_effective_aa_mode_is_off_for_debug_views() {
        assert_eq!(effective_aa_mode(0, 0), 0);
        assert_eq!(effective_aa_mode(0, 1), 1);
        // Depth / view-space normal debug views stay unfiltered on purpose.
        assert_eq!(effective_aa_mode(1, 1), 0);
        assert_eq!(effective_aa_mode(2, 1), 0);
    }

    #[test]
    fn test_post_push_layout_matches_the_shader() {
        use shader_bindings::post::{PostPush, PUSH_CONSTANT_SIZE};
        assert_eq!(std::mem::offset_of!(PostPush, tonemapMode), 0);
        assert_eq!(std::mem::offset_of!(PostPush, aaMode), 4);
        assert_eq!(std::mem::offset_of!(PostPush, invTexelSize), 8);
        // #502 color grading rides in the same push-constant block.
        assert_eq!(std::mem::offset_of!(PostPush, contrast), 16);
        assert_eq!(std::mem::offset_of!(PostPush, saturation), 20);
        assert_eq!(std::mem::offset_of!(PostPush, vignette), 24);
        assert_eq!(std::mem::size_of::<PostPush>(), 28);
        assert_eq!(PUSH_CONSTANT_SIZE, 28);
    }

    #[test]
    fn test_select_input_target_debug_modes() {
        let (h1, l1) = select_input_target(1, true, true, true);
        assert_eq!(h1, FORWARD_DEPTH_H);
        assert_eq!(l1, vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL);

        let (h2, l2) = select_input_target(2, true, true, true);
        assert_eq!(h2, FORWARD_NORMAL_H);
        assert_eq!(l2, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }
}
