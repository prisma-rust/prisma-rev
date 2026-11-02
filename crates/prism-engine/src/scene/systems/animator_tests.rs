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

/// 子骨骼恒等于 +90°（绕 Z）的剪辑：与 [`bend_clip`] 的起始姿态（0°）明确不同。
fn bent_90_clip(name: &str) -> Clip {
    Clip {
        name: name.into(),
        duration: 1.0,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: vec![Track {
            joint: 1,
            translation: Vec::new(),
            rotation: vec![prism_anim::QuatKey {
                time: 0.0,
                value: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            }],
            scale: Vec::new(),
        }],
    }
}

/// 把实体切到 `path`，并把 `blend_duration` / `playing` 设好（不跑系统）。
fn switch_clip(world: &mut World, entity: Entity, path: &str, blend_duration: f32) {
    let animator = world.get_mut::<Animator>(entity).unwrap();
    animator.clip_path = path.into();
    animator.time = 0.0;
    animator.blend_duration = blend_duration;
    animator.playing = true;
}

#[test]
fn switching_clips_crossfades_instead_of_popping() {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bent90.ranim", bent_90_clip("bent90"));
    // 起点：bend 的 0s（子骨骼 0°）。
    animator_system(&mut world, 0.0);
    let start = child_tip(&world, entity);
    assert!((start.y - 1.0).abs() < 1e-4, "start tip {start:?}");

    switch_clip(&mut world, entity, "bent90.ranim", 1.0);
    // 第一帧：权重仍接近 0 → 还是旧姿态（这就是"不硬切"）。
    animator_system(&mut world, 0.0);
    let faded_in = child_tip(&world, entity);
    assert!(
        (faded_in.y - 1.0).abs() < 1e-4,
        "过渡开头该是旧姿态，得到 {faded_in:?}"
    );

    // 半程：权重 0.5 → 约 45°。
    animator_system(&mut world, 0.5);
    let half = child_tip(&world, entity);
    let angle = std::f32::consts::FRAC_PI_4;
    assert!(
        (half.x - (2.0 - angle.sin())).abs() < 1e-3,
        "半程该在 45°，得到 {half:?}"
    );
    assert!((half.y - angle.cos()).abs() < 1e-3, "half {half:?}");

    // 走完：权重 1 → 完整的新剪辑（90°），来源被清掉。
    animator_system(&mut world, 0.5);
    let done = child_tip(&world, entity);
    assert!((done.x - 1.0).abs() < 1e-3, "结束该是 90°，得到 {done:?}");
    assert!(done.y.abs() < 1e-3, "结束该是 90°，得到 {done:?}");
    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert!(
        head.previous_clip_path.is_empty(),
        "过渡结束后不该继续持有来源"
    );
}

#[test]
fn a_zero_blend_duration_switches_immediately() {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bent90.ranim", bent_90_clip("bent90"));
    animator_system(&mut world, 0.0);

    switch_clip(&mut world, entity, "bent90.ranim", 0.0);
    animator_system(&mut world, 0.0);

    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "0 时长 = 直接切，得到 {tip:?}");
    assert!(world
        .get::<AnimatorPlayhead>(entity)
        .unwrap()
        .previous_clip_path
        .is_empty());
}

#[test]
fn a_paused_animator_does_not_advance_the_crossfade() {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bent90.ranim", bent_90_clip("bent90"));
    animator_system(&mut world, 0.0);

    switch_clip(&mut world, entity, "bent90.ranim", 1.0);
    world.get_mut::<Animator>(entity).unwrap().playing = false;
    animator_system(&mut world, 0.4);
    animator_system(&mut world, 0.4);

    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert_eq!(head.blend_elapsed, 0.0, "暂停时过渡也不该走");
    assert_eq!(head.previous_clip_path, "bend.ranim", "来源仍然保留");
    let tip = child_tip(&world, entity);
    assert!(
        (tip.y - 1.0).abs() < 1e-4,
        "仍然显示旧姿态（0° 的 tip 在 y=1），得到 {tip:?}"
    );
}

#[test]
fn losing_the_source_clip_drops_the_fade_instead_of_sticking() {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bent90.ranim", bent_90_clip("bent90"));
    animator_system(&mut world, 0.0);

    switch_clip(&mut world, entity, "bent90.ranim", 1.0);
    // 过渡开始的同一帧就把旧剪辑从库里拿掉（热重载/删资产）。
    {
        let library = world.get_resource_mut::<SkeletonLibrary>().unwrap();
        library.clear();
        library.insert_skeleton("rig.rskel", two_bone_skeleton());
        library.insert_clip("bent90.ranim", bent_90_clip("bent90"));
    }
    animator_system(&mut world, 0.0);

    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert!(head.previous_clip_path.is_empty(), "缺来源时直接放弃过渡");
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "直接切到新剪辑，得到 {tip:?}");
}

#[test]
fn blend_weights_are_clamped_and_need_both_a_source_and_a_duration() {
    let mut head = AnimatorPlayhead {
        clip_path: "new".into(),
        time: 0.0,
        finished: false,
        previous_clip_path: "old".into(),
        previous_time: 0.0,
        blend_elapsed: 0.0,
    };
    assert_eq!(head.blend_weight(1.0), Some(0.0));
    head.blend_elapsed = 0.5;
    assert_eq!(head.blend_weight(1.0), Some(0.5));
    head.blend_elapsed = 99.0;
    assert_eq!(head.blend_weight(1.0), Some(1.0), "权重夹到 1");
    assert_eq!(head.blend_weight(0.0), None, "0 时长 = 无过渡");
    head.previous_clip_path.clear();
    assert_eq!(head.blend_weight(1.0), None, "没有来源就没有权重");
}

