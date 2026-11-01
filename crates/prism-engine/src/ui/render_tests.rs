//! `Active` 过滤：隐藏的 UI 既不进绘制列表，也不参与命中测试。
//!
//! `Active` 是层级面板眼睛按钮写入的组件，缺失视为激活；父级关闭要连带隐藏子树。

use super::{convert_ui_draw_list_to_overlay, ui_render_system, UiDrawItem, UiDrawList};
use crate::ui::components::*;
use crate::ui::input::{ui_input_system, UiInputState};
use crate::ui::{is_ui_active, ScreenSize};
use prism_ecs::{Entity, World};
use prism_scene::components::Active;
use prism_scene::HierarchyHelper;

fn world_with_screen() -> World {
    let mut world = World::new();
    world.insert_resource(ScreenSize::new(800, 600));
    world
}

fn spawn_quad(world: &mut World, rect: [f32; 4]) -> Entity {
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(
        entity,
        Style {
            background: [1.0, 0.0, 0.0, 1.0],
            ..Style::default()
        },
    );
    world.insert(entity, ComputedLayout::from_rect(rect));
    entity
}

fn spawn_text(world: &mut World, rect: [f32; 4], content: &str) -> Entity {
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, Style::default());
    world.insert(entity, ComputedLayout::from_rect(rect));
    world.insert(
        entity,
        Text {
            content: content.to_string(),
            ..Text::default()
        },
    );
    entity
}

fn spawn_interactive(world: &mut World, rect: [f32; 4]) -> Entity {
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, Style::default());
    world.insert(entity, ComputedLayout::from_rect(rect));
    world.insert(entity, Interaction::default());
    entity
}

#[test]
fn buttons_draw_their_interaction_colors() {
    // #528：`Button` 覆盖 `Style.background`，并跟随 hover / pressed 换色。
    let mut world = world_with_screen();
    let entity = spawn_quad(&mut world, [10.0, 10.0, 100.0, 40.0]);
    let button = Button::default();
    world.insert(entity, button);

    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    assert_eq!(overlay.quads[0].color, button.normal, "默认用 normal 色");

    world.insert(
        entity,
        Interaction {
            hovered: true,
            ..Default::default()
        },
    );
    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    assert_eq!(overlay.quads[0].color, button.hover);

    world.insert(
        entity,
        Interaction {
            hovered: true,
            pressed: true,
            ..Default::default()
        },
    );
    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    assert_eq!(overlay.quads[0].color, button.pressed, "按下比 hover 优先");
}

#[test]
fn a_style_image_becomes_a_textured_quad_with_the_background_as_tint() {
    // #591：`Style.image` 解析出 bindless 槽位后，这一格用图片 + `background` tint。
    let mut world = world_with_screen();
    let entity = spawn_quad(&mut world, [0.0, 0.0, 64.0, 64.0]);
    world.get_mut::<Style>(entity).unwrap().image = "textures/icon.png".into();
    world.get_mut::<Style>(entity).unwrap().background = [0.5, 1.0, 0.5, 1.0];
    world.insert(
        entity,
        crate::scene::runtime_components::UiImageRef {
            asset_id: prism_scene::SceneAssetId::generate(),
            texture_slot: 7,
            size: [64, 64],
            generation: 1,
        },
    );

    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    let quad = overlay.quads.first().expect("one quad");
    assert_eq!(quad.texture, Some(7));
    assert_eq!(quad.color, [0.5, 1.0, 0.5, 1.0], "background 当 tint");
}

#[test]
fn an_image_with_a_transparent_background_draws_in_its_own_colours() {
    // 作者只写 `image` 不写 `background` 时，alpha 为 0 被当作不透明白色 tint。
    let mut world = world_with_screen();
    let entity = spawn_quad(&mut world, [0.0, 0.0, 32.0, 32.0]);
    let style = world.get_mut::<Style>(entity).unwrap();
    style.image = "textures/icon.png".into();
    style.background = [0.0; 4];
    world.insert(
        entity,
        crate::scene::runtime_components::UiImageRef {
            asset_id: prism_scene::SceneAssetId::generate(),
            texture_slot: 3,
            size: [32, 32],
            generation: 1,
        },
    );

    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    let quad = overlay
        .quads
        .first()
        .expect("image quads are never skipped");
    assert_eq!(quad.texture, Some(3));
    assert_eq!(quad.color, [1.0, 1.0, 1.0, 1.0]);
}

