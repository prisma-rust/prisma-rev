//! 脚部锁定：压掉切换动作时的滑步（#560 P3）。
//!
//! 动作匹配换帧时，新旧帧的步态相位不同：脚在两帧之间瞬移几厘米，看起来就是
//! 地面在脚下滑。锁定层记住「这只脚已经踩实的位置」，之后每次求值把脚拉回
//! 那个位置，直到它自己迈步为止。
//!
//! 纯数学，和 [`crate::selection`] 一样不认识 ECS / 渲染：调用方给出
//! [`Skeleton`] 与 [`Pose`]，本模块**就地**修正姿态里脚关节的局部位移。
//!
//! 修正是有界的（[`FootLockSettings::max_offset`]）：锁太久会把腿拉成长短腿，
//! 宁可松脚也不要毁掉姿态。

use glam::Vec3;
use prism_anim::pose::Pose;
use prism_anim::skeleton::Skeleton;

/// 脚部锁定的参数。
#[derive(Debug, Clone, PartialEq)]
pub struct FootLockSettings {
    /// 要跟踪的关节下标（通常是左右脚 / 脚趾），按骨架关节编号。
    pub joints: Vec<usize>,
    /// 单帧位移速度低于它才算「踩实」（米/秒）。
    pub plant_speed: f32,
    /// 超过它立刻松脚，不再往回拉（米/秒）。
    pub release_speed: f32,
    /// 单帧修正量的上限（米）。防止锁太久把腿拉长。
    pub max_offset: f32,
    /// 0 = 关闭；其余值缩放修正量。
    pub weight: f32,
}

impl Default for FootLockSettings {
    fn default() -> Self {
        Self {
            joints: Vec::new(),
            plant_speed: 0.35,
            release_speed: 0.9,
            max_offset: 0.25,
            weight: 1.0,
        }
    }
}

impl FootLockSettings {
    /// 同一组关节，按名字解析；找不到的名字被忽略。
    pub fn tracking_all(skeleton: &Skeleton) -> Self {
        Self {
            joints: (0..skeleton.joints().len()).collect(),
            ..Self::default()
        }
    }

    fn sanitized(&self) -> (f32, f32, f32, f32) {
        let positive = |value: f32, fallback: f32| {
            if value.is_finite() && value > 0.0 {
                value
            } else {
                fallback
            }
        };
        let plant = positive(self.plant_speed, 0.35);
        let release = positive(self.release_speed, 0.9).max(plant);
        let max_offset = positive(self.max_offset, 0.25);
        let weight = if self.weight.is_finite() {
            self.weight.clamp(0.0, 1.0)
        } else {
            0.0
        };
        (plant, release, max_offset, weight)
    }
}

/// 每只脚的状态。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct FootState {
    /// 上一次求值后脚的世界位置。
    last: Option<Vec3>,
    /// 踩实的世界位置；`None` = 这只脚正在迈步。
    planted: Option<Vec3>,
}

/// 一次 [`FootLock::update`] 的结果，便于调试面板显示。
#[derive(Debug, Clone, PartialEq)]
pub struct FootLockReport {
    /// 本次被拉回原位的脚数。
    pub locked: usize,
    /// 本次结束时仍然踩实的脚数。
    pub planted: usize,
    /// 本次修正量的最大值（米）。
    pub max_correction: f32,
}

impl FootLockReport {
    fn empty() -> Self {
        Self {
            locked: 0,
            planted: 0,
            max_correction: 0.0,
        }
    }
}

/// 跨帧的脚部锁定状态（每只脚一个槽，顺序与 `settings.joints` 一致）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FootLock {
    feet: Vec<FootState>,
}

impl FootLock {
    /// 按跟踪的脚数分配状态。
    pub fn new(feet: usize) -> Self {
        Self {
            feet: vec![FootState::default(); feet],
        }
    }

    /// 换骨架 / 换场景 / 传送后调用：下一帧重新判定踩实。
    pub fn reset(&mut self) {
        for foot in &mut self.feet {
            *foot = FootState::default();
        }
    }

    pub fn feet(&self) -> usize {
        self.feet.len()
    }

