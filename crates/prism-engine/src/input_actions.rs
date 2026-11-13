//! 引擎层的**按键绑定**：把「哪个动作」和「按哪个键」分开。
//!
//! 两条规则：
//!
//! 1. **默认不绑定**。引擎自己没有「全局快捷键」——HUD / 调试图 / 色调映射 / PBR 隔离
//!    只是 `RenderSettings` 上的字段，**谁想要谁绑**。宿主（prism-app 的调试层、编辑器）
//!    用 [`KeyBindings::default_engine_debug`] 显式声明；项目不声明就一个键都不占。
//! 2. **引擎绑了的键先问项目**。宿主分发前先调 `FrameHook::on_platform_event`，钩子
//!    返回 true 才轮到引擎的绑定生效——项目永远有最终发言权。

use crate::input::KeyCode;

/// 引擎内置的调试动作。
///
/// 这些动作只改 [`crate::render_settings::RenderSettings`] 的调试字段，
/// 默认**没有任何按键绑定**（见模块文档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineAction {
    /// 开关运行时诊断 HUD。
    ToggleHud,
    /// 轮换 PostPass 调试图（0 = 正常 / 1 = 深度 / 2 = 视空间法线）。
    CycleDebugTarget,
    /// 轮换色调映射算子（0 = Reinhard / 1 = ACES / 2 = passthrough）。
    CycleTonemap,
    /// PBR 分量隔离：只显示第 `bit` 个分量（0..=14），15 位是蒙皮权重热力图。
    IsolatePbrComponent(u32),
}

/// 数字键 → PBR 分量。两套语义只差 Shift，所以只写一张表。
///
/// 不按 Shift：1..=9 → 0..=8，0 → 9。
/// 按 Shift：1..=5 → 10..=14，6 → 15（蒙皮权重热力图），其余数字键不绑。
fn digit_bindings(shift: bool) -> Vec<(KeyCode, u32)> {
    let digits = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
        KeyCode::Digit0,
    ];
    digits
        .iter()
        .enumerate()
        .filter_map(|(index, key)| {
            let index = index as u32;
            let bit = if shift {
                if index < 6 {
                    10 + index
                } else {
                    return None;
                }
            } else {
                index
            };
            Some((*key, bit))
        })
        .collect()
}

/// 一条绑定：某个按键触发某个动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyBinding {
    pub key: KeyCode,
    pub action: EngineAction,
}

/// 一张绑定表。空表是默认值。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyBindings {
    bindings: Vec<KeyBinding>,
}

impl KeyBindings {
    /// 空表：引擎一个键都不占。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 宿主调试层用的默认绑定（prism-app 的 `engine-debug` feature 调用它）。
    ///
    /// F3 = HUD，Tab = 调试图，T = 色调映射，1..=9 = PBR 分量 0..=8，
    /// Shift+1..=Shift+6 = PBR 分量 9..=14。字母移动键一个都不占。
    ///
    /// 这张表不会自动生效：项目不开调试层、或在钩子里消费事件，就能完全接管这些键。
    pub fn default_engine_debug() -> Self {
        let mut bindings = Vec::new();
        bindings.push(KeyBinding {
            key: KeyCode::F3,
            action: EngineAction::ToggleHud,
        });
        bindings.push(KeyBinding {
            key: KeyCode::Tab,
            action: EngineAction::CycleDebugTarget,
        });
        bindings.push(KeyBinding {
            key: KeyCode::KeyT,
            action: EngineAction::CycleTonemap,
        });
        // 不按 Shift：`1..=9` 是分量 0..=8，`0` 是分量 9（和键盘上数字的排列
        // 一致，也和引擎历史上那套 PBR 隔离快捷键一致）。
        for (key, bit) in digit_bindings(false) {
            bindings.push(KeyBinding {
                key,
                action: EngineAction::IsolatePbrComponent(bit),
            });
        }
        Self { bindings }
    }

    /// 宿主调试层的绑定，按 Shift 状态给出两套数字键语义。
    ///
    /// 不按 Shift 时 `1..=9, 0` 是 PBR 分量 0..=9；按 Shift 时 `1..=5` 是 10..=14、
    /// `6` 是蒙皮权重热力图（第 15 位）。字母键与其余功能键不受 Shift 影响。
    ///
    /// 把 Shift 作为参数而不是第二条表，是因为它只改数字键的**含义**，不改绑定集合。
    pub fn engine_debug(shift: bool) -> Self {
        let mut bindings = Self::default_engine_debug();
        bindings
            .bindings
            .retain(|binding| !matches!(binding.action, EngineAction::IsolatePbrComponent(_)));
        for (key, bit) in digit_bindings(shift) {
            bindings.bind(key, EngineAction::IsolatePbrComponent(bit));
        }
        bindings
    }

    /// 绑定一个键；同一个键重复绑定时以**后加入**的为准。
    pub fn bind(&mut self, key: KeyCode, action: EngineAction) -> &mut Self {
        self.bindings.push(KeyBinding { key, action });
        self
    }

    /// 是否有任何绑定。
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// 全部绑定（顺序即加入顺序；查表时后者覆盖前者）。
    pub fn iter(&self) -> impl Iterator<Item = &KeyBinding> {
        self.bindings.iter()
    }

    /// 查一个键触发的动作；重复绑定时取最后加入的那条。
    pub fn action_for(&self, key: KeyCode) -> Option<EngineAction> {
        self.bindings
            .iter()
            .rev()
            .find(|binding| binding.key == key)
            .map(|binding| binding.action)
    }
}

