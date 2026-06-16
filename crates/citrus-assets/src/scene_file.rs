//! `.scene` asset files (RON): a list of placed objects with sources and
//! material references, so multiple scenes can be saved and reloaded.

use std::path::Path;

use anyhow::{Context as _, Result};
use citrus_render::{MaterialFeatures, MaterialParams};
use serde::{Deserialize, Serialize};

use crate::material_file::MaterialTextures;

pub const SCENE_EXTENSION: &str = "scene";

/// Where an object's mesh came from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ObjectSource {
    /// A model file in the project; `mesh` is the flattened primitive index
    /// (slot 0) produced by the importer (stable for an unchanged file).
    /// `extra_meshes` holds the flattened indices of any additional material
    /// slots on the same object (empty for single-material meshes).
    Model {
        path: String,
        mesh: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extra_meshes: Vec<usize>,
    },
    /// A built-in test-scene mesh (sphere/cube/plane by index).
    Builtin { mesh: usize },
    /// A generated primitive shape.
    Primitive { shape: PrimitiveShape },
    /// A transform-only grouping node.
    Empty,
    /// A camera placement.
    Camera,
    /// A light placement. The light's kind and settings live in its
    /// `Light` component; this only marks the object as a light (icon, default
    /// component, gathering).
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrimitiveShape {
    Cube,
    Sphere,
    Capsule,
    Plane,
}

impl PrimitiveShape {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cube => "Cube",
            Self::Sphere => "Sphere",
            Self::Capsule => "Capsule",
            Self::Plane => "Plane",
        }
    }
}

fn default_shader() -> String {
    "standard".into()
}

/// Which material an object uses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MaterialRef {
    /// A `.material` asset file (project-relative path).
    File(String),
    /// Snapshot of parameters applied over whatever textures the object's
    /// imported material carried.
    Inline {
        params: MaterialParams,
        features: MaterialFeatures,
        /// "standard" or a project-relative `.frag` path.
        #[serde(default = "default_shader")]
        shader: String,
        /// Custom-shader property values by name.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        custom: std::collections::BTreeMap<String, Vec<f32>>,
        /// Draw-order priority (Unity render queue); None = derive from alpha.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        render_queue: Option<i32>,
        /// User-assigned texture slots (project-relative). Empty = none / use
        /// the object's import-embedded textures.
        #[serde(default, skip_serializing_if = "MaterialTextures::is_empty")]
        textures: MaterialTextures,
    },
}

/// One serialized component: registry name + its RON-encoded state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentData {
    pub kind: String,
    pub data: String,
}

fn default_true() -> bool {
    true
}

