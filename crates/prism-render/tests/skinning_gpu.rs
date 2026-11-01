//! GPU golden test for skinned draws (#560 P0-3).
//!
//! Renders the same cube twice through the real render graph:
//!
//! 1. static draw (`bone_offset = None`, no skin buffer bound),
//! 2. skinned draw (`bone_offset = Some(0)` + an identity bone palette and
//!    every vertex weighted 100 % to joint 0).
//!
//! Identity skinning must be **pixel-identical** to the static path — that is
//! the whole contract of the blended bone matrix, and it also proves the
//! second vertex buffer, the `vertexSkinnedMain` entry point and the bone SSBO
//! binding line up. A third pass pushes a translated palette through and
//! asserts the image actually moves, so a silently-ignored palette fails too.
//!
//! Skips gracefully on hosts without a Vulkan driver.

use prism_render::descriptor::FrameUBOData;
use prism_render::managers::SkinAttributes;
use prism_render::render_graph::{DrawItem, DrawState, PostGrade};
use prism_render::{GraphRenderer, RenderMode};

const SIZE: usize = 256;

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
    // projection (column-major, [col][row])
    let mut proj = [[0.0f32; 4]; 4];
    proj[0][0] = inv_tan; // aspect = 1
    proj[1][1] = -inv_tan; // Vulkan y-flip
    proj[2][2] = far / (near - far);
    proj[2][3] = -1.0;
    proj[3][2] = (far * near) / (near - far);
    // camera sits at z = +3, so view = translate(0, 0, -3)
    let mut view = IDENTITY;
    view[3][2] = -3.0;
    // view_proj = proj * view  (column-major multiply)
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
        // Overhead key light so the cube is lit even with no ECS lights.
        light_direction: [0.0, -1.0, 0.0, 1.0],
        light_color: [1.0, 1.0, 1.0, 0.6],
        view: IDENTITY,
        light_view_proj: IDENTITY,
        tonemap_mode: 2, // passthrough: compare raw HDR -> RGBA8 bytes
        viewport_size: [SIZE as f32, SIZE as f32],
        exposure: 1.0,
        shadow_strength: 0.0,
        _pad2: [0.0; 3],
    }
}

fn draw_item(mesh: prism_render::managers::MeshHandle, bone_offset: Option<u32>) -> DrawItem {
    DrawItem {
        mesh,
        entity_id: 1,
        model: IDENTITY,
        material: None,
        cast_shadow: false,
        visible_in_main_pass: true,
        cast_shadow_two_sided: false,
        bone_offset,
        state: DrawState::default(),
        world_bounds: None,
        index_start: 0,
        index_count: None,
    }
}

fn render(renderer: &mut GraphRenderer, items: &[DrawItem], bones: &[[[f32; 4]; 4]]) -> Vec<u8> {
    let data = frame_data();
    renderer
        .render(
            items,
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
            bones,
        )
        .expect("headless render failed");
    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback failed");
    renderer.readback_pixels().expect("readback_pixels failed")
}

fn non_background(pixels: &[u8]) -> usize {
    // The clear colour is a flat value; anything that differs from the corner
    // pixel is geometry.
    let corner = &pixels[0..4];
    pixels
        .chunks_exact(4)
        .filter(|pixel| *pixel != corner)
        .count()
}

#[test]
fn an_identity_bone_palette_matches_the_static_path() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let cube = prism_render::primitives::builtin("__builtin/cube").expect("builtin cube");

    let static_handle = renderer.register_mesh(&cube).expect("register static cube");
    let static_pixels = render(&mut renderer, &[draw_item(static_handle, None)], &[]);
    let drawn = non_background(&static_pixels);
    assert!(
        drawn > 1000,
        "the cube must cover some pixels, otherwise the comparison proves nothing ({drawn})"
    );

    // Every vertex fully weighted to joint 0, whose matrix is the identity.
    let skin = SkinAttributes {
        joints: vec![[0, 0, 0, 0]; cube.positions.len()],
        weights: vec![[1.0, 0.0, 0.0, 0.0]; cube.positions.len()],
    };
    let skinned_handle = renderer
        .register_mesh_skinned(&cube, &skin)
        .expect("register skinned cube");
    let skinned_pixels = render(
        &mut renderer,
        &[draw_item(skinned_handle, Some(0))],
        &[IDENTITY],
    );

    assert_eq!(
        static_pixels.len(),
        skinned_pixels.len(),
        "readback sizes must match"
    );
    let differing = static_pixels
        .iter()
        .zip(skinned_pixels.iter())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        differing, 0,
        "identity skinning must be pixel-identical to the static path ({differing} bytes differ)"
    );
}

#[test]
fn a_translated_bone_palette_moves_the_mesh() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let cube = prism_render::primitives::builtin("__builtin/cube").expect("builtin cube");
    let skin = SkinAttributes {
        joints: vec![[0, 0, 0, 0]; cube.positions.len()],
        weights: vec![[1.0, 0.0, 0.0, 0.0]; cube.positions.len()],
    };
    let handle = renderer
        .register_mesh_skinned(&cube, &skin)
        .expect("register skinned cube");

    let identity_pixels = render(&mut renderer, &[draw_item(handle, Some(0))], &[IDENTITY]);
    assert!(
        non_background(&identity_pixels) > 1000,
        "cube must be visible"
    );

    // Slide the bone 0.6 units along +X: the silhouette has to move.
    let mut moved = IDENTITY;
    moved[3][0] = 0.6;
    let moved_pixels = render(&mut renderer, &[draw_item(handle, Some(0))], &[moved]);

    let differing = identity_pixels
        .iter()
        .zip(moved_pixels.iter())
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        differing > 100,
        "a translated palette must change the image ({differing} bytes differ)"
    );
}
