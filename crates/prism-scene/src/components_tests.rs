//! `Camera` / `Light` 的 schema 与数学约定测试（`components.rs` 的测试模块）。

use super::*;

fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

fn assert_mat_eq(a: glam::Mat4, b: glam::Mat4) {
    let (a, b) = (a.to_cols_array(), b.to_cols_array());
    for i in 0..16 {
        assert!(approx(a[i], b[i]), "element {i}: {} != {}", a[i], b[i]);
    }
}

#[test]
fn perspective_matches_vulkan_convention() {
    let cam = Camera {
        fov_y_degrees: 90.0,
        near: 0.1,
        far: 100.0,
        ..Camera::default()
    };
    let aspect = 2.0;
    let p = cam.projection_matrix(aspect);
    let inv_tan = 1.0 / (45.0_f32.to_radians()).tan();

    assert!(approx(p.col(0).x, inv_tan / aspect));
    // Vulkan y-flip：p[1][1] 为负。
    assert!(approx(p.col(1).y, -inv_tan));
    assert!(approx(p.col(2).z, cam.far / (cam.near - cam.far)));
    assert!(approx(p.col(2).w, -1.0));
    assert!(approx(
        p.col(3).z,
        cam.near * cam.far / (cam.near - cam.far)
    ));
}

#[test]
fn perspective_maps_near_to_zero_and_far_to_one() {
    let cam = Camera {
        near: 0.5,
        far: 50.0,
        ..Camera::default()
    };
    let p = cam.projection_matrix(1.0);
    for (dist, expected) in [(cam.near, 0.0), (cam.far, 1.0)] {
        let clip = p * glam::vec4(0.0, 0.0, -dist, 1.0);
        let ndc_z = clip.z / clip.w;
        assert!((ndc_z - expected).abs() < 1e-4, "z={dist} → {ndc_z}");
    }
}

#[test]
fn orthographic_maps_half_sizes_and_depth() {
    let cam = Camera {
        orthographic: true,
        orthographic_size: 2.0,
        near: 0.5,
        far: 9.5,
        ..Camera::default()
    };
    let p = cam.projection_matrix(2.0);

    // 半高 2 → NDC 1/2；y-flip 后上边缘（y=+2）落到 NDC y = -1。
    assert!(approx(p.col(1).y, -0.5));
    // 半宽 = 半高 × aspect = 4。
    assert!(approx(p.col(0).x, 0.25));

    let top = p * glam::vec4(0.0, 2.0, -1.0, 1.0);
    assert!(approx(top.y / top.w, -1.0));
    let right = p * glam::vec4(4.0, 0.0, -1.0, 1.0);
    assert!(approx(right.x / right.w, 1.0));

    for (dist, expected) in [(cam.near, 0.0), (cam.far, 1.0)] {
        let clip = p * glam::vec4(0.0, 0.0, -dist, 1.0);
        let ndc_z = clip.z / clip.w;
        assert!((ndc_z - expected).abs() < 1e-4, "z={dist} → {ndc_z}");
    }
}

#[test]
fn degenerate_aspect_falls_back_to_one() {
    let cam = Camera::default();
    let sane = cam.projection_matrix(1.0);
    assert_mat_eq(cam.projection_matrix(0.0), sane);
    assert_mat_eq(cam.projection_matrix(f32::NAN), sane);
}

#[test]
fn world_to_camera_inverts_camera_to_world() {
    let camera_to_world = glam::Mat4::from_translation(glam::Vec3::new(1.0, 2.0, 3.0));
    let world_to_camera = Camera::world_to_camera_matrix(camera_to_world);
    assert_mat_eq(camera_to_world * world_to_camera, glam::Mat4::IDENTITY);
    assert_mat_eq(
        Camera::camera_to_world_matrix(world_to_camera),
        camera_to_world,
    );
}

