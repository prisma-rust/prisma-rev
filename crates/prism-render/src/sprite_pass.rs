//! 2D 精灵通道。
//!
//! [`SpritePass`] 把 ECS 收集来的精灵实例（[`SpriteDrawItem`]）作为
//! **实例化四边形**绘制到 ForwardPass 的 HDR 颜色附件里——和
//! [`crate::skybox_pass::SkyboxPass`] 一样，它不拥有自己的 render pass /
//! 帧缓冲，而是在 ForwardPass 已开启的 pass 内记录命令。
//!
//! 设计要点（见 `docs/plans/2026-10-27-cozy-fishing-design.md`）：
//!
//! - **一次 draw call**：所有精灵共享一个实例缓冲，纹理索引进 bindless 表；
//! - **关闭深度测试/写入**：2D 的前后关系由 CPU 侧排序
//!   （`sorting_layer` → `order_in_layer` → `z`）保证，不依赖深度缓冲；
//! - **Nearest 采样**：像素画在 `sprites.slang` 里固定用 bindless 表里的
//!   nearest 采样器，帧矩形由 CPU 预先 clamp，避免精灵表串色；
//! - 输出到线性 HDR 目标，由 `PostPass` 在 `tonemap_mode = 2`（直通）时
//!   保持原始调色板。
//!
//! 描述符布局：绑定 ForwardPass 共享的 bindless 表，但放在 **set 0**
//! （该布局对象与集合索引无关；scene 管线把它绑在 set 1）。

use std::mem::size_of;

use anyhow::{Context as _, Result};
use ash::vk;

use crate::buffer::{create_buffer, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;
use crate::pipeline::{blend_factor, blend_operation, GraphicsPipeline, PipelineDesc};
use crate::render_graph::SpriteDrawItem;
use crate::shader;
use crate::shader_bindings::sprites as bindings;
use prism_gfx::{BlendMode, BlendOp};

const SPRITE_VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/sprites.vert.spv");
const SPRITE_FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/sprites.frag.spv");

/// 初始实例容量（按需增长）。
const INITIAL_CAPACITY: u32 = 256;

/// 单实例顶点数据 —— 必须与 `sprites.slang` 的 `SpriteVertexIn` 一致。
///
/// 四个角点是**世界空间**坐标（顺序 `BL, BR, TR, TL`）：精灵的模型矩阵在 CPU
/// 侧展开，折线的三角形直接给顶点（#604）。三角形 = 最后两个角点重合的四边形。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SpriteInstance {
    /// 四边形四个角（世界空间）。
    corners: [[f32; 3]; 4],
    /// UV 矩形 `(u0, v0, u1, v1)`。
    uv_rect: [f32; 4],
    /// 线性 RGBA 乘算色。
    color: [f32; 4],
    /// bindless 纹理索引。
    texture: u32,
    _pad: [u32; 3],
}

impl SpriteInstance {
    fn binding_description() -> vk::VertexInputBindingDescription {
        vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Self>() as u32)
            .input_rate(vk::VertexInputRate::INSTANCE)
    }

    fn attribute_descriptions() -> [vk::VertexInputAttributeDescription; 7] {
        let vec4 = |location: u32, offset: u32| {
            vk::VertexInputAttributeDescription::default()
                .location(location)
                .binding(0)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(offset)
        };
        let vec3 = |location: u32, offset: u32| {
            vk::VertexInputAttributeDescription::default()
                .location(location)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(offset)
        };
        [
            vec3(0, 0),
            vec3(1, 12),
            vec3(2, 24),
            vec3(3, 36),
            vec4(4, 48),
            vec4(5, 64),
            vk::VertexInputAttributeDescription::default()
                .binding(0)
                .location(6)
                .format(vk::Format::R32_UINT)
                .offset(80),
        ]
    }
}

