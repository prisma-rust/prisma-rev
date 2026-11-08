//! Nearest-neighbour lookup over a [`MotionDatabase`].

use crate::database::MotionDatabase;
use crate::pose_feature::PoseFeature;
use crate::selection::{SearchBias, SelectionHistory};
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
    search_biased(database, pose, trajectory, weights, SearchBias::NONE, None)
}

/// [`search`] with a sticky decision: the frame that is still playing is kept
/// unless another frame is clearly better, and frames picked moments ago are
/// penalised so the choice does not oscillate between near-identical samples.
///
/// The returned `cost` includes the multiplier; `pose_cost` and
/// `trajectory_cost` stay raw so a debug view can show the unmodified distance.
pub fn search_biased(
    database: &MotionDatabase,
    pose: &PoseFeature,
    trajectory: &Trajectory,
    weights: &MatchWeights,
    bias: SearchBias,
    history: Option<&SelectionHistory>,
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
        let raw = pose_cost * pose_weight + trajectory_cost * trajectory_weight;
        let cost = match history {
            Some(history) => raw * bias.multiplier(index, history),
            None => raw,
        };
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
