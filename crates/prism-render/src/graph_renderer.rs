//! 基于 RenderGraph 的渲染器驱动程序。
//!
//! [`GraphRenderer`] 取代了旧版渲染器。它拥有 Vulkan 上下文、
//! 交换链、命令池 + 每帧命令缓冲区、帧 UBO、IBL 资源以及三个场景管理器
//!（网格、纹理、材质）。它构建包含 [`ShadowMapPass`] 和 [`ForwardPass`]
//! 的 [`RenderGraph`]，每帧执行并呈现到交换链。

use std::sync::Arc;

use anyhow::Context as _;
use ash::vk;

use crate::context::VulkanContext;
use crate::descriptor::{
    DescriptorLayout, DescriptorPool, FrameUBO, FrameUBOData, GpuLight, PtAnalyticLight,
};
use crate::external_overlay::{OverlayMessage, SwapchainOverlay};
use crate::forward_pass::ForwardPass;
use crate::ibl::IblResources;

/// 构造 IBL 资源。`ibl` feature 关闭时一律返回 1x1 中性灰（descriptor 布局不变，
/// 前向 pass 照常绑定 set 2），HDR 解码 / 卷积 / 预过滤 / BRDF LUT 全部不参与编译。
#[cfg(feature = "ibl")]
fn make_ibl(
    context: std::sync::Arc<crate::context::VulkanContext>,
    command_pool: ash::vk::CommandPool,
    queue: ash::vk::Queue,
    env: Option<Vec<u8>>,
    layout: ash::vk::DescriptorSetLayout,
) -> anyhow::Result<IblResources> {
    match env {
        Some(bytes) => IblResources::new(context, command_pool, queue, Some(bytes), layout),
        None => IblResources::new_empty(context, command_pool, queue, layout),
    }
}

#[cfg(not(feature = "ibl"))]
fn make_ibl(
    context: std::sync::Arc<crate::context::VulkanContext>,
    command_pool: ash::vk::CommandPool,
    queue: ash::vk::Queue,
    env: Option<Vec<u8>>,
    layout: ash::vk::DescriptorSetLayout,
) -> anyhow::Result<IblResources> {
    let _ = env; // 无 IBL 构建：忽略环境贴图，始终用中性立方体。
    IblResources::new_empty(context, command_pool, queue, layout)
}
use crate::managers::{
    AssetTextureHandle, MaterialHandle, MaterialUploadInput, MeshHandle, MeshUploadInput,
    RenderMaterialManager, RenderMeshManager, RenderTextureManager, TextureUploadInput,
};
use crate::mesh::Vertex;
use crate::offscreen::OffscreenTarget;
#[cfg(feature = "pathtrace")]
use crate::pt_pass::PathTracePass;
/// `ShadowMapPass::setup` 是 trait 方法，只有注册阴影 pass 时才需要导入。
#[cfg(feature = "shadows")]
use crate::render_graph::RenderPassNode;
use crate::render_graph::{
    DrawItem, GraphFrame, RenderGraph, RenderGraphBuilder, RenderMode, RenderSettings, ShadowMode,
};
use crate::scene_scope::SceneScope;
use crate::swapchain::Swapchain;

/// One resolved 绘制 for the bindless PBR path. The engine pre-resolves 资源
/// handles into render-side 网格 handles + 材质 SSBO slots and hands the
/// 渲染器 this flat 列表 (so the 渲染器 stays free of `prism_asset`
/// types). Previously lived in the deprecated monolithic 渲染器 kept here
/// as the engine<->renderer 交换 类型
pub struct SceneDrawItem {
    pub mesh: MeshHandle,
    pub material_slot: u32,
    pub model: [[f32; 4]; 4],
}

/// Bundled per-frame 输入 from the engine / app 层 to [`GraphRenderer`].
///
/// 内置 each 帧 by [`render_system`] (ECS → flat data) and consumed by
/// [`GraphRenderer::execute`], which unpacks it into [`GraphFrame`] +
/// [`RenderContext`] and hands them to the 渲染 图
///
/// This 结构体 is the **data boundary** between the CPU 更新 (ECS queries,
/// 相机 math, 光源 分辨率 …) and the GPU 渲染 管线 future
/// phases (prepare / scene sync) may inject additional data here without
/// touching the [`GraphRenderer`] plumbing.
#[derive(Clone)]
pub struct FrameInput<'a> {
    pub draw_items: &'a [DrawItem],
    /// ECS-authored gizmos for this frame.
    pub gizmos: &'a [crate::gizmo::GizmoInstance],
    /// ECS-authored 2D sprites for this frame (sorted back-to-front by the
    /// engine). Empty for 3D-only scenes.
    pub sprite_items: &'a [crate::render_graph::SpriteDrawItem],
    /// World-space particle billboards for this frame (sorted back-to-front
    /// by the engine). Empty when no `ParticleSystem` emits this frame.
    pub particle_items: &'a [crate::render_graph::ParticleDrawItem],
    /// ECS-authored skybox parameters; `None` means no sky draw.
    pub skybox: Option<crate::skybox_pass::SkyboxSettings>,
    pub frame_data: &'a FrameUBOData,
    pub light_view_proj: [[f32; 4]; 4],
    pub inv_projection: [[f32; 4]; 4],
    pub debug_flags: u32,
    pub tonemap_mode: u32,
    /// Post-process anti-aliasing mode (0 = off, 1 = FXAA). Runs inside
    /// `PostPass`, so it costs no extra render targets and works identically for
    /// the swapchain and offscreen (editor viewport) paths.
    pub aa_mode: u32,
    pub debug_rt: u32,
    /// Display-referred color grading (#502). `PostPass` applies it after
    /// tonemapping; [`crate::PostGrade::default`] is an exact no-op.
    pub grade: crate::render_graph::PostGrade,
    pub proj22: f32,
    pub proj32: f32,
    pub lights: &'a [GpuLight],
    pub render_mode: RenderMode,
    pub pt_max_bounces: u32,
    /// 最大值 PT primary + shadow 射线 长度 世界 units). Forwarded to the PT
    /// pass as a 推送 常量 so the 检查器 can 调音 it live.
    pub pt_ray_max_distance: f32,
    /// Exposure multiplier applied to the final 高动态范围 颜色 before tonemapping.
    /// When the 渲染 众数 is [`RenderMode::PathTrace`] this value is also
    /// forwarded to the PT 计算 着色器 as a 推送 常量
    pub exposure: f32,
    /// 最大 iterations (samples per 像素 for path tracing.
    /// 0 = accumulate forever 默认
    pub pt_max_iterations: u32,
    /// Analytic lights for path tracing (point/spot/area/directional).
    /// Passed via SSBO to the PT 计算 着色器 for multi-light NEE.
    pub pt_lights: &'a [PtAnalyticLight],
    /// When `true`, the path tracer should reset its accumulation 下一个 帧
    /// 集合 when directional-light properties (intensity/color/direction) change.
    pub pt_accum_dirty: bool,
    /// Whether a usable 相机 实体 was 找到 When `false`, passes should
    /// skip camera-dependent 功 (skybox, PT rays) and leave the 清空 颜色
    pub has_camera: bool,
    /// Whether the active camera projects orthographically. The path tracer
    /// uses it to cast parallel primary rays instead of a perspective fan
    /// (issue #10); the rasterizer already gets this from the projection
    /// matrix alone.
    pub camera_orthographic: bool,
    /// 主方向光是否投阴影（`Light.shadows != None`）。`false` 时整帧
    /// 跳过阴影贴图渲染（`ShadowMode::None`），着色器侧也会得到 `shadowStrength = 0`。
    pub shadows_enabled: bool,
    /// 清空 颜色 for the scene 颜色 附件 Applied when the 渲染 pass
    /// begins; shows through where no skybox or geometry is drawn. 默认 gray
    /// `[0.5, 0.5, 0.5, 1.0]` lets the user distinguish "nothing drew" from
    /// black.
    pub clear_color: [f32; 4],
    /// UI 叠加 绘制 commands (filled by engine's ui_render_system).
    /// `None` in headless 众数
    pub ui_overlay: Option<&'a crate::ui_overlay::UiOverlayInput>,
    /// 是否在场景渲染中绘制地面参考网格。
    pub show_grid: bool,
}

/// GPU session that owns the long-lived Vulkan 运行时 objects 设备
/// 命令 池 描述符 infrastructure). Survives 交换链 recreation
/// and is the 最小 集合 of fields that must outlive all 渲染 passes.
///
/// Extracted from [`GraphRenderer`] as the 第一个 step toward the dedicated
/// render-thread separation (PR-L2): once the 运行时 is self-contained, the
/// engine can 移动 it (and the [`RenderGraph`]) onto a separate 线程 without
/// moving scene-level managers and GUI 状态
pub struct RenderRuntime {
    pub command_pool: vk::CommandPool,
    pub command_buffers: Vec<vk::CommandBuffer>,
    #[allow(dead_code)]
    pub descriptor_layout: DescriptorLayout,
    #[allow(dead_code)]
    pub descriptor_pool: DescriptorPool,
    pub frame_ubos: Vec<FrameUBO>,
    /// **Must be the 最后一个 field** — Rust drops 结构体 fields in 声明
    /// order, so `context` (which owns the `ash::Device`) is destroyed *last*,
    /// after all child Vulkan objects (`descriptor_layout`, `descriptor_pool`,
    /// `frame_ubos`) have been cleaned 上 Without this ordering the 设备 is
    /// freed 第一个 and subsequent drops use a dangling handle → 访问 violation.
    pub context: Arc<VulkanContext>,
}

impl RenderRuntime {
    /// 构建 the 运行时 from a pre-created Vulkan context.
    ///
    /// `cmd_buffer_count` determines how many 命令 buffers to allocate
    /// (one per 交换链 图像
    fn new(context: Arc<VulkanContext>, cmd_buffer_count: u32) -> anyhow::Result<Self> {
        let descriptor_layout =
            DescriptorLayout::new(&context.device).context("create descriptor layout")?;
        let frame_count = 2u32;
        let descriptor_pool =
            DescriptorPool::new(&context.device, frame_count).context("create descriptor pool")?;
        let descriptor_sets = descriptor_pool
            .allocate_sets(&context.device, &descriptor_layout, frame_count)
            .context("allocate descriptor sets")?;

        let frame_ubos = descriptor_sets
            .into_iter()
            .map(|set| FrameUBO::new(&context, set))
            .collect::<anyhow::Result<Vec<_>>>()
            .context("create frame UBOs")?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(context.graphics_queue_family);
        let command_pool = unsafe { context.device.create_command_pool(&pool_info, None) }
            .context("create command pool")?;

        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(cmd_buffer_count);
        let command_buffers = unsafe { context.device.allocate_command_buffers(&alloc_info) }
            .context("allocate command buffers")?;

        Ok(Self {
            command_pool,
            command_buffers,
            descriptor_layout,
            descriptor_pool,
            frame_ubos,
            context, // last — dropped last via declaration order
        })
    }
}

