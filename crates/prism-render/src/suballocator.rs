//! 显存子分配器与暂存传输 Arena。
//!
//! 提供基于 Chunk 的 Vulkan 显存子分配管理（#1030），
//! 减少直接调用 `vkAllocateMemory` 的次数并防止达到驱动 `maxMemoryAllocationCount` 上限。
//!
//! 核心结构：
//! - [`MemorySuballocator`]: 多内存类型索引的大块 `VkDeviceMemory` 管理器，
//!   执行对齐子分配与释放时相邻区间合并（free-list coalescing）。
//! - [`StagingArena`]: 批量上传（[`crate::batch::BatchUploader`]）专用的分块暂存区，
//!   将数百次逐资源 staging `vkAllocateMemory` 合并为大块暂存缓冲分配，并在提交前显式 unmap
//!   以保证跨驱动（包括 llvmpipe）正确同步。
//! - [`SuballocatorStats`]: 保留显存、活跃显存、Chunk 数量与分配数统计。

use anyhow::{Context as _, Result};
use ash::vk;
use serde::Serialize;

use crate::buffer::{create_buffer, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;

/// 将数值向上按 `alignment` 对齐。
pub fn align_up(value: vk::DeviceSize, alignment: vk::DeviceSize) -> vk::DeviceSize {
    if alignment <= 1 {
        return value;
    }
    let remainder = value % alignment;
    if remainder == 0 {
        value
    } else {
        value + (alignment - remainder)
    }
}

/// 显存中的连续空闲区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRange {
    pub offset: vk::DeviceSize,
    pub size: vk::DeviceSize,
}

/// 显存子分配句柄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Suballocation {
    pub memory: vk::DeviceMemory,
    pub offset: vk::DeviceSize,
    pub size: vk::DeviceSize,
    pub chunk_id: usize,
    pub memory_type_index: u32,
}

/// 显存子分配器运行状态统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuballocatorStats {
    pub total_reserved_bytes: u64,
    pub active_used_bytes: u64,
    pub chunk_count: usize,
    pub allocation_count: usize,
}

/// 拥有一整块 `VkDeviceMemory` 的 Chunk，维护内部空闲区间列表。
pub struct MemoryChunk {
    pub memory: vk::DeviceMemory,
    pub size: vk::DeviceSize,
    pub memory_type_index: u32,
    free_ranges: Vec<MemoryRange>,
    active_allocations: usize,
    allocated_bytes: vk::DeviceSize,
}

impl MemoryChunk {
    /// 分配并初始化一个新的显存 Chunk。
    pub fn new(device: &ash::Device, size: vk::DeviceSize, memory_type_index: u32) -> Result<Self> {
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(memory_type_index);
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }
            .context("MemoryChunk: allocate device memory")?;
        Ok(Self {
            memory,
            size,
            memory_type_index,
            free_ranges: vec![MemoryRange { offset: 0, size }],
            active_allocations: 0,
            allocated_bytes: 0,
        })
    }

    /// 构造用于纯算法测试的 Mock Chunk（无 Vulkan 设备调用）。
    #[cfg(test)]
    pub(crate) fn new_mock(size: vk::DeviceSize, memory_type_index: u32) -> Self {
        Self {
            memory: vk::DeviceMemory::null(),
            size,
            memory_type_index,
            free_ranges: vec![MemoryRange { offset: 0, size }],
            active_allocations: 0,
            allocated_bytes: 0,
        }
    }

    /// 在当前 Chunk 中分配指定大小和对齐要求的区间。
    pub fn allocate(
        &mut self,
        size: vk::DeviceSize,
        alignment: vk::DeviceSize,
    ) -> Option<vk::DeviceSize> {
        if size == 0 {
            return Some(0);
        }
        let alignment = alignment.max(1);
        for i in 0..self.free_ranges.len() {
            let range = self.free_ranges[i];
            let aligned_offset = align_up(range.offset, alignment);
            let end_offset = aligned_offset.checked_add(size)?;
            let range_end = range.offset.checked_add(range.size)?;

            if end_offset <= range_end {
                // 命中空闲区间：移除原区间并插入分割后的剩余部分
                self.free_ranges.remove(i);
                let mut insert_idx = i;
                if aligned_offset > range.offset {
                    self.free_ranges.insert(
                        insert_idx,
                        MemoryRange {
                            offset: range.offset,
                            size: aligned_offset - range.offset,
                        },
                    );
                    insert_idx += 1;
                }
                if end_offset < range_end {
                    self.free_ranges.insert(
                        insert_idx,
                        MemoryRange {
                            offset: end_offset,
                            size: range_end - end_offset,
                        },
                    );
                }
                self.active_allocations += 1;
                self.allocated_bytes += size;
                return Some(aligned_offset);
            }
        }
        None
    }

    /// 释放区间并将相邻的空闲区间自动合并。
    pub fn free(&mut self, offset: vk::DeviceSize, size: vk::DeviceSize) {
        if size == 0 {
            return;
        }
        let free_range = MemoryRange { offset, size };
        let pos = match self.free_ranges.binary_search_by_key(&offset, |r| r.offset) {
            Ok(idx) => idx,
            Err(idx) => idx,
        };
        self.free_ranges.insert(pos, free_range);
        self.active_allocations = self.active_allocations.saturating_sub(1);
        self.allocated_bytes = self.allocated_bytes.saturating_sub(size);

        // 合并相邻区间
        let mut i = 0;
        while i + 1 < self.free_ranges.len() {
            let current = self.free_ranges[i];
            let next = self.free_ranges[i + 1];
            if current.offset + current.size == next.offset {
                self.free_ranges[i].size += next.size;
                self.free_ranges.remove(i + 1);
            } else {
                i += 1;
            }
        }
    }

    /// 当前 Chunk 是否没有任何活跃分配。
    pub fn is_empty(&self) -> bool {
        self.active_allocations == 0
    }

    /// 释放底层的 Vulkan DeviceMemory。
    pub fn destroy(&mut self, device: &ash::Device) {
        if self.memory != vk::DeviceMemory::null() {
            unsafe {
                device.free_memory(self.memory, None);
            }
            self.memory = vk::DeviceMemory::null();
        }
        self.free_ranges.clear();
        self.active_allocations = 0;
        self.allocated_bytes = 0;
    }
}

