//! PrismaRev 的模块化渲染通道图。
//!
//! 取代旧版整体式渲染器。每个渲染阶段（GBuffer、RayQuery、SHARC GI、Lighting、Post）
//! 是一个 [`RenderPassNode`]，声明其输入/输出和一个执行方法。
//! 通道注册到 [`RenderGraph`] 中，该图管理临时资源分配和执行顺序。
//!
//! ## 设计
//!
//! - **通道是 trait 对象**——可在运行时添加/移除（特性开关：RT 开/关、GI 模式切换）。
//! - **资源句柄是类型化 ID**——图拥有实际的 Vulkan 资源；通道通过句柄引用它们，
//!   而非原始 `vk::Image`。
//! - **临时附件**使用 `LAZILY_ALLOCATED` 内存以实现 TBDR 效率（参见 `transient.rs`）。
//! - **子通道融合**——读取彼此 GBuffer 的通道可以融合到单个渲染通道中，
//!   以避免瓦片内存回写。

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use ash::vk;

use crate::capabilities::RayTracingCaps;
use crate::context::VulkanContext;
use crate::descriptor::{FrameUBO, GpuLight, PtAnalyticLight};
use crate::managers::{MeshHandle, RenderMeshManager};

/// A typed handle to a graph-managed resource (image, buffer).
/// The inner `u32` is an index into the graph's resource table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceHandle(pub u32);

/// Well-known graph-edge 资源 handles published by `ForwardPass` and 读取
/// by downstream passes (`GtaoPass`, `PostPass`). Fixed (not counter-based)
/// so a pass added later can reference them without knowing the upstream
/// pass's internal handle field. The graph's `next_handle` counter is kept
/// below this range (see `create_resource_at`), so there is no collision.
pub const FORWARD_DEPTH_H: ResourceHandle = ResourceHandle(1000);
pub const FORWARD_NORMAL_H: ResourceHandle = ResourceHandle(1001);
pub const FORWARD_COLOR_H: ResourceHandle = ResourceHandle(1002);
/// PT (path tracing) 输出 颜色 — replaces FORWARD_COLOR_H when
/// `RenderSettings.render_mode == PathTrace`. Written by PathTracePass
/// as a storage+sampled image, read by PostPass for tonemapping.
pub const PT_COLOR_H: ResourceHandle = ResourceHandle(1003);
/// Directional light depth shadow map — written by `ShadowMapPass` and
/// sampled in `ForwardPass` fragment shader for shadowing.
pub const SHADOW_MAP_H: ResourceHandle = ResourceHandle(1004);
/// TAA-resolved HDR color (#883). Written by `TaaPass`, read by `PostPass`
/// instead of `FORWARD_COLOR_H` when `RenderSettings::taa` is enabled.
pub const TAA_COLOR_H: ResourceHandle = ResourceHandle(1005);
/// Per-pixel screen-space motion vectors (#883 Phase 2). Written by
/// `ForwardPass` (SV_Target2) and sampled by `TaaPass` for object motion.
pub const FORWARD_VELOCITY_H: ResourceHandle = ResourceHandle(1006);
/// ANF-upscaled full-resolution HDR color (#1161). Written by `AnfPass`,
/// read by `PostPass` instead of `FORWARD_COLOR_H` / `TAA_COLOR_H` when ANF is active.
pub const ANF_COLOR_H: ResourceHandle = ResourceHandle(1007);

impl ResourceHandle {
    pub const INVALID: ResourceHandle = ResourceHandle(u32::MAX);
}

/// Resource type for graph-managed attachments.
#[derive(Clone, Debug)]
pub enum ResourceType {
    /// Color attachment (GBuffer layer, HDR output, etc.)
    ColorAttachment {
        format: vk::Format,
        extent: vk::Extent2D,
        sample_count: vk::SampleCountFlags,
    },
    /// Depth/stencil attachment
    DepthAttachment {
        extent: vk::Extent2D,
        sample_count: vk::SampleCountFlags,
    },
    /// Storage image (compute pass output, read by later passes)
    StorageImage {
        format: vk::Format,
        extent: vk::Extent3D,
    },
    /// Storage buffer (SHARC hash/accumulation/resolved buffers)
    StorageBuffer { size: u64 },
}

/// Description of a resource a pass needs — either reads from or writes to.
#[derive(Clone, Debug)]
pub struct ResourceUsage {
    pub handle: ResourceHandle,
    pub access: vk::AccessFlags,
    pub stage: vk::PipelineStageFlags,
    pub layout: vk::ImageLayout,
}

/// Direction of a declared resource edge. Read edges cause the graph to
/// transition the image into `usage.layout` (with src from the last writer);
/// write edges record the layout the pass leaves the image in (via its render
/// pass `final_layout`), so the next reader's barrier knows the true
/// `old_layout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    Read,
    Write,
}

/// One declared resource access (a read or write edge) for dependency
/// resolution and automatic barrier insertion.
#[derive(Clone, Debug)]
pub struct ResourceEdge {
    pub pass_idx: usize,
    pub usage: ResourceUsage,
    pub kind: EdgeKind,
}

/// Per-resource lifecycle span `[first_write_pass, last_read_pass]`, computed
/// at build time. Currently only surfaced to the visualizer; reserved as input
/// for future TBDR 内存 aliasing (已实现).
#[derive(Clone, Debug, Default)]
pub struct ResourceLifecycle {
    pub first_write: Option<usize>,
    pub last_read: Option<usize>,
}

impl ResourceLifecycle {
    /// Fold a single edge into the span.
    pub fn update(&mut self, e: &ResourceEdge) {
        match e.kind {
            EdgeKind::Write => {
                self.first_write = Some(self.first_write.map_or(e.pass_idx, |w| w.min(e.pass_idx)));
            }
            EdgeKind::Read => {
                self.last_read = Some(self.last_read.map_or(e.pass_idx, |r| r.max(e.pass_idx)));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Read-only snapshots for render-graph visualizers / diagnostics.
//
// The engine-side viz must not borrow `RenderGraph` (its passes are private
// trait objects) nor 触摸 `vk::*` handles inside the UI 闭包 These
// plain-data structs are produced by `RenderGraph::snapshot` + each pass's
// `RenderPassNode::graph_info`, cloned once per frame, and consumed by the UI.
// ---------------------------------------------------------------------------

/// Render mode selector — full rasterized PBR vs real-time path tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RenderMode {
    /// 标准 rasterized PBR 管线 (ForwardPass + ShadowMap + GTAO + Post).
    #[default]
    Raster,
    /// Real-time path tracing (PathTracePass compute + Post).
    PathTrace,
}

/// Coarse classification of a pass for visualization (coloring / iconography).
/// Kept in sync with the concrete pass structs that override `graph_info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PassKind {
    /// Rasterized depth-only shadow map (`ShadowMapPass`).
    Shadow,
    /// 向前 PBR scene 渲染 (`ForwardPass`).
    Forward,
    /// Half-resolution screen-space ambient occlusion (`GtaoPass`).
    Gtao,
    /// Fullscreen tonemap / present (`PostPass`).
    Post,
    /// Real-time path tracing compute pass (`PathTracePass`).
    Pt,
    /// 离屏 RenderTexture 渲染 (`RenderToTexturePass`).
    Rt,
    /// Qualcomm Adreno Neural Fusion compute pass (`AnfPass`).
    Anf,
    /// Unrecognized pass future / experimental).
    #[default]
    Unknown,
}

/// Static description of one graph-managed resource for the visualizer.
/// Mirrors the relevant subset of [`GraphResource`] without exposing Vulkan
/// handles.
#[derive(Clone, Debug)]
pub struct ResourceInfo {
    pub handle: ResourceHandle,
    pub res_type: ResourceType,
    /// `true` once `allocate_resources` has created the backing image.
    pub allocated: bool,
}

/// Static description of one pass for the visualizer: its declared resource
/// edges (`inputs` = handles it reads via `GraphResources::published_view`,
/// `outputs` = handles it publishes) plus a coarse kind for coloring.
///
/// Side-inputs that bypass the graph (shadow view, IBL set, previous-frame AO
/// bound via `set_ao`) are NOT listed here - they are surfaced as human-readable
/// notes by the viz instead, since they don't flow through `GraphResources`.
#[derive(Clone, Debug)]
pub struct PassInfo {
    /// Execution index (filled in by `RenderGraph::snapshot`).
    pub index: usize,
    pub name: String,
    pub kind: PassKind,
    /// Resource handles this pass reads from upstream passes.
    pub inputs: Vec<ResourceHandle>,
    /// Resource handles this pass publishes for downstream passes.
    pub outputs: Vec<ResourceHandle>,
}

/// A complete read-only snapshot of the render graph: passes in execution
/// order, the resource table, and the active settings. Produced per-frame by
/// [`RenderGraph::snapshot`].
#[derive(Clone, Debug)]
pub struct RenderGraphSnapshot {
    pub passes: Vec<PassInfo>,
    pub resources: Vec<ResourceInfo>,
    pub settings: RenderSettings,
}

/// Shadow rendering strategy.
///
/// Selected per-frame by [`RenderSettings::resolve_shadow`] using probed
/// ray-tracing capabilities, so the running path adapts to the GPU. Mirrors
/// `docs/DESIGN.md` §2.3: `VK_KHR_ray_query` present → RayQuery soft shadow;
/// otherwise fall back to a rasterized depth-only shadow map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ShadowMode {
    /// No shadows.
    None,
    /// Rasterized depth-only shadow map (always available; the fallback path).
    Raster,
    /// RayQuery inline soft shadow (requires `VK_KHR_ray_query` + a built TLAS).
    RayQuery,
    /// Automatic: RayQuery when available and RT is enabled, else Raster.
    #[default]
    Auto,
}

/// Quality / feature settings that passes consult at execution time.
/// These are the runtime-switchable knobs described in
/// `docs/mobile-raytracing-gi-design.md`.
#[derive(Clone, Debug)]
pub struct RenderSettings {
    /// GBuffer color format toggle.
    /// `true` = RGBA32F (quality), `false` = R10G10B10A2 (bandwidth, default).
    pub gbuffer_high_precision: bool,

    /// Ray tracing master switch.
    pub ray_tracing_enabled: bool,

    /// Ray query resolution scale: 1.0 = full res, 0.5 = half res (default).
    /// Setting to 1.0 disables half-resolution (user wants full quality).
    pub ray_query_resolution_scale: f32,

    /// SHARC hash-map capacity (number of voxel slots).
    /// Mobile default: 2^20 (1M). Desktop: 2^23 (8M).
    pub sharc_capacity: u32,

    /// SHARC scene scale — controls voxel physical size.
    pub sharc_scene_scale: f32,

    /// Shadow strategy. `Auto` (default) picks RayQuery when RT is enabled and
    /// `VK_KHR_ray_query` is supported, otherwise falls back to the rasterized
    /// shadow map. See [`ShadowMode`].
    pub shadow_mode: ShadowMode,

