//! PrismaRev 编辑器（egui + ash Vulkan）。
//!
//! 与 Web 编辑器（Tauri + Vue）共享同一份数据层（`prism-editor-core`），
//! 与 GPUI 编辑器共享同一套布局 / 文档 / 撤销模型。关键差异在视口：
//! Scene / Game 视图**零拷贝** —— 引擎渲染到离屏 `VkImage`，egui 用同一个
//! `VkDevice` 直接采样，没有 GPU→CPU 回读。
//!
//! 运行：`cargo run -p prism-editor-egui`（需要 Vulkan 设备 + 已编译的 `.spv`）。

mod app;
mod build;
mod control;
mod diagnostics;
mod engine_host;
mod gpu;
mod logging;
mod loose_assets;
mod palette;
mod panels;
mod project_module;
mod screenshot;
mod theme;
mod ui_tree;
mod viewport;
mod widgets;

use egui_ash::{AppCreator, AshRenderState, CreationContext, RunOption};
use gpu_allocator::vulkan::Allocator;
use std::sync::{Arc, Mutex};

struct EditorCreator;

impl AppCreator<Arc<Mutex<Allocator>>> for EditorCreator {
    type App = app::EditorApp;

    fn create(&self, cc: CreationContext) -> (Self::App, AshRenderState<Arc<Mutex<Allocator>>>) {
        let app = match app::EditorApp::new(&cc) {
            Ok(app) => app,
            Err(error) => {
                panic!("failed to create PrismaRev editor: {error:#}");
            }
        };
        let render_state = app.render_state();
        (app, render_state)
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("prism-editor-egui {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }

    logging::install("warn");

    let options = RunOption {
        clear_color: [0.055, 0.06, 0.07, 1.0],
        viewport_builder: Some(
            egui::ViewportBuilder::default()
                .with_title("PrismaRev Editor")
                .with_inner_size(egui::vec2(1440.0, 900.0))
                .with_min_inner_size(egui::vec2(900.0, 600.0)),
        ),
        ..Default::default()
    };

    egui_ash::run("prism-editor-egui", EditorCreator, options)
}
