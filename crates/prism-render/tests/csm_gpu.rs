//! GPU smoke test for the 2-cascade shadow atlas (#526 CSM).
//!
//! Verifies the renderer allocates a wide atlas when constructed with two
//! cascades, the shadow pass still submits casters, and the frame (including
//! the fragment-side cascade selection) renders without validation errors.
//! Skips gracefully on hosts without a Vulkan driver.

use prism_render::culling::Aabb;
use prism_render::descriptor::FrameUBOData;
use prism_render::render_graph::{DrawItem, DrawState, PostGrade};
use prism_render::shadow_map_pass::ShadowMapPass;
use prism_render::{GraphRenderer, RenderMode};

const SIZE: usize = 64;

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn try_create() -> Option<GraphRenderer> {
    match GraphRenderer::offscreen_new_with_shadow_size(SIZE as u32, SIZE as u32, None, 1024, 2) {
        Ok(renderer) => Some(renderer),
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                || message.contains("VK_ERROR_EXTENSION_NOT_PRESENT")
                || message.contains("no devices")
                || message.contains("No valid")
                || message.contains("PhysicalDevice")
            {
                eprintln!("[SKIP] no Vulkan driver: {error:#}");
                return None;
            }
            panic!("offscreen_new_with_shadow_size failed: {error:#}");
        }
    }
}

fn frame_data() -> FrameUBOData {
    FrameUBOData {
        view_proj: IDENTITY,
        camera_position: [0.0, 0.0, 3.0, 0.0],
        light_direction: [0.0, -1.0, 0.0, 1.0],
        light_color: [1.0, 1.0, 1.0, 1.0],
        view: IDENTITY,
        light_view_proj: IDENTITY,
        light_view_proj1: IDENTITY,
        shadow_split: 30.0,
        shadow_cascade_count: 2,
        tonemap_mode: 2,
        viewport_size: [SIZE as f32, SIZE as f32],
        exposure: 1.0,
        shadow_strength: 1.0,
        _pad2: [0.0; 3],
        _pad3: [0.0; 2],
        prev_view_proj: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
    }
}

fn caster(mesh: prism_render::managers::MeshHandle) -> DrawItem {
    DrawItem {
        mesh,
        entity_id: 1,
        model: IDENTITY,
        material: None,
        cast_shadow: true,
        visible_in_main_pass: false,
        cast_shadow_two_sided: false,
        bone_offset: None,
        splat_slots: [u32::MAX; 4],
        splat_tiling: 0.0,
        state: DrawState::default(),
        world_bounds: None,
        index_start: 0,
        index_count: None,
    }
}

#[test]
fn two_cascades_allocate_an_atlas_and_render() {
    let Some(mut renderer) = try_create() else {
        return;
    };

    let extent = renderer
        .graph()
        .pass_ref::<ShadowMapPass>()
        .map(ShadowMapPass::shadow_extent)
        .expect("shadow pass is registered");
    assert_eq!(extent.width, 2048, "two 1024² tiles side by side");
    assert_eq!(extent.height, 1024);

    let cube = prism_render::primitives::builtin("__builtin/cube").expect("builtin cube");
    let mesh = renderer.register_mesh(&cube).expect("register cube");
    let data = frame_data();
    renderer
        .render(
            &[caster(mesh)],
            &data,
            IDENTITY,
            IDENTITY,
            0,
            2,
            0,
            0,
            1.0,
            0.0,
            &[],
            RenderMode::Raster,
            0,
            0.0,
            0,
            1.0,
            &[],
            PostGrade::default(),
            false,
            false,
            &[],
        )
        .expect("2-cascade render failed");

    let casters = renderer
        .graph()
        .pass_ref::<ShadowMapPass>()
        .map(ShadowMapPass::last_casters)
        .expect("shadow pass is registered");
    assert_eq!(
        casters,
        (1, 0),
        "the caster must be submitted to the cascade pass"
    );

    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback failed");
    let pixels = renderer.readback_pixels().expect("readback_pixels failed");
    assert_eq!(pixels.len(), SIZE * SIZE * 4);
}

