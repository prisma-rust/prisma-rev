//! 光栅化阴影映射——方向光的纯深度阴影通道。
//!
//! [`ShadowMapPass`] 从光源视角将场景深度渲染到深度纹理中（混合自适应
//! 阴影系统的纯深度后备方案，`docs/DESIGN.md` §2.3），由
//! [`crate::forward_pass::ForwardPass`] 用比较采样器采样判断遮挡。
//! 纯深度管线（`color_attachment_count = 0`），默认背面剔除 + 坡度/常量
//! 深度偏移以减少阴影痤疮与彼得潘现象；`ShadowCastingMode::TwoSided` 的 caster
//! 走不剔除的第二套管线（统一语义），光源空间矩阵走推送常量。

use anyhow::Context as _;
use anyhow::Result;
use ash::vk;

use crate::context::VulkanContext;
use crate::culling::{Aabb, Frustum};
use crate::mesh::{SkinVertex, Vertex};
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderPassNode,
    RenderSettings, ResourceHandle, ResourceType, ResourceUsage, ShadowMode, SHADOW_MAP_H,
};
use crate::shader;
use crate::shader_bindings::shadow_depth as bindings;

const _: () =
    assert!(std::mem::size_of::<bindings::ShadowPush>() as u32 == bindings::PUSH_CONSTANT_SIZE);

/// 光栅化阴影映射——混合自适应阴影系统的纯深度后备方案（`docs/DESIGN.md` §2.3）。
///
/// 当 `VK_KHR_ray_query` 不可用（或 RT 被禁用）时，渲染器选择此通道
/// 而非 [`RayQueryPass`]。它从光源视角将场景深度渲染到深度纹理中，
/// 光照通道稍后对该纹理进行采样（使用比较采样器来判断被照亮还是被遮挡）。
///
/// 该管线为纯深度管线（`color_attachment_count = 0`），
/// 默认背面剔除（`ShadowCastingMode::TwoSided` 例外，走不剔除的管线），
/// 叠加坡度/常量深度偏移以减少阴影痤疮和彼得潘现象，
/// 并通过推送常量（无 UBO）传递光源空间矩阵。
pub struct ShadowMapPass {
    /// Shadow 映射表 深度 附件 handle (created in `setup`).
    pub shadow_map: ResourceHandle,
    /// Square shadow tile 分辨率 (e.g. 2048).
    shadow_size: u32,
    /// Allocated atlas width: `shadow_size * cascade_count` (2048 or 4096).
    shadow_atlas_width: u32,
    /// Depth-only graphics 管线 (lazy-created on 第一个 执行
    ///
    /// 背面剔除版本，供 `ShadowCastingMode::Off` / `On` / `ShadowsOnly` 使用。
    pipeline: Option<GraphicsPipeline>,
    /// 双面（不剔除）depth-only 管线，供 `ShadowCastingMode::TwoSided` 使用。
    ///
    /// 引擎的 `ShadowCastingMode.TwoSided` 会在阴影渲染时关闭背面剔除，
    /// 即使物体材质本身开启了背面剔除。
    pipeline_two_sided: Option<GraphicsPipeline>,
    /// Shadow 渲染 pass (depth-only).
    render_pass: Option<vk::RenderPass>,
    /// 帧缓冲 wrapping the shadow 映射表 深度 视图
    framebuffer: Option<vk::Framebuffer>,
    /// Cloned 设备 handle for 放置
    device: Option<ash::Device>,
    /// Track whether the shadow map image layout has been transitioned from UNDEFINED.
    layout_initialized: bool,
    /// Skinned depth-only 管线 (#560 P0-3c)：与上面两套同形，但读第二个顶点缓冲
    /// （关节索引/权重）并用 set 0 binding 3 的骨骼调色板混合顶点。
    skinned: Option<GraphicsPipeline>,
    /// 双面版本的蒙皮管线。
    skinned_two_sided: Option<GraphicsPipeline>,
    /// set 0 只含 binding 3 = 骨骼调色板 SSBO；仅蒙皮管线使用。
    bone_layout: Option<vk::DescriptorSetLayout>,
    bone_pool: Option<vk::DescriptorPool>,
    /// 上一帧实际提交的 caster 数：`(静态, 蒙皮)`。只用于诊断与测试 ——
    /// 证明蒙皮分支确实走到了，而不是被静默跳过（#560 P0-3c）。
    last_casters: (u32, u32),
    /// 上一帧被光源视锥剔掉的 caster 数（#491）。只统计第一级联，避免
    /// 多级联时同一实体重复计数；用于诊断与 GPU 回归测试。
    last_culled_casters: u32,
    /// 每帧在飞的调色板缓冲（host-visible）+ 对应描述符集。
    bone_buffers: Vec<vk::Buffer>,
    bone_memories: Vec<vk::DeviceMemory>,
    bone_sets: Vec<vk::DescriptorSet>,
}

/// 阴影贴图边长（texel）。与 [`crate::SHADOW_MAP_SIZE`] 是同一个值，这里 re-export
/// 方便 pass 内部使用；引擎侧请用不带 feature 门限的那个。
pub use crate::SHADOW_MAP_SIZE;
/// CSM 级联数预留（DESIGN 计划）：当前单张正交阴影，下一步扩展扩展为多级联
pub const SHADOW_CASCADE_COUNT: u32 = 1; // CSM: 扩展为 3-4 级联并按相机视锥切片

