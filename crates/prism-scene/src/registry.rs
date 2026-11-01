//! ComponentRegistry — 通用组件反序列化注册表。
//!
//! 场景文件（.scene.json）中的每个实体携带一个组件列表，
//! 每个组件由其类型名（全限定路径）标识。运行时加载时通过
//! [`ComponentRegistry`] 查找对应的反序列化工厂，将 JSON 数据还原为
//! ECS 组件并插入实体。
//!
//! 内置组件通过 [`register_builtin_components`] 批量注册。

use std::collections::HashMap;

use prism_ecs::{Entity, World};
use prism_reflect::{Object, Reflect, ReflectError, TypeDesc, TypeId, TypeRegistry, Value};
use serde::{de::DeserializeOwned, Serialize};

// ---------------------------------------------------------------------------
// ComponentRegistry
// ---------------------------------------------------------------------------

type DeserializerFn = Box<dyn Fn(&mut World, Entity, &serde_json::Value) + Send + Sync>;

type JsonToObjectFn = Box<dyn Fn(&serde_json::Value) -> Option<Object> + Send + Sync>;

/// 产出某个组件的**作者 JSON 默认形状**（编辑器「添加组件」用）。
type DefaultJsonFn = Box<dyn Fn() -> serde_json::Value + Send + Sync>;

/// 反射对象 → 作者 JSON（编辑器「基于反射改字段后写回」用）。
type ObjectToJsonFn = Box<dyn Fn(&Object) -> Option<serde_json::Value> + Send + Sync>;

type ReflectApplyFn = fn(&mut World, Entity, &Object) -> bool;
type ReflectExtractFn = fn(&World, Entity) -> Option<Object>;
/// 在反射对象上按字段名写入（`Object -> C -> set_field -> Object`）。
type ReflectSetFn = fn(&mut Object, &str, Value) -> Result<(), ReflectError>;
/// 列出拥有该组件的实体（类型擦除的 `World::query`）。
type ReflectQueryFn = fn(&World) -> Vec<Entity>;
/// 移除该组件并报告它是否存在（类型擦除的 `World::remove`）。
type ReflectRemoveFn = fn(&mut World, Entity) -> bool;

/// 一个组件的反射适配器：`Object <-> 具体 ECS 组件`。
struct ReflectEntry {
    desc: &'static TypeDesc,
    apply: ReflectApplyFn,
    extract: ReflectExtractFn,
    set_field: ReflectSetFn,
    query: ReflectQueryFn,
    remove: ReflectRemoveFn,
}

