//! `ui_overlay` 的 GPU 部分（字体图集 + 管线 + 绘制）。
//!
//! `ui` feature 打开时才编译：关闭后 `ui_overlay` 模块只保留输入数据结构
//! （[`crate::ui_overlay::UiOverlayInput`] 等），引擎照常填充、
//! 渲染侧直接忽略，**fontdue / ttf-parser 不再进二进制**。
//!
//! 字形位图由 `text` 里的光栅后端提供（`ui-fontdue` / `ui-directwrite`），
//! 这里只负责图集打包、四边形展开与 Vulkan 绘制——与后端无关。
#[repr(C)]
struct UiVertex {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

use std::collections::HashMap;
use std::ffi::CString;
use std::mem::size_of;

use anyhow::{Context as _, Result};
use ash::vk;

use super::text::{self, baseline_offset, TextRasterizer};
use super::{UiOverlayInput, UiTextAlign, UiTextInput};
use crate::buffer::{create_buffer, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;
use crate::shader_bindings::ui_overlay::*;

#[allow(dead_code)]
const MAX_QUADS: usize = 16_384;
pub(crate) const VERTICES_PER_QUAD: u32 = 6;
const VERTEX_SIZE: vk::DeviceSize = size_of::<UiVertex>() as vk::DeviceSize;

// ---------------------------------------------------------------------------
// 字形图集
// ---------------------------------------------------------------------------

/// 图集边长（px）。R8 = 1MB 显存，intro 级 UI 用不完。
const ATLAS_SIZE: u32 = 1024;

/// 一个已光栅化并打包进图集的字形。
struct CachedGlyph {
    /// 图集内 rect。空白字形（w/h = 0）不占图集空间。
    rect: Option<[u32; 4]>,
    advance: f32,
    xmin: i32,
    ymin: i32,
}

/// 字形光栅 + 行打包的 R8 字形图集（host-visible，LINEAR tiling）。
///
/// 图集像素 (0,0) 保留为白色——plain quad 的 uv=(0,0) 采样到 alpha 1。
/// 布局保持 GENERAL：host 直接写入新字形；record 在 dirty 帧插入
/// HOST_WRITE → FRAGMENT_SHADER 屏障。
///
/// `rasterizer` 是**编译期选定**的文本后端（见 [`super::text`]）；
/// `None` = 这个构建没有文本后端或系统里找不到字体，文字命令被跳过。
struct GlyphAtlas {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
    rasterizer: Option<Box<dyn TextRasterizer>>,
    /// (char, font_size 量化到 0.5px) → 字形
    glyphs: HashMap<(char, u32), CachedGlyph>,
    /// 行打包游标
    x: u32,
    y: u32,
    row_h: u32,
    /// 本帧有新的字形写入 → 需要 HOST→SHADER 屏障
    dirty: bool,
    device: ash::Device,
}

impl GlyphAtlas {
    fn new(context: &VulkanContext, command_pool: vk::CommandPool) -> Result<Self> {
        let device = context.device.clone();
        let (image, memory) = Self::create_image(context)?;
        let view = Self::create_view(&device, image)?;
        let sampler = Self::create_sampler(&device)?;

        // UNDEFINED → GENERAL：host 侧直接 map 写（LINEAR tiling）。
        let cmd = allocate_temp_command_buffer(&device, command_pool)
            .context("GlyphAtlas: allocate command buffer")?;
        unsafe {
            device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .context("GlyphAtlas: begin command buffer")?;
        }
        let barrier = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::HOST_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&barrier),
            );
            device.end_command_buffer(cmd)?;
        }
        submit_and_wait(&device, context.graphics_queue, command_pool, cmd)?;

        // Reserve 像素 (0,0) = 白色（plain quad 采样它得到 alpha 1）。
        let ptr = unsafe {
            device
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .context("GlyphAtlas: map")?
        };
        unsafe {
            std::ptr::write_bytes(ptr as *mut u8, 255, 1);
            device.unmap_memory(memory);
        }

        let rasterizer = text::create();
        if let Some(backend) = rasterizer.as_deref() {
            if let Some(m) = backend.line_metrics(16.0) {
                log::debug!(
                    "ui overlay: 文本度量 @16px —— ascent {:.1} / descent {:.1} / line height {:.1}",
                    m.ascent,
                    m.descent,
                    m.line_height
                );
            }
        }

