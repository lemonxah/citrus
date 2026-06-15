//! Shader-variant pipeline cache for the citrus standard shader.
//!
//! Each material's feature set maps to a `PipelineKey`; variants are built
//! on demand via SPIR-V specialization constants, so disabled features
//! compile out of the fragment shader entirely.

use std::collections::HashMap;
use std::io::Cursor;

use anyhow::{Context as _, Result};
use ash::vk;

use crate::types::{AlphaMode, MaterialFeatures, Vertex};

const VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/standard.vert.spv"));
const FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/standard.frag.spv"));
const ERROR_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/error.frag.spv"));
const OUTLINE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/outline.vert.spv"));
const OUTLINE_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/outline.frag.spv"));
const SKYBOX_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/skybox.vert.spv"));
const SKYBOX_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/skybox.frag.spv"));
const SHADOW_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shadow.vert.spv"));

pub(crate) const PUSH_CONSTANT_SIZE: u32 = 128;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct PipelineKey {
    pub toon: bool,
    pub normal_map: bool,
    pub emission: bool,
    pub alpha_mode: u32,
    pub double_sided: bool,
    /// Error/missing-material fallback: pink swirl, ignores other flags.
    pub error: bool,
    /// Selection outline: inverted hull, front-face culled flat purple.
    pub outline: bool,
    /// Depth-only prepass (no color writes); masks outline interiors for
    /// transparent objects, which never write depth in the main pass.
    pub depth_only: bool,
    /// Fullscreen skybox background (no vertex buffer, depth-test only).
    pub skybox: bool,
    /// Shadow depth pass: vertex-only, depth into a shadow map (no color).
    pub shadow: bool,
    /// 0 = standard shader; otherwise a custom fragment shader
    /// (`ShaderId` + 1). Custom variants skip specialization constants.
    pub shader: u32,
}

impl PipelineKey {
    pub fn from_features(f: &MaterialFeatures) -> Self {
        Self {
            toon: f.toon,
            normal_map: f.normal_map,
            emission: f.emission,
            alpha_mode: match f.alpha_mode {
                AlphaMode::Opaque => 0,
                AlphaMode::Cutout => 1,
                AlphaMode::Blend => 2,
            },
            double_sided: f.double_sided,
            error: false,
            outline: false,
            depth_only: false,
            skybox: false,
            shadow: false,
            shader: 0,
        }
    }

    pub fn error() -> Self {
        Self {
            toon: false,
            normal_map: false,
            emission: false,
            alpha_mode: 0,
            double_sided: true,
            error: true,
            outline: false,
            depth_only: false,
            skybox: false,
            shadow: false,
            shader: 0,
        }
    }

    pub fn outline() -> Self {
        Self {
            toon: false,
            normal_map: false,
            emission: false,
            alpha_mode: 0,
            double_sided: false,
            error: false,
            outline: true,
            depth_only: false,
            skybox: false,
            shadow: false,
            shader: 0,
        }
    }

    pub fn depth_only() -> Self {
        Self {
            toon: false,
            normal_map: false,
            emission: false,
            alpha_mode: 0,
            double_sided: false,
            error: false,
            outline: false,
            depth_only: true,
            skybox: false,
            shadow: false,
            shader: 0,
        }
    }

    pub fn skybox() -> Self {
        Self {
            toon: false,
            normal_map: false,
            emission: false,
            alpha_mode: 0,
            double_sided: false,
            error: false,
            outline: false,
            depth_only: false,
            skybox: true,
            shadow: false,
            shader: 0,
        }
    }

    pub fn shadow() -> Self {
        Self {
            toon: false,
            normal_map: false,
            emission: false,
            alpha_mode: 0,
            double_sided: false,
            error: false,
            outline: false,
            depth_only: false,
            skybox: false,
            shadow: true,
            shader: 0,
        }
    }
}

pub(crate) struct PipelineCache {
    vert: vk::ShaderModule,
    frag: vk::ShaderModule,
    error_frag: vk::ShaderModule,
    outline_vert: vk::ShaderModule,
    outline_frag: vk::ShaderModule,
    skybox_vert: vk::ShaderModule,
    skybox_frag: vk::ShaderModule,
    shadow_vert: vk::ShaderModule,
    /// Runtime-registered custom fragment shaders, indexed by `ShaderId`.
    custom_frags: Vec<vk::ShaderModule>,
    pub set0_layout: vk::DescriptorSetLayout,
    pub set1_layout: vk::DescriptorSetLayout,
    pub layout: vk::PipelineLayout,
    color_format: vk::Format,
    depth_format: vk::Format,
    pipelines: HashMap<PipelineKey, vk::Pipeline>,
}

