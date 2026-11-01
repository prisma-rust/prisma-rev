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

/// A typed handle to a graph-managed 资源 图像 缓冲区
/// The inner `u32` is an 索引 into the graph's 资源 表
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceHandle(pub u32);

/// Well-known graph-edge 资源 handles published by `ForwardPass` and 读取
/// by downstream passes (`GtaoPass`, `PostPass`). Fixed (not counter-based)
/// so a pass added later can 引用 them without knowing the upstream
/// pass's 内部 handle field. The graph's `next_handle` 计数器 is kept
/// below this range (see `create_resource_at`), so there is no 碰撞
pub const FORWARD_DEPTH_H: ResourceHandle = ResourceHandle(1000);
pub const FORWARD_NORMAL_H: ResourceHandle = ResourceHandle(1001);
pub const FORWARD_COLOR_H: ResourceHandle = ResourceHandle(1002);
/// PT (path tracing) 输出 颜色 — replaces FORWARD_COLOR_H when
/// `RenderSettings.render_mode == PathTrace`. Written by PathTracePass
/// as a storage+sampled 图像 读取 by PostPass for tonemapping.
pub const PT_COLOR_H: ResourceHandle = ResourceHandle(1003);
/// Directional light depth shadow map — written by `ShadowMapPass` and
/// sampled in `ForwardPass` fragment shader for shadowing.
pub const SHADOW_MAP_H: ResourceHandle = ResourceHandle(1004);

impl ResourceHandle {
    pub const INVALID: ResourceHandle = ResourceHandle(u32::MAX);
}

/// 资源 类型 for graph-managed attachments.
#[derive(Clone, Debug)]
pub enum ResourceType {
    /// 颜色 附件 (GBuffer 层 高动态范围 输出 etc.)
    ColorAttachment {
        format: vk::Format,
        extent: vk::Extent2D,
        sample_count: vk::SampleCountFlags,
    },
    /// Depth/stencil 附件
    DepthAttachment {
        extent: vk::Extent2D,
        sample_count: vk::SampleCountFlags,
    },
    /// 存储 图像 计算 pass 输出 读取 by later passes)
    StorageImage {
        format: vk::Format,
        extent: vk::Extent3D,
    },
    /// 存储 缓冲区 (SHARC hash/accumulation/resolved buffers)
    StorageBuffer { size: u64 },
}

/// 描述 of a 资源 a pass needs — either reads from or writes to.
#[derive(Clone, Debug)]
pub struct ResourceUsage {
    pub handle: ResourceHandle,
    pub access: vk::AccessFlags,
    pub stage: vk::PipelineStageFlags,
    pub layout: vk::ImageLayout,
}

/// Direction of a declared 资源 edge. 读取 edges cause the 图 to
/// 过渡 the 图像 into `usage.layout` (with src from the 最后一个 writer);
/// 写入 edges record the 布局 the pass leaves the 图像 in (via its 渲染
/// pass `final_layout`), so the 下一个 reader's 屏障 knows the true
/// `old_layout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    Read,
    Write,
}

/// One declared 资源 访问 (a 读取 or 写入 edge) for dependency
/// 分辨率 and automatic 屏障 insertion.
#[derive(Clone, Debug)]
pub struct ResourceEdge {
    pub pass_idx: usize,
    pub usage: ResourceUsage,
    pub kind: EdgeKind,
}

/// Per-resource lifecycle span `[first_write_pass, last_read_pass]`, computed
/// at 构建 时间 Currently only surfaced to the visualizer; reserved as 输入
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
// The engine-side viz must not 借用 `RenderGraph` (its passes are 私有
// trait objects) nor 触摸 `vk::*` handles inside the UI 闭包 These
// plain-data structs are produced by `RenderGraph::snapshot` + each pass's
// `RenderPassNode::graph_info`, cloned once per 帧 and consumed by the UI.
// ---------------------------------------------------------------------------

/// 渲染 众数 selector — 完整 rasterized PBR vs real-time path tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RenderMode {
    /// 标准 rasterized PBR 管线 (ForwardPass + ShadowMap + GTAO + Post).
    #[default]
    Raster,
    /// Real-time path tracing (PathTracePass 计算 + Post).
    PathTrace,
}

/// 粗 classification of a pass for visualization (coloring / iconography).
/// Kept in sync with the concrete pass structs that 覆盖 `graph_info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PassKind {
    /// Rasterized depth-only shadow 映射表 (`ShadowMapPass`).
    Shadow,
    /// 向前 PBR scene 渲染 (`ForwardPass`).
    Forward,
    /// Half-resolution screen-space ambient 遮挡 (`GtaoPass`).
    Gtao,
    /// Fullscreen 色调映射 / present (`PostPass`).
    Post,
    /// Real-time path tracing 计算 pass (`PathTracePass`).
    Pt,
    /// 离屏 RenderTexture 渲染 (`RenderToTexturePass`).
    Rt,
    /// Unrecognized pass future / experimental).
    #[default]
    Unknown,
}

