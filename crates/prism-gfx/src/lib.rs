//! Render-state data (enums) shared across crates.
//!
//! Both sides consume these types:
//! - `prism-scene`: the authoring format (`.scene.json`), reflection schema and
//!   RSCN encoding;
//! - `prism-render`: translation into Vulkan pipeline and clear state.
//!
//! They therefore live in a neutral crate both can depend on rather than in
//! either side. Two hard constraints apply:
//!
//! 1. **Pure data**: no ash/Vulkan and no ECS dependency. Translating the
//!    semantics into pipeline state is `prism-render`'s job (for example
//!    `cull_flags()` and the blend-factor mapping).
//! 2. Reflection encodes them as small integers (a `U8` enum, `I32` for
//!    `RenderQueue`) so RSCN and remote tools need no extra type-table entries.
//!
//! `prism-scene::render_enums` is only a re-export of this crate; the legacy
//! paths (`prism_scene::CullMode`, `prism_scene::render_enums::*`) stay valid.

use prism_reflect::{FieldType, ReflectError, ReflectType, Value};
use serde::{Deserialize, Serialize};

/// How the camera clears the framebuffer.
///
/// The numeric values are fixed: `(int)Skybox == 1` … `(int)Nothing == 4`.
/// Zero is reserved by the engine as an unset/invalid value, so [`Default`]
/// stands in for it. Serde uses the variant names (`"Skybox"` /
/// `"SolidColor"` / `"Depth"` / `"Nothing"`), matching the other enums in
/// `.scene.json` such as `TextAlign`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CameraClearFlags {
    /// Fill the background with the skybox.
    #[default]
    Skybox,
    /// Fill the background with a solid color (see `Camera::background_color`).
    SolidColor,
    /// Clear depth only, keeping existing color (used by overlay cameras).
    Depth,
    /// Clear neither color nor depth.
    Nothing,
}

impl CameraClearFlags {
    /// The engine's ordinal value.
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Skybox => 1,
            Self::SolidColor => 2,
            Self::Depth => 3,
            Self::Nothing => 4,
        }
    }

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Skybox),
            2 => Some(Self::SolidColor),
            3 => Some(Self::Depth),
            4 => Some(Self::Nothing),
            _ => None,
        }
    }

    /// Whether this clears the color buffer (`Skybox` / `SolidColor`).
    pub const fn clears_color(self) -> bool {
        matches!(self, Self::Skybox | Self::SolidColor)
    }

    /// Whether this clears the depth buffer (everything except `Nothing`).
    pub const fn clears_depth(self) -> bool {
        !matches!(self, Self::Nothing)
    }

    /// Whether the skybox background is drawn.
    ///
    /// Only [`Skybox`](Self::Skybox) fills the background with the skybox
    /// material; the other clear modes (solid color, depth-only, neither) draw
    /// nothing even when the scene has an enabled `Skybox` entity. The forward
    /// pass in `prism-render` implements the solid-color fill and the
    /// multi-camera depth compositing (#461).
    pub const fn draws_skybox(self) -> bool {
        matches!(self, Self::Skybox)
    }
}

impl ReflectType for CameraClearFlags {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        // Same order as the declaration; these strings are the values used in `.scene.json`.
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

/// Light type (fixed values).
///
/// The engine's inspector only offers the types the runtime supports; the full
/// enum is kept here so authoring data round-trips without loss and can be
/// migrated, but the renderer currently implements only
/// [`Spot`](Self::Spot) / [`Directional`](Self::Directional) /
/// [`Point`](Self::Point) (see [`LightType::is_rendered`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightType {
    /// Spot light (cone).
    Spot,
    /// Directional light (infinitely far; direction comes from the entity transform).
    Directional,
    /// Point light (omnidirectional).
    #[default]
    Point,
    /// Rectangular area light = `LightType.Rectangle` (the new name for `Area`).
    Rectangle,
    /// Disc-shaped area light.
    Disc,
    /// Pyramid-shaped spot light.
    Pyramid,
    /// Box-shaped spot light.
    Box,
    /// Tube light.
    Tube,
}

impl LightType {
    /// The engine's ordinal value.
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

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
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

    /// Whether the current renderer actually draws this type.
    ///
    /// Area, box and tube lights are schema alignment only and `collect_lights`
    /// skips them; they are reserved for baking or would need extra pipeline
    /// support.
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

/// Shadow mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightShadows {
    /// Casts no shadows.
    #[default]
    None,
    /// Hard shadows (single sample).
    Hard,
    /// Soft shadows (PCF).
    Soft,
}

impl LightShadows {
    /// The engine's ordinal value.
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Hard => 1,
            Self::Soft => 2,
        }
    }

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::Hard),
            2 => Some(Self::Soft),
            _ => None,
        }
    }

    /// Whether this casts shadows (`Hard` / `Soft`).
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

/// Light render mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightRenderMode {
    /// Decided by the renderer (per-pixel or per-vertex).
    #[default]
    Auto,
    /// Force per-pixel.
    ForcePixel,
    /// Force per-vertex.
    ForceVertex,
}