/// 由模型矩阵展开四个世界空间角点（与旧 vertex shader 的
/// `m0*x + m1*y + m3` 完全一致，顺序 `BL, BR, TR, TL`）。
fn corners_from_model(model: &[[f32; 4]; 4]) -> [[f32; 3]; 4] {
    const UNIT: [[f32; 2]; 4] = [[-0.5, -0.5], [0.5, -0.5], [0.5, 0.5], [-0.5, 0.5]];
    let (m0, m1, m3) = (model[0], model[1], model[3]);
    let mut out = [[0.0f32; 3]; 4];
    for (corner, xy) in out.iter_mut().zip(UNIT) {
        for (axis, value) in corner.iter_mut().enumerate() {
            *value = m0[axis] * xy[0] + m1[axis] * xy[1] + m3[axis];
        }
    }
    out
}

/// `(src, dst, op)` 唯一标识一条混合管线。
type BlendKey = (BlendMode, BlendMode, BlendOp);

/// 2D 精灵实例化绘制通道。
pub struct SpritePass {
    /// 按混合状态缓存的管线（#610）：Alpha / Additive / Multiply 共用实例缓冲。
    pipelines: std::collections::HashMap<BlendKey, GraphicsPipeline>,
    /// 管线编译时使用的 render pass（ForwardPass 重建后需要重编）。
    built_for_render_pass: Option<vk::RenderPass>,
    instance_buffer: vk::Buffer,
    instance_memory: vk::DeviceMemory,
    instance_capacity: u32,
    device: Option<ash::Device>,
}

impl SpritePass {
    /// Creates an empty pass; the instance buffer is allocated lazily on the
    /// first [`SpritePass::draw`] (ForwardPass is constructed without a
    /// `VulkanContext`, so it cannot allocate GPU memory here).
    pub fn new() -> Self {
        Self {
            pipelines: std::collections::HashMap::new(),
            built_for_render_pass: None,
            instance_buffer: vk::Buffer::null(),
            instance_memory: vk::DeviceMemory::null(),
            instance_capacity: 0,
            device: None,
        }
    }
}

impl Default for SpritePass {
    fn default() -> Self {
        Self::new()
    }
}

impl SpritePass {
    /// 在已开启的 render pass 内绘制 `items`（按给定顺序，先画先覆盖）。
    ///
    /// `bindless_set` / `bindless_layout` 由 ForwardPass 提供；`view_proj`
    /// 是本帧世界→裁剪矩阵（2D 时即正交相机矩阵）。
    pub fn draw(
        &mut self,
        device: &ash::Device,
        context: &VulkanContext,
        cmd: vk::CommandBuffer,
        render_pass: vk::RenderPass,
        bindless_set: vk::DescriptorSet,
        bindless_layout: vk::DescriptorSetLayout,
        view_proj: &[[f32; 4]; 4],
        items: &[SpriteDrawItem],
    ) -> Result<()> {
        if items.is_empty() || bindless_set == vk::DescriptorSet::null() {
            return Ok(());
        }
        if self.built_for_render_pass != Some(render_pass) {
            self.pipelines.clear();
            self.built_for_render_pass = Some(render_pass);
        }
        self.grow(device, context, items.len() as u32)?;
        self.upload(context, items)?;

        // 保持 CPU 排序顺序，只把相邻的同混合状态合并成一次 draw。
        // 混合模式不是排序键，所以交错的精确顺序仍然保留。
        let mut start = 0usize;
        while start < items.len() {
            let key = (
                items[start].src_blend,
                items[start].dst_blend,
                items[start].blend_op,
            );
            let mut end = start + 1;
            while end < items.len()
                && (
                    items[end].src_blend,
                    items[end].dst_blend,
                    items[end].blend_op,
                ) == key
            {
                end += 1;
            }
            self.ensure_pipeline(device, render_pass, bindless_layout, key)?;
            let pipeline = self.pipelines.get(&key).expect("pipeline built above");
            unsafe {
                device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipeline.layout,
                    0,
                    std::slice::from_ref(&bindless_set),
                    &[],
                );
                device.cmd_push_constants(
                    cmd,
                    pipeline.layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    std::slice::from_raw_parts(
                        view_proj as *const [[f32; 4]; 4] as *const u8,
                        size_of::<[[f32; 4]; 4]>(),
                    ),
                );
                device.cmd_bind_vertex_buffers(cmd, 0, &[self.instance_buffer], &[0]);
                // 6 vertices = 2 triangles; one instance per sprite.
                device.cmd_draw(cmd, 6, (end - start) as u32, 0, start as u32);
            }
            start = end;
        }

