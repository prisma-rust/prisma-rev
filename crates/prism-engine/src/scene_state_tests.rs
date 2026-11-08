use super::*;
use crate::scene::components::LocalToWorld;

fn transform_json(position: [f32; 3]) -> String {
    format!(
        "{{\"position\":[{},{},{}],\"rotation\":[0,0,0,1],\"scale\":[1,1,1]}}",
        position[0], position[1], position[2]
    )
}

fn spawn_point_light(world: &mut World, position: [f32; 3]) -> Entity {
    let entity = world.spawn();
    world.insert(
        entity,
        LocalTransform {
            position: position.into(),
            ..Default::default()
        },
    );
    world.insert(entity, ScenePtLight::default());
    world.insert(entity, Active(true));
    entity
}

#[test]
fn saved_transform_keeps_the_full_quaternion() {
    let rotation = Quat::from_axis_angle(Vec3::Y, std::f32::consts::FRAC_PI_2);
    let transform = LocalTransform {
        position: Vec3::new(1.0, 2.0, 3.0),
        rotation,
        scale: Vec3::ONE,
    };

    let saved = saved_transform_to_json(&SavedTransform {
        id: Some(7),
        transform,
    });
    let rotation_field = saved
        .split("\"rotation\":[")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_default();
    assert_eq!(
        rotation_field.split(',').count(),
        4,
        "rotation must serialize all four quaternion components: {saved}"
    );

    let parsed = parse_transform_array(&format!("[{saved}]"));
    assert_eq!(parsed.len(), 1, "{saved}");
    // 四元数 q 与 -q 表示同一旋转，比较绝对值点积即可。
    assert_eq!(parsed[0].id, Some(7), "id must survive the round trip");
    assert!(
        parsed[0].transform.rotation.dot(rotation).abs() > 0.999_999,
        "expected {rotation:?}, got {:?}",
        parsed[0].transform.rotation
    );
}

#[test]
fn legacy_three_component_rotation_loads_as_identity() {
    // 旧文件（v1-v3）只写了 x,y,z；读入时回退到单位四元数，不 panic。
    let mut world = World::new();
    let entity = world.spawn();
    world.insert(entity, LocalTransform::default());
    let json = format!(
        "{{\"version\":3,\"transforms\":[{{\"position\":[1,2,3],\"rotation\":[0,0,0],\"scale\":[1,1,1]}}]}}"
    );

    apply_scene_state(&mut world, &json);

    let loaded = world.get::<LocalTransform>(entity).unwrap();
    assert_eq!(loaded.position, [1.0, 2.0, 3.0].into());
    assert_eq!(loaded.rotation, Quat::IDENTITY);
}

#[test]
fn empty_point_light_array_clears_existing_lights() {
    let mut world = World::new();
    let entity = spawn_point_light(&mut world, [2.0, 3.0, 4.0]);

    apply_scene_state(
        &mut world,
        r#"{"version":2,"pointLights":[],"transforms":[]}"#,
    );

    assert!(world.query::<ScenePtLight>().next().is_none());
    assert!(world.get::<ScenePtLight>(entity).is_none());
}

#[test]
fn removed_point_light_transform_does_not_consume_object_transform() {
    let mut world = World::new();
    let light = spawn_point_light(&mut world, [90.0, 90.0, 90.0]);
    let object = world.spawn();
    world.insert(object, LocalTransform::default());
    let object_json = transform_json([1.0, 2.0, 3.0]);
    let json = format!("{{\"version\":2,\"pointLights\":[],\"transforms\":[{object_json}]}}");

    apply_scene_state(&mut world, &json);

    assert_eq!(
        world.get::<LocalTransform>(object).unwrap().position,
        [1.0, 2.0, 3.0].into()
    );
    assert_eq!(
        world.get::<LocalTransform>(light).unwrap().position,
        [90.0, 90.0, 90.0].into()
    );
}

#[test]
fn version_two_keeps_position_and_active_state_with_point_light() {
    let mut world = World::new();
    let entity = spawn_point_light(&mut world, [0.0; 3]);
    let json = r#"{
            "version":2,
            "pointLights":[{
                "position":[4,5,6],
                "range":8,
                "color":[1,0.25,0.5],
                "intensity":42,
                "active":false
            }],
            "transforms":[]
        }"#;

    apply_scene_state(&mut world, json);

    let light = world.get::<ScenePtLight>(entity).unwrap();
    assert_eq!(light.color, [1.0, 0.25, 0.5].into());
    assert_eq!(light.intensity, 42.0);
    assert_eq!(light.range, 8.0);
    assert_eq!(
        world.get::<LocalTransform>(entity).unwrap().position,
        [4.0, 5.0, 6.0].into()
    );
    assert_eq!(world.get::<Active>(entity), Some(&Active(false)));
}

