//! A compact signature of a pose.

use glam::{Mat4, Vec3};

/// Tracked joint positions and velocities in skeleton-root space.
///
/// Velocities are finite differences against the previous frame and are zero on
/// the first frame. Joint order is whatever the caller passed in; two features
/// are only comparable when they were built with the same joint list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PoseFeature {
    positions: Vec<Vec3>,
    velocities: Vec<Vec3>,
}

impl PoseFeature {
    /// Build from matching position / velocity lists.
    pub fn new(positions: Vec<Vec3>, velocities: Vec<Vec3>) -> Option<Self> {
        (positions.len() == velocities.len()).then_some(Self {
            positions,
            velocities,
        })
    }

    /// Sample `joints` out of a bone-matrix list, differencing against the
    /// previous frame for the velocities.
    ///
    /// `previous` must have the same length as `world`; out-of-range joints are
    /// skipped instead of panicking, which keeps LOD-thinned poses usable.
    pub fn from_matrices(
        world: &[Mat4],
        previous: Option<&[Mat4]>,
        dt: f32,
        joints: &[usize],
    ) -> Self {
        let mut positions = Vec::with_capacity(joints.len());
        let mut velocities = Vec::with_capacity(joints.len());
        let step = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        for joint in joints {
            let Some(matrix) = world.get(*joint) else {
                continue;
            };
            let position = matrix.w_axis.truncate();
            positions.push(position);
            let velocity = match previous.and_then(|prev| prev.get(*joint)) {
                Some(prev) if step > 0.0 => (position - prev.w_axis.truncate()) / step,
                _ => Vec3::ZERO,
            };
            velocities.push(velocity);
        }
        Self {
            positions,
            velocities,
        }
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    pub fn positions(&self) -> &[Vec3] {
        &self.positions
    }

    pub fn velocities(&self) -> &[Vec3] {
        &self.velocities
    }

    /// Mean squared joint distance; `None` when the joint lists differ.
    pub fn distance_to(&self, other: &PoseFeature) -> Option<f32> {
        if self.positions.len() != other.positions.len() || self.positions.is_empty() {
            return None;
        }
        let mut total = 0.0;
        for index in 0..self.positions.len() {
            total += (self.positions[index] - other.positions[index]).length_squared();
            total += (self.velocities[index] - other.velocities[index]).length_squared();
        }
        Some(total / self.positions.len() as f32)
    }
}
