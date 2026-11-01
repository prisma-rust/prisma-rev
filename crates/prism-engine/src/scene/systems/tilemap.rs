//! 2D 瓦片地图（`Tilemap` → `SpriteDrawItem`，#529）。
//!
//! 一张网格只占一个实体：这里把**非空**格子展开成四边形，交给 `SpritePass`
//! 的实例缓冲，与精灵 / 折线 / 2D 粒子共用同一条通道与排序键
//! （`sorting_layer → order_in_layer → z → 实体 id`）。
//!
//! 裁剪：提取阶段有主相机视锥时逐格做 AABB 相交测试（只裁行列，不做 LOD / chunk）。
//! 没有视锥（headless、测试、离线工具）时输出全部非空格子。

use glam::{Mat4, Quat, Vec3};
use prism_ecs::World;
use prism_render::culling::{Aabb, Frustum};
use prism_render::SpriteDrawItem;
use prism_scene::components::{LocalToWorld, Tilemap};

use super::render::SortKey;
use crate::scene::runtime_components::TilemapRef;

/// 一条瓦片地图实体的绘制项。
///
/// `tileset.generation == 0`（还没上传完）或网格为空时返回空列表。
pub(crate) fn tilemap_draw_items(
    tilemap: &Tilemap,
    tileset: &TilemapRef,
    transform: &Mat4,
    entity_id: u32,
    frustum: Option<&Frustum>,
) -> Vec<(SortKey, SpriteDrawItem)> {
    if !tilemap.has_tiles() || tileset.generation == 0 {
        return Vec::new();
    }
    let (columns, rows) = tilemap.grid();
    let cells = columns as usize * rows as usize;
    let (cell_w, cell_h) = (tilemap.cell_size[0], tilemap.cell_size[1]);
    let mut out = Vec::new();

    for row in 0..rows {
        for column in 0..columns {
            let index = (row as usize) * (columns as usize) + column as usize;
            if index >= cells || index >= tilemap.tiles.len() {
                break;
            }
            let tile = tilemap.tiles[index];
            if tile == 0 {
                continue;
            }
            let center = tilemap.cell_center(column, row, rows);
            let model = *transform
                * Mat4::from_scale_rotation_translation(
                    Vec3::new(cell_w, cell_h, 1.0),
                    Quat::IDENTITY,
                    Vec3::new(center[0], center[1], 0.0),
                );

            if let Some(frustum) = frustum {
                if !frustum.intersects_aabb(&cell_bounds(&model)) {
                    continue;
                }
            }

            let key = (
                tilemap.sorting_layer,
                tilemap.order_in_layer,
                model.w_axis.z,
                entity_id,
            );
            out.push((
                key,
                SpriteDrawItem {
                    model: model.to_cols_array_2d(),
                    corners: None,
                    uv_rect: tilemap.tile_uv(tile - 1, tileset.size),
                    color: tilemap.color,
                    texture: tileset.texture_slot,
                    src_blend: prism_scene::render_enums::BlendMode::SrcAlpha,
                    dst_blend: prism_scene::render_enums::BlendMode::OneMinusSrcAlpha,
                    blend_op: prism_scene::render_enums::BlendOp::Add,
                },
            ));
        }
    }
    out
}

/// 一格的**世界空间** AABB（单位四边形四角变换后取 min/max）。
///
/// `model` 已经包含格子尺寸与实体变换，所以这里用 `±0.5` 的局部坐标，
/// 不能再乘一次 `cell_size`。
fn cell_bounds(model: &Mat4) -> Aabb {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for (x, y) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
        let point = model.transform_point3(Vec3::new(x, y, 0.0));
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    Aabb { min, max }
}

/// 世界里所有 `Tilemap` 实体的绘制项（含视锥裁剪）。
pub(crate) fn tilemap_entries(
    world: &World,
    frustum: Option<&Frustum>,
) -> Vec<(SortKey, SpriteDrawItem)> {
    let mut out = Vec::new();
    for (entity, transform, tilemap) in world.query2::<LocalToWorld, Tilemap>() {
        if !crate::scene::systems::hierarchy::is_active_in_hierarchy(world, entity) {
            continue;
        }
        let Some(tileset) = world.get::<TilemapRef>(entity) else {
            continue;
        };
        out.extend(tilemap_draw_items(
            tilemap,
            tileset,
            &transform.0,
            entity.id(),
            frustum,
        ));
    }
    out
}

#[cfg(test)]
#[path = "tilemap_tests.rs"]
mod tests;
