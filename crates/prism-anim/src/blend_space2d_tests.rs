use super::*;
use crate::clip::{Track, Vec3Key};
use crate::skeleton::JointTransform;
use glam::Vec3;

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
        ..Default::default()
    })
}

fn reference() -> Pose {
    Pose::from_locals(vec![JointTransform::IDENTITY])
}

/// 四个样本构成一个 0..10 × 0..10 的正方形，位置写进 x 轴位移，方便断言。
fn square() -> BlendSpace2D {
    BlendSpace2D::new(
        vec![
            BlendSample2D {
                position: [0.0, 0.0],
                clip: clip("a", 1.0, Vec3::ZERO),
            },
            BlendSample2D {
                position: [10.0, 0.0],
                clip: clip("b", 1.0, Vec3::new(10.0, 0.0, 0.0)),
            },
            BlendSample2D {
                position: [10.0, 10.0],
                clip: clip("c", 1.0, Vec3::new(20.0, 0.0, 0.0)),
            },
            BlendSample2D {
                position: [0.0, 10.0],
                clip: clip("d", 1.0, Vec3::new(30.0, 0.0, 0.0)),
            },
        ],
        reference(),
    )
}

#[test]
fn a_square_produces_two_triangles() {
    let space = square();
    assert_eq!(space.triangles().len(), 2);
    assert!(!space.is_empty());
}

#[test]
fn the_centre_sits_on_the_shared_diagonal_of_both_triangles() {
    // 正方形沿 a–c 对角线拆成两个三角形；中心落在这条公共边上，两个三角形给出
    // 同一个结果：a(0) 与 c(20) 各一半。
    let mut space = square();
    let mut out = Pose::from_locals(Vec::new());
    assert!(space.evaluate_into([5.0, 5.0], 0.0, &mut out));
    let x = out.local(0).unwrap().translation.x;
    assert!((x - 10.0).abs() < 1e-4, "expected 10, got {x}");
}

#[test]
fn an_exact_sample_returns_that_clip() {
    let mut space = square();
    let mut out = Pose::from_locals(Vec::new());
    assert!(space.evaluate_into([10.0, 10.0], 0.0, &mut out));
    assert!((out.local(0).unwrap().translation.x - 20.0).abs() < 1e-6);
}

#[test]
fn a_point_on_an_edge_weights_only_its_two_endpoints() {
    let mut space = square();
    let mut out = Pose::from_locals(Vec::new());
    assert!(space.evaluate_into([5.0, 0.0], 0.0, &mut out));
    let x = out.local(0).unwrap().translation.x;
    assert!((x - 5.0).abs() < 1e-4, "expected 5, got {x}");
}

#[test]
fn a_point_outside_the_hull_clamps_to_the_boundary() {
    let mut space = square();
    let mut out = Pose::from_locals(Vec::new());
    // 从中心往 +x 拉出去：应停在 x = 10 那条边上，仍按 y = 5 混合。
    assert!(space.evaluate_into([100.0, 5.0], 0.0, &mut out));
    let x = out.local(0).unwrap().translation.x;
    assert!((x - 15.0).abs() < 1e-3, "expected 15, got {x}");
}

#[test]
fn points_that_are_not_finite_are_ignored() {
    let space = BlendSpace2D::new(
        vec![
            BlendSample2D {
                position: [f32::NAN, 0.0],
                clip: clip("bad", 1.0, Vec3::ZERO),
            },
            BlendSample2D {
                position: [0.0, 0.0],
                clip: clip("a", 1.0, Vec3::ZERO),
            },
            BlendSample2D {
                position: [1.0, 0.0],
                clip: clip("b", 1.0, Vec3::ONE),
            },
            BlendSample2D {
                position: [0.0, 1.0],
                clip: clip("c", 1.0, Vec3::splat(2.0)),
            },
        ],
        reference(),
    );
    assert_eq!(space.samples().len(), 3);
    assert_eq!(space.triangles().len(), 1);
}

#[test]
fn duplicate_positions_keep_the_first_sample() {
    let space = BlendSpace2D::new(
        vec![
            BlendSample2D {
                position: [0.0, 0.0],
                clip: clip("first", 1.0, Vec3::ZERO),
            },
            BlendSample2D {
                position: [0.0, 0.0],
                clip: clip("second", 1.0, Vec3::ONE),
            },
            BlendSample2D {
                position: [1.0, 0.0],
                clip: clip("b", 1.0, Vec3::ONE),
            },
            BlendSample2D {
                position: [0.0, 1.0],
                clip: clip("c", 1.0, Vec3::splat(2.0)),
            },
        ],
        reference(),
    );
    assert_eq!(space.samples().len(), 3);
    assert_eq!(space.samples()[0].clip.name, "first");
}

#[test]
fn collinear_or_too_few_samples_have_no_triangles_and_do_not_write_the_pose() {
    let mut line = BlendSpace2D::new(
        vec![
            BlendSample2D {
                position: [0.0, 0.0],
                clip: clip("a", 1.0, Vec3::ZERO),
            },
            BlendSample2D {
                position: [1.0, 0.0],
                clip: clip("b", 1.0, Vec3::ONE),
            },
            BlendSample2D {
                position: [2.0, 0.0],
                clip: clip("c", 1.0, Vec3::splat(2.0)),
            },
        ],
        reference(),
    );
    assert!(line.is_empty());
    let mut out = Pose::from_locals(vec![JointTransform::new(
        Vec3::splat(9.0),
        glam::Quat::IDENTITY,
        Vec3::ONE,
    )]);
    assert!(!line.evaluate_into([1.0, 0.0], 0.0, &mut out));
    assert_eq!(out.local(0).unwrap().translation, Vec3::splat(9.0));

    let mut pair = BlendSpace2D::new(
        vec![
            BlendSample2D {
                position: [0.0, 0.0],
                clip: clip("a", 1.0, Vec3::ZERO),
            },
            BlendSample2D {
                position: [1.0, 0.0],
                clip: clip("b", 1.0, Vec3::ONE),
            },
        ],
        reference(),
    );
    assert!(pair.is_empty());
    assert!(!pair.evaluate_into([0.5, 0.0], 0.0, &mut out));
}

#[test]
fn non_finite_queries_are_rejected() {
    let mut space = square();
    let mut out = Pose::from_locals(Vec::new());
    assert!(!space.evaluate_into([f32::NAN, 0.0], 0.0, &mut out));
    assert!(!space.evaluate_into([0.0, f32::INFINITY], 0.0, &mut out));
}

#[test]
fn phase_is_normalised_per_clip() {
    // 两个剪辑时长不同，同一归一化相位各自采到自己的时间点。
    let short = Arc::new(Clip {
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
        ..Default::default()
    });
    let long = Arc::new(Clip {
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
        ..Default::default()
    });
    let mut space = BlendSpace2D::new(
        vec![
            BlendSample2D {
                position: [0.0, 0.0],
                clip: short,
            },
            BlendSample2D {
                position: [10.0, 0.0],
                clip: long,
            },
            BlendSample2D {
                position: [5.0, 10.0],
                clip: clip("other", 1.0, Vec3::ZERO),
            },
        ],
        reference(),
    );
    let mut out = Pose::from_locals(Vec::new());
    assert!(space.evaluate_into([5.0, 0.0], 0.5, &mut out));
    // 两段都到各自时长的中点：1 * 0.5 和 3 * 0.5，混合后 1.0。
    let x = out.local(0).unwrap().translation.x;
    assert!((x - 1.0).abs() < 1e-4, "expected 1.0, got {x}");
}
