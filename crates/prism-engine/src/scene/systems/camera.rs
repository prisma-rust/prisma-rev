//! 相机系统
//!
//! 将旧的 `crate::camera::Camera` 枚举（混入了编辑器字段和运行时状态）
//! 拆分为纯数据组件（`Camera` + [`FlyCameraController`]）和
//! 每帧从中派生运行时视图/投影矩阵的自由函数。
//!
//! - [`camera_controller_system`] 将输入应用于 `FlyCameraController` +
//!   同级 `LocalTransform`（写入偏航/俯仰/平移，并把朝向同步进
//!   `LocalTransform.rotation`，让子节点与 `LocalToWorld` 跟随视角）。
//! - [`compute_camera_output`] 读取 `Camera` + `FlyCameraController` +
//! `LocalToWorld` 并生成渲染器所需的矩阵。
//!
//! 坐标系约定：右手系，+Y 向上，相机看向 −Z 方向。
//! Vulkan y-flip 投影，深度范围 [0,1]。
//! 参见 `README.md` §Coordinate Conventions 和 `DESIGN.md`。

use glam::{Mat4, Vec3};

use prism_ecs::World;

use prism_scene::CameraClearFlags;

use crate::input::InputManager;
use crate::scene::components::{Camera, FlyCameraController, LocalToWorld, LocalTransform, Parent};

/// Return the first [`Camera`] component found in the world.
///
/// If there are multiple cameras (e.g. editor + game view), the ordering is
/// determined by the ECS storage (typically insertion order). Returns `None`
/// when no camera is present.
pub fn collect_camera(world: &World) -> Option<Camera> {
    world.query::<Camera>().next().map(|(_, c)| c.clone())
}

/// 所有**启用**相机，按 `Camera.depth` 升序（数值大的后绘制）= `Camera.allCameras`。
pub fn all_cameras(world: &World) -> Vec<prism_ecs::Entity> {
    let mut cameras: Vec<(f32, prism_ecs::Entity)> = world
        .query::<Camera>()
        .filter(|(_, cam)| cam.enabled)
        .map(|(entity, cam)| (cam.depth, entity))
        .collect();
    // `total_cmp` 让 NaN 也有全序，避免 `partial_cmp` 的 unwrap 风险。
    cameras.sort_by(|a, b| a.0.total_cmp(&b.0));
    cameras.into_iter().map(|(_, entity)| entity).collect()
}

/// 主相机 = `depth` 最大的启用相机 = `Camera.main`。
pub fn main_camera(world: &World) -> Option<prism_ecs::Entity> {
    all_cameras(world).pop()
}

/// 主相机之外的启用相机（诊断用）。
///
/// #461 之后所有启用相机都会参与合成：forward pass 按 `Camera.depth` 升序各画
/// 一遍，`clear_flags` 决定每层清颜色 / 深度。这个函数保留给需要「非主相机」
/// 列表的工具（统计、调试 UI），不再表示「被忽略」。
pub fn ignored_cameras(world: &World) -> Vec<prism_ecs::Entity> {
    let mut cameras = all_cameras(world);
    cameras.pop(); // 主相机（`depth` 最大）会画出来
    cameras
}

/// 真正会被渲染的相机 = `depth` 最大的启用相机里，第一台**有 `LocalToWorld`** 的。
///
/// 取景、清屏、宽高比写回、飞行输入都走这一处：只要大家用同一个选择函数，
/// 就不会出现「写完 A 的 aspect 却渲染 B」「输入驱动 A 但画面是 B」这类分叉。
pub fn active_camera(world: &World) -> Option<prism_ecs::Entity> {
    all_cameras(world)
        .into_iter()
        .rev()
        .find(|entity| world.get::<LocalToWorld>(*entity).is_some())
}

/// 无相机时使用的清屏色（中性灰：与纯黑/纯白区分，便于看出"什么都没画"）。
pub const DEFAULT_CLEAR_COLOR: [f32; 4] = [0.5, 0.5, 0.5, 1.0];

/// 指定相机的清屏色 = `Camera.backgroundColor`。
///
/// **取景与清屏必须同源**：调用方传入 [`CameraOutput::entity`]（真正产生 view 的
/// 那台相机），这里不再自己挑相机，所以两处不会因为选择规则不同而分叉。
/// 实体不存在或没有 `Camera` 时用 [`DEFAULT_CLEAR_COLOR`]。
pub fn clear_color(world: &World, camera: prism_ecs::Entity) -> [f32; 4] {
    world
        .get::<Camera>(camera)
        .map(|cam| cam.background_color)
        .unwrap_or(DEFAULT_CLEAR_COLOR)
}

