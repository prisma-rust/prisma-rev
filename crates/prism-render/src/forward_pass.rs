//! 前向 PBR 主渲染通道。
//!
//! [`ForwardPass`] 渲染整个游戏场景：环境天空盒 + 所有网格，片段着色
//! 阶段一次性完成 PBR 光照（IBL + 方向光阴影 + GTAO 环境光遮蔽 +
//! 全局光照 probe 采样），输出到每交换链图像的 HDR 中间目标
//! （颜色/深度/视图空间法线 MRT），由 [`crate::post::PostPass`] 采样
//! 并色调映射到交换链。
//!
//! 拥有每交换链图像一组 framebuffer（重建仅当交换链视图变化时），
//! 以及内嵌的 [`SkyboxPass`] 与 [`crate::gizmo::Gizmo`]。

use anyhow::Context as _;
use anyhow::Result;
use ash::vk;
use std::collections::HashMap;
use std::time::Instant;

use crate::context::VulkanContext;
use crate::gizmo::Gizmo;
use crate::ground_grid::GroundGrid;
use crate::mesh::Vertex;
use crate::pipeline::{blend_factor, blend_operation, cull_flags, GraphicsPipeline, PipelineDesc};
#[cfg(feature = "shadows")]
use crate::render_graph::SHADOW_MAP_H;
use crate::render_graph::{
    DrawState, GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder,
    RenderPassNode, RenderSettings, ResourceHandle, ResourceType, ResourceUsage, FORWARD_COLOR_H,
    FORWARD_DEPTH_H, FORWARD_NORMAL_H, FORWARD_VELOCITY_H,
};
use crate::shader;
use crate::shader_bindings::{mesh_vert, scene_frag};
use crate::skybox_pass::SkyboxPass;

// The reflected push-constant block and the Rust mirror must stay in lockstep;
// a mismatch would make `cmd_push_constants` write past the pipeline range.
const _: () = assert!(
    std::mem::size_of::<scene_frag::PbrBindlessPush>() as u32 == scene_frag::PUSH_CONSTANT_SIZE
);

/// Grouping key for static draws that can share one instanced draw call (#491).
///
/// The model matrix is deliberately absent: it becomes one entry in the bone
/// SSBO. Skinned, transparent and missing-mesh draws never enter this path.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct StaticBatchKey {
    mesh: crate::managers::MeshHandle,
    material: Option<u32>,
    state: DrawState,
    index_start: u32,
    index_count: Option<u32>,
}

struct StaticBatch {
    items: Vec<usize>,
    /// First entry in the packed bone/instance SSBO; `None` = single draw.
    instance_offset: Option<u32>,
}

/// Group visible opaque static draws by mesh / material / pipeline state.
fn build_static_batches_with(
    draw_list: &[crate::render_graph::DrawItem],
    mesh_exists: impl Fn(crate::managers::MeshHandle) -> bool,
) -> Vec<StaticBatch> {
    let mut batches: Vec<StaticBatch> = Vec::new();
    let mut slots: HashMap<StaticBatchKey, usize> = HashMap::new();
    for (index, item) in draw_list.iter().enumerate() {
        if !item.visible_in_main_pass || item.index_count == Some(0) || !mesh_exists(item.mesh) {
            continue;
        }
        // Skinned draws need their own bone offset and transparent draws need
        // their back-to-front order; both stay single-draw batches.
        let groupable = item.bone_offset.is_none()
            && item.splat_tiling == 0.0
            && item.state.render_queue <= prism_gfx::RenderQueue::GEOMETRY_LAST;
        if !groupable {
            batches.push(StaticBatch {
                items: vec![index],
                instance_offset: None,
            });
            continue;
        }
        let key = StaticBatchKey {
            mesh: item.mesh,
            material: item.material,
            state: item.state,
            index_start: item.index_start,
            index_count: item.index_count,
        };
        let slot = *slots.entry(key).or_insert_with(|| {
            batches.push(StaticBatch {
                items: Vec::new(),
                instance_offset: None,
            });
            batches.len() - 1
        });
        batches[slot].items.push(index);
    }
    batches
}

/// Assign instance offsets and split groups that do not fit `capacity`.
fn prepare_static_batches(
    draw_list: &[crate::render_graph::DrawItem],
    mesh_manager: &crate::managers::RenderMeshManager,
    capacity: usize,
) -> (Vec<StaticBatch>, Vec<[[f32; 4]; 4]>) {
    prepare_static_batches_with(draw_list, |mesh| mesh_manager.get(mesh).is_some(), capacity)
}

fn prepare_static_batches_with(
    draw_list: &[crate::render_graph::DrawItem],
    mesh_exists: impl Fn(crate::managers::MeshHandle) -> bool,
    capacity: usize,
) -> (Vec<StaticBatch>, Vec<[[f32; 4]; 4]>) {
    let mut out = Vec::new();
    let mut matrices: Vec<[[f32; 4]; 4]> = Vec::new();
    for batch in build_static_batches_with(draw_list, mesh_exists) {
        if batch.items.len() > 1 && matrices.len() + batch.items.len() <= capacity {
            let mut batch = batch;
            batch.instance_offset = Some(matrices.len() as u32);
            matrices.extend(batch.items.iter().map(|&i| draw_list[i].model));
            out.push(batch);
        } else if batch.items.len() == 1 {
            out.push(batch);
        } else {
            // Capacity exhausted: keep correctness by falling back to one draw
            // per entity instead of truncating the instance range.
            out.extend(batch.items.into_iter().map(|i| StaticBatch {
                items: vec![i],
                instance_offset: None,
            }));
        }
    }
    (out, matrices)
}

/// `assets/shaders/slang/outline.slang::OutlinePush` 的 Rust 镜像。
///
/// 反向外壳只读取帧 UBO（set 0 binding 0）和一个推送常量块；反射报告
/// 块大小 96 字节（mat4 + vec4 + float + std140 尾部填充）。
#[repr(C)]
#[derive(Clone, Copy)]
struct OutlinePush {
    model: [[f32; 4]; 4],
    color: [f32; 4],
    thickness: f32,
    _pad: [f32; 3],
}

/// 向前 scene pass (bindless PBR + neutral ambient + shadow 映射表 targeting
/// the 交换链
///
/// 描述符 集合 布局 (mirrors `scene_frag.slang`):
/// 集合 0 - per-frame UBO 绑定 0) + 材质 SSBO 绑定 1)
/// one 描述符 集合 per frame-in-flight (UBO 缓冲区 differs)
/// 集合 1 - bindless 纹理 表 (samplers + SRV 数组 owned by
///            `RenderTextureManager::bindless`)
/// 集合 2 - IBL resources (3 combined 图像 samplers: env, irradiance, prefiltered)
/// 集合 3 - shadow 映射表 (SAMPLED_IMAGE + 比较 采样器
/// 集合 4 - previous-frame GTAO R8 可见性 纹理 (combined 图像 采样器
pub struct ForwardPass {
    /// 高动态范围 intermediate 颜色 格式 (the ForwardPass no longer targets the
    /// 交换链 directly; PostPass tonemaps 高动态范围 -> 交换链
    color_format: vk::Format,
    /// 格式 of the view-space 法线 MRT 附件 (SV_Target1). Written by
    /// the scene 片元 着色器 and 读取 by the GTAO pass
    normal_format: vk::Format,
    /// #883 Phase 2: screen-space motion vectors (SV_Target2). RGBA16F keeps
    /// linear sampling universally available; only .xy are used.
    velocity_format: vk::Format,
    /// Bindless handle for the BRDF LUT (registered in the bindless 纹理 表
    brdf_handle: u32,
    /// Byte stride between camera slots in the per-frame UBO (#461).
    camera_stride: u32,
    /// One 帧缓冲 per 交换链 图像 With N 交换链 images and N
    /// frames in flight, several 命令 buffers can 引用 their
    /// respective framebuffers concurrently - so we can't keep just one
    /// rotating 帧缓冲 (destroying it while a prior frame's 命令
    /// 缓冲区 still references it triggers
    /// VUID-vkDestroyFramebuffer-framebuffer-00892 and cascades into a
    /// device-lost). Indexed by `image_index` from `acquire_next_image`.
    framebuffers: Vec<Option<vk::Framebuffer>>,
    /// One 高动态范围 颜色 图像 per 交换链 图像 (the ForwardPass 渲染 目标
    /// replacing the old direct-to-swapchain path). Reused by PostPass as its
    /// sampled 输入
    color_images: Vec<Option<crate::render_pass::NormalImage>>,
    /// One 深度 图像 per 交换链 图像 (each 帧缓冲 references its
    /// own 深度 视图 并行 to `framebuffers`.
    depth_images: Vec<Option<crate::render_pass::DepthImage>>,
    /// One view-space 法线 图像 per 交换链 图像 (MRT SV_Target1). Same
    /// per-slot 生命周期 as `depth_images`: rebuilt only when its 交换链
    /// 视图 changes.
    normal_images: Vec<Option<crate::render_pass::NormalImage>>,
    /// One screen-space motion-vector image per swapchain image (#883 Phase 2).
    velocity_images: Vec<Option<crate::render_pass::NormalImage>>,
    /// Cached image_index validity markers (one per 槽 `set_target` uses
    /// `framebuffers[idx].is_some()` as the 当前 check; this field is
    /// kept for parity with the old swapchain-view tracking 模式
    target_views: Vec<vk::ImageView>,
    /// Number of 交换链 images. 集合 by `set_image_count` (called from
    /// `GraphRenderer::recreate_swapchain` after the 交换链 is recreated)
    /// and used by `ensure_target` so the per-image 帧缓冲 vectors are
    /// sized correctly. Decouples 帧缓冲 (re)creation from
    /// `GraphRenderer`'s per-frame 调用 sequence.
    image_count: usize,
    /// 图 资源 handles for this pass's outputs, created in `setup` and
    /// published 视图 registered) in 执行 so downstream passes
    /// (`GtaoPass`, `PostPass`) 读取 them by handle instead of `GraphRenderer`
    /// poking into `ForwardPass` internals. The 图 does not allocate the
    /// underlying images (ForwardPass still owns its framebuffers in PR-1);
    /// only the handle->view 映射 lives in `GraphResources`.
    out_color_h: ResourceHandle,
    out_depth_h: ResourceHandle,
    out_normal_h: ResourceHandle,
    out_velocity_h: ResourceHandle,
    extent: vk::Extent2D,
    render_pass: Option<vk::RenderPass>,
    /// 主通道管线缓存：按 [`DrawState`]（剔除 / 深度写 / 混合）各一份。
    ///
    /// 所有变体的描述符集合布局 / 推送常量范围完全相同，只有光栅化与颜色
    /// 混合状态不同，因此描述符绑定与推送常量在一次绘制列表内可以跨变体复用。
    /// key 的第二项是线框开关（#431）：`POLYGON_MODE_LINE` 需要单独的管线。
    pipelines: std::collections::HashMap<(DrawState, bool, bool), GraphicsPipeline>,
    /// 选中描边的反向外壳管线（#487）：只读 set 0 的帧 UBO，片元输出纯色。
    /// 与场景网格共享顶点格式 / 渲染 pass / 深度附件，只是剔除正面、不写深度。
    outline_pipeline: Option<GraphicsPipeline>,
    /// 填充 + 线框叠加的覆盖管线（#431）。复用 outline 着色器（厚度 0、纯色），
    /// `POLYGON_MODE_LINE` + 负深度 bias 压在填充面上。
    shaded_wireframe_pipeline: Option<GraphicsPipeline>,
    ibl_descriptor_set: vk::DescriptorSet,
    /// IBL 描述符 集合 布局 (borrowed from `IblResources`). Used by the
    /// skybox pass to 构建 its 管线 布局
    ibl_layout: vk::DescriptorSetLayout,
    shadow_ds_layout: Option<vk::DescriptorSetLayout>,
    shadow_descriptor_set: vk::DescriptorSet,
    shadow_ds_pool: Option<vk::DescriptorPool>,
    /// 集合 0 - per-frame-in-flight 描述符 sets 绑定 the 帧 UBO
    /// 绑定 0) + the 材质 SSBO 绑定 1). Indexed by
    /// `frame_index` (frame-in-flight, 0..N), NOT 交换链 image_index.
    frame_sets: Vec<vk::DescriptorSet>,
    /// 集合 0 布局 帧 UBO + materials SSBO). Owned + destroyed on 放置
    frame_set_layout: Option<vk::DescriptorSetLayout>,
    /// 池 backing `frame_sets`. Owned + destroyed on 放置
    frame_set_pool: Option<vk::DescriptorPool>,
    /// 集合 1 - bindless 纹理 表 描述符 集合 (from
    /// `RenderTextureManager::bindless()`). Not owned by ForwardPass.
    bindless_set: vk::DescriptorSet,
    /// 集合 1 布局 (from `BindlessTextureTable::layout`). Borrowed for
    /// pipeline-layout creation; not destroyed by ForwardPass.
    bindless_layout: vk::DescriptorSetLayout,
    /// 光源 SSBO (集合 0 绑定 2): host-visible 缓冲区 holding up to
    /// `LIGHT_MAX` hard-coded point lights. One per frame-in-flight to avoid
    /// tearing and race conditions between concurrent in-flight frames.
    light_buffers: Vec<vk::Buffer>,
    light_memories: Vec<vk::DeviceMemory>,
    /// Bone palette SSBO (set 0 binding 3, #560 P0-3): one per frame in
    /// flight, `BONE_CAPACITY` mat4 entries each.
    bone_buffers: Vec<vk::Buffer>,
    bone_memories: Vec<vk::DeviceMemory>,
    /// Previous-frame model matrices (#883 Phase 2), indexed by draw-list
    /// position. Separate from the bone SSBO so uploads cannot clobber skinned
    /// palettes; read by the vertex stage as set 0 binding 4.
    prev_model_buffers: Vec<vk::Buffer>,
    prev_model_memories: Vec<vk::DeviceMemory>,
    /// 集合 4 - previous-frame GTAO R8 可见性 纹理 (combined 图像
    /// 采样器 One 描述符 集合 per frame-in-flight so updating the 环境光遮蔽
    /// 视图 for 帧 N doesn't disturb 帧 N-1's still-in-flight 集合
    ao_ds_layout: Option<vk::DescriptorSetLayout>,
    /// One 环境光遮蔽 描述符 集合 per frame-in-flight (parallels `frame_sets`).
    ao_descriptor_sets: Vec<vk::DescriptorSet>,
    ao_ds_pool: Option<vk::DescriptorPool>,
    ao_sampler: vk::Sampler,
    /// The 环境光遮蔽 视图 currently bound to each frame-in-flight's 环境光遮蔽 描述符
    /// 集合 Tracked so we skip 冗余 描述符 rewrites.
    ao_views: Vec<vk::ImageView>,
    /// 最后一个 时间 the AO_PROBE 调试 line in `set_ao` was logged; throttled to
    /// once per 秒 so it doesn't flood the 对数 at 帧 rate.
    last_probe_log: Instant,
    /// 集合 5 - probe 音量 全局光照 (borrowed from `SceneScope`, scene-level).
    /// 绑定 0: 3D 纹理 (SAMPLED_IMAGE), 绑定 1: ProbeVolumeInfo UBO.
    gi_descriptor_set: vk::DescriptorSet,
    /// 全局光照 描述符 集合 布局 (borrowed from `SceneScope`). Used for
    /// pipeline-layout creation; NOT destroyed by ForwardPass.
    gi_layout: vk::DescriptorSetLayout,
    /// Skybox background pass (draws the IBL env cubemap). Owns its 管线 +
    /// set-2 (IBL env) 布局 borrows the IBL 描述符 集合
    skybox: SkyboxPass,
    /// 2D sprite pass (instanced quads, bindless textures). Drawn after the
    /// 3D mesh list and before the debug gizmo; owns its pipeline + a
    /// per-frame instance buffer.
    sprites: crate::sprite_pass::SpritePass,
    /// 3D particle pass (camera-facing billboards with a soft radial falloff).
    /// Drawn after the opaque mesh list (depth test on, write off) and before
    /// the 2D sprite pass; owns its pipeline + a per-frame instance buffer.
    particles: crate::particle_pass::ParticlePass,
    /// World-space XYZ orientation gizmo, drawn on 顶部 of the scene 深度
    /// test 禁用 内置 lazily once the 渲染 pass 存在
    gizmo: Option<Gizmo>,
    /// 地面参考网格 (Y = 0)，开启深度测试被场景几何体自然遮挡
    ground_grid: Option<GroundGrid>,
    device: Option<ash::Device>,
}
impl ForwardPass {
    pub fn new(_swapchain_color_format: vk::Format) -> Self {
        Self {
            // 高动态范围 intermediate 目标 线性 PostPass tonemaps this to the
            // sRGB 交换链 The old `_swapchain_color_format` argument is
            // kept for API stability; PostPass owns the swapchain-format
            // 管线 + 渲染 pass
            color_format: vk::Format::R16G16B16A16_SFLOAT,
            // R16G16B16A16_SFLOAT: 有符号 浮点数 so view-space normals (which
            // can be 负 in any axis) 存储 without bias/packing. 4th
            // 通道 unused 着色器 writes 0).
            normal_format: vk::Format::R16G16B16A16_SFLOAT,
            velocity_format: vk::Format::R16G16B16A16_SFLOAT,
            brdf_handle: u32::MAX,
            camera_stride: 0,
            framebuffers: Vec::new(),
            color_images: Vec::new(),
            depth_images: Vec::new(),
            normal_images: Vec::new(),
            velocity_images: Vec::new(),
            target_views: Vec::new(),
            image_count: 0,
            out_color_h: ResourceHandle::INVALID,
            out_depth_h: ResourceHandle::INVALID,
            out_normal_h: ResourceHandle::INVALID,
            out_velocity_h: ResourceHandle::INVALID,
            extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
            render_pass: None,
            pipelines: std::collections::HashMap::new(),
            outline_pipeline: None,
            shaded_wireframe_pipeline: None,
            ibl_descriptor_set: vk::DescriptorSet::null(),
            ibl_layout: vk::DescriptorSetLayout::null(),
            shadow_ds_layout: None,
            shadow_descriptor_set: vk::DescriptorSet::null(),
            shadow_ds_pool: None,
            frame_sets: Vec::new(),
            frame_set_layout: None,
            frame_set_pool: None,
            bindless_set: vk::DescriptorSet::null(),
            bindless_layout: vk::DescriptorSetLayout::null(),
            light_buffers: Vec::new(),
            light_memories: Vec::new(),
            bone_buffers: Vec::new(),
            bone_memories: Vec::new(),
            prev_model_buffers: Vec::new(),
            prev_model_memories: Vec::new(),
            ao_ds_layout: None,
            ao_descriptor_sets: Vec::new(),
            ao_ds_pool: None,
            ao_sampler: vk::Sampler::null(),
            ao_views: Vec::new(),
            last_probe_log: Instant::now(),
            gi_descriptor_set: vk::DescriptorSet::null(),
            gi_layout: vk::DescriptorSetLayout::null(),
            skybox: SkyboxPass::new(vk::DescriptorSet::null(), vk::DescriptorSetLayout::null()),
            sprites: crate::sprite_pass::SpritePass::new(),
            particles: crate::particle_pass::ParticlePass::new(),
            gizmo: None,
            ground_grid: None,
            device: None,
        }
    }

