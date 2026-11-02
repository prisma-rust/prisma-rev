//! Data-driven physics scenario harness.
//!
//! Build a [`Scenario`] from [`BodySpec`] values, step the real
//! `prism_app::PhysicsRuntime` (rapier on its own thread) a fixed number of
//! times, and assert on poses / sleeping state / collision events with explicit
//! tolerances. No window, renderer or wall-clock time is involved.

#![allow(dead_code)] // Reusable helper surface; individual scenarios use a subset.

use std::collections::{BTreeMap, HashMap};

use prism_app::PhysicsRuntime;
use prism_engine::ecs::{Entity, World};
use prism_engine::physics::{PhysicsEvent, PhysicsEventKind, PhysicsEvents};
use prism_engine::scene::components::{
    BodyType, Collider, ColliderShape, LocalToWorld, LocalTransform, Rigidbody,
};
use prism_engine::scene::helpers::HierarchyHelper;
use prism_engine::scene::systems::hierarchy::hierarchy_system;

/// Collider shape plus its local-space centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShapeSpec {
    Box {
        half_extents: [f32; 3],
        center: [f32; 3],
    },
    Sphere {
        radius: f32,
        center: [f32; 3],
    },
    Capsule {
        half_height: f32,
        radius: f32,
        center: [f32; 3],
    },
}

impl ShapeSpec {
    pub fn boxed(half_extents: [f32; 3]) -> Self {
        Self::Box {
            half_extents,
            center: [0.0; 3],
        }
    }

    pub fn sphere(radius: f32) -> Self {
        Self::Sphere {
            radius,
            center: [0.0; 3],
        }
    }

    pub fn capsule(half_height: f32, radius: f32) -> Self {
        Self::Capsule {
            half_height,
            radius,
            center: [0.0; 3],
        }
    }

    pub fn center(mut self, center: [f32; 3]) -> Self {
        match &mut self {
            Self::Box { center: value, .. }
            | Self::Sphere { center: value, .. }
            | Self::Capsule { center: value, .. } => *value = center,
        }
        self
    }
}

/// One authored body in a scenario.
#[derive(Clone, Debug)]
pub struct BodySpec {
    pub name: String,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    pub body_type: BodyType,
    pub mass: f32,
    pub use_gravity: bool,
    pub ccd: bool,
    pub sensor: bool,
    pub shape: ShapeSpec,
    pub parent: Option<String>,
}

impl BodySpec {
    pub fn dynamic(name: &str, position: [f32; 3]) -> Self {
        Self::new(name, position, BodyType::Dynamic)
    }

    pub fn static_body(name: &str, position: [f32; 3]) -> Self {
        Self::new(name, position, BodyType::Static)
    }

    pub fn kinematic(name: &str, position: [f32; 3]) -> Self {
        Self::new(name, position, BodyType::Kinematic)
    }

    fn new(name: &str, position: [f32; 3], body_type: BodyType) -> Self {
        Self {
            name: name.to_string(),
            position,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            body_type,
            mass: 1.0,
            use_gravity: true,
            ccd: false,
            sensor: false,
            shape: ShapeSpec::boxed([0.5; 3]),
            parent: None,
        }
    }

    pub fn shape(mut self, shape: ShapeSpec) -> Self {
        self.shape = shape;
        self
    }

    pub fn boxed(mut self, half_extents: [f32; 3]) -> Self {
        self.shape = ShapeSpec::boxed(half_extents);
        self
    }

    pub fn sphere(mut self, radius: f32) -> Self {
        self.shape = ShapeSpec::sphere(radius);
        self
    }

    pub fn capsule(mut self, half_height: f32, radius: f32) -> Self {
        self.shape = ShapeSpec::capsule(half_height, radius);
        self
    }

    pub fn ccd(mut self) -> Self {
        self.ccd = true;
        self
    }

    pub fn sensor(mut self) -> Self {
        self.sensor = true;
        self
    }

    pub fn gravity(mut self, use_gravity: bool) -> Self {
        self.use_gravity = use_gravity;
        self
    }

    pub fn mass(mut self, mass: f32) -> Self {
        self.mass = mass;
        self
    }

    pub fn scale(mut self, scale: [f32; 3]) -> Self {
        self.scale = scale;
        self
    }

    pub fn rotation(mut self, rotation: [f32; 4]) -> Self {
        self.rotation = rotation;
        self
    }

    pub fn parent(mut self, parent: &str) -> Self {
        self.parent = Some(parent.to_string());
        self
    }
}

