//! 用于 RenderGraph 路径的 ECS 驱动渲染系统
//!
//! 定义了 [`SceneChanges`] 快照结构体（相机、光源、派生矩阵）
//! 以及主 [`render_system`] 函数，它每帧查询 ECS 世界，
//! 构建平面绘制列表并提交给 [`GraphRenderer::render`]。

use glam::{self, Mat4, Vec3, Vec4};

use crate::dirty_router::{draw_items_signature, DirtyRouter};
use crate::render_settings::{RenderSettings, DEFAULT_SHADOW_DISTANCE};
use crate::scene;
use crate::scene::components::Camera;
use prism_ecs::World;
use prism_render::{
    DrawItem, FrameUBOData, GizmoInstance, GpuLight, GraphRenderer, PtAnalyticLight,
    SkyboxSettings, UiOverlayInput, GPU_LIGHT_KIND_POINT, GPU_LIGHT_KIND_SPOT, PT_LIGHT_MAX,
};
use prism_scene::CameraClearFlags;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 上一次提示的「被忽略相机」数量。
static LAST_IGNORED_CAMERAS: AtomicUsize = AtomicUsize::new(0);

/// 场景里有比主相机更多的启用相机时提示一次（#461）。
///
/// 整条渲染链是单相机的，只有 `depth` 最大的那台会出图。以前这件事完全不吭声，
/// 用户在编辑器里加第二台相机、调 `depth` / `clear_flags` / 背景色都看不到变化，
/// 很容易以为是自己参数错了。每帧都 warn 会刷屏，所以只在数量**变多**时提示。
fn warn_ignored_cameras(world: &World) {
    let ignored = scene::systems::camera::ignored_cameras(world).len();
    let previous = LAST_IGNORED_CAMERAS.swap(ignored, Ordering::Relaxed);
    if ignored > previous {
        log::warn!(
            "{ignored} extra enabled camera(s) are ignored: this build renders only the \
             highest-`depth` camera (see #461)"
        );
    }
}

/// 上一次提示的「被丢弃局部光」数量（数量变化时才 warn，避免刷屏）。
static LAST_DROPPED_LIGHTS: AtomicUsize = AtomicUsize::new(0);

/// 局部光超过 `LIGHT_MAX` 时提示一次（#468）。
///
/// 上限本身是前向着色器的硬约束（SSBO 固定长度），但用户不该在「加了灯没反应」
/// 之后自己猜。每帧都 warn 会刷屏，所以只在丢弃数量**变化**时打一条。
fn warn_dropped_lights(total: usize, dropped: usize) {
    let previous = LAST_DROPPED_LIGHTS.swap(dropped, Ordering::Relaxed);
    if dropped > 0 && dropped != previous {
        log::warn!(
            "{dropped} of {total} local lights are not rendered: the forward pass only keeps the {} most important ones (intensity x range^2 / distance^2), see #468",
            prism_render::LIGHT_MAX
        );
    }
}

/// 预缩放因子，取代旧的 GPU 端 `exposure / PI` 单位转换。
/// 勒克斯（或坎德拉）在 CPU 上乘以此因子，使着色器直接接收有效辐射度。
/// `exposure` 随后成为应用于最终合成 HDR 颜色的纯亮度乘数。
const LUX_TO_RADIANCE_SCALE: f32 = 1.0 / (10_000.0 * std::f32::consts::PI);

// ---------------------------------------------------------------------------
// SceneChanges
// ---------------------------------------------------------------------------

/// 快照 of all per-frame scene data 相机 lights, derived matrices).
///
/// Produced by [`collect_scene_changes`] and consumed by [`render_system`] to
/// 构建 the [`FrameUBOData`] and [`FrameInput`].
#[derive(Clone, Default)]
pub struct SceneChanges {
    pub view_proj: Mat4,
    pub eye: Vec3,
    pub view: Mat4,
    /// Unrotated 投影 (GTAO pass needs the raw 矩阵 for
    /// 片段 → view-space reconstruction; the surface-rotation is applied
    /// only to `view_proj`).
    pub projection: Mat4,
    pub inv_projection: Mat4,
    pub proj22: f32,
    pub proj32: f32,
    /// Direction TO the 光源 packed as `[x, y, z, intensity]`.
    pub light_direction: Vec4,
    /// 光源 颜色 + ambient, packed as `[r, g, b, ambient]`.
    pub light_color: Vec4,
    /// Light-space 正交 view-projection (shadow 映射表
    pub light_view_proj: Mat4,
    /// Point lights collected from the ECS 世界 上 to `LIGHT_MAX`).
    ///
    /// 超预算时按重要性挑（见 [`lights::local_light_importance`]），不再是文档顺序。
    pub lights: Vec<GpuLight>,
    /// 场景里启用的局部光（点光 + 聚光）总数，截断**之前**的数量。
    pub local_lights_total: usize,
    /// 实际写进前向 SSBO 的局部光数量（≤ `LIGHT_MAX`）。
    pub local_lights_kept: usize,
    /// 被 `LIGHT_MAX` 挤掉的局部光实体（`.scene.json` 的 id；运行时实体没有 id，
    /// 只计数不列名）。编辑器用它在检查器里标出「这盏灯没有画」。
    pub dropped_light_ids: Vec<u32>,
    /// Analytic lights for path tracing (directional + point + spot).
    pub pt_lights: Vec<PtAnalyticLight>,
    /// Exposure multiplier from the 相机 实体
    pub exposure: f32,
    /// Whether a usable 相机 实体 was 找到 in the ECS 世界
    pub has_camera: bool,
    /// 活动相机是否用正交投影（`Camera::orthographic`）。PT 主光线需要它来
    /// 决定走平行光还是透视锥（issue #10）。
    pub camera_orthographic: bool,
    /// 清屏颜色 = 活动相机的 `background_color`（无相机时为引擎默认灰）。
    pub clear_color: [f32; 4],
    /// 清屏方式 = 活动相机的 `Camera.clear_flags`（无相机时为 `Skybox`）。
    /// 只有 `Skybox` 会绘制天空盒背景，见 [`CameraClearFlags::draws_skybox`]。
    pub clear_flags: CameraClearFlags,
    /// 主方向光是否投阴影 = `Light.shadows != None`。
    ///
    /// 为 `false` 时整帧跳过阴影贴图渲染（`ShadowMode::None`），
    /// 着色器也会因为 `shadow_strength = 0` 得到全亮结果。
    pub shadows_enabled: bool,
    /// 阴影强度 = `Light.shadowStrength`（0..=1，已 clamp）。
    pub shadow_strength: f32,
}

