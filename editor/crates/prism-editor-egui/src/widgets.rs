//! schema 驱动的字段控件（检查器用）。
//!
//! 规则与 Web 编辑器 / GPUI 编辑器一致：控件类型完全来自
//! `prism-editor-core` 的 `FieldSchema::widget()`，所以新增组件字段不需要改
//! 这里。写回统一走 `Workspace::set_field`（反射校验 + 撤销合并）。

use egui::{Response, Ui};
use egui_phosphor::regular as icons;
use prism_editor_core::viewport_math as vm;
use prism_editor_core::{FieldSchema, FieldWidget, Schema, TypeSchema};
use serde_json::Value;

/// 资产拖拽载荷：从 Project 面板拖到层级 / 检查器字段 / 目录。
#[derive(Clone, Debug)]
pub struct AssetDrag {
    pub path: String,
}

/// 组件卡片上的按钮动作（先收集，渲染结束后由调用方应用）。
pub enum CardAction {
    Copy(String),
    Remove(String),
    /// 在检查器顺序里上移 / 下移一个组件（#512）。
    MoveUp(String),
    MoveDown(String),
}

/// 一个组件卡片：标题栏 + 字段列表 + 复制/删除按钮。
///
/// `set(field, value)` 由调用方决定写到哪个实体（单选 / 多选批量）。
/// 同 [`component_card`]，但路径字段带资产选择器。
pub fn component_card_with_assets(
    ui: &mut Ui,
    registry: &Schema,
    component: &TypeSchema,
    value: &Value,
    actions: &mut Vec<CardAction>,
    set: &mut dyn FnMut(&str, Value),
    assets: &[String],
    doc_dir: &str,
) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(component.display_name())
                        .strong()
                        .size(12.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(icons::X)
                        .on_hover_text("Remove Component")
                        .clicked()
                    {
                        actions.push(CardAction::Remove(component.name.clone()));
                    }
                    // 组件顺序是文档的一部分（#512）：这两个箭头走一次撤销步。
                    if ui
                        .small_button(icons::CARET_DOWN)
                        .on_hover_text("Move Down")
                        .clicked()
                    {
                        actions.push(CardAction::MoveDown(component.name.clone()));
                    }
                    if ui
                        .small_button(icons::CARET_UP)
                        .on_hover_text("Move Up")
                        .clicked()
                    {
                        actions.push(CardAction::MoveUp(component.name.clone()));
                    }
                    if ui
                        .small_button(icons::COPY)
                        .on_hover_text("Copy Component")
                        .clicked()
                    {
                        actions.push(CardAction::Copy(component.name.clone()));
                    }
                });
            });
            ui.separator();

            let fields = component.fields.clone();
            for field in &fields {
                let Some(field_value) = value.get(&field.name) else {
                    continue;
                };
                if field.name == "type_id" {
                    continue;
                }
                field_row_at(ui, registry, field, field_value, "", set, assets, doc_dir);
            }
            let _ = &component;
        });
}

/// 单个字段的编辑控件。`set(field, value)` 写回（单选走 `set_field`，
/// 多选走 `set_field_multi`），所以这里不关心实体。
/// 资产字段的扩展名过滤（引擎的 object field 只列可接受的类型）。
fn asset_filter(name: &str) -> Option<&'static [&'static str]> {
    if name == "font" || name.ends_with("_font") {
        // #583：UI 文本可以用工程自带字体。
        Some(&["ttf", "otf", "ttc"])
    } else if name.contains("mesh") {
        Some(&["obj", "gltf", "glb", "fbx", "mesh"])
    } else if name.contains("sprite") || name.contains("texture") || name.ends_with("_tex") {
        Some(&["png", "jpg", "jpeg", "ktx2", "dds"])
    } else if name.contains("hdr") || name.contains("env") {
        Some(&["hdr", "exr"])
    } else if name.contains("volume") || name.contains("prefab") {
        Some(&["json"])
    } else if name.ends_with("_path") {
        Some(&[])
    } else {
        None
    }
}

