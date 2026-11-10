//! # prism-game-api — the gameplay API layer that is independent of the engine
//!
//! Mirrors the engine's script binding layer: **the same gameplay code** can be
//! statically linked into the Player or compiled into a dynamic library loaded by
//! the editor host. The only difference is who sits behind [`GameWorld`]:
//!
//! - [`GameWorld::host`]: borrows the host's own `prism_ecs::World` directly, with
//!   zero serialization.
//! - [`GameWorld::plugin`]: reads and writes through the [`HostApi`] function table
//!   the host hands down; component data crosses the boundary as RTTIBin byte
//!   streams.
//!
//! Both paths expose **exactly the same function signatures**, so project code does
//! not need two implementations.
//!
//! ## Boundary rules (do not break)
//!
//! - This crate does **not** depend on `prism-engine` / `prism-render` /
//!   `prism-app`. The moment one of those appears here, the plugin must link a
//!   second copy of the engine, `World` type identity splits, and the ABI becomes
//!   UB immediately.
//! - Component types are not mirrored here: `Style` / `Text` / `Name` /
//!   `Transform` and friends come straight from `prism-scene` (a pure-data crate),
//!   which projects can depend on themselves.
//! - Only `#[repr(C)]` PODs such as [`EntityHandle`] and byte streams cross the
//!   boundary; never references, `&mut World`, or trait objects.

#[cfg(feature = "host")]
use prism_ecs::World;
mod input_actions;
pub use input_actions::{ActionFrame, ActionState, InputActionMap};
use prism_ecs::{Component, Entity};
use prism_physics2d::{Aabb2, MoveOptions2D, MoveResult2, Obstacle2D};
pub use prism_project_api::keys;
#[cfg(feature = "plugin")]
use prism_project_api::PhysicsEvent2D as AbiPhysicsEvent2D;
pub use prism_project_api::PhysicsEventPhase2D as TriggerPhase2D;
pub use prism_project_api::{EntityHandle, InputSnapshot};
/// Shared 2D geometry used by both static Player and preview-module builds.
pub mod physics2d {
    pub use prism_physics2d::*;
}
use prism_reflect::Reflect;
pub use prism_scene::components::Camera2D;
use prism_scene::components::{
    Hitbox2D, Hurtbox2D, Interactable2D, LocalTransform, Name, TransformInterpolation2D,
};

fn valid_flag_key(key: &str) -> bool {
    !key.is_empty() && !key.contains(char::from(0))
}

/// The schema layer, re-exported for the `export_project!` macro so projects do
/// not have to depend on `prism-project-api` directly.
#[doc(hidden)]
pub use prism_project_api as __project_api;

#[cfg(feature = "plugin")]
mod project_entry;

/// The builtin scene component registry every project module shares.
///
/// `export_project!` uses it for the schema fingerprint and for the RTTIBin type
/// table; a project with extra components can keep its own registry instead.
/// The instance lives in `prism-scene`, so project modules, the engine and the
/// editor all hash the same schema.
pub fn builtin_registry() -> &'static prism_scene::ComponentRegistry {
    prism_scene::builtin_registry()
}

/// Schema fingerprint of [`builtin_registry`], which the host compares with its
/// own registry in `create`.
pub fn builtin_schema_fingerprint() -> u64 {
    builtin_registry().schema_fingerprint()
}

#[cfg(feature = "plugin")]
use prism_project_api::{
    Collider2DHit, HostApi, RaycastHit2D as AbiRaycastHit2D, HOST_CAP_PHYSICS, HOST_CAP_PHYSICS_2D,
    HOST_CAP_PHYSICS_EVENTS_2D, HOST_CAP_SCENE, SUCCESS,
};
#[cfg(feature = "plugin")]
use prism_reflect::{rtti_bin, TypeRegistry};
#[cfg(test)]
use prism_scene::components::Collider2D;

/// A 2D UI quad (defined in the ABI crate, re-exported here).
pub use prism_project_api::UiQuad;

/// The result of a single raycast hit (#522).
///
/// Both backends return the same shape: `point` / `normal` are in world space and
/// `distance` is measured from the ray origin; `is_trigger` colliders take part in
/// queries but do not block movement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RaycastHit {
    pub entity: EntityHandle,
    pub distance: f32,
    pub point: [f32; 3],
    pub normal: [f32; 3],
    pub is_trigger: bool,
}

/// An active 2D collider found in the gameplay world.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColliderInstance2D {
    pub entity: EntityHandle,
    pub bounds: Aabb2,
    pub is_trigger: bool,
    pub layer: u32,
    pub one_way_top: bool,
}

/// An enabled 2D interaction target within the requested range.
#[derive(Debug, Clone, PartialEq)]
pub struct InteractableCandidate2D {
    pub entity: EntityHandle,
    pub bounds: Aabb2,
    pub prompt: String,
    pub priority: i32,
    pub distance: f32,
}

/// One active hitbox-to-hurtbox overlap in the current gameplay snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageContact2D {
    pub hitbox: EntityHandle,
    pub hurtbox: EntityHandle,
    pub damage: u32,
    pub hitbox_team: u32,
    pub hurtbox_team: u32,
}

/// One entity-pair event from the most recently completed fixed step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerEvent2D {
    pub entity_a: EntityHandle,
    pub entity_b: EntityHandle,
    pub phase: TriggerPhase2D,
}

/// The nearest active 2D collider hit by a world-space ray.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RaycastHit2D {
    pub entity: EntityHandle,
    pub distance: f32,
    pub point: [f32; 2],
    pub normal: [f32; 2],
    pub is_trigger: bool,
    pub layer: u32,
}

/// Buffers keyboard edges from render-frame snapshots until the next gameplay
/// simulation step. Held state remains frame-local; only press/release edges
/// are merged into the snapshot returned by [`Self::take_for_step`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputEdgeBuffer {
    edges: [u64; 8],
}

impl InputEdgeBuffer {
    /// Accumulate key edges observed during this render frame.
    pub fn capture(&mut self, snapshot: InputSnapshot) {
        for (pending, current) in self.edges.iter_mut().zip(snapshot.key_edges) {
            *pending |= current;
        }
    }

    /// Add accumulated edges to the current snapshot, then consume them once.
    pub fn take_for_step(&mut self, mut snapshot: InputSnapshot) -> InputSnapshot {
        for (current, pending) in snapshot.key_edges.iter_mut().zip(self.edges) {
            *current |= pending;
        }
        self.edges = [0; 8];
        snapshot
    }

    /// Discard pending edges, for example when beginning deterministic replay.
    pub fn clear(&mut self) {
        self.edges = [0; 8];
    }
}

/// Converts an ECS handle into a cross-boundary handle.
pub fn handle_from_entity(entity: Entity) -> EntityHandle {
    EntityHandle {
        id: entity.id(),
        generation: entity.generation(),
    }
}

/// Restores a cross-boundary handle into an ECS handle (used on the host side; the
/// plugin side cannot construct a real `Entity`).
pub fn entity_from_handle(handle: EntityHandle) -> Entity {
    Entity::from_raw(handle.id, handle.generation)
}

/// The world as gameplay code sees it.
///
/// Every method operates on the **host's** world; the plugin backend translates the
/// same calls into host callbacks, so callers do not need to know how they were
/// compiled.
pub struct GameWorld<'a> {
    backend: Backend<'a>,
    /// Host backend only: the plugin backend takes its input from `HostApi::input`.
    ///
    /// Normally `input()` prefers the `InputSnapshot` resource in the world (written
    /// by the engine each frame); this field is the fallback for when the resource
    /// has not been written yet, and the mirror for `set_input`.
    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    input: InputSnapshot,
}

