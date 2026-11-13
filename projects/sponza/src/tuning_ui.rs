//! Sponza RTXGI & Path Tracing interactive tuning UI.
//!
//! Replicates the sections and controls of the NVIDIA RTXGI SDK
//! (`Samples/Pathtracer/UI.cpp`):
//! - **Generic**: Camera coordinates, VSync, Frame time / FPS.
//! - **Render**: Raster / Path Trace mode, Post (DLSS, Auto Exposure).
//! - **Path Tracing**: Mode (Reference, SHaRC, NRC), Bounces, Ray Max Distance, Exposure.
//! - **SHaRC (RTXGI)**: Enable Clear, Update, Resolve, Anti-firefly, Roughness threshold,
//!   Downscale factor, Scene scale, Accumulation frame count, Stale frame count.
//! - **Lighting / Tonemapping**: Reinhard, ACES, Passthrough.
//! - **DLSS**: Off, Quality, Balanced, Performance, UltraPerformance, DLAA.
//!
//! Keyboard & Interaction:
//! - `F1` / `Tab`: Toggle Tuning Panel.
//! - `Up` / `Down` arrows or `W`/`S`: Navigate parameter rows.
//! - `Left` / `Right` arrows or `A`/`D`: Adjust value down/up.
//! - `Space` / `Enter`: Toggle boolean / trigger action.
//! - `1` / `2` / `3`: Quick select GI Mode (1: Reference, 2: SHaRC, 3: NRC).
//! - `T`: Cycle tonemapper (Reinhard -> ACES -> Passthrough).
//! - `R`: Reset accumulation cache.
//! - `V`: Toggle VSync.

use log::info;
use prism_app::FrameHook;
use prism_app::{RenderShared, RenderStats};
use prism_engine::ecs::{Entity, World};
use prism_engine::render_settings::{DlssMode, GiMode, RenderMode, RenderSettings};
use prism_platform::{Key, PlatformEvent, PlatformWindow};
use prism_scene::ui::{
    Anchors, Canvas, CanvasScaleMode, Margin, Node, Pivot, Style, Text, TextAlign,
};
use prism_scene::Active;

const NUM_ITEMS: usize = 17;

#[derive(Debug)]
pub struct SponzaTuningHook {
    /// 指针锁定（相机接管键鼠）时的快照，每帧由 `on_tick` 从窗口同步。
    ///
    /// 键盘事件不带锁定信息，而面板按键要走「锁定态归相机、解锁态归面板」的
    /// 分流，所以这里存一份：事件处理时读到的是上一帧的值，比窗口查询快，
    /// 也不会在一帧里中途变卦。
    pub locked: bool,
    pub show: bool,
    pub selected_index: usize,

    // Parameters matching RTXGI Pathtracer UI
    pub render_mode: RenderMode,
    pub auto_exposure: bool,
    pub gi_mode: GiMode,
    pub bounces: u32,
    pub ray_max_distance: f32,
    pub tonemap_mode: u32,
    pub sharc_enable_clear: bool,
    pub sharc_enable_update: bool,
    pub sharc_enable_resolve: bool,
    pub sharc_enable_anti_firefly: bool,
    pub sharc_scene_scale: f32,
    pub sharc_capacity: u32,
    pub sharc_roughness_threshold: f32,
    pub sharc_downscale_factor: u32,
    pub sharc_accumulation_frame_num: u32,
    pub sharc_stale_frame_num: u32,
    pub dlss_mode: DlssMode,
    pub vsync: bool,
    pub request_reset: bool,

    // ECS UI entities
    canvas_entity: Option<Entity>,
    panel_bg_entity: Option<Entity>,
    panel_text_entity: Option<Entity>,
    status_text_entity: Option<Entity>,
}