/// Vulkan 显存子分配器。
pub struct MemorySuballocator {
    chunks: Vec<Option<MemoryChunk>>,
    default_chunk_size: vk::DeviceSize,
}

impl MemorySuballocator {
    /// 默认 64 MiB 单 Chunk 尺寸。
    pub const DEFAULT_CHUNK_SIZE: vk::DeviceSize = 64 * 1024 * 1024;

    pub fn new(default_chunk_size: vk::DeviceSize) -> Self {
        Self {
            chunks: Vec::new(),
            default_chunk_size: default_chunk_size.max(1024 * 1024),
        }
    }

    /// 分配一段指定大小、对齐与内存类型的显存。
    pub fn allocate(
        &mut self,
        device: &ash::Device,
        size: vk::DeviceSize,
        alignment: vk::DeviceSize,
        memory_type_index: u32,
    ) -> Result<Suballocation> {
        // 先尝试在已有同内存类型的 Chunk 中分配
        for (chunk_id, chunk_opt) in self.chunks.iter_mut().enumerate() {
            if let Some(chunk) = chunk_opt {
                if chunk.memory_type_index == memory_type_index {
                    if let Some(offset) = chunk.allocate(size, alignment) {
                        return Ok(Suballocation {
                            memory: chunk.memory,
                            offset,
                            size,
                            chunk_id,
                            memory_type_index,
                        });
                    }
                }
            }
        }

        // 无法容纳时分配新 Chunk
        let chunk_size = self
            .default_chunk_size
            .max(align_up(size, alignment).next_power_of_two());
        let mut new_chunk = MemoryChunk::new(device, chunk_size, memory_type_index)?;
        let offset = new_chunk
            .allocate(size, alignment)
            .ok_or_else(|| anyhow::anyhow!("failed to allocate from newly created memory chunk"))?;
        let memory = new_chunk.memory;

        let chunk_id = if let Some(empty_idx) = self.chunks.iter().position(|c| c.is_none()) {
            self.chunks[empty_idx] = Some(new_chunk);
            empty_idx
        } else {
            let id = self.chunks.len();
            self.chunks.push(Some(new_chunk));
            id
        };

        Ok(Suballocation {
            memory,
            offset,
            size,
            chunk_id,
            memory_type_index,
        })
    }

    /// 释放子分配区。若同内存类型下已有多个 Chunk 且当前 Chunk 空闲，则销毁释放回驱动。
    pub fn free(&mut self, device: &ash::Device, suballoc: &Suballocation) {
        if let Some(Some(chunk)) = self.chunks.get_mut(suballoc.chunk_id) {
            chunk.free(suballoc.offset, suballoc.size);
            if chunk.is_empty() {
                let same_type_count = self
                    .chunks
                    .iter()
                    .filter_map(|c| c.as_ref())
                    .filter(|c| c.memory_type_index == suballoc.memory_type_index)
                    .count();
                if same_type_count > 1 {
                    let mut removed = self.chunks[suballoc.chunk_id].take().unwrap();
                    removed.destroy(device);
                }
            }
        }
    }