/// 指定相机的清屏方式 = `Camera.clearFlags`。
///
/// 与 [`clear_color`] 同源：调用方传 [`CameraOutput::entity`]，缺省回退到
/// [`CameraClearFlags::Skybox`]（新建相机的默认值，也是引擎既有行为）。
pub fn clear_flags(world: &World, camera: prism_ecs::Entity) -> CameraClearFlags {
    world
        .get::<Camera>(camera)
        .map(|cam| cam.clear_flags)
        .unwrap_or_default()
}

/// Runtime camera output produced each frame by [`compute_camera_output`].
pub struct CameraOutput {
    pub view_proj: Mat4,
    pub view: Mat4,
    pub projection: Mat4,
    pub eye: Vec3,
    pub exposure: f32,
    /// The entity the camera was sourced from (for downstream look-ups).
    pub entity: prism_ecs::Entity,
}

/// Return a fallback `CameraOutput` when no usable Camera entity exists in the
/// ECS world. This avoids a fatal error — the engine renders a gray background
/// and the runtime HUD shows a "No 相机 hint.
///
/// The fallback places the viewer at `(0, 0, 5)` looking toward the origin with
/// a 75° FOV, 16:9 aspect, and exposure 1.0.
pub fn fallback_camera_output(surface_rotation: &Mat4, aspect: f32) -> CameraOutput {
    let eye = Vec3::new(0.0, 0.0, 5.0);
    // Simple look-at: eye at (0,0,5), target at origin, +Y up.
    // Right-handed, camera looks down -Z per engine convention.
    let view = Mat4::from_cols_array_2d(&[
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, -5.0, 1.0],
    ]);
    // Standard perspective: 75° FOV, 16:9-ish aspect, near=0.01, far=500.
    let fov_y = 75.0_f32.to_radians();
    let inv_tan = 1.0 / (fov_y * 0.5).tan();
    let near = 0.01;
    let far = 500.0;
    let mut proj = Mat4::ZERO;
    proj.col_mut(0).x = inv_tan / aspect;
    proj.col_mut(1).y = -inv_tan;
    proj.col_mut(2).z = far / (near - far);
    proj.col_mut(2).w = -1.0;
    proj.col_mut(3).z = near * far / (near - far);

    let vp = *surface_rotation * proj * view;

    CameraOutput {
        view_proj: vp,
        view,
        projection: proj,
        eye,
        exposure: 1.0,
        entity: prism_ecs::Entity::from_raw(u32::MAX, 0),
    }
}

/// 查找 [`active_camera`]（`depth` 最大的可渲染启用相机），derive its view/projection.
///
/// 取景必须与清屏同源：调用方拿 [`CameraOutput::entity`] 去
/// [`clear_color`]，两边就不会各自挑一次相机。
///
/// 没有 `FlyCameraController` 时回退到实体自身的朝向（`LocalToWorld` 的逆），
/// 也就是 里"相机由 Transform 驱动"的形状 —— 这样场景里只放一个
/// `Camera` + 变换就能出画面，不必强迫作者再加飞行控制器。
///
/// `surface_rotation` is the device-orientation matrix applied on top of the
/// view-projection (mirrors the old `mat_mul(&surface_rotation, &vp)` step in
/// `render_system`). Returns `None` if no usable camera exists.
pub fn compute_camera_output(world: &World, surface_rotation: &Mat4) -> Option<CameraOutput> {
    compute_camera_output_for(world, active_camera(world)?, surface_rotation)
}

/// [`compute_camera_output`] for one explicit camera entity (#461).
///
/// Returns `None` when the entity lost its `Camera` / `LocalToWorld` since the
/// caller enumerated it.
pub fn compute_camera_output_for(
    world: &World,
    entity: prism_ecs::Entity,
    surface_rotation: &Mat4,
) -> Option<CameraOutput> {
    let cam = world.get::<Camera>(entity)?;
    let world_tf = world.get::<LocalToWorld>(entity)?;

    // Eye position = world-space translation (column 3, rows 0..3 of the
    // column-major matrix). For a root camera this equals LocalTransform
    // translation; for a nested camera the hierarchy system already baked the
    // parent transform in.
    let col3 = world_tf.0.col(3);
    let eye = Vec3::new(col3.x, col3.y, col3.z);

    let view = match world.get::<FlyCameraController>(entity) {
        Some(ctrl) => fly_view(eye, ctrl.yaw, ctrl.pitch),
        // 单位变换 → 相机看向 −Z（项目约定）；任何旋转都由 LocalToWorld 携带。
        None => world_tf.0.inverse(),
    };
    let proj = perspective(cam);
    let vp = *surface_rotation * proj * view;

    Some(CameraOutput {
        view_proj: vp,
        view,
        projection: proj,
        eye,
        exposure: cam.exposure,
        entity,
    })
}

