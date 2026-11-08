//! 世界空间 XYZ 方向辅助工具（始终置顶的调试辅助）。
//!
//! 从场景原点绘制三个彩色箭头——X = 红色，Y = 绿色，Z = 蓝色——
//! 使观察者一目了然地读取世界坐标轴。禁用深度测试渲染，
//! 因此不会被 3D 场景遮挡。

use std::ffi::CString;
use std::mem::size_of;

use anyhow::Context as _;
use ash::vk;

use crate::buffer::{create_buffer, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;
use crate::shader_bindings::gizmo::GizmoPush;

/// One ECS-authored gizmo instance. `model` already contains the entity's
/// world transform and the component's scale. `mode`: 0 = translate, 1 = rotate, 2 = scale.
#[derive(Clone, Copy, Debug)]
pub struct GizmoInstance {
    pub model: [[f32; 4]; 4],
    pub mode: u32,
}

const GIZMO_VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/gizmo.vert.spv");
const GIZMO_FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/gizmo.frag.spv");

/// Per-vertex data for the gizmo: object-space position + color.
#[repr(C)]
#[derive(Clone, Copy)]
struct GizmoVertex {
    pos: [f32; 3],
    color: [f32; 3],
}

impl GizmoVertex {
    fn binding_description() -> vk::VertexInputBindingDescription {
        vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Self>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)
    }

    fn attribute_descriptions() -> [vk::VertexInputAttributeDescription; 2] {
        [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(12),
        ]
    }
}

pub struct Gizmo {
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    vertex_buffer: vk::Buffer,
    vertex_memory: vk::DeviceMemory,
    translate_count: u32,
    rotate_first: u32,
    rotate_count: u32,
    scale_first: u32,
    scale_count: u32,
    device: ash::Device,
}

