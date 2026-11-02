//! Data-driven physics scenarios: CCD, triggers, parenting, stacking and
//! reproducible initial conditions.
//!
//! Gated behind `--features physics`; without the feature this file compiles to
//! an empty test crate, which keeps the default player build free of rapier.

#![cfg(feature = "physics")]

mod support;

use prism_engine::physics::PhysicsEventKind;
use support::{BodySpec, PhysicsHarness, Scenario, ShapeSpec};

fn floor(name: &str, y: f32) -> BodySpec {
    BodySpec::static_body(name, [0.0, y, 0.0]).boxed([5.0, 0.5, 5.0])
}

#[test]
fn fast_body_vs_thin_floor_uses_ccd() {
    let scenario = Scenario::new()
        .body(BodySpec::static_body("ThinFloor", [0.0, 0.0, 0.0]).boxed([5.0, 0.02, 5.0]))
        .body(
            BodySpec::dynamic("FastBall", [0.0, 20.0, 0.0])
                .sphere(0.2)
                .ccd(),
        );

    let mut harness = PhysicsHarness::new(scenario);
    harness.run(360);

    harness.assert_y_between("FastBall", 0.1, 0.5);
    harness.assert_all_finite();
    assert!(
        harness
            .pose("FastBall")
            .sleeping
            .is_some_and(|sleeping| sleeping),
        "the ball should settle and sleep on the thin floor"
    );
}

#[test]
fn trigger_reports_started_and_stopped_without_blocking() {
    let scenario = Scenario::new()
        .body(
            BodySpec::static_body("Trigger", [0.0, 3.0, 0.0])
                .boxed([5.0, 0.5, 5.0])
                .sensor(),
        )
        .body(floor("Floor", -2.0))
        .body(BodySpec::dynamic("Ball", [0.0, 5.0, 0.0]).sphere(0.5));

    let mut harness = PhysicsHarness::new(scenario);
    harness.run(300);

    let sensor_started = harness
        .events()
        .iter()
        .find(|event| {
            event.kind == PhysicsEventKind::Started
                && event.is_sensor
                && event.involves("Ball", "Trigger")
        })
        .expect("sensor Started event");
    let sensor_stopped = harness
        .events()
        .iter()
        .find(|event| {
            event.kind == PhysicsEventKind::Stopped
                && event.is_sensor
                && event.involves("Ball", "Trigger")
        })
        .expect("sensor Stopped event");
    assert!(
        sensor_started.step < sensor_stopped.step,
        "trigger enter must precede exit: {sensor_started:?} / {sensor_stopped:?}"
    );

    assert!(
        harness.events().iter().any(|event| {
            event.kind == PhysicsEventKind::Started
                && !event.is_sensor
                && event.involves("Ball", "Floor")
        }),
        "solid landing should also produce a Started event: {:?}",
        harness.events()
    );

    harness.assert_y_between("Ball", -1.05, -0.95);
}

#[test]
fn parented_dynamic_body_integrates_in_parent_space() {
    let scenario = Scenario::new()
        .body(
            BodySpec::static_body("Parent", [10.0, 0.0, 0.0])
                .shape(ShapeSpec::boxed([5.0, 0.5, 5.0]).center([0.0, -0.5, 0.0])),
        )
        .body(
            BodySpec::dynamic("Child", [0.0, 5.0, 0.0])
                .sphere(0.5)
                .parent("Parent"),
        );

    let mut harness = PhysicsHarness::new(scenario);
    harness.run(240);

    harness.assert_position_within("Child", [0.0, 0.5, 0.0], 0.05);
    harness.assert_world_position_within("Child", [10.0, 0.5, 0.0], 0.05);
}

#[test]
fn stacked_boxes_stay_stable_and_sleep() {
    let scenario = Scenario::new()
        .body(floor("Floor", -0.5))
        .body(BodySpec::dynamic("BoxA", [0.0, 0.5, 0.0]).boxed([0.5; 3]))
        .body(BodySpec::dynamic("BoxB", [0.0, 1.5, 0.0]).boxed([0.5; 3]))
        .body(BodySpec::dynamic("BoxC", [0.0, 2.5, 0.0]).boxed([0.5; 3]));

    let mut harness = PhysicsHarness::new(scenario);
    harness.run(900);

    for (name, y) in [("BoxA", 0.5), ("BoxB", 1.5), ("BoxC", 2.5)] {
        harness.assert_position_within(name, [0.0, y, 0.0], 0.1);
        harness.assert_sleeping(name);
    }
    harness.assert_all_finite();
}

#[test]
fn capsule_shape_rests_on_the_floor() {
    let scenario = Scenario::new()
        .body(floor("Floor", -0.5))
        .body(BodySpec::dynamic("Capsule", [0.0, 3.0, 0.0]).capsule(0.5, 0.5));

    let mut harness = PhysicsHarness::new(scenario);
    harness.run(300);

    // Capsule total half-height = half_height + radius = 1.0.
    harness.assert_y_between("Capsule", 0.9, 1.1);
}

#[test]
fn seeded_initial_conditions_reproduce_the_same_trajectory() {
    fn scenario() -> Scenario {
        Scenario::new()
            .body(floor("Floor", -0.5))
            .body(BodySpec::dynamic("Ball", [0.0, 4.0, 0.0]).sphere(0.5))
            .seeded(0x5eed)
    }

    let mut first = PhysicsHarness::new(scenario());
    let mut second = PhysicsHarness::new(scenario());
    first.run(180);
    second.run(180);

    let first_pose = first.pose("Ball");
    let second_pose = second.pose("Ball");
    for index in 0..3 {
        assert!(
            (first_pose.position[index] - second_pose.position[index]).abs() <= 1e-5,
            "position[{index}] diverged: {} vs {}",
            first_pose.position[index],
            second_pose.position[index]
        );
    }
    assert_eq!(first_pose.sleeping, second_pose.sleeping);
    assert_eq!(canonical(first.events()), canonical(second.events()));
}

/// Rapier does not guarantee the order of the two entities in a collision pair,
/// so compare events with the pair sorted and the list itself sorted.
fn canonical(events: &[support::EventRecord]) -> Vec<(u32, u8, String, String, bool)> {
    let mut rows: Vec<(u32, u8, String, String, bool)> = events
        .iter()
        .map(|event| {
            let (body_a, body_b) = if event.body_a <= event.body_b {
                (event.body_a.clone(), event.body_b.clone())
            } else {
                (event.body_b.clone(), event.body_a.clone())
            };
            (
                event.step,
                matches!(event.kind, PhysicsEventKind::Started) as u8,
                body_a,
                body_b,
                event.is_sensor,
            )
        })
        .collect();
    rows.sort();
    rows
}
