//! Nearest-neighbour lookup over a [`MotionDatabase`].

use crate::database::MotionDatabase;
use crate::pose_feature::PoseFeature;
use crate::trajectory::{Trajectory, TrajectoryWeights};

/// Relative weight of the two halves of the cost.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchWeights {
    pub pose: f32,
    pub trajectory: f32,
    pub trajectory_weights: TrajectoryWeights,
}

impl Default for MatchWeights {
    fn default() -> Self {
        Self {
            pose: 1.0,
            trajectory: 1.0,
            trajectory_weights: TrajectoryWeights::default(),
        }
    }
}

/// Best frame for a query, with the cost broken down for debugging / tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionMatch {
    /// Index into the database.
    pub frame: usize,
    /// Clip and time of that frame, i.e. where to start playing.
    pub clip: usize,
    pub time: f32,
    pub pose_cost: f32,
    pub trajectory_cost: f32,
    pub cost: f32,
}

/// Linear scan for the frame that best continues `pose` along `trajectory`.
///
/// Frames whose features were built with a different joint list or trajectory
/// shape are skipped: comparing them would be meaningless rather than merely
/// expensive. Returns `None` when nothing is comparable.
pub fn search(
    database: &MotionDatabase,
    pose: &PoseFeature,
    trajectory: &Trajectory,
    weights: &MatchWeights,
) -> Option<MotionMatch> {
    let pose_weight = sanitize(weights.pose);
    let trajectory_weight = sanitize(weights.trajectory);
    let mut best: Option<MotionMatch> = None;
    for (index, frame) in database.frames().iter().enumerate() {
        let Some(pose_cost) = pose.distance_to(&frame.pose) else {
            continue;
        };
        let Some(trajectory_cost) =
            trajectory.distance_to(&frame.trajectory, &weights.trajectory_weights)
        else {
            continue;
        };
        let cost = pose_cost * pose_weight + trajectory_cost * trajectory_weight;
        let improves = match best {
            Some(best) => cost < best.cost,
            None => true,
        };
        if improves {
            best = Some(MotionMatch {
                frame: index,
                clip: frame.clip,
                time: frame.time,
                pose_cost,
                trajectory_cost,
                cost,
            });
        }
    }
    best
}

fn sanitize(weight: f32) -> f32 {
    if weight.is_finite() && weight > 0.0 {
        weight
    } else {
        0.0
    }
}
