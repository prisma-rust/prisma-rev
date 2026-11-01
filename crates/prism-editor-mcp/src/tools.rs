//! 文档 / 反射字段工具：全部落在 `prism-editor-api` 上，这里只做参数解析。
//!
//! 设计约束：**写入一律经过反射**（`set_component_field`），所以非法类型、
//! 只读字段、未知枚举变体都会被拒，不会写坏 `.scene.json`。

use std::path::{Path, PathBuf};

use prism_editor_api::asset_inspect::{inspect_asset, validate_asset};
use prism_editor_api::reflect_edit::set_component_field;
use prism_editor_api::scene::{self, DocKind};
use prism_editor_api::schema::{build_schema, Schema};
use serde_json::{json, Map, Value};

use crate::protocol::ToolDef;

pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "documents_list",
            description: "列出项目里的场景与预制体文档（assets/scenes/*.scene.json、assets/prefabs/*.prefab.json）。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["scene", "prefab", "all"], "default": "all" }
                }
            }),
        },
        ToolDef {
            name: "document_read",
            description: "读取一个文档的完整 JSON（已迁移到当前格式，实体带稳定 id）。",
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "相对项目根的路径" } },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "document_write",
            description: "校验并写回整个文档。字段级修改请优先用 field_set（它带反射校验）。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "object", "description": "{ version, entities }" }
                },
                "required": ["path", "doc"]
            }),
        },
        ToolDef {
            name: "schema_list",
            description: "列出所有已注册组件与嵌套结构体，以及每个字段的 kind / 枚举变体 / 只读标记。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "component": { "type": "string", "description": "只看这一个组件时填" }
                }
            }),
        },
        ToolDef {
            name: "entity_get",
            description: "按 id 或名字取一个实体（含 transform 与全部组件）。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "entity": { "type": "string", "description": "实体 id（数字）或名字" }
                },
                "required": ["path", "entity"]
            }),
        },
        ToolDef {
            name: "field_set",
            description: "用反射写入一个组件字段。field 支持 a.b 与 list[0].x；value 按作者 JSON 形状给（枚举用字符串）。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "entity": { "type": "string" },
                    "component": { "type": "string", "description": "如 prism_engine::scene::SpriteRenderer" },
                    "field": { "type": "string", "description": "字段路径，如 color 或 points[0]" },
                    "value": { "description": "新值" }
                },
                "required": ["path", "entity", "component", "field", "value"]
            }),
        },
        ToolDef {
            name: "component_add",
            description: "给实体加一个组件（用 schema 的默认值）。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "entity": { "type": "string" },
                    "component": { "type": "string" }
                },
                "required": ["path", "entity", "component"]
            }),
        },
        ToolDef {
            name: "asset_inspect",
            description: "检查一个已 cook 的资源（RSKL/RSKN/RANI/RMES/RTEX/RMAT）：计数、摘要与校验问题。路径相对项目根，如 assets/cooked/hero.rskin。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "相对项目根的路径" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "asset_validate",
            description: "只对已 cook 的资源跑校验（权重和、joint 越界、索引/子网格区间、关键帧时间与顺序）：ok=true 表示没有问题。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "skeleton": { "type": "string", "description": "可选：.ranim 的骨骼文件，用于检查轨道索引" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "component_remove",
            description: "移除实体的一个组件。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "entity": { "type": "string" },
                    "component": { "type": "string" }
                },
                "required": ["path", "entity", "component"]
            }),
        },
    ]
}

/// 执行一个工具；`Err` 会转成 MCP 的 `isError: true`。
pub fn call(root: &Path, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "documents_list" => documents_list(root, args),
        "document_read" => document_read(root, args),
        "document_write" => document_write(root, args),
        "schema_list" => schema_list(args),
        "entity_get" => entity_get(root, args),
        "field_set" => field_set(root, args),
        "component_add" => component_add(root, args),
        "asset_inspect" => asset_inspect(root, args),
        "asset_validate" => asset_validate(root, args),
        "component_remove" => component_remove(root, args),
        other => Err(format!("未知工具 '{other}'")),
    }
}

fn asset_inspect(root: &Path, args: &Value) -> Result<String, String> {
    let relative = str_arg(args, "path")?;
    let inspection = inspect_asset(root, &relative)?;
    serde_json::to_string_pretty(&inspection).map_err(|error| error.to_string())
}

fn asset_validate(root: &Path, args: &Value) -> Result<String, String> {
    let relative = str_arg(args, "path")?;
    let skeleton = args.get("skeleton").and_then(Value::as_str);
    let problems = validate_asset(root, &relative, skeleton)?;
    serde_json::to_string_pretty(&json!({
        "path": relative.replace('\\', "/"),
        "ok": problems.is_empty(),
        "problems": problems,
    }))
    .map_err(|error| error.to_string())
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("缺少字符串参数 '{key}'"))
}

