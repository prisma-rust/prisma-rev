//! 旧版轨道相机
//!
//! 保留供参考/未来重新集成，但**不**接入 ECS 或渲染器。
//! 当前激活的相机路径是 [`crate::scene::systems::camera`] 中的数据组件流程
//!（`Camera` + `FlyCameraController` + `LocalToWorld`）。
//!
//! `OrbitCamera` 是一个自包含的结构体，拥有自己的视图/投影数学
//!（围绕目标点的球面坐标）。它作为独立类型保留，
//! 以便将来添加轨道控制器组件时可以复用其数学计算。

/// 围绕目标点的球面坐标轨道相机。
pub struct OrbitCamera {
    pub target: [f32; 3],
    pub distance: f32,
    pub theta: f32, // azimuth (rad), 0 = +Z direction
    pub phi: f32,   // elevation (rad), π/2 = horizontal
    pub fov_y: f32,
    pub znear: f32,
    pub zfar: f32,
    /// Current aspect ratio (width / height). Set at construction and updated
    /// on resize / orientation change so [`OrbitCamera::view_proj`] needs no
    /// per-call aspect argument.
    pub aspect: f32,
    /// Exposure multiplier applied to the final HDR color before tonemapping.
    /// Default 1.0 = no scaling; range [0, 5] via inspector slider.
    /// Controls overall image brightness independently of light intensity.
    pub exposure: f32,
    /// When `false` the camera is skipped during scene collection (the
    /// renderer falls back to the next available camera).
    pub enabled: bool,
}

impl OrbitCamera {
    pub fn new(aspect: f32) -> Self {
        Self {
            target: [0.0; 3],
            distance: 5.0,
            theta: 0.0,
            phi: std::f32::consts::FRAC_PI_2, // horizontal
            fov_y: std::f32::consts::FRAC_PI_4,
            znear: 0.01,
            zfar: 100.0,
            aspect,
            exposure: 1.0,
            enabled: true,
        }
    }

    /// Update the aspect ratio (e.g. on window resize or orientation change)
    /// without disturbing the current orbit state.
    pub fn set_aspect(&mut self, aspect: f32) {
        self.aspect = aspect;
    }

    /// Eye position from spherical coords.
    pub fn eye(&self) -> [f32; 3] {
        let (s_th, c_th) = self.theta.sin_cos();
        let (s_ph, c_ph) = self.phi.sin_cos();
        [
            self.target[0] + self.distance * s_th * s_ph,
            self.target[1] + self.distance * c_ph,
            self.target[2] + self.distance * c_th * s_ph,
        ]
    }

    /// Column-major view-projection matrix, using the stored [`OrbitCamera::aspect`].
    pub fn view_proj(&self) -> [[f32; 4]; 4] {
        let eye = self.eye();
        let proj = self.perspective();
        let view = self.look_at(eye);
        // view_proj = proj * view (column-major)
        let mut vp = [[0.0f32; 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                for k in 0..4 {
                    vp[i][j] += proj[k][j] * view[i][k];
                }
            }
        }
        vp
    }

    /// Column-major projection matrix (Vulkan y-flip, depth range [0,1]).
    /// Exposed so callers that need projection separately (e.g. the GTAO pass
    /// reconstructs view-space positions from depth using `inv_proj`) can fetch
    /// it without recomputing it from `view_proj * inverse(view)`.
    pub fn projection(&self) -> [[f32; 4]; 4] {
        self.perspective()
    }

    /// Column-major world -> view matrix (used for view-space debug normals).
    pub fn view(&self) -> [[f32; 4]; 4] {
        self.look_at(self.eye())
    }

    fn perspective(&self) -> [[f32; 4]; 4] {
        let inv_tan = 1.0 / (self.fov_y * 0.5).tan();
        let mut p = [[0.0f32; 4]; 4];
        p[0][0] = inv_tan / self.aspect;
        p[1][1] = -inv_tan;
        p[2][2] = self.zfar / (self.znear - self.zfar);
        // Column-major: p[col][row]
        // p[2][3] = column 2, row 3 = contribution of z_view to gl_Position.w
        // Must be -1 so that w_clip = -z_view (perspective divide).
        p[2][3] = -1.0;
        // p[3][2] = column 3, row 2 = contribution of w_view(=1) to gl_Position.z
        p[3][2] = self.znear * self.zfar / (self.znear - self.zfar);
        p
    }

    fn look_at(&self, eye: [f32; 3]) -> [[f32; 4]; 4] {
        let fwd = [
            self.target[0] - eye[0],
            self.target[1] - eye[1],
            self.target[2] - eye[2],
        ];
        let fwd_len = (fwd[0] * fwd[0] + fwd[1] * fwd[1] + fwd[2] * fwd[2]).sqrt();
        let fwd = [fwd[0] / fwd_len, fwd[1] / fwd_len, fwd[2] / fwd_len];
        let up = [0.0, 1.0, 0.0];
        // Right-handed basis: right = forward × up (NOT up × forward, which
        // would negate the right vector and make the view matrix a reflection,
        // mirroring the scene horizontally).
        let right = [
            fwd[1] * up[2] - fwd[2] * up[1],
            fwd[2] * up[0] - fwd[0] * up[2],
            fwd[0] * up[1] - fwd[1] * up[0],
        ];
        let rl = (right[0] * right[0] + right[1] * right[1] + right[2] * right[2]).sqrt();
        let right = [right[0] / rl, right[1] / rl, right[2] / rl];
        // Re-orthogonalize up against the (now correct) right: up = right × forward.
        let up = [
            right[1] * fwd[2] - right[2] * fwd[1],
            right[2] * fwd[0] - right[0] * fwd[2],
            right[0] * fwd[1] - right[1] * fwd[0],
        ];
        // Column-major view matrix
        [
            [right[0], up[0], -fwd[0], 0.0],
            [right[1], up[1], -fwd[1], 0.0],
            [right[2], up[2], -fwd[2], 0.0],
            [
                -(right[0] * eye[0] + right[1] * eye[1] + right[2] * eye[2]),
                -(up[0] * eye[0] + up[1] * eye[1] + up[2] * eye[2]),
                fwd[0] * eye[0] + fwd[1] * eye[1] + fwd[2] * eye[2],
                1.0,
            ],
        ]
    }
}

#[cfg(test)]
#[path = "camera_tests.rs"]
mod tests;
