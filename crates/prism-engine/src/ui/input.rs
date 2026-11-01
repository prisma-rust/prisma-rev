//! UI Input System —— 鼠标命中测试 + Interaction 组件更新
//!
//! 每帧从 `UiInputState` resource 读取鼠标状态，
//! 对 `Node + ComputedLayout` 做命中测试，更新 `Interaction` 组件。
//!
//! # 命中规则
//!
//! - `Style::visible == false` 的元素不参与命中（与 `ui_render_system` 收集 quad
//!   时跳过不可见元素保持一致）。
//! - `Active(false)` 的实体不参与命中，父级关闭时整个子树都不参与（与绘制一致）。
//! - 重叠时只有**最上层**的元素拿到 `hovered / clicked / pressed`，点击在命中处
//!   被消费，不会“穿透”到背景面板。
//! - “最上层”按 UI 层级深度判定：子元素画在父元素之上，所以深度大的优先；同深度
//!   时面积更小的更具体，优先命中；深度和面积都相同时取查询顺序中靠后的实体
//!   （`ui_render_system` 也按查询顺序推入绘制列表，后入的在上）。

use crate::ui::components::*;
use crate::ui::is_ui_active;
use prism_ecs::{Entity, World};

/// 鼠标/触摸输入状态 —— 由 Engine 每帧从 `InputManager` 拷贝到 World。
#[derive(Clone, Debug, Default)]
pub struct UiInputState {
    /// 鼠标在窗口中的位置（像素）。
    pub cursor_pos: [f32; 2],
    /// 当前帧是否按下了左键（按下边沿）。
    pub left_clicked: bool,
    /// 左键当前是否按住。
    pub left_held: bool,
    /// 当前帧是否松开了左键（松开边沿）。点击在**松开**时成立。
    pub left_released: bool,
    /// 本帧新按下的按键列表（去重，任意键跳过等用）。
    pub pressed_keys: Vec<crate::input::KeyCode>,
}

/// 左键按下的目标（#528）。
///
/// 持久资源：按下时记下命中的实体，松开时只有「同一实体」才算点击。
/// 这样在按钮外按下、拖到按钮上松开不会误触发，反过来拖出按钮再松开也不算。
#[derive(Clone, Copy, Debug, Default)]
pub struct UiPressState {
    pub entity: Option<Entity>,
}

/// 键盘焦点（#528）：最近一次点击的按钮，`Enter` / `Space` 提交它。
#[derive(Clone, Copy, Debug, Default)]
pub struct UiFocus {
    pub entity: Option<Entity>,
}

