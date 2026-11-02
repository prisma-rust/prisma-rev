//! Source → import → cook → `.pak` → runtime round-trip scenarios.

mod support;

use prism_asset::core::AssetType;
use prism_asset::formats::RTEX_FORMAT_RGBA8;
use prism_asset::runtime::ResourceManager;
use support::{mesh_bounds, texture_pixel, AssetHarness};

const SCENE_V1: &str = r#"{
    "version": 3,
    "entities": [
        {
            "id": 1,
            "name": "Root",
            "parent": null,
            "transform": {
                "position": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0]
            },
            "components": {}
        },
        {
            "id": 2,
            "name": "Child",
            "parent": 1,
            "transform": {
                "position": [1.0, 2.0, 3.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0]
            },
            "components": {
                "prism_engine::scene::Active": true
            }
        }
    ]
}"#;

const SCENE_V2: &str = r#"{
    "version": 3,
    "entities": [
        {
            "id": 1,
            "name": "Root",
            "parent": null,
            "transform": {
                "position": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0]
            },
            "components": {}
        },
        {
            "id": 2,
            "name": "Child",
            "parent": 1,
            "transform": {
                "position": [4.0, 5.0, 6.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0]
            },
            "components": {
                "prism_engine::scene::Active": true
            }
        },
        {
            "id": 3,
            "name": "Extra",
            "parent": null,
            "transform": {
                "position": [7.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0]
            },
            "components": {}
        }
    ]
}"#;

const BAD_SCENE: &str = r#"{
    "version": 3,
    "entities": [
        {
            "id": 1,
            "name": "Bad",
            "parent": null,
            "transform": {
                "position": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0]
            },
            "components": {
                "not::a::RealComponent": {}
            }
        }
    ]
}"#;

const PREFAB: &str = r#"{
    "version": 3,
    "entities": [
        {
            "id": 1,
            "name": "Crate",
            "parent": null,
            "transform": {
                "position": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0]
            },
            "components": {}
        }
    ]
}"#;

const TRIANGLE_OBJ: &str = r#"v 0 0 0
v 1 0 0
v 0 1 0
vt 0 0
vt 1 0
vt 0 1
vn 0 0 1
f 1/1/1 2/2/1 3/3/1
"#;

fn write_project(harness: &mut AssetHarness) {
    harness.write_source("meshes/triangle.obj", TRIANGLE_OBJ.as_bytes());
    harness.write_source("prefabs/crate.prefab.json", PREFAB.as_bytes());
    harness.write_png(
        "textures/checker.png",
        2,
        2,
        &[
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ],
    );
}

fn assert_scene_v1(scene: &prism_scene::rscn::DecodedScene) {
    assert_eq!(scene.entities.len(), 2);
    assert_eq!(scene.entities[0].name, "Root");
    let child = scene
        .entities
        .iter()
        .find(|entity| entity.name == "Child")
        .expect("Child entity");
    assert_eq!(child.parent, Some(0));
    assert_eq!(child.position, [1.0, 2.0, 3.0]);
    assert!(
        child
            .components
            .iter()
            .any(|component| component.type_name == "prism_engine::scene::Active"),
        "Active component should survive the cook: {:?}",
        child.components
    );
}

#[test]
fn scene_mesh_texture_and_prefab_round_trip_through_pak() {
    let mut harness = AssetHarness::new();
    write_project(&mut harness);

    let scene_id = harness
        .stage_scene("scenes/main.scene.json", SCENE_V1)
        .expect("stage scene");
    let mesh_id = harness.import("meshes/triangle.obj").expect("import mesh");
    let texture_id = harness
        .import("textures/checker.png")
        .expect("import texture");
    let prefab_id = harness
        .import("prefabs/crate.prefab.json")
        .expect("import prefab");

    let report = harness.cook().expect("cook project");
    assert_eq!(report.cooked, 4);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.streamed, 0);
    assert_eq!(report.pak_entries, 4);
    assert!(report.pak_bytes > 0);

    let record = harness.package_record("meshes/triangle.obj").unwrap();
    assert_eq!(record.id, mesh_id.0);
    assert_eq!(record.type_id, AssetType::Mesh.to_u32());
    assert_eq!(
        harness
            .package_record("textures/checker.png")
            .unwrap()
            .type_id,
        AssetType::Texture.to_u32()
    );
    assert_eq!(
        harness
            .package_record("scenes/main.scene.json")
            .unwrap()
            .type_id,
        AssetType::Scene.to_u32()
    );
    let _ = (scene_id, texture_id, prefab_id);

    let mut manager = harness.open_pak().expect("open pak");
    let scene = harness
        .load_scene(&mut manager, "scenes/main.scene.json")
        .expect("load scene");
    assert_scene_v1(&scene);

    let mesh = harness
        .load_mesh(&mut manager, "meshes/triangle.obj")
        .expect("load mesh");
    assert_eq!(mesh.info.vert_count, 3);
    assert_eq!(mesh.info.idx_count, 3);
    let (min, max) = mesh_bounds(&mesh.info).expect("mesh bounds");
    assert_eq!(min, [0.0, 0.0, 0.0]);
    assert_eq!(max, [1.0, 1.0, 0.0]);

    let texture = harness
        .load_texture(&mut manager, "textures/checker.png")
        .expect("load texture");
    assert_eq!(texture.info.width, 2);
    assert_eq!(texture.info.height, 2);
    assert_eq!(texture.info.mip_levels, 2);
    assert_eq!(texture.info.format, RTEX_FORMAT_RGBA8);
    assert_eq!(texture_pixel(&texture, 0, 0), [255, 0, 0, 255]);
    assert_eq!(texture_pixel(&texture, 1, 0), [0, 255, 0, 255]);

    let prefab = harness
        .load_raw(&mut manager, "prefabs/crate.prefab.json")
        .expect("load prefab bytes");
    assert_eq!(prefab, PREFAB.as_bytes());
}

