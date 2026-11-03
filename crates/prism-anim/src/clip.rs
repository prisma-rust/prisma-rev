//! 动画剪辑：关键帧轨道与采样。

use glam::{Quat, Vec3};

use crate::pose::Pose;
use crate::sync::SyncMarkerSet;

/// 单个骨骼的一条通道关键帧（时间升序）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3Key {
    pub time: f32,
    pub value: Vec3,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuatKey {
    pub time: f32,
    pub value: Quat,
}

/// 一条轨道：一个骨骼的 position / rotation / scale 通道。
///
/// 空通道表示「这个剪辑不动这条通道」，采样时保留姿态里原有的值。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Track {
    pub joint: usize,
    pub translation: Vec<Vec3Key>,
    pub rotation: Vec<QuatKey>,
    pub scale: Vec<Vec3Key>,
}

impl Track {
    pub fn is_empty(&self) -> bool {
        self.translation.is_empty() && self.rotation.is_empty() && self.scale.is_empty()
    }
}

/// 剪辑里的一个关键帧事件（#560 P1 Notify）。
///
/// `name` 由项目解释（`Footstep.L` / `Traversal.Window` / …）：动画层只负责「在这一帧
/// 跨过了这个时间点」的事实，不决定它意味着什么。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipNotify {
    /// 触发时间（秒，剪辑本地时间）。
    pub time: f32,
    pub name: String,
}

/// 一段 Montage 区间（#560 P4）：按名字跳转 / 调试时显示。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipSection {
    pub name: String,
    /// 段起点（秒，剪辑本地时间）。
    pub start: f32,
    /// 段终点（秒，剪辑本地时间）；可以等于 `start`。
    pub end: f32,
}

impl ClipSection {
    /// 这个时间点是否落在段内（左闭右开；零长度段命中 `start`）。
    pub fn contains(&self, time: f32) -> bool {
        if !time.is_finite() || !self.start.is_finite() || !self.end.is_finite() {
            return false;
        }
        if self.end <= self.start {
            (time - self.start).abs() <= f32::EPSILON
        } else {
            time >= self.start && time < self.end
        }
    }
}

/// 一段 MotionWarping 窗口（#560 P4）：root motion 只在这个时间区间内允许被
/// 目标点 / 朝向扭曲。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipWarpWindow {
    pub name: String,
    pub start: f32,
    pub end: f32,
}

impl ClipWarpWindow {
    pub fn contains(&self, time: f32) -> bool {
        if !time.is_finite() || !self.start.is_finite() || !self.end.is_finite() {
            return false;
        }
        if self.end <= self.start {
            (time - self.start).abs() <= f32::EPSILON
        } else {
            time >= self.start && time < self.end
        }
    }

    /// 窗口内的归一化进度；零长度窗口在命中时返回 1。
    pub fn progress(&self, time: f32) -> Option<f32> {
        if !self.contains(time) {
            return None;
        }
        if self.end <= self.start {
            return Some(1.0);
        }
        Some(((time - self.start) / (self.end - self.start)).clamp(0.0, 1.0))
    }
}

/// 一段动画剪辑。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Clip {
    pub name: String,
    pub duration: f32,
    pub fps: f32,
    pub looped: bool,
    pub additive: bool,
    pub tracks: Vec<Track>,
    /// 关键帧事件（时间升序由 `from_info` / 编码器保证，这里不再排序）。
    pub notifies: Vec<ClipNotify>,
    /// 同步标记的归一化相位（升序；`from_info` 保证有限、去重）。
    pub sync_markers: Vec<f32>,
    /// Montage 段（按 `start` 升序；普通剪辑为空）。
    pub sections: Vec<ClipSection>,
    /// MotionWarping 窗口（按 `start` 升序；普通剪辑为空）。
    pub warp_windows: Vec<ClipWarpWindow>,
}

