//! Analytic two-bone IK (`hip -> knee -> foot`, `shoulder -> elbow -> wrist`).
//!
//! The solver works in component space on joint *positions*: it never looks at
//! the skeleton hierarchy, which keeps it usable from the runtime, the editor
//! preview and offline tools. [`apply_two_bone_ik`] is the only helper that knows
//! about a `Skeleton` / `Pose` pair.

use glam::{Quat, Vec3};

use prism_anim::{JointTransform, Pose, Skeleton};

/// Lengths (and target distances) below this are treated as degenerate.
///
/// Bones shorter than a fraction of a millimetre are not a thing in authored
/// skeletons, but they show up in malformed cooked data and would otherwise
/// produce NaNs from the normalisation below.
const MIN_LENGTH: f32 = 1e-4;

/// Three consecutive joints of a limb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwoBoneChain {
    /// Upper bone, e.g. the thigh (hip end).
    pub upper: usize,
    /// Lower bone, e.g. the shin (knee joint).
    pub middle: usize,
    /// End of the chain, e.g. the foot (ankle joint).
    pub end: usize,
}

impl TwoBoneChain {
    pub const fn new(upper: usize, middle: usize, end: usize) -> Self {
        Self { upper, middle, end }
    }

    /// The three joints must exist and be distinct: a duplicated joint would
    /// silently rotate the same bone twice.
    fn is_valid_for(&self, joint_count: usize) -> bool {
        self.upper != self.middle
            && self.upper != self.end
            && self.middle != self.end
            && self.upper < joint_count
            && self.middle < joint_count
            && self.end < joint_count
    }
}

/// Result of [`solve_two_bone`].
///
/// The rotations are **world-space deltas**: `new_world_rotation = delta * old`.
/// Positions are the values the solver aimed for, already clamped and blended by
/// the requested weight; [`apply_two_bone_ik`] reports the positions the pose
/// actually ends up with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwoBoneSolution {
    /// Delta rotation for the upper joint.
    pub upper_rotation: Quat,
    /// Delta rotation for the middle joint (already composed with the upper one).
    pub middle_rotation: Quat,
    /// Middle joint position after blending.
    pub middle_position: Vec3,
    /// End joint position after blending.
    pub end_position: Vec3,
    /// Distance from the upper joint along the target direction, after clamping.
    pub reach: f32,
    /// `true` when the target was out of reach or too close, so the chain
    /// straightened / folded instead of hitting it exactly.
    pub clamped: bool,
}

fn is_finite(point: Vec3) -> bool {
    point.is_finite()
}

/// Solve a two-bone chain analytically.
///
/// `upper` / `middle` / `end` are joint positions in component space, `target`
/// the wanted end position and `pole` a direction the bend should point to
/// (e.g. the knee should point towards the pole). `weight` in `0..=1` blends the
/// solution against the current pose: `0` (or less) solves nothing and returns
/// `None`; `1` applies it fully.
///
/// Returns `None` for degenerate input: non-finite positions, zero-length bones,
/// a target on top of the upper joint, or an unreachable fold (`|a - b|` larger
/// than `a + b`, which cannot happen for real bones but can for corrupted data).
pub fn solve_two_bone(
    upper: Vec3,
    middle: Vec3,
    end: Vec3,
    target: Vec3,
    pole: Vec3,
    weight: f32,
) -> Option<TwoBoneSolution> {
    if !weight.is_finite() || weight <= 0.0 {
        return None;
    }
    let weight = weight.min(1.0);
    if !is_finite(upper) || !is_finite(middle) || !is_finite(end) || !is_finite(target) {
        return None;
    }

    let upper_vector = middle - upper;
    let lower_vector = end - middle;
    let upper_length = upper_vector.length();
    let lower_length = lower_vector.length();
    if upper_length < MIN_LENGTH || lower_length < MIN_LENGTH {
        return None;
    }

    let to_target = target - upper;
    let target_distance = to_target.length();
    if target_distance < MIN_LENGTH {
        return None;
    }
    let direction = to_target / target_distance;

    // Keep a sliver of bend at both extremes: a perfectly straight chain has an
    // undefined bend plane and `from_rotation_arc` would return identity anyway.
    let max_reach = upper_length + lower_length - MIN_LENGTH;
    let min_reach = (upper_length - lower_length).abs() + MIN_LENGTH;
    if min_reach >= max_reach {
        return None;
    }
    let reach = target_distance.clamp(min_reach, max_reach);
    let clamped = (reach - target_distance).abs() > 1e-6;

    // Bend plane: the pole projected perpendicular to the target direction.
    // A pole parallel to the direction cannot define a plane, so fall back to
    // the axis least aligned with it (stable and deterministic per target).
    let mut bend = pole - direction * pole.dot(direction);
    if bend.length() < MIN_LENGTH {
        let axis = if direction.x.abs() < 0.9 {
            Vec3::X
        } else {
            Vec3::Y
        };
        bend = axis - direction * axis.dot(direction);
    }
    if bend.length() < MIN_LENGTH {
        return None;
    }
    let bend = bend.normalize();

    // Law of cosines at the upper joint.
    let cos_upper = ((upper_length * upper_length + reach * reach - lower_length * lower_length)
        / (2.0 * upper_length * reach))
        .clamp(-1.0, 1.0);
    let sin_upper = (1.0 - cos_upper * cos_upper).max(0.0).sqrt();

    let solved_middle =
        upper + direction * (cos_upper * upper_length) + bend * (sin_upper * upper_length);
    let solved_end = upper + direction * reach;

    let mut upper_rotation = Quat::from_rotation_arc(
        (upper_vector / upper_length).normalize(),
        (solved_middle - upper).normalize(),
    );
    let rotated_lower = upper_rotation * (lower_vector / lower_length);
    let mut middle_rotation_local = Quat::from_rotation_arc(
        rotated_lower.normalize(),
        (solved_end - solved_middle).normalize(),
    );

    if weight < 1.0 {
        upper_rotation = Quat::IDENTITY.slerp(upper_rotation, weight);
        middle_rotation_local = Quat::IDENTITY.slerp(middle_rotation_local, weight);
    }
    let middle_rotation = middle_rotation_local * upper_rotation;

    Some(TwoBoneSolution {
        upper_rotation,
        middle_rotation,
        middle_position: middle.lerp(solved_middle, weight),
        end_position: end.lerp(solved_end, weight),
        reach,
        clamped,
    })
}

