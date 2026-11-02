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
fn splat_colours_follow_height_and_can_be_disabled() {
    let field = ramp(5);
    let plain = field.build_mesh([10.0, 10.0], 1.0);
    assert!(
        plain.colors.is_empty(),
        "splat off keeps the old vertex stream"
    );

    let splat = field.build_mesh_with_splat([10.0, 10.0], 1.0, true);
    assert_eq!(splat.colors.len(), splat.positions.len());
    assert_ne!(splat.colors[0], splat.colors[splat.colors.len() - 1]);
    // 纯高度带（平地法线）的端点颜色。
    let low = terrain_splat_color(0.0, 0.0, 1.0, [0.0, 1.0, 0.0]);
    let high = terrain_splat_color(1.0, 0.0, 1.0, [0.0, 1.0, 0.0]);
    assert!((low[0] - SPLAT_SAND[0]).abs() < 1e-5);
    assert!((low[2] - SPLAT_SAND[2]).abs() < 1e-5);
    assert!((high[0] - SPLAT_SNOW[0]).abs() < 1e-5);
    assert!((high[2] - SPLAT_SNOW[2]).abs() < 1e-5);

    // LOD 网格同样带颜色，数量与顶点一致。
    let half = field.build_chunk_mesh_lod_with_splat([10.0, 10.0], 1.0, 0, 4, 0, 4, 2, true);
    assert_eq!(half.colors.len(), half.positions.len());
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
fn sculpt_raise_and_lower_only_touch_the_brush_radius() {
    let mut field = Heightfield::flat(9);
    let affected = field.sculpt(0.5, 0.5, 0.2, 5.0, SculptMode::Raise);
    assert!(affected > 0);
    assert!(field.sample_height(0.5, 0.5) > 4.9);
    assert_eq!(field.sample_height(0.0, 0.0), 0.0, "corner is outside");
    assert_eq!(field.sample_height(1.0, 1.0), 0.0, "corner is outside");

    field.sculpt(0.5, 0.5, 0.2, 5.0, SculptMode::Lower);
    assert!(field.sample_height(0.5, 0.5).abs() < 1e-4);
}

#[test]
fn sculpt_smooth_reduces_a_spike_and_flatten_uses_the_center_height() {
    let mut heights = vec![0.0f32; 9 * 9];
    heights[4 * 9 + 4] = 10.0;
    let bytes: Vec<u8> = heights
        .iter()
        .flat_map(|height| height.to_le_bytes())
        .collect();
    let mut field = Heightfield::from_raw(&bytes, 9).expect("9×9 field");
    field.sculpt(0.5, 0.5, 0.3, 0.5, SculptMode::Smooth);
    assert!(field.sample_height(0.5, 0.5) < 10.0);
    assert!(field.sample_height(0.5, 0.5) > 0.0);

    let mut ramp = ramp(5);
    ramp.sculpt(0.5, 0.5, 1.0, 1.0, SculptMode::Flatten);
    assert!((ramp.sample_height(0.5, 0.5) - 0.5).abs() < 1e-4);
}

#[test]
fn sculpt_ignores_invalid_parameters_and_raw_round_trips() {
    let mut field = Heightfield::flat(4);
    let before = field.heights().to_vec();
    assert_eq!(field.sculpt(f32::NAN, 0.5, 0.2, 1.0, SculptMode::Raise), 0);
    assert_eq!(field.sculpt(0.5, 0.5, -1.0, 1.0, SculptMode::Raise), 0);
    assert_eq!(field.sculpt(0.5, 0.5, 0.2, 0.0, SculptMode::Raise), 0);
    assert_eq!(field.heights(), before);

    field.sculpt(0.5, 0.5, 0.2, 2.0, SculptMode::Raise);
    let round_trip = Heightfield::from_raw(&field.to_raw_bytes(), 4).expect("round trip");
    assert_eq!(round_trip, field);
}

#[test]
fn vegetation_scatter_follows_the_heightfield_and_shares_one_mesh() {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        Terrain {
            size: [10.0, 10.0],
            chunks: 1,
            lod_distances: Vec::new(),
            vegetation_density: 100.0,
            vegetation_size: 2.0,
            ..Terrain::default()
        },
    );
    world.insert(entity, MeshRenderer::default());

    terrain_system(&mut world);

    let plants: Vec<(prism_ecs::Entity, TerrainVegetation)> = world
        .query::<TerrainVegetation>()
        .map(|(plant_entity, plant)| (plant_entity, plant.clone()))
        .collect();
    assert_eq!(plants.len(), 100, "density 100 × 100 m² / 100");
    let meshes = world.get_resource::<ProceduralMeshes>().expect("meshes");
    let mesh_path = plants[0].1.mesh_path.clone();
    assert!(meshes.get(&mesh_path).is_some());
    for (plant_entity, plant) in &plants {
        assert_eq!(plant.mesh_path, mesh_path, "all instances share one mesh");
        let transform = world
            .get::<LocalTransform>(*plant_entity)
            .expect("plant transform");
        assert!(transform.position.y.abs() < 1e-5, "flat terrain");
        assert!((1.5..=2.5).contains(&transform.scale.x));
        assert!(world.get::<MeshRef>(*plant_entity).is_some());
    }

    // 参数变化：重建，数量跟着密度走，旧实例回收。
    world.get_mut::<Terrain>(entity).unwrap().vegetation_density = 50.0;
    terrain_system(&mut world);
    assert_eq!(world.query::<TerrainVegetation>().count(), 50);
}

