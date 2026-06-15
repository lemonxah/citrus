//! citrus-render: ash-based Vulkan renderer.
//!
//! M2 scope: depth-tested mesh rendering with the citrus standard shader
//! (phase 1, variant cache via specialization constants), texture/material
//! system, and an egui overlay pass for the in-engine inspector.

mod alloc;
mod bake;
mod context;
mod frame;
mod gpu_gi;
mod pipeline;
mod post;
pub mod sdf;
mod swapchain;
mod texture;
mod types;

pub use gpu_gi::{GpuGiEmitter, GpuGiMarch, GpuGiMaterial};
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
/// set-1 sampler binding indices in texture-slot order (binding 4 is the FX UBO).
const TEXTURE_BINDINGS: [u32; 12] = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12];

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

/// Max baked light-probe volumes sampled per frame (in the FrameUbo).
const MAX_PROBE_VOLUMES: usize = 4;

/// One probe volume's runtime metadata in std140 layout. Mirrors
/// `ProbeVolume` in standard.frag.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct GpuProbeVolume {
    /// World → volume-local; local grid spans -size/2..+size/2.
    world_to_local: [[f32; 4]; 4],
    /// xyz = local box size, w = first probe index (sh_base) as float.
    size_base: [f32; 4],
    /// xyz = probe counts per axis (as floats), w unused.
    counts: [f32; 4],
}

/// One probe's SH-L1 in the GPU storage buffer (std430): 4 coefficients, each
/// an RGB in xyz. The `w` of each carries the SH-L1 of the directional
/// distance-to-geometry (`ProbeSh::dist`) for visibility weighting.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct GpuProbe {
    coeffs: [[f32; 4]; 4],
}

/// Pack a `ProbeSh` into its GPU form: RGB SH in xyz, distance SH in the w lanes.
fn gpu_probe(p: &ProbeSh) -> GpuProbe {
    GpuProbe {
        coeffs: [
            [p.coeffs[0][0], p.coeffs[0][1], p.coeffs[0][2], p.dist[0]],
            [p.coeffs[1][0], p.coeffs[1][1], p.coeffs[1][2], p.dist[1]],
            [p.coeffs[2][0], p.coeffs[2][1], p.coeffs[2][2], p.dist[2]],
            [p.coeffs[3][0], p.coeffs[3][1], p.coeffs[3][2], p.dist[3]],
        ],
    }
}

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
    misc: [f32; 4], // x = time, y = light count, z = shadow spacing, w = probe-volume count
    // Post-processing (per-pixel, in the surface shaders). After `misc` so the
    // skybox's truncated FrameData prefix can read them too.
    postfx0: [f32; 4], // x tonemap mode, y exposure EV, z grade exposure, w contrast
    postfx1: [f32; 4], // x saturation, y temperature, z tint, w grading enabled
    postfx2: [f32; 4], // x vignette enabled, y intensity, z smoothness, w screen w
    postfx3: [f32; 4], // xyz vignette color, w screen h
    /// Far view-space distance of each directional cascade (xyzw = up to 4).
    cascade_splits: [f32; 4],
    lights: [GpuLight; MAX_LIGHTS],
    shadow_vp: [[[f32; 4]; 4]; MAX_SHADOW_VIEWS],
    /// Baked probe volumes (count in `misc.w`); SH coefficients live in the
    /// set-0 binding-2 storage buffer, indexed via each volume's `size_base.w`.
    probe_volumes: [GpuProbeVolume; MAX_PROBE_VOLUMES],
    /// x = lightmap-UV-checker preview flag (>0.5 on); yzw reserved.
    debug: [f32; 4],
    /// Inverse view / projection — for reconstructing world position from a
    /// depth sample (screen-space GI). Appended last so existing shaders that
    /// read a prefix of FrameData are unaffected.
    inv_view: [[f32; 4]; 4],
    inv_proj: [[f32; 4]; 4],
    /// Screen-space reflections: x = enabled (>0.5), y = intensity,
    /// z = max ray distance (view-space units), w = roughness cutoff (no SSR above).
    ssr: [f32; 4],
    /// Reflection-probe zone: xyz = box center (world), w = intensity (0 = no probe).
    refl_center: [f32; 4],
    /// Reflection-probe zone: xyz = box half-extents (world), w = box-projection on.
    refl_extents: [f32; 4],
    /// Fog: xyz = colour, w = density (0 = no fog).
    fog_color: [f32; 4],
    /// Fog: x = height falloff, y = height ref (world Y), z = start distance, w unused.
    fog_params: [f32; 4],
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
            // valid depth in its selected face's map. Without the overlap the
            // shader's PCF taps spill into the atlas border and show a bright
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
        // View count in spot.w; its sign carries the filter mode (positive =
        // soft/PCF, negative = hard/single-tap), so no extra GPU field is
        // needed. The shader reads abs() for the count.
        lights[li].spot[3] = if l.soft_shadows { needed as f32 } else { -(needed as f32) };
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
    vertex_count: u32,
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
    /// Custom-shader push data (15 props + reserved lightmap layer).
    custom_data: [f32; 16],
    set: vk::DescriptorSet,
    /// Extended "FX" uniform block (set 1, binding 4); host-visible, rewritten on
    /// param change. Holds rim + animated-emission params for the built-in shaders.
    fx_ubo: Buffer,
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

/// Number of mip levels for a square cubemap of `size` (for roughness prefilter:
/// mip 0 = mirror, higher mips = rougher).
#[allow(dead_code)] // reflection-probe capture (in progress) — foundation landed
fn cube_mip_count(size: u32) -> u32 {
    32 - size.max(1).leading_zeros()
}

/// Create a cube-compatible color image (6 layers) with a roughness mip chain.
/// Usage covers rendering the faces (COLOR_ATTACHMENT), prefiltering by blit
/// (TRANSFER_SRC|DST), and sampling at runtime (SAMPLED).
#[allow(dead_code)] // reflection-probe capture (in progress) — foundation landed
fn create_cube_image(
    device: &ash::Device,
    allocator: &mut Allocator,
    format: vk::Format,
    size: u32,
    name: &str,
) -> Result<(vk::Image, Allocation, u32)> {
    let mips = cube_mip_count(size);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: size,
            height: size,
            depth: 1,
        })
        .mip_levels(mips)
        .array_layers(6)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST,
        )
        .flags(vk::ImageCreateFlags::CUBE_COMPATIBLE)
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
    Ok((image, allocation, mips))
}

/// A `samplerCube` view over all 6 faces + all mips (runtime sampling).
#[allow(dead_code)] // reflection-probe capture (in progress) — foundation landed
fn create_cube_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
    mips: u32,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::CUBE)
        .format(format)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(mips)
                .layer_count(6),
        );
    Ok(unsafe { device.create_image_view(&info, None)? })
}

/// A single-face, single-mip 2D view used as a render target when capturing or
/// prefiltering one cube face.
#[allow(dead_code)] // reflection-probe capture (in progress) — foundation landed
fn create_cube_face_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
    face: u32,
    mip: u32,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(mip)
                .level_count(1)
                .base_array_layer(face)
                .layer_count(1),
        );
    Ok(unsafe { device.create_image_view(&info, None)? })
}

/// A prefiltered environment reflection cubemap (the runtime side of a
/// reflection probe). Built once from the scene's skybox: a mirror cube at mip 0
/// that box-blurs toward rougher mips, sampled by reflection direction +
/// roughness→mip in the forward shader. A 1×1 neutral cube until a skybox loads,
/// so the `samplerCube` binding is always valid.
struct ReflectionEnv {
    image: vk::Image,
    view: vk::ImageView,
    alloc: Option<Allocation>,
}

const ENV_CUBE_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

impl ReflectionEnv {
    /// Direction for cube `face` at face-plane coords (`fx`,`fy`) in [-1,1].
    fn face_dir(face: usize, fx: f32, fy: f32) -> [f32; 3] {
        let d = match face {
            0 => [1.0, -fy, -fx],
            1 => [-1.0, -fy, fx],
            2 => [fx, 1.0, fy],
            3 => [fx, -1.0, -fy],
            4 => [fx, -fy, 1.0],
            _ => [-fx, -fy, -1.0],
        };
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
        [d[0] / l, d[1] / l, d[2] / l]
    }

    /// Sample an equirectangular RGBA8 image in world direction `dir`.
    fn sample_equirect(eq: &TextureData, dir: [f32; 3]) -> [u8; 4] {
        let u = dir[2].atan2(dir[0]) * std::f32::consts::FRAC_1_PI * 0.5 + 0.5;
        let v = dir[1].clamp(-1.0, 1.0).acos() * std::f32::consts::FRAC_1_PI;
        let px = ((u * eq.width as f32) as i64).rem_euclid(eq.width as i64) as usize;
        let py = ((v * eq.height as f32) as usize).min(eq.height as usize - 1);
        let i = (py * eq.width as usize + px) * 4;
        eq.pixels
            .get(i..i + 4)
            .map(|s| [s[0], s[1], s[2], s[3]])
            .unwrap_or([0, 0, 0, 255])
    }

    /// Build the mip-0 face data (RGBA8) for `size` from the equirect skybox.
    fn build_face_mip0(eq: &TextureData, face: usize, size: u32) -> Vec<u8> {
        let mut out = vec![0u8; (size * size * 4) as usize];
        for y in 0..size {
            for x in 0..size {
                let fx = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let fy = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let c = Self::sample_equirect(eq, Self::face_dir(face, fx, fy));
                let o = ((y * size + x) * 4) as usize;
                out[o..o + 4].copy_from_slice(&c);
            }
        }
        out
    }

    /// 2×2 box downsample of one RGBA8 face.
    fn downsample(src: &[u8], src_size: u32) -> Vec<u8> {
        let dst_size = (src_size / 2).max(1);
        let mut out = vec![0u8; (dst_size * dst_size * 4) as usize];
        for y in 0..dst_size {
            for x in 0..dst_size {
                for c in 0..4 {
                    let mut sum = 0u32;
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let sx = (x * 2 + dx).min(src_size - 1);
                            let sy = (y * 2 + dy).min(src_size - 1);
                            sum += src[((sy * src_size + sx) * 4 + c) as usize] as u32;
                        }
                    }
                    out[((y * dst_size + x) * 4 + c) as usize] = (sum / 4) as u8;
                }
            }
        }
        out
    }

    /// Build the reflection cube from an equirectangular skybox (CPU prefilter:
    /// mirror at mip 0, progressively box-blurred mips ≈ roughness).
    fn from_equirect(
        device: &ash::Device,
        allocator: &mut Allocator,
        command_pool: vk::CommandPool,
        queue: vk::Queue,
        eq: &TextureData,
    ) -> Result<Self> {
        Self::build_cube(device, allocator, command_pool, queue, 64, |face, size| {
            Self::build_face_mip0(eq, face, size)
        })
    }

    /// Resample an RGBA8 source image to a `size×size` cube face (nearest).
    fn resample_face(src: &TextureData, size: u32) -> Vec<u8> {
        let mut out = vec![0u8; (size * size * 4) as usize];
        for y in 0..size {
            for x in 0..size {
                let sx = (x * src.width / size).min(src.width.saturating_sub(1));
                let sy = (y * src.height / size).min(src.height.saturating_sub(1));
                let si = ((sy * src.width + sx) * 4) as usize;
                let o = ((y * size + x) * 4) as usize;
                if let Some(s) = src.pixels.get(si..si + 4) {
                    out[o..o + 4].copy_from_slice(s);
                } else {
                    out[o + 3] = 255;
                }
            }
        }
        out
    }

    /// Build the cube from 6 explicit face images (+X,-X,+Y,-Y,+Z,-Z order) — a
    /// native cubemap / 6-texture skybox. Mip 0 is the faces; rougher mips blur.
    fn from_faces(
        device: &ash::Device,
        allocator: &mut Allocator,
        command_pool: vk::CommandPool,
        queue: vk::Queue,
        faces: &[&TextureData; 6],
    ) -> Result<Self> {
        Self::build_cube(device, allocator, command_pool, queue, 256, |face, size| {
            Self::resample_face(faces[face], size)
        })
    }

    /// Shared cube builder: `face_mip0(face, size)` produces the mip-0 RGBA8 for
    /// each face; higher mips are box-downsampled. Uploads + leaves SHADER_READ.
    fn build_cube(
        device: &ash::Device,
        allocator: &mut Allocator,
        command_pool: vk::CommandPool,
        queue: vk::Queue,
        size: u32,
        face_mip0: impl Fn(usize, u32) -> Vec<u8>,
    ) -> Result<Self> {
        let (image, alloc, mips) =
            create_cube_image(device, allocator, ENV_CUBE_FORMAT, size, "reflection env cube")?;

        // CPU-build all faces × mips, packed (face-major, then mip) into staging.
        let mut data: Vec<u8> = Vec::new();
        // regions[(face,mip)] = (byte offset, mip size)
        let mut regions: Vec<(u64, u32)> = Vec::new();
        for face in 0..6usize {
            let mut cur = face_mip0(face, size);
            let mut s = size;
            for _ in 0..mips {
                regions.push((data.len() as u64, s));
                data.extend_from_slice(&cur);
                if s > 1 {
                    cur = Self::downsample(&cur, s);
                    s /= 2;
                }
            }
        }

        let mut staging = Buffer::new(
            device,
            allocator,
            data.len() as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::CpuToGpu,
            "reflection env staging",
        )?;
        staging.write(0, &data);

        let full = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(mips)
            .layer_count(6);
        crate::alloc::one_time_submit(device, command_pool, queue, |cb| unsafe {
            let to_dst = vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .subresource_range(full);
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            let mut copies = Vec::new();
            let mut idx = 0;
            for face in 0..6u32 {
                let mut s = size;
                for mip in 0..mips {
                    let (offset, msize) = regions[idx];
                    idx += 1;
                    copies.push(
                        vk::BufferImageCopy::default()
                            .buffer_offset(offset)
                            .image_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .mip_level(mip)
                                    .base_array_layer(face)
                                    .layer_count(1),
                            )
                            .image_extent(vk::Extent3D {
                                width: msize,
                                height: msize,
                                depth: 1,
                            }),
                    );
                    let _ = s;
                    s = (s / 2).max(1);
                }
            }
            device.cmd_copy_buffer_to_image(
                cb,
                staging.handle,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copies,
            );
            let to_read = vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .subresource_range(full);
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );
        })?;
        staging.destroy(device, allocator);

        let view = create_cube_view(device, image, ENV_CUBE_FORMAT, mips)?;
        Ok(Self {
            image,
            view,
            alloc: Some(alloc),
        })
    }

    /// A 1×1 neutral (black) cube so the binding is valid before a skybox loads.
    /// The default reflection environment when no skybox texture is set: the
    /// procedural sky gradient (matching `skybox.frag`), so reflective surfaces
    /// always reflect the sky instead of black.
    fn neutral(
        device: &ash::Device,
        allocator: &mut Allocator,
        command_pool: vk::CommandPool,
        queue: vk::Queue,
    ) -> Result<Self> {
        Self::build_cube(device, allocator, command_pool, queue, 64, |face, size| {
            let mut out = vec![0u8; (size * size * 4) as usize];
            let mix = |a: f32, b: f32, t: f32| a + (b - a) * t.clamp(0.0, 1.0);
            // Matches the procedural sky in skybox.frag.
            let horizon = [0.52f32, 0.60, 0.72];
            let zenith = [0.10f32, 0.16, 0.34];
            let ground = [0.06f32, 0.06, 0.08];
            for y in 0..size {
                for x in 0..size {
                    let fx = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                    let fy = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                    let d = Self::face_dir(face, fx, fy);
                    let c = if d[1] >= 0.0 {
                        let t = d[1];
                        [mix(horizon[0], zenith[0], t), mix(horizon[1], zenith[1], t), mix(horizon[2], zenith[2], t)]
                    } else {
                        let t = -d[1] * 2.0;
                        [mix(horizon[0], ground[0], t), mix(horizon[1], ground[1], t), mix(horizon[2], ground[2], t)]
                    };
                    let o = ((y * size + x) * 4) as usize;
                    out[o] = (c[0] * 255.0) as u8;
                    out[o + 1] = (c[1] * 255.0) as u8;
                    out[o + 2] = (c[2] * 255.0) as u8;
                    out[o + 3] = 255;
                }
            }
            out
        })
    }

    fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
        }
        if let Some(a) = self.alloc.take() {
            let _ = allocator.free(a);
        }
    }
}

