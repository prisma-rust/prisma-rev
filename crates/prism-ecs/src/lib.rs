//! PrismaRev ECS core.
//!
//! A minimal, data-oriented Entity-Component-System: entities are cheap
//! integer handles, components are plain data stored in type-indexed sparse
//! maps, and systems are ordinary functions that query the world for
//! component slices.

#![deny(warnings)]

use std::any::{Any, TypeId};
use std::collections::HashMap;

/// `.Collections`-shaped containers (`NativeArray` / `NativeList` / ...).
pub mod collections;
/// DOTS `EntityManager`-shaped [`World`] facade.
pub mod entity_manager;
/// `.Jobs`-shaped jobs and completion handles.
pub mod jobs;
/// DOTS `ISystem` / `SystemBase` / `ComponentSystemGroup`-shaped systems.
pub mod systems;

pub use collections::{Allocator, NativeArray, NativeList, NativeReference, NativeSlice};
pub use entity_manager::EntityManager;
pub use jobs::{
    schedule, schedule_for, schedule_parallel_for, IJob, IJobFor, IJobParallelFor, JobHandle,
};
pub use systems::{
    ComponentSystemGroup, ISystem, SystemBase, SystemGroup, INITIALIZATION_SYSTEM_GROUP,
    PRESENTATION_SYSTEM_GROUP, SIMULATION_SYSTEM_GROUP,
};

/// A lightweight handle to a game object. Carries a generation so stale
/// handles left over after deletion are distinguishable from recycled ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity {
    id: u32,
    generation: u32,
}

impl Entity {
    /// Raw index into the entity allocator. Stable for the entity's lifetime.
    pub fn id(self) -> u32 {
        self.id
    }

    /// Monotonically increasing version; bumped each time the slot is recycled.
    pub fn generation(self) -> u32 {
        self.generation
    }

    /// Construct an entity handle from raw parts. Intended for sentinel uses
    /// (e.g. the editor's per-component-type euler cache, which needs a stable
    /// key not tied to any real entity); normal entity creation goes through
    /// [`World::spawn`].
    pub fn from_raw(id: u32, generation: u32) -> Self {
        Self { id, generation }
    }
}

/// Marker for component types. Components must be `'static + Send + Sync` so
/// they can be stored in type-erased pools and safely sent across threads.
pub trait Component: 'static + Send + Sync {}

// Blanket impl: any plain `'static + Send + Sync` data is a component.
impl<T: 'static + Send + Sync> Component for T {}

/// Central storage for all entities and their components.
///
/// Component data lives in `ComponentPool`s keyed by [`TypeId`]. Each pool is
/// a sparse map from entity id -> value, so adding/removing components is cheap
/// and entities can have arbitrary component combinations (no archetypes yet).
pub struct World {
    /// Entity slots. Each slot holds the generation the slot currently
    /// represents: for a live entity it matches that entity's generation;
    /// for a freed/recyclable slot it holds the generation the *next*
    /// recycled handle will have (old + 1), so stale handles stay dead.
    entities: Vec<u32>,
    /// Parallel to `entities`: `true` while the slot holds a live entity.
    ///
    /// A freed slot keeps its bumped generation but is marked dead here, so a
    /// handle forged as `{ id, generation + 1 }` (which matches the stored
    /// generation) cannot touch the slot before it is actually recycled.
    alive: Vec<bool>,
    /// Parallel to `entities`: whether each live entity is active (i.e.
    /// should be included in queries). Inactive entities remain in the world
    /// but are skipped by all query iterators.
    active: Vec<bool>,
    /// Indices of freed slots available for reuse.
    free: Vec<u32>,
    /// Component storage: one pool per type. Pools are stored type-erased as
    /// `dyn ErasedPool` (which is also `Any`) so [`Self::despawn`] can drop a
    /// component without knowing its concrete type while typed accessors
    /// downcast back to `ComponentPool<T>`.
    pools: HashMap<TypeId, Box<dyn ErasedPool>>,
    /// Global singleton resources, keyed by type. Used for data like the
    /// camera or `RenderState` that doesn't belong to any single entity.
    resources: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl World {
    pub fn new() -> Self {
        Self {
            entities: Vec::new(),
            alive: Vec::new(),
            active: Vec::new(),
            free: Vec::new(),
            pools: HashMap::new(),
            resources: HashMap::new(),
        }
    }

    /// Borrow this world as the DOTS-shaped [`EntityManager`] (mirrors
    /// `World.EntityManager`).
    pub fn entity_manager(&mut self) -> EntityManager<'_> {
        EntityManager::new(self)
    }