    /// 阴影贴图边长（texel，#526）。
    ///
    /// 这是**渲染图资源**大小，在创建 / 重建 shadow pass 时生效；运行中修改
    /// 需要重建渲染图（当前 `GraphRenderer` 只在构造时读取）。会被
    /// [`crate::MIN_SHADOW_MAP_SIZE`] / [`crate::MAX_SHADOW_MAP_SIZE`] 钳制。
    pub shadow_map_size: u32,

    /// 方向光级联数（#526）：`1` = 单张方形阴影图，`2` = 2×1 atlas。
    /// 与 `shadow_map_size` 一样在构造渲染图时生效。
    pub shadow_cascade_count: u32,

    /// Render mode: Raster (PBR) or PathTrace (real-time PT).
    pub render_mode: RenderMode,

    /// Maximum path depth (bounces) for path tracing.
    pub pt_max_bounces: u32,
    /// Max world-space length of PT primary + shadow rays. Smaller values cut
    /// long-range bounces (and distant shadow casters) — an artistic focus/
    /// fog control and a cost cap on huge scenes.
    pub pt_ray_max_distance: f32,
    /// Maximum iterations (samples per pixel) for path tracing.
    /// 0 = accumulate forever (default).
    pub pt_max_iterations: u32,

    /// Temporal anti-aliasing (#883). `false` keeps the pre-TAA path.
    pub taa: bool,
    /// History blend weight for TAA, clamped to `0..=0.95`; 0 = current frame.
    pub taa_strength: f32,
    /// 内部渲染分辨率 `[宽度, 高度]`（#613）；`[0, 0]` = 关闭。
    ///
    /// 开启后场景（网格 / 2D 精灵 / 瓦片 / 粒子 / PT）按这个尺寸渲染，
    /// `PostPass` 再放大到窗口。UI 仍在窗口分辨率上绘制。
    pub internal_resolution: [u32; 2],

    /// 内部图像放大到窗口的方式（见 [`InternalScaleMode`]）。
    pub internal_scale_mode: InternalScaleMode,

    /// Qualcomm Adreno Neural Fusion Super Resolution (#1161).
    pub anf_sr: bool,
    /// ANF Super Resolution sharpness parameter [0.0..=1.0].
    pub anf_sharpness: f32,
    /// Qualcomm Adreno Neural Fusion Frame Generation (#1161).
    pub anf_fg: bool,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            gbuffer_high_precision: true,
            ray_tracing_enabled: false,
            ray_query_resolution_scale: 0.5,
            sharc_capacity: 1 << 20,
            sharc_scene_scale: 1.0,
            shadow_mode: ShadowMode::Auto,
            shadow_map_size: crate::SHADOW_MAP_SIZE,
            shadow_cascade_count: 1,
            render_mode: RenderMode::Raster,
            pt_max_bounces: 3,
            pt_ray_max_distance: 1000.0,
            pt_max_iterations: 0,
            taa: false,
            taa_strength: 0.9,
            internal_resolution: [0, 0],
            internal_scale_mode: InternalScaleMode::Stretch,
            anf_sr: false,
            anf_sharpness: 0.5,
            anf_fg: false,
        }
    }
}

impl RenderSettings {
    /// Resolve the effective shadow mode given probed capabilities.
    ///
    /// `Auto` selects RayQuery when ray tracing is enabled and
    /// `VK_KHR_ray_query` is supported, otherwise falls back to the
    /// rasterized shadow map. Explicit modes pass through unchanged.
    pub fn resolve_shadow(&self, caps: &RayTracingCaps) -> ShadowMode {
        match self.shadow_mode {
            ShadowMode::Auto => {
                if self.ray_tracing_enabled && caps.has_ray_query() {
                    ShadowMode::RayQuery
                } else {
                    ShadowMode::Raster
                }
            }
            other => other,
        }
    }
}

/// 逐绘制的渲染状态（`Material` / `Renderer` 上那组决定管线状态的字段）。
///
/// 它同时是 `ForwardPass` 的管线缓存键：混用不同状态的绘制各自拿到正确的
/// 剔除 / 深度写 / 混合配置，而描述符集合布局与推送常量在所有变体之间相同。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DrawState {
    /// 剔除模式（`CullMode`）。
    pub cull_mode: prism_gfx::CullMode,
    /// 渲染队列（`Material.renderQueue`），决定绘制顺序。
    pub render_queue: i32,
    /// 是否写深度（`_ZWrite`）。
    pub z_write: bool,
    /// 源混合因子（`_SrcBlend`）。
    pub src_blend: prism_gfx::BlendMode,
    /// 目标混合因子（`_DstBlend`）。
    pub dst_blend: prism_gfx::BlendMode,
    /// 混合运算（`_BlendOp`）。
    pub blend_op: prism_gfx::BlendOp,
}

impl Default for DrawState {
    fn default() -> Self {
        Self {
            cull_mode: prism_gfx::CullMode::Back,
            render_queue: prism_gfx::RenderQueue::GEOMETRY,
            z_write: true,
            // 引擎的不透明默认值（与 `MeshRenderer::default` 保持一致）。
            src_blend: prism_gfx::BlendMode::One,
            dst_blend: prism_gfx::BlendMode::Zero,
            blend_op: prism_gfx::BlendOp::Add,
        }
    }
}

impl DrawState {
    /// 是否需要开启混合；用 `One` / `Zero` 表示不透明。
    pub fn blends(&self) -> bool {
        self.src_blend != prism_gfx::BlendMode::One
            || self.dst_blend != prism_gfx::BlendMode::Zero
            || self.blend_op != prism_gfx::BlendOp::Add
    }
}

/// One draw call's static data, supplied by the engine each frame.
/// Graph passes read these to record geometry draws into their attachments.
#[derive(Clone)]
pub struct DrawItem {
    /// GPU mesh handle (from [`crate::managers::RenderMeshManager`]).
    pub mesh: MeshHandle,
    /// 产生这个绘制项的 ECS 实体 id（`prism_ecs::Entity::id()`）。
    ///
    /// 渲染器本身不依赖 ECS；编辑器把选中的实体 id 交给
    /// [`GraphRenderer::set_outline_entities`]，`ForwardPass` 据此给对应的
    /// 网格画选中描边（#487）。程序化 / 测试绘制可用 `u32::MAX` 表示无实体。
    pub entity_id: u32,
    /// Model matrix (world transform) for this instance.
    pub model: [[f32; 4]; 4],
    /// Material SSBO slot (index into `RenderMaterialManager`'s
    /// `GpuMaterial[]` buffer) for the bindless PBR path. `None` -> slot 0
    /// (the fallback material). `app.rs` resolves `MaterialHandle` -> slot
    /// via `mat_map` when building the draw list, so passes can push the
    /// slot directly without a per-draw `slot_of()` lookup.
    pub material: Option<u32>,
    /// 是否写阴影贴图（`ShadowCastingMode::casts_shadows`）。
    pub cast_shadow: bool,
    /// 是否在相机主通道可见（`ShadowsOnly` 为 `false`）。
    pub visible_in_main_pass: bool,
    /// 阴影通道是否关闭背面剔除（`ShadowCastingMode::TwoSided`）。
    /// `false` = 背面剔除（默认），`true` = 双面投射。只被 `ShadowMapPass` 读取。
    pub cast_shadow_two_sided: bool,
    /// 管线状态（剔除 / 队列 / 深度写 / 混合），来自 `MeshRenderer`。
    /// 阴影贴图管线忽略剔除与混合（固定双面、无混合写深度）。
    pub state: DrawState,
    /// 世界空间 AABB（网格生命周期内算一次）；没有包围盒时保守地不剔除。
    ///
    /// 主相机视锥剔除在提取阶段使用；光源视锥剔除（#526）在帧打包后用它
    /// 判断是否还需要写阴影贴图。
    pub world_bounds: Option<crate::culling::Aabb>,
    /// 索引缓冲里的起始索引（#520 submesh）。非索引网格忽略。
    pub index_start: u32,
    /// 本次绘制使用的索引数；`None` = 整个网格（旧行为）。
    ///
    /// `Some(0)` 的绘制项会被渲染器跳过，方便调用方表达空 submesh。
    pub index_count: Option<u32>,
    /// Skinned draw (#560 P0-3): index of this draw's first bone-palette
    /// matrix inside the frame's bone SSBO (`ForwardPass::BONE_CAPACITY`).
    ///
    /// `None` = static mesh; the vertex stage then skips the bone buffer
    /// entirely, so existing draws cost exactly what they did before.
    pub bone_offset: Option<u32>,
    /// 纹理层 splat 的 4 个材质槽（#517）；`u32::MAX` = 该层缺失。
    pub splat_slots: [u32; 4],
    /// 纹理层 splat 的 UV 平铺倍数；`0` = 不是 splat 绘制。
    pub splat_tiling: f32,
}

impl DrawItem {
    /// 世界空间位置（取自 `model` 的平移列，列主序 `model[3][0..3]`）。
    pub fn world_position(&self) -> [f32; 3] {
        [self.model[3][0], self.model[3][1], self.model[3][2]]
    }
}

