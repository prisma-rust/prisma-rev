use super::*;
use glam::{Mat4, Quat, Vec3};
use prism_anim::{Joint, JointTransform, Track, Vec3Key};
use prism_scene::components::SkeletonRoot;

fn two_bone_skeleton() -> Skeleton {
    Skeleton::new(vec![
        Joint {
            name: "root".into(),
            parent: None,
            reference_local: JointTransform::IDENTITY,
            inverse_bind: Mat4::IDENTITY,
        },
        Joint {
            name: "child".into(),
            parent: Some(0),
            reference_local: JointTransform::new(
                Vec3::new(2.0, 0.0, 0.0),
                Quat::IDENTITY,
                Vec3::ONE,
            ),
            inverse_bind: Mat4::from_translation(Vec3::new(2.0, 0.0, 0.0)).inverse(),
        },
    ])
    .expect("two bones")
}

/// 子骨骼绕 Z 从 0 转到 90°，时长 1s。
fn bend_clip() -> Clip {
    Clip {
        name: "bend".into(),
        duration: 1.0,
        fps: 30.0,
        looped: false,
        additive: false,
        tracks: vec![Track {
            joint: 1,
            translation: Vec::new(),
            rotation: vec![
                prism_anim::QuatKey {
                    time: 0.0,
                    value: Quat::IDENTITY,
                },
                prism_anim::QuatKey {
                    time: 1.0,
                    value: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
                },
            ],
            scale: Vec::new(),
        }],
    }
}

fn world_with_animator(clip_path: &str) -> (World, Entity) {
    let mut world = World::new();
    let mut library = SkeletonLibrary::new();
    library.insert_skeleton("rig.rskel", two_bone_skeleton());
    library.insert_clip("bend.ranim", bend_clip());
    world.insert_resource(library);

    let entity = world.spawn();
    world.insert(
        entity,
        SkeletonRoot {
            skeleton_path: "rig.rskel".into(),
            default_clip_path: "bend.ranim".into(),
        },
    );
    world.insert(
        entity,
        Animator {
            clip_path: clip_path.into(),
            ..Default::default()
        },
    );
    (world, entity)
}

/// 把 bind 姿态下一个**离关节有距离**的顶点（局部 `(2, 1, 0)`）变换到世界。
///
/// 关节原点本身在旋转下是不动的，所以必须取一个偏置点才看得出动画。
fn child_tip(world: &World, entity: Entity) -> Vec3 {
    let palette = world.get::<BonePalette>(entity).expect("palette");
    let matrix = Mat4::from_cols_array_2d(&palette.matrices[1]);
    matrix.transform_point3(Vec3::new(2.0, 1.0, 0.0))
}

#[test]
fn the_animator_produces_a_palette_and_advances_time() {
    let (mut world, entity) = world_with_animator("");
    assert_eq!(
        animator_system(&mut world, 0.0),
        1,
        "第一帧就算出 static pose"
    );

    let palette = world.get::<BonePalette>(entity).unwrap();
    assert_eq!(palette.len(), 2);

    // 0.5s 处子骨骼转了 45°：端点应该离开 +X 轴。
    animator_system(&mut world, 0.5);
    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert!((head.time - 0.5).abs() < 1e-5);
    // (2,1,0) 绕关节转 45°：x = 2 - sin45, y = cos45。
    let tip = child_tip(&world, entity);
    assert!((tip.x - (2.0 - 0.70710678)).abs() < 1e-4, "tip {tip:?}");
    assert!((tip.y - 0.70710678).abs() < 1e-4, "tip {tip:?}");
}

#[test]
fn a_paused_animator_keeps_its_pose() {
    let (mut world, entity) = world_with_animator("");
    animator_system(&mut world, 0.0);
    world.get_mut::<Animator>(entity).unwrap().playing = false;
    animator_system(&mut world, 5.0);
    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert_eq!(head.time, 0.0, "暂停时不推进时间");
    assert!(world.get::<BonePalette>(entity).is_some());
}

