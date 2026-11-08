/// 抽象按键码（平台无关）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    KeyW,
    KeyA,
    KeyS,
    KeyD,
    KeyQ,
    KeyE,
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
    Other(u32),
}

impl KeyCode {
    /// 稳定的按键位编号，用于宿主下发给项目模块的 `InputSnapshot` 位图。
    ///
    /// 这是**跨模块 ABI 的一部分**：数字一旦发布就不能重排，否则已编译的
    /// 项目模块会把按键认错。新增按键请追加到末尾。
    /// `None` = 这个键码放不进 `InputSnapshot` 的位图（见 [`Self::Other`] 分支）。
    pub fn bit_index(self) -> Option<usize> {
        Some(match self {
            Self::KeyW => 0,
            Self::KeyA => 1,
            Self::KeyS => 2,
            Self::KeyD => 3,
            Self::KeyQ => 4,
            Self::KeyE => 5,
            Self::Space => 6,
            Self::ShiftLeft => 7,
            Self::ShiftRight => 8,
            Self::ControlLeft => 9,
            Self::ControlRight => 10,
            Self::AltLeft => 11,
            Self::AltRight => 12,
            Self::Escape => 13,
            Self::Tab => 14,
            Self::Enter => 15,
            Self::ArrowUp => 16,
            Self::ArrowDown => 17,
            Self::ArrowLeft => 18,
            Self::ArrowRight => 19,
            Self::Digit0 => 20,
            Self::Digit1 => 21,
            Self::Digit2 => 22,
            Self::Digit3 => 23,
            Self::Digit4 => 24,
            Self::Digit5 => 25,
            Self::Digit6 => 26,
            Self::Digit7 => 27,
            Self::Digit8 => 28,
            Self::Digit9 => 29,
            // 位图 30..=255 一共 226 个槽位，只放得下 vk < 226 的原始键码。
            //
            // 超出的键码必须**丢弃**，不能取模：取模会让 vk 226 和 vk 452 落在
            // 同一个位上，玩家按一个冷门键却看到另一个键被按住，比丢键难查得多。
            Self::Other(code) if (code as usize) < 226 => 30 + code as usize,
            Self::Other(_) => return None,
        })
    }
}

/// Key or button state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementState {
    Pressed,
    Released,
}

/// Mouse button abstraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u16),
}

/// Touch phase (platform-independent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// A single touch event.
#[derive(Clone, Copy, Debug)]
pub struct TouchEvent {
    pub id: u64,
    pub phase: TouchPhase,
    pub position: [f64; 2],
}

/// Per-frame input state.
///
/// Owns the raw input state (held keys, mouse position, transient just-pressed
/// events) and the pointer-lock state machine.  Keyboard shortcuts that trigger
/// app- or editor-level actions are handled by the caller — this struct only
/// tracks which keys are pressed.
///
/// ## Usage
/// 1. Call individual handlers (`handle_keyboard`, `handle_mouse_button`,
///    `handle_mouse_move`, `handle_scroll`, `handle_touch`) from the app's
///    event handler to route window-system events into the state machine.
/// 2. Call [`begin_frame`](Self::begin_frame) at the end of each frame to
///    clear transient state.
/// 3. Query helpers (`key_held`, `mouse_delta`, etc.) during the frame's
///    update logic.
#[derive(Default, Clone)]
pub struct InputManager {
    // Persistent (accumulated across frames)
    keys_held: rustc_hash::FxHashSet<KeyCode>,
    mouse_buttons_held: rustc_hash::FxHashSet<MouseButton>,
    mouse_position: [f64; 2],

    // Transient (cleared each frame by begin_frame)
    keys_just_pressed: Vec<KeyCode>,
    keys_just_released: Vec<KeyCode>,
    mouse_just_pressed: Vec<MouseButton>,
    mouse_just_released: Vec<MouseButton>,
    mouse_delta: [f64; 2],
    scroll_delta: f64,
    touches: Vec<TouchEvent>,

    // Pointer-lock state
    /// Whether the cursor is hidden + grabbed for FPS-style look.
    pub pointer_locked: bool,
    /// Whether ALT is held and has temporarily released a locked pointer.
    pub alt_temp_release: bool,
    /// Set when the window regains focus; the next left-click focuses the
    /// window instead of entering pointer lock.
    pub focus_return_click: bool,
    /// Whether the pointer was locked before the inspector/visualizer was
    /// opened, so it can be re-locked when the UI closes.
    pub lock_before_inspector: bool,
}