        Ok(Self {
            image,
            memory,
            view,
            sampler,
            rasterizer,
            glyphs: HashMap::new(),
            x: 1,
            y: 0,
            row_h: 0,
            dirty: false,
            device,
        })
    }

    /// 单行文本的基线相对矩形顶边的偏移（来自后端的行度量）。
    fn baseline_offset(&self, font_size: f32) -> f32 {
        let metrics = self
            .rasterizer
            .as_deref()
            .and_then(|backend| backend.line_metrics(font_size));
        baseline_offset(metrics, font_size)
    }

    fn create_image(context: &VulkanContext) -> Result<(vk::Image, vk::DeviceMemory)> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .extent(vk::Extent3D {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { context.device.create_image(&image_info, None) }
            .context("GlyphAtlas: create image")?;
        let mem_reqs = unsafe { context.device.get_image_memory_requirements(image) };
        let mem_type = find_memory_type(
            &context.physical_device_memory_properties,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        );
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);
        let memory = unsafe { context.device.allocate_memory(&alloc, None) }
            .context("GlyphAtlas: allocate")?;
        unsafe {
            context
                .device
                .bind_image_memory(image, memory, 0)
                .context("GlyphAtlas: bind")?;
        }
        Ok((image, memory))
    }

    fn create_view(device: &ash::Device, image: vk::Image) -> Result<vk::ImageView> {
        let info = vk::ImageViewCreateInfo::default()
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
        unsafe { device.create_image_view(&info, None) }.context("GlyphAtlas: image view")
    }

    fn create_sampler(device: &ash::Device) -> Result<vk::Sampler> {
        let info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .max_lod(0.0);
        unsafe { device.create_sampler(&info, None) }.context("GlyphAtlas: sampler")
    }

    /// 取字形（光栅化 + 打包，缓存）。空格等空白字形不占图集。
    fn glyph(&mut self, ch: char, font_size: f32) -> Option<&CachedGlyph> {
        let key = (ch, (font_size * 2.0).round() as u32);
        if !self.glyphs.contains_key(&key) {
            let glyph = self.rasterizer.as_mut()?.rasterize(ch, font_size)?;
            let rect = if glyph.width > 0 && glyph.height > 0 {
                self.pack(&glyph.pixels, glyph.width, glyph.height)
            } else {
                None
            };
            self.glyphs.insert(
                key,
                CachedGlyph {
                    rect,
                    advance: glyph.advance,
                    xmin: glyph.xmin,
                    ymin: glyph.ymin,
                },
            );
        }
        self.glyphs.get(&key)
    }

    /// 行打包：把 bitmap 拷贝进图集，返回图集 rect。
    fn pack(&mut self, bitmap: &[u8], w: usize, h: usize) -> Option<[u32; 4]> {
        let w = w as u32;
        let h = h as u32;
        if self.x + w > ATLAS_SIZE {
            self.x = 0;
            self.y += self.row_h;
            self.row_h = 0;
        }
        if self.y + h > ATLAS_SIZE {
            log::warn!("ui overlay: glyph atlas full ({ATLAS_SIZE}px) — dropping glyph");
            return None;
        }
        let rect = [self.x, self.y, w, h];
        // 拷贝到 host-visible 内存。
        let ptr = unsafe {
            self.device
                .map_memory(self.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .expect("GlyphAtlas: map")
        };
        unsafe {
            let base = ptr as *mut u8;
            for row in 0..h {
                let src = &bitmap[(row * w) as usize..((row + 1) * w) as usize];
                let dst = base.add(((self.y + row) * ATLAS_SIZE + self.x) as usize);
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, w as usize);
            }
            self.device.unmap_memory(self.memory);
        }
        self.x += w + 1; // 1px 间距防相邻字形渗色
        self.row_h = self.row_h.max(h);
        self.dirty = true;
        Some(rect)
    }
}

impl Drop for GlyphAtlas {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_image_view(self.view, None);
            self.device.free_memory(self.memory, None);
            self.device.destroy_image(self.image, None);
        }
    }
}

