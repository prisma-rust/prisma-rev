//! 项目级渲染设置（`assets/settings.ini` 的 `[render]` 段）。
//!
//! 引擎启动时由 `prism_app::load_config` 读取并覆盖 `RenderSettings`，
//! 因此这里的字段就是「编辑器 → 运行时」的持久化契约。读写保留文件里
//! 其它段（`[app]` / `[window]` / `[build]`），但 INI 往返不会保留注释。
//!
//! 这里刻意重复一份字段定义（而不是依赖 `prism-engine`）：编辑器后端
//! 必须保持 Vulkan-free。字段名/取值与 `prism_engine::config::RenderConfig`
//! 一一对应。

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ini::IniDocument;

/// 前端使用的渲染设置（camelCase JSON）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderSettingsData {
    /// `"raster"` 或 `"path_trace"`。
    pub render_mode: String,
    /// PBR isolate 位掩码（0 = 正常渲染）。
    pub debug_flags: u32,
    /// 0 = Reinhard, 1 = ACES, 2 = passthrough（2D / 像素画，保留源色）。
    pub tonemap_mode: u32,
    /// 0 = HDR tonemap, 1 = depth, 2 = view-space normal。
    pub debug_rt: u32,
    pub pt_max_bounces: u32,
    pub pt_ray_max_distance: f32,
    /// 0 = 无限累积。
    pub pt_max_iterations: u32,
}

impl Default for RenderSettingsData {
    fn default() -> Self {
        Self {
            render_mode: "raster".to_string(),
            debug_flags: 0,
            tonemap_mode: 0,
            debug_rt: 0,
            pt_max_bounces: 3,
            pt_ray_max_distance: 1000.0,
            pt_max_iterations: 0,
        }
    }
}

impl RenderSettingsData {
    fn validate(&self) -> Result<(), String> {
        match self.render_mode.as_str() {
            "raster" | "path_trace" => {}
            other => {
                return Err(format!(
                    "render_mode 只能是 \"raster\" 或 \"path_trace\"（收到 {other:?}）"
                ))
            }
        }
        if self.tonemap_mode > 2 {
            return Err("tonemap_mode 只能是 0（Reinhard）、1（ACES）或 2（Passthrough）".into());
        }
        if self.debug_rt > 2 {
            return Err("debug_rt 只能是 0 / 1 / 2".into());
        }
        if !(1..=64).contains(&self.pt_max_bounces) {
            return Err("pt_max_bounces 必须在 1..=64".into());
        }
        if !(self.pt_ray_max_distance.is_finite() && self.pt_ray_max_distance > 0.0) {
            return Err("pt_ray_max_distance 必须是正数".into());
        }
        Ok(())
    }
}

/// 写端目标：`assets/settings.ini`。
pub fn settings_path(root: &Path) -> PathBuf {
    root.join("assets").join("settings.ini")
}

/// 旧的 `assets/settings.toml`——只读回退，写端不再产出。
pub fn legacy_settings_path(root: &Path) -> PathBuf {
    root.join("assets").join("settings.toml")
}

/// 读取现有配置文档：`settings.ini` 优先，缺失时回退旧 `settings.toml`
/// （并提示迁移）。两者都没有时返回空文档（= 全部默认值）。
fn read_document(root: &Path) -> Result<IniDocument, String> {
    for (path, legacy) in [
        (settings_path(root), false),
        (legacy_settings_path(root), true),
    ] {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                if legacy {
                    log::warn!(
                        "{} is deprecated: run `prism-build-pipeline build-scenes` to migrate \
                         it to settings.ini",
                        path.display()
                    );
                }
                return Ok(IniDocument::parse(&text));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("无法读取 {}: {error}", path.display())),
        }
    }
    Ok(IniDocument::default())
}

fn write_document(root: &Path, document: &IniDocument) -> Result<(), String> {
    let path = settings_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建 {}: {error}", parent.display()))?;
    }
    std::fs::write(&path, document.render())
        .map_err(|error| format!("无法写入 {}: {error}", path.display()))
}

