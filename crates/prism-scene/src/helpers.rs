//! Safe API for modifying parent-child relationships.
//!
//! **Invariant**: [`Child`] is *derived* from [`Parent`] references — never
//! mutate `Child` directly.  Always use [`HierarchyHelper::reparent`].

use prism_ecs::{Entity, World};

use crate::components::{
    Active, Child, Collider2D, LocalToWorld, LocalTransform, Parent, Tilemap, TilemapCollider2D,
};
use prism_physics2d::Aabb2;

/// One world-space AABB produced by a scene collider component.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColliderBounds2D {
    pub entity: Entity,
    pub bounds: Aabb2,
    pub is_trigger: bool,
    pub layer: u32,
    pub one_way_top: bool,
}

/// Collect active 2D collider bounds, including generated Tilemap colliders.
///
/// This shared scene query keeps the static Player and dynamic project-module
/// host on the same hierarchy, transform, layer, and tile-id semantics.
pub fn colliders2d(world: &World, layer_mask: u32) -> Vec<ColliderBounds2D> {
    let mut out = Vec::new();
    for (entity, collider) in world.query::<Collider2D>() {
        if collider.effective_layer() & layer_mask == 0 || !is_active_in_hierarchy(world, entity) {
            continue;
        }
        let Some(matrix) = world_matrix(world, entity) else {
            continue;
        };
        let Some(bounds) = collider.world_bounds_with_matrix(matrix) else {
            continue;
        };
        out.push(ColliderBounds2D {
            entity,
            bounds,
            is_trigger: collider.is_sensor || collider.is_trigger,
            layer: collider.effective_layer(),
            one_way_top: collider.one_way_top,
        });
    }
    for (entity, collider) in world.query::<TilemapCollider2D>() {
        if collider.layer & layer_mask == 0 || !is_active_in_hierarchy(world, entity) {
            continue;
        }
        let Some(tilemap) = world.get::<Tilemap>(entity) else {
            continue;
        };
        let Some(matrix) = world_matrix(world, entity) else {
            continue;
        };
        out.extend(
            crate::components::tilemap_collider_shapes(tilemap, collider, matrix)
                .into_iter()
                .map(|shape| ColliderBounds2D {
                    entity,
                    bounds: shape.bounds,
                    is_trigger: collider.is_trigger,
                    layer: collider.layer,
                    one_way_top: shape.one_way_top,
                }),
        );
    }
    out.sort_by(|a, b| {
        (a.entity.id(), a.entity.generation())
            .cmp(&(b.entity.id(), b.entity.generation()))
            .then_with(|| a.bounds.min[0].total_cmp(&b.bounds.min[0]))
            .then_with(|| a.bounds.min[1].total_cmp(&b.bounds.min[1]))
            .then_with(|| a.bounds.max[0].total_cmp(&b.bounds.max[0]))
            .then_with(|| a.bounds.max[1].total_cmp(&b.bounds.max[1]))
    });
    out
}

/// Resolve an entity's current world matrix from its local hierarchy.
///
/// This reads `LocalTransform` values directly so a collision query made during
/// a gameplay step sees writes made before the hierarchy propagation pass.
/// Transform-less hierarchy nodes inherit the accumulated parent matrix. A
/// cached `LocalToWorld` is used only when the entire chain has no local
/// transform; malformed cycles return `None` rather than a stale cache.
pub fn world_matrix(world: &World, entity: Entity) -> Option<glam::Mat4> {
    if !world.is_alive(entity) {
        return None;
    }
    let mut chain = Vec::new();
    let mut cursor = entity;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(cursor) {
            return None;
        }
        chain.push(cursor);
        let Some(parent) = world.get::<Parent>(cursor).map(|parent| parent.0) else {
            break;
        };
        if !world.is_alive(parent)
            || !world
                .get::<Child>(parent)
                .is_some_and(|children| children.0.contains(&cursor))
        {
            break;
        }
        cursor = parent;
    }

    let mut matrix = glam::Mat4::IDENTITY;
    let mut has_world_matrix = false;
    for ancestor in chain.into_iter().rev() {
        if let Some(local) = world.get::<LocalTransform>(ancestor) {
            matrix *= local.to_model_matrix();
            has_world_matrix = true;
        } else if !has_world_matrix {
            if let Some(cached) = world.get::<LocalToWorld>(ancestor) {
                matrix = cached.0;
                has_world_matrix = true;
            }
        }
    }
    has_world_matrix.then_some(matrix)
}

