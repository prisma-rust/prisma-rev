//! 统一 CRT 调度器（Custom Render Texture 管理系统）。
//!
//! 引擎底层每帧遍历所有注册的 [`RenderTexture`]，按各自配置
//! （[`RtUpdateMode`] + period + 绑定 shader）决定是否执行一次全屏 blit：
//! - `OnLoad`：首次调度渲染一次
//! - `Realtime`：每帧（或按 period 间隔）自动渲染
//! - `OnDemand`：仅 `request_update()` 标记后渲染
//!
//! 作为单个 [`RenderPassNode`] 挂在渲染图早期（Scene 之前），与 scene
//! 零耦合。渲染完成后把每个 RT 的 image/view/bindless 槽位发布到 graph，
//! 下游 pass（如 `RtPreviewPass`）直接采样。

use anyhow::Context as _;
use ash::vk;
use std::collections::HashMap;
use std::ffi::CString;

use crate::bindless::{BindlessTextureTable, TextureHandle};
use crate::context::VulkanContext;
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderPassNode,
    RenderSettings, ResourceHandle, ResourceUsage,
};
use crate::render_texture::{RenderTexture, RtShader};
use crate::shader::{load_shader_module, shader_stage};
use crate::shader_bindings::rt_render::{
    RtRenderPush, ENTRY_FRAGMENT_MAIN as RT_FRAG, ENTRY_VERTEX_MAIN as RT_VERT,
};

const RT_RENDER_VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/rt_render.vert.spv");
const RT_RENDER_FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/rt_render.frag.spv");

/// 调度器输出句柄基址：第 i 个注册的 RT 发布到 `RT_OUTPUT_H + i`。
/// 固定基址（与 SCENE_*_H 同一约定）使下游 pass 无需知道注册顺序。
pub const RT_OUTPUT_H: ResourceHandle = ResourceHandle(1004);

/// 统一 CRT 调度器：持有所有注册的 RenderTexture，每帧按配置渲染。
pub struct RenderTextureScheduler {
    /// 克隆的 device（Drop 时销毁）。
    device: Option<ash::Device>,
    /// 注册的 RT 列表（句柄 = RT_OUTPUT_H + 索引）。
    rts: Vec<RenderTexture>,
    /// 通用离屏 render pass（一个，所有 RT 共用）。
    render_pass: Option<vk::RenderPass>,
    /// per-RT framebuffer（与 `rts` 同索引；RT resize 时重建）。
    framebuffers: Vec<Option<vk::Framebuffer>>,
    /// 每个 framebuffer 创建时对应的 RT view（resize 比对用）。
    fb_views: Vec<Option<vk::ImageView>>,
    /// 每 shader 一条管线（缓存，惰性创建）。
    pipelines: HashMap<RtShader, GraphicsPipeline>,
    /// 当前调度尺寸（GraphRenderer 传入，RT resize 用）。
    extent: vk::Extent2D,
}

impl RenderTextureScheduler {
    pub fn new(context: &VulkanContext) -> Self {
        Self {
            device: Some(context.device.clone()),
            rts: Vec::new(),
            render_pass: None,
            framebuffers: Vec::new(),
            fb_views: Vec::new(),
            pipelines: HashMap::new(),
            extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
        }
    }

    /// 注册一个 RT。返回其输出句柄（`RT_OUTPUT_H + index`），下游 pass
    /// 用它采样/读。RT 的 extent 在创建时已确定。
    pub fn add(&mut self, rt: RenderTexture) -> ResourceHandle {
        self.rts.push(rt);
        let handle = ResourceHandle(RT_OUTPUT_H.0 + self.rts.len() as u32 - 1);
        log::trace!("RenderTextureScheduler: registered rt -> {handle:?}");
        handle
    }

    pub fn len(&self) -> usize {
        self.rts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rts.is_empty()
    }

    /// 按输出句柄访问 RT。
    pub fn rt(&self, handle: ResourceHandle) -> Option<&RenderTexture> {
        let idx = handle.0.checked_sub(RT_OUTPUT_H.0)? as usize;
        self.rts.get(idx)
    }

    /// 按输出句柄可变访问 RT（配置更新：模式/period/shader/request_update）。
    pub fn rt_mut(&mut self, handle: ResourceHandle) -> Option<&mut RenderTexture> {
        let idx = handle.0.checked_sub(RT_OUTPUT_H.0)? as usize;
        self.rts.get_mut(idx)
    }

