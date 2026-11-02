//! `prism-editor-mcp`：把 `prism-editor-api` 的数据层以 MCP 工具形式暴露给
//! agent（stdio，一行一条 JSON-RPC 消息）。
//!
//! 用法：
//!
//! ```text
//! prism-editor-mcp --project projects/fishing
//! ```
//!
//! 项目根也可以用 `PRISMREV_PROJECT` 指定；都没给时按 `assets/` 向上找。
//!
//! 设计边界：这个进程**不链接** engine / render，也不开 Vulkan —— 它只读写
//! `.scene.json`、跑反射校验，需要画面时用 `screenshot` 起一个子进程。

mod editor_egui;
mod game;

use std::io::{BufRead, Write};

use prism_editor_mcp::{protocol, resolve_root, tools};
use protocol::{Request, ToolDef};
use serde_json::{json, Value};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let root = match resolve_root() {
        Ok(root) => root,
        Err(err) => {
            eprintln!("prism-editor-mcp: {err}");
            std::process::exit(2);
        }
    };
    log::info!("project root: {}", root.display());

    let host = game::GameHost::new();
    let editor = editor_egui::EditorHost::new();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(err) => {
                log::warn!("丢弃无法解析的消息: {err}");
                continue;
            }
        };
        // 通知没有 id，不回复。
        if request.is_notification() {
            continue;
        }
        let id = request.id.clone().unwrap_or(Value::Null);
        let response = handle(&root, &host, &editor, &request, id);
        let encoded = serde_json::to_string(&response).unwrap_or_else(|_| "{}".into());
        if writeln!(stdout, "{encoded}").is_err() {
            break;
        }
        let _ = stdout.flush();
    }
}

fn handle(
    root: &std::path::Path,
    host: &game::GameHost,
    editor: &editor_egui::EditorHost,
    request: &Request,
    id: Value,
) -> Value {
    match request.method.as_str() {
        "initialize" => protocol::success(id, protocol::initialize_result()),
        "ping" => protocol::success(id, json!({})),
        "tools/list" => {
            let mut defs: Vec<ToolDef> = tools::definitions();
            defs.extend(game::definitions());
            defs.extend(editor_egui::definitions());
            protocol::success(id, json!({ "tools": defs }))
        }
        "tools/call" => {
            let name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let outcome = if editor_egui::definitions()
                .iter()
                .any(|def| def.name == name)
            {
                editor.call(root, &name, &args)
            } else if game::definitions().iter().any(|def| def.name == name) {
                host.call(root, &name, &args)
            } else {
                tools::call(root, &name, &args)
            };
            match outcome {
                Ok(text) => protocol::success(id, protocol::text_content(text, false)),
                Err(err) => protocol::success(id, protocol::text_content(err, true)),
            }
        }
        other => protocol::failure(id, -32601, format!("不支持的方法 '{other}'")),
    }
}
