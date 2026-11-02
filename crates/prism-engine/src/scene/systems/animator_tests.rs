use super::*;
use glam::{Mat4, Quat, Vec3};
use prism_anim::{
    AnimState, AnimTransition, ClipNotify, Joint, JointTransform, Pose, StateMachine, Track,
    TransitionCondition, Vec3Key,
};
use prism_scene::components::{AnimLayer, SkeletonRoot};

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
        ..Default::default()
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
        ..Default::default()
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
    assert!(head.previous_pose.is_none(), "过渡结束后不该继续持有来源");
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
        .previous_pose
        .is_none());
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
    assert!(head.previous_pose.is_some(), "暂停时快照仍然保留");
    let tip = child_tip(&world, entity);
    assert!(
        (tip.y - 1.0).abs() < 1e-4,
        "仍然显示旧姿态（0° 的 tip 在 y=1），得到 {tip:?}"
    );
}

#[test]
fn losing_the_source_clip_keeps_the_snapshot_and_still_completes() {
    // 过渡源是**姿态快照**，所以旧剪辑从库里消失（删资产 / 热重载）也不影响淡出：
    // 以前这里会直接硬切，现在必须照常走完过渡。
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bent90.ranim", bent_90_clip("bent90"));
    animator_system(&mut world, 0.0);

    switch_clip(&mut world, entity, "bent90.ranim", 1.0);
    // 过渡开始的同一帧就把旧剪辑从库里拿掉。
    {
        let library = world.get_resource_mut::<SkeletonLibrary>().unwrap();
        library.clear();
        library.insert_skeleton("rig.rskel", two_bone_skeleton());
        library.insert_clip("bent90.ranim", bent_90_clip("bent90"));
    }
    animator_system(&mut world, 0.0);

    let head = world.get::<AnimatorPlayhead>(entity).unwrap();
    assert!(head.previous_pose.is_some(), "快照还在，过渡不该被放弃");
    let tip = child_tip(&world, entity);
    assert!(
        (tip.y - 1.0).abs() < 1e-4,
        "过渡开头仍然是旧姿态，得到 {tip:?}"
    );

    // 走完：纯新剪辑，快照丢掉。
    animator_system(&mut world, 1.0);
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "过渡完该是 90°，得到 {tip:?}");
    assert!(world
        .get::<AnimatorPlayhead>(entity)
        .unwrap()
        .previous_pose
        .is_none());
}

#[test]
fn blend_weights_are_clamped_and_need_both_a_source_and_a_duration() {
    let mut head = AnimatorPlayhead {
        clip_path: "new".into(),
        time: 0.0,
        finished: false,
        previous_pose: Some(Pose::default()),
        pose: Pose::default(),
        blend_elapsed: 0.0,
        blend_total: None,
    };
    assert_eq!(head.blend_weight(1.0), Some(0.0));
    head.blend_elapsed = 0.5;
    assert_eq!(head.blend_weight(1.0), Some(0.5));
    head.blend_elapsed = 99.0;
    assert_eq!(head.blend_weight(1.0), Some(1.0), "权重夹到 1");
    assert_eq!(head.blend_weight(0.0), None, "0 时长 = 无过渡");
    head.previous_pose = None;
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
    assert!(head.previous_pose.is_some(), "进来时冻住了旧姿态");
    assert_eq!(head.clip_path, BLEND_SPACE_KEY);

    // 走完过渡：纯混合空间姿态（90°），来源清空。
    animator_system(&mut world, 1.0);
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "过渡完该是 90°，得到 {tip:?}");
    assert!(world
        .get::<AnimatorPlayhead>(entity)
        .unwrap()
        .previous_pose
        .is_none());
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
        ..Default::default()
    };
    library.insert_clip("char/walk.ranim", clip);
    assert_eq!(
        library.clip_key_times("char/walk.ranim"),
        vec![0.0, 1.25, 2.0]
    );
    assert!(library.clip_key_times("char/missing.ranim").is_empty());
}

fn clip_with_notify(time: f32, name: &str) -> Clip {
    let mut clip = bend_clip();
    clip.duration = 1.0;
    clip.looped = true;
    clip.notifies = vec![ClipNotify {
        time,
        name: name.to_string(),
    }];
    clip
}