    /// 所有 RT 的 bindless 句柄（调试/外部使用）。
    pub fn handles(&self) -> impl Iterator<Item = (ResourceHandle, TextureHandle)> + '_ {
        self.rts.iter().enumerate().map(|(i, rt)| {
            (
                ResourceHandle(RT_OUTPUT_H.0 + i as u32),
                rt.texture_handle(),
            )
        })
    }

    /// 统一调整所有 RT 尺寸（swapchain recreate 时调用）。
    /// bindless 句柄保持不变；framebuffer 由下一帧 execute 的
    /// `drop_stale_framebuffers` 检测到 view 变化后重建。
    pub fn resize_all(
        &mut self,
        context: &VulkanContext,
        bindless: &mut BindlessTextureTable,
        extent: vk::Extent2D,
    ) -> anyhow::Result<()> {
        self.extent = extent;
        for rt in self.rts.iter_mut() {
            rt.resize(context, bindless, extent)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // 内部：GPU 基础设施（惰性创建）
    // ------------------------------------------------------------------

    /// 通用离屏 render pass：所有 RT 共用。final_layout =
    /// SHADER_READ_ONLY_OPTIMAL 供下游采样。
    fn ensure_render_pass(&mut self, device: &ash::Device) -> anyhow::Result<()> {
        if self.render_pass.is_none() {
            let rt = self
                .rts
                .first()
                .context("RenderTextureScheduler: no RT registered")?;
            let color_attachment = vk::AttachmentDescription::default()
                .format(rt.format())
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .final_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
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
                .context("RenderTextureScheduler: create render pass")?;
            self.render_pass = Some(rp);
        }
        Ok(())
    }

    /// 按 shader 惰性创建管线（缓存；本 pass 无 descriptor set）。
    fn ensure_pipeline(
        &mut self,
        device: &ash::Device,
        shader: RtShader,
    ) -> anyhow::Result<vk::Pipeline> {
        if let Some(p) = self.pipelines.get(&shader) {
            return Ok(p.pipeline);
        }
        let rp = self.render_pass.context("render pass missing")?;
        let (vert_spv, frag_spv, vert_entry, frag_entry) = match shader {
            RtShader::BitmapPattern => (
                RT_RENDER_VERT_SPV,
                RT_RENDER_FRAG_SPV,
                CString::new(RT_VERT).unwrap(),
                CString::new(RT_FRAG).unwrap(),
            ),
        };
        let vert_module = load_shader_module(device, vert_spv)
            .context("RenderTextureScheduler: load vert module")?;
        let frag_module = load_shader_module(device, frag_spv)
            .context("RenderTextureScheduler: load frag module")?;
        let stages = [
            shader_stage(vk::ShaderStageFlags::VERTEX, vert_module, &vert_entry),
            shader_stage(vk::ShaderStageFlags::FRAGMENT, frag_module, &frag_entry),
        ];

        // RtRenderPush = { uint pattern }（4 字节，无 std140 尾部填充）。
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<RtRenderPush>() as u32)];

        let pipeline = GraphicsPipeline::new(&PipelineDesc {
            device,
            shader_stages: &stages,
            vertex_binding_desc: &[],
            vertex_attr_descs: &[],
            descriptor_set_layouts: &[],
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
        .context("RenderTextureScheduler: create pipeline")?;

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }
        Ok(self.pipelines.entry(shader).or_insert(pipeline).pipeline)
    }

    /// 确保 RT 的 framebuffer 存在（RT resize 时调用方先 drop 旧的，
    /// 这里只负责创建缺失项）。
    fn ensure_framebuffer(
        &mut self,
        device: &ash::Device,
        index: usize,
    ) -> anyhow::Result<vk::Framebuffer> {
        let rt = &self.rts[index];
        let rp = self.render_pass.context("render pass missing")?;
        let fb = match self.framebuffers.get(index).copied().flatten() {
            Some(fb) => fb,
            None => {
                let attachments = [rt.view()];
                let fb = unsafe {
                    device.create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(rp)
                            .attachments(&attachments)
                            .width(rt.extent().width)
                            .height(rt.extent().height)
                            .layers(1),
                        None,
                    )
                }
                .context("RenderTextureScheduler: create framebuffer")?;
                if self.framebuffers.len() <= index {
                    self.framebuffers.resize(index + 1, None);
                    self.fb_views.resize(index + 1, None);
                }
                self.framebuffers[index] = Some(fb);
                self.fb_views[index] = Some(rt.view());
                fb
            }
        };
        Ok(fb)
    }

    /// 丢弃引用了已失效 RT view 的 framebuffer（RT resize 后），下一帧重建。
    fn drop_stale_framebuffers(&mut self, device: &ash::Device) {
        for (i, rt) in self.rts.iter().enumerate() {
            let fb_view = self.fb_views.get(i).copied().flatten();
            if fb_view.is_some() && fb_view != Some(rt.view()) {
                if let Some(fb) = self.framebuffers[i].take() {
                    unsafe { device.destroy_framebuffer(fb, None) };
                    self.fb_views[i] = None;
                }
            }
        }
    }
}

