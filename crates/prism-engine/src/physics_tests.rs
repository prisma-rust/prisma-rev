use super::*;
use prism_scene::components::{Collider, ColliderShape, LocalToWorld};

fn entity_with(
    world: &mut World,
    collider: Collider,
    translation: glam::Vec3,
    scale: f32,
) -> Entity {
    let entity = world.spawn();
    world.insert(entity, collider);
    world.insert(
        entity,
        LocalToWorld(glam::Mat4::from_scale_rotation_translation(
            glam::Vec3::splat(scale),
            glam::Quat::IDENTITY,
            translation,
        )),
    );
    entity
}

fn boxed(half: f32) -> Collider {
    Collider {
        shape: ColliderShape::Box,
        half_extents: [half; 3],
        ..Default::default()
    }
}

#[test]
fn a_ray_hits_the_near_face_of_a_box() {
    let mut world = World::new();
    let entity = entity_with(&mut world, boxed(1.0), glam::Vec3::ZERO, 1.0);

    let hit = raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 100.0, u32::MAX)
        .expect("ray hits the box");
    assert_eq!(hit.entity, entity);
    assert!((hit.distance - 4.0).abs() < 1e-4, "got {}", hit.distance);
    assert!((hit.point[2] + 1.0).abs() < 1e-4);
    assert!(
        (hit.normal[2] + 1.0).abs() < 1e-4,
        "normal faces the ray: {:?}",
        hit.normal
    );
}

#[test]
fn a_ray_that_misses_returns_nothing() {
    let mut world = World::new();
    entity_with(&mut world, boxed(1.0), glam::Vec3::ZERO, 1.0);
    assert!(raycast(&world, [0.0, 5.0, -5.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).is_none());
    // 方向太短 / 距离为 0 也不该命中。
    assert!(raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 0.0], 100.0, u32::MAX).is_none());
    assert!(raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 0.0, u32::MAX).is_none());
}

#[test]
fn the_closest_hit_wins() {
    let mut world = World::new();
    let far = entity_with(&mut world, boxed(1.0), glam::Vec3::new(0.0, 0.0, 5.0), 1.0);
    let near = entity_with(&mut world, boxed(1.0), glam::Vec3::ZERO, 1.0);
    let hit = raycast(&world, [0.0, 0.0, -10.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).unwrap();
    assert_eq!(hit.entity, near);
    assert_ne!(hit.entity, far);
}

#[test]
fn max_distance_cuts_the_hit_off() {
    let mut world = World::new();
    entity_with(&mut world, boxed(1.0), glam::Vec3::ZERO, 1.0);
    assert!(raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 3.9, u32::MAX).is_none());
    assert!(raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 4.1, u32::MAX).is_some());
}

#[test]
fn the_layer_mask_filters_colliders() {
    let mut world = World::new();
    let mut collider = boxed(1.0);
    collider.layer = 0b0010;
    let entity = entity_with(&mut world, collider, glam::Vec3::ZERO, 1.0);
    assert!(raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 100.0, 0b0001).is_none());
    let hit = raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 100.0, 0b0010).unwrap();
    assert_eq!(hit.entity, entity);
}

#[test]
fn entity_scale_grows_the_collider() {
    let mut world = World::new();
    entity_with(&mut world, boxed(1.0), glam::Vec3::ZERO, 2.0);
    let hit = raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).unwrap();
    assert!(
        (hit.distance - 3.0).abs() < 1e-4,
        "看起来是 2 倍大的盒子：{}",
        hit.distance
    );
}

#[test]
fn a_rotated_box_is_hit_on_its_face() {
    let mut world = World::new();
    let entity = world.spawn();
    world.insert(entity, boxed(1.0));
    world.insert(
        entity,
        LocalToWorld(glam::Mat4::from_rotation_translation(
            glam::Quat::from_rotation_x(std::f32::consts::FRAC_PI_4),
            glam::Vec3::ZERO,
        )),
    );
    // 绕 X 轴转 45°：盒子沿 z 方向的半宽变成 1·cos45 + 1·sin45 = √2。
    let hit = raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).unwrap();
    assert!(
        (hit.distance - (5.0 - 2f32.sqrt())).abs() < 1e-3,
        "got {}",
        hit.distance
    );
    // 命中面的法线是倾斜的：长度 1，且朝向射线来的方向。
    let normal = glam::Vec3::from_array(hit.normal);
    assert!((normal.length() - 1.0).abs() < 1e-3, "got {normal:?}");
    assert!(normal.z < 0.0, "normal faces the ray: {normal:?}");
}

