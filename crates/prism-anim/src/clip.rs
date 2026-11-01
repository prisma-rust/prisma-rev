//! 动画剪辑：关键帧轨道与采样。

use glam::{Quat, Vec3};

use crate::pose::Pose;

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

/// 一段动画剪辑。
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub name: String,
    pub duration: f32,
    pub fps: f32,
    pub looped: bool,
    pub additive: bool,
    pub tracks: Vec<Track>,
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
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.iter().all(Track::is_empty)
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

    /// 先按剪辑自己的 `looped` 折算时间再采样（单独使用剪辑时的方便入口）。
    pub fn sample_wrapped_into(&self, time: f32, pose: &mut Pose) -> bool {
        let local = self.wrap_time(time);
        self.sample_into(local, pose)
    }

    /// 轨道覆盖的时间上限（没有关键帧时是 `duration`）。
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
