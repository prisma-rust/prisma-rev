//! `editor_*` tools that drive a **running** egui editor (`editor/`).
//!
//! The editor opens a local control channel on `127.0.0.1:<port>` and writes
//! the port to `<project root>/target/editor/control.port`. This module only
//! does TCP + JSON; it does not link engine / render and does not open Vulkan.
//!
//! | Tool | Purpose |
//! |---|---|
//! | `editor_launch` | Starts `prism-editor-egui` (debug build) and waits for the control channel |
//! | `editor_status` | Frame count, current scene, selection, Play state |
//! | `editor_capture` | Captures window/viewport PNG (`target` = `window`/`scene`/`game`) |
//! | `editor_quit` | Asks the editor to exit |
//! | `editor_ui` | UI node-tree snapshot, click/set by node (like DOM operations in browser MCP) |
//! | `editor_play` | Play / Stop / Pause (Play compiles and loads the project module) |
//! | `editor_logs` | Reads the Console buffer (project-module logs and panic backtraces) |
//! | `editor_input` | Injects keyboard/mouse into Play (one event per frame) for automation |

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use prism_editor_mcp::protocol::ToolDef;

/// The process started by `editor_launch`.
#[derive(Default)]
pub struct EditorHost {
    child: Mutex<Option<Child>>,
}

impl EditorHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn call(&self, root: &Path, name: &str, args: &Value) -> Result<String, String> {
        match name {
            "editor_launch" => self.launch(root, args),
            "editor_status" => status(root),
            "editor_capture" => capture(root, args),
            "editor_quit" => quit(self),
            "editor_ui" => ui(root, args),
            "editor_play" => play(root, args),
            "editor_logs" => logs(root, args),
            "editor_input" => input(root, args),
            other => Err(format!("未知的编辑器工具 '{other}'")),
        }
    }

    fn launch(&self, root: &Path, args: &Value) -> Result<String, String> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| "editor child lock poisoned")?;
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => return Err("编辑器已经在运行（先 editor_quit）".into()),
                _ => *guard = None,
            }
        }
        // The port file identifies *this* process: delete it first and wait for
        // it to reappear before considering the editor ready.
        let port_file = port_file(root);
        let _ = std::fs::remove_file(&port_file);

        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| default_command(root));
        let mut parts = command.split_whitespace();
        let program = parts.next().ok_or("command 为空")?;
        let mut child = Command::new(program)
            .args(parts)
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("启动 '{program}' 失败: {err}"))?;

        let wait = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);
        let deadline = Instant::now() + Duration::from_millis(wait);
        while Instant::now() < deadline {
            if port_file.exists() && control_port(root).is_ok() {
                // Returning as soon as the port appears would let the first
                // `editor_ui` see an empty dock (panels are not drawn on the
                // first frame), so wait until a few frames have actually been
                // drawn before handing back control.
                let ready = request(root, json!({ "cmd": "status" }))
                    .ok()
                    .and_then(|status| status.get("frames").and_then(Value::as_u64))
                    .map(|frames| frames >= 3)
                    .unwrap_or(false);
                if ready {
                    let pid = child.id();
                    *guard = Some(child);
                    return Ok(json!({
                        "ok": true,
                        "pid": pid,
                        "control": port_file.display().to_string(),
                        "command": command,
                    })
                    .to_string());
                }
            }
            if let Ok(Some(code)) = child.try_wait() {
                return Err(format!("编辑器退出（code {code}），控制通道没起来"));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(format!("等待控制通道超时（{wait} ms）"))
    }
}

pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "editor_launch",
            description: "启动 egui 编辑器（默认 target/debug/prism-editor-egui），等本地控制通道就绪后返回 pid。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "覆盖启动命令（相对项目根）" },
                    "wait_ms": { "type": "integer", "default": 30000 }
                }
            }),
        },
        ToolDef {
            name: "editor_status",
            description: "正在运行的 egui 编辑器状态：帧数、当前场景、选中实体、Play / 暂停、是否有未保存改动。",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "editor_capture",
            description: "让正在运行的编辑器截图：target=window 抓整窗（UI + 视口），target=scene|game 抓该视口的引擎离屏图。返回 PNG 绝对路径。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "enum": ["window", "scene", "game"],
                        "description": "抓什么：整窗 / Scene view / Game view，默认 window"
                    },
                    "path": { "type": "string", "description": "输出 PNG（相对项目根或绝对路径）；不给则写到 target/editor/captures/" },
                    "size": { "type": "string", "description": "如 1920x1080（最近邻缩放在编辑器里做）" }
                }
            }),
        },
        ToolDef {
            name: "editor_ui",
            description: "编辑器 UI 节点树与指针：action=snapshot 返回整棵界面树（面板 / 文档 / 层级实体 / 资产 / 检查器字段 / 视口物体 / 模态对话框 / 命令表）；action=click 按节点 id 点击（command:Save / panel:hierarchy / doc:0 / entity:3 / asset:assets/x.prefab.json / view:scene:entity:2 / dialog:close:discard）；action=move 报告视口某点下的目标；action=wheel 在视口里滚动（Scene 缩放 / Game 滚轮）；action=drag 在视口里拖动（gizmo / 框选 / 轨道 / 玩法指针）；action=set 给检查器字段或对话框输入框赋值。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["snapshot", "click", "move", "wheel", "drag", "set"], "default": "snapshot" },
                    "node": { "type": "string", "description": "click / set / move / wheel 时的节点 id（见 snapshot 返回）" },
                    "offset": { "description": "move / wheel 时相对节点锚点的像素偏移 [dx,dy]" },
                    "tab": { "type": "string", "description": "没有 node 时用 tab + x/y（sceneView / gameView）" },
                    "x": { "type": "number", "description": "视口局部 X（没有 node 时）" },
                    "y": { "type": "number", "description": "视口局部 Y（没有 node 时）" },
                    "delta": { "type": "number", "description": "wheel 的滚轮增量（默认 120）" },
                    "from": { "description": "drag 起点：{node,offset} 或 {x,y}" },
                    "to": { "description": "drag 终点：{node,offset} 或 {x,y}" },
                    "steps": { "type": "integer", "description": "drag 的采样步数（默认 8）" },
                    "mode": { "type": "string", "enum": ["orbit"], "description": "drag 时强制轨道相机" },
                    "ctrl": { "type": "boolean", "description": "drag 时按住 Ctrl（gizmo 吸附反转）" },
                    "value": { "description": "set 时的值（JSON：数字 / 字符串 / bool / 数组）" }
                }
            }),
        },
        ToolDef {
            name: "editor_play",
            description: "控制编辑器 Play：action=play 开始（会后台编译并加载工程模块，工程代码在 Game view 里跑）、stop 停止、pause 暂停/继续。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["play", "stop", "pause"],
                        "default": "play"
                    }
                }
            }),
        },
        ToolDef {
            name: "editor_logs",
            description: "读编辑器 Console 缓冲（引擎日志 + 工程模块的 project: 日志 + panic 调用栈）。since 是上一轮的 next，用于增量拉取。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "since": { "type": "integer", "default": 0, "description": "只要 seq >= since 的行（用上次返回的 next）" },
                    "limit": { "type": "integer", "default": 200 }
                }
            }),
        },
        ToolDef {
            name: "editor_input",
            description: "给正在 Play 的 Game View 注入输入：key=space/w/a/s/d/... 或 mouse=left/right/middle，state=press|release；click=true 等价于左键按下+松开。事件按帧排队，按下/松开不会被同一帧吞掉。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "按键名：w/a/s/d/q/e/space/shift/ctrl/alt/escape/tab/enter/up/down/left/right/0-9" },
                    "mouse": { "type": "string", "enum": ["left", "right", "middle"] },
                    "state": { "type": "string", "enum": ["press", "release"], "default": "press" },
                    "click": { "type": "boolean", "description": "左键点一下（按下 + 松开）" }
                }
            }),
        },
        ToolDef {
            name: "editor_quit",
            description: "让编辑器走正常退出路径（会落盘 state.json）。",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
    ]
}

