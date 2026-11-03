//! 组件 schema（`prism_editor_api::build_schema` 的 Rust 镜像）。
//!
//! 检查器完全由 schema 驱动：字段的控件类型来自 `kind` / `structType` / `variants`
//! 与 形状的 `PropertyDrawer`（`renderer` + `attributes`）。因此新增一个
//! 组件或字段不需要改 UI —— 与 Web 编辑器是同一份规则。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PropertyRenderer {
    Default,
    Slider,
    TextArea,
    Color,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PropertyAttribute {
    Range {
        min: f32,
        max: f32,
    },
    Min {
        min: f32,
    },
    Max {
        max: f32,
    },
    Multiline {
        lines: u32,
    },
    TextArea {
        #[serde(default)]
        min_lines: u32,
        #[serde(default)]
        max_lines: u32,
    },
    ColorUsage {
        #[serde(default)]
        show_alpha: bool,
        #[serde(default)]
        hdr: bool,
    },
    Header {
        text: String,
    },
    Tooltip {
        text: String,
    },
    Delayed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PropertyDrawer {
    pub renderer: Option<PropertyRenderer>,
    #[serde(default)]
    pub attributes: Vec<PropertyAttribute>,
    #[serde(default)]
    pub custom: Option<String>,
}

impl PropertyDrawer {
    pub fn renderer(&self) -> PropertyRenderer {
        self.renderer.unwrap_or(PropertyRenderer::Default)
    }

    pub fn range(&self) -> Option<(f32, f32)> {
        self.attributes
            .iter()
            .find_map(|attribute| match attribute {
                PropertyAttribute::Range { min, max } => Some((*min, *max)),
                _ => None,
            })
    }

    pub fn min(&self) -> Option<f32> {
        self.attributes
            .iter()
            .find_map(|attribute| match attribute {
                PropertyAttribute::Min { min } => Some(*min),
                _ => None,
            })
    }

    pub fn max(&self) -> Option<f32> {
        self.attributes
            .iter()
            .find_map(|attribute| match attribute {
                PropertyAttribute::Max { max } => Some(*max),
                _ => None,
            })
    }

    pub fn tooltip(&self) -> Option<&str> {
        self.attributes
            .iter()
            .find_map(|attribute| match attribute {
                PropertyAttribute::Tooltip { text } => Some(text.as_str()),
                _ => None,
            })
    }

    pub fn header(&self) -> Option<&str> {
        self.attributes
            .iter()
            .find_map(|attribute| match attribute {
                PropertyAttribute::Header { text } => Some(text.as_str()),
                _ => None,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FieldSchema {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub struct_type: Option<String>,
    #[serde(default)]
    pub inner: Option<Box<FieldSchema>>,
    #[serde(default)]
    pub array_len: Option<usize>,
    #[serde(default)]
    pub variants: Option<Vec<String>>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub drawer: Option<PropertyDrawer>,
}

/// 检查器里一个字段实际用什么控件画。
#[derive(Debug, Clone, PartialEq)]
pub enum FieldWidget {
    Bool,
    Integer,
    Float,
    Text,
    Multiline,
    /// `glam::Vec3` / `[f32;3]` 之类，分量编辑。
    Vector {
        len: usize,
    },
    /// `glam::Quat` 用欧拉角编辑。
    Quat,
    Color {
        alpha: bool,
    },
    Enum {
        variants: Vec<String>,
    },
    List,
    Option,
    Struct,
    ReadOnly,
}

impl FieldSchema {
    pub fn drawer(&self) -> Option<&PropertyDrawer> {
        self.drawer.as_ref()
    }

    pub fn label(&self) -> &str {
        &self.name
    }

    pub fn widget(&self) -> FieldWidget {
        if self.read_only {
            return FieldWidget::ReadOnly;
        }
        if let Some(drawer) = &self.drawer {
            match drawer.renderer() {
                PropertyRenderer::Slider => return FieldWidget::Float,
                PropertyRenderer::TextArea => return FieldWidget::Multiline,
                PropertyRenderer::Color => return FieldWidget::Color { alpha: true },
                _ => {}
            }
        }
        if let Some(variants) = &self.variants {
            if !variants.is_empty() {
                return FieldWidget::Enum {
                    variants: variants.clone(),
                };
            }
        }
        match self.kind.as_str() {
            "bool" => FieldWidget::Bool,
            "f32" => FieldWidget::Float,
            "i32" | "u8" | "u32" | "u64" => FieldWidget::Integer,
            "string" => FieldWidget::Text,
            "array" => match self.struct_type.as_deref() {
                Some("glam::Vec3") => FieldWidget::Vector { len: 3 },
                Some("glam::Quat") => FieldWidget::Quat,
                // RGBA / 颜色数组走色块 + 分量。
                _ => match self.array_len {
                    Some(len @ (3 | 4)) if self.name.contains("color") => {
                        FieldWidget::Color { alpha: len == 4 }
                    }
                    Some(len @ (2..=4)) => FieldWidget::Vector { len },
                    _ => FieldWidget::List,
                },
            },
            "list" => FieldWidget::List,
            "option" => FieldWidget::Option,
            "struct" => match self.struct_type.as_deref() {
                Some("glam::Vec3") => FieldWidget::Vector { len: 3 },
                Some("glam::Quat") => FieldWidget::Quat,
                Some("glam::Vec4") => FieldWidget::Vector { len: 4 },
                _ => FieldWidget::Struct,
            },
            _ => FieldWidget::ReadOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TypeSchema {
    pub name: String,
    #[serde(default)]
    pub type_id: String,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub is_component: bool,
    #[serde(default)]
    pub fields: Vec<FieldSchema>,
    #[serde(default)]
    pub default_json: Option<Value>,
}

impl TypeSchema {
    /// `prism_engine::scene::Camera` → `Camera`。
    pub fn display_name(&self) -> &str {
        self.name.rsplit("::").next().unwrap_or(&self.name)
    }

    pub fn category(&self) -> &str {
        let mut parts = self.name.split("::");
        let first = parts.next().unwrap_or("");
        if self.name.starts_with("prism_engine::ui") {
            "UI"
        } else if self.name.starts_with("prism_engine::scene::Audio") {
            "Audio"
        } else if first.contains("render") {
            "Rendering"
        } else {
            "Scene"
        }
    }

    pub fn default_value(&self) -> Value {
        self.default_json.clone().unwrap_or(Value::Null)
    }

    pub fn field(&self, name: &str) -> Option<&FieldSchema> {
        self.fields.iter().find(|field| field.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Schema {
    #[serde(default)]
    pub types: Vec<TypeSchema>,
}

impl Schema {
    pub fn from_value(value: &Value) -> Option<Self> {
        let types = value.get("types")?;
        serde_json::from_value(serde_json::json!({ "types": types })).ok()
    }

    pub fn components(&self) -> Vec<&TypeSchema> {
        self.types.iter().filter(|item| item.is_component).collect()
    }

    pub fn get(&self, name: &str) -> Option<&TypeSchema> {
        self.types.iter().find(|item| item.name == name)
    }

    pub fn component(&self, name: &str) -> Option<&TypeSchema> {
        self.types
            .iter()
            .find(|item| item.is_component && item.name == name)
    }

    /// 「添加组件」菜单：按分类分组。
    pub fn component_menu(&self) -> Vec<(&str, Vec<&TypeSchema>)> {
        let mut categories: Vec<(&str, Vec<&TypeSchema>)> = Vec::new();
        for component in self.components() {
            let category = component.category();
            match categories.iter_mut().find(|(name, _)| *name == category) {
                Some((_, items)) => items.push(component),
                None => categories.push((category, vec![component])),
            }
        }
        categories
    }

    pub fn component_summary(&self, name: &str) -> Option<String> {
        let component = self.component(name)?;
        let mut lines = vec![component.display_name().to_string()];
        if component.version > 1 {
            lines.push(format!("v{}", component.version));
        }
        lines.push(format!("{} 个字段", component.fields.len()));
        Some(lines.join(" · "))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityPreset {
    pub id: String,
    pub label: String,
    pub name: String,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub overrides: Map<String, Value>,
    #[serde(default = "default_rotation")]
    pub rotation: [f32; 4],
    #[serde(default)]
    pub position: [f32; 3],
}

fn default_rotation() -> [f32; 4] {
    [0.0, 0.0, 0.0, 1.0]
}

impl EntityPreset {
    pub fn from_list(value: &Value) -> Vec<Self> {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }
}

/// 字段路径里的一段：对象键或数组下标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

pub fn parse_path(path: &str) -> Vec<PathSegment> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = path.chars().peekable();
    fn flush(current: &mut String, segments: &mut Vec<PathSegment>) {
        if !current.is_empty() {
            segments.push(PathSegment::Key(std::mem::take(current)));
        }
    }
    while let Some(ch) = chars.next() {
        match ch {
            '.' => flush(&mut current, &mut segments),
            '[' => {
                flush(&mut current, &mut segments);
                let mut index = String::new();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == ']' {
                        break;
                    }
                    index.push(next);
                }
                if let Ok(index) = index.parse::<usize>() {
                    segments.push(PathSegment::Index(index));
                }
            }
            _ => current.push(ch),
        }
    }
    flush(&mut current, &mut segments);
    segments
}

pub fn value_at<'a>(root: &'a Value, segments: &[PathSegment]) -> Option<&'a Value> {
    let mut current = root;
    for segment in segments {
        current = match segment {
            PathSegment::Key(key) => current.get(key)?,
            PathSegment::Index(index) => current.get(*index)?,
        };
    }
    Some(current)
}

/// 反射写入的编辑器侧等价物：只改存在的路径，不创建中间节点。
pub fn set_at(root: &mut Value, segments: &[PathSegment], value: Value) -> bool {
    if segments.is_empty() {
        *root = value;
        return true;
    }
    let mut current = root;
    for (position, segment) in segments.iter().enumerate() {
        let last = position + 1 == segments.len();
        match segment {
            PathSegment::Key(key) => {
                let Some(map) = current.as_object_mut() else {
                    return false;
                };
                if last {
                    if !map.contains_key(key) {
                        return false;
                    }
                    map.insert(key.clone(), value);
                    return true;
                }
                let Some(next) = map.get_mut(key) else {
                    return false;
                };
                current = next;
            }
            PathSegment::Index(index) => {
                let Some(items) = current.as_array_mut() else {
                    return false;
                };
                let Some(next) = items.get_mut(*index) else {
                    return false;
                };
                if last {
                    *next = value;
                    return true;
                }
                current = next;
            }
        }
    }
    false
}

pub fn get_at(root: &Value, path: &str) -> Option<Value> {
    value_at(root, &parse_path(path)).cloned()
}

pub fn set_at_path(root: &mut Value, path: &str, value: Value) -> bool {
    set_at(root, &parse_path(path), value)
}

/// 组件默认值 + 预设覆盖（浅合并，与 Web 编辑器一致）。
pub fn component_value_with_overrides(default: &Value, overrides: Option<&Value>) -> Value {
    match (default, overrides) {
        (Value::Object(base), Some(Value::Object(over))) => {
            let mut merged = base.clone();
            for (key, value) in over {
                merged.insert(key.clone(), value.clone());
            }
            Value::Object(merged)
        }
        (_, Some(value)) => value.clone(),
        (value, None) => value.clone(),
    }
}

/// 一个 f32 字段能接受的取值范围（`Range` 特性优先，其次 `Min`/`Max`）。
pub fn numeric_bounds(field: &FieldSchema) -> (f32, Option<f32>, Option<f32>) {
    let drawer = field.drawer();
    let range = drawer.and_then(|drawer| drawer.range());
    let min = drawer.and_then(|drawer| drawer.min());
    let max = drawer.and_then(|drawer| drawer.max());
    match range {
        Some((min, max)) => (min, Some(min), Some(max)),
        None => (0.0, min, max),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_json() -> Value {
        serde_json::json!({
            "types": [
                {
                    "name": "prism_engine::scene::Camera",
                    "typeId": "abc",
                    "version": 2,
                    "isComponent": true,
                    "defaultJson": {
                        "fov_y_degrees": 60.0,
                        "background_color": [0.5, 0.5, 0.5, 1.0],
                        "clear_flags": "Skybox"
                    },
                    "fields": [
                        { "name": "fov_y_degrees", "kind": "f32", "structType": null, "inner": null,
                          "arrayLen": null, "variants": null, "readOnly": false,
                          "drawer": { "renderer": "slider", "attributes": [{ "type": "range", "min": 1.0, "max": 179.0 }], "custom": null } },
                        { "name": "background_color", "kind": "array", "structType": null, "inner": null,
                          "arrayLen": 4, "variants": null, "readOnly": false },
                        { "name": "clear_flags", "kind": "struct", "structType": null, "inner": null,
                          "arrayLen": null, "variants": ["Skybox", "Solid"], "readOnly": false },
                        { "name": "position", "kind": "struct", "structType": "glam::Vec3", "inner": null,
                          "arrayLen": null, "variants": null, "readOnly": false },
                        { "name": "id", "kind": "u32", "structType": null, "inner": null,
                          "arrayLen": null, "variants": null, "readOnly": true }
                    ]
                },
                {
                    "name": "glam::Vec3",
                    "typeId": "v3",
                    "version": 1,
                    "isComponent": false,
                    "defaultJson": [0.0, 0.0, 0.0],
                    "fields": []
                }
            ]
        })
    }

    #[test]
    fn parses_schema_and_resolves_widgets() {
        let schema = Schema::from_value(&schema_json()).expect("schema");
        assert_eq!(schema.components().len(), 1);
        let camera = schema.component("prism_engine::scene::Camera").unwrap();
        assert_eq!(camera.display_name(), "Camera");
        assert_eq!(camera.fields.len(), 5);

        let fov = camera.field("fov_y_degrees").unwrap();
        assert_eq!(fov.widget(), FieldWidget::Float);
        assert_eq!(numeric_bounds(fov), (1.0, Some(1.0), Some(179.0)));

        assert_eq!(
            camera.field("background_color").unwrap().widget(),
            FieldWidget::Color { alpha: true }
        );
        assert_eq!(
            camera.field("clear_flags").unwrap().widget(),
            FieldWidget::Enum {
                variants: vec!["Skybox".into(), "Solid".into()]
            }
        );
        assert_eq!(
            camera.field("position").unwrap().widget(),
            FieldWidget::Vector { len: 3 }
        );
        assert_eq!(camera.field("id").unwrap().widget(), FieldWidget::ReadOnly);
    }

    #[test]
    fn component_menu_groups_by_category() {
        let schema = Schema::from_value(&schema_json()).unwrap();
        let menu = schema.component_menu();
        assert_eq!(menu.len(), 1);
        assert_eq!(menu[0].0, "Scene");
        assert_eq!(menu[0].1.len(), 1);
    }

    #[test]
    fn paths_read_write_nested_arrays() {
        let mut value = serde_json::json!({
            "transform": { "position": [1.0, 2.0, 3.0] },
            "materials": [ { "color": [1.0, 0.0, 0.0, 1.0] } ]
        });
        assert_eq!(
            get_at(&value, "transform.position[1]"),
            Some(Value::from(2.0))
        );
        assert!(set_at_path(
            &mut value,
            "transform.position[1]",
            Value::from(5.0)
        ));
        assert_eq!(
            get_at(&value, "transform.position"),
            Some(serde_json::json!([1.0, 5.0, 3.0]))
        );

        assert!(set_at_path(
            &mut value,
            "materials[0].color[3]",
            Value::from(0.5)
        ));
        assert_eq!(
            get_at(&value, "materials[0].color[3]"),
            Some(Value::from(0.5))
        );

        // 不存在的路径不创建节点。
        assert!(!set_at_path(&mut value, "nope.deep", Value::Null));
        assert!(value.get("nope").is_none());
    }

    #[test]
    fn parse_path_handles_plain_and_indexed() {
        assert_eq!(
            parse_path("a.b[2].c"),
            vec![
                PathSegment::Key("a".into()),
                PathSegment::Key("b".into()),
                PathSegment::Index(2),
                PathSegment::Key("c".into())
            ]
        );
        assert!(parse_path("").is_empty());
    }

    #[test]
    fn component_overrides_merge_shallowly() {
        let default = serde_json::json!({"a": 1, "b": 2});
        let over = serde_json::json!({"b": 9});
        assert_eq!(
            component_value_with_overrides(&default, Some(&over)),
            serde_json::json!({"a": 1, "b": 9})
        );
    }

    #[test]
    fn preset_parsing_is_tolerant() {
        let presets = EntityPreset::from_list(&serde_json::json!([
            {"id": "empty", "label": "空实体", "name": "entity"},
            {"id": "camera", "label": "相机", "name": "Camera",
             "components": ["prism_engine::scene::Camera"],
             "overrides": {}, "position": [0.0, 0.0, 10.0], "rotation": [0.0, 0.0, 0.0, 1.0]}
        ]));
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].rotation, default_rotation());
        assert_eq!(presets[1].position, [0.0, 0.0, 10.0]);
    }
}