impl InputManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call at the START of each frame to reset transient state.
    pub fn begin_frame(&mut self) {
        self.keys_just_pressed.clear();
        self.keys_just_released.clear();
        self.mouse_just_pressed.clear();
        self.mouse_just_released.clear();
        self.mouse_delta = [0.0; 2];
        self.scroll_delta = 0.0;
        self.touches.clear();
    }

    // --- Query helpers ---

    pub fn key_held(&self, key: KeyCode) -> bool {
        self.keys_held.contains(&key)
    }
    pub fn key_just_pressed(&self, key: KeyCode) -> bool {
        self.keys_just_pressed.contains(&key)
    }

    /// 本帧新按下的所有按键（去重，已按下又按不会重复出现）。
    pub fn pressed_keys(&self) -> &[KeyCode] {
        &self.keys_just_pressed
    }

    /// 当前按住的所有按键（包含本帧刚按下的）。
    ///
    /// 宿主下发给项目模块的 `InputSnapshot` 用按住语义：gameplay 代码的
    /// `input.key(...)` 检测的是持续按住，而不是只在按下那一帧为真。
    pub fn held_keys(&self) -> impl Iterator<Item = KeyCode> + '_ {
        self.keys_held.iter().copied()
    }
    pub fn key_just_released(&self, key: KeyCode) -> bool {
        self.keys_just_released.contains(&key)
    }

    /// 本帧松开的所有按键（宿主填 `InputSnapshot::key_edges` 用）。
    pub fn released_keys(&self) -> &[KeyCode] {
        &self.keys_just_released
    }
    pub fn mouse_held(&self, button: MouseButton) -> bool {
        self.mouse_buttons_held.contains(&button)
    }
    pub fn mouse_just_pressed(&self, button: MouseButton) -> bool {
        self.mouse_just_pressed.contains(&button)
    }

    /// 本帧刚按下的所有鼠标键（`Input.anyKeyDown` 的鼠标部分）。
    pub fn buttons_just_pressed(&self) -> &[MouseButton] {
        &self.mouse_just_pressed
    }

    /// 鼠标键本帧刚释放（`Input.GetMouseButtonUp`）。
    pub fn mouse_just_released(&self, button: MouseButton) -> bool {
        self.mouse_just_released.contains(&button)
    }

    /// 当前是否有任意键按住（`Input.anyKey` 的键盘部分）。
    pub fn any_key_held(&self) -> bool {
        !self.keys_held.is_empty()
    }

    /// 当前是否有任意鼠标键按住。
    pub fn any_mouse_button_held(&self) -> bool {
        !self.mouse_buttons_held.is_empty()
    }
    pub fn mouse_delta(&self) -> [f64; 2] {
        self.mouse_delta
    }
    /// 本帧累计的垂直滚动量，**单位是「滚轮行」**（一格传统滚轮 = 1.0，
    /// 触控板的小数行会平滑累积）。
    ///
    /// 平台层已经把 winit 的像素级增量折算成行（见
    /// `prism_platform::PIXELS_PER_SCROLL_LINE`），所以缩放/滚动曲线可以按
    /// 「1.0 ≈ 一格」写，不必再区分设备。
    pub fn scroll_delta(&self) -> f64 {
        self.scroll_delta
    }
    pub fn mouse_position(&self) -> [f64; 2] {
        self.mouse_position
    }
    pub fn touches(&self) -> &[TouchEvent] {
        &self.touches
    }

    // --- Low-level event handlers (insert raw data) ---

    pub fn handle_keyboard(&mut self, key: KeyCode, state: ElementState) {
        match state {
            ElementState::Pressed => {
                if self.keys_held.insert(key) {
                    self.keys_just_pressed.push(key);
                }
            }
            ElementState::Released => {
                if self.keys_held.remove(&key) {
                    self.keys_just_released.push(key);
                }
            }
        }
    }

    pub fn handle_mouse_move(&mut self, position: [f64; 2]) {
        self.mouse_delta[0] += position[0] - self.mouse_position[0];
        self.mouse_delta[1] += position[1] - self.mouse_position[1];
        self.mouse_position = position;
    }

    /// Set the pointer position without accumulating delta.
    pub fn set_mouse_position(&mut self, position: [f64; 2]) {
        self.mouse_position = position;
    }

    pub fn handle_mouse_button(&mut self, button: MouseButton, state: ElementState) {
        match state {
            ElementState::Pressed => {
                if self.mouse_buttons_held.insert(button) {
                    self.mouse_just_pressed.push(button);
                }
            }
            ElementState::Released => {
                if self.mouse_buttons_held.remove(&button) {
                    self.mouse_just_released.push(button);
                }
            }
        }
    }

    /// Handle a scroll event. `delta_y` is positive for scroll up (zoom in),
    /// 负 for 滚动 下。单位是滚轮行（一格 = 1.0）。
    pub fn handle_scroll(&mut self, delta_y: f64) {
        self.scroll_delta += delta_y;
    }

    pub fn handle_touch(&mut self, id: u64, phase: TouchPhase, position: [f64; 2]) {
        self.touches.push(TouchEvent {
            id,
            phase,
            position,
        });
    }

    /// Update the pointer-lock state without performing any window-system
    /// operations (cursor grab / visibility).  The caller is responsible for
    /// adjusting the actual cursor state via the window system.
    ///
    /// Resets accumulated mouse delta so the view doesn't jump after the
    /// transition.
    pub fn set_locked(&mut self, locked: bool) {
        self.pointer_locked = locked;
        self.begin_frame();
    }
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;