/// 静态 描述 of one graph-managed 资源 for the visualizer.
/// Mirrors the relevant subset of [`GraphResource`] without exposing Vulkan
/// handles.
#[derive(Clone, Debug)]
pub struct ResourceInfo {
    pub handle: ResourceHandle,
    pub res_type: ResourceType,
    /// `true` once `allocate_resources` has created the backing 图像
    pub allocated: bool,
}

/// 静态 描述 of one pass for the visualizer: its declared 资源
/// edges (`inputs` = handles it reads via `GraphResources::published_view`,
/// `outputs` = handles it publishes) plus a 粗 kind for coloring.
///
/// Side-inputs that bypass the 图 (shadow 视图 IBL 集合 previous-frame 环境光遮蔽
/// bound via `set_ao`) are NOT listed here - they are surfaced as human-readable
/// notes by the viz instead, since they don't 流程 through `GraphResources`.
#[derive(Clone, Debug)]
pub struct PassInfo {
    /// 执行 索引 (filled in by `RenderGraph::snapshot`).
    pub index: usize,
    pub name: String,
    pub kind: PassKind,
    /// 资源 handles this pass reads from upstream passes.
    pub inputs: Vec<ResourceHandle>,
    /// 资源 handles this pass publishes for downstream passes.
    pub outputs: Vec<ResourceHandle>,
}

/// A 完整 read-only 快照 of the 渲染 图 passes in 执行
/// order, the 资源 表 and the 激活 settings. Produced per-frame by
/// [`RenderGraph::snapshot`].
#[derive(Clone, Debug)]
pub struct RenderGraphSnapshot {
    pub passes: Vec<PassInfo>,
    pub resources: Vec<ResourceInfo>,
    pub settings: RenderSettings,
}

/// Shadow 渲染 策略
///
/// Selected per-frame by [`RenderSettings::resolve_shadow`] using probed
/// ray-tracing capabilities, so the running path adapts to the GPU. Mirrors
/// `docs/DESIGN.md` §2.3: `VK_KHR_ray_query` present → RayQuery 软体 shadow;
/// otherwise fall 后 to a rasterized depth-only shadow 映射表
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ShadowMode {
    /// No shadows.
    None,
    /// Rasterized depth-only shadow 映射表 (always available; the 回退 path).
    Raster,
    /// RayQuery inline 软体 shadow (requires `VK_KHR_ray_query` + a 内置 TLAS).
    RayQuery,
    /// Automatic: RayQuery when available and RT is 启用 else 光栅化
    #[default]
    Auto,
}

/// Quality / 特性 settings that passes consult at 执行 时间
/// These are the runtime-switchable knobs described in
/// `docs/mobile-raytracing-gi-design.md`.
#[derive(Clone, Debug)]
pub struct RenderSettings {
    /// GBuffer 颜色 格式 toggle.
    /// `true` = RGBA32F (quality), `false` = R10G10B10A2 带宽 默认
    pub gbuffer_high_precision: bool,

    /// 射线 tracing master switch.
    pub ray_tracing_enabled: bool,

    /// 射线 查询 分辨率 音阶 1.0 = 完整 res, 0.5 = half res 默认
    /// 设置 to 1.0 disables half-resolution (user wants 完整 quality).
    pub ray_query_resolution_scale: f32,

    /// SHARC hash-map 容量 (number of voxel slots).
    /// Mobile 默认 2^20 (1M). Desktop: 2^23 (8M).
    pub sharc_capacity: u32,

    /// SHARC scene 音阶 — controls voxel 物理 大小
    pub sharc_scene_scale: f32,

    /// Shadow 策略 `Auto` 默认 picks RayQuery when RT is 启用 and
    /// `VK_KHR_ray_query` is supported, otherwise falls 后 to the rasterized
    /// shadow 映射表 See [`ShadowMode`].
    pub shadow_mode: ShadowMode,

    /// 渲染 众数 光栅化 (PBR) or PathTrace (real-time PT).
    pub render_mode: RenderMode,

    /// 最大 path 深度 (bounces) for path tracing.
    pub pt_max_bounces: u32,
    /// 最大值 world-space 长度 of PT primary + shadow rays. Smaller values cut
    /// long-range bounces (and distant shadow casters) — an artistic focus/
    /// fog 控制 and a cost cap on huge scenes.
    pub pt_ray_max_distance: f32,
    /// 最大 iterations (samples per 像素 for path tracing.
    /// 0 = accumulate forever 默认
    pub pt_max_iterations: u32,

    /// 内部渲染分辨率 `[宽度, 高度]`（#613）；`[0, 0]` = 关闭。
    ///
    /// 开启后场景（网格 / 2D 精灵 / 瓦片 / 粒子 / PT）按这个尺寸渲染，
    /// `PostPass` 再放大到窗口。UI 仍在窗口分辨率上绘制。
    pub internal_resolution: [u32; 2],