    /// 就地修正姿态里的脚关节位移，返回本次锁定情况。
    ///
    /// `dt` 非正 / 非有限时只记录位置、不做修正：没有时间步就谈不上速度。
    pub fn update(
        &mut self,
        skeleton: &Skeleton,
        pose: &mut Pose,
        settings: &FootLockSettings,
        dt: f32,
    ) -> FootLockReport {
        let (plant_speed, release_speed, max_offset, weight) = settings.sanitized();
        if settings.joints.len() != self.feet.len() {
            self.feet = vec![FootState::default(); settings.joints.len()];
        }
        if settings.joints.is_empty() || weight <= 0.0 {
            return FootLockReport::empty();
        }
        let joints = skeleton.joints();
        let world = skeleton.world_matrices(pose);
        let usable = |joint: usize| joint < joints.len() && joint < world.len();
        let valid_dt = dt.is_finite() && dt > 0.0;

        let mut report = FootLockReport::empty();
        for (slot, joint) in settings.joints.iter().copied().enumerate() {
            if !usable(joint) {
                continue;
            }
            let current = world[joint].w_axis.truncate();
            let foot = &mut self.feet[slot];

            // 速度用「上次求值后」的位置算：锁定修正本身不进速度，否则会自我抵消。
            let speed = match (foot.last, valid_dt) {
                (Some(last), true) => (current - last).length() / dt,
                _ => 0.0,
            };

            if let Some(anchor) = foot.planted {
                if valid_dt && speed > release_speed {
                    foot.planted = None;
                } else {
                    let delta = anchor - current;
                    let correction = delta.clamp_length_max(max_offset) * weight;
                    if correction.length_squared() > 1e-12 {
                        if let (Some(local), Some(parent_world)) =
                            (pose.local(joint), parent_rotation(&world, joints, joint))
                        {
                            let local_delta = parent_world.inverse() * correction;
                            pose.set_local(
                                joint,
                                prism_anim::skeleton::JointTransform {
                                    translation: local.translation + local_delta,
                                    ..local
                                },
                            );
                        }
                        report.locked += 1;
                        report.max_correction = report.max_correction.max(correction.length());
                    }
                }
            }

            if foot.planted.is_none() && valid_dt && speed <= plant_speed {
                foot.planted = Some(current);
            }

            // 记住修正后的位置：下一次速度判定从「看起来在哪」出发。
            foot.last = Some(match foot.planted {
                Some(anchor) if report.locked > 0 => anchor,
                _ => current,
            });
            if foot.planted.is_some() {
                report.planted += 1;
            }
        }
        report
    }
}

/// 父关节的世界旋转；根关节返回单位旋转。
fn parent_rotation(
    world: &[glam::Mat4],
    joints: &[prism_anim::skeleton::Joint],
    joint: usize,
) -> Option<glam::Quat> {
    let parent = joints.get(joint)?.parent;
    match parent {
        Some(parent) if parent < world.len() => {
            Some(world[parent].to_scale_rotation_translation().1)
        }
        _ => Some(glam::Quat::IDENTITY),
    }
}

#[cfg(test)]
mod tests {
    use glam::Vec3;
    use prism_anim::pose::Pose;
    use prism_anim::skeleton::{Joint, JointTransform, Skeleton};

    use super::{FootLock, FootLockSettings};

    fn skeleton() -> Skeleton {
        // root -> thigh -> foot，沿 +Y 的直腿。
        Skeleton::new(vec![
            Joint {
                name: "root".into(),
                parent: None,
                reference_local: JointTransform {
                    translation: Vec3::ZERO,
                    rotation: glam::Quat::IDENTITY,
                    scale: Vec3::ONE,
                },
                inverse_bind: glam::Mat4::IDENTITY,
            },
            Joint {
                name: "thigh".into(),
                parent: Some(0),
                reference_local: JointTransform {
                    translation: Vec3::new(0.0, -0.5, 0.0),
                    rotation: glam::Quat::IDENTITY,
                    scale: Vec3::ONE,
                },
                inverse_bind: glam::Mat4::IDENTITY,
            },
            Joint {
                name: "foot".into(),
                parent: Some(1),
                reference_local: JointTransform {
                    translation: Vec3::new(0.0, -0.5, 0.0),
                    rotation: glam::Quat::IDENTITY,
                    scale: Vec3::ONE,
                },
                inverse_bind: glam::Mat4::IDENTITY,
            },
        ])
        .expect("valid skeleton")
    }

    fn foot_world(skeleton: &Skeleton, pose: &Pose) -> Vec3 {
        skeleton.world_matrices(pose)[2].w_axis.truncate()
    }

    fn settings() -> FootLockSettings {
        FootLockSettings {
            joints: vec![2],
            ..FootLockSettings::default()
        }
    }