// ---------------------------------------------------------------------------
// Extract 帧 packet (sim phase)
// ---------------------------------------------------------------------------

/// Run the extract phase: scene changes + draw-item 列表
/// producing a [`FramePacket`] for later consumption by [`render_system`].
///
/// **This is the API the sim (game‑logic) 线程 calls.** [`render_system`]
/// consumes the packet without accessing the ECS 世界
///
/// Transforms are **not** propagated here: [`crate::engine::Engine::update`]
/// does it at the end of the simulation step, so the extract phase is read-only
/// with respect to `LocalToWorld` and gameplay never sees stale matrices.
pub fn extract_frame_packet(
    world: &mut World,
    display_aspect: f32,
    surface_rotation: &Mat4,
) -> FramePacket {
    extract_frame_packet_with_shadow(
        world,
        display_aspect,
        surface_rotation,
        DEFAULT_SHADOW_DISTANCE,
    )
}

/// [`extract_frame_packet`]，阴影覆盖距离由调用方给定（`RenderSettings::shadow_distance`）。
pub fn extract_frame_packet_with_shadow(
    world: &mut World,
    display_aspect: f32,
    surface_rotation: &Mat4,
    shadow_distance: f32,
) -> FramePacket {
    let mut packet = FramePacket::default();
    extract_frame_packet_into_with_shadow(
        world,
        display_aspect,
        surface_rotation,
        shadow_distance,
        &mut packet,
    );
    packet
}

/// [`extract_frame_packet`] into a recycled packet.
///
/// Every collection in the packet is cleared and refilled, so a caller that
/// keeps one packet alive across frames — `prism-app` recycles it through the
/// render thread — reuses the allocations instead of building new `Vec`s every
/// frame (#66).
pub fn extract_frame_packet_into(
    world: &mut World,
    display_aspect: f32,
    surface_rotation: &Mat4,
    packet: &mut FramePacket,
) {
    extract_frame_packet_into_with_shadow(
        world,
        display_aspect,
        surface_rotation,
        DEFAULT_SHADOW_DISTANCE,
        packet,
    );
}

/// [`extract_frame_packet_into`] 加阴影覆盖距离（米，见 `RenderSettings::shadow_distance`）。
///
/// 阴影正交盒子的大小与中心都跟着它走，所以宿主每帧把它传进来，而不是让引擎
/// 猜一个常量（#526）。
pub fn extract_frame_packet_into_with_shadow(
    world: &mut World,
    display_aspect: f32,
    surface_rotation: &Mat4,
    shadow_distance: f32,
    packet: &mut FramePacket,
) {
    // 1. Collect per-frame scene 状态 (needs orientation).
    collect_scene_changes_into(
        world,
        display_aspect,
        surface_rotation,
        shadow_distance,
        &mut packet.scene,
    );

    // 2. Build the flat draw list and ECS-authored render helpers.
    let scene_view = scene::SceneReadView::new(world);
    // 视锥剔除（#491）：main-pass 只提交主相机视锥内的实体。退化矩阵
    // （`Frustum::from_view_proj` 返回 `None`）时自动退回「不剔除」。
    let frustum =
        prism_render::culling::Frustum::from_view_proj(&packet.scene.view_proj.to_cols_array_2d());
    // 引擎的 `renderQueue` 顺序：不透明段按队列升序，透明段按距离从远到近。
    // 排序必须在提交给渲染器之前完成（`ForwardPass` 只按列表顺序绘制）。
    scene_view.draw_items_with_frustum_into(frustum.as_ref(), &mut packet.draw_items);
    prism_render::sort_draw_items(&mut packet.draw_items, packet.scene.eye.to_array());
    // 光源视锥剔除（#526）：阴影盒子外的投射者不必进 shadow pass。必须放在
    // 主相机剔除之后——相机外的对象既不画主通道，也不再浪费阴影贴图。
    let light_view_proj = packet.scene.light_view_proj.to_cols_array_2d();
    scene::systems::render::cull_shadow_casters(&mut packet.draw_items, &light_view_proj);
    scene_view.gizmo_instances_into(&mut packet.gizmos);
    // 只有 `clearFlags == Skybox` 时才绘制天空盒：其余清屏方式（纯色 /
    // 只清深度 / 不清）即使场景里有启用的 `Skybox` 实体也不画。`ForwardPass`
    // 对 `None` 直接跳过天空盒绘制，所以这里就是它的开关。
    packet.skybox = if packet.scene.clear_flags.draws_skybox() {
        scene_view.skybox_settings()
    } else {
        None
    };
    // 2D 也走同一份主相机视锥：瓦片地图逐格裁剪（#529）。
    scene_view.sprite_items_with_frustum_into(frustum.as_ref(), &mut packet.sprite_items);
    // 2D 光照（#611）：只有 `receive_lights` 的精灵会用到，没灯光时为空。
    scene_view.sprite_lights_into(&mut packet.sprite_lights);
    // 3D 粒子：世界空间 billboard，按到眼睛的距离远到近排序（透明混合）。
    scene_view.particle_items_into(packet.scene.eye, &mut packet.particle_items);

    // 3. 构建 the UI 叠加 from the ECS 世界 (includes UiQuad/TextCmd).
    crate::ui::convert_ui_draw_list_to_overlay_into(world, &mut packet.ui_overlay);

    // 渲染设置由应用层（`App`）在发送前覆盖；引擎默认值保证独立使用
    // `extract_frame_packet` 时仍然可用。
    packet.settings = RenderSettings::default();
}

// ---------------------------------------------------------------------------
// FramePacket
// ---------------------------------------------------------------------------