    /// Allocate a fresh entity handle (starts active).
    pub fn spawn(&mut self) -> Entity {
        if let Some(id) = self.free.pop() {
            // Recycle a freed slot: the stored generation is already the next
            // one (old + 1), so a recycled handle is distinguishable from the
            // stale handle that was just freed.
            let generation = self.entities[id as usize];
            self.alive[id as usize] = true;
            self.active[id as usize] = true;
            Entity { id, generation }
        } else {
            let id = self.entities.len() as u32;
            self.entities.push(0);
            self.alive.push(true);
            self.active.push(true);
            Entity { id, generation: 0 }
        }
    }

    /// Mark an entity as deleted; its slot becomes recyclable and its
    /// components are dropped.
    pub fn despawn(&mut self, entity: Entity) {
        if self.is_alive(entity) {
            // Drop all components for this entity from every pool. Each pool
            // is `dyn ErasedPool`, so this needs no concrete type.
            for pool in self.pools.values_mut() {
                pool.remove(entity.id);
            }
            // Store the *next* generation so the slot can be recycled with a
            // fresh, distinguishable handle, and mark the slot dead so a handle
            // forged as `{ id, generation + 1 }` cannot touch it before the
            // recycle actually happens.
            self.entities[entity.id as usize] = entity.generation + 1;
            self.alive[entity.id as usize] = false;
            self.free.push(entity.id);
        }
    }

