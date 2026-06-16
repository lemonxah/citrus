//! Ray-traced reflections (1 bounce) via Vulkan ray query. Reuses the GI bake's
//! acceleration-structure builders (`bake::build_blas`/`build_tlas`): a BLAS is
//! cached per mesh, a TLAS is rebuilt each frame from the visible draws, and a
//! compute pass (`rt_reflect.comp`) traces a reflection ray per reflective pixel,
//! shades the hit, and composites it into the resolved HDR target the post pass
//! reads. Only used when the GPU supports ray query (`ctx.accel`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use ash::vk;
use bytemuck::{Pod, Zeroable};
use glam::Mat4;
use gpu_allocator::vulkan::Allocator;

use crate::alloc::Buffer;
use crate::bake::{self, Accel};
use crate::context::GpuContext;
use crate::{GpuMesh, frame::FRAMES_IN_FLIGHT};

const SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rt_reflect.comp.spv"));

/// Per-instance shading data, mirrors `Instance` in `rt_reflect.comp`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuInstance {
    vtx_addr: u64,
    idx_addr: u64,
    albedo: [f32; 4],
    emission: [f32; 4],
}

/// One scene instance to reflect: mesh + world transform + surface colour.
pub(crate) struct RtInstance {
    pub mesh: usize,
    pub transform: Mat4,
    pub albedo: [f32; 3],
    pub emission: [f32; 3],
}

/// Light for hit shading, mirrors `Light` in the shader (matches `bake`'s layout).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct RtLight {
    pub position: [f32; 4],  // xyz, w = kind (0 dir / 1 point / 2 spot)
    pub direction: [f32; 4], // xyz, w = range
    pub color: [f32; 4],
    pub spot: [f32; 4], // x cos_inner, y cos_outer
}

/// Resolve uniforms, mirrors `RtData` in the shader.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct RtUniforms {
    pub inv_proj: [[f32; 4]; 4],
    pub inv_view: [[f32; 4]; 4],
    pub camera_pos: [f32; 4],
    pub refl_center: [f32; 4],
    pub refl_extents: [f32; 4],
    pub params: [f32; 4], // x rough cutoff, y intensity, z light count, w _
    pub screen: [f32; 4],
}

/// Per-in-flight-slot small buffers (camera uniforms + lights), recreated each
/// frame and freed when the slot is reused.
struct RtFrame {
    lights: Buffer,
    ubo: Buffer,
}

pub(crate) struct RtReflect {
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    /// BLAS per mesh index (built once, reused across frames).
    blas: HashMap<usize, Accel>,
    /// Per-in-flight-slot transient uniforms/lights.
    frames: Vec<Option<RtFrame>>,
    /// Persistent TLAS + instance SSBO, rebuilt only when the scene changes (a
    /// per-frame rebuild with its blocking AS build is what tanks the framerate).
    tlas: Option<Accel>,
    tlas_buffers: Vec<Buffer>,
    instances: Option<Buffer>,
    /// Signature (mesh ids + transforms) of the cached TLAS; rebuild on mismatch.
    tlas_sig: u64,
}

