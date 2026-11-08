//! Frame-time benchmark report export (#1002).
//!
//! The HUD shows the rolling percentiles of the last few seconds, which is
//! enough to notice a hitch but not to compare two builds: the first frames
//! carry shader compilation and asset uploads, and a rolling window keeps
//! forgetting old samples. A benchmark run drops a warm-up prefix and keeps a
//! fixed number of samples, then writes one JSON file in the
//! `prism-bench-report` format.
//!
//! - `PRISMREV_BENCH_REPORT=<path>` enables the export and names the file.
//! - `PRISMREV_BENCH_WARMUP=<n>` frames recorded but dropped (default 120).
//! - `PRISMREV_BENCH_FRAMES=<n>` frames sampled before the report is written
//!   (default 600).
//!
//! The report is written once, from the render thread, on the frame that
//! completes the sample set. The process keeps running afterwards, so the run
//! can still be watched or captured; stop it as usual. Missing parent
//! directories are created.

use std::io;
use std::path::{Path, PathBuf};

use prism_render::frame_stats::{BenchMetadata, FrameStats, FrameStatsSummary};

/// Environment variable that names the report file.
pub const REPORT_ENV: &str = "PRISMREV_BENCH_REPORT";
/// Environment variable with the number of warm-up frames to drop.
pub const WARMUP_ENV: &str = "PRISMREV_BENCH_WARMUP";
/// Environment variable with the number of sampled frames.
pub const FRAMES_ENV: &str = "PRISMREV_BENCH_FRAMES";

const DEFAULT_WARMUP: u32 = 120;
const DEFAULT_FRAMES: u32 = 600;

/// What one benchmark run should do, read from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchPlan {
    pub path: PathBuf,
    pub warmup_frames: u32,
    pub frames: u32,
}

impl BenchPlan {
    /// Reads the plan from the environment; `None` when `PRISMREV_BENCH_REPORT`
    /// is unset or empty.
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os(REPORT_ENV).filter(|value| !value.is_empty())?;
        Some(Self {
            path: PathBuf::from(path),
            warmup_frames: frame_count(WARMUP_ENV, DEFAULT_WARMUP),
            frames: frame_count(FRAMES_ENV, DEFAULT_FRAMES),
        })
    }
}

/// Frame budget from `name`, or `default` when it is unset, unparsable or zero.
fn frame_count(name: &str, default: u32) -> u32 {
    match std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
    {
        Some(0) | None => default,
        Some(count) => count,
    }
}

/// Samples frames after the warm-up and knows when the run is complete.
#[derive(Debug, Clone)]
pub struct BenchCollector {
    plan: BenchPlan,
    warmup_left: u32,
    stats: FrameStats,
}

impl BenchCollector {
    pub fn new(plan: BenchPlan) -> Self {
        let stats = FrameStats::new(plan.frames.max(1) as usize);
        Self {
            warmup_left: plan.warmup_frames,
            plan,
            stats,
        }
    }

    pub fn plan(&self) -> &BenchPlan {
        &self.plan
    }

    /// Number of samples collected so far.
    pub fn samples(&self) -> usize {
        self.stats.cpu_samples().len()
    }

    /// True once the sample set is complete.
    pub fn is_complete(&self) -> bool {
        self.samples() >= self.plan.frames.max(1) as usize
    }

    /// Records one frame. Warm-up frames are counted and dropped; frames after
    /// the sample set is complete are ignored, and the method keeps returning
    /// `true` so a caller that polls it can still tell the run finished.
    pub fn sample(&mut self, cpu_ms: f32, gpu_ms: Option<f32>) -> bool {
        if self.warmup_left > 0 {
            self.warmup_left -= 1;
            return false;
        }
        if self.is_complete() {
            return true;
        }
        self.stats.push_frame(cpu_ms, gpu_ms);
        self.is_complete()
    }

    /// Distribution of the collected samples.
    pub fn summary(&self) -> FrameStatsSummary {
        self.stats.summary()
    }

