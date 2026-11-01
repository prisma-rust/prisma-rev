//! UI Layout System —— 锚点布局计算
//!
//! 每帧查询所有 `Node + Style` 的实体，按需补上 `ComputedLayout` 输出组件，
//! 再根据锚点、固定尺寸、边距和父容器尺寸计算出最终屏幕空间矩形。
//!
//! # 布局规则
//! - 根元素（无 Parent）的父容器 = 屏幕尺寸（由 `ScreenSize` resource 提供）
//! - 子元素的父容器 = 父元素的 `ComputedLayout`
//! - 锚点定义子边相对于父边的映射
//! - 固定尺寸覆盖锚点拉伸
//!
//! # Canvas 缩放（#463）
//! 带 [`Canvas`](crate::ui::components::Canvas) 的节点会给整棵子树定一个缩放
//! 系数：布局在「虚拟分辨率」里算（`ScreenSize / scale`），写回
//! `ComputedLayout.rect` 时再乘回 `scale`。这样作者按设计稿像素写
//! `Style.width` / `margin` / 锚点，1080p 和 720p 得到同一套构图。
//! `ComputedLayout.scale` 留给渲染系统换算字号和圆角。

use crate::ui::components::*;
use prism_ecs::World;

/// 屏幕尺寸 resource —— 每帧由 engine 更新。
#[derive(Clone, Copy, Debug)]
pub struct ScreenSize {
    pub width: f32,
    pub height: f32,
}

impl ScreenSize {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width as f32,
            height: height as f32,
        }
    }
}

