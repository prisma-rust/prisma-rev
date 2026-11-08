//! Two-bone IK tests: reach, clamping, pole plane, blending and pose write-back.

use glam::{Mat4, Quat, Vec3};

use prism_anim::{Joint, JointTransform, Pose, Skeleton};

use crate::{apply_two_bone_ik, solve_two_bone, TwoBoneChain};

/// Straight chain along -Y: hip at (0, 1, 0), knee at (0, 0.5, 0), foot at origin.
fn chain() -> (Vec3, Vec3, Vec3) {
    (
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, 0.5, 0.0),
        Vec3::ZERO,
    )
}

fn skeleton() -> Skeleton {
    Skeleton::new(vec![
        Joint {
            name: "hip".into(),
            parent: None,
            reference_local: JointTransform::new(
                Vec3::new(0.0, 1.0, 0.0),
                Quat::IDENTITY,
                Vec3::ONE,
            ),
            inverse_bind: Mat4::IDENTITY,
        },
        Joint {
            name: "knee".into(),
            parent: Some(0),
            reference_local: JointTransform::new(
                Vec3::new(0.0, -0.5, 0.0),
                Quat::IDENTITY,
                Vec3::ONE,
            ),
            inverse_bind: Mat4::IDENTITY,
        },
        Joint {
            name: "foot".into(),
            parent: Some(1),
            reference_local: JointTransform::new(
                Vec3::new(0.0, -0.5, 0.0),
                Quat::IDENTITY,
                Vec3::ONE,
            ),
            inverse_bind: Mat4::IDENTITY,
        },
    ])
    .expect("valid chain")
}

fn end_position(pose: &Pose, skeleton: &Skeleton) -> Vec3 {
    skeleton.world_matrices(pose)[2]
        .to_scale_rotation_translation()
        .2
}

#[test]
fn reachable_target_is_hit_exactly() {
    let (upper, middle, end) = chain();
    let target = Vec3::new(0.3, 0.4, 0.2);
    let solution = solve_two_bone(upper, middle, end, target, Vec3::Z, 1.0).expect("solvable");
    assert!(!solution.clamped);
    assert!((solution.end_position - target).length() < 1e-4);
    assert!((solution.reach - 0.7).abs() < 1e-4);
}

#[test]
fn out_of_reach_target_straightens_the_chain() {
    let (upper, middle, end) = chain();
    let target = Vec3::new(0.0, -10.0, 0.0);
    let solution = solve_two_bone(upper, middle, end, target, Vec3::Z, 1.0).expect("solvable");
    assert!(solution.clamped);
    assert!((solution.reach - (1.0 - 1e-4)).abs() < 1e-3);
    // Straight down: the knee sits between hip and foot on the same line.
    assert!(solution.middle_position.x.abs() < 0.02);
    assert!(solution.middle_position.z.abs() < 0.02);
}

#[test]
fn too_close_target_folds_the_chain() {
    // Unequal bones so the folded chain still has a non-trivial minimum reach
    // (0.6 - 0.4 = 0.2); a target inside that radius cannot be reached.
    let upper = Vec3::new(0.0, 1.0, 0.0);
    let middle = Vec3::new(0.0, 0.4, 0.0);
    let end = Vec3::ZERO;
    let target = upper + Vec3::new(0.0, -0.05, 0.0);
    let solution = solve_two_bone(upper, middle, end, target, Vec3::Z, 1.0).expect("solvable");
    assert!(solution.clamped);
    assert!((solution.reach - (0.2 + 1e-4)).abs() < 1e-3);
    assert!((solution.end_position.distance(upper) - solution.reach).abs() < 1e-3);
}

#[test]
fn pole_decides_the_bend_plane() {
    let (upper, middle, end) = chain();
    let target = Vec3::new(0.0, 0.3, 0.0);
    let toward_z = solve_two_bone(upper, middle, end, target, Vec3::Z, 1.0).expect("solvable");
    let toward_x = solve_two_bone(upper, middle, end, target, Vec3::X, 1.0).expect("solvable");
    assert!(toward_z.middle_position.z > 0.1);
    assert!(toward_x.middle_position.x > 0.1);
    assert!((toward_z.end_position - toward_x.end_position).length() < 1e-4);
}