    /// True if `entity` refers to a currently live slot.
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.alive.get(entity.id as usize).copied().unwrap_or(false)
            && self.entities.get(entity.id as usize) == Some(&entity.generation)
    }

    /// True if the entity is alive and active. Inactive entities are excluded
    /// from queries so they effectively stop participating in all systems.
    pub fn is_active(&self, entity: Entity) -> bool {
        self.is_alive(entity) && self.active.get(entity.id as usize).copied().unwrap_or(true)
    }

    /// Set the active state of a live entity. Has no effect on dead entities.
    pub fn set_active(&mut self, entity: Entity, value: bool) {
        if self.is_alive(entity) {
            if let Some(slot) = self.active.get_mut(entity.id as usize) {
                *slot = value;
            }
        }
    }

    /// Attach a component value to `entity`, replacing any existing one of the
    /// same type. No-op (and logged) if the entity is not alive.
    pub fn insert<T: Component>(&mut self, entity: Entity, component: T) {
        if !self.is_alive(entity) {
            log::trace!("insert on dead entity {entity:?} ignored");
            return;
        }
        let pool = self
            .pools
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(ComponentPool::<T>::new()));
        pool_downcast_mut::<T>(pool.as_mut()).insert(entity.id, component);
    }
    /// Borrow a component if present.
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        if !self.is_alive(entity) {
            return None;
        }
        self.pools
            .get(&TypeId::of::<T>())
            .and_then(|pool| pool_downcast_ref::<T>(pool.as_ref()).get(entity.id))
    }

    /// Mutably borrow a component if present.
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        if !self.is_alive(entity) {
            return None;
        }
        self.pools
            .get_mut(&TypeId::of::<T>())
            .and_then(|pool| pool_downcast_mut::<T>(pool.as_mut()).get_mut(entity.id))
    }

    /// Remove a component type from `entity`, returning the owned value.
    pub fn remove<T: Component>(&mut self, entity: Entity) -> Option<T> {
        if !self.is_alive(entity) {
            return None;
        }
        self.pools
            .get_mut(&TypeId::of::<T>())
            .and_then(|pool| pool_downcast_mut::<T>(pool.as_mut()).remove(entity.id))
    }

    /// Iterate over all `(Entity, &T)` pairs for a single component type.
    ///
    /// Lazily walks the component's dense storage; the entity generation is
    /// read directly from `self.entities` (no per-query clone). Inactive
    /// entities are skipped.
    pub fn query<T: Component>(&self) -> impl Iterator<Item = (Entity, &T)> {
        let entities = &self.entities;
        let active = &self.active;
        let pool = self.pools.get(&TypeId::of::<T>());
        pool.into_iter()
            .flat_map(move |p| pool_downcast_ref::<T>(p.as_ref()).iter())
            .filter_map(move |(id, value)| {
                let generation = *entities.get(id as usize)?;
                if !active.get(id as usize).copied().unwrap_or(true) {
                    return None;
                }
                Some((Entity { id, generation }, value))
            })
    }

    /// Like [`Self::query`], but **includes** inactive entities.
    ///
    /// Iterates all alive entities that have component `T`, regardless of the
    /// per-entity active flag. Used by the editor's entity tree to show
    /// disabled entities in the hierarchy.
    pub fn query_inactive_inclusive<T: Component>(&self) -> impl Iterator<Item = (Entity, &T)> {
        let entities = &self.entities;
        let pool = self.pools.get(&TypeId::of::<T>());
        pool.into_iter()
            .flat_map(move |p| pool_downcast_ref::<T>(p.as_ref()).iter())
            .filter_map(move |(id, value)| {
                let generation = *entities.get(id as usize)?;
                Some((Entity { id, generation }, value))
            })
    }

    /// Iterate over all `(Entity, &mut T)` pairs for a single component type.
    /// Inactive entities are skipped.
    pub fn query_mut<T: Component>(&mut self) -> impl Iterator<Item = (Entity, &mut T)> {
        let entities = &self.entities;
        let active = &self.active;
        let pool = self.pools.get_mut(&TypeId::of::<T>());
        pool.into_iter()
            .flat_map(move |p| pool_downcast_mut::<T>(p.as_mut()).iter_mut())
            .filter_map(move |(id, value)| {
                let generation = *entities.get(id as usize)?;
                // `active` and `pools` are disjoint fields, so the shared read
                // stays valid while the pool borrow is alive.
                if !active.get(id as usize).copied().unwrap_or(true) {
                    return None;
                }
                Some((Entity { id, generation }, value))
            })
    }

    /// Lazily iterate over entities that have **both** `A` and `B`, yielding
    /// `(Entity, &A, &B)`. This is a sparse-set join: it walks pool `A` and
    /// probes pool `B` for each entity id, allocating nothing. Inactive
    /// entities are skipped.
    pub fn query2<A: Component, B: Component>(&self) -> impl Iterator<Item = (Entity, &A, &B)> {
        let entities = &self.entities;
        let active = &self.active;
        let pool_a = self
            .pools
            .get(&TypeId::of::<A>())
            .map(|p| pool_downcast_ref::<A>(p.as_ref()));
        let pool_b = self
            .pools
            .get(&TypeId::of::<B>())
            .map(|p| pool_downcast_ref::<B>(p.as_ref()));
        pool_a.into_iter().flat_map(move |a| {
            pool_b.into_iter().flat_map(move |b| {
                a.iter().filter_map(move |(id, av)| {
                    let bv = b.get(id)?;
                    let generation = *entities.get(id as usize).unwrap_or(&0);
                    if !active.get(id as usize).copied().unwrap_or(true) {
                        return None;
                    }
                    Some((Entity { id, generation }, av, bv))
                })
            })
        })
    }

    /// Lazily iterate over entities that have `A`, `B`, and `C` simultaneously.
    /// Inactive entities are skipped.
    pub fn query3<A: Component, B: Component, C: Component>(
        &self,
    ) -> impl Iterator<Item = (Entity, &A, &B, &C)> {
        let entities = &self.entities;
        let active = &self.active;
        let pool_a = self
            .pools
            .get(&TypeId::of::<A>())
            .map(|p| pool_downcast_ref::<A>(p.as_ref()));
        let pool_b = self
            .pools
            .get(&TypeId::of::<B>())
            .map(|p| pool_downcast_ref::<B>(p.as_ref()));
        let pool_c = self
            .pools
            .get(&TypeId::of::<C>())
            .map(|p| pool_downcast_ref::<C>(p.as_ref()));
        pool_a.into_iter().flat_map(move |a| {
            pool_b.into_iter().flat_map(move |b| {
                pool_c.into_iter().flat_map(move |c| {
                    a.iter().filter_map(move |(id, av)| {
                        let bv = b.get(id)?;
                        let cv = c.get(id)?;
                        let generation = *entities.get(id as usize).unwrap_or(&0);
                        if !active.get(id as usize).copied().unwrap_or(true) {
                            return None;
                        }
                        Some((Entity { id, generation }, av, bv, cv))
                    })
                })
            })
        })
    }

    /// Mutable two-component query: `(Entity, &mut A, &B)`. The first component
    /// is mutable, the second shared. This is the common pattern for systems
    /// that write to a transform while reading a mesh/handle. Returns a lazy
    /// iterator (no allocation).
    ///
    /// # Borrowing argument
    ///
    /// `pools[A]` and `pools[B]` are distinct `HashMap` entries, but a plain
    /// `get_mut` + `get` pair is not expressible in safe code (the second
    /// borrow of the map invalidates the first). [`HashMap::get_disjoint_mut`]
    /// hands out both entries at once, so the two references are provably
    /// disjoint and no raw pointer / `unsafe` is needed. `get_disjoint_mut`
    /// panics on duplicate keys, which is why the same-type query is rejected
    /// before the call; `A` and `B` are otherwise different types, so their
    /// pools never alias.
    pub fn query2_mut<A: Component, B: Component>(
        &mut self,
    ) -> Box<dyn Iterator<Item = (Entity, &mut A, &B)> + '_> {
        let type_a = TypeId::of::<A>();
        let type_b = TypeId::of::<B>();
        // `get_disjoint_mut` panics for duplicate keys.
        if type_a == type_b {
            return Box::new(std::iter::empty());
        }
        let generation_for = &self.entities;
        let active = &self.active;
        let [pool_a, pool_b] = self.pools.get_disjoint_mut([&type_a, &type_b]);
        let (a, b) = match (pool_a, pool_b) {
            (Some(a), Some(b)) => (
                pool_downcast_mut::<A>(a.as_mut()),
                pool_downcast_ref::<B>(&**b),
            ),
            _ => return Box::new(std::iter::empty()),
        };
        Box::new(a.iter_mut().filter_map(move |(id, av)| {
            let bv = b.get(id)?;
            let generation = *generation_for.get(id as usize).unwrap_or(&0);
            if !active.get(id as usize).copied().unwrap_or(true) {
                return None;
            }
            Some((Entity { id, generation }, av, bv))
        }))
    }

    // Component-type enumeration (for editor auto-recognition).

    /// Iterate over every component type currently stored in the world,
    /// yielding `(TypeId, type_name)`. This is the foundation of the editor's
    /// auto-recognition inspector: it can list every component on an entity
    /// without hardcoding the component types.
    ///
    /// Order is unspecified (driven by `HashMap` iteration); callers that need
    /// a stable order must sort. Every type that has had a component inserted
    /// is yielded, even if its pool is currently empty (a pool is created on
    /// the first insert and never removed).
    pub fn iter_component_types(&self) -> impl Iterator<Item = (TypeId, &'static str)> + '_ {
        self.pools
            .iter()
            .map(|(type_id, pool)| (*type_id, pool.type_name()))
    }

    /// True if `entity` has a component of the erased `type_id`. Used together
    /// with [`World::iter_component_types`] to enumerate an entity's components
    /// without knowing their concrete types.
    pub fn has_component(&self, entity: Entity, type_id: TypeId) -> bool {
        if !self.is_alive(entity) {
            return false;
        }
        self.pools
            .get(&type_id)
            .is_some_and(|pool| pool.contains(entity.id))
    }

    // Resources: global singleton data not tied to an entity.

    /// Insert a global resource, replacing any existing one of the same type.
    /// Resources are singletons keyed by type: camera, `RenderState`, etc.
    pub fn insert_resource<R: 'static + Send + Sync>(&mut self, resource: R) {
        self.resources.insert(TypeId::of::<R>(), Box::new(resource));
    }

    /// Borrow a global resource by type if it exists.
    pub fn get_resource<R: 'static>(&self) -> Option<&R> {
        self.resources
            .get(&TypeId::of::<R>())
            .and_then(|b| b.downcast_ref::<R>())
    }

    /// Mutably borrow a global resource by type if it exists.
    pub fn get_resource_mut<R: 'static>(&mut self) -> Option<&mut R> {
        self.resources
            .get_mut(&TypeId::of::<R>())
            .and_then(|b| b.downcast_mut::<R>())
    }

    /// Remove a global resource by type, returning it if it existed.
    pub fn remove_resource<R: 'static + Send + Sync>(&mut self) -> Option<R> {
        self.resources
            .remove(&TypeId::of::<R>())
            .and_then(|b| b.downcast::<R>().ok())
            .map(|b| *b)
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