/// 读取 `[render]` 段（文件/段缺失时返回默认值）。
pub fn read_render_settings(root: &Path) -> Result<RenderSettingsData, String> {
    let document = read_document(root)?;
    let mut data = RenderSettingsData::default();
    if let Some(value) = document.get_string("render", "render_mode") {
        data.render_mode = value;
    }
    data.debug_flags = document
        .get_u32("render", "debug_flags")
        .unwrap_or(data.debug_flags);
    data.tonemap_mode = document
        .get_u32("render", "tonemap_mode")
        .unwrap_or(data.tonemap_mode);
    data.debug_rt = document
        .get_u32("render", "debug_rt")
        .unwrap_or(data.debug_rt);
    data.pt_max_bounces = document
        .get_u32("render", "pt_max_bounces")
        .unwrap_or(data.pt_max_bounces);
    data.pt_ray_max_distance = document
        .get_f32("render", "pt_ray_max_distance")
        .unwrap_or(data.pt_ray_max_distance);
    data.pt_max_iterations = document
        .get_u32("render", "pt_max_iterations")
        .unwrap_or(data.pt_max_iterations);
    Ok(data)
}

/// 写入 `[render]` 段（保留文件中的其它段）。
pub fn write_render_settings(root: &Path, data: &RenderSettingsData) -> Result<(), String> {
    data.validate()?;

    let mut document = read_document(root)?;
    document.set("render", "render_mode", &data.render_mode);
    document.set("render", "debug_flags", &data.debug_flags.to_string());
    document.set("render", "tonemap_mode", &data.tonemap_mode.to_string());
    document.set("render", "debug_rt", &data.debug_rt.to_string());
    document.set("render", "pt_max_bounces", &data.pt_max_bounces.to_string());
    document.set(
        "render",
        "pt_ray_max_distance",
        &format!("{:.1}", data.pt_ray_max_distance),
    );
    document.set(
        "render",
        "pt_max_iterations",
        &data.pt_max_iterations.to_string(),
    );
    write_document(root, &document)
}

// ---------------------------------------------------------------------------
// Project settings (`[app]` / `[window]` / `[build]`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettingsData {
    pub company: String,
    pub name: String,
    pub version: String,
}

