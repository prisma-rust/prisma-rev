//! 离线 pass-dump 跑批器：不创建窗口、不推进模拟，按固定帧数跑离屏渲染，
//! 并让渲染图在指定帧把各个 pass 的附件写成 PNG。
//!
//! 用法：
//! ```bash
//! cd projects/sponza
//! PRISMREV_PASS_DUMP=target/diag/passes \
//! PRISMREV_PASS_DUMP_FRAME=200 \
//! SPONZA_RENDER_MODE=pt_off \
//! cargo run --manifest-path ../../crates/prism-app/Cargo.toml \
//!     --example pass_dump -- assets/scenes/sponza.scene.json 220
//! ```
//!
//! 参数：`<scene.json> [frames]`，`frames` 默认取 `PRISMREV_PASS_DUMP_FRAME`
//! 之后再多几帧（保证 dump 帧一定被跑到）。
//!
//! 与 `PRISMREV_SCREENSHOT` 的区别：那条路径只能在有 swapchain 的窗口化运行里
//! 工作（headless 下会报 `screenshot: no swapchain`），而且只拿最终后处理结果。
//! 这里 dump 的是渲染图的中间附件，正是定位「相机是否看到几何体」「PT 有没有
//! 出图」这类问题需要的证据。

use std::path::Path;
use std::time::Duration;

use prism_app::App;
use prism_engine::config::AppConfig;
use prism_engine::render_settings::{GiMode, RenderMode, RenderSettings};

/// Render mode / GI mode from `PRISM_RENDER_MODE` (the same knob `projects/*`
/// use, spelled with a neutral prefix because this example is shared):
/// `raster` → raster + GI off; `pt_off` → path trace + GI off; anything else
/// (including unset) → path trace + SHARC, matching the Sponza project.
fn configure(settings: &mut RenderSettings) {
    let mode = std::env::var("PRISM_RENDER_MODE")
        .or_else(|_| std::env::var("SPONZA_RENDER_MODE"))
        .unwrap_or_default();
    if mode.eq_ignore_ascii_case("raster") {
        settings.render_mode = RenderMode::Raster;
        settings.gi_mode = GiMode::Off;
    } else if mode.eq_ignore_ascii_case("pt_off") {
        settings.render_mode = RenderMode::PathTrace;
        settings.gi_mode = GiMode::Off;
    } else if mode.eq_ignore_ascii_case("sharc") {
        settings.render_mode = RenderMode::PathTrace;
        settings.gi_mode = GiMode::Sharc;
    }
    settings.pt_max_bounces = 4;
    settings.pt_max_iterations = 0;
    settings.pt_ray_max_distance = 1000.0;
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let scene = args
        .next()
        .unwrap_or_else(|| "assets/scenes/sponza.scene.json".to_string());

    let dump_frame: u32 = std::env::var("PRISMREV_PASS_DUMP_FRAME")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let frames: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(dump_frame + 20);

    let width: u32 = std::env::var("WIDTH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1280);
    let height: u32 = std::env::var("HEIGHT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(720);

    if std::env::var("PRISMREV_PASS_DUMP").is_err() {
        log::warn!(
            "PRISMREV_PASS_DUMP is unset; this run renders {} frames and dumps nothing",
            frames
        );
    }

    let mut app = App::hosted(AppConfig::default()).with_render_settings(configure);
    app.load_scene_json_file(Path::new(&scene))
        .map_err(|error| anyhow::anyhow!("load scene '{scene}': {error}"))?;
    app.start_hosted(width, height);

    // `host_tick(false)` 跳过 fixed_update / update / late_update，只跑层级传播
    // 和帧提取：相机、光照、变换都停在作者写入的值上，帧与帧之间没有模拟漂移。
    let mut last_report = 0u32;
    for index in 0..frames.max(1) {
        app.host_tick(false);
        for _ in 0..100 {
            if let Some(failure) = app.render_failure() {
                anyhow::bail!("render thread failed: {failure}");
            }
            if app.take_hosted_frame().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if index + 1 >= last_report + 50 {
            last_report = index + 1;
            log::info!("pass_dump: {} / {frames} frames", index + 1);
        }
    }

    app.shutdown_hosted();

    match std::env::var("PRISMREV_PASS_DUMP") {
        Ok(dir) if !dir.trim().is_empty() => println!(
            "pass dump: frames={frames} dir={} subdir=frame_{dump_frame:04}",
            dir.trim()
        ),
        _ => println!("pass dump: disabled (PRISMREV_PASS_DUMP unset)"),
    }
    Ok(())
}
