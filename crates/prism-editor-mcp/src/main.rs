//! `prism-editor-mcp` exposes the `prism-editor-api` data layer as MCP tools
//! for agents (stdio transport, provided by `rmcp`).
//!
//! Usage:
//!
//! ```text
//! prism-editor-mcp --project projects/fishing
//! ```
//!
//! The project root can also come from `PRISMREV_PROJECT`; when neither is
//! given, it walks up looking for `assets/`.
//!
//! Design boundary: this process does **not** link engine / render and does
//! not open Vulkan. It only reads and writes `.scene.json` and runs reflection
//! validation; when it needs an image it spawns a child process through
//! `screenshot`.

mod editor_egui;
mod game;

use std::path::PathBuf;
use std::sync::Arc;

use prism_editor_mcp::{resolve_root, tools, ToolDef};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;

/// The MCP server: the project root plus the two process-spawning hosts.
struct PrismEditorServer {
    root: PathBuf,
    game: game::GameHost,
    editor: editor_egui::EditorHost,
}

impl PrismEditorServer {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            game: game::GameHost::new(),
            editor: editor_egui::EditorHost::new(),
        }
    }

    /// Every tool this server exposes, in table order.
    fn tool_definitions() -> Vec<ToolDef> {
        let mut defs = tools::definitions();
        defs.extend(game::definitions());
        defs.extend(editor_egui::definitions());
        defs
    }

    /// Routes a call to the module that owns the tool.
    fn dispatch(&self, name: &str, args: &Value) -> Result<String, String> {
        if editor_egui::definitions()
            .iter()
            .any(|definition| definition.name == name)
        {
            self.editor.call(&self.root, name, args)
        } else if game::definitions()
            .iter()
            .any(|definition| definition.name == name)
        {
            self.game.call(&self.root, name, args)
        } else {
            tools::call(&self.root, name, args)
        }
    }
}

impl ServerHandler for PrismEditorServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("prism-editor-mcp", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: Self::tool_definitions().iter().map(to_tool).collect(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let name = request.name.to_string();
        // Tool bodies read and write files and spawn `cargo`/the game binary;
        // `block_in_place` keeps that off the async scheduler without turning
        // every tool into an async function.
        let outcome = tokio::task::block_in_place(|| self.dispatch(&name, &args));
        let result = match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(err) => CallToolResult::error(vec![ContentBlock::text(err)]),
        };
        Ok(result.into())
    }
}

fn to_tool(definition: &ToolDef) -> Tool {
    let schema: JsonObject = match &definition.input_schema {
        Value::Object(map) => map.clone(),
        _ => JsonObject::new(),
    };
    Tool::new(definition.name, definition.description, Arc::new(schema))
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let root = match resolve_root() {
        Ok(root) => root,
        Err(err) => {
            eprintln!("prism-editor-mcp: {err}");
            std::process::exit(2);
        }
    };
    log::info!("project root: {}", root.display());

    let server = PrismEditorServer::new(root);
    let service = match server.serve(stdio()).await {
        Ok(service) => service,
        Err(err) => {
            eprintln!("prism-editor-mcp: MCP initialize failed: {err}");
            std::process::exit(1);
        }
    };
    if let Err(err) = service.waiting().await {
        eprintln!("prism-editor-mcp: MCP session ended with an error: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server() -> PrismEditorServer {
        PrismEditorServer::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    }

    #[test]
    fn tool_table_has_unique_names_and_valid_schemas() {
        let defs = PrismEditorServer::tool_definitions();
        assert!(!defs.is_empty(), "the server must expose tools");
        let mut seen = std::collections::HashSet::new();
        for definition in &defs {
            assert!(!definition.name.is_empty());
            assert!(
                seen.insert(definition.name),
                "duplicate tool name {}",
                definition.name
            );
            let tool = to_tool(definition);
            assert_eq!(tool.name, definition.name);
            assert!(
                !tool.input_schema.is_empty(),
                "{} has no input schema",
                tool.name
            );
        }
    }

    #[test]
    fn advertisement_lists_tools_and_names_the_server() {
        let server = server();
        let info = server.get_info();
        assert_eq!(info.server_info.name, "prism-editor-mcp");
        assert!(info.capabilities.tools.is_some(), "tools capability");
    }

    #[test]
    fn unknown_tool_returns_a_tool_level_error() {
        let server = server();
        let error = server
            .dispatch("no_such_tool", &json!({}))
            .expect_err("unknown tool must fail");
        assert!(error.contains("no_such_tool"), "{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The tool tables in `docs/editor-mcp.md` are the only entry point agents
    /// have; a missing row means a tool that cannot be discovered. Keep the
    /// documented names and required arguments in sync with the definitions.
    #[test]
    fn docs_tables_list_every_registered_tool() {
        const DOCS: &str = include_str!("../../../docs/editor-mcp.md");
        let documented: BTreeSet<&str> = DOCS
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("| `")?;
                let (name, _) = rest.split_once('`')?;
                name.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '_')
                    .then_some(name)
            })
            .collect();

        let mut definitions = tools::definitions();
        definitions.extend(game::definitions());
        definitions.extend(editor_egui::definitions());
        let registered: BTreeSet<&str> = definitions.iter().map(|def| def.name).collect();

        assert_eq!(
            documented, registered,
            "docs/editor-mcp.md 的工具表与注册表不一致（左 = 文档，右 = 代码）"
        );

        for def in &definitions {
            let required = def
                .input_schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            for arg in required {
                assert!(
                    DOCS.contains(&format!("`{arg}`")),
                    "工具 `{}` 的必填参数 `{arg}` 没有写进 docs/editor-mcp.md",
                    def.name
                );
            }
        }
    }
}
