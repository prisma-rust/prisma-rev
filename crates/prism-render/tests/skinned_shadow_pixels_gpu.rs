//! Pixel-level check that skinned casts move their shadow (#560 P0-3c).
//!
//! `skinned_shadow_gpu.rs` proves the skinned draw *reaches* the shadow pass
//! (via `ShadowMapPass::last_casters`), but not that the resulting depth is the
//! **skinned** depth: a pass that bound the wrong vertex buffer would still
//! report one caster. This test closes that gap by looking at the pixels.
//!
//! Scene: a large static receiver quad on the ground plus a `ShadowsOnly`
//! skinned caster above it (invisible in the main pass, so *only* its shadow can
//! change the image). The same caster is drawn twice with a palette that moves it
//! 2.4 m along X; the darkened pixels on the receiver must move with it.
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

fn mul4(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for (col, out_col) in out.iter_mut().enumerate() {
        for (row, value) in out_col.iter_mut().enumerate() {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[k][row] * b[col][k];
            }
            *value = sum;
        }
    }
    out
}

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / len, v[1] / len, v[2] / len]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Right-handed look-at in the engine's column-major layout (`m[col][row]`).
fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> [[f32; 4]; 4] {
    let f = normalize3([target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]]);
    let s = normalize3(cross(f, up));
    let u = cross(s, f);
    [
        [s[0], u[0], -f[0], 0.0],
        [s[1], u[1], -f[1], 0.0],
        [s[2], u[2], -f[2], 0.0],
        [-dot(s, eye), -dot(u, eye), dot(f, eye), 1.0],
    ]
}

/// Camera projection: 45° vertical FOV, Vulkan depth [0,1] and y-flip.
fn perspective(near: f32, far: f32) -> [[f32; 4]; 4] {
    let inv_tan = 1.0 / (std::f32::consts::FRAC_PI_4 * 0.5).tan();
    let mut proj = [[0.0f32; 4]; 4];
    proj[0][0] = inv_tan; // aspect = 1
    proj[1][1] = -inv_tan; // Vulkan y-flip
    proj[2][2] = far / (near - far);
    proj[2][3] = -1.0;
    proj[3][2] = (far * near) / (near - far);
    proj
}

/// Orthographic light projection: x/y in [-half, half] → NDC [-1,1], depth along
/// the light view axis → [0,1]. **No y-flip**, matching `sample_shadow()` in
/// `scene_frag.slang`.
fn ortho(half: f32, near: f32, far: f32) -> [[f32; 4]; 4] {
    let mut m = [[0.0f32; 4]; 4];
    m[0][0] = 1.0 / half;
    m[1][1] = 1.0 / half;
    m[2][2] = -1.0 / (far - near);
    m[3][2] = -near / (far - near);
    m[3][3] = 1.0;
    m
}

/// Light 10 m above the origin, shining straight down.
fn light_view_proj() -> [[f32; 4]; 4] {
    let view = look_at([0.0, 10.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 1.0]);
    mul4(&ortho(4.0, 1.0, 20.0), &view)
}

