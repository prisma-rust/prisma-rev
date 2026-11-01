//! GPU test for the selection outline pass (#487).
//!
//! Renders one cube with an ECS entity id matching `set_outline_entities` and
//! checks that orange outline pixels appear around the silhouette. Also
//! renders the same frame with an empty selection and verifies the orange
//! pixels disappear, so the test catches a pass that simply paints everything.

use prism_render::{DrawItem, DrawState, FrameUBOData, GraphRenderer, PostGrade, RenderMode};

fn try_create_headless() -> Option<GraphRenderer> {
    match GraphRenderer::headless_new(None) {
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
            panic!("headless_new failed: {error:#}");
        }
    }
}

fn mul(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            out[column][row] = (0..4).map(|k| a[k][row] * b[column][k]).sum();
        }
    }
    out
}

fn camera() -> ([[f32; 4]; 4], [[f32; 4]; 4]) {
    // Camera at (0, 0, 3) looking down -Z at a unit cube centred on origin.
    let view = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, -3.0, 1.0],
    ];
    let near = 0.1_f32;
    let far = 100.0_f32;
    let f = 1.0 / (45.0_f32.to_radians() * 0.5).tan();
    let aspect = 1.0_f32;
    let mut proj = [[0.0f32; 4]; 4];
    proj[0][0] = f / aspect;
    proj[1][1] = -f; // Vulkan NDC +Y points down.
    proj[2][2] = far / (near - far);
    proj[2][3] = -1.0;
    proj[3][2] = near * far / (near - far);
    (mul(&proj, &view), view)
}

fn frame_data(view_proj: [[f32; 4]; 4], view: [[f32; 4]; 4]) -> FrameUBOData {
    FrameUBOData {
        view_proj,
        camera_position: [0.0, 0.0, 3.0, 0.0],
        light_direction: [0.3, 0.8, 0.5, 0.8],
        light_color: [1.0, 0.98, 0.95, 0.25],
        view,
        light_view_proj: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        tonemap_mode: 1,
        viewport_size: [256.0, 256.0],
        exposure: 1.0,
        shadow_strength: 0.0,
        _pad2: [0.0; 3],
    }
}

fn draw_item(mesh: prism_render::managers::MeshHandle) -> DrawItem {
    DrawItem {
        mesh,
        entity_id: 7,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        material: None,
        cast_shadow: false,
        visible_in_main_pass: true,
        cast_shadow_two_sided: false,
        state: DrawState::default(),
        world_bounds: None,
        index_start: 0,
        index_count: None,
    }
}

fn render(renderer: &mut GraphRenderer, item: &DrawItem) -> Vec<u8> {
    render_with(renderer, item, false, false)
}

fn render_with(
    renderer: &mut GraphRenderer,
    item: &DrawItem,
    wireframe: bool,
    shaded_wireframe: bool,
) -> Vec<u8> {
    let (view_proj, view) = camera();
    let frame_data = frame_data(view_proj, view);
    renderer
        .render(
            std::slice::from_ref(item),
            &frame_data,
            frame_data.light_view_proj,
            [[0.0; 4]; 4],
            0,
            1,
            0,
            0,
            view_proj[2][2],
            view_proj[3][2],
            &[],
            RenderMode::Raster,
            0,
            0.0,
            0,
            1.0,
            &[],
            PostGrade::default(),
            wireframe,
            shaded_wireframe,
        )
        .expect("headless render failed");
    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback failed");
    renderer.readback_pixels().expect("readback_pixels failed")
}

fn orange_pixels(pixels: &[u8]) -> usize {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| {
            let (r, g, b) = (pixel[0] as f32, pixel[1] as f32, pixel[2] as f32);
            r > 120.0 && r > g * 1.6 && g > b * 1.5
        })
        .count()
}

fn dark_pixels(pixels: &[u8]) -> usize {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] < 40 && pixel[1] < 40 && pixel[2] < 40)
        .count()
}

#[test]
fn shaded_wireframe_keeps_the_fill_and_adds_dark_edges() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    if !renderer.context().wireframe_supported {
        eprintln!("[SKIP] fillModeNonSolid not supported by this driver");
        return;
    }
    let mesh = renderer
        .register_mesh(&prism_render::primitives::builtin("box").expect("box mesh"))
        .expect("register box");
    let item = draw_item(mesh);

    let plain = render_with(&mut renderer, &item, false, false);
    let shaded = render_with(&mut renderer, &item, false, true);

    let plain_dark = dark_pixels(&plain);
    let shaded_dark = dark_pixels(&shaded);
    assert!(
        shaded_dark > plain_dark + 50,
        "expected the fill to stay and dark wire edges to be added: plain={plain_dark}, shaded={shaded_dark}"
    );
    // 填充仍然在：线框不是唯一内容。
    assert!(
        shaded_dark < 256 * 256,
        "shaded wireframe must not replace the filled image"
    );
}

#[test]
fn selection_outline_draws_only_for_the_selected_entity() {
    let Some(mut renderer) = try_create_headless() else {
        return;
    };
    let mesh = renderer
        .register_mesh(&prism_render::primitives::builtin("box").expect("box mesh"))
        .expect("register box");
    let item = draw_item(mesh);

    renderer.set_outline_entities(&[]);
    let without = render(&mut renderer, &item);

    renderer.set_outline_entities(&[99]);
    let unrelated = render(&mut renderer, &item);

    renderer.set_outline_entities(&[7]);
    let with = render(&mut renderer, &item);

    let without_count = orange_pixels(&without);
    let unrelated_count = orange_pixels(&unrelated);
    let with_count = orange_pixels(&with);
    assert!(
        with_count > without_count + 200,
        "expected an orange outline: without={without_count}, with={with_count}"
    );
    assert!(
        with_count > unrelated_count + 200,
        "only the matching entity id should receive an outline: unrelated={unrelated_count}, with={with_count}"
    );
    assert!(
        with_count < 256 * 256 / 2,
        "outline should be a ring, not a filled object: {with_count} orange pixels"
    );
}
