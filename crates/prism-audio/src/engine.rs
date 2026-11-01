//! 音频引擎 — 拥有 Firewheel 上下文并管理声音播放。

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

/// 解码后的音频数据，可供播放。
#[derive(Clone)]
pub struct AudioData {
    /// 交错排列的 f32 采样（例如立体声为 L,R,L,R）
    pub samples: Vec<f32>,
    /// 采样率，单位 Hz（例如 44100）。
    pub sample_rate: u32,
    /// 声道数（1 = 单声道，2 = 立体声）
    pub channels: u16,
    /// 总持续时间
    pub duration: Duration,
}

/// 控制正在播放的声音的句柄
///
/// 使用 [`AudioEngine`] 的方法来控制播放：
/// `engine.stop(&handle)`, `engine.set_volume(&handle, 0.5)` 等。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackHandle(u64);

impl PlaybackHandle {
    /// 当此句柄由失败的 `play()` 调用产生时返回 `false`
    ///（例如音频为空或图形错误，且永远不会指向真实的声音）
    pub fn is_valid(&self) -> bool {
        self.0 != 0
    }
}

// ---------------------------------------------------------------------------

/// 音频引擎的配置。
#[derive(Clone)]
pub struct AudioConfig {
    /// 首选音频设备名称。`None` = 系统默认。
    pub device_name: Option<String>,
    /// 输出采样率。默认 44100。
    pub sample_rate: u32,
    /// 输出声道数。默认 2 = 立体声。
    pub channels: u16,
    /// 主音量 0.0–1.0。默认 1.0。
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

// ---------------------------------------------------------------------------

struct ActiveSound {
    handle: PlaybackHandle,
    node_id: NodeID,
    /// 立体声输出上的音量 / 声像节点；单声道设备为 `None`。
    ///
    /// 声像必须在播放中实时更新（#508），所以它不能像旧实现那样在 `play()`
    /// 时烘焙进采样数据 —— 这里用 Firewheel 的 `VolumePanNode` 做实时 pan。
    pan_node: Option<NodeID>,
    /// 最近一次下发给 `pan_node` 的声像，避免每帧重复排队事件。
    pan: f32,
    /// 本地副本，用于调用 `sync_*_event()`。
    sampler: SamplerNode,
    /// 采样器节点的共享状态，用于观察音频线程真实的播放进度。
    ///
    /// 音频线程只写入 `SamplerState` 内部的共享通道，节点本身不暴露完成事件，
    /// 所以这是唯一能区分「已播完」与「还没开始播」的途径。
    state: SamplerState,
    /// 是否已经观察到该采样器真正播放过。
    ///
    /// 采样器刚创建时报告 `Stopped`，直到音频线程处理完第一个音频块才变为
    /// `Playing`；因此首次看到 `Stopped` 不代表播放结束。
    seen_playing: bool,
}

/// 主音频引擎。
///
/// ```ignore
/// let mut engine = AudioEngine::new(AudioConfig::default())?;
/// engine.update(); // 每帧调用
///
/// let snd = engine.play(&my_audio);
/// engine.set_volume(&snd, 0.5);
/// ```
pub struct AudioEngine {
    ctx: FirewheelContext,
    /// 保持存活以确保音频流持续；放置顺序很重要。
    _stream: Option<CpalStream>,
    active: Vec<ActiveSound>,
    next_id: u64,
    master_volume: f32,
    channels: u16,
    /// 打开/重建音频流所用的配置（挂起时保留，恢复时复用）。
    config: AudioConfig,
}

impl AudioEngine {
    /// 创建新的音频引擎并启动音频流。
    ///
    /// 如果音频设备无法打开，会记录警告，但引擎仍会运行（静默模式——游戏不应因音频而崩溃）。
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

        // 尝试激活 cpal 音频流（失败仅告警，静默运行——游戏不应因音频而崩溃）。
        engine.arm_stream();
        Ok(engine)
    }

    /// 尝试打开音频设备并注册 cpal 回调。失败仅告警并保持静默。
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

