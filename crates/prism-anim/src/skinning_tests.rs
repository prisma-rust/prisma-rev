use super::*;
use glam::{Mat4, Quat, Vec3};

#[test]
fn a_single_full_weight_uses_that_bone() {
    let palette = vec![
        Mat4::IDENTITY,
        Mat4::from_translation(Vec3::new(0.0, 10.0, 0.0)),
    ];
    let joints = [[1u16, 0, 0, 0]];
    let weights = [[1.0f32, 0.0, 0.0, 0.0]];
    let positions = [[0.0f32, 0.0, 0.0]];
    let skinned = skin_positions(&palette, &joints, &weights, &positions);
    assert!((skinned[0][1] - 10.0).abs() < 1e-5, "got {skinned:?}");
}

#[test]
fn two_bones_blend_halfway() {
    let palette = vec![
        Mat4::IDENTITY,
        Mat4::from_translation(Vec3::new(0.0, 10.0, 0.0)),
    ];
    let joints = [[0u16, 1, 0, 0]];
    let weights = [[0.5f32, 0.5, 0.0, 0.0]];
    let skinned = skin_positions(&palette, &joints, &weights, &[[0.0f32, 0.0, 0.0]]);
    assert!((skinned[0][1] - 5.0).abs() < 1e-5, "got {skinned:?}");
}

#[test]
fn weights_that_do_not_sum_to_one_are_normalized() {
    let palette = vec![
        Mat4::IDENTITY,
        Mat4::from_translation(Vec3::new(0.0, 10.0, 0.0)),
    ];
    // 0.25 / 0.25（和为 0.5）应该等价于 0.5 / 0.5。
    let skinned = skin_positions(
        &palette,
        &[[0u16, 1, 0, 0]],
        &[[0.25f32, 0.25, 0.0, 0.0]],
        &[[0.0f32, 0.0, 0.0]],
    );
    assert!((skinned[0][1] - 5.0).abs() < 1e-5, "got {skinned:?}");
}

#[test]
fn a_vertex_with_no_weights_stays_put() {
    let palette = vec![Mat4::from_translation(Vec3::new(9.0, 9.0, 9.0))];
    let matrix = skin_matrix(&palette, &[0, 0, 0, 0], &[0.0, 0.0, 0.0, 0.0]);
    assert_eq!(matrix, Mat4::IDENTITY);
}

#[test]
fn out_of_range_bones_are_skipped_not_panicking() {
    let palette = vec![Mat4::IDENTITY];
    let matrix = skin_matrix(&palette, &[99, 0, 0, 0], &[1.0, 0.0, 0.0, 0.0]);
    assert_eq!(matrix, Mat4::IDENTITY, "唯一可用槽也越界 → 保持静止");
}

#[test]
fn normals_are_rotated_and_normalized() {
    let rotate = Mat4::from_quat(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2));
    let palette = vec![rotate];
    let normals = skin_normals(
        &palette,
        &[[0u16, 0, 0, 0]],
        &[[1.0f32, 0.0, 0.0, 0.0]],
        &[[1.0f32, 0.0, 0.0]],
    );
    // +X 绕 Z 转 90° → +Y。
    assert!(normals[0][1].abs() > 0.99, "got {normals:?}");
    let length = (normals[0][0].powi(2) + normals[0][1].powi(2) + normals[0][2].powi(2)).sqrt();
    assert!((length - 1.0).abs() < 1e-5);
}

#[test]
fn a_rotated_bone_matches_a_hand_computed_skinned_vertex() {
    // 骨骼绕 Z 转 90°，顶点在 (1,0,0) → 应落到 (0,1,0)。
    let rotate = Mat4::from_quat(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2));
    let palette = vec![rotate];
    let skinned = skin_positions(
        &palette,
        &[[0u16, 0, 0, 0], [0, 0, 0, 0]],
        &[[1.0f32, 0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]],
        &[[1.0f32, 0.0, 0.0], [0.0, 1.0, 0.0]],
    );
    assert!(skinned[0][1].abs() > 0.99, "got {skinned:?}");
    assert!(skinned[0][0].abs() < 1e-5);
    assert!(skinned[1][0] < -0.99, "(0,1,0) → (-1,0,0), got {skinned:?}");
}

#[test]
fn missing_attributes_leave_the_tail_untouched() {
    let palette = vec![Mat4::from_translation(Vec3::new(0.0, 1.0, 0.0))];
    let positions = [[0.0f32, 0.0, 0.0], [0.0, 0.0, 0.0]];
    let skinned = skin_positions(
        &palette,
        &[[0u16, 0, 0, 0]],
        &[[1.0f32, 0.0, 0.0, 0.0]],
        &positions,
    );
    assert!((skinned[0][1] - 1.0).abs() < 1e-5);
    assert_eq!(skinned[1], positions[1], "没有蒙皮数据的顶点原样保留");
}

#[test]
fn the_palette_arrays_are_column_major() {
    let palette = vec![Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0))];
    let arrays = palette_to_arrays(&palette);
    assert_eq!(arrays[0][3], [1.0, 2.0, 3.0, 1.0]);
}