    /// 内部图像放大到窗口的方式（见 [`InternalScaleMode`]）。
    pub internal_scale_mode: InternalScaleMode,
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
            render_mode: RenderMode::Raster,
            pt_max_bounces: 3,
            pt_ray_max_distance: 1000.0,
            pt_max_iterations: 0,
            internal_resolution: [0, 0],
            internal_scale_mode: InternalScaleMode::Stretch,
        }
    }
}

impl RenderSettings {
    /// 解析 the effective shadow 众数 given probed capabilities.
    ///
    /// `Auto` selects RayQuery when 射线 tracing is 启用 and
    /// `VK_KHR_ray_query` is supported, otherwise falls 后 to the
    /// rasterized shadow 映射表 Explicit modes pass through unchanged.
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

/// One 绘制 call's 静态 data, supplied by the engine each 帧
/// 图 passes 读取 these to record geometry draws into their attachments.
#[derive(Clone)]
pub struct DrawItem {
    /// GPU 网格 handle (from [`crate::managers::RenderMeshManager`]).
    pub mesh: MeshHandle,
    /// 模型 矩阵 世界 变换 for this 实例
    pub model: [[f32; 4]; 4],
    /// 材质 SSBO 槽 索引 into `RenderMaterialManager`'s
    /// `GpuMaterial[]` 缓冲区 for the bindless PBR path. `None` -> 槽 0
    /// (the 回退 材质 `app.rs` resolves `MaterialHandle` -> 槽
    /// via `mat_map` when building the 绘制 列表 so passes can 推送 the
    /// 槽 directly without a per-draw `slot_of()` lookup.
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

/// Display-referred color grading applied by `PostPass` **after** tonemapping
/// (#502).
///
/// All three knobs are no-ops at their defaults, so an untouched
/// `PostGrade::default()` produces exactly the pre-#502 image:
///
/// - `contrast`: scale around mid-grey `0.5` (`1.0` = unchanged);
/// - `saturation`: Rec.709 luma interpolation (`1.0` = unchanged, `0.0` = grey);
/// - `vignette`: corner falloff strength (`0.0` = off, `1.0` = corners black).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PostGrade {
    pub contrast: f32,
    pub saturation: f32,
    pub vignette: f32,
}

impl Default for PostGrade {
    fn default() -> Self {
        Self {
            contrast: 1.0,
            saturation: 1.0,
            vignette: 0.0,
        }
    }
}

impl PostGrade {
    /// `true` when every knob is at its default (the pass can skip the math).
    pub fn is_neutral(&self) -> bool {
        *self == Self::default()
    }

    /// Clamp to the ranges the shader expects (`contrast`/`saturation` never
    /// negative, `vignette` in `0..=1`).
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
        }
    }
}