/// UI Input System —— 每帧运行，做命中测试更新 Interaction 组件。
///
/// 必须在 `ui::layout` 之后运行（需要 ComputedLayout 已经就绪）。
///
/// 点击语义与 Unity 的 `Clickable` 一致：在元素内按下、**并且在元素内松开**
/// 才置 `clicked`；`pressed` 表示指针仍按在按下目标上。带 `Button` 且
/// `interactable = false` 的元素只更新 `hovered`，不响应按下 / 点击。
/// 有键盘焦点（最近点击的按钮）时，`Enter` / `Space` 也会置 `clicked`。
pub fn ui_input_system(world: &mut World) {
    let state = match world.get_resource::<UiInputState>() {
        Some(s) => s.clone(),
        None => return,
    };

    // 第零步：`Button` 是可作者化的交互元素，场景里不一定显式挂
    // `Interaction`（那是运行期状态）；这里补齐，作者只写 `Button` 也能用。
    let buttons: Vec<Entity> = world.query::<Button>().map(|(entity, _)| entity).collect();
    for entity in buttons {
        if world.get::<Interaction>(entity).is_none() {
            world.insert(entity, Interaction::default());
        }
    }

    // 第一步：清空所有之前帧的状态
    for (_, interaction) in world.query_mut::<Interaction>() {
        interaction.clicked = false;
        interaction.hovered = false;
        interaction.pressed = false;
    }

    let hit = topmost_hit(world, state.cursor_pos);
    let mut press = world.remove_resource::<UiPressState>().unwrap_or_default();
    let mut focus = world.remove_resource::<UiFocus>().unwrap_or_default();

    // 第二步：按下时记录目标；点击 = 同一目标上松开。
    if state.left_clicked {
        press.entity = hit;
        if hit.and_then(|entity| world.get::<Button>(entity)).is_some() {
            focus.entity = hit;
        }
    }

    // 第三步：只把状态写给最上层元素
    if let Some(entity) = hit {
        let interactable = world
            .get::<Button>(entity)
            .map(|button| button.interactable)
            .unwrap_or(true);
        if let Some(interaction) = world.get_mut::<Interaction>(entity) {
            interaction.hovered = true;
            if interactable {
                interaction.pressed =
                    press.entity == Some(entity) && (state.left_held || state.left_clicked);
                interaction.clicked = press.entity == Some(entity) && state.left_released;
            }
        }
    }

    // 第四步：键盘激活。焦点元素不需要在光标下（光标可能已经移开）。
    let activate = state.pressed_keys.iter().any(|key| {
        matches!(
            key,
            crate::input::KeyCode::Enter | crate::input::KeyCode::Space
        )
    });
    if activate {
        if let Some(entity) = focus.entity {
            let interactable = world
                .get::<Button>(entity)
                .map(|button| button.interactable)
                .unwrap_or(true);
            if interactable {
                if let Some(interaction) = world.get_mut::<Interaction>(entity) {
                    interaction.clicked = true;
                }
            }
        }
    }

    // 松开后按下目标失效（无论在元素内外松开）。
    if state.left_released {
        press.entity = None;
    }
    world.insert_resource(press);
    world.insert_resource(focus);
}

/// 光标下最上层的可交互 UI 实体（`None` = 没有命中）。
fn topmost_hit(world: &World, cursor: [f32; 2]) -> Option<Entity> {
    let mut best: Option<(Entity, u32, f32)> = None;

    for (entity, _node, _interaction) in world.query2::<Node, Interaction>() {
        // 被 `Active(false)` 关闭的元素（含父级关闭）不参与命中。
        if !is_ui_active(world, entity) {
            continue;
        }
        // 不可见元素既画不出来也不该可点。
        if !world
            .get::<Style>(entity)
            .map(|style| style.visible)
            .unwrap_or(true)
        {
            continue;
        }

        let Some(layout) = world.get::<ComputedLayout>(entity) else {
            continue;
        };
        if !rect_contains(layout.rect, cursor) {
            continue;
        }

        let depth = ui_depth(world, entity);
        let area = layout.rect[2].max(0.0) * layout.rect[3].max(0.0);
        // 深度大的优先；同深度面积小的优先；完全相同时保留后遍历到的
        // （后入绘制列表 = 画在上面）。
        let better = best.is_none_or(|(_, best_depth, best_area)| {
            depth > best_depth || (depth == best_depth && area <= best_area)
        });
        if better {
            best = Some((entity, depth, area));
        }
    }

    best.map(|(entity, _, _)| entity)
}

/// 屏幕空间矩形 `[left, top, width, height]` 是否包含光标（像素坐标，y 向下）。
fn rect_contains(rect: [f32; 4], cursor: [f32; 2]) -> bool {
    let [left, top, width, height] = rect;
    cursor[0] >= left && cursor[0] <= left + width && cursor[1] >= top && cursor[1] <= top + height
}

/// UI 层级深度（根 = 0）。层级只认场景的 `Parent`。
///
/// 最大 64 层：脏数据成环时不至于卡死一帧。
fn ui_depth(world: &World, entity: Entity) -> u32 {
    const MAX_DEPTH: u32 = 64;
    let mut depth = 0;
    let mut current = entity;
    while depth < MAX_DEPTH {
        let Some(parent) = world
            .get::<prism_scene::components::Parent>(current)
            .map(|parent| parent.0)
        else {
            break;
        };
        depth += 1;
        current = parent;
    }
    depth
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
