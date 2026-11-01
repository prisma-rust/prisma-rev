//! 命令表与快捷键（`App.vue` 的 `MENUS` + 快捷键面板的等价物）。
//!
//! 所有菜单项都对应一个 `Command`；`SHORTCUTS` 是「命令 → 默认键位」的单一事实源，
//! 菜单里显示的提示与按键分发用的是同一张表。

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Command {
    // 文件
    NewScene,
    NewPrefab,
    OpenDocument,
    Save,
    SaveAs,
    RenameDocument,
    DeleteDocument,
    ReloadDocument,
    // 编辑
    Undo,
    Redo,
    DuplicateEntity,
    DeleteSelection,
    SelectAll,
    CopyEntity,
    CutEntity,
    PasteEntity,
    PasteComponent,
    ClearComponentClipboard,
    Preferences,
    ProjectSettings,
    ThemePanel,
    // 构建
    BuildProject,
    CookScenes,
    OfflineTools,
    // 运行
    Play,
    Pause,
    Stop,
    Step,
    // 窗口
    ToggleSceneView,
    ToggleGameView,
    ToggleDocs,
    ToggleHierarchy,
    ToggleAssets,
    ToggleInspector,
    ToggleLog,
    ToggleRenderGraph,
    ToggleBuild,
    ToggleTools,
    ToggleHistory,
    ResetLayout,
    // 帮助
    Shortcuts,
    About,
    // 视口
    FrameAll,
    FrameSelection,
    ResetSceneCamera,
}

