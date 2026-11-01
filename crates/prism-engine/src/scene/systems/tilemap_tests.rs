// ---------------------------------------------------------------------------
// Tests：瓦片地图 → SpriteDrawItem（#529）
// ---------------------------------------------------------------------------

use super::*;
use crate::scene::components::{SceneAssetId, SpriteRenderer};
use prism_ecs::Entity;

fn spawn_tilemap(
    world: &mut World,
    tilemap: Tilemap,
    translation: glam::Vec3,
    tileset_size: [u32; 2],
    texture_slot: u32,
    generation: u32,
) -> Entity {
    let entity = world.spawn();
    world.insert(
        entity,
        LocalToWorld(glam::Mat4::from_translation(translation)),
    );
    world.insert(entity, tilemap);
    world.insert(
        entity,
        TilemapRef {
            asset_id: SceneAssetId::generate(),
            texture_slot,
            size: tileset_size,
            generation,
        },
    );
    entity
}

fn map(tiles: Vec<u32>, columns: u32, cell_size: [f32; 2]) -> Tilemap {
    Tilemap {
        tileset_path: "sprites/tiles.png".into(),
        tile_size: [16, 16],
        cell_size,
        columns,
        tiles,
        ..Tilemap::default()
    }
}

/// 世界坐标中心（`model` 的平移列）。
fn center_of(item: &SpriteDrawItem) -> [f32; 3] {
    [item.model[3][0], item.model[3][1], item.model[3][2]]
}

#[test]
fn each_non_empty_cell_becomes_one_quad() {
    let mut world = World::new();
    // 2x2，只有左上和右下两格有瓦片。
    spawn_tilemap(
        &mut world,
        map(vec![1, 0, 0, 2], 2, [1.0, 1.0]),
        glam::Vec3::ZERO,
        [64, 64],
        5,
        1,
    );

    let items = tilemap_entries(&world, None);
    assert_eq!(items.len(), 2, "空格子不产生绘制项");
    assert_eq!(items[0].1.texture, 5);
    assert_eq!(items[1].1.texture, 5);
    // 行优先：左上先画。
    assert_eq!(center_of(&items[0].1), [0.5, 1.5, 0.0]);
    assert_eq!(center_of(&items[1].1), [1.5, 0.5, 0.0]);
}

#[test]
fn tile_uv_maps_the_index_into_the_tileset_grid() {
    let mut world = World::new();
    // tileset 64x32 + 16x16 瓦片 = 4 列 2 行；索引 5（= 第 6 块）落在第 2 行第 2 列。
    spawn_tilemap(
        &mut world,
        map(vec![6], 1, [1.0, 1.0]),
        glam::Vec3::ZERO,
        [64, 32],
        3,
        1,
    );

    let items = tilemap_entries(&world, None);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].1.uv_rect, [0.25, 0.5, 0.5, 1.0]);
}

#[test]
fn tile_index_is_clamped_to_the_tileset() {
    let mut world = World::new();
    // 索引 99 超出 4x2 的 tileset：夹到最后一块，不能采样到图外。
    spawn_tilemap(
        &mut world,
        map(vec![100], 1, [1.0, 1.0]),
        glam::Vec3::ZERO,
        [64, 32],
        3,
        1,
    );

    let items = tilemap_entries(&world, None);
    assert_eq!(items[0].1.uv_rect, [0.75, 0.5, 1.0, 1.0]);
}

#[test]
fn cells_are_placed_top_down_with_the_authored_cell_size() {
    let mut world = World::new();
    // 2 列 x 2 行，格子 2x1：第 0 行在上（y 大），行号越大越往下。
    spawn_tilemap(
        &mut world,
        map(vec![1, 1, 1, 1], 2, [2.0, 1.0]),
        glam::Vec3::ZERO,
        [64, 64],
        1,
        1,
    );

    let items = tilemap_entries(&world, None);
    let centers: Vec<[f32; 3]> = items.iter().map(|(_, item)| center_of(item)).collect();
    assert_eq!(
        centers,
        vec![
            [1.0, 1.5, 0.0],
            [3.0, 1.5, 0.0],
            [1.0, 0.5, 0.0],
            [3.0, 0.5, 0.0],
        ]
    );
}

#[test]
fn columns_zero_lays_the_tiles_out_in_one_row() {
    let mut world = World::new();
    spawn_tilemap(
        &mut world,
        map(vec![1, 2, 3], 0, [1.0, 1.0]),
        glam::Vec3::ZERO,
        [64, 64],
        1,
        1,
    );

    let map = world.query::<Tilemap>().next().unwrap().1;
    assert_eq!(map.grid(), (3, 1));
    let items = tilemap_entries(&world, None);
    assert_eq!(items.len(), 3);
    assert_eq!(center_of(&items[2].1), [2.5, 0.5, 0.0]);
}

