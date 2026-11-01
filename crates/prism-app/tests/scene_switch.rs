//! 换场语义的端到端测试（#482）：`App::load_scene_json_file` 复用同一个
//! `Engine`——旧场景卸载、`Persistent` 实体保留、不会出现「所有东西两份」。
//!
//! 这些用例不启动渲染线程（`start_hosted` 之前 `render_shared` 为 `None`），
//! 所以没有 GPU 也能跑。

use std::path::{Path, PathBuf};

use prism_app::App;

fn write_scene(dir: &Path, name: &str, entity_name: &str) -> PathBuf {
    let path = dir.join(format!("{name}.scene.json"));
    let json = format!(
        r#"{{
  "version": 3,
  "entities": [
    {{
      "name": "Main Camera",
      "parent": null,
      "transform": {{ "position": [0.0, 1.0, 5.0] }},
      "components": {{ "prism_engine::scene::Camera": {{}} }}
    }},
    {{
      "name": "{entity_name}",
      "parent": null,
      "transform": {{}},
      "components": {{}}
    }}
  ]
}}"#
    );
    std::fs::write(&path, json).expect("write scene");
    path
}

fn named_entities(app: &mut App) -> Vec<String> {
    app.engine_mut()
        .world()
        .query::<prism_engine::scene::components::Name>()
        .map(|(_, name)| name.0.clone())
        .collect()
}

#[test]
fn switching_scenes_unloads_the_previous_one_instead_of_duplicating_it() {
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = std::env::temp_dir().join(format!("prism-switch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let first = write_scene(&dir, "first", "First Thing");
    let second = write_scene(&dir, "second", "Second Thing");

    let mut app = App::hosted(prism_engine::config::AppConfig::default());
    app.load_scene_json_file(&first).expect("load first");
    let mut names = named_entities(&mut app);
    names.sort();
    assert_eq!(names, vec!["First Thing", "Main Camera"]);

    app.load_scene_json_file(&second).expect("load second");
    let mut names = named_entities(&mut app);
    names.sort();
    assert_eq!(
        names,
        vec!["Main Camera", "Second Thing"],
        "换场必须卸载旧场景，而不是把两份都留着"
    );

    // 同一个场景再加载一次也不重复；engine 还是同一个实例。
    let engine_before = app.engine_mut().scene_generation();
    app.load_scene_json_file(&second).expect("reload second");
    let mut names = named_entities(&mut app);
    names.sort();
    assert_eq!(names, vec!["Main Camera", "Second Thing"]);
    assert!(
        app.engine_mut().scene_generation() > engine_before,
        "每次换场 / 卸载都要推进场景代数"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn persistent_entities_survive_a_scene_switch() {
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = std::env::temp_dir().join(format!("prism-persist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let first = write_scene(&dir, "a", "Scene A");
    let second = write_scene(&dir, "b", "Scene B");

    let mut app = App::hosted(prism_engine::config::AppConfig::default());
    app.load_scene_json_file(&first).expect("load first");

    // 把第一场的实体标成 `Persistent`（游戏侧等价 `DontDestroyOnLoad`）。
    let persistent = {
        let world = app.engine_mut().world_mut();
        let entity = world
            .query::<prism_engine::scene::components::Name>()
            .find(|(_, name)| name.0 == "Scene A")
            .map(|(entity, _)| entity)
            .expect("Scene A entity");
        world.insert(entity, prism_engine::scene::components::Persistent);
        entity
    };

    app.load_scene_json_file(&second).expect("load second");

    let mut names = named_entities(&mut app);
    names.sort();
    assert_eq!(
        names,
        vec!["Main Camera", "Scene A", "Scene B"],
        "Persistent 实体必须活过换场"
    );
    // 同一个实体句柄仍然有效（engine 没有被重建）。
    assert!(
        app.engine_mut()
            .world()
            .get::<prism_engine::scene::components::Name>(persistent)
            .is_some(),
        "复用引擎时 Persistent 实体的句柄不应失效"
    );

    std::fs::remove_dir_all(&dir).ok();
}