fn doc_path(root: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn documents_list(root: &Path, args: &Value) -> Result<String, String> {
    let kind = args.get("kind").and_then(Value::as_str).unwrap_or("all");
    let kinds: &[DocKind] = match kind {
        "scene" => &[DocKind::Scene],
        "prefab" => &[DocKind::Prefab],
        _ => &[DocKind::Scene, DocKind::Prefab],
    };
    let mut out = Vec::new();
    for kind in kinds {
        let entries = scene::list_docs(root, *kind)?;
        for entry in entries {
            // 对外一律用项目相对路径，agent 拼 `path` 时不用关心绝对前缀。
            let root_prefix = root.to_string_lossy();
            let relative = entry
                .path
                .strip_prefix(root_prefix.as_ref())
                .unwrap_or(&entry.path)
                .trim_start_matches(['\\', '/'])
                .replace('\\', "/");
            out.push(json!({
                "name": entry.name,
                "path": relative,
                "kind": format!("{:?}", kind).to_lowercase(),
            }));
        }
    }
    serde_json::to_string_pretty(&out).map_err(|err| err.to_string())
}

fn document_read(root: &Path, args: &Value) -> Result<String, String> {
    let path = doc_path(root, &str_arg(args, "path")?);
    let doc = scene::read_doc(&path)?;
    serde_json::to_string_pretty(&doc).map_err(|err| err.to_string())
}

fn document_write(root: &Path, args: &Value) -> Result<String, String> {
    let path = doc_path(root, &str_arg(args, "path")?);
    let doc = args
        .get("doc")
        .ok_or_else(|| "缺少 'doc' 参数".to_string())?;
    scene::validate_doc(doc)?;
    scene::write_doc(&path, doc)?;
    Ok(format!("写入 {}", path.display()))
}

fn schema_list(args: &Value) -> Result<String, String> {
    let schema = build_schema();
    let only = args.get("component").and_then(Value::as_str);
    let types: Vec<&_> = schema
        .types
        .iter()
        .filter(|ty| only.is_none_or(|name| ty.name == name))
        .collect();
    if types.is_empty() {
        return Err(match only {
            Some(name) => format!("没有名为 '{name}' 的类型"),
            None => "schema 为空".to_string(),
        });
    }
    serde_json::to_string_pretty(&types).map_err(|err| err.to_string())
}

/// 实体定位：纯数字按 `id`，否则按 `name`（取第一个同名实体）。
fn locate<'a>(doc: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>, String> {
    let entities = doc
        .get_mut("entities")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "文档没有 entities 数组".to_string())?;
    let by_id = key.parse::<u64>().ok();
    let found = entities.iter_mut().find(|entity| match by_id {
        Some(id) => entity.get("id").and_then(Value::as_u64) == Some(id),
        None => entity.get("name").and_then(Value::as_str) == Some(key),
    });
    found
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("找不到实体 '{key}'"))
}

fn entity_get(root: &Path, args: &Value) -> Result<String, String> {
    let path = doc_path(root, &str_arg(args, "path")?);
    let mut doc = scene::read_doc(&path)?;
    let entity = locate(&mut doc, &str_arg(args, "entity")?)?;
    serde_json::to_string_pretty(entity).map_err(|err| err.to_string())
}

fn field_set(root: &Path, args: &Value) -> Result<String, String> {
    let path = doc_path(root, &str_arg(args, "path")?);
    let entity_key = str_arg(args, "entity")?;
    let component = str_arg(args, "component")?;
    let field = str_arg(args, "field")?;
    let value = args
        .get("value")
        .ok_or_else(|| "缺少 'value' 参数".to_string())?
        .clone();

    let mut doc = scene::read_doc(&path)?;
    let entity = locate(&mut doc, &entity_key)?;
    let components = entity
        .get_mut("components")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("实体 '{entity_key}' 没有 components"))?;
    let before = components
        .get(&component)
        .cloned()
        .ok_or_else(|| format!("实体 '{entity_key}' 上没有组件 '{component}'"))?;

    // `set_component_field` 从「改后」对象里读新值，所以先按路径打补丁；
    // 类型不合法会在它内部的 `object_from_json` 处被拒。
    let mut after = before.clone();
    patch_path(&mut after, &field, value)?;
    let updated = set_component_field(&component, &before, &field, &after)?;
    components.insert(component.clone(), updated.clone());

    scene::validate_doc(&doc)?;
    scene::write_doc(&path, &doc)?;
    serde_json::to_string_pretty(&updated).map_err(|err| err.to_string())
}

