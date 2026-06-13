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
        }
    }
}

pub struct MeshData {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

/// RGBA8 pixel data ready for upload.
pub struct TextureData {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// True for color data (albedo, emission), false for data maps
    /// (normal, ORM).
    pub srgb: bool,
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
    pub metallic: f32,
    pub roughness: f32,
    pub toon_steps: f32,
    pub pbr_toon_blend: f32,
    pub alpha_cutoff: f32,
    pub normal_strength: f32,
    pub occlusion_strength: f32,
}

impl Default for MaterialParams {
    fn default() -> Self {
        Self {
            base_color: [1.0; 4],
            emission_color: [0.0; 3],
            emission_intensity: 1.0,
            metallic: 0.0,
            roughness: 0.7,
            toon_steps: 3.0,
            pbr_toon_blend: 1.0,
            alpha_cutoff: 0.5,
            normal_strength: 1.0,
            occlusion_strength: 1.0,
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

pub struct MaterialDesc {
    pub name: String,
    pub params: MaterialParams,
    pub features: MaterialFeatures,
    pub albedo: Option<TextureHandle>,
    pub normal: Option<TextureHandle>,
    pub orm: Option<TextureHandle>,
    pub emission: Option<TextureHandle>,
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
}

impl Default for LightData {
    fn default() -> Self {
        Self {
            direction: Vec3::new(-0.4, -1.0, -0.3),
            color: [1.0, 0.98, 0.92],
            intensity: 3.0,
            ambient: [0.13, 0.14, 0.18],
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
    /// Spot inner cone full angle (degrees) — full brightness inside it.
    pub spot_inner_deg: f32,
    /// Spot outer cone full angle (degrees) — zero brightness outside it.
    pub spot_outer_deg: f32,
    /// Render shadows for this light (depth map from its POV).
    pub cast_shadows: bool,
    /// Light-clip-space depth-compare bias.
    pub shadow_bias: f32,
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
}

pub struct EguiDraw {
    pub pixels_per_point: f32,
    pub primitives: Vec<egui::ClippedPrimitive>,
    pub textures_delta: egui::TexturesDelta,
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
}

pub struct FrameInput<'a> {
    pub clear_color: [f32; 4],
    pub camera: CameraData,
    /// Ambient + key-light fallback (the latter feeds custom-shader macros).
    pub light: LightData,
    /// Every light gathered from the scene this frame. When empty, the
    /// standard shader falls back to the single key directional in `light`.
    pub lights: &'a [LightInstance],
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
    pub egui: Option<EguiDraw>,
}