/// Per-frame scene + lighting 状态 shared with every pass via [`RenderContext`].
///
/// The `GraphRenderer` populates this once per 帧 (before driving the
/// 图 with the camera/light UBO, the 绘制 列表 and the light-space
/// view-projection used by both the shadow pass and the lighting pass
pub struct GraphFrame<'a> {
    /// Per-frame UBO 相机 + 光源 Its 描述符 集合 is bound at 集合 0.
    pub frame_ubo: &'a FrameUBO,
    /// 绘制 列表 for the 当前 帧
    pub draw_list: &'a [DrawItem],
    /// ECS-authored gizmos for this frame.
    pub gizmos: &'a [crate::gizmo::GizmoInstance],
    /// ECS-authored 2D sprites for this frame (sorted back-to-front).
    pub sprite_items: &'a [SpriteDrawItem],
    /// `Light2D` entities for this frame, strongest first (#611).
    pub sprite_lights: &'a [SpriteLight],
    /// World-space particle billboards for this frame (sorted back-to-front).
    pub particle_items: &'a [ParticleDrawItem],
    /// ECS-authored skybox parameters (`None` = skip the sky draw).
    pub skybox: Option<crate::skybox_pass::SkyboxSettings>,
    /// 网格 管理器 — passes 解析 [`DrawItem::mesh`] handles to GPU buffers.
    pub mesh_manager: &'a RenderMeshManager,
    /// Light-space view-projection 正交 used by the shadow pass and
    /// by the lighting pass to project 世界 positions into the shadow 映射表
    pub light_view_proj: [[f32; 4]; 4],
    /// Effective shadow 众数 for this 帧 (after 能力 分辨率
    pub shadow_mode: ShadowMode,
    /// PBR 分量 toggle bitmask (15 bits, see `scene_frag.slang`
    /// `PBR_FLAG_*`). 0 = all components neutral (raw baseColor). Forwarded
    /// to the bindless 推送 常量 `debug_flags` field.
    pub debug_flags: u32,
    /// Inverse-view 旋转 (upper-left 3x3 of inverse(view)), packed as mat4.
    /// Used by the skybox pass to 旋转 view-space look directions into 世界
    /// 空间 Because the 视图 矩阵 is a 刚体 变换 this is just the
    /// transpose of the upper-left 3x3 of 视图 (the 旋转 basis), with w=0
    /// on the 4th 行
    pub inv_view_rot: [[f32; 4]; 4],
    /// 完整 world-space view-projection 片段 = proj * 视图 including the
    /// 表面 旋转 Used by the world-space gizmo (drawn on 顶部 of the
    /// scene) so the axes track the 相机
    pub view_proj: [[f32; 4]; 4],
    /// Point lights collected from the ECS this 帧 rewritten into the
    /// scene shader's 光源 SSBO. Forwarded to `ForwardPass::execute` so it can
    /// 更新 its 描述符 集合 without `GraphRenderer` poking it directly.
    pub lights: &'a [GpuLight],
    /// Previous-frame GTAO 可见性 视图 (1-frame 延迟 `ForwardPass`
    /// binds this as its 环境光遮蔽 输入 it reads `ao[(frame + 1) % 2]` written by
    /// `GtaoPass` 最后一个 帧 Forwarded via `GraphFrame` so the 图 not
    /// `GraphRenderer`, owns the cross-pass wiring.
    pub ao_view: vk::ImageView,
    /// 色调映射 众数 for `PostPass` (Reinhard / ACES / ...). Forwarded so
    /// `PostPass::execute` reads it from the 图 context.
    pub tonemap_mode: u32,
    /// Post-process anti-aliasing mode for `PostPass` (0 = off, 1 = FXAA).
    /// Requires no extra targets: FXAA runs inside the tonemap pass, so the
    /// offscreen (editor viewport) and swapchain paths get the same result.
    pub aa_mode: u32,
    /// PostPass 调试 render-target viewer (Tab 调 0 = 法线 tonemapped
    /// 高动态范围 1 = linearized 深度 2 = view-space 法线 PostPass picks which
    /// published 视图 to 样本 based on this.
    pub debug_rt: u32,
    /// Display-referred color grading, applied in `PostPass` after tonemapping
    /// (#502). [`PostGrade::default`] is an exact no-op.
    pub grade: PostGrade,
    /// 投影 矩阵 entries `[2][2]` / `[3][2]` (column-major
    /// `m[col][row]`) used by PostPass to linearize the 深度 缓冲区 for the
    /// 调试 深度 视图 (`view_z = proj22 * d + proj32`).
    pub proj22: f32,
    pub proj32: f32,
    /// Inverse 投影 (used by `GtaoPass` to reconstruct view-space
    /// 半径 from screen-space samples). Forwarded via `GraphFrame`.
    pub inv_projection: [[f32; 4]; 4],
    /// 交换链 图像 views for the 当前 帧 Forwarded so `PostPass`
    /// can (re)build its per-swapchain-image framebuffers inside 执行
    /// (mirroring `ForwardPass::ensure_target`), instead of relying on
    /// `GraphRenderer` to 调用 `set_target` every 帧
    pub swapchain_views: &'a [vk::ImageView],
    /// 激活 渲染 众数 光栅化 vs PathTrace). Passes check this to decide
    /// whether to run (ForwardPass skips in PT 众数 PathTracePass skips in
    /// 光栅化 众数
    pub render_mode: RenderMode,
    /// Path tracing 最大值 bounces.
    pub pt_max_bounces: u32,
    /// 最大值 world-space 长度 of PT primary + shadow rays. Smaller values cut
    /// long-range bounces (and distant shadow casters), useful as an artistic
    /// "fog"/focus 控制 and to 限制 cost on huge scenes.
    pub pt_ray_max_distance: f32,
    /// 最大 iterations (samples per 像素 0 = accumulate forever.
    pub pt_max_iterations: u32,
    /// 相机 world-space position [x, y, z, light_count] from the 帧 UBO.
    pub camera_pos: [f32; 4],
    /// 光源 direction [x, y, z, intensity] from the 帧 UBO.
    pub light_dir: [f32; 4],
    /// 光源 颜色 [r, g, b, ambient] from the 帧 UBO. Forwarded to the
    /// path-trace pass so it can apply the scene's actual sun 颜色 instead
    /// of a hardcoded white (the rasterizer reads this via the FrameUBO).
    pub light_color: [f32; 4],
    /// Exposure multiplier applied to the final 高动态范围 颜色 before tonemapping.
    /// Forwarded from [`FrameInput`](crate::graph_renderer::FrameInput) so both
    /// ForwardPass (via FrameUBOData) and PathTracePass (via 推送 常量 apply
    /// the same exposure value from the 相机 实体
    pub exposure: f32,
    /// Analytic lights for path tracing (point/spot/area/directional).
    /// Written into the PT lights SSBO for multi-light NEE.
    pub pt_lights: &'a [PtAnalyticLight],
    /// When `true`, the path tracer should reset its accumulation 下一个 帧
    /// 集合 when directional-light properties change.
    pub pt_accum_dirty: bool,
    /// Whether a usable 相机 实体 was 找到 When `false`, the skybox and
    /// PT pass should be skipped so the 清空 颜色 (gray) shows through.
    pub has_camera: bool,
    /// Whether the active camera is orthographic; the PT pass casts parallel
    /// primary rays in that case (issue #10).
    pub camera_orthographic: bool,
    /// 清空 颜色 for the scene 颜色 附件 Applied by ForwardPass on
    /// render-pass 开始 可见 when the skybox and scene geometry are
    /// absent or 透明 (e.g. no-camera 回退 Matches the app-level
    /// `clear_color` 参数 passed to `render_system`.
    pub clear_color: [f32; 4],
    /// 是否在场景渲染中绘制地面参考网格。
    pub show_grid: bool,
}

