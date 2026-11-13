//! PrismaRev 用户游戏项目入口。
//!
//! 引擎初始化、事件循环、渲染全部由 [`prism_app::app`] 包办；本项目的
//! 代码通过完全 ECS 的方式接入：
//! - `register_scene`：注册启动场景的 ECS 实体与资源；
//! - `add_scene_system`：注册场景的 ECS system；
//! - `run()`：启动事件循环，引擎自动按 `PRISMREV_LAUNCH_CONFIG` 调度场景。
//!
//! 桌面端由 launcher/ 以 `template` 二进制 spawn（hub 模式）；可读
//! `PRISMREV_LAUNCH_CONFIG` env 覆盖启动配置（见 `LaunchConfig`）。
//! Android 入口在 `lib.rs`（cdylib `libtemplate_core.so`）。

#![deny(warnings)]

fn main() {
    // 构造应用（完成引擎初始化），注册 intro 场景后启动。
    template_core::build_app().run();
}