impl Default for SponzaTuningHook {
    fn default() -> Self {
        let (render_mode, gi_mode) = match std::env::var("SPONZA_RENDER_MODE") {
            Ok(mode) if mode.eq_ignore_ascii_case("raster") => (RenderMode::Raster, GiMode::Off),
            Ok(mode) if mode.eq_ignore_ascii_case("pt_off") => (RenderMode::PathTrace, GiMode::Off),
            _ => (RenderMode::PathTrace, GiMode::Sharc),
        };

        Self {
            locked: false,
            show: true,
            selected_index: 0,
            render_mode,
            auto_exposure: true,
            gi_mode,
            bounces: 4,
            ray_max_distance: 1000.0,
            tonemap_mode: 1, // ACES
            sharc_enable_clear: false,
            sharc_enable_update: true,
            sharc_enable_resolve: true,
            sharc_enable_anti_firefly: true,
            sharc_scene_scale: 50.0,
            sharc_capacity: 1 << 20,
            sharc_roughness_threshold: 0.4,
            sharc_downscale_factor: 5,
            sharc_accumulation_frame_num: 20,
            sharc_stale_frame_num: 60,
            dlss_mode: DlssMode::Off,
            vsync: false,
            request_reset: false,
            canvas_entity: None,
            panel_bg_entity: None,
            panel_text_entity: None,
            status_text_entity: None,
        }
    }
}

impl SponzaTuningHook {
    fn cycle_render_mode(&mut self) {
        // Raster <-> Path Trace. Either direction invalidates the path tracing
        // accumulation.
        self.render_mode = match self.render_mode {
            RenderMode::PathTrace => RenderMode::Raster,
            _ => RenderMode::PathTrace,
        };
        self.request_reset = true;
    }

