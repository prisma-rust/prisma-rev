//! 跨 crate 共享的渲染状态数据（渲染状态枚举）。
//!
//! 这些类型同时被两侧使用：
//! - `prism-scene`：作者格式（`.scene.json`）、反射 schema、RSCN 编码；
//! - `prism-render`：翻译成 Vulkan 管线/清屏状态。
//!
//! 所以它们住在两者都能依赖的中立 crate 里，而不是任何一侧。这里有两条硬约束：
//!
//! 1. **纯数据**：不依赖 ash/Vulkan、不依赖 ECS，把语义翻译成管线状态是
//!    `prism-render` 的职责（例如 `cull_flags()` / 混合因子映射）。
//! 2. 反射编码固定为小整数（`U8` 枚举 / `RenderQueue` 用 `I32`），
//!    这样 RSCN 与远端工具不需要额外的类型表条目。
//!
//! `prism-scene::render_enums` 只是本 crate 的 re-export，历史路径
//! （`prism_scene::CullMode`、`prism_scene::render_enums::*`）保持不变。

use prism_reflect::{FieldType, ReflectError, ReflectType, Value};
use serde::{Deserialize, Serialize};

/// 相机清屏方式。
///
/// 数值固定：`(int)Skybox == 1` … `(int)Nothing == 4`（引擎的 `0` 是
/// 未设置的非法值，这里用 [`Default`] 代替）。序列化形式是变体名
/// （`"Skybox"` / `"SolidColor"` / `"Depth"` / `"Nothing"`），与 `.scene.json` 里
/// 其它枚举（`TextAlign`）一致。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CameraClearFlags {
    /// 用天空盒填充背景。
    #[default]
    Skybox,
    /// 用纯色填充背景（见 `Camera::background_color`）。
    SolidColor,
    /// 只清深度，保留已有颜色（叠加相机用）。
    Depth,
    /// 颜色与深度都不清。
    Nothing,
}

impl CameraClearFlags {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Skybox => 1,
            Self::SolidColor => 2,
            Self::Depth => 3,
            Self::Nothing => 4,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Skybox),
            2 => Some(Self::SolidColor),
            3 => Some(Self::Depth),
            4 => Some(Self::Nothing),
            _ => None,
        }
    }

    /// 是否清颜色缓冲（`Skybox` / `SolidColor`）。
    pub const fn clears_color(self) -> bool {
        matches!(self, Self::Skybox | Self::SolidColor)
    }

    /// 是否清深度缓冲（除 `Nothing` 之外都清）。
    pub const fn clears_depth(self) -> bool {
        !matches!(self, Self::Nothing)
    }

    /// 是否绘制天空盒背景。
    ///
    /// 只有 [`Skybox`](Self::Skybox) 会在背景填充天空盒材质；其余清屏方式
    /// （纯色 / 仅清深度 / 不清）即使场景里挂着启用的 `Skybox` 实体也不应绘制。
    /// 纯色填充与多相机按 `depth` 叠加由 `prism-render` 的 forward pass 实现（#461）。
    pub const fn draws_skybox(self) -> bool {
        matches!(self, Self::Skybox)
    }
}

impl ReflectType for CameraClearFlags {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        // 顺序与声明一致；这些字符串就是 `.scene.json` 里的取值。
        Some(&["Skybox", "SolidColor", "Depth", "Nothing"])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("CameraClearFlags 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw).ok_or_else(|| {
            ReflectError::BadData(format!("invalid CameraClearFlags discriminant {raw}"))
        })
    }
}

/// 光源类型（取值固定）。
///
/// 引擎的 Inspector 只让选运行时支持的类型；这里保留完整枚举以便**无损**对照与
/// 迁移，但渲染器目前只实现 [`Spot`](Self::Spot) / [`Directional`](Self::Directional) /
/// [`Point`](Self::Point)（见 [`LightType::is_rendered`]）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightType {
    /// 聚光（锥形）。
    Spot,
    /// 平行光（无限远，方向由实体变换决定）。
    Directional,
    /// 点光源（全向）。
    #[default]
    Point,
    /// 矩形面光源 = `LightType.Rectangle`（`Area` 的新名字）。
    Rectangle,
    /// 圆形面光源。
    Disc,
    /// 金字塔形聚光。
    Pyramid,
    /// 盒形聚光。
    Box,
    /// 管状光源。
    Tube,
}

