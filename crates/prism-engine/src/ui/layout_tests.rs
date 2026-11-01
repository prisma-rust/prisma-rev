//! `ComputedLayout` 是布局系统的输出：作者只写 `Node + Style`。
//!
//! 层级只认场景的 `Parent`（`prism_scene::HierarchyHelper` 维护），没有第二份
//! `UiParent` 状态。

use super::{ui_layout_system, ScreenSize};
use crate::ui::components::*;
use prism_ecs::{Entity, World};
use prism_scene::HierarchyHelper;

fn world_with_screen() -> World {
    let mut world = World::new();
    world.insert_resource(ScreenSize::new(800, 600));
    world
}

#[test]
fn node_without_computed_layout_is_still_laid_out() {
    let mut world = world_with_screen();
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, Style::fixed(200.0, 100.0));
    assert!(
        world.get::<ComputedLayout>(entity).is_none(),
        "authoring must not require the output component"
    );

    ui_layout_system(&mut world);

    let layout = world
        .get::<ComputedLayout>(entity)
        .expect("layout system must insert its output component");
    // 居中锚点 + 200x100，父矩形 = 屏幕 800x600。
    assert_eq!(layout.rect, [300.0, 250.0, 200.0, 100.0]);
}

#[test]
fn stretch_root_fills_the_screen_without_pivot_offset() {
    let mut world = world_with_screen();
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, Style::default()); // STRETCH 锚点 + CENTER pivot

    ui_layout_system(&mut world);

    // pivot 只对点锚点生效；拉伸时原点不能被 pivot 拉到负值。
    assert_eq!(
        world.get::<ComputedLayout>(entity).unwrap().rect,
        [0.0, 0.0, 800.0, 600.0]
    );
}

#[test]
fn a_canvas_scales_its_subtree_to_the_reference_resolution() {
    // #463：1920×1080 设计稿在 960×540 的屏幕上整体缩到 0.5。
    let mut world = World::new();
    world.insert_resource(ScreenSize::new(960, 540));

    let canvas = world.spawn();
    world.insert(canvas, Node {});
    world.insert(canvas, Style::default()); // STRETCH 根节点
    world.insert(canvas, Canvas::default());

    let child = world.spawn();
    world.insert(child, Node {});
    world.insert(child, Style::fixed(400.0, 300.0));
    HierarchyHelper::reparent(&mut world, child, Some(canvas));

    ui_layout_system(&mut world);

    let root_layout = *world.get::<ComputedLayout>(canvas).unwrap();
    assert_eq!(root_layout.rect, [0.0, 0.0, 960.0, 540.0]);
    assert!((root_layout.scale - 0.5).abs() < 1e-5, "{root_layout:?}");

    // 设计稿里居中的 400×300 → 虚拟 (760, 390) → 屏幕 (380, 195)。
    let child_layout = *world.get::<ComputedLayout>(child).unwrap();
    assert_eq!(child_layout.rect, [380.0, 195.0, 200.0, 150.0]);
    assert!((child_layout.scale - 0.5).abs() < 1e-5);
}

#[test]
fn a_canvas_with_constant_pixels_keeps_the_authored_size() {
    let mut world = World::new();
    world.insert_resource(ScreenSize::new(960, 540));

    let canvas = world.spawn();
    world.insert(canvas, Node {});
    world.insert(canvas, Style::default());
    world.insert(
        canvas,
        Canvas {
            scale_mode: CanvasScaleMode::ConstantPixelSize,
            ..Canvas::default()
        },
    );

    let child = world.spawn();
    world.insert(child, Node {});
    world.insert(child, Style::fixed(400.0, 300.0));
    HierarchyHelper::reparent(&mut world, child, Some(canvas));

    ui_layout_system(&mut world);

    // 1 屏幕像素 = 1 设计像素：行为与引入 Canvas 之前完全一致。
    assert_eq!(
        world.get::<ComputedLayout>(child).unwrap().rect,
        [280.0, 120.0, 400.0, 300.0]
    );
    assert_eq!(world.get::<ComputedLayout>(child).unwrap().scale, 1.0);
}