impl Command {
    pub const ALL: &'static [Command] = &[
        Command::NewScene,
        Command::NewPrefab,
        Command::OpenDocument,
        Command::Save,
        Command::SaveAs,
        Command::RenameDocument,
        Command::DeleteDocument,
        Command::ReloadDocument,
        Command::Undo,
        Command::Redo,
        Command::DuplicateEntity,
        Command::DeleteSelection,
        Command::SelectAll,
        Command::CopyEntity,
        Command::CutEntity,
        Command::PasteEntity,
        Command::PasteComponent,
        Command::ClearComponentClipboard,
        Command::Preferences,
        Command::ProjectSettings,
        Command::ThemePanel,
        Command::BuildProject,
        Command::CookScenes,
        Command::OfflineTools,
        Command::Play,
        Command::Pause,
        Command::Stop,
        Command::Step,
        Command::ToggleSceneView,
        Command::ToggleGameView,
        Command::ToggleDocs,
        Command::ToggleHierarchy,
        Command::ToggleAssets,
        Command::ToggleInspector,
        Command::ToggleLog,
        Command::ToggleRenderGraph,
        Command::ToggleBuild,
        Command::ToggleTools,
        Command::ToggleHistory,
        Command::ResetLayout,
        Command::Shortcuts,
        Command::About,
        Command::FrameAll,
        Command::FrameSelection,
        Command::ResetSceneCamera,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Command::NewScene => "file/new",
            Command::NewPrefab => "file/new-prefab",
            Command::OpenDocument => "file/open",
            Command::Save => "file/save",
            Command::SaveAs => "file/save-as",
            Command::RenameDocument => "file/rename",
            Command::DeleteDocument => "file/delete",
            Command::ReloadDocument => "file/reload",
            Command::Undo => "edit/undo",
            Command::Redo => "edit/redo",
            Command::DuplicateEntity => "edit/duplicate",
            Command::DeleteSelection => "edit/delete",
            Command::SelectAll => "edit/select-all",
            Command::CopyEntity => "edit/copy",
            Command::CutEntity => "edit/cut",
            Command::PasteEntity => "edit/paste",
            Command::PasteComponent => "edit/paste-component",
            Command::ClearComponentClipboard => "edit/clear-component-clipboard",
            Command::Preferences => "edit/preferences",
            Command::ProjectSettings => "edit/project-settings",
            Command::ThemePanel => "edit/theme",
            Command::BuildProject => "build/project",
            Command::CookScenes => "build/cook",
            Command::OfflineTools => "build/tools",
            Command::Play => "play/start",
            Command::Pause => "play/pause",
            Command::Stop => "play/stop",
            Command::Step => "play/step",
            Command::ToggleSceneView => "window/toggle/sceneView",
            Command::ToggleGameView => "window/toggle/gameView",
            Command::ToggleDocs => "window/toggle/docs",
            Command::ToggleHierarchy => "window/toggle/hierarchy",
            Command::ToggleAssets => "window/toggle/assets",
            Command::ToggleInspector => "window/toggle/inspector",
            Command::ToggleLog => "window/toggle/log",
            Command::ToggleRenderGraph => "window/toggle/renderGraph",
            Command::ToggleBuild => "window/toggle/build",
            Command::ToggleTools => "window/toggle/tools",
            Command::ToggleHistory => "window/toggle/history",
            Command::ResetLayout => "window/reset-layout",
            Command::Shortcuts => "help/shortcuts",
            Command::About => "help/about",
            Command::FrameAll => "viewport/frame-all",
            Command::FrameSelection => "viewport/frame-selection",
            Command::ResetSceneCamera => "viewport/reset-scene-camera",
        }
    }

    /// i18n key（`menu.*` / `toolbar.*` / `shortcuts.*`）。
    pub fn label_key(self) -> &'static str {
        match self {
            Command::NewScene => "menu.newScene",
            Command::NewPrefab => "menu.newPrefab",
            Command::OpenDocument => "menu.openDocument",
            Command::Save => "menu.save",
            Command::SaveAs => "menu.saveAs",
            Command::RenameDocument => "hierarchy.rename",
            Command::DeleteDocument => "common.delete",
            Command::ReloadDocument => "common.reload",
            Command::Undo => "menu.undo",
            Command::Redo => "menu.redo",
            Command::DuplicateEntity => "common.duplicate",
            Command::DeleteSelection => "common.delete",
            Command::SelectAll => "common.all",
            Command::CopyEntity => "hierarchy.copy",
            Command::CutEntity => "hierarchy.cut",
            Command::PasteEntity => "hierarchy.paste",
            Command::PasteComponent => "common.paste",
            Command::ClearComponentClipboard => "common.clear",
            Command::Preferences => "menu.preferences",
            Command::ProjectSettings => "menu.projectSettings",
            Command::ThemePanel => "menu.theme",
            Command::BuildProject => "menu.buildProject",
            Command::CookScenes => "menu.buildScenes",
            Command::OfflineTools => "menu.offlineTools",
            Command::Play => "common.run",
            Command::Pause => "common.pause",
            Command::Stop => "common.stop",
            Command::Step => "common.step",
            Command::ToggleSceneView => "menu.sceneView",
            Command::ToggleGameView => "menu.gameView",
            Command::ToggleDocs => "menu.docs",
            Command::ToggleHierarchy => "menu.hierarchy",
            Command::ToggleAssets => "menu.assets",
            Command::ToggleInspector => "menu.inspector",
            Command::ToggleLog => "menu.log",
            Command::ToggleRenderGraph => "menu.renderGraph",
            Command::ToggleBuild => "menu.buildPanel",
            Command::ToggleTools => "menu.tools",
            Command::ToggleHistory => "menu.history",
            Command::ResetLayout => "common.reset",
            Command::Shortcuts => "menu.shortcuts",
            Command::About => "menu.about",
            Command::FrameAll => "viewport.frameAll",
            Command::FrameSelection => "viewport.focusSelection",
            Command::ResetSceneCamera => "viewport.resetSceneCamera",
        }
    }

    pub fn default_shortcut(self) -> Option<&'static str> {
        SHORTCUTS
            .iter()
            .find(|entry| entry.command == self)
            .map(|entry| entry.keys)
    }

    /// 面板命令（`window/toggle/<id>`）→ 面板 id。
    pub fn panel_id(self) -> Option<&'static str> {
        match self {
            Command::ToggleSceneView => Some("sceneView"),
            Command::ToggleGameView => Some("gameView"),
            Command::ToggleDocs => Some("docs"),
            Command::ToggleHierarchy => Some("hierarchy"),
            Command::ToggleAssets => Some("assets"),
            Command::ToggleInspector => Some("inspector"),
            Command::ToggleLog => Some("log"),
            Command::ToggleRenderGraph => Some("renderGraph"),
            Command::ToggleBuild => Some("build"),
            Command::ToggleTools => Some("tools"),
            Command::ToggleHistory => Some("history"),
            Command::Preferences => Some("preferences"),
            Command::ProjectSettings => Some("projectSettings"),
            Command::ThemePanel => Some("theme"),
            Command::Shortcuts => Some("shortcuts"),
            Command::About => Some("about"),
            _ => None,
        }
    }
}

pub struct ShortcutEntry {
    pub command: Command,
    pub keys: &'static str,
}

