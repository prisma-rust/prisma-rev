//! UI ECS 组件 —— 数据驱动 UI 的组件定义
//!
//! # 理念
//! UI 元素 = Entity + [`Node`] + [`Style`] + [`ComputedLayout`] + (可选 [`Text`])
//! Layout System 读取 Style → 写入 ComputedLayout
//! Render System 读取 ComputedLayout + Text → 生成绘制命令

use prism_reflect::{FieldType, Reflect, ReflectError, ReflectType, Value};
use serde::{Deserialize, Serialize};

// ── 标记组件 ─────────────────────────────────────────────────

/// **标记**：此 Entity 是 UI 元素。Layout System 只查询有此组件的实体。
///
/// 定义为空花括号结构体（而非单元结构体），以便场景 JSON 用 `{}` 反序列化。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Node")]
pub struct Node {}

// ── 锚点系统 ─────────────────────────────────────────────────

/// 锚点角 —— 定义子元素边相对于父边的位置（0..=1 归一化）。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Anchors")]
pub struct Anchors {
    /// 水平起始角（0 = 父左，1 = 父右）
    pub min_x: f32,
    /// 垂直起始角（0 = 父顶，1 = 父底）
    pub min_y: f32,
    /// 水平结束角（0 = 父左，1 = 父右）
    pub max_x: f32,
    /// 垂直结束角（0 = 父顶，1 = 父底）
    pub max_y: f32,
}

impl Anchors {
    /// 填满父容器
    pub const STRETCH: Self = Self {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    /// 左上角
    pub const TOP_LEFT: Self = Self {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 0.0,
        max_y: 0.0,
    };
    /// 居中
    pub const CENTER: Self = Self {
        min_x: 0.5,
        min_y: 0.5,
        max_x: 0.5,
        max_y: 0.5,
    };
    /// 底部居中
    pub const BOTTOM_CENTER: Self = Self {
        min_x: 0.5,
        min_y: 1.0,
        max_x: 0.5,
        max_y: 1.0,
    };
}

impl Default for Anchors {
    fn default() -> Self {
        Self::STRETCH
    }
}

/// Pivot —— 旋转/缩放/定位的中心点（归一化 0..=1）。
///
/// 只有**点锚点**（`Anchors` 该轴 `min == max`）才用 pivot 对齐尺寸；
/// 拉伸轴（`min != max`）的矩形由锚点与边距决定，pivot 不参与
/// （与引擎的 `anchorMin/anchorMax` + `pivot` 语义一致）。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Pivot")]
pub struct Pivot {
    pub x: f32,
    pub y: f32,
}

impl Pivot {
    pub const CENTER: Self = Self { x: 0.5, y: 0.5 };
    pub const TOP_LEFT: Self = Self { x: 0.0, y: 0.0 };
}

impl Default for Pivot {
    fn default() -> Self {
        Self::CENTER
    }
}

/// 边距（像素）。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Margin")]
pub struct Margin {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

// ── Style ─────────────────────────────────────────────────────

/// UI 元素的视觉和布局属性。
#[derive(Clone, Debug, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Style")]
pub struct Style {
    /// 固定宽度（px）。`None` = 由锚点决定。
    #[serde(default)]
    pub width: Option<f32>,
    /// 固定高度（px）。`None` = 由锚点决定。
    #[serde(default)]
    pub height: Option<f32>,
    /// 锚点。
    #[serde(default)]
    pub anchors: Anchors,
    /// 枢轴（归一化，0..=1）。
    #[serde(default)]
    pub pivot: Pivot,
    /// 边距偏移（px）。
    #[serde(default)]
    pub margin: Margin,
    /// 背景色（RGBA，0..=1）。`[0;4]` = 透明。
    #[serde(default)]
    pub background: [f32; 4],
    /// 边框圆角（px）。
    #[serde(default)]
    pub border_radius: f32,
    /// 可见性。
    #[serde(default = "default_visible")]
    pub visible: bool,
}

/// `Style::visible` 的 serde 默认值。
///
/// 必须与 [`Style::default`] 保持一致：作者 JSON 省略 `visible` 时元素是可见的，
/// 不能落到 `bool::default()`（`false`）。
fn default_visible() -> bool {
    true
}

impl Default for Style {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            anchors: Anchors::STRETCH,
            pivot: Pivot::CENTER,
            margin: Margin::default(),
            background: [0.0; 4],
            border_radius: 0.0,
            visible: true,
        }
    }
}

