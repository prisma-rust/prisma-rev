//! End-to-end Workspace scenarios for the headless editor harness.

mod support;

use serde_json::Value;
use support::{EditorHarness, HEALTH};

fn entity_json<'a>(doc: &'a Value, name: &str) -> &'a Value {
    doc["entities"]
        .as_array()
        .expect("entities array")
        .iter()
        .find(|entity| entity["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("entity {name:?} not found on disk"))
}

#[test]
fn transform_edit_undo_and_redo_round_trip() {
    let mut harness = EditorHarness::new();
    let cube = harness.entity_index("Cube");
    assert_eq!(harness.transform(cube), [1.0, 2.0, 3.0]);
    assert!(!harness.workspace().document().is_dirty());

    harness.set_transform(cube, [7.0, 8.0, 9.0]);
    assert_eq!(harness.transform(cube), [7.0, 8.0, 9.0]);
    assert!(harness.workspace().document().is_dirty());
    assert!(harness.workspace().can_undo());

    harness.undo();
    assert_eq!(harness.transform(cube), [1.0, 2.0, 3.0]);
    assert!(harness.workspace().can_redo());

    harness.redo();
    assert_eq!(harness.transform(cube), [7.0, 8.0, 9.0]);
}

#[test]
fn multi_selection_component_add_remove_and_undo() {
    let mut harness = EditorHarness::new();
    let cube = harness.entity_index("Cube");
    let camera = harness.entity_index("Main Camera");
    harness.select(&[cube, camera]);
    assert_eq!(harness.workspace().selection.len(), 2);

    let added = harness
        .workspace_mut()
        .add_component_multi(&[cube, camera], HEALTH);
    assert_eq!(added, 2);
    assert_eq!(harness.health(cube), Some(10));
    assert_eq!(harness.health(camera), Some(10));

    harness.set_health(cube, 42);
    assert_eq!(harness.health(cube), Some(42));

    harness
        .workspace_mut()
        .remove_component_multi(&[cube, camera], HEALTH);
    assert_eq!(harness.health(cube), None);
    assert_eq!(harness.health(camera), None);

    harness.undo();
    assert_eq!(harness.health(cube), Some(42));
    assert_eq!(harness.health(camera), Some(10));

    harness.redo();
    assert_eq!(harness.health(cube), None);
    assert_eq!(harness.health(camera), None);
}

#[test]
fn save_reload_and_external_conflict_are_observable() {
    let mut harness = EditorHarness::new();
    let cube = harness.entity_index("Cube");
    harness.workspace_mut().add_component_multi(&[cube], HEALTH);
    harness.set_health(cube, 42);

    let scene_index = harness.workspace().active_index();
    let saved_path = harness.workspace_mut().save_active().expect("save");
    assert_eq!(saved_path, "assets/scenes/main.scene.json");
    assert!(!harness.workspace().document().is_dirty());
    assert_eq!(
        entity_json(&harness.disk_json(), "Cube")["components"][HEALTH]["hp"].as_i64(),
        Some(42)
    );

    harness
        .workspace_mut()
        .reload_document_from_disk(scene_index)
        .expect("reload");
    let cube = harness.entity_index("Cube");
    assert_eq!(harness.health(cube), Some(42));

    let mut external = harness.disk_json();
    let entities = external["entities"].as_array_mut().expect("entities");
    let cube_json = entities
        .iter_mut()
        .find(|entity| entity["name"].as_str() == Some("Cube"))
        .expect("Cube on disk");
    cube_json["components"][HEALTH]["hp"] = serde_json::json!(999);
    std::fs::write(
        harness.scene_path(),
        serde_json::to_string_pretty(&external).unwrap(),
    )
    .expect("write external change");

    assert_eq!(harness.workspace_mut().check_external_changes(), 1);
    let error = harness.workspace_mut().save_active().expect_err("conflict");
    assert!(error.to_string().contains("已被修改"), "{error}");
    assert_eq!(harness.workspace().pending_save_conflict, Some(scene_index));

    harness
        .workspace_mut()
        .reload_document_from_disk(scene_index)
        .expect("reload external version");
    let cube = harness.entity_index("Cube");
    assert_eq!(harness.health(cube), Some(999));
    assert!(!harness.workspace().document().is_dirty());
}

#[test]
fn prefab_instance_override_can_be_reverted() {
    let mut harness = EditorHarness::new();
    let inserted = harness
        .workspace_mut()
        .instantiate_prefab_asset("assets/prefabs/Crate.prefab.json", None)
        .expect("instantiate prefab");
    assert_eq!(inserted, 1);

    let instance = harness
        .workspace()
        .primary_selection()
        .expect("prefab root selected");
    let original = harness.transform(instance);
    harness.set_transform(
        instance,
        [original[0] + 2.0, original[1] + 1.0, original[2]],
    );

    let overrides = harness
        .workspace()
        .prefab_overrides(instance)
        .expect("read overrides");
    let position = overrides
        .iter()
        .find(|entry| entry.component == "transform" && entry.field == "position")
        .expect("position override");
    assert_eq!(position.instance_value, serde_json::json!([6.0, 1.0, 0.0]));

    assert!(harness.workspace_mut().revert_prefab_override(position));
    assert_eq!(harness.transform(instance), original);
}
