//! Scene 渲染 系统 — collects [`DrawItem`]s from ECS entities.
//!
//! Iterates entities that carry [`LocalToWorld`] and [`MeshRef`] and are
//! 激活 The 结果 is fed to [`prism_render`]'s [`GraphRenderer`] each 帧

use prism_ecs::World;
use prism_render::{DrawItem, DrawState};

use crate::scene::components::*;

/// 从 `MeshRenderer` 的渲染状态字段构造绘制状态（材质/渲染器状态）。
///
/// 没有该组件的运行时实体按 [`DrawState::default`]（不透明、队列 2000、
/// 背面剔除、写深度）处理。
pub(crate) fn draw_state(renderer: Option<&MeshRenderer>) -> DrawState {
    match renderer {
        Some(renderer) => DrawState {
            cull_mode: renderer.cull_mode,
            render_queue: renderer.render_queue.value(),
            z_write: renderer.z_write,
            src_blend: renderer.src_blend,
            dst_blend: renderer.dst_blend,
            blend_op: renderer.blend_op,
        },
        None => DrawState::default(),
    }
}

/// Collect all 可见 绘制 items from the ECS 世界
///
/// An 实体 produces a `DrawItem` if and only if it has all of:
/// - [`LocalToWorld`]
/// - [`MeshRef`]
/// - [`Active(true)`]
///
/// [`MaterialRef`] is **optional**: a mesh-only renderer (empty
/// `MeshRenderer::material_path`, or an entity spawned by game code without a
/// resolved material) still draws, with [`DrawItem::material`] left as `None`
/// so the renderer falls back to its default material slot.
///
/// Entities without an 激活 分量 默认 to 激活 (the 分量
/// defaults to `true`).
pub fn scene_render_system(world: &World) -> Vec<DrawItem> {
    let mut out = Vec::new();
    scene_render_system_into(world, &mut out);
    out
}

/// [`scene_render_system`] into a caller-owned buffer.
///
/// The frame packet keeps this `Vec` across frames, so `clear()` + `extend`
/// reuses the allocation instead of doing one `collect()` per frame (#66).
pub fn scene_render_system_into(world: &World, out: &mut Vec<DrawItem>) {
    scene_render_system_into_with_frustum(world, None, out);
}

/// 网格局部包围盒表（`MeshHandle → 局部 AABB`），由资产解析写回。
///
/// 放在 World 资源里而不是每个 [`MeshRef`] 上：同一个网格被多个实体复用时只存一份，
/// 热重载换句柄时也只更新一条记录。没有记录的网格**不参与剔除**（数据未知时保持
/// 旧行为），所以程序化网格（标定球、gizmo）仍然照旧绘制。
#[derive(Debug, Default)]
pub struct MeshBounds {
    bounds:
        std::collections::HashMap<prism_render::managers::MeshHandle, prism_render::culling::Aabb>,
}

impl MeshBounds {
    pub fn insert(
        &mut self,
        handle: prism_render::managers::MeshHandle,
        bounds: prism_render::culling::Aabb,
    ) {
        self.bounds.insert(handle, bounds);
    }

    pub fn get(
        &self,
        handle: prism_render::managers::MeshHandle,
    ) -> Option<&prism_render::culling::Aabb> {
        self.bounds.get(&handle)
    }

    pub fn len(&self) -> usize {
        self.bounds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bounds.is_empty()
    }
}

