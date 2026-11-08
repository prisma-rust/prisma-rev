//! Frame-time statistics and the exportable benchmark report (#1002).
//!
//! The debug HUD only ever had the last frame time and a smoothed average, so a
//! change that made the tail worse looked identical to one that did not. This
//! module keeps a fixed-capacity ring of CPU and GPU frame times, summarises it
//! with percentiles, the 1% / 0.1% lows and a fixed-bucket histogram, and writes
//! the result as a `prism-bench-report` JSON document.
//!
//! Pure data: no `vk::*` handles and no renderer types, so the same code serves
//! the renderer's own benchmark test today and a future `--bench` entry point in
//! `prism-app`. Same split as [`crate::render_graph_report`].
//!
//! Runs are only comparable within one machine and one report version: the
//! absolute numbers move with the adapter, the driver and the CPU.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Report format tag written into every file.
pub const REPORT_FORMAT: &str = "prism-bench-report";
/// Report schema version. Bump it when a field changes meaning or disappears;
/// readers of a newer version must refuse the file instead of guessing.
pub const REPORT_VERSION: u32 = 1;
/// Frames kept when the caller does not ask for a different capacity
/// (≈2 minutes at 60 fps).
pub const DEFAULT_CAPACITY: usize = 8192;

/// Edges of the fixed histogram buckets, in milliseconds.
///
/// Bucket `i` counts frames in `[EDGE[i], EDGE[i + 1])`; the last bucket counts
/// everything at or above the final edge. Part of the report format: two
/// histograms can only be added or compared when every run used the same edges,
/// so these values must not depend on configuration. Appending an edge or
/// changing one is a report version bump.
pub const HISTOGRAM_BOUNDS_MS: [f64; 24] = [
    0.0, 1.0, 2.0, 3.0, 4.17, 5.0, 6.0, 7.0, 8.33, 10.0, 12.0, 14.0, 16.67, 20.0, 25.0, 33.33,
    40.0, 50.0, 66.67, 100.0, 150.0, 250.0, 500.0, 1000.0,
];

/// Bucket count of the histogram: one per edge, the last being the overflow.
pub const HISTOGRAM_BUCKETS: usize = HISTOGRAM_BOUNDS_MS.len();

