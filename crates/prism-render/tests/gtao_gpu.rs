//! GPU coverage for GTAO's temporal history path (#883 Phase 3).
//!
//! These renders go through `GtaoPass` with a temporal set (previous AO +
//! velocity) and exercise both the first-frame raw path and subsequent
//! reprojected frames. A full image comparison of AO itself is not exposed by
//! `FrameHarness`, so the test asserts stable frame production while a box
//! moves across the screen.

#[path = "golden_support/mod.rs"]
mod golden_support;

use golden_support::{draw_item, FrameHarness, FrameOptions};

#[test]
fn gtao_temporal_history_survives_object_motion() {
    let Some(mut harness) = FrameHarness::new(256) else {
        return;
    };

    let mut item = draw_item(harness.mesh_handle());
    for frame in 0..8 {
        item.model[3][0] = (frame as f32 - 4.0) * 0.15;
        let pixels = harness.render_draws(std::slice::from_ref(&item), FrameOptions::default());
        assert_eq!(pixels.len(), 256 * 256 * 4);
    }

    // A camera move changes the velocity field; the temporal descriptor set
    // must follow it without validation errors or a dropped frame.
    let moved = harness.render_draws(
        std::slice::from_ref(&item),
        FrameOptions {
            camera_position: Some([0.0, 0.0, 3.4]),
            ..FrameOptions::default()
        },
    );
    assert_eq!(moved.len(), 256 * 256 * 4);
}
