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
    /// Bindless SRV slot of a background image (`None` = flat colour, #591).
    pub texture: Option<u32>,
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
    /// 工程自带字体的资产路径；`None` = 系统默认字体（#583）。
    pub font: Option<String>,
    /// 按矩形宽度换行（#527）；`false` = 单行。
    pub wrap: bool,
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
