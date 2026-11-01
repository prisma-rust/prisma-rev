//! Hierarchy 系统 — computes [`LocalToWorld`] for every 实体
//!
//! Walks the scene 树 from root entities in DFS order, accumulating parent
//! transforms: `world_child = world_parent × local_child`.
//!
//! Run once per 帧 **after** any local-transform or reparenting changes.
//!
//! 树形不一致时按 统一语义兜底（见 `#48`）：
//! - 没有 `LocalTransform` 的中间节点**不阻断传播**：它自己保持无
//!   `LocalToWorld`，但子节点继续拿到它的累积矩阵；
//! - `Parent` 指向已死实体、或父实体的 `Child` 里并没有列出它的子节点，
//!   视为根（`LocalToWorld = LocalTransform`），而不是永远冻结在旧值。

use std::collections::HashSet;

use prism_ecs::{Entity, World};

use crate::scene::components::*;

/// Recompute [`LocalToWorld`] for every 实体 in the hierarchy.
///
/// - Roots (entities without [`Parent`]) get `LocalToWorld = LocalTransform`.
/// - Child get `LocalToWorld = parent_WorldTransform × local_Transform`.
/// - Entities without `LocalTransform` are not given a `LocalToWorld`, but they
///   no longer block their children (the accumulated parent matrix is passed
///   through).
///
/// This 函数 is intended to be called once per 帧 after all 局部
/// 变换 changes have been applied.
pub fn hierarchy_system(world: &mut World) {
    let roots: Vec<Entity> = candidate_entities(world)
        .into_iter()
        .filter(|entity| !has_live_parent(world, *entity))
        .collect();

    let mut visited: HashSet<Entity> = HashSet::new();
    for root in roots {
        if !visited.insert(root) {
            continue;
        }
        // 无 `LocalTransform` 的根（只有 `Child`）按单位矩阵向下传播。
        let world_mat = match world.get::<LocalTransform>(root).cloned() {
            Some(local) => {
                let matrix = local.to_model_matrix();
                world.insert(root, LocalToWorld(matrix));
                matrix
            }
            None => glam::Mat4::IDENTITY,
        };
        visit_children(world, root, world_mat, &mut visited);
    }
}

/// 可能参与传播的实体：带 `LocalTransform` 的，以及带 `Child` 的
/// （后者即使自己没有变换，也要让子树继续传播）。
fn candidate_entities(world: &World) -> Vec<Entity> {
    let mut entities: Vec<Entity> = world.query::<LocalTransform>().map(|(e, _)| e).collect();
    entities.extend(world.query::<Child>().map(|(e, _)| e));
    let mut seen: HashSet<Entity> = HashSet::new();
    entities.retain(|entity| seen.insert(*entity));
    entities
}

/// 实体是否挂在一个**有效的**父级下：父实体还活着，且父的 `Child` 列表里
/// 确实列出了它。任何一条不满足都按根处理（里子物体会提升为根）。
fn has_live_parent(world: &World, entity: Entity) -> bool {
    let Some(parent) = world.get::<Parent>(entity).map(|parent| parent.0) else {
        return false;
    };
    world.is_alive(parent)
        && world
            .get::<Child>(parent)
            .is_some_and(|children| children.0.contains(&entity))
}

