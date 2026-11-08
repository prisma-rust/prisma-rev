//! `docs/editor-mcp.md` must name every tool this server registers.
//!
//! The tool definitions live in `src/tools.rs`, `src/game.rs` and `src/editor_egui.rs`,
//! while the tables in the doc are a hand-written copy. That copy had already fallen
//! eight tools behind before #1001, so a cheap name-level check is enough to keep it
//! honest without generating markdown from code.

use std::path::PathBuf;

const FILES: [&str; 3] = ["tools.rs", "game.rs", "editor_egui.rs"];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir()
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/prism-editor-mcp has a repository root")
        .to_path_buf()
}

/// Tool names, read from the `name: "…"` field of each `ToolDef`.
fn registered_tools() -> Vec<String> {
    let source_dir = manifest_dir().join("src");
    let mut tools = Vec::new();
    for file in FILES {
        let text = std::fs::read_to_string(source_dir.join(file))
            .unwrap_or_else(|error| panic!("read {file}: {error}"));
        for line in text.lines() {
            let Some(rest) = line.trim().strip_prefix("name: \"") else {
                continue;
            };
            let Some(end) = rest.find('"') else { continue };
            tools.push(rest[..end].to_string());
        }
    }
    tools
}

#[test]
fn every_registered_tool_is_documented() {
    let tools = registered_tools();
    assert_eq!(tools.len(), 22, "unexpected tool count: {tools:?}");

    let docs = std::fs::read_to_string(repo_root().join("docs/editor-mcp.md"))
        .expect("docs/editor-mcp.md exists");
    let missing: Vec<&String> = tools
        .iter()
        .filter(|name| !docs.contains(&format!("`{name}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/editor-mcp.md does not mention: {missing:?}"
    );
}