fn default_lightmap_scale() -> f32 {
    1.0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SceneEntry {
    /// Stable object id (UUID string). Empty in legacy scenes; the engine
    /// assigns a fresh one on load.
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub source: ObjectSource,
    /// Whether the object renders / its light contributes. Disabled objects
    /// stay in the scene but are skipped at draw time.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Marks the object as non-moving so the lighting bake includes it as a
    /// lightmapped surface + ray-trace occluder ("Contribute GI" in the editor).
    #[serde(default)]
    pub static_geometry: bool,
    /// Per-object lightmap-resolution multiplier (Unity's "Scale In Lightmap").
    /// 1.0 = the scene texel density; raise for a sharper surface, lower to save
    /// texels.
    #[serde(default = "default_lightmap_scale")]
    pub lightmap_scale: f32,
    pub material: MaterialRef,
    /// Materials for additional slots beyond the first (parallel to
    /// `ObjectSource::Model::extra_meshes`). Empty for single-material objects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_materials: Vec<MaterialRef>,
    /// Index of the parent entry in this file, if any. Transforms are local
    /// to the parent.
    #[serde(default)]
    pub parent: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<ComponentData>,
    pub translation: [f32; 3],
    /// Quaternion xyzw.
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

/// Scene-level environment: ambient fill + a world "sun/moon" directional
/// light + skybox toggle. Configured in the editor's Environment window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldEnvironment {
    /// Linear ambient fill color.
    pub ambient: [f32; 3],
    /// Ambient multiplier.
    pub ambient_intensity: f32,
    /// Whether the world sun contributes.
    pub sun_enabled: bool,
    pub sun_color: [f32; 3],
    pub sun_intensity: f32,
    /// World-space travel direction of the sun (will be normalized).
    pub sun_direction: [f32; 3],
    /// Draw the skybox behind the scene (off = clear-color/black background).
    pub skybox_enabled: bool,
    /// Shadow-map resolution per layer (pixels). Common values 512–4096.
    #[serde(default = "default_shadow_resolution")]
    pub shadow_resolution: u32,
    /// PCF kernel softness multiplier (1.0 = one texel spacing).
    #[serde(default = "default_shadow_softness")]
    pub shadow_softness: f32,
    /// Directional shadow coverage (world units). The ortho box is fit to
    /// this size ahead of the camera. Smaller = sharper, less coverage.
    #[serde(default = "default_shadow_distance")]
    pub shadow_distance: f32,
    /// Lighting-bake settings (Bakery-style: texels-per-meter density).
    #[serde(default)]
    pub bake: BakeSettings,
    /// Realtime-GI settings: when enabled (and no bake exists) the engine
    /// continuously re-traces light probes from the realtime lights so surfaces
    /// show live indirect bounce, both in the editor and in a shipped game.
    #[serde(default, deserialize_with = "de_realtime_gi")]
    pub realtime_gi: RealtimeGi,
    /// Always-applied global post-processing (the implicit "global volume").
    /// Local `VolumeComponent`s blend on top of this by priority/weight. Defaults
    /// to ACES tonemap so a fresh scene is tonemapped without needing a volume.
    #[serde(default)]
    pub postfx: crate::PostFxProfile,
    /// Exponential distance + height fog (atmospheric depth).
    #[serde(default)]
    pub fog_enabled: bool,
    #[serde(default = "default_fog_color")]
    pub fog_color: [f32; 3],
    #[serde(default = "default_fog_density")]
    pub fog_density: f32,
    /// Height falloff (fog thins above `fog_height_ref`; 0 = uniform).
    #[serde(default)]
    pub fog_height_falloff: f32,
    #[serde(default)]
    pub fog_height_ref: f32,
    /// View distance before fog starts.
    #[serde(default)]
    pub fog_start_distance: f32,
    /// Cubemap skybox: 6 project-relative face image paths in +X,-X,+Y,-Y,+Z,-Z
    /// order. Takes precedence over the equirect skybox when set.
    #[serde(default)]
    pub skybox_faces: Option<[String; 6]>,
}

fn default_fog_color() -> [f32; 3] {
    [0.6, 0.68, 0.78]
}
fn default_fog_density() -> f32 {
    0.02
}

/// Accept both the legacy `realtime_gi: bool` (just the enable toggle) and the
/// current struct form, so scenes saved before the settings landed still load.
/// Uses a Visitor instead of `#[serde(untagged)]` so the struct path delegates
/// to the typed `RealtimeGi` deserialize. Untagged would buffer into a
/// self-describing `Content`, which can't replay RON enum variants (the `mode`
/// field), breaking the valid struct form.
fn de_realtime_gi<'de, D>(d: D) -> Result<RealtimeGi, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = RealtimeGi;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a bool (legacy) or a RealtimeGi struct")
        }
        fn visit_bool<E: serde::de::Error>(self, enabled: bool) -> Result<RealtimeGi, E> {
            Ok(RealtimeGi {
                enabled,
                ..Default::default()
            })
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            map: A,
        ) -> Result<RealtimeGi, A::Error> {
            RealtimeGi::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    d.deserialize_any(V)
}

/// How the realtime-GI probe trace is computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum GiMode {
    /// FluxRT: hardware ray-query (RT cores) against the scene BVH. Most accurate.
    /// (Legacy scenes named this `Hardware`/`RayQuery`.)
    #[default]
    #[serde(alias = "Hardware", alias = "RayQuery")]
    FluxRT,
    /// Flux: software ray-marching of per-mesh signed distance fields (no RT
    /// cores, runs anywhere). (Legacy scenes named this `Software`.)
    #[serde(alias = "Software")]
    Flux,
    /// FluxVoxel: analytic voxel light volume (no ray tracing). Injects lights +
    /// emissive into an SH-L1 probe grid every frame — cheapest backend, built
    /// for VR. Static + dynamic meshes read it; dynamic lights mix in live.
    /// (Older scenes named this `FluxVR`.)
    #[serde(alias = "FluxVR")]
    FluxVoxel,
}

impl GiMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::FluxRT => "FluxRT (hardware)",
            Self::Flux => "Flux (software)",
            Self::FluxVoxel => "FluxVoxel",
        }
    }
    pub const ALL: [GiMode; 3] = [Self::FluxRT, Self::Flux, Self::FluxVoxel];
}

/// Flux quality preset. Drives per-frame samples-per-probe and the march step
/// cap; the temporal accumulator does the rest, so even Performance stays smooth
/// once settled. Probe density (screen-probe spacing) is fixed for now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum FluxQuality {
    /// Fewest rays: cheapest, slightly noisier while moving.
    Performance,
    #[default]
    Balanced,
    High,
    /// Most rays: sharpest contact GI, highest cost.
    Ultra,
}