    /// Ensure the 帧缓冲 for `image_index` 存在 and is 内置 against the
    /// 当前 extent. Returns the 帧缓冲 handle via
    /// `self.framebuffers[image_index]` 读取 by 执行
    ///
    /// With N 交换链 images and N frames in flight, several 命令 buffers
    /// can be in flight at once - each referencing its own 帧缓冲 So we
    /// keep **one 帧缓冲 per 交换链 image** (plus its own 高动态范围 颜色 +
    /// 深度 + 法线 图像 and only rebuild an entry when the extent changed.
    /// This avoids destroying a 帧缓冲 that a prior (still in-flight)
    /// 命令 缓冲区 references (VUID-vkDestroyFramebuffer-framebuffer-00892).
    ///
    /// `image_index` is the value returned by `acquire_next_image`;
    /// `image_count` is `swapchain.views.len()` (so we can 大小 the per-slot
    /// vectors on the 第一个 调用 or after a recreate).
    pub fn set_image_count(&mut self, image_count: usize) {
        self.image_count = image_count;
    }

    /// Idempotent per-frame (re)creation of this 交换链 image's
    /// 帧缓冲 + HDR/depth/normal attachments. Called from `ForwardPass::
    /// 执行 (driven by the `RenderGraph`) so 帧缓冲 lifecycle no
    /// longer depends on `GraphRenderer` calling `set_target` every 帧
    /// Rebuilds only the entry for `image_index` when it is 缺少 or the
    /// 交换链 changed; safe against in-flight framebuffers (mirrors the
    /// old `set_target` 契约
    /// 场景切换时切换 IBL 描述符 集合 句柄与 BRDF LUT 的 bindless 槽。
    /// set2 布局共享（不重建），仅替换 set 句柄并同步 skybox pass 绑定的 set2。
    pub fn set_ibl(&mut self, descriptor_set: vk::DescriptorSet, brdf_handle: u32) {
        self.ibl_descriptor_set = descriptor_set;
        self.brdf_handle = brdf_handle;
        self.skybox.set_descriptor_set(descriptor_set);
    }

    pub fn ensure_target(
        &mut self,
        device: &ash::Device,
        context: &crate::context::VulkanContext,
        image_index: u32,
        extent: vk::Extent2D,
    ) -> Result<()> {
        self.set_target(device, context, self.image_count, image_index, extent)
    }

    pub fn set_target(
        &mut self,
        device: &ash::Device,
        context: &crate::context::VulkanContext,
        image_count: usize,
        image_index: u32,
        extent: vk::Extent2D,
    ) -> Result<()> {
        if extent.width == 0 || extent.height == 0 {
            return Ok(());
        }

        let idx = image_index as usize;
        if idx >= image_count {
            return Ok(());
        }

        // The 渲染 pass must exist before we 构建 a 帧缓冲 against it.
        // `ensure_render_pass` is idempotent (early-returns once 集合
        self.ensure_render_pass(context)?;

        // If the 交换链 图像 count changed (recreate with a different
        // 图像 count) or the extent changed, tear everything 下 and 调整大小
        // the per-image vectors. This is the only place we 销毁 framebuffers
        // wholesale; per-frame we only (re)build the single entry for this
        // `image_index` - so an in-flight frame's 帧缓冲 is never touched.
        let swapchain_changed = self.target_views.len() != image_count || self.extent != extent;
        if swapchain_changed {
            self.drop_target(device);
            self.target_views = vec![vk::ImageView::null(); image_count];
            self.extent = extent;
            self.framebuffers = (0..image_count).map(|_| None).collect();
            self.color_images = (0..image_count).map(|_| None).collect();
            self.depth_images = (0..image_count).map(|_| None).collect();
            self.normal_images = (0..image_count).map(|_| None).collect();
            self.velocity_images = (0..image_count).map(|_| None).collect();
        }

        // 构建 this image's 帧缓冲 + 颜色 + 深度 + 法线 if not
        // already 当前
        let already_current = self.framebuffers[idx].is_some();
        if !already_current {
            let rp = self
                .render_pass
                .context("ForwardPass: render_pass missing in set_target")?;

            // 替换 the 高动态范围 颜色 图像 for this 槽
            let color_image =
                crate::render_pass::NormalImage::new(context, extent, self.color_format)
                    .context("ForwardPass: create HDR color image")?;
            if let Some(mut old) = self.color_images[idx].take() {
                unsafe { old.destroy(device) };
            }
            self.color_images[idx] = Some(color_image);

            // 替换 the 深度 图像 for this 槽 创建 new, 销毁 old).
            let depth_image = crate::render_pass::DepthImage::new(context, extent)
                .context("ForwardPass: create depth image")?;
            if let Some(mut old) = self.depth_images[idx].take() {
                unsafe { old.destroy(device) };
            }
            self.depth_images[idx] = Some(depth_image);

            // 替换 the view-space 法线 MRT 图像 for this 槽
            let normal_image =
                crate::render_pass::NormalImage::new(context, extent, self.normal_format)
                    .context("ForwardPass: create normal image")?;
            if let Some(mut old) = self.normal_images[idx].take() {
                unsafe { old.destroy(device) };
            }
            self.normal_images[idx] = Some(normal_image);

            // #883 Phase 2: per-pixel motion vectors for TAA.
            let velocity_image =
                crate::render_pass::NormalImage::new(context, extent, self.velocity_format)
                    .context("ForwardPass: create velocity image")?;
            if let Some(mut old) = self.velocity_images[idx].take() {
                unsafe { old.destroy(device) };
            }
            self.velocity_images[idx] = Some(velocity_image);

            // 销毁 the old 帧缓冲 for this 槽 BEFORE creating the
            // new one (order doesn't matter for 验证 here since both
            // 引用 the same 槽 but destroy-old-first is tidy).
            if let Some(old_fb) = self.framebuffers[idx].take() {
                unsafe { device.destroy_framebuffer(old_fb, None) };
            }

            let color = self.color_images[idx].as_ref().unwrap();
            let depth = self.depth_images[idx].as_ref().unwrap();
            let normal = self.normal_images[idx].as_ref().unwrap();
            let velocity = self.velocity_images[idx].as_ref().unwrap();
            // 渲染 pass 附件 order: 颜色 深度 法线 速度
            let attachments = [color.view, depth.view, normal.view, velocity.view];
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
            .context("ForwardPass: create framebuffer")?;
            self.framebuffers[idx] = Some(fb);
        }
        Ok(())
    }

    /// 放置 the swapchain-derived framebuffers + 深度 images.
    ///
    /// Must be called **before** the 交换链 is recreated (and from
    /// `set_target` when the 交换链 changes): each 帧缓冲 wraps a
    /// 交换链 图像 视图 + 深度 视图 and `Swapchain::recreate` destroys
    /// the old views. Destroying the views while the framebuffers still
    /// 引用 them triggers `vkDestroyImageView` 验证 errors which
    /// cascade into a device-lost on the 下一个 submit.
    ///
    /// Framebuffers are destroyed before their 深度 images (each 帧缓冲
    /// references its 深度 视图 as an 附件 The 渲染 pass + 管线
    /// are kept (they don't 引用 交换链 views); `set_target` rebuilds
    /// the framebuffers + 深度 on the 下一个 帧
    pub fn drop_target(&mut self, device: &ash::Device) {
        // Framebuffers 第一个 (they 引用 颜色 + 深度 + 法线 views).
        for fb in self.framebuffers.drain(..).flatten() {
            unsafe { device.destroy_framebuffer(fb, None) };
        }
        // Then 高动态范围 颜色 images.
        for color in self.color_images.drain(..).flatten() {
            let mut c = color;
            unsafe { c.destroy(device) };
        }
        // Then 深度 images (destroys each 深度 视图
        for depth in self.depth_images.drain(..).flatten() {
            let mut d = depth;
            unsafe { d.destroy(device) };
        }
        // Then view-space 法线 MRT images.
        for normal in self.normal_images.drain(..).flatten() {
            let mut n = normal;
            unsafe { n.destroy(device) };
        }
        // Then motion-vector images (#883 Phase 2).
        for velocity in self.velocity_images.drain(..).flatten() {
            let mut v = velocity;
            unsafe { v.destroy(device) };
        }
        self.target_views.clear();
        self.extent = vk::Extent2D {
            width: 0,
            height: 0,
        };
    }

