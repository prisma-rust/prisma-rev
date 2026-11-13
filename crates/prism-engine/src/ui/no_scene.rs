//! Placeholder screen for "no scene was loaded".
//!
//! A player that resolves no scene used to draw nothing but the clear colour: a
//! plain grey window with no hint about the missing `assets/*.pak`, which reads
//! as "the engine is broken" rather than "the assets were not packaged".
//!
//! This pushes a slowly scrolling checkerboard plus centred text into the regular
//! [`UiDrawList`], so the frame still comes from the ordinary UI pass — no extra
//! render pass, no special shader, and the clear colour keeps its own meaning.

use super::components::TextAlign;
use super::layout::ScreenSize;
use super::render::{UiDrawList, UiQuad, UiTextCmd};
use prism_ecs::World;

/// Grid cell size is derived from the window width so a 4K window does not
/// multiply the quad count; clamped so the pattern stays recognisable.
fn cell_size(width: f32) -> f32 {
    (width / 32.0).clamp(48.0, 128.0)
}

/// How fast the pattern slides, in points per second.
const SCROLL_POINTS_PER_SEC: f32 = 26.0;

const DARK: [f32; 4] = [0.16, 0.16, 0.18, 1.0];
const LIGHT: [f32; 4] = [0.30, 0.30, 0.34, 1.0];
const PANEL: [f32; 4] = [0.03, 0.03, 0.05, 0.82];
const TITLE: [f32; 4] = [1.0, 0.82, 0.35, 1.0];
const BODY: [f32; 4] = [0.92, 0.92, 0.94, 1.0];
const HINT: [f32; 4] = [0.72, 0.86, 1.0, 1.0];

/// Layer pairing: pattern below the panel, panel below the text.
const PATTERN_LAYER: i32 = 9_000;
const PANEL_LAYER: i32 = 9_001;
const TEXT_LAYER: i32 = 9_002;

/// Append the "no scene" placeholder to `list`.
///
/// `time` is seconds since the placeholder first appeared; it only drives the
/// scroll offset. Nothing is drawn when the window size is not known yet.
pub fn push_no_scene_screen(list: &mut UiDrawList, width: f32, height: f32, time: f32) {
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    push_checkerboard(list, width, height, time);
    if width < 160.0 || height < 120.0 {
        // Too small for a readable panel; the pattern alone is already unmistakable.
        return;
    }

    // The panel keeps the text readable over the pattern at any scroll offset.
    let panel_width = (width * 0.72).min(width - 24.0).max(1.0);
    let panel_height = (height * 0.32).clamp(160.0, 280.0);
    let panel_left = (width - panel_width) * 0.5;
    let panel_top = (height - panel_height) * 0.5;
    list.push_quad(UiQuad {
        rect: [panel_left, panel_top, panel_width, panel_height],
        color: PANEL,
        border_radius: 14.0,
        texture: None,
        layer: PANEL_LAYER,
        order: 0,
    });

    let text_width = panel_width - 32.0;
    let text_left = panel_left + 16.0;
    let line_gap = 8.0;
    let title_size = (panel_height * 0.19).clamp(20.0, 34.0);
    let body_size = (panel_height * 0.115).clamp(13.0, 20.0);
    let hint_size = body_size * 0.92;

    let title_top = panel_top + panel_height * 0.12;
    push_text(
        list,
        [text_left, title_top, text_width, title_size * 1.3],
        "NO SCENE LOADED",
        title_size,
        TITLE,
    );
    push_text(
        list,
        [
            text_left,
            title_top + title_size * 1.55,
            text_width,
            body_size * 1.5,
        ],
        "Scenes are read from assets/*.pak and none was mounted.",
        body_size,
        BODY,
    );
    // 提示里给完整命令：只写 "run build-scenes" 的话，读的人还得猜
    // `cargo run build-scenes` 之类的错误写法。
    let steps = match crate::asset_paths::asset_dir() {
        Some(dir) => crate::asset_paths::build_scenes_steps(&dir),
        None => [
            "cd <project that owns assets/>".to_string(),
            "cargo run -q -p prism-build-pipeline -- build-scenes".to_string(),
        ],
    };
    let mut hint_top = title_top + title_size * 1.55 + body_size * 1.5 + line_gap;
    for line in [
        "Build it (or just run bash scripts/dev.sh <project>):".to_string(),
        steps[0].clone(),
        steps[1].clone(),
    ] {
        push_text(
            list,
            [text_left, hint_top, text_width, hint_size * 1.5],
            line.as_str(),
            hint_size,
            HINT,
        );
        hint_top += hint_size * 1.65;
    }
}

fn push_checkerboard(list: &mut UiDrawList, width: f32, height: f32, time: f32) {
    let cell = cell_size(width);
    let period = cell * 2.0;
    // Wrapping at one full period keeps the pattern seamless: the grid shifts by
    // exactly two cells, which has the same parity.
    let shift = (time * SCROLL_POINTS_PER_SEC).rem_euclid(period);
    let step = cell + 1.0; // one point of overlap, so no seam shows between cells
    let mut row = 0;
    let mut top = -shift;
    while top < height {
        let mut col = 0;
        let mut left = -shift;
        while left < width {
            let color = if (row + col) % 2 == 0 { DARK } else { LIGHT };
            list.push_quad(UiQuad {
                rect: [left, top, step, step],
                color,
                border_radius: 0.0,
                texture: None,
                layer: PATTERN_LAYER,
                order: 0,
            });
            left += cell;
            col += 1;
        }
        top += cell;
        row += 1;
    }
}

fn push_text(
    list: &mut UiDrawList,
    rect: [f32; 4],
    content: &str,
    font_size: f32,
    color: [f32; 4],
) {
    list.push_text(UiTextCmd {
        rect,
        content: content.to_string(),
        font_size,
        color,
        alignment: TextAlign::Center,
        font: String::new(),
        wrap: true,
        layer: TEXT_LAYER,
        order: 0,
    });
}

/// Append the placeholder to the frame's [`UiDrawList`] resource.
///
/// No-op when the host has not published a screen size yet or the UI pass is not
/// running (a UI-less build), so it can be called unconditionally from `update`.
pub fn push_no_scene_screen_into(world: &mut World, time: f32) {
    let Some(size) = world.get_resource::<ScreenSize>().copied() else {
        return;
    };
    let Some(list) = world.get_resource_mut::<UiDrawList>() else {
        return;
    };
    push_no_scene_screen(list, size.width, size.height, time);
}

#[cfg(test)]
#[path = "no_scene_tests.rs"]
mod tests;
