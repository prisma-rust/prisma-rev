#![cfg(feature = "sharc")]
//! SHaRC 世界缓存的生命周期规则（修复规格 §8）。
//!
//! 缓存有两种寿命：屏幕历史（累积、运动向量）跟随相机与 extent 变化；世界缓存
//! 按世界位置存辐射，只有辐射场本身改变时才失效。这个测试用
//! [`GraphRenderer::sharc_clear_count`] 观察清空是否真的发生：
//!
//! - resize 只丢屏幕历史，世界缓存保留；
//! - 灯光变化、几何/场景切换、天空变化各自让缓存失效；
//! - 缓存空闲（光栅模式）时记录的失效原因跨帧保留，等缓存再次被使用才消费；
//! - `instances_dirty` 不算失效：绘制列表每帧重建，相机一动签名就变。
//!
//! 没有 Vulkan 设备或没有 ray query 时按现有 GPU 测试的约定跳过。

use prism_render::render_graph::{DrawItem, DrawState, GiMode, PostGrade, RenderMode};
use prism_render::{DirtyDispatchPlan, FrameInput, FrameUBOData, GraphRenderer};

const SIZE: usize = 128;

fn try_create() -> Option<GraphRenderer> {
    match GraphRenderer::offscreen_new(SIZE as u32, SIZE as u32, None) {
        Ok(mut renderer) => {
            if !renderer.context().rt_caps.has_ray_query() {
                eprintln!("[SKIP] no VK_KHR_ray_query support");
                return None;
            }
            // 小容量 + 默认场景尺度：缓存够跑，显存占用可控。
            renderer.settings_mut().gi_mode = GiMode::Sharc;
            renderer.settings_mut().sharc_capacity = 1 << 14;
            renderer.settings_mut().sharc_scene_scale = 50.0;
            Some(renderer)
        }
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
    DrawItem {
        mesh,
        entity_id,
        model,
        material: None,
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

fn frame_data(view_proj: [[f32; 4]; 4], extent: (usize, usize)) -> FrameUBOData {
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
        viewport_size: [extent.0 as f32, extent.1 as f32],
        exposure: 1.0,
        shadow_strength: 0.0,
        _pad2: [0.0; 3],
        _pad3: [0.0; 2],
        prev_view_proj: view_proj,
    }
}

/// 走完五阶段并回读一帧。`plan` 为空时等同于「本帧没有任何脏数据」。
fn render_frame(
    renderer: &mut GraphRenderer,
    items: &[DrawItem],
    mode: RenderMode,
    plan: Option<&DirtyDispatchPlan>,
    extent: (usize, usize),
) {
    let view_proj = perspective();
    let ctx = renderer.begin_frame().expect("begin_frame").expect("frame");
    let data = frame_data(view_proj, extent);
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
        render_mode: mode,
        pt_max_bounces: 3,
        pt_ray_max_distance: 100.0,
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
        dispatch_plan: plan,
    };
    renderer.prepare(&ctx, &input).expect("prepare");
    renderer.execute(&ctx, &input).expect("execute");
    let out_of_date = renderer.present(&ctx).expect("present");
    renderer.end_frame(&ctx, out_of_date).expect("end_frame");
    renderer
        .copy_offscreen_to_readback()
        .expect("copy_offscreen_to_readback");
    renderer.readback_pixels().expect("readback_pixels");
}

fn box_items(renderer: &mut GraphRenderer) -> Vec<DrawItem> {
    let mesh = renderer
        .register_mesh(&prism_render::primitives::builtin("box").expect("builtin box"))
        .expect("register box");
    vec![
        draw_item(mesh, translation_scale(0.0, 0.0, -4.0, 2.0), 1),
        draw_item(mesh, translation_scale(0.0, -1.5, -6.0, 2.0), 2),
    ]
}

#[test]
fn resize_reprojects_the_world_cache_instead_of_clearing_it() {
    let Some(mut renderer) = try_create() else {
        return;
    };
    let items = box_items(&mut renderer);

    // 第一帧创建缓存并清空一次（初始分配）。
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    let after_init = renderer.sharc_clear_count();
    assert_eq!(after_init, 1, "cache allocation clears exactly once");

    // 静止的相机：世界缓存没有理由失效。
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    assert_eq!(
        renderer.sharc_clear_count(),
        after_init,
        "a steady frame must not touch the world cache"
    );

    // resize 只让屏幕历史（运动向量 / 累积）失效；世界缓存按世界位置存辐射，
    // 换分辨率只需要重投影。
    renderer
        .resize_offscreen(192, 160)
        .expect("resize_offscreen");
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (192, 160),
    );
    assert_eq!(
        renderer.sharc_clear_count(),
        after_init,
        "resize must keep the world-space cache"
    );
}

