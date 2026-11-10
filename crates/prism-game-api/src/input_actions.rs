use std::collections::BTreeMap;

use prism_project_api::InputSnapshot;

/// The state of a named gameplay action for one simulation step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActionState {
    pub held: bool,
    pub pressed: bool,
    pub released: bool,
}

/// Named action states sampled from one input snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionFrame {
    actions: BTreeMap<String, ActionState>,
}

impl ActionFrame {
    pub fn action(&self, name: &str) -> ActionState {
        self.actions.get(name).copied().unwrap_or_default()
    }

    /// Combines four digital actions into a normalized 2D direction.
    pub fn vector2(&self, left: &str, right: &str, down: &str, up: &str) -> [f32; 2] {
        let mut value = [
            self.action(right).held as i8 as f32 - self.action(left).held as i8 as f32,
            self.action(up).held as i8 as f32 - self.action(down).held as i8 as f32,
        ];
        let length = (value[0] * value[0] + value[1] * value[1]).sqrt();
        if length > 1.0 {
            value[0] /= length;
            value[1] /= length;
        }
        value
    }
}

/// Rebindable keyboard action map with edge state retained across fixed steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputActionMap {
    bindings: BTreeMap<String, Vec<usize>>,
    previous_held: BTreeMap<String, bool>,
}

impl InputActionMap {
    pub fn bind_keys(&mut self, action: impl Into<String>, keys: impl IntoIterator<Item = usize>) {
        let action = action.into();
        let mut keys: Vec<_> = keys
            .into_iter()
            .filter(|key| *key < InputSnapshot::KEY_COUNT)
            .collect();
        keys.sort_unstable();
        keys.dedup();
        self.bindings.insert(action.clone(), keys);
        self.previous_held.remove(&action);
    }

    pub fn sample(&mut self, input: InputSnapshot) -> ActionFrame {
        let mut frame = ActionFrame::default();
        for (name, keys) in &self.bindings {
            let held = keys.iter().any(|key| input.key(*key));
            let pressed_edge = keys.iter().any(|key| input.key_pressed(*key));
            let released_edge = keys.iter().any(|key| input.key_released(*key));
            let was_held = self.previous_held.get(name).copied().unwrap_or(false);
            let pressed = (!was_held && held) || (!was_held && pressed_edge);
            let released = (was_held && !held) || (!held && pressed_edge && released_edge);
            frame.actions.insert(
                name.clone(),
                ActionState {
                    held,
                    pressed,
                    released,
                },
            );
            self.previous_held.insert(name.clone(), held);
        }
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_project_api::keys;

    #[test]
    fn action_edges_follow_combined_bindings() {
        let mut map = InputActionMap::default();
        map.bind_keys("jump", [keys::SPACE, keys::KEY_W]);

        let mut input = InputSnapshot::default();
        input.set_key(keys::SPACE, true);
        input.set_key_edge(keys::SPACE, true);
        assert_eq!(
            map.sample(input).action("jump"),
            ActionState {
                held: true,
                pressed: true,
                released: false,
            }
        );

        input.set_key_edge(keys::SPACE, false);
        input.set_key_edge(keys::KEY_W, false);
        input.set_key(keys::SPACE, false);
        input.set_key(keys::KEY_W, true);
        input.set_key_edge(keys::KEY_W, true);
        assert_eq!(
            map.sample(input).action("jump"),
            ActionState {
                held: true,
                pressed: false,
                released: false,
            }
        );

        input.set_key(keys::KEY_W, false);
        input.set_key_edge(keys::KEY_W, false);
        assert!(map.sample(input).action("jump").released);
    }

    #[test]
    fn a_tap_between_samples_reports_both_edges() {
        let mut map = InputActionMap::default();
        map.bind_keys("jump", [keys::SPACE]);
        let mut input = InputSnapshot::default();
        input.set_key_edge(keys::SPACE, true);
        input.set_key_edge(keys::SPACE, false);
        let action = map.sample(input).action("jump");
        assert_eq!(
            action,
            ActionState {
                held: false,
                pressed: true,
                released: true,
            }
        );
    }

    #[test]
    fn digital_vector_is_normalized_and_rebinds_replace_old_keys() {
        let mut map = InputActionMap::default();
        map.bind_keys("left", [keys::KEY_A]);
        map.bind_keys("right", [keys::KEY_D]);
        map.bind_keys("up", [keys::SPACE]);
        map.bind_keys("down", [keys::KEY_S]);
        map.bind_keys("left", [keys::ARROW_LEFT]);

        let mut input = InputSnapshot::default();
        input.set_key(keys::KEY_A, true);
        assert_eq!(
            map.sample(input).vector2("left", "right", "down", "up"),
            [0.0, 0.0]
        );

        input.set_key(keys::ARROW_LEFT, true);
        input.set_key(keys::SPACE, true);
        let direction = map.sample(input).vector2("left", "right", "down", "up");
        assert!((direction[0] + std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!((direction[1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }
}