/// Screen-probe tile size: one screen-space GI probe is traced per
/// `SCREEN_PROBE_DIV × SCREEN_PROBE_DIV` block of pixels, then bilinearly
/// upsampled to full resolution in the forward shader (Lumen screen-probe
/// scheme). Larger is cheaper and softer; smaller is sharper and uses more rays.
const SCREEN_PROBE_DIV: u32 = 4;

/// Runtime-tunable Flux (screen-space GI) parameters, set from the Environment
/// tab via [`Renderer::set_flux_settings`]. These drive the per-frame trace that
/// was previously hardcoded.
#[derive(Clone, Copy)]
pub struct FluxSettings {
    /// Rays per screen probe per frame (temporal accumulation smooths the rest).
    pub samples: u32,
    /// Indirect bounces per ray.
    pub bounces: u32,
    /// Max trace distance in world units; 0 = auto from the GDF bounds.
    pub march_distance: f32,
    /// Per-sample firefly clamp on the bounce term (caps bright outliers).
    pub firefly_clamp: f32,
    /// Temporal smoothing 0..1: higher is smoother with more lag, lower is sharper and noisier.
    pub smoothing: f32,
    /// Indirect strength multiplier (applied before temporal accumulation).
    pub intensity: f32,
    /// Screen-space reflections: trace specular rays against the depth prepass +
    /// last frame's colour. Only active while Flux runs (it owns the depth prepass).
    pub ssr_enabled: bool,
    /// SSR reflection strength multiplier.
    pub ssr_intensity: f32,
    /// SSR max ray distance in view-space units.
    pub ssr_max_distance: f32,
    /// SSR roughness cutoff: surfaces rougher than this skip the march (the cheap
    /// ambient-specular env approximation carries them instead).
    pub ssr_roughness_cutoff: f32,
}

impl Default for FluxSettings {
    fn default() -> Self {
        // Mirrors the old hardcoded trace (Balanced preset).
        Self {
            samples: 10,
            bounces: 2,
            march_distance: 0.0,
            firefly_clamp: 4.0,
            smoothing: 0.5,
            intensity: 1.0,
            ssr_enabled: true,
            ssr_intensity: 1.0,
            ssr_max_distance: 40.0,
            ssr_roughness_cutoff: 0.6,
        }
    }
}

/// Screen-space GI targets: a full-res sampleable camera depth prepass + a
/// reduced-resolution screen-probe radiance buffer (one probe per
/// `SCREEN_PROBE_DIV²` pixels), bilinearly upsampled by the forward shader.
struct ScreenGiTargets {
    /// Probe-grid resolution (screen extent / SCREEN_PROBE_DIV).
    probe_extent: vk::Extent2D,
    depth_image: vk::Image,
    depth_view: vk::ImageView,
    depth_alloc: Option<Allocation>,
    /// Ping-pong screen-probe buffers: temporal reprojection reads the previous
    /// frame's image and writes the other (in-place would race since reprojection
    /// reads neighbour texels). `parity` selects which is current.
    gi_image: [vk::Image; 2],
    gi_view: [vk::ImageView; 2],
    gi_alloc: [Option<Allocation>; 2],
}

impl ScreenGiTargets {
    fn new(ctx: &GpuContext, allocator: &mut Allocator, extent: vk::Extent2D) -> Result<Self> {
        let (depth_image, depth_alloc) = create_image(
            &ctx.device,
            allocator,
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            extent,
            "sgi depth prepass",
        )?;
        let depth_view = create_view(
            &ctx.device,
            depth_image,
            DEPTH_FORMAT,
            vk::ImageAspectFlags::DEPTH,
        )?;
        // The probe buffer is the screen-probe grid: one texel per tile.
        let probe_extent = vk::Extent2D {
            width: extent.width.div_ceil(SCREEN_PROBE_DIV).max(1),
            height: extent.height.div_ceil(SCREEN_PROBE_DIV).max(1),
        };
        let mut gi_image = [vk::Image::null(); 2];
        let mut gi_view = [vk::ImageView::null(); 2];
        let mut gi_alloc: [Option<Allocation>; 2] = [None, None];
        for k in 0..2 {
            let (img, alloc) = create_image(
                &ctx.device,
                allocator,
                vk::Format::R16G16B16A16_SFLOAT,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
                probe_extent,
                "sgi screen probes",
            )?;
            gi_view[k] = create_view(
                &ctx.device,
                img,
                vk::Format::R16G16B16A16_SFLOAT,
                vk::ImageAspectFlags::COLOR,
            )?;
            gi_image[k] = img;
            gi_alloc[k] = Some(alloc);
        }
        Ok(Self {
            probe_extent,
            depth_image,
            depth_view,
            depth_alloc: Some(depth_alloc),
            gi_image,
            gi_view,
            gi_alloc,
        })
    }

    fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_image_view(self.depth_view, None);
            device.destroy_image(self.depth_image, None);
            for k in 0..2 {
                device.destroy_image_view(self.gi_view[k], None);
                device.destroy_image(self.gi_image[k], None);
            }
        }
        if let Some(a) = self.depth_alloc.take() {
            let _ = allocator.free(a);
        }
        for k in 0..2 {
            if let Some(a) = self.gi_alloc[k].take() {
                let _ = allocator.free(a);
            }
        }
    }
}

/// A persistent copy of the previous frame's lit colour, sampled by SSR. The
/// scene's final colour is copied into this each frame (after the scene pass)
/// so the next frame's reflection march has a colour source. `ready` is false
/// until the first copy lands, gating SSR off for one frame after (re)creation.
struct ColorHistory {
    image: vk::Image,
    view: vk::ImageView,
    alloc: Option<Allocation>,
    extent: vk::Extent2D,
    ready: bool,
}

impl ColorHistory {
    fn new(
        device: &ash::Device,
        allocator: &mut Allocator,
        command_pool: vk::CommandPool,
        queue: vk::Queue,
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> Result<Self> {
        let (image, alloc) = create_image(
            device,
            allocator,
            format,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            extent,
            "ssr color history",
        )?;
        let view = create_view(device, image, format, vk::ImageAspectFlags::COLOR)?;
        // Clear to black and leave it shader-readable, so the first frame samples a
        // valid (black) image rather than UNDEFINED content/layout.
        crate::alloc::one_time_submit(device, command_pool, queue, |cb| unsafe {
            image_barrier(
                device, cb, image, vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::TRANSFER, vk::AccessFlags2::TRANSFER_WRITE,
            );
            device.cmd_clear_color_image(
                cb,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] },
                &[vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1)],
            );
            image_barrier(
                device, cb, image, vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::TRANSFER, vk::AccessFlags2::TRANSFER_WRITE,
                vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::SHADER_READ,
            );
        })?;
        Ok(Self {
            image,
            view,
            alloc: Some(alloc),
            extent,
            ready: true,
        })
    }

    /// Copy `src` (a scene colour image, same format + extent) into the history.
    /// `src_before`/`src_after` are the source's layout around the copy so the
    /// caller's pass state is preserved. Leaves the history in SHADER_READ_ONLY.
    fn record_copy(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        src: vk::Image,
        src_before: vk::ImageLayout,
        src_after: vk::ImageLayout,
    ) {
        let dst_old = if self.ready {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        } else {
            vk::ImageLayout::UNDEFINED
        };
        image_barrier(
            device, cb, self.image, vk::ImageAspectFlags::COLOR,
            dst_old, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::SHADER_READ,
            vk::PipelineStageFlags2::TRANSFER, vk::AccessFlags2::TRANSFER_WRITE,
        );
        image_barrier(
            device, cb, src, vk::ImageAspectFlags::COLOR,
            src_before, vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::TRANSFER, vk::AccessFlags2::TRANSFER_READ,
        );
        let region = vk::ImageCopy::default()
            .src_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .dst_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .extent(vk::Extent3D {
                width: self.extent.width,
                height: self.extent.height,
                depth: 1,
            });
        unsafe {
            device.cmd_copy_image(
                cb,
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
        }
        image_barrier(
            device, cb, self.image, vk::ImageAspectFlags::COLOR,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::TRANSFER, vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::SHADER_READ,
        );
        image_barrier(
            device, cb, src, vk::ImageAspectFlags::COLOR,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL, src_after,
            vk::PipelineStageFlags2::TRANSFER, vk::AccessFlags2::TRANSFER_READ,
            vk::PipelineStageFlags2::BOTTOM_OF_PIPE, vk::AccessFlags2::empty(),
        );
        self.ready = true;
    }

    fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
        }
        if let Some(a) = self.alloc.take() {
            let _ = allocator.free(a);
        }
    }
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
    /// Per-camera Flux targets (sampleable depth prepass + ping-pong gather), so
    /// the preview gets its own screen-space GI traced from the game camera.
    sgi: ScreenGiTargets,
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
        probe_buffer: vk::Buffer,
        lightmap_view: vk::ImageView,
        lightmap_sampler: vk::Sampler,
        env_cube_view: vk::ImageView,
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
                            // shadow(1)+lightmap(3)+GI(4)+SSR depth(5)+color(6)+env cube(7)
                            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                            descriptor_count: 6 * FRAMES_IN_FLIGHT as u32,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_BUFFER,
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
            let probe_info = [vk::DescriptorBufferInfo::default()
                .buffer(probe_buffer)
                .range(vk::WHOLE_SIZE)];
            let lightmap_info = [vk::DescriptorImageInfo::default()
                .sampler(lightmap_sampler)
                .image_view(lightmap_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let env_info = [vk::DescriptorImageInfo::default()
                .sampler(sampler)
                .image_view(env_cube_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
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
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&probe_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(3)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&lightmap_info),
                // Binding 4 (screen-GI) — the preview doesn't run the gather, so
                // just point it at a valid image; the shader gates its use on the
                // screen-GI-active flag (off for the preview).
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(4)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&lightmap_info),
                // Bindings 5/6 (SSR depth + colour) — preview never runs SSR
                // (frame.ssr.x stays 0), so point them at a valid image for set
                // completeness only.
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(5)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&lightmap_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(6)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&lightmap_info),
                // Binding 7: the environment reflection cube (shared with the
                // main renderer; re-pointed by set_skybox on rebuild).
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(7)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&env_info),
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

        let sgi = ScreenGiTargets::new(ctx, allocator, extent)?;

        Ok(Self {
            extent,
            color,
            color_view,
            color_alloc: Some(color_alloc),
            depth,
            depth_view,
            depth_alloc: Some(depth_alloc),
            sgi,
            ubos,
            sets,
            ubo_pool,
            egui_layout,
            egui_pool,
            texture_id,
        })
    }

    fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        self.sgi.destroy(device, allocator);
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

/// Offscreen target for the EDITOR viewport. The 3D scene renders here sized to
/// the viewport dock rect (so it isn't rasterized at full-window resolution under
/// the panels) and egui shows it as an image. Editor-only: the game/runtime has
/// no `ViewportTarget` and renders straight to the swapchain unchanged. Reuses
/// the main camera descriptor set, so it only holds the color/depth surfaces +
/// the egui texture.
#[allow(dead_code)] // wired into render() in stage 2 of the viewport-RTT refactor
struct ViewportTarget {
    extent: vk::Extent2D,
    color: vk::Image,
    color_view: vk::ImageView,
    color_alloc: Option<Allocation>,
    depth: vk::Image,
    depth_view: vk::ImageView,
    depth_alloc: Option<Allocation>,
    /// Rect-sized Flux targets for the editor main-camera trace (the game keeps
    /// the renderer's own `self.sgi`).
    sgi: ScreenGiTargets,
    /// Previous-frame colour for SSR in the editor viewport.
    ssr_color: ColorHistory,
    /// HDR scene target + post descriptor set: the viewport scene renders linear
    /// HDR here, then the post pass (tonemap + bloom) writes `color`.
    hdr_image: vk::Image,
    hdr_view: vk::ImageView,
    hdr_alloc: Option<Allocation>,
    post_pool: vk::DescriptorPool,
    post_set: vk::DescriptorSet,
    egui_layout: vk::DescriptorSetLayout,
    egui_pool: vk::DescriptorPool,
    texture_id: egui::TextureId,
}