impl ShadowMapPass {
    pub fn new() -> Self {
        Self {
            shadow_map: SHADOW_MAP_H,
            shadow_size: SHADOW_MAP_SIZE,
            shadow_atlas_width: SHADOW_MAP_SIZE,
            pipeline: None,
            pipeline_two_sided: None,
            render_pass: None,
            framebuffer: None,
            device: None,
            layout_initialized: false,
            skinned: None,
            skinned_two_sided: None,
            bone_layout: None,
            bone_pool: None,
            last_casters: (0, 0),
            last_culled_casters: 0,
            bone_buffers: Vec::new(),
            bone_memories: Vec::new(),
            bone_sets: Vec::new(),
        }
    }

    /// Shadow 映射表 资源 handle (for the lighting pass to 读取
    pub fn shadow_map_handle(&self) -> ResourceHandle {
        self.shadow_map
    }

    /// Shadow-atlas extent (`shadow_size * cascade_count` x `shadow_size`).
    /// Exposed for the render-graph visualizer; each cascade tile is square.
    /// 上一帧提交的 caster 数：`(静态, 蒙皮)`。
    ///
    /// 蒙皮 caster 曾经被整个跳过（没有蒙皮深度管线），这里留着计数是为了让
    /// 回归能被测试抓到，而不是只靠肉眼看阴影。
    pub fn last_casters(&self) -> (u32, u32) {
        self.last_casters
    }

    /// 上一帧被**光源视锥**剔掉的 caster 数（#491）。
    ///
    /// 主相机视锥剔除发生在提取阶段；阴影贴图只覆盖相机附近的一小块区域，
    /// 所以这里的第二道剔除能省掉「主相机看得到、但不在阴影覆盖范围内」的
    /// 投射者。与 [`Self::last_casters`] 一样，只统计第一级联。
    pub fn last_culled_casters(&self) -> u32 {
        self.last_culled_casters
    }

    pub fn shadow_extent(&self) -> vk::Extent2D {
        vk::Extent2D {
            width: self.shadow_atlas_width,
            height: self.shadow_size,
        }
    }

    /// 创建 a depth-only 渲染 pass (single 深度 附件 no 颜色
    ///
    /// Uses `DEPTH_STENCIL_ATTACHMENT_OPTIMAL` / `DEPTH_STENCIL_READ_ONLY_OPTIMAL`
    /// rather than the separate-depth-only layouts: the latter require the
    /// `separateDepthStencilLayouts` Vulkan 1.2 特性 which we don't enable
    /// (it's optional and not uniformly available on mobile). The combined
    /// layouts are 有效 for a `D32_SFLOAT` (depth-only) 图像 with the 深度
    /// 宽高比 masked in the 视图
    fn create_render_pass(
        device: &ash::Device,
        depth_format: vk::Format,
    ) -> anyhow::Result<vk::RenderPass> {
        let depth_attachment = vk::AttachmentDescription::default()
            .format(depth_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            // `UNDEFINED` + LOAD_OP_CLEAR is the Vulkan-idiomatic way to say
            // "discard incoming contents, I'll 清空 the 渲染 pass performs
            // the implicit `any -> DEPTH_STENCIL_ATTACHMENT_OPTIMAL` 过渡
            // This removes the need for the hand-rolled `UNDEFINED -> 附件
            // `cmd_pipeline_barrier` that used to precede `cmd_begin_render_pass`
            // (the 图像 is graph-managed and re-cleared every 帧 so there
            // is nothing to preserve between frames).
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL);

        let depth_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);

        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .depth_stencil_attachment(&depth_ref);

        // Wait for prior shadow-map fragment shader sampling to finish reading
        // before writing depth again.
        let dependency_from_external = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::FRAGMENT_SHADER)
            .dst_stage_mask(vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE);

        // Ensure depth writes in subpass 0 are complete and visible before
        // subsequent fragment shaders sample the shadow map.
        let dependency_to_external = vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::LATE_FRAGMENT_TESTS)
            .dst_stage_mask(vk::PipelineStageFlags::FRAGMENT_SHADER)
            .src_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);

        let dependencies = [dependency_from_external, dependency_to_external];

        let create_info = vk::RenderPassCreateInfo::default()
            .attachments(std::slice::from_ref(&depth_attachment))
            .subpasses(std::slice::from_ref(&subpass))
            .dependencies(&dependencies);

        let handle = unsafe { device.create_render_pass(&create_info, None) }
            .context("create shadow render pass")?;
        Ok(handle)
    }

    /// 阴影通道使用的背面剔除模式（`ShadowCastingMode.TwoSided` 关闭剔除）。
    ///
    /// 独立成函数便于单测：GPU 侧的管线选择只有这一个判定点。
    fn shadow_cull_mode(two_sided: bool) -> vk::CullModeFlags {
        if two_sided {
            vk::CullModeFlags::NONE
        } else {
            vk::CullModeFlags::BACK
        }
    }

    /// 阴影通道正面三角形绕序。
    ///
    /// 正交光照投影矩阵（`light_view_proj`）没有相机透视视锥的 Vulkan Y 翻转，
    /// 其行列式为正；而透视投影包含负 Y 缩放。因此在光照裁剪空间下，面向光源
    /// 的正面三角形绕序为 [`vk::FrontFace::CLOCKWISE`]。若使用默认的
    /// `COUNTER_CLOCKWISE`，背向剔除会将面向光源的正面剔除。
    pub(crate) const SHADOW_FRONT_FACE: vk::FrontFace = vk::FrontFace::CLOCKWISE;

    /// 创建一套 depth-only 阴影管线；两套管线只差剔除模式。
    ///
    /// `warmup` 与 `execute` 各调用两次（背面剔除 / 双面），避免把同样的加载
    /// 逻辑复制两遍。
    fn create_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        cull_mode: vk::CullModeFlags,
    ) -> anyhow::Result<GraphicsPipeline> {
        const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/shadow_depth.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/shadow_depth.frag.spv");
        let vert_module =
            shader::load_shader_module(device, VERT_SPV).context("load shadow vert module")?;
        let frag_module =
            shader::load_shader_module(device, FRAG_SPV).context("load shadow frag module")?;

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

        let binding_desc = Vertex::binding_description();
        let position_attr = vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(0);

        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(bindings::PUSH_CONSTANT_SIZE)];

        // Depth-only 管线 + 深度 bias；剔除模式由调用方决定。
        let pipeline = GraphicsPipeline::new(&PipelineDesc {
            device,
            shader_stages: &shader_stages,
            vertex_binding_desc: std::slice::from_ref(&binding_desc),
            vertex_attr_descs: std::slice::from_ref(&position_attr),
            descriptor_set_layouts: &[],
            push_constant_ranges: &push,
            render_pass,
            subpass: 0,
            cull_mode: Some(cull_mode),
            depth_bias_enable: Some(true),
            depth_bias_constant_factor: Some(32.0),
            depth_bias_slope_factor: Some(4.0),
            depth_write_enable: Some(true),
            depth_test_enable: None,
            color_attachment_count: Some(0),
            color_blend_attachments: None,
            front_face: Some(Self::SHADOW_FRONT_FACE),
            polygon_mode: None,
        })
        .context("create shadow depth-only pipeline")?;

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        Ok(pipeline)
    }
}