fn component_add(root: &Path, args: &Value) -> Result<String, String> {
    let path = doc_path(root, &str_arg(args, "path")?);
    let entity_key = str_arg(args, "entity")?;
    let component = str_arg(args, "component")?;
    let schema: Schema = build_schema();
    let ty = schema
        .type_by_name(&component)
        .ok_or_else(|| format!("'{component}' 不是已注册类型"))?;
    if !ty.is_component {
        return Err(format!("'{component}' 是嵌套结构体，不能挂到实体上"));
    }
    let default = ty
        .default_json
        .clone()
        .ok_or_else(|| format!("'{component}' 没有默认值"))?;

    let mut doc = scene::read_doc(&path)?;
    let entity = locate(&mut doc, &entity_key)?;
    let components = entity
        .get_mut("components")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("实体 '{entity_key}' 没有 components"))?;
    if components.contains_key(&component) {
        return Err(format!("实体 '{entity_key}' 已经有 '{component}'"));
    }
    components.insert(component.clone(), default.clone());

    scene::validate_doc(&doc)?;
    scene::write_doc(&path, &doc)?;
    serde_json::to_string_pretty(&default).map_err(|err| err.to_string())
}

fn component_remove(root: &Path, args: &Value) -> Result<String, String> {
    let path = doc_path(root, &str_arg(args, "path")?);
    let entity_key = str_arg(args, "entity")?;
    let component = str_arg(args, "component")?;

    let mut doc = scene::read_doc(&path)?;
    let entity = locate(&mut doc, &entity_key)?;
    let components = entity
        .get_mut("components")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("实体 '{entity_key}' 没有 components"))?;
    if components.remove(&component).is_none() {
        return Err(format!("实体 '{entity_key}' 上没有 '{component}'"));
    }

    scene::validate_doc(&doc)?;
    scene::write_doc(&path, &doc)?;
    Ok(format!("已移除 {component}"))
}

/// 按 `a.b[2].c` 写入 JSON；只用于给 `set_component_field` 准备「改后」值。
fn patch_path(target: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let mut cursor = target;
    for segment in parse_path(path)? {
        match segment {
            Segment::Key(key) => {
                let map = cursor
                    .as_object_mut()
                    .ok_or_else(|| format!("'{key}' 的容器不是对象"))?;
                cursor = map
                    .get_mut(&key)
                    .ok_or_else(|| format!("字段 '{key}' 不存在"))?;
            }
            Segment::Index(index) => {
                let list = cursor
                    .as_array_mut()
                    .ok_or_else(|| format!("[{index}] 的容器不是列表"))?;
                let len = list.len();
                cursor = list
                    .get_mut(index)
                    .ok_or_else(|| format!("下标 [{index}] 越界（长度 {len}）"))?;
            }
        }
    }
    *cursor = value;
    Ok(())
}

enum Segment {
    Key(String),
    Index(usize),
}

fn parse_path(path: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    for part in path.split('.') {
        if part.is_empty() {
            return Err(format!("字段路径 '{path}' 有空段"));
        }
        let (name, indices) = match part.find('[') {
            Some(at) => (&part[..at], &part[at..]),
            None => (part, ""),
        };
        if !name.is_empty() {
            segments.push(Segment::Key(name.to_string()));
        }
        let mut rest = indices;
        while let Some(stripped) = rest.strip_prefix('[') {
            let end = stripped
                .find(']')
                .ok_or_else(|| format!("字段路径 '{path}' 缺少 ']'"))?;
            let index = stripped[..end]
                .parse::<usize>()
                .map_err(|_| format!("字段路径 '{path}' 的下标不是数字"))?;
            segments.push(Segment::Index(index));
            rest = &stripped[end + 1..];
        }
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_nested_paths() {
        let mut value = json!({ "a": { "b": [1, 2, 3] }, "c": 0 });
        patch_path(&mut value, "a.b[1]", json!(9)).unwrap();
        assert_eq!(value["a"]["b"][1], 9);
        patch_path(&mut value, "c", json!(5)).unwrap();
        assert_eq!(value["c"], 5);
    }

    #[test]
    fn rejects_bad_paths() {
        let mut value = json!({ "a": 1 });
        assert!(patch_path(&mut value, "missing", json!(2)).is_err());
        assert!(patch_path(&mut value, "a[0]", json!(2)).is_err());
        assert!(parse_path("a..b").is_err());
        assert!(parse_path("a[1").is_err());
    }
}
