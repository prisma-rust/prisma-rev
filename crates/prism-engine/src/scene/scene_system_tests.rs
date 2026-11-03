//! `SceneManager` 的场景切换语义：成功时卸载旧场景、清空环境缓存；
//! 失败时保持当前场景不动。

use super::*;
use crate::scene::components::{SceneAssetId, SceneMember};

/// spawn 一个属于 `scene` 的实体（最小复刻 loader 写入的归属标记）。
fn spawn_scene_entity(world: &mut World, scene: SceneAssetId) -> Entity {
    let entity = world.spawn();
    world.insert(entity, SceneMember(scene));
    entity
}

fn instance(scene_id: SceneAssetId, all_entities: Vec<Entity>) -> SceneInstance {
    SceneInstance {
        scene_id,
        root_entities: all_entities.clone(),
        all_entities,
    }
}

#[test]
fn switching_scenes_unloads_the_previous_scene() {
    let mut world = World::new();
    let mut manager = SceneManager::new();
    let previous = SceneAssetId::generate();
    let current = SceneAssetId::generate();

    let previous_entities: Vec<Entity> = (0..3)
        .map(|_| spawn_scene_entity(&mut world, previous))
        .collect();
    // 引擎自有实体（例如 `runtime_initialize` 的回退相机）没有 `SceneMember`，
    // 切场景时不能被误删。
    let engine_owned = world.spawn();
    let current_entities: Vec<Entity> = (0..2)
        .map(|_| spawn_scene_entity(&mut world, current))
        .collect();

    let unloaded = manager.adopt_scene(
        &mut world,
        "second",
        "scenes/second.rscn",
        &instance(current, current_entities.clone()),
    );

    assert_eq!(unloaded, previous_entities.len());
    assert!(previous_entities
        .iter()
        .all(|entity| !world.is_alive(*entity)));
    assert!(current_entities
        .iter()
        .all(|entity| world.is_alive(*entity)));
    assert!(
        world.is_alive(engine_owned),
        "entities without SceneMember are owned by the engine, not the scene"
    );
    assert_eq!(manager.current_scene_name(), Some("second"));
    assert_eq!(
        manager.current_scene_path.as_deref(),
        Some("scenes/second.rscn")
    );
}

#[test]
fn switching_scenes_drops_the_previous_environment_bytes() {
    let mut world = World::new();
    let mut manager = SceneManager::new();
    manager.set_environment_bytes(Some(vec![1, 2, 3]));

    manager.adopt_scene(
        &mut world,
        "next",
        "scenes/next.rscn",
        &instance(SceneAssetId::generate(), Vec::new()),
    );

    assert!(
        manager.environment_bytes.is_none(),
        "IBL must be re-resolved from the new scene instead of reusing the old HDR"
    );
}

#[test]
fn a_missing_scene_keeps_the_current_scene_loaded() {
    let mut world = World::new();
    let mut rm = ResourceManager::new();
    let registry = ComponentRegistry::new();
    let mut manager = SceneManager::new();
    manager.current_scene_name = Some("intro".to_owned());
    manager.current_scene_path = Some("scenes/intro.rscn".to_owned());
    manager.set_environment_bytes(Some(vec![7]));
    let intro_entity = spawn_scene_entity(&mut world, SceneAssetId::generate());

    // manifest 里有这个场景，但资源包里没有注册对应路径 → 加载失败。
    let manifest = "\
[[scenes]]
name = \"missing\"
path = \"scenes/missing.rscn\"
";
    let loaded =
        manager.load_from_manifest_text(&mut rm, &mut world, &registry, manifest, Some("missing"));

    assert!(loaded.is_none());
    assert_eq!(manager.current_scene_name(), Some("intro"));
    assert_eq!(
        manager.current_scene_path.as_deref(),
        Some("scenes/intro.rscn")
    );
    assert_eq!(manager.environment_bytes.as_deref(), Some(&[7u8][..]));
    assert!(world.is_alive(intro_entity));
}

#[test]
fn unloading_an_empty_world_is_a_no_op() {
    let mut world = World::new();
    let keep = SceneAssetId::generate();

    assert_eq!(unload_scene_members(&mut world, Some(keep)), 0);

    let entity = spawn_scene_entity(&mut world, keep);
    assert_eq!(unload_scene_members(&mut world, Some(keep)), 0);
    assert!(world.is_alive(entity));
}

