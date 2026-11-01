/// UI 叠加——屏幕空间四边形 + 字形四边形（数据层）。
///
/// GPU 部分（字体图集、管线、绘制）在 `ui_overlay_gpu.rs`，由 `ui` feature 控制。
/// 关闭 `ui` 时本模块只剩输入数据结构，引擎照常填充、渲染侧忽略。
//
// A filled rectangle in NDC 空间

#[derive(Clone, Debug)]
pub struct UiQuad {
    pub rect: [f32; 4],
    pub color: [f32; 4],
    pub border_radius: f32,
}

/// 文本绘制命令 — screen-space pixel 矩形 (top-left origin) + 内容.
/// 开启 `ui` feature 时由 GPU 侧的 `UiOverlay::record` 展开成逐字形四边形。
#[derive(Clone, Debug)]
pub struct UiTextInput {
    /// 像素矩形 `[left, top, width, height]`（布局系统输出）。
    pub rect: [f32; 4],
    pub content: String,
    pub font_size: f32,
    pub color: [f32; 4],
    pub alignment: UiTextAlign,
    /// 超宽时按 `rect[2]` 换行（`Text::wrap`，#527）。`false` 保持旧的单行行为。
    pub wrap: bool,
}

impl Default for UiTextInput {
    fn default() -> Self {
        Self {
            rect: [0.0; 4],
            content: String::new(),
            font_size: 16.0,
            color: [1.0; 4],
            alignment: UiTextAlign::Left,
            wrap: false,
        }
    }
}

/// 文本对齐（在 record 时按整串测量宽度决定首字符 x）。
#[derive(Clone, Copy, Debug, Default)]
pub enum UiTextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// 绘制顺序里的一项：引用 [`UiOverlayInput::quads`] / [`UiOverlayInput::texts`]。
///
/// 引擎按 UI 层级算好顺序后依次填入，渲染侧照着画——矩形和文本因此可以交错
/// （一个全屏遮罩能盖住它前面的文字，见 #462）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiOverlayDraw {
    Quad(u32),
    Text(u32),
}

/// Per‑frame 输入 from the engine.
#[derive(Clone, Default)]
pub struct UiOverlayInput {
    pub quads: Vec<UiQuad>,
    pub texts: Vec<UiTextInput>,
    /// 交错绘制顺序。为空时退化为「先全部矩形、再全部文本」（旧行为）。
    pub order: Vec<UiOverlayDraw>,
}

impl UiOverlayInput {
    pub fn clear(&mut self) {
        self.quads.clear();
        self.texts.clear();
        self.order.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.quads.is_empty() && self.texts.is_empty()
    }

    /// 追加一个矩形，并登记它在绘制顺序里的位置。
    pub fn push_quad(&mut self, quad: UiQuad) {
        self.order
            .push(UiOverlayDraw::Quad(self.quads.len() as u32));
        self.quads.push(quad);
    }

    /// 追加一条文本，并登记它在绘制顺序里的位置。
    pub fn push_text(&mut self, text: UiTextInput) {
        self.order
            .push(UiOverlayDraw::Text(self.texts.len() as u32));
        self.texts.push(text);
    }
}

// ---------------------------------------------------------------------------
// 文本断行（#527）
// ---------------------------------------------------------------------------

/// 多行断行结果：`(start, end)` 是指向字符数组的下标区间（`end` 不含）。
pub type LineRange = (usize, usize);

/// 按像素宽度断行，`\n` 始终是硬换行。
///
/// * `chars` 是 `(字符, 前进量)`，与文本逐字符对齐；
/// * `wrap == false` 时只按 `\n` 断（保持旧的单行语义，但不再把换行当缺字形丢掉）；
/// * `wrap == true` 且某行超过 `max_width` 时，在**最后一个空格**处断；
///   单词本身比整行还宽时按字符硬断（不会出现空行或被吃掉的字符）。
pub fn wrap_line_ranges(chars: &[(char, f32)], max_width: f32, wrap: bool) -> Vec<LineRange> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut width = 0.0f32;
    let mut last_space: Option<usize> = None;
    let can_wrap = wrap && max_width > 0.0;
    let advance_of = |index: usize| -> f32 {
        let advance = chars[index].1;
        if advance.is_finite() {
            advance
        } else {
            0.0
        }
    };

    for (index, (ch, _)) in chars.iter().enumerate() {
        if *ch == '\n' {
            lines.push((start, index));
            start = index + 1;
            width = 0.0;
            last_space = None;
            continue;
        }
        let advance = advance_of(index);
        if can_wrap && index > start && width + advance > max_width {
            // 断点优先级：当前字符是空格 → 就在这里断；否则退回最后一个空格；
            // 单词本身比整行还宽时按字符硬断。
            let break_at = if *ch == ' ' {
                index
            } else {
                last_space.filter(|space| *space > start).unwrap_or(index)
            };
            lines.push((start, break_at));
            start = if *ch == ' ' || break_at == index {
                index + usize::from(*ch == ' ')
            } else {
                break_at + 1
            };
            // 当前字符（或它之后的部分）留在新行，重新量一遍宽度。
            if start <= index {
                width = (start..=index).map(&advance_of).sum();
                last_space = (start..=index).rev().find(|i| chars[*i].0 == ' ');
            } else {
                width = 0.0;
                last_space = None;
            }
        } else {
            width += advance;
            if *ch == ' ' {
                last_space = Some(index);
            }
        }
    }
    lines.push((start, chars.len()));
    lines
}

