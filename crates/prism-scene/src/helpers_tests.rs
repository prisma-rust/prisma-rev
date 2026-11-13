use super::*;
use prism_ecs::World;

#[test]
fn reparent_creates_children() {
    let mut world = World::new();
    let parent = world.spawn();
    let child = world.spawn();

    HierarchyHelper::reparent(&mut world, child, Some(parent));

    assert_eq!(world.get::<Parent>(child), Some(&Parent(parent)));
    let children = world
        .get::<Child>(parent)
        .expect("parent should have Child");
    assert!(children.0.contains(&child));
}

#[test]
fn reparent_to_none_removes_parent() {
    let mut world = World::new();
    let parent = world.spawn();
    let child = world.spawn();

    HierarchyHelper::reparent(&mut world, child, Some(parent));
    HierarchyHelper::reparent(&mut world, child, None);

    assert!(world.get::<Parent>(child).is_none());
    let children = world.get::<Child>(parent).unwrap();
    assert!(!children.0.contains(&child));
}

#[test]
fn reparent_updates_old_and_new_parent() {
    let mut world = World::new();
    let p1 = world.spawn();
    let p2 = world.spawn();
    let child = world.spawn();

    HierarchyHelper::reparent(&mut world, child, Some(p1));
    HierarchyHelper::reparent(&mut world, child, Some(p2));

    assert_eq!(world.get::<Parent>(child), Some(&Parent(p2)));
    let c1 = world.get::<Child>(p1).unwrap();
    assert!(!c1.0.contains(&child));
    let c2 = world.get::<Child>(p2).unwrap();
    assert!(c2.0.contains(&child));
}

#[test]
fn reparent_to_same_is_noop() {
    let mut world = World::new();
    let parent = world.spawn();
    let child = world.spawn();

    HierarchyHelper::reparent(&mut world, child, Some(parent));
    let before = world.get::<Child>(parent).unwrap().0.clone();
    HierarchyHelper::reparent(&mut world, child, Some(parent));
    let after = world.get::<Child>(parent).unwrap().0.clone();
    assert_eq!(before, after);
}

#[test]
fn self_parent_rejected() {
    let mut world = World::new();
    let e = world.spawn();
    HierarchyHelper::reparent(&mut world, e, Some(e));
    assert!(world.get::<Parent>(e).is_none());
}

#[test]
fn dead_entity_rejected() {
    let mut world = World::new();
    let parent = world.spawn();
    let child = world.spawn();
    world.despawn(child);
    HierarchyHelper::reparent(&mut world, child, Some(parent));
    assert!(world.get::<Child>(parent).is_none());
}

#[test]
fn dead_new_parent_leaves_hierarchy_untouched() {
    let mut world = World::new();
    let old_parent = world.spawn();
    let dead_parent = world.spawn();
    let child = world.spawn();

    HierarchyHelper::reparent(&mut world, child, Some(old_parent));
    world.despawn(dead_parent);
    HierarchyHelper::reparent(&mut world, child, Some(dead_parent));

    assert_eq!(world.get::<Parent>(child), Some(&Parent(old_parent)));
    let children = world.get::<Child>(old_parent).unwrap();
    assert!(children.0.contains(&child));
}

#[test]
fn cycle_is_rejected() {
    let mut world = World::new();
    let a = world.spawn();
    let b = world.spawn();
    let c = world.spawn();

    HierarchyHelper::reparent(&mut world, b, Some(a));
    HierarchyHelper::reparent(&mut world, c, Some(b));

    // a -> b -> c, so making a a child of c would close a cycle.
    HierarchyHelper::reparent(&mut world, a, Some(c));

    assert!(world.get::<Parent>(a).is_none());
    let a_children = world.get::<Child>(a).unwrap();
    assert!(a_children.0.contains(&b));
    let b_children = world.get::<Child>(b).unwrap();
    assert!(b_children.0.contains(&c));
    assert!(world.get::<Child>(c).is_none());
}