#[test]
fn an_unresolved_image_falls_back_to_the_flat_colour() {
    let mut world = world_with_screen();
    let entity = spawn_quad(&mut world, [0.0, 0.0, 16.0, 16.0]);
    world.get_mut::<Style>(entity).unwrap().image = "textures/missing.png".into();
    world.insert(
        entity,
        crate::scene::runtime_components::UiImageRef {
            asset_id: prism_scene::SceneAssetId::generate(),
            texture_slot: 0,
            size: [0, 0],
            generation: 0,
        },
    );

    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    let quad = overlay.quads.first().expect("背景色仍然要画");
    assert_eq!(quad.texture, None);
    assert_eq!(quad.color, [1.0, 0.0, 0.0, 1.0]);
}

#[test]
fn border_radius_is_normalised_by_the_quad_size() {
    let mut world = world_with_screen();
    let entity = spawn_quad(&mut world, [100.0, 100.0, 200.0, 100.0]);
    world.get_mut::<Style>(entity).unwrap().border_radius = 20.0;

    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    let quad = overlay.quads.first().expect("one quad");
    // 200px wide quad → NDC width 0.5; 20px radius → 20 / 200 * 2 = 0.2.
    assert!((quad.border_radius - 0.2).abs() < 1e-6, "{quad:?}");
}

#[test]
fn text_font_path_reaches_the_overlay_input() {
    // #583：`Text.font` 是资产路径；空字符串表示用系统默认字体。
    let mut world = world_with_screen();
    let entity = spawn_text(&mut world, [0.0, 0.0, 200.0, 24.0], "HUD");

    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    assert_eq!(overlay.texts[0].font, None, "空 = 系统默认字体");

    world.get_mut::<Text>(entity).unwrap().font = "assets/fonts/hud.ttf".into();
    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    assert_eq!(
        overlay.texts[0].font.as_deref(),
        Some("assets/fonts/hud.ttf")
    );
}

#[test]
fn inactive_entities_are_skipped_by_drawing() {
    let mut world = world_with_screen();
    let visible_quad = spawn_quad(&mut world, [0.0, 0.0, 10.0, 10.0]);
    let hidden_quad = spawn_quad(&mut world, [20.0, 0.0, 10.0, 10.0]);
    let visible_text = spawn_text(&mut world, [0.0, 20.0, 10.0, 10.0], "shown");
    let hidden_text = spawn_text(&mut world, [20.0, 20.0, 10.0, 10.0], "hidden");

    world.insert(hidden_quad, Active(false));
    world.insert(hidden_text, Active(false));

    ui_render_system(&mut world);

    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    assert_eq!(draw_list.quads().count(), 1, "只有激活的 quad 应该被收集");
    assert_eq!(
        draw_list.quads().next().expect("quad").rect,
        [0.0, 0.0, 10.0, 10.0]
    );
    assert_eq!(draw_list.texts().count(), 1, "只有激活的文本应该被收集");
    assert_eq!(draw_list.texts().next().expect("text").content, "shown");

    // 隐藏的实体不会进入上屏数据。
    let overlay = convert_ui_draw_list_to_overlay(&world);
    assert_eq!(overlay.quads.len(), 1);
    assert_eq!(overlay.texts.len(), 1);

    // 激活状态只是过滤条件，不是删除：组件仍然在。
    assert!(world.get::<ComputedLayout>(visible_quad).is_some());
    assert!(world.get::<ComputedLayout>(visible_text).is_some());
}

