//! Per-frame-in-flight resources: command buffer and sync primitives.

use anyhow::Result;
use ash::vk;

pub const FRAMES_IN_FLIGHT: usize = 2;

pub struct Frame {
    pub command_buffer: vk::CommandBuffer,
    /// Signaled by acquire, waited on by the render submit.
    pub image_available: vk::Semaphore,
    /// Signaled by the render submit, waited on by the CPU before reusing
    /// this frame's resources.
    pub in_flight: vk::Fence,
}

impl Frame {
    pub fn new(device: &ash::Device, command_buffer: vk::CommandBuffer) -> Result<Self> {
        let image_available =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
        let in_flight = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?
        };
        Ok(Self {
            command_buffer,
            image_available,
            in_flight,
        })
    }

    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_semaphore(self.image_available, None);
            device.destroy_fence(self.in_flight, None);
        }
    }
}
