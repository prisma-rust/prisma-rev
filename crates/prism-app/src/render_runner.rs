//! 渲染线程——从主线程接收 [`FramePacket`] 与叠加层消息，
//! 并驱动 [`GraphRenderer`]（begin_frame → 执行 → present）。
//!
//! 渲染线程由 [`App`](crate::app::App) 在 `resumed` 中**异步**启动：
//! 主线程只创建窗口（快速），渲染线程在此**内部**构建 [`GraphRenderer`]
//! （~数百毫秒，但不再阻塞窗口事件）。运行直到主线程将 `running` 标志设为 `false`。
//!
//! 资产解析也在此线程完成 GPU 段：主线程 CPU 段产出的 [`AssetResolveRequest`]
//! 经通道投递，本线程每帧取走并交给 `GraphRenderer::apply_asset_requests`。

use std::sync::Arc;
use std::time::Instant;

use prism_engine::dirty_router::{draw_items_signature, DirtyRouter};
use prism_engine::ecs::Entity;
use prism_engine::render_settings::RenderSettings;
use prism_engine::render_system::FramePacket;
use prism_platform::SendWindowHandles;
use prism_render::asset_bridge::{AssetResolveRequest, AssetResolveResult};
use prism_render::frame_stats::{FrameStats, FrameStatsSummary};
use prism_render::SwapchainOverlay;
use prism_render::{FrameInput, FrameUBOData, GraphRenderer};
// 直接用 `raw-window-handle` crate（winit 的 `winit::raw_window_handle` 就是它的
// 重导出）：`win32-native` 构建里不能出现任何 `winit::` 路径。
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};

use crate::render_shared::{FrameImage, RenderShared, RenderStats};

/// 进程级原始窗口句柄的 `Send` 包装。
///
/// `RawDisplayHandle`/`RawWindowHandle` 自身因含有 `NonNull<c_void>`
/// （Wayland 变体）被 Rust 静态判定为非 `Send`，无法直接跨线程捕获。但本
/// 引擎仅在 Windows/Android 运行，其实际变体是进程级的指针值（Win32 的
/// `HWND` / Android 的 `ANativeWindow` 指针，display 句柄为空结构体），
/// 在任意线程用于创建 Vulkan 表面都是安全的；且窗口生命周期覆盖整个渲染
/// 线程，故此处显式标注 `Send` 是成立的。
/// 把主线程提取的 [`RawDisplayHandle`]/[`RawWindowHandle`] 重新包装成
/// `HasDisplayHandle`/`HasWindowHandle`，供渲染线程内的 `GraphRenderer::new`
/// 使用。winit 的 `Window::window_handle()` 在**非创建线程**上会返回错误
/// （句柄不可用），因此必须在主线程把原始句柄取出来再跨线程传递——
/// 原始 HWND 是进程级的值，在任意线程用于创建 Vulkan 表面都是安全的。
struct RawHandleBridge<'a> {
    display: &'a RawDisplayHandle,
    window: &'a RawWindowHandle,
}

impl HasDisplayHandle for RawHandleBridge<'_> {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // 原始句柄由主线程保证有效；此处仅重新借用。
        Ok(unsafe { DisplayHandle::borrow_raw(*self.display) })
    }
}

impl HasWindowHandle for RawHandleBridge<'_> {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Ok(unsafe { WindowHandle::borrow_raw(*self.window) })
    }
}

/// 帧率计数器的平滑因子（值越低越平滑）。
const FPS_ALPHA: f32 = 0.05;

/// 外部叠加层工厂：渲染线程启动前由主线程提供，返回 [`SwapchainOverlay`]。
pub type OverlayFactory = Box<dyn Fn() -> Box<dyn SwapchainOverlay> + Send>;

/// 渲染线程的呈现目标。
pub enum RenderTarget {
    /// 真实窗口 + 交换链（游戏 / launcher）。
    Surface {
        handles: SendWindowHandles,
        extensions: Vec<String>,
        vsync: bool,
    },
    /// Native surface supplied by an embedding host. The host owns the
    /// window; the renderer owns only the Vulkan surface and swapchain.
    NativeSurface {
        handles: SendWindowHandles,
        extensions: Vec<String>,
        vsync: bool,
    },
    /// 无窗口离屏图像（编辑器视口）。帧由宿主经 [`RenderShared::request_frame`]
    /// 请求，渲染线程读回 CPU 后放进 [`RenderShared::frame`]。
    Offscreen { width: u32, height: u32 },
}

