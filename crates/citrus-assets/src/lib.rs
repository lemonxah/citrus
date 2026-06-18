//! citrus-assets: CPU-side scene loading.
//!
//! M2: glTF 2.0 worlds/props + a procedural test scene. M3 adds VRM avatar
//! parsing (humanoid rig, expressions, spring bones) on top of the glTF
//! loader.

mod asset_meta;
mod bake_file;
mod fbx_loader;
mod gltf_loader;
mod lightmap_uv;
mod material_file;
mod post_file;
mod procedural;
mod project_file;
mod scene_file;
mod shader_file;
mod skeleton;
mod tex_cache;

pub use asset_meta::{
    AssetMeta, ImporterSettings, META_EXT, ModelImport, load_asset_meta,
    load_or_create_asset_meta, meta_path, save_asset_meta,
};
pub use fbx_loader::{load_fbx, load_fbx_with};
pub use gltf_loader::load_gltf;
pub use lightmap_uv::generate_lightmap_uv;
pub use skeleton::{
    AnimChannel, AnimationClip, ChannelPath, Joint, Skeleton, skin_direction, skin_position,
};
pub use material_file::{
    MATERIAL_EXTENSION, MaterialFile, MaterialTextures, load_material_file, load_texture_file,
    save_material_file,
};
pub use bake_file::{
    LIGHTDATA_EXTENSION, LIGHTMAP_EXTENSION, LightDataFile, LightmapEntry, LightmapFile,
    ProbeVolumeData, load_lightdata, load_lightmaps, save_lightdata, save_lightmaps,
};
pub use post_file::{
    Bloom, ChromaticAberration, ColorGrading, POSTFX_EXTENSION, PostFxProfile, TonemapMode,
    Vignette, blend_profiles, load_postfx, save_postfx,
};
pub use procedural::{primitive_mesh, test_scene};
pub use project_file::{
    PROJECT_FILE_NAME, ProjectFile, ProjectSettings, load_project_file, save_project_file,
};
pub use scene_file::{
    ComponentData, EditorCamera, FluxQuality, GiMode, MaterialRef, ObjectSource, PrimitiveShape,
    ProbeFallback, RealtimeGi, SCENE_EXTENSION, SceneEntry, BakeSettings, SceneFile,
    WorldEnvironment, load_scene_file, save_scene_file,
};
pub use tex_cache::load_texture_bc;
pub use shader_file::{
    SHADER_EXTENSION, SHADER_PROP_FLOATS, SHADER_TEMPLATE, ShaderProp, ShaderPropKind,
    ShaderSource, compile_shader, load_shader_file, parse_shader_source,
};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use citrus_render::{MaterialFeatures, MaterialParams, MeshData, TextureData};
use glam::Mat4;

/// One (mesh, material) sub-part of an instance.
#[derive(Clone, Copy)]
pub struct MeshSlot {
    pub mesh: usize,
    pub material: usize,
}

/// One renderable placement at a transform, with one or more material slots.
/// A single-material node has one slot; a multi-material mesh has one slot per
/// material (all drawn at the same transform, presented as one scene object).
pub struct Instance {
    pub name: String,
    pub transform: Mat4,
    pub slots: Vec<MeshSlot>,
}

impl Instance {
    /// A single-material instance (the common case).
    pub fn single(name: impl Into<String>, mesh: usize, material: usize, transform: Mat4) -> Self {
        Self {
            name: name.into(),
            transform,
            slots: vec![MeshSlot { mesh, material }],
        }
    }
}

/// A material referencing scene-local texture indices; the engine maps
/// these to renderer handles at upload.
pub struct SceneMaterial {
    pub name: String,
    pub params: MaterialParams,
    pub features: MaterialFeatures,
    pub albedo: Option<usize>,
    pub normal: Option<usize>,
    pub orm: Option<usize>,
    pub emission: Option<usize>,
}

pub struct Scene {
    pub meshes: Vec<MeshData>,
    pub textures: Vec<TextureData>,
    pub materials: Vec<SceneMaterial>,
    pub instances: Vec<Instance>,
    /// Imported armatures (one per glTF skin / FBX skin deformer). Vertex joint
    /// indices are skin-local and line up with `skeletons[mesh's skin].joints`.
    pub skeletons: Vec<Skeleton>,
    /// Imported skeletal animation clips (shared across the file's skeletons).
    pub animations: Vec<AnimationClip>,
}

/// True if the extension is an importable model format.
pub fn is_model_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some("gltf" | "glb" | "fbx")
    )
}

/// Sidecar marker that records "generate lightmap UVs for this model's meshes".
/// Its presence (content unused) means each mesh lacking a real UV1 gets a
/// deterministic generated unwrap on load — so the generated UVs persist across
/// sessions without storing the whole rebuilt mesh.
pub fn lightmap_uv_marker_path(model: impl AsRef<Path>) -> PathBuf {
    model.as_ref().with_extension("lmuv")
}

/// Create (or remove) the lightmap-UV marker for a model. Called by the editor
/// when the user opts to generate lightmap UVs at bake time.
pub fn set_lightmap_uv_marker(model: impl AsRef<Path>, on: bool) -> Result<()> {
    let p = lightmap_uv_marker_path(&model);
    if on {
        std::fs::write(&p, b"citrus-lightmap-uv\n")
            .with_context(|| format!("writing lightmap-UV marker {}", p.display()))?;
    } else if p.exists() {
        std::fs::remove_file(&p)
            .with_context(|| format!("removing lightmap-UV marker {}", p.display()))?;
    }
    Ok(())
}

/// Apply the lightmap-UV marker after loading: replace every mesh that has no
/// real UV1 with a generated non-overlapping unwrap. No-op without the marker.
fn apply_lightmap_uv_marker(path: &Path, scene: &mut Scene) {
    if !lightmap_uv_marker_path(path).exists() {
        return;
    }
    for mesh in &mut scene.meshes {
        if !mesh.has_lightmap_uv {
            *mesh = generate_lightmap_uv(mesh);
        }
    }
}

/// Import any supported model format, dispatching on extension.
pub fn load_model(path: impl AsRef<Path>) -> Result<Scene> {
    let path = path.as_ref();
    let mut scene = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("gltf" | "glb") => load_gltf(path),
        Some("fbx") => load_fbx(path),
        other => bail!("unsupported model format {other:?} (gltf, glb, fbx)"),
    }?;
    apply_lightmap_uv_marker(path, &mut scene);
    Ok(scene)
}

/// Load a model applying its `.meta` sidecar import settings if the sidecar
/// exists (read-only, safe in a shipped game; the editor creates/edits the meta
/// separately). FBX honours the settings; glTF uses defaults for now.
pub fn load_model_with_meta(path: impl AsRef<Path>) -> Result<Scene> {
    let path = path.as_ref();
    let model = load_asset_meta(path)?
        .and_then(|m| m.importer.as_model().cloned())
        .unwrap_or_default();
    let mut scene = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("gltf" | "glb") => load_gltf(path),
        Some("fbx") => load_fbx_with(path, &model),
        other => bail!("unsupported model format {other:?} (gltf, glb, fbx)"),
    }?;
    apply_lightmap_uv_marker(path, &mut scene);
    Ok(scene)
}
