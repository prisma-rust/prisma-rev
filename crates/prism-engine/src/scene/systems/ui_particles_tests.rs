use super::*;
use crate::scene::components::UiParticleKind;
use crate::scene::runtime_components::Hidden;
use crate::ui::{ExternalUiQuads, UiQuad};

fn screen(world: &mut World, width: u32, height: u32) {
    world.insert_resource(ScreenSize::new(width, height));
}

fn spark_emitter(layer: i32) -> UiParticleEmitter {
    UiParticleEmitter {
        kind: UiParticleKind::Spark,
        emission_rate: 100.0,
        gravity: 0.0,
        wind: 0.0,
        speed: [10.0, 20.0],
        length: [4.0, 4.0],
        width: [2.0, 2.0],
        alpha: [0.5, 0.5],
        layer,
        ..UiParticleEmitter::default()
    }
}

#[test]
fn spawns_quads_on_the_component_layer() {
    let mut world = World::new();
    screen(&mut world, 800, 600);
    let entity = world.spawn();
    // Rain keeps its configured trail length; the spark preset ignores it.
    world.insert(
        entity,
        UiParticleEmitter {
            kind: UiParticleKind::Rain,
            speed: [1.0, 1.0],
            ..spark_emitter(7)
        },
    );

    let alive = ui_particle_emitter_system(&mut world, 0.1);
    assert!(alive > 0, "100 particles/s over 0.1 s must spawn something");

    let queued = world
        .get_resource::<ExternalUiQuads>()
        .expect("quads must be queued for the UI pass");
    assert_eq!(queued.0.len(), alive);
    for quad in &queued.0 {
        assert_eq!(quad.layer, 7, "layer comes from the component");
        assert!((quad.rect[2] - 2.0).abs() < 1e-3, "width in pixels");
        assert!((quad.rect[3] - 4.0).abs() < 1e-3, "trail length in pixels");
        assert!(quad.color[3] > 0.0);
    }
}

#[test]
fn step_is_capped_by_the_screen_bounds() {
    let mut world = World::new();
    screen(&mut world, 320, 240);
    let entity = world.spawn();
    world.insert(entity, spark_emitter(1));

    // Spawn positions stay above the top edge so particles fade in on screen.
    for _ in 0..3 {
        ui_particle_emitter_system(&mut world, 0.1);
        let queued = world.get_resource::<ExternalUiQuads>().expect("quads");
        for quad in &queued.0 {
            assert!(quad.rect[0] + quad.rect[2] > 0.0);
            assert!(quad.rect[1] < 240.0 + 60.0);
        }
        world.remove_resource::<ExternalUiQuads>();
    }
}

#[test]
fn a_paused_emitter_stops_spawning() {
    let mut world = World::new();
    screen(&mut world, 800, 600);
    let entity = world.spawn();
    world.insert(entity, spark_emitter(1));

    let first = ui_particle_emitter_system(&mut world, 0.1);
    assert!(first > 0);

    let mut paused = spark_emitter(1);
    paused.playing = false;
    world.insert(entity, paused);
    let second = ui_particle_emitter_system(&mut world, 0.1);
    assert_eq!(
        second, first,
        "no new particles may spawn while paused; live ones keep their count"
    );
}

#[test]
fn hidden_emitters_do_not_run() {
    let mut world = World::new();
    screen(&mut world, 800, 600);
    let entity = world.spawn();
    world.insert(entity, spark_emitter(1));
    world.insert(entity, Hidden(true));

    assert_eq!(ui_particle_emitter_system(&mut world, 0.1), 0);
    assert!(world.get_resource::<ExternalUiQuads>().is_none());
}

#[test]
fn quads_are_appended_to_quads_queued_by_the_project() {
    let mut world = World::new();
    screen(&mut world, 800, 600);
    let entity = world.spawn();
    world.insert(entity, spark_emitter(1));
    world.insert_resource(ExternalUiQuads(vec![UiQuad {
        rect: [0.0, 0.0, 10.0, 10.0],
        layer: 900,
        ..UiQuad::default()
    }]));

    let alive = ui_particle_emitter_system(&mut world, 0.1);
    let queued = world.get_resource::<ExternalUiQuads>().expect("quads");
    assert_eq!(queued.0.len(), alive + 1);
    assert_eq!(queued.0[0].layer, 900, "project quads keep their slot");
}

#[test]
fn a_non_positive_step_does_nothing() {
    let mut world = World::new();
    screen(&mut world, 800, 600);
    let entity = world.spawn();
    world.insert(entity, spark_emitter(1));

    assert_eq!(ui_particle_emitter_system(&mut world, 0.0), 0);
    assert!(world.get_resource::<ExternalUiQuads>().is_none());
    assert!(world.get::<UiParticleEmitterState>(entity).is_none());
}