/// 按引擎的 `renderQueue` 规则给绘制列表排序。
///
/// - 队列值升序；相等时保持原有顺序（`sort_by` 是稳定排序）。
/// - 队列大于 [`prism_gfx::RenderQueue::GEOMETRY_LAST`]（透明段）内部按与
///   相机的距离**从远到近**，这样 alpha 混合的叠加顺序才正确。
/// - 相机位置取 `eye`（世界空间）；不透明段不做距离排序。
pub fn sort_draw_items(items: &mut [DrawItem], eye: [f32; 3]) {
    let distance_sq = |item: &DrawItem| {
        let p = item.world_position();
        let d = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
    };
    items.sort_by(|a, b| {
        a.state
            .render_queue
            .cmp(&b.state.render_queue)
            .then_with(|| {
                if b.state.render_queue > prism_gfx::RenderQueue::GEOMETRY_LAST {
                    // 透明：远的先画。
                    distance_sq(b)
                        .partial_cmp(&distance_sq(a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
    });
}

/// One world-space particle billboard for
/// [`crate::particle_pass::ParticlePass`].
///
/// The engine simulates the particles on the CPU and hands the renderer a flat
/// list. The camera basis is **not** part of the record: the pass expands the
/// quad along `GraphFrame::inv_view_rot`'s right/up columns, so the billboard
/// faces whatever camera rendered the frame.
#[derive(Clone, Copy, Debug)]
pub struct ParticleDrawItem {
    /// World-space center (xyz) and radius in world units (w).
    pub center: [f32; 3],
    /// Billboard half-size / radius in world units.
    pub size: f32,
    /// Linear RGBA color (already faded by the particle's age).
    pub color: [f32; 4],
}

/// One 2D sprite instance for [`crate::sprite_pass::SpritePass`].
#[derive(Clone, Copy, Debug)]
pub struct SpriteDrawItem {
    /// World matrix applied to the unit quad (includes size + pivot).
    ///
    /// Ignored when [`Self::corners`] is `Some`: line triangles provide their
    /// world-space vertices explicitly (#604).
    pub model: [[f32; 4]; 4],
    /// Explicit quad corners in world space, in the order
    /// `[bottom-left, bottom-right, top-right, top-left]`.
    ///
    /// `None` for sprites (the vertex shader expands [`Self::model`]); lines
    /// use it to draw non-overlapping triangles. A triangle is a quad whose
    /// last two corners coincide.
    pub corners: Option<[[f32; 3]; 4]>,
    /// UV rect `(u0, v0, u1, v1)` inside the sprite sheet.
    pub uv_rect: [f32; 4],
    /// Linear RGBA tint multiplied with the sampled texel.
    pub color: [f32; 4],
    /// Bindless texture slot.
    pub texture: u32,
    /// Blend state for this sprite (#610).
    pub src_blend: prism_gfx::BlendMode,
    pub dst_blend: prism_gfx::BlendMode,
    pub blend_op: prism_gfx::BlendOp,
    /// World-space clipping rect `[min_x, min_y, max_x, max_y]` (#582).
    ///
    /// [`NO_CLIP`] means "draw everywhere"; anything with `max_x >= min_x`
    /// clips per-fragment in the shader.
    pub clip: [f32; 4],
    /// Whether [`SpriteLight`]s modulate this sprite (#611).
    ///
    /// `false` keeps the unlit `texture × color` result, so 2D scenes that have
    /// no lights look exactly as they did before lighting existed.
    pub receive_lights: bool,
}

/// 2D light kinds — must match `SPRITE_LIGHT_*` in `sprites.slang`.
pub const SPRITE_LIGHT_AMBIENT: u32 = 0;
pub const SPRITE_LIGHT_POINT: u32 = 1;
pub const SPRITE_LIGHT_DIRECTIONAL: u32 = 2;

/// Maximum number of 2D lights uploaded per frame (#611).
///
/// The sprite shader loops over this many entries; extra lights are dropped by
/// the engine (sorted by contribution so the strongest survive).
pub const SPRITE_LIGHT_MAX: usize = 16;

/// One 2D light for [`crate::sprite_pass::SpritePass`] (#611).
///
/// 64-byte, `std430`-friendly record (three `float4`s) mirroring
/// `SpriteLight` in `sprites.slang`. Kept separate from [`GpuLight`], whose
/// photometric units (candela / lux) are meaningless for 2D sprites.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpriteLight {
    /// `x, y` = world position; `z` = falloff radius (`0` = no falloff);
    /// `w` = intensity.
    pub position_range: [f32; 4],
    /// Linear RGB colour; `w` = kind ([`SPRITE_LIGHT_AMBIENT`] / …).
    pub color_kind: [f32; 4],
    /// `x, y` = direction (world space); `zw` reserved.
    pub direction: [f32; 4],
}

impl SpriteLight {
    /// A point light at `position` with `radius` and `intensity`.
    pub fn point(position: [f32; 2], color: [f32; 3], intensity: f32, radius: f32) -> Self {
        Self {
            position_range: [position[0], position[1], radius, intensity.max(0.0)],
            color_kind: [color[0], color[1], color[2], SPRITE_LIGHT_POINT as f32],
            direction: [0.0; 4],
        }
    }

    /// A flat ambient light (no position / falloff).
    pub fn ambient(color: [f32; 3], intensity: f32) -> Self {
        Self {
            position_range: [0.0, 0.0, 0.0, intensity.max(0.0)],
            color_kind: [color[0], color[1], color[2], SPRITE_LIGHT_AMBIENT as f32],
            direction: [0.0; 4],
        }
    }

    /// A directional light (whole sprite multiplied by `color × intensity`).
    ///
    /// 方向会归一化；零向量 / 非有限值回落到「向下」，这样着色器读到的永远是
    /// 一个合法单位向量。
    pub fn directional(direction: [f32; 2], color: [f32; 3], intensity: f32) -> Self {
        let direction = normalize_light_direction(direction);
        Self {
            position_range: [0.0, 0.0, 0.0, intensity.max(0.0)],
            color_kind: [
                color[0],
                color[1],
                color[2],
                SPRITE_LIGHT_DIRECTIONAL as f32,
            ],
            direction: [direction[0], direction[1], 0.0, 0.0],
        }
    }

    /// Strongest-first ordering key used when trimming to [`SPRITE_LIGHT_MAX`].
    fn weight(&self) -> f32 {
        let intensity = self.position_range[3].max(0.0);
        let luma =
            0.2126 * self.color_kind[0] + 0.7152 * self.color_kind[1] + 0.0722 * self.color_kind[2];
        intensity * luma.max(0.0)
    }
}

/// 把 2D 光照方向归一化；零向量 / 非有限值回落到「向下」。
fn normalize_light_direction(direction: [f32; 2]) -> [f32; 2] {
    let length = (direction[0] * direction[0] + direction[1] * direction[1]).sqrt();
    if length.is_finite() && length > 1e-6 {
        [direction[0] / length, direction[1] / length]
    } else {
        [0.0, -1.0]
    }
}

/// 把 `lights` 修剪到 [`SPRITE_LIGHT_MAX`]，强度高的优先（#611）。
pub fn trim_sprite_lights(lights: &mut Vec<SpriteLight>) {
    if lights.len() <= SPRITE_LIGHT_MAX {
        return;
    }
    lights.sort_by(|a, b| b.weight().total_cmp(&a.weight()));
    lights.truncate(SPRITE_LIGHT_MAX);
}

/// Disabled clip rect: `max < min`, which the sprite shader treats as "no clip".
pub const NO_CLIP: [f32; 4] = [0.0, 0.0, -1.0, -1.0];

/// 把作者的裁剪矩形规范成 `[min_x, min_y, max_x, max_y]`。
///
/// 关闭、非有限值、或任一轴退化成零面积时返回 [`NO_CLIP`]——作者刚勾上
/// `clip_enabled` 但还没填矩形时精灵不会凭空消失。
pub fn clip_bounds(enabled: bool, rect: [f32; 4]) -> [f32; 4] {
    if !enabled || rect.iter().any(|value| !value.is_finite()) {
        return NO_CLIP;
    }
    let bounds = [
        rect[0].min(rect[2]),
        rect[1].min(rect[3]),
        rect[0].max(rect[2]),
        rect[1].max(rect[3]),
    ];
    if bounds[0] == bounds[2] || bounds[1] == bounds[3] {
        return NO_CLIP;
    }
    bounds
}

// ---------------------------------------------------------------------------
// 内部渲染分辨率（#613）
// ---------------------------------------------------------------------------

/// 内部渲染分辨率放大到窗口的方式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InternalScaleMode {
    /// 铺满整个窗口：允许非整数倍，用线性采样放大。
    #[default]
    Stretch,
    /// 整数倍放大并居中，四周留黑边；用最近邻采样，像素方块等宽（像素画）。
    Integer,
}

impl InternalScaleMode {
    /// 作者可写的名字（`settings.ini` 用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stretch => "stretch",
            Self::Integer => "integer",
        }
    }

    /// 解析 `"stretch"` / `"integer"`（大小写不敏感）；未知值返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stretch" | "fill" => Some(Self::Stretch),
            "integer" | "pixel" | "nearest" => Some(Self::Integer),
            _ => None,
        }
    }
}

/// 窗口 extent → 内部渲染 extent；`resolution == [0, 0]` 返回 `None`（关闭）。
///
/// 只有一个维度为 0 时按窗口宽高比推导另一个，避免作者写半边尺寸时得到
/// 0 像素的目标。
pub fn internal_extent(swapchain: vk::Extent2D, resolution: [u32; 2]) -> Option<vk::Extent2D> {
    let (mut width, mut height) = (resolution[0], resolution[1]);
    if width == 0 && height == 0 {
        return None;
    }
    if width == 0 {
        let aspect = if swapchain.height > 0 {
            swapchain.width as f32 / swapchain.height as f32
        } else {
            1.0
        };
        width = ((height as f32 * aspect).round() as u32).max(1);
    }
    if height == 0 {
        let aspect = if swapchain.width > 0 {
            swapchain.height as f32 / swapchain.width as f32
        } else {
            1.0
        };
        height = ((width as f32 * aspect).round() as u32).max(1);
    }
    Some(vk::Extent2D {
        width: width.max(1),
        height: height.max(1),
    })
}

/// `PostPass` 把内部图像放到窗口里的目标矩形。
///
/// `Stretch` 铺满整个窗口；`Integer` 取 `floor(min(倍率))`（至少 1）并居中，
/// 剩下的边由 render pass 的清屏色填成黑边。内部尺寸比窗口还大时按比例缩小。
pub fn post_destination(
    swapchain: vk::Extent2D,
    internal: vk::Extent2D,
    mode: InternalScaleMode,
) -> vk::Rect2D {
    let full = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: swapchain,
    };
    if internal.width == 0 || internal.height == 0 {
        return full;
    }
    let scale = match mode {
        InternalScaleMode::Stretch => return full,
        InternalScaleMode::Integer => {
            let by_width = swapchain.width as f32 / internal.width as f32;
            let by_height = swapchain.height as f32 / internal.height as f32;
            let factor = by_width.min(by_height);
            if factor >= 1.0 {
                factor.floor()
            } else {
                factor
            }
        }
    };
    let width = ((internal.width as f32 * scale).round() as u32).clamp(1, swapchain.width.max(1));
    let height =
        ((internal.height as f32 * scale).round() as u32).clamp(1, swapchain.height.max(1));
    vk::Rect2D {
        offset: vk::Offset2D {
            x: ((swapchain.width as i32 - width as i32) / 2).max(0),
            y: ((swapchain.height as i32 - height as i32) / 2).max(0),
        },
        extent: vk::Extent2D { width, height },
    }
}

/// [`post_destination`] 的纯数据版本：`(offset, extent)`，不依赖 `ash`。
///
/// 宿主（`prism-app`）用它推导「窗口像素 → UI 布局空间」的映射（#988），
/// 保证叠在画面上的 HUD 与 PostPass 放大后的矩形完全重合。
pub fn post_destination_px(
    swapchain: [u32; 2],
    internal: [u32; 2],
    mode: InternalScaleMode,
) -> ([u32; 2], [u32; 2]) {
    let rect = post_destination(
        vk::Extent2D {
            width: swapchain[0],
            height: swapchain[1],
        },
        vk::Extent2D {
            width: internal[0],
            height: internal[1],
        },
        mode,
    );
    (
        [rect.offset.x.max(0) as u32, rect.offset.y.max(0) as u32],
        [rect.extent.width, rect.extent.height],
    )
}