pub struct GraphRenderer {
    swapchain: Option<Swapchain>,
    /// Present-mode preference reused when the surface is recreated after
    /// a suspend/resume cycle (`Swapchain::new_with_vsync`).
    vsync: bool,
    mesh_manager: RenderMeshManager,
    texture_manager: RenderTextureManager,
    material_manager: RenderMaterialManager,
    // Owned for RAII; IBL cubemap + 描述符 集合 are consumed via the
    // 描述符 集合 handle stored in `forward_pass`. Explicitly destroyed
    // in 销毁 so the 设备 handle is 有效 during cleanup.
    ibl: IblResources,
    /// Scene-level 全局光照 probe 音量 resources 集合 5). Survives 交换链
    /// recreation; only rebuilt on scene/level change.
    scene_scope: SceneScope,
    /// 固定 bindless 槽位，持有当前 IBL 的 BRDF LUT 视图。`set_environment`
    /// 重建 IBL 时用 `register_with_handle` 覆盖同一槽，避免 bindless 表增长。
    brdf_bindless_slot: u32,
    graph: RenderGraph,
    /// All 渲染 passes (ShadowMapPass + ForwardPass + GtaoPass + PostPass)
    /// are owned by the 图 and executed in registration order. The
    /// `GraphRenderer` no longer pokes individual passes; it drives them via
    /// `graph.execute` and reaches into them only for lifecycle ops
    /// (`recreate_swapchain`) via `graph.pass_mut`.
    settings: RenderSettings,
    shadow_sampler: vk::Sampler,
    // Captured from the graph's allocated shadow 映射表 consumed via the
    // 描述符 集合 in `forward_pass`.
    #[allow(dead_code)]
    shadow_view: vk::ImageView,
    /// `shadows` feature 关闭时的 1×1 远深度图（填充 forward 的 set 3）。
    #[cfg(not(feature = "shadows"))]
    dummy_shadow: crate::dummy::SolidImage,
    /// `gtao` feature 关闭时的 1×1 白色 AO 图（填充 forward 的 set 4）。
    #[cfg(not(feature = "gtao"))]
    dummy_ao: crate::dummy::SolidImage,
    #[allow(dead_code)]
    color_format: vk::Format,
    /// Optional 外部叠加层（宿主 UI 等）rendered on 顶部 of the ECS UI
    /// 叠加 When present, 执行 records it after the ECS UI overlay and it
    /// owns the COLOR_ATTACHMENT_OPTIMAL -> PRESENT_SRC_KHR 过渡. The
    /// 实现 is injected by the app 层 (eg 编辑器 host) — this 渲染器
    /// knows nothing about the concrete UI framework.
    external_overlay: Option<Box<dyn SwapchainOverlay>>,
    /// ECS UI 叠加 (GPU-only) — quads + fontdue glyph text, rendered on 顶部
    /// of the ForwardPass 输出 (below the external overlay). Created lazily
    /// on the 渲染 线程 when the 引擎 sends UI 命令
    #[cfg(feature = "ui")]
    ui_overlay: Option<crate::ui_overlay::UiOverlay>,
    /// Offscreen counterpart of [`Self::ui_overlay`]. It differs only in the
    /// render-pass final layout (`COLOR_ATTACHMENT_OPTIMAL` instead of
    /// `PRESENT_SRC_KHR`), so the two paths need separate caches (#597).
    #[cfg(feature = "ui")]
    ui_overlay_offscreen: Option<crate::ui_overlay::UiOverlay>,
    /// 工程自带字体（`Text.font` 的资产路径 → 字体原始字节，#583）。
    ///
    /// `UiOverlay` 是懒创建的（第一帧有 UI 命令时才建），所以字体先存在这里，
    /// 创建 overlay 时灌进去；已经存在的 overlay 直接当场注册。
    ui_fonts: std::collections::BTreeMap<String, Vec<u8>>,
    /// True when no 表面 is available (headless / CI / server 众数
    is_headless: bool,
    /// Offscreen 目标 for headless 众数 — owned device-local 图像 +
    /// host-visible staging 缓冲区 `None` in windowed 众数
    offscreen: Option<OffscreenTarget>,
    /// Fence used to synchronize command buffer reuse in headless / offscreen mode.
    headless_fence: vk::Fence,
    /// 已预约、尚未在 `execute` 里落地的截图请求。
    pending_screenshot: Option<crate::screenshot::ScreenshotRequest>,
    /// 已经记录进命令缓冲区、等待帧提交后读回的截图。
    screenshot: Option<crate::screenshot::ScreenshotCapture>,
    /// per-pass GPU 时间戳池（#518）；设备不支持时为 `None`。
    gpu_timer: Option<crate::gpu_timer::GpuTimer>,
    /// 最近一次读回的 per-pass GPU 时间（延迟一帧）。
    gpu_timings: crate::gpu_timer::GpuTimings,

    // ── 最后一个 field ──────────────────────────────────────────────────
    /// Long-lived GPU session 设备 命令 池 descriptors, UBOs).
    ///
    /// **Must be the 最后一个 field** — Rust drops 结构体 fields in 声明
    /// order, so 运行时 (which owns the `Arc<VulkanContext>`) is destroyed
    /// *last*, after all other Vulkan-dependent fields have been cleaned 上
    /// This prevents the `ash::Device` from being freed while sibling-field
    /// drops still 引用 it.
    runtime: RenderRuntime,
}

/// Per-frame context returned by [`GraphRenderer::begin_frame`], consumed by
/// [`GraphRenderer::execute`] and [`GraphRenderer::present`].
pub struct FrameCtx {
    pub device: ash::Device,
    pub cmd: vk::CommandBuffer,
    pub image_index: u32,
    pub frame_index: u32,
    pub extent: vk::Extent2D,
    fence: vk::Fence,
    image_available: vk::Semaphore,
    render_finished: vk::Semaphore,
}

/// 把已登记的工程字体灌进（刚创建的）UI overlay（#583）。
#[cfg(feature = "ui")]
fn apply_ui_fonts(
    fonts: &std::collections::BTreeMap<String, Vec<u8>>,
    overlay: &mut crate::ui_overlay::UiOverlay,
) {
    for (name, bytes) in fonts {
        overlay.register_font(name, bytes.clone());
    }
}

impl GraphRenderer {
    pub fn new(
        window_extensions: Vec<&str>,
        window: &dyn raw_window_handle::HasDisplayHandle,
        window_handle: &dyn raw_window_handle::HasWindowHandle,
        // 窗口模式：IBL 由场景 `EnvironmentLighting` 在加载时经 `set_environment`
        // 驱动；此处忽略启动期 env_bytes（保持引擎「场景声明式光照」）。
        _env_bytes: Option<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        Self::new_with_vsync(window_extensions, window, window_handle, _env_bytes, true)
    }

    /// Like [`GraphRenderer::new`], but with an explicit vsync preference
    /// forwarded to the swapchain present-mode selection.
    pub fn new_with_vsync(
        window_extensions: Vec<&str>,
        window: &dyn raw_window_handle::HasDisplayHandle,
        window_handle: &dyn raw_window_handle::HasWindowHandle,
        _env_bytes: Option<Vec<u8>>,
        vsync: bool,
    ) -> anyhow::Result<Self> {
        let context = Arc::new(VulkanContext::new(&window_extensions)?);
        let swapchain = Swapchain::new_with_vsync(&context, window, window_handle, vsync)?;
        let color_format = swapchain.format.format;

        // 构建 the GPU 运行时 描述符 infrastructure, 命令 池
        // per-frame 命令 buffers, and 帧 UBOs. These survive 交换链
        // recreation; the 运行时 is independent of scene-level resources.
        let runtime = RenderRuntime::new(context.clone(), swapchain.views.len() as u32)?;

        // IBL：开屏/无光照场景以廉价「中性空 IBL」启动（不做 CPU 预卷积，≈0ms）。
        // 真正的 IBL 预卷积由 `set_environment` 在加载带光照的 3D 场景时按需重建。
        // 描述符 集合 布局 共享创建一次，确保场景切换重建 IBL 时管线 布局 稳定。
        let ibl_ds_layout = crate::ibl::ibl_descriptor_set_layout(&context.device)
            .context("create IBL ds layout")?;
        let ibl = IblResources::new_empty(
            context.clone(),
            runtime.command_pool,
            context.graphics_queue,
            ibl_ds_layout,
        )
        .context("create empty IBL resources")?;

        let mut texture_manager =
            RenderTextureManager::new(&context, runtime.command_pool, context.graphics_queue, 1024)
                .context("create RenderTextureManager")?;
        let material_manager =
            RenderMaterialManager::new(&context).context("create RenderMaterialManager")?;
        let mesh_manager = RenderMeshManager::new();

        let shadow_sampler = unsafe {
            context.device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .compare_enable(true)
                    .compare_op(vk::CompareOp::LESS)
                    .border_color(vk::BorderColor::FLOAT_OPAQUE_WHITE)
                    .unnormalized_coordinates(false),
                None,
            )
        }
        .context("create shadow comparison sampler")?;

        let resolved = RenderSettings::default().resolve_shadow(&context.rt_caps);
        let settings = RenderSettings {
            shadow_mode: resolved,
            ray_tracing_enabled: false,
            ..Default::default()
        };

        // 构建 图 with ShadowMapPass. 调用 setup() on the pass before
        // adding it so it registers its shadow-map 资源 then allocate the
        // graph's Vulkan resources (the shadow 映射表 深度 图像 and fetch its
        // 图像 视图 for the ForwardPass to 样本
        //
        // `shadows` feature 关闭时整块不编译：图直接从空开始，ForwardPass 的
        // set 3 改绑一张 1×1 远深度 dummy（descriptor 布局不变，仍能过验证层）。
        #[cfg(feature = "shadows")]
        let (mut graph, shadow_view) = {
            let mut shadow_pass = crate::shadow_map_pass::ShadowMapPass::new();
            let mut builder = RenderGraphBuilder::new().settings(&settings);
            shadow_pass.setup(&mut builder, &settings);
            let shadow_handle = shadow_pass.shadow_map_handle();
            builder.add_pass(Box::new(shadow_pass));
            let mut graph = builder.build();
            graph
                .allocate_resources(&context.device, &context.physical_device_memory_properties)
                .context("allocate graph resources")?;
            let shadow_view = graph
                .image_view(shadow_handle)
                .context("shadow map view not found")?;
            (graph, shadow_view)
        };

        #[cfg(not(feature = "shadows"))]
        let (mut graph, shadow_view, dummy_shadow) = {
            let mut graph = RenderGraphBuilder::new().settings(&settings).build();
            graph
                .allocate_resources(&context.device, &context.physical_device_memory_properties)
                .context("allocate graph resources (shadows off)")?;
            let dummy = crate::dummy::shadow_map_dummy(
                &context,
                runtime.command_pool,
                context.graphics_queue,
            )
            .context("create 1x1 dummy shadow map")?;
            log::warn!(
                "`shadows` feature 关闭：实时阴影不可用（set 3 绑定 1x1 远深度图）；\
                 需要阴影请在项目里启用 prism-render/shadows"
            );
            let view = dummy.view();
            (graph, view, dummy)
        };

        // 创建 forward_pass and wire its resources: IBL 集合 shadow 映射表 视图 +
        // 比较 采样器 bindless 纹理 表 材质 SSBO, and the
        // per-frame UBO buffers (one set0 描述符 集合 per frame-in-flight).
        // ForwardPass is executed directly by GraphRenderer (it targets the
        // 交换链 not a graph-managed 资源
        let frame_ubo_buffers: Vec<vk::Buffer> =
            runtime.frame_ubos.iter().map(|u| u.buffer).collect();
        let bindless = texture_manager.bindless_mut();
        let materials_buffer = material_manager.buffer();

        // Register the BRDF LUT in the bindless 纹理 表
        let brdf_handle = bindless
            .register(ibl.brdf_image_view())
            .context("register BRDF LUT into bindless table")?;
        log::info!(
            "IBL: BRDF LUT registered as bindless handle {}",
            brdf_handle.0
        );

        let mut forward_pass = ForwardPass::new(color_format);
        // Scene-level 全局光照 probe 音量 (SceneScope). Created before ForwardPass
        // wiring so its 描述符 集合 + 布局 can be borrowed 集合 5).
        let scene_scope = SceneScope::new(context.clone()).context("SceneScope::new")?;
        forward_pass
            .set_resources(
                &context,
                ibl.descriptor_set,
                ibl.descriptor_set_layout,
                shadow_view,
                shadow_sampler,
                bindless.set,
                bindless.layout,
                materials_buffer,
                &frame_ubo_buffers,
                brdf_handle.0,
                scene_scope.descriptor_set,
                scene_scope.descriptor_set_layout,
            )
            .context("ForwardPass: set_resources")?;

        // GTAO pass half-resolution screen-space 环境光遮蔽 Runs after ForwardPass
        // every 帧 and produces a double-buffered R8 环境光遮蔽 纹理 the scene
        // samples (1-frame 延迟 to attenuate IBL diffuse + specular.
        //
        // `gtao` feature 关闭时整块不编译：set 4 改绑 1×1 白色（无遮蔽）dummy。
        #[cfg(feature = "gtao")]
        let swapchain_extent = swapchain.extent;
        #[cfg(feature = "gtao")]
        let gtao_pass =
            crate::gtao::GtaoPass::new(&context, runtime.command_pool, swapchain_extent)
                .context("GtaoPass::new")?;
        #[cfg(not(feature = "gtao"))]
        let dummy_ao =
            crate::dummy::ao_texture_dummy(&context, runtime.command_pool, context.graphics_queue)
                .context("create 1x1 dummy AO texture")?;
        #[cfg(not(feature = "gtao"))]
        log::warn!(
            "`gtao` feature 关闭：屏幕空间环境光遮蔽不可用（set 4 绑定 1x1 白色图）；\
             需要 AO 请在项目里启用 prism-render/gtao"
        );

