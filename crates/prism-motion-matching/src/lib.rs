//! Motion matching core (#560 P3).
//!
//! The pieces are deliberately independent of the ECS and the renderer, the same
//! way [`prism_anim`] keeps clips and poses free of engine types:
//!
//! - [`trajectory`] — where the character is asked to go, as a short series of
//!   future samples (position, velocity, facing).
//! - [`pose_feature`] — a compact signature of a pose (tracked joint positions
//!   and velocities in skeleton-root space).
//! - [`database`] — a clip sampled into frames that carry both features.
//! - [`search`] — nearest-neighbour lookup of the frame that best continues both
//!   the current pose and the desired trajectory.
//!
//! Everything is brute force on purpose: the first slice is about the data model
//! and the cost function, and a linear scan of a few thousand frames is cheap
//! enough. A KD-tree / VP-tree belongs behind [`search::search`] once profiling
//! says otherwise.
//!
//! Conventions match the animator: model space is the skeleton root's space, the
//! entity transform is applied later, and `+Y` is up.

pub mod database;
pub mod pose_feature;
pub mod search;
pub mod trajectory;

pub use database::{DatabaseSettings, MotionDatabase, MotionFrame};
pub use pose_feature::PoseFeature;
pub use search::{search, MatchWeights, MotionMatch};
pub use trajectory::{Trajectory, TrajectoryPoint, TrajectoryWeights};

#[cfg(test)]
mod tests;
