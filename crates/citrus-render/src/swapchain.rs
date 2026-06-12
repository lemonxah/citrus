//! Swapchain creation, image views, and per-image present semaphores.

use anyhow::{Context as _, Result};
use ash::{khr, vk};

use crate::context::GpuContext;

pub struct Swapchain {
    loader: khr::swapchain::Device,
    pub handle: vk::SwapchainKHR,
    pub images: Vec<vk::Image>,
    pub views: Vec<vk::ImageView>,
    /// One per swapchain image: signaled by the submit that renders to that
    /// image, waited on by present.
    pub render_finished: Vec<vk::Semaphore>,
    #[allow(dead_code)] // consumed by pipeline creation in the mesh milestone
    pub format: vk::SurfaceFormatKHR,
    pub extent: vk::Extent2D,
}

impl Swapchain {
    pub fn new(ctx: &GpuContext, width: u32, height: u32, vsync: bool) -> Result<Self> {
        let loader = khr::swapchain::Device::new(&ctx.instance, &ctx.device);

        let caps = unsafe {
            ctx.surface_loader
                .get_physical_device_surface_capabilities(ctx.physical_device, ctx.surface)?
        };
        let formats = unsafe {
            ctx.surface_loader
                .get_physical_device_surface_formats(ctx.physical_device, ctx.surface)?
        };
        let present_modes = unsafe {
            ctx.surface_loader
                .get_physical_device_surface_present_modes(ctx.physical_device, ctx.surface)?
        };
        // FIFO (vsync) is always available. Without vsync prefer MAILBOX
        // (uncapped, no tearing), then IMMEDIATE (uncapped, may tear).
        let present_mode = if vsync {
            vk::PresentModeKHR::FIFO
        } else {
            [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::IMMEDIATE]
                .into_iter()
                .find(|m| present_modes.contains(m))
                .unwrap_or(vk::PresentModeKHR::FIFO)
        };

        let format = formats
            .iter()
            .copied()
            .find(|f| {
                f.format == vk::Format::B8G8R8A8_SRGB
                    && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
            })
            .or_else(|| formats.first().copied())
            .context("surface reports no formats")?;

        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: height.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };

        let mut image_count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            image_count = image_count.min(caps.max_image_count);
        }

        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(ctx.surface)
            .min_image_count(image_count)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(present_mode)
            .clipped(true);
        let handle = unsafe { loader.create_swapchain(&info, None) }
            .context("creating swapchain")?;

        let images = unsafe { loader.get_swapchain_images(handle)? };
        let views = images
            .iter()
            .map(|&image| {
                let info = vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format.format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    );
                unsafe { ctx.device.create_image_view(&info, None) }
            })
            .collect::<Result<Vec<_>, _>>()?;

        let render_finished = images
            .iter()
            .map(|_| unsafe {
                ctx.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            })
            .collect::<Result<Vec<_>, _>>()?;

        tracing::debug!(
            ?extent,
            format = ?format.format,
            images = images.len(),
            ?present_mode,
            "swapchain created"
        );

        Ok(Self {
            loader,
            handle,
            images,
            views,
            render_finished,
            format,
            extent,
        })
    }

    pub fn acquire(
        &self,
        signal: vk::Semaphore,
    ) -> ash::prelude::VkResult<(u32, bool)> {
        unsafe {
            self.loader
                .acquire_next_image(self.handle, u64::MAX, signal, vk::Fence::null())
        }
    }

    pub fn present(
        &self,
        queue: vk::Queue,
        image_index: u32,
        wait: vk::Semaphore,
    ) -> ash::prelude::VkResult<bool> {
        let waits = [wait];
        let swapchains = [self.handle];
        let indices = [image_index];
        let info = vk::PresentInfoKHR::default()
            .wait_semaphores(&waits)
            .swapchains(&swapchains)
            .image_indices(&indices);
        unsafe { self.loader.queue_present(queue, &info) }
    }

    /// Caller must ensure the GPU is idle or no longer using these objects.
    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            for &view in &self.views {
                device.destroy_image_view(view, None);
            }
            for &sem in &self.render_finished {
                device.destroy_semaphore(sem, None);
            }
            self.loader.destroy_swapchain(self.handle, None);
        }
        self.views.clear();
        self.render_finished.clear();
        self.images.clear();
    }
}