/// Context passed to each pass's 执行
pub struct RenderContext<'a> {
    pub device: &'a ash::Device,
    pub context: &'a VulkanContext,
    pub settings: &'a RenderSettings,
    pub cmd: vk::CommandBuffer,
    pub frame_index: u32,
    /// 交换链 图像 索引 returned by `acquire_next_image`. 不同 from
    /// `frame_index` (which is the frame-in-flight 索引 with N 交换链
    /// images and 2 frames in flight, `frame_index` cycles 0..2 while
    /// `image_index` cycles 0..N. Passes that own per-swapchain-image resources
    /// (e.g. `ForwardPass`'s framebuffers) 索引 by this, not `frame_index`.
    pub image_index: u32,
    /// 当前 交换链 extent.
    pub extent: vk::Extent2D,
    /// 场景实际渲染的 extent（内部渲染分辨率，#613）。
    ///
    /// 没有开启内部分辨率时与 [`Self::extent`] 相同。`ForwardPass` / 路径追踪
    /// 按这个尺寸分配目标；`PostPass` / UI 叠加 / 截图仍用 `extent`。
    pub render_extent: vk::Extent2D,
    /// Per-frame scene + lighting 状态 (see [`GraphFrame`]).
    pub frame: &'a GraphFrame<'a>,
}

/// trait for a modular 渲染 pass
///
/// Each pass declares its 资源 needs via [`setup`] and records commands
/// in 执行 The 图 calls these in topological order.
pub trait RenderPassNode: std::any::Any {
    /// Human-readable name (for debugging / 性能分析
    fn name(&self) -> &str;

    /// Declare 资源 reads/writes. Called once during 图 编译
    /// The pass should register its needs via `graph.create_resource(...)` /
    /// `graph.read(...)` / `graph.write(...)`. `settings` is the 运行时
    /// 渲染 配置 (e.g. `gbuffer_high_precision`) so the pass
    /// can pick the 右 格式 for its attachments.
    fn setup(&mut self, graph: &mut RenderGraphBuilder, settings: &RenderSettings);

    /// Record Vulkan commands into `ctx.cmd`. `resources` is mutable so the
    /// pass can 发布 its 输出 views 深度 / 法线 / 高动态范围 for downstream
    /// passes to 读取 by handle.
    fn execute(&mut self, ctx: &RenderContext, resources: &mut GraphResources) -> Result<()>;

    /// Pre‑create pipelines / 着色器 modules at 加载 时间 so the 第一个 帧
    /// doesn't pay pipeline‑compilation cost. 默认 is a no‑op; passes with
    /// lazy pipelines 覆盖 this.
    fn warmup(
        &mut self,
        _device: &ash::Device,
        _context: &crate::context::VulkanContext,
    ) -> Result<()> {
        Ok(())
    }

    /// Read-only 快照 of this pass's declared 资源 edges + 粗 kind,
    /// for the render-graph visualizer. 默认 returns an "unknown" pass with
    /// no edges; concrete passes 覆盖 to populate `kind`/`inputs`/`outputs`.
    ///
    /// The 执行 索引 is filled in by [`RenderGraph::snapshot`] (the pass
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
// 资源 状态 tracker — automatic 屏障 derivation
// ---------------------------------------------------------------------------

/// Per-resource GPU 布局 + 访问 + 阶段 快照 for auto-barrier 逻辑
#[derive(Clone, Copy, Debug)]
pub struct ResourceState {
    pub layout: vk::ImageLayout,
    pub access: vk::AccessFlags,
    pub stage: vk::PipelineStageFlags,
}

/// Tracks per-`(handle, image_index)` 资源 状态 across passes and frames.
/// Used by `RenderGraph::execute` to derive automatic barriers between passes.
/// `reset()` clears all 状态 (called after 交换链 recreation).
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

    /// Look 上 当前 状态 returns UNDEFINED / 空 / TOP_OF_PIPE when
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

