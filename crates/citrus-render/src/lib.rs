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
use glam::{Mat4, Vec3};
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

/// Maximum scene lights evaluated per frame by the standard shader. Matches
/// `MAX_LIGHTS` in standard.frag; the array lives in the frame UBO.
const MAX_LIGHTS: usize = 16;

/// One light in std140 layout (four vec4s). Mirrors `Light` in standard.frag.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct GpuLight {
    /// xyz world position, w = kind (0 directional, 1 point, 2 spot).
    pos_kind: [f32; 4],
    /// xyz travel direction (normalized), w = range.
    dir_range: [f32; 4],
    /// rgb color premultiplied by intensity, w = cos(outer half-angle).
    color: [f32; 4],
    /// x = cos(inner half-angle); yzw reserved.
    spot: [f32; 4],
}

/// Pack a scene light into its std140 GPU representation.
fn gpu_light(l: &LightInstance) -> GpuLight {
    let (kind, cos_outer, cos_inner) = match l.kind {
        LightKind::Directional => (0.0, -1.0, 1.0),
        LightKind::Point => (1.0, -1.0, 1.0),
        LightKind::Spot => (
            2.0,
            (l.spot_outer_deg.to_radians() * 0.5).cos(),
            (l.spot_inner_deg.to_radians() * 0.5).cos(),
        ),
    };
    let d = l.direction.normalize_or_zero();
    GpuLight {
        pos_kind: [l.position.x, l.position.y, l.position.z, kind],
        dir_range: [d.x, d.y, d.z, l.range],
        color: [
            l.color[0] * l.intensity,
            l.color[1] * l.intensity,
            l.color[2] * l.intensity,
            cos_outer,
        ],
        // spot = [cos_inner, shadow_base_layer (-1 = none), bias, view_count].
        // The shadow planner patches y/z/w when this light casts shadows.
        spot: [cos_inner, -1.0, 0.0, 0.0],
    }
}

/// Shadow view-projection matrices, one per shadow-map layer.
const MAX_SHADOW_VIEWS: usize = MAX_SHADOW_MAPS;

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
    misc: [f32; 4], // x = time in seconds, y = active light count
    /// Far view-space distance of each directional cascade (xyzw = up to 4).
    cascade_splits: [f32; 4],
    lights: [GpuLight; MAX_LIGHTS],
    shadow_vp: [[[f32; 4]; 4]; MAX_SHADOW_VIEWS],
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

/// Push constants for the shadow depth pass: the light's view-projection and
/// the object's model matrix (same 128-byte block, different interpretation).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ShadowPush {
    light_vp: [[f32; 4]; 4],
    model: [[f32; 4]; 4],
}

const _: () = assert!(size_of::<ShadowPush>() == pipeline::PUSH_CONSTANT_SIZE as usize);

/// One shadow-map render: which atlas layer + the light view-projection.
struct ShadowView {
    layer: usize,
    view_proj: Mat4,
}

/// Allocate shadow-map layers to shadow-casting lights, compute their
/// view-projections, and patch each light's shadow info into `lights[*].spot`.
/// Returns the per-view render list and the packed `shadow_vp` matrices.
fn plan_shadows(
    input: &FrameInput,
    lights: &mut [GpuLight; MAX_LIGHTS],
    count: usize,
) -> (Vec<ShadowView>, [[[f32; 4]; 4]; MAX_SHADOW_VIEWS], [f32; 4]) {
    let mut views = Vec::new();
    let mut shadow_vp = [Mat4::IDENTITY.to_cols_array_2d(); MAX_SHADOW_VIEWS];
    let mut cascade_splits = [0.0f32; 4];
    let mut next_layer = 0usize;
    let mut cascaded_done = false;
    let cam_center = input.camera.position;

    for li in 0..count.min(input.lights.len()) {
        let l = &input.lights[li];
        if !l.cast_shadows {
            continue;
        }
        let kind = lights[li].pos_kind[3]; // 0 dir, 1 point, 2 spot
        let dir = l.direction.normalize_or(Vec3::NEG_Y);
        let up = if dir.y.abs() > 0.99 { Vec3::Z } else { Vec3::Y };
        let range = l.range.max(1.0);

        // The first shadow-casting directional light is cascaded.
        let cascaded = kind == 0.0 && !cascaded_done;
        let needed = if kind == 1.0 {
            6
        } else if cascaded {
            NUM_CASCADES
        } else {
            1
        };
        if next_layer + needed > MAX_SHADOW_MAPS {
            continue; // out of shadow slots
        }

        let mats: Vec<Mat4> = if cascaded {
            let (mats, splits) = cascade_matrices(input, dir, up);
            cascade_splits = splits;
            cascaded_done = true;
            mats
        } else if kind == 0.0 {
            // Extra directional (non-cascaded): one tight box ahead of camera.
            let dist = input.shadow_distance.clamp(2.0, 500.0);
            let s = dist * 0.5;
            let cam_fwd = input
                .camera
                .view
                .inverse()
                .transform_vector3(Vec3::NEG_Z)
                .normalize_or(Vec3::NEG_Z);
            let center = cam_center + cam_fwd * s;
            let eye = center - dir * dist;
            let view = Mat4::look_to_rh(eye, dir, up);
            let proj = Mat4::orthographic_rh(-s, s, -s, s, 0.05, dist * 2.0);
            vec![proj * view]
        } else if kind == 2.0 {
            // Spot: perspective along the cone.
            let fov = l
                .spot_outer_deg
                .to_radians()
                .clamp(0.1, std::f32::consts::PI - 0.05);
            let view = Mat4::look_to_rh(l.position, dir, up);
            let proj = Mat4::perspective_rh(fov, 1.0, 0.1, range);
            vec![proj * view]
        } else {
            // Point: 6 cube faces (+X,-X,+Y,-Y,+Z,-Z), matching the shader's
            // face pick. The FOV is a touch wider than 90° so neighbouring
            // faces overlap: a fragment near a face boundary then still has
            // valid depth in its selected face's map, instead of the shader's
            // PCF taps spilling into the atlas border and showing a bright
            // seam between faces.
            let faces = [
                (Vec3::X, Vec3::NEG_Y),
                (Vec3::NEG_X, Vec3::NEG_Y),
                (Vec3::Y, Vec3::Z),
                (Vec3::NEG_Y, Vec3::NEG_Z),
                (Vec3::Z, Vec3::NEG_Y),
                (Vec3::NEG_Z, Vec3::NEG_Y),
            ];
            let fov = std::f32::consts::FRAC_PI_2 * 1.12; // ~101°, ~11° overlap
            let proj = Mat4::perspective_rh(fov, 1.0, 0.1, range);
            faces
                .iter()
                .map(|(fwd, up)| proj * Mat4::look_to_rh(l.position, *fwd, *up))
                .collect()
        };

        let base = next_layer;
        for (k, m) in mats.iter().enumerate() {
            let layer = base + k;
            shadow_vp[layer] = m.to_cols_array_2d();
            views.push(ShadowView {
                layer,
                view_proj: *m,
            });
        }
        lights[li].spot[1] = base as f32;
        lights[li].spot[2] = l.shadow_bias;
        lights[li].spot[3] = needed as f32;
        next_layer += needed;
    }
    (views, shadow_vp, cascade_splits)
}

