//! `Build` / `Build.Profile` 形状的投影。
//!
//! 引擎的构建由 `prism-build-pipeline`（`build-scenes`）+ `cargo build` 完成，
//! 编辑器只是把「构建哪个配置、哪些场景、输出到哪」整理成声明式数据。这一层
//! 把现有 `assets/settings.ini` 的 `[build]` 段与 `assets/scenes.json` 投影成
//! 引擎的形状，供属性面板 / 未来的原生宿主读取。
//!
//! | | 这里 |
//! |---|---|
//! | 编辑器接口 | [`BuildProfile`] |
//! | `BuildPlayerOptions.scenes` / `EditorBuildSettings.scenes` | [`BuildScene`]（`build_index` = 实际构建顺序） |
//! | `BuildTargetGroup` | [`BuildProfile::target_group`]（由 `[build].target` 推导） |
//! | `BuildOptions.Development` / `AllowDebugging` | [`BuildOptions`]（由 `[build].profile == "debug"` 推导） |
//!
//! **不新增构建目标**：`[build].target` 目前只允许 `windows`（见
//! [`crate::settings`] 的校验）；这里只做 命名的投影，不替引擎承诺
//! 尚未实现的平台。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::play::{read_scene_manifest, SceneListEntry};
use crate::settings::{read_project_settings, BuildSettingsData};

/// 场景在构建里的位置 = `EditorBuildSettings.scenes` 的数组下标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildScene {
    /// 场景名（`scenes.json` 的 `name`）。
    pub name: String,
    /// 作者路径（相对项目根，`/` 分隔）。
    pub path: String,
    /// 是否参与构建（未勾选的场景仍保留在列表里）。
    pub enabled: bool,
    /// 实际构建顺序（只数启用项，从 0 开始）；未启用时为 `None`。
    pub build_index: Option<usize>,
}

/// 路径是否是可构建/可打包为场景包的格式（`.scene.json` 或 `.rscn`）。
pub fn is_cookable_scene_path(path: &str) -> bool {
    let lower = path.trim().to_ascii_lowercase();
    lower.ends_with(".scene.json") || lower.ends_with(".rscn")
}

impl BuildScene {
    /// 该条目的路径是否为支持的场景格式。
    pub fn is_cookable(&self) -> bool {
        is_cookable_scene_path(&self.path)
    }
}

/// `BuildOptions`（只投影引擎实际有语义的两位）。
///
/// PrismaRev 没有 profile 级别的任意 flag，因此两位都由
/// `[build].profile == "debug"` 推导：debug 配置 = 开发构建 + 允许调试。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildOptions {
    /// `BuildOptions.Development`。
    pub development: bool,
    /// `BuildOptions.AllowDebugging`。
    pub allow_debugging: bool,
    /// `BuildOptions.ConnectWithProfiler`（引擎暂无性能分析器，恒为 `false`）。
    pub connect_with_profiler: bool,
}

impl BuildOptions {
    /// 由 `[build].profile`（`debug` / `release`）推导。
    pub fn from_profile(profile: &str) -> Self {
        let development = profile.eq_ignore_ascii_case("debug");
        Self {
            development,
            allow_debugging: development,
            connect_with_profiler: false,
        }
    }
}

/// 一次构建的完整描述 = `Build.Profile.BuildProfile`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildProfile {
    /// 配置名（`[build].profile`：`debug` / `release`）。
    pub name: String,
    /// 构建目标（`[build].target`，当前只有 `windows`）。
    pub target: String,
    /// `BuildTargetGroup` 名字（`Standalone` / `Android` / …）。
    pub target_group: &'static str,
    /// 由配置名推导的 `BuildOptions`。
    pub options: BuildOptions,
    /// 输出目录（相对项目根）。
    pub output_dir: String,
    /// 构建完成后是否直接运行（编辑器选项，对应 Build Settings 的勾选）。
    pub run_after: bool,
    /// 场景列表（声明顺序；`build_index` 给出实际构建顺序）。
    pub scenes: Vec<BuildScene>,
}

/// `[build].target` → `BuildTargetGroup` 名字。
///
/// 未知目标返回 `Unknown`（而不是猜一个最接近的组）。
pub fn target_group(target: &str) -> &'static str {
    match target.trim().to_ascii_lowercase().as_str() {
        "windows" | "linux" | "macos" => "Standalone",
        "android" => "Android",
        "ios" => "iOS",
        "web" | "webgl" | "wasm" => "WebGL",
        _ => "Unknown",
    }
}

