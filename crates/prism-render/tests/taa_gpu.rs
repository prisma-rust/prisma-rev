//! GPU tests for the TAA resolve pass (#883 Phase 1).
//!
//! Disabling TAA must stay bit-identical to the pre-TAA path; enabling it
//! must change the jittered frame and keep rendering across a camera move.
//! Skips gracefully on hosts without a Vulkan driver.

#[path = "golden_support/mod.rs"]
mod golden_support;

use golden_support::{draw_item, FrameHarness, FrameOptions};
use prism_render::RenderMode;

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
/// Path-trace mode resolves the traced image with the guides the tracer writes,
/// not with the raster G-buffer (#883 follow-up).
#[test]
fn taa_resolves_the_path_traced_image() {
    let Some(mut harness) = FrameHarness::new(256) else {
        return;
    };
    let item = draw_item(harness.mesh_handle());
    let pt_options = FrameOptions {
        render_mode: RenderMode::PathTrace,
        ..FrameOptions::default()
    };

    // The tracer accumulates, so let it settle before comparing frames.
    let mut untemporal = Vec::new();
    for _ in 0..8 {
        untemporal = harness.render_draws(std::slice::from_ref(&item), pt_options);
    }
    assert_eq!(untemporal.len(), 256 * 256 * 4);

    // A traced frame must not be a flat clear: the box has to be visible.
    let lit = untemporal
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
        .count();
    assert!(lit > 100, "path tracer should light part of the frame");

    // Turning TAA on in path-trace mode must still produce a full frame. This
    // is the regression guard for the `!is_pt` gate that used to keep the
    // temporal resolve out of path-trace mode entirely.
    harness.set_taa(true, 0.9);
    let mut temporal = Vec::new();
    for _ in 0..8 {
        temporal = harness.render_draws(std::slice::from_ref(&item), pt_options);
    }
    assert_eq!(temporal.len(), 256 * 256 * 4);
    let resolved_lit = temporal
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
        .count();
    assert!(
        resolved_lit > 100,
        "the resolved path-traced frame should still show the scene"
    );

    // Disabling TAA must fall back to the raw traced image again.
    harness.set_taa(false, 0.9);
    let mut again = Vec::new();
    for _ in 0..8 {
        again = harness.render_draws(std::slice::from_ref(&item), pt_options);
    }
    assert_eq!(again.len(), 256 * 256 * 4);
}

/// The temporal resolve has to run *after* the tracer that feeds it, and the
/// tracer has to publish every guide the resolve reads. Both are easy to break
/// silently: the graph never reorders passes, and a missing guide only shows up
/// as a subtle ghosting artifact.
#[test]
fn the_resolve_runs_after_the_tracer_and_reads_its_guides() {
    use prism_render::render_graph::{PT_DEPTH_H, PT_NORMAL_ROUGHNESS_H, PT_VELOCITY_H};

    let Some(harness) = FrameHarness::new(64) else {
        return;
    };
    let snapshot = harness.graph().snapshot();

    let pt = snapshot
        .passes
        .iter()
        .find(|p| p.name == "PathTracePass")
        .expect("path tracer registered");
    let taa = snapshot
        .passes
        .iter()
        .find(|p| p.name == "TaaPass")
        .expect("taa registered");

    assert!(
        taa.index > pt.index,
        "TaaPass (#{}) must run after PathTracePass (#{}) so it can consume this frame's guides",
        taa.index,
        pt.index
    );

    for guide in [PT_DEPTH_H, PT_VELOCITY_H, PT_NORMAL_ROUGHNESS_H] {
        assert!(
            pt.outputs.contains(&guide),
            "PathTracePass must publish {guide:?}"
        );
    }
}
