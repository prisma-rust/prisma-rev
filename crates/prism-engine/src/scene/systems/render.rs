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
type SortKey = (i32, i32, f32, u32);

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

/// Collect all 2D line draw items (`LineRenderer`) expanded into quads.
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
    let mut entries = sprite_entries(world);
    entries.extend(line_entries(world));
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
                    uv_rect: sprite_uv_rect(sprite, sref.size),
                    color: sprite.color,
                    texture: sref.texture_slot,
                    src_blend: sprite.src_blend,
                    dst_blend: sprite.dst_blend,
                    blend_op: sprite.blend_op,
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
        let key_base = (line.sorting_layer, line.order_in_layer, z, entity.id());

        for (index, pair) in line.points.windows(2).enumerate() {
            let (p0, p1) = (pair[0], pair[1]);
            let a = glam::Vec2::new(p0[0], p0[1]);
            let b = glam::Vec2::new(p1[0], p1[1]);
            let dir = b - a;
            let length = dir.length();
            if length <= f32::EPSILON {
                continue;
            }
            // Quad: m0 spans the segment, m1 spans the (doubled) half-width.
            let normal = glam::Vec2::new(-dir.y, dir.x) / length * width;
            let mid = (a + b) * 0.5;
            let model = glam::Mat4::from_cols(
                glam::Vec4::new(dir.x, dir.y, 0.0, 0.0),
                glam::Vec4::new(normal.x, normal.y, 0.0, 0.0),
                glam::Vec4::new(0.0, 0.0, 1.0, 0.0),
                glam::Vec4::new(mid.x, mid.y, p0[2], 1.0),
            );
            out.push((
                (key_base.0, key_base.1, z, key_base.3 + index as u32),
                prism_render::SpriteDrawItem {
                    model: model.to_cols_array_2d(),
                    uv_rect: [0.0, 0.0, 1.0, 1.0],
                    color: line.color,
                    texture: SOLID_TEXTURE,
                    // 折线没有作者态混合字段：保持标准 alpha 混合。
                    src_blend: prism_scene::render_enums::BlendMode::SrcAlpha,
                    dst_blend: prism_scene::render_enums::BlendMode::OneMinusSrcAlpha,
                    blend_op: prism_scene::render_enums::BlendOp::Add,
                },
            ));
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
