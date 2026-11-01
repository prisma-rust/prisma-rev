//! RenderTexture 消费端示例 pass：全屏采样（bindless）→ swapchain。
//!
//! 演示引擎式 RT 消费：从 [`crate::rt_scheduler::RT_OUTPUT_H`] 拿调度器
//! 发布的 bindless 槽位塞进 push constant，shader 从 `bindlessSrvs[]` 直接
//! 采样 —— 与采样普通纹理无差别。RT 由 [`crate::rt_scheduler::RenderTextureScheduler`]
//! 统一渲染，本 pass 只读不写，与 scene 零耦合。

use anyhow::Context as _;
use ash::vk;
use std::ffi::CString;

use crate::context::VulkanContext;
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderPassNode,
    RenderSettings, ResourceUsage,
};
use crate::rt_scheduler::RT_OUTPUT_H;
use crate::shader::{load_shader_module, shader_stage};
use crate::shader_bindings::rt_preview::{
    RtPreviewPush, ENTRY_FRAGMENT_MAIN as PREVIEW_FRAG, ENTRY_VERTEX_MAIN as PREVIEW_VERT,
};

const RT_PREVIEW_VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/rt_preview.vert.spv");
const RT_PREVIEW_FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/rt_preview.frag.spv");

/// 全屏采样 RenderTexture（bindless）→ swapchain 的消费 pass。
pub struct RtPreviewPass {
    /// 克隆的 device（Drop 时销毁）。
    device: Option<ash::Device>,
    color_format: vk::Format,
    /// bindless 表 layout（本 pass 的 set 0），由 GraphRenderer 注入（管线创建用）。
    bindless_layout: Option<vk::DescriptorSetLayout>,
    /// bindless 表 set（execute 时绑定 set 0）。
    bindless_set: Option<vk::DescriptorSet>,
    render_pass: Option<vk::RenderPass>,
    /// per-swapchain-image framebuffers（复用 post/ui_overlay 模式）。
    framebuffers: Vec<Option<vk::Framebuffer>>,
    target_views: Vec<vk::ImageView>,
    extent: vk::Extent2D,
    pipeline: Option<GraphicsPipeline>,
}

impl RtPreviewPass {
    pub fn new(context: &VulkanContext, color_format: vk::Format) -> Self {
        Self {
            device: Some(context.device.clone()),
            color_format,
            bindless_layout: None,
            bindless_set: None,
            render_pass: None,
            framebuffers: Vec::new(),
            target_views: Vec::new(),
            extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
            pipeline: None,
        }
    }

    /// 注入 bindless 表 layout（GraphRenderer 构建 graph 后调用）。
    pub fn set_bindless_layout(&mut self, layout: vk::DescriptorSetLayout) {
        self.bindless_layout = Some(layout);
    }

    /// 注入 bindless 表 set（execute 时绑定 set 0）。
    pub fn set_bindless_set(&mut self, set: vk::DescriptorSet) {
        self.bindless_set = Some(set);
    }

    fn ensure_render_pass(&mut self, device: &ash::Device) -> anyhow::Result<()> {
        if self.render_pass.is_none() {
            // 全屏覆盖：CLEAR 得到独立画面（RT 内容即整屏）。
            let color_attachment = vk::AttachmentDescription::default()
                .format(self.color_format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);
            let color_ref = vk::AttachmentReference::default()
                .attachment(0)
                .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
            let subpass = vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(std::slice::from_ref(&color_ref));
            let create_info = vk::RenderPassCreateInfo::default()
                .attachments(std::slice::from_ref(&color_attachment))
                .subpasses(std::slice::from_ref(&subpass));
            let rp = unsafe { device.create_render_pass(&create_info, None) }
                .context("RtPreviewPass: create render pass")?;
            self.render_pass = Some(rp);
        }
        Ok(())
    }