/// Per-tick extract 结果 camera/light 快照 + 绘制 items + UI 叠加
/// Produced by [`extract_frame_packet`], consumed by [`render_system`].
///
/// `Default` exists so a caller can hold one packet and refill it with
/// [`extract_frame_packet_into`] (see #66).
#[derive(Default)]
pub struct FramePacket {
    pub scene: SceneChanges,
    pub draw_items: Vec<DrawItem>,
    /// ECS `Gizmo` entities, already transformed into world space.
    pub gizmos: Vec<GizmoInstance>,
    /// ECS `Skybox` parameters for this frame (`None` = do not draw).
    pub skybox: Option<SkyboxSettings>,
    /// ECS `SpriteRenderer` entities resolved into flat 2D draw records
    /// (already sorted back-to-front).
    pub sprite_items: Vec<prism_render::SpriteDrawItem>,
    /// `Light2D` entities for the 2D sprite pass (#611).
    pub sprite_lights: Vec<prism_render::SpriteLight>,
    /// `ParticleSystem` emitters resolved into world-space billboards
    /// (already sorted back-to-front).
    pub particle_items: Vec<prism_render::ParticleDrawItem>,
    pub ui_overlay: UiOverlayInput,
    /// 应用层渲染设置（调试标志 / 色调映射 / PT 参数）。主线程每帧写入，
    /// 渲染线程只读——保证 `App::render_settings` 的修改（快捷键、项目配置）
    /// 能真正到达渲染线程。
    pub settings: RenderSettings,
}

// ---------------------------------------------------------------------------
// Scene 集合
// ---------------------------------------------------------------------------

/// 读取 the ECS 世界 and the orientation parameters, then produce
/// a [`SceneChanges`] 快照 for the 当前 帧
/// 采集一帧的 scene 快照（不带复用的便捷包装，测试用）。
#[cfg(test)]
#[allow(dead_code)] // 仅测试用：生产路径直接走 `extract_frame_packet_*`。
fn collect_scene_changes(
    world: &mut World,
    display_aspect: f32,
    surface_rotation: &Mat4,
) -> SceneChanges {
    collect_scene_changes_with_shadow(
        world,
        display_aspect,
        surface_rotation,
        DEFAULT_SHADOW_DISTANCE,
    )
}

/// 阴影覆盖距离（米）参与的场景采集。`extract_frame_packet*` 的默认入口用
/// [`DEFAULT_SHADOW_DISTANCE`]，宿主可以传 `RenderSettings::shadow_distance`。
#[allow(dead_code)] // 同上：只有测试从这一层进去（阴影距离由测试显式给）。
fn collect_scene_changes_with_shadow(
    world: &mut World,
    display_aspect: f32,
    surface_rotation: &Mat4,
    shadow_distance: f32,
) -> SceneChanges {
    let mut out = SceneChanges::default();
    collect_scene_changes_into(
        world,
        display_aspect,
        surface_rotation,
        shadow_distance,
        &mut out,
    );
    out
}