        Ok(())
    }

    fn ensure_pipeline(
        &mut self,
        device: &ash::Device,
        render_pass: vk::RenderPass,
        bindless_layout: vk::DescriptorSetLayout,
        blend: BlendKey,
    ) -> Result<()> {
        if self.pipelines.contains_key(&blend) {
            return Ok(());
        }
        self.device = Some(device.clone());

        let vert_module =
            shader::load_shader_module(device, SPRITE_VERT_SPV).context("SpritePass: vert")?;
        let frag_module =
            shader::load_shader_module(device, SPRITE_FRAG_SPV).context("SpritePass: frag")?;
        let vert_entry = std::ffi::CString::new(bindings::ENTRY_VERTEX_MAIN).unwrap();
        let frag_entry = std::ffi::CString::new(bindings::ENTRY_FRAGMENT_MAIN).unwrap();
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

        let binding_descs = [SpriteInstance::binding_description()];
        let attr_descs = SpriteInstance::attribute_descriptions();

        // Push constant = `SpritePush` (one mat4 = 64 bytes, vertex stage only).
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(size_of::<bindings::SpritePush>() as u32)];

        // ForwardPass' render pass has two color attachments (HDR color +
        // view-space normal). Sprites blend into SV_Target0 and leave the
        // normal target untouched (write mask empty), mirroring SkyboxPass.
        let (src_blend, dst_blend, blend_op) = blend;
        let alpha_blend = src_blend == BlendMode::SrcAlpha
            && dst_blend == BlendMode::OneMinusSrcAlpha
            && blend_op == BlendOp::Add;
        let blend_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)
                .blend_enable(true)
                .src_color_blend_factor(blend_factor(src_blend))
                .dst_color_blend_factor(blend_factor(dst_blend))
                .color_blend_op(blend_operation(blend_op))
                // alpha 通道只在标准 alpha 混合时累乘；加色 / 乘法的 alpha
                // 保持目标值，避免把后面的精灵变透明。
                .src_alpha_blend_factor(if alpha_blend {
                    vk::BlendFactor::ONE
                } else {
                    vk::BlendFactor::ZERO
                })
                .dst_alpha_blend_factor(if alpha_blend {
                    vk::BlendFactor::ZERO
                } else {
                    vk::BlendFactor::ONE
                })
                .alpha_blend_op(vk::BlendOp::ADD),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty())
                .blend_enable(false),
        ];

        let pipeline = GraphicsPipeline::new(&PipelineDesc {
            device,
            shader_stages: &shader_stages,
            vertex_binding_desc: &binding_descs,
            vertex_attr_descs: &attr_descs,
            descriptor_set_layouts: std::slice::from_ref(&bindless_layout),
            push_constant_ranges: &push,
            render_pass,
            subpass: 0,
            cull_mode: Some(vk::CullModeFlags::NONE),
            depth_bias_enable: None,
            depth_bias_constant_factor: None,
            depth_bias_slope_factor: None,
            // 2D 顺序由 CPU 排序决定：不测试也不写深度。
            depth_write_enable: Some(false),
            depth_test_enable: Some(false),
            color_attachment_count: None,
            color_blend_attachments: Some(&blend_attachments),
            front_face: None,
        })
        .context("SpritePass: create pipeline")?;

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        self.pipelines.insert(blend, pipeline);
        self.built_for_render_pass = Some(render_pass);
        Ok(())
    }

    fn grow(&mut self, device: &ash::Device, context: &VulkanContext, needed: u32) -> Result<()> {
        if needed <= self.instance_capacity {
            return Ok(());
        }
        let capacity = needed.next_power_of_two().max(INITIAL_CAPACITY);
        // First allocation happens here (capacity == 0), so there is no old
        // buffer to release yet.
        let (buffer, memory) = create_buffer(
            context,
            capacity as vk::DeviceSize * size_of::<SpriteInstance>() as vk::DeviceSize,
            BufferUsage::VERTEX_BUFFER,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("SpritePass: grow instance buffer")?;
        unsafe {
            if self.instance_buffer != vk::Buffer::null() {
                device.destroy_buffer(self.instance_buffer, None);
            }
            if self.instance_memory != vk::DeviceMemory::null() {
                device.free_memory(self.instance_memory, None);
            }
        }
        self.instance_buffer = buffer;
        self.instance_memory = memory;
        self.instance_capacity = capacity;
        Ok(())
    }

    fn upload(&self, context: &VulkanContext, items: &[SpriteDrawItem]) -> Result<()> {
        let bytes = items.len() * size_of::<SpriteInstance>();
        let instances: Vec<SpriteInstance> = items
            .iter()
            .map(|item| SpriteInstance {
                corners: item
                    .corners
                    .unwrap_or_else(|| corners_from_model(&item.model)),
                uv_rect: item.uv_rect,
                color: item.color,
                texture: item.texture,
                _pad: [0; 3],
            })
            .collect();
        unsafe {
            let ptr = context
                .device
                .map_memory(
                    self.instance_memory,
                    0,
                    bytes as vk::DeviceSize,
                    vk::MemoryMapFlags::empty(),
                )
                .context("SpritePass: map instance memory")?;
            std::ptr::copy_nonoverlapping(instances.as_ptr() as *const u8, ptr as *mut u8, bytes);
            context.device.unmap_memory(self.instance_memory);
        }
        Ok(())
    }

    /// 交换链重建 / 销毁前释放 GPU 资源。
    pub fn destroy(&mut self, device: &ash::Device) {
        self.pipelines.clear();
        self.built_for_render_pass = None;
        unsafe {
            if self.instance_buffer != vk::Buffer::null() {
                device.destroy_buffer(self.instance_buffer, None);
                self.instance_buffer = vk::Buffer::null();
            }
            if self.instance_memory != vk::DeviceMemory::null() {
                device.free_memory(self.instance_memory, None);
                self.instance_memory = vk::DeviceMemory::null();
            }
        }
        self.instance_capacity = 0;
        self.device = None;
    }
}