impl Gizmo {
    /// Build the gizmo geometry + pipeline for `render_pass`.
    pub fn new(context: &VulkanContext, render_pass: vk::RenderPass) -> anyhow::Result<Self> {
        let device = context.device.clone();

        // --- Static geometry (generated once on the CPU) ---
        let (verts, translate_count, (rotate_first, rotate_count), (scale_first, scale_count)) =
            generate_all_gizmos();
        let vertex_count = verts.len() as u32;
        let buf_size = (vertex_count as usize * size_of::<GizmoVertex>()) as vk::DeviceSize;
        let (vertex_buffer, vertex_memory) = create_buffer(
            context,
            buf_size,
            BufferUsage::VERTEX_BUFFER,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("create gizmo vertex buffer")?;
        unsafe {
            let ptr = device
                .map_memory(vertex_memory, 0, buf_size, vk::MemoryMapFlags::empty())
                .context("map gizmo vertex memory")?;
            std::ptr::copy_nonoverlapping(
                verts.as_ptr() as *const u8,
                ptr as *mut u8,
                buf_size as usize,
            );
            device.unmap_memory(vertex_memory);
        }

        // --- Shaders ---
        let vert_module =
            crate::shader::load_shader_module(&device, GIZMO_VERT_SPV).context("gizmo vert")?;
        let frag_module =
            crate::shader::load_shader_module(&device, GIZMO_FRAG_SPV).context("gizmo frag")?;
        // Entry-point names from Slang reflection (slangc keeps vertexMain /
        // fragmentMain via -fvk-use-entrypoint-name; see shader_bindings).
        let vert_entry = CString::new(crate::shader_bindings::gizmo::ENTRY_VERTEX_MAIN).unwrap();
        let frag_entry = CString::new(crate::shader_bindings::gizmo::ENTRY_FRAGMENT_MAIN).unwrap();
        let vert_stage = crate::shader::shader_stage(
            vk::ShaderStageFlags::VERTEX,
            vert_module,
            vert_entry.as_c_str(),
        );
        let frag_stage = crate::shader::shader_stage(
            vk::ShaderStageFlags::FRAGMENT,
            frag_module,
            frag_entry.as_c_str(),
        );
        let shader_stages = [vert_stage, frag_stage];

        // --- pipeline layout: push constants = viewProj + model (2 mat4) ---
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(128)];
        let layout_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_constant_ranges);
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }
            .context("create gizmo pipeline layout")?;

        // --- Fixed-function state ---
        let binding_desc = [GizmoVertex::binding_description()];
        let attr_descs = GizmoVertex::attribute_descriptions();
        let vertex_input_info = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&binding_desc)
            .vertex_attribute_descriptions(&attr_descs);
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST)
            .primitive_restart_enable(false);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
            .depth_clamp_enable(false)
            .rasterizer_discard_enable(false)
            .polygon_mode(vk::PolygonMode::FILL)
            .line_width(1.0)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .depth_bias_enable(false);
        let multisampling = vk::PipelineMultisampleStateCreateInfo::default()
            .sample_shading_enable(false)
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // Depth OFF → always drawn on top of the scene.
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(false)
            .depth_write_enable(false);
        let color_blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(false);
        // The gizmo draws inside ForwardPass's render pass, which has 3 color
        // attachments (color + view-normal MRT + motion vectors). Every pipeline
        // bound in that render pass must declare a matching attachmentCount, so we
        // list 3 blend states: attachment 0 (color) writes RGBA, while attachments
        // 1 and 2 (normal, motion) keep an empty write mask so the cleared values
        // are preserved.
        let blend_attachments = [
            color_blend_attachment,
            // 附件 1 = view-space 法线 MRT：空写入遮罩，保留 ForwardPass 清好的值。
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty())
                .blend_enable(false),
            // 附件 2 = 运动矢量（#883）：同样只写 0 号附件，否则
            // `pColorBlendState->attachmentCount` 与渲染 pass 的
            // 3 个颜色附件不一致（#973，VUID-...-renderPass-07609）。
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty())
                .blend_enable(false),
        ];
        let color_blend_state =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);

        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&shader_stages)
            .vertex_input_state(&vertex_input_info)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .dynamic_state(&dynamic_state)
            .rasterization_state(&rasterizer)
            .multisample_state(&multisampling)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend_state)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipeline = unsafe {
            device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
                .map_err(|(_, e)| e)
                .context("create gizmo pipeline")?
        }[0];

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        Ok(Self {
            pipeline,
            layout,
            vertex_buffer,
            vertex_memory,
            translate_count,
            rotate_first,
            rotate_count,
            scale_first,
            scale_count,
            device,
        })
    }

    /// Record one draw per ECS-authored gizmo. Call between `begin_frame`
    /// and `end_frame`, after the 3D scene draws.
    pub fn draw(
        &self,
        cmd: vk::CommandBuffer,
        view_proj: &[[f32; 4]; 4],
        instances: &[GizmoInstance],
    ) {
        if instances.is_empty() {
            return;
        }
        let device = &self.device;
        let buffers = [self.vertex_buffer];
        let offsets = [0u64];
        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            device.cmd_bind_vertex_buffers(cmd, 0, &buffers, &offsets);
            for instance in instances {
                let push = GizmoPush {
                    viewProj: *view_proj,
                    model: instance.model,
                };
                device.cmd_push_constants(
                    cmd,
                    self.layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    bytemuck::bytes_of(&push),
                );
                let (first, count) = match instance.mode {
                    1 => (self.rotate_first, self.rotate_count),
                    2 => (self.scale_first, self.scale_count),
                    _ => (0, self.translate_count),
                };
                device.cmd_draw(cmd, count, 1, first, 0);
            }
        }
    }
}

impl Drop for Gizmo {
    fn drop(&mut self) {
        unsafe {
            self.device.device_wait_idle().ok();
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device.destroy_buffer(self.vertex_buffer, None);
            self.device.free_memory(self.vertex_memory, None);
        }
    }
}

// ---------------------------------------------------------------------------
// Geometry generation
// ---------------------------------------------------------------------------

