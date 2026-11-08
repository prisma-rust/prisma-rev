//! 船体漂移 + 相机跟随。
//!
//! M0 阶段船不接受输入，只按正弦轨迹缓慢漂移，用来验证
//! 「正交相机 + 2D 精灵 + 排序」整条链路；后续由 `fishing` 状态机接管
//! 输入（WASD / 手柄）与抛竿逻辑。
//!
//! 状态与写入都走 [`prism_game_api::GameWorld`]：同一份逻辑在静态 Player 和
//! 编辑器加载的动态模块里都能跑，且不需要引擎类型。

use prism_game_api::{EntityHandle, GameWorld};
use prism_scene::components::LocalTransform;

/// 相机离场景平面的距离（相机在 +Z 看向 −Z）。
const CAMERA_Z: f32 = 10.0;
/// 相机相对船的水面高度（相机在船的斜上方）。
const CAMERA_LIFT: f32 = 100.0;
/// 船漂移的横向幅度（世界单位）。
const DRIFT_AMPLITUDE: f32 = 40.0;
/// 漂移角速度（弧度/秒）。
const DRIFT_SPEED: f32 = 0.35;
/// 船的基础位置。
const BOAT_BASE: [f32; 3] = [0.0, 80.0, 0.0];

/// 漂移状态——**项目私有**，由调用方持有（Player 侧放在 ECS resource 里）。
#[derive(Default)]
pub struct BoatState {
    elapsed: f32,
    boat: Option<EntityHandle>,
    camera: Option<EntityHandle>,
}

/// 每帧推进船与相机。
pub fn tick(state: &mut Option<BoatState>, world: &mut GameWorld<'_>, dt: f32) {
    if state.is_none() {
        *state = Some(BoatState {
            boat: world.find_by_name("boat"),
            camera: world.find_by_name("camera"),
            ..BoatState::default()
        });
    }

    let Some(current) = state.as_mut() else {
        return;
    };
    current.elapsed += dt;

    let drift = (current.elapsed * DRIFT_SPEED).sin() * DRIFT_AMPLITUDE;
    let bob = (current.elapsed * 1.3).sin() * 0.06;

    if let Some(boat) = current.boat {
        if let Some(mut transform) = world.get::<LocalTransform>(boat) {
            transform.position =
                glam::Vec3::new(BOAT_BASE[0] + drift, BOAT_BASE[1] + bob, BOAT_BASE[2]);
            // 轻微摇摆，让像素画看起来是浮在水上。
            let angle = (current.elapsed * 1.1).sin() * 0.035;
            transform.rotation = glam::Quat::from_rotation_z(angle);
            world.set(boat, transform);
        }
    }

    if let Some(camera) = current.camera {
        if let Some(mut transform) = world.get::<LocalTransform>(camera) {
            // 相机跟随船的一半幅度，保留一点视差。
            transform.position = glam::Vec3::new(drift * 0.5, BOAT_BASE[1] + CAMERA_LIFT, CAMERA_Z);
            world.set(camera, transform);
        }
    }
}