/// 蒙皮 casters 的调色板预算 = 主通道的上限（两份缓冲各自 256 KiB）。
pub const SHADOW_BONE_CAPACITY: usize = crate::forward_pass::ForwardPass::BONE_CAPACITY;

impl ShadowMapPass {
    /// 蒙皮版本的纯深度管线：读第二个顶点缓冲（binding 1 = 关节/权重）并用
    /// set 0 binding 3 的骨骼调色板混合顶点（#560 P0-3c）。
    fn create_skinned_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        cull_mode: vk::CullModeFlags,
        bone_layout: vk::DescriptorSetLayout,
    ) -> anyhow::Result<GraphicsPipeline> {
        const VERT_SPV: &[u8] =
            include_bytes!("../../../assets/shaders/shadow_depth_skinned.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/shadow_depth.frag.spv");
        let vert_module =
            shader::load_shader_module(device, VERT_SPV).context("load skinned shadow vert")?;
        let frag_module =
            shader::load_shader_module(device, FRAG_SPV).context("load skinned shadow frag")?;

        // 反射 JSON 只带第一个入口点，bindgen 因此没有 skinned 常量；与
        // `ForwardPass` 的写法保持一致（同样的字面量）。
        let vert_entry = std::ffi::CString::new("vertexSkinnedMain").unwrap();
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

        let binding_descs = [
            Vertex::binding_description(),
            SkinVertex::binding_description(),
        ];
        let mut attributes: Vec<vk::VertexInputAttributeDescription> =
            Vertex::attribute_descriptions().to_vec();
        attributes.extend_from_slice(&SkinVertex::attribute_descriptions());

        let set_layouts = [bone_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(bindings::PUSH_CONSTANT_SIZE)];

        let pipeline = GraphicsPipeline::new(&PipelineDesc {
            device,
            shader_stages: &shader_stages,
            vertex_binding_desc: &binding_descs,
            vertex_attr_descs: &attributes,
            descriptor_set_layouts: &set_layouts,
            push_constant_ranges: &push,
            render_pass,
            subpass: 0,
            cull_mode: Some(cull_mode),
            depth_bias_enable: Some(true),
            depth_bias_constant_factor: Some(32.0),
            depth_bias_slope_factor: Some(4.0),
            depth_write_enable: Some(true),
            depth_test_enable: None,
            color_attachment_count: Some(0),
            color_blend_attachments: None,
            front_face: Some(Self::SHADOW_FRONT_FACE),
            polygon_mode: None,
        })
        .context("create skinned shadow depth-only pipeline")?;

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }
        Ok(pipeline)
    }

    /// 懒创建蒙皮管线 + 每帧在飞的骨骼调色板缓冲/描述符集。幂等。
    fn ensure_skinned(
        &mut self,
        context: &VulkanContext,
        render_pass: vk::RenderPass,
    ) -> Result<()> {
        let device = &context.device;

        if self.bone_layout.is_none() {
            let set_bindings = [vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::BONES_BINDING)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX)];
            let layout = unsafe {
                device.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&set_bindings),
                    None,
                )
            }
            .context("shadow: create bone descriptor set layout")?;
            self.bone_layout = Some(layout);
        }

        let layout = self.bone_layout.unwrap();
        if self.skinned.is_none() {
            self.skinned = Some(
                Self::create_skinned_pipeline(
                    device,
                    render_pass,
                    Self::shadow_cull_mode(false),
                    layout,
                )
                .context("create skinned shadow pipeline")?,
            );
        }
        if self.skinned_two_sided.is_none() {
            self.skinned_two_sided = Some(
                Self::create_skinned_pipeline(
                    device,
                    render_pass,
                    Self::shadow_cull_mode(true),
                    layout,
                )
                .context("create two-sided skinned shadow pipeline")?,
            );
        }

        if self.bone_buffers.is_empty() {
            let frames = crate::swapchain::FRAMES_IN_FLIGHT;
            let size =
                (SHADOW_BONE_CAPACITY * std::mem::size_of::<[[f32; 4]; 4]>()) as vk::DeviceSize;
            let mut buffers = Vec::with_capacity(frames);
            let mut memories = Vec::with_capacity(frames);
            for _ in 0..frames {
                let (buffer, memory) = crate::buffer::create_buffer(
                    context,
                    size,
                    crate::buffer::BufferUsage::STORAGE_BUFFER,
                    crate::buffer::MemoryProperties::HOST_VISIBLE
                        | crate::buffer::MemoryProperties::HOST_COHERENT,
                )
                .context("shadow: create bone palette buffer")?;
                let ptr =
                    unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) }
                        .context("shadow: map bone palette memory")?;
                unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, size as usize) };
                unsafe { device.unmap_memory(memory) };
                buffers.push(buffer);
                memories.push(memory);
            }

            let pool_sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(frames as u32)];
            let pool = unsafe {
                device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(frames as u32)
                        .pool_sizes(&pool_sizes),
                    None,
                )
            }
            .context("shadow: create bone descriptor pool")?;

            let layouts = vec![layout; frames];
            let sets = unsafe {
                device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool)
                        .set_layouts(&layouts),
                )
            }
            .context("shadow: allocate bone descriptor sets")?;

            for (set, buffer) in sets.iter().zip(&buffers) {
                let info = [vk::DescriptorBufferInfo::default()
                    .buffer(*buffer)
                    .offset(0)
                    .range(vk::WHOLE_SIZE)];
                let writes = [vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(bindings::BONES_BINDING)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&info)];
                unsafe { device.update_descriptor_sets(&writes, &[]) };
            }

            self.bone_pool = Some(pool);
            self.bone_buffers = buffers;
            self.bone_memories = memories;
            self.bone_sets = sets;
        }

        Ok(())
    }

    /// 上传本帧的骨骼调色板（列主序 mat4）。超出 [`SHADOW_BONE_CAPACITY`] 的部分截断，
    /// 与主通道的 `bone_offset` 分配保持一致。
    pub fn update_bones(
        &mut self,
        device: &ash::Device,
        frame_index: u32,
        matrices: &[[[f32; 4]; 4]],
    ) -> Result<()> {
        if self.bone_memories.is_empty() {
            return Ok(());
        }
        let index = (frame_index as usize) % self.bone_memories.len();
        let memory = self.bone_memories[index];
        if memory == vk::DeviceMemory::null() {
            return Ok(());
        }
        let count = matrices.len().min(SHADOW_BONE_CAPACITY);
        let matrix_size = std::mem::size_of::<[[f32; 4]; 4]>();
        let total_bytes = (SHADOW_BONE_CAPACITY * matrix_size) as vk::DeviceSize;
        let ptr = unsafe { device.map_memory(memory, 0, total_bytes, vk::MemoryMapFlags::empty()) }
            .context("ShadowMapPass::update_bones: map")?;
        unsafe {
            let dst = ptr as *mut u8;
            // 清掉尾部：越界读取时拿到零矩阵，而不是上一帧的骨骼。
            std::ptr::write_bytes(
                dst.add(count * matrix_size),
                0,
                (SHADOW_BONE_CAPACITY - count) * matrix_size,
            );
            if count > 0 {
                std::ptr::copy_nonoverlapping(
                    matrices.as_ptr() as *const u8,
                    dst,
                    count * matrix_size,
                );
            }
            device.unmap_memory(memory);
        }
        Ok(())
    }
}

