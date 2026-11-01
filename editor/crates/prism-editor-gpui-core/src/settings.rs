//! 项目设置（`assets/settings.ini` 的 `[app]` / `[window]` / `[build]` 段）。
//!
//! 写回时**保留文件里其它段**（`[render]` 等），只覆盖这三个段里我们负责的键，
//! 与 Web 编辑器 `prism-editor-api::settings` 的行为一致。
//!
//! 方言见 [`crate::ini`]（`docs/DESIGN.md` §「项目配置文件」）。旧项目的
//! `assets/settings.toml` 仍可读（只读回退），写端只产出 `settings.ini`。

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::ini::IniDocument;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub company: String,
    pub name: String,
    pub version: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            company: "PrismaRev".into(),
            name: "prisma".into(),
            version: "0.1.0".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub resizable: bool,
    pub fullscreen: bool,
    pub vsync: bool,
}

impl Default for WindowSettings {
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
#[serde(default)]
pub struct BuildSettings {
    pub target: String,
    pub profile: String,
    pub output_dir: String,
    pub run_after: bool,
}

impl Default for BuildSettings {
    fn default() -> Self {
        Self {
            target: "windows".into(),
            profile: "release".into(),
            output_dir: "build/windows".into(),
            run_after: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectSettings {
    pub app: AppSettings,
    pub window: WindowSettings,
    pub build: BuildSettings,
}

impl ProjectSettings {
    /// 写端目标：`assets/settings.ini`。
    pub fn path(root: &Path) -> PathBuf {
        root.join("assets/settings.ini")
    }

    /// 旧项目的 `assets/settings.toml`（只读回退）。
    pub fn legacy_path(root: &Path) -> PathBuf {
        root.join("assets/settings.toml")
    }

    /// 读取；文件不存在返回默认值。畸形行会被跳过（INI 解析不会整体失败）。
    pub fn read(root: &Path) -> io::Result<Self> {
        Ok(Self::from_document(&Self::read_document(root)?))
    }

    fn read_document(root: &Path) -> io::Result<IniDocument> {
        for (path, legacy) in [(Self::path(root), false), (Self::legacy_path(root), true)] {
            match fs::read_to_string(&path) {
                Ok(text) => {
                    if legacy {
                        log::warn!(
                            "{} is deprecated: run `prism-build-pipeline build-scenes` to \
                             migrate it to settings.ini",
                            path.display()
                        );
                    }
                    return Ok(IniDocument::parse(&text));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(IniDocument::default())
    }

    pub fn from_document(document: &IniDocument) -> Self {
        let mut settings = Self::default();
        settings.app.company = string_or(document, "app", "company", &settings.app.company);
        settings.app.name = string_or(document, "app", "name", &settings.app.name);
        settings.app.version = string_or(document, "app", "version", &settings.app.version);

        settings.window.title = string_or(document, "window", "title", &settings.window.title);
        settings.window.width = int_or(document, "window", "width", settings.window.width);
        settings.window.height = int_or(document, "window", "height", settings.window.height);
        settings.window.resizable =
            bool_or(document, "window", "resizable", settings.window.resizable);
        settings.window.fullscreen =
            bool_or(document, "window", "fullscreen", settings.window.fullscreen);
        settings.window.vsync = bool_or(document, "window", "vsync", settings.window.vsync);

        settings.build.target = string_or(document, "build", "target", &settings.build.target);
        settings.build.profile = string_or(document, "build", "profile", &settings.build.profile);
        settings.build.output_dir =
            string_or(document, "build", "output_dir", &settings.build.output_dir);
        settings.build.run_after =
            bool_or(document, "build", "run_after", settings.build.run_after);
        settings
    }

    /// 合并写回：只覆盖 app/window/build 里我们负责的键，其它段原样保留。
    pub fn write(&self, root: &Path) -> io::Result<()> {
        let mut document = Self::read_document(root)?;

        document.set("app", "company", &self.app.company);
        document.set("app", "name", &self.app.name);
        document.set("app", "version", &self.app.version);

        document.set("window", "title", &self.window.title);
        document.set("window", "width", &self.window.width.to_string());
        document.set("window", "height", &self.window.height.to_string());
        document.set("window", "resizable", &self.window.resizable.to_string());
        document.set("window", "fullscreen", &self.window.fullscreen.to_string());
        document.set("window", "vsync", &self.window.vsync.to_string());

        document.set("build", "target", &self.build.target);
        document.set("build", "profile", &self.build.profile);
        document.set("build", "output_dir", &self.build.output_dir);
        document.set("build", "run_after", &self.build.run_after.to_string());

        let path = Self::path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, document.render())
    }
}

/// `[build] target` 的常用取值（下拉里给提示，不限制手写成别的）。
pub const TARGET_CHOICES: &[&str] = &["windows", "android", "linux"];
/// `[build] profile` 的常用取值（与 `scripts/build-dist.*` 的 profile 名一致）。
pub const PROFILE_CHOICES: &[&str] = &["debug", "release", "dist", "tiny"];

/// 「项目设置」面板的一个可编辑字段。
///
/// 这张表是**唯一**的字段清单：面板按它渲染，搜索按它过滤。「13 个键都能改」
/// 这条承诺由 `every_written_key_is_editable` 测试守着 —— 给 `write()` 加键却
/// 忘了加表项，测试会红。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsField {
    pub section: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    /// 额外的搜索词（中文 / 同义词），不显示。
    pub keywords: &'static str,
}

pub const SETTINGS_SECTIONS: &[&str] = &["app", "window", "build"];

pub const SETTINGS_FIELDS: &[SettingsField] = &[
    SettingsField {
        section: "app",
        key: "company",
        label: "Company",
        keywords: "公司 厂商 publisher",
    },
    SettingsField {
        section: "app",
        key: "name",
        label: "App name",
        keywords: "名字 应用 package",
    },
    SettingsField {
        section: "app",
        key: "version",
        label: "Version",
        keywords: "版本号 版本",
    },
    SettingsField {
        section: "window",
        key: "title",
        label: "Window title",
        keywords: "标题 窗口",
    },
    SettingsField {
        section: "window",
        key: "width",
        label: "Window width",
        keywords: "宽 分辨率 size",
    },
    SettingsField {
        section: "window",
        key: "height",
        label: "Window height",
        keywords: "高 分辨率 size",
    },
    SettingsField {
        section: "window",
        key: "resizable",
        label: "Resizable",
        keywords: "可缩放 拉伸",
    },
    SettingsField {
        section: "window",
        key: "fullscreen",
        label: "Fullscreen",
        keywords: "全屏",
    },
    SettingsField {
        section: "window",
        key: "vsync",
        label: "V-Sync",
        keywords: "垂直同步 同步",
    },
    SettingsField {
        section: "build",
        key: "target",
        label: "Build target",
        keywords: "平台 目标 android windows",
    },
    SettingsField {
        section: "build",
        key: "profile",
        label: "Build profile",
        keywords: "配置 debug release dist tiny",
    },
    SettingsField {
        section: "build",
        key: "output_dir",
        label: "Output dir",
        keywords: "输出 目录 路径",
    },
    SettingsField {
        section: "build",
        key: "run_after",
        label: "Run after build",
        keywords: "运行 启动 play",
    },
];

/// 字段是否命中查询（label / key / 段名 / 关键词，大小写不敏感）。
pub fn field_matches(field: &SettingsField, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    [field.label, field.key, field.section, field.keywords]
        .iter()
        .any(|text| text.to_lowercase().contains(&query))
}

/// 按搜索词过滤字段表；空查询返回全部。
pub fn matching_fields(query: &str) -> Vec<&'static SettingsField> {
    SETTINGS_FIELDS
        .iter()
        .filter(|field| field_matches(field, query))
        .collect()
}

impl ProjectSettings {
    /// 恢复某一段的默认值（`app` / `window` / `build`）。
    pub fn reset_section(&mut self, section: &str) {
        match section {
            "app" => self.app = AppSettings::default(),
            "window" => self.window = WindowSettings::default(),
            "build" => self.build = BuildSettings::default(),
            _ => {}
        }
    }
}

/// 面板的读盘缓存：只在「首次 / 文件内容变了 / 手动重载」时解析，
/// 编辑中的值留在这里，显式 Apply 才落盘（#428 的 1、2、5 条）。
pub struct SettingsCache {
    root: PathBuf,
    /// 编辑中的值。
    pub settings: ProjectSettings,
    /// 磁盘上的值（Apply 后同步）。
    saved: ProjectSettings,
    /// 上次读到的问题（解析/IO），面板顶部展示。
    pub error: Option<String>,
    /// 上次读到的原始文本；用它判断「磁盘上的文件变了没有」。
    ///
    /// 只比 mtime 在粗粒度文件系统上会漏掉同一秒内的覆盖写（已知 flaky 测试就是
    /// 这个原因）；决策面板是显式打开的，读一个 1–2 KB 的配置文件可以接受。
    text: Option<String>,
    /// 编辑期间磁盘被改过（面板提示，但不覆盖用户输入）。
    external_change: bool,
}

impl SettingsCache {
    pub fn load(root: impl Into<PathBuf>) -> Self {
        let mut cache = Self {
            root: root.into(),
            settings: ProjectSettings::default(),
            saved: ProjectSettings::default(),
            error: None,
            text: None,
            external_change: false,
        };
        cache.reload();
        cache
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn is_dirty(&self) -> bool {
        self.settings != self.saved
    }

    pub fn external_change(&self) -> bool {
        self.external_change
    }

    /// 文件在磁盘上变了就重读一次。有未保存改动时**不覆盖**编辑中的值，
    /// 只把 `external_change` 立起来让面板提示。
    pub fn refresh(&mut self) -> bool {
        let current = read_settings_text(&self.root);
        if current == self.text {
            return false;
        }
        if self.is_dirty() {
            self.external_change = true;
            self.text = current;
            return false;
        }
        self.reload();
        true
    }

    fn reload(&mut self) {
        self.text = read_settings_text(&self.root);
        match ProjectSettings::read(&self.root) {
            Ok(settings) => {
                self.settings = settings.clone();
                self.saved = settings;
                self.error = None;
                self.external_change = false;
            }
            Err(error) => {
                self.error = Some(format!("read assets/settings.ini: {error}"));
                self.settings = ProjectSettings::default();
                self.saved = ProjectSettings::default();
            }
        }
    }

    /// 落盘 + 同步「已保存」快照。
    pub fn apply(&mut self) -> io::Result<()> {
        self.settings.write(&self.root)?;
        self.saved = self.settings.clone();
        self.error = None;
        self.external_change = false;
        self.text = read_settings_text(&self.root);
        Ok(())
    }

    /// 丢掉未保存的改动。
    pub fn revert(&mut self) {
        self.settings = self.saved.clone();
    }

    /// 丢弃未保存改动并整份重读（文件坏了也能用：读到默认值）。
    pub fn reload_from_disk(&mut self) {
        self.settings = self.saved.clone();
        self.reload();
    }

    pub fn reset_section(&mut self, section: &str) {
        self.settings.reset_section(section);
    }

    pub fn reset_all(&mut self) {
        self.settings = ProjectSettings::default();
    }
}

/// 读 `assets/settings.ini`（缺省回退旧 `settings.toml`）的原始文本。
fn read_settings_text(root: &std::path::Path) -> Option<String> {
    for path in [
        ProjectSettings::path(root),
        ProjectSettings::legacy_path(root),
    ] {
        if let Ok(text) = fs::read_to_string(&path) {
            return Some(text);
        }
    }
    None
}

fn string_or(document: &IniDocument, section: &str, key: &str, fallback: &str) -> String {
    document
        .get_string(section, key)
        .unwrap_or_else(|| fallback.to_string())
}

fn int_or(document: &IniDocument, section: &str, key: &str, fallback: u32) -> u32 {
    document
        .get_u32(section, key)
        .map(|value| value.clamp(1, 16_384))
        .unwrap_or(fallback)
}

fn bool_or(document: &IniDocument, section: &str, key: &str, fallback: bool) -> bool {
    document.get_bool(section, key).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(1);

    struct TempRoot {
        root: PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "prism-editor-gpui-settings-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("assets")).unwrap();
            Self { root }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn every_written_key_is_editable() {
        let temp = TempRoot::new();
        ProjectSettings::default().write(&temp.root).unwrap();
        let text = fs::read_to_string(ProjectSettings::path(&temp.root)).unwrap();
        let document = IniDocument::parse(&text);
        for section in SETTINGS_SECTIONS {
            let keys: Vec<&str> = document
                .sections()
                .iter()
                .find(|entry| entry.name == *section)
                .map(|entry| entry.entries.iter().map(|(key, _)| key.as_str()).collect())
                .unwrap_or_default();
            assert!(!keys.is_empty(), "[{section}] is empty");
            for key in keys {
                assert!(
                    SETTINGS_FIELDS
                        .iter()
                        .any(|field| field.section == *section && field.key == key),
                    "[{section}] {key} is written but has no SettingsField entry"
                );
            }
        }
    }

    #[test]
    fn field_search_matches_labels_keys_and_chinese_keywords() {
        let hits = matching_fields("vsync");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "vsync");

        assert!(matching_fields("全屏")
            .iter()
            .any(|field| field.key == "fullscreen"));
        assert!(matching_fields("BUILD")
            .iter()
            .all(|field| field.section == "build"));
        assert_eq!(matching_fields("").len(), SETTINGS_FIELDS.len());
        assert!(matching_fields("nope-nothing").is_empty());
    }

    #[test]
    fn cache_only_reloads_when_the_file_changes() {
        let temp = TempRoot::new();
        ProjectSettings::default().write(&temp.root).unwrap();
        let mut cache = SettingsCache::load(&temp.root);
        assert!(!cache.refresh(), "nothing changed yet");

        let mut settings = ProjectSettings::default();
        settings.app.company = "Elsewhere".into();
        settings.write(&temp.root).unwrap();
        assert!(cache.refresh(), "file contents changed");
        assert_eq!(cache.settings.app.company, "Elsewhere");
        assert!(!cache.is_dirty());
    }

    #[test]
    fn cache_keeps_unsaved_edits_when_the_file_changes_underneath() {
        let temp = TempRoot::new();
        ProjectSettings::default().write(&temp.root).unwrap();
        let mut cache = SettingsCache::load(&temp.root);
        cache.settings.app.name = "edited".into();
        assert!(cache.is_dirty());

        // 同长度 / 同一秒内的覆盖写也要能检测到（按内容比较，不看 mtime）。
        let mut other = ProjectSettings::default();
        other.app.name = "external".into();
        other.write(&temp.root).unwrap();
        assert!(!cache.refresh(), "unsaved edits win");
        assert_eq!(cache.settings.app.name, "edited");
        assert!(cache.external_change());

        cache.revert();
        assert!(!cache.is_dirty());
        assert_eq!(cache.settings.app.name, AppSettings::default().name);
    }

    #[test]
    fn cache_reset_section_restores_only_that_section() {
        let temp = TempRoot::new();
        let mut cache = SettingsCache::load(&temp.root);
        cache.settings.app.name = "fishing".into();
        cache.settings.window.width = 800;
        cache.reset_section("window");
        assert_eq!(cache.settings.window.width, WindowSettings::default().width);
        assert_eq!(cache.settings.app.name, "fishing");

        cache.reset_all();
        assert_eq!(cache.settings, ProjectSettings::default());
        assert!(!cache.is_dirty());
    }

    #[test]
    fn apply_writes_the_edited_values() {
        let temp = TempRoot::new();
        let mut cache = SettingsCache::load(&temp.root);
        cache.settings.build.target = "android".into();
        cache.settings.window.fullscreen = true;
        cache.apply().unwrap();
        assert!(!cache.is_dirty());

        let reread = ProjectSettings::read(&temp.root).unwrap();
        assert_eq!(reread.build.target, "android");
        assert!(reread.window.fullscreen);
    }

    #[test]
    fn missing_file_reads_defaults() {
        let temp = TempRoot::new();
        let settings = ProjectSettings::read(&temp.root).unwrap();
        assert_eq!(settings, ProjectSettings::default());
    }

    #[test]
    fn write_creates_ini_and_roundtrips() {
        let temp = TempRoot::new();
        let mut settings = ProjectSettings::default();
        settings.app.company = "Acme".into();
        settings.window.width = 1280;
        settings.window.vsync = false;
        settings.build.run_after = true;

        settings.write(&temp.root).unwrap();
        let text = fs::read_to_string(ProjectSettings::path(&temp.root)).unwrap();
        assert!(text.contains("[app]"));
        assert!(text.contains("company = \"Acme\""));
        assert!(text.contains("width = 1280"));

        let read = ProjectSettings::read(&temp.root).unwrap();
        assert_eq!(read, settings);
    }

    #[test]
    fn write_preserves_other_sections_and_unknown_keys() {
        let temp = TempRoot::new();
        fs::write(
            ProjectSettings::path(&temp.root),
            r#"[app]
company = "Old"
custom = "keep-me"

[render]
render_mode = "path_trace"
debug_flags = 7

[window]
width = 800
title = "Old"
"#,
        )
        .unwrap();

        let mut settings = ProjectSettings::read(&temp.root).unwrap();
        assert_eq!(settings.app.company, "Old");
        assert_eq!(settings.window.width, 800);
        assert_eq!(settings.window.height, 900, "缺失键用默认值");

        settings.app.company = "New".into();
        settings.write(&temp.root).unwrap();

        let text = fs::read_to_string(ProjectSettings::path(&temp.root)).unwrap();
        assert!(text.contains("render_mode = \"path_trace\""));
        assert!(text.contains("debug_flags = 7"));
        assert!(text.contains("custom = \"keep-me\""));
        assert!(text.contains("company = \"New\""));
        assert!(text.contains("width = 800"));
    }

    #[test]
    fn legacy_toml_settings_are_readable() {
        let temp = TempRoot::new();
        fs::write(
            ProjectSettings::legacy_path(&temp.root),
            "[app]\ncompany = \"Legacy\"\n[window]\nwidth = 800\n",
        )
        .unwrap();

        let settings = ProjectSettings::read(&temp.root).unwrap();
        assert_eq!(settings.app.company, "Legacy");
        assert_eq!(settings.window.width, 800);

        // 写端只产出 settings.ini。
        settings.write(&temp.root).unwrap();
        assert!(ProjectSettings::path(&temp.root).is_file());
        assert!(ProjectSettings::legacy_path(&temp.root).is_file());
    }

    #[test]
    fn malformed_lines_fall_back_to_defaults() {
        let temp = TempRoot::new();
        fs::write(ProjectSettings::path(&temp.root), "[app\ncompany =").unwrap();
        let settings = ProjectSettings::read(&temp.root).unwrap();
        assert_eq!(settings, ProjectSettings::default());
    }

    #[test]
    fn out_of_range_numbers_are_clamped() {
        let temp = TempRoot::new();
        fs::write(
            ProjectSettings::path(&temp.root),
            "[window]\nwidth = 999999999\nheight = 0\n",
        )
        .unwrap();
        let settings = ProjectSettings::read(&temp.root).unwrap();
        assert_eq!(settings.window.width, 16_384);
        assert_eq!(settings.window.height, 1);
    }
}
