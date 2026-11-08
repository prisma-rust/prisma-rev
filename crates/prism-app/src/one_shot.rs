//! 一次性音效（#987）：把 `GameWorld::play_sound` 排队的请求接到混音器上。
//!
//! 分层和场景音频一致：项目代码只**排队**（`prism_engine::sound::SoundCommands`），
//! 本模块在 `App` 的 tick 结束后真正解码 + 播放。这样 `GameWorld` 的 host 后端
//! 不需要碰音频设备，插件（编辑器 Play）则走 ABI 回调，两条路互不影响。
//!
//! 代价是解码发生在请求的那一帧（同步读文件）。短音效（水花 / 上钩提示）没问题；
//! 长音乐请用 `AudioSource` 组件，那条路是异步解码 + 按路径缓存的。

use std::collections::HashMap;
use std::path::PathBuf;

use prism_asset::runtime::ResourceManager;
use prism_audio::{AudioData, AudioEngine, PlaybackHandle};
use prism_engine::ecs::World;
use prism_engine::sound::{SoundCommand, SoundCommands};

/// 正在播放的一次性音效。
pub struct OneShotSound {
    handle: PlaybackHandle,
    /// 保留片段用于 `repeat`（`AudioEngine` 没有循环标志，播完重放）。
    clip: AudioData,
    volume: f32,
    repeat: bool,
}

impl OneShotSound {
    pub fn is_repeating(&self) -> bool {
        self.repeat
    }
}

/// 解析音效路径：优先 `assets/` 下的松散文件，其次 `.pak` 里的同名资源。
///
/// 发布包只有 `.pak`，所以 pak 这条分支不是可选项：`build-scenes` 会把
/// `assets/audio/**` 整个目录打进包里（和 `sprites/` 一样，因为运行时按路径取）。
fn load_clip(relative: &str, pack: &mut ResourceManager) -> Option<AudioData> {
    if let Some(path) = prism_engine::asset_paths::asset_path(relative) {
        if path.is_file() {
            return match prism_audio::decoder::decode_file(&path) {
                Ok(clip) => Some(clip),
                Err(error) => {
                    log::warn!("play_sound: decode '{relative}' failed: {error}");
                    None
                }
            };
        }
    }
    let id = pack.id_by_path(relative)?;
    let bytes = pack.load_with_deps_raw(id).ok()?;
    match prism_audio::decoder::decode_auto(&bytes) {
        Ok(clip) => Some(clip),
        Err(error) => {
            log::warn!("play_sound: decode '{relative}' from the package failed: {error}");
            None
        }
    }
}

/// 取走并执行本帧排队的一次性音效请求。
///
/// `audio` 为 `None`（没声卡 / 没开 `audio` feature）时请求会被丢弃，只 warn 一次。
pub fn drain(
    world: &mut World,
    audio: Option<&mut AudioEngine>,
    sounds: &mut HashMap<u32, OneShotSound>,
    pack: &mut ResourceManager,
) {
    let Some(commands) = world.get_resource_mut::<SoundCommands>() else {
        return;
    };
    let commands: Vec<SoundCommand> = commands.take();
    if commands.is_empty() && sounds.is_empty() {
        return;
    }

    let Some(audio) = audio else {
        if !commands.is_empty() {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                log::warn!("play_sound: no audio device is available; sound requests are dropped");
            });
        }
        return;
    };

    for command in commands {
        match command {
            SoundCommand::Play {
                id,
                path,
                volume,
                repeat,
            } => {
                let Some(clip) = load_clip(&path, pack) else {
                    log::warn!("play_sound: '{path}' not found in assets/ or the package");
                    continue;
                };
                let handle = audio.play(&clip);
                if !handle.is_valid() {
                    log::warn!("play_sound: mixer refused '{path}'");
                    continue;
                }
                audio.set_volume(&handle, volume);
                // 同一个 id 重播时先停掉旧的，避免句柄泄漏。
                if let Some(old) = sounds.remove(&id) {
                    audio.stop(&old.handle);
                }
                log::debug!(
                    "play_sound: '{path}' started as id {id} ({} frames, volume {volume:.2}, repeat {repeat})",
                    clip.samples.len() / clip.channels.max(1) as usize
                );
                sounds.insert(
                    id,
                    OneShotSound {
                        handle,
                        clip,
                        volume,
                        repeat,
                    },
                );
            }
            SoundCommand::Stop { id } => {
                if let Some(sound) = sounds.remove(&id) {
                    audio.stop(&sound.handle);
                }
            }
        }
    }

    // 循环音效：混音器没有循环标志，播完就重放。
    for sound in sounds.values_mut() {
        if !sound.repeat || audio.is_playing(&sound.handle) {
            continue;
        }
        let handle = audio.play(&sound.clip);
        if handle.is_valid() {
            audio.set_volume(&handle, sound.volume);
            sound.handle = handle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_clip_is_skipped_without_panicking() {
        let mut world = World::new();
        let mut commands = SoundCommands::default();
        let id = commands.play("audio/does-not-exist.ogg", 1.0, false);
        world.insert_resource(commands);
        let mut sounds = HashMap::new();
        // 没有混音器：请求被丢弃，队列必须被清空（否则每帧重试）。
        let mut pack = ResourceManager::new();
        drain(&mut world, None, &mut sounds, &mut pack);
        assert!(sounds.is_empty());
        assert!(world
            .get_resource::<SoundCommands>()
            .map(SoundCommands::is_empty)
            .unwrap_or(false));
        assert_ne!(id, 0);
    }
}
