//! GPU software-GI probe march. Runs `sw_gi.comp` over the merged Global
//! Distance Field (a 3D distance texture plus a nearest-instance index texture)
//! to produce SH-L1 radiance + directional-distance probes, the same packed
//! layout the standard shader samples.
//!
//! The GDF (distance + index textures + per-instance materials) is cached and
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

use crate::alloc::Buffer;
use crate::context::GpuContext;
use crate::texture::GpuTexture;
use crate::types::{BakeLight, LightKind, ProbeSh};

const SW_GI_COMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sw_gi.comp.spv"));
const SCREEN_GI_COMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/screen_gi.comp.spv"));
const FLUX_DENOISE_COMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/flux_denoise.comp.spv"));
const FLUX_INTEGRATE_COMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/flux_integrate.comp.spv"));
const SCREEN_GI_RT_COMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/screen_gi_rt.comp.spv"));

/// One SH probe volume's layout (std140 mirror of `GiProbeVolume` in the shader
/// / `GpuProbeVolume` on the renderer): the world SH radiance cache the gather
/// reads at ray hits.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct GiVolume {
    pub world_to_local: [[f32; 4]; 4],
    pub size_base: [f32; 4], // xyz local box size, w = first probe index (sh_base)
    pub counts: [f32; 4],    // xyz probe counts per axis
}

pub const GI_MAX_VOLUMES: usize = 4;

/// Screen-GI compute params (std140 UBO mirror of `screen_gi.comp`'s Params).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ScreenGiParams {
    pub inv_view_proj: [[f32; 4]; 4],
    pub prev_view_proj: [[f32; 4]; 4], // reproject world pos → previous-frame UV
    pub cam: [f32; 4],
    pub prev_cam: [f32; 4],            // previous frame camera pos (disocclusion)
    pub gdf_min: [f32; 4],
    pub gdf_max: [f32; 4],
    pub sky: [f32; 4],
    pub counts: [u32; 4], // samples, bounces, light_count, emitter_count
    pub march: [f32; 4],  // eps, max_dist, temporal_alpha, _
    pub misc: [u32; 4],   // seed, screen_w, screen_h, flip_y
    pub probe_info: [f32; 4], // x = probe volume count
    pub volumes: [GiVolume; GI_MAX_VOLUMES],
}

/// Push constants for flux_denoise.comp (probe-space à-trous denoise).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DenoisePush {
    size: [i32; 2], // probe-grid resolution
    step: i32,      // base à-trous tap spacing (probe texels)
    _pad: i32,
}

/// UBO for screen_gi_rt.comp (scalar layout). A subset of ScreenGiParams (no GDF
/// bounds / probe volumes — the RT gather traces the TLAS, not the SDF). Built and
/// verified ahead of the RT-gather dispatch wiring (the one remaining item-4 step).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
#[allow(dead_code)] // consumed by the upcoming screen_gi_rt dispatch in screen_resolve_record
struct RtGiParams {
    inv_view_proj: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    cam: [f32; 4],
    prev_cam: [f32; 4],
    sky: [f32; 4],
    counts: [u32; 4],
    march: [f32; 4],
    misc: [u32; 4],
}

impl RtGiParams {
    #[allow(dead_code)]
    fn from_screen(p: &ScreenGiParams) -> Self {
        Self {
            inv_view_proj: p.inv_view_proj,
            prev_view_proj: p.prev_view_proj,
            cam: p.cam,
            prev_cam: p.prev_cam,
            sky: p.sky,
            counts: p.counts,
            march: p.march,
            misc: p.misc,
        }
    }
}

/// Push constants for flux_integrate.comp (full-res per-pixel SH resolve).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct IntegratePush {
    inv_view_proj: [[f32; 4]; 4],
    cam: [f32; 4],
    dims: [i32; 4], // screen_w, screen_h, probe_w, probe_h
    misc: [f32; 4], // flip_y, probe_div, pad, pad
}

/// Per-frame Flux trace resources that must outlive the command-buffer recording
/// (the descriptor pool + the host SSBOs/UBO the dispatch reads). When the trace
/// is folded into the main frame command buffer there is no fence wait to bound
/// their lifetime, so the caller holds these and frees them one frame later,
/// after the frame fence that consumed them has signalled.
pub struct ScreenGiTransient {
    desc_pool: vk::DescriptorPool,
    light_buf: Buffer,
    emitter_buf: Buffer,
    param_buf: Buffer,
}

