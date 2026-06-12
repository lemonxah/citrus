//! citrus-render: ash-based Vulkan renderer.
//!
//! M2 scope: depth-tested mesh rendering with the citrus standard shader
//! (phase 1, variant cache via specialization constants), texture/material
//! system, and an egui overlay pass for the in-engine inspector.

mod alloc;
mod context;
mod frame;
mod pipeline;
mod swapchain;
mod texture;
mod types;

pub use types::*;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use ash::vk;
use bytemuck::{Pod, Zeroable};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{
    Allocation, AllocationCreateDesc, AllocationScheme, Allocator, AllocatorCreateDesc,
};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

use alloc::Buffer;
use context::GpuContext;
use frame::{FRAMES_IN_FLIGHT, Frame};
use pipeline::{PipelineCache, PipelineKey};
use swapchain::Swapchain;
use texture::GpuTexture;

const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
const MAX_MATERIALS: u32 = 1024;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FrameUbo {
    view: [[f32; 4]; 4],
    proj: [[f32; 4]; 4],
    view_proj: [[f32; 4]; 4],
    camera_pos: [f32; 4],
    light_dir: [f32; 4],
    light_color: [f32; 4],
    ambient: [f32; 4],
    misc: [f32; 4], // x = time in seconds
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PushData {
    model: [[f32; 4]; 4],
    base_color: [f32; 4],
    emission: [f32; 4],
    params0: [f32; 4],
    params1: [f32; 4],
}

const _: () = assert!(size_of::<PushData>() == pipeline::PUSH_CONSTANT_SIZE as usize);

struct GpuMesh {
    vertex_buffer: Buffer,
    index_buffer: Buffer,
    index_count: u32,
}

struct Material {
    #[allow(dead_code)] // used by the editor's material list
    name: String,
    params: MaterialParams,
    features: MaterialFeatures,
    has_normal_texture: bool,
    error: bool,
    set: vk::DescriptorSet,
}

struct DepthTarget {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Option<Allocation>,
}

impl DepthTarget {
    fn new(ctx: &GpuContext, allocator: &mut Allocator, extent: vk::Extent2D) -> Result<Self> {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(DEPTH_FORMAT)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { ctx.device.create_image(&info, None)? };
        let requirements = unsafe { ctx.device.get_image_memory_requirements(image) };
        let allocation = allocator.allocate(&AllocationCreateDesc {
            name: "depth",
            requirements,
            location: MemoryLocation::GpuOnly,
            linear: false,
            allocation_scheme: AllocationScheme::GpuAllocatorManaged,
        })?;
        unsafe {
            ctx.device
                .bind_image_memory(image, allocation.memory(), allocation.offset())?
        };
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(DEPTH_FORMAT)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::DEPTH)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = unsafe { ctx.device.create_image_view(&view_info, None)? };
        Ok(Self {
            image,
            view,
            allocation: Some(allocation),
        })
    }

    fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
        }
        if let Some(allocation) = self.allocation.take() {
            let _ = allocator.free(allocation);
        }
    }
}

pub struct Renderer {
    window: Arc<Window>,
    ctx: GpuContext,
    allocator: Option<Arc<Mutex<Allocator>>>,
    swapchain: Swapchain,
    depth: DepthTarget,
    command_pool: vk::CommandPool,
    frames: Vec<Frame>,
    frame_index: usize,
    needs_resize: bool,

    pipeline_cache: PipelineCache,
    descriptor_pool: vk::DescriptorPool,
    material_pool: vk::DescriptorPool,
    frame_ubos: Vec<Buffer>,
    frame_sets: Vec<vk::DescriptorSet>,
    sampler: vk::Sampler,
    default_textures: Vec<GpuTexture>, // [albedo, normal, orm, emission]

    meshes: Vec<GpuMesh>,
    textures: Vec<GpuTexture>,
    materials: Vec<Material>,

    egui: Option<egui_ash_renderer::Renderer>,
    /// Texture frees deferred until those frames can no longer be in flight.
    egui_free_queue: VecDeque<Vec<egui::TextureId>>,
    last_stats: RenderStats,
    vsync: bool,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let display = window.display_handle()?.as_raw();
        let handle = window.window_handle()?.as_raw();
        let ctx = GpuContext::new(display, handle, c"citrus")?;

