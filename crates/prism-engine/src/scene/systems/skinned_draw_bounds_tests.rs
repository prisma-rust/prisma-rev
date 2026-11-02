use super::*;
use prism_render::culling::Aabb;

fn unit_bind() -> Aabb {
    Aabb::new([-0.5, -0.5, -0.5], [0.5, 0.5, 0.5])
}

fn identity_array() -> [[f32; 4]; 4] {
    glam::Mat4::IDENTITY.to_cols_array_2d()
}

fn translated(x: f32) -> [[f32; 4]; 4] {
    glam::Mat4::from_translation(glam::vec3(x, 0.0, 0.0)).to_cols_array_2d()
}

#[test]
fn an_identity_palette_leaves_the_bind_box_untouched() {
    let bounds = skinned_local_bounds(&unit_bind(), &[identity_array()]).expect("bounds");
    assert_eq!(bounds.min, unit_bind().min);
    assert_eq!(bounds.max, unit_bind().max);
}

#[test]
fn a_moving_bone_stretches_the_box_along_its_travel() {
    // 一个骨骼留在原地、另一个沿 +X 走了 4 米：蒙皮后的顶点落在这两个盒子之间，
    // 所以包围盒必须同时盖住 (−0.5..0.5) 和 (3.5..4.5)。
    let bounds =
        skinned_local_bounds(&unit_bind(), &[identity_array(), translated(4.0)]).expect("bounds");
    assert!(
        (bounds.min[0] + 0.5).abs() < 1e-5,
        "min.x = {}",
        bounds.min[0]
    );
    assert!(
        (bounds.max[0] - 4.5).abs() < 1e-5,
        "max.x = {}",
        bounds.max[0]
    );
    // 其它轴不动。
    assert!((bounds.min[1] + 0.5).abs() < 1e-5);
    assert!((bounds.max[2] - 0.5).abs() < 1e-5);
}

#[test]
fn a_broken_palette_is_ignored_and_an_empty_one_has_no_bounds() {
    assert!(skinned_local_bounds(&unit_bind(), &[]).is_none());
    let nan = glam::Mat4::from_translation(glam::vec3(f32::NAN, 0.0, 0.0)).to_cols_array_2d();
    assert!(
        skinned_local_bounds(&unit_bind(), &[nan]).is_none(),
        "非有限矩阵不能产生一个假的小盒子"
    );
    // 好坏混合：只取有效的那一个。
    let bounds = skinned_local_bounds(&unit_bind(), &[nan, translated(2.0)]).expect("bounds");
    assert!((bounds.max[0] - 2.5).abs() < 1e-5);
}

#[test]
fn the_cull_uses_the_skinned_box_not_the_bind_pose() {
    // 相机在原点朝 −Z，物体在 (0,0,-5)。bind 盒子很小且位于原点附近 —— 在这里
    // 把实体摆到画外再由骨骼把它拉回画面：只看 bind 盒子会误剔。
    let mut world = prism_ecs::World::new();
    let entity = world.spawn();
    world.insert(entity, LocalToWorld(glam::Mat4::IDENTITY));
    world.insert(entity, SkinnedMeshRenderer::default());
    world.insert(
        entity,
        crate::scene::runtime_components::MeshRef {
            asset_id: prism_scene::components::SceneAssetId::generate(),
            render_handle: prism_render::managers::MeshHandle::default(),
            generation: 1,
        },
    );
    let mut bounds = MeshBounds::default();
    bounds.insert(
        prism_render::managers::MeshHandle::default(),
        Aabb::new([-0.5, -0.5, -0.5], [0.5, 0.5, 0.5]),
    );
    world.insert_resource(bounds);

    // bind 盒子的位置远在 x = +1000（画面外）；调色板把网格拉回相机前方。
    world.insert(
        entity,
        BonePalette::from_matrices(&[glam::Mat4::from_translation(glam::vec3(1000.0, 0.0, 0.0))]),
    );

    let projection = prism_scene::components::Camera {
        fov_y_degrees: 60.0,
        near: 0.1,
        far: 100.0,
        ..Default::default()
    }
    .projection_matrix(1.0);
    let frustum = prism_render::culling::Frustum::from_view_proj(&projection.to_cols_array_2d())
        .expect("usable frustum");

    // bind pose 的盒子在 (0,0,-5)（视锥内），蒙皮后跑到 x=1000（视锥外）。
    // 用调色板剔除时必须消失；不传调色板（bind pose）时仍然出现。
    let mut items: Vec<DrawItem> = Vec::new();
    let mut palette: Vec<[[f32; 4]; 4]> = Vec::new();
    let added = append_skinned_draw_items(&world, Some(&frustum), &mut items, &mut palette);
    assert_eq!(added, 0, "蒙皮后的盒子在画面外，必须被剔除");

    // 让骨骼回到原位：同一个实体又出现在绘制列表里。
    world.insert(
        entity,
        BonePalette::from_matrices(&[glam::Mat4::from_translation(glam::vec3(0.0, 0.0, -5.0))]),
    );
    let added = append_skinned_draw_items(&world, Some(&frustum), &mut items, &mut palette);
    assert_eq!(added, 1, "骨骼把网格放回画面内就该被画");
}
