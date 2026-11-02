//! Animation sampling and playback scenarios.
//!
//! The pose tests exercise `prism-anim`'s pure sampling (multi-track, loop
//! boundaries, deterministic tolerance-based comparison). The playback tests
//! drive the real engine system at a fixed step and assert the event sequence,
//! playhead state and sampled transform for forward, reverse, paused, looping
//! and one-shot clips.

mod animation_support;

use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};

use animation_support::{
    assert_pose_close, engine_clip, engine_events, identity_pose, pose_fixture, sample_clip,
    AnimationHarness, JointSnapshot, PoseSnapshot,
};
use glam::Quat;
use prism_scene::animation::AnimationEvent;

fn assert_vec3(actual: [f32; 3], expected: [f32; 3], tolerance: f32) {
    for (axis, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
        assert!(
            (actual - expected).abs() <= tolerance,
            "component {axis}: {actual} != {expected} (tolerance {tolerance})"
        );
    }
}

fn assert_quat(actual: [f32; 4], expected: Quat, tolerance: f32) {
    let expected = expected.to_array();
    let dot: f32 = actual.iter().zip(expected.iter()).map(|(a, b)| a * b).sum();
    let sign = if dot < 0.0 { -1.0 } else { 1.0 };
    for (axis, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
        let expected = sign * expected;
        assert!(
            (actual - expected).abs() <= tolerance,
            "rotation component {axis}: {actual} != {expected} (tolerance {tolerance})"
        );
    }
}

fn joint(translation: [f32; 3], rotation: Quat, scale: [f32; 3]) -> JointSnapshot {
    JointSnapshot::new(translation, rotation, scale)
}

fn pose(joints: Vec<JointSnapshot>) -> PoseSnapshot {
    PoseSnapshot { joints }
}

#[test]
fn pose_samples_are_deterministic_and_multi_track() {
    let clip = pose_fixture(false);
    let base = identity_pose(2);
    let times = [-1.0, 0.0, 0.5, 1.0, 1.5, 2.0, 3.0];

    let first: Vec<_> = times
        .iter()
        .map(|time| sample_clip(&clip, *time, &base))
        .collect();
    let second: Vec<_> = times
        .iter()
        .map(|time| sample_clip(&clip, *time, &base))
        .collect();
    for (actual, expected) in first.iter().zip(&second) {
        // Re-sampling the same time must be bit-stable in this pure path.
        assert_pose_close(actual, expected, 0.0);
    }

    // Before the first key: endpoint hold.
    assert_pose_close(
        &first[0],
        &pose(vec![
            joint([0.0, 0.0, 0.0], Quat::IDENTITY, [1.0, 1.0, 1.0]),
            joint([0.0, 1.0, 0.0], Quat::IDENTITY, [1.0, 1.0, 1.0]),
        ]),
        1e-6,
    );

    // Halfway through joint 0's translation (0→4 over 1s), rotation (0→90°
    // over 2s) and scale (1→2 over 2s); joint 1's scale peaks at 1s.
    assert_pose_close(
        &first[2],
        &pose(vec![
            joint(
                [2.0, 0.0, 0.0],
                Quat::from_rotation_y(PI / 8.0),
                [1.25, 1.25, 1.25],
            ),
            joint([0.0, 1.5, 0.0], Quat::IDENTITY, [1.0, 1.0, 2.0]),
        ]),
        1e-6,
    );

    // Second key boundary: every channel has an explicit value.
    assert_pose_close(
        &first[3],
        &pose(vec![
            joint(
                [4.0, 0.0, 0.0],
                Quat::from_rotation_y(FRAC_PI_4),
                [1.5, 1.5, 1.5],
            ),
            joint([0.0, 2.0, 0.0], Quat::IDENTITY, [1.0, 1.0, 3.0]),
        ]),
        1e-6,
    );

    // Between the two 1s keys on joint 1's scale: interpolate back down.
    assert_pose_close(
        &first[4],
        &pose(vec![
            joint(
                [4.0, 1.0, 0.0],
                Quat::from_rotation_y(3.0 * PI / 8.0),
                [1.75, 1.75, 1.75],
            ),
            joint([0.0, 2.5, 0.0], Quat::IDENTITY, [1.0, 1.0, 2.0]),
        ]),
        1e-6,
    );

    // Endpoint hold after the clip and before it are the same sample.
    let end = pose(vec![
        joint(
            [4.0, 2.0, 0.0],
            Quat::from_rotation_y(FRAC_PI_2),
            [2.0, 2.0, 2.0],
        ),
        joint([0.0, 3.0, 0.0], Quat::IDENTITY, [1.0, 1.0, 1.0]),
    ]);
    assert_pose_close(&first[5], &end, 1e-6);
    assert_pose_close(&first[6], &end, 1e-6);
}

#[test]
fn looped_sampling_handles_the_boundary_just_before_and_after() {
    let clip = pose_fixture(true);
    let base = identity_pose(2);

    let before = sample_clip(&clip, 1.999, &base);
    assert_vec3(before.joint(0).translation, [4.0, 1.998, 0.0], 1e-4);
    assert_vec3(before.joint(1).translation, [0.0, 2.999, 0.0], 1e-4);

    // Exactly at duration wraps to the first key...
    let boundary = sample_clip(&clip, 2.0, &base);
    let start = sample_clip(&clip, 0.0, &base);
    assert_pose_close(&boundary, &start, 1e-6);

    // ...and just after the boundary is just after the first key.
    let after = sample_clip(&clip, 2.001, &base);
    assert_vec3(after.joint(0).translation, [0.004, 0.0, 0.0], 1e-4);
    assert_vec3(after.joint(1).translation, [0.0, 1.001, 0.0], 1e-4);

    // Multiple wraps stay deterministic.
    let wrapped = sample_clip(&clip, 2.5, &base);
    let half = sample_clip(&clip, 0.5, &base);
    assert_pose_close(&wrapped, &half, 1e-5);
}