#[test]
fn light_change_clears_the_cache() {
    let Some(mut renderer) = try_create() else {
        return;
    };
    let items = box_items(&mut renderer);
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    let after_init = renderer.sharc_clear_count();

    // 绘制列表每帧重建，签名随相机/剔除变化；它不能算作几何失效，否则相机动
    // 一下就会清空世界缓存。
    let instances_only = DirtyDispatchPlan {
        instances_dirty: true,
        emissive_table_dirty: true,
        ..Default::default()
    };
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        Some(&instances_only),
        (SIZE, SIZE),
    );
    assert_eq!(
        renderer.sharc_clear_count(),
        after_init,
        "instances_dirty alone must not clear the world cache"
    );

    // 灯光变了：旧体素里的辐射来自已经不存在的入射光，按 §8 硬清空。
    let lights = DirtyDispatchPlan {
        lights_dirty: true,
        pt_accum_dirty: true,
        ..Default::default()
    };
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        Some(&lights),
        (SIZE, SIZE),
    );
    assert_eq!(
        renderer.sharc_clear_count(),
        after_init + 1,
        "a lighting change must clear the world cache"
    );

    // 失效原因是一次性的：下一帧不再清。
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    assert_eq!(renderer.sharc_clear_count(), after_init + 1);
    assert!(!renderer.sharc_invalidation_pending());
}

#[test]
fn sky_change_clears_the_cache() {
    let Some(mut renderer) = try_create() else {
        return;
    };
    let items = box_items(&mut renderer);
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    let after_init = renderer.sharc_clear_count();

    // 换环境 = 换训练时的入射光；旧体素里的辐射不再属于这个场景。
    renderer.set_environment(None).expect("set_environment");
    assert!(
        renderer.sharc_invalidation_pending(),
        "swapping the IBL must queue an invalidation"
    );
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    assert_eq!(
        renderer.sharc_clear_count(),
        after_init + 1,
        "an environment swap must clear the world cache"
    );
}

#[test]
fn invalidation_stays_pending_while_the_cache_is_idle() {
    let Some(mut renderer) = try_create() else {
        return;
    };
    let items = box_items(&mut renderer);
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    let after_init = renderer.sharc_clear_count();

    // 场景卸载发生在光栅帧里：此时没有 SHaRC 可清，原因必须跨帧留住。
    renderer.invalidate_pt_geometry();
    assert!(renderer.sharc_invalidation_pending());
    render_frame(
        &mut renderer,
        &items,
        RenderMode::Raster,
        None,
        (SIZE, SIZE),
    );
    assert_eq!(
        renderer.sharc_clear_count(),
        after_init,
        "a raster frame must not touch the cache"
    );
    assert!(
        renderer.sharc_invalidation_pending(),
        "the reason must survive an idle frame"
    );

    // 回到 SHaRC 的第一帧才消费它 —— 也正是旧体素会变可见的那一帧。
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    assert_eq!(renderer.sharc_clear_count(), after_init + 1);
    assert!(!renderer.sharc_invalidation_pending());
}

#[test]
fn voxel_scale_change_clears_the_cache() {
    let Some(mut renderer) = try_create() else {
        return;
    };
    let items = box_items(&mut renderer);
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    let after_init = renderer.sharc_clear_count();

    // 同一尺度再跑一帧：没有格式变化，缓存保留。
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    assert_eq!(renderer.sharc_clear_count(), after_init);

    // 改 voxel 尺度等于改 hash 键：旧条目还留在原地，但它们现在描述的是别的
    // 体素（§8「cache 格式变化清空」）。
    renderer.settings_mut().sharc_scene_scale = 25.0;
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    assert_eq!(
        renderer.sharc_clear_count(),
        after_init + 1,
        "a voxel-format change must clear the cache"
    );

    // 一次性：同一尺度继续跑不再清。
    render_frame(
        &mut renderer,
        &items,
        RenderMode::PathTrace,
        None,
        (SIZE, SIZE),
    );
    assert_eq!(renderer.sharc_clear_count(), after_init + 1);
}
