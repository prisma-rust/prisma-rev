use glam::{Mat4, Quat, Vec3};
use prism_anim::clip::{Clip, Track, Vec3Key};
use prism_anim::skeleton::Joint;
use prism_anim::{JointTransform, Skeleton};

use crate::database::{DatabaseSettings, MotionDatabase, MotionFrame};
use crate::pose_feature::PoseFeature;
use crate::search::{search, MatchWeights};
use crate::trajectory::{Trajectory, TrajectoryPoint, TrajectoryWeights};

fn chain() -> Skeleton {
    let joint = |name: &str, parent: Option<usize>, at: Vec3| Joint {
        name: name.into(),
        parent,
        reference_local: JointTransform::new(at, Quat::IDENTITY, Vec3::ONE),
        inverse_bind: Mat4::IDENTITY,
    };
    Skeleton::new(vec![
        joint("root", None, Vec3::ZERO),
        joint("spine", Some(0), Vec3::new(0.0, 0.5, 0.0)),
    ])
    .expect("valid hierarchy")
}

/// Root walks forward at 2 m/s along -Z; spine bobs for a distinguishable pose.
fn walking_clip() -> Clip {
    let root = Track {
        joint: 0,
        translation: vec![
            Vec3Key {
                time: 0.0,
                value: Vec3::ZERO,
            },
            Vec3Key {
                time: 1.0,
                value: Vec3::new(0.0, 0.0, -2.0),
            },
        ],
        rotation: Vec::new(),
        scale: Vec::new(),
    };
    let spine = Track {
        joint: 1,
        translation: vec![
            Vec3Key {
                time: 0.0,
                value: Vec3::ZERO,
            },
            Vec3Key {
                time: 1.0,
                value: Vec3::new(0.0, 0.4, 0.0),
            },
        ],
        rotation: Vec::new(),
        scale: Vec::new(),
    };
    Clip {
        name: "walk".into(),
        duration: 1.0,
        fps: 30.0,
        tracks: vec![root, spine],
        ..Clip::default()
    }
}

#[test]
fn a_prediction_keeps_velocity_and_bearing() {
    let trajectory = Trajectory::predicted(Vec3::ZERO, Vec3::NEG_Z, Vec3::NEG_Z * 2.0, &[0.5, 1.0]);
    assert_eq!(trajectory.len(), 2);
    assert_eq!(trajectory.points()[1].position, Vec3::NEG_Z * 2.0);
    assert_eq!(trajectory.points()[1].velocity, Vec3::NEG_Z * 2.0);
    assert_eq!(trajectory.points()[0].facing, Vec3::NEG_Z);
}

#[test]
fn trajectories_of_different_shapes_do_not_compare() {
    let short = Trajectory::predicted(Vec3::ZERO, Vec3::Z, Vec3::Z, &[0.5]);
    let long = Trajectory::predicted(Vec3::ZERO, Vec3::Z, Vec3::Z, &[0.5, 1.0]);
    assert!(short
        .distance_to(&long, &TrajectoryWeights::default())
        .is_none());
    assert_eq!(
        short.distance_to(&short, &TrajectoryWeights::default()),
        Some(0.0)
    );
    assert!(Trajectory::new(vec![0.5, 0.5], vec![TrajectoryPoint::default(); 2]).is_none());
}

#[test]
fn pose_features_difference_positions_for_velocity() {
    let previous = vec![Mat4::from_translation(Vec3::ZERO), Mat4::IDENTITY];
    let current = vec![
        Mat4::from_translation(Vec3::new(0.0, 0.0, -1.0)),
        Mat4::IDENTITY,
    ];
    let feature = PoseFeature::from_matrices(&current, Some(&previous), 0.5, &[0]);
    assert_eq!(feature.len(), 1);
    assert_eq!(feature.positions()[0], Vec3::new(0.0, 0.0, -1.0));
    assert_eq!(feature.velocities()[0], Vec3::new(0.0, 0.0, -2.0));
    let first_frame = PoseFeature::from_matrices(&current, None, 0.5, &[0]);
    assert_eq!(first_frame.velocities()[0], Vec3::ZERO);
}

#[test]
fn a_built_database_describes_the_clip_and_search_finds_the_pose() {
    let skeleton = chain();
    let clip = walking_clip();
    let settings = DatabaseSettings {
        sample_interval: 0.25,
        trajectory_times: vec![0.25, 0.5],
        track_joints: vec![0, 1],
        root_joint: 0,
    };
    let database = MotionDatabase::build(&skeleton, &[&clip], &settings);
    assert_eq!(database.len(), 5, "0.0 ..= 1.0 every 0.25 s");

    let middle = &database.frames()[2];
    assert!((middle.time - 0.5).abs() < 1e-5);
    // Root sits at -1.0 m at t = 0.5 and moves 2 m/s towards -Z, so a sample
    // 0.25 s ahead is at -1.5 m.
    let first = middle.trajectory.points()[0];
    assert!(
        (first.position.z + 1.5).abs() < 1e-4,
        "{:?}",
        first.position
    );
    assert!(first.velocity.z < -1.5, "{:?}", first.velocity);
    assert_eq!(first.facing, Vec3::NEG_Z);

    let found = search(
        &database,
        &middle.pose,
        &middle.trajectory,
        &MatchWeights::default(),
    )
    .expect("a frame comparable to itself");
    assert_eq!(found.frame, 2);
    assert!(found.cost < 1e-6, "{}", found.cost);
}

#[test]
fn frames_with_another_joint_list_are_skipped() {
    let pose = PoseFeature::new(vec![Vec3::ZERO], vec![Vec3::ZERO]).expect("equal lengths");
    let trajectory = Trajectory::predicted(Vec3::ZERO, Vec3::Z, Vec3::Z, &[0.5]);
    let stranger = MotionFrame {
        clip: 0,
        time: 0.0,
        pose: PoseFeature::new(vec![Vec3::ZERO; 2], vec![Vec3::ZERO; 2]).expect("equal lengths"),
        trajectory: trajectory.clone(),
    };
    let database = MotionDatabase::from_frames(vec![stranger]);
    assert!(search(&database, &pose, &trajectory, &MatchWeights::default()).is_none());
}