// ---------------------------------------------------------------------------
// UiOverlay
// ---------------------------------------------------------------------------

/// GPU-side UI 叠加
pub struct UiOverlay {
    pipeline: Option<vk::Pipeline>,
    layout: Option<vk::PipelineLayout>,
    set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    vertex_buffer: vk::Buffer,
    vertex_memory: vk::DeviceMemory,
    vertex_capacity: u32,
    render_pass: vk::RenderPass,
    /// 创建 `render_pass` 时使用的颜色格式；swapchain 格式变化时用来判断
    /// 是否需要重建（见 [`UiOverlay::set_color_format`]）。
    color_format: vk::Format,
    /// render pass 结束时的图像布局：交换链是 `PRESENT_SRC_KHR`，离屏目标
    /// 没有呈现引擎，必须留在 `COLOR_ATTACHMENT_OPTIMAL`（#597）。
    final_layout: vk::ImageLayout,
    atlas: GlyphAtlas,
    device: ash::Device,
    /// One framebuffer per swapchain image view/extent, reused across frames.
    /// Cleared by [`UiOverlay::on_swapchain_views_changed`] because the old
    /// image views are destroyed during swapchain recreation.
    framebuffers: Vec<CachedFramebuffer>,
}

#[derive(Clone, Copy)]
struct CachedFramebuffer {
    view: vk::ImageView,
    extent: vk::Extent2D,
    handle: vk::Framebuffer,
}

impl UiOverlay {
    /// 用默认的 `B8G8R8A8_SRGB` 颜色格式构造。
    ///
    /// 知道 swapchain 实际格式的调用方（如 [`GraphRenderer`](crate::GraphRenderer)）
    /// 应改用 [`UiOverlay::with_color_format`]：surface 在首选格式不可用时会
    /// 回退到 `available[0]`（Android 常见 `R8G8B8A8_*`），render pass 与
    /// image view 的格式必须一致（VUID-VkFramebufferCreateInfo-renderPass-02711）。
    pub fn new(context: &VulkanContext, command_pool: vk::CommandPool) -> Result<Self> {
        Self::with_color_format(context, command_pool, vk::Format::B8G8R8A8_SRGB)
    }

    /// 用 swapchain 的实际颜色格式构造叠加层。
    pub fn with_color_format(
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        color_format: vk::Format,
    ) -> Result<Self> {
        Self::with_color_format_and_final_layout(
            context,
            command_pool,
            color_format,
            vk::ImageLayout::PRESENT_SRC_KHR,
        )
    }