/// Post-processing knobs consumed by `PostPass` (#502).
///
/// `contrast` / `saturation` / `vignette` are display-referred and run **after**
/// tonemapping. `bloom_intensity` / `bloom_threshold` run in HDR **before**
/// tonemapping, so bright pixels bloom without clipping the highlight first.
///
/// All knobs are no-ops at their defaults: `PostGrade::default()` produces
/// exactly the pre-#502 image.
///
/// - `contrast`: scale around mid-grey `0.5` (`1.0` = unchanged);
/// - `saturation`: Rec.709 luma interpolation (`1.0` = unchanged, `0.0` = grey);
/// - `vignette`: corner falloff strength (`0.0` = off, `1.0` = corners black);
/// - `bloom_intensity`: additive bloom strength (`0.0` = off);
/// - `bloom_threshold`: HDR luminance above which pixels start blooming;
/// - `chromatic_aberration`: radial red/blue split strength (`0.0` = off);
/// - `film_grain`: display-referred monochrome noise (`0.0` = off);
/// - `temperature`: white-balance warm/cool shift in `-1..=1` (`0.0` = neutral);
/// - `tint`: white-balance magenta/green shift in `-1..=1` (`0.0` = neutral);
/// - `dof_focus_distance`: depth-of-field focus plane in metres;
/// - `dof_aperture`: depth-of-field strength in `0..=1` (`0.0` = off);
/// - `dof_max_blur`: largest depth-of-field blur radius in pixels;
/// - `lut_strength`: blend toward the sampled 3D LUT in `0..=1` (`0.0` = off);
/// - `motion_blur`: camera motion-blur strength in `0..=1` (`0.0` = off);
/// - `motion_blur_max_pixels`: largest motion-blur trail in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PostGrade {
    pub contrast: f32,
    pub saturation: f32,
    pub vignette: f32,
    pub bloom_intensity: f32,
    pub bloom_threshold: f32,
    pub chromatic_aberration: f32,
    pub film_grain: f32,
    pub temperature: f32,
    pub tint: f32,
    pub dof_focus_distance: f32,
    pub dof_aperture: f32,
    pub dof_max_blur: f32,
    pub lut_strength: f32,
    pub motion_blur: f32,
    pub motion_blur_max_pixels: f32,
}

impl Default for PostGrade {
    fn default() -> Self {
        Self {
            contrast: 1.0,
            saturation: 1.0,
            vignette: 0.0,
            bloom_intensity: 0.0,
            bloom_threshold: 1.0,
            chromatic_aberration: 0.0,
            film_grain: 0.0,
            temperature: 0.0,
            tint: 0.0,
            dof_focus_distance: 1.0,
            dof_aperture: 0.0,
            dof_max_blur: 8.0,
            lut_strength: 0.0,
            motion_blur: 0.0,
            motion_blur_max_pixels: 16.0,
        }
    }
}

impl PostGrade {
    /// `true` when every knob is at its default (the pass can skip the math).
    pub fn is_neutral(&self) -> bool {
        *self == Self::default()
    }

    /// Clamp to the ranges the shader expects (`contrast`/`saturation` never
    /// negative, `vignette`/`bloom_intensity` in `0..=1`, threshold non-negative).
    pub fn sanitized(self) -> Self {
        Self {
            contrast: if self.contrast.is_finite() {
                self.contrast.max(0.0)
            } else {
                1.0
            },
            saturation: if self.saturation.is_finite() {
                self.saturation.max(0.0)
            } else {
                1.0
            },
            vignette: if self.vignette.is_finite() {
                self.vignette.clamp(0.0, 1.0)
            } else {
                0.0
            },
            bloom_intensity: if self.bloom_intensity.is_finite() {
                self.bloom_intensity.clamp(0.0, 1.0)
            } else {
                0.0
            },
            bloom_threshold: if self.bloom_threshold.is_finite() {
                self.bloom_threshold.max(0.0)
            } else {
                1.0
            },
            chromatic_aberration: if self.chromatic_aberration.is_finite() {
                self.chromatic_aberration.clamp(0.0, 1.0)
            } else {
                0.0
            },
            film_grain: if self.film_grain.is_finite() {
                self.film_grain.clamp(0.0, 1.0)
            } else {
                0.0
            },
            temperature: if self.temperature.is_finite() {
                self.temperature.clamp(-1.0, 1.0)
            } else {
                0.0
            },
            tint: if self.tint.is_finite() {
                self.tint.clamp(-1.0, 1.0)
            } else {
                0.0
            },
            dof_focus_distance: if self.dof_focus_distance.is_finite() {
                self.dof_focus_distance.max(0.001)
            } else {
                1.0
            },
            dof_aperture: if self.dof_aperture.is_finite() {
                self.dof_aperture.clamp(0.0, 1.0)
            } else {
                0.0
            },
            dof_max_blur: if self.dof_max_blur.is_finite() {
                self.dof_max_blur.clamp(0.0, 32.0)
            } else {
                8.0
            },
            lut_strength: if self.lut_strength.is_finite() {
                self.lut_strength.clamp(0.0, 1.0)
            } else {
                0.0
            },
            motion_blur: if self.motion_blur.is_finite() {
                self.motion_blur.clamp(0.0, 1.0)
            } else {
                0.0
            },
            motion_blur_max_pixels: if self.motion_blur_max_pixels.is_finite() {
                self.motion_blur_max_pixels.clamp(0.0, 64.0)
            } else {
                16.0
            },
        }
    }
}

/// One camera composited into the frame (#461).
///
/// Every enabled `Camera` produces one of these, sorted by `Camera.depth`
/// ascending. The forward pass draws the scene once per entry, clearing the
/// colour / depth buffers according to the camera's `clearFlags`, so a
/// depth-ordered stack of opaque views composites into the single HDR target.
///
/// The first version covers opaque full-screen layers only: no per-camera
/// viewport rect and no per-camera post stack.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraView {
    /// `surface_rotation * projection * view`.
    pub view_proj: [[f32; 4]; 4],
    /// `world -> view`.
    pub view: [[f32; 4]; 4],
    /// Inverse of `view`, used by the skybox pass to orient the environment.
    pub inv_view: [[f32; 4]; 4],
    /// Camera world position (`.w` is filled with the light count at upload).
    pub eye: [f32; 3],
    /// Clear colour (`Camera.backgroundColor`).
    pub clear_color: [f32; 4],
    /// `clearFlags` clears the colour buffer (`Skybox` / `SolidColor`).
    pub clear_color_enabled: bool,
    /// `clearFlags` clears the depth buffer (everything except `Nothing`).
    pub clear_depth: bool,
    /// Draw the skybox for this camera (`clearFlags == Skybox`).
    pub draw_skybox: bool,
    /// Exposure multiplier for this camera.
    pub exposure: f32,
    /// `false` for the synthetic fallback when the scene has no camera.
    pub has_camera: bool,
}

impl CameraView {
    /// Build the implicit single-camera view used by the legacy call sites
    /// (`GraphRenderer::render`, tests) whose `FrameUBOData` describes the only
    /// camera.
    pub fn legacy(
        view_proj: [[f32; 4]; 4],
        inv_view: [[f32; 4]; 4],
        clear_color: [f32; 4],
        has_camera: bool,
    ) -> Self {
        Self {
            view_proj,
            // The forward pass only reads `view` for its own bookkeeping; the
            // skybox reads `inv_view`, so the fallback keeps the legacy
            // rotation-only inverse it already had.
            view: inv_view,
            inv_view,
            eye: [0.0; 3],
            clear_color,
            clear_color_enabled: true,
            clear_depth: true,
            draw_skybox: true,
            exposure: 1.0,
            has_camera,
        }
    }
}

