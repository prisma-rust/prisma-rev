use glam::Vec3;

use super::{BlendLayer, BlendStack, BoneMask};
use crate::pose::Pose;
use crate::skeleton::{Joint, JointTransform, Skeleton};

fn pose(xs: &[f32]) -> Pose {
    Pose::from_locals(
        xs.iter()
            .map(|x| JointTransform {
                translation: Vec3::new(*x, 0.0, 0.0),
                ..JointTransform::IDENTITY
            })
            .collect(),
    )
}

fn x(pose: &Pose, joint: usize) -> f32 {
    pose.local(joint).unwrap().translation.x
}

#[test]
fn an_empty_stack_returns_the_base_pose() {
    let stack = BlendStack::new(pose(&[1.0, 2.0]), pose(&[0.0, 0.0]));
    let mut out = pose(&[9.0, 9.0]);
    stack.evaluate_into(&mut out);
    assert_eq!(out, pose(&[1.0, 2.0]));
    assert!(stack.is_empty());
}

#[test]
fn a_full_weight_layer_replaces_the_base() {
    let mut stack = BlendStack::new(pose(&[0.0, 0.0]), pose(&[0.0, 0.0]));
    stack.push(BlendLayer::new(pose(&[10.0, 20.0])));
    let mut out = pose(&[]);
    stack.evaluate_into(&mut out);
    assert_eq!(x(&out, 0), 10.0);
    assert_eq!(x(&out, 1), 20.0);
}

#[test]
fn layers_blend_on_top_of_the_accumulated_result() {
    let mut stack = BlendStack::new(pose(&[0.0, 0.0]), pose(&[0.0, 0.0]));
    stack.push(BlendLayer::new(pose(&[10.0, 10.0])).with_weight(0.5));
    stack.push(BlendLayer::new(pose(&[20.0, 20.0])).with_weight(0.5));
    let mut out = pose(&[]);
    stack.evaluate_into(&mut out);
    // 0 -> 5 (first 50%) -> 12.5 (second 50% of 5..20)
    assert_eq!(x(&out, 0), 12.5);
    assert_eq!(x(&out, 1), 12.5);
}

#[test]
fn a_bone_mask_only_changes_the_listed_joints() {
    let mut stack = BlendStack::new(pose(&[0.0, 0.0]), pose(&[0.0, 0.0]));
    stack.push(
        BlendLayer::new(pose(&[10.0, 10.0])).with_mask(BoneMask::from_weights(vec![1.0, 0.0])),
    );
    let mut out = pose(&[]);
    stack.evaluate_into(&mut out);
    assert_eq!(x(&out, 0), 10.0);
    assert_eq!(x(&out, 1), 0.0);
}

#[test]
fn additive_layers_apply_only_the_delta_on_masked_joints() {
    let mut stack = BlendStack::new(pose(&[1.0, 1.0]), pose(&[0.0, 0.0]));
    stack.push(
        BlendLayer::new(pose(&[2.0, 2.0]))
            .with_additive(true)
            .with_mask(BoneMask::from_weights(vec![1.0, 0.0])),
    );
    let mut out = pose(&[]);
    stack.evaluate_into(&mut out);
    // joint 0: 1 + (2 - 0) = 3; joint 1 is masked out.
    assert_eq!(x(&out, 0), 3.0);
    assert_eq!(x(&out, 1), 1.0);
}

#[test]
fn activation_delay_and_blend_in_ramp_the_weight() {
    let mut stack = BlendStack::new(pose(&[0.0]), pose(&[0.0]));
    stack.push(BlendLayer::new(pose(&[10.0])).with_timing(0.25, 0.5));
    let mut out = pose(&[]);

    stack.advance(0.2);
    stack.evaluate_into(&mut out);
    assert_eq!(x(&out, 0), 0.0, "not active before the delay");

    stack.advance(0.05);
    stack.evaluate_into(&mut out);
    assert_eq!(x(&out, 0), 0.0, "delay is inclusive at its boundary");

    stack.advance(0.25);
    stack.evaluate_into(&mut out);
    assert!(
        (x(&out, 0) - 5.0).abs() < 1e-5,
        "half-way through the fade-in"
    );

    stack.advance(0.25);
    stack.evaluate_into(&mut out);
    assert!((x(&out, 0) - 10.0).abs() < 1e-5, "fade-in complete");
}

#[test]
fn max_active_blends_keeps_the_first_layers() {
    let mut stack = BlendStack::new(pose(&[0.0]), pose(&[0.0]));
    stack.set_max_active_blends(1);
    stack.push(BlendLayer::new(pose(&[10.0])));
    stack.push(BlendLayer::new(pose(&[20.0])));
    let mut out = pose(&[]);
    stack.evaluate_into(&mut out);
    assert_eq!(x(&out, 0), 10.0);
}

#[test]
fn invalid_weights_and_masks_are_clamped_or_ignored() {
    assert_eq!(
        BoneMask::from_weights(vec![2.0, -1.0, f32::NAN]).weights(),
        &[1.0, 0.0, 0.0]
    );

    let mut stack = BlendStack::new(pose(&[0.0]), pose(&[0.0]));
    stack.push(BlendLayer::new(pose(&[10.0])).with_weight(f32::NAN));
    stack.push(BlendLayer::new(pose(&[20.0])).with_weight(-1.0));
    stack.push(BlendLayer::new(pose(&[30.0])).with_weight(5.0));
    let mut out = pose(&[]);
    stack.evaluate_into(&mut out);
    // The first two are dropped; the last is clamped to 1.
    assert_eq!(x(&out, 0), 30.0);
}

#[test]
fn from_names_builds_a_full_length_mask() {
    let skeleton = Skeleton::new(vec![
        Joint {
            name: "root".to_string(),
            parent: None,
            reference_local: JointTransform::IDENTITY,
            inverse_bind: glam::Mat4::IDENTITY,
        },
        Joint {
            name: "hand".to_string(),
            parent: Some(0),
            reference_local: JointTransform::IDENTITY,
            inverse_bind: glam::Mat4::IDENTITY,
        },
    ])
    .unwrap();
    let mask = BoneMask::from_names(&skeleton, &["hand", "missing"]);
    assert_eq!(mask.weights(), &[0.0, 1.0]);
    assert_eq!(mask.weight(0), 0.0);
    assert_eq!(mask.weight(1), 1.0);
    // Missing indices default to zero for a partial mask.
    assert_eq!(mask.weight(2), 0.0);
    // The empty mask means "all bones".
    assert_eq!(BoneMask::all().weight(99), 1.0);
}

#[test]
fn advance_does_not_evaluate_and_evaluate_does_not_advance() {
    let mut stack = BlendStack::new(pose(&[0.0]), pose(&[0.0]));
    stack.push(BlendLayer::new(pose(&[10.0])).with_timing(0.0, 1.0));
    let mut out = pose(&[]);
    stack.evaluate_into(&mut out);
    assert_eq!(x(&out, 0), 0.0);
    assert_eq!(stack.layers()[0].elapsed(), 0.0);
    stack.advance(0.5);
    assert_eq!(stack.layers()[0].elapsed(), 0.5);
    stack.evaluate_into(&mut out);
    assert!((x(&out, 0) - 5.0).abs() < 1e-5);
}
