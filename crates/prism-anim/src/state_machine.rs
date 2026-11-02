//! 数据驱动的状态机核心（#560 P1）。
//!
//! 只做**求值**：状态引用剪辑路径，转移带「参数条件 + 最早可打断比例 + 过渡时长」。
//! 真正的播放（采样、交叉淡入、播放头）在 `prism-engine` 的 `animator` 里，这一层
//! 不碰 ECS／渲染，因此可以单独测。
//!
//! 设计约定：
//!
//! - **转移按作者顺序求值，第一个「条件全真 + 已达最早打断比例」的胜出**；同一帧
//!   只走一条转移（UE 的语义），避免一帧连跳两个状态。
//! - **Trigger 是消耗型的**：`matches` 命中一次就从参数表里拿走，所以「按一下跳一次」
//!   不会因为条件持续为真而每帧重进。
//! - 非有限参数（NaN）永远不参与比较，条件视为不成立 —— 坏输入不该让角色乱跳状态。

use std::collections::{BTreeMap, BTreeSet};

/// 动画参数表：状态机转移的输入。
///
/// 用 `BTreeMap` / `BTreeSet` 而不是 HashMap：参数名是作者写的字符串，条目个位数，
/// 有序容器的遍历顺序确定（调试输出、序列化都稳定）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnimParameters {
    floats: BTreeMap<String, f32>,
    bools: BTreeMap<String, bool>,
    triggers: BTreeSet<String>,
}

impl AnimParameters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_float(&mut self, name: impl Into<String>, value: f32) {
        self.floats.insert(name.into(), value);
    }

    pub fn float(&self, name: &str) -> Option<f32> {
        self.floats.get(name).copied()
    }

    pub fn set_bool(&mut self, name: impl Into<String>, value: bool) {
        self.bools.insert(name.into(), value);
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        self.bools.get(name).copied()
    }

    /// 置一个 trigger，下一次匹配到它的转移会消耗掉。
    pub fn set_trigger(&mut self, name: impl Into<String>) {
        self.triggers.insert(name.into());
    }

    pub fn has_trigger(&self, name: &str) -> bool {
        self.triggers.contains(name)
    }

    /// 取走 trigger；没置过返回 `false`。
    pub fn take_trigger(&mut self, name: &str) -> bool {
        self.triggers.remove(name)
    }

    pub fn clear_triggers(&mut self) {
        self.triggers.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.floats.is_empty() && self.bools.is_empty() && self.triggers.is_empty()
    }
}

/// 一条转移条件。
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionCondition {
    /// 浮点参数「大于」阈值。
    FloatGreater { name: String, threshold: f32 },
    /// 浮点参数「小于」阈值。
    FloatLess { name: String, threshold: f32 },
    /// 布尔参数等于给定值（参数缺失时按 `false`）。
    BoolIs { name: String, expected: bool },
    /// 消耗一次 trigger。
    Trigger { name: String },
}

impl TransitionCondition {
    /// 求值；`Trigger` 命中时会把它从 `params` 里拿走。
    ///
    /// 参数缺失 / 非有限 → `false`：宁可留在当前状态，也不要因为数据没填好乱跳。
    pub fn matches(&self, params: &mut AnimParameters) -> bool {
        match self {
            Self::FloatGreater { name, threshold } => match params.float(name) {
                Some(value) if value.is_finite() && threshold.is_finite() => value > *threshold,
                _ => false,
            },
            Self::FloatLess { name, threshold } => match params.float(name) {
                Some(value) if value.is_finite() && threshold.is_finite() => value < *threshold,
                _ => false,
            },
            Self::BoolIs { name, expected } => params.bool(name).unwrap_or(false) == *expected,
            Self::Trigger { name } => params.take_trigger(name),
        }
    }
}

/// 一个状态：指向一段剪辑，外加播放参数。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimState {
    pub name: String,
    /// `.ranim` 路径；空 = 这个状态不播东西（例如「死亡」靠别的手段表现）。
    pub clip_path: String,
    pub speed: f32,
    pub looped: bool,
}