#[cfg(test)]
mod abi_key_tests {
    use super::KeyCode;
    use prism_project_api::keys;

    /// 键位编号是宿主/项目共用的 ABI：改 `bit_index` 必须同步更新
    /// `prism_project_api::keys`，否则动态模块读到的输入会错位。
    #[test]
    fn abi_key_table_matches_bit_index() {
        assert_eq!(KeyCode::KeyW.bit_index(), Some(keys::KEY_W));
        assert_eq!(KeyCode::KeyA.bit_index(), Some(keys::KEY_A));
        assert_eq!(KeyCode::KeyS.bit_index(), Some(keys::KEY_S));
        assert_eq!(KeyCode::KeyD.bit_index(), Some(keys::KEY_D));
        assert_eq!(KeyCode::KeyQ.bit_index(), Some(keys::KEY_Q));
        assert_eq!(KeyCode::KeyE.bit_index(), Some(keys::KEY_E));
        assert_eq!(KeyCode::Space.bit_index(), Some(keys::SPACE));
        assert_eq!(KeyCode::ShiftLeft.bit_index(), Some(keys::SHIFT_LEFT));
        assert_eq!(KeyCode::ShiftRight.bit_index(), Some(keys::SHIFT_RIGHT));
        assert_eq!(KeyCode::ControlLeft.bit_index(), Some(keys::CONTROL_LEFT));
        assert_eq!(KeyCode::ControlRight.bit_index(), Some(keys::CONTROL_RIGHT));
        assert_eq!(KeyCode::AltLeft.bit_index(), Some(keys::ALT_LEFT));
        assert_eq!(KeyCode::AltRight.bit_index(), Some(keys::ALT_RIGHT));
        assert_eq!(KeyCode::Escape.bit_index(), Some(keys::ESCAPE));
        assert_eq!(KeyCode::Tab.bit_index(), Some(keys::TAB));
        assert_eq!(KeyCode::Enter.bit_index(), Some(keys::ENTER));
        assert_eq!(KeyCode::ArrowUp.bit_index(), Some(keys::ARROW_UP));
        assert_eq!(KeyCode::ArrowDown.bit_index(), Some(keys::ARROW_DOWN));
        assert_eq!(KeyCode::ArrowLeft.bit_index(), Some(keys::ARROW_LEFT));
        assert_eq!(KeyCode::ArrowRight.bit_index(), Some(keys::ARROW_RIGHT));
        assert_eq!(KeyCode::Digit0.bit_index(), Some(keys::DIGIT_0));
        assert_eq!(KeyCode::Other(5).bit_index(), Some(keys::OTHER_BASE + 5));
    }

    /// 不同键码不能共用一个位：#986 之前 `Other(code)` 取模 226，
    /// vk 226 和 vk 452 都落到 30，按一个冷门键会报出另一个键。
    #[test]
    fn other_key_codes_never_alias_each_other() {
        let mut seen = [None; 256];
        for code in 0u32..226 {
            let index = KeyCode::Other(code)
                .bit_index()
                .expect("code < 226 必须放得进位图");
            assert!(index < 256, "code {code} → {index} 越界");
            assert!(
                seen[index].is_none(),
                "code {code} 与 {:?} 抢同一个位 {index}",
                seen[index]
            );
            seen[index] = Some(code);
        }
    }