enum Backend<'a> {
    #[cfg(feature = "host")]
    Host(&'a mut World),
    #[cfg(feature = "plugin")]
    Plugin(PluginBackend<'a>),
}

impl<'a> GameWorld<'a> {
    /// Borrows the host world directly (static Player / inside the editor host).
    #[cfg(feature = "host")]
    pub fn host(world: &'a mut World) -> Self {
        Self {
            backend: Backend::Host(world),
            input: InputSnapshot::default(),
        }
    }

    /// Operates on the host world through the function table the host hands down
    /// (inside the dynamic library).
    ///
    /// `registry` is only used to encode components into RTTIBin's type table; the
    /// decode side is self-describing, so the read path does not need it.
    #[cfg(feature = "plugin")]
    pub fn plugin(api: &'a HostApi, registry: &'a TypeRegistry) -> Self {
        Self {
            backend: Backend::Plugin(PluginBackend { api, registry }),
            input: api.input,
        }
    }

    pub fn input(&self) -> InputSnapshot {
        match &self.backend {
            // Static Player: the engine writes this copy into the World resource
            // each frame (#589).
            #[cfg(feature = "host")]
            Backend::Host(world) => world
                .get_resource::<InputSnapshot>()
                .copied()
                .unwrap_or(self.input),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.api.input,
        }
    }

    /// Samples named gameplay actions from this world's current input snapshot.
    pub fn input_actions(&self, map: &mut InputActionMap) -> ActionFrame {
        map.sample(self.input())
    }

    /// Overrides this frame's input (host backend).
    ///
    /// Not needed during normal operation: `Engine::update` writes
    /// [`prism_engine::input::input_snapshot`] into the world every frame. This entry
    /// point is for tests and for hosts that drive the world themselves; after the
    /// write, `input()` on the same frame reads the new value immediately. The plugin
    /// backend's input is handed down by the host every tick through `HostApi::input`.
    #[cfg(feature = "host")]
    pub fn set_input(&mut self, input: InputSnapshot) {
        self.input = input;
        match &mut self.backend {
            Backend::Host(world) => world.insert_resource(input),
            // The plugin backend reads the input the host hands down every tick
            // through `HostApi::input`, so there is nothing to write here.
            #[cfg(feature = "plugin")]
            Backend::Plugin(_) => {}
        }
    }

    pub fn screen_size(&self) -> [f32; 2] {
        self.input().screen
    }

    /// Interpolation fraction between the previous and current fixed simulation states.
    pub fn interpolation_alpha(&self) -> f32 {
        self.input().interpolation_alpha
    }

    /// Whether a loose asset exists (level JSON, textures with time-of-day
    /// variants, and so on).
    ///
    /// Loose files only; `.pak` contents are still handled by the runtime package.
    /// The plugin backend forwards this to the host and returns `false` when the
    /// host provides no asset callback.
    pub fn asset_exists(&self, relative: &str) -> bool {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => prism_engine::asset_paths::asset_exists(relative),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.asset_exists(relative),
        }
    }

    /// Saves the engine's built-in scene state (camera / lights / transforms,
    /// `scene_state.json`).
    ///
    /// The save path is chosen by the host (from the project's `[app] company/name`
    /// settings); returns whether it succeeded. Returns `false` and warns once when
    /// there is no host callback (#484).
    pub fn save_scene_state(&self) -> bool {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => {
                prism_engine::scene_state::save_scene_state(world);
                true
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.save_scene_state(),
        }
    }

    /// Loads the scene state: `true` = restored, `false` = no save exists or the
    /// host does not support it.
    pub fn load_scene_state(&mut self) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => prism_engine::scene_state::load_scene_state(world),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.load_scene_state(),
        }
    }

    /// 写入一条玩家存档并立即落盘（#984）。
    ///
    /// 键值都是字符串：结构由工程自己序列化（`serde_json` 也好、只看
    /// `"caught"` 这种计数也好），引擎不理解业务 schema。
    /// 插件后端走 `HostApi::save_value`（能力位 `HOST_CAP_SAVE`）；宿主没有
    /// 存档实现时返回 `false`。
    pub fn save_data(&mut self, key: &str, value: &str) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => {
                let Some(store) = world.get_resource_mut::<prism_engine::user_data::UserStore>()
                else {
                    log::warn!(
                        "save_data: no UserStore resource; the host did not open a save file"
                    );
                    return false;
                };
                store.set(key, value);
                match store.flush() {
                    Ok(_) => true,
                    Err(error) => {
                        log::warn!("save_data {key}: {error}");
                        false
                    }
                }
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.save_value(key, value.as_bytes()),
        }
    }

    /// 读一条玩家存档；没有这个键 = `None`（不是错误）。
    pub fn load_data(&self, key: &str) -> Option<String> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world
                .get_resource::<prism_engine::user_data::UserStore>()
                .and_then(|store| store.get(key))
                .map(str::to_string),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend
                .load_value(key)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        }
    }

    /// Reads a global persistent gameplay flag. Missing flags are false.
    ///
    /// Flags use an engine-owned storage namespace so doors, switches and NPCs
    /// can share state without agreeing on a serialized value format.
    pub fn get_flag(&self, key: &str) -> bool {
        if !valid_flag_key(key) {
            return false;
        }
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world
                .get_resource::<prism_engine::user_data::UserStore>()
                .is_some_and(|store| store.get_flag(key)),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.get_flag(key),
        }
    }

    /// Sets a global persistent gameplay flag and flushes it immediately.
    pub fn set_flag(&mut self, key: &str, value: bool) -> bool {
        if !valid_flag_key(key) {
            return false;
        }
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => {
                let Some(store) = world.get_resource_mut::<prism_engine::user_data::UserStore>()
                else {
                    return false;
                };
                store.set_flag(key, value);
                store.flush().is_ok() && !store.is_read_only()
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.set_flag(key, value),
        }
    }

    /// Requests a scene switch (#482): `additive = true` keeps the old scene
    /// (`LoadSceneMode::Additive`).
    ///
    /// Returning `true` only means the **request reached the host**; the actual
    /// switch happens after this tick ends, and the next frame's
    /// [`Self::scene_generation`] is incremented. All old entity handles become
    /// invalid.
    pub fn load_scene(&mut self, path: &str, additive: bool) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => {
                // The host backend cannot switch scenes itself; the runtime
                // host (HOST_CAP_SCENE) executes the request after the tick.
                let _ = (path, additive);
                log::warn!(
                    "load_scene({path:?}, additive = {additive}) is only available through the runtime host (HOST_CAP_SCENE)"
                );
                false
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.load_scene(path, additive),
        }
    }

    /// Requests unloading the current scene (a `Persistent` subtree is kept); also
    /// deferred.
    pub fn unload_scene(&mut self) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => {
                log::warn!(
                    "unload_scene is only available through the runtime host (HOST_CAP_SCENE)"
                );
                false
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.unload_scene(),
        }
    }

    /// Plays a one-shot sound (#987): `path` is relative to `assets/`, `volume` is a
    /// linear gain in `0..1`.
    ///
    /// Returns the host-assigned sound id (usable with [`Self::stop_sound`]); `None`
    /// means the clip is missing, decoding failed, or the backend has no audio.
    ///
    /// - the host backend queues the request into
    ///   `prism_engine::sound::SoundCommands`; `App` plays it after the tick ends;
    /// - the plugin backend goes through `HostApi::play_sound` (capability bit
    ///   `HOST_CAP_AUDIO`); the editor returns `None` when no audio implementation
    ///   was injected instead of silently swallowing the call.
    pub fn play_sound(&mut self, path: &str, volume: f32, repeat: bool) -> Option<u32> {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(backend) => {
                // `Engine::empty` inserts this resource; add one for custom hosts.
                if backend
                    .get_resource::<prism_engine::sound::SoundCommands>()
                    .is_none()
                {
                    backend.insert_resource(prism_engine::sound::SoundCommands::default());
                }
                let commands = backend
                    .get_resource_mut::<prism_engine::sound::SoundCommands>()
                    .expect("SoundCommands was just inserted");
                Some(commands.play(path, volume, repeat))
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.play_sound(path, volume, repeat),
        }
    }

    /// Stops the sound id returned by [`Self::play_sound`]; unknown or finished ids
    /// are a no-op.
    pub fn stop_sound(&mut self, id: u32) {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(backend) => {
                if let Some(commands) =
                    backend.get_resource_mut::<prism_engine::sound::SoundCommands>()
                {
                    commands.stop(id);
                }
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.stop_sound(id),
        }
    }

    /// Scene generation: incremented by the host on every scene switch / unload.
    /// Project code uses it to tell whether cached entity handles are stale.
    ///
    /// The static Player (host backend) never switches scenes, so it is always `0`.
    pub fn scene_generation(&self) -> u64 {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => 0,
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.scene_generation(),
        }
    }

    /// Raycast query (#522): returns the closest hit.
    ///
    /// - The host backend calls `prism_engine::physics::raycast` directly (the same
    ///   geometry used by editor picking and gameplay queries);
    /// - the plugin backend goes through `HostApi::raycast`; returns `None` when the
    ///   host has not injected an implementation (no `HOST_CAP_PHYSICS`).
    ///
    /// A collider takes part only when `layer_mask` AND its `layer` is non-zero;
    /// hits beyond `max_distance` do not count.
    pub fn raycast(
        &self,
        origin: [f32; 3],
        direction: [f32; 3],
        max_distance: f32,
        layer_mask: u32,
    ) -> Option<RaycastHit> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => {
                prism_engine::physics::raycast(world, origin, direction, max_distance, layer_mask)
                    .map(|hit| RaycastHit {
                        entity: handle_from_entity(hit.entity),
                        distance: hit.distance,
                        point: hit.point,
                        normal: hit.normal,
                        is_trigger: hit.is_trigger,
                    })
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => {
                if backend.api.capabilities & HOST_CAP_PHYSICS == 0 {
                    return None;
                }
                let raycast = backend.api.raycast?;
                let mut raw = prism_project_api::RaycastHit::default();
                let code = unsafe {
                    raycast(
                        backend.api.user_data,
                        &origin,
                        &direction,
                        max_distance,
                        layer_mask,
                        &mut raw,
                    )
                };
                (code == 1).then_some(RaycastHit {
                    entity: raw.entity,
                    distance: raw.distance,
                    point: raw.point,
                    normal: raw.normal,
                    is_trigger: raw.is_trigger != 0,
                })
            }
        }
    }

    /// Returns active 2D colliders whose layers intersect `layer_mask`.
    /// Colliders use the entity's complete `LocalToWorld` matrix, falling back to
    /// the local transform for a freshly spawned root before hierarchy propagation.
    pub fn colliders2d(&self, layer_mask: u32) -> Vec<ColliderInstance2D> {
        let mut colliders: Vec<_> = match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => prism_scene::colliders2d(world, layer_mask)
                .into_iter()
                .map(|collider| ColliderInstance2D {
                    entity: handle_from_entity(collider.entity),
                    bounds: collider.bounds,
                    is_trigger: collider.is_trigger,
                    layer: collider.layer,
                    one_way_top: collider.one_way_top,
                })
                .collect(),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.colliders2d(layer_mask),
        };
        colliders.sort_by_key(|collider| (collider.entity.id, collider.entity.generation));
        colliders
    }

    /// Returns the latest fixed-step 2D trigger events.
    ///
    /// Events are a snapshot, not a queue: the engine replaces it after each
    /// fixed step, so gameplay should consume the phases once per step.
    pub fn trigger_events2d(&self) -> Vec<TriggerEvent2D> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world
                .get_resource::<prism_engine::physics::TriggerEvents2D>()
                .map(|events| {
                    events
                        .events()
                        .iter()
                        .filter_map(|event| {
                            Some(TriggerEvent2D {
                                entity_a: event.entity_a,
                                entity_b: event.entity_b,
                                phase: event.phase()?,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.trigger_events2d(),
        }
    }

    /// Returns colliders overlapping `bounds`, including triggers.
    pub fn overlap2d(&self, bounds: Aabb2, layer_mask: u32) -> Vec<ColliderInstance2D> {
        self.colliders2d(layer_mask)
            .into_iter()
            .filter(|collider| bounds.overlaps(collider.bounds))
            .collect()
    }

    /// Returns enabled interaction targets within `max_distance` of `origin`.
    ///
    /// Candidates require a `Collider2D` or `TilemapCollider2D` on the same
    /// entity. Results sort by descending priority, then ascending distance,
    /// then entity handle, so target selection is stable across backends.
    pub fn interactables2d(
        &self,
        origin: [f32; 2],
        max_distance: f32,
        layer_mask: u32,
    ) -> Vec<InteractableCandidate2D> {
        if !origin.into_iter().all(f32::is_finite)
            || !max_distance.is_finite()
            || max_distance < 0.0
        {
            return Vec::new();
        }

        let interactables: std::collections::HashMap<_, _> = self
            .query::<Interactable2D>()
            .into_iter()
            .filter_map(|entity| {
                self.get::<Interactable2D>(entity)
                    .filter(|interactable| interactable.enabled)
                    .map(|interactable| (entity, interactable))
            })
            .collect();

        let mut by_entity = std::collections::HashMap::new();
        for collider in self.colliders2d(layer_mask) {
            let Some(interactable) = interactables.get(&collider.entity) else {
                continue;
            };
            let nearest = [
                origin[0].clamp(collider.bounds.min[0], collider.bounds.max[0]),
                origin[1].clamp(collider.bounds.min[1], collider.bounds.max[1]),
            ];
            let dx = origin[0] - nearest[0];
            let dy = origin[1] - nearest[1];
            let distance = dx.hypot(dy);
            if distance > max_distance {
                continue;
            }
            let candidate = InteractableCandidate2D {
                entity: collider.entity,
                bounds: collider.bounds,
                prompt: interactable.prompt.clone(),
                priority: interactable.priority,
                distance,
            };
            by_entity
                .entry(collider.entity)
                .and_modify(|current: &mut InteractableCandidate2D| {
                    if candidate.distance < current.distance {
                        *current = candidate.clone();
                    }
                })
                .or_insert(candidate);
        }
        let mut candidates: Vec<_> = by_entity.into_values().collect();
        candidates.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.distance.total_cmp(&right.distance))
                .then_with(|| {
                    (left.entity.id, left.entity.generation)
                        .cmp(&(right.entity.id, right.entity.generation))
                })
        });
        candidates
    }

    /// Returns enabled hitbox/hurtbox overlaps for the current world state.
    ///
    /// Both entities must carry the corresponding metadata component and an
    /// active 2D collider. Same-team pairs are ignored, and multiple collider
    /// fragments on either entity produce one stable contact pair. Damage is a
    /// snapshot; gameplay owns hit cooldowns, invulnerability and responses.
    pub fn damage_contacts2d(
        &self,
        hitbox_layer_mask: u32,
        hurtbox_layer_mask: u32,
    ) -> Vec<DamageContact2D> {
        let hitboxes: std::collections::HashMap<_, _> = self
            .query::<Hitbox2D>()
            .into_iter()
            .filter_map(|entity| {
                self.get::<Hitbox2D>(entity)
                    .filter(|hitbox| hitbox.enabled)
                    .map(|hitbox| (entity, hitbox))
            })
            .collect();
        let hurtboxes: std::collections::HashMap<_, _> = self
            .query::<Hurtbox2D>()
            .into_iter()
            .filter_map(|entity| {
                self.get::<Hurtbox2D>(entity)
                    .filter(|hurtbox| hurtbox.enabled)
                    .map(|hurtbox| (entity, hurtbox))
            })
            .collect();
        if hitboxes.is_empty() || hurtboxes.is_empty() {
            return Vec::new();
        }

        let hitbox_colliders: Vec<_> = self
            .colliders2d(hitbox_layer_mask)
            .into_iter()
            .filter(|collider| hitboxes.contains_key(&collider.entity))
            .collect();
        let hurtbox_colliders: Vec<_> = self
            .colliders2d(hurtbox_layer_mask)
            .into_iter()
            .filter(|collider| hurtboxes.contains_key(&collider.entity))
            .collect();
        let mut contacts = std::collections::HashSet::new();
        let mut out = Vec::new();
        for hitbox in &hitbox_colliders {
            let Some(hitbox_data) = hitboxes.get(&hitbox.entity) else {
                continue;
            };
            for hurtbox in &hurtbox_colliders {
                if hitbox.entity == hurtbox.entity
                    || !hitbox.bounds.overlaps(hurtbox.bounds)
                    || !contacts.insert((hitbox.entity, hurtbox.entity))
                {
                    continue;
                }
                let Some(hurtbox_data) = hurtboxes.get(&hurtbox.entity) else {
                    continue;
                };
                if hitbox_data.team == hurtbox_data.team {
                    contacts.remove(&(hitbox.entity, hurtbox.entity));
                    continue;
                }
                out.push(DamageContact2D {
                    hitbox: hitbox.entity,
                    hurtbox: hurtbox.entity,
                    damage: hitbox_data.damage,
                    hitbox_team: hitbox_data.team,
                    hurtbox_team: hurtbox_data.team,
                });
            }
        }
        out.sort_by_key(|contact| {
            (
                contact.hitbox.id,
                contact.hitbox.generation,
                contact.hurtbox.id,
                contact.hurtbox.generation,
            )
        });
        out
    }

    /// Casts a world-space ray against active 2D AABB colliders and returns the
    /// nearest hit. Triggers participate in the query; movement APIs still
    /// exclude them as obstacles.
    pub fn raycast2d(
        &self,
        origin: [f32; 2],
        direction: [f32; 2],
        max_distance: f32,
        layer_mask: u32,
    ) -> Option<RaycastHit2D> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => self
                .colliders2d(layer_mask)
                .into_iter()
                .filter_map(|collider| {
                    prism_physics2d::raycast_aabb(origin, direction, max_distance, collider.bounds)
                        .map(|hit| RaycastHit2D {
                            entity: collider.entity,
                            distance: hit.distance,
                            point: hit.point,
                            normal: hit.normal,
                            is_trigger: collider.is_trigger,
                            layer: collider.layer,
                        })
                })
                .min_by(|left, right| left.distance.total_cmp(&right.distance)),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => {
                if backend.api.capabilities & HOST_CAP_PHYSICS_2D == 0 {
                    return None;
                }
                let raycast = backend.api.raycast2d?;
                let mut raw = AbiRaycastHit2D::default();
                let status = unsafe {
                    raycast(
                        backend.api.user_data,
                        &origin,
                        &direction,
                        max_distance,
                        layer_mask,
                        &mut raw,
                    )
                };
                (status == 1).then_some(RaycastHit2D {
                    entity: raw.entity,
                    distance: raw.distance,
                    point: raw.point,
                    normal: raw.normal,
                    is_trigger: raw.is_trigger != 0,
                    layer: raw.layer,
                })
            }
        }
    }

    /// Sweeps an AABB against active, non-trigger 2D colliders in the world.
    pub fn move_and_slide2d(
        &self,
        entity: Option<EntityHandle>,
        center: [f32; 2],
        half_extents: [f32; 2],
        delta: [f32; 2],
        layer_mask: u32,
    ) -> MoveResult2 {
        self.move_and_slide2d_with_options(
            entity,
            center,
            half_extents,
            delta,
            layer_mask,
            MoveOptions2D::default(),
        )
    }

    /// Sweeps an AABB with platformer-specific one-way platform options.
    pub fn move_and_slide2d_with_options(
        &self,
        entity: Option<EntityHandle>,
        center: [f32; 2],
        half_extents: [f32; 2],
        delta: [f32; 2],
        layer_mask: u32,
        options: MoveOptions2D,
    ) -> MoveResult2 {
        let obstacles: Vec<_> = self
            .colliders2d(layer_mask)
            .into_iter()
            .filter(|collider| !collider.is_trigger && Some(collider.entity) != entity)
            .map(|collider| Obstacle2D {
                bounds: collider.bounds,
                one_way_top: collider.one_way_top,
            })
            .collect();
        prism_physics2d::move_and_slide_with_options(
            center,
            half_extents,
            delta,
            &obstacles,
            options,
        )
    }

    /// Reads a loose text asset.
    pub fn read_asset_text(&self, relative: &str) -> std::io::Result<String> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => prism_engine::asset_paths::read_asset_text(relative),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.read_asset_text(relative),
        }
    }

    pub fn spawn(&mut self) -> EntityHandle {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => handle_from_entity(world.spawn()),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.spawn(),
        }
    }

    pub fn despawn(&mut self, entity: EntityHandle) {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world.despawn(entity_from_handle(entity)),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.despawn(entity),
        }
    }

    pub fn is_alive(&self, entity: EntityHandle) -> bool {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world.is_alive(entity_from_handle(entity)),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.is_alive(entity),
        }
    }

    /// All entities that have component `T` (same `Active` semantics as the host's
    /// `World::query`).
    pub fn query<T: Component + Reflect>(&self) -> Vec<EntityHandle> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world
                .query::<T>()
                .map(|(entity, _)| handle_from_entity(entity))
                .collect(),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.query(T::type_desc().name),
        }
    }

    /// First entity whose [`Name`] equals `name`.
    ///
    /// Both backends implement this through component reads, so no extra host
    /// callback is needed and a plugin and the static player resolve the same
    /// entity. Names are not required to be unique: when several entities share
    /// one, the result is the first one in ECS iteration order, which is **not**
    /// a stable authoring order — keep scene names unique.
    pub fn find_by_name(&self, name: &str) -> Option<EntityHandle> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world
                .query::<Name>()
                .find(|(_, value)| value.0 == name)
                .map(|(entity, _)| handle_from_entity(entity)),
            #[cfg(feature = "plugin")]
            Backend::Plugin(_) => self.query::<Name>().into_iter().find(|&entity| {
                self.get::<Name>(entity)
                    .is_some_and(|value| value.0 == name)
            }),
        }
    }

    pub fn get<T: Component + Reflect + Clone>(&self, entity: EntityHandle) -> Option<T> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world.get::<T>(entity_from_handle(entity)).cloned(),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => {
                let bytes = backend.get(entity, T::type_desc().name)?;
                let decoded = rtti_bin::decode(&bytes).ok()?;
                T::from_object(decoded.objects.first()?).ok()
            }
        }
    }

    /// Writes a component (inserting it if absent). Returns `false` when the handle
    /// is dead.
    pub fn set<T: Component + Reflect>(&mut self, entity: EntityHandle, value: T) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => {
                let entity = entity_from_handle(entity);
                if !world.is_alive(entity) {
                    return false;
                }
                world.insert(entity, value);
                true
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => {
                let object = value.to_object();
                match rtti_bin::encode_one(&object, backend.registry) {
                    Ok(bytes) => backend.set(entity, &bytes),
                    Err(error) => {
                        log::warn!("game-api: encode `{}` failed: {error}", T::type_desc().name);
                        false
                    }
                }
            }
        }
    }

    /// Enable render interpolation for a fixed-step 2D entity.
    ///
    /// The component stores the entity's current local XY position. Call this
    /// after assigning its initial transform and before its first fixed step.
    pub fn enable_transform_interpolation2d(&mut self, entity: EntityHandle) -> bool {
        let Some(transform) = self.get::<LocalTransform>(entity) else {
            return false;
        };
        self.set(
            entity,
            TransformInterpolation2D {
                previous: [transform.position.x, transform.position.y],
                current: [transform.position.x, transform.position.y],
            },
        )
    }

    /// Reset interpolation history after teleporting, respawning, or changing rooms.
    pub fn snap_transform_interpolation2d(&mut self, entity: EntityHandle) -> bool {
        let Some(transform) = self.get::<LocalTransform>(entity) else {
            return false;
        };
        let position = [transform.position.x, transform.position.y];
        let Some(mut history) = self.get::<TransformInterpolation2D>(entity) else {
            return false;
        };
        history.previous = position;
        history.current = position;
        self.set(entity, history)
    }

    /// Removes a component, returning whether it existed.
    pub fn remove<T: Component + Reflect>(&mut self, entity: EntityHandle) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world.remove::<T>(entity_from_handle(entity)).is_some(),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.remove(entity, T::type_desc().name),
        }
    }

    /// Submits a UI quad for this frame's UI draw list.
    ///
    /// The host backend queues it in the world's [`ExternalUiQuads`] resource, so the
    /// static Player renders it even though the `GameWorld` itself is dropped at the
    /// end of the tick. The plugin backend forwards it through `HostApi::draw_quad`.
    ///
    /// [`ExternalUiQuads`]: prism_engine::ui::ExternalUiQuads
    pub fn draw_quad(&mut self, quad: UiQuad) {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => {
                let mut external = world
                    .remove_resource::<prism_engine::ui::ExternalUiQuads>()
                    .unwrap_or_default();
                external.0.push(prism_engine::ui::UiQuad {
                    rect: quad.rect,
                    color: quad.color,
                    border_radius: quad.border_radius,
                    // Project quads are solid colour only (no bindless texture slot).
                    texture: None,
                    layer: quad.layer,
                    order: prism_engine::ui::EXTERNAL_UI_ORDER,
                });
                world.insert_resource(external);
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.draw(quad),
        }
    }
}