/// Percentile of the frame time, in milliseconds, that `fraction` of frames beat.
fn percentile_ms(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

/// Mean of the slowest `fraction` of frames, as a frame rate.
///
/// This is the conventional "1% low": the reciprocal of the average frame time
/// of the worst frames, not the smallest frame rate seen. `fraction` is clamped
/// so at least one frame always counts.
fn low_fps(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let count = ((fraction * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    let slowest = &sorted[sorted.len() - count..];
    let mean = slowest.iter().sum::<f64>() / count as f64;
    if mean > 0.0 {
        1000.0 / mean
    } else {
        0.0
    }
}

/// One series of frame times with a fixed capacity.
///
/// The ring keeps the newest samples and drops the oldest, so a long run does
/// not grow without bound; the summary then describes the most recent window.
#[derive(Debug, Clone)]
pub struct FrameSeries {
    samples: VecDeque<f64>,
    capacity: usize,
}

impl FrameSeries {
    /// A series that keeps at most `capacity` samples (`0` is treated as `1`).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: VecDeque::with_capacity(capacity.min(DEFAULT_CAPACITY)),
            capacity,
        }
    }

    /// Records one frame time in milliseconds. Non-finite and negative values
    /// are ignored: a missing GPU timestamp must not poison the percentiles.
    pub fn push(&mut self, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Aggregated view of the recorded samples.
    pub fn summary(&self) -> SeriesSummary {
        let mut sorted: Vec<f64> = self.samples.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);

        let mut histogram = [0u64; HISTOGRAM_BUCKETS];
        for ms in &sorted {
            // The last edge whose value is `<= ms`; the final bucket collects
            // everything at or above the largest edge.
            let bucket = HISTOGRAM_BOUNDS_MS
                .iter()
                .rposition(|edge| *ms >= *edge)
                .unwrap_or(0);
            histogram[bucket] += 1;
        }

        let mean_ms = if sorted.is_empty() {
            0.0
        } else {
            sorted.iter().sum::<f64>() / sorted.len() as f64
        };

        SeriesSummary {
            samples: sorted.len(),
            mean_ms,
            p50_ms: percentile_ms(&sorted, 0.50),
            p95_ms: percentile_ms(&sorted, 0.95),
            p99_ms: percentile_ms(&sorted, 0.99),
            p999_ms: percentile_ms(&sorted, 0.999),
            low_1pct_fps: low_fps(&sorted, 0.01),
            low_0_1pct_fps: low_fps(&sorted, 0.001),
            histogram,
        }
    }
}

impl Default for FrameSeries {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

/// One series in a report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesSummary {
    /// Samples that went into the numbers below.
    pub samples: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    /// 1% low: frame rate of the slowest 1% of frames.
    pub low_1pct_fps: f64,
    /// 0.1% low: frame rate of the slowest 0.1% of frames.
    pub low_0_1pct_fps: f64,
    /// Frame counts per [`HISTOGRAM_BOUNDS_MS`] bucket; the last entry counts
    /// frames at or above the largest edge.
    pub histogram: [u64; HISTOGRAM_BUCKETS],
}

impl SeriesSummary {
    /// Sum of the histogram buckets, i.e. [`Self::samples`] as counted by the
    /// buckets. Used by the tests and by report readers that only look at the
    /// histogram.
    pub fn histogram_total(&self) -> u64 {
        self.histogram.iter().sum()
    }
}

/// CPU and GPU frame times of one run.
#[derive(Debug, Clone)]
pub struct FrameStats {
    cpu: FrameSeries,
    gpu: FrameSeries,
}

impl FrameStats {
    pub fn new(capacity: usize) -> Self {
        Self {
            cpu: FrameSeries::new(capacity),
            gpu: FrameSeries::new(capacity),
        }
    }

    /// Records one frame. `gpu_ms` is `None` when the device could not report
    /// GPU timestamps (the `gpu` summary is then omitted from the report).
    pub fn push(&mut self, cpu_ms: f64, gpu_ms: Option<f64>) {
        self.cpu.push(cpu_ms);
        if let Some(gpu_ms) = gpu_ms {
            self.gpu.push(gpu_ms);
        }
    }

    pub fn cpu(&self) -> &FrameSeries {
        &self.cpu
    }

    pub fn gpu(&self) -> &FrameSeries {
        &self.gpu
    }

    /// Number of frames recorded, i.e. the CPU sample count.
    pub fn frames(&self) -> usize {
        self.cpu.len()
    }

    pub fn summary(&self) -> FrameStatsSummary {
        FrameStatsSummary {
            frames: self.cpu.len(),
            cpu: self.cpu.summary(),
            gpu: if self.gpu.is_empty() {
                None
            } else {
                Some(self.gpu.summary())
            },
        }
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

/// Both series of a run, as written into the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameStatsSummary {
    pub frames: usize,
    pub cpu: SeriesSummary,
    /// Omitted when the device has no timestamp support.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<SeriesSummary>,
}

/// `prism-bench-report` v1 JSON document.
///
/// Only aggregate values are stored: the raw frame stream stays in memory, so a
/// report stays small enough to commit or attach to an issue.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameStatsReport {
    pub format: String,
    pub version: u32,
    /// Unix milliseconds when the report was generated.
    pub generated_at_ms: u64,
    /// Scene or harness name, so two reports can be told apart.
    pub scene: String,
    pub width: u32,
    pub height: u32,
    /// `adapter_info()` of the device that rendered the frames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    pub os: String,
    pub arch: String,
    /// Logical CPU count; frame times on a busy machine are not comparable.
    pub cores: usize,
    pub stats: FrameStatsSummary,
}

impl FrameStatsReport {
    /// Builds a report from a finished run.
    pub fn new(scene: impl Into<String>, width: u32, height: u32, stats: &FrameStats) -> Self {
        Self {
            format: REPORT_FORMAT.to_string(),
            version: REPORT_VERSION,
            generated_at_ms: now_unix_ms(),
            scene: scene.into(),
            width,
            height,
            adapter: None,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cores: std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1),
            stats: stats.summary(),
        }
    }

    /// Records the device identity, which is what makes two reports' absolute
    /// numbers comparable (or not).
    pub fn with_adapter(mut self, adapter: impl Into<String>) -> Self {
        self.adapter = Some(adapter.into());
        self
    }

    /// Pretty JSON; the same bytes [`Self::write_json`] puts on disk.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|error| {
            // The struct is plain data; serialisation cannot fail in practice.
            format!("{{\"error\":\"{error}\"}}")
        })
    }

    /// Writes the report, creating parent directories.
    ///
    /// The bytes go to a sibling temporary file first and are renamed into
    /// place, so a reader never sees half a JSON document.
    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(values: &[f64]) -> FrameSeries {
        let mut series = FrameSeries::new(values.len().max(1));
        for value in values {
            series.push(*value);
        }
        series
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        // 1..=100 ms: the 50th percentile is the 50th sample, the 99th is 99.
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        let summary = series(&values).summary();
        assert_eq!(summary.samples, 100);
        assert_eq!(summary.p50_ms, 50.0);
        assert_eq!(summary.p95_ms, 95.0);
        assert_eq!(summary.p99_ms, 99.0);
        assert_eq!(summary.p999_ms, 100.0, "0.999 of 100 rounds up to 100");
        assert!((summary.mean_ms - 50.5).abs() < 1e-9);
    }

    #[test]
    fn lows_average_the_slowest_frames() {
        // 99 frames at 10 ms and one at 100 ms: the 1% low is the 100 ms frame.
        let mut values = vec![10.0; 99];
        values.push(100.0);
        let summary = series(&values).summary();
        assert!((summary.low_1pct_fps - 10.0).abs() < 1e-9);

        // Four frames: the slowest 0.1% is still one frame, not zero frames.
        let summary = series(&[5.0, 10.0, 20.0, 40.0]).summary();
        assert!((summary.low_1pct_fps - 25.0).abs() < 1e-9);
        assert!((summary.low_0_1pct_fps - 25.0).abs() < 1e-9);
    }

    #[test]
    fn histogram_buckets_follow_the_fixed_bounds() {
        let mut stats = FrameStats::new(16);
        for ms in [0.5, 1.0, 3.5, 16.67, 16.68, 2000.0] {
            stats.push(ms, None);
        }
        let summary = stats.summary();
        let histogram = summary.cpu.histogram;
        assert_eq!(summary.cpu.histogram_total(), 6);
        assert_eq!(histogram[0], 1, "0.5 ms is in [0, 1)");
        assert_eq!(histogram[1], 1, "1.0 ms is in [1, 2)");
        assert_eq!(histogram[3], 1, "3.5 ms is in [3, 4.17)");
        assert_eq!(
            histogram[12], 2,
            "16.67 and 16.68 ms are both in [16.67, 20)"
        );
        assert_eq!(
            histogram[HISTOGRAM_BUCKETS - 1],
            1,
            "anything at or above 1000 ms lands in the overflow bucket"
        );
        assert_eq!(summary.cpu.samples, 6);
    }

    #[test]
    fn the_ring_keeps_the_newest_samples() {
        let mut stats = FrameStats::new(3);
        for ms in 1..=5 {
            stats.push(f64::from(ms), None);
        }
        assert_eq!(stats.frames(), 3);
        let summary = stats.summary();
        assert_eq!(summary.cpu.samples, 3);
        assert_eq!(summary.cpu.p50_ms, 4.0, "only 3, 4 and 5 ms are kept");
        assert_eq!(summary.cpu.histogram_total(), 3);
    }

    #[test]
    fn non_finite_samples_are_ignored() {
        let mut stats = FrameStats::new(8);
        stats.push(f64::NAN, None);
        stats.push(f64::INFINITY, None);
        stats.push(-1.0, None);
        stats.push(2.0, Some(f32::NAN.into()));
        assert_eq!(stats.frames(), 1);
        let summary = stats.summary();
        assert!(summary.cpu.mean_ms.is_finite());
        assert!(
            summary.gpu.is_none(),
            "a non-finite GPU sample leaves the series empty"
        );
    }

    #[test]
    fn the_report_carries_both_series_and_the_format_tag() {
        let mut stats = FrameStats::new(16);
        for ms in [4.0, 5.0, 6.0, 40.0] {
            stats.push(ms, Some(ms / 2.0));
        }
        let report =
            FrameStatsReport::new("fixed box", 640, 360, &stats).with_adapter("test device");
        let value: serde_json::Value =
            serde_json::from_str(&report.to_json()).expect("report is JSON");

        assert_eq!(value["format"], REPORT_FORMAT);
        assert_eq!(value["version"], REPORT_VERSION);
        assert_eq!(value["scene"], "fixed box");
        assert_eq!(value["width"], 640);
        assert_eq!(value["height"], 360);
        assert_eq!(value["adapter"], "test device");
        assert_eq!(value["stats"]["frames"], 4);
        assert_eq!(value["stats"]["cpu"]["samples"], 4);
        assert_eq!(value["stats"]["gpu"]["samples"], 4);
        assert!(value["stats"]["cpu"]["p99Ms"].as_f64().unwrap().is_finite());
        assert!(value["generatedAtMs"].as_u64().unwrap() > 0);
        assert_eq!(
            value["stats"]["cpu"]["histogram"].as_array().unwrap().len(),
            HISTOGRAM_BUCKETS
        );
    }

    #[test]
    fn a_report_without_gpu_timestamps_omits_the_gpu_series() {
        let mut stats = FrameStats::new(8);
        stats.push(8.0, None);
        let report = FrameStatsReport::new("cpu only", 320, 200, &stats);
        let value: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        assert!(value["stats"]["gpu"].is_null());
        assert!(value["adapter"].is_null());
    }

    #[test]
    fn writing_the_report_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = std::env::temp_dir().join(format!("prism-bench-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/benchmark.json");

        let mut stats = FrameStats::new(8);
        stats.push(16.0, None);
        let report = FrameStatsReport::new("write", 64, 64, &stats);
        report.write_json(&path).expect("write");

        let first = std::fs::read_to_string(&path).expect("written file");
        assert!(first.contains(REPORT_FORMAT));
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temporary file is renamed into place"
        );

        // Writing again overwrites in place instead of appending.
        let second = FrameStatsReport::new("write", 64, 64, &stats);
        second.write_json(&path).expect("rewrite");
        let text = std::fs::read_to_string(&path).expect("rewritten file");
        assert_eq!(text.matches(REPORT_FORMAT).count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
