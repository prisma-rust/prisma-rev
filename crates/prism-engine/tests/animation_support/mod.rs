//! Deterministic animation sampling and playback harness.
//!
//! Two layers are covered:
//!
//! - [`sample_clip`] samples a `prism_anim::Clip` into a [`PoseSnapshot`] at an
//!   explicit (already wrapped) time, so keyframe interpolation, multi-track
//!   clips and loop boundaries can be compared with a per-component tolerance.
//! - [`AnimationHarness`] drives the real `keyframe_animation_system` at a fixed
//!   timestep and records the playhead, sampled `LocalTransform` and every
//!   `AnimationEventQueue` entry. That makes forward, reverse, paused, looping
//!   and one-shot event sequences observable without an engine loop or GPU.
//!
//! The harness is intentionally test-only: it owns a bare `World` and has no
//! wall-clock dependency.

#![allow(dead_code)] // Reusable helper surface; individual scenarios use a subset.

use glam::{Quat, Vec3};

use prism_anim::{Clip, JointTransform, Pose, QuatKey, Track, Vec3Key as AnimVec3Key};
use prism_engine::ecs::{Entity, World};
use prism_engine::scene::components::{KeyframeAnimation, LocalTransform};
use prism_engine::scene::systems::animation::{
    keyframe_animation_system, AnimationEventQueue, ClipPlayhead,
};
use prism_scene::animation::{AnimationEvent, Vec3Key as SceneVec3Key};

// ---------------------------------------------------------------------------
// Pose sampling (prism-anim)
// ---------------------------------------------------------------------------

/// One joint's sampled local TRS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JointSnapshot {
    pub translation: [f32; 3],
    /// Quaternion in `xyzw` order.
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl JointSnapshot {
    pub fn new(translation: [f32; 3], rotation: Quat, scale: [f32; 3]) -> Self {
        Self {
            translation,
            rotation: rotation.to_array(),
            scale,
        }
    }
}

/// A sampled pose, one entry per joint.
#[derive(Clone, Debug, PartialEq)]
pub struct PoseSnapshot {
    pub joints: Vec<JointSnapshot>,
}

impl PoseSnapshot {
    pub fn from_pose(pose: &Pose) -> Self {
        let joints = pose
            .locals()
            .iter()
            .map(|local| JointSnapshot {
                translation: local.translation.to_array(),
                rotation: local.rotation.to_array(),
                scale: local.scale.to_array(),
            })
            .collect();
        Self { joints }
    }

    pub fn joint(&self, index: usize) -> JointSnapshot {
        *self
            .joints
            .get(index)
            .unwrap_or_else(|| panic!("joint {index} not in snapshot"))
    }
}

/// A two-joint base pose used by the sampling scenarios.
pub fn identity_pose(joints: usize) -> Pose {
    Pose::from_locals(vec![JointTransform::IDENTITY; joints])
}

/// Sample `clip` at `time` using its own loop/clamp policy and return the pose.
pub fn sample_clip(clip: &Clip, time: f32, base: &Pose) -> PoseSnapshot {
    let mut pose = base.clone();
    clip.sample_wrapped_into(time, &mut pose);
    PoseSnapshot::from_pose(&pose)
}

/// Assert two poses match component-wise within `tolerance`.
///
/// Rotations are compared after normalizing the quaternion sign, so `q` and
/// `-q` (the same rotation) do not produce a false failure.
pub fn assert_pose_close(actual: &PoseSnapshot, expected: &PoseSnapshot, tolerance: f32) {
    assert_eq!(
        actual.joints.len(),
        expected.joints.len(),
        "pose joint count differs"
    );
    for (index, (actual, expected)) in actual.joints.iter().zip(&expected.joints).enumerate() {
        for axis in 0..3 {
            assert_close(
                &format!("joint {index} translation[{axis}]"),
                actual.translation[axis],
                expected.translation[axis],
                tolerance,
            );
            assert_close(
                &format!("joint {index} scale[{axis}]"),
                actual.scale[axis],
                expected.scale[axis],
                tolerance,
            );
        }
        let dot: f32 = actual
            .rotation
            .iter()
            .zip(expected.rotation.iter())
            .map(|(a, b)| a * b)
            .sum();
        let sign = if dot < 0.0 { -1.0 } else { 1.0 };
        for axis in 0..4 {
            assert_close(
                &format!("joint {index} rotation[{axis}]"),
                actual.rotation[axis],
                sign * expected.rotation[axis],
                tolerance,
            );
        }
    }
}

