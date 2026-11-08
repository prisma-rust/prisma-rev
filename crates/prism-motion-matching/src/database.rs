//! Clips sampled into searchable frames.

use glam::Vec3;
use prism_anim::clip::Clip;
use prism_anim::Skeleton;

use crate::pose_feature::PoseFeature;
use crate::trajectory::{horizontal, Trajectory, TrajectoryPoint};

/// How a clip is turned into frames.
#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseSettings {
    /// Seconds between sampled frames (also the pose velocity step).
    pub sample_interval: f32,
    /// Future offsets used to describe the trajectory at each frame.
    pub trajectory_times: Vec<f32>,
    /// Joints whose positions make up the pose feature.
    pub track_joints: Vec<usize>,
    /// Joint whose motion defines the trajectory (usually the root).
    pub root_joint: usize,
}

impl Default for DatabaseSettings {
    fn default() -> Self {
        Self {
            sample_interval: 1.0 / 30.0,
            trajectory_times: vec![0.35, 0.70, 1.10],
            track_joints: Vec::new(),
            root_joint: 0,
        }
    }
}

impl DatabaseSettings {
    /// Track every joint — the default for a small skeleton, and the sane
    /// starting point before a project picks a feature set.
    pub fn tracking_all(skeleton: &Skeleton) -> Self {
        Self {
            track_joints: (0..skeleton.joint_count()).collect(),
            ..Self::default()
        }
    }
}

/// One searchable frame of one clip.
#[derive(Debug, Clone, PartialEq)]
pub struct MotionFrame {
    /// Index into the clip list handed to [`MotionDatabase::build`].
    pub clip: usize,
    /// Frame time inside that clip.
    pub time: f32,
    pub pose: PoseFeature,
    pub trajectory: Trajectory,
}

/// Every frame of every clip.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MotionDatabase {
    frames: Vec<MotionFrame>,
}

impl MotionDatabase {
    /// Wrap frames that were produced elsewhere (tests, offline tools, caches).
    pub fn from_frames(frames: Vec<MotionFrame>) -> Self {
        Self { frames }
    }

    /// Sample `clips` into frames, one every `sample_interval` seconds.
    ///
    /// Poses are evaluated from the skeleton's rest pose, so the frames describe
    /// the clip alone. Trajectories are derived from the root joint's future
    /// motion inside the same clip; looping clips wrap, one-shot clips hold the
    /// last value past their end.
    pub fn build(skeleton: &Skeleton, clips: &[&Clip], settings: &DatabaseSettings) -> Self {
        let mut frames = Vec::new();
        if clips.is_empty() || skeleton.joint_count() == 0 {
            return Self { frames };
        }
        let step = if settings.sample_interval.is_finite() && settings.sample_interval > 0.0 {
            settings.sample_interval
        } else {
            DatabaseSettings::default().sample_interval
        };
        for (index, clip) in clips.iter().enumerate() {
            let duration = clip.duration.max(0.0);
            let count = if duration > 0.0 {
                ((duration / step).floor() as usize) + 1
            } else {
                1
            };
            let mut previous: Option<Vec<glam::Mat4>> = None;
            for frame in 0..count {
                let time = (frame as f32 * step).min(duration);
                let mut pose = skeleton.rest_pose();
                clip.sample_into(time, &mut pose);
                let world = skeleton.world_matrices(&pose);
                let feature = PoseFeature::from_matrices(
                    &world,
                    previous.as_deref(),
                    step,
                    &settings.track_joints,
                );
                previous = Some(world);
                let trajectory = frame_trajectory(skeleton, clip, time, duration, settings);
                frames.push(MotionFrame {
                    clip: index,
                    time,
                    pose: feature,
                    trajectory,
                });
            }
        }
        Self { frames }
    }

    pub fn frames(&self) -> &[MotionFrame] {
        &self.frames
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

fn frame_trajectory(
    skeleton: &Skeleton,
    clip: &Clip,
    time: f32,
    duration: f32,
    settings: &DatabaseSettings,
) -> Trajectory {
    let sample = |offset: f32| -> Vec3 {
        let at = time + offset;
        let at = if duration > 0.0 && clip.looped {
            at.rem_euclid(duration)
        } else {
            at.clamp(0.0, duration)
        };
        let mut pose = skeleton.rest_pose();
        clip.sample_into(at, &mut pose);
        pose.local(settings.root_joint)
            .map(|local| local.translation)
            .unwrap_or(Vec3::ZERO)
    };

    let origin = sample(0.0);
    let facing_sample = settings
        .trajectory_times
        .first()
        .copied()
        .filter(|first| *first > 0.0)
        .unwrap_or(0.1);
    let facing = horizontal(sample(facing_sample) - origin).unwrap_or(Vec3::Z);

    let mut points = Vec::with_capacity(settings.trajectory_times.len());
    let mut times = Vec::with_capacity(settings.trajectory_times.len());
    for offset in &settings.trajectory_times {
        if !offset.is_finite() || *offset <= 0.0 {
            continue;
        }
        let position = sample(*offset);
        let velocity = if duration > 0.0 {
            (sample(offset + settings.sample_interval) - position) / settings.sample_interval
        } else {
            Vec3::ZERO
        };
        times.push(*offset);
        points.push(TrajectoryPoint::new(position, velocity, facing));
    }
    Trajectory::new(times, points).unwrap_or_default()
}
