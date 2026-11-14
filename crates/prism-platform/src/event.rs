//! Window-library-independent event and key types.
//!
//! Both window backends (winit, Win32) translate their raw events into the
//! [`PlatformEvent`] here, and `prism-app` consumes only this layer — so with
//! `winit-backend` disabled no winit type takes part in compilation.
//!
//! [`Key`] models the physical keys the engine actually uses (WASD/QE/T, the
//! number row, arrow keys, modifiers, F1–F12), plus [`Key::Letter`] for the rest of
//! the alphabet — projects bind `M` / `R` / `V` themselves, so a letter must not
//! degrade to a backend-specific raw code. Every other key lands in [`Key::Other`]
//! with the **backend raw code** as payload (winit discriminant / Win32 virtual key
//! code `VK_*`) instead of being silently dropped.

/// A physical key (window-library independent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    KeyW,
    KeyA,
    KeyS,
    KeyD,
    KeyQ,
    KeyE,
    /// `T` — the tonemapping toggle used for PBR debugging.
    KeyT,
    Space,
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    Escape,
    Tab,
    Enter,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    /// A letter key without a dedicated variant, as an ASCII `b'A'..=b'Z'`.
    ///
    /// Letters are the one key group projects bind freely, so they get a
    /// backend-independent payload: both backends produce `Letter(b'M')` and a
    /// project never writes a Win32 virtual-key literal. WASD/Q/E/T keep their own
    /// variants because the engine's camera system looks them up by `KeyCode`.
    Letter(u8),
    /// An unmodeled physical key.
    ///
    /// The payload is the **backend raw code** (winit discriminant / Win32 `VK_*`).
    Other(u32),
}

/// Win32 virtual key codes (the `wParam` of `WM_KEYDOWN` / `WM_KEYUP`).
///
/// Only the constants used by [`Key::from_vk`] are copied here, avoiding pulling in
/// the whole `windows-sys` module for a few numbers; the values match `winuser.h`.
mod vk {
    pub const VK_TAB: u32 = 0x09;
    pub const VK_RETURN: u32 = 0x0D;
    pub const VK_SHIFT: u32 = 0x10;
    pub const VK_CONTROL: u32 = 0x11;
    pub const VK_MENU: u32 = 0x12;
    pub const VK_ESCAPE: u32 = 0x1B;
    pub const VK_SPACE: u32 = 0x20;
    pub const VK_LEFT: u32 = 0x25;
    pub const VK_UP: u32 = 0x26;
    pub const VK_RIGHT: u32 = 0x27;
    pub const VK_DOWN: u32 = 0x28;
    pub const VK_LSHIFT: u32 = 0xA0;
    pub const VK_RSHIFT: u32 = 0xA1;
    pub const VK_LCONTROL: u32 = 0xA2;
    pub const VK_RCONTROL: u32 = 0xA3;
    pub const VK_LMENU: u32 = 0xA4;
    pub const VK_RMENU: u32 = 0xA5;
    pub const VK_F1: u32 = 0x70;
}