#[allow(dead_code)] // wired in stage 2 of the viewport-RTT refactor
impl ViewportTarget {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: &GpuContext,
        allocator: &mut Allocator,
        command_pool: vk::CommandPool,
        queue: vk::Queue,
        color_format: vk::Format,
        sampler: vk::Sampler,
        post_layout: vk::DescriptorSetLayout,
        post_sampler: vk::Sampler,
        extent: vk::Extent2D,
        egui: &mut egui_ash_renderer::Renderer,
    ) -> Result<Self> {
        let device = &ctx.device;
        let (color, color_alloc) = create_image(
            device,
            allocator,
            color_format,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            extent,
            "viewport color",
        )?;
        // HDR scene target + a post descriptor set pointing at it.
        let (hdr_image, hdr_alloc) = create_image(
            device,
            allocator,
            post::HDR_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            extent,
            "viewport hdr",
        )?;
        let hdr_view = create_view(device, hdr_image, post::HDR_FORMAT, vk::ImageAspectFlags::COLOR)?;
        let post_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&[
                    vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        descriptor_count: 1,
                    },
                ]),
                None,
            )?
        };
        let post_set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(post_pool)
                    .set_layouts(&[post_layout]),
            )?[0]
        };
        let post_info = [vk::DescriptorImageInfo::default()
            .sampler(post_sampler)
            .image_view(hdr_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        unsafe {
            device.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(post_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&post_info)],
                &[],
            )
        };
        let color_view = create_view(device, color, color_format, vk::ImageAspectFlags::COLOR)?;
        let (depth, depth_alloc) = create_image(
            device,
            allocator,
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
            extent,
            "viewport depth",
        )?;
        let depth_view = create_view(device, depth, DEPTH_FORMAT, vk::ImageAspectFlags::DEPTH)?;
        let egui_layout = egui_ash_renderer::vulkan::create_vulkan_descriptor_set_layout(device)
            .map_err(|e| anyhow::anyhow!("viewport descriptor layout: {e}"))?;
        let egui_pool = egui_ash_renderer::vulkan::create_vulkan_descriptor_pool(device, 1)
            .map_err(|e| anyhow::anyhow!("viewport descriptor pool: {e}"))?;
        let egui_set = egui_ash_renderer::vulkan::create_vulkan_descriptor_set(
            device, egui_layout, egui_pool, color_view, sampler,
        )
        .map_err(|e| anyhow::anyhow!("viewport descriptor set: {e}"))?;
        let texture_id = egui.add_user_texture(egui_set);
        let sgi = ScreenGiTargets::new(ctx, allocator, extent)?;
        // SSR samples the viewport's previous HDR scene colour.
        let ssr_color = ColorHistory::new(device, allocator, command_pool, queue, post::HDR_FORMAT, extent)?;
        Ok(Self {
            extent,
            color,
            color_view,
            color_alloc: Some(color_alloc),
            depth,
            depth_view,
            depth_alloc: Some(depth_alloc),
            sgi,
            ssr_color,
            hdr_image,
            hdr_view,
            hdr_alloc: Some(hdr_alloc),
            post_pool,
            post_set,
            egui_layout,
            egui_pool,
            texture_id,
        })
    }

    fn destroy(&mut self, device: &ash::Device, allocator: &mut Allocator) {
        self.sgi.destroy(device, allocator);
        self.ssr_color.destroy(device, allocator);
        unsafe {
            device.destroy_descriptor_pool(self.post_pool, None);
            device.destroy_image_view(self.hdr_view, None);
            device.destroy_image(self.hdr_image, None);
            device.destroy_descriptor_pool(self.egui_pool, None);
            device.destroy_descriptor_set_layout(self.egui_layout, None);
            device.destroy_image_view(self.color_view, None);
            device.destroy_image(self.color, None);
            device.destroy_image_view(self.depth_view, None);
            device.destroy_image(self.depth, None);
        }
        if let Some(a) = self.hdr_alloc.take() {
            let _ = allocator.free(a);
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
    ctx: GpuContext,
    allocator: Option<Arc<Mutex<Allocator>>>,
    swapchain: Swapchain,
    depth: DepthTarget,
    /// Screen-space GI: depth prepass + gather output (recreated on resize).
    sgi: ScreenGiTargets,
    /// Previous-frame lit colour for SSR (swapchain path; recreated on resize).
    ssr_color: ColorHistory,
    /// HDR scene target + fullscreen post pass (tonemap + bloom) for the game
    /// swapchain path. The editor viewport keeps inline tonemap for now.
    post_pass: post::PostPass,
    /// Prefiltered environment reflection cubemap (reflection probe), rebuilt
    /// from the skybox. Sampled in the forward shader's specular term.
    reflection_env: ReflectionEnv,
    /// Dedicated set-0 + UBO for the precomputed scene reflection capture (6-face
    /// render from a probe position into the env cube). Set once, reused per face.
    refl_capture_pool: vk::DescriptorPool,
    refl_capture_set: vk::DescriptorSet,
    refl_capture_ubo: Buffer,
    /// World-space center for the next scene reflection capture; `Some` requests a
    /// (re)capture on the next frame (the scene reflects its surroundings, not just
    /// the sky). Consumed once, then cleared.
    recapture_reflection: Option<glam::Vec3>,
    /// Flip Y in the screen-GI depth→world reconstruction (runtime toggle so the
    /// Y-flipped-viewport convention can be corrected without a rebuild).
    screen_gi_flip_y: bool,
    /// Whether the screen-GI image holds a valid previous-frame result (for
    /// temporal accumulation). False on the first frame + after a resize.
    sgi_history_valid: bool,
    /// Emissive sphere area-lights for the screen-GI gather (analytic NEE, so
    /// the emitter bounce stays smooth without the coarse-GDF footprint). Set by
    /// the realtime-GI driver alongside the GDF.
    gi_emitters: Vec<gpu_gi::GpuGiEmitter>,
    /// Runtime Flux trace parameters (Environment tab → Flux GI).
    flux_settings: FluxSettings,
    /// Last camera view-proj the screen-GI was traced for; while unchanged the
    /// gather is skipped and the converged result reused (a still camera costs
    /// nothing).
    sgi_last_view_proj: Option<glam::Mat4>,
    /// Consecutive still frames — keep tracing (temporal accumulation) until the
    /// result has converged, then idle. Reset when the camera moves.
    sgi_still_frames: u32,
    /// Ping-pong parity: which gather image is current (written this trace); the
    /// other holds the previous frame's result for temporal reprojection.
    sgi_parity: usize,
    /// Monotonic trace counter feeding the trace RNG seed. Without per-frame
    /// variation every frame samples the identical random directions, so the
    /// Monte Carlo noise is fixed-pattern and temporal accumulation can never
    /// average it out; that was the source of the static blotches.
    sgi_frame: u32,
    /// Per-frame-in-flight Flux trace resources (descriptor pool + host buffers)
    /// kept alive after the trace is recorded into the main cb, freed one frame
    /// later once that frame's fence has signalled.
    sgi_transients: Vec<Option<gpu_gi::ScreenGiTransient>>,
    /// Temporal state + transient ring for the in-game camera preview's own Flux
    /// trace (mirrors the editor-viewport state above, but for the game camera).
    preview_sgi_parity: usize,
    preview_sgi_last_view_proj: Option<glam::Mat4>,
    preview_sgi_last_cam: glam::Vec3,
    preview_sgi_history_valid: bool,
    preview_sgi_frame: u32,
    preview_sgi_transients: Vec<Option<gpu_gi::ScreenGiTransient>>,
    /// Camera position the previous gather was traced from (reprojection).
    sgi_last_cam: glam::Vec3,
    command_pool: vk::CommandPool,
    frames: Vec<Frame>,
    frame_index: usize,
    needs_resize: bool,

    pipeline_cache: PipelineCache,
    descriptor_pool: vk::DescriptorPool,
    material_pool: vk::DescriptorPool,
    /// Shared zero FX block bound to non-material set-1 users (e.g. the skybox
    /// set) so set-1 binding 4 is always valid.
    default_fx_ubo: Buffer,
    frame_ubos: Vec<Buffer>,
    frame_sets: Vec<vk::DescriptorSet>,
    sampler: vk::Sampler,
    /// Linear CLAMP_TO_EDGE sampler for baked lightmaps. The material `sampler`
    /// is REPEAT (tiling), which wraps at the atlas border and bleeds chart
    /// edges into the opposite side.
    lightmap_sampler: vk::Sampler,
    default_textures: Vec<GpuTexture>, // [albedo, normal, orm, emission]
    /// Fullscreen skybox: descriptor set (set 1) + optional equirect texture.
    /// When `skybox_has_texture` is false the shader draws a procedural sky.
    skybox_set: vk::DescriptorSet,
    skybox_texture: Option<GpuTexture>,
    skybox_has_texture: bool,
    /// True when the skybox is a cubemap (rendered from the env cube, binding 7)
    /// rather than the equirect texture.
    skybox_is_cube: bool,
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
    /// Editor-only offscreen target for the viewport 3D (see `ViewportTarget`).
    /// `None` for the game runtime (renders straight to the swapchain).
    #[allow(dead_code)] // read by the RTT render branch (stage 2, in progress)
    viewport_target: Option<ViewportTarget>,
    /// Baked light-probe SH (set-0 binding 2). A 1-probe zero buffer until a
    /// bake is loaded, so the binding is always valid.
    probe_buffer: Buffer,
    /// Number of probes the `probe_buffer` is sized for; lets `update_probe_sh`
    /// rewrite it in place (cheap, no realloc/stall) when the count is unchanged.
    probe_count: usize,
    /// Probe-volume metadata mirrored into each frame's FrameUbo (count drives
    /// `misc.w`). Empty = fall back to flat ambient.
    probe_volumes: Vec<GpuProbeVolume>,
    /// Baked lightmap array (set-0 binding 3), one layer per static object. A
    /// 1x1 black single layer until a bake is loaded.
    lightmaps: GpuTexture,
    last_stats: RenderStats,
    vsync: bool,
    /// GPU software-GI march pipeline (None if compute init failed → CPU march).
    gpu_gi: Option<gpu_gi::GpuGi>,

    /// Declared last so it drops last: `GpuContext::drop` destroys the Vulkan
    /// surface, which must happen while the underlying winit window still
    /// exists. EngineApp drops its own `Arc<Window>` clone before the renderer,
    /// so this field can be the final strong reference. Keeping it last means
    /// the window outlives `ctx`/`surface` teardown; otherwise NVIDIA/Wayland
    /// segfaults on exit.
    window: Arc<Window>,
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
        let sgi =
            ScreenGiTargets::new(&ctx, &mut allocator.lock().unwrap(), swapchain.extent)?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(ctx.queue_family);
        let command_pool = unsafe { ctx.device.create_command_pool(&pool_info, None)? };
        // SSR samples the previous frame's HDR scene colour (the game path now
        // renders HDR + tonemaps in the post pass), so the history is HDR too.
        let ssr_color = ColorHistory::new(
            &ctx.device,
            &mut allocator.lock().unwrap(),
            command_pool,
            ctx.queue,
            post::HDR_FORMAT,
            swapchain.extent,
        )?;
        let post_pass = post::PostPass::new(
            &ctx.device,
            &mut allocator.lock().unwrap(),
            swapchain.format.format,
            swapchain.extent,
            FRAMES_IN_FLIGHT,
        )?;
        let reflection_env = ReflectionEnv::neutral(
            &ctx.device,
            &mut allocator.lock().unwrap(),
            command_pool,
            ctx.queue,
        )?;

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
                // shadow(1)+lightmap(3)+screen-GI(4)+SSR depth(5)+SSR color(6)+env cube(7)
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 6 * FRAMES_IN_FLIGHT as u32,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
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
        // +1 set for the skybox, which also draws from this pool (it shares the
        // set-1 layout: 4 samplers + the FX uniform buffer).
        let material_sets = MAX_MATERIALS + 1;
        let material_pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: material_sets * 12,
            },
            // One FX uniform buffer per material set (+ the skybox set).
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: material_sets,
            },
        ];
        let material_pool = unsafe {
            ctx.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(material_sets)
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

        // Lightmaps are a packed UV atlas — clamp (not tile) so bilinear filtering
        // at the atlas border doesn't wrap to the opposite edge (a seam line).
        let lightmap_sampler = unsafe {
            ctx.device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )?
        };

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

        // Baked-probe SH buffer (set 0, binding 2): a single zero probe until a
        // bake is loaded, so the binding is always valid. Wire it into every
        // frame set.
        let probe_buffer = {
            let mut alloc = allocator.lock().unwrap();
            let mut buf = Buffer::new(
                &ctx.device,
                &mut alloc,
                size_of::<GpuProbe>() as u64,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                MemoryLocation::CpuToGpu,
                "probe sh (default)",
            )?;
            buf.write(0, bytemuck::bytes_of(&GpuProbe::default()));
            buf
        };
        for &set in &frame_sets {
            let info = [vk::DescriptorBufferInfo::default()
                .buffer(probe_buffer.handle)
                .range(vk::WHOLE_SIZE)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&info);
            unsafe { ctx.device.update_descriptor_sets(&[write], &[]) };
        }

        // Baked-lightmap array (set 0, binding 3): a 1x1 black single layer
        // until a bake loads, wired into every frame set.
        let lightmaps = GpuTexture::upload_lightmap_array(
            &ctx.device,
            &mut allocator.lock().unwrap(),
            command_pool,
            ctx.queue,
            &[vec![0.0; 4]],
            1,
        )?;
        for &set in &frame_sets {
            let info = [vk::DescriptorImageInfo::default()
                .sampler(lightmap_sampler)
                .image_view(lightmaps.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { ctx.device.update_descriptor_sets(&[write], &[]) };
        }

        // Screen-space GI gather result (set 0, binding 4). Wired into every
        // frame set; re-pointed after a resize (the target is recreated).
        for &set in &frame_sets {
            let info = [vk::DescriptorImageInfo::default()
                .sampler(lightmap_sampler)
                .image_view(sgi.gi_view[0])
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { ctx.device.update_descriptor_sets(&[write], &[]) };
        }

        // SSR inputs (set 0, bindings 5 = scene depth, 6 = last-frame colour).
        // Pointed at valid images so the sets are complete; re-pointed per-pass in
        // render()/record_viewport_scene to the active camera's depth + colour
        // history. The shader gates sampling on frame.ssr.x, so stale content here
        // is never read until a pass wires the real targets.
        for &set in &frame_sets {
            let depth_info = [vk::DescriptorImageInfo::default()
                .sampler(lightmap_sampler)
                .image_view(sgi.depth_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let color_info = [vk::DescriptorImageInfo::default()
                .sampler(lightmap_sampler)
                .image_view(lightmaps.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let env_info = [vk::DescriptorImageInfo::default()
                .sampler(sampler)
                .image_view(reflection_env.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(5)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&depth_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(6)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&color_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(7)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&env_info),
            ];
            unsafe { ctx.device.update_descriptor_sets(&writes, &[]) };
        }

        // Dedicated set-0 + UBO for the scene reflection capture, bound with the
        // same shadow/probe/lightmap/SGI/env resources as the frame sets (the
        // capture reuses the live lighting). Its UBO is rewritten per cube face.
        let (refl_capture_pool, refl_capture_set, refl_capture_ubo) = {
            let pool = unsafe {
                ctx.device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::UNIFORM_BUFFER,
                            descriptor_count: 1,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                            descriptor_count: 5,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_BUFFER,
                            descriptor_count: 1,
                        },
                    ]),
                    None,
                )?
            };
            let layouts = [pipeline_cache.set0_layout];
            let set = unsafe {
                ctx.device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool)
                        .set_layouts(&layouts),
                )?[0]
            };
            let ubo = Buffer::new(
                &ctx.device,
                &mut allocator.lock().unwrap(),
                size_of::<FrameUbo>() as u64,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                MemoryLocation::CpuToGpu,
                "refl capture ubo",
            )?;
            let buffer_info = [vk::DescriptorBufferInfo::default()
                .buffer(ubo.handle)
                .range(size_of::<FrameUbo>() as u64)];
            let shadow_info = [vk::DescriptorImageInfo::default()
                .sampler(shadow_atlas.sampler)
                .image_view(shadow_atlas.array_view)
                .image_layout(vk::ImageLayout::DEPTH_READ_ONLY_OPTIMAL)];
            let probe_info = [vk::DescriptorBufferInfo::default()
                .buffer(probe_buffer.handle)
                .range(vk::WHOLE_SIZE)];
            let lm_info = [vk::DescriptorImageInfo::default()
                .sampler(lightmap_sampler)
                .image_view(lightmaps.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let depth_info = [vk::DescriptorImageInfo::default()
                .sampler(lightmap_sampler)
                .image_view(sgi.depth_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let env_info = [vk::DescriptorImageInfo::default()
                .sampler(sampler)
                .image_view(reflection_env.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let mk = |binding: u32, ty: vk::DescriptorType| {
                vk::WriteDescriptorSet::default().dst_set(set).dst_binding(binding).descriptor_type(ty)
            };
            let writes = [
                mk(0, vk::DescriptorType::UNIFORM_BUFFER).buffer_info(&buffer_info),
                mk(1, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&shadow_info),
                mk(2, vk::DescriptorType::STORAGE_BUFFER).buffer_info(&probe_info),
                mk(3, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&lm_info),
                mk(4, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&lm_info),
                mk(5, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&depth_info),
                mk(6, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&lm_info),
                mk(7, vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&env_info),
            ];
            unsafe { ctx.device.update_descriptor_sets(&writes, &[]) };
            (pool, set, ubo)
        };

        // 1x1 defaults, indexed by texture slot (see create_material's slot list):
        // albedo white, flat normal, white ORM, white emission, white opacity,
        // white emission-mask, then 3×(black matcap, white matcap-mask). Black
        // matcaps contribute nothing; white masks pass through.
        let defaults = [
            ([255u8, 255, 255, 255], true),  // 0 albedo
            ([128, 128, 255, 255], false),   // 1 normal
            ([255, 255, 255, 255], false),   // 2 orm
            ([255, 255, 255, 255], true),    // 3 emission
            ([255, 255, 255, 255], false),   // 4 opacity
            ([255, 255, 255, 255], false),   // 5 emission mask
            ([0, 0, 0, 255], true),          // 6 matcap 1
            ([255, 255, 255, 255], false),   // 7 matcap 1 mask
            ([0, 0, 0, 255], true),          // 8 matcap 2
            ([255, 255, 255, 255], false),   // 9 matcap 2 mask
            ([0, 0, 0, 255], true),          // 10 matcap 3
            ([255, 255, 255, 255], false),   // 11 matcap 3 mask
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

        // Shared zero FX block for set-1 users that aren't materials (skybox).
        let default_fx_ubo = {
            let mut alloc = allocator.lock().unwrap();
            let fx = MaterialFx::from_params(&MaterialParams::default());
            make_fx_ubo(&ctx.device, &mut alloc, fx, "default fx ubo")?
        };

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
            // Fill all 12 sampler slots with defaults (the skybox shader only
            // reads slot 0, but every binding must be valid). set_skybox writes
            // the real equirect into binding 0 later.
            let infos: Vec<[vk::DescriptorImageInfo; 1]> = (0..12)
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
                        .dst_binding(TEXTURE_BINDINGS[i])
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(info)
                })
                .collect();
            ctx.device.update_descriptor_sets(&writes, &[]);
            // Binding 4: the shared zero FX block (skybox ignores it).
            let fx_info = [vk::DescriptorBufferInfo::default()
                .buffer(default_fx_ubo.handle)
                .range(vk::WHOLE_SIZE)];
            let fx_write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(&fx_info);
            ctx.device.update_descriptor_sets(&[fx_write], &[]);
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

        // GPU software-GI march pipeline; None falls back to the CPU march.
        let gpu_gi = gpu_gi::GpuGi::new(&ctx)
            .map_err(|e| tracing::warn!("GPU GI init failed, using CPU march: {e:#}"))
            .ok();

        Ok(Self {
            window,
            ctx,
            allocator: Some(allocator),
            swapchain,
            depth,
            sgi,
            ssr_color,
            post_pass,
            reflection_env,
            refl_capture_pool,
            refl_capture_set,
            refl_capture_ubo,
            recapture_reflection: None,
            screen_gi_flip_y: true,
            sgi_history_valid: false,
            gi_emitters: Vec::new(),
            flux_settings: FluxSettings::default(),
            sgi_last_view_proj: None,
            sgi_still_frames: 0,
            sgi_parity: 0,
            sgi_frame: 0,
            sgi_transients: (0..FRAMES_IN_FLIGHT).map(|_| None).collect(),
            preview_sgi_parity: 0,
            preview_sgi_last_view_proj: None,
            preview_sgi_last_cam: glam::Vec3::ZERO,
            preview_sgi_history_valid: false,
            preview_sgi_frame: 0,
            preview_sgi_transients: (0..FRAMES_IN_FLIGHT).map(|_| None).collect(),
            sgi_last_cam: glam::Vec3::ZERO,
            command_pool,
            frames,
            frame_index: 0,
            needs_resize: false,
            pipeline_cache,
            descriptor_pool,
            material_pool,
            default_fx_ubo,
            frame_ubos,
            frame_sets,
            sampler,
            lightmap_sampler,
            default_textures,
            skybox_set,
            skybox_texture: None,
            skybox_has_texture: false,
            skybox_is_cube: false,
            shadow_atlas,
            meshes: Vec::new(),
            textures: Vec::new(),
            materials: Vec::new(),
            egui: Some(egui),
            egui_free_queue: VecDeque::new(),
            camera_preview: None,
            viewport_target: None,
            probe_buffer,
            probe_count: 1, // default buffer holds one probe
            probe_volumes: Vec::new(),
            lightmaps,
            last_stats: RenderStats::default(),
            vsync: true,
            gpu_gi,
        })
    }

    /// Upload baked light-probe SH (set-0 binding 2) and the per-volume
    /// metadata sampled in the standard shader. Empty `volumes` reverts to the
    /// 1-probe zero buffer so fragments fall back to flat ambient. Call after a
    /// bake or when loading a scene's `.lightdata`.
    pub fn set_baked_probes(
        &mut self,
        probes: &[ProbeSh],
        volumes: &[(Mat4, [f32; 3], [u32; 3], u32)],
    ) {
        // Pack probes into std430 GpuProbe (4 vec4 each); at least one entry so
        // the buffer is never zero-sized.
        let gpu: Vec<GpuProbe> = if probes.is_empty() {
            vec![GpuProbe::default()]
        } else {
            probes.iter().map(gpu_probe).collect()
        };
        self.probe_volumes = volumes
            .iter()
            .map(|(w2l, size, counts, base)| GpuProbeVolume {
                world_to_local: w2l.to_cols_array_2d(),
                size_base: [size[0], size[1], size[2], *base as f32],
                counts: [counts[0] as f32, counts[1] as f32, counts[2] as f32, 0.0],
            })
            .collect();

        unsafe {
            let _ = self.ctx.device.device_wait_idle();
        }
        let alloc = self.allocator();
        let bytes = std::mem::size_of_val(gpu.as_slice()) as u64;
        let mut buf = match Buffer::new(
            &self.ctx.device,
            &mut alloc.lock().unwrap(),
            bytes,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            MemoryLocation::CpuToGpu,
            "probe sh",
        ) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("probe buffer alloc: {e:#}");
                return;
            }
        };
        buf.write(0, bytemuck::cast_slice(&gpu));
        // Re-point binding 2 on every frame set + the camera preview's sets.
        let mut sets: Vec<vk::DescriptorSet> = self.frame_sets.clone();
        if let Some(preview) = &self.camera_preview {
            sets.extend_from_slice(&preview.sets);
        }
        for set in sets {
            let info = [vk::DescriptorBufferInfo::default()
                .buffer(buf.handle)
                .range(vk::WHOLE_SIZE)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&info);
            unsafe { self.ctx.device.update_descriptor_sets(&[write], &[]) };
        }
        let mut old = std::mem::replace(&mut self.probe_buffer, buf);
        old.destroy(&self.ctx.device, &mut alloc.lock().unwrap());
        self.probe_count = gpu.len();
    }

    /// Cheap per-frame rewrite of the probe SH SSBO in place: no realloc, no
    /// `device_wait_idle`, no descriptor rewrite. Used to smoothly ease the
    /// realtime-GI probes toward each new trace every frame. Returns false when
    /// the probe count differs from the current buffer (caller must then use
    /// `set_baked_probes` to resize). The write is unsynchronized w.r.t. the GPU,
    /// which is fine for low-frequency, temporally-smoothed GI (a rare torn read
    /// is invisible) and avoids a full pipeline stall.
    pub fn update_probe_sh(&mut self, probes: &[ProbeSh]) -> bool {
        if probes.is_empty() || probes.len() != self.probe_count {
            return false;
        }
        let gpu: Vec<GpuProbe> = probes.iter().map(gpu_probe).collect();
        self.probe_buffer.write(0, bytemuck::cast_slice(&gpu));
        true
    }

    /// Upload baked lightmaps (set-0 binding 3) as a 2D array, one layer per
    /// object (resampled to a common size). Empty reverts to a 1x1 black layer.
    /// Object→layer indices come from `BakedData.object_lightmap`; the draw path
    /// puts the layer in the push constant.
    pub fn set_baked_lightmaps(&mut self, lightmaps: &[BakedLightmap]) {
        // All layers share the largest baked size (texture-array requirement),
        // so a big surface (e.g. the floor) keeps full resolution. Capped at
        // 4096 (matches the max_lightmap dropdown). Every object's layer is
        // padded to this size, so a uniform 4096² array costs 256 MB per
        // object; the follow-up per-object atlas removes that padding.
        let size = lightmaps
            .iter()
            .map(|l| l.size)
            .max()
            .unwrap_or(1)
            .clamp(1, 4096);
        let layers: Vec<Vec<f32>> = if lightmaps.is_empty() {
            vec![vec![0.0; 4]]
        } else {
            lightmaps
                .iter()
                .map(|l| resample_rgba32f(&l.pixels, l.size, size))
                .collect()
        };

        unsafe {
            let _ = self.ctx.device.device_wait_idle();
        }
        let alloc = self.allocator();
        let tex = match GpuTexture::upload_lightmap_array(
            &self.ctx.device,
            &mut alloc.lock().unwrap(),
            self.command_pool,
            self.ctx.queue,
            &layers,
            size,
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("lightmap array upload: {e:#}");
                return;
            }
        };
        let mut sets: Vec<vk::DescriptorSet> = self.frame_sets.clone();
        if let Some(preview) = &self.camera_preview {
            sets.extend_from_slice(&preview.sets);
        }
        for set in sets {
            let info = [vk::DescriptorImageInfo::default()
                .sampler(self.lightmap_sampler)
                .image_view(tex.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { self.ctx.device.update_descriptor_sets(&[write], &[]) };
        }
        let mut old = std::mem::replace(&mut self.lightmaps, tex);
        old.destroy(&self.ctx.device, &mut alloc.lock().unwrap());
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
        // When ray tracing is available the bake reads these buffers as
        // acceleration-structure inputs and as SSBOs (via device address),
        // so they need the extra usage flags + device-address storage.
        let rt = if self.ctx.ray_tracing() {
            vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                | vk::BufferUsageFlags::ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_KHR
                | vk::BufferUsageFlags::STORAGE_BUFFER
        } else {
            vk::BufferUsageFlags::empty()
        };
        let allocator = self.allocator();
        let mut alloc = allocator.lock().unwrap();
        let vertex_buffer = alloc::upload_buffer(
            &self.ctx.device,
            &mut alloc,
            self.command_pool,
            self.ctx.queue,
            bytemuck::cast_slice(&data.vertices),
            vk::BufferUsageFlags::VERTEX_BUFFER | rt,
            "vertices",
        )?;
        let index_buffer = alloc::upload_buffer(
            &self.ctx.device,
            &mut alloc,
            self.command_pool,
            self.ctx.queue,
            bytemuck::cast_slice(&data.indices),
            vk::BufferUsageFlags::INDEX_BUFFER | rt,
            "indices",
        )?;
        self.meshes.push(GpuMesh {
            vertex_buffer,
            index_buffer,
            index_count: data.indices.len() as u32,
            vertex_count: data.vertices.len() as u32,
        });
        Ok(MeshHandle(self.meshes.len() - 1))
    }

    /// Upload a mesh whose vertices are updated every frame (CPU skinning). The
    /// vertex buffer is host-visible so [`update_mesh_vertices`] can rewrite it
    /// cheaply; the index buffer is static/device-local. (No double-buffering
    /// yet, so a fast-moving skinned mesh can tear by a frame — acceptable until
    /// GPU skinning lands.)
    pub fn upload_mesh_skinned(&mut self, data: &MeshData) -> Result<MeshHandle> {
        let allocator = self.allocator();
        let mut alloc = allocator.lock().unwrap();
        let mut vertex_buffer = Buffer::new(
            &self.ctx.device,
            &mut alloc,
            std::mem::size_of_val(&data.vertices[..]) as u64,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            MemoryLocation::CpuToGpu,
            "skinned vertices",
        )?;
        vertex_buffer.write(0, bytemuck::cast_slice(&data.vertices));
        let index_buffer = alloc::upload_buffer(
            &self.ctx.device,
            &mut alloc,
            self.command_pool,
            self.ctx.queue,
            bytemuck::cast_slice(&data.indices),
            vk::BufferUsageFlags::INDEX_BUFFER,
            "skinned indices",
        )?;
        self.meshes.push(GpuMesh {
            vertex_buffer,
            index_buffer,
            index_count: data.indices.len() as u32,
            vertex_count: data.vertices.len() as u32,
        });
        Ok(MeshHandle(self.meshes.len() - 1))
    }

    /// Rewrite a skinned mesh's vertices (host-visible buffer from
    /// [`upload_mesh_skinned`]). Call with the CPU-skinned vertices each frame.
    pub fn update_mesh_vertices(&mut self, handle: MeshHandle, vertices: &[Vertex]) {
        if let Some(mesh) = self.meshes.get_mut(handle.0) {
            mesh.vertex_buffer.write(0, bytemuck::cast_slice(vertices));
        }
    }

    /// Whether the GPU lighting bake can run (ray query support).
    pub fn supports_baking(&self) -> bool {
        self.ctx.ray_tracing()
    }

    /// Run the GPU lighting bake (lightmaps + probe SH). Blocks until done;
    /// invoked from an explicit editor action, never the frame loop.
    pub fn bake_lighting(&self, input: &BakeInput<'_>) -> Result<BakeOutput> {
        let allocator = self.allocator();
        bake::bake(
            &self.ctx,
            &allocator,
            self.command_pool,
            &self.meshes,
            input,
        )
    }

    /// (Re)upload the cached Global Distance Field for the GPU GI march. Call only
    /// when geometry changes; `gi_march` then reuses it every trace. No-op if the
    /// GPU pipeline is unavailable.
    #[allow(clippy::too_many_arguments)]
    pub fn gi_set_gdf(
        &mut self,
        dist: &[f32],
        index: &[u32],
        dims: [u32; 3],
        min: [f32; 3],
        max: [f32; 3],
        materials: &[GpuGiMaterial],
    ) {
        let allocator = self.allocator();
        if let Some(gpu) = self.gpu_gi.as_mut()
            && let Err(e) =
                gpu.set_gdf(&self.ctx, &allocator, self.command_pool, dist, index, dims, min, max, materials)
        {
            tracing::warn!("GPU GI set_gdf failed: {e:#}");
        }
    }

    /// Whether the GPU GI compute pipeline initialised. When false the realtime-GI
    /// driver skips the GDF build/upload entirely and marches on the CPU instead.
    pub fn gi_gpu_available(&self) -> bool {
        self.gpu_gi.is_some()
    }

    /// True while an async GPU march is submitted but not yet read back.
    pub fn gi_marching(&self) -> bool {
        self.gpu_gi.as_ref().is_some_and(|g| g.is_marching())
    }

    /// True once a GDF is uploaded, so screen-space GI can run its gather.
    pub fn gi_has_gdf(&self) -> bool {
        self.gpu_gi.as_ref().is_some_and(|g| g.has_gdf())
    }

    /// Cache the emissive sphere area-lights the screen-GI gather samples (NEE).
    /// A changed emitter set also invalidates the idle-skip so it re-traces.
    pub fn gi_set_emitters(&mut self, emitters: &[GpuGiEmitter]) {
        if self.gi_emitters.len() != emitters.len() {
            self.sgi_last_view_proj = None; // emitter count changed, re-trace
        }
        self.gi_emitters.clear();
        self.gi_emitters.extend_from_slice(emitters);
        // Any emitter change should re-trace (emissive moved/toggled).
        self.sgi_last_view_proj = None;
    }

    /// Set the runtime Flux trace parameters (Environment tab → Flux GI). A
    /// changed setting re-traces so the new quality/intensity takes effect.
    pub fn set_flux_settings(&mut self, s: FluxSettings) {
        self.flux_settings = s;
        self.sgi_last_view_proj = None;
    }

    /// Toggle the screen-GI depth→world Y-flip (corrects a flipped gather without
    /// a rebuild).
    pub fn set_screen_gi_flip_y(&mut self, flip: bool) {
        self.screen_gi_flip_y = flip;
    }

    pub fn screen_gi_flip_y(&self) -> bool {
        self.screen_gi_flip_y
    }

    /// Begin an async GPU march (submit, don't wait). Returns true if a march was
    /// started. The result is collected later via [`gi_march_poll`] so the trace
    /// never blocks the frame.
    pub fn gi_march_begin(&mut self, m: &GpuGiMarch<'_>) -> bool {
        let allocator = self.allocator();
        let Some(gpu) = self.gpu_gi.as_mut() else {
            return false;
        };
        match gpu.march_begin(&self.ctx, &allocator, m) {
            Ok(started) => started,
            Err(e) => {
                tracing::warn!("GPU GI march failed to start: {e:#}");
                false
            }
        }
    }

    /// Collect a finished async GPU march, or None if still running / none in
    /// flight. One `ProbeSh` per probe.
    pub fn gi_march_poll(&mut self) -> Option<Vec<ProbeSh>> {
        let allocator = self.allocator();
        let gpu = self.gpu_gi.as_mut()?;
        gpu.march_poll(&self.ctx, &allocator)
    }

    /// Raw Vulkan handles (as `u64`) + queue family, for sharing this device with
    /// OpenXR (`XR_KHR_vulkan_enable2`): (instance, physical_device, device,
    /// queue_family_index). The single graphics queue is index 0.
    pub fn vulkan_raw_handles(&self) -> (u64, u64, u64, u32) {
        use ash::vk::Handle as _;
        (
            self.ctx.instance.handle().as_raw(),
            self.ctx.physical_device.as_raw(),
            self.ctx.device.handle().as_raw(),
            self.ctx.queue_family,
        )
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

        // 12 texture slots in slot order; bindings skip 4 (the FX UBO).
        let slots = [
            desc.albedo,
            desc.normal,
            desc.orm,
            desc.emission,
            desc.opacity,
            desc.emission_mask,
            desc.matcap[0],
            desc.matcap_mask[0],
            desc.matcap[1],
            desc.matcap_mask[1],
            desc.matcap[2],
            desc.matcap_mask[2],
        ];
        let bindings = TEXTURE_BINDINGS;
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
                    .dst_binding(bindings[i])
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(info)
            })
            .collect();
        unsafe { self.ctx.device.update_descriptor_sets(&writes, &[]) };

        // Per-material FX uniform block (set 1, binding 4).
        let fx_ubo = {
            let allocator = self.allocator();
            let mut alloc = allocator.lock().unwrap();
            make_fx_ubo(
                &self.ctx.device,
                &mut alloc,
                MaterialFx::from_params(&desc.params),
                "material fx ubo",
            )?
        };
        let fx_info = [vk::DescriptorBufferInfo::default()
            .buffer(fx_ubo.handle)
            .range(vk::WHOLE_SIZE)];
        let fx_write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(4)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .buffer_info(&fx_info);
        unsafe { self.ctx.device.update_descriptor_sets(&[fx_write], &[]) };

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
            fx_ubo,
        });
        Ok(MaterialHandle(self.materials.len() - 1))
    }

    /// Rewrite a material's FX uniform block from its current params (set 1,
    /// binding 4). Called after a param edit so rim / animated emission update
    /// live. Host-visible write; materials are static at runtime so there's no
    /// in-flight hazard there, and an editor edit at worst tears for one frame.
    pub fn upload_material_fx(&mut self, handle: MaterialHandle) {
        let Some(material) = self.materials.get_mut(handle.0) else {
            return;
        };
        let fx = MaterialFx::from_params(&material.params);
        material.fx_ubo.write(0, bytemuck::bytes_of(&fx));
    }

    /// Rebind a material's 12 texture slots on its existing descriptor set
    /// (binding order = create_material's slot list; binding 4 is the FX UBO and
    /// is untouched). `None` in a slot binds the 1×1 default for that slot.
    /// Waits for device idle first since the set may be in use by an in-flight
    /// frame; texture edits are user-initiated and rare, so the stall is fine.
    pub fn set_material_textures(&mut self, handle: MaterialHandle, slots: &[Option<TextureHandle>; 12]) {
        let Some(material) = self.materials.get(handle.0) else {
            return;
        };
        let set = material.set;
        let views: Vec<vk::ImageView> = slots
            .iter()
            .enumerate()
            .map(|(i, slot)| match slot {
                Some(h) => self.textures[h.0].view,
                None => self.default_textures[i].view,
            })
            .collect();
        unsafe {
            let _ = self.ctx.device.device_wait_idle();
        }
        let image_infos: Vec<[vk::DescriptorImageInfo; 1]> = views
            .iter()
            .map(|view| {
                [vk::DescriptorImageInfo::default()
                    .sampler(self.sampler)
                    .image_view(*view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
            })
            .collect();
        let writes: Vec<_> = image_infos
            .iter()
            .enumerate()
            .map(|(i, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(TEXTURE_BINDINGS[i])
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(info)
            })
            .collect();
        unsafe { self.ctx.device.update_descriptor_sets(&writes, &[]) };
        // Slot 1 is the normal map; keep the feature gate's source in sync.
        self.materials[handle.0].has_normal_texture = slots[1].is_some();
    }

    /// Set (or clear) the equirectangular skybox texture. `None` reverts to
    /// the procedural gradient sky. The old texture is dropped after the GPU
    /// goes idle.
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
                self.skybox_is_cube = false;
                self.rebuild_reflection_env(Some(data))?;
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
                self.rebuild_reflection_env(None)?;
            }
        }
        Ok(())
    }

    /// Rebuild the prefiltered environment reflection cube from the skybox (or a
    /// neutral cube when there's no skybox) and re-point the `samplerCube`
    /// binding (set 0, binding 7) on every set that samples it. Called from
    /// `set_skybox`, which has already idled the device.
    fn rebuild_reflection_env(&mut self, data: Option<&TextureData>) -> Result<()> {
        let allocator = self.allocator();
        let new_env = {
            let mut alloc = allocator.lock().unwrap();
            match data {
                Some(eq) => ReflectionEnv::from_equirect(
                    &self.ctx.device,
                    &mut alloc,
                    self.command_pool,
                    self.ctx.queue,
                    eq,
                )?,
                None => ReflectionEnv::neutral(
                    &self.ctx.device,
                    &mut alloc,
                    self.command_pool,
                    self.ctx.queue,
                )?,
            }
        };
        self.swap_reflection_env(new_env);
        Ok(())
    }

    /// Replace the env reflection cube + re-point the `samplerCube` binding
    /// (set 0, binding 7) on every set that samples it (frame sets + preview).
    fn swap_reflection_env(&mut self, new_env: ReflectionEnv) {
        let allocator = self.allocator();
        let mut old = std::mem::replace(&mut self.reflection_env, new_env);
        old.destroy(&self.ctx.device, &mut allocator.lock().unwrap());

        let view = self.reflection_env.view;
        let sampler = self.sampler;
        let mut sets: Vec<vk::DescriptorSet> = self.frame_sets.clone();
        if let Some(preview) = &self.camera_preview {
            sets.extend_from_slice(&preview.sets);
        }
        for set in sets {
            let info = [vk::DescriptorImageInfo::default()
                .sampler(sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(7)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { self.ctx.device.update_descriptor_sets(&[write], &[]) };
        }
    }

    /// Set a cubemap / 6-texture skybox (face order +X,-X,+Y,-Y,+Z,-Z). The env
    /// reflection cube is built from the faces (sharp at mip 0 for the skybox,
    /// blurred mips for reflections), and the skybox renders from it (binding 7).
    pub fn set_skybox_cube(&mut self, faces: [&TextureData; 6]) -> Result<()> {
        unsafe {
            let _ = self.ctx.device.device_wait_idle();
        }
        let env = {
            let allocator = self.allocator();
            let mut alloc = allocator.lock().unwrap();
            ReflectionEnv::from_faces(
                &self.ctx.device,
                &mut alloc,
                self.command_pool,
                self.ctx.queue,
                &faces,
            )?
        };
        self.swap_reflection_env(env);
        self.skybox_is_cube = true;
        self.skybox_has_texture = false;
        Ok(())
    }

    /// Request a precomputed scene reflection capture from `center` on the next
    /// frame: the scene's surroundings are rendered into the reflection cube (not
    /// just the sky), so reflective surfaces show real geometry. Call after a
    /// scene loads / lighting settles.
    pub fn request_reflection_capture(&mut self, center: glam::Vec3) {
        self.recapture_reflection = Some(center);
    }

    /// Render the scene into the reflection cube from `center` (6 faces, 90° FOV)
    /// + box-blur a roughness mip chain, then swap it in as the reflection env.
    /// Reuses the frame's lights/shadows. NOTE: face orientation + prefilter want
    /// on-device verification.
    #[allow(clippy::too_many_arguments)]
    fn do_reflection_capture(
        &mut self,
        center: glam::Vec3,
        lights: [GpuLight; MAX_LIGHTS],
        count: usize,
        shadow_vp: [[[f32; 4]; 4]; MAX_SHADOW_VIEWS],
        cascade_splits: [f32; 4],
        input: &FrameInput,
        order: &[usize],
    ) -> Result<()> {
        use glam::{Mat4, Vec3};
        let size = 128u32;
        let device = self.ctx.device.clone();
        let queue = self.ctx.queue;
        let pool = self.command_pool;
        let ext = vk::Extent2D { width: size, height: size };

        let (cube, cube_alloc, mips) = {
            let allocator = self.allocator();
            let mut alloc = allocator.lock().unwrap();
            create_cube_image(&device, &mut alloc, ENV_CUBE_FORMAT, size, "reflection capture cube")?
        };
        let (depth_img, depth_alloc) = {
            let allocator = self.allocator();
            let mut alloc = allocator.lock().unwrap();
            create_image(
                &device,
                &mut alloc,
                DEPTH_FORMAT,
                vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
                ext,
                "reflection capture depth",
            )?
        };
        let depth_view = create_view(&device, depth_img, DEPTH_FORMAT, vk::ImageAspectFlags::DEPTH)?;

        // Face look directions (+X,-X,+Y,-Y,+Z,-Z) matching the cube sampling.
        let faces = [
            (Vec3::X, Vec3::NEG_Y),
            (Vec3::NEG_X, Vec3::NEG_Y),
            (Vec3::Y, Vec3::Z),
            (Vec3::NEG_Y, Vec3::NEG_Z),
            (Vec3::Z, Vec3::NEG_Y),
            (Vec3::NEG_Z, Vec3::NEG_Y),
        ];
        let proj = Mat4::perspective_rh(std::f32::consts::FRAC_PI_2, 1.0, 0.05, 2000.0);
        let capture_set = self.refl_capture_set;

        for (i, (dir, up)) in faces.iter().enumerate() {
            let cam = CameraData {
                view: Mat4::look_at_rh(center, center + *dir, *up),
                proj,
                position: center,
            };
            let mut ubo = frame_ubo(
                &cam, input, lights, count, shadow_vp, cascade_splits,
                &self.probe_volumes, [size as f32, size as f32],
            );
            ubo.debug[2] = 0.0; // no screen-GI in the cube capture
            ubo.debug[3] = 0.0; // inline tonemap -> LDR cube
            ubo.ssr = [0.0; 4];
            self.refl_capture_ubo.write(0, bytemuck::bytes_of(&ubo));
            let face_view = create_cube_face_view(&device, cube, ENV_CUBE_FORMAT, i as u32, 0)?;

            let face_range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(0)
                .level_count(1)
                .base_array_layer(i as u32)
                .layer_count(1);
            crate::alloc::one_time_submit(&device, pool, queue, |cb| {
                unsafe {
                    let to_color = vk::ImageMemoryBarrier::default()
                        .image(cube)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                        .subresource_range(face_range);
                    let dep_b = vk::ImageMemoryBarrier::default()
                        .image(depth_img)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                        .dst_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)
                        .subresource_range(
                            vk::ImageSubresourceRange::default()
                                .aspect_mask(vk::ImageAspectFlags::DEPTH)
                                .level_count(1)
                                .layer_count(1),
                        );
                    device.cmd_pipeline_barrier(
                        cb,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                            | vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_color, dep_b],
                    );
                    let color_att = vk::RenderingAttachmentInfo::default()
                        .image_view(face_view)
                        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                        .load_op(vk::AttachmentLoadOp::CLEAR)
                        .store_op(vk::AttachmentStoreOp::STORE)
                        .clear_value(vk::ClearValue {
                            color: vk::ClearColorValue { float32: input.clear_color },
                        });
                    let depth_att = vk::RenderingAttachmentInfo::default()
                        .image_view(depth_view)
                        .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                        .load_op(vk::AttachmentLoadOp::CLEAR)
                        .store_op(vk::AttachmentStoreOp::DONT_CARE)
                        .clear_value(vk::ClearValue {
                            depth_stencil: vk::ClearDepthStencilValue { depth: 1.0, stencil: 0 },
                        });
                    let atts = [color_att];
                    let info = vk::RenderingInfo::default()
                        .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: ext })
                        .layer_count(1)
                        .color_attachments(&atts)
                        .depth_attachment(&depth_att);
                    device.cmd_begin_rendering(cb, &info);
                    let vp = vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: size as f32,
                        height: size as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    };
                    device.cmd_set_viewport(cb, 0, &[vp]);
                    device.cmd_set_scissor(
                        cb,
                        0,
                        &[vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: ext }],
                    );
                }
                if input.draw_skybox {
                    let _ = self.record_skybox(&device, cb, capture_set, false);
                }
                let _ = self.record_scene_draws(&device, cb, order, input, capture_set);
                unsafe {
                    device.cmd_end_rendering(cb);
                    // Face -> shader-read so the mip-gen blit can read it.
                    let to_read = vk::ImageMemoryBarrier::default()
                        .image(cube)
                        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                        .subresource_range(face_range);
                    device.cmd_pipeline_barrier(
                        cb,
                        vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_read],
                    );
                }
            })?;
            unsafe { device.destroy_image_view(face_view, None) };
        }

        // Generate the roughness mip chain by successive linear blits (all 6
        // layers at once), then leave the whole cube shader-readable.
        crate::alloc::one_time_submit(&device, pool, queue, |cb| unsafe {
            let barrier = |img, old, new, src, dst, mip| {
                vk::ImageMemoryBarrier::default()
                    .image(img)
                    .old_layout(old)
                    .new_layout(new)
                    .src_access_mask(src)
                    .dst_access_mask(dst)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .base_mip_level(mip)
                            .level_count(1)
                            .layer_count(6),
                    )
            };
            // mip 0 is SHADER_READ (from the face renders) -> TRANSFER_SRC.
            device.cmd_pipeline_barrier(
                cb, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(), &[], &[],
                &[barrier(cube, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::AccessFlags::SHADER_READ, vk::AccessFlags::TRANSFER_READ, 0)],
            );
            let mut src_size = size as i32;
            for mip in 1..mips {
                let dst_size = (src_size / 2).max(1);
                device.cmd_pipeline_barrier(
                    cb, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(), &[], &[],
                    &[barrier(cube, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE, mip)],
                );
                let blit = vk::ImageBlit::default()
                    .src_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(mip - 1)
                            .layer_count(6),
                    )
                    .src_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D { x: src_size, y: src_size, z: 1 },
                    ])
                    .dst_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(mip)
                            .layer_count(6),
                    )
                    .dst_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D { x: dst_size, y: dst_size, z: 1 },
                    ]);
                device.cmd_blit_image(
                    cb,
                    cube,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    cube,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::LINEAR,
                );
                // This mip -> TRANSFER_SRC for the next iteration.
                device.cmd_pipeline_barrier(
                    cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(), &[], &[],
                    &[barrier(cube, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::TRANSFER_READ, mip)],
                );
                src_size = dst_size;
            }
            // All mips are now TRANSFER_SRC -> SHADER_READ.
            let full = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(mips)
                .layer_count(6);
            device.cmd_pipeline_barrier(
                cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(), &[], &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(cube)
                    .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .subresource_range(full)],
            );
        })?;

        let cube_view = create_cube_view(&device, cube, ENV_CUBE_FORMAT, mips)?;
        {
            let allocator = self.allocator();
            let mut alloc = allocator.lock().unwrap();
            unsafe {
                device.destroy_image_view(depth_view, None);
                device.destroy_image(depth_img, None);
            }
            let _ = alloc.free(depth_alloc);
        }
        self.swap_reflection_env(ReflectionEnv {
            image: cube,
            view: cube_view,
            alloc: Some(cube_alloc),
        });
        Ok(())
    }

    /// Draw the fullscreen skybox into the active rendering pass (call after
    /// begin_rendering + viewport/scissor, before scene geometry).
    fn record_skybox(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        frame_set: vk::DescriptorSet,
        hdr: bool,
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
        // params0.z = cubemap mode: sample the env cube (binding 7) by direction.
        push.params0[2] = if self.skybox_is_cube { 1.0 } else { 0.0 };
        // params0.w = HDR output: skip inline tonemap (the post pass handles it).
        push.params0[3] = if hdr { 1.0 } else { 0.0 };
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
        for material in &mut self.materials {
            material.fx_ubo.destroy(&self.ctx.device, &mut alloc);
        }
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
        self.sgi.destroy(&self.ctx.device, &mut alloc);
        self.ssr_color.destroy(&self.ctx.device, &mut alloc);
        self.swapchain = Swapchain::new(&self.ctx, size.width, size.height, self.vsync)?;
        self.depth = DepthTarget::new(&self.ctx, &mut alloc, self.swapchain.extent)?;
        self.sgi = ScreenGiTargets::new(&self.ctx, &mut alloc, self.swapchain.extent)?;
        self.ssr_color = ColorHistory::new(
            &self.ctx.device,
            &mut alloc,
            self.command_pool,
            self.ctx.queue,
            post::HDR_FORMAT,
            self.swapchain.extent,
        )?;
        self.post_pass.resize(&self.ctx.device, &mut alloc, self.swapchain.extent)?;
        self.sgi_history_valid = false; // recreated target has no history yet
        self.sgi_parity = 0;
        self.sgi_last_view_proj = None;
        drop(alloc);
        // Re-point the screen-GI sampler (binding 4) at the recreated target.
        for &set in &self.frame_sets {
            let info = [vk::DescriptorImageInfo::default()
                .sampler(self.lightmap_sampler)
                .image_view(self.sgi.gi_view[0])
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { self.ctx.device.update_descriptor_sets(&[write], &[]) };
        }
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
        // Screen-space GI: prefetch the depth-only pipeline (needs &mut self,
        // before the per-frame immutable borrow below) when a GDF is available.
        let sgi_active = self.gi_has_gdf();
        let depth_pipe = if sgi_active {
            // The shadow pipeline is the color-less depth-only one (matches our
            // standalone depth prepass with no color attachment).
            Some(self.pipeline_cache.get(&device, PipelineKey::shadow())?)
        } else {
            None
        };
        let in_flight = self.frames[self.frame_index].in_flight;
        let image_available = self.frames[self.frame_index].image_available;
        let command_buffer = self.frames[self.frame_index].command_buffer;

        unsafe { device.wait_for_fences(&[in_flight], true, u64::MAX)? };

        // This frame slot's prior submission has retired, so the Flux trace
        // resources it referenced (descriptor pool + host buffers, folded into
        // that submission's command buffer) are now safe to free.
        if let Some(mut t) = self.sgi_transients[self.frame_index].take() {
            let alloc = self.allocator();
            t.destroy(&device, &mut alloc.lock().unwrap());
        }
        if let Some(mut t) = self.preview_sgi_transients[self.frame_index].take() {
            let alloc = self.allocator();
            t.destroy(&device, &mut alloc.lock().unwrap());
        }

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

        let image_index = match self.swapchain.acquire(image_available) {
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

        unsafe { device.reset_fences(&[in_flight])? };

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
                soft_shadows: true,
                shadow_bias: 0.0,
            });
            count = 1;
        }

        // Plan shadow maps (allocates atlas layers, patches lights[*].spot)
        // before building the UBOs that carry the shadow matrices.
        let (shadow_views, shadow_vp, cascade_splits) = plan_shadows(input, &mut lights, count);

        // Screen size for the frame UBO = the render target the main camera draws
        // into: the viewport rect under editor RTT, else the swapchain. The Flux
        // upsample samples by gl_FragCoord / this size, so it must match or the GI
        // lands in the wrong place.
        let main_extent = input.viewport_extent.unwrap_or([
            self.swapchain.extent.width,
            self.swapchain.extent.height,
        ]);
        let mut ubo = frame_ubo(
            &input.camera,
            input,
            lights,
            count,
            shadow_vp,
            cascade_splits,
            &self.probe_volumes,
            [main_extent[0] as f32, main_extent[1] as f32],
        );
        // debug.z = screen-GI active, so the forward shader samples the gather
        // instead of the coarse world-probe grid.
        ubo.debug[2] = if sgi_active { 1.0 } else { 0.0 };
        // debug.w = HDR output: both the game swapchain path AND the editor
        // viewport render linear HDR (a post pass tonemaps + blooms). The editor
        // swapchain pass is egui-only; the camera preview keeps inline tonemap via
        // its own UBO. So the main frame UBO is always HDR.
        ubo.debug[3] = 1.0;
        // SSR rides on the Flux depth prepass, so it only runs while screen-GI is
        // active. The per-pass binding writes (below / in record_viewport_scene)
        // point bindings 5/6 at the active camera's depth + colour history.
        let fs = self.flux_settings;
        let ssr_on = fs.ssr_enabled && sgi_active;
        ubo.ssr = if ssr_on {
            [1.0, fs.ssr_intensity, fs.ssr_max_distance, fs.ssr_roughness_cutoff]
        } else {
            [0.0; 4]
        };
        if let Some(p) = input.reflection_probe {
            ubo.refl_center = [p.center[0], p.center[1], p.center[2], p.intensity.max(0.0)];
            ubo.refl_extents = [
                p.half_extents[0],
                p.half_extents[1],
                p.half_extents[2],
                if p.box_projection { 1.0 } else { 0.0 },
            ];
        }
        if let Some(f) = input.fog {
            ubo.fog_color = [f.color[0], f.color[1], f.color[2], f.density.max(0.0)];
            ubo.fog_params = [f.height_falloff.max(0.0), f.height_ref, f.start_distance.max(0.0), 0.0];
        }
        self.frame_ubos[self.frame_index].write(0, bytemuck::bytes_of(&ubo));

        // Decide whether to trace the screen-GI gather this frame, then point
        // the forward sampler (binding 4) at the image that will hold the latest
        // result. The actual depth prepass + GDF trace is recorded into the main
        // frame command buffer below (no separate submit/fence stall).
        let mut sgi_record: Option<(vk::Pipeline, bool, usize, glam::Mat4, glam::Vec3, f32)> = None;
        // Skip the viewport-camera Flux trace when the viewport is hidden (e.g.
        // only the Camera tab is shown) — nothing samples its result.
        if let Some(dp) = depth_pipe.filter(|_| input.render_viewport) {
            // Temporal reprojection means we can keep accumulating while the
            // camera moves (the gather reprojects the previous result). Trace
            // while moving, and for a few frames after stopping so it fully
            // converges, then idle (reuse the converged image; a still camera
            // costs nothing). gi_set_emitters resets sgi_last_view_proj.
            let view_proj = input.camera.proj * input.camera.view;
            let camera_moved = self.sgi_last_view_proj != Some(view_proj);
            if camera_moved {
                self.sgi_still_frames = 0;
            } else {
                self.sgi_still_frames = self.sgi_still_frames.saturating_add(1);
            }
            const SGI_CONVERGE_FRAMES: u32 = 48;
            if camera_moved || self.sgi_still_frames < SGI_CONVERGE_FRAMES {
                // New seed every trace so each frame samples different directions
                // and temporal accumulation can converge the noise.
                self.sgi_frame = self.sgi_frame.wrapping_add(1);
                let hist_valid = self.sgi_history_valid;
                let cur = self.sgi_parity & 1; // ping-pong slot written this frame
                // Capture the previous camera before advancing it, so the gather
                // reprojects history from where the surface actually was last
                // frame (advancing first would make reprojection identity and smear).
                let prev_vp = self.sgi_last_view_proj.unwrap_or(view_proj);
                let prev_cam = if self.sgi_last_view_proj.is_some() {
                    self.sgi_last_cam
                } else {
                    input.camera.position
                };
                // Motion-aware temporal blend, scaled by the Flux smoothing
                // setting (0 = responsive and sharper, 1 = smooth with more lag):
                // snap on the first frame, trust the fresh trace more while moving
                // (cuts reprojection-residual smear), converge gently when still.
                let s = self.flux_settings.smoothing.clamp(0.0, 1.0);
                let alpha = if !hist_valid {
                    1.0
                } else if camera_moved {
                    0.5 - 0.4 * s
                } else {
                    0.12 - 0.10 * s
                };
                sgi_record = Some((dp, hist_valid, cur, prev_vp, prev_cam, alpha));
                // Advance temporal state for next frame; the swap makes this
                // frame's output the next frame's history.
                self.sgi_history_valid = true;
                self.sgi_last_view_proj = Some(view_proj);
                self.sgi_last_cam = input.camera.position;
                self.sgi_parity ^= 1;
            }
            // After the swap, gi[parity ^ 1] is the slot written this frame (or
            // the last-written one on a skip frame); sample that.
            let cur = (self.sgi_parity ^ 1) & 1;
            let cur_view = self.sgi.gi_view[cur];
            let info = [vk::DescriptorImageInfo::default()
                .sampler(self.lightmap_sampler)
                .image_view(cur_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(self.frame_sets[self.frame_index])
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { device.update_descriptor_sets(&[write], &[]) };

            // SSR (bindings 5/6) for the swapchain (game) path: this camera's
            // depth prepass + its colour history. The viewport path re-points
            // these to its own targets in record_viewport_scene.
            let depth_info = [vk::DescriptorImageInfo::default()
                .sampler(self.lightmap_sampler)
                .image_view(self.sgi.depth_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let color_info = [vk::DescriptorImageInfo::default()
                .sampler(self.lightmap_sampler)
                .image_view(self.ssr_color.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let ssr_writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(self.frame_sets[self.frame_index])
                    .dst_binding(5)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&depth_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(self.frame_sets[self.frame_index])
                    .dst_binding(6)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&color_info),
            ];
            unsafe { device.update_descriptor_sets(&ssr_writes, &[]) };
        }

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
                        self.probe_buffer.handle,
                        self.lightmaps.view,
                        self.lightmap_sampler,
                        self.reflection_env.view,
                        self.egui.as_mut().unwrap(),
                    )?;
                    self.camera_preview = Some(preview);
                }
            }
            if let Some(preview) = &mut self.camera_preview {
                let mut cam_ubo = frame_ubo(
                    camera,
                    input,
                    lights,
                    count,
                    shadow_vp,
                    cascade_splits,
                    &self.probe_volumes,
                    [preview.extent.width as f32, preview.extent.height as f32],
                );
                // The preview runs its own Flux trace when a GDF exists, so flag
                // its shader to sample the screen-GI (binding 4) like the viewport.
                cam_ubo.debug[2] = if sgi_active { 1.0 } else { 0.0 };
                preview.ubos[self.frame_index].write(0, bytemuck::bytes_of(&cam_ubo));
            }
        }

        // Editor RTT: (re)create the offscreen viewport target at the requested
        // dock-rect size. None (game) leaves it absent → renders to swapchain.
        if let Some([vw, vh]) = input.viewport_extent {
            let vw = vw.max(1);
            let vh = vh.max(1);
            let need = self
                .viewport_target
                .as_ref()
                .map_or(true, |t| t.extent.width != vw || t.extent.height != vh);
            if need && let Some(allocator) = self.allocator.clone() {
                unsafe {
                    let _ = self.ctx.device.device_wait_idle();
                }
                let mut alloc = allocator.lock().unwrap();
                if let Some(mut old) = self.viewport_target.take() {
                    old.destroy(&self.ctx.device, &mut alloc);
                }
                let fmt = self.swapchain.format.format;
                let post_layout = self.post_pass.set_layout();
                let post_sampler = self.post_pass.sampler();
                self.viewport_target = Some(ViewportTarget::new(
                    &self.ctx,
                    &mut alloc,
                    self.command_pool,
                    self.ctx.queue,
                    fmt,
                    self.sampler,
                    post_layout,
                    post_sampler,
                    vk::Extent2D { width: vw, height: vh },
                    self.egui.as_mut().unwrap(),
                )?);
            }
        }

        let cb = command_buffer;
        let image = self.swapchain.images[image_index as usize];
        let color_view = self.swapchain.views[image_index as usize];
        let extent = self.swapchain.extent;
        let render_finished = self.swapchain.render_finished[image_index as usize];
        let frame_set = self.frame_sets[self.frame_index];

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

        // Precomputed scene reflection capture (once, on request): render the
        // scene into the reflection cube from the requested center, reusing this
        // frame's lights/shadows. Uses its own submits, before the main cb.
        if let Some(center) = self.recapture_reflection.take() {
            if let Err(e) =
                self.do_reflection_capture(center, lights, count, shadow_vp, cascade_splits, input, &order)
            {
                tracing::warn!("reflection capture failed: {e:#}");
            }
        }

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

            // Flux GI: depth prepass + GDF trace, folded into this frame's cb so
            // there are no extra submits or fence stalls. Writes the gather image
            // that binding 4 (set above) now points at; transitions it to
            // SHADER_READ before the forward pass samples it.
            if let Some((dp, hist_valid, cur, prev_vp, prev_cam, alpha)) = sgi_record {
                if let Err(e) =
                    self.record_screen_gi(&device, cb, dp, input, hist_valid, cur, prev_vp, prev_cam, alpha)
                {
                    tracing::warn!("Flux GI record failed: {e:#}");
                }
            }

            // Offscreen main-camera pass next, so the swapchain's egui pass
            // can sample the result this frame.
            if render_camera_preview && self.camera_preview.is_some() {
                self.record_camera_preview(&device, cb, &order, input, input.clear_color)?;
            }

            // Editor RTT: render the viewport 3D into its offscreen texture (the
            // swapchain pass below skips the scene because the editor sets
            // render_viewport=false, and egui shows this texture).
            if input.viewport_extent.is_some() && self.viewport_target.is_some() {
                if let Err(e) = self.record_viewport_scene(&device, cb, &order, input) {
                    tracing::warn!("viewport RTT failed: {e:#}");
                }
            }

            // The game path renders the scene into the HDR target (the post pass
            // tonemaps + blooms into the swapchain afterwards); the editor
            // swapchain pass (egui only) renders straight to the swapchain.
            let game = input.render_viewport;
            let (scene_image, scene_view) = if game {
                (
                    self.post_pass.hdr_image(self.frame_index),
                    self.post_pass.hdr_view(self.frame_index),
                )
            } else {
                (image, color_view)
            };

            // Color: undefined -> color attachment.
            image_barrier(
                &device,
                cb,
                scene_image,
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
                .image_view(scene_view)
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

            // Skybox + scene only when the viewport is visible; otherwise the
            // swapchain is just cleared (above) and egui draws the dock over it.
            let mut stats = RenderStats::default();
            if input.render_viewport {
                if input.draw_skybox {
                    self.record_skybox(&device, cb, frame_set, true)?;
                }
                stats = self.record_scene_draws(&device, cb, &order, input, frame_set)?;
            }

            // Selection outlines: inverted hull pass over highlighted draws.
            let outline_draws: Vec<usize> = (0..input.draws.len())
                .filter(|&i| input.draws[i].highlight > 0.0)
                .collect();
            if input.render_viewport && !outline_draws.is_empty() {
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

            // Editor swapchain pass draws egui here (no scene/HDR). The game path
            // draws egui in the post pass below (on the tonemapped image).
            if !game {
                if let Some(egui_draw) = &input.egui {
                    self.egui
                        .as_mut()
                        .unwrap()
                        .cmd_draw(cb, extent, egui_draw.pixels_per_point, &egui_draw.primitives)
                        .map_err(|e| anyhow::anyhow!("recording egui draw: {e}"))?;
                }
            }

            device.cmd_end_rendering(cb);

            if game {
                // SSR colour history: copy this frame's HDR scene colour so next
                // frame's reflection march has a source.
                self.ssr_color.record_copy(
                    &device,
                    cb,
                    scene_image,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                );
                // HDR scene -> shader-read for the post pass.
                image_barrier(
                    &device, cb, scene_image, vk::ImageAspectFlags::COLOR,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::SHADER_READ,
                );
                // Swapchain -> color attachment for the post pass.
                image_barrier(
                    &device, cb, image, vk::ImageAspectFlags::COLOR,
                    vk::ImageLayout::UNDEFINED, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::empty(),
                    vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                );
                let post_color = vk::RenderingAttachmentInfo::default()
                    .image_view(color_view)
                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::DONT_CARE)
                    .store_op(vk::AttachmentStoreOp::STORE);
                let post_attachments = [post_color];
                let post_info = vk::RenderingInfo::default()
                    .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent })
                    .layer_count(1)
                    .color_attachments(&post_attachments);
                device.cmd_begin_rendering(cb, &post_info);
                self.post_pass
                    .record(&device, cb, self.frame_index, extent, &input.postfx);
                // egui on top of the tonemapped image.
                if let Some(egui_draw) = &input.egui {
                    self.egui
                        .as_mut()
                        .unwrap()
                        .cmd_draw(cb, extent, egui_draw.pixels_per_point, &egui_draw.primitives)
                        .map_err(|e| anyhow::anyhow!("recording egui draw: {e}"))?;
                }
                device.cmd_end_rendering(cb);
            }

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