impl ScreenGiTransient {
    pub fn destroy(&mut self, device: &ash::Device, alloc: &mut Allocator) {
        unsafe { device.destroy_descriptor_pool(self.desc_pool, None) };
        self.light_buf.destroy(device, alloc);
        self.emitter_buf.destroy(device, alloc);
        self.param_buf.destroy(device, alloc);
    }
}

/// Per-instance material the march shades GDF hits with (looked up by the index
/// texture). Albedo drives the diffuse bounce; emission makes the surface a
/// light source.
#[derive(Clone, Copy)]
pub struct GpuGiMaterial {
    pub albedo: [f32; 3],
    pub emission: [f32; 3],
    /// Metalness [0,1]; the diffuse bounce uses albedo·(1−metallic).
    pub metallic: f32,
    /// Roughness [0,1]; reserved for the specular GI trace.
    pub roughness: f32,
}

/// An emissive instance reduced to a sphere area-light, sampled directly (NEE) by
/// the march so emitter fill is smooth instead of blotchy Monte-Carlo noise.
#[derive(Clone, Copy)]
pub struct GpuGiEmitter {
    pub center: [f32; 3],
    pub radius: f32,
    pub emission: [f32; 3],
}

/// Per-trace march inputs (lights/probes/params that change every trace; the GDF
/// is cached separately via `set_gdf`).
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

/// An async march submitted to the GPU but not yet read back. Its buffers must
/// stay alive until the fence signals; polled each frame so the trace never
/// blocks the main thread (GI work decoupled from the frame).
struct InFlight {
    fence: vk::Fence,
    cb: vk::CommandBuffer,
    desc_pool: vk::DescriptorPool,
    light_buf: Buffer,
    probe_buf: Buffer,
    emitter_buf: Buffer,
    out_buf: Buffer,
    count: usize,
}

/// Persistent compute pipeline + samplers + cached GDF for the GI march.
pub struct GpuGi {
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    dist_sampler: vk::Sampler,  // trilinear, clamp
    index_sampler: vk::Sampler, // nearest, clamp
    gdf: Option<CachedGdf>,
    /// Dedicated pool for async march command buffers (isolated from the
    /// renderer's per-frame pool).
    cmd_pool: vk::CommandPool,
    in_flight: Option<InFlight>,
    /// Screen-space GI final-gather compute (per-pixel GDF trace).
    screen_set_layout: vk::DescriptorSetLayout,
    screen_pipeline_layout: vk::PipelineLayout,
    screen_pipeline: vk::Pipeline,
    /// Probe-space GI denoise compute (à-trous filter on the gather, at probe res).
    denoise_set_layout: vk::DescriptorSetLayout,
    denoise_pipeline_layout: vk::PipelineLayout,
    denoise_pipeline: vk::Pipeline,
    /// Full-res screen-probe integrate compute (per-pixel-normal SH resolve).
    integrate_set_layout: vk::DescriptorSetLayout,
    integrate_pipeline_layout: vk::PipelineLayout,
    integrate_pipeline: vk::Pipeline,
    /// HARDWARE ray-query screen-probe gather (RT-core backend; ray-query devices).
    /// `None` when the device lacks ray-query — the SDF gather is then the only path.
    rt_set_layout: Option<vk::DescriptorSetLayout>,
    rt_pipeline_layout: Option<vk::PipelineLayout>,
    rt_pipeline: Option<vk::Pipeline>,
}

