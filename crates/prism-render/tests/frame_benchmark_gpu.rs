//! End-to-end frame-time benchmark over the golden-frame scene (#1002).
//!
//! Reuses the deterministic offscreen scene from `golden_support` and the real
//! composed render graph, so the numbers describe the frame the engine actually
//! records, not a single kernel. The test writes a `prism-bench-report` JSON to
//! `target/bench/frame-benchmark.json` and checks the report's shape; it does
//! **not** assert absolute timings, which move with the adapter, the driver and
//! the machine.
//!
//! Each sample covers one `FrameHarness::render` call: record, submit, copy the
//! offscreen image and wait for it, so the loop is paced and the tail is not
//! hidden behind a queue of frames in flight. Absolute values are therefore a
//! few hundred microseconds above an in-game frame; compare runs on the same
//! machine, against each other, with the same build profile.
//!
//! Set `PRISM_BENCH_FRAMES=<n>` to change the frame count (default 120).

mod golden_support;

use std::path::PathBuf;
use std::time::Instant;

use prism_render::frame_stats::{FrameStats, FrameStatsReport, HISTOGRAM_BUCKETS};

/// Offscreen size; small enough to keep CI time low, large enough to exercise
/// the raster passes rather than one clear.
const SIZE: usize = 320;
/// Frames rendered before sampling so pipeline compilation and uploads do not
/// land in the tail.
const WARMUP_FRAMES: usize = 5;

fn frame_count() -> usize {
    std::env::var("PRISM_BENCH_FRAMES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|frames: &usize| *frames > 0)
        .unwrap_or(120)
}

/// `<target>/bench/frame-benchmark.json`, honouring `CARGO_TARGET_DIR`.
fn report_path() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    target.join("bench").join("frame-benchmark.json")
}

#[test]
fn benchmark_writes_a_report_with_percentiles_and_a_histogram() {
    let Some(mut harness) = golden_support::FrameHarness::new(SIZE) else {
        return;
    };
    let options = golden_support::FrameOptions::default();
    for _ in 0..WARMUP_FRAMES {
        let _ = harness.render(options);
    }

    let frames = frame_count();
    let mut stats = FrameStats::new(frames * 2);
    for _ in 0..frames {
        let start = Instant::now();
        let _ = harness.render(options);
        let cpu_ms = start.elapsed().as_secs_f64() * 1000.0;
        stats.push(cpu_ms, harness.gpu_total_ms());
    }

    let summary = stats.summary();
    assert_eq!(summary.frames, frames);
    assert!(
        summary.cpu.mean_ms > 0.0 && summary.cpu.mean_ms.is_finite(),
        "benchmark produced no usable CPU samples: {summary:?}"
    );
    assert!(
        summary.cpu.p50_ms <= summary.cpu.p99_ms && summary.cpu.p99_ms <= summary.cpu.p999_ms,
        "percentiles are not ordered: {summary:?}"
    );
    assert!(
        summary.cpu.low_1pct_fps > 0.0 && summary.cpu.low_1pct_fps.is_finite(),
        "1% low is not finite: {summary:?}"
    );
    assert_eq!(
        summary.cpu.histogram_total(),
        frames as u64,
        "every sample has to land in exactly one histogram bucket"
    );

    let report = FrameStatsReport::new("golden-frame box", SIZE as u32, SIZE as u32, &stats)
        .with_adapter(harness.adapter_info());
    let path = report_path();
    report
        .write_json(&path)
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));

    let text = std::fs::read_to_string(&path).expect("report is readable");
    let value: serde_json::Value = serde_json::from_str(&text).expect("report is JSON");
    assert_eq!(value["format"], "prism-bench-report");
    assert_eq!(value["version"], 1);
    assert_eq!(value["scene"], "golden-frame box");
    assert_eq!(value["stats"]["frames"], frames);
    assert_eq!(value["stats"]["cpu"]["samples"], frames);
    assert_eq!(
        value["stats"]["cpu"]["histogram"].as_array().map(Vec::len),
        Some(HISTOGRAM_BUCKETS)
    );
    assert!(value["adapter"].is_string(), "adapter identity is recorded");

    let gpu = value["stats"]["gpu"]["p99Ms"]
        .as_f64()
        .map(|p99| format!("{p99:.3} ms"))
        .unwrap_or_else(|| "unavailable".to_string());
    eprintln!(
        "frame benchmark ({frames} frames, {SIZE}x{SIZE}): mean {:.3} ms, p50 {:.3} ms, \
         p99 {:.3} ms, 1% low {:.1} fps, GPU p99 {gpu} → {}",
        summary.cpu.mean_ms,
        summary.cpu.p50_ms,
        summary.cpu.p99_ms,
        summary.cpu.low_1pct_fps,
        path.display()
    );
}
