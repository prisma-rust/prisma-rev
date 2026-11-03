//! 姿态混合与一维混合空间（#560 P1 的地基）。
//!
//! 动画图 / BlendStack / BlendSpace 都需要同一件事：**把两个姿态按权重合成
//! 一个**。这一层只做数学，不依赖 engine / render / ECS，所以编辑器预览、离线
//! 工具和运行时可以共用同一份实现。
//!
//! 约定与 `prism-anim` 其余部分一致：四元数 `xyzw`、右手 +Y up、`Pose` 是每根
//! 骨骼的**局部** TRS（父空间）。
//!
//! 两个刻意的设计选择：
//!
//! - 旋转走 `slerp`（最短路径），不是对四元数分量做 lerp —— 后者在接近 180°
//!   时会让骨骼"抽一下"，而这正是动作混合最常见的失败形态；
//! - 长度不一致时**只混合较短的关节数**，多出来的关节保留 `a` 的姿态。骨架
//!   版本不一致时宁可少混一根骨骼，也不要 panic 或把缺失关节塌到原点。

use std::sync::Arc;

use glam::{Quat, Vec3};

use crate::clip::Clip;
use crate::pose::Pose;
use crate::skeleton::JointTransform;
use crate::sync::SyncGroup;

/// 把 `t` 规范化到 `[0, 1]`（NaN → 0）。
fn clamp_weight(t: f32) -> f32 {
    if t.is_nan() {
        0.0
    } else {
        t.clamp(0.0, 1.0)
    }
}

/// 两个关节变换按权重混合：平移/缩放线性插值，旋转最短路径 slerp。
pub fn blend_joint(a: &JointTransform, b: &JointTransform, t: f32) -> JointTransform {
    JointTransform {
        translation: a.translation.lerp(b.translation, t),
        rotation: a.rotation.slerp(b.rotation, t),
        scale: a.scale.lerp(b.scale, t),
    }
}

/// 把 `a` → `b` 的混合写进 `out`，返回实际混合的关节数。
///
/// `out` 会被重设成 `a` 的长度：多出来的关节（`b` 没有的部分）保持 `a` 的
/// 姿态，`out` 里原本多出来的关节会被丢弃。返回的关节数是两者中较短的。
pub fn blend_into(out: &mut Pose, a: &Pose, b: &Pose, t: f32) -> usize {
    let t = clamp_weight(t);
    let shared = a.len().min(b.len());
    // `out` 与 `a` / `b` 一定是不同的姿态（借用检查器保证），所以可以边算边写，
    // 不需要中间缓冲。
    out.resize_with(a.len(), |index| {
        let from = a.local(index).unwrap_or(JointTransform::IDENTITY);
        match b.local(index).filter(|_| index < shared) {
            Some(to) => blend_joint(&from, &to, t),
            None => from,
        }
    });
    shared
}

/// 把叠加层（additive）姿态按权重打到底层姿态上，返回实际处理的关节数。
///
/// 语义与 UE 的「local space additive」一致：叠加剪辑的局部变换相对**参考姿态**
/// 的差值才是增量 ——
///
/// - 平移：`base + (additive - reference) * t`
/// - 旋转：`base * slerp(identity, reference⁻¹ * additive, t)`
/// - 缩放：`base * lerp(1, additive / reference, t)`（参考缩放为 0 时当 1）
///
/// `t = 0` 时结果与 `base` 完全一致，`t = 1` 时是完整的叠加结果。就地改写 `base`，
/// 没有中间缓冲：叠加层只改一部分关节时，剩下的关节保持不动。
///
/// 这是「Layer + Additive」的基础：呼吸、受击抖动、瞄准偏移这类只该改一部分骨骼的
/// 剪辑，不需要再做一整套基础剪辑（#560 P1）。
pub fn apply_additive(base: &mut Pose, additive: &Pose, reference: &Pose, weight: f32) -> usize {
    let t = clamp_weight(weight);
    if t <= 0.0 {
        return 0;
    }
    let shared = base.len().min(additive.len()).min(reference.len());
    for index in 0..shared {
        let (Some(current), Some(layer), Some(rest)) = (
            base.local(index),
            additive.local(index),
            reference.local(index),
        ) else {
            continue;
        };
        base.set_local(index, additive_joint(&current, &layer, &rest, t));
    }
    shared
}