#[test]
fn camera_defaults() {
    let cam = Camera::default();
    assert_eq!(cam.clear_flags, CameraClearFlags::Skybox);
    assert!(!cam.orthographic);
    assert!(approx(cam.orthographic_size, 5.0));
    assert!(approx(cam.depth, 0.0));
    assert!(approx(cam.background_color[3], 1.0));
}

#[test]
fn json_round_trips_new_fields() {
    let cam = Camera {
        clear_flags: CameraClearFlags::SolidColor,
        background_color: [0.1, 0.2, 0.3, 1.0],
        orthographic: true,
        orthographic_size: 7.5,
        depth: 2.0,
        ..Camera::default()
    };
    let json = serde_json::to_string(&cam).unwrap();
    let back: Camera = serde_json::from_str(&json).unwrap();
    assert_eq!(back.clear_flags, CameraClearFlags::SolidColor);
    assert!(back.orthographic);
    assert!(approx(back.orthographic_size, 7.5));
    assert!(approx(back.depth, 2.0));
    assert!(approx(back.background_color[1], 0.2));
}

#[test]
fn older_json_without_new_fields_uses_defaults() {
    // 只有老字段的文档（v2 之前的快门相机）必须仍然可用 —— 新增字段走 serde 默认。
    let json = r#"{"fov_y_degrees":72.0,"near":0.2,"far":500.0,"exposure":1.5,"aspect":1.0,"enabled":true}"#;
    let cam: Camera = serde_json::from_str(json).unwrap();
    assert!(approx(cam.fov_y_degrees, 72.0));
    assert_eq!(cam.clear_flags, CameraClearFlags::Skybox);
    assert!(!cam.orthographic);
}

// ---------------------------------------------------------------------------
// Light
// ---------------------------------------------------------------------------

#[test]
fn light_defaults() {
    let light = Light::default();
    assert!(light.enabled);
    // `new Light()` 默认是点光源；聚光角度与色温沿用引擎默认值。
    assert_eq!(light.light_type, LightType::Point);
    assert_eq!(light.shadows, LightShadows::None);
    assert_eq!(light.render_mode, LightRenderMode::Auto);
    assert!(approx(light.spot_angle, 30.0));
    assert!(approx(light.inner_spot_angle, 21.8));
    assert!(approx(light.color_temperature, 6570.0));
    assert!(!light.use_color_temperature);
    assert!(approx(light.shadow_strength, 1.0));
    assert!(!light.is_shadow_caster());
}

