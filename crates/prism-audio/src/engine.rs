//! Audio engine — owns the Firewheel context and manages sound playback.

use std::num::NonZeroU32;
use std::time::Duration;

use firewheel::core::channel_config::{ChannelCount, NonZeroChannelCount};
use firewheel::core::diff::{Notify, ParamPath};
use firewheel::core::event::{NodeEventType, ParamData};
use firewheel::core::node::NodeID;
use firewheel::cpal::{CpalConfig, CpalOutputConfig, CpalStream};
use firewheel::nodes::sampler::*;
use firewheel::nodes::volume_pan::VolumePanNode;
use firewheel::*;

use crate::error::AudioError;

/// Decoded audio data ready for playback.
#[derive(Clone)]
pub struct AudioData {
    /// Interleaved f32 samples (`L,R,L,R` for stereo, for example).
    pub samples: Vec<f32>,
    /// Sample rate in Hz (for example 44100).
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo).
    pub channels: u16,
    /// Total duration.
    pub duration: Duration,
}

/// Handle to a playing sound.
///
/// Control playback through [`AudioEngine`] methods such as
/// `engine.stop(&handle)` and `engine.set_volume(&handle, 0.5)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackHandle(u64);

impl PlaybackHandle {
    /// Returns `false` when this handle came from a failed `play()` call
    /// (for example empty audio or a graph error); such a handle never refers
    /// to a real sound.
    pub fn is_valid(&self) -> bool {
        self.0 != 0
    }
}

/// Configuration for the audio engine.
#[derive(Clone)]
pub struct AudioConfig {
    /// Preferred audio device name. `None` selects the system default.
    pub device_name: Option<String>,
    /// Output sample rate. Defaults to 44100.
    pub sample_rate: u32,
    /// Output channel count. Defaults to 2 (stereo).
    pub channels: u16,
    /// Master volume in the range 0.0–1.0. Defaults to 1.0.
    pub master_volume: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device_name: None,
            sample_rate: 44100,
            channels: 2,
            master_volume: 1.0,
        }
    }
}

struct ActiveSound {
    handle: PlaybackHandle,
    node_id: NodeID,
    /// Volume/pan node on a stereo output; `None` on mono devices.
    ///
    /// Pan must update during playback (#508), so it cannot be baked into the
    /// sample data at `play()` time as it was previously — Firewheel's
    /// `VolumePanNode` provides the real-time pan instead.
    pan_node: Option<NodeID>,
    /// Last pan value pushed to `pan_node`, so events are not requeued every frame.
    pan: f32,
    /// Local copy used to call `sync_*_event()`.
    sampler: SamplerNode,
    /// Shared state of the sampler node, used to observe the true playback
    /// progress on the audio thread.
    ///
    /// The audio thread only writes to the shared channels inside
    /// `SamplerState`; the node itself exposes no completion event, so this is
    /// the only way to tell "finished" apart from "not started yet".
    state: SamplerState,
    /// Whether this sampler has actually been observed playing.
    ///
    /// A freshly created sampler reports `Stopped` until the audio thread has
    /// processed its first block and it becomes `Playing`; seeing `Stopped`
    /// for the first time therefore does not mean playback ended.
    seen_playing: bool,
}

/// Main audio engine.
///
/// ```ignore
/// let mut engine = AudioEngine::new(AudioConfig::default())?;
/// engine.update(); // call once per frame
///
/// let snd = engine.play(&my_audio);
/// engine.set_volume(&snd, 0.5);
/// ```
pub struct AudioEngine {
    ctx: FirewheelContext,
    /// Kept alive to keep the audio stream running; drop order matters.
    _stream: Option<CpalStream>,
    active: Vec<ActiveSound>,
    next_id: u64,
    master_volume: f32,
    channels: u16,
    /// Configuration used to open/rebuild the audio stream (kept across
    /// suspend and reused on resume).
    config: AudioConfig,
}

