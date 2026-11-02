//! 状态机的 cooked 格式解码（#560 P1）。
//!
//! 作者写 `*.statemachine.json`，cooker 编成 `.rasm` 放进 `.pak`；运行时只解码。
//! 与 `.rskel` / `.rskin` / `.ranim` 同一套分层（编码端在 `prism-cooker`）。
//!
//! ## RASM v1
//!
//! ```text
//! [magic:4 "RASM"][version:1][state_count:4][transition_count:4]
//! states: state_count × {
//!     name_len: u16, name: utf8
//!     clip_len: u16, clip_path: utf8
//!     speed: f32
//!     flags: u8              // bit0 = looped
//! }
//! transitions: transition_count × {
//!     from: u32              // u32::MAX = 任意状态
//!     to:   u32
//!     min_state_fraction: f32
//!     duration: f32
//!     condition_count: u32
//!     conditions: condition_count × {
//!         kind: u8
//!         name_len: u16, name: utf8
//!         payload: 依 kind
//!             FLOAT_GREATER / FLOAT_LESS → f32 threshold
//!             BOOL_IS                    → u8 (0/1)
//!             TRIGGER                    → 无
//!     }
//! }
//! ```
//!
//! 状态和转移都用**名字**引用剪辑、用**下标**引用状态：下标在 cook 时就解析好，
//! 运行时不需要再查名字。所有计数都有上限（`MAX_STATES` / `MAX_TRANSITIONS` /
//! `MAX_CONDITIONS`），坏文件返回 `None` 而不是分配一大块内存。

use crate::formats_skinned::Reader;

/// RASM magic。
pub const RASM_MAGIC: &[u8; 4] = b"RASM";
/// RASM 版本。
pub const RASM_VERSION: u8 = 1;
/// 状态上限。
pub const MAX_STATES: usize = 4096;
/// 转转移上限。
pub const MAX_TRANSITIONS: usize = 65536;
/// 单条转移的条件上限。
pub const MAX_CONDITIONS: usize = 256;

/// 状态标志：循环播放。
pub const RASM_STATE_LOOPED: u8 = 1 << 0;

/// 条件种类。
pub const CONDITION_FLOAT_GREATER: u8 = 0;
/// 条件种类：浮点小于。
pub const CONDITION_FLOAT_LESS: u8 = 1;
/// 条件种类：布尔等于。
pub const CONDITION_BOOL_IS: u8 = 2;
/// 条件种类：触发器（命中即消耗）。
pub const CONDITION_TRIGGER: u8 = 3;

/// 解码后的一个状态。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimStateInfo {
    pub name: String,
    pub clip_path: String,
    pub speed: f32,
    pub looped: bool,
}

/// 解码后的一条转移条件。
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionConditionInfo {
    FloatGreater { name: String, threshold: f32 },
    FloatLess { name: String, threshold: f32 },
    BoolIs { name: String, expected: bool },
    Trigger { name: String },
}

impl TransitionConditionInfo {
    pub fn parameter_name(&self) -> &str {
        match self {
            Self::FloatGreater { name, .. }
            | Self::FloatLess { name, .. }
            | Self::BoolIs { name, .. }
            | Self::Trigger { name } => name,
        }
    }
}

/// 解码后的一条转移。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimTransitionInfo {
    /// 源状态下标；`None` = 任意状态。
    pub from: Option<u32>,
    pub to: u32,
    /// 源状态至少播到这个比例才允许转移（0 = 随时可以）。
    pub min_state_fraction: f32,
    /// 交叉淡入时长（秒）；0 = 硬切。
    pub duration: f32,
    pub conditions: Vec<TransitionConditionInfo>,
}

/// 解码后的 `.rasm`。
#[derive(Debug, Clone, PartialEq)]
pub struct StateMachineInfo {
    pub states: Vec<AnimStateInfo>,
    pub transitions: Vec<AnimTransitionInfo>,
}

/// 解码 `.rasm`。
pub fn decode_rasm(data: &[u8]) -> Option<StateMachineInfo> {
    if data.len() < 13 || &data[..4] != RASM_MAGIC {
        return None;
    }
    let mut reader = Reader::new(data, 4);
    if reader.u8()? != RASM_VERSION {
        return None;
    }
    let state_count = reader.u32()? as usize;
    let transition_count = reader.u32()? as usize;
    if state_count > MAX_STATES || transition_count > MAX_TRANSITIONS {
        return None;
    }

    let mut states = Vec::with_capacity(state_count);
    for _ in 0..state_count {
        let name = read_string(&mut reader)?;
        let clip_path = read_string(&mut reader)?;
        let speed = reader.f32()?;
        let flags = reader.u8()?;
        if flags & !RASM_STATE_LOOPED != 0 {
            return None;
        }
        states.push(AnimStateInfo {
            name,
            clip_path,
            speed,
            looped: flags & RASM_STATE_LOOPED != 0,
        });
    }

    let mut transitions = Vec::with_capacity(transition_count);
    for _ in 0..transition_count {
        let from = reader.u32()?;
        let to = reader.u32()?;
        let min_state_fraction = reader.f32()?;
        let duration = reader.f32()?;
        let condition_count = reader.u32()? as usize;
        if condition_count > MAX_CONDITIONS {
            return None;
        }
        let mut conditions = Vec::with_capacity(condition_count);
        for _ in 0..condition_count {
            let kind = reader.u8()?;
            let name = read_string(&mut reader)?;
            let condition = match kind {
                CONDITION_FLOAT_GREATER => TransitionConditionInfo::FloatGreater {
                    name,
                    threshold: reader.f32()?,
                },
                CONDITION_FLOAT_LESS => TransitionConditionInfo::FloatLess {
                    name,
                    threshold: reader.f32()?,
                },
                CONDITION_BOOL_IS => TransitionConditionInfo::BoolIs {
                    name,
                    expected: reader.u8()? != 0,
                },
                CONDITION_TRIGGER => TransitionConditionInfo::Trigger { name },
                _ => return None,
            };
            conditions.push(condition);
        }
        transitions.push(AnimTransitionInfo {
            from: if from == u32::MAX { None } else { Some(from) },
            to,
            min_state_fraction,
            duration,
            conditions,
        });
    }

    if reader.remaining() != 0 {
        return None;
    }

    Some(StateMachineInfo {
        states,
        transitions,
    })
}

/// 读一个 `u16` 长度前缀的 UTF-8 字符串。
fn read_string(reader: &mut Reader<'_>) -> Option<String> {
    let len = reader.u16()? as usize;
    String::from_utf8(reader.take(len)?.to_vec()).ok()
}

#[cfg(test)]
#[path = "formats_state_machine_tests.rs"]
mod tests;