/// 采集进调用方持有的快照。
///
/// `out` is the previous frame's snapshot (the recycled frame packet), so the
/// light buffers keep their allocation instead of being re-collected (#66).
fn collect_scene_changes_into(
    world: &mut World,
    display_aspect: f32,
    surface_rotation: &Mat4,
    shadow_distance: f32,
    out: &mut SceneChanges,
) {
    let fallback_dir = Vec4::new(
        -std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        0.0,
    );
    let fallback_col = Vec4::new(1.0, 1.0, 1.0, 0.0);

    // 1. 相机 — 真正会被渲染的那台（`active_camera`：`depth` 最大的可渲染启用相机）。
    //    取景、清屏、aspect 写回都是同一个实体，不会各挑一次。
    warn_ignored_cameras(world);
    let (view_proj, eye, view, projection, exposure, has_camera, camera_entity) = {
        // 宽高比写到**将被渲染**的相机上（`active_camera` 与下面的取景选择一致）。
        if let Some(cam_entity) = scene::systems::camera::active_camera(world) {
            if let Some(cam) = world.get_mut::<Camera>(cam_entity) {
                cam.aspect = display_aspect;
            }
        }
        match scene::systems::camera::compute_camera_output(world, surface_rotation) {
            Some(out) => (
                out.view_proj,
                out.eye,
                out.view,
                out.projection,
                out.exposure,
                true,
                Some(out.entity),
            ),
            None => {
                log::warn!("no usable Camera entity — using fallback");
                let fb = scene::systems::camera::fallback_camera_output(
                    surface_rotation,
                    display_aspect,
                );
                (
                    fb.view_proj,
                    fb.eye,
                    fb.view,
                    fb.projection,
                    fb.exposure,
                    false,
                    None,
                )
            }
        }
    };

    let inv_projection = projection.inverse();
    let proj22 = projection.col(2).z;
    let proj32 = projection.col(3).z;
    let _ = projection;

    // 1.5 正交标志 —— 与取景/清屏来自**同一台**相机。PT 主光线据此决定
    //     是共享相机原点（透视）还是逐像素近平面起点（正交，issue #10）。
    let camera_orthographic = camera_entity
        .and_then(|entity| world.get::<Camera>(entity))
        .map(|cam| cam.orthographic)
        .unwrap_or(false);
    // 2. 光源 —— 统一 `Light`（形状）与旧三组件归一成 `LightSample`。
    let samples = scene::systems::lights::collect_light_samples(world);
    let dir_light = samples
        .iter()
        .find(|sample| sample.kind == scene::systems::lights::LightKind::Directional)
        .copied();

    let light_direction = dir_light
        .map(|l| directional_light_ubo_direction(l.direction, l.intensity * LUX_TO_RADIANCE_SCALE))
        .unwrap_or(fallback_dir);
    let light_color = dir_light
        .map(|l| Vec4::new(l.color.x, l.color.y, l.color.z, l.ambient))
        .unwrap_or(fallback_col);

    // 2.5 阴影开关：方向光的 `Light.shadows` / `shadowStrength`。
    //     没有方向光时阴影没有意义（fallback 方向只是一个常量），直接关掉。
    let (shadows_enabled, shadow_strength) =
        scene::systems::lights::directional_shadow_state(&samples);

    // 3. 局部光源（点光 + 聚光）进前向 SSBO，超过 LIGHT_MAX 时按重要性挑。
    //    `color.w` 是类型位（0 = 点光，1 = 聚光），聚光额外带朝向与内外锥余弦。
    //
    //    以前是 `.take(LIGHT_MAX)`：谁被保留取决于 ECS 迭代顺序（≈ 文档顺序），
    //    第九盏灯被静默丢掉，删掉前面某盏它又会突然出现（#468）。现在按
    //    「强度 × 范围² / 相机距离²」排序，近处/亮/范围大的优先。
    let mut locals: Vec<(usize, f32)> = samples
        .iter()
        .enumerate()
        .filter(|(_, sample)| sample.kind != scene::systems::lights::LightKind::Directional)
        .map(|(index, sample)| {
            (
                index,
                scene::systems::lights::local_light_importance(sample, eye),
            )
        })
        .collect();
    out.local_lights_total = locals.len();
    // 重要性降序；同分时按文档顺序，保证多帧之间稳定。
    locals.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    locals.truncate(prism_render::LIGHT_MAX as usize);
    // 回到文档顺序写 SSBO：内容一致，只是便于调试时逐盏对照层级面板。
    locals.sort_unstable_by_key(|(index, _)| *index);
    out.local_lights_kept = locals.len();
    out.dropped_light_ids.clear();

    out.lights.clear();
    for (index, _) in &locals {
        let sample = &samples[*index];
        let radiance = sample.color * sample.intensity * LUX_TO_RADIANCE_SCALE;
        let is_spot = sample.kind == scene::systems::lights::LightKind::Spot;
        out.lights.push(GpuLight {
            position: [
                sample.position.x,
                sample.position.y,
                sample.position.z,
                sample.range,
            ],
            color: [
                radiance.x,
                radiance.y,
                radiance.z,
                if is_spot {
                    GPU_LIGHT_KIND_SPOT
                } else {
                    GPU_LIGHT_KIND_POINT
                },
            ],
            direction: [
                sample.direction.x,
                sample.direction.y,
                sample.direction.z,
                sample.cos_outer,
            ],
            params: [sample.cos_inner, 0.0, 0.0, 0.0],
        });
    }

    let dropped = out.local_lights_total - out.local_lights_kept;
    if dropped > 0 {
        let kept: std::collections::HashSet<usize> =
            locals.iter().map(|(index, _)| *index).collect();
        for (index, sample) in samples.iter().enumerate() {
            if kept.contains(&index)
                || sample.kind == scene::systems::lights::LightKind::Directional
            {
                continue;
            }
            if let Some(id) =
                world.get::<crate::scene::runtime_components::SceneEntityId>(sample.entity)
            {
                out.dropped_light_ids.push(id.0);
            }
        }
        out.dropped_light_ids.sort_unstable();
        out.dropped_light_ids.dedup();
    }
    warn_dropped_lights(out.local_lights_total, dropped);

    // 4. 路径追踪的解析光：只喂点光（PT 着色器还没有聚光锥角分支）。
    out.pt_lights.clear();
    for sample in samples
        .iter()
        .filter(|sample| sample.kind == scene::systems::lights::LightKind::Point)
    {
        if out.pt_lights.len() >= PT_LIGHT_MAX as usize {
            break;
        }
        let radiance = sample.color * sample.intensity * LUX_TO_RADIANCE_SCALE;
        out.pt_lights.push(PtAnalyticLight::point(
            sample.position.into(),
            radiance.into(),
            sample.range,
        ));
    }

    // 5. Light-space view-projection（阴影贴图，#526）。
    //
    // 盒子中心取**相机前方 `distance * 0.5`**，而不是 `eye` 本身：以前一半贴图
    // 浪费在相机背后；现在整块都盖在可见区域上，并在光空间里按 texel 吸附，
    // 相机平移时阴影边缘不再爬行。
    let distance = if shadow_distance.is_finite() && shadow_distance > 0.0 {
        shadow_distance
    } else {
        DEFAULT_SHADOW_DISTANCE
    };
    let forward = -Vec3::new(view.x_axis.z, view.y_axis.z, view.z_axis.z).normalize_or_zero();
    let shadow_center = shadow_box_center(eye, forward, distance);
    let light_view_proj = light_view_proj_snapped(
        &light_direction,
        distance * 0.5,
        &shadow_center,
        prism_render::SHADOW_MAP_SIZE,
    );

    // 6. 清屏色 —— 与取景来自**同一台**相机（`Camera.backgroundColor`）。
    let clear_color = camera_entity
        .map(|entity| scene::systems::camera::clear_color(world, entity))
        .unwrap_or(scene::systems::camera::DEFAULT_CLEAR_COLOR);
    // 清屏方式也来自同一台相机（`Camera.clearFlags`）。
    let clear_flags = camera_entity
        .map(|entity| scene::systems::camera::clear_flags(world, entity))
        .unwrap_or_default();

    out.view_proj = view_proj;
    out.eye = eye;
    out.view = view;
    out.projection = projection;
    out.inv_projection = inv_projection;
    out.proj22 = proj22;
    out.proj32 = proj32;
    out.light_direction = light_direction;
    out.light_color = light_color;
    out.light_view_proj = light_view_proj;
    out.exposure = exposure;
    out.has_camera = has_camera;
    out.camera_orthographic = camera_orthographic;
    out.clear_color = clear_color;
    out.clear_flags = clear_flags;
    out.shadows_enabled = shadows_enabled;
    out.shadow_strength = shadow_strength;
}

// ---------------------------------------------------------------------------
// 渲染 系统
// ---------------------------------------------------------------------------

// 清屏色不再由渲染系统写死：它随 `SceneChanges::clear_color` 从活动相机
// （`Camera::background_color`）过来；无相机时用
// `scene::systems::camera::DEFAULT_CLEAR_COLOR`（中性灰）。