/// Per-frame scene + lighting state shared with every pass via [`RenderContext`].
///
/// The `GraphRenderer` populates this once per frame (before driving the
/// graph) with the camera/light UBO, the draw list, and the light-space
/// view-projection used by both the shadow pass and the lighting pass
pub struct GraphFrame<'a> {
    /// Per-frame UBO (camera + light). Its descriptor set is bound at set 0.
    pub frame_ubo: &'a FrameUBO,
    /// Camera layers composited in `Camera.depth` order (#461). Empty means
    /// "one implicit camera from `FrameUBOData`", which keeps the legacy
    /// single-camera path bit-identical.
    pub cameras: &'a [CameraView],
    /// Draw list for the current frame.
    pub draw_list: &'a [DrawItem],
    /// ECS-authored gizmos for this frame.
    pub gizmos: &'a [crate::gizmo::GizmoInstance],
    /// ECS-authored 2D sprites for this frame (sorted back-to-front).
    pub sprite_items: &'a [SpriteDrawItem],
    /// `Light2D` entities for this frame, strongest first (#611).
    pub sprite_lights: &'a [SpriteLight],
    /// World-space particle billboards for this frame (sorted back-to-front).
    pub particle_items: &'a [ParticleDrawItem],
    /// Bone palette for skinned draws (#560 P0-3); indexed by
    /// [`DrawItem::bone_offset`]. Empty when nothing is skinned.
    pub bone_matrices: &'a [[[f32; 4]; 4]],
    /// ECS-authored skybox parameters (`None` = skip the sky draw).
    pub skybox: Option<crate::skybox_pass::SkyboxSettings>,
    /// Mesh manager — passes resolve [`DrawItem::mesh`] handles to GPU buffers.
    pub mesh_manager: &'a RenderMeshManager,
    /// Light-space view-projection (orthographic) used by the shadow pass and
    /// by the lighting pass to project world positions into the shadow map.
    pub light_view_proj: [[f32; 4]; 4],
    /// Far-cascade light-space VP (#526 CSM); ignored when
    /// `shadow_cascade_count <= 1`.
    pub light_view_proj1: [[f32; 4]; 4],
    /// Camera-forward distance of the near/far cascade split.
    pub shadow_split: f32,
    /// 1 = single shadow map, 2 = two-cascade atlas.
    pub shadow_cascade_count: u32,
    /// Effective shadow mode for this frame (after capability resolution).
    pub shadow_mode: ShadowMode,
    /// PBR component toggle bitmask (15 bits, see `scene_frag.slang`
    /// `PBR_FLAG_*`). 0 = all components neutral (raw baseColor). Forwarded
    /// to the bindless push constant `debug_flags` field.
    pub debug_flags: u32,
    /// 前向管线线框模式（#431）。设备不支持 `fillModeNonSolid` 时忽略。
    pub wireframe: bool,
    /// 正常填充 + 线框叠加（#431）。`wireframe` 为 true 时以纯线框为准。
    pub shaded_wireframe: bool,
    /// 需要画选中描边的 ECS 实体 id（#487）。
    ///
    /// 由编辑器通过 [`GraphRenderer::set_outline_entities`] 每帧设置；空切片
    /// 表示没有选中物体。`ForwardPass` 只对 `DrawItem::entity_id` 命中的绘制
    /// 项画反向外壳。
    pub outline_entities: &'a [u32],
    /// Inverse-view rotation (upper-left 3x3 of inverse(view)), packed as mat4.
    /// Used by the skybox pass to rotate view-space look directions into world
    /// space. Because the view matrix is a rigid transform, this is just the
    /// transpose of the upper-left 3x3 of `view` (the rotation basis), with w=0
    /// on the 4th row.
    pub inv_view_rot: [[f32; 4]; 4],
    /// Full world-space view-projection (clip = proj * view), including the
    /// surface rotation. Used by the world-space gizmo (drawn on top of the
    /// scene) so the axes track the camera.
    pub view_proj: [[f32; 4]; 4],
    /// Subpixel jitter applied to `view_proj` this frame, in NDC units
    /// (#883). Zero when TAA is off; `TaaPass` subtracts it to recover the
    /// unjittered camera matrix for history reprojection.
    pub view_proj_jitter: [f32; 2],
    /// Point lights collected from the ECS this frame, rewritten into the
    /// scene shader's 光源 SSBO. Forwarded to `ForwardPass::execute` so it can
    /// update its descriptor set without `GraphRenderer` poking it directly.
    pub lights: &'a [GpuLight],
    /// Previous-frame GTAO 可见性 视图 (1-frame 延迟 `ForwardPass`
    /// binds this as its AO input; it reads `ao[(frame + 1) % 2]` written by
    /// `GtaoPass` last frame. Forwarded via `GraphFrame` so the graph, not
    /// `GraphRenderer`, owns the cross-pass wiring.
    pub ao_view: vk::ImageView,
    /// Tonemap mode for `PostPass` (Reinhard / ACES / ...). Forwarded so
    /// `PostPass::execute` reads it from the graph context.
    pub tonemap_mode: u32,
    /// Post-process anti-aliasing mode for `PostPass` (0 = off, 1 = FXAA).
    /// Requires no extra targets: FXAA runs inside the tonemap pass, so the
    /// offscreen (editor viewport) and swapchain paths get the same result.
    pub aa_mode: u32,
    /// PostPass debug render-target viewer (Tab key). 0 = normal tonemapped
    /// HDR, 1 = linearized depth, 2 = view-space normal. PostPass picks which
    /// published view to sample based on this.
    pub debug_rt: u32,
    /// Display-referred color grading, applied in `PostPass` after tonemapping
    /// (#502). [`PostGrade::default`] is an exact no-op.
    pub grade: PostGrade,
    /// Projection matrix entries `[2][2]` / `[3][2]` (column-major
    /// `m[col][row]`) used by PostPass to linearize the depth buffer for the
    /// debug depth view (`view_z = proj22 * d + proj32`).
    pub proj22: f32,
    pub proj32: f32,
    /// Inverse projection (used by `GtaoPass` to reconstruct view-space
    /// radius from screen-space samples). Forwarded via `GraphFrame`.
    pub inv_projection: [[f32; 4]; 4],
    /// Swapchain image views for the current frame. Forwarded so `PostPass`
    /// can (re)build its per-swapchain-image framebuffers inside `execute`
    /// (mirroring `ForwardPass::ensure_target`), instead of relying on
    /// `GraphRenderer` to call `set_target` every frame.
    pub swapchain_views: &'a [vk::ImageView],
    /// Active render mode (Raster vs PathTrace). Passes check this to decide
    /// whether to run (ForwardPass skips in PT 众数 PathTracePass skips in
    /// Raster mode).
    pub render_mode: RenderMode,
    /// Path tracing max bounces.
    pub pt_max_bounces: u32,
    /// Max world-space length of PT primary + shadow rays. Smaller values cut
    /// long-range bounces (and distant shadow casters), useful as an artistic
    /// "fog"/focus control and to limit cost on huge scenes.
    pub pt_ray_max_distance: f32,
    /// Maximum iterations (samples per pixel). 0 = accumulate forever.
    pub pt_max_iterations: u32,
    /// Camera world-space position [x, y, z, light_count] from the frame UBO.
    pub camera_pos: [f32; 4],
    /// Light direction [x, y, z, intensity] from the frame UBO.
    pub light_dir: [f32; 4],
    /// Light color [r, g, b, ambient] from the frame UBO. Forwarded to the
    /// path-trace pass so it can apply the scene's actual sun color instead
    /// of a hardcoded white (the rasterizer reads this via the FrameUBO).
    pub light_color: [f32; 4],
    /// Exposure multiplier applied to the final HDR color before tonemapping.
    /// Forwarded from [`FrameInput`](crate::graph_renderer::FrameInput) so both
    /// ForwardPass (via FrameUBOData) and PathTracePass (via 推送 常量 apply
    /// the same exposure value from the camera entity.
    pub exposure: f32,
    /// Analytic lights for path tracing (point/spot/area/directional).
    /// Written into the PT lights SSBO for multi-light NEE.
    pub pt_lights: &'a [PtAnalyticLight],
    /// When `true`, the path tracer should reset its accumulation next frame.
    /// Set when directional-light properties change.
    pub pt_accum_dirty: bool,
    /// Whether a usable Camera entity was found. When `false`, the skybox and
    /// PT pass should be skipped so the clear color (gray) shows through.
    pub has_camera: bool,
    /// Whether the active camera is orthographic; the PT pass casts parallel
    /// primary rays in that case (issue #10).
    pub camera_orthographic: bool,
    /// 清空 颜色 for the scene 颜色 附件 Applied by ForwardPass on
    /// render-pass begin; visible when the skybox and scene geometry are
    /// absent or transparent (e.g. no-camera fallback). Matches the app-level
    /// `clear_color` parameter passed to `render_system`.
    pub clear_color: [f32; 4],
    /// 是否在场景渲染中绘制地面参考网格。
    pub show_grid: bool,
}

/// Context passed to each pass's `execute`.
pub struct RenderContext<'a> {
    pub device: &'a ash::Device,
    pub context: &'a VulkanContext,
    pub settings: &'a RenderSettings,
    pub cmd: vk::CommandBuffer,
    pub frame_index: u32,
    /// Swapchain image index returned by `acquire_next_image`. Distinct from
    /// `frame_index` (which is the frame-in-flight index): with N swapchain
    /// images and 2 frames in flight, `frame_index` cycles 0..2 while
    /// `image_index` cycles 0..N. Passes that own per-swapchain-image resources
    /// (e.g. `ForwardPass`'s framebuffers) 索引 by this, not `frame_index`.
    pub image_index: u32,
    /// Current swapchain extent.
    pub extent: vk::Extent2D,
    /// 场景实际渲染的 extent（内部渲染分辨率，#613）。
    ///
    /// 没有开启内部分辨率时与 [`Self::extent`] 相同。`ForwardPass` / 路径追踪
    /// 按这个尺寸分配目标；`PostPass` / UI 叠加 / 截图仍用 `extent`。
    pub render_extent: vk::Extent2D,
    /// Per-frame scene + lighting state (see [`GraphFrame`]).
    pub frame: &'a GraphFrame<'a>,
}

/// Trait for a modular render pass.
///
/// Each pass declares its resource needs via [`setup`] and records commands
/// in [`execute`]. The graph calls these in topological order.
pub trait RenderPassNode: std::any::Any {
    /// Human-readable name (for debugging / profiling).
    fn name(&self) -> &str;

    /// Declare resource reads/writes. Called once during graph compilation.
    /// The pass should register its needs via `graph.create_resource(...)` /
    /// `graph.read(...)` / `graph.write(...)`. `settings` is the runtime
    /// render configuration (e.g. `gbuffer_high_precision`) so the pass
    /// can pick the right format for its attachments.
    fn setup(&mut self, graph: &mut RenderGraphBuilder, settings: &RenderSettings);

    /// Record Vulkan commands into `ctx.cmd`. `resources` is mutable so the
    /// pass can publish its output views (depth / normal / HDR) for downstream
    /// passes to read by handle.
    fn execute(&mut self, ctx: &RenderContext, resources: &mut GraphResources) -> Result<()>;

    /// Pre‑create pipelines / shader modules at load time so the first frame
    /// doesn't pay pipeline‑compilation cost. Default is a no‑op; passes with
    /// lazy pipelines override this.
    fn warmup(
        &mut self,
        _device: &ash::Device,
        _context: &crate::context::VulkanContext,
    ) -> Result<()> {
        Ok(())
    }

    /// Read-only snapshot of this pass's declared resource edges + coarse kind,
    /// for the render-graph visualizer. Default returns an "unknown" pass with
    /// no edges; concrete passes override to populate `kind`/`inputs`/`outputs`.
    ///
    /// The execution `index` is filled in by [`RenderGraph::snapshot`] (the pass
    /// does not know its own position); implementations should leave it as
    /// `usize::MAX` or `0`.
    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Unknown,
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Resource state tracker — automatic barrier derivation
// ---------------------------------------------------------------------------

/// Per-resource GPU layout + access + stage snapshot for auto-barrier logic.
#[derive(Clone, Copy, Debug)]
pub struct ResourceState {
    pub layout: vk::ImageLayout,
    pub access: vk::AccessFlags,
    pub stage: vk::PipelineStageFlags,
}

/// Tracks per-`(handle, image_index)` resource state across passes and frames.
/// Used by `RenderGraph::execute` to derive automatic barriers between passes.
/// `reset()` clears all state (called after swapchain recreation).
#[derive(Clone, Debug)]
pub struct ResourceStateTracker {
    states: HashMap<(ResourceHandle, u32), ResourceState>,
}

impl ResourceStateTracker {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    /// Look up current state; returns UNDEFINED / empty / TOP_OF_PIPE when
    /// unknown (initial-transition semantics).
    pub fn get(&self, handle: ResourceHandle, image_index: u32) -> ResourceState {
        self.states
            .get(&(handle, image_index))
            .copied()
            .unwrap_or(ResourceState {
                layout: vk::ImageLayout::UNDEFINED,
                access: vk::AccessFlags::empty(),
                stage: vk::PipelineStageFlags::TOP_OF_PIPE,
            })
    }

    /// Record that a resource is now in the given state (called after a pass's
    /// render pass transitions it via `final_layout`).
    pub fn set(
        &mut self,
        handle: ResourceHandle,
        image_index: u32,
        layout: vk::ImageLayout,
        access: vk::AccessFlags,
        stage: vk::PipelineStageFlags,
    ) {
        self.states.insert(
            (handle, image_index),
            ResourceState {
                layout,
                access,
                stage,
            },
        );
    }

    /// Batch-apply write edges from a pass: each write edge records the layout
    /// the pass leaves the resource in.
    pub fn apply_writes(&mut self, edges: &[ResourceEdge], image_index: u32) {
        for e in edges {
            if e.kind == EdgeKind::Write {
                self.set(
                    e.usage.handle,
                    image_index,
                    e.usage.layout,
                    e.usage.access,
                    e.usage.stage,
                );
            }
        }
    }