fn asset_matches(name: &str, path: &str) -> bool {
    match asset_filter(name) {
        Some([]) | None => true,
        Some(extensions) => extensions
            .iter()
            .any(|extension| path.to_lowercase().ends_with(&format!(".{extension}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_fields_commit_on_enter_or_focus_loss_only() {
        // 编辑中：每敲一个字符都不碰文档。
        assert_eq!(
            text_commit_decision(true, false, false, false, false),
            TextCommit::Keep
        );
        assert_eq!(
            text_commit_decision(true, false, true, false, false),
            TextCommit::Commit
        );
        // 丢焦点 = 提交（点别处那种情况）。
        assert_eq!(
            text_commit_decision(false, true, false, false, false),
            TextCommit::Commit
        );
        // Esc 丢弃，即使同一帧还按了回车。
        assert_eq!(
            text_commit_decision(true, false, true, true, false),
            TextCommit::Cancel
        );
        // 多行框里回车是换行，只能靠失焦提交。
        assert_eq!(
            text_commit_decision(true, false, true, false, true),
            TextCommit::Keep
        );
    }

    #[test]
    fn asset_filters_match_expected_extensions() {
        assert!(asset_matches("mesh_path", "assets/ship.GLTF"));
        assert!(!asset_matches("mesh_path", "assets/ship.png"));
        assert!(asset_matches("sprite_path", "assets/hero.png"));
        assert!(asset_matches("font", "assets/fonts/hud.ttf"));
        assert!(!asset_matches("font", "assets/hero.png"));
        assert!(asset_matches("environment_path", "assets/sunset.exr"));
        assert!(asset_matches("custom_path", "assets/data.bin"));
        assert!(asset_matches("label", "anything.bin"));
    }

    #[test]
    fn card_actions_and_field_rows_can_render_without_gpu() {
        let ctx = egui::Context::default();
        let schema = Schema::from_value(&serde_json::json!({
            "types": [{
                "name": "Test",
                "displayName": "Test",
                "typeId": "test",
                "version": 1,
                "isComponent": true,
                "defaultJson": {"value": 0.0},
                "fields": [{
                    "name": "value",
                    "kind": "f32",
                    "structType": null,
                    "inner": null,
                    "arrayLen": null,
                    "variants": null,
                    "readOnly": false
                }]
            }]
        }))
        .unwrap();
        let component = schema.component("Test").unwrap().clone();
        let mut actions = Vec::new();
        let mut writes = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                component_card_with_assets(
                    ui,
                    &schema,
                    &component,
                    &serde_json::json!({"value": 1.0}),
                    &mut actions,
                    &mut |path, value| writes.push((path.to_owned(), value)),
                    &[],
                    "assets",
                );
            });
        });
        assert!(actions.is_empty());
        assert!(writes.is_empty());
    }
}

/// 数值输入框接的表达式解析器（`90/2`、`2*(3+4)`、`sin(30)`）。
///
/// `DragValue::custom_parser` 只在用户敲完回车时调用，所以平时显示的还是数字本身。
pub fn number_parser(text: &str) -> Option<f64> {
    prism_editor_core::expr::parse_number_or_expression(text)
}

/// 画一个可拖动的字段标签，返回**这一帧**拖动产生的增量（已按 `speed` 与修饰键换算）。
///
/// Unity 的 `EditorGUIUtility.DragZoneRect`：把数值字段左边的文字变成拖动热区，
/// 左右拖就能连续调值，不用去够右边那个小把手（#479）。按住 Shift 精调（×0.1），
/// 按住 Alt 粗调（×10）。
pub fn drag_label(ui: &mut Ui, text: &str, speed: f32) -> f32 {
    let response = ui
        .add(egui::Label::new(egui::RichText::new(text).size(11.0)).sense(egui::Sense::drag()))
        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
        .on_hover_text("Drag to change");
    if !response.dragged() {
        return 0.0;
    }
    let mut delta = response.drag_delta().x;
    if delta == 0.0 {
        return 0.0;
    }
    let (shift, alt) = ui.input(|input| (input.modifiers.shift, input.modifiers.alt));
    if shift {
        delta *= 0.1;
    }
    if alt {
        delta *= 10.0;
    }
    delta * speed
}