#[test]
fn deactivating_a_parent_hides_the_whole_subtree() {
    let mut world = world_with_screen();

    let parent = spawn_quad(&mut world, [0.0, 0.0, 400.0, 300.0]);
    let child = spawn_quad(&mut world, [10.0, 10.0, 100.0, 50.0]);
    let grandchild = spawn_text(&mut world, [20.0, 20.0, 80.0, 20.0], "nested");
    HierarchyHelper::reparent(&mut world, child, Some(parent));
    HierarchyHelper::reparent(&mut world, grandchild, Some(child));

    // 子节点自己没有 + 只有父节点被关掉。
    assert!(is_ui_active(&world, grandchild));
    world.insert(parent, Active(false));
    assert!(!is_ui_active(&world, parent));
    assert!(!is_ui_active(&world, child), "父级关闭必须连带子级");
    assert!(!is_ui_active(&world, grandchild), "祖先关闭必须连带孙级");

    ui_render_system(&mut world);
    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    assert!(draw_list.quads().count() == 0);
    assert!(draw_list.texts().count() == 0);
}

#[test]
fn missing_active_component_counts_as_active() {
    let mut world = world_with_screen();
    let entity = spawn_quad(&mut world, [0.0, 0.0, 10.0, 10.0]);
    assert!(
        world.get::<Active>(entity).is_none(),
        "场景里只有显式关闭的实体才写 Active"
    );
    assert!(is_ui_active(&world, entity));
}

#[test]
fn inactive_element_does_not_block_hit_testing() {
    let mut world = world_with_screen();

    // 两个完全重叠的可交互元素：后遍历到的（面积相同 → 更靠“上”）本该赢，
    // 但它被 Active(false) 关掉了，光标应该落到下面的那个。
    let below = spawn_interactive(&mut world, [0.0, 0.0, 200.0, 100.0]);
    let above = spawn_interactive(&mut world, [0.0, 0.0, 200.0, 100.0]);
    world.insert(above, Active(false));

    world.insert_resource(UiInputState {
        cursor_pos: [50.0, 50.0],
        left_clicked: true,
        left_held: true,
        ..UiInputState::default()
    });
    ui_input_system(&mut world);
    // #528：点击在松开时成立，补一个松开帧。
    world.insert_resource(UiInputState {
        cursor_pos: [50.0, 50.0],
        left_released: true,
        ..UiInputState::default()
    });
    ui_input_system(&mut world);

    let below_interaction = world.get::<Interaction>(below).expect("interaction");
    assert!(below_interaction.hovered, "被隐藏的遮挡元素不应吞掉点击");
    assert!(below_interaction.clicked);
    let above_interaction = world.get::<Interaction>(above).expect("interaction");
    assert!(!above_interaction.hovered);
    assert!(!above_interaction.clicked);
}

#[test]
fn invisible_text_is_skipped_like_quads() {
    let mut world = world_with_screen();

    let visible_text = spawn_text(&mut world, [0.0, 0.0, 100.0, 20.0], "shown");
    let hidden_text = spawn_text(&mut world, [0.0, 20.0, 100.0, 20.0], "hidden");
    let hidden_quad = spawn_quad(&mut world, [0.0, 40.0, 100.0, 20.0]);

    // 只改 `Style::visible`（不是 `Active`）：背景 quad 与字形都应该一起消失。
    world.insert(
        hidden_text,
        Style {
            visible: false,
            ..Style::default()
        },
    );
    world.insert(
        hidden_quad,
        Style {
            visible: false,
            background: [1.0, 0.0, 0.0, 1.0],
            ..Style::default()
        },
    );

    ui_render_system(&mut world);

    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    assert!(draw_list.quads().count() == 0, "隐藏的 quad 不应进绘制列表");
    assert_eq!(draw_list.texts().count(), 1, "只有可见文本应该被收集");
    assert_eq!(draw_list.texts().next().expect("text").content, "shown");
    assert_eq!(
        draw_list.texts().next().expect("text").rect,
        [0.0, 0.0, 100.0, 20.0]
    );

    // 上屏数据同样只剩可见文本。
    let overlay = convert_ui_draw_list_to_overlay(&world);
    assert!(overlay.quads.is_empty());
    assert_eq!(overlay.texts.len(), 1);
    assert_eq!(overlay.texts[0].content, "shown");

    // `visible` 只是过滤条件，不是删除：组件仍在，重新打开即恢复绘制。
    assert!(world.get::<Text>(hidden_text).is_some());
    assert!(world.get::<ComputedLayout>(visible_text).is_some());

    world.insert(
        hidden_text,
        Style {
            visible: true,
            ..Style::default()
        },
    );
    ui_render_system(&mut world);
    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    assert_eq!(draw_list.texts().count(), 2, "重新可见后应恢复绘制");
}

