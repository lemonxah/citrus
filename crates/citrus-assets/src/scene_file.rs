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
    /// Open-top box with walls (a bucket / tub).
    Bucket,
}

impl PrimitiveShape {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cube => "Cube",
            Self::Sphere => "Sphere",
            Self::Capsule => "Capsule",
            Self::Plane => "Plane",
            Self::Bucket => "Bucket",
        }
    }
}

fn default_shader() -> String {
    "standard".into()
}

/// Which material an object uses. The inline snapshot is large, but a scene
/// holds few of these and boxing it would change every match site.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum MaterialRef {
    /// A `.material` asset file (project-relative path).
    File(String),
    /// The object's own material exactly as its model (or primitive) brings
    /// it: nothing stored, so unedited model materials cost no scene space.
    Model,
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

/// One serialized component: registry name + its RON-encoded state, and
/// whether it is switched on (omitted when it is).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComponentData {
    pub kind: String,
    pub data: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

fn is_true(v: &bool) -> bool {
    *v
}

impl ComponentData {
    /// A switched-on component.
    pub fn new(kind: impl Into<String>, data: impl Into<String>) -> Self {
        Self { kind: kind.into(), data: data.into(), enabled: true }
    }

    /// `c` saved, switched off when it is wrapped in [`citrus_core::Disabled`].
    pub fn of(c: &dyn citrus_core::Component) -> Self {
        Self { kind: c.type_name().to_owned(), data: c.save(), enabled: citrus_core::component_enabled(c) }
    }

    /// Rebuilt through `registry`, switched off when saved off.
    pub fn load(&self, registry: &citrus_core::ComponentRegistry) -> Option<Box<dyn citrus_core::Component>> {
        let c = registry.load(&self.kind, &self.data)?;
        Some(if self.enabled { c } else { Box::new(citrus_core::Disabled(c)) })
    }
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
    /// "Cast Shadows" (Unity's MeshRenderer setting). Omitted when On, so
    /// older scenes load as On.
    #[serde(default, skip_serializing_if = "citrus_core::ShadowCasting::is_on")]
    pub cast_shadows: citrus_core::ShadowCasting,
    /// Object layer index (0..32, Unity-style): camera culling mask (rendering)
    /// + layer-collision matrix (physics). 0 = "Default".
    #[serde(default)]
    pub layer: u8,
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
    /// On a model's root: the model whose untouched parts aren't stored (they
    /// are rebuilt from the model file on load).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelInstanceRef>,
    /// On a stored model part: which of the model's parts it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_part: Option<u32>,
}

/// A model placed in a scene, stored as a reference plus what changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInstanceRef {
    pub path: String,
    /// Parts deleted from this instance (indices into the model's parts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<u32>,
}

/// One part (instance) of a model as imported: what a pristine scene object
/// for it looks like.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelPart {
    pub name: String,
    pub mesh: usize,
    pub extra_meshes: Vec<usize>,
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl ModelPart {
    /// The parts of a loaded model, in the order its objects are created.
    pub fn of(model: &crate::Scene) -> Vec<ModelPart> {
        model
            .instances
            .iter()
            .filter_map(|inst| {
                let slot0 = inst.slots.first()?;
                let (s, r, t) = inst.transform.to_scale_rotation_translation();
                Some(ModelPart {
                    name: inst.name.clone(),
                    mesh: slot0.mesh,
                    extra_meshes: inst.slots[1..].iter().map(|x| x.mesh).collect(),
                    translation: t.to_array(),
                    rotation: r.to_array(),
                    scale: s.to_array(),
                })
            })
            .collect()
    }
}

fn close(a: &[f32], b: &[f32]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
}