#[cfg(feature = "plugin")]
struct PluginBackend<'a> {
    api: &'a HostApi,
    registry: &'a TypeRegistry,
}

/// The forwarding target for `log!` records in a module (the host-provided
/// `HostApi::log`).
#[cfg(feature = "plugin")]
#[derive(Clone, Copy)]
struct LogSink {
    user_data: usize,
    log: prism_project_api::LogMessage,
}

#[cfg(feature = "plugin")]
static LOG_SINK: std::sync::OnceLock<LogSink> = std::sync::OnceLock::new();

#[cfg(feature = "plugin")]
struct HostLogger;

/// `log::set_logger` requires a `&'static`: the workspace's `log` has the `alloc`
/// feature disabled, so use a static instance instead of `set_boxed_logger`.
#[cfg(feature = "plugin")]
static HOST_LOGGER: HostLogger = HostLogger;

#[cfg(feature = "plugin")]
impl log::Log for HostLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        LOG_SINK.get().is_some()
    }

    fn log(&self, record: &log::Record<'_>) {
        if LOG_SINK.get().is_none() {
            return;
        }
        // Kept in sync with `prism-runtime-host::host_log`: 0 error / 1 warn /
        // 2 debug / 3 info.
        let level = match record.level() {
            log::Level::Error => 0,
            log::Level::Warn => 1,
            log::Level::Debug => 2,
            log::Level::Info | log::Level::Trace => 3,
        };
        let text = format!("{}: {}", record.target(), record.args());
        forward_log(level, &text);
    }

    fn flush(&self) {}
}

