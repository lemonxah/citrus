//! HDR scene target + fullscreen post-processing pass.
//!
//! The forward pass renders linear HDR into [`PostPass`]'s per-frame
//! `R16G16B16A16_SFLOAT` target (no inline tonemap). This pass then samples it
//! and applies bloom + chromatic aberration + exposure + colour grading +
//! tonemap + vignette (`post.frag`), writing display-space colour to the
//! swapchain. One target per frame-in-flight so a frame's write never races the
//! previous frame's read.

use std::io::Cursor;

use anyhow::Result;
use ash::vk;
use bytemuck::{Pod, Zeroable};
use gpu_allocator::vulkan::{Allocation, Allocator};

use crate::PostFx;

const FULLSCREEN_VERT_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/fullscreen.vert.spv"));
const POST_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/post.frag.spv"));

/// Linear HDR scene-colour format.
pub(crate) const HDR_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PostPush {
    p0: [f32; 4], // tonemap mode, exposure EV, grade exposure, contrast
    p1: [f32; 4], // saturation, temperature, tint, grading enabled
    p2: [f32; 4], // vignette enabled, intensity, smoothness, _
    p3: [f32; 4], // vignette color rgb, _
    p4: [f32; 4], // bloom enabled, threshold, intensity, radius
    p5: [f32; 4], // CA enabled, CA intensity, _, _
}

struct HdrTarget {
    image: vk::Image,
    view: vk::ImageView,
    alloc: Option<Allocation>,
}

pub(crate) struct PostPass {
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    targets: Vec<HdrTarget>,
    extent: vk::Extent2D,
}

impl PostPass {
    pub fn new(
        device: &ash::Device,
        allocator: &mut Allocator,
        swapchain_format: vk::Format,
        extent: vk::Extent2D,
        frames: usize,
    ) -> Result<Self> {
        let set_bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&set_bindings),
                None,
            )?
        };
        let push_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .size(std::mem::size_of::<PostPush>() as u32)];
        let layouts = [set_layout];
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&push_ranges),
                None,
            )?
        };

        let vert = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default()
                    .code(&ash::util::read_spv(&mut Cursor::new(FULLSCREEN_VERT_SPV))?),
                None,
            )?
        };
        let frag = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default()
                    .code(&ash::util::read_spv(&mut Cursor::new(POST_FRAG_SPV))?),
                None,
            )?
        };
        let pipeline = Self::build_pipeline(device, pipeline_layout, vert, frag, swapchain_format)?;
        unsafe {
            device.destroy_shader_module(vert, None);
            device.destroy_shader_module(frag, None);
        }

        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )?
        };

        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(frames as u32)
                    .pool_sizes(&[vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        descriptor_count: frames as u32,
                    }]),
                None,
            )?
        };

        let mut pass = Self {
            set_layout,
            pipeline_layout,
            pipeline,
            sampler,
            pool,
            sets: Vec::new(),
            targets: Vec::new(),
            extent,
        };
        pass.create_targets(device, allocator, extent, frames)?;
        Ok(pass)
    }

    fn build_pipeline(
        device: &ash::Device,
        layout: vk::PipelineLayout,
        vert: vk::ShaderModule,
        frag: vk::ShaderModule,
        color_format: vk::Format,
    ) -> Result<vk::Pipeline> {
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag)
                .name(c"main"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default();
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [color_format];
        let mut rendering =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .push_next(&mut rendering);
        let pipeline = unsafe {
            device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
                .map_err(|(_, e)| e)?[0]
        };
        Ok(pipeline)
    }

    fn create_targets(
        &mut self,
        device: &ash::Device,
        allocator: &mut Allocator,
        extent: vk::Extent2D,
        frames: usize,
    ) -> Result<()> {
        let layouts = vec![self.set_layout; frames];
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(&layouts),
            )?
        };
        for &set in &sets {
            let (image, alloc) = crate::create_image(
                device,
                allocator,
                HDR_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                extent,
                "scene hdr",
            )?;
            let view = crate::create_view(device, image, HDR_FORMAT, vk::ImageAspectFlags::COLOR)?;
            let info = [vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { device.update_descriptor_sets(&[write], &[]) };
            self.targets.push(HdrTarget {
                image,
                view,
                alloc: Some(alloc),
            });
        }
        self.sets = sets;
        self.extent = extent;
        Ok(())
    }

    pub fn hdr_image(&self, frame: usize) -> vk::Image {
        self.targets[frame].image
    }
    pub fn hdr_view(&self, frame: usize) -> vk::ImageView {
        self.targets[frame].view
    }

    /// The descriptor set layout (1 sampled HDR image) + sampler, so other
    /// targets (the editor viewport) can allocate their own post source set.
    pub fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }
    pub fn sampler(&self) -> vk::Sampler {
        self.sampler
    }

    /// Record the fullscreen post pass: samples the HDR target for `frame` (must
    /// already be in SHADER_READ_ONLY) and writes display colour to the active
    /// dynamic-rendering attachment (the swapchain). Bind nothing else first.
    pub fn record(
        &self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        frame: usize,
        extent: vk::Extent2D,
        post: &PostFx,
    ) {
        self.record_set(device, cb, self.sets[frame], extent, post);
    }

    /// Run the post pass sampling an externally-provided descriptor set (binding
    /// 0 = the HDR source). Used by the editor viewport, which owns its own HDR
    /// target + set.
    pub fn record_set(
        &self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        set: vk::DescriptorSet,
        extent: vk::Extent2D,
        post: &PostFx,
    ) {
        let push = PostPush {
            p0: [post.tonemap as f32, post.exposure, post.grade_exposure, post.contrast],
            p1: [
                post.saturation,
                post.temperature,
                post.tint,
                if post.grading_enabled { 1.0 } else { 0.0 },
            ],
            p2: [
                if post.vignette_enabled { 1.0 } else { 0.0 },
                post.vignette_intensity,
                post.vignette_smoothness,
                0.0,
            ],
            p3: [post.vignette_color[0], post.vignette_color[1], post.vignette_color[2], 0.0],
            p4: [
                if post.bloom_enabled { 1.0 } else { 0.0 },
                post.bloom_threshold,
                post.bloom_intensity,
                post.bloom_radius,
            ],
            p5: [if post.ca_enabled { 1.0 } else { 0.0 }, post.ca_intensity, 0.0, 0.0],
        };
        unsafe {
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: extent.width as f32,
                height: extent.height as f32,
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
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[set],
                &[],
            );
            device.cmd_push_constants(
                cb,
                self.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                bytemuck::bytes_of(&push),
            );
            device.cmd_draw(cb, 3, 1, 0, 0);
        }
    }

    pub fn resize(
        &mut self,
        device: &ash::Device,
        allocator: &mut Allocator,
        extent: vk::Extent2D,
    ) -> Result<()> {
        let frames = self.targets.len();
        self.destroy_targets(device, allocator);
        unsafe {
            device.reset_descriptor_pool(self.pool, vk::DescriptorPoolResetFlags::empty())?;
        }
        self.create_targets(device, allocator, extent, frames)
    }

    fn destroy_targets(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        for t in &mut self.targets {
            unsafe {
                device.destroy_image_view(t.view, None);
                device.destroy_image(t.image, None);
            }
            if let Some(a) = t.alloc.take() {
                let _ = allocator.free(a);
            }
        }
        self.targets.clear();
    }

    pub fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        self.destroy_targets(device, allocator);
        unsafe {
            device.destroy_descriptor_pool(self.pool, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}