impl Default for ShadowMapPass {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderPassNode for ShadowMapPass {
    fn name(&self) -> &str {
        "ShadowMapPass"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, settings: &RenderSettings) {
        // #526：大小来自 RenderSettings（构造 GraphRenderer 时写入），并在安全
        // 范围内钳制；运行中改字段不会重建已经分配好的图像。
        self.shadow_size = settings
            .shadow_map_size
            .clamp(crate::MIN_SHADOW_MAP_SIZE, crate::MAX_SHADOW_MAP_SIZE);
        let size = self.shadow_size;
        let cascades = settings.shadow_cascade_count.clamp(1, 2);
        self.shadow_atlas_width = size * cascades;
        self.shadow_map = SHADOW_MAP_H;
        graph.create_resource_at(
            SHADOW_MAP_H,
            ResourceType::DepthAttachment {
                // #526 CSM: one square tile per cascade. With one cascade the
                // atlas is just the legacy square map; with two it is 2×1.
                extent: vk::Extent2D {
                    width: self.shadow_atlas_width,
                    height: size,
                },
                sample_count: vk::SampleCountFlags::TYPE_1,
            },
        );
        graph.write_usage(ResourceUsage {
            handle: SHADOW_MAP_H,
            access: vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            stage: vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
            layout: vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        });
    }

    fn execute(&mut self, ctx: &RenderContext, resources: &mut GraphResources) -> Result<()> {
        // Only 渲染 when the rasterized shadow path is 激活 The 图
        // 构建器 adds this pass only for `ShadowMode::Raster`, but guard
        // anyway so a misconfigured 图 can't waste a 深度 pass
        if ctx.frame.shadow_mode != ShadowMode::Raster {
            if !self.layout_initialized {
                if let Some(shadow_img) = resources.image(self.shadow_map) {
                    let barrier = vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(shadow_img)
                        .src_access_mask(vk::AccessFlags::empty())
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::DEPTH,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        });
                    unsafe {
                        ctx.device.cmd_pipeline_barrier(
                            ctx.cmd,
                            vk::PipelineStageFlags::TOP_OF_PIPE,
                            vk::PipelineStageFlags::FRAGMENT_SHADER,
                            vk::DependencyFlags::empty(),
                            &[],
                            &[],
                            std::slice::from_ref(&barrier),
                        );
                    }
                    self.layout_initialized = true;
                }
            }
            return Ok(());
        }

