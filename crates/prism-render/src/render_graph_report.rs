//! 渲染图报告（JSON）——把运行时 `RenderGraphSnapshot` 导出成**纯数据**，
//! 供编辑器 / 外部工具可视化成 DAG。
//!
//! ```text
//! 渲染线程 GraphRenderer ──snapshot()──► RenderGraphReport ──JSON──► 编辑器面板
//! ```
//!
//! 这里只有 DTO 和序列化：**不出现** `vk::*` 句柄、不依赖 ECS / 引擎设置类型。
//! 引擎的 `RenderSettings` 到 [`ReportSettings`] 的映射放在 `prism-engine`
//! （`RenderSettings::to_report_settings`），所以本模块对 `prism-app` 和
//! `editor` 都是同一个实现。
//!
//! 写文件采用「临时文件 + rename」，避免读者读到半截 JSON。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::render_graph::{PassInfo, PassKind, RenderGraphSnapshot, RenderMode, ResourceHandle};

/// 游戏进程写报告的输出路径环境变量键（编辑器自己写本地文件，不看它）。
pub const ENV_KEY: &str = "PRISMREV_RENDER_GRAPH_REPORT";

/// 每多少帧刷新一次报告（≈1 秒 @60fps；图结构变化不频繁）。
pub const REFRESH_INTERVAL_FRAMES: u64 = 60;

/// 管理器侧的实时资源统计（#518 Phase 2）。
///
/// 与 `resources` 不同：那是渲染图自己分配的目标；这里数的是纹理 / 网格 / 材质的
/// **驻留量**（整个渲染器生命周期内累积，热重载后会回落）。字节数是估算值：
/// 纹理只算 base mip（未乘 mip 链），网格按标准顶点布局 + 4 字节索引。
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportStats {
    pub textures: usize,
    pub texture_bytes: u64,
    pub meshes: usize,
    pub mesh_bytes: u64,
    pub materials: usize,
    pub material_bytes: u64,
}

impl ReportStats {
    /// 三类资源合计的估算字节数。
    pub fn total_bytes(&self) -> u64 {
        self.texture_bytes + self.mesh_bytes + self.material_bytes
    }
}

/// `RenderGraphSnapshot` 的 JSON 形状。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderGraphReport {
    /// Unix 毫秒时间戳。
    pub generated_at_ms: u64,
    /// 采样时的帧序号。
    pub frame: u64,
    /// 采样时的渲染设置。
    pub settings: ReportSettings,
    /// 按执行顺序排列的 pass。
    pub passes: Vec<ReportPass>,
    /// 图管理的资源。
    pub resources: Vec<ReportResource>,
    /// 上一帧的总 GPU 时间（ms，#518）。设备不支持时间戳时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_total_ms: Option<f32>,
    /// 纹理 / 网格 / 材质的实时驻留统计（#518 Phase 2）；旧报告没有该字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<ReportStats>,
}

/// 报告里的渲染设置子集（与编辑器 `RenderSettingsData` 对齐）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportSettings {
    pub render_mode: &'static str,
    pub debug_flags: u32,
    pub tonemap_mode: u32,
    pub debug_rt: u32,
    pub pt_max_bounces: u32,
    pub pt_ray_max_distance: f32,
    pub pt_max_iterations: u32,
}

impl ReportSettings {
    /// 从原始字段构造（`prism-engine` 的 `RenderSettings` 用它填表）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        render_mode: RenderMode,
        debug_flags: u32,
        tonemap_mode: u32,
        debug_rt: u32,
        pt_max_bounces: u32,
        pt_ray_max_distance: f32,
        pt_max_iterations: u32,
    ) -> Self {
        Self {
            render_mode: match render_mode {
                RenderMode::Raster => "raster",
                RenderMode::PathTrace => "path_trace",
            },
            debug_flags,
            tonemap_mode,
            debug_rt,
            pt_max_bounces,
            pt_ray_max_distance,
            pt_max_iterations,
        }
    }
}