impl AudioEngine {
    /// Creates a new audio engine and starts the audio stream.
    ///
    /// If the audio device cannot be opened, a warning is logged but the
    /// engine still runs in silent mode; the game must not crash because of
    /// audio.
    pub fn new(config: AudioConfig) -> Result<Self, AudioError> {
        let channels = config.channels;

        let ctx = FirewheelContext::new(FirewheelConfig {
            num_graph_outputs: ChannelCount::new(channels as u32).unwrap_or(ChannelCount::STEREO),
            ..Default::default()
        });

        let mut engine = Self {
            ctx,
            _stream: None,
            active: Vec::new(),
            next_id: 1,
            master_volume: config.master_volume,
            channels,
            config,
        };

        engine.arm_stream();
        Ok(engine)
    }

    /// Tries to open the audio device and register the cpal callback.
    /// Failures only log a warning and leave the engine silent.
    fn arm_stream(&mut self) {
        match Self::try_start_stream(&mut self.ctx, &self.config) {
            Ok(stream) => {
                ::log::info!(
                    "Audio stream started ({} Hz, {} ch)",
                    self.config.sample_rate,
                    self.channels
                );
                self._stream = Some(stream);
            }
            Err(e) => {
                ::log::warn!("Audio stream failed to start, running silent: {e}");
                self._stream = None;
            }
        }
    }

    /// Suspends audio output: releases the audio device (drops the cpal
    /// stream) but keeps the Firewheel graph and all active playback nodes, so
    /// resuming continues playback instead of restarting it.
    ///
    /// Called by the host (`prism-app`) on platform background transitions
    /// (Android `onPause` / iOS backgrounding). Firewheel 0.12's
    /// [`CpalStream`] exposes no pause/play control, so dropping the stream and
    /// re-registering the callback on resume is the only way to suspend it —
    /// matching the platform semantics where the thread is only registered,
    /// never owned. No-op when there is no stream.
    pub fn suspend_stream(&mut self) {
        if self._stream.is_none() {
            return;
        }
        ::log::debug!("Audio stream suspended (device released; graph kept)");
        drop(self._stream.take());
    }

    /// Resumes audio output: if [`Self::suspend_stream`] released the stream,
    /// reopen the device with the pre-suspend configuration and re-register
    /// the callback; active nodes, volumes and playback positions are
    /// preserved. No-op while the stream is still active.
    pub fn resume_stream(&mut self) {
        if self._stream.is_some() {
            return;
        }
        self.arm_stream();
    }

    fn try_start_stream(
        ctx: &mut FirewheelContext,
        config: &AudioConfig,
    ) -> Result<CpalStream, AudioError> {
        let cpal_config = if let Some(ref name) = config.device_name {
            let host = firewheel::cpal::cpal::default_host();
            let host_enum = firewheel::cpal::HostEnumerator { host };
            let device_id = host_enum
                .output_devices()
                .into_iter()
                .find(|d| d.name.as_deref() == Some(name.as_str()))
                .ok_or_else(|| AudioError::DeviceNotFound(name.clone()))?
                .id;

            CpalConfig {
                output: CpalOutputConfig {
                    device_id: Some(device_id),
                    desired_sample_rate: Some(config.sample_rate),
                    ..Default::default()
                },
                input: None,
            }
        } else {
            CpalConfig {
                output: CpalOutputConfig {
                    desired_sample_rate: Some(config.sample_rate),
                    ..Default::default()
                },
                input: None,
            }
        };

        CpalStream::new(ctx, cpal_config).map_err(|e| AudioError::Init(e.to_string()))
    }

    /// Must be called once per frame, preferably at the end of the frame.
    ///
    /// Removes finished sounds and advances the Firewheel context. See
    /// [`should_keep_sound`] for the liveness rule: a freshly created sampler
    /// reports `Stopped` until the audio thread processes its first block, so
    /// "has been observed `Playing`" is what separates "not started" from
    /// "finished".
    pub fn update(&mut self) {
        let has_output = self._stream.is_some();
        let ctx = &mut self.ctx;
        self.active.retain_mut(|s| {
            let playback_state = s.state.playback_state();
            if should_keep_sound(playback_state, &mut s.seen_playing, has_output) {
                return true;
            }
            let _ = ctx.remove_node(s.node_id);
            if let Some(pan_id) = s.pan_node {
                let _ = ctx.remove_node(pan_id);
            }
            false
        });

        let _ = self.ctx.update();
    }

