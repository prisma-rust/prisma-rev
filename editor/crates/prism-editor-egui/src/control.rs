//! 本地控制通道：让 MCP（`crates/prism-editor-mcp`）驱动**正在运行的**编辑器。
//!
//! 为什么不是环境变量 / 子进程：窗口截图必须发生在编辑器进程里（`PrintWindow`
//! 要有 HWND，引擎离屏图像要有 `GraphRenderer`），而 egui 是单线程事件循环，
//! 所以外部只能通过一个通道把请求排进 UI 线程。
//!
//! 形态：
//!
//! - 启动时在 `127.0.0.1:0`（内核分配端口）上 `TcpListener`，把端口写进
//!   `<state 目录>/control.port`，退出时删掉；只监听回环，不接受远程连接。
//! - 每行一条 JSON，一问一答：
//!
//! ```text
//! → {"cmd":"ping"}
//! ← {"ok":true,"pid":1234,"frames":42,"scene":"default.scene.json"}
//! → {"cmd":"capture","target":"window","path":"shot.png","size":"1920x1080"}
//! ← {"ok":true,"path":"...","width":1440,"height":900}
//! ```
//!
//! 支持的 `cmd`：`ping` / `status` / `capture` / `quit` / `quit.confirm` /
//! `quit.cancel`（后两者走「未保存改动」三选一，不强制退出）。
//!
//! 请求从 socket 线程 push 进 `mpsc`，UI 线程每帧开头 `drain()` 一次并在
//! 同一帧内完成截图（窗口截图取的是上一帧呈现结果），结果用回执 channel 送回。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use serde_json::{json, Value};

/// 一条待处理请求 + 回执通道。
pub struct ControlRequest {
    pub command: String,
    pub args: Value,
    pub reply: Sender<Value>,
}

/// 运行中的控制通道。`Drop` 时删掉端口文件。
pub struct ControlServer {
    port_file: PathBuf,
    requests: Receiver<ControlRequest>,
}

impl ControlServer {
    /// 在 `state_dir` 下开通道并写 `control.port`。
    pub fn start(state_dir: &Path) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| anyhow::anyhow!("control: bind loopback failed: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| anyhow::anyhow!("control: local_addr failed: {error}"))?
            .port();

        let (tx, rx) = mpsc::channel::<ControlRequest>();
        let tx = Arc::new(tx);
        std::thread::Builder::new()
            .name("editor-control".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let tx = Arc::clone(&tx);
                    std::thread::spawn(move || serve(stream, &tx));
                }
            })
            .map_err(|error| anyhow::anyhow!("control: spawn accept thread failed: {error}"))?;

        std::fs::create_dir_all(state_dir).ok();
        let port_file = state_dir.join("control.port");
        std::fs::write(&port_file, format!("{port}\n"))
            .map_err(|error| anyhow::anyhow!("control: write {}: {error}", port_file.display()))?;
        log::info!("control: listening on 127.0.0.1:{port}");

        Ok(Self {
            port_file,
            requests: rx,
        })
    }

    /// UI 线程每帧调用一次：取走这一帧要处理的请求（非阻塞）。
    pub fn drain(&self) -> Vec<ControlRequest> {
        self.requests.try_iter().collect()
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        std::fs::remove_file(&self.port_file).ok();
    }
}

/// 单连接：读一行 JSON → 排队 → 回一行 JSON。
fn serve(stream: TcpStream, tx: &Sender<ControlRequest>) {
    let peer_ok = stream
        .peer_addr()
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false);
    if !peer_ok {
        return;
    }
    let reader_stream = match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let mut writer = stream;
    let mut reader = BufReader::new(reader_stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({ "ok": false, "error": format!("bad json: {error}") })
                );
                continue;
            }
        };
        let command = value
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if command.is_empty() {
            let _ = writeln!(writer, "{}", json!({ "ok": false, "error": "missing cmd" }));
            continue;
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let request = ControlRequest {
            command,
            args: value,
            reply: reply_tx,
        };
        if tx.send(request).is_err() {
            return;
        }
        // UI 线程最多 5s 内回执；超时说明编辑器卡住 / 没在跑 UI 循环。
        let response = reply_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_else(|_| json!({ "ok": false, "error": "editor did not answer in 5s" }));
        if writeln!(writer, "{response}").is_err() {
            return;
        }
        let _ = writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn server_publishes_loopback_port_and_removes_it_on_drop() {
        let dir = std::env::temp_dir().join(format!(
            "prisma-editor-control-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let server = ControlServer::start(&dir).unwrap();
        let port_file = dir.join("control.port");
        let port: u16 = std::fs::read_to_string(&port_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        writeln!(stream, "{{\"cmd\":\"PiNg\"}}").unwrap();

        let request = loop {
            match server.requests.try_recv() {
                Ok(request) => break request,
                Err(mpsc::TryRecvError::Empty) => std::thread::yield_now(),
                Err(mpsc::TryRecvError::Disconnected) => panic!("control queue disconnected"),
            }
        };
        assert_eq!(request.command, "ping");
        request.reply.send(json!({ "ok": true })).unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["ok"],
            true
        );

        drop(server);
        assert!(!port_file.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_and_missing_commands_return_errors_without_queueing() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let client = TcpStream::connect(address).unwrap();
        let (server_stream, _) = listener.accept().unwrap();
        std::thread::spawn(move || serve(server_stream, &tx));
        let mut writer = client;
        writeln!(writer, "{{bad").unwrap();
        writeln!(writer, "{{\"value\":1}}").unwrap();
        let mut reader = BufReader::new(writer.try_clone().unwrap());
        let mut first = String::new();
        let mut second = String::new();
        reader.read_line(&mut first).unwrap();
        reader.read_line(&mut second).unwrap();
        assert!(serde_json::from_str::<Value>(&first).unwrap()["error"]
            .as_str()
            .unwrap()
            .starts_with("bad json:"));
        assert_eq!(
            serde_json::from_str::<Value>(&second).unwrap()["error"],
            "missing cmd"
        );
        assert!(rx.try_recv().is_err());
    }
}