/// Apply free-fly input for one frame to the first `FlyCameraController` +
/// sibling `LocalTransform` found on a `Camera` entity.
///
/// `look_active` is the pointer-lock state and it owns **all** camera input:
/// while it is false the camera neither rotates from the mouse delta (a held
/// right mouse button still does) nor moves from WASD/QE. The project UI owns
/// the keyboard in that state, so a shared key can never drive both at once.
///
/// Returns `true` if a controller was updated (so callers can skip the legacy
/// demo-spin animation for that entity).
pub fn camera_controller_system(
    world: &mut World,
    input: &InputManager,
    dt: f32,
    look_active: bool,
) -> Option<prism_ecs::Entity> {
    use crate::input::{KeyCode, MouseButton};

    // 输入必须驱动**正在渲染**的那台相机，否则多相机场景里 WASD/鼠标会移动另一台。
    // 先拿到实体 id，让 &self 借用在这里结束，后面才能取 &mut 分量。
    let cam_entity = active_camera(world)?;

    let ctrl = world.get_mut::<FlyCameraController>(cam_entity)?;
    let move_speed;
    let look_sensitivity;
    {
        // Scope the &mut borrow of ctrl so we can later borrow LocalTransform.
        // Look: either right-drag (non-locked) or direct mouse-follow (locked).
        let effective_look = look_active || input.mouse_held(MouseButton::Right);
        if effective_look {
            let d = input.mouse_delta();
            ctrl.yaw -= d[0] as f32 * ctrl.look_sensitivity;
            ctrl.pitch -= d[1] as f32 * ctrl.look_sensitivity;
            // Clamp just shy of straight up/down. The yaw-based `right()` keeps
            // the basis well-defined at any pitch, and ~89° reads as "looking
            // straight up" while avoiding pole-crossing roll.
            let lim = std::f32::consts::FRAC_PI_2 - 0.02;
            ctrl.pitch = ctrl.pitch.clamp(-lim, lim);
        }

        // Mouse wheel adjusts base move speed.
        let scroll = input.scroll_delta() as f32;
        if scroll.abs() > 0.0 {
            ctrl.move_speed *= 1.0 - scroll * 0.1;
            ctrl.move_speed = ctrl.move_speed.clamp(0.5, 200.0);
        }
        move_speed = ctrl.move_speed;
        look_sensitivity = ctrl.look_sensitivity;
    }

    // Translation: WASD/QE/Space/Ctrl relative to the yaw/pitch basis. Position
    // lives on the sibling LocalTransform (roots) - nested cameras use the
    // LocalToWorld derived by the hierarchy 系统 and shouldn't be moved by
    // input directly, so we only write to LocalTransform.
    let boost = if input.key_held(KeyCode::ShiftLeft) || input.key_held(KeyCode::ShiftRight) {
        4.0
    } else {
        1.0
    };
    let speed = move_speed * boost * dt;

    let (yaw, pitch) = {
        let c = world.get::<FlyCameraController>(cam_entity)?;
        (c.yaw, c.pitch)
    };
    let f = forward(yaw, pitch);
    let r = right(yaw);
    let up = Vec3::Y;
    let mut movev = Vec3::ZERO;
    // 走位键只在指针锁定时归相机：解锁态键盘属于项目的 UI（Sponza 的调参菜单
    // 也用 WASD 导航），同一次按键绝不能同时翻菜单和移动相机。
    if look_active && input.key_held(KeyCode::KeyW) {
        movev += f;
    }
    if look_active && input.key_held(KeyCode::KeyS) {
        movev -= f;
    }
    if look_active && input.key_held(KeyCode::KeyD) {
        movev += r;
    }
    if look_active && input.key_held(KeyCode::KeyA) {
        movev -= r;
    }
    if look_active && (input.key_held(KeyCode::Space) || input.key_held(KeyCode::KeyE)) {
        movev += up;
    }
    if look_active && (input.key_held(KeyCode::ControlLeft) || input.key_held(KeyCode::KeyQ)) {
        movev -= up;
    }

    // 朝向也要写回：`hierarchy_system` 只从 `LocalTransform.rotation` 推导
    // 子节点（billboard / 武器 / 聚光灯）的 `LocalToWorld`，只写 yaw/pitch
    // 会让它们停在旧朝向上。
    //
    // `yaw`/`pitch`（以及 WASD 基向量）是**世界**空间的，而 `LocalTransform`
    // 是相对父级的：挂在旋转父级下时先减去父级的世界旋转，否则子节点朝向
    // 会被父级二次旋转、WASD 也会把世界轴直接加到局部坐标上。
    let world_rot = fly_rotation(yaw, pitch);
    let parent_rot = parent_world_rotation(world, cam_entity);
    let local_rot = match parent_rot {
        Some(p) => p.inverse() * world_rot,
        None => world_rot,
    };

    let ml = movev.length();
    let delta_world = if ml > 1e-6 {
        movev * (speed / ml)
    } else {
        Vec3::ZERO
    };
    let delta_local = match parent_rot {
        Some(p) => p.inverse() * delta_world,
        None => delta_world,
    };

    if let Some(t) = world.get_mut::<LocalTransform>(cam_entity) {
        t.rotation = local_rot;
        t.position += delta_local;
    }

    // Touch `look_sensitivity` so the compiler doesn't warn about it being
    // unused when the look branch above didn't run - it is read indirectly via
    // ctrl.look_sensitivity. (No-op; kept for clarity.)
    let _ = look_sensitivity;

    Some(cam_entity)
}

