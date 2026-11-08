//! Retargeting a pose from one skeleton onto another (#560 P5).
//!
//! The first slice is the FK half: take a [`Pose`] evaluated on a source
//! skeleton and produce a pose the target skeleton can play. There is no IK
//! goal, no pole vector and no chain-type mapping yet — those come with the
//! `IKRig` / `RetargeterDef` asset slice, which needs the asset types from
//! `prism-asset` first.
//!
//! Everything here is arithmetic on joints: no assets, no ECS, no renderer.
//!
//! ## What is transferred
//!
//! For every mapped joint pair the rotation is copied (optionally re-expressed
//! relative to the rest poses, so a source clip authored with the arms out does
//! not bend the target's arms), and the translation is rescaled by how far the
//! joint is from its parent *in this frame* compared to the source rest pose.
//! That keeps a short character's feet on the floor when it plays a tall
//! character's walk instead of stretching to the source's bone lengths.

use glam::{Quat, Vec3};
use prism_anim::{JointTransform, Pose, Skeleton};

/// One source → target joint pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JointPair {
    pub source: usize,
    pub target: usize,
}

impl JointPair {
    pub fn new(source: usize, target: usize) -> Self {
        Self { source, target }
    }
}

/// How a pose is moved onto the target skeleton.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetargetOptions {
    /// Re-express rotations relative to both rest poses: `rest_t * (rest_s⁻¹ * anim_s)`.
    ///
    /// Off means "copy the local rotation verbatim", which is what a retarget
    /// pose authored specifically for the pair implies.
    pub relative_to_rest: bool,
    /// Keep the target's bone lengths, scaling each joint's offset by the ratio
    /// between this frame's length and the source rest length.
    pub scale_translation: bool,
    /// Extra scale applied on top of the rest-pose ratio (character height
    /// difference); `1.0` keeps the target's own proportions.
    pub global_scale: f32,
    /// Copy the source root's horizontal motion instead of ignoring it.
    pub transfer_root: bool,
}

impl Default for RetargetOptions {
    fn default() -> Self {
        Self {
            relative_to_rest: true,
            scale_translation: true,
            global_scale: 1.0,
            transfer_root: false,
        }
    }
}

/// Which source joint drives which target joint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetargetMap {
    pairs: Vec<JointPair>,
}

impl RetargetMap {
    /// Build from explicit pairs; a target joint mapped twice keeps the first.
    pub fn new(pairs: Vec<JointPair>) -> Self {
        let mut kept: Vec<JointPair> = Vec::with_capacity(pairs.len());
        for pair in pairs {
            if kept.iter().any(|existing| existing.target == pair.target) {
                continue;
            }
            kept.push(pair);
        }
        Self { pairs: kept }
    }

    /// Match joints with identical names, which is the common case for a
    /// project that re-exports the same rig at a different scale.
    pub fn by_name(source: &Skeleton, target: &Skeleton) -> Self {
        let mut pairs = Vec::new();
        for (source_index, joint) in source.joints().iter().enumerate() {
            if let Some(target_index) = target.joint_index(&joint.name) {
                pairs.push(JointPair::new(source_index, target_index));
            }
        }
        Self::new(pairs)
    }

