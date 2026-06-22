//! Public data types of the renderer API: CPU-side asset data, material
//! definitions, handles, and per-frame input.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use serde::{Deserialize, Serialize};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    pub color: [f32; 4],
    /// xyz tangent, w handedness (+1/-1), glTF convention.
    pub tangent: [f32; 4],
    /// Second UV set: lightmap / baked-lighting coordinates. Read from the
    /// model's second UV channel, or generated when absent. Kept at the end of
    /// the struct so existing vertex attributes keep their offsets.
    pub uv1: [f32; 2],
    /// Skinning: up to 4 skeleton joint indices influencing this vertex. All
    /// zero on static meshes (the skinned pipeline variant is only used when the
    /// mesh has a skeleton, so static meshes ignore these). Appended last to keep
    /// existing attribute offsets stable.
    pub joints: [u32; 4],
    /// Skinning: the 4 joint weights (normalized). All zero on static meshes.
    pub weights: [f32; 4],
}

impl Default for Vertex {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0; 2],
            color: [1.0; 4],
            tangent: [1.0, 0.0, 0.0, 1.0],
            uv1: [0.0; 2],
            joints: [0; 4],
            weights: [0.0; 4],
        }
    }
}

pub struct MeshData {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// True when `uv1` is a real, non-overlapping lightmap UV set — the model's
    /// own TEXCOORD_1, a primitive's generated atlas, or a generated unwrap.
    /// False when `uv1` is just a copy of `uv0` (no dedicated lightmap UV), which
    /// must NOT be baked (overlapping charts → garbage lightmap). Gates the bake
    /// (offer to generate) and the UV-checker preview. `#[serde(default)]`-style
    /// default is `false` so older callers stay conservative.
    pub has_lightmap_uv: bool,
}

/// Pixel data ready for upload. RGBA8 by default; when `hdr` is set, `pixels`
/// holds RGBA **f16** (8 bytes/texel) and uploads as R16G16B16A16_SFLOAT — the
/// native HDR-float path for EXR/HDR sources (no LDR clamp, linear).
pub struct TextureData {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// True for color data (albedo, emission), false for data maps
    /// (normal, ORM). Ignored when `hdr` (float is always linear).
    pub srgb: bool,
    /// `pixels` is RGBA f16 linear (R16G16B16A16_SFLOAT) rather than RGBA8.
    pub hdr: bool,
}

/// Pixel format of a (possibly block-compressed) prepared texture. BC variants
/// carry one byte per texel (4x4 blocks, 16 bytes each); raw variants are the
/// fallback for non-multiple-of-4 dimensions BC can't tile cleanly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TexFormat {
    /// BC7 sRGB — color maps (albedo, emission).
    Bc7Srgb,
    /// BC7 UNORM — linear data maps (normal, ORM, roughness, AO, metal).
    Bc7Unorm,
    /// BC6H unsigned float — HDR sources (EXR).
    Bc6h,
    /// Uncompressed RGBA8 sRGB.
    RgbaSrgb,
    /// Uncompressed RGBA8 linear.
    RgbaUnorm,
    /// Uncompressed RGBA f16 linear (8 bytes/texel).
    RgbaF16,
}

/// A texture decoded + (optionally) block-compressed off the main thread, with a
/// full mip chain, ready to upload with no further CPU work. Produced by the
/// asset import cache; consumed by `GpuTexture::upload_compressed`.
pub struct CompressedTexture {
    pub format: TexFormat,
    pub width: u32,
    pub height: u32,
    /// Mip levels, largest first; each is the exact byte payload for that level.
    pub mips: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AlphaMode {
    #[default]
    Opaque,
    Cutout,
    Blend,
}

/// Feature toggles: each combination selects a shader/pipeline variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialFeatures {
    pub toon: bool,
    pub normal_map: bool,
    pub emission: bool,
    pub alpha_mode: AlphaMode,
    pub double_sided: bool,
}