#[test]
fn a_notify_fires_once_when_the_playhead_crosses_it() {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bend.ranim", clip_with_notify(0.5, "Footstep.L"));

    // 第一帧只建立播放头：不该把整段剪辑的事件一次性播出来。
    animator_system(&mut world, 0.0);
    assert!(world.get_resource::<AnimEvents>().unwrap().is_empty());

    // 0 → 0.4：还没到。
    animator_system(&mut world, 0.4);
    assert!(world.get_resource::<AnimEvents>().unwrap().is_empty());

    // 0.4 → 0.8：跨过 0.5，触发一次。
    animator_system(&mut world, 0.4);
    let events = world.get_resource::<AnimEvents>().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events.events[0].entity, entity);
    assert_eq!(events.events[0].name, "Footstep.L");
    assert!((events.events[0].time - 0.5).abs() < 1e-6);
    assert_eq!(
        events.names_for(entity).collect::<Vec<_>>(),
        vec!["Footstep.L"]
    );

    // 再走一帧（没跨过任何时间点）：事件表被重建为空，不会重复播。
    animator_system(&mut world, 0.1);
    assert!(world.get_resource::<AnimEvents>().unwrap().is_empty());
}

#[test]
fn a_notify_at_the_wrap_point_fires_on_the_next_loop() {
    let (mut world, _entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bend.ranim", clip_with_notify(0.95, "Footstep.R"));

    animator_system(&mut world, 0.0);
    animator_system(&mut world, 0.9); // 到 0.9
    assert!(world.get_resource::<AnimEvents>().unwrap().is_empty());

    // 0.9 → 1.0 → 回绕到 0.0：这一段跨过 0.95。
    animator_system(&mut world, 0.1);
    let names: Vec<String> = world
        .get_resource::<AnimEvents>()
        .unwrap()
        .events
        .iter()
        .map(|event| event.name.clone())
        .collect();
    assert_eq!(names, vec!["Footstep.R".to_string()]);
}

#[test]
fn a_paused_animator_does_not_fire_notifies() {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bend.ranim", clip_with_notify(0.5, "Footstep.L"));
    animator_system(&mut world, 0.0);
    animator_system(&mut world, 0.4);

    world.get_mut::<Animator>(entity).unwrap().playing = false;
    animator_system(&mut world, 0.4);
    assert!(
        world.get_resource::<AnimEvents>().unwrap().is_empty(),
        "暂停时时间不推进，也就不该有事件"
    );
}

#[test]
fn a_reversed_animator_does_not_fire_notifies() {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bend.ranim", clip_with_notify(0.5, "Footstep.L"));
    animator_system(&mut world, 0.0);
    animator_system(&mut world, 0.9);

    world.get_mut::<Animator>(entity).unwrap().speed = -1.0;
    // 0.9 → 0.5 倒着跨过 0.5：倒放不触发（宁可少一次，也不倒着播）。
    animator_system(&mut world, 0.4);
    assert!(world.get_resource::<AnimEvents>().unwrap().is_empty());
}

#[test]
fn leaving_a_blend_space_crossfades_from_the_frozen_pose() {
    // 以前「混合空间 → 单剪辑」是硬切（来源是占位键，库里没有可采样的剪辑）。
    // 现在过渡源是**姿态快照**，所以这里必须真的淡出。
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);
    use_blend_space(&mut world, entity, 0.5);
    world.get_mut::<Animator>(entity).unwrap().playing = false;
    animator_system(&mut world, 0.0);
    let mixed = child_tip(&world, entity);
    assert!(
        (mixed.x - (2.0 - std::f32::consts::FRAC_PI_4.sin())).abs() < 1e-3,
        "混合空间该是 45°，得到 {mixed:?}"
    );

    // 切到单剪辑（90°），1 秒过渡。
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.blend_clip_paths.clear();
        animator.blend_positions.clear();
        animator.clip_path = "bent90.ranim".into();
        animator.blend_duration = 1.0;
        animator.playing = true;
        animator.time = 0.0;
    }
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.x - (2.0 - std::f32::consts::FRAC_PI_4.sin())).abs() < 1e-3,
        "过渡开头仍是 45° 的旧姿态，得到 {tip:?}"
    );

    // 走完：纯 90°。
    animator_system(&mut world, 1.0);
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "过渡完该是 90°，得到 {tip:?}");
}

/// 叠加层测试用：基础剪辑停在 0°，叠加剪辑在 0° 上再转 90°。
fn additive_setup() -> (World, Entity) {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bent90.ranim", bent_90_clip("bent90"));
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.playing = false;
        animator.time = 0.0;
        animator.additive_clip_path = "bent90.ranim".into();
        animator.additive_weight = 1.0;
    }
    (world, entity)
}

