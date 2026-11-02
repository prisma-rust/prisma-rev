//! Headless editor harness for end-to-end Workspace scenarios.
//!
//! The harness owns a temporary project with a scene, a prefab, and a minimal
//! schema. Tests drive the same public `Workspace` operations the editor uses
//! (open, select, edit, undo/redo, save, reload) and assert both in-memory and
//! on-disk state.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use prism_editor_gpui_core::{FieldSchema, SceneDoc, TypeSchema, Workspace};
use serde_json::{json, Value};

pub const HEALTH: &str = "test::Health";

static NEXT_HARNESS: AtomicU64 = AtomicU64::new(0);

/// A temporary project plus an initialized [`Workspace`].
pub struct EditorHarness {
    root: PathBuf,
    workspace: Workspace,
}

impl EditorHarness {
    /// Create an isolated project with `Main.scene.json` and `Crate.prefab.json`.
    pub fn new() -> Self {
        let id = NEXT_HARNESS.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("prism-editor-harness-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("assets/scenes")).expect("create scenes dir");
        fs::create_dir_all(root.join("assets/prefabs")).expect("create prefabs dir");

        fs::write(root.join("assets/scenes/main.scene.json"), scene_fixture())
            .expect("write scene fixture");
        fs::write(
            root.join("assets/prefabs/Crate.prefab.json"),
            prefab_fixture(),
        )
        .expect("write prefab fixture");
        fs::write(
            root.join("assets/scenes.json"),
            "{\n  \"scenes\": [\n    { \"name\": \"Main\", \"path\": \"scenes/main.rscn\", \"enabled\": true }\n  ]\n}\n",
        )
        .expect("write scene manifest");

        let mut workspace = Workspace::sample();
        let opened = workspace.load_project(root.clone()).expect("load project");
        assert!(opened >= 2, "scene and prefab should be discovered");
        workspace.schema.types.push(health_schema());

        Self { root, workspace }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspace
    }

    pub fn scene_path(&self) -> PathBuf {
        self.root.join("assets/scenes/main.scene.json")
    }

    pub fn disk_json(&self) -> Value {
        let text = fs::read_to_string(self.scene_path()).expect("read scene from disk");
        serde_json::from_str(&text).expect("parse scene from disk")
    }

    pub fn entity_index(&self, name: &str) -> usize {
        self.workspace
            .document()
            .doc
            .entities
            .iter()
            .position(|entity| entity.display_name() == name)
            .unwrap_or_else(|| panic!("entity {name:?} not found"))
    }

    pub fn select(&mut self, indices: &[usize]) {
        self.workspace.select_many(indices, false);
    }

    pub fn transform(&self, index: usize) -> [f32; 3] {
        self.workspace.document().doc.entities[index]
            .transform
            .position
    }

    pub fn set_transform(&mut self, index: usize, position: [f32; 3]) {
        self.workspace.edit("move entity", |document| {
            document.doc.entities[index].transform.position = position;
        });
    }

    pub fn health(&self, index: usize) -> Option<i64> {
        self.workspace.document().doc.entities[index]
            .components
            .get(HEALTH)
            .and_then(|value| value.get("hp"))
            .and_then(Value::as_i64)
    }

    pub fn set_health(&mut self, index: usize, hp: i64) {
        self.workspace.edit("set health", |document| {
            let value = document.doc.entities[index]
                .components
                .get_mut(HEALTH)
                .expect("entity has Health");
            value["hp"] = json!(hp);
        });
    }

    pub fn undo(&mut self) {
        self.workspace.undo();
    }

    pub fn redo(&mut self) {
        self.workspace.redo();
    }
}

impl Drop for EditorHarness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn scene_fixture() -> String {
    let mut doc = SceneDoc::default();
    let root = doc.add_named_child(None, "Root");
    let cube = doc.add_named_child(Some(root), "Cube");
    let camera = doc.add_named_child(None, "Main Camera");
    doc.entities[cube].transform.position = [1.0, 2.0, 3.0];
    doc.entities[camera].transform.position = [0.0, 1.0, -10.0];
    doc.to_json_string()
}

fn prefab_fixture() -> String {
    let mut doc = SceneDoc::default();
    let crate_root = doc.add_named_child(None, "Crate");
    doc.entities[crate_root].transform.position = [4.0, 0.0, 0.0];
    doc.to_json_string()
}

fn health_schema() -> TypeSchema {
    TypeSchema {
        name: HEALTH.to_string(),
        type_id: "test-health".to_string(),
        version: 1,
        is_component: true,
        fields: vec![FieldSchema {
            name: "hp".to_string(),
            kind: "i32".to_string(),
            ..Default::default()
        }],
        default_json: Some(json!({ "hp": 10 })),
    }
}