// ---------------------------------------------------------------------------
// 清单解析：scenes.json 优先，旧 scenes.toml 回退
// ---------------------------------------------------------------------------

#[test]
fn parses_the_json_manifest() {
    let manifest = SceneManifest::parse(
        r#"{"scenes":[{"name":"intro","path":"scenes/intro.rscn","enabled":true},
                      {"name":"draft","path":"scenes/draft.rscn","enabled":false}]}"#,
    )
    .expect("json manifest");
    assert_eq!(manifest.scenes.len(), 2);
    assert_eq!(manifest.scenes[0].name, "intro");
    assert!(manifest.scenes[0].enabled);
    assert!(!manifest.scenes[1].enabled);
}

#[test]
fn json_manifest_defaults_enabled_to_true() {
    let manifest =
        SceneManifest::parse(r#"{"scenes":[{"name":"a","path":"scenes/a.rscn"}]}"#).unwrap();
    assert!(manifest.scenes[0].enabled);
}

#[test]
fn json_manifest_preserves_scene_guid() {
    let manifest = SceneManifest::parse(
        r#"{"scenes":[{"name":"a","path":"scenes/a.rscn",
                       "guid":"11111111111111111111111111111111"}]}"#,
    )
    .unwrap();
    assert_eq!(
        manifest.scenes[0].guid.as_deref(),
        Some("11111111111111111111111111111111")
    );
}

#[test]
fn manifest_guid_resolves_to_the_source_scene_path() {
    let temp = tempfile::tempdir().unwrap();
    let assets = temp.path().join("assets");
    std::fs::create_dir_all(assets.join("scenes")).unwrap();
    std::fs::write(assets.join("scenes/renamed.scene.json"), b"{}").unwrap();
    let meta =
        prism_asset::AssetMeta::ensure(&assets, "scenes/renamed.scene.json", "SceneImporter")
            .unwrap();

    let mut manifest = SceneManifest::parse(&format!(
        r#"{{"scenes":[{{"name":"a","path":"scenes/old.rscn","guid":"{}"}}]}}"#,
        meta.guid
    ))
    .unwrap();
    resolve_manifest_guids(&mut manifest, &assets);
    assert_eq!(manifest.scenes[0].path, "scenes/renamed.rscn");
}

#[test]
fn falls_back_to_the_legacy_toml_manifest() {
    let manifest = SceneManifest::parse(
        "# hand written\n[[scenes]]\nname = \"intro\"\npath = \"scenes/intro.rscn\"\n\
         [[scenes]]\nname = \"off\"\npath = \"scenes/off.rscn\"\nenabled = false\n",
    )
    .expect("legacy manifest");
    assert_eq!(manifest.scenes.len(), 2);
    assert_eq!(manifest.scenes[0].name, "intro");
    assert!(manifest.scenes[0].enabled, "缺省 enabled 视为启用");
    assert!(!manifest.scenes[1].enabled);
}

#[test]
fn garbage_manifest_is_reported_instead_of_silently_empty() {
    assert!(SceneManifest::parse("scenes = 3\n").is_err());
    assert!(SceneManifest::parse("{ not json").is_err());
}

// ---------------------------------------------------------------------------
// 场景卸载（#482）
// ---------------------------------------------------------------------------

/// `Persistent` 实体 + 它的整棵子树都活过一次换场。
#[test]
fn unloading_a_scene_keeps_persistent_subtrees() {
    use crate::scene::components::{Child, Persistent};
    use prism_scene::HierarchyHelper;

    let mut world = World::new();
    let mut manager = SceneManager::new();
    let scene = SceneAssetId::generate();

    let level_entity = spawn_scene_entity(&mut world, scene);
    let persistent_root = spawn_scene_entity(&mut world, scene);
    world.insert(persistent_root, Persistent);
    let child = spawn_scene_entity(&mut world, scene);
    let grandchild = spawn_scene_entity(&mut world, scene);
    HierarchyHelper::reparent(&mut world, child, Some(persistent_root));
    HierarchyHelper::reparent(&mut world, grandchild, Some(child));

    // 场景里一个普通的中间节点，用来确认树结构没被整体保留。
    assert!(world.get::<Child>(persistent_root).is_some());

    let unloaded = manager.unload_scene(&mut world);

    assert_eq!(unloaded, 1, "只有不在 Persistent 子树里的那个实体被删");
    assert!(!world.is_alive(level_entity));
    assert!(world.is_alive(persistent_root));
    assert!(world.is_alive(child), "Persistent 的后代跟着一起留下");
    assert!(world.is_alive(grandchild));
    assert_eq!(manager.current_scene_name(), None);
    assert!(manager.generation() >= 1, "卸载必须递增场景代数");
}

