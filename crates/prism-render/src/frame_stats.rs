//! Frame-time distribution (CPU interval and GPU total) for the runtime HUD
//! and for saving a comparable benchmark report (#1002).
//!
//! The renderer already tracks the last frame time and a smoothed FPS, which
//! answers "how long was this frame" but not "did this change make the tail
//! worse". This module keeps a fixed ring buffer per series and derives
//! percentiles, 1% / 0.1% low FPS, and a fixed-bucket histogram.
//!
//! The histogram edges are part of the report format: they never change with
//! configuration, so histograms from different runs can be added together and
//! compared. Bump [`BENCH_REPORT_VERSION`] instead of editing the table.

use std::collections::VecDeque;

/// Report format version written by [`FrameStatsSummary::to_json`].
pub const BENCH_REPORT_VERSION: u32 = 1;

/// Report format name written by [`FrameStatsSummary::to_json`].
pub const BENCH_REPORT_FORMAT: &str = "prism-bench-report";

/// Upper bounds of the frame-time buckets, in milliseconds.
///
/// A sample goes into the first bucket whose upper bound is greater than the
/// sample, so there is always one extra bucket for anything above the last
/// edge. The values are the common refresh-rate / hitch thresholds, not a
/// configuration knob.
pub const FRAME_HISTOGRAM_EDGES_MS: [f32; 24] = [
    0.0, 1.0, 2.0, 3.0, 4.17, 5.0, 6.0, 7.0, 8.33, 10.0, 12.0, 14.0, 16.67, 20.0, 25.0, 33.33,
    40.0, 50.0, 66.67, 100.0, 150.0, 250.0, 500.0, 1000.0,
];

/// Number of histogram buckets: one per edge plus the overflow bucket.
pub const FRAME_HISTOGRAM_BUCKETS: usize = FRAME_HISTOGRAM_EDGES_MS.len() + 1;

/// Ring buffers for CPU frame intervals and GPU frame totals.
///
/// Both series share one capacity; GPU samples are only pushed when the device
/// reports timings, so the two sample counts can differ. Non-finite and
/// negative samples are dropped instead of poisoning the percentiles.
#[derive(Debug, Clone)]
pub struct FrameStats {
    capacity: usize,
    cpu_ms: VecDeque<f32>,
    gpu_ms: VecDeque<f32>,
}

impl FrameStats {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            cpu_ms: VecDeque::new(),
            gpu_ms: VecDeque::new(),
        }
    }

    /// Records one frame. `gpu_ms` is `None` when the device has no timestamps.
    pub fn push_frame(&mut self, cpu_ms: f32, gpu_ms: Option<f32>) {
        push_sample(&mut self.cpu_ms, self.capacity, cpu_ms);
        if let Some(gpu_ms) = gpu_ms {
            push_sample(&mut self.gpu_ms, self.capacity, gpu_ms);
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn cpu_samples(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
        self.cpu_ms.iter().copied()
    }

    pub fn gpu_samples(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
        self.gpu_ms.iter().copied()
    }

    /// Derives the percentiles and histogram of both series.
    ///
    /// This sorts the samples, so a caller that runs it every frame pays for a
    /// sort every frame; refresh it periodically (the render thread does it a
    /// few times per second) and reuse the result.
    pub fn summary(&self) -> FrameStatsSummary {
        FrameStatsSummary {
            cpu: series_summary(&self.cpu_ms),
            gpu: series_summary(&self.gpu_ms),
        }
    }
}

fn push_sample(samples: &mut VecDeque<f32>, capacity: usize, ms: f32) {
    if !ms.is_finite() || ms < 0.0 {
        return;
    }
    if samples.len() == capacity {
        samples.pop_front();
    }
    samples.push_back(ms);
}

/// Distribution of one frame-time series.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeriesSummary {
    pub samples: usize,
    pub mean_ms: f32,
    pub p50_ms: f32,
    pub p95_ms: f32,
    pub p99_ms: f32,
    pub p999_ms: f32,
    /// FPS of the slowest 1% of frames (mean of their frame times).
    pub low_1pct_fps: f32,
    /// FPS of the slowest 0.1% of frames.
    pub low_0_1pct_fps: f32,
    /// One count per [`FRAME_HISTOGRAM_EDGES_MS`] edge, plus the overflow bucket.
    pub histogram: [u32; FRAME_HISTOGRAM_BUCKETS],
}