impl Style {
    /// 创建全屏填充的透明 UI 元素。
    pub fn fullscreen() -> Self {
        Self::default()
    }

    /// 创建一个固定大小的 UI 元素（锚点居中）。
    pub fn fixed(w: f32, h: f32) -> Self {
        Self {
            width: Some(w),
            height: Some(h),
            anchors: Anchors::CENTER,
            pivot: Pivot::CENTER,
            ..Default::default()
        }
    }
}

// ── ComputedLayout ────────────────────────────────────────────

/// **Layout System 输出**：最终屏幕空间矩形（像素）。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::ComputedLayout")]
pub struct ComputedLayout {
    /// 屏幕空间边界 `[left, top, width, height]`
    #[serde(default)]
    pub rect: [f32; 4],
    /// 层级绘制顺序（深度优先前序：父在前，子在后，兄弟按文档顺序）。
    ///
    /// 由布局系统每帧重算，和 `rect` 一样是**派生值**——作者不写它，`.scene.json`
    /// 里也没有这个字段。同一实体的背景和文本共用同一个 `order`（靠
    /// [`crate::ui`] 的绘制列表再按「先矩形后文本」区分），所以兄弟节点的 z 序
    /// 就是层级面板里看到的顺序（见 #462）。
    #[serde(default)]
    pub order: u32,
    /// 这个节点所在 Canvas 的缩放系数（#463）。`rect` 已经是**屏幕像素**，
    /// 这个值只用来把字号 / 圆角这类「设计像素」量换算到屏幕像素。
    /// `1.0` = 没有 Canvas（旧行为）。
    #[serde(default = "identity_scale")]
    pub scale: f32,
}

/// `ComputedLayout::scale` 的 serde 默认：`1.0`（无缩放）。
pub(crate) fn identity_scale() -> f32 {
    1.0
}

impl ComputedLayout {
    /// 只带矩形的布局（顺序留 0，缩放 1，由布局系统随后填充）。
    pub fn from_rect(rect: [f32; 4]) -> Self {
        Self {
            rect,
            order: 0,
            scale: 1.0,
        }
    }

    pub fn left(&self) -> f32 {
        self.rect[0]
    }
    pub fn top(&self) -> f32 {
        self.rect[1]
    }
    pub fn width(&self) -> f32 {
        self.rect[2]
    }
    pub fn height(&self) -> f32 {
        self.rect[3]
    }
}

// ── Text ──────────────────────────────────────────────────────

/// 文本内容。
///
/// `#[serde(default)]`：旧文档或手工编辑的 JSON 可能只写 `content`，
/// 缺失字段回落到 [`Text::default`]，而不是让整个组件反序列化失败。
#[derive(Clone, Debug, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Text")]
#[serde(default)]
pub struct Text {
    pub content: String,
    pub font_size: f32,
    pub color: [f32; 4],
    pub alignment: TextAlign,
    /// 工程自带字体（`assets/fonts/*.ttf|otf|ttc` 的相对路径，空 = 系统默认字体，#583）。
    ///
    /// 字体文件是普通资产：cooker 识别为 `AssetType::Font` 原样进 `.pak`，
    /// 运行时经 `ResourceManager` 读出字节交给 UI 光栅器。
    pub font: String,
}

impl Default for Text {
    fn default() -> Self {
        Self {
            content: String::new(),
            font_size: 16.0,
            color: [1.0; 4],
            alignment: TextAlign::Center,
            font: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub enum TextAlign {
    #[default]
    Center,
    Left,
    Right,
}

impl ReflectType for TextAlign {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Center", "Left", "Right"])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::Center => 0,
            Self::Left => 1,
            Self::Right => 2,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Center),
            1 => Ok(Self::Left),
            2 => Ok(Self::Right),
            other => Err(ReflectError::BadData(format!(
                "invalid TextAlign discriminant {other}"
            ))),
        }
    }
}

