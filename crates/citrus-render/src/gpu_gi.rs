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
/// blocks the main thread (Lumen-style, GI work decoupled from the frame).
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
        extent: vk::Extent2D,
        lights: &[BakeLight],
        emitters: &[GpuGiEmitter],
        history_valid: bool,
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
                descriptor_count: 4, // dist, index, depth, history
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 3,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 1,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: 1,
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
            // out image: GENERAL -> SHADER_READ_ONLY for the forward pass.
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
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
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
