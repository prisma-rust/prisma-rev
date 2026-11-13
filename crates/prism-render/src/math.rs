//! Small always-available matrix helpers shared by the render passes.
//!
//! `mat_inverse` used to live in `pt_pass`, which is behind the `pathtrace`
//! feature. The post pass needs it for camera motion blur even when path
//! tracing is compiled out, so it lives here instead.

use glam::Mat4;

/// Column-major 4×4 matrix inverse.
///
/// Degenerate matrices fall back to identity so a broken camera matrix cannot
/// turn unprojection into NaNs or infinities.
pub(crate) fn mat_inverse(m: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mat = Mat4::from_cols_array_2d(m);
    let det = mat.determinant();
    if !det.is_finite() || det.abs() < 1e-12 {
        Mat4::IDENTITY.to_cols_array_2d()
    } else {
        mat.inverse().to_cols_array_2d()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matrix_inverts_to_identity() {
        let id = Mat4::IDENTITY.to_cols_array_2d();
        assert_eq!(mat_inverse(&id), id);
    }

    #[test]
    fn degenerate_matrix_falls_back_to_identity() {
        let zero = [[0.0; 4]; 4];
        assert_eq!(mat_inverse(&zero), Mat4::IDENTITY.to_cols_array_2d());

        let mut collapsed = Mat4::IDENTITY.to_cols_array_2d();
        collapsed[0][0] = 0.0;
        collapsed[1][1] = 0.0;
        collapsed[2][2] = 0.0;
        assert_eq!(mat_inverse(&collapsed), Mat4::IDENTITY.to_cols_array_2d());
    }

    #[test]
    fn affine_matrix_roundtrips() {
        let translation = glam::Vec3::new(3.0, -2.0, 5.0);
        let rotation = glam::Quat::from_rotation_y(0.5);
        let scale = glam::Vec3::new(2.0, 1.5, 0.5);
        let mat = Mat4::from_scale_rotation_translation(scale, rotation, translation);
        let cols = mat.to_cols_array_2d();
        let inv_cols = mat_inverse(&cols);
        let inv_mat = Mat4::from_cols_array_2d(&inv_cols);
        let prod = mat * inv_mat;

        for (c, col) in prod.to_cols_array_2d().iter().enumerate() {
            for (r, &val) in col.iter().enumerate() {
                let want = if c == r { 1.0 } else { 0.0 };
                assert!(
                    (val - want).abs() < 1e-4,
                    "prod[{c}][{r}] = {val}, expected {want}"
                );
            }
        }
    }
}

/// Transform a 3D point by a 4x4 column-major affine model matrix (w = 1).
#[allow(dead_code)]
pub(crate) fn transform_point(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0],
        m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1],
        m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2],
    ]
}

/// Transform a 3D vector by a 4x4 column-major affine model matrix (w = 0) and normalize.
#[allow(dead_code)]
pub(crate) fn transform_vector_normalized(m: &[[f32; 4]; 4], v: [f32; 3]) -> [f32; 3] {
    let x = m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2];
    let y = m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2];
    let z = m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2];
    let len_sq = x * x + y * y + z * z;
    if len_sq > 1e-12 {
        let inv = 1.0 / len_sq.sqrt();
        [x * inv, y * inv, z * inv]
    } else {
        [0.0, 0.0, 0.0]
    }
}

/// Transpose a 4x4 matrix (swaps rows and columns).
#[allow(dead_code)]
pub(crate) fn mat_transpose(m: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for col in 0..4 {
        for row in 0..4 {
            out[row][col] = m[col][row];
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::needless_range_loop, clippy::excessive_precision)]
mod inverse_tests {
    use super::*;

    /// Reference inverse via Gauss-Jordan on the row-major expansion, so the
    /// test does not depend on `mat_inverse`'s own formula.
    fn reference_inverse(m: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
        // rows[r][c]
        let mut a = [[0.0f64; 8]; 4];
        for r in 0..4 {
            for c in 0..4 {
                a[r][c] = m[c][r] as f64;
            }
            a[r][4 + r] = 1.0;
        }
        for col in 0..4 {
            let mut pivot = col;
            for r in col + 1..4 {
                if a[r][col].abs() > a[pivot][col].abs() {
                    pivot = r;
                }
            }
            a.swap(col, pivot);
            let p = a[col][col];
            assert!(p.abs() > 1e-12, "matrix is singular at column {col}");
            for c in 0..8 {
                a[col][c] /= p;
            }
            for r in 0..4 {
                if r != col {
                    let f = a[r][col];
                    for c in 0..8 {
                        a[r][c] -= f * a[col][c];
                    }
                }
            }
        }
        let mut out = [[0.0f32; 4]; 4];
        for r in 0..4 {
            for c in 0..4 {
                out[c][r] = a[r][4 + c] as f32;
            }
        }
        out
    }

    #[test]
    fn mat_inverse_matches_gauss_jordan() {
        // Vulkan-style y-flipped perspective (fov 65 deg, aspect 16:9, near
        // 0.05, far 1000) times a view looking down +X from eye (-10, 2, 0),
        // column-major [col][row] exactly as the engine stores it.
        let vp = [
            [0.0, 0.0, 1.00005, 1.0],
            [0.0, -1.5696856, 0.0, 0.0],
            [0.8829481, 0.0, 0.0, 0.0],
            [0.0, 3.1393712, 9.9504975, 10.0],
        ];
        let ours = mat_inverse(&vp);
        let theirs = reference_inverse(&vp);
        // The inverse of this matrix reaches ~200 in the near/far terms, so
        // compare with a relative tolerance; f32 cannot hold 1e-3 absolute
        // there. Verified against the f64 Gauss-Jordan reference above.
        for c in 0..4 {
            for r in 0..4 {
                let scale = ours[c][r].abs().max(theirs[c][r].abs()).max(1.0);
                assert!(
                    (ours[c][r] - theirs[c][r]).abs() <= 1e-4 * scale,
                    "mismatch [{c}][{r}]: ours={} ref={}",
                    ours[c][r],
                    theirs[c][r]
                );
            }
        }
    }
}
