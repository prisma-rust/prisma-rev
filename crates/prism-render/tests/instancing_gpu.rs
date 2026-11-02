//! GPU readback test for static-mesh instancing (#491).
//!
//! Two copies of the same box/mesh/material are submitted with different model
//! translations. `ForwardPass` groups them into one instanced draw; the image
//! must contain a box on both sides. A broken instance matrix (e.g. both draws
//! using the first model) would leave one side empty and fail the comparison.
//! Skips gracefully on hosts without a Vulkan driver.

#[path = "golden_support/mod.rs"]
mod golden_support;

use golden_support::{draw_item, FrameHarness, FrameOptions};
use prism_render::DrawItem;

fn translated(mesh: prism_render::managers::MeshHandle, x: f32) -> DrawItem {
    let mut item = draw_item(mesh);
    item.model[3] = [x, 0.0, 0.0, 1.0];
    item
}

fn changed(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
        .filter(|(a, b)| a[..3].iter().zip(&b[..3]).any(|(x, y)| x.abs_diff(*y) > 4))
        .count()
}

#[test]
fn identical_static_draws_are_instanced_at_their_own_transforms() {
    let Some(mut harness) = FrameHarness::new(256) else {
        return;
    };
    let mesh = harness.mesh_handle();
    let left = translated(mesh, -0.6);
    let right = translated(mesh, 0.6);

    let left_only = harness.render_draws(std::slice::from_ref(&left), FrameOptions::default());
    let right_only = harness.render_draws(std::slice::from_ref(&right), FrameOptions::default());
    let both = harness.render_draws(&[left, right], FrameOptions::default());

    let right_side = changed(&both, &left_only);
    let left_side = changed(&both, &right_only);
    assert!(
        right_side > 300,
        "the right instance should draw a second box, changed={right_side}"
    );
    assert!(
        left_side > 300,
        "the left instance should draw its own box, changed={left_side}"
    );
}
