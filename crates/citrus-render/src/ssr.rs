//! Deferred screen-space-reflection resolve pass.
//!
//! Runs fullscreen after the forward pass. The forward pass renders the lit HDR
//! colour (with the env-cube reflection already composited) plus a reflectance +
//! roughness G-buffer. This pass marches the reflection ray against the depth
//! prepass and the CURRENT-frame colour (no 1-frame lag), then writes a resolved
//! HDR target the post pass tonemaps. See `ssr_resolve.frag`.
//!
//! [`SsrResolve`] owns the shared pipeline/layout/sampler. Each render target
//! (the game swapchain path and the editor viewport each have one) owns an
//! [`SsrTarget`]: its G-buffer, resolved HDR image, descriptor sets and uniform
//! buffer, with its own tiny pool so resizes are independent.

use std::io::Cursor;

use anyhow::Result;
use ash::vk;
use bytemuck::{Pod, Zeroable};
use gpu_allocator::vulkan::{Allocation, Allocator};

use crate::alloc::Buffer;
use crate::post::HDR_FORMAT;
use crate::{GBUF_FORMAT, create_image, create_view, image_barrier};

const FULLSCREEN_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fullscreen.vert.spv"));
const SSR_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ssr_resolve.frag.spv"));

/// Per-frame uniforms for the resolve shader. Mirrors `SsrData` in
/// `ssr_resolve.frag` (std140: mat4 + vec4 are all 16-byte aligned).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct SsrUniforms {
    pub proj: [[f32; 4]; 4],
    pub inv_proj: [[f32; 4]; 4],
    pub view: [[f32; 4]; 4],
    pub inv_view: [[f32; 4]; 4],
    pub camera_pos: [f32; 4],
    pub ssr: [f32; 4],
    pub refl_center: [f32; 4],
    pub refl_extents: [f32; 4],
    pub fog_color: [f32; 4],
    pub fog_params: [f32; 4],
    /// Sun in-scatter for volumetric fog: rgb = directional light colour*intensity,
    /// w = Henyey-Greenstein anisotropy (forward-scatter glow toward the sun).
    pub fog_light: [f32; 4],
    /// xyz = direction TO the sun (world), w = time (animates the fog noise).
    pub fog_sun: [f32; 4],
    pub screen: [f32; 4],
}

/// Per-render-target SSR resources.
pub(crate) struct SsrTarget {
    /// Reflectance.rgb + roughness.a, written by the forward pass (MRT slot 1).
    pub gbuf_image: vk::Image,
    pub gbuf_view: vk::ImageView,
    gbuf_alloc: Option<Allocation>,
    /// Resolved HDR colour (scene + screen-space reflections); fed to the post pass.
    resolved_image: vk::Image,
    resolved_view: vk::ImageView,
    resolved_alloc: Option<Allocation>,
    /// Resolve-pass inputs (binding 0 colour, 1 gbuf, 2 depth, 3 env, 4 ubo).
    resolve_set: vk::DescriptorSet,
    /// Post-pass source set (binding 0 = resolved HDR), allocated with the post
    /// pass's set layout so `PostPass::record_set` can sample the resolved image.
    pub post_set: vk::DescriptorSet,
    ubo: Buffer,
    pool: vk::DescriptorPool,
    extent: vk::Extent2D,
}

impl SsrTarget {
    /// The resolved HDR image/view (post reads it). Also the RT reflection
    /// compute pass's storage output.
    pub fn resolved_image(&self) -> vk::Image {
        self.resolved_image
    }
    pub fn resolved_view(&self) -> vk::ImageView {
        self.resolved_view
    }

    pub fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_image_view(self.gbuf_view, None);
            device.destroy_image(self.gbuf_image, None);
            device.destroy_image_view(self.resolved_view, None);
            device.destroy_image(self.resolved_image, None);
            device.destroy_descriptor_pool(self.pool, None);
        }
        if let Some(a) = self.gbuf_alloc.take() {
            let _ = allocator.free(a);
        }
        if let Some(a) = self.resolved_alloc.take() {
            let _ = allocator.free(a);
        }
        self.ubo.destroy(device, allocator);
    }
}

pub(crate) struct SsrResolve {
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
}

