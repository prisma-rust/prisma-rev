//! Runs the game process and captures screenshots, closing the "edit then
//! look" loop for MCP.
//!
//! The game is a **child process** (`cargo run --release` or a direct exe).
//! Reader threads collect stdout/stderr into a ring buffer, and `game_logs`
//! returns the last N lines. Screenshots reuse the engine's
//! `PRISMREV_SCREENSHOT*` environment variables: spawn the process, wait for
//! the PNG to land, then stop it.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use prism_editor_mcp::ToolDef;

/// Log buffer capacity: enough to diagnose problems without exhausting memory.
const LOG_CAPACITY: usize = 2000;

pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "game_run",
            description: "在项目根目录启动游戏（默认 `cargo run --release`），日志进缓冲区。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "覆盖启动命令，默认 `cargo run --release`" },
                    "env": { "type": "object", "description": "额外环境变量，值为字符串" }
                }
            }),
        },
        ToolDef {
            name: "game_stop",
            description: "结束由 game_run / screenshot 启动的进程。",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "game_logs",
            description: "取最近 N 行游戏日志（stdout + stderr 混合）。",
            input_schema: json!({
                "type": "object",
                "properties": { "lines": { "type": "integer", "default": 200 } }
            }),
        },
        ToolDef {
            name: "screenshot",
            description: "启动游戏、在指定帧抓图、再结束进程，返回 PNG 路径与分辨率。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "输出 PNG（相对项目根或绝对路径）" },
                    "size": { "type": "string", "description": "如 1920x1080；不给则用窗口分辨率" },
                    "scale": { "type": "number", "description": "窗口分辨率的倍数（size 优先）" },
                    "frame": { "type": "integer", "description": "抓第几帧，默认 120" },
                    "timeout_ms": { "type": "integer", "description": "等待上限，默认 60000" },
                    "command": { "type": "string", "description": "覆盖启动命令" },
                    "env": { "type": "object" }
                },
                "required": ["path"]
            }),
        },
    ]
}

/// The process started by `game_run` plus its log buffer.
#[derive(Default)]
pub struct GameHost {
    child: Mutex<Option<Child>>,
    logs: Arc<Mutex<VecDeque<String>>>,
}

impl GameHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn call(&self, root: &Path, name: &str, args: &Value) -> Result<String, String> {
        match name {
            "game_run" => self.run(root, args),
            "game_stop" => self.stop(),
            "game_logs" => self.logs(args),
            "screenshot" => self.screenshot(root, args),
            other => Err(format!("未知工具 '{other}'")),
        }
    }

    fn spawn(
        &self,
        root: &Path,
        args: &Value,
        extra_env: &[(String, String)],
    ) -> Result<(), String> {
        let mut guard = self.child.lock().map_err(|_| "进程锁被污染".to_string())?;
        if guard.is_some() {
            return Err("已经有游戏在跑，先 game_stop".into());
        }
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("cargo run --release");
        let mut parts = command.split_whitespace();
        let program = parts.next().ok_or_else(|| "启动命令为空".to_string())?;

        let mut cmd = Command::new(program);
        cmd.args(parts)
            .current_dir(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = args.get("env").and_then(Value::as_object) {
            for (key, value) in env {
                if let Some(value) = value.as_str() {
                    cmd.env(key, value);
                }
            }
        }
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .spawn()
            .map_err(|err| format!("启动 '{command}' 失败: {err}"))?;
        self.pipe_logs(&mut child);
        *guard = Some(child);
        Ok(())
    }

    fn pipe_logs(&self, child: &mut Child) {
        let logs = Arc::clone(&self.logs);
        if let Some(stdout) = child.stdout.take() {
            let logs = Arc::clone(&logs);
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    push(&logs, line);
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    push(&logs, line);
                }
            });
        }
    }

    fn run(&self, root: &Path, args: &Value) -> Result<String, String> {
        self.spawn(root, args, &[])?;
        Ok("已启动（日志用 game_logs 取）".into())
    }

    fn stop(&self) -> Result<String, String> {
        let mut guard = self.child.lock().map_err(|_| "进程锁被污染".to_string())?;
        match guard.take() {
            Some(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
                Ok("已结束".into())
            }
            None => Ok("没有在跑的进程".into()),
        }
    }

    fn logs(&self, args: &Value) -> Result<String, String> {
        let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(200) as usize;
        let logs = self.logs.lock().map_err(|_| "日志锁被污染".to_string())?;
        let skip = logs.len().saturating_sub(lines);
        Ok(logs
            .iter()
            .skip(skip)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn screenshot(&self, root: &Path, args: &Value) -> Result<String, String> {
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "缺少 'path'".to_string())?;
        let target = {
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|err| format!("建目录失败: {err}"))?;
        }
        // A stale file would make the wait-for-file loop return immediately;
        // delete it first.
        let _ = std::fs::remove_file(&target);

        let frame = args.get("frame").and_then(Value::as_u64).unwrap_or(120);
        let timeout = Duration::from_millis(
            args.get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(60_000),
        );

        let mut env = vec![
            (
                "PRISMREV_SCREENSHOT".to_string(),
                target.display().to_string(),
            ),
            ("PRISMREV_SCREENSHOT_FRAME".to_string(), frame.to_string()),
        ];
        if let Some(size) = args.get("size").and_then(Value::as_str) {
            env.push(("PRISMREV_SCREENSHOT_SIZE".to_string(), size.to_string()));
        } else if let Some(scale) = args.get("scale").and_then(Value::as_f64) {
            env.push(("PRISMREV_SCREENSHOT_SCALE".to_string(), scale.to_string()));
        }

        self.spawn(root, args, &env)?;
        let started = Instant::now();
        let mut written = false;
        while started.elapsed() < timeout {
            if target.exists() {
                // The file may have just been created and not fully written;
                // wait for its size to settle.
                let first = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                std::thread::sleep(Duration::from_millis(200));
                let second = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                if first == second && second > 0 {
                    written = true;
                    break;
                }
            }
            if !self.is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.stop();

        if !written {
            let tail = self.logs(&json!({ "lines": 30 })).unwrap_or_default();
            return Err(format!(
                "等待截图超时（{} ms）\n最近的日志：\n{tail}",
                timeout.as_millis()
            ));
        }
        let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        Ok(serde_json::to_string_pretty(&json!({
            "path": target.display().to_string(),
            "bytes": bytes,
        }))
        .unwrap_or_default())
    }

    fn is_running(&self) -> bool {
        let Ok(mut guard) = self.child.lock() else {
            return false;
        };
        let Some(child) = guard.as_mut() else {
            return false;
        };
        matches!(child.try_wait(), Ok(None))
    }
}

fn push(logs: &Arc<Mutex<VecDeque<String>>>, line: String) {
    if let Ok(mut logs) = logs.lock() {
        if logs.len() == LOG_CAPACITY {
            logs.pop_front();
        }
        logs.push_back(line);
    }
}