// ── Canvas / 缩放 ─────────────────────────────────────────────

/// Canvas 缩放模式（对齐 Unity `CanvasScaler` 的三种 Screen Match Mode）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasScaleMode {
    /// 1 屏幕像素 = 1 设计像素（默认 = 没有 Canvas 的旧行为）。
    #[default]
    ConstantPixelSize,
    /// 按参考分辨率等比缩放：宽、高各自算一个比例，再按
    /// [`Canvas::match_width_or_height`] 在对数空间插值。
    ScaleWithScreenSize,
    /// 只用宽或高之一：`match_width_or_height` < 0.5 取宽度，否则取高度。
    MatchWidthOrHeight,
}

impl ReflectType for CanvasScaleMode {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&[
            "ConstantPixelSize",
            "ScaleWithScreenSize",
            "MatchWidthOrHeight",
        ])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::ConstantPixelSize => 0,
            Self::ScaleWithScreenSize => 1,
            Self::MatchWidthOrHeight => 2,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::ConstantPixelSize),
            1 => Ok(Self::ScaleWithScreenSize),
            2 => Ok(Self::MatchWidthOrHeight),
            other => Err(ReflectError::BadData(format!(
                "invalid CanvasScaleMode discriminant {other}"
            ))),
        }
    }
}

/// UI 画布：把「设计分辨率」映射到实际屏幕，整棵子树等比缩放（#463）。
///
/// 挂在某个根节点上时，它的子节点都按 [`Canvas::scale_factor`] 换算：
/// 布局在**虚拟分辨率**里算，写回 `ComputedLayout.rect` 时乘回缩放系数，
/// 所以 `Style.width` / `margin` / 锚点都按设计稿数值写，1080p 和 720p
/// 看到的是同一套布局。字号和圆角这类绝对量由渲染系统按
/// `ComputedLayout.scale` 一起放大。
///
/// 嵌套 Canvas 不做多个虚拟坐标系：子节点只继承最近的 Canvas（与 Unity
/// 的 nesting 行为不同，会在文档里说明）。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Canvas")]
#[serde(default)]
pub struct Canvas {
    /// 设计稿分辨率（如 1920×1080）。
    pub reference_resolution: [f32; 2],
    pub scale_mode: CanvasScaleMode,
    /// `0` = 完全贴合宽度，`1` = 完全贴合高度。
    pub match_width_or_height: f32,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            reference_resolution: [1920.0, 1080.0],
            scale_mode: CanvasScaleMode::ScaleWithScreenSize,
            match_width_or_height: 0.5,
        }
    }
}

impl Canvas {
    /// 屏幕像素 / 设计像素 的缩放系数。
    ///
    /// 退化输入（0 或非有限值）按 1 处理，避免整棵 UI 变成 NaN。
    pub fn scale_factor(&self, screen: (f32, f32)) -> f32 {
        let width = finite_positive(screen.0, 1.0);
        let height = finite_positive(screen.1, 1.0);
        let reference_width = finite_positive(self.reference_resolution[0], 1920.0);
        let reference_height = finite_positive(self.reference_resolution[1], 1080.0);
        let by_width = width / reference_width;
        let by_height = height / reference_height;
        let factor = match self.scale_mode {
            CanvasScaleMode::ConstantPixelSize => 1.0,
            CanvasScaleMode::MatchWidthOrHeight => {
                if self.match_width_or_height >= 0.5 {
                    by_height
                } else {
                    by_width
                }
            }
            CanvasScaleMode::ScaleWithScreenSize => {
                let t = match self.match_width_or_height {
                    t if t.is_finite() => t.clamp(0.0, 1.0),
                    _ => 0.5,
                };
                (by_width.ln() * (1.0 - t) + by_height.ln() * t).exp()
            }
        };
        finite_positive(factor, 1.0)
    }
}