/// Fit `NUM_CASCADES` directional shadow view-projections to depth slices of
/// the camera frustum (near→`shadow_distance`), each as a bounding sphere for
/// rotation-stable, tight coverage. Returns the matrices and the far
/// view-space distance of each cascade (for selection in the shader).
fn cascade_matrices(input: &FrameInput, dir: Vec3, up: Vec3) -> (Vec<Mat4>, [f32; 4]) {
    let near = 0.5f32;
    let far = input.shadow_distance.clamp(2.0, 500.0);
    let lambda = 0.7f32; // blend uniform↔logarithmic split scheme
    let inv_view = input.camera.view.inverse();
    let p = input.camera.proj;
    let tan_v = 1.0 / p.y_axis.y; // tan(fovy/2)
    let tan_h = 1.0 / p.x_axis.x; // tan(fovx/2)
    let corners_at = |d: f32| -> [Vec3; 4] {
        let h = d * tan_v;
        let w = d * tan_h;
        [
            inv_view.transform_point3(Vec3::new(-w, h, -d)),
            inv_view.transform_point3(Vec3::new(w, h, -d)),
            inv_view.transform_point3(Vec3::new(w, -h, -d)),
            inv_view.transform_point3(Vec3::new(-w, -h, -d)),
        ]
    };

    let mut splits = [far; 4];
    let mut mats = Vec::with_capacity(NUM_CASCADES);
    let mut prev = near;
    for (i, slot) in splits.iter_mut().enumerate().take(NUM_CASCADES) {
        let si = (i + 1) as f32 / NUM_CASCADES as f32;
        let uniform = near + (far - near) * si;
        let logd = near * (far / near).powf(si);
        let split = uniform * (1.0 - lambda) + logd * lambda;
        *slot = split;
        // Bounding sphere of this slice's 8 frustum corners.
        let near_c = corners_at(prev);
        let far_c = corners_at(split);
        let mut center = Vec3::ZERO;
        for c in near_c.iter().chain(far_c.iter()) {
            center += *c;
        }
        center /= 8.0;
        let mut radius = 0.0f32;
        for c in near_c.iter().chain(far_c.iter()) {
            radius = radius.max((*c - center).length());
        }
        radius = radius.max(0.01);
        let eye = center - dir * (radius * 2.0);
        let view = Mat4::look_to_rh(eye, dir, up);
        let proj = Mat4::orthographic_rh(-radius, radius, -radius, radius, 0.01, radius * 4.0);
        mats.push(proj * view);
        prev = split;
    }
    (mats, splits)
}

struct GpuMesh {
    vertex_buffer: Buffer,
    index_buffer: Buffer,
    index_count: u32,
}

/// Unity-style render queue: draws are ordered by this number, with named
/// breakpoints and gaps for fine-tuning. Transparent (≥ TRANSPARENT) sorts
/// back-to-front within the queue.
pub const RENDER_QUEUE_GEOMETRY: i32 = 2000;
pub const RENDER_QUEUE_ALPHA_TEST: i32 = 2450;
pub const RENDER_QUEUE_TRANSPARENT: i32 = 3000;

/// Default render queue for a material given its alpha mode.
pub fn default_render_queue(alpha: AlphaMode) -> i32 {
    match alpha {
        AlphaMode::Opaque => RENDER_QUEUE_GEOMETRY,
        AlphaMode::Cutout => RENDER_QUEUE_ALPHA_TEST,
        AlphaMode::Blend => RENDER_QUEUE_TRANSPARENT,
    }
}