impl BuildProfile {
    /// 由 `[build]` 段 + 场景列表构造。启用项按声明顺序拿到连续的
    /// `build_index`（0..n），未启用项为 `None`。
    pub fn new(settings: &BuildSettingsData, scenes: &[SceneListEntry]) -> Self {
        let mut next_index = 0;
        let scenes = scenes
            .iter()
            .map(|entry| {
                let build_index = if entry.enabled {
                    let index = next_index;
                    next_index += 1;
                    Some(index)
                } else {
                    None
                };
                BuildScene {
                    name: entry.name.clone(),
                    path: entry.path.clone(),
                    enabled: entry.enabled,
                    build_index,
                }
            })
            .collect();

        Self {
            name: settings.profile.clone(),
            target: settings.target.clone(),
            target_group: target_group(&settings.target),
            options: BuildOptions::from_profile(&settings.profile),
            output_dir: settings.output_dir.clone(),
            run_after: settings.run_after,
            scenes,
        }
    }

    /// 实际构建顺序 = 只取启用场景，顺序即 `build_index`。
    pub fn build_scene_order(&self) -> Vec<&BuildScene> {
        self.scenes.iter().filter(|scene| scene.enabled).collect()
    }

    /// 启用的场景数（= 构建索引的范围）。
    pub fn enabled_scene_count(&self) -> usize {
        self.scenes.iter().filter(|scene| scene.enabled).count()
    }

    /// 启用的可构建场景数（对应 `build-scenes` 最终能打包的场景）。
    pub fn cookable_scene_count(&self) -> usize {
        self.scenes
            .iter()
            .filter(|scene| scene.enabled && scene.is_cookable())
            .count()
    }

    /// 构建前的守卫：至少有一个启用场景、目标与输出目录非空，且启用场景必须为可构建格式。
    ///
    /// 对应 `BuildPipeline.BuildPlayer` 在没有场景时直接失败的行为。
    pub fn validate(&self) -> Result<(), String> {
        if self.target.trim().is_empty() {
            return Err("构建目标不能为空".into());
        }
        if self.output_dir.trim().is_empty() {
            return Err("输出目录不能为空".into());
        }
        if self.enabled_scene_count() == 0 {
            return Err("没有启用的场景：先在场景列表里勾选至少一个".into());
        }
        for scene in &self.scenes {
            if scene.enabled && !scene.is_cookable() {
                return Err(format!(
                    "启用场景 '{}' 不是受支持的场景格式（需要 .scene.json 或 .rscn）",
                    scene.path
                ));
            }
        }
        if self.cookable_scene_count() == 0 {
            return Err("没有可构建的启用场景（需要 .scene.json 或 .rscn）".into());
        }
        Ok(())
    }
}