    /// 查询当前子分配统计信息。
    pub fn stats(&self) -> SuballocatorStats {
        let mut total_reserved_bytes = 0;
        let mut active_used_bytes = 0;
        let mut chunk_count = 0;
        let mut allocation_count = 0;

        for chunk in self.chunks.iter().flatten() {
            total_reserved_bytes += chunk.size;
            active_used_bytes += chunk.allocated_bytes;
            chunk_count += 1;
            allocation_count += chunk.active_allocations;
        }

        SuballocatorStats {
            total_reserved_bytes,
            active_used_bytes,
            chunk_count,
            allocation_count,
        }
    }

    /// 销毁所有 Chunk 并释放底层 Vulkan 显存。
    pub fn destroy(&mut self, device: &ash::Device) {
        for chunk_opt in self.chunks.iter_mut() {
            if let Some(mut chunk) = chunk_opt.take() {
                chunk.destroy(device);
            }
        }
        self.chunks.clear();
    }
}

// ---------------------------------------------------------------------------
// Staging Arena
// ---------------------------------------------------------------------------

/// 单个用于暂存上传的 Buffer + Memory Chunk。
pub struct StagingChunk {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    pub capacity: vk::DeviceSize,
    pub cursor: vk::DeviceSize,
    pub mapped_ptr: *mut u8,
}

// Safety: StagingChunk 独占其映射指针与 Vulkan 对象，跨线程移动安全。
unsafe impl Send for StagingChunk {}
unsafe impl Sync for StagingChunk {}

impl StagingChunk {
    pub fn new(context: &VulkanContext, capacity: vk::DeviceSize) -> Result<Self> {
        let (buffer, memory) = create_buffer(
            context,
            capacity,
            BufferUsage::TRANSFER_SRC,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("StagingChunk: create staging buffer")?;

        let mapped_ptr = unsafe {
            context
                .device
                .map_memory(memory, 0, capacity, vk::MemoryMapFlags::empty())
        }
        .context("StagingChunk: map staging memory")? as *mut u8;

        Ok(Self {
            buffer,
            memory,
            capacity,
            cursor: 0,
            mapped_ptr,
        })
    }

    pub fn can_fit(&self, size: vk::DeviceSize, alignment: vk::DeviceSize) -> bool {
        let offset = align_up(self.cursor, alignment);
        match offset.checked_add(size) {
            Some(end) => end <= self.capacity,
            None => false,
        }
    }

    pub fn stage(
        &mut self,
        data: &[u8],
        alignment: vk::DeviceSize,
    ) -> (vk::Buffer, vk::DeviceSize) {
        let offset = align_up(self.cursor, alignment);
        if !data.is_empty() && !self.mapped_ptr.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    self.mapped_ptr.add(offset as usize),
                    data.len(),
                );
            }
        }
        self.cursor = offset + data.len() as vk::DeviceSize;
        (self.buffer, offset)
    }

    pub fn unmap(&mut self, device: &ash::Device) {
        if !self.mapped_ptr.is_null() {
            unsafe {
                device.unmap_memory(self.memory);
            }
            self.mapped_ptr = std::ptr::null_mut();
        }
    }

    pub fn destroy(&mut self, device: &ash::Device) {
        self.unmap(device);
        if self.buffer != vk::Buffer::null() {
            unsafe {
                device.destroy_buffer(self.buffer, None);
            }
            self.buffer = vk::Buffer::null();
        }
        if self.memory != vk::DeviceMemory::null() {
            unsafe {
                device.free_memory(self.memory, None);
            }
            self.memory = vk::DeviceMemory::null();
        }
    }
}

/// 批量上传暂存区 Arena。
///
/// 替代逐个资源单独 `create_buffer` + `vkAllocateMemory`，批量加载时将数百次小分配聚合为
/// 少量 16~64 MiB Chunk 分配。提交前统一调用 `unmap_all` 显式解绑映射，保证 llvmpipe
/// 等纯软件栅格化后端与硬件驱动的渲染一致性。
pub struct StagingArena {
    chunks: Vec<StagingChunk>,
    default_chunk_size: vk::DeviceSize,
    allocation_count: usize,
}

impl StagingArena {
    /// 默认单 Chunk 大小（16 MiB）。
    pub const DEFAULT_CHUNK_SIZE: vk::DeviceSize = 16 * 1024 * 1024;

    pub fn new(default_chunk_size: vk::DeviceSize) -> Self {
        Self {
            chunks: Vec::new(),
            default_chunk_size: default_chunk_size.max(1024 * 1024),
            allocation_count: 0,
        }
    }