impl Clip {
    pub fn from_info(info: &prism_asset::formats_skinned::AnimClipInfo) -> Self {
        let tracks = info
            .tracks
            .iter()
            .map(|track| Track {
                joint: track.joint as usize,
                translation: track
                    .positions
                    .iter()
                    .filter(|key| key.time.is_finite() && key.value.iter().all(|v| v.is_finite()))
                    .map(|key| Vec3Key {
                        time: key.time,
                        value: Vec3::from_array(key.value),
                    })
                    .collect(),
                rotation: track
                    .rotations
                    .iter()
                    .filter(|key| key.time.is_finite() && key.value.iter().all(|v| v.is_finite()))
                    .map(|key| QuatKey {
                        time: key.time,
                        value: normalize_quat(Quat::from_array(key.value)),
                    })
                    .collect(),
                scale: track
                    .scales
                    .iter()
                    .filter(|key| key.time.is_finite() && key.value.iter().all(|v| v.is_finite()))
                    .map(|key| Vec3Key {
                        time: key.time,
                        value: Vec3::from_array(key.value),
                    })
                    .collect(),
            })
            .collect();
        Self {
            name: info.name.clone(),
            duration: if info.duration.is_finite() && info.duration > 0.0 {
                info.duration
            } else {
                info.clip_duration()
            },
            fps: info.fps,
            looped: info.looped,
            additive: info.additive,
            tracks,
            notifies: info
                .notifies
                .iter()
                .filter(|notify| notify.time.is_finite() && !notify.name.is_empty())
                .map(|notify| ClipNotify {
                    time: notify.time,
                    name: notify.name.clone(),
                })
                .collect(),
            sync_markers: info
                .sync_markers
                .iter()
                .copied()
                .filter(|phase| phase.is_finite())
                .map(|phase| phase.rem_euclid(1.0))
                .collect(),
            sections: info
                .sections
                .iter()
                .filter(|section| {
                    !section.name.trim().is_empty()
                        && section.start.is_finite()
                        && section.end.is_finite()
                        && section.start >= 0.0
                        && section.end >= section.start
                })
                .map(|section| ClipSection {
                    name: section.name.clone(),
                    start: section.start,
                    end: section.end,
                })
                .collect(),
            warp_windows: info
                .warp_windows
                .iter()
                .filter(|window| {
                    !window.name.trim().is_empty()
                        && window.start.is_finite()
                        && window.end.is_finite()
                        && window.start >= 0.0
                        && window.end >= window.start
                })
                .map(|window| ClipWarpWindow {
                    name: window.name.clone(),
                    start: window.start,
                    end: window.end,
                })
                .collect(),
        }
    }

    /// 这段剪辑的同步标记；没有标记时返回 `None`。
    pub fn sync_marker_set(&self) -> Option<SyncMarkerSet> {
        SyncMarkerSet::new(self.sync_markers.iter().copied())
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.iter().all(Track::is_empty)
    }

    /// 这一帧跨越的事件：区间 `(from, to]`；循环剪辑跨过结尾时把 `[0, to]` 也算上。
    ///
    /// `from` / `to` 都是**已折算的本地时间**（`0..=duration`）。起点取开区间是有意的：
    /// 事件在它恰好等于 `from` 的那一帧已经触发过，再用闭区间会让停在某帧上反复播。
    /// 结果按时间升序。
    pub fn notifies_between(&self, from: f32, to: f32) -> Vec<&ClipNotify> {
        if self.notifies.is_empty() || !from.is_finite() || !to.is_finite() {
            return Vec::new();
        }
        let duration = self.duration;
        if !duration.is_finite() || duration <= 0.0 {
            return Vec::new();
        }
        let mut fired: Vec<&ClipNotify> = Vec::new();
        // `to < from` 只可能是循环回绕（`rem_euclid` 之后），分两段扫。
        let wrapped = to < from;
        for notify in &self.notifies {
            let hit = if wrapped {
                (notify.time > from && notify.time <= duration)
                    || (notify.time >= 0.0 && notify.time <= to)
            } else {
                notify.time > from && notify.time <= to
            };
            if hit {
                fired.push(notify);
            }
        }
        fired.sort_by(|a, b| a.time.total_cmp(&b.time));
        fired
    }

    /// 把本地时间折算成采样时间：循环剪辑取余、单次剪辑夹到 `[0, duration]`。
    pub fn wrap_time(&self, time: f32) -> f32 {
        if !time.is_finite() {
            return 0.0;
        }
        if self.duration <= 0.0 {
            return 0.0;
        }
        if self.looped {
            time.rem_euclid(self.duration)
        } else {
            time.clamp(0.0, self.duration)
        }
    }

    /// 把剪辑在 `time` 处的采样写进 `pose`，返回是否有任何通道被写入。
    ///
    /// `time` 是**局部时间**：调用方决定要不要先过 [`Clip::wrap_time`]
    /// （`Animator` 组件允许覆盖剪辑自带的循环设置，所以这里不替它绕回）。
    pub fn sample_into(&self, time: f32, pose: &mut Pose) -> bool {
        let time = if time.is_finite() { time } else { 0.0 };
        let mut touched = false;
        for track in &self.tracks {
            let Some(current) = pose.local(track.joint) else {
                continue;
            };
            let mut next = current;
            if let Some(value) = sample_vec3(&track.translation, time) {
                next.translation = value;
                touched = true;
            }
            if let Some(value) = sample_quat(&track.rotation, time) {
                next.rotation = value;
                touched = true;
            }
            if let Some(value) = sample_vec3(&track.scale, time) {
                next.scale = value;
                touched = true;
            }
            if next != current {
                pose.set_local(track.joint, next);
            }
        }
        touched
    }