    /// Metadata for the report: adapter, extent and the plan's frame counts.
    pub fn metadata(&self, adapter: Option<String>, extent: Option<(u32, u32)>) -> BenchMetadata {
        BenchMetadata {
            adapter,
            extent,
            warmup_frames: self.plan.warmup_frames,
            scene: None,
        }
    }

    /// Writes the report to the plan's path.
    pub fn write(&self, metadata: &BenchMetadata) -> io::Result<()> {
        write_report(&self.plan.path, &self.summary(), metadata)
    }
}

/// Writes `summary` to `path` through a sibling temporary file, so a reader
/// never sees a half-written report.
pub fn write_report(
    path: &Path,
    summary: &FrameStatsSummary,
    metadata: &BenchMetadata,
) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let json = summary.to_json_with_metadata(metadata);
    let temp = temp_path(path);
    std::fs::write(&temp, json.as_bytes())?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bench-report.json".to_owned());
    path.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "prism-bench-report-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn plan(frames: u32, warmup_frames: u32) -> BenchPlan {
        BenchPlan {
            path: temp_dir().join("report.json"),
            warmup_frames,
            frames,
        }
    }

    #[test]
    fn warmup_frames_are_not_sampled() {
        let mut collector = BenchCollector::new(plan(4, 2));
        assert!(!collector.sample(50.0, None));
        assert!(!collector.sample(50.0, None));
        assert_eq!(collector.samples(), 0);

        for _ in 0..3 {
            assert!(!collector.sample(10.0, None));
        }
        assert_eq!(collector.samples(), 3);
        assert!(
            collector.sample(10.0, None),
            "the fourth sample completes the run"
        );
        assert!(collector.is_complete());
        assert_eq!(collector.summary().cpu.expect("cpu summary").samples, 4);
    }

    #[test]
    fn samples_after_completion_do_not_change_the_report() {
        let mut collector = BenchCollector::new(plan(2, 0));
        assert!(!collector.sample(10.0, None));
        assert!(collector.sample(10.0, None));
        assert!(
            collector.sample(99.0, None),
            "a finished run stays finished"
        );
        let cpu = collector.summary().cpu.expect("cpu summary");
        assert_eq!(cpu.samples, 2);
        assert_eq!(cpu.mean_ms, 10.0);
    }

    #[test]
    fn gpu_samples_stay_optional() {
        let mut collector = BenchCollector::new(plan(2, 0));
        collector.sample(10.0, Some(4.0));
        collector.sample(12.0, None);
        let summary = collector.summary();
        assert_eq!(summary.cpu.expect("cpu").samples, 2);
        assert_eq!(summary.gpu.expect("gpu").samples, 1);
    }

    #[test]
    fn report_is_written_with_metadata_and_parent_directories() {
        let plan = plan(2, 30);
        let collector = BenchCollector::new(plan.clone());
        let metadata = collector.metadata(Some("Test GPU".to_owned()), Some((1280, 720)));
        collector.write(&metadata).expect("write report");

        let json = std::fs::read_to_string(&plan.path).expect("read report");
        assert!(json.contains("\"format\":\"prism-bench-report\""), "{json}");
        assert!(json.contains("\"adapter\":\"Test GPU\""), "{json}");
        assert!(json.contains("\"extent\":\"1280x720\""), "{json}");
        assert!(json.contains("\"warmupFrames\":30"), "{json}");
        // The temporary file is renamed into place, not left behind.
        let leftovers: Vec<_> = std::fs::read_dir(plan.path.parent().expect("parent"))
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temporary files");
    }

    #[test]
    fn an_existing_report_is_replaced() {
        let plan = plan(1, 0);
        std::fs::write(&plan.path, "old").expect("seed file");
        let mut collector = BenchCollector::new(plan.clone());
        collector.sample(10.0, None);
        collector
            .write(&BenchMetadata::default())
            .expect("write report");
        let json = std::fs::read_to_string(&plan.path).expect("read report");
        assert!(
            json.starts_with("{\"format\":\"prism-bench-report\""),
            "{json}"
        );
        assert!(!json.contains("old"), "{json}");
    }
}