    fn adjust_parameter(&mut self, increase: bool) {
        match self.selected_index {
            0 => self.cycle_render_mode(),
            1 => {
                // GI Mode: Off <-> Sharc <-> Nrc
                self.gi_mode = match self.gi_mode {
                    GiMode::Off => {
                        if increase {
                            GiMode::Sharc
                        } else {
                            GiMode::Nrc
                        }
                    }
                    GiMode::Sharc => {
                        if increase {
                            GiMode::Nrc
                        } else {
                            GiMode::Off
                        }
                    }
                    GiMode::Nrc => {
                        if increase {
                            GiMode::Off
                        } else {
                            GiMode::Sharc
                        }
                    }
                };
                // GI modes only exist in path tracing, so selecting one also
                // leaves raster mode.
                self.render_mode = RenderMode::PathTrace;
                self.request_reset = true;
            }
            2 => {
                // Bounces: 1 .. 24
                if increase {
                    self.bounces = (self.bounces + 1).min(24);
                } else {
                    self.bounces = self.bounces.saturating_sub(1).max(1);
                }
                self.request_reset = true;
            }
            3 => {
                // Ray max distance: 100.0 .. 5000.0
                if increase {
                    self.ray_max_distance = (self.ray_max_distance + 100.0).min(5000.0);
                } else {
                    self.ray_max_distance = (self.ray_max_distance - 100.0).max(100.0);
                }
                self.request_reset = true;
            }
            4 => {
                // Tonemapper: 0=Reinhard, 1=ACES, 2=Passthrough
                if increase {
                    self.tonemap_mode = (self.tonemap_mode + 1) % 3;
                } else {
                    self.tonemap_mode = if self.tonemap_mode == 0 {
                        2
                    } else {
                        self.tonemap_mode - 1
                    };
                }
            }
            5 => self.sharc_enable_clear = !self.sharc_enable_clear,
            6 => self.sharc_enable_update = !self.sharc_enable_update,
            7 => self.sharc_enable_resolve = !self.sharc_enable_resolve,
            8 => self.sharc_enable_anti_firefly = !self.sharc_enable_anti_firefly,
            9 => {
                // Scene scale: 5.0 .. 100.0
                if increase {
                    self.sharc_scene_scale = (self.sharc_scene_scale + 5.0).min(100.0);
                } else {
                    self.sharc_scene_scale = (self.sharc_scene_scale - 5.0).max(5.0);
                }
                self.request_reset = true;
            }
            10 => {
                // Roughness threshold: 0.0 .. 1.0
                if increase {
                    self.sharc_roughness_threshold =
                        (self.sharc_roughness_threshold + 0.05).min(1.0);
                } else {
                    self.sharc_roughness_threshold =
                        (self.sharc_roughness_threshold - 0.05).max(0.0);
                }
                self.request_reset = true;
            }
            11 => {
                // Downscale factor: 1 .. 10
                if increase {
                    self.sharc_downscale_factor = (self.sharc_downscale_factor + 1).min(10);
                } else {
                    self.sharc_downscale_factor =
                        self.sharc_downscale_factor.saturating_sub(1).max(1);
                }
            }
            12 => {
                // Accumulation frames: 1 .. 120
                if increase {
                    self.sharc_accumulation_frame_num =
                        (self.sharc_accumulation_frame_num + 5).min(120);
                } else {
                    self.sharc_accumulation_frame_num =
                        self.sharc_accumulation_frame_num.saturating_sub(5).max(1);
                }
            }
            13 => {
                // Stale frames: 1 .. 120
                if increase {
                    self.sharc_stale_frame_num = (self.sharc_stale_frame_num + 5).min(120);
                } else {
                    self.sharc_stale_frame_num =
                        self.sharc_stale_frame_num.saturating_sub(5).max(1);
                }
            }
            14 => {
                // Cache capacity: 2^16 .. 2^23 entries (doubles per step).
                // Resizing rebuilds the buffers, so the cache restarts empty.
                if increase {
                    self.sharc_capacity = (self.sharc_capacity * 2).min(1 << 23);
                } else {
                    self.sharc_capacity = (self.sharc_capacity / 2).max(1 << 16);
                }
                self.request_reset = true;
            }
            15 => {
                // DLSS Mode
                self.dlss_mode = match self.dlss_mode {
                    DlssMode::Off => {
                        if increase {
                            DlssMode::Quality
                        } else {
                            DlssMode::Dlaa
                        }
                    }
                    DlssMode::Quality => {
                        if increase {
                            DlssMode::Balanced
                        } else {
                            DlssMode::Off
                        }
                    }
                    DlssMode::Balanced => {
                        if increase {
                            DlssMode::Performance
                        } else {
                            DlssMode::Quality
                        }
                    }
                    DlssMode::Performance => {
                        if increase {
                            DlssMode::UltraPerformance
                        } else {
                            DlssMode::Balanced
                        }
                    }
                    DlssMode::UltraPerformance => {
                        if increase {
                            DlssMode::Dlaa
                        } else {
                            DlssMode::Performance
                        }
                    }
                    DlssMode::Dlaa => {
                        if increase {
                            DlssMode::Off
                        } else {
                            DlssMode::UltraPerformance
                        }
                    }
                };
                self.request_reset = true;
            }
            16 => self.auto_exposure = !self.auto_exposure,
            _ => {}
        }
    }

    fn toggle_parameter(&mut self) {
        match self.selected_index {
            0 | 1 | 15 => self.adjust_parameter(true),
            5 => self.sharc_enable_clear = !self.sharc_enable_clear,
            6 => self.sharc_enable_update = !self.sharc_enable_update,
            7 => self.sharc_enable_resolve = !self.sharc_enable_resolve,
            8 => self.sharc_enable_anti_firefly = !self.sharc_enable_anti_firefly,
            16 => self.auto_exposure = !self.auto_exposure,
            _ => {}
        }
    }
}

/// 一次按键的路由结果。
///
/// 钩子先于 App 看到事件：返回 `false` 时 App 会走自己的快捷键（F3 / Tab / T /
/// 数字键 PBR 隔离）**并**把按键交给 InputManager。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyRoute {
    /// 面板不处理：原样交给 App（含引擎的移动键）。
    Forward,
    /// 面板吃掉了这个键（App 的快捷键与引擎的按键状态都不再看到它）。
    Consume,
}