/// Stable id for part `k` of the model instance whose root has id `root`.
fn part_id(root: &str, k: usize) -> String {
    let mut h1 = 0xcbf2_9ce4_8422_2325u64;
    let mut h2 = 0x8422_2325_cbf2_9ce4u64;
    for b in root.bytes().chain((k as u64).to_le_bytes()) {
        h1 = (h1 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        h2 = (h2 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(5);
    }
    let b = (((h1 as u128) << 64) | h2 as u128).to_be_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

impl SceneFile {
    /// Model paths this file needs, including ones only referenced by a
    /// compacted model root.
    pub fn model_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in &self.entries {
            let path = match (&e.source, &e.model) {
                (_, Some(m)) => Some(&m.path),
                (ObjectSource::Model { path, .. }, None) => Some(path),
                _ => None,
            };
            if let Some(p) = path
                && !out.contains(p)
            {
                out.push(p.clone());
            }
        }
        out
    }

    /// Store each model placed in the scene as its root plus what changed:
    /// parts still exactly as imported (and not referenced by anything) are
    /// dropped, deleted parts are listed, edited ones stay as entries tagged
    /// with their part index. `parts_of` lists a model's parts.
    pub fn compact_models(&mut self, parts_of: &dyn Fn(&str) -> Option<Vec<ModelPart>>) {
        let n = self.entries.len();
        let mut has_children = vec![false; n];
        for e in &self.entries {
            if let Some(p) = e.parent.filter(|p| *p < n) {
                has_children[p] = true;
            }
        }
        let all_data: String = self.entries.iter().flat_map(|e| e.components.iter().map(|c| c.data.as_str())).collect::<Vec<_>>().join(" ");
        let mut drop = vec![false; n];
        for r in 0..n {
            if self.entries[r].parent.is_some() || !matches!(self.entries[r].source, ObjectSource::Empty) || self.entries[r].model.is_some() {
                continue;
            }
            let children: Vec<usize> = (0..n).filter(|&c| self.entries[c].parent == Some(r)).collect();
            let model_children: Vec<usize> = children.iter().copied().filter(|&c| matches!(self.entries[c].source, ObjectSource::Model { .. })).collect();
            let Some(path) = model_children.first().and_then(|&c| match &self.entries[c].source {
                ObjectSource::Model { path, .. } => Some(path.clone()),
                _ => None,
            }) else {
                continue;
            };
            if !model_children.iter().all(|&c| matches!(&self.entries[c].source, ObjectSource::Model { path: p, .. } if *p == path)) {
                continue;
            }
            let Some(parts) = parts_of(&path) else { continue };
            // Match each model child to a distinct part: same meshes, preferring
            // the same name, in order.
            let mut claimed = vec![false; parts.len()];
            let mut assign = Vec::new();
            for &c in &model_children {
                let ObjectSource::Model { mesh, extra_meshes, .. } = &self.entries[c].source else { unreachable!() };
                let fits = |k: usize| !claimed[k] && parts[k].mesh == *mesh && parts[k].extra_meshes == *extra_meshes;
                let name = &self.entries[c].name;
                let k = (0..parts.len()).find(|&k| fits(k) && parts[k].name == *name).or_else(|| (0..parts.len()).find(|&k| fits(k)));
                match k {
                    Some(k) => {
                        claimed[k] = true;
                        assign.push((c, k));
                    }
                    None => break,
                }
            }
            if assign.len() != model_children.len() {
                continue;
            }
            for &(c, k) in &assign {
                let e = &self.entries[c];
                let p = &parts[k];
                let pristine = e.name == p.name
                    && e.enabled
                    && !e.static_geometry
                    && e.lightmap_scale == 1.0
                    && e.cast_shadows.is_on()
                    && e.layer == 0
                    && matches!(e.material, MaterialRef::Model)
                    && e.extra_materials.iter().all(|m| matches!(m, MaterialRef::Model))
                    && e.extra_materials.len() == p.extra_meshes.len()
                    && e.components.is_empty()
                    && !has_children[c]
                    && (e.id.is_empty() || !all_data.contains(&e.id))
                    && close(&e.translation, &p.translation)
                    && close(&e.rotation, &p.rotation)
                    && close(&e.scale, &p.scale);
                if pristine {
                    drop[c] = true;
                } else {
                    self.entries[c].model_part = Some(k as u32);
                }
            }
            let removed = (0..parts.len()).filter(|&k| !claimed[k]).map(|k| k as u32).collect();
            self.entries[r].model = Some(ModelInstanceRef { path, removed });
        }
        if !drop.iter().any(|d| *d) {
            return;
        }
        let mut new_index = vec![None; n];
        let mut next = 0;
        for i in 0..n {
            if !drop[i] {
                new_index[i] = Some(next);
                next += 1;
            }
        }
        let old = std::mem::take(&mut self.entries);
        self.entries = old
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !drop[*i])
            .map(|(_, mut e)| {
                e.parent = e.parent.and_then(|p| new_index.get(p).copied().flatten());
                e
            })
            .collect();
    }

    /// Rebuild the parts compacted away by [`Self::compact_models`]: every
    /// part of a model root that wasn't removed and isn't stored as an entry
    /// is appended as a pristine child with a stable id.
    pub fn expand_models(&mut self, parts_of: &dyn Fn(&str) -> Option<Vec<ModelPart>>) {
        let n = self.entries.len();
        for r in 0..n {
            let Some(m) = self.entries[r].model.clone() else { continue };
            let Some(parts) = parts_of(&m.path) else { continue };
            let stored: Vec<u32> = (0..n).filter(|&c| self.entries[c].parent == Some(r)).filter_map(|c| self.entries[c].model_part).collect();
            let root_id = self.entries[r].id.clone();
            for (k, p) in parts.iter().enumerate() {
                if m.removed.contains(&(k as u32)) || stored.contains(&(k as u32)) {
                    continue;
                }
                self.entries.push(SceneEntry {
                    id: part_id(&root_id, k),
                    name: p.name.clone(),
                    source: ObjectSource::Model { path: m.path.clone(), mesh: p.mesh, extra_meshes: p.extra_meshes.clone() },
                    enabled: true,
                    static_geometry: false,
                    lightmap_scale: 1.0,
                    cast_shadows: citrus_core::ShadowCasting::On,
                    layer: 0,
                    material: MaterialRef::Model,
                    extra_materials: vec![MaterialRef::Model; p.extra_meshes.len()],
                    parent: Some(r),
                    components: Vec::new(),
                    translation: p.translation,
                    rotation: p.rotation,
                    scale: p.scale,
                    model: None,
                    model_part: Some(k as u32),
                });
            }
        }
    }
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
    #[default]
    FluxRT,
    /// Flux: software ray-marching of per-mesh signed distance fields (no RT
    /// cores, runs anywhere).
    Flux,
    /// FluxVoxel: analytic voxel light volume (no ray tracing). Injects lights +
    /// emissive into an SH-L1 probe grid every frame — cheapest backend, built
    /// for VR. Static + dynamic meshes read it; dynamic lights mix in live.
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

    // --- Per-backend capability flags ---
    // Each backend declares which settings it actually consumes, so the
    // Environment UI can render only the relevant controls (modular settings)
    // and the engine can skip work a backend ignores.

    /// Path-traced bounce count (FluxRT / Flux). FluxVoxel injects direct-only.
    pub fn uses_bounces(self) -> bool {
        matches!(self, Self::FluxRT | Self::Flux)
    }
    /// Rays-per-probe quality preset (the ray-marching/ray-query backends).
    pub fn uses_quality(self) -> bool {
        matches!(self, Self::FluxRT | Self::Flux)
    }
    /// Per-probe sample count + firefly clamp (the sampling backends).
    pub fn uses_samples(self) -> bool {
        matches!(self, Self::FluxRT | Self::Flux)
    }
    /// Signed-distance-field resolution (software march only).
    pub fn uses_gdf(self) -> bool {
        matches!(self, Self::Flux)
    }
    /// March distance (the software march; RT uses the BVH, Voxel uses volumes).
    pub fn uses_march_distance(self) -> bool {
        matches!(self, Self::Flux)
    }
    /// Voxel-grid density (the FluxVoxel analytic volume only).
    pub fn uses_voxel_density(self) -> bool {
        matches!(self, Self::FluxVoxel)
    }
    /// DDGI-style per-probe visibility (currently the FluxVoxel voxel-light path).
    pub fn uses_ddgi_occlusion(self) -> bool {
        matches!(self, Self::FluxVoxel)
    }
    /// LPV-style propagation bounce (FluxVoxel only — RT/SDF bounce via their march).
    pub fn uses_propagation(self) -> bool {
        matches!(self, Self::FluxVoxel)
    }
    /// VXGI-style specular-from-volume (FluxVoxel only — RT/SDF reflect via cube/BVH).
    pub fn uses_voxel_specular(self) -> bool {
        matches!(self, Self::FluxVoxel)
    }
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

/// How the auto FluxVoxel grid is laid out when there are no author-placed
/// FluxVolumes. Trades coverage of empty space against probe count (= per-frame
/// cost). Only consulted by the FluxVoxel backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum VoxelGridMode {
    /// One grid spanning the whole static-scene AABB. Simplest, but the probe
    /// count scales with level size — empty space you never visit is computed too.
    #[default]
    WholeScene,
    /// A fixed-size grid centred on the camera (snapped to the probe spacing so it
    /// scrolls in discrete steps). Constant probe count regardless of level size:
    /// space you never go to is never computed. Best for large levels.
    CameraClipmap,
    /// Whole-scene bounds, but probes far from any geometry are dropped (left dark)
    /// so open air between surfaces costs nothing to inject/propagate.
    Occupancy,
    /// Tight grids hugging clusters of objects; the gaps between clusters aren't
    /// covered. Sparse by construction. Capped at a few clusters (shader volume cap).
    PerObject,
}

impl VoxelGridMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::WholeScene => "Whole scene",
            Self::CameraClipmap => "Camera clipmap",
            Self::Occupancy => "Occupancy culled",
            Self::PerObject => "Per object",
        }
    }
    pub const ALL: [VoxelGridMode; 4] = [
        Self::WholeScene,
        Self::CameraClipmap,
        Self::Occupancy,
        Self::PerObject,
    ];
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
    /// Multiplier on how much light emissive objects throw into the GI (their
    /// on-screen brightness is unchanged). 1 = physical; small emitters at
    /// emission intensity ~1 are dim light sources, so raise this (like
    /// Lumen's emissive boost) for stronger coloured pools.
    pub emissive_boost: f32,
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
    /// 2 = ray-traced (needs GPU ray-query support, falls back to SSR).
    pub reflection_mode: u8,
    /// Ray-traced reflections: rays per pixel. Rough surfaces spread them over
    /// their glossy lobe, so more rays = smoother glossy reflections.
    pub rt_samples: u32,
    /// Ray-traced reflections: bounces (2+ shows mirrors / metals inside reflections).
    pub rt_bounces: u32,
    // --- FluxVoxel backend ---
    /// Probes per world meter for the auto scene-covering FluxVoxel grid (used
    /// when there are 0 author-placed FluxVolumes). Higher = finer GI, more cost.
    pub voxel_density: f32,
    /// Auto-build a scene-covering FluxVoxel grid when no FluxVolumes exist, so
    /// the whole scene gets voxel GI (not just inside author-placed volumes).
    /// Author-placed FluxVolumes and the auto grid are mutually exclusive: placing
    /// any FluxVolume turns the auto grid off (the author then controls coverage).
    pub voxel_auto_grid: bool,
    /// Layout strategy for the auto grid (see `VoxelGridMode`). Lets large levels
    /// keep a constant probe count (camera clipmap) or skip empty space.
    pub voxel_grid_mode: VoxelGridMode,
    /// Half-extent (metres) of the camera-clipmap auto grid: the grid is a cube of
    /// side `2 * extent` centred on the camera. Larger = more GI range, more cost.
    pub voxel_clipmap_extent: f32,
    /// DDGI-style occlusion for FluxVoxel voxel lights: per-probe directional
    /// distance moments + a Chebyshev visibility test so voxel lights are blocked
    /// by geometry (cheap shadows). Toggle off on low-end/VR to save the moment
    /// write + test. Expensive-ish; ON by default.
    pub voxel_ddgi_occlusion: bool,
    /// FluxVoxel light propagation (LPV-style ≥1 diffuse bounce): spread injected
    /// radiance through the probe grid so light fills shadowed pockets. Toggle off
    /// on low-end/VR for direct-only (cheapest). ON by default.
    pub voxel_propagation: bool,
    /// FluxVoxel specular GI: metallic/rough surfaces sample the probe volume in the
    /// reflection direction (VXGI-style glossy approx) so they pick up emissive +
    /// voxel-light bounce, not just the reflection cube. ON by default.
    pub voxel_specular: bool,
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
            emissive_boost: 1.0,
            // Default: Flux only (no world-probe march). The DDGI fallback is
            // opt-in for projects using the in-game camera / off-screen GI.
            probe_fallback: ProbeFallback::Off,
            ssr_enabled: true,
            ssr_intensity: 1.0,
            ssr_max_distance: 40.0,
            ssr_roughness_cutoff: 0.6,
            reflection_mode: 1,
            rt_samples: 1,
            rt_bounces: 1,
            voxel_density: 1.0,
            voxel_auto_grid: true,
            voxel_grid_mode: VoxelGridMode::WholeScene,
            voxel_clipmap_extent: 16.0,
            voxel_ddgi_occlusion: true,
            voxel_propagation: true,
            voxel_specular: true,
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
    /// GPU throttle: fraction of each GPU bake dispatch's time to then idle the GPU
    /// so the desktop stays responsive while baking. 0 = max speed (bake hogs the
    /// GPU), 1 = ~50% duty cycle (≈2× slower bake, system stays usable).
    #[serde(default = "default_gpu_throttle")]
    pub gpu_throttle: f32,
    /// Store baked lightmaps as BC6H (~16x smaller on disk and in VRAM than
    /// RGBA32F). Off writes the raw v1 format.
    pub compress_lightmaps: bool,
    /// Store baked reflection-probe cubes (full mip chain) as BC6H.
    pub compress_reflections: bool,
}