impl RenderPassNode for RenderTextureScheduler {
    fn name(&self) -> &str {
        "RenderTextures"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // 每个 RT 声明一个 write 边：渲染后布局 = SHADER_READ_ONLY_OPTIMAL
        // （render pass final_layout），供下游读边 barrier 推导。RT 是外部
        // 持久资源，graph 不分配、不 create_resource_at。
        for i in 0..self.rts.len() {
            graph.write_usage(ResourceUsage {
                handle: ResourceHandle(RT_OUTPUT_H.0 + i as u32),
                access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            });
        }
    }

    fn execute(
        &mut self,
        ctx: &RenderContext,
        resources: &mut GraphResources,
    ) -> anyhow::Result<()> {
        if self.rts.is_empty() {
            return Ok(());
        }
        let device = ctx.device;
        let cmd = ctx.cmd;
        self.ensure_render_pass(device)?;
        self.drop_stale_framebuffers(device);
        let rp = self.render_pass.context("render pass missing")?;
        let mut rendered = vec![false; self.rts.len()];

        // 第一阶段：快照每个 RT 的渲染决策（shader + 是否需要渲染）。
        // 下一步扩展执行阶段要交错借用 `&mut self`（管线/framebuffer 创建）与
        // `&mut self.rts[i]`（pattern 生成），先快照避免借用冲突。
        let plan: Vec<(Option<RtShader>, bool)> = self
            .rts
            .iter()
            .map(|rt| (rt.active_shader(), rt.needs_render()))
            .collect();

        // 第二阶段：按快照执行 blit。
        for (i, &(shader, needs)) in plan.iter().enumerate() {
            if let Some(shader) = shader {
                if needs {
                    let pipeline = self.ensure_pipeline(device, shader)?;
                    let fb = self.ensure_framebuffer(device, i)?;
                    let pattern = self.rts[i].next_pattern() as u32;
                    let (image, extent) = {
                        let rt = &self.rts[i];
                        (rt.image(), rt.extent())
                    };

                    // 任意旧布局 → COLOR_ATTACHMENT_OPTIMAL。全屏重画，
                    // 无内容保留需求（丢弃式更新，同 Realtime 默认）。
                    let barrier = vk::ImageMemoryBarrier2::default()
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                        .src_access_mask(vk::AccessFlags2::MEMORY_READ)
                        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                        .image(image)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        });
                    let barriers = [barrier];
                    let dep = vk::DependencyInfo::default().image_memory_barriers(&barriers);
                    // `vkCmdPipelineBarrier2` (1.3 core) is not exported on a 1.2
                    // 实例 — use the KHR 包装器 like `buffer.rs` does.
                    let sync2 = crate::vulkan_compat::LegacySync2 {
                        device: &ctx.context.device,
                    };
                    unsafe { sync2.cmd_pipeline_barrier2(cmd, &dep) };

                    let clear = vk::ClearValue {
                        color: vk::ClearColorValue {
                            float32: [0.0, 0.0, 0.0, 1.0],
                        },
                    };
                    let begin = vk::RenderPassBeginInfo::default()
                        .render_pass(rp)
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
                        device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
                    }
                    let push = RtRenderPush { pattern };
                    unsafe {
                        device.cmd_push_constants(
                            cmd,
                            self.pipelines[&shader].layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            std::slice::from_raw_parts(
                                &push as *const _ as *const u8,
                                std::mem::size_of::<RtRenderPush>(),
                            ),
                        );
                    }
                    unsafe { device.cmd_draw(cmd, 3, 1, 0, 0) };
                    unsafe { device.cmd_end_render_pass(cmd) };
                    rendered[i] = true;
                }
            }

            // 发布 RT（image + view + bindless 槽位）供下游采样。
            let rt = &self.rts[i];
            resources.set_image(ResourceHandle(RT_OUTPUT_H.0 + i as u32), rt.image());
            resources.set_image_view(ResourceHandle(RT_OUTPUT_H.0 + i as u32), rt.view());
            resources.set_param(
                ResourceHandle(RT_OUTPUT_H.0 + i as u32),
                rt.texture_handle().0,
            );
        }

        // 帧末推进状态：渲染帧标记初始化/清 pending，所有 RT 无条件 tick
        // （计数器必须每帧推进，否则 Realtime+period 判定会卡死）。
        for (i, rt) in self.rts.iter_mut().enumerate() {
            if rendered[i] {
                rt.mark_rendered();
            }
            rt.tick();
        }
        Ok(())
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Rt,
            inputs: Vec::new(),
            outputs: (0..self.rts.len())
                .map(|i| ResourceHandle(RT_OUTPUT_H.0 + i as u32))
                .collect(),
        }
    }
}

impl Drop for RenderTextureScheduler {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            for fb in self.framebuffers.drain(..).flatten() {
                unsafe { device.destroy_framebuffer(fb, None) };
            }
            if let Some(rp) = self.render_pass.take() {
                unsafe { device.destroy_render_pass(rp, None) };
            }
            // pipelines 由 GraphicsPipeline RAII 释放；rts 各自 Drop。
        }
    }
}
