//! 3D 粒子 billboard 通道。
//!
//! [`ParticlePass`] 把引擎 CPU 模拟出来的粒子（[`ParticleDrawItem`]）作为
//! **实例化四边形**画进 ForwardPass 的 HDR 颜色附件。和
//! [`crate::sprite_pass::SpritePass`] 的差别只有三点：
//!
//! - **保留深度测试**（写关闭）：粒子在世界空间，前面的几何必须遮挡它们；
//! - **面向相机的展开**：顶点阶段用 `GraphFrame::inv_view_rot` 的 right / up
//!   列展开四边形，CPU 只提交中心点 + 尺寸；
//! - **不采样贴图**：alpha 由四边形 UV 的径向衰减算出（软边圆点），所以
//!   粒子不需要资产、也不需要 bindless 表。
//!
//! 排序（远到近）由引擎在提交前完成，`ForwardPass` 只按列表顺序绘制；
//! 混合是直通 alpha（`SRC_ALPHA` / `ONE_MINUS_SRC_ALPHA`）。

use std::mem::size_of;

use anyhow::{Context as _, Result};
use ash::vk;

use crate::buffer::{create_buffer, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
use crate::render_graph::ParticleDrawItem;
use crate::shader;
use crate::shader_bindings::particles as bindings;

const PARTICLE_VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/particles.vert.spv");
const PARTICLE_FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/particles.frag.spv");

/// 初始实例容量（按需增长）。
const INITIAL_CAPACITY: u32 = 256;

/// 单实例顶点数据 —— 必须与 `particles.slang` 的 `ParticleVertexIn` 一致。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ParticleInstance {
    /// xyz = 世界空间中心，w = 世界单位半径。
    center_size: [f32; 4],
    /// 线性 RGBA（已按粒子年龄插值过）。
    color: [f32; 4],
}

impl ParticleInstance {
    fn binding_description() -> vk::VertexInputBindingDescription {
        vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Self>() as u32)
            .input_rate(vk::VertexInputRate::INSTANCE)
    }

    fn attribute_descriptions() -> [vk::VertexInputAttributeDescription; 2] {
        let vec4 = |location: u32, offset: u32| {
            vk::VertexInputAttributeDescription::default()
                .location(location)
                .binding(0)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(offset)
        };
        [vec4(0, 0), vec4(1, 16)]
    }
}

/// 3D 粒子实例化绘制通道。
pub struct ParticlePass {
    pipeline: Option<GraphicsPipeline>,
    /// 管线编译时使用的 render pass（ForwardPass 重建后需要重编）。
    built_for_render_pass: Option<vk::RenderPass>,
    instance_buffer: vk::Buffer,
    instance_memory: vk::DeviceMemory,
    instance_capacity: u32,
    device: Option<ash::Device>,
}

impl ParticlePass {
    /// 创建空通道；实例缓冲在首次 [`ParticlePass::draw`] 时惰性分配
    /// （ForwardPass 构造时没有 `VulkanContext`）。
    pub fn new() -> Self {
        Self {
            pipeline: None,
            built_for_render_pass: None,
            instance_buffer: vk::Buffer::null(),
            instance_memory: vk::DeviceMemory::null(),
            instance_capacity: 0,
            device: None,
        }
    }

    /// 在已开启的 render pass 内绘制 `items`（按给定顺序，先画先被覆盖）。
    ///
    /// `inv_view_rot` 是「视图旋转的转置」（`GraphFrame::inv_view_rot`），
    /// 它的第 0 / 1 列就是相机的世界空间 right / up，用来把四边形展开成
    /// billboard。
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        device: &ash::Device,
        context: &VulkanContext,
        cmd: vk::CommandBuffer,
        render_pass: vk::RenderPass,
        view_proj: &[[f32; 4]; 4],
        inv_view_rot: &[[f32; 4]; 4],
        items: &[ParticleDrawItem],
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.ensure_pipeline(device, render_pass)?;
        self.grow(device, context, items.len() as u32)?;
        self.upload(context, items)?;

        let pipeline = self.pipeline.as_ref().expect("pipeline built above");

