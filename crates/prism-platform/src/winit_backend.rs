//! winit backend: window creation plus winit event → neutral event
//! ([`PlatformEvent`]) translation.
//!
//! Only compiled when `winit-backend` is enabled; in a `win32-native`-only build
//! this module is left out entirely, so winit is never linked into the artifact.

use std::time::Instant;

use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

use crate::config::WindowConfig;
use crate::event::{Key, Modifiers, PlatformEvent, TouchPhase};
use crate::window::PlatformWindow;

/// Creates the winit window on the **main thread** (fast, no Vulkan / GPU work).
pub(crate) fn create_window(
    event_loop: &ActiveEventLoop,
    window_cfg: &WindowConfig,
) -> PlatformWindow {
    let t_start = Instant::now();

    let mut attrs = Window::default_attributes()
        .with_title(&window_cfg.title)
        .with_inner_size(winit::dpi::LogicalSize::new(
            window_cfg.width as f64,
            window_cfg.height as f64,
        ))
        .with_resizable(window_cfg.resizable)
        .with_maximized(window_cfg.maximized)
        .with_visible(window_cfg.visible)
        .with_decorations(window_cfg.decorations);

    if let (Some(w), Some(h)) = (window_cfg.min_width, window_cfg.min_height) {
        attrs = attrs.with_min_inner_size(winit::dpi::LogicalSize::new(w as f64, h as f64));
    }
    if let (Some(w), Some(h)) = (window_cfg.max_width, window_cfg.max_height) {
        attrs = attrs.with_max_inner_size(winit::dpi::LogicalSize::new(w as f64, h as f64));
    }
    if let (Some(x), Some(y)) = (window_cfg.position_x, window_cfg.position_y) {
        attrs = attrs.with_position(winit::dpi::LogicalPosition::new(x as f64, y as f64));
    }
    if window_cfg.fullscreen {
        attrs = attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
    }

    let window = event_loop
        .create_window(attrs)
        .expect("failed to create window");

    log::info!(
        "PlatformContext: window created in {}ms",
        (Instant::now() - t_start).as_millis(),
    );

    // winit's `Window` is the window owner itself; the render thread only takes raw
    // handles, so the `Window` is no longer needed.
    PlatformWindow::from_winit(window)
}

impl Key {
    /// Maps a winit physical key code to a neutral key.
    ///
    /// Unlisted keys degrade to [`Key::Other`] (payload is the winit discriminant)
    /// instead of being silently dropped.
    pub fn from_winit(code: winit::keyboard::KeyCode) -> Self {
        use winit::keyboard::KeyCode as Wk;
        match code {
            Wk::KeyW => Key::KeyW,
            Wk::KeyA => Key::KeyA,
            Wk::KeyS => Key::KeyS,
            Wk::KeyD => Key::KeyD,
            Wk::KeyQ => Key::KeyQ,
            Wk::KeyE => Key::KeyE,
            Wk::KeyT => Key::KeyT,
            Wk::Space => Key::Space,
            Wk::ShiftLeft => Key::ShiftLeft,
            Wk::ShiftRight => Key::ShiftRight,
            Wk::ControlLeft => Key::ControlLeft,
            Wk::ControlRight => Key::ControlRight,
            Wk::AltLeft => Key::AltLeft,
            Wk::AltRight => Key::AltRight,
            Wk::Escape => Key::Escape,
            Wk::Tab => Key::Tab,
            Wk::Enter => Key::Enter,
            Wk::ArrowUp => Key::ArrowUp,
            Wk::ArrowDown => Key::ArrowDown,
            Wk::ArrowLeft => Key::ArrowLeft,
            Wk::ArrowRight => Key::ArrowRight,
            Wk::Digit0 => Key::Digit0,
            Wk::Digit1 => Key::Digit1,
            Wk::Digit2 => Key::Digit2,
            Wk::Digit3 => Key::Digit3,
            Wk::Digit4 => Key::Digit4,
            Wk::Digit5 => Key::Digit5,
            Wk::Digit6 => Key::Digit6,
            Wk::Digit7 => Key::Digit7,
            Wk::Digit8 => Key::Digit8,
            Wk::Digit9 => Key::Digit9,
            Wk::F1 => Key::F1,
            Wk::F2 => Key::F2,
            Wk::F3 => Key::F3,
            Wk::F4 => Key::F4,
            Wk::F5 => Key::F5,
            Wk::F6 => Key::F6,
            Wk::F7 => Key::F7,
            Wk::F8 => Key::F8,
            Wk::F9 => Key::F9,
            Wk::F10 => Key::F10,
            Wk::F11 => Key::F11,
            Wk::F12 => Key::F12,
            other => Key::Other(other as u32),
        }
    }
}