    pub fn pairs(&self) -> &[JointPair] {
        &self.pairs
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Whether `source` / `target` are in range for every pair of this map.
    pub fn fits(&self, source: &Skeleton, target: &Skeleton) -> bool {
        self.pairs
            .iter()
            .all(|pair| pair.source < source.joint_count() && pair.target < target.joint_count())
    }
}

fn length_ratio(source_rest: Vec3, source_now: Vec3) -> f32 {
    let rest = source_rest.length();
    if !rest.is_finite() || rest <= 1e-5 {
        return 1.0;
    }
    let now = source_now.length();
    if !now.is_finite() {
        return 1.0;
    }
    now / rest
}

/// Move `source_pose` (evaluated on `source`) onto `target`.
///
/// Target joints that are not in the map keep their rest pose, so a map that
/// covers only the FK chain still produces a valid, playable pose. Returns
/// `None` when the map does not fit the skeletons or the pose length does not
/// match the source skeleton: a silently truncated pose would look like a bug in
/// the animation, not in the caller.
pub fn retarget(
    source: &Skeleton,
    source_pose: &Pose,
    target: &Skeleton,
    map: &RetargetMap,
    options: &RetargetOptions,
) -> Option<Pose> {
    if !map.fits(source, target) || source_pose.len() != source.joint_count() {
        return None;
    }
    let mut pose = target.rest_pose();
    let global_scale = if options.global_scale.is_finite() && options.global_scale > 0.0 {
        options.global_scale
    } else {
        1.0
    };
    for pair in map.pairs() {
        let Some(source_local) = source_pose.local(pair.source) else {
            continue;
        };
        let source_joint = &source.joints()[pair.source];
        let target_joint = &target.joints()[pair.target];
        let source_rest = source_joint.reference_local;
        let target_rest = target_joint.reference_local;

        let rotation = if options.relative_to_rest {
            relative_rotation(
                target_rest.rotation,
                source_rest.rotation,
                source_local.rotation,
            )
        } else {
            source_local.rotation
        };
        let is_root = source_joint.parent.is_none() && target_joint.parent.is_none();
        let translation = if is_root {
            if options.transfer_root {
                source_local.translation * global_scale
            } else {
                target_rest.translation
            }
        } else if options.scale_translation {
            let ratio = length_ratio(source_rest.translation, source_local.translation);
            target_rest.translation * ratio * global_scale
        } else {
            target_rest.translation * global_scale
        };
        pose.set_local(
            pair.target,
            JointTransform::new(translation, rotation, source_local.scale),
        );
    }
    Some(pose)
}

/// Convenience wrapper: retarget and return the target's bone palette.
pub fn retarget_palette(
    source: &Skeleton,
    source_pose: &Pose,
    target: &Skeleton,
    map: &RetargetMap,
    options: &RetargetOptions,
) -> Option<Vec<glam::Mat4>> {
    let pose = retarget(source, source_pose, target, map, options)?;
    Some(target.bone_palette(&pose))
}

/// Re-express a single quaternion against two rest poses, exposed for tests.
pub(crate) fn relative_rotation(target_rest: Quat, source_rest: Quat, source_now: Quat) -> Quat {
    target_rest * source_rest.inverse() * source_now
}

#[cfg(test)]
mod tests {
    use glam::{Mat4, Quat, Vec3};
    use prism_anim::skeleton::Joint;
    use prism_anim::{JointTransform, Pose, Skeleton};

    use super::{retarget, JointPair, RetargetMap, RetargetOptions};

    fn joint(name: &str, parent: Option<usize>, at: Vec3) -> Joint {
        Joint {
            name: name.into(),
            parent,
            reference_local: JointTransform::new(at, Quat::IDENTITY, Vec3::ONE),
            inverse_bind: Mat4::IDENTITY,
        }
    }

    /// Two joints, thigh 0.5 m.
    fn skeleton(thigh: f32) -> Skeleton {
        Skeleton::new(vec![
            joint("hips", None, Vec3::new(0.0, 1.0, 0.0)),
            joint("thigh", Some(0), Vec3::new(0.0, -thigh, 0.0)),
        ])
        .expect("valid hierarchy")
    }

    #[test]
    fn identical_skeletons_round_trip_the_pose() {
        let source = skeleton(0.5);
        let target = skeleton(0.5);
        let mut pose = source.rest_pose();
        pose.set_local(
            1,
            JointTransform::new(
                Vec3::new(0.0, -0.5, 0.0),
                Quat::from_rotation_x(0.35),
                Vec3::ONE,
            ),
        );
        let map = RetargetMap::by_name(&source, &target);
        let result = retarget(&source, &pose, &target, &map, &RetargetOptions::default())
            .expect("compatible skeletons");
        let local = result.local(1).expect("thigh");
        assert!((local.rotation.dot(Quat::from_rotation_x(0.35)).abs() - 1.0).abs() < 1e-5);
        assert!((local.translation.y + 0.5).abs() < 1e-5);
    }

