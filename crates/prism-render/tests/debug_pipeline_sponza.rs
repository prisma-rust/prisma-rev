//! Offline multi-pass debug suite for Sponza rendering pipeline.
//!
//! Executes pass-by-pass and outputs intermediate images to isolate rendering failures:
//! - Step 1: Baseline Clear
//! - Step 2: Calibration Quad (ForwardPass pipeline verification)
//! - Step 3: Real Crytek Sponza Forward Raster (Opaque)
//! - Step 4: Sponza Forward Raster Wireframe
//! - Step 5: Sponza Forward Depth Buffer (`debug_rt = 1`)
//! - Step 6: Sponza Forward View-Space Normals (`debug_rt = 2`)
//! - Step 7: Sponza Path Tracing Direct Lighting (1 Bounce)
//! - Step 8: Sponza Path Tracing Multi-Bounce + RTXGI SHARC

mod golden_support;

use golden_support::format_version;
use prism_render::managers::{MaterialUploadInput, MeshUploadInput};
use prism_render::render_graph::{GiMode, PostGrade, RenderMode};
use prism_render::{DrawItem, DrawState, FrameUBOData, GraphRenderer};
use std::fs;
use std::path::{Path, PathBuf};

const WIDTH: u32 = 960;
const HEIGHT: u32 = 540;

fn save_rgba_png(path: &Path, pixels: &[u8], width: u32, height: u32) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    image::save_buffer_with_format(
        path,
        pixels,
        width,
        height,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )?;
    Ok(())
}

fn mul_mat(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            out[column][row] = (0..4).map(|k| a[k][row] * b[column][k]).sum();
        }
    }
    out
}

fn camera_view_proj(
    eye: [f32; 3],
    look_dir: [f32; 3],
    up: [f32; 3],
    aspect: f32,
) -> ([[f32; 4]; 4], [[f32; 4]; 4]) {
    let fwd = {
        let len =
            (look_dir[0] * look_dir[0] + look_dir[1] * look_dir[1] + look_dir[2] * look_dir[2])
                .sqrt();
        [look_dir[0] / len, look_dir[1] / len, look_dir[2] / len]
    };
    // right = fwd x up
    let right = {
        let rx = fwd[1] * up[2] - fwd[2] * up[1];
        let ry = fwd[2] * up[0] - fwd[0] * up[2];
        let rz = fwd[0] * up[1] - fwd[1] * up[0];
        let len = (rx * rx + ry * ry + rz * rz).sqrt();
        [rx / len, ry / len, rz / len]
    };
    // true_up = right x fwd
    let true_up = [
        right[1] * fwd[2] - right[2] * fwd[1],
        right[2] * fwd[0] - right[0] * fwd[2],
        right[0] * fwd[1] - right[1] * fwd[0],
    ];

    // In engine convention, camera looks down -Z:
    // So camera Z axis = -fwd
    let view = [
        [right[0], true_up[0], -fwd[0], 0.0],
        [right[1], true_up[1], -fwd[1], 0.0],
        [right[2], true_up[2], -fwd[2], 0.0],
        [
            -(right[0] * eye[0] + right[1] * eye[1] + right[2] * eye[2]),
            -(true_up[0] * eye[0] + true_up[1] * eye[1] + true_up[2] * eye[2]),
            -(-fwd[0] * eye[0] - fwd[1] * eye[1] - fwd[2] * eye[2]),
            1.0,
        ],
    ];

    let near = 0.05_f32;
    let far = 1000.0_f32;
    let fov_y = 65.0_f32.to_radians();
    let f = 1.0 / (fov_y * 0.5).tan();

    let mut proj = [[0.0f32; 4]; 4];
    proj[0][0] = f / aspect;
    proj[1][1] = -f; // Vulkan NDC +Y down
    proj[2][2] = far / (near - far);
    proj[2][3] = -1.0;
    proj[3][2] = near * far / (near - far);

    (mul_mat(&proj, &view), view)
}

