// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;
use prism_ecs::World;
use prism_render::managers::MeshHandle;
use prism_render::DrawState;
use prism_scene::render_enums::BlendOp;
use prism_scene::{BlendMode, CullMode, RenderQueue, ShadowCastingMode};

#[test]
fn entity_with_all_components_yields_draw_item() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(
        e,
        LocalToWorld(glam::Mat4::from_cols_array_2d(&[[1.0; 4]; 4])),
    );
    world.insert(
        e,
        MeshRef {
            asset_id: SceneAssetId::generate(),
            render_handle: MeshHandle::default(),
            generation: 1,
        },
    );
    world.insert(
        e,
        MaterialRef {
            asset_id: SceneAssetId::generate(),
            material_slot: 2,
            generation: 1,
        },
    );
    // No 激活 → defaults to true.

    let items = scene_render_system(&world);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].material, Some(2));
}

#[test]
fn inactive_entity_is_skipped() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(
        e,
        LocalToWorld(glam::Mat4::from_cols_array_2d(&[[1.0; 4]; 4])),
    );
    world.insert(
        e,
        MeshRef {
            asset_id: SceneAssetId::generate(),
            render_handle: MeshHandle::default(),
            generation: 1,
        },
    );
    world.insert(
        e,
        MaterialRef {
            asset_id: SceneAssetId::generate(),
            material_slot: 0,
            generation: 1,
        },
    );
    world.insert(e, Active(false));

    let items = scene_render_system(&world);
    assert!(items.is_empty());
}

#[test]
fn entity_without_mesh_is_skipped() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(
        e,
        LocalToWorld(glam::Mat4::from_cols_array_2d(&[[1.0; 4]; 4])),
    );
    // No MeshRef, no MaterialRef → no 绘制 item.

    let items = scene_render_system(&world);
    assert!(items.is_empty());
}

#[test]
fn mesh_without_material_still_draws_with_default_material() {
    let mut world = World::new();
    let e = world.spawn();
    world.insert(
        e,
        LocalToWorld(glam::Mat4::from_cols_array_2d(&[[1.0; 4]; 4])),
    );
    world.insert(
        e,
        MeshRef {
            asset_id: SceneAssetId::generate(),
            render_handle: MeshHandle::default(),
            generation: 1,
        },
    );
    // 没有 `MaterialRef`：`MeshRenderer { material_path: "" }` 的
    // 网格-only 实体（编辑器 fixture 就是这种形状）。

    let items = scene_render_system(&world);
    assert_eq!(items.len(), 1, "有网格、无材质的实体也应该绘制");
    assert_eq!(items[0].material, None, "缺失材质应回落到默认材质槽");
}

#[test]
fn multiple_entities() {
    let mut world = World::new();
    for _ in 0..3 {
        let e = world.spawn();
        world.insert(
            e,
            LocalToWorld(glam::Mat4::from_cols_array_2d(&[[1.0; 4]; 4])),
        );
        world.insert(
            e,
            MeshRef {
                asset_id: SceneAssetId::generate(),
                render_handle: MeshHandle::default(),
                generation: 1,
            },
        );
        world.insert(
            e,
            MaterialRef {
                asset_id: SceneAssetId::generate(),
                material_slot: 0,
                generation: 1,
            },
        );
    }

    let items = scene_render_system(&world);
    assert_eq!(items.len(), 3);
}

#[test]
fn empty_world_yields_empty_draw_list() {
    let world = World::new();
    let items = scene_render_system(&world);
    assert!(items.is_empty());
}

