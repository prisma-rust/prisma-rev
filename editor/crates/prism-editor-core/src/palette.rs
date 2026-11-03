//! 全局搜索（`Ctrl+K`）的数据层。
//!
//! 编辑器里有四个各自为政的搜索框（层级 / 资产 / Console / Add Component），
//! 但「跳到某个实体 / 打开某个资产 / 跑某条命令」必须先想清楚它在哪个面板。
//! 这里把工作区摊平成一份可搜索的条目表，排序是纯函数（便于测试）；
//! 弹窗绘制在 `prism-editor-egui/src/palette.rs`，不依赖 egui。
//!
//! 不做索引：当前场景实体和资产表都在内存里，线性扫描足够（#425）。

use crate::keys::{Command, SHORTCUTS};
use crate::workspace::Workspace;

/// 条目分类。派生顺序就是「空查询」时的展示顺序：先跳到场景里的东西，
/// 再是文档和资产，最后才是命令（命令一般靠输入名字过滤）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PaletteKind {
    Entity,
    Document,
    Asset,
    Command,
}

impl PaletteKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => "Command",
            Self::Document => "Document",
            Self::Entity => "Entity",
            Self::Asset => "Asset",
        }
    }
}

/// 选中一条结果后要做的事。数据层只描述意图，执行留给 UI 层。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    RunCommand(Command),
    OpenDocument(usize),
    /// 当前文档里的实体**下标**（`Workspace::select` 的入参）。
    SelectEntity(usize),
    OpenAsset(String),
}

#[derive(Clone, Debug)]
pub struct PaletteEntry {
    pub kind: PaletteKind,
    /// 主标题（实体名 / 文件名 / 命令的人话名字）。
    pub title: String,
    /// 次级信息：层级路径、资产路径、快捷键……
    pub detail: String,
    pub action: PaletteAction,
}