struct Material {
    #[allow(dead_code)] // used by the editor's material list
    name: String,
    params: MaterialParams,
    features: MaterialFeatures,
    /// Draw-order priority (Unity render queue).
    render_queue: i32,
    has_normal_texture: bool,
    error: bool,
    /// Custom fragment shader; None = standard shader.
    custom_shader: Option<ShaderId>,
    /// Custom-shader push data (16 floats packed by property offsets).
    custom_data: [f32; 16],
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

/// Create a GPU-only 2D image + its backing allocation.
fn create_image(
    device: &ash::Device,
    allocator: &mut Allocator,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
    extent: vk::Extent2D,
    name: &str,
) -> Result<(vk::Image, Allocation)> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { device.create_image(&info, None)? };
    let requirements = unsafe { device.get_image_memory_requirements(image) };
    let allocation = allocator.allocate(&AllocationCreateDesc {
        name,
        requirements,
        location: MemoryLocation::GpuOnly,
        linear: false,
        allocation_scheme: AllocationScheme::GpuAllocatorManaged,
    })?;
    unsafe { device.bind_image_memory(image, allocation.memory(), allocation.offset())? };
    Ok((image, allocation))
}

fn create_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
    aspect: vk::ImageAspectFlags,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(aspect)
                .level_count(1)
                .layer_count(1),
        );
    Ok(unsafe { device.create_image_view(&info, None)? })
}

/// Fixed-size offscreen render target for the scene's main camera, shown in
/// the editor's Camera tab. Rendered each frame the tab needs it, then sampled
/// by egui as a registered user texture.
struct CameraPreview {
    extent: vk::Extent2D,
    color: vk::Image,
    color_view: vk::ImageView,
    color_alloc: Option<Allocation>,
    depth: vk::Image,
    depth_view: vk::ImageView,
    depth_alloc: Option<Allocation>,
    /// Per-frame-in-flight camera UBOs + their set-0 descriptor sets.
    ubos: Vec<Buffer>,
    sets: Vec<vk::DescriptorSet>,
    ubo_pool: vk::DescriptorPool,
    /// egui user-texture plumbing (combined image sampler) for the color image.
    egui_layout: vk::DescriptorSetLayout,
    egui_pool: vk::DescriptorPool,
    texture_id: egui::TextureId,
}

impl CameraPreview {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: &GpuContext,
        allocator: &mut Allocator,
        set0_layout: vk::DescriptorSetLayout,
        color_format: vk::Format,
        sampler: vk::Sampler,
        shadow_view: vk::ImageView,
        shadow_sampler: vk::Sampler,
        egui: &mut egui_ash_renderer::Renderer,
    ) -> Result<Self> {
        let device = &ctx.device;
        let extent = vk::Extent2D {
            width: 1280,
            height: 720,
        };

        let (color, color_alloc) = create_image(
            device,
            allocator,
            color_format,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            extent,
            "camera preview color",
        )?;
        let color_view = create_view(device, color, color_format, vk::ImageAspectFlags::COLOR)?;
        let (depth, depth_alloc) = create_image(
            device,
            allocator,
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
            extent,
            "camera preview depth",
        )?;
        let depth_view = create_view(device, depth, DEPTH_FORMAT, vk::ImageAspectFlags::DEPTH)?;

        // Own pool for the per-frame camera view UBOs (binding 0) + shadow
        // sampler (binding 1).
        let ubo_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::UNIFORM_BUFFER,
                            descriptor_count: FRAMES_IN_FLIGHT as u32,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                            descriptor_count: FRAMES_IN_FLIGHT as u32,
                        },
                    ]),
                None,
            )?
        };
        let mut ubos = Vec::new();
        let mut sets = Vec::new();
        for i in 0..FRAMES_IN_FLIGHT {
            let buffer = Buffer::new(
                device,
                allocator,
                size_of::<FrameUbo>() as u64,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                MemoryLocation::CpuToGpu,
                &format!("camera ubo {i}"),
            )?;
            let layouts = [set0_layout];
            let set = unsafe {
                device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(ubo_pool)
                        .set_layouts(&layouts),
                )?[0]
            };
            let buffer_info = [vk::DescriptorBufferInfo::default()
                .buffer(buffer.handle)
                .range(size_of::<FrameUbo>() as u64)];
            let shadow_info = [vk::DescriptorImageInfo::default()
                .sampler(shadow_sampler)
                .image_view(shadow_view)
                .image_layout(vk::ImageLayout::DEPTH_READ_ONLY_OPTIMAL)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&buffer_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&shadow_info),
            ];
            unsafe { device.update_descriptor_sets(&writes, &[]) };
            ubos.push(buffer);
            sets.push(set);
        }

        // Register the color image with egui as a user texture. The set layout
        // is structurally identical to egui's internal one, so the set is
        // compatible with its pipeline.
        let egui_layout = egui_ash_renderer::vulkan::create_vulkan_descriptor_set_layout(device)
            .map_err(|e| anyhow::anyhow!("camera preview descriptor layout: {e}"))?;
        let egui_pool = egui_ash_renderer::vulkan::create_vulkan_descriptor_pool(device, 1)
            .map_err(|e| anyhow::anyhow!("camera preview descriptor pool: {e}"))?;
        let egui_set = egui_ash_renderer::vulkan::create_vulkan_descriptor_set(
            device,
            egui_layout,
            egui_pool,
            color_view,
            sampler,
        )
        .map_err(|e| anyhow::anyhow!("camera preview descriptor set: {e}"))?;
        let texture_id = egui.add_user_texture(egui_set);

        Ok(Self {
            extent,
            color,
            color_view,
            color_alloc: Some(color_alloc),
            depth,
            depth_view,
            depth_alloc: Some(depth_alloc),
            ubos,
            sets,
            ubo_pool,
            egui_layout,
            egui_pool,
            texture_id,
        })
    }

    fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        for b in &mut self.ubos {
            b.destroy(device, allocator);
        }
        unsafe {
            device.destroy_descriptor_pool(self.ubo_pool, None);
            device.destroy_descriptor_pool(self.egui_pool, None);
            device.destroy_descriptor_set_layout(self.egui_layout, None);
            device.destroy_image_view(self.color_view, None);
            device.destroy_image(self.color, None);
            device.destroy_image_view(self.depth_view, None);
            device.destroy_image(self.depth, None);
        }
        if let Some(a) = self.color_alloc.take() {
            let _ = allocator.free(a);
        }
        if let Some(a) = self.depth_alloc.take() {
            let _ = allocator.free(a);
        }
    }
}

