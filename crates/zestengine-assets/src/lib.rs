//! zestengine-assets: CPU-side scene loading.
//!
//! M2: glTF 2.0 worlds/props + a procedural test scene. M3 adds VRM avatar
//! parsing (humanoid rig, expressions, spring bones) on top of the glTF
//! loader.

mod anim_clip;
mod anim_controller;
mod asset_meta;
mod bake_file;
mod behaviour_tree;
mod collision;
mod fbx_loader;
mod gltf_loader;
pub mod hdr_codec;
pub mod vrm;
pub mod simplify;
mod lightmap_uv;
mod material_file;
mod meshlet;
mod post_file;
mod procedural;
mod project_file;
mod scene_file;
mod sequencer;
mod shader_file;
mod shader_props;
mod shader_graph;
mod skeleton;
mod tex_cache;
pub mod package;
pub mod game_pak;
// Project Hub support: template catalogue, recent projects, sibling executables.
pub mod launcher;
pub mod project_templates;
pub mod recent_projects;

pub use asset_meta::{
    AssetMeta, ImporterSettings, META_EXT, ModelImport, load_asset_meta,
    load_or_create_asset_meta, meta_path, save_asset_meta,
};
pub use anim_clip::{
    ANIM_CLIP_EXTENSION, AnimClip, AnimTrack, ClipEvent, KeyInterp, Keyframe, TrackProperty,
    load_anim_clip, save_anim_clip,
};
pub use anim_controller::{
    ANIM_CONTROLLER_EXTENSION, AnimCondition, AnimLayer, AnimParamKind, AnimParameter, AnimState,
    AnimTransition, AnimatorController, CondOp, LayerBlend, Motion, StateMachine,
    load_anim_controller, save_anim_controller,
};
pub use collision::{
    CollisionKind, CollisionPiece, extract_collision, match_collision_target, parse_collision_name,
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
pub use meshlet::{Meshlet, build_lod_meshlets, build_meshlets};
pub use bake_file::{
    LIGHTDATA_EXTENSION, LIGHTMAP_EXTENSION, LightDataFile, LightmapEntry, LightmapFile, LightmapFileInfo, LightmapLayers,
    ProbeVolumeData, load_lightdata, load_lightmap_layers, lightmap_file_info, load_lightmaps, read_lightmap_layers, save_lightdata,
    save_lightmap_layers, save_lightmaps, write_atomic,
};
pub use behaviour_tree::{
    BEHAVIOUR_TREE_EXTENSION, BehaviourTree, load_behaviour_tree, save_behaviour_tree,
};
pub use post_file::{
    Bloom, ChromaticAberration, ColorGrading, POSTFX_EXTENSION, PostFxProfile, TonemapMode,
    Vignette, blend_over, blend_profiles, load_postfx, save_postfx,
};
pub use procedural::{BUCKET_FLOOR, BUCKET_WALL, heightfield_mesh, heightfield_mesh_with_weights, import_heightmap, primitive_mesh, test_scene};
pub use project_file::{
    PROJECT_FILE_NAME, ProjectFile, ProjectSettings, load_project_file, save_project_file,
};
pub use scene_file::{
    ComponentData, EditorCamera, FluxQuality, GiMode, MaterialRef, ModelInstanceRef, ModelPart, ObjectSource, PrimitiveShape,
    ProbeFallback, RealtimeGi, SCENE_EXTENSION, SceneEntry, BakeSettings, SceneFile,
    VoxelGridMode, WorldEnvironment, load_scene_file, save_scene_file,
};
pub use sequencer::{
    SEQUENCER_EXTENSION, SeqChannel, SeqCurve, SeqInterp, SeqKey, SeqTrack, SeqTrackKind, Sequencer,
    load_sequencer, save_sequencer,
};
pub use tex_cache::{
    ASTC_CACHE_EXTENSION, AstcBlocks, astc_blocks, astc_cache_blob, compress_embedded_texture, encode_astc_level, load_texture_bc, seed_astc_cache,
    seed_texture_cache, set_astc_blocks, texture_cache_blob, texture_mean, texture_mean_linear, validate_compressed,
};
pub use shader_file::{
    ALPHA_MODE_CONSTANT_ID, HLSL_SHADER_EXTENSION, ShaderCondOp, ShaderCondTerm, ShaderCondition,
    HLSL_SHADER_TEMPLATE, KeywordSet, MAX_SHADER_KEYWORDS, MAX_SHADER_TEXTURES, PropStorage,
    SHADER_EXTENSION, SHADER_PROP_FLOATS, SHADER_TEMPLATE, SHADER_TEXTURE_SLOT_BASE,
    SHADER_UBO_MAX_BYTES, SHADER_VALUE_BASE, ShaderGroup, ShaderLang, ShaderPreset, ShaderProp,
    ShaderPropKind, ShaderSource, TextureDefault, assemble_shader, bake_shader_variant,
    compile_shader, compile_shader_variant, is_shader_path, keyword_hash, load_baked_variant,
    load_shader_file, parse_shader_source, parse_shader_source_lang, precompile_shader_file,
    scoped_shader_target_env, shader_lang_for, shader_target_env, variant_path, ShaderTargetEnv,
    ShaderTargetEnvGuard,
};
pub use shader_graph::{
    SHADER_GRAPH_EXTENSION, GraphNode, GraphNodeKind, GraphTextureSlot, GraphValueType,
    ShaderGraph, default_graph, generate_glsl, graph_shader_source, infer_types,
    is_shader_graph_path, load_graph, load_shader_graph, save_shader_graph, topo_order, validate,
};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zestengine_render::{MaterialFeatures, MaterialParams, MeshData, TextureData};
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

/// One blendshape (morph target) of a mesh: sparse per-vertex offsets.
#[derive(Clone, Debug, Default)]
pub struct MorphTarget {
    pub name: String,
    /// (vertex index, position offset, normal offset).
    pub deltas: Vec<(u32, [f32; 3], [f32; 3])>,
}

pub struct Scene {
    pub meshes: Vec<MeshData>,
    /// Blendshapes per mesh, parallel to `meshes` (shorter or empty = none).
    pub morphs: Vec<Vec<MorphTarget>>,
    /// VRM spring bones and colliders (empty for other models).
    pub springs: vrm::SpringImport,
    pub textures: Vec<TextureData>,
    pub materials: Vec<SceneMaterial>,
    pub instances: Vec<Instance>,
    /// Imported armatures (one per glTF skin / FBX skin deformer). Vertex joint
    /// indices are skin-local and line up with `skeletons[mesh's skin].joints`.
    pub skeletons: Vec<Skeleton>,
    /// Imported skeletal animation clips (shared across the file's skeletons).
    pub animations: Vec<AnimationClip>,
    /// Authored collision pieces (`UCX_` / `UBX_` / `USP_` / `UCP_` objects),
    /// filled by [`extract_collision`] on load; not in `instances`.
    pub collision: Vec<CollisionPiece>,
}

/// True if the extension is an importable model format.
pub fn is_model_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some("gltf" | "glb" | "vrm" | "fbx")
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
        std::fs::write(&p, b"zestengine-lightmap-uv\n")
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
    let rendered = collision::rendered_meshes(scene);
    for (mesh, rendered) in scene.meshes.iter_mut().zip(rendered) {
        if rendered && !mesh.has_lightmap_uv {
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
        Some("gltf" | "glb" | "vrm") => load_gltf(path),
        Some("fbx") => load_fbx(path),
        other => bail!("unsupported model format {other:?} (gltf, glb, fbx)"),
    }?;
    extract_collision(&mut scene);
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
        Some("gltf" | "glb" | "vrm") => load_gltf(path),
        Some("fbx") => load_fbx_with(path, &model),
        other => bail!("unsupported model format {other:?} (gltf, glb, fbx)"),
    }?;
    extract_collision(&mut scene);
    apply_lightmap_uv_marker(path, &mut scene);
    Ok(scene)
}