#[test]
fn has_children_works() {
    let mut world = World::new();
    let parent = world.spawn();
    let child = world.spawn();
    assert!(!HierarchyHelper::has_children(&world, parent));
    HierarchyHelper::reparent(&mut world, child, Some(parent));
    assert!(HierarchyHelper::has_children(&world, parent));
}

#[test]
fn active_hierarchy_queries_read_current_parent_state_without_waiting_for_propagation() {
    use crate::components::Active;

    let mut world = World::new();
    let parent = world.spawn();
    let child = world.spawn();
    HierarchyHelper::reparent(&mut world, child, Some(parent));

    world.insert(parent, Active(true));
    assert!(is_active_in_hierarchy(&world, child));

    world.insert(parent, Active(false));
    assert!(!is_active_in_hierarchy(&world, child));

    world.insert(parent, Active(true));
    assert!(is_active_in_hierarchy(&world, child));
}

#[test]
fn world_matrix_reads_current_parent_and_child_locals_before_propagation() {
    use crate::components::{LocalToWorld, LocalTransform};

    let mut world = World::new();
    let parent = world.spawn();
    let child = world.spawn();
    world.insert(
        parent,
        LocalTransform {
            position: glam::Vec3::new(10.0, 20.0, 0.0),
            ..LocalTransform::default()
        },
    );
    world.insert(
        child,
        LocalTransform {
            position: glam::Vec3::new(3.0, 4.0, 0.0),
            ..LocalTransform::default()
        },
    );
    HierarchyHelper::reparent(&mut world, child, Some(parent));
    world.insert(child, LocalToWorld(glam::Mat4::IDENTITY));
    world.get_mut::<LocalTransform>(parent).unwrap().position.x = 30.0;

    let matrix = world_matrix(&world, child).unwrap();
    assert_eq!(matrix.w_axis.truncate(), glam::Vec3::new(33.0, 24.0, 0.0));
}

#[test]
fn world_matrix_propagates_through_transformless_nodes() {
    use crate::components::{LocalToWorld, LocalTransform};

    let mut world = World::new();
    let root = world.spawn();
    let middle = world.spawn();
    let leaf = world.spawn();
    world.insert(
        root,
        LocalTransform {
            position: glam::Vec3::new(8.0, 12.0, 0.0),
            ..LocalTransform::default()
        },
    );
    world.insert(
        middle,
        LocalToWorld(glam::Mat4::from_translation(glam::Vec3::splat(500.0))),
    );
    world.insert(
        leaf,
        LocalTransform {
            position: glam::Vec3::new(2.0, 3.0, 0.0),
            ..LocalTransform::default()
        },
    );
    HierarchyHelper::reparent(&mut world, middle, Some(root));
    HierarchyHelper::reparent(&mut world, leaf, Some(middle));

    let matrix = world_matrix(&world, leaf).unwrap();
    assert_eq!(matrix.w_axis.truncate(), glam::Vec3::new(10.0, 15.0, 0.0));
    assert_eq!(
        world_matrix(&world, middle).unwrap().w_axis.truncate(),
        glam::Vec3::new(8.0, 12.0, 0.0)
    );
}

#[test]
fn world_matrix_does_not_use_cached_transform_from_a_cycle() {
    use crate::components::{LocalToWorld, LocalTransform};

    let mut world = World::new();
    let a = world.spawn();
    let b = world.spawn();
    world.insert(a, LocalTransform::default());
    world.insert(b, LocalTransform::default());
    world.insert(a, Parent(b));
    world.insert(b, Parent(a));
    world.insert(a, Child(vec![b]));
    world.insert(b, Child(vec![a]));
    world.insert(
        a,
        LocalToWorld(glam::Mat4::from_translation(glam::Vec3::splat(900.0))),
    );

    assert!(world_matrix(&world, a).is_none());
}