    /// Tear 下 ALL ForwardPass GPU resources (framebuffers, 深度 images,
    /// 渲染 pass 管线 shadow 描述符 集合 布局 + 池
    ///
    /// Called from `GraphRenderer::destroy` on shutdown. After this the
    /// ForwardPass is 空 `device_wait_idle` must already have been called by
    /// the 调用者 so no 命令 buffers are in flight.
    pub fn destroy(&mut self, device: &ash::Device) {
        // Framebuffers + 深度 images (swapchain-derived).
        self.drop_target(device);

        // 渲染 pass
        if let Some(rp) = self.render_pass.take() {
            unsafe { device.destroy_render_pass(rp, None) };
        }
        // 管线（三种剔除变体，frees 管线 + 布局 via GraphicsPipeline::Drop）。
        self.pipelines.clear();
        self.shaded_wireframe_pipeline = None;

        // 集合 0: 帧 UBO + materials SSBO 布局 + 池 (sets freed with 池
        if let Some(layout) = self.frame_set_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(layout, None) };
        }
        if let Some(pool) = self.frame_set_pool.take() {
            unsafe { device.destroy_descriptor_pool(pool, None) };
        }
        self.frame_sets.clear();

        // Shadow 描述符 集合 布局 + 池 (the 集合 itself is freed with
        // the 池
        if let Some(layout) = self.shadow_ds_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(layout, None) };
        }
        if let Some(pool) = self.shadow_ds_pool.take() {
            unsafe { device.destroy_descriptor_pool(pool, None) };
        }
        self.shadow_descriptor_set = vk::DescriptorSet::null();
        // 光源 SSBO.
        for buf in self.light_buffers.drain(..) {
            if buf != vk::Buffer::null() {
                unsafe { device.destroy_buffer(buf, None) };
            }
        }
        for mem in self.light_memories.drain(..) {
            if mem != vk::DeviceMemory::null() {
                unsafe { device.free_memory(mem, None) };
            }
        }
        for buf in self.bone_buffers.drain(..) {
            if buf != vk::Buffer::null() {
                unsafe { device.destroy_buffer(buf, None) };
            }
        }
        for mem in self.bone_memories.drain(..) {
            if mem != vk::DeviceMemory::null() {
                unsafe { device.free_memory(mem, None) };
            }
        }
        for buf in self.prev_model_buffers.drain(..) {
            if buf != vk::Buffer::null() {
                unsafe { device.destroy_buffer(buf, None) };
            }
        }
        for mem in self.prev_model_memories.drain(..) {
            if mem != vk::DeviceMemory::null() {
                unsafe { device.free_memory(mem, None) };
            }
        }
        // Skybox pass (its own 管线 + set-2 布局
        self.skybox.destroy(device);
        // Gizmo (its own 管线 + 顶点 缓冲区 放置 frees them).
        self.gizmo = None;
        self.ground_grid = None;

        // 集合 4: 环境光遮蔽 描述符 集合 布局 + 池 + 采样器
        if let Some(layout) = self.ao_ds_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(layout, None) };
        }
        if let Some(pool) = self.ao_ds_pool.take() {
            unsafe { device.destroy_descriptor_pool(pool, None) };
        }
        if self.ao_sampler != vk::Sampler::null() {
            unsafe { device.destroy_sampler(self.ao_sampler, None) };
            self.ao_sampler = vk::Sampler::null();
        }
        self.ao_descriptor_sets.clear();
        self.ao_views.clear();

        // 集合 5 全局光照 probe 音量 is borrowed from SceneScope — not destroyed here.
        self.gi_descriptor_set = vk::DescriptorSet::null();
        self.gi_layout = vk::DescriptorSetLayout::null();

        self.device = None;
    }
    /// Wire all 外部 resources the ForwardPass needs:
    /// - IBL cubemap 描述符 集合 集合 2)
    /// - shadow 映射表 视图 + 比较 采样器 集合 3)
    /// - bindless 纹理 表 集合 + 布局 集合 1)
    /// - 材质 SSBO 缓冲区 + per-frame UBO buffers 集合 0, one 集合 per
    /// frame-in-flight so each frame's UBO 缓冲区 is bound without 运行时
    /// 描述符 rewrites)
    /// - 光源 SSBO 缓冲区 集合 0 绑定 2, hard-coded point lights)
    /// - 全局光照 probe 音量 描述符 集合 + 布局 集合 5, borrowed from SceneScope)
    ///
    /// `frame_ubo_buffers` 长度 determines the frame-in-flight count (== set0
    /// 集合 count). `materials_buffer` is the `RenderMaterialManager` SSBO.
    #[allow(clippy::too_many_arguments)]
    pub fn set_resources(
        &mut self,
        context: &crate::context::VulkanContext,
        ibl_descriptor_set: vk::DescriptorSet,
        ibl_layout: vk::DescriptorSetLayout,
        shadow_view: vk::ImageView,
        shadow_sampler: vk::Sampler,
        bindless_set: vk::DescriptorSet,
        bindless_layout: vk::DescriptorSetLayout,
        materials_buffer: vk::Buffer,
        frame_ubo_buffers: &[vk::Buffer],
        brdf_handle: u32,
        gi_descriptor_set: vk::DescriptorSet,
        gi_layout: vk::DescriptorSetLayout,
        camera_stride: u32,
    ) -> Result<()> {
        let device = &context.device;
        self.ibl_descriptor_set = ibl_descriptor_set;
        self.ibl_layout = ibl_layout;
        self.bindless_set = bindless_set;
        self.bindless_layout = bindless_layout;
        self.brdf_handle = brdf_handle;
        self.camera_stride = camera_stride;
        self.gi_descriptor_set = gi_descriptor_set;
        self.gi_layout = gi_layout;
        // Skybox reuses the IBL env cubemap 描述符 集合 + 布局 集合 0).
        self.skybox = SkyboxPass::new(ibl_descriptor_set, ibl_layout);

        // ---- 集合 0: per-frame UBO 绑定 0) + materials SSBO 绑定 1)
        // + 光源 SSBO 绑定 2) ----
        // One 描述符 集合 per frame-in-flight; each binds its own UBO
        // 缓冲区 at 绑定 0 and the (shared) materials SSBO at 绑定 1
        // and the (shared) 光源 SSBO at 绑定 2.
        // 内置 once here; never rewritten at 运行时
        let frame_set_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(scene_frag::FRAME_BINDING)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(scene_frag::MATERIALS_BINDING)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(scene_frag::POINT_LIGHTS_BINDING)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            // binding 3: bone palette (#560 P0-3), read by the vertex stage.
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX),
            // binding 4: previous-frame model matrices (#883 Phase 2), one
            // entry per draw-list item. Read by the vertex stage to build
            // object motion vectors.
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX),
        ];
        let frame_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&frame_set_bindings),
                None,
            )
        }
        .context("ForwardPass: create set0 (frame+materials+lights) layout")?;

        // Tear 下 any prior set0 layout/pool/sets (e.g. on re-init).
        if let Some(old) = self.frame_set_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(old, None) };
        }
        if let Some(old) = self.frame_set_pool.take() {
            unsafe { device.destroy_descriptor_pool(old, None) };
        }
        self.frame_sets.clear();

        let fif_count = frame_ubo_buffers.len();
        // 池 needs: UNIFORM_BUFFER fif_count, plus three STORAGE_BUFFER
        // bindings per set (materials, point lights, bone palette).
        let frame_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(fif_count as u32)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::UNIFORM_BUFFER,
                            descriptor_count: fif_count as u32,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_BUFFER,
                            descriptor_count: (fif_count * 4) as u32,
                        },
                    ]),
                None,
            )
        }
        .context("ForwardPass: create set0 pool")?;

        let layout_ptrs: Vec<vk::DescriptorSetLayout> =
            (0..fif_count).map(|_| frame_layout).collect();
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(frame_pool)
                    .set_layouts(&layout_ptrs),
            )
        }
        .context("ForwardPass: allocate set0 sets")?;

        // ---- 光源 SSBO (绑定 2) ----
        // 创建 host-visible, coherent 缓冲区 for up to `LIGHT_MAX` point
        // lights, one per frame-in-flight to avoid tearing and data races.
        // The 缓冲区 is zero-initialized here; `ForwardPass::update_lights`
        // rewrites the contents every frame from the ECS `PointLight` 查询 (see `render_system`).
        let light_ssbo_size = (crate::descriptor::LIGHT_MAX as vk::DeviceSize)
            * crate::descriptor::GPU_LIGHT_SIZE as vk::DeviceSize;
        let mut new_light_buffers = Vec::with_capacity(fif_count);
        let mut new_light_memories = Vec::with_capacity(fif_count);
        for _ in 0..fif_count {
            let (buf, mem) = crate::buffer::create_buffer(
                context,
                light_ssbo_size,
                crate::buffer::BufferUsage::STORAGE_BUFFER,
                crate::buffer::MemoryProperties::HOST_VISIBLE
                    | crate::buffer::MemoryProperties::HOST_COHERENT,
            )
            .context("ForwardPass: create light SSBO buffer")?;

            // Zero-initialize so the 第一个 帧 (before any `update_lights` 调用)
            // doesn't 读取 garbage.
            let light_ptr =
                unsafe { device.map_memory(mem, 0, light_ssbo_size, vk::MemoryMapFlags::empty()) }
                    .context("ForwardPass: map light SSBO memory")?;
            unsafe {
                std::ptr::write_bytes(light_ptr as *mut u8, 0, light_ssbo_size as usize);
            }
            unsafe { device.unmap_memory(mem) };

            new_light_buffers.push(buf);
            new_light_memories.push(mem);
        }

        // ---- Bone palette SSBO (binding 3, #560 P0-3) ----
        // One host-visible buffer per frame in flight. `BONE_CAPACITY` is the
        // number of mat4 entries a single frame can address; the engine packs
        // every skinned draw's palette into it each frame.
        let bone_ssbo_size =
            (Self::BONE_CAPACITY as vk::DeviceSize) * (std::mem::size_of::<[[f32; 4]; 4]>() as u64);
        let mut new_bone_buffers = Vec::with_capacity(fif_count);
        let mut new_bone_memories = Vec::with_capacity(fif_count);
        for _ in 0..fif_count {
            let (buf, mem) = crate::buffer::create_buffer(
                context,
                bone_ssbo_size,
                crate::buffer::BufferUsage::STORAGE_BUFFER,
                crate::buffer::MemoryProperties::HOST_VISIBLE
                    | crate::buffer::MemoryProperties::HOST_COHERENT,
            )
            .context("ForwardPass: create bone SSBO buffer")?;
            let ptr =
                unsafe { device.map_memory(mem, 0, bone_ssbo_size, vk::MemoryMapFlags::empty()) }
                    .context("ForwardPass: map bone SSBO memory")?;
            unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, bone_ssbo_size as usize) };
            unsafe { device.unmap_memory(mem) };
            new_bone_buffers.push(buf);
            new_bone_memories.push(mem);
        }

        // ---- Previous-frame model SSBO (binding 4, #883 Phase 2) ----
        // One mat4 per draw-list item, uploaded by `GraphRenderer::execute`
        // before the graph runs. Separate from bones so a frame with both
        // skinned and moving static draws cannot overwrite either palette.
        let prev_model_ssbo_size = (Self::PREV_MODEL_CAPACITY as vk::DeviceSize)
            * (std::mem::size_of::<[[f32; 4]; 4]>() as u64);
        let mut new_prev_model_buffers = Vec::with_capacity(fif_count);
        let mut new_prev_model_memories = Vec::with_capacity(fif_count);
        for _ in 0..fif_count {
            let (buf, mem) = crate::buffer::create_buffer(
                context,
                prev_model_ssbo_size,
                crate::buffer::BufferUsage::STORAGE_BUFFER,
                crate::buffer::MemoryProperties::HOST_VISIBLE
                    | crate::buffer::MemoryProperties::HOST_COHERENT,
            )
            .context("ForwardPass: create prev-model SSBO buffer")?;
            let ptr = unsafe {
                device.map_memory(mem, 0, prev_model_ssbo_size, vk::MemoryMapFlags::empty())
            }
            .context("ForwardPass: map prev-model SSBO memory")?;
            unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, prev_model_ssbo_size as usize) };
            unsafe { device.unmap_memory(mem) };
            new_prev_model_buffers.push(buf);
            new_prev_model_memories.push(mem);
        }

        // 销毁 old 光源 缓冲区 if any.
        for buf in self.light_buffers.drain(..) {
            if buf != vk::Buffer::null() {
                unsafe { device.destroy_buffer(buf, None) };
            }
        }
        for mem in self.light_memories.drain(..) {
            if mem != vk::DeviceMemory::null() {
                unsafe { device.free_memory(mem, None) };
            }
        }
        for buf in self.bone_buffers.drain(..) {
            if buf != vk::Buffer::null() {
                unsafe { device.destroy_buffer(buf, None) };
            }
        }
        for mem in self.bone_memories.drain(..) {
            if mem != vk::DeviceMemory::null() {
                unsafe { device.free_memory(mem, None) };
            }
        }
        for buf in self.prev_model_buffers.drain(..) {
            if buf != vk::Buffer::null() {
                unsafe { device.destroy_buffer(buf, None) };
            }
        }
        for mem in self.prev_model_memories.drain(..) {
            if mem != vk::DeviceMemory::null() {
                unsafe { device.free_memory(mem, None) };
            }
        }
        self.light_buffers = new_light_buffers;
        self.light_memories = new_light_memories;
        self.bone_buffers = new_bone_buffers;
        self.bone_memories = new_bone_memories;
        self.prev_model_buffers = new_prev_model_buffers;
        self.prev_model_memories = new_prev_model_memories;

        // 写入 each 集合 绑定 0 = this frame's UBO, 绑定 1 = materials SSBO,
        // 绑定 2 = this frame's 光源 SSBO.
        let ubo_size = std::mem::size_of::<crate::descriptor::FrameUBOData>() as vk::DeviceSize;
        let mat_ssbo_size = vk::WHOLE_SIZE; // SSBO: whole buffer is fine.
        let mat_info = vk::DescriptorBufferInfo::default()
            .buffer(materials_buffer)
            .offset(0)
            .range(mat_ssbo_size);
        let light_infos: Vec<vk::DescriptorBufferInfo> = self
            .light_buffers
            .iter()
            .map(|buf| {
                vk::DescriptorBufferInfo::default()
                    .buffer(*buf)
                    .offset(0)
                    .range(vk::WHOLE_SIZE)
            })
            .collect();
        // Collect all per-frame UBO infos 第一个 so the `writes` 切片
        // references below don't conflict with mutating `ubo_infos`.
        let ubo_infos: Vec<vk::DescriptorBufferInfo> = frame_ubo_buffers
            .iter()
            .map(|buf| {
                vk::DescriptorBufferInfo::default()
                    .buffer(*buf)
                    .offset(0)
                    .range(ubo_size)
            })
            .collect();
        let bone_infos: Vec<vk::DescriptorBufferInfo> = self
            .bone_buffers
            .iter()
            .map(|buf| {
                vk::DescriptorBufferInfo::default()
                    .buffer(*buf)
                    .offset(0)
                    .range(vk::WHOLE_SIZE)
            })
            .collect();
        let prev_model_infos: Vec<vk::DescriptorBufferInfo> = self
            .prev_model_buffers
            .iter()
            .map(|buf| {
                vk::DescriptorBufferInfo::default()
                    .buffer(*buf)
                    .offset(0)
                    .range(vk::WHOLE_SIZE)
            })
            .collect();
        let mut writes = Vec::with_capacity(fif_count * 5);
        for (i, set) in sets.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(scene_frag::FRAME_BINDING)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(std::slice::from_ref(&ubo_infos[i])),
            );
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(scene_frag::MATERIALS_BINDING)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&mat_info)),
            );
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(scene_frag::POINT_LIGHTS_BINDING)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&light_infos[i])),
            );
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(3)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&bone_infos[i])),
            );
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(4)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&prev_model_infos[i])),
            );
        }
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        self.frame_set_layout = Some(frame_layout);
        self.frame_set_pool = Some(frame_pool);
        self.frame_sets = sets;

        // ---- 集合 3: shadow 映射表 (SAMPLED_IMAGE + 比较 采样器 ----
        let shadow_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(scene_frag::SHADOW_MAP_BINDING)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(scene_frag::SHADOW_SAMPLER_BINDING)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let shadow_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&shadow_bindings),
                None,
            )
        }
        .context("ForwardPass: create shadow ds layout")?;

        if let Some(old) = self.shadow_ds_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(old, None) };
        }
        if let Some(old) = self.shadow_ds_pool.take() {
            unsafe { device.destroy_descriptor_pool(old, None) };
        }
        self.shadow_descriptor_set = vk::DescriptorSet::null();

        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::SAMPLED_IMAGE,
                            descriptor_count: 1,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::SAMPLER,
                            descriptor_count: 1,
                        },
                    ]),
                None,
            )
        }
        .context("ForwardPass: create shadow ds pool")?;

        let ds = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&[shadow_layout]),
            )
        }
        .context("ForwardPass: allocate shadow ds")?[0];

        let image_info = vk::DescriptorImageInfo::default()
            .image_view(shadow_view)
            .image_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL);
        let sampler_info = vk::DescriptorImageInfo::default()
            .sampler(shadow_sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(scene_frag::SHADOW_MAP_BINDING)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(std::slice::from_ref(&image_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(scene_frag::SHADOW_SAMPLER_BINDING)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(std::slice::from_ref(&sampler_info)),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        self.shadow_ds_layout = Some(shadow_layout);
        self.shadow_ds_pool = Some(pool);
        self.shadow_descriptor_set = ds;

        // ---- 集合 4: previous-frame GTAO R8 可见性 (combined 图像 采样器 ----
        // The 环境光遮蔽 视图 is updated every 帧 by `set_ao` (GraphRenderer passes
        // the GTAO pass's double-buffered 视图 for the 帧 the scene reads).
        // Here we only 创建 the 布局 + 池 + 采样器 + 描述符 集合 the
        // image_info 写入 happens in `set_ao` once a 视图 is available.
        let ao_bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(scene_frag::AO_TEX_BINDING)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let ao_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&ao_bindings),
                None,
            )
        }
        .context("ForwardPass: create set4 (AO) ds layout")?;

        // Tear 下 any prior set4 layout/pool/sampler (e.g. on re-init).
        if let Some(old) = self.ao_ds_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(old, None) };
        }
        if let Some(old) = self.ao_ds_pool.take() {
            unsafe { device.destroy_descriptor_pool(old, None) };
        }
        if self.ao_sampler != vk::Sampler::null() {
            unsafe { device.destroy_sampler(self.ao_sampler, None) };
        }
        self.ao_descriptor_sets.clear();
        self.ao_views.clear();

        let ao_sampler = unsafe {
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
        .context("ForwardPass: create AO sampler")?;

        // One 环境光遮蔽 描述符 集合 per frame-in-flight so `set_ao` can 更新
        // 帧 N's 集合 without disturbing 帧 N-1's still-in-flight 集合
        // (VUID-vkUpdateDescriptorSets-None-03047). The frame-in-flight count
        // matches `frame_ubo_buffers.len()` (== `frame_sets.len()`).
        let ao_fif = frame_ubo_buffers.len() as u32;
        let ao_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(ao_fif)
                    .pool_sizes(&[vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        descriptor_count: ao_fif,
                    }]),
                None,
            )
        }
        .context("ForwardPass: create set4 (AO) ds pool")?;

        let ao_layouts = vec![ao_layout; ao_fif as usize];
        let ao_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(ao_pool)
                    .set_layouts(&ao_layouts),
            )
        }
        .context("ForwardPass: allocate set4 (AO) ds")?;
        let ao_sets: Vec<vk::DescriptorSet> = ao_sets;

        self.ao_ds_layout = Some(ao_layout);
        self.ao_ds_pool = Some(ao_pool);
        self.ao_sampler = ao_sampler;
        self.ao_descriptor_sets = ao_sets;
        self.ao_views = vec![vk::ImageView::null(); ao_fif as usize];
        // The actual image_info 写入 happens in `set_ao` once the GTAO pass
        // produces its 第一个 环境光遮蔽 视图 Until then the descriptors point at null;
        // `PBR_FLAG_AO` is off by 默认 so nothing samples it.

        // 集合 5 全局光照 probe 音量 is borrowed from SceneScope — already wired
        // via `gi_descriptor_set` / `gi_layout` parameters above.

        Ok(())
    }

    /// 更新 the 集合 4 环境光遮蔽 描述符 for `frame_index` to point at 视图
    /// (the 上一个 frame's GTAO 输出 Called every 帧 from
    /// `GraphRenderer::render` BEFORE `forward_pass.execute`. Skips the
    /// 描述符 写入 when 视图 matches the currently-bound 视图 for this
    /// frame-in-flight.
    pub fn set_ao(&mut self, device: &ash::Device, frame_index: u32, view: vk::ImageView) {
        let i = (frame_index as usize) % self.ao_descriptor_sets.len();
        // TEMP PROBE: confirm set_ao runs with 有效 inputs. Throttled to once
        // per 秒 so the 对数 isn't flooded at 帧 rate; emitted at
        // 跟踪 so it stays quiet under the 默认 信息 滤波器
        if self.last_probe_log.elapsed().as_secs_f32() >= 1.0 {
            self.last_probe_log = Instant::now();
            log::trace!(
                "AO_PROBE set_ao: frame={} slot={} view={:?} prev_bound={:?} ao_views={:?} will_write={}",
                frame_index,
                i,
                view,
                self.ao_views[i],
                self.ao_views,
                view != self.ao_views[i] && view != vk::ImageView::null()
            );
        }
        if view == self.ao_views[i] {
            return;
        }
        self.ao_views[i] = view;
        if view == vk::ImageView::null() {
            // No 环境光遮蔽 yet 第一个 帧 or GTAO 禁用 - leave the 描述符
            // unbound. The shader's `aoTex.SampleLevel` is only reached when
            // PBR_FLAG_AO is 集合 which the app leaves off until the user
            // toggles it (by which 时间 视图 is non-null).
            return;
        }
        let image_info = vk::DescriptorImageInfo::default()
            .image_view(view)
            .sampler(self.ao_sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.ao_descriptor_sets[i])
            .dst_binding(scene_frag::AO_TEX_BINDING)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&image_info));
        unsafe { device.update_descriptor_sets(&[write], &[]) };
    }

    /// 借用 the 高动态范围 颜色 图像 视图 for `image_index`. Consumed by the
    /// PostPass as its sampled 输入
    pub fn color_view(&self, image_index: u32) -> Option<vk::ImageView> {
        self.color_images
            .get(image_index as usize)
            .and_then(|c| c.as_ref())
            .map(|c| c.view)
    }

    /// 借用 the 高动态范围 颜色 图像 handle for `image_index`. The PostPass needs
    /// the 图像 to record its SHADER_READ_ONLY_OPTIMAL 布局 屏障
    pub fn color_image(&self, image_index: u32) -> Option<vk::Image> {
        self.color_images
            .get(image_index as usize)
            .and_then(|c| c.as_ref())
            .map(|c| c.image)
    }

    /// 借用 the 深度 图像 视图 for `image_index` (the 槽 ForwardPass just
    /// rendered into). The GTAO pass samples it after ForwardPass stores 深度
    pub fn depth_view(&self, image_index: u32) -> Option<vk::ImageView> {
        self.depth_images
            .get(image_index as usize)
            .and_then(|d| d.as_ref())
            .map(|d| d.view)
    }

    /// 借用 the 深度 图像 handle for `image_index`. The GTAO pass needs the
    /// 图像 (not just the 视图 to record its DEPTH_STENCIL_READ_ONLY_OPTIMAL
    /// 布局 屏障 before sampling.
    pub fn depth_image(&self, image_index: u32) -> Option<vk::Image> {
        self.depth_images
            .get(image_index as usize)
            .and_then(|d| d.as_ref())
            .map(|d| d.image)
    }

    /// 借用 the view-space 法线 MRT 视图 for `image_index`. Consumed by
    /// the GTAO pass when its 众数 == 0` 法线 MRT path).
    pub fn normal_view(&self, image_index: u32) -> Option<vk::ImageView> {
        self.normal_images
            .get(image_index as usize)
            .and_then(|n| n.as_ref())
            .map(|n| n.view)
    }

    /// 借用 the view-space 法线 MRT 图像 handle for `image_index`. The
    /// GTAO pass needs the 图像 to record its SHADER_READ_ONLY_OPTIMAL 布局
    /// 屏障 before sampling.
    pub fn normal_image(&self, image_index: u32) -> Option<vk::Image> {
        self.normal_images
            .get(image_index as usize)
            .and_then(|n| n.as_ref())
            .map(|n| n.image)
    }

    /// Borrow the motion-vector image view for `image_index` (#883 Phase 2).
    pub fn velocity_view(&self, image_index: u32) -> Option<vk::ImageView> {
        self.velocity_images
            .get(image_index as usize)
            .and_then(|v| v.as_ref())
            .map(|v| v.view)
    }

    /// Borrow the motion-vector image handle for `image_index` (#883 Phase 2).
    pub fn velocity_image(&self, image_index: u32) -> Option<vk::Image> {
        self.velocity_images
            .get(image_index as usize)
            .and_then(|v| v.as_ref())
            .map(|v| v.image)
    }

    /// The full-resolution extent the scene was rendered at. The GTAO pass
    /// uses this (halved) to 大小 its own 视口 + 环境光遮蔽 textures.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// 高动态范围 intermediate 颜色 格式 (the scene 目标 PostPass tonemaps).
    /// Exposed for the render-graph visualizer.
    pub fn color_format(&self) -> vk::Format {
        self.color_format
    }

    /// View-space 法线 MRT 格式 读取 by GTAO). Exposed for the viz.
    pub fn normal_format(&self) -> vk::Format {
        self.normal_format
    }

    /// Number of 交换链 images (framebuffers / 高动态范围 颜色 / 深度 / 法线
    /// slots are all sized to this). Exposed for the viz.
    pub fn image_count(&self) -> usize {
        self.image_count
    }

    /// The three well-known 输出 handles 颜色 法线 深度 in the
    /// same order `setup` declares them. Exposed for the viz's edge labels.
    pub fn out_handles(&self) -> [ResourceHandle; 3] {
        [self.out_color_h, self.out_normal_h, self.out_depth_h]
    }

    /// Clamp light count to `LIGHT_MAX` to avoid buffer overflow.
    #[inline]
    pub const fn clamped_light_count(raw_count: usize) -> usize {
        let max = crate::descriptor::LIGHT_MAX as usize;
        if raw_count > max {
            max
        } else {
            raw_count
        }
    }

    /// Rewrite the point-light SSBO for this frame from a fresh `&[GpuLight]` slice.
    /// Called every frame from `ForwardPass::execute` with the lights collected by
    /// `render_system` from the ECS world. Unused slots (between `lights.len()`
    /// and `LIGHT_MAX`) are zeroed so the shader doesn't read stale data.
    ///
    /// Copies at most `LIGHT_MAX` lights to avoid buffer overflow if more lights are present.
    /// Uses the SSBO dedicated to `frame_index` to prevent data races across frames in flight.
    ///
    /// The buffer is `HOST_VISIBLE | HOST_COHERENT`, so this is a plain map +
    /// copy + unmap. Safe to call before the SSBO is first bound (the descriptor
    /// points at the same buffer regardless of its contents).
    /// How many bone matrices one frame can address (#560 P0-3).
    ///
    /// 4096 mat4 = 256 KiB per frame in flight; the engine packs palettes of
    /// every skinned draw into this window and refuses to draw beyond it.
    pub const BONE_CAPACITY: usize = 4096;

    /// How many previous-model matrices one frame can address (#883 Phase 2).
    pub const PREV_MODEL_CAPACITY: usize = 4096;

    /// Upload this frame's bone palette (skinning matrices, column-major).
    ///
    /// `matrices` longer than [`Self::BONE_CAPACITY`] is truncated: the draw
    /// loop uses the same clamp when it assigns `bone_offset`.
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
        let mem = self.bone_memories[index];
        if mem == vk::DeviceMemory::null() {
            return Ok(());
        }
        let count = matrices.len().min(Self::BONE_CAPACITY);
        let matrix_size = std::mem::size_of::<[[f32; 4]; 4]>();
        let total_bytes = (Self::BONE_CAPACITY * matrix_size) as vk::DeviceSize;
        let ptr = unsafe { device.map_memory(mem, 0, total_bytes, vk::MemoryMapFlags::empty()) }
            .context("ForwardPass::update_bones: map")?;
        unsafe {
            let dst = ptr as *mut u8;
            // Clear the tail so a shader that indexes past the live range
            // reads identity-ish zeros instead of last frame's bones.
            std::ptr::write_bytes(
                dst.add(count * matrix_size),
                0,
                (Self::BONE_CAPACITY - count) * matrix_size,
            );
            if count > 0 {
                std::ptr::copy_nonoverlapping(
                    matrices.as_ptr() as *const u8,
                    dst,
                    count * matrix_size,
                );
            }
            device.unmap_memory(mem);
        }
        Ok(())
    }

    /// Upload the previous-frame model matrix for each draw-list item
    /// (#883 Phase 2). The shader indexes this by the draw-list position
    /// pushed in `_padding.y`; a missing entry falls back to the current
    /// model on the CPU side (camera-only motion).
    pub fn update_prev_models(
        &mut self,
        device: &ash::Device,
        frame_index: u32,
        models: &[[[f32; 4]; 4]],
    ) -> Result<()> {
        if self.prev_model_memories.is_empty() {
            return Ok(());
        }
        let index = (frame_index as usize) % self.prev_model_memories.len();
        let mem = self.prev_model_memories[index];
        if mem == vk::DeviceMemory::null() {
            return Ok(());
        }
        let count = models.len().min(Self::PREV_MODEL_CAPACITY);
        let matrix_size = std::mem::size_of::<[[f32; 4]; 4]>();
        let total_bytes = (Self::PREV_MODEL_CAPACITY * matrix_size) as vk::DeviceSize;
        let ptr = unsafe { device.map_memory(mem, 0, total_bytes, vk::MemoryMapFlags::empty()) }
            .context("ForwardPass::update_prev_models: map")?;
        unsafe {
            let dst = ptr as *mut u8;
            std::ptr::write_bytes(
                dst.add(count * matrix_size),
                0,
                (Self::PREV_MODEL_CAPACITY - count) * matrix_size,
            );
            if count > 0 {
                std::ptr::copy_nonoverlapping(
                    models.as_ptr() as *const u8,
                    dst,
                    count * matrix_size,
                );
            }
            device.unmap_memory(mem);
        }
        Ok(())
    }

    pub fn update_lights(
        &mut self,
        device: &ash::Device,
        frame_index: u32,
        lights: &[crate::descriptor::GpuLight],
    ) -> Result<()> {
        if self.light_memories.is_empty() {
            // SSBO not allocated yet (set_resources not called). Nothing to do;
            // the first frame after set_resources will see zeros.
            return Ok(());
        }
        let mem = self.light_memories[(frame_index as usize) % self.light_memories.len()];
        if mem == vk::DeviceMemory::null() {
            return Ok(());
        }
        let total_bytes =
            (crate::descriptor::LIGHT_MAX as usize) * crate::descriptor::GPU_LIGHT_SIZE;
        let ptr = unsafe {
            device.map_memory(
                mem,
                0,
                total_bytes as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )
        }
        .context("ForwardPass::update_lights: map")?;
        // Zero the whole buffer then copy in the active lights up to LIGHT_MAX.
        // Cheaper than tracking which slots changed, and keeps unused slots well-defined.
        let count = Self::clamped_light_count(lights.len());
        let copy_bytes = count * crate::descriptor::GPU_LIGHT_SIZE;
        unsafe {
            std::ptr::write_bytes(ptr as *mut u8, 0, total_bytes);
            if copy_bytes > 0 {
                std::ptr::copy_nonoverlapping(
                    lights.as_ptr() as *const u8,
                    ptr as *mut u8,
                    copy_bytes,
                );
            }
        }
        unsafe { device.unmap_memory(mem) };
        Ok(())
    }
    fn ensure_render_pass(&mut self, context: &crate::context::VulkanContext) -> Result<()> {
        if self.render_pass.is_some() {
            return Ok(());
        }
        let device = &context.device;
        self.device = Some(device.clone());

        // 附件 0: 交换链 颜色 高动态范围 lit 颜色 post-tonemap).
        let color_attachment = vk::AttachmentDescription::default()
            .format(self.color_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            // #461: every camera after the first must preserve what came
            // before, so the load op is LOAD and the pass clears explicitly
            // with `vkCmdClearAttachments` according to `Camera.clearFlags`.
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            // Leave the 交换链 图像 in COLOR_ATTACHMENT_OPTIMAL so a
            // subsequent external overlay pass can 加载 it and 过渡 it to
            // PRESENT_SRC_KHR. When the external overlay is 禁用 the 调用者
            // (GraphRenderer::render) records a 回退 管线 屏障 to
            // PRESENT_SRC_KHR after this pass ends.
            .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

        let color_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

        // 附件 1: scene 深度 (D32_SFLOAT). 存储 because the GTAO pass
        // samples it after ForwardPass (it was DONT_CARE before GTAO existed).
        // Final 布局 is DEPTH_STENCIL_ATTACHMENT_OPTIMAL; the GTAO pass
        // transitions it to DEPTH_STENCIL_READ_ONLY_OPTIMAL before sampling.
        let depth_attachment = vk::AttachmentDescription::default()
            .format(vk::Format::D32_SFLOAT)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);

        let depth_ref = vk::AttachmentReference::default()
            .attachment(1)
            .layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);

        // 附件 2: view-space 法线 MRT (R16G16B16A16_SFLOAT). 存储 so
        // the GTAO pass can 样本 it. Final COLOR_ATTACHMENT_OPTIMAL; the GTAO
        // pass transitions it to SHADER_READ_ONLY_OPTIMAL before sampling.
        let normal_attachment = vk::AttachmentDescription::default()
            .format(self.normal_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

        let normal_ref = vk::AttachmentReference::default()
            .attachment(2)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

        // 附件 3: screen-space motion vectors (#883 Phase 2), RGBA16F (.xy used). Cleared
        // to zero; every mesh draw overwrites its footprint. Stored so TaaPass
        // can sample it after the pass.
        let velocity_attachment = vk::AttachmentDescription::default()
            .format(self.velocity_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

        let velocity_ref = vk::AttachmentReference::default()
            .attachment(3)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

        let color_refs = [color_ref, normal_ref, velocity_ref];
        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_refs)
            .depth_stencil_attachment(&depth_ref);

        let dependency = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS,
            )
            .dst_stage_mask(
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS,
            )
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                    | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            );

        let attachments = [
            color_attachment,
            depth_attachment,
            normal_attachment,
            velocity_attachment,
        ];
        let rp_create_info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(std::slice::from_ref(&subpass))
            .dependencies(std::slice::from_ref(&dependency));

        let rp = unsafe { device.create_render_pass(&rp_create_info, None) }
            .context("ForwardPass: create render pass")?;
        self.render_pass = Some(rp);

        // Lazily 构建 the world-space gizmo 管线 against this 渲染 pass
        // (the gizmo draws inside the same 渲染 pass on 顶部 of the scene).
        if self.gizmo.is_none() {
            self.gizmo = Some(Gizmo::new(context, rp).context("ForwardPass: create gizmo")?);
        }
        if self.ground_grid.is_none() {
            self.ground_grid =
                Some(GroundGrid::new(context, rp).context("ForwardPass: create ground grid")?);
        }
        Ok(())
    }
    /// 为绘制列表里出现的每种 [`DrawState`] 各建一条管线（幂等）。
    ///
    /// 剔除 / 深度写 / 混合都是 Vulkan **管线状态**，所以按状态分管线；所有
    /// 变体的描述符布局与推送常量完全一致。调用方在录制命令前把本帧需要的
    /// 状态收集齐（见 `execute`），避免在命令缓冲录制中间创建管线。
    fn ensure_pipelines(
        &mut self,
        device: &ash::Device,
        states: &[DrawState],
        wireframe: bool,
    ) -> Result<()> {
        let missing: Vec<DrawState> = states
            .iter()
            .copied()
            .filter(|state| {
                !self.pipelines.contains_key(&(*state, wireframe, false))
                    || !self.pipelines.contains_key(&(*state, wireframe, true))
            })
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let rp = self
            .render_pass
            .context("ForwardPass: render_pass not created before pipeline")?;

        // 顶点阶段复用 mesh_vert.vert.spv（`MeshPush { model }`，64 字节）；管线
        // 按 `shader_bindings::scene_frag::PbrBindlessPush`（120 字节）推送同一个
        // push-constant 块，顶点阶段只读最前面的 model，Vulkan 允许推送范围大于
        // 某个阶段实际读取的字节数。
        // 片元阶段 scene_frag.frag.spv（bindless PBR + shadow）。
        const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/mesh_vert.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/scene_frag.frag.spv");
        let vert_module =
            shader::load_shader_module(device, VERT_SPV).context("ForwardPass: load vert")?;
        let frag_module =
            shader::load_shader_module(device, FRAG_SPV).context("ForwardPass: load frag")?;

        let vert_entry = std::ffi::CString::new(mesh_vert::ENTRY_VERTEX_MAIN).unwrap();
        let frag_entry = std::ffi::CString::new(scene_frag::ENTRY_FRAGMENT_MAIN).unwrap();

        // Skinned variant: same module family, `vertexSkinnedMain` declares the
        // extra joints/weights inputs from vertex buffer 1 (#560 P0-3). The
        // reflection JSON only carries the first entry point, so the skinned
        // entry name stays a literal here.
        const VERT_SKINNED_SPV: &[u8] =
            include_bytes!("../../../assets/shaders/mesh_vert_skinned.vert.spv");
        let vert_skinned_module = shader::load_shader_module(device, VERT_SKINNED_SPV)
            .context("ForwardPass: load skinned vert")?;
        let skin_entry = std::ffi::CString::new("vertexSkinnedMain").unwrap();
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
        let attr_descs = Vertex::attribute_descriptions();
        let skin_binding_desc = crate::mesh::SkinVertex::binding_description();
        let skin_attr_descs = crate::mesh::SkinVertex::attribute_descriptions();

        // 集合 0: 帧 UBO 绑定 0) + materials SSBO 绑定 1).
        let set0_layout = self
            .frame_set_layout
            .context("ForwardPass: set0 (frame+materials) layout not set (call set_resources)")?;
        // 集合 1: bindless 纹理 表 (samplers + SRV 数组
        let set1_layout = self.bindless_layout;
        // 集合 2: IBL resources (3 combined 图像 samplers: env, irradiance, prefiltered).
        // Use the IBL's own 描述符 集合 布局 集合 via `set_resources`) so the
        // 阶段 flags 匹配 exactly 片元 | 计算 — the path-tracing pass
        // samples the same IBL cubemap in a 计算 着色器 Creating a separate
        // 布局 with mismatched 阶段 flags would 触发器 VUID-vkCmdBindDescriptorSets-
        // pDescriptorSets-00358.
        let set2_layout = self.ibl_layout;
        // 集合 3: shadow 映射表 (SAMPLED_IMAGE + 采样器
        let set3_layout = self
            .shadow_ds_layout
            .context("ForwardPass: shadow ds layout not set")?;
        // 集合 4: previous-frame GTAO R8 可见性 (combined 图像 采样器
        let set4_layout = self
            .ao_ds_layout
            .context("ForwardPass: set4 (AO) layout not set (call set_resources)")?;
        // 集合 5: probe 音量 全局光照 (SAMPLED_IMAGE + UBO), borrowed from SceneScope.
        let set5_layout = self.gi_layout;

        let set_layouts = [
            set0_layout,
            set1_layout,
            set2_layout,
            set3_layout,
            set4_layout,
            set5_layout,
        ];

        // Push constants: PbrBindlessPush (116 字节 std140, VERTEX|FRAGMENT).
        // The reflected size is also what `cmd_push_constants` sends, and the
        // module-level assertion guarantees the Rust mirror matches it.
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(scene_frag::PUSH_CONSTANT_SIZE)];

        for state in missing {
            // MRT: 附件 0 = 颜色（按 `DrawState` 混合），附件 1 = 视图法线（不混合）。
            let blend_attachments = color_blend_attachments(state);
            for skinned in [false, true] {
                let (variant_stages, variant_bindings, variant_attrs) = if skinned {
                    let stages = [
                        shader::shader_stage(
                            vk::ShaderStageFlags::VERTEX,
                            vert_skinned_module,
                            skin_entry.as_c_str(),
                        ),
                        shader::shader_stage(
                            vk::ShaderStageFlags::FRAGMENT,
                            frag_module,
                            frag_entry.as_c_str(),
                        ),
                    ];
                    (stages, [binding_desc, skin_binding_desc], [0; 2])
                } else {
                    (shader_stages, [binding_desc, skin_binding_desc], [0; 2])
                };
                let _ = variant_attrs;
                let binding_descs: &[vk::VertexInputBindingDescription] = if skinned {
                    &variant_bindings
                } else {
                    std::slice::from_ref(&variant_bindings[0])
                };
                let attr_descriptor_list: &[vk::VertexInputAttributeDescription] = if skinned {
                    &[
                        attr_descs[0],
                        attr_descs[1],
                        attr_descs[2],
                        attr_descs[3],
                        attr_descs[4],
                        skin_attr_descs[0],
                        skin_attr_descs[1],
                    ]
                } else {
                    &attr_descs
                };
                let pipeline = GraphicsPipeline::new(&PipelineDesc {
                device,
                shader_stages: &variant_stages,
                vertex_binding_desc: binding_descs,
                vertex_attr_descs: attr_descriptor_list,
                descriptor_set_layouts: &set_layouts,
                push_constant_ranges: &push,
                render_pass: rp,
                subpass: 0,
                cull_mode: Some(cull_flags(state.cull_mode)),
                depth_bias_enable: None,
                depth_bias_constant_factor: None,
                depth_bias_slope_factor: None,
                // 透明物体通常 `ZWrite Off`（`_ZWrite`）。
                depth_write_enable: Some(state.z_write),
                depth_test_enable: None,
                color_attachment_count: None,
                color_blend_attachments: Some(&blend_attachments),
                front_face: None,
                polygon_mode: Some(if wireframe {
                    vk::PolygonMode::LINE
                } else {
                    vk::PolygonMode::FILL
                }),
            })
            .with_context(|| {
                format!(
                    "ForwardPass: create pipeline ({state:?}, wireframe={wireframe}, skinned={skinned})"
                )
            })?;
                self.pipelines.insert((state, wireframe, skinned), pipeline);
            }
        }

        unsafe { device.destroy_shader_module(vert_module, None) };
        unsafe { device.destroy_shader_module(vert_skinned_module, None) };
        unsafe { device.destroy_shader_module(frag_module, None) };
        // set2_layout (IBL) is borrowed from `IblResources` 集合 via
        // `set_resources`) — do NOT 销毁 it here. `IblResources` owns the
        // 布局 生命周期 it outlives all passes.
        // set0/set1/set3 are owned elsewhere (frame_set_layout /
        // BindlessTextureTable / shadow_ds_layout).

        Ok(())
    }

    /// 惰性创建选中描边管线（#487）。
    ///
    /// 反向外壳只绑定 set 0（帧 UBO），所以管线布局比场景管线窄；剔除正面、
    /// 关闭深度写入，让扩大的背面只在原网格轮廓外通过深度测试。附件 1（视图
    /// 法线）不写，避免污染 GTAO 的输入。
    fn ensure_outline_pipeline(&mut self, device: &ash::Device) -> Result<()> {
        if self.outline_pipeline.is_some() {
            return Ok(());
        }
        let rp = self
            .render_pass
            .context("ForwardPass: render_pass not created before outline pipeline")?;
        let frame_set_layout = self
            .frame_set_layout
            .context("ForwardPass: set0 layout not set (call set_resources)")?;

        const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/outline.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/outline.frag.spv");
        let vert_module =
            shader::load_shader_module(device, VERT_SPV).context("outline: load vert")?;
        let frag_module =
            shader::load_shader_module(device, FRAG_SPV).context("outline: load frag")?;
        let vert_entry = std::ffi::CString::new("vertexMain").unwrap();
        let frag_entry = std::ffi::CString::new("fragmentMain").unwrap();
        let shader_stages = [
            shader::shader_stage(
                vk::ShaderStageFlags::VERTEX,
                vert_module,
                vert_entry.as_c_str(),
            ),
            shader::shader_stage(
                vk::ShaderStageFlags::FRAGMENT,
                frag_module,
                frag_entry.as_c_str(),
            ),
        ];
        let binding_desc = Vertex::binding_description();
        let attr_descs = Vertex::attribute_descriptions();
        let set_layouts = [frame_set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<OutlinePush>() as u32)];
        // 附件 0 写纯色、不混合；附件 1（视图法线）不写。
        let blend_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)
                .blend_enable(false),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty())
                .blend_enable(false),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty())
                .blend_enable(false),
        ];

        let pipeline = GraphicsPipeline::new(&PipelineDesc {
            device,
            shader_stages: &shader_stages,
            vertex_binding_desc: std::slice::from_ref(&binding_desc),
            vertex_attr_descs: &attr_descs,
            descriptor_set_layouts: &set_layouts,
            push_constant_ranges: &push,
            render_pass: rp,
            subpass: 0,
            // 正面剔除：只留下被法线外扩的背面，形成轮廓。
            cull_mode: Some(vk::CullModeFlags::FRONT),
            depth_bias_enable: None,
            depth_bias_constant_factor: None,
            depth_bias_slope_factor: None,
            depth_write_enable: Some(false),
            depth_test_enable: None,
            color_attachment_count: None,
            color_blend_attachments: Some(&blend_attachments),
            front_face: None,
            polygon_mode: Some(vk::PolygonMode::FILL),
        })
        .context("ForwardPass: create outline pipeline")?;

        unsafe { device.destroy_shader_module(vert_module, None) };
        unsafe { device.destroy_shader_module(frag_module, None) };
        self.outline_pipeline = Some(pipeline);
        Ok(())
    }

    /// 惰性创建「填充 + 线框叠加」管线（#431）。
    ///
    /// 复用 outline 着色器：顶点只做 `model` 变换（厚度 0，不外扩），片元输出
    /// 深色线。`POLYGON_MODE_LINE` + 负深度 bias 让线压在填充面上，避免
    /// z-fighting；不写深度，所以线不会污染后续的深度测试 / 选中描边。
    fn ensure_shaded_wireframe_pipeline(&mut self, device: &ash::Device) -> Result<()> {
        if self.shaded_wireframe_pipeline.is_some() {
            return Ok(());
        }
        let rp = self
            .render_pass
            .context("ForwardPass: render_pass not created before shaded wireframe")?;
        const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/outline.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/outline.frag.spv");
        let vert_module =
            shader::load_shader_module(device, VERT_SPV).context("wireframe overlay: load vert")?;
        let frag_module =
            shader::load_shader_module(device, FRAG_SPV).context("wireframe overlay: load frag")?;

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
        let binding_desc = Vertex::binding_description();
        let attr_descs = Vertex::attribute_descriptions();
        let frame_set_layout = self
            .frame_set_layout
            .context("ForwardPass: set0 layout not set (call set_resources)")?;
        let set_layouts = [frame_set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<OutlinePush>() as u32)];
        let blend_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)
                .blend_enable(false),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty())
                .blend_enable(false),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty())
                .blend_enable(false),
        ];
        let pipeline = GraphicsPipeline::new(&PipelineDesc {
            device,
            shader_stages: &shader_stages,
            vertex_binding_desc: std::slice::from_ref(&binding_desc),
            vertex_attr_descs: &attr_descs,
            descriptor_set_layouts: &set_layouts,
            push_constant_ranges: &push,
            render_pass: rp,
            subpass: 0,
            cull_mode: Some(vk::CullModeFlags::BACK),
            depth_bias_enable: Some(true),
            // 负 bias 把线拉向相机；线框只覆盖填充面的边缘，不写深度。
            depth_bias_constant_factor: Some(-2.0),
            depth_bias_slope_factor: Some(-1.0),
            depth_write_enable: Some(false),
            depth_test_enable: None,
            color_attachment_count: None,
            color_blend_attachments: Some(&blend_attachments),
            front_face: None,
            polygon_mode: Some(vk::PolygonMode::LINE),
        })
        .context("ForwardPass: create shaded wireframe pipeline")?;

        unsafe { device.destroy_shader_module(vert_module, None) };
        unsafe { device.destroy_shader_module(frag_module, None) };
        self.shaded_wireframe_pipeline = Some(pipeline);
        Ok(())
    }
}
impl RenderPassNode for ForwardPass {
    fn name(&self) -> &str {
        "ForwardPass"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // Declare 输出 handles (well-known, so downstream passes 读取 our
        // 深度 / 法线 / 高动态范围 views by handle). The 图 does NOT allocate
        // the underlying images in PR-1 (ForwardPass still owns its
        // framebuffers); only the handle->view 映射 is published in
        // 执行
        graph.create_resource_at(
            FORWARD_DEPTH_H,
            ResourceType::DepthAttachment {
                extent: vk::Extent2D {
                    width: 1,
                    height: 1,
                },
                sample_count: vk::SampleCountFlags::TYPE_1,
            },
        );
        graph.create_resource_at(
            FORWARD_NORMAL_H,
            ResourceType::ColorAttachment {
                format: self.normal_format,
                extent: vk::Extent2D {
                    width: 1,
                    height: 1,
                },
                sample_count: vk::SampleCountFlags::TYPE_1,
            },
        );
        graph.create_resource_at(
            FORWARD_COLOR_H,
            ResourceType::ColorAttachment {
                format: self.color_format,
                extent: vk::Extent2D {
                    width: 1,
                    height: 1,
                },
                sample_count: vk::SampleCountFlags::TYPE_1,
            },
        );
        graph.create_resource_at(
            FORWARD_VELOCITY_H,
            ResourceType::ColorAttachment {
                format: self.velocity_format,
                extent: vk::Extent2D {
                    width: 1,
                    height: 1,
                },
                sample_count: vk::SampleCountFlags::TYPE_1,
            },
        );
        self.out_depth_h = FORWARD_DEPTH_H;
        self.out_normal_h = FORWARD_NORMAL_H;
        self.out_color_h = FORWARD_COLOR_H;
        self.out_velocity_h = FORWARD_VELOCITY_H;

        // Declare read edge for directional shadow map sampled in fragment shader (set 3).
        // `shadows` 关闭时图里没有 ShadowMapPass，这个 handle 无人写入，
        // 也就不该声明读边（forward 采样的是一张不在图里的 1×1 dummy）。
        #[cfg(feature = "shadows")]
        graph.read_usage(ResourceUsage {
            handle: SHADOW_MAP_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        });

        // Declare 写入 edges so the 渲染 graph's 布局 cache records the
        // 布局 this pass leaves each 附件 in (matching the 渲染 pass
        // `final_layout`). Downstream `GtaoPass` / `PostPass` 读取 edges then
        // 触发器 the 附件 -> READ_ONLY / SHADER_READ_ONLY barriers
        // automatically, with `src` stage/access taken from these 写入 edges.
        // No 屏障 is emitted for the writes themselves: ForwardPass's 渲染
        // pass performs the UNDEFINED -> 附件 transitions via
        // `initial_layout` (see `create_render_pass`).
        graph.write_usage(ResourceUsage {
            handle: FORWARD_DEPTH_H,
            access: vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            stage: vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
            layout: vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
        });
        graph.write_usage(ResourceUsage {
            handle: FORWARD_NORMAL_H,
            access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        });
        graph.write_usage(ResourceUsage {
            handle: FORWARD_COLOR_H,
            access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        });
        graph.write_usage(ResourceUsage {
            handle: FORWARD_VELOCITY_H,
            access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        });
    }