#[test]
fn light_json_round_trips_and_fills_missing_fields() {
    let light = Light {
        light_type: LightType::Spot,
        shadows: LightShadows::Soft,
        use_color_temperature: true,
        color_temperature: 3200.0,
        ..Light::default()
    };
    let json = serde_json::to_string(&light).unwrap();
    let back: Light = serde_json::from_str(&json).unwrap();
    assert_eq!(back.light_type, LightType::Spot);
    assert_eq!(back.shadows, LightShadows::Soft);
    assert!(back.use_color_temperature);

    // 只写已知字段的文档也必须能反序列化（`#[serde(default)]`）。
    let partial: Light =
        serde_json::from_str(r#"{"light_type":"Directional","intensity":3.0}"#).unwrap();
    assert_eq!(partial.light_type, LightType::Directional);
    assert!(approx(partial.intensity, 3.0));
    assert_eq!(partial.shadows, LightShadows::None);
    assert!(approx(partial.range, 10.0));
}

#[test]
fn light_effective_color_applies_temperature_only_when_enabled() {
    let mut light = Light {
        color: glam::Vec3::new(1.0, 0.5, 0.25),
        color_temperature: 2000.0,
        ..Light::default()
    };
    // 关闭色温：原样返回。
    assert_eq!(light.effective_color(), light.color);

    light.use_color_temperature = true;
    let tinted = light.effective_color();
    // 2000 K 是暖色：红色分量保持，蓝色被明显压低。
    assert!(approx(tinted.x, light.color.x));
    assert!(tinted.z < light.color.z, "色温滤色片应压低蓝色: {tinted:?}");
}

#[test]
fn light_cone_half_angles_are_derived_from_full_angles() {
    let light = Light {
        light_type: LightType::Spot,
        // 引擎的 60° 全锥 = 30° 半锥。
        spot_angle: 60.0,
        inner_spot_angle: 20.0,
        ..Light::default()
    };
    let (inner, outer) = light.cone_half_angles_radians();
    assert!(approx(outer, 30.0_f32.to_radians()));
    assert!(approx(inner, 10.0_f32.to_radians()));

    // 内锥大于外锥时被裁到外锥，不会出现负的余弦区间。
    let broken = Light {
        spot_angle: 20.0,
        inner_spot_angle: 90.0,
        ..Light::default()
    };
    let (inner, outer) = broken.cone_half_angles_radians();
    assert!(inner <= outer);
}

#[test]
fn light_shadow_caster_requires_enabled_and_shadow_mode() {
    let light = Light {
        shadows: LightShadows::Hard,
        ..Light::default()
    };
    assert!(light.is_shadow_caster());

    let disabled = Light {
        enabled: false,
        ..light
    };
    assert!(!disabled.is_shadow_caster());
}

// ---------------------------------------------------------------------------
// SpriteRenderer（2D 精灵）
// ---------------------------------------------------------------------------

#[test]
fn sprite_defaults() {
    let sprite = SpriteRenderer::default();
    assert!(!sprite.has_sprite());
    assert_eq!(sprite.frame_size, [0, 0]);
    assert_eq!(sprite.frame_index, 0);
    assert!(approx(sprite.pixels_per_unit, 100.0));
    assert_eq!(sprite.pivot, [0.5, 0.5]);
    assert_eq!(sprite.color, [1.0, 1.0, 1.0, 1.0]);
    assert!(!sprite.flip_x && !sprite.flip_y);
    assert_eq!(sprite.sorting_layer, 0);
    assert_eq!(sprite.order_in_layer, 0);
}

#[test]
fn sprite_json_round_trips_and_fills_missing_fields() {
    let sprite = SpriteRenderer {
        sprite_path: "sprites/boat.png".into(),
        frame_size: [32, 16],
        sheet_columns: 4,
        sheet_row: 1,
        frame_index: 3,
        pixels_per_unit: 16.0,
        pivot: [0.0, 1.0],
        color: [0.5, 1.0, 0.25, 0.75],
        flip_x: true,
        flip_y: false,
        sorting_layer: 2,
        order_in_layer: -1,
        src_blend: BlendMode::SrcAlpha,
        dst_blend: BlendMode::One,
        blend_op: BlendOp::Add,
    };
    let json = serde_json::to_string(&sprite).unwrap();
    let back: SpriteRenderer = serde_json::from_str(&json).unwrap();
    assert_eq!(back.sprite_path, "sprites/boat.png");
    assert_eq!(back.frame_size, [32, 16]);
    assert_eq!(back.sheet_columns, 4);
    assert_eq!(back.sheet_row, 1);
    assert_eq!(back.frame_index, 3);
    assert!(approx(back.pixels_per_unit, 16.0));
    assert_eq!(back.pivot, [0.0, 1.0]);
    assert!(back.flip_x);
    assert_eq!(back.sorting_layer, 2);
    assert_eq!(back.order_in_layer, -1);
    assert_eq!(back.src_blend, BlendMode::SrcAlpha);
    assert_eq!(back.dst_blend, BlendMode::One);
    assert_eq!(back.blend_op, BlendOp::Add);

    // 老文档只有路径：其余字段走 serde 默认。
    let legacy: SpriteRenderer = serde_json::from_str(r#"{"sprite_path":"a.png"}"#).unwrap();
    assert_eq!(legacy.sprite_path, "a.png");
    assert!(approx(legacy.pixels_per_unit, 100.0));
    assert_eq!(legacy.frame_size, [0, 0]);
    assert_eq!(legacy.sheet_columns, 0, "老文档按单行图集处理");
    assert_eq!(legacy.sheet_row, 0);
    assert_eq!(
        legacy.src_blend,
        BlendMode::SrcAlpha,
        "老文档默认 alpha 混合"
    );
}

#[test]
fn sprite_world_size_uses_frame_or_texture_and_ppu() {
    // 整张纹理 64x32，PPU 16 -> 4x2 世界单位。
    let whole = SpriteRenderer {
        pixels_per_unit: 16.0,
        ..SpriteRenderer::default()
    };
    assert_eq!(whole.world_size([64, 32]), [4.0, 2.0]);

    // 精灵表单帧优先于纹理尺寸。
    let sheet = SpriteRenderer {
        frame_size: [32, 32],
        pixels_per_unit: 32.0,
        ..SpriteRenderer::default()
    };
    assert_eq!(sheet.frame_size_or([256, 32]), [32, 32]);
    assert_eq!(sheet.world_size([256, 32]), [1.0, 1.0]);

    // 非法 PPU 回落到 100，不产生 NaN/Inf。
    let broken = SpriteRenderer {
        pixels_per_unit: 0.0,
        ..SpriteRenderer::default()
    };
    assert_eq!(broken.world_size([100, 100]), [1.0, 1.0]);
}

#[test]
fn mesh_renderer_state_defaults_and_round_trips() {
    // 老文档只有 mesh/material：所有新字段必须回退到引擎的不透明默认值。
    let legacy: MeshRenderer =
        serde_json::from_str(r#"{"mesh_path":"m.mesh","material_path":"m.mat"}"#)
            .expect("缺少渲染状态字段的老 JSON 仍然可读");
    assert_eq!(legacy.cull_mode, CullMode::Back);
    assert_eq!(legacy.shadow_casting_mode, ShadowCastingMode::On);
    assert_eq!(legacy.render_queue.value(), RenderQueue::GEOMETRY);
    assert!(legacy.z_write);
    assert_eq!(legacy.src_blend, BlendMode::One);
    assert_eq!(legacy.dst_blend, BlendMode::Zero);
    assert_eq!(legacy.blend_op, BlendOp::Add);
    // One / Zero / Add = 不透明。
    assert!(!legacy.blends());

    // 作者格式用变体名（与 `CameraClearFlags` / `TextAlign` 一致），
    // 队列是数字（`Material.renderQueue` 的 int 形状）。
    let json = serde_json::to_value(MeshRenderer {
        mesh_path: "m.mesh".into(),
        material_path: "m.mat".into(),
        shadow_casting_mode: ShadowCastingMode::Off,
        cull_mode: CullMode::Off,
        render_queue: RenderQueue::new(RenderQueue::TRANSPARENT),
        z_write: false,
        src_blend: BlendMode::SrcAlpha,
        dst_blend: BlendMode::OneMinusSrcAlpha,
        blend_op: BlendOp::Add,
    })
    .unwrap();
    assert_eq!(json["cull_mode"], serde_json::json!("Off"));
    assert_eq!(json["render_queue"], serde_json::json!(3000));
    assert_eq!(json["src_blend"], serde_json::json!("SrcAlpha"));
    assert_eq!(json["z_write"], serde_json::json!(false));

    let back: MeshRenderer = serde_json::from_value(json).unwrap();
    assert_eq!(back.cull_mode, CullMode::Off);
    assert_eq!(back.shadow_casting_mode, ShadowCastingMode::Off);
    assert_eq!(back.render_queue.value(), RenderQueue::TRANSPARENT);
    assert!(!back.z_write);
    assert_eq!(back.dst_blend, BlendMode::OneMinusSrcAlpha);
    // 标准 alpha 混合 = 透明。
    assert!(back.blends());
}

#[test]
fn particle_system_round_trips_with_variant_names() {
    let system = ParticleSystem {
        emission_rate: 12.0,
        burst: 3,
        duration: 2.0,
        lifetime: [0.5, 1.5],
        shape: ParticleShape::Sphere,
        shape_radius: 0.75,
        ..ParticleSystem::default()
    };

    // 作者格式用变体名（与 `CameraClearFlags` / `TextAlign` 一致）。
    let json = serde_json::to_value(&system).unwrap();
    assert_eq!(json["shape"], serde_json::json!("Sphere"));
    assert_eq!(json["emission_rate"], serde_json::json!(12.0));

    let back: ParticleSystem = serde_json::from_value(json).unwrap();
    assert_eq!(back.shape, ParticleShape::Sphere);
    assert_eq!(back.burst, 3);
    assert_eq!(back.lifetime, [0.5, 1.5]);
}

#[test]
fn a_particle_system_without_fields_falls_back_to_the_defaults() {
    // 缺少字段保持默认（跨版本兼容）。
    let system: ParticleSystem = serde_json::from_str("{}").unwrap();
    assert_eq!(system.shape, ParticleShape::Cone);
    assert!(system.playing);
    assert_eq!(system.particle_cap(), 256);
    assert_eq!(system.emission_duration(), None);
    assert_eq!(ParticleSystem::random_range([2.0, 1.0], 0.25, 0.0), 1.25);
    assert_eq!(
        ParticleSystem::random_range([f32::NAN, 1.0], 0.25, 7.0),
        7.0
    );
}

#[test]
fn audio_sources_decay_with_distance_and_blend_toward_2d() {
    let source = AudioSource {
        sound_path: "audio/footstep.ogg".into(),
        volume: 0.8,
        spatial_blend: 1.0,
        min_distance: 2.0,
        max_distance: 10.0,
        ..AudioSource::default()
    };
    assert!(source.has_sound());
    assert_eq!(source.volume_at(0.0), 0.8, "近距离内不衰减");
    assert_eq!(source.volume_at(2.0), 0.8, "min_distance 仍然满音量");
    assert!((source.volume_at(6.0) - 0.4).abs() < 1e-6, "中点线性衰减");
    assert_eq!(source.volume_at(10.0), 0.0, "max_distance 起完全静止");
    assert_eq!(source.volume_at(1_000.0), 0.0);
    assert_eq!(source.volume_at(f32::NAN), 0.0);

    // 纯 2D：距离不起作用。
    let flat = AudioSource {
        spatial_blend: 0.0,
        ..source.clone()
    };
    assert_eq!(flat.volume_at(0.0), 0.8);
    assert_eq!(flat.volume_at(9_999.0), 0.8);

    // 半混合：衰减只生效一半。
    let half = AudioSource {
        spatial_blend: 0.5,
        ..source.clone()
    };
    assert!((half.volume_at(6.0) - 0.6).abs() < 1e-6);

    // 退化区间 / 越界音量都不会算出 >1 或 NaN。
    let broken = AudioSource {
        volume: 4.0,
        min_distance: 5.0,
        max_distance: 1.0,
        ..source
    };
    assert_eq!(broken.volume_at(3.0), 1.0);
    assert_eq!(AudioSource::default().volume_at(0.0), 1.0);
    assert!(!AudioSource::default().has_sound());
}

#[test]
fn line_json_defaults_to_miter_join_and_butt_caps() {
    // #604：老文档只有 points / width，新字段必须是 serde 默认值。
    let line: LineRenderer =
        serde_json::from_str(r#"{"points":[[0,0,0],[1,0,0]],"width":2}"#).unwrap();
    assert_eq!(line.join, LineJoin::Miter);
    assert_eq!(line.cap, LineCap::Butt);
    assert!((line.miter_limit - 4.0).abs() < 1e-6);

    let text = serde_json::to_string(&LineRenderer {
        join: LineJoin::Bevel,
        cap: LineCap::Round,
        ..line
    })
    .unwrap();
    assert!(text.contains("\"Bevel\""), "{text}");
    assert!(text.contains("\"Round\""), "{text}");
}