pub const SHORTCUTS: &[ShortcutEntry] = &[
    ShortcutEntry {
        command: Command::NewScene,
        keys: "Ctrl+N",
    },
    ShortcutEntry {
        command: Command::OpenDocument,
        keys: "Ctrl+O",
    },
    ShortcutEntry {
        command: Command::Save,
        keys: "Ctrl+S",
    },
    ShortcutEntry {
        command: Command::SaveAs,
        keys: "Ctrl+Shift+S",
    },
    ShortcutEntry {
        command: Command::Undo,
        keys: "Ctrl+Z",
    },
    ShortcutEntry {
        command: Command::Redo,
        keys: "Ctrl+Shift+Z",
    },
    ShortcutEntry {
        command: Command::DuplicateEntity,
        keys: "Ctrl+D",
    },
    ShortcutEntry {
        command: Command::DeleteSelection,
        keys: "Del",
    },
    ShortcutEntry {
        command: Command::SelectAll,
        keys: "Ctrl+A",
    },
    ShortcutEntry {
        command: Command::CopyEntity,
        keys: "Ctrl+C",
    },
    ShortcutEntry {
        command: Command::CutEntity,
        keys: "Ctrl+X",
    },
    ShortcutEntry {
        command: Command::PasteEntity,
        keys: "Ctrl+V",
    },
    ShortcutEntry {
        command: Command::PasteComponent,
        keys: "Ctrl+Shift+V",
    },
    ShortcutEntry {
        command: Command::Play,
        keys: "Ctrl+P",
    },
    ShortcutEntry {
        command: Command::Pause,
        keys: "Ctrl+Shift+P",
    },
    ShortcutEntry {
        command: Command::Stop,
        keys: "Ctrl+Shift+F5",
    },
    ShortcutEntry {
        command: Command::Step,
        keys: "Ctrl+Alt+P",
    },
    ShortcutEntry {
        command: Command::ToggleLog,
        keys: "Ctrl+Shift+L",
    },
    ShortcutEntry {
        command: Command::FrameAll,
        keys: "Home",
    },
    ShortcutEntry {
        command: Command::FrameSelection,
        keys: "F",
    },
    ShortcutEntry {
        command: Command::ResetSceneCamera,
        keys: "Ctrl+0",
    },
    // 面板切换（引擎的 Ctrl+1..5）。
    ShortcutEntry {
        command: Command::ToggleSceneView,
        keys: "Ctrl+1",
    },
    ShortcutEntry {
        command: Command::ToggleGameView,
        keys: "Ctrl+2",
    },
    ShortcutEntry {
        command: Command::ToggleInspector,
        keys: "Ctrl+3",
    },
    ShortcutEntry {
        command: Command::ToggleHierarchy,
        keys: "Ctrl+4",
    },
    ShortcutEntry {
        command: Command::ToggleAssets,
        keys: "Ctrl+5",
    },
];

/// 同一张表里的**额外**绑定：面板只展示 [`SHORTCUTS`] 里的那一条，但这些组合键
/// 同样会触发命令（#516）。
///
/// 放这里的都是「用户肌肉记忆里的第二条路」：`Ctrl+Y` 是 Windows 习惯的重做，
/// `Ctrl+Shift+C` 是 Console 在早期版本里的键位。
pub const SHORTCUT_ALIASES: &[ShortcutEntry] = &[
    ShortcutEntry {
        command: Command::Redo,
        keys: "Ctrl+Y",
    },
    ShortcutEntry {
        command: Command::ToggleLog,
        keys: "Ctrl+Shift+C",
    },
];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyChord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// 规范化后的键名（小写；`delete` / `home` / `f` / `s` …）。
    pub key: String,
}