/// Type-erased view of a pool. Inherits `Any` so typed accessors can still
/// downcast back to `ComponentPool<T>`.
///
/// The `contains` / `type_name` methods exist to let the editor
/// (`prism-editor`) enumerate which component types an entity has without
/// knowing the concrete types — the foundation of the auto-recognition
/// inspector. They are not used by the core ECS itself.
///
/// `Send + 'static` is required so the world can be moved across threads.
trait ErasedPool: Any + Send + 'static {
    fn remove(&mut self, id: u32);
    /// True if `id` currently has a component in this pool.
    fn contains(&self, id: u32) -> bool;
    /// Stable Rust type name (`std::any::type_name::<T>()`), for display.
    fn type_name(&self) -> &'static str;
}

/// Sparse-set storage for one component type.
///
/// Components are stored contiguously in `dense` (cache-friendly, no
/// per-component heap allocation or type erasure). `dense_entities[i]` is the
/// entity id of `dense[i]`; `sparse[id]` maps entity id -> index in `dense`
/// (`SPARSE_NONE` means "not present"). Iteration walks `dense` directly, so
/// queries are allocation-free and cache-coherent.
struct ComponentPool<T> {
    dense: Vec<T>,
    dense_entities: Vec<u32>,
    sparse: Vec<u32>,
}