fn play(root: &Path, args: &Value) -> Result<String, String> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("play");
    let cmd = match action {
        "play" => "play",
        "stop" => "stop",
        "pause" => "pause",
        other => return Err(format!("action 只能是 play / stop / pause，收到 '{other}'")),
    };
    let reply = request(root, json!({ "cmd": cmd }))?;
    Ok(reply.to_string())
}

fn logs(root: &Path, args: &Value) -> Result<String, String> {
    let since = args.get("since").and_then(Value::as_u64).unwrap_or(0);
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200);
    let reply = request(
        root,
        json!({ "cmd": "logs", "since": since, "limit": limit }),
    )?;
    Ok(reply.to_string())
}

fn input(root: &Path, args: &Value) -> Result<String, String> {
    let reply = request(
        root,
        json!({
            "cmd": "input",
            "key": args.get("key").cloned().unwrap_or(Value::Null),
            "mouse": args.get("mouse").cloned().unwrap_or(Value::Null),
            "state": args.get("state").cloned().unwrap_or(Value::Null),
            "click": args.get("click").cloned().unwrap_or(Value::Null),
        }),
    )?;
    Ok(reply.to_string())
}

/// Pointer arguments for `editor_ui`: forward only the keys the caller actually
/// provided, so `null` is not sent to the control channel.
fn pointer_args(args: &Value) -> Value {
    let mut body = serde_json::Map::new();
    for key in [
        "node", "offset", "tab", "x", "y", "delta", "from", "to", "steps", "mode", "ctrl",
    ] {
        if let Some(value) = args.get(key) {
            if !value.is_null() {
                body.insert(key.to_string(), value.clone());
            }
        }
    }
    Value::Object(body)
}

fn ui(root: &Path, args: &Value) -> Result<String, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("snapshot");
    let (cmd, mut body) = match action {
        "snapshot" => ("ui.snapshot", json!({})),
        "click" => {
            let node = args
                .get("node")
                .and_then(Value::as_str)
                .ok_or("click 需要 node")?;
            ("ui.click", json!({ "node": node }))
        }
        "move" => ("ui.move", pointer_args(args)),
        "wheel" => ("ui.wheel", pointer_args(args)),
        "drag" => {
            if args.get("from").is_none() || args.get("to").is_none() {
                return Err("drag 需要 from / to".to_string());
            }
            ("ui.drag", pointer_args(args))
        }
        "set" => {
            let node = args
                .get("node")
                .and_then(Value::as_str)
                .ok_or("set 需要 node")?;
            (
                "ui.set",
                json!({ "node": node, "value": args.get("value").cloned().unwrap_or(Value::Null) }),
            )
        }
        other => {
            return Err(format!(
                "未知 action '{other}'（snapshot / click / move / wheel / drag / set）"
            ))
        }
    };
    body["cmd"] = json!(cmd);
    let response = request(root, body)?;
    if response.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(response.to_string());
    }
    Ok(response.to_string())
}

fn status(root: &Path) -> Result<String, String> {
    let response = request(root, json!({ "cmd": "status" }))?;
    Ok(response.to_string())
}

fn capture(root: &Path, args: &Value) -> Result<String, String> {
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("window");
    if !matches!(target, "window" | "scene" | "game") {
        return Err(format!("未知 target '{target}'（window / scene / game）"));
    }
    let path = match args.get("path").and_then(Value::as_str) {
        Some(value) => {
            let value = PathBuf::from(value);
            if value.is_absolute() {
                value
            } else {
                root.join(value)
            }
        }
        None => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default();
            root.join("target/editor/captures")
                .join(format!("{target}-{stamp}.png"))
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("建目录失败: {err}"))?;
    }
    let mut request_body =
        json!({ "cmd": "capture", "target": target, "path": path.to_string_lossy() });
    if let Some(size) = args.get("size").and_then(Value::as_str) {
        request_body["size"] = json!(size);
    }
    let response = request(root, request_body)?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(response.to_string());
    }
    Ok(response.to_string())
}

