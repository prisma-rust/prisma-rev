//! 视口骨骼叠加（#560 P0-4 的增量）。
//!
//! 蒙皮网格在 Scene 视图里只有一张变形后的网格，看不出骨架长什么样、哪根骨骼在动。
//! 这里把选中实体的 `BonePalette` 还原成世界空间的关节位置，画成线 + 点：
//!
//! - `palette[j] = world_j * inverse_bind_j`（见 `prism_anim::Skeleton::bone_palette`），
//!   所以 `world_j = palette[j] * inverse_bind_j⁻¹`；
//! - 连线画在父 → 子之间，父骨骼下标在 cooked 数据里一定小于自身，所以不用排序。
//!
//! 纯数学放在 [`bone_overlay`] 里，视口只负责投影和画。

/// 世界空间的骨架：关节位置 + 父子连线。
#[derive(Debug, Clone, PartialEq)]
pub struct SkeletonOverlay {
    /// 每个关节的世界位置，顺序与骨架的 `joints` 一致。
    pub joints: Vec<[f32; 3]>,
    /// `(父下标, 子下标)`，用于画线。
    pub segments: Vec<(usize, usize)>,
}

impl SkeletonOverlay {
    pub fn is_empty(&self) -> bool {
        self.joints.is_empty()
    }
}

/// 由调色板 + 父表 + inverse bind 矩阵还原世界空间骨架。
///
/// 三个输入长度不一致时按最短的来（`palette` 可能被 `BONE_CAPACITY` 截断），
/// 非有限的结果会被跳过 —— 画一个 NaN 关节只会让整帧的线飞出去。
pub fn bone_overlay(
    palette: &[[[f32; 4]; 4]],
    parents: &[Option<usize>],
    inverse_binds: &[[[f32; 4]; 4]],
) -> SkeletonOverlay {
    let count = palette.len().min(parents.len()).min(inverse_binds.len());
    let mut joints = Vec::with_capacity(count);
    for index in 0..count {
        // world = palette * bind，bind = inverse_bind⁻¹。
        let bind = glam::Mat4::from_cols_array_2d(&inverse_binds[index]).inverse();
        let world = glam::Mat4::from_cols_array_2d(&palette[index]) * bind;
        let position = world.w_axis.truncate();
        if !position.is_finite() {
            joints.push([f32::NAN; 3]);
            continue;
        }
        joints.push([position.x, position.y, position.z]);
    }

    let mut segments = Vec::with_capacity(count);
    for (index, parent) in parents.iter().enumerate().take(count) {
        let Some(parent) = *parent else {
            continue;
        };
        if parent >= count || parent >= index {
            continue;
        }
        if !joints[index].iter().all(|value| value.is_finite())
            || !joints[parent].iter().all(|value| value.is_finite())
        {
            continue;
        }
        segments.push((parent, index));
    }

    SkeletonOverlay { joints, segments }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_palette(count: usize) -> Vec<[[f32; 4]; 4]> {
        vec![glam::Mat4::IDENTITY.to_cols_array_2d(); count]
    }

    fn translated_bind(x: f32) -> [[f32; 4]; 4] {
        glam::Mat4::from_translation(glam::vec3(x, 0.0, 0.0))
            .inverse()
            .to_cols_array_2d()
    }

    #[test]
    fn a_rest_pose_palette_puts_every_joint_on_its_bind_position() {
        // bind 在 +1m 的骨骼，调色板是单位矩阵 → 关节世界位置 = (1, 0, 0)。
        let overlay = bone_overlay(
            &identity_palette(2),
            &[None, Some(0)],
            &[translated_bind(0.0), translated_bind(1.0)],
        );
        assert_eq!(overlay.joints.len(), 2);
        assert!((overlay.joints[0][0]).abs() < 1e-5);
        assert!(
            (overlay.joints[1][0] - 1.0).abs() < 1e-5,
            "{:?}",
            overlay.joints
        );
        assert_eq!(overlay.segments, vec![(0, 1)]);
    }

    #[test]
    fn a_translated_palette_moves_the_joint_with_the_animation() {
        let mut palette = identity_palette(2);
        palette[1] = glam::Mat4::from_translation(glam::vec3(0.0, 2.0, 0.0)).to_cols_array_2d();
        let overlay = bone_overlay(
            &palette,
            &[None, Some(0)],
            &[translated_bind(0.0), translated_bind(1.0)],
        );
        assert!(
            (overlay.joints[1][1] - 2.0).abs() < 1e-5,
            "{:?}",
            overlay.joints
        );
    }

    #[test]
    fn mismatched_inputs_and_broken_matrices_are_dropped_instead_of_panicking() {
        // 调色板比父表短：按最短的来。
        let overlay = bone_overlay(
            &identity_palette(1),
            &[None, Some(0)],
            &[translated_bind(0.0), translated_bind(1.0)],
        );
        assert_eq!(overlay.joints.len(), 1);
        assert!(overlay.segments.is_empty());

        // NaN 关节不参与连线。
        let mut palette = identity_palette(2);
        palette[1][3][1] = f32::NAN;
        let overlay = bone_overlay(
            &palette,
            &[None, Some(0)],
            &[translated_bind(0.0), translated_bind(1.0)],
        );
        assert!(overlay.segments.is_empty());
    }

    #[test]
    fn a_parent_that_points_at_itself_or_forward_is_ignored() {
        let overlay = bone_overlay(
            &identity_palette(3),
            &[None, Some(1), Some(3)],
            &[
                translated_bind(0.0),
                translated_bind(1.0),
                translated_bind(2.0),
            ],
        );
        assert!(overlay.segments.is_empty(), "自环 / 越界父级不该连线");
    }
}
