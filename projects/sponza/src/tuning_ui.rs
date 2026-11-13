//! Sponza RTXGI & Path Tracing interactive tuning UI.
//!
//! Replicates the sections and controls of the NVIDIA RTXGI SDK
//! (`Samples/Pathtracer/UI.cpp`):
//! - **Generic**: Camera coordinates, VSync, Frame time / FPS.
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

const NUM_ITEMS: usize = 14;

#[derive(Debug)]
pub struct SponzaTuningHook {
    pub show: bool,
    pub selected_index: usize,

    // Parameters matching RTXGI Pathtracer UI
    pub render_mode: RenderMode,
    pub gi_mode: GiMode,
    pub bounces: u32,
    pub ray_max_distance: f32,
    pub tonemap_mode: u32,
    pub sharc_enable_clear: bool,
    pub sharc_enable_update: bool,
    pub sharc_enable_resolve: bool,
    pub sharc_enable_anti_firefly: bool,
    pub sharc_scene_scale: f32,
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
            Ok(mode) if mode.eq_ignore_ascii_case("raster") => {
                (RenderMode::Raster, GiMode::Off)
            }
            Ok(mode) if mode.eq_ignore_ascii_case("pt_off") => {
                (RenderMode::PathTrace, GiMode::Off)
            }
            _ => (RenderMode::PathTrace, GiMode::Sharc),
        };

        Self {
            show: true,
            selected_index: 0,
            render_mode,
            gi_mode,
            bounces: 4,
            ray_max_distance: 1000.0,
            tonemap_mode: 1, // ACES
            sharc_enable_clear: false,
            sharc_enable_update: true,
            sharc_enable_resolve: true,
            sharc_enable_anti_firefly: true,
            sharc_scene_scale: 50.0,
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
    fn adjust_parameter(&mut self, increase: bool) {
        match self.selected_index {
            0 => {
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
                self.request_reset = true;
            }
            1 => {
                // Bounces: 1 .. 24
                if increase {
                    self.bounces = (self.bounces + 1).min(24);
                } else {
                    self.bounces = self.bounces.saturating_sub(1).max(1);
                }
                self.request_reset = true;
            }
            2 => {
                // Ray max distance: 100.0 .. 5000.0
                if increase {
                    self.ray_max_distance = (self.ray_max_distance + 100.0).min(5000.0);
                } else {
                    self.ray_max_distance = (self.ray_max_distance - 100.0).max(100.0);
                }
                self.request_reset = true;
            }
            3 => {
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
            4 => self.sharc_enable_clear = !self.sharc_enable_clear,
            5 => self.sharc_enable_update = !self.sharc_enable_update,
            6 => self.sharc_enable_resolve = !self.sharc_enable_resolve,
            7 => self.sharc_enable_anti_firefly = !self.sharc_enable_anti_firefly,
            8 => {
                // Scene scale: 5.0 .. 100.0
                if increase {
                    self.sharc_scene_scale = (self.sharc_scene_scale + 5.0).min(100.0);
                } else {
                    self.sharc_scene_scale = (self.sharc_scene_scale - 5.0).max(5.0);
                }
                self.request_reset = true;
            }
            9 => {
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
            10 => {
                // Downscale factor: 1 .. 10
                if increase {
                    self.sharc_downscale_factor = (self.sharc_downscale_factor + 1).min(10);
                } else {
                    self.sharc_downscale_factor =
                        self.sharc_downscale_factor.saturating_sub(1).max(1);
                }
            }
            11 => {
                // Accumulation frames: 1 .. 120
                if increase {
                    self.sharc_accumulation_frame_num =
                        (self.sharc_accumulation_frame_num + 5).min(120);
                } else {
                    self.sharc_accumulation_frame_num =
                        self.sharc_accumulation_frame_num.saturating_sub(5).max(1);
                }
            }
            12 => {
                // Stale frames: 1 .. 120
                if increase {
                    self.sharc_stale_frame_num = (self.sharc_stale_frame_num + 5).min(120);
                } else {
                    self.sharc_stale_frame_num =
                        self.sharc_stale_frame_num.saturating_sub(5).max(1);
                }
            }
            13 => {
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
            _ => {}
        }
    }

    fn toggle_parameter(&mut self) {
        match self.selected_index {
            0 => self.adjust_parameter(true),
            4 => self.sharc_enable_clear = !self.sharc_enable_clear,
            5 => self.sharc_enable_update = !self.sharc_enable_update,
            6 => self.sharc_enable_resolve = !self.sharc_enable_resolve,
            7 => self.sharc_enable_anti_firefly = !self.sharc_enable_anti_firefly,
            13 => self.adjust_parameter(true),
            _ => {}
        }
    }
}

impl FrameHook for SponzaTuningHook {
    fn on_platform_event(&mut self, _window: &PlatformWindow, event: PlatformEvent) -> bool {
        let PlatformEvent::Key { key, pressed: true } = event else {
            return false;
        };

        match key {
            Key::F1 | Key::Tab => {
                self.show = !self.show;
                return true;
            }
            Key::Digit1 => {
                self.gi_mode = GiMode::Off;
                self.request_reset = true;
                return true;
            }
            Key::Digit2 => {
                self.gi_mode = GiMode::Sharc;
                self.request_reset = true;
                return true;
            }
            Key::Digit3 => {
                self.gi_mode = GiMode::Nrc;
                self.request_reset = true;
                return true;
            }
            Key::KeyT => {
                self.tonemap_mode = (self.tonemap_mode + 1) % 3;
                return true;
            }
            // Key 'R' (0x52 in Win32 VK)
            Key::Other(0x52) => {
                self.request_reset = true;
                return true;
            }
            // Key 'V' (0x56 in Win32 VK)
            Key::Other(0x56) => {
                self.vsync = !self.vsync;
                return true;
            }
            Key::ArrowUp | Key::KeyW if self.show => {
                if self.selected_index == 0 {
                    self.selected_index = NUM_ITEMS - 1;
                } else {
                    self.selected_index -= 1;
                }
                return true;
            }
            Key::ArrowDown | Key::KeyS if self.show => {
                self.selected_index = (self.selected_index + 1) % NUM_ITEMS;
                return true;
            }
            Key::ArrowLeft | Key::KeyA if self.show => {
                self.adjust_parameter(false);
                return true;
            }
            Key::ArrowRight | Key::KeyD if self.show => {
                self.adjust_parameter(true);
                return true;
            }
            Key::Space | Key::Enter if self.show => {
                self.toggle_parameter();
                return true;
            }
            _ => false,
        }
    }

    fn on_tick(
        &mut self,
        _window: &PlatformWindow,
        world: &mut World,
        settings: &mut RenderSettings,
        stats: &RenderStats,
        _shared: &RenderShared,
    ) {
        // Sync runtime settings to engine
        settings.render_mode = self.render_mode;
        settings.gi_mode = self.gi_mode;
        settings.pt_max_bounces = self.bounces;
        settings.pt_ray_max_distance = self.ray_max_distance;
        settings.tonemap_mode = self.tonemap_mode;
        settings.sharc_enable_clear = self.sharc_enable_clear;
        settings.sharc_enable_update = self.sharc_enable_update;
        settings.sharc_enable_resolve = self.sharc_enable_resolve;
        settings.sharc_enable_anti_firefly = self.sharc_enable_anti_firefly;
        settings.sharc_scene_scale = self.sharc_scene_scale;
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
                    height: Some(480.0),
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
                    height: Some(460.0),
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

                let on_off = |v: bool| if v { "ON" } else { "OFF" };

                let content = format!(
                    "=== Sponza RTXGI / PT Tuning ===\n\
                     Frame: {:.2} ms ({:.0} FPS) | GPU: {}\n\
                     Accum Samples: {} | [F1] Hide | [R] Reset\n\
                     ----------------------------------------\n\
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
                     --- Post / Upscaling -------------------\n\
                     {}DLSS Mode:          [ {} ]\n\
                     ----------------------------------------\n\
                     [Up/Down] Select   [Left/Right] Adjust\n\
                     [Space/Enter] Toggle   [V] VSync: {}",
                    ms,
                    fps,
                    gpu_str,
                    samples,
                    marker(0),
                    gi_str,
                    marker(1),
                    self.bounces,
                    marker(2),
                    self.ray_max_distance,
                    marker(3),
                    tonemap_str,
                    marker(4),
                    on_off(self.sharc_enable_clear),
                    marker(5),
                    on_off(self.sharc_enable_update),
                    marker(6),
                    on_off(self.sharc_enable_resolve),
                    marker(7),
                    on_off(self.sharc_enable_anti_firefly),
                    marker(8),
                    self.sharc_scene_scale,
                    marker(9),
                    self.sharc_roughness_threshold,
                    marker(10),
                    self.sharc_downscale_factor,
                    marker(11),
                    self.sharc_accumulation_frame_num,
                    marker(12),
                    self.sharc_stale_frame_num,
                    marker(13),
                    dlss_str,
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
                let status = format!(
                    "[F1/Tab] Tuning Panel | GI: {gi_short} | Bounces: {} | {:.0} FPS ({:.1} ms) | Samples: {} | [1] Ref [2] SHaRC [3] NRC",
                    self.bounces, fps, ms, samples
                );
                if let Some(t) = world.get_mut::<Text>(stext) {
                    t.content = status;
                }
            }
        }
    }
}
