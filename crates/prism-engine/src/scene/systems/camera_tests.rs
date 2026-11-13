// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;
use crate::input::{ElementState, KeyCode};
use prism_ecs::World;

#[test]
fn no_camera_returns_none() {
    let world = World::new();
    assert!(collect_camera(&world).is_none());
}

#[test]
fn finds_first_camera() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(
        e,
        Camera {
            fov_y_degrees: 75.0,
            near: 0.01,
            far: 500.0,
            ..Camera::default()
        },
    );
    let cam = collect_camera(&world);
    assert!(cam.is_some());
    assert_eq!(cam.unwrap().fov_y_degrees, 75.0);
}

#[test]
fn multiple_cameras_returns_first() {
    let mut world = World::new();
    let e1 = world.spawn();
    world.insert(
        e1,
        Camera {
            fov_y_degrees: 60.0,
            ..Camera::default()
        },
    );
    let e2 = world.spawn();
    world.insert(
        e2,
        Camera {
            fov_y_degrees: 90.0,
            near: 0.1,
            far: 100.0,
            ..Camera::default()
        },
    );

    let cam = collect_camera(&world).unwrap();
    // ECS 查询 order is 确定性 - 第一个 inserted should be 第一个
    assert_eq!(cam.fov_y_degrees, 60.0);
}

/// 取景与清屏必须来自同一台相机（issue #11）。
///
/// 两台启用相机、`depth` 不同、背景色不同：view 必须来自 `depth` 最大的那台
/// （`Camera.main`），而清屏色要是**同一个实体**的背景色。
#[test]
fn view_and_clear_color_come_from_the_same_camera() {
    let mut world = World::new();

    let low = world.spawn();
    world.insert(
        low,
        Camera {
            depth: 0.0,
            background_color: [1.0, 0.0, 0.0, 1.0],
            ..Camera::default()
        },
    );
    world.insert(
        low,
        LocalToWorld(Mat4::from_translation(Vec3::new(0.0, 0.0, 5.0))),
    );

    let high = world.spawn();
    world.insert(
        high,
        Camera {
            depth: 1.0,
            background_color: [0.0, 0.0, 1.0, 1.0],
            ..Camera::default()
        },
    );
    world.insert(
        high,
        LocalToWorld(Mat4::from_translation(Vec3::new(0.0, 2.0, 9.0))),
    );

    let out = compute_camera_output(&world, &Mat4::IDENTITY).expect("camera output");
    assert_eq!(out.entity, high, "view 应来自 depth 最大的相机");
    assert!(
        (out.eye.y - 2.0).abs() < 1e-5,
        "eye 应来自同一实体的变换，实际 {:?}",
        out.eye
    );
    assert_eq!(
        clear_color(&world, out.entity),
        [0.0, 0.0, 1.0, 1.0],
        "clear color 应来自真正渲染的那台相机"
    );
}

/// 主相机缺 `LocalToWorld` 时往低调到下一台可用相机，而不是整体掉进 fallback。
#[test]
fn camera_without_transform_falls_back_to_next_enabled() {
    let mut world = World::new();

    let usable = world.spawn();
    world.insert(
        usable,
        Camera {
            depth: 0.0,
            ..Camera::default()
        },
    );
    world.insert(
        usable,
        LocalToWorld(Mat4::from_translation(Vec3::new(0.0, 0.0, 3.0))),
    );

    let transform_less = world.spawn();
    world.insert(
        transform_less,
        Camera {
            depth: 5.0,
            ..Camera::default()
        },
    );

    let out = compute_camera_output(&world, &Mat4::IDENTITY).expect("camera output");
    assert_eq!(out.entity, usable);
}

/// `#461`：只有 `depth` 最大的那台会画出来，其余相机目前会被忽略 ——
/// 这个清单就是「用户加了却没生效」的那些相机，用来提示，而不是静默吞掉。
#[test]
fn ignored_cameras_lists_everything_but_the_main_camera() {
    let mut world = World::new();

    let low = world.spawn();
    world.insert(
        low,
        Camera {
            depth: -10.0,
            ..Camera::default()
        },
    );

    let main = world.spawn();
    world.insert(
        main,
        Camera {
            depth: 5.0,
            ..Camera::default()
        },
    );

    let middle = world.spawn();
    world.insert(
        middle,
        Camera {
            depth: 0.0,
            ..Camera::default()
        },
    );

    let off = world.spawn();
    world.insert(
        off,
        Camera {
            depth: 50.0,
            enabled: false,
            ..Camera::default()
        },
    );

    // 主相机 = depth 最大的**启用**相机。
    assert_eq!(main_camera(&world), Some(main));
    // 关掉的相机既不是主相机也不在忽略清单里（它本就不该参与）。
    let ignored = ignored_cameras(&world);
    assert_eq!(ignored.len(), 2);
    assert!(ignored.contains(&low));
    assert!(ignored.contains(&middle));
    assert!(!ignored.contains(&main));
    assert!(!ignored.contains(&off));
}

