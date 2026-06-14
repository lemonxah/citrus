//! GPU software-GI probe march. Runs `sw_gi.comp` over the merged Global
//! Distance Field (a 3D distance texture + a nearest-instance index texture)
//! to produce SH-L1 radiance + directional-distance probes — the same packed
//! layout the standard shader samples.
//!
//! The GDF (distance + index textures + per-instance materials) is **cached** and
//! only rebuilt/re-uploaded via `set_gdf` when geometry moves; `march` then runs
//! every trace against the cached GDF, creating only the small per-trace buffers
//! (lights/probes/output). That lets a static scene keep a high-resolution GDF
//! for free (sharp, un-faceted occlusion) instead of re-uploading it each frame.

use std::io::Cursor;
use std::mem::size_of;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use ash::vk;
use bytemuck::{Pod, Zeroable};
use glam::Vec3;
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::Allocator;

use crate::alloc::{self, Buffer};
use crate::context::GpuContext;
use crate::texture::GpuTexture;
use crate::types::{BakeLight, LightKind, ProbeSh};

const SW_GI_COMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sw_gi.comp.spv"));

/// Per-instance material the march shades GDF hits with (looked up by the index
/// texture). Albedo drives the diffuse bounce; emission makes the surface a
/// light source.
#[derive(Clone, Copy)]
pub struct GpuGiMaterial {
    pub albedo: [f32; 3],
    pub emission: [f32; 3],
}

/// An emissive instance reduced to a sphere area-light, sampled directly (NEE) by
/// the march so emitter fill is smooth instead of blotchy Monte-Carlo noise.
#[derive(Clone, Copy)]
pub struct GpuGiEmitter {
    pub center: [f32; 3],
    pub radius: f32,
    pub emission: [f32; 3],
}

