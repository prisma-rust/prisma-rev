//! 给 agent 看的 UI 节点树（类似浏览器 MCP 的 DOM / 可访问性快照）。
//!
//! egui 没有 DOM，也没有稳定到可以外部点击的控件句柄，所以这里不猜坐标：节点树
//! 直接来自**数据模型**（`Workspace` / `UiState`）+ 每帧记录的 dock 面板矩形。
//! 每个节点有稳定的 `id`，`ui.click` / `ui.set` 按 id 走和界面完全相同的代码路径
//! （`Command` / `Workspace::select` / `Workspace::set_field`），不注入合成鼠标事件。
//!
//! 节点 id 约定：
//!
//! | 前缀 | 含义 | 可点击 | 可设值 |
//! |---|---|---|---|
//! | `command:<Name>` | `Command` 枚举成员（`Command::ALL`） | ✅ | — |
//! | `panel:<tab>` | dock 里的面板（`sceneView` / `hierarchy` / …） | ✅ | — |
//! | `doc:<index>` | 打开的文档 | ✅ | — |
//! | `entity:<id>` | 层级树实体 | ✅ | — |
//! | `asset:<path>` | 资产面板条目 | ✅ | — |
//! | `field:<entity>:<component>:<path>` | 检查器字段 | — | ✅ |
//! | `resolution:<WxH>` / `resolution:fit:<WxH>` / `resolution:match` | Game 视图预览分辨率 / 宽高比（#463/#585） | ✅ | — |

use std::collections::BTreeMap;

use prism_editor_core::{Command, Workspace};
use serde_json::{json, Value};

/// 快照需要的运行时状态（都是 `EditorApp` 的字段，单独传进来便于测试）。
pub struct SnapshotInput<'a> {
    pub workspace: &'a Workspace,
    pub playing: bool,
    pub paused: bool,
    pub status: &'a str,
    pub frames: u64,
    pub fps: f32,
    pub gizmo_tool: Option<&'a str>,
    pub gizmo_space: &'a str,
    /// `panel_body` 每帧记录的 tab → 屏幕矩形。
    pub rects: &'a BTreeMap<String, egui::Rect>,
    /// leaf id → 当前激活的 tab。
    pub leaves: &'a [(String, String, Vec<String>)],
    /// Console 里当前可见的日志行（已按用户过滤），最多用前 50 条。
    pub logs: &'a [String],
    /// Scene / Game 视口里的物体（世界 → 视口像素投影已完成）。
    pub viewports: &'a [ViewObjects],
}

/// 视口里的一个物体：世界坐标 + 投影后的视口像素坐标。
///
/// 节点 id 是 `view:<tab>:entity:<id>`（见 [`view_object_node`]），
/// `ui.click` 接受它，点击落在投影点上——和用户在视口里点同一像素走同一条拾取路径。
#[derive(Debug, Clone, PartialEq)]
pub struct ViewObject {
    /// 节点 id（`view:<tab>:entity:<scene id>` 或 `view:<tab>:ecs:<index>`）。
    ///
    /// 工程代码 spawn 出来的实体没有 `.scene.json` 的稳定 id，用 ECS 索引兜底，
    /// 两类前缀不会撞在一起。
    pub node: String,
    /// 场景文档里的稳定 id；`None` = 运行时 spawn 的实体。
    pub entity: Option<u32>,
    pub name: String,
    pub world: [f32; 3],
    /// 投影后的视口局部像素（左上原点）；`None` = 在相机背后。
    pub screen: Option<[f32; 2]>,
    /// 视空间深度（越大越远）；`None` 与 `screen` 一起为 `None`。
    pub depth: Option<f32>,
    /// 是否参与渲染（Scene：`Active`；Game：`ActiveInHierarchy`）。
    pub visible: bool,
}

/// 一个视口的物体列表。
#[derive(Debug, Clone, PartialEq)]
pub struct ViewObjects {
    /// 面板 tab 名：`sceneView` / `gameView`。
    pub tab: &'static str,
    pub objects: Vec<ViewObject>,
}

/// 视口物体的节点 id（场景实体）。
pub fn view_object_node(tab: &str, entity: u32) -> String {
    format!("view:{tab}:entity:{entity}")
}

/// 视口物体的节点 id（运行时 spawn 的实体，按 ECS 索引）。
pub fn view_object_node_ecs(tab: &str, index: u32) -> String {
    format!("view:{tab}:ecs:{index}")
}

