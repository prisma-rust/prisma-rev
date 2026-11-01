//! `ui_input_system` 的命中测试契约：可见性、层级遮挡、点击消费、按下/松开语义。

use super::{ui_input_system, UiFocus, UiInputState};
use crate::ui::components::*;
use prism_ecs::{Entity, World};
use prism_scene::components::Parent;
use prism_scene::HierarchyHelper;

/// 造一个可交互 UI 元素（`Node + ComputedLayout + Style + Interaction`）。
fn spawn_ui(world: &mut World, rect: [f32; 4], visible: bool) -> Entity {
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, ComputedLayout::from_rect(rect));
    world.insert(
        entity,
        Style {
            visible,
            ..Default::default()
        },
    );
    world.insert(entity, Interaction::default());
    entity
}

fn input(world: &mut World, cursor: [f32; 2], clicked: bool, held: bool) {
    world.insert_resource(UiInputState {
        cursor_pos: cursor,
        left_clicked: clicked,
        left_held: held,
        left_released: false,
        pressed_keys: Vec::new(),
    });
}

/// 在 `cursor` 处按下左键。
fn press(world: &mut World, cursor: [f32; 2]) {
    input(world, cursor, true, true);
    ui_input_system(world);
}

/// 保持按住（没有按下/松开边沿）。
fn hold(world: &mut World, cursor: [f32; 2]) {
    input(world, cursor, false, true);
    ui_input_system(world);
}

/// 松开左键。
fn release(world: &mut World, cursor: [f32; 2]) {
    world.insert_resource(UiInputState {
        cursor_pos: cursor,
        left_clicked: false,
        left_held: false,
        left_released: true,
        pressed_keys: Vec::new(),
    });
    ui_input_system(world);
}

/// 按下并在同一位置松开（一次完整点击）。
fn click(world: &mut World, cursor: [f32; 2]) {
    press(world, cursor);
    release(world, cursor);
}

fn idle(world: &mut World, cursor: [f32; 2]) {
    input(world, cursor, false, false);
    ui_input_system(world);
}

fn state_of(world: &World, entity: Entity) -> Interaction {
    *world
        .get::<Interaction>(entity)
        .expect("entity has Interaction")
}

#[test]
fn click_is_consumed_by_the_top_most_element() {
    let mut world = World::new();
    let panel = spawn_ui(&mut world, [0.0, 0.0, 800.0, 600.0], true);
    let button = spawn_ui(&mut world, [350.0, 275.0, 100.0, 50.0], true);
    HierarchyHelper::reparent(&mut world, button, Some(panel));

    press(&mut world, [400.0, 300.0]);
    let pressed = state_of(&world, button);
    assert!(pressed.hovered, "光标在按钮内");
    assert!(pressed.pressed, "按下时 pressed 置位");
    assert!(!pressed.clicked, "点击在松开时才成立");

    release(&mut world, [400.0, 300.0]);
    let button_state = state_of(&world, button);
    assert!(button_state.hovered);
    assert!(button_state.clicked, "同一元素内按下并松开 = 点击");
    assert!(!button_state.pressed, "松开后不再 pressed");

    let panel_state = state_of(&world, panel);
    assert!(!panel_state.hovered, "背景面板不应同时被 hover");
    assert!(!panel_state.clicked, "点击应被最上层消费，不冒泡到面板");
    assert!(!panel_state.pressed);
}

#[test]
fn releasing_outside_the_button_is_not_a_click() {
    let mut world = World::new();
    let button = spawn_ui(&mut world, [100.0, 100.0, 100.0, 40.0], true);

    press(&mut world, [120.0, 110.0]);
    assert!(state_of(&world, button).pressed);
    release(&mut world, [500.0, 500.0]);

    let state = state_of(&world, button);
    assert!(!state.clicked, "拖出按钮再松开不算点击");
    assert!(!state.hovered);
}

#[test]
fn pressing_outside_then_releasing_inside_is_not_a_click() {
    let mut world = World::new();
    let button = spawn_ui(&mut world, [100.0, 100.0, 100.0, 40.0], true);

    press(&mut world, [500.0, 500.0]);
    release(&mut world, [120.0, 110.0]);

    let state = state_of(&world, button);
    assert!(state.hovered, "松开时光标确实在按钮上");
    assert!(!state.clicked, "按下目标不是它，不触发点击");
}

#[test]
fn disabled_button_hovers_but_never_clicks() {
    let mut world = World::new();
    let button = spawn_ui(&mut world, [100.0, 100.0, 100.0, 40.0], true);
    world.insert(
        button,
        Button {
            interactable: false,
            ..Default::default()
        },
    );

    press(&mut world, [120.0, 110.0]);
    release(&mut world, [120.0, 110.0]);

    let state = state_of(&world, button);
    assert!(state.hovered, "不可交互仍显示 hover 外观");
    assert!(!state.pressed);
    assert!(!state.clicked);
}