/// Resolve the current active state from an entity through its live parent chain.
///
/// Unlike the derived `ActiveInHierarchy` component, this reads the latest
/// `Active` values, so gameplay queries remain correct before the next hierarchy
/// propagation pass. A dead parent is treated as a detached root; malformed
/// cycles terminate once every visited entity has been checked.
pub fn is_active_in_hierarchy(world: &World, entity: Entity) -> bool {
    if !world.is_alive(entity) {
        return false;
    }

    let mut cursor = entity;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(cursor) {
            return true;
        }
        if world.get::<Active>(cursor).is_some_and(|active| !active.0) {
            return false;
        }
        let Some(parent) = world.get::<Parent>(cursor).map(|parent| parent.0) else {
            return true;
        };
        if !world.is_alive(parent) {
            return true;
        }
        cursor = parent;
    }
}

/// Safe API for managing the entity hierarchy.
pub struct HierarchyHelper;

impl HierarchyHelper {
    /// Set `entity`'s parent to `new_parent`.
    ///
    /// - `Some(parent)` attaches the entity under `parent`.
    /// - `None` detaches the entity (it becomes a root node).
    /// - Updates both old and new parent's [`Child`] 列表
    /// - Self-parent and cycles (reparenting under a descendant) are rejected.
    /// - Dead entities are rejected (logged and ignored).
    ///
    /// All validation happens **before** any mutation, so a rejected call leaves
    /// the existing hierarchy untouched.
    pub fn reparent(world: &mut World, entity: Entity, new_parent: Option<Entity>) {
        if !world.is_alive(entity) {
            log::warn!("HierarchyHelper::reparent: entity not alive");
            return;
        }
        if new_parent == Some(entity) {
            log::warn!("HierarchyHelper::reparent: self-parent not allowed");
            return;
        }
        if let Some(parent) = new_parent {
            if !world.is_alive(parent) {
                log::warn!("HierarchyHelper::reparent: parent not alive");
                return;
            }
            if is_descendant(world, parent, entity) {
                log::warn!("HierarchyHelper::reparent: cycle not allowed");
                return;
            }
        }

        // 1. 移除 from old parent's Child 列表
        if let Some(old_parent) = world.get::<Parent>(entity).map(|p| p.0) {
            if let Some(children) = world.get_mut::<Child>(old_parent) {
                children.0.retain(|e| *e != entity);
            }
        }

        match new_parent {
            Some(parent) => {
                world.insert(entity, Parent(parent));
                if let Some(children) = world.get_mut::<Child>(parent) {
                    if !children.0.contains(&entity) {
                        children.0.push(entity);
                    }
                } else {
                    world.insert(parent, Child(vec![entity]));
                }
            }
            None => {
                world.remove::<Parent>(entity);
            }
        }
    }

    /// Return `true` if `entity` has at least one child.
    pub fn has_children(world: &World, entity: Entity) -> bool {
        world
            .get::<Child>(entity)
            .map(|c| !c.0.is_empty())
            .unwrap_or(false)
    }
}

/// Return `true` if `candidate` is `ancestor` or one of its descendants.
///
/// A visited set guards against pre-existing malformed cycles so the walk can
/// never loop forever.
fn is_descendant(world: &World, candidate: Entity, ancestor: Entity) -> bool {
    let mut cursor = Some(candidate);
    let mut seen = std::collections::HashSet::new();
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        if !seen.insert(current) {
            return false;
        }
        cursor = world.get::<Parent>(current).map(|parent| parent.0);
    }
    false
}

#[cfg(test)]
#[path = "helpers_tests.rs"]
mod tests;