    /// Plays decoded audio and returns a playback control handle.
    ///
    /// `audio.samples` is interleaved by channel (`L,R,L,R` for stereo). When
    /// the length is not a multiple of the channel count, trailing samples
    /// that do not fill a whole frame are dropped — see [`deinterleave`].
    pub fn play(&mut self, audio: &AudioData) -> PlaybackHandle {
        let channels = audio.channels.max(1) as usize;
        let num_frames = audio.samples.len() / channels;
        if num_frames == 0 {
            ::log::error!("Cannot play empty audio");
            return PlaybackHandle(0);
        }

        let dropped = audio.samples.len() - num_frames * channels;
        if dropped > 0 {
            ::log::debug!(
                "Audio has {dropped} trailing sample(s) that do not fill a full {channels}-channel frame; dropping them"
            );
        }

        // Convert interleaved samples to planar layout for DecodedAudioF32.
        let planar = deinterleave(&audio.samples, channels);

        let sample_rate = match NonZeroU32::new(audio.sample_rate) {
            Some(r) => r,
            None => {
                ::log::error!("Invalid sample rate: {}", audio.sample_rate);
                return PlaybackHandle(0);
            }
        };

        // Build the sample resource as an ArcGc<dyn SampleResource>.
        let decoded_f32 = symphonium::DecodedAudioF32::new(planar, sample_rate, sample_rate);
        let decoded: symphonium::DecodedAudio = decoded_f32.into();
        let resource = firewheel::dyn_symphonium_resource(decoded);

        // Create the sampler node with its initial state.
        let sampler = SamplerNode {
            volume: Volume::from_percent(self.master_volume * 100.0),
            play: Notify::new(true),
            ..Default::default()
        };

        let sampler_config = SamplerConfig {
            channels: if self.channels >= 2 {
                NonZeroChannelCount::STEREO
            } else {
                NonZeroChannelCount::MONO
            },
            ..Default::default()
        };

        let node_id = match self.ctx.add_node(sampler, Some(sampler_config)) {
            Ok(nid) => nid,
            Err(e) => {
                ::log::error!("Failed to add sampler node to graph: {e}");
                return PlaybackHandle(0);
            }
        };

        // The sampler node registers its `SamplerState` as node state — the
        // only channel for observing playback completion, since Firewheel
        // emits no playback-finished event.
        let Some(state) = self.ctx.node_state::<SamplerState>(node_id).cloned() else {
            ::log::error!(
                "Sampler node {node_id:?} exposes no SamplerState; cannot track playback"
            );
            let _ = self.ctx.remove_node(node_id);
            return PlaybackHandle(0);
        };

        self.ctx
            .queue_event_for(node_id, SamplerNode::set_dyn_sample_event(resource));

        // Insert a real-time volume/pan node on stereo outputs (#508); mono
        // devices keep the direct chain.
        let pan_node = if self.channels >= 2 {
            let pan = VolumePanNode::from_pan(0.0);
            match self.ctx.add_node(pan, None) {
                Ok(id) => Some(id),
                Err(e) => {
                    ::log::error!("Failed to add pan node to graph: {e}");
                    let _ = self.ctx.remove_node(node_id);
                    return PlaybackHandle(0);
                }
            }
        } else {
            None
        };

        // Connect to the graph output. Mono connects (0, 0); stereo connects
        // the stereo pair.
        let out = self.ctx.graph_out_node_id();
        let res = match pan_node {
            Some(pan_id) => {
                // The sampler is always configured as STEREO on stereo devices
                // (`mono_to_stereo = true`).
                self.ctx
                    .connect(node_id, pan_id, &[(0, 0), (1, 1)], false)
                    .and_then(|_| self.ctx.connect_stereo(pan_id, out, false))
                    .map(|_| ())
            }
            None => self.ctx.connect(node_id, out, &[(0, 0)], false).map(|_| ()),
        };
        if let Err(e) = res {
            ::log::error!("Failed to connect sampler to graph output: {e}");
            let _ = self.ctx.remove_node(node_id);
            if let Some(pan_id) = pan_node {
                let _ = self.ctx.remove_node(pan_id);
            }
            return PlaybackHandle(0);
        }

        let id = PlaybackHandle(self.next_id);
        self.next_id += 1;

        self.active.push(ActiveSound {
            handle: id,
            node_id,
            pan_node,
            pan: 0.0,
            sampler,
            state,
            seen_playing: false,
        });

        id
    }

