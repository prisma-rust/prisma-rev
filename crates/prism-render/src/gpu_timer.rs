//! Per-pass GPU 计时（`vk::QueryPool` + `TIMESTAMP`，#518 Phase 1）。
//!
//! 用法（[`crate::graph_renderer::GraphRenderer`] 已经这样做）：
//!
//! 1. `begin_frame(device, frame_index)`：清掉这一帧槽位的查询（上一帧的栅栏已经等过，
//!    所以此刻安全），顺便把上一轮留下的结果读成 [`GpuTimings`]；
//! 2. 录制时对每个 pass 调 `begin_pass` / `end_pass`（写两个时间戳）；
//! 3. 下一帧同一槽位再回到第 1 步 —— 结果延迟一帧读回，不会 stall。
//!
//! 设备不支持时间戳（`timestampPeriod == 0` 或图形队列没开 `TIMESTAMP`）时
//! [`GpuTimer::new`] 返回 `Ok(None)`，调用方就当作「没有 GPU 计时」，
//! 回退到 CPU 墙钟（`frame_time_ms`）。

use ash::vk;

/// 一个 pass 的 GPU 时间。
#[derive(Debug, Clone, PartialEq)]
pub struct PassTiming {
    pub name: String,
    pub ms: f32,
}

/// 一帧的 GPU 计时结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuTimings {
    /// 设备是否支持时间戳查询（false = 下面全是空）。
    pub supported: bool,
    /// 所有 pass 的 GPU 时间之和（ms）。
    pub total_ms: f32,
    /// 按录制顺序排列的 pass。
    pub passes: Vec<PassTiming>,
}

impl GpuTimings {
    pub fn is_empty(&self) -> bool {
        self.passes.is_empty()
    }

    /// 某个 pass 的时间（按名字查，面板显示用）。
    pub fn pass_ms(&self, name: &str) -> Option<f32> {
        self.passes
            .iter()
            .find(|pass| pass.name == name)
            .map(|pass| pass.ms)
    }
}

/// 默认保留多少帧的 GPU 总时间（≈2 秒 @60fps）。
pub const DEFAULT_HISTORY_SAMPLES: usize = 120;

/// GPU 总时间的环形缓冲（#518 Phase 2 历史曲线）。
///
/// 只保存**已读回**的帧；设备不支持时间戳时调用方不会 push，所以空缓冲
/// 本身就是「没有 GPU 计时」的信号。非有限 / 负数样本直接丢弃，避免把
/// 未初始化或异常的时间戳画进面板。
#[derive(Debug, Clone)]
pub struct GpuHistory {
    capacity: usize,
    samples: std::collections::VecDeque<f32>,
}

impl GpuHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            samples: std::collections::VecDeque::new(),
        }
    }

    /// 追加一帧的总 GPU 时间（ms）；超过容量时丢最旧的一帧。
    pub fn push(&mut self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
    }

    /// 从旧到新迭代。
    pub fn samples(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
        self.samples.iter().copied()
    }

    pub fn to_vec(&self) -> Vec<f32> {
        self.samples.iter().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for GpuHistory {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_SAMPLES)
    }
}

/// 每个帧槽位能记录的 pass 上限；超过的部分不计时（不会 panic）。
pub const MAX_PASSES_PER_FRAME: usize = 24;

/// 时间戳查询池。
#[derive(Debug)]
pub struct GpuTimer {
    pool: vk::QueryPool,
    /// `timestampPeriod`（纳秒/刻度）。
    period_ns: f32,
    slots: usize,
    /// 每帧阶段顺序记录、该帧结束时读取的 pass 名字。
    names: Vec<Vec<String>>,
    /// 每帧记录的 pass 数。
    counts: Vec<usize>,
    /// 该槽位是否需要在本帧录制时先 `cmd_reset_query_pool`。
    ///
    /// 设备级 `reset_query_pool` 要 Vulkan 1.2（`hostQueryReset`），所以清理由命令
    /// 缓冲区里的 `cmd_reset_query_pool` 完成 —— 这条在所有目标上都可用。
    needs_reset: Vec<bool>,
    /// 最近一次解析出来的结果（每帧槽位一份）。
    last: Vec<GpuTimings>,
}

