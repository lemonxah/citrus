//! GPU skinning: a compute pass that writes skinned vertices straight into a
//! mesh's vertex buffer, so every other pass (shadows, main, planar, VR) draws
//! it unchanged. Palettes are queued on the CPU and uploaded into the current
//! frame slot's buffer after that slot's fence has retired.

use std::collections::HashMap;

use anyhow::Result;
use ash::vk;
use glam::Mat4;
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::Allocator;

use crate::alloc::Buffer;
use crate::frame::FRAMES_IN_FLIGHT;
use crate::types::Vertex;

// skin.comp hard-codes the vertex layout (26 floats, joints at 18, weights at 22).
const _: () = assert!(std::mem::size_of::<Vertex>() == 104);

const SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/skin.comp.spv"));

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Push {
    vertex_count: u32,
    stride: u32,
    joint_count: u32,
}

fn mat_floats(m: &[Mat4]) -> Vec<f32> {
    m.iter().flat_map(|m| m.to_cols_array()).collect()
}

struct GpuSkin {
    base: Buffer,
    palettes: Vec<Buffer>,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    vertex_count: u32,
    joint_count: u32,
}

pub(crate) struct GpuSkinner {
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    /// Keyed by mesh index.
    skins: HashMap<usize, GpuSkin>,
    pending: Vec<(usize, Vec<Mat4>)>,
}

impl GpuSkinner {
    pub fn new(device: &ash::Device) -> Result<Self> {
        let binding = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [binding(0), binding(1), binding(2)];
        let set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?
        };
        let layouts = [set_layout];
        let ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size(std::mem::size_of::<Push>() as u32)];
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&ranges),
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
        Ok(Self {
            set_layout,
            pipeline_layout,
            pipeline,
            skins: HashMap::new(),
            pending: Vec::new(),
        })
    }

    /// Register a mesh whose vertex buffer (`out`, STORAGE usage) is rewritten
    /// by the skinning pass from `base` (device-local copy of the bind pose).
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &mut self,
        device: &ash::Device,
        allocator: &mut Allocator,
        mesh: usize,
        out: vk::Buffer,
        base: Buffer,
        vertex_count: u32,
        joint_count: usize,
    ) -> Result<()> {
        let joint_count = joint_count.max(1);
        let size = (joint_count * std::mem::size_of::<Mat4>()) as u64;
        let mut palettes = Vec::with_capacity(FRAMES_IN_FLIGHT);
        for _ in 0..FRAMES_IN_FLIGHT {
            let mut b = Buffer::new(
                device,
                allocator,
                size,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                MemoryLocation::CpuToGpu,
                "skin palette",
            )?;
            b.write(0, bytemuck::cast_slice(&mat_floats(&vec![Mat4::IDENTITY; joint_count])));
            palettes.push(b);
        }
        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&[vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::STORAGE_BUFFER,
                        descriptor_count: 3 * FRAMES_IN_FLIGHT as u32,
                    }]),
                None,
            )?
        };
        let layouts = vec![self.set_layout; FRAMES_IN_FLIGHT];
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )?
        };
        for (f, &set) in sets.iter().enumerate() {
            let infos = [
                [vk::DescriptorBufferInfo::default().buffer(base.handle).range(vk::WHOLE_SIZE)],
                [vk::DescriptorBufferInfo::default().buffer(out).range(vk::WHOLE_SIZE)],
                [vk::DescriptorBufferInfo::default()
                    .buffer(palettes[f].handle)
                    .range(vk::WHOLE_SIZE)],
            ];
            let writes: Vec<_> = infos
                .iter()
                .enumerate()
                .map(|(b, info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(b as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(info)
                })
                .collect();
            unsafe { device.update_descriptor_sets(&writes, &[]) };
        }
        self.skins.insert(
            mesh,
            GpuSkin {
                base,
                palettes,
                pool,
                sets,
                vertex_count,
                joint_count: joint_count as u32,
            },
        );
        Ok(())
    }

    /// Queue this frame's pose for a mesh (applied by the next `record`).
    pub fn queue(&mut self, mesh: usize, palette: &[Mat4]) {
        if self.skins.contains_key(&mesh) {
            self.pending.retain(|(m, _)| *m != mesh);
            self.pending.push((mesh, palette.to_vec()));
        }
    }

    /// Upload queued palettes into frame slot `frame` and dispatch, followed by
    /// a barrier so vertex fetch sees the results. Call after the slot's fence.
    pub fn record(&mut self, device: &ash::Device, cb: vk::CommandBuffer, frame: usize) {
        if self.pending.is_empty() {
            return;
        }
        let stride = (std::mem::size_of::<Vertex>() / 4) as u32;
        unsafe { device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline) };
        for (mesh, palette) in self.pending.drain(..) {
            let Some(skin) = self.skins.get_mut(&mesh) else {
                continue;
            };
            let n = palette.len().min(skin.joint_count as usize);
            skin.palettes[frame].write(0, bytemuck::cast_slice(&mat_floats(&palette[..n])));
            let push = Push {
                vertex_count: skin.vertex_count,
                stride,
                joint_count: n as u32,
            };
            unsafe {
                device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::COMPUTE,
                    self.pipeline_layout,
                    0,
                    &[skin.sets[frame]],
                    &[],
                );
                device.cmd_push_constants(
                    cb,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::COMPUTE,
                    0,
                    bytemuck::bytes_of(&push),
                );
                device.cmd_dispatch(cb, skin.vertex_count.div_ceil(64), 1, 1);
            }
        }
        let barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::VERTEX_ATTRIBUTE_INPUT | vk::PipelineStageFlags2::COMPUTE_SHADER)
            .dst_access_mask(vk::AccessFlags2::VERTEX_ATTRIBUTE_READ | vk::AccessFlags2::SHADER_STORAGE_READ);
        let barriers = [barrier];
        unsafe {
            device.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().memory_barriers(&barriers));
        }
    }

    /// Drop every registered mesh (scene reset); the pipeline stays.
    pub fn clear(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        self.pending.clear();
        for (_, mut skin) in self.skins.drain() {
            skin.base.destroy(device, allocator);
            for mut p in skin.palettes.drain(..) {
                p.destroy(device, allocator);
            }
            unsafe { device.destroy_descriptor_pool(skin.pool, None) };
        }
    }

    pub fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        for (_, mut skin) in self.skins.drain() {
            skin.base.destroy(device, allocator);
            for mut p in skin.palettes.drain(..) {
                p.destroy(device, allocator);
            }
            unsafe { device.destroy_descriptor_pool(skin.pool, None) };
        }
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}
