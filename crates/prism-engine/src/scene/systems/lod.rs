//! 距离分级 LOD（#491）。
//!
//! 父实体挂 [`LodGroup`] 时，它的 [`Child`] 列表按顺序表示 LOD0、LOD1……
//! 每一帧按父实体到**最近启用相机**的距离选一级，并给每个子实体写运行时
//! [`LodHidden`]：
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

    let eyes: Vec<glam::Vec3> = world
        .query2::<Camera, LocalToWorld>()
        .filter(|(_, camera, _)| camera.enabled)
        .map(|(_, _, world)| world.0.w_axis.truncate())
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
        // 多相机时用最近的一台：只要有一台相机能看到最近级，就显示最近级，
        // 和主相机剔除的「任意相机可见即保留」语义一致。
        let distance = eyes
            .iter()
            .map(|eye| (*eye - position).length())
            .fold(f32::INFINITY, f32::min);
        let Some(level) = group.level_for_distance(distance, children.len()) else {
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