/// 构建平移箭头、旋转圆环和缩放手柄的几何体
fn generate_all_gizmos() -> (Vec<GizmoVertex>, u32, (u32, u32), (u32, u32)) {
    let mut v = Vec::new();
    let len = 1.5f32; // shaft length
    let thick = 0.045f32; // shaft half-thickness
    let head_len = 0.32f32; // cone height
    let head_r = 0.13f32; // cone base radius
    let segs = 20u32; // cone radial segments

    // (direction, perpendicular-1, perpendicular-2, 颜色)
    type Axis = ([f32; 3], [f32; 3], [f32; 3], [f32; 3]);
    let axes: [Axis; 3] = [
        (
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.25, 0.25],
        ),
        (
            [0.0, 1.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.3, 1.0, 0.3],
        ),
        (
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.3, 0.5, 1.0],
        ),
    ];

    // 1. Translate (平移箭头 + 2D 平面手柄角标)
    for (dir, p1, p2, color) in axes {
        let geo = AxisGeometry {
            dir,
            p1,
            p2,
            len,
            color,
        };
        push_shaft(&mut v, &geo, thick);
        push_cone(&mut v, &geo, head_len, head_r, segs);
    }
    // XY 面 (法线 +Z，蓝色)
    push_plane_handle(&mut v, [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.3, 0.5, 1.0]);
    // XZ 面 (法线 +Y，绿色，地表平移)
    push_plane_handle(&mut v, [1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.3, 1.0, 0.3]);
    // YZ 面 (法线 +X，红色)
    push_plane_handle(&mut v, [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.25, 0.25]);
    let translate_count = v.len() as u32;

    // 2. Rotate (旋转圆环)
    let rotate_first = v.len() as u32;
    let ring_radius = 1.5f32;
    let ring_thick = 0.025f32;
    let ring_segs = 48u32;
    for (dir, p1, p2, color) in axes {
        push_ring(
            &mut v,
            dir,
            p1,
            p2,
            ring_radius,
            ring_thick,
            ring_segs,
            color,
        );
    }
    let rotate_count = (v.len() as u32) - rotate_first;

    // 3. Scale (缩放轴 + 端点立方体 + 中心均匀缩放立方体)
    let scale_first = v.len() as u32;
    let cube_half = 0.09f32;
    for (dir, p1, p2, color) in axes {
        let geo = AxisGeometry {
            dir,
            p1,
            p2,
            len,
            color,
        };
        push_shaft(&mut v, &geo, thick);
        let tip_center = [dir[0] * len, dir[1] * len, dir[2] * len];
        push_box(&mut v, tip_center, [cube_half; 3], color);
    }
    push_box(&mut v, [0.0, 0.0, 0.0], [0.08; 3], [0.9, 0.9, 0.9]);
    let scale_count = (v.len() as u32) - scale_first;

    (
        v,
        translate_count,
        (rotate_first, rotate_count),
        (scale_first, scale_count),
    )
}

fn push_box(v: &mut Vec<GizmoVertex>, center: [f32; 3], half: [f32; 3], color: [f32; 3]) {
    let (cx, cy, cz) = (center[0], center[1], center[2]);
    let (hx, hy, hz) = (half[0], half[1], half[2]);
    let p = [
        [cx - hx, cy - hy, cz - hz],
        [cx + hx, cy - hy, cz - hz],
        [cx + hx, cy + hy, cz - hz],
        [cx - hx, cy + hy, cz - hz],
        [cx - hx, cy - hy, cz + hz],
        [cx + hx, cy - hy, cz + hz],
        [cx + hx, cy + hy, cz + hz],
        [cx - hx, cy + hy, cz + hz],
    ];
    // -Z
    push_tri(v, p[0], p[2], p[1], color);
    push_tri(v, p[0], p[3], p[2], color);
    // +Z
    push_tri(v, p[4], p[5], p[6], color);
    push_tri(v, p[4], p[6], p[7], color);
    // -X
    push_tri(v, p[0], p[4], p[7], color);
    push_tri(v, p[0], p[7], p[3], color);
    // +X
    push_tri(v, p[1], p[2], p[6], color);
    push_tri(v, p[1], p[6], p[5], color);
    // -Y
    push_tri(v, p[0], p[1], p[5], color);
    push_tri(v, p[0], p[5], p[4], color);
    // +Y
    push_tri(v, p[3], p[7], p[6], color);
    push_tri(v, p[3], p[6], p[2], color);
}

