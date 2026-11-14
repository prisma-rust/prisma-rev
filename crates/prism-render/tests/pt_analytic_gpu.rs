//! Analytic micro-scenes for the shared-transport reference variant.
//!
//! Spec §7.2 要求先用「答案可以闭式写出」的微场景验证输送本身，再用它去评判
//! 缓存：否则参考与消费者共享同一个错误，两者会互相认证。这里的每个场景都
//! 读 `PT_COLOR_H`（R32G32B32A32_SFLOAT，tonemap 之前的线性辐射），而不是
//! `readback_pixels`：后者是 8-bit、已被 tonemap，白炉里 0.25 与 0.5 会被压到
//! 0.19 与 0.31，能量断言就失去意义了。
//!
//! 场景刻意做小。`pt_max_bounces = 1` 直接删掉间接段，于是单次弹射的错误无处
//! 藏身；两段 throughput 的测试只加一段间接弹射，并且落在凸的漫反射盒子上 ——
//! 凸体表面朝外的余弦采样永远不可能重新击中自己，路径长度被物理地钉死在一段。
//!
//! 所有场景都用 `PtVariant::Reference`：无缓存、无 ReSTIR、无 firefly clamp、
//! 均匀灯采样。消费者变体的偏置项正是要被这套测试量出来的东西，不能混进来。
#![cfg(all(feature = "pathtrace", feature = "ibl"))]

use prism_render::managers::{MaterialAlphaMode, MaterialUploadInput, MeshHandle};
use prism_render::render_graph::{DrawItem, DrawState, GiMode, PostGrade, RenderMode};
use prism_render::{FrameInput, FrameUBOData, GraphRenderer, HdrReadback, PtVariant};

const SIZE: usize = 64;

/// RGBE 的指数字节。`hdr::rgbe_to_float` 的缩放是 `2^(e - 128 - 8)`，取 128
/// 时缩放正好是 1/256，于是尾数字节 `b` 解出的辐射就是 `b / 256` —— 0.25、
/// 0.375、0.5 都是 f32 精确值，解码不引入任何舍入。
const RGBE_EXPONENT: u8 = 128;

/// 关掉太阳：`light_dir.w` 是预乘的辐射强度，取 0 时
/// `sun_radiance = light_color.rgb * light_dir.w` 为零，NEE 的
/// `dot(L_rad, L_rad) > 1e-10` 检查会直接跳过。
const SUN_OFF: [f32; 4] = [0.0, 0.0, 1.0, 0.0];
const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

/// 只沿 +Z 传播的太阳，强度写在 `w`。
fn sun(intensity: f32) -> [f32; 4] {
    [0.0, 0.0, 1.0, intensity]
}

/// 构造一张每个像素都等于 `radiance` 的 Radiance RGBE 图。
///
/// `radiance` 必须是 1/256 的整数倍（上面的指数约定），并且换算出的尾数字节
/// 不能等于 2 —— 扫描线前两个字节是 `0x02 0x02` 时解码器会把它当成 RLE，而我
/// 们要的正是未压缩字面量扫描线。
fn constant_environment(radiance: f32) -> Vec<u8> {
    let mantissa = (radiance * 256.0).round() as u8;
    assert!(
        (f32::from(mantissa) / 256.0 - radiance).abs() < 1e-6,
        "radiance {radiance} is not a multiple of 1/256"
    );
    assert_ne!(mantissa, 2, "byte 2 would be decoded as an RLE scanline");

    let mut bytes = Vec::with_capacity(64 + SIZE * SIZE * 4);
    bytes.extend_from_slice(b"#?RADIANCE\n");
    bytes.extend_from_slice(b"FORMAT=32-bit_rle_rgbe\n");
    bytes.push(b'\n');
    bytes.extend_from_slice(format!("-Y {SIZE} +X {SIZE}\n").as_bytes());
    for _ in 0..SIZE * SIZE {
        bytes.extend_from_slice(&[mantissa, mantissa, mantissa, RGBE_EXPONENT]);
    }
    bytes
}