impl LightRenderMode {
    /// The engine's ordinal value.
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Auto => 0,
            Self::ForcePixel => 1,
            Self::ForceVertex => 2,
        }
    }

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
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

/// Shadow casting mode.
///
/// Set on the renderer (see
/// `prism_scene::components::MeshRenderer::shadow_casting_mode`); it decides
/// whether the object is **visible in the main pass** and whether it **writes
/// the shadow map**.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShadowCastingMode {
    /// Casts nothing and renders normally.
    Off,
    /// Renders normally and casts shadows (default).
    #[default]
    On,
    /// Two-sided shadow casting plus normal rendering.
    ///
    /// Corresponds to `ShadowCastingMode.TwoSided`: back-face culling is
    /// **disabled while shadows are rendered** (even when the material itself
    /// enables it), so single-sided geometry casts from both sides (see
    /// [`casts_two_sided_shadows`](Self::casts_two_sided_shadows)).
    TwoSided,
    /// Casts shadows only; invisible in the main pass.
    ShadowsOnly,
}

impl ShadowCastingMode {
    /// The engine's ordinal value.
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::TwoSided => 2,
            Self::ShadowsOnly => 3,
        }
    }

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            2 => Some(Self::TwoSided),
            3 => Some(Self::ShadowsOnly),
            _ => None,
        }
    }

    /// Whether this writes the shadow map (`On` / `TwoSided` / `ShadowsOnly`).
    pub const fn casts_shadows(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Whether this appears in the camera's main pass (`Off` / `On` / `TwoSided`).
    pub const fn visible_in_main_pass(self) -> bool {
        !matches!(self, Self::ShadowsOnly)
    }

    /// Whether shadow rendering disables back-face culling
    /// (`ShadowCastingMode.TwoSided`).
    ///
    /// `TwoSided` disables back-face culling **while shadows are rendered**,
    /// even when the object's material enables it. `Off` / `On` /
    /// `ShadowsOnly` keep back-face culling (`false`); only `TwoSided` casts
    /// from both sides (`true`). This affects the shadow pass only, not
    /// [`visible_in_main_pass`](Self::visible_in_main_pass).
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

/// Cull mode (fixed values).
///
/// Defaults to [`Back`](Self::Back), matching the existing `prism-render`
/// graphics pipeline's `vk::CullModeFlags::BACK`. `prism-render` performs the
/// actual translation into Vulkan state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CullMode {
    /// No culling (both sides visible).
    Off,
    /// Cull front faces, keeping only back faces.
    Front,
    /// Cull back faces (default).
    #[default]
    Back,
}

impl CullMode {
    /// The engine's ordinal value.
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Front => 1,
            Self::Back => 2,
        }
    }

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
    pub const fn from_ordinal_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Front),
            2 => Some(Self::Back),
            _ => None,
        }
    }

    /// Whether this culls a face (`Front` / `Back`).
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

/// Depth/stencil compare function (fixed values).
///
/// Defaults to [`LessEqual`](Self::LessEqual), the shader's default `ZTest`
/// value. [`Disabled`](Self::Disabled) means the engine's "depth test off";
/// Vulkan has no matching `vk::CompareOp`, so `prism-render` expresses it with
/// its own depth-test flag (`depth_test_enable(false)`, i.e. `VK_FALSE`)
/// instead of inverting a comparison.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareFunction {
    /// No comparison (equivalent to no test).
    Disabled,
    /// Never passes.
    Never,
    /// Less than.
    Less,
    /// Equal to.
    Equal,
    /// Less than or equal to (default).
    #[default]
    LessEqual,
    /// Greater than.
    Greater,
    /// Not equal to.
    NotEqual,
    /// Greater than or equal to.
    GreaterEqual,
    /// Always passes.
    Always,
}

impl CompareFunction {
    /// The engine's ordinal value.
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

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
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

    /// Whether a comparison is actually performed (everything except [`Disabled`](Self::Disabled)).
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

/// Blend factor (fixed values).
///
/// The engine's blend state is "source factor × target factor, combined by a
/// blend op". This enum describes the **factors**; see [`BlendOp`] for the op.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlendMode {
    /// `0`.
    Zero,
    /// `1` (default source factor; with [`Zero`](Self::Zero) it produces opaque blending).
    #[default]
    One,
    /// Destination color.
    DstColor,
    /// Source color.
    SrcColor,
    /// `1 - dst`.
    OneMinusDstColor,
    /// Source alpha.
    SrcAlpha,
    /// `1 - src`.
    OneMinusSrcColor,
    /// Destination alpha.
    DstAlpha,
    /// `1 - dst.a`.
    OneMinusDstAlpha,
    /// `min(src.a, 1 - dst.a)`.
    SrcAlphaSaturate,
    /// `1 - src.a` (the source factor for standard alpha blending).
    OneMinusSrcAlpha,
}