impl Key {
    /// Maps a Win32 virtual key code to a neutral key.
    ///
    /// A pure numeric table (no `windows-sys` dependency), so it can be unit-tested
    /// on non-Windows hosts too.
    ///
    /// Known limitation: `WM_KEYDOWN` reports the generic `VK_SHIFT` / `VK_CONTROL` /
    /// `VK_MENU` for Shift/Ctrl/Alt, which cannot distinguish left from right, so both
    /// are treated as the left key.
    pub fn from_vk(vk: u32) -> Self {
        match vk {
            vk::VK_TAB => Key::Tab,
            vk::VK_RETURN => Key::Enter,
            vk::VK_SHIFT | vk::VK_LSHIFT => Key::ShiftLeft,
            vk::VK_RSHIFT => Key::ShiftRight,
            vk::VK_CONTROL | vk::VK_LCONTROL => Key::ControlLeft,
            vk::VK_RCONTROL => Key::ControlRight,
            vk::VK_MENU | vk::VK_LMENU => Key::AltLeft,
            vk::VK_RMENU => Key::AltRight,
            vk::VK_ESCAPE => Key::Escape,
            vk::VK_SPACE => Key::Space,
            vk::VK_LEFT => Key::ArrowLeft,
            vk::VK_UP => Key::ArrowUp,
            vk::VK_RIGHT => Key::ArrowRight,
            vk::VK_DOWN => Key::ArrowDown,
            vk::VK_F1..=0x7B => match vk - vk::VK_F1 {
                0 => Key::F1,
                1 => Key::F2,
                2 => Key::F3,
                3 => Key::F4,
                4 => Key::F5,
                5 => Key::F6,
                6 => Key::F7,
                7 => Key::F8,
                8 => Key::F9,
                9 => Key::F10,
                10 => Key::F11,
                _ => Key::F12,
            },
            // Digit row: the `VK_*` codes are the ASCII codes for `0`-`9`.
            0x30..=0x39 => match vk - 0x30 {
                0 => Key::Digit0,
                1 => Key::Digit1,
                2 => Key::Digit2,
                3 => Key::Digit3,
                4 => Key::Digit4,
                5 => Key::Digit5,
                6 => Key::Digit6,
                7 => Key::Digit7,
                8 => Key::Digit8,
                _ => Key::Digit9,
            },
            0x41..=0x5A => match vk {
                0x57 => Key::KeyW,
                0x41 => Key::KeyA,
                0x53 => Key::KeyS,
                0x44 => Key::KeyD,
                0x51 => Key::KeyQ,
                0x45 => Key::KeyE,
                0x54 => Key::KeyT,
                other => Key::Letter(other as u8),
            },
            other => Key::Other(other),
        }
    }

    /// Returns this key's Win32 virtual key code.
    ///
    /// [`Key::Other`] returns its raw code; all others return the canonical `VK_*`
    /// (left/right modifiers use distinct codes such as `VK_LSHIFT` / `VK_RSHIFT`).
    /// Used because the engine's `KeyCode` has no dedicated variants for F1–F12, so
    /// a stable numeric payload is needed when forwarding.
    pub fn to_vk(self) -> u32 {
        match self {
            Key::Tab => 0x09,
            Key::Enter => 0x0D,
            Key::ShiftLeft => 0xA0,
            Key::ShiftRight => 0xA1,
            Key::ControlLeft => 0xA2,
            Key::ControlRight => 0xA3,
            Key::AltLeft => 0xA4,
            Key::AltRight => 0xA5,
            Key::Escape => 0x1B,
            Key::Space => 0x20,
            Key::ArrowLeft => 0x25,
            Key::ArrowUp => 0x26,
            Key::ArrowRight => 0x27,
            Key::ArrowDown => 0x28,
            Key::Digit0 => 0x30,
            Key::Digit1 => 0x31,
            Key::Digit2 => 0x32,
            Key::Digit3 => 0x33,
            Key::Digit4 => 0x34,
            Key::Digit5 => 0x35,
            Key::Digit6 => 0x36,
            Key::Digit7 => 0x37,
            Key::Digit8 => 0x38,
            Key::Digit9 => 0x39,
            Key::KeyA => 0x41,
            Key::KeyD => 0x44,
            Key::KeyE => 0x45,
            Key::KeyQ => 0x51,
            Key::KeyS => 0x53,
            Key::KeyT => 0x54,
            Key::KeyW => 0x57,
            Key::Letter(byte) => byte as u32,
            Key::F1 => 0x70,
            Key::F2 => 0x71,
            Key::F3 => 0x72,
            Key::F4 => 0x73,
            Key::F5 => 0x74,
            Key::F6 => 0x75,
            Key::F7 => 0x76,
            Key::F8 => 0x77,
            Key::F9 => 0x78,
            Key::F10 => 0x79,
            Key::F11 => 0x7A,
            Key::F12 => 0x7B,
            Key::Other(code) => code,
        }
    }
}