fn calibration_quad() -> MeshUploadInput {
    // Quad at X = -5.0, Y in [0, 4], Z in [-2, 2], facing toward -X (normal = [-1, 0, 0])
    let p0 = [-5.0, 0.0, -2.0];
    let p1 = [-5.0, 4.0, -2.0];
    let p2 = [-5.0, 4.0, 2.0];
    let p3 = [-5.0, 0.0, 2.0];
    let normal = [-1.0, 0.0, 0.0];

    MeshUploadInput {
        positions: vec![p0, p1, p2, p3],
        normals: vec![normal; 4],
        uvs: vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
        indices: vec![0, 1, 2, 0, 2, 3],
        tangents: vec![[0.0, 0.0, 1.0, 1.0]; 4],
        colors: vec![[1.0, 1.0, 1.0]; 4],
    }
}

fn load_sponza_primitives(
    path: &Path,
) -> anyhow::Result<Vec<(MeshUploadInput, MaterialUploadInput)>> {
    // 贴图资源没有全部入库，而这里只用几何与材质因子（贴图槽位留空），
    // 所以绕开 `gltf::import` 的图片解码，直接读文档 + buffer。
    let bytes = std::fs::read(path)?;
    let gltf = gltf::Gltf::from_slice(&bytes)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut buffers = Vec::new();
    for buffer in gltf.document.buffers() {
        match buffer.source() {
            gltf::buffer::Source::Uri(uri) => buffers.push(std::fs::read(base.join(uri))?),
            gltf::buffer::Source::Bin => {
                anyhow::bail!("GLB 内嵌 buffer 未支持：{}", path.display())
            }
        }
    }
    let doc = gltf.document;
    let mut out = Vec::new();

    for mesh in doc.meshes() {
        for prim in mesh.primitives() {
            let reader = prim.reader(|buffer| Some(&buffers[buffer.index()]));
            let positions: Vec<[f32; 3]> = match reader.read_positions() {
                Some(p) => p.collect(),
                None => continue,
            };
            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(|n| n.collect())
                .unwrap_or_else(|| vec![[0.0, 1.0, 0.0]; positions.len()]);
            let uvs: Vec<[f32; 2]> = reader
                .read_tex_coords(0)
                .map(|tc| tc.into_f32().collect())
                .unwrap_or_else(|| vec![[0.0, 0.0]; positions.len()]);
            let indices: Vec<u32> = match reader.read_indices() {
                Some(idx) => idx.into_u32().collect(),
                None => (0..positions.len() as u32).collect(),
            };
            let tangents: Vec<[f32; 4]> = reader
                .read_tangents()
                .map(|t| t.collect())
                .unwrap_or_else(|| vec![[1.0, 0.0, 0.0, 1.0]; positions.len()]);
            let colors: Vec<[f32; 3]> = reader
                .read_colors(0)
                .map(|c| c.into_rgb_f32().collect())
                .unwrap_or_else(|| vec![[1.0, 1.0, 1.0]; positions.len()]);

            let mat = prim.material();
            let pbr = mat.pbr_metallic_roughness();
            let base_color = pbr.base_color_factor();
            let roughness = pbr.roughness_factor();
            let metallic = pbr.metallic_factor();

            let mesh_input = MeshUploadInput {
                positions,
                normals,
                uvs,
                indices,
                tangents,
                colors,
            };

            let mat_input = MaterialUploadInput {
                base_color,
                metallic,
                roughness,
                emissive: mat.emissive_factor(),
                albedo_tex: None,
                normal_tex: None,
                metallic_roughness_tex: None,
                emissive_tex: None,
                occlusion_tex: None,
                normal_scale: 1.0,
                occlusion_strength: 1.0,
                transmission: 0.0,
                ior: 1.5,
                translucency: 0.0,
                anisotropy: 0.0,
                clearcoat: 0.0,
                clearcoat_roughness: 0.0,
                emissive_strength: 1.0,
            };

            out.push((mesh_input, mat_input));
        }
    }

    Ok(out)
}