#[cfg(test)]
mod wrap_tests {
    use super::wrap_line_ranges;

    /// 等宽字符：每个 10px，测试里只关心断行位置。
    fn chars(text: &str) -> Vec<(char, f32)> {
        text.chars().map(|ch| (ch, 10.0)).collect()
    }

    fn slice(chars: &[(char, f32)], range: (usize, usize)) -> String {
        chars[range.0..range.1].iter().map(|(ch, _)| *ch).collect()
    }

    #[test]
    fn without_wrap_only_explicit_newlines_break_lines() {
        let text = "a very long line
second";
        let ranges = wrap_line_ranges(&chars(text), 30.0, false);
        assert_eq!(ranges.len(), 2);
        assert_eq!(slice(&chars(text), ranges[0]), "a very long line");
        assert_eq!(slice(&chars(text), ranges[1]), "second");
    }

    #[test]
    fn wrapping_breaks_on_the_last_space() {
        let text = "one two three";
        // 每字符 10px，宽度 70 → 一行放 7 个字符："one two" 正好 7。
        let ranges = wrap_line_ranges(&chars(text), 70.0, true);
        let lines: Vec<String> = ranges
            .iter()
            .map(|range| slice(&chars(text), *range))
            .collect();
        assert_eq!(lines, vec!["one two", "three"]);
    }

    #[test]
    fn a_word_longer_than_the_line_is_broken_by_character() {
        let text = "abcdefghij";
        let ranges = wrap_line_ranges(&chars(text), 30.0, true);
        let lines: Vec<String> = ranges
            .iter()
            .map(|range| slice(&chars(text), *range))
            .collect();
        assert_eq!(lines, vec!["abc", "def", "ghi", "j"]);
        // 不丢字符、也不产生空行。
        assert_eq!(lines.concat(), text);
    }

    #[test]
    fn wrap_and_explicit_newlines_compose() {
        let text = "aa bb
cc dd";
        let ranges = wrap_line_ranges(&chars(text), 30.0, true);
        let lines: Vec<String> = ranges
            .iter()
            .map(|range| slice(&chars(text), *range))
            .collect();
        assert_eq!(lines, vec!["aa", "bb", "cc", "dd"]);
    }

    #[test]
    fn an_unwrapped_empty_width_keeps_everything_on_one_line() {
        let text = "no wrap here";
        assert_eq!(wrap_line_ranges(&chars(text), 0.0, true).len(), 1);
    }
}

#[cfg(feature = "ui")]
mod gpu;
#[cfg(feature = "ui")]
pub(crate) mod text;
#[cfg(feature = "ui")]
pub use gpu::UiOverlay;

// ---------------------------------------------------------------------------
// 文本后端 feature 组合校验
// ---------------------------------------------------------------------------
//
// 两个后端互斥：同时打开会把 fontdue + ttf-parser（~90 KB）白白留在二进制里，
// 而用户以为自己已经切到系统文本渲染了。宁可编译失败也不要静默多占体积。

// `ui-fontdue` 与 `ui-directwrite` 只能选一个。
#[cfg(all(feature = "ui-fontdue", feature = "ui-directwrite"))]
compile_error!(
    "features `ui-fontdue` 与 `ui-directwrite` 互斥：只选一个文本光栅后端。\n\
     Windows 发布包要省体积时用 `default-features = false, features = [\"ui\", \
     \"ui-directwrite\"]`；其它平台用 `\"ui-fontdue\"`。"
);

// DirectWrite 是 Windows 专属后端，非 Windows 目标没有 `windows` 绑定可用。
#[cfg(all(feature = "ui-directwrite", not(windows)))]
compile_error!(
    "feature `ui-directwrite` 只在 Windows 目标可用（它依赖 `windows` 的 DirectWrite/GDI 绑定）。\n\
     非 Windows 目标请用 `ui-fontdue`，或不选任何文本后端（只画 quad、不画文字）。"
);