/// 未启用的相机既不取景也不提供清屏色。
#[test]
fn disabled_camera_is_skipped_entirely() {
    let mut world = World::new();

    let active = world.spawn();
    world.insert(active, Camera::default());
    world.insert(active, LocalToWorld(Mat4::IDENTITY));

    let disabled = world.spawn();
    world.insert(
        disabled,
        Camera {
            depth: 9.0,
            enabled: false,
            background_color: [1.0, 0.0, 0.0, 1.0],
            ..Camera::default()
        },
    );
    world.insert(disabled, LocalToWorld(Mat4::IDENTITY));

    assert_eq!(main_camera(&world), Some(active));
    let out = compute_camera_output(&world, &Mat4::IDENTITY).expect("camera output");
    assert_eq!(out.entity, active);
    assert_eq!(
        clear_color(&world, out.entity),
        Camera::default().background_color
    );
}

/// 实体上没有 `Camera`（或已删除）时清屏色退化为 `DEFAULT_CLEAR_COLOR`。
#[test]
fn clear_color_for_camera_less_entity_is_default() {
    let mut world = World::new();
    let entity = world.spawn();
    assert_eq!(clear_color(&world, entity), DEFAULT_CLEAR_COLOR);
}

/// 清屏方式与清屏色同源：读的是同一台相机的 `clear_flags`。
#[test]
fn clear_flags_reads_the_camera_component() {
    let mut world = World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        Camera {
            clear_flags: CameraClearFlags::SolidColor,
            ..Camera::default()
        },
    );
    assert_eq!(clear_flags(&world, entity), CameraClearFlags::SolidColor);
}

/// 实体上没有 `Camera` 时清屏方式退化为引擎默认的 `Skybox`。
#[test]
fn clear_flags_for_camera_less_entity_is_skybox() {
    let world = World::new();
    let entity = prism_ecs::Entity::from_raw(u32::MAX, 0);
    assert_eq!(clear_flags(&world, entity), CameraClearFlags::Skybox);
}

/// 飞行输入必须驱动**正在渲染**的相机（PR #38 review finding 2）。
///
/// 先插入的低 `depth` 相机才是旧的「第一个 enabled」选择结果；输入应改投
/// `active_camera`（`depth` 最大且可渲染的那台）。
#[test]
fn fly_input_drives_the_rendered_camera() {
    let mut world = World::new();

    let low = world.spawn();
    world.insert(
        low,
        Camera {
            depth: 0.0,
            ..Camera::default()
        },
    );
    world.insert(low, FlyCameraController::default());
    world.insert(low, LocalTransform::default());
    world.insert(low, LocalToWorld(Mat4::IDENTITY));

    let high = world.spawn();
    world.insert(
        high,
        Camera {
            depth: 1.0,
            ..Camera::default()
        },
    );
    world.insert(high, FlyCameraController::default());
    world.insert(high, LocalTransform::default());
    world.insert(high, LocalToWorld(Mat4::IDENTITY));

    let mut input = InputManager::new();
    input.handle_keyboard(KeyCode::KeyW, ElementState::Pressed);

    // 走位键只在指针锁定时归相机，所以这里传 `look_active = true`。
    let updated = camera_controller_system(&mut world, &input, 1.0, true);
    assert_eq!(updated, Some(high), "输入应送给渲染中的相机");

    let moved = world
        .get::<LocalTransform>(high)
        .expect("rendered camera transform")
        .position
        .length();
    assert!(moved > 0.0, "渲染中的相机应被移动，实际 {moved}");

    let untouched = world
        .get::<LocalTransform>(low)
        .expect("low camera transform")
        .position
        .length();
    assert!(
        untouched < 1e-6,
        "低 depth 相机不应被移动，实际 {untouched}"
    );
}

/// 未锁定指针时键盘属于项目 UI：WASD 绝不能同时移动相机。
#[test]
fn wasd_does_not_move_the_camera_while_unlocked() {
    let mut world = World::new();
    let cam = world.spawn();
    world.insert(cam, Camera::default());
    world.insert(cam, FlyCameraController::default());
    world.insert(cam, LocalTransform::default());
    world.insert(cam, LocalToWorld(Mat4::IDENTITY));

    let mut input = InputManager::new();
    for key in [KeyCode::KeyW, KeyCode::KeyA, KeyCode::KeyS, KeyCode::KeyD] {
        input.handle_keyboard(key, ElementState::Pressed);
    }

    camera_controller_system(&mut world, &input, 1.0, false);
    let moved = world
        .get::<LocalTransform>(cam)
        .expect("camera transform")
        .position
        .length();
    assert!(moved < 1e-6, "未锁定时相机不该被 WASD 移动，实际 {moved}");
}

// ---------------------------------------------------------------------------
// 朝向写回（issue #121）
// ---------------------------------------------------------------------------

