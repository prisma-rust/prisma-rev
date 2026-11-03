//! DOTS `.Entities.EntityManager`-shaped [`World`] facade.
//!
//! This is an **additive alignment layer**: the core data structures remain
//! [`World`] / [`Entity`]; this module only exposes the engine's structural
//! change entry points under familiar names, so people coming from DOTS have a
//! consistent entry point and future `SystemBase`-style code has one place to
//! call.
//!
//! Naming (Rust uses snake_case; the DOTS name is in parentheses):
//! [`create_entity`](EntityManager::create_entity) (`CreateEntity`),
//! [`destroy_entity`](EntityManager::destroy_entity) (`DestroyEntity`),
//! [`add_component`](EntityManager::add_component) (`AddComponent`/`AddComponentData`),
//! [`set_component_data`](EntityManager::set_component_data) (`SetComponentData`),
//! [`get_component_data`](EntityManager::get_component_data) (`GetComponentData`),
//! [`has_component`](EntityManager::has_component) (`HasComponent`),
//! [`remove_component`](EntityManager::remove_component) (`RemoveComponent`),
//! [`exists`](EntityManager::exists) (`Exists`),
//! [`set_enabled`](EntityManager::set_enabled) (`SetEnabled`).

use crate::{Component, Entity, World};

/// Mutable borrow facade over [`World`] exposing the DOTS
/// `EntityManager`-shaped structural-change API.
///
/// In DOTS, `EntityManager` is the only entry point for structural changes
/// (adding/removing components, creating/destroying entities); here it likewise
/// holds `&mut World`, keeping it separate from read-only query semantics.
pub struct EntityManager<'w> {
    world: &'w mut World,
}

impl<'w> EntityManager<'w> {
    /// Borrows `world` to construct the facade (DOTS: `World.EntityManager`).
    pub fn new(world: &'w mut World) -> Self {
        Self { world }
    }

    /// Read-only access to the underlying [`World`].
    pub fn world(&self) -> &World {
        self.world
    }

    /// Mutable access to the underlying [`World`].
    pub fn world_mut(&mut self) -> &mut World {
        self.world
    }

    /// Consumes the facade and returns the borrow of the underlying [`World`].
    pub fn into_world(self) -> &'w mut World {
        self.world
    }

    /// Creates an entity (DOTS: `EntityManager.CreateEntity`).
    pub fn create_entity(&mut self) -> Entity {
        self.world.spawn()
    }

    /// Destroys an entity (DOTS: `EntityManager.DestroyEntity`).
    pub fn destroy_entity(&mut self, entity: Entity) {
        self.world.despawn(entity);
    }

    /// Whether the entity is alive (DOTS: `EntityManager.Exists`).
    pub fn exists(&self, entity: Entity) -> bool {
        self.world.is_alive(entity)
    }

    /// Whether the entity is enabled (DOTS: `EntityManager.IsEnabled`).
    ///
    /// Maps to this project's [`World::is_active`]; hidden entities are skipped
    /// by regular render queries.
    pub fn is_enabled(&self, entity: Entity) -> bool {
        self.world.is_active(entity)
    }

    /// Sets the entity's enabled state (DOTS: `EntityManager.SetEnabled`).
    pub fn set_enabled(&mut self, entity: Entity, value: bool) {
        self.world.set_active(entity, value);
    }

    /// Adds or overwrites a component (DOTS: `AddComponent` /
    /// `AddComponentData`).
    pub fn add_component<T: Component>(&mut self, entity: Entity, component: T) {
        self.world.insert(entity, component);
    }

    /// Overwrites the component data of an existing entity (DOTS:
    /// `SetComponentData`).
    pub fn set_component_data<T: Component>(&mut self, entity: Entity, component: T) {
        self.world.insert(entity, component);
    }

    /// Reads a component (DOTS: `GetComponentData`).
    pub fn get_component_data<T: Component>(&self, entity: Entity) -> Option<&T> {
        self.world.get::<T>(entity)
    }

    /// Mutably reads a component (the reference form of DOTS
    /// `GetComponentData`).
    pub fn get_component_data_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        self.world.get_mut::<T>(entity)
    }

    /// Whether the entity has this component (DOTS: `HasComponent`).
    pub fn has_component<T: Component>(&self, entity: Entity) -> bool {
        self.world.get::<T>(entity).is_some()
    }

    /// Removes a component and returns its data (DOTS: `RemoveComponent`).
    pub fn remove_component<T: Component>(&mut self, entity: Entity) -> Option<T> {
        self.world.remove::<T>(entity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Health(i32);

    #[test]
    fn entity_manager_crud_roundtrip() {
        let mut world = World::new();
        let mut em = EntityManager::new(&mut world);

        let e = em.create_entity();
        assert!(em.exists(e));

        em.add_component(e, Health(10));
        assert!(em.has_component::<Health>(e));
        assert_eq!(em.get_component_data::<Health>(e), Some(&Health(10)));

        em.set_component_data(e, Health(3));
        assert_eq!(em.get_component_data::<Health>(e), Some(&Health(3)));

        assert_eq!(em.remove_component::<Health>(e), Some(Health(3)));
        assert!(!em.has_component::<Health>(e));

        em.destroy_entity(e);
        assert!(!em.exists(e));
    }

    #[test]
    fn entity_manager_enabled_matches_active() {
        let mut world = World::new();
        let mut em = EntityManager::new(&mut world);
        let e = em.create_entity();
        assert!(em.is_enabled(e));
        em.set_enabled(e, false);
        assert!(!em.is_enabled(e));
    }

    #[test]
    fn world_entity_manager_convenience() {
        let mut world = World::new();
        let e = world.entity_manager().create_entity();
        assert!(world.is_alive(e));
    }
}