    fn execute(&mut self, ctx: &RenderContext, resources: &mut GraphResources) -> Result<()> {
        // 帧缓冲 + HDR/depth/normal lifecycle now owned here (driven by
        // the 图 not by `GraphRenderer::render`. `ensure_target` is
        // idempotent: (re)builds only the 槽 for `image_index` when 缺少
        // or the 交换链 changed. `set_ao` rebinds the previous-frame GTAO
        // 可见性 视图 (1-frame 延迟 `update_lights` rewrites the
        // point-light SSBO from the ECS-collected lights for this 帧
        // #613：内部渲染分辨率开启时目标小于交换链。
        self.ensure_target(ctx.device, ctx.context, ctx.image_index, ctx.render_extent)?;
        self.set_ao(ctx.device, ctx.frame_index, ctx.frame.ao_view);
        self.update_lights(ctx.device, ctx.frame_index, ctx.frame.lights)?;

        self.ensure_render_pass(ctx.context)?;
        // 录制命令前把本帧需要的管线状态收集齐（不透明默认状态永远需要：
        // 描述符集用它的 layout 绑定）。
        // 设备不支持 `fillModeNonSolid` 时静默退回填充，不影响正常渲染。
        let wireframe = ctx.frame.wireframe && ctx.context.wireframe_supported;
        let shaded_wireframe =
            !wireframe && ctx.frame.shaded_wireframe && ctx.context.wireframe_supported;
        let mut states: Vec<DrawState> = vec![DrawState::default()];
        let mut seen = std::collections::HashSet::new();
        seen.insert(DrawState::default());
        for item in ctx.frame.draw_list {
            if item.visible_in_main_pass && seen.insert(item.state) {
                states.push(item.state);
            }
        }
        self.ensure_pipelines(ctx.device, &states, wireframe)?;
        if shaded_wireframe {
            self.ensure_shaded_wireframe_pipeline(ctx.device)?;
        }

        let rp = self.render_pass.unwrap();
        // Pick the per-swapchain-image 帧缓冲 Indexed by `image_index`
        // (NOT `frame_index`): with N 交换链 images and 2 frames in flight,
        // several 命令 buffers 引用 different framebuffers
        // concurrently, so each 交换链 图像 has its own.
        let idx = ctx.image_index as usize;
        let fb = self
            .framebuffers
            .get(idx)
            .copied()
            .flatten()
            .context("ForwardPass: no framebuffer for image_index (call set_target first)")?;
        // 描述符 / 推送常量布局在所有状态变体之间完全相同，所以用默认
        // （不透明、背面剔除）变体的 layout 绑定一次即可；循环里只按需重绑管线。
        // Copy the Vulkan handles out: `pipeline` borrows `self.pipelines`,
        // and the per-camera loop below may lazily create the outline pipeline
        // (`&mut self`). Handles are `Copy`, the borrow is not.
        let (scene_pipeline, scene_layout) = {
            let pipeline = self
                .pipelines
                .get(&(DrawState::default(), wireframe, false))
                .context("ForwardPass: default pipeline missing")?;
            (pipeline.pipeline, pipeline.layout)
        };

        // 解析 the per-frame 描述符 集合 now (used after the skybox 绘制
        // when we re-bind the scene 管线 + descriptors).
        let frame_set = self
            .frame_sets
            .get(ctx.frame_index as usize)
            .copied()
            .context("ForwardPass: no set0 descriptor set for frame_index (call set_resources)")?;

        // 清空 values indexed by 附件 number: 0 = 高动态范围 颜色 1 = 深度
        // 2 = view-space 法线 MRT. Even though 附件 2 is cleared, its
        // 清空 value is irrelevant (the 片元 着色器 overwrites every
        // 像素 use 不透明 black. The count must be >= the highest cleared
        // 附件 索引 + 1 (VUID-VkRenderPassBeginInfo-clearValueCount-00902).
        // 颜色 附件 0 uses the frame's clear_color (from the app-level
        // `clear_color` 参数 so the "no 相机 回退 shows gray etc.
        let cc = ctx.frame.clear_color;
        let clear_values = [
            vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [cc[0], cc[1], cc[2], cc[3]],
                },
            },
            vk::ClearValue {
                depth_stencil: vk::ClearDepthStencilValue {
                    depth: 1.0,
                    stencil: 0,
                },
            },
            vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 0.0],
                },
            },
            // Attachment 3: motion vectors. Zero means "no motion".
            vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 0.0],
                },
            },
        ];

        let begin_info = vk::RenderPassBeginInfo::default()
            .render_pass(rp)
            .framebuffer(fb)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.extent,
            })
            .clear_values(&clear_values);

        unsafe {
            ctx.device
                .cmd_begin_render_pass(ctx.cmd, &begin_info, vk::SubpassContents::INLINE)
        };

        // #461: composite every enabled camera, in `Camera.depth` order.
        // The legacy path (`cameras` empty) synthesises one camera from the
        // frame UBO so existing callers keep the exact old frame.
        let cameras: Vec<crate::render_graph::CameraView> = if ctx.frame.cameras.is_empty() {
            vec![crate::render_graph::CameraView::legacy(
                ctx.frame.view_proj,
                ctx.frame.inv_view_rot,
                ctx.frame.clear_color,
                ctx.frame.has_camera,
            )]
        } else {
            ctx.frame
                .cameras
                .iter()
                .take(crate::descriptor::MAX_CAMERAS)
                .copied()
                .collect()
        };

        // 统计真正提交的绘制数（`ShadowsOnly` 会被跳过）。多相机时累加。
        let mut drawn = 0usize;
        for (camera_index, camera) in cameras.iter().enumerate() {
            // Clear according to `Camera.clearFlags`: `Skybox` / `SolidColor`
            // clear colour (and the view-normal MRT), everything except
            // `Nothing` also clears depth. Camera 0 always ends up with a
            // defined image because the default flags clear both.
            let mut clears: Vec<vk::ClearAttachment> = Vec::new();
            if camera.clear_color_enabled {
                clears.push(vk::ClearAttachment {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    color_attachment: 0,
                    clear_value: vk::ClearValue {
                        color: vk::ClearColorValue {
                            float32: camera.clear_color,
                        },
                    },
                });
                clears.push(vk::ClearAttachment {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    color_attachment: 2,
                    clear_value: vk::ClearValue {
                        color: vk::ClearColorValue {
                            float32: [0.0, 0.0, 0.0, 0.0],
                        },
                    },
                });
            }
            if camera.clear_depth {
                clears.push(vk::ClearAttachment {
                    aspect_mask: vk::ImageAspectFlags::DEPTH,
                    color_attachment: 1,
                    clear_value: vk::ClearValue {
                        depth_stencil: vk::ClearDepthStencilValue {
                            depth: 1.0,
                            stencil: 0,
                        },
                    },
                });
            }
            if !clears.is_empty() {
                let rect = vk::ClearRect {
                    rect: vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: self.extent,
                    },
                    base_array_layer: 0,
                    layer_count: 1,
                };
                unsafe {
                    ctx.device
                        .cmd_clear_attachments(ctx.cmd, &clears, std::slice::from_ref(&rect));
                }
            }

            // 绘制 the skybox 第一个 (background) for cameras whose clear
            // flags ask for it. It uses its own 管线 + IBL env set and writes
            // no depth, so scene geometry always occludes it.
            if camera.draw_skybox && camera.has_camera {
                if let Some(skybox) = ctx.frame.skybox {
                    if let Err(e) = self.skybox.draw(
                        ctx.device,
                        ctx.cmd,
                        self.render_pass.unwrap(),
                        self.extent,
                        &camera.inv_view,
                        skybox,
                    ) {
                        log::warn!("SkyboxPass draw failed (skybox skipped): {e:#}");
                    }
                }
            }

            // Re-bind the scene 管线 + all 描述符 sets. Required after the
            // skybox draw (incompatible layout at set 0) and per camera because
            // set 0 carries a camera-slot dynamic offset (#461).
            let dynamic_offset = camera_index as u32 * self.camera_stride;
            unsafe {
                ctx.device.cmd_bind_pipeline(
                    ctx.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    scene_pipeline,
                );
                // 集合 0: 帧 UBO + materials SSBO + 光源 SSBO
                ctx.device.cmd_bind_descriptor_sets(
                    ctx.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    scene_layout,
                    0,
                    std::slice::from_ref(&frame_set),
                    std::slice::from_ref(&dynamic_offset),
                );
                // 集合 1: bindless 纹理 表
                ctx.device.cmd_bind_descriptor_sets(
                    ctx.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    scene_layout,
                    1,
                    std::slice::from_ref(&self.bindless_set),
                    &[],
                );
                // 集合 2: IBL cubemap
                ctx.device.cmd_bind_descriptor_sets(
                    ctx.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    scene_layout,
                    2,
                    std::slice::from_ref(&self.ibl_descriptor_set),
                    &[],
                );
                // 集合 3: shadow 映射表
                ctx.device.cmd_bind_descriptor_sets(
                    ctx.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    scene_layout,
                    3,
                    std::slice::from_ref(&self.shadow_descriptor_set),
                    &[],
                );
                // 集合 4: previous-frame GTAO 可见性 (combined 图像 采样器).
                let ao_set = self
                    .ao_descriptor_sets
                    .get(ctx.frame_index as usize)
                    .copied()
                    .unwrap_or(vk::DescriptorSet::null());
                ctx.device.cmd_bind_descriptor_sets(
                    ctx.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    scene_layout,
                    4,
                    std::slice::from_ref(&ao_set),
                    &[],
                );
                // 集合 5: probe 音量 全局光照 (scene-level, 静态).
                ctx.device.cmd_bind_descriptor_sets(
                    ctx.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    scene_layout,
                    5,
                    std::slice::from_ref(&self.gi_descriptor_set),
                    &[],
                );
            }

            unsafe {
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

                // 循环开始前刚绑定的是默认（不透明）管线；只有遇到别的状态才重绑
                // （剔除 / 深度写 / 混合都是管线状态）。描述符与推送常量布局在所有
                // 变体之间一致，所以不需要重新绑定描述符集。
                // #491: pack static instance matrices after the live bone
                // palette in the same SSBO. Skinned draws keep their existing
                // offsets; instanced draws start at `bone_count`.
                let bone_count = ctx
                    .frame
                    .bone_matrices
                    .len()
                    .min(ForwardPass::BONE_CAPACITY);
                let capacity = ForwardPass::BONE_CAPACITY.saturating_sub(bone_count);
                let (batches, instance_matrices) =
                    prepare_static_batches(ctx.frame.draw_list, ctx.frame.mesh_manager, capacity);
                if !instance_matrices.is_empty() {
                    let mut packed = Vec::with_capacity(bone_count + instance_matrices.len());
                    packed.extend_from_slice(&ctx.frame.bone_matrices[..bone_count]);
                    packed.extend_from_slice(&instance_matrices);
                    self.update_bones(ctx.device, ctx.frame_index, &packed)?;
                }

                let mut bound_state = (DrawState::default(), wireframe, false);
                for batch in &batches {
                    let item = &ctx.frame.draw_list[batch.items[0]];
                    let uploaded_mesh = match ctx.frame.mesh_manager.get(item.mesh) {
                        Some(m) => m,
                        None => continue,
                    };
                    let uploaded = &uploaded_mesh.mesh;
                    let skinned = item.bone_offset.is_some();
                    let skin = if skinned {
                        uploaded_mesh.skin.as_ref()
                    } else {
                        None
                    };
                    if skinned && skin.is_none() {
                        continue;
                    }
                    let key = (item.state, wireframe, skinned);
                    if key != bound_state {
                        let variant = self.pipelines.get(&key).with_context(|| {
                            format!("ForwardPass: no pipeline for {:?}", item.state)
                        })?;
                        ctx.device.cmd_bind_pipeline(
                            ctx.cmd,
                            vk::PipelineBindPoint::GRAPHICS,
                            variant.pipeline,
                        );
                        bound_state = key;
                    }
                    drawn += 1;

                    if let Some(skin) = skin {
                        let vertex_buffers = [uploaded.vertex_buffer, skin.buffer];
                        let offsets = [0u64, 0u64];
                        ctx.device
                            .cmd_bind_vertex_buffers(ctx.cmd, 0, &vertex_buffers, &offsets);
                    } else {
                        let vertex_buffers = [uploaded.vertex_buffer];
                        let offsets = [0u64];
                        ctx.device
                            .cmd_bind_vertex_buffers(ctx.cmd, 0, &vertex_buffers, &offsets);
                    }

                    // #491: multi-item batches read their model matrices from
                    // the bone SSBO; count 0 keeps the old push-model path.
                    let instance_count = batch
                        .instance_offset
                        .map(|_| batch.items.len() as u32)
                        .unwrap_or(0);
                    let instance_offset = batch
                        .instance_offset
                        .map(|offset| bone_count as u32 + offset)
                        .unwrap_or(0);
                    let pc = crate::shader_bindings::scene_frag::PbrBindlessPush {
                        model: item.model,
                        material_slot: item.material.unwrap_or(0),
                        env_handle: self.brdf_handle,
                        debug_flags: ctx.frame.debug_flags,
                        bone_offset: item.bone_offset.unwrap_or(0),
                        skin_mode: u32::from(item.bone_offset.is_some()),
                        instance_offset,
                        instance_count,
                        // Draw-list index for the previous-model SSBO and the
                        // TAA reactive mask (transparent draws drop history).
                        prev_draw_index: batch.items[0] as u32,
                        reactive_mask: u32::from(item.state.blends()),
                        splat_slot0: item.splat_slots[0],
                        splat_slot1: item.splat_slots[1],
                        splat_slot2: item.splat_slots[2],
                        splat_slot3: item.splat_slots[3],
                        splat_tiling: item.splat_tiling,
                    };
                    ctx.device.cmd_push_constants(
                        ctx.cmd,
                        scene_layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        std::slice::from_raw_parts(
                            &pc as *const _ as *const u8,
                            scene_frag::PUSH_CONSTANT_SIZE as usize,
                        ),
                    );

                    let draw_instances = instance_count.max(1);
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
                            draw_instances,
                            item.index_start,
                            0,
                            0,
                        );
                    } else {
                        ctx.device
                            .cmd_draw(ctx.cmd, uploaded.vertex_count, draw_instances, 0, 0);
                    }
                }
            }

            // 填充 + 线框叠加（#431）：填充已经画完，再用 outline 着色器画一遍
            // `POLYGON_MODE_LINE`；负深度 bias + 不写深度让线稳定压在面上。
            if shaded_wireframe {
                let overlay = self
                    .shaded_wireframe_pipeline
                    .as_ref()
                    .context("ForwardPass: shaded wireframe pipeline missing")?;
                unsafe {
                    ctx.device.cmd_bind_pipeline(
                        ctx.cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        overlay.pipeline,
                    );
                    ctx.device.cmd_bind_descriptor_sets(
                        ctx.cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        overlay.layout,
                        0,
                        std::slice::from_ref(&frame_set),
                        std::slice::from_ref(&dynamic_offset),
                    );
                }
                for item in ctx.frame.draw_list {
                    if !item.visible_in_main_pass {
                        continue;
                    }
                    let Some(uploaded) = ctx.frame.mesh_manager.get(item.mesh) else {
                        continue;
                    };
                    let pc = OutlinePush {
                        model: item.model,
                        // 深色线在亮面上可读；不随材质变化，接近 Unity 的
                        // Shaded Wireframe。
                        color: [0.02, 0.02, 0.02, 1.0],
                        thickness: 0.0,
                        _pad: [0.0; 3],
                    };
                    unsafe {
                        let vertex_buffers = [uploaded.mesh.vertex_buffer];
                        let offsets = [0u64];
                        ctx.device
                            .cmd_bind_vertex_buffers(ctx.cmd, 0, &vertex_buffers, &offsets);
                        ctx.device.cmd_push_constants(
                            ctx.cmd,
                            overlay.layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            std::slice::from_raw_parts(
                                &pc as *const _ as *const u8,
                                std::mem::size_of::<OutlinePush>(),
                            ),
                        );
                        if let Some(ib) = uploaded.mesh.index_buffer {
                            let index_count = item.index_count.unwrap_or(uploaded.mesh.index_count);
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
                            ctx.device
                                .cmd_draw(ctx.cmd, uploaded.mesh.vertex_count, 1, 0, 0);
                        }
                    }
                }
            }

            // 选中描边（#487）：把选中的网格再画一遍，正面剔除 + 法线外扩。
            // 放在不透明网格之后、粒子 / 精灵之前：被其它几何挡住的部分会正常
            // 被深度测试丢弃，不会像屏幕空间线框那样浮在最上面。
            let outline_requested = ctx.frame.draw_list.iter().any(|item| {
                item.visible_in_main_pass && ctx.frame.outline_entities.contains(&item.entity_id)
            });
            if outline_requested {
                self.ensure_outline_pipeline(ctx.device)?;
                let outline = self
                    .outline_pipeline
                    .as_ref()
                    .context("ForwardPass: outline pipeline missing")?;
                unsafe {
                    ctx.device.cmd_bind_pipeline(
                        ctx.cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        outline.pipeline,
                    );
                    ctx.device.cmd_bind_descriptor_sets(
                        ctx.cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        outline.layout,
                        0,
                        std::slice::from_ref(&frame_set),
                        std::slice::from_ref(&dynamic_offset),
                    );
                }

                for item in ctx.frame.draw_list {
                    if !item.visible_in_main_pass
                        || !ctx.frame.outline_entities.contains(&item.entity_id)
                    {
                        continue;
                    }
                    let Some(uploaded) = ctx.frame.mesh_manager.get(item.mesh) else {
                        continue;
                    };
                    // 线宽随物体尺度走，缩放很大的物体不会只剩一条细缝。
                    let scale = (0..3)
                        .map(|column| {
                            let c = &item.model[column];
                            (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt()
                        })
                        .fold(0.0f32, f32::max);
                    let pc = OutlinePush {
                        model: item.model,
                        color: [4.0, 0.25, 0.02, 1.0],
                        thickness: 0.02 * scale.max(1e-4),
                        _pad: [0.0; 3],
                    };
                    unsafe {
                        let vertex_buffers = [uploaded.mesh.vertex_buffer];
                        let offsets = [0u64];
                        ctx.device
                            .cmd_bind_vertex_buffers(ctx.cmd, 0, &vertex_buffers, &offsets);
                        ctx.device.cmd_push_constants(
                            ctx.cmd,
                            outline.layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            std::slice::from_raw_parts(
                                &pc as *const _ as *const u8,
                                std::mem::size_of::<OutlinePush>(),
                            ),
                        );
                        if let Some(ib) = uploaded.mesh.index_buffer {
                            let index_count = item.index_count.unwrap_or(uploaded.index_count());
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
                            ctx.device
                                .cmd_draw(ctx.cmd, uploaded.vertex_count(), 1, 0, 0);
                        }
                    }
                }
            }

            // 世界空间粒子：深度测试开启（被前面的几何遮挡），所以画在不透明网格
            // 之后、2D 精灵之前；透明排序由引擎在提交前完成（远到近）。
            if !ctx.frame.particle_items.is_empty() {
                if let Err(e) = self.particles.draw(
                    ctx.device,
                    ctx.context,
                    ctx.cmd,
                    self.render_pass.unwrap(),
                    &camera.view_proj,
                    &camera.inv_view,
                    ctx.frame.particle_items,
                ) {
                    log::warn!("ParticlePass draw failed (particles skipped): {e:#}");
                }
            }

            // 2D sprites, drawn after the mesh list so they overlay 3D geometry.
            // The pass disables depth test/write; ordering comes from the engine's
            // CPU-side sort (sorting_layer -> order_in_layer -> z).
            if !ctx.frame.sprite_items.is_empty() {
                if let Err(e) = self.sprites.draw(
                    ctx.device,
                    ctx.context,
                    ctx.cmd,
                    self.render_pass.unwrap(),
                    self.bindless_set,
                    self.bindless_layout,
                    &camera.view_proj,
                    ctx.frame.sprite_items,
                    ctx.frame.sprite_lights,
                ) {
                    log::warn!("SpritePass draw failed (sprites skipped): {e:#}");
                }
            }

            // 绘制地面参考网格（深度测试开启，被场景实体遮挡）
            if ctx.frame.show_grid {
                if let Some(grid) = &self.ground_grid {
                    grid.draw(ctx.cmd, &camera.view_proj, ctx.frame.camera_orthographic);
                }
            }

            // 绘制 the world-space XYZ gizmo on 顶部 of the scene (its 管线 has
            // 深度 test 禁用 so it is never occluded). Uses the same
            // view-projection the scene was drawn with.
            if let Some(gizmo) = &self.gizmo {
                gizmo.draw(ctx.cmd, &camera.view_proj, ctx.frame.gizmos);
            }
        }

        unsafe { ctx.device.cmd_end_render_pass(ctx.cmd) };

        // 发布 our 输出 views under the handles declared in `setup` so
        // downstream passes (`GtaoPass`, `PostPass`) 读取 them by handle
        // instead of `GraphRenderer` reaching into ForwardPass internals.
        let idx = ctx.image_index;
        if let (Some(v), Some(i)) = (self.color_view(idx), self.color_image(idx)) {
            resources.set_image_view(self.out_color_h, v);
            resources.set_image(self.out_color_h, i);
        }
        if let (Some(v), Some(i)) = (self.depth_view(idx), self.depth_image(idx)) {
            resources.set_image_view(self.out_depth_h, v);
            resources.set_image(self.out_depth_h, i);
        }
        if let (Some(v), Some(i)) = (self.normal_view(idx), self.normal_image(idx)) {
            resources.set_image_view(self.out_normal_h, v);
            resources.set_image(self.out_normal_h, i);
        }
        if let (Some(v), Some(i)) = (self.velocity_view(idx), self.velocity_image(idx)) {
            resources.set_image_view(self.out_velocity_h, v);
            resources.set_image(self.out_velocity_h, i);
        }

        log::trace!(
            "ForwardPass: rendered {} draws into {}x{} ({} in list)",
            drawn,
            self.extent.width,
            self.extent.height,
            ctx.frame.draw_list.len()
        );
        Ok(())
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Forward,
            // Shadow 视图 is declared as an input (SHADOW_MAP_H); IBL and
            // previous-frame AO are bound via `set_resources` / `set_ao` and
            // bypass `GraphResources`, so they aren't listed as graph edges.
            #[cfg(feature = "shadows")]
            inputs: vec![SHADOW_MAP_H],
            #[cfg(not(feature = "shadows"))]
            inputs: Vec::new(),
            outputs: vec![
                self.out_depth_h,
                self.out_normal_h,
                self.out_color_h,
                self.out_velocity_h,
            ],
        }
    }

    fn warmup(&mut self, device: &ash::Device, context: &VulkanContext) -> Result<()> {
        if !self.pipelines.is_empty() {
            return Ok(());
        }
        self.ensure_render_pass(context)?;
        self.ensure_pipelines(device, &[DrawState::default()], false)
    }
}