#[test]
fn vegetation_and_its_procedural_mesh_are_removed_with_the_terrain() {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        Terrain {
            chunks: 1,
            lod_distances: Vec::new(),
            vegetation_density: 20.0,
            ..Terrain::default()
        },
    );
    terrain_system(&mut world);
    let path = world
        .query::<TerrainVegetation>()
        .next()
        .map(|(_, plant)| plant.mesh_path.clone())
        .expect("plants");

    world.despawn(entity);
    terrain_system(&mut world);

    assert_eq!(world.query::<TerrainVegetation>().count(), 0);
    assert!(world
        .get_resource::<ProceduralMeshes>()
        .is_none_or(|meshes| meshes.get(&path).is_none()));
}

#[test]
fn the_default_vegetation_mesh_is_a_unit_cross_quad() {
    let mesh = vegetation_cross_mesh();
    assert_eq!(mesh.positions.len(), 8);
    assert_eq!(mesh.indices.len(), 12);
    assert!(mesh.positions.iter().all(|position| position[1] >= 0.0));
    assert!(mesh.positions.iter().any(|position| position[1] == 1.0));
}

#[test]
fn texture_splat_writes_weights_and_material_paths() {
    let layers = vec![
        "materials/sand.mat.json".to_string(),
        "materials/grass.mat.json".to_string(),
        "materials/rock.mat.json".to_string(),
        "materials/snow.mat.json".to_string(),
    ];
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        Terrain {
            chunks: 1,
            lod_distances: Vec::new(),
            splat_materials: layers.clone(),
            splat_tiling: 8.0,
            ..Terrain::default()
        },
    );
    world.insert(entity, MeshRenderer::default());
    terrain_system(&mut world);

    let data = world.get::<TerrainData>(entity).expect("terrain data");
    let mesh = world
        .get_resource::<ProceduralMeshes>()
        .and_then(|meshes| meshes.get(&data.mesh_path))
        .expect("mesh");
    assert_eq!(mesh.colors.len(), mesh.positions.len());
    assert_eq!(mesh.tangents.len(), mesh.positions.len());
    assert!(mesh.colors.iter().all(|color| {
        color[0] >= 0.0
            && color[1] >= 0.0
            && color[2] >= 0.0
            && color[0] + color[1] + color[2] <= 1.001
    }));

    let renderer = world.get::<MeshRenderer>(entity).expect("renderer");
    assert_eq!(renderer.material_paths, layers);
    assert_eq!(renderer.material_path, "materials/sand.mat.json");
    assert_eq!(
        world.get::<TerrainSplat>(entity).map(|splat| splat.tiling),
        Some(8.0)
    );
    assert!(world.get::<MaterialRef>(entity).is_some());
}

#[test]
fn texture_splat_weights_cover_sand_grass_rock_and_snow() {
    let sand = terrain_splat_weights(0.0, 0.0, 1.0, [0.0, 1.0, 0.0]);
    let snow = terrain_splat_weights(1.0, 0.0, 1.0, [0.0, 1.0, 0.0]);
    assert!(sand[0] > 0.9, "low ground is sand: {sand:?}");
    assert!(
        snow[0] < 0.05 && snow[2] < 0.05,
        "high ground is snow: {snow:?}"
    );

    let steep = terrain_splat_weights(0.5, 0.0, 1.0, [0.0, 0.2, 0.0]);
    assert!(steep[2] > 0.5, "steep slope leans to rock: {steep:?}");
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
    let mut chunked = base.clone();
    chunked.chunks = 8;
    assert_ne!(key, terrain_key(1, &chunked));
}

#[test]
fn the_terrain_system_builds_a_mesh_and_points_the_renderer_at_it() {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        Terrain {
            chunks: 1,
            lod_distances: Vec::new(),
            ..Terrain::default()
        },
    );
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

