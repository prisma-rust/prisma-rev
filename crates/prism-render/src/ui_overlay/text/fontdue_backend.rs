//! 文本光栅后端 A：`fontdue`（全平台，默认）。
//!
//! 字体有两条来源（#583）：
//!
//! * **系统默认字体**：启动时按 [`FONT_CANDIDATES`] 找第一个可用文件，或读
//!   `PRISMREV_UI_FONT`。找不到时不再直接放弃——工程可能自带字体，所以实例照常
//!   创建，只是 `font` 为空且没有注册字体时文字仍然被丢弃（quad 背景照常绘制）。
//! * **工程自带字体**：宿主把 `assets/fonts/*.ttf|otf` 的字节经
//!   [`TextRasterizer::register_font`] 注册进来，`Text.font` 写相对路径即可选用。
//!   某个路径没注册 / 解析失败时回落到系统默认字体，并只 warn 一次。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use fontdue::Font;

use super::{GlyphBitmap, LineMetrics, TextRasterizer};

/// 字体文件候选（按序尝试；ttc 用 collection_index 指定第一个字体）。
///
/// 优先挑带 CJK 的字体——intro 标题是中文。全部失败则回落到「只有注册的工程
/// 字体能用」，并 log::warn。
const FONT_CANDIDATES: &[(&str, u32)] = &[
    // Windows
    ("C:/Windows/Fonts/msyh.ttc", 0),   // 微软雅黑（含中文）
    ("C:/Windows/Fonts/simhei.ttf", 0), // 黑体（含中文）
    ("C:/Windows/Fonts/arial.ttf", 0),
    // Android
    ("/system/fonts/NotoSansCJK-Regular.ttc", 0),
    ("/system/fonts/NotoSansCJKsc-Regular.otf", 0),
    ("/system/fonts/DroidSansFallback.ttf", 0),
    ("/system/fonts/Roboto-Regular.ttf", 0),
    // Linux
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
    (
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        0,
    ),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
    // macOS
    ("/System/Library/Fonts/PingFang.ttc", 0),
    ("/System/Library/Fonts/Helvetica.ttc", 0),
];

/// Return the first candidate for which `load` produces a value.
///
/// Missing/unreadable candidates must not abort the search: only the first
/// *available* f
fn first_available<T>(
    candidates: &[(&str, u32)],
    mut load: impl FnMut(&str, u32) -> Option<T>,
) -> Option<(String, T)> {
    for &(path, collection_index) in candidates {
        if let Some(value) = load(path, collection_index) {
            return Some((path.to_string(), value));
        }
    }
    None
}

/// 加载第一个可用的系统字体（fontdue 只接受单字体；ttc 走 collection_index）。
///
/// `PRISMREV_UI_FONT` 可以显式指定字体文件：Android / 精简系统上候选路径可能都不
/// 存在，那时游戏要能自带一份字体，而不是默默丢掉所有文字。
fn load_system_font() -> Option<Font> {
    let load = |path: &str, collection_index: u32| {
        let data = std::fs::read(path).ok()?;
        Font::from_bytes(
            data,
            fontdue::FontSettings {
                collection_index,
                ..Default::default()
            },
        )
        .ok()
    };

    if let Some(path) = std::env::var_os("PRISMREV_UI_FONT") {
        let path = path.to_string_lossy().into_owned();
        match load(&path, 0) {
            Some(font) => {
                log::info!("ui overlay: loaded font {path} (PRISMREV_UI_FONT)");
                return Some(font);
            }
            // 显式指定的字体用不了就照常找候选字体，但必须说清楚，否则
            // 「字体没换」和「字体根本没加载」在日志里长得一样。
            None => log::warn!(
                "ui overlay: PRISMREV_UI_FONT={path} could not be loaded — falling back to system fonts"
            ),
        }
    }

    let (path, font) = first_available(FONT_CANDIDATES, load)?;
    log::info!("ui overlay: loaded font {path}");
    Some(font)
}

pub(crate) struct FontdueRasterizer {
    /// 系统默认字体；`None` 时只有注册过的工程字体能用。
    default: Option<Font>,
    /// 工程自带字体：资产路径（`assets/fonts/x.ttf`）→ 解析后的字体。
    fonts: HashMap<String, Font>,
    /// 已经 warn 过「这个路径没注册」的集合：`line_metrics` 是 `&self`，用
    /// `RefCell` 保证每帧查字形也不会刷日志。
    reported_missing: RefCell<HashSet<String>>,
}

