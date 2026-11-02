//! 距离分级 LOD（#491）。
//!
//! 父实体挂 [`LodGroup`] 时，它的 [`Child`] 列表按顺序表示 LOD0、LOD1……
//! 每一帧按 `LodGroup` 的配置选一级：`screen_ratios` 非空时用相机投影算屏幕
//! 高度占比，否则按父实体到**最近启用相机**的距离。选中后给每个子实体写
//! 运行时 [`LodHidden`]：
//!
//! - 当前级 `LodHidden(false)`；
//! - 其它级 `LodHidden(true)`。
//!
//! 隐藏只影响渲染类系统（`is_active_in_hierarchy` 会跳过它），**不改写**
//! 作者的 `Active`，也不碰编辑器的 `Hidden`。没有相机或距离非有限时显示
//! LOD0，保证不会因为数据缺失把整组物体藏掉。
//!
//! 系统在 `Engine::update` 的 `hierarchy_system` / `active_hierarchy_system`
//! 之后运行，所以同一帧的提取看到的是当前世界矩阵算出的距离。

use prism_ecs::{Entity, World};

use crate::scene::components::{Camera, Child, LocalToWorld, LodGroup, LodHidden};

/// 按距离给每个 `LodGroup` 的子实体写 `LodHidden`。
pub fn lod_system(world: &mut World) {
    // 先把上一帧的标记全部清掉：`LodGroup` 被删、子实体被移出 `Child`、
    // 或者相机被删时，残留的 `true` 不能让物体永远消失。
    let marked: Vec<Entity> = world
        .query::<LodHidden>()
        .map(|(entity, _)| entity)
        .collect();
    for entity in marked {
        if let Some(hidden) = world.get_mut::<LodHidden>(entity) {
            hidden.0 = false;
        }
    }

    // 屏幕占比模式需要每台相机的投影 y 缩放；正交相机不随距离缩小。
    let cameras: Vec<(glam::Vec3, f32, bool)> = world
        .query2::<Camera, LocalToWorld>()
        .filter(|(_, camera, _)| camera.enabled)
        .map(|(_, camera, world)| {
            let scale = camera.projection_matrix(camera.aspect).y_axis.y.abs();
            (world.0.w_axis.truncate(), scale, camera.orthographic)
        })
        .collect();

    let groups: Vec<(Entity, LodGroup, glam::Vec3)> = world
        .query2::<LodGroup, LocalToWorld>()
        .map(|(entity, group, world)| (entity, group.clone(), world.0.w_axis.truncate()))
        .collect();

    for (entity, group, position) in groups {
        let children: Vec<Entity> = world
            .get::<Child>(entity)
            .map(|children| children.0.clone())
            .unwrap_or_default();
        if children.is_empty() {
            continue;
        }
        // 距离模式用最近的一台相机；屏幕占比模式用占比最大的一台，语义都是
        // 「任意相机看到最近级就显示最近级」。
        let distance = cameras
            .iter()
            .map(|(eye, _, _)| (*eye - position).length())
            .fold(f32::INFINITY, f32::min);

        let radius = group.bounds_radius;
        let use_screen_ratio = !group.screen_ratios.is_empty()
            && radius.is_finite()
            && radius > 0.0
            && !cameras.is_empty();
        let level = if use_screen_ratio {
            let ratio = cameras
                .iter()
                .map(|(eye, scale, orthographic)| {
                    if *orthographic {
                        radius * *scale
                    } else {
                        let distance = (*eye - position).length();
                        if distance > 1e-6 {
                            radius * *scale / distance
                        } else {
                            1.0
                        }
                    }
                })
                .fold(0.0, f32::max);
            group
                .level_for_screen_ratio(ratio, children.len())
                .or_else(|| group.level_for_distance(distance, children.len()))
        } else {
            group.level_for_distance(distance, children.len())
        };
        let Some(level) = level else {
            continue;
        };
        for (index, child) in children.iter().enumerate() {
            world.insert(*child, LodHidden(index != level));
        }
    }
}

#[cfg(test)]
#[path = "lod_tests.rs"]
mod tests;
