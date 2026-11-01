//! `keyframe_animation_system` 测试。

use super::*;
use crate::scene::components::KeyframeAnimation;
use prism_scene::animation::{AnimationEvent, Vec3Key};

fn clip(position: &[(f32, [f32; 3])], euler: &[(f32, [f32; 3])]) -> KeyframeAnimation {
    KeyframeAnimation {
        position: position
            .iter()
            .map(|(time, value)| Vec3Key::new(*time, *value))
            .collect(),
        euler_xyz: euler
            .iter()
            .map(|(time, value)| Vec3Key::new(*time, *value))
            .collect(),
        ..KeyframeAnimation::default()
    }
}

fn spawn(world: &mut World, animation: KeyframeAnimation) -> Entity {
    let entity = world.spawn();
    world.insert(entity, LocalTransform::default());
    world.insert(entity, animation);
    entity
}

fn transform_of(world: &World, entity: Entity) -> LocalTransform {
    world.get::<LocalTransform>(entity).unwrap().clone()
}

#[test]
fn playback_writes_the_sampled_position_into_the_transform() {
    let mut world = World::new();
    let entity = spawn(
        &mut world,
        clip(&[(0.0, [0.0; 3]), (2.0, [4.0, 0.0, 0.0])], &[]),
    );

    keyframe_animation_system(&mut world, 1.0);
    assert!((transform_of(&world, entity).position.x - 2.0).abs() < 1e-5);
}

#[test]
fn a_paused_clip_keeps_the_current_pose() {
    let mut world = World::new();
    let entity = spawn(
        &mut world,
        KeyframeAnimation {
            playing: false,
            ..clip(&[(0.0, [1.0, 0.0, 0.0]), (4.0, [5.0, 0.0, 0.0])], &[])
        },
    );

    for _ in 0..10 {
        keyframe_animation_system(&mut world, 1.0);
    }
    assert!((transform_of(&world, entity).position.x - 1.0).abs() < 1e-5);
}

#[test]
fn a_looped_clip_wraps_the_playhead() {
    let mut world = World::new();
    let entity = spawn(
        &mut world,
        clip(&[(0.0, [0.0; 3]), (1.0, [10.0, 0.0, 0.0])], &[]),
    );

    // 1.5 秒 → 一个完整循环 + 半程。
    keyframe_animation_system(&mut world, 1.5);
    let head = *world.get::<ClipPlayhead>(entity).unwrap();
    assert!(head.time > 1.5 - 1e-4);
    assert!(!head.finished);
    assert!((transform_of(&world, entity).position.x - 5.0).abs() < 1e-4);
}

#[test]
fn a_single_shot_clip_holds_the_last_key_and_reports_finished() {
    let mut world = World::new();
    let entity = spawn(
        &mut world,
        KeyframeAnimation {
            looped: false,
            ..clip(&[(0.0, [0.0; 3]), (1.0, [10.0, 0.0, 0.0])], &[])
        },
    );

    for _ in 0..5 {
        keyframe_animation_system(&mut world, 1.0);
    }
    let head = *world.get::<ClipPlayhead>(entity).unwrap();
    assert!(head.finished);
    assert!((head.time - 1.0).abs() < 1e-5, "playhead clamps at the end");
    assert!((transform_of(&world, entity).position.x - 10.0).abs() < 1e-5);
}

#[test]
fn speed_scales_the_playhead() {
    let mut world = World::new();
    let entity = spawn(
        &mut world,
        KeyframeAnimation {
            speed: 2.0,
            phase: 0.5,
            ..clip(&[(0.0, [0.0; 3]), (2.0, [2.0, 0.0, 0.0])], &[])
        },
    );

    // dt 0.25 × speed 2 = 0.5 秒，加上 phase 0.5 → 采样 1.0 秒 = 中间值。
    keyframe_animation_system(&mut world, 0.25);
    assert!((transform_of(&world, entity).position.x - 1.0).abs() < 1e-4);
}

#[test]
fn channels_without_keys_keep_the_authored_transform() {
    let mut world = World::new();
    let entity = spawn(
        &mut world,
        clip(&[(0.0, [3.0, 0.0, 0.0]), (1.0, [3.0, 0.0, 0.0])], &[]),
    );
    {
        let transform = world.get_mut::<LocalTransform>(entity).unwrap();
        transform.position = glam::Vec3::new(-1.0, 0.0, 0.0);
        transform.scale = glam::Vec3::new(2.0, 3.0, 4.0);
    }

    keyframe_animation_system(&mut world, 0.5);
    let transform = transform_of(&world, entity);
    // position 被轨道接管……
    assert!((transform.position.x - 3.0).abs() < 1e-5);
    // ……旋转 / 缩放没有关键帧，保持作者值。
    assert_eq!(transform.rotation, glam::Quat::IDENTITY);
    assert_eq!(transform.scale, glam::Vec3::new(2.0, 3.0, 4.0));
}

#[test]
fn euler_keys_are_degrees_and_rotate_the_transform() {
    let mut world = World::new();
    // 循环剪辑在 `time == duration` 处会绕回 0，检查末帧用单次播放。
    let entity = spawn(
        &mut world,
        KeyframeAnimation {
            looped: false,
            ..clip(&[], &[(0.0, [0.0; 3]), (1.0, [0.0, 90.0, 0.0])])
        },
    );

    keyframe_animation_system(&mut world, 1.0);
    let rotation = transform_of(&world, entity).rotation;
    let forward = rotation * glam::Vec3::Z;
    // 绕 Y 轴 +90°：+Z 转到 +X。
    assert!((forward.x - 1.0).abs() < 1e-5);
    assert!(forward.z.abs() < 1e-5);
}

