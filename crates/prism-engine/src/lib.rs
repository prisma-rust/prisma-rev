//! PrismaRev 引擎库：渲染系统、ECS 组件、资源解析、
//! 场景管理和编辑器集成。
//!
//! 这是一个**纯逻辑 crate**——不依赖任何窗口系统。
//! 事件循环、窗口创建和平台相关的输入分发位于 `src/app.rs`（二进制 crate）中。

#![deny(warnings)]
#![allow(clippy::all)]

pub mod anim_ground;
pub mod app;
pub mod asset;
pub mod asset_paths;
pub mod asset_resolver;
#[cfg(feature = "audio")]
pub mod audio;
pub mod calibration_spheres;
pub mod camera;
pub mod camera_controller;
pub mod config;
pub mod crash_dialog;
pub mod dirty_router;
pub mod ecs;
pub mod engine;
pub mod input;
pub mod input_facade;
pub mod launch_config;
pub mod physics;
pub mod physics2d;
#[cfg(test)]
mod physics2d_tests;
pub mod render_settings;
pub mod render_system;
pub mod scene;
pub mod scene_state;
pub mod sound;
pub mod ui;
pub mod user_data;
pub mod util;

pub use app::{AppBuilder, DefaultSubsystems, ScheduleLabel, Subsystem, System};
pub use engine::{orbit_camera_demo_system, Engine};
pub use input_facade::Input;
pub use render_system::{euler_xyz_deg_to_dir, render_system};
