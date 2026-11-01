use super::*;

fn ramp(cells: usize) -> Heightfield {
    let mut heights = Vec::with_capacity(cells * cells);
    for y in 0..cells {
        for x in 0..cells {
            heights.push(x as f32 / (cells - 1) as f32 + y as f32 / (cells - 1) as f32);
        }
    }
    // 直接走 from_raw，避免为测试开一个只有测试用的构造器。
    let bytes: Vec<u8> = heights
        .iter()
        .flat_map(|value| (value * 0.5).to_le_bytes())
        .collect();
    Heightfield::from_raw(&bytes, cells).expect("valid ramp")
}

/// 直接塞一个运行时 `TerrainData`（跳过文件加载），供查询测试用。
fn terrain_world(
    field: Heightfield,
    size: [f32; 2],
    translation: glam::Vec3,
) -> (prism_ecs::World, prism_ecs::Entity) {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        TerrainData {
            key: "test".to_string(),
            field: Arc::new(field),
            mesh_path: String::new(),
            size,
            height_scale: 1.0,
        },
    );
    world.insert(
        entity,
        LocalToWorld(glam::Mat4::from_translation(translation)),
    );
    (world, entity)
}

#[test]
fn terrain_raycast_hits_a_flat_field_from_above() {
    let (world, entity) = terrain_world(Heightfield::flat(4), [10.0, 10.0], glam::Vec3::ZERO);
    let hit = terrain_raycast(&world, [0.0, 5.0, 0.0], [0.0, -1.0, 0.0], 20.0)
        .expect("ray must hit the flat terrain");
    assert_eq!(hit.entity, entity);
    assert!(
        (hit.distance - 5.0).abs() < 1e-3,
        "distance {}",
        hit.distance
    );
    assert!(hit.point[1].abs() < 1e-3, "point {}", hit.point[1]);
    assert!(hit.normal[1] > 0.99, "normal {:?}", hit.normal);
}

#[test]
fn terrain_raycast_refines_a_slope() {
    // 2×2 斜坡：中心插值到 0.5，法线应该倾斜。
    let (world, _) = terrain_world(ramp(2), [10.0, 10.0], glam::Vec3::ZERO);
    let hit = terrain_raycast(&world, [0.0, 5.0, 0.0], [0.0, -1.0, 0.0], 20.0).expect("hit slope");
    assert!((hit.point[1] - 0.5).abs() < 0.02, "hit y {}", hit.point[1]);
    assert!(hit.normal[1] < 0.99, "slope normal {:?}", hit.normal);
    let length = glam::Vec3::from_array(hit.normal).length();
    assert!((length - 1.0).abs() < 1e-3, "normal length {length}");
}

#[test]
fn terrain_raycast_respects_distance_coverage_and_direction() {
    let (world, _) = terrain_world(Heightfield::flat(4), [10.0, 10.0], glam::Vec3::ZERO);

    assert!(terrain_raycast(&world, [0.0, 5.0, 0.0], [0.0, -1.0, 0.0], 3.0).is_none());
    assert!(terrain_raycast(&world, [20.0, 5.0, 0.0], [0.0, -1.0, 0.0], 20.0).is_none());
    assert!(terrain_raycast(&world, [0.0, 5.0, 0.0], [0.0, 1.0, 0.0], 20.0).is_none());
    assert!(terrain_raycast(&world, [0.0, -1.0, 0.0], [0.0, -1.0, 0.0], 20.0).is_none());
    assert!(terrain_raycast(&world, [0.0, 5.0, 0.0], [0.0, 0.0, 0.0], 20.0).is_none());
}

#[test]
fn terrain_sample_uses_the_highest_overlapping_field() {
    let (mut world, _) = terrain_world(Heightfield::flat(4), [10.0, 10.0], glam::Vec3::ZERO);
    let high = world.spawn();
    world.insert(
        high,
        TerrainData {
            key: "high".to_string(),
            field: Arc::new(Heightfield::flat(4)),
            mesh_path: String::new(),
            size: [10.0, 10.0],
            height_scale: 1.0,
        },
    );
    world.insert(
        high,
        LocalToWorld(glam::Mat4::from_translation(glam::vec3(0.0, 2.0, 0.0))),
    );

    let (entity, height, _) = terrain_sample(&world, 0.0, 0.0).expect("covered");
    assert_eq!(entity, high);
    assert!((height - 2.0).abs() < 1e-4);
}

#[test]
fn a_heightmap_needs_exactly_one_float_per_cell() {
    assert!(Heightfield::from_raw(&[], 2).is_none());
    assert!(Heightfield::from_raw(&[0.0f32.to_le_bytes()].concat(), 2).is_none());
    let ok = vec![0u8; 4 * 4 * 4];
    assert!(Heightfield::from_raw(&ok, 4).is_some());
    assert!(
        Heightfield::from_raw(&ok, 5).is_none(),
        "长度必须和分辨率一致"
    );
}

#[test]
fn sampling_is_bilinear_and_clamped() {
    let field = ramp(3);
    // 角点就是原始值。
    assert!((field.sample_height(0.0, 0.0) - 0.0).abs() < 1e-6);
    assert!((field.sample_height(1.0, 1.0) - 1.0).abs() < 1e-6);
    // 中点插值。
    let middle = field.sample_height(0.5, 0.5);
    assert!((middle - 0.5).abs() < 1e-6, "got {middle}");
    // 越界夹到边界。
    assert!((field.sample_height(-1.0, 0.0) - 0.0).abs() < 1e-6);
    assert!((field.sample_height(2.0, 1.0) - 1.0).abs() < 1e-6);
}

