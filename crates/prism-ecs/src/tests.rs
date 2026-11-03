use super::*;

#[derive(Debug, PartialEq)]
struct Position(f32, f32);

#[derive(Debug, PartialEq)]
struct Name(&'static str);

#[test]
fn spawn_and_is_alive() {
    let mut world = World::new();
    let e = world.spawn();
    assert!(world.is_alive(e));
    world.despawn(e);
    assert!(!world.is_alive(e));
}

#[test]
fn insert_get_remove() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(e, Position(1.0, 2.0));
    assert_eq!(world.get::<Position>(e), Some(&Position(1.0, 2.0)));

    world.get_mut::<Position>(e).unwrap().0 = 9.0;
    assert_eq!(world.get::<Position>(e), Some(&Position(9.0, 2.0)));

    assert_eq!(world.remove::<Position>(e), Some(Position(9.0, 2.0)));
    assert_eq!(world.get::<Position>(e), None);
}

#[test]
fn despawn_drops_components() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(e, Position(0.0, 0.0));
    world.insert(e, Name("hero"));
    world.despawn(e);
    assert_eq!(world.get::<Position>(e), None);
    assert_eq!(world.get::<Name>(e), None);
}

#[test]
fn generation_bumps_on_recycle() {
    let mut world = World::new();
    let e0 = world.spawn();
    world.despawn(e0);
    let e1 = world.spawn(); // reuses e0's slot
    assert_eq!(e0.id, e1.id);
    assert_ne!(e0.generation, e1.generation);
    assert!(!world.is_alive(e0)); // stale handle invalidated
    assert!(world.is_alive(e1));
}

#[test]
fn ghost_next_generation_handle_is_not_alive() {
    let mut world = World::new();
    let e = world.spawn();
    world.despawn(e);

    // A freed slot stores `generation + 1`, but that handle was never spawned.
    let ghost = Entity::from_raw(e.id(), e.generation() + 1);
    assert!(!world.is_alive(ghost));
    assert!(!world.is_active(ghost));

    world.insert(ghost, Position(7.0, 7.0));
    let recycled = world.spawn();
    assert_eq!(recycled.id(), e.id());
    assert_ne!(recycled.generation(), e.generation());
    // The rejected write must not leak into the recycled entity.
    assert_eq!(world.get::<Position>(recycled), None);
}

#[test]
fn despawn_ghost_does_not_double_free() {
    let mut world = World::new();
    let e = world.spawn();
    world.despawn(e);

    let ghost = Entity::from_raw(e.id(), e.generation() + 1);
    world.despawn(ghost); // must be a no-op

    let a = world.spawn();
    let b = world.spawn();
    assert_ne!(a, b, "a slot must not be handed out twice");
    assert!(world.is_alive(a));
    assert!(world.is_alive(b));
}

#[test]
fn query_visits_all_with_component() {
    let mut world = World::new();
    let a = world.spawn();
    let b = world.spawn();
    let _c = world.spawn();
    world.insert(a, Position(1.0, 0.0));
    world.insert(b, Position(2.0, 0.0));
    // c has no Position
    let count = world.query::<Position>().count();
    assert_eq!(count, 2);

    // query_mut can mutate
    for (_e, pos) in world.query_mut::<Position>() {
        pos.1 = 5.0;
    }
    assert_eq!(world.get::<Position>(a), Some(&Position(1.0, 5.0)));
    assert_eq!(world.get::<Position>(b), Some(&Position(2.0, 5.0)));
}

#[test]
fn insert_on_dead_entity_is_noop() {
    let mut world = World::new();
    let e = world.spawn();
    world.despawn(e);
    world.insert(e, Position(0.0, 0.0));
    // A recycled slot should not receive the stale insert.
    let e2 = world.spawn();
    assert_eq!(world.get::<Position>(e2), None);
}

#[derive(Debug, PartialEq)]
struct Velocity(f32, f32);

#[derive(Debug, PartialEq)]
struct Health(i32);

#[test]
fn query2_joins_two_components() {
    let mut world = World::new();
    let a = world.spawn();
    let b = world.spawn();
    let _c = world.spawn();

    world.insert(a, Position(1.0, 0.0));
    world.insert(a, Velocity(0.5, 0.0));
    world.insert(b, Position(2.0, 0.0));
    // `b` has no `Velocity`; `_c` has neither.
    world.insert(_c, Position(3.0, 0.0));

    let results: Vec<_> = world.query2::<Position, Velocity>().collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, a);
    assert_eq!(results[0].1, &Position(1.0, 0.0));
    assert_eq!(results[0].2, &Velocity(0.5, 0.0));
}