    /// 挂起音频输出：交还音频设备（丢弃 cpal 流），但保留 Firewheel 图与所有
    /// 活动播放节点，以便恢复后「续播」而非「重播」。
    ///
    /// 平台背景切换（Android `onPause` / iOS 退后台）时由宿主（`prism-app`）
    /// 调用。Firewheel 0.12 的 [`CpalStream`] 不暴露 pause/play 控制，挂起的
    /// 唯一机制是 drop 流、恢复时重新注册回调——正是「平台强制线程只注册、
    /// 不持有」的语义。流不存在时为空操作。
    pub fn suspend_stream(&mut self) {
        if self._stream.is_none() {
            return;
        }
        ::log::debug!("Audio stream suspended (device released; graph kept)");
        drop(self._stream.take());
    }

    /// 恢复音频输出：若流已被 [`Self::suspend_stream`] 释放，则以挂起前配置
    /// 重新打开设备并注册回调；活动节点/音量/播放位置原样保留。流仍活跃时为
    /// 空操作。
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

    /// 必须每帧调用一次（最好在帧结束时）。
    ///
    /// 移除已播放完成的声音并推进 Firewheel 上下文。存活判定见
    /// [`should_keep_sound`]：新创建的采样器在音频线程处理第一个块之前一直报告
    /// `Stopped`，所以必须用「是否观察到过 `Playing`」来区分「未开始」和「已结束」。
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

    /// 播放解码后的声音。返回播放控制句柄。
    ///
    /// `audio.samples` 按声道交错排列（立体声为 L,R,L,R）。长度不是声道数
    /// 整数倍时，尾部不足一整帧的样本会被丢弃——见 [`deinterleave`]。
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

        // 将交错格式转换为平面格式，用于 DecodedAudioF32。
        let planar = deinterleave(&audio.samples, channels);

        let sample_rate = match NonZeroU32::new(audio.sample_rate) {
            Some(r) => r,
            None => {
                ::log::error!("Invalid sample rate: {}", audio.sample_rate);
                return PlaybackHandle(0);
            }
        };

        // 将样本资源构建为 ArcGc<dyn SampleResource>。
        let decoded_f32 = symphonium::DecodedAudioF32::new(planar, sample_rate, sample_rate);
        let decoded: symphonium::DecodedAudio = decoded_f32.into();
        let resource = firewheel::dyn_symphonium_resource(decoded);

        // 创建带有初始状态的采样器节点。
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

        // 采样器节点把自己的 `SamplerState` 注册为 node state —— 这是观察
        // 播放完成的唯一通道（Firewheel 不提供播放结束事件）。
        let Some(state) = self.ctx.node_state::<SamplerState>(node_id).cloned() else {
            ::log::error!(
                "Sampler node {node_id:?} exposes no SamplerState; cannot track playback"
            );
            let _ = self.ctx.remove_node(node_id);
            return PlaybackHandle(0);
        };

        // 将样本资源加入队列。
        self.ctx
            .queue_event_for(node_id, SamplerNode::set_dyn_sample_event(resource));

        // 立体声输出上插一个实时音量 / 声像节点（#508）；单声道设备保持旧链路。
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