/// Continuously editable material parameters; delivered to the shader as
/// push constants every draw, so edits are live with zero sync hazards.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterialParams {
    pub base_color: [f32; 4],
    pub emission_color: [f32; 3],
    pub emission_intensity: f32,
    /// Mean LINEAR RGB of the emission map (1,1,1 = no map). Scales the per-
    /// instance emission fed to the RT reflection + RT GI gather so a mostly-black
    /// map (e.g. only a glowing visor) doesn't make the whole mesh reflect/bounce
    /// as a uniform light. The per-pixel forward emission is unaffected (it
    /// samples the actual map).
    pub emission_map_mean: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
    pub toon_steps: f32,
    pub pbr_toon_blend: f32,
    pub alpha_cutoff: f32,
    pub normal_strength: f32,
    pub occlusion_strength: f32,
    // --- extended "FX" params, delivered via a per-material UBO (set 1, binding
    // 4) so the built-in Standard/Toon shaders can have options beyond the
    // full 128-byte push block. ---
    /// Toon rim-light colour.
    pub rim_color: [f32; 3],
    /// Toon rim Fresnel exponent (higher = tighter edge).
    pub rim_power: f32,
    /// Toon rim strength (0 = off).
    pub rim_strength: f32,
    /// Toon cel-ramp edge softness.
    pub ramp_smoothness: f32,
    /// Animated UV scroll for the base maps (uv units / second).
    pub base_scroll: [f32; 2],
    /// Animated UV scroll for the emission map.
    pub emission_scroll: [f32; 2],
    /// Emission pulse speed (0 = steady); brightness oscillates.
    pub emission_pulse: f32,
    /// Additive matcap strengths (3 layers; 0 = off). Each layer samples its own
    /// matcap texture, multiplied by its mask.
    pub matcap_strength: [f32; 3],
    /// Per-layer matcap blend mode encoded as a float: 0 = Add, 1 = Multiply,
    /// 2 = Replace.
    pub matcap_blend: [f32; 3],
    // --- Per-texture UV transform for the main maps. `*_tiling` scales the UVs
    // (Unity tiling; default 1), `*_offset` shifts them (default 0). Mask slots
    // follow their parent map (opacity→albedo, emission_mask→emission). ---
    pub albedo_tiling: [f32; 2],
    pub albedo_offset: [f32; 2],
    pub normal_tiling: [f32; 2],
    pub normal_offset: [f32; 2],
    pub orm_tiling: [f32; 2],
    pub orm_offset: [f32; 2],
    pub emission_tiling: [f32; 2],
    pub emission_offset: [f32; 2],
    // Invert a split AO / Roughness / Metallic map (e.g. a smoothness map used in
    // the roughness slot). Only meaningful when the matching map is assigned.
    pub ao_invert: bool,
    pub roughness_invert: bool,
    pub metallic_invert: bool,
    /// Parallax occlusion mapping strength (uv shift at full height). 0 = off.
    pub displacement_scale: f32,
    /// Per-material reflection strength (1 = full, 0 = matte). Scales the env-cube
    /// probe reflection AND the screen-space/RT reflection for this material.
    pub reflection_intensity: f32,
    /// Per-material screen-space/RT reflection toggle (true = on). Off keeps the
    /// env-cube reflection but skips the deferred SSR/RT resolve for this material.
    pub screen_reflections: bool,
}

impl Default for MaterialParams {
    fn default() -> Self {
        Self {
            base_color: [1.0; 4],
            emission_color: [0.0; 3],
            emission_intensity: 1.0,
            emission_map_mean: [1.0; 3],
            metallic: 0.0,
            roughness: 0.7,
            toon_steps: 3.0,
            pbr_toon_blend: 1.0,
            alpha_cutoff: 0.5,
            normal_strength: 1.0,
            occlusion_strength: 1.0,
            rim_color: [1.0, 1.0, 1.0],
            rim_power: 4.0,
            rim_strength: 0.0,
            ramp_smoothness: 0.12,
            base_scroll: [0.0, 0.0],
            emission_scroll: [0.0, 0.0],
            emission_pulse: 0.0,
            matcap_strength: [0.0, 0.0, 0.0],
            matcap_blend: [0.0, 0.0, 0.0],
            ao_invert: false,
            roughness_invert: false,
            metallic_invert: false,
            displacement_scale: 0.0,
            reflection_intensity: 1.0,
            screen_reflections: true,
            albedo_tiling: [1.0, 1.0],
            albedo_offset: [0.0, 0.0],
            normal_tiling: [1.0, 1.0],
            normal_offset: [0.0, 0.0],
            orm_tiling: [1.0, 1.0],
            orm_offset: [0.0, 0.0],
            emission_tiling: [1.0, 1.0],
            emission_offset: [0.0, 0.0],
        }
    }
}