#[test]
fn model_matrix_carries_through() {
    let mut world = World::new();
    let e = world.spawn();
    let model = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 2.0, 0.0, 0.0],
        [0.0, 0.0, 3.0, 0.0],
        [4.0, 5.0, 6.0, 1.0],
    ];
    world.insert(e, LocalToWorld(glam::Mat4::from_cols_array_2d(&model)));
    world.insert(
        e,
        MeshRef {
            asset_id: SceneAssetId::generate(),
            render_handle: MeshHandle::default(),
            generation: 1,
        },
    );
    world.insert(
        e,
        MaterialRef {
            asset_id: SceneAssetId::generate(),
            material_slot: 0,
            generation: 1,
        },
    );

    let items = scene_render_system(&world);
    assert_eq!(items[0].model, model);
}

/// 建一个带渲染三件套的实体，可选 `MeshRenderer`（决定 `shadowCastingMode`）。
fn spawn_renderable(world: &mut World, renderer: Option<MeshRenderer>) -> prism_ecs::Entity {
    let e = world.spawn();
    world.insert(
        e,
        LocalToWorld(glam::Mat4::from_cols_array_2d(&[[1.0; 4]; 4])),
    );
    world.insert(
        e,
        MeshRef {
            asset_id: SceneAssetId::generate(),
            render_handle: MeshHandle::default(),
            generation: 1,
        },
    );
    world.insert(
        e,
        MaterialRef {
            asset_id: SceneAssetId::generate(),
            material_slot: 0,
            generation: 1,
        },
    );
    if let Some(renderer) = renderer {
        world.insert(e, renderer);
    }
    e
}

#[test]
fn missing_mesh_renderer_defaults_to_casting_and_visible() {
    let mut world = World::new();
    spawn_renderable(&mut world, None);

    let items = scene_render_system(&world);
    assert_eq!(items.len(), 1);
    assert!(items[0].cast_shadow, "默认 ShadowCastingMode 应为 On");
    assert!(items[0].visible_in_main_pass);
    assert!(!items[0].cast_shadow_two_sided, "默认 On 走背面剔除阴影");
}

#[test]
fn shadow_casting_mode_off_disables_only_the_shadow() {
    let mut world = World::new();
    spawn_renderable(
        &mut world,
        Some(MeshRenderer {
            shadow_casting_mode: ShadowCastingMode::Off,
            ..Default::default()
        }),
    );

    let items = scene_render_system(&world);
    assert!(!items[0].cast_shadow);
    assert!(items[0].visible_in_main_pass);
    assert!(!items[0].cast_shadow_two_sided);
}

#[test]
fn shadows_only_hides_from_the_main_pass() {
    let mut world = World::new();
    spawn_renderable(
        &mut world,
        Some(MeshRenderer {
            shadow_casting_mode: ShadowCastingMode::ShadowsOnly,
            ..Default::default()
        }),
    );

    let items = scene_render_system(&world);
    assert!(items[0].cast_shadow);
    assert!(!items[0].visible_in_main_pass);
    assert!(!items[0].cast_shadow_two_sided);
}

#[test]
fn render_state_defaults_to_opaque_and_follows_the_component() {
    let mut world = World::new();
    // 没有 `MeshRenderer`：默认不透明、队列 2000、背面剔除、写深度。
    spawn_renderable(&mut world, None);
    let items = scene_render_system(&world);
    assert_eq!(items[0].state, DrawState::default());
    assert_eq!(items[0].state.cull_mode, CullMode::Back);
    assert!(!items[0].state.blends());

    // 透明材质状态必须原样传到绘制项（`CullMode.Off` / 队列 / 混合）。
    let mut world = World::new();
    spawn_renderable(
        &mut world,
        Some(MeshRenderer {
            cull_mode: CullMode::Off,
            render_queue: RenderQueue::new(RenderQueue::TRANSPARENT),
            z_write: false,
            src_blend: BlendMode::SrcAlpha,
            dst_blend: BlendMode::OneMinusSrcAlpha,
            ..Default::default()
        }),
    );
    let items = scene_render_system(&world);
    assert_eq!(items[0].state.cull_mode, CullMode::Off);
    assert_eq!(items[0].state.render_queue, RenderQueue::TRANSPARENT);
    assert!(!items[0].state.z_write);
    assert!(items[0].state.blends());
}