#[test]
fn a_once_clip_stops_at_the_end_and_flags_finished() {
    let (mut world, entity) = world_with_animator("");
    world.get_mut::<Animator>(entity).unwrap().looped = false;
    animator_system(&mut world, 10.0);
    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert!((head.time - 1.0).abs() < 1e-5, "单次播放夹在 duration");
    assert!(head.finished);
    // 末端姿态：子骨骼转了 90°，(2,1,0) 绕关节转到 (1,0,0)。
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-4, "tip {tip:?}");
    assert!(tip.y.abs() < 1e-4, "tip {tip:?}");
}

#[test]
fn a_looping_clip_wraps_the_playhead() {
    let (mut world, entity) = world_with_animator("");
    world.get_mut::<Animator>(entity).unwrap().looped = true;
    world.get_mut::<Animator>(entity).unwrap().speed = 1.0;
    animator_system(&mut world, 2.25);
    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert!(!head.finished, "循环剪辑永远不会 finished");
    // 2.25s / 1.0s 循环 → 采样 0.25s 处（约 22.5°）。
    let angle = 0.25 * std::f32::consts::FRAC_PI_2;
    let tip = child_tip(&world, entity);
    assert!((tip.x - (2.0 - angle.sin())).abs() < 1e-3, "tip {tip:?}");
    assert!((tip.y - angle.cos()).abs() < 1e-3, "tip {tip:?}");
}

#[test]
fn the_default_clip_from_the_skeleton_root_is_used_when_none_is_set() {
    let (mut world, entity) = world_with_animator("");
    animator_system(&mut world, 0.0);
    assert_eq!(
        world.get::<AnimatorPlayhead>(entity).unwrap().clip_path,
        "bend.ranim"
    );
}

#[test]
fn an_animator_without_its_assets_is_left_alone() {
    let (mut world, entity) = world_with_animator("");
    world.get_resource_mut::<SkeletonLibrary>().unwrap().clear();
    assert_eq!(animator_system(&mut world, 0.5), 0);
    assert!(world.get::<BonePalette>(entity).is_none());
    assert!(world.get::<AnimatorPlayhead>(entity).is_none());
}

#[test]
fn changing_the_clip_restarts_from_the_authored_time() {
    let (mut world, entity) = world_with_animator("");
    animator_system(&mut world, 0.5);
    let mut other = bend_clip();
    other.name = "other".into();
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("other.ranim", other);

    let animator = world.get_mut::<Animator>(entity).unwrap();
    animator.clip_path = "other.ranim".into();
    animator.time = 0.25;
    animator.playing = false;
    animator_system(&mut world, 0.5);

    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert_eq!(head.clip_path, "other.ranim");
    assert!((head.time - 0.25).abs() < 1e-5, "换剪辑按作者时间重来");
}

#[test]
fn the_sanitizer_rejects_bad_times() {
    assert_eq!(sanitize_time(f32::NAN), 0.0);
    assert_eq!(sanitize_time(f32::INFINITY), 0.0);
    assert_eq!(sanitize_time(2.5), 2.5);
}

#[test]
fn the_library_reports_what_it_holds() {
    let mut library = SkeletonLibrary::new();
    assert!(library.insert_skeleton("a", two_bone_skeleton()));
    assert!(
        !library.insert_skeleton("a", two_bone_skeleton()),
        "重复插入"
    );
    assert!(library.insert_clip("c", bend_clip()));
    assert!(library.has_skeleton("a"));
    assert!(library.has_clip("c"));
    assert!(library.skeleton("a").is_some());
    assert!(library.clip("c").is_some());
    assert_eq!(library.skeleton_count(), 1);
    assert_eq!(library.clip_count(), 1);
    library.clear();
    assert!(library.skeleton("a").is_none());
}

#[test]
fn a_track_pointing_at_a_missing_joint_does_not_stop_the_palette() {
    let (mut world, entity) = world_with_animator("");
    let mut clip = bend_clip();
    clip.tracks.push(Track {
        joint: 42,
        translation: vec![Vec3Key {
            time: 0.0,
            value: Vec3::ONE,
        }],
        rotation: Vec::new(),
        scale: Vec::new(),
    });
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bend.ranim", clip);
    assert_eq!(animator_system(&mut world, 0.1), 1);
    assert_eq!(world.get::<BonePalette>(entity).unwrap().len(), 2);
}
