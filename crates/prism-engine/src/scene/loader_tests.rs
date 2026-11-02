// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;
use glam::Quat;
use prism_ecs::World;

// ── helpers ───────────────────────────────────────────────────────

/// 构建 RSCN 字节 (v2 格式 from a simple 描述
///
/// Each 实体 元组 `(name, parent_idx, 平移 旋转 音阶
/// has_mesh, mesh_path, has_material, mat_path, has_light, light_type,
/// light_color, light_intensity, light_range, has_camera, camera_fov,
/// camera_near, camera_far, has_skybox, skybox_hdr_path, skybox_enabled)`.
#[allow(clippy::too_many_arguments)]
fn make_rscn(entities: &[RscnEntity]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RSCN");
    buf.push(2); // version 2 (v2 = skybox support in header + per-entity)
    buf.extend_from_slice(&(entities.len() as u32).to_le_bytes());

    // v2 header: env_len(2) + env_path 空 = no skybox at header level).
    buf.extend_from_slice(&0u16.to_le_bytes());

    for e in entities {
        // Name.
        let name_bytes = e.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);

        // Parent.
        let parent: i32 = e.parent.map(|p| p as i32).unwrap_or(-1);
        buf.extend_from_slice(&parent.to_le_bytes());

        // 变换
        for &v in &e.position {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        for &v in &e.rotation {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        for &v in &e.scale {
            buf.extend_from_slice(&v.to_le_bytes());
        }

        // Flags.
        let mut flags: u8 = 0;
        if e.has_mesh {
            flags |= FLAG_HAS_MESH;
        }
        if e.has_material {
            flags |= FLAG_HAS_MATERIAL;
        }
        if e.has_light {
            flags |= FLAG_HAS_LIGHT;
        }
        if e.has_camera {
            flags |= FLAG_HAS_CAMERA;
        }
        if e.has_skybox {
            flags |= FLAG_HAS_SKYBOX;
        }
        buf.push(flags);

        // 网格 path.
        if e.has_mesh {
            let path_bytes = e.mesh_path.as_bytes();
            buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(path_bytes);
        }

        // 材质 path.
        if e.has_material {
            let path_bytes = e.material_path.as_bytes();
            buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(path_bytes);
        }

        // 光源
        if e.has_light {
            buf.push(e.light_type);
            for &v in &e.light_color {
                buf.extend_from_slice(&v.to_le_bytes());
            }
            buf.extend_from_slice(&e.light_intensity.to_le_bytes());
            buf.extend_from_slice(&e.light_range.to_le_bytes());
            buf.extend_from_slice(&e.light_inner_cone.to_le_bytes());
            buf.extend_from_slice(&e.light_outer_cone.to_le_bytes());
        }

        // 相机
        if e.has_camera {
            buf.extend_from_slice(&e.camera_fov.to_le_bytes());
            buf.extend_from_slice(&e.camera_near.to_le_bytes());
            buf.extend_from_slice(&e.camera_far.to_le_bytes());
        }

        // Skybox.
        if e.has_skybox {
            let path_bytes = e.skybox_hdr_path.as_bytes();
            buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(path_bytes);
            buf.push(if e.skybox_enabled { 1 } else { 0 });
        }
    }

    buf
}

struct RscnEntity {
    name: &'static str,
    parent: Option<u32>,
    position: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
    has_mesh: bool,
    mesh_path: &'static str,
    has_material: bool,
    material_path: &'static str,
    has_light: bool,
    light_type: u8,
    light_color: [f32; 3],
    light_intensity: f32,
    light_range: f32,
    light_inner_cone: f32,
    light_outer_cone: f32,
    has_camera: bool,
    camera_fov: f32,
    camera_near: f32,
    camera_far: f32,
    has_skybox: bool,
    skybox_hdr_path: &'static str,
    skybox_enabled: bool,
}

fn simple_entity(name: &'static str, parent: Option<u32>) -> RscnEntity {
    RscnEntity {
        name,
        parent,
        position: [0.0; 3],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
        has_mesh: false,
        mesh_path: "",
        has_material: false,
        material_path: "",
        has_light: false,
        light_type: 0,
        light_color: [0.0; 3],
        light_intensity: 0.0,
        light_range: 0.0,
        light_inner_cone: 0.0,
        light_outer_cone: 0.0,
        has_camera: false,
        camera_fov: 0.0,
        camera_near: 0.0,
        camera_far: 0.0,
        has_skybox: false,
        skybox_hdr_path: "",
        skybox_enabled: true,
    }
}

// ── parse_rscn tests ────────────────────────────────────────────

#[test]
fn parse_single_root() {
    let e = simple_entity("Root", None);
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();
    assert_eq!(parsed.entities.len(), 1);
    assert_eq!(parsed.entities[0].name, "Root");
    assert!(parsed.entities[0].parent.is_none());
}

#[test]
fn parse_parent_child() {
    let root = simple_entity("Root", None);
    let child = simple_entity("Child", Some(0));
    let bytes = make_rscn(&[root, child]);
    let parsed = parse_rscn(&bytes).unwrap();
    assert_eq!(parsed.entities.len(), 2);
    assert_eq!(parsed.entities[0].name, "Root");
    assert!(parsed.entities[0].parent.is_none());
    assert_eq!(parsed.entities[1].name, "Child");
    assert_eq!(parsed.entities[1].parent, Some(0));
}

#[test]
fn parse_with_transform() {
    let e = RscnEntity {
        position: [1.0, 2.0, 3.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [2.0, 2.0, 2.0],
        ..simple_entity("Moved", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();
    assert_eq!(parsed.entities[0].position, [1.0, 2.0, 3.0]);
    assert_eq!(parsed.entities[0].rotation, [0.0, 0.0, 0.0, 1.0]);
    assert_eq!(parsed.entities[0].scale, [2.0, 2.0, 2.0]);
}

#[test]
fn parse_with_mesh_and_material() {
    let e = RscnEntity {
        has_mesh: true,
        mesh_path: "models/box.gltf",
        has_material: true,
        material_path: "materials/plastic.mat",
        ..simple_entity("Prop", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();
    assert!(parsed.entities[0].has_mesh);
    assert_eq!(parsed.entities[0].mesh_path, "models/box.gltf");
    assert!(parsed.entities[0].has_material);
    assert_eq!(parsed.entities[0].material_path, "materials/plastic.mat");
}

#[test]
fn parse_with_light() {
    let e = RscnEntity {
        has_light: true,
        light_type: 0, // directional
        light_color: [1.0, 0.95, 0.9],
        light_intensity: 3.0,
        ..simple_entity("Sun", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();
    assert!(parsed.entities[0].has_light);
    assert_eq!(parsed.entities[0].light_type, 0);
    assert_eq!(parsed.entities[0].light_color[0], 1.0);
}

#[test]
fn parse_with_camera() {
    let e = RscnEntity {
        has_camera: true,
        camera_fov: 60.0,
        camera_near: 0.1,
        camera_far: 1000.0,
        ..simple_entity("Cam", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();
    assert!(parsed.entities[0].has_camera);
    assert_eq!(parsed.entities[0].camera_fov, 60.0);
}

#[test]
fn parse_unnamed_entity() {
    let e = RscnEntity {
        name: "",
        ..simple_entity("", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();
    assert_eq!(parsed.entities[0].name, "");
}

#[test]
fn parse_rejects_bad_magic() {
    assert!(parse_rscn(b"XXXX").is_err());
}

#[test]
fn parse_rejects_too_short() {
    assert!(parse_rscn(b"RSCN").is_err());
}

#[test]
fn parse_rejects_bad_version() {
    let mut data = b"RSCN".to_vec();
    data.push(99);
    data.extend_from_slice(&1u32.to_le_bytes());
    assert!(parse_rscn(&data).is_err());
}

// ── spawn_from_parsed tests ─────────────────────────────────────

#[test]
fn spawn_single_root_entity() {
    let e = simple_entity("Root", None);
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    assert_eq!(inst.all_entities.len(), 1);
    assert_eq!(inst.root_entities.len(), 1);
}

#[test]
fn spawn_parent_child_hierarchy() {
    let root = simple_entity("Root", None);
    let child = simple_entity("Child", Some(0));
    let bytes = make_rscn(&[root, child]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    assert_eq!(inst.all_entities.len(), 2);
    assert_eq!(inst.root_entities.len(), 1);

    // Check hierarchy.
    let child_entity = inst.all_entities[1];
    assert!(world.get::<Parent>(child_entity).is_some());
    assert_eq!(
        world.get::<Parent>(child_entity).unwrap().0,
        inst.all_entities[0]
    );
}

#[test]
fn spawn_has_scene_member_and_active() {
    let e = simple_entity("E", None);
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let entity = inst.all_entities[0];
    assert_eq!(world.get::<SceneMember>(entity), Some(&SceneMember(sid)));
    assert_eq!(world.get::<Active>(entity), Some(&Active(true)));
}

#[test]
fn spawn_has_local_transform() {
    let e = RscnEntity {
        position: [10.0, 20.0, 30.0].into(),
        ..simple_entity("Moved", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let lt = world.get::<LocalTransform>(inst.all_entities[0]).unwrap();
    assert_eq!(lt.position, [10.0, 20.0, 30.0].into());
    assert_eq!(lt.rotation, Quat::IDENTITY);
}

#[test]
fn spawn_has_world_transform() {
    let e = simple_entity("E", None);
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let wt = world.get::<LocalToWorld>(inst.all_entities[0]).unwrap();
    // Identity 模型 矩阵
    assert_eq!(wt.0.col(0)[0], 1.0);
    assert_eq!(wt.0.col(3)[3], 1.0);
}

#[test]
fn spawn_with_mesh_component() {
    let e = RscnEntity {
        has_mesh: true,
        mesh_path: "models/cube.gltf",
        ..simple_entity("Cube", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let mr = world.get::<MeshRef>(inst.all_entities[0]);
    assert!(mr.is_some(), "entity should have MeshRef");
}

#[test]
fn spawn_with_material_component() {
    let e = RscnEntity {
        has_material: true,
        material_path: "materials/red.mat",
        ..simple_entity("Red", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let mar = world.get::<MaterialRef>(inst.all_entities[0]);
    assert!(mar.is_some(), "entity should have MaterialRef");
}

#[test]
fn spawn_with_directional_light() {
    let e = RscnEntity {
        has_light: true,
        light_type: 0, // directional
        light_color: [1.0, 1.0, 1.0],
        light_intensity: 3.0,
        ..simple_entity("Sun", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let dl = world.get::<DirectionalLight>(inst.all_entities[0]);
    assert!(dl.is_some(), "entity should have DirectionalLight");
    assert_eq!(dl.unwrap().color, [1.0, 1.0, 1.0].into());
}

#[test]
fn spawn_with_point_light() {
    let e = RscnEntity {
        has_light: true,
        light_type: 1, // point
        light_color: [1.0, 0.0, 0.0],
        light_intensity: 500.0,
        light_range: 30.0,
        ..simple_entity("Point", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let pl = world.get::<PointLight>(inst.all_entities[0]);
    assert!(pl.is_some());
    assert_eq!(pl.unwrap().range, 30.0);
}

#[test]
fn spawn_with_spot_light() {
    let e = RscnEntity {
        has_light: true,
        light_type: 2, // spot
        light_color: [0.9, 0.9, 1.0],
        light_intensity: 200.0,
        light_range: 50.0,
        light_inner_cone: 0.2,
        light_outer_cone: 0.5,
        ..simple_entity("Spot", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let sl = world.get::<SpotLight>(inst.all_entities[0]);
    assert!(sl.is_some());
    assert_eq!(sl.unwrap().inner_cone_angle, 0.2);
}

#[test]
fn spawn_with_camera() {
    let e = RscnEntity {
        has_camera: true,
        camera_fov: 75.0,
        camera_near: 0.01,
        camera_far: 500.0,
        ..simple_entity("Cam", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let cam = world.get::<Camera>(inst.all_entities[0]);
    assert!(cam.is_some());
    assert_eq!(cam.unwrap().fov_y_degrees, 75.0);
}

#[test]
fn spawn_multiple_roots() {
    let r1 = simple_entity("R1", None);
    let r2 = simple_entity("R2", None);
    let bytes = make_rscn(&[r1, r2]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    assert_eq!(inst.root_entities.len(), 2);
    assert_eq!(inst.all_entities.len(), 2);
}

#[test]
fn spawn_rejects_dead_parent() {
    // 实体 with parent 索引 beyond the 实体 数组
    // This is a malformed scene — the loader checks bounds.
    let child = RscnEntity {
        parent: Some(999),
        ..simple_entity("Orphan", None)
    };
    let bytes = make_rscn(&[child]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    // The orphan 实体 should exist but have no Parent 分量
    assert_eq!(inst.all_entities.len(), 1);
    assert!(world.get::<Parent>(inst.all_entities[0]).is_none());
    // It should be counted as a root since it has no Parent.
    assert_eq!(inst.root_entities.len(), 1);
}

#[test]
fn spawn_camera_emits_renderer_and_data_components() {
    // 相机 at [1,2,3], identity 旋转 (looks 下 −Z), 60° 视场角
    let e = RscnEntity {
        position: [1.0, 2.0, 3.0],
        has_camera: true,
        camera_fov: 60.0,
        camera_near: 0.1,
        camera_far: 1000.0,
        ..simple_entity("Cam", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let sid = SceneAssetId::generate();
    let inst = loader.spawn_from_parsed(&mut world, &parsed, sid).unwrap();

    let entity = inst.all_entities[0];

    // Data 分量 读取 by scene::systems::camera::collect_camera).
    let data = world
        .get::<Camera>(entity)
        .expect("scene::components::Camera should be present");
    assert_eq!(data.fov_y_degrees, 60.0);
    assert_eq!(data.near, 0.1);
    assert_eq!(data.far, 1000.0);

    // Free-fly controller (yaw/pitch derived from the 实体 四元数
    // Position lives on the sibling LocalTransform, not on the controller.
    let ctrl = world
        .get::<FlyCameraController>(entity)
        .expect("FlyCameraController should be present");
    // Identity 四元数 -> 向前 (0,0,-1) -> yaw=0, pitch=0.
    assert!(ctrl.yaw.abs() < 1e-5, "yaw={}", ctrl.yaw);
    assert!(ctrl.pitch.abs() < 1e-5);

    // Position is the LocalTransform 平移
    let lt = world
        .get::<LocalTransform>(entity)
        .expect("LocalTransform should be present");
    assert_eq!(lt.position, [1.0, 2.0, 3.0].into());
}

#[test]
fn spawn_camera_yaw_from_quaternion() {
    // 90° 旋转 about +Y: 四元数 (0, sin45, 0, cos45). A −Z 向前
    // rotated +90° about Y points to −X, which FlyCamera expresses as
    // yaw=π/2 (向前 = [-sin(π/2), 0, -cos(π/2)] = [-1, 0, 0]).
    let sqrt2_inv = std::f32::consts::FRAC_PI_4.sin(); // sin(45°)
    let e = RscnEntity {
        rotation: [0.0, sqrt2_inv, 0.0, sqrt2_inv],
        has_camera: true,
        camera_fov: 60.0,
        camera_near: 0.1,
        camera_far: 1000.0,
        ..simple_entity("CamYaw", None)
    };
    let bytes = make_rscn(&[e]);
    let parsed = parse_rscn(&bytes).unwrap();

    let mut world = World::new();
    let loader = SceneLoader::new();
    let inst = loader
        .spawn_from_parsed(&mut world, &parsed, SceneAssetId::generate())
        .unwrap();

    let ctrl = world
        .get::<FlyCameraController>(inst.all_entities[0])
        .expect("FlyCameraController should be present");
    // `atan2` 只给出 ±π 之间的值；这个朝向的 yaw 就是 π/2。
    assert!(
        (ctrl.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
        "yaw={}",
        ctrl.yaw
    );
    assert!(ctrl.pitch.abs() < 1e-5);
}

// ── RSCN v4 ─────────────────────────────────────────────────────

#[test]
fn load_rscn_v4_applies_components_and_hierarchy() {
    let json = br#"{
            "version": 1,
            "entities": [
                {
                    "name": "Root",
                    "parent": null,
                    "transform": {},
                    "components": {
                        "prism_engine::scene::Camera": { "fov_y_degrees": 70.0 }
                    }
                },
                {
                    "name": "Child",
                    "parent": 0,
                    "transform": { "position": [1.0, 2.0, 3.0] },
                    "components": {
                        "prism_engine::scene::PointLight": {
                            "color": [0.25, 0.5, 1.0],
                            "intensity": 42.0,
                            "range": 9.0
                        }
                    }
                },
                {
                    "name": "Env",
                    "parent": null,
                    "transform": {},
                    "components": {
                        "prism_engine::scene::Skybox": {
                            "hdr_path": "../env/test.hdr",
                            "enabled": true
                        }
                    }
                }
            ]
        }"#;
    // 走迁移漏斗：v1 文档没有实体 id，直接 `from_slice` 会按设计报错。
    let scene = prism_scene::SceneJson::parse(std::str::from_utf8(json).unwrap()).unwrap();
    let mut registry = crate::scene::ComponentRegistry::new();
    crate::scene::register_builtin_components(&mut registry);
    let bytes = prism_scene::rscn::encode_scene(&scene, &registry).unwrap();

    let mut world = World::new();
    let mut loader = SceneLoader::new();
    let inst = loader
        .load_and_spawn(&mut world, SceneSource::RawCooked(bytes.clone()), &registry)
        .unwrap();

    assert_eq!(inst.all_entities.len(), 3);
    assert_eq!(inst.root_entities.len(), 2);

    // RSCN stores entities parent-first, but sibling/root order is not the
    // authoring order. Resolve by name instead of assuming indices.
    let find = |name: &str| {
        *inst
            .all_entities
            .iter()
            .find(|&&entity| {
                world
                    .get::<Name>(entity)
                    .is_some_and(|component| component.0 == name)
            })
            .unwrap_or_else(|| panic!("missing entity {name}"))
    };
    let root = find("Root");
    let child = find("Child");
    let env = find("Env");

    assert_eq!(world.get::<Camera>(root).unwrap().fov_y_degrees, 70.0);
    assert!(world.get::<FlyCameraController>(root).is_some());

    assert_eq!(world.get::<PointLight>(child).unwrap().intensity, 42.0);
    assert_eq!(
        world.get::<LocalTransform>(child).unwrap().position,
        glam::Vec3::new(1.0, 2.0, 3.0)
    );
    assert_eq!(world.get::<Parent>(child).unwrap().0, root);
    assert_eq!(
        world.get::<Skybox>(env).unwrap().hdr_path,
        "../env/test.hdr"
    );

    assert_eq!(
        read_env_path_from_rscn_bytes(&bytes).as_deref(),
        Some("../env/test.hdr")
    );
}

/// 有网格、无材质的 `MeshRenderer`（编辑器 fixture 的常见形状）必须仍然
/// 进入绘制列表：缺 `MaterialRef` 时由渲染器回落默认材质槽。
#[test]
fn load_rscn_v4_mesh_without_material_still_draws() {
    let json = br#"{
            "version": 1,
            "entities": [
                {
                    "name": "MeshOnly",
                    "parent": null,
                    "transform": {},
                    "components": {
                        "prism_engine::scene::MeshRenderer": {
                            "mesh_path": "models/box.gltf",
                            "material_path": ""
                        }
                    }
                }
            ]
        }"#;
    let scene = prism_scene::SceneJson::parse(std::str::from_utf8(json).unwrap()).unwrap();
    let mut registry = crate::scene::ComponentRegistry::new();
    crate::scene::register_builtin_components(&mut registry);
    let bytes = prism_scene::rscn::encode_scene(&scene, &registry).unwrap();

    let mut world = World::new();
    let mut loader = SceneLoader::new();
    loader
        .load_and_spawn(&mut world, SceneSource::RawCooked(bytes), &registry)
        .unwrap();

    let entity = world
        .query::<MeshRef>()
        .next()
        .map(|(entity, _)| entity)
        .expect("mesh-only entity should get a MeshRef");
    assert!(
        world.get::<MaterialRef>(entity).is_none(),
        "空材质路径不应插入 MaterialRef"
    );

    let items = crate::scene::SceneReadView::new(&world).draw_items();
    assert_eq!(items.len(), 1, "网格-only 实体应进入绘制列表");
    assert_eq!(items[0].material, None, "缺失材质应回落到默认材质槽");
}

#[test]
fn load_json_mesh_without_material_derives_mesh_ref() {
    let json = r#"{
            "version": 3,
            "entities": [{
                "id": 1,
                "name": "MeshOnly",
                "parent": null,
                "transform": {},
                "components": {
                    "prism_engine::scene::MeshRenderer": {
                        "mesh_path": "__builtin/box",
                        "material_path": ""
                    }
                }
            }]
        }"#;

    let mut registry = crate::scene::ComponentRegistry::new();
    crate::scene::register_builtin_components(&mut registry);
    let mut world = World::new();
    let loader = SceneLoader::new();
    loader
        .spawn_from_json_text(
            &mut world,
            json,
            "mesh-only.scene.json",
            SceneAssetId::generate(),
            &registry,
        )
        .expect("json scene should load");

    let entity = world
        .query::<MeshRef>()
        .next()
        .map(|(entity, _)| entity)
        .expect("json mesh entity should derive MeshRef");
    assert!(world.get::<MaterialRef>(entity).is_none());
    let items = crate::scene::SceneReadView::new(&world).draw_items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].material, None);
}

// ── integration via load_and_spawn ──────────────────────────────

#[test]
fn load_from_raw_cooked() {
    let e = simple_entity("E", None);
    let bytes = make_rscn(&[e]);

    let mut world = World::new();
    let mut loader = SceneLoader::new();
    let registry = crate::scene::ComponentRegistry::new();
    let inst = loader
        .load_and_spawn(&mut world, SceneSource::RawCooked(bytes), &registry)
        .unwrap();

    assert_eq!(inst.all_entities.len(), 1);
    assert_eq!(inst.root_entities.len(), 1);
}

/// Smoke-test the engine-builtin 默认 scene committed at
/// `assets/scenes/default.rscn`. Ignored by 默认 because it depends on
/// the repo working-tree 布局 (run from the repo root); run with:
///   `cargo test -p prism-engine load_committed_default_rscn -- --ignored --nocapture`
/// Guards against the cooked scene drifting out of sync with the loader.
#[test]
#[ignore]
fn load_committed_default_rscn() {
    // 搜索 both the repo root and the crate dir so the test works
    // regardless of which directory `cargo test` was invoked from.
    let candidates = [
        std::path::PathBuf::from("assets/scenes/default.rscn"),
        std::path::PathBuf::from("../../assets/scenes/default.rscn"),
    ];
    let path = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone());
    if !path.exists() {
        eprintln!("skipping: {} not found (cwd mismatch)", path.display());
        return;
    }
    let mut world = World::new();
    let mut loader = SceneLoader::new();
    let registry = crate::scene::ComponentRegistry::new();
    let inst = loader
        .load_and_spawn(&mut world, SceneSource::CookedFile(path.into()), &registry)
        .expect("default.rscn should parse");

    // 6 entities: 1 skybox + 1 相机 + 1 directional 光源 + 3 point lights.
    assert_eq!(inst.all_entities.len(), 6);

    // Exactly one 相机 实体 with a FlyCameraController + 相机 data
    // 分量 + LocalTransform (position lives on the 变换
    let cameras: Vec<_> = world.query::<Camera>().collect();
    assert_eq!(cameras.len(), 1, "expected exactly one camera");

    // The 相机 should be positioned at [0, 2.5, 18] (per default.scene.json).
    let cam_entity = cameras[0].0;
    let lt = world
        .get::<LocalTransform>(cam_entity)
        .expect("camera should have a LocalTransform");
    assert_eq!(lt.position, [0.0, 2.5, 18.0].into());
    // And it should carry a free-fly controller.
    assert!(world.get::<FlyCameraController>(cam_entity).is_some());
}

// ── `.scene.json` parent = entity id (not array index) ─────────────

/// 把一个 `.scene.json` 文本直接铺进 world，返回 (实例, 世界)。
fn spawn_json_text(json: &str) -> (SceneInstance, World) {
    let mut registry = crate::scene::ComponentRegistry::new();
    crate::scene::register_builtin_components(&mut registry);
    let mut world = World::new();
    let loader = SceneLoader::new();
    let inst = loader
        .spawn_from_json_text(
            &mut world,
            json,
            "<test>",
            SceneAssetId::generate(),
            &registry,
        )
        .unwrap();
    (inst, world)
}

#[test]
fn json_parent_ids_are_resolved_not_used_as_array_indices() {
    // 数组顺序是 [child, root]，但 parent 引用的是 id 9（位于下标 1）。
    let json = r#"{
        "version": 3,
        "entities": [
            { "id": 5, "name": "Child", "parent": 9, "transform": {}, "components": {} },
            { "id": 9, "name": "Root", "parent": null, "transform": {}, "components": {} }
        ]
    }"#;

    let (inst, world) = spawn_json_text(json);
    assert_eq!(inst.all_entities.len(), 2);

    let child = inst.all_entities[0];
    let root = inst.all_entities[1];

    let parent = world
        .get::<Parent>(child)
        .expect("child 应挂到 root 下（parent 是 id，不是下标）");
    assert_eq!(parent.0, root);
    assert!(world.get::<Parent>(root).is_none());
    assert_eq!(inst.root_entities, vec![root]);
}

#[test]
fn json_parent_chain_follows_ids_after_reordering() {
    // root(3) ← mid(7) ← leaf(1)：三层按 id 串起来，与数组顺序无关。
    let json = r#"{
        "version": 3,
        "entities": [
            { "id": 1, "name": "Leaf", "parent": 7, "transform": {}, "components": {} },
            { "id": 7, "name": "Mid", "parent": 3, "transform": {}, "components": {} },
            { "id": 3, "name": "Root", "parent": null, "transform": {}, "components": {} }
        ]
    }"#;

    let (inst, world) = spawn_json_text(json);
    let (leaf, mid, root) = (
        inst.all_entities[0],
        inst.all_entities[1],
        inst.all_entities[2],
    );

    assert_eq!(world.get::<Parent>(leaf).unwrap().0, mid);
    assert_eq!(world.get::<Parent>(mid).unwrap().0, root);
    assert!(world.get::<Parent>(root).is_none());
    assert_eq!(inst.root_entities, vec![root]);
}

#[test]
fn json_dangling_parent_id_keeps_the_entity_as_a_root() {
    let json = r#"{
        "version": 3,
        "entities": [
            { "id": 4, "name": "Orphan", "parent": 42, "transform": {}, "components": {} }
        ]
    }"#;

    let (inst, world) = spawn_json_text(json);
    let entity = inst.all_entities[0];
    assert!(world.get::<Parent>(entity).is_none());
    assert_eq!(inst.root_entities, vec![entity]);
}
