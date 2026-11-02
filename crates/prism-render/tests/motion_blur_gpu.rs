//! GPU readback test for camera motion blur (#502 Phase 2).
//!
//! The first frame establishes the previous camera matrix. The second frame
//! moves the camera 0.3 m along +Z; with motion blur enabled the reprojected
//! trail must change edge pixels compared with the same move without the
//! effect. Skips gracefully on hosts without a Vulkan driver.

#[path = "golden_support/mod.rs"]
mod golden_support;

use golden_support::{FrameHarness, FrameOptions};
use prism_render::render_graph::PostGrade;

#[test]
fn camera_motion_blur_smears_a_moving_view() {
    let Some(mut moving) = FrameHarness::new(256) else {
        return;
    };
    let Some(mut sharp) = FrameHarness::new(256) else {
        return;
    };

    // Establish a previous camera matrix for the moving renderer.
    moving.render(FrameOptions::default());
    let blurred = moving.render(FrameOptions {
        camera_position: Some([0.0, 0.0, 3.3]),
        grade: PostGrade {
            motion_blur: 1.0,
            motion_blur_max_pixels: 16.0,
            ..PostGrade::default()
        },
        ..FrameOptions::default()
    });

    // Same camera position, effect off. This is the reference frame.
    let reference = sharp.render(FrameOptions {
        camera_position: Some([0.0, 0.0, 3.3]),
        ..FrameOptions::default()
    });

    let changed = blurred
        .as_chunks::<4>()
        .0
        .iter()
        .zip(reference.as_chunks::<4>().0.iter())
        .filter(|(before, after)| {
            before[..3]
                .iter()
                .zip(&after[..3])
                .any(|(a, b)| a.abs_diff(*b) > 4)
        })
        .count();
    assert!(
        changed > 300,
        "camera motion blur should visibly smear the moved view, changed={changed}"
    );
}