    fn ensure_pipeline(&mut self, device: &ash::Device) -> anyhow::Result<()> {
        if self.pipeline.is_none() {
            let layout = self
                .bindless_layout
                .context("RtPreviewPass: set_bindless_layout not called")?;
            let rp = self.render_pass.context("render pass missing")?;

            let vert_module = load_shader_module(device, RT_PREVIEW_VERT_SPV)
                .context("RtPreviewPass: load vert module")?;
            let frag_module = load_shader_module(device, RT_PREVIEW_FRAG_SPV)
                .context("RtPreviewPass: load frag module")?;
            let vert_entry = CString::new(PREVIEW_VERT).unwrap();
            let frag_entry = CString::new(PREVIEW_FRAG).unwrap();
            let stages = [
                shader_stage(vk::ShaderStageFlags::VERTEX, vert_module, &vert_entry),
                shader_stage(vk::ShaderStageFlags::FRAGMENT, frag_module, &frag_entry),
            ];

            // RtPreviewPush = { uint textureHandle }。
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                .offset(0)
                .size(std::mem::size_of::<RtPreviewPush>() as u32)];

            let pipeline = GraphicsPipeline::new(&PipelineDesc {
                device,
                shader_stages: &stages,
                vertex_binding_desc: &[],
                vertex_attr_descs: &[],
                descriptor_set_layouts: &[layout],
                push_constant_ranges: &push,
                render_pass: rp,
                subpass: 0,
                cull_mode: None,
                depth_bias_enable: None,
                depth_bias_constant_factor: None,
                depth_bias_slope_factor: None,
                depth_write_enable: None,
                depth_test_enable: None,
                color_attachment_count: Some(1),
                color_blend_attachments: None,
                front_face: None,
                polygon_mode: None,
            })
            .context("RtPreviewPass: create pipeline")?;

            unsafe {
                device.destroy_shader_module(vert_module, None);
                device.destroy_shader_module(frag_module, None);
            }
            self.pipeline = Some(pipeline);
        }
        Ok(())
    }

    /// 确保 per-swapchain framebuffers 与当前 swapchain views + extent 匹配。
    fn ensure_framebuffers(
        &mut self,
        device: &ash::Device,
        swapchain_views: &[vk::ImageView],
        extent: vk::Extent2D,
    ) -> anyhow::Result<()> {
        if self.target_views == swapchain_views
            && self.extent == extent
            && !self.framebuffers.is_empty()
        {
            return Ok(());
        }
        for fb in self.framebuffers.drain(..).flatten() {
            unsafe { device.destroy_framebuffer(fb, None) };
        }
        self.framebuffers = Vec::with_capacity(swapchain_views.len());
        let rp = self.render_pass.context("render pass missing")?;
        for &view in swapchain_views {
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
            .context("RtPreviewPass: create framebuffer")?;
            self.framebuffers.push(Some(fb));
        }
        self.target_views = swapchain_views.to_vec();
        self.extent = extent;
        Ok(())
    }
}

impl RenderPassNode for RtPreviewPass {
    fn name(&self) -> &str {
        "RtPreview"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // 读边：拓扑序确保在 RenderTextureScheduler 之后；barrier 由图自动
        // 推导（writer 留下的 SHADER_READ_ONLY_OPTIMAL == 本 pass 期望布局
        // → 无操作）。
        graph.read_usage(ResourceUsage {
            handle: RT_OUTPUT_H,
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
        let device = ctx.device;
        let cmd = ctx.cmd;
        let extent = ctx.extent;
        let swapchain_views = ctx.frame.swapchain_views;

        self.ensure_render_pass(device)?;
        self.ensure_pipeline(device)?;
        self.ensure_framebuffers(device, swapchain_views, extent)?;

        let fb = self
            .framebuffers
            .get(ctx.image_index as usize)
            .copied()
            .flatten()
            .context("RtPreviewPass: no framebuffer for image index")?;
        let pipeline = self.pipeline.as_ref().context("pipeline missing")?;

        // RT 的 bindless 槽位（RenderTextureScheduler 发布）。
        let slot = resources.param(RT_OUTPUT_H).unwrap_or(0);

        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass.expect("rp"))
            .framebuffer(fb)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            })
            .clear_values(std::slice::from_ref(&clear));
        unsafe {
            device.cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);
        }

        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline)
        };
        // set 0 = bindless 表（rt_preview.slang 的 BINDLESS_SRVS）。
        let bindless_set = self
            .bindless_set
            .context("RtPreviewPass: set_bindless_set not called")?;
        unsafe {
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout,
                0,
                std::slice::from_ref(&bindless_set),
                &[],
            );
        }
        let push = RtPreviewPush {
            textureHandle: slot,
        };
        unsafe {
            device.cmd_push_constants(
                cmd,
                pipeline.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                std::slice::from_raw_parts(
                    &push as *const _ as *const u8,
                    std::mem::size_of::<RtPreviewPush>(),
                ),
            );
        }
        unsafe { device.cmd_draw(cmd, 3, 1, 0, 0) };
        unsafe { device.cmd_end_render_pass(cmd) };

        log::trace!("RtPreview: sampled RT slot {slot} -> swapchain");
        Ok(())
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Post,
            inputs: vec![RT_OUTPUT_H],
            outputs: Vec::new(),
        }
    }
}

impl Drop for RtPreviewPass {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            for fb in self.framebuffers.drain(..).flatten() {
                unsafe { device.destroy_framebuffer(fb, None) };
            }
            if let Some(rp) = self.render_pass.take() {
                unsafe { device.destroy_render_pass(rp, None) };
            }
        }
    }
}