    /// Build barriers for all read edges whose tracked layout differs from the
    /// declared usage. Returns `(barriers, src_stage, dst_stage)`; callers
    /// should emit a single `vkCmdPipelineBarrier` with the union of stages.
    /// Skips resources already in the desired 布局 or 待发布.
    ///
    /// Each emitted barrier also updates the tracked layout to the reader's
    /// `usage.layout`, so a second reader of the same resource in the same
    /// frame sees the post-transition layout as its `old_layout` instead of a
    /// stale pre-transition value (which would trip VUID-oldLayout-01197).
    pub fn build_read_barriers(
        &mut self,
        edges: &[ResourceEdge],
        last_writers: &HashMap<ResourceHandle, ResourceUsage>,
        image_index: u32,
        resources: &GraphResources,
    ) -> (
        Vec<vk::ImageMemoryBarrier<'_>>,
        vk::PipelineStageFlags,
        vk::PipelineStageFlags,
    ) {
        let mut barriers: Vec<vk::ImageMemoryBarrier> = Vec::new();
        let mut max_src_stage = vk::PipelineStageFlags::empty();
        let mut max_dst_stage = vk::PipelineStageFlags::empty();

        for re in edges {
            if re.kind != EdgeKind::Read {
                continue;
            }
            let handle = re.usage.handle;
            let current = self.get(handle, image_index);
            if current.layout == re.usage.layout {
                continue;
            }

            let image = resources
                .published_image(handle)
                .or_else(|| resources.image(handle));
            let image = match image {
                Some(img) => img,
                None => continue,
            };

            let (src_access, src_stage) = match last_writers.get(&handle) {
                Some(w) => (w.access, w.stage),
                None => (
                    vk::AccessFlags::empty(),
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                ),
            };

            let aspect = aspect_mask_for_layout(re.usage.layout);
            barriers.push(
                vk::ImageMemoryBarrier::default()
                    .old_layout(current.layout)
                    .new_layout(re.usage.layout)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .src_access_mask(src_access)
                    .dst_access_mask(re.usage.access)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: aspect,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
            );
            max_src_stage |= src_stage;
            max_dst_stage |= re.usage.stage;

            // Advance the tracked state to the post-barrier layout, matching
            // the GPU transition we just recorded. This must happen per-read,
            // not per-write: a layout change is driven by the read edge here,
            // and `apply_writes` only runs after the pass executes.
            self.set(
                handle,
                image_index,
                re.usage.layout,
                re.usage.access,
                re.usage.stage,
            );
        }

        (barriers, max_src_stage, max_dst_stage)
    }

    /// Clear all tracked state (swapchain recreation).
    pub fn reset(&mut self) {
        self.states.clear();
    }
}

impl Default for ResourceStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// A resource entry in the graph's resource table.
#[derive(Clone)]
pub struct GraphResource {
    pub handle: ResourceHandle,
    pub res_type: ResourceType,
    /// Owning Vulkan image (None until allocated).
    pub image: Option<vk::Image>,
    pub image_view: Option<vk::ImageView>,
    pub memory: Option<vk::DeviceMemory>,
}

/// Resource table passed to passes at execute time.
/// Besides the graph-owned images (allocated in `allocate_resources`), it
/// carries **pass-exported views + images** (e.g. `ForwardPass` publishes its
/// depth / normal / HDR views AND images here so downstream passes like
/// `GtaoPass` / `PostPass` can read them by handle). This is the minimal
/// graph-edge resource handoff for PR-1: the graph does not own the
/// underlying images (passes still create their own framebuffers), but it is
/// the channel through which passes exchange resource handles instead of
/// `GraphRenderer` poking each pass
pub struct GraphResources {
    pub resources: HashMap<ResourceHandle, GraphResource>,
    /// Pass-published image views, keyed by `ResourceHandle`.
    pub image_views: HashMap<ResourceHandle, vk::ImageView>,
    /// Pass-published images (handles), keyed by `ResourceHandle`. Needed by
    /// downstream passes that emit layout barriers (which reference the image,
    /// not the view).
    pub images: HashMap<ResourceHandle, vk::Image>,
    /// Pass-published scalar values (e.g. bindless slots), keyed by
    /// `ResourceHandle`. Generic handle handoff for producers → consumers
    /// that don't map to a Vulkan handle (see `RenderToTexturePass` → the
    /// `TextureHandle` it publishes under `RT_OUTPUT_H`).
    pub params: HashMap<ResourceHandle, u32>,
}

impl GraphResources {
    pub fn new() -> Self {
        Self {
            resources: HashMap::new(),
            image_views: HashMap::new(),
            images: HashMap::new(),
            params: HashMap::new(),
        }
    }

    /// Clear per-frame published views, images, and params for the next frame.
    /// Retains the underlying capacity of the HashMaps to avoid per-frame allocations.
    pub fn clear_frame(&mut self) {
        self.image_views.clear();
        self.images.clear();
        self.params.clear();
    }

    pub fn image(&self, h: ResourceHandle) -> Option<vk::Image> {
        self.resources.get(&h).and_then(|r| r.image)
    }

    pub fn image_view(&self, h: ResourceHandle) -> Option<vk::ImageView> {
        self.resources.get(&h).and_then(|r| r.image_view)
    }

    /// Publish an image view under a handle so downstream passes can read it.
    pub fn set_image_view(&mut self, h: ResourceHandle, view: vk::ImageView) {
        self.image_views.insert(h, view);
    }

    /// Publish an image under a handle (for downstream layout barriers).
    pub fn set_image(&mut self, h: ResourceHandle, image: vk::Image) {
        self.images.insert(h, image);
    }

    /// Read a view published by an upstream pass.
    pub fn published_view(&self, h: ResourceHandle) -> Option<vk::ImageView> {
        self.image_views.get(&h).copied()
    }

    /// Read an image published by an upstream pass.
    pub fn published_image(&self, h: ResourceHandle) -> Option<vk::Image> {
        self.images.get(&h).copied()
    }

    /// 发布 a scalar value under a handle (e.g. a bindless slot) so downstream
    /// passes can 读取 it without knowing the producer's internals.
    pub fn set_param(&mut self, h: ResourceHandle, value: u32) {
        self.params.insert(h, value);
    }

    /// 读取 a scalar published by an upstream pass.
    pub fn param(&self, h: ResourceHandle) -> Option<u32> {
        self.params.get(&h).copied()
    }
}

impl Default for GraphResources {
    fn default() -> Self {
        Self::new()
    }
}

/// Groups edges by their execution `pass_idx` into preallocated buckets.
fn build_pass_edges(passes_count: usize, edges: &[ResourceEdge]) -> Vec<Vec<ResourceEdge>> {
    let mut buckets: Vec<Vec<ResourceEdge>> = vec![Vec::new(); passes_count];
    for e in edges {
        if e.pass_idx < buckets.len() {
            buckets[e.pass_idx].push(e.clone());
        }
    }
    buckets
}

// ---------------------------------------------------------------------------
// Graph builder — collects passes and resource declarations, then compiles.
// ---------------------------------------------------------------------------

pub struct RenderGraphBuilder {
    passes: Vec<Box<dyn RenderPassNode + 'static>>,
    resources: HashMap<ResourceHandle, GraphResource>,
    /// Declared read/write edges collected from passes' `setup`. Indexed by
    /// `pass_idx` (= `pass_idx_offset + passes.len()` at the moment
    /// `read_usage`/`write_usage` is called during setup, before `add_pass`
    /// pushes the pass). The offset is non-zero when this builder is a
    /// temporary created by `RenderGraph::add_pass` to set up a pass that will
    /// be appended to an already-populated graph.
    edges: Vec<ResourceEdge>,
    next_handle: u32,
    settings: RenderSettings,
    /// Index of the first pass this builder will register, in the final
    /// graph's pass list. Zero for a fresh builder; set to
    /// `RenderGraph::passes.len()` by `RenderGraph::add_pass` so edges declared
    /// during a pass's `setup` get the correct absolute `pass_idx`.
    pass_idx_offset: usize,
}

impl RenderGraphBuilder {
    pub fn new() -> Self {
        Self {
            passes: Vec::new(),
            resources: HashMap::new(),
            edges: Vec::new(),
            next_handle: 0,
            settings: RenderSettings::default(),
            pass_idx_offset: 0,
        }
    }

    /// Set the base pass index for edges declared via this builder. Used by
    /// [`RenderGraph::add_pass`] so a pass appended to an already-built graph
    /// records edges with its true absolute `pass_idx` rather than 0.
    pub fn pass_idx_offset(mut self, offset: usize) -> Self {
        self.pass_idx_offset = offset;
        self
    }

    /// Override the render settings used when `setup` is called on passes.
    pub fn settings(mut self, settings: &RenderSettings) -> Self {
        self.settings = settings.clone();
        self
    }

    /// Register a pass. Order of insertion = execution order (simple
    /// linear pipeline for now; topological sort can be added later).
    pub fn add_pass(&mut self, pass: Box<dyn RenderPassNode + 'static>) {
        self.passes.push(pass);
    }

    /// Create a transient resource managed by the graph.
    pub fn create_resource(&mut self, res_type: ResourceType) -> ResourceHandle {
        let handle = ResourceHandle(self.next_handle);
        self.next_handle += 1;
        self.resources.insert(
            handle,
            GraphResource {
                handle,
                res_type,
                image: None,
                image_view: None,
                memory: None,
            },
        );
        handle
    }

    /// Create a transient resource at a specific handle (e.g. a well-known
    /// graph-edge handle like `FORWARD_DEPTH_H`). Used so downstream passes can
    /// reference a publisher's output without knowing its internal field.
    pub fn create_resource_at(&mut self, handle: ResourceHandle, res_type: ResourceType) {
        self.resources.insert(
            handle,
            GraphResource {
                handle,
                res_type,
                image: None,
                image_view: None,
                memory: None,
            },
        );
    }

    /// Mark the pass currently being set up (i.e. the next one `add_pass`
    /// will push, at index `self.passes.len()`) as reading a resource, with
    /// the access/stage/layout it reads with. The graph uses this to insert a
    /// `vkCmdPipelineBarrier` before the pass when the image's current layout
    /// differs from `usage.layout`.
    ///
    /// Must be called from within `RenderPassNode::setup` (before `add_pass`
    /// pushes the pass calling it elsewhere panics.
    pub fn read_usage(&mut self, usage: ResourceUsage) {
        self.push_edge(usage, EdgeKind::Read);
    }

    /// Mark the pass currently being set up as writing a resource, with the
    /// access/stage/layout it leaves the image in (typically the render pass
    /// `final_layout`). The graph records this as the resource's current
    /// layout after the pass executes, so the next reader's barrier knows the
    /// true `old_layout`. No barrier is emitted for the write itself (the
    /// pass's render pass performs the layout transition implicitly).
    pub fn write_usage(&mut self, usage: ResourceUsage) {
        self.push_edge(usage, EdgeKind::Write);
    }

    fn push_edge(&mut self, usage: ResourceUsage, kind: EdgeKind) {
        self.edges.push(ResourceEdge {
            pass_idx: self.pass_idx_offset + self.passes.len(),
            usage,
            kind,
        });
    }

    /// Mark a pass (by index) as reading a resource.
    /// Tracked for future barrier generation and topological sort.
    pub fn read(&mut self, pass_idx: usize, handle: ResourceHandle) {
        // Future: push to a dependency list for barrier generation
        let _ = (pass_idx, handle);
    }

    /// Mark a pass (by index) as writing a resource.
    /// Tracked for future barrier generation and topological sort.
    pub fn write(&mut self, pass_idx: usize, handle: ResourceHandle) {
        let _ = (pass_idx, handle);
    }

