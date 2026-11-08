//! Minimal MCP (Model Context Protocol) stdio transport: JSON-RPC 2.0, one
//! message per line.
//!
//! Implements only the subset agents actually use: `initialize`, `tools/list`,
//! `tools/call` and `ping`. There are no resource, prompt or sampling
//! capabilities and no progress notifications; tools are synchronous and
//! return their result directly.
//!
//! # Why this is hand-written instead of `rmcp` (#1045)
//!
//! The whole server is one synchronous read loop over stdin, and this file is
//! its protocol layer. `rmcp` would bring an async runtime (tokio) into a
//! process whose tools all block on `cargo` / file IO anyway, and the two
//! capabilities it adds — resources and prompts — are not what this server
//! exposes. Keeping the subset means the process stays dependency-light and
//! the MCP spec revision in [`initialize_result`] is one string to bump.
//!
//! Revisit when any of these becomes true: a protocol revision changes the
//! message framing or handshake beyond a version string, a client needs
//! resources / prompts / progress notifications, or the tool table grows a
//! streaming result. If `rmcp` is adopted then, check first that its tokio
//! dependency stays inside this process: `prism-editor-mcp` is not part of the
//! player tree, and `scripts/check_deps.py` must keep passing.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// An incoming JSON-RPC request (notifications have no `id`).
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// Static tool description; serializes directly into an MCP `tools/list` entry.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

pub fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn failure(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

/// Wraps a tool result into MCP's `content` array.
pub fn text_content(text: impl Into<String>, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text.into() }],
        "isError": is_error,
    })
}

pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "prism-editor-mcp", "version": env!("CARGO_PKG_VERSION") },
    })
}