    /// 用显式的最终布局构造叠加层。
    ///
    /// 交换链用 [`vk::ImageLayout::PRESENT_SRC_KHR`]；离屏目标（编辑器视口）
    /// 没有呈现引擎，必须用 [`vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL`]，
    /// 否则 render pass 会留下非法布局（#597）。
    pub fn with_color_format_and_final_layout(
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        color_format: vk::Format,
        final_layout: vk::ImageLayout,
    ) -> Result<Self> {
        let device = context.device.clone();
        let render_pass = Self::create_render_pass(&device, color_format, final_layout)?;
        let atlas = GlyphAtlas::new(context, command_pool)?;

        let init_vertices = 1024u32;
        let buf_size = VERTEX_SIZE * init_vertices as u64;
        let (vertex_buffer, vertex_memory) = create_buffer(
            context,
            buf_size,
            BufferUsage::VERTEX_BUFFER,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("UiOverlay: create vertex buffer")?;

        let (set_layout, descriptor_pool, descriptor_set) =
            Self::create_descriptors(&device, atlas.view, atlas.sampler)?;

        Ok(Self {
            pipeline: None,
            layout: None,
            set_layout,
            descriptor_pool,
            descriptor_set,
            vertex_buffer,
            vertex_memory,
            vertex_capacity: init_vertices,
            render_pass,
            color_format,
            final_layout,
            atlas,
            device,
            framebuffers: Vec::new(),
        })
    }

    /// 让 render pass 跟随 swapchain 的实际颜色格式。
    ///
    /// swapchain 重建后格式可能变化（首选格式不可用时回退到 `available[0]`，
    /// Android 上常见 `R8G8B8A8_*`）。格式变化时按新格式重建 render pass，
    /// 并销毁依赖旧 render pass 的 pipeline 与 framebuffer；pipeline 会在
    /// 下一次 [`UiOverlay::record`] 时按新 pass 懒重建。
    pub fn set_color_format(&mut self, color_format: vk::Format) -> Result<()> {
        if self.color_format == color_format {
            return Ok(());
        }
        // 先建新的再拆旧的，创建失败时旧 pass 仍然有效。
        let render_pass = Self::create_render_pass(&self.device, color_format, self.final_layout)?;
        unsafe {
            if let Some(pipeline) = self.pipeline.take() {
                self.device.destroy_pipeline(pipeline, None);
            }
            if let Some(layout) = self.layout.take() {
                self.device.destroy_pipeline_layout(layout, None);
            }
            for cached in self.framebuffers.drain(..) {
                self.device.destroy_framebuffer(cached.handle, None);
            }
            self.device.destroy_render_pass(self.render_pass, None);
        }
        self.render_pass = render_pass;
        self.color_format = color_format;
        log::debug!("UiOverlay: render pass rebuilt for {color_format:?}");
        Ok(())
    }

    /// set 0 binding 0 = 字形图集 combined image sampler。
    fn create_descriptors(
        device: &ash::Device,
        atlas_view: vk::ImageView,
        atlas_sampler: vk::Sampler,
    ) -> Result<(
        vk::DescriptorSetLayout,
        vk::DescriptorPool,
        vk::DescriptorSet,
    )> {
        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(GLYPH_ATLAS_BINDING)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));
        let set_layout = unsafe { device.create_descriptor_set_layout(&layout_info, None) }
            .context("UiOverlay: descriptor set layout")?;

