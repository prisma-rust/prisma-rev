//! 2D 虚拟相机单元测试（#1160）。

use prism_ecs::World;
use prism_scene::components::{Camera, Camera2D, LocalTransform};

use super::camera2d::camera_2d_system;

#[test]
fn test_camera_2d_instant_follow() {
    let mut world = World::new();

    let target = world.spawn();
    world.insert(
        target,
        LocalTransform {
            position: glam::vec3(10.0, 5.0, 0.0),
            ..Default::default()
        },
    );

    let camera = world.spawn();
    world.insert(
        camera,
        LocalTransform {
            position: glam::vec3(0.0, 0.0, -10.0),
            ..Default::default()
        },
    );
    world.insert(
        camera,
        Camera2D {
            target_entity: target.id() as u64,
            damping: 0.0,
            ..Default::default()
        },
    );

    camera_2d_system(&mut world, 0.016);

    let cam_tf = world.get::<LocalTransform>(camera).unwrap();
    assert!((cam_tf.position.x - 10.0).abs() < 1e-4);
    assert!((cam_tf.position.y - 5.0).abs() < 1e-4);
}

#[test]
fn test_camera_2d_deadzone() {
    let mut world = World::new();

    let target = world.spawn();
    world.insert(
        target,
        LocalTransform {
            position: glam::vec3(1.0, 1.0, 0.0),
            ..Default::default()
        },
    );

    let camera = world.spawn();
    world.insert(
        camera,
        LocalTransform {
            position: glam::vec3(0.0, 0.0, -10.0),
            ..Default::default()
        },
    );
    world.insert(
        camera,
        Camera2D {
            target_entity: target.id() as u64,
            damping: 0.0,
            deadzone: [2.0, 2.0],
            ..Default::default()
        },
    );

    // 目标在死区内 (1.0 <= 2.0)，相机保持不动
    camera_2d_system(&mut world, 0.016);
    let cam_tf = world.get::<LocalTransform>(camera).unwrap();
    assert_eq!(cam_tf.position.x, 0.0);
    assert_eq!(cam_tf.position.y, 0.0);

    // 目标移出死区 (5.0 > 2.0)，相机跟随移动 5.0 - 2.0 = 3.0
    world.get_mut::<LocalTransform>(target).unwrap().position = glam::vec3(5.0, 0.0, 0.0);
    camera_2d_system(&mut world, 0.016);
    let cam_tf = world.get::<LocalTransform>(camera).unwrap();
    assert!((cam_tf.position.x - 3.0).abs() < 1e-4);
}

#[test]
fn test_camera_2d_confiner() {
    let mut world = World::new();

    let target = world.spawn();
    world.insert(
        target,
        LocalTransform {
            position: glam::vec3(100.0, 100.0, 0.0),
            ..Default::default()
        },
    );

    let camera = world.spawn();
    world.insert(
        camera,
        Camera {
            orthographic: true,
            orthographic_size: 5.0,
            aspect: 2.0,
            ..Default::default()
        },
    );
    world.insert(
        camera,
        LocalTransform {
            position: glam::vec3(0.0, 0.0, -10.0),
            ..Default::default()
        },
    );
    world.insert(
        camera,
        Camera2D {
            target_entity: target.id() as u64,
            damping: 0.0,
            confiner_enabled: true,
            confiner_bounds: [-20.0, -10.0, 20.0, 10.0],
            ..Default::default()
        },
    );

    // 视口半宽 = 5.0 * 2.0 = 10.0，半高 = 5.0
    // 边界限制：X 范围 [-20 + 10, 20 - 10] = [-10, 10]，Y 范围 [-10 + 5, 10 - 5] = [-5, 5]
    camera_2d_system(&mut world, 0.016);
    let cam_tf = world.get::<LocalTransform>(camera).unwrap();
    assert!((cam_tf.position.x - 10.0).abs() < 1e-4);
    assert!((cam_tf.position.y - 5.0).abs() < 1e-4);
}

#[test]
fn test_camera_2d_pixel_snapping() {
    let mut world = World::new();

    let target = world.spawn();
    world.insert(
        target,
        LocalTransform {
            position: glam::vec3(1.033, 2.088, 0.0),
            ..Default::default()
        },
    );

    let camera = world.spawn();
    world.insert(
        camera,
        LocalTransform {
            position: glam::vec3(0.0, 0.0, -10.0),
            ..Default::default()
        },
    );
    world.insert(
        camera,
        Camera2D {
            target_entity: target.id() as u64,
            damping: 0.0,
            pixel_snapping: true,
            pixels_per_unit: 16.0,
            ..Default::default()
        },
    );

    camera_2d_system(&mut world, 0.016);
    let cam_tf = world.get::<LocalTransform>(camera).unwrap();
    // 1.033 * 16 = 16.528 -> 17 / 16 = 1.0625
    // 2.088 * 16 = 33.408 -> 33 / 16 = 2.0625
    assert!((cam_tf.position.x - 1.0625).abs() < 1e-4);
    assert!((cam_tf.position.y - 2.0625).abs() < 1e-4);
}