#[test]
fn version_one_consumes_only_leading_point_light_transforms() {
    let mut world = World::new();
    let light = spawn_point_light(&mut world, [0.0; 3]);
    let object = world.spawn();
    world.insert(object, LocalTransform::default());
    let light_transform = transform_json([4.0, 5.0, 6.0]);
    let object_transform = transform_json([7.0, 8.0, 9.0]);
    let json = format!(
            "{{\"pointLights\":[{{\"range\":12,\"color\":[1,0.2,0.2],\"intensity\":150}}],\"transforms\":[{light_transform},{object_transform}]}}"
        );

    apply_scene_state(&mut world, &json);

    assert_eq!(
        world.get::<LocalTransform>(light).unwrap().position,
        [4.0, 5.0, 6.0].into()
    );
    assert_eq!(
        world.get::<LocalTransform>(object).unwrap().position,
        [7.0, 8.0, 9.0].into()
    );
}

#[test]
fn saved_point_light_does_not_create_an_entity() {
    let mut world = World::new();
    let json = r#"{
            "version":2,
            "pointLights":[{
                "position":[4,5,6],
                "range":8,
                "color":[1,0.25,0.5],
                "intensity":42,
                "active":true
            }],
            "transforms":[]
        }"#;

    apply_scene_state(&mut world, json);

    assert!(world.query::<ScenePtLight>().next().is_none());
    assert!(world.query::<LocalTransform>().next().is_none());
    assert!(world.query::<Active>().next().is_none());
}

// ---------------------------------------------------------------------------
// #45 — 保存/恢复必须用「被渲染的那台相机」，而不是插入顺序第一台
// ---------------------------------------------------------------------------

/// 生成一台完整相机（`Camera` + `FlyCameraController` + `LocalTransform`）。
/// `with_world_matrix` 决定是否再补上层级系统算出的 `LocalToWorld`
/// （`active_camera` 要求它存在，加载早期则可能还没有）。
fn spawn_camera(
    world: &mut World,
    depth: f32,
    position: [f32; 3],
    enabled: bool,
    with_world_matrix: bool,
) -> Entity {
    let entity = world.spawn();
    world.insert(
        entity,
        Camera {
            depth,
            enabled,
            ..Default::default()
        },
    );
    world.insert(entity, FlyCameraController::default());
    world.insert(
        entity,
        LocalTransform {
            position: position.into(),
            ..Default::default()
        },
    );
    if with_world_matrix {
        world.insert(entity, LocalToWorld(glam::Mat4::IDENTITY));
    }
    entity
}

#[test]
fn saved_camera_state_uses_the_rendered_camera() {
    let mut world = World::new();
    let behind = spawn_camera(&mut world, 0.0, [1.0, 0.0, 0.0], true, true);
    let rendered = spawn_camera(&mut world, 1.0, [2.0, 3.0, 4.0], true, true);
    let disabled = spawn_camera(&mut world, 9.0, [5.0, 6.0, 7.0], false, true);

    let snapshot = snapshot_camera_state(&world).expect("world has cameras");
    assert_eq!(snapshot.position, Vec3::new(2.0, 3.0, 4.0));

    // 保存走同一选择器：最高 `depth` 的**启用**相机，而不是插入顺序第一台。
    assert_eq!(state_camera(&world), Some(rendered));
    assert_ne!(state_camera(&world), Some(behind));
    assert_ne!(state_camera(&world), Some(disabled));
}

#[test]
fn restored_camera_state_writes_the_rendered_camera_only() {
    let mut world = World::new();
    let behind = spawn_camera(&mut world, 0.0, [1.0, 1.0, 1.0], true, true);
    let rendered = spawn_camera(&mut world, 1.0, [2.0, 2.0, 2.0], true, true);
    let json = format!(
        "{{\"version\":3,\"camera\":{},\"transforms\":[]}}",
        CameraState {
            position: Vec3::new(7.0, 8.0, 9.0),
            yaw: 0.5,
            pitch: -0.25,
            fov_y: 1.0,
            move_speed: 12.0,
            look_sensitivity: 0.01,
            znear: 0.2,
            zfar: 500.0,
        }
        .to_json()
    );

    apply_scene_state(&mut world, &json);

    let rendered_transform = world.get::<LocalTransform>(rendered).unwrap();
    assert_eq!(rendered_transform.position, Vec3::new(7.0, 8.0, 9.0));
    let rendered_controller = world.get::<FlyCameraController>(rendered).unwrap();
    assert_eq!(rendered_controller.yaw, 0.5);
    assert_eq!(rendered_controller.move_speed, 12.0);
    assert_eq!(
        world.get::<Camera>(rendered).unwrap().fov_y_degrees,
        1.0_f32.to_degrees()
    );

    // 较低 `depth` 的相机保持原样。
    let behind_transform = world.get::<LocalTransform>(behind).unwrap();
    assert_eq!(behind_transform.position, Vec3::new(1.0, 1.0, 1.0));
    assert_eq!(world.get::<FlyCameraController>(behind).unwrap().yaw, 0.0);
}