    /// Stops a sound immediately.
    pub fn stop(&mut self, handle: &PlaybackHandle) {
        if let Some(idx) = self.active.iter().position(|s| s.handle == *handle) {
            let sound = self.active.swap_remove(idx);
            let _ = self.ctx.remove_node(sound.node_id);
            if let Some(pan_id) = sound.pan_node {
                let _ = self.ctx.remove_node(pan_id);
            }
        }
    }

    /// Sets the volume of a playing sound (0.0 = silent, 1.0 = original).
    pub fn set_volume(&mut self, handle: &PlaybackHandle, volume: f32) {
        let Some(s) = self.active.iter_mut().find(|s| s.handle == *handle) else {
            return;
        };
        let vol = volume.clamp(0.0, 1.0);
        s.sampler.volume = Volume::from_percent(vol * 100.0);
        self.ctx
            .queue_event_for(s.node_id, s.sampler.sync_volume_event());
    }

    /// Sets the pan in real time: `-1` full left, `0` center, `+1` full right.
    ///
    /// On stereo devices this takes effect smoothly through the
    /// `VolumePanNode` in the playback chain, without replaying the clip; mono
    /// devices have no left/right channels, so the call is a no-op.
    /// Non-finite values are ignored.
    pub fn set_pan(&mut self, handle: &PlaybackHandle, pan: f32) {
        if !pan.is_finite() {
            return;
        }
        let pan = pan.clamp(-1.0, 1.0);
        let Some(s) = self.active.iter_mut().find(|s| s.handle == *handle) else {
            return;
        };
        s.pan = pan;
        let Some(pan_id) = s.pan_node else {
            return;
        };
        self.ctx.queue_event_for(
            pan_id,
            NodeEventType::Param {
                data: ParamData::F32(pan),
                // VolumePanNode field order: volume(0), pan(1), ...
                path: ParamPath::Single(1),
            },
        );
    }

    /// Pauses a sound (freezes its playback position).
    pub fn pause(&mut self, handle: &PlaybackHandle) {
        let Some(s) = self.active.iter_mut().find(|s| s.handle == *handle) else {
            return;
        };
        *s.sampler.play = false;
        self.ctx
            .queue_event_for(s.node_id, s.sampler.sync_play_event());
    }

    /// Resumes a paused sound.
    pub fn resume(&mut self, handle: &PlaybackHandle) {
        let Some(s) = self.active.iter_mut().find(|s| s.handle == *handle) else {
            return;
        };
        *s.sampler.play = true;
        self.ctx
            .queue_event_for(s.node_id, s.sampler.sync_play_event());
    }

    /// Checks whether a sound is currently playing.
    ///
    /// The sampler is driven by the audio thread: right after `play()`, and
    /// until the audio thread processes its first block, this returns `false`.
    /// Paused sounds also return `false`.
    pub fn is_playing(&self, handle: &PlaybackHandle) -> bool {
        let Some(s) = self.active.iter().find(|s| s.handle == *handle) else {
            return false;
        };
        s.state.playback_state() == PlaybackState::Playing
    }

    /// Stops all playing sounds immediately.
    pub fn stop_all(&mut self) {
        for s in self.active.drain(..) {
            let _ = self.ctx.remove_node(s.node_id);
        }
    }

    /// Sets the master volume (0.0–1.0). Affects sounds started by subsequent
    /// `play()` calls.
    pub fn set_master_volume(&mut self, volume: f32) {
        self.master_volume = volume.clamp(0.0, 1.0);
    }

    /// Current master volume.
    pub fn master_volume(&self) -> f32 {
        self.master_volume
    }

    /// Whether the audio stream is running.
    pub fn is_active(&self) -> bool {
        self._stream.is_some()
    }

    /// Number of currently active sounds.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop_all();
        // CpalStream must be dropped before FirewheelContext.
        let stream = self._stream.take();
        drop(stream);
    }
}

