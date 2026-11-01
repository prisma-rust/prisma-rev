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
}

impl ComputedLayout {
    /// 只带矩形的布局（顺序留 0，由布局系统随后填充）。
    pub fn from_rect(rect: [f32; 4]) -> Self {
        Self { rect, order: 0 }
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
    use super::{Button, Interaction, Style, Text, TextAlign};

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