/// 单个关节的叠加结果（见 [`apply_additive`]）。
pub fn additive_joint(
    base: &JointTransform,
    additive: &JointTransform,
    reference: &JointTransform,
    weight: f32,
) -> JointTransform {
    let t = clamp_weight(weight);
    if t <= 0.0 {
        return *base;
    }
    // 参考姿态的逆：旋转取共轭，缩放取倒数。
    let rest_rotation = reference.rotation.normalize();
    let delta_rotation = rest_rotation.inverse() * additive.rotation.normalize();
    let rotation =
        (base.rotation.normalize() * Quat::IDENTITY.slerp(delta_rotation, t)).normalize();

    let delta_translation = additive.translation - reference.translation;
    let translation = base.translation + delta_translation * t;

    let scale_ratio = |additive: f32, reference: f32| -> f32 {
        if reference.abs() <= 1e-6 {
            1.0
        } else {
            additive / reference
        }
    };
    let scale = Vec3::new(
        scale_ratio(additive.scale.x, reference.scale.x),
        scale_ratio(additive.scale.y, reference.scale.y),
        scale_ratio(additive.scale.z, reference.scale.z),
    );
    let scale = base.scale * scale.lerp(Vec3::ONE, 1.0 - t);

    JointTransform {
        translation,
        rotation,
        scale,
    }
}

/// [`blend_into`] 的分配版本，方便工具与测试。
pub fn blend_poses(a: &Pose, b: &Pose, t: f32) -> Pose {
    let mut out = Pose::from_locals(Vec::new());
    blend_into(&mut out, a, b, t);
    out
}

/// 一维混合空间的一个采样点：`position` 是参数轴上的位置，`clip` 是它对应的剪辑。
#[derive(Debug, Clone)]
pub struct BlendSample {
    pub position: f32,
    pub clip: Arc<Clip>,
}

/// 一维混合空间（速度 0 → 走 → 跑 这种）。
///
/// `evaluate_into` 的 `phase` 是**归一化时间**（0..1，按剪辑自己的时长循环）：
/// 混合空间里各剪辑长度通常不同，按秒同步会让两个动画互相追着跑，归一化相位
/// 才是「步幅对齐」的正确做法。
///
/// 每个剪辑在采样前都会先把姿态重置成 `reference`（一般是骨架的 reference
/// 姿态）：剪辑只覆盖部分轨道时，没被覆盖的关节保持 reference，而不是塌成
/// 单位变换 —— 与 `Animator` 的求值顺序一致。[`Clip::sample_into`] 只写已存在
/// 的关节，所以这一步是必须的。
#[derive(Debug, Clone, Default)]
pub struct BlendSpace1D {
    samples: Vec<BlendSample>,
    /// `samples` 的位置轴（与 `samples` 同步排序），每帧求值不再现算一遍。
    positions: Vec<f32>,
    reference: Pose,
    /// 所有样本都有同步标记时，用第一个样本做领跑剪辑映射相位。
    sync: Option<SyncGroup>,
    scratch: [Pose; 2],
}

impl BlendSpace1D {
    /// 建一个混合空间：丢掉非有限位置并按位置排序（作者填顺序无所谓）。
    pub fn new(samples: Vec<BlendSample>, reference: Pose) -> Self {
        let mut samples: Vec<BlendSample> = samples
            .into_iter()
            .filter(|sample| sample.position.is_finite())
            .collect();
        samples.sort_by(|a, b| a.position.total_cmp(&b.position));
        let positions = samples.iter().map(|sample| sample.position).collect();
        let sync = samples
            .iter()
            .map(|sample| sample.clip.sync_marker_set())
            .collect::<Option<Vec<_>>>()
            .and_then(SyncGroup::new)
            .filter(|group| group.len() == samples.len());
        Self {
            samples,
            positions,
            reference,
            sync,
            scratch: [Pose::default(), Pose::default()],
        }
    }

    /// 样本 `index` 实际使用的归一化相位。
    ///
    /// 有同步组时以当前权重最大的样本（`leader`）为领跑剪辑，把 phase 映射到
    /// 每个样本自己的步幅；没有标记 / 标记结构不一致时所有样本共用同一个 phase。
    fn phase_for(&self, leader: usize, index: usize, phase: f32) -> f32 {
        self.sync
            .as_ref()
            .filter(|group| leader < group.len() && index < group.len())
            .map_or(phase, |group| group.map_phase(leader, index, phase))
    }

    /// 采样前重置用的姿态。
    pub fn reference(&self) -> &Pose {
        &self.reference
    }

