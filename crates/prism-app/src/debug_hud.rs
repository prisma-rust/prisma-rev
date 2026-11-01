//! 运行时诊断 HUD（无 UI 框架依赖）。
//!
//! egui 编辑器移除后，帧率 / 渲染模式 / 调试状态的屏幕显示由本模块接管：
//! 它只向已有的 ECS UI 叠加（[`prism_render::UiOverlayInput`]）追加文本命令，
//! 由 `prism-render` 的 `ui_overlay` 通道统一光栅化，因此不需要任何外部 UI
//! 框架或额外渲染 pass。
//!
//! 快捷键（`prism-app` 处理）：
//! - `F3`：显示 / 隐藏 HUD。

use prism_engine::render_settings::{pbr_flag_label, RenderMode, RenderSettings};
use prism_render::ui_overlay::{UiTextAlign, UiTextInput};
use prism_render::UiOverlayInput;

use crate::render_shared::RenderStats;

const MARGIN: f32 = 10.0;
const LINE_HEIGHT: f32 = 20.0;
const FONT_SIZE: f32 = 15.0;
const TEXT_COLOR: [f32; 4] = [0.92, 0.92, 0.92, 0.95];
const WARN_COLOR: [f32; 4] = [1.0, 0.72, 0.3, 1.0];

/// 运行时 HUD 的显示状态。
#[derive(Clone, Copy, Debug, Default)]
pub struct DebugHud {
    /// `F3` 切换。
    pub show: bool,
}

impl DebugHud {
    /// 把当前帧的诊断文本追加到 UI 叠加。
    ///
    /// `width` / `height` 为交换链逻辑尺寸（像素），用于把 HUD 放在左上角
    /// 以及居中绘制「无相机」提示。
    pub fn append(
        &self,
        ui: &mut UiOverlayInput,
        stats: &RenderStats,
        settings: &RenderSettings,
        has_camera: bool,
        width: f32,
        height: f32,
    ) {
        if !self.show {
            return;
        }

        let mut lines = Vec::with_capacity(5);
        let mut line = format!("{:.1} ms  |  {:.0} FPS", stats.frame_time_ms, stats.fps);
        match stats.gpu_timings.as_ref().filter(|gpu| gpu.supported) {
            Some(gpu) => line.push_str(&format!("  |  GPU {:.1} ms", gpu.total_ms)),
            // 设备不支持时间戳：显式写 n/a，不拿 CPU 墙钟冒充 GPU 时间。
            None => line.push_str("  |  GPU n/a"),
        }
        if settings.render_mode == RenderMode::PathTrace {
            let max = if settings.pt_max_iterations > 0 {
                settings.pt_max_iterations.to_string()
            } else {
                "inf".to_string()
            };
            let samples = stats.pt_frame_count.unwrap_or(0);
            line.push_str(&format!("  |  PT {samples}/{max} smp"));
        }
        lines.push(line);

        let mode = match settings.render_mode {
            RenderMode::Raster => "Raster",
            RenderMode::PathTrace => "PathTrace",
        };
        let tonemap = match settings.tonemap_mode {
            0 => "Reinhard",
            1 => "ACES",
            _ => "Passthrough",
        };
        lines.push(format!(
            "{mode}  |  Tonemap: {tonemap}  |  PBR: {}",
            pbr_flag_label(settings.debug_flags)
        ));

        // 最贵的 3 个 pass：与渲染图面板同源（上一帧读回的 timestamp 结果）。
        if let Some(gpu) = stats.gpu_timings.as_ref().filter(|gpu| gpu.supported) {
            if !gpu.passes.is_empty() {
                let mut passes: Vec<_> = gpu.passes.iter().collect();
                passes.sort_by(|a, b| b.ms.total_cmp(&a.ms));
                let top: Vec<String> = passes
                    .iter()
                    .take(3)
                    .map(|pass| format!("{} {:.1}", pass.name, pass.ms))
                    .collect();
                let suffix = if passes.len() > top.len() { " …" } else { "" };
                lines.push(format!("GPU passes: {}{suffix}", top.join(" · ")));
            }
        }

        if settings.debug_rt != 0 {
            let target = match settings.debug_rt {
                1 => "depth",
                2 => "normal (view-space)",
                _ => "unknown",
            };
            lines.push(format!("Debug RT: {target}"));
        }

        if !has_camera {
            ui.push_text(UiTextInput {
                rect: [0.0, (height * 0.5 - 12.0).max(0.0), width, 24.0],
                content: "[ No camera ]".to_string(),
                font_size: 20.0,
                color: WARN_COLOR,
                alignment: UiTextAlign::Center,
                font: None,
                wrap: false,
            });
        }

        for (i, line) in lines.iter().enumerate() {
            ui.push_text(UiTextInput {
                rect: [
                    MARGIN,
                    MARGIN + i as f32 * LINE_HEIGHT,
                    (width - MARGIN * 2.0).max(1.0),
                    LINE_HEIGHT,
                ],
                content: line.clone(),
                font_size: FONT_SIZE,
                color: TEXT_COLOR,
                alignment: UiTextAlign::Left,
                font: None,
                wrap: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_render::gpu_timer::{GpuTimings, PassTiming};

    fn append(stats: &RenderStats) -> Vec<String> {
        let mut ui = UiOverlayInput::default();
        DebugHud { show: true }.append(
            &mut ui,
            stats,
            &RenderSettings::default(),
            true,
            800.0,
            600.0,
        );
        ui.texts.into_iter().map(|text| text.content).collect()
    }

    #[test]
    fn hud_shows_gpu_total_and_the_three_slowest_passes() {
        let stats = RenderStats {
            frame_time_ms: 16.0,
            fps: 60.0,
            pt_frame_count: None,
            gpu_timings: Some(GpuTimings {
                supported: true,
                total_ms: 12.5,
                passes: vec![
                    PassTiming {
                        name: "shadow".into(),
                        ms: 2.0,
                    },
                    PassTiming {
                        name: "forward".into(),
                        ms: 8.0,
                    },
                    PassTiming {
                        name: "post".into(),
                        ms: 0.5,
                    },
                    PassTiming {
                        name: "skybox".into(),
                        ms: 0.25,
                    },
                ],
            }),
        };

        let lines = append(&stats);
        assert!(lines[0].contains("GPU 12.5 ms"), "{lines:?}");
        assert_eq!(
            lines[2],
            "GPU passes: forward 8.0 · shadow 2.0 · post 0.5 …"
        );
    }

    #[test]
    fn hud_marks_gpu_timing_unavailable_instead_of_faking_it() {
        let stats = RenderStats::default();
        let lines = append(&stats);
        assert!(lines[0].contains("GPU n/a"), "{lines:?}");
        assert!(lines.iter().all(|line| !line.starts_with("GPU passes")));
    }
}