/// Solve and write a two-bone chain back into `pose`.
///
/// `target` and `pole` are component space (the space of
/// [`Skeleton::world_matrices`]). The world-space deltas from
/// [`solve_two_bone`] are converted into local rotations for the two bones, so
/// the rest of the hierarchy is untouched; the returned solution reports the
/// positions the pose actually reached after blending.
pub fn apply_two_bone_ik(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: TwoBoneChain,
    target: Vec3,
    pole: Vec3,
    weight: f32,
) -> Option<TwoBoneSolution> {
    if !chain.is_valid_for(skeleton.joint_count()) {
        return None;
    }
    let world = skeleton.world_matrices(pose);
    let (_, upper_world_rotation, upper_position) =
        world[chain.upper].to_scale_rotation_translation();
    let (_, _, middle_position) = world[chain.middle].to_scale_rotation_translation();
    let (_, _, end_position) = world[chain.end].to_scale_rotation_translation();

    let mut solution = solve_two_bone(
        upper_position,
        middle_position,
        end_position,
        target,
        pole,
        weight,
    )?;

    // Local delta for a bone: `parent_world^-1 * world_delta * parent_world`,
    // so that `world_delta * parent_world * local` composes correctly.
    let parent_world_rotation = skeleton.joints()[chain.upper]
        .parent
        .map(|parent| world[parent].to_scale_rotation_translation().1)
        .unwrap_or(Quat::IDENTITY);
    let upper_local = pose.local(chain.upper)?;
    let upper_delta_local =
        parent_world_rotation.inverse() * solution.upper_rotation * parent_world_rotation;
    pose.set_local(
        chain.upper,
        JointTransform {
            rotation: (upper_delta_local * upper_local.rotation).normalize(),
            ..upper_local
        },
    );

    // The middle bone's parent is the upper bone, whose world rotation changed.
    let upper_world_after = solution.upper_rotation * upper_world_rotation;
    let middle_local = pose.local(chain.middle)?;
    let middle_delta_local =
        upper_world_after.inverse() * solution.middle_rotation * upper_world_rotation;
    pose.set_local(
        chain.middle,
        JointTransform {
            rotation: (middle_delta_local * middle_local.rotation).normalize(),
            ..middle_local
        },
    );

    // Report where the chain actually ended up: slerp on the two rotations is not
    // the same curve as lerping the positions.
    let posed = skeleton.world_matrices(pose);
    let (_, _, posed_middle) = posed[chain.middle].to_scale_rotation_translation();
    let (_, _, posed_end) = posed[chain.end].to_scale_rotation_translation();
    solution.middle_position = posed_middle;
    solution.end_position = posed_end;
    Some(solution)
}

#[cfg(test)]
#[path = "two_bone_tests.rs"]
mod tests;