impl FluxQuality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Performance => "Performance",
            Self::Balanced => "Balanced",
            Self::High => "High",
            Self::Ultra => "Ultra",
        }
    }
    pub const ALL: [FluxQuality; 4] =
        [Self::Performance, Self::Balanced, Self::High, Self::Ultra];
    /// Rays per screen probe per frame (temporal accumulation does the rest).
    /// One probe serves SCREEN_PROBE_DIV² pixels (16 at DIV=4), so the per-pixel
    /// cost is amortized — match ~64 rays/probe at the top preset rather
    /// than the old per-pixel-era counts (which badly undersampled the probes).
    pub fn samples(self) -> u32 {
        match self {
            Self::Performance => 16,
            Self::Balanced => 32,
            Self::High => 48,
            Self::Ultra => 64,
        }
    }
}

/// World-probe fallback policy. Flux drives the main view; the legacy world-probe
/// DDGI grid only feeds the in-game camera + off-screen fallback, so it doesn't
/// need to re-march every frame (that was a redundant ~6ms CPU cost).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ProbeFallback {
    /// No world-probe march while Flux is active (lowest CPU).
    Off,
    /// March the world probes at a low cadence for the fallback paths.
    #[default]
    Throttled,
}

impl ProbeFallback {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Throttled => "Throttled",
        }
    }
    pub const ALL: [ProbeFallback; 2] = [Self::Off, Self::Throttled];
}

/// Realtime global-illumination (probe) settings. Drives the live probe re-trace
/// that lets realtime lights cast soft indirect bounce without a bake.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeGi {
    pub enabled: bool,
    /// Flux quality preset (samples-per-probe + march step cap).
    pub quality: FluxQuality,
    /// Indirect bounces per path (1 = single bounce; more = softer fill).
    pub bounces: u32,
    /// GI strength multiplier on the indirect term.
    pub intensity: f32,
    /// Temporal smoothing 0..1: higher = smoother but more motion lag; lower =
    /// sharper/more responsive but noisier while the camera moves.
    pub smoothing: f32,
    // --- Advanced ---
    /// GDF resolution (occlusion + bounce detail): 64 / 128 / 256.
    pub gdf_resolution: u32,
    /// Max trace distance in world units (0 = auto from scene size).
    pub march_distance: f32,
    /// Firefly clamp on bounce samples; caps bright outliers (lower = calmer).
    pub firefly_clamp: f32,
    /// World-probe fallback policy for the in-game camera / off-screen surfaces.
    pub probe_fallback: ProbeFallback,
    /// Screen-space reflections: trace specular rays against the depth prepass +
    /// last frame's colour. Only active while Flux runs (it owns the depth prepass).
    pub ssr_enabled: bool,
    /// SSR reflection strength multiplier.
    pub ssr_intensity: f32,
    /// SSR max ray distance in view-space units.
    pub ssr_max_distance: f32,
    /// SSR roughness cutoff: surfaces rougher than this skip the march.
    pub ssr_roughness_cutoff: f32,
    /// Reflection model: 0 = environment cube only, 1 = screen-space (SSR),
    /// 2 = ray-traced (1 bounce; needs GPU ray-query support, falls back to SSR).
    pub reflection_mode: u8,
    // --- Internal: not shown in the UI. The world-probe DDGI fallback path still
    // uses these; samples/mode are derived from `quality`/Flux on load. ---
    #[doc(hidden)]
    pub mode: GiMode,
    #[doc(hidden)]
    pub samples: u32,
    #[doc(hidden)]
    pub probe_spacing: f32,
    #[doc(hidden)]
    pub temporal_blend: f32,
    #[doc(hidden)]
    pub update_interval: f32,
}

impl Default for RealtimeGi {
    fn default() -> Self {
        // Flux is the realtime GI path: soft, gently-settling realtime-GI look. The
        // internal world-probe fields stay only for the throttled fallback.
        Self {
            enabled: false,
            quality: FluxQuality::Balanced,
            bounces: 2,
            intensity: 1.0,
            smoothing: 0.5,
            gdf_resolution: 128,
            march_distance: 0.0, // auto
            firefly_clamp: 4.0,
            // Default: Flux only (no world-probe march). The DDGI fallback is
            // opt-in for projects using the in-game camera / off-screen GI.
            probe_fallback: ProbeFallback::Off,
            ssr_enabled: true,
            ssr_intensity: 1.0,
            ssr_max_distance: 40.0,
            ssr_roughness_cutoff: 0.6,
            reflection_mode: 1,
            mode: GiMode::Flux,
            samples: 64,
            probe_spacing: 1.0,
            temporal_blend: 0.12,
            // 0 = the GDF + emitter feed (what Flux samples) refreshes every
            // frame so moving emitters track without lag.
            update_interval: 0.0,
        }
    }
}

/// Lighting-bake parameters, authored per scene. Resolution is a texel
/// density (Bakery / Unity style); each static object's lightmap size is
/// `density × world-AABB size`, clamped to `max_lightmap`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BakeSettings {
    /// Lightmap texels per world meter.
    pub texel_density: f32,
    /// Indirect bounces per path (0 = direct + sky only).
    pub bounces: u32,
    /// Paths traced per texel / per probe.
    pub samples: u32,
    /// Upper clamp on a single object's lightmap resolution.
    pub max_lightmap: u32,
}

