//! GPU readback test for the #502 3D LUT grade.
//!
//! Renders a flat colored frame through the real graph (ForwardPass clear →
//! PostPass LUT) and checks that:
//! - an identity LUT at strength 1 matches the ungraded frame;
//! - a red/blue-swap LUT actually swaps the channels.
//!
//! The swap test avoids assuming a particular transfer function: the LUT runs
//! in display-referred space before the hardware sRGB encode, and swapping R/B
//! commutes with the per-channel encode.

use std::path::{Path, PathBuf};

use prism_render::descriptor::FrameUBOData;
use prism_render::render_graph::PostGrade;
use prism_render::{FrameInput, GraphRenderer, RenderMode};

const SIZE: usize = 256;
const CLEAR: [f32; 4] = [0.8, 0.3, 0.1, 1.0];

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

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

/// Write a 2×2×2 `.cube` where `map(r, g, b)` produces each texel's output.
fn write_lut(name: &str, map: impl Fn(f32, f32, f32) -> [f32; 3]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "prisma-lut-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.cube"));
    let mut text = String::from("LUT_3D_SIZE 2\n");
    for b in 0..=1 {
        for g in 0..=1 {
            for r in 0..=1 {
                let rgb = map(r as f32, g as f32, b as f32);
                text.push_str(&format!("{} {} {}\n", rgb[0], rgb[1], rgb[2]));
            }
        }
    }
    std::fs::write(&path, text).unwrap();
    path
}

fn render_frame(
    renderer: &mut GraphRenderer,
    grade: PostGrade,
    lut_path: Option<&Path>,
) -> Vec<u8> {
    let data = FrameUBOData::default();
    let ctx = renderer.begin_frame().expect("begin_frame").expect("frame");
    let input = FrameInput {
        draw_items: &[],
        gizmos: &[],
        sprite_items: &[],
        sprite_lights: &[],
        particle_items: &[],
        bone_matrices: &[],
        skybox: None,
        frame_data: &data,
        cameras: &[],
        light_view_proj: IDENTITY,
        light_view_proj1: IDENTITY,
        shadow_split: 0.0,
        shadow_cascade_count: 1,
        inv_projection: IDENTITY,
        debug_flags: 0,
        wireframe: false,
        shaded_wireframe: false,
        tonemap_mode: 2, // passthrough: only the clear color + LUT matter
        aa_mode: 0,
        debug_rt: 0,
        grade,
        lut_path,
        proj22: 1.0,
        proj32: 0.0,
        lights: &[],
        render_mode: RenderMode::Raster,
        pt_max_bounces: 0,
        pt_ray_max_distance: 0.0,
        pt_max_iterations: 0,
        exposure: 1.0,
        pt_lights: &[],
        pt_accum_dirty: true,
        has_camera: false,
        camera_orthographic: false,
        clear_color: CLEAR,
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
        .expect("copy_offscreen_to_readback failed");
    renderer.readback_pixels().expect("readback_pixels failed")
}

fn center(pixels: &[u8]) -> [u8; 4] {
    let index = ((SIZE / 2) * SIZE + SIZE / 2) * 4;
    [
        pixels[index],
        pixels[index + 1],
        pixels[index + 2],
        pixels[index + 3],
    ]
}

fn channel_diff(a: [u8; 4], b: [u8; 4]) -> i32 {
    (0..3)
        .map(|i| (a[i] as i32 - b[i] as i32).abs())
        .max()
        .unwrap()
}

#[test]
fn identity_lut_at_full_strength_matches_the_ungraded_frame() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let plain = render_frame(&mut renderer, PostGrade::default(), None);
    let path = write_lut("identity", |r, g, b| [r, g, b]);
    let graded = render_frame(
        &mut renderer,
        PostGrade {
            lut_strength: 1.0,
            ..PostGrade::default()
        },
        Some(&path),
    );
    // One 8-bit step of trilinear/rounding noise is expected; anything larger
    // means the identity LUT is not being sampled at texel centres.
    assert!(
        channel_diff(center(&plain), center(&graded)) <= 1,
        "identity LUT changed the frame: {:?} -> {:?}",
        center(&plain),
        center(&graded)
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_swap_lut_swaps_the_red_and_blue_channels() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let plain = render_frame(&mut renderer, PostGrade::default(), None);
    let path = write_lut("swap", |r, g, b| [b, g, r]);
    let graded = render_frame(
        &mut renderer,
        PostGrade {
            lut_strength: 1.0,
            ..PostGrade::default()
        },
        Some(&path),
    );
    let plain = center(&plain);
    let graded = center(&graded);
    assert!(
        channel_diff(graded, [plain[2], plain[1], plain[0], plain[3]]) <= 2,
        "expected R/B swap around {plain:?}, got {graded:?}"
    );
    // And the swap must actually be visible for this non-grey clear color.
    assert!(
        channel_diff(graded, plain) > 10,
        "the swap LUT had no visible effect: {plain:?} -> {graded:?}"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