impl KeyChord {
    pub fn parse(text: &str) -> Option<KeyChord> {
        let mut chord = KeyChord::default();
        let mut key = String::new();
        for part in text.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "shift" => chord.shift = true,
                "alt" | "option" => chord.alt = true,
                other if !other.is_empty() => {
                    if !key.is_empty() {
                        return None;
                    }
                    key = normalize_key(other);
                }
                _ => {}
            }
        }
        if key.is_empty() {
            return None;
        }
        chord.key = key;
        Some(chord)
    }

    /// gpui 的按键事件直接喂进来；`key` 是平台键名（`a` / `Delete` / `Home`）。
    pub fn from_event(ctrl: bool, shift: bool, alt: bool, key: &str) -> KeyChord {
        KeyChord {
            ctrl,
            shift,
            alt,
            key: normalize_key(key),
        }
    }

    pub fn matches(&self, event: &KeyChord) -> bool {
        self.ctrl == event.ctrl
            && self.shift == event.shift
            && self.alt == event.alt
            && self.key == event.key
    }

    /// 菜单里显示的文本（与 `SHORTCUTS` 的写法一致）。
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        parts.push(match self.key.as_str() {
            "delete" => "Del".to_string(),
            "backspace" => "Backspace".to_string(),
            "escape" => "Esc".to_string(),
            "arrowup" => "Up".to_string(),
            "arrowdown" => "Down".to_string(),
            "arrowleft" => "Left".to_string(),
            "arrowright" => "Right".to_string(),
            "enter" => "Enter".to_string(),
            "space" => "Space".to_string(),
            "home" => "Home".to_string(),
            "end" => "End".to_string(),
            "pageup" => "PageUp".to_string(),
            "pagedown" => "PageDown".to_string(),
            "tab" => "Tab".to_string(),
            other => other.to_uppercase(),
        });
        parts.join("+")
    }
}

fn normalize_key(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "del" | "delete" => "delete".to_string(),
        "esc" | "escape" => "escape".to_string(),
        "return" | "enter" => "enter".to_string(),
        "home" | "f" => key.to_ascii_lowercase(),
        other => other.to_string(),
    }
}

/// 按键 → 命令。先匹配完整 chord（含修饰键），再回退到无修饰键的单键动作。
pub fn command_for(event: &KeyChord) -> Option<Command> {
    SHORTCUTS
        .iter()
        .find(|entry| {
            KeyChord::parse(entry.keys)
                .map(|chord| chord.matches(event))
                .unwrap_or(false)
        })
        .map(|entry| entry.command)
}

/// 命令的默认键位（[`SHORTCUTS`] 里那条）。
pub fn default_keys(command: Command) -> Option<&'static str> {
    SHORTCUTS
        .iter()
        .find(|entry| entry.command == command)
        .map(|entry| entry.keys)
}

/// 命令当前生效的键位：用户覆盖优先，其次默认值。
pub fn effective_keys(command: Command, overrides: &BTreeMap<String, String>) -> Option<String> {
    overrides
        .get(command.id())
        .cloned()
        .or_else(|| default_keys(command).map(str::to_string))
}

/// 当前生效的全部绑定（主键 + 别名）。
///
/// 被用户覆盖过的命令**不再使用旧别名**：一个命令只留一个键位，否则改键之后
/// 旧键仍然生效，界面和实际行为对不上。
pub fn effective_bindings(overrides: &BTreeMap<String, String>) -> Vec<(Command, KeyChord)> {
    let mut out = Vec::new();
    for entry in SHORTCUTS {
        if let Some(chord) = effective_keys(entry.command, overrides)
            .as_deref()
            .and_then(KeyChord::parse)
        {
            out.push((entry.command, chord));
        }
    }
    for entry in SHORTCUT_ALIASES {
        if overrides.contains_key(entry.command.id()) {
            continue;
        }
        if let Some(chord) = KeyChord::parse(entry.keys) {
            out.push((entry.command, chord));
        }
    }
    out
}

