//! A second OS window that renders only an egui UI (the profiler), sharing the
//! main `GpuContext` (instance/device/queue). It owns its own surface,
//! swapchain, egui renderer, and a single-frame-in-flight command buffer + sync.
//! Kept deliberately simple: the profiler is low-rate, so one in-flight frame
//! avoids the ping-pong bookkeeping the main renderer needs.

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use ash::vk;
use gpu_allocator::vulkan::Allocator;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use crate::EguiDraw;
use crate::context::GpuContext;
use crate::image_barrier;
use crate::swapchain::Swapchain;

pub struct ProfilerWindow {
    allocator: Arc<Mutex<Allocator>>,
    surface: vk::SurfaceKHR,
    swapchain: Swapchain,
    egui: egui_ash_renderer::Renderer,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    image_available: vk::Semaphore,
    in_flight: vk::Fence,
    needs_resize: bool,
    /// egui texture frees deferred to the next frame (after the fence retires),
    /// mirroring the main renderer's id-reuse-safe free ordering.
    pending_free: Vec<egui::TextureId>,
    width: u32,
    height: u32,
    vsync: bool,
}

impl ProfilerWindow {
    pub fn new(
        ctx: &GpuContext,
        allocator: Arc<Mutex<Allocator>>,
        display: RawDisplayHandle,
        window: RawWindowHandle,
        width: u32,
        height: u32,
        vsync: bool,
    ) -> Result<Self> {
        let surface = unsafe {
            ash_window::create_surface(&ctx.entry, &ctx.instance, display, window, None)
        }
        .context("creating profiler surface")?;

        let swapchain = Swapchain::new_for_surface(ctx, surface, width, height, vsync)?;

        let egui = egui_ash_renderer::Renderer::with_gpu_allocator(
            allocator.clone(),
            ctx.device.clone(),
            egui_ash_renderer::DynamicRendering {
                color_attachment_format: swapchain.format.format,
                depth_attachment_format: None,
            },
            egui_ash_renderer::Options {
                in_flight_frames: 1,
                enable_depth_test: false,
                enable_depth_write: false,
                srgb_framebuffer: true,
            },
        )
        .map_err(|e| anyhow::anyhow!("creating profiler egui renderer: {e}"))?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(ctx.queue_family);
        let command_pool = unsafe { ctx.device.create_command_pool(&pool_info, None)? };
        let command_buffer = unsafe {
            ctx.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0]
        };
        let image_available =
            unsafe { ctx.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
        let in_flight = unsafe {
            ctx.device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?
        };

        Ok(Self {
            allocator,
            surface,
            swapchain,
            egui,
            command_pool,
            command_buffer,
            image_available,
            in_flight,
            needs_resize: false,
            pending_free: Vec::new(),
            width,
            height,
            vsync,
        })
    }

    pub fn request_resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.needs_resize = true;
    }

    fn recreate(&mut self, ctx: &GpuContext) -> Result<()> {
        unsafe { ctx.device.device_wait_idle()? };
        self.swapchain.destroy(&ctx.device);
        self.swapchain =
            Swapchain::new_for_surface(ctx, self.surface, self.width, self.height, self.vsync)?;
        self.needs_resize = false;
        Ok(())
    }

    /// Record + submit + present the egui UI to the profiler window. Skips
    /// silently on a zero-size window or an out-of-date swapchain (re-armed for
    /// the next frame).
    pub fn render(&mut self, ctx: &GpuContext, draw: &EguiDraw) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Ok(());
        }
        if self.needs_resize {
            self.recreate(ctx)?;
        }
        let device = &ctx.device;

        unsafe { device.wait_for_fences(&[self.in_flight], true, u64::MAX)? };

        // The prior frame retired, so its egui texture frees are now safe.
        if !self.pending_free.is_empty() {
            let ids = std::mem::take(&mut self.pending_free);
            let _ = self.egui.free_textures(&ids);
        }
        if !draw.textures_delta.set.is_empty() {
            self.egui
                .set_textures(ctx.queue, self.command_pool, &draw.textures_delta.set)
                .map_err(|e| anyhow::anyhow!("profiler egui set_textures: {e}"))?;
        }

        let image_index = match self.swapchain.acquire(self.image_available) {
            Ok((index, suboptimal)) => {
                if suboptimal {
                    self.needs_resize = true;
                }
                index
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.needs_resize = true;
                return Ok(());
            }
            Err(e) => return Err(e).context("acquiring profiler swapchain image"),
        };

        unsafe { device.reset_fences(&[self.in_flight])? };

        let cb = self.command_buffer;
        let image = self.swapchain.images[image_index as usize];
        let view = self.swapchain.views[image_index as usize];
        let render_finished = self.swapchain.render_finished[image_index as usize];
        let extent = self.swapchain.extent;

        unsafe {
            device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            image_barrier(
                device, cb, image, vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::UNDEFINED, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            );

            let color = vk::RenderingAttachmentInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue { float32: [0.02, 0.02, 0.03, 1.0] },
                });
            let attachments = [color];
            let info = vk::RenderingInfo::default()
                .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent })
                .layer_count(1)
                .color_attachments(&attachments);
            device.cmd_begin_rendering(cb, &info);
            self.egui
                .cmd_draw(cb, extent, draw.pixels_per_point, &draw.primitives)
                .map_err(|e| anyhow::anyhow!("profiler egui draw: {e}"))?;
            device.cmd_end_rendering(cb);

            image_barrier(
                device, cb, image, vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL, vk::ImageLayout::PRESENT_SRC_KHR,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::BOTTOM_OF_PIPE, vk::AccessFlags2::empty(),
            );

            device.end_command_buffer(cb)?;

            let wait = [vk::SemaphoreSubmitInfo::default()
                .semaphore(self.image_available)
                .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
            let cbs = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
            let signal = [vk::SemaphoreSubmitInfo::default()
                .semaphore(render_finished)
                .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
            let submit = vk::SubmitInfo2::default()
                .wait_semaphore_infos(&wait)
                .command_buffer_infos(&cbs)
                .signal_semaphore_infos(&signal);
            device.queue_submit2(ctx.queue, &[submit], self.in_flight)?;
        }

        match self.swapchain.present(ctx.queue, image_index, render_finished) {
            Ok(suboptimal) => {
                if suboptimal {
                    self.needs_resize = true;
                }
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.needs_resize = true,
            Err(e) => return Err(e).context("presenting profiler swapchain"),
        }

        if !draw.textures_delta.free.is_empty() {
            self.pending_free
                .extend_from_slice(&draw.textures_delta.free);
        }
        Ok(())
    }

    pub fn destroy(&mut self, ctx: &GpuContext) {
        unsafe {
            let _ = ctx.device.device_wait_idle();
            if !self.pending_free.is_empty() {
                let ids = std::mem::take(&mut self.pending_free);
                let _ = self.egui.free_textures(&ids);
            }
            ctx.device.destroy_semaphore(self.image_available, None);
            ctx.device.destroy_fence(self.in_flight, None);
            self.swapchain.destroy(&ctx.device);
            ctx.device.destroy_command_pool(self.command_pool, None);
            ctx.surface_loader.destroy_surface(self.surface, None);
        }
        // egui renderer + allocator handle drop themselves.
        let _ = &self.allocator;
    }
}
