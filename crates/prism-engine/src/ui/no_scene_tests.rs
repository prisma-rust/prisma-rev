//! Tests for the "no scene" placeholder screen.

use super::{push_no_scene_screen, push_no_scene_screen_into};
use crate::ui::layout::ScreenSize;
use crate::ui::render::{UiDrawItem, UiDrawList};
use prism_ecs::World;

fn drawn(width: f32, height: f32, time: f32) -> UiDrawList {
    let mut list = UiDrawList::default();
    push_no_scene_screen(&mut list, width, height, time);
    list
}

#[test]
fn unknown_window_size_draws_nothing() {
    assert!(drawn(0.0, 720.0, 0.0).items.is_empty());
    assert!(drawn(1280.0, 0.0, 0.0).items.is_empty());
}

#[test]
fn pattern_covers_the_window_and_text_sits_on_top() {
    let list = drawn(1280.0, 720.0, 0.0);
    let quads: Vec<_> = list.quads().collect();
    // A 1280 pt window with a 48 pt cell needs at least 26 x 15 cells.
    assert!(quads.len() > 26 * 15, "only {} quads", quads.len());

    // Every quad is inside the extended grid and has a positive size.
    for quad in &quads {
        assert!(quad.rect[2] > 0.0 && quad.rect[3] > 0.0);
        assert!(quad.rect[0] < 1280.0 && quad.rect[1] < 720.0);
    }

    let texts: Vec<_> = list.texts().collect();
    assert_eq!(texts.len(), 5);
    assert!(texts[0].content.contains("NO SCENE"));
    // The hint must be copy-pasteable, not a bare "run build-scenes".
    let hint: Vec<&str> = texts.iter().map(|text| text.content.as_str()).collect();
    assert!(hint.iter().any(|line| line.starts_with("cd ")));
    assert!(hint
        .iter()
        .any(|line| line.contains("prism-build-pipeline") && line.contains("build-scenes")));
    // Text is centred on the window and drawn above the pattern.
    let top = &texts[0];
    assert!((top.rect[0] + top.rect[2] * 0.5 - 640.0).abs() < 1.0);
    assert!(quads.iter().all(|quad| quad.layer < top.layer));
}

#[test]
fn pattern_scrolls_with_time() {
    let first = drawn(1280.0, 720.0, 0.0);
    let later = drawn(1280.0, 720.0, 0.5);
    let a = first.quads().next().unwrap();
    let b = later.quads().next().unwrap();
    assert_ne!(
        a.rect[0], b.rect[0],
        "the checkerboard should slide horizontally"
    );
    assert_ne!(a.rect[1], b.rect[1], "and vertically");
}

#[test]
fn one_full_period_is_seamless() {
    // The scroll wraps at two cells, so a whole period looks identical again.
    let start = drawn(1280.0, 720.0, 0.0);
    let period = super::cell_size(1280.0) * 2.0 / super::SCROLL_POINTS_PER_SEC;
    let wrapped = drawn(1280.0, 720.0, period);
    let a = start.quads().next().unwrap();
    let b = wrapped.quads().next().unwrap();
    assert!((a.rect[0] - b.rect[0]).abs() < 0.01);
    assert!((a.rect[1] - b.rect[1]).abs() < 0.01);
}

#[test]
fn world_hook_requires_screen_size_and_draw_list() {
    let mut world = World::new();
    push_no_scene_screen_into(&mut world, 0.0);
    world.insert_resource(ScreenSize::new(800, 600));
    push_no_scene_screen_into(&mut world, 0.0);
    assert!(world.get_resource::<UiDrawList>().is_none());

    world.insert_resource(UiDrawList::default());
    push_no_scene_screen_into(&mut world, 0.0);
    let list = world.get_resource::<UiDrawList>().unwrap();
    assert!(matches!(list.items.first(), Some(UiDrawItem::Quad(_))));
    assert!(!list.items.is_empty());
}