/// 组件反序列化注册表。
///
/// 同时持有一条 **反射路径**：`prism-reflect` 的 `Object` 可以绕过 serde，
/// 直接还原成组件。后者是 RTTIBin / 编辑器的通用入口，前者保留给
/// `.scene.json` 作者格式。
pub struct ComponentRegistry {
    deserializers: HashMap<String, DeserializerFn>,
    reflect: TypeRegistry,
    reflect_entries: HashMap<TypeId, ReflectEntry>,
    name_to_id: HashMap<String, TypeId>,
    json_to_object: HashMap<String, JsonToObjectFn>,
    object_to_json: HashMap<String, ObjectToJsonFn>,
    defaults: HashMap<String, DefaultJsonFn>,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self {
            deserializers: HashMap::new(),
            reflect: TypeRegistry::new(),
            reflect_entries: HashMap::new(),
            name_to_id: HashMap::new(),
            json_to_object: HashMap::new(),
            object_to_json: HashMap::new(),
            defaults: HashMap::new(),
        }
    }

    /// 注册一个 serde 组件类型。
    pub fn register<C>(&mut self, name: &str)
    where
        C: DeserializeOwned + Send + Sync + 'static,
    {
        let name = name.to_string();
        self.deserializers.insert(
            name,
            Box::new(move |world, entity, value| {
                match serde_json::from_value::<C>(value.clone()) {
                    Ok(comp) => {
                        world.insert(entity, comp);
                    }
                    Err(e) => {
                        log::warn!(
                            "ComponentRegistry: failed to deserialize '{}': {e}",
                            std::any::type_name::<C>(),
                        );
                    }
                }
            }),
        );
    }

    /// 注册一个自定义反序列化器。
    pub fn register_with<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&mut World, Entity, &serde_json::Value) + 'static + Send + Sync,
    {
        self.deserializers.insert(name.to_string(), Box::new(f));
    }

    /// 注册一个反射组件类型。
    ///
    /// 类型名取自 `#[reflect(name = "...")]`（应与 `.scene.json` 中的组件名
    /// 一致），因此调用方不再重复传名字。
    pub fn register_reflect<C>(&mut self)
    where
        C: Reflect + Send + Sync,
    {
        let desc = C::type_desc();
        self.reflect.register::<C>();
        self.name_to_id.insert(desc.name.to_owned(), desc.id);
        self.reflect_entries.insert(
            desc.id,
            ReflectEntry {
                desc,
                apply: |world, entity, object| match <C as Reflect>::from_object(object) {
                    Ok(component) => {
                        world.insert(entity, component);
                        true
                    }
                    Err(err) => {
                        log::warn!(
                            "ComponentRegistry: failed to reflect-restore '{}': {err}",
                            <C as Reflect>::type_desc().name,
                        );
                        false
                    }
                },
                extract: |world, entity| {
                    world
                        .get::<C>(entity)
                        .map(|component| <C as Reflect>::to_object(component))
                },
                set_field: |object, field, value| {
                    let mut component = <C as Reflect>::from_object(object)?;
                    component.set_field(field, value)?;
                    *object = component.to_object();
                    Ok(())
                },
                query: |world| world.query::<C>().map(|(entity, _)| entity).collect(),
                remove: |world, entity| world.remove::<C>(entity).is_some(),
            },
        );
    }

    /// 在反射对象上按字段名写入，返回写入后的对象。
    ///
    /// 走的是 [`Reflect::set_field`]：`#[reflect(read_only)]` 字段会被拒绝，
    /// 值类型也必须是该字段声明的反射类型。类型未注册时返回
    /// [`ReflectError::NotRegistered`]。
    pub fn set_object_field(
        &self,
        object: &mut Object,
        field: &str,
        value: Value,
    ) -> Result<(), ReflectError> {
        let entry = self
            .reflect_entries
            .get(&object.type_id)
            .ok_or(ReflectError::NotRegistered(object.type_id))?;
        (entry.set_field)(object, field, value)
    }

    /// 注册一个既支持 serde 反序列化、又支持反射的组件。
    ///
    /// `name` 必须与 `#[reflect(name = "...")]` 一致（场景 JSON 的键）。
    /// 同时登记 JSON → `Object` 的转换器，供 cook 工具把作者 JSON
    /// 转成紧凑的 RSCN 载荷。
    pub fn register_reflect_serde<C>(&mut self, name: &str)
    where
        C: Reflect + DeserializeOwned + Serialize + Send + Sync + 'static,
    {
        self.register::<C>(name);
        self.register_reflect::<C>();
        let key = name.to_string();
        self.json_to_object.insert(
            key.clone(),
            Box::new(move |value| {
                serde_json::from_value::<C>(value.clone())
                    .ok()
                    .map(|component| <C as Reflect>::to_object(&component))
            }),
        );
        // 编辑器「添加组件」的默认作者 JSON：`C::default()` 的 serde 形状。
        self.defaults.insert(
            key.clone(),
            Box::new(|| serde_json::to_value(C::default()).unwrap_or(serde_json::Value::Null)),
        );
        // 反射对象 → 作者 JSON（`Object -> C -> serde JSON`）。
        self.object_to_json.insert(
            key,
            Box::new(|object| {
                let component = <C as Reflect>::from_object(object).ok()?;
                serde_json::to_value(component).ok()
            }),
        );
    }

    /// 手工登记「反射对象 → 作者 JSON」转换器。
    ///
    /// 用于 `Name` 这类 serde 形状与派生默认不一致的组件。
    pub fn register_object_to_json<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&Object) -> Option<serde_json::Value> + Send + Sync + 'static,
    {
        self.object_to_json.insert(name.to_string(), Box::new(f));
    }

    /// 反射对象 → 作者 JSON（未登记返回 `None`）。
    pub fn object_to_json(&self, name: &str, object: &Object) -> Option<serde_json::Value> {
        self.object_to_json.get(name).and_then(|f| f(object))
    }

    /// 为某个组件手工登记作者 JSON 默认形状。
    ///
    /// 用于 `Name` 这类 serde 形状与派生默认不一致的组件
    /// （`register_reflect_serde` 无法覆盖）。
    pub fn set_default_json(&mut self, name: &str, value: serde_json::Value) {
        self.defaults
            .insert(name.to_string(), Box::new(move || value.clone()));
    }

    /// 某个组件的作者 JSON 默认形状（编辑器「添加组件」用）。
    ///
    /// 未登记返回 `None`；调用方可回退到 `serde_json::Value::Object` 空表。
    pub fn default_json(&self, name: &str) -> Option<serde_json::Value> {
        self.defaults.get(name).map(|f| f())
    }

    /// 所有已登记组件的类型名（稳定顺序，供编辑器组件列表）。
    pub fn component_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self
            .reflect_entries
            .values()
            .map(|entry| entry.desc.name)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// 为已有反射组件登记一个自定义 JSON → `Object` 转换器。
    ///
    /// 用于 `Name` 这种 JSON 形状与派生 serde 默认形状不一致的组件。
    pub fn register_json_object<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&serde_json::Value) -> Option<Object> + Send + Sync + 'static,
    {
        self.json_to_object.insert(name.to_string(), Box::new(f));
    }

    /// 把作者 JSON 转成反射对象（cook 工具路径）。
    pub fn object_from_json(&self, name: &str, value: &serde_json::Value) -> Option<Object> {
        self.json_to_object.get(name).and_then(|f| f(value))
    }

    /// 只注册类型描述（不注册为组件）。
    ///
    /// 用于组件字段里出现的嵌套类型（如 `glam::Vec3` / `glam::Quat`）：
    /// `rtti_bin::encode` 收集类型表时要求这些类型也已在 `TypeRegistry` 中。
    pub fn register_reflect_type<T: Reflect>(&mut self) {
        self.reflect.register::<T>();
    }

    /// 反序列化并插入一个组件。
    pub fn apply(&self, world: &mut World, entity: Entity, name: &str, data: &serde_json::Value) {
        match self.deserializers.get(name) {
            Some(f) => f(world, entity, data),
            None => {
                log::warn!("ComponentRegistry: unknown component '{name}' — skipping");
            }
        }
    }

    /// 用反射对象插入组件（RTTIBin / 编辑器路径）。
    ///
    /// 返回 `false` 表示类型未注册或字段还原失败。
    pub fn apply_object(&self, world: &mut World, entity: Entity, object: &Object) -> bool {
        match self.reflect_entries.get(&object.type_id) {
            Some(entry) => (entry.apply)(world, entity, object),
            None => {
                log::warn!(
                    "ComponentRegistry: unknown reflected component '{}' ({}) — skipping",
                    object.type_name,
                    object.type_id,
                );
                false
            }
        }
    }

    /// 从实体提取反射对象（用于保存 / 编辑器）。
    pub fn extract_object(&self, world: &World, entity: Entity, type_id: TypeId) -> Option<Object> {
        let entry = self.reflect_entries.get(&type_id)?;
        (entry.extract)(world, entity)
    }

    /// 按类型名提取反射对象。
    pub fn extract_object_by_name(
        &self,
        world: &World,
        entity: Entity,
        name: &str,
    ) -> Option<Object> {
        let type_id = self.name_to_id.get(name)?;
        self.extract_object(world, entity, *type_id)
    }

    /// 类型名 → 稳定类型 id。
    pub fn type_id(&self, name: &str) -> Option<TypeId> {
        self.name_to_id.get(name).copied()
    }

    /// 列出拥有组件 `type_id` 的实体（类型擦除，供宿主 ABI 使用）。
    ///
    /// 返回 `None` 表示该类型没有注册反射组件。
    pub fn entities_with(&self, world: &World, type_id: TypeId) -> Option<Vec<Entity>> {
        let entry = self.reflect_entries.get(&type_id)?;
        Some((entry.query)(world))
    }

    /// 按 `type_id` 移除组件，返回它是否存在过。
    ///
    /// 类型未注册时返回 `false`（与「组件不存在」一致，调用方无需区分）。
    pub fn remove_component(&self, world: &mut World, entity: Entity, type_id: TypeId) -> bool {
        match self.reflect_entries.get(&type_id) {
            Some(entry) => (entry.remove)(world, entity),
            None => false,
        }
    }

    /// 类型描述（编辑器属性面板 / schema 浏览用）。
    pub fn descriptor(&self, type_id: TypeId) -> Option<&'static TypeDesc> {
        self.reflect_entries.get(&type_id).map(|entry| entry.desc)
    }

    /// 底层 `TypeRegistry`，供 `rtti_bin::encode` 收集类型表。
    pub fn type_registry(&self) -> &TypeRegistry {
        &self.reflect
    }

    /// 已注册的反射组件数量。
    pub fn reflect_component_count(&self) -> usize {
        self.reflect_entries.len()
    }

    /// Compute a deterministic schema fingerprint of all reflected components.
    ///
    /// Changes whenever any component is added, removed, or has its schema / fields
    /// changed (including nested types via `TypeDesc::schema_hash`).
    pub fn schema_fingerprint(&self) -> u64 {
        const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut entries: Vec<(&str, u64)> = self
            .reflect_entries
            .values()
            .map(|entry| {
                let hash = entry.desc.schema_hash(&self.reflect);
                (entry.desc.name, hash)
            })
            .collect();
        entries.sort_unstable_by_key(|&(name, _)| name);

        let mut hash = FNV_OFFSET_BASIS;
        for (name, schema_hash) in entries {
            for byte in name.as_bytes() {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            hash ^= schema_hash;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 内置组件注册
// ---------------------------------------------------------------------------

/// 注册所有内置场景与 UI 组件到 registry。
///
/// 这是作者 JSON、cook 工具和运行时共用的唯一 schema 入口。
/// Returns the builtin components schema fingerprint.
pub fn builtin_schema_fingerprint() -> u64 {
    static FINGERPRINT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FINGERPRINT.get_or_init(|| {
        let mut registry = ComponentRegistry::new();
        register_builtin_components(&mut registry);
        registry.schema_fingerprint()
    })
}

pub fn register_builtin_components(registry: &mut ComponentRegistry) {
    use crate::components::*;
    #[cfg(feature = "group-ui")]
    use crate::ui::{
        Anchors, Button, Canvas, ComputedLayout, Interaction, Layout, Margin, Node, Pivot, Style,
        Text,
    };

    // ── 场景基础组件 ──
    registry.register_reflect_serde::<Active>("prism_engine::scene::Active");

    // Name 使用自定义 JSON 形状（字符串，而不是 {"0": ...}）。
    registry.register_with("prism_engine::scene::Name", |world, entity, value| {
        if let Some(s) = value.as_str() {
            world.insert(entity, Name(s.to_string()));
        } else {
            log::warn!("ComponentRegistry: Name requires a string value, got {value:?}");
        }
    });
    registry.register_reflect::<Name>();
    registry.register_json_object("prism_engine::scene::Name", |value| {
        value.as_str().map(|s| Name(s.to_string()).to_object())
    });
    registry.set_default_json("prism_engine::scene::Name", serde_json::json!("entity"));
    registry.register_object_to_json("prism_engine::scene::Name", |object| {
        object
            .get("0")
            .and_then(|value| value.as_str().ok())
            .map(|name| serde_json::json!(name))
    });

    // ── 光源 ──
    #[cfg(feature = "group-3d")]
    {
        registry.register_reflect_serde::<Light>("prism_engine::scene::Light");
        registry
            .register_reflect_serde::<DirectionalLight>("prism_engine::scene::DirectionalLight");
        registry.register_reflect_serde::<PointLight>("prism_engine::scene::PointLight");
        registry.register_reflect_serde::<SpotLight>("prism_engine::scene::SpotLight");

        // ── 声明式光照（场景驱动 IBL / GI，避免引擎启动无条件加载）──
        registry.register_reflect_serde::<EnvironmentLighting>(
            "prism_engine::scene::EnvironmentLighting",
        );
        registry.register_reflect_serde::<GiConfig>("prism_engine::scene::GiConfig");
    }

    // ── 相机 ──
    registry.register_reflect_serde::<Camera>("prism_engine::scene::Camera");
    #[cfg(feature = "group-3d")]
    {
        registry.register_reflect_serde::<FlyCameraController>(
            "prism_engine::scene::FlyCameraController",
        );
    }

    // ── 天空盒 / 渲染 bundle ──
    #[cfg(feature = "group-3d")]
    {
        registry.register_reflect_serde::<Skybox>("prism_engine::scene::Skybox");
        registry.register_reflect_serde::<Gizmo>("prism_engine::scene::Gizmo");
        registry.register_reflect_serde::<MeshRenderer>("prism_engine::scene::MeshRenderer");
    }

    // ── 2D 精灵（作者态；GPU 句柄由运行时 `SpriteRef` 持有）──
    #[cfg(feature = "group-2d")]
    {
        registry.register_reflect_serde::<SpriteRenderer>("prism_engine::scene::SpriteRenderer");
        registry.register_reflect_serde::<LineRenderer>("prism_engine::scene::LineRenderer");
        registry.register_reflect_serde::<SpriteAnimation>("prism_engine::scene::SpriteAnimation");
        registry.register_reflect_serde::<Tilemap>("prism_engine::scene::Tilemap");
        // #611：2D 精灵光照（与 SpriteRenderer 同组，没有精灵就没有意义）。
        registry.register_reflect_serde::<Light2D>("prism_engine::scene::Light2D");
    }

    // ── 本地变换 ──
    // 只登记**类型**（RTTIBin 类型表 / 编辑器字段描述），不登记为可挂载组件：
    // 作者只写 `EntityJson::transform`，由 loader / cooker bake 成
    // `LocalTransform` + `LocalToWorld`（作者态那个不可移除的 Transform）。
    // 若登记成组件，`registry.apply` 会在 `LocalToWorld` 计算完之后覆盖
    // `LocalTransform`，留下一个按旧变换算出的世界矩阵。
    registry.register_reflect_type::<LocalTransform>();

    // ── 关键帧动画 ──
    // 轨道里的是嵌套结构体，先登记类型（RTTIBin 类型表 / 编辑器字段描述），
    // 再登记组件本身。
    registry.register_reflect_type::<crate::animation::Vec3Key>();
    registry.register_reflect_type::<crate::animation::AnimationEvent>();
    registry.register_reflect_serde::<KeyframeAnimation>("prism_engine::scene::KeyframeAnimation");

    // ── 粒子发射器（作者态；活粒子在运行时 `ParticleEmitter` 里）──
    registry.register_reflect_serde::<ParticleSystem>("prism_engine::scene::ParticleSystem");
    registry.register_reflect_serde::<ParticleEmitter2D>("prism_engine::scene::ParticleEmitter2D");

    // ── 音频（作者态；解码与混音在宿主层 `prism-app::scene_audio`）──
    registry.register_reflect_serde::<AudioSource>("prism_engine::scene::AudioSource");
    registry.register_reflect_serde::<Terrain>("prism_engine::scene::Terrain");
    registry.register_reflect_serde::<SkeletonRoot>("prism_engine::scene::SkeletonRoot");
    registry
        .register_reflect_serde::<SkinnedMeshRenderer>("prism_engine::scene::SkinnedMeshRenderer");
    registry.register_reflect_serde::<Collider>("prism_engine::scene::Collider");
    registry.register_reflect_serde::<Rigidbody>("prism_engine::scene::Rigidbody");
    registry.register_reflect_serde::<AudioListener>("prism_engine::scene::AudioListener");

    // ── 预制体实例（编辑器元数据）──
    registry.register_reflect_serde::<PrefabInstance>("prism_engine::scene::PrefabInstance");

    // ── UI 组件 ──
    #[cfg(feature = "group-ui")]
    {
        registry.register_reflect_serde::<Layout>("prism_engine::ui::Layout");
        registry.register_reflect_serde::<Node>("prism_engine::ui::Node");
        registry.register_reflect_serde::<Style>("prism_engine::ui::Style");
        // `ComputedLayout` 是布局系统的输出（`rect` 每帧被重写），由
        // `ui::layout` 自己 ensure；只登记类型，不让作者挂载。
        registry.register_reflect_type::<ComputedLayout>();
        registry.register_reflect_serde::<Text>("prism_engine::ui::Text");
        // #528：可交互 UI 的组件必须能在场景 / 预制体里保存。
        registry.register_reflect_serde::<Interaction>("prism_engine::ui::Interaction");
        registry.register_reflect_serde::<Button>("prism_engine::ui::Button");
        // #463：画布缩放（参考分辨率 → 实际屏幕）。
        registry.register_reflect_serde::<Canvas>("prism_engine::ui::Canvas");
    }

    // ── 嵌套类型（类型表/cook 需要）──
    registry.register_reflect_type::<glam::Vec2>();
    registry.register_reflect_type::<glam::Vec3>();
    registry.register_reflect_type::<glam::Vec4>();
    registry.register_reflect_type::<glam::Quat>();
    #[cfg(feature = "group-ui")]
    {
        registry.register_reflect_type::<Anchors>();
        registry.register_reflect_type::<Pivot>();
        registry.register_reflect_type::<Margin>();
        // `Style.layout` 的嵌套类型（#527 / #709）：漏登记会让所有带 `Style`
        // 的场景在 RSCN 编码时以 `type ... is not registered` 失败。
        registry.register_reflect_type::<Layout>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Camera, LocalTransform, Name};
    use prism_reflect::rtti_bin;

    fn test_registry() -> ComponentRegistry {
        let mut registry = ComponentRegistry::new();
        registry.register_reflect_type::<glam::Vec3>();
        registry.register_reflect_type::<glam::Quat>();
        registry.register_reflect::<Camera>();
        registry.register_reflect::<Name>();
        registry.register_reflect::<LocalTransform>();
        registry
    }

    #[test]
    fn reflected_components_roundtrip_through_rtti_bin() {
        let registry = test_registry();
        let camera = Camera {
            fov_y_degrees: 72.0,
            ..Camera::default()
        };
        let name = Name("player".into());
        let transform = LocalTransform {
            position: glam::Vec3::new(1.0, 2.0, 3.0),
            ..LocalTransform::default()
        };

        let objects = vec![camera.to_object(), name.to_object(), transform.to_object()];
        let bytes = rtti_bin::encode(&objects, registry.type_registry()).unwrap();
        let decoded = rtti_bin::decode(&bytes).unwrap();

        let mut world = World::new();
        let entity = world.spawn();
        for object in &decoded.objects {
            assert!(registry.apply_object(&mut world, entity, object));
        }

        assert_eq!(world.get::<Camera>(entity).unwrap().fov_y_degrees, 72.0);
        assert_eq!(world.get::<Name>(entity).unwrap().0, "player");
        assert_eq!(
            world.get::<LocalTransform>(entity).unwrap().position,
            glam::Vec3::new(1.0, 2.0, 3.0)
        );

        // 导出走同一份 schema。
        let extracted = registry
            .extract_object_by_name(&world, entity, "prism_engine::scene::Camera")
            .unwrap();
        assert_eq!(extracted, camera.to_object());
    }

    #[test]
    fn unregistered_object_is_rejected() {
        let registry = ComponentRegistry::new();
        let object = Camera::default().to_object();
        let mut world = World::new();
        let entity = world.spawn();
        assert!(!registry.apply_object(&mut world, entity, &object));
        assert!(world.get::<Camera>(entity).is_none());
    }

    #[test]
    fn builtin_components_expose_defaults_and_names() {
        let mut registry = ComponentRegistry::new();
        crate::registry::register_builtin_components(&mut registry);

        // 每个内置组件都有「添加组件」默认值，且能反序列化回组件。
        let names = registry.component_names();

        // schema_fingerprint is deterministic and non-zero
        let fp1 = registry.schema_fingerprint();
        assert_ne!(fp1, 0);
        let fp2 = builtin_schema_fingerprint();
        assert_eq!(fp1, fp2);
        assert!(names.contains(&"prism_engine::scene::Camera"));
        for name in &names {
            let default = registry
                .default_json(name)
                .unwrap_or_else(|| panic!("missing default for {name}"));
            let mut world = World::new();
            let entity = world.spawn();
            registry.apply(&mut world, entity, name, &default);

            // 走反射路径导出，形状应与默认一致（Name 走自定义转换器）。
            if let Some(object) = registry.extract_object_by_name(&world, entity, name) {
                assert_eq!(
                    registry.object_from_json(name, &default),
                    Some(object),
                    "{name} default JSON does not round-trip"
                );
            }
        }

        // Name 的默认形状是裸字符串（自定义 JSON 形状）。
        assert_eq!(
            registry.default_json("prism_engine::scene::Name"),
            Some(serde_json::json!("entity"))
        );

        // 嵌套在作者组件里的类型也必须登记，否则 RSCN 编码会以
        // `type ... is not registered` 失败（#709 的 `Style.layout`）。
        for nested in [
            "prism_engine::ui::Anchors",
            "prism_engine::ui::Pivot",
            "prism_engine::ui::Margin",
            "prism_engine::ui::Layout",
        ] {
            assert!(
                registry
                    .type_registry()
                    .descriptor(TypeId::from_name(nested))
                    .is_some(),
                "nested type {nested} is not registered"
            );
        }

        // 每个组件都能「作者 JSON -> 反射对象 -> 作者 JSON」往返。
        for name in &names {
            let default = registry.default_json(name).unwrap();
            let object = registry
                .object_from_json(name, &default)
                .unwrap_or_else(|| panic!("{name} default JSON is not convertable"));
            let back = registry
                .object_to_json(name, &object)
                .unwrap_or_else(|| panic!("{name} has no object_to_json"));
            assert_eq!(back, default, "{name} JSON/reflect round-trip mismatch");
        }
    }
}
