//! 动画图 cooked 格式解码（#560 P1）。
//!
//! 作者写 `*.animgraph.json`，cooker 编成 `.rag` 放进 `.pak`；运行时只解码。
//! 与 `.rskel` / `.rskin` / `.ranim` / `.rasm` 同一套分层（编码端在
//! `prism-cooker::cooker::anim_graph`）。
//!
//! ## RAGP v2
//!
//! ```text
//! [magic:4 "RAGP"][version:1]
//! [parameter_count:4]
//! parameters: parameter_count × {
//!     name_len: u16, name: utf8
//!     kind: u8                  // 0 = float, 1 = bool, 2 = trigger
//!     default: f32
//! }
//! [node_count:4]
//! nodes: node_count × {
//!     name_len: u16, name: utf8
//!     kind: u8                  // GraphNodeKind
//!     flags: u8                 // bit0 = looped, bit1 = additive
//!     speed: f32
//!     weight: f32
//!     [clip_count:4] clips: clip_count × { path_len: u16, path: utf8 }
//!     [sample_count:4] samples: sample_count × { x: f32, y: f32 }
//!     param_x_len: u16, param_x: utf8
//!     param_y_len: u16, param_y: utf8
//!     machine_len: u16, machine_path: utf8
//!     [input_count:4] inputs: input_count × {
//!         node: u32
//!         weight: f32
//!         [mask_count:4] mask: mask_count × { name_len: u16, name: utf8 }
//!     }
//!     [speed_count:4] sample_speeds: speed_count × f32  // v2
//! }
//! [output:4]                    // 输出节点下标
//! ```
//!
//! v2 在节点末尾追加每样本速度表；v1 没有该表，解出来为空。连线在 cook 时
//! 就解析成节点下标，运行时不需要再查名字。所有计数都有上限，
//! 坏文件返回 `None` 而不是分配一大块内存；cooker 还会拒绝重复节点名、未知
//! 连线和成环的图。

use crate::formats_skinned::Reader;

/// RAGP magic。
pub const RAGP_MAGIC: &[u8; 4] = b"RAGP";
/// RAGP 版本。
pub const RAGP_VERSION: u8 = 2;

/// 参数上限。
pub const MAX_GRAPH_PARAMETERS: usize = 1024;
/// 节点上限。
pub const MAX_GRAPH_NODES: usize = 4096;
/// 单节点剪辑 / 采样点上限。
pub const MAX_GRAPH_CLIPS: usize = 4096;
/// 单节点输入连线上限。
pub const MAX_GRAPH_INPUTS: usize = 256;
/// 单条连线的骨骼遮罩上限。
pub const MAX_GRAPH_BONE_MASK: usize = 1024;

/// 节点标志：循环。
pub const RAGP_NODE_LOOPED: u8 = 1 << 0;
/// 节点标志：additive。
pub const RAGP_NODE_ADDITIVE: u8 = 1 << 1;

/// 图参数类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphParameterKind {
    Float,
    Bool,
    Trigger,
}

impl GraphParameterKind {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Float),
            1 => Some(Self::Bool),
            2 => Some(Self::Trigger),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Float => 0,
            Self::Bool => 1,
            Self::Trigger => 2,
        }
    }
}

/// 图参数。`default` 对 bool / trigger 也是 0/1。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphParameterInfo {
    pub name: String,
    pub kind: GraphParameterKind,
    pub default: f32,
}

/// 节点类型（对应 #560 P1 的动画图节点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphNodeKind {
    /// 播放单个剪辑。
    ClipPlayer,
    /// 一维混合空间。
    BlendSpace1D,
    /// 二维混合空间。
    BlendSpace2D,
    /// 多层姿态栈。
    BlendStack,
    /// Montage / Notify 插槽。
    Slot,
    /// 按骨骼遮罩分层混合。
    LayeredBlend,
    /// 缓存一份姿态。
    CachedPose,
    /// 引用 `.rasm` 状态机。
    StateMachine,
    /// 图输出。
    Output,
}

impl GraphNodeKind {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::ClipPlayer),
            1 => Some(Self::BlendSpace1D),
            2 => Some(Self::BlendSpace2D),
            3 => Some(Self::BlendStack),
            4 => Some(Self::Slot),
            5 => Some(Self::LayeredBlend),
            6 => Some(Self::CachedPose),
            7 => Some(Self::StateMachine),
            8 => Some(Self::Output),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::ClipPlayer => 0,
            Self::BlendSpace1D => 1,
            Self::BlendSpace2D => 2,
            Self::BlendStack => 3,
            Self::Slot => 4,
            Self::LayeredBlend => 5,
            Self::CachedPose => 6,
            Self::StateMachine => 7,
            Self::Output => 8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ClipPlayer => "clip-player",
            Self::BlendSpace1D => "blend-space-1d",
            Self::BlendSpace2D => "blend-space-2d",
            Self::BlendStack => "blend-stack",
            Self::Slot => "slot",
            Self::LayeredBlend => "layered-blend",
            Self::CachedPose => "cached-pose",
            Self::StateMachine => "state-machine",
            Self::Output => "output",
        }
    }
}

/// 一条节点输入连线。`node` 是上游节点下标。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphInputInfo {
    pub node: u32,
    pub weight: f32,
    /// 该连线只影响的骨骼名；空 = 全身。
    pub bone_mask: Vec<String>,
}

