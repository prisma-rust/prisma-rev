//! Foot placement: probe the ground under each foot, plant the ones that have
//! ground, and let the pelvis follow so the legs stay inside their reach.
//!
//! The solver is deliberately ground-agnostic: the caller supplies a
//! [`GroundQuery`] (in the engine that is the physics raycast, in tests a flat
//! or stepped surface) and the solver only does kinematics. Everything is in
//! component space, like [`crate::two_bone`].

use glam::Vec3;

use prism_anim::{Pose, Skeleton};

use crate::two_bone::{apply_two_bone_ik, TwoBoneChain};

/// One downward ground probe result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundHit {
    pub point: Vec3,
    /// Surface normal, normalized by the solver.
    pub normal: Vec3,
}

/// Whatever can answer "what is under this point".
pub trait GroundQuery {
    /// Cast from `origin` along `direction` (normalized) up to `max_distance`.
    fn cast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<GroundHit>;
}

/// Tuning for [`FootPlacement::update`]. Distances are world units (metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FootPlacementSettings {
    /// How far above the ankle the downward probe starts.
    pub probe_height: f32,
    /// Ground higher than this above the ankle is ignored (a wall, not a step).
    pub max_step_up: f32,
    /// Ground farther than this below the ankle counts as no ground.
    pub max_step_down: f32,
    /// Ankle height above the ground when planted.
    pub foot_offset: f32,
    /// How far the pelvis may be lowered (negative value) when a planted foot
    /// would otherwise pull the leg past its reach.
    pub pelvis_min: f32,
    /// Time constant of the target / weight smoothing; `0` snaps immediately.
    pub blend_seconds: f32,
    /// Global weight (`0` disables foot placement without removing the system).
    pub weight: f32,
    /// Pole direction per leg (component space); the knee bends towards it.
    pub leg_pole: [Vec3; 2],
}

impl Default for FootPlacementSettings {
    fn default() -> Self {
        Self {
            probe_height: 0.15,
            max_step_up: 0.35,
            max_step_down: 0.45,
            foot_offset: 0.05,
            pelvis_min: -0.25,
            blend_seconds: 0.08,
            weight: 1.0,
            leg_pole: [Vec3::Z, Vec3::Z],
        }
    }
}

/// Per-foot state kept across frames.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FootState {
    /// `true` while the foot has ground under it.
    pub planted: bool,
    /// Smoothed ankle target in component space.
    pub position: Vec3,
    /// `0..=1` blend actually applied this frame.
    pub weight: f32,
}

/// What [`FootPlacement::update`] did this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FootPlacementReport {
    pub feet: [FootState; 2],
    /// Vertical pelvis correction applied to the hips joint (<= 0).
    pub pelvis_offset: f32,
    /// `false` when the input was rejected (pose left untouched).
    pub applied: bool,
}

/// Two-feet placement state.
#[derive(Debug, Clone, PartialEq)]
pub struct FootPlacement {
    feet: [FootState; 2],
    /// Pelvis offset already baked into the pose, so the correction is applied
    /// as a delta instead of accumulating frame after frame.
    applied_pelvis: f32,
}

impl Default for FootPlacement {
    fn default() -> Self {
        Self::new()
    }
}

fn smoothing_alpha(dt: f32, seconds: f32) -> f32 {
    if !dt.is_finite() || dt <= 0.0 {
        return 0.0;
    }
    if !seconds.is_finite() || seconds <= 0.0 {
        return 1.0;
    }
    (1.0 - (-dt / seconds).exp()).clamp(0.0, 1.0)
}

fn ankle(world: &[glam::Mat4], joint: usize) -> Vec3 {
    world[joint].to_scale_rotation_translation().2
}

impl FootPlacement {
    pub fn new() -> Self {
        Self {
            feet: [FootState::default(); 2],
            applied_pelvis: 0.0,
        }
    }

    pub fn foot(&self, index: usize) -> Option<&FootState> {
        self.feet.get(index)
    }