#[test]
fn an_additive_layer_rotates_on_top_of_the_base_clip() {
    let (mut world, entity) = additive_setup();
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    // 基础剪辑 0° + 叠加增量 90° = 90°。
    assert!(
        (tip.x - 1.0).abs() < 1e-3 && tip.y.abs() < 1e-3,
        "叠加层该把子骨转到 90°，得到 {tip:?}"
    );
}

#[test]
fn the_additive_weight_scales_the_delta() {
    let (mut world, entity) = additive_setup();
    world.get_mut::<Animator>(entity).unwrap().additive_weight = 0.5;
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.x - (2.0 - std::f32::consts::FRAC_PI_4.sin())).abs() < 1e-3,
        "半个增量 = 45°，得到 {tip:?}"
    );
}

#[test]
fn a_missing_or_disabled_additive_layer_leaves_the_base_pose_alone() {
    // 路径写错（剪辑不在库里）。
    let (mut world, entity) = additive_setup();
    world
        .get_mut::<Animator>(entity)
        .unwrap()
        .additive_clip_path = "missing.ranim".into();
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.y - 1.0).abs() < 1e-4,
        "没有叠加层时保持 0°，得到 {tip:?}"
    );

    // 权重 0 / NaN。
    let (mut world, entity) = additive_setup();
    world.get_mut::<Animator>(entity).unwrap().additive_weight = 0.0;
    animator_system(&mut world, 0.0);
    assert!((child_tip(&world, entity).y - 1.0).abs() < 1e-4);

    let (mut world, entity) = additive_setup();
    world.get_mut::<Animator>(entity).unwrap().additive_weight = f32::NAN;
    animator_system(&mut world, 0.0);
    assert!((child_tip(&world, entity).y - 1.0).abs() < 1e-4);
}

#[test]
fn the_library_reports_clip_notifies_for_the_timeline() {
    let mut library = SkeletonLibrary::new();
    let mut clip = bent_90_clip("with-notify");
    clip.notifies = vec![
        prism_anim::ClipNotify {
            time: 0.25,
            name: "Footstep.L".into(),
        },
        prism_anim::ClipNotify {
            time: 0.5,
            name: "Footstep.R".into(),
        },
    ];
    library.insert_clip("char/walk.ranim", clip);
    assert_eq!(
        library.clip_notifies("char/walk.ranim"),
        vec![
            (0.25, "Footstep.L".to_string()),
            (0.5, "Footstep.R".to_string())
        ]
    );
    assert!(library.clip_notifies("char/missing.ranim").is_empty());
}

/// 一个「idle → run」的两状态机：`speed > 0.5` 时转移。
fn speed_machine(duration: f32, min_state_fraction: f32) -> StateMachine {
    StateMachine {
        states: vec![
            AnimState {
                name: "idle".into(),
                clip_path: "bend.ranim".into(),
                speed: 1.0,
                looped: false,
            },
            AnimState {
                name: "run".into(),
                clip_path: "bent90.ranim".into(),
                speed: 1.0,
                looped: false,
            },
        ],
        transitions: vec![AnimTransition {
            from: 0,
            to: 1,
            min_state_fraction,
            duration,
            conditions: vec![TransitionCondition::FloatGreater {
                name: "speed".into(),
                threshold: 0.5,
            }],
        }],
    }
}

fn install_machine(world: &mut World, machine: StateMachine) {
    let mut library = StateMachineLibrary::new();
    library.insert("hero.rasm", machine);
    world.insert_resource(library);
}

#[test]
fn a_state_machine_drives_the_clip_and_writes_the_state_back() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);
    install_machine(&mut world, speed_machine(0.0, 0.0));
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.state_machine_path = "hero.rasm".into();
        animator.playing = false;
    }

    // 参数为 0：停在 idle（bend.ranim 的 0°）。
    animator_system(&mut world, 0.0);
    assert_eq!(world.get::<Animator>(entity).unwrap().state, "idle");
    let tip = child_tip(&world, entity);
    assert!((tip.y - 1.0).abs() < 1e-4, "idle 该是 0°，得到 {tip:?}");

    // 参数过阈值：下一帧转到 run（bent90.ranim 的 90°）。
    let mut params = prism_anim::AnimParameters::default();
    params.set_float("speed", 1.0);
    world.insert_resource(params);
    animator_system(&mut world, 0.0);
    assert_eq!(world.get::<Animator>(entity).unwrap().state, "run");
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "run 该是 90°，得到 {tip:?}");
    // 换了状态就从 0 重新播，而不是接着 idle 的时间。
    assert!(
        world.get::<AnimatorPlayhead>(entity).unwrap().time.abs() < 1e-6,
        "换状态后时间该归零"
    );
}