impl AnimState {
    pub fn new(name: impl Into<String>, clip_path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            clip_path: clip_path.into(),
            speed: 1.0,
            looped: true,
        }
    }
}

/// 一条转移。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimTransition {
    /// 源状态下标；`usize::MAX` 表示「任意状态」（`from_any`）。
    pub from: usize,
    pub to: usize,
    /// 条件全真、且当前状态已播放到该比例（`0.0` = 随时可打断，`1.0` = 必须播完）
    /// 之后才允许转移。
    pub min_state_fraction: f32,
    /// 过渡时长（秒）；`0` = 硬切。
    pub duration: f32,
    pub conditions: Vec<TransitionCondition>,
}

impl AnimTransition {
    pub fn new(from: usize, to: usize) -> Self {
        Self {
            from,
            to,
            min_state_fraction: 0.0,
            duration: 0.0,
            conditions: Vec::new(),
        }
    }

    /// 从任意状态出发的转移。
    pub fn from_any(to: usize) -> Self {
        Self::new(ANY_STATE, to)
    }

    pub fn with_conditions(mut self, conditions: Vec<TransitionCondition>) -> Self {
        self.conditions = conditions;
        self
    }

    pub fn with_duration(mut self, duration: f32) -> Self {
        self.duration = duration;
        self
    }

    pub fn with_min_state_fraction(mut self, fraction: f32) -> Self {
        self.min_state_fraction = fraction;
        self
    }
}

/// `AnimTransition::from` 取这个值表示「任意状态」。
pub const ANY_STATE: usize = usize::MAX;

/// 一次成功求值的结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransitionTarget {
    pub state: usize,
    /// 过渡时长（秒），已经过滤掉非有限 / 负值。
    pub duration: f32,
}

/// 状态机定义。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateMachine {
    pub states: Vec<AnimState>,
    pub transitions: Vec<AnimTransition>,
}

impl StateMachine {
    pub fn new(states: Vec<AnimState>, transitions: Vec<AnimTransition>) -> Self {
        Self {
            states,
            transitions,
        }
    }

    pub fn state_index(&self, name: &str) -> Option<usize> {
        self.states.iter().position(|state| state.name == name)
    }

    pub fn state(&self, index: usize) -> Option<&AnimState> {
        self.states.get(index)
    }

    /// 求下一条转移。
    ///
    /// `state_time` / `state_duration` 是**当前状态**已经播放的秒数与剪辑时长，
    /// 用来判断最早打断比例（时长未知时按「已播完」处理，否则状态会卡死）。
    ///
    /// 返回 `None` = 留在当前状态。命中的 trigger 会被消耗，未命中的保持原样 ——
    /// 所以「同一帧先求值的转移消耗了 trigger」不会影响后面的转移判断。
    pub fn next_transition(
        &self,
        from: usize,
        params: &mut AnimParameters,
        state_time: f32,
        state_duration: f32,
    ) -> Option<TransitionTarget> {
        if from >= self.states.len() {
            return None;
        }
        let fraction = state_fraction(state_time, state_duration);
        for transition in &self.transitions {
            if transition.from != ANY_STATE && transition.from != from {
                continue;
            }
            if transition.to >= self.states.len() || transition.to == from {
                continue;
            }
            if fraction + 1e-4 < sanitize_fraction(transition.min_state_fraction) {
                continue;
            }
            if !transition
                .conditions
                .iter()
                .all(|condition| condition.matches(params))
            {
                continue;
            }
            return Some(TransitionTarget {
                state: transition.to,
                duration: if transition.duration.is_finite() {
                    transition.duration.max(0.0)
                } else {
                    0.0
                },
            });
        }
        None
    }
}

/// 当前状态播放到哪儿（0..=1）。时长未知 / 非有限 → 视为已播完。
fn state_fraction(state_time: f32, state_duration: f32) -> f32 {
    if !state_duration.is_finite() || state_duration <= 0.0 {
        return 1.0;
    }
    if !state_time.is_finite() {
        return 0.0;
    }
    (state_time / state_duration).clamp(0.0, 1.0)
}

fn sanitize_fraction(fraction: f32) -> f32 {
    if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
#[path = "state_machine_tests.rs"]
mod tests;
