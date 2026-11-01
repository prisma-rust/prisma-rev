//! 文本光栅后端：**谁把字符变成覆盖度位图**。
//!
//! `ui_overlay/gpu.rs` 里的图集打包、四边形展开、Vulkan 上传与后端无关；这里只定义
//! 后端接口（[`TextRasterizer`]）与两个实现：
//!
//! | feature | 后端 | 平台 | 说明 |
//! |---|---|---|---|
//! | `ui-fontdue`（默认） | `fontdue_backend` | 全平台 | 自带 ttf-parser，直接读字体文件（约 90 KB） |
//! | `ui-directwrite` | `directwrite` | 仅 Windows | 系统 `IDWriteFactory` + `IDWriteBitmapRenderTarget`，不链接 fontdue |
//!
//! 两个后端必须产出**同一套坐标系与度量**，否则切换 feature 会让文本跳动：
//!
//! * `advance` / `xmin`：笔位置向右的推进量与位图左边缘相对笔位置的偏移（px）。
//! * `ymin`：位图**底边**相对基线的偏移，**向上为正**（与 `fontdue::Metrics::ymin` 同义）。
//!   `UiOverlay::emit_text` 直接把 `baseline + ymin` 当成四边形顶边，所以两个后端
//!   必须同样解释这个字段，视觉结果才一致。
//! * [`LineMetrics`]：基线到行顶 / 行底 / 行高（px），`emit_text` 用 [`baseline_offset`]
//!   从 `ascent` 推出基线（不再用 `0.8 × font_size` 的硬编码近似）。
//!
//! 字体来源（#583）：
//!
//! * `font = None`（组件里 `font` 为空）= 系统默认字体（`PRISMREV_UI_FONT` /
//!   内置候选路径）。
//! * `font = Some(path)` = 工程自带字体，由宿主用 [`TextRasterizer::register_font`]
//!   注册过字节；找不到（未注册 / 解析失败）时回落到系统默认字体并只 warn 一次。
//! * 图集按 `(字体 id, 字符, 字号)` 分桶，多个字体共用同一张 R8 图集，所以混合
//!   多种字体的 UI 仍然只有一次 draw call。
//!
//! 明确**不做**的事（两个后端都只画单字形，不进复杂文本管线）：
//!
//! * 阿拉伯语 / 印度语系的连写与上下文替换（需要 `IDWriteTextLayout` 的 shaping 或
//!   HarfBuzz）：两个后端都只按 `cmap` 取单字形，这类文本形状不正确。
//! * 可变字体（axis 插值）、Emoji 彩色字形、连字（ligature）、字距调整（kerning）。

/// 一个已光栅化字形的覆盖度位图（8-bit alpha，行优先，长度 = `width * height`）。
pub(crate) struct GlyphBitmap {
    pub width: usize,
    pub height: usize,
    /// 笔位置推进量（px）。
    pub advance: f32,
    /// 位图左边缘相对笔位置的整像素偏移（可为负）。
    pub xmin: i32,
    /// 位图**底边**相对基线的整像素偏移，向上为正（见模块文档）。
    pub ymin: i32,
    pub pixels: Vec<u8>,
}

/// 行度量（px，全部为「距离」，不带符号）。
pub(crate) struct LineMetrics {
    /// 基线 → 行顶。
    pub ascent: f32,
    /// 基线 → 行底。
    pub descent: f32,
    /// 行高（含 line gap）。
    pub line_height: f32,
}

/// 单行文本的基线相对文本矩形顶边的偏移。
///
/// 有字体行度量时用 `ascent`；拿不到度量（字体缺失 / 度量 API 失败）时退回历史上的
/// `0.8 × font_size` 近似，保证不会因为度量缺失把文字画出矩形。
pub(crate) fn baseline_offset(metrics: Option<LineMetrics>, font_size: f32) -> f32 {
    match metrics {
        Some(metrics) if metrics.ascent > 0.0 => metrics.ascent,
        // 没有度量时保持旧行为（`emit_text` 一直用这个近似）。
        _ => font_size * 0.8,
    }
}

/// 一个文本光栅后端。
///
/// 实现必须可增量调用：`rasterize` 只在图集未命中时调用一次，结果由图集缓存。
pub(crate) trait TextRasterizer {
    /// 行度量（px）。`None` = 该字号不可用（调用方退回 [`baseline_offset`] 的近似）。
    ///
    /// `font` 是工程自带字体的资产路径；`None` 或未注册时用系统默认字体。
    fn line_metrics(&self, font: Option<&str>, font_size: f32) -> Option<LineMetrics>;

    /// 光栅化单个字符。空白字形（如空格）返回 `width == height == 0`，
    /// 但仍然带正确的 `advance`。
    fn rasterize(&mut self, font: Option<&str>, ch: char, font_size: f32) -> Option<GlyphBitmap>;

    /// 注册工程自带字体（`.ttf` / `.otf` / `.ttc` 的原始字节，#583）。
    ///
    /// 返回 `false` 表示该后端不支持多字体或字节解析失败；调用方照常继续，
    /// 文本会回落到系统默认字体。
    fn register_font(&mut self, _name: &str, _bytes: Vec<u8>) -> bool {
        false
    }
}

#[cfg(all(windows, feature = "ui-directwrite"))]
mod directwrite;
#[cfg(feature = "ui-fontdue")]
mod fontdue_backend;
// 纯像素运算，平台无关（Linux 上跑单测；非 Windows 构建里没有调用方）。
#[allow(dead_code)]
mod coverage;

/// 按编译进来的 feature 构造后端；`None` = 只画 quad、不画文字。
///
/// fontdue 后端即使找不到系统字体也会返回实例：工程自带字体是运行时注册的
/// （#583），不能因为在没有系统字体的机器上启动就把后端永久关掉。
#[cfg(feature = "ui-fontdue")]
pub(crate) fn create() -> Option<Box<dyn TextRasterizer>> {
    Some(Box::new(fontdue_backend::FontdueRasterizer::new()) as Box<dyn TextRasterizer>)
}

#[cfg(all(windows, feature = "ui-directwrite", not(feature = "ui-fontdue")))]
pub(crate) fn create() -> Option<Box<dyn TextRasterizer>> {
    directwrite::DirectWriteRasterizer::new()
        .map(|backend| Box::new(backend) as Box<dyn TextRasterizer>)
}

#[cfg(not(any(feature = "ui-fontdue", all(windows, feature = "ui-directwrite"))))]
pub(crate) fn create() -> Option<Box<dyn TextRasterizer>> {
    log::warn!(
        "ui overlay: 没有编译进文本光栅后端（需要 feature `ui-fontdue` 或 `ui-directwrite`）\
         —— UI 只画背景四边形，文字命令被忽略"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_uses_ascent_when_available() {
        let metrics = LineMetrics {
            ascent: 22.0,
            descent: 6.0,
            line_height: 28.0,
        };
        assert_eq!(baseline_offset(Some(metrics), 24.0), 22.0);
    }

    #[test]
    fn baseline_falls_back_to_legacy_approximation() {
        // 以前是硬编码 `font_size * 0.8`：没有度量时必须保持这个值。
        assert_eq!(baseline_offset(None, 24.0), 24.0f32 * 0.8);
        let broken = LineMetrics {
            ascent: 0.0,
            descent: 0.0,
            line_height: 0.0,
        };
        assert_eq!(baseline_offset(Some(broken), 24.0), 24.0f32 * 0.8);
    }
}