impl RtReflect {
    pub fn new(device: &ash::Device) -> Result<Self> {
        let sampler_binding = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            sampler_binding(1),
            sampler_binding(2),
            sampler_binding(3),
            sampler_binding(4),
            vk::DescriptorSetLayoutBinding::default()
                .binding(5)
                .descriptor_type(vk::DescriptorType::ACCELERATION_STRUCTURE_KHR)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(6)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(7)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(8)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
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
        let module = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default()
                    .code(&ash::util::read_spv(&mut std::io::Cursor::new(SPV))?),
                None,
            )?
        };
        let pipeline = unsafe {
            device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(
                            vk::PipelineShaderStageCreateInfo::default()
                                .stage(vk::ShaderStageFlags::COMPUTE)
                                .module(module)
                                .name(c"main"),
                        )
                        .layout(pipeline_layout)],
                    None,
                )
                .map_err(|(_, e)| e)?[0]
        };
        unsafe { device.destroy_shader_module(module, None) };
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
                    .max_sets(FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_IMAGE,
                            descriptor_count: FRAMES_IN_FLIGHT as u32,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                            descriptor_count: (FRAMES_IN_FLIGHT * 4) as u32,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::ACCELERATION_STRUCTURE_KHR,
                            descriptor_count: FRAMES_IN_FLIGHT as u32,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_BUFFER,
                            descriptor_count: (FRAMES_IN_FLIGHT * 2) as u32,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::UNIFORM_BUFFER,
                            descriptor_count: FRAMES_IN_FLIGHT as u32,
                        },
                    ]),
                None,
            )?
        };
        let layouts = vec![set_layout; FRAMES_IN_FLIGHT];
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )?
        };
        Ok(Self {
            set_layout,
            pipeline_layout,
            pipeline,
            sampler,
            pool,
            sets,
            blas: HashMap::new(),
            frames: (0..FRAMES_IN_FLIGHT).map(|_| None).collect(),
            tlas: None,
            tlas_buffers: Vec::new(),
            instances: None,
            tlas_sig: 0,
        })
    }

    /// Build/refresh the cached BLAS-per-mesh + the scene TLAS + per-instance
    /// shading SSBO (vertex/index device addresses + albedo/emission), rebuilt only
    /// when the scene signature changes. Returns the TLAS handle + instance buffer
    /// handle, or None when there's no ray-query device or nothing to trace. Shared
    /// by the reflection dispatch (`record`) AND the Flux-RT GI gather — both key
    /// the same cache by scene signature, so one scene builds one TLAS.
    pub fn ensure_tlas(
        &mut self,
        ctx: &GpuContext,
        allocator: &Arc<Mutex<Allocator>>,
        command_pool: vk::CommandPool,
        meshes: &[GpuMesh],
        instances: &[RtInstance],
    ) -> Result<Option<(vk::AccelerationStructureKHR, vk::Buffer)>> {
        let Some(accel) = ctx.accel.as_ref() else {
            return Ok(None);
        };
        let device = &ctx.device;
        let scratch_align = bake::accel_scratch_alignment(ctx);

        // BLAS per referenced mesh (cached across frames).
        for inst in instances {
            if !self.blas.contains_key(&inst.mesh) {
                let blas = bake::build_blas(
                    ctx, accel, allocator, command_pool, &meshes[inst.mesh], scratch_align,
                )?;
                self.blas.insert(inst.mesh, blas);
            }
        }

        let mut tlas_instances = Vec::with_capacity(instances.len());
        let mut gpu_instances = Vec::with_capacity(instances.len());
        let mut sig: u64 = 0xcbf29ce484222325;
        let fnv = |v: u64, s: &mut u64| {
            *s ^= v;
            *s = s.wrapping_mul(0x100000001b3);
        };
        for (i, inst) in instances.iter().enumerate() {
            let Some(blas) = self.blas.get(&inst.mesh) else {
                continue;
            };
            let mesh = &meshes[inst.mesh];
            gpu_instances.push(GpuInstance {
                vtx_addr: mesh.vertex_buffer.device_address(device),
                idx_addr: mesh.index_buffer.device_address(device),
                albedo: [inst.albedo[0], inst.albedo[1], inst.albedo[2], 0.0],
                emission: [inst.emission[0], inst.emission[1], inst.emission[2], 0.0],
            });
            tlas_instances.push(vk::AccelerationStructureInstanceKHR {
                transform: bake::transform_matrix(&inst.transform),
                instance_custom_index_and_mask: vk::Packed24_8::new(i as u32, 0xFF),
                instance_shader_binding_table_record_offset_and_flags: vk::Packed24_8::new(
                    0,
                    vk::GeometryInstanceFlagsKHR::TRIANGLE_FACING_CULL_DISABLE.as_raw() as u8,
                ),
                acceleration_structure_reference: vk::AccelerationStructureReferenceKHR {
                    device_handle: blas.address,
                },
            });
            fnv(inst.mesh as u64, &mut sig);
            for f in inst.transform.to_cols_array() {
                fnv(f.to_bits() as u64, &mut sig);
            }
        }
        if tlas_instances.is_empty() {
            return Ok(None);
        }
        if self.tlas.is_none() || sig != self.tlas_sig {
            unsafe { device.device_wait_idle().ok() };
            {
                let mut a = allocator.lock().unwrap();
                if let Some(mut t) = self.tlas.take() {
                    unsafe { accel.destroy_acceleration_structure(t.handle, None) };
                    t.buffer.destroy(device, &mut a);
                }
                for mut b in self.tlas_buffers.drain(..) {
                    b.destroy(device, &mut a);
                }
                if let Some(mut b) = self.instances.take() {
                    b.destroy(device, &mut a);
                }
            }
            let mut tlas_buffers = Vec::new();
            let tlas = bake::build_tlas(
                ctx, accel, allocator, command_pool, &tlas_instances, scratch_align,
                &mut tlas_buffers,
            )?;
            let inst_buf = {
                let mut a = allocator.lock().unwrap();
                host_buffer(device, &mut a, bytemuck::cast_slice(&gpu_instances), "rt-instances")?
            };
            self.tlas = Some(tlas);
            self.tlas_buffers = tlas_buffers;
            self.instances = Some(inst_buf);
            self.tlas_sig = sig;
        }
        Ok(Some((
            self.tlas.as_ref().unwrap().handle,
            self.instances.as_ref().unwrap().handle,
        )))
    }

    /// Build/refresh the acceleration structures + buffers for `frame` and record
    /// the reflection compute dispatch, writing the composited result into
    /// `out_view` (a STORAGE-usable RGBA16F image, left in GENERAL). The caller
    /// barriers `out_image` to SHADER_READ afterward for the post pass.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        ctx: &GpuContext,
        allocator: &Arc<Mutex<Allocator>>,
        command_pool: vk::CommandPool,
        cb: vk::CommandBuffer,
        frame: usize,
        meshes: &[GpuMesh],
        instances: &[RtInstance],
        lights: &[RtLight],
        out_image: vk::Image,
        out_view: vk::ImageView,
        color_view: vk::ImageView,
        depth_view: vk::ImageView,
        gbuf_view: vk::ImageView,
        env_view: vk::ImageView,
        uniforms: &RtUniforms,
        extent: vk::Extent2D,
    ) -> Result<()> {
        let Some(_accel) = ctx.accel.as_ref() else {
            return Ok(());
        };
        let device = &ctx.device;

        // Free this slot's previous small buffers (its GPU work finished when the
        // renderer waited this frame's fence).
        if let Some(mut old) = self.frames[frame].take() {
            let mut a = allocator.lock().unwrap();
            old.lights.destroy(device, &mut a);
            old.ubo.destroy(device, &mut a);
        }

        // BLAS + TLAS + instance SSBO (shared cache via ensure_tlas).
        let Some((tlas_handle, inst_handle)) =
            self.ensure_tlas(ctx, allocator, command_pool, meshes, instances)?
        else {
            return Ok(()); // nothing to reflect; leave the resolved target as-is
        };

        // Per-frame small buffers: lights + camera uniforms.
        let (light_buf, ubo) = {
            let mut alloc = allocator.lock().unwrap();
            let light_bytes: Vec<RtLight> = if lights.is_empty() {
                vec![RtLight::zeroed()]
            } else {
                lights.to_vec()
            };
            let light_buf =
                host_buffer(device, &mut alloc, bytemuck::cast_slice(&light_bytes), "rt-lights")?;
            let mut ubo = Buffer::new(
                device,
                &mut alloc,
                std::mem::size_of::<RtUniforms>() as u64,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                gpu_allocator::MemoryLocation::CpuToGpu,
                "rt-uniforms",
            )?;
            ubo.write(0, bytemuck::bytes_of(uniforms));
            (light_buf, ubo)
        };

        // Write the descriptor set for this frame.
        let set = self.sets[frame];
        let out_info = [vk::DescriptorImageInfo::default()
            .image_view(out_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let img = |v: vk::ImageView| {
            [vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(v)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        };
        let color_i = img(color_view);
        let depth_i = img(depth_view);
        let gbuf_i = img(gbuf_view);
        let env_i = img(env_view);
        let tlas_handles = [tlas_handle];
        let mut tlas_write = vk::WriteDescriptorSetAccelerationStructureKHR::default()
            .acceleration_structures(&tlas_handles);
        let inst_info = [vk::DescriptorBufferInfo::default().buffer(inst_handle).range(vk::WHOLE_SIZE)];
        let light_info = [vk::DescriptorBufferInfo::default().buffer(light_buf.handle).range(vk::WHOLE_SIZE)];
        let ubo_info = [vk::DescriptorBufferInfo::default().buffer(ubo.handle).range(vk::WHOLE_SIZE)];
        let w = |b: u32, ty: vk::DescriptorType| vk::WriteDescriptorSet::default().dst_set(set).dst_binding(b).descriptor_type(ty);
        let mut as_w = w(5, vk::DescriptorType::ACCELERATION_STRUCTURE_KHR);
        as_w = as_w.push_next(&mut tlas_write);
        as_w.descriptor_count = 1;
        let writes = [
            w(0, vk::DescriptorType::STORAGE_IMAGE).image_info(&out_info),
            w(1, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&color_i),
            w(2, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&depth_i),
            w(3, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&gbuf_i),
            w(4, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&env_i),
            as_w,
            w(6, vk::DescriptorType::STORAGE_BUFFER).buffer_info(&inst_info),
            w(7, vk::DescriptorType::STORAGE_BUFFER).buffer_info(&light_info),
            w(8, vk::DescriptorType::UNIFORM_BUFFER).buffer_info(&ubo_info),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        // Output image UNDEFINED -> GENERAL for the compute write.
        crate::image_barrier(
            device, cb, out_image, vk::ImageAspectFlags::COLOR,
            vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL,
            vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::empty(),
            vk::PipelineStageFlags2::COMPUTE_SHADER, vk::AccessFlags2::SHADER_WRITE,
        );
        unsafe {
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[set],
                &[],
            );
            device.cmd_dispatch(cb, extent.width.div_ceil(8), extent.height.div_ceil(8), 1);
        }

        self.frames[frame] = Some(RtFrame { lights: light_buf, ubo });
        Ok(())
    }

    pub fn destroy(&mut self, ctx: &GpuContext, allocator: &mut Allocator) {
        let device = &ctx.device;
        for slot in self.frames.iter_mut() {
            if let Some(mut f) = slot.take() {
                f.lights.destroy(device, allocator);
                f.ubo.destroy(device, allocator);
            }
        }
        if let Some(mut t) = self.tlas.take() {
            if let Some(accel) = ctx.accel.as_ref() {
                unsafe { accel.destroy_acceleration_structure(t.handle, None) };
            }
            t.buffer.destroy(device, allocator);
        }
        for mut b in self.tlas_buffers.drain(..) {
            b.destroy(device, allocator);
        }
        if let Some(mut b) = self.instances.take() {
            b.destroy(device, allocator);
        }
        for (_, mut a) in self.blas.drain() {
            if let Some(accel) = ctx.accel.as_ref() {
                unsafe { accel.destroy_acceleration_structure(a.handle, None) };
            }
            a.buffer.destroy(device, allocator);
        }
        unsafe {
            device.destroy_descriptor_pool(self.pool, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

/// A host-visible SSBO seeded with `data`.
fn host_buffer(device: &ash::Device, alloc: &mut Allocator, data: &[u8], name: &str) -> Result<Buffer> {
    let mut buf = Buffer::new(
        device,
        alloc,
        data.len().max(16) as u64,
        vk::BufferUsageFlags::STORAGE_BUFFER,
        gpu_allocator::MemoryLocation::CpuToGpu,
        name,
    )
    .with_context(|| format!("creating {name}"))?;
    buf.write(0, data);
    Ok(buf)
}
