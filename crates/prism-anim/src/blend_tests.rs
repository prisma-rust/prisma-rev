use super::*;
use crate::clip::{Track, Vec3Key};
use glam::{Quat, Vec3};

fn t(translation: Vec3) -> JointTransform {
    JointTransform::new(translation, Quat::IDENTITY, Vec3::ONE)
}

fn pose(locals: Vec<JointTransform>) -> Pose {
    Pose::from_locals(locals)
}

#[test]
fn blending_at_the_ends_returns_the_original_poses() {
    let a = pose(vec![t(Vec3::ZERO), t(Vec3::X)]);
    let b = pose(vec![t(Vec3::Y), t(Vec3::Z)]);
    assert_eq!(blend_poses(&a, &b, 0.0), a);
    assert_eq!(blend_poses(&a, &b, 1.0), b);
}

#[test]
fn blending_at_the_middle_interpolates_translation_and_scale() {
    let a = pose(vec![JointTransform::new(
        Vec3::ZERO,
        Quat::IDENTITY,
        Vec3::ONE,
    )]);
    let b = pose(vec![JointTransform::new(
        Vec3::new(2.0, 0.0, 0.0),
        Quat::IDENTITY,
        Vec3::splat(3.0),
    )]);
    let middle = blend_poses(&a, &b, 0.5);
    let local = middle.local(0).expect("joint 0");
    assert!((local.translation.x - 1.0).abs() < 1e-6);
    assert!((local.scale.x - 2.0).abs() < 1e-6);
}

#[test]
fn rotations_take_the_shortest_path() {
    // 170° → 190°（= −170°）只差 20°，slerp 必须走 20° 那条路，而不是绕 340°。
    let from = JointTransform::new(
        Vec3::ZERO,
        Quat::from_rotation_y(170f32.to_radians()),
        Vec3::ONE,
    );
    let to = JointTransform::new(
        Vec3::ZERO,
        Quat::from_rotation_y(190f32.to_radians()),
        Vec3::ONE,
    );
    let middle = blend_joint(&from, &to, 0.5);
    // 最短路径的中点 = 180°（绕 Y）。
    let expected = Quat::from_rotation_y(180f32.to_radians());
    assert!(
        middle.rotation.dot(expected).abs() > 0.999,
        "rotated by {}° instead of 180°",
        middle.rotation.to_euler(glam::EulerRot::YXZ).0.to_degrees()
    );
}

#[test]
fn weights_are_clamped_and_nan_falls_back_to_the_first_pose() {
    let a = pose(vec![t(Vec3::ZERO)]);
    let b = pose(vec![t(Vec3::new(10.0, 0.0, 0.0))]);
    assert_eq!(blend_poses(&a, &b, -2.0), a);
    assert_eq!(blend_poses(&a, &b, 5.0), b);
    assert_eq!(blend_poses(&a, &b, f32::NAN), a);
}

#[test]
fn mismatched_lengths_blend_the_shared_joints_and_keep_the_rest_of_a() {
    let a = pose(vec![t(Vec3::ZERO), t(Vec3::new(1.0, 0.0, 0.0))]);
    let b = pose(vec![t(Vec3::new(4.0, 0.0, 0.0))]);
    let mut out = pose(Vec::new());
    let shared = blend_into(&mut out, &a, &b, 0.5);
    assert_eq!(shared, 1, "只有一根关节可以混");
    assert_eq!(out.len(), 2, "长度按 a 保留，缺失的关节不塌到原点");
    assert!((out.local(0).unwrap().translation.x - 2.0).abs() < 1e-6);
    assert!(
        (out.local(1).unwrap().translation.x - 1.0).abs() < 1e-6,
        "b 没有的关节保持 a 的姿态"
    );
}

#[test]
fn a_longer_out_buffer_is_replaced_not_left_over() {
    let mut out = pose(vec![t(Vec3::splat(9.0)); 5]);
    blend_into(
        &mut out,
        &pose(vec![t(Vec3::ZERO)]),
        &pose(vec![t(Vec3::ZERO)]),
        0.5,
    );
    assert_eq!(out.len(), 1);
}

