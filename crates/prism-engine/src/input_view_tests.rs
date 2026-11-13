use super::*;
use crate::input::{ElementState, KeyCode, MouseButton};

fn manager_with_w() -> InputManager {
    let mut manager = InputManager::new();
    manager.handle_keyboard(KeyCode::KeyW, ElementState::Pressed);
    manager
}

#[test]
fn get_key_maps_to_held() {
    let manager = manager_with_w();
    let input = Input::new(&manager);
    assert!(input.get_key(KeyCode::KeyW));
    assert!(!input.get_key(KeyCode::KeyA));
}

#[test]
fn get_key_down_and_up_are_transient() {
    let mut manager = manager_with_w();
    {
        let input = Input::new(&manager);
        assert!(input.get_key_down(KeyCode::KeyW));
        assert!(!input.get_key_up(KeyCode::KeyW));
    }

    manager.begin_frame();
    {
        let input = Input::new(&manager);
        assert!(!input.get_key_down(KeyCode::KeyW));
    }

    manager.handle_keyboard(KeyCode::KeyW, ElementState::Released);
    let input = Input::new(&manager);
    assert!(input.get_key_up(KeyCode::KeyW));
    assert!(!input.get_key(KeyCode::KeyW));
}

#[test]
fn mouse_buttons_track_held_down_and_up() {
    let mut manager = InputManager::new();
    manager.handle_mouse_button(MouseButton::Left, ElementState::Pressed);
    {
        let input = Input::new(&manager);
        assert!(input.get_mouse_button(MouseButton::Left));
        assert!(input.get_mouse_button_down(MouseButton::Left));
        assert!(!input.get_mouse_button_up(MouseButton::Left));
        assert!(!input.get_mouse_button(MouseButton::Right));
    }

    manager.begin_frame();
    manager.handle_mouse_button(MouseButton::Left, ElementState::Released);
    let input = Input::new(&manager);
    assert!(!input.get_mouse_button(MouseButton::Left));
    assert!(input.get_mouse_button_up(MouseButton::Left));
}

#[test]
fn any_key_covers_keyboard_and_mouse() {
    let empty = InputManager::new();
    let input = Input::new(&empty);
    assert!(!input.any_key());
    assert!(!input.any_key_down());

    let keyboard = manager_with_w();
    let input = Input::new(&keyboard);
    assert!(input.any_key());
    assert!(input.any_key_down());

    let mut mouse = InputManager::new();
    mouse.handle_mouse_button(MouseButton::Right, ElementState::Pressed);
    let input = Input::new(&mouse);
    assert!(input.any_key());
    assert!(input.any_key_down());
}

#[test]
fn mouse_position_and_scroll_delta() {
    let mut manager = InputManager::new();
    manager.set_mouse_position([100.0, 250.0]);
    manager.handle_scroll(-2.0);
    let input = Input::new(&manager);
    assert_eq!(input.mouse_position(), [100.0, 250.0]);
    assert_eq!(input.mouse_scroll_delta(), [0.0, -2.0]);
}

#[test]
fn get_axis_supports_the_three_mouse_axes() {
    let mut manager = InputManager::new();
    manager.set_mouse_position([10.0, 10.0]);
    manager.handle_mouse_move([13.0, 6.0]);
    manager.handle_scroll(1.0);
    let input = Input::new(&manager);
    assert_eq!(input.get_axis("Mouse X"), 3.0);
    assert_eq!(input.get_axis("Mouse Y"), -4.0);
    assert_eq!(input.get_axis("Mouse ScrollWheel"), 1.0);
    assert_eq!(input.get_axis("Horizontal"), 0.0);
    assert_eq!(input.get_axis_raw("Mouse X"), 3.0);
}
