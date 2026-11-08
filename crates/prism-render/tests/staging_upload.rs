//! Uploads through the shared staging ring (#1030).
//!
//! Every upload goes through `VulkanContext`'s ring, which hands out offsets,
//! wraps when the tail no longer fits and grows when a payload is larger than
//! the current capacity. These tests push payloads through all three paths and
//! read the destination back, so a stale offset or a bad wrap shows up as
//! corrupted bytes rather than as a validation message nobody reads.

use ash::vk;
use prism_render::buffer::{create_buffer, upload_to_buffer, MemoryProperties};
use prism_render::context::VulkanContext;

/// Headless context; `None` when the machine has no usable Vulkan driver.
fn headless() -> Option<VulkanContext> {
    match VulkanContext::new(&[]) {
        Ok(context) => Some(context),
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                || message.contains("VK_ERROR_EXTENSION_NOT_PRESENT")
                || message.contains("no devices")
                || message.contains("No valid")
                || message.contains("PhysicalDevice")
            {
                eprintln!("[SKIP] no Vulkan driver: {error:#}");
                return None;
            }
            panic!("VulkanContext::new failed: {error:#}");
        }
    }
}

fn pattern(size: usize, seed: u8) -> Vec<u8> {
    (0..size)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Uploads `data` into a fresh host-visible buffer and returns what landed
/// there, so the caller compares bytes instead of trusting the copy.
unsafe fn round_trip(
    context: &VulkanContext,
    command_pool: vk::CommandPool,
    data: &[u8],
) -> Vec<u8> {
    let size = data.len() as vk::DeviceSize;
    let (destination, memory) = create_buffer(
        context,
        size,
        vk::BufferUsageFlags::TRANSFER_DST,
        MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
    )
    .expect("create destination buffer");

    unsafe {
        upload_to_buffer(
            context,
            command_pool,
            context.graphics_queue,
            destination,
            size,
            data,
        )
        .expect("upload payload");

        let ptr = context
            .device
            .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
            .expect("map destination");
        let read_back = std::slice::from_raw_parts(ptr as *const u8, data.len()).to_vec();
        context.device.unmap_memory(memory);

        context.device.destroy_buffer(destination, None);
        context.device.free_memory(memory, None);
        read_back
    }
}

#[test]
fn uploads_reuse_the_staging_ring_across_wrap_and_growth() {
    let Some(context) = headless() else {
        return;
    };
    let command_pool = unsafe {
        context.device.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(context.graphics_queue_family),
            None,
        )
    }
    .expect("create command pool");

    // Sizes walk the ring through every path: first allocation, a second slot,
    // a wrap (the third payload does not fit in the 1 MiB initial capacity),
    // growth (3 MiB is larger than the current capacity), and a payload that is
    // not a multiple of the ring's 16-byte alignment.
    let payloads = [
        pattern(64 * 1024, 1),
        pattern(512 * 1024, 2),
        pattern(512 * 1024, 3),
        pattern(3 * 1024 * 1024, 4),
        pattern(300, 5),
        pattern(64 * 1024, 6),
    ];

    for (index, payload) in payloads.iter().enumerate() {
        let read_back = unsafe { round_trip(&context, command_pool, payload) };
        assert_eq!(
            read_back.len(),
            payload.len(),
            "payload {index} length changed"
        );
        assert!(
            read_back == *payload,
            "payload {index} ({} bytes) was corrupted by the staging ring",
            payload.len()
        );
    }

    unsafe { context.device.destroy_command_pool(command_pool, None) };
}
