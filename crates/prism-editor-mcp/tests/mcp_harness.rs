//! End-to-end MCP tool scenarios over a temporary project.

mod support;

use std::collections::BTreeSet;

use prism_editor_mcp::tools;
use serde_json::{json, Value};
use support::{McpHarness, SCENE};

const CAMERA: &str = "prism_engine::scene::Camera";

#[test]
fn documents_list_and_read_round_trip() {
    let harness = McpHarness::new();
    let list = harness
        .call_json("documents_list", json!({ "kind": "scene" }))
        .expect("list documents");
    let listed = list.as_array().expect("documents array");
    assert!(
        listed
            .iter()
            .any(|entry| entry["path"].as_str() == Some(SCENE)),
        "scene must be listed: {list}"
    );

    let doc = harness
        .call_json("document_read", json!({ "path": SCENE }))
        .expect("read document");
    assert_eq!(doc["entities"].as_array().expect("entities").len(), 2);
    assert_eq!(doc["entities"][1]["name"], "Cube");
}

#[test]
fn component_add_field_set_entity_get_and_remove_round_trip() {
    let harness = McpHarness::new();

    let added = harness
        .call_json(
            "component_add",
            json!({ "path": SCENE, "entity": "Cube", "component": CAMERA }),
        )
        .expect("add component");
    assert!(
        added.get("exposure").is_some(),
        "Camera default should contain exposure: {added}"
    );

    let updated = harness
        .call_json(
            "field_set",
            json!({
                "path": SCENE,
                "entity": "Cube",
                "component": CAMERA,
                "field": "exposure",
                "value": 2.5
            }),
        )
        .expect("set field");
    assert_eq!(updated["exposure"], 2.5);

    let entity = harness
        .call_json("entity_get", json!({ "path": SCENE, "entity": "Cube" }))
        .expect("get entity by name");
    assert_eq!(entity["components"][CAMERA]["exposure"], 2.5);

    let by_id = harness
        .call_json("entity_get", json!({ "path": SCENE, "entity": "2" }))
        .expect("get entity by id");
    assert_eq!(by_id["name"], "Cube");

    let removed = harness
        .call(
            "component_remove",
            json!({ "path": SCENE, "entity": "Cube", "component": CAMERA }),
        )
        .expect("remove component");
    assert!(removed.contains("已移除"), "{removed}");
    let entity = harness
        .call_json("entity_get", json!({ "path": SCENE, "entity": "Cube" }))
        .expect("get entity after removal");
    assert!(
        entity["components"].get(CAMERA).is_none(),
        "component should be gone: {entity}"
    );
}

#[test]
fn invalid_inputs_return_actionable_errors() {
    let harness = McpHarness::new();

    let missing_entity = harness
        .call("entity_get", json!({ "path": SCENE, "entity": "Missing" }))
        .expect_err("missing entity");
    assert!(missing_entity.contains("找不到实体"), "{missing_entity}");

    let missing_file = harness
        .call("document_read", json!({ "path": "../outside.scene.json" }))
        .expect_err("missing document");
    assert!(missing_file.contains("无法读取"), "{missing_file}");

    let bad_component = harness
        .call(
            "field_set",
            json!({
                "path": SCENE,
                "entity": "Cube",
                "component": "not::a::Component",
                "field": "value",
                "value": 1
            }),
        )
        .expect_err("unknown component");
    assert!(bad_component.contains("没有组件"), "{bad_component}");

    let bad_add = harness
        .call(
            "component_add",
            json!({ "path": SCENE, "entity": "Cube", "component": "not::a::Type" }),
        )
        .expect_err("unknown component type");
    assert!(bad_add.contains("不是已注册类型"), "{bad_add}");

    let invalid_doc = harness
        .call(
            "document_write",
            json!({ "path": SCENE, "doc": { "version": 3, "entities": [] } }),
        )
        .expect_err("empty document");
    assert!(!invalid_doc.is_empty());

    let unknown = harness
        .call("does_not_exist", json!({}))
        .expect_err("unknown tool");
    assert!(unknown.contains("未知工具"), "{unknown}");

    assert!(harness.disk_text().contains("Cube"));
}

#[test]
fn tool_schema_and_protocol_shape_are_stable() {
    let defs = tools::definitions();
    let expected: BTreeSet<&str> = [
        "asset_inspect",
        "asset_validate",
        "component_add",
        "component_remove",
        "document_read",
        "document_write",
        "documents_list",
        "entity_get",
        "field_set",
        "schema_list",
    ]
    .into_iter()
    .collect();
    let names: BTreeSet<&str> = defs.iter().map(|def| def.name).collect();
    assert_eq!(names, expected);

    for def in &defs {
        assert_eq!(
            def.input_schema["type"], "object",
            "{} must use an object schema",
            def.name
        );
        if let Some(required) = def.input_schema["required"].as_array() {
            let properties: &serde_json::Map<String, Value> = def.input_schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{} has required fields but no properties", def.name));
            for field in required {
                let field = field.as_str().expect("required field name");
                assert!(
                    properties.contains_key(field),
                    "{} requires unknown property {field}",
                    def.name
                );
            }
        }
    }

    // The wire format itself is `rmcp`'s responsibility now; what this crate
    // still owns is the tool table, so the schema checks above are the part
    // that has to keep holding.
}
