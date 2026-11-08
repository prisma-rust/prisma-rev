//! 组件 schema：把 `prism-scene` 的反射注册表投影成前端属性面板可消费的形状。
//!
//! 前端不硬编码任何组件类型——拿到 [`Schema`] 后按字段 `kind`
//! （`bool` / `f32` / `string` / `struct` / `list` / `option` / `array`…）
//! 生成对应控件，`struct` 字段再按类型名递归查表。

use prism_reflect::{FieldDesc, FieldType, TypeRegistry};
use prism_scene::ComponentRegistry;
use serde::Serialize;

use crate::property_drawer::{PropertyDrawer, PropertyDrawerRegistry};

/// 一个类型的字段 schema。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FieldSchema {
    /// schema 字段名（作者 JSON 的键）。
    pub name: String,
    /// 控件类型标签：`bool` / `i32` / `f32` / `f64` / `string` /
    /// `struct` / `list` / `option` / `array`。
    pub kind: &'static str,
    /// `kind == "struct"` 时的嵌套类型名（前端按名字递归查 [`Schema`]）。
    pub struct_type: Option<String>,
    /// `list` / `option` / `array` 的元素 schema。
    pub inner: Option<Box<FieldSchema>>,
    /// `array` 的定长长度。
    pub array_len: Option<usize>,
    /// 枚举变体名（按声明顺序）。`Some` 时前端应渲染**下拉框**，
    /// 选项文字与写回值都是变体名（即作者 JSON 里的字符串）。
    pub variants: Option<Vec<String>>,
    /// 编辑器只读（`#[reflect(read_only)]`）。
    pub read_only: bool,
    /// 字段绘制器 = `PropertyDrawer`（`[Range]` / `[TextArea]` /
    /// `[ColorUsage]` / `[CustomPropertyDrawer]`）。缺省（不序列化）时前端按
    /// `kind` 选默认控件。见 [`crate::property_drawer`]。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drawer: Option<PropertyDrawer>,
}

/// 一个类型（组件或嵌套结构体）的 schema。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TypeSchema {
    /// 规范化类型名（如 `prism_engine::scene::Camera`）。
    pub name: String,
    /// 稳定类型 id（16 位十六进制）。
    pub type_id: String,
    /// schema 版本。
    pub version: u32,
    /// 是否为可挂到实体上的组件（`false` = 仅作嵌套字段类型）。
    pub is_component: bool,
    /// 字段列表（源码顺序）。
    pub fields: Vec<FieldSchema>,
    /// 组件「添加」时的默认作者 JSON（非组件为 `None`）。
    pub default_json: Option<serde_json::Value>,
}

/// 整个编辑器的类型表。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Schema {
    /// 组件在前、嵌套结构体在后，各自按类型名排序。
    pub types: Vec<TypeSchema>,
}

impl Schema {
    /// 按类型名查一项。
    pub fn type_by_name(&self, name: &str) -> Option<&TypeSchema> {
        self.types.iter().find(|ty| ty.name == name)
    }

    /// 所有可挂载组件的类型名（稳定顺序）。
    pub fn component_names(&self) -> Vec<&str> {
        self.types
            .iter()
            .filter(|ty| ty.is_component)
            .map(|ty| ty.name.as_str())
            .collect()
    }
}

/// 进程内共享的内置组件注册表。
///
/// 注册内置组件要建若干 `HashMap` 并注册整棵反射类型树，而字段写入 /
/// 规范化是**输入事件级**的热路径（拖动滑块时每帧都会调用），因此这里只
/// 构建一次。实例由 `prism_scene::builtin_registry` 提供，编辑器、引擎与
/// 工程模块共用同一份 schema。注册表构建后只读，回调都要求
/// `Send + Sync`，可以安全地跨线程共享。
///
/// 需要追加自定义组件的宿主仍然用 [`build_schema_with`] 传入自己的注册表，
/// 不受这里影响。
pub fn builtin_registry() -> &'static ComponentRegistry {
    prism_scene::builtin_registry()
}

/// 构建完整 schema（内置组件的唯一来源是 `prism-scene` 的注册表）。
///
/// 字段绘制器用 [`PropertyDrawerRegistry::with_builtin`] 的内置声明。
pub fn build_schema() -> Schema {
    build_schema_from(builtin_registry())
}