    /// Compile into an executable graph.
    pub fn build(self) -> RenderGraph {
        let lifecycles = compute_lifecycles(&self.edges);
        let pass_edges = build_pass_edges(self.passes.len(), &self.edges);
        let last_writers = build_last_writers(&self.edges);
        let g = RenderGraph {
            passes: self.passes,
            graph_resources: GraphResources {
                resources: self.resources,
                image_views: HashMap::new(),
                images: HashMap::new(),
                params: HashMap::new(),
            },
            settings: self.settings,
            edges: self.edges,
            pass_edges,
            last_writers,
            state_tracker: ResourceStateTracker::new(),
            lifecycles,
            last_barrier_probe: Instant::now(),
        };
        g.validate_edges();
        g
    }
}

/// Compute the `[first_write, last_read]` span per resource from the declared
/// edges. Used by the visualizer and reserved as input for future TBDR memory
/// aliasing (no aliasing is performed today).
fn compute_lifecycles(edges: &[ResourceEdge]) -> HashMap<ResourceHandle, ResourceLifecycle> {
    let mut map: HashMap<ResourceHandle, ResourceLifecycle> = HashMap::new();
    for e in edges {
        map.entry(e.usage.handle).or_default().update(e);
    }
    map
}

impl Default for RenderGraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Executable graph
// ---------------------------------------------------------------------------

pub struct RenderGraph {
    passes: Vec<Box<dyn RenderPassNode + 'static>>,
    graph_resources: GraphResources,
    settings: RenderSettings,
    /// Declared read/write edges, collected from each pass's `setup`.
    edges: Vec<ResourceEdge>,
    /// Precomputed pass_idx -> edges bucket list for fast traversal without per-frame cloning.
    pass_edges: Vec<Vec<ResourceEdge>>,
    /// Precomputed handle -> last writer's usage for barrier src stage/access.
    last_writers: HashMap<ResourceHandle, ResourceUsage>,
    /// Per-`(handle, image_index)` resource state tracker, persisted across
    /// frames so cross-frame reads (e.g. GTAO's double-buffered AO) keep their
    /// 布局 Keyed by `image_index` because `ForwardPass`/`PostPass` own
    /// per-swapchain-image attachments under the same handle.
    state_tracker: ResourceStateTracker,
    /// `[first_write, last_read]` span per resource, for the visualizer and
    /// future aliasing.
    lifecycles: HashMap<ResourceHandle, ResourceLifecycle>,
    /// Last time the `BARRIER_PROBE` trace lines were emitted; throttled to
    /// once per second so the log isn't flooded at frame rate.
    last_barrier_probe: Instant,
}

impl RenderGraph {
    /// Borrow a registered pass by concrete type (for lifecycle operations
    /// like `recreate_swapchain`, which must call into a specific pass).
    /// Returns `None` if no pass of that type was registered.
    pub fn pass_mut<T: RenderPassNode + 'static>(&mut self) -> Option<&mut T> {
        self.passes
            .iter_mut()
            .find_map(|p| (&mut **p as &mut dyn std::any::Any).downcast_mut::<T>())
    }

    /// Immutable borrow of a registered pass by concrete type. The read-only
    /// counterpart to [`pass_mut`](Self::pass_mut); used by the render-graph
    /// visualizer to pull live per-pass state (extent / format / image_count)
    /// without mutating the graph.
    pub fn pass_ref<T: RenderPassNode + 'static>(&self) -> Option<&T> {
        self.passes
            .iter()
            .find_map(|p| (&**p as &dyn std::any::Any).downcast_ref::<T>())
    }

    /// Active render settings (feature knobs consulted by passes at execute
    /// time). Exposed read-only for the visualizer's header summary.
    pub fn settings(&self) -> &RenderSettings {
        &self.settings
    }

    /// Call [`warmup`](RenderPassNode::warmup) on every registered pass.
    /// Designed to be called once after graph construction so that lazy
    /// pipelines are compiled ahead of the first frame.
    pub fn warmup_passes(
        &mut self,
        device: &ash::Device,
        context: &crate::context::VulkanContext,
    ) -> Result<()> {
        for pass in &mut self.passes {
            pass.warmup(device, context)?;
        }
        Ok(())
    }

    /// Iterator over all declared graph resources (depth/color attachments,
    /// storage images). Exposed read-only for the visualizer.
    pub fn resources(&self) -> impl Iterator<Item = &GraphResource> {
        self.graph_resources.resources.values()
    }

    /// Look up a graph-managed resource by handle (immutable).
    pub fn resource(&self, h: ResourceHandle) -> Option<&GraphResource> {
        self.graph_resources.resources.get(&h)
    }

    /// Build a complete read-only snapshot of the graph for the visualizer:
    /// passes in execution order (with `index` filled in), the resource table,
    /// and the active settings. Cheap to call per-frame - clones only the
    /// small declarative metadata, never Vulkan handles.
    pub fn snapshot(&self) -> RenderGraphSnapshot {
        let passes = self
            .passes
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut info = p.graph_info();
                info.index = i;
                info
            })
            .collect();
        let resources = self
            .graph_resources
            .resources
            .values()
            .map(|r| ResourceInfo {
                handle: r.handle,
                res_type: r.res_type.clone(),
                allocated: r.image.is_some(),
            })
            .collect();
        RenderGraphSnapshot {
            passes,
            resources,
            settings: self.settings.clone(),
        }
    }

    /// Recomputes the pass edge bucket cache and last writer mapping when the graph
    /// topology changes (e.g. at build time or after `add_pass`).
    fn rebuild_topology_cache(&mut self) {
        self.pass_edges = build_pass_edges(self.passes.len(), &self.edges);
        self.last_writers = build_last_writers(&self.edges);
    }

    /// 追加 a pass to an already-built 图 (e.g. ForwardPass / GtaoPass /
    /// PostPass, registered after the shadow map's resources are allocated so
    /// the scene can bind the shadow view). Runs `setup` on the new pass
    /// (merging its declared resources into the graph) and appends it to the
    /// execution order.
    pub fn add_pass(&mut self, mut pass: Box<dyn RenderPassNode + 'static>) {
        let pass_idx = self.passes.len();
        let mut b = RenderGraphBuilder::new()
            .settings(&self.settings)
            .pass_idx_offset(pass_idx);
        pass.setup(&mut b, &self.settings);
        for (h, r) in b.resources {
            self.graph_resources.resources.insert(h, r);
        }
        // Edges were recorded with pass_idx = offset + b.passes.len() == pass_idx
        // (b is a fresh builder with no passes pushed); sanity-check before merging.
        for mut e in b.edges {
            debug_assert_eq!(e.pass_idx, pass_idx);
            e.pass_idx = pass_idx;
            self.lifecycles
                .entry(e.usage.handle)
                .or_default()
                .update(&e);
            self.edges.push(e);
        }
        self.passes.push(pass);
        self.rebuild_topology_cache();
    }

    /// Drop all cached image layouts. Called after `recreate_swapchain` (where
    /// every per-swapchain-image attachment is rebuilt) so stale layout state
    /// doesn't suppress the first-frame barriers.
    pub fn reset_layouts(&mut self) {
        self.state_tracker.reset();
    }

    /// Validate declared edges: warn on reads before writes (potential
    /// cross-frame / ordering issue) and log an error on dependency cycles.
    /// Execution order is never reordered (the registration order in
    /// `GraphRenderer::new` reflects physical dependencies).
    fn validate_edges(&self) {
        use std::collections::HashSet;
        // Per-handle write-before-read check.
        let mut last_write: HashMap<ResourceHandle, usize> = HashMap::new();
        for e in &self.edges {
            match e.kind {
                EdgeKind::Write => {
                    last_write.insert(e.usage.handle, e.pass_idx);
                }
                EdgeKind::Read => match last_write.get(&e.usage.handle) {
                    Some(w) if *w > e.pass_idx => {
                        log::warn!(
                            "render-graph: pass {} reads {:?} before pass {} writes it \
                             (cross-frame dependency? ensure manual barriers cover this)",
                            e.pass_idx,
                            e.usage.handle,
                            w
                        );
                    }
                    _ => {}
                },
            }
        }
        // Cycle detection: pass A -> pass B if A writes a handle B reads.
        let n = self.passes.len();
        let mut adj: Vec<HashSet<usize>> = vec![HashSet::new(); n];
        for e in &self.edges {
            if e.kind == EdgeKind::Read {
                if let Some(w) = last_write.get(&e.usage.handle) {
                    if *w != e.pass_idx {
                        adj[*w].insert(e.pass_idx);
                    }
                }
            }
        }
        let mut color = vec![0u8; n]; // 0=white,1=gray,2=black
        let mut has_cycle = false;
        fn dfs(u: usize, adj: &[HashSet<usize>], color: &mut [u8], has_cycle: &mut bool) {
            color[u] = 1;
            for &v in &adj[u] {
                match color[v] {
                    1 => {
                        *has_cycle = true;
                    }
                    0 => dfs(v, adj, color, has_cycle),
                    _ => {}
                }
            }
            color[u] = 2;
        }
        for s in 0..n {
            if color[s] == 0 {
                dfs(s, &adj, &mut color, &mut has_cycle);
            }
        }
        if has_cycle {
            log::error!("render-graph: dependency cycle detected among passes");
        }
    }

    /// Run all registered passes in order, recording into `ctx.cmd`.
    ///
    /// Before each pass, the graph inspects that pass's declared **read** edges
    /// and emits a `vkCmdPipelineBarrier` per resource whose cached layout
    /// differs from the read's `usage.layout`. The barrier's `src` stage/access
    /// come from the last **write** edge on that handle (the pass that left the
    /// image in its current layout); if no writer is known, `TOP_OF_PIPE` /
    /// empty access is used (initial-transition semantics). After each pass,
    /// its **write** edges update the cached layout (no barrier emitted - the
    /// pass's own render pass performs that transition via `final_layout`).
    ///
    /// Note: cross-frame reads (e.g. GTAO's double-buffered AO fed back to
    /// `ForwardPass`) and the 交换链 `-> PRESENT_SRC_KHR` 过渡 are NOT
    /// graph edges and remain manual (see the pass-level comments). The layout
    /// cache only tracks the four graph-flow handles (shadow / scene depth /
    /// normal / HDR color).
    pub fn execute(&mut self, ctx: &RenderContext) -> Result<()> {
        self.execute_with_timer(ctx, None)
    }

    /// 同上，但记录每个 pass 的 GPU 时间戳（#518）。
    ///
    /// `timer` 为 `Some` 时，pass 首尾各写一个时间戳；时间戳池由调用方拥有，
    /// 结果也要等下一帧再由调用方读回（避免 stall）。
    pub fn execute_with_timer(
        &mut self,
        ctx: &RenderContext,
        mut timer: Option<&mut crate::gpu_timer::GpuTimer>,
    ) -> Result<()> {
        self.graph_resources.clear_frame();

        // Throttle the BARRIER_PROBE trace lines to once per second so the log
        // isn't flooded at frame rate. The probe is a debugging aid for the
        // automatic barrier pipeline; gating it here avoids passing `Instant`
        // state through the free functions.
        let probe = if self.last_barrier_probe.elapsed().as_secs_f32() >= 1.0 {
            self.last_barrier_probe = Instant::now();
            true
        } else {
            false
        };

        for (pass_idx, pass) in self.passes.iter_mut().enumerate() {
            // Build and emit barriers for this pass's read edges, then
            // update the tracker with write edges after the pass executes.
            let (barriers, src_stage, dst_stage) = self.state_tracker.build_read_barriers(
                &self.pass_edges[pass_idx],
                &self.last_writers,
                ctx.image_index,
                &self.graph_resources,
            );
            if !barriers.is_empty() {
                let src_stage = if src_stage.is_empty() {
                    vk::PipelineStageFlags::TOP_OF_PIPE
                } else {
                    src_stage
                };
                let dst_stage = if dst_stage.is_empty() {
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE
                } else {
                    dst_stage
                };
                unsafe {
                    ctx.device.cmd_pipeline_barrier(
                        ctx.cmd,
                        src_stage,
                        dst_stage,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &barriers,
                    );
                }
            }

            if probe {
                for e in &self.pass_edges[pass_idx] {
                    if e.kind == EdgeKind::Read {
                        let st = self.state_tracker.get(e.usage.handle, ctx.image_index);
                        log::trace!(
                            "BARRIER_PROBE pass {} {} {:?}: tracked={:?} desired={:?} image_index={}",
                            pass_idx,
                            if e.kind == EdgeKind::Read { "read" } else { "write" },
                            e.usage.handle,
                            st.layout,
                            e.usage.layout,
                            ctx.image_index
                        );
                    }
                }
            }

            let timed = timer
                .as_deref_mut()
                .map(|timer| timer.begin_pass(ctx.device, ctx.cmd, ctx.frame_index, pass.name()))
                .unwrap_or(false);
            pass.execute(ctx, &mut self.graph_resources)?;
            if timed {
                if let Some(timer) = timer.as_deref_mut() {
                    timer.end_pass(ctx.device, ctx.cmd, ctx.frame_index);
                }
            }

            self.state_tracker
                .apply_writes(&self.pass_edges[pass_idx], ctx.image_index);
        }

        Ok(())
    }

    /// Allocate (or re-use) Vulkan resources for all declared graph resources.
    /// Called once at startup or when the graph topology changes.
    pub fn allocate_resources(
        &mut self,
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<()> {
        for res in self.graph_resources.resources.values_mut() {
            if res.image.is_some() {
                continue; // already allocated
            }
            match &res.res_type {
                ResourceType::ColorAttachment {
                    format,
                    extent,
                    sample_count,
                } => {
                    let (image, view, memory) = create_transient_image(
                        device,
                        mem_props,
                        *format,
                        *extent,
                        *sample_count,
                        vk::ImageUsageFlags::COLOR_ATTACHMENT
                            | vk::ImageUsageFlags::INPUT_ATTACHMENT
                            | vk::ImageUsageFlags::STORAGE,
                    )?;
                    res.image = Some(image);
                    res.image_view = Some(view);
                    res.memory = Some(memory);
                }
                ResourceType::DepthAttachment {
                    extent,
                    sample_count,
                } => {
                    let (image, view, memory) = create_transient_image(
                        device,
                        mem_props,
                        vk::Format::D32_SFLOAT,
                        *extent,
                        *sample_count,
                        vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT
                            | vk::ImageUsageFlags::INPUT_ATTACHMENT
                            | vk::ImageUsageFlags::SAMPLED,
                    )?;
                    res.image = Some(image);
                    res.image_view = Some(view);
                    res.memory = Some(memory);
                }
                ResourceType::StorageImage { format, extent } => {
                    let (image, view, memory) = create_transient_image(
                        device,
                        mem_props,
                        *format,
                        vk::Extent2D {
                            width: extent.width,
                            height: extent.height,
                        },
                        vk::SampleCountFlags::TYPE_1,
                        vk::ImageUsageFlags::STORAGE
                            | vk::ImageUsageFlags::TRANSFER_DST
                            | vk::ImageUsageFlags::SAMPLED,
                    )?;
                    res.image = Some(image);
                    res.image_view = Some(view);
                    res.memory = Some(memory);
                }
                ResourceType::StorageBuffer { size: _ } => {
                    // Buffers allocated on demand by the pass that owns them
                    // (e.g. SHARC buffers are created in SharcPass::setup).
                }
            }
        }
        Ok(())
    }

    /// Look up a graph-managed image view by resource handle.
    /// Returns `None` if the handle does not exist or the resource has no view.
    pub fn image_view(&self, h: ResourceHandle) -> Option<vk::ImageView> {
        self.graph_resources.image_view(h)
    }

    /// Destroy all owned Vulkan resources.
    pub fn destroy(&mut self, device: &ash::Device) {
        for res in self.graph_resources.resources.values() {
            unsafe {
                if let Some(view) = res.image_view {
                    device.destroy_image_view(view, None);
                }
                if let Some(image) = res.image {
                    device.destroy_image(image, None);
                }
                if let Some(mem) = res.memory {
                    device.free_memory(mem, None);
                }
            }
        }
        self.graph_resources.resources.clear();
    }
}