impl GpuGi {
    pub fn new(ctx: &GpuContext) -> Result<Self> {
        let device = &ctx.device;
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

        let cmd_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(ctx.queue_family),
                None,
            )?
        };

        // Screen-space GI pipeline: GDF (0,1) + insts(2) + lights(3) + emitters(4)
        // + depth sampler(5) + output storage image(6) + params UBO(7).
        let storage_img = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(stage)
        };
        let ubo = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(stage)
        };
        let screen_bindings = [
            sampled(0),
            sampled(1),
            ssbo(2),
            ssbo(3),
            ssbo(4),
            sampled(5),
            storage_img(6),
            ubo(7),
            sampled(8), // history (previous frame's result, for temporal accum)
            ssbo(9),    // world SH probe radiance cache (read at ray hits)
            ssbo(10),   // per-screen-probe SH-L1 output (cur, written)
            ssbo(11),   // per-screen-probe SH-L1 (prev frame, read for temporal)
        ];
        let screen_set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&screen_bindings),
                None,
            )?
        };
        let screen_layouts = [screen_set_layout];
        let screen_pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&screen_layouts),
                None,
            )?
        };
        let screen_code = ash::util::read_spv(&mut Cursor::new(SCREEN_GI_COMP))?;
        let screen_module = unsafe {
            device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&screen_code), None)?
        };
        let screen_pipeline = unsafe {
            device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(
                            vk::PipelineShaderStageCreateInfo::default()
                                .stage(stage)
                                .module(screen_module)
                                .name(c"main"),
                        )
                        .layout(screen_pipeline_layout)],
                    None,
                )
                .map_err(|(_, e)| e)?[0]
        };
        unsafe { device.destroy_shader_module(screen_module, None) };

        // Probe-space denoise: input gather (sampled), output filtered (storage).
        let denoise_bindings = [sampled(0), storage_img(1)];
        let denoise_set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&denoise_bindings),
                None,
            )?
        };
        let denoise_layouts = [denoise_set_layout];
        let denoise_pc = [vk::PushConstantRange::default()
            .stage_flags(stage)
            .offset(0)
            .size(size_of::<DenoisePush>() as u32)];
        let denoise_pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&denoise_layouts)
                    .push_constant_ranges(&denoise_pc),
                None,
            )?
        };
        let denoise_code = ash::util::read_spv(&mut Cursor::new(FLUX_DENOISE_COMP))?;
        let denoise_module = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&denoise_code),
                None,
            )?
        };
        let denoise_pipeline = unsafe {
            device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(
                            vk::PipelineShaderStageCreateInfo::default()
                                .stage(stage)
                                .module(denoise_module)
                                .name(c"main"),
                        )
                        .layout(denoise_pipeline_layout)],
                    None,
                )
                .map_err(|(_, e)| e)?[0]
        };
        unsafe { device.destroy_shader_module(denoise_module, None) };

        // Full-res integrate: scalar GI (sampled) + depth (sampled) + screen-probe
        // SH (ssbo) → integrated GI (storage).
        let integrate_bindings = [sampled(0), sampled(1), ssbo(2), storage_img(3)];
        let integrate_set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&integrate_bindings),
                None,
            )?
        };
        let integrate_layouts = [integrate_set_layout];
        let integrate_pc = [vk::PushConstantRange::default()
            .stage_flags(stage)
            .offset(0)
            .size(size_of::<IntegratePush>() as u32)];
        let integrate_pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&integrate_layouts)
                    .push_constant_ranges(&integrate_pc),
                None,
            )?
        };
        let integrate_code = ash::util::read_spv(&mut Cursor::new(FLUX_INTEGRATE_COMP))?;
        let integrate_module = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&integrate_code),
                None,
            )?
        };
        let integrate_pipeline = unsafe {
            device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(
                            vk::PipelineShaderStageCreateInfo::default()
                                .stage(stage)
                                .module(integrate_module)
                                .name(c"main"),
                        )
                        .layout(integrate_pipeline_layout)],
                    None,
                )
                .map_err(|(_, e)| e)?[0]
        };
        unsafe { device.destroy_shader_module(integrate_module, None) };

        // Hardware ray-query gather pipeline — only when the device exposes ray-query
        // (the shader references accelerationStructureEXT). Bindings mirror
        // screen_gi_rt.comp: depth, out, history, TLAS, instances, lights, emitters,
        // sh-out, sh-prev, params UBO.
        let (rt_set_layout, rt_pipeline_layout, rt_pipeline) = if ctx.ray_tracing() {
            let accel = |b: u32| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::ACCELERATION_STRUCTURE_KHR)
                    .descriptor_count(1)
                    .stage_flags(stage)
            };
            let rt_bindings = [
                sampled(0), storage_img(1), sampled(2), accel(3),
                ssbo(4), ssbo(5), ssbo(6), ssbo(7), ssbo(8), ubo(9),
            ];
            let rt_set_layout = unsafe {
                device.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&rt_bindings),
                    None,
                )?
            };
            let rt_layouts = [rt_set_layout];
            let rt_pipeline_layout = unsafe {
                device.create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&rt_layouts),
                    None,
                )?
            };
            let rt_code = ash::util::read_spv(&mut Cursor::new(SCREEN_GI_RT_COMP))?;
            let rt_module = unsafe {
                device.create_shader_module(
                    &vk::ShaderModuleCreateInfo::default().code(&rt_code),
                    None,
                )?
            };
            let rt_pipeline = unsafe {
                device
                    .create_compute_pipelines(
                        vk::PipelineCache::null(),
                        &[vk::ComputePipelineCreateInfo::default()
                            .stage(
                                vk::PipelineShaderStageCreateInfo::default()
                                    .stage(stage)
                                    .module(rt_module)
                                    .name(c"main"),
                            )
                            .layout(rt_pipeline_layout)],
                        None,
                    )
                    .map_err(|(_, e)| e)?[0]
            };
            unsafe { device.destroy_shader_module(rt_module, None) };
            (Some(rt_set_layout), Some(rt_pipeline_layout), Some(rt_pipeline))
        } else {
            (None, None, None)
        };

        Ok(Self {
            set_layout,
            pipeline_layout,
            pipeline,
            dist_sampler,
            index_sampler,
            gdf: None,
            cmd_pool,
            in_flight: None,
            screen_set_layout,
            screen_pipeline_layout,
            screen_pipeline,
            denoise_set_layout,
            denoise_pipeline_layout,
            denoise_pipeline,
            integrate_set_layout,
            integrate_pipeline_layout,
            integrate_pipeline,
            rt_set_layout,
            rt_pipeline_layout,
            rt_pipeline,
        })
    }

    /// (Re)upload the GDF (distance + index 3D textures and per-instance
    /// materials), replacing any cached one. Call only when geometry changes.
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
                    // .w lanes carry the PBR scalars the diffuse bounce needs:
                    // albedo.w = metalness (shader uses albedo·(1−metalness)),
                    // emission.w = roughness (reserved for the specular trace).
                    albedo: [m.albedo[0], m.albedo[1], m.albedo[2], m.metallic],
                    emission: [m.emission[0], m.emission[1], m.emission[2], m.roughness],
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

    /// True while an async march is submitted but not yet read back.
    pub fn is_marching(&self) -> bool {
        self.in_flight.is_some()
    }

    /// True once a GDF has been uploaded (screen-GI can trace it).
    pub fn has_gdf(&self) -> bool {
        self.gdf.is_some()
    }

    /// Begin an async march against the cached GDF: record + submit the compute
    /// with a fence but DON'T wait. Returns false if no GDF, no probes, or a
    /// march is already in flight. Read the result later via [`march_poll`].
    pub fn march_begin(
        &mut self,
        ctx: &GpuContext,
        allocator: &Arc<Mutex<Allocator>>,
        m: &GpuGiMarch<'_>,
    ) -> Result<bool> {
        if self.in_flight.is_some() {
            return Ok(false);
        }
        let Some(gdf) = self.gdf.as_ref() else {
            return Ok(false);
        };
        let device = &ctx.device;
        let count = m.probes.len();
        if count == 0 {
            return Ok(false);
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
        let light_buf =
            host_ssbo(device, &mut alloc, bytemuck::cast_slice(&lights), "gi lights")?;
        let probe_buf =
            host_ssbo(device, &mut alloc, bytemuck::cast_slice(&probes), "gi probes")?;
        let emitter_buf =
            host_ssbo(device, &mut alloc, bytemuck::cast_slice(&emitters), "gi emitters")?;
        let out_buf = Buffer::new(
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
        // Record + submit WITHOUT waiting; the fence is polled next frame.
        let cb = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0]
        };
        unsafe {
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
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
            device.end_command_buffer(cb)?;
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
            let cbs = [cb];
            let submit = vk::SubmitInfo::default().command_buffers(&cbs);
            device.queue_submit(ctx.queue, &[submit], fence)?;
            self.in_flight = Some(InFlight {
                fence,
                cb,
                desc_pool,
                light_buf,
                probe_buf,
                emitter_buf,
                out_buf,
                count,
            });
        }
        Ok(true)
    }

    /// Poll the in-flight march; when the GPU has finished, read back the probe
    /// SH, free its resources, and return them. `None` while still running or
    /// when nothing is in flight.
    pub fn march_poll(
        &mut self,
        ctx: &GpuContext,
        allocator: &Arc<Mutex<Allocator>>,
    ) -> Option<Vec<ProbeSh>> {
        let f = self.in_flight.as_ref()?;
        let device = &ctx.device;
        // Non-blocking check: NOT_READY → still running.
        let ready = unsafe { device.get_fence_status(f.fence) }.unwrap_or(false);
        if !ready {
            return None;
        }
        let mut f = self.in_flight.take().unwrap();
        let count = f.count;
        let bytes = f.out_buf.read();
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
        let mut alloc = allocator.lock().unwrap();
        unsafe {
            device.destroy_fence(f.fence, None);
            device.free_command_buffers(self.cmd_pool, &[f.cb]);
            device.destroy_descriptor_pool(f.desc_pool, None);
        }
        f.light_buf.destroy(device, &mut alloc);
        f.probe_buf.destroy(device, &mut alloc);
        f.emitter_buf.destroy(device, &mut alloc);
        f.out_buf.destroy(device, &mut alloc);
        Some(out)
    }

    /// Screen-space GI final gather: per-pixel GDF trace into `out_view` (a
    /// STORAGE image). Reads `depth_view` (must already be SHADER_READ_ONLY).
    /// Synchronous (blocks) for now; async is a follow-up.
    /// Leaves `out` in SHADER_READ_ONLY_OPTIMAL for the forward pass to sample.
    /// Record the Flux gather dispatch into an externally-owned command buffer
    /// (the main frame cb) instead of submitting + blocking on its own fence.
    /// Returns the transient descriptor pool/buffers the caller must keep alive
    /// until that frame's fence signals, then free via [`ScreenGiTransient::destroy`].
    #[allow(clippy::too_many_arguments)]
    pub fn screen_resolve_record(
        &self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        allocator: &Arc<Mutex<Allocator>>,
        depth_view: vk::ImageView,
        depth_sampler: vk::Sampler,
        out_image: vk::Image,
        out_view: vk::ImageView,
        history_view: vk::ImageView,
        // Probe-res denoise target: the gather is written to `out_image`, then the
        // à-trous denoise reads it and writes here; the forward samples THIS.
        filtered_image: vk::Image,
        filtered_view: vk::ImageView,
        // Per-screen-probe SH-L1 buffers: the gather writes `sh_buf` (binding 10) and
        // reads `prev_sh_buf` (binding 11, previous frame, ping-pong) for temporal
        // accumulation of the SH; the integrate then reads `sh_buf`.
        sh_buf: vk::Buffer,
        prev_sh_buf: vk::Buffer,
        // Full-res integrate output (the forward samples THIS) + full screen extent.
        final_image: vk::Image,
        final_view: vk::ImageView,
        full_extent: vk::Extent2D,
        extent: vk::Extent2D,
        lights: &[BakeLight],
        emitters: &[GpuGiEmitter],
        history_valid: bool,
        // World SH radiance cache: the renderer's probe SSBO (always a valid
        // buffer; `params.probe_info.x` = volume count, 0 = none → shader skips it).
        probe_buffer: vk::Buffer,
        mut params: ScreenGiParams,
    ) -> Result<Option<ScreenGiTransient>> {
        let Some(gdf) = self.gdf.as_ref() else {
            return Ok(None);
        };
        let mut alloc = allocator.lock().unwrap();
        let gi_lights: Vec<GiLight> = if lights.is_empty() {
            vec![GiLight::zeroed()]
        } else {
            lights.iter().map(pack_light).collect()
        };
        let gi_emitters: Vec<GiEmitter> = if emitters.is_empty() {
            vec![GiEmitter::zeroed()]
        } else {
            emitters
                .iter()
                .map(|e| GiEmitter {
                    center_radius: [e.center[0], e.center[1], e.center[2], e.radius],
                    emission: [e.emission[0], e.emission[1], e.emission[2], 0.0],
                })
                .collect()
        };
        params.gdf_min = [gdf.min[0], gdf.min[1], gdf.min[2], 0.0];
        params.gdf_max = [gdf.max[0], gdf.max[1], gdf.max[2], 0.0];
        // Auto max trace distance (march.y == 0): the GDF box diagonal, so rays
        // can cross the whole scene without an arbitrary fixed cap.
        if params.march[1] <= 0.0 {
            let d = [
                gdf.max[0] - gdf.min[0],
                gdf.max[1] - gdf.min[1],
                gdf.max[2] - gdf.min[2],
            ];
            params.march[1] = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        }
        params.counts[2] = lights.len() as u32;
        params.counts[3] = emitters.len() as u32;
        params.misc[1] = extent.width;
        params.misc[2] = extent.height;
        // params.march[2] (temporal alpha) is set by the caller, motion-aware:
        // snap when fresh, trust the new trace more while moving, converge gently
        // when still. (history_valid only gates the descriptor sanity below.)
        let _ = history_valid;
        let light_buf =
            host_ssbo(device, &mut alloc, bytemuck::cast_slice(&gi_lights), "sgi lights")?;
        let emitter_buf = host_ssbo(
            device,
            &mut alloc,
            bytemuck::cast_slice(&gi_emitters),
            "sgi emitters",
        )?;
        let mut param_buf = Buffer::new(
            device,
            &mut alloc,
            size_of::<ScreenGiParams>() as u64,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            MemoryLocation::CpuToGpu,
            "sgi params",
        )?;
        param_buf.write(0, bytemuck::bytes_of(&params));
        drop(alloc);

        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 7, // gather:4 + denoise:1 + integrate:2(scalar,depth)
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 8, // gather:6 (insts,lights,emitters,cache,sh-cur,sh-prev) + integrate:1
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 3, // gather out + denoise out + integrate out
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: 1,
            },
        ];
        let desc_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(3) // gather + denoise + integrate sets
                    .pool_sizes(&pool_sizes),
                None,
            )?
        };
        let layouts = [self.screen_set_layout];
        let set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(desc_pool)
                    .set_layouts(&layouts),
            )?[0]
        };
        self.write_image(device, set, 0, self.dist_sampler, gdf.dist_tex.view);
        self.write_image(device, set, 1, self.index_sampler, gdf.index_tex.view);
        write_ssbo(device, set, 2, gdf.inst_buf.handle);
        write_ssbo(device, set, 3, light_buf.handle);
        write_ssbo(device, set, 4, emitter_buf.handle);
        write_ssbo(device, set, 9, probe_buffer); // world SH radiance cache
        write_ssbo(device, set, 10, sh_buf); // per-screen-probe SH-L1 output (cur)
        write_ssbo(device, set, 11, prev_sh_buf); // previous-frame SH (temporal)
        self.write_image(device, set, 5, depth_sampler, depth_view);
        // Storage image (binding 6): GENERAL layout for compute write.
        let img_info = [vk::DescriptorImageInfo::default()
            .image_view(out_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let img_write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(6)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .image_info(&img_info);
        let buf_info = [vk::DescriptorBufferInfo::default()
            .buffer(param_buf.handle)
            .range(vk::WHOLE_SIZE)];
        let buf_write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(7)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .buffer_info(&buf_info);
        // Binding 8: history = the PREVIOUS frame's gather image (ping-pong, in
        // SHADER_READ), reprojected per pixel for temporal accumulation.
        let _ = history_valid;
        let hist_info = [vk::DescriptorImageInfo::default()
            .sampler(depth_sampler)
            .image_view(history_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let hist_write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(8)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&hist_info);
        unsafe { device.update_descriptor_sets(&[img_write, buf_write, hist_write], &[]) };

        let groups_x = extent.width.div_ceil(8);
        let groups_y = extent.height.div_ceil(8);
        unsafe {
            // Output is a fresh (ping-pong) image fully overwritten this trace,
            // so discard its old contents. History is the other image (read-only).
            let to_general = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
                .image(out_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_general],
            );
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.screen_pipeline);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.screen_pipeline_layout,
                0,
                &[set],
                &[],
            );
            device.cmd_dispatch(cb, groups_x, groups_y, 1);
            // Gather out: GENERAL -> SHADER_READ_ONLY so the DENOISE compute can
            // sample it (and the forward as a fallback). Made available to compute.
            let to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .image(out_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );

            // --- Probe-space denoise pass (à-trous spatial filter) ---
            // Allocate the denoise set from the same per-frame pool, point it at the
            // gather (sampled) + the filtered output (storage), dispatch at probe res.
            let dn_layouts = [self.denoise_set_layout];
            let dn_set = device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(desc_pool)
                        .set_layouts(&dn_layouts),
                )
                .map(|v| v[0])?;
            self.write_image(device, dn_set, 0, depth_sampler, out_view);
            let dn_img_info = [vk::DescriptorImageInfo::default()
                .image_view(filtered_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let dn_img_write = vk::WriteDescriptorSet::default()
                .dst_set(dn_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&dn_img_info);
            device.update_descriptor_sets(&[dn_img_write], &[]);
            // filtered image: UNDEFINED -> GENERAL (fully overwritten this pass).
            let f_to_general = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
                .image(filtered_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[f_to_general],
            );
            let dn_push = DenoisePush {
                size: [extent.width as i32, extent.height as i32],
                step: 1,
                _pad: 0,
            };
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.denoise_pipeline);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.denoise_pipeline_layout,
                0,
                &[dn_set],
                &[],
            );
            device.cmd_push_constants(
                cb,
                self.denoise_pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(&dn_push),
            );
            device.cmd_dispatch(cb, groups_x, groups_y, 1);
            // filtered image: GENERAL -> SHADER_READ_ONLY so the INTEGRATE compute
            // can sample it (made available to compute, not fragment).
            let f_to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .image(filtered_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            // The gather wrote the per-probe SH buffer; make it visible to the
            // integrate compute that reads it (compute -> compute).
            let sh_barrier = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(sh_buf)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[sh_barrier],
                &[f_to_read],
            );

            // --- Full-res integrate pass (per-pixel-normal SH resolve) ---
            let in_layouts = [self.integrate_set_layout];
            let in_set = device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(desc_pool)
                        .set_layouts(&in_layouts),
                )
                .map(|v| v[0])?;
            self.write_image(device, in_set, 0, depth_sampler, filtered_view); // scalar GI
            self.write_image(device, in_set, 1, depth_sampler, depth_view);    // depth prepass
            write_ssbo(device, in_set, 2, sh_buf);                             // per-probe SH
            let in_img_info = [vk::DescriptorImageInfo::default()
                .image_view(final_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let in_img_write = vk::WriteDescriptorSet::default()
                .dst_set(in_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&in_img_info);
            device.update_descriptor_sets(&[in_img_write], &[]);
            let fin_to_general = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
                .image(final_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[fin_to_general],
            );
            let in_push = IntegratePush {
                inv_view_proj: params.inv_view_proj,
                cam: params.cam,
                dims: [
                    full_extent.width as i32,
                    full_extent.height as i32,
                    extent.width as i32,
                    extent.height as i32,
                ],
                misc: [params.misc[3] as f32, 0.0, 0.0, 0.0],
            };
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.integrate_pipeline);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.integrate_pipeline_layout,
                0,
                &[in_set],
                &[],
            );
            device.cmd_push_constants(
                cb,
                self.integrate_pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(&in_push),
            );
            device.cmd_dispatch(cb, full_extent.width.div_ceil(8), full_extent.height.div_ceil(8), 1);
            // integrated image: GENERAL -> SHADER_READ_ONLY for the forward pass.
            let fin_to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .image(final_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[fin_to_read],
            );
        }

        Ok(Some(ScreenGiTransient {
            desc_pool,
            light_buf,
            emitter_buf,
            param_buf,
        }))
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
        if let Some(mut f) = self.in_flight.take() {
            unsafe {
                let _ = device.wait_for_fences(&[f.fence], true, u64::MAX);
                device.destroy_fence(f.fence, None);
                device.free_command_buffers(self.cmd_pool, &[f.cb]);
                device.destroy_descriptor_pool(f.desc_pool, None);
            }
            f.light_buf.destroy(device, alloc);
            f.probe_buf.destroy(device, alloc);
            f.emitter_buf.destroy(device, alloc);
            f.out_buf.destroy(device, alloc);
        }
        if let Some(mut gdf) = self.gdf.take() {
            gdf.destroy(device, alloc);
        }
        unsafe {
            device.destroy_command_pool(self.cmd_pool, None);
            device.destroy_sampler(self.dist_sampler, None);
            device.destroy_sampler(self.index_sampler, None);
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_pipeline(self.screen_pipeline, None);
            device.destroy_pipeline_layout(self.screen_pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.screen_set_layout, None);
            device.destroy_pipeline(self.denoise_pipeline, None);
            device.destroy_pipeline_layout(self.denoise_pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.denoise_set_layout, None);
            device.destroy_pipeline(self.integrate_pipeline, None);
            device.destroy_pipeline_layout(self.integrate_pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.integrate_set_layout, None);
            if let Some(p) = self.rt_pipeline {
                device.destroy_pipeline(p, None);
            }
            if let Some(l) = self.rt_pipeline_layout {
                device.destroy_pipeline_layout(l, None);
            }
            if let Some(l) = self.rt_set_layout {
                device.destroy_descriptor_set_layout(l, None);
            }
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
