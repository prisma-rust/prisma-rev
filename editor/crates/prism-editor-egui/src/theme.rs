//! 主题：把数据层的 token（Web 编辑器同一套）映射到 egui。
//!
//! 颜色只在数据层定义一次，egui 侧不写死色值（与 Web 编辑器的
//! `style.css` 保持一致）。
//!
//! 视觉规则（对齐 `editor/src/style.css`）：
//! - 表面是**中性灰**：菜单/状态栏 `chrome`、面板 `panel`、标签栏 `bg-sunken`、
//!   浮动窗口 `panel-raised`；蓝色只出现在**选中/按下/焦点**（`selection` /
//!   `accent`），不要给普通控件铺蓝底。
//! - 直角（`CornerRadius::ZERO`）为主，只有弹出菜单 3px；1px 分隔线用 `border`。
//! - 控件不做 hover 放大（`expansion = 0`），桌面 / Web 都不抖。

use egui::{Color32, CornerRadius, FontId, Stroke};
use prism_editor_core::{ThemeColors, ThemeToken};

pub fn color32(colors: &ThemeColors, token: ThemeToken) -> Color32 {
    let rgba = colors.color(token);
    Color32::from_rgba_unmultiplied(rgba.r, rgba.g, rgba.b, rgba.a)
}

const RADIUS: CornerRadius = CornerRadius::ZERO;
const POPUP_RADIUS: CornerRadius = CornerRadius::same(3);

/// 应用一套紧凑的编辑器视觉（深色、密排、零圆角）。
pub fn apply(ctx: &egui::Context, colors: &ThemeColors) {
    let panel = color32(colors, ThemeToken::Panel);
    let panel_alt = color32(colors, ThemeToken::PanelAlt);
    let panel_raised = color32(colors, ThemeToken::PanelRaised);
    let sunken = color32(colors, ThemeToken::BgSunken);
    let border = color32(colors, ThemeToken::Border);
    let border_strong = color32(colors, ThemeToken::BorderStrong);
    let border_soft = color32(colors, ThemeToken::BorderSubtle);
    let text = color32(colors, ThemeToken::Text);
    let text_strong = color32(colors, ThemeToken::TextStrong);
    let text_soft = color32(colors, ThemeToken::TextSoft);
    let muted = color32(colors, ThemeToken::Muted);
    let row_hover = color32(colors, ThemeToken::RowHover);
    let menu_hover = color32(colors, ThemeToken::MenuHover);
    let menu_active = color32(colors, ThemeToken::MenuActive);
    let accent = color32(colors, ThemeToken::Accent);
    let accent_solid = color32(colors, ThemeToken::AccentSolid);
    let selection = color32(colors, ThemeToken::Selection);
    let on_accent = color32(colors, ThemeToken::OnAccent);

    let mut visuals = egui::Visuals::dark();

    // ---- 表面 ----
    visuals.panel_fill = panel;
    visuals.window_fill = panel_raised;
    visuals.extreme_bg_color = sunken;
    visuals.faint_bg_color = panel_alt;
    visuals.code_bg_color = sunken;
    visuals.window_stroke = Stroke::new(1.0_f32, border_strong);
    visuals.window_corner_radius = POPUP_RADIUS;
    visuals.menu_corner_radius = POPUP_RADIUS;
    visuals.window_shadow = egui::Shadow::NONE;
    // Web 的 `.menu-popup` 阴影：0 6px 18px rgba(0,0,0,.55)
    visuals.popup_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    visuals.striped = false;
    visuals.override_text_color = Some(text);
    visuals.hyperlink_color = color32(colors, ThemeToken::AccentAlt);
    visuals.text_cursor = egui::style::TextCursorStyle {
        stroke: Stroke::new(1.0_f32, text_strong),
        ..Default::default()
    };
    visuals.selection.bg_fill = selection;
    visuals.selection.stroke = Stroke::NONE;
    visuals.slider_trailing_fill = true;

    // ---- 控件 ----
    // 注意：egui 默认的 `weak_bg_fill`（按钮 / 可选中标签）是偏蓝的灰，
    // 这里全部换成 token 里的中性灰；蓝色只在 `active`（按下 / 选中）出现。
    let widgets = &mut visuals.widgets;

    widgets.noninteractive.bg_fill = panel;
    widgets.noninteractive.weak_bg_fill = panel;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text_soft);
    widgets.noninteractive.corner_radius = RADIUS;
    widgets.noninteractive.expansion = 0.0;

    widgets.inactive.bg_fill = panel_raised;
    widgets.inactive.weak_bg_fill = panel_raised;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, border_strong);
    widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text);
    widgets.inactive.corner_radius = RADIUS;
    widgets.inactive.expansion = 0.0;

    widgets.hovered.bg_fill = menu_hover;
    widgets.hovered.weak_bg_fill = menu_hover;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, border_strong);
    widgets.hovered.fg_stroke = Stroke::new(1.0_f32, text_strong);
    widgets.hovered.corner_radius = RADIUS;
    widgets.hovered.expansion = 0.0;

    widgets.active.bg_fill = accent_solid;
    widgets.active.weak_bg_fill = accent_solid;
    widgets.active.bg_stroke = Stroke::new(1.0_f32, accent_solid);
    widgets.active.fg_stroke = Stroke::new(1.0_f32, on_accent);
    widgets.active.corner_radius = RADIUS;
    widgets.active.expansion = 0.0;

    widgets.open.bg_fill = menu_active;
    widgets.open.weak_bg_fill = menu_active;
    widgets.open.bg_stroke = Stroke::new(1.0_f32, border_strong);
    widgets.open.fg_stroke = Stroke::new(1.0_f32, text_strong);
    widgets.open.corner_radius = RADIUS;
    widgets.open.expansion = 0.0;

    ctx.set_visuals(visuals);

    // ---- 度量 ----
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (egui::TextStyle::Small, FontId::proportional(10.0)),
        (egui::TextStyle::Body, FontId::proportional(12.0)),
        (egui::TextStyle::Button, FontId::proportional(12.0)),
        (egui::TextStyle::Heading, FontId::proportional(13.0)),
        (egui::TextStyle::Monospace, FontId::monospace(11.0)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(5.0, 3.0);
    style.spacing.button_padding = egui::vec2(7.0, 2.0);
    style.spacing.menu_margin = egui::Margin::same(3);
    style.spacing.indent = 12.0;
    style.spacing.interact_size.y = 20.0;
    style.spacing.combo_width = 160.0;
    style.spacing.scroll.floating = false;
    // 默认 handle 取 `fg_stroke`（正文灰）→ 滚动条会亮成白色；改用 `bg_fill`。
    style.spacing.scroll.foreground_color = false;
    style.spacing.scroll.bar_width = 9.0;
    style.spacing.scroll.bar_inner_margin = 4.0;
    style.spacing.scroll.bar_outer_margin = 0.0;
    style.spacing.scroll.handle_min_length = 20.0;
    style.animation_time = 0.06;
    let _ = (muted, row_hover, accent, border_soft);
    ctx.set_style(style);
}