impl LightType {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Spot => 0,
            Self::Directional => 1,
            Self::Point => 2,
            Self::Rectangle => 3,
            Self::Disc => 4,
            Self::Pyramid => 5,
            Self::Box => 6,
            Self::Tube => 7,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Spot),
            1 => Some(Self::Directional),
            2 => Some(Self::Point),
            3 => Some(Self::Rectangle),
            4 => Some(Self::Disc),
            5 => Some(Self::Pyramid),
            6 => Some(Self::Box),
            7 => Some(Self::Tube),
            _ => None,
        }
    }

    /// 当前渲染器是否真的会绘制该类型。
    ///
    /// 面光源 / 盒形 / 管状只是 schema 对齐，`collect_lights` 会跳过它们
    /// （里这类光源也只用于烘焙或需要额外管线支持）。
    pub const fn is_rendered(self) -> bool {
        matches!(self, Self::Spot | Self::Directional | Self::Point)
    }
}

impl ReflectType for LightType {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&[
            "Spot",
            "Directional",
            "Point",
            "Rectangle",
            "Disc",
            "Pyramid",
            "Box",
            "Tube",
        ])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("LightType 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw)
            .ok_or_else(|| ReflectError::BadData(format!("invalid LightType discriminant {raw}")))
    }
}

/// 阴影模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightShadows {
    /// 不投阴影。
    #[default]
    None,
    /// 硬阴影（单次采样）。
    Hard,
    /// 软阴影（PCF）。
    Soft,
}

impl LightShadows {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Hard => 1,
            Self::Soft => 2,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::Hard),
            2 => Some(Self::Soft),
            _ => None,
        }
    }

    /// 是否投阴影（`Hard` / `Soft`）。
    pub const fn casts_shadows(self) -> bool {
        !matches!(self, Self::None)
    }
}

impl ReflectType for LightShadows {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["None", "Hard", "Soft"])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("LightShadows 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw).ok_or_else(|| {
            ReflectError::BadData(format!("invalid LightShadows discriminant {raw}"))
        })
    }
}

/// 光源渲染模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightRenderMode {
    /// 由渲染器决定（逐像素还是逐顶点）。
    #[default]
    Auto,
    /// 强制逐像素。
    ForcePixel,
    /// 强制逐顶点。
    ForceVertex,
}

impl LightRenderMode {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Auto => 0,
            Self::ForcePixel => 1,
            Self::ForceVertex => 2,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Auto),
            1 => Some(Self::ForcePixel),
            2 => Some(Self::ForceVertex),
            _ => None,
        }
    }
}

impl ReflectType for LightRenderMode {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Auto", "ForcePixel", "ForceVertex"])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("LightRenderMode 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw).ok_or_else(|| {
            ReflectError::BadData(format!("invalid LightRenderMode discriminant {raw}"))
        })
    }
}

/// 阴影投射模式。
///
/// 挂在渲染器上（见 `prism_scene::components::MeshRenderer::shadow_casting_mode`），
/// 决定该物体**主通道是否可见**以及**是否写阴影贴图**。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShadowCastingMode {
    /// 不投影、正常渲染。
    Off,
    /// 正常渲染 + 投影（默认，等价于引擎的 `On`）。
    #[default]
    On,
    /// 双面投影 + 正常渲染。
    ///
    /// 对应 `ShadowCastingMode.TwoSided`：阴影渲染时**关闭背面剔除**
    /// （即使材质本身开启了背面剔除），因此单面几何体也会从两面投射阴影
    /// （见 [`casts_two_sided_shadows`](Self::casts_two_sided_shadows)）。
    TwoSided,
    /// 只投影，主通道不可见。
    ShadowsOnly,
}

impl ShadowCastingMode {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::TwoSided => 2,
            Self::ShadowsOnly => 3,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            2 => Some(Self::TwoSided),
            3 => Some(Self::ShadowsOnly),
            _ => None,
        }
    }

    /// 是否写阴影贴图（`On` / `TwoSided` / `ShadowsOnly`）。
    pub const fn casts_shadows(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// 是否出现在相机主通道（`Off` / `On` / `TwoSided`）。
    pub const fn visible_in_main_pass(self) -> bool {
        !matches!(self, Self::ShadowsOnly)
    }

    /// 阴影渲染是否关闭背面剔除（`ShadowCastingMode.TwoSided`）。
    ///
    /// 文档：`TwoSided` 会在**阴影渲染时关闭背面剔除**，即使物体材质
    /// 本身开了背面剔除。因此 `Off` / `On` / `ShadowsOnly` 走背面剔除
    /// （`false`），只有 `TwoSided` 双面投射（`true`）。只影响阴影通道，
    /// 不影响 [`visible_in_main_pass`](Self::visible_in_main_pass)。
    pub const fn casts_two_sided_shadows(self) -> bool {
        matches!(self, Self::TwoSided)
    }
}

