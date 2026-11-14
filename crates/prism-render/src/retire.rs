//! Frame-delayed resource destruction.
//!
//! Vulkan forbids destroying a buffer, image or pipeline while a submitted
//! command buffer still references it. Passes that grow a device buffer
//! mid-frame (UI vertex buffer, sprite/particle instance buffers, ReSTIR GI
//! targets on resize) therefore cannot free the old object immediately: the
//! previous `FRAMES_IN_FLIGHT - 1` submissions may still be executing.
//!
//! A `device_wait_idle` before the free works but stalls the whole GPU. This
//! queue instead parks the object in the slot that is currently recording and
//! releases it when that same frame slot comes around again — by which point
//! `begin_frame` has already waited on that slot's fence, which is the fence
//! the referencing submission signalled.

use ash::vk;

use crate::swapchain::FRAMES_IN_FLIGHT;

/// A Vulkan object awaiting its frame slot to retire.
///
/// Not every variant is used by every feature combination (`Image` belongs to
/// the ReSTIR GI resize path), so the enum is allowed to have unused variants.
#[allow(dead_code)]
pub(crate) enum RetiredResource {
    Buffer {
        buffer: vk::Buffer,
        memory: vk::DeviceMemory,
    },
    Image {
        view: vk::ImageView,
        image: vk::Image,
        memory: vk::DeviceMemory,
    },
    Pipeline(vk::Pipeline),
    PipelineLayout(vk::PipelineLayout),
    Framebuffer(vk::Framebuffer),
    RenderPass(vk::RenderPass),
}

impl RetiredResource {
    unsafe fn destroy(self, device: &ash::Device) {
        unsafe {
            match self {
                Self::Buffer { buffer, memory } => {
                    if buffer != vk::Buffer::null() {
                        device.destroy_buffer(buffer, None);
                    }
                    if memory != vk::DeviceMemory::null() {
                        device.free_memory(memory, None);
                    }
                }
                Self::Image {
                    view,
                    image,
                    memory,
                } => {
                    if view != vk::ImageView::null() {
                        device.destroy_image_view(view, None);
                    }
                    if image != vk::Image::null() {
                        device.destroy_image(image, None);
                    }
                    if memory != vk::DeviceMemory::null() {
                        device.free_memory(memory, None);
                    }
                }
                Self::Pipeline(pipeline) => {
                    if pipeline != vk::Pipeline::null() {
                        device.destroy_pipeline(pipeline, None);
                    }
                }
                Self::PipelineLayout(layout) => {
                    if layout != vk::PipelineLayout::null() {
                        device.destroy_pipeline_layout(layout, None);
                    }
                }
                Self::Framebuffer(framebuffer) => {
                    if framebuffer != vk::Framebuffer::null() {
                        device.destroy_framebuffer(framebuffer, None);
                    }
                }
                Self::RenderPass(render_pass) => {
                    if render_pass != vk::RenderPass::null() {
                        device.destroy_render_pass(render_pass, None);
                    }
                }
            }
        }
    }
}

/// Per-pass retirement queue, one bucket per frame slot.
#[derive(Default)]
pub(crate) struct RetireQueue {
    slots: [Vec<RetiredResource>; FRAMES_IN_FLIGHT],
}

impl RetireQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Park `resource` until the frame slot that is recording now comes around
    /// again. Call from `execute` / render recording, never from teardown.
    pub(crate) fn retire(&mut self, frame_index: u32, resource: RetiredResource) {
        // The current frame's slot is the one whose fence guards this
        // submission, so the object has to outlive exactly that slot.
        let slot = frame_index as usize % FRAMES_IN_FLIGHT;
        self.slots[slot].push(resource);
    }

    /// Release everything parked in `frame_index`'s slot.
    ///
    /// Must run right after that slot's fence was waited (i.e. at the top of
    /// `begin_frame`, before recording). The bucket it releases belongs to the
    /// frame that ran two frames ago in the same slot.
    pub(crate) fn collect(&mut self, device: &ash::Device, frame_index: u32) {
        let slot = frame_index as usize % FRAMES_IN_FLIGHT;
        for resource in self.slots[slot].drain(..) {
            unsafe { resource.destroy(device) };
        }
    }

    /// Number of objects parked in `frame_index`'s slot (tests only).
    #[cfg(test)]
    pub(crate) fn slot_len(&self, frame_index: u32) -> usize {
        self.slots[frame_index as usize % FRAMES_IN_FLIGHT].len()
    }

    /// Drop a slot's contents without a device handle (tests only).
    #[cfg(test)]
    pub(crate) fn clear_slot(&mut self, frame_index: u32) {
        self.slots[frame_index as usize % FRAMES_IN_FLIGHT].clear();
    }

    /// Release every parked object. Only call after `device_wait_idle`.
    pub(crate) fn destroy_all(&mut self, device: &ash::Device) {
        for slot in &mut self.slots {
            for resource in slot.drain(..) {
                unsafe { resource.destroy(device) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Objects parked in slot N are released when slot N comes around again,
    /// never in the frame that parked them.
    #[test]
    fn a_resource_survives_until_its_slot_rotates_back() {
        let mut queue = RetireQueue::new();
        queue.retire(0, RetiredResource::Pipeline(vk::Pipeline::null()));
        for slot in 1..FRAMES_IN_FLIGHT as u32 {
            queue.clear_slot(slot);
            assert_eq!(
                queue.slot_len(0),
                1,
                "slot 0 keeps its object while others run"
            );
        }
        queue.clear_slot(0);
        assert_eq!(queue.slot_len(0), 0, "released when its slot rotates back");
    }

    /// Every slot is independent: parking in one never blocks another.
    #[test]
    fn slots_do_not_share_buckets() {
        let mut queue = RetireQueue::new();
        for slot in 0..FRAMES_IN_FLIGHT as u32 {
            queue.retire(slot, RetiredResource::Pipeline(vk::Pipeline::null()));
        }
        for slot in 0..FRAMES_IN_FLIGHT as u32 {
            assert_eq!(queue.slot_len(slot), 1);
        }
        queue.clear_slot(1 % FRAMES_IN_FLIGHT as u32);
        assert_eq!(queue.slot_len(1 % FRAMES_IN_FLIGHT as u32), 0);
    }

    /// A frame index larger than the slot count wraps instead of panicking.
    #[test]
    fn frame_indices_wrap_to_slots() {
        let mut queue = RetireQueue::new();
        queue.retire(
            FRAMES_IN_FLIGHT as u32,
            RetiredResource::Pipeline(vk::Pipeline::null()),
        );
        assert_eq!(queue.slot_len(0), 1);
    }
}