impl Drop for ForwardPass {
    fn drop(&mut self) {
        // 安全性 net: if 销毁 wasn't called explicitly, tear 下 using
        // the cached 设备 handle. 销毁 is the preferred path (it runs
        // after `device_wait_idle`); this only fires on leaks / early drops.
        if let Some(device) = self.device.take() {
            // `drop_target` drains framebuffers + 深度 images.
            for fb in self.framebuffers.drain(..).flatten() {
                unsafe { device.destroy_framebuffer(fb, None) };
            }
            for depth in self.depth_images.drain(..).flatten() {
                let mut d = depth;
                unsafe { d.destroy(&device) };
            }
            if let Some(rp) = self.render_pass.take() {
                unsafe { device.destroy_render_pass(rp, None) };
            }
            // GraphicsPipeline::Drop frees the 管线 + 布局
            self.pipelines.clear();
            // 集合 0 帧 UBO + materials SSBO) 布局 + 池
            if let Some(layout) = self.frame_set_layout.take() {
                unsafe { device.destroy_descriptor_set_layout(layout, None) };
            }
            if let Some(pool) = self.frame_set_pool.take() {
                unsafe { device.destroy_descriptor_pool(pool, None) };
            }
            if let Some(layout) = self.shadow_ds_layout.take() {
                unsafe { device.destroy_descriptor_set_layout(layout, None) };
            }
            if let Some(pool) = self.shadow_ds_pool.take() {
                unsafe { device.destroy_descriptor_pool(pool, None) };
            }
            // 光源 SSBO.
            for buf in self.light_buffers.drain(..) {
                if buf != vk::Buffer::null() {
                    unsafe { device.destroy_buffer(buf, None) };
                }
            }
            for mem in self.light_memories.drain(..) {
                if mem != vk::DeviceMemory::null() {
                    unsafe { device.free_memory(mem, None) };
                }
            }
            // Skybox pass (its own 管线 + set-2 布局
            self.skybox.destroy(&device);
            // Gizmo (its own 管线 + 顶点 缓冲区 放置 frees them).
            self.gizmo = None;
            self.ground_grid = None;
            // 集合 5 全局光照 probe 音量 is borrowed from SceneScope — not destroyed here.
        }
    }
}