/// Sends **panics** from the project module (with their backtrace) to the host log.
///
/// A module is its own dynamic library with its own copy of `std`: the hook the host
/// installs through `set_hook` never sees panics inside the module, and the default
/// behavior only prints one line to stderr (the editor Console stays empty, and a
/// panic every frame floods stderr). This installs a hook on the module's own `std`
/// and sends `panicked at …` plus the `Backtrace` through the same path as
/// [`install_host_logger`].
///
/// Resolving project frames in the backtrace needs the `.pdb`: when the editor copies
/// the module into `staged/` it copies the `.pdb` along with it (see
/// `prism-editor-egui::project_module::stage_module`).
///
/// Like [`install_host_logger`], repeated calls are a no-op. It must be called in
/// `create`, and after [`install_host_logger`] (otherwise the log has no outlet yet).
#[cfg(feature = "plugin")]
pub fn install_host_panic_hook() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // `Backtrace::force_capture` does not depend on `RUST_BACKTRACE`; a stack
            // is wanted by default while playing.
            let text = format!(
                "{info}
{}",
                std::backtrace::Backtrace::force_capture()
            );
            forward_log(0, &text);
            // Keep the original behavior (stderr) so terminal debugging still works.
            previous(info);
        }));
    });
}

/// Sends one line of text to the host using the ABI's level convention
/// (0 error / 1 warn / 2 debug / 3 info).
#[cfg(feature = "plugin")]
fn forward_log(level: u32, text: &str) {
    let Some(sink) = LOG_SINK.get() else {
        return;
    };
    // SAFETY: as in `HostLogger::log`; the host log callback does not use
    // `user_data`, and the string is only valid for this call.
    unsafe {
        (sink.log)(
            sink.user_data as *mut core::ffi::c_void,
            level,
            text.as_ptr(),
            text.len(),
        );
    }
}

/// Wires the project module's own `log!` records to the host's `HostApi::log`.
///
/// A module is its own dynamic library that links **its own copy** of the `log`
/// crate — the logger the host installs in the editor process never sees records
/// from the module. A project just calls this once in `create`, and `log::info!` /
/// `log::error!` are sent through the ABI into the editor Console (the same path as
/// host logs, labeled with source `Game`).
///
/// Repeated calls are a no-op (only the first `log` callback is taken). `user_data`
/// is refreshed on every call because the host swaps in a new command context each
/// tick; but the host's logging callback (`prism-runtime-host`) does not depend on
/// it.
#[cfg(feature = "plugin")]
pub fn install_host_logger(host: &HostApi) {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    let Some(log) = host.log else {
        return;
    };
    let sink = LogSink {
        user_data: host.user_data as usize,
        log,
    };
    // Install the logger the first time; afterwards just refresh the sink
    // (`OnceLock::set` failing means it is already there).
    let fresh = LOG_SINK.set(sink).is_ok();
    if fresh {
        INSTALL.call_once(|| {
            if log::set_logger(&HOST_LOGGER).is_ok() {
                log::set_max_level(log::LevelFilter::Debug);
            }
        });
    }
}