#[test]
fn a_transition_duration_overrides_the_component_blend_duration() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);
    // 转移自己带 2 秒时长；组件设成 0（硬切），必须以转移为准。
    install_machine(&mut world, speed_machine(2.0, 0.0));
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.state_machine_path = "hero.rasm".into();
        animator.blend_duration = 0.0;
        animator.playing = false;
    }
    animator_system(&mut world, 0.0);
    let mut params = prism_anim::AnimParameters::default();
    params.set_float("speed", 1.0);
    world.insert_resource(params);

    // 转移的第一帧：权重 0 → 还是 idle 的 0° 姿态。
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.y - 1.0).abs() < 1e-4,
        "过渡开头仍是旧姿态，得到 {tip:?}"
    );

    // 走完 2 秒：90°。
    world.get_mut::<Animator>(entity).unwrap().playing = true;
    animator_system(&mut world, 2.0);
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "过渡完该是 90°，得到 {tip:?}");
}

#[test]
fn min_state_fraction_holds_the_transition_until_the_state_has_played() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);
    install_machine(&mut world, speed_machine(0.0, 0.5));
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.state_machine_path = "hero.rasm".into();
        animator.playing = true;
    }
    let mut params = prism_anim::AnimParameters::default();
    params.set_float("speed", 1.0);
    world.insert_resource(params);

    // 才播了 0.2s（< 50%）：条件成立也不转移。
    animator_system(&mut world, 0.2);
    assert_eq!(world.get::<Animator>(entity).unwrap().state, "idle");

    // 判据用的是**上一帧**的播放时间（那一帧确实播到了那里），所以跨过 50% 的
    // 那一帧还拦着，下一帧才放行。
    animator_system(&mut world, 0.5);
    assert_eq!(world.get::<Animator>(entity).unwrap().state, "idle");

    animator_system(&mut world, 0.0);
    assert_eq!(world.get::<Animator>(entity).unwrap().state, "run");
}

#[test]
fn a_missing_state_machine_falls_back_to_the_component_clip() {
    let (mut world, entity) = world_with_animator("bend.ranim");
    world
        .get_mut::<Animator>(entity)
        .unwrap()
        .state_machine_path = "missing.rasm".into();
    assert_eq!(animator_system(&mut world, 0.0), 1, "坏路径不该让实体消失");
    let tip = child_tip(&world, entity);
    assert!((tip.y - 1.0).abs() < 1e-4, "回落到 clip_path，得到 {tip:?}");
}

/// 把实体切成二维混合空间：`(0,0)` = bend（0°）、`(1,0)` / `(0,1)` = bent90（90°）。
fn use_blend_space_2d(world: &mut World, entity: Entity, x: f32, y: f32) {
    let animator = world.get_mut::<Animator>(entity).unwrap();
    animator.blend_2d_clip_paths = vec![
        "bend.ranim".into(),
        "bent90.ranim".into(),
        "bent90.ranim".into(),
    ];
    animator.blend_2d_positions = vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
    animator.blend_parameter = x;
    animator.blend_parameter_y = y;
    animator.time = 0.0;
    animator.playing = false;
}

#[test]
fn a_two_dimensional_blend_space_interpolates_on_the_parameter_plane() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);

    // 原点样本 = bend 的 0s（0°）。
    use_blend_space_2d(&mut world, entity, 0.0, 0.0);
    animator_system(&mut world, 0.0);
    assert_eq!(
        world.get::<AnimatorPlayhead>(entity).unwrap().clip_path,
        BLEND_SPACE_2D_KEY,
        "二维混合空间用自己的播放键"
    );
    let tip = child_tip(&world, entity);
    assert!((tip.y - 1.0).abs() < 1e-4, "样本原点该是 0°，得到 {tip:?}");

    // 沿 x 轴中点：0° 与 90° 各一半 → 45°。
    use_blend_space_2d(&mut world, entity, 0.5, 0.0);
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    let angle = std::f32::consts::FRAC_PI_4;
    assert!(
        (tip.x - (2.0 - angle.sin())).abs() < 1e-3,
        "x 轴中点该在 45°，得到 {tip:?}"
    );

    // 两个 90° 样本的边上：仍然是 90°。
    use_blend_space_2d(&mut world, entity, 0.5, 0.5);
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!((tip.x - 1.0).abs() < 1e-3, "对边该是 90°，得到 {tip:?}");
}

