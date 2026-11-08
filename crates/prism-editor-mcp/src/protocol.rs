//! Minimal MCP (Model Context Protocol) stdio transport: JSON-RPC 2.0, one
//! message per line.
//!
//! Implements only the subset agents actually use: `initialize`, `tools/list`,
//! `tools/call` and `ping`. There are no resource, prompt or sampling
//! capabilities and no progress notifications; tools are synchronous and
//! return their result directly.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// MCP revision advertised in `initialize`.
///
/// The hand-written transport only covers the subset below, so a spec bump is
/// normally a one-line change here. `docs/editor-mcp.md` records when to
/// replace it with the `rmcp` SDK instead.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

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
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "prism-editor-mcp", "version": env!("CARGO_PKG_VERSION") },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_tools_and_the_pinned_version() {
        let result = initialize_result();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["capabilities"]["tools"], json!({}));
        assert_eq!(result["serverInfo"]["name"], "prism-editor-mcp");
        assert!(result["serverInfo"]["version"]
            .as_str()
            .is_some_and(|version| !version.is_empty()));
    }

    #[test]
    fn notifications_carry_no_id_and_requests_do() {
        let notification: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(notification.is_notification());
        let request: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert!(!request.is_notification());
    }

    #[test]
    fn unknown_methods_get_the_jsonrpc_method_not_found_code() {
        let response = failure(Value::Null, -32601, "不支持的方法 'nope'");
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["jsonrpc"], "2.0");
    }
}