/// Decides whether an active sound should be kept by `update()`.
///
/// `playback_state` comes from the audio thread, `seen_playing` records
/// whether this sound has actually started playing, and `has_output` reports
/// whether an audio stream is currently consuming the graph (`false` when the
/// device could not be opened).
///
/// Rules:
/// - `Playing`: remember "started" and keep it;
/// - `Paused`: keep it so `resume()` can continue it;
/// - `Stopped`: keep it only when it never started and output really exists
///   (the audio thread may not have processed the first block yet); if it has
///   played before or there is no output at all, treat it as finished /
///   impossible to play and remove the node.
fn should_keep_sound(
    playback_state: PlaybackState,
    seen_playing: &mut bool,
    has_output: bool,
) -> bool {
    match playback_state {
        PlaybackState::Playing => {
            *seen_playing = true;
            true
        }
        PlaybackState::Paused => true,
        PlaybackState::Stopped => !*seen_playing && has_output,
    }
}

/// Splits interleaved samples into one planar buffer per channel.
///
/// `channels` of 0 is treated as mono. Every returned buffer has length
/// `samples.len() / channels`; trailing samples that do not fill a whole frame
/// are dropped, so `planar[ch][frame]` is always in bounds (no out-of-bounds
/// panic when `samples.len() % channels != 0`).
fn deinterleave(samples: &[f32], channels: usize) -> Vec<Vec<f32>> {
    let channels = channels.max(1);
    let num_frames = samples.len() / channels;
    let mut planar: Vec<Vec<f32>> = (0..channels)
        .map(|_| Vec::with_capacity(num_frames))
        .collect();
    for (i, &sample) in samples[..num_frames * channels].iter().enumerate() {
        planar[i % channels].push(sample);
    }
    planar
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_audio(channels: u16) -> AudioData {
        AudioData {
            samples: vec![0.1; 4410 * channels as usize],
            sample_rate: 44100,
            channels,
            duration: Duration::from_millis(100),
        }
    }

    #[test]
    fn test_play_stereo_default_config() {
        let mut engine = AudioEngine::new(AudioConfig::default()).expect("engine new");
        let audio = dummy_audio(2);
        let handle = engine.play(&audio);
        assert!(handle.is_valid());
        assert_eq!(engine.active_count(), 1);
        engine.stop(&handle);
        assert_eq!(engine.active_count(), 0);
    }

    #[test]
    fn test_play_mono_config() {
        let config = AudioConfig {
            channels: 1,
            ..Default::default()
        };
        let mut engine = AudioEngine::new(config).expect("engine new");
        let audio = dummy_audio(1);
        let handle = engine.play(&audio);
        assert!(handle.is_valid(), "mono play() must return a valid handle");
        assert_eq!(
            engine.active_count(),
            1,
            "mono play() must add an active sound"
        );
        engine.stop(&handle);
        assert_eq!(engine.active_count(), 0);
    }

    #[test]
    fn stereo_playback_has_a_realtime_pan_node() {
        let mut engine = AudioEngine::new(AudioConfig::default()).expect("engine new");
        let handle = engine.play(&dummy_audio(2));
        assert!(handle.is_valid());
        let sound = engine.active.first().expect("active sound");
        assert!(
            sound.pan_node.is_some(),
            "stereo output needs a VolumePanNode"
        );
        assert_eq!(sound.pan, 0.0);

        engine.set_pan(&handle, 2.5);
        assert_eq!(engine.active[0].pan, 1.0, "pan is clamped to [-1, 1]");
        engine.set_pan(&handle, f32::NAN);
        assert_eq!(engine.active[0].pan, 1.0, "non-finite pan is ignored");
        engine.stop(&handle);
        assert_eq!(engine.active_count(), 0);
    }

    #[test]
    fn mono_playback_has_no_pan_node() {
        let config = AudioConfig {
            channels: 1,
            ..Default::default()
        };
        let mut engine = AudioEngine::new(config).expect("engine new");
        let handle = engine.play(&dummy_audio(1));
        assert!(handle.is_valid());
        assert!(engine.active[0].pan_node.is_none());
        engine.set_pan(&handle, 1.0);
        assert_eq!(engine.active[0].pan, 1.0, "state still records the request");
        engine.stop(&handle);
    }

    #[test]
    fn test_play_empty_audio_returns_invalid_handle() {
        let mut engine = AudioEngine::new(AudioConfig::default()).expect("engine new");
        let audio = AudioData {
            samples: Vec::new(),
            sample_rate: 44100,
            channels: 2,
            duration: Duration::ZERO,
        };
        let handle = engine.play(&audio);
        assert!(!handle.is_valid());
        assert_eq!(engine.active_count(), 0);
    }

    #[test]
    fn test_play_invalid_sample_rate_returns_invalid_handle() {
        let mut engine = AudioEngine::new(AudioConfig::default()).expect("engine new");
        let audio = AudioData {
            samples: vec![0.1; 100],
            sample_rate: 0,
            channels: 2,
            duration: Duration::from_millis(10),
        };
        let handle = engine.play(&audio);
        assert!(!handle.is_valid());
        assert_eq!(engine.active_count(), 0);
    }

    #[test]
    fn sound_that_has_not_started_yet_is_kept_while_output_exists() {
        let mut seen_playing = false;
        assert!(
            should_keep_sound(PlaybackState::Stopped, &mut seen_playing, true),
            "a sampler reports Stopped until the audio thread processes the first block"
        );
        assert!(!seen_playing);
    }

    #[test]
    fn playing_sound_is_kept_and_marks_that_it_started() {
        let mut seen_playing = false;
        assert!(should_keep_sound(
            PlaybackState::Playing,
            &mut seen_playing,
            true
        ));
        assert!(seen_playing);
    }

    #[test]
    fn finished_sound_is_dropped_once_it_has_played() {
        let mut seen_playing = true;
        assert!(!should_keep_sound(
            PlaybackState::Stopped,
            &mut seen_playing,
            true
        ));
    }

    #[test]
    fn paused_sound_is_kept_so_resume_can_continue_it() {
        let mut never_started = false;
        assert!(should_keep_sound(
            PlaybackState::Paused,
            &mut never_started,
            true
        ));
        let mut played_before = true;
        assert!(should_keep_sound(
            PlaybackState::Paused,
            &mut played_before,
            true
        ));
    }

    #[test]
    fn stopped_sound_is_dropped_when_no_audio_output_exists() {
        let mut seen_playing = false;
        assert!(
            !should_keep_sound(PlaybackState::Stopped, &mut seen_playing, false),
            "without an audio stream the node would never start, so it must not leak"
        );
    }

    #[test]
    fn update_does_not_panic_and_follows_stream_availability() {
        // Without an audio device, play() must still create a trackable active
        // sound; update() can then drop the node that would never play. With a
        // device available the sound must be kept (the audio thread may not
        // have processed the first block yet).
        let mut engine = AudioEngine::new(AudioConfig::default()).expect("engine new");
        let handle = engine.play(&dummy_audio(2));
        assert!(handle.is_valid());
        assert_eq!(engine.active_count(), 1);

        let has_output = engine.is_active();
        engine.update();
        assert_eq!(
            engine.active_count(),
            usize::from(has_output),
            "a sound may only survive update() when it still has an audio stream"
        );
    }

    #[test]
    fn is_playing_is_false_for_unknown_handle() {
        let engine = AudioEngine::new(AudioConfig::default()).expect("engine new");
        assert!(!engine.is_playing(&PlaybackHandle(9999)));
    }

    #[test]
    fn deinterleave_splits_interleaved_stereo() {
        let planar = deinterleave(&[1.0, 2.0, 3.0, 4.0], 2);
        assert_eq!(planar, vec![vec![1.0, 3.0], vec![2.0, 4.0]]);
    }

    #[test]
    fn deinterleave_drops_a_partial_trailing_frame() {
        let planar = deinterleave(&[1.0, 2.0, 3.0], 2);
        assert_eq!(planar, vec![vec![1.0], vec![2.0]]);
        assert_eq!(planar[0].len(), 1, "one complete frame survives");
    }

    #[test]
    fn test_play_odd_sample_count_does_not_panic() {
        let mut engine = AudioEngine::new(AudioConfig::default()).expect("engine new");
        let audio = AudioData {
            samples: vec![0.1; 3],
            sample_rate: 44100,
            channels: 2,
            duration: Duration::from_millis(1),
        };
        let handle = engine.play(&audio);
        assert!(handle.is_valid(), "truncated audio still plays");
        assert_eq!(engine.active_count(), 1);
        engine.stop(&handle);
        assert_eq!(engine.active_count(), 0);
    }
}