/// `view:<tab>:entity:<id>` / `view:<tab>:ecs:<index>` → `tab`。
///
/// 只接受这两种 kind，避免把任意 `view:*` 节点当成物体。
pub fn parse_view_object_node(node: &str) -> Option<&str> {
    let rest = node.strip_prefix("view:")?;
    let mut parts = rest.split(':');
    let tab = parts.next()?;
    let kind = parts.next()?;
    let key = parts.next()?;
    if parts.next().is_some()
        || tab.is_empty()
        || (kind != "entity" && kind != "ecs")
        || key.parse::<u32>().is_err()
    {
        return None;
    }
    Some(tab)
}

/// 把一个视口的物体转成快照里的 `objects` 数组。
fn view_objects_json(_tab: &str, objects: &[ViewObject]) -> Value {
    Value::Array(
        objects
            .iter()
            .map(|object| {
                let mut node = json!({
                    "id": object.node,
                    "role": "viewObject",
                    "entity": object.entity,
                    "name": object.name,
                    "world": object.world,
                    "visible": object.visible,
                    "on_screen": object.screen.is_some(),
                });
                if let Some(screen) = object.screen {
                    node["screen"] = json!(screen);
                }
                if let Some(depth) = object.depth {
                    node["depth"] = json!(depth);
                }
                node
            })
            .collect(),
    )
}

/// 整个编辑器界面的节点树。
pub fn snapshot(input: &SnapshotInput<'_>) -> Value {
    let workspace = input.workspace;
    let mut root = json!({
        "role": "editor",
        "status": input.status,
        "frames": input.frames,
        "fps": input.fps,
        "playing": input.playing,
        "paused": input.paused,
        "gizmo": {
            "tool": input.gizmo_tool,
            "space": input.gizmo_space,
        },
    });

    let mut dock = Vec::new();
    for (leaf, active, tabs) in input.leaves {
        for tab in tabs {
            let mut node = json!({
                "id": format!("panel:{tab}"),
                "role": "panel",
                "label": panel_title(tab),
                "leaf": leaf,
                "tabs": tabs,
                "active": tab == active,
            });
            if let Some(rect) = input.rects.get(tab) {
                node["bounds"] = json!([rect.min.x, rect.min.y, rect.width(), rect.height()]);
            }
            if tab == active {
                node["children"] = panel_children(input, tab);
            }
            if let Some(objects) = input.viewports.iter().find(|view| view.tab == tab.as_str()) {
                node["objects"] = view_objects_json(tab, &objects.objects);
            }
            dock.push(node);
        }
    }
    root["dock"] = Value::Array(dock);

    root["documents"] = Value::Array(
        workspace
            .docs
            .iter()
            .enumerate()
            .map(|(index, doc)| {
                json!({
                    "id": format!("doc:{index}"),
                    "role": "document",
                    "label": doc
                        .path
                        .as_deref()
                        .map(file_name)
                        .unwrap_or_else(|| format!("untitled:{index}")),
                    "path": doc.path,
                    "dirty": doc.dirty,
                    "active": index == workspace.active_index(),
                })
            })
            .collect(),
    );

    root["assets"] = asset_tree(input);
    root["commands"] = Value::Array(
        Command::ALL
            .iter()
            .map(|command| json!(command_name(*command)))
            .collect(),
    );
    root
}

/// 面板内容节点（只有激活的 tab 展开）。
fn panel_children(input: &SnapshotInput<'_>, tab: &str) -> Value {
    let workspace = input.workspace;
    match tab {
        "hierarchy" => Value::Array(hierarchy_nodes(workspace)),
        "inspector" => Value::Array(inspector_components(workspace)),
        "docs" => Value::Array(
            workspace
                .docs
                .iter()
                .enumerate()
                .map(|(index, doc)| {
                    json!({
                        "id": format!("doc:{index}"),
                        "role": "document",
                        "label": doc.path.as_deref().map(file_name).unwrap_or_default(),
                        "dirty": doc.dirty,
                        "active": index == workspace.active_index(),
                    })
                })
                .collect(),
        ),
        "log" => Value::Array(
            input
                .logs
                .iter()
                .rev()
                .take(50)
                .map(|line| json!({ "role": "log", "message": line }))
                .collect(),
        ),
        _ => Value::Array(Vec::new()),
    }
}