#[test]
fn camera_choice_falls_back_to_main_camera_before_hierarchy_runs() {
    // 加载场景状态时层级传播可能还没跑过：没有 `LocalToWorld` 时
    // `active_camera` 返回 `None`，只能按 `main_camera`（最高 depth 的启用相机）兜底。
    let mut world = World::new();
    let _first = spawn_camera(&mut world, 0.0, [1.0, 0.0, 0.0], true, false);
    let main = spawn_camera(&mut world, 2.0, [2.0, 0.0, 0.0], true, false);
    assert!(camera::active_camera(&world).is_none());

    assert_eq!(state_camera(&world), Some(main));
    let snapshot = snapshot_camera_state(&world).expect("main camera is complete");
    assert_eq!(snapshot.position, Vec3::new(2.0, 0.0, 0.0));
}

/// #118：恢复按 `.scene.json` 稳定 id 配对，而不是按 ECS 池顺序 zip。
///
/// 存档里的顺序与池顺序故意相反：老实现会把位姿互换，这正是「场景里插入一个
/// 实体后所有存档位姿错位」的根因。
#[test]
fn v4_restores_transforms_by_entity_id_not_by_pool_order() {
    let mut world = World::new();
    let first = world.spawn();
    world.insert(first, SceneEntityId(1));
    world.insert(
        first,
        LocalTransform {
            position: Vec3::new(1.0, 0.0, 0.0),
            ..Default::default()
        },
    );
    let second = world.spawn();
    world.insert(second, SceneEntityId(2));
    world.insert(
        second,
        LocalTransform {
            position: Vec3::new(2.0, 0.0, 0.0),
            ..Default::default()
        },
    );

    let json = r#"{"version":4,"transforms":[
        {"id":2,"position":[20,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
        {"id":1,"position":[10,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}
    ]}"#;
    apply_scene_state(&mut world, json);

    assert_eq!(
        world.get::<LocalTransform>(first).unwrap().position,
        Vec3::new(10.0, 0.0, 0.0),
        "id 1 必须拿回自己的位姿"
    );
    assert_eq!(
        world.get::<LocalTransform>(second).unwrap().position,
        Vec3::new(20.0, 0.0, 0.0),
        "id 2 必须拿回自己的位姿"
    );
}

/// #118：存档里多出来的 id（场景已删掉的实体）不会占用别的实体的位姿。
#[test]
fn v4_ignores_saved_transforms_for_missing_entities() {
    let mut world = World::new();
    let kept = world.spawn();
    world.insert(kept, SceneEntityId(5));
    world.insert(
        kept,
        LocalTransform {
            position: Vec3::ZERO,
            ..Default::default()
        },
    );

    let json = r#"{"version":4,"transforms":[
        {"id":99,"position":[70,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
        {"id":5,"position":[50,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}
    ]}"#;
    apply_scene_state(&mut world, json);

    assert_eq!(
        world.get::<LocalTransform>(kept).unwrap().position,
        Vec3::new(50.0, 0.0, 0.0),
        "已删除实体的记录不能把位姿挤给别的实体（老实现会按顺序错位）"
    );
}

/// #484：存档文件名 / 目录由宿主决定。
#[test]
fn the_save_path_follows_the_host_directory_and_slot() {
    let base = std::path::Path::new("C:/games/save");
    assert_eq!(
        scene_state_path_in(Some(base), None),
        std::path::PathBuf::from("C:/games/save/scene_state.json")
    );
    assert_eq!(
        scene_state_path_in(Some(base), Some(3)),
        std::path::PathBuf::from("C:/games/save/scene_state-3.json")
    );
    // 没有任何目录时退化成当前目录（不再 panic）。
    let bare = scene_state_path_in(None, None);
    assert!(bare.to_string_lossy().ends_with("scene_state.json"));
}

/// #484：`SaveDirectory` resource 覆盖默认目录，槽位互不覆盖。
#[test]
fn slots_round_trip_through_the_save_directory() {
    let dir = std::env::temp_dir().join(format!("prism-scene-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut world = World::new();
    world.insert_resource(SaveDirectory(dir.clone()));

    let entity = world.spawn();
    world.insert(
        entity,
        LocalTransform {
            position: Vec3::new(1.0, 2.0, 3.0),
            ..Default::default()
        },
    );
    world.insert(entity, SceneEntityId(11));

    save_scene_state_slot(&world, 1);
    assert!(dir.join("scene_state-1.json").is_file());
    assert_eq!(scene_state_path(&world), dir.join("scene_state.json"));

    world.get_mut::<LocalTransform>(entity).unwrap().position = Vec3::new(9.0, 9.0, 9.0);
    assert!(load_scene_state_slot(&mut world, 1));
    assert_eq!(
        world.get::<LocalTransform>(entity).unwrap().position,
        Vec3::new(1.0, 2.0, 3.0)
    );

    // 不存在的槽位 = 没有存档，不能悄悄成功。
    assert!(!load_scene_state_slot(&mut world, 7));

    let _ = std::fs::remove_dir_all(&dir);
}

/// #484：显式路径原语（「另存存档」/ 测试用）。
#[test]
fn explicit_paths_work_without_a_save_directory_resource() {
    let dir = std::env::temp_dir().join(format!("prism-scene-state-x-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("custom/test.json");

    let mut world = World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        LocalTransform {
            position: Vec3::new(4.0, 5.0, 6.0),
            ..Default::default()
        },
    );
    world.insert(entity, SceneEntityId(3));

    save_scene_state_to(&world, &path);
    assert!(path.is_file(), "父目录要自动创建");

    world.get_mut::<LocalTransform>(entity).unwrap().position = Vec3::ZERO;
    assert!(load_scene_state_from(&mut world, &path));
    assert_eq!(
        world.get::<LocalTransform>(entity).unwrap().position,
        Vec3::new(4.0, 5.0, 6.0)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #1051: the advanced hour survives a save/load round trip through a file.
#[test]
fn scene_state_round_trips_the_time_of_day() {
    let dir = std::env::temp_dir().join(format!("prism-scene-state-clock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("scene_state.json");

    let mut world = World::new();
    let clock_entity = world.spawn();
    world.insert(clock_entity, SceneEntityId(4));
    world.insert(
        clock_entity,
        TimeOfDay {
            hours: 21.25,
            ..Default::default()
        },
    );
    save_scene_state_to(&world, &path);

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"version\":5"), "{text}");
    assert!(text.contains("\"timeOfDay\""), "{text}");

    // A fresh world at the authored hour must pick the saved one back up.
    let mut loaded = World::new();
    let restored = loaded.spawn();
    loaded.insert(restored, SceneEntityId(4));
    loaded.insert(restored, TimeOfDay::default());
    apply_scene_state(&mut loaded, &text);

    let clock = loaded.get::<TimeOfDay>(restored).unwrap();
    assert!((clock.hours - 21.25).abs() < 1e-4, "{text}");
    assert_eq!(
        clock.day_length_seconds,
        TimeOfDay::default().day_length_seconds,
        "only the hour is restored; day length stays authored data"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #1051: clocks are paired by `.scene.json` id, not by pool order.
#[test]
fn time_of_day_is_matched_by_entity_id() {
    let mut world = World::new();
    let first = world.spawn();
    world.insert(first, SceneEntityId(1));
    world.insert(
        first,
        TimeOfDay {
            hours: 1.0,
            ..Default::default()
        },
    );
    let second = world.spawn();
    world.insert(second, SceneEntityId(2));
    world.insert(
        second,
        TimeOfDay {
            hours: 2.0,
            ..Default::default()
        },
    );

    let json = r#"{"version":5,"timeOfDay":[{"id":2,"hours":22.0},{"id":1,"hours":11.0}]}"#;
    apply_scene_state(&mut world, json);

    assert!(
        (world.get::<TimeOfDay>(first).unwrap().hours - 11.0).abs() < 1e-4,
        "id 1 must get its own hour"
    );
    assert!(
        (world.get::<TimeOfDay>(second).unwrap().hours - 22.0).abs() < 1e-4,
        "id 2 must get its own hour"
    );
}

/// #1051: a v4 (or older) file has no clocks and must not reset the scene's.
#[test]
fn files_without_clocks_leave_the_clock_alone() {
    let mut world = World::new();
    let clock_entity = world.spawn();
    world.insert(
        clock_entity,
        TimeOfDay {
            hours: 9.0,
            ..Default::default()
        },
    );

    apply_scene_state(&mut world, r#"{"version":4,"transforms":[]}"#);

    assert_eq!(world.get::<TimeOfDay>(clock_entity).unwrap().hours, 9.0);
}