    #[test]
    fn a_planted_foot_is_held_through_a_small_shift() {
        let skeleton = skeleton();
        let mut pose = skeleton.rest_pose();
        let mut lock = FootLock::new(1);
        let settings = settings();

        // 第一帧：脚在 (0, -1, 0) 且不动 → 踩实。
        lock.update(&skeleton, &mut pose, &settings, 1.0 / 60.0);
        let anchor = foot_world(&skeleton, &pose);

        // 骨架整体前移 1 cm：没有锁定的话脚也会跟着走。
        let thigh = pose.local(1).unwrap();
        pose.set_local(
            1,
            JointTransform {
                translation: thigh.translation + Vec3::new(0.0, 0.0, -0.01),
                ..thigh
            },
        );
        let report = lock.update(&skeleton, &mut pose, &settings, 1.0 / 60.0);
        assert_eq!(report.locked, 1);
        let held = foot_world(&skeleton, &pose);
        assert!((held - anchor).length() < 1e-4, "{held:?} vs {anchor:?}");
    }

    #[test]
    fn a_stepping_foot_is_released() {
        let skeleton = skeleton();
        let mut pose = skeleton.rest_pose();
        let mut lock = FootLock::new(1);
        let settings = settings();
        lock.update(&skeleton, &mut pose, &settings, 1.0 / 60.0);

        // 一帧走 10 cm ≈ 6 m/s → 该迈步了，不再往回拉。
        let thigh = pose.local(1).unwrap();
        pose.set_local(
            1,
            JointTransform {
                translation: thigh.translation + Vec3::new(0.0, 0.0, -0.1),
                ..thigh
            },
        );
        let report = lock.update(&skeleton, &mut pose, &settings, 1.0 / 60.0);
        assert_eq!(report.locked, 0);
        assert_eq!(report.planted, 0);
    }

    #[test]
    fn the_correction_is_clamped() {
        let skeleton = skeleton();
        let mut pose = skeleton.rest_pose();
        let mut lock = FootLock::new(1);
        let settings = FootLockSettings {
            joints: vec![2],
            max_offset: 0.01,
            // 让脚「慢到算踩实」但位移仍超过上限：只拉回 1 cm。
            release_speed: 100.0,
            ..FootLockSettings::default()
        };
        lock.update(&skeleton, &mut pose, &settings, 1.0 / 60.0);
        let anchor = foot_world(&skeleton, &pose);
        let thigh = pose.local(1).unwrap();
        pose.set_local(
            1,
            JointTransform {
                translation: thigh.translation + Vec3::new(0.0, 0.0, -0.05),
                ..thigh
            },
        );
        let report = lock.update(&skeleton, &mut pose, &settings, 1.0 / 60.0);
        assert_eq!(report.locked, 1);
        assert!(report.max_correction <= 0.01 + 1e-6, "{report:?}");
        let held = foot_world(&skeleton, &pose);
        assert!((held - anchor).length() > 0.03, "partially recovered");
    }

    #[test]
    fn weight_zero_and_bad_dt_leave_the_pose_alone() {
        let skeleton = skeleton();
        let mut pose = skeleton.rest_pose();
        let before = pose.local(2).unwrap();
        let mut lock = FootLock::new(1);
        let report = lock.update(
            &skeleton,
            &mut pose,
            &FootLockSettings {
                joints: vec![2],
                weight: 0.0,
                ..FootLockSettings::default()
            },
            1.0 / 60.0,
        );
        assert_eq!(report.locked, 0);
        assert_eq!(pose.local(2).unwrap(), before);

        // dt = 0 时不判速度，也就不会锁。
        let report = lock.update(&skeleton, &mut pose, &settings(), 0.0);
        assert_eq!(report.planted, 0);
        assert_eq!(pose.local(2).unwrap(), before);
    }

    #[test]
    fn the_settings_are_sanitized_and_out_of_range_joints_are_skipped() {
        let skeleton = skeleton();
        let mut pose = skeleton.rest_pose();
        let mut lock = FootLock::new(2);
        let settings = FootLockSettings {
            joints: vec![2, 99],
            release_speed: f32::NAN,
            plant_speed: -1.0,
            max_offset: f32::INFINITY,
            weight: f32::INFINITY,
        };
        // 非有限的 weight 视为「关闭」：不猜调用方想要 0 还是 1。
        let report = lock.update(&skeleton, &mut pose, &settings, 1.0 / 60.0);
        assert_eq!(report.planted, 0);
        assert_eq!(lock.feet(), 2);

        // 其他非法值回落到默认；越界关节被跳过，不 panic。
        let sane = FootLockSettings {
            joints: vec![2, 99],
            ..FootLockSettings::default()
        };
        let report = lock.update(&skeleton, &mut pose, &sane, 1.0 / 60.0);
        assert_eq!(report.planted, 1);

        lock.reset();
        assert_eq!(lock.feet(), 2);
        let report = lock.update(&skeleton, &mut pose, &sane, 1.0 / 60.0);
        assert_eq!(report.planted, 1, "reset re-plants on the next frame");
    }
}