/// 渲染线程入口点。
///
/// 与旧实现不同，本函数在渲染线程**内部**构建 [`GraphRenderer`]：
/// - `handles` 是主线程在窗口创建线程上提取的原始句柄（[`SendWindowHandles`]）
///   ——`Window::window_handle()` 在非创建线程会失败，故必须主线程取出后传递。
/// - `extensions` 是主线程算好的 Vulkan 实例扩展名（依赖窗口 display handle）。
/// - `scene_env` 是场景声明式光照的 IBL 数据（None → 廉价空 IBL，不卷积）。
/// - `warmup` 是否在首帧前预热所有 GPU 管线。
/// - `vsync` 是否请求垂直同步的 present mode（来自 `WindowConfig.vsync`）。
/// - `overlay_factory` 若有，构建完成后立即安装外部叠加层。
///
/// 这样 `resumed()` 能在窗口建好后立即返回，窗口事件（关闭/移动/缩放）
/// 不再被渲染器初始化阻塞。循环：建渲染器 → 取 packet/asset/overlay 消息
/// → 构建输入 → begin/execute/present。当 `shared.running` 变为 `false` 时退出。
pub fn render_thread_main(
    shared: Arc<RenderShared>,
    target: RenderTarget,
    scene_env: Option<Vec<u8>>,
    warmup: bool,
    overlay_factory: Option<OverlayFactory>,
    shadow_map_size: u32,
    shadow_cascade_count: u32,
) {
    let mut dirty_router = DirtyRouter::new();
    let mut graph_report = crate::render_graph_report::ReportWriter::from_env();

    log::info!("Render thread started");

    // -------------------------------------------------------------------
    // 异步构建渲染器（~数百毫秒，但运行在渲染线程，不阻塞窗口事件）
    // -------------------------------------------------------------------
    // 原始句柄包装成 Has*Handle——raw 句柄是进程级的，跨线程安全。
    let created = match &target {
        RenderTarget::Surface {
            handles,
            extensions,
            vsync,
        } => {
            let extensions_ref: Vec<&str> = extensions.iter().map(|s| s.as_str()).collect();
            let handle_bridge = RawHandleBridge {
                display: &handles.display,
                window: &handles.window,
            };
            GraphRenderer::new_with_vsync_and_shadow_size(
                extensions_ref,
                &handle_bridge,
                &handle_bridge,
                None,
                *vsync,
                shadow_map_size,
                shadow_cascade_count,
            )
        }
        RenderTarget::NativeSurface {
            handles,
            extensions,
            vsync,
        } => {
            let extensions_ref: Vec<&str> = extensions.iter().map(|s| s.as_str()).collect();
            let handle_bridge = RawHandleBridge {
                display: &handles.display,
                window: &handles.window,
            };
            GraphRenderer::new_with_vsync_and_shadow_size(
                extensions_ref,
                &handle_bridge,
                &handle_bridge,
                None,
                *vsync,
                shadow_map_size,
                shadow_cascade_count,
            )
        }
        RenderTarget::Offscreen { width, height } => GraphRenderer::offscreen_new_with_shadow_size(
            *width,
            *height,
            None,
            shadow_map_size,
            shadow_cascade_count,
        ),
    };
    let mut renderer = match created {
        Ok(r) => r,
        Err(e) => {
            let message = format!("failed to create renderer: {e:#}");
            log::error!("Render thread: {message}");
            shared.fail_render(message);
            return;
        }
    };
    shared.mark("renderer_built");
    log::info!("Render thread: renderer built");

    if warmup {
        if let Err(e) = renderer.warmup_pipelines() {
            log::warn!("Render thread: pipeline warmup failed (continuing): {e:#}");
        }
        shared.mark("warmup_done");
    }

    // 按「场景声明式光照」构建 IBL：仅当场景携带 EnvironmentLighting/env_map
    // （或 `.rscn` 自带环境贴图）时才跑 CPU 预卷积；否则依赖启动期廉价空 IBL。
    if let Some(bytes) = scene_env {
        log::info!("scene lighting: building IBL environment (with convolve)");
        if let Err(e) = renderer.set_environment(Some(bytes)) {
            log::warn!("Render thread: failed to set scene environment: {e:#}");
        }
    } else {
        log::info!("scene lighting: no IBL environment — cheap empty IBL (no convolve)");
    }

    // 安装外部叠加层（宿主 UI 等），其 GPU 资源在 record 时懒创建。
    if let Some(factory) = overlay_factory {
        renderer.set_external_overlay(factory());
    }

    // 帧计时
    let mut smoothed_fps = 0.0f32;
    let mut frame_count: u64 = 0;
    // ~2 minutes at 60 fps; the summary is sorted, so it is refreshed a few
    // times per second instead of every frame (#1002).
    let mut frame_stats = FrameStats::new(8192);
    let mut frame_stats_summary = FrameStatsSummary::default();
    let mut first_frame_presented = false;
    let mut first_packet_received = false;
    let mut consecutive_errors: u32 = 0;
    let screenshot = crate::screenshot::ScreenshotPlan::from_env();
    let mut screenshot_done = false;
    let mut bench =
        crate::bench_report::BenchPlan::from_env().map(crate::bench_report::BenchCollector::new);
    let mut bench_report_written = false;

    while shared.running.load(std::sync::atomic::Ordering::Relaxed) {
        // 检查来自主线程的 PT 重置请求
        if shared.take_pt_reset() {
            renderer.request_pt_reset();
        }

        // ---- 资产上传请求（主线程 CPU 段 → 渲染线程 GPU 段） ----
        let requests = shared.take_asset_requests();
        if !requests.is_empty() {
            let (entities, stripped): (Vec<Entity>, Vec<AssetResolveRequest>) =
                requests.into_iter().unzip();
            let results = renderer.apply_asset_requests(stripped);
            let paired: Vec<(Entity, AssetResolveResult)> =
                entities.into_iter().zip(results).collect();
            shared.push_asset_results(paired);
        }

        let packet = shared.take_packet();
        // 应用主线程投递的叠加层消息（如"新的宿主 UI 帧"）。
        let overlay_messages = shared.take_overlay_messages();
        for msg in overlay_messages {
            renderer.apply_overlay_message(msg);
        }

        // 应用主线程投递的渲染器命令（如 set_environment 重建 IBL）。
        let renderer_messages = shared.take_renderer_messages();
        for msg in renderer_messages {
            msg(&mut renderer);
        }

        // The host drives this loop. Do not add a frame-rate sleep here.
        let packet = match packet {
            Some(packet) => {
                if !first_packet_received {
                    first_packet_received = true;
                    log::info!(
                        "first render packet received: draws={}, camera={}, gizmos={}, extent={}x{}",
                        packet.draw_items.len(),
                        packet.scene.has_camera,
                        packet.gizmos.len(),
                        renderer.extent().width,
                        renderer.extent().height,
                    );
                }
                packet
            }
            None => {
                let guard = shared.packet.lock().unwrap();
                if guard.is_none() && shared.running.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = shared
                        .packet_ready
                        .wait_timeout(guard, std::time::Duration::from_millis(50))
                        .unwrap();
                }
                continue;
            }
        };

        let frame_start = Instant::now();

        let frame_plan = screenshot.as_ref().filter(|plan| {
            !screenshot_done
                && plan.fires_on(frame_count + 1)
                && (!packet.draw_items.is_empty() || frame_count >= 300)
        });
        let presented = match render_one_frame(
            &mut renderer,
            &packet,
            &mut dirty_router,
            &packet.settings,
            frame_plan,
        ) {
            Ok(presented) => {
                consecutive_errors = 0;
                if !presented {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                presented
            }
            Err(e) => {
                consecutive_errors += 1;
                let text = format!("{e:#}");
                // Device-lost / surface failure is terminal: retrying only spins
                // the CPU and floods the log (observed: >1M lines). Report it to
                // the host so the viewport can stop and surface the reason.
                let fatal = text.contains("device has been lost")
                    || text.contains("DEVICE_LOST")
                    || text.contains("surface has been lost")
                    || consecutive_errors >= 3;
                if fatal {
                    let message =
                        format!("render failed after {consecutive_errors} attempt(s): {text}");
                    log::error!("Render thread: {message}");
                    shared.fail_render(message);
                    break;
                }
                log::error!("Render thread error: {e:#}");
                false
            }
        };
        if presented && !first_frame_presented {
            first_frame_presented = true;
            log::info!(
                "first frame presented: extent={}x{}, draws={}, camera={}, target={}",
                renderer.extent().width,
                renderer.extent().height,
                packet.draw_items.len(),
                packet.scene.has_camera,
                if renderer.is_headless() {
                    "offscreen"
                } else {
                    "surface"
                },
            );
            shared.mark("first_frame");
            shared.print_startup_report();
        }

        if frame_plan.is_some() {
            screenshot_done = true;
            if std::env::var("PRISMREV_EXIT_ON_SCREENSHOT").is_ok() {
                log::info!("PRISMREV_EXIT_ON_SCREENSHOT: screenshot finished, exiting process");
                std::process::exit(0);
            }
        }

        // Compute render stats.
        let elapsed = frame_start.elapsed();
        let frame_time_ms = elapsed.as_secs_f32() * 1000.0;

        frame_count += 1;
        if frame_count == 1 {
            smoothed_fps = 1.0 / elapsed.as_secs_f32().max(1e-6);
        } else {
            let instant_fps = 1.0 / elapsed.as_secs_f32().max(1e-6);
            smoothed_fps = FPS_ALPHA * instant_fps + (1.0 - FPS_ALPHA) * smoothed_fps;
        }

        let pt_count = renderer.pt_frame_count();
        let gpu_timings = renderer.gpu_timings().clone();

        frame_stats.push_frame(
            frame_time_ms,
            gpu_timings.supported.then_some(gpu_timings.total_ms),
        );
        if frame_count.is_multiple_of(30) {
            frame_stats_summary = frame_stats.summary();
        }

        // Benchmark report (#1002): warm-up frames only count down, and the file
        // is written once on the frame that completes the sample set.
        if let Some(collector) = bench.as_mut() {
            let complete = collector.sample(
                frame_time_ms,
                gpu_timings.supported.then_some(gpu_timings.total_ms),
            );
            if complete && !bench_report_written {
                bench_report_written = true;
                let extent = renderer.extent();
                let metadata = collector.metadata(
                    renderer.context().adapter_name(),
                    Some((extent.width, extent.height)),
                );
                match collector.write(&metadata) {
                    Ok(()) => log::info!(
                        "benchmark report written: {} ({} samples, {} warm-up frames)",
                        collector.plan().path.display(),
                        collector.samples(),
                        collector.plan().warmup_frames,
                    ),
                    Err(e) => log::error!(
                        "benchmark report write failed ({}): {e}",
                        collector.plan().path.display()
                    ),
                }
            }
        }

        // 渲染图报告（Web 编辑器可视化）：首帧 + 低频刷新。
        if graph_report.is_enabled() {
            graph_report.maybe_write_with_timings(
                &renderer.graph().snapshot(),
                packet.settings.to_report_settings(),
                frame_count,
                Some(&gpu_timings),
                Some(renderer.render_stats()),
                renderer.draw_call_report(
                    &packet.draw_items,
                    &packet.sprite_items,
                    &packet.particle_items,
                ),
                renderer.gpu_history().to_vec(),
            );
        }

        shared.set_render_stats(RenderStats {
            frame_time_ms,
            fps: smoothed_fps,
            pt_frame_count: pt_count,
            // 不支持时间戳的设备保持 None，HUD 会明确显示「n/a」而不是拿 CPU 墙钟冒充。
            gpu_timings: gpu_timings.supported.then_some(gpu_timings),
            frame_stats: frame_stats_summary.clone(),
        });

        // ---- 宿主（编辑器）帧读回：只读回 PostPass 已 tonemap 的离屏图像 ----
        if renderer.is_headless() {
            match renderer.offscreen_extent() {
                Some(extent) => {
                    if let Ok(Some(pixels)) = renderer.poll_offscreen_readback() {
                        *shared.frame.lock().unwrap() = Some(FrameImage {
                            width: extent.width,
                            height: extent.height,
                            pixels,
                        });
                    }
                    if shared
                        .frame_requested
                        .swap(false, std::sync::atomic::Ordering::Relaxed)
                    {
                        if let Err(e) = renderer.copy_offscreen_to_readback_async() {
                            log::warn!("Render thread: frame readback submit failed: {e:#}");
                        }
                    }
                }
                None => log::warn!("Render thread: frame requested but no offscreen target"),
            }
        }

        // 归还数据包：主线程下一帧 clear() + 重填，跳过每帧的 Vec 重分配（#66）。
        shared.recycle_packet(packet);
    }

    log::info!("Render thread exiting — destroying renderer");
    drop(renderer);
}