#[test]
fn test_sponza_pipeline_debug_passes() {
    let out_dir = PathBuf::from("target/debug_passes");
    fs::create_dir_all(&out_dir).expect("create debug_passes dir");

    let mut renderer =
        GraphRenderer::offscreen_new(WIDTH, HEIGHT, None).expect("create offscreen GraphRenderer");

    let adapter = {
        let properties = &renderer.context().physical_device_properties;
        let name = properties
            .device_name_as_c_str()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<invalid device name>".to_string());
        format!(
            "{name} | Vulkan API {} | driver {}",
            format_version(properties.api_version),
            format_version(properties.driver_version)
        )
    };
    println!("\n=== Vulkan Offscreen Debug Initialized ===");
    println!("Device: {}\n", adapter);

    let aspect = WIDTH as f32 / HEIGHT as f32;
    // Camera at [-10.0, 2.0, 0.0] looking along +X towards Sponza colonnade
    let eye = [-10.0, 2.0, 0.0];
    let (view_proj, view) = camera_view_proj(eye, [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], aspect);

    let frame_data = FrameUBOData {
        view_proj,
        camera_position: [eye[0], eye[1], eye[2], 0.0],
        light_direction: [0.2, 0.95, 0.0, 3.2],
        light_color: [1.0, 0.96, 0.90, 0.1],
        view,
        light_view_proj: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        tonemap_mode: 0, // Reinhard
        viewport_size: [WIDTH as f32, HEIGHT as f32],
        exposure: 1.0,
        shadow_strength: 0.8,
        ..Default::default()
    };

    // -----------------------------------------------------------------------
    // Pass 1: Empty Scene (Clear to Dark Slate Blue)
    // -----------------------------------------------------------------------
    renderer.settings_mut().render_mode = RenderMode::Raster;
    renderer.settings_mut().gi_mode = GiMode::Off;
    renderer
        .render(
            &[],
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
            false,
            false,
            &[],
        )
        .expect("render step 1");
    renderer
        .copy_offscreen_to_readback()
        .expect("readback step 1");
    let pixels = renderer.readback_pixels().expect("pixels step 1");
    let p1_path = out_dir.join("step1_clear.png");
    save_rgba_png(&p1_path, &pixels, WIDTH, HEIGHT).expect("save step 1");
    println!("Step 1 (Clear): Saved to {}", p1_path.display());

    // -----------------------------------------------------------------------
    // Pass 2: Calibration Quad (Red quad directly in front of camera)
    // -----------------------------------------------------------------------
    let quad_mesh = renderer
        .register_mesh(&calibration_quad())
        .expect("register quad");
    let red_mat = renderer
        .register_material(MaterialUploadInput {
            base_color: [1.0, 0.1, 0.1, 1.0],
            metallic: 0.0,
            roughness: 0.5,
            emissive: [0.0; 3],
            albedo_tex: None,
            normal_tex: None,
            metallic_roughness_tex: None,
            emissive_tex: None,
            occlusion_tex: None,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
            transmission: 0.0,
            ior: 1.5,
            translucency: 0.0,
            anisotropy: 0.0,
            clearcoat: 0.0,
            clearcoat_roughness: 0.0,
            emissive_strength: 1.0,
        })
        .expect("register red mat");
    renderer.flush_materials().expect("flush materials");
    let red_slot = renderer.material_slot(red_mat);

    let quad_item = DrawItem {
        mesh: quad_mesh,
        entity_id: 1,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        material: red_slot,
        cast_shadow: true,
        visible_in_main_pass: true,
        cast_shadow_two_sided: true,
        state: DrawState::default(),
        world_bounds: None,
        index_start: 0,
        index_count: None,
        bone_offset: None,
        splat_slots: [u32::MAX; 4],
        splat_tiling: 0.0,
    };

    renderer
        .render(
            &[quad_item],
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
            false,
            false,
            &[],
        )
        .expect("render step 2");
    renderer
        .copy_offscreen_to_readback()
        .expect("readback step 2");
    let pixels = renderer.readback_pixels().expect("pixels step 2");
    let p2_path = out_dir.join("step2_calibration_quad.png");
    save_rgba_png(&p2_path, &pixels, WIDTH, HEIGHT).expect("save step 2");
    println!("Step 2 (Calibration Quad): Saved to {}", p2_path.display());

    // -----------------------------------------------------------------------
    // Pass 3: Real Crytek Sponza Meshes Loading
    // -----------------------------------------------------------------------
    // 资源随仓库提交，按 crate 位置解析，避免写死某台机器上的绝对路径。
    let sponza_gltf_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../projects/sponza/assets/models/sponza/Sponza.gltf");
    let sponza_prims = load_sponza_primitives(&sponza_gltf_path).expect("load sponza gltf");
    println!(
        "Loaded {} primitives from Crytek Sponza glTF",
        sponza_prims.len()
    );

    let mut sponza_draws = Vec::new();

    // Scale 0.01 model matrix (transforms cm to meters)
    let sponza_model = [
        [0.01, 0.0, 0.0, 0.0],
        [0.0, 0.01, 0.0, 0.0],
        [0.0, 0.0, 0.01, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    for (i, (mesh_in, mat_in)) in sponza_prims.into_iter().enumerate() {
        let m_handle = renderer
            .register_mesh(&mesh_in)
            .expect("register sponza mesh");
        let mat_handle = renderer
            .register_material(mat_in)
            .expect("register sponza mat");
        let slot = renderer.material_slot(mat_handle);

        sponza_draws.push(DrawItem {
            mesh: m_handle,
            entity_id: 100 + i as u32,
            model: sponza_model,
            material: slot,
            cast_shadow: true,
            visible_in_main_pass: true,
            cast_shadow_two_sided: true,
            state: DrawState::default(),
            world_bounds: None,
            index_start: 0,
            index_count: None,
            bone_offset: None,
            splat_slots: [u32::MAX; 4],
            splat_tiling: 0.0,
        });
    }

    renderer.flush_materials().expect("flush sponza materials");
    println!("Registered {} Sponza DrawItems", sponza_draws.len());

    // -----------------------------------------------------------------------
    // Pass 4: Sponza Forward Raster (Solid Opaque)
    // -----------------------------------------------------------------------
    renderer
        .render(
            &sponza_draws,
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
            false,
            false,
            &[],
        )
        .expect("render step 4");
    renderer
        .copy_offscreen_to_readback()
        .expect("readback step 4");
    let pixels = renderer.readback_pixels().expect("pixels step 4");
    let p4_path = out_dir.join("step4_sponza_raster.png");
    save_rgba_png(&p4_path, &pixels, WIDTH, HEIGHT).expect("save step 4");
    println!("Step 4 (Sponza Raster): Saved to {}", p4_path.display());

    // -----------------------------------------------------------------------
    // Pass 5: Sponza Raster Wireframe
    // -----------------------------------------------------------------------
    renderer
        .render(
            &sponza_draws,
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
            true, // wireframe = true
            false,
            &[],
        )
        .expect("render step 5");
    renderer
        .copy_offscreen_to_readback()
        .expect("readback step 5");
    let pixels = renderer.readback_pixels().expect("pixels step 5");
    let p5_path = out_dir.join("step5_sponza_wireframe.png");
    save_rgba_png(&p5_path, &pixels, WIDTH, HEIGHT).expect("save step 5");
    println!("Step 5 (Sponza Wireframe): Saved to {}", p5_path.display());

    // -----------------------------------------------------------------------
    // Pass 6: Sponza Depth Buffer (debug_rt = 1)
    // -----------------------------------------------------------------------
    renderer
        .render(
            &sponza_draws,
            &frame_data,
            frame_data.light_view_proj,
            [[0.0; 4]; 4],
            0,
            1,
            1, // debug_rt = 1 (Depth)
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
            false,
            false,
            &[],
        )
        .expect("render step 6");
    renderer
        .copy_offscreen_to_readback()
        .expect("readback step 6");
    let pixels = renderer.readback_pixels().expect("pixels step 6");
    let p6_path = out_dir.join("step6_sponza_depth.png");
    save_rgba_png(&p6_path, &pixels, WIDTH, HEIGHT).expect("save step 6");
    println!(
        "Step 6 (Sponza Depth Buffer): Saved to {}",
        p6_path.display()
    );

    // -----------------------------------------------------------------------
    // Pass 7: Sponza View-Space Normals (debug_rt = 2)
    // -----------------------------------------------------------------------
    renderer
        .render(
            &sponza_draws,
            &frame_data,
            frame_data.light_view_proj,
            [[0.0; 4]; 4],
            0,
            1,
            2, // debug_rt = 2 (Normals)
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
            false,
            false,
            &[],
        )
        .expect("render step 7");
    renderer
        .copy_offscreen_to_readback()
        .expect("readback step 7");
    let pixels = renderer.readback_pixels().expect("pixels step 7");
    let p7_path = out_dir.join("step7_sponza_normals.png");
    save_rgba_png(&p7_path, &pixels, WIDTH, HEIGHT).expect("save step 7");
    println!(
        "Step 7 (Sponza View Normals): Saved to {}",
        p7_path.display()
    );

    // -----------------------------------------------------------------------
    // Pass 8: Sponza Path Tracing (Direct Lighting, 4 iterations)
    // -----------------------------------------------------------------------
    println!(
        "Building Path Tracing Scene for Sponza ({} instances)...",
        sponza_draws.len()
    );
    renderer
        .sync_pt_geometry(&sponza_draws)
        .expect("sync sponza geometry to pt");
    println!("Path Tracing TLAS/BLAS built successfully!");

    renderer.settings_mut().render_mode = RenderMode::PathTrace;
    renderer.settings_mut().gi_mode = GiMode::Off;
    renderer.settings_mut().pt_max_bounces = 3;

    for iter in 0..16 {
        renderer
            .render(
                &sponza_draws,
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
                RenderMode::PathTrace,
                3,
                1000.0,
                0,
                1.0,
                &[],
                PostGrade::default(),
                false,
                false,
                &[],
            )
            .expect("render pt iter");
        if (iter + 1) % 4 == 0 {
            println!("  PT Direct iter {} complete", iter + 1);
        }
    }

    renderer
        .copy_offscreen_to_readback()
        .expect("readback step 8");
    let pixels = renderer.readback_pixels().expect("pixels step 8");
    let p8_path = out_dir.join("step8_sponza_pt_direct.png");
    save_rgba_png(&p8_path, &pixels, WIDTH, HEIGHT).expect("save step 8");
    println!("Step 8 (Sponza PT Direct): Saved to {}", p8_path.display());

    // -----------------------------------------------------------------------
    // Pass 9: Sponza Path Tracing with RTXGI SHARC (32 iterations)
    // -----------------------------------------------------------------------
    renderer.settings_mut().render_mode = RenderMode::PathTrace;
    renderer.settings_mut().gi_mode = GiMode::Sharc;
    renderer.settings_mut().pt_max_bounces = 4;
    renderer.settings_mut().sharc_enable_clear = true;
    renderer.settings_mut().sharc_enable_update = true;
    renderer.settings_mut().sharc_enable_resolve = true;

    for iter in 0..32 {
        renderer
            .render(
                &sponza_draws,
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
                RenderMode::PathTrace,
                4,
                1000.0,
                0,
                1.0,
                &[],
                PostGrade::default(),
                false,
                false,
                &[],
            )
            .expect("render pt sharc iter");
        if (iter + 1) % 8 == 0 {
            println!("  PT SHARC iter {} complete", iter + 1);
        }
    }

    renderer
        .copy_offscreen_to_readback()
        .expect("readback step 9");
    let pixels = renderer.readback_pixels().expect("pixels step 9");
    let p9_path = out_dir.join("step9_sponza_pt_sharc.png");
    save_rgba_png(&p9_path, &pixels, WIDTH, HEIGHT).expect("save step 9");
    println!("Step 9 (Sponza PT SHARC): Saved to {}", p9_path.display());
}
