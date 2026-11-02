//! 姿态：每个骨骼的局部 TRS。

use crate::skeleton::JointTransform;

/// 一组骨骼局部变换。长度与骨架一致；访问越界返回 `None`。
///
/// `Default` 是**空姿态**（零根骨骼），不是单位姿态 —— 它只用于给
/// [`BlendSpace1D`](crate::blend::BlendSpace1D) 这类容器做初始值，调用方要先
/// 采样再读。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pose {
    locals: Vec<JointTransform>,
}

impl Pose {
    pub fn from_locals(locals: Vec<JointTransform>) -> Self {
        Self { locals }
    }

    pub fn len(&self) -> usize {
        self.locals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.locals.is_empty()
    }

    pub fn local(&self, index: usize) -> Option<JointTransform> {
        self.locals.get(index).copied()
    }

    pub fn set_local(&mut self, index: usize, transform: JointTransform) -> bool {
        match self.locals.get_mut(index) {
            Some(slot) => {
                *slot = transform;
                true
            }
            None => false,
        }
    }

    pub fn locals(&self) -> &[JointTransform] {
        &self.locals
    }

    /// 把姿态重设成 `count` 根关节，每根由 `fill` 给出初值。
    ///
    /// 容量够时不会重新分配：混合这类每帧都跑的路径靠它避免逐帧 `Vec` 分配。
    pub fn resize_with(&mut self, count: usize, mut fill: impl FnMut(usize) -> JointTransform) {
        self.locals.clear();
        // `reserve` 保证 `capacity >= len + count`（此时 len 已经是 0），容量够时是空操作。
        self.locals.reserve(count);
        for index in 0..count {
            self.locals.push(fill(index));
        }
    }

    /// 用另一个姿态覆盖自己（保留已分配的容量，不重新分配）。
    ///
    /// 混合空间每次求值都要先把 scratch 姿态重置成 reference 姿态，逐帧
    /// `clone()` 会白扔一块内存。
    pub fn copy_from(&mut self, other: &Pose) {
        self.locals.clear();
        self.locals.extend_from_slice(&other.locals);
    }

    /// 重置成单位变换（全部骨骼回到原点，`scale = 1`）。
    pub fn identity(&mut self) {
        self.locals.fill(JointTransform::IDENTITY);
    }
}

#[cfg(test)]
#[path = "pose_tests.rs"]
mod tests;