        let size = self.shadow_size;
        let shadow_view = match resources.image_view(self.shadow_map) {
            Some(v) => v,
            None => {
                log::warn!("ShadowMapPass: shadow map view not allocated; skipping");
                return Ok(());
            }
        };

        // Lazy-init 管线 + 渲染 pass + 帧缓冲 If warmup already
        // created 管线 + render_pass, only the 帧缓冲 still needs to
        // be created (it depends on `shadow_view` from the 图 resources,
        // which are only available during 执行
        if self.framebuffer.is_none() {
            let device = ctx.device;
            self.device = Some(device.clone());

            // 渲染 pass — shared between warmup and 执行
            if self.render_pass.is_none() {
                let render_pass = Self::create_render_pass(device, vk::Format::D32_SFLOAT)?;
                self.render_pass = Some(render_pass);
            }
            let rp = self.render_pass.unwrap();

            // 帧缓冲 — always needs the per-execute shadow_view.
            let framebuffer = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(rp)
                        .attachments(std::slice::from_ref(&shadow_view))
                        .width(self.shadow_atlas_width)
                        .height(size)
                        .layers(1),
                    None,
                )
            }
            .context("create shadow framebuffer")?;
            self.framebuffer = Some(framebuffer);

            // 管线 — may already exist when warmup ran ahead of 时间
            // 两套管线：背面剔除（默认）与双面/不剔除（`ShadowCastingMode::TwoSided`）。
            if self.pipeline.is_none() {
                self.pipeline = Some(
                    Self::create_pipeline(device, rp, Self::shadow_cull_mode(false))
                        .context("create shadow depth-only pipeline")?,
                );
            }
            if self.pipeline_two_sided.is_none() {
                self.pipeline_two_sided = Some(
                    Self::create_pipeline(device, rp, Self::shadow_cull_mode(true))
                        .context("create two-sided shadow depth-only pipeline")?,
                );
            }
        } // if self.framebuffer.is_none()

        // 蒙皮 casters 的管线 + 用户缓冲（#560 P0-3c）。
        {
            let render_pass = self.render_pass.unwrap_or_default();
            self.ensure_skinned(ctx.context, render_pass)?;
            self.update_bones(ctx.device, ctx.frame_index, ctx.frame.bone_matrices)?;
        }

        let pipeline_culled = self.pipeline.as_ref().unwrap();
        let pipeline_two_sided = self.pipeline_two_sided.as_ref().unwrap();
        let skinned_culled = self
            .skinned
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("skinned shadow pipeline missing"))?;
        let skinned_two_sided = self
            .skinned_two_sided
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("two-sided skinned shadow pipeline missing"))?;
        let render_pass = self.render_pass.unwrap();
        let framebuffer = self.framebuffer.unwrap();

        // The shadow map's `UNDEFINED -> DEPTH_STENCIL_ATTACHMENT_OPTIMAL`
        // 过渡 used to live here as a hand-rolled `cmd_pipeline_barrier`.
        // It is now handled implicitly by the 渲染 pass the 附件
        // `initial_layout = UNDEFINED` + `LOAD_OP_CLEAR` lets Vulkan 执行
        // the 过渡 inside `cmd_begin_render_pass` (see `create_render_pass`).
        // `shadow_img` is therefore no longer needed in this 函数 body.
        let _ = resources.image(self.shadow_map).unwrap_or_default();

        let clear = vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: 1.0,
                stencil: 0,
            },
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(render_pass)
            .framebuffer(framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: self.shadow_atlas_width,
                    height: size,
                },
            })
            .clear_values(std::slice::from_ref(&clear));
        unsafe {
            ctx.device
                .cmd_begin_render_pass(ctx.cmd, &begin, vk::SubpassContents::INLINE)
        };

        // 统计真正写进阴影贴图的 caster（`ShadowCastingMode::Off` 会被跳过）。
        // 双级联时同一批 caster 会画两次，这里只按第一遍计数。
        let allocated_cascades = (self.shadow_atlas_width / size).clamp(1, 2);
        let cascade_count = ctx.frame.shadow_cascade_count.clamp(1, allocated_cascades);
        let mut casters = 0usize;
        // 其中有多少走的是蒙皮管线（#560 P0-3c）。
        let mut skinned_casters = 0u32;
        // 被光源视锥剔除的 caster 只统计第一级联（#491）。
        self.last_culled_casters = 0;
        unsafe {
            for cascade in 0..cascade_count {
                // 每个级联固定占据 atlas 的一个正方形 tile；单级联只用左半边，
                // 这样 PCF 的 texel 尺寸和旧版 2048² 阴影图完全一致。
                let viewport_x = (cascade * size) as f32;
                let viewport_width = size as f32;
                let light_view_proj = if cascade == 0 {
                    ctx.frame.light_view_proj
                } else {
                    ctx.frame.light_view_proj1
                };
                // 光源视锥（#491）：阴影图只覆盖 main camera 附近的一小块，
                // 提取阶段按主相机保留的 caster 不一定落在这里。矩阵退化时
                // `from_view_proj` 返回 `None`，下面保守地全部提交。
                let light_frustum = Frustum::from_view_proj(&light_view_proj);
                ctx.device.cmd_set_viewport(
                    ctx.cmd,
                    0,
                    &[vk::Viewport {
                        x: viewport_x,
                        y: 0.0,
                        width: viewport_width,
                        height: size as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                ctx.device.cmd_set_scissor(
                    ctx.cmd,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D {
                            x: viewport_x as i32,
                            y: 0,
                        },
                        extent: vk::Extent2D {
                            width: viewport_width as u32,
                            height: size,
                        },
                    }],
                );

                // 按 (是否蒙皮, 是否双面) 选管线，只在切换时重绑。
                let mut bound_pipeline: Option<(bool, bool)> = None;
                for item in ctx.frame.draw_list {
                    // `ShadowCastingMode::Off` -> 不写阴影贴图。
                    if !item.cast_shadow {
                        continue;
                    }
                    // 不在这一级联阴影覆盖范围内的 caster 不提交（#491）。
                    // `world_bounds` 缺失（程序化网格 / 句柄未解析）时保持旧行为。
                    if !caster_survives_light_frustum(
                        item.world_bounds.as_ref(),
                        light_frustum.as_ref(),
                    ) {
                        if cascade == 0 {
                            self.last_culled_casters += 1;
                        }
                        continue;
                    }
                    let uploaded = match ctx.frame.mesh_manager.get(item.mesh) {
                        Some(m) => &m.mesh,
                        None => continue,
                    };

                    // 蒙皮 caster 走蒙皮管线（读第二个顶点缓冲 + 骨骼调色板）；
                    // 静态 caster 走原来的纯位置管线（#560 P0-3c）。
                    let skinned = item.bone_offset.is_some();
                    let skin_buffer = if skinned {
                        match ctx.frame.mesh_manager.get(item.mesh) {
                            Some(m) => match m.skin.as_ref() {
                                // 声明了蒙皮但缓冲没上传：画出来会是 bind pose 以外的垃圾，
                                // 宁可这个 caster 不投影。
                                Some(skin) => Some(skin.buffer),
                                None => continue,
                            },
                            None => continue,
                        }
                    } else {
                        None
                    };

                    let two_sided = item.cast_shadow_two_sided;
                    let pipeline = match (skinned, two_sided) {
                        (true, true) => skinned_two_sided,
                        (true, false) => skinned_culled,
                        (false, true) => pipeline_two_sided,
                        (false, false) => pipeline_culled,
                    };
                    if bound_pipeline != Some((skinned, two_sided)) {
                        ctx.device.cmd_bind_pipeline(
                            ctx.cmd,
                            vk::PipelineBindPoint::GRAPHICS,
                            pipeline.pipeline,
                        );
                        bound_pipeline = Some((skinned, two_sided));
                    }

                    if cascade == 0 {
                        casters += 1;
                        if skinned {
                            skinned_casters += 1;
                        }
                    }
                    let vertex_buffers = match skin_buffer {
                        Some(skin_buffer) => [uploaded.vertex_buffer, skin_buffer],
                        None => [uploaded.vertex_buffer, vk::Buffer::null()],
                    };
                    let buffer_count = if skin_buffer.is_some() { 2 } else { 1 };
                    let offsets = [0u64, 0u64];
                    ctx.device.cmd_bind_vertex_buffers(
                        ctx.cmd,
                        0,
                        &vertex_buffers[..buffer_count],
                        &offsets[..buffer_count],
                    );

                    if skin_buffer.is_some() {
                        let bone_set = match self
                            .bone_sets
                            .get(ctx.frame_index as usize % self.bone_sets.len().max(1))
                        {
                            Some(set) => *set,
                            None => continue,
                        };
                        let sets = [bone_set];
                        ctx.device.cmd_bind_descriptor_sets(
                            ctx.cmd,
                            vk::PipelineBindPoint::GRAPHICS,
                            pipeline.layout,
                            0,
                            &sets,
                            &[],
                        );
                    }

                    let pc = bindings::ShadowPush {
                        model: item.model,
                        lightViewProj: light_view_proj,
                        bone_offset: item.bone_offset.unwrap_or(0),
                        ..Default::default()
                    };
                    // 推送常量必须使用当前绑定管线的布局（两套管线各有独立 layout）。
                    ctx.device.cmd_push_constants(
                        ctx.cmd,
                        pipeline.layout,
                        vk::ShaderStageFlags::VERTEX,
                        0,
                        std::slice::from_raw_parts(
                            &pc as *const _ as *const u8,
                            bindings::PUSH_CONSTANT_SIZE as usize,
                        ),
                    );

                    if let Some(ib) = uploaded.index_buffer {
                        let index_count = item.index_count.unwrap_or(uploaded.index_count);
                        if index_count == 0 {
                            continue;
                        }
                        ctx.device
                            .cmd_bind_index_buffer(ctx.cmd, ib, 0, vk::IndexType::UINT32);
                        ctx.device.cmd_draw_indexed(
                            ctx.cmd,
                            index_count,
                            1,
                            item.index_start,
                            0,
                            0,
                        );
                    } else {
                        ctx.device.cmd_draw(ctx.cmd, uploaded.vertex_count, 1, 0, 0);
                    }
                }
            }
        }

        unsafe { ctx.device.cmd_end_render_pass(ctx.cmd) };
        self.layout_initialized = true;
        self.last_casters = (
            (casters as u32).saturating_sub(skinned_casters),
            skinned_casters,
        );

        log::trace!(
            "ShadowMapPass: rendered {} casters into {}x{} shadow atlas ({} in list)",
            casters,
            size * 2,
            size,
            ctx.frame.draw_list.len()
        );
        Ok(())
    }

    fn warmup(&mut self, device: &ash::Device, _context: &VulkanContext) -> Result<()> {
        if self.pipeline.is_some() && self.pipeline_two_sided.is_some() {
            return Ok(());
        }
        self.device = Some(device.clone());

        // 渲染 pass — same 布局 as 执行 uses.
        let render_pass = Self::create_render_pass(device, vk::Format::D32_SFLOAT)?;
        self.render_pass = Some(render_pass);

        // 管线 — 加载 着色器 modules, 创建两套 depth-only 管线
        // （背面剔除 / 双面），使渲染线程在首帧前就编译好两条几乎相同的管线。
        self.pipeline = Some(
            Self::create_pipeline(device, render_pass, Self::shadow_cull_mode(false))
                .context("warmup: shadow depth-only pipeline")?,
        );
        self.pipeline_two_sided = Some(
            Self::create_pipeline(device, render_pass, Self::shadow_cull_mode(true))
                .context("warmup: two-sided shadow depth-only pipeline")?,
        );
        Ok(())
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Shadow,
            inputs: Vec::new(),
            outputs: vec![self.shadow_map],
        }
    }
}