/// Both series of one report. The fields stay separate instead of being
/// merged into a single number.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameStatsSummary {
    pub cpu: Option<SeriesSummary>,
    pub gpu: Option<SeriesSummary>,
}

impl FrameStatsSummary {
    /// Minimal report body: format/version plus both series and the fixed
    /// histogram edges. Device and scene metadata belong to the caller.
    pub fn to_json(&self) -> String {
        let mut json = format!(
            "{{\"format\":\"{BENCH_REPORT_FORMAT}\",\"version\":{BENCH_REPORT_VERSION},\"histogramEdgesMs\":["
        );
        for (i, edge) in FRAME_HISTOGRAM_EDGES_MS.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!("{edge}"));
        }
        json.push_str("],\"cpu\":");
        match &self.cpu {
            Some(cpu) => json.push_str(&cpu.to_json()),
            None => json.push_str("null"),
        }
        json.push_str(",\"gpu\":");
        match &self.gpu {
            Some(gpu) => json.push_str(&gpu.to_json()),
            None => json.push_str("null"),
        }
        json.push('}');
        json
    }
}

impl SeriesSummary {
    pub fn to_json(&self) -> String {
        let mut histogram = String::from("[");
        for (i, count) in self.histogram.iter().enumerate() {
            if i > 0 {
                histogram.push(',');
            }
            histogram.push_str(&count.to_string());
        }
        histogram.push(']');
        format!(
            "{{\"samples\":{},\"meanMs\":{:.3},\"p50Ms\":{:.3},\"p95Ms\":{:.3},\"p99Ms\":{:.3},\"p999Ms\":{:.3},\"low1PctFps\":{:.1},\"low0_1PctFps\":{:.1},\"histogram\":{histogram}}}",
            self.samples,
            self.mean_ms,
            self.p50_ms,
            self.p95_ms,
            self.p99_ms,
            self.p999_ms,
            self.low_1pct_fps,
            self.low_0_1pct_fps,
        )
    }
}

fn series_summary(samples: &VecDeque<f32>) -> Option<SeriesSummary> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<f32> = samples.iter().copied().collect();
    sorted.sort_by(f32::total_cmp);

    let mut histogram = [0u32; FRAME_HISTOGRAM_BUCKETS];
    for sample in samples {
        let bucket = FRAME_HISTOGRAM_EDGES_MS
            .iter()
            .position(|edge| *sample < *edge)
            .unwrap_or(FRAME_HISTOGRAM_BUCKETS - 1);
        histogram[bucket] += 1;
    }

    let mean_ms = sorted.iter().sum::<f32>() / sorted.len() as f32;
    Some(SeriesSummary {
        samples: sorted.len(),
        mean_ms,
        p50_ms: percentile_ms(&sorted, 0.50),
        p95_ms: percentile_ms(&sorted, 0.95),
        p99_ms: percentile_ms(&sorted, 0.99),
        p999_ms: percentile_ms(&sorted, 0.999),
        low_1pct_fps: low_fps(&sorted, 0.01),
        low_0_1pct_fps: low_fps(&sorted, 0.001),
        histogram,
    })
}