/// [`scene_render_system_into`] with an optional camera frustum (#491).
///
/// `frustum == None` 时与旧行为完全一致（不剔除）。有视锥时，把网格的局部
/// 包围盒按实体模型矩阵变换到世界空间再做保守相交测试；**没有包围盒或变换非
/// 有限**的实体照旧提交（见 [`MeshBounds`]）。
///
/// 注意这只按**主相机**剔除：阴影通道共用同一份绘制列表，因此相机外的投射者
/// 也不会进阴影贴图（与 Unity 的「只为可见物体渲阴影」一致）。按光源视锥剔除
/// 是后续单独的一步。
pub fn scene_render_system_into_with_frustum(
    world: &World,
    frustum: Option<&prism_render::culling::Frustum>,
    out: &mut Vec<DrawItem>,
) {
    out.clear();
    let bounds = world.get_resource::<MeshBounds>();
    out.extend(
        world
            .query2::<LocalToWorld, MeshRef>()
            .filter(|(e, _, _)| crate::scene::systems::hierarchy::is_active_in_hierarchy(world, *e))
            .filter(|(_, wt, mr)| match frustum {
                Some(frustum) => bounds
                    .and_then(|bounds| bounds.get(mr.render_handle))
                    .and_then(|local| local.transformed(&wt.0.to_cols_array_2d()))
                    .map(|world_bounds| frustum.intersects_aabb(&world_bounds))
                    .unwrap_or(true),
                None => true,
            })
            .map(|(entity, wt, mr)| {
                // `MeshRenderer.shadow_casting_mode` / `cull_mode`（
                // `shadowCastingMode` / `CullMode`）。没有该组件的运行时实体按
                // 默认值处理（`On` / `Back`）。
                let renderer = world.get::<MeshRenderer>(entity);
                let mode = renderer
                    .map(|renderer| renderer.shadow_casting_mode)
                    .unwrap_or_default();
                DrawItem {
                    mesh: mr.render_handle,
                    model: wt.0.to_cols_array_2d(),
                    material: world
                        .get::<MaterialRef>(entity)
                        .map(|material| material.material_slot),
                    cast_shadow: mode.casts_shadows(),
                    visible_in_main_pass: mode.visible_in_main_pass(),
                    cast_shadow_two_sided: mode.casts_two_sided_shadows(),
                    state: draw_state(renderer),
                }
            }),
    );
}

/// 2D 绘制项的排序键：`(sorting_layer, order_in_layer, z, entity_id)`。
pub(crate) type SortKey = (i32, i32, f32, u32);

/// 纯色绘制标记：`SpriteDrawItem.texture` 取此值时着色器不采样贴图。
pub const SOLID_TEXTURE: u32 = u32::MAX;

/// Collect all 2D sprite draw items from the ECS world, sorted back-to-front.
///
/// An entity produces a [`SpriteDrawItem`](prism_render::SpriteDrawItem) when it
/// has [`LocalToWorld`], [`SpriteRenderer`] with a texture path, and a resolved
/// [`SpriteRef`] (`generation > 0`).
///
/// Sort key: `sorting_layer` → `order_in_layer` → world `z` → entity id.
/// Painter's algorithm draws far-to-near, so `z` ascending (camera looks −Z and
/// sits at +Z: larger `z` is closer).
pub fn scene_sprite_system(world: &World) -> Vec<prism_render::SpriteDrawItem> {
    sort_draw_items(sprite_entries(world))
}

/// Collect all 2D line draw items (`LineRenderer`) as non-overlapping
/// triangles (miter/bevel joins, butt/square/round caps — #604).
pub fn scene_line_system(world: &World) -> Vec<prism_render::SpriteDrawItem> {
    sort_draw_items(line_entries(world))
}

/// Every 2D draw item (sprites **and** lines) in one back-to-front list.
///
/// This is what the renderer consumes: sprites and lines share one pass, so
/// they must be sorted against each other, not in two independent lists.
pub fn scene_2d_system(world: &World) -> Vec<prism_render::SpriteDrawItem> {
    let mut out = Vec::new();
    scene_2d_system_into(world, &mut out);
    out
}

/// [`scene_2d_system`] into a caller-owned buffer (see
/// [`scene_render_system_into`] for why).
pub fn scene_2d_system_into(world: &World, out: &mut Vec<prism_render::SpriteDrawItem>) {
    scene_2d_system_into_with_frustum(world, None, out);
}