impl FontdueRasterizer {
    pub(crate) fn new() -> Self {
        let default = load_system_font();
        if default.is_none() {
            // 不再直接放弃：工程可以在运行时注册自带字体（#583）。但如果整个
            // 生命周期都没注册，文字仍然一个都不画——这里必须留下线索。
            log::warn!(
                "ui overlay: no usable system UI font (tried {} candidates) — text falls back to \
                 project fonts registered via `assets/fonts/`; set PRISMREV_UI_FONT to override",
                FONT_CANDIDATES.len()
            );
        }
        Self {
            default,
            fonts: HashMap::new(),
            reported_missing: RefCell::new(HashSet::new()),
        }
    }

    /// 该路径的字体；找不到时 warn 一次并回落到默认字体。
    fn resolve(&self, font: Option<&str>) -> Option<&Font> {
        if let Some(name) = font {
            if let Some(found) = self.fonts.get(name) {
                return Some(found);
            }
            if self.reported_missing.borrow_mut().insert(name.to_string()) {
                log::warn!(
                    "ui overlay: font '{name}' is not registered (not in the .pak / failed to \
                     parse) — falling back to the default font"
                );
            }
        }
        self.default.as_ref()
    }
}

impl TextRasterizer for FontdueRasterizer {
    fn line_metrics(&self, font: Option<&str>, font_size: f32) -> Option<LineMetrics> {
        let font = self.resolve(font)?;
        let metrics = font.horizontal_line_metrics(font_size)?;
        Some(LineMetrics {
            ascent: metrics.ascent,
            // fontdue 的 descent 是负数（向下为负），这里统一成「基线下方的正距离」。
            descent: -metrics.descent,
            line_height: metrics.new_line_size,
        })
    }

    fn rasterize(&mut self, font: Option<&str>, ch: char, font_size: f32) -> Option<GlyphBitmap> {
        // 先解析出字体再可变借用 rasterize：`resolve` 只读。
        let found = self.resolve(font)?;
        let (metrics, pixels) = found.rasterize(ch, font_size);
        Some(GlyphBitmap {
            width: metrics.width,
            height: metrics.height,
            advance: metrics.advance_width,
            xmin: metrics.xmin,
            ymin: metrics.ymin,
            pixels,
        })
    }

    fn register_font(&mut self, name: &str, bytes: Vec<u8>) -> bool {
        match Font::from_bytes(bytes, fontdue::FontSettings::default()) {
            Ok(font) => {
                log::info!("ui overlay: registered project font {name}");
                self.fonts.insert(name.to_string(), font);
                // 之前可能因为「没注册」warn 过，现在能用了就别再报。
                self.reported_missing.borrow_mut().remove(name);
                true
            }
            Err(error) => {
                log::warn!("ui overlay: project font '{name}' could not be parsed: {error}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_lookup_skips_missing_candidates() {
        let candidates = [("missing", 0), ("broken", 0), ("ok", 2)];
        let found = first_available(&candidates, |path, index| match path {
            "ok" => Some(index),
            _ => None,
        });
        assert_eq!(found, Some(("ok".to_string(), 2)));
    }

    #[test]
    fn font_lookup_returns_none_when_all_candidates_fail() {
        let candidates = [("missing", 0), ("broken", 0)];
        assert!(first_available(&candidates, |_, _| None::<u32>).is_none());
    }

    #[test]
    fn registering_broken_bytes_fails_without_dropping_the_default() {
        let mut backend = FontdueRasterizer {
            default: None,
            fonts: HashMap::new(),
            reported_missing: RefCell::new(HashSet::new()),
        };
        assert!(!backend.register_font("assets/fonts/broken.ttf", vec![0, 1, 2, 3]));
        assert!(backend.fonts.is_empty());
        // 没注册的字体 + 没有默认字体 = 拿不到字形，而不是 panic。
        assert!(backend
            .rasterize(Some("assets/fonts/broken.ttf"), 'a', 16.0)
            .is_none());
        assert!(backend
            .line_metrics(Some("assets/fonts/broken.ttf"), 16.0)
            .is_none());
    }

    #[test]
    fn a_registered_font_replaces_the_previous_bytes() {
        // 用仓库里真实存在的 TTF 验证注册路径（找不到就跳过，不把测试绑死在
        // 某个系统字体上）。
        let Some(bytes) = FONT_CANDIDATES
            .iter()
            .find_map(|(path, _)| std::fs::read(path).ok())
        else {
            return;
        };
        let mut backend = FontdueRasterizer {
            default: None,
            fonts: HashMap::new(),
            reported_missing: RefCell::new(HashSet::new()),
        };
        assert!(backend.register_font("assets/fonts/hud.ttf", bytes.clone()));
        assert!(backend
            .line_metrics(Some("assets/fonts/hud.ttf"), 24.0)
            .is_some());
        assert!(backend
            .rasterize(Some("assets/fonts/hud.ttf"), 'A', 24.0)
            .is_some());
        // 同一路径重新注册：直接覆盖，不报错。
        assert!(backend.register_font("assets/fonts/hud.ttf", bytes));
    }
}