impl Default for ReportSettings {
    fn default() -> Self {
        Self::new(RenderMode::Raster, 0, 0, 0, 3, 1000.0, 0)
    }
}

/// 一个 pass 的节点数据。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportPass {
    pub index: usize,
    pub name: String,
    /// `shadow` / `forward` / `gtao` / `post` / `pt` / `rt` / `unknown`。
    pub kind: &'static str,
    pub inputs: Vec<ReportHandle>,
    pub outputs: Vec<ReportHandle>,
    /// 该 pass 上一帧的 GPU 时间（ms，#518）；没有计时数据时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_ms: Option<f32>,
}

/// 一个资源的节点数据。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportResource {
    pub handle: u32,
    /// 可读名字（well-known handle 有名字；其余为 `resource#N`）。
    pub name: String,
    /// `color` / `depth` / `storage_image` / `storage_buffer` / `other`。
    pub kind: &'static str,
    /// 例如 `R16G16B16A16_SFLOAT`，可空。
    pub format: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_count: Option<u32>,
    pub size_bytes: Option<u64>,
    pub allocated: bool,
}

/// 边引用：handle 本身（前端用 handle 连接 pass）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
pub struct ReportHandle(pub u32);

impl RenderGraphReport {
    /// 从快照构建报告。`settings` 是每帧实际生效的渲染设置，
    /// `frame` 为调用方帧计数。
    pub fn from_snapshot(
        snapshot: &RenderGraphSnapshot,
        settings: ReportSettings,
        frame: u64,
    ) -> Self {
        Self::from_snapshot_with_timings(snapshot, settings, frame, None)
    }

    /// 同 [`Self::from_snapshot`]，但带上 per-pass GPU 时间（#518）。
    pub fn from_snapshot_with_timings(
        snapshot: &RenderGraphSnapshot,
        settings: ReportSettings,
        frame: u64,
        timings: Option<&crate::gpu_timer::GpuTimings>,
    ) -> Self {
        let passes = snapshot
            .passes
            .iter()
            .map(|pass| ReportPass {
                index: pass.index,
                name: pass.name.clone(),
                kind: pass_kind(pass),
                gpu_ms: timings.and_then(|timings| timings.pass_ms(&pass.name)),
                inputs: visible_inputs(pass, &settings)
                    .into_iter()
                    .map(|h| ReportHandle(h.0))
                    .collect(),
                outputs: pass.outputs.iter().map(|h| ReportHandle(h.0)).collect(),
            })
            .collect();

        let resources = snapshot
            .resources
            .iter()
            .map(|res| {
                let (kind, format, width, height, sample_count, size_bytes) = match &res.res_type {
                    crate::render_graph::ResourceType::ColorAttachment {
                        format,
                        extent,
                        sample_count,
                    } => (
                        "color",
                        Some(format!("{format:?}")),
                        Some(extent.width),
                        Some(extent.height),
                        Some(sample_count.as_raw()),
                        None,
                    ),
                    crate::render_graph::ResourceType::DepthAttachment {
                        extent,
                        sample_count,
                    } => (
                        "depth",
                        None,
                        Some(extent.width),
                        Some(extent.height),
                        Some(sample_count.as_raw()),
                        None,
                    ),
                    crate::render_graph::ResourceType::StorageImage { format, extent } => (
                        "storage_image",
                        Some(format!("{format:?}")),
                        Some(extent.width),
                        Some(extent.height),
                        None,
                        None,
                    ),
                    crate::render_graph::ResourceType::StorageBuffer { size } => {
                        ("storage_buffer", None, None, None, None, Some(*size))
                    }
                };
                ReportResource {
                    handle: res.handle.0,
                    name: resource_name(res.handle),
                    kind,
                    format,
                    width,
                    height,
                    sample_count,
                    size_bytes,
                    allocated: res.allocated,
                }
            })
            .collect();

        Self {
            generated_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            frame,
            settings,
            passes,
            resources,
            gpu_total_ms: timings
                .map(|timings| timings.total_ms)
                .filter(|total| *total > 0.0),
            stats: None,
        }
    }

