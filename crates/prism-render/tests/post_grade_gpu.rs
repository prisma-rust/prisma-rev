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

#[test]
fn film_grain_is_off_by_default_and_varies_pixels_at_full_strength() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let plain = render_graded(&mut renderer, PostGrade::default());
    let plain_again = render_graded(&mut renderer, PostGrade::default());
    assert_eq!(
        plain, plain_again,
        "the default grade must stay deterministic and grain-free"
    );

    let grained = render_graded(
        &mut renderer,
        PostGrade {
            film_grain: 1.0,
            ..PostGrade::default()
        },
    );

    let mut min = u8::MAX;
    let mut max = 0u8;
    for px in grained.as_chunks::<4>().0 {
        // Monochrome grain: all three colour channels move together.
        assert_eq!(px[0], px[1], "grain must be monochrome: {px:?}");
        assert_eq!(px[1], px[2], "grain must be monochrome: {px:?}");
        min = min.min(px[0]);
        max = max.max(px[0]);
    }
    assert!(
        max.saturating_sub(min) >= 16,
        "film grain should vary a flat frame: min={min}, max={max}"
    );
}

/// #502：白平衡在默认值上是逐位 no-op；正值偏暖、负值偏冷、tint 控制品红/绿。
#[test]
fn white_balance_tints_a_neutral_frame_and_zero_is_a_no_op() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let plain = render_graded(&mut renderer, PostGrade::default());
    let plain_again = render_graded(&mut renderer, PostGrade::default());
    assert_eq!(plain, plain_again, "neutral white balance must be stable");

    let warm = render_graded(
        &mut renderer,
        PostGrade {
            temperature: 1.0,
            ..PostGrade::default()
        },
    );
    let warm_center = pixel(&warm, SIZE / 2, SIZE / 2);
    assert!(
        warm_center[0] > warm_center[2] + 10,
        "warm balance should raise red above blue: {warm_center:?}"
    );

    let cool = render_graded(
        &mut renderer,
        PostGrade {
            temperature: -1.0,
            ..PostGrade::default()
        },
    );
    let cool_center = pixel(&cool, SIZE / 2, SIZE / 2);
    assert!(
        cool_center[2] > cool_center[0] + 10,
        "cool balance should raise blue above red: {cool_center:?}"
    );

    let magenta = render_graded(
        &mut renderer,
        PostGrade {
            tint: 1.0,
            ..PostGrade::default()
        },
    );
    let magenta_center = pixel(&magenta, SIZE / 2, SIZE / 2);
    assert!(
        magenta_center[0] > magenta_center[1] && magenta_center[2] > magenta_center[1],
        "magenta tint should raise red/blue above green: {magenta_center:?}"
    );
}

#[test]
fn bloom_brightens_a_uniform_hdr_frame() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let plain = render_graded(&mut renderer, PostGrade::default());
    let bloomed = render_graded(
        &mut renderer,
        PostGrade {
            bloom_intensity: 0.4,
            bloom_threshold: 0.0,
            ..PostGrade::default()
        },
    );

    let plain_center = pixel(&plain, SIZE / 2, SIZE / 2);
    let bloomed_center = pixel(&bloomed, SIZE / 2, SIZE / 2);
    assert!(
        bloomed_center[0] >= plain_center[0].saturating_add(8),
        "bloom should add light: {plain_center:?} -> {bloomed_center:?}"
    );
}

#[test]
fn a_high_bloom_threshold_leaves_the_frame_unchanged() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let plain = render_graded(&mut renderer, PostGrade::default());
    let bloomed = render_graded(
        &mut renderer,
        PostGrade {
            bloom_intensity: 1.0,
            bloom_threshold: 10.0,
            ..PostGrade::default()
        },
    );

    assert_eq!(
        plain, bloomed,
        "a threshold above every pixel must make bloom a no-op"
    );
}