        // PostPass parameters: 2 in-flight frames = 2 描述符 sets.
        let frame_count = 2u32;
        let post_pass = crate::post::PostPass::new(
            &context,
            color_format,
            frame_count,
            vk::ImageLayout::PRESENT_SRC_KHR,
        )
        .context("PostPass::new")?;

        // PathTracePass — real-time path tracing compute pass.
        // Registered when hardware acceleration structures are supported;
        // checks RenderSettings.render_mode internally to decide whether to
        // dispatch. Scene geometry is uploaded later via set_geometry.
        // `pathtrace` feature 关闭时整块不编译（Lego，见 DESIGN §12）。
        #[cfg(feature = "pathtrace")]
        #[cfg(feature = "pathtrace")]
        let pt_pass = if context.rt_caps.acceleration_structure {
            match PathTracePass::new(&context) {
                Ok(mut pt) => {
                    pt.set_material_resources(materials_buffer, bindless.set, bindless.layout);
                    pt.set_ibl_resources(ibl.descriptor_set, ibl.descriptor_set_layout);
                    log::info!("PathTracePass registered in render graph");
                    Some(pt)
                }
                Err(err) => {
                    log::warn!("Failed to initialize PathTracePass despite RT caps: {err:#}");
                    None
                }
            }
        } else {
            log::debug!("Acceleration structures not supported; PathTracePass not registered");
            None
        };

        // CRT 调度器：统一渲染所有注册的 RenderTexture（引擎式）。
        let rt_scheduler = crate::rt_scheduler::RenderTextureScheduler::new(&context);

        // Register all passes into the 图 in 执行 order.
        // Shadow -> Scene -> GTAO -> RenderTextures -> PathTrace -> Post.
        graph.add_pass(Box::new(forward_pass));
        // 启动路径：同步 per-swapchain-image 槽位数。`ForwardPass::execute` 的
        // `ensure_target` 按 `image_count` 建 framebuffer，`recreate_swapchain`
        // 只覆盖下一步扩展重建——首次创建必须在这里初始化，否则 `set_target` 里
        // `idx >= image_count` 的守卫会静默跳过，永远建不出 framebuffer。
        if let Some(forward_pass) = graph.pass_mut::<ForwardPass>() {
            forward_pass.set_image_count(swapchain.views.len());
        }
        #[cfg(feature = "gtao")]
        graph.add_pass(Box::new(gtao_pass));
        graph.add_pass(Box::new(rt_scheduler));
        #[cfg(feature = "pathtrace")]
        if let Some(pt) = pt_pass {
            graph.add_pass(Box::new(pt));
        }
        graph.add_pass(Box::new(post_pass));

        // per-pass GPU 计时（#518）：设备不支持时间戳时是 `None`（面板回退到 CPU 墙钟）。
        let gpu_timer = crate::gpu_timer::GpuTimer::new(
            &context.instance,
            &context.device,
            context.physical_device,
            runtime.command_buffers.len(),
        )?;