impl Drop for SpritePass {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            self.pipelines.clear();
            unsafe {
                if self.instance_buffer != vk::Buffer::null() {
                    device.destroy_buffer(self.instance_buffer, None);
                }
                if self.instance_memory != vk::DeviceMemory::null() {
                    device.free_memory(self.instance_memory, None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{corners_from_model, SpriteInstance};

    /// #604：角点由模型矩阵展开，必须与旧 vertex shader 的
    /// `m0*x + m1*y + m3` 逐位一致（顺序 BL, BR, TR, TL）。
    #[test]
    fn model_corners_match_the_old_vertex_shader() {
        let model = [
            [4.0, 0.0, 0.0, 0.0],
            [0.0, 2.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [10.0, 20.0, 0.0, 1.0],
        ];
        let corners = corners_from_model(&model);
        assert_eq!(corners[0], [8.0, 19.0, 0.0]);
        assert_eq!(corners[1], [12.0, 19.0, 0.0]);
        assert_eq!(corners[2], [12.0, 21.0, 0.0]);
        assert_eq!(corners[3], [8.0, 21.0, 0.0]);
    }

    /// 顶点布局是 Rust 与 `sprites.slang` 之间的手工契约，改一边就必须改另一边。
    #[test]
    fn the_instance_layout_matches_the_shader_locations() {
        assert_eq!(std::mem::size_of::<SpriteInstance>(), 96);
        let attrs = SpriteInstance::attribute_descriptions();
        let locations: Vec<u32> = attrs.iter().map(|attr| attr.location).collect();
        assert_eq!(locations, vec![0, 1, 2, 3, 4, 5, 6]);
        let offsets: Vec<u32> = attrs.iter().map(|attr| attr.offset).collect();
        assert_eq!(offsets, vec![0, 12, 24, 36, 48, 64, 80]);
    }
}