    /// Record that a 资源 is now in the given 状态 (called after a pass's
    /// 渲染 pass transitions it via `final_layout`).
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

    /// Batch-apply 写入 edges from a pass each 写入 edge records the 布局
    /// the pass leaves the 资源 in.
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

    /// 构建 barriers for all 读取 edges whose tracked 布局 differs from the
    /// declared 用法 Returns `(barriers, src_stage, dst_stage)`; callers
    /// should 发射 a single `vkCmdPipelineBarrier` with the union of stages.
    /// Skips resources already in the desired 布局 or 待发布.
    ///
    /// Each emitted 屏障 also updates the tracked 布局 to the reader's
    /// `usage.layout`, so a 秒 reader of the same 资源 in the same
    /// 帧 sees the post-transition 布局 as its `old_layout` instead of a
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

            // Advance the tracked 状态 to the post-barrier 布局 matching
            // the GPU 过渡 we just recorded. This must happen per-read,
            // not per-write: a 布局 change is driven by the 读取 edge here,
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

    /// 清空 all tracked 状态 交换链 recreation).
    pub fn reset(&mut self) {
        self.states.clear();
    }
}

impl Default for ResourceStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// A 资源 entry in the graph's 资源 表
#[derive(Clone)]
pub struct GraphResource {
    pub handle: ResourceHandle,
    pub res_type: ResourceType,
    /// Owning Vulkan 图像 (None until allocated).
    pub image: Option<vk::Image>,
    pub image_view: Option<vk::ImageView>,
    pub memory: Option<vk::DeviceMemory>,
}

/// 资源 表 passed to passes at 执行 时间
/// Besides the graph-owned images (allocated in `allocate_resources`), it
/// carries **pass-exported views + images** (e.g. `ForwardPass` publishes its
/// 深度 / 法线 / 高动态范围 views AND images here so downstream passes like
/// `GtaoPass` / `PostPass` can 读取 them by handle). This is the minimal
/// graph-edge 资源 handoff for PR-1: the 图 does not own the
/// underlying images (passes still 创建 their own framebuffers), but it is
/// the 通道 through which passes 交换 资源 handles instead of
/// `GraphRenderer` poking each pass
pub struct GraphResources {
    pub resources: HashMap<ResourceHandle, GraphResource>,
    /// Pass-published 图像 views, keyed by `ResourceHandle`.
    pub image_views: HashMap<ResourceHandle, vk::ImageView>,
    /// Pass-published images (handles), keyed by `ResourceHandle`. Needed by
    /// downstream passes that 发射 布局 barriers (which 引用 the 图像
    /// not the 视图
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

    /// 发布 an 图像 视图 under a handle so downstream passes can 读取 it.
    pub fn set_image_view(&mut self, h: ResourceHandle, view: vk::ImageView) {
        self.image_views.insert(h, view);
    }

    /// 发布 an 图像 under a handle (for downstream 布局 barriers).
    pub fn set_image(&mut self, h: ResourceHandle, image: vk::Image) {
        self.images.insert(h, image);
    }

    /// 读取 a 视图 published by an upstream pass
    pub fn published_view(&self, h: ResourceHandle) -> Option<vk::ImageView> {
        self.image_views.get(&h).copied()
    }

    /// 读取 an 图像 published by an upstream pass
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
// 图 构建器 — collects passes and 资源 declarations, then compiles.
// ---------------------------------------------------------------------------

pub struct RenderGraphBuilder {
    passes: Vec<Box<dyn RenderPassNode + 'static>>,
    resources: HashMap<ResourceHandle, GraphResource>,
    /// Declared read/write edges collected from passes' `setup`. Indexed by
    /// `pass_idx` (= `pass_idx_offset + passes.len()` at the moment
    /// `read_usage`/`write_usage` is called during setup, before `add_pass`
    /// pushes the pass The 偏移 is non-zero when this 构建器 is a
    /// temporary created by `RenderGraph::add_pass` to 集合 上 a pass that will
    /// be appended to an already-populated 图
    edges: Vec<ResourceEdge>,
    next_handle: u32,
    settings: RenderSettings,
    /// 索引 of the 第一个 pass this 构建器 will register, in the final
    /// graph's pass 列表 零 for a fresh 构建器 集合 to
    /// `RenderGraph::passes.len()` by `RenderGraph::add_pass` so edges declared
    /// during a pass's `setup` get the correct 绝对 `pass_idx`.
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

    /// 集合 the base pass 索引 for edges declared via this 构建器 Used by
    /// [`RenderGraph::add_pass`] so a pass appended to an already-built 图
    /// records edges with its true 绝对 `pass_idx` rather than 0.
    pub fn pass_idx_offset(mut self, offset: usize) -> Self {
        self.pass_idx_offset = offset;
        self
    }

    /// 覆盖 the 渲染 settings used when `setup` is called on passes.
    pub fn settings(mut self, settings: &RenderSettings) -> Self {
        self.settings = settings.clone();
        self
    }

