//! `prism-render-graph.json` 的只读 DTO（与 `prism-app/src/render_graph_report.rs` 同形）。
//!
//! 编辑器只解析展示，不依赖 render / app crate。

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const ENV_KEY: &str = "PRISMREV_RENDER_GRAPH_REPORT";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderGraphReport {
    #[serde(default)]
    pub generated_at_ms: u64,
    #[serde(default)]
    pub frame: u64,
    #[serde(default)]
    pub passes: Vec<ReportPass>,
    #[serde(default)]
    pub resources: Vec<ReportResource>,
    /// 上一帧总 GPU 时间（ms）；设备不支持时间戳时缺失。
    #[serde(default)]
    pub gpu_total_ms: Option<f32>,
    /// 纹理 / 网格 / 材质的驻留统计（#518 Phase 2）；旧报告没有该字段。
    #[serde(default)]
    pub stats: Option<ReportStats>,
    /// 本帧的 draw call 列表（#518 Phase 2）；旧报告没有该字段。
    #[serde(default)]
    pub draw_calls: Vec<ReportDrawCall>,
}

/// 一条 draw call（#518 Phase 2 的数据层）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportDrawCall {
    #[serde(default)]
    pub pass: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub instances: usize,
    #[serde(default)]
    pub triangles: u64,
    #[serde(default)]
    pub visible: bool,
}

impl ReportDrawCall {
    /// 面板一行：`pass · label`。
    pub fn summary(&self) -> String {
        format!("{} · {}", self.pass, self.label)
    }
}

/// 管理器侧的实时资源统计（#518 Phase 2）。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportStats {
    #[serde(default)]
    pub textures: usize,
    #[serde(default)]
    pub texture_bytes: u64,
    #[serde(default)]
    pub meshes: usize,
    #[serde(default)]
    pub mesh_bytes: u64,
    #[serde(default)]
    pub materials: usize,
    #[serde(default)]
    pub material_bytes: u64,
}

impl ReportStats {
    pub fn total_bytes(&self) -> u64 {
        self.texture_bytes + self.mesh_bytes + self.material_bytes
    }