/// 用给定注册表 + 给定绘制器注册表构建 schema。
///
/// 宿主（未来插件、自定义组件）可以在内置声明之上追加自己的
/// `[CustomPropertyDrawer]`；字段级声明优先于类型级。
pub fn build_schema_with(registry: &ComponentRegistry, drawers: &PropertyDrawerRegistry) -> Schema {
    let component_names = registry.component_names();
    let types_registry = registry.type_registry();

    let mut types: Vec<TypeSchema> = types_registry
        .iter()
        .map(|(_, entry)| {
            let desc = entry.descriptor();
            let is_component = component_names.contains(&desc.name);
            TypeSchema {
                name: desc.name.to_string(),
                type_id: desc.id.to_string(),
                version: desc.version,
                is_component,
                fields: desc
                    .fields
                    .iter()
                    .map(|field| field_schema(field, types_registry, desc.name, drawers))
                    .collect(),
                default_json: if is_component {
                    registry.default_json(desc.name)
                } else {
                    None
                },
            }
        })
        .collect();

    // 组件优先，其后嵌套类型；同组按名字排序，保证 UI 顺序稳定。
    types.sort_by(|a, b| {
        b.is_component
            .cmp(&a.is_component)
            .then_with(|| a.name.cmp(&b.name))
    });

    Schema { types }
}

/// 用给定注册表构建 schema，绘制器用内置声明。测试可借此注入只含容器枚举
/// 字段的组件，不必为了覆盖 schema 逻辑往 `prism-scene` 里加真实组件。
fn build_schema_from(registry: &ComponentRegistry) -> Schema {
    build_schema_with(registry, &PropertyDrawerRegistry::with_builtin())
}

fn field_schema(
    field: &FieldDesc,
    registry: &TypeRegistry,
    declaring_type: &str,
    drawers: &PropertyDrawerRegistry,
) -> FieldSchema {
    let mut schema = leaf_schema(&field.ty, registry);
    schema.name = field.name.to_string();
    schema.read_only = field.read_only;
    let value_type = value_type_name(&schema);
    schema.drawer = drawers
        .drawer_for_field(declaring_type, field.name, value_type)
        .cloned();
    if let Some(variants) = field.enum_variants {
        attach_variants(
            &mut schema,
            variants.iter().map(|name| (*name).to_string()).collect(),
        );
    }
    schema
}

fn value_type_name(schema: &FieldSchema) -> Option<&str> {
    if let Some(ref st) = schema.struct_type {
        return Some(st.as_str());
    }
    if let Some(ref inner) = schema.inner {
        return value_type_name(inner);
    }
    None
}

/// 把枚举变体挂到**最内层**的标量 leaf 上。
///
/// `enum_variants()` 由 `Vec<T>` / `Option<T>` / `[T; N]` 向内层转发，所以
/// `Vec<Enum>` 字段拿到的是 `inner`（`u8`）该有的变体名；前端只有叶子控件
/// （以及数组/列表元素自己的 `FieldEditor`）会渲染下拉框。
fn attach_variants(schema: &mut FieldSchema, variants: Vec<String>) {
    match schema.inner.as_mut() {
        Some(inner) if matches!(schema.kind, "list" | "option" | "array") => {
            attach_variants(inner, variants);
        }
        _ => schema.variants = Some(variants),
    }
}