#[test]
fn blend_space_weights_clamp_to_the_ends() {
    let positions = [0.0, 2.0, 6.0];
    assert_eq!(blend_space_weights(&positions, -5.0), Some((0, 0, 0.0)));
    assert_eq!(blend_space_weights(&positions, 0.0), Some((0, 0, 0.0)));
    assert_eq!(blend_space_weights(&positions, 100.0), Some((2, 2, 0.0)));
    assert_eq!(blend_space_weights(&positions, 6.0), Some((2, 2, 0.0)));
}

#[test]
fn blend_space_weights_find_the_bracketing_pair() {
    let positions = [0.0, 2.0, 6.0];
    let (left, right, t) = blend_space_weights(&positions, 4.0).expect("bracketed");
    assert_eq!((left, right), (1, 2));
    assert!((t - 0.5).abs() < 1e-6, "4 is halfway between 2 and 6");
    let (left, right, t) = blend_space_weights(&positions, 1.0).expect("bracketed");
    assert_eq!((left, right), (0, 1));
    assert!((t - 0.5).abs() < 1e-6);
}

#[test]
fn blend_space_weights_reject_empty_and_non_finite_queries() {
    assert_eq!(blend_space_weights(&[], 1.0), None);
    assert_eq!(blend_space_weights(&[0.0, 1.0], f32::NAN), None);
    assert_eq!(blend_space_weights(&[0.0, 1.0], f32::INFINITY), None);
    // 单个样本：左右都指向自己。
    assert_eq!(blend_space_weights(&[3.0], 0.0), Some((0, 0, 0.0)));
}

fn clip(name: &str, duration: f32, translation: Vec3) -> Arc<Clip> {
    Arc::new(Clip {
        name: name.into(),
        duration,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: vec![Track {
            joint: 0,
            translation: vec![
                Vec3Key {
                    time: 0.0,
                    value: translation,
                },
                Vec3Key {
                    time: duration,
                    value: translation,
                },
            ],
            ..Default::default()
        }],
    })
}

fn reference() -> Pose {
    pose(vec![JointTransform::IDENTITY])
}

fn blend_space() -> BlendSpace1D {
    BlendSpace1D::new(
        vec![
            // 故意打乱顺序：作者填的顺序不该影响结果。
            BlendSample {
                position: 300.0,
                clip: clip("run", 2.0, Vec3::new(6.0, 0.0, 0.0)),
            },
            BlendSample {
                position: 0.0,
                clip: clip("idle", 1.0, Vec3::ZERO),
            },
            BlendSample {
                position: 150.0,
                clip: clip("walk", 1.5, Vec3::new(3.0, 0.0, 0.0)),
            },
        ],
        reference(),
    )
}

#[test]
fn a_blend_space_sorts_its_samples_and_reports_its_range() {
    let space = blend_space();
    let positions: Vec<f32> = space.samples().iter().map(|s| s.position).collect();
    assert_eq!(positions, vec![0.0, 150.0, 300.0]);
    assert_eq!(space.range(), Some((0.0, 300.0)));
    assert!(!space.is_empty());
}

#[test]
fn a_blend_space_evaluates_the_exact_sample_at_a_sample_position() {
    let mut space = blend_space();
    let mut out = Pose::from_locals(Vec::new());
    assert!(space.evaluate_into(0.0, 0.25, &mut out));
    let local = out.local(0).expect("joint 0");
    assert!(local.translation.x.abs() < 1e-6, "idle 在原点");

    assert!(space.evaluate_into(300.0, 0.25, &mut out));
    assert!((out.local(0).unwrap().translation.x - 6.0).abs() < 1e-6);
}

#[test]
fn a_blend_space_blends_the_two_neighbours_in_between() {
    let mut space = blend_space();
    let mut out = Pose::from_locals(Vec::new());
    // 75 = idle(0) 与 walk(150) 的中点。
    assert!(space.evaluate_into(75.0, 0.0, &mut out));
    let x = out.local(0).unwrap().translation.x;
    assert!((x - 1.5).abs() < 1e-5, "expected 1.5, got {x}");
}

