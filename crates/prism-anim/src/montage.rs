//! Montage 播放队列（#560 P1）。
//!
//! Montage 是一次性 / 循环的覆盖动画（受击、翻越、开火）。这一层只做播放状态：
//! 立即播放、FIFO 排队、单次结束自动接下一段、`stop` / `stop_after_current`，
//! 以及跨帧 Notify。采样仍然复用 [`Clip`]，姿态合成由调用方（图 Slot 节点或
//! 引擎）决定。
//!
//! 与 `Animator` 的普通剪辑不同，Montage 自带开始时间 / 循环 / 速度 / 权重 /
//! blend-in / blend-out，播放头只属于这个播放器，不会改写作者场景数据。

use std::collections::VecDeque;
use std::sync::Arc;

use crate::clip::{Clip, ClipSection};
use crate::pose::Pose;

/// 队列上限：坏逻辑（每帧 enqueue）不应该无限吃内存。
pub const MAX_MONTAGE_QUEUE: usize = 32;

/// Montage BranchIn 的 Notify 前缀：`BranchIn.<section>` 会在跨过该事件时跳到
/// 同名段起点（#560 P4）。段不存在时事件照常发出，但不跳转。
pub const BRANCH_IN_PREFIX: &str = "BranchIn.";

/// 一段待播放的 Montage。
#[derive(Debug, Clone)]
pub struct Montage {
    clip: Arc<Clip>,
    start_time: f32,
    /// 从剪辑的哪个 Montage 段开始；找不到时回退到 `start_time`。
    start_section: Option<String>,
    looped: bool,
    speed: f32,
    weight: f32,
    /// 起始淡入时长（秒）；0 = 立即满权重。
    blend_in: f32,
    /// 单次 Montage 结束前的淡出时长（秒）；循环 Montage 不使用。
    blend_out: f32,
}

impl Montage {
    /// 默认：从 0 开始、循环、速度 1、权重 1。
    pub fn new(clip: Arc<Clip>) -> Self {
        Self {
            clip,
            start_time: 0.0,
            start_section: None,
            looped: true,
            speed: 1.0,
            weight: 1.0,
            blend_in: 0.0,
            blend_out: 0.0,
        }
    }

    pub fn with_start_time(mut self, start_time: f32) -> Self {
        self.start_time = start_time;
        self
    }

    /// 从指定 Montage 段开始播放；段不存在时在 `start` 时回退到 `start_time`。
    pub fn with_start_section(mut self, section: impl Into<String>) -> Self {
        let section = section.into();
        self.start_section = (!section.trim().is_empty()).then_some(section);
        self
    }

    pub fn with_looping(mut self, looped: bool) -> Self {
        self.looped = looped;
        self
    }

    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }

    pub fn with_weight(mut self, weight: f32) -> Self {
        self.weight = weight;
        self
    }

    /// 起始淡入时长（秒）；负数 / 非有限值按 0 处理。
    pub fn with_blend_in(mut self, seconds: f32) -> Self {
        self.blend_in = sanitize_duration(seconds);
        self
    }

    /// 单次 Montage 结束前的淡出时长（秒）；循环 Montage 不使用。
    pub fn with_blend_out(mut self, seconds: f32) -> Self {
        self.blend_out = sanitize_duration(seconds);
        self
    }

    pub fn clip(&self) -> &Arc<Clip> {
        &self.clip
    }
}

/// 跨过的一个 Montage Notify。
#[derive(Debug, Clone, PartialEq)]
pub struct MontageNotify {
    /// 事件所属剪辑名（`Clip.name`，便于上层映射回资产）。
    pub clip_name: String,
    pub name: String,
    /// 事件在剪辑里的本地时间（秒）。
    pub time: f32,
}

#[derive(Debug, Clone)]
struct ActiveMontage {
    montage: Montage,
    time: f32,
    /// 这一段已经播放的绝对时长（秒），用于 blend-in；暂停时不推进。
    elapsed: f32,
    /// 作者设置的基准权重；实际权重还要乘淡入 / 淡出系数。
    weight: f32,
}