/// Per-material "FX" uniform block (std140); the GPU layout of the extended
/// [`MaterialParams`] fields. Five vec4s = 80 bytes.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MaterialFx {
    /// rgb rim colour, w rim power.
    pub rim: [f32; 4],
    /// x rim strength, y ramp smoothness, z emission pulse, w unused.
    pub toon: [f32; 4],
    /// xy base UV scroll, zw emission UV scroll.
    pub scroll: [f32; 4],
    /// xyz matcap layer strengths, w unused.
    pub matcap: [f32; 4],
    /// xyz matcap layer blend modes (0 Add / 1 Multiply / 2 Replace), w unused.
    pub matcap_blend: [f32; 4],
    /// Per-map UV transform: xy = tiling (scale), zw = offset.
    pub albedo_st: [f32; 4],
    pub normal_st: [f32; 4],
    pub orm_st: [f32; 4],
    pub emission_st: [f32; 4],
    /// Invert a split AO/Roughness/Metallic map: x = ao, y = roughness,
    /// z = metallic (0 = as-is, 1 = 1 - value). w unused.
    pub orm_invert: [f32; 4],
    /// Parallax occlusion mapping: x = displacement scale (0 = off). yzw unused.
    pub parallax: [f32; 4],
}

impl MaterialFx {
    pub fn from_params(p: &MaterialParams) -> Self {
        Self {
            rim: [p.rim_color[0], p.rim_color[1], p.rim_color[2], p.rim_power],
            toon: [p.rim_strength, p.ramp_smoothness, p.emission_pulse, 0.0],
            scroll: [
                p.base_scroll[0],
                p.base_scroll[1],
                p.emission_scroll[0],
                p.emission_scroll[1],
            ],
            matcap: [
                p.matcap_strength[0],
                p.matcap_strength[1],
                p.matcap_strength[2],
                0.0,
            ],
            matcap_blend: [
                p.matcap_blend[0],
                p.matcap_blend[1],
                p.matcap_blend[2],
                0.0,
            ],
            albedo_st: [
                p.albedo_tiling[0],
                p.albedo_tiling[1],
                p.albedo_offset[0],
                p.albedo_offset[1],
            ],
            normal_st: [
                p.normal_tiling[0],
                p.normal_tiling[1],
                p.normal_offset[0],
                p.normal_offset[1],
            ],
            orm_st: [
                p.orm_tiling[0],
                p.orm_tiling[1],
                p.orm_offset[0],
                p.orm_offset[1],
            ],
            emission_st: [
                p.emission_tiling[0],
                p.emission_tiling[1],
                p.emission_offset[0],
                p.emission_offset[1],
            ],
            orm_invert: [
                p.ao_invert as u32 as f32,
                p.roughness_invert as u32 as f32,
                p.metallic_invert as u32 as f32,
                0.0,
            ],
            // y = per-material reflection strength (scales env-cube + SSR/RT).
            // z = screen-space/RT reflection toggle (1 = on, 0 = cube-only). w free.
            parallax: [
                p.displacement_scale,
                p.reflection_intensity,
                if p.screen_reflections { 1.0 } else { 0.0 },
                0.0,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeshHandle(pub(crate) usize);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureHandle(pub(crate) usize);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaterialHandle(pub(crate) usize);
/// A registered custom fragment shader (compiled SPIR-V).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShaderId(pub(crate) usize);

#[derive(Default)]
pub struct MaterialDesc {
    pub name: String,
    pub params: MaterialParams,
    pub features: MaterialFeatures,
    pub albedo: Option<TextureHandle>,
    pub normal: Option<TextureHandle>,
    pub orm: Option<TextureHandle>,
    pub emission: Option<TextureHandle>,
    // Extended texture slots (bindings 5-12).
    pub opacity: Option<TextureHandle>,
    pub emission_mask: Option<TextureHandle>,
    pub matcap: [Option<TextureHandle>; 3],
    pub matcap_mask: [Option<TextureHandle>; 3],
    /// Split AO / Roughness / Metallic maps (bindings 13-15). When assigned each
    /// multiplies the corresponding packed-ORM channel (default white = no-op),
    /// so a material can use a packed ORM, separate maps, or a mix.
    pub ao: Option<TextureHandle>,
    pub roughness: Option<TextureHandle>,
    pub metallic: Option<TextureHandle>,
    /// Height / displacement map (binding 16) for parallax occlusion mapping.
    pub displacement: Option<TextureHandle>,
    /// Render with the error swirl shader (broken/missing material).
    pub error: bool,
}

pub struct CameraData {
    pub view: Mat4,
    pub proj: Mat4,
    pub position: Vec3,
}

/// Scene-wide lighting that isn't tied to a single light object: the ambient
/// fill and the "key" directional light exposed to custom shaders through the
/// `u_light_dir` / `u_light_color` macros. The full per-object light set is
/// passed separately as [`LightInstance`]s.
pub struct LightData {
    /// Direction the key light travels (from the light toward the scene).
    pub direction: Vec3,
    pub color: [f32; 3],
    pub intensity: f32,
    pub ambient: [f32; 3],
    /// Environment / skylight intensity that scales the fallback skybox IBL
    /// (the specular reflection of the environment cubemap on metallic/smooth
    /// surfaces). 0 = the skybox does NOT light the scene, so with no analytic
    /// lights and no ambient the scene is black instead of showing skybox
    /// reflections on metals. Explicit reflection probes set their own intensity
    /// and are unaffected. Defaults to 1 (full skybox IBL).
    pub env_intensity: f32,
}

impl Default for LightData {
    fn default() -> Self {
        Self {
            direction: Vec3::new(-0.4, -1.0, -0.3),
            color: [1.0, 0.98, 0.92],
            intensity: 3.0,
            ambient: [0.13, 0.14, 0.18],
            env_intensity: 1.0,
        }
    }
}

/// What shape of light a [`LightInstance`] emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightKind {
    Directional,
    Point,
    Spot,
}

/// One light gathered from the scene for a frame. Directional lights use only
/// `direction`; point lights only `position`; spot lights both.
#[derive(Clone, Copy, Debug)]
pub struct LightInstance {
    pub kind: LightKind,
    /// World-space position (point/spot).
    pub position: Vec3,
    /// World-space travel direction, normalized (directional/spot).
    pub direction: Vec3,
    /// Linear RGB color (not yet scaled by intensity).
    pub color: [f32; 3],
    pub intensity: f32,
    /// Distance at which point/spot reach zero.
    pub range: f32,
    /// Spot inner cone full angle (degrees); full brightness inside it.
    pub spot_inner_deg: f32,
    /// Spot outer cone full angle (degrees); zero brightness outside it.
    pub spot_outer_deg: f32,
    /// Render shadows for this light (depth map from its POV).
    pub cast_shadows: bool,
    /// Soft (PCF kernel) vs hard (single tap) shadow edge. Ignored when
    /// `cast_shadows` is false.
    pub soft_shadows: bool,
    /// Light-clip-space depth-compare bias.
    pub shadow_bias: f32,
    /// This light's contribution is BAKED into lightmaps (Baked/Mixed mode + a
    /// bake exists). It's kept in the realtime pass so **non-lightmapped** objects
    /// (dynamic objects, anything added after the bake) still receive it; the
    /// shader skips it for lightmapped objects, which already have it baked in. So
    /// a baked scene's dynamic objects aren't black. `false` = realtime (all objs).
    pub baked: bool,
}

pub struct DrawCmd {
    pub mesh: MeshHandle,
    pub material: MaterialHandle,
    pub transform: Mat4,
    /// Editor selection glow, 0.0 = off. Per-draw, not part of the material.
    pub highlight: f32,
    /// Mesh AABB center in object space; the outline pass inflates radially
    /// from it so hard-edged meshes stay watertight (no corner gaps).
    pub mesh_center: Vec3,
    /// Object-space bounding-sphere radius (AABB half-diagonal) around
    /// `mesh_center`, scaled by the transform to frustum-cull the main camera
    /// pass. 0 disables culling for this draw (always drawn).
    pub bound_radius: f32,
    /// Baked-lightmap array layer for this object's static GI, or -1 when the
    /// object has no lightmap (sample probes / flat ambient instead).
    pub lightmap_layer: i32,
    /// This object's lightmap resolution (texels/side) for the UV-checker
    /// preview — its native baked size, or the would-be size from bake settings
    /// for an un-baked static object. 0 = not lightmapped.
    pub lightmap_size: u32,
}

pub struct EguiDraw {
    pub pixels_per_point: f32,
    pub primitives: Vec<egui::ClippedPrimitive>,
    pub textures_delta: egui::TexturesDelta,
}

// ---------------------------------------------------------------- baking

/// One static mesh instance fed to the lighting bake. The bake rasters this
/// object's lightmap in `uv1` space and stores incoming light per texel.
#[derive(Clone, Copy, Debug)]
pub struct BakeInstance {
    pub mesh: MeshHandle,
    pub transform: Mat4,
    /// Square lightmap resolution for this object (texels per side).
    pub lightmap_size: u32,
    /// Diffuse albedo for indirect bounces (material base color, linear).
    pub albedo: [f32; 3],
    /// Emitted radiance (color × intensity, linear); surfaces glow into GI.
    pub emission: [f32; 3],
    /// Metalness [0,1]. The diffuse bounce uses albedo·(1−metallic): a metal has
    /// no diffuse albedo (its energy is specular/F0), so it absorbs the diffuse
    /// GI bounce instead of re-radiating it — what stops light pooling/growing in
    /// metal-on-metal contact cavities.
    pub metallic: f32,
    /// Roughness [0,1]. Carried for the surface cache / specular GI; the diffuse
    /// bounce magnitude is roughness-independent (Lambert), so it only modulates
    /// the (future) specular trace, not the diffuse albedo.
    pub roughness: f32,
}

/// A light contributing to the bake. Baked lights contribute their full
/// direct term; Mixed lights contribute only the indirect/shadow that the
/// realtime path can't do (the engine decides which to include).
#[derive(Clone, Copy, Debug)]
pub struct BakeLight {
    pub kind: LightKind,
    pub position: Vec3,
    pub direction: Vec3,
    /// Linear RGB × intensity (already scaled).
    pub color: [f32; 3],
    pub range: f32,
    pub spot_inner_deg: f32,
    pub spot_outer_deg: f32,
    /// Light source radius (world units) for soft shadows; 0 = hard.
    pub radius: f32,
}

/// Everything the GPU bake needs in one renderer-agnostic bundle.
pub struct BakeInput<'a> {
    pub instances: &'a [BakeInstance],
    pub lights: &'a [BakeLight],
    /// World-space probe centers (flattened across all volumes).
    pub probes: &'a [Vec3],
    /// Constant sky/ambient radiance for rays that escape the scene.
    pub sky_color: [f32; 3],
    /// Indirect bounces per path (0 = direct + sky only).
    pub bounces: u32,
    /// Paths traced per lightmap texel / per probe direction.
    pub samples: u32,
    /// Skip per-instance lightmap tracing and bake only the probes (the
    /// instances still act as occluders/bouncers). Used by the realtime-GI
    /// preview, which only needs probe SH each update.
    pub probes_only: bool,
    /// Fraction of each GPU bake submission's duration to then idle the GPU, so the
    /// desktop compositor gets a share instead of the bake hogging the GPU (which
    /// froze the machine). 0 = no throttle (the realtime-GI preview uses 0 so it
    /// stays responsive); the offline bake passes the user's `gpu_throttle`.
    pub gpu_idle_frac: f32,
}

/// One baked lightmap: `size`×`size` RGBA32F (rgb = irradiance, a = validity).
#[derive(Clone, Debug)]
pub struct BakedLightmap {
    pub size: u32,
    pub pixels: Vec<f32>,
}

/// SH-L1 irradiance for one probe: 4 coefficients × RGB, plus an SH-L1 of the
/// directional distance-to-geometry (4 scalar coefficients) used for DDGI-style
/// visibility weighting at sample time. `dist` left zero (the bake path) disables
/// the visibility test; the standard shader then weights probes by trilinear
/// only. The software march fills it so probes occluded from a fragment (e.g.
/// behind a wall) are down-weighted, killing light leaks.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeSh {
    pub coeffs: [[f32; 3]; 4],
    pub dist: [f32; 4],
    /// SH-L1 of the SQUARED first-hit distance (second moment), for a two-moment
    /// Chebyshev visibility test (DDGI-style) — smooth occlusion instead of a
    /// hard threshold. Zero = no data (the test falls back to single-moment).
    pub dist2: [f32; 4],
}

/// Result of a bake: a lightmap per instance (same order) and SH per probe.
#[derive(Clone, Debug, Default)]
pub struct BakeOutput {
    pub lightmaps: Vec<BakedLightmap>,
    pub probes: Vec<ProbeSh>,
}

/// Per-frame render statistics (last completed frame). Categories grow as
/// render passes are added (reflections, probes, shadows…).
#[derive(Clone, Copy, Debug, Default)]
pub struct RenderStats {
    /// Total scene draw calls this frame (excluding egui).
    pub draw_calls: u32,
    /// Opaque + cutout draws.
    pub opaque_draws: u32,
    /// Extra draws caused by transparency (alpha-blended, sorted pass).
    pub transparent_draws: u32,
    /// Extra draws caused by the selection outline pass.
    pub outline_draws: u32,
    /// Draws using the error/missing-material swirl shader.
    pub error_draws: u32,
    /// Pipeline binds (state switches) this frame.
    pub pipeline_binds: u32,
    /// Distinct materials referenced by this frame's draws.
    pub materials_drawn: u32,
    /// Compiled shader-variant pipelines in the cache.
    pub pipeline_variants: u32,
    /// Total GPU time for the last completed frame (ms), from timestamp queries.
    /// 0 when the device/queue does not support timestamps.
    pub gpu_frame_ms: f32,
    /// GPU time spent in the Flux GI trace (depth prepass + gather) last frame
    /// (ms). 0 on frames where the trace was skipped (converged still camera).
    pub gpu_gi_ms: f32,
    /// GPU per-pass breakdown (ms) for the active path, from timestamp zones.
    /// A pass that didn't run this frame reads 0.
    pub gpu_shadows_ms: f32,
    pub gpu_scene_ms: f32,
    pub gpu_reflect_ms: f32,
    pub gpu_post_ms: f32,
    pub gpu_egui_ms: f32,
    pub gpu_cam_preview_ms: f32,
}

pub struct FrameInput<'a> {
    pub clear_color: [f32; 4],
    pub camera: CameraData,
    /// Ambient + key-light fallback (the latter feeds custom-shader macros).
    pub light: LightData,
    /// Every light gathered from the scene this frame. When empty, the
    /// standard shader falls back to the single key directional in `light`.
    pub lights: &'a [LightInstance],
    /// Lights for the reflection-probe cube capture — the FULL set including
    /// Baked/Mixed lights that `lights` drops once a bake exists. The cube
    /// renders the scene without lightmaps, so it needs the analytic lights or
    /// it captures dark. Empty → fall back to `lights`.
    pub capture_lights: &'a [LightInstance],
    /// When set, the scene is also rendered from this camera into the offscreen
    /// preview target (the editor's Camera tab). `None` skips that pass.
    pub camera_preview: Option<CameraData>,
    /// Draw the skybox behind the scene. When false the clear color shows.
    pub draw_skybox: bool,
    /// PCF tap spacing in shadow-UV units (softness / shadow_resolution).
    pub shadow_pcf_texel: f32,
    /// Directional shadow coverage in world units (ortho box fit ahead of the
    /// camera). Smaller = sharper.
    pub shadow_distance: f32,
    /// Seconds since app start; drives animated shader effects.
    pub time: f32,
    pub draws: &'a [DrawCmd],
    /// Debug: render objects as a lightmap-UV checkerboard (cell size tracks
    /// each object's lightmap resolution) instead of their material.
    pub lightmap_preview: bool,
    /// GI debug view: 0 = off, 1 = world normals, 2 = indirect/GI term only.
    /// Lets you isolate the probe-grid blockiness on screen.
    pub gi_debug: u32,
    /// Whether the editor viewport is visible this frame. When false (e.g. only
    /// the Camera tab is shown) the main scene draws + the viewport Flux trace
    /// are skipped to save GPU; the swapchain is still cleared and egui drawn.
    /// Always true for the game runtime.
    pub render_viewport: bool,
    /// Editor only: when set, render the viewport 3D into an offscreen texture of
    /// this pixel size (the viewport dock rect) instead of the full swapchain, so
    /// it isn't rasterized under the dock panels. `None` for the game runtime,
    /// which renders straight to the swapchain.
    pub viewport_extent: Option<[u32; 2]>,
    /// Resolved post-processing parameters (from the camera's blended Volume
    /// profiles). Per-pixel effects applied in the surface shaders.
    pub postfx: PostFx,
    /// Active reflection-probe zone (the placed `ReflectionProbe` whose box
    /// contains the camera, or the nearest). Drives box-projected parallax +
    /// intensity for the environment reflection. `None` = treat the env cube as
    /// distant/infinite (no parallax, intensity 1).
    pub reflection_probe: Option<ReflectionProbeBox>,
    /// Distance + height exponential fog (atmospheric depth). `None` = no fog.
    pub fog: Option<FogParams>,
    /// FluxVoxel specular-from-volume: metallic/rough surfaces sample the probe
    /// volume in the reflection direction for emissive/voxel-light bounce (VXGI-style
    /// glossy approximation). Off = reflection cube only.
    pub voxel_specular: bool,
    pub egui: Option<EguiDraw>,
}

