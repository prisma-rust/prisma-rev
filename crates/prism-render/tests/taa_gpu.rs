//! GPU tests for the TAA resolve pass (#883 Phase 1).
//!
//! Disabling TAA must stay bit-identical to the pre-TAA path; enabling it
//! must change the jittered frame and keep rendering across a camera move.
//! Skips gracefully on hosts without a Vulkan driver.

#[path = "golden_support/mod.rs"]
mod golden_support;

use golden_support::{draw_item, FrameHarness, FrameOptions};

fn changed(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
        .filter(|(a, b)| a[..3].iter().zip(&b[..3]).any(|(x, y)| x.abs_diff(*y) > 2))
        .count()
}

#[test]
fn taa_is_off_by_default_and_can_resolve_a_jittered_frame() {
    let Some(mut harness) = FrameHarness::new(256) else {
        return;
    };

    // A rotated box gives diagonal edges, so subpixel jitter changes coverage.
    let mut item = draw_item(harness.mesh_handle());
    let c = std::f32::consts::FRAC_1_SQRT_2;
    let s = c;
    item.model = [
        [c, s, 0.0, 0.0],
        [-s, c, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let off_a = harness.render_draws(std::slice::from_ref(&item), FrameOptions::default());
    let off_b = harness.render_draws(std::slice::from_ref(&item), FrameOptions::default());
    assert_eq!(off_a, off_b, "TAA off must stay deterministic");

    harness.set_taa(true, 0.9);
    let mut taa_a = Vec::new();
    let mut taa_b = Vec::new();
    for _ in 0..12 {
        taa_a = taa_b;
        taa_b = harness.render_draws(std::slice::from_ref(&item), FrameOptions::default());
    }
    let diff = changed(&off_a, &taa_b);
    assert!(
        diff > 20,
        "TAA should change the jittered frame after several samples"
    );
    assert!(
        changed(&taa_a, &taa_b) > 0,
        "the jitter sequence should change consecutive TAA frames"
    );

    // A camera move must not crash and must keep producing a frame.
    let moved = harness.render_draws(
        std::slice::from_ref(&item),
        FrameOptions {
            camera_position: Some([0.0, 0.0, 3.2]),
            ..FrameOptions::default()
        },
    );
    assert_eq!(moved.len(), 256 * 256 * 4);

    // Turning TAA off returns to the legacy path.
    harness.set_taa(false, 0.9);
    let off_c = harness.render_draws(std::slice::from_ref(&item), FrameOptions::default());
    assert_eq!(off_a, off_c, "disabling TAA must restore the legacy path");
}

#[test]
fn taa_handles_object_motion_and_reactive_transparency() {
    let Some(mut harness) = FrameHarness::new(256) else {
        return;
    };
    harness.set_taa(true, 0.9);

    let mut item = draw_item(harness.mesh_handle());
    // Move the box sideways for several frames. With per-object velocity the
    // resolve must keep producing a stable frame instead of accumulating the
    // old silhouette.
    for frame in 0..8 {
        item.model[3][0] = (frame as f32 - 4.0) * 0.12;
        let pixels = harness.render_draws(std::slice::from_ref(&item), FrameOptions::default());
        assert_eq!(pixels.len(), 256 * 256 * 4);
    }

    // Transparent draws set the reactive mask (normal.w), so TAA must trust the
    // current frame rather than blending in stale history. This mainly guards
    // the render-pass/pipeline wiring (three MRT attachments + mask flag).
    let mut transparent = item;
    transparent.state.src_blend = prism_gfx::BlendMode::SrcAlpha;
    transparent.state.dst_blend = prism_gfx::BlendMode::OneMinusSrcAlpha;
    transparent.state.z_write = false;
    transparent.model[3][0] = 0.0;
    let a = harness.render_draws(std::slice::from_ref(&transparent), FrameOptions::default());
    transparent.model[3][0] = 0.2;
    let b = harness.render_draws(std::slice::from_ref(&transparent), FrameOptions::default());
    assert_eq!(a.len(), b.len());
    assert_ne!(a, b, "moving transparent geometry must still update");
}