    /// 附带管理器侧的资源统计（#518 Phase 2）。
    pub fn with_stats(mut self, stats: ReportStats) -> Self {
        self.stats = Some(stats);
        self
    }

    /// 原子写入 JSON（临时文件 + rename）。
    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

/// 报告节流：仅当环境变量存在且到达刷新间隔时写文件。
///
/// 游戏进程用 `from_env()`；编辑器自己决定输出路径（面板要读本地文件）。
pub struct ReportWriter {
    path: Option<std::path::PathBuf>,
    last_frame: Option<u64>,
}

impl ReportWriter {
    /// 读取环境变量；未设置时返回一个永远不写的 writer。
    pub fn from_env() -> Self {
        Self {
            path: std::env::var_os(ENV_KEY).map(std::path::PathBuf::from),
            last_frame: None,
        }
    }

    /// 直接指定输出路径（编辑器用）。
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            last_frame: None,
        }
    }

    /// 关掉写入（编辑器还没有工程根时）。
    pub fn disabled() -> Self {
        Self {
            path: None,
            last_frame: None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// 首帧 + 每 [`REFRESH_INTERVAL_FRAMES`] 帧写一次。
    pub fn maybe_write(
        &mut self,
        snapshot: &RenderGraphSnapshot,
        settings: ReportSettings,
        frame: u64,
    ) {
        self.maybe_write_with_timings(snapshot, settings, frame, None, None);
    }

    /// 同 [`Self::maybe_write`]，附带 per-pass GPU 时间（#518）。
    pub fn maybe_write_with_timings(
        &mut self,
        snapshot: &RenderGraphSnapshot,
        settings: ReportSettings,
        frame: u64,
        timings: Option<&crate::gpu_timer::GpuTimings>,
        stats: Option<ReportStats>,
    ) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let due = match self.last_frame {
            None => true,
            Some(last) => frame.saturating_sub(last) >= REFRESH_INTERVAL_FRAMES,
        };
        if !due {
            return;
        }
        self.last_frame = Some(frame);

        let mut report =
            RenderGraphReport::from_snapshot_with_timings(snapshot, settings, frame, timings);
        if let Some(stats) = stats {
            report = report.with_stats(stats);
        }
        if let Err(error) = report.write_json(path) {
            log::warn!(
                "render graph report write failed ({}): {error}",
                path.display()
            );
        }
    }
}

fn pass_kind(pass: &PassInfo) -> &'static str {
    match pass.kind {
        PassKind::Shadow => "shadow",
        PassKind::Forward => "forward",
        PassKind::Gtao => "gtao",
        PassKind::Post => "post",
        PassKind::Pt => "pt",
        PassKind::Rt => "rt",
        PassKind::Unknown => "unknown",
    }
}

/// 报告里展示的 pass 输入。
///
/// `PostPass` 的 `graph_info` 只声明光栅化输入（`FORWARD_COLOR_H`），但
/// 路径追踪模式下实际读取的是 `PT_COLOR_H`。图结构本身由 pass 声明，
/// 这里按当前渲染模式修正可视化边。
fn visible_inputs(pass: &PassInfo, settings: &ReportSettings) -> Vec<ResourceHandle> {
    if pass.kind == PassKind::Post && settings.render_mode == "path_trace" {
        vec![crate::render_graph::PT_COLOR_H]
    } else {
        pass.inputs.clone()
    }
}

