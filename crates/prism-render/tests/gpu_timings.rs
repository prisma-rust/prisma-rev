//! GPU timestamp test for the #518 per-pass timings.
//!
//! Renders two headless frames through the real `GraphRenderer` and checks the
//! second call exposed timings for the first one (the readback is deliberately
//! delayed by one frame). Gracefully skips on hosts without a Vulkan driver.

use prism_render::descriptor::FrameUBOData;
use prism_render::render_graph::PostGrade;
use prism_render::{GraphRenderer, RenderMode};

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

fn render_once(renderer: &mut GraphRenderer) {
    renderer
        .render(
            &[],
            &FrameUBOData::default(),
            IDENTITY,
            IDENTITY,
            0,   // debug flags
            2,   // tonemap: passthrough
            0,   // AA off
            0,   // debug_rt
            1.0, // proj22
            0.0, // proj32
            &[],
            RenderMode::Raster,
            0,
            0.0,
            0,
            1.0,
            &[],
            PostGrade::default(),
            false,
            false,
        )
        .expect("headless render failed");
}

#[test]
fn the_second_frame_reports_the_first_frames_pass_timings() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };

    render_once(&mut renderer);
    // 第一帧结束时还没有可读的结果（读回被有意延迟一帧）。
    render_once(&mut renderer);

    let timings = renderer.gpu_timings();
    if !timings.supported {
        eprintln!("[SKIP] device does not report timestamps");
        return;
    }
    assert!(
        !timings.passes.is_empty(),
        "a rendered frame must produce at least one pass timing"
    );
    let total: f32 = timings.passes.iter().map(|pass| pass.ms).sum();
    assert!(
        (total - timings.total_ms).abs() < 1e-3,
        "total must equal the sum of the passes ({total} vs {})",
        timings.total_ms
    );
    assert!(
        timings.passes.iter().all(|pass| pass.ms >= 0.0),
        "timings are non-negative"
    );
    assert!(
        timings.passes.iter().any(|pass| pass.ms > 0.0),
        "at least one pass must have taken measurable time"
    );
}