/// 层级树（按文档顺序，带 `depth`）。
fn hierarchy_nodes(workspace: &Workspace) -> Vec<Value> {
    let doc = &workspace.document().doc;
    let mut out = Vec::new();
    for (index, entity) in doc.entities.iter().enumerate() {
        let id = entity.id.unwrap_or(index as u32);
        out.push(json!({
            "id": format!("entity:{id}"),
            "role": "entity",
            "label": entity.display_name(),
            "parent": entity.parent,
            "selected": workspace.selection.contains(&index),
            "components": entity.components.keys().cloned().collect::<Vec<_>>(),
        }));
    }
    out
}

/// 选中实体的组件字段（可 `ui.set` 的路径）。
fn inspector_components(workspace: &Workspace) -> Vec<Value> {
    let doc = &workspace.document().doc;
    let mut out = Vec::new();
    for index in workspace.selection.iter().take(4) {
        let Some(entity) = doc.entities.get(*index) else {
            continue;
        };
        let entity_id = entity.id.unwrap_or(*index as u32);
        let mut components = Vec::new();
        components.push(component_node(
            entity_id,
            "Transform",
            &serde_json::to_value(&entity.transform).unwrap_or(Value::Null),
        ));
        for (name, value) in entity.components.iter() {
            components.push(component_node(entity_id, name, value));
        }
        out.push(json!({
            "id": format!("entity:{entity_id}"),
            "role": "entity",
            "label": entity.display_name(),
            "entity": entity_id,
            "children": components,
        }));
    }
    out
}

fn component_node(entity_id: u32, name: &str, value: &Value) -> Value {
    let mut fields = Vec::new();
    flatten(value, "", &mut |path, value| {
        fields.push(json!({
            "id": format!("field:{entity_id}:{name}:{path}"),
            "role": "field",
            "path": path,
            "value": value,
        }));
    });
    json!({
        "id": format!("component:{entity_id}:{name}"),
        "role": "component",
        "label": name,
        "children": fields,
    })
}

/// 把嵌套对象摊平成字段路径。
///
/// 对象用点号（`position.x`），数组**同时**给出整体值和下标
/// （`position` 是 `[x,y,z]`，另有 `position[0]` …），这样 agent 既能整组替换
/// 也能改单个分量（`prism_editor_core::schema::parse_path` 认 `a.b[2]`）。
fn flatten(value: &Value, prefix: &str, push: &mut impl FnMut(String, Value)) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(child, &path, push);
            }
        }
        Value::Array(items) => {
            push(prefix.to_string(), value.clone());
            for (index, child) in items.iter().enumerate() {
                if child.is_array() || child.is_object() {
                    flatten(child, &format!("{prefix}[{index}]"), push);
                } else {
                    push(format!("{prefix}[{index}]"), child.clone());
                }
            }
        }
        other => push(prefix.to_string(), other.clone()),
    }
}

fn asset_tree(input: &SnapshotInput<'_>) -> Value {
    let workspace = input.workspace;
    let assets = workspace
        .filtered_assets()
        .into_iter()
        .map(|asset| {
            json!({
                "id": format!("asset:{}", asset.path),
                "role": "asset",
                "label": file_name(&asset.path),
                "path": asset.path,
                "kind": format!("{:?}", asset.kind).to_ascii_lowercase(),
            })
        })
        .collect::<Vec<_>>();
    json!({ "role": "panel", "children": assets })
}

fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

/// `panel:<tab>` → 界面上的标题。
pub fn panel_title(tab: &str) -> &'static str {
    match tab {
        "sceneView" => "Scene",
        "gameView" => "Game",
        "hierarchy" => "Hierarchy",
        "inspector" => "Inspector",
        "assets" => "Assets",
        "docs" => "Documents",
        "log" => "Console",
        "history" => "History",
        "renderGraph" => "Render Graph",
        "build" => "Build Settings",
        "problems" => "Problems",
        "prefabOverrides" => "Prefab Overrides",
        "material" => "Material",
        "tools" => "Offline Tools",
        "settings" => "Project Settings",
        "preferences" => "Preferences",
        "theme" => "Theme",
        _ => "Panel",
    }
}

/// `Command` 的稳定名字（和 `Debug` 一致，agent 用这个名字点击）。
pub fn command_name(command: Command) -> String {
    format!("{command:?}")
}

/// 名字 → `Command`（`ui.click` 的 `command:<Name>`）。
pub fn command_by_name(name: &str) -> Option<Command> {
    Command::ALL
        .iter()
        .copied()
        .find(|command| command_name(*command) == name)
}