#[test]
fn canvas_scale_multiplies_font_size_and_border_radius() {
    // #463：字体与圆角是「设计像素」，要按所在 Canvas 的缩放换算。
    let mut world = world_with_screen();
    let scaled = ComputedLayout {
        rect: [0.0, 0.0, 200.0, 100.0],
        order: 0,
        scale: 2.0,
    };

    let text = world.spawn();
    world.insert(text, Node {});
    world.insert(text, Style::default());
    world.insert(text, scaled);
    world.insert(
        text,
        Text {
            content: "HUD".to_string(),
            font_size: 20.0,
            ..Text::default()
        },
    );

    let quad = world.spawn();
    world.insert(quad, Node {});
    world.insert(
        quad,
        Style {
            background: [1.0, 1.0, 1.0, 1.0],
            border_radius: 8.0,
            ..Style::default()
        },
    );
    world.insert(quad, scaled);

    ui_render_system(&mut world);
    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    let text = draw_list.texts().next().expect("one text");
    assert_eq!(text.font_size, 40.0);
    let quad = draw_list.quads().next().expect("one quad");
    assert_eq!(quad.border_radius, 16.0);

    // 坏值（0 / NaN）按 1 处理，不能让顶点算出 NaN。
    let broken = ComputedLayout {
        rect: [0.0, 0.0, 10.0, 10.0],
        order: 0,
        scale: f32::NAN,
    };
    let entity = spawn_text(&mut world, [0.0, 0.0, 10.0, 10.0], "x");
    world.insert(entity, broken);
    ui_render_system(&mut world);
    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    let broken_text = draw_list
        .texts()
        .find(|text| text.content == "x")
        .expect("broken-scale text");
    assert_eq!(
        broken_text.font_size,
        Text::default().font_size,
        "NaN 缩放不能把字号吃掉"
    );
}

#[test]
fn text_without_style_is_still_drawn() {
    let mut world = world_with_screen();
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, ComputedLayout::from_rect([0.0, 0.0, 10.0, 10.0]));
    world.insert(
        entity,
        Text {
            content: "no style".to_string(),
            ..Text::default()
        },
    );

    ui_render_system(&mut world);

    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    assert_eq!(
        draw_list.texts().count(),
        1,
        "缺少 Style 的文本按可见处理（与 `visible` 的默认值一致）"
    );
}

#[test]
fn overlay_ndc_keeps_screen_orientation() {
    // Vulkan NDC：`y = -1` 是屏幕顶。像素左上角必须落在 (-1, -1)，
    // 否则整个 UI 层会被上下镜像（intro 的标题倒着显示就是这个 bug）。
    let mut world = world_with_screen(); // 800x600
    spawn_quad(&mut world, [0.0, 0.0, 400.0, 300.0]);
    ui_render_system(&mut world);

    let overlay = convert_ui_draw_list_to_overlay(&world);
    assert_eq!(overlay.quads.len(), 1);
    let [x0, y0, x1, y1] = overlay.quads[0].rect;
    assert_eq!([x0, y0], [-1.0, -1.0], "左上角像素 → NDC 左上角");
    assert_eq!([x1, y1], [0.0, 0.0], "屏幕中心 → NDC 原点");

    // 竖直方向单调：更靠下的像素产生更大的 NDC y。
    let mut world = world_with_screen();
    spawn_quad(&mut world, [0.0, 100.0, 10.0, 10.0]);
    ui_render_system(&mut world);
    let overlay = convert_ui_draw_list_to_overlay(&world);
    let [_, top, _, bottom] = overlay.quads[0].rect;
    assert!(top < bottom, "NDC 上边必须小于下边");
    assert!(top > -1.0, "y=100 应该已经离开屏幕顶");
}

// ── 层级绘制顺序（#462）────────────────────────────────────────

fn spawn_node(world: &mut World, bg: Option<[f32; 4]>, text: Option<&str>) -> Entity {
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(
        entity,
        Style {
            background: bg.unwrap_or([0.0; 4]),
            ..Style::default()
        },
    );
    if let Some(content) = text {
        world.insert(
            entity,
            Text {
                content: content.to_string(),
                ..Text::default()
            },
        );
    }
    entity
}