/// [`drag_label`] 的单值版本：拖动标签直接改 `value`，返回是否变了。
pub fn scrubbable_label(ui: &mut Ui, text: &str, value: &mut f32, speed: f32) -> bool {
    let delta = drag_label(ui, text, speed);
    if delta == 0.0 {
        return false;
    }
    *value += delta;
    true
}

/// 文本字段该不该写回文档（#479：编辑期间只改本地缓冲）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextCommit {
    /// 继续编辑，别碰文档。
    Keep,
    /// Enter（单行）或失焦：把缓冲写回。
    Commit,
    /// Esc：丢弃缓冲，回到文档里的值。
    Cancel,
}

/// 输入框状态的纯函数部分，方便单测（egui 交互没法在测试里模拟）。
pub(crate) fn text_commit_decision(
    focused: bool,
    lost_focus: bool,
    enter: bool,
    escape: bool,
    multiline: bool,
) -> TextCommit {
    if focused && escape {
        return TextCommit::Cancel;
    }
    // 多行文本的 Enter 是换行，只能靠失焦提交。
    if (focused && enter && !multiline) || lost_focus {
        return TextCommit::Commit;
    }
    TextCommit::Keep
}

/// 延迟提交的文本输入：编辑期间只改 `egui` 里的临时缓冲，提交时返回新值。
///
/// 以前每敲一个字符就 `set_field`：撤销栈被灌满、`dirty` 一直闪，
/// 还会触发文档同步的整场重载（#435 / #473）。
fn delayed_text_edit(
    ui: &mut Ui,
    id: egui::Id,
    value: &str,
    multiline: bool,
) -> (Response, String, Option<String>) {
    let mut buffer = ui
        .data_mut(|data| data.get_temp::<String>(id))
        .unwrap_or_else(|| value.to_string());
    let response = if multiline {
        ui.add(egui::TextEdit::multiline(&mut buffer))
    } else {
        ui.add(egui::TextEdit::singleline(&mut buffer))
    };
    let focused = response.has_focus();
    let (enter, escape) = ui.input(|input| {
        (
            input.key_pressed(egui::Key::Enter),
            input.key_pressed(egui::Key::Escape),
        )
    });
    let mut committed = None;
    match text_commit_decision(focused, response.lost_focus(), enter, escape, multiline) {
        TextCommit::Cancel => {
            buffer = value.to_string();
            response.surrender_focus();
        }
        TextCommit::Commit => {
            if buffer != value {
                committed = Some(buffer.clone());
            }
        }
        // 没在编辑（或被别的控件抢走焦点且已经处理过提交）：跟随文档里的值。
        TextCommit::Keep => {
            if !focused {
                buffer = value.to_string();
            }
        }
    }
    ui.data_mut(|data| data.insert_temp(id, buffer.clone()));
    (response, buffer, committed)
}

