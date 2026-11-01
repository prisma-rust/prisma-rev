//! CPU 蒙皮参考实现。
//!
//! GPU 路径（P0-3）会做同样的事，只是矩阵与顶点都在显存里；这里的实现是
//! **golden 基准**：同一份 palette / joints / weights 喂进去，两者结果必须一致。

use glam::{Mat4, Vec3};

/// 一个顶点的混合矩阵：`sum(w[i] * palette[joints[i]])`（权重和归一化到 1）。
///
/// 权重和为 0 的顶点退化成单位矩阵（保持静止，而不是塌到原点）。
pub fn skin_matrix(palette: &[Mat4], joints: &[u16; 4], weights: &[f32; 4]) -> Mat4 {
    let mut matrix = Mat4::ZERO;
    let mut total = 0.0f32;
    for slot in 0..4 {
        let weight = weights[slot];
        if weight <= 0.0 {
            continue;
        }
        let Some(palette_matrix) = palette.get(usize::from(joints[slot])) else {
            continue;
        };
        matrix += *palette_matrix * weight;
        total += weight;
    }
    if total <= f32::EPSILON {
        return Mat4::IDENTITY;
    }
    if (total - 1.0).abs() > 1e-4 {
        matrix *= 1.0 / total;
    }
    matrix
}

/// 批量算每个顶点的混合矩阵（P0-3 可以把这份直接对比 GPU 的 palette 采样）。
pub fn skin_matrices(palette: &[Mat4], joints: &[[u16; 4]], weights: &[[f32; 4]]) -> Vec<Mat4> {
    let count = joints.len().min(weights.len());
    (0..count)
        .map(|index| skin_matrix(palette, &joints[index], &weights[index]))
        .collect()
}

/// 蒙皮位置：`w * matrix * vec4(position, 1)`。
///
/// `joints` / `weights` 比 `positions` 短时，多出来的顶点保持原样。
pub fn skin_positions(
    palette: &[Mat4],
    joints: &[[u16; 4]],
    weights: &[[f32; 4]],
    positions: &[[f32; 3]],
) -> Vec<[f32; 3]> {
    let count = joints.len().min(weights.len()).min(positions.len());
    let mut out = Vec::with_capacity(positions.len());
    for (index, position) in positions.iter().enumerate() {
        if index >= count {
            out.push(*position);
            continue;
        }
        let matrix = skin_matrix(palette, &joints[index], &weights[index]);
        let skinned = matrix.transform_point3(Vec3::from_array(*position));
        out.push(skinned.to_array());
    }
    out
}

/// 蒙皮法线：只用混合矩阵的 3×3（去掉平移），再归一化。
pub fn skin_normals(
    palette: &[Mat4],
    joints: &[[u16; 4]],
    weights: &[[f32; 4]],
    normals: &[[f32; 3]],
) -> Vec<[f32; 3]> {
    let count = joints.len().min(weights.len()).min(normals.len());
    let mut out = Vec::with_capacity(normals.len());
    for (index, normal) in normals.iter().enumerate() {
        if index >= count {
            out.push(*normal);
            continue;
        }
        let matrix = skin_matrix(palette, &joints[index], &weights[index]);
        let rotated = matrix.transform_vector3(Vec3::from_array(*normal));
        let length = rotated.length();
        out.push(if length > 1e-6 {
            (rotated / length).to_array()
        } else {
            *normal
        });
    }
    out
}

/// palette 的裸数组形式（上传 GPU 用）。
pub fn palette_to_arrays(palette: &[Mat4]) -> Vec<[[f32; 4]; 4]> {
    palette
        .iter()
        .map(|matrix| matrix.to_cols_array_2d())
        .collect()
}

#[cfg(test)]
#[path = "skinning_tests.rs"]
mod tests;