#[test]
fn two_sided_requests_two_sided_shadow_casting() {
    let mut world = World::new();
    spawn_renderable(
        &mut world,
        Some(MeshRenderer {
            shadow_casting_mode: ShadowCastingMode::TwoSided,
            ..Default::default()
        }),
    );

    let items = scene_render_system(&world);
    // `ShadowCastingMode::TwoSided`：投影且阴影通道关闭背面剔除。
    assert!(items[0].cast_shadow);
    assert!(items[0].visible_in_main_pass);
    assert!(items[0].cast_shadow_two_sided);
}

// ---------------------------------------------------------------------------
// 2D 精灵
// ---------------------------------------------------------------------------

fn spawn_sprite(
    world: &mut World,
    translation: glam::Vec3,
    sprite: SpriteRenderer,
    texture_size: [u32; 2],
    texture_slot: u32,
) -> prism_ecs::Entity {
    let e = world.spawn();
    let local = LocalTransform {
        position: translation,
        ..LocalTransform::default()
    };
    world.insert(e, LocalToWorld(local.to_model_matrix()));
    world.insert(e, sprite);
    world.insert(
        e,
        SpriteRef {
            asset_id: SceneAssetId::generate(),
            texture_slot,
            size: texture_size,
            generation: 1,
        },
    );
    e
}

#[test]
fn sprite_entity_yields_draw_item_with_world_size() {
    let mut world = World::new();
    spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "sprites/boat.png".into(),
            pixels_per_unit: 100.0,
            ..SpriteRenderer::default()
        },
        [32, 32],
        7,
    );

    let items = scene_sprite_system(&world);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].texture, 7);
    assert_eq!(items[0].uv_rect, [0.0, 0.0, 1.0, 1.0]);
    // 32 px / 100 ppu = 0.32 world units, centred pivot → no offset.
    assert!((items[0].model[0][0] - 0.32).abs() < 1e-6);
    assert!((items[0].model[1][1] - 0.32).abs() < 1e-6);
    assert!((items[0].model[3][0]).abs() < 1e-6);
}

#[test]
fn sprite_pivot_offsets_the_quad() {
    let mut world = World::new();
    spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "a.png".into(),
            pixels_per_unit: 100.0,
            pivot: [0.0, 0.0], // bottom-left
            ..SpriteRenderer::default()
        },
        [100, 100],
        0,
    );

    let items = scene_sprite_system(&world);
    // Pivot at bottom-left: the quad shifts +X and +Y by half a unit so the
    // entity origin lands on the sprite's bottom-left corner.
    assert!((items[0].model[3][0] - 0.5).abs() < 1e-6);
    assert!((items[0].model[3][1] - 0.5).abs() < 1e-6);
}

#[test]
fn sprite_sheet_frame_selects_horizontal_uv_slice() {
    let mut world = World::new();
    spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "sheet.png".into(),
            frame_size: [32, 32],
            frame_index: 2,
            ..SpriteRenderer::default()
        },
        [128, 32],
        0,
    );

    let items = scene_sprite_system(&world);
    assert_eq!(items[0].uv_rect, [0.5, 0.0, 0.75, 1.0]);
}

#[test]
fn sprite_sheet_rows_select_the_vertical_uv_slice() {
    // #584：8x4 图集（128x64 / 每帧 32x32），选第 2 行、行内第 2 列。
    let mut world = World::new();
    spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "sheet.png".into(),
            frame_size: [32, 32],
            sheet_row: 1,
            frame_index: 1,
            ..SpriteRenderer::default()
        },
        [128, 64],
        0,
    );

    let items = scene_sprite_system(&world);
    assert_eq!(items[0].uv_rect, [0.25, 0.5, 0.5, 1.0]);
}