#[test]
fn a_blend_space_clamps_outside_its_range() {
    let mut space = blend_space();
    let mut out = Pose::from_locals(Vec::new());
    assert!(space.evaluate_into(-100.0, 0.0, &mut out));
    assert!(out.local(0).unwrap().translation.x.abs() < 1e-6);
    assert!(space.evaluate_into(9999.0, 0.0, &mut out));
    assert!((out.local(0).unwrap().translation.x - 6.0).abs() < 1e-6);
}

#[test]
fn the_phase_is_normalised_so_clips_of_different_lengths_stay_aligned() {
    // 两个剪辑时长不同（1s / 2s），同一归一化相位下各自采到自己的关键帧。
    let mut space = BlendSpace1D::new(
        vec![
            BlendSample {
                position: 0.0,
                clip: Arc::new(Clip {
                    name: "short".into(),
                    duration: 1.0,
                    fps: 30.0,
                    looped: true,
                    additive: false,
                    tracks: vec![Track {
                        joint: 0,
                        translation: vec![
                            Vec3Key {
                                time: 0.0,
                                value: Vec3::ZERO,
                            },
                            Vec3Key {
                                time: 1.0,
                                value: Vec3::new(1.0, 0.0, 0.0),
                            },
                        ],
                        ..Default::default()
                    }],
                }),
            },
            BlendSample {
                position: 1.0,
                clip: Arc::new(Clip {
                    name: "long".into(),
                    duration: 2.0,
                    fps: 30.0,
                    looped: true,
                    additive: false,
                    tracks: vec![Track {
                        joint: 0,
                        translation: vec![
                            Vec3Key {
                                time: 0.0,
                                value: Vec3::ZERO,
                            },
                            Vec3Key {
                                time: 2.0,
                                value: Vec3::new(3.0, 0.0, 0.0),
                            },
                        ],
                        ..Default::default()
                    }],
                }),
            },
        ],
        reference(),
    );
    let mut out = Pose::from_locals(Vec::new());
    // 相位 0.5：short 采到 0.5s（x=0.5），long 采到 1.0s（x=1.5），按位置 0.5 混 → x=1.0。
    assert!(space.evaluate_into(0.5, 0.5, &mut out));
    let x = out.local(0).unwrap().translation.x;
    assert!((x - 1.0).abs() < 1e-4, "expected 1.0, got {x}");
}

#[test]
fn phase_wraps_and_non_finite_phase_falls_back_to_zero() {
    let mut space = blend_space();
    let mut out = Pose::from_locals(Vec::new());
    assert!(space.evaluate_into(0.0, 1.25, &mut out));
    assert!(out.local(0).unwrap().translation.x.abs() < 1e-5);
    assert!(space.evaluate_into(0.0, f32::NAN, &mut out));
    assert!(out.local(0).unwrap().translation.x.abs() < 1e-5);
    assert!(space.evaluate_into(0.0, -0.25, &mut out));
    assert!(out.local(0).unwrap().translation.x.abs() < 1e-5);
}

#[test]
fn an_empty_blend_space_writes_nothing() {
    let mut space = BlendSpace1D::default();
    let mut out = Pose::from_locals(vec![t(Vec3::splat(7.0))]);
    assert!(!space.evaluate_into(0.0, 0.0, &mut out));
    assert!(space.is_empty());
    assert_eq!(space.range(), None);
    assert!(
        (out.local(0).unwrap().translation.x - 7.0).abs() < 1e-6,
        "失败时不该清空调用方的姿态"
    );
}

#[test]
fn non_finite_positions_are_dropped_when_building() {
    let space = BlendSpace1D::new(
        vec![
            BlendSample {
                position: f32::NAN,
                clip: clip("bad", 1.0, Vec3::ZERO),
            },
            BlendSample {
                position: 1.0,
                clip: clip("good", 1.0, Vec3::ZERO),
            },
        ],
        reference(),
    );
    assert_eq!(space.samples().len(), 1);
}