#[test]
fn bad_blend_durations_are_treated_as_zero() {
    assert_eq!(sanitize_blend_duration(f32::NAN), 0.0);
    assert_eq!(sanitize_blend_duration(-1.0), 0.0);
    assert_eq!(sanitize_blend_duration(f32::INFINITY), 0.0);
    assert_eq!(sanitize_blend_duration(0.0), 0.0);
    assert_eq!(sanitize_blend_duration(0.2), 0.2);
}

fn install_bent90(world: &mut World) {
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bent90.ranim", bent_90_clip("bent90"));
}

/// 把实体切成「bend ↔ bent90」的一维混合空间，`parameter` 是轴上的位置。
fn use_blend_space(world: &mut World, entity: Entity, parameter: f32) {
    let animator = world.get_mut::<Animator>(entity).unwrap();
    animator.blend_clip_paths = vec!["bend.ranim".into(), "bent90.ranim".into()];
    animator.blend_positions = vec![0.0, 1.0];
    animator.blend_parameter = parameter;
    animator.time = 0.0;
    animator.playing = false;
}

#[test]
fn a_one_dimensional_blend_space_is_sampled_from_the_parameter() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);

    // 参数 0 → 纯 bend.ranim（0s 处是 0°）。
    use_blend_space(&mut world, entity, 0.0);
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!((tip.y - 1.0).abs() < 1e-4, "参数 0 该是 0°，得到 {tip:?}");
    assert_eq!(
        world.get::<AnimatorPlayhead>(entity).unwrap().clip_path,
        BLEND_SPACE_KEY,
        "混合空间用自己的播放键"
    );

    // 参数 1 → 纯 bent90（90°）。
    use_blend_space(&mut world, entity, 1.0);
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "参数 1 该是 90°，得到 {tip:?}");

    // 参数 0.5 → 45°。
    use_blend_space(&mut world, entity, 0.5);
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    let angle = std::f32::consts::FRAC_PI_4;
    assert!(
        (tip.x - (2.0 - angle.sin())).abs() < 1e-3,
        "参数 0.5 该在 45°，得到 {tip:?}"
    );
}

#[test]
fn a_blend_space_needs_two_loaded_clips_and_falls_back_to_the_single_one() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);

    // 两个路径，但其中一条不在库里 → 只剩一个样本，退回单剪辑。
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.blend_clip_paths = vec!["bend.ranim".into(), "missing.ranim".into()];
        animator.blend_positions = vec![0.0, 1.0];
        animator.playing = false;
    }
    animator_system(&mut world, 0.0);
    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert_eq!(head.clip_path, "bend.ranim", "缺一个样本时退回单剪辑");

    // 只有一条路径 + 一条位置：不算混合空间。
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.blend_clip_paths = vec!["bend.ranim".into()];
        animator.blend_positions = vec![0.0, 1.0];
    }
    animator_system(&mut world, 0.0);
    assert_eq!(
        world.get::<AnimatorPlayhead>(entity).unwrap().clip_path,
        "bend.ranim"
    );
}

#[test]
fn a_blend_space_clamps_out_of_range_parameters() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);
    for (parameter, expect_90) in [(-5.0, false), (50.0, true)] {
        use_blend_space(&mut world, entity, parameter);
        animator_system(&mut world, 0.0);
        let tip = child_tip(&world, entity);
        if expect_90 {
            assert!((tip.x - 1.0).abs() < 1e-3, "参数 50 夹到 1，得到 {tip:?}");
        } else {
            assert!((tip.y - 1.0).abs() < 1e-4, "参数 -5 夹到 0，得到 {tip:?}");
        }
    }
}

#[test]
fn entering_a_blend_space_crossfades_from_the_single_clip() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);
    // 先按单剪辑跑一帧，把播放头建立在 bend.ranim 上。
    animator_system(&mut world, 0.0);

    use_blend_space(&mut world, entity, 1.0);
    world.get_mut::<Animator>(entity).unwrap().playing = true;
    world.get_mut::<Animator>(entity).unwrap().blend_duration = 1.0;
    // 过渡开始：权重 0 → 还是 bend 的姿态。
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.y - 1.0).abs() < 1e-4,
        "进来第一帧还是旧姿态，得到 {tip:?}"
    );
    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert_eq!(head.previous_clip_path, "bend.ranim");
    assert_eq!(head.clip_path, BLEND_SPACE_KEY);

    // 走完过渡：纯混合空间姿态（90°），来源清空。
    animator_system(&mut world, 1.0);
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "过渡完该是 90°，得到 {tip:?}");
    assert!(world
        .get::<AnimatorPlayhead>(entity)
        .unwrap()
        .previous_clip_path
        .is_empty());
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

#[test]
fn the_library_reports_clip_key_times_for_the_timeline() {
    let mut library = SkeletonLibrary::new();
    let clip = Clip {
        name: "walk".into(),
        duration: 2.0,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: vec![Track {
            joint: 1,
            translation: vec![
                Vec3Key {
                    time: 0.0,
                    value: Vec3::ZERO,
                },
                Vec3Key {
                    time: 1.25,
                    value: Vec3::X,
                },
                Vec3Key {
                    time: 2.0,
                    value: Vec3::Y,
                },
            ],
            ..Default::default()
        }],
    };
    library.insert_clip("char/walk.ranim", clip);
    assert_eq!(
        library.clip_key_times("char/walk.ranim"),
        vec![0.0, 1.25, 2.0]
    );
    assert!(library.clip_key_times("char/missing.ranim").is_empty());
}
