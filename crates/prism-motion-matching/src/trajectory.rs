//! Where the character is asked to go.

use glam::Vec3;

/// One future sample of the desired motion.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TrajectoryPoint {
    /// Position in model space (skeleton root space).
    pub position: Vec3,
    /// Velocity in model space, world units per second.
    pub velocity: Vec3,
    /// Unit facing direction; only the horizontal part is compared.
    pub facing: Vec3,
}

impl TrajectoryPoint {
    pub fn new(position: Vec3, velocity: Vec3, facing: Vec3) -> Self {
        Self {
            position,
            velocity,
            facing,
        }
    }
}

/// How much each feature contributes to [`Trajectory::distance_to`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrajectoryWeights {
    pub position: f32,
    pub velocity: f32,
    pub facing: f32,
}

impl Default for TrajectoryWeights {
    fn default() -> Self {
        Self {
            position: 1.0,
            velocity: 0.3,
            facing: 0.7,
        }
    }
}

/// A short series of future samples at fixed offsets.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Trajectory {
    times: Vec<f32>,
    points: Vec<TrajectoryPoint>,
}

impl Trajectory {
    /// Build from matching time / point lists.
    ///
    /// Returns `None` when the lengths differ, a time is not finite or not
    /// increasing, or no sample was given: comparing trajectories of different
    /// shapes would silently compare the wrong pairs.
    pub fn new(times: Vec<f32>, points: Vec<TrajectoryPoint>) -> Option<Self> {
        if times.is_empty() || times.len() != points.len() {
            return None;
        }
        let mut previous = f32::NEG_INFINITY;
        for time in &times {
            if !time.is_finite() || *time <= previous {
                return None;
            }
            previous = *time;
        }
        Some(Self { times, points })
    }

    /// Straight-line prediction: the character keeps `velocity` and `facing`.
    pub fn predicted(origin: Vec3, facing: Vec3, velocity: Vec3, times: &[f32]) -> Self {
        let facing = horizontal(facing).unwrap_or(Vec3::Z);
        let points = times
            .iter()
            .map(|time| TrajectoryPoint::new(origin + velocity * *time, velocity, facing))
            .collect();
        Self {
            times: times.to_vec(),
            points,
        }
    }

    pub fn times(&self) -> &[f32] {
        &self.times
    }

    pub fn points(&self) -> &[TrajectoryPoint] {
        &self.points
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Weighted mean squared difference; `None` when the shapes differ.
    pub fn distance_to(&self, other: &Trajectory, weights: &TrajectoryWeights) -> Option<f32> {
        if self.times.len() != other.times.len() || self.times.is_empty() {
            return None;
        }
        let mut total = 0.0;
        for (left, right) in self.points.iter().zip(&other.points) {
            total += (left.position - right.position).length_squared() * weights.position;
            total += (left.velocity - right.velocity).length_squared() * weights.velocity;
            let facing = horizontal(left.facing)
                .zip(horizontal(right.facing))
                .map(|(a, b)| 1.0 - a.dot(b).clamp(-1.0, 1.0))
                .unwrap_or(0.0);
            total += facing * weights.facing;
        }
        Some(total / self.points.len() as f32)
    }
}

/// Normalized horizontal part of `direction`; `None` when it is (nearly) vertical
/// or degenerate.
pub(crate) fn horizontal(direction: Vec3) -> Option<Vec3> {
    let flat = Vec3::new(direction.x, 0.0, direction.z);
    let length = flat.length();
    if !length.is_finite() || length <= 1e-5 {
        return None;
    }
    Some(flat / length)
}