fn try_create(env: Option<Vec<u8>>) -> Option<GraphRenderer> {
    let mut renderer = match GraphRenderer::offscreen_new(SIZE as u32, SIZE as u32, env) {
        Ok(renderer) => renderer,
        Err(err) => {
            let message = format!("{err:#}");
            if message.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                || message.contains("VK_ERROR_EXTENSION_NOT_PRESENT")
                || message.contains("no devices")
                || message.contains("No valid")
                || message.contains("PhysicalDevice")
            {
                eprintln!("[SKIP] no usable Vulkan device: {message}");
                return None;
            }
            panic!("failed to create the offscreen renderer: {message}");
        }
    };
    // 没有 ray query 就没有 `PathTracePass`，整套测试无处落脚。
    if !renderer.context().rt_caps.has_ray_query() {
        eprintln!("[SKIP] no VK_KHR_ray_query support");
        return None;
    }
    renderer.set_pt_variant(PtVariant::Reference);
    Some(renderer)
}

fn register_box(renderer: &mut GraphRenderer) -> MeshHandle {
    renderer
        .register_mesh(&prism_render::primitives::builtin("box").expect("builtin box"))
        .expect("register box")
}

fn draw_item(mesh: MeshHandle, model: [[f32; 4]; 4], material: u32, entity_id: u32) -> DrawItem {
    DrawItem {
        mesh,
        entity_id,
        model,
        material: Some(material),
        cast_shadow: false,
        visible_in_main_pass: true,
        cast_shadow_two_sided: false,
        state: DrawState::default(),
        world_bounds: None,
        index_start: 0,
        index_count: None,
        bone_offset: None,
        splat_slots: [u32::MAX; 4],
        splat_tiling: 0.0,
    }
}

/// 注册一个材质并返回它的 SSBO 槽位。SSBO 是整块上传的，所以必须
/// `flush_materials` 之后路径追踪才读得到它。
fn upload_material(renderer: &mut GraphRenderer, input: MaterialUploadInput) -> u32 {
    let handle = renderer
        .register_material(input)
        .expect("register material");
    let slot = renderer.material_slot(handle).expect("material slot");
    renderer.flush_materials().expect("flush materials");
    slot
}