        Ok(Self {
            swapchain: Some(swapchain),
            vsync,
            runtime,
            mesh_manager,
            texture_manager,
            material_manager,
            ibl,
            scene_scope,
            brdf_bindless_slot: brdf_handle.0,
            graph,
            settings,
            shadow_sampler,
            shadow_view,
            #[cfg(not(feature = "shadows"))]
            dummy_shadow,
            #[cfg(not(feature = "gtao"))]
            dummy_ao,
            color_format,
            external_overlay: None,
            #[cfg(feature = "ui")]
            ui_overlay: None,
            #[cfg(feature = "ui")]
            ui_overlay_offscreen: None,
            ui_fonts: std::collections::BTreeMap::new(),
            is_headless: false,
            offscreen: None,
            headless_fence: vk::Fence::null(),
            pending_screenshot: None,
            screenshot: None,
            gpu_timer,
            gpu_timings: Default::default(),
        })
    }
    // -------------------------------------------------------------------
    // 公开 API
    // -------------------------------------------------------------------

    pub fn context(&self) -> &VulkanContext {
        &self.runtime.context
    }
    pub fn context_arc(&self) -> Arc<VulkanContext> {
        self.runtime.context.clone()
    }
    pub fn command_pool(&self) -> vk::CommandPool {
        self.runtime.command_pool
    }
    pub fn graphics_queue(&self) -> vk::Queue {
        self.runtime.context.graphics_queue
    }

    /// Whether this 渲染器 was created in headless 众数 (no 窗口 表面
    pub fn is_headless(&self) -> bool {
        self.is_headless
    }

    /// 场景 pass 要采样的「上一帧环境光遮蔽」视图。
    ///
    /// `gtao` 开启时取 GTAO pass 的双缓冲 `(frame+1)%2`；关闭时返回 1×1 白色
    /// dummy（可见性 1 = 无遮蔽），descriptor 布局与绑定次数完全一致。
    #[cfg(feature = "gtao")]
    fn ao_view(&mut self, frame: u32) -> vk::ImageView {
        self.graph
            .pass_mut::<crate::gtao::GtaoPass>()
            .map(|g| g.ao_view((frame + 1) % 2))
            .unwrap_or_else(vk::ImageView::null)
    }

    #[cfg(not(feature = "gtao"))]
    fn ao_view(&mut self, _frame: u32) -> vk::ImageView {
        self.dummy_ao.view()
    }

    /// 离屏渲染目标的尺寸（无窗口宿主模式）；有交换链时返回 `None`。
    pub fn offscreen_extent(&self) -> Option<ash::vk::Extent2D> {
        self.offscreen.as_ref().map(|target| target.extent())
    }

    /// 注册一个工程自带字体（`.ttf` / `.otf` / `.ttc` 的原始字节，#583）。
    ///
    /// 宿主从 `.pak` 读出字节后调用一次即可；名字是 `Text.font` 里写的资产路径。
    /// UI overlay 是懒创建的，所以这里先把字节存起来，等第一帧真正画 UI 时
    /// 再灌进图集；已经创建的那一份立即生效。
    pub fn register_ui_font(&mut self, name: impl Into<String>, bytes: Vec<u8>) {
        let name = name.into();
        self.ui_fonts.insert(name.clone(), bytes.clone());
        #[cfg(feature = "ui")]
        {
            if let Some(gpu) = self.ui_overlay.as_mut() {
                gpu.register_font(&name, bytes.clone());
            }
            if let Some(gpu) = self.ui_overlay_offscreen.as_mut() {
                gpu.register_font(&name, bytes);
            }
        }
        #[cfg(not(feature = "ui"))]
        let _ = (name, bytes);
    }

    /// 离屏颜色图像视图（`R8G8B8A8_UNORM`）。帧结束后先调用
    /// [`Self::offscreen_present_for_sampling`]，再把它注册成 UI 纹理。
    pub fn offscreen_image_view(&self) -> Option<ash::vk::ImageView> {
        self.offscreen.as_ref().map(|target| target.image_view())
    }

    /// 离屏颜色图像的格式。
    pub fn offscreen_image_format(&self) -> Option<ash::vk::Format> {
        self.offscreen.as_ref().map(|target| target.format())
    }

    /// 把帧结束后的离屏图像转到 `SHADER_READ_ONLY_OPTIMAL` 并等待 GPU
    /// 完成，供 UI 框架直接采样（零拷贝视口）。
    ///
    /// 帧结束后离屏图像停在 `COLOR_ATTACHMENT_OPTIMAL`（headless 上下文没有
    /// `VK_KHR_swapchain`，不能用 `PRESENT_SRC_KHR`）；下一次帧的 PostPass 用
    /// `initial_layout = UNDEFINED`（load op clear），因此从任何布局回来都合法。
    pub fn offscreen_present_for_sampling(&mut self) -> anyhow::Result<()> {
        let context = self.runtime.context.clone();
        let target = self
            .offscreen
            .as_mut()
            .context("offscreen_present_for_sampling: no OffscreenTarget")?;
        target.transition_to_shader_read(&context, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
    }

    // ── headless 众数 ──────────────────────────────────────────────

    /// 创建 a headless `GraphRenderer` — no 窗口 表面 or 交换链
    /// Useful for CI tests, dedicated servers, and offline 资源 baking.
    ///
    /// `env_bytes` is optional IBL environment 映射表 data (`None` = 默认 sky).
    pub fn headless_new(env_bytes: Option<Vec<u8>>) -> anyhow::Result<Self> {
        Self::offscreen_new(256, 256, env_bytes)
    }

    /// 创建 a 无窗口 `GraphRenderer` rendering into an offscreen 图像 of
    /// `width × height` (编辑器 host 模式 / 离屏预览).
    ///
    /// 与 [`Self::headless_new`] 同一条渲染路径：完整图（Shadow → Scene →
    /// GTAO → Post）跑完后，PostPass 把 **tonemap 后的结果** 写进离屏图像，
    /// 随后 [`Self::copy_offscreen_to_readback`] + [`Self::readback_pixels`]
    /// 把那一帧读回 CPU。
    pub fn offscreen_new(
        width: u32,
        height: u32,
        env_bytes: Option<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let context = Arc::new(VulkanContext::new(&[])?);
        Self::offscreen_new_with_context(context, width, height, env_bytes)
    }

    /// Like [`Self::offscreen_new`], but on a **host-provided** context.
    ///
    /// This is the zero-copy host path: the editor creates the `VkDevice`
    /// (shared with its UI framework via [`VulkanContext::from_host`]) and the
    /// renderer draws into its own offscreen image. After each frame
    /// [`Self::offscreen_present_for_sampling`] turns that image into a
    /// `SHADER_READ_ONLY_OPTIMAL` texture and
    /// [`Self::offscreen_image_view`] hands it to the UI framework —
    /// no `vkCmdCopyImageToBuffer` / CPU round-trip.
    pub fn offscreen_new_with_context(
        context: Arc<VulkanContext>,
        width: u32,
        height: u32,
        env_bytes: Option<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let offscreen = OffscreenTarget::new(&context, width, height)?;
        let color_format = offscreen.format();

        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        let headless_fence = unsafe { context.device.create_fence(&fence_info, None) }
            .context("create headless frame fence")?;

        // 运行时 with 2 命令 buffers (minimal for headless ops).
        let runtime = RenderRuntime::new(context.clone(), 2)?;

        // Headless 众数 still builds the 完整 渲染 图 so the
        // graph's topology is validated even without a display.
        let resolver = RenderSettings::default().resolve_shadow(&context.rt_caps);
        let settings = RenderSettings {
            shadow_mode: resolver,
            ray_tracing_enabled: false,
            ..Default::default()
        };

        let ibl_ds_layout = crate::ibl::ibl_descriptor_set_layout(&context.device)
            .context("create IBL ds layout (headless)")?;
        let ibl = make_ibl(
            context.clone(),
            runtime.command_pool,
            context.graphics_queue,
            env_bytes,
            ibl_ds_layout,
        )
        .context("create IBL resources (headless)")?;

        let mut texture_manager =
            RenderTextureManager::new(&context, runtime.command_pool, context.graphics_queue, 1024)
                .context("create RenderTextureManager (headless)")?;
        let material_manager = RenderMaterialManager::new(&context)
            .context("create RenderMaterialManager (headless)")?;
        let mesh_manager = RenderMeshManager::new();

        let shadow_sampler = unsafe {
            context.device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .compare_enable(true)
                    .compare_op(vk::CompareOp::LESS)
                    .border_color(vk::BorderColor::FLOAT_OPAQUE_WHITE)
                    .unnormalized_coordinates(false),
                None,
            )
        }
        .context("create shadow comparison sampler (headless)")?;

        // 图 Shadow -> Scene -> GTAO -> PathTrace -> Post.
        #[cfg(feature = "shadows")]
        let (mut graph, shadow_view) = {
            let mut shadow_pass = crate::shadow_map_pass::ShadowMapPass::new();
            let mut builder = RenderGraphBuilder::new().settings(&settings);
            shadow_pass.setup(&mut builder, &settings);
            let shadow_handle = shadow_pass.shadow_map_handle();
            builder.add_pass(Box::new(shadow_pass));
            let mut graph = builder.build();
            graph
                .allocate_resources(&context.device, &context.physical_device_memory_properties)
                .context("allocate graph resources (headless)")?;
            let shadow_view = graph
                .image_view(shadow_handle)
                .context("shadow map view not found (headless)")?;
            (graph, shadow_view)
        };

        #[cfg(not(feature = "shadows"))]
        let (mut graph, shadow_view, dummy_shadow) = {
            let mut graph = RenderGraphBuilder::new().settings(&settings).build();
            graph
                .allocate_resources(&context.device, &context.physical_device_memory_properties)
                .context("allocate graph resources (headless, shadows off)")?;
            let dummy = crate::dummy::shadow_map_dummy(
                &context,
                runtime.command_pool,
                context.graphics_queue,
            )
            .context("create 1x1 dummy shadow map (headless)")?;
            log::warn!(
                "`shadows` feature 关闭：实时阴影不可用（set 3 绑定 1x1 远深度图）；\
                 需要阴影请在项目里启用 prism-render/shadows"
            );
            let view = dummy.view();
            (graph, view, dummy)
        };

        let frame_ubo_buffers: Vec<vk::Buffer> =
            runtime.frame_ubos.iter().map(|u| u.buffer).collect();
        let bindless = texture_manager.bindless_mut();
        let materials_buffer = material_manager.buffer();

        let brdf_handle = bindless
            .register(ibl.brdf_image_view())
            .context("register BRDF LUT into bindless table (headless)")?;

        let mut forward_pass = ForwardPass::new(color_format);
        let scene_scope = SceneScope::new(context.clone()).context("SceneScope::new (headless)")?;
        forward_pass
            .set_resources(
                &context,
                ibl.descriptor_set,
                ibl.descriptor_set_layout,
                shadow_view,
                shadow_sampler,
                bindless.set,
                bindless.layout,
                materials_buffer,
                &frame_ubo_buffers,
                brdf_handle.0,
                scene_scope.descriptor_set,
                scene_scope.descriptor_set_layout,
            )
            .context("ForwardPass::set_resources (headless)")?;

        let frame_count = 2u32;
        // Headless contexts do not enable `VK_KHR_swapchain`, so the offscreen
        // image must never end a render pass in `PRESENT_SRC_KHR`; it stays a
        // color attachment until something explicitly transitions it.
        let post_pass = crate::post::PostPass::new(
            &context,
            color_format,
            frame_count,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        )
        .context("PostPass::new (headless)")?;

        // `gtao` feature 关闭时 set 4 退化为 1×1 白色（无遮蔽）。
        #[cfg(feature = "gtao")]
        let gtao_pass =
            crate::gtao::GtaoPass::new(&context, runtime.command_pool, offscreen.extent())
                .context("GtaoPass::new (headless)")?;
        #[cfg(not(feature = "gtao"))]
        let dummy_ao =
            crate::dummy::ao_texture_dummy(&context, runtime.command_pool, context.graphics_queue)
                .context("create 1x1 dummy AO texture (headless)")?;
        #[cfg(not(feature = "gtao"))]
        log::warn!(
            "`gtao` feature 关闭：屏幕空间环境光遮蔽不可用（set 4 绑定 1x1 白色图）；\
             需要 AO 请在项目里启用 prism-render/gtao"
        );

        // CRT 调度器（headless 下同样接入图，支持无窗口离屏 RT 调度）。
        let rt_scheduler = crate::rt_scheduler::RenderTextureScheduler::new(&context);

        #[cfg(feature = "pathtrace")]
        let pt_pass = if context.rt_caps.acceleration_structure {
            match PathTracePass::new(&context) {
                Ok(mut pt) => {
                    pt.set_material_resources(materials_buffer, bindless.set, bindless.layout);
                    pt.set_ibl_resources(ibl.descriptor_set, ibl.descriptor_set_layout);
                    log::info!("PathTracePass registered in render graph (headless)");
                    Some(pt)
                }
                Err(err) => {
                    log::warn!(
                        "Failed to initialize PathTracePass (headless) despite RT caps: {err:#}"
                    );
                    None
                }
            }
        } else {
            None
        };

        graph.add_pass(Box::new(forward_pass));
        if let Some(forward_pass) = graph.pass_mut::<ForwardPass>() {
            forward_pass.set_image_count(1);
            forward_pass.set_target(&context.device, &context, 1, 0, offscreen.extent())?;
        }
        #[cfg(feature = "gtao")]
        graph.add_pass(Box::new(gtao_pass));
        graph.add_pass(Box::new(rt_scheduler));
        #[cfg(feature = "pathtrace")]
        if let Some(pt) = pt_pass {
            graph.add_pass(Box::new(pt));
        }
        graph.add_pass(Box::new(post_pass));
        if let Some(post_pass) = graph.pass_mut::<crate::post::PostPass>() {
            post_pass.set_target(
                &context.device,
                std::slice::from_ref(&offscreen.image_view()),
                0,
                offscreen.extent(),
            )?;
        }

        let gpu_timer = crate::gpu_timer::GpuTimer::new(
            &context.instance,
            &context.device,
            context.physical_device,
            runtime.command_buffers.len(),
        )?;

        Ok(Self {
            swapchain: None,
            vsync: false,
            runtime,
            mesh_manager,
            texture_manager,
            material_manager,
            ibl,
            scene_scope,
            brdf_bindless_slot: brdf_handle.0,
            graph,
            settings,
            shadow_sampler,
            shadow_view,
            #[cfg(not(feature = "shadows"))]
            dummy_shadow,
            #[cfg(not(feature = "gtao"))]
            dummy_ao,
            color_format,
            external_overlay: None,
            #[cfg(feature = "ui")]
            ui_overlay: None,
            #[cfg(feature = "ui")]
            ui_overlay_offscreen: None,
            ui_fonts: std::collections::BTreeMap::new(),
            is_headless: true,
            pending_screenshot: None,
            screenshot: None,
            gpu_timer,
            gpu_timings: Default::default(),
            offscreen: Some(offscreen),
            headless_fence,
        })
    }

    /// 清空 the offscreen 图像 to 颜色 复制 to the host-visible 缓冲区
    /// and wait for the GPU. Only 有效 in headless 众数
    pub fn clear_offscreen(&mut self, color: [f32; 4]) -> anyhow::Result<()> {
        let target = self
            .offscreen
            .as_mut()
            .context("clear_offscreen called but no OffscreenTarget (not headless?)")?;
        target.clear_and_copy(&self.runtime.context, color)
    }

    /// 读取 后 像素 data from the offscreen 目标
    /// Returns RGBA 字节 256 × 256 = 262144 字节
    /// Only 有效 after [`clear_offscreen`](Self::clear_offscreen) or
    /// [`copy_offscreen_to_readback`](Self::copy_offscreen_to_readback).
    pub fn readback_pixels(&self) -> anyhow::Result<Vec<u8>> {
        let target = self
            .offscreen
            .as_ref()
            .context("readback_pixels called but no OffscreenTarget")?;
        target.readback(&self.runtime.context)
    }

    /// 轮询异步离屏读回；GPU 未完成时返回 `None`。
    pub fn poll_offscreen_readback(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        self.offscreen
            .as_mut()
            .context("poll_offscreen_readback called but no OffscreenTarget")?
            .poll_readback(&self.runtime.context)
    }

    /// Copy the rendered offscreen image to the host-visible readback buffer
    /// and wait for completion. Only valid in headless mode. Afterwards call
    /// [`readback_pixels`](Self::readback_pixels).
    pub fn copy_offscreen_to_readback(&mut self) -> anyhow::Result<()> {
        let target = self
            .offscreen
            .as_mut()
            .context("copy_offscreen_to_readback called but no OffscreenTarget (not headless?)")?;
        target.copy_to_readback(
            &self.runtime.context,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        )
    }

    /// 异步提交离屏图像读回；已有读回未完成时跳过本次提交。
    pub fn copy_offscreen_to_readback_async(&mut self) -> anyhow::Result<bool> {
        self.offscreen
            .as_mut()
            .context("copy_offscreen_to_readback_async called but no OffscreenTarget")?
            .copy_to_readback_async(
                &self.runtime.context,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            )
    }

    /// Immutable 借用 of the 渲染 图 (passes + declared resources +
    /// settings). Exposed for the render-graph visualizer (F2): the viz takes a
    /// per-frame 快照 from this and reads live per-pass 状态 via
    /// `pass_ref::<T>()`. Read-only - no mutation path is exposed.
    pub fn graph(&self) -> &RenderGraph {
        &self.graph
    }

    /// Mutable 借用 of the 渲染 图 Used internally for lifecycle ops
    /// and by the app 层 to reach passes (e.g. PathTracePass::set_geometry)
    /// after construction.
    pub fn graph_mut(&mut self) -> &mut RenderGraph {
        &mut self.graph
    }

    /// Request a path-tracer accumulation reset on the 下一个 帧 调用 this
    /// when a 渲染 参数 that affects traced radiance changes 最大值
    /// bounces, exposure, 光源 color/direction/intensity, scene reload, ...).
    /// No-op if PathTracePass isn't in the 图
    pub fn request_pt_reset(&mut self) {
        #[cfg(feature = "pathtrace")]
        if let Some(pt) = self.graph.pass_mut::<PathTracePass>() {
            pt.request_reset();
        }
    }

    /// 当前 PT 帧 计数器 (number of accumulated samples per 像素
    /// Capped at `pt_max_iterations` when freeze is 激活 (> 0), so the UI
    /// doesn't keep counting after the 着色器 stops accumulating.
    /// Returns `None` if the path-trace pass is not in the 图
    pub fn pt_frame_count(&self) -> Option<u32> {
        // 不用 `return`：关掉 `pathtrace` 时函数体只剩这一句，`return None;`
        // 会让 clippy 的 `needless_return` 直接报错（-D warnings 下编译失败）。
        #[cfg(not(feature = "pathtrace"))]
        {
            None
        }
        #[cfg(feature = "pathtrace")]
        self.graph.pass_ref::<PathTracePass>().map(|pt| {
            let fc = pt.frame_count();
            let max = self.settings.pt_max_iterations;
            if max > 0 && fc > max {
                max
            } else {
                fc
            }
        })
    }

    /// 注入外部叠加层（宿主 UI 等）。主线程在启动渲染线程前调用；
    /// overlay 的 GPU 资源由实现方在 `record` 时懒创建。
    pub fn set_external_overlay(&mut self, overlay: Box<dyn SwapchainOverlay>) {
        self.external_overlay = Some(overlay);
    }

    /// 把一条类型擦除的消息（如"新的宿主 UI 帧"）投递给外部叠加层。
    /// 渲染线程从 shared 通路取出消息后调用；无叠加层时静默丢弃。
    pub fn apply_overlay_message(&mut self, msg: OverlayMessage) {
        if let Some(overlay) = self.external_overlay.as_mut() {
            msg(overlay.as_mut());
        }
    }

    /// IBL 描述符 集合 + 布局 for the environment cubemap 集合 2).
    /// Used by PathTracePass and ForwardPass for 高动态范围 sky / 间接 lighting.
    pub fn ibl_descriptor_set(&self) -> vk::DescriptorSet {
        self.ibl.descriptor_set
    }
    pub fn ibl_descriptor_set_layout(&self) -> vk::DescriptorSetLayout {
        self.ibl.descriptor_set_layout
    }

    /// 按场景重建 IBL。`env_bytes = Some(hdr)` 跑完整 CPU 预卷积；
    /// `None` 重建为廉价中性空 IBL（与 `new_empty` 等价）。用于在「场景切换」
    /// 时按需构建/移除光照，而不在引擎启动期无条件加载 3D IBL/GI。
    ///
    /// 共享的 set2 描述符 布局 保持稳定（`self.ibl.descriptor_set_layout`），
    /// 仅替换 descriptor set 句柄与 BRDF LUT（覆盖同一 bindless 槽），
    /// forward pass / skybox 管线 布局 无需重建。
    pub fn set_environment(&mut self, env_bytes: Option<Vec<u8>>) -> anyhow::Result<()> {
        // The previous IBL (and its BRDF LUT view) may still be sampled by
        // frames that are in flight. Finish them before overwriting the
        // bindless slot or destroying the old images/samplers; otherwise the
        // GPU can read freed resources (device-lost / validation error).
        // Environment switches are rare, so an idle wait is acceptable.
        unsafe { self.runtime.context.device.device_wait_idle() }
            .context("wait idle before replacing IBL")?;

        // `None` = 无光照场景（开屏/UI）：走廉价中性空 IBL（new_empty，不卷积，≈0ms）。
        // 共享 set2 描述符 布局 保持稳定。
        let new_ibl = make_ibl(
            self.runtime.context.clone(),
            self.runtime.command_pool,
            self.runtime.context.graphics_queue,
            env_bytes,
            self.ibl.descriptor_set_layout,
        )
        .context("rebuild IBL resources for scene")?;
        // 覆盖同一 bindless 槽的 BRDF LUT（表不增长）。
        self.texture_manager
            .bindless_mut()
            .register_with_handle(self.brdf_bindless_slot, new_ibl.brdf_image_view())
            .context("overwrite BRDF LUT bindless slot")?;

        // 切换 forward pass / skybox 的 set2（布局共享，不重建管线）。
        if let Some(fp) = self.graph.pass_mut::<ForwardPass>() {
            fp.set_ibl(new_ibl.descriptor_set, self.brdf_bindless_slot);
        }
        #[cfg(feature = "pathtrace")]
        if let Some(pt) = self.graph.pass_mut::<PathTracePass>() {
            pt.set_ibl_resources(new_ibl.descriptor_set, self.ibl.descriptor_set_layout);
        }

        // 销毁旧 IBL（不销毁共享 布局）后接管新的。
        self.ibl.destroy();
        self.ibl = new_ibl;
        Ok(())
    }

    pub fn register_mesh(&mut self, input: &MeshUploadInput) -> anyhow::Result<MeshHandle> {
        self.mesh_manager.register(
            &self.runtime.context,
            self.runtime.command_pool,
            self.runtime.context.graphics_queue,
            input,
        )
    }

    pub fn create_mesh(
        &self,
        vertices: &[Vertex],
        indices: Option<&[u32]>,
    ) -> anyhow::Result<crate::mesh::Mesh> {
        crate::mesh::Mesh::new(
            &self.runtime.context,
            self.runtime.command_pool,
            self.runtime.context.graphics_queue,
            vertices,
            indices,
        )
    }

    pub fn register_mesh_into(
        &mut self,
        uploader: &mut crate::batch::BatchUploader<'_>,
        input: &MeshUploadInput,
    ) -> anyhow::Result<MeshHandle> {
        self.mesh_manager
            .register_into(&self.runtime.context, uploader, input)
    }

    pub fn register_texture(
        &mut self,
        input: &TextureUploadInput,
    ) -> anyhow::Result<AssetTextureHandle> {
        self.texture_manager.reserve(
            &self.runtime.context,
            self.runtime.command_pool,
            self.runtime.context.graphics_queue,
            input,
        )
    }

    pub fn register_texture_into(
        &mut self,
        uploader: &mut crate::batch::BatchUploader<'_>,
        input: &TextureUploadInput,
    ) -> anyhow::Result<AssetTextureHandle> {
        self.texture_manager
            .reserve_into(&self.runtime.context, uploader, input)
    }

    pub fn register_material(
        &mut self,
        input: MaterialUploadInput,
    ) -> anyhow::Result<MaterialHandle> {
        self.material_manager.register(input)
    }

    pub fn texture_srv(&self, handle: AssetTextureHandle) -> crate::bindless::TextureHandle {
        self.texture_manager.get_srv(handle)
    }

    pub fn material_slot(&self, handle: MaterialHandle) -> Option<u32> {
        self.material_manager.slot_of(handle)
    }

    pub fn flush_materials(&mut self) -> anyhow::Result<()> {
        self.material_manager.upload(
            &self.runtime.context,
            self.runtime.command_pool,
            self.runtime.context.graphics_queue,
        )
    }

    pub fn mesh_manager(&self) -> &RenderMeshManager {
        &self.mesh_manager
    }

    /// GPU 侧资产解析：执行主线程准备好的上传请求，返回句柄。
    ///
    /// 在渲染线程调用（拥有 `&mut self`）。每个请求独立上传 mesh / 纹理 /
    /// 材质：纹理先上传取 bindless 槽，再据此组装 `MaterialUploadInput` 上传材质。
    /// 返回顺序与输入一致，调用方按实体配对写回 `World`。
    ///
    /// 输入为空时直接返回，避免无谓地创建 `BatchUploader` 命令缓冲。
    pub fn apply_asset_requests(
        &mut self,
        requests: Vec<crate::asset_bridge::AssetResolveRequest>,
    ) -> Vec<crate::asset_bridge::AssetResolveResult> {
        if requests.is_empty() {
            return Vec::new();
        }

        let ctx = self.context_arc();
        let cmd_pool = self.command_pool();
        let mut uploader = match crate::batch::BatchUploader::new(&ctx, cmd_pool) {
            Ok(u) => u,
            Err(e) => {
                log::error!("apply_asset_requests: BatchUploader::new failed: {e}");
                return Vec::new();
            }
        };

        let mut results = Vec::with_capacity(requests.len());
        for req in requests {
            let mesh_handle = match &req.mesh {
                Some(input) => self.register_mesh_into(&mut uploader, input).ok(),
                None => None,
            };

            let material_slot = match &req.material {
                Some(mat) => {
                    let mut slots: [Option<u32>; 5] = [None; 5];
                    for (i, tex) in mat.textures.iter().enumerate() {
                        if let Some(t) = tex {
                            if let Ok(h) = self.register_texture_into(&mut uploader, t) {
                                slots[i] = Some(self.texture_srv(h).0);
                            }
                        }
                    }
                    let s = mat.scalars;
                    let input = crate::managers::MaterialUploadInput {
                        base_color: [s[0], s[1], s[2], s[3]],
                        metallic: s[4],
                        roughness: s[5],
                        emissive: [s[6], s[7], s[8]],
                        albedo_tex: slots[0],
                        normal_tex: slots[1],
                        metallic_roughness_tex: slots[2],
                        emissive_tex: slots[3],
                        occlusion_tex: slots[4],
                        normal_scale: s[10],
                        occlusion_strength: s[11],
                        transmission: s[12],
                        ior: s[13],
                        translucency: s[14],
                        anisotropy: s[15],
                        clearcoat: s[16],
                        clearcoat_roughness: s[17],
                        emissive_strength: s[9],
                    };
                    self.register_material(input)
                        .ok()
                        .and_then(|h| self.material_slot(h))
                }
                None => None,
            };

            let (texture_slot, texture_size) = match &req.texture {
                Some(input) => {
                    let size = [input.width, input.height];
                    match self.register_texture_into(&mut uploader, input) {
                        Ok(handle) => (Some(self.texture_srv(handle).0), Some(size)),
                        Err(e) => {
                            log::warn!("apply_asset_requests: sprite texture upload failed: {e}");
                            (None, None)
                        }
                    }
                }
                None => (None, None),
            };

            results.push(crate::asset_bridge::AssetResolveResult {
                mesh_handle,
                material_slot,
                texture_slot,
                texture_size,
            });
        }

        if let Err(e) = uploader.finish(self.graphics_queue()) {
            log::error!("apply_asset_requests: BatchUploader::finish failed: {e}");
        }
        if let Err(e) = self.flush_materials() {
            log::warn!("apply_asset_requests: flush_materials failed: {e}");
        }

        results
    }

    /// Read-only 访问 to the 纹理 管理器 (owns the bindless 纹理
    /// 表 Used by the path-trace pass wiring to fetch the shared bindless
    /// 描述符 集合 + 布局
    pub fn texture_manager(&self) -> &RenderTextureManager {
        &self.texture_manager
    }

    /// Read-only 访问 to the 材质 管理器 (owns the `GpuMaterial[]`
    /// SSBO). Used by the path-trace pass to bind the materials SSBO.
    pub fn material_manager(&self) -> &RenderMaterialManager {
        &self.material_manager
    }

    /// 加载 a pre-parsed probe 音量 data directly (RM or 字节 path).
    ///
    /// Returns `true` if the 音量 was accepted (scene check + validity check
    /// passed, GPU upload succeeded).
    pub fn load_probe_volume_data(
        &mut self,
        data: crate::probe_loader::ProbeVolumeData,
        scene_name: Option<&str>,
    ) -> bool {
        // Scene 绑定 check: reject a 音量 baked for a different scene.
        if let Some(name) = scene_name {
            if !name.is_empty() && !data.scene_name.is_empty() && data.scene_name != name {
                log::warn!(
                    "GraphRenderer: baked GI data is for scene '{}', but loaded scene is \
                     '{}'; keeping synthetic sky field (rebake to apply)",
                    data.scene_name,
                    name
                );
                return false;
            }
        }

        // All-miss bake check.
        if data.global_hit_ratio >= 0.0 && data.global_hit_ratio < 0.05 {
            log::warn!(
                "GraphRenderer: baked GI data looks invalid (hit_ratio={:.3} < 0.05, \
                 all rays missed the TLAS); keeping synthetic sky field",
                data.global_hit_ratio
            );
            return false;
        }

        match self.scene_scope.from_probe_data(&data) {
            Ok(()) => {
                log::info!(
                    "GraphRenderer: loaded baked GI probe volume (dims {:?}, scene='{}', \
                     hit_ratio={:.3})",
                    data.dims,
                    data.scene_name,
                    data.global_hit_ratio
                );
                true
            }
            Err(e) => {
                log::warn!("GraphRenderer: failed to upload baked probe volume: {e:#}");
                false
            }
        }
    }

    /// 加载 a baked 全局光照 probe 音量 from a `.bin` file on disk.
    ///
    /// Convenience 包装器 around [`load_probe_volume_data`] for dev / loose
    /// files. The RM path should 调用 [`load_probe_volume_data`] directly
    /// with 字节 parsed via [`probe_loader::load_probe_volume_from_bytes`].
    pub fn load_probe_volume_file(
        &mut self,
        path: &std::path::Path,
        scene_name: Option<&str>,
    ) -> bool {
        let data = match crate::probe_loader::load_probe_volume(path) {
            Ok(d) => d,
            Err(e) => {
                log::info!(
                    "GraphRenderer: no baked GI at {} ({e}); keeping synthetic sky field",
                    path.display()
                );
                return false;
            }
        };
        self.load_probe_volume_data(data, scene_name)
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.swapchain
            .as_ref()
            .map(|s| s.extent)
            .unwrap_or_default()
    }

    pub fn orientation(&self) -> (f32, [[f32; 4]; 4]) {
        use vk::SurfaceTransformFlagsKHR as T;
        let extent = self.extent();
        let transform = self
            .swapchain
            .as_ref()
            .map(|s| s.pre_transform())
            .unwrap_or(T::IDENTITY);
        let portrait_buffer = extent.width < extent.height;
        let (dw, dh) = if portrait_buffer {
            (extent.height, extent.width)
        } else {
            (extent.width, extent.height)
        };
        let angle = match transform {
            T::ROTATE_90 => std::f32::consts::FRAC_PI_2,
            T::ROTATE_270 => -std::f32::consts::FRAC_PI_2,
            T::ROTATE_180 => std::f32::consts::PI,
            _ => 0.0,
        };
        let aspect = if dh == 0 { 1.0 } else { dw as f32 / dh as f32 };
        let (s, c) = angle.sin_cos();
        let rotation = [
            [c, s, 0.0, 0.0],
            [-s, c, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        (aspect, rotation)
    }

    pub fn has_swapchain(&self) -> bool {
        self.swapchain.is_some()
    }

    /// Pre‑compile all lazy‑created pipelines so the 第一个 帧 does not
    /// stall on 管线 creation. 调用 once after construction, before any
    /// [`execute`](Self::execute).
    pub fn warmup_pipelines(&mut self) -> anyhow::Result<()> {
        let device = self.runtime.context.device.clone();
        self.graph.warmup_passes(&device, &self.runtime.context)
    }

    pub fn suspend_surface(&mut self) {
        if self.is_headless {
            return;
        }
        let device = &self.runtime.context.device;
        unsafe { device.device_wait_idle() }.ok();
        if let Some(mut sw) = self.swapchain.take() {
            unsafe { sw.destroy(device) };
        }
        log::info!("GraphRenderer suspended");
    }

    pub fn resume_surface(
        &mut self,
        window: &dyn raw_window_handle::HasDisplayHandle,
        window_handle: &dyn raw_window_handle::HasWindowHandle,
    ) -> anyhow::Result<()> {
        if self.is_headless || self.swapchain.is_some() {
            return Ok(());
        }
        let swapchain =
            Swapchain::new_with_vsync(&self.runtime.context, window, window_handle, self.vsync)?;
        // 恢复路径与启动路径同样需要同步 ForwardPass 的 per-image 槽位数
        // （见 `GraphRenderer::new` 里的注释）。
        let image_count = swapchain.views.len();
        self.swapchain = Some(swapchain);
        if let Some(forward_pass) = self.graph.pass_mut::<ForwardPass>() {
            forward_pass.set_image_count(image_count);
        }
        log::info!("GraphRenderer resumed");
        Ok(())
    }

    pub fn recreate_swapchain(&mut self) -> anyhow::Result<()> {
        if self.is_headless {
            return Ok(());
        }
        // Wait for the GPU to finish all in-flight 功 BEFORE destroying any
        // framebuffers. The 上一个 frame's 命令 缓冲区 references both
        // the ForwardPass framebuffers and the external overlay framebuffers; without
        // this wait, vkDestroyFramebuffer fires while a 命令 缓冲区 is still
        // executing (VUID-vkDestroyFramebuffer-framebuffer-00892).
        unsafe { self.runtime.context.device.device_wait_idle() }
            .context("recreate_swapchain: device_wait_idle")?;

        // 放置 the ForwardPass 帧缓冲 + 深度 图像 BEFORE the 交换链 is
        // recreated: the 帧缓冲 wraps a 交换链 图像 视图 and
        // `Swapchain::recreate` destroys the old views. Destroying the views
        // while the 帧缓冲 still references them triggers a 验证
        // 错误 (VUID-vkDestroyImageView-imageView-01026) which cascades into a
        // device-lost on the 下一个 队列 submit.
        //
        // This is the single entry point for 交换链 recreation - the
        // acquire/present out-of-date paths in 渲染 also route through
        // here so the 帧缓冲 is always torn 下 第一个
        if let Some(forward_pass) = self.graph.pass_mut::<ForwardPass>() {
            forward_pass.drop_target(&self.runtime.context.device);
        }
        // PostPass wraps 交换链 views too (its framebuffers 目标 the
        // 交换链 directly). 放置 them on the same lifecycle.
        if let Some(post) = self.graph.pass_mut::<crate::post::PostPass>() {
            post.drop_target(&self.runtime.context.device);
        }
        // ECS UI 叠加按 image view 缓存 framebuffer；必须在 `sw.recreate()`
        // 销毁旧 image view **之前**释放，否则 framebuffer 会引用已销毁的
        // view（VUID-vkDestroyImageView-imageView-01026），可导致验证错误/
        // device lost。此处 device 已 idle。
        #[cfg(feature = "ui")]
        if let Some(gpu) = self.ui_overlay.as_mut() {
            gpu.on_swapchain_views_changed();
        }

        if let Some(sw) = self.swapchain.as_mut() {
            sw.recreate(&self.runtime.context)?;
        }

        // 通知外部叠加层：per-swapchain framebuffers 已按新 views 重建。
        if let Some(sw) = self.swapchain.as_ref() {
            if let Some(overlay) = self.external_overlay.as_mut() {
                overlay.on_swapchain_views_changed(&sw.views, sw.extent);
            }
        }

        // 依赖交换链尺寸/图像数的 pass 必须在 `sw.recreate()` **之后**按新值
        // resize：recreate 之前 `sw.extent` 与 `sw.views.len()` 还是旧交换链的，
        // `GtaoPass::recreate_target` 拿旧的半分辨率做早退（old/2 == 旧 extent），
        // 而 `set_image_count` 会让 `ForwardPass::ensure_target` 缺少新图像对应的
        // 槽位（`image_index >= image_count` 直接 return）。
        if let Some((extent, image_count)) = self
            .swapchain
            .as_ref()
            .map(|sw| (sw.extent, sw.views.len()))
        {
            if let Some(forward_pass) = self.graph.pass_mut::<ForwardPass>() {
                forward_pass.set_image_count(image_count);
            }
            // GTAO 拥有自己的 AO 图像（不来自交换链），但尺寸是交换链的一半。
            #[cfg(feature = "gtao")]
            if let Some(gtao) = self.graph.pass_mut::<crate::gtao::GtaoPass>() {
                if let Err(e) =
                    gtao.recreate_target(&self.runtime.context, self.runtime.command_pool, extent)
                {
                    log::warn!("GtaoPass recreate_target failed: {e:#}");
                }
            }
            // CRT 调度器的 RenderTextures 跟随 swapchain 尺寸（演示 RT 全屏）。
            if let Some(sched) = self
                .graph
                .pass_mut::<crate::rt_scheduler::RenderTextureScheduler>()
            {
                if let Err(e) = sched.resize_all(
                    &self.runtime.context,
                    self.texture_manager.bindless_mut(),
                    extent,
                ) {
                    log::warn!("RenderTextureScheduler resize_all failed: {e:#}");
                }
            }
        }

        // All per-swapchain-image attachments (ForwardPass HDR/depth/normal,
        // PostPass 帧缓冲 were just rebuilt, so the 渲染 graph's cached
        // 图像 layouts are stale. 清空 them so the 第一个 帧 after
        // recreate re-transitions from UNDEFINED instead of trusting a 布局
        // that no longer matches the fresh images.
        self.graph.reset_layouts();
        Ok(())
    }

    /// 调整**离屏**渲染目标尺寸（编辑器视口面板改大小时走这里）。
    ///
    /// 顺序与 [`Self::recreate_swapchain`] 一致：先 `device_wait_idle`，再拆掉
    /// 引用旧视图的 framebuffer，然后换目标、按新尺寸重建依赖 pass，最后清掉
    /// 图缓存里的 layout（新图像从 UNDEFINED 重新过渡）。
    pub fn resize_offscreen(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        let Some(target) = self.offscreen.as_ref() else {
            return Ok(());
        };
        if target.extent().width == width && target.extent().height == height {
            return Ok(());
        }

        unsafe { self.runtime.context.device.device_wait_idle() }
            .context("resize_offscreen: device_wait_idle")?;

        if let Some(forward_pass) = self.graph.pass_mut::<ForwardPass>() {
            forward_pass.drop_target(&self.runtime.context.device);
        }
        if let Some(post) = self.graph.pass_mut::<crate::post::PostPass>() {
            post.drop_target(&self.runtime.context.device);
        }
        // UI 叠加按 image view 缓存 framebuffer，必须先于旧视图销毁释放。
        #[cfg(feature = "ui")]
        {
            if let Some(gpu) = self.ui_overlay.as_mut() {
                gpu.on_swapchain_views_changed();
            }
            if let Some(gpu) = self.ui_overlay_offscreen.as_mut() {
                gpu.on_swapchain_views_changed();
            }
        }

        let context = self.runtime.context.clone();
        let fresh = OffscreenTarget::new(&context, width, height)?;
        let extent = fresh.extent();
        if let Some(mut old) = self.offscreen.replace(fresh) {
            unsafe { old.destroy(&context.device) };
        }
        let view = self
            .offscreen
            .as_ref()
            .map(|target| target.image_view())
            .unwrap_or(vk::ImageView::null());

        if let Some(forward_pass) = self.graph.pass_mut::<ForwardPass>() {
            forward_pass.set_image_count(1);
            forward_pass.set_target(&context.device, &context, 1, 0, extent)?;
        }
        if let Some(post) = self.graph.pass_mut::<crate::post::PostPass>() {
            post.set_target(&context.device, std::slice::from_ref(&view), 0, extent)?;
        }
        #[cfg(feature = "gtao")]
        if let Some(gtao) = self.graph.pass_mut::<crate::gtao::GtaoPass>() {
            if let Err(e) = gtao.recreate_target(&context, self.runtime.command_pool, extent) {
                log::warn!("GtaoPass recreate_target failed (offscreen resize): {e:#}");
            }
        }
        if let Some(sched) = self
            .graph
            .pass_mut::<crate::rt_scheduler::RenderTextureScheduler>()
        {
            if let Err(e) = sched.resize_all(&context, self.texture_manager.bindless_mut(), extent)
            {
                log::warn!("RenderTextureScheduler resize_all failed (offscreen resize): {e:#}");
            }
        }

        self.graph.reset_layouts();
        Ok(())
    }
    // -------------------------------------------------------------------
    // 帧 lifecycle — phase API
    // -------------------------------------------------------------------

    /// Phase 1/3: acquire 交换链 图像 reset & 开始 the 命令 缓冲区
    ///
    /// Returns a [`FrameCtx`] carrying the per-frame Vulkan handles. On
    /// 交换链 out-of-date returns `Ok(None)` — the 调用者 should return
    /// early (the 交换链 was recreated internally). On real 错误 returns
    /// `Err`.
    ///
    /// In headless 众数 always returns the 命令 缓冲区 from the 运行时
    /// 池 (no acquire needed).
    pub fn begin_frame(&mut self) -> anyhow::Result<Option<FrameCtx>> {
        let device = self.runtime.context.device.clone();

        // Headless: no acquire, just grab cmd buf[0].
        if self.is_headless {
            if self.headless_fence != vk::Fence::null() {
                unsafe {
                    device
                        .wait_for_fences(&[self.headless_fence], true, u64::MAX)
                        .context("wait for headless frame fence")?;
                    device
                        .reset_fences(&[self.headless_fence])
                        .context("reset headless frame fence")?;
                }
            }

            self.begin_gpu_timings(&device, 0);

            let cmd = self.runtime.command_buffers[0];
            unsafe { device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()) }
                .context("reset command buffer (headless)")?;
            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            unsafe { device.begin_command_buffer(cmd, &begin_info) }
                .context("begin command buffer (headless)")?;

            // Re-use the offscreen 目标 extent as a stand-in.
            let extent = self
                .offscreen
                .as_ref()
                .map(|o| o.extent())
                .unwrap_or(vk::Extent2D {
                    width: 256,
                    height: 256,
                });

            return Ok(Some(FrameCtx {
                device,
                cmd,
                image_index: 0,
                frame_index: 0,
                extent,
                fence: self.headless_fence,
                image_available: vk::Semaphore::null(),
                render_finished: vk::Semaphore::null(),
            }));
        }
        let device = self.runtime.context.device.clone();

        // --- Acquire 下一个 图像 ---
        let (image_index, frame, image_available, render_finished, fence) = match self
            .swapchain
            .as_mut()
            .context("begin_frame called with no swapchain")?
            .acquire_next_image(&device)
        {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("out of date") {
                    log::debug!("acquire out of date, recreating");
                    self.recreate_swapchain()?;
                    return Ok(None);
                }
                return Err(e);
            }
        };

        let cmd = self.runtime.command_buffers[frame];
        let extent = self.extent();

        // 上一帧（同一帧槽位）的时间戳结果现在一定写好了（acquire 已等过栅栏）。
        self.begin_gpu_timings(&device, frame as u32);

        // --- Reset & 开始 命令 缓冲区 ---
        unsafe { device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()) }
            .context("reset command buffer")?;
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { device.begin_command_buffer(cmd, &begin_info) }.context("begin command buffer")?;

        Ok(Some(FrameCtx {
            device,
            cmd,
            image_index,
            frame_index: frame as u32,
            extent,
            fence,
            image_available,
            render_finished,
        }))
    }

    /// 读回该帧槽位上一轮的时间戳，并清空它给本帧用（#518）。
    fn begin_gpu_timings(&mut self, device: &ash::Device, frame_index: u32) {
        let Some(timer) = self.gpu_timer.as_mut() else {
            return;
        };
        timer.begin_frame(device, frame_index);
        self.gpu_timings = timer.timings(frame_index).clone();
    }

    /// 最近一帧的 per-pass GPU 时间。
    ///
    /// `supported = false`（或 `passes` 为空）表示设备不支持时间戳，调用方应回退到
    /// CPU 墙钟（`frame_time_ms`）。结果比当前帧**晚一帧**：Vulkan 的时间戳只在
    /// 提交完成后才可读，延迟读回才不会 stall 管线。
    pub fn gpu_timings(&self) -> &crate::gpu_timer::GpuTimings {
        &self.gpu_timings
    }

    /// 准备阶段：在命令录制前批量同步 CPU 场景状态和 GPU 资源。
    /// 当前实现：校验输入 + 日志脏标记，为下一步扩展 DirtyDispatch 预留上传点。
    pub fn prepare(&mut self, _ctx: &FrameCtx, input: &FrameInput<'_>) -> anyhow::Result<()> {
        // 未来在此消费 DirtyFlags 并调度 RenderTextureManager / RenderMeshManager 等批量上传
        // 当前仅做输入校验与同步点预留，保证五阶段调用链完整（DESIGN §8）
        if input.pt_accum_dirty {
            log::trace!("prepare: pt accum dirty — will reset accumulation");
        }
        if input.draw_items.is_empty() && !input.has_camera {
            log::trace!("prepare: no draw_items and no camera — frame will clear only");
        }
        Ok(())
    }

    /// Phase 2/3: record all 渲染 commands into the frame's 命令 缓冲区
    ///
    /// Updates the per-frame UBO, builds the [`GraphFrame`], executes the
    /// 渲染 图 (ShadowMap → Scene → GTAO → Post), records the external
    /// 叠加 if present (or inserts the swapchain-layout 屏障 and ends
    /// the 命令 缓冲区
    ///
    /// Recording errors are captured and returned, but the 命令 缓冲区 is
    /// **always ended** — even on 失败 — so that [`present`] can submit a
    /// 部分 缓冲区 and keep the in-flight 围栏 signaled.
    pub fn execute(&mut self, ctx: &FrameCtx, input: &FrameInput<'_>) -> anyhow::Result<()> {
        let device = &ctx.device;
        let cmd = ctx.cmd;
        let frame = ctx.frame_index as usize;
        let image_index = ctx.image_index;
        let extent = ctx.extent;

        let FrameInput {
            draw_items,
            gizmos,
            sprite_items,
            particle_items,
            skybox,
            frame_data,
            light_view_proj,
            inv_projection,
            debug_flags,
            tonemap_mode,
            aa_mode,
            debug_rt,
            grade,
            proj22,
            proj32,
            lights,
            render_mode,
            pt_max_bounces,
            pt_ray_max_distance,
            pt_max_iterations,
            exposure,
            pt_lights,
            pt_accum_dirty,
            has_camera,
            camera_orthographic,
            clear_color,
            shadows_enabled,
            ui_overlay,
            show_grid,
        } = input;
        #[cfg(not(feature = "ui"))]
        let _ = ui_overlay;
        let light_view_proj = *light_view_proj;
        let inv_projection = *inv_projection;
        let debug_flags = *debug_flags;
        let tonemap_mode = *tonemap_mode;
        let aa_mode = *aa_mode;
        let debug_rt = *debug_rt;
        let proj22 = *proj22;
        let proj32 = *proj32;

        // Record into a 结果 rather than `?`-propagating: if any step
        // fails we still must `end_command_buffer` below so the in-flight
        // 围栏 gets signaled in `present`. Otherwise the 下一个 frame's
        // `wait_for_fences` would hang forever.
        let mut record: anyhow::Result<()> = Ok(());

        // --- 更新 帧 UBO ---
        if record.is_ok() {
            record = self.runtime.frame_ubos[frame]
                .update(device, frame_data)
                .context("update frame UBO");
        }

        // --- 执行 渲染 图 (Shadow -> Scene -> GTAO -> Post) ---
        if record.is_ok() {
            let ao_view = self.ao_view(frame as u32);
            let offscreen_views = self.offscreen.as_ref().map(|o| [o.image_view()]);
            let swapchain_views: &[vk::ImageView] = match (&self.swapchain, &offscreen_views) {
                (Some(sw), _) => sw.views.as_slice(),
                (None, Some(ov)) => ov.as_slice(),
                (None, None) => &[],
            };
            let graph_frame = GraphFrame {
                frame_ubo: &self.runtime.frame_ubos[frame],
                draw_list: draw_items,
                gizmos,
                sprite_items,
                particle_items,
                skybox: *skybox,
                mesh_manager: &self.mesh_manager,
                light_view_proj,
                // 场景里没有投阴影的方向光时整帧跳过阴影 pass。
                shadow_mode: if *shadows_enabled {
                    self.settings.shadow_mode
                } else {
                    ShadowMode::None
                },
                debug_flags,
                grade: grade.sanitized(),
                inv_view_rot: {
                    let v = &frame_data.view;
                    let mut m = [[0.0f32; 4]; 4];
                    for c in 0..3 {
                        for r in 0..3 {
                            m[c][r] = v[r][c];
                        }
                    }
                    m[3][3] = 1.0;
                    m
                },
                view_proj: frame_data.view_proj,
                lights,
                ao_view,
                tonemap_mode,
                aa_mode,
                debug_rt,
                proj22,
                proj32,
                inv_projection,
                swapchain_views,
                render_mode: *render_mode,
                pt_max_bounces: *pt_max_bounces,
                pt_ray_max_distance: *pt_ray_max_distance,
                pt_max_iterations: *pt_max_iterations,
                camera_pos: frame_data.camera_position,
                light_dir: frame_data.light_direction,
                light_color: frame_data.light_color,
                exposure: *exposure,
                pt_lights,
                pt_accum_dirty: *pt_accum_dirty,
                has_camera: *has_camera,
                camera_orthographic: *camera_orthographic,
                clear_color: *clear_color,
                show_grid: *show_grid,
            };
            let render_ctx = crate::render_graph::RenderContext {
                device,
                context: &self.runtime.context,
                settings: &self.settings,
                cmd,
                frame_index: frame as u32,
                image_index,
                extent,
                frame: &graph_frame,
            };
            record = self
                .graph
                .execute_with_timer(&render_ctx, self.gpu_timer.as_mut())
                .context("graph execute");
        }

        // --- ECS UI 叠加 (quads + fontdue glyph text) ---
        //
        // Recorded between the 渲染 图 output and the external overlay
        // (or the explicit PRESENT 屏障). For the swapchain path its render
        // pass owns COLOR_ATTACHMENT_OPTIMAL → PRESENT_SRC_KHR; for the
        // offscreen path it stays in COLOR_ATTACHMENT_OPTIMAL so the host can
        // sample the image after `offscreen_present_for_sampling` (#597).
        //
        // 图 末尾 pass（RtPreview）final = PRESENT_SRC_KHR，所以 swapchain
        // image 在 graph 后已是展示布局；UI/overlay render pass 期望
        // COLOR_ATTACHMENT_OPTIMAL 初始布局 → 各自 record 前都要显式转回。
        #[cfg(feature = "ui")]
        if record.is_ok() {
            let has_ui = ui_overlay.is_some_and(|o| !o.quads.is_empty() || !o.texts.is_empty());
            if has_ui {
                // UI 背景图（#591）从场景共享的 bindless 表取样。
                let bindless_set = self.texture_manager.bindless().set;
                let bindless_layout = self.texture_manager.bindless().layout;
                if let Some(sw) = self.swapchain.as_ref() {
                    // The overlay render pass must use the swapchain's actual
                    // color format; surfaces fall back to `available[0]`
                    // (often `R8G8B8A8_*` on Android) when the preferred
                    // `B8G8R8A8_SRGB` is unavailable.
                    let color_format = sw.format.format;
                    if self.ui_overlay.is_none() {
                        let mut gpu = crate::ui_overlay::UiOverlay::with_color_format(
                            &self.runtime.context,
                            self.runtime.command_pool,
                            color_format,
                            bindless_layout,
                        )
                        .context("create ui overlay")?;
                        apply_ui_fonts(&self.ui_fonts, &mut gpu);
                        self.ui_overlay = Some(gpu);
                    }
                    if let Some(gpu) = self.ui_overlay.as_mut() {
                        gpu.set_color_format(color_format)
                            .context("ui overlay color format")?;
                        // PostPass leaves the swapchain image in PRESENT; turn it
                        // back into a color attachment for the UI load pass.
                        let image = sw.images[image_index as usize];
                        let barrier = vk::ImageMemoryBarrier::default()
                            .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
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
                            .src_access_mask(vk::AccessFlags::NONE)
                            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
                        unsafe {
                            device.cmd_pipeline_barrier(
                                cmd,
                                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                vk::DependencyFlags::empty(),
                                &[],
                                &[],
                                std::slice::from_ref(&barrier),
                            );
                        }
                        record = gpu
                            .record(
                                &self.runtime.context,
                                cmd,
                                extent,
                                sw.views[image_index as usize],
                                ui_overlay.unwrap(),
                                bindless_set,
                            )
                            .context("ui overlay record");
                    }
                } else if let Some((image, view, target_extent, color_format)) =
                    self.offscreen.as_ref().map(|target| {
                        (
                            target.image(),
                            target.image_view(),
                            target.extent(),
                            target.format(),
                        )
                    })
                {
                    // Headless / editor viewport: no swapchain, so the UI pass
                    // must end in COLOR_ATTACHMENT_OPTIMAL and use its own
                    // UiOverlay instance (different final layout).
                    if self.ui_overlay_offscreen.is_none() {
                        let mut gpu =
                            crate::ui_overlay::UiOverlay::with_color_format_and_final_layout(
                                &self.runtime.context,
                                self.runtime.command_pool,
                                color_format,
                                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                                bindless_layout,
                            )
                            .context("create offscreen ui overlay")?;
                        apply_ui_fonts(&self.ui_fonts, &mut gpu);
                        self.ui_overlay_offscreen = Some(gpu);
                    }
                    if let Some(gpu) = self.ui_overlay_offscreen.as_mut() {
                        gpu.set_color_format(color_format)
                            .context("offscreen ui overlay color format")?;
                        // The graph already leaves the target in
                        // COLOR_ATTACHMENT_OPTIMAL; this barrier only makes
                        // its color writes visible to the UI pass.
                        let barrier = vk::ImageMemoryBarrier::default()
                            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
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
                            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                            .dst_access_mask(
                                vk::AccessFlags::COLOR_ATTACHMENT_READ
                                    | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                            );
                        unsafe {
                            device.cmd_pipeline_barrier(
                                cmd,
                                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                vk::DependencyFlags::empty(),
                                &[],
                                &[],
                                std::slice::from_ref(&barrier),
                            );
                        }
                        record = gpu
                            .record(
                                &self.runtime.context,
                                cmd,
                                target_extent,
                                view,
                                ui_overlay.unwrap(),
                                bindless_set,
                            )
                            .context("offscreen ui overlay record");
                    }
                }
            }
        }

        // --- 过渡 交换链 图像 to PRESENT_SRC_KHR ---
        //
        // 外部叠加层（宿主 UI）在 ECS UI 之后 record：image 此时
        // 已是 PRESENT_SRC_KHR（graph 末尾 RtPreview 或 ECS UI 的 final
        // layout），叠加 render pass 期望 COLOR_ATTACHMENT_OPTIMAL 初始
        // 布局 → record 前显式转回。无叠加时无需任何转换。
        if record.is_ok() {
            let overlay_has_content = self
                .external_overlay
                .as_ref()
                .is_some_and(|o| o.has_content());
            if overlay_has_content {
                if let Some(sw) = self.swapchain.as_ref() {
                    let image = sw.images[image_index as usize];
                    let barrier = vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
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
                        .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
                    unsafe {
                        device.cmd_pipeline_barrier(
                            cmd,
                            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                            vk::DependencyFlags::empty(),
                            &[],
                            &[],
                            std::slice::from_ref(&barrier),
                        );
                    }
                }
                if let Some(sw) = self.swapchain.as_ref() {
                    if let Some(overlay) = self.external_overlay.as_mut() {
                        let oc = crate::external_overlay::OverlayRecordCtx {
                            device,
                            context: &self.runtime.context,
                            command_pool: self.runtime.command_pool,
                            graphics_queue: self.runtime.context.graphics_queue,
                            cmd,
                            swapchain_views: &sw.views,
                            image_index,
                            extent,
                            color_format: self.color_format,
                        };
                        record = overlay.record(&oc).context("external overlay record");
                    }
                } else {
                    record = Err(anyhow::anyhow!("external overlay: swapchain missing"));
                }
            }
            // 无叠加：image 已是 PRESENT_SRC_KHR，即展示布局，无需转换。
        }

        // --- 截图：交换链图像 → host-visible 缓冲区 ---
        //
        // 必须放在图像回到 PRESENT_SRC_KHR 之后、`end_command_buffer` 之前：
        // 拷贝本身需要 TRANSFER_SRC_OPTIMAL，记录完再转回展示布局。
        if record.is_ok() {
            if let Some(request) = self.pending_screenshot.take() {
                let capture = crate::screenshot::ScreenshotCapture::new(
                    &self.runtime.context,
                    extent,
                    request,
                );
                match capture {
                    Ok(capture) => {
                        let image = self
                            .swapchain
                            .as_ref()
                            .map(|sw| sw.images[image_index as usize]);
                        match image {
                            Some(image) => match capture.record_copy(device, cmd, image) {
                                Ok(()) => self.screenshot = Some(capture),
                                Err(err) => record = Err(err),
                            },
                            None => record = Err(anyhow::anyhow!("screenshot: no swapchain")),
                        }
                    }
                    Err(err) => record = Err(err),
                }
            }
        }

        // --- 结束 命令 缓冲区 (always attempted) ---
        if let Err(end_err) = unsafe { device.end_command_buffer(cmd) } {
            if record.is_ok() {
                record = Err(anyhow::anyhow!("end command buffer: {end_err:?}"));
            }
        }

        record
    }

    /// Phase 3/3: submit the recorded 命令 缓冲区 and present to the
    /// 交换链
    ///
    /// Runs **regardless** of whether 执行 returned an 错误 — the
    /// in-flight 围栏 (reset during [`begin_frame`]) must be signaled so the
    /// 下一个 帧 does not hang. Returns `true` when the 交换链 was
    /// recreated (out-of-date on present).
    /// Phase 3/3: submit and present. In headless 众数 this is a no-op
    /// (the offscreen 目标 owns its own command-submission path).
    pub fn present(&mut self, ctx: &FrameCtx) -> anyhow::Result<bool> {
        // Headless: just submit with a fence-out but skip present.
        if self.is_headless {
            let cmd_bufs = [ctx.cmd];
            let submit = vk::SubmitInfo::default().command_buffers(&cmd_bufs);
            unsafe {
                self.runtime.context.device.queue_submit(
                    self.runtime.context.graphics_queue,
                    &[submit],
                    ctx.fence,
                )
            }
            .context("headless queue submit")?;
            if ctx.fence != vk::Fence::null() {
                unsafe {
                    self.runtime
                        .context
                        .device
                        .wait_for_fences(&[ctx.fence], true, u64::MAX)
                }
                .context("headless wait for fence")?;
            }
            return Ok(false);
        }
        let wait_semaphores = [ctx.image_available];
        let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let signal_semaphores = [ctx.render_finished];
        let cmd_bufs = [ctx.cmd];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_semaphores)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&cmd_bufs)
            .signal_semaphores(&signal_semaphores);
        unsafe {
            ctx.device
                .queue_submit(self.runtime.context.graphics_queue, &[submit], ctx.fence)
        }
        .context("queue submit")?;

        let out_of_date = self
            .swapchain
            .as_mut()
            .context("present: no swapchain")?
            .present(
                self.runtime.context.graphics_queue,
                ctx.image_index,
                ctx.render_finished,
            )?;

        if out_of_date {
            log::debug!("present out of date, recreating");
            self.recreate_swapchain()?;
        }

        Ok(out_of_date)
    }

    /// 帧结束阶段：推进帧级状态并执行延迟回收。
    pub fn end_frame(&mut self, _ctx: &FrameCtx, _presented: bool) -> anyhow::Result<()> {
        Ok(())
    }

    /// 渲染 a 帧 one-shot convenience that calls [`begin_frame`],
    /// 执行 and [`present`] in order.
    ///
    /// This is a 兼容性 包装器 new 代码 should prefer the explicit
    /// phase API for finer 错误 handling and future prepare-stage insertion.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        draw_items: &[DrawItem],
        frame_data: &FrameUBOData,
        light_view_proj: [[f32; 4]; 4],
        inv_projection: [[f32; 4]; 4],
        debug_flags: u32,
        tonemap_mode: u32,
        aa_mode: u32,
        debug_rt: u32,
        proj22: f32,
        proj32: f32,
        lights: &[GpuLight],
        render_mode: RenderMode,
        pt_max_bounces: u32,
        pt_ray_max_distance: f32,
        pt_max_iterations: u32,
        exposure: f32,
        pt_lights: &[PtAnalyticLight],
        grade: crate::render_graph::PostGrade,
    ) -> anyhow::Result<bool> {
        let ctx = match self.begin_frame()? {
            Some(c) => c,
            None => return Ok(false),
        };
        let input = FrameInput {
            draw_items,
            gizmos: &[],
            sprite_items: &[],
            particle_items: &[],
            skybox: None,
            frame_data,
            light_view_proj,
            inv_projection,
            debug_flags,
            tonemap_mode,
            aa_mode,
            debug_rt,
            grade,
            proj22,
            proj32,
            lights,
            render_mode,
            pt_max_bounces,
            pt_ray_max_distance,
            pt_max_iterations,
            exposure,
            pt_lights,
            pt_accum_dirty: true,
            has_camera: true,
            // The convenience wrapper has no camera entity to inspect and
            // always drove a perspective projection before, so keep that.
            camera_orthographic: false,
            clear_color: [0.5, 0.5, 0.5, 1.0],
            shadows_enabled: true,
            ui_overlay: None,
            show_grid: false,
        };
        self.prepare(&ctx, &input)?;
        let exec_result = self.execute(&ctx, &input);
        let out_of_date = self.present(&ctx)?;
        exec_result?; // propagate recording error after fence is safe
        self.end_frame(&ctx, out_of_date)?;
        Ok(out_of_date)
    }

    /// 释放 all GPU resources.
    /// 预约一次截图：下一帧 `execute` 把交换链图像拷贝到读回缓冲区，
    /// `finish_screenshot` 再写成 PNG。
    ///
    /// `size` 为 `None` 时与窗口分辨率一致；给了尺寸就最近邻缩放
    /// （像素画放大不会糊）。
    pub fn request_screenshot(
        &mut self,
        path: impl Into<std::path::PathBuf>,
        size: Option<[u32; 2]>,
        scale: Option<f32>,
    ) {
        self.pending_screenshot = Some(crate::screenshot::ScreenshotRequest {
            path: path.into(),
            size,
            scale,
        });
    }

    /// 帧提交之后调用：等待本次提交的围栏、读回像素、写 PNG。
    ///
    /// 没有进行中的截图时返回 `Ok(None)`。
    pub fn finish_screenshot(
        &mut self,
        ctx: &FrameCtx,
    ) -> anyhow::Result<Option<std::path::PathBuf>> {
        let Some(capture) = self.screenshot.take() else {
            return Ok(None);
        };
        if ctx.fence != vk::Fence::null() {
            unsafe {
                self.runtime
                    .context
                    .device
                    .wait_for_fences(&[ctx.fence], true, u64::MAX)
            }
            .context("screenshot: wait for fence")?;
        }
        let result = capture.finish(&self.runtime.context, self.color_format);
        let mut capture = capture;
        unsafe { capture.destroy(&self.runtime.context.device) };
        result.map(Some)
    }

    pub fn destroy(&mut self) {
        let device = &self.runtime.context.device;
        unsafe { device.device_wait_idle() }.ok();

        if let Some(mut capture) = self.screenshot.take() {
            unsafe { capture.destroy(device) };
        }

        if let Some(timer) = self.gpu_timer.as_mut() {
            timer.destroy(device);
        }

        // 销毁 IBL resources (env/irradiance/prefiltered cubes, BRDF LUT).
        // 共享的 set2 描述符 布局 由 GraphRenderer 持有，此处单独销毁
        // （在 ibl.destroy() 把字段置空前取出句柄）。
        let ibl_ds_layout = self.ibl.descriptor_set_layout;
        self.ibl.destroy();
        if ibl_ds_layout != vk::DescriptorSetLayout::null() {
            unsafe { device.destroy_descriptor_set_layout(ibl_ds_layout, None) };
        }

        // 销毁 scene managers.
        self.material_manager.destroy(device);
        self.texture_manager.destroy();
        self.mesh_manager.destroy(device);

        // 销毁 ForwardPass (framebuffers, 深度 images, 渲染 pass
        // 管线 shadow 描述符 集合 Without this, vkDestroyDevice
        // reports leaked VkImage/VkDeviceMemory/VkImageView/VkRenderPass.
        if let Some(forward_pass) = self.graph.pass_mut::<ForwardPass>() {
            forward_pass.destroy(device);
        }

        // 销毁 scene-level 全局光照 probe 音量 (SceneScope). Must happen AFTER
        // ForwardPass::destroy (ForwardPass borrows the 描述符 集合
        self.scene_scope.destroy();

        // 销毁 GTAO pass 环境光遮蔽 images, 渲染 pass 管线 描述符
        // sets, 采样器
        #[cfg(feature = "gtao")]
        if let Some(gtao) = self.graph.pass_mut::<crate::gtao::GtaoPass>() {
            gtao.destroy(device);
        }
        #[cfg(not(feature = "gtao"))]
        self.dummy_ao.destroy(device);

        // 销毁 PostPass (framebuffers, 渲染 pass 管线 描述符
        // 集合 采样器
        if let Some(post) = self.graph.pass_mut::<crate::post::PostPass>() {
            post.destroy(device);
        }

        // 销毁 PathTracePass (accumulation images, geometry buffers,
        // BLAS/TLAS, 描述符 集合 布局 池
        #[cfg(feature = "pathtrace")]
        if let Some(pt) = self.graph.pass_mut::<PathTracePass>() {
            pt.destroy(device);
        }

        // 销毁 ShadowMapPass 帧缓冲 渲染 pass pipeline/layout).
        // This MUST happen BEFORE scene_scope/graph destruction (which owns
        // the Arc<VulkanContext>) because Rust field-drop order drops the
        // context-holders (runtime/ibl/scene_scope) *before* the 图 — if
        // ShadowMapPass relied on its 放置 alone, the 设备 handle would be
        // stale by the 时间 it ran, causing leaked resources + 访问 violation.
        #[cfg(feature = "shadows")]
        if let Some(shadow) = self
            .graph
            .pass_mut::<crate::shadow_map_pass::ShadowMapPass>()
        {
            shadow.destroy(device);
        }
        // `shadows` 关闭时没有 ShadowMapPass，销毁 1×1 dummy 深度图。
        #[cfg(not(feature = "shadows"))]
        self.dummy_shadow.destroy(device);

        // 销毁外部叠加层（render pass/framebuffers/渲染器）。
        if let Some(overlay) = self.external_overlay.as_mut() {
            overlay.destroy();
        }

        // 销毁 shadow 采样器
        unsafe { device.destroy_sampler(self.shadow_sampler, None) };

        // 销毁 图 resources (shadow 映射表 images, etc.).
        self.graph.destroy(device);

        // 销毁 命令 池
        unsafe { device.destroy_command_pool(self.runtime.command_pool, None) };

        // 销毁 交换链
        if let Some(mut sw) = self.swapchain.take() {
            unsafe { sw.destroy(device) };
        }

        // 销毁 offscreen 目标 (headless 众数
        if let Some(mut target) = self.offscreen.take() {
            unsafe { target.destroy(device) };
        }

        if self.headless_fence != vk::Fence::null() {
            unsafe { device.destroy_fence(self.headless_fence, None) };
            self.headless_fence = vk::Fence::null();
        }
    }
}

// 安全性 UI 状态已移出渲染线程（主线程持有），
// 渲染线程上剩余字段只持有 ash/Vulkan handles
// (vk::*, ash::Device → u64 wrappers Send+Sync), Arc<VulkanContext> (Send+Sync
// because all inner fields are Vulkan handles), or 容器 types whose
// elements are Send. These fields are accessed exclusively from the 渲染
// 线程 so there is no 并发 &mut mutation.
unsafe impl Send for GraphRenderer {}

impl Drop for GraphRenderer {
    fn drop(&mut self) {
        self.destroy();
    }
}