fn draw_texts(draw_list: &UiDrawList) -> Vec<String> {
    draw_list.texts().map(|t| t.content.clone()).collect()
}

#[test]
fn a_later_sibling_covers_the_text_of_an_earlier_one() {
    let mut world = world_with_screen();

    let title = spawn_node(&mut world, None, Some("Intro"));
    let blackout = spawn_node(&mut world, Some([0.0, 0.0, 0.0, 1.0]), None);

    crate::ui::layout::ui_layout_system(&mut world);
    ui_render_system(&mut world);

    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    // 文本必须在黑场之前画，否则黑场盖不住它。
    assert_eq!(draw_list.items.len(), 2);
    assert!(
        matches!(&draw_list.items[0], UiDrawItem::Text(t) if t.content == "Intro"),
        "标题文本应该先画"
    );
    assert!(
        matches!(&draw_list.items[1], UiDrawItem::Quad(q) if q.color[3] == 1.0),
        "后加的全屏遮罩应该盖住它"
    );
    assert!(
        world.get::<ComputedLayout>(title).unwrap().order
            < world.get::<ComputedLayout>(blackout).unwrap().order
    );
}

#[test]
fn children_draw_above_their_parent_even_when_created_first() {
    let mut world = world_with_screen();

    let child = spawn_node(&mut world, Some([0.0, 1.0, 0.0, 1.0]), None);
    let parent = spawn_node(&mut world, Some([1.0, 0.0, 0.0, 1.0]), None);
    HierarchyHelper::reparent(&mut world, child, Some(parent));

    crate::ui::layout::ui_layout_system(&mut world);
    ui_render_system(&mut world);

    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    assert_eq!(draw_list.items.len(), 2);
    // 父先画、子后画：子面板永远在父背景之上。
    assert!(matches!(&draw_list.items[0], UiDrawItem::Quad(q) if q.color[1] == 0.0));
    assert!(matches!(&draw_list.items[1], UiDrawItem::Quad(q) if q.color[1] == 1.0));
}

#[test]
fn siblings_follow_the_document_id_not_the_spawn_order() {
    let mut world = world_with_screen();

    // 先 spawn 的那个拿到更大的 id：绘制顺序要跟文档顺序走。
    let later_in_document = spawn_node(&mut world, Some([0.0, 0.0, 1.0, 1.0]), None);
    let earlier_in_document = spawn_node(&mut world, Some([1.0, 0.0, 0.0, 1.0]), None);
    world.insert(
        later_in_document,
        crate::scene::components::SceneEntityId(7),
    );
    world.insert(
        earlier_in_document,
        crate::scene::components::SceneEntityId(2),
    );

    crate::ui::layout::ui_layout_system(&mut world);
    ui_render_system(&mut world);

    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    // id 2（红）在文档里更靠前，尽管它后 spawn。
    assert!(matches!(&draw_list.items[0], UiDrawItem::Quad(q) if q.color[0] == 1.0));
    assert!(matches!(&draw_list.items[1], UiDrawItem::Quad(q) if q.color[2] == 1.0));
}

#[test]
fn an_external_layer_still_draws_above_the_whole_ui() {
    let mut world = world_with_screen();

    spawn_node(&mut world, Some([1.0, 0.0, 0.0, 1.0]), Some("Intro"));

    world.insert_resource(crate::ui::ExternalUiQuads(vec![crate::ui::UiQuad {
        rect: [0.0, 0.0, 8.0, 6.0],
        color: [0.5, 0.5, 0.5, 1.0],
        border_radius: 0.0,
        layer: 1,
        ..crate::ui::UiQuad::default()
    }]));

    crate::ui::layout::ui_layout_system(&mut world);
    ui_render_system(&mut world);

    let draw_list = world.get_resource::<UiDrawList>().expect("draw list");
    assert_eq!(draw_list.items.len(), 3);
    assert!(
        matches!(&draw_list.items[2], UiDrawItem::Quad(q) if q.layer == 1),
        "layer 1 的外部 quad 必须画在所有 layer 0 之上"
    );
    assert_eq!(draw_texts(draw_list), vec!["Intro".to_string()]);
}
