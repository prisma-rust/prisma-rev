//! Deterministic fixed-step simulation scenarios.
//!
//! These tests drive `Engine::update` at a fixed timestep with no window, GPU,
//! wall clock or sleeps. Every scenario asserts on the same `Snapshot` shape the
//! harness exposes to agents and reviewers.

mod support;

use prism_engine::input::KeyCode;
use prism_engine::scene::components::{KeyframeAnimation, LocalTransform};
use prism_engine::scene::systems::animation::keyframe_animation_system;
use prism_engine::ui::{ui_layout_system, ui_render_system, ScreenSize, Style};
use prism_scene::animation::{AnimationEvent, Vec3Key};
use support::{reparent, ScriptedInput, SimHarness, Snapshot};

fn approx(actual: [f32; 3], expected: [f32; 3], tolerance: f32) {
    for (index, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
        assert!(
            (actual - expected).abs() <= tolerance,
            "component {index}: {actual} != {expected} (tol {tolerance})"
        );
    }
}

fn hierarchy_fixture() -> &'static str {
    r#"{
        "version": 3,
        "entities": [
            {
                "id": 1,
                "name": "Root",
                "parent": null,
                "transform": {
                    "position": [1.0, 2.0, 3.0],
                    "rotation": [0.0, 0.0, 0.0, 1.0],
                    "scale": [1.0, 1.0, 1.0]
                },
                "components": {}
            },
            {
                "id": 2,
                "name": "Child",
                "parent": 1,
                "transform": {
                    "position": [0.0, 1.0, 0.0],
                    "rotation": [0.0, 0.0, 0.0, 1.0],
                    "scale": [1.0, 1.0, 1.0]
                },
                "components": {}
            }
        ]
    }"#
}

#[test]
fn hierarchy_propagation_and_scene_requests_are_observable() {
    let mut harness = SimHarness::new(1).with_step(1.0 / 60.0);
    harness.load_scene_json(hierarchy_fixture());

    assert_eq!(harness.tick_count(), 0);
    let first = harness.tick();
    assert_eq!(harness.tick_count(), 1);
    assert!((harness.step() - 1.0 / 60.0).abs() < 1e-8);
    assert_eq!(first.scene.scene_members, 2);
    assert_eq!(first.scene.generation, 1);
    assert!(
        first.scene.current_scene.is_none(),
        "author JSON has no manifest name"
    );
    assert!(
        first
            .scene
            .requests
            .iter()
            .any(|request| request.starts_with("load:")),
        "load request must be captured: {:?}",
        first.scene.requests
    );

    let child = &first.entities["Child"];
    approx(
        child.world_position.expect("child world position"),
        [1.0, 3.0, 3.0],
        1e-5,
    );

    let root = harness.entity("Root");
    harness
        .world_mut()
        .get_mut::<LocalTransform>(root)
        .expect("root transform")
        .position
        .x += 1.0;
    let second = harness.tick();
    approx(
        second.entities["Child"]
            .world_position
            .expect("child world position"),
        [2.0, 3.0, 3.0],
        1e-5,
    );

    let diffs = first.diff(&second, 1e-5);
    assert!(
        diffs
            .iter()
            .any(|diff| diff.path == "entities.Child.world_position[0]"),
        "diff must name the entity and field: {diffs:?}"
    );

    let removed = harness.unload_scene();
    assert_eq!(removed, 2);
    let third = harness.checkpoint();
    assert_eq!(third.scene.scene_members, 0);
    assert_eq!(third.scene.generation, second.scene.generation + 1);
    assert!(
        third
            .scene
            .requests
            .last()
            .is_some_and(|request| request.starts_with("unload:")),
        "unload request must be captured: {:?}",
        third.scene.requests
    );
    assert!(third.entities.is_empty());
}

#[test]
fn animation_events_fire_on_fixed_tick_boundaries() {
    let mut harness = SimHarness::new(7).with_step(0.1);
    let actor = harness.spawn_transformed("Actor", [0.0, 0.0, 0.0]);
    harness.world_mut().insert(
        actor,
        KeyframeAnimation {
            name: "walk".into(),
            duration: 1.0,
            looped: false,
            position: vec![
                Vec3Key::new(0.0, [0.0, 0.0, 0.0]),
                Vec3Key::new(1.0, [10.0, 0.0, 0.0]),
            ],
            events: vec![
                AnimationEvent::new(0.25, "step"),
                AnimationEvent::new(0.5, "done"),
            ],
            ..KeyframeAnimation::default()
        },
    );
    harness
        .schedule_mut()
        .add_system("clip::animation", |world, dt| {
            keyframe_animation_system(world, dt);
        });

    let snapshots = harness.run(6);
    for (index, snapshot) in snapshots.iter().enumerate() {
        let tick = index as u64 + 1;
        let names: Vec<&str> = snapshot
            .animation_events
            .iter()
            .map(|event| event.name.as_str())
            .collect();
        match tick {
            1 | 2 => assert!(names.is_empty(), "tick {tick}: {names:?}"),
            3 => assert_eq!(names, ["step"], "tick {tick}"),
            4 => assert!(names.is_empty(), "tick {tick}: {names:?}"),
            5 => assert_eq!(names, ["done"], "tick {tick}"),
            6 => assert!(names.is_empty(), "tick {tick}: {names:?}"),
            _ => unreachable!(),
        }
    }

    let fifth = &snapshots[4];
    let actor_state = fifth.entities["Actor"]
        .animation
        .as_ref()
        .expect("clip playhead");
    assert!((actor_state.clip_time.unwrap() - 0.5).abs() <= 1e-5);
    assert_eq!(actor_state.clip_finished, Some(false));
    approx(
        fifth.entities["Actor"].position.expect("actor transform"),
        [5.0, 0.0, 0.0],
        1e-4,
    );

    let tail = harness.run(4);
    let last = tail.last().expect("ten ticks");
    let state = last.entities["Actor"]
        .animation
        .as_ref()
        .expect("clip state");
    assert_eq!(state.clip_finished, Some(true));
    approx(
        last.entities["Actor"].position.expect("actor transform"),
        [10.0, 0.0, 0.0],
        1e-4,
    );
    assert!(
        last.animation_events.is_empty(),
        "events must not repeat after the clip ends"
    );
}

