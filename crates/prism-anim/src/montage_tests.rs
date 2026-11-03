use std::sync::Arc;

use glam::Vec3;

use super::{Montage, MontagePlayer, MAX_MONTAGE_QUEUE};
use crate::clip::{Clip, ClipNotify, ClipSection, Track, Vec3Key};
use crate::pose::Pose;
use crate::skeleton::JointTransform;

fn pose() -> Pose {
    Pose::from_locals(vec![JointTransform::IDENTITY])
}

fn clip(name: &str, x: f32, notify: Option<(&str, f32)>) -> Arc<Clip> {
    Arc::new(Clip {
        name: name.into(),
        duration: 1.0,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: vec![Track {
            joint: 0,
            translation: vec![
                Vec3Key {
                    time: 0.0,
                    value: Vec3::new(x, 0.0, 0.0),
                },
                Vec3Key {
                    time: 1.0,
                    value: Vec3::new(x, 0.0, 0.0),
                },
            ],
            rotation: Vec::new(),
            scale: Vec::new(),
        }],
        notifies: notify
            .map(|(name, time)| {
                vec![ClipNotify {
                    time,
                    name: name.into(),
                }]
            })
            .unwrap_or_default(),
        sync_markers: Vec::new(),
        sections: Vec::new(),
    })
}

fn sectioned_clip() -> Arc<Clip> {
    let mut clip = (*clip("sectioned", 0.0, Some(("Step", 0.6)))).clone();
    clip.sections = vec![
        ClipSection {
            name: "Start".into(),
            start: 0.0,
            end: 0.4,
        },
        ClipSection {
            name: "Vault".into(),
            start: 0.4,
            end: 0.8,
        },
        ClipSection {
            name: "Land".into(),
            start: 0.8,
            end: 1.0,
        },
    ];
    Arc::new(clip)
}

fn sample(player: &MontagePlayer) -> f32 {
    let mut out = Pose::default();
    assert!(player.sample_into(&pose(), &mut out));
    out.local(0).unwrap().translation.x
}

#[test]
fn play_now_starts_the_montage_immediately() {
    let mut player = MontagePlayer::new();
    assert!(!player.is_playing());
    player.play_now(Montage::new(clip("a", 5.0, None)).with_weight(0.5));
    assert!(player.is_playing());
    assert_eq!(player.active_weight(), 0.5);
    assert!((sample(&player) - 5.0).abs() < 1e-5);
}

#[test]
fn a_non_looping_montage_finishes_and_starts_the_next_queued_one() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(clip("a", 1.0, None)).with_looping(false));
    player.enqueue(Montage::new(clip("b", 2.0, None)));
    assert_eq!(player.queue_len(), 1);

    assert!(player.tick(0.5));
    assert!((sample(&player) - 1.0).abs() < 1e-5);
    assert!(player.tick(0.6), "finishing A starts B");
    assert!(!player.is_finished());
    assert!((sample(&player) - 2.0).abs() < 1e-5);
    assert_eq!(player.queue_len(), 0);
}

#[test]
fn finishing_the_last_montage_marks_the_player_finished() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(clip("a", 1.0, None)).with_looping(false));
    assert!(player.tick(0.6));
    assert!(!player.tick(0.5), "the last tick finishes the last montage");
    assert!(player.is_finished());
    assert!(!player.is_playing());
}

#[test]
fn stop_and_stop_after_current_clear_the_right_state() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(clip("a", 1.0, None)).with_looping(false));
    player.enqueue(Montage::new(clip("b", 2.0, None)));
    player.stop_after_current();
    assert_eq!(player.queue_len(), 0);
    assert!(player.is_playing());
    player.stop();
    assert!(!player.is_playing());
    assert!(player.active_clip().is_none());
}

#[test]
fn looping_montages_emit_notifies_across_the_wrap() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(clip("a", 0.0, Some(("Step", 0.9)))));
    assert!(player.tick(0.5));
    assert!(player.take_notifies().is_empty());
    assert!(player.tick(0.5));
    let events = player.take_notifies();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "Step");
}

