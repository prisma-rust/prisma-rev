//! GPU readback test for the #502 color-grading knobs.
//!
//! Renders a headless frame through the real `GraphRenderer` pipeline
//! (ForwardPass clear → PostPass tonemap + grading) and inspects the pixels.
//! This is the end-to-end check that the push-constant fields actually reach
//! `post.slang` — the pure-Rust layout test can only prove the offsets.
//!
//! Like `headless_clear.rs` it gracefully skips on hosts without a Vulkan
//! driver (CI containers without a GPU).

use prism_render::descriptor::FrameUBOData;
use prism_render::render_graph::PostGrade;
use prism_render::{GraphRenderer, RenderMode};

const SIZE: usize = 256;

/// Identity 4×4 in the column-major shape the renderer uses.
const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn try_create_headless() -> Option<GraphRenderer> {
    match GraphRenderer::headless_new(None) {
        Ok(renderer) => Some(renderer),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                || msg.contains("VK_ERROR_EXTENSION_NOT_PRESENT")
                || msg.contains("no devices")
                || msg.contains("No valid")
                || msg.contains("PhysicalDevice")
            {
                eprintln!("[SKIP] no Vulkan driver: {e:#}");
                return None;
            }
            panic!("headless_new failed: {e:#}");
        }
    }
}

/// Render an empty (clear-only) frame with `grade` and return the RGBA8 pixels.
fn render_graded(renderer: &mut GraphRenderer, grade: PostGrade) -> Vec<u8> {
    let frame_data = FrameUBOData::default();
    renderer
        .render(
            &[], // no draw items
            &frame_data,
            IDENTITY, // light view-proj
            IDENTITY, // inverse projection
            0,        // debug flags
            2,        // tonemap: passthrough, so only grading changes the pixel
            0,        // AA off
            0,        // debug_rt
            1.0,      // proj22
            0.0,      // proj32
            &[],      // no lights
            RenderMode::Raster,
            0,   // pt bounces
            0.0, // pt ray distance
            0,   // pt iterations
            1.0, // exposure
            &[], // no PT lights
            grade,
            false,
            &[], // bone matrices
        )
        .expect("headless render failed");
    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback failed");
    renderer.readback_pixels().expect("readback_pixels failed")
}

fn pixel(pixels: &[u8], x: usize, y: usize) -> [u8; 4] {
    let index = (y * SIZE + x) * 4;
    [
        pixels[index],
        pixels[index + 1],
        pixels[index + 2],
        pixels[index + 3],
    ]
}

#[test]
fn a_neutral_grade_leaves_the_frame_flat() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let pixels = render_graded(&mut renderer, PostGrade::default());
    assert_eq!(pixels.len(), SIZE * SIZE * 4);

    let center = pixel(&pixels, SIZE / 2, SIZE / 2);
    let corner = pixel(&pixels, 0, 0);
    assert_eq!(
        center, corner,
        "neutral grading must keep the cleared frame uniform"
    );
}

#[test]
fn a_vignette_darkens_the_corners_but_not_the_center() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let plain = render_graded(&mut renderer, PostGrade::default());
    let graded = render_graded(
        &mut renderer,
        PostGrade {
            vignette: 1.0,
            ..PostGrade::default()
        },
    );

    let plain_center = pixel(&plain, SIZE / 2, SIZE / 2);
    let graded_center = pixel(&graded, SIZE / 2, SIZE / 2);
    let graded_corner = pixel(&graded, 0, 0);

    // 中心几乎不动（`smoothstep` 从 0.35 才开始），四角明显变暗。
    assert!(
        graded_center[0].abs_diff(plain_center[0]) <= 4,
        "center changed too much: {plain_center:?} -> {graded_center:?}"
    );
    assert!(
        graded_corner[0] + 40 < graded_center[0],
        "corner should be much darker than the center: {graded_corner:?} vs {graded_center:?}"
    );
}

#[test]
fn zero_saturation_flattens_the_channels() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    // The wrapper clears to a neutral grey, so saturation 0 alone cannot prove
    // much on its own — but it must at least keep RGB equal (the luma blend is
    // a no-op on grey and must not introduce a tint).
    let pixels = render_graded(
        &mut renderer,
        PostGrade {
            saturation: 0.0,
            ..PostGrade::default()
        },
    );
    let center = pixel(&pixels, SIZE / 2, SIZE / 2);
    assert_eq!(
        center[0], center[1],
        "saturation 0 must flatten red/green: {center:?}"
    );
    assert_eq!(
        center[1], center[2],
        "saturation 0 must flatten green/blue: {center:?}"
    );
}
