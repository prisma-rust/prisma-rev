//! 关键帧动画：轨道数据与采样。
//!
//! 引擎原本只有 2D 精灵帧动画（[`crate::components::SpriteAnimation`]，把播放
//! 进度换算成 `SpriteRenderer.frame_index`）。本模块补上**曲线**动画的最小可用
//! 面：位置 / 旋转 / 缩放三条轨道、线性插值、端点保持（不外插）、循环与单次播放。
//!
//! 这里只做「时间 → 值」的纯函数，不碰 ECS；播放头与写回 `Transform` 在引擎侧
//! （`prism_engine::scene::systems::animation::keyframe_animation_system`）。
//! 剪辑事件（[`AnimationEvent`]）也放在这里：只是 `(time, name)` 数据，触发在引擎侧。
//! 骨骼
//! 蒙皮不在这层，但骨骼动画的剪辑采样与这里完全一致——骨骼只是把采样结果写给
//! 骨骼节点的 `Transform`。
//!
//! 约定：
//!
//! - 关键帧按 `time` 升序排列（编辑器写入时会排序）；采样用二分查找。
//! - 时间轴两端**保持端点值**（Unity 的 `AnimationClip` 默认同样不外插），
//!   所以首帧之前 / 末帧之后都是常量。
//! - 旋转用 XYZ 欧拉角（**度**），与 [`crate::components::DirectionalLight`] 的
//!   `euler_xyz` 同一约定：手写 JSON 与编辑器里都可读。

use prism_reflect::Reflect;
use serde::{Deserialize, Serialize};

/// 位置 / 缩放 / 欧拉角轨道上的单个关键帧。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Reflect)]
#[serde(default)]
#[reflect(name = "prism_engine::animation::Vec3Key")]
pub struct Vec3Key {
    /// 关键帧时间（秒，相对剪辑起点）。
    pub time: f32,
    /// 值；旋转轨道里是 XYZ 欧拉角（度）。
    pub value: [f32; 3],
}

impl Default for Vec3Key {
    fn default() -> Self {
        Self {
            time: 0.0,
            value: [0.0; 3],
        }
    }
}

impl Vec3Key {
    pub fn new(time: f32, value: [f32; 3]) -> Self {
        Self { time, value }
    }
}

/// 剪辑上的一个事件（#490）：播放头穿过 `time` 时由引擎发射。
///
/// 事件名只是字符串，引擎不解释语义；玩法把名字映射成音效 / 特效 / 状态切换。
/// 时间单位与关键帧一致（秒，相对剪辑起点）；`time` 非有限值的事件会被忽略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Reflect)]
#[serde(default)]
#[reflect(name = "prism_engine::animation::AnimationEvent")]
pub struct AnimationEvent {
    /// 触发时间（秒，相对剪辑起点）。
    pub time: f32,
    /// 事件名（玩法自定义）。
    pub name: String,
}

impl Default for AnimationEvent {
    fn default() -> Self {
        Self {
            time: 0.0,
            name: String::new(),
        }
    }
}

impl AnimationEvent {
    pub fn new(time: f32, name: impl Into<String>) -> Self {
        Self {
            time,
            name: name.into(),
        }
    }
}

/// 一次采样的结果。
///
/// `None` = 该通道**没有关键帧**，调用方必须保留作者设的值（不能让空轨道把
/// 作者的旋转清零）。三条通道互相独立，这正是「只动画 position」这类常见用法
/// 的前提。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClipSample {
    pub position: Option<[f32; 3]>,
    /// XYZ 欧拉角，**度**。
    pub euler_xyz: Option<[f32; 3]>,
    pub scale: Option<[f32; 3]>,
}

impl ClipSample {
    /// 是否采到了任何通道。
    pub fn is_empty(&self) -> bool {
        self.position.is_none() && self.euler_xyz.is_none() && self.scale.is_none()
    }
}

/// 轨道里最后一个关键帧的时间（空轨道为 `0.0`）。
pub fn max_key_time(keys: &[Vec3Key]) -> f32 {
    keys.iter()
        .map(|key| key.time)
        .filter(|time| time.is_finite())
        .fold(0.0f32, f32::max)
}

/// 在一条轨道上采样 `time` 处的值。
///
/// 空轨道返回 `None`；只有一个关键帧、或时间落在首帧之前 / 末帧之后时返回端点
/// 值；其余情况在相邻两帧之间线性插值。`time` 非有限值按 `0` 处理。
pub fn sample_vec3(keys: &[Vec3Key], time: f32) -> Option<[f32; 3]> {
    let first = keys.first()?;
    let time = if time.is_finite() { time } else { 0.0 };
    if time <= first.time || keys.len() == 1 {
        return Some(first.value);
    }
    // 最后一个 `time <= keys[i].time` 的下标（`time > keys[0].time` 保证存在）。
    let mut low = 0usize;
    let mut high = keys.len();
    while low < high {
        let mid = (low + high) / 2;
        if keys[mid].time <= time {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    let index = low - 1;
    let start = &keys[index];
    let Some(end) = keys.get(index + 1) else {
        return Some(start.value);
    };
    let span = end.time - start.time;
    if span <= f32::EPSILON {
        // 两个关键帧同一时刻（或乱序）：用先出现的那一个，避免除零放大。
        return Some(start.value);
    }
    let t = ((time - start.time) / span).clamp(0.0, 1.0);
    Some([
        start.value[0] + (end.value[0] - start.value[0]) * t,
        start.value[1] + (end.value[1] - start.value[1]) * t,
        start.value[2] + (end.value[2] - start.value[2]) * t,
    ])
}

/// 按时间升序排序（稳定），同一时刻的关键帧保持插入顺序。
pub fn sort_keys(keys: &mut [Vec3Key]) {
    keys.sort_by(|a, b| a.time.total_cmp(&b.time));
}

#[cfg(test)]
#[path = "animation_tests.rs"]
mod tests;
