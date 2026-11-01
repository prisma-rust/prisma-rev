//! 编辑器本地状态持久化（对应 Web 编辑器的 localStorage 键）。
//!
//! Web 侧存三份：`prisma-editor.dock.v4` / `prisma-editor.theme.v1` /
//! `prisma-editor.locale.v1`（外加 preferences）。GPUI 壳把它们合并成**一个**
//! JSON 文件（默认 `<project>/target/editor-gpui/state.json`），方便备份与排查。
//!
//! 语义与前端一致：主题只存「预设 + 被改过的 token」；布局走 `DockLayout`
//! 的 sanitize；未知面板 / 非法颜色 / 不存在的预设都静默回退。

use crate::dock::DockLayout;
use crate::i18n::Locale;
use crate::theme::{Rgba, ThemeToken, DEFAULT_THEME_PRESET, THEME_PRESETS};
use crate::workspace::Workspace;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const STATE_VERSION: u32 = 1;

/// 本机首选项（与 Web 编辑器的 preferences 面板同字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Preferences {
    pub autosave: bool,
    pub confirm_delete: bool,
    pub compact: bool,
    /// 松散资产变化后自动重打场景包（#495）。默认开：改贴图 / HDR / 网格后
    /// 不用退出 Play 就能看到；关掉可以避免频繁重打包。
    pub hot_reload_assets: bool,
    pub max_log_lines: f32,
    pub locale_auto: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            autosave: true,
            confirm_delete: true,
            compact: false,
            hot_reload_assets: true,
            max_log_lines: 5000.0,
            locale_auto: true,
        }
    }
}

/// Scene 视图与渲染设置（#432）：重启后保持上次的开关与工具。
///
/// 引擎的 `RenderSettings` 不归 `core` 管，所以这里只存一段不透明 JSON，
/// 由编辑器自己负责序列化/反序列化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SceneSettings {
    pub show_grid: bool,
    pub show_skybox: bool,
    /// `World` / `Local`（ASCII，避免本地化文案进存档）。
    pub gizmo_space: String,
    /// `Translate` / `Rotate` / `Scale`。
    pub gizmo_tool: String,
    /// 引擎 `RenderSettings` 的 JSON 镜像；`None` = 旧存档，用默认值。
    pub render: Option<Value>,
}