#[test]
fn explicit_sheet_columns_override_the_texture_derivation() {
    // 纹理按 32px 宽能切 4 列，但作者显式声明 2 列：frame_index=2 应进入第 2 行。
    let mut world = World::new();
    spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "sheet.png".into(),
            frame_size: [32, 32],
            sheet_columns: 2,
            frame_index: 2,
            ..SpriteRenderer::default()
        },
        [128, 64],
        0,
    );

    let items = scene_sprite_system(&world);
    assert_eq!(items[0].uv_rect, [0.0, 0.5, 0.25, 1.0]);
}

#[test]
fn sprite_blend_state_reaches_the_draw_item() {
    // #610：加色辉光 = SrcAlpha / One，而不是默认的 alpha 混合。
    let mut world = World::new();
    spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "glow.png".into(),
            src_blend: BlendMode::SrcAlpha,
            dst_blend: BlendMode::One,
            blend_op: BlendOp::Add,
            ..SpriteRenderer::default()
        },
        [64, 64],
        0,
    );
    let items = scene_sprite_system(&world);
    assert_eq!(items[0].src_blend, BlendMode::SrcAlpha);
    assert_eq!(items[0].dst_blend, BlendMode::One);
    assert_eq!(items[0].blend_op, BlendOp::Add);
}

#[test]
fn sprite_flip_swaps_uv_edges() {
    let mut world = World::new();
    spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "sheet.png".into(),
            frame_size: [32, 32],
            frame_index: 0,
            flip_x: true,
            flip_y: true,
            ..SpriteRenderer::default()
        },
        [64, 32],
        0,
    );

    let items = scene_sprite_system(&world);
    assert_eq!(items[0].uv_rect, [0.5, 1.0, 0.0, 0.0]);
}

#[test]
fn sprites_sort_by_layer_order_then_z() {
    let mut world = World::new();
    let make = |layer: i32, order: i32| SpriteRenderer {
        sprite_path: "a.png".into(),
        sorting_layer: layer,
        order_in_layer: order,
        ..SpriteRenderer::default()
    };

    // Spawned out of order on purpose.
    let near = spawn_sprite(
        &mut world,
        glam::Vec3::new(0.0, 0.0, 1.0),
        make(0, 0),
        [10, 10],
        1,
    );
    let far = spawn_sprite(
        &mut world,
        glam::Vec3::new(0.0, 0.0, -1.0),
        make(0, 0),
        [10, 10],
        2,
    );
    let top = spawn_sprite(
        &mut world,
        glam::Vec3::new(0.0, 0.0, -5.0),
        make(1, 0),
        [10, 10],
        3,
    );
    let ordered = spawn_sprite(
        &mut world,
        glam::Vec3::new(0.0, 0.0, -9.0),
        make(0, 1),
        [10, 10],
        4,
    );
    let _ = (near, far, top, ordered);

    let textures: Vec<u32> = scene_sprite_system(&world)
        .iter()
        .map(|i| i.texture)
        .collect();
    // layer 0 first (order 0 → z ascending, then order 1), then layer 1.
    assert_eq!(textures, vec![2, 1, 4, 3]);
}

#[test]
fn unresolved_sprite_is_skipped() {
    let mut world = World::new();
    let e = spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "a.png".into(),
            ..SpriteRenderer::default()
        },
        [10, 10],
        0,
    );
    world.get_mut::<SpriteRef>(e).unwrap().generation = 0;

    assert!(scene_sprite_system(&world).is_empty());
}

// ---------------------------------------------------------------------------
// 2D 折线（LineRenderer）
// ---------------------------------------------------------------------------

fn spawn_line(world: &mut World, points: Vec<[f32; 3]>, layer: i32) -> prism_ecs::Entity {
    let e = world.spawn();
    world.insert(e, LocalToWorld(glam::Mat4::IDENTITY));
    world.insert(
        e,
        LineRenderer {
            points,
            width: 2.0,
            color: [1.0, 0.5, 0.0, 1.0],
            sorting_layer: layer,
            order_in_layer: 0,
        },
    );
    e
}

