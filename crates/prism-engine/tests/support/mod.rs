//! Deterministic fixed-step harness for engine simulation tests.
//!
//! `SimHarness` owns an [`Engine`], an [`InputManager`], a fixed timestep and a
//! tick counter. Tests build a small fixture (either from author JSON or through
//! the `spawn_*` helpers), script input per tick, then inspect [`Snapshot`]s.
//!
//! The harness deliberately has no wall-clock dependency: one `tick()` always
//! advances exactly `step` seconds. Randomness is opt-in through
//! [`SimHarness::install_random_walker`], which uses a seeded SplitMix64 stream.

#![allow(dead_code)] // Reusable helper surface; individual scenarios use a subset.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use prism_engine::ecs::World;
use prism_engine::input::{ElementState, InputManager, KeyCode, MouseButton};
use prism_engine::scene::components::{
    Child, LocalToWorld, LocalTransform, Name, Parent, SceneMember,
};
use prism_engine::scene::systems::animation::{
    AnimationEventQueue, ClipPlayhead, SpriteAnimationState,
};
use prism_engine::ui::{ComputedLayout, Node, Style, UiDrawItem, UiDrawList};
use prism_engine::Engine;
use prism_scene::HierarchyHelper;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A queued input event, applied at the start of the next tick.
#[derive(Clone, Copy, Debug)]
pub enum InputAction {
    Key(KeyCode, bool),
    MouseButton(MouseButton, bool),
    MouseMove([f64; 2]),
}

/// One scripted simulation input for a single tick.
#[derive(Clone, Copy, Debug)]
pub enum ScriptedInput {
    /// Press a key/button for this tick (held until released).
    PressKey(KeyCode),
    ReleaseKey(KeyCode),
    PressMouse(MouseButton),
    ReleaseMouse(MouseButton),
    MoveMouse([f64; 2]),
}

/// An isolated simulation environment.
pub struct SimHarness {
    engine: Engine,
    input: InputManager,
    step: f32,
    tick: u64,
    tracked: Vec<prism_engine::ecs::Entity>,
    queued_input: Vec<InputAction>,
    scene_requests: Vec<String>,
    root: PathBuf,
}

