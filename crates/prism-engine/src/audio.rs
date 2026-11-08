//! ECS 音频集成。
//!
//! 提供 [`AudioSource`] ECS 组件和 [`sync_audio_sources`] 函数，
//! 每帧桥接 ECS 状态与 [`AudioEngine`]。
//!
//! # 状态：遗留代码路径
//!
//! 这里的 `AudioSource` **不是**场景组件（不进 `.scene.json` / RSCN），
//! 也持有内联 PCM，作者没法在编辑器里写。场景音频请用
//! `prism_engine::scene::components::AudioSource`（反射组件，按 `sound_path`
//! 加载），由 `prism-app::scene_audio` 驱动混音器（#508）。
//! [`sync_audio_sources`] 目前没有任何调用方；保留它是为了不破坏已有的
//! 外部用法，新代码不要接这条路径。

use prism_audio::{AudioData, AudioEngine, PlaybackHandle};
use prism_ecs::World;

/// 用于在实体上播放音频的 ECS 组件
///
/// 每帧 [`sync_audio_sources`] 读取每个带有此组件的实体，
/// 并相应驱动 [`AudioEngine`] 的方法：
///
/// * `playing && handle is None` → 开始播放
/// * `playing && handle 存在` → 更新音量
/// * `!playing && handle 存在` → 停止
pub struct AudioSource {
    /// 要播放的音频片段。设为 `None` 可保持组件存活但不带片段（用于预留槽位）。
    pub data: Option<AudioData>,

    /// Volume level, 0.0 (silent) to 1.0 (original), clamped.
    pub volume: f32,

    /// Whether this source should be playing. Set to `false` to stop.
    pub playing: bool,

    /// Whether the 片段 loops. 已接线 to the sampler's repeat 众数
    pub repeat: bool,

    /// Internal: the active playback handle, if any.
    pub(crate) handle: Option<PlaybackHandle>,
}

impl AudioSource {
    /// 创建一个新的 `AudioSource`，可供播放。
    pub fn new(data: AudioData) -> Self {
        Self {
            data: Some(data),
            volume: 1.0,
            playing: true,
            repeat: false,
            handle: None,
        }
    }

    /// Create a silent placeholder (no clip, not playing).
    pub fn silent() -> Self {
        Self {
            data: None,
            volume: 1.0,
            playing: false,
            repeat: false,
            handle: None,
        }
    }
}

/// Synchronise all [`AudioSource`] components in `world` with `engine`.
///
/// Must be called once per frame, **after** [`AudioEngine::update`], so that
/// the engine has already GC'd finished sounds before we decide whether to
/// re-start them.
pub fn sync_audio_sources(engine: &mut AudioEngine, world: &mut World) {
    for (_, src) in world.query_mut::<AudioSource>() {
        if src.playing {
            if let Some(ref data) = src.data {
                if src.handle.is_none() {
                    // 开始新的播放。
                    let handle = engine.play(data);
                    if handle.is_valid() {
                        engine.set_volume(&handle, src.volume);
                        src.handle = Some(handle);
                    }
                } else if let Some(ref handle) = src.handle {
                    // Update volume on the existing playback.
                    engine.set_volume(handle, src.volume);
                }
            }
        } else if let Some(handle) = src.handle.take() {
            // Was playing, now stopped.
            engine.stop(&handle);
        }
    }
}