impl Default for BakeSettings {
    fn default() -> Self {
        Self {
            texel_density: 16.0,
            // High-quality multi-bounce by default (the FluxBaker slider goes to 8):
            // 6 bounces + plenty of paths per texel so the indirect settles to a
            // clean, good-looking lightmap rather than a noisy 1–2-bounce preview.
            bounces: 6,
            samples: 256,
            max_lightmap: 512,
        }
    }
}

fn default_shadow_resolution() -> u32 {
    2048
}

fn default_shadow_softness() -> f32 {
    1.0
}

fn default_shadow_distance() -> f32 {
    25.0
}

impl Default for WorldEnvironment {
    fn default() -> Self {
        Self {
            ambient: [0.13, 0.14, 0.18],
            ambient_intensity: 1.0,
            sun_enabled: true,
            sun_color: [1.0, 0.98, 0.92],
            sun_intensity: 3.0,
            sun_direction: [-0.4, -1.0, -0.3],
            skybox_enabled: true,
            shadow_resolution: default_shadow_resolution(),
            shadow_softness: default_shadow_softness(),
            shadow_distance: default_shadow_distance(),
            bake: BakeSettings::default(),
            realtime_gi: RealtimeGi::default(),
            postfx: crate::PostFxProfile::default(),
            fog_enabled: false,
            fog_color: default_fog_color(),
            fog_density: default_fog_density(),
            fog_height_falloff: 0.0,
            fog_height_ref: 0.0,
            fog_start_distance: 0.0,
            skybox_faces: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SceneFile {
    pub entries: Vec<SceneEntry>,
    /// Project-relative path to the equirectangular skybox image, if any.
    /// `None` uses the procedural gradient sky.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skybox: Option<String>,
    /// Scene environment / world lighting.
    #[serde(default)]
    pub environment: WorldEnvironment,
    /// Last editor (fly-cam) viewpoint, restored on open so the scene reopens
    /// framed the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_camera: Option<EditorCamera>,
    /// Object ids whose hierarchy row was COLLAPSED in the scene tree, so the
    /// tree reopens in the same expanded/collapsed state. (Stored as collapsed
    /// since the default is expanded.)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed: Vec<String>,
}

/// Saved editor fly-camera pose (mirrors `FlyCamera`'s position/yaw/pitch).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct EditorCamera {
    pub position: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
}

#[cfg(test)]
mod rgi_compat_tests {
    use super::*;

    #[test]
    fn legacy_bool_and_struct_both_parse() {
        let legacy: WorldEnvironment =
            ron::from_str("(ambient:(0.1,0.1,0.1),ambient_intensity:1.0,sun_enabled:true,sun_color:(1.0,1.0,1.0),sun_intensity:3.0,sun_direction:(0.0,-1.0,0.0),skybox_enabled:true,realtime_gi:false)")
                .expect("legacy bool form must parse");
        assert!(!legacy.realtime_gi.enabled);

        let full: WorldEnvironment =
            ron::from_str("(ambient:(0.1,0.1,0.1),ambient_intensity:1.0,sun_enabled:true,sun_color:(1.0,1.0,1.0),sun_intensity:3.0,sun_direction:(0.0,-1.0,0.0),skybox_enabled:true,realtime_gi:(enabled:true,bounces:3))")
                .expect("struct form must parse");
        assert!(full.realtime_gi.enabled);
        assert_eq!(full.realtime_gi.bounces, 3);

        // The struct form with the `mode` enum field. This is what broke the
        // untagged shim (RON enum can't replay through serde's Content buffer).
        let with_mode: WorldEnvironment =
            ron::from_str("(ambient:(0.1,0.1,0.1),ambient_intensity:1.0,sun_enabled:true,sun_color:(1.0,1.0,1.0),sun_intensity:3.0,sun_direction:(0.0,-1.0,0.0),skybox_enabled:true,realtime_gi:(enabled:true,mode:Software,bounces:2,samples:64,intensity:1.0,probe_spacing:2.0,temporal_blend:0.12,update_interval:0.2))")
                .expect("struct form with mode enum must parse");
        assert_eq!(with_mode.realtime_gi.mode, GiMode::Flux);
    }
}

pub fn load_scene_file(path: impl AsRef<Path>) -> Result<SceneFile> {
    let path = path.as_ref();
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("parsing scene file {}", path.display()))
}

pub fn save_scene_file(path: impl AsRef<Path>, scene: &SceneFile) -> Result<()> {
    let path = path.as_ref();
    let text = ron::ser::to_string_pretty(scene, ron::ser::PrettyConfig::default())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