impl SimHarness {
    /// Create a harness with a 60 Hz fixed step and the given deterministic seed.
    ///
    /// The seed is only used by [`Self::install_random_walker`]; scenarios that
    /// do not need randomness can ignore it.
    pub fn new(seed: u64) -> Self {
        let root = std::env::temp_dir().join(format!(
            "prism-sim-harness-{}-{seed}-{}",
            std::process::id(),
            next_harness_id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create harness temp dir");
        Self {
            engine: Engine::empty(),
            input: InputManager::new(),
            step: 1.0 / 60.0,
            tick: 0,
            tracked: Vec::new(),
            queued_input: Vec::new(),
            scene_requests: Vec::new(),
            root,
        }
    }

    /// Override the fixed timestep (seconds).
    pub fn with_step(mut self, step: f32) -> Self {
        assert!(step > 0.0 && step.is_finite(), "step must be positive");
        self.step = step;
        self
    }

    pub fn step(&self) -> f32 {
        self.step
    }

    pub fn tick_count(&self) -> u64 {
        self.tick
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    pub fn world(&self) -> &World {
        self.engine.world()
    }

    pub fn world_mut(&mut self) -> &mut World {
        self.engine.world_mut()
    }

    pub fn schedule_mut(&mut self) -> &mut prism_engine::ecs::schedule::Schedule {
        self.engine.schedule_mut()
    }

    /// Spawn a named entity without assuming a transform.
    pub fn spawn(&mut self, name: &str) -> prism_engine::ecs::Entity {
        let entity = self.engine.world_mut().spawn();
        self.engine
            .world_mut()
            .insert(entity, Name(name.to_string()));
        self.tracked.push(entity);
        entity
    }

    /// Spawn a named entity with a local transform (and a matching initial world
    /// transform so snapshots are meaningful before the first tick).
    pub fn spawn_transformed(
        &mut self,
        name: &str,
        position: [f32; 3],
    ) -> prism_engine::ecs::Entity {
        let entity = self.spawn(name);
        let transform = LocalTransform {
            position: glam::Vec3::from_array(position),
            ..LocalTransform::default()
        };
        let world = self.engine.world_mut();
        world.insert(entity, transform.clone());
        world.insert(entity, LocalToWorld(transform.to_model_matrix()));
        entity
    }

    /// Spawn a named UI node. The layout system fills in `ComputedLayout`.
    pub fn spawn_ui(&mut self, name: &str, style: Style) -> prism_engine::ecs::Entity {
        let entity = self.spawn(name);
        let world = self.engine.world_mut();
        world.insert(entity, Node {});
        world.insert(entity, style);
        entity
    }

    /// Load an author-format `.scene.json` fixture. Entities are tracked for
    /// snapshots; the temporary file is removed when the harness is dropped.
    pub fn load_scene_json(&mut self, text: &str) -> Vec<prism_engine::ecs::Entity> {
        let path = self.root.join(format!(
            "scene-{}-{}.scene.json",
            self.tick,
            self.scene_requests.len()
        ));
        fs::write(&path, text).expect("write scene fixture");
        let entities = self
            .engine
            .load_scene_json_file(&path)
            .expect("load scene fixture");
        self.tracked.extend(entities.iter().copied());
        self.scene_requests.push(format!("load:{}", path.display()));
        entities
    }

    /// Unload the current scene (the harness observes and records the request).
    pub fn unload_scene(&mut self) -> usize {
        let removed = self.engine.unload_scene();
        self.scene_requests.push(format!("unload:{removed}"));
        removed
    }

    /// Find a tracked entity by its `Name` component.
    pub fn entity(&self, name: &str) -> prism_engine::ecs::Entity {
        self.tracked
            .iter()
            .copied()
            .find(|entity| {
                self.world()
                    .get::<Name>(*entity)
                    .is_some_and(|entity_name| entity_name.0 == name)
            })
            .unwrap_or_else(|| panic!("entity {name:?} not found in harness"))
    }

    /// Queue a press that is applied at the start of the next tick.
    pub fn press_key(&mut self, key: KeyCode) {
        self.queued_input.push(InputAction::Key(key, true));
    }

    /// Queue a release that is applied at the start of the next tick.
    pub fn release_key(&mut self, key: KeyCode) {
        self.queued_input.push(InputAction::Key(key, false));
    }

    /// Queue a mouse button transition for the next tick.
    pub fn mouse_button(&mut self, button: MouseButton, pressed: bool) {
        self.queued_input
            .push(InputAction::MouseButton(button, pressed));
    }

    /// Queue a cursor position for the next tick.
    pub fn move_mouse(&mut self, position: [f64; 2]) {
        self.queued_input.push(InputAction::MouseMove(position));
    }

    /// Apply a batch of scripted input for the next tick.
    pub fn script(&mut self, input: &[ScriptedInput]) {
        for action in input {
            match *action {
                ScriptedInput::PressKey(key) => self.press_key(key),
                ScriptedInput::ReleaseKey(key) => self.release_key(key),
                ScriptedInput::PressMouse(button) => self.mouse_button(button, true),
                ScriptedInput::ReleaseMouse(button) => self.mouse_button(button, false),
                ScriptedInput::MoveMouse(position) => self.move_mouse(position),
            }
        }
    }

    /// Register a deterministic random walker that moves `entity` on every tick.
    ///
    /// The same seed always produces the same sequence, so two harnesses with
    /// the same fixture and script produce identical snapshots.
    pub fn install_random_walker(&mut self, entity: prism_engine::ecs::Entity, seed: u64) {
        let mut state = seed ^ 0x9e37_79b9_7f4a_7c15;
        self.schedule_mut()
            .add_system("sim::random_walker", move |world, dt| {
                state = splitmix64(state);
                let x = unit_from_bits(state) * dt;
                state = splitmix64(state);
                let y = unit_from_bits(state) * dt;
                state = splitmix64(state);
                let z = unit_from_bits(state) * dt;
                if let Some(transform) = world.get_mut::<LocalTransform>(entity) {
                    transform.position += glam::Vec3::new(x, y, z);
                }
            });
    }

    /// Advance exactly one fixed step and capture the post-tick snapshot.
    pub fn tick(&mut self) -> Snapshot {
        self.input.begin_frame();
        for action in std::mem::take(&mut self.queued_input) {
            match action {
                InputAction::Key(key, pressed) => self.input.handle_keyboard(
                    key,
                    if pressed {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    },
                ),
                InputAction::MouseButton(button, pressed) => self.input.handle_mouse_button(
                    button,
                    if pressed {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    },
                ),
                InputAction::MouseMove(position) => self.input.handle_mouse_move(position),
            }
        }
        self.tick += 1;
        self.engine.update(self.step, &self.input);
        self.checkpoint()
    }

    /// Run `ticks` fixed steps, returning one snapshot per tick.
    pub fn run(&mut self, ticks: u32) -> Vec<Snapshot> {
        (0..ticks).map(|_| self.tick()).collect()
    }

    /// Capture the current state without advancing time.
    pub fn checkpoint(&self) -> Snapshot {
        let world = self.engine.world();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for entity in &self.tracked {
            if let Some(name) = world.get::<Name>(*entity) {
                *counts.entry(name.0.clone()).or_default() += 1;
            }
        }

        let mut entities = BTreeMap::new();
        for entity in &self.tracked {
            if !world.is_alive(*entity) {
                continue;
            }
            let id = entity.id();
            let name = world
                .get::<Name>(*entity)
                .map(|name| name.0.clone())
                .unwrap_or_else(|| format!("#{id}"));
            let key = if counts.get(&name).copied().unwrap_or_default() > 1 {
                format!("{name}#{id}")
            } else {
                name.clone()
            };
            let components = component_names(world, *entity);
            let transform = world.get::<LocalTransform>(*entity);
            let world_transform = world.get::<LocalToWorld>(*entity);
            let animation = animation_state(world, *entity);
            let ui = world
                .get::<ComputedLayout>(*entity)
                .map(|layout| UiNodeState {
                    rect: layout.rect,
                    order: layout.order,
                    scale: layout.scale,
                });

            entities.insert(
                key,
                EntitySnapshot {
                    id,
                    name,
                    parent: world.get::<Parent>(*entity).map(|parent| parent.0.id()),
                    children: world
                        .get::<Child>(*entity)
                        .map(|children| children.0.iter().map(|child| child.id()).collect())
                        .unwrap_or_default(),
                    components,
                    position: transform.map(|value| value.position.to_array()),
                    rotation: transform.map(|value| value.rotation.to_array()),
                    scale: transform.map(|value| value.scale.to_array()),
                    world_position: world_transform.map(|value| {
                        let (_, _, translation) = value.0.to_scale_rotation_translation();
                        translation.to_array()
                    }),
                    animation,
                    ui,
                },
            );
        }

        Snapshot {
            tick: self.tick,
            time: self.tick as f32 * self.step,
            scene: SceneSnapshot {
                generation: self.engine.scene_generation(),
                current_scene: self.engine.current_scene_name().map(str::to_string),
                scene_members: world.query::<SceneMember>().count(),
                requests: self.scene_requests.clone(),
            },
            entities,
            animation_events: world
                .get_resource::<AnimationEventQueue>()
                .map(|queue| {
                    queue
                        .events
                        .iter()
                        .map(|event| AnimationEventState {
                            entity: event.entity.id(),
                            name: event.name.clone(),
                            time: event.time,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            ui_quads: world
                .get_resource::<UiDrawList>()
                .map(ui_quads)
                .unwrap_or_default(),
            input: world
                .get_resource::<prism_project_api::InputSnapshot>()
                .map(InputSnapshotView::from_snapshot),
        }
    }
}

impl Drop for SimHarness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The observable state captured after a tick (or at a checkpoint).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Snapshot {
    pub tick: u64,
    pub time: f32,
    pub scene: SceneSnapshot,
    /// Keyed by entity name (or `name#id` for duplicate names) for stable,
    /// human-readable diff paths like `entities.Cube.position[0]`.
    pub entities: BTreeMap<String, EntitySnapshot>,
    pub animation_events: Vec<AnimationEventState>,
    pub ui_quads: Vec<UiQuadState>,
    pub input: Option<InputSnapshotView>,
}

impl Snapshot {
    /// Structural diff with an absolute float tolerance.
    ///
    /// Returns one entry per changed field; arrays recurse with `[index]` paths.
    pub fn diff(&self, other: &Snapshot, tolerance: f32) -> Vec<Diff> {
        let mut diffs = Vec::new();
        let left = serde_json::to_value(self).expect("snapshot serializes");
        let right = serde_json::to_value(other).expect("snapshot serializes");
        diff_value("", &left, &right, tolerance, &mut diffs);
        diffs
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SceneSnapshot {
    pub generation: u64,
    pub current_scene: Option<String>,
    pub scene_members: usize,
    /// Harness-observed load/unload requests (the order they were issued).
    pub requests: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EntitySnapshot {
    pub id: u32,
    pub name: String,
    pub parent: Option<u32>,
    pub children: Vec<u32>,
    /// Every component type currently attached to the entity, sorted.
    pub components: Vec<String>,
    pub position: Option<[f32; 3]>,
    pub rotation: Option<[f32; 4]>,
    pub scale: Option<[f32; 3]>,
    pub world_position: Option<[f32; 3]>,
    pub animation: Option<AnimationState>,
    pub ui: Option<UiNodeState>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AnimationState {
    pub clip_time: Option<f32>,
    pub clip_finished: Option<bool>,
    pub clip_playing: Option<bool>,
    pub sprite_time: Option<f32>,
    pub sprite_frame: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AnimationEventState {
    pub entity: u32,
    pub name: String,
    pub time: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UiNodeState {
    pub rect: [f32; 4],
    pub order: u32,
    pub scale: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UiQuadState {
    pub rect: [f32; 4],
    pub color: [f32; 4],
    pub order: u32,
    pub layer: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputSnapshotView {
    pub cursor: [f32; 2],
    pub left_clicked: bool,
    pub left_held: bool,
    pub keys: [u64; 4],
}

impl InputSnapshotView {
    fn from_snapshot(snapshot: &prism_project_api::InputSnapshot) -> Self {
        Self {
            cursor: snapshot.cursor,
            left_clicked: snapshot.left_clicked,
            left_held: snapshot.left_held,
            keys: snapshot.keys,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub path: String,
    pub left: String,
    pub right: String,
}

impl std::fmt::Display for Diff {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {} != {}", self.path, self.left, self.right)
    }
}

fn component_names(world: &World, entity: prism_engine::ecs::Entity) -> Vec<String> {
    let mut names: Vec<String> = world
        .iter_component_types()
        .filter(|(type_id, _)| world.has_component(entity, *type_id))
        .map(|(_, name)| name.to_string())
        .collect();
    names.sort();
    names
}

fn animation_state(world: &World, entity: prism_engine::ecs::Entity) -> Option<AnimationState> {
    let clip = world.get::<ClipPlayhead>(entity);
    let sprite = world.get::<SpriteAnimationState>(entity);
    let playing = world
        .get::<prism_engine::scene::components::KeyframeAnimation>(entity)
        .map(|clip| clip.playing);
    let frame = world
        .get::<prism_engine::scene::components::SpriteRenderer>(entity)
        .map(|renderer| renderer.frame_index);
    if clip.is_none() && sprite.is_none() && playing.is_none() && frame.is_none() {
        return None;
    }
    Some(AnimationState {
        clip_time: clip.map(|head| head.time),
        clip_finished: clip.map(|head| head.finished),
        clip_playing: playing,
        sprite_time: sprite.map(|state| state.time),
        sprite_frame: frame,
    })
}

fn ui_quads(list: &UiDrawList) -> Vec<UiQuadState> {
    list.items
        .iter()
        .filter_map(|item| match item {
            UiDrawItem::Quad(quad) => Some(UiQuadState {
                rect: quad.rect,
                color: quad.color,
                order: quad.order,
                layer: quad.layer,
            }),
            UiDrawItem::Text(_) => None,
        })
        .collect()
}

fn diff_value(path: &str, left: &Value, right: &Value, tolerance: f32, out: &mut Vec<Diff>) {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            let left_number = left.as_f64().unwrap_or(f64::NAN);
            let right_number = right.as_f64().unwrap_or(f64::NAN);
            if !numbers_match(left_number, right_number, tolerance) {
                out.push(Diff {
                    path: path.to_string(),
                    left: left.to_string(),
                    right: right.to_string(),
                });
            }
        }
        (Value::Object(left), Value::Object(right)) => {
            let mut keys: Vec<&String> = left.keys().chain(right.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                let next = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => {
                        diff_value(&next, left, right, tolerance, out);
                    }
                    (left, right) => out.push(Diff {
                        path: next,
                        left: left
                            .map(Value::to_string)
                            .unwrap_or_else(|| "<missing>".into()),
                        right: right
                            .map(Value::to_string)
                            .unwrap_or_else(|| "<missing>".into()),
                    }),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                out.push(Diff {
                    path: path.to_string(),
                    left: format!("len {}", left.len()),
                    right: format!("len {}", right.len()),
                });
            }
            for (index, (left, right)) in left.iter().zip(right.iter()).enumerate() {
                diff_value(&format!("{path}[{index}]"), left, right, tolerance, out);
            }
        }
        _ => {
            if left != right {
                out.push(Diff {
                    path: path.to_string(),
                    left: left.to_string(),
                    right: right.to_string(),
                });
            }
        }
    }
}

fn numbers_match(left: f64, right: f64, tolerance: f32) -> bool {
    if left == right {
        return true;
    }
    if !left.is_finite() || !right.is_finite() {
        return false;
    }
    (left - right).abs() <= tolerance.max(0.0) as f64
}

fn unit_from_bits(bits: u64) -> f32 {
    // 24 high-quality random bits mapped to [-0.5, 0.5).
    ((bits >> 40) as f32 / (1u32 << 24) as f32) - 0.5
}

fn splitmix64(state: u64) -> u64 {
    let mut state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = (state ^ (state >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

fn next_harness_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Helper used by the hierarchy scenario: parent `child` under `parent`.
pub fn reparent(
    world: &mut World,
    child: prism_engine::ecs::Entity,
    parent: prism_engine::ecs::Entity,
) {
    HierarchyHelper::reparent(world, child, Some(parent));
}