impl ReflectType for ShadowCastingMode {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Off", "On", "TwoSided", "ShadowsOnly"])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("ShadowCastingMode 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw).ok_or_else(|| {
            ReflectError::BadData(format!("invalid ShadowCastingMode discriminant {raw}"))
        })
    }
}

/// 剔除模式（取值固定）。
///
/// 默认 [`Back`](Self::Back)，与 `prism-render` 现有图形管线的
/// `vk::CullModeFlags::BACK` 一致。真正翻译成 Vulkan 状态的是 `prism-render`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CullMode {
    /// 不剔除（双面可见）。
    Off,
    /// 剔除正面，只保留背面。
    Front,
    /// 剔除背面（默认）。
    #[default]
    Back,
}

impl CullMode {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Front => 1,
            Self::Back => 2,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Front),
            2 => Some(Self::Back),
            _ => None,
        }
    }

    /// 是否剔除某个面（`Front` / `Back`）。
    pub const fn culls(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl ReflectType for CullMode {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Off", "Front", "Back"])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("CullMode 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw)
            .ok_or_else(|| ReflectError::BadData(format!("invalid CullMode discriminant {raw}")))
    }
}

/// 深度/模板比较函数（取值固定）。
///
/// 默认 [`LessEqual`](Self::LessEqual)：着色器里 `ZTest` 的默认值。
/// [`Disabled`](Self::Disabled) 对应引擎的“关闭深度测试”，Vulkan 没有对应的
/// `vk::CompareOp`，由 `prism-render` 用它自己的深度测试开关
/// （`depth_test_enable(false)`，即 `VK_FALSE`）表达，而不是拿某个比较运算去反转。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareFunction {
    /// 关闭比较（等价于不测试）。
    Disabled,
    /// 永不通过。
    Never,
    /// 小于。
    Less,
    /// 等于。
    Equal,
    /// 小于等于（默认）。
    #[default]
    LessEqual,
    /// 大于。
    Greater,
    /// 不等于。
    NotEqual,
    /// 大于等于。
    GreaterEqual,
    /// 永远通过。
    Always,
}

impl CompareFunction {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Disabled => 0,
            Self::Never => 1,
            Self::Less => 2,
            Self::Equal => 3,
            Self::LessEqual => 4,
            Self::Greater => 5,
            Self::NotEqual => 6,
            Self::GreaterEqual => 7,
            Self::Always => 8,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Disabled),
            1 => Some(Self::Never),
            2 => Some(Self::Less),
            3 => Some(Self::Equal),
            4 => Some(Self::LessEqual),
            5 => Some(Self::Greater),
            6 => Some(Self::NotEqual),
            7 => Some(Self::GreaterEqual),
            8 => Some(Self::Always),
            _ => None,
        }
    }

    /// 是否真的做比较（除 [`Disabled`](Self::Disabled) 之外都做）。
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

impl ReflectType for CompareFunction {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&[
            "Disabled",
            "Never",
            "Less",
            "Equal",
            "LessEqual",
            "Greater",
            "NotEqual",
            "GreaterEqual",
            "Always",
        ])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("CompareFunction 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw).ok_or_else(|| {
            ReflectError::BadData(format!("invalid CompareFunction discriminant {raw}"))
        })
    }
}

/// 混合因子（取值固定）。
///
/// 引擎的混合状态是「源因子 × 目标因子 + 混合运算」，这个枚举描述
/// **因子**；混合运算见 [`BlendOp`]。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlendMode {
    /// `0`。
    Zero,
    /// `1`（默认源因子；与 [`Zero`](Self::Zero) 组成不透明混合）。
    #[default]
    One,
    /// 目标颜色。
    DstColor,
    /// 源颜色。
    SrcColor,
    /// `1 - dst`。
    OneMinusDstColor,
    /// 源 alpha。
    SrcAlpha,
    /// `1 - src`。
    OneMinusSrcColor,
    /// 目标 alpha。
    DstAlpha,
    /// `1 - dst.a`。
    OneMinusDstAlpha,
    /// `min(src.a, 1 - dst.a)`。
    SrcAlphaSaturate,
    /// `1 - src.a`（标准 alpha 混合的源因子）。
    OneMinusSrcAlpha,
}

