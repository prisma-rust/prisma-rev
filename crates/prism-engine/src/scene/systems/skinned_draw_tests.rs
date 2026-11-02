use super::*;
use crate::scene::runtime_components::{MaterialRef, MeshRef};
use prism_render::render_graph::DrawItem;
use prism_scene::components::{LocalToWorld, SceneAssetId, SkinnedMeshRenderer};

fn bones(count: usize) -> Vec<glam::Mat4> {
    (0..count)
        .map(|index| glam::Mat4::from_translation(glam::vec3(index as f32, 0.0, 0.0)))
        .collect()
}

fn spawn_skinned(world: &mut prism_ecs::World, palette: Option<usize>) -> prism_ecs::Entity {
    let entity = world.spawn();
    world.insert(entity, LocalToWorld(glam::Mat4::IDENTITY));
    world.insert(
        entity,
        SkinnedMeshRenderer {
            skinned_mesh_path: "char/hero.rskin".into(),
            material_path: "char/hero.mat.json".into(),
            ..Default::default()
        },
    );
    world.insert(
        entity,
        MeshRef {
            asset_id: SceneAssetId::generate(),
            render_handle: prism_render::managers::MeshHandle::default(),
            generation: 1,
        },
    );
    // `MeshBounds` 是 World 资源（#491），不是实体组件。
    if world.get_resource::<MeshBounds>().is_none() {
        let mut bounds = MeshBounds::default();
        bounds.insert(
            prism_render::managers::MeshHandle::default(),
            prism_render::culling::Aabb::new([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]),
        );
        world.insert_resource(bounds);
    }
    if let Some(count) = palette {
        world.insert(entity, BonePalette::from_matrices(&bones(count)));
    }
    entity
}

#[test]
fn a_skinned_entity_without_a_palette_draws_in_bind_pose() {
    let mut world = prism_ecs::World::new();
    spawn_skinned(&mut world, None);
    let mut items: Vec<DrawItem> = Vec::new();
    let mut palette: Vec<[[f32; 4]; 4]> = Vec::new();

    let added = append_skinned_draw_items(&world, None, &mut items, &mut palette);

    assert_eq!(added, 1);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].bone_offset, None, "没有调色板 = 静态路径");
    assert!(palette.is_empty(), "bind pose 不需要上传骨骼矩阵");
}

#[test]
fn a_palette_is_appended_and_the_offset_points_at_it() {
    let mut world = prism_ecs::World::new();
    let first = spawn_skinned(&mut world, Some(2));
    let second = spawn_skinned(&mut world, Some(3));
    let mut items: Vec<DrawItem> = Vec::new();
    let mut palette: Vec<[[f32; 4]; 4]> = Vec::new();

    let added = append_skinned_draw_items(&world, None, &mut items, &mut palette);

    assert_eq!(added, 2);
    assert_eq!(palette.len(), 5, "两个实体的骨骼矩阵首尾相接");
    let offsets: Vec<Option<u32>> = items.iter().map(|item| item.bone_offset).collect();
    assert_eq!(offsets, vec![Some(0), Some(2)]);
    // 第二份调色板真的在 offset 2 之后。
    assert!((palette[2][3][0] - 0.0).abs() < 1e-6);
    assert!((palette[3][3][0] - 1.0).abs() < 1e-6);
    let _ = (first, second);
}

#[test]
fn a_palette_that_does_not_fit_the_bone_budget_falls_back_to_the_bind_pose() {
    let mut world = prism_ecs::World::new();
    spawn_skinned(&mut world, Some(BONE_BUDGET + 1));
    let mut items: Vec<DrawItem> = Vec::new();
    let mut palette: Vec<[[f32; 4]; 4]> = Vec::new();

    append_skinned_draw_items(&world, None, &mut items, &mut palette);

    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].bone_offset, None,
        "超预算时宁可用 bind pose，也不能越界读骨骼缓冲"
    );
    assert!(palette.is_empty());
}

#[test]
fn the_remaining_budget_shrinks_as_palettes_are_appended() {
    let mut world = prism_ecs::World::new();
    spawn_skinned(&mut world, Some(BONE_BUDGET - 1));
    spawn_skinned(&mut world, Some(2));
    let mut items: Vec<DrawItem> = Vec::new();
    let mut palette: Vec<[[f32; 4]; 4]> = Vec::new();

    append_skinned_draw_items(&world, None, &mut items, &mut palette);

    assert_eq!(items[0].bone_offset, Some(0));
    assert_eq!(
        items[1].bone_offset, None,
        "第二份放不下了：剩 1 个槽，但它需要 2 个"
    );
    assert_eq!(palette.len(), BONE_BUDGET - 1);
}

#[test]
fn a_frustum_culls_the_skinned_draw_item() {
    let mut world = prism_ecs::World::new();
    let entity = spawn_skinned(&mut world, None);
    // 相机在原点朝 −Z，把物体推到 x=1000（画面外）→ 整个盒在视锥外。
    world.insert(
        entity,
        LocalToWorld(glam::Mat4::from_translation(glam::vec3(1000.0, 0.0, -5.0))),
    );
    let mut items: Vec<DrawItem> = Vec::new();
    let mut palette: Vec<[[f32; 4]; 4]> = Vec::new();
    let projection = prism_scene::components::Camera {
        fov_y_degrees: 60.0,
        near: 0.1,
        far: 100.0,
        ..Default::default()
    }
    .projection_matrix(1.0);
    let frustum = prism_render::culling::Frustum::from_view_proj(&projection.to_cols_array_2d())
        .expect("usable frustum");

    let added = append_skinned_draw_items(&world, Some(&frustum), &mut items, &mut palette);

    assert_eq!(added, 0);
    assert!(items.is_empty());
    assert!(palette.is_empty(), "被剔除的实体不该占用骨骼预算");
}

#[test]
fn submeshes_become_one_draw_item_each() {
    let mut world = prism_ecs::World::new();
    let entity = spawn_skinned(&mut world, None);
    world.insert(
        entity,
        crate::scene::runtime_components::MeshSubmeshes {
            submeshes: vec![
                crate::scene::runtime_components::MeshSubmesh {
                    index_start: 0,
                    index_count: 36,
                    material_index: Some(0),
                },
                crate::scene::runtime_components::MeshSubmesh {
                    index_start: 36,
                    index_count: 12,
                    material_index: None,
                },
            ],
        },
    );
    world.insert(
        entity,
        MaterialRef {
            asset_id: SceneAssetId::generate(),
            material_slot: 7,
            generation: 1,
        },
    );
    let mut items: Vec<DrawItem> = Vec::new();
    let mut palette: Vec<[[f32; 4]; 4]> = Vec::new();

    let added = append_skinned_draw_items(&world, None, &mut items, &mut palette);

    assert_eq!(added, 2);
    assert_eq!(items[0].index_start, 0);
    assert_eq!(items[0].index_count, Some(36));
    assert_eq!(items[1].index_start, 36);
    assert_eq!(items[1].index_count, Some(12));
    // 没有 `MaterialSlots` 时回落到实体级的材质槽。
    assert_eq!(items[0].material, Some(7));
    assert_eq!(items[1].material, Some(7));
}