#[test]
fn nested_nodes_inherit_the_nearest_canvas_scale() {
    let mut world = World::new();
    world.insert_resource(ScreenSize::new(1920, 1080));

    let canvas = world.spawn();
    world.insert(canvas, Node {});
    world.insert(canvas, Style::default());
    world.insert(
        canvas,
        Canvas {
            reference_resolution: [960.0, 540.0],
            ..Canvas::default()
        },
    );

    let group = world.spawn();
    world.insert(group, Node {});
    world.insert(group, Style::fixed(960.0, 540.0));
    HierarchyHelper::reparent(&mut world, group, Some(canvas));

    let leaf = world.spawn();
    world.insert(leaf, Node {});
    world.insert(leaf, Style::fixed(100.0, 50.0));
    HierarchyHelper::reparent(&mut world, leaf, Some(group));

    ui_layout_system(&mut world);

    // 960×540 设计稿放大到 1920×1080 = 2×。
    let group_layout = *world.get::<ComputedLayout>(group).unwrap();
    assert_eq!(group_layout.rect, [0.0, 0.0, 1920.0, 1080.0]);
    assert_eq!(group_layout.scale, 2.0);

    // 叶子在设计稿里居中于 960×540：(430, 245) → 屏幕 (860, 490)。
    let leaf_layout = *world.get::<ComputedLayout>(leaf).unwrap();
    assert_eq!(leaf_layout.rect, [860.0, 490.0, 200.0, 100.0]);
    assert_eq!(leaf_layout.scale, 2.0);
}

#[test]
fn stale_computed_layout_is_overwritten() {
    let mut world = world_with_screen();
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, Style::fixed(200.0, 100.0));
    world.insert(entity, ComputedLayout::from_rect([9.0; 4]));

    ui_layout_system(&mut world);

    let layout = world.get::<ComputedLayout>(entity).unwrap();
    assert_eq!(layout.rect, [300.0, 250.0, 200.0, 100.0]);
}

#[test]
fn child_is_positioned_relative_to_its_parent() {
    let mut world = world_with_screen();

    let parent = world.spawn();
    world.insert(parent, Node {});
    world.insert(parent, Style::fixed(400.0, 300.0));

    let child = world.spawn();
    world.insert(child, Node {});
    world.insert(child, Style::fixed(100.0, 50.0));
    HierarchyHelper::reparent(&mut world, child, Some(parent));

    ui_layout_system(&mut world);

    // 父 = [200, 150, 400, 300]；子居中于父 → 中心 (400, 300)，尺寸 100x50。
    assert_eq!(
        world.get::<ComputedLayout>(parent).unwrap().rect,
        [200.0, 150.0, 400.0, 300.0]
    );
    assert_eq!(
        world.get::<ComputedLayout>(child).unwrap().rect,
        [350.0, 275.0, 100.0, 50.0]
    );
}

#[test]
fn child_created_before_parent_uses_the_fresh_parent_rect() {
    let mut world = world_with_screen();

    // 故意先建子、后建父：查询顺序里子节点排在父节点前面。
    // 预插入 `ComputedLayout::default()` 之后，父节点的组件总「存在」，
    // 旧实现据此认为父节点已就绪，用默认矩形给子节点定了一次位置。
    let child = world.spawn();
    world.insert(child, Node {});
    world.insert(child, Style::default()); // 拉伸填满父容器

    let parent = world.spawn();
    world.insert(parent, Node {});
    world.insert(parent, Style::fixed(400.0, 300.0));

    HierarchyHelper::reparent(&mut world, child, Some(parent));

    ui_layout_system(&mut world);

    // 父 = [200, 150, 400, 300]（屏幕居中 400x300），子必须用它本帧的矩形。
    assert_eq!(
        world.get::<ComputedLayout>(parent).unwrap().rect,
        [200.0, 150.0, 400.0, 300.0]
    );
    assert_eq!(
        world.get::<ComputedLayout>(child).unwrap().rect,
        [200.0, 150.0, 400.0, 300.0]
    );
}

#[test]
fn deeper_child_waits_for_the_whole_ancestor_chain() {
    let mut world = world_with_screen();

    // 顺序同样是「孙 → 子 → 父」，且父节点位置在屏幕中央（非零原点），
    // 这样任何一层用到默认矩形都会被断言抓到。
    let grandchild = world.spawn();
    world.insert(grandchild, Node {});
    world.insert(grandchild, Style::default());

    let child = world.spawn();
    world.insert(child, Node {});
    world.insert(child, Style::fixed(300.0, 200.0));

    let parent = world.spawn();
    world.insert(parent, Node {});
    world.insert(parent, Style::fixed(400.0, 300.0));

    HierarchyHelper::reparent(&mut world, child, Some(parent));
    HierarchyHelper::reparent(&mut world, grandchild, Some(child));

    ui_layout_system(&mut world);

    assert_eq!(
        world.get::<ComputedLayout>(parent).unwrap().rect,
        [200.0, 150.0, 400.0, 300.0]
    );
    assert_eq!(
        world.get::<ComputedLayout>(child).unwrap().rect,
        [250.0, 200.0, 300.0, 200.0]
    );
    assert_eq!(
        world.get::<ComputedLayout>(grandchild).unwrap().rect,
        [250.0, 200.0, 300.0, 200.0]
    );
}

