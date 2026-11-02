//! GPU readback test for depth of field (#502 Phase 2).
//!
//! Reuses the golden-frame scene (one box at the origin, camera at `z = 3`) so
//! the depth buffer contains real geometry. Focuses at 0.2 m, far in front of
//! the box: the box must lose edge contrast. Skips gracefully on hosts without
//! a Vulkan driver.

#[path = "golden_support/mod.rs"]
mod golden_support;

use golden_support::{FrameHarness, FrameOptions};
use prism_render::render_graph::PostGrade;

#[test]
fn depth_of_field_blurs_geometry_outside_the_focus_plane() {
    let Some(mut harness) = FrameHarness::new(256) else {
        return;
    };

    let sharp = harness.render(FrameOptions::default());
    let sharp_again = harness.render(FrameOptions::default());
    assert_eq!(
        sharp, sharp_again,
        "the default grade must stay deterministic"
    );

    let blurred = harness.render(FrameOptions {
        grade: PostGrade {
            // The box sits 3 m from the camera; focusing at 0.2 m puts it far
            // outside the depth of field.
            dof_focus_distance: 0.2,
            dof_aperture: 1.0,
            dof_max_blur: 12.0,
            ..PostGrade::default()
        },
        ..FrameOptions::default()
    });

    let changed = sharp
        .as_chunks::<4>()
        .0
        .iter()
        .zip(blurred.as_chunks::<4>().0.iter())
        .filter(|(before, after)| {
            before[..3]
                .iter()
                .zip(&after[..3])
                .any(|(a, b)| a.abs_diff(*b) > 4)
        })
        .count();
    assert!(
        changed > 500,
        "depth of field should visibly blur the out-of-focus box, changed={changed}"
    );
}