        let allocator = Allocator::new(&AllocatorCreateDesc {
            instance: ctx.instance.clone(),
            device: ctx.device.clone(),
            physical_device: ctx.physical_device,
            debug_settings: Default::default(),
            buffer_device_address: false,
            allocation_sizes: Default::default(),
        })
        .context("creating GPU allocator")?;
        let allocator = Arc::new(Mutex::new(allocator));

        let size = window.inner_size();
        let swapchain = Swapchain::new(&ctx, size.width, size.height, true)?;
        let depth = DepthTarget::new(&ctx, &mut allocator.lock().unwrap(), swapchain.extent)?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(ctx.queue_family);
        let command_pool = unsafe { ctx.device.create_command_pool(&pool_info, None)? };

        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(FRAMES_IN_FLIGHT as u32);
        let command_buffers = unsafe { ctx.device.allocate_command_buffers(&alloc_info)? };
        let frames = command_buffers
            .into_iter()
            .map(|cb| Frame::new(&ctx.device, cb))
            .collect::<Result<Vec<_>>>()?;

        let pipeline_cache =
            PipelineCache::new(&ctx.device, swapchain.format.format, DEPTH_FORMAT)?;

        // Frame UBOs live in their own pool; the material pool is reset
        // wholesale when a scene is unloaded.
        let frame_pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::UNIFORM_BUFFER,
            descriptor_count: FRAMES_IN_FLIGHT as u32,
        }];
        let descriptor_pool = unsafe {
            ctx.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&frame_pool_sizes),
                None,
            )?
        };
        let material_pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: MAX_MATERIALS * 4,
        }];
        let material_pool = unsafe {
            ctx.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_MATERIALS)
                    .pool_sizes(&material_pool_sizes),
                None,
            )?
        };

        // Per-frame UBOs + descriptor sets.
        let mut frame_ubos = Vec::new();
        let mut frame_sets = Vec::new();
        {
            let mut alloc = allocator.lock().unwrap();
            for i in 0..FRAMES_IN_FLIGHT {
                let buffer = Buffer::new(
                    &ctx.device,
                    &mut alloc,
                    size_of::<FrameUbo>() as u64,
                    vk::BufferUsageFlags::UNIFORM_BUFFER,
                    MemoryLocation::CpuToGpu,
                    &format!("frame ubo {i}"),
                )?;
                let layouts = [pipeline_cache.set0_layout];
                let set = unsafe {
                    ctx.device.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(descriptor_pool)
                            .set_layouts(&layouts),
                    )?[0]
                };
                let buffer_info = [vk::DescriptorBufferInfo::default()
                    .buffer(buffer.handle)
                    .range(size_of::<FrameUbo>() as u64)];
                let write = vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&buffer_info);
                unsafe { ctx.device.update_descriptor_sets(&[write], &[]) };
                frame_ubos.push(buffer);
                frame_sets.push(set);
            }
        }

        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::REPEAT)
            .address_mode_v(vk::SamplerAddressMode::REPEAT)
            .address_mode_w(vk::SamplerAddressMode::REPEAT)
            .max_lod(vk::LOD_CLAMP_NONE);
        let sampler = unsafe { ctx.device.create_sampler(&sampler_info, None)? };

        // 1x1 defaults: white albedo (sRGB), flat normal, white ORM, white
        // emission mask. Bound wherever a material has no texture assigned.
        let defaults = [
            ([255u8, 255, 255, 255], true),
            ([128, 128, 255, 255], false),
            ([255, 255, 255, 255], false),
            ([255, 255, 255, 255], true),
        ];
        let mut default_textures = Vec::new();
        {
            let mut alloc = allocator.lock().unwrap();
            for (pixel, srgb) in defaults {
                default_textures.push(GpuTexture::upload(
                    &ctx.device,
                    &mut alloc,
                    command_pool,
                    ctx.queue,
                    &TextureData {
                        width: 1,
                        height: 1,
                        pixels: pixel.to_vec(),
                        srgb,
                    },
                )?);
            }
        }

        let egui = egui_ash_renderer::Renderer::with_gpu_allocator(
            allocator.clone(),
            ctx.device.clone(),
            egui_ash_renderer::DynamicRendering {
                color_attachment_format: swapchain.format.format,
                depth_attachment_format: Some(DEPTH_FORMAT),
            },
            egui_ash_renderer::Options {
                in_flight_frames: FRAMES_IN_FLIGHT,
                enable_depth_test: false,
                enable_depth_write: false,
                srgb_framebuffer: true,
            },
        )
        .map_err(|e| anyhow::anyhow!("creating egui renderer: {e}"))?;

        Ok(Self {
            window,
            ctx,
            allocator: Some(allocator),
            swapchain,
            depth,
            command_pool,
            frames,
            frame_index: 0,
            needs_resize: false,
            pipeline_cache,
            descriptor_pool,
            material_pool,
            frame_ubos,
            frame_sets,
            sampler,
            default_textures,
            meshes: Vec::new(),
            textures: Vec::new(),
            materials: Vec::new(),
            egui: Some(egui),
            egui_free_queue: VecDeque::new(),
            last_stats: RenderStats::default(),
            vsync: true,
        })
    }

    /// Toggle vsync (FIFO ↔ MAILBOX/IMMEDIATE); takes effect next frame via
    /// swapchain recreation.
    pub fn set_vsync(&mut self, vsync: bool) {
        if self.vsync != vsync {
            self.vsync = vsync;
            self.needs_resize = true;
        }
    }

    pub fn vsync(&self) -> bool {
        self.vsync
    }

    /// Statistics of the last rendered frame.
    pub fn stats(&self) -> RenderStats {
        self.last_stats
    }

    fn allocator(&self) -> Arc<Mutex<Allocator>> {
        self.allocator.as_ref().unwrap().clone()
    }

    pub fn upload_mesh(&mut self, data: &MeshData) -> Result<MeshHandle> {
        let allocator = self.allocator();
        let mut alloc = allocator.lock().unwrap();
        let vertex_buffer = alloc::upload_buffer(
            &self.ctx.device,
            &mut alloc,
            self.command_pool,
            self.ctx.queue,
            bytemuck::cast_slice(&data.vertices),
            vk::BufferUsageFlags::VERTEX_BUFFER,
            "vertices",
        )?;
        let index_buffer = alloc::upload_buffer(
            &self.ctx.device,
            &mut alloc,
            self.command_pool,
            self.ctx.queue,
            bytemuck::cast_slice(&data.indices),
            vk::BufferUsageFlags::INDEX_BUFFER,
            "indices",
        )?;
        self.meshes.push(GpuMesh {
            vertex_buffer,
            index_buffer,
            index_count: data.indices.len() as u32,
        });
        Ok(MeshHandle(self.meshes.len() - 1))
    }

    pub fn upload_texture(&mut self, data: &TextureData) -> Result<TextureHandle> {
        let allocator = self.allocator();
        let texture = GpuTexture::upload(
            &self.ctx.device,
            &mut allocator.lock().unwrap(),
            self.command_pool,
            self.ctx.queue,
            data,
        )?;
        self.textures.push(texture);
        Ok(TextureHandle(self.textures.len() - 1))
    }

    pub fn create_material(&mut self, desc: &MaterialDesc) -> Result<MaterialHandle> {
        let layouts = [self.pipeline_cache.set1_layout];
        let set = unsafe {
            self.ctx.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.material_pool)
                    .set_layouts(&layouts),
            )?[0]
        };

        let slots = [desc.albedo, desc.normal, desc.orm, desc.emission];
        let image_infos: Vec<[vk::DescriptorImageInfo; 1]> = slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let view = match slot {
                    Some(handle) => self.textures[handle.0].view,
                    None => self.default_textures[i].view,
                };
                [vk::DescriptorImageInfo::default()
                    .sampler(self.sampler)
                    .image_view(view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
            })
            .collect();
        let writes: Vec<_> = image_infos
            .iter()
            .enumerate()
            .map(|(i, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(i as u32)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(info)
            })
            .collect();
        unsafe { self.ctx.device.update_descriptor_sets(&writes, &[]) };

        let mut features = desc.features;
        features.normal_map &= desc.normal.is_some();
        self.materials.push(Material {
            name: desc.name.clone(),
            params: desc.params,
            features,
            has_normal_texture: desc.normal.is_some(),
            error: desc.error,
            set,
        });
        Ok(MaterialHandle(self.materials.len() - 1))
    }

    /// Flag a material as broken; it renders with the error swirl shader.
    pub fn set_material_error(&mut self, handle: MaterialHandle, error: bool) {
        self.materials[handle.0].error = error;
    }

    pub fn material_params_mut(&mut self, handle: MaterialHandle) -> &mut MaterialParams {
        &mut self.materials[handle.0].params
    }

    pub fn set_material_features(&mut self, handle: MaterialHandle, features: MaterialFeatures) {
        let material = &mut self.materials[handle.0];
        material.features = features;
        material.features.normal_map &= material.has_normal_texture;
    }

    pub fn resize(&mut self) {
        self.needs_resize = true;
    }

    /// Tear down all scene resources (meshes, textures, materials) so a new
    /// scene can be uploaded from scratch. Existing handles become invalid.
    pub fn reset_scene(&mut self) -> Result<()> {
        unsafe { self.ctx.device.device_wait_idle()? };
        let allocator = self.allocator();
        let mut alloc = allocator.lock().unwrap();
        for mesh in &mut self.meshes {
            mesh.vertex_buffer.destroy(&self.ctx.device, &mut alloc);
            mesh.index_buffer.destroy(&self.ctx.device, &mut alloc);
        }
        self.meshes.clear();
        for texture in &mut self.textures {
            texture.destroy(&self.ctx.device, &mut alloc);
        }
        self.textures.clear();
        self.materials.clear();
        unsafe {
            self.ctx
                .device
                .reset_descriptor_pool(self.material_pool, vk::DescriptorPoolResetFlags::empty())?;
        }
        Ok(())
    }

    fn recreate_swapchain(&mut self) -> Result<()> {
        let size = self.window.inner_size();
        unsafe { self.ctx.device.device_wait_idle()? };
        let allocator = self.allocator();
        let mut alloc = allocator.lock().unwrap();
        self.swapchain.destroy(&self.ctx.device);
        self.depth.destroy(&self.ctx.device, &mut alloc);
        self.swapchain = Swapchain::new(&self.ctx, size.width, size.height, self.vsync)?;
        self.depth = DepthTarget::new(&self.ctx, &mut alloc, self.swapchain.extent)?;
        self.needs_resize = false;
        Ok(())
    }

    pub fn render(&mut self, input: &FrameInput<'_>) -> Result<()> {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        if self.needs_resize {
            self.recreate_swapchain()?;
        }

        // Upload any new/changed egui textures before recording the frame.
        if let Some(egui_draw) = &input.egui {
            if !egui_draw.textures_delta.set.is_empty() {
                self.egui
                    .as_mut()
                    .unwrap()
                    .set_textures(
                        self.ctx.queue,
                        self.command_pool,
                        &egui_draw.textures_delta.set,
                    )
                    .map_err(|e| anyhow::anyhow!("uploading egui textures: {e}"))?;
            }
        }

        let device = self.ctx.device.clone();
        let frame = &self.frames[self.frame_index];

        unsafe { device.wait_for_fences(&[frame.in_flight], true, u64::MAX)? };

        // This frame's prior submission has retired; egui textures freed two
        // frames ago are safe to release now.
        while self.egui_free_queue.len() >= FRAMES_IN_FLIGHT {
            let ids = self.egui_free_queue.pop_front().unwrap();
            self.egui
                .as_mut()
                .unwrap()
                .free_textures(&ids)
                .map_err(|e| anyhow::anyhow!("freeing egui textures: {e}"))?;
        }

        let image_index = match self.swapchain.acquire(frame.image_available) {
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
            Err(e) => return Err(e).context("acquiring swapchain image"),
        };

        unsafe { device.reset_fences(&[frame.in_flight])? };

        // Per-frame uniforms.
        let ubo = FrameUbo {
            view: input.camera.view.to_cols_array_2d(),
            proj: input.camera.proj.to_cols_array_2d(),
            view_proj: (input.camera.proj * input.camera.view).to_cols_array_2d(),
            camera_pos: input.camera.position.extend(1.0).to_array(),
            light_dir: input.light.direction.normalize().extend(0.0).to_array(),
            light_color: [
                input.light.color[0] * input.light.intensity,
                input.light.color[1] * input.light.intensity,
                input.light.color[2] * input.light.intensity,
                1.0,
            ],
            ambient: [
                input.light.ambient[0],
                input.light.ambient[1],
                input.light.ambient[2],
                1.0,
            ],
            misc: [input.time, 0.0, 0.0, 0.0],
        };
        self.frame_ubos[self.frame_index].write(0, bytemuck::bytes_of(&ubo));

        let cb = frame.command_buffer;
        let image = self.swapchain.images[image_index as usize];
        let color_view = self.swapchain.views[image_index as usize];
        let extent = self.swapchain.extent;
        let render_finished = self.swapchain.render_finished[image_index as usize];
        let frame_set = self.frame_sets[self.frame_index];
        let image_available = frame.image_available;
        let in_flight = frame.in_flight;

        // Sort: opaque/cutout first, then transparent back-to-front.
        let mut order: Vec<usize> = (0..input.draws.len()).collect();
        let distance = |i: usize| {
            let pos = input.draws[i].transform.w_axis.truncate();
            (pos - input.camera.position).length_squared()
        };
        order.sort_by(|&a, &b| {
            let blend_a = self.materials[input.draws[a].material.0].features.alpha_mode
                == AlphaMode::Blend;
            let blend_b = self.materials[input.draws[b].material.0].features.alpha_mode
                == AlphaMode::Blend;
            blend_a.cmp(&blend_b).then_with(|| {
                if blend_a {
                    distance(b).total_cmp(&distance(a))
                } else {
                    std::cmp::Ordering::Equal
                }
            })
        });

        unsafe {
            device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // Color: undefined -> color attachment.
            image_barrier(
                &device,
                cb,
                image,
                vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::TOP_OF_PIPE,
                vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            );
            // Depth: undefined -> depth attachment (contents discarded).
            image_barrier(
                &device,
                cb,
                self.depth.image,
                vk::ImageAspectFlags::DEPTH,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::TOP_OF_PIPE,
                vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
            );

            let color_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(color_view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: input.clear_color,
                    },
                });
            let depth_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(self.depth.view)
                .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::DONT_CARE)
                .clear_value(vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue {
                        depth: 1.0,
                        stencil: 0,
                    },
                });
            let color_attachments = [color_attachment];
            let rendering_info = vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                })
                .layer_count(1)
                .color_attachments(&color_attachments)
                .depth_attachment(&depth_attachment);

            device.cmd_begin_rendering(cb, &rendering_info);

            // Negative-height viewport: Y up in NDC, glTF winding preserved.
            let viewport = vk::Viewport {
                x: 0.0,
                y: extent.height as f32,
                width: extent.width as f32,
                height: -(extent.height as f32),
                min_depth: 0.0,
                max_depth: 1.0,
            };
            device.cmd_set_viewport(cb, 0, &[viewport]);
            device.cmd_set_scissor(
                cb,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );

            let mut stats = RenderStats::default();
            let mut materials_seen: Vec<bool> = vec![false; self.materials.len()];

            let mut bound_pipeline = vk::Pipeline::null();
            for &i in &order {
                let draw = &input.draws[i];
                let material = &self.materials[draw.material.0];
                let mesh = &self.meshes[draw.mesh.0];

                stats.draw_calls += 1;
                if material.error {
                    stats.error_draws += 1;
                } else if material.features.alpha_mode == AlphaMode::Blend {
                    stats.transparent_draws += 1;
                } else {
                    stats.opaque_draws += 1;
                }
                if let Some(seen) = materials_seen.get_mut(draw.material.0) {
                    if !*seen {
                        *seen = true;
                        stats.materials_drawn += 1;
                    }
                }

                let key = if material.error {
                    PipelineKey::error()
                } else {
                    PipelineKey::from_features(&material.features)
                };
                let pipeline = self.pipeline_cache.get(&device, key)?;
                if pipeline != bound_pipeline {
                    stats.pipeline_binds += 1;
                    device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
                    device.cmd_bind_descriptor_sets(
                        cb,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.pipeline_cache.layout,
                        0,
                        &[frame_set],
                        &[],
                    );
                    bound_pipeline = pipeline;
                }
                device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_cache.layout,
                    1,
                    &[material.set],
                    &[],
                );

                let p = &material.params;
                let push = PushData {
                    model: draw.transform.to_cols_array_2d(),
                    base_color: p.base_color,
                    emission: [
                        p.emission_color[0] * p.emission_intensity,
                        p.emission_color[1] * p.emission_intensity,
                        p.emission_color[2] * p.emission_intensity,
                        0.0,
                    ],
                    params0: [p.metallic, p.roughness, p.toon_steps, p.pbr_toon_blend],
                    params1: [
                        p.alpha_cutoff,
                        p.normal_strength,
                        p.occlusion_strength,
                        draw.highlight,
                    ],
                };
                device.cmd_push_constants(
                    cb,
                    self.pipeline_cache.layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    bytemuck::bytes_of(&push),
                );

                device.cmd_bind_vertex_buffers(cb, 0, &[mesh.vertex_buffer.handle], &[0]);
                device.cmd_bind_index_buffer(
                    cb,
                    mesh.index_buffer.handle,
                    0,
                    vk::IndexType::UINT32,
                );
                device.cmd_draw_indexed(cb, mesh.index_count, 1, 0, 0, 0);
            }

            // Selection outlines: inverted hull pass over highlighted draws.
            let outline_draws: Vec<usize> = (0..input.draws.len())
                .filter(|&i| input.draws[i].highlight > 0.0)
                .collect();
            if !outline_draws.is_empty() {
                stats.outline_draws += outline_draws.len() as u32 * 2;
                stats.draw_calls += outline_draws.len() as u32 * 2;
                stats.pipeline_binds += 2;

                // Depth-only prepass: transparent objects never write depth
                // in the main pass, so without this the hull's interior
                // shows through and the whole object turns purple.
                let depth_pipeline =
                    self.pipeline_cache.get(&device, PipelineKey::depth_only())?;
                device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, depth_pipeline);
                device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_cache.layout,
                    0,
                    &[frame_set],
                    &[],
                );
                for &i in &outline_draws {
                    let draw = &input.draws[i];
                    let material = &self.materials[draw.material.0];
                    let mesh = &self.meshes[draw.mesh.0];
                    device.cmd_bind_descriptor_sets(
                        cb,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.pipeline_cache.layout,
                        1,
                        &[material.set],
                        &[],
                    );
                    let push = PushData {
                        model: draw.transform.to_cols_array_2d(),
                        base_color: [0.0; 4],
                        emission: [0.0; 4],
                        params0: [0.0; 4],
                        params1: [0.0; 4],
                    };
                    device.cmd_push_constants(
                        cb,
                        self.pipeline_cache.layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        bytemuck::bytes_of(&push),
                    );
                    device.cmd_bind_vertex_buffers(cb, 0, &[mesh.vertex_buffer.handle], &[0]);
                    device.cmd_bind_index_buffer(
                        cb,
                        mesh.index_buffer.handle,
                        0,
                        vk::IndexType::UINT32,
                    );
                    device.cmd_draw_indexed(cb, mesh.index_count, 1, 0, 0, 0);
                }

                let pipeline = self.pipeline_cache.get(&device, PipelineKey::outline())?;
                device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
                device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_cache.layout,
                    0,
                    &[frame_set],
                    &[],
                );
                bound_pipeline = pipeline;
                let _ = bound_pipeline;
                for i in outline_draws {
                    let draw = &input.draws[i];
                    let material = &self.materials[draw.material.0];
                    let mesh = &self.meshes[draw.mesh.0];
                    device.cmd_bind_descriptor_sets(
                        cb,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.pipeline_cache.layout,
                        1,
                        &[material.set],
                        &[],
                    );
                    let p = &material.params;
                    let push = PushData {
                        model: draw.transform.to_cols_array_2d(),
                        base_color: p.base_color,
                        emission: [0.0; 4],
                        params0: [0.0; 4],
                        params1: [0.0, 0.0, 0.0, draw.highlight],
                    };
                    device.cmd_push_constants(
                        cb,
                        self.pipeline_cache.layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        bytemuck::bytes_of(&push),
                    );
                    device.cmd_bind_vertex_buffers(cb, 0, &[mesh.vertex_buffer.handle], &[0]);
                    device.cmd_bind_index_buffer(
                        cb,
                        mesh.index_buffer.handle,
                        0,
                        vk::IndexType::UINT32,
                    );
                    device.cmd_draw_indexed(cb, mesh.index_count, 1, 0, 0, 0);
                }
            }

            // egui overlay (manages its own viewport/scissor/pipeline).
            if let Some(egui_draw) = &input.egui {
                self.egui
                    .as_mut()
                    .unwrap()
                    .cmd_draw(
                        cb,
                        extent,
                        egui_draw.pixels_per_point,
                        &egui_draw.primitives,
                    )
                    .map_err(|e| anyhow::anyhow!("recording egui draw: {e}"))?;
            }

            device.cmd_end_rendering(cb);

            image_barrier(
                &device,
                cb,
                image,
                vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::PRESENT_SRC_KHR,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::BOTTOM_OF_PIPE,
                vk::AccessFlags2::empty(),
            );

            stats.pipeline_variants = self.pipeline_cache.variant_count();
            self.last_stats = stats;

            device.end_command_buffer(cb)?;

            let wait_infos = [vk::SemaphoreSubmitInfo::default()
                .semaphore(image_available)
                .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
            let cb_infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
            let signal_infos = [vk::SemaphoreSubmitInfo::default()
                .semaphore(render_finished)
                .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
            let submit = vk::SubmitInfo2::default()
                .wait_semaphore_infos(&wait_infos)
                .command_buffer_infos(&cb_infos)
                .signal_semaphore_infos(&signal_infos);
            device.queue_submit2(self.ctx.queue, &[submit], in_flight)?;
        }

        match self
            .swapchain
            .present(self.ctx.queue, image_index, render_finished)
        {
            Ok(suboptimal) => {
                if suboptimal {
                    self.needs_resize = true;
                }
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.needs_resize = true,
            Err(e) => return Err(e).context("presenting swapchain image"),
        }

        if let Some(egui_draw) = &input.egui {
            if !egui_draw.textures_delta.free.is_empty() {
                self.egui_free_queue
                    .push_back(egui_draw.textures_delta.free.clone());
            }
        }

        self.frame_index = (self.frame_index + 1) % FRAMES_IN_FLIGHT;
        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let device = &self.ctx.device;
        unsafe {
            let _ = device.device_wait_idle();
        }

        // egui renderer first: it frees its allocations through the shared
        // allocator, which must still be alive.
        drop(self.egui.take());

        let allocator = self.allocator.as_ref().unwrap().clone();
        {
            let mut alloc = allocator.lock().unwrap();
            for mesh in &mut self.meshes {
                mesh.vertex_buffer.destroy(device, &mut alloc);
                mesh.index_buffer.destroy(device, &mut alloc);
            }
            for texture in &mut self.textures {
                texture.destroy(device, &mut alloc);
            }
            for texture in &mut self.default_textures {
                texture.destroy(device, &mut alloc);
            }
            for ubo in &mut self.frame_ubos {
                ubo.destroy(device, &mut alloc);
            }
            self.depth.destroy(device, &mut alloc);
        }

        unsafe {
            device.destroy_sampler(self.sampler, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_pool(self.material_pool, None);
        }
        self.pipeline_cache.destroy(device);
        for frame in &mut self.frames {
            frame.destroy(device);
        }
        unsafe {
            device.destroy_command_pool(self.command_pool, None);
        }
        self.swapchain.destroy(device);

        drop(allocator);
        self.allocator = None; // Allocator drops here, before GpuContext's device
    }
}

#[allow(clippy::too_many_arguments)]
fn image_barrier(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    aspect: vk::ImageAspectFlags,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags2,
    src_access: vk::AccessFlags2,
    dst_stage: vk::PipelineStageFlags2,
    dst_access: vk::AccessFlags2,
) {
    let image_barrier = vk::ImageMemoryBarrier2::default()
        .image(image)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(aspect)
                .level_count(1)
                .layer_count(1),
        );
    let barriers = [image_barrier];
    let dependency = vk::DependencyInfo::default().image_memory_barriers(&barriers);
    unsafe {
        device.cmd_pipeline_barrier2(cb, &dependency);
    }
}