/// 相机背后的盒子。主射线永远打不到它，它存在的唯一理由是让
/// `PathTracePass` 有几何：没有实例时 `pt_scene` 为空，整个 PT dispatch 被
/// 跳过，`PT_COLOR_H` 根本不会发布。
fn behind_camera_box(renderer: &mut GraphRenderer) -> Vec<DrawItem> {
    let mesh = register_box(renderer);
    let slot = upload_material(
        renderer,
        MaterialUploadInput {
            base_color: [0.0, 0.0, 0.0, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            alpha_mode: MaterialAlphaMode::Opaque,
            ..Default::default()
        },
    );
    vec![draw_item(
        mesh,
        translation_scale(0.0, 0.0, 4.0, 2.0),
        slot,
        1,
    )]
}

/// 相机前方 5 米处的漫反射盒子（半边长 1，占画面高度约 48%，中心像素一定能
/// 命中正面）。`roughness = 1.0` 让 bounce 0 的散射恒走余弦分支：源码里
/// `prob_diffuse = surf.roughness`，只有这条路径的 throughput 与采样方向无关。
fn diffuse_box(renderer: &mut GraphRenderer, albedo: f32) -> Vec<DrawItem> {
    let mesh = register_box(renderer);
    let slot = upload_material(
        renderer,
        MaterialUploadInput {
            base_color: [albedo, albedo, albedo, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            alpha_mode: MaterialAlphaMode::Opaque,
            ..Default::default()
        },
    );
    vec![draw_item(
        mesh,
        translation_scale(0.0, 0.0, -6.0, 2.0),
        slot,
        1,
    )]
}

/// 只有自发辐射的盒子。`base_color` 全黑，所以反射项为零，屏幕上不该出现
/// 任何非辐射来源。注意不调 `set_emissive`：那会把三角形注册进发光三角表，
/// 从而打开一遍额外的 NEE 采样，而我们测的是材质自身的 emissive 项。
fn emissive_box(renderer: &mut GraphRenderer, emission: f32) -> Vec<DrawItem> {
    let mesh = register_box(renderer);
    let slot = upload_material(
        renderer,
        MaterialUploadInput {
            base_color: [0.0, 0.0, 0.0, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            // `to_gpu` 只把 emissive[0] 送进 SSBO，且着色器里
            // `s.emissive = ei * es`，所以这个值就是期望的辐射。
            emissive: [emission, 0.0, 0.0],
            emissive_strength: 1.0,
            alpha_mode: MaterialAlphaMode::Opaque,
            ..Default::default()
        },
    );
    vec![draw_item(
        mesh,
        translation_scale(0.0, 0.0, -6.0, 2.0),
        slot,
        1,
    )]
}

/// 两个前后错开的漫反射盒子。散射出来的射线有可能打到另一个盒子上，于是
/// bounce 1+ 真的会命中表面 —— 这是唯一能让 SHaRC 的 hash 查找真正跑起来的
/// 几何形状（单靠一个凸盒子，间接段永远是 miss）。
fn two_box_scene(renderer: &mut GraphRenderer, albedo: f32) -> Vec<DrawItem> {
    let mesh = register_box(renderer);
    let slot = upload_material(
        renderer,
        MaterialUploadInput {
            base_color: [albedo, albedo, albedo, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            alpha_mode: MaterialAlphaMode::Opaque,
            ..Default::default()
        },
    );
    vec![
        draw_item(mesh, translation_scale(0.0, 0.0, -4.0, 2.0), slot, 1),
        draw_item(mesh, translation_scale(0.0, -2.4, -6.0, 2.0), slot, 2),
    ]
}

fn perspective() -> [[f32; 4]; 4] {
    let fov = 45.0f32.to_radians();
    let near = 0.1f32;
    let far = 100.0f32;
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
    [
        [scale, 0.0, 0.0, 0.0],
        [0.0, scale, 0.0, 0.0],
        [0.0, 0.0, scale, 0.0],
        [x, y, z, 1.0],
    ]
}

fn frame_data(
    view_proj: [[f32; 4]; 4],
    sun_direction: [f32; 4],
    sun_color: [f32; 4],
) -> FrameUBOData {
    FrameUBOData {
        view_proj,
        // `push.camera_pos.xyz` 是主射线的起点，测试把相机钉在原点。
        camera_position: [0.0, 0.0, 0.0, 0.0],
        light_direction: sun_direction,
        light_color: sun_color,
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

/// 跑一帧路径追踪并把 `PT_COLOR_H` 以线性 f32 读回来。
///
/// `request_hdr_readback` 必须在 `execute` 之前（拷贝是记进那一帧的命令
/// 缓冲的），`take_hdr_readback` 必须在 `present` 之后（headless 的 present
/// 会等帧栅栏，之后映射显存才安全）。
fn render_pt_frame(
    renderer: &mut GraphRenderer,
    items: &[DrawItem],
    sun_direction: [f32; 4],
    sun_color: [f32; 4],
    pt_max_bounces: u32,
    reset: bool,
    readback: bool,
) -> Option<HdrReadback> {
    let view_proj = perspective();
    let ctx = renderer.begin_frame().expect("begin_frame").expect("frame");
    let data = frame_data(view_proj, sun_direction, sun_color);
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
        render_mode: RenderMode::PathTrace,
        pt_max_bounces,
        pt_ray_max_distance: 100.0,
        // 不冻结：每帧一个样本就够 —— 下面每个场景的估计量都是零方差
        // （常量环境、常量 emissive、单次 NEE），不存在需要收敛的噪声。
        pt_max_iterations: 0,
        // `result *= push.camera_pos.w`：曝光乘在输出上，解析值只有在
        // exposure == 1 时才等于辐射本身。
        exposure: 1.0,
        pt_lights: &[],
        // `reset` 为真时从零开始累积，于是「一帧 == 一个蒙特卡洛样本」；为假
        // 时让累积跨帧收敛，用于需要低方差的能量比较。
        pt_accum_dirty: reset,
        has_camera: true,
        camera_orthographic: false,
        clear_color: [0.0, 0.0, 0.0, 1.0],
        shadows_enabled: false,
        ui_overlay: None,
        show_grid: false,
        dispatch_plan: None,
    };
    if readback {
        renderer.request_hdr_readback();
    }
    renderer.prepare(&ctx, &input).expect("prepare");
    renderer.execute(&ctx, &input).expect("execute");
    let out_of_date = renderer.present(&ctx).expect("present");
    renderer.end_frame(&ctx, out_of_date).expect("end_frame");
    if !readback {
        return None;
    }
    Some(
        renderer
            .take_hdr_readback()
            .expect("take_hdr_readback")
            .expect("the path tracer did not publish PT_COLOR_H this frame"),
    )
}

/// 单帧、清零累积、把 PT_COLOR_H 读回来。
fn trace_frame(
    renderer: &mut GraphRenderer,
    items: &[DrawItem],
    sun_direction: [f32; 4],
    sun_color: [f32; 4],
    pt_max_bounces: u32,
) -> HdrReadback {
    render_pt_frame(
        renderer,
        items,
        sun_direction,
        sun_color,
        pt_max_bounces,
        true,
        true,
    )
    .expect("frame published PT_COLOR_H")
}

/// 累积 `samples` 帧后读回：只有第一帧清零累积，所以读回值就是 `samples` 个样
/// 本的均值（`PT_COLOR_H` 里存的是 running mean）。
fn accumulate(
    renderer: &mut GraphRenderer,
    items: &[DrawItem],
    sun_direction: [f32; 4],
    sun_color: [f32; 4],
    pt_max_bounces: u32,
    samples: usize,
) -> HdrReadback {
    let mut hdr = None;
    for sample in 0..samples {
        let last = sample + 1 == samples;
        hdr = render_pt_frame(
            renderer,
            items,
            sun_direction,
            sun_color,
            pt_max_bounces,
            sample == 0,
            last,
        );
    }
    hdr.expect("frame published PT_COLOR_H")
}

/// 整幅图（红色通道）的最小值与最大值。常量场景应该退化成 min == max。
fn value_range(hdr: &HdrReadback) -> (f32, f32) {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for y in 0..hdr.height {
        for x in 0..hdr.width {
            let value = hdr.pixel(x, y).expect("in-range pixel")[0];
            lo = lo.min(value);
            hi = hi.max(value);
        }
    }
    (lo, hi)
}

fn assert_uniform(hdr: &HdrReadback, expected: f32, what: &str) {
    assert!(
        !hdr.has_non_finite(),
        "{what}: readback contains {} non-finite texels",
        hdr.non_finite_count()
    );
    let (lo, hi) = value_range(hdr);
    assert!(
        (lo - expected).abs() < 1e-4 && (hi - expected).abs() < 1e-4,
        "{what}: radiance range [{lo}, {hi}] != {expected}"
    );
}

#[test]
fn constant_environment_reads_back_linearly() {
    // 白炉：常量环境 0.25，场景里没有任何东西挡射线，每个像素都只该看到环境。
    let Some(mut renderer) = try_create(Some(constant_environment(0.25))) else {
        return;
    };
    let items = behind_camera_box(&mut renderer);

    let quarter = trace_frame(&mut renderer, &items, SUN_OFF, WHITE, 1);
    assert_uniform(&quarter, 0.25, "furnace at 0.25");

    // 同样的场景换成两倍辐射。
    renderer
        .set_environment(Some(constant_environment(0.5)))
        .expect("set_environment");
    let half = trace_frame(&mut renderer, &items, SUN_OFF, WHITE, 1);
    assert_uniform(&half, 0.5, "furnace at 0.5");

    // 线性性：辐射翻倍，回读也必须翻倍。这条断言就是「回读在 tonemap 之前」
    // 的证明 —— tonemap 后的 8-bit 路径会把 0.25 和 0.5 压成约 0.19 和 0.31，
    // 比值只有 1.6 倍。
    let ratio = half.mean_luminance() / quarter.mean_luminance();
    assert!(
        (ratio - 2.0).abs() < 1e-3,
        "hdr readback is not linear: 0.5 / 0.25 read back as {ratio}"
    );
}

#[test]
fn two_segment_throughput_matches_the_analytic_value() {
    // 常量环境 0.375（尾数字节 96）。用和上面不同的辐射值，是为了让 IBL 磁盘
    // 缓存的哈希不同 —— 两个测试并行跑时不会去抢同一个缓存文件。
    let Some(mut renderer) = try_create(Some(constant_environment(0.375))) else {
        return;
    };
    let albedo = 0.5;
    let items = diffuse_box(&mut renderer, albedo);

    // 两段：主射线 → 盒子表面（一次散射）→ miss → 环境。
    let hdr = trace_frame(&mut renderer, &items, SUN_OFF, WHITE, 2);
    assert!(!hdr.has_non_finite(), "two-segment path produced NaN/Inf");

    // 背景：零段散射，得到的就是环境辐射本身（miss 传播）。
    let corner = hdr.pixel(0, 0).expect("corner pixel")[0];
    assert!(
        (corner - 0.375).abs() < 1e-4,
        "background {corner} != 0.375"
    );

    // 正面：一段漫反射散射后再 miss。逐段 throughput 是
    // (brdf·cos) / pdf；roughness = 1 时 pdf 恒为余弦 pdf (cos/π)，于是
    //   throughput = (albedo/π + specular)·cos / (cos/π) = albedo + π·specular。
    // 正入射下 D = 1/π、V = 0.25、F = 0.1024，因此 π·specular = 0.1024·0.25
    // = 0.0256。整条路径的期望值就是 (albedo + 0.0256) · 0.375。
    let expected = (albedo + 0.0256) * 0.375;
    let face = hdr
        .pixel(SIZE as u32 / 2, SIZE as u32 / 2)
        .expect("centre pixel")[0];
    let relative = (face - expected).abs() / expected;
    assert!(
        relative < 0.05,
        "diffuse face read {face}, analytic value {expected} ({relative:.1}% off)"
    );

    // 凸体表面的朝外余弦采样永远不可能重新击中自己，所以提高弹射次数不该
    // 改变任何像素：多出来的那些段全都是空的。
    let deep = trace_frame(&mut renderer, &items, SUN_OFF, WHITE, 4);
    let deep_face = deep
        .pixel(SIZE as u32 / 2, SIZE as u32 / 2)
        .expect("centre pixel")[0];
    assert!(
        (deep_face - face).abs() < 1e-4,
        "a convex scene must terminate after one indirect segment: {face} -> {deep_face}"
    );
    let (lo, hi) = value_range(&deep);
    assert!(lo >= 0.0, "negative radiance {lo}");
    assert!(hi < 1e3, "runaway radiance {hi}");
}

#[test]
fn pure_emission_matches_the_material() {
    // 黑环境（`new_empty` 是 1x1x6 全零立方体），全场唯一的光源是材质自身的
    // emissive 项。
    let Some(mut renderer) = try_create(None) else {
        return;
    };
    let emission = 0.6f32;
    let items = emissive_box(&mut renderer, emission);

    // pt_max_bounces = 1：bounce 0 只累加 emissive，然后累加 NEE（太阳关掉、
    // 没有发光三角），循环一次都不进。期望值就是 emissive 本身。
    let hdr = trace_frame(&mut renderer, &items, SUN_OFF, WHITE, 1);
    assert!(!hdr.has_non_finite(), "emissive scene produced NaN/Inf");

    let centre = hdr
        .pixel(SIZE as u32 / 2, SIZE as u32 / 2)
        .expect("centre pixel");
    assert!(
        (centre[0] - emission).abs() < 1e-3,
        "emissive face read {} instead of {emission}",
        centre[0]
    );
    // `GpuMaterial` 只把 `emissive[0]` 打包进 SSBO（`metallic_roughness_emissive.z`），
    // 再乘 `emissive_strength` 后对整个 RGB 生效 —— 所以发光体是灰体，三个通道
    // 都等于同一个标量。这条断言把「灰体」这个事实钉在接口上。
    for (channel, value) in [("green", centre[1]), ("blue", centre[2])] {
        assert!(
            (value - emission).abs() < 1e-3,
            "{channel} channel read {value} instead of {emission}: the emissive term is scalar",
        );
    }

    // 盒子占画面高度约 48%，角落的射线打到黑环境上，必须是严格 0。
    let corner = hdr.pixel(0, 0).expect("corner pixel")[0];
    assert!(corner.abs() < 1e-6, "background {corner} is not black");
}

#[test]
fn directional_light_is_linear_in_radiance() {
    let Some(mut renderer) = try_create(None) else {
        return;
    };
    let albedo = 0.5;
    let items = diffuse_box(&mut renderer, albedo);

    let one = trace_frame(&mut renderer, &items, sun(1.0), WHITE, 1);
    let two = trace_frame(&mut renderer, &items, sun(2.0), WHITE, 1);

    // 单方向光、pt_max_bounces = 1：唯一的光传输项是 bounce 0 的 NEE。
    // L_dir 与 +Z 面法线同向（n_dot_l = 1），L_rad = light_color.rgb · 1，
    // 遮挡射线朝盒子外侧打，必然无遮挡，所以
    //   L = (albedo/π + F·D·V) · 强度
    // 其中 F = 0.1024（金属度 0）、D = 1/π、V = 0.25。
    let brdf = albedo / std::f32::consts::PI + 0.1024 * 0.25 / std::f32::consts::PI;
    let centre = one
        .pixel(SIZE as u32 / 2, SIZE as u32 / 2)
        .expect("centre pixel")[0];
    let relative = (centre - brdf).abs() / brdf;
    assert!(
        relative < 0.05,
        "single directional light read {centre}, analytic value {brdf} ({relative:.1}% off)"
    );

    // 线性性：强度翻倍，辐射跟着翻倍。这条断言把「NEE 里少了或多了 1/pdf
    // 因子」这类错误挡住 —— 均匀采样一盏灯的 1/pdf 就是 1。
    let centre_two = two
        .pixel(SIZE as u32 / 2, SIZE as u32 / 2)
        .expect("centre pixel")[0];
    let ratio = centre_two / centre;
    assert!(
        (ratio - 2.0).abs() < 1e-2,
        "direct lighting is not linear in radiance: {ratio}"
    );
    assert!(!one.has_non_finite() && !two.has_non_finite());
}

#[test]
fn a_degenerate_sharc_hash_does_not_poison_the_image() {
    // §7.2 的最后一项：「NaN/Inf 与空 hash」。hash 容量为 0 时缓存表根本建立
    // 不起来，但 push 常量里的 query 位依然会被置上，于是着色器真的会去查一
    // 张退化表。此时图像必须仍然有限、非负 —— 一次越界读或 NaN 传播都会让
    // PT_COLOR_H 出现脏值，而这正是下游 tonemap 藏不住的东西。
    let Some(mut renderer) = try_create(Some(constant_environment(0.25))) else {
        return;
    };
    renderer.settings_mut().gi_mode = GiMode::Sharc;
    renderer.settings_mut().sharc_capacity = 0;
    let items = two_box_scene(&mut renderer, 0.5);

    let hdr = trace_frame(&mut renderer, &items, SUN_OFF, WHITE, 4);
    assert!(
        !hdr.has_non_finite(),
        "degenerate hash produced {} non-finite texels",
        hdr.non_finite_count()
    );
    let (lo, hi) = value_range(&hdr);
    assert!(lo >= 0.0, "degenerate hash produced negative radiance {lo}");
    // 唯一的光源是常量环境 0.25；漫反射盒子只会让它衰减，所以任何像素都不该
    // 超过环境本身（外加一点 GGX 高光）。
    assert!(hi < 0.5, "degenerate hash amplified radiance to {hi}");
}

#[test]
fn cache_on_preserves_the_analytic_energy() {
    // §7.2 剩下的三项在这里一起量：缓存命中时必须抄走 `cached_radiance` 之后
    // 直接 `break`（`pt_render.slang:771-810`），不能再补一次本地 NEE；训练侧
    // 判定「这个体素已经足够可信」（`SharcUpdateHit` 返回 false）后结束路径，
    // 不能让沉积能量翻倍；而主射线 miss 的天空项与缓存完全无关。三者共同的、
    // 可观测的后果是同一条：同一个场景开缓存与不开缓存，帧能量必须一致。
    //
    // SHaRC 是有偏的（体素分辨率、材质去调制、firefly 抑制），§7.2 因此只要求
    // 约定的漫反射 ROI 相对平均亮度误差 ≤5%，以及「没有稳定的双倍计能」—— 命中
    // 时又叠一次 NEE 的典型后果是间接项翻倍，在这套场景里表现为 30% 以上的整
    // 体偏移，离 5% 的门槛很远。
    let Some(mut renderer) = try_create(Some(constant_environment(0.625))) else {
        return;
    };
    renderer.set_pt_variant(PtVariant::Consume);
    let items = two_box_scene(&mut renderer, 0.5);
    let samples = 16;
    let bounces = 4;

    renderer.settings_mut().gi_mode = GiMode::Off;
    let uncached = accumulate(&mut renderer, &items, SUN_OFF, WHITE, bounces, samples);

    // 反向对照：关掉 gi_mode 时这两个计数器必须一动不动。没有这一条，下面
    // 「计数器 > 0」的断言就无法排除「计数器根本恒为 1」这种坏法。
    #[cfg(feature = "sharc")]
    assert_eq!(
        (
            renderer.sharc_train_dispatches(),
            renderer.sharc_resolve_dispatches()
        ),
        (0, 0),
        "gi_mode = Off must not train or resolve the SHaRC cache"
    );

    // 体素边长是 `2^level / sharc_scene_scale`，而 `level` 由命中点到相机的距离
    // 决定（这套场景里 2–6 米 → level 1–2）。sceneScale = 5 时体素是 0.4–0.8 米，
    // 与两个盒子之间的线段长度同一量级，`valid_hit` 的两条几何门槛才过得去；
    // 出厂值 50 会得到 4 厘米级的体素，命中稀到只剩个位数像素。
    renderer.settings_mut().gi_mode = GiMode::Sharc;
    renderer.settings_mut().sharc_capacity = 1 << 14;
    renderer.settings_mut().sharc_scene_scale = 5.0;
    let cached = accumulate(&mut renderer, &items, SUN_OFF, WHITE, bounces, samples);

    // 缓存真的跑起来了，否则下面的能量比较是空转。计时器里出现
    // "SharcTrain" / "SharcResolve" 只说明那两段 GPU 区间被打开过
    // （`GraphRenderer::execute` 无条件 `begin_pass`），区间里的
    // `dispatch_training` 仍可能被守卫静默早退；渲染器自己记的真实 dispatch
    // 次数才是证据。
    #[cfg(feature = "sharc")]
    {
        let trained = renderer.sharc_train_dispatches();
        let resolved = renderer.sharc_resolve_dispatches();
        assert!(
            trained > 0,
            "SHaRC never recorded a training dispatch in {samples} frames"
        );
        assert!(
            resolved > 0,
            "SHaRC never recorded a resolve dispatch in {samples} frames"
        );
    }

    for (name, hdr) in [("uncached", &uncached), ("cached", &cached)] {
        assert!(
            !hdr.has_non_finite(),
            "{name}: {} non-finite texels",
            hdr.non_finite_count()
        );
        let (lo, _) = value_range(hdr);
        assert!(lo >= 0.0, "{name}: negative radiance {lo}");

        // miss 传播：画面左上角的主射线从两块盒子旁边擦过去，只看见常量天空。
        // 缓存不得参与这一项 —— 无论它训练了多久。
        let corner = hdr.pixel(0, 0).expect("corner pixel")[0];
        assert!(
            (corner - 0.625).abs() < 1e-4,
            "{name}: corner sees {corner}, expected the 0.625 sky"
        );
    }

    // 训练与解析真的 dispatch 了，也只是必要条件 —— 训练跑起来但没有一个体素
    // 落进被查询的位置，画面同样不会变。真正的证据是画面：命中缓存会抄走
    // `cached_radiance` 并直接结束路径，同一像素的估计值必然与纯路径追踪不同。
    let mut changed = 0usize;
    for y in 0..SIZE as u32 {
        for x in 0..SIZE as u32 {
            let plain = uncached.pixel(x, y).expect("uncached pixel")[0];
            let accelerated = cached.pixel(x, y).expect("cached pixel")[0];
            if (plain - accelerated).abs() > 1e-3 {
                changed += 1;
            }
        }
    }
    assert!(
        changed > 0,
        "the consumer never read the cache: every one of the {SIZE}x{SIZE} pixels is \
         identical with and without SHaRC"
    );

    let uncached_mean = uncached.mean_luminance();
    let cached_mean = cached.mean_luminance();
    let gap = (cached_mean - uncached_mean).abs() / uncached_mean;
    assert!(
        gap < 0.05,
        "the cache moved the frame energy by {:.2}% ({uncached_mean} -> {cached_mean}); \
         a hit that kept paying for the local NEE would show up here as a ~2x indirect term",
        gap * 100.0
    );
}
