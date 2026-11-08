//! 遮挡剔除的 GPU 端到端验证（#1007）。
//!
//! 用上一帧深度把完全被挡住的盒子剔出主通道，同时保持画面不变。
//! 没有 Vulkan 设备时按现有 GPU 测试的约定跳过。

use prism_render::culling::Aabb;
use prism_render::render_graph::{DrawItem, DrawState, PostGrade, RenderMode};
use prism_render::FrameInput;
use prism_render::{FrameUBOData, GraphRenderer};

const SIZE: usize = 128;

fn try_create() -> Option<GraphRenderer> {
    match GraphRenderer::offscreen_new(SIZE as u32, SIZE as u32, None) {
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
            panic!("offscreen_new failed: {error:#}");
        }
    }
}

/// 相机在原点、看向 −Z 的 Vulkan 透视矩阵（near 0.1 / far 100）。
fn perspective() -> [[f32; 4]; 4] {
    let fov = 45f32.to_radians();
    let (near, far) = (0.1f32, 100.0f32);
    let f = 1.0 / (fov * 0.5).tan();
    let mut m = [[0.0f32; 4]; 4];
    m[0][0] = f;
    m[1][1] = -f;
    m[2][2] = far / (near - far);
    m[2][3] = -1.0;
    m[3][2] = (near * far) / (near - far);
    m
}

fn translation_scale(x: f32, y: f32, z: f32, scale: f32) -> [[f32; 4]; 4] {
    let mut m = [[0.0f32; 4]; 4];
    m[0][0] = scale;
    m[1][1] = scale;
    m[2][2] = scale;
    m[3] = [x, y, z, 1.0];
    m
}

fn draw_item(
    mesh: prism_render::managers::MeshHandle,
    model: [[f32; 4]; 4],
    entity_id: u32,
) -> DrawItem {
    // 盒子网格是单位立方体，模型矩阵只做平移 + 均匀缩放。
    let scale = model[0][0].abs() * 0.5;
    let x = model[3][0];
    let y = model[3][1];
    let z = model[3][2];
    DrawItem {
        mesh,
        entity_id,
        model,
        material: None,
        cast_shadow: false,
        visible_in_main_pass: true,
        cast_shadow_two_sided: false,
        state: DrawState::default(),
        world_bounds: Some(Aabb::new(
            [x - scale, y - scale, z - scale],
            [x + scale, y + scale, z + scale],
        )),
        index_start: 0,
        index_count: None,
        bone_offset: None,
        splat_slots: [u32::MAX; 4],
        splat_tiling: 0.0,
    }
}

fn frame_data(view_proj: [[f32; 4]; 4]) -> FrameUBOData {
    FrameUBOData {
        view_proj,
        camera_position: [0.0, 0.0, 0.0, 0.0],
        light_direction: [0.3, 0.8, 0.5, 0.8],
        light_color: [1.0, 1.0, 1.0, 1.0],
        view: view_proj,
        light_view_proj: view_proj,
        light_view_proj1: view_proj,
        shadow_split: 0.0,
        shadow_cascade_count: 1,
        tonemap_mode: 2,
        viewport_size: [SIZE as f32, SIZE as f32],
        exposure: 1.0,
        shadow_strength: 0.0,
        _pad2: [0.0; 3],
        _pad3: [0.0; 2],
        prev_view_proj: view_proj,
    }
}

/// 渲染一帧并返回中心像素；`cull` 为真时在 `begin_frame` 之后应用遮挡剔除。
fn render(renderer: &mut GraphRenderer, items: &mut Vec<DrawItem>, cull: bool) -> ([u8; 4], usize) {
    let view_proj = perspective();
    let ctx = renderer.begin_frame().expect("begin_frame").expect("frame");
    let culled = if cull {
        renderer.cull_occluded_draws(items).culled
    } else {
        0
    };
    let data = frame_data(view_proj);
    let input = FrameInput {
        draw_items: items,
        gizmos: &[],
        sprite_items: &[],
        sprite_lights: &[],
        particle_items: &[],
        bone_matrices: &[],
        skybox: None,
        frame_data: &data,
        cameras: &[],
        light_view_proj: view_proj,
        light_view_proj1: view_proj,
        shadow_split: 0.0,
        shadow_cascade_count: 1,
        inv_projection: view_proj,
        debug_flags: 0,
        wireframe: false,
        shaded_wireframe: false,
        tonemap_mode: 2,
        aa_mode: 0,
        debug_rt: 0,
        grade: PostGrade::default(),
        lut_path: None,
        proj22: view_proj[2][2],
        proj32: view_proj[3][2],
        lights: &[],
        render_mode: RenderMode::Raster,
        pt_max_bounces: 0,
        pt_ray_max_distance: 0.0,
        pt_max_iterations: 0,
        exposure: 1.0,
        pt_lights: &[],
        pt_accum_dirty: true,
        has_camera: true,
        camera_orthographic: false,
        clear_color: [0.1, 0.1, 0.1, 1.0],
        shadows_enabled: false,
        ui_overlay: None,
        show_grid: false,
    };
    renderer.prepare(&ctx, &input).expect("prepare");
    renderer.execute(&ctx, &input).expect("execute");
    let out_of_date = renderer.present(&ctx).expect("present");
    renderer.end_frame(&ctx, out_of_date).expect("end_frame");

    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback");
    let pixels = renderer.readback_pixels().expect("readback_pixels");
    let center = (SIZE / 2 * SIZE + SIZE / 2) * 4;
    (
        [
            pixels[center],
            pixels[center + 1],
            pixels[center + 2],
            pixels[center + 3],
        ],
        culled,
    )
}

#[test]
fn a_fully_covered_box_is_culled_without_changing_the_image() {
    let Some(mut renderer) = try_create() else {
        return;
    };
    renderer.set_occlusion_culling(true);
    let box_mesh = renderer
        .register_mesh(&prism_render::primitives::builtin("box").expect("builtin box"))
        .expect("register box");

    // 4 单位大的盒子在 z = −5，几乎盖满画面；另一个盒子在 z = −30 完全躲在后面。
    let mut items = vec![
        draw_item(box_mesh, translation_scale(0.0, 0.0, -5.0, 4.0), 1),
        draw_item(box_mesh, translation_scale(0.0, 0.0, -30.0, 1.0), 2),
    ];

    // 第一帧没有上一帧深度：不剔任何东西。
    let (first, culled) = render(&mut renderer, &mut items, true);
    assert_eq!(culled, 0, "the first frame has no depth to test against");

    // 第二帧：近处盒子写下的深度完全覆盖远处盒子。
    let (second, culled) = render(&mut renderer, &mut items, true);
    assert_eq!(culled, 1, "the hidden box must be culled");
    assert_eq!(first, second, "culling must not change the visible image");
    assert!(renderer.occlusion_counts().culled >= 1);
}

#[test]
fn occlusion_culling_off_keeps_every_draw_visible() {
    let Some(mut renderer) = try_create() else {
        return;
    };
    let box_mesh = renderer
        .register_mesh(&prism_render::primitives::builtin("box").expect("builtin box"))
        .expect("register box");
    let mut items = vec![draw_item(
        box_mesh,
        translation_scale(0.0, 0.0, -30.0, 1.0),
        1,
    )];
    for _ in 0..2 {
        let (_, culled) = render(&mut renderer, &mut items, false);
        assert_eq!(culled, 0);
        assert!(items[0].visible_in_main_pass);
    }
}