        let pool_size = vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: 1,
        };
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(std::slice::from_ref(&pool_size));
        let pool = unsafe { device.create_descriptor_pool(&pool_info, None) }
            .context("UiOverlay: descriptor pool")?;

        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(std::slice::from_ref(&set_layout));
        let set = unsafe { device.allocate_descriptor_sets(&alloc_info) }
            .context("UiOverlay: allocate descriptor set")?[0];

        let image_info = vk::DescriptorImageInfo::default()
            .image_layout(vk::ImageLayout::GENERAL)
            .image_view(atlas_view)
            .sampler(atlas_sampler);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(GLYPH_ATLAS_BINDING)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&image_info));
        unsafe {
            device.update_descriptor_sets(std::slice::from_ref(&write), &[]);
        }

        Ok((set_layout, pool, set))
    }

    fn create_render_pass(
        device: &ash::Device,
        color_format: vk::Format,
        final_layout: vk::ImageLayout,
    ) -> Result<vk::RenderPass> {
        let color_attachment = vk::AttachmentDescription::default()
            .format(color_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .final_layout(final_layout);

        let color_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(std::slice::from_ref(&color_ref));

        let dep = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

        let create_info = vk::RenderPassCreateInfo::default()
            .attachments(std::slice::from_ref(&color_attachment))
            .subpasses(std::slice::from_ref(&subpass))
            .dependencies(std::slice::from_ref(&dep));

        let rp = unsafe { device.create_render_pass(&create_info, None) }
            .context("UiOverlay: create_render_pass")?;
        Ok(rp)
    }

    fn ensure_pipeline(&mut self, device: &ash::Device, extent: vk::Extent2D) -> Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        let (pipeline, layout) = Self::create_pipeline(device, self.render_pass, extent)?;
        self.pipeline = Some(pipeline);
        self.layout = Some(layout);
        Ok(())
    }

    fn create_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        _extent: vk::Extent2D,
    ) -> Result<(vk::Pipeline, vk::PipelineLayout)> {
        const VERT_SPV: &[u8] = include_bytes!("../../../../assets/shaders/ui_overlay.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../../assets/shaders/ui_overlay.frag.spv");
        let vert_module =
            crate::shader::load_shader_module(device, VERT_SPV).context("UiOverlay: load vert")?;
        let frag_module =
            crate::shader::load_shader_module(device, FRAG_SPV).context("UiOverlay: load frag")?;

        let vert_entry = CString::new(ENTRY_VERTEX_MAIN).unwrap();
        let frag_entry = CString::new(ENTRY_FRAGMENT_MAIN).unwrap();
        let shader_stages = [
            crate::shader::shader_stage(
                vk::ShaderStageFlags::VERTEX,
                vert_module,
                vert_entry.as_c_str(),
            ),
            crate::shader::shader_stage(
                vk::ShaderStageFlags::FRAGMENT,
                frag_module,
                frag_entry.as_c_str(),
            ),
        ];

        // set 0 binding 0 — glyph atlas（见 GlyphAtlas::create_descriptors）。
        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(GLYPH_ATLAS_BINDING)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let set_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));
        let set_layout = unsafe { device.create_descriptor_set_layout(&set_layout_info, None) }
            .context("UiOverlay: pipeline set layout")?;
        let layout_info =
            vk::PipelineLayoutCreateInfo::default().set_layouts(std::slice::from_ref(&set_layout));
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }
            .context("UiOverlay: pipeline layout")?;
        unsafe {
            device.destroy_descriptor_set_layout(set_layout, None);
        }

        let binding_desc = vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<UiVertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX);
        let attrs = [
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(1)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(8),
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(2)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(16),
        ];
        let vertex_input_info = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(std::slice::from_ref(&binding_desc))
            .vertex_attribute_descriptions(&attrs);

        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST)
            .primitive_restart_enable(false);

        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        // 管线 状态 no 深度 Alpha 混合
        let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::CLOCKWISE)
            .line_width(1.0);
        let multisampling = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD);
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .logic_op_enable(false)
            .attachments(std::slice::from_ref(&blend));
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(false)
            .depth_write_enable(false);

        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&shader_stages)
            .vertex_input_state(&vertex_input_info)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterizer)
            .multisample_state(&multisampling)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic_state_info)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);

        let pipeline = unsafe {
            device
                .create_graphics_pipelines(
                    vk::PipelineCache::null(),
                    std::slice::from_ref(&pipeline_info),
                    None,
                )
                .map_err(|(_, e)| e)
        }
        .context("UiOverlay: graphics pipeline")?[0];

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }
        Ok((pipeline, layout))
    }

    fn grow_buffer(&mut self, context: &VulkanContext, needed: u32) -> Result<()> {
        if needed <= self.vertex_capacity {
            return Ok(());
        }
        let new_cap = needed.next_power_of_two();
        let buf_size = VERTEX_SIZE * new_cap as u64;
        let (buf, mem) = create_buffer(
            context,
            buf_size,
            BufferUsage::VERTEX_BUFFER,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("UiOverlay: grow vertex buffer")?;
        unsafe {
            self.device.destroy_buffer(self.vertex_buffer, None);
            self.device.free_memory(self.vertex_memory, None);
        }
        self.vertex_buffer = buf;
        self.vertex_memory = mem;
        self.vertex_capacity = new_cap;
        Ok(())
    }

    /// 估算顶点数：quads × 6 + 每个文本字符 × 6。
    fn estimate_vertex_count(&self, input: &UiOverlayInput) -> u32 {
        let mut chars = 0usize;
        for t in &input.texts {
            chars += t.content.chars().count();
        }
        input.quads.len() as u32 * VERTICES_PER_QUAD + (chars as u32) * VERTICES_PER_QUAD
    }

    /// Record UI 叠加 绘制 commands into `cmd`.
    pub fn record(
        &mut self,
        context: &VulkanContext,
        cmd: vk::CommandBuffer,
        extent: vk::Extent2D,
        target_view: vk::ImageView,
        input: &UiOverlayInput,
    ) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }
        self.ensure_pipeline(&context.device, extent)?;

        // 估算值只用于预留容量：单个字符只有在有字形位图时才写顶点
        // （空格没有 rect、atlas 满或无字体时直接跳过），实际顶点数通常更少。
        let estimated_vertices = self.estimate_vertex_count(input);

        // Build 顶点 (quads + 逐字形展开的 texts)。
        let mut vertices: Vec<UiVertex> = Vec::with_capacity(estimated_vertices as usize);
        let w = extent.width as f32;
        let h = extent.height as f32;

        // 按引擎算好的层级顺序交错画矩形和文本（旧路径没有顺序时退化成
        // 「先全部矩形、再全部文本」）。
        let push_quad = |vertices: &mut Vec<UiVertex>, quad: &crate::ui_overlay::UiQuad| {
            let [x0, y0, x1, y1] = quad.rect;
            let [r, g, b, a] = quad.color;
            emit_quad(
                vertices,
                [x0, y0],
                [x1, y1],
                [0.0, 0.0],
                [0.0, 0.0],
                [r, g, b, a],
            );
        };

        if input.order.is_empty() {
            for quad in &input.quads {
                push_quad(&mut vertices, quad);
            }
            for t in &input.texts {
                self.emit_text(&mut vertices, t, w, h);
            }
        } else {
            for draw in &input.order {
                match *draw {
                    crate::ui_overlay::UiOverlayDraw::Quad(index) => {
                        if let Some(quad) = input.quads.get(index as usize) {
                            push_quad(&mut vertices, quad);
                        }
                    }
                    crate::ui_overlay::UiOverlayDraw::Text(index) => {
                        if let Some(t) = input.texts.get(index as usize) {
                            self.emit_text(&mut vertices, t, w, h);
                        }
                    }
                }
            }
        }

        // 上传与 draw 都使用实际顶点数：估算值可能高于 `vertices.len()`，
        // 按它复制会读到 Vec 之外未初始化内存（UB，GPU 上是杂散三角形）。
        let vert_count =
            u32::try_from(vertices.len()).context("UiOverlay: vertex count exceeds u32")?;
        if vert_count == 0 {
            return Ok(());
        }
        self.grow_buffer(context, vert_count)?;

        // Upload 顶点 data.
        let vert_bytes = vertices.len() * size_of::<UiVertex>();
        unsafe {
            let ptr = context
                .device
                .map_memory(
                    self.vertex_memory,
                    0,
                    vert_bytes as u64,
                    vk::MemoryMapFlags::empty(),
                )
                .context("UiOverlay: map")?;
            std::ptr::copy_nonoverlapping(
                vertices.as_ptr() as *const u8,
                ptr as *mut u8,
                vert_bytes,
            );
            context.device.unmap_memory(self.vertex_memory);
        }

        // 图集有新字形 → HOST_WRITE → FRAGMENT_SHADER 屏障（GENERAL 布局不变）。
        if self.atlas.dirty {
            let barrier = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.atlas.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            unsafe {
                context.device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::HOST,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    std::slice::from_ref(&barrier),
                );
            }
            self.atlas.dirty = false;
        }

        // Reuse the framebuffer for this swapchain image instead of creating
        // (and retaining) one every frame. Not thread-safe by itself; record
        // runs on the render thread and swapchain recreation clears the cache.
        let fb = match self.framebuffers.iter().find(|cached| {
            cached.view == target_view
                && cached.extent.width == extent.width
                && cached.extent.height == extent.height
        }) {
            Some(cached) => cached.handle,
            None => {
                let attachments = [target_view];
                let fb_info = vk::FramebufferCreateInfo::default()
                    .render_pass(self.render_pass)
                    .attachments(&attachments)
                    .width(extent.width)
                    .height(extent.height)
                    .layers(1);
                let handle = unsafe { context.device.create_framebuffer(&fb_info, None) }
                    .context("UiOverlay: framebuffer")?;
                self.framebuffers.push(CachedFramebuffer {
                    view: target_view,
                    extent,
                    handle,
                });
                handle
            }
        };

        // Record 绘制
        let rp_begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(fb)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            });
        unsafe {
            context
                .device
                .cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);
            context.device.cmd_bind_pipeline(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.unwrap(),
            );
            context.device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.layout.unwrap(),
                0,
                std::slice::from_ref(&self.descriptor_set),
                &[],
            );

            let vp = vk::Viewport::default()
                .x(0.0)
                .y(0.0)
                .width(extent.width as f32)
                .height(extent.height as f32)
                .min_depth(0.0)
                .max_depth(1.0);
            let sc = vk::Rect2D::default()
                .offset(vk::Offset2D { x: 0, y: 0 })
                .extent(extent);
            context
                .device
                .cmd_set_viewport(cmd, 0, std::slice::from_ref(&vp));
            context
                .device
                .cmd_set_scissor(cmd, 0, std::slice::from_ref(&sc));

            let bufs = [self.vertex_buffer];
            context
                .device
                .cmd_bind_vertex_buffers(cmd, 0, &bufs, &[0u64]);
            context.device.cmd_draw(cmd, vert_count, 1, 0, 0);
            context.device.cmd_end_render_pass(cmd);
        }
        Ok(())
    }

    /// Discard framebuffers built for the previous swapchain image views.
    ///
    /// Must be called after the device is idle and **before** the old
    /// swapchain image views are destroyed, otherwise the cached framebuffers
    /// keep references to destroyed views (VUID-vkDestroyImageView-...).
    pub fn on_swapchain_views_changed(&mut self) {
        for cached in self.framebuffers.drain(..) {
            unsafe { self.device.destroy_framebuffer(cached.handle, None) };
        }
    }

    /// 把一个文本命令展开成逐字形四边形（像素 → NDC，字形带图集 uv）。
    fn emit_text(&mut self, vertices: &mut Vec<UiVertex>, t: &UiTextInput, w: f32, h: f32) {
        let [left, top, rect_w, rect_h] = t.rect;
        if t.font_size <= 0.0 || rect_w <= 0.0 || rect_h <= 0.0 {
            return;
        }
        if self.atlas.rasterizer.is_none() {
            return;
        }

        // 1. 测量整串宽度（advance 求和）。
        let mut total = 0.0f32;
        for ch in t.content.chars() {
            if let Some(g) = self.atlas.glyph(ch, t.font_size) {
                total += g.advance;
            }
        }
        if total <= 0.0 {
            return;
        }

        // 2. 对齐决定起始 x。
        let start_x = match t.alignment {
            UiTextAlign::Left => left,
            UiTextAlign::Center => left + (rect_w - total) * 0.5,
            UiTextAlign::Right => left + rect_w - total,
        };

        // 3. 逐字形发射四边形。基线来自后端行度量（ascent），拿不到时退回 0.8×字号。
        let baseline = top + self.atlas.baseline_offset(t.font_size);
        let [r, g, b, a] = t.color;
        let mut cursor_x = start_x;
        for ch in t.content.chars() {
            let Some(glyph) = self.atlas.glyph(ch, t.font_size) else {
                continue;
            };
            if let Some([gx, gy, gw, gh]) = glyph.rect {
                let px = cursor_x + glyph.xmin as f32;
                let py = baseline + glyph.ymin as f32;
                // 像素坐标（左上角原点，y 向下）→ Vulkan NDC（y=-1 屏幕顶，y=+1 屏幕底）。
                // 两者 y 方向一致，所以不做翻转；翻一次会把文字画成倒的。
                let x0 = (px / w) * 2.0 - 1.0;
                let y0 = pixel_y_to_ndc(py, h);
                let x1 = ((px + gw as f32) / w) * 2.0 - 1.0;
                let y1 = pixel_y_to_ndc(py + gh as f32, h);
                let inv = 1.0 / ATLAS_SIZE as f32;
                let u0 = gx as f32 * inv;
                let v0 = gy as f32 * inv;
                let u1 = (gx + gw) as f32 * inv;
                let v1 = (gy + gh) as f32 * inv;
                emit_quad(
                    vertices,
                    [x0, y0],
                    [x1, y1],
                    [u0, v0],
                    [u1, v1],
                    [r, g, b, a],
                );
            }
            cursor_x += glyph.advance;
        }
    }
}

