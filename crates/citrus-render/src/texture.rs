//! GPU texture upload (RGBA8, no mipmaps yet — M3 adds mip generation).

use anyhow::Result;
use ash::vk;
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator};

use crate::alloc::{Buffer, one_time_submit};
use crate::types::TextureData;

pub(crate) struct GpuTexture {
    pub image: vk::Image,
    pub view: vk::ImageView,
    allocation: Option<Allocation>,
}

impl GpuTexture {
    pub fn upload(
        device: &ash::Device,
        allocator: &mut Allocator,
        pool: vk::CommandPool,
        queue: vk::Queue,
        data: &TextureData,
    ) -> Result<Self> {
        let format = if data.srgb {
            vk::Format::R8G8B8A8_SRGB
        } else {
            vk::Format::R8G8B8A8_UNORM
        };
        let extent = vk::Extent3D {
            width: data.width,
            height: data.height,
            depth: 1,
        };

        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(extent)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&info, None)? };
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let allocation = allocator.allocate(&AllocationCreateDesc {
            name: "texture",
            requirements,
            location: MemoryLocation::GpuOnly,
            linear: false,
            allocation_scheme: AllocationScheme::GpuAllocatorManaged,
        })?;
        unsafe { device.bind_image_memory(image, allocation.memory(), allocation.offset())? };

        let mut staging = Buffer::new(
            device,
            allocator,
            data.pixels.len() as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::CpuToGpu,
            "texture staging",
        )?;
        staging.write(0, &data.pixels);

        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);

        one_time_submit(device, pool, queue, |cb| unsafe {
            let to_transfer = vk::ImageMemoryBarrier2::default()
                .image(image)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
                .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .subresource_range(range);
            let barriers = [to_transfer];
            device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&barriers),
            );

            let copy = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(extent);
            device.cmd_copy_buffer_to_image(
                cb,
                staging.handle,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            );

            let to_sampled = vk::ImageMemoryBarrier2::default()
                .image(image)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .subresource_range(range);
            let barriers = [to_sampled];
            device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&barriers),
            );
        })?;

        staging.destroy(device, allocator);

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(range);
        let view = unsafe { device.create_image_view(&view_info, None)? };

        Ok(Self {
            image,
            view,
            allocation: Some(allocation),
        })
    }

    /// Upload baked lightmaps as a sampled `R32G32B32A32_SFLOAT` 2D array, one
    /// layer per object. Every `layer` must already be `size*size*4` floats
    /// (RGBA32F, resampled to a common size by the caller). Sampled in the
    /// standard shader by `uv1` + per-object layer for static GI.
    pub fn upload_lightmap_array(
        device: &ash::Device,
        allocator: &mut Allocator,
        pool: vk::CommandPool,
        queue: vk::Queue,
        layers: &[Vec<f32>],
        size: u32,
    ) -> Result<Self> {
        let format = vk::Format::R32G32B32A32_SFLOAT;
        let n = layers.len().max(1) as u32;
        let extent = vk::Extent3D {
            width: size.max(1),
            height: size.max(1),
            depth: 1,
        };
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(extent)
            .mip_levels(1)
            .array_layers(n)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&info, None)? };
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let allocation = allocator.allocate(&AllocationCreateDesc {
            name: "lightmap array",
            requirements,
            location: MemoryLocation::GpuOnly,
            linear: false,
            allocation_scheme: AllocationScheme::GpuAllocatorManaged,
        })?;
        unsafe { device.bind_image_memory(image, allocation.memory(), allocation.offset())? };

        let layer_floats = (extent.width * extent.height * 4) as usize;
        let layer_bytes = layer_floats * std::mem::size_of::<f32>();
        let mut staging = Buffer::new(
            device,
            allocator,
            (n as usize * layer_bytes) as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::CpuToGpu,
            "lightmap staging",
        )?;
        for (i, layer) in layers.iter().enumerate() {
            // Pad/truncate to exactly one layer's worth (defensive).
            let mut buf = vec![0f32; layer_floats];
            let k = layer.len().min(layer_floats);
            buf[..k].copy_from_slice(&layer[..k]);
            staging.write(i * layer_bytes, bytemuck::cast_slice(&buf));
        }

        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(n);

        one_time_submit(device, pool, queue, |cb| unsafe {
            let to_transfer = vk::ImageMemoryBarrier2::default()
                .image(image)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
                .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .subresource_range(range);
            device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&[to_transfer]),
            );
            for i in 0..n {
                let copy = vk::BufferImageCopy::default()
                    .buffer_offset((i as usize * layer_bytes) as u64)
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .base_array_layer(i)
                            .layer_count(1),
                    )
                    .image_extent(extent);
                device.cmd_copy_buffer_to_image(
                    cb,
                    staging.handle,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[copy],
                );
            }
            let to_sampled = vk::ImageMemoryBarrier2::default()
                .image(image)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .subresource_range(range);
            device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&[to_sampled]),
            );
        })?;
        staging.destroy(device, allocator);

        let view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D_ARRAY)
                    .format(format)
                    .subresource_range(range),
                None,
            )?
        };
        Ok(Self {
            image,
            view,
            allocation: Some(allocation),
        })
    }

    pub fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
        }
        if let Some(allocation) = self.allocation.take() {
            let _ = allocator.free(allocation);
        }
    }
}
