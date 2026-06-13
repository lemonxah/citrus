//! `.scene` asset files (RON): a list of placed objects with sources and
//! material references, so multiple scenes can be saved and reloaded.

use std::path::Path;

use anyhow::{Context as _, Result};
use citrus_render::{MaterialFeatures, MaterialParams};
use serde::{Deserialize, Serialize};

pub const SCENE_EXTENSION: &str = "scene";

/// Where an object's mesh came from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ObjectSource {
    /// A model file in the project; `mesh` is the flattened primitive index
    /// produced by the importer (stable for an unchanged file).
    Model { path: String, mesh: usize },
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
    /// lightmapped surface + ray-trace occluder.
    #[serde(default)]
    pub static_geometry: bool,
    pub material: MaterialRef,
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
    /// Directional shadow coverage (world units) — the ortho box is fit to
    /// this size ahead of the camera. Smaller = sharper, less coverage.
    #[serde(default = "default_shadow_distance")]
    pub shadow_distance: f32,
    /// Lighting-bake settings (Bakery-style: texels-per-meter density).
    #[serde(default)]
    pub bake: BakeSettings,
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
            bounces: 2,
            samples: 128,
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