/// 读取项目根下的 `assets/settings.ini` + `assets/scenes.json`。
pub fn read_build_profile(root: &Path) -> Result<BuildProfile, String> {
    let settings = read_project_settings(root)?;
    let scenes = read_scene_manifest(root)?;
    Ok(BuildProfile::new(&settings.build, &scenes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "prism_editor_build_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    fn settings() -> BuildSettingsData {
        BuildSettingsData {
            target: "windows".into(),
            profile: "release".into(),
            output_dir: "build/windows".into(),
            run_after: false,
        }
    }

    fn scene(name: &str, enabled: bool) -> SceneListEntry {
        SceneListEntry {
            name: name.into(),
            path: format!("scenes/{name}.scene.json"),
            enabled,
            guid: None,
        }
    }

    #[test]
    fn target_groups_match_names() {
        assert_eq!(target_group("windows"), "Standalone");
        assert_eq!(target_group("Linux"), "Standalone");
        assert_eq!(target_group("android"), "Android");
        assert_eq!(target_group("iOS"), "iOS");
        assert_eq!(target_group("webgl"), "WebGL");
        assert_eq!(target_group("dreamcast"), "Unknown");
    }

    #[test]
    fn build_options_follow_the_profile() {
        assert_eq!(
            BuildOptions::from_profile("release"),
            BuildOptions::default()
        );
        let debug = BuildOptions::from_profile("debug");
        assert!(debug.development);
        assert!(debug.allow_debugging);
        assert!(!debug.connect_with_profiler);
    }

    #[test]
    fn enabled_scenes_get_contiguous_build_indexes() {
        let profile = BuildProfile::new(
            &settings(),
            &[
                scene("intro", true),
                scene("draft", false),
                scene("main", true),
            ],
        );
        let indexes: Vec<Option<usize>> = profile
            .scenes
            .iter()
            .map(|scene| scene.build_index)
            .collect();
        assert_eq!(indexes, vec![Some(0), None, Some(1)]);
        assert_eq!(profile.enabled_scene_count(), 2);
        assert_eq!(
            profile
                .build_scene_order()
                .iter()
                .map(|scene| scene.name.as_str())
                .collect::<Vec<_>>(),
            vec!["intro", "main"]
        );
    }

    #[test]
    fn profile_carries_target_options_and_output() {
        let mut build = settings();
        build.profile = "debug".into();
        build.run_after = true;
        let profile = BuildProfile::new(&build, &[scene("main", true)]);
        assert_eq!(profile.name, "debug");
        assert_eq!(profile.target, "windows");
        assert_eq!(profile.target_group, "Standalone");
        assert_eq!(profile.output_dir, "build/windows");
        assert!(profile.run_after);
        assert!(profile.options.development);
    }

    #[test]
    fn validate_requires_an_enabled_scene() {
        assert!(BuildProfile::new(&settings(), &[]).validate().is_err());
        assert!(BuildProfile::new(&settings(), &[scene("draft", false)])
            .validate()
            .is_err());
        assert!(BuildProfile::new(&settings(), &[scene("main", true)])
            .validate()
            .is_ok());

        let mut empty_output = settings();
        empty_output.output_dir = "  ".into();
        assert!(BuildProfile::new(&empty_output, &[scene("main", true)])
            .validate()
            .is_err());
    }

    #[test]
    fn validate_rejects_unsupported_scene_paths() {
        let bad_scene = SceneListEntry {
            name: "notes".into(),
            path: "docs/notes.txt".into(),
            enabled: true,
            guid: None,
        };
        assert!(
            BuildProfile::new(&settings(), std::slice::from_ref(&bad_scene))
                .validate()
                .is_err()
        );

        // 禁用状态下的不支持路径不阻止构建
        let disabled_bad = SceneListEntry {
            enabled: false,
            ..bad_scene
        };
        assert!(
            BuildProfile::new(&settings(), &[disabled_bad, scene("main", true)])
                .validate()
                .is_ok()
        );

        // .rscn 也是支持的可构建场景路径
        let rscn_scene = SceneListEntry {
            name: "main".into(),
            path: "scenes/main.rscn".into(),
            enabled: true,
            guid: None,
        };
        assert!(BuildProfile::new(&settings(), &[rscn_scene])
            .validate()
            .is_ok());
    }

    #[test]
    fn serializes_with_camel_case_keys() {
        let profile = BuildProfile::new(&settings(), &[scene("main", true)]);
        let json = serde_json::to_value(&profile).expect("serialize");
        assert_eq!(json["name"], "release");
        assert_eq!(json["target"], "windows");
        assert_eq!(json["targetGroup"], "Standalone");
        assert_eq!(json["outputDir"], "build/windows");
        assert_eq!(json["runAfter"], false);
        assert_eq!(json["options"]["allowDebugging"], false);
        assert_eq!(json["scenes"][0]["buildIndex"], 0);
    }

    #[test]
    fn read_build_profile_reads_settings_and_manifest() {
        let root = temp_root("read");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(
            root.join("assets/settings.ini"),
            r#"
[app]
company = "Excurs1ons"
name = "PrismaRev"
version = "0.1.0"

[window]
title = "PrismaRev"
width = 1280
height = 720
resizable = true
fullscreen = false
vsync = true

[build]
target = "windows"
profile = "release"
output_dir = "build/windows"
run_after = false
"#,
        )
        .unwrap();
        crate::play::write_scene_manifest(&root, &[scene("intro", true), scene("draft", false)])
            .unwrap();

        let profile = read_build_profile(&root).expect("profile");
        assert_eq!(profile.name, "release");
        assert_eq!(profile.target_group, "Standalone");
        assert_eq!(profile.enabled_scene_count(), 1);
        assert_eq!(profile.scenes[0].build_index, Some(0));
        assert_eq!(profile.scenes[1].build_index, None);
        assert!(profile.validate().is_ok());

        std::fs::remove_dir_all(root).ok();
    }
}
