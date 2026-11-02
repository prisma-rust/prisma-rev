//! Golden-frame scenarios for the composed render graph.
//!
//! The scene, camera, extent and settings are fixed in `golden_support`; the
//! committed PNGs live in `tests/goldens`. Use `PRISM_UPDATE_GOLDENS=1` to
//! regenerate them after an intentional rendering change.

mod golden_support;

use golden_support::{
    assert_golden, compare, write_mismatch_artifacts, FrameHarness, FrameOptions,
};
use prism_render::render_graph::PostGrade;

const SIZE: usize = 256;

fn harness() -> Option<FrameHarness> {
    FrameHarness::new(SIZE)
}

#[test]
fn baseline_frame_matches_golden() {
    let Some(mut harness) = harness() else {
        return;
    };
    let pixels = harness.render(FrameOptions::default());
    assert_golden("baseline", &pixels, SIZE);
}

#[test]
fn shaded_wireframe_frame_matches_golden() {
    let Some(mut harness) = harness() else {
        return;
    };
    if !harness.supports_wireframe() {
        eprintln!("[SKIP] fillModeNonSolid not supported by this driver");
        return;
    }
    let pixels = harness.render(FrameOptions {
        shaded_wireframe: true,
        ..FrameOptions::default()
    });
    assert_golden("shaded_wireframe", &pixels, SIZE);
}

#[test]
fn bloom_frame_matches_golden() {
    let Some(mut harness) = harness() else {
        return;
    };
    let pixels = harness.render(FrameOptions {
        grade: PostGrade {
            bloom_intensity: 0.35,
            bloom_threshold: 0.0,
            ..PostGrade::default()
        },
        ..FrameOptions::default()
    });
    assert_golden("bloom", &pixels, SIZE);
}

/// The failure path must leave actionable artifacts for CI triage.
#[test]
fn mismatch_artifacts_include_actual_and_diff() {
    let expected = vec![0u8; 8 * 8 * 4];
    let mut actual = expected.clone();
    actual[0] = 255;
    actual[1] = 128;

    let diff = compare(&expected, &actual);
    assert_eq!(diff.changed_pixels, 1);
    assert_eq!(diff.max_diff, 255);
    assert!(!diff.is_within_tolerance());

    let dir = write_mismatch_artifacts("selftest", &actual, &expected, 8)
        .expect("write mismatch artifacts");
    assert!(
        dir.join("selftest.actual.png").is_file(),
        "actual artifact missing in {}",
        dir.display()
    );
    assert!(
        dir.join("selftest.diff.png").is_file(),
        "diff artifact missing in {}",
        dir.display()
    );
}