    /// 将数据拷贝进暂存区，返回对应的 `(VkBuffer, offset)`。
    pub fn stage(
        &mut self,
        context: &VulkanContext,
        data: &[u8],
        alignment: vk::DeviceSize,
    ) -> Result<(vk::Buffer, vk::DeviceSize)> {
        let size = data.len() as vk::DeviceSize;
        let alignment = alignment.max(1);
        self.allocation_count += 1;

        if let Some(last_chunk) = self.chunks.last_mut() {
            if last_chunk.can_fit(size, alignment) {
                return Ok(last_chunk.stage(data, alignment));
            }
        }

        // 当前 Chunk 放不下，创建新 Chunk
        let needed = align_up(size, alignment);
        let chunk_size = self.default_chunk_size.max(needed.next_power_of_two());
        let mut chunk = StagingChunk::new(context, chunk_size)?;
        let result = chunk.stage(data, alignment);
        self.chunks.push(chunk);
        Ok(result)
    }

    /// 在提交命令前显式解映射所有 Chunk（针对 llvmpipe 驱动时序要求）。
    pub fn unmap_all(&mut self, device: &ash::Device) {
        for chunk in &mut self.chunks {
            chunk.unmap(device);
        }
    }

    /// 释放全部暂存 Chunk 与显存。
    pub fn destroy(&mut self, device: &ash::Device) {
        for mut chunk in self.chunks.drain(..) {
            chunk.destroy(device);
        }
        self.allocation_count = 0;
    }

    /// 获取暂存区统计。
    pub fn stats(&self) -> SuballocatorStats {
        let mut total_reserved_bytes = 0;
        let mut active_used_bytes = 0;
        for chunk in &self.chunks {
            total_reserved_bytes += chunk.capacity;
            active_used_bytes += chunk.cursor;
        }
        SuballocatorStats {
            total_reserved_bytes,
            active_used_bytes,
            chunk_count: self.chunks.len(),
            allocation_count: self.allocation_count,
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_up() {
        assert_eq!(align_up(0, 16), 0);
        assert_eq!(align_up(1, 16), 16);
        assert_eq!(align_up(16, 16), 16);
        assert_eq!(align_up(17, 16), 32);
        assert_eq!(align_up(300, 256), 512);
        assert_eq!(align_up(512, 256), 512);
        assert_eq!(align_up(10, 1), 10);
        assert_eq!(align_up(10, 0), 10);
    }

    #[test]
    fn test_chunk_allocation_and_coalescing() {
        let mut chunk = MemoryChunk::new_mock(1024, 0);
        assert_eq!(chunk.free_ranges.len(), 1);
        assert_eq!(
            chunk.free_ranges[0],
            MemoryRange {
                offset: 0,
                size: 1024
            }
        );

        // 分配 1: offset 0, size 100
        let a1 = chunk.allocate(100, 16).unwrap();
        assert_eq!(a1, 0);

        // 分配 2: offset 112 (align 16), size 200
        let a2 = chunk.allocate(200, 16).unwrap();
        assert_eq!(a2, 112);

        // 分配 3: offset 320 (align 64), size 100
        let a3 = chunk.allocate(100, 64).unwrap();
        assert_eq!(a3, 320);

        assert_eq!(chunk.active_allocations, 3);
        assert_eq!(chunk.allocated_bytes, 400);

        // 释放 a2 (112..312)
        chunk.free(a2, 200);
        assert_eq!(chunk.active_allocations, 2);

        // 释放 a1 (0..100) -> 与对齐空隙 (100..112) 及已释放的 a2 (112..312) 合并为 0..320
        chunk.free(a1, 100);
        assert_eq!(chunk.active_allocations, 1);

        // 释放 a3 (320..420) -> 全部空闲区间合并回 0..1024
        chunk.free(a3, 100);
        assert_eq!(chunk.active_allocations, 0);
        assert_eq!(chunk.allocated_bytes, 0);
        assert_eq!(chunk.free_ranges.len(), 1);
        assert_eq!(
            chunk.free_ranges[0],
            MemoryRange {
                offset: 0,
                size: 1024
            }
        );
    }

    #[test]
    fn test_chunk_out_of_memory() {
        let mut chunk = MemoryChunk::new_mock(500, 0);
        assert!(chunk.allocate(600, 16).is_none());
        let a = chunk.allocate(500, 16).unwrap();
        assert_eq!(a, 0);
        assert!(chunk.allocate(1, 1).is_none());
    }

    #[test]
    fn test_suballocator_stats() {
        let mut suballoc = MemorySuballocator::new(64 * 1024 * 1024);
        let s = suballoc.stats();
        assert_eq!(s.total_reserved_bytes, 0);
        assert_eq!(s.active_used_bytes, 0);
        assert_eq!(s.chunk_count, 0);
        assert_eq!(s.allocation_count, 0);
    }
}