    /// 面板显示用的三行（名字, 数量, 字节）；纯函数，方便单测。
    pub fn rows(&self) -> [(&'static str, usize, u64); 3] {
        [
            ("Textures", self.textures, self.texture_bytes),
            ("Meshes", self.meshes, self.mesh_bytes),
            ("Materials", self.materials, self.material_bytes),
        ]
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReportPass {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub inputs: Vec<u32>,
    #[serde(default)]
    pub outputs: Vec<u32>,
    /// 该 pass 上一帧的 GPU 时间（ms，#518）。
    #[serde(default, rename = "gpuMs")]
    pub gpu_ms: Option<f32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportResource {
    #[serde(default)]
    pub handle: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub allocated: bool,
}

impl RenderGraphReport {
    pub fn resource_name(&self, handle: u32) -> String {
        self.resources
            .iter()
            .find(|resource| resource.handle == handle)
            .map(|resource| resource.name.clone())
            .unwrap_or_else(|| format!("resource#{handle}"))
    }

    /// 解析报告文本（面板手里的 JSON 字符串）。
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }

    /// 有 GPU 计时数据时的总时间。
    pub fn gpu_total_ms(&self) -> Option<f32> {
        self.gpu_total_ms
    }

    /// 按 GPU 时间从高到低排的 pass（没有计时数据的排最后）。
    pub fn passes_by_cost(&self) -> Vec<&ReportPass> {
        let mut passes: Vec<&ReportPass> = self.passes.iter().collect();
        passes.sort_by(|a, b| {
            b.gpu_ms
                .unwrap_or(-1.0)
                .total_cmp(&a.gpu_ms.unwrap_or(-1.0))
        });
        passes
    }

    /// 解析报告文件；损坏 / 缺字段时返回 `None`。
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// 报告路径：环境变量优先，其次 `<project>/target/prism-render-graph.json`。
pub fn report_path(root: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(ENV_KEY) {
        return Some(PathBuf::from(path));
    }
    let candidate = root?.join("target/prism-render-graph.json");
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_calls_are_optional_and_summarized() {
        let legacy: RenderGraphReport =
            serde_json::from_str(r#"{"generatedAtMs":1,"frame":1,"passes":[],"resources":[]}"#)
                .unwrap();
        assert!(legacy.draw_calls.is_empty());

        let parsed: RenderGraphReport = serde_json::from_str(
            r#"{"generatedAtMs":1,"frame":2,"passes":[],"resources":[],
                "drawCalls":[
                  {"pass":"forward","label":"mesh #0 · material 2","instances":1,
                   "triangles":12,"visible":true},
                  {"pass":"sprite","label":"SrcAlpha / OneMinusSrcAlpha","instances":4,
                   "triangles":8,"visible":false}
                ]}"#,
        )
        .unwrap();
        assert_eq!(parsed.draw_calls.len(), 2);
        assert_eq!(
            parsed.draw_calls[0].summary(),
            "forward · mesh #0 · material 2"
        );
        assert_eq!(parsed.draw_calls[1].instances, 4);
        assert!(!parsed.draw_calls[1].visible);
    }

    #[test]
    fn resource_stats_are_optional_and_have_rows() {
        // 旧报告没有 stats：整段隐藏，不拿 0 值冒充。
        let legacy: RenderGraphReport =
            serde_json::from_str(r#"{"generatedAtMs":1,"frame":1,"passes":[],"resources":[]}"#)
                .unwrap();
        assert!(legacy.stats.is_none());

        let parsed: RenderGraphReport = serde_json::from_str(
            r#"{"generatedAtMs":1,"frame":2,"passes":[],"resources":[],
                "stats":{"textures":3,"textureBytes":1024,"meshes":2,"meshBytes":2048,
                         "materials":4,"materialBytes":384}}"#,
        )
        .unwrap();
        let stats = parsed.stats.expect("stats");
        assert_eq!(stats.total_bytes(), 3456);
        assert_eq!(
            stats.rows(),
            [
                ("Textures", 3, 1024),
                ("Meshes", 2, 2048),
                ("Materials", 4, 384),
            ]
        );
    }

    #[test]
    fn parses_runtime_report_and_resolves_handles() {
        let report: RenderGraphReport = serde_json::from_str(
            r#"{
                "generatedAtMs": 123,
                "frame": 7,
                "settings": {},
                "passes": [
                    {"index":0,"name":"ShadowMapPass","kind":"shadow","inputs":[1],"outputs":[2]}
                ],
                "resources": [
                    {"handle":1,"name":"scene","kind":"buffer","format":null,"allocated":true},
                    {"handle":2,"name":"shadow_map","kind":"depth","format":"D32_SFLOAT",
                     "width":2048,"height":2048,"sizeBytes":16777216,"allocated":true}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(report.frame, 7);
        assert_eq!(report.passes.len(), 1);
        assert_eq!(report.passes[0].kind, "shadow");
        assert_eq!(report.resource_name(2), "shadow_map");
        assert_eq!(report.resource_name(99), "resource#99");
        assert_eq!(report.resources[1].format.as_deref(), Some("D32_SFLOAT"));
    }

    #[test]
    fn gpu_timings_parse_and_sort_by_cost() {
        let json = r#"{
            "frame": 12,
            "gpuTotalMs": 7.5,
            "passes": [
                {"index":0,"name":"ShadowMapPass","kind":"shadow","gpuMs":1.25},
                {"index":1,"name":"ForwardPass","kind":"forward","gpuMs":5.5},
                {"index":2,"name":"PostPass","kind":"post"}
            ],
            "resources": []
        }"#;
        let report = RenderGraphReport::parse(json).expect("parses");
        assert_eq!(report.gpu_total_ms(), Some(7.5));
        let ordered: Vec<&str> = report
            .passes_by_cost()
            .iter()
            .map(|pass| pass.name.as_str())
            .collect();
        assert_eq!(
            ordered,
            vec!["ForwardPass", "ShadowMapPass", "PostPass"],
            "most expensive first, un-timed passes last"
        );
        assert_eq!(report.passes[0].gpu_ms, Some(1.25));
        assert_eq!(report.passes[2].gpu_ms, None);
    }

    #[test]
    fn a_report_without_timings_has_no_gpu_total() {
        let report =
            RenderGraphReport::parse(r#"{"frame":1,"passes":[],"resources":[]}"#).expect("parses");
        assert_eq!(report.gpu_total_ms(), None);
        assert!(report.passes_by_cost().is_empty());
    }

    #[test]
    fn missing_file_is_none() {
        assert!(RenderGraphReport::load(Path::new("/definitely/missing/report.json")).is_none());
    }
}