impl SsrResolve {
    pub fn new(device: &ash::Device) -> Result<Self> {
        // Bindings: 0 scene colour, 1 gbuf, 2 depth, 3 env cube, 4 uniforms.
        let sampler_binding = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        };
        let bindings = [
            sampler_binding(0),
            sampler_binding(1),
            sampler_binding(2),
            sampler_binding(3),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?
        };
        let layouts = [set_layout];
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts),
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
                    .code(&ash::util::read_spv(&mut Cursor::new(SSR_FRAG_SPV))?),
                None,
            )?
        };
        let pipeline = Self::build_pipeline(device, pipeline_layout, vert, frag)?;
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

        Ok(Self {
            set_layout,
            pipeline_layout,
            pipeline,
            sampler,
        })
    }

    fn build_pipeline(
        device: &ash::Device,
        layout: vk::PipelineLayout,
        vert: vk::ShaderModule,
        frag: vk::ShaderModule,
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
        let color_formats = [HDR_FORMAT];
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

    /// Allocate one target's resources (G-buffer + resolved HDR + sets + ubo).
    /// `post_set_layout` is the post pass's set layout so the resolved image can
    /// be sampled by `PostPass::record_set`.
    pub fn create_target(
        &self,
        device: &ash::Device,
        allocator: &mut Allocator,
        post_set_layout: vk::DescriptorSetLayout,
        extent: vk::Extent2D,
    ) -> Result<SsrTarget> {
        let (gbuf_image, gbuf_alloc) = create_image(
            device,
            allocator,
            GBUF_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            extent,
            "ssr gbuffer",
        )?;
        let gbuf_view = create_view(device, gbuf_image, GBUF_FORMAT, vk::ImageAspectFlags::COLOR)?;
        let (resolved_image, resolved_alloc) = create_image(
            device,
            allocator,
            HDR_FORMAT,
            // STORAGE too: the ray-traced reflection compute pass writes this image.
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::STORAGE,
            extent,
            "ssr resolved",
        )?;
        let resolved_view =
            create_view(device, resolved_image, HDR_FORMAT, vk::ImageAspectFlags::COLOR)?;

        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default().max_sets(2).pool_sizes(&[
                    vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        descriptor_count: 5,
                    },
                    vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::UNIFORM_BUFFER,
                        descriptor_count: 1,
                    },
                ]),
                None,
            )?
        };
        let layouts = [self.set_layout, post_set_layout];
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )?
        };
        let resolve_set = sets[0];
        let post_set = sets[1];

        let ubo = Buffer::new(
            device,
            allocator,
            std::mem::size_of::<SsrUniforms>() as u64,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            gpu_allocator::MemoryLocation::CpuToGpu,
            "ssr uniforms",
        )?;

        // Static bindings: gbuf (1) + ubo (4) on the resolve set, resolved (0) on
        // the post set. Colour (0), depth (2) and env (3) are written per-frame.
        let gbuf_info = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(gbuf_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let ubo_info = [vk::DescriptorBufferInfo::default()
            .buffer(ubo.handle)
            .offset(0)
            .range(std::mem::size_of::<SsrUniforms>() as u64)];
        let resolved_info = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(resolved_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(resolve_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&gbuf_info),
            vk::WriteDescriptorSet::default()
                .dst_set(resolve_set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(&ubo_info),
            vk::WriteDescriptorSet::default()
                .dst_set(post_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&resolved_info),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        Ok(SsrTarget {
            gbuf_image,
            gbuf_view,
            gbuf_alloc: Some(gbuf_alloc),
            resolved_image,
            resolved_view,
            resolved_alloc: Some(resolved_alloc),
            resolve_set,
            post_set,
            ubo,
            pool,
            extent,
        })
    }

    /// Record the resolve pass for `target`. Inputs must already be in
    /// SHADER_READ_ONLY: `color_view` (forward HDR), the target's gbuf,
    /// `depth_view` (depth prepass), `env_view` (environment cube). On return the
    /// resolved image is SHADER_READ_ONLY for the post pass to sample via
    /// `target.post_set`.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        target: &mut SsrTarget,
        color_view: vk::ImageView,
        depth_view: vk::ImageView,
        env_view: vk::ImageView,
        uniforms: &SsrUniforms,
    ) {
        target.ubo.write(0, bytemuck::bytes_of(uniforms));
        let extent = target.extent;

        // Per-frame inputs: colour (0), depth (2), env cube (3).
        let color_info = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(color_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let depth_info = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(depth_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let env_info = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(env_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(target.resolve_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&color_info),
            vk::WriteDescriptorSet::default()
                .dst_set(target.resolve_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&depth_info),
            vk::WriteDescriptorSet::default()
                .dst_set(target.resolve_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&env_info),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        image_barrier(
            device,
            cb,
            target.resolved_image,
            vk::ImageAspectFlags::COLOR,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::TOP_OF_PIPE,
            vk::AccessFlags2::empty(),
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        );

        unsafe {
            let att = vk::RenderingAttachmentInfo::default()
                .image_view(target.resolved_view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::DONT_CARE)
                .store_op(vk::AttachmentStoreOp::STORE);
            let atts = [att];
            let info = vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                })
                .layer_count(1)
                .color_attachments(&atts);
            device.cmd_begin_rendering(cb, &info);
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
                &[target.resolve_set],
                &[],
            );
            device.cmd_draw(cb, 3, 1, 0, 0);
            device.cmd_end_rendering(cb);
        }

        image_barrier(
            device,
            cb,
            target.resolved_image,
            vk::ImageAspectFlags::COLOR,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_READ,
        );
    }

    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
    }
}