/// Converts a winit event into a platform-independent event.
pub fn to_platform_event(event: &WindowEvent) -> PlatformEvent {
    match event {
        WindowEvent::CloseRequested => PlatformEvent::CloseRequested,
        WindowEvent::Focused(focused) => PlatformEvent::Focused(*focused),
        WindowEvent::Resized(size) => PlatformEvent::Resized {
            width: size.width,
            height: size.height,
        },
        WindowEvent::CursorMoved { position, .. } => PlatformEvent::CursorMoved {
            x: position.x,
            y: position.y,
        },
        WindowEvent::MouseInput { state, button, .. } => PlatformEvent::MouseButton {
            button: match button {
                winit::event::MouseButton::Left => 0,
                winit::event::MouseButton::Right => 1,
                winit::event::MouseButton::Middle => 2,
                winit::event::MouseButton::Back => 3,
                winit::event::MouseButton::Forward => 4,
                winit::event::MouseButton::Other(v) => *v,
            },
            pressed: *state == winit::event::ElementState::Pressed,
        },
        WindowEvent::MouseWheel { delta, .. } => PlatformEvent::MouseWheel {
            delta: match delta {
                winit::event::MouseScrollDelta::LineDelta(_, y) => *y as f64,
                winit::event::MouseScrollDelta::PixelDelta(pos) => {
                    crate::event::scroll_lines_from_pixels(pos.y)
                }
            },
        },
        WindowEvent::KeyboardInput { event, .. } => PlatformEvent::Key {
            key: match event.physical_key {
                winit::keyboard::PhysicalKey::Code(code) => Key::from_winit(code),
                winit::keyboard::PhysicalKey::Unidentified(_) => Key::Other(u32::MAX),
            },
            pressed: event.state == winit::event::ElementState::Pressed,
        },
        WindowEvent::ModifiersChanged(modifiers) => {
            let state = modifiers.state();
            PlatformEvent::Modifiers(Modifiers {
                shift: state.shift_key(),
                control: state.control_key(),
                alt: state.alt_key(),
            })
        }
        WindowEvent::Touch(touch) => PlatformEvent::Touch {
            id: touch.id,
            x: touch.location.x,
            y: touch.location.y,
            phase: match touch.phase {
                winit::event::TouchPhase::Started => TouchPhase::Started,
                winit::event::TouchPhase::Moved => TouchPhase::Moved,
                winit::event::TouchPhase::Ended => TouchPhase::Ended,
                winit::event::TouchPhase::Cancelled => TouchPhase::Cancelled,
            },
        },
        _ => PlatformEvent::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalSize;

    #[test]
    fn converts_resize_without_exposing_winit_types() {
        let event = WindowEvent::Resized(PhysicalSize::new(1280, 720));
        assert_eq!(
            to_platform_event(&event),
            PlatformEvent::Resized {
                width: 1280,
                height: 720
            }
        );
    }

    #[test]
    fn converts_close_request() {
        assert_eq!(
            to_platform_event(&WindowEvent::CloseRequested),
            PlatformEvent::CloseRequested
        );
    }

    #[test]
    fn converts_touch_with_pointer_id() {
        let event = WindowEvent::Touch(winit::event::Touch {
            device_id: winit::event::DeviceId::dummy(),
            phase: winit::event::TouchPhase::Started,
            location: winit::dpi::PhysicalPosition::new(12.5, 34.0),
            force: None,
            id: 7,
        });
        assert_eq!(
            to_platform_event(&event),
            PlatformEvent::Touch {
                id: 7,
                x: 12.5,
                y: 34.0,
                phase: TouchPhase::Started,
            }
        );
    }

    #[test]
    fn keeps_touch_phase_and_distinct_pointer_ids() {
        let make = |phase, id| {
            WindowEvent::Touch(winit::event::Touch {
                device_id: winit::event::DeviceId::dummy(),
                phase,
                location: winit::dpi::PhysicalPosition::new(1.0, 2.0),
                force: Some(winit::event::Force::Normalized(0.5)),
                id,
            })
        };
        assert_eq!(
            to_platform_event(&make(winit::event::TouchPhase::Moved, 1)),
            PlatformEvent::Touch {
                id: 1,
                x: 1.0,
                y: 2.0,
                phase: TouchPhase::Moved,
            }
        );
        assert_eq!(
            to_platform_event(&make(winit::event::TouchPhase::Cancelled, 2)),
            PlatformEvent::Touch {
                id: 2,
                x: 1.0,
                y: 2.0,
                phase: TouchPhase::Cancelled,
            }
        );
    }

    fn wheel(delta: winit::event::MouseScrollDelta) -> PlatformEvent {
        to_platform_event(&WindowEvent::MouseWheel {
            device_id: winit::event::DeviceId::dummy(),
            delta,
            phase: winit::event::TouchPhase::Moved,
        })
    }

    fn wheel_lines(delta: winit::event::MouseScrollDelta) -> f64 {
        match wheel(delta) {
            PlatformEvent::MouseWheel { delta } => delta,
            other => panic!("expected a wheel event, got {other:?}"),
        }
    }

    #[test]
    fn converts_line_delta_wheel_notches_unchanged() {
        use winit::event::MouseScrollDelta as W;
        assert_eq!(wheel_lines(W::LineDelta(0.0, 1.0)), 1.0);
        assert_eq!(wheel_lines(W::LineDelta(0.0, -2.0)), -2.0);
        // A high-precision wheel reports fractional lines, passed through directly.
        assert_eq!(wheel_lines(W::LineDelta(0.0, 0.25)), 0.25);
    }

    #[test]
    fn converts_pixel_delta_wheel_into_lines() {
        use winit::dpi::PhysicalPosition;
        use winit::event::MouseScrollDelta as W;
        assert_eq!(
            wheel_lines(W::PixelDelta(PhysicalPosition::new(0.0, 100.0))),
            2.0
        );
        assert_eq!(
            wheel_lines(W::PixelDelta(PhysicalPosition::new(0.0, -75.0))),
            -1.5
        );
        assert_eq!(
            wheel_lines(W::PixelDelta(PhysicalPosition::new(0.0, 12.0))),
            12.0 / crate::event::PIXELS_PER_SCROLL_LINE
        );
    }

    /// The key map is hand-written: spot-check movement / modifier / function / digit
    /// keys so a winit-side change cannot silently misalign the two.
    #[test]
    fn winit_keycodes_map_to_native_keys() {
        use winit::keyboard::KeyCode as Wk;
        for (winit, expected) in [
            (Wk::KeyW, Key::KeyW),
            (Wk::KeyA, Key::KeyA),
            (Wk::KeyS, Key::KeyS),
            (Wk::KeyD, Key::KeyD),
            (Wk::Space, Key::Space),
            (Wk::Escape, Key::Escape),
            (Wk::Tab, Key::Tab),
            (Wk::Enter, Key::Enter),
            (Wk::ArrowUp, Key::ArrowUp),
            (Wk::Digit0, Key::Digit0),
            (Wk::Digit9, Key::Digit9),
            (Wk::F3, Key::F3),
        ] {
            assert_eq!(Key::from_winit(winit), expected, "{winit:?}");
        }
        // Unlisted keys must not be dropped: they degrade to the winit discriminant.
        assert_eq!(Key::from_winit(Wk::KeyZ), Key::Other(Wk::KeyZ as u32));
    }
}