impl ShadowMapPass {
    /// Tear 下 all GPU resources 帧缓冲 渲染 pass pipeline/layout).
    ///
    /// Called from [`GraphRenderer::destroy`] on shutdown **before** the
    /// `Arc<VulkanContext>` 引用 count drops to 零 Without this explicit
    /// 调用 `ShadowMapPass` relies on its 放置 impl — but Rust's 结构体 field
    /// 放置 order means the 图 (and thus this pass is dropped *after* the
    /// `Arc<VulkanContext>` holders (`runtime`/`ibl`/`scene_scope`), at which
    /// point the 设备 handle is already stale and calling
    /// `destroy_framebuffer` / `destroy_render_pass` on it causes an 访问
    /// violation.
    ///
    /// After this 调用 `self.device` is `None`, so the subsequent 放置 becomes
    /// a no-op.
    pub fn destroy(&mut self, device: &ash::Device) {
        if let Some(fb) = self.framebuffer.take() {
            unsafe { device.destroy_framebuffer(fb, None) };
        }
        if let Some(rp) = self.render_pass.take() {
            unsafe { device.destroy_render_pass(rp, None) };
        }
        // GraphicsPipeline::Drop frees the 管线 + 布局
        self.pipeline = None;
        self.pipeline_two_sided = None;
        self.skinned = None;
        self.skinned_two_sided = None;
        // 描述符集随 pool 一起释放。
        self.bone_sets.clear();
        if let Some(pool) = self.bone_pool.take() {
            unsafe { device.destroy_descriptor_pool(pool, None) };
        }
        if let Some(layout) = self.bone_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(layout, None) };
        }
        for buffer in self.bone_buffers.drain(..) {
            if buffer != vk::Buffer::null() {
                unsafe { device.destroy_buffer(buffer, None) };
            }
        }
        for memory in self.bone_memories.drain(..) {
            if memory != vk::DeviceMemory::null() {
                unsafe { device.free_memory(memory, None) };
            }
        }
        self.device = None;
        self.layout_initialized = false;
    }
}