/// Bilinear resample of a square RGBA32F image (`src_size`²) to `dst`². Used to
/// pack per-object lightmaps (varying resolutions) into one uniform texture
/// array. Same-size (or malformed) input is copied/padded.
fn resample_rgba32f(src: &[f32], src_size: u32, dst: u32) -> Vec<f32> {
    let s = src_size.max(1) as usize;
    let d = dst.max(1) as usize;
    if s == d || src.len() < s * s * 4 {
        let mut out = vec![0.0; d * d * 4];
        let k = out.len().min(src.len());
        out[..k].copy_from_slice(&src[..k]);
        return out;
    }
    let mut out = vec![0.0f32; d * d * 4];
    let smax = (s - 1) as f32;
    for y in 0..d {
        let fy = (y as f32 + 0.5) * s as f32 / d as f32 - 0.5;
        let y0 = fy.floor().clamp(0.0, smax) as usize;
        let y1 = (y0 + 1).min(s - 1);
        let ty = (fy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..d {
            let fx = (x as f32 + 0.5) * s as f32 / d as f32 - 0.5;
            let x0 = fx.floor().clamp(0.0, smax) as usize;
            let x1 = (x0 + 1).min(s - 1);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            for c in 0..4 {
                let p00 = src[(y0 * s + x0) * 4 + c];
                let p10 = src[(y0 * s + x1) * 4 + c];
                let p01 = src[(y1 * s + x0) * 4 + c];
                let p11 = src[(y1 * s + x1) * 4 + c];
                let top = p00 + (p10 - p00) * tx;
                let bot = p01 + (p11 - p01) * tx;
                out[(y * d + x) * 4 + c] = top + (bot - top) * ty;
            }
        }
    }
    out
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
    probe_volumes: &[GpuProbeVolume],
    screen: [f32; 2],
) -> FrameUbo {
    let mut volumes = [GpuProbeVolume::default(); MAX_PROBE_VOLUMES];
    let nv = probe_volumes.len().min(MAX_PROBE_VOLUMES);
    volumes[..nv].copy_from_slice(&probe_volumes[..nv]);
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
        misc: [input.time, count as f32, input.shadow_pcf_texel, nv as f32],
        cascade_splits,
        lights,
        shadow_vp,
        probe_volumes: volumes,
        debug: [
            if input.lightmap_preview { 1.0 } else { 0.0 },
            input.gi_debug as f32,
            0.0,
            0.0,
        ],
        inv_view: camera.view.inverse().to_cols_array_2d(),
        inv_proj: camera.proj.inverse().to_cols_array_2d(),
        // SSR disabled by default; the renderer sets this per-pass from
        // flux_settings once it knows the depth prepass + colour history are valid.
        ssr: [0.0; 4],
        // Reflection-probe zone; the renderer fills these from input.reflection_probe.
        refl_center: [0.0; 4],
        refl_extents: [0.0; 4],
        // Fog; the renderer fills these from input.fog.
        fog_color: [0.0; 4],
        fog_params: [0.0; 4],
        postfx0: [
            input.postfx.tonemap as f32,
            input.postfx.exposure,
            input.postfx.grade_exposure,
            input.postfx.contrast,
        ],
        postfx1: [
            input.postfx.saturation,
            input.postfx.temperature,
            input.postfx.tint,
            if input.postfx.grading_enabled { 1.0 } else { 0.0 },
        ],
        postfx2: [
            if input.postfx.vignette_enabled { 1.0 } else { 0.0 },
            input.postfx.vignette_intensity,
            input.postfx.vignette_smoothness,
            screen[0],
        ],
        postfx3: [
            input.postfx.vignette_color[0],
            input.postfx.vignette_color[1],
            input.postfx.vignette_color[2],
            screen[1],
        ],
    }
}

