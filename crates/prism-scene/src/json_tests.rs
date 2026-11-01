//! `.scene.json` 作者层的结构不变量。
//!
//! 重点是「变换只有一个权威来源」：作者写 `transform`，loader / cooker 负责
//! bake 成 `LocalTransform` + `LocalToWorld`；`components` 里再写一份会被
//! 迁移折回 `transform`（读）或被校验拒绝（写 / cook）。

use prism_reflect::Reflect;

use crate::{register_builtin_components, ComponentRegistry, LocalTransform, SceneJson};

const DERIVED_COMPONENT: &str = "prism_engine::scene::LocalTransform";

fn scene_with(components: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": [{
            "name": "e",
            "parent": null,
            "transform": {
                "position": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0],
            },
            "components": components,
        }],
    })
}

#[test]
fn legacy_local_transform_component_is_normalized_on_read() {
    // 历史文档把变换写成组件。读入时应 bake 到 `transform` 而不是保留组件，
    // 否则 loader 会在 `LocalToWorld` 算完之后再覆盖 `LocalTransform`。
    let scene = SceneJson::from_value(serde_json::json!({
        "entities": [{
            "components": {
                DERIVED_COMPONENT: { "position": [5.0, 0.0, 0.0] },
            }
        }],
    }))
    .expect("legacy document should still load");

    assert!(scene.entities[0].components.is_empty());
    assert_eq!(scene.entities[0].transform.position, [5.0, 0.0, 0.0]);
}

#[test]
fn hand_built_derived_component_is_rejected_by_validate() {
    let mut scene = SceneJson::from_value(scene_with(serde_json::json!({}))).unwrap();
    scene.entities[0]
        .components
        .insert(DERIVED_COMPONENT.to_string(), serde_json::json!({}));

    let error = scene
        .validate()
        .expect_err("validate must reject derived component");
    assert!(error.contains("LocalTransform"), "{error}");

    // cook 走同一条 `validate`，因此也会在编码前失败。
    let mut registry = ComponentRegistry::new();
    register_builtin_components(&mut registry);
    assert!(crate::encode_scene(&scene, &registry).is_err());
}

#[test]
fn legacy_computed_layout_is_stripped_on_read() {
    let scene = SceneJson::from_value(serde_json::json!({
        "entities": [{
            "components": {
                "prism_engine::ui::Node": {},
                "prism_engine::ui::Style": {},
                "prism_engine::ui::ComputedLayout": { "rect": [0.0, 0.0, 10.0, 10.0] },
            }
        }],
    }))
    .expect("legacy UI document should still load");

    let components = &scene.entities[0].components;
    assert!(!components.contains_key("prism_engine::ui::ComputedLayout"));
    assert!(components.contains_key("prism_engine::ui::Node"));
    assert!(components.contains_key("prism_engine::ui::Style"));
}

#[test]
fn hand_built_computed_layout_is_rejected_by_validate() {
    let mut scene = SceneJson::from_value(scene_with(serde_json::json!({}))).unwrap();
    scene.entities[0].components.insert(
        "prism_engine::ui::ComputedLayout".to_string(),
        serde_json::json!({}),
    );

    assert!(scene.validate().is_err());
}

#[test]
fn derived_components_are_types_not_mountable_components() {
    let mut registry = ComponentRegistry::new();
    register_builtin_components(&mut registry);

    for name in super::DERIVED_COMPONENTS {
        assert!(
            !registry.component_names().contains(name),
            "{name} must not be offered as an authorable component"
        );
        assert!(
            registry.default_json(name).is_none(),
            "{name} must not have an authoring default"
        );
    }

    // 仍然要留在类型表里：RTTIBin 类型表 / 编辑器字段描述依赖它。
    let id = <LocalTransform as Reflect>::type_desc().id;
    assert!(
        registry.type_registry().descriptor(id).is_some(),
        "LocalTransform must stay registered as a reflect type"
    );
}

/// 链式场景：`entities[i].parent == i - 1`。
fn chain_scene(n: usize) -> SceneJson {
    let entities: Vec<serde_json::Value> = (0..n)
        .map(|index| {
            serde_json::json!({
                "name": format!("e{index}"),
                "parent": if index == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(index - 1)
                },
                "components": {},
            })
        })
        .collect();
    SceneJson::from_value(serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": entities,
    }))
    .expect("chain scene should parse")
}

#[test]
fn topological_order_is_parent_first_for_deep_chains() {
    // 旧实现每个节点都扫全表（O(n²)），2 万节点会做 4 亿次比较。
    let n = 20_000;
    let order = chain_scene(n).topological_order();
    assert_eq!(order, (0..n).collect::<Vec<_>>());
}