/// Key::to_vk 的 'M' / 'R' / 'V'：平台层把两个后端统一成 Win32 VK_*。
const VK_M: u32 = 0x4D;
const VK_R: u32 = 0x52;
const VK_V: u32 = 0x56;

impl SponzaTuningHook {
    /// 面板按键不区分按下 / 松开：按一次触发一次。
    fn on_key_pressed(&mut self, key: Key) -> KeyRoute {
        match key {
            // 面板开关：任何遮挡键都不能抢它，否则面板一开就再也关不掉。
            Key::F1 | Key::Tab => {
                self.show = !self.show;
                return KeyRoute::Consume;
            }
            // GI 模式是 Sponza 的招牌操作，面板收着也要能用（和顶部状态栏写的
            // 「[1] Ref [2] SHaRC [3] NRC」一致）。
            Key::Digit1 => {
                self.gi_mode = GiMode::Off;
                self.request_reset = true;
                return KeyRoute::Consume;
            }
            Key::Digit2 => {
                self.gi_mode = GiMode::Sharc;
                self.request_reset = true;
                return KeyRoute::Consume;
            }
            Key::Digit3 => {
                self.gi_mode = GiMode::Nrc;
                self.request_reset = true;
                return KeyRoute::Consume;
            }
            // M / R / V 是面板自己的功能键，同时 R 也是引擎的移动键。
            // 指针锁定时它们归相机，面板只在解锁态接收。
            Key::Other(VK_M) if !self.locked => {
                self.cycle_render_mode();
                return KeyRoute::Consume;
            }
            Key::Other(VK_R) if !self.locked => {
                self.request_reset = true;
                return KeyRoute::Consume;
            }
            Key::Other(VK_V) if !self.locked => {
                self.vsync = !self.vsync;
                return KeyRoute::Consume;
            }
            // 面板关着时下面这些都没意义：交给 App 处理自己的 T / 数字键。
            _ if !self.show => KeyRoute::Forward,
            Key::KeyT => {
                self.tonemap_mode = (self.tonemap_mode + 1) % 3;
                return KeyRoute::Consume;
            }
            Key::ArrowUp => {
                self.selected_index = if self.selected_index == 0 {
                    NUM_ITEMS - 1
                } else {
                    self.selected_index - 1
                };
                return KeyRoute::Consume;
            }
            Key::ArrowDown => {
                self.selected_index = (self.selected_index + 1) % NUM_ITEMS;
                return KeyRoute::Consume;
            }
            Key::ArrowLeft => {
                self.adjust_parameter(false);
                return KeyRoute::Consume;
            }
            Key::ArrowRight => {
                self.adjust_parameter(true);
                return KeyRoute::Consume;
            }
            Key::Space | Key::Enter => {
                self.toggle_parameter();
                return KeyRoute::Consume;
            }
            // WASD 是引擎的移动键，**永远**原样交给相机（App 的快捷键里没有它们，
            // 不存在抢键问题）。面板开着时顺带当方向键导航——先前用「未锁定才导航」
            // 做过分流，结果一开始指针没锁定，走位键全被面板吃掉，相机一动不动。
            Key::KeyW | Key::KeyA | Key::KeyS | Key::KeyD => {
                if self.show {
                    self.navigate_with_wasd(key);
                }
                return KeyRoute::Forward;
            }
            _ => KeyRoute::Forward,
        }
    }

    /// 解锁态下 WASD 与方向键同义（面板是唯一在用键盘的地方）。
    fn navigate_with_wasd(&mut self, key: Key) {
        match key {
            Key::KeyW => self.on_key_pressed(Key::ArrowUp),
            Key::KeyS => self.on_key_pressed(Key::ArrowDown),
            Key::KeyA => self.on_key_pressed(Key::ArrowLeft),
            Key::KeyD => self.on_key_pressed(Key::ArrowRight),
            _ => KeyRoute::Forward,
        };
    }
}