#[test]
fn forward_playback_fires_events_in_order_and_samples_the_pose() {
    let mut harness = AnimationHarness::new(engine_clip(false, 1.0, engine_events()), 0.1);
    let ticks = harness.run(12);

    assert_eq!(
        harness.event_names(),
        vec![
            (1, "start".to_string()),
            (3, "quarter".to_string()),
            (5, "half".to_string()),
            (9, "late".to_string()),
        ]
    );

    // Step 5 reached 0.5s: position 5, euler 45°, scale 1.5.
    let half = &ticks[4];
    assert_vec3(half.position, [5.0, 0.0, 0.0], 1e-4);
    assert_vec3(half.scale, [1.5, 1.5, 1.5], 1e-4);
    assert_quat(half.rotation, Quat::from_rotation_y(FRAC_PI_4), 1e-5);
    assert!(!half.finished);

    // A one-shot clip holds the last key and reports finished thereafter.
    let last = &ticks[11];
    assert_eq!(last.playhead_time, 1.0);
    assert!(last.finished);
    assert_vec3(last.position, [10.0, 0.0, 0.0], 1e-4);
    assert_vec3(last.scale, [2.0, 2.0, 2.0], 1e-4);
    assert_quat(last.rotation, Quat::from_rotation_y(FRAC_PI_2), 1e-5);

    // Holding the endpoint must not replay the last event.
    let after = harness.run(5);
    assert_eq!(harness.event_names().len(), 4);
    for tick in after {
        assert!(tick.events.is_empty(), "endpoint replay: {tick:?}");
        assert!(tick.finished);
        assert_vec3(tick.position, [10.0, 0.0, 0.0], 1e-4);
    }
}

#[test]
fn reverse_playback_fires_events_backwards() {
    // Times sit away from exact tick boundaries so f32 accumulation cannot
    // move the crossing into a neighbouring step.
    let events = vec![
        AnimationEvent::new(0.22, "quarter"),
        AnimationEvent::new(0.48, "half"),
        AnimationEvent::new(0.88, "late"),
    ];
    let mut harness =
        AnimationHarness::new(engine_clip(false, -1.0, events), 0.1).with_playhead(1.0);
    let ticks = harness.run(10);

    assert_eq!(
        harness.event_names(),
        vec![
            (2, "late".to_string()),
            (6, "half".to_string()),
            (8, "quarter".to_string()),
        ]
    );

    // Step 6 is at 0.4s: position 4, euler 36°, scale 1.4.
    let tick = &ticks[5];
    assert_vec3(tick.position, [4.0, 0.0, 0.0], 1e-4);
    assert_vec3(tick.scale, [1.4, 1.4, 1.4], 1e-4);
    assert_quat(
        tick.rotation,
        Quat::from_rotation_y(36.0f32.to_radians()),
        1e-4,
    );

    // The playhead reaches 0 without claiming the one-shot clip finished.
    assert!(harness.last().playhead_time.abs() < 1e-5);
    assert!(!harness.last().finished);
}

#[test]
fn paused_playback_holds_the_pose_and_emits_no_events() {
    let mut clip = engine_clip(false, 1.0, engine_events());
    clip.playing = false;
    let mut harness = AnimationHarness::new(clip, 0.1);
    let ticks = harness.run(20);

    assert!(harness.events().is_empty());
    assert!(ticks.iter().all(|tick| tick.playhead_time == 0.0));
    for tick in &ticks {
        assert_vec3(tick.position, [0.0, 0.0, 0.0], 1e-6);
        assert_vec3(tick.scale, [1.0, 1.0, 1.0], 1e-6);
        assert_quat(tick.rotation, Quat::IDENTITY, 1e-6);
        assert!(!tick.finished);
    }
}

#[test]
fn looped_playback_fires_events_once_per_cycle() {
    let events = vec![
        AnimationEvent::new(0.05, "early"),
        AnimationEvent::new(0.85, "late"),
    ];
    let mut harness = AnimationHarness::new(engine_clip(true, 1.0, events), 0.1);
    let ticks = harness.run(21);

    assert_eq!(
        harness.event_names(),
        vec![
            (1, "early".to_string()),
            (9, "late".to_string()),
            (11, "early".to_string()),
            (19, "late".to_string()),
            (21, "early".to_string()),
        ]
    );

    // Step 10 is exactly the wrap: local time 0, so the pose is back at start.
    assert!((ticks[9].playhead_time - 1.0).abs() < 1e-4);
    assert_vec3(ticks[9].position, [0.0, 0.0, 0.0], 1e-5);
    // Step 11 is 0.1s into the second cycle.
    assert_vec3(ticks[10].position, [1.0, 0.0, 0.0], 1e-4);
    assert!(!ticks[10].finished);
}

#[test]
fn one_shot_event_at_the_end_is_not_repeated() {
    let events = vec![AnimationEvent::new(1.0, "done")];
    let mut harness = AnimationHarness::new(engine_clip(false, 1.0, events), 0.25);
    let ticks = harness.run(8);

    // The playhead reaches 1.0 on step 4 and is clamped there afterwards.
    assert_eq!(harness.event_names(), vec![(4, "done".to_string())]);
    assert!(ticks[3].finished);
    assert_eq!(ticks[3].playhead_time, 1.0);
    for tick in &ticks[4..] {
        assert!(tick.events.is_empty(), "end event repeated: {tick:?}");
    }
}