    /// Register a pass Order of insertion = 执行 order (simple
    /// 线性 管线 for now; topological 排序 can be added later).
    pub fn add_pass(&mut self, pass: Box<dyn RenderPassNode + 'static>) {
        self.passes.push(pass);
    }

    /// 创建 a transient 资源 managed by the 图
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

    /// 创建 a transient 资源 at a specific handle (e.g. a well-known
    /// graph-edge handle like `FORWARD_DEPTH_H`). Used so downstream passes can
    /// 引用 a publisher's 输出 without knowing its 内部 field.
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

    /// Mark the pass currently being 集合 上 (i.e. the 下一个 one `add_pass`
    /// will 推送 at 索引 `self.passes.len()`) as reading a 资源 with
    /// the access/stage/layout it reads with. The 图 uses this to 插入 a
    /// `vkCmdPipelineBarrier` before the pass when the image's 当前 布局
    /// differs from `usage.layout`.
    ///
    /// Must be called from within `RenderPassNode::setup` (before `add_pass`
    /// pushes the pass calling it elsewhere panics.
    pub fn read_usage(&mut self, usage: ResourceUsage) {
        self.push_edge(usage, EdgeKind::Read);
    }

    /// Mark the pass currently being 集合 上 as writing a 资源 with the
    /// access/stage/layout it leaves the 图像 in (typically the 渲染 pass
    /// `final_layout`). The 图 records this as the resource's 当前
    /// 布局 after the pass executes, so the 下一个 reader's 屏障 knows the
    /// true `old_layout`. No 屏障 is emitted for the 写入 itself (the
    /// pass's 渲染 pass performs the 布局 过渡 implicitly).
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

    /// Mark a pass (by 索引 as reading a 资源
    /// Tracked for future 屏障 generation and topological 排序
    pub fn read(&mut self, pass_idx: usize, handle: ResourceHandle) {
        // future 推送 to a dependency 列表 for 屏障 generation
        let _ = (pass_idx, handle);
    }

    /// Mark a pass (by 索引 as writing a 资源
    /// Tracked for future 屏障 generation and topological 排序
    pub fn write(&mut self, pass_idx: usize, handle: ResourceHandle) {
        let _ = (pass_idx, handle);
    }

    /// 编译 into an 可执行文件 图
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

/// 计算 the `[first_write, last_read]` span per 资源 from the declared
/// edges. Used by the visualizer and reserved as 输入 for future TBDR 内存
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
// 可执行文件 图
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
    /// Per-`(handle, image_index)` 资源 状态 tracker, persisted across
    /// frames so cross-frame reads (e.g. GTAO's double-buffered 环境光遮蔽 keep their
    /// 布局 Keyed by `image_index` because `ForwardPass`/`PostPass` own
    /// per-swapchain-image attachments under the same handle.
    state_tracker: ResourceStateTracker,
    /// `[first_write, last_read]` span per 资源 for the visualizer and
    /// future aliasing.
    lifecycles: HashMap<ResourceHandle, ResourceLifecycle>,
    /// 最后一个 时间 the `BARRIER_PROBE` 跟踪 lines were emitted; throttled to
    /// once per 秒 so the 对数 isn't flooded at 帧 rate.
    last_barrier_probe: Instant,
}

impl RenderGraph {
    /// 借用 a registered pass by concrete 类型 (for lifecycle operations
    /// like `recreate_swapchain`, which must 调用 into a specific pass
    /// Returns `None` if no pass of that 类型 was registered.
    pub fn pass_mut<T: RenderPassNode + 'static>(&mut self) -> Option<&mut T> {
        self.passes
            .iter_mut()
            .find_map(|p| (&mut **p as &mut dyn std::any::Any).downcast_mut::<T>())
    }