/// Layout System —— 每帧运行，计算所有 UI 元素的屏幕空间矩形。
pub fn ui_layout_system(world: &mut World) {
    let screen = world
        .get_resource::<ScreenSize>()
        .copied()
        .unwrap_or(ScreenSize {
            width: 1920.0,
            height: 1080.0,
        });

    // 先收集所有 `Node + Style` 实体及 Parent。
    // 层级只认场景的 `Parent`（`HierarchyHelper` 维护的那一份），不再有第二份
    // `UiParent` 状态。`ComputedLayout` 是本系统的**输出**：作者不写它，
    // 缺失时在这里补默认值，否则布局 / 命中测试 / 绘制（都以它为查询条件）
    // 会静默跳过该实体。
    // 分两遍：第一遍处理无 Parent 的根节点，第二遍处理子节点
    let layout_queue: Vec<(prism_ecs::Entity, Option<prism_ecs::Entity>)> = world
        .query2::<Node, Style>()
        .map(|(entity, _node, _style)| {
            let parent = world.get::<prism_scene::components::Parent>(entity);
            (entity, parent.map(|p| p.0))
        })
        .collect();

    for (entity, _parent) in &layout_queue {
        if world.get::<ComputedLayout>(*entity).is_none() {
            world.insert(*entity, ComputedLayout::default());
        }
    }

    // 每个节点所在 Canvas 的缩放（#463）。最近的 Canvas 祖先说了算；
    // 没有 Canvas 时是 1.0，行为与引入 Canvas 之前一致。
    let canvases: std::collections::HashMap<prism_ecs::Entity, Canvas> = world
        .query::<Canvas>()
        .map(|(entity, canvas)| (entity, *canvas))
        .collect();
    let parents: std::collections::HashMap<prism_ecs::Entity, prism_ecs::Entity> = layout_queue
        .iter()
        .filter_map(|(entity, parent)| parent.map(|parent| (*entity, parent)))
        .collect();
    let mut scales: std::collections::HashMap<prism_ecs::Entity, f32> =
        std::collections::HashMap::new();
    for (entity, _) in &layout_queue {
        let scale = node_scale(*entity, &parents, &canvases, &screen, &mut scales, 0);
        scales.insert(*entity, scale);
    }

    // 布局组的父子关系（#527）。兄弟顺序必须和绘制顺序一致：按场景 `id`
    // （运行时 `spawn` 的实体排在后面），这样「先加的在左/上」符合直觉，
    // 也和 `assign_draw_order` 完全同源。
    let layout_entities: std::collections::HashSet<prism_ecs::Entity> =
        layout_queue.iter().map(|(entity, _)| *entity).collect();
    let mut children: std::collections::HashMap<prism_ecs::Entity, Vec<prism_ecs::Entity>> =
        std::collections::HashMap::new();
    let mut roots: Vec<prism_ecs::Entity> = Vec::new();
    for (entity, parent) in &layout_queue {
        match parent.filter(|parent| layout_entities.contains(parent)) {
            Some(parent) => children.entry(parent).or_default().push(*entity),
            None => roots.push(*entity),
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(|entity| sibling_key(world, *entity));
    }
    roots.sort_by_key(|entity| sibling_key(world, *entity));

    // 父先子后的遍历顺序。`seen` 同时防住 Parent 成环。
    let mut order: Vec<prism_ecs::Entity> = Vec::new();
    let mut seen: std::collections::HashSet<prism_ecs::Entity> = std::collections::HashSet::new();
    let mut stack: Vec<prism_ecs::Entity> = roots.iter().rev().copied().collect();
    while let Some(entity) = stack.pop() {
        if !seen.insert(entity) {
            continue;
        }
        order.push(entity);
        if let Some(kids) = children.get(&entity) {
            for kid in kids.iter().rev() {
                stack.push(*kid);
            }
        }
    }

    let styles: std::collections::HashMap<prism_ecs::Entity, Style> = layout_queue
        .iter()
        .filter_map(|(entity, _)| {
            world
                .get::<Style>(*entity)
                .map(|style| (*entity, style.clone()))
        })
        .collect();

    // 第一遍 measure：子先父后。布局组用子元素的尺寸量出自己的内容尺寸，
    // `fit_content` 时据此收缩（#527）。
    let measured = measure_sizes(&order, &children, &styles);

    // 第二遍 arrange：父先子后。布局组的子元素按排列顺序定位，其余节点继续
    // 走「锚点 × 父矩形」的原有算法。
    let mut resolved: std::collections::HashMap<prism_ecs::Entity, [f32; 4]> =
        std::collections::HashMap::new();
    // 每个布局组的主轴游标（虚拟坐标，相对内容区起点）。
    let mut cursors: std::collections::HashMap<prism_ecs::Entity, f32> =
        std::collections::HashMap::new();

    for entity in &order {
        let Some(style) = styles.get(entity) else {
            continue;
        };
        let own = measured.get(entity).copied();
        let scale = scales.get(entity).copied().unwrap_or(1.0);
        let root_rect = [0.0, 0.0, screen.width / scale, screen.height / scale];

        let parent = parents.get(entity).copied();
        let rect = match parent {
            // 父级不是 UI 实体（没有 Node + Style）：沿用它已有的输出矩形。
            Some(parent) if !layout_entities.contains(&parent) => {
                let parent_rect = world
                    .get::<ComputedLayout>(parent)
                    .map(|layout| {
                        let parent_scale = if layout.scale.is_finite() && layout.scale > 0.0 {
                            layout.scale
                        } else {
                            1.0
                        };
                        [
                            layout.rect[0] / parent_scale,
                            layout.rect[1] / parent_scale,
                            layout.rect[2] / parent_scale,
                            layout.rect[3] / parent_scale,
                        ]
                    })
                    .unwrap_or(root_rect);
                anchored_rect(style, parent_rect, own)
            }
            Some(parent) => {
                let parent_rect = resolved.get(&parent).copied().unwrap_or(root_rect);
                match styles.get(&parent) {
                    Some(parent_style)
                        if parent_style.layout.direction != LayoutDirection::None =>
                    {
                        place_in_layout(parent, parent_style.layout, parent_rect, own, &mut cursors)
                    }
                    _ => anchored_rect(style, parent_rect, own),
                }
            }
            None => anchored_rect(style, root_rect, own),
        };

        let screen_rect = [
            rect[0] * scale,
            rect[1] * scale,
            rect[2] * scale,
            rect[3] * scale,
        ];
        if let Some(layout) = world.get_mut::<ComputedLayout>(*entity) {
            layout.rect = screen_rect;
            layout.scale = scale;
        } else {
            world.insert(
                *entity,
                ComputedLayout {
                    rect: screen_rect,
                    order: 0,
                    scale,
                },
            );
        }
        resolved.insert(*entity, rect);
    }
    assign_draw_order(world);
}

/// 实体所在 Canvas 的缩放（#463）：最近的 Canvas 祖先决定，没有则 1.0。
///
/// 递归 + 记忆化；`depth` 上限防止坏层级（A 的父是 B、B 的父是 A）无限递归。
fn node_scale(
    entity: prism_ecs::Entity,
    parents: &std::collections::HashMap<prism_ecs::Entity, prism_ecs::Entity>,
    canvases: &std::collections::HashMap<prism_ecs::Entity, Canvas>,
    screen: &ScreenSize,
    cache: &mut std::collections::HashMap<prism_ecs::Entity, f32>,
    depth: u32,
) -> f32 {
    if let Some(scale) = cache.get(&entity) {
        return *scale;
    }
    if depth > 64 {
        return 1.0;
    }
    let scale = if let Some(canvas) = canvases.get(&entity) {
        canvas.scale_factor((screen.width, screen.height))
    } else if let Some(parent) = parents.get(&entity) {
        node_scale(*parent, parents, canvases, screen, cache, depth + 1)
    } else {
        1.0
    };
    cache.insert(entity, scale);
    scale
}

/// 给每个 UI 实体算 `ComputedLayout::order`（绘制顺序）。
///
/// 语义对齐 UGUI：**同一父级下按兄弟顺序深度优先遍历**，父先于子，一个节点的
/// 整棵子树都排在它的下一个兄弟之前。这样「后加的全屏遮罩」才能盖住前面的标题
/// 文字（#462），而不是永远按 0/1 两个 layer 分批画。
///
/// 兄弟顺序取 `.scene.json` 的 `id`（`SceneEntityId`）；运行时 `spawn` 出来的
/// 实体没有它，排在所有场景实体之后（按实体分配下标），和 UGUI 里新
/// `Instantiate` 的兄弟默认追加到末尾一致。
pub fn assign_draw_order(world: &mut World) {
    use std::collections::HashMap;

    let mut children: HashMap<prism_ecs::Entity, Vec<prism_ecs::Entity>> = HashMap::new();
    let mut roots: Vec<prism_ecs::Entity> = Vec::new();
    let mut all: Vec<prism_ecs::Entity> = Vec::new();

    for (entity, _node, _style) in world.query2::<Node, Style>() {
        all.push(entity);
    }

    let is_ui: std::collections::HashSet<prism_ecs::Entity> = all.iter().copied().collect();

    for &entity in &all {
        match world
            .get::<prism_scene::components::Parent>(entity)
            .map(|parent| parent.0)
            .filter(|parent| is_ui.contains(parent))
        {
            Some(parent) => children.entry(parent).or_default().push(entity),
            None => roots.push(entity),
        }
    }

    roots.sort_by_key(|entity| sibling_key(world, *entity));
    for list in children.values_mut() {
        list.sort_by_key(|entity| sibling_key(world, *entity));
    }

    // 深度优先前序；`depth` 只是防御 Parent 成环（环里的节点会在自己的子树里
    // 再次出现，但不至于无限递归）。
    let mut order: u32 = 0;
    let mut stack: Vec<(prism_ecs::Entity, u32)> =
        roots.into_iter().rev().map(|entity| (entity, 0)).collect();
    let mut seen: std::collections::HashSet<prism_ecs::Entity> = std::collections::HashSet::new();

    while let Some((entity, depth)) = stack.pop() {
        if !seen.insert(entity) {
            continue;
        }
        if let Some(layout) = world.get_mut::<ComputedLayout>(entity) {
            layout.order = order;
            order = order.saturating_add(1);
        }
        if depth < 64 {
            if let Some(kids) = children.get(&entity) {
                for &kid in kids.iter().rev() {
                    stack.push((kid, depth + 1));
                }
            }
        }
    }
}

/// 兄弟排序键：场景 `id` 优先，运行时实体按分配下标。
fn sibling_key(world: &World, entity: prism_ecs::Entity) -> (u32, u32) {
    let scene_id = world
        .get::<crate::scene::components::SceneEntityId>(entity)
        .map(|id| id.0)
        .unwrap_or(u32::MAX);
    (scene_id, entity.id())
}

/// 方向相关的 `(主轴, 交叉轴)` 尺寸。
fn main_cross(direction: LayoutDirection, size: [f32; 2]) -> (f32, f32) {
    match direction {
        LayoutDirection::Row => (size[0], size[1]),
        LayoutDirection::Column => (size[1], size[0]),
        LayoutDirection::None => (0.0, 0.0),
    }
}

/// measure 遍：每个节点的「内容尺寸」。
///
/// 非布局组节点就是显式宽/高（没写算 0）；布局组在 `fit_content` 时把自己
/// 收缩到「子元素主轴之和 + spacing + padding」与「交叉轴最大值 + padding」。
fn measure_sizes(
    order: &[prism_ecs::Entity],
    children: &std::collections::HashMap<prism_ecs::Entity, Vec<prism_ecs::Entity>>,
    styles: &std::collections::HashMap<prism_ecs::Entity, Style>,
) -> std::collections::HashMap<prism_ecs::Entity, [f32; 2]> {
    /// 有限且非负；坏值按 0。
    fn extent(value: f32) -> f32 {
        if value.is_finite() && value > 0.0 {
            value
        } else {
            0.0
        }
    }

    let mut measured: std::collections::HashMap<prism_ecs::Entity, [f32; 2]> =
        std::collections::HashMap::new();
    for entity in order.iter().rev() {
        let Some(style) = styles.get(entity) else {
            continue;
        };
        let mut size = [
            style.width.map(extent).unwrap_or(0.0),
            style.height.map(extent).unwrap_or(0.0),
        ];
        let layout = style.layout;
        if layout.direction != LayoutDirection::None {
            let padding = layout.padding;
            let spacing = extent(layout.spacing);
            let mut main = 0.0;
            let mut cross: f32 = 0.0;
            let mut count = 0usize;
            if let Some(kids) = children.get(entity) {
                for kid in kids {
                    let Some(kid_size) = measured.get(kid).copied() else {
                        continue;
                    };
                    let (kid_main, kid_cross) = main_cross(layout.direction, kid_size);
                    if count > 0 {
                        main += spacing;
                    }
                    count += 1;
                    main += kid_main;
                    cross = cross.max(kid_cross);
                }
            }
            if layout.fit_content {
                let (content_main, content_cross) = match layout.direction {
                    LayoutDirection::Row => (
                        extent(padding.left) + extent(padding.right),
                        extent(padding.top) + extent(padding.bottom),
                    ),
                    LayoutDirection::Column => (
                        extent(padding.top) + extent(padding.bottom),
                        extent(padding.left) + extent(padding.right),
                    ),
                    LayoutDirection::None => (0.0, 0.0),
                };
                let (content_w, content_h) = match layout.direction {
                    LayoutDirection::Row => (main + content_main, cross + content_cross),
                    LayoutDirection::Column => (cross + content_cross, main + content_main),
                    LayoutDirection::None => (0.0, 0.0),
                };
                if style.width.is_none() {
                    size[0] = content_w.max(0.0);
                }
                if style.height.is_none() {
                    size[1] = content_h.max(0.0);
                }
            }
        }
        measured.insert(*entity, size);
    }
    measured
}

/// 把一个子元素放进父级的布局组：主轴按游标累加，交叉轴按 `child_align`。
fn place_in_layout(
    parent: prism_ecs::Entity,
    layout: Layout,
    parent_rect: [f32; 4],
    own: Option<[f32; 2]>,
    cursors: &mut std::collections::HashMap<prism_ecs::Entity, f32>,
) -> [f32; 4] {
    let padding = layout.padding;
    let content = [
        parent_rect[0] + padding.left,
        parent_rect[1] + padding.top,
        (parent_rect[2] - padding.left - padding.right).max(0.0),
        (parent_rect[3] - padding.top - padding.bottom).max(0.0),
    ];
    let own = own.unwrap_or([0.0, 0.0]);
    let (main, cross_default) = main_cross(layout.direction, own);
    let cross_usable = match layout.direction {
        LayoutDirection::Row => content[3],
        LayoutDirection::Column => content[2],
        LayoutDirection::None => 0.0,
    };
    let cross = if layout.child_align == ChildAlign::Stretch {
        cross_usable
    } else {
        cross_default
    };
    let cross_offset = match layout.child_align {
        ChildAlign::Start | ChildAlign::Stretch => 0.0,
        ChildAlign::Center => ((cross_usable - cross) * 0.5).max(0.0),
        ChildAlign::End => (cross_usable - cross).max(0.0),
    };
    let cursor = cursors.entry(parent).or_insert(0.0);
    let rect = match layout.direction {
        LayoutDirection::Row => [content[0] + *cursor, content[1] + cross_offset, main, cross],
        LayoutDirection::Column => [content[0] + cross_offset, content[1] + *cursor, cross, main],
        LayoutDirection::None => [content[0], content[1], main, cross],
    };
    *cursor += main
        + if layout.spacing.is_finite() && layout.spacing > 0.0 {
            layout.spacing
        } else {
            0.0
        };
    rect
}

/// 锚点路径：`fit_content` 的布局组没有显式宽/高时，用 measure 出的内容尺寸
/// 覆盖，再按 anchor/pivot 重新定位。
fn anchored_rect(style: &Style, parent: [f32; 4], measured: Option<[f32; 2]>) -> [f32; 4] {
    if style.layout.direction != LayoutDirection::None && style.layout.fit_content {
        let measured = measured.unwrap_or([0.0, 0.0]);
        let size = [
            style.width.unwrap_or(measured[0]).max(0.0),
            style.height.unwrap_or(measured[1]).max(0.0),
        ];
        return compute_rect_sized(style, parent, Some(size));
    }
    compute_rect(style, parent)
}

/// 根据 Style 和父容器矩形计算屏幕空间矩形。
///
/// 语义对齐引擎的 `anchorMin` / `anchorMax` + `pivot`：
/// - **拉伸轴**（`min != max`）：位置由锚点与边距决定，`pivot` **不参与**
///   （否则全屏根元素会得到负的原点）；
/// - **点锚点轴**（`min == max`）：尺寸取 `width`/`height`，位置按 `pivot` 对齐。
fn compute_rect(style: &Style, parent: [f32; 4]) -> [f32; 4] {
    compute_rect_sized(style, parent, None)
}

/// [`compute_rect`] 的可选尺寸覆盖（布局组 `fit_content` 用）。
fn compute_rect_sized(style: &Style, parent: [f32; 4], size: Option<[f32; 2]>) -> [f32; 4] {
    let (px, py, pw, ph) = (parent[0], parent[1], parent[2], parent[3]);

    // 锚点 → 矩形四边
    let anchor_left = px + style.anchors.min_x * pw;
    let anchor_right = px + style.anchors.max_x * pw;
    let anchor_top = py + style.anchors.min_y * ph;
    let anchor_bottom = py + style.anchors.max_y * ph;

    let margin = &style.margin;

    let stretched_x = style.anchors.min_x != style.anchors.max_x;
    let stretched_y = style.anchors.min_y != style.anchors.max_y;

    // 固定尺寸 vs 锚点拉伸；布局组的 `fit_content` 用调用方给的内容尺寸。
    let w = size
        .map(|size| size[0])
        .or(style.width)
        .unwrap_or_else(|| (anchor_right - anchor_left) - margin.left - margin.right);
    let h = size
        .map(|size| size[1])
        .or(style.height)
        .unwrap_or_else(|| (anchor_bottom - anchor_top) - margin.top - margin.bottom);
    let w = w.max(0.0);
    let h = h.max(0.0);

    // 拉伸轴：锚点+边距决定原点；点锚点轴：pivot 参与定位。
    // 点锚点必须同时应用对面的 margin（right/bottom），否则贴右下角的元素
    // 会紧贴屏幕边缘（#593）。
    let left = if stretched_x {
        anchor_left + margin.left
    } else {
        anchor_left + margin.left - margin.right - style.pivot.x * w
    };
    let top = if stretched_y {
        anchor_top + margin.top
    } else {
        anchor_top + margin.top - margin.bottom - style.pivot.y * h
    };

    [left, top, w, h]
}

/// 如果锚点 min == max（点锚点），用此 + 固定尺寸定位。
fn _anchor_point_rect(style: &Style, parent: [f32; 4]) -> [f32; 4] {
    let (px, py, pw, ph) = (parent[0], parent[1], parent[2], parent[3]);

    let anchor_x = px + style.anchors.min_x * pw;
    let anchor_y = py + style.anchors.min_y * ph;

    let w = style.width.unwrap_or(0.0);
    let h = style.height.unwrap_or(0.0);

    let left = anchor_x - style.pivot.x * w + style.margin.left - style.margin.right;
    let top = anchor_y - style.pivot.y * h + style.margin.top - style.margin.bottom;

    [left, top, w.max(0.0), h.max(0.0)]
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