#[test]
fn the_terrain_system_splits_large_terrains_into_cullable_chunks() {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        Terrain {
            chunks: 2,
            resolution: 32,
            lod_distances: Vec::new(),
            ..Terrain::default()
        },
    );
    world.insert(
        entity,
        MeshRenderer {
            material_path: "materials/terrain.mat.json".to_string(),
            ..Default::default()
        },
    );
    terrain_system(&mut world);

    assert!(
        world.get::<MeshRef>(entity).is_none(),
        "多块模式下 owner 不再直接绘制"
    );
    let data = world.get::<TerrainData>(entity).expect("terrain data");
    assert!(data.mesh_path.starts_with(PROCEDURAL_PREFIX));

    let chunks: Vec<_> = world
        .query::<TerrainChunk>()
        .map(|(chunk_entity, chunk)| (chunk_entity, chunk.clone()))
        .collect();
    assert_eq!(chunks.len(), 4, "2×2 chunks");
    let sets: Vec<_> = world
        .query::<TerrainLodSet>()
        .map(|(set_entity, set)| (set_entity, set.clone()))
        .collect();
    assert_eq!(sets.len(), 1, "没有 LOD 阈值时只有一级 set");
    let children = world.get::<Child>(entity).expect("children");
    assert_eq!(children.0, vec![sets[0].0]);

    {
        let meshes = world.get_resource::<ProceduralMeshes>().expect("meshes");
        for (chunk_entity, chunk) in &chunks {
            assert_eq!(chunk.owner, entity);
            assert!(world
                .get::<prism_scene::components::Parent>(*chunk_entity)
                .is_some());
            assert!(world.get::<LocalTransform>(*chunk_entity).is_some());
            assert!(world.get::<MeshRef>(*chunk_entity).is_some());
            let renderer = world
                .get::<MeshRenderer>(*chunk_entity)
                .expect("chunk renderer");
            assert_eq!(renderer.mesh_path, chunk.mesh_path);
            assert_eq!(renderer.material_path, "materials/terrain.mat.json");
            let mesh = meshes.get(&chunk.mesh_path).expect("chunk mesh");
            assert!(!mesh.positions.is_empty());
        }
        // 32 samples / 2 chunks：接缝行被两边各存一次，所以顶点总数是 (16+17)²；
        // 三角形仍然只覆盖 31×31 个 quad。
        let total_vertices: usize = chunks
            .iter()
            .map(|(_, chunk)| meshes.get(&chunk.mesh_path).unwrap().positions.len())
            .sum();
        let total_indices: usize = chunks
            .iter()
            .map(|(_, chunk)| meshes.get(&chunk.mesh_path).unwrap().indices.len())
            .sum();
        assert_eq!(total_vertices, 33 * 33, "接缝行被两边各存一次");
        assert_eq!(total_indices, 31 * 31 * 6);
    }

    // 参数没变：不重建。
    terrain_system(&mut world);
    assert_eq!(world.query::<TerrainChunk>().count(), 4);

    // 切回单块：子实体与旧网格都回收，owner 重新进入上传队列。
    world.get_mut::<Terrain>(entity).unwrap().chunks = 1;
    terrain_system(&mut world);
    assert_eq!(world.query::<TerrainChunk>().count(), 0);
    assert_eq!(world.query::<TerrainLodSet>().count(), 0);
    assert!(world.get::<Child>(entity).is_none(), "空 Child 应被移除");
    assert!(world.get::<MeshRef>(entity).is_some());
    let data = world.get::<TerrainData>(entity).unwrap();
    assert!(world
        .get_resource::<ProceduralMeshes>()
        .is_some_and(|meshes| meshes.get(&data.mesh_path).is_some()));
    assert!(world
        .get::<MeshRenderer>(entity)
        .is_some_and(|renderer| renderer.mesh_path == data.mesh_path));
}

