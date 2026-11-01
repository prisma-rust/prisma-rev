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

    // 本帧已经算出的矩形。
    //
    // `ComputedLayout` 是每帧的输出，读 `world` 里的值只能证明「组件存在」，
    // 不能证明「它是本帧算的」：预插入保证组件一定存在，于是子节点排在父节点
    // 前面时会拿上一帧（或刚插入的默认值）当父矩形，算完也不会再重算。
    // 这里用帧内的 `resolved` 记录真正就绪的父节点，恢复「父先子后」的顺序保证。
    let mut resolved: std::collections::HashMap<prism_ecs::Entity, [f32; 4]> =
        std::collections::HashMap::new();
    let layout_entities: std::collections::HashSet<prism_ecs::Entity> =
        layout_queue.iter().map(|(entity, _)| *entity).collect();

    // 按 Parent 层级处理（先根后子）
    let mut remaining: Vec<_> = layout_queue.into_iter().collect();
    let mut pass = 0;
    while !remaining.is_empty() && pass < 16 {
        pass += 1;
        let mut next_pass = Vec::new();

        for (entity, parent_opt) in remaining.drain(..) {
            let parent_rect = match parent_opt {
                Some(parent_entity) => match resolved.get(&parent_entity) {
                    Some(rect) => *rect,
                    None if layout_entities.contains(&parent_entity) => {
                        // 父节点本帧还没算：等它先算完。
                        next_pass.push((entity, parent_opt));
                        continue;
                    }
                    // 父节点不是 UI 实体（没有 `Node + Style`）：沿用它的现有输出。
                    None => match world.get::<ComputedLayout>(parent_entity) {
                        Some(layout) => layout.rect,
                        None => {
                            next_pass.push((entity, parent_opt));
                            continue;
                        }
                    },
                },
                None => [0.0, 0.0, screen.width, screen.height],
            };

            let style = match world.get::<Style>(entity) {
                Some(s) => s.clone(),
                None => continue,
            };

            let rect = compute_rect(&style, parent_rect);

            if let Some(layout) = world.get_mut::<ComputedLayout>(entity) {
                layout.rect = rect;
            } else {
                world.insert(entity, ComputedLayout::from_rect(rect));
            }
            resolved.insert(entity, rect);
        }

        remaining = next_pass;
    }

    assign_draw_order(world);
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

    let key = |world: &World, entity: prism_ecs::Entity| -> (u32, u32) {
        let scene_id = world
            .get::<crate::scene::components::SceneEntityId>(entity)
            .map(|id| id.0)
            .unwrap_or(u32::MAX);
        (scene_id, entity.id())
    };

    roots.sort_by_key(|entity| key(world, *entity));
    for list in children.values_mut() {
        list.sort_by_key(|entity| key(world, *entity));
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

/// 根据 Style 和父容器矩形计算屏幕空间矩形。
///
/// 语义对齐引擎的 `anchorMin` / `anchorMax` + `pivot`：
/// - **拉伸轴**（`min != max`）：位置由锚点与边距决定，`pivot` **不参与**
///   （否则全屏根元素会得到负的原点）；
/// - **点锚点轴**（`min == max`）：尺寸取 `width`/`height`，位置按 `pivot` 对齐。
fn compute_rect(style: &Style, parent: [f32; 4]) -> [f32; 4] {
    let (px, py, pw, ph) = (parent[0], parent[1], parent[2], parent[3]);

    // 锚点 → 矩形四边
    let anchor_left = px + style.anchors.min_x * pw;
    let anchor_right = px + style.anchors.max_x * pw;
    let anchor_top = py + style.anchors.min_y * ph;
    let anchor_bottom = py + style.anchors.max_y * ph;

    let margin = &style.margin;

    let stretched_x = style.anchors.min_x != style.anchors.max_x;
    let stretched_y = style.anchors.min_y != style.anchors.max_y;

    // 固定尺寸 vs 锚点拉伸
    let w = style
        .width
        .unwrap_or_else(|| (anchor_right - anchor_left) - margin.left - margin.right);
    let h = style
        .height
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