#[test]
fn enter_activates_the_focused_button() {
    let mut world = World::new();
    let button = spawn_ui(&mut world, [100.0, 100.0, 100.0, 40.0], true);
    world.insert(button, Button::default());

    click(&mut world, [120.0, 110.0]);
    assert_eq!(
        world.get_resource::<UiFocus>().and_then(|f| f.entity),
        Some(button),
        "点击按钮后它拿到键盘焦点"
    );

    idle(&mut world, [500.0, 500.0]);
    world.insert_resource(UiInputState {
        cursor_pos: [500.0, 500.0],
        left_clicked: false,
        left_held: false,
        left_released: false,
        pressed_keys: vec![crate::input::KeyCode::Enter],
    });
    ui_input_system(&mut world);
    assert!(
        state_of(&world, button).clicked,
        "焦点按钮上按 Enter 等同点击"
    );

    // 空格同样提交。
    idle(&mut world, [500.0, 500.0]);
    world.insert_resource(UiInputState {
        cursor_pos: [500.0, 500.0],
        left_clicked: false,
        left_held: false,
        left_released: false,
        pressed_keys: vec![crate::input::KeyCode::Space],
    });
    ui_input_system(&mut world);
    assert!(state_of(&world, button).clicked);
}

#[test]
fn a_button_without_an_interaction_component_still_works() {
    // 作者只写 `Button`（场景里不挂运行期状态）：输入系统会补 `Interaction`。
    let mut world = World::new();
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, ComputedLayout::from_rect([0.0, 0.0, 100.0, 40.0]));
    world.insert(entity, Style::default());
    world.insert(entity, Button::default());
    assert!(world.get::<Interaction>(entity).is_none());

    click(&mut world, [10.0, 10.0]);
    assert!(world.get::<Interaction>(entity).is_some());
    assert!(state_of(&world, entity).clicked);
}

#[test]
fn hidden_element_is_not_hittable() {
    let mut world = World::new();
    let hidden = spawn_ui(&mut world, [0.0, 0.0, 800.0, 600.0], false);

    click(&mut world, [400.0, 300.0]);

    let state = state_of(&world, hidden);
    assert!(!state.hovered, "`visible: false` 的元素不可 hover");
    assert!(!state.clicked, "`visible: false` 的元素不可点击");
    assert!(!state.pressed);
}

#[test]
fn at_equal_depth_the_smaller_element_wins() {
    let mut world = World::new();
    // 两个同深度兄弟重叠：面积小的更“具体”，视为在上层。
    let big = spawn_ui(&mut world, [0.0, 0.0, 400.0, 400.0], true);
    let small = spawn_ui(&mut world, [100.0, 100.0, 50.0, 50.0], true);

    click(&mut world, [120.0, 120.0]);

    assert!(state_of(&world, small).clicked);
    assert!(!state_of(&world, big).hovered);
}

#[test]
fn state_is_cleared_when_the_cursor_leaves() {
    let mut world = World::new();
    let button = spawn_ui(&mut world, [10.0, 10.0, 100.0, 40.0], true);

    click(&mut world, [20.0, 20.0]);
    assert!(state_of(&world, button).clicked);

    // 下一帧光标移出：clicked/hovered/pressed 必须全部清掉。
    idle(&mut world, [500.0, 500.0]);
    let state = state_of(&world, button);
    assert!(!state.hovered);
    assert!(!state.clicked);
    assert!(!state.pressed);
}

#[test]
fn cyclic_parent_chain_does_not_hang() {
    let mut world = World::new();
    let a = spawn_ui(&mut world, [0.0, 0.0, 100.0, 100.0], true);
    let b = spawn_ui(&mut world, [0.0, 0.0, 100.0, 100.0], true);
    // 手工造脏数据（`HierarchyHelper` 会拒绝成环）：深度计算必须自己兜底。
    world.insert(a, Parent(b));
    world.insert(b, Parent(a));

    idle(&mut world, [10.0, 10.0]);

    let hovered = u8::from(state_of(&world, a).hovered) + u8::from(state_of(&world, b).hovered);
    assert_eq!(hovered, 1, "成环时仍应只命中一个元素");
}

#[test]
fn holding_keeps_pressed_until_release() {
    let mut world = World::new();
    let button = spawn_ui(&mut world, [10.0, 10.0, 100.0, 40.0], true);

    press(&mut world, [20.0, 20.0]);
    assert!(state_of(&world, button).pressed);
    hold(&mut world, [20.0, 20.0]);
    let state = state_of(&world, button);
    assert!(state.pressed, "按住期间 pressed 持续为真");
    assert!(!state.clicked, "clicked 只在一帧有效");
}
