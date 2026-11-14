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

/// winit 的字母键判别值 → 按键字符 → Win32 \`VK_*\`。
///
/// 只列字母行：其余键（F1–F12 / 方向键 / 数字行）在两张表里本来就同号。
const LETTER_KEYS: &[(winit::keyboard::KeyCode, u8)] = &[
    (winit::keyboard::KeyCode::KeyA, b'A'),
    (winit::keyboard::KeyCode::KeyB, b'B'),
    (winit::keyboard::KeyCode::KeyC, b'C'),
    (winit::keyboard::KeyCode::KeyD, b'D'),
    (winit::keyboard::KeyCode::KeyE, b'E'),
    (winit::keyboard::KeyCode::KeyF, b'F'),
    (winit::keyboard::KeyCode::KeyG, b'G'),
    (winit::keyboard::KeyCode::KeyH, b'H'),
    (winit::keyboard::KeyCode::KeyI, b'I'),
    (winit::keyboard::KeyCode::KeyJ, b'J'),
    (winit::keyboard::KeyCode::KeyK, b'K'),
    (winit::keyboard::KeyCode::KeyL, b'L'),
    (winit::keyboard::KeyCode::KeyM, b'M'),
    (winit::keyboard::KeyCode::KeyN, b'N'),
    (winit::keyboard::KeyCode::KeyO, b'O'),
    (winit::keyboard::KeyCode::KeyP, b'P'),
    (winit::keyboard::KeyCode::KeyQ, b'Q'),
    (winit::keyboard::KeyCode::KeyR, b'R'),
    (winit::keyboard::KeyCode::KeyS, b'S'),
    (winit::keyboard::KeyCode::KeyT, b'T'),
    (winit::keyboard::KeyCode::KeyU, b'U'),
    (winit::keyboard::KeyCode::KeyV, b'V'),
    (winit::keyboard::KeyCode::KeyW, b'W'),
    (winit::keyboard::KeyCode::KeyX, b'X'),
    (winit::keyboard::KeyCode::KeyY, b'Y'),
    (winit::keyboard::KeyCode::KeyZ, b'Z'),
];

/// 把 winit 的 **字母键判别值** 换成与后端无关的 \`Key::Letter\`，其余键原样返回。
///
/// 两条规则缺一条，就会有一个后端和另一个后端不认同一串键：
///
/// 1. Key 里有专用变体的字母（W/A/S/D/Q/E/T）必须保留变体。这些键进
///    InputManager 时是 KeyCode::KeyW 这类枚举值，相机系统按枚举查表；
///    若在这里压成 \`Letter(b'W')\`，事件链就变成
///    \`Letter(b'W')\` -> \`KeyCode::Other(0x57)\`，key_held(KeyCode::KeyW) 永远为假
///    ——这就是「Sponza 里 WASD 完全不动相机」的根因（Win32 后端走
///    Key::from_vk，字母本来就是专用变体，所以只有 winit 后端坏）。
/// 2. 其余字母走 \`Key::Letter\`：两个后端给出同一个值，项目按
///    \`Letter(b'M')\` 写快捷键，不必知道后端用的是哪套原始键码。
fn winit_to_vk(code: winit::keyboard::KeyCode) -> Key {
    let key = Key::from_winit(code);
    if !matches!(key, Key::Other(_)) {
        return key;
    }
    LETTER_KEYS
        .iter()
        .find_map(|(winit_code, byte)| (*winit_code == code).then_some(Key::Letter(*byte)))
        .unwrap_or(key)
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
                // 统一用 **Win32 VK** 作为 `Key::Other` 的载荷（`Key::to_vk` 的契约）：
                // `Key::from_winit` 会直传 winit 的 `KeyCode` 判别值（`KeyZ = 75`），
                // 和 Win32 后端的 `'Z' = 0x5A = 90` 对不上，Sponza 里按 `R`/`V`/`M`
                // （`Key::Other(0x52/0x56/0x4D)`）在两个后端只会有一个生效。
                winit::keyboard::PhysicalKey::Code(code) => winit_to_vk(code),
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

    /// 事件层用的是 `winit_to_vk`，它对字母键有两条规则：
    ///
    /// - 有专用 `Key` 变体的字母（W/A/S/D/Q/E/T）必须保留变体，否则
    ///   `InputManager` 里存的是 `KeyCode::Other(0x57)`，相机系统的
    ///   `key_held(KeyCode::KeyW)` 永远为假，WASD 不动。
    /// - 没有专用变体的字母换成 `Key::Letter`，两个后端才给出同一个值。
    #[test]
    fn winit_letters_map_to_win32_codes() {
        use winit::keyboard::KeyCode as Wk;
        // 专用变体：和 Win32 后端的 `Key::from_vk` 结果一致。
        assert_eq!(winit_to_vk(Wk::KeyW), Key::KeyW);
        assert_eq!(winit_to_vk(Wk::KeyA), Key::KeyA);
        assert_eq!(winit_to_vk(Wk::KeyS), Key::KeyS);
        assert_eq!(winit_to_vk(Wk::KeyD), Key::KeyD);
        assert_eq!(winit_to_vk(Wk::KeyQ), Key::KeyQ);
        assert_eq!(winit_to_vk(Wk::KeyE), Key::KeyE);
        assert_eq!(winit_to_vk(Wk::KeyT), Key::KeyT);
        // 无专用变体的字母：与后端无关的 ASCII 字母。
        assert_eq!(winit_to_vk(Wk::KeyR), Key::Letter(b'R'));
        assert_eq!(winit_to_vk(Wk::KeyV), Key::Letter(b'V'));
        assert_eq!(winit_to_vk(Wk::KeyM), Key::Letter(b'M'));
        // 非字母键保持原样（两张表本来同号）。
        assert_eq!(winit_to_vk(Wk::F3), Key::F3);
        assert_eq!(winit_to_vk(Wk::ArrowUp), Key::ArrowUp);
    }
}