/// Recursively visit children, 计算 and 存储 their 世界 变换
fn visit_children(
    world: &mut World,
    parent: Entity,
    parent_world: glam::Mat4,
    visited: &mut HashSet<Entity>,
) {
    // Index into the parent's child list instead of cloning it: the recursive
    // calls mutate components, so the `&Child` borrow cannot be held across
    // them, but re-reading one element per child is still allocation-free
    // (#66 — this used to clone the whole list for every node, per frame).
    let child_count = world
        .get::<Child>(parent)
        .map_or(0usize, |children| children.0.len());

    for index in 0..child_count {
        let Some(child) = world
            .get::<Child>(parent)
            .and_then(|children| children.0.get(index).copied())
        else {
            break;
        };
        // `visited` 同时兜住脏数据里可能出现的环（A.Child=[B]，B.Child=[A]）。
        if !world.is_alive(child) || !visited.insert(child) {
            continue;
        }
        let child_world = match world.get::<LocalTransform>(child).cloned() {
            Some(local) => {
                let matrix = parent_world * local.to_model_matrix();
                world.insert(child, LocalToWorld(matrix));
                matrix
            }
            // 无变换的中间节点：自身不写 `LocalToWorld`，但把父矩阵原样
            // 传给后代（里 Transform 缺失等价于单位变换）。
            None => parent_world,
        };
        visit_children(world, child, child_world, visited);
    }
}

/// 把 [`Active`] 沿父子关系传播成 [`ActiveInHierarchy`]。
///
/// Unity 的 `activeSelf`（层级里那个眼睛）与 `activeInHierarchy`（实际是否参与
/// 渲染 / 逻辑）是两件事：任一祖先 inactive，整棵子树都算 inactive。之前引擎
/// 只按单实体的 `Active` 过滤，关掉父级后子物体照常出现（#433）。
///
/// 与 [`hierarchy_system`] 一样在 `Engine::update` 末尾跑一次 —— 用户在 tick 里
/// 关掉父级，同一帧的 extract 就要看到结果。这里单独走一趟而不是塞进
/// `hierarchy_system`：矩阵传播只为「有 `LocalTransform` / `Child`」的实体建树，
/// 而 `Active` 可能挂在不带变换的纯逻辑节点上。
pub fn active_hierarchy_system(world: &mut World) {
    let mut entities: Vec<Entity> = world.query::<Active>().map(|(entity, _)| entity).collect();
    entities.extend(world.query::<Parent>().map(|(entity, _)| entity));
    entities.sort_unstable_by_key(|entity| entity.id());
    entities.dedup();

    // 沿父链累乘，命中已算过的祖先就直接复用（深树也是 O(n)）。
    let mut resolved: std::collections::HashMap<Entity, bool> = std::collections::HashMap::new();
    for entity in entities {
        let mut chain: Vec<Entity> = Vec::new();
        let mut effective = true;
        let mut cursor = Some(entity);
        while let Some(current) = cursor {
            if let Some(done) = resolved.get(&current) {
                effective &= *done;
                break;
            }
            // 脏数据里可能成环（A→B→A）：按已累计的结果收尾，别死循环。
            if chain.contains(&current) {
                break;
            }
            chain.push(current);
            effective &= world
                .get::<Active>(current)
                .map(|active| active.0)
                .unwrap_or(true);
            cursor = world
                .get::<Parent>(current)
                .map(|parent| parent.0)
                .filter(|parent| world.is_alive(*parent));
        }
        for item in chain {
            resolved.insert(item, effective);
        }
    }

    for (entity, value) in resolved {
        world.insert(entity, ActiveInHierarchy(value));
    }
}

/// 渲染 / 光照 / UI 过滤用的有效激活状态：优先派生值，其次自身 [`Active`]。
///
/// 派生组件缺失时（实体刚 spawn 还没跑过系统）退回自身 `Active`，与旧行为一致。
/// 编辑器的 Scene 可见性（[`Hidden`]）在最后叠加：隐藏只影响渲染类系统，
/// 不改 `Active` / `ActiveInHierarchy`，也不影响物理或脚本。
pub fn is_active_in_hierarchy(world: &World, entity: Entity) -> bool {
    if world.get::<Hidden>(entity).is_some_and(|hidden| hidden.0) {
        return false;
    }
    if let Some(derived) = world.get::<ActiveInHierarchy>(entity) {
        return derived.0;
    }
    world
        .get::<Active>(entity)
        .map(|active| active.0)
        .unwrap_or(true)
}

#[cfg(test)]
#[path = "hierarchy_tests.rs"]
mod tests;