/// A reusable, data-driven physics scenario.
#[derive(Clone, Debug, Default)]
pub struct Scenario {
    pub bodies: Vec<BodySpec>,
}

impl Scenario {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn body(mut self, body: BodySpec) -> Self {
        self.bodies.push(body);
        self
    }

    /// Apply a deterministic, tiny horizontal jitter to dynamic bodies.
    ///
    /// Useful for breaking perfect symmetry without making a run
    /// irreproducible: the same seed always produces the same offsets.
    pub fn seeded(mut self, seed: u64) -> Self {
        for body in &mut self.bodies {
            if body.body_type != BodyType::Dynamic {
                continue;
            }
            let mut state = seed ^ name_hash(&body.name);
            state = splitmix64(state);
            body.position[0] += signed_unit(state) * 1.0e-3;
            state = splitmix64(state);
            body.position[2] += signed_unit(state) * 1.0e-3;
        }
        self
    }
}

/// Observed pose of one body after a step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyPose {
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    /// `None` for static / kinematic bodies (rapier only reports dynamic ones).
    pub sleeping: Option<bool>,
}

/// One collision / trigger event, with stable body names and the step it fired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRecord {
    pub step: u32,
    pub kind: PhysicsEventKind,
    pub body_a: String,
    pub body_b: String,
    pub is_sensor: bool,
}

impl EventRecord {
    pub fn involves(&self, a: &str, b: &str) -> bool {
        (self.body_a == a && self.body_b == b) || (self.body_a == b && self.body_b == a)
    }
}

/// Per-step observation.
#[derive(Clone, Debug, PartialEq)]
pub struct StepSnapshot {
    pub step: u32,
    pub poses: BTreeMap<String, BodyPose>,
    pub events: Vec<EventRecord>,
}

/// Drives a real [`PhysicsRuntime`] over a temporary ECS world.
pub struct PhysicsHarness {
    world: World,
    runtime: PhysicsRuntime,
    entities: BTreeMap<String, Entity>,
    names: HashMap<Entity, String>,
    step: u32,
    events: Vec<EventRecord>,
}

impl PhysicsHarness {
    pub fn new(scenario: Scenario) -> Self {
        let mut world = World::new();
        let mut entities = BTreeMap::new();
        let mut names = HashMap::new();

        for body in &scenario.bodies {
            let entity = world.spawn();
            world.insert(
                entity,
                LocalTransform {
                    position: glam::Vec3::from_array(body.position),
                    rotation: glam::Quat::from_array(body.rotation),
                    scale: glam::Vec3::from_array(body.scale),
                },
            );
            world.insert(
                entity,
                Rigidbody {
                    body_type: body.body_type,
                    mass: body.mass,
                    use_gravity: body.use_gravity,
                    ccd: body.ccd,
                    ..Rigidbody::default()
                },
            );
            world.insert(entity, collider(&body.shape, body.sensor));
            entities.insert(body.name.clone(), entity);
            names.insert(entity, body.name.clone());
        }

        for body in &scenario.bodies {
            let Some(parent_name) = &body.parent else {
                continue;
            };
            let child = entities[&body.name];
            let parent = *entities
                .get(parent_name)
                .unwrap_or_else(|| panic!("unknown parent {parent_name:?} for {:?}", body.name));
            HierarchyHelper::reparent(&mut world, child, Some(parent));
        }

        hierarchy_system(&mut world);

        Self {
            runtime: PhysicsRuntime::spawn(),
            world,
            entities,
            names,
            step: 0,
            events: Vec::new(),
        }
    }