impl Renderer {
    /// Screen-space GI: render a camera depth prepass, then trace the GDF per
    /// pixel into the gather target (sampled by the forward pass). Synchronous
    /// (own submits) for now. `depth_pipe` is the depth-only pipeline; `frame_set`
    /// supplies the camera UBO (binding 0). Returns true if the gather ran.
    /// Record the Flux gather (depth prepass + GDF trace) into the main frame
    /// command buffer `cb`, writing gather image `cur`. No fence wait: the
    /// per-frame trace resources are returned into `sgi_transients[frame_index]`
    /// and freed one frame later, after this frame's fence signals. `cur` is the
    /// ping-pong slot to write; `cur ^ 1` is the history read this frame.
    #[allow(clippy::too_many_arguments)]
    /// Thin wrapper: trace Flux for the editor viewport camera into `self.sgi`,
    /// stashing the per-frame transient for deferred free.
    fn record_screen_gi(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        depth_pipe: vk::Pipeline,
        input: &FrameInput,
        history_valid: bool,
        cur: usize,
        prev_vp: glam::Mat4,
        prev_cam: glam::Vec3,
        alpha: f32,
    ) -> Result<()> {
        let prev = cur ^ 1;
        let t = self.record_flux_trace(
            device,
            cb,
            depth_pipe,
            input,
            &input.camera,
            self.sgi.depth_image,
            self.sgi.depth_view,
            self.sgi.probe_extent,
            self.sgi.gi_image[cur],
            self.sgi.gi_view[cur],
            self.sgi.gi_view[prev],
            self.swapchain.extent,
            history_valid,
            prev_vp,
            prev_cam,
            alpha,
            self.sgi_frame,
            self.screen_gi_flip_y,
        )?;
        if let Some(t) = t {
            self.sgi_transients[self.frame_index] = Some(t);
        }
        Ok(())
    }