fn assert_close(label: &str, actual: f32, expected: f32, tolerance: f32) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{label}: actual {actual} != expected {expected} (tolerance {tolerance})"
    );
}

fn anim_vec3(time: f32, value: [f32; 3]) -> AnimVec3Key {
    AnimVec3Key {
        time,
        value: Vec3::from_array(value),
    }
}

fn anim_quat(time: f32, value: Quat) -> QuatKey {
    QuatKey { time, value }
}

/// Multi-track `prism-anim` fixture:
///
/// - joint 0: translation, rotation and scale keys (0→2s);
/// - joint 1: translation and scale keys (0→2s).
pub fn pose_fixture(looped: bool) -> Clip {
    Clip {
        name: "multi-track".to_string(),
        duration: 2.0,
        fps: 30.0,
        looped,
        additive: false,
        tracks: vec![
            Track {
                joint: 0,
                translation: vec![
                    anim_vec3(0.0, [0.0, 0.0, 0.0]),
                    anim_vec3(1.0, [4.0, 0.0, 0.0]),
                    anim_vec3(2.0, [4.0, 2.0, 0.0]),
                ],
                rotation: vec![
                    anim_quat(0.0, Quat::IDENTITY),
                    anim_quat(2.0, Quat::from_rotation_y(std::f32::consts::FRAC_PI_2)),
                ],
                scale: vec![
                    anim_vec3(0.0, [1.0, 1.0, 1.0]),
                    anim_vec3(2.0, [2.0, 2.0, 2.0]),
                ],
            },
            Track {
                joint: 1,
                translation: vec![
                    anim_vec3(0.0, [0.0, 1.0, 0.0]),
                    anim_vec3(2.0, [0.0, 3.0, 0.0]),
                ],
                rotation: Vec::new(),
                scale: vec![
                    anim_vec3(0.0, [1.0, 1.0, 1.0]),
                    anim_vec3(1.0, [1.0, 1.0, 3.0]),
                    anim_vec3(2.0, [1.0, 1.0, 1.0]),
                ],
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Playback + events (engine)
// ---------------------------------------------------------------------------

/// One engine tick's observable animation state.
#[derive(Clone, Debug, PartialEq)]
pub struct Tick {
    pub step: u32,
    pub playhead_time: f32,
    pub finished: bool,
    pub position: [f32; 3],
    /// Quaternion in `xyzw` order.
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    pub events: Vec<FiredEvent>,
}

/// An event emitted on a specific playback tick.
#[derive(Clone, Debug, PartialEq)]
pub struct FiredEvent {
    pub step: u32,
    pub name: String,
    pub time: f32,
}

/// Fixed-step driver around `keyframe_animation_system`.
pub struct AnimationHarness {
    world: World,
    entity: Entity,
    step: f32,
    tick: u32,
    ticks: Vec<Tick>,
}

impl AnimationHarness {
    /// Spawn one entity with a `LocalTransform` and `clip`.
    pub fn new(clip: KeyframeAnimation, step: f32) -> Self {
        assert!(step > 0.0 && step.is_finite(), "step must be positive");
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(entity, LocalTransform::default());
        world.insert(entity, clip);
        Self {
            world,
            entity,
            step,
            tick: 0,
            ticks: Vec::new(),
        }
    }

    /// Seed the runtime playhead, e.g. to start a reverse clip at its end.
    pub fn with_playhead(mut self, time: f32) -> Self {
        self.world.insert(
            self.entity,
            ClipPlayhead {
                time,
                finished: false,
            },
        );
        self
    }

    pub fn entity(&self) -> Entity {
        self.entity
    }

    pub fn step(&self) -> f32 {
        self.step
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    pub fn clip(&self) -> &KeyframeAnimation {
        self.world
            .get::<KeyframeAnimation>(self.entity)
            .expect("harness entity keeps its clip")
    }

    pub fn clip_mut(&mut self) -> &mut KeyframeAnimation {
        self.world
            .get_mut::<KeyframeAnimation>(self.entity)
            .expect("harness entity keeps its clip")
    }

    pub fn ticks(&self) -> &[Tick] {
        &self.ticks
    }

    pub fn last(&self) -> &Tick {
        self.ticks.last().expect("no tick has been run yet")
    }

    /// Advance exactly one fixed step and capture the result.
    pub fn tick(&mut self) -> Tick {
        self.tick += 1;
        keyframe_animation_system(&mut self.world, self.step);

        let transform = self
            .world
            .get::<LocalTransform>(self.entity)
            .expect("harness entity keeps its transform")
            .clone();
        let head = self
            .world
            .get::<ClipPlayhead>(self.entity)
            .copied()
            .unwrap_or_default();
        let events = self
            .world
            .get_resource::<AnimationEventQueue>()
            .map(|queue| {
                queue
                    .events
                    .iter()
                    .filter(|event| event.entity == self.entity)
                    .map(|event| FiredEvent {
                        step: self.tick,
                        name: event.name.clone(),
                        time: event.time,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let tick = Tick {
            step: self.tick,
            playhead_time: head.time,
            finished: head.finished,
            position: transform.position.to_array(),
            rotation: transform.rotation.to_array(),
            scale: transform.scale.to_array(),
            events,
        };
        self.ticks.push(tick.clone());
        tick
    }

    /// Advance `count` fixed steps and return the captured ticks.
    pub fn run(&mut self, count: usize) -> Vec<Tick> {
        let start = self.ticks.len();
        for _ in 0..count {
            self.tick();
        }
        self.ticks[start..].to_vec()
    }

    /// Every event emitted so far, flattened in emission order.
    pub fn events(&self) -> Vec<FiredEvent> {
        self.ticks
            .iter()
            .flat_map(|tick| tick.events.iter().cloned())
            .collect()
    }

    /// Event names paired with the tick that emitted them.
    pub fn event_names(&self) -> Vec<(u32, String)> {
        self.events()
            .into_iter()
            .map(|event| (event.step, event.name))
            .collect()
    }
}

fn scene_vec3(time: f32, value: [f32; 3]) -> SceneVec3Key {
    SceneVec3Key::new(time, value)
}

/// Engine clip fixture with position/euler/scale keys over a one-second clip.
///
/// `events` is explicit so a reverse scenario can omit the t=0 cue.
pub fn engine_clip(looped: bool, speed: f32, events: Vec<AnimationEvent>) -> KeyframeAnimation {
    KeyframeAnimation {
        name: "engine-fixture".to_string(),
        duration: 1.0,
        speed,
        looped,
        playing: true,
        phase: 0.0,
        position: vec![
            scene_vec3(0.0, [0.0, 0.0, 0.0]),
            scene_vec3(1.0, [10.0, 0.0, 0.0]),
        ],
        euler_xyz: vec![
            scene_vec3(0.0, [0.0, 0.0, 0.0]),
            scene_vec3(1.0, [0.0, 90.0, 0.0]),
        ],
        scale: vec![
            scene_vec3(0.0, [1.0, 1.0, 1.0]),
            scene_vec3(1.0, [2.0, 2.0, 2.0]),
        ],
        events,
    }
}

/// The default forward fixture's event set: `start` at 0, then quarter/half/late.
pub fn engine_events() -> Vec<AnimationEvent> {
    vec![
        AnimationEvent::new(0.0, "start"),
        AnimationEvent::new(0.25, "quarter"),
        AnimationEvent::new(0.5, "half"),
        AnimationEvent::new(0.9, "late"),
    ]
}