    /// Recompute both feet and write the result into `pose`.
    ///
    /// `hips` is the joint the two legs hang from; `legs` are the
    /// `thigh -> shin -> ankle` chains. The pelvis correction is a translation
    /// of the `hips` joint, so it is only exact while `hips` is the root joint:
    /// otherwise the offset is applied in the hips' parent space along component
    /// `+Y`, which is what authored rigs expect but is still an approximation.
    ///
    /// Returns `applied = false` (and leaves `pose` untouched) for invalid
    /// chains, a missing pelvis joint, or a pose shorter than the skeleton.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        skeleton: &Skeleton,
        pose: &mut Pose,
        hips: usize,
        legs: [TwoBoneChain; 2],
        settings: &FootPlacementSettings,
        ground: &dyn GroundQuery,
        dt: f32,
    ) -> FootPlacementReport {
        let mut report = FootPlacementReport {
            feet: self.feet,
            pelvis_offset: 0.0,
            applied: false,
        };
        let joint_count = skeleton.joint_count();
        // The upper bone of each leg hangs from the pelvis; the pelvis itself is
        // never rotated here (both legs share it and would be applied twice).
        let legs_hang_from_hips = legs.iter().all(|leg| {
            leg.is_valid_for(joint_count) && skeleton.joints()[leg.upper].parent == Some(hips)
        });
        if hips >= joint_count || pose.len() != joint_count || !legs_hang_from_hips {
            return report;
        }
        let global_weight = if settings.weight.is_finite() {
            settings.weight.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if global_weight <= 0.0 {
            self.feet = [FootState::default(); 2];
            report.feet = self.feet;
            report.applied = true;
            return report;
        }

        let alpha = smoothing_alpha(dt, settings.blend_seconds);
        let up = Vec3::Y;
        let world = skeleton.world_matrices(pose);
        let hip_position = ankle(&world, hips);

        // 1. Resolve each foot against the ground and smooth its target.
        let mut targets = [Vec3::ZERO; 2];
        for index in 0..2 {
            let current = ankle(&world, legs[index].end);
            let cast = ground.cast(
                current + up * settings.probe_height.max(0.0),
                -up,
                settings.probe_height.max(0.0) + settings.max_step_down.max(0.0),
            );
            let desired = cast.and_then(|hit| {
                let normal = if hit.normal.length_squared() > 1e-8 {
                    hit.normal.normalize()
                } else {
                    up
                };
                let height = (hit.point - current).dot(up);
                // Anything above `max_step_up` is a wall/ceiling, and anything
                // below `max_step_down` is a hole: both unplant the foot.
                if height > settings.max_step_up.max(0.0) {
                    return None;
                }
                Some(hit.point + normal * settings.foot_offset)
            });

            let state = &mut self.feet[index];
            match desired {
                Some(desired) => {
                    let target = if state.planted && settings.blend_seconds > 0.0 {
                        state.position.lerp(desired, alpha)
                    } else {
                        desired
                    };
                    state.position = target;
                    state.planted = true;
                    state.weight =
                        (state.weight + (global_weight - state.weight) * alpha).clamp(0.0, 1.0);
                }
                None => {
                    state.planted = false;
                    state.weight = (state.weight - state.weight * alpha).clamp(0.0, 1.0);
                }
            }
            targets[index] = state.position;
        }

        // 2. Pelvis correction: lower it just enough for the most over-stretched
        // leg. Raising the pelvis would only put planted feet further out of
        // reach, so a foot that is above the stance is left to the IK (the knee
        // simply bends further).
        // Measure against the un-adjusted pelvis, not the pose we already moved:
        // otherwise the correction would be recomputed as zero as soon as the
        // previous frame's drop made the leg reach and the pelvis would bounce.
        let hip_reference = hip_position - up * self.applied_pelvis;
        let mut required_drop = 0.0f32;
        for index in 0..2 {
            if self.feet[index].weight <= 0.0 {
                continue;
            }
            let (_, _, knee) = world[legs[index].middle].to_scale_rotation_translation();
            let (_, _, foot) = world[legs[index].end].to_scale_rotation_translation();
            let reach = (knee.distance(hip_reference) + foot.distance(knee)) * 0.98;
            let needed = (targets[index] - hip_reference).length() - reach;
            required_drop = required_drop.max(needed);
        }
        let pelvis_offset = (-required_drop).clamp(settings.pelvis_min.min(0.0), 0.0);
        let pelvis_delta = pelvis_offset - self.applied_pelvis;
        if pelvis_delta.abs() > 1e-6 {
            if let Some(hips_local) = pose.local(hips) {
                pose.set_local(
                    hips,
                    prism_anim::JointTransform {
                        translation: hips_local.translation + up * pelvis_delta,
                        ..hips_local
                    },
                );
            }
            self.applied_pelvis = pelvis_offset;
        }

        // 3. Bend each leg onto its target.
        for index in 0..2 {
            let weight = self.feet[index].weight;
            if weight <= 0.0 {
                continue;
            }
            let _ = apply_two_bone_ik(
                skeleton,
                pose,
                legs[index],
                targets[index],
                settings.leg_pole[index],
                weight,
            );
        }

        report.feet = self.feet;
        report.pelvis_offset = pelvis_offset;
        report.applied = true;
        report
    }

    /// Drop all plant state, e.g. after a teleport or a lane change.
    ///
    /// The pelvis offset stays accounted for: it is still baked into the pose the
    /// caller is animating, so the next update corrects it by a delta instead of
    /// applying it twice.
    pub fn reset(&mut self) {
        self.feet = [FootState::default(); 2];
    }
}

#[cfg(test)]
#[path = "foot_tests.rs"]
mod tests;