    /// Record a Flux trace (depth prepass + GDF gather) for an arbitrary camera
    /// into caller-owned targets, returning the per-frame transient to hold until
    /// the frame fence signals. Shared by the editor viewport and the in-game
    /// camera preview (each passes its own camera + targets).
    #[allow(clippy::too_many_arguments)]
    fn record_flux_trace(
        &self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        depth_pipe: vk::Pipeline,
        input: &FrameInput,
        cam: &CameraData,
        depth_image: vk::Image,
        depth_view: vk::ImageView,
        probe_extent: vk::Extent2D,
        gi_out_image: vk::Image,
        gi_out_view: vk::ImageView,
        gi_hist_view: vk::ImageView,
        full_extent: vk::Extent2D,
        history_valid: bool,
        prev_vp: glam::Mat4,
        prev_cam: glam::Vec3,
        alpha: f32,
        frame_seed: u32,
        flip_y: bool,
    ) -> Result<Option<gpu_gi::ScreenGiTransient>> {
        if self.gpu_gi.as_ref().map_or(true, |g| !g.has_gdf()) {
            return Ok(None);
        }
        let extent = full_extent;
        // 1) Depth prepass: render opaque geometry depth-only into depth_image,
        //    recorded directly into the main cb (no separate submit/stall).
        unsafe {
            let to_depth = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)
                .image(depth_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::DEPTH)
                        .level_count(1)
                        .layer_count(1),
                );
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_depth],
            );
            let depth_att = vk::RenderingAttachmentInfo::default()
                .image_view(depth_view)
                .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue { depth: 1.0, stencil: 0 },
                });
            let info = vk::RenderingInfo::default()
                .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent })
                .layer_count(1)
                .depth_attachment(&depth_att);
            device.cmd_begin_rendering(cb, &info);
            // Match the main pass's negative-height (Y-flip) viewport so the
            // depth buffer aligns with the shaded image.
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
                &[vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent }],
            );
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, depth_pipe);
            // The shadow pipeline (depth-only, no color attachment) uses
            // `shadow.vert`, which reads only push constants (no descriptor sets).
            let view_proj = (cam.proj * cam.view).to_cols_array_2d();
            for draw in input.draws {
                // Transparent surfaces are kept out of the Flux depth prepass:
                // otherwise the screen probe at a glass pixel reconstructs the
                // glass and traces GI there, which the glass samples as a milky
                // diffuse fill. (Emissive surfaces ARE included now — they sample
                // their own GI; the old square-halo bleed was self-illumination
                // from the half-Lambert wrap, which the proper cosine removed.)
                let mat = &self.materials[draw.material.0];
                if mat.render_queue >= RENDER_QUEUE_TRANSPARENT {
                    continue;
                }
                let mesh = &self.meshes[draw.mesh.0];
                let push = ShadowPush {
                    light_vp: view_proj,
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
                device.cmd_bind_index_buffer(cb, mesh.index_buffer.handle, 0, vk::IndexType::UINT32);
                device.cmd_draw_indexed(cb, mesh.index_count, 1, 0, 0, 0);
            }
            device.cmd_end_rendering(cb);
            let to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .image(depth_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::DEPTH)
                        .level_count(1)
                        .layer_count(1),
                );
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );
        }

        // 2) Screen-GI gather: trace the GDF per pixel into the gather image.
        let allocator = self.allocator();
        let inv_vp = (cam.proj * cam.view).inverse();
        let amb = input.light.ambient; // dim sky/ambient for escaped rays
        let lights: Vec<BakeLight> = input
            .lights
            .iter()
            .map(|l| BakeLight {
                kind: l.kind,
                position: l.position,
                direction: l.direction,
                color: [
                    l.color[0] * l.intensity,
                    l.color[1] * l.intensity,
                    l.color[2] * l.intensity,
                ],
                range: l.range,
                spot_inner_deg: l.spot_inner_deg,
                spot_outer_deg: l.spot_outer_deg,
                radius: 0.0,
            })
            .collect();
        // prev_vp / prev_cam are the PREVIOUS frame's camera (captured before the
        // caller advanced sgi_last_*), so the gather reprojects history correctly;
        // alpha is the motion-aware temporal blend the caller chose.
        // Runtime Flux params (Environment tab → Flux GI). Samples/bounces drive
        // the trace; temporal accumulation smooths low per-frame sample counts.
        // march[1] (max trace distance) of 0 means auto: screen_resolve_record
        // fills it from the GDF diagonal. sky.w carries the indirect intensity;
        // march.w the firefly clamp.
        let fs = self.flux_settings;
        let params = gpu_gi::ScreenGiParams {
            inv_view_proj: inv_vp.to_cols_array_2d(),
            prev_view_proj: prev_vp.to_cols_array_2d(),
            cam: cam.position.extend(1.0).to_array(),
            prev_cam: prev_cam.extend(1.0).to_array(),
            gdf_min: [0.0; 4],
            gdf_max: [0.0; 4],
            sky: [amb[0], amb[1], amb[2], fs.intensity],
            counts: [fs.samples.max(1), fs.bounces.max(1), 0, 0],
            march: [0.02, fs.march_distance.max(0.0), alpha, fs.firefly_clamp],
            misc: [
                frame_seed,
                full_extent.width,
                full_extent.height,
                if flip_y { 1 } else { 0 },
            ],
        };
        // Trace at probe resolution (one probe per SCREEN_PROBE_DIV² pixels);
        // the depth is full-res but sampled by normalized UV, so this is
        // resolution-independent. The forward shader bilinearly upsamples.
        let transient = self.gpu_gi.as_ref().unwrap().screen_resolve_record(
            device,
            cb,
            &allocator,
            depth_view,
            self.lightmap_sampler,
            gi_out_image,
            gi_out_view,
            gi_hist_view,
            probe_extent,
            &lights,
            &self.gi_emitters,
            history_valid,
            params,
        )?;
        Ok(transient)
    }

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
                // Custom shaders get 15 prop floats; the 16th lane (d3.w) is
                // reserved for the baked-lightmap layer so custom shaders can
                // sample static GI just like the standard shader (preamble's
                // `citrus_baked_gi`). -1 = no lightmap → fall back to probe GI.
                let layer = if input.lightmap_preview {
                    draw.lightmap_size as f32
                } else {
                    draw.lightmap_layer as f32
                };
                PushData {
                    model: draw.transform.to_cols_array_2d(),
                    base_color: d[0..4].try_into().unwrap(),
                    emission: d[4..8].try_into().unwrap(),
                    params0: d[8..12].try_into().unwrap(),
                    params1: [d[12], d[13], d[14], layer],
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
                    // w = baked-lightmap layer (-1 = none); the standard shader
                    // samples it for static GI. (Selection highlight is drawn by
                    // the separate outline pass, so w is free here.)
                    params1: [
                        p.alpha_cutoff,
                        p.normal_strength,
                        p.occlusion_strength,
                        // Normally the lightmap layer; in UV-checker preview the
                        // object's lightmap resolution (the shader reads it then).
                        if input.lightmap_preview {
                            draw.lightmap_size as f32
                        } else {
                            draw.lightmap_layer as f32
                        },
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
        // Flux GI for the in-game camera: trace from the preview (game) camera
        // into the preview's own targets, before its color pass, so the preview
        // shows screen-space GI like the main viewport. Folded into the same cb;
        // its own temporal state + transient ring (freed a frame later).
        if self.gi_has_gdf()
            && let Some(pcam) = input.camera_preview.as_ref()
        {
            let (sgi_depth_image, sgi_depth_view, sgi_probe_extent, sgi_gi_image, sgi_gi_view, pextent) = {
                let p = self.camera_preview.as_ref().unwrap();
                (
                    p.sgi.depth_image,
                    p.sgi.depth_view,
                    p.sgi.probe_extent,
                    p.sgi.gi_image,
                    p.sgi.gi_view,
                    p.extent,
                )
            };
            let depth_pipe = self.pipeline_cache.get(device, PipelineKey::shadow())?;
            let vp = pcam.proj * pcam.view;
            let moved = self.preview_sgi_last_view_proj != Some(vp);
            let cur = self.preview_sgi_parity & 1;
            let prev = cur ^ 1;
            let prev_vp = self.preview_sgi_last_view_proj.unwrap_or(vp);
            let prev_cam = if self.preview_sgi_last_view_proj.is_some() {
                self.preview_sgi_last_cam
            } else {
                pcam.position
            };
            let hist_valid = self.preview_sgi_history_valid;
            let s = self.flux_settings.smoothing.clamp(0.0, 1.0);
            let alpha = if !hist_valid {
                1.0
            } else if moved {
                0.5 - 0.4 * s
            } else {
                0.12 - 0.10 * s
            };
            self.preview_sgi_frame = self.preview_sgi_frame.wrapping_add(1);
            let seed = self.preview_sgi_frame;
            let flip = self.screen_gi_flip_y;
            match self.record_flux_trace(
                device,
                cb,
                depth_pipe,
                input,
                pcam,
                sgi_depth_image,
                sgi_depth_view,
                sgi_probe_extent,
                sgi_gi_image[cur],
                sgi_gi_view[cur],
                sgi_gi_view[prev],
                pextent,
                hist_valid,
                prev_vp,
                prev_cam,
                alpha,
                seed,
                flip,
            ) {
                Ok(t) => {
                    if let Some(t) = t {
                        self.preview_sgi_transients[self.frame_index] = Some(t);
                    }
                    self.preview_sgi_history_valid = true;
                    self.preview_sgi_last_view_proj = Some(vp);
                    self.preview_sgi_last_cam = pcam.position;
                    self.preview_sgi_parity ^= 1;
                    // Point the preview's binding 4 at the gather just written.
                    let view = sgi_gi_view[cur];
                    let set = self.camera_preview.as_ref().unwrap().sets[self.frame_index];
                    let info = [vk::DescriptorImageInfo::default()
                        .sampler(self.lightmap_sampler)
                        .image_view(view)
                        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                    let write = vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(4)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&info);
                    unsafe { device.update_descriptor_sets(&[write], &[]) };
                }
                Err(e) => tracing::warn!("preview Flux trace failed: {e:#}"),
            }
        }

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
            self.record_skybox(device, cb, frame_set, false)?;
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

    /// egui texture for the editor viewport's offscreen render (RTT), if active.
    pub fn viewport_texture(&self) -> Option<(egui::TextureId, [f32; 2])> {
        self.viewport_target.as_ref().map(|t| {
            (t.texture_id, [t.extent.width as f32, t.extent.height as f32])
        })
    }

    /// Editor RTT: render the viewport 3D (Flux trace + skybox + scene) into the
    /// viewport target's offscreen color/depth at the dock-rect resolution, then
    /// transition the color to SHADER_READ so egui can show it. Uses the editor
    /// (main) camera + its sgi temporal state, but the target's own rect-sized
    /// sgi/color/depth. Only called by the editor; the game renders to swapchain.
    fn record_viewport_scene(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        order: &[usize],
        input: &FrameInput,
    ) -> Result<()> {
        let (col_img, col_view, dep_img, dep_view, ext, s_depth_img, s_depth_view, s_probe, s_img, s_view, s_ssr_view, hdr_img, hdr_view, post_set) = {
            let vt = self.viewport_target.as_ref().unwrap();
            (
                vt.color, vt.color_view, vt.depth, vt.depth_view, vt.extent,
                vt.sgi.depth_image, vt.sgi.depth_view, vt.sgi.probe_extent,
                vt.sgi.gi_image, vt.sgi.gi_view, vt.ssr_color.view,
                vt.hdr_image, vt.hdr_view, vt.post_set,
            )
        };
        let frame_set = self.frame_sets[self.frame_index];

        // 1) Flux trace (editor camera -> the viewport target's sgi), reusing the
        //    main-camera temporal state.
        if self.gi_has_gdf() {
            let dp = self.pipeline_cache.get(device, PipelineKey::shadow())?;
            let view_proj = input.camera.proj * input.camera.view;
            let camera_moved = self.sgi_last_view_proj != Some(view_proj);
            if camera_moved {
                self.sgi_still_frames = 0;
            } else {
                self.sgi_still_frames = self.sgi_still_frames.saturating_add(1);
            }
            const CF: u32 = 48;
            if camera_moved || self.sgi_still_frames < CF {
                self.sgi_frame = self.sgi_frame.wrapping_add(1);
                let hist_valid = self.sgi_history_valid;
                let cur = self.sgi_parity & 1;
                let prev_vp = self.sgi_last_view_proj.unwrap_or(view_proj);
                let prev_cam = if self.sgi_last_view_proj.is_some() {
                    self.sgi_last_cam
                } else {
                    input.camera.position
                };
                let s = self.flux_settings.smoothing.clamp(0.0, 1.0);
                let alpha = if !hist_valid {
                    1.0
                } else if camera_moved {
                    0.5 - 0.4 * s
                } else {
                    0.12 - 0.10 * s
                };
                let seed = self.sgi_frame;
                let flip = self.screen_gi_flip_y;
                let t = self.record_flux_trace(
                    device, cb, dp, input, &input.camera, s_depth_img, s_depth_view, s_probe,
                    s_img[cur], s_view[cur], s_view[cur ^ 1], ext, hist_valid, prev_vp, prev_cam,
                    alpha, seed, flip,
                )?;
                if let Some(t) = t {
                    self.sgi_transients[self.frame_index] = Some(t);
                }
                self.sgi_history_valid = true;
                self.sgi_last_view_proj = Some(view_proj);
                self.sgi_last_cam = input.camera.position;
                self.sgi_parity ^= 1;
            }
            let cur = (self.sgi_parity ^ 1) & 1;
            let info = [vk::DescriptorImageInfo::default()
                .sampler(self.lightmap_sampler)
                .image_view(s_view[cur])
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default()
                .dst_set(frame_set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info);
            unsafe { device.update_descriptor_sets(&[write], &[]) };

            // SSR (bindings 5/6) for the editor viewport: this target's depth
            // prepass + its own colour history.
            let depth_info = [vk::DescriptorImageInfo::default()
                .sampler(self.lightmap_sampler)
                .image_view(s_depth_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let color_info = [vk::DescriptorImageInfo::default()
                .sampler(self.lightmap_sampler)
                .image_view(s_ssr_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let ssr_writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(frame_set)
                    .dst_binding(5)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&depth_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(frame_set)
                    .dst_binding(6)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&color_info),
            ];
            unsafe { device.update_descriptor_sets(&ssr_writes, &[]) };
        }

        // 2) Scene color pass into the viewport's HDR target (post pass tonemaps
        //    + blooms into `color` afterwards).
        unsafe {
            image_barrier(
                device, cb, hdr_img, vk::ImageAspectFlags::COLOR,
                vk::ImageLayout::UNDEFINED, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            );
            image_barrier(
                device, cb, dep_img, vk::ImageAspectFlags::DEPTH,
                vk::ImageLayout::UNDEFINED, vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
            );
            let color_att = vk::RenderingAttachmentInfo::default()
                .image_view(hdr_view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue { float32: [0.016, 0.016, 0.024, 1.0] },
                });
            let depth_att = vk::RenderingAttachmentInfo::default()
                .image_view(dep_view)
                .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::DONT_CARE)
                .clear_value(vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue { depth: 1.0, stencil: 0 },
                });
            let color_atts = [color_att];
            let info = vk::RenderingInfo::default()
                .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: ext })
                .layer_count(1)
                .color_attachments(&color_atts)
                .depth_attachment(&depth_att);
            device.cmd_begin_rendering(cb, &info);
            let viewport = vk::Viewport {
                x: 0.0,
                y: ext.height as f32,
                width: ext.width as f32,
                height: -(ext.height as f32),
                min_depth: 0.0,
                max_depth: 1.0,
            };
            device.cmd_set_viewport(cb, 0, &[viewport]);
            device.cmd_set_scissor(cb, 0, &[vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: ext }]);
        }
        if input.draw_skybox {
            self.record_skybox(device, cb, frame_set, false)?;
        }
        self.record_scene_draws(device, cb, order, input, frame_set)?;
        unsafe { device.cmd_end_rendering(cb) };
        // SSR colour history for the viewport: copy the HDR scene colour before it
        // goes shader-read for the post pass.
        if self.flux_settings.ssr_enabled && self.gi_has_gdf() {
            if let Some(vt) = self.viewport_target.as_mut() {
                vt.ssr_color.record_copy(
                    device,
                    cb,
                    hdr_img,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                );
            }
        }
        // HDR -> shader-read; then the post pass (tonemap + bloom) into `color`.
        image_barrier(
            device, cb, hdr_img, vk::ImageAspectFlags::COLOR,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::SHADER_READ,
        );
        image_barrier(
            device, cb, col_img, vk::ImageAspectFlags::COLOR,
            vk::ImageLayout::UNDEFINED, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::empty(),
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        );
        unsafe {
            let post_att = vk::RenderingAttachmentInfo::default()
                .image_view(col_view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::DONT_CARE)
                .store_op(vk::AttachmentStoreOp::STORE);
            let atts = [post_att];
            let info = vk::RenderingInfo::default()
                .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: ext })
                .layer_count(1)
                .color_attachments(&atts);
            device.cmd_begin_rendering(cb, &info);
            self.post_pass.record_set(device, cb, post_set, ext, &input.postfx);
            device.cmd_end_rendering(cb);
        }
        image_barrier(
            device, cb, col_img, vk::ImageAspectFlags::COLOR,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::SHADER_READ,
        );
        Ok(())
    }
}

