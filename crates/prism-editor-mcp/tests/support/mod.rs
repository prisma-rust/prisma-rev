//! Temporary-project harness for the MCP document tools.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use prism_editor_api::scene;
use prism_editor_mcp::tools;
use serde_json::{json, Value};

pub const SCENE: &str = "assets/scenes/main.scene.json";

static NEXT_HARNESS: AtomicU64 = AtomicU64::new(0);

/// An isolated project plus direct access to the MCP tool dispatch layer.
pub struct McpHarness {
    root: PathBuf,
}

impl McpHarness {
    pub fn new() -> Self {
        let id = NEXT_HARNESS.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("prism-mcp-harness-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("assets/scenes")).expect("create scenes dir");
        scene::write_doc(&root.join(SCENE), &scene_fixture()).expect("write scene fixture");
        Self { root }
    }

    pub fn scene_path(&self) -> PathBuf {
        self.root.join(SCENE)
    }

    pub fn call(&self, name: &str, args: Value) -> Result<String, String> {
        tools::call(&self.root, name, &args)
    }

    pub fn call_json(&self, name: &str, args: Value) -> Result<Value, String> {
        let text = self.call(name, args)?;
        serde_json::from_str(&text)
            .map_err(|error| format!("{name} returned invalid JSON: {error}"))
    }

    pub fn disk_text(&self) -> String {
        fs::read_to_string(self.scene_path()).expect("read scene text from disk")
    }
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn scene_fixture() -> Value {
    json!({
        "version": 3,
        "entities": [
            {
                "id": 1,
                "name": "Root",
                "transform": {
                    "position": [0.0, 0.0, 0.0],
                    "rotation": [0.0, 0.0, 0.0, 1.0],
                    "scale": [1.0, 1.0, 1.0]
                },
                "components": {}
            },
            {
                "id": 2,
                "parent": 1,
                "name": "Cube",
                "transform": {
                    "position": [1.0, 2.0, 3.0],
                    "rotation": [0.0, 0.0, 0.0, 1.0],
                    "scale": [1.0, 1.0, 1.0]
                },
                "components": {}
            }
        ]
    })
}