#[test]
fn adjacent_chunks_share_the_seam_and_keep_global_uvs() {
    let field = ramp(5);
    let left = field.build_chunk_mesh([10.0, 10.0], 1.0, 0, 2, 0, 2);
    let right = field.build_chunk_mesh([10.0, 10.0], 1.0, 2, 4, 0, 2);
    assert_eq!(left.positions.len(), 9);
    assert_eq!(right.positions.len(), 9);

    // 左块右边缘在局部 x=+2.5，右块左边缘在局部 x=-2.5；加上块中心后都落在世界 x=0。
    let left_edge: Vec<_> = left
        .positions
        .iter()
        .filter(|position| (position[0] - 2.5).abs() < 1e-5)
        .collect();
    let right_edge: Vec<_> = right
        .positions
        .iter()
        .filter(|position| (position[0] + 2.5).abs() < 1e-5)
        .collect();
    assert_eq!(left_edge.len(), 3);
    assert_eq!(right_edge.len(), 3);
    for (left, right) in left_edge.iter().zip(right_edge.iter()) {
        assert!((left[1] - right[1]).abs() < 1e-5, "seam heights differ");
        assert!((left[2] - right[2]).abs() < 1e-5, "seam z differs");
    }

    // UV 用整张地形的 [0,1]，接缝两侧都是 u=0.5。
    let seam_u = 2.0 / 4.0;
    assert!(left.uvs.iter().any(|uv| (uv[0] - seam_u).abs() < 1e-5));
    assert!(right.uvs.iter().any(|uv| (uv[0] - seam_u).abs() < 1e-5));
}

#[test]
fn terrain_lod_switches_all_chunks_together() {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        Terrain {
            chunks: 2,
            resolution: 32,
            lod_distances: vec![10.0],
            ..Terrain::default()
        },
    );
    world.insert(entity, LocalTransform::default());
    world.insert(entity, LocalToWorld(glam::Mat4::IDENTITY));
    let camera = world.spawn();
    world.insert(camera, LocalToWorld(glam::Mat4::IDENTITY));
    world.insert(camera, prism_scene::components::Camera::default());

    terrain_system(&mut world);

    let sets: Vec<_> = world
        .query::<TerrainLodSet>()
        .map(|(set_entity, set)| (set_entity, set.clone()))
        .collect();
    assert_eq!(sets.len(), 2, "one threshold → LOD0 + LOD1");
    assert_eq!(world.query::<TerrainChunk>().count(), 8);
    let visible_level = |world: &prism_ecs::World| -> usize {
        sets.iter()
            .find(|(set_entity, _)| {
                world.get::<Child>(*set_entity).is_some_and(|children| {
                    children.0.iter().any(|child| {
                        !world
                            .get::<crate::scene::components::LodHidden>(*child)
                            .is_some_and(|hidden| hidden.0)
                    })
                })
            })
            .map(|(_, set)| set.level)
            .unwrap_or(usize::MAX)
    };
    assert_eq!(visible_level(&world), 0, "camera at the origin → LOD0");

    world.insert(
        camera,
        LocalToWorld(glam::Mat4::from_translation(glam::vec3(100.0, 0.0, 0.0))),
    );
    terrain_system(&mut world);
    assert_eq!(visible_level(&world), 1, "far camera → LOD1");
}

#[test]
fn lod_meshes_reduce_resolution_but_keep_the_boundary() {
    let field = ramp(9);
    let full = field.build_chunk_mesh([10.0, 10.0], 1.0, 0, 8, 0, 8);
    let half = field.build_chunk_mesh_lod([10.0, 10.0], 1.0, 0, 8, 0, 8, 2);
    let quarter = field.build_chunk_mesh_lod([10.0, 10.0], 1.0, 0, 8, 0, 8, 4);
    assert_eq!(full.positions.len(), 9 * 9);
    assert_eq!(half.positions.len(), 5 * 5);
    assert_eq!(quarter.positions.len(), 3 * 3);

    // 四个角必须完全一致，否则相邻块 / 切换级别会露出裂缝。
    for mesh in [&half, &quarter] {
        let side = (mesh.positions.len() as f64).sqrt() as usize;
        for (full_index, lod_index) in [
            (0, 0),
            (8, side - 1),
            (72, (side - 1) * side),
            (80, side * side - 1),
        ] {
            assert_eq!(
                full.positions[full_index], mesh.positions[lod_index],
                "corner {full_index}"
            );
        }
    }
}

#[test]
fn despawning_a_terrain_removes_its_chunks_and_meshes() {
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(
        entity,
        Terrain {
            chunks: 2,
            resolution: 16,
            lod_distances: Vec::new(),
            ..Terrain::default()
        },
    );
    terrain_system(&mut world);

    let paths: Vec<String> = world
        .query::<TerrainChunk>()
        .map(|(_, chunk)| chunk.mesh_path.clone())
        .collect();
    assert_eq!(paths.len(), 4);

    world.despawn(entity);
    terrain_system(&mut world);

    assert_eq!(world.query::<TerrainChunk>().count(), 0);
    assert_eq!(world.query::<TerrainLodSet>().count(), 0);
    let meshes = world.get_resource::<ProceduralMeshes>().expect("meshes");
    for path in paths {
        assert!(meshes.get(&path).is_none(), "{path} should be gone");
    }
}
