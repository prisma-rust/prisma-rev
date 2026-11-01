use super::*;

fn key_w() -> KeyCode {
    KeyCode::KeyW
}
fn key_space() -> KeyCode {
    KeyCode::Space
}

#[test]
fn new_is_empty() {
    let s = InputManager::new();
    assert!(!s.key_held(KeyCode::KeyW));
    assert!(!s.mouse_held(MouseButton::Left));
    assert_eq!(s.mouse_delta(), [0.0; 2]);
    assert_eq!(s.scroll_delta(), 0.0);
    assert_eq!(s.mouse_position(), [0.0; 2]);
    assert!(s.touches().is_empty());
}

#[test]
fn key_press_adds_held_and_just_pressed() {
    let mut s = InputManager::new();
    s.handle_keyboard(key_w(), ElementState::Pressed);
    assert!(s.key_held(KeyCode::KeyW));
    assert!(s.key_just_pressed(KeyCode::KeyW));
}

#[test]
fn held_keys_lists_pressed_keys_until_release() {
    let mut s = InputManager::new();
    s.handle_keyboard(KeyCode::KeyW, ElementState::Pressed);
    s.handle_keyboard(KeyCode::Space, ElementState::Pressed);
    s.begin_frame();
    let held: Vec<KeyCode> = s.held_keys().collect();
    assert!(held.contains(&KeyCode::KeyW));
    assert!(held.contains(&KeyCode::Space));

    s.handle_keyboard(KeyCode::KeyW, ElementState::Released);
    let held: Vec<KeyCode> = s.held_keys().collect();
    assert!(!held.contains(&KeyCode::KeyW));
    assert!(held.contains(&KeyCode::Space));
}

#[test]
fn key_held_survives_begin_frame() {
    let mut s = InputManager::new();
    s.handle_keyboard(key_w(), ElementState::Pressed);
    s.begin_frame();
    assert!(s.key_held(KeyCode::KeyW));
    assert!(!s.key_just_pressed(KeyCode::KeyW)); // transient cleared
}

#[test]
fn key_just_released_on_release() {
    let mut s = InputManager::new();
    s.handle_keyboard(key_w(), ElementState::Pressed);
    s.begin_frame();
    s.handle_keyboard(key_w(), ElementState::Released);
    assert!(!s.key_held(KeyCode::KeyW));
    assert!(s.key_just_released(KeyCode::KeyW));
}

#[test]
fn duplicate_key_press_does_not_double_just_pressed() {
    let mut s = InputManager::new();
    s.handle_keyboard(key_w(), ElementState::Pressed);
    s.handle_keyboard(key_w(), ElementState::Pressed); // duplicate
    assert!(s.key_held(KeyCode::KeyW));
    assert_eq!(s.keys_just_pressed.len(), 1); // only once
}

#[test]
fn mouse_delta_accumulates_and_resets() {
    let mut s = InputManager::new();
    s.handle_mouse_move([100.0, 200.0]);
    assert_eq!(s.mouse_delta(), [100.0, 200.0]);
    assert_eq!(s.mouse_position(), [100.0, 200.0]);

    s.handle_mouse_move([110.0, 195.0]);
    assert_eq!(s.mouse_delta(), [110.0, 195.0]); // full delta from origin
    assert_eq!(s.mouse_position(), [110.0, 195.0]);

    s.begin_frame();
    assert_eq!(s.mouse_delta(), [0.0, 0.0]);
    assert_eq!(s.mouse_position(), [110.0, 195.0]); // position persists
}

#[test]
fn scroll_delta_y() {
    let mut s = InputManager::new();
    s.handle_scroll(3.0);
    assert!((s.scroll_delta() - 3.0).abs() < 1e-9);
    s.handle_scroll(-1.0);
    assert!((s.scroll_delta() - 2.0).abs() < 1e-9); // accumulated
}

#[test]
fn mouse_button_held_and_just_pressed() {
    let mut s = InputManager::new();
    s.handle_mouse_button(MouseButton::Left, ElementState::Pressed);
    assert!(s.mouse_held(MouseButton::Left));

    s.begin_frame();
    assert!(s.mouse_held(MouseButton::Left));
    assert_eq!(s.mouse_just_pressed.len(), 0); // transient cleared
}

#[test]
fn mouse_button_release_clears_held() {
    let mut s = InputManager::new();
    s.handle_mouse_button(MouseButton::Left, ElementState::Pressed);
    s.begin_frame();
    s.handle_mouse_button(MouseButton::Left, ElementState::Released);
    assert!(!s.mouse_held(MouseButton::Left));
}

#[test]
fn touch_events_accumulate_and_clear() {
    let mut s = InputManager::new();
    s.handle_touch(1, TouchPhase::Started, [10.0, 20.0]);
    s.handle_touch(2, TouchPhase::Moved, [30.0, 40.0]);
    assert_eq!(s.touches().len(), 2);
    assert_eq!(s.touches()[0].id, 1);
    assert_eq!(s.touches()[1].id, 2);

    s.begin_frame();
    assert!(s.touches().is_empty());
}

#[test]
fn begin_frame_clears_all_transient() {
    let mut s = InputManager::new();
    s.handle_keyboard(key_space(), ElementState::Pressed);
    s.handle_mouse_button(MouseButton::Right, ElementState::Pressed);
    s.handle_mouse_move([50.0, 60.0]);
    s.handle_scroll(5.0);

    s.begin_frame();
    assert!(!s.key_just_pressed(KeyCode::Space));
    assert!(s.key_held(KeyCode::Space)); // held persists
    assert_eq!(s.mouse_delta(), [0.0; 2]);
    assert_eq!(s.scroll_delta(), 0.0);
}

/// #589：静态 Player 读的是 `InputSnapshot` 资源，映射必须把按键、边沿、
/// 鼠标位置与滚轮都带上（以前 host 后端只回 `InputSnapshot::default()`）。
#[test]
fn snapshot_carries_cursor_keys_edges_and_scroll() {
    use prism_project_api::keys;

    let mut input = InputManager::new();
    input.handle_mouse_move([120.0, 45.0]);
    input.handle_mouse_button(MouseButton::Left, ElementState::Pressed);
    input.handle_keyboard(KeyCode::Space, ElementState::Pressed);
    input.handle_scroll(1.5);

    let snapshot = input_snapshot(&input, [640.0, 360.0]);
    assert_eq!(snapshot.cursor, [120.0, 45.0]);
    assert_eq!(snapshot.screen, [640.0, 360.0]);
    assert!(snapshot.left_clicked);
    assert!(snapshot.left_held);
    assert_eq!(snapshot.scroll, 1.5);
    assert!(snapshot.key(keys::SPACE));
    assert!(snapshot.key_pressed(keys::SPACE));

    // 下一帧：边沿清掉，按住仍在。
    input.begin_frame();
    let next = input_snapshot(&input, [640.0, 360.0]);
    assert!(next.key(keys::SPACE));
    assert!(!next.key_pressed(keys::SPACE));
    assert!(!next.left_clicked);
}