/// Render one frame: build [`FrameInput`] from a [`FramePacket`] then drive
/// the five-phase API (begin → prepare → execute → present → end).
fn render_one_frame(
    renderer: &mut GraphRenderer,
    packet: &FramePacket,
    dirty_router: &mut DirtyRouter,
    settings: &RenderSettings,
    screenshot: Option<&crate::screenshot::ScreenshotPlan>,
) -> anyhow::Result<bool> {
    let scene = &packet.scene;
    let draw_items = &packet.draw_items;

    let dirty_flags = dirty_router.update(scene);
    let draw_items_dirty = dirty_router.draw_items_dirty(draw_items_signature(draw_items));
    if dirty_flags.any() || draw_items_dirty {
        log::trace!(
            "dirty_flags: camera={} dir_light={} point_lights={} exposure={} draw_items={}",
            dirty_flags.camera,
            dirty_flags.directional_light,
            dirty_flags.point_lights,
            dirty_flags.exposure,
            draw_items_dirty
        );
    }

    let extent = renderer.extent();
    renderer.set_taa(settings.taa, settings.taa_strength);
    let mut frame_data = FrameUBOData {
        view_proj: scene.view_proj.to_cols_array_2d(),
        camera_position: [
            scene.eye[0],
            scene.eye[1],
            scene.eye[2],
            scene.lights.len() as f32,
        ],
        light_direction: scene.light_direction.to_array(),
        light_color: scene.light_color.to_array(),
        view: scene.view.to_cols_array_2d(),
        light_view_proj: scene.light_view_proj.to_cols_array_2d(),
        light_view_proj1: scene.light_view_proj1.to_cols_array_2d(),
        shadow_split: scene.shadow_split,
        shadow_cascade_count: scene.shadow_cascade_count,
        tonemap_mode: settings.tonemap_mode,
        viewport_size: [extent.width as f32, extent.height as f32],
        exposure: scene.exposure,
        shadow_strength: scene.shadow_strength,
        _pad2: [0.0; 3],
        _pad3: [0.0; 2],
        prev_view_proj: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
    };
    renderer.apply_taa_jitter(&mut frame_data);

    // #502：全局 LUT 强度 + assets/ 下的 .cube 路径（体积调色不覆盖 LUT）。
    let mut resolved_grade = packet.grade.unwrap_or_else(|| settings.post_grade());
    resolved_grade.lut_strength = settings.lut_strength;
    let lut_path = if settings.lut.trim().is_empty() {
        None
    } else {
        prism_engine::asset_paths::asset_path(settings.lut.trim())
    };
    // #613：内部分辨率必须在 `begin_frame` 之前生效——begin_frame 用它决定
    // HDR 中间目标尺寸，execute 再用它做 PostPass 的目标矩形。
    renderer.set_internal_resolution(settings.internal_resolution, settings.internal_scale_mode);
    renderer.set_occlusion_culling(settings.occlusion_culling);

    // GI / SHARC knobs: the renderer owns its own copy of these (they decide
    // whether the SHARC passes run at all), so forward them each frame.
    // `render_mode` and the PT knobs travel through `FrameInput` instead.
    //
    // `sharc_enable_clear` is deliberately not copied: it is a one-shot latch
    // that the renderer consumes and resets, so copying it every frame would
    // wipe the cache every frame. Callers that need a reset set it through
    // `GraphRenderer::settings_mut()`.
    {
        let renderer_settings = renderer.settings_mut();
        renderer_settings.gi_mode = settings.gi_mode;
        renderer_settings.sharc_scene_scale = settings.sharc_scene_scale;
        renderer_settings.sharc_enable_update = settings.sharc_enable_update;
        renderer_settings.sharc_enable_resolve = settings.sharc_enable_resolve;
        renderer_settings.sharc_enable_anti_firefly = settings.sharc_enable_anti_firefly;
        renderer_settings.sharc_roughness_threshold = settings.sharc_roughness_threshold;
        renderer_settings.sharc_downscale_factor = settings.sharc_downscale_factor;
        renderer_settings.sharc_accumulation_frame_num = settings.sharc_accumulation_frame_num;
        renderer_settings.sharc_stale_frame_num = settings.sharc_stale_frame_num;
    }

    let ctx = match renderer.begin_frame()? {
        Some(c) => c,
        None => return Ok(false),
    };
    // #1007：上一帧粗深度在 `begin_frame` 里读回，所以过滤放在它之后。
    let mut occlusion_items = Vec::new();
    let main_pass_items: &[prism_render::render_graph::DrawItem] = if settings.occlusion_culling {
        occlusion_items.extend_from_slice(draw_items);
        renderer.cull_occluded_draws(&mut occlusion_items);
        &occlusion_items
    } else {
        draw_items
    };
    let input = FrameInput {
        draw_items: main_pass_items,
        gizmos: &packet.gizmos,
        sprite_items: &packet.sprite_items,
        sprite_lights: &packet.sprite_lights,
        particle_items: &packet.particle_items,
        bone_matrices: &packet.bone_matrices,
        skybox: packet.skybox,
        frame_data: &frame_data,
        cameras: &scene.cameras,
        light_view_proj: scene.light_view_proj.to_cols_array_2d(),
        light_view_proj1: scene.light_view_proj1.to_cols_array_2d(),
        shadow_split: scene.shadow_split,
        shadow_cascade_count: scene.shadow_cascade_count,
        inv_projection: scene.inv_projection.to_cols_array_2d(),
        debug_flags: settings.debug_flags,
        wireframe: settings.wireframe,
        shaded_wireframe: settings.shaded_wireframe,
        tonemap_mode: settings.tonemap_mode,
        aa_mode: settings.aa_mode,
        debug_rt: settings.debug_rt,
        grade: resolved_grade,
        lut_path: lut_path.as_deref(),
        proj22: scene.proj22,
        proj32: scene.proj32,
        lights: &scene.lights,
        render_mode: settings.render_mode,
        pt_max_bounces: settings.pt_max_bounces,
        pt_ray_max_distance: settings.pt_ray_max_distance,
        pt_max_iterations: settings.pt_max_iterations,
        exposure: scene.exposure,
        pt_lights: &scene.pt_lights,
        pt_accum_dirty: dirty_flags.directional_light || draw_items_dirty,
        has_camera: scene.has_camera,
        camera_orthographic: scene.camera_orthographic,
        clear_color: scene.clear_color,
        shadows_enabled: scene.shadows_enabled,
        ui_overlay: Some(&packet.ui_overlay),
        show_grid: settings.show_grid,
    };
    if let Some(plan) = screenshot {
        renderer.request_screenshot(plan.path.clone(), plan.size, plan.scale);
    }
    renderer
        .prepare(&ctx, &input)
        .and_then(|_| renderer.execute(&ctx, &input))
        .inspect_err(|_| {
            let _ = renderer.present(&ctx);
            let _ = renderer.end_frame(&ctx, false);
        })?;
    // `GraphRenderer::present` returns whether the swapchain was out of date,
    // not whether queue submission/present succeeded. Reaching this point
    // means the frame was submitted to the native surface.
    let out_of_date = renderer.present(&ctx)?;
    if screenshot.is_some() {
        if let Err(err) = renderer.finish_screenshot(&ctx) {
            log::error!("screenshot failed: {err:#}");
        }
    }
    renderer.end_frame(&ctx, out_of_date)?;

    Ok(true)
}