#[test]
fn topological_order_keeps_parents_before_children_for_trees() {
    let scene = SceneJson::from_value(serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": [
            { "parent": null }, { "parent": 0 }, { "parent": 0 },
            { "parent": 1 }, { "parent": 1 }, { "parent": 2 },
        ],
    }))
    .unwrap();

    let order = scene.topological_order();
    assert_eq!(order.len(), 6);
    let mut position = [0usize; 6];
    for (index, entity_index) in order.iter().enumerate() {
        position[*entity_index] = index;
    }
    for (index, entity) in scene.entities.iter().enumerate() {
        if let Some(parent) = entity.parent {
            assert!(
                position[parent as usize] < position[index],
                "parent of {index} must be encoded first"
            );
        }
    }
}

#[test]
fn topological_order_lists_cycles_last_without_hanging() {
    let scene = SceneJson::from_value(serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": [{ "parent": 1 }, { "parent": 0 }],
    }))
    .unwrap();

    let mut order = scene.topological_order();
    order.sort_unstable();
    assert_eq!(order, vec![0, 1]);
}

#[test]
fn deep_chain_cooks_and_decodes_with_parent_before_child() {
    let scene = chain_scene(5_000);
    let registry = ComponentRegistry::new();

    let bytes = crate::encode_scene(&scene, &registry).expect("chain scenes must cook");
    let decoded = crate::decode_scene(&bytes, &registry).expect("cooked scene must decode");

    assert_eq!(decoded.entities.len(), 5_000);
    for (index, entity) in decoded.entities.iter().enumerate() {
        if let Some(parent) = entity.parent {
            assert!(
                (parent as usize) < index,
                "parent index must precede the child in the payload"
            );
        } else {
            assert_eq!(index, 0, "only the chain root may be a root");
        }
    }
}

#[test]
fn entity_ids_must_be_unique() {
    let error = SceneJson::from_value(serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": [
            { "id": 1, "parent": null },
            { "id": 1, "parent": null },
        ],
    }))
    .unwrap()
    .validate()
    .expect_err("duplicate id must be rejected");
    assert!(error.contains("duplicate id"), "{error}");
}

#[test]
fn parent_must_reference_an_existing_id() {
    let error = SceneJson::from_value(serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": [
            { "id": 0, "parent": null },
            { "id": 7, "parent": 42 },
        ],
    }))
    .unwrap()
    .validate()
    .expect_err("dangling parent id must be rejected");
    assert!(error.contains("does not exist"), "{error}");
}

#[test]
fn self_parent_is_checked_by_id_not_position() {
    let error = SceneJson::from_value(serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": [
            { "id": 0, "parent": null },
            { "id": 5, "parent": 5 },
        ],
    }))
    .unwrap()
    .validate()
    .expect_err("self parent must be rejected");
    assert!(error.contains("self-parent"), "{error}");
}

#[test]
fn v2_documents_get_identity_ids_on_migration() {
    // v2：`parent` 是数组下标，没有 id。
    let scene = SceneJson::from_value(serde_json::json!({
        "version": 2,
        "entities": [
            { "name": "root", "parent": null },
            { "name": "mid", "parent": 0 },
            { "name": "leaf", "parent": 1 },
        ],
    }))
    .unwrap();

    assert_eq!(scene.version, crate::SCENE_DOC_VERSION);
    assert_eq!(scene.entities[0].id, 0);
    assert_eq!(scene.entities[1].id, 1);
    assert_eq!(scene.entities[2].id, 2);
    // 恒等映射：旧的父下标数值直接变成 id，层级不变。
    assert_eq!(scene.entities[1].parent, Some(0));
    assert_eq!(scene.entities[2].parent, Some(1));
    scene.validate().unwrap();
    assert_eq!(scene.topological_order(), vec![0, 1, 2]);
}

#[test]
fn next_id_is_monotonic_and_never_reuses() {
    let scene = SceneJson::from_value(serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": [
            { "id": 3, "parent": null },
            { "id": 9, "parent": 3 },
        ],
    }))
    .unwrap();
    // 删掉 id 9 也不会让下一个 id 回退到 9。
    assert_eq!(scene.next_id(), 10);
}

#[test]
fn topological_order_resolves_non_contiguous_ids() {
    let scene = SceneJson::from_value(serde_json::json!({
        "version": crate::SCENE_DOC_VERSION,
        "entities": [
            { "id": 40, "parent": null, "name": "root" },
            { "id": 12, "parent": 40, "name": "child" },
            { "id": 3, "parent": 12, "name": "grandchild" },
        ],
    }))
    .unwrap();

    let order = scene.topological_order();
    // 索引而非 id：40 → 0，12 → 1，3 → 2。
    assert_eq!(order, vec![0, 1, 2]);
}