    pub fn entity(&self, name: &str) -> Entity {
        *self
            .entities
            .get(name)
            .unwrap_or_else(|| panic!("body {name:?} not found in scenario"))
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    pub fn step_count(&self) -> u32 {
        self.step
    }

    /// Advance one fixed physics step and capture the resulting state.
    pub fn step(&mut self) -> StepSnapshot {
        let next = self.step + 1;
        assert!(
            self.runtime.step(&mut self.world),
            "physics thread stopped before step {next}"
        );
        hierarchy_system(&mut self.world);
        self.step = next;

        let mut events = Vec::new();
        if let Some(queue) = self.world.get_resource::<PhysicsEvents>() {
            for event in &queue.events {
                let record = self.record(next, *event);
                events.push(record.clone());
                self.events.push(record);
            }
        }

        let mut poses = BTreeMap::new();
        for (name, entity) in &self.entities {
            poses.insert(name.clone(), self.pose_of(*entity));
        }
        StepSnapshot {
            step: next,
            poses,
            events,
        }
    }

    /// Advance `steps` fixed steps, returning one snapshot per step.
    pub fn run(&mut self, steps: u32) -> Vec<StepSnapshot> {
        (0..steps).map(|_| self.step()).collect()
    }

    pub fn pose(&self, name: &str) -> BodyPose {
        self.pose_of(self.entity(name))
    }

    pub fn world_position(&self, name: &str) -> Option<[f32; 3]> {
        self.world
            .get::<LocalToWorld>(self.entity(name))
            .map(|matrix| matrix.0.transform_point3(glam::Vec3::ZERO).to_array())
    }

    /// All events observed so far (including earlier steps).
    pub fn events(&self) -> &[EventRecord] {
        &self.events
    }

    pub fn events_on(&self, step: u32) -> impl Iterator<Item = &EventRecord> {
        self.events.iter().filter(move |event| event.step == step)
    }

    pub fn assert_position_within(&self, name: &str, expected: [f32; 3], tolerance: f32) {
        let actual = self.pose(name).position;
        for (index, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                (actual - expected).abs() <= tolerance,
                "{name}.position[{index}] = {actual}, expected {expected} ± {tolerance}"
            );
        }
    }

    pub fn assert_world_position_within(&self, name: &str, expected: [f32; 3], tolerance: f32) {
        let actual = self
            .world_position(name)
            .unwrap_or_else(|| panic!("{name} has no world transform"));
        for (index, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                (actual - expected).abs() <= tolerance,
                "{name}.world_position[{index}] = {actual}, expected {expected} ± {tolerance}"
            );
        }
    }

    pub fn assert_y_between(&self, name: &str, min: f32, max: f32) {
        let y = self.pose(name).position[1];
        assert!(
            (min..=max).contains(&y),
            "{name}.position[1] = {y}, expected {min}..={max}"
        );
    }

    pub fn assert_sleeping(&self, name: &str) {
        assert_eq!(
            self.pose(name).sleeping,
            Some(true),
            "{name} should have gone to sleep"
        );
    }

    pub fn assert_all_finite(&self) {
        for (name, pose) in self.entities.keys().map(|name| (name, self.pose(name))) {
            assert!(
                pose.position.iter().all(|value| value.is_finite())
                    && pose.rotation.iter().all(|value| value.is_finite()),
                "{name} has a non-finite pose: {pose:?}"
            );
        }
    }

    fn pose_of(&self, entity: Entity) -> BodyPose {
        let transform = self
            .world
            .get::<LocalTransform>(entity)
            .unwrap_or_else(|| panic!("entity {entity:?} has no LocalTransform"));
        BodyPose {
            position: transform.position.to_array(),
            rotation: transform.rotation.to_array(),
            sleeping: self.runtime.is_sleeping(entity),
        }
    }

    fn record(&self, step: u32, event: PhysicsEvent) -> EventRecord {
        EventRecord {
            step,
            kind: event.kind,
            body_a: self.name_of(event.entity_a),
            body_b: self.name_of(event.entity_b),
            is_sensor: event.is_sensor,
        }
    }

    fn name_of(&self, entity: Entity) -> String {
        self.names
            .get(&entity)
            .cloned()
            .unwrap_or_else(|| format!("entity#{}", entity.id()))
    }
}

fn collider(shape: &ShapeSpec, sensor: bool) -> Collider {
    let mut collider = Collider {
        is_trigger: sensor,
        ..Collider::default()
    };
    match *shape {
        ShapeSpec::Box {
            half_extents,
            center,
        } => {
            collider.shape = ColliderShape::Box;
            collider.half_extents = half_extents;
            collider.center = center;
        }
        ShapeSpec::Sphere { radius, center } => {
            collider.shape = ColliderShape::Sphere;
            collider.radius = radius;
            collider.center = center;
        }
        ShapeSpec::Capsule {
            half_height,
            radius,
            center,
        } => {
            collider.shape = ColliderShape::Capsule;
            collider.half_height = half_height;
            collider.radius = radius;
            collider.center = center;
        }
    }
    collider
}

fn name_hash(name: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn splitmix64(state: u64) -> u64 {
    let mut state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = (state ^ (state >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

fn signed_unit(bits: u64) -> f32 {
    ((bits >> 40) as f32 / (1u32 << 24) as f32) - 0.5
}
