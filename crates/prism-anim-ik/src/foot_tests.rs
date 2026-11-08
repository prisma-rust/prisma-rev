//! Foot placement tests: flat ground, a step, holes, walls and the pose guards.

use glam::{Mat4, Quat, Vec3};

use prism_anim::{Joint, JointTransform, Pose, Skeleton};

use crate::foot::{FootPlacement, FootPlacementSettings, GroundHit, GroundQuery};
use crate::two_bone::TwoBoneChain;

/// Pelvis at (0, 1, 0), two legs at x = ±0.15: thigh at 0.95, knee at 0.50,
/// ankle at 0.10 (bone lengths 0.45 / 0.40).
fn skeleton() -> Skeleton {
    let joint = |name: &str, parent: Option<usize>, translation: Vec3| Joint {
        name: name.into(),
        parent,
        reference_local: JointTransform::new(translation, Quat::IDENTITY, Vec3::ONE),
        inverse_bind: Mat4::IDENTITY,
    };
    Skeleton::new(vec![
        joint("hips", None, Vec3::new(0.0, 1.0, 0.0)),
        joint("thigh_l", Some(0), Vec3::new(-0.15, -0.05, 0.0)),
        joint("knee_l", Some(1), Vec3::new(0.0, -0.45, 0.0)),
        joint("ankle_l", Some(2), Vec3::new(0.0, -0.40, 0.0)),
        joint("thigh_r", Some(0), Vec3::new(0.15, -0.05, 0.0)),
        joint("knee_r", Some(4), Vec3::new(0.0, -0.45, 0.0)),
        joint("ankle_r", Some(5), Vec3::new(0.0, -0.40, 0.0)),
    ])
    .expect("valid skeleton")
}

const LEGS: [TwoBoneChain; 2] = [TwoBoneChain::new(1, 2, 3), TwoBoneChain::new(4, 5, 6)];

/// Horizontal plane at a given height.
struct FlatGround {
    height: f32,
}

impl GroundQuery for FlatGround {
    fn cast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<GroundHit> {
        if direction.y >= 0.0 {
            return None;
        }
        let distance = (origin.y - self.height) / -direction.y;
        (distance >= 0.0 && distance <= max_distance).then_some(GroundHit {
            point: Vec3::new(origin.x, self.height, origin.z),
            normal: Vec3::Y,
        })
    }
}

/// Nothing to stand on.
struct EmptyGround;

impl GroundQuery for EmptyGround {
    fn cast(&self, _: Vec3, _: Vec3, _: f32) -> Option<GroundHit> {
        None
    }
}

/// Higher plane on the left half, flat on the right.
struct StepGround;

impl GroundQuery for StepGround {
    fn cast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<GroundHit> {
        let height = if origin.x < 0.0 { 0.2 } else { 0.0 };
        FlatGround { height }.cast(origin, direction, max_distance)
    }
}

/// Reports ground far above the probe (a ceiling / wall face).
struct CeilingGround;

impl GroundQuery for CeilingGround {
    fn cast(&self, origin: Vec3, _: Vec3, _: f32) -> Option<GroundHit> {
        Some(GroundHit {
            point: origin + Vec3::new(0.0, 0.6, 0.0),
            normal: Vec3::Y,
        })
    }
}

fn ankle_y(skeleton: &Skeleton, pose: &Pose, joint: usize) -> f32 {
    skeleton.world_matrices(pose)[joint]
        .to_scale_rotation_translation()
        .2
        .y
}

fn run(
    placement: &mut FootPlacement,
    skeleton: &Skeleton,
    pose: &mut Pose,
    settings: &FootPlacementSettings,
    ground: &dyn GroundQuery,
    frames: usize,
) {
    for _ in 0..frames {
        placement.update(skeleton, pose, 0, LEGS, settings, ground, 1.0 / 60.0);
    }
}

#[test]
fn flat_ground_plants_both_feet() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let mut placement = FootPlacement::new();
    let settings = FootPlacementSettings::default();
    run(
        &mut placement,
        &skeleton,
        &mut pose,
        &settings,
        &FlatGround { height: 0.0 },
        90,
    );
    for leg in LEGS {
        let state = placement.foot(legs_index(leg)).expect("two feet");
        assert!(state.planted);
        assert!((state.weight - 1.0).abs() < 1e-3);
        let y = ankle_y(&skeleton, &pose, leg.end);
        assert!((y - settings.foot_offset).abs() < 0.03, "ankle y = {y}");
    }
}

