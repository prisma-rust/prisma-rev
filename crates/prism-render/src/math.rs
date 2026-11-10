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