fn digit_key(digit: u32) -> KeyCode {
    match digit {
        0 => KeyCode::Digit0,
        1 => KeyCode::Digit1,
        2 => KeyCode::Digit2,
        3 => KeyCode::Digit3,
        4 => KeyCode::Digit4,
        5 => KeyCode::Digit5,
        6 => KeyCode::Digit6,
        7 => KeyCode::Digit7,
        8 => KeyCode::Digit8,
        _ => KeyCode::Digit9,
    }
}

fn f_key(index: u32) -> KeyCode {
    match index {
        0 => KeyCode::F1,
        1 => KeyCode::F2,
        2 => KeyCode::F3,
        3 => KeyCode::F4,
        4 => KeyCode::F5,
        5 => KeyCode::F6,
        6 => KeyCode::F7,
        7 => KeyCode::F8,
        8 => KeyCode::F9,
        9 => KeyCode::F10,
        10 => KeyCode::F11,
        _ => KeyCode::F12,
    }
}

/// 平台层虚拟键码（`prism_platform::Key::to_vk()`，Win32 `VK_*` 契约）→ 引擎键码。
///
/// 换算只吃一个 `u32`，所以放在引擎里：两个窗口后端共用同一张表，并且可测。
/// 没进表的键保留原始码（`KeyCode::Other(vk)`），项目按字面量认。
pub fn key_code_from_vk(vk: u32) -> KeyCode {
    match vk {
        0x57 => KeyCode::KeyW,
        0x41 => KeyCode::KeyA,
        0x53 => KeyCode::KeyS,
        0x44 => KeyCode::KeyD,
        0x51 => KeyCode::KeyQ,
        0x45 => KeyCode::KeyE,
        0x54 => KeyCode::KeyT,
        0x20 => KeyCode::Space,
        0xA0 => KeyCode::ShiftLeft,
        0xA1 => KeyCode::ShiftRight,
        0xA2 => KeyCode::ControlLeft,
        0xA3 => KeyCode::ControlRight,
        0xA4 => KeyCode::AltLeft,
        0xA5 => KeyCode::AltRight,
        0x1B => KeyCode::Escape,
        0x09 => KeyCode::Tab,
        0x0D => KeyCode::Enter,
        0x26 => KeyCode::ArrowUp,
        0x28 => KeyCode::ArrowDown,
        0x25 => KeyCode::ArrowLeft,
        0x27 => KeyCode::ArrowRight,
        0x30..=0x39 => digit_key(vk - 0x30),
        0x70..=0x7B => f_key(vk - 0x70),
        other => KeyCode::Other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认绑定表是**空的**：引擎不许在没被要求时占任何键。
    #[test]
    fn engine_binds_no_keys_by_default() {
        let bindings = KeyBindings::empty();
        assert!(bindings.is_empty());
        for vk in [0x09u32, 0x54, 0x70, 0x31, 0x52] {
            assert_eq!(bindings.action_for(key_code_from_vk(vk)), None);
        }
    }

    /// 宿主调试层的绑定覆盖 F3 / Tab / T / 数字键，且不碰字母移动键。
    #[test]
    fn host_debug_bindings_cover_the_documented_keys() {
        let bindings = KeyBindings::default_engine_debug();
        assert_eq!(
            bindings.action_for(KeyCode::F3),
            Some(EngineAction::ToggleHud)
        );
        assert_eq!(
            bindings.action_for(KeyCode::Tab),
            Some(EngineAction::CycleDebugTarget)
        );
        assert_eq!(
            bindings.action_for(KeyCode::KeyT),
            Some(EngineAction::CycleTonemap)
        );
        assert_eq!(
            bindings.action_for(KeyCode::Digit1),
            Some(EngineAction::IsolatePbrComponent(0))
        );
        assert_eq!(
            bindings.action_for(KeyCode::Digit0),
            Some(EngineAction::IsolatePbrComponent(9))
        );
        assert_eq!(
            bindings.action_for(KeyCode::Digit9),
            Some(EngineAction::IsolatePbrComponent(8))
        );
        for key in [
            KeyCode::KeyW,
            KeyCode::KeyA,
            KeyCode::KeyS,
            KeyCode::KeyD,
            KeyCode::KeyQ,
            KeyCode::KeyE,
            KeyCode::Space,
        ] {
            assert_eq!(bindings.action_for(key), None, "{key:?} 不该被调试层占用");
        }
    }

    /// `Key::to_vk` 的契约（Win32 虚拟键码）→ 引擎键码。
    #[test]
    fn virtual_key_codes_map_to_engine_keys() {
        assert_eq!(key_code_from_vk(0x57), KeyCode::KeyW);
        assert_eq!(key_code_from_vk(0x51), KeyCode::KeyQ);
        assert_eq!(key_code_from_vk(0x45), KeyCode::KeyE);
        assert_eq!(key_code_from_vk(0x54), KeyCode::KeyT);
        assert_eq!(key_code_from_vk(0x09), KeyCode::Tab);
        assert_eq!(key_code_from_vk(0x70), KeyCode::F1);
        assert_eq!(key_code_from_vk(0x72), KeyCode::F3);
        assert_eq!(key_code_from_vk(0x7B), KeyCode::F12);
        assert_eq!(key_code_from_vk(0x31), KeyCode::Digit1);
        assert_eq!(key_code_from_vk(0x52), KeyCode::Other(0x52));
    }
}