/// [`scene_2d_system_into`] 加主相机视锥：瓦片地图逐格裁剪（#529）。
///
/// 精灵 / 折线 / 粒子不参与剔除（它们本来就只是少量实例）；瓦片格子可能上千，
/// 裁掉屏幕外的行列能显著减少实例缓冲的写入。
pub fn scene_2d_system_into_with_frustum(
    world: &World,
    frustum: Option<&prism_render::culling::Frustum>,
    out: &mut Vec<prism_render::SpriteDrawItem>,
) {
    let mut entries = sprite_entries(world);
    entries.extend(line_entries(world));
    // 2D 粒子是纯色四边形，和精灵共用同一条通道与排序（#594）。
    entries.extend(super::particles2d::particle_entries_2d(world));
    // 瓦片地图：一个实体一整张网格（#529）。
    entries.extend(super::tilemap::tilemap_entries(world, frustum));
    sort_draw_items_into(entries, out);
}

fn sort_draw_items(
    entries: Vec<(SortKey, prism_render::SpriteDrawItem)>,
) -> Vec<prism_render::SpriteDrawItem> {
    let mut out = Vec::new();
    sort_draw_items_into(entries, &mut out);
    out
}

fn sort_draw_items_into(
    mut entries: Vec<(SortKey, prism_render::SpriteDrawItem)>,
    out: &mut Vec<prism_render::SpriteDrawItem>,
) {
    entries.sort_by(|a, b| {
        let (ak, bk) = (&a.0, &b.0);
        ak.0.cmp(&bk.0)
            .then(ak.1.cmp(&bk.1))
            .then(ak.2.partial_cmp(&bk.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(ak.3.cmp(&bk.3))
    });
    out.clear();
    out.extend(entries.into_iter().map(|(_, item)| item));
}

/// 收集场景里的 [`Light2D`]（#611），按贡献从强到弱，最多
/// [`prism_render::SPRITE_LIGHT_MAX`] 盏。
///
/// 只有 `SpriteRenderer.receive_lights` 为真的精灵才会用到这些数据，所以
/// 没有灯光时返回空列表即可——着色器看到 `light_count == 0` 会保持 unlit。
///
/// 3D 光源（`PointLight` / `DirectionalLight` / `SpotLight`）不会照亮 2D 精灵；
/// 只放了 3D 光源时这里 warn 一次，避免「组件合法但完全没效果」被静默吞掉。
pub fn sprite_light_entries(world: &World) -> Vec<prism_render::SpriteLight> {
    use std::sync::Once;
    static WARNED: Once = Once::new();

    let mut lights: Vec<prism_render::SpriteLight> = Vec::new();
    for (entity, transform, light) in world.query2::<LocalToWorld, Light2D>() {
        if !crate::scene::systems::hierarchy::is_active_in_hierarchy(world, entity)
            || !light.contributes()
        {
            continue;
        }
        let origin = transform.0.w_axis;
        let position = [origin.x, origin.y];
        let color = light.color;
        let intensity = light.strength();
        let radius = light.falloff_radius();
        lights.push(match light.kind {
            prism_scene::components::Light2DKind::Ambient => {
                prism_render::SpriteLight::ambient(color, intensity)
            }
            prism_scene::components::Light2DKind::Point => {
                prism_render::SpriteLight::point(position, color, intensity, radius)
            }
            prism_scene::components::Light2DKind::Directional => {
                prism_render::SpriteLight::directional(light.direction, color, intensity)
            }
        });
    }

    if lights.is_empty() {
        let has_3d_light = world
            .query::<prism_scene::components::PointLight>()
            .next()
            .is_some()
            || world
                .query::<prism_scene::components::SpotLight>()
                .next()
                .is_some()
            || world
                .query::<prism_scene::components::DirectionalLight>()
                .next()
                .is_some();
        if has_3d_light {
            WARNED.call_once(|| {
                log::warn!(
                    "2D sprites ignore 3D lights (PointLight / SpotLight / DirectionalLight); \
                     add a `Light2D` entity and enable `SpriteRenderer.receive_lights` (#611)"
                );
            });
        }
    }

    prism_render::trim_sprite_lights(&mut lights);
    lights
}

fn sprite_entries(world: &World) -> Vec<(SortKey, prism_render::SpriteDrawItem)> {
    world
        .query3::<LocalToWorld, SpriteRenderer, SpriteRef>()
        .filter(|(entity, _, sprite, sref)| {
            let active = crate::scene::systems::hierarchy::is_active_in_hierarchy(world, *entity);
            active && sprite.has_sprite() && sref.generation > 0
        })
        .map(|(entity, world_transform, sprite, sref)| {
            let size = sprite.world_size(sref.size);
            // Place the entity origin on the sprite pivot (convention:
            // (0,0) is the bottom-left corner).
            let offset = glam::vec3(
                -(sprite.pivot[0] - 0.5) * size[0],
                -(sprite.pivot[1] - 0.5) * size[1],
                0.0,
            );
            let local = glam::Mat4::from_scale_rotation_translation(
                glam::vec3(size[0], size[1], 1.0),
                glam::Quat::IDENTITY,
                offset,
            );
            let model = world_transform.0 * local;
            let key: SortKey = (
                sprite.sorting_layer,
                sprite.order_in_layer,
                world_transform.0.w_axis.z,
                entity.id(),
            );
            (
                key,
                prism_render::SpriteDrawItem {
                    model: model.to_cols_array_2d(),
                    corners: None,
                    uv_rect: sprite_uv_rect(sprite, sref.size),
                    color: sprite.color,
                    texture: sref.texture_slot,
                    src_blend: sprite.src_blend,
                    dst_blend: sprite.dst_blend,
                    blend_op: sprite.blend_op,
                    clip: prism_render::clip_bounds(sprite.clip_enabled, sprite.clip_rect),
                    receive_lights: sprite.receive_lights,
                },
            )
        })
        .collect()
}

fn line_entries(world: &World) -> Vec<(SortKey, prism_render::SpriteDrawItem)> {
    let mut out = Vec::new();
    for (entity, line) in world.query::<LineRenderer>() {
        let active = crate::scene::systems::hierarchy::is_active_in_hierarchy(world, entity);
        if !active || !line.has_line() {
            continue;
        }
        let width = if line.width.is_finite() && line.width > 0.0 {
            line.width
        } else {
            0.1
        };
        let z = world
            .get::<LocalToWorld>(entity)
            .map(|t| t.0.w_axis.z)
            .unwrap_or(0.0);
        // 同一条折线的所有三角形共用排序键：`sort_by` 是稳定排序，三角形按
        // 生成顺序绘制，而它们互不重叠，所以内部顺序不影响结果（#604）。
        let key: SortKey = (line.sorting_layer, line.order_in_layer, z, entity.id());
        let model = glam::Mat4::IDENTITY.to_cols_array_2d();

        for triangle in tessellate_line(&line.points, width, line.join, line.cap, line.miter_limit)
        {
            out.push((
                key,
                prism_render::SpriteDrawItem {
                    model,
                    corners: Some([triangle[0], triangle[1], triangle[2], triangle[2]]),
                    uv_rect: [0.0, 0.0, 1.0, 1.0],
                    color: line.color,
                    texture: SOLID_TEXTURE,
                    // 折线没有作者态混合字段：保持标准 alpha 混合。
                    src_blend: prism_scene::render_enums::BlendMode::SrcAlpha,
                    dst_blend: prism_scene::render_enums::BlendMode::OneMinusSrcAlpha,
                    blend_op: prism_scene::render_enums::BlendOp::Add,
                    clip: prism_render::NO_CLIP,
                    receive_lights: false,
                },
            ));
        }
    }
    out
}

/// 把一条世界空间折线三角化成**互不重叠**的笔触（#604）。
///
/// 每段是一个四边形（两个三角形）。拐点处：
///
/// - 内侧两段共用同一个顶点（两条内侧偏移线的交点），所以内侧既没有缺口
///   也不会叠加；
/// - `Miter` 外侧也共用斜接点；超过 `miter_limit`（相对半宽）时退化成
///   `Bevel`，外侧各段停在自己的偏移点，再用一个三角形补角。
///
/// 这样半透明折线的每一块像素只被混合一次。`cap` 只影响两个端点：
/// `Butt` / `Square` / `Round`（8 段扇形）。
///
/// 输入里的连续重复点会先被去掉——零长度段没有方向，也画不出面积。
pub(crate) fn tessellate_line(
    points: &[[f32; 3]],
    width: f32,
    join: LineJoin,
    cap: LineCap,
    miter_limit: f32,
) -> Vec<[[f32; 3]; 3]> {
    use glam::Vec2;

    let mut pts: Vec<(Vec2, f32)> = Vec::with_capacity(points.len());
    for point in points {
        let v = Vec2::new(point[0], point[1]);
        if pts
            .last()
            .map_or(true, |(last, _)| last.distance_squared(v) > 1e-10)
        {
            pts.push((v, point[2]));
        }
    }
    if pts.len() < 2 {
        return Vec::new();
    }

    let half = (width * 0.5).max(1e-5);
    let miter_limit = if miter_limit.is_finite() && miter_limit >= 1.0 {
        miter_limit
    } else {
        4.0
    };
    let n = pts.len();
    let dirs: Vec<Vec2> = pts
        .windows(2)
        .map(|pair| (pair[1].0 - pair[0].0).normalize())
        .collect();
    // 左法线，长度 = 半个线宽。
    let normals: Vec<Vec2> = dirs
        .iter()
        .map(|dir| Vec2::new(-dir.y, dir.x) * half)
        .collect();

    // `start[i]` = 第 i 段起点两侧的 (左, 右)；`end[i]` = 第 i 段终点两侧。
    let mut start: Vec<(Vec2, Vec2)> = vec![(Vec2::ZERO, Vec2::ZERO); n];
    let mut end: Vec<(Vec2, Vec2)> = vec![(Vec2::ZERO, Vec2::ZERO); n];
    let mut bevels: Vec<(Vec2, Vec2, Vec2)> = Vec::new();

    // 端点：方头沿线段方向外扩半个线宽，圆头由后面的扇形补上。
    let shift = |dir: Vec2| match cap {
        LineCap::Square => dir * half,
        _ => Vec2::ZERO,
    };
    start[0] = (
        pts[0].0 + normals[0] - shift(dirs[0]),
        pts[0].0 - normals[0] - shift(dirs[0]),
    );
    end[n - 2] = (
        pts[n - 1].0 + normals[n - 2] + shift(dirs[n - 2]),
        pts[n - 1].0 - normals[n - 2] + shift(dirs[n - 2]),
    );

    for i in 1..n - 1 {
        let point = pts[i].0;
        let (n0, n1) = (normals[i - 1], normals[i]);
        let cross = dirs[i - 1].perp_dot(dirs[i]);
        // `n` 是左法线：右转（cross < 0）时外侧在左边。
        let outer_sign = if cross < 0.0 { 1.0 } else { -1.0 };
        let sum = n0.normalize_or_zero() + n1.normalize_or_zero();
        // 折返接近 180° 时两条偏移线几乎平行，斜接点跑到无穷远。
        let cos_half = (sum * 0.5).length();
        let bisector = sum.normalize_or_zero();
        let miter = if cos_half > 1e-3 && bisector != Vec2::ZERO {
            half / cos_half
        } else {
            f32::INFINITY
        };

        if join == LineJoin::Miter && miter <= half * miter_limit {
            let offset = bisector * miter * outer_sign;
            start[i] = (point + offset, point - offset);
            end[i - 1] = start[i];
            continue;
        }

        // 斜切：内侧共用斜接点，外侧各段停在自己的偏移点。
        let inner = point - bisector * miter * outer_sign;
        let outer_prev = point + n0 * outer_sign;
        let outer_next = point + n1 * outer_sign;
        if outer_sign > 0.0 {
            end[i - 1] = (outer_prev, inner);
            start[i] = (outer_next, inner);
        } else {
            end[i - 1] = (inner, outer_prev);
            start[i] = (inner, outer_next);
        }
        bevels.push((point, outer_prev, outer_next));
    }

    let z_at = |index: usize| pts[index].1;
    let mut out: Vec<[[f32; 3]; 3]> = Vec::new();
    let point3 = |v: Vec2, z: f32| [v.x, v.y, z];

    for index in 0..n - 1 {
        let (left0, right0) = start[index];
        let (left1, right1) = end[index];
        let z = z_at(index);
        out.push([point3(left0, z), point3(right0, z), point3(right1, z)]);
        out.push([point3(left0, z), point3(right1, z), point3(left1, z)]);
    }

    for (point, outer_prev, outer_next) in bevels {
        let z = z_at(0);
        out.push([
            point3(point, z),
            point3(outer_prev, z),
            point3(outer_next, z),
        ]);
    }

    if cap == LineCap::Round {
        const CAP_SEGMENTS: usize = 8;
        let rotate = |v: Vec2, angle: f32| {
            let (sin, cos) = angle.sin_cos();
            Vec2::new(v.x * cos - v.y * sin, v.x * sin + v.y * cos)
        };
        // 起点向 -dir 扫、终点向 +dir 扫：+90° 从 n 转到 -dir，-90° 转到 +dir。
        for (index, sign) in [(0usize, 1.0f32), (n - 1, -1.0)] {
            let point = pts[index].0;
            let z = z_at(index);
            let normal = normals[index.min(n - 2)];
            for step in 0..CAP_SEGMENTS {
                let a0 = std::f32::consts::PI * step as f32 / CAP_SEGMENTS as f32;
                let a1 = std::f32::consts::PI * (step + 1) as f32 / CAP_SEGMENTS as f32;
                out.push([
                    point3(point, z),
                    point3(point + rotate(normal, a0 * sign), z),
                    point3(point + rotate(normal, a1 * sign), z),
                ]);
            }
        }
    }

    out
}

/// UV rect `(u0, v0, u1, v1)` of one sprite-sheet frame.
///
/// `v = 0` is the texture's first row (image top), matching `sprites.slang`'s
/// vertex mapping; `flip_*` swaps the corresponding edges so no extra vertex
/// transform is needed.
fn sprite_uv_rect(sprite: &SpriteRenderer, texture_size: [u32; 2]) -> [f32; 4] {
    let tex_w = texture_size[0].max(1) as f32;
    let tex_h = texture_size[1].max(1) as f32;
    let frame = sprite.frame_size_or(texture_size);
    let frame_w = frame[0].max(1) as f32;
    let frame_h = frame[1].max(1) as f32;

    // Sheets are laid out row-major. `frame_index` counts within the selected
    // row first; only an index past the last column continues to the next row.
    let columns = sprite.sheet_columns_or(texture_size);
    let rows = sprite.sheet_rows(texture_size);
    let row = sprite.sheet_row.min(rows - 1);
    let column = sprite.frame_index % columns;
    let row = (row + sprite.frame_index / columns).min(rows - 1);

    let mut u0 = column as f32 * frame_w / tex_w;
    let mut u1 = ((column as f32 + 1.0) * frame_w / tex_w).min(1.0);
    let mut v0 = row as f32 * frame_h / tex_h;
    let mut v1 = ((row as f32 + 1.0) * frame_h / tex_h).min(1.0);

    if sprite.flip_x {
        std::mem::swap(&mut u0, &mut u1);
    }
    if sprite.flip_y {
        std::mem::swap(&mut v0, &mut v1);
    }
    [u0, v0, u1, v1]
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
