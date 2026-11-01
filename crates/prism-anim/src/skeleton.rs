//! 骨架：层次、参考姿态与 `bone_palette`。

use glam::{Mat4, Quat, Vec3};

use crate::pose::Pose;

/// 一条骨骼的局部变换（与 cooked 格式的 10 个 f32 一一对应）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointTransform {
    pub translation: Vec3,
    /// 四元数，`xyzw` 布局（`glam::Quat`）。
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Default for JointTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl JointTransform {
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };

    pub fn new(translation: Vec3, rotation: Quat, scale: Vec3) -> Self {
        Self {
            translation,
            rotation: rotation.normalize(),
            scale,
        }
    }

    /// 从 cooked 布局（`t.xyz, r.xyzw, s.xyz`）解包。
    pub fn from_array(values: [f32; 10]) -> Self {
        let rotation = Quat::from_xyzw(values[3], values[4], values[5], values[6]);
        Self {
            translation: Vec3::new(values[0], values[1], values[2]),
            rotation: normalize_quat(rotation),
            scale: Vec3::new(values[7], values[8], values[9]),
        }
    }

    /// 打包回 cooked 布局。
    pub fn to_array(self) -> [f32; 10] {
        [
            self.translation.x,
            self.translation.y,
            self.translation.z,
            self.rotation.x,
            self.rotation.y,
            self.rotation.z,
            self.rotation.w,
            self.scale.x,
            self.scale.y,
            self.scale.z,
        ]
    }

    /// 局部矩阵 `T * R * S`。
    pub fn matrix(self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    /// 线性插值（旋转走 slerp）。`t = 0` 返回 `self`。
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            translation: self.translation.lerp(other.translation, t),
            rotation: self.rotation.slerp(other.rotation, t),
            scale: self.scale.lerp(other.scale, t),
        }
    }

    /// 从矩阵分解（缩放取列长，含负缩放时按最小分量翻到 X 轴）。
    pub fn from_matrix(matrix: Mat4) -> Self {
        let (scale, rotation, translation) = matrix.to_scale_rotation_translation();
        Self {
            translation,
            rotation: normalize_quat(rotation),
            scale,
        }
    }
}

impl From<prism_asset::formats_skinned::JointTransform> for JointTransform {
    fn from(value: prism_asset::formats_skinned::JointTransform) -> Self {
        Self::from_array(value.to_array())
    }
}

fn normalize_quat(rotation: Quat) -> Quat {
    if rotation.length_squared() > 1e-12 {
        rotation.normalize()
    } else {
        Quat::IDENTITY
    }
}

/// 一条骨骼。
#[derive(Debug, Clone, PartialEq)]
pub struct Joint {
    pub name: String,
    /// 父骨骼下标（cooked 数据保证父一定在子前面）。
    pub parent: Option<usize>,
    /// reference（bind）姿态下的父空间变换。
    pub reference_local: JointTransform,
    /// 蒙皮矩阵：把顶点从 reference 姿态空间变换到骨骼空间。
    pub inverse_bind: Mat4,
}

/// 骨架层次。
#[derive(Debug, Clone, PartialEq)]
pub struct Skeleton {
    joints: Vec<Joint>,
}

impl Skeleton {
    /// 从 cooked `SkeletonInfo` 构建，顺手校验拓扑序（父下标必须小于自身）。
    pub fn from_info(info: &prism_asset::formats_skinned::SkeletonInfo) -> Option<Self> {
        if info.joints.is_empty() || !info.is_topologically_sorted() {
            return None;
        }
        let joints = info
            .joints
            .iter()
            .enumerate()
            .map(|(index, joint)| {
                let parent = joint.parent.map(usize::from).filter(|p| *p < index);
                Joint {
                    name: joint.name.clone(),
                    parent,
                    reference_local: JointTransform::from(joint.reference_local),
                    inverse_bind: Mat4::from_cols_array(&joint.inverse_bind),
                }
            })
            .collect();
        Some(Self { joints })
    }

    /// 直接用骨骼列表构造（测试与离线工具用）。
    pub fn new(joints: Vec<Joint>) -> Option<Self> {
        let mut valid = !joints.is_empty();
        for (index, joint) in joints.iter().enumerate() {
            if joint.parent.is_some_and(|parent| parent >= index) {
                valid = false;
            }
        }
        valid.then_some(Self { joints })
    }

    pub fn joints(&self) -> &[Joint] {
        &self.joints
    }

    pub fn joint_count(&self) -> usize {
        self.joints.len()
    }

    pub fn joint_index(&self, name: &str) -> Option<usize> {
        self.joints.iter().position(|joint| joint.name == name)
    }

    /// reference 姿态（每个骨骼的 `reference_local`）。
    pub fn rest_pose(&self) -> Pose {
        Pose::from_locals(
            self.joints
                .iter()
                .map(|joint| joint.reference_local)
                .collect(),
        )
    }

    /// 姿态下的骨骼世界矩阵（父空间乘积，父必在子前）。
    pub fn world_matrices(&self, pose: &Pose) -> Vec<Mat4> {
        let mut world = Vec::with_capacity(self.joints.len());
        for (index, joint) in self.joints.iter().enumerate() {
            let local = pose
                .local(index)
                .map(JointTransform::matrix)
                .unwrap_or(Mat4::IDENTITY);
            let matrix = match joint.parent {
                Some(parent) => world[parent] * local,
                None => local,
            };
            world.push(matrix);
        }
        world
    }

    /// `bone_palette = world(pose) * inverse_bind`（GPU 直接上传这一份）。
    pub fn bone_palette(&self, pose: &Pose) -> Vec<Mat4> {
        self.world_matrices(pose)
            .into_iter()
            .zip(&self.joints)
            .map(|(world, joint)| world * joint.inverse_bind)
            .collect()
    }
}

#[cfg(test)]
#[path = "skeleton_tests.rs"]
pub(crate) mod tests;
