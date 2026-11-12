//! Safe API for modifying parent-child relationships.
//!
//! **Invariant**: [`Child`] is *derived* from [`Parent`] references — never
//! mutate `Child` directly.  Always use [`HierarchyHelper::reparent`].

use prism_ecs::{Entity, World};

use crate::components::{Active, Child, LocalToWorld, LocalTransform, Parent};

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