impl Default for SceneSettings {
    fn default() -> Self {
        Self {
            show_grid: true,
            show_skybox: true,
            gizmo_space: "World".to_string(),
            gizmo_tool: "Translate".to_string(),
            render: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorState {
    pub version: u32,
    /// `{version, dock, floating}`（与 Web 的 dock v4 同一形状）。
    pub dock: Value,
    pub theme_preset: String,
    /// token key（`bg` / `selection` …）→ `#rrggbb`。
    #[serde(default)]
    pub theme_overrides: BTreeMap<String, String>,
    /// `zh-CN` / `en-US`。
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default)]
    pub preferences: Preferences,
    /// Scene 视图 / 渲染设置（`#[serde(default)]` 保证旧 `state.json` 仍能读）。
    #[serde(default)]
    pub scene: SceneSettings,
    /// 被最大化的面板 tab（`#[serde(default)]`：旧存档 = 正常布局）。
    #[serde(default)]
    pub maximized: Option<String>,
}

fn default_locale() -> String {
    "zh-CN".to_string()
}

impl Default for EditorState {
    fn default() -> Self {
        let workspace = Workspace::sample();
        Self::capture(&workspace, &Preferences::default())
    }
}

impl EditorState {
    pub fn capture(workspace: &Workspace, preferences: &Preferences) -> Self {
        let mut theme_overrides = BTreeMap::new();
        for (token, color) in &workspace.theme.overrides {
            theme_overrides.insert(token.key().to_string(), color.to_hex());
        }
        Self {
            version: STATE_VERSION,
            dock: workspace.layout_json(),
            theme_preset: workspace.theme.preset.clone(),
            theme_overrides,
            locale: workspace.i18n.locale().code().to_string(),
            preferences: preferences.clone(),
            scene: SceneSettings {
                show_grid: workspace.show_grid,
                show_skybox: workspace.show_skybox,
                gizmo_space: match workspace.gizmo_space {
                    crate::viewport_math::GizmoSpace::Local => "Local".to_string(),
                    crate::viewport_math::GizmoSpace::World => "World".to_string(),
                },
                gizmo_tool: "Translate".to_string(),
                render: None,
            },
            maximized: workspace.maximized_tab.clone(),
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn to_json_string(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// 宽容读取：字段缺失用默认值，版本不符也尽量恢复已知字段。
    pub fn from_json(value: &Value) -> Self {
        let mut state: Self =
            serde_json::from_value(value.clone()).unwrap_or_else(|_| Self::default());
        state.sanitize();
        state
    }

    pub fn from_json_str(text: &str) -> Self {
        match serde_json::from_str::<Value>(text) {
            Ok(value) => Self::from_json(&value),
            Err(_) => Self::default(),
        }
    }

    /// 应用到一个工作区（布局 / 主题 / 语言 / 首选项）。
    pub fn apply(&self, workspace: &mut Workspace, preferences: &mut Preferences) {
        let known = workspace.known_panels();
        let layout = DockLayout::from_json(&self.dock, &known, 1600.0, 900.0);
        workspace.dock = layout.dock;
        workspace.floating = layout.floating;

        workspace.theme.preset = if THEME_PRESETS
            .iter()
            .any(|preset| preset.id == self.theme_preset)
        {
            self.theme_preset.clone()
        } else {
            DEFAULT_THEME_PRESET.to_string()
        };
        workspace.theme.overrides.clear();
        for (key, hex) in &self.theme_overrides {
            let Some(token) = ThemeToken::ALL
                .iter()
                .find(|token| token.key() == key.as_str())
            else {
                continue;
            };
            if hex.len() == 7 && hex.starts_with('#') {
                workspace
                    .theme
                    .overrides
                    .insert(*token, Rgba::from_hex(hex));
            }
        }

        workspace.i18n.set_locale(Locale::parse(&self.locale));
        workspace.show_grid = self.scene.show_grid;
        workspace.show_skybox = self.scene.show_skybox;
        workspace.gizmo_space = match self.scene.gizmo_space.as_str() {
            "Local" | "local" => crate::viewport_math::GizmoSpace::Local,
            _ => crate::viewport_math::GizmoSpace::World,
        };
        // 面板可能已经被删掉（旧存档 / 面板改名），指向不存在的 tab 会让整个
        // dock 区空白，所以这里按当前 dock 过滤一次。
        let maximized = self.maximized.clone().filter(|tab| {
            crate::dock::collect_tabs(&workspace.dock)
                .iter()
                .any(|t| t == tab)
        });
        workspace.maximized_tab = maximized;

        *preferences = self.preferences.clone();
        preferences.max_log_lines = preferences.max_log_lines.clamp(500.0, 20_000.0);
    }

    fn sanitize(&mut self) {
        if self.theme_preset.is_empty() {
            self.theme_preset = DEFAULT_THEME_PRESET.to_string();
        }
        if !THEME_PRESETS
            .iter()
            .any(|preset| preset.id == self.theme_preset)
        {
            self.theme_preset = DEFAULT_THEME_PRESET.to_string();
        }
        self.theme_overrides.retain(|key, hex| {
            ThemeToken::ALL
                .iter()
                .any(|token| token.key() == key.as_str())
                && hex.len() == 7
                && hex.starts_with('#')
                && hex[1..].chars().all(|ch| ch.is_ascii_hexdigit())
        });
        if self.locale.is_empty() {
            self.locale = default_locale();
        }
        if !matches!(self.scene.gizmo_space.as_str(), "World" | "Local") {
            self.scene.gizmo_space = "World".to_string();
        }
        if !matches!(
            self.scene.gizmo_tool.as_str(),
            "Translate" | "Rotate" | "Scale"
        ) {
            self.scene.gizmo_tool = "Translate".to_string();
        }
        self.preferences.max_log_lines = self.preferences.max_log_lines.clamp(500.0, 20_000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dock::{DockEdge, DockZone};
    use serde_json::json;

    fn workspace() -> Workspace {
        Workspace::sample()
    }

    #[test]
    fn maximized_panel_survives_a_state_round_trip() {
        let mut source = workspace();
        source.toggle_maximized("sceneView");

        let state = EditorState::capture(&source, &Preferences::default());
        let mut target = Workspace::sample();
        let mut prefs = Preferences::default();
        state.apply(&mut target, &mut prefs);

        assert_eq!(target.maximized_tab.as_deref(), Some("sceneView"));

        // 存档里的面板已经不存在（改名 / 布局变了）：不能把整个 dock 区变成空白。
        let mut stale = state.clone();
        stale.maximized = Some("no-such-panel".to_string());
        stale.apply(&mut target, &mut prefs);
        assert_eq!(target.maximized_tab, None);
    }

    #[test]
    fn roundtrip_layout_theme_locale_and_preferences() {
        let mut source = workspace();
        source.toggle_panel("log");
        let inspector = source.leaf_of("inspector").unwrap();
        source.dock_tab("log", &inspector, DockZone::Center);
        source.float_panel("preferences", 120.0, 140.0);
        source.set_theme_preset("paper");
        source
            .theme
            .overrides
            .insert(ThemeToken::Accent, Rgba::from_hex("#123456"));
        source.set_locale(Locale::EnUs);
        let mut prefs = Preferences {
            autosave: false,
            compact: true,
            max_log_lines: 1234.0,
            ..Default::default()
        };

        let state = EditorState::capture(&source, &prefs);
        let text = state.to_json_string();
        let restored = EditorState::from_json_str(&text);

        let mut target = workspace();
        let mut target_prefs = Preferences::default();
        restored.apply(&mut target, &mut target_prefs);

        assert_eq!(target.dock, source.dock);
        assert_eq!(target.floating, source.floating);
        assert_eq!(target.theme.preset, "paper");
        assert_eq!(
            target.theme.overrides.get(&ThemeToken::Accent),
            Some(&Rgba::from_hex("#123456"))
        );
        assert_eq!(target.i18n.locale(), Locale::EnUs);
        assert!(!target_prefs.autosave);
        assert!(target_prefs.compact);
        assert_eq!(target_prefs.max_log_lines, 1234.0);
        prefs.compact = true;
    }

    #[test]
    fn scene_settings_survive_roundtrip_and_old_state_uses_defaults() {
        let mut source = workspace();
        source.show_grid = false;
        source.show_skybox = false;
        source.gizmo_space = crate::viewport_math::GizmoSpace::Local;
        let mut state = EditorState::capture(&source, &Preferences::default());
        state.scene.gizmo_tool = "Scale".to_string();
        state.scene.render = Some(json!({ "tonemapMode": 1, "pathTrace": true }));

        let restored = EditorState::from_json_str(&state.to_json_string());
        assert!(!restored.scene.show_grid);
        assert!(!restored.scene.show_skybox);
        assert_eq!(restored.scene.gizmo_space, "Local");
        assert_eq!(restored.scene.gizmo_tool, "Scale");
        assert!(restored.scene.render.is_some());

        let mut target = workspace();
        let mut prefs = Preferences::default();
        restored.apply(&mut target, &mut prefs);
        assert!(!target.show_grid);
        assert!(!target.show_skybox);
        assert_eq!(target.gizmo_space, crate::viewport_math::GizmoSpace::Local);

        // 旧存档没有 `scene` 段：走默认值，不能读失败。
        let old = json!({ "version": 1, "dock": {}, "themePreset": "studio" });
        let old_state = EditorState::from_json(&old);
        assert!(old_state.scene.show_grid);
        assert_eq!(old_state.scene.gizmo_tool, "Translate");
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        let value = json!({
            "version": 99,
            "dock": { "version": 4, "dock": { "type": "leaf", "tabs": ["ghost"], "active": "ghost" }, "floating": [] },
            "themePreset": "not-a-theme",
            "themeOverrides": { "accent": "salmon", "nope": "#ffffff", "bg": "#00ff00" },
            "locale": "fr-FR",
            "preferences": { "maxLogLines": 999999.0 }
        });
        let state = EditorState::from_json(&value);
        assert_eq!(state.theme_preset, DEFAULT_THEME_PRESET);
        assert_eq!(state.theme_overrides.len(), 1, "只留下合法 token + 颜色");
        assert!(state.theme_overrides.contains_key("bg"));
        assert_eq!(state.preferences.max_log_lines, 20_000.0);

        let mut workspace = workspace();
        let mut prefs = Preferences::default();
        state.apply(&mut workspace, &mut prefs);
        // 未知面板被丢弃，回退到默认布局（至少有 sceneView）。
        assert!(workspace.dock_tabs().iter().any(|tab| tab == "sceneView"));
        assert_eq!(workspace.i18n.locale(), Locale::ZhCn);
    }

    #[test]
    fn corrupt_json_returns_defaults() {
        let state = EditorState::from_json_str("{ not json");
        assert_eq!(state.version, STATE_VERSION);
        assert_eq!(state.theme_preset, DEFAULT_THEME_PRESET);
        assert!(state.preferences.autosave);
    }

    #[test]
    fn state_json_shape_is_stable() {
        let state = EditorState::capture(&workspace(), &Preferences::default());
        let value = state.to_json();
        assert_eq!(value["version"], STATE_VERSION);
        assert!(value["dock"]["dock"].is_object());
        assert_eq!(value["themePreset"], "studio");
        assert_eq!(value["locale"], "zh-CN");
        assert!(value["preferences"]["maxLogLines"].is_number());
    }

    #[test]
    fn edge_dock_and_float_survive_roundtrip() {
        let mut source = workspace();
        source.dock_tab_at_edge("log", DockEdge::Right);
        source.float_panel("build", 300.0, 200.0);
        let state = EditorState::capture(&source, &Preferences::default());
        let restored = EditorState::from_json_str(&state.to_json_string());
        let mut target = workspace();
        let mut prefs = Preferences::default();
        restored.apply(&mut target, &mut prefs);
        assert_eq!(target.dock, source.dock);
        assert_eq!(target.floating, source.floating);
    }
}