/// 卸载之后场景代数要变：项目模块靠它丢弃陈旧句柄。
#[test]
fn every_scene_switch_bumps_the_generation() {
    let mut world = World::new();
    let mut manager = SceneManager::new();
    assert_eq!(manager.generation(), 0);

    let first = SceneAssetId::generate();
    let first_entities: Vec<Entity> = (0..2)
        .map(|_| spawn_scene_entity(&mut world, first))
        .collect();
    manager.adopt_scene(
        &mut world,
        "first",
        "scenes/first.rscn",
        &instance(first, first_entities),
    );
    let after_first = manager.generation();
    assert_eq!(after_first, 1);

    let second = SceneAssetId::generate();
    let second_entities: Vec<Entity> = (0..1)
        .map(|_| spawn_scene_entity(&mut world, second))
        .collect();
    manager.adopt_scene(
        &mut world,
        "second",
        "scenes/second.rscn",
        &instance(second, second_entities),
    );
    assert_eq!(manager.generation(), after_first + 1);
}

/// 作者格式（`.scene.json`）的 `Single` 接管：旧场景被卸载而不是叠加。
#[test]
fn adopting_an_author_scene_switches_instead_of_stacking() {
    let mut world = World::new();
    let mut manager = SceneManager::new();
    let old = SceneAssetId::generate();
    let old_entities: Vec<Entity> = (0..3)
        .map(|_| spawn_scene_entity(&mut world, old))
        .collect();

    let new = SceneAssetId::generate();
    let new_entities: Vec<Entity> = (0..2)
        .map(|_| spawn_scene_entity(&mut world, new))
        .collect();

    let unloaded =
        manager.adopt_author_scene(&mut world, &instance(new, new_entities.clone()), false);

    assert_eq!(unloaded, old_entities.len());
    assert!(old_entities.iter().all(|entity| !world.is_alive(*entity)));
    assert!(new_entities.iter().all(|entity| world.is_alive(*entity)));
    assert_eq!(
        manager.current_scene_name(),
        None,
        "作者格式没有 manifest 名字，不能留着上一个场景的名字"
    );
    assert_eq!(manager.generation(), 1);
}

/// `Additive`：两边都留着，代数不变（旧句柄仍然有效）。
#[test]
fn additive_loads_keep_both_scenes() {
    let mut world = World::new();
    let mut manager = SceneManager::new();
    let first = SceneAssetId::generate();
    let first_entities: Vec<Entity> = (0..2)
        .map(|_| spawn_scene_entity(&mut world, first))
        .collect();
    let second = SceneAssetId::generate();
    let second_entities: Vec<Entity> = (0..2)
        .map(|_| spawn_scene_entity(&mut world, second))
        .collect();

    let unloaded =
        manager.adopt_author_scene(&mut world, &instance(second, second_entities.clone()), true);

    assert_eq!(unloaded, 0);
    assert!(first_entities.iter().all(|entity| world.is_alive(*entity)));
    assert!(second_entities.iter().all(|entity| world.is_alive(*entity)));
    assert_eq!(manager.generation(), 0);
}

/// 父级属于旧场景被删、子级是 `Persistent`：子级留下，父级断链（只警告）。
#[test]
fn a_persistent_child_survives_a_dead_parent() {
    use crate::scene::components::Persistent;
    use prism_scene::HierarchyHelper;

    let mut world = World::new();
    let mut manager = SceneManager::new();
    let scene = SceneAssetId::generate();
    let parent = spawn_scene_entity(&mut world, scene);
    let child = spawn_scene_entity(&mut world, scene);
    world.insert(child, Persistent);
    HierarchyHelper::reparent(&mut world, child, Some(parent));

    manager.unload_scene(&mut world);

    assert!(!world.is_alive(parent));
    assert!(world.is_alive(child));
}