/// 颜色附件的混合状态（MRT：附件 0 = 颜色，附件 1 = 视图法线）。
///
/// 附件 0 按 材质状态（`_SrcBlend` / `_DstBlend` / `_BlendOp`）配置；
/// 默认值 `One`/`Zero`/`Add` 表示不透明，此时直接关掉混合。附件 1 永远不混合
/// （视图空间法线需要原样写入）。
fn color_blend_attachments(state: DrawState) -> [vk::PipelineColorBlendAttachmentState; 3] {
    let mut color = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA);
    if state.blends() {
        color = color
            .blend_enable(true)
            .src_color_blend_factor(blend_factor(state.src_blend))
            .dst_color_blend_factor(blend_factor(state.dst_blend))
            .color_blend_op(blend_operation(state.blend_op))
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
            .alpha_blend_op(vk::BlendOp::ADD);
    }
    [
        color,
        // View-space normal + reactive mask (alpha) are never blended.
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(false),
        // Motion vectors are per-fragment and overwrite the previous value.
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::R | vk::ColorComponentFlags::G)
            .blend_enable(false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamped_light_count_clamps_to_light_max() {
        assert_eq!(ForwardPass::clamped_light_count(0), 0);
        assert_eq!(ForwardPass::clamped_light_count(1), 1);
        assert_eq!(
            ForwardPass::clamped_light_count(crate::descriptor::LIGHT_MAX as usize),
            crate::descriptor::LIGHT_MAX as usize
        );
        assert_eq!(
            ForwardPass::clamped_light_count(crate::descriptor::LIGHT_MAX as usize + 1),
            crate::descriptor::LIGHT_MAX as usize
        );
        assert_eq!(
            ForwardPass::clamped_light_count(100),
            crate::descriptor::LIGHT_MAX as usize
        );
    }

    #[test]
    fn forward_pass_initializes_empty_light_resources() {
        let pass = ForwardPass::new(vk::Format::B8G8R8A8_UNORM);
        assert!(pass.light_buffers.is_empty());
        assert!(pass.light_memories.is_empty());
    }

    fn static_draw(mesh: crate::managers::MeshHandle, material: Option<u32>) -> crate::DrawItem {
        crate::DrawItem {
            mesh,
            entity_id: 1,
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            material,
            cast_shadow: false,
            visible_in_main_pass: true,
            cast_shadow_two_sided: false,
            bone_offset: None,
            splat_slots: [u32::MAX; 4],
            splat_tiling: 0.0,
            state: DrawState::default(),
            world_bounds: None,
            index_start: 0,
            index_count: None,
        }
    }

    #[test]
    fn static_batches_group_identical_mesh_and_material() {
        let mesh = crate::managers::MeshHandle::default();
        let draws = [
            static_draw(mesh, Some(3)),
            static_draw(mesh, Some(3)),
            static_draw(mesh, Some(4)),
        ];
        let batches = build_static_batches_with(&draws, |_| true);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].items, vec![0, 1]);
        assert_eq!(batches[1].items, vec![2]);
    }

    #[test]
    fn static_batches_keep_skinned_and_transparent_as_singletons() {
        let mesh = crate::managers::MeshHandle::default();
        let mut skinned = static_draw(mesh, None);
        skinned.bone_offset = Some(0);
        let mut transparent = static_draw(mesh, None);
        transparent.state.render_queue = prism_gfx::RenderQueue::TRANSPARENT;
        let mut invisible = static_draw(mesh, None);
        invisible.visible_in_main_pass = false;
        let mut empty = static_draw(mesh, None);
        empty.index_count = Some(0);
        let draws = [skinned, transparent, invisible, empty];
        let batches = build_static_batches_with(&draws, |_| true);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].items, vec![0]);
        assert_eq!(batches[0].instance_offset, None);
        assert_eq!(batches[1].items, vec![1]);
        assert_eq!(batches[1].instance_offset, None);
        let draws = [static_draw(mesh, None), static_draw(mesh, None)];
        assert!(build_static_batches_with(&draws, |_| false).is_empty());
    }

    #[test]
    fn static_batches_do_not_overflow_the_instance_capacity() {
        let mesh = crate::managers::MeshHandle::default();
        let capacity = 4;
        let draws: Vec<_> = (0..=capacity).map(|_| static_draw(mesh, None)).collect();
        let (batches, matrices) = prepare_static_batches_with(&draws, |_| true, capacity);
        assert!(matrices.is_empty());
        assert_eq!(batches.len(), draws.len());
        assert!(batches.iter().all(|batch| batch.instance_offset.is_none()));
    }

    #[test]
    fn static_batches_assign_offsets_to_the_first_group() {
        let mesh = crate::managers::MeshHandle::default();
        let mut draws = vec![static_draw(mesh, None), static_draw(mesh, None)];
        draws.push(static_draw(mesh, Some(1)));
        let (batches, matrices) = prepare_static_batches_with(&draws, |_| true, 8);
        assert_eq!(matrices.len(), 2);
        assert_eq!(batches[0].instance_offset, Some(0));
        assert_eq!(batches[0].items.len(), 2);
        assert_eq!(batches[1].instance_offset, None);
        assert_eq!(batches[1].items.len(), 1);
    }
}
