//! Foot placement in the animator (#560 P4).
//!
//! [`prism_anim_ik::foot::FootPlacement`] works purely on a [`Pose`], which is
//! expressed in the skeleton root's space (the entity's `LocalToWorld` is
//! applied later by the shader). Colliders, on the other hand, live in world
//! space, so the ground probe has to go through the entity transform. That
//! conversion is what [`ModelGroundQuery`] does; the rest of this module is
//! bookkeeping: resolve authored bone names, build the settings, keep the
//! smoothing state across frames.
//!
//! Per-frame state is engine-owned (like the animator playhead): it is inserted
//! by the animator system and is not authored or reflected.

use std::collections::HashMap;

use glam::{Mat4, Vec3};
use prism_anim::{Pose, Skeleton};
use prism_anim_ik::foot::{FootPlacement, FootPlacementSettings, GroundHit, GroundQuery};
use prism_anim_ik::two_bone::TwoBoneChain;
use prism_ecs::{Entity, World};
use prism_scene::components::{FootIk, LocalToWorld};

use crate::anim_ground::SceneGroundQuery;

/// Per-entity smoothing state, kept between frames.
#[derive(Debug, Clone, Default)]
pub struct FootIkState {
    pub placement: FootPlacement,
}

impl FootIkState {
    pub fn new() -> Self {
        Self::default()
    }
}

fn joint(skeleton: &Skeleton, name: &str) -> Option<usize> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    skeleton.joint_index(name)
}

fn chain(skeleton: &Skeleton, names: [&str; 3]) -> Option<TwoBoneChain> {
    Some(TwoBoneChain::new(
        joint(skeleton, names[0])?,
        joint(skeleton, names[1])?,
        joint(skeleton, names[2])?,
    ))
}

fn settings(component: &FootIk) -> FootPlacementSettings {
    let defaults = FootPlacementSettings::default();
    FootPlacementSettings {
        probe_height: component.probe_height,
        max_step_up: component.max_step_up,
        max_step_down: component.max_step_down,
        foot_offset: component.foot_offset,
        pelvis_min: component.pelvis_min,
        blend_seconds: component.blend_seconds,
        weight: component.weight,
        leg_pole: defaults.leg_pole,
    }
}

/// Answers foot probes in skeleton-root space by casting in world space.
///
/// The pose the solver hands in is model space, so origins, directions and the
/// reach are transformed with the entity matrix and the hit is mapped back.
/// A uniform model scale is assumed (the probe reach is scaled by the direction
/// length, which is the model scale).
struct ModelGroundQuery<'a> {
    world: &'a World,
    to_world: Mat4,
    to_model: Mat4,
    layer_mask: u32,
    ignore: Entity,
}

impl GroundQuery for ModelGroundQuery<'_> {
    fn cast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<GroundHit> {
        let origin = self.to_world.transform_point3(origin);
        let direction = self.to_world.transform_vector3(direction);
        let scale = direction.length();
        if !scale.is_finite() || scale <= 1e-6 || !(max_distance > 0.0) {
            return None;
        }
        let hit = SceneGroundQuery::new(self.world, self.layer_mask)
            .excluding(self.ignore)
            .cast(origin, direction / scale, max_distance * scale)?;
        Some(GroundHit {
            point: self.to_model.transform_point3(hit.point),
            normal: self
                .to_model
                .transform_vector3(hit.normal)
                .normalize_or_zero(),
        })
    }
}

/// Run foot placement for one skeleton evaluated this frame.
///
/// Returns `false` and leaves `pose` untouched when the entity has no
/// [`FootIk`], the component is disabled, or any authored bone name does not
/// resolve in `skeleton`.
pub fn apply_foot_ik(
    world: &World,
    entity: Entity,
    skeleton: &Skeleton,
    pose: &mut Pose,
    state: &mut FootIkState,
    dt: f32,
) -> bool {
    let Some(component) = world.get::<FootIk>(entity) else {
        return false;
    };
    if !component.enabled || !(component.weight > 0.0) {
        return false;
    }
    if pose.len() != skeleton.joint_count() {
        return false;
    }
    let Some(hips) = joint(skeleton, &component.hips_joint) else {
        return false;
    };
    let Some(left) = chain(
        skeleton,
        [
            &component.left_thigh,
            &component.left_shin,
            &component.left_foot,
        ],
    ) else {
        return false;
    };
    let Some(right) = chain(
        skeleton,
        [
            &component.right_thigh,
            &component.right_shin,
            &component.right_foot,
        ],
    ) else {
        return false;
    };

    let model = world
        .get::<LocalToWorld>(entity)
        .map(|transform| transform.0)
        .unwrap_or(Mat4::IDENTITY);
    let ground = ModelGroundQuery {
        world,
        to_world: model,
        to_model: model.inverse(),
        layer_mask: component.ground_layer,
        ignore: entity,
    };
    state
        .placement
        .update(
            skeleton,
            pose,
            hips,
            [left, right],
            &settings(component),
            &ground,
            dt,
        )
        .applied
}

/// Run foot placement for `entity` while streaming the animator over the world.
///
/// Loads the state on first sight and reports whether the solve applied; the
/// caller inserts the map back into the world when the pass is done.
pub fn apply_for_entity(
    world: &World,
    entity: Entity,
    skeleton: &Skeleton,
    pose: &mut Pose,
    states: &mut HashMap<Entity, FootIkState>,
    dt: f32,
) -> bool {
    let state = states.entry(entity).or_default();
    apply_foot_ik(world, entity, skeleton, pose, state, dt)
}