fn finite_positive(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

// ── Interaction ───────────────────────────────────────────────

/// 交互状态（由 Input System 每帧更新）。
///
/// 从 #528 起也可以挂在场景里：状态每帧被重写，但组件本身可以序列化，
/// 因此场景 / 预制体 / 模板能保存「这个元素可交互」这件事，游戏代码
/// 也不用自己插入运行时组件。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Interaction")]
#[serde(default)]
pub struct Interaction {
    pub hovered: bool,
    /// 指针在**按下目标**上按住（拖出元素后松开就不算按下）。
    pub pressed: bool,
    /// 一帧有效：在元素内按下、并在元素内松开（或键盘激活）时置位。
    pub clicked: bool,
}

// ── Button ───────────────────────────────────────────────────

/// 可点击按钮的**外观**状态色（#528）。
///
/// 只是数据：`ui_render_system` 按 [`Interaction`] 挑选颜色，`ui_input_system`
/// 负责 hover / pressed / clicked 语义。没有 `Button` 的 UI 元素继续用
/// [`Style::background`]，两者可以共存（`Button` 优先）。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::ui::Button")]
#[serde(default)]
pub struct Button {
    /// `false` = 显示 [`Button::disabled`] 颜色，且不再响应点击。
    pub interactable: bool,
    pub normal: [f32; 4],
    pub hover: [f32; 4],
    pub pressed: [f32; 4],
    pub disabled: [f32; 4],
}

impl Default for Button {
    fn default() -> Self {
        Self {
            interactable: true,
            normal: [0.18, 0.20, 0.24, 1.0],
            hover: [0.24, 0.27, 0.32, 1.0],
            pressed: [0.12, 0.14, 0.17, 1.0],
            disabled: [0.15, 0.15, 0.16, 0.85],
        }
    }
}

impl Button {
    /// 当前应画的背景色；`interaction` 缺失时按未交互处理。
    pub fn color(&self, interaction: Option<&Interaction>) -> [f32; 4] {
        if !self.interactable {
            return self.disabled;
        }
        match interaction {
            Some(state) if state.pressed => self.pressed,
            Some(state) if state.hovered => self.hover,
            _ => self.normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Button, Canvas, CanvasScaleMode, ComputedLayout, Interaction, Style, Text, TextAlign,
    };

    #[test]
    fn button_colors_follow_the_interaction_state() {
        let button = Button::default();
        assert_eq!(button.color(None), button.normal);
        assert_eq!(
            button.color(Some(&Interaction {
                hovered: true,
                ..Default::default()
            })),
            button.hover
        );
        assert_eq!(
            button.color(Some(&Interaction {
                hovered: true,
                pressed: true,
                ..Default::default()
            })),
            button.pressed,
            "按下比 hover 优先"
        );
        let disabled = Button {
            interactable: false,
            ..Default::default()
        };
        assert_eq!(
            disabled.color(Some(&Interaction {
                hovered: true,
                pressed: true,
                ..Default::default()
            })),
            disabled.disabled
        );
    }

    #[test]
    fn button_and_interaction_round_trip_through_json() {
        let button: Button = serde_json::from_str(r#"{"interactable":false}"#).unwrap();
        assert!(!button.interactable);
        assert_eq!(button.normal, Button::default().normal, "其余字段走默认");
        let json = serde_json::to_string(&button).unwrap();
        let back: Button = serde_json::from_str(&json).unwrap();
        assert!(!back.interactable);

        let interaction: Interaction =
            serde_json::from_str(r#"{"hovered":true,"pressed":false,"clicked":true}"#).unwrap();
        assert!(interaction.hovered && interaction.clicked && !interaction.pressed);
        let legacy: Interaction = serde_json::from_str("{}").unwrap();
        assert!(!legacy.hovered, "空对象 = 未交互");
    }

    #[test]
    fn style_partial_json_is_visible_by_default() {
        let style: Style = serde_json::from_str(r#"{"width":100}"#).unwrap();
        assert_eq!(style.width, Some(100.0));
        assert!(
            style.visible,
            "omitting `visible` must not hide the element"
        );
    }

    #[test]
    fn text_font_defaults_to_the_system_font_and_round_trips() {
        // #583：旧文档没有 `font` → 空字符串 = 系统默认字体。
        let text: Text = serde_json::from_str(r#"{"content":"hi","font_size":20}"#).unwrap();
        assert!(text.font.is_empty());

        let authored: Text = serde_json::from_str(
            r#"{"content":"hi","font_size":20,"font":"assets/fonts/hud.ttf"}"#,
        )
        .unwrap();
        assert_eq!(authored.font, "assets/fonts/hud.ttf");
        let json = serde_json::to_string(&authored).unwrap();
        let back: Text = serde_json::from_str(&json).unwrap();
        assert_eq!(back.font, "assets/fonts/hud.ttf");
    }

    #[test]
    fn canvas_scales_the_reference_resolution_to_the_screen() {
        // #463：1920×1080 设计稿在 960×540 上应该是 0.5。
        let canvas = Canvas::default();
        assert_eq!(canvas.scale_mode, CanvasScaleMode::ScaleWithScreenSize);
        assert!((canvas.scale_factor((960.0, 540.0)) - 0.5).abs() < 1e-5);
        assert!((canvas.scale_factor((1920.0, 1080.0)) - 1.0).abs() < 1e-5);
        assert!((canvas.scale_factor((3840.0, 2160.0)) - 2.0).abs() < 1e-5);

        // 宽高比不同时，match 在 log 空间插值：960×1080 的宽比 0.5、高比 1.0。
        let skewed = canvas.scale_factor((960.0, 1080.0));
        assert!((skewed - 0.5f32.sqrt()).abs() < 1e-4, "{skewed}");

        // 只贴合一边时直接取该边。
        let width_only = Canvas {
            scale_mode: CanvasScaleMode::MatchWidthOrHeight,
            match_width_or_height: 0.0,
            ..Canvas::default()
        };
        assert!((width_only.scale_factor((960.0, 1080.0)) - 0.5).abs() < 1e-5);
        let height_only = Canvas {
            scale_mode: CanvasScaleMode::MatchWidthOrHeight,
            match_width_or_height: 1.0,
            ..Canvas::default()
        };
        assert!((height_only.scale_factor((960.0, 1080.0)) - 1.0).abs() < 1e-5);

        // ConstantPixelSize = 1，退化输入也回到 1 而不是 NaN/0。
        let pixels = Canvas {
            scale_mode: CanvasScaleMode::ConstantPixelSize,
            ..Canvas::default()
        };
        assert_eq!(pixels.scale_factor((640.0, 360.0)), 1.0);
        assert_eq!(pixels.scale_factor((0.0, f32::NAN)), 1.0);
    }

    #[test]
    fn canvas_round_trips_and_legacy_layouts_default_to_scale_one() {
        let canvas = Canvas {
            reference_resolution: [1280.0, 720.0],
            scale_mode: CanvasScaleMode::MatchWidthOrHeight,
            match_width_or_height: 1.0,
        };
        let json = serde_json::to_string(&canvas).unwrap();
        let back: Canvas = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reference_resolution, [1280.0, 720.0]);
        assert_eq!(back.scale_mode, CanvasScaleMode::MatchWidthOrHeight);

        // 旧 `.scene.json` 里没有 `scale` 字段：按 1 处理，字号不被缩小。
        let layout: ComputedLayout =
            serde_json::from_str(r#"{"rect":[0,0,100,50],"order":3}"#).unwrap();
        assert_eq!(layout.scale, 1.0);
    }

    #[test]
    fn style_empty_object_matches_default() {
        let style: Style = serde_json::from_str("{}").unwrap();
        assert_eq!(style.width, Style::default().width);
        assert!(style.visible);
    }

    #[test]
    fn style_explicit_invisible_is_respected() {
        let style: Style = serde_json::from_str(r#"{"visible":false}"#).unwrap();
        assert!(!style.visible);
    }

    #[test]
    fn text_partial_json_fills_defaults() {
        let text: Text = serde_json::from_str(r#"{"content":"hello"}"#).unwrap();
        assert_eq!(text.content, "hello");
        assert_eq!(text.font_size, 16.0);
        assert_eq!(text.color, [1.0; 4]);
        assert!(matches!(text.alignment, TextAlign::Center));
    }

    #[test]
    fn text_empty_object_is_default() {
        let text: Text = serde_json::from_str("{}").unwrap();
        assert!(text.content.is_empty());
        assert_eq!(text.font_size, Text::default().font_size);
    }
}