#[test]
fn missing_source_and_truncated_texture_report_actionable_errors() {
    let mut harness = AssetHarness::new();

    let missing = harness.import("meshes/missing.obj").unwrap_err();
    assert!(
        missing.contains("import meshes/missing.obj"),
        "error must name the source: {missing}"
    );
    // 只认 `os error <n>` 这一段：它由 Rust 的 `io::Error` 固定拼出来，而前半句是
    // **系统本地化**的（中文 Windows 上缺目录是 `os error 3`，不是 2）。断言具体
    // 英文/错误码会让同一个代码路径在不同语言 / 平台的机器上假红。
    assert!(
        missing.contains("os error"),
        "error must name the OS failure: {missing}"
    );

    harness.write_source("textures/broken.png", b"\x89PNG\r\n\x1a\nnot a real png");
    let broken = harness.import("textures/broken.png").unwrap_err();
    assert!(
        broken.contains("import textures/broken.png"),
        "error must name the source: {broken}"
    );
    assert!(
        broken.contains("Failed to decode image"),
        "error must come from the texture importer: {broken}"
    );

    harness
        .stage_scene("scenes/bad.scene.json", BAD_SCENE)
        .expect("stage bad scene");
    let bad_scene = harness.cook().unwrap_err();
    assert!(
        bad_scene.contains("cook:"),
        "error must come from the cook stage: {bad_scene}"
    );
    assert!(
        bad_scene.contains("not::a::RealComponent"),
        "error must name the unknown component: {bad_scene}"
    );
}

#[test]
fn truncated_pak_is_rejected_by_the_runtime_loader() {
    let mut harness = AssetHarness::new();
    write_project(&mut harness);
    harness
        .stage_scene("scenes/main.scene.json", SCENE_V1)
        .unwrap();
    harness.import("meshes/triangle.obj").unwrap();
    harness.import("textures/checker.png").unwrap();
    let report = harness.cook().expect("cook project");
    assert!(report.pak_bytes > 32);

    let bytes = harness.pak_bytes();
    let mut manager = ResourceManager::new();
    let error = manager
        .load_package_bytes(&bytes[..bytes.len() / 2])
        .expect_err("truncated pak must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("truncated") || message.contains("checksum"),
        "loader must explain the truncation: {message}"
    );
}

#[test]
fn recooking_after_a_component_change_updates_the_loaded_scene() {
    let mut harness = AssetHarness::new();
    harness
        .stage_scene("scenes/main.scene.json", SCENE_V1)
        .expect("stage v1");
    harness.cook().expect("cook v1");

    let mut manager = harness.open_pak().expect("open v1 pak");
    let first = harness
        .load_scene(&mut manager, "scenes/main.scene.json")
        .expect("load v1");
    assert_scene_v1(&first);

    harness
        .recook_scene("scenes/main.scene.json", SCENE_V2)
        .expect("stage v2");
    let report = harness.cook().expect("cook v2");
    assert_eq!(report.cooked, 1);

    let mut manager = harness.open_pak().expect("open v2 pak");
    let second = harness
        .load_scene(&mut manager, "scenes/main.scene.json")
        .expect("load v2");
    assert_eq!(second.entities.len(), 3);
    let child = second
        .entities
        .iter()
        .find(|entity| entity.name == "Child")
        .expect("Child entity");
    assert_eq!(child.position, [4.0, 5.0, 6.0]);
    assert!(
        second.entities.iter().any(|entity| entity.name == "Extra"),
        "new entity must appear after recooking: {:?}",
        second.entities.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
}