#[cfg(feature = "plugin")]
impl PluginBackend<'_> {
    /// 一次性音效（#987）：没有 `HOST_CAP_AUDIO` 时返回 `None`。
    fn play_sound(&self, path: &str, volume: f32, repeat: bool) -> Option<u32> {
        if self.api.capabilities & prism_project_api::HOST_CAP_AUDIO == 0 {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                log::warn!("host does not expose one-shot audio (HOST_CAP_AUDIO)");
            });
            return None;
        }
        let play = self.api.play_sound?;
        let mut id = 0u32;
        let status = unsafe {
            play(
                self.api.user_data,
                path.as_ptr(),
                path.len(),
                volume,
                u8::from(repeat),
                &mut id,
            )
        };
        (status == SUCCESS && id != 0).then_some(id)
    }

    fn stop_sound(&self, id: u32) {
        if self.api.capabilities & prism_project_api::HOST_CAP_AUDIO == 0 {
            return;
        }
        if let Some(stop) = self.api.stop_sound {
            unsafe { stop(self.api.user_data, id) };
        }
    }

    fn spawn(&self) -> EntityHandle {
        let Some(spawn) = self.api.spawn_entity else {
            return EntityHandle::PLACEHOLDER;
        };
        let mut out = EntityHandle::PLACEHOLDER;
        let status = unsafe { spawn(self.api.user_data, &mut out) };
        if status == SUCCESS {
            out
        } else {
            EntityHandle::PLACEHOLDER
        }
    }

    fn despawn(&self, entity: EntityHandle) {
        let Some(despawn) = self.api.despawn_entity else {
            log::debug!("game-api: host does not expose despawn_entity");
            return;
        };
        let status = unsafe { despawn(self.api.user_data, entity) };
        if status != SUCCESS {
            log::debug!("game-api: despawn rejected by host (status {status})");
        }
    }

    fn is_alive(&self, entity: EntityHandle) -> bool {
        match self.api.is_alive {
            Some(is_alive) => unsafe { is_alive(self.api.user_data, entity) == 1 },
            // Fall back to "handle is non-empty" when the host provides no liveness
            // query; reads and writes still validate separately.
            None => !entity.is_placeholder(),
        }
    }

    /// 写入玩家存档（#984）；宿主没有能力位时只警告一次。
    fn save_value(&self, key: &str, value: &[u8]) -> bool {
        if self.api.capabilities & prism_project_api::HOST_CAP_SAVE == 0 {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                log::warn!("host does not expose player saves (HOST_CAP_SAVE)");
            });
            return false;
        }
        let Some(save) = self.api.save_value else {
            return false;
        };
        let key = key.as_bytes();
        let status = unsafe {
            save(
                self.api.user_data,
                key.as_ptr(),
                key.len(),
                value.as_ptr(),
                value.len(),
            )
        };
        status == SUCCESS
    }

    /// 读玩家存档（#984）：先问长度再取内容，和宿主的两段读取一致。
    fn load_value(&self, key: &str) -> Option<Vec<u8>> {
        if self.api.capabilities & prism_project_api::HOST_CAP_SAVE == 0 {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                log::warn!("host does not expose player saves (HOST_CAP_SAVE)");
            });
            return None;
        }
        let load = self.api.load_value?;
        let key = key.as_bytes();
        let len = unsafe {
            load(
                self.api.user_data,
                key.as_ptr(),
                key.len(),
                std::ptr::null_mut(),
                0,
            )
        };
        if len < 0 {
            return None;
        }
        let mut out = vec![0u8; len as usize];
        let status = unsafe {
            load(
                self.api.user_data,
                key.as_ptr(),
                key.len(),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        if status < 0 {
            return None;
        }
        out.truncate(status as usize);
        Some(out)
    }

    fn get_flag(&self, key: &str) -> bool {
        if self.api.capabilities & prism_project_api::HOST_CAP_FLAGS == 0 {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| log::warn!("host does not expose persistent flags"));
            return false;
        }
        let Some(get) = self.api.get_flag else {
            return false;
        };
        let name = key.as_bytes();
        unsafe { get(self.api.user_data, name.as_ptr(), name.len()) == 1 }
    }

    fn set_flag(&self, key: &str, value: bool) -> bool {
        if self.api.capabilities & prism_project_api::HOST_CAP_FLAGS == 0 {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| log::warn!("host does not expose persistent flags"));
            return false;
        }
        let Some(set) = self.api.set_flag else {
            return false;
        };
        let name = key.as_bytes();
        unsafe {
            set(
                self.api.user_data,
                name.as_ptr(),
                name.len(),
                u8::from(value),
            ) == SUCCESS
        }
    }

    fn colliders2d(&self, layer_mask: u32) -> Vec<ColliderInstance2D> {
        if self.api.capabilities & HOST_CAP_PHYSICS_2D == 0 {
            return Vec::new();
        }
        let Some(query) = self.api.query_colliders2d else {
            return Vec::new();
        };
        let mut count = 0usize;
        let status = unsafe {
            query(
                self.api.user_data,
                layer_mask,
                std::ptr::null_mut(),
                0,
                &mut count,
            )
        };
        if status != SUCCESS || count == 0 {
            return Vec::new();
        }
        let mut raw = vec![Collider2DHit::default(); count];
        let status = unsafe {
            query(
                self.api.user_data,
                layer_mask,
                raw.as_mut_ptr(),
                raw.len(),
                &mut count,
            )
        };
        if status != SUCCESS {
            return Vec::new();
        }
        raw.truncate(count);
        raw.into_iter()
            .map(|hit| ColliderInstance2D {
                entity: hit.entity,
                bounds: Aabb2::new(hit.bounds[0], hit.bounds[1], hit.bounds[2], hit.bounds[3]),
                is_trigger: hit.is_trigger != 0,
                layer: hit.layer,
                one_way_top: hit.one_way_top != 0,
            })
            .collect()
    }

    fn trigger_events2d(&self) -> Vec<TriggerEvent2D> {
        if self.api.capabilities & HOST_CAP_PHYSICS_EVENTS_2D == 0 {
            return Vec::new();
        }
        let Some(query) = self.api.query_trigger_events2d else {
            return Vec::new();
        };
        let mut count = 0usize;
        let status = unsafe { query(self.api.user_data, std::ptr::null_mut(), 0, &mut count) };
        if status != SUCCESS || count == 0 {
            return Vec::new();
        }
        let mut raw = vec![AbiPhysicsEvent2D::default(); count];
        let status = unsafe { query(self.api.user_data, raw.as_mut_ptr(), raw.len(), &mut count) };
        if status != SUCCESS {
            return Vec::new();
        }
        raw.truncate(count);
        raw.into_iter()
            .filter_map(|event| {
                Some(TriggerEvent2D {
                    entity_a: event.entity_a,
                    entity_b: event.entity_b,
                    phase: event.phase()?,
                })
            })
            .collect()
    }

    fn draw(&self, quad: UiQuad) {
        let Some(draw) = self.api.draw_quad else {
            log::debug!("game-api: host does not expose draw_quad");
            return;
        };
        unsafe { draw(self.api.user_data, quad) };
    }

    fn query(&self, type_name: &str) -> Vec<EntityHandle> {
        let Some(query) = self.api.query_entities else {
            return Vec::new();
        };
        let name = type_name.as_bytes();
        let mut count = 0usize;
        let status = unsafe {
            query(
                self.api.user_data,
                name.as_ptr(),
                name.len(),
                std::ptr::null_mut(),
                0,
                &mut count,
            )
        };
        if status != SUCCESS || count == 0 {
            return Vec::new();
        }

        let mut out = vec![EntityHandle::default(); count];
        let mut written = 0usize;
        let status = unsafe {
            query(
                self.api.user_data,
                name.as_ptr(),
                name.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        if status != SUCCESS {
            return Vec::new();
        }
        out.truncate(written);
        out
    }

    fn get(&self, entity: EntityHandle, type_name: &str) -> Option<Vec<u8>> {
        let get = self.api.get_component?;
        let name = type_name.as_bytes();
        let mut ptr: *const u8 = std::ptr::null();
        let mut len = 0usize;
        let status = unsafe {
            get(
                self.api.user_data,
                entity,
                name.as_ptr(),
                name.len(),
                &mut ptr,
                &mut len,
            )
        };
        if status != SUCCESS || ptr.is_null() {
            return None;
        }
        // The pointer is only valid until the next host call: copy it out
        // immediately.
        Some(unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec())
    }

    fn set(&self, entity: EntityHandle, bytes: &[u8]) -> bool {
        let Some(set) = self.api.set_component else {
            return false;
        };
        let status = unsafe { set(self.api.user_data, entity, bytes.as_ptr(), bytes.len()) };
        status == SUCCESS
    }

    fn remove(&self, entity: EntityHandle, type_name: &str) -> bool {
        let Some(remove) = self.api.remove_component else {
            return false;
        };
        let name = type_name.as_bytes();
        let status = unsafe { remove(self.api.user_data, entity, name.as_ptr(), name.len()) };
        status == SUCCESS
    }

    /// Goes through the host's `HOST_CAP_ASSET`: the project does not assemble
    /// `assets/` paths itself.
    fn asset_exists(&self, relative: &str) -> bool {
        let Some(callback) = self.api.asset_exists else {
            return false;
        };
        unsafe { callback(self.api.user_data, relative.as_ptr(), relative.len()) == 1 }
    }

    fn save_scene_state(&self) -> bool {
        let Some(callback) = self.api.save_scene_state else {
            log::warn!("host does not expose scene-state saving (HOST_CAP_STATE)");
            return false;
        };
        unsafe { callback(self.api.user_data) == SUCCESS }
    }

    fn load_scene_state(&self) -> bool {
        let Some(callback) = self.api.load_scene_state else {
            log::warn!("host does not expose scene-state loading (HOST_CAP_STATE)");
            return false;
        };
        unsafe { callback(self.api.user_data) == 1 }
    }

    fn load_scene(&self, path: &str, additive: bool) -> bool {
        if self.api.capabilities & HOST_CAP_SCENE == 0 {
            log::warn!("host does not expose scene switching (HOST_CAP_SCENE)");
            return false;
        }
        let Some(callback) = self.api.load_scene else {
            log::warn!("host does not expose scene switching (HOST_CAP_SCENE)");
            return false;
        };
        unsafe {
            callback(
                self.api.user_data,
                path.as_ptr(),
                path.len(),
                u8::from(additive),
            ) == SUCCESS
        }
    }

    fn unload_scene(&self) -> bool {
        if self.api.capabilities & HOST_CAP_SCENE == 0 {
            log::warn!("host does not expose scene switching (HOST_CAP_SCENE)");
            return false;
        }
        let Some(callback) = self.api.unload_scene else {
            log::warn!("host does not expose scene switching (HOST_CAP_SCENE)");
            return false;
        };
        unsafe { callback(self.api.user_data) == SUCCESS }
    }

    fn scene_generation(&self) -> u64 {
        self.api.scene_generation
    }

    fn read_asset_text(&self, relative: &str) -> std::io::Result<String> {
        let Some(callback) = self.api.read_asset_file else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "host does not expose loose asset access",
            ));
        };
        let mut ptr: *const u8 = std::ptr::null();
        let mut len = 0usize;
        let status = unsafe {
            callback(
                self.api.user_data,
                relative.as_ptr(),
                relative.len(),
                &mut ptr,
                &mut len,
            )
        };
        if status != SUCCESS || (ptr.is_null() && len != 0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("asset `{relative}` not found"),
            ));
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        String::from_utf8(bytes.to_vec())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "host")]
    #[test]
    fn host_global_flags_are_persistent_user_store_entries() {
        let mut world = World::new();
        world.insert_resource(prism_engine::user_data::UserStore::in_memory());
        {
            let mut api = GameWorld::host(&mut world);
            assert!(!api.get_flag("well.gate.open"));
            assert!(api.set_flag("well.gate.open", true));
            assert!(api.get_flag("well.gate.open"));
            assert!(!api.set_flag("", true));
        }
        let api = GameWorld::host(&mut world);
        assert!(api.get_flag("well.gate.open"));
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_global_flags_use_the_host_capability() {
        #[derive(Default)]
        struct FakeSave(std::collections::BTreeMap<String, Vec<u8>>);
        impl prism_runtime_host::SaveIo for FakeSave {
            fn save_value(&mut self, key: &str, value: &[u8]) -> bool {
                self.0.insert(key.to_string(), value.to_vec());
                true
            }

            fn load_value(&mut self, key: &str) -> Option<Vec<u8>> {
                self.0.get(key).cloned()
            }
        }

        let mut world = World::new();
        let registry = prism_scene::builtin_registry();
        let mut save = FakeSave::default();
        let mut commands =
            prism_runtime_host::WorldCommands::new(&mut world, registry, 1).with_save(&mut save);
        {
            let host = commands.api();
            assert_ne!(host.capabilities & prism_project_api::HOST_CAP_FLAGS, 0);
            let mut api = GameWorld::plugin(&host, registry.type_registry());
            assert!(!api.get_flag("well.gate.open"));
            assert!(api.set_flag("well.gate.open", true));
            assert!(api.get_flag("well.gate.open"));
        }
        assert_eq!(
            save.0.get("__prism.flag.well.gate.open"),
            Some(&b"1".to_vec())
        );
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_reads_trigger_events_through_the_host_api() {
        fn trigger_reader(
            _world: &prism_ecs::World,
            out: &mut Vec<prism_project_api::PhysicsEvent2D>,
        ) {
            out.push(prism_project_api::PhysicsEvent2D::new(
                EntityHandle {
                    id: 4,
                    generation: 2,
                },
                EntityHandle {
                    id: 9,
                    generation: 1,
                },
                TriggerPhase2D::Enter,
            ));
        }

        let mut world = prism_ecs::World::new();
        let registry = prism_scene::builtin_registry();
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, registry, 1)
            .with_trigger_events2d(trigger_reader);
        let host = commands.api();
        assert_ne!(
            host.capabilities & prism_project_api::HOST_CAP_PHYSICS_EVENTS_2D,
            0
        );
        let api = GameWorld::plugin(&host, registry.type_registry());
        assert_eq!(
            api.trigger_events2d(),
            vec![TriggerEvent2D {
                entity_a: EntityHandle {
                    id: 4,
                    generation: 2,
                },
                entity_b: EntityHandle {
                    id: 9,
                    generation: 1,
                },
                phase: TriggerPhase2D::Enter,
            }]
        );
    }

    #[test]
    fn input_edges_accumulate_until_consumed_without_staling_held_state() {
        let mut edges = InputEdgeBuffer::default();
        let mut press = InputSnapshot::default();
        press.set_key(keys::SPACE, true);
        press.set_key_edge(keys::SPACE, true);
        edges.capture(press);
        edges.capture(InputSnapshot::default());

        let mut current = InputSnapshot::default();
        current.set_key(keys::SPACE, false);
        let stepped = edges.take_for_step(current);
        assert!(stepped.key_pressed(keys::SPACE));
        assert!(!stepped.key(keys::SPACE));
        assert!(!edges
            .take_for_step(InputSnapshot::default())
            .key_pressed(keys::SPACE));
    }

    #[test]
    fn input_edge_buffer_merges_press_and_release_edges() {
        let mut edges = InputEdgeBuffer::default();
        let mut press = InputSnapshot::default();
        press.set_key_edge(keys::SPACE, true);
        edges.capture(press);
        let mut release = InputSnapshot::default();
        release.set_key_edge(keys::SPACE, false);
        edges.capture(release);

        let stepped = edges.take_for_step(InputSnapshot::default());
        assert!(stepped.key_pressed(keys::SPACE));
        assert!(stepped.key_released(keys::SPACE));
    }

    /// #589: the static Player's input comes from the `InputSnapshot` the engine
    /// writes into the world each frame, not from the default value at
    /// `GameWorld::host()` construction time.
    #[cfg(feature = "host")]
    #[test]
    fn host_input_reads_the_world_resource() {
        let mut world = World::new();
        world.insert_resource(InputSnapshot {
            cursor: [12.0, 34.0],
            left_clicked: true,
            ..InputSnapshot::default()
        });

        let api = GameWorld::host(&mut world);
        assert_eq!(api.input().cursor, [12.0, 34.0]);
        assert!(api.input().left_clicked);
        assert_eq!(api.screen_size(), [0.0, 0.0]);
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_input_actions_sample_the_current_snapshot() {
        let mut world = World::new();
        let mut input = InputSnapshot::default();
        input.set_key(keys::KEY_D, true);
        input.set_key_edge(keys::KEY_D, true);
        world.insert_resource(input);

        let mut map = InputActionMap::default();
        map.bind_keys("move_right", [keys::KEY_D]);
        let api = GameWorld::host(&mut world);
        assert_eq!(
            api.input_actions(&mut map).action("move_right"),
            ActionState {
                held: true,
                pressed: true,
                released: false,
            }
        );
    }

    /// `set_input` must write through to the world: the next `GameWorld` over the
    /// same world can read it too.
    #[cfg(feature = "host")]
    #[test]
    fn set_input_writes_through_to_the_world() {
        let mut world = World::new();
        {
            let mut api = GameWorld::host(&mut world);
            api.set_input(InputSnapshot {
                scroll: 2.5,
                ..InputSnapshot::default()
            });
            assert_eq!(api.input().scroll, 2.5);
        }
        assert_eq!(
            world
                .get_resource::<InputSnapshot>()
                .expect("set_input writes the resource")
                .scroll,
            2.5
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_transform_interpolation_can_be_enabled_and_snapped() {
        use prism_scene::components::{LocalTransform, TransformInterpolation2D};

        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            LocalTransform {
                position: glam::Vec3::new(4.0, 6.0, 0.0),
                ..LocalTransform::default()
            },
        );
        let mut api = GameWorld::host(&mut world);
        let handle = handle_from_entity(entity);
        assert!(api.enable_transform_interpolation2d(handle));
        assert_eq!(
            api.get::<TransformInterpolation2D>(handle).unwrap().current,
            [4.0, 6.0]
        );
        let mut transform = api.get::<LocalTransform>(handle).unwrap();
        transform.position = glam::Vec3::new(10.0, 12.0, 0.0);
        assert!(api.set(handle, transform));
        assert!(api.snap_transform_interpolation2d(handle));
        assert_eq!(
            api.get::<TransformInterpolation2D>(handle)
                .unwrap()
                .previous,
            [10.0, 12.0]
        );
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_transform_interpolation_crosses_the_host_api() {
        use prism_scene::components::{LocalTransform, TransformInterpolation2D};

        let mut world = World::new();
        let registry = prism_scene::builtin_registry();
        let entity = world.spawn();
        world.insert(
            entity,
            LocalTransform {
                position: glam::Vec3::new(3.0, 5.0, 0.0),
                ..LocalTransform::default()
            },
        );
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, registry, 1);
        let host = commands.api();
        let mut api = GameWorld::plugin(&host, registry.type_registry());
        let handle = EntityHandle {
            id: entity.id(),
            generation: entity.generation(),
        };
        assert!(api.get::<LocalTransform>(handle).is_some());
        assert!(api.enable_transform_interpolation2d(handle));
        assert_eq!(
            api.get::<TransformInterpolation2D>(handle).unwrap().current,
            [3.0, 5.0]
        );
    }

    #[derive(Reflect, Clone, Debug, Default, PartialEq)]
    struct Health {
        value: f32,
    }

    #[cfg(feature = "plugin")]
    struct TestAssets;

    #[cfg(feature = "plugin")]
    impl prism_runtime_host::AssetIo for TestAssets {
        fn exists(&self, path: &str) -> bool {
            path == "lake_layers.json"
        }

        fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
            if path == "lake_layers.json" {
                Ok(br#"{"layers":[]}"#.to_vec())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, path))
            }
        }
    }

    /// #484: when the host injects no scene-state implementation,
    /// `save/load_scene_state` clearly return `false`.
    #[cfg(feature = "plugin")]
    #[test]
    fn scene_state_is_unavailable_without_a_host_callback() {
        let mut world = World::new();
        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, &components, 1);
        let host = commands.api();
        let mut api = GameWorld::plugin(&host, components.type_registry());

        assert!(!api.save_scene_state());
        assert!(!api.load_scene_state());
    }

    /// #482: a scene switch request goes through the host callback; the host takes
    /// it after the tick ends and executes it.
    #[cfg(feature = "plugin")]
    #[test]
    fn scene_requests_are_forwarded_to_the_host() {
        let mut world = World::new();
        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);
        let mut commands =
            prism_runtime_host::WorldCommands::new(&mut world, &components, 7).with_scene_switch();
        let host = commands.api();
        assert_eq!(
            host.capabilities & prism_project_api::HOST_CAP_SCENE,
            prism_project_api::HOST_CAP_SCENE
        );
        let mut api = GameWorld::plugin(&host, components.type_registry());
        assert_eq!(api.scene_generation(), 7);

        assert!(api.load_scene("assets/scenes/second.scene.json", false));
        assert_eq!(
            commands.take_scene_request(),
            Some(prism_runtime_host::SceneRequest::Load {
                path: "assets/scenes/second.scene.json".to_string(),
                additive: false,
            })
        );
        assert!(commands.take_scene_request().is_none());

        assert!(api.load_scene("assets/scenes/extra.scene.json", true));
        assert_eq!(
            commands.take_scene_request(),
            Some(prism_runtime_host::SceneRequest::Load {
                path: "assets/scenes/extra.scene.json".to_string(),
                additive: true,
            })
        );

        assert!(api.unload_scene());
        assert_eq!(
            commands.take_scene_request(),
            Some(prism_runtime_host::SceneRequest::Unload)
        );
    }

    /// The static Player never switches scenes: it clearly returns false rather than
    /// pretending to succeed.
    #[cfg(feature = "host")]
    #[test]
    fn host_backend_reports_scene_switching_unavailable() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);
        assert!(!api.load_scene("assets/scenes/second.scene.json", false));
        assert!(!api.unload_scene());
        assert_eq!(api.scene_generation(), 0);
    }

    /// #522: the host backend uses the engine's pure geometry query directly (no ABI
    /// needed).
    #[cfg(feature = "host")]
    #[test]
    fn host_backend_raycasts_against_colliders() {
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(entity, prism_scene::components::Collider::default());
        world.insert(
            entity,
            prism_scene::components::LocalToWorld(glam::Mat4::IDENTITY),
        );

        let api = GameWorld::host(&mut world);
        let hit = api
            .raycast([0.0, 0.0, 5.0], [0.0, 0.0, -1.0], 10.0, u32::MAX)
            .expect("the default box must be hit");
        assert_eq!(hit.entity, handle_from_entity(entity));
        assert!((hit.distance - 4.5).abs() < 1e-5);
        assert_eq!(hit.normal, [0.0, 0.0, 1.0]);
        assert!(!hit.is_trigger);

        // Casting in the opposite direction misses.
        assert!(api
            .raycast([0.0, 0.0, -5.0], [0.0, 0.0, -1.0], 10.0, u32::MAX)
            .is_none());
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_backend_raycast2d_returns_nearest_world_collider() {
        use prism_scene::components::{Collider2D, LocalTransform};

        let mut world = World::new();
        let far = world.spawn();
        world.insert(
            far,
            LocalTransform {
                position: glam::Vec3::new(20.0, 4.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            far,
            Collider2D {
                half_extents: [2.0, 2.0],
                layer: 0b0010,
                ..Collider2D::default()
            },
        );
        let near = world.spawn();
        world.insert(
            near,
            LocalTransform {
                position: glam::Vec3::new(8.0, 4.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            near,
            Collider2D {
                half_extents: [1.0, 1.0],
                is_trigger: true,
                layer: 0b0010,
                ..Collider2D::default()
            },
        );

        let api = GameWorld::host(&mut world);
        let hit = api
            .raycast2d([0.0, 4.0], [1.0, 0.0], 100.0, 0b0010)
            .expect("the nearest 2D collider should be hit");
        assert_eq!(hit.entity, handle_from_entity(near));
        assert!((hit.distance - 7.0).abs() < 1e-5);
        assert_eq!(hit.point, [7.0, 4.0]);
        assert_eq!(hit.normal, [-1.0, 0.0]);
        assert!(hit.is_trigger);
        assert!(api
            .raycast2d([0.0, 4.0], [1.0, 0.0], 100.0, 0b0100)
            .is_none());
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_backend_exposes_tilemap_colliders_through_the_same_query() {
        use prism_scene::components::{LocalTransform, Tilemap, TilemapCollider2D};

        let mut world = World::new();
        let tilemap = world.spawn();
        world.insert(
            tilemap,
            LocalTransform {
                position: glam::Vec3::new(20.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            tilemap,
            Tilemap {
                columns: 2,
                cell_size: [4.0, 4.0],
                tiles: vec![0, 7, 7, 0],
                ..Tilemap::default()
            },
        );
        world.insert(
            tilemap,
            TilemapCollider2D {
                solid_tile_ids: vec![7],
                layer: 0b1000,
                ..TilemapCollider2D::default()
            },
        );

        let api = GameWorld::host(&mut world);
        let hits = api.colliders2d(0b1000);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].bounds, Aabb2::new(20.0, 0.0, 24.0, 4.0));
        assert_eq!(hits[1].bounds, Aabb2::new(24.0, 4.0, 28.0, 8.0));
        assert_eq!(hits[0].entity, handle_from_entity(tilemap));
        let moved = api.move_and_slide2d(None, [0.0, 2.0], [1.0, 1.0], [40.0, 0.0], 0b1000);
        assert_eq!(moved.position, [19.0, 2.0]);
        assert!(moved.hit_x);
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_backend_moves_through_and_lands_on_one_way_collider() {
        use prism_scene::components::{Collider2D, LocalTransform};

        let mut world = World::new();
        let platform = world.spawn();
        world.insert(
            platform,
            LocalTransform {
                position: glam::Vec3::new(0.0, 2.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            platform,
            Collider2D {
                half_extents: [8.0, 1.0],
                one_way_top: true,
                layer: 0b1000,
                ..Collider2D::default()
            },
        );
        let api = GameWorld::host(&mut world);
        let hits = api.colliders2d(0b1000);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].one_way_top);

        let upward = api.move_and_slide2d(None, [0.0, -4.0], [1.0, 1.0], [0.0, 10.0], 0b1000);
        assert_eq!(upward.position, [0.0, 6.0]);
        let landed = api.move_and_slide2d(None, [0.0, 10.0], [1.0, 1.0], [0.0, -20.0], 0b1000);
        assert!((landed.position[1] - 4.0).abs() < 1e-5);
        assert!(landed.grounded);
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_raycast2d_crosses_the_host_api() {
        use prism_scene::components::{Collider2D, LocalTransform};

        let mut world = World::new();
        let wall = world.spawn();
        world.insert(
            wall,
            LocalTransform {
                position: glam::Vec3::new(12.0, 2.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            wall,
            Collider2D {
                half_extents: [2.0, 3.0],
                layer: 0b0100,
                ..Collider2D::default()
            },
        );
        let registry = prism_scene::builtin_registry();
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, registry, 1);
        let host = commands.api();
        let api = GameWorld::plugin(&host, registry.type_registry());
        let hit = api
            .raycast2d([0.0, 2.0], [1.0, 0.0], 100.0, 0b0100)
            .expect("plugin raycast should forward to the host");
        assert_eq!(hit.entity.id, wall.id());
        assert!((hit.distance - 10.0).abs() < 1e-5);
        assert_eq!(hit.normal, [-1.0, 0.0]);
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_raycast2d_hits_tilemap_generated_bounds() {
        use prism_scene::components::{LocalTransform, Tilemap, TilemapCollider2D};

        let mut world = World::new();
        let tilemap = world.spawn();
        world.insert(
            tilemap,
            LocalTransform {
                position: glam::Vec3::new(8.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            tilemap,
            Tilemap {
                columns: 1,
                cell_size: [2.0, 4.0],
                tiles: vec![3],
                ..Tilemap::default()
            },
        );
        world.insert(
            tilemap,
            TilemapCollider2D {
                solid_tile_ids: vec![3],
                layer: 0b1000,
                ..TilemapCollider2D::default()
            },
        );
        let registry = prism_scene::builtin_registry();
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, registry, 1);
        let host = commands.api();
        let api = GameWorld::plugin(&host, registry.type_registry());

        let hit = api
            .raycast2d([0.0, 2.0], [1.0, 0.0], 20.0, 0b1000)
            .expect("the plugin raycast should see generated tilemap bounds");
        assert_eq!(hit.entity.id, tilemap.id());
        assert_eq!(hit.distance, 8.0);
        assert_eq!(hit.point, [8.0, 2.0]);
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_input_actions_sample_the_host_snapshot() {
        let mut world = World::new();
        let registry = prism_scene::builtin_registry();
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, registry, 1);
        let mut input = InputSnapshot::default();
        input.set_key(keys::SPACE, true);
        input.set_key_edge(keys::SPACE, true);
        commands.set_input(input);
        let host = commands.api();
        let api = GameWorld::plugin(&host, registry.type_registry());

        let mut map = InputActionMap::default();
        map.bind_keys("jump", [keys::SPACE]);
        assert_eq!(
            api.input_actions(&mut map).action("jump"),
            ActionState {
                held: true,
                pressed: true,
                released: false,
            }
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_backend_queries_and_moves_against_2d_colliders() {
        use prism_scene::components::{Active, Child, LocalTransform, Parent};

        let mut world = World::new();
        let wall = world.spawn();
        world.insert(
            wall,
            Collider2D {
                half_extents: [1.0, 5.0],
                ..Collider2D::default()
            },
        );
        world.insert(
            wall,
            LocalTransform {
                position: glam::Vec3::new(10.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        let transformed_parent = world.spawn();
        world.insert(
            transformed_parent,
            LocalTransform {
                position: glam::Vec3::new(30.0, 40.0, 0.0),
                scale: glam::Vec3::new(2.0, 1.0, 1.0),
                rotation: glam::Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            },
        );
        let transformed_child = world.spawn();
        world.insert(transformed_child, Parent(transformed_parent));
        world.insert(transformed_parent, Child(vec![transformed_child]));
        world.insert(
            transformed_child,
            LocalTransform {
                position: glam::Vec3::new(3.0, 4.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            transformed_child,
            Collider2D {
                offset: [1.0, -2.0],
                half_extents: [2.0, 1.0],
                ..Collider2D::default()
            },
        );
        prism_engine::scene::systems::hierarchy::hierarchy_system(&mut world);
        world
            .get_mut::<LocalTransform>(transformed_parent)
            .unwrap()
            .position
            .x = 40.0;
        let inactive_parent = world.spawn();
        world.insert(inactive_parent, Active(true));
        let inactive_child = world.spawn();
        world.insert(inactive_child, Parent(inactive_parent));
        world.insert(inactive_child, Collider2D::default());
        world.insert(inactive_child, LocalTransform::default());
        prism_engine::scene::systems::hierarchy::active_hierarchy_system(&mut world);

        let mut api = GameWorld::host(&mut world);
        let parent_handle = handle_from_entity(inactive_parent);
        assert!(api
            .overlap2d(Aabb2::new(-1.0, -1.0, 1.0, 1.0), u32::MAX)
            .iter()
            .any(|hit| hit.entity == handle_from_entity(inactive_child)));
        assert!(api.set(parent_handle, Active(false)));
        let hits = api.overlap2d(Aabb2::new(8.0, -1.0, 9.5, 1.0), u32::MAX);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity, handle_from_entity(wall));
        let parented = api
            .colliders2d(u32::MAX)
            .into_iter()
            .find(|hit| hit.entity == handle_from_entity(transformed_child))
            .expect("parented collider should be present");
        for (actual, expected) in parented
            .bounds
            .min
            .into_iter()
            .chain(parented.bounds.max)
            .zip([37.0, 44.0, 39.0, 52.0])
        {
            assert!((actual - expected).abs() < 1e-5);
        }
        assert!(api
            .overlap2d(Aabb2::new(-1.0, -1.0, 1.0, 1.0), u32::MAX)
            .is_empty());

        let moved = api.move_and_slide2d(None, [0.0, 0.0], [1.0, 1.0], [20.0, 0.0], u32::MAX);
        assert!((moved.position[0] - 8.0).abs() < 1e-4);
        assert!(moved.hit_x);
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_backend_sorts_nearby_interactables_by_priority_then_distance() {
        use prism_scene::components::{Interactable2D, LocalTransform};

        let mut world = World::new();
        let farther_high_priority = world.spawn();
        world.insert(
            farther_high_priority,
            LocalTransform {
                position: glam::Vec3::new(6.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            farther_high_priority,
            Collider2D {
                half_extents: [0.5, 0.5],
                layer: 0b0010,
                ..Collider2D::default()
            },
        );
        world.insert(
            farther_high_priority,
            Interactable2D {
                prompt: "Open gate".to_string(),
                priority: 2,
                ..Interactable2D::default()
            },
        );

        let nearer = world.spawn();
        world.insert(
            nearer,
            LocalTransform {
                position: glam::Vec3::new(2.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            nearer,
            Collider2D {
                half_extents: [0.5, 0.5],
                layer: 0b0010,
                ..Collider2D::default()
            },
        );
        world.insert(nearer, Interactable2D::default());

        let filtered_layer = world.spawn();
        world.insert(
            filtered_layer,
            LocalTransform {
                position: glam::Vec3::new(1.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            filtered_layer,
            Collider2D {
                layer: 0b0100,
                ..Collider2D::default()
            },
        );
        world.insert(filtered_layer, Interactable2D::default());

        let disabled = world.spawn();
        world.insert(disabled, LocalTransform::default());
        world.insert(disabled, Collider2D::default());
        world.insert(
            disabled,
            Interactable2D {
                enabled: false,
                ..Interactable2D::default()
            },
        );

        let api = GameWorld::host(&mut world);
        let candidates = api.interactables2d([0.0, 0.0], 10.0, 0b0010);
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0].entity,
            handle_from_entity(farther_high_priority)
        );
        assert_eq!(candidates[0].prompt, "Open gate");
        assert!((candidates[0].distance - 5.5).abs() < 1e-5);
        assert_eq!(candidates[1].entity, handle_from_entity(nearer));
        assert!(api.interactables2d([0.0, 0.0], -1.0, u32::MAX).is_empty());
        assert!(api
            .interactables2d([f32::NAN, 0.0], 10.0, u32::MAX)
            .is_empty());
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_backend_reports_hostile_damage_contacts_once_and_in_stable_order() {
        use prism_scene::components::{
            Hitbox2D, Hurtbox2D, LocalTransform, Tilemap, TilemapCollider2D,
        };

        let mut world = World::new();
        let hitbox = world.spawn();
        world.insert(hitbox, LocalTransform::default());
        world.insert(
            hitbox,
            Collider2D {
                half_extents: [2.0, 2.0],
                is_trigger: true,
                layer: 0b0001,
                ..Collider2D::default()
            },
        );
        world.insert(
            hitbox,
            Hitbox2D {
                damage: 3,
                team: 1,
                enabled: true,
            },
        );

        let hurtbox = world.spawn();
        world.insert(hurtbox, LocalTransform::default());
        world.insert(
            hurtbox,
            Collider2D {
                half_extents: [1.0, 1.0],
                layer: 0b0010,
                ..Collider2D::default()
            },
        );
        world.insert(
            hurtbox,
            Hurtbox2D {
                team: 2,
                enabled: true,
            },
        );
        world.insert(
            hurtbox,
            Tilemap {
                columns: 2,
                cell_size: [1.0, 1.0],
                tiles: vec![1, 1],
                ..Tilemap::default()
            },
        );
        world.insert(
            hurtbox,
            TilemapCollider2D {
                solid_tile_ids: vec![1],
                layer: 0b0010,
                ..TilemapCollider2D::default()
            },
        );

        let friendly = world.spawn();
        world.insert(friendly, LocalTransform::default());
        world.insert(
            friendly,
            Collider2D {
                layer: 0b0010,
                ..Collider2D::default()
            },
        );
        world.insert(
            friendly,
            Hurtbox2D {
                team: 1,
                enabled: true,
            },
        );

        let disabled = world.spawn();
        world.insert(disabled, LocalTransform::default());
        world.insert(disabled, Collider2D::default());
        world.insert(
            disabled,
            Hurtbox2D {
                team: 2,
                enabled: false,
            },
        );

        let api = GameWorld::host(&mut world);
        let contacts = api.damage_contacts2d(0b0001, 0b0010);
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].hitbox, handle_from_entity(hitbox));
        assert_eq!(contacts[0].hurtbox, handle_from_entity(hurtbox));
        assert_eq!(contacts[0].damage, 3);
        assert_eq!((contacts[0].hitbox_team, contacts[0].hurtbox_team), (1, 2));
        assert!(api.damage_contacts2d(0, 0b0010).is_empty());
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_queries_the_same_2d_collider_schema() {
        use prism_scene::components::{Active, ActiveInHierarchy, Child, LocalTransform, Parent};

        let mut world = World::new();
        let registry = prism_scene::builtin_registry();
        let wall = world.spawn();
        world.insert(
            wall,
            Collider2D {
                half_extents: [1.0, 5.0],
                ..Collider2D::default()
            },
        );
        world.insert(
            wall,
            LocalTransform {
                position: glam::Vec3::new(10.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        let transformed_parent = world.spawn();
        world.insert(
            transformed_parent,
            LocalTransform {
                position: glam::Vec3::new(30.0, 40.0, 0.0),
                scale: glam::Vec3::new(2.0, 1.0, 1.0),
                rotation: glam::Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            },
        );
        let transformed_child = world.spawn();
        world.insert(transformed_child, Parent(transformed_parent));
        world.insert(transformed_parent, Child(vec![transformed_child]));
        world.insert(
            transformed_child,
            LocalTransform {
                position: glam::Vec3::new(3.0, 4.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            transformed_child,
            Collider2D {
                offset: [1.0, -2.0],
                half_extents: [2.0, 1.0],
                ..Collider2D::default()
            },
        );
        prism_engine::scene::systems::hierarchy::hierarchy_system(&mut world);
        world
            .get_mut::<LocalTransform>(transformed_parent)
            .unwrap()
            .position
            .x = 40.0;
        let inactive_parent = world.spawn();
        world.insert(inactive_parent, Active(true));
        let inactive_child = world.spawn();
        world.insert(inactive_child, Parent(inactive_parent));
        world.insert(inactive_child, Collider2D::default());
        world.insert(inactive_child, LocalTransform::default());
        world.insert(inactive_child, ActiveInHierarchy(true));
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, registry, 1);
        let host = commands.api();
        let mut api = GameWorld::plugin(&host, registry.type_registry());

        let parent_handle = EntityHandle {
            id: inactive_parent.id(),
            generation: inactive_parent.generation(),
        };
        assert!(api.set(parent_handle, Active(false)));
        let hits = api.overlap2d(Aabb2::new(8.0, -1.0, 9.5, 1.0), u32::MAX);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity.id, wall.id());
        assert!(api
            .overlap2d(Aabb2::new(-1.0, -1.0, 1.0, 1.0), u32::MAX)
            .is_empty());
        let parented = api
            .colliders2d(u32::MAX)
            .into_iter()
            .find(|hit| hit.entity == handle_from_entity(transformed_child))
            .expect("plugin query should preserve parented collider world bounds");
        for (actual, expected) in parented
            .bounds
            .min
            .into_iter()
            .chain(parented.bounds.max)
            .zip([37.0, 44.0, 39.0, 52.0])
        {
            assert!((actual - expected).abs() < 1e-5);
        }
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_reads_and_sorts_interactable_components() {
        use prism_scene::components::{Interactable2D, LocalTransform};

        let mut world = World::new();
        let registry = prism_scene::builtin_registry();
        let second = world.spawn();
        world.insert(
            second,
            LocalTransform {
                position: glam::Vec3::new(4.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            second,
            Collider2D {
                half_extents: [0.5, 0.5],
                layer: 0b0010,
                ..Collider2D::default()
            },
        );
        world.insert(
            second,
            Interactable2D {
                prompt: "Talk".to_string(),
                priority: 1,
                ..Interactable2D::default()
            },
        );
        let first = world.spawn();
        world.insert(
            first,
            LocalTransform {
                position: glam::Vec3::new(2.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            first,
            Collider2D {
                half_extents: [0.5, 0.5],
                layer: 0b0010,
                ..Collider2D::default()
            },
        );
        world.insert(
            first,
            Interactable2D {
                prompt: "Open".to_string(),
                priority: 1,
                ..Interactable2D::default()
            },
        );

        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, registry, 1);
        let host = commands.api();
        let api = GameWorld::plugin(&host, registry.type_registry());
        let candidates = api.interactables2d([0.0, 0.0], 10.0, 0b0010);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].entity.id, first.id());
        assert_eq!(candidates[0].prompt, "Open");
        assert_eq!(candidates[1].entity.id, second.id());
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_reports_damage_contacts_through_reflected_components() {
        use prism_scene::components::{Hitbox2D, Hurtbox2D, LocalTransform};

        let mut world = World::new();
        let registry = prism_scene::builtin_registry();
        let hitbox = world.spawn();
        world.insert(hitbox, LocalTransform::default());
        world.insert(
            hitbox,
            Collider2D {
                half_extents: [2.0, 2.0],
                is_trigger: true,
                layer: 0b0001,
                ..Collider2D::default()
            },
        );
        world.insert(
            hitbox,
            Hitbox2D {
                damage: 2,
                team: 3,
                enabled: true,
            },
        );
        let hurtbox = world.spawn();
        world.insert(hurtbox, LocalTransform::default());
        world.insert(
            hurtbox,
            Collider2D {
                half_extents: [1.0, 1.0],
                layer: 0b0010,
                ..Collider2D::default()
            },
        );
        world.insert(
            hurtbox,
            Hurtbox2D {
                team: 4,
                enabled: true,
            },
        );

        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, registry, 1);
        let host = commands.api();
        let api = GameWorld::plugin(&host, registry.type_registry());
        let contacts = api.damage_contacts2d(0b0001, 0b0010);
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].hitbox.id, hitbox.id());
        assert_eq!(contacts[0].hurtbox.id, hurtbox.id());
        assert_eq!(contacts[0].damage, 2);
        assert_eq!((contacts[0].hitbox_team, contacts[0].hurtbox_team), (3, 4));
    }

    /// #522: the plugin backend forwards the query to the host; returns `None` when
    /// the host injected no implementation.
    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_forwards_raycast_to_the_host() {
        fn fake_raycast(
            _world: &World,
            origin: [f32; 3],
            _direction: [f32; 3],
            _max_distance: f32,
            layer_mask: u32,
        ) -> Option<prism_runtime_host::RaycastResult> {
            if layer_mask == 0 {
                return None;
            }
            Some(prism_runtime_host::RaycastResult {
                entity: prism_ecs::Entity::from_raw(7, 2),
                distance: 1.5,
                point: origin,
                normal: [0.0, 1.0, 0.0],
                is_trigger: true,
            })
        }

        let mut world = World::new();
        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);

        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, &components, 1)
            .with_raycast(fake_raycast);
        let host = commands.api();
        assert_ne!(host.capabilities & prism_project_api::HOST_CAP_PHYSICS, 0);
        let api = GameWorld::plugin(&host, components.type_registry());
        let hit = api
            .raycast([1.0, 2.0, 3.0], [0.0, 0.0, -1.0], 10.0, 1)
            .expect("the fake host always hits");
        assert_eq!(
            hit.entity,
            EntityHandle {
                id: 7,
                generation: 2
            }
        );
        assert_eq!(hit.distance, 1.5);
        assert_eq!(hit.point, [1.0, 2.0, 3.0]);
        assert_eq!(hit.normal, [0.0, 1.0, 0.0]);
        assert!(hit.is_trigger);

        // Do not call the callback when the host did not declare the capability bit.
        let mut plain = prism_runtime_host::WorldCommands::new(&mut world, &components, 1);
        let host = plain.api();
        let api = GameWorld::plugin(&host, components.type_registry());
        assert!(api
            .raycast([0.0; 3], [0.0, 0.0, -1.0], 10.0, u32::MAX)
            .is_none());
    }

    /// The plugin backend reads loose assets through `HOST_CAP_ASSET`, with the same
    /// signature as the host backend.
    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_reads_assets_through_the_host() {
        let mut world = World::new();
        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, &components, 1)
            .with_assets(&TestAssets);
        let host = commands.api();
        let api = GameWorld::plugin(&host, components.type_registry());

        assert!(api.asset_exists("lake_layers.json"));
        assert!(!api.asset_exists("missing.json"));
        assert_eq!(
            api.read_asset_text("lake_layers.json").unwrap(),
            r#"{"layers":[]}"#
        );
        assert!(api.read_asset_text("missing.json").is_err());
    }

    #[test]
    fn components_round_trip_through_handles() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);

        let entity = api.spawn();
        assert!(api.is_alive(entity));
        assert!(api.set(entity, Health { value: 3.0 }));
        assert_eq!(api.get::<Health>(entity), Some(Health { value: 3.0 }));
        assert_eq!(api.query::<Health>(), vec![entity]);

        assert!(api.remove::<Health>(entity));
        assert_eq!(api.get::<Health>(entity), None);
        assert!(api.query::<Health>().is_empty());
    }

    /// #1056: gameplay code binds scene entities by name; the lookup lives in the
    /// API instead of every project re-walking `query::<Name>()`.
    #[test]
    fn host_backend_finds_entities_by_name() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);

        let boat = api.spawn();
        assert!(api.set(boat, Name("boat".to_owned())));
        let camera = api.spawn();
        assert!(api.set(camera, Name("camera".to_owned())));
        let anonymous = api.spawn();
        assert!(api.set(anonymous, Health { value: 1.0 }));

        assert_eq!(api.find_by_name("boat"), Some(boat));
        assert_eq!(api.find_by_name("camera"), Some(camera));
        assert_eq!(api.find_by_name("missing"), None);
    }

    #[test]
    fn writes_to_dead_handles_are_rejected() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);

        let entity = api.spawn();
        api.despawn(entity);
        assert!(!api.is_alive(entity));
        assert!(!api.set(entity, Health { value: 1.0 }));
        assert_eq!(api.get::<Health>(entity), None);
    }

    #[test]
    fn recycled_slot_does_not_revive_stale_handle() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);

        let stale = api.spawn();
        api.despawn(stale);
        let fresh = api.spawn();

        // The same slot is reused, but with a different generation: the old handle
        // must stay invalid.
        assert_eq!(stale.id, fresh.id);
        assert_ne!(stale.generation, fresh.generation);
        assert!(!api.is_alive(stale));
        assert!(api.is_alive(fresh));
    }

    #[test]
    fn host_quads_are_queued_for_the_engine() {
        let mut world = World::new();
        {
            let mut api = GameWorld::host(&mut world);
            api.draw_quad(UiQuad {
                layer: 3,
                ..UiQuad::default()
            });
        }

        // The `GameWorld` is gone; the quad survives in the world resource the
        // engine merges into `UiDrawList` (#1048).
        let external = world
            .remove_resource::<prism_engine::ui::ExternalUiQuads>()
            .expect("quad queued in the world");
        assert_eq!(external.0.len(), 1);
        assert_eq!(external.0[0].layer, 3);
        assert_eq!(external.0[0].order, prism_engine::ui::EXTERNAL_UI_ORDER);
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_reads_and_writes_through_host_callbacks() {
        use prism_scene::components::Name;

        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);

        let mut world = World::new();
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, &components, 1);
        let api = commands.api();
        let registry = components.type_registry();

        {
            let mut view = GameWorld::plugin(&api, registry);
            let entity = view.spawn();
            assert!(view.set(entity, Name("from_plugin".to_owned())));
            assert_eq!(view.query::<Name>(), vec![entity]);
            assert_eq!(
                view.get::<Name>(entity).map(|name| name.0),
                Some("from_plugin".to_owned())
            );
            assert!(view.remove::<Name>(entity));
            assert!(view.query::<Name>().is_empty());
            assert!(view.is_alive(entity));
            view.despawn(entity);
            assert!(!view.is_alive(entity));
        }

        drop(commands);
        assert!(world.query::<Name>().next().is_none());
    }

    /// #1056: the plugin backend resolves names through the same component reads,
    /// so no extra ABI callback is involved.
    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_finds_entities_by_name() {
        use prism_scene::components::Name;

        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);

        let mut world = World::new();
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, &components, 1);
        let api = commands.api();
        let registry = components.type_registry();

        let mut view = GameWorld::plugin(&api, registry);
        let boat = view.spawn();
        assert!(view.set(boat, Name("boat".to_owned())));
        let camera = view.spawn();
        assert!(view.set(camera, Name("camera".to_owned())));

        assert_eq!(view.find_by_name("camera"), Some(camera));
        assert_eq!(view.find_by_name("missing"), None);
    }
}