#[test]
fn a_two_dimensional_blend_space_needs_an_area_and_falls_back() {
    let (mut world, entity) = world_with_animator("");
    install_bent90(&mut world);

    // 只有两个样本：没有三角形，退回单剪辑。
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.blend_2d_clip_paths = vec!["bend.ranim".into(), "bent90.ranim".into()];
        animator.blend_2d_positions = vec![[0.0, 0.0], [1.0, 0.0]];
        animator.playing = false;
    }
    animator_system(&mut world, 0.0);
    assert_eq!(
        world.get::<AnimatorPlayhead>(entity).unwrap().clip_path,
        "bend.ranim"
    );

    // 三个共线样本：三角化失败，同样退回单剪辑。
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.blend_2d_clip_paths = vec![
            "bend.ranim".into(),
            "bent90.ranim".into(),
            "bend.ranim".into(),
        ];
        animator.blend_2d_positions = vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]];
    }
    animator_system(&mut world, 0.0);
    assert_eq!(
        world.get::<AnimatorPlayhead>(entity).unwrap().clip_path,
        "bend.ranim"
    );
}

/// 动画图层测试用：基础剪辑停在 0°，图层是 90° 的 bent90。
fn layered_setup() -> (World, Entity) {
    let (mut world, entity) = world_with_animator("");
    world
        .get_resource_mut::<SkeletonLibrary>()
        .unwrap()
        .insert_clip("bent90.ranim", bent_90_clip("bent90"));
    {
        let animator = world.get_mut::<Animator>(entity).unwrap();
        animator.playing = false;
        animator.time = 0.0;
    }
    (world, entity)
}

fn push_layer(world: &mut World, entity: Entity, layer: AnimLayer) {
    world
        .get_mut::<Animator>(entity)
        .unwrap()
        .layers
        .push(layer);
}

#[test]
fn an_animation_layer_blends_over_the_base_pose() {
    let (mut world, entity) = layered_setup();
    push_layer(
        &mut world,
        entity,
        AnimLayer {
            clip_path: "bent90.ranim".into(),
            weight: 1.0,
            ..Default::default()
        },
    );
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.x - 1.0).abs() < 1e-3 && tip.y.abs() < 1e-3,
        "整层 90° 应覆盖 0° 基础姿态，得到 {tip:?}"
    );

    // 权重 0.5：只转 45°。
    world.get_mut::<Animator>(entity).unwrap().layers[0].weight = 0.5;
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.x - (2.0 - std::f32::consts::FRAC_PI_4.sin())).abs() < 1e-3,
        "半权重该是 45°，得到 {tip:?}"
    );
}

#[test]
fn a_layer_bone_mask_limits_which_joints_move() {
    let (mut world, entity) = layered_setup();
    push_layer(
        &mut world,
        entity,
        AnimLayer {
            clip_path: "bent90.ranim".into(),
            weight: 1.0,
            bone_mask: vec!["root".into()],
            ..Default::default()
        },
    );
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.y - 1.0).abs() < 1e-4 && (tip.x - 2.0).abs() < 1e-4,
        "遮罩只含 root，child 不该转，得到 {tip:?}"
    );
}

#[test]
fn an_additive_layer_uses_the_reference_delta() {
    let (mut world, entity) = layered_setup();
    push_layer(
        &mut world,
        entity,
        AnimLayer {
            clip_path: "bent90.ranim".into(),
            weight: 1.0,
            additive: true,
            ..Default::default()
        },
    );
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.x - 1.0).abs() < 1e-3 && tip.y.abs() < 1e-3,
        "additive 90° 应加到 0° 基础姿态，得到 {tip:?}"
    );
}

#[test]
fn a_missing_or_zero_weight_layer_is_ignored() {
    let (mut world, entity) = layered_setup();
    push_layer(
        &mut world,
        entity,
        AnimLayer {
            clip_path: "missing.ranim".into(),
            weight: 1.0,
            ..Default::default()
        },
    );
    push_layer(
        &mut world,
        entity,
        AnimLayer {
            clip_path: "bent90.ranim".into(),
            weight: f32::NAN,
            ..Default::default()
        },
    );
    animator_system(&mut world, 0.0);
    let tip = child_tip(&world, entity);
    assert!(
        (tip.y - 1.0).abs() < 1e-4,
        "无效图层不该改变基础姿态，得到 {tip:?}"
    );
}