/// 解码后的一个节点。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphNodeInfo {
    pub name: String,
    pub kind: GraphNodeKind,
    pub looped: bool,
    pub additive: bool,
    pub speed: f32,
    pub weight: f32,
    /// ClipPlayer 一个；BlendSpace 每个采样一个。
    pub clips: Vec<String>,
    /// BlendSpace 的采样坐标（1D 只用 x；其他节点为空）。
    pub positions: Vec<[f32; 2]>,
    /// BlendSpace 每样本播放速度（空 = 全部 1 / 共享相位；按 `clips` 顺序）。
    pub sample_speeds: Vec<f32>,
    pub parameter_x: String,
    pub parameter_y: String,
    pub state_machine_path: String,
    pub inputs: Vec<GraphInputInfo>,
}

/// 解码后的 `.rag`。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimGraphInfo {
    pub parameters: Vec<GraphParameterInfo>,
    pub nodes: Vec<GraphNodeInfo>,
    /// 输出节点下标（`nodes` 内）。
    pub output: u32,
}

/// 解码 `.rag`。
pub fn decode_rag(data: &[u8]) -> Option<AnimGraphInfo> {
    if data.len() < 9 || &data[..4] != RAGP_MAGIC {
        return None;
    }
    let mut reader = Reader::new(data, 4);
    let version = reader.u8()?;
    if version == 0 || version > RAGP_VERSION {
        return None;
    }

    let parameter_count = reader.u32()? as usize;
    if parameter_count > MAX_GRAPH_PARAMETERS {
        return None;
    }
    let mut parameters = Vec::with_capacity(parameter_count);
    for _ in 0..parameter_count {
        let name = read_string(&mut reader)?;
        let kind = GraphParameterKind::from_u8(reader.u8()?)?;
        parameters.push(GraphParameterInfo {
            name,
            kind,
            default: reader.f32()?,
        });
    }

    let node_count = reader.u32()? as usize;
    if node_count == 0 || node_count > MAX_GRAPH_NODES {
        return None;
    }
    let mut nodes = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let name = read_string(&mut reader)?;
        let kind = GraphNodeKind::from_u8(reader.u8()?)?;
        let flags = reader.u8()?;
        if flags & !(RAGP_NODE_LOOPED | RAGP_NODE_ADDITIVE) != 0 {
            return None;
        }
        let speed = reader.f32()?;
        let weight = reader.f32()?;

        let clip_count = reader.u32()? as usize;
        if clip_count > MAX_GRAPH_CLIPS {
            return None;
        }
        let mut clips = Vec::with_capacity(clip_count);
        for _ in 0..clip_count {
            clips.push(read_string(&mut reader)?);
        }

        let sample_count = reader.u32()? as usize;
        if sample_count > MAX_GRAPH_CLIPS {
            return None;
        }
        let mut positions = Vec::with_capacity(sample_count);
        for _ in 0..sample_count {
            positions.push([reader.f32()?, reader.f32()?]);
        }

        let parameter_x = read_string(&mut reader)?;
        let parameter_y = read_string(&mut reader)?;
        let state_machine_path = read_string(&mut reader)?;

        let input_count = reader.u32()? as usize;
        if input_count > MAX_GRAPH_INPUTS {
            return None;
        }
        let mut inputs = Vec::with_capacity(input_count);
        for _ in 0..input_count {
            let node = reader.u32()?;
            if node as usize >= node_count {
                return None;
            }
            let weight = reader.f32()?;
            let mask_count = reader.u32()? as usize;
            if mask_count > MAX_GRAPH_BONE_MASK {
                return None;
            }
            let mut bone_mask = Vec::with_capacity(mask_count);
            for _ in 0..mask_count {
                bone_mask.push(read_string(&mut reader)?);
            }
            inputs.push(GraphInputInfo {
                node,
                weight,
                bone_mask,
            });
        }

        // v2：节点末尾追加每样本速度表；v1 无此表。
        let mut sample_speeds = Vec::new();
        if version >= 2 {
            let speed_count = reader.u32()? as usize;
            if speed_count > MAX_GRAPH_CLIPS {
                return None;
            }
            sample_speeds.reserve(speed_count);
            for _ in 0..speed_count {
                let speed = reader.f32()?;
                if !speed.is_finite() || speed < 0.0 {
                    return None;
                }
                sample_speeds.push(speed);
            }
        }

        nodes.push(GraphNodeInfo {
            name,
            kind,
            looped: flags & RAGP_NODE_LOOPED != 0,
            additive: flags & RAGP_NODE_ADDITIVE != 0,
            speed,
            weight,
            clips,
            positions,
            sample_speeds,
            parameter_x,
            parameter_y,
            state_machine_path,
            inputs,
        });
    }

    let output = reader.u32()?;
    if output as usize >= node_count {
        return None;
    }
    if reader.remaining() != 0 {
        return None;
    }

    Some(AnimGraphInfo {
        parameters,
        nodes,
        output,
    })
}

fn read_string(reader: &mut Reader<'_>) -> Option<String> {
    let len = reader.u16()? as usize;
    String::from_utf8(reader.take(len)?.to_vec()).ok()
}

#[cfg(test)]
#[path = "formats_anim_graph_tests.rs"]
mod tests;