#[test]
fn keyframe_animation_tracks_cook_and_decode() {
    // 轨道是「结构体列表」，最容易在 RSCN 的位置化编码里掉数据（#490）。
    let mut registry = ComponentRegistry::new();
    register_builtin_components(&mut registry);
    let scene = SceneJson::parse(
        r#"{
            "version": 3,
            "entities": [{
                "id": 0,
                "name": "Bob",
                "components": {
                    "prism_engine::scene::KeyframeAnimation": {
                        "name": "walk",
                        "duration": 2.0,
                        "speed": 1.0,
                        "looped": true,
                        "playing": true,
                        "phase": 0.0,
                        "position": [
                            {"time": 0.0, "value": [0,0,0]},
                            {"time": 2.0, "value": [0,0,4]}
                        ],
                        "euler_xyz": [{"time": 0.0, "value": [0,0,0]}],
                        "scale": []
                    }
                }
            }]
        }"#,
    )
    .unwrap();

    let bytes = crate::encode_scene(&scene, &registry).expect("scene with a clip must cook");
    let decoded = crate::decode_scene(&bytes, &registry).expect("cooked scene must decode");
    let clip = decoded.entities[0]
        .components
        .iter()
        .find(|component| component.type_name == "prism_engine::scene::KeyframeAnimation")
        .expect("clip component must survive the cook");

    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    assert!(registry.apply_object(&mut world, entity, clip));
    let clip = world
        .get::<crate::components::KeyframeAnimation>(entity)
        .expect("clip must apply to the world");
    assert_eq!(clip.name, "walk");
    assert_eq!(clip.clip_duration(), 2.0);
    assert_eq!(clip.position.len(), 2);
    assert_eq!(clip.position[1].time, 2.0);
    assert_eq!(clip.position[1].value, [0.0, 0.0, 4.0]);
    assert_eq!(clip.euler_xyz.len(), 1);
    assert!(clip.scale.is_empty());
}

#[test]
fn a_particle_system_survives_cook_and_decode() {
    // 发射器带一个枚举字段（`shape`）：RSCN 把枚举写成小整数，解码时再还原，
    // 最容易在位置化编码里掉值（#501）。
    let mut registry = ComponentRegistry::new();
    register_builtin_components(&mut registry);
    let scene = SceneJson::parse(
        r#"{
            "version": 3,
            "entities": [{
                "id": 0,
                "name": "Fire",
                "components": {
                    "prism_engine::scene::ParticleSystem": {
                        "playing": true,
                        "emission_rate": 30.0,
                        "burst": 4,
                        "duration": 3.0,
                        "looping": false,
                        "max_particles": 64,
                        "lifetime": [0.4, 0.9],
                        "start_speed": [1.0, 2.0],
                        "start_size": [0.1, 0.2],
                        "start_color": [1.0, 0.6, 0.2, 1.0],
                        "end_color": [0.6, 0.1, 0.0, 0.0],
                        "gravity": [0.0, 1.0, 0.0],
                        "drag": 0.3,
                        "shape": "Cone",
                        "shape_radius": 0.2,
                        "cone_angle_degrees": 25.0
                    }
                }
            }]
        }"#,
    )
    .unwrap();

    let bytes = crate::encode_scene(&scene, &registry).expect("scene with an emitter must cook");
    let decoded = crate::decode_scene(&bytes, &registry).expect("cooked scene must decode");
    let component = decoded.entities[0]
        .components
        .iter()
        .find(|component| component.type_name == "prism_engine::scene::ParticleSystem")
        .expect("emitter component must survive the cook");

    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    assert!(registry.apply_object(&mut world, entity, component));
    let system = world
        .get::<crate::components::ParticleSystem>(entity)
        .expect("emitter must apply to the world");
    assert_eq!(system.emission_rate, 30.0);
    assert_eq!(system.burst, 4);
    assert!(!system.looping);
    assert_eq!(system.lifetime, [0.4, 0.9]);
    assert_eq!(system.shape, crate::components::ParticleShape::Cone);
    assert_eq!(system.start_color, [1.0, 0.6, 0.2, 1.0]);
    assert_eq!(system.particle_cap(), 64);
}