#[test]
fn ui_layout_and_quads_are_captured_deterministically() {
    let mut harness = SimHarness::new(3).with_step(1.0 / 60.0);
    harness
        .world_mut()
        .insert_resource(ScreenSize::new(800, 600));
    harness
        .schedule_mut()
        .add_system("ui::layout", |world, _dt| ui_layout_system(world));
    harness
        .schedule_mut()
        .add_system("ui::render", |world, _dt| ui_render_system(world));

    let mut style = Style::fixed(200.0, 100.0);
    style.background = [0.25, 0.5, 0.75, 1.0];
    harness.spawn_ui("Panel", style);

    let snapshot = harness.tick();
    let panel = snapshot.entities["Panel"]
        .ui
        .as_ref()
        .expect("computed layout");
    assert_eq!(panel.rect, [300.0, 250.0, 200.0, 100.0]);
    assert_eq!(snapshot.ui_quads.len(), 1);
    assert_eq!(snapshot.ui_quads[0].rect, panel.rect);
    assert_eq!(snapshot.ui_quads[0].color, [0.25, 0.5, 0.75, 1.0]);
}

#[test]
fn same_seed_and_script_are_bit_identical_across_runs() {
    fn run(seed: u64) -> Vec<Snapshot> {
        let mut harness = SimHarness::new(seed).with_step(0.05);
        let walker = harness.spawn_transformed("Walker", [0.0, 0.0, 0.0]);
        harness.install_random_walker(walker, seed);
        harness.script(&[
            ScriptedInput::PressKey(KeyCode::KeyW),
            ScriptedInput::MoveMouse([64.0, 32.0]),
        ]);

        let mut snapshots = Vec::new();
        for tick in 1..=12 {
            if tick == 4 {
                harness.script(&[ScriptedInput::ReleaseKey(KeyCode::KeyW)]);
            }
            if tick == 6 {
                harness.script(&[ScriptedInput::MoveMouse([128.0, 48.0])]);
            }
            snapshots.push(harness.tick());
        }
        snapshots
    }

    let first = run(42);
    let second = run(42);
    assert_eq!(first, second, "same seed + same script must be identical");

    assert!(
        first[0].input.as_ref().is_some_and(|input| input.keys[0]
            & (1 << KeyCode::KeyW.bit_index().expect("named key"))
            != 0),
        "scripted key press must reach the input snapshot: {:?}",
        first[0].input
    );
    assert_eq!(first[0].input.as_ref().unwrap().cursor, [64.0, 32.0]);
    assert!(
        first[3].input.as_ref().is_some_and(|input| input.keys[0]
            & (1 << KeyCode::KeyW.bit_index().expect("named key"))
            == 0),
        "release must clear the held bit: {:?}",
        first[3].input
    );

    let other = run(43);
    assert_ne!(first, other, "a different seed must change the trace");
}

#[test]
fn diff_reports_entity_component_and_field_with_tolerance() {
    let mut harness = SimHarness::new(9);
    let cube = harness.spawn_transformed("Cube", [1.0, 2.0, 3.0]);
    harness.tick();

    let before = harness.checkpoint();
    harness
        .world_mut()
        .get_mut::<LocalTransform>(cube)
        .expect("cube transform")
        .position
        .x += 0.25;
    let after = harness.checkpoint();

    let strict = before.diff(&after, 1e-6);
    let position = strict
        .iter()
        .find(|diff| diff.path == "entities.Cube.position[0]")
        .expect("diff names entity/field");
    assert_eq!(position.left, "1.0");
    assert_eq!(position.right, "1.25");

    let loose = before.diff(&after, 0.5);
    assert!(
        loose
            .iter()
            .all(|diff| diff.path != "entities.Cube.position[0]"),
        "tolerance must suppress float-only changes: {loose:?}"
    );
}

#[test]
fn reparenting_is_immediately_visible_after_a_tick() {
    let mut harness = SimHarness::new(5);
    let root = harness.spawn_transformed("Root", [10.0, 0.0, 0.0]);
    let child = harness.spawn_transformed("Child", [1.0, 0.0, 0.0]);
    reparent(harness.world_mut(), child, root);

    let snapshot = harness.tick();
    approx(
        snapshot.entities["Child"]
            .world_position
            .expect("world position"),
        [11.0, 0.0, 0.0],
        1e-5,
    );
}