/// 把 `chord` 绑给 `command` 是否与别的命令冲突（返回占用者）。
pub fn conflicting_command(
    overrides: &BTreeMap<String, String>,
    command: Command,
    chord: &KeyChord,
) -> Option<Command> {
    effective_bindings(overrides)
        .into_iter()
        .find(|(other, existing)| *other != command && existing.matches(chord))
        .map(|(other, _)| other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_aliases() {
        let chord = KeyChord::parse("Ctrl+Shift+Z").unwrap();
        assert!(chord.ctrl && chord.shift && !chord.alt);
        assert_eq!(chord.key, "z");

        assert_eq!(KeyChord::parse("Del").unwrap().key, "delete");
        assert_eq!(KeyChord::parse("Alt+Enter").unwrap().key, "enter");
        assert!(KeyChord::parse("Ctrl+").is_none());
        assert!(KeyChord::parse("Ctrl+a+b").is_none());
    }

    #[test]
    fn matches_events() {
        let chord = KeyChord::parse("Ctrl+Shift+Z").unwrap();
        assert!(chord.matches(&KeyChord::from_event(true, true, false, "Z")));
        assert!(!chord.matches(&KeyChord::from_event(true, false, false, "z")));
    }

    #[test]
    fn resolves_commands() {
        assert_eq!(
            command_for(&KeyChord::from_event(true, false, false, "s")),
            Some(Command::Save)
        );
        assert_eq!(
            command_for(&KeyChord::from_event(false, false, false, "Delete")),
            Some(Command::DeleteSelection)
        );
        assert_eq!(
            command_for(&KeyChord::from_event(false, false, false, "f")),
            Some(Command::FrameSelection)
        );
        assert_eq!(
            command_for(&KeyChord::from_event(false, false, false, "x")),
            None
        );
    }

    #[test]
    fn every_command_has_unique_id_and_label() {
        let mut ids: Vec<&str> = Command::ALL.iter().map(|command| command.id()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "命令 id 必须唯一");

        for command in Command::ALL {
            assert!(!command.label_key().is_empty());
            assert!(!command.id().is_empty());
        }
    }

    #[test]
    fn overrides_replace_default_keys_and_drop_the_old_alias() {
        // #421：改键后旧键不再生效；别名也不该继续触发同一个命令。
        let mut overrides = BTreeMap::new();
        overrides.insert(Command::Play.id().to_string(), "Ctrl+Shift+P".to_string());
        assert_eq!(
            effective_keys(Command::Play, &overrides).as_deref(),
            Some("Ctrl+Shift+P")
        );
        assert_eq!(
            effective_keys(Command::Save, &overrides).as_deref(),
            Some("Ctrl+S")
        );
        let bindings = effective_bindings(&overrides);
        assert!(bindings.iter().any(|(command, chord)| {
            *command == Command::Play && chord.matches(&KeyChord::parse("Ctrl+Shift+P").unwrap())
        }));

        let mut overrides = BTreeMap::new();
        overrides.insert(Command::Redo.id().to_string(), "Ctrl+Alt+Z".to_string());
        let bindings = effective_bindings(&overrides);
        assert!(
            !bindings.iter().any(|(command, chord)| {
                *command == Command::Redo && chord.matches(&KeyChord::parse("Ctrl+Y").unwrap())
            }),
            "覆盖后 Ctrl+Y 这个别名必须失效"
        );
        assert!(bindings.iter().any(|(command, chord)| {
            *command == Command::Redo && chord.matches(&KeyChord::parse("Ctrl+Alt+Z").unwrap())
        }));
    }

    #[test]
    fn conflicts_are_reported_against_the_effective_table() {
        let overrides = BTreeMap::new();
        let save = KeyChord::parse("Ctrl+S").unwrap();
        assert_eq!(
            conflicting_command(&overrides, Command::Play, &save),
            Some(Command::Save)
        );
        // 绑回自己原来的键不算冲突。
        assert_eq!(conflicting_command(&overrides, Command::Save, &save), None);

        // 覆盖表参与判断：把 Play 改成 Ctrl+Shift+P 后，别的命令不能再占它。
        let mut overrides = BTreeMap::new();
        overrides.insert(Command::Play.id().to_string(), "Ctrl+Shift+P".to_string());
        let chord = KeyChord::parse("Ctrl+Shift+P").unwrap();
        assert_eq!(
            conflicting_command(&overrides, Command::Save, &chord),
            Some(Command::Play)
        );
    }

    #[test]
    fn shortcut_table_points_at_real_commands() {
        for entry in SHORTCUTS {
            let chord = KeyChord::parse(entry.keys).unwrap_or_else(|| panic!("bad {}", entry.keys));
            assert_eq!(chord.display(), entry.keys.replace("Del", "Del"));
        }
        // 同一 chord 不能绑两个命令。
        for (index, first) in SHORTCUTS.iter().enumerate() {
            let first_chord = KeyChord::parse(first.keys).unwrap();
            for second in &SHORTCUTS[index + 1..] {
                let second_chord = KeyChord::parse(second.keys).unwrap();
                assert!(
                    !first_chord.matches(&second_chord) || first.command == second.command,
                    "{} 与 {} 冲突",
                    first.keys,
                    second.keys
                );
            }
        }
    }

    #[test]
    fn panel_commands_map_to_panel_ids() {
        assert_eq!(Command::ToggleLog.panel_id(), Some("log"));
        assert_eq!(Command::ToggleHistory.panel_id(), Some("history"));
        assert_eq!(Command::Preferences.panel_id(), Some("preferences"));
        assert_eq!(Command::Save.panel_id(), None);
    }
}