fn camera_view_proj() -> [[f32; 4]; 4] {
    let view = look_at([0.0, 2.5, 6.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
    mul4(&perspective(0.1, 50.0), &view)
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

fn frame_data(shadow_strength: f32) -> FrameUBOData {
    FrameUBOData {
        view_proj: camera_view_proj(),
        camera_position: [0.0, 2.5, 6.0, 0.0],
        // `lightDirection` points *toward* the light: straight up, so the
        // receiver plane (normal +Y) is fully lit.
        light_direction: [0.0, 1.0, 0.0, 1.0],
        light_color: [1.0, 1.0, 1.0, 1.0],
        view: IDENTITY,
        light_view_proj: light_view_proj(),
        tonemap_mode: 2, // passthrough: compare raw HDR -> RGBA8 bytes
        viewport_size: [SIZE as f32, SIZE as f32],
        exposure: 1.0,
        shadow_strength,
        _pad2: [0.0; 3],
        light_view_proj1: IDENTITY,
        shadow_split: 0.0,
        shadow_cascade_count: 1,
        _pad3: [0.0; 2],
    }
}

fn scale(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    let mut m = IDENTITY;
    m[0][0] = x;
    m[1][1] = y;
    m[2][2] = z;
    m
}

/// Receiver: a 10×10 ground quad. No shadow casting (only the caster does).
fn receiver(mesh: prism_render::managers::MeshHandle) -> DrawItem {
    DrawItem {
        mesh,
        entity_id: 2,
        model: scale(10.0, 1.0, 10.0),
        material: None,
        cast_shadow: false,
        visible_in_main_pass: true,
        cast_shadow_two_sided: false,
        bone_offset: None,
        state: DrawState::default(),
        world_bounds: None,
        index_start: 0,
        index_count: None,
    }
}

/// Skinned cube that only exists for the shadow pass (`ShadowsOnly`).
fn caster(mesh: prism_render::managers::MeshHandle) -> DrawItem {
    DrawItem {
        mesh,
        entity_id: 1,
        model: IDENTITY,
        material: None,
        cast_shadow: true,
        visible_in_main_pass: false,
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
    shadow_strength: f32,
) -> Vec<u8> {
    let data = frame_data(shadow_strength);
    renderer
        .render(
            items,
            &data,
            light_view_proj(),
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
            bones,
        )
        .expect("headless render failed");
    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback failed");
    renderer.readback_pixels().expect("readback_pixels failed")
}

/// Palette matrix that lifts a bind-pose cube (centred at the origin) to
/// `(x, 0.5, 0)` — i.e. hovering just above the receiver plane.
fn placed_at(x: f32) -> [[f32; 4]; 4] {
    let mut m = IDENTITY;
    m[3][0] = x;
    m[3][1] = 0.5;
    m
}

/// Pixels on the receiver that are darker than the unshadowed baseline.
///
/// Uses the **sum of R+G+B**, not a single channel: the default material in the
/// headless renderer is a purple-ish placeholder (green ≈ 11/255), so a green
/// threshold would miss the shadow entirely while red/blue drop by ~80.
///
/// Returns `(count, mean_x_in_pixels)` so the caller can check both "there is a
/// shadow" and "the shadow moved".
fn shadowed_pixels(baseline: &[u8], image: &[u8]) -> (usize, f32) {
    let luminance = |pixel: &[u8]| pixel[0] as i32 + pixel[1] as i32 + pixel[2] as i32;
    let mut count = 0usize;
    let mut sum_x = 0.0f32;
    for (index, (lit, shaded)) in baseline
        .as_chunks::<4>()
        .0
        .iter()
        .zip(image.as_chunks::<4>().0)
        .enumerate()
    {
        if luminance(lit) - luminance(shaded) >= 8 {
            count += 1;
            sum_x += (index % SIZE) as f32;
        }
    }
    if count == 0 {
        (0, 0.0)
    } else {
        (count, sum_x / count as f32)
    }
}

#[test]
fn a_moving_skinned_caster_moves_its_shadow_on_the_receiver() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let quad = prism_render::primitives::builtin("__builtin/quad").expect("builtin quad");
    let cube = prism_render::primitives::builtin("__builtin/cube").expect("builtin cube");
    let receiver_handle = renderer.register_mesh(&quad).expect("register receiver");
    let skin = SkinAttributes {
        joints: vec![[0, 0, 0, 0]; cube.positions.len()],
        weights: vec![[1.0, 0.0, 0.0, 0.0]; cube.positions.len()],
    };
    let caster_handle = renderer
        .register_mesh_skinned(&cube, &skin)
        .expect("register skinned caster");

    let items = [receiver(receiver_handle), caster(caster_handle)];

    // 基线：关掉阴影，接收面没有暗块。
    let baseline = render(&mut renderer, &items, &[placed_at(-1.2)], 0.0);
    let (shadow_count, _) = shadowed_pixels(&baseline, &baseline);
    assert_eq!(shadow_count, 0, "the baseline must not contain a shadow");

    // 开阴影 + 把骨骼放到 x = −1.2：接收面上出现一块阴影。
    let left = render(&mut renderer, &items, &[placed_at(-1.2)], 1.0);
    let (left_count, left_center) = shadowed_pixels(&baseline, &left);
    assert!(
        left_count > 300,
        "a skinned caster must darken the receiver ({left_count} pixels changed)"
    );

    // 骨骼移到 x = +1.2：阴影必须跟着一起走，而不是留在原地。
    let right = render(&mut renderer, &items, &[placed_at(1.2)], 1.0);
    let (right_count, right_center) = shadowed_pixels(&baseline, &right);
    assert!(
        right_count > 300,
        "the moved caster must still cast ({right_count} pixels changed)"
    );
    assert!(
        left_center < SIZE as f32 / 2.0 - 10.0,
        "shadow should sit on the left half (centre x = {left_center})"
    );
    assert!(
        right_center > SIZE as f32 / 2.0 + 10.0,
        "shadow should sit on the right half (centre x = {right_center})"
    );
    assert!(
        right_center - left_center > 40.0,
        "the shadow must follow the bone: {left_center} -> {right_center}"
    );
}
