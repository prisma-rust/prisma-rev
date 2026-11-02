use prism_ecs::{Entity, World};
use prism_scene::components::{Camera, Child, LocalToWorld, LocalTransform, LodGroup, Parent};

use super::lod_system;
use crate::scene::components::LodHidden;
use crate::scene::systems::hierarchy::is_active_in_hierarchy;

fn spawn_camera(world: &mut World, position: glam::Vec3) -> Entity {
    let entity = world.spawn();
    world.insert(
        entity,
        LocalTransform {
            position,
            ..Default::default()
        },
    );
    world.insert(entity, LocalToWorld(glam::Mat4::from_translation(position)));
    world.insert(entity, Camera::default());
    entity
}

fn spawn_group(
    world: &mut World,
    distances: Vec<f32>,
    child_count: usize,
) -> (Entity, Vec<Entity>) {
    let parent = world.spawn();
    world.insert(parent, LocalTransform::default());
    world.insert(parent, LocalToWorld(glam::Mat4::IDENTITY));
    world.insert(parent, LodGroup { distances });

    let children: Vec<Entity> = (0..child_count)
        .map(|index| {
            let child = world.spawn();
            world.insert(child, Parent(parent));
            // 子实体的位置不影响选级：距离从父实体算。
            world.insert(
                child,
                LocalTransform {
                    position: glam::Vec3::new(index as f32, 0.0, 0.0),
                    ..Default::default()
                },
            );
            child
        })
        .collect();
    world.insert(parent, Child(children.clone()));
    (parent, children)
}

fn hidden(world: &World, entity: Entity) -> bool {
    world
        .get::<LodHidden>(entity)
        .map(|hidden| hidden.0)
        .unwrap_or(false)
}

#[test]
fn the_nearest_camera_distance_selects_the_level() {
    let mut world = World::new();
    let camera = spawn_camera(&mut world, glam::Vec3::ZERO);
    let (_parent, children) = spawn_group(&mut world, vec![10.0, 50.0], 3);

    // 距离 0 -> LOD0
    lod_system(&mut world);
    assert!(!hidden(&world, children[0]));
    assert!(hidden(&world, children[1]));
    assert!(hidden(&world, children[2]));
    assert!(is_active_in_hierarchy(&world, children[0]));
    assert!(!is_active_in_hierarchy(&world, children[1]));

    // 距离 20 -> LOD1
    world.insert(
        camera,
        LocalToWorld(glam::Mat4::from_translation(glam::Vec3::new(
            20.0, 0.0, 0.0,
        ))),
    );
    lod_system(&mut world);
    assert!(hidden(&world, children[0]));
    assert!(!hidden(&world, children[1]));
    assert!(hidden(&world, children[2]));

    // 距离 100 -> 超过所有阈值 -> 最后一级
    world.insert(
        camera,
        LocalToWorld(glam::Mat4::from_translation(glam::Vec3::new(
            100.0, 0.0, 0.0,
        ))),
    );
    lod_system(&mut world);
    assert!(hidden(&world, children[0]));
    assert!(hidden(&world, children[1]));
    assert!(!hidden(&world, children[2]));
}

#[test]
fn multiple_cameras_use_the_closest_one() {
    let mut world = World::new();
    spawn_camera(&mut world, glam::Vec3::new(100.0, 0.0, 0.0));
    spawn_camera(&mut world, glam::Vec3::new(5.0, 0.0, 0.0));
    let (_, children) = spawn_group(&mut world, vec![10.0, 50.0], 2);

    lod_system(&mut world);
    assert!(!hidden(&world, children[0]), "the near camera should win");
    assert!(hidden(&world, children[1]));
}

#[test]
fn a_disabled_camera_is_ignored() {
    let mut world = World::new();
    let camera = spawn_camera(&mut world, glam::Vec3::new(100.0, 0.0, 0.0));
    world.get_mut::<Camera>(camera).unwrap().enabled = false;
    let (_, children) = spawn_group(&mut world, vec![10.0, 50.0], 2);

    lod_system(&mut world);
    // 没有启用相机 -> 保守显示 LOD0。
    assert!(!hidden(&world, children[0]));
    assert!(hidden(&world, children[1]));
}

#[test]
fn removing_the_group_clears_the_runtime_hidden_flags() {
    let mut world = World::new();
    spawn_camera(&mut world, glam::Vec3::new(100.0, 0.0, 0.0));
    let (parent, children) = spawn_group(&mut world, vec![10.0, 50.0], 2);

    lod_system(&mut world);
    assert!(hidden(&world, children[0]));
    assert!(!hidden(&world, children[1]));

    world.remove::<LodGroup>(parent);
    lod_system(&mut world);
    assert!(!hidden(&world, children[0]));
    assert!(!hidden(&world, children[1]));
}

#[test]
fn level_for_distance_handles_degenerate_input() {
    let group = LodGroup {
        distances: vec![10.0, 50.0],
    };
    assert_eq!(group.level_for_distance(0.0, 0), None);
    assert_eq!(group.level_for_distance(0.0, 3), Some(0));
    assert_eq!(group.level_for_distance(10.0, 3), Some(0));
    assert_eq!(group.level_for_distance(10.1, 3), Some(1));
    assert_eq!(group.level_for_distance(50.0, 3), Some(1));
    assert_eq!(group.level_for_distance(50.1, 3), Some(2));
    // 非有限距离 / 负距离都按 0 处理，不会跳到最低级。
    assert_eq!(group.level_for_distance(f32::NAN, 3), Some(0));
    assert_eq!(group.level_for_distance(f32::INFINITY, 3), Some(0));
    assert_eq!(group.level_for_distance(-5.0, 3), Some(0));
    // 没有阈值时始终 LOD0。
    assert_eq!(
        LodGroup {
            distances: Vec::new()
        }
        .level_for_distance(999.0, 3),
        Some(0)
    );
}