    /// 只采样一个关节（动画 LOD 仅根骨 / 根运动用），返回该关节是否有轨道。
    ///
    /// 没被采样到的关节保持 `pose` 里的值；轨道缺少某个通道（平移 / 旋转 / 缩放）
    /// 时那个通道也不动，和 [`Self::sample_into`] 的语义一致。越界关节返回
    /// `false`。
    pub fn sample_joint_into(&self, time: f32, joint: usize, pose: &mut Pose) -> bool {
        let time = if time.is_finite() { time } else { 0.0 };
        let Some(current) = pose.local(joint) else {
            return false;
        };
        let mut touched = false;
        let mut next = current;
        for track in &self.tracks {
            if track.joint != joint {
                continue;
            }
            if let Some(value) = sample_vec3(&track.translation, time) {
                next.translation = value;
                touched = true;
            }
            if let Some(value) = sample_quat(&track.rotation, time) {
                next.rotation = value;
                touched = true;
            }
            if let Some(value) = sample_vec3(&track.scale, time) {
                next.scale = value;
                touched = true;
            }
        }
        if next != current {
            pose.set_local(joint, next);
        }
        touched
    }

    /// 先按剪辑自己的 `looped` 折算时间再采样（单独使用剪辑时的方便入口）。
    pub fn sample_wrapped_into(&self, time: f32, pose: &mut Pose) -> bool {
        let local = self.wrap_time(time);
        self.sample_into(local, pose)
    }

    /// 轨道覆盖的时间上限（没有关键帧时是 `duration`）。
    /// 所有轨道用到的关键帧时间点（升序、去重、只要有限值）。
    ///
    /// 编辑器画时间轴刻度用：剪辑里各轨道的关键帧往往错开，合并后才是「这个剪辑
    /// 在哪些时刻会变」。
    pub fn key_times(&self) -> Vec<f32> {
        let mut times: Vec<f32> = Vec::new();
        for track in &self.tracks {
            for key in &track.translation {
                times.push(key.time);
            }
            for key in &track.rotation {
                times.push(key.time);
            }
            for key in &track.scale {
                times.push(key.time);
            }
        }
        times.retain(|time| time.is_finite());
        times.sort_by(|a, b| a.total_cmp(b));
        times.dedup_by(|a, b| (*a - *b).abs() <= 1e-6);
        times
    }

    pub fn max_key_time(&self) -> f32 {
        let mut max = 0.0f32;
        for track in &self.tracks {
            for key in &track.translation {
                max = max.max(key.time);
            }
            for key in &track.rotation {
                max = max.max(key.time);
            }
            for key in &track.scale {
                max = max.max(key.time);
            }
        }
        max
    }
}

/// 时间 `time` 处的值：首帧前/末帧后保持端点，中间线性插值。
pub fn sample_vec3(keys: &[Vec3Key], time: f32) -> Option<Vec3> {
    let (first, rest) = keys.split_first()?;
    if rest.is_empty() || time <= first.time {
        return Some(first.value);
    }
    let last = rest.last()?;
    if time >= last.time {
        return Some(last.value);
    }
    match find_span(keys, time, |key| key.time) {
        Some((before, after)) => {
            let span = after.time - before.time;
            if span <= f32::EPSILON {
                Some(before.value)
            } else {
                let t = (time - before.time) / span;
                Some(before.value.lerp(after.value, t))
            }
        }
        // 只有一个关键帧覆盖这个时间点。
        None => Some(keys[0].value),
    }
}

/// 四元数版本：slerp（自动走短弧）。
pub fn sample_quat(keys: &[QuatKey], time: f32) -> Option<Quat> {
    let (first, rest) = keys.split_first()?;
    if rest.is_empty() || time <= first.time {
        return Some(first.value);
    }
    let last = rest.last()?;
    if time >= last.time {
        return Some(last.value);
    }
    match find_span(keys, time, |key| key.time) {
        Some((before, after)) => {
            let span = after.time - before.time;
            if span <= f32::EPSILON {
                Some(before.value)
            } else {
                let t = (time - before.time) / span;
                Some(normalize_quat(before.value.slerp(after.value, t)))
            }
        }
        None => Some(keys[0].value),
    }
}

/// 二分找 `key.time <= time` 的最后一帧及其后继。
fn find_span<T>(keys: &[T], time: f32, time_of: impl Fn(&T) -> f32) -> Option<(&T, &T)> {
    let mut low = 0usize;
    let mut high = keys.len() - 1;
    while low + 1 < high {
        let middle = (low + high) / 2;
        if time_of(&keys[middle]) <= time {
            low = middle;
        } else {
            high = middle;
        }
    }
    Some((&keys[low], &keys[high]))
}

fn normalize_quat(rotation: Quat) -> Quat {
    if rotation.length_squared() > 1e-12 {
        rotation.normalize()
    } else {
        Quat::IDENTITY
    }
}

#[cfg(test)]
#[path = "clip_tests.rs"]
mod tests;