impl ActiveMontage {
    /// 这一帧实际用于姿态混合的权重。
    fn effective_weight(&self) -> f32 {
        let fade_in = if self.montage.blend_in > 0.0 {
            (self.elapsed / self.montage.blend_in).clamp(0.0, 1.0)
        } else {
            1.0
        };
        // 循环 Montage 每圈都会回到结尾，按剩余时间淡出会周期性闪一下；
        // 因此只在单次 Montage 上应用 blend-out。
        let fade_out = if !self.montage.looped && self.montage.blend_out > 0.0 {
            let local = local_time(self.montage.clip.duration, self.montage.looped, self.time);
            let remaining = (self.montage.clip.duration - local).max(0.0);
            (remaining / self.montage.blend_out).clamp(0.0, 1.0)
        } else {
            1.0
        };
        (self.weight * fade_in * fade_out).clamp(0.0, 1.0)
    }
}

/// Montage 播放器：一个活动段 + 一个 FIFO 队列。
#[derive(Debug, Clone, Default)]
pub struct MontagePlayer {
    active: Option<ActiveMontage>,
    queue: VecDeque<Montage>,
    finished: bool,
    paused: bool,
    pending_notifies: Vec<MontageNotify>,
}

impl MontagePlayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 立即播放（替换当前段，清空队列）。返回是否接受。
    pub fn play_now(&mut self, montage: Montage) -> bool {
        self.active = Some(start(montage));
        self.queue.clear();
        self.finished = false;
        self.paused = false;
        self.pending_notifies.clear();
        true
    }

    /// 排队；空闲时立即开始。队列满时丢弃并返回 `false`。
    pub fn enqueue(&mut self, montage: Montage) -> bool {
        if self.active.is_none() {
            self.active = Some(start(montage));
            self.finished = false;
            return true;
        }
        if self.queue.len() >= MAX_MONTAGE_QUEUE {
            return false;
        }
        self.queue.push_back(montage);
        true
    }

    /// 停止并清空队列。
    pub fn stop(&mut self) {
        self.active = None;
        self.queue.clear();
        self.finished = false;
        self.paused = false;
        self.pending_notifies.clear();
    }

    /// 当前段播完后停止，不再接队列里的下一段。
    pub fn stop_after_current(&mut self) {
        self.queue.clear();
    }

    pub fn is_playing(&self) -> bool {
        self.active.is_some()
    }

    /// 暂停 / 恢复活动段（#560 Montage 播放控制）。暂停期间 `tick` 不推进时间、
    /// 不采样新的 Notify，队列和权重都保持不变。
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 活动段已经播放的时间（秒）；没有活动段返回 `None`。
    pub fn active_time(&self) -> Option<f32> {
        self.active.as_ref().map(|active| active.time)
    }

    /// 当前时间所在的 Montage 段；没有活动段 / 不在任何段内时返回 `None`。
    pub fn active_section(&self) -> Option<&ClipSection> {
        let active = self.active.as_ref()?;
        let local = local_time(
            active.montage.clip.duration,
            active.montage.looped,
            active.time,
        );
        active
            .montage
            .clip
            .sections
            .iter()
            .find(|section| section.contains(local))
    }

    /// 跳到命名 Montage 段的起点；段不存在或没有活动段时返回 `false`。
    ///
    /// 暂停状态保持不变：暂停中跳段后仍停在段首，恢复后才继续推进。
    pub fn jump_to_section(&mut self, name: &str) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let Some(section) = active
            .montage
            .clip
            .sections
            .iter()
            .find(|section| section.name == name)
        else {
            return false;
        };
        active.time = section.start;
        self.pending_notifies.clear();
        true
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub fn active_clip(&self) -> Option<&Arc<Clip>> {
        self.active.as_ref().map(|active| &active.montage.clip)
    }

    /// 当前实际混合权重（基准权重 × blend-in / blend-out 系数）。
    pub fn active_weight(&self) -> f32 {
        self.active
            .as_ref()
            .map_or(0.0, ActiveMontage::effective_weight)
    }

    pub fn set_active_weight(&mut self, weight: f32) {
        if let Some(active) = self.active.as_mut() {
            active.weight = sanitize_weight(weight);
        }
    }

    /// 推进时间；单次 Montage 播完时自动接下一段。返回是否仍在一段里。
    pub fn tick(&mut self, dt: f32) -> bool {
        if self.paused {
            return self.active.is_some();
        }
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let previous_local = local_time(
            active.montage.clip.duration,
            active.montage.looped,
            active.time,
        );
        let speed = sanitize_speed(active.montage.speed);
        active.time += dt * speed;
        // blend-in 是墙钟时长，不随播放倍速缩放；暂停时 tick 提前返回，所以会冻结。
        active.elapsed += dt;
        let local = local_time(
            active.montage.clip.duration,
            active.montage.looped,
            active.time,
        );
        let mut branch_target = None;
        if dt > 0.0 && speed >= 0.0 {
            for notify in active.montage.clip.notifies_between(previous_local, local) {
                if let Some(target_name) = notify.name.strip_prefix(BRANCH_IN_PREFIX) {
                    if let Some(target) = active
                        .montage
                        .clip
                        .sections
                        .iter()
                        .find(|section| section.name == target_name)
                    {
                        branch_target = Some(target.start);
                    }
                }
                self.pending_notifies.push(MontageNotify {
                    clip_name: active.montage.clip.name.clone(),
                    name: notify.name.clone(),
                    time: notify.time,
                });
            }
        }
        if let Some(start) = branch_target {
            // BranchIn 是显式跳段：不再按循环回绕，也不在同一帧继续扫目标段里的 Notify。
            active.time = start;
        } else if active.montage.looped {
            active.time = local;
        }

        let finished =
            !active.montage.looped && speed > 0.0 && active.time >= active.montage.clip.duration;
        if finished {
            if let Some(next) = self.queue.pop_front() {
                self.active = Some(start(next));
                self.finished = false;
            } else {
                self.active = None;
                self.finished = true;
            }
            return self.active.is_some();
        }
        true
    }

    /// 采样当前 Montage 到 `out`（先重置成 reference）。没有活动段返回 `false`。
    pub fn sample_into(&self, reference: &Pose, out: &mut Pose) -> bool {
        let Some(active) = self.active.as_ref() else {
            return false;
        };
        out.copy_from(reference);
        let local = local_time(
            active.montage.clip.duration,
            active.montage.looped,
            active.time,
        );
        active.montage.clip.sample_into(local, out);
        true
    }

    /// 取走这一帧跨过的 Notify（整体 drain）。
    pub fn take_notifies(&mut self) -> Vec<MontageNotify> {
        std::mem::take(&mut self.pending_notifies)
    }
}

fn start(montage: Montage) -> ActiveMontage {
    let section_start = montage
        .start_section
        .as_deref()
        .and_then(|name| {
            montage
                .clip
                .sections
                .iter()
                .find(|section| section.name == name)
        })
        .map(|section| section.start);
    let start_time = section_start.unwrap_or_else(|| {
        if montage.start_time.is_finite() {
            montage.start_time.max(0.0)
        } else {
            0.0
        }
    });
    ActiveMontage {
        weight: sanitize_weight(montage.weight),
        time: local_time(montage.clip.duration, montage.looped, start_time),
        elapsed: 0.0,
        montage,
    }
}

fn sanitize_weight(weight: f32) -> f32 {
    if weight.is_finite() {
        weight.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn sanitize_speed(speed: f32) -> f32 {
    if speed.is_finite() {
        speed
    } else {
        1.0
    }
}

fn sanitize_duration(seconds: f32) -> f32 {
    if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    }
}

fn local_time(duration: f32, looped: bool, time: f32) -> f32 {
    if duration <= 0.0 {
        return 0.0;
    }
    if looped {
        time.rem_euclid(duration)
    } else {
        time.clamp(0.0, duration)
    }
}

#[cfg(test)]
#[path = "montage_tests.rs"]
mod tests;