impl BlendMode {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Zero => 0,
            Self::One => 1,
            Self::DstColor => 2,
            Self::SrcColor => 3,
            Self::OneMinusDstColor => 4,
            Self::SrcAlpha => 5,
            Self::OneMinusSrcColor => 6,
            Self::DstAlpha => 7,
            Self::OneMinusDstAlpha => 8,
            Self::SrcAlphaSaturate => 9,
            Self::OneMinusSrcAlpha => 10,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Zero),
            1 => Some(Self::One),
            2 => Some(Self::DstColor),
            3 => Some(Self::SrcColor),
            4 => Some(Self::OneMinusDstColor),
            5 => Some(Self::SrcAlpha),
            6 => Some(Self::OneMinusSrcColor),
            7 => Some(Self::DstAlpha),
            8 => Some(Self::OneMinusDstAlpha),
            9 => Some(Self::SrcAlphaSaturate),
            10 => Some(Self::OneMinusSrcAlpha),
            _ => None,
        }
    }
}

impl ReflectType for BlendMode {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&[
            "Zero",
            "One",
            "DstColor",
            "SrcColor",
            "OneMinusDstColor",
            "SrcAlpha",
            "OneMinusSrcColor",
            "DstAlpha",
            "OneMinusDstAlpha",
            "SrcAlphaSaturate",
            "OneMinusSrcAlpha",
        ])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("BlendMode 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw)
            .ok_or_else(|| ReflectError::BadData(format!("invalid BlendMode discriminant {raw}")))
    }
}

/// 混合运算。
///
/// 与 [`BlendMode`] 一起构成 材质的混合状态：
/// `结果 = srcFactor × src op dstFactor × dst`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlendOp {
    /// `src + dst`（默认）。
    #[default]
    Add,
    /// `src - dst`。
    Subtract,
    /// `dst - src`。
    ReverseSubtract,
    /// `min(src, dst)`。
    Min,
    /// `max(src, dst)`。
    Max,
}

impl BlendOp {
    /// 引擎的枚举数值。
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Add => 0,
            Self::Subtract => 1,
            Self::ReverseSubtract => 2,
            Self::Min => 3,
            Self::Max => 4,
        }
    }

    /// 从枚举数值还原；未知值返回 `None`。
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Add),
            1 => Some(Self::Subtract),
            2 => Some(Self::ReverseSubtract),
            3 => Some(Self::Min),
            4 => Some(Self::Max),
            _ => None,
        }
    }
}

impl ReflectType for BlendOp {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Add", "Subtract", "ReverseSubtract", "Min", "Max"])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(self.ordinal_value()))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_u64()?;
        let raw = u32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("BlendOp 数值越界: {raw}")))?;
        Self::from_ordinal_value(raw)
            .ok_or_else(|| ReflectError::BadData(format!("invalid BlendOp discriminant {raw}")))
    }
}

/// 渲染队列。
///
/// **形状差异**：引擎的 `RenderQueue` 是包裹一个 `int` 的 struct（不是枚举），
/// 具名队列只是 `static readonly int` 常量。这里保持同样的形状：i32 newtype +
/// 关联常量，这样作者数据里的任意队列值都能无损读写。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RenderQueue(i32);

impl RenderQueue {
    /// 背景（最先绘制）。
    pub const BACKGROUND: i32 = 1000;
    /// 不透明几何（默认队列）。
    pub const GEOMETRY: i32 = 2000;
    /// Alpha 测试之后、不透明几何结束之前。
    pub const ALPHA_TEST: i32 = 2450;
    /// 不透明区间的上界。
    pub const GEOMETRY_LAST: i32 = 2500;
    /// 透明物体。
    pub const TRANSPARENT: i32 = 3000;
    /// 叠加层（最后绘制）。
    pub const OVERLAY: i32 = 4000;

    /// 用任意队列值构造。
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    /// 队列数值（里的 `RenderQueue.value`）。
    pub const fn value(self) -> i32 {
        self.0
    }

    /// 是否属于不透明区间（`[0, GeometryLast]`）。
    ///
    /// 负数虽然允许存在（存档里可以无损保存任意队列值），但不在引擎的不透明
    /// 区间内，所以下界必须是 0。
    pub const fn is_opaque(self) -> bool {
        self.0 >= 0 && self.0 <= Self::GEOMETRY_LAST
    }

    /// 是否属于透明区间（`> GeometryLast`）。
    pub const fn is_transparent(self) -> bool {
        self.0 > Self::GEOMETRY_LAST
    }
}

impl Default for RenderQueue {
    fn default() -> Self {
        Self(Self::GEOMETRY)
    }
}

impl ReflectType for RenderQueue {
    fn field_type() -> FieldType {
        FieldType::I32
    }

    fn to_value(&self) -> Value {
        Value::I64(i64::from(self.0))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        let raw = value.as_i64()?;
        let raw = i32::try_from(raw)
            .map_err(|_| ReflectError::BadData(format!("RenderQueue 数值越界: {raw}")))?;
        Ok(Self(raw))
    }
}