/// 往顶点流里推一个四边形（两个三角形，顺时针）。
fn emit_quad(
    out: &mut Vec<UiVertex>,
    p0: [f32; 2],
    p1: [f32; 2],
    uv0: [f32; 2],
    uv1: [f32; 2],
    color: [f32; 4],
) {
    let [x0, y0] = p0;
    let [x1, y1] = p1;
    let [u0, v0] = uv0;
    let [u1, v1] = uv1;
    for &(px, py, u, v) in &[
        (x0, y0, u0, v0),
        (x1, y0, u1, v0),
        (x0, y1, u0, v1),
        (x1, y1, u1, v1),
        (x1, y0, u1, v0),
        (x0, y1, u0, v1),
    ] {
        out.push(UiVertex {
            pos: [px, py],
            uv: [u, v],
            color,
        });
    }
}

impl Drop for UiOverlay {
    fn drop(&mut self) {
        unsafe {
            if let Some(p) = self.pipeline {
                self.device.destroy_pipeline(p, None);
            }
            if let Some(l) = self.layout {
                self.device.destroy_pipeline_layout(l, None);
            }
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
            for cached in self.framebuffers.drain(..) {
                self.device.destroy_framebuffer(cached.handle, None);
            }
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_buffer(self.vertex_buffer, None);
            self.device.free_memory(self.vertex_memory, None);
        }
    }
}