#[test]
fn an_empty_clip_attaches_no_playhead() {
    let mut world = World::new();
    let entity = spawn(&mut world, KeyframeAnimation::default());

    keyframe_animation_system(&mut world, 1.0);
    assert!(world.get::<ClipPlayhead>(entity).is_none());
    let transform = transform_of(&world, entity);
    assert_eq!(transform.position, glam::Vec3::ZERO);
    assert_eq!(transform.rotation, glam::Quat::IDENTITY);
    assert_eq!(transform.scale, glam::Vec3::ONE);
}

#[test]
fn an_entity_without_a_transform_is_left_alone() {
    let mut world = World::new();
    let entity = world.spawn();
    world.insert(entity, clip(&[(0.0, [0.0; 3]), (1.0, [1.0; 3])], &[]));

    keyframe_animation_system(&mut world, 1.0);
    assert!(world.get::<ClipPlayhead>(entity).is_none());
}

fn fired(world: &World) -> Vec<(String, f32)> {
    world
        .get_resource::<AnimationEventQueue>()
        .map(|queue| {
            queue
                .events
                .iter()
                .map(|event| (event.name.clone(), event.time))
                .collect()
        })
        .unwrap_or_default()
}

/// #490：播放头穿过事件时间时入队；没穿过的帧不发。
#[test]
fn events_fire_when_the_playhead_crosses_them() {
    let mut world = World::new();
    let entity = spawn(
        &mut world,
        KeyframeAnimation {
            looped: false,
            events: vec![
                AnimationEvent::new(0.5, "step"),
                AnimationEvent::new(1.5, "land"),
            ],
            ..clip(&[(0.0, [0.0; 3]), (2.0, [10.0; 3])], &[])
        },
    );

    keyframe_animation_system(&mut world, 0.4);
    assert!(fired(&world).is_empty());

    keyframe_animation_system(&mut world, 0.2);
    assert_eq!(fired(&world), vec![("step".to_string(), 0.5)]);
    assert_eq!(
        world.get_resource::<AnimationEventQueue>().unwrap().events[0].entity,
        entity
    );

    keyframe_animation_system(&mut world, 0.8);
    assert!(fired(&world).is_empty(), "1.5 has not been crossed yet");

    keyframe_animation_system(&mut world, 0.3);
    assert_eq!(fired(&world), vec![("land".to_string(), 1.5)]);

    // 夹在末帧之后不再重复。
    keyframe_animation_system(&mut world, 0.5);
    assert!(fired(&world).is_empty());
}

#[test]
fn a_zero_time_event_fires_on_the_first_advance() {
    let mut world = World::new();
    spawn(
        &mut world,
        KeyframeAnimation {
            looped: false,
            events: vec![AnimationEvent::new(0.0, "start")],
            ..clip(&[(0.0, [0.0; 3]), (1.0, [1.0; 3])], &[])
        },
    );

    keyframe_animation_system(&mut world, 0.1);
    assert_eq!(fired(&world), vec![("start".to_string(), 0.0)]);
}

#[test]
fn a_looped_clip_fires_events_once_per_cycle() {
    let mut world = World::new();
    spawn(
        &mut world,
        KeyframeAnimation {
            events: vec![AnimationEvent::new(0.25, "tick")],
            ..clip(&[(0.0, [0.0; 3]), (1.0, [10.0; 3])], &[])
        },
    );

    keyframe_animation_system(&mut world, 0.3);
    assert_eq!(fired(&world), vec![("tick".to_string(), 0.25)]);
    keyframe_animation_system(&mut world, 0.3);
    assert!(fired(&world).is_empty());
    keyframe_animation_system(&mut world, 0.3);
    assert!(fired(&world).is_empty());
    // 0.9 → 1.2：回绕到 0.2，还没到 0.25。
    keyframe_animation_system(&mut world, 0.3);
    assert!(fired(&world).is_empty());
    // 0.2 → 0.5：第二圈的 tick。
    keyframe_animation_system(&mut world, 0.3);
    assert_eq!(fired(&world), vec![("tick".to_string(), 0.25)]);
}

#[test]
fn paused_clips_do_not_fire_events_and_the_queue_is_cleared() {
    let mut world = World::new();
    spawn(
        &mut world,
        KeyframeAnimation {
            playing: false,
            events: vec![AnimationEvent::new(0.1, "never")],
            ..clip(&[(0.0, [0.0; 3]), (1.0, [1.0; 3])], &[])
        },
    );

    keyframe_animation_system(&mut world, 1.0);
    assert!(fired(&world).is_empty());

    // 上一帧有事件、下一帧没有时也必须被清空。
    let stale_entity = world.spawn();
    world.insert_resource(AnimationEventQueue {
        events: vec![FiredAnimationEvent {
            entity: stale_entity,
            name: "stale".to_string(),
            time: 99.0,
        }],
    });
    keyframe_animation_system(&mut world, 1.0);
    assert!(fired(&world).is_empty());
}

#[test]
fn a_clip_with_only_events_still_advances_and_fires() {
    let mut world = World::new();
    spawn(
        &mut world,
        KeyframeAnimation {
            looped: false,
            events: vec![AnimationEvent::new(0.2, "cue")],
            position: Vec::new(),
            euler_xyz: Vec::new(),
            scale: Vec::new(),
            ..KeyframeAnimation::default()
        },
    );

    keyframe_animation_system(&mut world, 0.3);
    assert_eq!(fired(&world), vec![("cue".to_string(), 0.2)]);
}