#[test]
fn a_sphere_uses_its_radius_and_reports_a_radial_normal() {
    let mut world = World::new();
    let collider = Collider {
        shape: ColliderShape::Sphere,
        radius: 2.0,
        ..Default::default()
    };
    entity_with(&mut world, collider, glam::Vec3::new(0.0, 0.0, 1.0), 1.0);

    let hit = raycast(&world, [0.0, 0.0, -10.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).unwrap();
    assert!((hit.distance - 9.0).abs() < 1e-4, "got {}", hit.distance);
    assert!((hit.normal[2] + 1.0).abs() < 1e-4, "got {:?}", hit.normal);
    // 擦边的射线打不中。
    assert!(raycast(&world, [2.5, 0.0, -10.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).is_none());
}

#[test]
fn a_ray_starting_inside_a_sphere_still_hits_its_far_side() {
    let mut world = World::new();
    let collider = Collider {
        shape: ColliderShape::Sphere,
        radius: 2.0,
        ..Default::default()
    };
    entity_with(&mut world, collider, glam::Vec3::ZERO, 1.0);
    let hit = raycast(&world, [0.0, 0.0, 0.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).unwrap();
    assert!((hit.distance - 2.0).abs() < 1e-4, "got {}", hit.distance);
}

#[test]
fn a_capsule_covers_its_radius_and_half_height() {
    let mut world = World::new();
    let collider = Collider {
        shape: ColliderShape::Capsule,
        radius: 0.5,
        half_height: 1.0,
        ..Default::default()
    };
    entity_with(&mut world, collider, glam::Vec3::ZERO, 1.0);
    // 从上方垂直打下来：命中高度 = half_height + radius。
    let hit = raycast(&world, [0.0, 10.0, 0.0], [0.0, -1.0, 0.0], 100.0, u32::MAX).unwrap();
    assert!((hit.distance - 8.5).abs() < 1e-3, "got {}", hit.distance);
}

#[test]
fn triggers_are_reported_but_flagged() {
    let mut world = World::new();
    let collider = Collider {
        is_trigger: true,
        ..boxed(1.0)
    };
    entity_with(&mut world, collider, glam::Vec3::ZERO, 1.0);
    let hit = raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).unwrap();
    assert!(hit.is_trigger);
}

#[test]
fn colliders_without_a_world_transform_are_skipped() {
    let mut world = World::new();
    let entity = world.spawn();
    world.insert(entity, boxed(1.0));
    assert!(raycast(&world, [0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 100.0, u32::MAX).is_none());
}

#[test]
fn push_out_keeps_a_particle_outside_a_box_surface() {
    let shape = WorldShape::new(&boxed(1.0), &glam::Mat4::IDENTITY);
    let (position, normal) = shape
        .push_out(glam::vec3(1.1, 0.0, 0.0), 0.2)
        .expect("point is within the particle radius of the +X face");
    assert!((position.x - 1.2).abs() < 1e-5, "position {position:?}");
    assert!((normal.x - 1.0).abs() < 1e-5, "normal {normal:?}");
    assert!(shape.push_out(glam::vec3(1.5, 0.0, 0.0), 0.2).is_none());
}

#[test]
fn push_out_uses_the_nearest_face_when_inside_a_box() {
    let shape = WorldShape::new(&boxed(1.0), &glam::Mat4::IDENTITY);
    let (position, normal) = shape
        .push_out(glam::vec3(0.9, 0.0, 0.0), 0.25)
        .expect("inside point is pushed out");
    assert!((position.x - 1.25).abs() < 1e-5, "position {position:?}");
    assert!((normal.x - 1.0).abs() < 1e-5, "normal {normal:?}");
}

#[test]
fn push_out_uses_the_radial_normal_for_a_sphere() {
    let collider = Collider {
        shape: ColliderShape::Sphere,
        radius: 1.0,
        ..Default::default()
    };
    let shape = WorldShape::new(&collider, &glam::Mat4::IDENTITY);
    let (position, normal) = shape
        .push_out(glam::vec3(0.0, 1.1, 0.0), 0.2)
        .expect("point is within the particle radius of the sphere");
    assert!((position.y - 1.2).abs() < 1e-5, "position {position:?}");
    assert!((normal.y - 1.0).abs() < 1e-5, "normal {normal:?}");
}
