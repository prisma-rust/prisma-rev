//! 多层姿态栈（#560 P1 的 BlendStack 基础层）。
//!
//! 一个 [`BlendStack`] 从基础姿态开始，按**添加顺序**把若干 [`BlendLayer`]
//! 叠上去。每层可以：
//!
//! - 是普通层（把当前结果朝该层姿态混合）或 additive 层（把相对 reference
//!   姿态的增量打上去，复用 [`crate::blend::additive_joint`]）；
//! - 带 per-bone 遮罩（0 = 不动这根骨骼，1 = 完全应用），用于「只让上半身
//!   呼吸」「只让右手瞄准」这类分层；
//! - 延迟 `activation_delay` 秒后才开始、再用 `blend_in_time` 秒淡入；
//! - 被 `max_active_blends` 限制同时生效的层数（超出的层不参与求值）。
//!
//! 这一层只做数学，不依赖 ECS / 渲染 / 资产格式；动画图求值器、编辑器预览和
//! 离线工具可以共用。权重语义与 UE 的 layered blend 一致：后添加的层在**当前
//! 结果**上继续混合，`weight = 1` 会完全覆盖前一层在受影响骨骼上的结果。
//!
//! 约定与 `prism-anim` 其余部分一致：四元数 `xyzw`、右手 +Y up、姿态是每根
//! 骨骼的局部 TRS。

use crate::blend::{additive_joint, blend_joint};
use crate::pose::Pose;
use crate::skeleton::{JointTransform, Skeleton};

/// 每根骨骼的混合权重，`0` = 完全不动，`1` = 完全应用。
///
/// 空遮罩表示「所有骨骼都是 1」，这是默认值，不需要为整身层分配一个全 1 的
/// `Vec`。非空时，超出遮罩长度的骨骼按 `0` 处理：骨架版本不一致时宁可不动
/// 未知骨骼，也不要把它们误当成全身权重。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoneMask {
    weights: Vec<f32>,
}

impl BoneMask {
    /// 全身遮罩（等价于没有遮罩）。
    pub fn all() -> Self {
        Self::default()
    }

    /// 从逐骨骼权重构造；NaN 按 `0`，其余夹到 `[0, 1]`。
    pub fn from_weights(weights: Vec<f32>) -> Self {
        Self {
            weights: weights
                .into_iter()
                .map(|weight| {
                    if weight.is_nan() {
                        0.0
                    } else {
                        weight.clamp(0.0, 1.0)
                    }
                })
                .collect(),
        }
    }

    /// 按骨骼名构造：列出的骨骼为 1，其余为 0；找不到的名字忽略。
    ///
    /// 结果长度等于骨架关节数，所以索引顺序与姿态一致。
    pub fn from_names(skeleton: &Skeleton, names: &[&str]) -> Self {
        let mut weights = vec![0.0; skeleton.joint_count()];
        for name in names {
            if let Some(index) = skeleton.joint_index(name) {
                weights[index] = 1.0;
            }
        }
        Self { weights }
    }

    /// 是否作用到所有骨骼。
    pub fn is_all(&self) -> bool {
        self.weights.is_empty()
    }

    /// 某根骨骼的权重。空遮罩 = 1；超出范围 = 0。
    pub fn weight(&self, joint: usize) -> f32 {
        if self.weights.is_empty() {
            1.0
        } else {
            self.weights.get(joint).copied().unwrap_or(0.0)
        }
    }

    /// 逐骨骼权重（空 = 全身）。
    pub fn weights(&self) -> &[f32] {
        &self.weights
    }
}

/// 叠加到 [`BlendStack`] 上的一层。
#[derive(Clone, Debug)]
pub struct BlendLayer {
    /// 该层本帧的姿态（调用方从剪辑 / 混合空间采样后写进来）。
    pose: Pose,
    /// 基础权重，`0..=1`；NaN / 负数按 0 处理。
    weight: f32,
    /// per-bone 遮罩。
    mask: BoneMask,
    /// `true` = 相对 reference 姿态的增量层。
    additive: bool,
    /// 压入后至少等这么多秒才开始生效。
    activation_delay: f32,
    /// 生效后再用这么多秒从 0 淡到 `weight`；`0` = 立即到满权重。
    blend_in_time: f32,
    /// 压入后经过的时间（由 [`BlendStack::advance`] 推进）。
    elapsed: f32,
}

impl BlendLayer {
    /// 普通层，默认全身、权重 1、无延迟、立即生效。
    pub fn new(pose: Pose) -> Self {
        Self {
            pose,
            weight: 1.0,
            mask: BoneMask::all(),
            additive: false,
            activation_delay: 0.0,
            blend_in_time: 0.0,
            elapsed: 0.0,
        }
    }

    pub fn with_weight(mut self, weight: f32) -> Self {
        self.weight = weight;
        self
    }

    pub fn with_mask(mut self, mask: BoneMask) -> Self {
        self.mask = mask;
        self
    }

    pub fn with_additive(mut self, additive: bool) -> Self {
        self.additive = additive;
        self
    }