fn legs_index(leg: TwoBoneChain) -> usize {
    if leg.end == LEGS[0].end {
        0
    } else {
        1
    }
}

#[test]
fn a_step_lifts_one_foot_and_keeps_the_other_down() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let mut placement = FootPlacement::new();
    let settings = FootPlacementSettings::default();
    run(
        &mut placement,
        &skeleton,
        &mut pose,
        &settings,
        &StepGround,
        90,
    );
    let left = ankle_y(&skeleton, &pose, LEGS[0].end);
    let right = ankle_y(&skeleton, &pose, LEGS[1].end);
    assert!(
        (left - (0.2 + settings.foot_offset)).abs() < 0.03,
        "left = {left}"
    );
    assert!(
        (right - settings.foot_offset).abs() < 0.03,
        "right = {right}"
    );
}

#[test]
fn no_ground_unplants_both_feet() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let mut placement = FootPlacement::new();
    let settings = FootPlacementSettings::default();
    run(
        &mut placement,
        &skeleton,
        &mut pose,
        &settings,
        &FlatGround { height: 0.0 },
        30,
    );
    run(
        &mut placement,
        &skeleton,
        &mut pose,
        &settings,
        &EmptyGround,
        90,
    );
    for index in 0..2 {
        let state = placement.foot(index).expect("two feet");
        assert!(!state.planted);
        assert!(state.weight < 0.01);
    }
}

#[test]
fn a_surface_above_the_step_limit_is_not_ground() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let mut placement = FootPlacement::new();
    let settings = FootPlacementSettings::default();
    let report = placement.update(
        &skeleton,
        &mut pose,
        0,
        LEGS,
        &settings,
        &CeilingGround,
        1.0 / 60.0,
    );
    assert!(report.applied);
    for index in 0..2 {
        assert!(!report.feet[index].planted);
    }
}

#[test]
fn pelvis_correction_is_clamped_to_the_authored_limits() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let mut placement = FootPlacement::new();
    let settings = FootPlacementSettings {
        pelvis_min: -0.01,
        blend_seconds: 0.0,
        ..FootPlacementSettings::default()
    };
    let report = placement.update(
        &skeleton,
        &mut pose,
        0,
        LEGS,
        &settings,
        &FlatGround { height: -0.3 },
        1.0 / 60.0,
    );
    assert!((report.pelvis_offset + 0.01).abs() < 1e-5);
}

#[test]
fn zero_weight_disables_placement_and_leaves_the_pose_alone() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let before = pose.locals().to_vec();
    let mut placement = FootPlacement::new();
    let settings = FootPlacementSettings {
        weight: 0.0,
        ..FootPlacementSettings::default()
    };
    let report = placement.update(
        &skeleton,
        &mut pose,
        0,
        LEGS,
        &settings,
        &FlatGround { height: 0.0 },
        1.0 / 60.0,
    );
    assert!(report.applied);
    assert_eq!(report.feet[0].weight, 0.0);
    assert_eq!(pose.locals(), before.as_slice());
}

#[test]
fn a_leg_that_does_not_hang_from_the_hips_is_rejected() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let before = pose.locals().to_vec();
    let mut placement = FootPlacement::new();
    let bad = [TwoBoneChain::new(2, 3, 3), LEGS[1]];
    let report = placement.update(
        &skeleton,
        &mut pose,
        0,
        bad,
        &FootPlacementSettings::default(),
        &EmptyGround,
        1.0 / 60.0,
    );
    assert!(!report.applied);
    assert_eq!(pose.locals(), before.as_slice());
}

#[test]
fn a_zero_dt_frame_keeps_the_previous_state() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let mut placement = FootPlacement::new();
    let settings = FootPlacementSettings::default();
    run(
        &mut placement,
        &skeleton,
        &mut pose,
        &settings,
        &FlatGround { height: 0.0 },
        30,
    );
    let before = placement.foot(0).copied().expect("two feet");
    placement.update(&skeleton, &mut pose, 0, LEGS, &settings, &EmptyGround, 0.0);
    let after = placement.foot(0).copied().expect("two feet");
    // dt = 0 snaps the smoothing but the foot is still resolved against the
    // empty ground, so it unplants while keeping the last target position.
    assert_eq!(after.position, before.position);
}