/// Consume a pre-extracted [`FramePacket`] and drive the GPU.
///
/// **Each 调用 is stateless with respect to the ECS world.** The simulation
/// 线程 owns [`World`](prism_ecs::World) and produces the packet; this
/// 函数仅读取数据包和渲染器
///
/// Returns `Err` only when [`GraphRenderer::render`] fails.
pub fn render_system(
    renderer: &mut GraphRenderer,
    packet: &FramePacket,
    dirty_router: &mut DirtyRouter,
) -> anyhow::Result<()> {
    // 渲染设置随 packet 一起从主线程过来（见 `FramePacket::settings`）。
    let settings = &packet.settings;
    let FramePacket {
        ref scene,
        ref draw_items,
        ref gizmos,
        skybox,
        ref sprite_items,
        ref sprite_lights,
        ref particle_items,
        ref ui_overlay,
        settings: _,
    } = *packet;

    // 1. Diff against the 上一个 帧 via DirtyRouter.
    let dirty_flags = dirty_router.update(scene);
    if dirty_flags.any() {
        log::trace!(
            "dirty_flags: camera={} dir_light={} point_lights={}",
            dirty_flags.camera,
            dirty_flags.directional_light,
            dirty_flags.point_lights,
        );
    }
    let SceneChanges {
        view_proj,
        eye,
        view,
        projection: _,
        inv_projection,
        proj22,
        proj32,
        light_direction,
        light_color,
        light_view_proj,
        ref lights,
        ref pt_lights,
        exposure,
        has_camera,
        camera_orthographic,
        // 清屏色直接从 `scene` 读（`scene.clear_color`），这里不解构。
        clear_color: _,
        // 清屏方式只影响 extract 阶段的天空盒开关，渲染系统不再需要。
        clear_flags: _,
        shadows_enabled,
        shadow_strength,
        // 光源预算只在编辑器统计里用（`#468`），渲染侧不需要。
        ..
    } = scene;
    let light_count = lights.len() as f32;

    // 2. 构建 the per-frame UBO.
    // FrameUBOData is #[repr(C)] GPU data — 转换 glam → raw arrays at this boundary.
    let frame_data = FrameUBOData {
        view_proj: view_proj.to_cols_array_2d(),
        camera_position: [eye.x, eye.y, eye.z, light_count],
        light_direction: light_direction.to_array(),
        light_color: light_color.to_array(),
        view: view.to_cols_array_2d(),
        light_view_proj: light_view_proj.to_cols_array_2d(),
        tonemap_mode: settings.tonemap_mode,
        viewport_size: {
            let e = renderer.extent();
            [e.width as f32, e.height as f32]
        },
        exposure: *exposure,
        shadow_strength: *shadow_strength,
        _pad2: [0.0; 3],
    };

    // 2.5. 扩展脏检测：draw_items 内容变化（矩阵 / mesh / material / 阴影标志）
    //      也视为场景脏（触发 BLAS/instance 更新与 PT 累积重置），不只比较数量。
    let draw_items_dirty = dirty_router.draw_items_dirty(draw_items_signature(draw_items));
    if draw_items_dirty {
        log::trace!("dirty: draw_items changed");
    }
    let pt_bounces_dirty = dirty_router.pt_bounces_dirty(settings.pt_max_bounces);

    // 3. Drive the render-graph phase API.
    let ctx = match renderer.begin_frame()? {
        Some(c) => c,
        None => return Ok(()),
    };
    let input = prism_render::FrameInput {
        draw_items,
        gizmos,
        sprite_items,
        sprite_lights,
        particle_items,
        // 蒙皮调色板（#560 P0-3）：引擎侧在 P0-3b 里把 `BonePalette` 打进
        // `FramePacket`，这里先按“没有蒙皮绘制项”填充。
        bone_matrices: &[],
        skybox,
        frame_data: &frame_data,
        light_view_proj: light_view_proj.to_cols_array_2d(),
        inv_projection: inv_projection.to_cols_array_2d(),
        debug_flags: settings.debug_flags,
        wireframe: settings.wireframe,
        tonemap_mode: settings.tonemap_mode,
        aa_mode: settings.aa_mode,
        debug_rt: settings.debug_rt,
        grade: prism_render::PostGrade {
            contrast: settings.contrast,
            saturation: settings.saturation,
            vignette: settings.vignette,
        },
        proj22: *proj22,
        proj32: *proj32,
        lights,
        render_mode: settings.render_mode,
        pt_max_bounces: settings.pt_max_bounces,
        pt_ray_max_distance: settings.pt_ray_max_distance,
        pt_max_iterations: settings.pt_max_iterations,
        exposure: *exposure,
        pt_lights,
        pt_accum_dirty: dirty_flags.directional_light
            || dirty_flags.point_lights
            || draw_items_dirty
            || pt_bounces_dirty,
        has_camera: *has_camera,
        camera_orthographic: *camera_orthographic,
        clear_color: scene.clear_color,
        shadows_enabled: *shadows_enabled,
        ui_overlay: Some(ui_overlay),
        show_grid: settings.show_grid,
    };
    // §8 五阶段：prepare 阶段批量同步（当前实现 dirty 日志 + 未来上传点）
    renderer.prepare(&ctx, &input).map_err(|e| {
        let _ = renderer.present(&ctx);
        e
    })?;
    renderer.execute(&ctx, &input).map_err(|e| {
        let _ = renderer.present(&ctx);
        e
    })?;
    let _ = renderer.present(&ctx)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Euler angle ↔ direction helpers
// ---------------------------------------------------------------------------

/// 转换 XYZ Euler angles 角度 to a unit direction 向量 (direction
/// TO the 光源 in 世界 空间
pub fn euler_xyz_deg_to_dir(e: Vec3) -> Vec3 {
    let p = e.x.to_radians();
    let y = e.y.to_radians();
    let r = e.z.to_radians();
    let (sp, cp) = p.sin_cos();
    let (sy, cy) = y.sin_cos();
    let _ = r;
    Vec3::new(cp * sy, sp, cp * cy).normalize_or_zero()
}

/// Inverse of [`euler_xyz_deg_to_dir`]: derive XYZ Euler angles 角度 from
/// a direction 向量
pub fn dir_to_euler_xyz_deg(d: Vec3) -> Vec3 {
    let n = d.normalize_or_zero();
    Vec3::new(n.y.asin().to_degrees(), n.x.atan2(n.z).to_degrees(), 0.0)
}

// ---------------------------------------------------------------------------
// 矩阵 helpers
// ---------------------------------------------------------------------------

/// 方向光从「光传播方向」（`transform.forward`）转换为
/// `FrameUBO.lightDirection` 的「指向光源」约定（见 `common.slang`）。
///
/// `LightSample.direction` 与光学的 `transform.forward` 一致；着色器和阴影相机都要求
/// 指向光源的方向，所以方向光在写入 UBO 时取反。点光 / 聚光的 `direction` 是锥轴传播
/// 方向，不走这条通路。
fn directional_light_ubo_direction(direction: Vec3, radiance: f32) -> Vec4 {
    Vec4::new(-direction.x, -direction.y, -direction.z, radiance)
}

/// 构建 an 正交 light-space view-projection 矩阵
/// 光空间正交盒子，并把盒心**吸附到阴影贴图的 texel 网格**上（#526）。
///
/// `texels` 是阴影贴图的边长（`0`/`1` 表示不吸附）：盒心在光空间里 round 到
/// `2 * half / texels` 的整数倍，相机连续平移时贴图就不会整块重采样，
/// 阴影边缘不再爬行。
/// 阴影正交盒子的中心：相机前方 `distance * 0.5` 处（#526）。
///
/// 以前直接把相机位置当中心，导致一半贴图落在相机背后；退化输入（距离非正 /
/// 朝向为零）退回相机位置，避免把光矩阵算歪。
fn shadow_box_center(eye: Vec3, forward: Vec3, distance: f32) -> Vec3 {
    if !distance.is_finite() || distance <= 0.0 || forward == Vec3::ZERO {
        return eye;
    }
    eye + forward * (distance * 0.5)
}

fn light_view_proj_snapped(light_dir: &Vec4, half: f32, center: &Vec3, texels: u32) -> Mat4 {
    let l = Vec3::new(light_dir.x, light_dir.y, light_dir.z).normalize_or_zero();
    let dist = half * 2.0;
    let up = if l.y.abs() > 0.99 { Vec3::Z } else { Vec3::Y };
    // 光的方向与盒子基向量跟盒心无关，先用未吸附的中心算出来。
    let fwd = (-l).normalize_or_zero();
    let right = fwd.cross(up).normalize_or_zero();
    let true_up = right.cross(fwd);

    // texel 吸附：只量化盒子平面（right / true_up）上的位置；深度轴保持连续，
    // 否则遮挡体到光源的距离也会被量化。
    let center = if texels > 1 && half.is_finite() && half > 0.0 {
        let texel = (2.0 * half) / texels as f32;
        let snap = |value: f32| (value / texel).round() * texel;
        let local = Vec3::new(
            snap(right.dot(*center)),
            snap(true_up.dot(*center)),
            fwd.dot(*center),
        );
        right * local.x + true_up * local.y + fwd * local.z
    } else {
        *center
    };
    let eye = center + l * dist;

    let view = Mat4::from_cols(
        glam::vec4(right.x, true_up.x, -fwd.x, 0.0),
        glam::vec4(right.y, true_up.y, -fwd.y, 0.0),
        glam::vec4(right.z, true_up.z, -fwd.z, 0.0),
        glam::vec4(-right.dot(eye), -true_up.dot(eye), fwd.dot(eye), 1.0),
    );
    let projection = Mat4::from_cols(
        glam::vec4(half.recip(), 0.0, 0.0, 0.0),
        glam::vec4(0.0, half.recip(), 0.0, 0.0),
        glam::vec4(0.0, 0.0, -1.0 / (2.5 * dist), 0.0),
        glam::vec4(0.0, 0.0, -0.5 * dist / (2.5 * dist), 1.0),
    );
    // clip = projection * view * world（列向量约定）。之前误写成 view * projection，
    // 阴影深度会变成 -60 左右、永远落在 [0,1] 之外；culling 也就无从谈起（#526）。
    projection * view
}

#[allow(dead_code)]
fn norm3(a: [f32; 3]) -> [f32; 3] {
    let l = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).max(1e-8).sqrt();
    [a[0] / l, a[1] / l, a[2] / l]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::components::LocalToWorld;

    /// 阴影盒子以相机**前方**为中心，不是以相机位置为中心（#526）。
    #[test]
    fn the_shadow_box_is_centred_in_front_of_the_camera() {
        let eye = Vec3::new(0.0, 10.0, 0.0);
        let forward = Vec3::new(0.0, 0.0, -1.0);
        let center = shadow_box_center(eye, forward, 60.0);
        assert!((center - Vec3::new(0.0, 10.0, -30.0)).length() < 1e-5);
        // 退化输入：距离非法 / 朝向为零时退回相机位置。
        assert_eq!(shadow_box_center(eye, forward, 0.0), eye);
        assert_eq!(shadow_box_center(eye, Vec3::ZERO, 60.0), eye);
        assert_eq!(shadow_box_center(eye, forward, f32::NAN), eye);
    }

    /// 相机平移一点点时，盒心被吸附到同一个 texel：同一个世界点会落在同一个
    /// 贴图位置，阴影边缘不会爬行（深度轴允许有轻微的亚 texel 差异）。
    #[test]
    fn texel_snapping_freezes_the_shadow_matrix_between_texels() {
        let dir = Vec4::new(-0.4, -1.0, -0.3, 0.0);
        let project = |center: Vec3, probe: Vec3| {
            let m = light_view_proj_snapped(&dir, 30.0, &center, 2048);
            let clip = m * probe.extend(1.0);
            (clip.x, clip.y)
        };
        let probe = Vec3::new(3.0, 1.5, -25.0);
        // 基位置特意不在 texel 边界上（否则 round() 的落点本来就会翻）。
        let base = Vec3::new(0.137, -0.211, -30.0);
        let (ax, ay) = project(base, probe);
        // 平面内位移远小于一个 texel（60 / 2048 ≈ 0.029 世界单位）。
        let (bx, by) = project(base + Vec3::new(0.0005, 0.0, 0.0), probe);
        assert!(
            (ax - bx).abs() < 1e-6 && (ay - by).abs() < 1e-6,
            "sub-texel camera motion must not move the shadow map: {ax},{ay} vs {bx},{by}"
        );
        // 挪出好几个 texel 就该真的动。
        let (cx, cy) = project(base + Vec3::new(0.5, 0.0, 0.0), probe);
        assert!(
            (ax - cx).abs() > 1e-3 || (ay - cy).abs() > 1e-3,
            "moving several texels must move the projection"
        );
    }

    /// 阴影投影必须是 `projection * view`：盒子内的点要落在 Vulkan 的 [0,1]
    /// 深度范围里，否则 shadow pass 什么都写不进去，视锥剔除也无从谈起（#526）。
    #[test]
    fn the_shadow_projection_stays_inside_vulkan_depth_range() {
        let dir = Vec4::new(-0.4, -1.0, -0.3, 0.0);
        let light = light_view_proj_snapped(&dir, 30.0, &Vec3::ZERO, 2048);
        for point in [
            Vec3::ZERO,
            Vec3::new(10.0, 10.0, 10.0),
            Vec3::new(-10.0, -10.0, -10.0),
        ] {
            let clip = light * point.extend(1.0);
            assert!(
                clip.z >= 0.0 && clip.z <= clip.w,
                "{point:?} maps to depth {} outside [0,1]",
                clip.z / clip.w
            );
            assert!(clip.x.abs() <= clip.w, "{point:?} maps outside x");
            assert!(clip.y.abs() <= clip.w, "{point:?} maps outside y");
        }
        let far = light * Vec3::new(500.0, 0.0, 0.0).extend(1.0);
        assert!(far.x.abs() > far.w, "500 units away must leave the box");
    }

    /// 阴影距离越大，正交盒子覆盖的世界范围越大。
    #[test]
    fn a_longer_shadow_distance_covers_more_world() {
        let dir = Vec4::new(0.0, -1.0, 0.0, 0.0);
        let near = light_view_proj_snapped(&dir, 30.0, &Vec3::ZERO, 2048);
        let far = light_view_proj_snapped(&dir, 60.0, &Vec3::ZERO, 2048);
        // 同一个世界点：近盒里在盒子外，远盒里在盒子内。
        let probe = Vec3::new(45.0, 0.0, 0.0).extend(1.0);
        let in_near = near * probe;
        let in_far = far * probe;
        assert!(
            in_near.x.abs() > 1.0,
            "half=30 covers 60 units: {in_near:?}"
        );
        assert!(
            in_far.x.abs() <= 1.0,
            "half=60 covers 120 units: {in_far:?}"
        );
    }

    fn shadow_caster(bounds: Option<prism_render::culling::Aabb>) -> DrawItem {
        DrawItem {
            mesh: Default::default(),
            entity_id: u32::MAX,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            material: None,
            cast_shadow: true,
            visible_in_main_pass: true,
            cast_shadow_two_sided: false,
            bone_offset: None,
            state: Default::default(),
            world_bounds: bounds,
            index_start: 0,
            index_count: None,
        }
    }

    /// 光源视锥外的投射者不再写阴影贴图；没有 AABB 的实体保守提交（#526）。
    #[test]
    fn shadow_casters_outside_the_light_box_are_skipped() {
        use prism_render::culling::Aabb;

        let dir = Vec4::new(0.0, -1.0, 0.0, 0.0);
        let light = light_view_proj_snapped(&dir, 30.0, &Vec3::ZERO, 2048);
        let mut items = vec![
            shadow_caster(Some(Aabb::new([-1.0; 3], [1.0; 3]))),
            shadow_caster(Some(Aabb::new([499.0, -1.0, -1.0], [501.0, 1.0, 1.0]))),
            shadow_caster(None),
        ];
        scene::systems::render::cull_shadow_casters(&mut items, &light.to_cols_array_2d());
        assert!(
            items[0].cast_shadow,
            "inside the shadow box must still cast"
        );
        assert!(!items[1].cast_shadow, "far outside the box must be skipped");
        assert!(
            items[2].cast_shadow,
            "unknown bounds must stay conservative"
        );
    }

    /// 退化矩阵（没有可用视锥）时一个都不裁，避免把整场阴影删掉。
    #[test]
    fn an_invalid_light_frustum_disables_shadow_culling() {
        use prism_render::culling::Aabb;

        let mut items = vec![shadow_caster(Some(Aabb::new(
            [499.0, -1.0, -1.0],
            [501.0, 1.0, 1.0],
        )))];
        scene::systems::render::cull_shadow_casters(&mut items, &Mat4::ZERO.to_cols_array_2d());
        assert!(items[0].cast_shadow);
    }

    /// `LightSample.direction` 是光传播方向，UBO 的 `lightDirection` 要「指向光源」。
    #[test]
    fn directional_ubo_direction_points_to_the_light() {
        let ubo = directional_light_ubo_direction(Vec3::Z, 2.0);
        assert!((ubo.truncate() - Vec3::NEG_Z).length() < 1e-6);
        assert_eq!(ubo.w, 2.0);
    }

    /// PR #38 review finding 1：视口 aspect 必须写到**实际渲染**的相机上。
    /// 主相机（`depth` 更大）缺 `LocalToWorld` 时渲染的是低 `depth` 那台。
    #[test]
    fn viewport_aspect_lands_on_the_rendered_camera() {
        let mut world = World::new();

        let rendered = world.spawn();
        world.insert(
            rendered,
            Camera {
                depth: 0.0,
                ..Camera::default()
            },
        );
        world.insert(rendered, LocalToWorld(Mat4::IDENTITY));

        let transform_less = world.spawn();
        world.insert(
            transform_less,
            Camera {
                depth: 5.0,
                ..Camera::default()
            },
        );

        let changes = collect_scene_changes(&mut world, 2.5, &Mat4::IDENTITY);

        assert!(changes.has_camera);
        let rendered_aspect = world
            .get::<Camera>(rendered)
            .expect("rendered camera")
            .aspect;
        assert!(
            (rendered_aspect - 2.5).abs() < 1e-6,
            "渲染用的相机应拿到视口 aspect，实际 {rendered_aspect}"
        );
        let untouched = world
            .get::<Camera>(transform_less)
            .expect("transform-less camera")
            .aspect;
        assert!(
            (untouched - Camera::default().aspect).abs() < 1e-6,
            "不参与渲染的相机不应被改写 aspect，实际 {untouched}"
        );
    }

    /// issue #10：PT 主光线要区分透视/正交，所以「正交标志」也必须来自
    /// **实际渲染**的那台相机，而不是随便挑一台。
    #[test]
    fn orthographic_flag_comes_from_the_rendered_camera() {
        let mut world = World::new();

        let rendered = world.spawn();
        world.insert(
            rendered,
            Camera {
                depth: 0.0,
                orthographic: true,
                ..Camera::default()
            },
        );
        world.insert(rendered, LocalToWorld(Mat4::IDENTITY));

        let transform_less = world.spawn();
        world.insert(
            transform_less,
            Camera {
                depth: 5.0,
                orthographic: false,
                ..Camera::default()
            },
        );

        let changes = collect_scene_changes(&mut world, 16.0 / 9.0, &Mat4::IDENTITY);
        assert!(changes.has_camera);
        assert!(
            changes.camera_orthographic,
            "正交标志应跟随实际渲染的相机（有 LocalToWorld 的那台）"
        );

        // 没有可渲染相机时退化到 fallback 透视相机。
        let mut empty = World::new();
        let fallback = collect_scene_changes(&mut empty, 16.0 / 9.0, &Mat4::IDENTITY);
        assert!(!fallback.has_camera);
        assert!(!fallback.camera_orthographic);
    }

    /// #468：局部光超过 `LIGHT_MAX` 时按重要性挑，而不是文档顺序。
    ///
    /// 场景里 5 盏近处亮灯 + 5 盏远处暗灯：留下的必须是 5 盏亮的再加 3 盏暗的，
    /// 被丢的只有 2 盏最靠后的暗灯（同分时按文档顺序）。
    #[test]
    fn only_the_most_important_local_lights_reach_the_ssbo() {
        use crate::scene::components::{LocalTransform, PointLight};
        use crate::scene::runtime_components::SceneEntityId;

        let mut world = World::new();
        let camera = world.spawn();
        world.insert(camera, Camera::default());
        world.insert(camera, LocalToWorld(Mat4::IDENTITY));

        for id in 1..=5u32 {
            let light = world.spawn();
            world.insert(
                light,
                PointLight {
                    intensity: 100.0,
                    range: 10.0,
                    ..Default::default()
                },
            );
            world.insert(
                light,
                LocalTransform {
                    position: Vec3::new(0.0, 0.0, -1.0),
                    ..Default::default()
                },
            );
            world.insert(light, SceneEntityId(id));
        }
        for id in 6..=10u32 {
            let light = world.spawn();
            world.insert(
                light,
                PointLight {
                    intensity: 1.0,
                    range: 1.0,
                    ..Default::default()
                },
            );
            world.insert(
                light,
                LocalTransform {
                    position: Vec3::new(0.0, 0.0, -1000.0),
                    ..Default::default()
                },
            );
            world.insert(light, SceneEntityId(id));
        }

        let changes = collect_scene_changes(&mut world, 16.0 / 9.0, &Mat4::IDENTITY);

        assert_eq!(changes.local_lights_total, 10, "十盏灯都该被统计到");
        assert_eq!(
            changes.local_lights_kept,
            prism_render::LIGHT_MAX as usize,
            "SSBO 容量不变"
        );
        assert_eq!(changes.lights.len(), prism_render::LIGHT_MAX as usize);
        assert_eq!(
            changes.dropped_light_ids,
            vec![9, 10],
            "被丢的是最远的暗灯，不是文档里最后两盏亮灯"
        );
    }

    /// 统一语义：只有 `clearFlags == Skybox` 才会绘制天空盒背景。
    ///
    /// 场景里放一个启用的 `Skybox` 实体，逐一切换相机的清屏方式；只有 `Skybox`
    /// 时 `FramePacket.skybox` 才是 `Some`，其余都应是 `None`（`ForwardPass`
    /// 对 `None` 直接跳过天空盒）。
    #[test]
    fn skybox_is_only_drawn_for_skybox_clear_flags() {
        use crate::scene::components::Skybox;

        for (flags, draws) in [
            (CameraClearFlags::Skybox, true),
            (CameraClearFlags::SolidColor, false),
            (CameraClearFlags::Depth, false),
            (CameraClearFlags::Nothing, false),
        ] {
            let mut world = World::new();

            let camera = world.spawn();
            world.insert(
                camera,
                Camera {
                    clear_flags: flags,
                    ..Camera::default()
                },
            );
            world.insert(camera, LocalToWorld(Mat4::IDENTITY));

            let sky = world.spawn();
            world.insert(
                sky,
                Skybox {
                    hdr_path: "env.hdr".to_string(),
                    enabled: true,
                    ..Skybox::default()
                },
            );

            let packet = extract_frame_packet(&mut world, 1.0, &Mat4::IDENTITY);
            assert_eq!(packet.scene.clear_flags, flags);
            assert_eq!(
                packet.skybox.is_some(),
                draws,
                "clear_flags = {flags:?} 时是否绘制天空盒应为 {draws}"
            );
        }
    }

    /// #66：`extract_frame_packet_into` 必须复用调用方数据包里的分配，
    /// 否则每帧都会重新 `collect()` 一遍。
    #[test]
    fn extract_into_reuses_the_packet_allocations() {
        use crate::scene::components::Gizmo;

        let mut world = World::new();
        let camera = world.spawn();
        world.insert(camera, Camera::default());
        world.insert(camera, LocalToWorld(Mat4::IDENTITY));
        let gizmo = world.spawn();
        world.insert(
            gizmo,
            Gizmo {
                enabled: true,
                ..Gizmo::default()
            },
        );
        world.insert(gizmo, LocalToWorld(Mat4::IDENTITY));

        let capacities = |packet: &FramePacket| {
            [
                packet.draw_items.capacity(),
                packet.gizmos.capacity(),
                packet.sprite_items.capacity(),
                packet.sprite_lights.capacity(),
                packet.scene.lights.capacity(),
                packet.scene.pt_lights.capacity(),
                packet.ui_overlay.quads.capacity(),
                packet.ui_overlay.texts.capacity(),
                packet.ui_overlay.order.capacity(),
            ]
        };

        let mut packet = FramePacket::default();
        extract_frame_packet_into(&mut world, 1.0, &Mat4::IDENTITY, &mut packet);
        assert!(!packet.gizmos.is_empty(), "用例需要至少一个绘制项");
        let first = capacities(&packet);

        extract_frame_packet_into(&mut world, 1.0, &Mat4::IDENTITY, &mut packet);
        let second = capacities(&packet);
        assert_eq!(
            first, second,
            "第二次 extract 应复用同一批 Vec 容量，而不是重新分配"
        );
        assert!(!packet.gizmos.is_empty());
    }
}