        // 相机基向量：`inv_view_rot` 的第 0 / 1 列（w = 0）。
        let camera_right = [
            inv_view_rot[0][0],
            inv_view_rot[0][1],
            inv_view_rot[0][2],
            0.0,
        ];
        let camera_up = [
            inv_view_rot[1][0],
            inv_view_rot[1][1],
            inv_view_rot[1][2],
            0.0,
        ];
        let push = bindings::ParticlePush {
            viewProj: *view_proj,
            camRight: camera_right,
            camUp: camera_up,
        };

        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
            device.cmd_push_constants(
                cmd,
                pipeline.layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                std::slice::from_raw_parts(
                    &push as *const bindings::ParticlePush as *const u8,
                    size_of::<bindings::ParticlePush>(),
                ),
            );
            device.cmd_bind_vertex_buffers(cmd, 0, &[self.instance_buffer], &[0]);
            // 6 vertices = 2 triangles, one instance per particle.
            device.cmd_draw(cmd, 6, items.len() as u32, 0, 0);
        }

        Ok(())
    }

    fn ensure_pipeline(&mut self, device: &ash::Device, render_pass: vk::RenderPass) -> Result<()> {
        if self.pipeline.is_some() && self.built_for_render_pass == Some(render_pass) {
            return Ok(());
        }
        self.device = Some(device.clone());
        self.pipeline = None;

        let vert_module =
            shader::load_shader_module(device, PARTICLE_VERT_SPV).context("ParticlePass: vert")?;
        let frag_module =
            shader::load_shader_module(device, PARTICLE_FRAG_SPV).context("ParticlePass: frag")?;
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

        let binding_descs = [ParticleInstance::binding_description()];
        let attr_descs = ParticleInstance::attribute_descriptions();

        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(size_of::<bindings::ParticlePush>() as u32)];

        // ForwardPass' render pass 有两个颜色附件（HDR 颜色 + 视图空间法线）。
        // 粒子只混到 SV_Target0，法线目标写掩码留空。
        let blend_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
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
            descriptor_set_layouts: &[],
            push_constant_ranges: &push,
            render_pass,
            subpass: 0,
            cull_mode: Some(vk::CullModeFlags::NONE),
            depth_bias_enable: None,
            depth_bias_constant_factor: None,
            depth_bias_slope_factor: None,
            // 粒子是半透明的世界空间面片：测深度（被几何遮挡），不写深度。
            depth_write_enable: Some(false),
            depth_test_enable: Some(true),
            color_attachment_count: None,
            color_blend_attachments: Some(&blend_attachments),
            front_face: None,
            polygon_mode: None,
        })
        .context("ParticlePass: create pipeline")?;

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        self.pipeline = Some(pipeline);
        self.built_for_render_pass = Some(render_pass);
        Ok(())
    }

    fn grow(&mut self, device: &ash::Device, context: &VulkanContext, needed: u32) -> Result<()> {
        if needed <= self.instance_capacity {
            return Ok(());
        }
        let capacity = needed.next_power_of_two().max(INITIAL_CAPACITY);
        let (buffer, memory) = create_buffer(
            context,
            capacity as vk::DeviceSize * size_of::<ParticleInstance>() as vk::DeviceSize,
            BufferUsage::VERTEX_BUFFER,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("ParticlePass: grow instance buffer")?;
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

    fn upload(&self, context: &VulkanContext, items: &[ParticleDrawItem]) -> Result<()> {
        let bytes = items.len() * size_of::<ParticleInstance>();
        let instances: Vec<ParticleInstance> = items
            .iter()
            .map(|item| ParticleInstance {
                center_size: [item.center[0], item.center[1], item.center[2], item.size],
                color: item.color,
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
                .context("ParticlePass: map instance memory")?;
            std::ptr::copy_nonoverlapping(instances.as_ptr() as *const u8, ptr as *mut u8, bytes);
            context.device.unmap_memory(self.instance_memory);
        }
        Ok(())
    }

    /// 交换链重建 / 销毁前释放 GPU 资源。
    pub fn destroy(&mut self, device: &ash::Device) {
        self.pipeline = None;
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

impl Default for ParticlePass {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ParticlePass {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            self.pipeline = None;
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
    use super::*;

    /// 着色器里的 push constant 块是 `viewProj`(64) + `camRight`(16) +
    /// `camUp`(16)；Rust 侧必须一个字节都不差，否则相机基向量会错位。
    #[test]
    fn push_constant_layout_matches_the_shader() {
        assert_eq!(size_of::<bindings::ParticlePush>(), 96);
        assert_eq!(bindings::PUSH_CONSTANT_SIZE, 96);
        let push = bindings::ParticlePush::default();
        assert_eq!(
            (&push.viewProj as *const _ as usize) - (&push as *const _ as usize),
            0
        );
        assert_eq!(
            (&push.camRight as *const _ as usize) - (&push as *const _ as usize),
            64
        );
        assert_eq!(
            (&push.camUp as *const _ as usize) - (&push as *const _ as usize),
            80
        );
    }

    /// 单个实例记录 = center(12) + size(4) + color(16)。
    #[test]
    fn the_instance_record_is_thirty_two_bytes() {
        assert_eq!(size_of::<ParticleInstance>(), 32);
    }
}