#[test]
fn heights_keep_the_units_the_cli_writes() {
    // heightmap CLI 写出的是米（可以为负），不能被钳成 [0, 1]。
    let mut bytes = Vec::new();
    for value in [-11000.0f32, 250.0, 0.0, 8850.0] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let field = Heightfield::from_raw(&bytes, 2).expect("2x2 field");
    assert!((field.sample_height(0.0, 0.0) + 11000.0).abs() < 1e-3);
    assert!((field.sample_height(1.0, 0.0) - 250.0).abs() < 1e-3);
    assert!((field.sample_height(1.0, 1.0) - 8850.0).abs() < 1e-3);
}

#[test]
fn a_rising_slope_tilts_the_normal_away_from_plus_x() {
    let field = ramp(5);
    let normal = field.sample_normal(0.5, 0.5, 10.0);
    assert!(normal[1] > 0.0, "normal points up: {normal:?}");
    assert!(normal[0] < 0.0, "the slope rises along +x: {normal:?}");
    let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    assert!((length - 1.0).abs() < 1e-4);
}

#[test]
fn the_mesh_covers_the_requested_size() {
    let field = Heightfield::flat(4);
    let mesh = field.build_mesh([20.0, 10.0], 5.0);
    assert_eq!(mesh.positions.len(), 16);
    assert_eq!(mesh.indices.len(), 9 * 6);
    let xs: Vec<f32> = mesh.positions.iter().map(|p| p[0]).collect();
    let zs: Vec<f32> = mesh.positions.iter().map(|p| p[2]).collect();
    assert!((xs.iter().cloned().fold(f32::INFINITY, f32::min) + 10.0).abs() < 1e-5);
    assert!((xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max) - 10.0).abs() < 1e-5);
    assert!((zs.iter().cloned().fold(f32::INFINITY, f32::min) + 5.0).abs() < 1e-5);
    // 全平的地面：法线都是 +Y，高度是 0。
    assert!(mesh.normals.iter().all(|n| (n[1] - 1.0).abs() < 1e-5));
    assert!(mesh.positions.iter().all(|p| p[1].abs() < 1e-6));
    assert_eq!(mesh.uvs.len(), 16);
}

#[test]
fn height_scale_scales_the_vertices() {
    let field = ramp(2);
    let mesh = field.build_mesh([1.0, 1.0], 100.0);
    let highest = mesh
        .positions
        .iter()
        .map(|p| p[1])
        .fold(f32::NEG_INFINITY, f32::max);
    assert!((highest - 100.0).abs() < 1e-3, "got {highest}");
}

#[test]
fn the_terrain_key_changes_with_the_parameters() {
    let base = Terrain::default();
    let key = terrain_key(1, &base);
    assert_eq!(key, terrain_key(1, &base));
    assert_ne!(key, terrain_key(2, &base), "换个实体要重建");
    let mut scaled = base.clone();
    scaled.height_scale = 20.0;
    assert_ne!(key, terrain_key(1, &scaled));
    let mut smoother = base.clone();
    smoother.resolution = 64;
    assert_ne!(key, terrain_key(1, &smoother));
}

#[test]
fn the_terrain_system_builds_a_mesh_and_points_the_renderer_at_it() {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(entity, Terrain::default());
    world.insert(entity, MeshRenderer::default());

    terrain_system(&mut world);

    let data = world.get::<TerrainData>(entity).expect("terrain data");
    assert!(data.mesh_path.starts_with(PROCEDURAL_PREFIX));
    let renderer = world.get::<MeshRenderer>(entity).expect("renderer");
    assert_eq!(renderer.mesh_path, data.mesh_path);
    let meshes = world.get_resource::<ProceduralMeshes>().expect("meshes");
    assert!(
        meshes.get(&data.mesh_path).is_some(),
        "网格要登记进程序化网格表"
    );
    assert!(world.get::<MeshRef>(entity).is_some(), "要排队上传");

    // 参数没变：第二次跑不重建（路径不变、引用还在）。
    let path = data.mesh_path.clone();
    terrain_system(&mut world);
    assert_eq!(world.get::<TerrainData>(entity).unwrap().mesh_path, path);

    // 参数变了：新路径，旧网格被回收。
    world.get_mut::<Terrain>(entity).unwrap().height_scale = 42.0;
    terrain_system(&mut world);
    let new_path = world.get::<TerrainData>(entity).unwrap().mesh_path.clone();
    assert_ne!(new_path, path);
    let meshes = world.get_resource::<ProceduralMeshes>().unwrap();
    assert!(meshes.get(&path).is_none(), "旧网格不该留在表里");
    assert!(meshes.get(&new_path).is_some());
}

#[test]
fn the_terrain_height_follows_the_entity_placement() {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(entity, Terrain::default());
    world.insert(
        entity,
        prism_scene::components::LocalToWorld(glam::Mat4::from_translation(glam::vec3(
            10.0, 2.0, -5.0,
        ))),
    );
    terrain_system(&mut world);

    // 默认地形是平的：到处都是 2.0（实体的高度）。
    let height = terrain_height(&world, 10.0, -5.0).expect("covered");
    assert!((height - 2.0).abs() < 1e-4);
    // 范围外没有地形。
    assert!(terrain_height(&world, 1000.0, -5.0).is_none());
}