/// 生成无名字段 schema（供 list/option/array 的元素与顶层复用）。
fn leaf_schema(ty: &FieldType, registry: &TypeRegistry) -> FieldSchema {
    let (kind, struct_type, inner, array_len) = match ty {
        FieldType::Bool => ("bool", None, None, None),
        FieldType::I8 => ("i8", None, None, None),
        FieldType::I16 => ("i16", None, None, None),
        FieldType::I32 => ("i32", None, None, None),
        FieldType::I64 => ("i64", None, None, None),
        FieldType::U8 => ("u8", None, None, None),
        FieldType::U16 => ("u16", None, None, None),
        FieldType::U32 => ("u32", None, None, None),
        FieldType::U64 => ("u64", None, None, None),
        FieldType::F32 => ("f32", None, None, None),
        FieldType::F64 => ("f64", None, None, None),
        FieldType::String => ("string", None, None, None),
        FieldType::Struct(id) => (
            "struct",
            registry.descriptor(*id).map(|desc| desc.name.to_string()),
            None,
            None,
        ),
        FieldType::List(inner) => (
            "list",
            None,
            Some(Box::new(leaf_schema(inner, registry))),
            None,
        ),
        FieldType::Optional(inner) => (
            "option",
            None,
            Some(Box::new(leaf_schema(inner, registry))),
            None,
        ),
        FieldType::Array(inner, len) => (
            "array",
            None,
            Some(Box::new(leaf_schema(inner, registry))),
            Some(*len),
        ),
    };

    FieldSchema {
        name: String::new(),
        kind,
        struct_type,
        inner,
        array_len,
        variants: None,
        read_only: false,
        drawer: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_scene::register_builtin_components;

    #[test]
    fn schema_exposes_enum_variants_for_dropdowns() {
        let schema = build_schema();

        // `Camera.clear_flags` 是 `CameraClearFlags`：u8 编码 + 变体名。
        let camera = schema
            .type_by_name("prism_engine::scene::Camera")
            .expect("Camera schema");
        let clear_flags = camera
            .fields
            .iter()
            .find(|f| f.name == "clear_flags")
            .expect("Camera.clear_flags");
        assert_eq!(clear_flags.kind, "u8");
        assert_eq!(
            clear_flags.variants.as_deref(),
            Some(
                &[
                    "Skybox".to_string(),
                    "SolidColor".to_string(),
                    "Depth".to_string(),
                    "Nothing".to_string()
                ][..]
            )
        );

        // `Text.alignment` 走同一套机制（证明不是为 Camera 特判的）。
        let text = schema
            .type_by_name("prism_engine::ui::Text")
            .expect("Text schema");
        let alignment = text
            .fields
            .iter()
            .find(|f| f.name == "alignment")
            .expect("Text.alignment");
        assert_eq!(
            alignment.variants.as_deref(),
            Some(
                &[
                    "Center".to_string(),
                    "Left".to_string(),
                    "Right".to_string()
                ][..]
            )
        );

        // 普通数值字段没有变体。
        let exposure = camera.fields.iter().find(|f| f.name == "exposure").unwrap();
        assert!(exposure.variants.is_none());
    }

    #[test]
    fn schema_exposes_components_and_nested_types() {
        let schema = build_schema();

        // 组件优先排序，且每个组件都带默认值。
        let first_non_component = schema
            .types
            .iter()
            .position(|ty| !ty.is_component)
            .unwrap_or(schema.types.len());
        assert!(schema.types[..first_non_component]
            .iter()
            .all(|ty| ty.is_component));
        assert!(schema.types[first_non_component..]
            .iter()
            .all(|ty| !ty.is_component));

        let camera = schema
            .type_by_name("prism_engine::scene::Camera")
            .expect("Camera schema");
        assert!(camera.is_component);
        assert!(camera.default_json.is_some());

        // `aspect` 是只读字段，`enabled` 不是。
        assert!(
            camera
                .fields
                .iter()
                .find(|f| f.name == "aspect")
                .unwrap()
                .read_only
        );
        assert!(
            !camera
                .fields
                .iter()
                .find(|f| f.name == "enabled")
                .unwrap()
                .read_only
        );

        // 嵌套类型 `glam::Vec3` 出现在表里，且 Style.anchors 指向 struct。
        assert!(schema.type_by_name("glam::Vec3").is_some());
        assert!(schema.type_by_name("prism_engine::ui::Anchors").is_some());

        let style = schema.type_by_name("prism_engine::ui::Style").unwrap();
        let anchors = style.fields.iter().find(|f| f.name == "anchors").unwrap();
        assert_eq!(anchors.kind, "struct");
        assert_eq!(
            anchors.struct_type.as_deref(),
            Some("prism_engine::ui::Anchors")
        );
        let width = style.fields.iter().find(|f| f.name == "width").unwrap();
        assert_eq!(width.kind, "option");
        assert_eq!(width.inner.as_ref().unwrap().kind, "f32");

        // 非组件类型没有默认值。
        let vec3 = schema.type_by_name("glam::Vec3").unwrap();
        assert!(!vec3.is_component);
        assert!(vec3.default_json.is_none());
        assert_eq!(vec3.fields.len(), 3);
    }

    #[test]
    fn array_fields_report_length_and_element_kind() {
        let schema = build_schema();
        let layout = schema
            .type_by_name("prism_engine::ui::ComputedLayout")
            .unwrap();
        let rect = layout.fields.iter().find(|f| f.name == "rect").unwrap();
        assert_eq!(rect.kind, "array");
        assert_eq!(rect.array_len, Some(4));
        assert_eq!(rect.inner.as_ref().unwrap().kind, "f32");
    }

    // —— 字段绘制器（`PropertyDrawer` / `PropertyAttribute`） ——

    #[test]
    fn builtin_drawers_reach_editor_fields() {
        use crate::property_drawer::PropertyRenderer;

        let schema = build_schema();
        let field = |type_name: &str, field: &str| {
            schema
                .type_by_name(type_name)
                .unwrap_or_else(|| panic!("missing type {type_name}"))
                .fields
                .iter()
                .find(|f| f.name == field)
                .unwrap_or_else(|| panic!("missing field {type_name}.{field}"))
                .drawer
                .clone()
        };

        // `[Range]` → 滑动条 + 上下界。
        let exposure = field("prism_engine::scene::Camera", "exposure").expect("Camera.exposure");
        assert_eq!(exposure.renderer, PropertyRenderer::Slider);
        assert_eq!(exposure.range_bounds(), Some((0.0, 5.0)));
        assert!(exposure.tooltip().is_some());

        // `[ColorUsage]` → 取色盘。
        let background =
            field("prism_engine::scene::Camera", "background_color").expect("background_color");
        assert_eq!(background.renderer, PropertyRenderer::Color);
        assert_eq!(background.color_usage_flags(), Some((true, false)));

        // `[TextArea]` → 多行文本框。
        let content = field("prism_engine::ui::Text", "content").expect("Text.content");
        assert_eq!(content.renderer, PropertyRenderer::TextArea);
        assert_eq!(content.line_range(), Some((2, 6)));

        // 没有声明的字段不带 drawer（前端退回默认控件）。
        assert!(field("prism_engine::scene::Camera", "aspect").is_none());
    }

    #[test]
    fn builtin_registry_is_shared_and_matches_a_fresh_one() {
        // 同一进程只构建一次（字段写入是输入事件热路径）。
        assert!(std::ptr::eq(builtin_registry(), builtin_registry()));

        // 缓存内容必须与手工注册的结果一致，避免把初始化写歪。
        let mut fresh = ComponentRegistry::new();
        register_builtin_components(&mut fresh);
        assert_eq!(
            builtin_registry().component_names(),
            fresh.component_names()
        );

        // `build_schema()` 走的就是这份共享注册表。
        assert_eq!(
            build_schema().component_names(),
            builtin_registry().component_names()
        );
    }

    #[test]
    fn custom_drawers_can_be_added_on_top_of_builtins() {
        use crate::property_drawer::{PropertyDrawer, PropertyDrawerRegistry, PropertyRenderer};

        let mut drawers = PropertyDrawerRegistry::with_builtin();
        drawers.register_custom(
            "prism_engine::scene::Camera",
            "background_color",
            "LinearColorEditor",
        );
        drawers.register(
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            PropertyDrawer::range(10.0, 120.0),
        );

        let mut registry = ComponentRegistry::new();
        register_builtin_components(&mut registry);
        let custom_schema = build_schema_with(&registry, &drawers);

        let camera = custom_schema
            .type_by_name("prism_engine::scene::Camera")
            .unwrap();
        let background = camera
            .fields
            .iter()
            .find(|f| f.name == "background_color")
            .unwrap()
            .drawer
            .clone()
            .expect("custom drawer");
        assert_eq!(background.renderer, PropertyRenderer::Custom);
        assert_eq!(background.custom.as_deref(), Some("LinearColorEditor"));

        // 同字段的后注册声明覆盖内置的 `[Range(1,179)]`。
        let fov = camera
            .fields
            .iter()
            .find(|f| f.name == "fov_y_degrees")
            .unwrap()
            .drawer
            .clone()
            .expect("fov drawer");
        assert_eq!(fov.range_bounds(), Some((10.0, 120.0)));

        // 未覆盖的内置声明仍在。
        let exposure = camera
            .fields
            .iter()
            .find(|f| f.name == "exposure")
            .unwrap()
            .drawer
            .clone()
            .expect("exposure drawer");
        assert_eq!(exposure.range_bounds(), Some((0.0, 5.0)));

        // 类型级绘制器（`[CustomPropertyDrawer(typeof(Anchors))]`）：
        // 凡是值类型为 `prism_engine::ui::Anchors` 的字段（如 `Style.anchors`）都获得该绘制器。
        drawers.register_type(
            "prism_engine::ui::Anchors",
            PropertyDrawer::custom("AnchorGizmo"),
        );
        let custom_schema_with_type = build_schema_with(&registry, &drawers);
        let style = custom_schema_with_type
            .type_by_name("prism_engine::ui::Style")
            .expect("Style component");
        let anchors_field = style
            .fields
            .iter()
            .find(|f| f.name == "anchors")
            .expect("Style.anchors");
        let anchors_drawer = anchors_field
            .drawer
            .as_ref()
            .expect("anchors type-level drawer");
        assert_eq!(anchors_drawer.renderer, PropertyRenderer::Custom);
        assert_eq!(anchors_drawer.custom.as_deref(), Some("AnchorGizmo"));

        // 宿主注册表是局部覆盖：默认 schema 仍是内置的 `[ColorUsage]`。
        let builtin_schema = build_schema();
        let builtin_background = builtin_schema
            .type_by_name("prism_engine::scene::Camera")
            .and_then(|ty| ty.fields.iter().find(|f| f.name == "background_color"))
            .and_then(|f| f.drawer.as_ref())
            .expect("builtin color drawer");
        assert_eq!(builtin_background.renderer, PropertyRenderer::Color);
    }

    // —— 容器里的枚举：`Vec<Enum>` / `Option<Enum>` / `[Enum; N]` ——
    use prism_reflect::{FieldType, Reflect, ReflectError, ReflectType, Value};

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    enum TestAlign {
        #[default]
        Left,
        Center,
    }

    impl ReflectType for TestAlign {
        fn field_type() -> FieldType {
            FieldType::U8
        }

        fn enum_variants() -> Option<&'static [&'static str]> {
            Some(&["Left", "Center"])
        }

        fn to_value(&self) -> Value {
            Value::U64(u64::from(*self as u8))
        }

        fn from_value(value: Value) -> Result<Self, ReflectError> {
            match value.as_u64()? {
                0 => Ok(Self::Left),
                1 => Ok(Self::Center),
                other => Err(ReflectError::BadData(format!("invalid TestAlign: {other}"))),
            }
        }
    }

    #[derive(Debug, Default, Reflect)]
    #[reflect(name = "test::ContainerEnums")]
    struct ContainerEnums {
        flags: Vec<TestAlign>,
        maybe: Option<TestAlign>,
        fixed: [TestAlign; 2],
    }

    #[test]
    fn container_enum_variants_reach_the_inner_leaf() {
        let mut registry = ComponentRegistry::new();
        registry.register_reflect::<ContainerEnums>();
        let schema = build_schema_from(&registry);
        let ty = schema
            .type_by_name("test::ContainerEnums")
            .expect("test component schema");
        let field = |name: &str| ty.fields.iter().find(|f| f.name == name).unwrap();
        let expected = || Some(vec!["Left".to_string(), "Center".to_string()]);

        // 容器本身不带变体，变体在最内层的标量 leaf 上。
        for (name, kind) in [("flags", "list"), ("maybe", "option"), ("fixed", "array")] {
            let schema = field(name);
            assert_eq!(schema.kind, kind, "{name} kind");
            assert_eq!(
                schema.variants, None,
                "{name} must not carry variants itself"
            );
            let inner = schema.inner.as_ref().expect("container inner schema");
            assert_eq!(inner.kind, "u8", "{name} inner kind");
            assert_eq!(inner.variants, expected(), "{name} inner variants");
        }

        assert_eq!(field("fixed").array_len, Some(2));
    }
}