#[test]
fn line_renders_one_quad_per_segment() {
    let mut world = World::new();
    spawn_line(
        &mut world,
        vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 10.0, 0.0]],
        0,
    );

    let items = scene_line_system(&world);
    assert_eq!(items.len(), 2, "3 points -> 2 segments");
    for item in &items {
        assert_eq!(item.texture, SOLID_TEXTURE);
        assert_eq!(item.color, [1.0, 0.5, 0.0, 1.0]);
    }

    // First segment runs along +X: m0 = (10, 0), m1 = (0, 2) (width 2).
    assert!((items[0].model[0][0] - 10.0).abs() < 1e-5);
    assert!((items[0].model[0][1]).abs() < 1e-5);
    assert!((items[0].model[1][1] - 2.0).abs() < 1e-5);
    // Midpoint of the first segment.
    assert!((items[0].model[3][0] - 5.0).abs() < 1e-5);
}

#[test]
fn line_with_one_point_is_skipped() {
    let mut world = World::new();
    spawn_line(&mut world, vec![[0.0, 0.0, 0.0]], 0);
    assert!(scene_line_system(&world).is_empty());
}

#[test]
fn degenerate_segment_is_skipped() {
    let mut world = World::new();
    spawn_line(&mut world, vec![[1.0, 1.0, 0.0], [1.0, 1.0, 0.0]], 0);
    assert!(scene_line_system(&world).is_empty());
}

#[test]
fn sprites_and_lines_sort_together() {
    let mut world = World::new();
    spawn_sprite(
        &mut world,
        glam::Vec3::ZERO,
        SpriteRenderer {
            sprite_path: "a.png".into(),
            sorting_layer: 1,
            ..SpriteRenderer::default()
        },
        [10, 10],
        7,
    );
    spawn_line(&mut world, vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]], 5);

    let items = scene_2d_system(&world);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].texture, 7, "sprite (layer 1) draws first");
    assert_eq!(
        items[1].texture, SOLID_TEXTURE,
        "line (layer 5) draws on top"
    );
}

/// #433：关掉父级，子物体也必须从 draw list 里消失（Unity 的
/// `activeInHierarchy`）。派生值由 `active_hierarchy_system` 写入。
#[test]
fn child_of_an_inactive_parent_is_skipped() {
    let mut world = World::new();
    let parent = world.spawn();
    let child = world.spawn();
    world.insert(parent, Active(false));
    world.insert(child, Active(true));
    crate::scene::helpers::HierarchyHelper::reparent(&mut world, child, Some(parent));
    world.insert(
        child,
        LocalToWorld(glam::Mat4::from_cols_array_2d(&[[1.0; 4]; 4])),
    );
    world.insert(
        child,
        MeshRef {
            asset_id: SceneAssetId::generate(),
            render_handle: MeshHandle::default(),
            generation: 1,
        },
    );
    world.insert(
        child,
        MaterialRef {
            asset_id: SceneAssetId::generate(),
            material_slot: 0,
            generation: 1,
        },
    );

    crate::scene::systems::hierarchy::active_hierarchy_system(&mut world);
    assert!(scene_render_system(&world).is_empty());

    // 父级重新打开 → 子物体回来。
    world.insert(parent, Active(true));
    crate::scene::systems::hierarchy::active_hierarchy_system(&mut world);
    assert_eq!(scene_render_system(&world).len(), 1);
}

// ── 视锥剔除（#491） ──────────────────────────────────────────────

/// 相机在原点、看向 −Z 的视锥（与 `prism-render` 的 culling 测试同一构造）。
fn camera_frustum() -> prism_render::culling::Frustum {
    // 走引擎自己的投影（Vulkan 深度 [0,1] + y-flip），不要另写一份公式。
    let camera = prism_scene::Camera {
        fov_y_degrees: 45.0,
        near: 0.1,
        far: 100.0,
        ..prism_scene::Camera::default()
    };
    let view = glam::camera::rh::view::look_at_mat4(
        glam::Vec3::ZERO,
        glam::Vec3::new(0.0, 0.0, -1.0),
        glam::Vec3::Y,
    );
    let view_proj = camera.projection_matrix(1.0) * view;
    prism_render::culling::Frustum::from_view_proj(&view_proj.to_cols_array_2d())
        .expect("camera frustum must be valid")
}