#[test]
fn an_audio_source_survives_cook_and_decode() {
    // 音频组件只有一个字符串路径 + 若干标量；RSCN 位置化编码最容易丢的是
    // 字符串（`sound_path`）与布尔（#508）。
    let mut registry = ComponentRegistry::new();
    register_builtin_components(&mut registry);
    let scene = SceneJson::parse(
        r#"{
            "version": 3,
            "entities": [{
                "id": 0,
                "name": "Speaker",
                "components": {
                    "prism_engine::scene::AudioSource": {
                        "sound_path": "audio/ambient.ogg",
                        "volume": 0.35,
                        "looped": true,
                        "play_on_awake": false,
                        "bus": "Music",
                        "spatial_blend": 1.0,
                        "min_distance": 3.0,
                        "max_distance": 20.0
                    },
                    "prism_engine::scene::AudioListener": {
                        "gain": 0.8
                    }
                }
            }]
        }"#,
    )
    .unwrap();

    let bytes = crate::encode_scene(&scene, &registry).expect("scene with audio must cook");
    let decoded = crate::decode_scene(&bytes, &registry).expect("cooked scene must decode");

    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    for component in &decoded.entities[0].components {
        assert!(registry.apply_object(&mut world, entity, component));
    }
    let source = world
        .get::<crate::components::AudioSource>(entity)
        .expect("audio source must apply to the world");
    assert_eq!(source.sound_path, "audio/ambient.ogg");
    assert_eq!(source.volume, 0.35);
    assert!(source.looped);
    assert!(!source.play_on_awake);
    assert_eq!(source.bus, crate::components::AudioBus::Music);
    assert_eq!(source.spatial_blend, 1.0);
    assert_eq!(source.max_distance, 20.0);
    let listener = world
        .get::<crate::components::AudioListener>(entity)
        .expect("audio listener must apply to the world");
    assert_eq!(listener.gain, 0.8);
}

#[test]
fn physics_components_survive_cook_and_decode() {
    // 物理组件的两个枚举（ColliderShape / BodyType）走的是「JSON 用变体名、RSCN 用小整数」
    // 的手写 ReflectType，最容易在往返里错位（#522）。
    let mut registry = ComponentRegistry::new();
    register_builtin_components(&mut registry);
    let scene = SceneJson::parse(
        r#"{
            "version": 3,
            "entities": [{
                "id": 0,
                "name": "Crate",
                "components": {
                    "prism_engine::scene::Collider": {
                        "shape": "Sphere",
                        "center": [0.0, 1.0, 0.0],
                        "radius": 0.75,
                        "is_trigger": true,
                        "layer": 4
                    },
                    "prism_engine::scene::Rigidbody": {
                        "body_type": "Kinematic",
                        "mass": 12.5,
                        "use_gravity": false
                    }
                }
            }]
        }"#,
    )
    .unwrap();

    let bytes = crate::encode_scene(&scene, &registry).expect("scene with physics must cook");
    let decoded = crate::decode_scene(&bytes, &registry).expect("cooked scene must decode");

    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    for component in &decoded.entities[0].components {
        assert!(registry.apply_object(&mut world, entity, component));
    }

    let collider = world
        .get::<crate::components::Collider>(entity)
        .expect("collider must apply");
    assert_eq!(collider.shape, crate::components::ColliderShape::Sphere);
    assert_eq!(collider.radius, 0.75);
    assert_eq!(collider.center, [0.0, 1.0, 0.0]);
    assert!(collider.is_trigger);
    assert_eq!(collider.layer, 4);

    let body = world
        .get::<crate::components::Rigidbody>(entity)
        .expect("rigidbody must apply");
    assert_eq!(body.body_type, crate::components::BodyType::Kinematic);
    assert_eq!(body.mass, 12.5);
    assert!(!body.use_gravity);
}

#[test]
fn a_terrain_survives_cook_and_decode() {
    // 地形组件是「路径 + 若干标量」，RSCN 的位置化编码最容易丢字符串（heightmap）
    // 和 `[f32; 2]`（size）（#517）。
    let mut registry = ComponentRegistry::new();
    register_builtin_components(&mut registry);
    let scene = SceneJson::parse(
        r#"{
            "version": 3,
            "entities": [{
                "id": 0,
                "name": "Terrain",
                "components": {
                    "prism_engine::scene::Terrain": {
                        "heightmap": "heightmaps/terrain.raw",
                        "size": [512.0, 256.0],
                        "height_scale": 1.5,
                        "resolution": 0
                    }
                }
            }]
        }"#,
    )
    .unwrap();

    let bytes = crate::encode_scene(&scene, &registry).expect("scene with terrain must cook");
    let decoded = crate::decode_scene(&bytes, &registry).expect("cooked scene must decode");

    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    for component in &decoded.entities[0].components {
        assert!(registry.apply_object(&mut world, entity, component));
    }
    let terrain = world
        .get::<crate::components::Terrain>(entity)
        .expect("terrain must apply to the world");
    assert_eq!(terrain.heightmap, "heightmaps/terrain.raw");
    assert_eq!(terrain.size, [512.0, 256.0]);
    assert_eq!(terrain.height_scale, 1.5);
    assert_eq!(terrain.resolution, 0, "0 = 跟随高度图大小");
}
