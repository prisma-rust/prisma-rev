//! UI Render System —— 收集 UI 绘制命令
//!
//! 每帧查询所有 `Node + ComputedLayout` 的实体，生成有序的 `UiDrawList`。
//! `UiDrawList` 被设置到 `World` Resource 中，供 `GraphRenderer` 的 UI Pass 消费。

use crate::ui::components::*;
use crate::ui::{is_ui_active, ScreenSize};
use prism_ecs::World;

// ── 绘制命令 ─────────────────────────────────────────────────

/// 一个带背景色的矩形（圆角暂由 shader 处理）。
#[derive(Clone, Copy, Debug)]
pub struct UiQuad {
    /// 屏幕空间矩形 `[left, top, width, height]`
    pub rect: [f32; 4],
    /// RGBA 颜色 0..=1，按 **sRGB** 作者值解释（渲染前解码到线性，#592）。
    pub color: [f32; 4],
    /// 圆角半径（px）
    pub border_radius: f32,
    /// 背景图 bindless 槽位（#591）；`None` = 纯色填充。
    pub texture: Option<u32>,
    /// 绘制层级（z-index）。层级内再按 [`UiQuad::order`] 排。
    ///
    /// 普通 UI 节点都是 0；需要「永远压在某层之上」的东西（雨滴粒子、调试
    /// 叠加）显式写更大的值。
    pub layer: i32,
    /// 层级内的绘制顺序，来自 [`ComputedLayout::order`]。
    pub order: u32,
}

impl Default for UiQuad {
    fn default() -> Self {
        Self {
            rect: [0.0; 4],
            color: [0.0; 4],
            border_radius: 0.0,
            texture: None,
            layer: 0,
            order: EXTERNAL_UI_ORDER,
        }
    }
}

/// 一个文本绘制命令。
#[derive(Clone, Debug)]
pub struct UiTextCmd {
    pub rect: [f32; 4],
    pub content: String,
    pub font_size: f32,
    /// RGBA sRGB 作者值（与 [`UiQuad::color`] 相同）。
    pub color: [f32; 4],
    pub alignment: TextAlign,
    /// 工程自带字体的资产路径；空 = 系统默认字体（#583）。
    pub font: String,
    /// 按矩形宽度换行（#527）；`false` = 单行。
    pub wrap: bool,
    /// 见 [`UiQuad::layer`]。
    pub layer: i32,
    /// 见 [`UiQuad::order`]。
    pub order: u32,
}

/// 没有层级来源的绘制命令（外部提交的 quad / 粒子）排在所有 UI 实体之后。
pub const EXTERNAL_UI_ORDER: u32 = u32::MAX;

/// 一条绘制命令。
///
/// 背景矩形和文本放在**同一个**列表里，按 `(layer, order, 类型)` 排序后交错
/// 绘制——这正是 UGUI 在 Canvas 里做的事（#462）：文字不再无条件压在所有背景
/// 之上，后加的全屏遮罩能盖住先前的标题。
#[derive(Clone, Debug)]
pub enum UiDrawItem {
    Quad(UiQuad),
    Text(UiTextCmd),
}

/// UI 绘制命令列表 —— 已按绘制顺序排好，供渲染 Pass 顺序消费。
#[derive(Clone, Debug, Default)]
pub struct UiDrawList {
    pub items: Vec<UiDrawItem>,
}

impl UiDrawList {
    pub fn push_quad(&mut self, quad: UiQuad) {
        self.items.push(UiDrawItem::Quad(quad));
    }

    pub fn push_text(&mut self, text: UiTextCmd) {
        self.items.push(UiDrawItem::Text(text));
    }

    pub fn extend_quads(&mut self, quads: impl IntoIterator<Item = UiQuad>) {
        self.items.extend(quads.into_iter().map(UiDrawItem::Quad));
    }

    pub fn quads(&self) -> impl Iterator<Item = &UiQuad> {
        self.items.iter().filter_map(|item| match item {
            UiDrawItem::Quad(quad) => Some(quad),
            UiDrawItem::Text(_) => None,
        })
    }