/// Modifier-key state (an abstraction over `WindowEvent::ModifiersChanged` /
/// `GetKeyState`).
///
/// Neither `KeyboardInput` nor `WM_KEYDOWN` carries modifiers, so the PBR component
/// debug shortcuts (Shift+1..5) can only be distinguished through this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
}

impl Modifiers {
    pub const fn empty() -> Self {
        Self {
            shift: false,
            control: false,
            alt: false,
        }
    }
}

/// A window-library-independent snapshot of a platform event.
///
/// `prism-app` and host extensions should consume this type instead of depending on
/// winit's `WindowEvent` directly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlatformEvent {
    CloseRequested,
    Focused(bool),
    Resized {
        width: u32,
        height: u32,
    },
    CursorMoved {
        /// Physical-pixel coordinates within the window client area.
        x: f64,
        y: f64,
    },
    /// **Raw device delta** under pointer lock (relative motion, not absolute
    /// coordinates).
    ///
    /// The winit backend gets this from `DeviceEvent::MouseMotion`; the Win32 backend
    /// from "hide + recenter every frame" mouse capture. This is the only event with
    /// delta semantics.
    MouseMotion {
        dx: f64,
        dy: f64,
    },
    MouseButton {
        button: u16,
        pressed: bool,
    },
    /// Vertical scroll amount in **wheel lines** (one traditional wheel notch = 1.0).
    ///
    /// On some platforms / devices winit reports a pixel delta (trackpads, precision
    /// wheels); the platform layer converts it to lines using
    /// [`PIXELS_PER_SCROLL_LINE`], so consumers can treat "1.0 ≈ one notch"
    /// uniformly without knowing the source device.
    MouseWheel {
        delta: f64,
    },
    Key {
        key: Key,
        pressed: bool,
    },
    /// A modifier-key change (current Shift/Ctrl/Alt snapshot).
    Modifiers(Modifiers),
    Touch {
        /// Touch-point id (stable from finger-down to finger-up; used to tell
        /// multi-touch contacts apart).
        id: u64,
        x: f64,
        y: f64,
        phase: TouchPhase,
    },
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// Conversion constant from pixel delta to wheel lines (50 px ≈ one notch).
///
/// [`MouseScrollDelta`](https://docs.rs/winit/latest/winit/event/enum.MouseScrollDelta.html)
/// has two variants with different units: a traditional wheel reports `LineDelta`
/// (one notch = `1.0`) while a trackpad / high-precision wheel reports `PixelDelta`
/// (one notch is typically tens to hundreds of pixels). Mixing the two directly
/// would make the same zoom 60–120× faster on one platform than another, so the
/// platform layer converts here and `PlatformEvent::MouseWheel`'s `delta` and
/// `InputManager::scroll_delta` are always in lines.
///
/// 50 is an empirical value (Windows' `WHEEL_DELTA` and common trackpad step sizes
/// fall in this range), not a protocol constant of any platform; only the contract
/// that "the same wheel notch converts to the same result in either unit" must
/// hold — changing this number does not change the feel definition of the zoom
/// curve.
pub const PIXELS_PER_SCROLL_LINE: f64 = 50.0;