/// Exponential distance + height fog parameters.
#[derive(Clone, Copy, Debug)]
pub struct FogParams {
    pub color: [f32; 3],
    /// Fog buildup per world unit of view distance.
    pub density: f32,
    /// Height falloff: fog thins above `height_ref` at this rate (0 = uniform).
    pub height_falloff: f32,
    /// World Y where fog is at full density.
    pub height_ref: f32,
    /// View distance (world units) before fog starts accumulating.
    pub start_distance: f32,
}

/// A placed reflection-probe zone in world space, for box-projected reflections.
#[derive(Clone, Copy, Debug)]
pub struct ReflectionProbeBox {
    pub center: [f32; 3],
    pub half_extents: [f32; 3],
    pub intensity: f32,
    /// Box-projected parallax (Unity-style) vs. infinite/distant sampling.
    pub box_projection: bool,
    /// Captured cubemap face resolution for this probe.
    pub resolution: u32,
}

/// Resolved per-frame post-processing parameters (the blended profile flattened
/// for the GPU). Per-pixel effects (exposure, tonemap, color grading, vignette)
/// are applied in the surface shaders; chromatic aberration + bloom need a
/// a fullscreen pass (follow-up).
#[derive(Clone, Copy, Debug)]
pub struct PostFx {
    /// 0 = none, 1 = Reinhard, 2 = ACES.
    pub tonemap: u32,
    /// Tonemap exposure in stops (EV); color ×= 2^exposure before the operator.
    pub exposure: f32,
    pub grading_enabled: bool,
    pub grade_exposure: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub temperature: f32,
    pub tint: f32,
    pub vignette_enabled: bool,
    pub vignette_intensity: f32,
    pub vignette_smoothness: f32,
    pub vignette_color: [f32; 3],
    pub bloom_enabled: bool,
    pub bloom_threshold: f32,
    pub bloom_intensity: f32,
    pub bloom_radius: f32,
    pub ca_enabled: bool,
    pub ca_intensity: f32,
}

impl Default for PostFx {
    fn default() -> Self {
        Self {
            tonemap: 2, // ACES, matches the previous hardcoded behavior
            exposure: 0.0,
            grading_enabled: false,
            grade_exposure: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            temperature: 0.0,
            tint: 0.0,
            vignette_enabled: false,
            vignette_intensity: 0.4,
            vignette_smoothness: 0.4,
            vignette_color: [0.0, 0.0, 0.0],
            bloom_enabled: false,
            bloom_threshold: 1.0,
            bloom_intensity: 0.5,
            bloom_radius: 0.5,
            ca_enabled: false,
            ca_intensity: 0.3,
        }
    }
}