    /// 设置延迟与淡入时长（负数按 0）。
    pub fn with_timing(mut self, activation_delay: f32, blend_in_time: f32) -> Self {
        self.activation_delay = sanitize_seconds(activation_delay);
        self.blend_in_time = sanitize_seconds(blend_in_time);
        self
    }

    pub fn pose(&self) -> &Pose {
        &self.pose
    }

    pub fn pose_mut(&mut self) -> &mut Pose {
        &mut self.pose
    }

    pub fn weight(&self) -> f32 {
        self.weight
    }

    pub fn set_weight(&mut self, weight: f32) {
        self.weight = weight;
    }

    pub fn mask(&self) -> &BoneMask {
        &self.mask
    }

    pub fn is_additive(&self) -> bool {
        self.additive
    }

    pub fn elapsed(&self) -> f32 {
        self.elapsed
    }

    /// 从头开始计时（例如层被重新激活时）。
    pub fn reset_elapsed(&mut self) {
        self.elapsed = 0.0;
    }

    /// 当前有效权重：基础权重 × 延迟/淡入系数 × 遮罩（在求值时再乘）。
    fn effective_weight(&self) -> f32 {
        let weight = sanitize_weight(self.weight);
        if weight <= 0.0 || self.elapsed < self.activation_delay {
            return 0.0;
        }
        if self.blend_in_time <= 0.0 {
            return weight;
        }
        let t = ((self.elapsed - self.activation_delay) / self.blend_in_time).clamp(0.0, 1.0);
        weight * t
    }
}

/// 多层姿态栈。
#[derive(Clone, Debug)]
pub struct BlendStack {
    base: Pose,
    /// additive 层使用的参考（bind）姿态。
    reference: Pose,
    layers: Vec<BlendLayer>,
    /// 同时参与求值的普通 + additive 层上限；`0` = 不限制。
    max_active_blends: usize,
}

impl BlendStack {
    /// 用基础姿态和 additive 参考姿态建栈。
    pub fn new(base: Pose, reference: Pose) -> Self {
        Self {
            base,
            reference,
            layers: Vec::new(),
            max_active_blends: 0,
        }
    }

    pub fn base(&self) -> &Pose {
        &self.base
    }

    pub fn base_mut(&mut self) -> &mut Pose {
        &mut self.base
    }

    pub fn reference(&self) -> &Pose {
        &self.reference
    }

    pub fn set_reference(&mut self, reference: Pose) {
        self.reference = reference;
    }

    pub fn layers(&self) -> &[BlendLayer] {
        &self.layers
    }

    pub fn layers_mut(&mut self) -> &mut Vec<BlendLayer> {
        &mut self.layers
    }

    /// 压入一层，返回它的下标。
    pub fn push(&mut self, layer: BlendLayer) -> usize {
        let index = self.layers.len();
        self.layers.push(layer);
        index
    }

    /// 移除一层；越界返回 `None`。
    pub fn remove(&mut self, index: usize) -> Option<BlendLayer> {
        (index < self.layers.len()).then(|| self.layers.remove(index))
    }

    pub fn clear(&mut self) {
        self.layers.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// 限制同时生效的层数；`0` 表示不限制。超出的层按**添加顺序**不参与。
    pub fn set_max_active_blends(&mut self, max_active: usize) {
        self.max_active_blends = max_active;
    }

    pub fn max_active_blends(&self) -> usize {
        self.max_active_blends
    }

    /// 推进每层的计时（`dt` 非有限或为负时按 0）。
    pub fn advance(&mut self, dt: f32) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        for layer in &mut self.layers {
            layer.elapsed = (layer.elapsed + dt).min(86_400.0);
        }
    }

    /// 求值到 `out`：以 base 为起点，按顺序叠加所有生效层。
    ///
    /// 失败（base 为空）时不动 `out`。调用方可以每帧复用同一个 `out`，避免
    /// 为每个角色分配新姿态。
    pub fn evaluate_into(&self, out: &mut Pose) {
        if self.base.is_empty() {
            return;
        }
        out.copy_from(&self.base);
        let joint_count = out.len();
        let mut active = 0usize;

        for layer in &self.layers {
            let weight = layer.effective_weight();
            if weight <= 0.0 {
                continue;
            }
            if self.max_active_blends > 0 && active >= self.max_active_blends {
                break;
            }
            active += 1;

            let shared = joint_count.min(layer.pose.len());
            for joint in 0..shared {
                let mask = layer.mask.weight(joint);
                let weight = weight * mask;
                if weight <= 0.0 {
                    continue;
                }
                let (Some(current), Some(target)) = (out.local(joint), layer.pose.local(joint))
                else {
                    continue;
                };
                if layer.additive {
                    let reference = self
                        .reference
                        .local(joint)
                        .unwrap_or(JointTransform::IDENTITY);
                    out.set_local(joint, additive_joint(&current, &target, &reference, weight));
                } else {
                    out.set_local(joint, blend_joint(&current, &target, weight));
                }
            }
        }
    }
}

fn sanitize_weight(weight: f32) -> f32 {
    if weight.is_finite() {
        weight.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn sanitize_seconds(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
#[path = "blend_stack_tests.rs"]
mod tests;
