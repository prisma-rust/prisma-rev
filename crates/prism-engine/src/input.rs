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
    pub fn bit_index(self) -> usize {
        match self {
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
            // 平台原始键码落在 30..256；位图容量足够，冲突只影响非常用键。
            Self::Other(code) => 30 + (code as usize % 226),
        }
    }
}

/// 调 or 按钮 状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementState {
    Pressed,
    Released,
}

/// 鼠标 按钮 抽象
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u16),
}

/// 触摸 phase (platform-independent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// A single 触摸 事件
#[derive(Clone, Copy, Debug)]
pub struct TouchEvent {
    pub id: u64,
    pub phase: TouchPhase,
    pub position: [f64; 2],
}

/// Per-frame 输入 状态
///
/// Owns the raw 输入 状态 (held keys, 鼠标 position, transient just-pressed
/// events) and the pointer-lock 状态 机 键盘 shortcuts that 触发器
/// app- or editor-level actions are handled by the 调用者 — this 结构体 only
/// tracks which keys are pressed.
///
/// ## 用法
/// 1. 调用 individual handlers (`handle_keyboard`, `handle_mouse_button`,
///    `handle_mouse_move`, `handle_scroll`, `handle_touch`) from the app's
/// 事件 处理器 to route window-system events into the 状态 机
/// 2. 调用 [`begin_frame`](Self::begin_frame) at the 结束 of each 帧 to
/// 清空 transient 状态
/// 3. 查询 helpers (`key_held`, `mouse_delta`, etc.) during the frame's
/// 更新 逻辑
#[derive(Default, Clone)]
pub struct InputManager {
    // Persistent (accumulated across frames)
    keys_held: rustc_hash::FxHashSet<KeyCode>,
    mouse_buttons_held: rustc_hash::FxHashSet<MouseButton>,
    mouse_position: [f64; 2],

    // Transient (cleared each 帧 by begin_frame)
    keys_just_pressed: Vec<KeyCode>,
    keys_just_released: Vec<KeyCode>,
    mouse_just_pressed: Vec<MouseButton>,
    mouse_just_released: Vec<MouseButton>,
    mouse_delta: [f64; 2],
    scroll_delta: f64,
    touches: Vec<TouchEvent>,

    // Pointer-lock 状态
    /// Whether the cursor is 隐藏 + grabbed for FPS-style look.
    pub pointer_locked: bool,
    /// Whether ALT is held and has temporarily released a locked 指针
    pub alt_temp_release: bool,
    /// 集合 when the 窗口 regains focus; the 下一个 left-click focuses the
    /// 窗口 instead of entering 指针 lock.
    pub focus_return_click: bool,
    /// Whether the 指针 was locked before the inspector/visualizer was
    /// opened, so it can be re-locked when the UI closes.
    pub lock_before_inspector: bool,
}

impl InputManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 调用 at the START of each 帧 to reset transient 状态
    pub fn begin_frame(&mut self) {
        self.keys_just_pressed.clear();
        self.keys_just_released.clear();
        self.mouse_just_pressed.clear();
        self.mouse_just_released.clear();
        self.mouse_delta = [0.0; 2];
        self.scroll_delta = 0.0;
        self.touches.clear();
    }

    // --- 查询 helpers ---

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

    // --- Low-level 事件 handlers 插入 raw data) ---

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

    /// 集合 the 指针 position without accumulating delta.
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

    /// Handle a 滚动 事件 `delta_y` is 正 for 滚动 上 (zoom in),
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

    /// 更新 the pointer-lock 状态 without performing any window-system
    /// operations (cursor grab / 可见性 The 调用者 is responsible for
    /// adjusting the actual cursor 状态 via the 窗口 系统
    ///
    /// Resets accumulated 鼠标 delta so the 视图 doesn't jump after the
    /// 过渡
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
        assert_eq!(KeyCode::KeyW.bit_index(), keys::KEY_W);
        assert_eq!(KeyCode::KeyA.bit_index(), keys::KEY_A);
        assert_eq!(KeyCode::KeyS.bit_index(), keys::KEY_S);
        assert_eq!(KeyCode::KeyD.bit_index(), keys::KEY_D);
        assert_eq!(KeyCode::KeyQ.bit_index(), keys::KEY_Q);
        assert_eq!(KeyCode::KeyE.bit_index(), keys::KEY_E);
        assert_eq!(KeyCode::Space.bit_index(), keys::SPACE);
        assert_eq!(KeyCode::ShiftLeft.bit_index(), keys::SHIFT_LEFT);
        assert_eq!(KeyCode::ShiftRight.bit_index(), keys::SHIFT_RIGHT);
        assert_eq!(KeyCode::ControlLeft.bit_index(), keys::CONTROL_LEFT);
        assert_eq!(KeyCode::ControlRight.bit_index(), keys::CONTROL_RIGHT);
        assert_eq!(KeyCode::AltLeft.bit_index(), keys::ALT_LEFT);
        assert_eq!(KeyCode::AltRight.bit_index(), keys::ALT_RIGHT);
        assert_eq!(KeyCode::Escape.bit_index(), keys::ESCAPE);
        assert_eq!(KeyCode::Tab.bit_index(), keys::TAB);
        assert_eq!(KeyCode::Enter.bit_index(), keys::ENTER);
        assert_eq!(KeyCode::ArrowUp.bit_index(), keys::ARROW_UP);
        assert_eq!(KeyCode::ArrowDown.bit_index(), keys::ARROW_DOWN);
        assert_eq!(KeyCode::ArrowLeft.bit_index(), keys::ARROW_LEFT);
        assert_eq!(KeyCode::ArrowRight.bit_index(), keys::ARROW_RIGHT);
        assert_eq!(KeyCode::Digit0.bit_index(), keys::DIGIT_0);
        assert_eq!(KeyCode::Other(5).bit_index(), keys::OTHER_BASE + 5);
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

    for key in input.held_keys() {
        snapshot.set_key(key.bit_index(), true);
    }
    for key in input.pressed_keys() {
        snapshot.set_key_edge(key.bit_index(), true);
    }
    for key in input.released_keys() {
        snapshot.set_key_edge(key.bit_index(), false);
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