    pub fn texts(&self) -> impl Iterator<Item = &UiTextCmd> {
        self.items.iter().filter_map(|item| match item {
            UiDrawItem::Quad(_) => None,
            UiDrawItem::Text(text) => Some(text),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// 外部（宿主 / 项目模块）提交的 UI 四边形。
///
/// `ui_render_system` 每帧重建 `UiDrawList`，所以提交方必须在**同一帧的 UI
/// 渲染系统之前**写入本资源；系统会把它们并入绘制列表（参与 `layer`/`order`
/// 排序）并清空资源，避免跨帧重复绘制。
#[derive(Clone, Debug, Default)]
pub struct ExternalUiQuads(pub Vec<UiQuad>);

/// 布局节点的 Canvas 缩放（#463）；坏值按 1 处理，避免 NaN 扩散到顶点。
fn layout_scale(layout: &ComputedLayout) -> f32 {
    if layout.scale.is_finite() && layout.scale > 0.0 {
        layout.scale
    } else {
        1.0
    }
}

/// 每帧更新 `UiDrawList` resource —— 查询所有 UI 实体生成绘制命令。
pub fn ui_render_system(world: &mut World) {
    // (layer, order, 类型：0 = 矩形 / 1 = 文本, 序号) —— 序号让排序稳定。
    let mut pending: Vec<((i32, u32, u8, u32), UiDrawItem)> = Vec::new();

    // 外部提交的四边形先并入，随后的排序会把它们放到正确的层。
    if let Some(external) = world.remove_resource::<ExternalUiQuads>() {
        for (seq, quad) in external.0.into_iter().enumerate() {
            let layer = quad.layer;
            let order = quad.order;
            pending.push(((layer, order, 0, seq as u32), UiDrawItem::Quad(quad)));
        }
    }

    // 收集背景矩形
    for (entity, layout, style) in world.query2::<ComputedLayout, Style>() {
        if !is_ui_active(world, entity) {
            continue;
        }
        if !style.visible {
            continue;
        }
        // #528：`Button` 按交互状态换色，没有 `Button` 时继续用
        // `Style::background`（两者可以共存，按钮优先）。
        let texture = world
            .get::<crate::scene::runtime_components::UiImageRef>(entity)
            .filter(|image| image.generation > 0)
            .map(|image| image.texture_slot);
        let mut bg = match world.get::<Button>(entity) {
            Some(button) => button.color(world.get::<Interaction>(entity)),
            None => style.background,
        };
        // #591：有背景图时 `background` 只当作 tint；整掉 alpha 说明作者没写
        // tint，就按不透明白色显示图片原色。
        if texture.is_some() && bg[3] <= 0.0 {
            bg = [1.0, 1.0, 1.0, 1.0];
        }
        if bg[3] <= 0.0 && texture.is_none() {
            continue;
        } // 完全透明跳过

        let seq = pending.len() as u32;
        // #463：圆角是「设计像素」，按所在 Canvas 的缩放换算到屏幕像素。
        let scale = layout_scale(layout);
        pending.push((
            (0, layout.order, 0, seq),
            UiDrawItem::Quad(UiQuad {
                rect: layout.rect,
                color: bg,
                border_radius: style.border_radius * scale,
                texture,
                layer: 0,
                order: layout.order,
            }),
        ));
    }

    // 收集文本
    for (entity, layout, text) in world.query2::<ComputedLayout, Text>() {
        if !is_ui_active(world, entity) {
            continue;
        }
        // 与上面的 quad 一致：`Style::visible == false` 的实体不产生绘制命令。
        // 文本实体通常也挂 `Style`（`Node + Style + Text`），没有 `Style` 时按可见处理。
        if matches!(world.get::<Style>(entity), Some(style) if !style.visible) {
            continue;
        }
        if text.content.is_empty() {
            continue;
        }

        let seq = pending.len() as u32;
        pending.push((
            (0, layout.order, 1, seq),
            UiDrawItem::Text(UiTextCmd {
                rect: layout.rect,
                content: text.content.clone(),
                // #463：字号同样按 Canvas 缩放，否则 4K 屏幕上文字只有一半大。
                font_size: text.font_size * layout_scale(layout),
                color: text.color,
                alignment: text.alignment,
                font: text.font.clone(),
                wrap: text.wrap,
                layer: 0,
                order: layout.order,
            }),
        ));
    }

    // 先层、再层级顺序、最后「同一节点里背景在文本之前」。
    pending.sort_by_key(|(key, _)| *key);

    let mut draw_list = UiDrawList {
        items: Vec::with_capacity(pending.len()),
    };
    draw_list
        .items
        .extend(pending.into_iter().map(|(_, item)| item));

    world.insert_resource(draw_list);
}

/// 将引擎的 `UiDrawList`（像素坐标）转换为渲染器的 `UiOverlayInput`（NDC）。
///
/// 从 world 的 `ScreenSize` resource 读取屏幕尺寸。
pub fn convert_ui_draw_list_to_overlay(world: &World) -> prism_render::UiOverlayInput {
    let mut out = prism_render::UiOverlayInput::default();
    convert_ui_draw_list_to_overlay_into(world, &mut out);
    out
}

/// [`convert_ui_draw_list_to_overlay`] into a caller-owned buffer.
///
/// The frame packet owns the `UiOverlayInput` across frames, so this refills
/// the existing `quads` / `texts` allocations instead of building new ones (#66).
pub fn convert_ui_draw_list_to_overlay_into(world: &World, out: &mut prism_render::UiOverlayInput) {
    out.clear();

    let screen = world
        .get_resource::<ScreenSize>()
        .copied()
        .unwrap_or(ScreenSize {
            width: 1920.0,
            height: 1080.0,
        });
    let w = screen.width as f32;
    let h = screen.height as f32;

    let Some(draw_list) = world.get_resource::<UiDrawList>() else {
        return;
    };

    for item in &draw_list.items {
        let q = match item {
            UiDrawItem::Quad(quad) => quad,
            UiDrawItem::Text(text) => {
                // 文本命令按像素坐标原样透传（record 时展开成逐字形四边形）。
                out.push_text(prism_render::ui_overlay::UiTextInput {
                    rect: text.rect,
                    content: text.content.clone(),
                    font_size: text.font_size,
                    color: text.color,
                    alignment: match text.alignment {
                        TextAlign::Left => prism_render::ui_overlay::UiTextAlign::Left,
                        TextAlign::Center => prism_render::ui_overlay::UiTextAlign::Center,
                        TextAlign::Right => prism_render::ui_overlay::UiTextAlign::Right,
                    },
                    font: (!text.font.is_empty()).then(|| text.font.clone()),
                    wrap: text.wrap,
                });
                continue;
            }
        };
        let [px, py, pw, ph] = q.rect; // pixel left, top, width, height
                                       // NDC: [-1,1] x [-1,1]，origin center。**Vulkan 的 Y 轴朝下**：
                                       // `y = -1` 是屏幕顶、`+1` 是屏幕底（相机投影里的
                                       // `p[1][1] = -inv_tan` 正是按这个约定翻的）。
                                       // 像素坐标 y 也是向下增长，所以这里**不翻**：
                                       // 再翻一次会把整个 UI 层上下镜像。
        let x0 = (px / w) * 2.0 - 1.0;
        let y0 = (py / h) * 2.0 - 1.0;
        let x1 = ((px + pw) / w) * 2.0 - 1.0;
        let y1 = ((py + ph) / h) * 2.0 - 1.0;
        // NDC border 半径：按 quad 自身尺寸归一化，渲染侧再乘回像素尺寸
        // 才能得到与作者写法一致的圆角（#634）。
        let br_ndc = if pw.max(ph) > 0.0 {
            q.border_radius / pw.max(ph) * 2.0
        } else {
            0.0
        };
        out.push_quad(prism_render::ui_overlay::UiQuad {
            rect: [x0, y0, x1, y1], // NDC [xmin, ymin, xmax, ymax]
            color: q.color,
            border_radius: br_ndc,
            texture: q.texture,
        });
    }
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