impl Drop for RenderGraph {
    fn drop(&mut self) {
        // Resources should be destroyed explicitly via destroy().
        // If not, they leak — we can't call device destroy in Drop without
        // holding a device reference. This is intentional: the owner of the
        // graph (Renderer/Engine) must call destroy() before dropping.
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pick the image aspect mask for a layout transition barrier: depth/stencil
/// layouts use the DEPTH aspect (this project uses D32_SFLOAT, no separate
/// stencil), all color/sample/storage layouts use COLOR.
fn aspect_mask_for_layout(layout: vk::ImageLayout) -> vk::ImageAspectFlags {
    match layout {
        vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL
        | vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL
        | vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL
        | vk::ImageLayout::DEPTH_READ_ONLY_OPTIMAL
        | vk::ImageLayout::STENCIL_ATTACHMENT_OPTIMAL
        | vk::ImageLayout::STENCIL_READ_ONLY_OPTIMAL => vk::ImageAspectFlags::DEPTH,
        _ => vk::ImageAspectFlags::COLOR,
    }
}

/// Build a `handle -> &ResourceUsage` map of the last writer (highest pass_idx
/// that writes the handle). Readers use this to fill a barrier's src
/// stage/access; a handle with no writer uses `TOP_OF_PIPE` / empty access
/// (initial-transition semantics).
fn build_last_writers(edges: &[ResourceEdge]) -> HashMap<ResourceHandle, ResourceUsage> {
    let mut map: HashMap<ResourceHandle, (usize, &ResourceUsage)> = HashMap::new();
    for e in edges {
        if e.kind == EdgeKind::Write {
            match map.get(&e.usage.handle) {
                Some((prev_idx, _)) if *prev_idx >= e.pass_idx => {}
                _ => {
                    map.insert(e.usage.handle, (e.pass_idx, &e.usage));
                }
            }
        }
    }
    map.into_iter().map(|(h, (_, u))| (h, u.clone())).collect()
}

/// Create a graph-owned 2D image and bind memory for it.
///
/// Lazy (`LAZILY_ALLOCATED`) memory is only legal for images created with
/// `TRANSIENT_ATTACHMENT`, and such an image must not also be `SAMPLED` or
/// `STORAGE`. Every resource this graph allocates today is consumed by other
/// passes (color -> post, depth/shadow -> sampled), so they all end up in
/// `DEVICE_LOCAL`: a sampled depth image in a TBDR lazy heap produces
/// validation errors and undefined reads.
fn create_transient_image(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    format: vk::Format,
    extent: vk::Extent2D,
    sample_count: vk::SampleCountFlags,
    usage: vk::ImageUsageFlags,
) -> anyhow::Result<(vk::Image, vk::ImageView, vk::DeviceMemory)> {
    let flags = vk::ImageCreateFlags::empty();
    let _ = flags; // suppress unused warning

    let image_create_info = vk::ImageCreateInfo {
        image_type: vk::ImageType::TYPE_2D,
        format,
        extent: vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        },
        mip_levels: 1,
        array_layers: 1,
        samples: sample_count,
        tiling: vk::ImageTiling::OPTIMAL,
        usage,
        sharing_mode: vk::SharingMode::EXCLUSIVE,
        initial_layout: vk::ImageLayout::UNDEFINED,
        ..Default::default()
    };

    let image = unsafe { device.create_image(&image_create_info, None) }?;
    let req = unsafe { device.get_image_memory_requirements(image) };

    // For real transient attachments, prefer LAZILY_ALLOCATED 内存 类型.
    let lazy = lazy_allocation_allowed(usage);
    debug_assert!(
        !lazy
            || !(usage.contains(vk::ImageUsageFlags::SAMPLED)
                || usage.contains(vk::ImageUsageFlags::STORAGE)),
        "LAZILY_ALLOCATED requires TRANSIENT_ATTACHMENT without SAMPLED/STORAGE"
    );
    let mem_type = if lazy {
        find_memory_type(
            mem_props,
            req.memory_type_bits,
            vk::MemoryPropertyFlags::LAZILY_ALLOCATED,
        )
        .or_else(|| {
            // Fallback to device-local if no lazy type available (non-TBDR GPU)
            find_memory_type(
                mem_props,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
        })
        .ok_or_else(|| anyhow::anyhow!("no suitable memory type for transient image"))?
    } else {
        find_memory_type(
            mem_props,
            req.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| anyhow::anyhow!("no suitable memory type for storage image"))?
    };

    let memory = unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo {
                allocation_size: req.size,
                memory_type_index: mem_type,
                ..Default::default()
            },
            None,
        )
    }?;
    unsafe { device.bind_image_memory(image, memory, 0) }?;

    let aspect = if format == vk::Format::D32_SFLOAT {
        vk::ImageAspectFlags::DEPTH
    } else {
        vk::ImageAspectFlags::COLOR
    };

    let view = unsafe {
        device.create_image_view(
            &vk::ImageViewCreateInfo {
                image,
                view_type: vk::ImageViewType::TYPE_2D,
                format,
                subresource_range: vk::ImageSubresourceRange {
                    aspect_mask: aspect,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                ..Default::default()
            },
            None,
        )
    }?;

    Ok((image, view, memory))
}

/// Whether `usage` permits backing the image with `LAZILY_ALLOCATED` memory.
///
/// Vulkan only allows that memory for `TRANSIENT_ATTACHMENT` images, and the
/// same image may not be `SAMPLED`/`STORAGE` (so its contents need never be
/// written back to the heap).
fn lazy_allocation_allowed(usage: vk::ImageUsageFlags) -> bool {
    usage.contains(vk::ImageUsageFlags::TRANSIENT_ATTACHMENT)
        && !usage.contains(vk::ImageUsageFlags::SAMPLED)
        && !usage.contains(vk::ImageUsageFlags::STORAGE)
}

fn find_memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..mem_props.memory_type_count).find(|&i| {
        (type_filter & (1 << i)) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(flags)
    })
}

#[cfg(test)]
#[path = "render_graph_tests.rs"]
mod tests;
