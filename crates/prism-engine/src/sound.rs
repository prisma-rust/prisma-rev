//! 一次性音效的请求队列（#987）。
//!
//! 项目代码（`GameWorld::play_sound`）只把请求排进这里，宿主每帧取走并真正
//! 播放：host 后端只借得到 `World`，拿不到音频设备；编辑器 Play 走 ABI 回调，
//! 不走这条路径。本模块不依赖 `prism-audio`，所以关掉 `audio` feature 也能编译。

/// 项目代码提交的一次性音效请求（#987）。
///
/// `GameWorld::play_sound` 只**记录**请求：混音器（`App`）在 tick 结束后取走并
/// 真正播放，因为 `GameWorld` 的 host 后端只借得到 `World`，拿不到音频设备。
/// 编辑器 Play 下走 ABI 回调，不走这条路径（模块和宿主是两个引擎实例）。
#[derive(Debug, Clone, PartialEq)]
pub enum SoundCommand {
    /// 播放 `path`；`id` 由 [`SoundCommands`] 分配，用于 `stop_sound`。
    Play {
        id: u32,
        path: String,
        volume: f32,
        repeat: bool,
    },
    /// 停止某个 id（未知 id 由宿主忽略）。
    Stop { id: u32 },
}

/// [`SoundCommand`] 的队列 resource。
#[derive(Debug, Default)]
pub struct SoundCommands {
    commands: Vec<SoundCommand>,
    next_id: u32,
}

impl SoundCommands {
    /// 排队一次播放，返回分配的音效 id（从 1 开始，`0` 保留为「无效」）。
    pub fn play(&mut self, path: impl Into<String>, volume: f32, repeat: bool) -> u32 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        self.commands.push(SoundCommand::Play {
            id,
            path: path.into(),
            volume: if volume.is_finite() {
                volume.clamp(0.0, 1.0)
            } else {
                1.0
            },
            repeat,
        });
        id
    }

    /// 排队停止某个 id。
    pub fn stop(&mut self, id: u32) {
        if id != 0 {
            self.commands.push(SoundCommand::Stop { id });
        }
    }

    /// 取走本帧排队的请求（宿主每帧调一次）。
    pub fn take(&mut self) -> Vec<SoundCommand> {
        std::mem::take(&mut self.commands)
    }

    /// 还有多少请求没被取走（测试 / 诊断用）。
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}
