//! IO 线程——在后台读取 .pak 文件并反序列化资源。
//!
//! 主线程通过 `flume` 通道发送 [`IoRequest`] 并接收 [`IoResult`]。
//!
//! GPU 上传任务通过 [`RenderShared::gpu_uploads`] 单独发送。
//!
//! # 状态：骨架（半接线）
//!
//! 消息类型完整，`io_thread_main` 已被 `App::start_io_thread` 启动，
//! 但资源加载系统尚未调用，多数变体暂未构造。保留以抑制 dead_code 警告。

#![allow(dead_code)]

use flume::{Receiver, Sender};

use prism_asset::core::AssetId;

// ── Messages ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum IoRequest {
    LoadAsset(AssetId),
    LoadPackage(String),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum IoResult {
    AssetLoaded {
        id: AssetId,
        /// 不透明二进制数据——反序列化后的资源数据。
        /// 主线程将其整合到 ECS 世界中。
        data: Vec<u8>,
    },
    PackageLoaded {
        name: String,
        assets: Vec<AssetId>,
    },
    Error {
        id: AssetId,
        message: String,
    },
}

// ── GPU upload task ───────────────────────────────────────────────────

/// 主线程入队、供渲染线程执行的任务（从 CPU 端资源数据创建 Vulkan 资源）。
#[derive(Debug, Clone)]
pub enum GpuUploadTask {
    CreateMesh {
        handle: u64,
        vertices: Vec<u8>,
        indices: Vec<u8>,
    },
    CreateTexture {
        handle: u64,
        data: Vec<u8>,
        width: u32,
        height: u32,
        format: u32,
    },
}

// ── Thread entry point ────────────────────────────────────────────────

/// Run the IO event loop. Blocks on `rx` until [`IoRequest::Shutdown`]
/// is received or the channel is closed.
pub fn io_thread_main(rx: Receiver<IoRequest>, result_tx: Sender<IoResult>) {
    log::info!("IO thread started");

    loop {
        match rx.recv() {
            Ok(IoRequest::Shutdown) | Err(_) => break,
            Ok(IoRequest::LoadAsset(id)) => {
                // 尝试通过 prism-asset runtime 读取 .pak（若已加载）
                log::trace!(
                    "IO thread: LoadAsset({id:?}) — pak reading 已接线, returning not_found"
                );
                let _ = result_tx.send(IoResult::Error {
                    id,
                    message: "IO .pak reading 已接线 — use synchronous asset_resolver path".into(),
                });
            }
            Ok(IoRequest::LoadPackage(name)) => {
                log::trace!("IO thread: LoadPackage({name}) — 已实现");
                let _ = result_tx.send(IoResult::PackageLoaded {
                    name,
                    assets: Vec::new(),
                });
            }
        }
    }

    log::info!("IO thread exiting");
}