/// Nearest-rank percentile of an ascending slice.
fn percentile_ms(sorted: &[f32], fraction: f32) -> f32 {
    let rank = (fraction * sorted.len() as f32).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// FPS of the slowest `fraction` of frames: `1000 / mean(slowest frame times)`.
fn low_fps(sorted: &[f32], fraction: f32) -> f32 {
    let count = ((sorted.len() as f32 * fraction).ceil() as usize)
        .max(1)
        .min(sorted.len());
    let slowest = &sorted[sorted.len() - count..];
    let mean_ms = slowest.iter().sum::<f32>() / count as f32;
    if mean_ms > 0.0 {
        1000.0 / mean_ms
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_with(samples: &[f32]) -> FrameStats {
        let mut stats = FrameStats::new(samples.len().max(1));
        for sample in samples {
            stats.push_frame(*sample, None);
        }
        stats
    }

    #[test]
    fn percentiles_follow_nearest_rank() {
        let stats = stats_with(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let cpu = stats.summary().cpu.expect("cpu summary");
        assert_eq!(cpu.samples, 5);
        assert_eq!(cpu.mean_ms, 3.0);
        assert_eq!(cpu.p50_ms, 3.0);
        // ceil(0.95 * 5) = 5 -> the largest sample.
        assert_eq!(cpu.p95_ms, 5.0);
        assert_eq!(cpu.p99_ms, 5.0);
        assert_eq!(cpu.p999_ms, 5.0);
    }

    #[test]
    fn low_fps_uses_the_slowest_frames() {
        // 99 fast frames and one 100 ms hitch.
        let mut samples = vec![1.0f32; 99];
        samples.push(100.0);
        let stats = stats_with(&samples);
        let cpu = stats.summary().cpu.expect("cpu summary");
        // ceil(0.01 * 100) = 1 -> only the hitch.
        assert!((cpu.low_1pct_fps - 10.0).abs() < 1e-4);
        // 0.1% of 100 frames still rounds up to one frame.
        assert!((cpu.low_0_1pct_fps - 10.0).abs() < 1e-4);
    }

    #[test]
    fn histogram_lands_on_the_fixed_buckets() {
        let stats = stats_with(&[0.5, 16.0, 16.67, 2000.0]);
        let cpu = stats.summary().cpu.expect("cpu summary");
        assert_eq!(cpu.histogram.iter().sum::<u32>(), 4);
        // 0.5 -> bucket of edge 1.0 (index 1); 16.0 -> edge 16.67 (index 12);
        // 16.67 is not < 16.67 -> edge 20 (index 13); 2000 -> overflow.
        assert_eq!(cpu.histogram[1], 1);
        assert_eq!(cpu.histogram[12], 1);
        assert_eq!(cpu.histogram[13], 1);
        assert_eq!(cpu.histogram[FRAME_HISTOGRAM_BUCKETS - 1], 1);
    }

    #[test]
    fn the_ring_buffer_keeps_only_the_newest_samples() {
        let mut stats = FrameStats::new(3);
        for ms in [1.0, 2.0, 3.0, 4.0] {
            stats.push_frame(ms, None);
        }
        let samples: Vec<f32> = stats.cpu_samples().collect();
        assert_eq!(samples, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn gpu_samples_are_optional_and_tracked_separately() {
        let mut stats = FrameStats::new(4);
        stats.push_frame(16.0, Some(8.0));
        stats.push_frame(17.0, None);
        stats.push_frame(18.0, Some(9.0));

        let summary = stats.summary();
        assert_eq!(summary.cpu.expect("cpu").samples, 3);
        assert_eq!(summary.gpu.expect("gpu").samples, 2);
    }

    #[test]
    fn invalid_samples_are_ignored() {
        let mut stats = FrameStats::new(8);
        stats.push_frame(f32::NAN, Some(f32::INFINITY));
        stats.push_frame(-1.0, Some(-2.0));
        assert!(stats.summary().cpu.is_none());
        assert!(stats.summary().gpu.is_none());
    }

    #[test]
    fn json_carries_the_format_and_both_series() {
        let mut stats = FrameStats::new(8);
        stats.push_frame(16.0, Some(4.0));
        let json = stats.summary().to_json();
        assert!(json.contains("\"format\":\"prism-bench-report\""), "{json}");
        assert!(json.contains("\"version\":1"), "{json}");
        assert!(json.contains("\"cpu\":{"), "{json}");
        assert!(json.contains("\"gpu\":{"), "{json}");
        assert_eq!(
            json.matches("\"histogram\":[").count(),
            2,
            "both series carry a histogram: {json}"
        );
    }

    #[test]
    fn json_reports_missing_series_as_null() {
        let empty = FrameStats::new(8).summary().to_json();
        assert!(empty.contains("\"cpu\":null"), "{empty}");
        assert!(empty.contains("\"gpu\":null"), "{empty}");
    }
}