/// 暴露给 `ui.set` 的错误提示：列出组件里可写的字段路径。
pub fn flatten_paths(value: &Value, prefix: &str, push: &mut impl FnMut(&str, &Value)) {
    flatten(value, prefix, &mut |path, value| push(&path, &value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_produces_dotted_paths() {
        let value = json!({ "position": { "x": 1.0, "y": 2 }, "tags": [1, 2] });
        let mut paths = Vec::new();
        flatten(&value, "", &mut |path, value| paths.push((path, value)));
        let names: Vec<&str> = paths.iter().map(|(path, _)| path.as_str()).collect();
        assert!(names.contains(&"position.x"));
        assert!(names.contains(&"position.y"));
        assert!(names.contains(&"tags"));
        assert!(names.contains(&"tags[0]"));
    }

    #[test]
    fn command_names_round_trip() {
        for command in Command::ALL {
            assert_eq!(command_by_name(&command_name(*command)), Some(*command));
        }
        assert_eq!(command_by_name("Nope"), None);
    }

    #[test]
    fn panel_titles_are_stable() {
        assert_eq!(panel_title("sceneView"), "Scene");
        assert_eq!(panel_title("hierarchy"), "Hierarchy");
    }

    #[test]
    fn snapshot_includes_gizmo_tool_and_space() {
        let workspace = Workspace::sample();
        let rects = BTreeMap::new();
        let leaves = Vec::new();
        let logs = Vec::new();
        let input = SnapshotInput {
            workspace: &workspace,
            playing: false,
            paused: false,
            status: "ready",
            frames: 3,
            fps: 60.0,
            gizmo_tool: Some("Move"),
            gizmo_space: "World",
            rects: &rects,
            leaves: &leaves,
            logs: &logs,
            viewports: &[],
        };
        let value = snapshot(&input);
        assert_eq!(value["gizmo"]["tool"], "Move");
        assert_eq!(value["gizmo"]["space"], "World");
        assert_eq!(value["fps"], 60.0);
    }

    #[test]
    fn view_object_nodes_round_trip() {
        assert_eq!(view_object_node("sceneView", 2), "view:sceneView:entity:2");
        assert_eq!(view_object_node_ecs("gameView", 9), "view:gameView:ecs:9");
        assert_eq!(
            parse_view_object_node("view:gameView:entity:17"),
            Some("gameView")
        );
        assert_eq!(
            parse_view_object_node("view:gameView:ecs:17"),
            Some("gameView")
        );
        assert_eq!(parse_view_object_node("panel:sceneView"), None);
        assert_eq!(parse_view_object_node("view:sceneView:entity:x"), None);
        assert_eq!(parse_view_object_node("view:sceneView"), None);
        assert_eq!(parse_view_object_node("view::entity:1"), None);
        assert_eq!(parse_view_object_node("view:sceneView:created:1"), None);
    }

    #[test]
    fn snapshot_lists_viewport_objects() {
        let workspace = Workspace::sample();
        let mut rects = BTreeMap::new();
        rects.insert(
            "sceneView".to_string(),
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0)),
        );
        let leaves = vec![(
            "leaf-1".to_string(),
            "sceneView".to_string(),
            vec!["sceneView".to_string()],
        )];
        let logs = Vec::new();
        let viewports = vec![ViewObjects {
            tab: "sceneView",
            objects: vec![ViewObject {
                node: view_object_node("sceneView", 2),
                entity: Some(2),
                name: "boat".into(),
                world: [13.0, 80.0, 0.0],
                screen: Some([400.0, 300.0]),
                depth: Some(5.0),
                visible: true,
            }],
        }];
        let input = SnapshotInput {
            workspace: &workspace,
            playing: false,
            paused: false,
            status: "ready",
            frames: 1,
            fps: 60.0,
            gizmo_tool: None,
            gizmo_space: "World",
            rects: &rects,
            leaves: &leaves,
            logs: &logs,
            viewports: &viewports,
        };
        let value = snapshot(&input);
        let node = value["dock"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "panel:sceneView")
            .expect("scene view node");
        let objects = node["objects"].as_array().expect("objects array");
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0]["id"], "view:sceneView:entity:2");
        assert_eq!(objects[0]["name"], "boat");
        assert_eq!(objects[0]["screen"], serde_json::json!([400.0, 300.0]));
        assert_eq!(objects[0]["on_screen"], true);
    }
}