/// Shadow-map array: each shadow-casting light reserves layers (1 for
/// directional/spot, 6 for point). Sampled as a `sampler2DArrayShadow`.
/// Shadow-map array layers. Enough for a cascaded directional (NUM_CASCADES)
/// plus several extra lights. 12 layers × 2048² × 4 bytes (D32) ≈ 192 MB.
const MAX_SHADOW_MAPS: usize = 12;
/// Default shadow-map resolution per layer (runtime-configurable).
const SHADOW_DIM: u32 = 2048;
/// Cascade count for the directional (sun) shadow.
const NUM_CASCADES: usize = 4;

struct ShadowAtlas {
    /// Resolution per layer (pixels); runtime-configurable.
    dim: u32,
    image: vk::Image,
    alloc: Option<Allocation>,
    /// Whole-array view used for sampling (set 0 binding 1).
    array_view: vk::ImageView,
    /// One single-layer view per array layer, used as a render target.
    layer_views: Vec<vk::ImageView>,
    /// Depth-compare sampler (PCF).
    sampler: vk::Sampler,
}

impl ShadowAtlas {
    fn new(ctx: &GpuContext, allocator: &mut Allocator, dim: u32) -> Result<Self> {
        let device = &ctx.device;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(DEPTH_FORMAT)
            .extent(vk::Extent3D {
                width: dim,
                height: dim,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(MAX_SHADOW_MAPS as u32)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&info, None)? };
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let alloc = allocator.allocate(&AllocationCreateDesc {
            name: "shadow atlas",
            requirements,
            location: MemoryLocation::GpuOnly,
            linear: false,
            allocation_scheme: AllocationScheme::GpuAllocatorManaged,
        })?;
        unsafe { device.bind_image_memory(image, alloc.memory(), alloc.offset())? };

        let array_view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D_ARRAY)
                    .format(DEPTH_FORMAT)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::DEPTH)
                            .level_count(1)
                            .layer_count(MAX_SHADOW_MAPS as u32),
                    ),
                None,
            )?
        };
        let mut layer_views = Vec::with_capacity(MAX_SHADOW_MAPS);
        for layer in 0..MAX_SHADOW_MAPS as u32 {
            layer_views.push(unsafe {
                device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(DEPTH_FORMAT)
                        .subresource_range(
                            vk::ImageSubresourceRange::default()
                                .aspect_mask(vk::ImageAspectFlags::DEPTH)
                                .level_count(1)
                                .base_array_layer(layer)
                                .layer_count(1),
                        ),
                    None,
                )?
            });
        }

        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_BORDER)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_BORDER)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_BORDER)
                    .border_color(vk::BorderColor::FLOAT_OPAQUE_WHITE)
                    .compare_enable(true)
                    .compare_op(vk::CompareOp::LESS_OR_EQUAL),
                None,
            )?
        };

        Ok(Self {
            dim,
            image,
            alloc: Some(alloc),
            array_view,
            layer_views,
            sampler,
        })
    }

    fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_sampler(self.sampler, None);
            device.destroy_image_view(self.array_view, None);
            for view in self.layer_views.drain(..) {
                device.destroy_image_view(view, None);
            }
            device.destroy_image(self.image, None);
        }
        if let Some(a) = self.alloc.take() {
            let _ = allocator.free(a);
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
    /// Fullscreen skybox: descriptor set (set 1) + optional equirect texture.
    /// When `skybox_has_texture` is false the shader draws a procedural sky.
    skybox_set: vk::DescriptorSet,
    skybox_texture: Option<GpuTexture>,
    skybox_has_texture: bool,
    /// Shadow-map array (set 0 binding 1) + per-layer render targets.
    shadow_atlas: ShadowAtlas,

    meshes: Vec<GpuMesh>,
    textures: Vec<GpuTexture>,
    materials: Vec<Material>,

    egui: Option<egui_ash_renderer::Renderer>,
    /// Texture frees deferred until those frames can no longer be in flight.
    egui_free_queue: VecDeque<Vec<egui::TextureId>>,
    /// Offscreen main-camera target, created lazily the first frame the Camera
    /// tab asks for it.
    camera_preview: Option<CameraPreview>,
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
            // Must match the device: buffer device address is enabled only
            // when ray tracing (the lighting bake) is available.
            buffer_device_address: ctx.ray_tracing(),
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
        // wholesale when a scene is unloaded. Each frame set 0 also carries the
        // shadow-map sampler at binding 1.
        let frame_pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: FRAMES_IN_FLIGHT as u32,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: FRAMES_IN_FLIGHT as u32,
            },
        ];
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

        // Shadow atlas + wire its array view into every frame set (binding 1).
        let shadow_atlas = ShadowAtlas::new(&ctx, &mut allocator.lock().unwrap(), SHADOW_DIM)?;
        for &set in &frame_sets {
            let info = [vk::DescriptorImageInfo::default()
                .sampler(shadow_atlas.sampler)
                .image_view(shadow_atlas.array_view)
                .image_layout(vk::ImageLayout::DEPTH_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { ctx.device.update_descriptor_sets(&[write], &[]) };
        }

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

        // Skybox descriptor set (set 1 layout): the equirect texture sits in
        // slot 0; default white fills the unused slots. Bound for the
        // fullscreen skybox pass.
        let skybox_set = unsafe {
            let layouts = [pipeline_cache.set1_layout];
            let set = ctx.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(material_pool)
                    .set_layouts(&layouts),
            )?[0];
            let infos: Vec<[vk::DescriptorImageInfo; 1]> = (0..4)
                .map(|i| {
                    [vk::DescriptorImageInfo::default()
                        .sampler(sampler)
                        .image_view(default_textures[i].view)
                        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
                })
                .collect();
            let writes: Vec<_> = infos
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
            ctx.device.update_descriptor_sets(&writes, &[]);
            set
        };

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
            skybox_set,
            skybox_texture: None,
            skybox_has_texture: false,
            shadow_atlas,
            meshes: Vec::new(),
            textures: Vec::new(),
            materials: Vec::new(),
            egui: Some(egui),
            egui_free_queue: VecDeque::new(),
            camera_preview: None,
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
            render_queue: default_render_queue(features.alpha_mode),
            has_normal_texture: desc.normal.is_some(),
            error: desc.error,
            custom_shader: None,
            custom_data: [0.0; 16],
            set,
        });
        Ok(MaterialHandle(self.materials.len() - 1))
    }

    /// Set (or clear) the equirectangular skybox texture. `None` reverts to
    /// the procedural gradient sky. The old texture is dropped after the GPU
    /// goes idle to stay safe.
    pub fn set_skybox(&mut self, data: Option<&TextureData>) -> Result<()> {
        match data {
            Some(data) => {
                let allocator = self.allocator();
                let texture = GpuTexture::upload(
                    &self.ctx.device,
                    &mut allocator.lock().unwrap(),
                    self.command_pool,
                    self.ctx.queue,
                    data,
                )?;
                let info = [vk::DescriptorImageInfo::default()
                    .sampler(self.sampler)
                    .image_view(texture.view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let write = vk::WriteDescriptorSet::default()
                    .dst_set(self.skybox_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&info);
                unsafe {
                    let _ = self.ctx.device.device_wait_idle();
                    self.ctx.device.update_descriptor_sets(&[write], &[]);
                }
                if let Some(mut old) = self.skybox_texture.take() {
                    old.destroy(&self.ctx.device, &mut self.allocator().lock().unwrap());
                }
                self.skybox_texture = Some(texture);
                self.skybox_has_texture = true;
            }
            None => {
                unsafe {
                    let _ = self.ctx.device.device_wait_idle();
                }
                let info = [vk::DescriptorImageInfo::default()
                    .sampler(self.sampler)
                    .image_view(self.default_textures[0].view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let write = vk::WriteDescriptorSet::default()
                    .dst_set(self.skybox_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&info);
                unsafe { self.ctx.device.update_descriptor_sets(&[write], &[]) };
                if let Some(mut old) = self.skybox_texture.take() {
                    old.destroy(&self.ctx.device, &mut self.allocator().lock().unwrap());
                }
                self.skybox_has_texture = false;
            }
        }
        Ok(())
    }

    /// Draw the fullscreen skybox into the active rendering pass (call after
    /// begin_rendering + viewport/scissor, before scene geometry).
    fn record_skybox(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        frame_set: vk::DescriptorSet,
    ) -> Result<()> {
        let pipeline = self.pipeline_cache.get(device, PipelineKey::skybox())?;
        let mut push = PushData {
            model: glam::Mat4::IDENTITY.to_cols_array_2d(),
            base_color: [0.0; 4],
            emission: [0.0; 4],
            params0: [0.0; 4],
            params1: [0.0; 4],
        };
        push.params0[0] = if self.skybox_has_texture { 1.0 } else { 0.0 };
        unsafe {
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_cache.layout,
                0,
                &[frame_set, self.skybox_set],
                &[],
            );
            device.cmd_push_constants(
                cb,
                self.pipeline_cache.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                bytemuck::bytes_of(&push),
            );
            device.cmd_draw(cb, 3, 1, 0, 0);
        }
        Ok(())
    }

    /// Re-create the shadow atlas at a new per-layer resolution (clamped to a
    /// sane range). No-op when unchanged. Re-binds the array view on every
    /// frame + camera-preview descriptor set.
    pub fn set_shadow_resolution(&mut self, dim: u32) -> Result<()> {
        let dim = dim.clamp(256, 8192);
        if dim == self.shadow_atlas.dim {
            return Ok(());
        }
        let allocator = match &self.allocator {
            Some(a) => a.clone(),
            None => return Ok(()),
        };
        unsafe {
            let _ = self.ctx.device.device_wait_idle();
        }
        let new_atlas = ShadowAtlas::new(&self.ctx, &mut allocator.lock().unwrap(), dim)?;
        let mut old = std::mem::replace(&mut self.shadow_atlas, new_atlas);
        old.destroy(&self.ctx.device, &mut allocator.lock().unwrap());

        // Re-point binding 1 on the frame sets and the camera-preview sets.
        let info = [vk::DescriptorImageInfo::default()
            .sampler(self.shadow_atlas.sampler)
            .image_view(self.shadow_atlas.array_view)
            .image_layout(vk::ImageLayout::DEPTH_READ_ONLY_OPTIMAL)];
        let mut sets: Vec<vk::DescriptorSet> = self.frame_sets.clone();
        if let Some(preview) = &self.camera_preview {
            sets.extend_from_slice(&preview.sets);
        }
        for set in sets {
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { self.ctx.device.update_descriptor_sets(&[write], &[]) };
        }
        Ok(())
    }

    /// Register a runtime-compiled custom fragment shader (SPIR-V bytes).
    pub fn register_shader(&mut self, spirv: &[u8]) -> Result<ShaderId> {
        Ok(ShaderId(
            self.pipeline_cache
                .register_custom(&self.ctx.device, spirv)?,
        ))
    }

    /// Switch a material between the standard shader (None) and a custom one.
    pub fn set_material_shader(&mut self, handle: MaterialHandle, shader: Option<ShaderId>) {
        self.materials[handle.0].custom_shader = shader;
    }

    /// Custom-shader property values, delivered as push constants per draw.
    pub fn set_material_custom_data(&mut self, handle: MaterialHandle, data: [f32; 16]) {
        self.materials[handle.0].custom_data = data;
    }

    /// Flag a material as broken; it renders with the error swirl shader.
    pub fn set_material_error(&mut self, handle: MaterialHandle, error: bool) {
        self.materials[handle.0].error = error;
    }

    /// Set a material's render-queue priority (draw order).
    pub fn set_material_render_queue(&mut self, handle: MaterialHandle, queue: i32) {
        self.materials[handle.0].render_queue = queue;
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
        if let Some(egui_draw) = &input.egui
            && !egui_draw.textures_delta.set.is_empty()
        {
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
        //
        // Pack the scene lights into the std140 array. When the scene has no
        // light objects, synthesize one directional light from the key-light
        // fallback so existing scenes look unchanged. The key-light fields
        // (light_dir/light_color) stay populated for custom-shader macros.
        let mut lights = [GpuLight::default(); MAX_LIGHTS];
        let mut count = 0usize;
        for l in input.lights.iter().take(MAX_LIGHTS) {
            lights[count] = gpu_light(l);
            count += 1;
        }
        if count == 0 {
            // No scene lights: keep the legacy single directional so existing
            // scenes render identically.
            lights[0] = gpu_light(&LightInstance {
                kind: LightKind::Directional,
                position: Vec3::ZERO,
                direction: input.light.direction,
                color: input.light.color,
                intensity: input.light.intensity,
                range: 0.0,
                spot_inner_deg: 0.0,
                spot_outer_deg: 0.0,
                cast_shadows: false,
                shadow_bias: 0.0,
            });
            count = 1;
        }

        // Plan shadow maps (allocates atlas layers, patches lights[*].spot)
        // before building the UBOs that carry the shadow matrices.
        let (shadow_views, shadow_vp, cascade_splits) = plan_shadows(input, &mut lights, count);

        let ubo = frame_ubo(
            &input.camera,
            input,
            lights,
            count,
            shadow_vp,
            cascade_splits,
        );
        self.frame_ubos[self.frame_index].write(0, bytemuck::bytes_of(&ubo));

        // Lazily create the offscreen camera target the first time the Camera
        // tab requests it, then write its view UBO for this frame.
        let render_camera_preview = input.camera_preview.is_some();
        if let Some(camera) = &input.camera_preview {
            if self.camera_preview.is_none() {
                let allocator = self.allocator.clone();
                if let Some(allocator) = allocator {
                    let preview = CameraPreview::new(
                        &self.ctx,
                        &mut allocator.lock().unwrap(),
                        self.pipeline_cache.set0_layout,
                        self.swapchain.format.format,
                        self.sampler,
                        self.shadow_atlas.array_view,
                        self.shadow_atlas.sampler,
                        self.egui.as_mut().unwrap(),
                    )?;
                    self.camera_preview = Some(preview);
                }
            }
            if let Some(preview) = &mut self.camera_preview {
                let cam_ubo = frame_ubo(camera, input, lights, count, shadow_vp, cascade_splits);
                preview.ubos[self.frame_index].write(0, bytemuck::bytes_of(&cam_ubo));
            }
        }

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
        // Primary key: render queue (Unity-style). Within the same queue,
        // transparent draws (queue ≥ TRANSPARENT) sort back-to-front so blends
        // composite correctly; opaque queues keep their order.
        let queue = |i: usize| self.materials[input.draws[i].material.0].render_queue;
        order.sort_by(|&a, &b| {
            queue(a).cmp(&queue(b)).then_with(|| {
                if queue(a) >= RENDER_QUEUE_TRANSPARENT {
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

            // Shadow maps first: both the camera-preview and the main scene
            // pass sample them, so they must be rendered + made readable up
            // front.
            self.record_shadows(&device, cb, &shadow_views, input.draws)?;

            // Offscreen main-camera pass next, so the swapchain's egui pass
            // can sample the result this frame.
            if render_camera_preview && self.camera_preview.is_some() {
                self.record_camera_preview(&device, cb, &order, input, input.clear_color)?;
            }

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

            // Skybox first (background), then the scene over it.
            if input.draw_skybox {
                self.record_skybox(&device, cb, frame_set)?;
            }
            let mut stats = self.record_scene_draws(&device, cb, &order, input, frame_set)?;

            // Selection outlines: inverted hull pass over highlighted draws.
            let outline_draws: Vec<usize> = (0..input.draws.len())
                .filter(|&i| input.draws[i].highlight > 0.0)
                .collect();
            if !outline_draws.is_empty() {
                stats.outline_draws += outline_draws.len() as u32 * 2;
                stats.draw_calls += outline_draws.len() as u32 * 2;
                stats.pipeline_binds += 2;

                // The outline must never be occluded: wipe the scene's depth
                // and re-lay only the selected objects' depth (prepass below),
                // so the hull can only be masked by the object itself. Safe
                // because nothing after the outline reads scene depth (egui
                // renders with depth testing off).
                let clear = vk::ClearAttachment {
                    aspect_mask: vk::ImageAspectFlags::DEPTH,
                    color_attachment: 0,
                    clear_value: vk::ClearValue {
                        depth_stencil: vk::ClearDepthStencilValue {
                            depth: 1.0,
                            stencil: 0,
                        },
                    },
                };
                let clear_rect = vk::ClearRect {
                    rect: vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    },
                    base_array_layer: 0,
                    layer_count: 1,
                };
                device.cmd_clear_attachments(cb, &[clear], &[clear_rect]);

                // Depth-only prepass: re-lays the selected objects' own depth
                // (also needed for transparent objects, which never write
                // depth in the main pass — without it the hull's interior
                // shows through and the whole object turns purple).
                let depth_pipeline = self
                    .pipeline_cache
                    .get(&device, PipelineKey::depth_only())?;
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
                        params0: draw.mesh_center.extend(0.0).to_array(),
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

        if let Some(egui_draw) = &input.egui
            && !egui_draw.textures_delta.free.is_empty()
        {
            self.egui_free_queue
                .push_back(egui_draw.textures_delta.free.clone());
        }

        self.frame_index = (self.frame_index + 1) % FRAMES_IN_FLIGHT;
        Ok(())
    }
}

/// Build the per-frame UBO for a given camera, sharing the already-packed
/// scene lights and ambient/key-light fallback.
#[allow(clippy::too_many_arguments)]
fn frame_ubo(
    camera: &CameraData,
    input: &FrameInput,
    lights: [GpuLight; MAX_LIGHTS],
    count: usize,
    shadow_vp: [[[f32; 4]; 4]; MAX_SHADOW_VIEWS],
    cascade_splits: [f32; 4],
) -> FrameUbo {
    FrameUbo {
        view: camera.view.to_cols_array_2d(),
        proj: camera.proj.to_cols_array_2d(),
        view_proj: (camera.proj * camera.view).to_cols_array_2d(),
        camera_pos: camera.position.extend(1.0).to_array(),
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
        misc: [input.time, count as f32, input.shadow_pcf_texel, 0.0],
        cascade_splits,
        lights,
        shadow_vp,
    }
}

impl Renderer {
    /// Record the opaque+transparent scene draws (no outline, no egui) for one
    /// pass, binding `frame_set` as set 0. Returns draw statistics.
    fn record_scene_draws(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        order: &[usize],
        input: &FrameInput,
        frame_set: vk::DescriptorSet,
    ) -> Result<RenderStats> {
        let mut stats = RenderStats::default();
        let mut materials_seen: Vec<bool> = vec![false; self.materials.len()];
        let mut bound_pipeline = vk::Pipeline::null();
        for &i in order {
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
            if let Some(seen) = materials_seen.get_mut(draw.material.0)
                && !*seen
            {
                *seen = true;
                stats.materials_drawn += 1;
            }

            let key = if material.error {
                PipelineKey::error()
            } else {
                let mut key = PipelineKey::from_features(&material.features);
                if let Some(shader) = material.custom_shader {
                    key.shader = shader.0 as u32 + 1;
                }
                key
            };
            let pipeline = self.pipeline_cache.get(device, key)?;
            if pipeline != bound_pipeline {
                stats.pipeline_binds += 1;
                device_bind(device, cb, pipeline);
                unsafe {
                    device.cmd_bind_descriptor_sets(
                        cb,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.pipeline_cache.layout,
                        0,
                        &[frame_set],
                        &[],
                    );
                }
                bound_pipeline = pipeline;
            }
            unsafe {
                device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_cache.layout,
                    1,
                    &[material.set],
                    &[],
                );
            }

            let p = &material.params;
            let push = if material.custom_shader.is_some() && !material.error {
                let d = &material.custom_data;
                PushData {
                    model: draw.transform.to_cols_array_2d(),
                    base_color: d[0..4].try_into().unwrap(),
                    emission: d[4..8].try_into().unwrap(),
                    params0: d[8..12].try_into().unwrap(),
                    params1: d[12..16].try_into().unwrap(),
                }
            } else {
                PushData {
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
                }
            };
            unsafe {
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
        Ok(stats)
    }

    /// Record the offscreen main-camera pass into the preview target. Assumes
    /// the preview exists and its UBO for this frame has been written.
    fn record_camera_preview(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        order: &[usize],
        input: &FrameInput,
        clear_color: [f32; 4],
    ) -> Result<()> {
        let preview = self.camera_preview.as_ref().unwrap();
        let extent = preview.extent;
        let color = preview.color;
        let color_view = preview.color_view;
        let depth = preview.depth;
        let depth_view = preview.depth_view;
        let frame_set = preview.sets[self.frame_index];

        unsafe {
            image_barrier(
                device,
                cb,
                color,
                vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::TOP_OF_PIPE,
                vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            );
            image_barrier(
                device,
                cb,
                depth,
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
                        float32: clear_color,
                    },
                });
            let depth_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(depth_view)
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
        }

        if input.draw_skybox {
            self.record_skybox(device, cb, frame_set)?;
        }
        self.record_scene_draws(device, cb, order, input, frame_set)?;

        unsafe {
            device.cmd_end_rendering(cb);
            // Color attachment -> shader read for egui to sample this frame.
            image_barrier(
                device,
                cb,
                color,
                vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            );
        }
        Ok(())
    }

    /// Render every shadow view into its atlas layer, then transition the
    /// whole atlas to a sampleable depth-read layout. Always leaves the atlas
    /// readable (even with zero casters) so the scene pass can sample it.
    fn record_shadows(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        views: &[ShadowView],
        draws: &[DrawCmd],
    ) -> Result<()> {
        let image = self.shadow_atlas.image;
        let dim = self.shadow_atlas.dim;
        let layers = MAX_SHADOW_MAPS as u32;
        let to_attachment = |old: vk::ImageLayout| {
            let barrier = vk::ImageMemoryBarrier2::default()
                .image(image)
                .old_layout(old)
                .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
                .src_access_mask(vk::AccessFlags2::empty())
                .dst_stage_mask(
                    vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                        | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                )
                .dst_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::DEPTH)
                        .level_count(1)
                        .layer_count(layers),
                );
            let barriers = [barrier];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&barriers);
            unsafe { device.cmd_pipeline_barrier2(cb, &dep) };
        };
        to_attachment(vk::ImageLayout::UNDEFINED);

        if !views.is_empty() {
            let pipeline = self.pipeline_cache.get(device, PipelineKey::shadow())?;
            for view in views {
                let depth_view = self.shadow_atlas.layer_views[view.layer];
                let depth_attachment = vk::RenderingAttachmentInfo::default()
                    .image_view(depth_view)
                    .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::CLEAR)
                    .store_op(vk::AttachmentStoreOp::STORE)
                    .clear_value(vk::ClearValue {
                        depth_stencil: vk::ClearDepthStencilValue {
                            depth: 1.0,
                            stencil: 0,
                        },
                    });
                let extent = vk::Extent2D {
                    width: dim,
                    height: dim,
                };
                let rendering_info = vk::RenderingInfo::default()
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    })
                    .layer_count(1)
                    .depth_attachment(&depth_attachment);
                unsafe {
                    device.cmd_begin_rendering(cb, &rendering_info);
                    // Positive-height viewport: shadow uv = ndc*0.5+0.5 matches.
                    let viewport = vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: dim as f32,
                        height: dim as f32,
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
                    device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
                    for draw in draws {
                        // Transparent (alpha-blended) objects don't cast solid
                        // shadows.
                        if self.materials[draw.material.0].features.alpha_mode == AlphaMode::Blend {
                            continue;
                        }
                        let mesh = &self.meshes[draw.mesh.0];
                        let push = ShadowPush {
                            light_vp: view.view_proj.to_cols_array_2d(),
                            model: draw.transform.to_cols_array_2d(),
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
                    device.cmd_end_rendering(cb);
                }
            }
        }

        // Whole atlas -> depth-read for sampling in the scene passes.
        let barrier = vk::ImageMemoryBarrier2::default()
            .image(image)
            .old_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::DEPTH_READ_ONLY_OPTIMAL)
            .src_stage_mask(vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS)
            .src_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::DEPTH)
                    .level_count(1)
                    .layer_count(layers),
            );
        let barriers = [barrier];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&barriers);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };
        Ok(())
    }

    /// egui texture id + pixel size of the main-camera preview, if it exists.
    pub fn camera_preview_texture(&self) -> Option<(egui::TextureId, [f32; 2])> {
        self.camera_preview.as_ref().map(|p| {
            (
                p.texture_id,
                [p.extent.width as f32, p.extent.height as f32],
            )
        })
    }
}

/// Bind a graphics pipeline (small helper to keep the draw loop tidy).
fn device_bind(device: &ash::Device, cb: vk::CommandBuffer, pipeline: vk::Pipeline) {
    unsafe {
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
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
            if let Some(texture) = &mut self.skybox_texture {
                texture.destroy(device, &mut alloc);
            }
            self.shadow_atlas.destroy(device, &mut alloc);
            for ubo in &mut self.frame_ubos {
                ubo.destroy(device, &mut alloc);
            }
            if let Some(mut preview) = self.camera_preview.take() {
                preview.destroy(device, &mut alloc);
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