    pub fn samples(&self) -> &[BlendSample] {
        &self.samples
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 参数轴范围；空混合空间返回 `None`。
    pub fn range(&self) -> Option<(f32, f32)> {
        let first = self.samples.first()?.position;
        let last = self.samples.last()?.position;
        Some((first, last))
    }

    /// 当前参数下权重最大的样本下标（Notify 的 Dominant 策略用）。
    ///
    /// 正好在左右样本中点时取右侧；只有一个样本时返回它。
    pub fn dominant_sample(&self, at: f32) -> Option<usize> {
        let (left, right, t) = blend_space_weights(&self.positions, at)?;
        Some(if t < 0.5 { left } else { right })
    }

    /// 在 `at` 处求值到 `out`。返回是否真的写入了姿态（空空间 / 采样失败 → `false`）。
    ///
    /// 失败时**不动** `out`：调用方可以继续用上一帧的姿态，而不是拿到一个空骨架。
    pub fn evaluate_into(&mut self, at: f32, phase: f32, out: &mut Pose) -> bool {
        let Some((left, right, t)) = blend_space_weights(&self.positions, at) else {
            return false;
        };
        let leader = self.dominant_sample(at).unwrap_or(0);

        if left == right {
            self.scratch[0].copy_from(&self.reference);
            let phase = self.phase_for(leader, left, phase);
            if !sample_at_phase(&self.samples[left].clip, phase, &mut self.scratch[0]) {
                return false;
            }
            out.copy_from(&self.scratch[0]);
            return true;
        }

        self.scratch[0].copy_from(&self.reference);
        self.scratch[1].copy_from(&self.reference);
        let left_phase = self.phase_for(leader, left, phase);
        let right_phase = self.phase_for(leader, right, phase);
        if !sample_at_phase(&self.samples[left].clip, left_phase, &mut self.scratch[0]) {
            return false;
        }
        if !sample_at_phase(&self.samples[right].clip, right_phase, &mut self.scratch[1]) {
            return false;
        }
        blend_into(out, &self.scratch[0], &self.scratch[1], t);
        true
    }

    /// 每个样本用**自己的相位**求值（独立播放头，每样本速度不同时用）。
    ///
    /// `phases[i]` 是样本 `i` 的归一化相位；缺失的样本按 0 采样。这个入口不做
    /// SyncGroup 相位映射——调用方已经为每个样本算好了自己的时间。
    pub fn evaluate_into_with_phases(&mut self, at: f32, phases: &[f32], out: &mut Pose) -> bool {
        let Some((left, right, t)) = blend_space_weights(&self.positions, at) else {
            return false;
        };
        let phase = |index: usize| phases.get(index).copied().unwrap_or(0.0);

        if left == right {
            self.scratch[0].copy_from(&self.reference);
            if !sample_at_phase(&self.samples[left].clip, phase(left), &mut self.scratch[0]) {
                return false;
            }
            out.copy_from(&self.scratch[0]);
            return true;
        }

        self.scratch[0].copy_from(&self.reference);
        self.scratch[1].copy_from(&self.reference);
        if !sample_at_phase(&self.samples[left].clip, phase(left), &mut self.scratch[0]) {
            return false;
        }
        if !sample_at_phase(
            &self.samples[right].clip,
            phase(right),
            &mut self.scratch[1],
        ) {
            return false;
        }
        blend_into(out, &self.scratch[0], &self.scratch[1], t);
        true
    }
}

/// 一维混合空间的位置插值：返回 `(左样本下标, 右样本下标, t)`。
///
/// 纯函数，便于单测。`positions` 必须按升序排列（[`BlendSpace1D::new`] 保证）。
/// 范围外夹到两端（`t = 0`），只有一个样本时左右都指向它，空表 / 非有限查询
/// 返回 `None`。
pub fn blend_space_weights(positions: &[f32], at: f32) -> Option<(usize, usize, f32)> {
    if positions.is_empty() || !at.is_finite() {
        return None;
    }
    let last = positions.len() - 1;
    if at <= positions[0] {
        return Some((0, 0, 0.0));
    }
    if at >= positions[last] {
        return Some((last, last, 0.0));
    }
    // 线性扫描：混合空间的样本数是作者手填的量级（个位数到几十），二分不值得。
    for index in 0..last {
        let (low, high) = (positions[index], positions[index + 1]);
        if at >= low && at <= high {
            let span = high - low;
            let t = if span <= f32::EPSILON {
                0.0
            } else {
                (at - low) / span
            };
            return Some((index, index + 1, t));
        }
    }
    // 未排序 / 含 NaN 的输入：夹到最后一个样本，而不是返回一个越界下标。
    Some((last, last, 0.0))
}

/// 按归一化相位采样一个剪辑（越界循环，`phase` 非有限时当 0）。
pub(crate) fn sample_at_phase(clip: &Clip, phase: f32, out: &mut Pose) -> bool {
    let phase = if phase.is_finite() { phase } else { 0.0 };
    let wrapped = phase - phase.floor();
    clip.sample_wrapped_into(clip.duration * wrapped, out)
}

#[cfg(test)]
#[path = "blend_tests.rs"]
mod tests;