impl Default for AppSettingsData {
    fn default() -> Self {
        Self {
            company: "Excurs1ons".into(),
            name: "PrismaRev".into(),
            version: "0.1.0".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowSettingsData {
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub resizable: bool,
    pub fullscreen: bool,
    pub vsync: bool,
}

impl Default for WindowSettingsData {
    fn default() -> Self {
        Self {
            title: "PrismaRev".into(),
            width: 1600,
            height: 900,
            resizable: true,
            fullscreen: false,
            vsync: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildSettingsData {
    pub target: String,
    pub profile: String,
    pub output_dir: String,
    pub run_after: bool,
}

impl Default for BuildSettingsData {
    fn default() -> Self {
        Self {
            target: "windows".into(),
            profile: "release".into(),
            output_dir: "build/windows".into(),
            run_after: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSettingsData {
    pub app: AppSettingsData,
    pub window: WindowSettingsData,
    pub build: BuildSettingsData,
}

impl ProjectSettingsData {
    fn validate(&self) -> Result<(), String> {
        if self.app.name.trim().is_empty() || self.app.company.trim().is_empty() {
            return Err("应用名称和公司名不能为空".into());
        }
        if self.window.title.trim().is_empty() {
            return Err("窗口标题不能为空".into());
        }
        if !(320..=16384).contains(&self.window.width)
            || !(240..=16384).contains(&self.window.height)
        {
            return Err("窗口尺寸必须在 320x240 .. 16384x16384".into());
        }
        if self.build.target != "windows" {
            return Err("目前只支持 windows 构建目标".into());
        }
        if !matches!(self.build.profile.as_str(), "debug" | "release") {
            return Err("构建配置只能是 debug 或 release".into());
        }
        if self.build.output_dir.trim().is_empty() {
            return Err("输出目录不能为空".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 只写入「真正改动的字段」
// ---------------------------------------------------------------------------
//
// 编辑器里有多个面板同时编辑同一个 `settings.ini`（项目设置面板管
// `[app]`/`[window]`/`[build]`/`[render]`，构建面板也改 `[build]`）。面板都是
// 打开时读一份快照、保存时整段写回，于是一个面板会把另一个面板**没碰过**的
// 段（以及同段里没碰过的字段）用旧值覆盖掉。
//
// 这里让前端把「打开时的快照 before」和「现在的值 after」一起发过来，只把
// 两者不同的叶子写到磁盘上当前的设置里：没改过的字段永远以磁盘为准。

/// 把 `before` → `after` 之间**改动的叶子**合并到 `current` 上。
fn merge_changed_values(
    current: serde_json::Value,
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> serde_json::Value {
    if before == after {
        return current;
    }

    let (
        serde_json::Value::Object(mut current),
        serde_json::Value::Object(before),
        serde_json::Value::Object(after),
    ) = (current, before, after)
    else {
        return after.clone();
    };

    for (key, after_value) in after {
        // 面板没改这个键（或它根本不在快照里）→ 保留磁盘上的值。
        if before.get(key) == Some(after_value) {
            continue;
        }
        let Some(before_value) = before.get(key) else {
            continue;
        };
        let Some(slot) = current.get_mut(key) else {
            continue;
        };
        let merged = merge_changed_values(std::mem::take(slot), before_value, after_value);
        if let Some(slot) = current.get_mut(key) {
            *slot = merged;
        }
    }

    serde_json::Value::Object(current)
}

fn merged_settings<T>(current: &T, before: &T, after: &T) -> Result<T, String>
where
    T: Serialize + DeserializeOwned,
{
    let to_value =
        |value: &T| serde_json::to_value(value).map_err(|error| format!("序列化设置失败: {error}"));
    let merged = merge_changed_values(to_value(current)?, &to_value(before)?, &to_value(after)?);
    serde_json::from_value(merged).map_err(|error| format!("合并设置失败: {error}"))
}

/// 只写入 `[app]`/`[window]`/`[build]` 中 `before` → `after` 真正改动的字段。
///
/// 其它面板在两次读写之间改过的字段保留磁盘值；`before == after` 时不写文件。
pub fn write_project_settings_diff(
    root: &Path,
    before: &ProjectSettingsData,
    after: &ProjectSettingsData,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let current = read_project_settings(root)?;
    let merged = merged_settings(&current, before, after)?;
    write_project_settings(root, &merged)
}

/// 只写入 `[render]` 中 `before` → `after` 真正改动的字段。
pub fn write_render_settings_diff(
    root: &Path,
    before: &RenderSettingsData,
    after: &RenderSettingsData,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let current = read_render_settings(root)?;
    let merged = merged_settings(&current, before, after)?;
    write_render_settings(root, &merged)
}

/// 读取项目设置（`[app]` / `[window]` / `[build]`）；缺失时使用默认值。
pub fn read_project_settings(root: &Path) -> Result<ProjectSettingsData, String> {
    let document = read_document(root)?;
    let mut data = ProjectSettingsData::default();

    if let Some(value) = document.get_string("app", "company") {
        data.app.company = value;
    }
    if let Some(value) = document.get_string("app", "name") {
        data.app.name = value;
    }
    if let Some(value) = document.get_string("app", "version") {
        data.app.version = value;
    }

    if let Some(value) = document.get_string("window", "title") {
        data.window.title = value;
    }
    data.window.width = document
        .get_u32("window", "width")
        .unwrap_or(data.window.width);
    data.window.height = document
        .get_u32("window", "height")
        .unwrap_or(data.window.height);
    data.window.resizable = document
        .get_bool("window", "resizable")
        .unwrap_or(data.window.resizable);
    data.window.fullscreen = document
        .get_bool("window", "fullscreen")
        .unwrap_or(data.window.fullscreen);
    data.window.vsync = document
        .get_bool("window", "vsync")
        .unwrap_or(data.window.vsync);

    if let Some(value) = document.get_string("build", "target") {
        data.build.target = value;
    }
    if let Some(value) = document.get_string("build", "profile") {
        data.build.profile = value;
    }
    if let Some(value) = document.get_string("build", "output_dir") {
        data.build.output_dir = value;
    }
    data.build.run_after = document
        .get_bool("build", "run_after")
        .unwrap_or(data.build.run_after);

    Ok(data)
}

/// 写入项目设置，保留 `[render]` 等其它段。
pub fn write_project_settings(root: &Path, data: &ProjectSettingsData) -> Result<(), String> {
    data.validate()?;
    let mut document = read_document(root)?;

    document.set("app", "company", &data.app.company);
    document.set("app", "name", &data.app.name);
    document.set("app", "version", &data.app.version);

    document.set("window", "title", &data.window.title);
    document.set("window", "width", &data.window.width.to_string());
    document.set("window", "height", &data.window.height.to_string());
    document.set("window", "resizable", &data.window.resizable.to_string());
    document.set("window", "fullscreen", &data.window.fullscreen.to_string());
    document.set("window", "vsync", &data.window.vsync.to_string());

    document.set("build", "target", &data.build.target);
    document.set("build", "profile", &data.build.profile);
    document.set("build", "output_dir", &data.build.output_dir);
    document.set("build", "run_after", &data.build.run_after.to_string());

    write_document(root, &document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ini::IniDocument;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        dir
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = temp_dir("prism-editor-api-settings-missing");
        assert_eq!(
            read_render_settings(&dir).unwrap(),
            RenderSettingsData::default()
        );
    }

    #[test]
    fn roundtrip_preserves_other_sections() {
        let dir = temp_dir("prism-editor-api-settings-roundtrip");
        std::fs::write(
            settings_path(&dir),
            "[app]\nname = \"Demo\"\n\n[render]\nrender_mode = \"path_trace\"\n",
        )
        .unwrap();

        let mut data = read_render_settings(&dir).unwrap();
        assert_eq!(data.render_mode, "path_trace");
        data.pt_max_bounces = 5;
        write_render_settings(&dir, &data).unwrap();

        let text = std::fs::read_to_string(settings_path(&dir)).unwrap();
        let document = IniDocument::parse(&text);
        assert_eq!(document.get("app", "name"), Some("Demo"));
        assert_eq!(read_render_settings(&dir).unwrap().pt_max_bounces, 5);
    }

    #[test]
    fn legacy_toml_settings_are_readable() {
        let dir = temp_dir("prism-editor-api-settings-legacy");
        std::fs::write(
            legacy_settings_path(&dir),
            "[render]\nrender_mode = \"path_trace\"\ntonomap = 0\ntonemap_mode = 1\n",
        )
        .unwrap();
        let data = read_render_settings(&dir).unwrap();
        assert_eq!(data.render_mode, "path_trace");
        assert_eq!(data.tonemap_mode, 1);

        // 写端只产出 settings.ini；旧文件不再被改写。
        write_render_settings(&dir, &data).unwrap();
        assert!(settings_path(&dir).is_file());
        assert!(legacy_settings_path(&dir).is_file());
    }

    #[test]
    fn passthrough_tonemap_is_a_valid_project_setting() {
        let dir = temp_dir("prism-editor-api-settings-passthrough");
        let data = RenderSettingsData {
            tonemap_mode: 2,
            ..Default::default()
        };
        write_render_settings(&dir, &data).unwrap();
        assert_eq!(read_render_settings(&dir).unwrap().tonemap_mode, 2);
    }

    #[test]
    fn invalid_values_are_rejected() {
        let dir = temp_dir("prism-editor-api-settings-invalid");
        let data = RenderSettingsData {
            tonemap_mode: 9,
            ..Default::default()
        };
        assert!(write_render_settings(&dir, &data).is_err());
    }

    #[test]
    fn project_settings_roundtrip_preserves_render() {
        let dir = temp_dir("prism-editor-api-project-settings-roundtrip");
        std::fs::write(
            settings_path(&dir),
            "[render]\nrender_mode = \"path_trace\"\n",
        )
        .unwrap();

        let mut data = read_project_settings(&dir).unwrap();
        assert_eq!(data.app.name, "PrismaRev");
        data.app.name = "DemoGame".into();
        data.window.width = 1280;
        data.build.profile = "debug".into();
        write_project_settings(&dir, &data).unwrap();

        let text = std::fs::read_to_string(settings_path(&dir)).unwrap();
        let document = IniDocument::parse(&text);
        assert_eq!(document.get("app", "name"), Some("DemoGame"));
        assert_eq!(document.get_u32("window", "width"), Some(1280));
        assert_eq!(document.get("build", "profile"), Some("debug"));
        assert_eq!(document.get("render", "render_mode"), Some("path_trace"));
    }

    #[test]
    fn project_settings_reject_invalid_build_profile() {
        let dir = temp_dir("prism-editor-api-project-settings-invalid");
        let mut data = ProjectSettingsData::default();
        data.build.profile = "shipping".into();
        assert!(write_project_settings(&dir, &data).is_err());
    }

    #[test]
    fn project_settings_diff_keeps_changes_from_other_panels() {
        let dir = temp_dir("prism-editor-api-project-settings-diff-panels");
        write_project_settings(&dir, &ProjectSettingsData::default()).unwrap();

        // 构建面板打开时的快照。
        let before = read_project_settings(&dir).unwrap();

        // 项目设置面板在此期间改了 [app]，并已写入磁盘。
        let mut other = read_project_settings(&dir).unwrap();
        other.app.name = "Renamed".into();
        write_project_settings(&dir, &other).unwrap();

        // 构建面板只改了 [build].output_dir 后保存。
        let mut after = before.clone();
        after.build.output_dir = "build/out".into();
        write_project_settings_diff(&dir, &before, &after).unwrap();

        let saved = read_project_settings(&dir).unwrap();
        assert_eq!(saved.app.name, "Renamed", "别的面板改过的段被旧快照覆盖了");
        assert_eq!(saved.build.output_dir, "build/out");
    }

    #[test]
    fn project_settings_diff_keeps_fields_the_panel_did_not_touch() {
        let dir = temp_dir("prism-editor-api-project-settings-diff-fields");
        write_project_settings(&dir, &ProjectSettingsData::default()).unwrap();
        let before = read_project_settings(&dir).unwrap();

        // 同一段里的另一个字段被别处改了。
        let mut other = read_project_settings(&dir).unwrap();
        other.build.profile = "debug".into();
        write_project_settings(&dir, &other).unwrap();

        // 面板只改窗口宽度（快照里的 build.profile 还是 release，但没改过）。
        let mut after = before.clone();
        after.window.width = 1280;
        write_project_settings_diff(&dir, &before, &after).unwrap();

        let saved = read_project_settings(&dir).unwrap();
        assert_eq!(saved.window.width, 1280);
        assert_eq!(saved.build.profile, "debug", "未改动的同段字段被旧值覆盖了");
    }

    #[test]
    fn project_settings_diff_does_not_rewrite_unchanged_snapshots() {
        let dir = temp_dir("prism-editor-api-project-settings-diff-noop");
        // 手写文件（写回会丢掉注释）用来证明「没改动就不写文件」。
        std::fs::write(settings_path(&dir), "# keep me\n[app]\nname = \"Demo\"\n").unwrap();

        let snapshot = read_project_settings(&dir).unwrap();
        write_project_settings_diff(&dir, &snapshot, &snapshot.clone()).unwrap();

        let text = std::fs::read_to_string(settings_path(&dir)).unwrap();
        assert!(text.contains("# keep me"), "无改动时不应重写文件: {text}");
    }

    #[test]
    fn render_settings_diff_only_writes_changed_fields() {
        let dir = temp_dir("prism-editor-api-render-settings-diff");
        write_render_settings(&dir, &RenderSettingsData::default()).unwrap();
        let before = read_render_settings(&dir).unwrap();

        // 别处改了色调映射。
        let mut other = read_render_settings(&dir).unwrap();
        other.tonemap_mode = 1;
        write_render_settings(&dir, &other).unwrap();

        // 面板只改最大弹射次数。
        let mut after = before.clone();
        after.pt_max_bounces = 7;
        write_render_settings_diff(&dir, &before, &after).unwrap();

        let saved = read_render_settings(&dir).unwrap();
        assert_eq!(saved.pt_max_bounces, 7);
        assert_eq!(saved.tonemap_mode, 1, "未改动的同段字段被旧值覆盖了");
    }
}