// ---------------------------------------------------------------------------
// 一次性 command buffer / 内存类型 辅助（与 ibl.rs 同模式）
// ---------------------------------------------------------------------------

fn allocate_temp_command_buffer(
    device: &ash::Device,
    pool: vk::CommandPool,
) -> anyhow::Result<vk::CommandBuffer> {
    let cmd = unsafe {
        device.allocate_command_buffers(&vk::CommandBufferAllocateInfo {
            command_pool: pool,
            level: vk::CommandBufferLevel::PRIMARY,
            command_buffer_count: 1,
            ..Default::default()
        })
    }?[0];
    Ok(cmd)
}

fn submit_and_wait(
    device: &ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
) -> anyhow::Result<()> {
    let cmd_arr = [cmd];
    let submit = vk::SubmitInfo::default().command_buffers(&cmd_arr);
    let fence = unsafe {
        device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .context("UiOverlay submit_and_wait: create fence")?
    };
    unsafe {
        device
            .queue_submit(queue, &[submit], fence)
            .context("UiOverlay submit_and_wait: queue_submit")?;
        device
            .wait_for_fences(&[fence], true, u64::MAX)
            .context("UiOverlay submit_and_wait: wait_for_fences")?;
        device.destroy_fence(fence, None);
        device.free_command_buffers(pool, &cmd_arr);
    }
    Ok(())
}

fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
    flags: vk::MemoryPropertyFlags,
) -> u32 {
    for i in 0..props.memory_type_count {
        if (type_filter & (1 << i)) != 0
            && props.memory_types[i as usize]
                .property_flags
                .contains(flags)
        {
            return i;
        }
    }
    for i in 0..props.memory_type_count {
        if (type_filter & (1 << i)) != 0 {
            return i;
        }
    }
    panic!("no suitable memory type found");
}

/// 像素 y（左上角原点、向下增长）→ Vulkan NDC y（`-1` = 屏幕顶，`+1` = 屏幕底）。
///
/// 相机投影里的 `p[1][1] = -inv_tan` 就是按这个约定翻的，所以 UI 这里**不能再翻**：
/// 早先写成 `(h - py) / h * 2 - 1`（OpenGL 的 Y-up 直觉）把整个 UI 层画成了上下镜像
/// —— intro 的标题倒着显示、锚在底部的 prompt 跑到屏幕顶部。
fn pixel_y_to_ndc(py: f32, h: f32) -> f32 {
    (py / h) * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::pixel_y_to_ndc;

    #[test]
    fn pixel_y_maps_screen_top_to_negative_one() {
        // Vulkan NDC y 朝下：像素顶端是 -1，像素底端是 +1，屏幕中线是 0。
        assert_eq!(pixel_y_to_ndc(0.0, 720.0), -1.0);
        assert_eq!(pixel_y_to_ndc(720.0, 720.0), 1.0);
        assert_eq!(pixel_y_to_ndc(360.0, 720.0), 0.0);
    }

    #[test]
    fn pixel_y_keeps_vertical_order() {
        // 单调递增：像素 y 更大的点必须落在更下方（NDC y 更大）。
        // 这正是旧实现（`h - py`）违反的性质。
        let ys: Vec<f32> = (0..=10)
            .map(|i| pixel_y_to_ndc(i as f32 * 72.0, 720.0))
            .collect();
        for pair in ys.windows(2) {
            assert!(pair[1] > pair[0], "NDC y 必须随像素 y 单调递增");
        }
    }
}