/// Per-trace march inputs (lights/probes/params — change every trace; the GDF is
/// cached separately via `set_gdf`).
pub struct GpuGiMarch<'a> {
    pub lights: &'a [BakeLight],
    pub emitters: &'a [GpuGiEmitter],
    pub probes: &'a [Vec3],
    pub samples: u32,
    pub bounces: u32,
    pub sky: [f32; 3],
    pub eps: f32,
    pub max_dist: f32,
    pub seed: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GiInst {
    albedo: [f32; 4],
    emission: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GiLight {
    pos_kind: [f32; 4],
    dir_range: [f32; 4],
    color: [f32; 4],
    spot: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GiEmitter {
    center_radius: [f32; 4],
    emission: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GiPush {
    gdf_min: [f32; 4],
    gdf_max: [f32; 4],
    sky: [f32; 4],
    counts: [u32; 4], // samples, bounces, light_count, probe_count
    march: [f32; 4],  // eps, max_dist, _, _
    misc: [u32; 4],   // seed, _, _, _
}

fn pack_light(l: &BakeLight) -> GiLight {
    let kind = match l.kind {
        LightKind::Directional => 0.0,
        LightKind::Point => 1.0,
        LightKind::Spot => 2.0,
    };
    GiLight {
        pos_kind: [l.position.x, l.position.y, l.position.z, kind],
        dir_range: [l.direction.x, l.direction.y, l.direction.z, l.range],
        color: [l.color[0], l.color[1], l.color[2], 0.0],
        spot: [l.spot_inner_deg, l.spot_outer_deg, l.radius, 0.0],
    }
}

/// Cached GPU-side GDF: the two 3D textures + per-instance materials + bounds.
/// Persists across traces; replaced only when geometry changes.
struct CachedGdf {
    dist_tex: GpuTexture,
    index_tex: GpuTexture,
    inst_buf: Buffer,
    min: [f32; 3],
    max: [f32; 3],
}

impl CachedGdf {
    fn destroy(&mut self, device: &ash::Device, alloc: &mut Allocator) {
        self.dist_tex.destroy(device, alloc);
        self.index_tex.destroy(device, alloc);
        self.inst_buf.destroy(device, alloc);
    }
}

/// Persistent compute pipeline + samplers + cached GDF for the GI march.
pub struct GpuGi {
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    dist_sampler: vk::Sampler,  // trilinear, clamp
    index_sampler: vk::Sampler, // nearest, clamp
    gdf: Option<CachedGdf>,
}

impl GpuGi {
    pub fn new(device: &ash::Device) -> Result<Self> {
        let stage = vk::ShaderStageFlags::COMPUTE;
        let sampled = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(stage)
        };
        let ssbo = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(stage)
        };
        let bindings = [sampled(0), sampled(1), ssbo(2), ssbo(3), ssbo(4), ssbo(5), ssbo(6)];
        let set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?
        };
        let set_layouts = [set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(stage)
            .size(size_of::<GiPush>() as u32)];
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push),
                None,
            )?
        };
        let code = ash::util::read_spv(&mut Cursor::new(SW_GI_COMP))?;
        let module = unsafe {
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?
        };
        let pipeline = unsafe {
            device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(
                            vk::PipelineShaderStageCreateInfo::default()
                                .stage(stage)
                                .module(module)
                                .name(c"main"),
                        )
                        .layout(pipeline_layout)],
                    None,
                )
                .map_err(|(_, e)| e)?[0]
        };
        unsafe { device.destroy_shader_module(module, None) };

        let mk_sampler = |filter: vk::Filter| -> Result<vk::Sampler> {
            let info = vk::SamplerCreateInfo::default()
                .mag_filter(filter)
                .min_filter(filter)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
            Ok(unsafe { device.create_sampler(&info, None)? })
        };
        let dist_sampler = mk_sampler(vk::Filter::LINEAR)?;
        let index_sampler = mk_sampler(vk::Filter::NEAREST)?;

        Ok(Self {
            set_layout,
            pipeline_layout,
            pipeline,
            dist_sampler,
            index_sampler,
            gdf: None,
        })
    }

    /// (Re)upload the GDF — distance + index 3D textures and per-instance
    /// materials — replacing any cached one. Call only when geometry changes.
    #[allow(clippy::too_many_arguments)]
    pub fn set_gdf(
        &mut self,
        ctx: &GpuContext,
        allocator: &Arc<Mutex<Allocator>>,
        command_pool: vk::CommandPool,
        dist: &[f32],
        index: &[u32],
        dims: [u32; 3],
        min: [f32; 3],
        max: [f32; 3],
        materials: &[GpuGiMaterial],
    ) -> Result<()> {
        let device = &ctx.device;
        let mut alloc = allocator.lock().unwrap();
        let dist_tex =
            GpuTexture::upload_volume(device, &mut alloc, command_pool, ctx.queue, dist, dims)?;
        let index_tex =
            GpuTexture::upload_volume_u32(device, &mut alloc, command_pool, ctx.queue, index, dims)?;
        let insts: Vec<GiInst> = if materials.is_empty() {
            vec![GiInst::zeroed()]
        } else {
            materials
                .iter()
                .map(|m| GiInst {
                    albedo: [m.albedo[0], m.albedo[1], m.albedo[2], 0.0],
                    emission: [m.emission[0], m.emission[1], m.emission[2], 0.0],
                })
                .collect()
        };
        let inst_buf = host_ssbo(device, &mut alloc, bytemuck::cast_slice(&insts), "gi insts")?;
        if let Some(mut old) = self.gdf.take() {
            old.destroy(device, &mut alloc);
        }
        self.gdf = Some(CachedGdf {
            dist_tex,
            index_tex,
            inst_buf,
            min,
            max,
        });
        Ok(())
    }

    /// March the probes against the cached GDF; one `ProbeSh` per probe. Returns
    /// empty if no GDF has been uploaded yet.
    pub fn march(
        &self,
        ctx: &GpuContext,
        allocator: &Arc<Mutex<Allocator>>,
        command_pool: vk::CommandPool,
        m: &GpuGiMarch<'_>,
    ) -> Result<Vec<ProbeSh>> {
        let Some(gdf) = self.gdf.as_ref() else {
            return Ok(Vec::new());
        };
        let device = &ctx.device;
        let count = m.probes.len();
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut alloc = allocator.lock().unwrap();

        let lights: Vec<GiLight> = if m.lights.is_empty() {
            vec![GiLight::zeroed()]
        } else {
            m.lights.iter().map(pack_light).collect()
        };
        let probes: Vec<[f32; 4]> = m.probes.iter().map(|p| [p.x, p.y, p.z, 0.0]).collect();
        let emitters: Vec<GiEmitter> = if m.emitters.is_empty() {
            vec![GiEmitter::zeroed()]
        } else {
            m.emitters
                .iter()
                .map(|e| GiEmitter {
                    center_radius: [e.center[0], e.center[1], e.center[2], e.radius],
                    emission: [e.emission[0], e.emission[1], e.emission[2], 0.0],
                })
                .collect()
        };
        let mut light_buf =
            host_ssbo(device, &mut alloc, bytemuck::cast_slice(&lights), "gi lights")?;
        let mut probe_buf =
            host_ssbo(device, &mut alloc, bytemuck::cast_slice(&probes), "gi probes")?;
        let mut emitter_buf =
            host_ssbo(device, &mut alloc, bytemuck::cast_slice(&emitters), "gi emitters")?;
        let mut out_buf = Buffer::new(
            device,
            &mut alloc,
            (count * size_of::<[f32; 16]>()) as u64,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::GpuToCpu,
            "gi out",
        )?;

        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 2,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 5,
            },
        ];
        let desc_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )?
        };
        let set_layouts = [self.set_layout];
        let set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(desc_pool)
                    .set_layouts(&set_layouts),
            )?[0]
        };
        self.write_image(device, set, 0, self.dist_sampler, gdf.dist_tex.view);
        self.write_image(device, set, 1, self.index_sampler, gdf.index_tex.view);
        write_ssbo(device, set, 2, gdf.inst_buf.handle);
        write_ssbo(device, set, 3, light_buf.handle);
        write_ssbo(device, set, 4, probe_buf.handle);
        write_ssbo(device, set, 5, out_buf.handle);
        write_ssbo(device, set, 6, emitter_buf.handle);

        let push = GiPush {
            gdf_min: [gdf.min[0], gdf.min[1], gdf.min[2], 0.0],
            gdf_max: [gdf.max[0], gdf.max[1], gdf.max[2], 0.0],
            sky: [m.sky[0], m.sky[1], m.sky[2], 0.0],
            counts: [
                m.samples.max(1),
                m.bounces.max(1),
                m.lights.len() as u32,
                count as u32,
            ],
            march: [m.eps, m.max_dist, 0.0, 0.0],
            misc: [m.seed, m.emitters.len() as u32, 0, 0],
        };
        let groups = (count as u32).div_ceil(64);
        alloc::one_time_submit(device, command_pool, ctx.queue, |cb| unsafe {
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[set],
                &[],
            );
            device.cmd_push_constants(
                cb,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(&push),
            );
            device.cmd_dispatch(cb, groups, 1, 1);
        })?;

        let bytes = out_buf.read();
        let raw: &[f32] = bytemuck::cast_slice(&bytes);
        let mut out = Vec::with_capacity(count);
        for p in 0..count {
            let b = p * 16;
            let mut sh = ProbeSh::default();
            for k in 0..4 {
                sh.coeffs[k] = [raw[b + k * 4], raw[b + k * 4 + 1], raw[b + k * 4 + 2]];
                sh.dist[k] = raw[b + k * 4 + 3];
            }
            out.push(sh);
        }

        unsafe { device.destroy_descriptor_pool(desc_pool, None) };
        light_buf.destroy(device, &mut alloc);
        probe_buf.destroy(device, &mut alloc);
        emitter_buf.destroy(device, &mut alloc);
        out_buf.destroy(device, &mut alloc);
        Ok(out)
    }

    fn write_image(
        &self,
        device: &ash::Device,
        set: vk::DescriptorSet,
        binding: u32,
        sampler: vk::Sampler,
        view: vk::ImageView,
    ) {
        let info = [vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(binding)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&info);
        unsafe { device.update_descriptor_sets(&[write], &[]) };
    }

    pub fn destroy(&mut self, device: &ash::Device, alloc: &mut Allocator) {
        if let Some(mut gdf) = self.gdf.take() {
            gdf.destroy(device, alloc);
        }
        unsafe {
            device.destroy_sampler(self.dist_sampler, None);
            device.destroy_sampler(self.index_sampler, None);
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

fn host_ssbo(device: &ash::Device, alloc: &mut Allocator, data: &[u8], name: &str) -> Result<Buffer> {
    let mut buf = Buffer::new(
        device,
        alloc,
        data.len().max(4) as u64,
        vk::BufferUsageFlags::STORAGE_BUFFER,
        MemoryLocation::CpuToGpu,
        name,
    )?;
    if !data.is_empty() {
        buf.write(0, data);
    }
    Ok(buf)
}

fn write_ssbo(device: &ash::Device, set: vk::DescriptorSet, binding: u32, buffer: vk::Buffer) {
    let info = [vk::DescriptorBufferInfo::default()
        .buffer(buffer)
        .range(vk::WHOLE_SIZE)];
    let write = vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(binding)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .buffer_info(&info);
    unsafe { device.update_descriptor_sets(&[write], &[]) };
}