#[test]
fn start_time_looping_and_invalid_values_are_sanitized() {
    let mut player = MontagePlayer::new();
    player.play_now(
        Montage::new(clip("a", 1.0, None))
            .with_start_time(1.75)
            .with_looping(true)
            .with_speed(f32::NAN)
            .with_weight(5.0),
    );
    assert_eq!(player.active_weight(), 1.0);
    // start time wraps to 0.75; sampling the constant track still gives 1.
    assert!((sample(&player) - 1.0).abs() < 1e-5);
    player.tick(0.5);
    assert!(player.is_playing());
}

#[test]
fn the_queue_is_bounded() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(clip("a", 0.0, None)));
    for _ in 0..MAX_MONTAGE_QUEUE {
        assert!(player.enqueue(Montage::new(clip("q", 0.0, None))));
    }
    assert!(!player.enqueue(Montage::new(clip("overflow", 0.0, None))));
    assert_eq!(player.queue_len(), MAX_MONTAGE_QUEUE);
}

#[test]
fn a_paused_player_freezes_time_and_notifies_until_resumed() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(clip("hit", 1.0, Some(("step", 0.2)))));
    player.tick(0.1);
    assert!((player.active_time().unwrap() - 0.1).abs() < 1e-5);
    player.take_notifies();

    player.set_paused(true);
    assert!(player.is_paused());
    assert!(player.tick(0.5), "暂停时仍然在播");
    assert!(
        (player.active_time().unwrap() - 0.1).abs() < 1e-5,
        "暂停不推进时间"
    );
    assert!(player.take_notifies().is_empty(), "暂停不发 Notify");

    player.set_paused(false);
    assert!(!player.is_paused());
    player.tick(0.1);
    assert!((player.active_time().unwrap() - 0.2).abs() < 1e-5);
    let notifies = player.take_notifies();
    assert_eq!(notifies.len(), 1, "恢复后跨过的 Notify 才发");
    assert_eq!(notifies[0].name, "step");
}

#[test]
fn playing_a_new_montage_resumes_the_player() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(clip("a", 1.0, None)));
    player.set_paused(true);
    player.play_now(Montage::new(clip("b", 2.0, None)));
    assert!(!player.is_paused(), "显式播放会解除暂停");
    assert!((player.active_time().unwrap()).abs() < 1e-5);
}

#[test]
fn a_montage_can_start_at_a_named_section() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(sectioned_clip()).with_start_section("Vault"));
    assert!((player.active_time().unwrap() - 0.4).abs() < 1e-5);
    assert_eq!(
        player.active_section().map(|section| section.name.as_str()),
        Some("Vault")
    );

    // 段名不存在时回退到 start_time，而不是卡在 0 或 panic。
    player.play_now(
        Montage::new(sectioned_clip())
            .with_start_section("Missing")
            .with_start_time(0.9),
    );
    assert!((player.active_time().unwrap() - 0.9).abs() < 1e-5);
    assert_eq!(
        player.active_section().map(|section| section.name.as_str()),
        Some("Land")
    );
}

#[test]
fn jumping_to_a_section_moves_the_playhead_and_clears_pending_notifies() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(sectioned_clip()).with_looping(false));
    player.tick(0.5);
    assert_eq!(
        player.active_section().map(|section| section.name.as_str()),
        Some("Vault")
    );

    assert!(player.jump_to_section("Land"));
    assert!((player.active_time().unwrap() - 0.8).abs() < 1e-5);
    assert_eq!(
        player.active_section().map(|section| section.name.as_str()),
        Some("Land")
    );
    assert!(player.take_notifies().is_empty(), "跳段不应重放中间 Notify");
    assert!(!player.jump_to_section("Missing"));
    assert!((player.active_time().unwrap() - 0.8).abs() < 1e-5);
}

#[test]
fn section_playback_keeps_the_paused_state() {
    let mut player = MontagePlayer::new();
    player.play_now(Montage::new(sectioned_clip()));
    player.set_paused(true);
    assert!(player.jump_to_section("Vault"));
    assert!(player.is_paused());
    assert!((player.active_time().unwrap() - 0.4).abs() < 1e-5);
    assert!(player.tick(0.2));
    assert!((player.active_time().unwrap() - 0.4).abs() < 1e-5);
}
