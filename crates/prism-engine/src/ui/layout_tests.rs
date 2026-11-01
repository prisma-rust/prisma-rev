//! `ComputedLayout` 是布局系统的输出：作者只写 `Node + Style`。
//!
//! 层级只认场景的 `Parent`（`prism_scene::HierarchyHelper` 维护），没有第二份
//! `UiParent` 状态。

use super::{ui_layout_system, ScreenSize};
use crate::ui::components::*;
use prism_ecs::World;
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
