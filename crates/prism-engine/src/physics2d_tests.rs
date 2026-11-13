//! 2D 物理系统单元测试（#1160）。

use prism_ecs::World;
use prism_scene::components::{PhysicsCollider2D, LocalTransform, RigidBody2D, RigidBody2DType};

use super::physics2d::{
    overlap_box_2d, overlap_circle_2d, raycast_2d, step_2d, Physics2DEventKind, Physics2DEvents,
};

#[test]
fn test_physics2d_gravity_integration() {
    let mut world = World::new();

    let e = world.spawn();
    world.insert(
        e,
        LocalTransform {
            position: glam::vec3(0.0, 10.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(
        e,
        RigidBody2D {
            body_type: RigidBody2DType::Dynamic,
            mass: 1.0,
            linear_velocity: [0.0, 0.0],
            gravity_scale: 1.0,
            linear_damping: 0.0,
            ..Default::default()
        },
    );

    step_2d(&mut world, 0.1, [0.0, -10.0]);

    let rb = world.get::<RigidBody2D>(e).unwrap();
    assert!((rb.linear_velocity[1] - (-1.0)).abs() < 1e-4);

    let tf = world.get::<LocalTransform>(e).unwrap();
    assert!((tf.position.y - 9.9).abs() < 1e-4);
}

#[test]
fn test_physics2d_circle_circle_collision() {
    let mut world = World::new();

    let c1 = world.spawn();
    world.insert(
        c1,
        LocalTransform {
            position: glam::vec3(0.0, 0.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(c1, PhysicsCollider2D::new_circle(1.0));
    world.insert(
        c1,
        RigidBody2D {
            body_type: RigidBody2DType::Dynamic,
            mass: 1.0,
            linear_velocity: [2.0, 0.0],
            ..Default::default()
        },
    );

    let c2 = world.spawn();
    world.insert(
        c2,
        LocalTransform {
            position: glam::vec3(1.5, 0.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(c2, PhysicsCollider2D::new_circle(1.0));
    world.insert(
        c2,
        RigidBody2D {
            body_type: RigidBody2DType::Dynamic,
            mass: 1.0,
            linear_velocity: [-2.0, 0.0],
            ..Default::default()
        },
    );

    step_2d(&mut world, 0.016, [0.0, 0.0]);

    let rb1 = world.get::<RigidBody2D>(c1).unwrap();
    let rb2 = world.get::<RigidBody2D>(c2).unwrap();
    // 碰撞后两球应相互反弹或速度方向反转
    assert!(rb1.linear_velocity[0] < 2.0);
    assert!(rb2.linear_velocity[0] > -2.0);
}

#[test]
fn test_physics2d_box_box_collision() {
    let mut world = World::new();

    // 静态地板
    let floor = world.spawn();
    world.insert(
        floor,
        LocalTransform {
            position: glam::vec3(0.0, 0.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(floor, PhysicsCollider2D::new_box(10.0, 1.0));
    world.insert(
        floor,
        RigidBody2D {
            body_type: RigidBody2DType::Static,
            ..Default::default()
        },
    );

    // 下落的动态方块
    let box_e = world.spawn();
    world.insert(
        box_e,
        LocalTransform {
            position: glam::vec3(0.0, 1.8, 0.0),
            ..Default::default()
        },
    );
    world.insert(box_e, PhysicsCollider2D::new_box(1.0, 1.0));
    world.insert(
        box_e,
        RigidBody2D {
            body_type: RigidBody2DType::Dynamic,
            mass: 2.0,
            linear_velocity: [0.0, -5.0],
            ..Default::default()
        },
    );

    step_2d(&mut world, 0.02, [0.0, 0.0]);

    let rb = world.get::<RigidBody2D>(box_e).unwrap();
    // 撞击静止地面后，下落速度被阻止或反弹
    assert!(rb.linear_velocity[1] >= -0.1);
}

#[test]
fn test_physics2d_sensor_events() {
    let mut world = World::new();

    // 传感器实体
    let sensor = world.spawn();
    world.insert(
        sensor,
        LocalTransform {
            position: glam::vec3(0.0, 0.0, 0.0),
            ..Default::default()
        },
    );
    let mut sensor_col = PhysicsCollider2D::new_circle(2.0);
    sensor_col.is_sensor = true;
    world.insert(sensor, sensor_col);

    // 穿过传感器的物体
    let walker = world.spawn();
    world.insert(
        walker,
        LocalTransform {
            position: glam::vec3(0.0, 0.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(walker, PhysicsCollider2D::new_circle(0.5));
    world.insert(
        walker,
        RigidBody2D {
            body_type: RigidBody2DType::Kinematic,
            linear_velocity: [10.0, 0.0],
            ..Default::default()
        },
    );

    // 第 1 步：重叠，触发 Started 事件
    step_2d(&mut world, 0.1, [0.0, 0.0]);
    let events = world.get_resource::<Physics2DEvents>().unwrap();
    assert!(events
        .events
        .iter()
        .any(|e| e.kind == Physics2DEventKind::Started && e.is_sensor));

    // 第 2 步：移出传感器范围 (x = 1.0 + 10 * 0.5 = 6.0 > 2.5)
    world.get_mut::<LocalTransform>(walker).unwrap().position = glam::vec3(10.0, 0.0, 0.0);
    step_2d(&mut world, 0.1, [0.0, 0.0]);
    let events2 = world.get_resource::<Physics2DEvents>().unwrap();
    assert!(events2
        .events
        .iter()
        .any(|e| e.kind == Physics2DEventKind::Stopped && e.is_sensor));
}

#[test]
fn test_physics2d_raycast_circle() {
    let mut world = World::new();

    let circle = world.spawn();
    world.insert(
        circle,
        LocalTransform {
            position: glam::vec3(5.0, 0.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(circle, PhysicsCollider2D::new_circle(1.0));

    // 从原点向 +X 发射射线
    let hit =
        raycast_2d(&world, [0.0, 0.0], [1.0, 0.0], 10.0, 0xFFFF_FFFF).expect("should hit circle");
    assert_eq!(hit.entity, circle);
    assert!((hit.distance - 4.0).abs() < 1e-4);
    assert!((hit.normal[0] - (-1.0)).abs() < 1e-4);
}

#[test]
fn test_physics2d_raycast_box() {
    let mut world = World::new();

    let box_e = world.spawn();
    world.insert(
        box_e,
        LocalTransform {
            position: glam::vec3(0.0, 5.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(box_e, PhysicsCollider2D::new_box(2.0, 1.0));

    // 从下方往 +Y 发射射线
    let hit = raycast_2d(&world, [0.0, 0.0], [0.0, 1.0], 10.0, 0xFFFF_FFFF)
        .expect("should hit box bottom");
    assert_eq!(hit.entity, box_e);
    // 盒中心 y=5, 半高=1，底面 y=4
    assert!((hit.distance - 4.0).abs() < 1e-4);
    assert!((hit.normal[1] - (-1.0)).abs() < 1e-4);
}

#[test]
fn test_physics2d_overlap_queries() {
    let mut world = World::new();

    let e1 = world.spawn();
    world.insert(
        e1,
        LocalTransform {
            position: glam::vec3(1.0, 1.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(e1, PhysicsCollider2D::new_circle(1.0));

    let e2 = world.spawn();
    world.insert(
        e2,
        LocalTransform {
            position: glam::vec3(10.0, 10.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(e2, PhysicsCollider2D::new_box(1.0, 1.0));

    let circle_hits = overlap_circle_2d(&world, [0.0, 0.0], 3.0, 0xFFFF_FFFF);
    assert!(circle_hits.contains(&e1));
    assert!(!circle_hits.contains(&e2));

    let box_hits = overlap_box_2d(&world, [10.0, 10.0], [2.0, 2.0], 0.0, 0xFFFF_FFFF);
    assert!(box_hits.contains(&e2));
    assert!(!box_hits.contains(&e1));
}