#[test]
fn query3_joins_three_components() {
    let mut world = World::new();
    let a = world.spawn();
    let b = world.spawn();

    world.insert(a, Position(1.0, 0.0));
    world.insert(a, Velocity(0.5, 0.0));
    world.insert(a, Health(100));
    // `b` is missing `Health`.
    world.insert(b, Position(2.0, 0.0));
    world.insert(b, Velocity(1.0, 0.0));

    let results: Vec<_> = world.query3::<Position, Velocity, Health>().collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, a);
    assert_eq!(results[0].3, &Health(100));
}

#[test]
fn query2_mut_writes_a_reads_b() {
    let mut world = World::new();
    let a = world.spawn();
    world.insert(a, Position(1.0, 2.0));
    world.insert(a, Velocity(0.5, -0.5));

    for (_e, pos, vel) in world.query2_mut::<Position, Velocity>() {
        pos.0 += vel.0;
        pos.1 += vel.1;
    }
    assert_eq!(world.get::<Position>(a), Some(&Position(1.5, 1.5)));
}

#[test]
fn query2_mut_yields_disjoint_mutable_borrows() {
    let mut world = World::new();
    let a = world.spawn();
    let b = world.spawn();
    world.insert(a, Position(1.0, 1.0));
    world.insert(a, Velocity(0.5, 0.25));
    world.insert(b, Position(2.0, 2.0));
    world.insert(b, Velocity(3.0, 4.0));

    // Hold every `&mut Position` at the same time: the two component pools are
    // borrowed mutably side by side, so an overlapping (aliasing) split would
    // be rejected here. `Velocity` stays readable throughout.
    let mut rows: Vec<(Entity, &mut Position, (f32, f32))> = Vec::new();
    for (entity, position, velocity) in world.query2_mut::<Position, Velocity>() {
        rows.push((entity, position, (velocity.0, velocity.1)));
    }
    assert_eq!(rows.len(), 2);
    for (_, position, (vx, vy)) in &mut rows {
        position.0 += *vx;
        position.1 += *vy;
    }
    drop(rows);

    assert_eq!(world.get::<Position>(a), Some(&Position(1.5, 1.25)));
    assert_eq!(world.get::<Position>(b), Some(&Position(5.0, 6.0)));
    // The read-only pool was not modified.
    assert_eq!(world.get::<Velocity>(a), Some(&Velocity(0.5, 0.25)));
}

#[test]
fn query2_mut_rejects_the_same_type_in_both_slots() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(e, Position(1.0, 2.0));

    // `get_disjoint_mut` panics on duplicate keys, so the same-type query must
    // short-circuit to an empty iterator instead of reaching the pool map.
    assert!(world.query2_mut::<Position, Position>().next().is_none());
    assert_eq!(world.get::<Position>(e), Some(&Position(1.0, 2.0)));
}

#[test]
fn query2_mut_is_empty_when_the_read_pool_is_missing() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(e, Position(1.0, 2.0));

    assert_eq!(world.query2_mut::<Position, Velocity>().count(), 0);
    assert_eq!(world.get::<Position>(e), Some(&Position(1.0, 2.0)));
}

#[test]
fn query2_mut_skips_inactive_entities() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(e, Position(1.0, 2.0));
    world.insert(e, Velocity(1.0, 1.0));
    world.set_active(e, false);

    assert_eq!(world.query2_mut::<Position, Velocity>().count(), 0);
    assert_eq!(world.get::<Position>(e), Some(&Position(1.0, 2.0)));
}

#[test]
fn query2_empty_when_one_pool_missing() {
    let mut world = World::new();
    let a = world.spawn();
    world.insert(a, Position(0.0, 0.0));
    // No entity has `Velocity` at all.
    assert!(world.query2::<Position, Velocity>().next().is_none());
}

#[test]
fn resources_insert_get_mut_remove() {
    let mut world = World::new();
    assert!(world.get_resource::<Health>().is_none());

    world.insert_resource(Health(42));
    assert_eq!(world.get_resource::<Health>(), Some(&Health(42)));

    world.get_resource_mut::<Health>().unwrap().0 -= 10;
    assert_eq!(world.get_resource::<Health>(), Some(&Health(32)));

    let removed = world.remove_resource::<Health>();
    assert_eq!(removed, Some(Health(32)));
    assert!(world.get_resource::<Health>().is_none());
}

#[test]
fn resources_are_type_keyed_singletons() {
    let mut world = World::new();
    world.insert_resource(Health(100));
    world.insert_resource(Health(200)); // replaces
    assert_eq!(world.get_resource::<Health>(), Some(&Health(200)));
}