fn resource_name(handle: ResourceHandle) -> String {
    match handle.0 {
        handle if handle == crate::render_graph::FORWARD_DEPTH_H.0 => "forward.depth".into(),
        handle if handle == crate::render_graph::FORWARD_NORMAL_H.0 => "forward.normal".into(),
        handle if handle == crate::render_graph::FORWARD_COLOR_H.0 => "forward.color".into(),
        handle if handle == crate::render_graph::PT_COLOR_H.0 => "pt.color".into(),
        handle => format!("resource#{handle}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> RenderGraphSnapshot {
        RenderGraphSnapshot {
            passes: vec![
                PassInfo {
                    index: 0,
                    name: "Forward".into(),
                    kind: PassKind::Forward,
                    inputs: vec![],
                    outputs: vec![crate::render_graph::FORWARD_COLOR_H],
                },
                PassInfo {
                    index: 1,
                    name: "Post".into(),
                    kind: PassKind::Post,
                    inputs: vec![crate::render_graph::FORWARD_COLOR_H],
                    outputs: vec![],
                },
            ],
            resources: vec![],
            settings: Default::default(),
        }
    }

    #[test]
    fn report_serializes_edges_and_settings() {
        let report = RenderGraphReport::from_snapshot(&snapshot(), ReportSettings::default(), 42);
        assert_eq!(report.passes.len(), 2);
        assert_eq!(report.passes[1].inputs[0].0, 1002);
        assert_eq!(report.settings.render_mode, "raster");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["passes"][0]["kind"], "forward");
    }

    #[test]
    fn resource_stats_are_optional_and_serialize_in_camel_case() {
        let plain = RenderGraphReport::from_snapshot(&snapshot(), ReportSettings::default(), 1);
        assert!(plain.stats.is_none());
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("stats").is_none(), "旧报告不该凭空多出 stats");

        let stats = ReportStats {
            textures: 3,
            texture_bytes: 1024,
            meshes: 2,
            mesh_bytes: 2048,
            materials: 4,
            material_bytes: 384,
        };
        assert_eq!(stats.total_bytes(), 3456);
        let report = RenderGraphReport::from_snapshot(&snapshot(), ReportSettings::default(), 1)
            .with_stats(stats);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["stats"]["textures"], 3);
        assert_eq!(json["stats"]["textureBytes"], 1024);
        assert_eq!(json["stats"]["materialBytes"], 384);
    }

    #[test]
    fn path_trace_mode_rewires_the_post_pass_input() {
        let settings = ReportSettings::new(RenderMode::PathTrace, 0, 0, 0, 3, 1000.0, 0);
        let report = RenderGraphReport::from_snapshot(&snapshot(), settings, 7);
        assert_eq!(report.passes[1].inputs[0].0, 1003);
        assert_eq!(report.settings.render_mode, "path_trace");
    }

    #[test]
    fn well_known_resources_get_names() {
        assert_eq!(
            resource_name(crate::render_graph::FORWARD_COLOR_H),
            "forward.color"
        );
        assert_eq!(resource_name(ResourceHandle(7)), "resource#7");
    }

    #[test]
    fn writer_writes_only_every_interval() {
        let dir = std::env::temp_dir().join("prism-render-graph-report");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("graph.json");
        let mut writer = ReportWriter::new(path.clone());
        let snap = snapshot();

        writer.maybe_write(&snap, ReportSettings::default(), 1);
        assert!(path.is_file());
        let first = std::fs::metadata(&path).unwrap().len();

        writer.maybe_write(&snap, ReportSettings::default(), 30);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), first);
        writer.maybe_write(
            &snap,
            ReportSettings::default(),
            REFRESH_INTERVAL_FRAMES + 1,
        );
        assert!(path.is_file());

        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed["frame"], REFRESH_INTERVAL_FRAMES + 1);
    }

    #[test]
    fn a_disabled_writer_never_touches_disk() {
        let dir = std::env::temp_dir().join("prism-render-graph-report-off");
        let _ = std::fs::remove_dir_all(&dir);
        let mut writer = ReportWriter::disabled();
        writer.maybe_write(&snapshot(), ReportSettings::default(), 1);
        assert!(!dir.exists());
        assert!(!writer.is_enabled());
    }
}