/// Converts a pixel-level scroll delta into wheel lines (fractions kept, so
/// trackpad scrolling accumulates smoothly).
pub fn scroll_lines_from_pixels(pixels: f64) -> f64 {
    pixels / PIXELS_PER_SCROLL_LINE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The VK map is hand-written: spot-check movement / modifier / function / digit keys.
    #[test]
    fn win32_virtual_keys_map_to_native_keys() {
        assert_eq!(Key::from_vk(0x57), Key::KeyW);
        assert_eq!(Key::from_vk(0x41), Key::KeyA);
        assert_eq!(Key::from_vk(0x53), Key::KeyS);
        assert_eq!(Key::from_vk(0x44), Key::KeyD);
        assert_eq!(Key::from_vk(0x51), Key::KeyQ);
        assert_eq!(Key::from_vk(0x45), Key::KeyE);
        assert_eq!(Key::from_vk(0x54), Key::KeyT);
        assert_eq!(Key::from_vk(0x20), Key::Space);
        assert_eq!(Key::from_vk(0x10), Key::ShiftLeft);
        assert_eq!(Key::from_vk(0xA1), Key::ShiftRight);
        assert_eq!(Key::from_vk(0x11), Key::ControlLeft);
        assert_eq!(Key::from_vk(0x1B), Key::Escape);
        assert_eq!(Key::from_vk(0x09), Key::Tab);
        assert_eq!(Key::from_vk(0x0D), Key::Enter);
        assert_eq!(Key::from_vk(0x25), Key::ArrowLeft);
        assert_eq!(Key::from_vk(0x28), Key::ArrowDown);
        for (vk, expected) in [
            (0x30u32, Key::Digit0),
            (0x31, Key::Digit1),
            (0x35, Key::Digit5),
            (0x39, Key::Digit9),
        ] {
            assert_eq!(Key::from_vk(vk), expected, "VK 0x{vk:02x}");
        }
        assert_eq!(Key::from_vk(0x72), Key::F3);
        assert_eq!(Key::from_vk(0x7B), Key::F12);
    }

    /// `to_vk` and `from_vk` must be inverses (`prism-app` uses `to_vk` to build the
    /// numeric payload for the engine's `KeyCode::Other`; a mapping drift
    /// misidentifies keys).
    #[test]
    fn key_round_trips_through_virtual_key_codes() {
        for key in [
            Key::KeyW,
            Key::KeyA,
            Key::KeyS,
            Key::KeyD,
            Key::KeyQ,
            Key::KeyE,
            Key::KeyT,
            Key::Space,
            Key::ShiftLeft,
            Key::ShiftRight,
            Key::ControlLeft,
            Key::ControlRight,
            Key::AltLeft,
            Key::AltRight,
            Key::Escape,
            Key::Tab,
            Key::Enter,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::Digit0,
            Key::Digit1,
            Key::Digit2,
            Key::Digit3,
            Key::Digit4,
            Key::Digit5,
            Key::Digit6,
            Key::Digit7,
            Key::Digit8,
            Key::Digit9,
            Key::F1,
            Key::F6,
            Key::F12,
        ] {
            assert_eq!(Key::from_vk(key.to_vk()), key, "{key:?}");
        }
        assert_eq!(Key::Other(0x24).to_vk(), 0x24);
    }

    /// Unmodeled keys must not be dropped: they degrade to the backend raw code.
    #[test]
    fn unmapped_virtual_keys_fall_back_to_raw_code() {
        assert_eq!(Key::from_vk(0x5A), Key::Letter(b'Z')); // 'Z'
        assert_eq!(Key::from_vk(0x24), Key::Other(0x24)); // VK_HOME
        assert_eq!(Key::from_vk(0x7C), Key::Other(0x7C)); // VK_F13
    }

    /// Regression guard: one wheel notch must reach the engine as 1.0 line in either
    /// unit, or zoom / movement speed would differ 60–120× by platform.
    #[test]
    fn one_notch_is_one_line_in_either_unit() {
        let line = 1.0;
        let pixel = scroll_lines_from_pixels(PIXELS_PER_SCROLL_LINE);
        assert!(
            (line - pixel).abs() < 1e-9,
            "line delta {line} and one line worth of pixels {pixel} must agree"
        );
        assert_eq!(scroll_lines_from_pixels(100.0), 2.0);
        assert_eq!(scroll_lines_from_pixels(-75.0), -1.5);
    }
}
