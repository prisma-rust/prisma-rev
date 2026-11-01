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

        let mut lines = Vec::with_capacity(4);
        let mut line = format!("{:.1} ms  |  {:.0} FPS", stats.frame_time_ms, stats.fps);
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
            });
        }
    }
}