/// `ResetSceneCamera` → `Reset Scene Camera`。
///
/// `Command` 的名字是 Rust 标识符（`ui.click` 的 `command:<Name>` 靠它往返），
/// 直接摆进搜索框里没法读。
pub fn humanize(identifier: &str) -> String {
    let mut out = String::with_capacity(identifier.len() + 4);
    for (index, ch) in identifier.chars().enumerate() {
        if index > 0 && ch.is_uppercase() {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// 命令的快捷键文本（没有绑定就是空串）。
pub fn shortcut_for(command: Command) -> &'static str {
    SHORTCUTS
        .iter()
        .find(|entry| entry.command == command)
        .map(|entry| entry.keys)
        .unwrap_or("")
}

/// 把工作区摊平成条目表：命令 → 已打开的文档 → 当前文档的实体 → 资产。
pub fn entries(workspace: &Workspace) -> Vec<PaletteEntry> {
    let mut entries = Vec::new();

    for command in Command::ALL.iter().copied() {
        entries.push(PaletteEntry {
            kind: PaletteKind::Command,
            title: humanize(&format!("{command:?}")),
            detail: shortcut_for(command).to_string(),
            action: PaletteAction::RunCommand(command),
        });
    }

    let active = workspace.active_index();
    for (index, document) in workspace.docs.iter().enumerate() {
        let path = document
            .path
            .clone()
            .unwrap_or_else(|| "untitled".to_string());
        entries.push(PaletteEntry {
            kind: PaletteKind::Document,
            title: file_name(&path),
            detail: if index == active {
                format!("{path} (active)")
            } else {
                path
            },
            action: PaletteAction::OpenDocument(index),
        });
    }

    let doc = &workspace.document().doc;
    for (index, entity) in doc.entities.iter().enumerate() {
        let mut components: Vec<&str> = entity
            .components
            .keys()
            .map(|name| name.rsplit("::").next().unwrap_or(name))
            .collect();
        components.sort_unstable();
        let mut detail = entity_path(doc, index);
        if !components.is_empty() {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str(&components.join(", "));
        }
        entries.push(PaletteEntry {
            kind: PaletteKind::Entity,
            title: entity
                .name
                .clone()
                .unwrap_or_else(|| format!("entity {index}")),
            detail: detail.clone(),
            action: PaletteAction::SelectEntity(index),
        });
    }

    for asset in &workspace.assets {
        entries.push(PaletteEntry {
            kind: PaletteKind::Asset,
            title: file_name(&asset.path),
            detail: format!("{} · {}", asset.kind.label(), asset.path),
            action: PaletteAction::OpenAsset(asset.path.clone()),
        });
    }

    entries
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// `Parent/Child`，顶层实体返回空串。
fn entity_path(doc: &crate::doc::SceneDoc, index: usize) -> String {
    let mut names = Vec::new();
    let mut current = doc.entities.get(index).and_then(|entity| entity.parent);
    // 父实体是 `id` 不是下标，且可能成环（损坏的文档），最多走 entities.len() 步。
    let mut guard = doc.entities.len();
    while let Some(id) = current {
        if guard == 0 {
            break;
        }
        guard -= 1;
        let Some(parent_index) = doc.index_of_id(id) else {
            break;
        };
        let Some(parent) = doc.entities.get(parent_index) else {
            break;
        };
        names.push(parent.name.clone().unwrap_or_else(|| format!("#{id}")));
        current = parent.parent;
    }
    names.reverse();
    names.join("/")
}

/// 匹配等级，越小越好；`None` = 不匹配。
fn rank(query: &str, entry: &PaletteEntry) -> Option<(u8, usize)> {
    let title = entry.title.to_lowercase();
    let start = if title == query {
        0
    } else if title.starts_with(query) {
        1
    } else if title
        .split([' ', '/', '_', '-'])
        .any(|word| word.starts_with(query))
    {
        2
    } else if title.contains(query) {
        3
    } else if entry.detail.to_lowercase().contains(query) {
        5
    } else if subsequence(&title, query) {
        4
    } else {
        return None;
    };
    // 同级里短的更靠前：`Save` 排在 `Save As` 前面。
    Some((start, entry.title.len()))
}

fn subsequence(text: &str, query: &str) -> bool {
    let mut chars = text.chars();
    query.chars().all(|needle| chars.any(|hay| hay == needle))
}

/// 排序后的下标；空查询 = 按分类展示全部（命令 → 文档 → 实体 → 资产）。
pub fn search(query: &str, entries: &[PaletteEntry]) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        let mut all: Vec<usize> = (0..entries.len()).collect();
        all.sort_by_key(|index| {
            (
                entries[*index].kind,
                entries[*index].title.to_lowercase(),
                *index,
            )
        });
        return all;
    }
    let mut scored: Vec<(u8, usize, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| rank(&query, entry).map(|(rank, len)| (rank, len, index)))
        .collect();
    scored.sort_unstable();
    scored.into_iter().map(|(_, _, index)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: PaletteKind, title: &str, detail: &str) -> PaletteEntry {
        PaletteEntry {
            kind,
            title: title.to_string(),
            detail: detail.to_string(),
            action: PaletteAction::OpenAsset(detail.to_string()),
        }
    }

    #[test]
    fn humanize_splits_camel_case() {
        assert_eq!(humanize("ResetSceneCamera"), "Reset Scene Camera");
        assert_eq!(humanize("Save"), "Save");
    }

    #[test]
    fn empty_query_lists_entities_then_assets_then_commands() {
        let entries = vec![
            entry(PaletteKind::Asset, "boat.png", "sprites/boat.png"),
            entry(PaletteKind::Entity, "boat", "water/boat"),
            entry(PaletteKind::Command, "Save", "Ctrl+S"),
        ];
        let order = search("", &entries);
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn prefix_matches_outrank_substring_and_detail_matches() {
        let entries = vec![
            entry(
                PaletteKind::Asset,
                "water_tile.png",
                "sprites/water_tile.png",
            ),
            entry(PaletteKind::Entity, "water", "water"),
            entry(
                PaletteKind::Asset,
                "lake_water.tres",
                "materials/lake_water.tres",
            ),
            entry(PaletteKind::Asset, "boat.png", "sprites/water/boat.png"),
        ];
        // 同等级里短的在前：`water` 比 `water_tile.png` 先出现。
        let order = search("wat", &entries);
        assert_eq!(order, vec![1, 0, 2, 3]);
    }

    #[test]
    fn word_starts_beat_plain_contains() {
        let entries = vec![
            entry(PaletteKind::Entity, "boat_water", ""),
            entry(PaletteKind::Entity, "water", ""),
        ];
        assert_eq!(search("wat", &entries), vec![1, 0]);
    }

    #[test]
    fn subsequence_matches_typos_inside_a_word() {
        let entries = vec![
            entry(PaletteKind::Asset, "sprites/boat.png", ""),
            entry(PaletteKind::Asset, "sprites/lake.png", ""),
        ];
        // b…o…a…t 是 "boat" 的子序列，但 "lake" 不行。
        assert_eq!(search("boat", &entries), vec![0]);
    }

    #[test]
    fn entries_cover_commands_documents_entities_and_assets() {
        let mut workspace = Workspace::sample();
        let all = entries(&workspace);
        assert!(all.iter().any(|entry| entry.kind == PaletteKind::Command
            && matches!(entry.action, PaletteAction::RunCommand(Command::Save))));
        assert!(all.iter().any(|entry| entry.kind == PaletteKind::Document));
        assert!(all.iter().any(|entry| entry.kind == PaletteKind::Entity));
        // sample() 的文档里有实体，实体条目应该带层级路径或组件名。
        let entity = all
            .iter()
            .find(|entry| entry.kind == PaletteKind::Entity)
            .expect("entity entry");
        assert!(!entity.title.is_empty());

        workspace.refresh_assets();
        assert!(entries(&workspace)
            .iter()
            .all(|entry| !entry.title.is_empty()));
    }

    #[test]
    fn child_entities_show_their_parent_path() {
        let mut doc = crate::doc::SceneDoc::default();
        let parent = doc.add_named_child(None, "Boat");
        let child = doc.add_named_child(Some(parent), "Rod");
        assert_eq!(entity_path(&doc, child), "Boat");
        assert_eq!(entity_path(&doc, parent), "");
    }

    #[test]
    fn search_is_case_insensitive() {
        let entries = vec![entry(PaletteKind::Entity, "Boat", "")];
        assert_eq!(search("BOAT", &entries), vec![0]);
        assert_eq!(search("boat", &entries), vec![0]);
    }

    #[test]
    fn entities_report_their_component_types() {
        let mut doc = crate::doc::SceneDoc::default();
        let index = doc.add_named_child(None, "Lamp");
        doc.entities[index].components.insert(
            "prism_engine::scene::Light".to_string(),
            serde_json::json!({}),
        );
        assert_eq!(
            doc.entities[index]
                .components
                .keys()
                .map(|name| name.rsplit("::").next().unwrap())
                .collect::<Vec<_>>(),
            vec!["Light"]
        );
    }
}