    /// 位图只有 226 个剩余槽位：超出范围必须**丢弃**（`None`），不能折叠。
    #[test]
    fn other_key_codes_above_the_table_are_dropped() {
        for code in [226u32, 227, 255, 452, 1000, u32::MAX] {
            assert_eq!(
                KeyCode::Other(code).bit_index(),
                None,
                "code {code} 不该被折叠成别的键"
            );
        }
    }

    /// 位图边界：最大可表示键码正好落在最后一个 bit 上。
    #[test]
    fn the_last_representable_key_fills_the_last_bit() {
        assert_eq!(KeyCode::Other(225).bit_index(), Some(255));
        let mut snapshot = prism_project_api::InputSnapshot::default();
        snapshot.set_key(255, true);
        assert_eq!(
            snapshot.keys[3] >> 63,
            1,
            "第 255 位应在最后一个 word 的最高位"
        );
    }
}

// ---------------------------------------------------------------------------
// 跨 ABI 的输入快照（#589）
// ---------------------------------------------------------------------------

/// 把引擎的 [`InputManager`] 折成一份 [`InputSnapshot`]。
///
/// `screen` 是绘图区尺寸（像素，与 `cursor` 同一坐标系）。静态 Player 的
/// `GameWorld::input()` 和编辑器插件形态共用这一份映射，避免两边字段语义漂移。
pub fn input_snapshot(input: &InputManager, screen: [f32; 2]) -> prism_project_api::InputSnapshot {
    use prism_project_api::{InputSnapshot, TouchSnapshot};

    let mut snapshot = InputSnapshot::default();
    let cursor = input.mouse_position();
    snapshot.cursor = [cursor[0] as f32, cursor[1] as f32];
    snapshot.screen = screen;
    snapshot.left_clicked = input.mouse_just_pressed(MouseButton::Left);
    snapshot.left_held = input.mouse_held(MouseButton::Left);
    snapshot.right_clicked = input.mouse_just_pressed(MouseButton::Right);
    snapshot.right_held = input.mouse_held(MouseButton::Right);
    snapshot.middle_clicked = input.mouse_just_pressed(MouseButton::Middle);
    snapshot.middle_held = input.mouse_held(MouseButton::Middle);
    let delta = input.mouse_delta();
    snapshot.mouse_delta = [delta[0] as f32, delta[1] as f32];
    snapshot.scroll = input.scroll_delta() as f32;
    snapshot.pointer_locked = input.pointer_locked;

    // 放不进位图的键码要丢得干净：宁可少一个键，也不能让它冒充别的键。
    let mut dropped: Option<u32> = None;
    for key in input.held_keys() {
        match key.bit_index() {
            Some(index) => snapshot.set_key(index, true),
            None => {
                if let KeyCode::Other(code) = key {
                    dropped = dropped.or(Some(code));
                }
            }
        }
    }
    for key in input.pressed_keys() {
        if let Some(index) = key.bit_index() {
            snapshot.set_key_edge(index, true);
        }
    }
    for key in input.released_keys() {
        if let Some(index) = key.bit_index() {
            snapshot.set_key_edge(index, false);
        }
    }
    if let Some(code) = dropped {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            log::warn!(
                "input: key code {code} does not fit the InputSnapshot bitmap                  (30 + code must stay below 256); keys above 225 are ignored,                  later codes are not logged"
            )
        });
    }

    let touches = input.touches();
    let count = touches.len().min(InputSnapshot::MAX_TOUCHES);
    snapshot.touch_count = count as u32;
    for (slot, touch) in snapshot.touches.iter_mut().zip(touches.iter()).take(count) {
        *slot = TouchSnapshot {
            id: touch.id,
            position: [touch.position[0] as f32, touch.position[1] as f32],
            phase: touch_phase_code(touch.phase),
            _reserved: 0,
        };
    }
    // `keys` 位图与 `KeyCode::bit_index` 是同一套编号（见上面的 ABI 注释）。
    snapshot
}

/// 引擎触点阶段 → ABI 常量。
fn touch_phase_code(phase: TouchPhase) -> u32 {
    use prism_project_api::{
        TOUCH_PHASE_CANCELLED, TOUCH_PHASE_ENDED, TOUCH_PHASE_MOVED, TOUCH_PHASE_STARTED,
    };
    match phase {
        TouchPhase::Started => TOUCH_PHASE_STARTED,
        TouchPhase::Moved => TOUCH_PHASE_MOVED,
        TouchPhase::Ended => TOUCH_PHASE_ENDED,
        TouchPhase::Cancelled => TOUCH_PHASE_CANCELLED,
    }
}