    #[test]
    fn a_shorter_target_keeps_its_own_bone_length() {
        let source = skeleton(0.5);
        let target = skeleton(0.25);
        let pose = source.rest_pose();
        let map = RetargetMap::by_name(&source, &target);
        let result = retarget(&source, &pose, &target, &map, &RetargetOptions::default())
            .expect("compatible");
        let local = result.local(1).expect("thigh");
        assert!((local.translation.y + 0.25).abs() < 1e-5, "{local:?}");
    }

    #[test]
    fn a_bent_source_leg_scales_the_target_offset() {
        let source = skeleton(0.5);
        let target = skeleton(0.5);
        let mut pose = source.rest_pose();
        // Fold the leg to 80 % of its rest length, as a crouch would.
        pose.set_local(
            1,
            JointTransform::new(
                Vec3::new(0.0, -0.4, 0.0),
                Quat::from_rotation_x(0.6),
                Vec3::ONE,
            ),
        );
        let map = RetargetMap::by_name(&source, &target);
        let result = retarget(&source, &pose, &target, &map, &RetargetOptions::default())
            .expect("compatible");
        let local = result.local(1).expect("thigh");
        assert!((local.translation.y + 0.4).abs() < 1e-5, "{local:?}");
    }

    #[test]
    fn rotations_are_taken_relative_to_both_rest_poses() {
        // The source rest pose already has the arm out at 90°, the target rest
        // pose does not, so a rest-pose frame must leave the target at rest.
        let source = Skeleton::new(vec![
            joint("hips", None, Vec3::new(0.0, 1.0, 0.0)),
            Joint {
                name: "arm".into(),
                parent: Some(0),
                reference_local: JointTransform::new(
                    Vec3::new(0.5, 0.0, 0.0),
                    Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
                    Vec3::ONE,
                ),
                inverse_bind: Mat4::IDENTITY,
            },
        ])
        .expect("valid");
        let target = Skeleton::new(vec![
            joint("hips", None, Vec3::new(0.0, 1.0, 0.0)),
            joint("arm", Some(0), Vec3::new(0.5, 0.0, 0.0)),
        ])
        .expect("valid");
        let map = RetargetMap::new(vec![JointPair::new(0, 0), JointPair::new(1, 1)]);
        let result = retarget(
            &source,
            &source.rest_pose(),
            &target,
            &map,
            &RetargetOptions::default(),
        )
        .expect("compatible");
        let arm = result.local(1).expect("arm");
        assert!(
            (arm.rotation.dot(Quat::IDENTITY).abs() - 1.0).abs() < 1e-5,
            "{:?}",
            arm.rotation
        );
    }

    #[test]
    fn unmapped_target_joints_keep_their_rest_pose() {
        let source = skeleton(0.5);
        let target = skeleton(0.5);
        let map = RetargetMap::new(vec![JointPair::new(0, 0)]);
        let mut pose = source.rest_pose();
        pose.set_local(
            1,
            JointTransform::new(Vec3::new(0.0, -0.9, 0.0), Quat::IDENTITY, Vec3::ONE),
        );
        let result = retarget(&source, &pose, &target, &map, &RetargetOptions::default())
            .expect("compatible");
        assert_eq!(result.local(1), target.rest_pose().local(1));
    }

    #[test]
    fn incompatible_input_is_rejected() {
        let source = skeleton(0.5);
        let target = skeleton(0.5);
        let map = RetargetMap::new(vec![JointPair::new(0, 7)]);
        assert!(retarget(
            &source,
            &source.rest_pose(),
            &target,
            &map,
            &RetargetOptions::default()
        )
        .is_none());
        let short_pose = Pose::from_locals(Vec::new());
        let good_map = RetargetMap::by_name(&source, &target);
        assert!(retarget(
            &source,
            &short_pose,
            &target,
            &good_map,
            &RetargetOptions::default()
        )
        .is_none());
    }
}