impl BlendMode {
    /// The engine's ordinal value.
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

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
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

/// Blend operation.
///
/// Together with [`BlendMode`] this forms a material's blend state:
/// `result = srcFactor × src op dstFactor × dst`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlendOp {
    /// `src + dst` (default).
    #[default]
    Add,
    /// `src - dst`.
    Subtract,
    /// `dst - src`.
    ReverseSubtract,
    /// `min(src, dst)`.
    Min,
    /// `max(src, dst)`.
    Max,
}

impl BlendOp {
    /// The engine's ordinal value.
    pub const fn ordinal_value(self) -> u32 {
        match self {
            Self::Add => 0,
            Self::Subtract => 1,
            Self::ReverseSubtract => 2,
            Self::Min => 3,
            Self::Max => 4,
        }
    }

    /// Reconstructs from the engine's ordinal value; returns `None` if unknown.
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

/// Render queue.
///
/// **Shape note**: the engine's `RenderQueue` is a struct wrapping an `int`,
/// not an enum, and its named queues are just `static readonly int` constants.
/// This mirrors that shape as an `i32` newtype plus associated constants, so
/// any queue value in authoring data round-trips without loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RenderQueue(i32);

impl RenderQueue {
    /// Background (drawn first).
    pub const BACKGROUND: i32 = 1000;
    /// Opaque geometry (the default queue).
    pub const GEOMETRY: i32 = 2000;
    /// After alpha testing, before the end of opaque geometry.
    pub const ALPHA_TEST: i32 = 2450;
    /// Upper bound of the opaque range.
    pub const GEOMETRY_LAST: i32 = 2500;
    /// Transparent objects.
    pub const TRANSPARENT: i32 = 3000;
    /// Overlay (drawn last).
    pub const OVERLAY: i32 = 4000;

    /// Constructs from an arbitrary queue value.
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    /// The queue value (the engine's `RenderQueue.value`).
    pub const fn value(self) -> i32 {
        self.0
    }

    /// Whether this falls in the opaque range (`[0, GeometryLast]`).
    ///
    /// Negative values are allowed (saved data can preserve any queue value
    /// losslessly) but are outside the engine's opaque range, so the lower
    /// bound must be 0.
    pub const fn is_opaque(self) -> bool {
        self.0 >= 0 && self.0 <= Self::GEOMETRY_LAST
    }

    /// Whether this falls in the transparent range (`> GeometryLast`).
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

/// Converts a color temperature (Kelvin) to linear RGB, matching
/// `Mathf.CorrelatedColorTemperatureToRGB`.
///
/// The engine's version is native; this uses the common black-body
/// approximation (Tanner Helland's piecewise fit): compute the 0–1 sRGB
/// components, convert to linear, and clamp to `[1000, 40000]` K (the
/// inspector's range). The result is **not** normalized: 6500 K is close to
/// `(1, 1, 1)`, and the temperature just acts as a color filter multiplied
/// into `Light.color`.
pub fn correlated_color_temperature_to_rgb(kelvin: f32) -> glam::Vec3 {
    let kelvin = if kelvin.is_finite() {
        kelvin.clamp(1000.0, 40000.0)
    } else {
        6500.0
    };
    let t = kelvin / 100.0;

    let srgb = |c: f32| {
        let c = c.clamp(0.0, 255.0) / 255.0;
        // sRGB → linear: the engine's light colors are linear in the renderer.
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

        // Only Skybox draws the skybox background (`Camera.clearFlags` semantics).
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
        // Ordinals: Spot=0, Directional=1, Point=2, Rectangle(Area)=3, Disc=4,
        // Pyramid=5, Box=6, Tube=7.
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
        // Reflection uses u8 encoding (like `CameraClearFlags`) so no extra
        // type-table entries are needed.
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

        // Only `TwoSided` disables back-face culling in the shadow pass.
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
        // Low temperatures are warm/red, high temperatures cool/blue; 6500 K is near neutral white.
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
        // Non-finite input falls back to 6500 K instead of producing NaN.
        let nan = correlated_color_temperature_to_rgb(f32::NAN);
        assert!(nan.is_finite());
    }

    #[test]
    fn cull_mode_ordinals() {
        // Ordinals: Off=0, Front=1, Back=2.
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
        // Ordinals: Disabled=0 … Always=8.
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
        // The shader's default `ZTest` value is LessEqual.
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
        // Opaque blending = One / Zero.
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

        // The ranges are bounded by GeometryLast; any value is preserved losslessly.
        assert!(RenderQueue::new(1000).is_opaque());
        assert!(RenderQueue::new(2500).is_opaque());
        assert!(!RenderQueue::new(2501).is_opaque());
        assert!(RenderQueue::new(2501).is_transparent());
        assert!(RenderQueue::new(3000).is_transparent());
        // Negative values are preserved losslessly but are outside the engine's
        // opaque range `[0, GeometryLast]`.
        assert!(!RenderQueue::new(-1).is_opaque());
        assert!(!RenderQueue::new(-1).is_transparent());
        assert!(!RenderQueue::new(-1000).is_opaque());
        assert_eq!(RenderQueue::new(4242).value(), 4242);

        // Reflection uses i32 encoding (not a u8 enum) so negative values survive.
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