impl FrameHook for SponzaTuningHook {
    fn on_platform_event(&mut self, _window: &PlatformWindow, event: PlatformEvent) -> bool {
        if !matches!(event, PlatformEvent::Key { .. }) {
            log::info!("HOOK-OTHER {event:?}");
            return false;
        }
        let PlatformEvent::Key { key, pressed } = event else {
            return false;
        };
        log::info!("HOOK-KEY {key:?} pressed={pressed} locked={} show={}", self.locked, self.show);
        if pressed && self.on_key_pressed(key) == KeyRoute::Consume {
            return true;
        }
        // false = 没消费：App 照常走快捷键并路由进 InputManager（锁定态下 WASD
        // 走的就是这条：面板不管，相机照常移动）。
        false
    }

    fn on_tick(
        &mut self,
        window: &PlatformWindow,
        world: &mut World,
        settings: &mut RenderSettings,
        stats: &RenderStats,
        _shared: &RenderShared,
    ) {
        // 指针锁定 = 相机接管键鼠。平台层是唯一真值来源（App 里那份
        // `InputManager::pointer_locked` 钩子拿不到），每帧同步一次给键盘分流用。
        self.locked = window.is_pointer_locked();

        // Sync runtime settings to engine
        settings.render_mode = self.render_mode;
        settings.auto_exposure = self.auto_exposure;
        settings.gi_mode = self.gi_mode;
        settings.pt_max_bounces = self.bounces;
        settings.pt_ray_max_distance = self.ray_max_distance;
        settings.tonemap_mode = self.tonemap_mode;
        // 锁定后光标是藏起来的，UI 不该再处理悬停 / 点击：否则光标被夹在窗口中央
        // 时，指针下恰好有按钮就会被「按住」，锁定期间界面一直亮着被按下的样子。
        if self.locked {
            world.remove_resource::<prism_engine::ui::UiInputState>();
        }
        // 指针锁定 = 在走位看画面，面板收起来（F1 / Tab 随时可以叫回来）。
        self.show = self.show && !self.locked;
        settings.sharc_enable_clear = self.sharc_enable_clear;
        settings.sharc_enable_update = self.sharc_enable_update;
        settings.sharc_enable_resolve = self.sharc_enable_resolve;
        settings.sharc_enable_anti_firefly = self.sharc_enable_anti_firefly;
        settings.sharc_scene_scale = self.sharc_scene_scale;
        settings.sharc_capacity = self.sharc_capacity;
        settings.sharc_roughness_threshold = self.sharc_roughness_threshold;
        settings.sharc_downscale_factor = self.sharc_downscale_factor;
        settings.sharc_accumulation_frame_num = self.sharc_accumulation_frame_num;
        settings.sharc_stale_frame_num = self.sharc_stale_frame_num;
        settings.dlss_mode = self.dlss_mode;

        if self.request_reset {
            self.request_reset = false;
            settings.pt_max_iterations = 0;
            info!("Sponza: path tracing accumulation reset requested");
        }

        // Initialize UI entities once
        if self.canvas_entity.is_none() {
            let canvas = world.spawn();
            world.insert(canvas, Node::default());
            world.insert(
                canvas,
                Canvas {
                    reference_resolution: [1280.0, 720.0],
                    scale_mode: CanvasScaleMode::ScaleWithScreenSize,
                    match_width_or_height: 0.5,
                },
            );
            world.insert(canvas, Active(true));
            self.canvas_entity = Some(canvas);

            // Panel background quad
            let bg = world.spawn();
            world.insert(bg, Node::default());
            world.insert(
                bg,
                Style {
                    width: Some(450.0),
                    height: Some(520.0),
                    anchors: Anchors::TOP_LEFT,
                    pivot: Pivot::TOP_LEFT,
                    margin: Margin {
                        left: 12.0,
                        top: 12.0,
                        ..Margin::default()
                    },
                    background: [0.06, 0.08, 0.11, 0.92],
                    border_radius: 8.0,
                    visible: true,
                    ..Style::default()
                },
            );
            world.insert(bg, Active(true));
            self.panel_bg_entity = Some(bg);

            // Panel multiline text
            let ptext = world.spawn();
            world.insert(ptext, Node::default());
            world.insert(
                ptext,
                Style {
                    width: Some(430.0),
                    height: Some(500.0),
                    anchors: Anchors::TOP_LEFT,
                    pivot: Pivot::TOP_LEFT,
                    margin: Margin {
                        left: 22.0,
                        top: 20.0,
                        ..Margin::default()
                    },
                    visible: true,
                    ..Style::default()
                },
            );
            world.insert(
                ptext,
                Text {
                    content: String::new(),
                    font_size: 13.0,
                    color: [0.94, 0.96, 1.0, 1.0],
                    alignment: TextAlign::Left,
                    wrap: true,
                    ..Text::default()
                },
            );
            world.insert(ptext, Active(true));
            self.panel_text_entity = Some(ptext);

            // Top status banner (visible when panel is closed)
            let stext = world.spawn();
            world.insert(stext, Node::default());
            world.insert(
                stext,
                Style {
                    width: Some(800.0),
                    height: Some(28.0),
                    anchors: Anchors::TOP_LEFT,
                    pivot: Pivot::TOP_LEFT,
                    margin: Margin {
                        left: 14.0,
                        top: 10.0,
                        ..Margin::default()
                    },
                    visible: true,
                    ..Style::default()
                },
            );
            world.insert(
                stext,
                Text {
                    content: String::new(),
                    font_size: 13.0,
                    color: [0.72, 0.88, 1.0, 0.95],
                    alignment: TextAlign::Left,
                    wrap: false,
                    ..Text::default()
                },
            );
            world.insert(stext, Active(true));
            self.status_text_entity = Some(stext);
        }

        // Format and update text
        let fps = stats.fps;
        let ms = stats.frame_time_ms;
        let samples = stats.pt_frame_count.unwrap_or(0);
        let gpu_str = match &stats.gpu_timings {
            Some(g) if g.supported => format!("{:.2} ms", g.total_ms),
            _ => "n/a".to_string(),
        };

        if self.show {
            if let Some(bg) = self.panel_bg_entity {
                world.set_active(bg, true);
            }
            if let Some(stext) = self.status_text_entity {
                world.set_active(stext, false);
            }
            if let Some(ptext) = self.panel_text_entity {
                world.set_active(ptext, true);

                let marker = |idx: usize| {
                    if idx == self.selected_index {
                        "> "
                    } else {
                        "  "
                    }
                };

                let gi_str = match self.gi_mode {
                    GiMode::Off => "Reference (PT)",
                    GiMode::Sharc => "SHaRC (RTXGI)",
                    GiMode::Nrc => "NRC (Neural)",
                };

                let tonemap_str = match self.tonemap_mode {
                    0 => "Reinhard",
                    1 => "ACES",
                    _ => "Passthrough",
                };

                let dlss_str = match self.dlss_mode {
                    DlssMode::Off => "Off",
                    DlssMode::Quality => "Quality",
                    DlssMode::Balanced => "Balanced",
                    DlssMode::Performance => "Performance",
                    DlssMode::UltraPerformance => "Ultra Performance",
                    DlssMode::Dlaa => "DLAA",
                };

                let mode_str = match self.render_mode {
                    RenderMode::PathTrace => "Path Trace",
                    RenderMode::Raster => "Raster",
                    _ => "Sprites 2D",
                };

                let on_off = |v: bool| if v { "ON" } else { "OFF" };

                let content = format!(
                    "=== Sponza RTXGI / PT Tuning ===\n\
                     Frame: {:.2} ms ({:.0} FPS) | GPU: {}\n\
                     Accum Samples: {} | [F1] Hide | [R] Reset\n\
                     ----------------------------------------\n\
                     {}Render Mode:        [ {} ] (Key: M)\n\
                     {}GI Mode:            [ {} ] (Keys: 1, 2, 3)\n\
                     {}Max Bounces:        [ {} ]\n\
                     {}Ray Max Dist:       [ {:.0} ]\n\
                     {}Tonemapper:         [ {} ] (Key: T)\n\
                     --- RTXGI SHaRC Options ----------------\n\
                     {}Enable Clear:       [ {} ]\n\
                     {}Enable Update:      [ {} ]\n\
                     {}Enable Resolve:     [ {} ]\n\
                     {}Anti-Firefly:       [ {} ]\n\
                     {}Scene Scale:        [ {:.1} ]\n\
                     {}Roughness Thresh:   [ {:.2} ]\n\
                     {}Downscale Factor:   [ {} ]\n\
                     {}Accum Frame Num:    [ {} ]\n\
                     {}Stale Frame Num:    [ {} ]\n\
                     {}Cache Capacity:     [ {}K ]\n\
                     --- Post / Upscaling -------------------\n\
                     {}DLSS Mode:          [ {} ]\n\
                     {}Auto Exposure:      [ {} ]\n\
                     ----------------------------------------\n\
                     [Up/Down] Select   [Left/Right] Adjust\n\
                     [Space/Enter] Toggle   [V] VSync: {}",
                    ms,
                    fps,
                    gpu_str,
                    samples,
                    marker(0),
                    mode_str,
                    marker(1),
                    gi_str,
                    marker(2),
                    self.bounces,
                    marker(3),
                    self.ray_max_distance,
                    marker(4),
                    tonemap_str,
                    marker(5),
                    on_off(self.sharc_enable_clear),
                    marker(6),
                    on_off(self.sharc_enable_update),
                    marker(7),
                    on_off(self.sharc_enable_resolve),
                    marker(8),
                    on_off(self.sharc_enable_anti_firefly),
                    marker(9),
                    self.sharc_scene_scale,
                    marker(10),
                    self.sharc_roughness_threshold,
                    marker(11),
                    self.sharc_downscale_factor,
                    marker(12),
                    self.sharc_accumulation_frame_num,
                    marker(13),
                    self.sharc_stale_frame_num,
                    marker(14),
                    self.sharc_capacity / 1024,
                    marker(15),
                    dlss_str,
                    marker(16),
                    on_off(self.auto_exposure),
                    on_off(self.vsync),
                );

                if let Some(t) = world.get_mut::<Text>(ptext) {
                    t.content = content;
                }
            }
        } else {
            if let Some(bg) = self.panel_bg_entity {
                world.set_active(bg, false);
            }
            if let Some(ptext) = self.panel_text_entity {
                world.set_active(ptext, false);
            }
            if let Some(stext) = self.status_text_entity {
                world.set_active(stext, true);

                let gi_short = match self.gi_mode {
                    GiMode::Off => "Reference",
                    GiMode::Sharc => "SHaRC",
                    GiMode::Nrc => "NRC",
                };
                let mode_short = match self.render_mode {
                    RenderMode::PathTrace => "PT",
                    RenderMode::Raster => "Raster",
                    _ => "Sprites2D",
                };
                let status = format!(
                    "[F1/Tab] Tuning Panel | Mode: {mode_short} [M] | GI: {gi_short} | Bounces: {} | {:.0} FPS ({:.1} ms) | Samples: {} | [1] Ref [2] SHaRC [3] NRC",
                    self.bounces, fps, ms, samples
                );
                if let Some(t) = world.get_mut::<Text>(stext) {
                    t.content = status;
                }
            }
        }
    }
}