    /// Immutable 借用 of a registered pass by concrete 类型 The read-only
    /// counterpart to [`pass_mut`](Self::pass_mut); used by the render-graph
    /// visualizer to pull live per-pass 状态 (extent / 格式 / image_count)
    /// without mutating the 图
    pub fn pass_ref<T: RenderPassNode + 'static>(&self) -> Option<&T> {
        self.passes
            .iter()
            .find_map(|p| (&**p as &dyn std::any::Any).downcast_ref::<T>())
    }

    /// 激活 渲染 settings 特性 knobs consulted by passes at 执行
    /// 时间 Exposed read-only for the visualizer's header 摘要
    pub fn settings(&self) -> &RenderSettings {
        &self.settings
    }

    /// 调用 [`warmup`](RenderPassNode::warmup) on every registered pass
    /// Designed to be called once after 图 construction so that lazy
    /// pipelines are compiled ahead of the 第一个 帧
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

    /// 迭代器 over all declared 图 resources (depth/color attachments,
    /// 存储 images). Exposed read-only for the visualizer.
    pub fn resources(&self) -> impl Iterator<Item = &GraphResource> {
        self.graph_resources.resources.values()
    }

    /// Look 上 a graph-managed 资源 by handle (immutable).
    pub fn resource(&self, h: ResourceHandle) -> Option<&GraphResource> {
        self.graph_resources.resources.get(&h)
    }

    /// 构建 a 完整 read-only 快照 of the 图 for the visualizer:
    /// passes in 执行 order (with 索引 filled in), the 资源 表
    /// and the 激活 settings. Cheap to 调用 per-frame - clones only the
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
    /// the scene can bind the shadow 视图 Runs `setup` on the new pass
    /// (merging its declared resources into the 图 and appends it to the
    /// 执行 order.
    pub fn add_pass(&mut self, mut pass: Box<dyn RenderPassNode + 'static>) {
        let pass_idx = self.passes.len();
        let mut b = RenderGraphBuilder::new()
            .settings(&self.settings)
            .pass_idx_offset(pass_idx);
        pass.setup(&mut b, &self.settings);
        for (h, r) in b.resources {
            self.graph_resources.resources.insert(h, r);
        }
        // Edges were recorded with pass_idx = 偏移 + b.passes.len() == pass_idx
        // (b is a fresh 构建器 with no passes pushed); sanity-check before merging.
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

    /// 放置 all cached 图像 layouts. Called after `recreate_swapchain` (where
    /// every per-swapchain-image 附件 is rebuilt) so stale 布局 状态
    /// doesn't suppress the first-frame barriers.
    pub fn reset_layouts(&mut self) {
        self.state_tracker.reset();
    }

    /// Validate declared edges: warn on reads before writes (potential
    /// cross-frame / ordering issue) and 对数 an 错误 on dependency cycles.
    /// 执行 order is never reordered (the registration order in
    /// `GraphRenderer::new` reflects 物理 dependencies).
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
    /// Before each pass the 图 inspects that pass's declared **read** edges
    /// and emits a `vkCmdPipelineBarrier` per 资源 whose cached 布局
    /// differs from the read's `usage.layout`. The barrier's `src` stage/access
    /// come from the 最后一个 **write** edge on that handle (the pass that 左 the
    /// 图像 in its 当前 布局 if no writer is known, `TOP_OF_PIPE` /
    /// 空 访问 is used (initial-transition semantics). After each pass
    /// its **write** edges 更新 the cached 布局 (no 屏障 emitted - the
    /// pass's own 渲染 pass performs that 过渡 via `final_layout`).
    ///
    /// 音符 cross-frame reads (e.g. GTAO's double-buffered 环境光遮蔽 fed 后 to
    /// `ForwardPass`) and the 交换链 `-> PRESENT_SRC_KHR` 过渡 are NOT
    /// 图 edges and remain manual (see the pass-level comments). The 布局
    /// cache only tracks the four graph-flow handles (shadow / scene 深度 /
    /// 法线 / 高动态范围 颜色
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

        // Throttle the BARRIER_PROBE 跟踪 lines to once per 秒 so the 对数
        // isn't flooded at 帧 rate. The probe is a debugging aid for the
        // automatic 屏障 管线 gating it here avoids passing `Instant`
        // 状态 through the free functions.
        let probe = if self.last_barrier_probe.elapsed().as_secs_f32() >= 1.0 {
            self.last_barrier_probe = Instant::now();
            true
        } else {
            false
        };

        for (pass_idx, pass) in self.passes.iter_mut().enumerate() {
            // 构建 and 发射 barriers for this pass's 读取 edges, then
            // 更新 the tracker with 写入 edges after the pass executes.
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

    /// Allocate (or re-use) Vulkan resources for all declared 图 resources.
    /// Called once at startup or when the 图 topology changes.
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

    /// Look 上 a graph-managed 图像 视图 by 资源 handle.
    /// Returns `None` if the handle does not exist or the 资源 has no 视图
    pub fn image_view(&self, h: ResourceHandle) -> Option<vk::ImageView> {
        self.graph_resources.image_view(h)
    }

    /// 销毁 all owned Vulkan resources.
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
        // Resources should be destroyed explicitly via 销毁
        // If not, they leak — we can't 调用 设备 销毁 in 放置 without
        // holding a 设备 引用 This is intentional: the 所有者 of the
        // 图 (Renderer/Engine) must 调用 销毁 before dropping.
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pick the 图像 宽高比 遮罩 for a 布局 过渡 屏障 depth/stencil
/// layouts use the 深度 宽高比 (this project uses D32_SFLOAT, no separate
/// 模板 all color/sample/storage layouts use 颜色
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

/// 构建 a `handle -> &ResourceUsage` 映射表 of the 最后一个 writer (highest pass_idx
/// that writes the handle). Readers use this to fill a barrier's src
/// stage/access; a handle with no writer uses `TOP_OF_PIPE` / 空 访问
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
            // 回退 to device-local if no lazy 类型 available (non-TBDR GPU)
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