/// 色温（开尔文）→ 线性 RGB，对应 `Mathf.CorrelatedColorTemperatureToRGB`。
///
/// 引擎的实现是 native 的；这里用常用的黑体辐射近似（Tanner Helland 的分段
/// 拟合），先按 sRGB 得到 0–1 分量、再转成线性空间，并裁到 `[1000, 40000]` K
/// （与 Inspector 的取值范围一致）。结果**不**做归一化：6500 K 接近
/// `(1, 1, 1)`，色温只是乘到 `Light.color` 上的滤色片。
pub fn correlated_color_temperature_to_rgb(kelvin: f32) -> glam::Vec3 {
    let kelvin = if kelvin.is_finite() {
        kelvin.clamp(1000.0, 40000.0)
    } else {
        6500.0
    };
    let t = kelvin / 100.0;

    let srgb = |c: f32| {
        let c = c.clamp(0.0, 255.0) / 255.0;
        // sRGB → 线性：引擎的灯颜色在渲染里是线性空间。
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };

    let r = if t <= 66.0 {
        255.0
    } else {
        329.698_73 * (t - 60.0).powf(-0.133_204_76)
    };
    let g = if t <= 66.0 {
        99.470_8 * t.ln() - 161.119_57
    } else {
        288.122_17 * (t - 60.0).powf(-0.075_514_846)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.517_73 * (t - 10.0).ln() - 305.044_8
    };

    glam::Vec3::new(srgb(r), srgb(g), srgb(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_reflect::ReflectType;

    #[test]
    fn clear_flags_ordinals_match() {
        assert_eq!(CameraClearFlags::Skybox.ordinal_value(), 1);
        assert_eq!(CameraClearFlags::SolidColor.ordinal_value(), 2);
        assert_eq!(CameraClearFlags::Depth.ordinal_value(), 3);
        assert_eq!(CameraClearFlags::Nothing.ordinal_value(), 4);
    }

    #[test]
    fn from_ordinal_value_round_trips_and_rejects_unknown() {
        for flags in [
            CameraClearFlags::Skybox,
            CameraClearFlags::SolidColor,
            CameraClearFlags::Depth,
            CameraClearFlags::Nothing,
        ] {
            assert_eq!(
                CameraClearFlags::from_ordinal_value(flags.ordinal_value()),
                Some(flags)
            );
        }
        assert_eq!(CameraClearFlags::from_ordinal_value(0), None);
        assert_eq!(CameraClearFlags::from_ordinal_value(5), None);
    }

    #[test]
    fn clear_predicates() {
        assert!(CameraClearFlags::Skybox.clears_color());
        assert!(CameraClearFlags::SolidColor.clears_color());
        assert!(!CameraClearFlags::Depth.clears_color());
        assert!(!CameraClearFlags::Nothing.clears_color());

        assert!(CameraClearFlags::Skybox.clears_depth());
        assert!(CameraClearFlags::Depth.clears_depth());
        assert!(!CameraClearFlags::Nothing.clears_depth());

        // 只有 Skybox 会绘制天空盒背景（`Camera.clearFlags` 语义）。
        assert!(CameraClearFlags::Skybox.draws_skybox());
        assert!(!CameraClearFlags::SolidColor.draws_skybox());
        assert!(!CameraClearFlags::Depth.draws_skybox());
        assert!(!CameraClearFlags::Nothing.draws_skybox());
    }

    #[test]
    fn reflect_round_trip_uses_u8() {
        assert_eq!(CameraClearFlags::field_type(), FieldType::U8);
        for flags in [
            CameraClearFlags::Skybox,
            CameraClearFlags::SolidColor,
            CameraClearFlags::Depth,
            CameraClearFlags::Nothing,
        ] {
            let value = flags.to_value();
            assert_eq!(CameraClearFlags::from_value(value).unwrap(), flags);
        }
        assert!(CameraClearFlags::from_value(Value::U64(9)).is_err());
    }

    #[test]
    fn serde_uses_variant_names() {
        let json = serde_json::to_string(&CameraClearFlags::SolidColor).unwrap();
        assert_eq!(json, "\"SolidColor\"");
        let back: CameraClearFlags = serde_json::from_str("\"Depth\"").unwrap();
        assert_eq!(back, CameraClearFlags::Depth);
    }

    #[test]
    fn default_is_skybox() {
        assert_eq!(CameraClearFlags::default(), CameraClearFlags::Skybox);
    }

    #[test]
    fn light_type_ordinals() {
        // : Spot=0, Directional=1, Point=2, Rectangle(Area)=3, Disc=4,
        // Pyramid=5, Box=6, Tube=7。
        assert_eq!(LightType::Spot.ordinal_value(), 0);
        assert_eq!(LightType::Directional.ordinal_value(), 1);
        assert_eq!(LightType::Point.ordinal_value(), 2);
        assert_eq!(LightType::Rectangle.ordinal_value(), 3);
        assert_eq!(LightType::Tube.ordinal_value(), 7);

        for ty in [
            LightType::Spot,
            LightType::Directional,
            LightType::Point,
            LightType::Rectangle,
            LightType::Disc,
            LightType::Pyramid,
            LightType::Box,
            LightType::Tube,
        ] {
            assert_eq!(LightType::from_ordinal_value(ty.ordinal_value()), Some(ty));
        }
        assert_eq!(LightType::from_ordinal_value(8), None);
        assert_eq!(LightType::default(), LightType::Point);
    }

    #[test]
    fn only_implemented_light_types_are_rendered() {
        assert!(LightType::Spot.is_rendered());
        assert!(LightType::Directional.is_rendered());
        assert!(LightType::Point.is_rendered());
        assert!(!LightType::Rectangle.is_rendered());
        assert!(!LightType::Disc.is_rendered());
        assert!(!LightType::Pyramid.is_rendered());
        assert!(!LightType::Box.is_rendered());
        assert!(!LightType::Tube.is_rendered());
    }

    #[test]
    fn light_shadows_and_render_mode_ordinals() {
        assert_eq!(LightShadows::None.ordinal_value(), 0);
        assert_eq!(LightShadows::Hard.ordinal_value(), 1);
        assert_eq!(LightShadows::Soft.ordinal_value(), 2);
        assert!(!LightShadows::None.casts_shadows());
        assert!(LightShadows::Hard.casts_shadows());
        assert!(LightShadows::Soft.casts_shadows());
        assert_eq!(LightShadows::from_ordinal_value(3), None);

        assert_eq!(LightRenderMode::Auto.ordinal_value(), 0);
        assert_eq!(LightRenderMode::ForcePixel.ordinal_value(), 1);
        assert_eq!(LightRenderMode::ForceVertex.ordinal_value(), 2);
        assert_eq!(LightRenderMode::from_ordinal_value(3), None);
    }

    #[test]
    fn light_enums_expose_dropdown_variants() {
        assert_eq!(
            LightType::enum_variants().unwrap(),
            &[
                "Spot",
                "Directional",
                "Point",
                "Rectangle",
                "Disc",
                "Pyramid",
                "Box",
                "Tube"
            ]
        );
        assert_eq!(
            LightShadows::enum_variants().unwrap(),
            &["None", "Hard", "Soft"]
        );
        assert_eq!(
            LightRenderMode::enum_variants().unwrap(),
            &["Auto", "ForcePixel", "ForceVertex"]
        );
    }

    #[test]
    fn light_enums_reflect_round_trip_uses_u8() {
        // 反射用 u8 编码（与 `CameraClearFlags` 一致），这样不需要额外的类型表条目。
        for ty in [LightType::Spot, LightType::Directional, LightType::Point] {
            assert_eq!(ty.to_value(), Value::U64(u64::from(ty.ordinal_value())));
            assert_eq!(LightType::from_value(ty.to_value()).unwrap(), ty);
        }
        assert_eq!(LightType::field_type(), FieldType::U8);
        assert!(LightType::from_value(Value::U64(99)).is_err());
        assert!(LightShadows::from_value(Value::U64(99)).is_err());
        assert!(LightRenderMode::from_value(Value::U64(99)).is_err());
    }

    #[test]
    fn shadow_casting_mode_ordinals_and_predicates() {
        assert_eq!(ShadowCastingMode::Off.ordinal_value(), 0);
        assert_eq!(ShadowCastingMode::On.ordinal_value(), 1);
        assert_eq!(ShadowCastingMode::TwoSided.ordinal_value(), 2);
        assert_eq!(ShadowCastingMode::ShadowsOnly.ordinal_value(), 3);
        assert_eq!(ShadowCastingMode::from_ordinal_value(4), None);
        assert_eq!(ShadowCastingMode::default(), ShadowCastingMode::On);
        assert_eq!(
            ShadowCastingMode::enum_variants().unwrap(),
            &["Off", "On", "TwoSided", "ShadowsOnly"]
        );

        assert!(!ShadowCastingMode::Off.casts_shadows());
        assert!(ShadowCastingMode::On.casts_shadows());
        assert!(ShadowCastingMode::TwoSided.casts_shadows());
        assert!(ShadowCastingMode::ShadowsOnly.casts_shadows());

        assert!(ShadowCastingMode::Off.visible_in_main_pass());
        assert!(ShadowCastingMode::On.visible_in_main_pass());
        assert!(ShadowCastingMode::TwoSided.visible_in_main_pass());
        assert!(!ShadowCastingMode::ShadowsOnly.visible_in_main_pass());

        // ：只有 `TwoSided` 在阴影通道关闭背面剔除。
        assert!(!ShadowCastingMode::Off.casts_two_sided_shadows());
        assert!(!ShadowCastingMode::On.casts_two_sided_shadows());
        assert!(ShadowCastingMode::TwoSided.casts_two_sided_shadows());
        assert!(!ShadowCastingMode::ShadowsOnly.casts_two_sided_shadows());

        for mode in [
            ShadowCastingMode::Off,
            ShadowCastingMode::On,
            ShadowCastingMode::TwoSided,
            ShadowCastingMode::ShadowsOnly,
        ] {
            assert_eq!(
                ShadowCastingMode::from_value(mode.to_value()).unwrap(),
                mode
            );
        }
    }

    #[test]
    fn color_temperature_goes_warm_to_cool() {
        // 低色温偏红、高色温偏蓝，6500 K 接近中性白。
        let warm = correlated_color_temperature_to_rgb(2000.0);
        assert!(warm.x > warm.z, "2000K 应偏暖: {warm:?}");

        let cool = correlated_color_temperature_to_rgb(15000.0);
        assert!(cool.z > cool.x, "15000K 应偏冷: {cool:?}");

        let daylight = correlated_color_temperature_to_rgb(6500.0);
        for c in [daylight.x, daylight.y, daylight.z] {
            assert!((0.9..=1.1).contains(&c), "6500K 应接近白色: {daylight:?}");
        }
    }

    #[test]
    fn color_temperature_clamps_and_falls_back() {
        let lowest = correlated_color_temperature_to_rgb(-100.0);
        assert_eq!(lowest, correlated_color_temperature_to_rgb(1000.0));
        let highest = correlated_color_temperature_to_rgb(1.0e9);
        assert_eq!(highest, correlated_color_temperature_to_rgb(40000.0));
        // 非有限值退化为 6500 K，而不是产生 NaN。
        let nan = correlated_color_temperature_to_rgb(f32::NAN);
        assert!(nan.is_finite());
    }

    #[test]
    fn cull_mode_ordinals() {
        // : Off=0, Front=1, Back=2。
        assert_eq!(CullMode::Off.ordinal_value(), 0);
        assert_eq!(CullMode::Front.ordinal_value(), 1);
        assert_eq!(CullMode::Back.ordinal_value(), 2);
        assert_eq!(CullMode::from_ordinal_value(3), None);
        assert_eq!(CullMode::default(), CullMode::Back);
        assert_eq!(
            CullMode::enum_variants().unwrap(),
            &["Off", "Front", "Back"]
        );

        assert!(!CullMode::Off.culls());
        assert!(CullMode::Front.culls());
        assert!(CullMode::Back.culls());

        for mode in [CullMode::Off, CullMode::Front, CullMode::Back] {
            assert_eq!(CullMode::from_value(mode.to_value()).unwrap(), mode);
            assert_eq!(
                serde_json::to_string(&mode).unwrap(),
                format!("\"{mode:?}\"")
            );
        }
        assert!(CullMode::from_value(Value::U64(9)).is_err());
    }

    #[test]
    fn compare_function_ordinals() {
        // : Disabled=0 … Always=8。
        let all = [
            (CompareFunction::Disabled, 0),
            (CompareFunction::Never, 1),
            (CompareFunction::Less, 2),
            (CompareFunction::Equal, 3),
            (CompareFunction::LessEqual, 4),
            (CompareFunction::Greater, 5),
            (CompareFunction::NotEqual, 6),
            (CompareFunction::GreaterEqual, 7),
            (CompareFunction::Always, 8),
        ];
        for (func, value) in all {
            assert_eq!(func.ordinal_value(), value);
            assert_eq!(CompareFunction::from_ordinal_value(value), Some(func));
            assert_eq!(CompareFunction::from_value(func.to_value()).unwrap(), func);
        }
        assert_eq!(CompareFunction::from_ordinal_value(9), None);
        // 着色器里 `ZTest` 的默认值是 LessEqual。
        assert_eq!(CompareFunction::default(), CompareFunction::LessEqual);
        assert!(!CompareFunction::Disabled.is_enabled());
        assert!(CompareFunction::LessEqual.is_enabled());
        assert_eq!(
            CompareFunction::enum_variants().unwrap(),
            &[
                "Disabled",
                "Never",
                "Less",
                "Equal",
                "LessEqual",
                "Greater",
                "NotEqual",
                "GreaterEqual",
                "Always",
            ]
        );
        assert!(CompareFunction::from_value(Value::U64(99)).is_err());
    }

    #[test]
    fn blend_mode_ordinals() {
        let all = [
            (BlendMode::Zero, 0),
            (BlendMode::One, 1),
            (BlendMode::DstColor, 2),
            (BlendMode::SrcColor, 3),
            (BlendMode::OneMinusDstColor, 4),
            (BlendMode::SrcAlpha, 5),
            (BlendMode::OneMinusSrcColor, 6),
            (BlendMode::DstAlpha, 7),
            (BlendMode::OneMinusDstAlpha, 8),
            (BlendMode::SrcAlphaSaturate, 9),
            (BlendMode::OneMinusSrcAlpha, 10),
        ];
        for (mode, value) in all {
            assert_eq!(mode.ordinal_value(), value);
            assert_eq!(BlendMode::from_ordinal_value(value), Some(mode));
            assert_eq!(BlendMode::from_value(mode.to_value()).unwrap(), mode);
        }
        assert_eq!(BlendMode::from_ordinal_value(11), None);
        // 不透明混合 = One / Zero。
        assert_eq!(BlendMode::default(), BlendMode::One);
        assert_eq!(BlendMode::enum_variants().unwrap().len(), 11);
        assert_eq!(BlendMode::enum_variants().unwrap()[10], "OneMinusSrcAlpha");
        assert!(BlendMode::from_value(Value::U64(99)).is_err());
    }

    #[test]
    fn blend_op_ordinals() {
        let table = [
            (BlendOp::Add, 0),
            (BlendOp::Subtract, 1),
            (BlendOp::ReverseSubtract, 2),
            (BlendOp::Min, 3),
            (BlendOp::Max, 4),
        ];
        for (op, value) in table {
            assert_eq!(op.ordinal_value(), value);
            assert_eq!(BlendOp::from_ordinal_value(value), Some(op));
            assert_eq!(
                BlendOp::from_value(Value::U64(u64::from(value))).unwrap(),
                op
            );
        }
        assert_eq!(BlendOp::from_ordinal_value(5), None);
        assert_eq!(BlendOp::default(), BlendOp::Add);
        assert_eq!(BlendOp::enum_variants().unwrap().len(), 5);
        assert_eq!(BlendOp::enum_variants().unwrap()[2], "ReverseSubtract");
        assert!(BlendOp::from_value(Value::U64(99)).is_err());
    }

    #[test]
    fn render_queue_numeric_values() {
        assert_eq!(RenderQueue::BACKGROUND, 1000);
        assert_eq!(RenderQueue::GEOMETRY, 2000);
        assert_eq!(RenderQueue::ALPHA_TEST, 2450);
        assert_eq!(RenderQueue::GEOMETRY_LAST, 2500);
        assert_eq!(RenderQueue::TRANSPARENT, 3000);
        assert_eq!(RenderQueue::OVERLAY, 4000);

        assert_eq!(RenderQueue::default(), RenderQueue::new(2000));
        assert_eq!(RenderQueue::default().value(), 2000);

        // 区间以 GeometryLast 为界；任意值都能无损保留。
        assert!(RenderQueue::new(1000).is_opaque());
        assert!(RenderQueue::new(2500).is_opaque());
        assert!(!RenderQueue::new(2501).is_opaque());
        assert!(RenderQueue::new(2501).is_transparent());
        assert!(RenderQueue::new(3000).is_transparent());
        // 负数可以无损保存，但不属于引擎的不透明区间 `[0, GeometryLast]`。
        assert!(!RenderQueue::new(-1).is_opaque());
        assert!(!RenderQueue::new(-1).is_transparent());
        assert!(!RenderQueue::new(-1000).is_opaque());
        assert_eq!(RenderQueue::new(4242).value(), 4242);

        // 反射按 i32 编码（不是 u8 枚举），负值也保留。
        assert_eq!(RenderQueue::field_type(), FieldType::I32);
        for value in [-5, 0, 2000, 4000] {
            let queue = RenderQueue::new(value);
            assert_eq!(queue.to_value(), Value::I64(i64::from(value)));
            assert_eq!(RenderQueue::from_value(queue.to_value()).unwrap(), queue);
        }
        assert!(RenderQueue::from_value(Value::I64(i64::from(i32::MAX) + 1)).is_err());
        assert!(RenderQueue::from_value(Value::Str("2000".into())).is_err());
    }
}