#[test]
fn the_entity_transform_moves_the_whole_grid() {
    let mut world = World::new();
    spawn_tilemap(
        &mut world,
        map(vec![1], 1, [1.0, 1.0]),
        glam::Vec3::new(10.0, 20.0, -3.0),
        [16, 16],
        1,
        1,
    );

    let items = tilemap_entries(&world, None);
    assert_eq!(center_of(&items[0].1), [10.5, 20.5, -3.0]);
    // z 参与排序键（相机看 −Z，z 越大越近）。
    assert_eq!(items[0].0 .2, -3.0);
}

#[test]
fn unresolved_or_empty_tilemaps_draw_nothing() {
    let mut world = World::new();
    // generation = 0：tileset 还没上传完。
    spawn_tilemap(
        &mut world,
        map(vec![1], 1, [1.0, 1.0]),
        glam::Vec3::ZERO,
        [16, 16],
        1,
        0,
    );
    assert!(tilemap_entries(&world, None).is_empty());

    // 空路径 / 全空格 / 非法格子尺寸都不绘制。
    let mut world = World::new();
    spawn_tilemap(
        &mut world,
        Tilemap {
            tiles: vec![1, 2],
            ..Tilemap::default()
        },
        glam::Vec3::ZERO,
        [16, 16],
        1,
        1,
    );
    assert!(tilemap_entries(&world, None).is_empty(), "空 tileset_path");

    let mut world = World::new();
    spawn_tilemap(
        &mut world,
        map(vec![0, 0], 2, [1.0, 1.0]),
        glam::Vec3::ZERO,
        [16, 16],
        1,
        1,
    );
    assert!(tilemap_entries(&world, None).is_empty(), "没有非空格子");

    let mut world = World::new();
    spawn_tilemap(
        &mut world,
        map(vec![1], 1, [0.0, 1.0]),
        glam::Vec3::ZERO,
        [16, 16],
        1,
        1,
    );
    assert!(tilemap_entries(&world, None).is_empty(), "cell_size 为 0");
}

#[test]
fn inactive_tilemaps_are_skipped() {
    let mut world = World::new();
    let entity = spawn_tilemap(
        &mut world,
        map(vec![1], 1, [1.0, 1.0]),
        glam::Vec3::ZERO,
        [16, 16],
        1,
        1,
    );
    world.insert(entity, prism_scene::components::Active(false));
    assert!(tilemap_entries(&world, None).is_empty());
}

#[test]
fn the_camera_frustum_culls_off_screen_cells() {
    let mut world = World::new();
    // 8x1 的横向地图，格子 10 宽；视锥只看 x ∈ [-5, 5]。
    spawn_tilemap(
        &mut world,
        map(vec![1; 8], 8, [10.0, 10.0]),
        glam::Vec3::ZERO,
        [64, 64],
        1,
        1,
    );

    // 走引擎自己的投影（Vulkan 深度 + y-flip），不要另写一份公式。
    let camera = prism_scene::Camera {
        orthographic: true,
        orthographic_size: 5.0,
        near: 0.1,
        far: 100.0,
        ..prism_scene::Camera::default()
    };
    let view = glam::camera::rh::view::look_at_mat4(
        glam::Vec3::new(0.0, 0.0, 10.0),
        glam::Vec3::ZERO,
        glam::Vec3::Y,
    );
    let view_proj = camera.projection_matrix(1.0) * view;
    let frustum =
        prism_render::culling::Frustum::from_view_proj(&view_proj.to_cols_array_2d()).unwrap();

    let items = tilemap_entries(&world, Some(&frustum));
    let visible: Vec<f32> = items.iter().map(|(_, item)| center_of(item)[0]).collect();
    assert_eq!(visible, vec![5.0], "只有中心在视锥内的那一格");
}

#[test]
fn tilemaps_sort_against_sprites_by_layer() {
    let mut world = World::new();
    // 背景瓦片层 -1（远），精灵层 0（近）。
    let mut tilemap = map(vec![1], 1, [1.0, 1.0]);
    tilemap.sorting_layer = -1;
    spawn_tilemap(&mut world, tilemap, glam::Vec3::ZERO, [16, 16], 4, 1);
    let sprite = world.spawn();
    world.insert(
        sprite,
        LocalToWorld(glam::Mat4::from_translation(glam::Vec3::ZERO)),
    );
    world.insert(
        sprite,
        SpriteRenderer {
            sprite_path: "sprites/boat.png".into(),
            ..SpriteRenderer::default()
        },
    );
    world.insert(
        sprite,
        crate::scene::components::SpriteRef {
            asset_id: SceneAssetId::generate(),
            texture_slot: 9,
            size: [16, 16],
            generation: 1,
        },
    );

    let items = crate::scene::systems::render::scene_2d_system(&world);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].texture, 4, "背景瓦片先画");
    assert_eq!(items[1].texture, 9, "精灵后画");
}
