// 蒙皮（#560 P0-3b）：把 `SkinnedMeshRenderer` 实体接成绘制项。
//
// 单独一个文件而不是塞进 `render.rs`：静态网格那条循环已经很满，这里的分支
// 还要顺手把每个实体的骨骼调色板追加进帧包。

use prism_ecs::{Entity, World};
use prism_render::culling::{Aabb, Frustum};
use prism_render::render_graph::DrawItem;

use super::animator::BonePalette;
use super::render::{draw_state, visible_in_frustums};
use crate::scene::components::{
    LocalToWorld, MaterialRef, MaterialSlots, MeshRef, MeshSubmeshes, SkinnedMeshRenderer,
};
use crate::scene::systems::render::MeshBounds;

/// 每帧最大骨骼矩阵数（与 `ForwardPass::BONE_CAPACITY` 一致）。
///
/// 超出的实体退回 bind pose（`bone_offset = None`）并告警一次，而不是让
/// 多个实例的调色板互相覆盖。
const BONE_BUDGET: usize = prism_render::forward_pass::ForwardPass::BONE_CAPACITY;

/// 把 `SkinnedMeshRenderer` 实体追加到绘制列表，并收集它们的骨骼调色板。
///
/// `bones` 是整帧共享的调色板缓冲：每个实体把自己 `BonePalette` 的矩阵追加在
/// 尾部，绘制项只记 `bone_offset`（顶点阶段按它索引 SSBO），所以同一个骨架
/// 被多个网格复用时也不需要复制。
///
/// 返回追加的绘制项数量。
pub fn append_skinned_draw_items(
    world: &World,
    frustum: Option<&Frustum>,
    out: &mut Vec<DrawItem>,
    bones: &mut Vec<[[f32; 4]; 4]>,
) -> usize {
    match frustum {
        Some(frustum) => append_skinned_draw_items_with_frustums(
            world,
            std::slice::from_ref(frustum),
            out,
            bones,
        ),
        None => append_skinned_draw_items_with_frustums(world, &[], out, bones),
    }
}

/// [`append_skinned_draw_items`] for a set of camera frustums (#491).
///
/// 与静态网格一致：只要任意一台相机能看到就保留，空切片 = 不剔除。
pub fn append_skinned_draw_items_with_frustums(
    world: &World,
    frustums: &[Frustum],
    out: &mut Vec<DrawItem>,
    bones: &mut Vec<[[f32; 4]; 4]>,
) -> usize {
    let bounds = world.get_resource::<MeshBounds>();
    let mut appended = 0;

    for (entity, wt, mr, skinned) in world.query3::<LocalToWorld, MeshRef, SkinnedMeshRenderer>() {
        if !crate::scene::systems::hierarchy::is_active_in_hierarchy(world, entity) {
            continue;
        }
        let model = wt.0.to_cols_array_2d();
        let palette = world
            .get::<BonePalette>(entity)
            .filter(|palette| !palette.matrices.is_empty());
        // 剔除用**蒙皮后**的包围盒：bind pose 的盒子在角色摆大动作时会偏，
        // 用它做剔除会把还在画面里的角色剔掉（#560 P0-3c）。
        let local_bounds = bounds
            .and_then(|bounds| bounds.get(mr.render_handle))
            .map(|bind| match palette {
                Some(palette) => skinned_local_bounds(bind, &palette.matrices).unwrap_or(*bind),
                None => *bind,
            });
        let world_bounds = local_bounds.and_then(|local| local.transformed(&model));
        if !visible_in_frustums(frustums, world_bounds.as_ref()) {
            continue;
        }

        let bone_offset = append_palette(palette, entity, BONE_BUDGET, bones);
        let renderer = skinned.as_mesh_renderer();
        let state = draw_state(Some(&renderer));
        let material_ref = world.get::<MaterialRef>(entity);
        let material_slots = world.get::<MaterialSlots>(entity);
        let submeshes = world.get::<MeshSubmeshes>(entity);
        let mode = skinned.shadow_casting_mode;

        let has_submeshes = submeshes.is_some_and(|submeshes| !submeshes.submeshes.is_empty());
        if has_submeshes {
            for (index, submesh) in submeshes.unwrap().submeshes.iter().enumerate() {
                let material = match material_slots {
                    Some(slots) => slots.slots.get(index).copied().flatten(),
                    None => material_ref.map(|material| material.material_slot),
                };
                out.push(DrawItem {
                    mesh: mr.render_handle,
                    entity_id: entity.id(),
                    model,
                    material,
                    cast_shadow: mode.casts_shadows(),
                    visible_in_main_pass: mode.visible_in_main_pass(),
                    cast_shadow_two_sided: mode.casts_two_sided_shadows(),
                    state,
                    world_bounds,
                    index_start: submesh.index_start,
                    index_count: Some(submesh.index_count),
                    bone_offset,
                    splat_slots: [u32::MAX; 4],
                    splat_tiling: 0.0,
                });
                appended += 1;
            }
        } else {
            let material = match material_slots {
                Some(slots) => slots.slots.first().copied().flatten(),
                None => material_ref.map(|material| material.material_slot),
            };
            out.push(DrawItem {
                mesh: mr.render_handle,
                entity_id: entity.id(),
                model,
                material,
                cast_shadow: mode.casts_shadows(),
                visible_in_main_pass: mode.visible_in_main_pass(),
                cast_shadow_two_sided: mode.casts_two_sided_shadows(),
                state,
                world_bounds,
                index_start: 0,
                index_count: None,
                bone_offset,
                splat_slots: [u32::MAX; 4],
                splat_tiling: 0.0,
            });
            appended += 1;
        }
    }

    appended
}

/// 追加一个实体的骨骼调色板，返回它在整帧缓冲里的起始下标。
///
/// 没有 `BonePalette`（动画系统还没跑 / 骨架缺失）或超出预算时返回 `None`，
/// 绘制项按 bind pose 画。
fn append_palette(
    palette: Option<&BonePalette>,
    entity: Entity,
    budget: usize,
    bones: &mut Vec<[[f32; 4]; 4]>,
) -> Option<u32> {
    let palette = palette?;
    if palette.matrices.is_empty() {
        return None;
    }
    if bones.len() + palette.matrices.len() > budget {
        log::warn!(
            "skinning: bone palette budget exhausted ({} + {} > {budget}); \
             entity {} is drawn in its bind pose",
            bones.len(),
            palette.matrices.len(),
            entity.id()
        );
        return None;
    }
    let offset = bones.len() as u32;
    bones.extend_from_slice(&palette.matrices);
    Some(offset)
}

/// 蒙皮后的局部包围盒：把 bind 包围盒按调色板里每个骨骼矩阵变换后取并集。
///
/// 蒙皮顶点 = `Σ wᵢ · (Mᵢ · p)`（权重非负、和为 1），因此它一定落在
/// `{Mᵢ · p}` 的凸包里 —— 并集是保守上界，不会把可见几何剔掉。
/// 调色板为空或全部矩阵非有限时返回 `None`，调用方退回 bind 包围盒。
fn skinned_local_bounds(bind: &Aabb, palette: &[[[f32; 4]; 4]]) -> Option<Aabb> {
    let mut merged: Option<Aabb> = None;
    for matrix in palette {
        if let Some(bounds) = bind.transformed(matrix) {
            merged = match merged {
                Some(accumulated) => accumulated.union(&bounds),
                None => Some(bounds),
            };
        }
    }
    merged
}

#[cfg(test)]
#[path = "skinned_draw_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "skinned_draw_bounds_tests.rs"]
mod bounds_tests;