impl Drop for ShadowMapPass {
    fn drop(&mut self) {
        if let Some(device) = &self.device {
            if let Some(fb) = self.framebuffer.take() {
                unsafe { device.destroy_framebuffer(fb, None) };
            }
            if let Some(rp) = self.render_pass.take() {
                unsafe { device.destroy_render_pass(rp, None) };
            }
            // GraphicsPipeline's own 放置 frees the 管线 + 布局
        }
    }
}

/// 一个 caster 是否还要写进这一级联的阴影贴图（#491）。
///
/// 保守优先：没有世界包围盒、或光源矩阵退化（`Frustum::from_view_proj`
/// 返回 `None`）时都保留。只有「有包围盒 + 有有效视锥 + 完全在外」才剔除。
fn caster_survives_light_frustum(bounds: Option<&Aabb>, frustum: Option<&Frustum>) -> bool {
    match (bounds, frustum) {
        (Some(bounds), Some(frustum)) => frustum.intersects_aabb(bounds),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_sided_shadow_casting_disables_backface_culling() {
        // `ShadowCastingMode::TwoSided`：阴影渲染关闭背面剔除。
        assert_eq!(
            ShadowMapPass::shadow_cull_mode(false),
            vk::CullModeFlags::BACK
        );
        assert_eq!(
            ShadowMapPass::shadow_cull_mode(true),
            vk::CullModeFlags::NONE
        );
    }

    #[test]
    fn shadow_pass_uses_clockwise_front_face() {
        // 无 Y-flip 的光照正交投影下，正面绕序为 CLOCKWISE
        assert_eq!(ShadowMapPass::SHADOW_FRONT_FACE, vk::FrontFace::CLOCKWISE);
    }

    #[test]
    fn shadow_pass_registers_resource_and_outputs() {
        let mut shadow_pass = ShadowMapPass::new();
        assert_eq!(shadow_pass.shadow_map_handle(), SHADOW_MAP_H);

        let settings = RenderSettings::default();
        let mut builder = RenderGraphBuilder::new().settings(&settings);
        shadow_pass.setup(&mut builder, &settings);
        builder.add_pass(Box::new(shadow_pass));
        let graph = builder.build();

        assert!(graph.resource(SHADOW_MAP_H).is_some());
        let snapshot = graph.snapshot();
        assert_eq!(snapshot.passes.len(), 1);
        assert_eq!(snapshot.passes[0].outputs, vec![SHADOW_MAP_H]);
        assert!(snapshot.passes[0].inputs.is_empty());
    }

    /// #526：贴图边长由 `RenderSettings::shadow_map_size` 决定，并在安全范围内钳制。
    #[test]
    fn shadow_pass_honours_the_configured_map_size() {
        for (requested, expected) in [
            (1024u32, 1024u32),
            (crate::MIN_SHADOW_MAP_SIZE - 1, crate::MIN_SHADOW_MAP_SIZE),
            (crate::MAX_SHADOW_MAP_SIZE + 1, crate::MAX_SHADOW_MAP_SIZE),
        ] {
            let settings = RenderSettings {
                shadow_map_size: requested,
                ..Default::default()
            };
            let mut shadow_pass = ShadowMapPass::new();
            let mut builder = RenderGraphBuilder::new().settings(&settings);
            shadow_pass.setup(&mut builder, &settings);
            builder.add_pass(Box::new(shadow_pass));
            let graph = builder.build();

            let resource = graph.resource(SHADOW_MAP_H).expect("shadow resource");
            match resource.res_type {
                ResourceType::DepthAttachment { extent, .. } => {
                    assert_eq!(extent.width, expected, "requested {requested}");
                    assert_eq!(extent.height, expected, "requested {requested}");
                }
                ref other => panic!("expected a depth attachment, got {other:?}"),
            }
        }
    }

    /// #526 CSM: two cascades allocate a 2×1 atlas, one square tile each.
    #[test]
    fn two_cascades_allocate_a_side_by_side_atlas() {
        let settings = RenderSettings {
            shadow_map_size: 1024,
            shadow_cascade_count: 2,
            ..Default::default()
        };
        let mut shadow_pass = ShadowMapPass::new();
        let mut builder = RenderGraphBuilder::new().settings(&settings);
        shadow_pass.setup(&mut builder, &settings);
        builder.add_pass(Box::new(shadow_pass));
        let graph = builder.build();

        let resource = graph.resource(SHADOW_MAP_H).expect("shadow resource");
        match resource.res_type {
            ResourceType::DepthAttachment { extent, .. } => {
                assert_eq!(extent.width, 2048);
                assert_eq!(extent.height, 1024);
            }
            ref other => panic!("expected a depth attachment, got {other:?}"),
        }
    }

    /// 列主序矩阵，把世界 `[-1, 1]^3` 映射到 Vulkan 裁剪空间
    /// `x/y ∈ [-1, 1]`、`z ∈ [0, 1]`（`w = 1`）。
    ///
    /// 只用它测试光源视锥，不参与渲染。
    fn unit_box_ortho() -> [[f32; 4]; 4] {
        [
            [0.5, 0.0, 0.0, 0.0],
            [0.0, 0.5, 0.0, 0.0],
            [0.0, 0.0, 0.5, 0.0],
            [0.0, 0.0, 0.5, 1.0],
        ]
    }

    /// #491：光源视锥之外的 caster 被剔除，覆盖范围内 / 数据缺失时保留。
    #[test]
    fn casters_outside_the_light_frustum_are_skipped() {
        let matrix = unit_box_ortho();
        let frustum = Frustum::from_view_proj(&matrix).expect("unit-box ortho is a valid frustum");

        let inside = Aabb::new([-0.5, -0.5, 0.1], [0.5, 0.5, 0.9]);
        let outside = Aabb::new([10.0, -0.5, 0.1], [11.0, 0.5, 0.9]);
        let straddling = Aabb::new([0.9, -0.5, 0.1], [1.1, 0.5, 0.9]);

        assert!(caster_survives_light_frustum(Some(&inside), Some(&frustum)));
        assert!(!caster_survives_light_frustum(
            Some(&outside),
            Some(&frustum)
        ));
        // 只交叠一部分也要保留（保守剔除，不切物体）。
        assert!(caster_survives_light_frustum(
            Some(&straddling),
            Some(&frustum)
        ));
        // 没有包围盒 / 没有可用视锥：保持旧行为，不剔除。
        assert!(caster_survives_light_frustum(None, Some(&frustum)));
        assert!(caster_survives_light_frustum(Some(&outside), None));
    }

    #[test]
    fn degenerate_light_matrix_keeps_every_caster() {
        // 全零矩阵没有可提取的平面；此时任何剔除都只是瞎裁。
        let degenerate = [[0.0f32; 4]; 4];
        assert!(Frustum::from_view_proj(&degenerate).is_none());
        let bounds = Aabb::new([10.0, 10.0, 10.0], [11.0, 11.0, 11.0]);
        assert!(caster_survives_light_frustum(
            Some(&bounds),
            Frustum::from_view_proj(&degenerate).as_ref()
        ));
    }

    #[test]
    fn forward_pass_reads_shadow_map_from_shadow_pass() {
        let mut shadow_pass = ShadowMapPass::new();
        let settings = RenderSettings::default();
        let mut builder = RenderGraphBuilder::new().settings(&settings);
        shadow_pass.setup(&mut builder, &settings);
        builder.add_pass(Box::new(shadow_pass));
        let mut graph = builder.build();

        let forward_pass = crate::forward_pass::ForwardPass::new(vk::Format::B8G8R8A8_SRGB);
        graph.add_pass(Box::new(forward_pass));

        let snapshot = graph.snapshot();
        assert_eq!(snapshot.passes.len(), 2);
        assert_eq!(snapshot.passes[0].outputs, vec![SHADOW_MAP_H]);
        assert!(snapshot.passes[1].inputs.contains(&SHADOW_MAP_H));
    }
}