// --- fly-camera math (ported from the deleted `FlyCamera`) ---------------

/// Unit forward vector from yaw/pitch.
///
/// 向前 = `[-sin(yaw)·cos(pitch), sin(pitch), -cos(yaw)·cos(pitch)]`。
/// `yaw = 0` 看向 **−Z**（与 `.scene.json` 里 identity 变换、以及
/// `README.md` 的坐标约定一致）；`yaw` 增大向左转。
fn forward(yaw: f32, pitch: f32) -> Vec3 {
    let (s_y, c_y) = yaw.sin_cos();
    let (s_p, c_p) = pitch.sin_cos();
    Vec3::new(-s_y * c_p, s_p, -c_y * c_p)
}

/// Unit right vector, derived from yaw only (not forward × worldUp).
///
/// 右 = `[cos(yaw), 0, -sin(yaw)]`；`yaw = 0` 时是 **+X**。用 yaw 单独构造，
/// 所以任意 音高 下都有定义（包括 音高 = ±π/2，此时 向前 × worldUp 会退化）。
fn right(yaw: f32) -> Vec3 {
    let (s_y, c_y) = yaw.sin_cos();
    Vec3::new(c_y, 0.0, -s_y)
}

/// `yaw`/`pitch` 对应的相机世界旋转（相机看向自身 −Z）。
///
/// 与 [`fly_view`] 用同一组基向量（列 = 右 / 上 / −前），所以写回
/// `LocalTransform.rotation` 后，相机子节点和 `LocalToWorld` 得到的朝向
/// 与画面里看到的完全一致。`yaw = 0, pitch = 0` → 单位四元数。
fn fly_rotation(yaw: f32, pitch: f32) -> glam::Quat {
    let f = forward(yaw, pitch).normalize();
    let r = right(yaw);
    let u = r.cross(f);
    glam::Quat::from_mat3(&glam::Mat3::from_cols(r, u, -f)).normalize()
}

/// 父级的世界旋转（没有父级 / 父级还没算 `LocalToWorld` 时 `None`）。
///
/// `hierarchy_system` 把父级的 scale 也乘进了 `LocalToWorld`，这里只要旋转，
/// 所以取 T×R×S 分解里的正交部分（并归一化，避免非均匀 scale 把四元数拉歪）。
fn parent_world_rotation(world: &World, entity: prism_ecs::Entity) -> Option<glam::Quat> {
    let parent = world.get::<Parent>(entity)?.0;
    let parent_world = world.get::<LocalToWorld>(parent)?.0;
    let (_, rotation, _) = parent_world.to_scale_rotation_translation();
    Some(rotation.normalize())
}

/// Column-major view matrix for a free-fly camera at `eye` with `yaw`/`pitch`.
fn fly_view(eye: Vec3, yaw: f32, pitch: f32) -> Mat4 {
    let f = forward(yaw, pitch).normalize();
    let r = right(yaw);
    let u = r.cross(f);
    Mat4::from_cols(
        glam::vec4(r.x, u.x, -f.x, 0.0),
        glam::vec4(r.y, u.y, -f.y, 0.0),
        glam::vec4(r.z, u.z, -f.z, 0.0),
        glam::vec4(-r.dot(eye), -u.dot(eye), f.dot(eye), 1.0),
    )
}

/// Column-major projection matrix (Vulkan y-flip, depth range [0,1]).
///
/// 透视 / 正交的选择在 [`Camera::projection_matrix`] 里（`prism-scene` 的
/// `components_tests.rs` 覆盖了 y-flip 与 `near → 0` / `far → 1` 的映射）。
fn perspective(cam: &Camera) -> Mat4 {
    cam.projection_matrix(cam.aspect)
}

#[cfg(test)]
#[path = "camera_tests.rs"]
mod tests;
