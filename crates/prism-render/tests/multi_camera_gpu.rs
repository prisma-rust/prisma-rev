//! GPU test for depth-ordered multi-camera compositing (#461).
//!
//! Two full-screen cameras are composited in order. Each camera's
//! `clearFlags` decides whether it clears colour / depth, so the final pixel
//! proves both the ordering and the clear semantics without any geometry:
//!
//! 1. red `SolidColor` camera, then a `Nothing` camera → **red** survives;
//! 2. red `SolidColor`, then green `SolidColor` → **green** wins.
//!
//! Skips gracefully on hosts without a Vulkan driver.

use prism_render::descriptor::FrameUBOData;
use prism_render::render_graph::CameraView;
use prism_render::{DrawState, FrameInput, GraphRenderer, PostGrade, RenderMode};

const SIZE: usize = 64;

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn try_create() -> Option<GraphRenderer> {
    match GraphRenderer::offscreen_new(SIZE as u32, SIZE as u32, None) {
        Ok(renderer) => Some(renderer),
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                || message.contains("VK_ERROR_EXTENSION_NOT_PRESENT")
                || message.contains("no devices")
                || message.contains("No valid")
                || message.contains("PhysicalDevice")
            {
                eprintln!("[SKIP] no Vulkan driver: {error:#}");
                return None;
            }
            panic!("offscreen_new failed: {error:#}");
        }
    }
}

fn frame_data() -> FrameUBOData {
    FrameUBOData {
        view_proj: IDENTITY,
        camera_position: [0.0, 0.0, 3.0, 0.0],
        light_direction: [0.3, 0.8, 0.5, 0.8],
        light_color: [1.0, 1.0, 1.0, 1.0],
        view: IDENTITY,
        light_view_proj: IDENTITY,
        light_view_proj1: IDENTITY,
        shadow_split: 0.0,
        shadow_cascade_count: 1,
        // Passthrough: the readback is the raw clear colour.
        tonemap_mode: 2,
        viewport_size: [SIZE as f32, SIZE as f32],
        exposure: 1.0,
        shadow_strength: 0.0,
        _pad2: [0.0; 3],
        _pad3: [0.0; 2],
    }
}

fn camera(color: [f32; 4], flags: (bool, bool)) -> CameraView {
    CameraView {
        view_proj: IDENTITY,
        view: IDENTITY,
        inv_view: IDENTITY,
        eye: [0.0, 0.0, 3.0],
        clear_color: color,
        clear_color_enabled: flags.0,
        clear_depth: flags.1,
        draw_skybox: false,
        exposure: 1.0,
        has_camera: true,
    }
}

fn render(renderer: &mut GraphRenderer, cameras: &[CameraView]) -> [u8; 4] {
    let data = frame_data();
    let ctx = renderer.begin_frame().expect("begin_frame").expect("frame");
    let input = FrameInput {
        draw_items: &[],
        gizmos: &[],
        sprite_items: &[],
        sprite_lights: &[],
        particle_items: &[],
        bone_matrices: &[],
        skybox: None,
        frame_data: &data,
        cameras,
        light_view_proj: IDENTITY,
        light_view_proj1: IDENTITY,
        shadow_split: 0.0,
        shadow_cascade_count: 1,
        inv_projection: IDENTITY,
        debug_flags: 0,
        wireframe: false,
        shaded_wireframe: false,
        tonemap_mode: 2,
        aa_mode: 0,
        debug_rt: 0,
        grade: PostGrade::default(),
        proj22: 1.0,
        proj32: 0.0,
        lights: &[],
        render_mode: RenderMode::Raster,
        pt_max_bounces: 0,
        pt_ray_max_distance: 0.0,
        pt_max_iterations: 0,
        exposure: 1.0,
        pt_lights: &[],
        pt_accum_dirty: true,
        has_camera: true,
        camera_orthographic: false,
        clear_color: [0.5, 0.5, 0.5, 1.0],
        shadows_enabled: false,
        ui_overlay: None,
        show_grid: false,
    };
    renderer.prepare(&ctx, &input).expect("prepare");
    renderer.execute(&ctx, &input).expect("execute");
    let out_of_date = renderer.present(&ctx).expect("present");
    renderer.end_frame(&ctx, out_of_date).expect("end_frame");

    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback");
    let pixels = renderer.readback_pixels().expect("readback_pixels");
    let center = (SIZE / 2 * SIZE + SIZE / 2) * 4;
    [
        pixels[center],
        pixels[center + 1],
        pixels[center + 2],
        pixels[center + 3],
    ]
}

#[test]
fn depth_ordered_cameras_composite_and_honour_clear_flags() {
    let Some(mut renderer) = try_create() else {
        return;
    };

    // Camera 1 is `Nothing`: it must not clear the red background.
    let red = camera([1.0, 0.0, 0.0, 1.0], (true, true));
    let nothing = camera([0.0, 0.0, 1.0, 1.0], (false, false));
    let pixel = render(&mut renderer, &[red, nothing]);
    assert!(
        pixel[0] > 200 && pixel[1] < 60,
        "`Nothing` must preserve the previous camera's colour, got {pixel:?}"
    );

    // Both cameras clear colour: the later (higher depth) one wins.
    let green = camera([0.0, 1.0, 0.0, 1.0], (true, true));
    let pixel = render(&mut renderer, &[red, green]);
    assert!(
        pixel[1] > 200 && pixel[0] < 60,
        "the last camera's clear colour must win, got {pixel:?}"
    );
}

#[test]
fn a_single_implicit_camera_still_renders_the_legacy_clear_colour() {
    let Some(mut renderer) = try_create() else {
        return;
    };
    // Empty camera list = legacy path: `clear_color` from the frame input.
    let pixel = render(&mut renderer, &[]);
    let expected = [128u8, 128, 128];
    for (actual, expected) in pixel.iter().zip(expected.iter()) {
        assert!(
            actual.abs_diff(*expected) <= 2,
            "legacy gray clear must be preserved, got {pixel:?}"
        );
    }
    let _ = DrawState::default();
}