/// 与源码单测相同的单位盒正交投影：世界 `[-1, 1]^3` → Vulkan 裁剪空间。
fn unit_box_ortho() -> [[f32; 4]; 4] {
    [
        [0.5, 0.0, 0.0, 0.0],
        [0.0, 0.5, 0.0, 0.0],
        [0.0, 0.0, 0.5, 0.0],
        [0.0, 0.0, 0.5, 1.0],
    ]
}

/// #491：`world_bounds` 落在阴影覆盖范围之外的 caster 不会进阴影通道。
///
/// 两个 caster 用同一个立方体、同一个模型矩阵，唯一区别是 `world_bounds`：
/// 如果剔除没生效，两者都会被提交（`last_casters == (2, 0)`）。
#[test]
fn casters_outside_the_light_frustum_are_not_submitted() {
    let Some(mut renderer) = try_create() else {
        return;
    };

    let cube = prism_render::primitives::builtin("__builtin/cube").expect("builtin cube");
    let mesh = renderer.register_mesh(&cube).expect("register cube");

    let mut inside = caster(mesh);
    inside.entity_id = 1;
    inside.world_bounds = Some(Aabb::new([-0.5, -0.5, 0.1], [0.5, 0.5, 0.9]));

    let mut outside = caster(mesh);
    outside.entity_id = 2;
    // 完全在单位盒之外；光源矩阵退化时它仍会被提交，所以这条断言能区分
    // 「剔除生效」和「矩阵没法建视锥」。
    outside.world_bounds = Some(Aabb::new([10.0, -0.5, 0.1], [11.0, 0.5, 0.9]));

    let mut data = frame_data();
    data.light_view_proj = unit_box_ortho();
    data.light_view_proj1 = unit_box_ortho();
    data.shadow_cascade_count = 1;

    renderer
        .render(
            &[inside, outside],
            &data,
            IDENTITY,
            IDENTITY,
            0,
            2,
            0,
            0,
            1.0,
            0.0,
            &[],
            RenderMode::Raster,
            0,
            0.0,
            0,
            1.0,
            &[],
            PostGrade::default(),
            false,
            false,
            &[],
        )
        .expect("shadow culling render failed");

    let shadow_pass = renderer
        .graph()
        .pass_ref::<ShadowMapPass>()
        .expect("shadow pass is registered");
    assert_eq!(
        shadow_pass.last_casters(),
        (1, 0),
        "only the caster inside the light frustum should be submitted"
    );
    assert_eq!(
        shadow_pass.last_culled_casters(),
        1,
        "the caster outside the light frustum should be counted as culled"
    );
}

/// #491：两个相同 mesh / 剔除模式的静态 caster 合并成一次实例化 draw。
///
/// `last_casters` 仍是逻辑 caster 数（2），`last_shadow_draw_calls` 必须是 1；
/// 如果实例化没有生效，这里会是 2。
#[test]
fn identical_static_casters_share_one_shadow_draw_call() {
    let Some(mut renderer) = try_create() else {
        return;
    };

    let cube = prism_render::primitives::builtin("__builtin/cube").expect("builtin cube");
    let mesh = renderer.register_mesh(&cube).expect("register cube");

    let mut left = caster(mesh);
    left.entity_id = 1;
    left.model[3] = [-0.5, 0.0, 0.0, 1.0];
    let mut right = caster(mesh);
    right.entity_id = 2;
    right.model[3] = [0.5, 0.0, 0.0, 1.0];

    let mut data = frame_data();
    data.light_view_proj = IDENTITY;
    data.light_view_proj1 = IDENTITY;
    data.shadow_cascade_count = 1;

    renderer
        .render(
            &[left, right],
            &data,
            IDENTITY,
            IDENTITY,
            0,
            2,
            0,
            0,
            1.0,
            0.0,
            &[],
            RenderMode::Raster,
            0,
            0.0,
            0,
            1.0,
            &[],
            PostGrade::default(),
            false,
            false,
            &[],
        )
        .expect("shadow instancing render failed");

    let shadow_pass = renderer
        .graph()
        .pass_ref::<ShadowMapPass>()
        .expect("shadow pass is registered");
    assert_eq!(
        shadow_pass.last_casters(),
        (2, 0),
        "both static casters are logically submitted"
    );
    assert_eq!(
        shadow_pass.last_shadow_draw_calls(),
        1,
        "identical casters should share one instanced draw"
    );
}