fn push_ring(
    v: &mut Vec<GizmoVertex>,
    dir: [f32; 3],
    p1: [f32; 3],
    p2: [f32; 3],
    radius: f32,
    thick: f32,
    segs: u32,
    color: [f32; 3],
) {
    for i in 0..segs {
        let a0 = (i as f32 / segs as f32) * std::f32::consts::TAU;
        let a1 = ((i + 1) as f32 / segs as f32) * std::f32::consts::TAU;
        let (c0, s0) = (a0.cos(), a0.sin());
        let (c1, s1) = (a1.cos(), a1.sin());

        let rad0 = [
            p1[0] * c0 + p2[0] * s0,
            p1[1] * c0 + p2[1] * s0,
            p1[2] * c0 + p2[2] * s0,
        ];
        let rad1 = [
            p1[0] * c1 + p2[0] * s1,
            p1[1] * c1 + p2[1] * s1,
            p1[2] * c1 + p2[2] * s1,
        ];

        // 平面内带状表面
        let r_in = radius - thick;
        let r_out = radius + thick;
        let p0_in = [rad0[0] * r_in, rad0[1] * r_in, rad0[2] * r_in];
        let p0_out = [rad0[0] * r_out, rad0[1] * r_out, rad0[2] * r_out];
        let p1_in = [rad1[0] * r_in, rad1[1] * r_in, rad1[2] * r_in];
        let p1_out = [rad1[0] * r_out, rad1[1] * r_out, rad1[2] * r_out];
        push_tri(v, p0_in, p0_out, p1_out, color);
        push_tri(v, p0_in, p1_out, p1_in, color);

        // 垂直于平面的带状表面（保证各视角可见）
        let p0_up = [
            rad0[0] * radius + dir[0] * thick,
            rad0[1] * radius + dir[1] * thick,
            rad0[2] * radius + dir[2] * thick,
        ];
        let p0_down = [
            rad0[0] * radius - dir[0] * thick,
            rad0[1] * radius - dir[1] * thick,
            rad0[2] * radius - dir[2] * thick,
        ];
        let p1_up = [
            rad1[0] * radius + dir[0] * thick,
            rad1[1] * radius + dir[1] * thick,
            rad1[2] * radius + dir[2] * thick,
        ];
        let p1_down = [
            rad1[0] * radius - dir[0] * thick,
            rad1[1] * radius - dir[1] * thick,
            rad1[2] * radius - dir[2] * thick,
        ];
        push_tri(v, p0_down, p0_up, p1_up, color);
        push_tri(v, p0_down, p1_up, p1_down, color);
    }
}

fn push_tri(v: &mut Vec<GizmoVertex>, a: [f32; 3], b: [f32; 3], c: [f32; 3], color: [f32; 3]) {
    v.push(GizmoVertex { pos: a, color });
    v.push(GizmoVertex { pos: b, color });
    v.push(GizmoVertex { pos: c, color });
}

/// Geometry common to both shaft and cone: axis direction, two perpendicular
/// vectors, length, and color. Extracted as a struct to avoid passing 6+
/// positional arguments.
struct AxisGeometry {
    dir: [f32; 3],
    p1: [f32; 3],
    p2: [f32; 3],
    len: f32,
    color: [f32; 3],
}

/// Push a thin axis-aligned box from the origin to `dir * len`.
fn push_shaft(v: &mut Vec<GizmoVertex>, geo: &AxisGeometry, t: f32) {
    let dir = geo.dir;
    let p1 = geo.p1;
    let p2 = geo.p2;
    let len = geo.len;
    let color = geo.color;
    let corner = |s: f32, a: f32, b: f32| -> [f32; 3] {
        [
            dir[0] * len * s + p1[0] * t * a + p2[0] * t * b,
            dir[1] * len * s + p1[1] * t * a + p2[1] * t * b,
            dir[2] * len * s + p1[2] * t * a + p2[2] * t * b,
        ]
    };
    let c000 = corner(0.0, -1.0, -1.0);
    let c001 = corner(0.0, -1.0, 1.0);
    let c010 = corner(0.0, 1.0, -1.0);
    let c011 = corner(0.0, 1.0, 1.0);
    let c100 = corner(1.0, -1.0, -1.0);
    let c101 = corner(1.0, -1.0, 1.0);
    let c110 = corner(1.0, 1.0, -1.0);
    let c111 = corner(1.0, 1.0, 1.0);

    // -p1 face
    push_tri(v, c000, c001, c011, color);
    push_tri(v, c000, c011, c010, color);
    // +p1 face
    push_tri(v, c100, c110, c111, color);
    push_tri(v, c100, c111, c101, color);
    // -p2 face
    push_tri(v, c000, c010, c110, color);
    push_tri(v, c000, c110, c100, color);
    // +p2 face
    push_tri(v, c001, c101, c111, color);
    push_tri(v, c001, c111, c011, color);
    // -dir face (base)
    push_tri(v, c000, c100, c101, color);
    push_tri(v, c000, c101, c001, color);
    // +dir face (tip)
    push_tri(v, c010, c011, c111, color);
    push_tri(v, c010, c111, c110, color);
}