impl PipelineCache {
    pub fn new(
        device: &ash::Device,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self> {
        let vert_code = ash::util::read_spv(&mut Cursor::new(VERT_SPV))?;
        let frag_code = ash::util::read_spv(&mut Cursor::new(FRAG_SPV))?;
        let error_code = ash::util::read_spv(&mut Cursor::new(ERROR_FRAG_SPV))?;
        let vert = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&vert_code),
                None,
            )?
        };
        let frag = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&frag_code),
                None,
            )?
        };
        let error_frag = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&error_code),
                None,
            )?
        };
        let outline_vert_code = ash::util::read_spv(&mut Cursor::new(OUTLINE_VERT_SPV))?;
        let outline_frag_code = ash::util::read_spv(&mut Cursor::new(OUTLINE_FRAG_SPV))?;
        let outline_vert = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&outline_vert_code),
                None,
            )?
        };
        let outline_frag = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&outline_frag_code),
                None,
            )?
        };
        let skybox_vert_code = ash::util::read_spv(&mut Cursor::new(SKYBOX_VERT_SPV))?;
        let skybox_frag_code = ash::util::read_spv(&mut Cursor::new(SKYBOX_FRAG_SPV))?;
        let skybox_vert = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&skybox_vert_code),
                None,
            )?
        };
        let skybox_frag = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&skybox_frag_code),
                None,
            )?
        };
        let shadow_vert_code = ash::util::read_spv(&mut Cursor::new(SHADOW_VERT_SPV))?;
        let shadow_vert = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&shadow_vert_code),
                None,
            )?
        };

        // set 0: per-frame UBO (binding 0), shadow-map array (binding 1), and
        // the baked light-probe SH storage buffer (binding 2, runtime GI).
        let frame_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            // Baked lightmap array (static-object GI), sampled by uv1 + layer.
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            // Screen-space GI gather result (full-res indirect irradiance).
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let set0_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&frame_bindings),
                None,
            )?
        };

        // set 1: material textures (albedo, normal, ORM, emission)
        // Texture samplers at bindings 0-3 (albedo/normal/orm/emission) and 5-12
        // (opacity, emission mask, 3 matcaps + 3 matcap masks). Binding 4 is the
        // FX uniform block. Every material set writes all of these (create_material
        // + the skybox set), so they're always valid.
        let sampler_bindings: [u32; 12] = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut tex_bindings: Vec<_> = sampler_bindings
            .iter()
            .map(|&i| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(i)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            })
            .collect();
        // Binding 4: per-material "FX" uniform block (rim, animated emission,
        // matcap strengths — extended params beyond the 128-byte push block).
        tex_bindings.push(
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        );
        let set1_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&tex_bindings),
                None,
            )?
        };

        let set_layouts = [set0_layout, set1_layout];
        let push_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .size(PUSH_CONSTANT_SIZE)];
        let layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push_ranges),
                None,
            )?
        };

        Ok(Self {
            vert,
            frag,
            error_frag,
            outline_vert,
            outline_frag,
            skybox_vert,
            skybox_frag,
            shadow_vert,
            custom_frags: Vec::new(),
            set0_layout,
            set1_layout,
            layout,
            color_format,
            depth_format,
            pipelines: HashMap::new(),
        })
    }

    pub fn variant_count(&self) -> u32 {
        self.pipelines.len() as u32
    }

    /// Register a runtime-compiled custom fragment shader; returns its
    /// index. Modules live until the cache is destroyed (hot reload
    /// registers a replacement rather than destroying in-flight modules).
    pub fn register_custom(&mut self, device: &ash::Device, spirv: &[u8]) -> Result<usize> {
        let code =
            ash::util::read_spv(&mut Cursor::new(spirv)).context("reading custom shader SPIR-V")?;
        let module = unsafe {
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?
        };
        self.custom_frags.push(module);
        Ok(self.custom_frags.len() - 1)
    }

    pub fn get(&mut self, device: &ash::Device, key: PipelineKey) -> Result<vk::Pipeline> {
        if let Some(&pipeline) = self.pipelines.get(&key) {
            return Ok(pipeline);
        }
        tracing::debug!(?key, "compiling pipeline variant");
        let pipeline = self.create_variant(device, key)?;
        self.pipelines.insert(key, pipeline);
        Ok(pipeline)
    }

    fn create_variant(&self, device: &ash::Device, key: PipelineKey) -> Result<vk::Pipeline> {
        let spec_data: [u32; 4] = [
            key.toon as u32,
            key.normal_map as u32,
            key.emission as u32,
            key.alpha_mode,
        ];
        let spec_entries: Vec<_> = (0..4u32)
            .map(|id| vk::SpecializationMapEntry {
                constant_id: id,
                offset: id * 4,
                size: 4,
            })
            .collect();
        let spec = vk::SpecializationInfo::default()
            .map_entries(&spec_entries)
            .data(bytemuck::cast_slice(&spec_data));

        let vert_module = if key.shadow {
            self.shadow_vert
        } else if key.skybox {
            self.skybox_vert
        } else if key.outline {
            self.outline_vert
        } else {
            self.vert
        };
        let frag_stage = if key.skybox {
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.skybox_frag)
                .name(c"main")
        } else if key.depth_only {
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.error_frag)
                .name(c"main")
        } else if key.outline {
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.outline_frag)
                .name(c"main")
        } else if key.error {
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.error_frag)
                .name(c"main")
        } else if key.shader > 0 {
            // Custom shaders define no specialization constants.
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.custom_frags[key.shader as usize - 1])
                .name(c"main")
        } else {
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.frag)
                .name(c"main")
                .specialization_info(&spec)
        };
        let vertex_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(c"main");
        // The shadow pass is depth-only: vertex stage alone, no fragment.
        let stages: Vec<vk::PipelineShaderStageCreateInfo> = if key.shadow {
            vec![vertex_stage]
        } else {
            vec![vertex_stage, frag_stage]
        };

        let bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Vertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let attributes = [
            vk::VertexInputAttributeDescription {
                location: 0,
                binding: 0,
                format: vk::Format::R32G32B32_SFLOAT,
                offset: 0,
            },
            vk::VertexInputAttributeDescription {
                location: 1,
                binding: 0,
                format: vk::Format::R32G32B32_SFLOAT,
                offset: 12,
            },
            vk::VertexInputAttributeDescription {
                location: 2,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 24,
            },
            vk::VertexInputAttributeDescription {
                location: 3,
                binding: 0,
                format: vk::Format::R32G32B32A32_SFLOAT,
                offset: 32,
            },
            vk::VertexInputAttributeDescription {
                location: 4,
                binding: 0,
                format: vk::Format::R32G32B32A32_SFLOAT,
                offset: 48,
            },
            // uv1 (lightmap UVs), offset 64 — after tangent.
            vk::VertexInputAttributeDescription {
                location: 5,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 64,
            },
        ];
        // The skybox is a vertex-buffer-less fullscreen triangle.
        let vertex_input = if key.skybox {
            vk::PipelineVertexInputStateCreateInfo::default()
        } else {
            vk::PipelineVertexInputStateCreateInfo::default()
                .vertex_binding_descriptions(&bindings)
                .vertex_attribute_descriptions(&attributes)
        };

        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);

        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(if key.outline {
                // Inverted-hull outline keeps back faces only.
                vk::CullModeFlags::FRONT
            } else if key.shadow || key.double_sided || key.skybox {
                // Shadow pass renders both faces so the closest (light-facing)
                // surface lays the depth. Culling front faces (second-depth)
                // instead leaks a lit line along a caster's light terminator;
                // a slope-scaled bias in the sampler handles the acne.
                vk::CullModeFlags::NONE
            } else {
                vk::CullModeFlags::BACK
            })
            // Counter-clockwise like glTF; the scene viewport flips Y with
            // negative height, preserving winding.
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);

        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        let transparent = key.alpha_mode == 2;
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            // Skybox tests against the cleared far depth but never writes, so
            // geometry drawn afterwards always wins.
            .depth_write_enable(!key.skybox && ((!transparent && !key.outline) || key.depth_only))
            .depth_compare_op(vk::CompareOp::LESS_OR_EQUAL);

        let blend_attachment = if key.depth_only {
            // No color writes; only the depth buffer is touched.
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty())
        } else if transparent {
            vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD)
                .color_write_mask(vk::ColorComponentFlags::RGBA)
        } else {
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)
        };
        // The shadow pass has no color attachment.
        let blend_attachments = [blend_attachment];
        let color_blend = if key.shadow {
            vk::PipelineColorBlendStateCreateInfo::default()
        } else {
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments)
        };

        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        let color_formats = [self.color_format];
        let mut rendering = if key.shadow {
            // Depth-only target: no color attachments.
            vk::PipelineRenderingCreateInfo::default().depth_attachment_format(self.depth_format)
        } else {
            vk::PipelineRenderingCreateInfo::default()
                .color_attachment_formats(&color_formats)
                .depth_attachment_format(self.depth_format)
        };

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
            .layout(self.layout)
            .push_next(&mut rendering);

        let pipeline = unsafe {
            device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
                .map_err(|(_, e)| e)
        }
        .context("creating standard shader pipeline variant")?[0];
        Ok(pipeline)
    }

    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            for (_, pipeline) in self.pipelines.drain() {
                device.destroy_pipeline(pipeline, None);
            }
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_set_layout(self.set0_layout, None);
            device.destroy_descriptor_set_layout(self.set1_layout, None);
            device.destroy_shader_module(self.vert, None);
            device.destroy_shader_module(self.frag, None);
            device.destroy_shader_module(self.error_frag, None);
            device.destroy_shader_module(self.outline_vert, None);
            device.destroy_shader_module(self.outline_frag, None);
            device.destroy_shader_module(self.skybox_vert, None);
            device.destroy_shader_module(self.skybox_frag, None);
            device.destroy_shader_module(self.shadow_vert, None);
            for module in self.custom_frags.drain(..) {
                device.destroy_shader_module(module, None);
            }
        }
    }
}