/// Bind a graphics pipeline (small helper to keep the draw loop tidy).
fn device_bind(device: &ash::Device, cb: vk::CommandBuffer, pipeline: vk::Pipeline) {
    unsafe {
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
    }
}

/// Create a host-visible uniform buffer holding one [`MaterialFx`] block.
fn make_fx_ubo(
    device: &ash::Device,
    alloc: &mut Allocator,
    fx: MaterialFx,
    name: &str,
) -> Result<Buffer> {
    let mut buf = Buffer::new(
        device,
        alloc,
        size_of::<MaterialFx>() as u64,
        vk::BufferUsageFlags::UNIFORM_BUFFER,
        MemoryLocation::CpuToGpu,
        name,
    )?;
    buf.write(0, bytemuck::bytes_of(&fx));
    Ok(buf)
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
            self.lightmaps.destroy(device, &mut alloc);
            self.probe_buffer.destroy(device, &mut alloc);
            if let Some(gpu_gi) = &mut self.gpu_gi {
                gpu_gi.destroy(device, &mut alloc);
            }
            // Free any deferred Flux trace resources still held (the device idle
            // wait above guarantees the GPU is done with them).
            for slot in &mut self.sgi_transients {
                if let Some(mut t) = slot.take() {
                    t.destroy(device, &mut alloc);
                }
            }
            for slot in &mut self.preview_sgi_transients {
                if let Some(mut t) = slot.take() {
                    t.destroy(device, &mut alloc);
                }
            }
            for ubo in &mut self.frame_ubos {
                ubo.destroy(device, &mut alloc);
            }
            for material in &mut self.materials {
                material.fx_ubo.destroy(device, &mut alloc);
            }
            self.default_fx_ubo.destroy(device, &mut alloc);
            if let Some(mut preview) = self.camera_preview.take() {
                preview.destroy(device, &mut alloc);
            }
            if let Some(mut vt) = self.viewport_target.take() {
                vt.destroy(device, &mut alloc);
            }
            self.depth.destroy(device, &mut alloc);
            self.sgi.destroy(device, &mut alloc);
            self.ssr_color.destroy(device, &mut alloc);
            self.post_pass.destroy(device, &mut alloc);
            self.reflection_env.destroy(device, &mut alloc);
            self.refl_capture_ubo.destroy(device, &mut alloc);
        }
        unsafe {
            device.destroy_descriptor_pool(self.refl_capture_pool, None);
        }

        unsafe {
            device.destroy_sampler(self.sampler, None);
            device.destroy_sampler(self.lightmap_sampler, None);
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
