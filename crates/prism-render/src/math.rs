//! Small always-available matrix helpers shared by the render passes.
//!
//! `mat_inverse` used to live in `pt_pass`, which is behind the `pathtrace`
//! feature. The post pass needs it for camera motion blur even when path
//! tracing is compiled out, so it lives here instead.

/// Column-major 4×4 matrix inverse (Cramer's rule, transposed cofactor).
///
/// Degenerate matrices fall back to identity so a broken camera matrix cannot
/// turn unprojection into NaNs or infinities.
pub(crate) fn mat_inverse(m: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let (a00, a01, a02, a03) = (m[0][0], m[0][1], m[0][2], m[0][3]);
    let (a10, a11, a12, a13) = (m[1][0], m[1][1], m[1][2], m[1][3]);
    let (a20, a21, a22, a23) = (m[2][0], m[2][1], m[2][2], m[2][3]);
    let (a30, a31, a32, a33) = (m[3][0], m[3][1], m[3][2], m[3][3]);

    let b00 = a00 * a11 - a01 * a10;
    let b01 = a00 * a12 - a02 * a10;
    let b02 = a00 * a13 - a03 * a10;
    let b03 = a01 * a12 - a02 * a11;
    let b04 = a01 * a13 - a03 * a11;
    let b05 = a02 * a13 - a03 * a12;
    let b06 = a20 * a31 - a21 * a30;
    let b07 = a20 * a32 - a22 * a30;
    let b08 = a20 * a33 - a23 * a30;
    let b09 = a21 * a32 - a22 * a31;
    let b10 = a21 * a33 - a23 * a31;
    let b11 = a22 * a33 - a23 * a32;

    let det = b00 * b11 - b01 * b10 + b02 * b09 + b03 * b08 - b04 * b07 + b05 * b06;
    if det.abs() < 1e-12 {
        return [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
    }
    let inv_det = 1.0 / det;

    [
        [
            (a11 * b11 - a12 * b10 + a13 * b09) * inv_det,
            (-a01 * b11 + a02 * b10 - a03 * b09) * inv_det,
            (a31 * b05 - a32 * b04 + a33 * b03) * inv_det,
            (-a21 * b05 + a22 * b04 - a23 * b03) * inv_det,
        ],
        [
            (-a10 * b11 + a12 * b08 - a13 * b07) * inv_det,
            (a00 * b11 - a02 * b08 + a03 * b07) * inv_det,
            (-a30 * b05 + a32 * b02 - a33 * b01) * inv_det,
            (a20 * b05 - a22 * b02 + a23 * b01) * inv_det,
        ],
        [
            (a10 * b10 - a11 * b08 + a13 * b06) * inv_det,
            (-a00 * b10 + a01 * b08 - a03 * b06) * inv_det,
            (a30 * b04 - a31 * b02 + a33 * b00) * inv_det,
            (-a20 * b04 + a21 * b02 - a23 * b00) * inv_det,
        ],
        [
            (-a10 * b09 + a11 * b07 - a12 * b06) * inv_det,
            (a00 * b09 - a01 * b07 + a02 * b06) * inv_det,
            (-a30 * b03 + a31 * b01 - a32 * b00) * inv_det,
            (a20 * b03 - a21 * b01 + a22 * b00) * inv_det,
        ],
    ]
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
