//! GPU test for the skin-weight heatmap (#560).
//!
//! The heatmap is a debug view (`debug_flags` bit 15): every vertex is coloured
//! by its dominant joint instead of being lit, which is the only practical way
//! to tell whether an imported glTF actually weighted the mesh to the right
//! bones. The interesting failure mode is silent: the flag arrives at the push
//! constants but the vertex stage never writes `skinHeat`, so the view looks
//! like a flat grey mesh and nobody notices.
//!
//! This test draws one cube whose vertices alternate between two joints with
//! identical identity palettes (so the geometry and the lighting are the same
//! either way) and asserts the pixels disagree in hue only when the flag is on.
//!
//! Skips gracefully on hosts without a Vulkan driver.

use prism_render::descriptor::FrameUBOData;
use prism_render::managers::SkinAttributes;
use prism_render::render_graph::{DrawItem, DrawState, PostGrade};
use prism_render::{GraphRenderer, RenderMode};

const SIZE: usize = 256;
/// `scene_frag.slang`: `PBR_FLAG_SKIN_WEIGHTS`.
const SKIN_WEIGHTS_FLAG: u32 = 1 << 15;

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// Camera at +Z looking down −Z, 45° vertical FOV, Vulkan depth range + y-flip.
fn view_proj() -> [[f32; 4]; 4] {
    let fov = std::f32::consts::FRAC_PI_4;
    let (near, far) = (0.1f32, 10.0f32);
    let inv_tan = 1.0 / (fov * 0.5).tan();
    let mut proj = [[0.0f32; 4]; 4];
    proj[0][0] = inv_tan; // aspect = 1
    proj[1][1] = -inv_tan; // Vulkan y-flip
    proj[2][2] = far / (near - far);
    proj[2][3] = -1.0;
    proj[3][2] = (far * near) / (near - far);
    let mut view = IDENTITY;
    view[3][2] = -3.0;
    let mut m = [[0.0f32; 4]; 4];
    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += proj[k][row] * view[col][k];
            }
            m[col][row] = sum;
        }
    }
    m
}

fn try_create_headless() -> Option<GraphRenderer> {
    match GraphRenderer::headless_new(None) {
        Ok(renderer) => Some(renderer),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                || msg.contains("VK_ERROR_EXTENSION_NOT_PRESENT")
                || msg.contains("no devices")
                || msg.contains("No valid")
                || msg.contains("PhysicalDevice")
            {
                eprintln!("[SKIP] no Vulkan driver: {e:#}");
                return None;
            }
            panic!("headless_new failed: {e:#}");
        }
    }
}

fn frame_data() -> FrameUBOData {
    FrameUBOData {
        view_proj: view_proj(),
        camera_position: [0.0, 0.0, 3.0, 0.0],
        light_direction: [0.0, -1.0, 0.0, 1.0],
        light_color: [1.0, 1.0, 1.0, 0.6],
        view: IDENTITY,
        light_view_proj: IDENTITY,
        tonemap_mode: 2, // passthrough: raw HDR -> RGBA8 bytes
        viewport_size: [SIZE as f32, SIZE as f32],
        exposure: 1.0,
        shadow_strength: 0.0,
        _pad2: [0.0; 3],
    }
}

fn draw_item(mesh: prism_render::managers::MeshHandle) -> DrawItem {
    DrawItem {
        mesh,
        entity_id: 1,
        model: IDENTITY,
        material: None,
        cast_shadow: false,
        visible_in_main_pass: true,
        cast_shadow_two_sided: false,
        bone_offset: Some(0),
        state: DrawState::default(),
        world_bounds: None,
        index_start: 0,
        index_count: None,
    }
}

