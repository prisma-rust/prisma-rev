use super::{update_trigger_events2d, TriggerEvents2D};
use prism_ecs::World;
use prism_project_api::PhysicsEventPhase2D;
use prism_scene::components::{Active, Collider2D, LocalTransform};

fn collider(world: &mut World, position: [f32; 2], is_trigger: bool) -> prism_ecs::Entity {
    let entity = world.spawn();
    world.insert(
        entity,
        LocalTransform {
            position: glam::Vec3::new(position[0], position[1], 0.0),
            ..LocalTransform::default()
        },
    );
    world.insert(
        entity,
        Collider2D {
            half_extents: [2.0, 2.0],
            is_trigger,
            layer: 1,
            ..Collider2D::default()
        },
    );
    world.insert(entity, Active(true));
    entity
}

#[test]
fn trigger_events_track_enter_stay_and_exit() {
    let mut world = World::new();
    let solid = collider(&mut world, [0.0, 0.0], false);
    let sensor = collider(&mut world, [1.0, 0.0], true);

    update_trigger_events2d(&mut world);
    let events = world
        .get_resource::<TriggerEvents2D>()
        .expect("trigger event resource")
        .events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].phase(), Some(PhysicsEventPhase2D::Enter));
    assert_eq!(events[0].entity_a.id, solid.id());
    assert_eq!(events[0].entity_b.id, sensor.id());

    update_trigger_events2d(&mut world);
    assert_eq!(
        world.get_resource::<TriggerEvents2D>().unwrap().events()[0].phase(),
        Some(PhysicsEventPhase2D::Stay)
    );

    world
        .get_mut::<LocalTransform>(sensor)
        .expect("sensor transform")
        .position
        .x = 20.0;
    update_trigger_events2d(&mut world);
    assert_eq!(
        world.get_resource::<TriggerEvents2D>().unwrap().events()[0].phase(),
        Some(PhysicsEventPhase2D::Exit)
    );
}
