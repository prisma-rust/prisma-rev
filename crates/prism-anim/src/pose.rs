//! 姿态：每个骨骼的局部 TRS。

use crate::skeleton::JointTransform;

/// 一组骨骼局部变换。长度与骨架一致；访问越界返回 `None`。
#[derive(Debug, Clone, PartialEq)]
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

    /// 重置成单位变换（全部骨骼回到原点，`scale = 1`）。
    pub fn identity(&mut self) {
        self.locals.fill(JointTransform::IDENTITY);
    }
}

#[cfg(test)]
#[path = "pose_tests.rs"]
mod tests;