/// Push a cone (side + base cap) at the tip of the axis.
fn push_cone(v: &mut Vec<GizmoVertex>, geo: &AxisGeometry, head_len: f32, head_r: f32, segs: u32) {
    let dir = geo.dir;
    let p1 = geo.p1;
    let p2 = geo.p2;
    let len = geo.len;
    let color = geo.color;
    let apex = [
        dir[0] * (len + head_len),
        dir[1] * (len + head_len),
        dir[2] * (len + head_len),
    ];
    let base = |ang: f32| -> [f32; 3] {
        [
            dir[0] * len + p1[0] * head_r * ang.cos() + p2[0] * head_r * ang.sin(),
            dir[1] * len + p1[1] * head_r * ang.cos() + p2[1] * head_r * ang.sin(),
            dir[2] * len + p1[2] * head_r * ang.cos() + p2[2] * head_r * ang.sin(),
        ]
    };
    let center = [dir[0] * len, dir[1] * len, dir[2] * len];

    for i in 0..segs {
        let a0 = (i as f32 / segs as f32) * std::f32::consts::TAU;
        let a1 = ((i + 1) as f32 / segs as f32) * std::f32::consts::TAU;
        let b0 = base(a0);
        let b1 = base(a1);
        push_tri(v, apex, b0, b1, color); // side
        push_tri(v, center, b1, b0, color); // base cap
    }
}

/// 推送双面 2D 平面手柄 quad + 边缘 L 型边框。
fn push_plane_handle(v: &mut Vec<GizmoVertex>, axis1: [f32; 3], axis2: [f32; 3], color: [f32; 3]) {
    let min = 0.35f32;
    let max = 0.70f32;
    let p0 = [
        axis1[0] * min + axis2[0] * min,
        axis1[1] * min + axis2[1] * min,
        axis1[2] * min + axis2[2] * min,
    ];
    let p1 = [
        axis1[0] * max + axis2[0] * min,
        axis1[1] * max + axis2[1] * min,
        axis1[2] * max + axis2[2] * min,
    ];
    let p2 = [
        axis1[0] * max + axis2[0] * max,
        axis1[1] * max + axis2[1] * max,
        axis1[2] * max + axis2[2] * max,
    ];
    let p3 = [
        axis1[0] * min + axis2[0] * max,
        axis1[1] * min + axis2[1] * max,
        axis1[2] * min + axis2[2] * max,
    ];

    let face_color = [color[0] * 0.75, color[1] * 0.75, color[2] * 0.75];
    // 正面
    push_tri(v, p0, p1, p2, face_color);
    push_tri(v, p0, p2, p3, face_color);
    // 反面
    push_tri(v, p2, p1, p0, face_color);
    push_tri(v, p3, p2, p0, face_color);

    // 外边缘 L 型边框 (p1 -> p2 -> p3)
    let border_thick = 0.025f32;
    push_line_strip(v, p1, p2, axis1, border_thick, color);
    push_line_strip(v, p3, p2, axis2, border_thick, color);
}

fn push_line_strip(
    v: &mut Vec<GizmoVertex>,
    start: [f32; 3],
    end: [f32; 3],
    perp: [f32; 3],
    thick: f32,
    color: [f32; 3],
) {
    let a = start;
    let b = end;
    let c = [
        b[0] - perp[0] * thick,
        b[1] - perp[1] * thick,
        b[2] - perp[2] * thick,
    ];
    let d = [
        a[0] - perp[0] * thick,
        a[1] - perp[1] * thick,
        a[2] - perp[2] * thick,
    ];

    push_tri(v, a, b, c, color);
    push_tri(v, a, c, d, color);
    push_tri(v, c, b, a, color);
    push_tri(v, d, c, a, color);
}