fn render(
    renderer: &mut GraphRenderer,
    items: &[DrawItem],
    bones: &[[[f32; 4]; 4]],
    debug_flags: u32,
) -> Vec<u8> {
    let data = frame_data();
    renderer
        .render(
            items,
            &data,
            IDENTITY,
            IDENTITY,
            debug_flags,
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
            false, // wireframe
            false, // shaded wireframe
            bones,
        )
        .expect("headless render failed");
    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback failed");
    renderer.readback_pixels().expect("readback_pixels failed")
}

/// How many pixels are clearly tinted towards `channel` (0=R, 1=G, 2=B): the
/// channel leads by at least 40/255 and the colour is not near-grey.
fn tinted(pixels: &[u8], channel: usize) -> usize {
    let corner = &pixels[0..4];
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| *pixel != corner)
        .filter(|pixel| {
            let (r, g, b) = (pixel[0] as i32, pixel[1] as i32, pixel[2] as i32);
            let lead = match channel {
                0 => r - g.max(b),
                1 => g - r.max(b),
                _ => b - r.max(g),
            };
            lead >= 40
        })
        .count()
}

#[test]
fn the_heatmap_colours_vertices_by_their_dominant_joint() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let cube = prism_render::primitives::builtin("__builtin/cube").expect("builtin cube");
    // 一半顶点挂 joint 0、一半挂 joint 1，权重都是 1 —— 两个 joint 的矩阵都是单位，
    // 所以几何完全一样，任何颜色差异都只能来自热图着色。
    let count = cube.positions.len();
    let sk = SkinAttributes {
        joints: (0..count)
            .map(|index| [u32::from(index % 2 == 0) as u16, 0, 0, 0])
            .collect(),
        weights: vec![[1.0, 0.0, 0.0, 0.0]; count],
    };
    let handle = renderer
        .register_mesh_skinned(&cube, &sk)
        .expect("register skinned cube");

    // 正常着色：默认材质是中性色，不该出现明显色偏。
    let shaded = render(
        &mut renderer,
        &[draw_item(handle)],
        &[IDENTITY, IDENTITY],
        0,
    );
    let shaded_tinted: usize = (0..3).map(|channel| tinted(&shaded, channel)).sum();
    assert!(
        shaded_tinted < 100,
        "the lit view must not already look like a heatmap ({shaded_tinted} tinted pixels)"
    );

    // 热图：hue = joint × 金色角。joint 0 → 红 (1, .15, .15)，
    // joint 1 (0.618 圈) → 蓝 (.15, .4, 1)。
    let heat = render(
        &mut renderer,
        &[draw_item(handle)],
        &[IDENTITY, IDENTITY],
        SKIN_WEIGHTS_FLAG,
    );
    let red = tinted(&heat, 0);
    let blue = tinted(&heat, 2);
    assert!(
        red >= 100,
        "joint 0 should tint part of the mesh red (got {red} pixels)"
    );
    assert!(
        blue >= 100,
        "joint 1 should tint part of the mesh blue (got {blue} pixels)"
    );
    assert_ne!(
        shaded, heat,
        "the flag must change the image; otherwise the varying never arrived"
    );
}

#[test]
fn an_unweighted_vertex_stays_grey_in_the_heatmap() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let cube = prism_render::primitives::builtin("__builtin/cube").expect("builtin cube");
    // 权重全 0 = 坏导入 / 静态数据：不能把它画成某根骨骼的颜色。
    let sk = SkinAttributes {
        joints: vec![[0, 0, 0, 0]; cube.positions.len()],
        weights: vec![[0.0; 4]; cube.positions.len()],
    };
    let handle = renderer
        .register_mesh_skinned(&cube, &sk)
        .expect("register skinned cube");

    let heat = render(
        &mut renderer,
        &[draw_item(handle)],
        &[IDENTITY, IDENTITY],
        SKIN_WEIGHTS_FLAG,
    );
    let tinted_total: usize = (0..3).map(|channel| tinted(&heat, channel)).sum();
    assert!(
        tinted_total < 100,
        "zero weights must stay neutral grey ({tinted_total} tinted pixels)"
    );
}