/// Sentinel stored in `sparse` for entity ids that have no component.
const SPARSE_NONE: u32 = u32::MAX;

impl<T: 'static> ComponentPool<T> {
    fn new() -> Self {
        Self {
            dense: Vec::new(),
            dense_entities: Vec::new(),
            sparse: Vec::new(),
        }
    }

    fn insert(&mut self, id: u32, value: T) {
        if id as usize >= self.sparse.len() {
            self.sparse.resize(id as usize + 1, SPARSE_NONE);
        }
        let idx = self.sparse[id as usize] as usize;
        if idx < self.dense.len() {
            // Already present: overwrite in place (no reordering).
            self.dense[idx] = value;
        } else {
            self.sparse[id as usize] = self.dense.len() as u32;
            self.dense.push(value);
            self.dense_entities.push(id);
        }
    }

    fn get(&self, id: u32) -> Option<&T> {
        let idx = self.sparse.get(id as usize).copied()? as usize;
        if idx < self.dense.len() {
            Some(&self.dense[idx])
        } else {
            None
        }
    }

    fn get_mut(&mut self, id: u32) -> Option<&mut T> {
        let idx = self.sparse.get(id as usize).copied()? as usize;
        if idx < self.dense.len() {
            Some(&mut self.dense[idx])
        } else {
            None
        }
    }

    /// Remove and return the component for `id`, if present. Uses `swap_remove`
    /// so `dense` stays contiguous; the moved-last entity's `sparse` entry is
    /// patched to its new index.
    fn remove(&mut self, id: u32) -> Option<T> {
        let idx = *self.sparse.get(id as usize)?;
        if idx == SPARSE_NONE || idx as usize >= self.dense.len() {
            return None;
        }
        let idx = idx as usize;
        let last = self.dense.len() - 1;
        let value = self.dense.swap_remove(idx);
        self.dense_entities.swap_remove(idx);
        if idx < last {
            let moved_id = self.dense_entities[idx];
            self.sparse[moved_id as usize] = idx as u32;
        }
        self.sparse[id as usize] = SPARSE_NONE;
        Some(value)
    }

    fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        self.dense_entities.iter().copied().zip(self.dense.iter())
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (u32, &mut T)> {
        self.dense_entities
            .iter()
            .copied()
            .zip(self.dense.iter_mut())
    }

    /// True if `id` has a component in this pool. Mirrors the lookup logic of
    /// [`ComponentPool::get`] but without returning the value.
    fn contains(&self, id: u32) -> bool {
        match self.sparse.get(id as usize).copied() {
            Some(idx) => (idx as usize) < self.dense.len(),
            None => false,
        }
    }
}

impl<T: 'static + Send + Sync> ErasedPool for ComponentPool<T> {
    fn remove(&mut self, id: u32) {
        self.remove(id); // drops the value
    }

    fn contains(&self, id: u32) -> bool {
        ComponentPool::contains(self, id)
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }
}

fn pool_downcast_ref<T: 'static>(pool: &dyn ErasedPool) -> &ComponentPool<T> {
    let any: &dyn Any = pool;
    any.downcast_ref::<ComponentPool<T>>()
        .expect("pool TypeId mismatch")
}

fn pool_downcast_mut<T: 'static>(pool: &mut dyn ErasedPool) -> &mut ComponentPool<T> {
    let any: &mut dyn Any = pool;
    any.downcast_mut::<ComponentPool<T>>()
        .expect("pool TypeId mismatch")
}

#[cfg(test)]
mod tests;