/// `yaw = 0` 必须看向 −Z（`.scene.json` 里 identity 变换的含义），不是 +X。
#[test]
fn yaw_zero_looks_down_negative_z() {
    let f = forward(0.0, 0.0);
    assert!(
        (f - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-6,
        "forward(0, 0) 应为 −Z，实际 {f:?}"
    );
    let r = right(0.0);
    assert!(
        (r - Vec3::X).length() < 1e-6,
        "right(0) 应为 +X，实际 {r:?}"
    );
    assert!(
        fly_rotation(0.0, 0.0).dot(glam::Quat::IDENTITY).abs() > 1.0 - 1e-6,
        "yaw = pitch = 0 应得到单位四元数（= 看向 −Z）"
    );
}

/// view 矩阵与写回 `LocalTransform.rotation` 的朝向必须是同一个，
/// 否则相机子节点会和画面错位。
#[test]
fn fly_view_matches_fly_rotation() {
    let yaw = 0.7;
    let pitch = -0.3;
    let eye = Vec3::new(1.0, 2.0, -3.0);
    let view = fly_view(eye, yaw, pitch);
    let expected = Mat4::from_rotation_translation(fly_rotation(yaw, pitch), eye).inverse();
    for c in 0..4 {
        for r in 0..4 {
            assert!(
                (view.col(c)[r] - expected.col(c)[r]).abs() < 1e-4,
                "view[{c}][{r}] = {} vs {}",
                view.col(c)[r],
                expected.col(c)[r]
            );
        }
    }
}

/// 环视后相机子节点必须跟着转（issue #121 的核心症状）。
#[test]
fn children_follow_the_controller_orientation() {
    let mut world = World::new();
    let cam = world.spawn();
    world.insert(cam, Camera::default());
    world.insert(
        cam,
        FlyCameraController {
            yaw: std::f32::consts::FRAC_PI_2,
            ..Default::default()
        },
    );
    world.insert(cam, LocalTransform::default());
    world.insert(cam, LocalToWorld(Mat4::IDENTITY));

    // 相机正前方一个单位的子节点（局部 −Z）
    let child = world.spawn();
    world.insert(
        child,
        LocalTransform {
            position: Vec3::new(0.0, 0.0, -1.0),
            ..Default::default()
        },
    );
    crate::scene::helpers::HierarchyHelper::reparent(&mut world, child, Some(cam));

    camera_controller_system(&mut world, &InputManager::new(), 0.0, false);
    crate::scene::systems::hierarchy::hierarchy_system(&mut world);

    // yaw = π/2 → 看向 −X，所以子节点的世界位置是 (−1, 0, 0)
    let world_pos = world
        .get::<LocalToWorld>(child)
        .expect("child world transform")
        .0
        .col(3)
        .truncate();
    assert!(
        (world_pos - Vec3::new(-1.0, 0.0, 0.0)).length() < 1e-4,
        "子节点应转到相机正前方，实际 {world_pos:?}"
    );
}

/// 相机挂在旋转父级下时：世界朝向仍是 yaw/pitch 的方向，WASD 也沿世界轴移动
/// （不能把世界轴向量直接加到局部坐标上）。
#[test]
fn parented_camera_keeps_world_orientation_and_movement() {
    let mut world = World::new();

    let parent = world.spawn();
    world.insert(
        parent,
        LocalTransform {
            rotation: glam::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            ..Default::default()
        },
    );
    world.insert(parent, LocalToWorld(Mat4::IDENTITY));

    let cam = world.spawn();
    world.insert(cam, Camera::default());
    world.insert(cam, FlyCameraController::default()); // yaw = 0 → 世界 −Z
    world.insert(
        cam,
        LocalTransform {
            position: Vec3::new(1.0, 0.0, 0.0),
            ..Default::default()
        },
    );
    world.insert(cam, LocalToWorld(Mat4::IDENTITY));
    crate::scene::helpers::HierarchyHelper::reparent(&mut world, cam, Some(parent));

    // 先算一次父级世界矩阵，控制器才读得到父级旋转。
    crate::scene::systems::hierarchy::hierarchy_system(&mut world);

    let child = world.spawn();
    world.insert(
        child,
        LocalTransform {
            position: Vec3::new(0.0, 0.0, -2.0),
            ..Default::default()
        },
    );
    crate::scene::helpers::HierarchyHelper::reparent(&mut world, child, Some(cam));

    camera_controller_system(&mut world, &InputManager::new(), 0.0, false);
    crate::scene::systems::hierarchy::hierarchy_system(&mut world);

    let cam_pos = world.get::<LocalToWorld>(cam).unwrap().0.col(3).truncate();
    let child_pos = world
        .get::<LocalToWorld>(child)
        .unwrap()
        .0
        .col(3)
        .truncate();
    assert!(
        child_pos.z - cam_pos.z < -1.9,
        "子节点应在相机世界 −Z 方向，实际 dz = {}",
        child_pos.z - cam_pos.z
    );

    let before = cam_pos;
    let mut input = InputManager::new();
    input.handle_keyboard(KeyCode::KeyW, ElementState::Pressed);
    camera_controller_system(&mut world, &input, 1.0, true);
    crate::scene::systems::hierarchy::hierarchy_system(&mut world);
    let after = world.get::<LocalToWorld>(cam).unwrap().0.col(3).truncate();

    let delta = after - before;
    assert!(delta.z < -0.5, "W 应沿世界 −Z 前进，实际 {delta:?}");
    assert!(
        delta.x.abs() < 1e-4,
        "父级旋转不该把 W 带偏到 X，实际 {delta:?}"
    );
}