fn quit(host: &EditorHost) -> Result<String, String> {
    // The control channel also works when `--project` cannot find a project
    // root: use the known port file directly.
    let root = prism_editor_mcp::resolve_root().map_err(|err| err.to_string())?;
    let response = request(&root, json!({ "cmd": "quit" }))?;
    let mut guard = host
        .child
        .lock()
        .map_err(|_| "editor child lock poisoned")?;
    if let Some(mut child) = guard.take() {
        let _ = child.wait();
    }
    Ok(response.to_string())
}

/// File the editor writes its port to (`PRISMREV_EDITOR_CONTROL` can override it
/// with a port number).
fn port_file(root: &Path) -> PathBuf {
    match std::env::var_os("PRISMREV_EDITOR_CONTROL") {
        Some(value) => PathBuf::from(value),
        None => root.join("target/editor/control.port"),
    }
}

fn control_port(root: &Path) -> Result<u16, String> {
    if let Ok(value) = std::env::var("PRISMREV_EDITOR_CONTROL") {
        if let Ok(port) = value.trim().parse::<u16>() {
            return Ok(port);
        }
    }
    let file = port_file(root);
    let text = std::fs::read_to_string(&file).map_err(|err| {
        format!(
            "读不到控制端口 {}: {err}（编辑器没开，或先 editor_launch）",
            file.display()
        )
    })?;
    text.trim()
        .parse::<u16>()
        .map_err(|err| format!("控制端口文件内容无效: {err}"))
}

fn request(root: &Path, body: Value) -> Result<Value, String> {
    let port = control_port(root)?;
    let stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|err| format!("连不上编辑器控制通道 127.0.0.1:{port}: {err}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut writer = stream
        .try_clone()
        .map_err(|err| format!("复制 socket 失败: {err}"))?;
    writeln!(writer, "{body}").map_err(|err| format!("写控制通道失败: {err}"))?;
    writer.flush().ok();
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| format!("读控制通道失败: {err}"))?;
    serde_json::from_str(line.trim()).map_err(|err| format!("控制通道回执不是 JSON: {err}"))
}

fn default_command(root: &Path) -> String {
    let editor_root = find_editor_root(root).unwrap_or_else(|| root.to_path_buf());
    let debug = editor_root.join("editor/target/debug/prism-editor-egui");
    let release = editor_root.join("editor/target/release/prism-editor-egui");
    for candidate in [release, debug] {
        let with_exe = if cfg!(windows) {
            candidate.with_extension("exe")
        } else {
            candidate.clone()
        };
        if with_exe.exists() {
            return with_exe.display().to_string();
        }
    }
    // Fall back to cargo if the editor has never been built (it compiles
    // first: slow but usable).
    format!(
        "cargo run -p prism-editor-egui --manifest-path {}",
        editor_root.join("editor/Cargo.toml").display()
    )
}

fn find_editor_root(root: &Path) -> Option<PathBuf> {
    let mut current = Some(root);
    while let Some(dir) = current {
        if dir.join("editor/Cargo.toml").is_file() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// The port file exists but the editor is dead: remove the stale file (used
/// before `editor_launch`).
#[allow(dead_code)]
pub fn stale_port_file(root: &Path) -> bool {
    let file = port_file(root);
    if let Ok(text) = std::fs::read_to_string(&file) {
        if let Ok(port) = text.trim().parse::<u16>() {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return false;
            }
        }
    }
    let _ = std::fs::remove_file(&file);
    true
}

#[allow(dead_code)]
fn free_port_probe() -> Option<u16> {
    TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|addr| addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_editor_from_nested_user_project() {
        let mut repo = std::env::current_dir().unwrap();
        while !repo.join("editor/Cargo.toml").is_file() {
            repo = repo.parent().expect("repository root").to_path_buf();
        }
        let fishing = repo.join("projects/fishing");
        assert_eq!(find_editor_root(&fishing), Some(repo));
    }
}