        // 连接到图形输出。单声道连接 (0, 0)，立体声连接立体声对。
        let out = self.ctx.graph_out_node_id();
        let res = match pan_node {
            Some(pan_id) => {
                // 采样器在立体声设备上始终配置为 STEREO（mono_to_stereo = true）。
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

    /// 立即停止声音。
    pub fn stop(&mut self, handle: &PlaybackHandle) {
        if let Some(idx) = self.active.iter().position(|s| s.handle == *handle) {
            let sound = self.active.swap_remove(idx);
            let _ = self.ctx.remove_node(sound.node_id);
            if let Some(pan_id) = sound.pan_node {
                let _ = self.ctx.remove_node(pan_id);
            }
        }
    }

    /// 设置正在播放的声音的音量（0.0 = 静音，1.0 = 原始音量）
    pub fn set_volume(&mut self, handle: &PlaybackHandle, volume: f32) {
        let Some(s) = self.active.iter_mut().find(|s| s.handle == *handle) else {
            return;
        };
        let vol = volume.clamp(0.0, 1.0);
        s.sampler.volume = Volume::from_percent(vol * 100.0);
        self.ctx
            .queue_event_for(s.node_id, s.sampler.sync_volume_event());
    }

    /// 实时设置声像：`-1` 全左、`0` 居中、`+1` 全右。
    ///
    /// 立体声设备上通过播放链里的 `VolumePanNode` 平滑生效，不需要重播片段；
    /// 单声道设备没有左右声道，调用是空操作。非有限值忽略。
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
                // VolumePanNode 字段顺序：volume(0), pan(1), ...
                path: ParamPath::Single(1),
            },
        );
    }

    /// 暂停声音（冻结播放位置）。
    pub fn pause(&mut self, handle: &PlaybackHandle) {
        let Some(s) = self.active.iter_mut().find(|s| s.handle == *handle) else {
            return;
        };
        *s.sampler.play = false;
        self.ctx
            .queue_event_for(s.node_id, s.sampler.sync_play_event());
    }

    /// 恢复暂停的声音
    pub fn resume(&mut self, handle: &PlaybackHandle) {
        let Some(s) = self.active.iter_mut().find(|s| s.handle == *handle) else {
            return;
        };
        *s.sampler.play = true;
        self.ctx
            .queue_event_for(s.node_id, s.sampler.sync_play_event());
    }

    /// 检查声音当前是否正在播放。
    ///
    /// 采样器由音频线程驱动：刚 `play()` 后、音频线程处理第一个音频块之前，
    /// 这里会返回 `false`。暂停中的声音返回 `false`。
    pub fn is_playing(&self, handle: &PlaybackHandle) -> bool {
        let Some(s) = self.active.iter().find(|s| s.handle == *handle) else {
            return false;
        };
        s.state.playback_state() == PlaybackState::Playing
    }

    /// 立即停止所有正在播放的声音。
    pub fn stop_all(&mut self) {
        for s in self.active.drain(..) {
            let _ = self.ctx.remove_node(s.node_id);
        }
    }

    /// 设置主音量（0.0–1.0）。影响下一步扩展播放的声音。
    pub fn set_master_volume(&mut self, volume: f32) {
        self.master_volume = volume.clamp(0.0, 1.0);
    }

    /// 当前主音量
    pub fn master_volume(&self) -> f32 {
        self.master_volume
    }

    /// 音频流是否正在运行。
    pub fn is_active(&self) -> bool {
        self._stream.is_some()
    }

    /// 当前活跃的声音数量。
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

/// 判断一个活动声音在 `update()` 时是否应继续保留。
///
/// `playback_state` 来自音频线程，`seen_playing` 记录本声音是否已经真正开始播放，
/// `has_output` 表示当前是否真的有音频流在消费图（设备打不开时为 `false`）。
///
/// 规则：
/// - `Playing`：记住「已开始」并保留；
/// - `Paused`：保留，`resume()` 才能续播；
/// - `Stopped`：只在「从未开始」且「确实有输出」时保留（音频线程可能还没处理到
///   第一个块）；已经播过或根本没有输出时视为结束/不可能播放，移除节点。
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

/// 把交错排列的样本拆成每个声道一条平面缓冲。
///
/// `channels` 为 0 时按单声道处理。返回的每条缓冲长度都是
/// `samples.len() / channels`；尾部不足一整帧的样本被丢弃，所以
/// `planar[ch][frame]` 的下标永远在范围内（`samples.len() % channels != 0`
/// 时不再越界 panic）。
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
        // 没有音频设备时，play() 仍要建立可跟踪的活动声音；随后 update() 才能清掉
        // 永远不会播放的节点。设备可用时则必须继续保留（音频线程可能还没跑第一块）。
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