/// 顶部 / 底部 chrome 条（菜单栏、工具栏、状态栏）的底：`chrome` 填充。
pub fn chrome_fill(colors: &ThemeColors) -> Color32 {
    color32(colors, ThemeToken::Chrome)
}

/// 普通面板的底（与 egui 的 `panel_fill` 一致）。
pub fn panel_fill(colors: &ThemeColors) -> Color32 {
    color32(colors, ThemeToken::Panel)
}

/// 1px 分隔线 / 边框色。
pub fn border(colors: &ThemeColors) -> Color32 {
    color32(colors, ThemeToken::Border)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_installs_editor_visuals_and_metrics() {
        let ctx = egui::Context::default();
        let colors = ThemeColors::preset("studio");
        apply(&ctx, &colors);
        let visuals = ctx.style().visuals.clone();
        assert_eq!(visuals.window_corner_radius, CornerRadius::same(3));
        assert_eq!(visuals.widgets.inactive.corner_radius, CornerRadius::ZERO);
        assert_eq!(ctx.style().spacing.interact_size.y, 20.0);
        assert_eq!(panel_fill(&colors), color32(&colors, ThemeToken::Panel));
        assert_eq!(chrome_fill(&colors), color32(&colors, ThemeToken::Chrome));
    }

    #[test]
    fn font_installation_is_safe_without_system_font_files() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        assert!(ctx.fonts(|fonts| !fonts.families().is_empty()));
    }
}

/// 字体：优先用系统 UI 字体（贴近 Web 编辑器的系统无衬线），
/// 再追加一个 CJK 回退（egui 自带字体没有中文）。
///
/// 都找不到时保留 egui 的内置字体，不影响功能。
pub fn install_fonts(ctx: &egui::Context) {
    if std::env::var("PRISMREV_EDITOR_NO_CJK").is_ok() {
        return;
    }

    const UI_SANS: &[&str] = &[
        // Windows
        "C:/Windows/Fonts/segoeui.ttf",
        // Linux（Web 编辑器用系统 UI 字体，这里挑最接近的）
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        // macOS
        "/System/Library/Fonts/Helvetica.ttc",
    ];
    const MONO: &[&str] = &[
        "C:/Windows/Fonts/consola.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        "/System/Library/Fonts/Menlo.ttc",
    ];
    const CJK: &[&str] = &[
        // Android / Termux
        "/system/fonts/NotoSansCJK-Regular.ttc",
        "/system/fonts/NotoSansCJKsc-Regular.otf",
        "/system/fonts/DroidSansFallback.ttf",
        // Linux
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        // Windows
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        // macOS
        "/System/Library/Fonts/PingFang.ttc",
    ];

    fn first_existing(paths: &'static [&'static str]) -> Option<(&'static str, Vec<u8>)> {
        paths
            .iter()
            .find_map(|path| std::fs::read(path).ok().map(|bytes| (*path, bytes)))
    }

    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = 0;
    // Phosphor Icons（MIT/Apache-2.0）作为图标字形，插在正文 UI 字体后面，
    // 这样 `PLAY` / `EYE` / `FOLDER` 这些常量都能按文本直接画。
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    if let Some((path, bytes)) = first_existing(UI_SANS) {
        fonts.font_data.insert(
            "ui-sans".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "ui-sans".to_owned());
        loaded += 1;
        log::info!("UI sans font: {path}");
    }
    if let Some((path, bytes)) = first_existing(MONO) {
        fonts.font_data.insert(
            "ui-mono".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "ui-mono".to_owned());
        loaded += 1;
        log::info!("UI mono font: {path}");
    }
    if let Some((path, bytes)) = first_existing(CJK) {
        fonts.font_data.insert(
            "cjk".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_owned());
        }
        loaded += 1;
        log::info!("CJK fallback font: {path}");
    }

    if loaded == 0 {
        log::debug!("no system fonts found; using egui built-in fonts");
    }
    ctx.set_fonts(fonts);
}