#[test]
fn pole_parallel_to_the_target_falls_back_to_a_stable_axis() {
    let (upper, middle, end) = chain();
    // Target straight down, pole straight down as well: no bend plane.
    let target = Vec3::new(0.0, 0.4, 0.0);
    let solution = solve_two_bone(upper, middle, end, target, Vec3::NEG_Y, 1.0).expect("solvable");
    assert!(solution.middle_position.is_finite());
    assert!((solution.end_position - target).length() < 1e-4);
}

#[test]
fn weight_blends_between_the_pose_and_the_target() {
    let (upper, middle, end) = chain();
    let target = Vec3::new(0.3, 0.4, 0.2);
    let half = solve_two_bone(upper, middle, end, target, Vec3::Z, 0.5).expect("solvable");
    let full = solve_two_bone(upper, middle, end, target, Vec3::Z, 1.0).expect("solvable");
    // Half way: the end sits between the original position and the target.
    assert!(half.end_position.distance(end) > 0.01);
    assert!(half.end_position.distance(target) > full.end_position.distance(target));
    assert_ne!(half.upper_rotation, Quat::IDENTITY);
}

#[test]
fn degenerate_input_is_rejected() {
    let (upper, middle, end) = chain();
    // Zero-length upper bone.
    assert!(solve_two_bone(upper, upper, end, Vec3::ZERO, Vec3::Z, 1.0).is_none());
    // Zero-length lower bone.
    assert!(solve_two_bone(upper, middle, middle, Vec3::ZERO, Vec3::Z, 1.0).is_none());
    // Target on top of the upper joint.
    assert!(solve_two_bone(upper, middle, end, upper, Vec3::Z, 1.0).is_none());
    // Non-finite target / pole.
    assert!(solve_two_bone(upper, middle, end, Vec3::NAN, Vec3::Z, 1.0).is_none());
    // Zero / negative / NaN weight solves nothing.
    assert!(solve_two_bone(upper, middle, end, Vec3::ZERO, Vec3::Z, 0.0).is_none());
    assert!(solve_two_bone(upper, middle, end, Vec3::ZERO, Vec3::Z, -1.0).is_none());
    assert!(solve_two_bone(upper, middle, end, Vec3::ZERO, Vec3::Z, f32::NAN).is_none());
}

#[test]
fn applying_ik_bends_the_pose_onto_the_target() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let target = Vec3::new(0.3, 0.4, 0.2);
    let solution = apply_two_bone_ik(
        &skeleton,
        &mut pose,
        TwoBoneChain::new(0, 1, 2),
        target,
        Vec3::Z,
        1.0,
    )
    .expect("solvable");
    let reached = end_position(&pose, &skeleton);
    assert!((reached - target).length() < 1e-3);
    assert!((solution.end_position - reached).length() < 1e-6);
    // The hip itself does not move (only rotations are written).
    let hip = skeleton.world_matrices(&pose)[0]
        .to_scale_rotation_translation()
        .2;
    assert!((hip - Vec3::new(0.0, 1.0, 0.0)).length() < 1e-6);
}

#[test]
fn zero_weight_leaves_the_pose_untouched() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let before = pose.locals().to_vec();
    assert!(apply_two_bone_ik(
        &skeleton,
        &mut pose,
        TwoBoneChain::new(0, 1, 2),
        Vec3::new(0.3, 0.4, 0.2),
        Vec3::Z,
        0.0,
    )
    .is_none());
    assert_eq!(pose.locals(), before.as_slice());
}

#[test]
fn invalid_chain_indices_are_rejected() {
    let skeleton = skeleton();
    let mut pose = skeleton.rest_pose();
    let before = pose.locals().to_vec();
    for chain in [
        TwoBoneChain::new(0, 1, 9),
        TwoBoneChain::new(0, 0, 2),
        TwoBoneChain::new(1, 2, 1),
    ] {
        assert!(apply_two_bone_ik(
            &skeleton,
            &mut pose,
            chain,
            Vec3::new(0.3, 0.4, 0.2),
            Vec3::Z,
            1.0,
        )
        .is_none());
    }
    assert_eq!(pose.locals(), before.as_slice());
}
