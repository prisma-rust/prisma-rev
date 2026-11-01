use super::*;
use glam::Vec3;

fn v3(time: f32, value: Vec3) -> Vec3Key {
    Vec3Key { time, value }
}

fn q(time: f32, value: Quat) -> QuatKey {
    QuatKey { time, value }
}

#[test]
fn sampling_interpolates_between_keys_and_holds_the_ends() {
    let keys = [v3(0.0, Vec3::ZERO), v3(1.0, Vec3::new(10.0, 0.0, 0.0))];
    assert_eq!(sample_vec3(&keys, -5.0), Some(Vec3::ZERO));
    let middle = sample_vec3(&keys, 0.5).unwrap();
    assert!((middle.x - 5.0).abs() < 1e-6, "got {middle:?}");
    assert_eq!(sample_vec3(&keys, 9.0), Some(Vec3::new(10.0, 0.0, 0.0)));
    assert!(sample_vec3(&[], 0.0).is_none());
}

#[test]
fn a_single_key_is_constant() {
    let keys = [v3(0.5, Vec3::new(1.0, 2.0, 3.0))];
    assert_eq!(sample_vec3(&keys, 0.0), Some(Vec3::new(1.0, 2.0, 3.0)));
    assert_eq!(sample_vec3(&keys, 99.0), Some(Vec3::new(1.0, 2.0, 3.0)));
}

#[test]
fn three_keys_pick_the_right_segment() {
    let keys = [
        v3(0.0, Vec3::ZERO),
        v3(1.0, Vec3::new(2.0, 0.0, 0.0)),
        v3(2.0, Vec3::new(0.0, 4.0, 0.0)),
    ];
    // 1.5 落在第二段中间（2,0,0) → (0,4,0)。
    let mid = sample_vec3(&keys, 1.5).unwrap();
    assert!((mid.x - 1.0).abs() < 1e-6, "got {mid:?}");
    assert!((mid.y - 2.0).abs() < 1e-6, "got {mid:?}");
    // 第一段。
    let first = sample_vec3(&keys, 0.25).unwrap();
    assert!((first.x - 0.5).abs() < 1e-6, "got {first:?}");
}

#[test]
fn rotations_slerp_the_short_way() {
    let keys = [
        q(0.0, Quat::IDENTITY),
        q(1.0, Quat::from_rotation_y(std::f32::consts::PI)),
    ];
    let half = sample_quat(&keys, 0.5).unwrap();
    let rotated = half * Vec3::X;
    assert!(
        rotated.z.abs() > 0.99,
        "half of 180° is 90°, got {rotated:?}"
    );
    // 端点保持。
    assert!(sample_quat(&keys, -1.0)
        .unwrap()
        .abs_diff_eq(Quat::IDENTITY, 1e-6));
}

#[test]
fn looping_times_wrap_and_once_clips_clamp() {
    let mut clip = Clip {
        name: "walk".into(),
        duration: 2.0,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: Vec::new(),
    };
    assert!((clip.wrap_time(2.5) - 0.5).abs() < 1e-6);
    assert!((clip.wrap_time(-0.5) - 1.5).abs() < 1e-6, "负时间也要绕回");
    assert_eq!(clip.wrap_time(f32::NAN), 0.0);

    clip.looped = false;
    assert_eq!(clip.wrap_time(2.5), 2.0);
    assert_eq!(clip.wrap_time(-1.0), 0.0);

    clip.duration = 0.0;
    assert_eq!(clip.wrap_time(3.0), 0.0);
}

#[test]
fn sampling_writes_only_the_channels_that_have_keys() {
    let skeleton = crate::skeleton::tests::two_bone_skeleton();
    let mut pose = skeleton.rest_pose();
    let clip = Clip {
        name: "bend".into(),
        duration: 1.0,
        fps: 30.0,
        looped: false,
        additive: false,
        tracks: vec![Track {
            joint: 1,
            translation: Vec::new(),
            rotation: vec![
                q(0.0, Quat::IDENTITY),
                q(1.0, Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)),
            ],
            scale: Vec::new(),
        }],
    };
    assert!(clip.sample_into(0.0, &mut pose));
    // 位置没有被写（剪辑不动 position 通道）。
    assert_eq!(pose.local(1).unwrap().translation, Vec3::new(2.0, 0.0, 0.0));
    clip.sample_into(1.0, &mut pose);
    let rotated = pose.local(1).unwrap().rotation * Vec3::X;
    assert!(rotated.y.abs() > 0.99, "got {rotated:?}");
}

#[test]
fn a_track_for_a_missing_joint_is_ignored() {
    let skeleton = crate::skeleton::tests::two_bone_skeleton();
    let mut pose = skeleton.rest_pose();
    let clip = Clip {
        name: "bad".into(),
        duration: 1.0,
        fps: 30.0,
        looped: false,
        additive: false,
        tracks: vec![Track {
            joint: 99,
            translation: vec![v3(0.0, Vec3::ONE)],
            ..Default::default()
        }],
    };
    let before = pose.clone();
    clip.sample_into(0.0, &mut pose);
    assert_eq!(pose, before, "指向不存在骨骼的轨道不该改动姿态");
}

#[test]
fn max_key_time_sees_every_channel() {
    let clip = Clip {
        name: "c".into(),
        duration: 5.0,
        fps: 30.0,
        looped: false,
        additive: false,
        tracks: vec![
            Track {
                joint: 0,
                translation: vec![v3(0.0, Vec3::ZERO), v3(3.0, Vec3::ONE)],
                ..Default::default()
            },
            Track {
                joint: 1,
                scale: vec![v3(1.0, Vec3::ONE), v3(7.5, Vec3::ONE)],
                ..Default::default()
            },
        ],
    };
    assert!((clip.max_key_time() - 7.5).abs() < 1e-6);
    assert!(!clip.is_empty());
    assert!(Clip {
        tracks: vec![Track {
            joint: 0,
            ..Default::default()
        }],
        ..clip
    }
    .is_empty());
}