fn spawn_mesh(world: &mut World, model: glam::Mat4) -> prism_ecs::Entity {
    let entity = world.spawn();
    world.insert(entity, LocalToWorld(model));
    world.insert(
        entity,
        MeshRef {
            asset_id: SceneAssetId::generate(),
            render_handle: MeshHandle::default(),
            generation: 1,
        },
    );
    entity
}

fn unit_bounds() -> prism_render::culling::Aabb {
    prism_render::culling::Aabb::new([-0.5; 3], [0.5; 3])
}

#[test]
fn a_mesh_outside_the_camera_frustum_is_culled() {
    let mut world = World::new();
    // 两个实体共用同一个网格句柄：区别只在模型矩阵。
    spawn_mesh(
        &mut world,
        glam::Mat4::from_translation(glam::Vec3::new(0.0, 0.0, -5.0)),
    );
    spawn_mesh(
        &mut world,
        glam::Mat4::from_translation(glam::Vec3::new(0.0, 0.0, 5.0)),
    );
    let mut bounds = MeshBounds::default();
    bounds.insert(MeshHandle::default(), unit_bounds());
    world.insert_resource(bounds);

    let frustum = camera_frustum();
    let mut out = Vec::new();
    scene_render_system_into_with_frustum(&world, Some(&frustum), &mut out);

    assert_eq!(out.len(), 1, "只有相机前的网格留下");
    assert!(out[0].model[3][2] < 0.0, "留下的是 −Z 方向的实体");
}

#[test]
fn meshes_without_bounds_are_never_culled() {
    let mut world = World::new();
    spawn_mesh(
        &mut world,
        glam::Mat4::from_translation(glam::Vec3::new(0.0, 0.0, 5.0)),
    );

    let frustum = camera_frustum();
    let mut out = Vec::new();
    // 没有 `MeshBounds`：程序化网格 / 还没解析完的网格照旧提交。
    scene_render_system_into_with_frustum(&world, Some(&frustum), &mut out);
    assert_eq!(out.len(), 1);
}

#[test]
fn culling_keeps_the_rest_of_the_extraction_rules() {
    let mut world = World::new();
    let entity = spawn_mesh(
        &mut world,
        glam::Mat4::from_translation(glam::Vec3::new(0.0, 0.0, -5.0)),
    );
    world.insert(entity, Active(false));
    let mut bounds = MeshBounds::default();
    bounds.insert(MeshHandle::default(), unit_bounds());
    world.insert_resource(bounds);

    let frustum = camera_frustum();
    let mut out = Vec::new();
    scene_render_system_into_with_frustum(&world, Some(&frustum), &mut out);
    assert!(out.is_empty(), "视锥内但未激活的实体仍然不画");
}

#[test]
fn no_frustum_matches_the_unculled_extraction() {
    let mut world = World::new();
    spawn_mesh(
        &mut world,
        glam::Mat4::from_translation(glam::Vec3::new(0.0, 0.0, -5.0)),
    );
    spawn_mesh(
        &mut world,
        glam::Mat4::from_translation(glam::Vec3::new(0.0, 0.0, 5.0)),
    );
    let mut bounds = MeshBounds::default();
    bounds.insert(MeshHandle::default(), unit_bounds());
    world.insert_resource(bounds);

    let mut culled = Vec::new();
    scene_render_system_into_with_frustum(&world, None, &mut culled);
    assert_eq!(culled.len(), 2);
    assert_eq!(culled.len(), scene_render_system(&world).len());
}