#[test]
fn point_anchor_applies_opposite_margins() {
    let mut world = world_with_screen();
    let entity = world.spawn();
    world.insert(entity, Node {});
    let mut style = Style::fixed(100.0, 40.0);
    style.anchors = Anchors {
        min_x: 1.0,
        min_y: 1.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    style.pivot = Pivot { x: 1.0, y: 1.0 };
    style.margin = Margin {
        left: 0.0,
        right: 24.0,
        top: 0.0,
        bottom: 20.0,
    };
    world.insert(entity, style);

    ui_layout_system(&mut world);

    // 右下角 (800,600) - margin(24,20) - 尺寸(100,40)。
    assert_eq!(
        world.get::<ComputedLayout>(entity).unwrap().rect,
        [676.0, 540.0, 100.0, 40.0]
    );
}

#[test]
fn stretched_child_fills_its_parent() {
    let mut world = world_with_screen();

    let parent = world.spawn();
    world.insert(parent, Node {});
    world.insert(parent, Style::fixed(400.0, 300.0));

    let child = world.spawn();
    world.insert(child, Node {});
    world.insert(child, Style::default()); // 拉伸填满父容器
    HierarchyHelper::reparent(&mut world, child, Some(parent));

    ui_layout_system(&mut world);

    // 父 = [200, 150, 400, 300]（屏幕居中 400x300）
    assert_eq!(
        world.get::<ComputedLayout>(child).unwrap().rect,
        [200.0, 150.0, 400.0, 300.0]
    );
}

/// 布局组用的 Style：固定尺寸 + 指定 layout。
fn layout_style(
    width: f32,
    height: f32,
    direction: LayoutDirection,
    spacing: f32,
    padding: Margin,
    child_align: ChildAlign,
    fit_content: bool,
) -> Style {
    let mut style = Style::fixed(width, height);
    style.layout = Layout {
        direction,
        spacing,
        padding,
        child_align,
        fit_content,
    };
    style
}

fn spawn_child(world: &mut World, parent: Entity, style: Style) -> Entity {
    let entity = world.spawn();
    world.insert(entity, Node {});
    world.insert(entity, style);
    HierarchyHelper::reparent(world, entity, Some(parent));
    entity
}

fn rect_of(world: &World, entity: Entity) -> [f32; 4] {
    world.get::<ComputedLayout>(entity).unwrap().rect
}

#[test]
fn a_row_layout_group_stacks_children_with_spacing_and_padding() {
    let mut world = world_with_screen();
    let root = world.spawn();
    world.insert(root, Node {});
    world.insert(
        root,
        layout_style(
            400.0,
            200.0,
            LayoutDirection::Row,
            10.0,
            Margin {
                left: 5.0,
                right: 5.0,
                top: 7.0,
                bottom: 7.0,
            },
            ChildAlign::Start,
            false,
        ),
    );
    let first = spawn_child(&mut world, root, Style::fixed(60.0, 20.0));
    let second = spawn_child(&mut world, root, Style::fixed(40.0, 30.0));

    ui_layout_system(&mut world);

    // 居中 + 400x200 → 根 [200,200,...]；内容区从 +padding 开始。
    assert_eq!(rect_of(&world, root), [200.0, 200.0, 400.0, 200.0]);
    assert_eq!(rect_of(&world, first), [205.0, 207.0, 60.0, 20.0]);
    assert_eq!(rect_of(&world, second), [275.0, 207.0, 40.0, 30.0]);
}

#[test]
fn a_column_layout_group_places_children_top_to_bottom() {
    let mut world = world_with_screen();
    let root = world.spawn();
    world.insert(root, Node {});
    world.insert(
        root,
        layout_style(
            200.0,
            300.0,
            LayoutDirection::Column,
            12.0,
            Margin::default(),
            ChildAlign::Start,
            false,
        ),
    );
    let first = spawn_child(&mut world, root, Style::fixed(50.0, 40.0));
    let second = spawn_child(&mut world, root, Style::fixed(70.0, 25.0));

    ui_layout_system(&mut world);

    // 200x300 居中 → [300,150,...]。
    assert_eq!(rect_of(&world, root), [300.0, 150.0, 200.0, 300.0]);
    assert_eq!(rect_of(&world, first), [300.0, 150.0, 50.0, 40.0]);
    assert_eq!(rect_of(&world, second), [300.0, 202.0, 70.0, 25.0]);
}

#[test]
fn child_align_places_children_on_the_cross_axis() {
    let mut world = world_with_screen();
    let root = world.spawn();
    world.insert(root, Node {});
    world.insert(
        root,
        layout_style(
            200.0,
            300.0,
            LayoutDirection::Row,
            0.0,
            Margin::default(),
            ChildAlign::Center,
            false,
        ),
    );
    let child = spawn_child(&mut world, root, Style::fixed(40.0, 20.0));
    let stretched = spawn_child(&mut world, root, Style::fixed(40.0, 20.0));

    ui_layout_system(&mut world);

    let root_rect = rect_of(&world, root);
    assert_eq!(rect_of(&world, child), [300.0, 290.0, 40.0, 20.0]);
    // Stretch 需要另一组布局：先确认 Center 的交叉轴偏移是 (300-20)/2。
    assert_eq!(root_rect, [300.0, 150.0, 200.0, 300.0]);
    assert_eq!(rect_of(&world, stretched)[0], 340.0);
}

#[test]
fn child_align_stretch_fills_the_cross_axis() {
    let mut world = world_with_screen();
    let root = world.spawn();
    world.insert(root, Node {});
    world.insert(
        root,
        layout_style(
            200.0,
            300.0,
            LayoutDirection::Row,
            0.0,
            Margin::default(),
            ChildAlign::Stretch,
            false,
        ),
    );
    let child = spawn_child(&mut world, root, Style::fixed(40.0, 20.0));

    ui_layout_system(&mut world);

    // 高度被拉满内容区（300），宽度仍取自己的 40。
    assert_eq!(rect_of(&world, child), [300.0, 150.0, 40.0, 300.0]);
}

#[test]
fn fit_content_shrinks_a_layout_group_to_its_children() {
    let mut world = world_with_screen();
    let root = world.spawn();
    world.insert(root, Node {});
    let mut style = layout_style(
        0.0,
        0.0,
        LayoutDirection::Row,
        10.0,
        Margin {
            left: 4.0,
            right: 4.0,
            top: 6.0,
            bottom: 6.0,
        },
        ChildAlign::Start,
        true,
    );
    // 不写死宽高：fit_content 才有东西可收缩。
    style.width = None;
    style.height = None;
    style.anchors = Anchors::CENTER;
    style.pivot = Pivot::CENTER;
    world.insert(root, style);

    let first = spawn_child(&mut world, root, Style::fixed(60.0, 20.0));
    let second = spawn_child(&mut world, root, Style::fixed(40.0, 30.0));

    ui_layout_system(&mut world);

    // 内容宽 = 60 + 10 + 40 = 110，加左右 padding 8 → 118；
    // 内容高 = max(20,30) = 30，加上下 padding 12 → 42。
    let root_rect = rect_of(&world, root);
    assert_eq!(root_rect[2..], [118.0, 42.0]);
    // 居中锚点 + pivot 0.5 → 原点 = 屏幕中心 - 尺寸一半。
    assert_eq!(root_rect[0..2], [400.0 - 59.0, 300.0 - 21.0]);
    assert_eq!(rect_of(&world, first)[0], 400.0 - 59.0 + 4.0);
    assert_eq!(rect_of(&world, second)[0], 400.0 - 59.0 + 4.0 + 60.0 + 10.0);
}

#[test]
fn a_layout_group_overrides_child_anchors_but_not_its_own() {
    let mut world = world_with_screen();
    let root = world.spawn();
    world.insert(root, Node {});
    world.insert(
        root,
        layout_style(
            200.0,
            100.0,
            LayoutDirection::Row,
            0.0,
            Margin::default(),
            ChildAlign::Start,
            false,
        ),
    );
    // 子元素自己写「居中锚点」，但在布局组里必须由游标定位。
    let mut child_style = Style::fixed(40.0, 20.0);
    child_style.anchors = Anchors::CENTER;
    child_style.pivot = Pivot::CENTER;
    let child = spawn_child(&mut world, root, child_style);

    ui_layout_system(&mut world);

    let root_rect = rect_of(&world, root);
    assert_eq!(
        rect_of(&world, child),
        [root_rect[0], root_rect[1], 40.0, 20.0]
    );
}

#[test]
fn children_without_a_layout_group_keep_their_anchors() {
    let mut world = world_with_screen();
    let root = world.spawn();
    world.insert(root, Node {});
    world.insert(root, Style::fixed(200.0, 100.0));
    let child = spawn_child(&mut world, root, Style::fixed(40.0, 20.0));

    ui_layout_system(&mut world);

    // 默认 STRETCH 锚点 + 固定尺寸 → 居中（旧行为）。
    let root_rect = rect_of(&world, root);
    assert_eq!(
        rect_of(&world, child),
        [root_rect[0] + 80.0, root_rect[1] + 40.0, 40.0, 20.0]
    );
}