#[cfg(test)]
mod tests {
    use glam::{Mat4, Quat, Vec3};
    use prism_anim::skeleton::Joint;
    use prism_anim::{JointTransform, Pose, Skeleton};
    use prism_ecs::World;
    use prism_scene::components::{Collider, ColliderShape, FootIk, LocalToWorld, LocalTransform};

    use super::{apply_foot_ik, FootIkState};

    /// hips (y = 1.0) with a straight left leg down to an ankle at y = 0.4 and a
    /// shorter right leg; both legs hang from the pelvis.
    fn skeleton() -> Skeleton {
        let joint = |name: &str, parent: Option<usize>, at: Vec3| Joint {
            name: name.into(),
            parent,
            reference_local: JointTransform::new(at, Quat::IDENTITY, Vec3::ONE),
            inverse_bind: Mat4::IDENTITY,
        };
        Skeleton::new(vec![
            joint("hips", None, Vec3::new(0.0, 1.0, 0.0)),
            joint("left_thigh", Some(0), Vec3::new(0.2, 0.0, 0.0)),
            joint("left_shin", Some(1), Vec3::new(0.0, -0.3, 0.0)),
            joint("left_foot", Some(2), Vec3::new(0.0, -0.3, 0.0)),
            joint("right_thigh", Some(0), Vec3::new(-0.2, 0.0, 0.0)),
            joint("right_shin", Some(4), Vec3::new(0.0, -0.3, 0.0)),
            joint("right_foot", Some(5), Vec3::new(0.0, -0.3, 0.0)),
        ])
        .expect("valid hierarchy")
    }

    fn character(world: &mut World, bevel: Vec3) -> prism_ecs::Entity {
        let entity = world.spawn();
        world.insert(
            entity,
            FootIk {
                hips_joint: "hips".into(),
                left_thigh: "left_thigh".into(),
                left_shin: "left_shin".into(),
                left_foot: "left_foot".into(),
                right_thigh: "right_thigh".into(),
                right_shin: "right_shin".into(),
                right_foot: "right_foot".into(),
                ..FootIk::default()
            },
        );
        world.insert(
            entity,
            LocalTransform {
                position: bevel,
                ..LocalTransform::default()
            },
        );
        world.insert(entity, LocalToWorld(Mat4::from_translation(bevel)));
        entity
    }

    fn floor(world: &mut World) {
        let entity = world.spawn();
        world.insert(
            entity,
            Collider {
                shape: ColliderShape::Box,
                half_extents: [20.0, 0.1, 20.0],
                layer: 1,
                ..Collider::default()
            },
        );
        world.insert(
            entity,
            LocalToWorld(Mat4::from_translation(Vec3::new(0.0, -0.1, 0.0))),
        );
    }

    fn ankle_y(skeleton: &Skeleton, pose: &Pose, name: &str) -> f32 {
        let index = skeleton.joint_index(name).expect("joint");
        skeleton.world_matrices(pose)[index].w_axis.y
    }

    #[test]
    fn a_planted_foot_reaches_the_ground() {
        let skeleton = skeleton();
        let mut world = World::new();
        floor(&mut world);
        let entity = character(&mut world, Vec3::ZERO);
        let mut pose = skeleton.rest_pose();
        let rest = ankle_y(&skeleton, &pose, "left_foot");
        let mut state = FootIkState::new();
        let mut applied = false;
        for _ in 0..30 {
            applied |= apply_foot_ik(&world, entity, &skeleton, &mut pose, &mut state, 1.0 / 60.0);
        }
        assert!(applied, "foot placement should apply");
        let planted = ankle_y(&skeleton, &pose, "left_foot");
        assert!(planted < rest - 0.1, "rest {rest} -> {planted}");
        assert!(planted > 0.0, "{planted} must stay above the floor");
    }

    #[test]
    fn without_the_component_the_pose_is_untouched() {
        let skeleton = skeleton();
        let mut world = World::new();
        floor(&mut world);
        let entity = world.spawn();
        let mut pose = skeleton.rest_pose();
        let before = pose.clone();
        let mut state = FootIkState::new();
        assert!(!apply_foot_ik(
            &world,
            entity,
            &skeleton,
            &mut pose,
            &mut state,
            1.0 / 60.0
        ));
        assert_eq!(pose, before);
    }

    #[test]
    fn unresolved_bone_names_leave_the_pose_untouched() {
        let skeleton = skeleton();
        let mut world = World::new();
        floor(&mut world);
        let entity = character(&mut world, Vec3::ZERO);
        if let Some(component) = world.get_mut::<FootIk>(entity) {
            component.left_foot = "not_a_joint".into();
        }
        let mut pose = skeleton.rest_pose();
        let before = pose.clone();
        let mut state = FootIkState::new();
        assert!(!apply_foot_ik(
            &world,
            entity,
            &skeleton,
            &mut pose,
            &mut state,
            1.0 / 60.0
        ));
        assert_eq!(pose, before);
    }

    #[test]
    fn the_entity_transform_places_the_probe_in_world_space() {
        let skeleton = skeleton();
        let mut world = World::new();
        floor(&mut world);
        // Lifting the character by 5 m puts the ground far below the ankles.
        let entity = character(&mut world, Vec3::new(0.0, 5.0, 0.0));
        let mut pose = skeleton.rest_pose();
        let before = pose.clone();
        let mut state = FootIkState::new();
        for _ in 0..10 {
            apply_foot_ik(&world, entity, &skeleton, &mut pose, &mut state, 1.0 / 60.0);
        }
        assert_eq!(pose, before, "no ground within reach, no plant");
    }
}