fn default_gpu_throttle() -> f32 {
    1.0
}

impl Default for BakeSettings {
    fn default() -> Self {
        Self {
            // Detailed by default so lightmaps aren't pixelated/blocky: 40 texels/m
            // with a 2048 per-object cap (was 16/m capped at 512, which clamped any
            // sizeable surface to visible texel squares). Bakes take longer, but the
            // result is sharp. Crank texel_density (slider to 1024) + Max Lightmap
            // (up to 4096) for even finer detail.
            texel_density: 40.0,
            // High-quality multi-bounce by default (the FluxBaker slider goes to 8):
            // 6 bounces + plenty of paths per texel so the indirect settles to a
            // clean, good-looking lightmap rather than a noisy 1–2-bounce preview.
            bounces: 6,
            samples: 512,
            max_lightmap: 2048,
            gpu_throttle: default_gpu_throttle(),
            compress_lightmaps: true,
            compress_reflections: true,
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
    /// Layer names + collision matrix (Unity-style). Drives physics collision
    /// filtering and per-camera render culling masks.
    #[serde(default)]
    pub layers: citrus_core::LayerSettings,
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
    fn component_enabled_flag_is_saved_only_when_off() {
        let on = ComponentData::new("Grabbable", "()");
        assert!(!ron::to_string(&on).unwrap().contains("enabled"), "omitted while on");
        let off = ComponentData { enabled: false, ..on.clone() };
        let text = ron::to_string(&off).unwrap();
        assert!(text.contains("enabled:false"), "{text}");
        assert_eq!(ron::from_str::<ComponentData>(&text).unwrap(), off);
        let old: ComponentData = ron::from_str("(kind:\"Grabbable\",data:\"()\")").unwrap();
        assert!(old.enabled, "old scenes load switched on");
    }

    #[test]
    fn voxel_grid_mode_all_covers_every_variant_with_labels() {
        assert_eq!(VoxelGridMode::ALL.len(), 4);
        for m in VoxelGridMode::ALL {
            assert!(!m.label().is_empty());
        }
        // Default is the simplest whole-scene grid (current behaviour).
        assert_eq!(VoxelGridMode::default(), VoxelGridMode::WholeScene);
    }

    #[test]
    fn realtime_gi_default_has_grid_mode_and_clipmap_extent() {
        let gi = RealtimeGi::default();
        assert_eq!(gi.voxel_grid_mode, VoxelGridMode::WholeScene);
        assert!(gi.voxel_clipmap_extent > 0.0);
    }

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
            ron::from_str("(ambient:(0.1,0.1,0.1),ambient_intensity:1.0,sun_enabled:true,sun_color:(1.0,1.0,1.0),sun_intensity:3.0,sun_direction:(0.0,-1.0,0.0),skybox_enabled:true,realtime_gi:(enabled:true,mode:Flux,bounces:2,samples:64,intensity:1.0,probe_spacing:2.0,temporal_blend:0.12,update_interval:0.2))")
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

#[cfg(test)]
mod model_instance_tests {
    use super::*;

    fn parts() -> Vec<ModelPart> {
        (0..4)
            .map(|k| ModelPart {
                name: format!("Part{k}"),
                mesh: k,
                extra_meshes: Vec::new(),
                translation: [k as f32, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
            })
            .collect()
    }

    fn entry(name: &str, source: ObjectSource, parent: Option<usize>, t: [f32; 3]) -> SceneEntry {
        SceneEntry {
            id: format!("00000000-0000-0000-0000-{:012}", name.len() * 1000 + t[0] as usize),
            name: name.into(),
            source,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: citrus_core::ShadowCasting::On,
            layer: 0,
            material: MaterialRef::Model,
            extra_materials: Vec::new(),
            parent,
            components: Vec::new(),
            translation: t,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            model: None,
            model_part: None,
        }
    }

    fn model(k: usize) -> ObjectSource {
        ObjectSource::Model { path: "models/house.glb".into(), mesh: k, extra_meshes: Vec::new() }
    }

    #[test]
    fn pristine_parts_drop_out_and_come_back() {
        let mut file = SceneFile::default();
        file.entries.push(entry("House", ObjectSource::Empty, None, [0.0; 3]));
        file.entries.push(entry("Part0", model(0), Some(0), [0.0, 0.0, 0.0]));
        file.entries.push(entry("Part1", model(1), Some(0), [1.0, 5.0, 0.0])); // moved
        // Part2 deleted by the user.
        file.entries.push(entry("Part3", model(3), Some(0), [3.0, 0.0, 0.0]));
        file.entries.push(entry("Lamp", ObjectSource::Empty, Some(0), [0.0; 3]));
        // Something references Part3, so it must keep its id and stay stored.
        let part3_id = file.entries[3].id.clone();
        file.entries[4].components.push(ComponentData::new("Spin", format!("(target:(Some(\"{part3_id}\")))")));

        let full = file.clone();
        file.compact_models(&|_| Some(parts()));
        assert_eq!(file.entries.len(), 4, "Part0 dropped");
        assert_eq!(file.entries[0].model, Some(ModelInstanceRef { path: "models/house.glb".into(), removed: vec![2] }));
        assert_eq!(file.entries[1].model_part, Some(1));
        assert!(file.entries.iter().all(|e| e.parent.is_none_or(|p| p < file.entries.len())));

        let text = ron::ser::to_string_pretty(&file, ron::ser::PrettyConfig::default()).unwrap();
        let mut back: SceneFile = ron::from_str(&text).unwrap();
        back.expand_models(&|_| Some(parts()));
        back.expand_models(&|_| Some(parts()));
        assert_eq!(back.entries.len(), full.entries.len(), "expansion is idempotent and restores the part");
        let p0 = back.entries.iter().find(|e| e.name == "Part0").unwrap();
        assert_eq!((p0.translation, p0.parent), ([0.0; 3], Some(0)));
        assert!(back.entries.iter().all(|e| e.name != "Part2"), "removed stays removed");
        let moved = back.entries.iter().find(|e| e.name == "Part1").unwrap();
        assert_eq!(moved.translation, [1.0, 5.0, 0.0]);
        let ids: Vec<&str> = back.entries.iter().map(|e| e.id.as_str()).collect();
        let mut again = back.clone();
        again.entries.truncate(4);
        again.expand_models(&|_| Some(parts()));
        assert_eq!(again.entries.last().unwrap().id, *ids.last().unwrap(), "rebuilt parts keep stable ids");
    }

    #[test]
    fn edited_materials_and_components_keep_a_part_stored() {
        let mut file = SceneFile::default();
        file.entries.push(entry("House", ObjectSource::Empty, None, [0.0; 3]));
        for k in 0..4 {
            file.entries.push(entry(&format!("Part{k}"), model(k), Some(0), [k as f32, 0.0, 0.0]));
        }
        file.entries[2].material = MaterialRef::File("materials/brick.material".into());
        file.entries[3].components.push(ComponentData::new("Spin", "()"));
        file.compact_models(&|_| Some(parts()));
        assert_eq!(file.entries.len(), 3, "root + the two edited parts");
    }

    #[test]
    fn cast_shadows_keeps_a_part_stored() {
        let mut file = SceneFile::default();
        file.entries.push(entry("House", ObjectSource::Empty, None, [0.0; 3]));
        for k in 0..4 {
            file.entries.push(entry(&format!("Part{k}"), model(k), Some(0), [k as f32, 0.0, 0.0]));
        }
        file.entries[2].cast_shadows = citrus_core::ShadowCasting::ShadowsOnly;
        file.compact_models(&|_| Some(parts()));
        assert_eq!(file.entries.len(), 2, "root + the part that no longer casts like the model");
        assert_eq!(file.entries[1].cast_shadows, citrus_core::ShadowCasting::ShadowsOnly);
    }

    #[test]
    fn cast_shadows_is_omitted_when_on_and_round_trips() {
        let mut file = SceneFile::default();
        file.entries.push(entry("Plane", ObjectSource::Empty, None, [0.0; 3]));
        // On is not written, so this text is also what older scenes look like.
        let text = ron::ser::to_string_pretty(&file, ron::ser::PrettyConfig::default()).unwrap();
        assert!(!text.contains("cast_shadows"));
        let back: SceneFile = ron::from_str(&text).unwrap();
        assert_eq!(back.entries[0].cast_shadows, citrus_core::ShadowCasting::On);

        for mode in [citrus_core::ShadowCasting::Off, citrus_core::ShadowCasting::TwoSided, citrus_core::ShadowCasting::ShadowsOnly] {
            file.entries[0].cast_shadows = mode;
            let text = ron::ser::to_string_pretty(&file, ron::ser::PrettyConfig::default()).unwrap();
            assert!(text.contains("cast_shadows"));
            let back: SceneFile = ron::from_str(&text).unwrap();
            assert_eq!(back.entries[0].cast_shadows, mode);
        }
    }
}