/// 带资产选择器的字段行（路径字段右侧多一个 `…` 按钮，列出 `assets` 里的候选）。
fn field_row_at(
    ui: &mut Ui,
    registry: &Schema,
    field: &FieldSchema,
    value: &Value,
    prefix: &str,
    set: &mut dyn FnMut(&str, Value),
    assets: &[String],
    doc_dir: &str,
) {
    let widget = field.widget();
    let label = field.label();
    let name = format!("{prefix}{}", field.name);

    match widget {
        FieldWidget::ReadOnly => {
            ui.horizontal(|ui| {
                ui.label(label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format_json(value))
                            .monospace()
                            .weak()
                            .size(11.0),
                    );
                });
            });
        }
        FieldWidget::Bool => {
            let mut checked = value.as_bool().unwrap_or(false);
            if ui.checkbox(&mut checked, label).changed() {
                set(&name, Value::Bool(checked));
            }
        }
        FieldWidget::Integer => {
            let mut current = value.as_i64().unwrap_or(0);
            ui.horizontal(|ui| {
                let mut scrubbed = current as f32;
                let dragged = scrubbable_label(ui, label, &mut scrubbed, 1.0);
                if dragged {
                    current = scrubbed.round() as i64;
                }
                let drag = ui
                    .add(
                        egui::DragValue::new(&mut current)
                            .speed(1.0)
                            .custom_parser(number_parser),
                    )
                    .on_hover_text("Type an expression: 90/2, 2*(3+4)");
                if dragged || drag.changed() {
                    set(&name, Value::from(current));
                }
            });
        }
        FieldWidget::Float => {
            let mut current = value.as_f64().unwrap_or(0.0) as f32;
            ui.horizontal(|ui| {
                let dragged = scrubbable_label(ui, label, &mut current, 0.01);
                let drawer = field.drawer();
                let mut drag = egui::DragValue::new(&mut current).speed(0.01);
                if let Some((min, max)) = drawer.and_then(|drawer| drawer.range()) {
                    drag = drag.range(min..=max);
                } else {
                    if let Some(min) = drawer.and_then(|drawer| drawer.min()) {
                        drag = drag.range(min..=f32::MAX);
                    }
                    if let Some(max) = drawer.and_then(|drawer| drawer.max()) {
                        drag = drag.range(f32::MIN..=max);
                    }
                }
                let response = ui
                    .add(drag.custom_parser(number_parser))
                    .on_hover_text("Type an expression: 90/2, 2*(3+4), sin(30)");
                if dragged || response.changed() {
                    set(&name, Value::from(current as f64));
                }
            });
        }
        FieldWidget::Text => {
            let mut current = value.as_str().unwrap_or_default().to_string();
            let edit_id = ui.make_persistent_id(("field-text", name.as_str()));
            ui.horizontal(|ui| {
                ui.label(label);
                let (response, buffer, committed) =
                    delayed_text_edit(ui, edit_id, &current, false);
                current = buffer;
                if let Some(text) = committed {
                    set(&name, Value::String(text));
                }
                // 引擎的「把资产拖到对象字段」：路径类字段接受资产拖放。
                let looks_like_asset = name.ends_with("_path")
                    || matches!(name.as_str(), "prefab" | "env_map" | "volume" | "mesh");
                // 引用断掉（被删 / 改名 / 拼错）时红字标出来：引擎那边只会往
                // 日志写一句 warn，画面里是「物体不见了」或「贴图变白」（#467）。
                if looks_like_asset
                    && !current.is_empty()
                    && prism_editor_core::assets::resolve_reference(assets, doc_dir, &current)
                        .is_none()
                {
                    ui.label(
                        egui::RichText::new("⚠ Missing")
                            .size(10.5)
                            .color(egui::Color32::from_rgb(232, 96, 96)),
                    )
                    .on_hover_text(format!(
                        "{current} is not in the asset database — the file was deleted,                          renamed, or the path is misspelt"
                    ));
                }
                if looks_like_asset {
                    if let Some(payload) = response.dnd_release_payload::<AssetDrag>() {
                        set(&name, Value::String(payload.path.clone()));
                    }
                    let mut picked: Option<String> = None;
                    ui.menu_button("…", |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt((name.clone(), "asset-picker"))
                            .max_height(260.0)
                            .show(ui, |ui| {
                                if assets.is_empty() {
                                    ui.label(egui::RichText::new("no assets").weak().size(10.0));
                                }
                                for path in assets {
                                    if !asset_matches(&name, path) {
                                        continue;
                                    }
                                    if ui.selectable_label(current == *path, path).clicked() {
                                        picked = Some(path.clone());
                                        ui.close();
                                    }
                                }
                            });
                    });
                    if let Some(path) = picked {
                        set(&name, Value::String(path));
                    }
                }
            });
        }
        FieldWidget::Multiline => {
            let current = value.as_str().unwrap_or_default().to_string();
            let edit_id = ui.make_persistent_id(("field-multiline", name.as_str()));
            ui.label(label);
            let (_response, _buffer, committed) = delayed_text_edit(ui, edit_id, &current, true);
            if let Some(text) = committed {
                set(&name, Value::String(text));
            }
        }
        FieldWidget::Vector { len } => {
            let mut values: Vec<f32> = (0..len)
                .map(|axis| {
                    value
                        .get(axis)
                        .and_then(|item| item.as_f64())
                        .unwrap_or(0.0) as f32
                })
                .collect();
            ui.horizontal(|ui| {
                // 拖标签 = 所有分量一起动（Unity 的 Vector3 label 行为）。
                let mut scratch = 0.0f32;
                let delta = drag_label(ui, label, 0.01);
                if delta != 0.0 {
                    for item in values.iter_mut() {
                        *item += delta;
                    }
                    scratch = delta;
                    let mut next = value.clone();
                    if let Some(array) = next.as_array_mut() {
                        for (axis, item) in values.iter().enumerate() {
                            if let Some(slot) = array.get_mut(axis) {
                                *slot = Value::from(*item as f64);
                            }
                        }
                        set(&name, next);
                    }
                }
                let _ = scratch;
                for (axis, item) in values.iter_mut().enumerate() {
                    if ui
                        .add(
                            egui::DragValue::new(item)
                                .speed(0.01)
                                .custom_parser(number_parser),
                        )
                        .changed()
                    {
                        let mut next = value.clone();
                        if let Some(array) = next.as_array_mut() {
                            if let Some(slot) = array.get_mut(axis) {
                                *slot = Value::from(*item as f64);
                            }
                            set(&name, next);
                        }
                    }
                }
            });
        }
        FieldWidget::Quat => {
            // 四元数按 XYZ 欧拉角编辑（`R = Rz Ry Rx`，与 glam 的
            // `EulerRot::XYZ` 一致）：直接编辑 x/y/z/w 四个分量没人算得出来。
            let current = [0usize, 1, 2, 3].map(|axis| {
                value
                    .get(axis)
                    .and_then(|item| item.as_f64())
                    .unwrap_or(if axis == 3 { 1.0 } else { 0.0 }) as f32
            });
            let mut degrees = vm::quat_to_euler_degrees(current);
            ui.horizontal(|ui| {
                ui.label(label);
                for axis in 0..3 {
                    let response = ui
                        .add(
                            egui::DragValue::new(&mut degrees[axis])
                                .speed(0.5)
                                .max_decimals(2)
                                .suffix("°")
                                .prefix(["X ", "Y ", "Z "][axis])
                                .custom_parser(number_parser),
                        )
                        .on_hover_text("Type an expression: 90/2, 45+15");
                    if response.changed() {
                        let quat = vm::euler_degrees_to_quat(degrees);
                        set(
                            &name,
                            Value::Array(
                                quat.iter()
                                    .map(|component| Value::from(*component as f64))
                                    .collect(),
                            ),
                        );
                        break;
                    }
                }
            });
        }
        FieldWidget::Color { alpha } => {
            let mut rgba = read_color(value, alpha);
            ui.horizontal(|ui| {
                ui.label(label);
                if ui.color_edit_button_rgba_unmultiplied(&mut rgba).changed() {
                    set(&name, color_value(rgba, alpha));
                }
            });
        }
        FieldWidget::Enum { variants } => {
            let current = value.as_str().unwrap_or_default().to_string();
            ui.horizontal(|ui| {
                ui.label(label);
                egui::ComboBox::from_id_salt((name.clone(), field.name.clone()))
                    .selected_text(current.clone())
                    .show_ui(ui, |ui| {
                        for variant in &variants {
                            if ui.selectable_label(*variant == current, variant).clicked() {
                                set(&name, Value::String(variant.clone()));
                            }
                        }
                    });
            });
        }
        FieldWidget::List => {
            json_row(ui, label, value);
        }
        // `Option<T>`: a checkbox says whether the value exists, then the inner
        // scalar is edited in place. Authors used to have to hand-edit JSON to
        // set `Style.width` / `Style.height`.
        FieldWidget::Option => {
            let inner = field.inner.as_deref();
            let mut enabled = !value.is_null();
            if ui.checkbox(&mut enabled, label).changed() {
                set(
                    &name,
                    if enabled {
                        inner.map(default_value).unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    },
                );
            }
            if enabled {
                if let Some(inner) = inner {
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        optional_scalar(ui, &name, inner, value, set);
                    });
                }
            }
        }
        // Nested struct (`Style.anchors`, `Style.pivot`, `Margin` …): recurse
        // through the registry so every leaf keeps its real widget and path.
        FieldWidget::Struct => {
            let fields = field
                .struct_type
                .as_deref()
                .and_then(|name| registry.get(name))
                .map(|schema| schema.fields.clone())
                .unwrap_or_default();
            if fields.is_empty() {
                json_row(ui, label, value);
            } else {
                egui::CollapsingHeader::new(label)
                    .id_salt(("struct", name.clone()))
                    .default_open(true)
                    .show(ui, |ui| {
                        let child_prefix = format!("{name}.");
                        for child in &fields {
                            let child_value =
                                value.get(&child.name).cloned().unwrap_or(Value::Null);
                            field_row_at(
                                ui,
                                registry,
                                child,
                                &child_value,
                                &child_prefix,
                                set,
                                assets,
                                doc_dir,
                            );
                        }
                    });
            }
        }
    }
}