impl GpuTimer {
    /// 创建计时器；设备不支持时间戳时返回 `Ok(None)`。
    ///
    /// `slots` = frames in flight（每条时间线一个独立区间，避免互相覆盖）。
    pub fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical: vk::PhysicalDevice,
        slots: usize,
    ) -> anyhow::Result<Option<Self>> {
        let slots = slots.max(1);
        let properties = unsafe { instance.get_physical_device_properties(physical) };
        let period_ns = properties.limits.timestamp_period;
        if period_ns <= 0.0 {
            log::info!("GPU timings disabled: timestampPeriod is 0");
            return Ok(None);
        }
        let timestamp_bits =
            unsafe { instance.get_physical_device_queue_family_properties(physical) }
                .iter()
                .map(|family| family.timestamp_valid_bits)
                .max()
                .unwrap_or(0);
        if timestamp_bits == 0 {
            log::info!("GPU timings disabled: no queue family supports timestamps");
            return Ok(None);
        }

        let query_count = (slots * MAX_PASSES_PER_FRAME * 2) as u32;
        let create_info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(query_count);
        let pool = match unsafe { device.create_query_pool(&create_info, None) } {
            Ok(pool) => pool,
            Err(error) => {
                log::warn!("GPU timings disabled: query pool creation failed ({error})");
                return Ok(None);
            }
        };

        Ok(Some(Self {
            pool,
            period_ns,
            slots,
            names: vec![Vec::new(); slots],
            counts: vec![0; slots],
            needs_reset: vec![true; slots],
            last: vec![GpuTimings::default(); slots],
        }))
    }

    /// 销毁查询池。必须由拥有者显式调用（`GraphRenderer` 的 `Drop`）。
    pub fn destroy(&mut self, device: &ash::Device) {
        if self.pool != vk::QueryPool::null() {
            unsafe { device.destroy_query_pool(self.pool, None) };
            self.pool = vk::QueryPool::null();
        }
    }

    /// 最近一次解析出的结果（某个帧槽位）。
    pub fn timings(&self, frame_index: u32) -> &GpuTimings {
        let slot = (frame_index as usize) % self.slots;
        &self.last[slot]
    }

    /// 开始一帧：读回上一轮的结果，然后清掉这个槽位的时间戳。
    ///
    /// 调用时机必须在「该帧槽位的栅栏已被等待」之后（`begin_frame` 里就是这样）。
    pub fn begin_frame(&mut self, device: &ash::Device, frame_index: u32) {
        let slot = (frame_index as usize) % self.slots;
        let count = self.counts[slot];
        if count > 0 {
            self.last[slot] = self.read_results(device, slot, count);
        } else {
            self.last[slot] = GpuTimings::default();
        }
        self.names[slot].clear();
        self.counts[slot] = 0;
        self.needs_reset[slot] = true;
    }

    /// 记录一个 pass 的起始时间戳。超过每帧上限的 pass 会被忽略（返回 `false`）。
    pub fn begin_pass(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        frame_index: u32,
        name: &str,
    ) -> bool {
        let slot = (frame_index as usize) % self.slots;
        let index = self.counts[slot];
        if index >= MAX_PASSES_PER_FRAME {
            return false;
        }
        let base = self.slot_base(slot);
        if self.needs_reset[slot] {
            // 上一轮的结果已经在 `begin_frame` 里读走了，这里才清。
            unsafe {
                device.cmd_reset_query_pool(
                    cmd,
                    self.pool,
                    base,
                    (MAX_PASSES_PER_FRAME * 2) as u32,
                );
            }
            self.needs_reset[slot] = false;
        }
        let query = base + (index * 2) as u32;
        unsafe {
            device.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, self.pool, query);
        }
        self.names[slot].push(name.to_string());
        self.counts[slot] = index + 1;
        true
    }

    /// 记录上一个 pass 的结束时间戳。
    pub fn end_pass(&mut self, device: &ash::Device, cmd: vk::CommandBuffer, frame_index: u32) {
        let slot = (frame_index as usize) % self.slots;
        let index = self.counts[slot];
        if index == 0 {
            return;
        }
        let query = self.slot_base(slot) + ((index - 1) * 2 + 1) as u32;
        unsafe {
            device.cmd_write_timestamp(
                cmd,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.pool,
                query,
            );
        }
    }

    fn slot_base(&self, slot: usize) -> u32 {
        (slot * MAX_PASSES_PER_FRAME * 2) as u32
    }

    fn read_results(&self, device: &ash::Device, slot: usize, count: usize) -> GpuTimings {
        let base = self.slot_base(slot);
        let mut data = vec![0u64; count * 2];
        // 不用 `WITH_AVAILABILITY`：调用方保证这个槽位的栅栏已经等过，
        // 所以结果一定写好；带 avail 位时返回值会变成 (avail, value) 交错的布局。
        let result = unsafe {
            device.get_query_pool_results(self.pool, base, &mut data, vk::QueryResultFlags::TYPE_64)
        };
        if let Err(error) = result {
            log::debug!("GPU timing readback skipped: {error}");
            return GpuTimings {
                supported: true,
                ..Default::default()
            };
        }

        let names = &self.names[slot];
        let mut passes = Vec::with_capacity(count.min(names.len()));
        let mut total = 0.0f32;
        for index in 0..count.min(names.len()) {
            let begin = data[index * 2];
            let end = data[index * 2 + 1];
            // 未写完 / 乱序的一对跳过，不猜结果。
            if end <= begin {
                continue;
            }
            let ticks = (end - begin) as f32;
            let ms = ticks * self.period_ns / 1_000_000.0;
            total += ms;
            passes.push(PassTiming {
                name: names[index].clone(),
                ms,
            });
        }

        GpuTimings {
            supported: true,
            total_ms: total,
            passes,
        }
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn history_keeps_the_newest_samples_up_to_capacity() {
        let mut history = GpuHistory::new(3);
        for ms in [1.0, 2.0, 3.0, 4.0] {
            history.push(ms);
        }
        assert_eq!(history.to_vec(), vec![2.0, 3.0, 4.0]);
        assert_eq!(history.len(), 3);
        assert_eq!(history.capacity(), 3);
    }

    #[test]
    fn history_ignores_invalid_samples_and_zero_capacity_is_clamped() {
        let mut history = GpuHistory::new(0);
        assert_eq!(history.capacity(), 1);
        history.push(f32::NAN);
        history.push(f32::INFINITY);
        history.push(-1.0);
        assert!(history.is_empty());
        history.push(0.0);
        history.push(1.5);
        assert_eq!(history.to_vec(), vec![1.5]);
        assert_eq!(history.samples().sum::<f32>(), 1.5);
    }
}