/// 嵌套字段的灰色 JSON 兜底（列表、无法解析的 struct）。
fn json_row(ui: &mut Ui, label: &str, value: &Value) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(format_json(value))
                    .monospace()
                    .weak()
                    .size(10.0),
            );
        });
    });
}

/// `Option<T>` 有值时画哪个控件；完全由 `inner.widget()` 决定，不猜 JSON 类型。
fn default_value(field: &FieldSchema) -> Value {
    match field.widget() {
        FieldWidget::Bool => Value::Bool(false),
        FieldWidget::Integer => Value::from(0),
        FieldWidget::Float => Value::from(0.0),
        FieldWidget::Text | FieldWidget::Multiline => Value::String(String::new()),
        FieldWidget::Vector { len } => Value::Array(vec![Value::from(0.0); len]),
        FieldWidget::Color { alpha } => {
            let mut channels = vec![Value::from(1.0), Value::from(1.0), Value::from(1.0)];
            if alpha {
                channels.push(Value::from(1.0));
            }
            Value::Array(channels)
        }
        _ => Value::Null,
    }
}

fn optional_scalar(
    ui: &mut Ui,
    path: &str,
    inner: &FieldSchema,
    value: &Value,
    set: &mut dyn FnMut(&str, Value),
) {
    match inner.widget() {
        FieldWidget::Bool => {
            let mut checked = value.as_bool().unwrap_or(false);
            if ui.checkbox(&mut checked, "").changed() {
                set(path, Value::Bool(checked));
            }
        }
        FieldWidget::Integer => {
            let mut current = value.as_i64().unwrap_or(0);
            if ui
                .add(egui::DragValue::new(&mut current).speed(1.0))
                .changed()
            {
                set(path, Value::from(current));
            }
        }
        FieldWidget::Float => {
            let mut current = value.as_f64().unwrap_or(0.0) as f32;
            if ui
                .add(egui::DragValue::new(&mut current).speed(0.01))
                .changed()
            {
                set(path, Value::from(current as f64));
            }
        }
        FieldWidget::Text => {
            let mut current = value.as_str().unwrap_or_default().to_string();
            if ui.text_edit_singleline(&mut current).changed() {
                set(path, Value::String(current));
            }
        }
        _ => {
            ui.label(
                egui::RichText::new(format_json(value))
                    .monospace()
                    .weak()
                    .size(10.0),
            );
        }
    }
}

fn format_json(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 80 {
        format!("{}…", &text[..80])
    } else {
        text
    }
}

fn read_color(value: &Value, alpha: bool) -> [f32; 4] {
    let channel = |index: usize, fallback: f32| {
        value
            .get(index)
            .and_then(|item| item.as_f64())
            .map(|item| item as f32)
            .unwrap_or(fallback)
    };
    [
        channel(0, 1.0),
        channel(1, 1.0),
        channel(2, 1.0),
        if alpha { channel(3, 1.0) } else { 1.0 },
    ]
}

fn color_value(color: [f32; 4], alpha: bool) -> Value {
    let mut array = vec![
        Value::from(color[0] as f64),
        Value::from(color[1] as f64),
        Value::from(color[2] as f64),
    ];
    if alpha {
        array.push(Value::from(color[3] as f64));
    }
    Value::Array(array)
}
