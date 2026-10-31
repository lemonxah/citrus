//! Scene state: objects with TRS + provenance, GPU upload, material
//! management (imported, file-based, and edited), picking, and .scene
//! save/load.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use citrus_assets::{MaterialRef, ObjectSource, SceneEntry, SceneFile};
use citrus_core::{Component, ComponentCtx, ComponentRegistry, ObjectId};
use citrus_core::{AlphaModeModel, MaterialModel};
use citrus_render::{
    AlphaMode, DrawCmd, MaterialDesc, MaterialFeatures, MaterialHandle, MaterialParams, MeshHandle,
    Renderer, TextureHandle,
};
use glam::{Mat4, Quat, Vec3};

use crate::shaders::ShaderLibrary;

/// Per-pass inputs for [`LoadedScene::run_pass`].
pub struct PassEnv<'a> {
    pub dt: f32,
    pub unscaled_dt: f32,
    pub time: f32,
    pub water_time: f32,
    pub time_scale: f32,
    pub input: &'a citrus_core::InputState,
    pub net: &'a citrus_core::NetView,
    pub physics: Option<&'a dyn citrus_core::PhysicsQuery>,
    pub nav: Option<&'a dyn citrus_core::NavQuery>,
    pub events: &'a [citrus_core::AnyBox],
    pub screen: &'a citrus_core::ScreenInfo,
    pub save: &'a citrus_core::SaveGame,
    pub strings: &'a citrus_core::Localization,
}

/// Component lookup for one pass step: every object's components, except the
/// running object, whose components are split around the one executing.
struct PassLookup<'a> {
    objects: &'a [SceneObject],
    self_index: usize,
    before: &'a [Box<dyn Component>],
    after: &'a [Box<dyn Component>],
}

impl citrus_core::ComponentLookup for PassLookup<'_> {
    fn component(&self, object: usize, type_id: std::any::TypeId) -> Option<&dyn Component> {
        let matches = |c: &&dyn Component| c.as_any().type_id() == type_id;
        if object == self.self_index {
            self.before.iter().chain(self.after.iter()).map(|c| c.as_ref()).find(matches)
        } else {
            self.objects.get(object)?.components.iter().map(|c| c.as_ref()).find(matches)
        }
    }
}

/// Render data for mesh objects; empties and cameras have none.
#[derive(Clone, Copy)]
pub struct RenderInfo {
    /// Index into scene-local mesh arrays.
    pub mesh: usize,
    /// Index into `materials`.
    pub material: usize,
}

pub struct SceneObject {
    /// Stable unique identity (assigned at creation, serialized in `.scene`).
    /// Cross-object references use this rather than the name or array index.
    pub id: ObjectId,
    pub name: String,
    pub render: Option<RenderInfo>,
    /// Additional render slots beyond `render` (slot 0). Each is its own
    /// (mesh, material) drawn at the same transform, so one imported mesh can
    /// expose multiple material slots. Empty for single-material objects.
    pub extra_render: Vec<RenderInfo>,
    pub source: ObjectSource,
    /// When false the object is skipped at draw time (and its light, if any,
    /// stops contributing) but it stays in the scene.
    pub enabled: bool,
    /// Marks the object as non-moving so its geometry is included in the
    /// lighting bake (lightmaps + as a ray-trace occluder). Dynamic objects
    /// instead sample baked light probes.
    pub static_geometry: bool,
    /// Per-object lightmap-resolution multiplier ("Scale In Lightmap").
    pub lightmap_scale: f32,
    /// "Cast Shadows": On / Off / Two Sided / Shadows Only.
    pub cast_shadows: citrus_core::ShadowCasting,
    /// Layer index (0..32, Unity-style). Drives the camera culling mask
    /// (rendering) and the layer-collision matrix (physics). 0 = "Default".
    pub layer: u8,
    /// Parent object index; transform is local to it.
    pub parent: Option<usize>,
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
    pub components: Vec<Box<dyn Component>>,
}

impl SceneObject {
    pub fn local_transform(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    /// Iterate every render slot (primary + extras) as `RenderInfo`.
    pub fn render_slots(&self) -> impl Iterator<Item = RenderInfo> + '_ {
        self.render
            .into_iter()
            .chain(self.extra_render.iter().copied())
    }

    /// Serialize all components, switched-off ones included (undo snapshots,
    /// play-mode restore).
    pub fn save_components(&self) -> Vec<citrus_assets::ComponentData> {
        self.components.iter().map(|c| citrus_assets::ComponentData::of(c.as_ref())).collect()
    }

    /// Rebuild the component list from serialized state.
    pub fn load_components(&mut self, saved: &[citrus_assets::ComponentData], registry: &ComponentRegistry) {
        self.components = saved.iter().filter_map(|c| c.load(registry)).collect();
    }

    pub fn kind_label(&self) -> &'static str {
        match self.source {
            ObjectSource::Empty => "Empty",
            ObjectSource::Camera => "Camera",
            ObjectSource::Light => "Light",
            ObjectSource::Primitive { .. } => "Primitive",
            _ => "Mesh",
        }
    }
}

pub struct MaterialEntry {
    pub model: MaterialModel,
    pub default: MaterialModel,
    pub handle: MaterialHandle,
    /// Mean LINEAR RGB of the emission map, cached at apply_material. Scales the
    /// GI emitter + RT-reflection emission so a mostly-black map (e.g. a glowing
    /// visor only) doesn't act as a uniform full-mesh light. `[1,1,1]` = no map.
    /// Derived/runtime only — deliberately NOT on `MaterialModel`, so it stays
    /// out of edit/dirty detection (PartialEq) and undo snapshots.
    pub emission_map_mean: [f32; 3],
    /// Mean diffuse albedo + metalness the LIGHTMAP BAKE uses, computed at
    /// apply_material from the albedo + ORM textures (× the scalar factors). The
    /// bake gbuffer is flat (no per-texel texture sampling), so a textured object
    /// must not bake with just its scalar `base_color`/`metallic` — a glTF PBR
    /// material with an MR map keeps `metallic = 1.0` as a *multiplier*, which
    /// would make the whole surface read fully-metallic (zero diffuse → black
    /// lightmap). These hold the texture-mean-corrected values instead.
    pub bake_albedo: [f32; 3],
    pub bake_metallic: f32,
    /// Set when this material came from (or was saved to) a `.material` file.
    pub file: Option<PathBuf>,
    /// True for imported-model materials whose textures came embedded in the
    /// model file: they can't be expressed in a `.material` file (no paths), so
    /// they stay inline. Retained for a future "convert to asset" path.
    #[allow(dead_code)]
    pub embedded_textures: bool,
    /// Texture handles bound at creation (import-embedded or `.material`-loaded),
    /// in the 12-slot order. Used as the per-slot fallback when the model has no
    /// explicit texture path, so editing one slot doesn't drop the others.
    pub base_textures: TexSlots,
    /// The texture handles currently bound on the descriptor set. apply_material
    /// only rebinds (a GPU-stalling op) when the resolved slots differ from this,
    /// so per-frame param edits (slider drags) don't thrash the textures.
    pub bound_textures: TexSlots,
}

/// The 12 material texture slots in binding order: albedo, normal, orm,
/// emission, opacity, emission_mask, matcap0, matcap0_mask, matcap1,
/// matcap1_mask, matcap2, matcap2_mask.
pub type TexSlots = [Option<TextureHandle>; 16];

#[derive(Clone, Copy)]
pub struct MeshInfo {
    pub vertices: u32,
    pub triangles: u32,
}

/// A CPU-skinned mesh: its renderer mesh (host-visible vertex buffer), the
/// original bind-pose vertices, and which imported skeleton drives it.
struct SkinnedMesh {
    mesh_index: usize,
    base_vertices: Vec<citrus_render::Vertex>,
    skeleton: usize,
    /// Bind-space AABB of the vertices each joint influences, for posed
    /// culling bounds (`posed_bounds`).
    joint_boxes: Vec<(u32, Vec3, Vec3)>,
    /// Blendshapes, driven by Face Tracking / Blend Shapes.
    morphs: Vec<citrus_assets::MorphTarget>,
}

impl SkinnedMesh {
    fn new(mesh_index: usize, base_vertices: Vec<citrus_render::Vertex>, skeleton: usize) -> Self {
        let mut boxes: HashMap<u32, (Vec3, Vec3)> = HashMap::new();
        for v in &base_vertices {
            let p = Vec3::from(v.position);
            for (j, w) in v.joints.iter().zip(v.weights) {
                if w > 0.0 {
                    let b = boxes.entry(*j).or_insert((p, p));
                    b.0 = b.0.min(p);
                    b.1 = b.1.max(p);
                }
            }
        }
        let joint_boxes = boxes.into_iter().map(|(j, (lo, hi))| (j, lo, hi)).collect();
        Self { mesh_index, base_vertices, skeleton, joint_boxes, morphs: Vec::new() }
    }
}

/// The model template a LOD level draws: its own model, or a mesh generated
/// from the object's model (`base`) at the level's triangle share. None =
/// the base mesh.
fn lod_template_key(level: &citrus_core::LodLevel, base: Option<&str>) -> Option<String> {
    if !level.model.is_empty() {
        return Some(level.model.clone());
    }
    let base = base?;
    (level.keep > 0.0 && level.keep < 1.0).then(|| format!("{base}#lod{:.3}", level.keep))
}

/// The renderer's copy of an object's "Cast Shadows" setting.
pub fn render_shadow_casting(c: citrus_core::ShadowCasting) -> citrus_render::ShadowCasting {
    match c {
        citrus_core::ShadowCasting::On => citrus_render::ShadowCasting::On,
        citrus_core::ShadowCasting::Off => citrus_render::ShadowCasting::Off,
        citrus_core::ShadowCasting::TwoSided => citrus_render::ShadowCasting::TwoSided,
        citrus_core::ShadowCasting::ShadowsOnly => citrus_render::ShadowCasting::ShadowsOnly,
    }
}

/// Object-space AABB of a skinned mesh in the pose `palette`: each joint's
/// bind-space vertex box moved by that joint's skinning matrix. A vertex
/// blended between joints lies inside the union of its joints' boxes.
fn posed_bounds(boxes: &[(u32, Vec3, Vec3)], palette: &[Mat4]) -> Option<(Vec3, Vec3)> {
    let mut out: Option<(Vec3, Vec3)> = None;
    for &(j, lo, hi) in boxes {
        let Some(m) = palette.get(j as usize) else {
            continue;
        };
        for k in 0..8 {
            let c = Vec3::new(
                if k & 1 == 0 { lo.x } else { hi.x },
                if k & 2 == 0 { lo.y } else { hi.y },
                if k & 4 == 0 { lo.z } else { hi.z },
            );
            let p = m.transform_point3(c);
            out = Some(match out {
                Some((a, b)) => (a.min(p), b.max(p)),
                None => (p, p),
            });
        }
    }
    out
}

/// An object's own skinned vertex buffers, driven by its `Animator`.
struct AnimInstance {
    /// (source skinned mesh index, this object's GPU copy).
    slots: Vec<(usize, MeshHandle)>,
    /// Skinned by the GPU compute pass (else CPU-written).
    gpu: bool,
}

/// Wire edges of a collider shape (local space), shared.
pub type ColliderWire = std::sync::Arc<Vec<(Vec3, Vec3)>>;

/// A Humanoid IK object as the body fit sees it.
pub(crate) struct HumanoidAvatar<'a> {
    pub index: usize,
    pub skeleton: &'a citrus_assets::Skeleton,
    pub rig: crate::humanoid::HumanoidRig,
    /// Driven by this machine's trackers.
    pub local: bool,
    /// Its Network Avatar (itself or an ancestor).
    pub network_avatar: Option<usize>,
    /// The object its tracking space sits on (the VR Player by default).
    pub tracking_root: Option<usize>,
}

/// The local player's body fit as the rest of the scene reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LocalBody {
    /// Hand reach multiplier for local avatars (arm span matching).
    pub arm_reach: f32,
    /// Avatar scale over its authored size (replicated to other players).
    pub avatar_scale: f32,
    /// Measured standing eye height (m), when known.
    pub eye_height: Option<f32>,
}

impl Default for LocalBody {
    fn default() -> Self {
        Self { arm_reach: 1.0, avatar_scale: 1.0, eye_height: None }
    }
}

/// An IK-posed skeleton in world space.
#[derive(Clone, Debug, Default)]
pub(crate) struct AvatarPose {
    pub joints: Vec<Vec3>,
    pub parents: Vec<Option<usize>>,
}

pub struct LoadedScene {
    /// Rebuilt every frame from renderable objects.
    pub draws: Vec<DrawCmd>,
    pub objects: Vec<SceneObject>,
    pub materials: Vec<MaterialEntry>,
    /// Imported armatures + animation clips (skeletal rigging). Skinned meshes
    /// reference a skeleton by index; the first clip plays on a loop for now.
    skeletons: Vec<citrus_assets::Skeleton>,
    animations: Vec<citrus_assets::AnimationClip>,
    skinned_meshes: Vec<SkinnedMesh>,
    /// Posed object-space bounds of skinned meshes, refreshed by
    /// `update_skinning`: per Animator object (id, mesh), and per shared mesh.
    pose_bounds: HashMap<(ObjectId, usize), (Vec3, Vec3)>,
    /// When each render-texture camera last refreshed (its Update Rate).
    camera_target_last: HashMap<ObjectId, std::time::Instant>,
    shared_pose_bounds: HashMap<usize, (Vec3, Vec3)>,
    /// Skeleton index each entry of `animations` animates (parallel).
    clip_skeleton: Vec<usize>,
    /// Per-object skinned copies for objects with an `Animator`.
    anim_instances: HashMap<ObjectId, AnimInstance>,
    /// Global animation playback clock (seconds), advanced each frame.
    anim_time: f32,
    /// Full-body IK targets for humanoid avatars (from VR trackers, gameplay,
    /// procedural, or network). When set + the rig is humanoid, the avatar is
    /// IK-posed instead of playing the clip. Not VR-specific.
    ik_targets: Option<citrus_core::IkTargets>,
    /// When set, humanoid avatars get terrain foot IK applied on top of their
    /// animated/IK pose each skinning update (feet plant on uneven ground).
    foot_ik: Option<crate::humanoid::FootIkParams>,
    /// The local player's body fit, set by the engine's body tracking.
    pub(crate) local_body: LocalBody,
    /// Record this object's IK-posed skeleton (the body panel's preview).
    pub(crate) avatar_preview_for: Option<usize>,
    pub(crate) avatar_preview: Option<AvatarPose>,
    /// Planted-foot state of each object's Humanoid IK.
    body_ik_state: HashMap<ObjectId, crate::humanoid::BodyIkState>,
    /// This machine's face tracking this frame (headset or OSC).
    local_face: Option<citrus_core::FaceFrame>,
    /// Clock for cloth wind gusts (s).
    cloth_time: f32,
    /// Where cloth cull distances are measured from (None = the main camera).
    cloth_viewer: Option<Vec3>,
    /// Lip sync: the local mic's visemes, and remote players' by the object
    /// their peer owns.
    local_visemes: Option<citrus_core::VisemeFrame>,
    remote_visemes: HashMap<ObjectId, citrus_core::VisemeFrame>,
    mesh_handles: Vec<MeshHandle>,
    mesh_infos: Vec<MeshInfo>,
    mesh_bounds: Vec<(Vec3, Vec3)>,
    /// Parallel to `mesh_bounds`: whether each mesh has a real non-overlapping
    /// lightmap UV (its own UV1 / a generated unwrap), so it's safe to bake. A
    /// mesh whose uv1 is just a uv0 copy is `false` and excluded from the bake +
    /// the UV-checker preview until a lightmap UV is generated for it.
    mesh_has_lightmap_uv: Vec<bool>,
    /// CPU positions + indices kept per mesh for software-GI SDF generation.
    mesh_geometry: Vec<(Vec<Vec3>, Vec<u32>)>,
    /// Lazily-built signed distance field per mesh (software GI). `None` until
    /// first use; index-parallel to `mesh_handles`. `Arc` so the march can run
    /// on a background thread sharing the SDFs.
    mesh_sdf: Vec<Option<std::sync::Arc<citrus_render::sdf::SdfVolume>>>,
    /// Loaded `.postfx` profiles by project-relative path (Volume references).
    /// Cached on first use; reload the scene to pick up external edits.
    postfx_cache: std::collections::HashMap<String, citrus_assets::PostFxProfile>,
    /// Volumes affecting the last `effective_postfx` camera: (object name,
    /// weight, effects its profile overrides). Shown by the Environment tab.
    pub postfx_overrides: Vec<(String, f32, Vec<&'static str>)>,
    primitive_meshes: HashMap<citrus_assets::PrimitiveShape, usize>,
    default_material: Option<usize>,
    /// model path -> base index of its meshes in the scene arrays
    /// (a model's primitives are appended contiguously).
    model_mesh_base: HashMap<PathBuf, usize>,
    /// Imported model path -> (first mesh index, template material per model
    /// mesh). Lets scene entries and runtime prefab spawns reuse uploaded models.
    model_templates: HashMap<String, (usize, Vec<usize>)>,
    /// Each model's parts as imported (scene files store model instances as
    /// their changes against these).
    model_parts: HashMap<String, Vec<citrus_assets::ModelPart>>,
    /// Builtin test-scene meshes: (first mesh index, template materials).
    builtin_template: Option<(usize, Vec<usize>)>,
    material_file_cache: HashMap<PathBuf, usize>,
    texture_file_cache: HashMap<(PathBuf, bool), TextureHandle>,
    /// Project-relative equirectangular skybox image (None = procedural sky).
    pub skybox: Option<String>,
    /// World lighting / environment (ambient + sun + skybox toggle).
    pub environment: citrus_assets::WorldEnvironment,
    /// Layer names + collision matrix (Unity-style). Round-trips with the scene.
    pub layers: citrus_core::LayerSettings,
    /// Render-time layer visibility mask. In the editor this is the viewport's
    /// layer toggle (default all-on); in a running game it's set from the active
    /// camera's `culling_mask`. A draw is skipped when its layer bit is clear.
    pub visible_layers: u32,
    /// Baked lighting result (None until a bake runs). Re-uploaded to the
    /// renderer for runtime sampling.
    pub baked: Option<BakedData>,
    /// Steam Audio bake (serialized probe batch, `<scene>.audiobake`).
    pub audio_bake: Option<std::sync::Arc<Vec<u8>>>,
    /// Baked occlusion culling (`<scene>.occlusion`), if any.
    pub occlusion: Option<crate::occlusion::OcclusionData>,
    /// Position of the camera occlusion culling is evaluated from (set by the
    /// host before `sync_draws`); None disables culling.
    pub occlusion_camera: Option<Vec3>,
    /// The editor's lightmap UV-checker preview is on (draws then carry each
    /// object's lightmap resolution).
    pub lightmap_preview: bool,
    /// Per-frame world transforms / active flags from `sync_draws_multi`.
    frame_cache_world: Vec<Mat4>,
    frame_cache_active: Vec<bool>,
    /// Camera a possessed pawn activated this play session (2A). Overrides the
    /// default "smallest camera id" selection while set. Cleared on Stop.
    pub active_camera_override: Option<ObjectId>,
    /// Draws not tied to an object's render slots (world-UI quads, particles),
    /// refilled by the host every frame and appended by `sync_draws`.
    pub extra_draws: Vec<DrawCmd>,
    /// Splashes and wakes from physics this frame, for the water effects.
    pub water_splashes: Vec<citrus_core::WaterSplash>,
    pub water_wakes: Vec<crate::water_fx::FxWake>,
    /// Pushes from landing water (waterfalls) for Play's ripple step.
    pub water_fx_pushes: Vec<(usize, citrus_core::WaterPush)>,
    /// Water poured from containers, to draw this frame (Play).
    pub water_pours: Vec<crate::water_carry::PourDrop>,
    /// Poured water that hit something (point, velocity, litres): a few drops.
    pub water_drips: Vec<(Vec3, Vec3, f32)>,
    /// Wave Rider offsets this frame (object index -> world offset, tilt),
    /// applied to what's drawn only: the synced transform stays the anchor.
    pub ride_offsets: HashMap<usize, (Vec3, Quat)>,
    /// Mesh Collider shapes built from render meshes (decomposition is slow).
    pub collider_shapes: std::sync::Mutex<HashMap<crate::physics::ColliderShapeKey, std::sync::Arc<crate::physics::MeshColliderShape>>>,
    /// Authored collision pieces (`UCX_` etc.) of the loaded models.
    pub authored_collision: crate::authored_collision::AuthoredCollision,
    /// Their wire edges for the editor gizmo.
    pub collider_wires: std::sync::Mutex<HashMap<crate::physics::ColliderShapeKey, ColliderWire>>,
    /// Camera position for LOD selection (set by the host before `sync_draws`).
    pub lod_camera: Option<Vec3>,
    /// Current LOD level per object (hysteresis).
    lod_state: HashMap<ObjectId, Option<usize>>,
    /// LOD models that failed to load (don't retry every frame).
    lod_failed: std::collections::HashSet<String>,
    /// Scene file each additively loaded object came from (for unloading).
    pub object_origin: HashMap<ObjectId, String>,
    /// Animator Controllers and `.anim` clips in use (loaded on demand).
    pub anim_assets: crate::anim_graph::AnimAssets,
}

/// One probe volume's runtime metadata: where it sits and which SH range it
/// owns, so dynamic objects can trilinearly interpolate.
#[derive(Clone)]
pub struct ProbeVolumeMeta {
    /// World → volume-local (probe grid spans -size/2..+size/2 in local).
    pub world_to_local: Mat4,
    pub size: [f32; 3],
    pub counts: [usize; 3],
    /// First probe index (into `BakedData.probe_sh`) for this volume.
    pub sh_base: usize,
    /// True for FluxVoxel voxel volumes (baked from FluxVoxel Lights). At runtime
    /// these are owned by `realtime_gi::update_flux_voxel` (it seeds its static base
    /// from them, then adds dynamic lights), NOT uploaded as plain DDGI probes.
    pub flux_voxel: bool,
}

/// Baked lighting, kept on the scene and pushed to the renderer for runtime.
#[derive(Clone, Default)]
pub struct BakedData {
    /// Object index → lightmap layer in the renderer's lightmap array.
    pub object_lightmap: std::collections::HashMap<usize, usize>,
    /// Raw RGBA32F maps of a fresh bake (and the live bake preview); emptied
    /// once `lightmap_array` holds the encoded layers.
    pub lightmaps: Vec<citrus_render::BakedLightmap>,
    /// GPU-ready lightmap layers (loaded from `.lightmap`, or the encoded
    /// fresh bake), in this device's upload format. Wins over `lightmaps`.
    pub lightmap_array: Option<std::sync::Arc<citrus_render::HdrTextureArray>>,
    pub probe_volumes: Vec<ProbeVolumeMeta>,
    pub probe_sh: Vec<citrus_render::ProbeSh>,
}

impl BakedData {
    pub fn lightmap_count(&self) -> usize {
        self.lightmap_array.as_ref().map_or(self.lightmaps.len(), |a| a.layers.len())
    }

    /// Edge of the largest lightmap (the array's common layer size once encoded).
    pub fn lightmap_max_size(&self) -> u32 {
        match &self.lightmap_array {
            Some(a) => a.size,
            None => self.lightmaps.iter().map(|l| l.size).max().unwrap_or(0),
        }
    }

    /// VRAM of the lightmap array as uploaded (raw maps upload as RGBA32F).
    pub fn lightmap_vram_bytes(&self) -> u64 {
        match &self.lightmap_array {
            Some(a) => a.byte_size(),
            None => {
                let size = citrus_render::lightmap_array_size(self.lightmaps.iter().map(|l| l.size)) as u64;
                self.lightmaps.len() as u64 * size * size * 16
            }
        }
    }

    pub fn lightmap_format(&self) -> citrus_render::HdrTextureFormat {
        self.lightmap_array.as_ref().map_or(citrus_render::HdrTextureFormat::Rgba32f, |a| a.format)
    }

    /// Push the lightmaps to the renderer (encoded array when present).
    pub fn upload_lightmaps(&self, renderer: &mut citrus_render::Renderer) {
        match &self.lightmap_array {
            Some(a) => renderer.set_lightmap_array(a),
            None => renderer.set_baked_lightmaps(&self.lightmaps),
        }
    }
}

/// Owned bake inputs gathered from the scene; `BakeInput` borrows from this.
pub struct BakeGather {
    pub instances: Vec<citrus_render::BakeInstance>,
    /// Object index per instance (parallel to `instances`).
    pub instance_objects: Vec<usize>,
    /// Whether each instance's object is static (parallel to `instances`).
    /// Realtime GI re-traces only on static-geometry / moving-emitter changes,
    /// so a dynamic prop falling/jittering under physics doesn't keep retracing.
    pub instance_static: Vec<bool>,
    pub lights: Vec<citrus_render::BakeLight>,
    pub probes: Vec<Vec3>,
    pub probe_volumes: Vec<ProbeVolumeMeta>,
    pub sky_color: [f32; 3],
}

impl LoadedScene {
    pub fn empty() -> Self {
        Self {
            draws: Vec::new(),
            objects: Vec::new(),
            materials: Vec::new(),
            skeletons: Vec::new(),
            animations: Vec::new(),
            skinned_meshes: Vec::new(),
            pose_bounds: HashMap::new(),
            camera_target_last: HashMap::new(),
            shared_pose_bounds: HashMap::new(),
            clip_skeleton: Vec::new(),
            anim_instances: HashMap::new(),
            anim_time: 0.0,
            ik_targets: None,
            foot_ik: None,
            local_body: LocalBody::default(),
            avatar_preview_for: None,
            avatar_preview: None,
            body_ik_state: HashMap::new(),
            local_face: None,
            cloth_time: 0.0,
            cloth_viewer: None,
            local_visemes: None,
            remote_visemes: HashMap::new(),
            mesh_geometry: Vec::new(),
            mesh_sdf: Vec::new(),
            postfx_cache: std::collections::HashMap::new(),
            postfx_overrides: Vec::new(),
            mesh_handles: Vec::new(),
            mesh_infos: Vec::new(),
            mesh_bounds: Vec::new(),
            mesh_has_lightmap_uv: Vec::new(),
            primitive_meshes: HashMap::new(),
            default_material: None,
            model_mesh_base: HashMap::new(),
            model_templates: HashMap::new(),
            model_parts: HashMap::new(),
            builtin_template: None,
            extra_draws: Vec::new(),
            water_splashes: Vec::new(),
            water_wakes: Vec::new(),
            water_fx_pushes: Vec::new(),
            water_pours: Vec::new(),
            water_drips: Vec::new(),
            ride_offsets: HashMap::new(),
            collider_shapes: Default::default(),
            collider_wires: Default::default(),
            authored_collision: Default::default(),
            lod_camera: None,
            lod_state: HashMap::new(),
            lod_failed: Default::default(),
            object_origin: HashMap::new(),
            anim_assets: Default::default(),
            material_file_cache: HashMap::new(),
            texture_file_cache: HashMap::new(),
            skybox: None,
            environment: citrus_assets::WorldEnvironment::default(),
            layers: citrus_core::LayerSettings::default(),
            visible_layers: citrus_core::all_layers_mask(),
            baked: None,
            audio_bake: None,
            occlusion: None,
            occlusion_camera: None,
            lightmap_preview: false,
            frame_cache_world: Vec::new(),
            frame_cache_active: Vec::new(),
            active_camera_override: None,
        }
    }

    pub fn mesh_info(&self, mesh: usize) -> MeshInfo {
        self.mesh_infos[mesh]
    }

    /// Center of a mesh's AABB in object space (the "Center" pivot).
    pub fn mesh_center_local(&self, mesh: usize) -> Vec3 {
        let (min, max) = self.mesh_bounds[mesh];
        (min + max) * 0.5
    }

    /// Object-space AABB (min, max) of a mesh, used by physics to fit a
    /// collider to a mesh's extents.
    pub fn mesh_aabb(&self, mesh: usize) -> (Vec3, Vec3) {
        self.mesh_bounds[mesh]
    }

    /// Lightmap resolution (texels/side) this object would bake at under the
    /// current bake settings: `texel_density × world AABB`, clamped to
    /// `max_lightmap`. 0 if it has no mesh. Used by the UV-checker preview and
    /// the bake.
    /// Lightmap resolution for object `i`, reusing an already-computed world
    /// matrix so the per-frame `sync_draws` pass doesn't re-walk each parent chain
    /// just to re-derive a scale it already has (from `world_transforms`).
    pub fn lightmap_size_for_world(&self, i: usize, world: Mat4) -> u32 {
        let Some(render) = self.objects[i].render else {
            return 0;
        };
        let scale = world.to_scale_rotation_translation().0;
        let (min, max) = self.mesh_bounds[render.mesh];
        let extent = (max - min) * scale;
        let max_extent = extent.x.max(extent.y).max(extent.z).max(0.01);
        let s = self.environment.bake;
        let density = s.texel_density * self.objects[i].lightmap_scale.max(0.0);
        // Floor at 64: multi-chart meshes (the cube's 6-face atlas) need enough
        // texels that the inter-chart gutter is ≥2 texels, else bilinear bleeds
        // between charts and shows seam lines.
        ((density * max_extent).round() as u32).clamp(64, s.max_lightmap.max(64))
    }

    /// World transform of an object (walks the parent chain).
    pub fn world_transform(&self, index: usize) -> Mat4 {
        let mut m = self.objects[index].local_transform();
        let mut i = index;
        let mut guard = 0;
        while let Some(p) = self.objects[i].parent {
            if guard > 64 || p >= self.objects.len() {
                break;
            }
            m = self.objects[p].local_transform() * m;
            i = p;
            guard += 1;
        }
        m
    }

    /// World transform of every object, computed in ONE O(n) memoized pass
    /// instead of `world_transform(i)` per object (which re-walks each parent
    /// chain → O(n·depth) when called for the whole scene every frame). Each
    /// entry is `parent_world * local`, resolving parents on demand with a
    /// recursion guard (cycles / dangling parents fall back to the local
    /// transform, matching `world_transform`'s break-out behaviour).
    pub fn world_transforms(&self) -> Vec<Mat4> {
        let n = self.objects.len();
        // 0 = unvisited, 1 = in-progress (cycle sentinel), 2 = done.
        let mut state = vec![0u8; n];
        let mut out = vec![Mat4::IDENTITY; n];
        for i in 0..n {
            self.resolve_world(i, &mut state, &mut out, None);
        }
        out
    }

    /// [`Self::world_transforms`] with the Wave Rider offsets applied (and
    /// carried to children): where things are drawn.
    pub fn render_world_transforms(&self) -> Vec<Mat4> {
        if self.ride_offsets.is_empty() {
            return self.world_transforms();
        }
        let n = self.objects.len();
        let mut state = vec![0u8; n];
        let mut out = vec![Mat4::IDENTITY; n];
        for i in 0..n {
            self.resolve_world(i, &mut state, &mut out, Some(&self.ride_offsets));
        }
        out
    }

    fn resolve_world(&self, i: usize, state: &mut [u8], out: &mut [Mat4], offsets: Option<&HashMap<usize, (Vec3, Quat)>>) -> Mat4 {
        if state[i] == 2 {
            return out[i];
        }
        let local = self.objects[i].local_transform();
        // Mark in-progress so a cycle reaching back to `i` falls back to local
        // (the `state[p] != 1` guard below) instead of recursing forever.
        state[i] = 1;
        // In-progress parent (cycle) or root/dangling parent → treat local as world.
        let world = match self.objects[i].parent {
            Some(p) if p < out.len() && p != i && state[p] != 1 => {
                let pw = self.resolve_world(p, state, out, offsets);
                pw * local
            }
            _ => local,
        };
        let world = match offsets.and_then(|o| o.get(&i)) {
            Some(&(off, tilt)) => {
                let pos = world.w_axis.truncate();
                Mat4::from_translation(pos + off) * Mat4::from_quat(tilt) * Mat4::from_translation(-pos) * world
            }
            None => world,
        };
        state[i] = 2;
        out[i] = world;
        world
    }

    /// World-space bounds of an object: (center, radius). Used for F-focus.
    pub fn object_bounds(&self, index: usize) -> (Vec3, f32) {
        let object = &self.objects[index];
        let world = self.world_transform(index);
        match &object.render {
            Some(r) => {
                let (min, max) = self.mesh_bounds[r.mesh];
                let center = world.transform_point3((min + max) * 0.5);
                let scale = Vec3::new(
                    world.x_axis.length(),
                    world.y_axis.length(),
                    world.z_axis.length(),
                );
                let radius = ((max - min) * 0.5 * scale).length();
                (center, radius.max(0.05))
            }
            None => (world.w_axis.truncate(), 0.5),
        }
    }

    /// Re-parent `child`, preserving its world transform. Rejects cycles.
    /// Put `members` under a new Empty named "Group" at their centre (under
    /// their shared parent, if they have one), keeping their world
    /// transforms. Members inside another member's subtree stay where they
    /// are. Returns the group's index.
    pub fn group_into_empty(&mut self, members: &[usize]) -> Option<usize> {
        let mut picked: Vec<usize> = members.iter().copied().filter(|&m| m < self.objects.len()).collect();
        picked.sort_unstable();
        picked.dedup();
        let is_under = |scene: &Self, mut i: usize, set: &[usize]| {
            while let Some(p) = scene.objects[i].parent {
                if set.contains(&p) {
                    return true;
                }
                i = p;
            }
            false
        };
        let tops: Vec<usize> = picked.iter().copied().filter(|&m| !is_under(self, m, &picked)).collect();
        if tops.is_empty() {
            return None;
        }
        let parent = self.objects[tops[0]].parent.filter(|p| tops.iter().all(|&m| self.objects[m].parent == Some(*p)));
        let centre = tops.iter().map(|&m| self.world_transform(m).w_axis.truncate()).sum::<Vec3>() / tops.len() as f32;
        let parent_world = parent.map_or(Mat4::IDENTITY, |p| self.world_transform(p));
        let ids: Vec<ObjectId> = tops.iter().map(|&m| self.objects[m].id).collect();
        let group_id = ObjectId::new();
        self.objects.push(SceneObject {
            id: group_id,
            name: "Group".into(),
            render: None,
            extra_render: Vec::new(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: citrus_core::ShadowCasting::On,
            layer: 0,
            parent,
            translation: parent_world.inverse().transform_point3(centre),
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components: Vec::new(),
        });
        for id in ids {
            let (Some(child), Some(group)) = (self.index_of(id), self.index_of(group_id)) else { continue };
            self.reorder_object(child, Some(group), None);
        }
        self.index_of(group_id)
    }

    pub fn set_parent(&mut self, child: usize, parent: Option<usize>) {
        if let Some(p) = parent {
            if p == child || p >= self.objects.len() {
                return;
            }
            // Reject if `child` is an ancestor of `p`.
            let mut i = p;
            let mut guard = 0;
            while let Some(pp) = self.objects[i].parent {
                if pp == child {
                    return;
                }
                i = pp;
                guard += 1;
                if guard > 64 {
                    return;
                }
            }
        }
        let world = self.world_transform(child);
        self.objects[child].parent = parent;
        let parent_world = parent.map_or(Mat4::IDENTITY, |p| self.world_transform(p));
        let local = parent_world.inverse() * world;
        let (scale, rotation, translation) = local.to_scale_rotation_translation();
        let o = &mut self.objects[child];
        o.translation = translation;
        o.rotation = rotation;
        o.scale = scale;
    }

    /// Move `child` (and its subtree) under `new_parent`, inserted in the
    /// objects Vec immediately before `before` (None = append at the end of
    /// the new parent's children). Preserves world transform and remaps every
    /// positional parent index. Returns the old→new index map (empty if the
    /// move was rejected), so callers can fix up selection indices.
    pub fn reorder_object(
        &mut self,
        child: usize,
        new_parent: Option<usize>,
        before: Option<usize>,
    ) -> Vec<usize> {
        let len = self.objects.len();
        if child >= len {
            return Vec::new();
        }
        // Reject cycles: new_parent must not be `child` or a descendant of it.
        if let Some(p) = new_parent {
            if p >= len || p == child {
                return Vec::new();
            }
            let mut i = p;
            let mut guard = 0;
            while let Some(pp) = self.objects[i].parent {
                if pp == child {
                    return Vec::new();
                }
                i = pp;
                guard += 1;
                if guard > 64 {
                    break;
                }
            }
        }

        // Re-parent, preserving the world transform.
        let world = self.world_transform(child);
        self.objects[child].parent = new_parent;
        let parent_world = new_parent.map_or(Mat4::IDENTITY, |p| self.world_transform(p));
        let local = parent_world.inverse() * world;
        let (scale, rotation, translation) = local.to_scale_rotation_translation();
        {
            let o = &mut self.objects[child];
            o.translation = translation;
            o.rotation = rotation;
            o.scale = scale;
        }

        // The moving block = child's subtree in display (DFS pre-order).
        let moving = self.subtree_preorder(child);
        let mut in_moving = vec![false; len];
        for &m in &moving {
            in_moving[m] = true;
        }
        let rest: Vec<usize> = (0..len).filter(|i| !in_moving[*i]).collect();
        let insert_at = match before {
            Some(b) if b < len && !in_moving[b] => {
                rest.iter().position(|&i| i == b).unwrap_or(rest.len())
            }
            _ => rest.len(),
        };
        let mut new_order = Vec::with_capacity(len);
        new_order.extend_from_slice(&rest[..insert_at]);
        new_order.extend_from_slice(&moving);
        new_order.extend_from_slice(&rest[insert_at..]);

        let mut map = vec![0usize; len];
        for (ni, &oi) in new_order.iter().enumerate() {
            map[oi] = ni;
        }
        // Rebuild the Vec in the new order, then remap every parent index.
        let mut slots: Vec<Option<SceneObject>> = self.objects.drain(..).map(Some).collect();
        let mut rebuilt = Vec::with_capacity(len);
        for &oi in &new_order {
            rebuilt.push(slots[oi].take().unwrap());
        }
        self.objects = rebuilt;
        for o in &mut self.objects {
            o.parent = o.parent.map(|p| map[p]);
        }
        map
    }

    /// Indices of `root` and its descendants, in DFS pre-order (== display
    /// order, since children iterate in ascending index).
    fn subtree_preorder(&self, root: usize) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_subtree(root, &mut out);
        out
    }

    fn collect_subtree(&self, root: usize, out: &mut Vec<usize>) {
        out.push(root);
        for i in 0..self.objects.len() {
            if self.objects[i].parent == Some(root) {
                self.collect_subtree(i, out);
            }
        }
    }

    fn ensure_default_material(&mut self, renderer: &mut Renderer) -> Result<usize> {
        if let Some(i) = self.default_material {
            return Ok(i);
        }
        let desc = citrus_render::MaterialDesc {
            name: "Default".into(),
            params: MaterialParams::default(),
            features: MaterialFeatures::default(),
            albedo: None,
            normal: None,
            orm: None,
            emission: None,
            error: false,
            ..Default::default()
        };
        let handle = renderer.create_material(&desc)?;
        let model = model_from_material(
            "Default",
            &MaterialParams::default(),
            &MaterialFeatures::default(),
            false,
        );
        self.materials.push(MaterialEntry {
            default: model.clone(),
            model,
            handle,
            emission_map_mean: [1.0; 3],
            bake_albedo: [1.0; 3],
            bake_metallic: 0.0,
            file: None,
            embedded_textures: false,
            base_textures: TexSlots::default(),
            bound_textures: TexSlots::default(),
        });
        let index = self.materials.len() - 1;
        self.default_material = Some(index);
        Ok(index)
    }

    fn ensure_primitive_mesh(
        &mut self,
        renderer: &mut Renderer,
        shape: citrus_assets::PrimitiveShape,
    ) -> Result<usize> {
        if let Some(&i) = self.primitive_meshes.get(&shape) {
            return Ok(i);
        }
        let data = citrus_assets::primitive_mesh(shape);
        self.mesh_handles.push(renderer.upload_mesh(&data)?);
        self.mesh_geometry.push((
            data.vertices.iter().map(|v| Vec3::from(v.position)).collect(),
            data.indices.clone(),
        ));
        self.mesh_sdf.push(None);
        self.mesh_infos.push(MeshInfo {
            vertices: data.vertices.len() as u32,
            triangles: data.indices.len() as u32 / 3,
        });
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for v in &data.vertices {
            min = min.min(Vec3::from(v.position));
            max = max.max(Vec3::from(v.position));
        }
        self.mesh_bounds.push((min, max));
        self.mesh_has_lightmap_uv.push(data.has_lightmap_uv);
        let index = self.mesh_handles.len() - 1;
        self.primitive_meshes.insert(shape, index);
        Ok(index)
    }

    /// Spawn an empty / camera / primitive. Returns the new object index.
    pub fn spawn(
        &mut self,
        renderer: &mut Renderer,
        source: ObjectSource,
        name: String,
        position: Vec3,
    ) -> Result<usize> {
        let render = match &source {
            ObjectSource::Primitive { shape } => {
                let mesh = self.ensure_primitive_mesh(renderer, *shape)?;
                let material = self.ensure_default_material(renderer)?;
                Some(RenderInfo { mesh, material })
            }
            _ => None,
        };
        self.objects.push(SceneObject {
            id: ObjectId::new(),
            name,
            render,
            extra_render: Vec::new(),
            source,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: citrus_core::ShadowCasting::On,
            layer: 0,
            parent: None,
            translation: position,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components: Vec::new(),
        });
        Ok(self.objects.len() - 1)
    }

    /// Upload an imported asset scene, appending its content. `source_path`
    /// is recorded for .scene provenance (None = built-in test scene).
    pub fn add_asset_scene(
        &mut self,
        renderer: &mut Renderer,
        scene: &citrus_assets::Scene,
        source_path: Option<&Path>,
    ) -> Result<()> {
        let mesh_base = self.mesh_handles.len();
        if let Some(path) = source_path {
            self.model_mesh_base.insert(path.to_owned(), mesh_base);
        }
        self.authored_collision.register(scene, mesh_base, source_path.map(|p| p.to_string_lossy()).as_deref());

        let textures: Vec<_> = scene
            .textures
            .iter()
            .map(|t| renderer.upload_texture(t))
            .collect::<Result<_>>()?;
        // Carry imported armatures + animation clips (skeletal rigging). Joint
        // indices on the vertices are skin-local, matching `skeletons[*].joints`;
        // a skinned mesh references skeleton 0 (the common single-armature case).
        let skel_base = self.skeletons.len();
        self.skeletons.extend(scene.skeletons.iter().cloned());
        self.animations.extend(scene.animations.iter().cloned());
        self.clip_skeleton
            .extend(std::iter::repeat_n(skel_base, scene.animations.len()));
        let has_skeleton = !scene.skeletons.is_empty();
        for (mesh_i, mesh) in scene.meshes.iter().enumerate() {
            // A mesh is skinned if any vertex carries skin weights and the model
            // brought a skeleton. Skinned meshes get a host-visible buffer + a
            // CPU-skinning record so they animate each frame.
            let skinned = has_skeleton && mesh.vertices.iter().any(|v| v.weights.iter().sum::<f32>() > 0.0);
            if skinned {
                let handle = renderer.upload_mesh_skinned(mesh)?;
                self.mesh_handles.push(handle);
                let mut sm = SkinnedMesh::new(self.mesh_handles.len() - 1, mesh.vertices.clone(), skel_base);
                sm.morphs = scene.morphs.get(mesh_i).cloned().unwrap_or_default();
                self.skinned_meshes.push(sm);
            } else {
                self.mesh_handles.push(renderer.upload_mesh(mesh)?);
            }
            self.mesh_geometry.push((
                mesh.vertices.iter().map(|v| Vec3::from(v.position)).collect(),
                mesh.indices.clone(),
            ));
            self.mesh_sdf.push(None);
            self.mesh_infos.push(MeshInfo {
                vertices: mesh.vertices.len() as u32,
                triangles: mesh.indices.len() as u32 / 3,
            });
            let mut min = Vec3::splat(f32::INFINITY);
            let mut max = Vec3::splat(f32::NEG_INFINITY);
            for v in &mesh.vertices {
                min = min.min(Vec3::from(v.position));
                max = max.max(Vec3::from(v.position));
            }
            self.mesh_bounds.push((min, max));
            self.mesh_has_lightmap_uv.push(mesh.has_lightmap_uv);
        }

        let material_base = self.materials.len();
        for material in &scene.materials {
            let desc = MaterialDesc {
                name: material.name.clone(),
                params: material.params,
                features: material.features,
                albedo: material.albedo.map(|i| textures[i]),
                normal: material.normal.map(|i| textures[i]),
                orm: material.orm.map(|i| textures[i]),
                emission: material.emission.map(|i| textures[i]),
                error: false,
                ..Default::default()
            };
            let handle = renderer.create_material(&desc)?;
            let model = model_from_material(
                &material.name,
                &material.params,
                &material.features,
                material.normal.is_some(),
            );
            // Slot order matches TexSlots / create_material. Imported models
            // only carry albedo/normal/orm/emission; the rest are unset.
            let mut base_textures = TexSlots::default();
            base_textures[0] = material.albedo.map(|i| textures[i]);
            base_textures[1] = material.normal.map(|i| textures[i]);
            base_textures[2] = material.orm.map(|i| textures[i]);
            base_textures[3] = material.emission.map(|i| textures[i]);
            self.materials.push(MaterialEntry {
                default: model.clone(),
                model,
                handle,
                emission_map_mean: [1.0; 3],
                bake_albedo: [1.0; 3],
                bake_metallic: 0.0,
                file: None,
                embedded_textures: material.albedo.is_some()
                    || material.normal.is_some()
                    || material.orm.is_some()
                    || material.emission.is_some(),
                base_textures,
                bound_textures: base_textures,
            });
        }

        // Group all of the model's objects under a single root (named after the
        // file), so an imported model is one entry in the Scene tree you can move
        // as a unit, and the loaders' per-material mesh split stays tidy.
        if let Some(p) = source_path {
            self.model_parts.entry(p.to_string_lossy().into_owned()).or_insert_with(|| citrus_assets::ModelPart::of(scene));
        }
        let root_index = self.objects.len();
        let root_name = source_path
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Model".to_string());
        self.objects.push(SceneObject {
            id: ObjectId::new(),
            name: root_name,
            render: None,
            extra_render: Vec::new(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: citrus_core::ShadowCasting::On,
            layer: 0,
            parent: None,
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components: Vec::new(),
        });

        let first_new = self.objects.len();
        for instance in &scene.instances {
            let Some(&slot0) = instance.slots.first() else {
                continue;
            };
            let (scale, rotation, translation) = instance.transform.to_scale_rotation_translation();
            let render = RenderInfo {
                mesh: mesh_base + slot0.mesh,
                material: material_base + slot0.material,
            };
            // Slots beyond the first become extra render slots / extra meshes.
            let extra_render: Vec<RenderInfo> = instance.slots[1..]
                .iter()
                .map(|s| RenderInfo {
                    mesh: mesh_base + s.mesh,
                    material: material_base + s.material,
                })
                .collect();
            let extra_meshes: Vec<usize> = instance.slots[1..].iter().map(|s| s.mesh).collect();
            let source = match source_path {
                Some(path) => ObjectSource::Model {
                    path: path.to_string_lossy().into_owned(),
                    mesh: slot0.mesh,
                    extra_meshes,
                },
                None => ObjectSource::Builtin { mesh: slot0.mesh },
            };
            self.objects.push(SceneObject {
                id: ObjectId::new(),
                name: instance.name.clone(),
                render: Some(render),
                extra_render,
                source,
                enabled: true,
                static_geometry: false,
                lightmap_scale: 1.0,
                cast_shadows: citrus_core::ShadowCasting::On,
                layer: 0,
                parent: Some(root_index),
                translation,
                rotation,
                scale,
                components: Vec::new(),
            });
        }
        self.attach_springs(first_new, &scene.springs);
        Ok(())
    }

    /// Like `add_asset_scene` but the meshes/textures were already uploaded on a
    /// loader thread and installed into the renderer at `mesh_base_g`/`tex_base_g`
    /// (renderer-global indices), with `skinned` flagging skinned meshes. Builds
    /// the scene metadata + materials + objects on the main thread (no uploads).
    pub fn add_installed_asset(
        &mut self,
        renderer: &mut Renderer,
        scene: &citrus_assets::Scene,
        mesh_handles: &[MeshHandle],
        tex_handles: &[TextureHandle],
        skinned: &[bool],
        source_path: Option<&Path>,
    ) -> Result<()> {
        let mesh_base = self.mesh_handles.len();
        if let Some(path) = source_path {
            self.model_mesh_base.insert(path.to_owned(), mesh_base);
        }
        self.authored_collision.register(scene, mesh_base, source_path.map(|p| p.to_string_lossy()).as_deref());
        let textures: &[TextureHandle] = tex_handles;
        let skel_base = self.skeletons.len();
        self.skeletons.extend(scene.skeletons.iter().cloned());
        self.animations.extend(scene.animations.iter().cloned());
        self.clip_skeleton
            .extend(std::iter::repeat_n(skel_base, scene.animations.len()));
        for (i, mesh) in scene.meshes.iter().enumerate() {
            self.mesh_handles.push(mesh_handles[i]);
            if skinned.get(i).copied().unwrap_or(false) {
                let mut sm = SkinnedMesh::new(self.mesh_handles.len() - 1, mesh.vertices.clone(), skel_base);
                sm.morphs = scene.morphs.get(i).cloned().unwrap_or_default();
                self.skinned_meshes.push(sm);
            }
            self.mesh_geometry.push((
                mesh.vertices.iter().map(|v| Vec3::from(v.position)).collect(),
                mesh.indices.clone(),
            ));
            self.mesh_sdf.push(None);
            self.mesh_infos.push(MeshInfo {
                vertices: mesh.vertices.len() as u32,
                triangles: mesh.indices.len() as u32 / 3,
            });
            let mut min = Vec3::splat(f32::INFINITY);
            let mut max = Vec3::splat(f32::NEG_INFINITY);
            for v in &mesh.vertices {
                min = min.min(Vec3::from(v.position));
                max = max.max(Vec3::from(v.position));
            }
            self.mesh_bounds.push((min, max));
            self.mesh_has_lightmap_uv.push(mesh.has_lightmap_uv);
        }

        let material_base = self.materials.len();
        for material in &scene.materials {
            let desc = MaterialDesc {
                name: material.name.clone(),
                params: material.params,
                features: material.features,
                albedo: material.albedo.map(|i| textures[i]),
                normal: material.normal.map(|i| textures[i]),
                orm: material.orm.map(|i| textures[i]),
                emission: material.emission.map(|i| textures[i]),
                error: false,
                ..Default::default()
            };
            let handle = renderer.create_material(&desc)?;
            let model = model_from_material(
                &material.name,
                &material.params,
                &material.features,
                material.normal.is_some(),
            );
            let mut base_textures = TexSlots::default();
            base_textures[0] = material.albedo.map(|i| textures[i]);
            base_textures[1] = material.normal.map(|i| textures[i]);
            base_textures[2] = material.orm.map(|i| textures[i]);
            base_textures[3] = material.emission.map(|i| textures[i]);
            self.materials.push(MaterialEntry {
                default: model.clone(),
                model,
                handle,
                emission_map_mean: [1.0; 3],
                bake_albedo: [1.0; 3],
                bake_metallic: 0.0,
                file: None,
                embedded_textures: material.albedo.is_some()
                    || material.normal.is_some()
                    || material.orm.is_some()
                    || material.emission.is_some(),
                base_textures,
                bound_textures: base_textures,
            });
        }

        if let Some(p) = source_path {
            self.model_parts.entry(p.to_string_lossy().into_owned()).or_insert_with(|| citrus_assets::ModelPart::of(scene));
        }
        let root_index = self.objects.len();
        let root_name = source_path
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Model".to_string());
        self.objects.push(SceneObject {
            id: ObjectId::new(),
            name: root_name,
            render: None,
            extra_render: Vec::new(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: citrus_core::ShadowCasting::On,
            layer: 0,
            parent: None,
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components: Vec::new(),
        });
        let first_new = self.objects.len();
        for instance in &scene.instances {
            let Some(&slot0) = instance.slots.first() else {
                continue;
            };
            let (scale, rotation, translation) = instance.transform.to_scale_rotation_translation();
            let render = RenderInfo {
                mesh: mesh_base + slot0.mesh,
                material: material_base + slot0.material,
            };
            let extra_render: Vec<RenderInfo> = instance.slots[1..]
                .iter()
                .map(|s| RenderInfo {
                    mesh: mesh_base + s.mesh,
                    material: material_base + s.material,
                })
                .collect();
            let extra_meshes: Vec<usize> = instance.slots[1..].iter().map(|s| s.mesh).collect();
            let source = match source_path {
                Some(path) => ObjectSource::Model {
                    path: path.to_string_lossy().into_owned(),
                    mesh: slot0.mesh,
                    extra_meshes,
                },
                None => ObjectSource::Builtin { mesh: slot0.mesh },
            };
            self.objects.push(SceneObject {
                id: ObjectId::new(),
                name: instance.name.clone(),
                render: Some(render),
                extra_render,
                source,
                enabled: true,
                static_geometry: false,
                lightmap_scale: 1.0,
                cast_shadows: citrus_core::ShadowCasting::On,
                layer: 0,
                parent: Some(root_index),
                translation,
                rotation,
                scale,
                components: Vec::new(),
            });
        }
        self.attach_springs(first_new, &scene.springs);
        Ok(())
    }

    /// Load (or fetch cached) a `.material` file as a material entry.
    /// Returns the material index. Broken files yield an error-shader
    /// material so the problem is visible in the viewport.
    pub fn material_from_file(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        path: &Path,
        project_root: &Path,
    ) -> usize {
        if let Some(&index) = self.material_file_cache.get(path) {
            return index;
        }
        let index = match self.try_load_material_file(renderer, shaders, path, project_root) {
            Ok(index) => index,
            Err(e) => {
                tracing::error!("loading material {}: {e:#}", path.display());
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "broken".into());
                let desc = MaterialDesc {
                    name: format!("{name} (error)"),
                    params: MaterialParams::default(),
                    features: MaterialFeatures::default(),
                    albedo: None,
                    normal: None,
                    orm: None,
                    emission: None,
                    error: true,
                    ..Default::default()
                };
                let handle = renderer.create_material(&desc).expect("material pool full");
                let model = model_from_material(
                    &format!("{name} (error)"),
                    &MaterialParams::default(),
                    &MaterialFeatures::default(),
                    false,
                );
                self.materials.push(MaterialEntry {
                    default: model.clone(),
                    model,
                    handle,
                    emission_map_mean: [1.0; 3],
                    bake_albedo: [1.0; 3],
                    bake_metallic: 0.0,
                    file: Some(path.to_owned()),
                    embedded_textures: false,
                    base_textures: TexSlots::default(),
                    bound_textures: TexSlots::default(),
                });
                self.materials.len() - 1
            }
        };
        self.material_file_cache.insert(path.to_owned(), index);
        index
    }

    fn try_load_material_file(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        path: &Path,
        project_root: &Path,
    ) -> Result<usize> {
        let file = citrus_assets::load_material_file(path)?;
        let camera_sizes: HashMap<ObjectId, [u32; 2]> = [&file.textures.albedo, &file.textures.emission]
            .into_iter()
            .flatten()
            .filter_map(|p| camera_texture_id(p))
            .map(|id| (id, self.camera_texture_size(id)))
            .collect();
        let mut load_tex = |slot: &Option<PathBuf>, srgb: bool| -> Result<Option<TextureHandle>> {
            let Some(rel) = slot else { return Ok(None) };
            // `camera:<object id>`: a render-texture camera's live target.
            if let Some(id) = camera_texture_id(rel) {
                let size = camera_sizes.get(&id).copied().unwrap_or([512, 512]);
                return renderer.camera_target(id.raw(), size[0], size[1]).map(Some);
            }
            let abs = citrus_core::asset_ref::resolve_rel_path(project_root, rel);
            if let Some(&handle) = self.texture_file_cache.get(&(abs.clone(), srgb)) {
                return Ok(Some(handle));
            }
            let data = citrus_assets::load_texture_file(&abs, srgb)?;
            let handle = renderer.upload_texture(&data)?;
            self.texture_file_cache.insert((abs, srgb), handle);
            Ok(Some(handle))
        };
        let albedo = load_tex(&file.textures.albedo, true)?;
        let normal = load_tex(&file.textures.normal, false)?;
        let orm = load_tex(&file.textures.orm, false)?;
        let emission = load_tex(&file.textures.emission, true)?;
        let opacity = load_tex(&file.textures.opacity, false)?;
        let emission_mask = load_tex(&file.textures.emission_mask, false)?;
        let matcap = [
            load_tex(&file.textures.matcap[0], true)?,
            load_tex(&file.textures.matcap[1], true)?,
            load_tex(&file.textures.matcap[2], true)?,
        ];
        let matcap_mask = [
            load_tex(&file.textures.matcap_mask[0], false)?,
            load_tex(&file.textures.matcap_mask[1], false)?,
            load_tex(&file.textures.matcap_mask[2], false)?,
        ];
        let ao = load_tex(&file.textures.ao, false)?;
        let roughness = load_tex(&file.textures.roughness, false)?;
        let metallic = load_tex(&file.textures.metallic, false)?;
        let displacement = load_tex(&file.textures.displacement, false)?;

        let desc = MaterialDesc {
            name: file.name.clone(),
            params: file.params,
            features: file.features,
            albedo,
            normal,
            orm,
            emission,
            opacity,
            emission_mask,
            matcap,
            matcap_mask,
            ao,
            roughness,
            metallic,
            displacement,
            error: false,
        };
        let handle = renderer.create_material(&desc)?;
        let mut model =
            model_from_material(&file.name, &file.params, &file.features, normal.is_some());
        model.shader = file.shader.clone();
        if let Some(q) = file.render_queue {
            model.render_queue = q;
        }
        // Surface the file's texture assignments in the editable model.
        model.textures = tex_paths_from_file(&file.textures);
        if file.shader != "standard" {
            let entry = shaders.resolve(renderer, project_root, &file.shader);
            if let Some(source) = &entry.source {
                model.custom_values = source.pack(&file.custom).to_vec();
            }
        }
        // File-backed materials are driven entirely by `model.textures` (so a
        // slot can be cleared); no embedded fallback. The bindings created above
        // get re-resolved from the same paths by apply_material below. Record
        // them so that resolve matches and apply_material skips a redundant
        // (GPU-stalling) rebind.
        let bound_textures = [
            albedo,
            normal,
            orm,
            emission,
            opacity,
            emission_mask,
            matcap[0],
            matcap_mask[0],
            matcap[1],
            matcap_mask[1],
            matcap[2],
            matcap_mask[2],
            ao,
            roughness,
            metallic,
            displacement,
        ];
        self.materials.push(MaterialEntry {
            default: model.clone(),
            model,
            handle,
            emission_map_mean: [1.0; 3],
            bake_albedo: [1.0; 3],
            bake_metallic: 0.0,
            file: Some(path.to_owned()),
            embedded_textures: false,
            base_textures: TexSlots::default(),
            bound_textures,
        });
        let index = self.materials.len() - 1;
        self.apply_material(renderer, shaders, project_root, index);
        Ok(index)
    }

    /// The material index of one of an object's render slots (0 = primary).
    pub fn slot_material(&self, object: usize, slot: usize) -> Option<usize> {
        let obj = self.objects.get(object)?;
        if slot == 0 {
            obj.render.map(|r| r.material)
        } else {
            obj.extra_render.get(slot - 1).map(|r| r.material)
        }
    }

    /// Set the material index of one of an object's render slots (0 = primary).
    pub fn set_slot_material(&mut self, object: usize, slot: usize, material: usize) {
        let Some(obj) = self.objects.get_mut(object) else {
            return;
        };
        if slot == 0 {
            if let Some(r) = &mut obj.render {
                r.material = material;
            }
        } else if let Some(r) = obj.extra_render.get_mut(slot - 1) {
            r.material = material;
        }
    }

    /// Assign a `.material` file to one of an object's material slots (slot 0 =
    /// the primary `render`, 1.. = `extra_render`). Out-of-range slots are
    /// ignored.
    pub fn assign_material(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        object: usize,
        slot: usize,
        path: &Path,
        project_root: &Path,
    ) {
        let material = self.material_from_file(renderer, shaders, path, project_root);
        let obj = &mut self.objects[object];
        if slot == 0 {
            if let Some(render) = &mut obj.render {
                render.material = material;
            }
        } else if let Some(r) = obj.extra_render.get_mut(slot - 1) {
            r.material = material;
        }
    }

    /// Mean linear RGB of a project-relative texture, or `[1,1,1]` without one.
    /// Disk-cached by citrus-assets (warmed by the scene loader's worker), so
    /// this never decodes on the main thread after the first load.
    fn texture_mean(project_root: &Path, rel: Option<&PathBuf>, srgb: bool) -> [f32; 3] {
        rel.map_or([1.0; 3], |rel| citrus_assets::texture_mean(citrus_core::asset_ref::resolve_rel_path(project_root, rel), srgb))
    }

    /// Push one material's inspector model into the renderer, resolving its
    /// shader (compiling custom shaders on first use).
    pub fn apply_material(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        project_root: &Path,
        index: usize,
    ) {
        // Resolve and rebind the 12 texture slots first, so `has_normal_texture`
        // is current before the feature gate (normal_map) reads it below.
        let handle = self.materials[index].handle;
        let tex_paths = self.materials[index].model.textures.clone();
        let base = self.materials[index].base_textures;
        let slots = self.resolve_texture_slots(renderer, project_root, &tex_paths, &base);
        if slots != self.materials[index].bound_textures {
            renderer.set_material_textures(handle, &slots);
            self.materials[index].bound_textures = slots;
        }
        // Keep the model's normal-map flag in sync with the resolved slot so the
        // inspector enables the Normal Strength slider once a normal map is
        // assigned (slot 1), and disables it again when cleared.
        self.materials[index].model.has_normal_texture = slots[1].is_some();

        // Cache the emission map's mean linear colour so the GI emitter can scale
        // its flux by how much of the surface actually emits (a mostly-black map
        // → a dim, visor-tinted emitter, not a uniform full-mesh light). The
        // emission MASK modulates emission the same way in the shader, so fold its
        // coverage (mean of the .r channel) into the same factor — otherwise a
        // small masked emissive region still acts as a full-flux GI emitter.
        let emission_path = self.materials[index].model.textures.emission.clone();
        let mask_path = self.materials[index].model.textures.emission_mask.clone();
        let map_mean = Self::texture_mean(project_root, emission_path.as_ref(), true);
        let mask_mean = Self::texture_mean(project_root, mask_path.as_ref(), false)[0];
        let emission_mean = [
            map_mean[0] * mask_mean,
            map_mean[1] * mask_mean,
            map_mean[2] * mask_mean,
        ];
        self.materials[index].emission_map_mean = emission_mean;

        // Bake albedo/metalness from the texture means × scalars (the lightmap
        // bake gbuffer is flat, so it can't sample per-texel — see MaterialEntry).
        // glTF PBR keeps metallic=1.0 as a *multiplier* on the MR map, so without
        // this a textured helmet would bake fully metallic (zero diffuse → black).
        let albedo_path = self.materials[index].model.textures.albedo.clone();
        let orm_path = self.materials[index].model.textures.orm.clone();
        let albedo_mean = Self::texture_mean(project_root, albedo_path.as_ref(), true);
        renderer.set_material_albedo_mean(handle, albedo_mean);
        // ORM .b = metalness.
        let orm_metallic_mean = Self::texture_mean(project_root, orm_path.as_ref(), false)[2];
        let bc = self.materials[index].model.base_color;
        let bake_albedo = [
            albedo_mean[0] * bc[0],
            albedo_mean[1] * bc[1],
            albedo_mean[2] * bc[2],
        ];
        let bake_metallic = (orm_metallic_mean * self.materials[index].model.metallic).clamp(0.0, 1.0);
        self.materials[index].bake_albedo = bake_albedo;
        self.materials[index].bake_metallic = bake_metallic;

        let entry = &self.materials[index];
        let m = &entry.model;
        let params = renderer.material_params_mut(handle);
        params.base_color = m.base_color;
        params.metallic = m.metallic;
        params.roughness = m.roughness;
        params.occlusion_strength = m.occlusion_strength;
        params.toon_steps = m.toon_steps;
        params.pbr_toon_blend = m.pbr_toon_blend;
        params.emission_color = m.emission_color;
        params.emission_intensity = m.emission_intensity;
        params.emission_map_mean = emission_mean;
        params.alpha_cutoff = m.alpha_cutoff;
        params.normal_strength = m.normal_strength;
        // Parallax-occlusion displacement: only active when a height map is bound
        // (else POM would march a flat white map and shift nothing). Without this
        // line the live param stayed 0 and displacement never applied.
        params.displacement_scale = if m.textures.displacement.is_some() {
            m.displacement_scale
        } else {
            0.0
        };
        params.rim_color = m.rim_color;
        params.rim_power = m.rim_power;
        params.rim_strength = m.rim_strength;
        params.ramp_smoothness = m.ramp_smoothness;
        params.emission_scroll = m.emission_scroll;
        params.emission_pulse = m.emission_pulse;
        // Per-texture UV tiling + offset. These were missing here, so editing (or
        // reapplying) a material reset its tiling to the default 1×/0 — "tiling not
        // working". apply_material is the live update path, so it MUST copy them.
        params.albedo_tiling = m.albedo_tiling;
        params.albedo_offset = m.albedo_offset;
        params.normal_tiling = m.normal_tiling;
        params.normal_offset = m.normal_offset;
        params.orm_tiling = m.orm_tiling;
        params.orm_offset = m.orm_offset;
        params.emission_tiling = m.emission_tiling;
        params.emission_offset = m.emission_offset;
        params.matcap_strength = m.matcap_strength;
        params.matcap_blend = [
            m.matcap_blend[0].to_f32(),
            m.matcap_blend[1].to_f32(),
            m.matcap_blend[2].to_f32(),
        ];
        renderer.set_material_features(handle, features_from_model(m));
        renderer.set_material_render_queue(handle, m.render_queue);
        // Push the extended FX params into the material's UBO (set 1, binding 4).
        renderer.upload_material_fx(handle);

        if m.shader == "standard" {
            renderer.set_material_shader(handle, None);
            renderer.set_material_outline(handle, None);
            renderer.set_material_error(handle, false);
            return;
        }
        let shader = m.shader.clone();
        let shader_entry = shaders.resolve(renderer, project_root, &shader);
        let (Some(_), Some(source)) = (shader_entry.id, shader_entry.source.clone()) else {
            // Broken/missing shader: error swirl.
            renderer.set_material_error(handle, true);
            return;
        };
        {
            let model = &mut self.materials[index].model;
            shader_entry.fit_values(&mut model.custom_values);
            shader_entry.sync_textures(&mut model.textures);
        }
        let values = self.materials[index].model.custom_values.clone();
        // v1 push lanes (the 16th, d3.w, is the engine-set lightmap layer).
        renderer.set_material_custom_data(handle, source.push_data(&values));
        // v2: std140 material block + texture properties (set 2).
        if source.ubo_size > 0 {
            renderer.set_material_custom_block(handle, &source.uniform_bytes(&values));
        }
        let tex_paths = self.materials[index].model.textures.shader.clone();
        let mut slots = Vec::new();
        for prop in source.textures() {
            let citrus_assets::ShaderPropKind::Texture { default, srgb, .. } = prop.kind else {
                continue;
            };
            let path = tex_paths.iter().find(|(n, _)| *n == prop.name).and_then(|(_, p)| p.clone());
            let tex = path.and_then(|rel| match self.load_texture_cached(renderer, project_root, &rel, srgb) {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::error!("loading texture {}: {e:#}", rel.display());
                    None
                }
            });
            slots.push((tex, crate::shaders::texture_default(default)));
        }
        let keywords = source.keywords(&values);
        let id = shaders.variant(renderer, project_root, &shader, &keywords);
        renderer.set_material_shader(handle, id);
        if !slots.is_empty() {
            renderer.set_material_custom_textures(handle, &slots);
        }
        renderer.set_material_custom_keywords(handle, keywords);
        renderer.set_material_outline(handle, custom_outline(&source, &values));
        renderer.set_material_error(handle, id.is_none());
    }

    /// Load a project-relative texture, caching by (absolute path, srgb). A
    /// `camera:<object id>` path is that render-texture camera's live target.
    fn load_texture_cached(
        &mut self,
        renderer: &mut Renderer,
        project_root: &Path,
        rel: &Path,
        srgb: bool,
    ) -> Result<TextureHandle> {
        if let Some(id) = camera_texture_id(rel) {
            let size = self.camera_texture_size(id);
            return renderer.camera_target(id.raw(), size[0], size[1]);
        }
        let abs = citrus_core::asset_ref::resolve_rel_path(project_root, rel);
        if let Some(&handle) = self.texture_file_cache.get(&(abs.clone(), srgb)) {
            return Ok(handle);
        }
        // BC import cache: decode + compress once (cached to disk), then upload
        // the compressed mip chain. The loader thread normally pre-seeds the
        // cache for scene materials; this path covers the single-queue fallback
        // and textures assigned at runtime (e.g. via the inspector).
        let tex = citrus_assets::load_texture_bc(&abs, srgb)?;
        let handle = renderer.upload_compressed_texture(&tex)?;
        self.texture_file_cache.insert((abs, srgb), handle);
        Ok(handle)
    }

    /// Resolve a material's 12 texture slots: an explicit model path (loaded and
    /// cached) overrides the per-slot `base` fallback (import-embedded handles).
    /// Colour slots (albedo/emission/matcaps) load as sRGB; data slots
    /// (normal/orm/opacity/masks) load linear.
    fn resolve_texture_slots(
        &mut self,
        renderer: &mut Renderer,
        project_root: &Path,
        paths: &citrus_core::MaterialTexturePaths,
        base: &TexSlots,
    ) -> TexSlots {
        let order = texture_slot_order(paths);
        let mut slots = *base;
        for (i, (path, srgb)) in order.iter().enumerate() {
            let Some(rel) = path else { continue };
            match self.load_texture_cached(renderer, project_root, rel, *srgb) {
                Ok(handle) => slots[i] = Some(handle),
                Err(e) => tracing::error!("loading texture {}: {e:#}", rel.display()),
            }
        }
        slots
    }

    /// Collect every (absolute path, srgb) texture the scene's materials
    /// reference, so a loader thread can decode + upload them off the main thread
    /// (the 4K EXR/PNG decode is otherwise the longest blocking step on load).
    /// Walks each entry's material refs (file + inline); model-embedded textures
    /// are handled separately via the model `PreparedScene`. De-duplicated by
    /// (path, srgb) — the same key `load_texture_cached` uses.
    pub fn collect_material_texture_refs(
        file: &SceneFile,
        project_root: &Path,
    ) -> Vec<(PathBuf, bool)> {
        Self::collect_texture_refs(file, project_root, texture_slot_order)
    }

    /// Textures `apply_material` averages (albedo, emission, emission mask,
    /// ORM), so the loader worker can warm their mean cache.
    pub fn collect_material_mean_refs(file: &SceneFile, project_root: &Path) -> Vec<(PathBuf, bool)> {
        Self::collect_texture_refs(file, project_root, |p| {
            [
                (p.albedo.clone(), true),
                (p.emission.clone(), true),
                (p.emission_mask.clone(), false),
                (p.orm.clone(), false),
            ]
        })
    }

    fn collect_texture_refs<I: IntoIterator<Item = (Option<PathBuf>, bool)>>(
        file: &SceneFile,
        project_root: &Path,
        slots: impl Fn(&citrus_core::MaterialTexturePaths) -> I,
    ) -> Vec<(PathBuf, bool)> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut add = |paths: &citrus_core::MaterialTexturePaths| {
            for (rel, srgb) in slots(paths)
                .into_iter()
                .filter_map(|(p, s)| p.map(|p| (p, s)))
                .filter(|(p, _)| camera_texture_id(p).is_none())
            {
                let abs = citrus_core::asset_ref::resolve_rel_path(project_root, &rel);
                if seen.insert((abs.clone(), srgb)) {
                    out.push((abs, srgb));
                }
            }
        };
        for entry in &file.entries {
            let refs = std::iter::once(&entry.material).chain(entry.extra_materials.iter());
            for mref in refs {
                match mref {
                    MaterialRef::File(path) => {
                        let abs = citrus_core::asset_ref::resolve_path(project_root, path);
                        if let Ok(mf) = citrus_assets::load_material_file(&abs) {
                            add(&tex_paths_from_file(&mf.textures));
                        }
                    }
                    MaterialRef::Inline { textures, .. } => {
                        add(&tex_paths_from_file(textures));
                    }
                    MaterialRef::Model => {}
                }
            }
        }
        out
    }

    /// Link a material to a `.material` file it was saved to (auto-save of
    /// previously file-less materials).
    pub fn set_material_file(&mut self, index: usize, path: PathBuf) {
        self.material_file_cache.insert(path.clone(), index);
        self.materials[index].file = Some(path);
    }

    /// Re-apply every material that uses one of `changed` shaders (hot
    /// reload).
    pub fn reapply_materials_using(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        project_root: &Path,
        changed: &[String],
    ) {
        for index in 0..self.materials.len() {
            if changed.contains(&self.materials[index].model.shader) {
                let shader = self.materials[index].model.shader.clone();
                if let Some(entry) = shaders.get(&shader) {
                    entry.remap_after_reload(&mut self.materials[index].model.custom_values);
                }
                self.apply_material(renderer, shaders, project_root, index);
            }
        }
    }

    /// Make sure every camera object carries a Camera component (spawned
    /// and legacy-scene cameras alike).
    pub fn ensure_camera_components(&mut self, registry: &ComponentRegistry) {
        for object in &mut self.objects {
            if matches!(object.source, ObjectSource::Camera)
                && !object.components.iter().any(|c| c.type_name() == "Camera")
                && let Some(camera) = registry.create("Camera")
            {
                object.components.push(camera);
            }
        }
    }

    /// Make sure every light object carries a Light component (a default
    /// directional one for legacy/edited scenes that lost it).
    pub fn ensure_light_components(&mut self, registry: &ComponentRegistry) {
        for object in &mut self.objects {
            if matches!(object.source, ObjectSource::Light)
                && !object.components.iter().any(|c| c.type_name() == "Light")
                && let Some(light) = registry.create("Light")
            {
                object.components.push(light);
            }
        }
    }

    /// Assign a stable, unique id to every camera that doesn't have one yet
    /// (id 0 = unassigned). New ids continue past the current maximum, so the
    /// oldest camera keeps the smallest id (and stays "main") across spawns
    /// and reloads. Returns true if any id was assigned (so the caller can
    /// persist the change).
    pub fn ensure_camera_ids(&mut self) -> bool {
        use citrus_core::CameraComponent;
        let mut max_id = 0u32;
        for object in &self.objects {
            for c in &object.components {
                if let Some(cam) = c.as_any().downcast_ref::<CameraComponent>() {
                    max_id = max_id.max(cam.id);
                }
            }
        }
        let mut changed = false;
        for object in &mut self.objects {
            for c in &mut object.components {
                if let Some(cam) = c.as_any_mut().downcast_mut::<CameraComponent>()
                    && cam.id == 0
                {
                    max_id += 1;
                    cam.id = max_id;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Headset play space of the first active `VR Player` (its world position
    /// and yaw), so stereo rendering follows the game's locomotion.
    #[cfg_attr(not(feature = "editor"), allow(dead_code))]
    pub fn vr_player_rig(&self) -> Option<citrus_core::VrRig> {
        let active = self.active_flags();
        let i = (0..self.objects.len()).find(|&i| {
            active[i] && self.objects[i].components.iter().any(|c| c.as_any().is::<citrus_core::VrPlayer>())
        })?;
        let (scale, rot, origin) = self.world_transform(i).to_scale_rotation_translation();
        let f = rot * Vec3::NEG_Z;
        let scale = ((scale.x + scale.y + scale.z) / 3.0).max(1e-3);
        Some(citrus_core::VrRig { origin, yaw: (-f.x).atan2(-f.z), scale })
    }

    /// Object index of the "main" camera: the one with the smallest camera id.
    /// Run [`ensure_camera_ids`](Self::ensure_camera_ids) first so every camera
    /// has an id.
    pub fn main_camera(&self) -> Option<usize> {
        use citrus_core::CameraComponent;
        // A pawn-activated camera (2A) wins while set and still present.
        if let Some(id) = self.active_camera_override
            && let Some(i) = self.objects.iter().position(|o| {
                o.id == id && o.components.iter().any(|c| c.as_any().is::<CameraComponent>())
            })
        {
            return Some(i);
        }
        let mut best: Option<(usize, u32)> = None;
        for (i, object) in self.objects.iter().enumerate() {
            if let Some(cam) = object
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<CameraComponent>())
                && !cam.render_texture
                && best.is_none_or(|(_, id)| cam.id < id)
            {
                best = Some((i, cam.id));
            }
        }
        best.map(|(i, _)| i)
    }

    /// View, projection, and world position of the main camera for the given
    /// aspect ratio (cameras look down -Z, the glTF convention).
    pub fn main_camera_view_proj(&self, aspect: f32) -> Option<(Mat4, Mat4, Vec3)> {
        self.camera_view_proj_for(self.main_camera()?, aspect)
    }

    /// Layer culling mask of the main camera (Unity-style); all layers when the
    /// scene has no camera. Drives render culling in the shipped game runtime.
    pub fn main_camera_culling_mask(&self) -> u32 {
        use citrus_core::CameraComponent;
        self.main_camera()
            .and_then(|i| {
                self.objects[i]
                    .components
                    .iter()
                    .find_map(|c| c.as_any().downcast_ref::<CameraComponent>())
            })
            .map(|c| c.culling_mask)
            .unwrap_or(citrus_core::all_layers_mask())
    }

    /// Render-texture size of the camera with object id `id` (default 512²).
    fn camera_texture_size(&self, id: ObjectId) -> [u32; 2] {
        self.index_of(id)
            .and_then(|i| self.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::CameraComponent>()))
            .map_or([512, 512], |c| c.texture_size)
    }

    /// Every active render-texture camera whose target exists, with its view
    /// for this frame (recorded before the main pass).
    pub fn camera_target_views(&mut self) -> Vec<citrus_render::CameraTargetView> {
        use citrus_core::CameraComponent;
        let now = std::time::Instant::now();
        let mut due = Vec::new();
        for i in 0..self.objects.len() {
            if !self.frame_active(i) {
                continue;
            }
            let Some(cam) = self.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<CameraComponent>()) else {
                continue;
            };
            if !cam.render_texture {
                continue;
            }
            // Update Rate: hold the last picture until the next refresh is due.
            let id = self.objects[i].id;
            if cam.update_hz > 0.0
                && self.camera_target_last.get(&id).is_some_and(|t| now.duration_since(*t).as_secs_f32() < 1.0 / cam.update_hz)
            {
                continue;
            }
            self.camera_target_last.insert(id, now);
            due.push(i);
        }
        due.into_iter()
            .filter_map(|i| {
                let cam = self.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<CameraComponent>())?;
                let aspect = cam.texture_size[0].max(1) as f32 / cam.texture_size[1].max(1) as f32;
                let (view, proj, position) = self.camera_view_proj_for(i, aspect)?;
                Some(citrus_render::CameraTargetView {
                    id: self.objects[i].id.raw(),
                    camera: citrus_render::CameraData { view, proj, position },
                    size: cam.texture_size,
                })
            })
            .collect()
    }

    /// View/proj/position for the camera on object `i` (None if it has no
    /// CameraComponent). Used for the selected-camera viewport preview.
    pub fn camera_view_proj_for(&self, i: usize, aspect: f32) -> Option<(Mat4, Mat4, Vec3)> {
        use citrus_core::CameraComponent;
        let cam = self
            .objects
            .get(i)?
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<CameraComponent>())?;
        let world = self.world_transform(i);
        let (_, rotation, position) = world.to_scale_rotation_translation();
        let forward = rotation * Vec3::NEG_Z;
        let up = rotation * Vec3::Y;
        let view = Mat4::look_to_rh(position, forward, up);
        let near = cam.near.max(0.001);
        let far = cam.far.max(near + 0.001);
        let proj = Mat4::perspective_rh(cam.fov_deg.to_radians(), aspect.max(0.01), near, far);
        Some((view, proj, position))
    }

    /// Run one lifecycle pass over the components of every active object for
    /// which `filter(index)` holds. `call` gets the object index. Each object's
    /// components are detached while it runs so the context can expose read
    /// access to every other component (`ctx.get`), including siblings.
    pub fn run_pass(
        &mut self,
        env: &PassEnv,
        commands: &mut Vec<citrus_core::ComponentCommand>,
        filter: impl Fn(usize) -> bool,
        mut call: impl FnMut(usize, &mut Box<dyn Component>, &mut ComponentCtx),
    ) {
        // Snapshot at the start of the pass; stale-by-one-pass for objects
        // updated earlier in it, which is fine for references.
        let world_transforms: Vec<Mat4> = self.world_transforms();
        let object_names: Vec<String> = self.objects.iter().map(|o| o.name.clone()).collect();
        let object_ids: Vec<ObjectId> = self.objects.iter().map(|o| o.id).collect();
        let id_index = self.id_index();
        let active = self.active_flags();
        let spawn_points: Vec<(usize, String)> = self
            .objects
            .iter()
            .enumerate()
            .filter_map(|(i, o)| {
                o.components
                    .iter()
                    .find_map(|c| c.as_any().downcast_ref::<citrus_core::SpawnPoint>())
                    .map(|sp| (i, sp.tag.clone()))
            })
            .collect();
        for index in 0..self.objects.len() {
            if !active[index] || !filter(index) || self.objects[index].components.is_empty() {
                continue;
            }
            let parent_world = self.objects[index]
                .parent
                .and_then(|p| world_transforms.get(p).copied());
            let mut components = std::mem::take(&mut self.objects[index].components);
            let mut translation = self.objects[index].translation;
            let mut rotation = self.objects[index].rotation;
            let mut scale = self.objects[index].scale;
            for k in 0..components.len() {
                let (before, rest) = components.split_at_mut(k);
                let (current, after) = rest.split_first_mut().expect("k < len");
                let lookup = PassLookup {
                    objects: &self.objects,
                    self_index: index,
                    before,
                    after,
                };
                call(
                    index,
                    current,
                    &mut ComponentCtx {
                        dt: env.dt,
                        unscaled_dt: env.unscaled_dt,
                        time: env.time,
                        water_time: env.water_time,
                        translation: &mut translation,
                        rotation: &mut rotation,
                        scale: &mut scale,
                        commands,
                        world_transforms: &world_transforms,
                        object_names: &object_names,
                        object_ids: &object_ids,
                        self_index: index,
                        parent_world,
                        input: env.input,
                        net: env.net,
                        spawn_points: &spawn_points,
                        id_index: &id_index,
                        active: &active,
                        lookup: &lookup,
                        physics: env.physics,
                        nav: env.nav,
                        events: env.events,
                        time_scale: env.time_scale,
                        screen: env.screen,
                        save: env.save,
                        strings: env.strings,
                    },
                );
            }
            let object = &mut self.objects[index];
            object.components = components;
            object.translation = translation;
            object.rotation = rotation;
            object.scale = scale;
        }
    }

    /// Stable id -> current index.
    pub fn id_index(&self) -> HashMap<ObjectId, usize> {
        self.objects.iter().enumerate().map(|(i, o)| (o.id, i)).collect()
    }

    pub fn index_of(&self, id: ObjectId) -> Option<usize> {
        self.objects.iter().position(|o| o.id == id)
    }

    /// `is_active` for every object in one pass (parents resolved once).
    pub fn active_flags(&self) -> Vec<bool> {
        self.active_flags_revealing(None)
    }

    /// Like [`Self::active_flags`], but `reveal` and its ancestors count as
    /// enabled (the editor shows a hidden menu page while it's selected).
    pub fn active_flags_revealing(&self, reveal: Option<usize>) -> Vec<bool> {
        let n = self.objects.len();
        let mut revealed = Vec::new();
        let mut k = reveal.filter(|&r| r < n);
        while let Some(i) = k {
            if revealed.contains(&i) || revealed.len() >= 64 {
                break;
            }
            revealed.push(i);
            k = self.objects[i].parent.filter(|&p| p < n);
        }
        // 0 = unknown, 1 = inactive, 2 = active.
        let mut state = vec![0u8; n];
        for i in 0..n {
            let mut chain = Vec::new();
            let mut cur = i;
            let resolved = loop {
                if state[cur] != 0 {
                    break state[cur];
                }
                if !self.objects[cur].enabled && !revealed.contains(&cur) {
                    break 1;
                }
                chain.push(cur);
                match self.objects[cur].parent {
                    Some(p) if p < n && chain.len() < 64 && !chain.contains(&p) => cur = p,
                    _ => break 2,
                }
            };
            for c in chain {
                state[c] = resolved;
            }
            if state[i] == 0 {
                state[i] = resolved;
            }
        }
        state.into_iter().map(|s| s == 2).collect()
    }

    /// CPU-side triangle geometry of a mesh (positions, indices), if kept.
    pub fn mesh_geometry(&self, mesh: usize) -> Option<(&[Vec3], &[u32])> {
        self.mesh_geometry
            .get(mesh)
            .filter(|(p, i)| !p.is_empty() && !i.is_empty())
            .map(|(p, i)| (p.as_slice(), i.as_slice()))
    }

    /// Apply `ComponentCommand::SetActiveCamera` (2A): make the camera object the
    /// active render camera while playing.
    pub fn set_active_camera(&mut self, id: ObjectId) {
        self.active_camera_override = Some(id);
    }

    /// Map each owning peer to the world position of the object it owns, so a
    /// remote peer's voice plays from where their avatar is (spatial voice).
    pub fn peer_voice_positions(
        &self,
        owners: &[(ObjectId, u64)],
    ) -> std::collections::HashMap<u64, Vec3> {
        let mut m = std::collections::HashMap::new();
        let mut from_avatar = std::collections::HashSet::new();
        for (id, peer) in owners {
            let Some(i) = self.objects.iter().position(|o| o.id == *id) else { continue };
            // A Network Avatar speaks from its head (else its replicated head target).
            let head = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<citrus_core::NetworkAvatar>())
                .and_then(|av| {
                    av.head
                        .id()
                        .and_then(|h| self.index_of(h))
                        .map(|h| self.world_transform(h).w_axis.truncate())
                        .or(av.targets.head.map(|(p, _)| p))
                });
            match head {
                Some(p) => {
                    m.insert(*peer, p);
                    from_avatar.insert(*peer);
                }
                None if !from_avatar.contains(peer) => {
                    m.insert(*peer, self.world_transform(i).w_axis.truncate());
                }
                None => {}
            }
        }
        m
    }

    /// Give the first skinned object added from a model its VRM spring bones:
    /// Bone Cloth per chain and the colliders they use.
    fn attach_springs(&mut self, first_new: usize, springs: &citrus_assets::vrm::SpringImport) {
        if springs.is_empty() {
            return;
        }
        let Some(i) = (first_new..self.objects.len()).find(|&i| self.has_skinned_mesh(i)) else { return };
        let comps = &mut self.objects[i].components;
        if !springs.colliders.is_empty() {
            comps.push(Box::new(citrus_core::ClothColliders { colliders: springs.colliders.clone(), ..Default::default() }));
        }
        for chain in &springs.chains {
            comps.push(Box::new(citrus_core::BoneCloth {
                roots: chain.roots.clone(),
                stiffness: [chain.stiffness, chain.stiffness],
                damping: chain.damping,
                gravity: chain.gravity,
                radius: [chain.radius, chain.radius],
                colliders: chain.colliders.clone(),
                ..Default::default()
            }));
        }
        tracing::info!("imported {} VRM spring chains and {} colliders", springs.chains.len(), springs.colliders.len());
    }

    /// Place every Wave Rider on the water at water clock `t`: fills each
    /// rider's state and the draw-only offsets. With `drift`, steep waves and
    /// current move the anchors of riders `owns` says this client controls
    /// (in play; the owner's motion then replicates like any transform).
    pub fn update_wave_riders(&mut self, t: f32, dt: f32, drift: bool, owns: &dyn Fn(ObjectId) -> bool) {
        self.ride_offsets.clear();
        let has_riders = self.objects.iter().any(|o| o.components.iter().any(|c| c.as_any().is::<citrus_core::WaveRider>()));
        if !has_riders {
            return;
        }
        let world = self.world_transforms();
        let active = self.active_flags();
        let waters: Vec<(Mat4, citrus_core::Water)> = (0..self.objects.len())
            .filter(|&i| active[i])
            .filter_map(|i| {
                self.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::Water>()).map(|w| (world[i], w.clone()))
            })
            .collect();
        let mut moves = Vec::new();
        for (i, o) in self.objects.iter_mut().enumerate() {
            if !active[i] {
                continue;
            }
            let Some(rider) = o.components.iter_mut().find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::WaveRider>()) else { continue };
            let anchor = world[i].w_axis.truncate();
            let state = waters.iter().find_map(|(m, w)| rider.ride(w, *m, anchor, t)).unwrap_or_default();
            rider.state = state;
            if state.in_water {
                self.ride_offsets.insert(i, (state.offset, state.tilt));
                if drift && state.on_surface && state.drift.length_squared() > 1e-8 && owns(o.id) {
                    moves.push((i, state.drift * dt));
                }
            }
        }
        for (i, delta) in moves {
            let local = match self.objects[i].parent {
                Some(p) if p < world.len() => world[p].inverse().transform_vector3(delta),
                _ => delta,
            };
            self.objects[i].translation += local;
        }
    }

    /// Advance the wind clock and let every Water body feel the Wind Zones
    /// at its centre (call once per frame, before `update_skinning`).
    pub fn update_water_wind(&mut self, dt: f32) {
        self.cloth_time += dt;
        let t = self.cloth_time;
        let has_water = self.objects.iter().any(|o| o.components.iter().any(|c| c.as_any().is::<citrus_core::Water>()));
        if !has_water {
            return;
        }
        let world = self.world_transforms();
        let active = self.active_flags();
        let winds: Vec<citrus_core::WindSample> = self
            .objects
            .iter()
            .enumerate()
            .filter(|(i, _)| active[*i])
            .filter_map(|(i, o)| {
                o.components
                    .iter()
                    .find_map(|c| c.as_any().downcast_ref::<citrus_core::ClothWind>())
                    .map(|w| citrus_core::WindSample::new(w, world[i]))
            })
            .collect();
        for (i, o) in self.objects.iter_mut().enumerate() {
            let Some(water) = o.components.iter_mut().find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::Water>()) else {
                continue;
            };
            let centre = world[i].w_axis.truncate();
            let w: Vec3 = winds.iter().map(|z| z.at(centre, t)).sum();
            let local = world[i].inverse().transform_vector3(w);
            water.feel_wind(glam::Vec2::new(local.x, local.z), dt);
        }
    }

    /// [`Self::to_scene_file`] with model instances stored as their changes
    /// (what the editor writes to disk).
    pub fn to_compact_scene_file(&self, project_root: &Path, shaders: &ShaderLibrary) -> SceneFile {
        let mut file = self.to_scene_file(project_root, shaders);
        file.compact_models(&|p| self.model_parts.get(p).cloned());
        file
    }

    /// The scene's Voice Chat settings (None = voice off).
    pub fn voice_settings(&self) -> Option<citrus_core::VoiceChat> {
        self.objects
            .iter()
            .enumerate()
            .filter(|(i, _)| self.is_active(*i))
            .find_map(|(_, o)| o.components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::VoiceChat>()).cloned())
    }

    /// Lip sync input for this frame: the local mic's visemes and each remote
    /// peer's (by peer id; matched to their Network Avatars through `owners`).
    pub fn set_visemes(
        &mut self,
        local: Option<citrus_core::VisemeFrame>,
        peers: &std::collections::HashMap<u64, citrus_core::VisemeFrame>,
        owners: &[(ObjectId, u64)],
    ) {
        self.local_visemes = local;
        self.remote_visemes.clear();
        for (id, peer) in owners {
            if let Some(v) = peers.get(peer) {
                self.remote_visemes.insert(*id, *v);
            }
        }
    }

    /// Apply `ComponentCommand::SetLocalTransform`: write another object's local
    /// TRS (used by pawns driving a child camera).
    pub fn set_local_transform(
        &mut self,
        id: ObjectId,
        translation: Option<Vec3>,
        rotation: Option<Quat>,
        scale: Option<Vec3>,
    ) {
        if let Some(o) = self.objects.iter_mut().find(|o| o.id == id) {
            if let Some(t) = translation {
                o.translation = t;
            }
            if let Some(r) = rotation {
                o.rotation = r;
            }
            if let Some(s) = scale {
                o.scale = s;
            }
        }
    }

    /// Replicate networked objects (2G): `NetworkObject` / `Sync` transforms,
    /// variables, animator parameters and avatar poses (see `net_sync`).
    pub fn network_sync(&mut self, session: &mut crate::net::NetSession, dt: f32) {
        crate::net_sync::sync(self, session, dt);
    }

    /// True if the object and all of its ancestors are enabled (a disabled
    /// parent hides its whole subtree).
    /// World transform from this frame's cache (filled by `sync_draws_multi`),
    /// falling back to a parent walk. For the per-frame readers that run after
    /// it (lights, post volumes, probes, audio).
    pub fn frame_world(&self, index: usize) -> Mat4 {
        if self.frame_cache_world.len() == self.objects.len() {
            self.frame_cache_world[index]
        } else {
            self.world_transform(index)
        }
    }

    /// `is_active` from this frame's cache (see [`frame_world`]).
    pub fn frame_active(&self, index: usize) -> bool {
        if self.frame_cache_active.len() == self.objects.len() {
            self.frame_cache_active[index]
        } else {
            self.is_active(index)
        }
    }

    pub fn is_active(&self, index: usize) -> bool {
        let mut i = index;
        let mut guard = 0;
        loop {
            if !self.objects[i].enabled {
                return false;
            }
            match self.objects[i].parent {
                Some(p) if p < self.objects.len() && guard < 64 => {
                    i = p;
                    guard += 1;
                }
                _ => return true,
            }
        }
    }

    /// Collect every `Light` component into renderer light instances, reading
    /// world position/orientation from each object's transform. Baked lights
    /// are included for now (the bake path isn't built yet), so scenes don't
    /// go dark.
    pub fn gather_lights(&self) -> Vec<citrus_render::LightInstance> {
        // Once a bake exists, Baked + Mixed lights are represented by it
        // (lightmaps/probes), so drop them from the realtime loop to avoid
        // double-counting. Realtime lights are always kept — including in the
        // FluxVoxel backend, where they now direct-light the forward pass
        // (previously dropped on the assumption the voxel grid covered them, which
        // broke once the auto-grid became mutually exclusive with placed volumes),
        // so realtime lights work everywhere a voxel volume may not cover.
        self.gather_lights_impl(self.lightmaps_in_use())
    }

    /// Whether the bake's lightmaps light static surfaces. Realtime GI no
    /// longer replaces them: with a bake loaded, Flux / FluxRT run in mixed mode
    /// and add only what the bake can't hold (moving emitters, realtime lights).
    pub fn lightmaps_in_use(&self) -> bool {
        self.baked.is_some()
    }

    /// All scene lights, **including Baked/Mixed even when a bake exists** — for
    /// the reflection-probe cube capture, which renders the scene fresh (no
    /// lightmaps) and so must light it with the analytic lights or the captured
    /// cube comes out dark (a baked scene's main lights would otherwise vanish
    /// from the reflection). No double-counting risk: the cube is a separate
    /// render, not composited with the lightmapped main pass.
    pub fn gather_lights_all(&self) -> Vec<citrus_render::LightInstance> {
        self.gather_lights_impl(false)
    }

    /// `drop_baked`: when a bake exists, flag Baked/Mixed lights so the shader
    /// applies them only to non-lightmapped objects (no double-count). Realtime
    /// lights are always kept and direct-light every backend, FluxVoxel included.
    fn gather_lights_impl(&self, drop_baked: bool) -> Vec<citrus_render::LightInstance> {
        use citrus_core::{LightComponent, LightKind, LightMode};
        let mut lights = Vec::new();
        for i in 0..self.objects.len() {
            if !self.frame_active(i) {
                continue;
            }
            let Some(light) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<LightComponent>())
            else {
                continue;
            };
            // Once a bake exists, Baked/Mixed lights are folded into the lightmaps.
            // Rather than DROP them (which left dynamic / non-lightmapped objects
            // pitch black in a baked scene), keep them flagged `baked`: the shader
            // applies them ONLY to objects with no lightmap, so static lightmapped
            // surfaces don't double-count while dynamic objects still get lit. They
            // don't cast realtime shadows (kept out of the shadow atlas).
            let is_baked = drop_baked && light.mode != LightMode::Realtime;
            let world = self.frame_world(i);
            let (_, rotation, position) = world.to_scale_rotation_translation();
            // Forward (-Z) is the light's travel direction.
            let direction = rotation * Vec3::NEG_Z;
            let kind = match light.kind {
                LightKind::Directional => citrus_render::LightKind::Directional,
                LightKind::Point => citrus_render::LightKind::Point,
                LightKind::Spot => citrus_render::LightKind::Spot,
            };
            lights.push(citrus_render::LightInstance {
                kind,
                position,
                direction,
                color: light.color,
                intensity: light.intensity,
                range: light.range,
                spot_inner_deg: light.spot_angle * (1.0 - light.spot_blend),
                spot_outer_deg: light.spot_angle,
                cast_shadows: light.shadow_type.casts() && !is_baked,
                soft_shadows: light.shadow_type.soft(),
                shadow_bias: light.shadow_bias,
                baked: is_baked,
            });
        }
        lights
    }

    /// Load the `.lightmap` / `.lightdata` bake sidecars sitting next to a scene
    /// into `self.baked` (clearing it if neither file exists). `base` is the
    /// scene path with its extension removed (e.g. `scenes/world`). Shared by
    /// the editor and the game runtime so both light the scene from a bake.
    pub fn load_bake_sidecars(&mut self, base: &std::path::Path) {
        self.baked = Self::read_bake_sidecars(base);
    }

    /// Read + parse the `.lightmap` / `.lightdata` sidecars (no GPU work, so a
    /// loader thread can do it). None when the scene has no bake.
    pub fn read_bake_sidecars(base: &std::path::Path) -> Option<BakedData> {
        let lm_path = base.with_extension("lightmap");
        let ld_path = base.with_extension("lightdata");
        if !lm_path.exists() && !ld_path.exists() {
            return None;
        }
        let mut baked = BakedData::default();
        // v1 (raw) or v2 (BC6H) .lightmap, transcoded for this GPU.
        match citrus_assets::load_lightmap_layers(&lm_path, citrus_render::HdrFormatSupport::current(), 1) {
            Ok(lm) => {
                for (layer, &object) in lm.objects.iter().enumerate() {
                    baked.object_lightmap.insert(object as usize, layer);
                }
                baked.lightmap_array = Some(std::sync::Arc::new(lm.array));
            }
            Err(e) if lm_path.exists() => tracing::error!("loading {}: {e:#}", lm_path.display()),
            Err(_) => {}
        }
        if let Ok(ld) = citrus_assets::load_lightdata(&ld_path) {
            baked.probe_volumes = ld
                .volumes
                .iter()
                .map(|v| ProbeVolumeMeta {
                    world_to_local: Mat4::from_cols_array(&v.world_to_local),
                    size: v.size,
                    counts: [v.counts[0] as usize, v.counts[1] as usize, v.counts[2] as usize],
                    sh_base: v.sh_base as usize,
                    flux_voxel: v.flux_voxel,
                })
                .collect();
            baked.probe_sh = ld
                .probes
                .iter()
                .map(|p| citrus_render::ProbeSh {
                    coeffs: [
                        [p[0], p[1], p[2]],
                        [p[3], p[4], p[5]],
                        [p[6], p[7], p[8]],
                        [p[9], p[10], p[11]],
                    ],
                    // Baked sidecars carry no visibility moments, so disabled.
                    dist: [0.0; 4],
                    dist2: [0.0; 4],
                    offset: [0.0; 3],
                })
                .collect();
        }
        tracing::info!(
            "loaded baked lighting: {} lightmap(s) ({}), {} probe(s)",
            baked.lightmap_count(),
            baked.lightmap_format().label(),
            baked.probe_sh.len()
        );
        Some(baked)
    }

    /// Coarse scene ambient from the baked probe SH (average L0 radiance), or
    /// None when there are no baked probes. Phase 5a: a flat fallback so the
    /// bake visibly affects the scene before per-fragment probe/lightmap
    /// sampling lands.
    pub fn baked_ambient(&self) -> Option<[f32; 3]> {
        let baked = self.baked.as_ref()?;
        if baked.probe_sh.is_empty() {
            return None;
        }
        let n = baked.probe_sh.len() as f32;
        let mut acc = [0.0f32; 3];
        for sh in &baked.probe_sh {
            for (a, c) in acc.iter_mut().zip(sh.coeffs[0]) {
                *a += c;
            }
        }
        // SH L0 basis Y0 = 0.282095 gives the average constant radiance term.
        const Y0: f32 = 0.282_094_8;
        Some([acc[0] / n * Y0, acc[1] / n * Y0, acc[2] / n * Y0])
    }

    /// Gather everything the GPU lighting bake needs: static-geometry
    /// instances (with per-object lightmap resolution from texel density),
    /// Baked-mode lights, and probe-volume grid points. See [`BakeGather`].
    pub fn gather_bake(&self) -> BakeGather {
        use citrus_core::{LightComponent, LightKind, LightMode, LightProbeVolume};

        let settings = self.environment.bake;
        let mut instances = Vec::new();
        let mut instance_objects = Vec::new();

        for i in 0..self.objects.len() {
            let obj = &self.objects[i];
            if !obj.static_geometry || !self.is_active(i) {
                continue;
            }
            let Some(render) = obj.render else { continue };
            // Skip meshes with no real lightmap UV (uv1 == uv0 fallback): baking
            // their overlapping charts yields garbage. The editor offers to
            // generate UVs for these before a bake (see `meshes_needing_lightmap_uv`).
            if !self.mesh_has_lightmap_uv.get(render.mesh).copied().unwrap_or(false) {
                continue;
            }
            let world = self.world_transform(i);
            let (min, max) = self.mesh_bounds[render.mesh];
            let scale = world.to_scale_rotation_translation().0.abs();
            let extent = (max - min) * scale;
            let max_extent = extent.x.max(extent.y).max(extent.z).max(0.01);
            let density = settings.texel_density * obj.lightmap_scale.max(0.0);
            let size = (density * max_extent).round() as u32;
            // Floor at 64 so multi-chart atlases (cube) keep ≥2-texel gutters.
            // A Shadows Only object is never drawn: it stays in the bake as a
            // shadow caster, with a token lightmap to keep layers index-aligned.
            let lightmap_size = if obj.cast_shadows.visible() {
                size.clamp(64, settings.max_lightmap.max(64))
            } else {
                8
            };

            let entry = &self.materials[render.material];
            let model = &entry.model;
            // Emitter flux = emission colour × intensity × emission-map mean, so a
            // mostly-black map contributes a dim, visor-tinted light instead of a
            // uniform full-mesh emitter (map mean defaults to 1 = no map).
            let emission = if model.emission_enabled {
                let m = entry.emission_map_mean;
                [
                    model.emission_color[0] * model.emission_intensity * m[0],
                    model.emission_color[1] * model.emission_intensity * m[1],
                    model.emission_color[2] * model.emission_intensity * m[2],
                ]
            } else {
                [0.0; 3]
            };
            instances.push(citrus_render::BakeInstance {
                mesh: self.mesh_handles[render.mesh],
                transform: world,
                lightmap_size,
                // Texture-mean-corrected albedo + metalness (see MaterialEntry):
                // a textured object bakes with its real average colour/metalness,
                // not just the scalar base_color/metallic, so an MR-mapped surface
                // (metallic scalar = 1.0 multiplier) isn't treated as fully metal.
                albedo: entry.bake_albedo,
                emission,
                metallic: entry.bake_metallic,
                roughness: model.roughness,
                shadow_casting: render_shadow_casting(obj.cast_shadows),
            });
            instance_objects.push(i);
        }

        // Bake captures Baked + Mixed lights (+ the environment sun below);
        // Realtime lights are never baked, they stay purely in the realtime
        // path. (Baked/Mixed are dropped from realtime once a bake exists; see
        // gather_lights.)
        let mut lights = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            let Some(light) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<LightComponent>())
            else {
                continue;
            };
            if light.mode == LightMode::Realtime {
                continue;
            }
            let world = self.world_transform(i);
            let (_, rotation, position) = world.to_scale_rotation_translation();
            let direction = rotation * Vec3::NEG_Z;
            let kind = match light.kind {
                LightKind::Directional => citrus_render::LightKind::Directional,
                LightKind::Point => citrus_render::LightKind::Point,
                LightKind::Spot => citrus_render::LightKind::Spot,
            };
            lights.push(citrus_render::BakeLight {
                kind,
                position,
                direction,
                color: [
                    light.color[0] * light.intensity,
                    light.color[1] * light.intensity,
                    light.color[2] * light.intensity,
                ],
                range: light.range,
                spot_inner_deg: light.spot_angle * (1.0 - light.spot_blend),
                spot_outer_deg: light.spot_angle,
                radius: light.radius.max(0.0),
            });
        }

        // Probe volumes → flattened world-space probe points + per-volume
        // metadata for runtime trilinear lookup.
        let mut probes = Vec::new();
        let mut probe_volumes = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            let Some(vol) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<LightProbeVolume>())
            else {
                continue;
            };
            let world = self.world_transform(i);
            let sh_base = probes.len();
            for local in vol.local_positions() {
                probes.push(world.transform_point3(local));
            }
            probe_volumes.push(ProbeVolumeMeta {
                world_to_local: world.inverse(),
                size: vol.size,
                counts: vol.counts(),
                sh_base,
                flux_voxel: false,
            });
        }

        // The environment sun is an environment light → bake it (and it's
        // dropped from the realtime sun when a bake exists).
        if self.environment.sun_enabled {
            let dir = Vec3::from(self.environment.sun_direction).normalize_or(Vec3::NEG_Y);
            let c = self.environment.sun_color;
            let s = self.environment.sun_intensity;
            lights.push(citrus_render::BakeLight {
                kind: citrus_render::LightKind::Directional,
                position: Vec3::ZERO,
                direction: dir,
                color: [c[0] * s, c[1] * s, c[2] * s],
                range: 0.0,
                spot_inner_deg: 0.0,
                spot_outer_deg: 0.0,
                // Soft edge for the sun (interpreted as angular spread for a
                // directional light in the bake's shadow sampling).
                radius: 0.03,
            });
        }

        let amb = self.environment.ambient;
        let ai = self.environment.ambient_intensity;
        // Diagnostic: what the bake actually received. An all-black bake usually
        // means zero lights here (none Baked/Mixed, sun off) or a static-instance
        // mismatch — this line shows which.
        tracing::info!(
            "bake gather: {} lights, {} static instances, sun_enabled={}, sky=[{:.3},{:.3},{:.3}]",
            lights.len(),
            instances.len(),
            self.environment.sun_enabled,
            amb[0] * ai,
            amb[1] * ai,
            amb[2] * ai,
        );
        for (n, l) in lights.iter().enumerate() {
            tracing::info!(
                "  bake light {n}: {:?} color=[{:.2},{:.2},{:.2}] pos={:?} range={:.1}",
                l.kind,
                l.color[0],
                l.color[1],
                l.color[2],
                l.position,
                l.range,
            );
        }
        BakeGather {
            // The bake only includes static geometry.
            instance_static: vec![true; instances.len()],
            instances,
            instance_objects,
            lights,
            probes,
            probe_volumes,
            sky_color: [amb[0] * ai, amb[1] * ai, amb[2] * ai],
        }
    }

    /// Build-time FluxVoxel bake input: the FluxVolume grids are the probe points,
    /// the STATIC FluxVoxel Lights are the light sources, and the scene's static
    /// geometry is the occluder/bouncer set (so the voxels carry real multi-bounce
    /// GI, not analytic direct-only). Reuses [`gather_bake`] for the geometry +
    /// sky, then swaps in FluxVoxel probes + lights. Returns `None` when there are no
    /// FluxVolumes or no static FluxVoxel Lights (nothing to bake). The returned
    /// `probe_volumes` are flagged `flux_voxel = true` so the runtime knows to feed
    /// them to `update_flux_voxel` (static base + live dynamic add) instead of
    /// uploading them as plain DDGI probes.
    pub fn gather_flux_voxel_bake(&self) -> Option<BakeGather> {
        let volumes = self.flux_voxel_volumes();
        if volumes.is_empty() {
            return None;
        }
        let static_lights: Vec<citrus_render::BakeLight> = self
            .flux_voxel_lights()
            .into_iter()
            .filter(|(_, s)| *s)
            .map(|(l, _)| l)
            .collect();
        if static_lights.is_empty() {
            return None;
        }
        let base = self.gather_bake();
        let mut probes = Vec::new();
        let mut probe_volumes = Vec::new();
        for (center, size, counts) in &volumes {
            let sh_base = probes.len();
            let positions = crate::sw_gi::flux_volume_positions(*center, *size, *counts);
            probes.extend(positions);
            probe_volumes.push(ProbeVolumeMeta {
                world_to_local: Mat4::from_translation(-*center),
                size: size.to_array(),
                counts: [counts[0] as usize, counts[1] as usize, counts[2] as usize],
                sh_base,
                flux_voxel: true,
            });
        }
        Some(BakeGather {
            instance_static: base.instance_static,
            instances: base.instances,
            instance_objects: base.instance_objects,
            lights: static_lights,
            probes,
            probe_volumes,
            sky_color: base.sky_color,
        })
    }

    /// Gather inputs for the realtime-GI preview: every active mesh object as a
    /// FluxVoxel light sources: every active object bearing a `FluxVoxelLight`
    /// component, as a point-light contribution at its CURRENT world position,
    /// tagged with whether the object is static. Static ones are baked into the
    /// volume once; dynamic ones (`false`) mix in live so a moving FluxVoxel light
    /// lights up static objects sharing the volume. Only components participate,
    /// so the author chooses what feeds FluxVoxel.
    pub fn flux_voxel_lights(&self) -> Vec<(citrus_render::BakeLight, bool)> {
        let mut out = Vec::new();
        // Realtime/Mixed normal LightComponents are NOT injected into the grid:
        // they direct-light the forward pass instead (Mixed is baked into the
        // lightmap for static objects and applied realtime only to dynamic ones —
        // see gather_lights). Injecting them here as well triple-counted their
        // energy (lightmap + forward + grid) — the "too much light" on Mixed.
        // The grid carries only dedicated FluxVoxelLight volume lights + emissive.
        //
        // A `FluxVoxelLight` component is a dedicated volume-only light.
        for i in 0..self.objects.len() {
            if !self.frame_active(i) {
                continue;
            }
            let Some(f) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<citrus_core::FluxVoxelLight>())
            else {
                continue;
            };
            let pos = self.frame_world(i).w_axis.truncate();
            let light = citrus_render::BakeLight {
                kind: citrus_render::LightKind::Point,
                position: pos,
                direction: Vec3::NEG_Y,
                color: [
                    f.color[0] * f.intensity,
                    f.color[1] * f.intensity,
                    f.color[2] * f.intensity,
                ],
                range: f.range.max(0.1),
                spot_inner_deg: 0.0,
                spot_outer_deg: 0.0,
                radius: 0.0,
            };
            out.push((light, self.objects[i].static_geometry));
        }
        // Static emitters join the cached, propagated grid; moving ones are
        // shaded per pixel instead (`flux_voxel_dynamic_emitters`), since a
        // probe grid shows its cells under an orb's sharp falloff.
        out.extend(self.emitter_voxel_lights(true).into_iter().map(|l| (l, true)));
        out
    }

    /// Moving emissive objects as FluxVoxel point lights, for the forward pass
    /// to evaluate per pixel with the grid's injection kernel.
    pub fn flux_voxel_dynamic_emitters(&self) -> Vec<citrus_render::BakeLight> {
        self.emitter_voxel_lights(false)
    }

    /// Emissive objects (static or moving) as additive FluxVoxel lights.
    fn emitter_voxel_lights(&self, static_geometry: bool) -> Vec<citrus_render::BakeLight> {
        let mut out = Vec::new();
        // Emissive objects act as additive FluxVoxel lights: their emission
        // (colour × intensity × emission-map mean) becomes a movable point light
        // with distance falloff, so the voxel GI is lit FROM them like any other
        // FluxVoxel light (instead of not at all). Brighter emission reaches
        // further. Dynamic (non-static) emitters track their movement per frame.
        for i in 0..self.objects.len() {
            if !self.frame_active(i) {
                continue;
            }
            if self.objects[i].static_geometry != static_geometry {
                continue;
            }
            let Some(render) = self.objects[i].render else {
                continue;
            };
            let entry = &self.materials[render.material];
            let m = &entry.model;
            if !m.emission_enabled {
                continue;
            }
            let mean = entry.emission_map_mean;
            // Gain on the emissive→voxel injection. The bounce a surface receives is
            // `albedo · irradiance / π`, so off a dark floor it's physically very dim
            // — invisible without help. An emissive object reads as a glowing LAMP,
            // so we inject it as a brighter area source into the volume to give a
            // clearly-visible VIEW-INDEPENDENT diffuse pool (the look the user wants).
            // Tunable; raise if emitters should throw more light, lower for subtler.
            const EMISSIVE_GI_GAIN: f32 = 8.0;
            let gain = EMISSIVE_GI_GAIN * self.environment.realtime_gi.emissive_boost.max(0.0);
            let total_flux = [
                m.emission_color[0] * m.emission_intensity * mean[0] * gain,
                m.emission_color[1] * m.emission_intensity * mean[1] * gain,
                m.emission_color[2] * m.emission_intensity * mean[2] * gain,
            ];
            let lum = 0.2126 * total_flux[0] + 0.7152 * total_flux[1] + 0.0722 * total_flux[2];
            if lum <= 1e-3 {
                continue;
            }
            // Multi-point emissive sampling (FLUXVOXEL_TODO §E): a compact orb is one
            // point at its centre, but an elongated emitter (light strip, screen,
            // visor) scatters K points along its longest axis so it lights its
            // surroundings over its whole extent, not just the object centre. Energy
            // is split across the points so the total emitted flux is preserved.
            let world = self.frame_world(i);
            let (lmin, lmax) = self.mesh_bounds[render.mesh];
            let lcenter = (lmin + lmax) * 0.5;
            let extent = (lmax - lmin).abs();
            let long_axis = if extent.x >= extent.y && extent.x >= extent.z {
                0
            } else if extent.y >= extent.z {
                1
            } else {
                2
            };
            let long = extent[long_axis];
            let thin = extent.min_element().max(1e-3);
            let k = if long > 1e-3 {
                ((long / thin).round() as usize).clamp(1, 8)
            } else {
                1
            };
            let inv_k = 1.0 / k as f32;
            let flux = [
                total_flux[0] * inv_k,
                total_flux[1] * inv_k,
                total_flux[2] * inv_k,
            ];
            // Range = pool radius (the falloff hits 0 here). Keep it MODEST: a bright
            // orb should glow a few metres, not flood the whole floor. Computed from
            // the TRUE emission (gain divided back out) so the GI brightness gain
            // above doesn't also inflate the reach.
            let plum = (lum / EMISSIVE_GI_GAIN) * inv_k;
            // Reach: enough to glow onto nearby surfaces (floor, couch, props), not
            // just the object itself — but capped so it doesn't flood the whole room.
            let range = (plum.sqrt() * 2.0).clamp(1.0, 8.0);
            for j in 0..k {
                let t = if k == 1 { 0.5 } else { j as f32 / (k - 1) as f32 };
                let mut lp = lcenter;
                lp[long_axis] = lmin[long_axis] + extent[long_axis] * t;
                let pos = world.transform_point3(lp);
                let light = citrus_render::BakeLight {
                    kind: citrus_render::LightKind::Point,
                    position: pos,
                    direction: Vec3::NEG_Y,
                    color: flux,
                    range,
                    spot_inner_deg: 0.0,
                    spot_outer_deg: 0.0,
                    radius: 0.0,
                };
                out.push(light);
            }
        }
        out
    }

    /// The build-time baked FluxVoxel static base, if a bake exists with FluxVoxel
    /// volumes. Returns the flattened SH accumulators, world positions, and the
    /// `set_baked_probes` meta tuples — exactly the three parallel arrays
    /// `realtime_gi::update_flux_voxel` keeps, so it can seed its static base from
    /// disk-baked voxels instead of recomputing them analytically each load.
    #[allow(clippy::type_complexity)]
    pub fn flux_voxel_baked(
        &self,
    ) -> Option<(
        Vec<[glam::Vec3; 4]>,
        Vec<Vec3>,
        Vec<(Mat4, [f32; 3], [u32; 3], u32)>,
    )> {
        let baked = self.baked.as_ref()?;
        if !baked.probe_volumes.iter().any(|v| v.flux_voxel) {
            return None;
        }
        let mut acc = Vec::new();
        let mut positions = Vec::new();
        let mut meta = Vec::new();
        let mut base = 0u32;
        for v in baked.probe_volumes.iter().filter(|v| v.flux_voxel) {
            // The bake stored world_to_local as translate(-center); recover center.
            let center = -v.world_to_local.w_axis.truncate();
            let size = Vec3::from(v.size);
            let counts = [v.counts[0] as u32, v.counts[1] as u32, v.counts[2] as u32];
            let pos = crate::sw_gi::flux_volume_positions(center, size, counts);
            let n = pos.len();
            for i in 0..n {
                let sh = &baked.probe_sh[v.sh_base + i];
                acc.push(crate::sw_gi::probe_to_acc(sh));
            }
            positions.extend(pos);
            meta.push((v.world_to_local, v.size, counts, base));
            base += n as u32;
        }
        Some((acc, positions, meta))
    }

    /// FluxVoxel voxel volumes: each author-placed `FluxVolume` component as
    /// `(world center, box size, probe counts)`. Falls back to a single volume
    /// covering the whole scene (so FluxVoxel works without an explicit volume).
    /// Coarse scene occupancy grid (world-space mesh AABBs) for FluxVoxel
    /// voxel-light occlusion. None when there's no geometry to occlude.
    pub fn flux_occupancy(&self) -> Option<crate::sw_gi::SceneOccupancy> {
        let mut boxes: Vec<(Vec3, Vec3)> = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) || self.objects[i].render.is_none() {
                continue;
            }
            // Emitters are NOT occluders: an emissive object's own mesh must not be
            // in the occupancy grid, or its light self-occludes (the ray from the orb
            // centre to nearby probes starts inside the orb's own solid cells and
            // reads as fully blocked). This is exactly why a STATIC emissive orb
            // (occluded inject) went dark while a dynamic one (non-occluded) lit fine.
            let emissive = self.objects[i].render_slots().any(|r| {
                self.materials
                    .get(r.material)
                    .is_some_and(|e| e.model.emission_enabled)
            });
            if emissive {
                continue;
            }
            let world = self.world_transform(i);
            for render in self.objects[i].render_slots() {
                let (min, max) = self.mesh_bounds[render.mesh];
                let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
                for cx in [min.x, max.x] {
                    for cy in [min.y, max.y] {
                        for cz in [min.z, max.z] {
                            let p = world.transform_point3(Vec3::new(cx, cy, cz));
                            lo = lo.min(p);
                            hi = hi.max(p);
                        }
                    }
                }
                if lo.is_finite() {
                    boxes.push((lo, hi));
                }
            }
        }
        crate::sw_gi::SceneOccupancy::build(&boxes, 48)
    }

    pub fn flux_voxel_volumes(&self) -> Vec<(Vec3, Vec3, [u32; 3])> {
        self.flux_voxel_volumes_view(None)
    }

    /// FluxVoxel volumes with the camera position available (for the
    /// `CameraClipmap` auto-grid mode). `view_pos = None` (the bake / gizmo path)
    /// makes the clipmap fall back to the scene-centred whole-scene grid.
    ///
    /// Author-placed FluxVolumes and the auto grid are MUTUALLY EXCLUSIVE: placing
    /// any FluxVolume turns the auto grid off entirely, so the author controls
    /// coverage exactly and the global density slider stops perturbing the placed
    /// region (changing density used to re-grid + drop the placed volume's bake).
    pub fn flux_voxel_volumes_view(&self, view_pos: Option<Vec3>) -> Vec<(Vec3, Vec3, [u32; 3])> {
        use citrus_assets::VoxelGridMode;
        let mut out = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            let Some(v) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<citrus_core::FluxVolume>())
            else {
                continue;
            };
            let center = self.world_transform(i).w_axis.truncate();
            out.push((center, Vec3::from(v.size), v.counts()));
        }
        // Mutual exclusion + master toggle: auto grid only when zero placed volumes.
        if !out.is_empty() || !self.environment.realtime_gi.voxel_auto_grid {
            return out;
        }

        let gi = &self.environment.realtime_gi;
        // density = probes per meter → spacing = 1/density.
        let density = gi.voxel_density.clamp(0.05, 8.0);
        let spacing = (1.0 / density).max(0.25);
        // Cap per axis so a huge scene can't explode the probe count (the per-frame
        // inject is O(probes); 48³ ≈ 110k is the upper bound for 1k+ fps headroom).
        let counts_for = |size: Vec3| {
            [
                ((size.x / spacing).round() as u32 + 1).clamp(2, 48),
                ((size.y / spacing).round() as u32 + 1).clamp(2, 48),
                ((size.z / spacing).round() as u32 + 1).clamp(2, 48),
            ]
        };

        match gi.voxel_grid_mode {
            // A fixed cube that follows the camera, snapped to the probe spacing so
            // it scrolls in whole-probe steps (the grid stays world-aligned → no
            // per-frame re-bake jitter). Constant probe count regardless of level
            // size: space you never visit is never computed. Falls back to the
            // scene centre when there's no camera (bake / gizmo path).
            VoxelGridMode::CameraClipmap => {
                let extent = gi.voxel_clipmap_extent.clamp(2.0, 256.0);
                // Scroll in steps of about a quarter of the extent (whole probes,
                // so the grid stays aligned): each step rebuilds the static base,
                // and the camera stays well inside the cube between steps.
                let step = spacing * (extent * 0.25 / spacing).round().max(1.0);
                let center = view_pos
                    .map(|p| (p / step).round() * step)
                    .or_else(|| self.flux_voxel_static_bounds().map(|(lo, hi)| (lo + hi) * 0.5))
                    .unwrap_or(Vec3::ZERO);
                let size = Vec3::splat(extent * 2.0);
                vec![(center, size, counts_for(size))]
            }
            // Tight grids hugging clusters of static geometry; the gaps between
            // clusters stay uncovered. Clustered down to the shader's volume cap.
            VoxelGridMode::PerObject => {
                let mut clusters = self.flux_voxel_object_aabbs();
                cluster_aabbs(&mut clusters, MAX_AUTO_VOLUMES);
                clusters
                    .into_iter()
                    .map(|(lo, hi)| {
                        let (lo, hi) = (lo - spacing, hi + spacing);
                        let center = (lo + hi) * 0.5;
                        let size = (hi - lo).max(Vec3::splat(0.5));
                        (center, size, counts_for(size))
                    })
                    .collect()
            }
            // A single box over the static-geometry bounds. WholeScene pads by a
            // fade margin so GI melts into ambient at the edge; Occupancy uses tight
            // bounds and (in update_flux_voxel) drops probes far from geometry so open
            // air costs nothing to inject.
            mode @ (VoxelGridMode::WholeScene | VoxelGridMode::Occupancy) => {
                let Some((lo, hi)) = self.flux_voxel_static_bounds() else {
                    return Vec::new();
                };
                let pad = if mode == VoxelGridMode::WholeScene {
                    ((hi - lo).length() * 0.1).max(1.0)
                } else {
                    spacing
                };
                let (lo, hi) = (lo - pad, hi + pad);
                // Snap to a 1 m grid so sub-metre jitter never re-bakes the base.
                let q = 1.0f32;
                let lo = (lo / q).floor() * q;
                let hi = (hi / q).ceil() * q;
                let center = (lo + hi) * 0.5;
                let size = (hi - lo).max(Vec3::splat(0.5));
                vec![(center, size, counts_for(size))]
            }
        }
    }

    /// World-space AABB over STATIC render geometry (the stable basis for the auto
    /// grid — a moving object mustn't reshape it). Falls back to ALL geometry when
    /// the scene has no static objects. None when there's no geometry at all.
    fn flux_voxel_static_bounds(&self) -> Option<(Vec3, Vec3)> {
        let bounds = |want_static: bool| {
            let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
            for i in 0..self.objects.len() {
                if !self.is_active(i) || self.objects[i].render.is_none() {
                    continue;
                }
                if want_static && !self.objects[i].static_geometry {
                    continue;
                }
                let world = self.world_transform(i);
                for render in self.objects[i].render_slots() {
                    let (min, max) = self.mesh_bounds[render.mesh];
                    for cx in [min.x, max.x] {
                        for cy in [min.y, max.y] {
                            for cz in [min.z, max.z] {
                                let p = world.transform_point3(Vec3::new(cx, cy, cz));
                                lo = lo.min(p);
                                hi = hi.max(p);
                            }
                        }
                    }
                }
            }
            (lo, hi)
        };
        let s = bounds(true);
        let (lo, hi) = if s.0.is_finite() { s } else { bounds(false) };
        lo.is_finite().then_some((lo, hi))
    }

    /// Per-object world-space AABBs over static render geometry (Per-object grid).
    fn flux_voxel_object_aabbs(&self) -> Vec<(Vec3, Vec3)> {
        let mut out = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) || self.objects[i].render.is_none() {
                continue;
            }
            if !self.objects[i].static_geometry {
                continue;
            }
            let world = self.world_transform(i);
            let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
            for render in self.objects[i].render_slots() {
                let (min, max) = self.mesh_bounds[render.mesh];
                for cx in [min.x, max.x] {
                    for cy in [min.y, max.y] {
                        for cz in [min.z, max.z] {
                            let p = world.transform_point3(Vec3::new(cx, cy, cz));
                            lo = lo.min(p);
                            hi = hi.max(p);
                        }
                    }
                }
            }
            if lo.is_finite() {
                out.push((lo, hi));
            }
        }
        out
    }

    /// bouncer/occluder, the realtime lights (+ env sun), and an automatic probe
    /// grid covering the scene bounds. Unlike `gather_bake` this includes
    /// non-static objects and the realtime lights, so the un-baked scene shows
    /// live indirect. Returns None when there's nothing to light.
    pub fn gather_realtime_gi(&self) -> Option<BakeGather> {
        use citrus_core::{LightComponent, LightKind};

        let mut instances = Vec::new();
        let mut instance_objects = Vec::new();
        let mut instance_static = Vec::new();
        // The probe volume + GDF bounds are sized from STATIC geometry only, so
        // moving a dynamic object (e.g. a freshly-added primitive) doesn't resize
        // the volume and perturb every other surface's GI. `flo/fhi` is a
        // full-scene fallback used only when the scene has no static geometry.
        let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
        let (mut flo, mut fhi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            if self.objects[i].render.is_none() {
                continue;
            }
            let is_static = self.objects[i].static_geometry;
            let world = self.world_transform(i);
            // Every material slot contributes geometry + emission to the trace.
            for render in self.objects[i].render_slots() {
                let (min, max) = self.mesh_bounds[render.mesh];
                // Expand the volume AABB by this slot's 8 transformed corners.
                // Static geometry sizes the real volume; everything feeds the
                // full-scene fallback (used only if nothing static exists).
                for cx in [min.x, max.x] {
                    for cy in [min.y, max.y] {
                        for cz in [min.z, max.z] {
                            let p = world.transform_point3(Vec3::new(cx, cy, cz));
                            flo = flo.min(p);
                            fhi = fhi.max(p);
                            if is_static {
                                lo = lo.min(p);
                                hi = hi.max(p);
                            }
                        }
                    }
                }
                let entry = &self.materials[render.material];
                let model = &entry.model;
                // Scale emitter flux by the emission-map mean (see the bake-gather
                // path above): a mostly-black map → a dim, correctly-tinted light.
                let emission = if model.emission_enabled {
                    let m = entry.emission_map_mean;
                    [
                        model.emission_color[0] * model.emission_intensity * m[0],
                        model.emission_color[1] * model.emission_intensity * m[1],
                        model.emission_color[2] * model.emission_intensity * m[2],
                    ]
                } else {
                    [0.0; 3]
                };
                instances.push(citrus_render::BakeInstance {
                    mesh: self.mesh_handles[render.mesh],
                    transform: world,
                    lightmap_size: 8, // unused: probes_only skips lightmap tracing
                    albedo: [model.base_color[0], model.base_color[1], model.base_color[2]],
                    emission,
                    metallic: model.metallic,
                    roughness: model.roughness,
                    shadow_casting: render_shadow_casting(self.objects[i].cast_shadows),
                });
                instance_objects.push(i);
                instance_static.push(is_static);
            }
        }
        // No static geometry → fall back to the full-scene bounds so an all-dynamic
        // scene still gets a volume (it just isn't movement-stable until something
        // is marked static).
        if !lo.is_finite() {
            lo = flo;
            hi = fhi;
        }
        if instances.is_empty() || !lo.is_finite() {
            return None;
        }

        // ALL active lights drive the GI (+ env sun below). Realtime GI only
        // runs when there's no bake, and in that state every light renders in
        // realtime regardless of its mode (see gather_lights), so the GI must
        // bounce them all. (Filtering to Realtime-only here left Baked/Mixed
        // point lights out, giving zero GI.)
        let mut lights = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            let Some(light) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<LightComponent>())
            else {
                continue;
            };
            let world = self.world_transform(i);
            let (_, rotation, position) = world.to_scale_rotation_translation();
            let kind = match light.kind {
                LightKind::Directional => citrus_render::LightKind::Directional,
                LightKind::Point => citrus_render::LightKind::Point,
                LightKind::Spot => citrus_render::LightKind::Spot,
            };
            lights.push(citrus_render::BakeLight {
                kind,
                position,
                direction: rotation * Vec3::NEG_Z,
                color: [
                    light.color[0] * light.intensity,
                    light.color[1] * light.intensity,
                    light.color[2] * light.intensity,
                ],
                range: light.range,
                spot_inner_deg: light.spot_angle * (1.0 - light.spot_blend),
                spot_outer_deg: light.spot_angle,
                radius: light.radius.max(0.0),
            });
        }
        if self.environment.sun_enabled {
            let dir = Vec3::from(self.environment.sun_direction).normalize_or(Vec3::NEG_Y);
            let c = self.environment.sun_color;
            let s = self.environment.sun_intensity;
            lights.push(citrus_render::BakeLight {
                kind: citrus_render::LightKind::Directional,
                position: Vec3::ZERO,
                direction: dir,
                color: [c[0] * s, c[1] * s, c[2] * s],
                range: 0.0,
                spot_inner_deg: 0.0,
                spot_outer_deg: 0.0,
                // Soft edge for the sun (interpreted as angular spread for a
                // directional light in the bake's shadow sampling).
                radius: 0.03,
            });
        }

        // Cascaded probe volumes (SDFGI-style), centered on the scene. One grid
        // over the whole scene is necessarily coarse, giving visible trilinear
        // "squares" near the action. So we nest grids: the coarsest covers the full
        // padded AABB, and each finer cascade halves the box (so it doubles the
        // probe density) around the same center. The shader picks the finest
        // cascade that contains a fragment (volumes are emitted finest-first), so
        // the center gets fine GI while the edges/sky stay cheap. Each cascade is
        // another full grid to march, so the count is capped.
        let software = self.environment.realtime_gi.mode == citrus_assets::GiMode::Flux;
        let spacing = self.environment.realtime_gi.probe_spacing.max(0.25);
        // Margin so geometry (esp. a large floor) sits well inside the outermost
        // cascade. Its edge fades to ambient, so too little pad lands that fade
        // ring on the visible floor as a hard line. Generous pad pushes the box
        // boundary off the geometry; it also enlarges the coarsest box, which
        // auto-raises the cascade count below, so the center stays dense.
        let pad = ((hi - lo).length() * 0.20).max(2.0);
        let (lo, hi) = (lo - pad, hi + pad);
        let center = (lo + hi) * 0.5;
        let size = (hi - lo).max(Vec3::splat(0.1)); // coarsest (full-scene) box
        // Per-axis probe count for every cascade, derived from the full-scene box
        // and clamped. Kept at 32: pushing it higher makes the coarse cell fine
        // enough that the cascade-count formula below collapses to a single grid,
        // which paradoxically coarsens the (off-center) emitter region. The
        // multi-cascade nest at 32 keeps the center denser. Grid-cell structure is
        // smoothed by the probe-grid denoise blur, not by raw count alone.
        // Software uses nested cascades (small boxes) so a 32-cap stays cheap on
        // the GPU march. RayQuery bakes a single grid *synchronously*, so 32³ =
        // 32768 probes is ~9ms/trace; cap it to 24³ (~13k) for realtime; the
        // probe-grid blur keeps the soft look at the lower resolution.
        let max_axis = if software { 32 } else { 24 };
        // Minimum 4 probes per axis: a shallow scene (e.g. a floor + objects with
        // a small vertical extent) otherwise gets only 2 probes on the Y axis, so
        // moving an object vertically swings the trilinear blend hugely (the
        // position-dependent under-mesh bounce). 4 gives a real vertical gradient.
        let axis_count = |e: f32| ((e / spacing).round() as i32).clamp(4, max_axis) as usize;
        let counts = [axis_count(size.x), axis_count(size.y), axis_count(size.z)];

        // Number of cascades: enough 2x refinements to bring the coarsest cell
        // size down toward ~0.3 m near the center, capped (each cascade is a full
        // extra grid to march). Hardware uses a single fine grid for now.
        const TARGET_FINE: f32 = 0.3;
        let coarse_cell = (0..3)
            .map(|a| size[a] / (counts[a].max(2) - 1) as f32)
            .fold(0.0f32, f32::max);
        let cascades = if software {
            ((coarse_cell / TARGET_FINE).log2().round() as i32 + 1).clamp(1, 3) as usize
        } else {
            1
        };

        // Emit finest-first so the shader's first-containing-volume rule selects
        // the densest cascade available at each fragment. All cascades share the
        // scene center (concentric) so the cross-fade between levels lines up and
        // the GI doesn't shift as the camera moves.
        let mut probes = Vec::new();
        let mut probe_volumes = Vec::new();
        for k in 0..cascades {
            let scale = 0.5f32.powi((cascades - 1 - k) as i32); // finest..1.0
            let cs = size * scale;
            let clo = center - cs * 0.5;
            let sh_base = probes.len();
            for gz in 0..counts[2] {
                for gy in 0..counts[1] {
                    for gx in 0..counts[0] {
                        let f = Vec3::new(
                            gx as f32 / (counts[0] - 1).max(1) as f32,
                            gy as f32 / (counts[1] - 1).max(1) as f32,
                            gz as f32 / (counts[2] - 1).max(1) as f32,
                        );
                        probes.push(clo + f * cs);
                    }
                }
            }
            probe_volumes.push(ProbeVolumeMeta {
                world_to_local: Mat4::from_translation(-center),
                size: cs.to_array(),
                counts,
                sh_base,
                flux_voxel: false,
            });
        }

        let amb = self.environment.ambient;
        let ai = self.environment.ambient_intensity;
        Some(BakeGather {
            instances,
            instance_objects,
            instance_static,
            lights,
            probes,
            probe_volumes,
            sky_color: [amb[0] * ai, amb[1] * ai, amb[2] * ai],
        })
    }

    /// Build the owned inputs for a software-GI march: lazily generates+caches
    /// per-mesh SDFs, then returns marchable instances (sharing the `Arc` SDFs)
    /// plus the scene size. All `Send`, so the caller can run the march on a
    /// background thread.
    pub fn software_gi_inputs(
        &mut self,
        gather: &BakeGather,
    ) -> (Vec<crate::sw_gi::SdfInstance>, f32) {
        // Resolve each gather instance to its local mesh index by its handle
        // (an object may contribute several slots, each a different mesh).
        // MeshHandle is Eq but not Hash, so resolve each instance once up front;
        // consecutive instances usually share a mesh, so a last-hit cache skips
        // most of the linear scans.
        let mut last: Option<(citrus_render::MeshHandle, Option<usize>)> = None;
        let inst_mesh: Vec<Option<usize>> = gather
            .instances
            .iter()
            .map(|inst| match last {
                Some((h, mi)) if h == inst.mesh => mi,
                _ => {
                    let mi = self.mesh_handles.iter().position(|&h| h == inst.mesh);
                    last = Some((inst.mesh, mi));
                    mi
                }
            })
            .collect();
        for &mi in inst_mesh.iter().flatten() {
            if self.mesh_sdf[mi].is_none() {
                let (pos, idx) = &self.mesh_geometry[mi];
                self.mesh_sdf[mi] =
                    Some(std::sync::Arc::new(citrus_render::sdf::generate_sdf(pos, idx, 32, 0.1)));
            }
        }
        let mut insts = Vec::with_capacity(gather.instances.len());
        for (k, instance) in gather.instances.iter().enumerate() {
            let Some(mi) = inst_mesh[k] else {
                continue;
            };
            let Some(sdf) = self.mesh_sdf[mi].as_ref() else {
                continue;
            };
            let static_geometry = gather
                .instance_objects
                .get(k)
                .map(|&obj| self.objects[obj].static_geometry)
                .unwrap_or(false);
            let world = instance.transform;
            let scale = (world.x_axis.length() + world.y_axis.length() + world.z_axis.length())
                / 3.0;
            insts.push(crate::sw_gi::SdfInstance {
                inv: world.inverse(),
                scale: scale.max(1e-4),
                sdf: sdf.clone(),
                albedo: instance.albedo,
                emission: instance.emission,
                metallic: instance.metallic,
                roughness: instance.roughness,
                static_geometry,
            });
        }
        let scene_size = gather
            .probe_volumes
            .first()
            .map(|v| Vec3::from(v.size).length())
            .unwrap_or(10.0);
        (insts, scene_size)
    }

    /// Blend the post-processing Volumes affecting `camera_pos` into the
    /// effective per-frame parameters (Unity-style: priority-ordered, weight ×
    /// local proximity). Profiles are loaded from `.postfx` files (cached).
    /// With no volumes, returns neutral defaults (ACES, no grading/vignette).
    /// Advance skeletal animation and CPU-skin all skinned meshes into their
    /// host-visible vertex buffers. Plays the first imported clip on a loop (a
    /// per-object Animator with clip selection is a follow-up). No-op when the
    /// scene has no rigged meshes.
    /// Set full-body IK targets for humanoid avatars this frame (VR, gameplay,
    /// procedural, …). `None` clears them so avatars play their clips.
    pub fn set_ik_targets(&mut self, targets: Option<citrus_core::IkTargets>) {
        self.ik_targets = targets;
    }

    /// The eye cloth cull distances are measured from (None = the main camera).
    pub fn set_cloth_viewer(&mut self, eye: Option<Vec3>) {
        self.cloth_viewer = eye;
    }

    /// This machine's face tracking for this frame (None = not tracking).
    pub fn set_local_face(&mut self, face: Option<citrus_core::FaceFrame>) {
        self.local_face = face;
    }

    pub fn local_face(&self) -> Option<&citrus_core::FaceFrame> {
        self.local_face.as_ref()
    }

    /// Enable/disable terrain foot IK for humanoid avatars (applied on top of the
    /// animated/IK pose so feet plant on uneven ground). `None` disables it.
    pub fn set_foot_ik(&mut self, params: Option<crate::humanoid::FootIkParams>) {
        self.foot_ik = params;
    }

    /// Whether terrain foot IK is currently enabled.
    pub fn foot_ik_enabled(&self) -> bool {
        self.foot_ik.is_some()
    }

    /// The Network Avatar an object belongs to: its own, else the nearest
    /// ancestor's (a worn avatar sits under the player's Network Avatar).
    pub(crate) fn network_avatar_of(&self, i: usize) -> Option<(usize, &citrus_core::NetworkAvatar)> {
        std::iter::successors(Some(i), |&k| self.objects.get(k).and_then(|o| o.parent)).find_map(|k| {
            let av = self.objects.get(k)?.components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::NetworkAvatar>())?;
            Some((k, av))
        })
    }

    /// Skeleton and humanoid bone map of a skinned object (an Avatar
    /// Descriptor's bone map on it or an ancestor wins).
    pub(crate) fn humanoid_of(&self, i: usize) -> Option<(&citrus_assets::Skeleton, crate::humanoid::HumanoidRig)> {
        let o = self.objects.get(i)?;
        let mesh = o.render_slots().map(|r| r.mesh).find(|m| self.skinned_meshes.iter().any(|sm| sm.mesh_index == *m))?;
        let sm = self.skinned_meshes.iter().find(|sm| sm.mesh_index == mesh)?;
        let skel = self.skeletons.get(sm.skeleton)?;
        let descriptor = std::iter::successors(Some(i), |&k| self.objects[k].parent).find_map(|k| {
            self.objects[k].components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::AvatarDescriptor>())
        });
        let rig = match descriptor {
            Some(desc) if !desc.bone_map.is_empty() => crate::humanoid::HumanoidRig::map_with_overrides(skel, &desc.bone_map).0,
            _ => crate::humanoid::HumanoidRig::map(skel),
        };
        Some((skel, rig))
    }

    /// Every active humanoid Humanoid IK object, local ones (driven by this
    /// machine's trackers) first.
    pub(crate) fn humanoid_avatars(&self) -> Vec<HumanoidAvatar<'_>> {
        use citrus_core::IkSource;
        let active = self.active_flags();
        let vr_player = self.objects.iter().position(|o| o.components.iter().any(|c| c.as_any().is::<citrus_core::VrPlayer>()));
        let mut out: Vec<HumanoidAvatar<'_>> = Vec::new();
        for (i, o) in self.objects.iter().enumerate() {
            if !active[i] {
                continue;
            }
            let Some(ik) = o.components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::HumanoidIk>()) else { continue };
            let Some((skeleton, rig)) = self.humanoid_of(i).filter(|(_, r)| r.is_humanoid()) else { continue };
            let net = self.network_avatar_of(i);
            let remote = net.is_some_and(|(_, a)| a.remote);
            let local = match ik.source {
                IkSource::Auto => !remote,
                IkSource::LocalTrackers => true,
                IkSource::Network | IkSource::Objects => false,
            };
            let tracking_root = ik.tracking_root.id().and_then(|id| self.index_of(id)).or(vr_player);
            out.push(HumanoidAvatar { index: i, skeleton, rig, local, network_avatar: net.map(|(k, _)| k), tracking_root });
        }
        out.sort_by_key(|a| !a.local);
        out
    }

    /// Give object `i` a skinned mesh posed by `skel` (no GPU data), for tests.
    #[cfg(test)]
    pub(crate) fn add_test_skeleton(&mut self, i: usize, skel: citrus_assets::Skeleton) {
        let mesh = 1_000_000 + self.skinned_meshes.len();
        self.skeletons.push(skel);
        self.skinned_meshes.push(SkinnedMesh::new(mesh, Vec::new(), self.skeletons.len() - 1));
        self.objects[i].render = Some(RenderInfo { mesh, material: 0 });
    }

    /// The avatar full-body tracking drives: the local player's Humanoid IK
    /// object, else (for default tracker offsets) any humanoid avatar.
    pub(crate) fn local_avatar(&self) -> Option<HumanoidAvatar<'_>> {
        self.humanoid_avatars().into_iter().next()
    }

    /// The local player's tracker targets (as set by `set_ik_targets`) in
    /// world space: tracking space sits on the scene's VR Player rig, or the
    /// world origin without one.
    pub fn local_tracking_world(&self) -> Option<citrus_core::TrackerTargets> {
        let t = self.ik_targets.filter(|t| t.any())?;
        let root = self
            .objects
            .iter()
            .position(|o| o.components.iter().any(|c| c.as_any().is::<citrus_core::VrPlayer>()))
            .map(|i| self.world_transform(i))
            .unwrap_or(Mat4::IDENTITY);
        Some(t.transformed(root))
    }

    /// World-space targets for each active object's Humanoid IK, with its
    /// solver settings.
    pub(crate) fn humanoid_ik_inputs(
        &self,
        active: &[bool],
        world: &[Mat4],
    ) -> HashMap<usize, (citrus_core::TrackerTargets, crate::humanoid::BodyIkSettings)> {
        use citrus_core::{HumanoidIk, IkSource, NetworkAvatar};
        let mut out = HashMap::new();
        let stage = self.ik_targets.filter(|t| t.any());
        let default_root = self
            .objects
            .iter()
            .position(|o| o.components.iter().any(|c| c.as_any().is::<citrus_core::VrPlayer>()));
        for (i, o) in self.objects.iter().enumerate() {
            if !active.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(ik) = o.components.iter().find_map(|c| c.as_any().downcast_ref::<HumanoidIk>()) else { continue };
            let net: Option<&NetworkAvatar> = self.network_avatar_of(i).map(|(_, a)| a);
            let objects = || {
                let mut slots = [None; citrus_core::TRACKER_SLOTS];
                for (k, r) in ik.objects.iter().enumerate().take(citrus_core::TRACKER_SLOTS) {
                    if let Some(j) = r.id().and_then(|id| self.index_of(id)) {
                        let (_, rotation, position) = world[j].to_scale_rotation_translation();
                        slots[k] = Some((position, rotation));
                    }
                }
                let targets = citrus_core::TrackerTargets::from_slots(slots);
                targets.any().then_some(targets)
            };
            let local = || {
                let root = ik.tracking_root.id().and_then(|id| self.index_of(id)).or(default_root);
                stage.map(|t| t.transformed(root.map(|r| world[r]).unwrap_or(Mat4::IDENTITY))).or_else(objects)
            };
            let targets = match ik.source {
                IkSource::Auto => match net {
                    Some(av) if av.remote => Some(av.targets),
                    _ => local(),
                },
                IkSource::LocalTrackers => local(),
                IkSource::Network => net.map(|av| av.targets),
                IkSource::Objects => objects(),
            };
            let Some(targets) = targets.filter(|t| t.any()) else { continue };
            let settings = crate::humanoid::BodyIkSettings {
                weight: ik.weight,
                spine_twist: ik.spine_twist,
                shoulder_rotation: ik.shoulder_rotation,
                plant_feet: ik.plant_feet,
                step_distance: ik.step_distance,
                step_angle: ik.step_angle.to_radians(),
                step_time: ik.step_time,
                step_height: ik.step_height,
                arm_reach: match (ik.source, net) {
                    (IkSource::Auto, Some(av)) if av.remote => 1.0,
                    (IkSource::Auto | IkSource::LocalTrackers, _) => self.local_body.arm_reach,
                    _ => 1.0,
                },
            };
            out.insert(i, (targets.transformed(world[i].inverse()), settings));
        }
        out
    }

    /// Approximate ground height + up-ish normal under a world point, from a
    /// downward ray against static render objects' AABBs. Only hits at or below
    /// `ceiling` count, which keeps a foot from grounding on the avatar's own AABB
    /// (its top sits at head height, far above the foot). `None` = no ground.
    ///
    /// AABB-coarse for now (box tops, flat normals) — a triangle-accurate raycast
    /// against the terrain mesh is the follow-up; the foot-IK math already takes a
    /// real `(height, normal)` so only this sampler needs upgrading.
    pub fn ground_height(&self, x: f32, z: f32, ceiling: f32) -> Option<(f32, Vec3)> {
        let origin = Vec3::new(x, 1.0e4, z);
        let dir = Vec3::NEG_Y;
        let mut best: Option<f32> = None;
        for (i, object) in self.objects.iter().enumerate() {
            if object.render.is_none() || !self.is_active(i) {
                continue;
            }
            let world = self.world_transform(i);
            let inv = world.inverse();
            let lo = inv.transform_point3(origin);
            let ld = inv.transform_vector3(dir);
            for render in object.render_slots() {
                let (min, max) = self.mesh_bounds[render.mesh];
                if let Some(t_local) = ray_aabb(lo, ld, min, max) {
                    let hit = world.transform_point3(lo + ld * t_local);
                    if hit.y <= ceiling {
                        best = Some(best.map_or(hit.y, |b| b.max(hit.y)));
                    }
                }
            }
        }
        best.map(|y| (y, Vec3::Y))
    }

    pub fn update_skinning(&mut self, renderer: &mut Renderer, dt: f32) {
        if self.skinned_meshes.is_empty() {
            return;
        }
        let skinned_by_mesh: HashMap<usize, usize> = self
            .skinned_meshes
            .iter()
            .enumerate()
            .map(|(k, sm)| (sm.mesh_index, k))
            .collect();
        let active = self.active_flags();
        let world = self.world_transforms();
        // Source meshes still drawn by some object without an Animator; only
        // those need the shared (whole-scene) pose.
        let mut shared_users: std::collections::HashSet<usize> = Default::default();
        let mut alive: std::collections::HashSet<ObjectId> = Default::default();
        // Bone cloth: collider shapes other objects' cloth can list, and per
        // shared mesh the object whose cloth poses it.
        let any_cloth = self.objects.iter().any(|o| crate::cloth::has_cloth(&o.components));
        let cloth_shapes = if any_cloth {
            crate::cloth::shapes_by_object(
                self.objects
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| active[*i])
                    .map(|(i, o)| (o.id, o.components.as_slice(), world[i])),
            )
        } else {
            HashMap::new()
        };
        let mut cloth_owner: HashMap<usize, usize> = HashMap::new();
        let cloth_time = self.cloth_time;
        let viewer = if any_cloth { self.cloth_viewer.or_else(|| self.main_camera_view_proj(1.0).map(|v| v.2)) } else { None };
        let winds: Vec<citrus_core::WindSample> = if any_cloth {
            self.objects
                .iter()
                .enumerate()
                .filter(|(i, _)| active[*i])
                .filter_map(|(i, o)| {
                    o.components
                        .iter()
                        .find_map(|c| c.as_any().downcast_ref::<citrus_core::ClothWind>())
                        .map(|w| citrus_core::WindSample::new(w, world[i]))
                })
                .collect()
        } else {
            Vec::new()
        };
        let ik_inputs = self.humanoid_ik_inputs(&active, &world);

        for i in 0..self.objects.len() {
            if !active[i] {
                continue;
            }
            let skinned_slots: Vec<usize> = self.objects[i]
                .render_slots()
                .map(|r| r.mesh)
                .filter(|m| skinned_by_mesh.contains_key(m))
                .collect();
            if skinned_slots.is_empty() {
                continue;
            }
            let id = self.objects[i].id;
            let components = &self.objects[i].components;
            let has_ik = components.iter().any(|c| c.as_any().is::<citrus_core::HumanoidIk>());
            let has_face = components.iter().any(|c| {
                c.as_any().is::<citrus_core::FaceTracking>()
                    || c.as_any().is::<citrus_core::BlendShapes>()
                    || c.as_any().is::<citrus_core::LipSync>()
            });
            if !has_ik
                && !has_face
                && !crate::cloth::has_mesh_cloth(components)
                && !components.iter().any(|c| c.as_any().is::<citrus_core::Animator>())
            {
                if any_cloth && crate::cloth::has_cloth(components) {
                    for &m in &skinned_slots {
                        cloth_owner.entry(m).or_insert(i);
                    }
                }
                shared_users.extend(skinned_slots);
                continue;
            }
            alive.insert(id);
            let skeleton = self.skinned_meshes[skinned_by_mesh[&skinned_slots[0]]].skeleton;
            let Some(skel) = self.skeletons.get(skeleton) else {
                continue;
            };
            let animator = self.objects[i]
                .components
                .iter_mut()
                .find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::Animator>());
            let (mut locals, gpu) = match animator {
                Some(animator) => {
                    let clips: Vec<usize> = (0..self.animations.len())
                        .filter(|&c| self.clip_skeleton.get(c) == Some(&skeleton))
                        .collect();
                    let (locals, root_local) = if crate::anim_graph::wants_graph(animator) {
                        let ctrl = self.anim_assets.prepare(animator);
                        let rig = self.anim_assets.skeleton_rig(skeleton, skel);
                        if animator.state.clips.len() != clips.len() {
                            animator.state.clips = clips.iter().map(|&c| self.animations[c].name.clone()).collect();
                        }
                        let set = crate::anim_graph::ClipSet {
                            imported: &self.animations,
                            indices: &clips,
                            assets: &self.anim_assets.clips,
                        };
                        let out = crate::anim_graph::advance(animator, ctrl.as_deref(), &rig, &set, dt);
                        (out.pose.to_locals(), out.root_delta)
                    } else {
                        advance_animator(animator, skel, &self.animations, &clips, dt)
                    };
                    animator.state.root_delta = world[i].transform_vector3(root_local);
                    (locals, animator.gpu_skinning)
                }
                None => (skel.rest_locals(), true),
            };
            // Morphed and cloth vertices are written by the CPU skinning path.
            if let Some(ik) = self.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::HumanoidIk>())
                && !ik.locomotion.animate(ik_inputs.get(&i).map(|(targets, _)| targets))
            {
                locals = skel.rest_locals();
                if let Some(animator) = self.objects[i].components.iter_mut().find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::Animator>()) {
                    animator.state.root_delta = Vec3::ZERO;
                }
            }
            let has_mesh_cloth = any_cloth && crate::cloth::has_mesh_cloth(&self.objects[i].components);
            let gpu = gpu && !has_face && !has_mesh_cloth;
            if has_ik {
                let descriptor = std::iter::successors(Some(i), |&k| self.objects[k].parent).find_map(|k| {
                    self.objects[k].components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::AvatarDescriptor>())
                });
                let rig = match descriptor {
                    Some(desc) if !desc.bone_map.is_empty() => crate::humanoid::HumanoidRig::map_with_overrides(skel, &desc.bone_map).0,
                    _ => crate::humanoid::HumanoidRig::map(skel),
                };
                let input = ik_inputs.get(&i);
                if let Some((targets, settings)) = input {
                    let state = self.body_ik_state.entry(id).or_default();
                    crate::humanoid::solve_body(skel, &rig, &mut locals, targets, settings, state, world[i], dt);
                }
                if let Some(ik) = self.objects[i]
                    .components
                    .iter_mut()
                    .find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::HumanoidIk>())
                {
                    ik.runtime.humanoid = rig.is_humanoid();
                    ik.runtime.targets = input.map(|(t, _)| *t).unwrap_or_default();
                }
                if self.avatar_preview_for == Some(i) {
                    self.avatar_preview = Some(AvatarPose {
                        joints: skel.world_matrices(&locals).iter().map(|m| world[i].transform_point3(m.w_axis.truncate())).collect(),
                        parents: skel.joints.iter().map(|j| j.parent).collect(),
                    });
                }
            }
            let morph_weights: Vec<Vec<f32>> = if has_face {
                let avatar = std::iter::successors(Some(i), |&k| self.objects[k].parent).find_map(|k| {
                    self.objects[k]
                        .components
                        .iter()
                        .find_map(|c| c.as_any().downcast_ref::<citrus_core::NetworkAvatar>())
                        .map(|av| (self.objects[k].id, av.remote, av.face.clone()))
                });
                let net_face = avatar.as_ref().map(|(_, remote, face)| (*remote, face.clone()));
                let visemes = match &avatar {
                    Some((id, true, _)) => self.remote_visemes.get(id).copied(),
                    _ => self.local_visemes,
                };
                let morphs: Vec<&[citrus_assets::MorphTarget]> =
                    skinned_slots.iter().map(|m| self.skinned_meshes[skinned_by_mesh[m]].morphs.as_slice()).collect();
                let out = crate::face_drive::drive(
                    &mut self.objects[i].components,
                    &morphs,
                    self.local_face.as_ref(),
                    net_face,
                    visemes.as_ref(),
                    dt,
                );
                if let Some((left, right, max)) = out.eyes {
                    let rig = crate::humanoid::HumanoidRig::map(skel);
                    crate::humanoid::apply_eye_gaze(skel, &rig, &mut locals, left, right, max);
                }
                out.weights
            } else {
                Vec::new()
            };
            if any_cloth {
                let verts: Vec<&[citrus_render::Vertex]> = skinned_slots
                    .iter()
                    .map(|m| self.skinned_meshes[skinned_by_mesh[m]].base_vertices.as_slice())
                    .collect();
                crate::cloth::step(&mut self.objects[i].components, skel, &verts, &mut locals, world[i], &cloth_shapes, &winds, viewer, cloth_time, dt);
            }
            let palette = skel.palette(&locals);

            if self.anim_instances.get(&id).is_none_or(|inst| inst.gpu != gpu) {
                let mut slots = Vec::new();
                for &m in &skinned_slots {
                    let sm = &self.skinned_meshes[skinned_by_mesh[&m]];
                    let data = citrus_render::MeshData {
                        vertices: sm.base_vertices.clone(),
                        indices: self.mesh_geometry[m].1.clone(),
                        has_lightmap_uv: false,
                    };
                    let uploaded = if gpu {
                        renderer.upload_mesh_gpu_skinned(&data, skel.joints.len())
                    } else {
                        renderer.upload_mesh_skinned(&data)
                    };
                    match uploaded {
                        Ok(handle) => slots.push((m, handle)),
                        Err(e) => tracing::error!("animator mesh upload: {e:#}"),
                    }
                }
                self.anim_instances.insert(id, AnimInstance { slots, gpu });
            }
            for &(m, _) in &self.anim_instances[&id].slots {
                let sm = &self.skinned_meshes[skinned_by_mesh[&m]];
                if let Some(b) = posed_bounds(&sm.joint_boxes, &palette) {
                    self.pose_bounds.insert((id, m), b);
                }
            }
            for (k, &(m, handle)) in self.anim_instances[&id].slots.iter().enumerate() {
                if gpu {
                    renderer.skin_mesh_gpu(handle, &palette);
                } else {
                    let sm = &self.skinned_meshes[skinned_by_mesh[&m]];
                    let weights = morph_weights.get(k).map(Vec::as_slice).unwrap_or(&[]);
                    let mut skinned = match crate::face_drive::morphed(&sm.base_vertices, &sm.morphs, weights) {
                        Some(v) => skin_vertices(&v, &palette),
                        None => skin_vertices(&sm.base_vertices, &palette),
                    };
                    let slot = if has_mesh_cloth { self.objects[i].render_slots().position(|r| r.mesh == m) } else { None };
                    if let Some(slot) = slot {
                        crate::cloth::step_mesh(
                            &mut self.objects[i].components,
                            slot,
                            skel,
                            &sm.base_vertices,
                            &mut skinned,
                            &self.mesh_geometry[m].1,
                            world[i],
                            &cloth_shapes,
                            &winds,
                            viewer,
                            cloth_time,
                            dt,
                        );
                    }
                    renderer.update_mesh_vertices(handle, &skinned);
                }
            }
        }
        self.anim_instances.retain(|id, _| alive.contains(id));
        self.body_ik_state.retain(|id, _| alive.contains(id));
        self.pose_bounds.retain(|(id, _), _| alive.contains(id));

        // Shared path (objects without an Animator): IK targets pose humanoid
        // avatars when present; otherwise the skeleton's first clip loops.
        let ik = self.ik_targets;
        let ik_active = ik
            .map(|t| {
                t.head.is_some()
                    || t.left_hand.is_some()
                    || t.right_hand.is_some()
                    || t.hips.is_some()
                    || t.left_foot.is_some()
                    || t.right_foot.is_some()
            })
            .unwrap_or(false);
        if !ik_active && self.animations.is_empty() && cloth_owner.is_empty() {
            return;
        }
        self.anim_time += dt;
        // Cloth-posed locals per owner object, so every mesh of one model
        // shares a single simulation step this frame.
        let mut cloth_posed: HashMap<usize, Vec<Mat4>> = HashMap::new();
        for sm in &self.skinned_meshes {
            if !shared_users.contains(&sm.mesh_index) {
                continue;
            }
            let owner = cloth_owner.get(&sm.mesh_index).copied();
            if !ik_active && self.animations.is_empty() && owner.is_none() {
                continue;
            }
            let Some(skel) = self.skeletons.get(sm.skeleton) else {
                continue;
            };
            let first_clip = (0..self.animations.len())
                .find(|&c| self.clip_skeleton.get(c) == Some(&sm.skeleton))
                .unwrap_or(0);
            let mut locals = match ik {
                Some(targets)
                    if ik_active && crate::humanoid::HumanoidRig::map(skel).is_humanoid() =>
                {
                    crate::humanoid::pose_from_trackers(skel, &targets)
                }
                _ => match self.animations.get(first_clip) {
                    Some(clip) => clip.sample(skel, self.anim_time),
                    None => skel.rest_locals(),
                },
            };
            // Terrain foot IK: plant the feet on the ground under them. The ground
            // sampler caps hits at the foot height + max_lift so the avatar never
            // grounds on its own AABB.
            if let Some(params) = self.foot_ik
                && crate::humanoid::HumanoidRig::map(skel).is_humanoid() {
                    let max_lift = params.max_lift;
                    crate::humanoid::apply_foot_ik(skel, &mut locals, &params, |p| {
                        self.ground_height(p.x, p.z, p.y + max_lift)
                    });
                }
            if let Some(o) = owner {
                if let Some(posed) = cloth_posed.get(&o) {
                    locals.clone_from(posed);
                } else {
                    let verts: Vec<&[citrus_render::Vertex]> = self
                        .skinned_meshes
                        .iter()
                        .filter(|m| cloth_owner.get(&m.mesh_index) == Some(&o) && m.skeleton == sm.skeleton)
                        .map(|m| m.base_vertices.as_slice())
                        .collect();
                    crate::cloth::step(&mut self.objects[o].components, skel, &verts, &mut locals, world[o], &cloth_shapes, &winds, viewer, cloth_time, dt);
                    cloth_posed.insert(o, locals.clone());
                }
            }
            let palette = skel.palette(&locals);
            if let Some(b) = posed_bounds(&sm.joint_boxes, &palette) {
                self.shared_pose_bounds.insert(sm.mesh_index, b);
            }
            renderer.update_mesh_vertices(
                self.mesh_handles[sm.mesh_index],
                &skin_vertices(&sm.base_vertices, &palette),
            );
        }
    }

    /// Whether an object draws a skinned mesh carrying blendshapes.
    pub fn has_blendshapes(&self, i: usize) -> bool {
        self.objects.get(i).is_some_and(|o| {
            o.render_slots().any(|r| self.skinned_meshes.iter().any(|sm| sm.mesh_index == r.mesh && !sm.morphs.is_empty()))
        })
    }

    /// Whether an object draws a skinned (rigged) mesh, i.e. its Animator
    /// drives bones rather than object transforms.
    pub fn has_skinned_mesh(&self, i: usize) -> bool {
        self.objects
            .get(i)
            .is_some_and(|o| o.render_slots().any(|r| self.skinned_meshes.iter().any(|sm| sm.mesh_index == r.mesh)))
    }

    /// Bone names and rest transforms of an object's skinned mesh (empty when
    /// it has none).
    #[cfg_attr(not(feature = "editor"), allow(dead_code))]
    pub fn skeleton_rest_pose(&self, i: usize) -> Vec<(String, crate::anim_graph::Trs)> {
        let Some(o) = self.objects.get(i) else {
            return Vec::new();
        };
        o.render_slots()
            .find_map(|r| self.skinned_meshes.iter().find(|sm| sm.mesh_index == r.mesh))
            .and_then(|sm| self.skeletons.get(sm.skeleton))
            .map(|sk| {
                sk.joints
                    .iter()
                    .map(|j| {
                        (j.name.clone(), crate::anim_graph::Trs { t: j.rest_translation, r: j.rest_rotation, s: j.rest_scale })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// GPU mesh to draw for an object's render slot: its Animator copy when it
    /// has one, else the shared mesh.
    fn draw_mesh(&self, id: ObjectId, mesh: usize) -> MeshHandle {
        self.anim_instances
            .get(&id)
            .and_then(|inst| inst.slots.iter().find(|(m, _)| *m == mesh).map(|(_, h)| *h))
            .unwrap_or(self.mesh_handles[mesh])
    }

    /// Distinct model paths (project-relative) used by active objects that have a
    /// **generated** lightmap UV — i.e. a `.lmuv` marker exists next to the model,
    /// so its meshes were auto-unwrapped on load. The FluxBaker tab lists these so
    /// they can be regenerated (re-run with the current unwrapper) or reverted.
    pub fn models_with_generated_lightmap_uv(&self, project_root: &Path) -> Vec<String> {
        use citrus_assets::ObjectSource;
        let mut out: Vec<String> = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            if let ObjectSource::Model { path, .. } = &self.objects[i].source
                && !out.contains(path)
                && citrus_assets::lightmap_uv_marker_path(project_root.join(path)).exists()
            {
                out.push(path.clone());
            }
        }
        out
    }

    /// Distinct model file paths (project-relative) of **static** objects whose
    /// mesh has NO real lightmap UV — i.e. models the bake would skip until a
    /// lightmap unwrap is generated. The editor uses this to offer "generate
    /// lightmap UVs" before a bake. Empty when every static object is bakeable.
    pub fn models_needing_lightmap_uv(&self) -> Vec<String> {
        use citrus_assets::ObjectSource;
        let mut out: Vec<String> = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) || !self.objects[i].static_geometry {
                continue;
            }
            let Some(r) = self.objects[i].render else { continue };
            if self.mesh_has_lightmap_uv.get(r.mesh).copied().unwrap_or(false) {
                continue;
            }
            if let ObjectSource::Model { path, .. } = &self.objects[i].source
                && !out.contains(path)
            {
                out.push(path.clone());
            }
        }
        out
    }

    /// The reflection-probe zone covering `camera_pos` (or the nearest), as a
    /// world-space box for box-projected reflections. `None` when the scene has
    /// no `ReflectionProbe` — the env cube is then sampled as distant/infinite.
    /// Every active reflection probe as (object id, capture point, resolution),
    /// for baking each probe's cube.
    pub fn reflection_probe_captures(&self) -> Vec<(u128, Vec3, u32)> {
        use citrus_core::ReflectionProbe;
        (0..self.objects.len())
            .filter(|&i| self.is_active(i))
            .filter_map(|i| {
                let probe = self.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<ReflectionProbe>())?;
                let capture = self.world_transform(i).w_axis.truncate() + Vec3::from(probe.capture_offset);
                Some((self.objects[i].id.raw(), capture, probe.resolution))
            })
            .collect()
    }

    pub fn active_reflection_probe(&self, camera_pos: Vec3) -> Option<citrus_render::ReflectionProbeBox> {
        self.active_reflection_probes(camera_pos)[0]
    }

    /// The two highest-priority reflection probes for `camera_pos`, best first
    /// (see [`active_reflection_probe`](Self::active_reflection_probe) for the
    /// ranking). The second one is blended per pixel with the first.
    pub fn active_reflection_probes(&self, camera_pos: Vec3) -> [Option<citrus_render::ReflectionProbeBox>; 2] {
        use citrus_core::ReflectionProbe;
        // Unreal-style priority: among probes whose box CONTAINS the point, the
        // highest Importance wins, tie-broken by the smallest box (a small specific
        // probe overrides a large surrounding one). When no probe contains the
        // point, the nearest box is used. Rank key (higher = better):
        // (contains, importance, -box_volume, -outside_distance).
        let mut ranked: Vec<((bool, i32, f32, f32), citrus_render::ReflectionProbeBox)> = Vec::new();
        for i in 0..self.objects.len() {
            if !self.frame_active(i) {
                continue;
            }
            let Some(probe) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<ReflectionProbe>())
            else {
                continue;
            };
            let origin = self.frame_world(i).w_axis.truncate();
            // Box (parallax proxy) and capture point are both offsets from the
            // object, so the capture can sit anywhere inside the room box.
            let center = origin + Vec3::from(probe.box_offset);
            let capture = origin + Vec3::from(probe.capture_offset);
            let half = Vec3::from(probe.size) * 0.5 * self.objects[i].scale;
            let d = (camera_pos - center).abs() - half;
            let outside = d.max(Vec3::ZERO).length();
            let contains = d.max_element() <= 0.0;
            let volume = (half.x * half.y * half.z).max(1e-4);
            let key = (contains, probe.importance, -volume, -outside);
            ranked.push((
                key,
                citrus_render::ReflectionProbeBox {
                    center: center.to_array(),
                    capture: capture.to_array(),
                    half_extents: half.to_array(),
                    intensity: probe.intensity.max(0.0),
                    box_projection: probe.box_projection,
                    resolution: probe.resolution,
                    realtime: probe.mode == citrus_core::ReflectionProbeMode::Realtime,
                    id: self.objects[i].id.raw(),
                    blend_distance: probe.blend_distance.max(0.0) * self.objects[i].scale.max_element(),
                    importance: probe.importance,
                    update_hz: probe.update_hz.max(0.0),
                    time_slicing: probe.time_slicing,
                    include_dynamic: probe.include_dynamic,
                },
            ));
        }
        // Best first: lexicographic on (contains, importance), then the f32 lanes.
        ranked.sort_by(|(a, _), (b, _)| {
            (b.0, b.1)
                .cmp(&(a.0, a.1))
                .then_with(|| (b.2, b.3).partial_cmp(&(a.2, a.3)).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut it = ranked.into_iter().map(|(_, p)| p);
        [it.next(), it.next()]
    }

    /// Whether object `i` counts as a planar-reflection surface this frame:
    /// active, drawn (has a mesh, layer visible) and carries the component.
    /// Shared by `sync_draws_multi` (index assignment) and `planar_surfaces()`
    /// (per-frame list) so indices line up.
    fn planar_surface_eligible(&self, i: usize) -> bool {
        self.objects[i].render.is_some()
            && self.frame_active(i)
            && self.objects[i].cast_shadows.visible()
            && self.visible_layers & (1u32 << (self.objects[i].layer as u32 & 31)) != 0
            && self.objects[i]
                .components
                .iter()
                .any(|c| c.as_any().is::<citrus_core::PlanarReflection>())
    }

    /// Active planar-reflection surfaces (first `MAX_PLANAR` eligible objects),
    /// index-aligned with the `DrawCmd::planar_index` values from `sync_draws`.
    /// The plane is the object's world +Y through its origin.
    pub fn planar_surfaces(&self) -> Vec<citrus_render::PlanarSurface> {
        let mut out = Vec::new();
        for i in 0..self.objects.len() {
            if out.len() >= citrus_render::MAX_PLANAR {
                break;
            }
            if !self.planar_surface_eligible(i) {
                continue;
            }
            let Some(p) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<citrus_core::PlanarReflection>())
            else {
                continue;
            };
            let world = self.world_transform(i);
            out.push(citrus_render::PlanarSurface {
                point: world.w_axis.truncate().to_array(),
                normal: world.y_axis.truncate().normalize_or(Vec3::Y).to_array(),
                max_roughness: p.max_roughness,
            });
        }
        out
    }

    /// Scene fog parameters for the renderer, or `None` when fog is disabled.
    pub fn fog_params(&self) -> Option<citrus_render::FogParams> {
        let e = &self.environment;
        e.fog_enabled.then_some(citrus_render::FogParams {
            color: e.fog_color,
            density: e.fog_density,
            height_falloff: e.fog_height_falloff,
            height_ref: e.fog_height_ref,
            start_distance: e.fog_start_distance,
        })
    }

    /// The post-processing at `camera_pos`: the scene's profile, the
    /// volumes around it by priority, and on top of all of them the water's
    /// underwater profile while the camera is below a surface (`water_t` the
    /// water clock).
    pub fn effective_postfx(
        &mut self,
        camera_pos: Vec3,
        project_root: &std::path::Path,
        water_t: f32,
    ) -> citrus_render::PostFx {
        use citrus_core::VolumeComponent;

        // (priority, weighted profile) for each contributing volume.
        let mut stack: Vec<(f32, citrus_assets::PostFxProfile, f32)> = Vec::new();
        let mut overrides = Vec::new();
        for i in 0..self.objects.len() {
            if !self.frame_active(i) {
                continue;
            }
            let Some(vol) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<VolumeComponent>())
            else {
                continue;
            };
            if vol.profile.trim().is_empty() || vol.weight <= 0.0 {
                continue;
            }
            // Weight: global = full; local = fade by distance to the box.
            let weight = if vol.global {
                vol.weight
            } else {
                // The box lives in the object's local space (rotated + scaled with
                // it, as drawn in the editor); the fade distance is in world units.
                let world = self.frame_world(i);
                let half = Vec3::from(vol.half_extents).abs();
                let local = world.inverse().transform_point3(camera_pos);
                let closest = world.transform_point3(local.clamp(-half, half));
                let outside = (camera_pos - closest).length();
                if outside <= 0.0 {
                    vol.weight
                } else if outside < vol.blend_distance.max(1e-3) {
                    vol.weight * (1.0 - outside / vol.blend_distance.max(1e-3))
                } else {
                    0.0
                }
            };
            if weight <= 0.0 {
                continue;
            }
            // Load + cache the profile.
            if !self.postfx_cache.contains_key(&vol.profile) {
                let path = citrus_core::asset_ref::resolve_path(project_root, &vol.profile);
                let profile = citrus_assets::load_postfx(&path).unwrap_or_default();
                self.postfx_cache.insert(vol.profile.clone(), profile);
            }
            let profile = self.postfx_cache[&vol.profile];
            stack.push((vol.priority, profile, weight.clamp(0.0, 1.0)));
            let effects: Vec<&'static str> = [
                (profile.tonemap.enabled, "tonemap"),
                (profile.bloom.enabled, "bloom"),
                (profile.color_grading.enabled, "color grading"),
                (profile.vignette.enabled, "vignette"),
                (profile.chromatic_aberration.enabled, "chromatic aberration"),
            ]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect();
            overrides.push((self.objects[i].name.clone(), weight.clamp(0.0, 1.0), effects));
        }
        if let Some(under) = crate::underwater::probe(self, camera_pos, water_t, 0.0)
            && !under.settings.postfx.trim().is_empty()
            && under.depth > 0.0
        {
            let key = under.settings.postfx.clone();
            if !self.postfx_cache.contains_key(&key) {
                let path = citrus_core::asset_ref::resolve_path(project_root, &key);
                let profile = citrus_assets::load_postfx(&path).unwrap_or_default();
                self.postfx_cache.insert(key.clone(), profile);
            }
            // Eases in over the first few centimetres below the surface.
            let weight = (under.depth / 0.05).clamp(0.0, 1.0);
            stack.push((f32::INFINITY, self.postfx_cache[&key], weight));
            overrides.push(("Under water".to_owned(), weight, Vec::new()));
        }
        self.postfx_overrides = overrides;

        stack.sort_by(|a, b| a.0.total_cmp(&b.0));
        // The scene's global postfx is the always-present base; volumes blend on
        // top by priority, overriding only the effects they enable.
        let blend: Vec<(citrus_assets::PostFxProfile, f32)> = stack.into_iter().map(|(_, p, w)| (p, w)).collect();
        let p = citrus_assets::blend_over(self.environment.postfx, &blend);

        let tonemap = match p.tonemap.mode {
            citrus_assets::TonemapMode::None => 0,
            citrus_assets::TonemapMode::Reinhard => 1,
            citrus_assets::TonemapMode::Aces => 2,
        };
        citrus_render::PostFx {
            tonemap,
            exposure: p.tonemap.exposure,
            grading_enabled: p.color_grading.enabled,
            grade_exposure: p.color_grading.exposure,
            contrast: p.color_grading.contrast,
            saturation: p.color_grading.saturation,
            temperature: p.color_grading.temperature,
            tint: p.color_grading.tint,
            vignette_enabled: p.vignette.enabled,
            vignette_intensity: p.vignette.intensity,
            vignette_smoothness: p.vignette.smoothness,
            vignette_color: p.vignette.color,
            bloom_enabled: p.bloom.enabled,
            bloom_threshold: p.bloom.threshold,
            bloom_intensity: p.bloom.intensity,
            bloom_radius: p.bloom.radius,
            ca_enabled: p.chromatic_aberration.enabled,
            ca_intensity: p.chromatic_aberration.intensity,
            underwater: None,
        }
    }

    /// Drop cached `.postfx` profiles so an edit to a profile file is picked up
    /// live by the volumes that reference it.
    pub fn invalidate_postfx_cache(&mut self) {
        self.postfx_cache.clear();
    }

    /// Object-space AABB (min, max) of an object's render mesh, for the mesh
    /// collider wireframe.
    pub fn render_mesh_bounds(&self, index: usize) -> Option<(Vec3, Vec3)> {
        self.objects[index]
            .render
            .map(|r| self.mesh_bounds[r.mesh])
    }

    /// Flatten every active `AudioSource` into a per-frame cue list (with
    /// world position) for the audio engine.
    pub fn gather_audio(&self) -> Vec<crate::audio::AudioCue> {
        use citrus_core::AudioSource;
        let mut cues = Vec::new();
        for i in 0..self.objects.len() {
            if !self.frame_active(i) {
                continue;
            }
            if let Some(src) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<AudioSource>())
            {
                let pos = self.frame_world(i).w_axis.truncate();
                cues.push(crate::audio::AudioCue::from_source(self.objects[i].id, src, pos));
            }
        }
        cues
    }

    /// World position of the first active `AudioListener` (the spatial "ears").
    pub fn audio_listener(&self) -> Option<Vec3> {
        use citrus_core::AudioListener;
        for i in 0..self.objects.len() {
            if !self.frame_active(i) {
                continue;
            }
            if self.objects[i]
                .components
                .iter()
                .any(|c| c.as_any().downcast_ref::<AudioListener>().is_some())
            {
                return Some(self.frame_world(i).w_axis.truncate());
            }
        }
        None
    }

    /// Sync all draw transforms from object TRS (cheap; runs every frame).
    pub fn sync_draws(&mut self, selected: Option<usize>, highlight: f32) {
        match selected {
            Some(i) => self.sync_draws_multi(&[i], highlight),
            None => self.sync_draws_multi(&[], highlight),
        }
    }

    /// Like [`sync_draws`] but highlights every object in `selected` (multi-select
    /// outlines). The first entry is the anchor; all get the same highlight.
    pub fn sync_draws_multi(&mut self, selected: &[usize], highlight: f32) {
        self.draws.clear();
        // One O(n) memoized pass instead of re-walking each parent chain per object.
        let world = self.render_world_transforms();
        self.frame_cache_active = (0..self.objects.len()).map(|i| self.is_active(i)).collect();
        // Locked reflection-probe boxes stay fixed in world space: compensate
        // box_offset by however far the object moved since last frame, so
        // dragging the probe repositions only the capture point inside the box.
        for (object, w) in self.objects.iter_mut().zip(world.iter()) {
            let pos = w.w_axis.truncate();
            let Some(probe) = object
                .components
                .iter_mut()
                .find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::ReflectionProbe>())
            else {
                continue;
            };
            if probe.lock_box
                && let Some(last) = probe.last_world_pos
            {
                let delta = pos - Vec3::from(last);
                if delta.length_squared() > 0.0 {
                    probe.box_offset = (Vec3::from(probe.box_offset) - delta).to_array();
                }
            }
            probe.last_world_pos = Some(pos.to_array());
        }
        // Planar-reflection surfaces: assign each drawn object with a
        // PlanarReflection component its layer index, in the SAME order (and
        // with the same visibility filter) as `planar_surfaces()` builds the
        // per-frame list, so DrawCmd.planar_index matches FrameInput.planar.
        let occ_cell = match (&self.occlusion, self.occlusion_camera) {
            (Some(o), Some(cam)) => o.cell_of(cam),
            _ => None,
        };
        let mut planar_idx = vec![-1i32; self.objects.len()];
        let mut planar_n = 0i32;
        for (i, slot) in planar_idx.iter_mut().enumerate() {
            if planar_n >= citrus_render::MAX_PLANAR as i32 {
                break;
            }
            if !self.planar_surface_eligible(i) {
                continue;
            }
            *slot = planar_n;
            planar_n += 1;
        }
        for i in 0..self.objects.len() {
            if self.objects[i].render.is_none() {
                continue;
            }
            if !self.is_active(i) {
                continue;
            }
            // Layer culling: skip objects whose layer bit is clear in the active
            // visibility mask (editor viewport toggle / game camera culling mask).
            if self.visible_layers & (1u32 << (self.objects[i].layer as u32 & 31)) == 0 {
                continue;
            }
            let lightmap_layer = self
                .baked
                .as_ref()
                .and_then(|b| b.object_lightmap.get(&i))
                .map(|&l| l as i32)
                .unwrap_or(-1);
            // For the UV-checker preview: the would-be lightmap resolution, shown
            // ONLY for objects that are BOTH static AND have a real non-overlapping
            // lightmap UV (their own UV1 / a generated unwrap). An object reusing
            // its uv0 isn't bakeable, so it must not show the checker. 0 = no
            // checker.
            let has_lm_uv = self.objects[i]
                .render
                .is_some_and(|r| self.mesh_has_lightmap_uv.get(r.mesh).copied().unwrap_or(false));
            // Only the UV-checker preview reads it (it decomposes the matrix).
            let lightmap_size = if self.lightmap_preview && self.objects[i].static_geometry && has_lm_uv {
                self.lightmap_size_for_world(i, world[i])
            } else {
                0
            };
            let transform = world[i];
            let highlight = if selected.contains(&i) { highlight } else { 0.0 };
            let occluded = self.objects[i].static_geometry
                && match (occ_cell, &self.occlusion) {
                    (Some(c), Some(o)) => !o.visible(c, self.objects[i].id),
                    _ => false,
                };
            let Ok(lod) = self.lod_level(i, transform.w_axis.truncate()) else {
                continue;
            };
            // One draw per material slot (slot 0 + extras), all at this transform.
            for (k, mut render) in self.objects[i].render_slots().enumerate() {
                if let Some((base, count)) = lod {
                    render.mesh = base + k.min(count - 1);
                }
                let id = self.objects[i].id;
                let (mn, mx) = self
                    .pose_bounds
                    .get(&(id, render.mesh))
                    .or_else(|| self.shared_pose_bounds.get(&render.mesh))
                    .copied()
                    .unwrap_or_else(|| self.mesh_aabb(render.mesh));
                self.draws.push(DrawCmd {
                    mesh: self.draw_mesh(self.objects[i].id, render.mesh),
                    material: self.materials[render.material].handle,
                    transform,
                    highlight,
                    mesh_center: (mn + mx) * 0.5,
                    bound_radius: ((mx - mn) * 0.5).length(),
                    lightmap_layer,
                    lightmap_size,
                    static_geometry: self.objects[i].static_geometry,
                    planar_index: planar_idx[i],
                    occluded,
                    shadow_casting: render_shadow_casting(self.objects[i].cast_shadows),
                });
            }
        }
        self.draws.extend(self.extra_draws.iter().copied());
        self.frame_cache_world = world;
    }

    /// Delete an object and its whole subtree, remapping the surviving
    /// objects' parent indices (objects are a positional Vec). Meshes and
    /// materials are left in place (slot GC is a separate concern).
    pub fn remove_object(&mut self, index: usize) {
        if index >= self.objects.len() {
            return;
        }
        // Mark the object and every descendant for removal.
        let mut remove = vec![false; self.objects.len()];
        remove[index] = true;
        // Children always have a higher index than... not guaranteed, so
        // iterate to a fixpoint over the parent links.
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..self.objects.len() {
                if remove[i] {
                    continue;
                }
                if let Some(p) = self.objects[i].parent
                    && p < remove.len()
                    && remove[p]
                {
                    remove[i] = true;
                    changed = true;
                }
            }
        }
        // old index -> new index for survivors.
        let mut remap = vec![None; self.objects.len()];
        let mut next = 0usize;
        for (i, slot) in remap.iter_mut().enumerate() {
            if !remove[i] {
                *slot = Some(next);
                next += 1;
            }
        }
        if let Some(cam) = self.active_camera_override
            && self.objects.iter().zip(&remove).any(|(o, &r)| r && o.id == cam)
        {
            self.active_camera_override = None;
        }
        let mut i = 0;
        self.objects.retain(|_| {
            let keep = !remove[i];
            i += 1;
            keep
        });
        for object in &mut self.objects {
            object.parent = object.parent.and_then(|p| remap.get(p).copied().flatten());
        }
        // Baked lightmap assignments are keyed by object index.
        if let Some(baked) = self.baked.as_mut() {
            baked.object_lightmap = baked
                .object_lightmap
                .drain()
                .filter_map(|(old, layer)| remap.get(old).copied().flatten().map(|new| (new, layer)))
                .collect();
        }
    }

    /// Name for a duplicate (editor spec §5): `base (N)` with the smallest free
    /// N >= 2, where `base` is `src_name` without a trailing `(N)` or `Copy`
    /// chain. `Spot Light` -> `Spot Light (2)`, then `(3)`; never `Copy Copy`.
    fn next_duplicate_name(&self, src_name: &str) -> String {
        let base = strip_dup_suffix(src_name);
        let existing: std::collections::HashSet<&str> =
            self.objects.iter().map(|o| o.name.as_str()).collect();
        (2..)
            .map(|n| format!("{base} ({n})"))
            .find(|c| !existing.contains(c.as_str()))
            .unwrap()
    }

    /// Duplicate an object and its whole subtree. Clones each object's
    /// transform/source/render (mesh + material are shared, not copied) and its
    /// components (via save→load through the registry), assigns fresh ids, and
    /// re-parents the copies among themselves; the duplicated root becomes a
    /// sibling of the original. Returns the new root index. Not undoable (like
    /// object deletion).
    pub fn duplicate_object(
        &mut self,
        index: usize,
        registry: &ComponentRegistry,
    ) -> Option<usize> {
        if index >= self.objects.len() {
            return None;
        }
        // Subtree = the object + all descendants (breadth-first).
        let mut subtree = vec![index];
        let mut i = 0;
        while i < subtree.len() {
            let parent = subtree[i];
            for c in 0..self.objects.len() {
                if self.objects[c].parent == Some(parent) && !subtree.contains(&c) {
                    subtree.push(c);
                }
            }
            i += 1;
        }
        let base = self.objects.len();
        let remap: std::collections::HashMap<usize, usize> = subtree
            .iter()
            .enumerate()
            .map(|(k, &old)| (old, base + k))
            .collect();

        let mut clones = Vec::with_capacity(subtree.len());
        for &old in &subtree {
            let src = &self.objects[old];
            let saved = src.save_components();
            // Root stays a sibling (keep original parent); descendants re-link
            // to the cloned parent.
            let parent = if old == index {
                src.parent
            } else {
                src.parent.and_then(|p| remap.get(&p).copied())
            };
            let mut obj = SceneObject {
                id: ObjectId::new(),
                name: if old == index {
                    self.next_duplicate_name(&src.name)
                } else {
                    src.name.clone()
                },
                render: src.render,
                extra_render: src.extra_render.clone(),
                source: src.source.clone(),
                enabled: src.enabled,
                static_geometry: src.static_geometry,
                lightmap_scale: src.lightmap_scale,
                cast_shadows: src.cast_shadows,
                layer: src.layer,
                parent,
                translation: src.translation,
                rotation: src.rotation,
                scale: src.scale,
                components: Vec::new(),
            };
            obj.load_components(&saved, registry);
            clones.push(obj);
        }
        self.objects.extend(clones);
        Some(base)
    }

    /// Ray-pick the closest object (ray vs object-space AABB).
    /// World-space distance to the nearest object the ray hits (AABB test, same
    /// as `pick`), or `None` if it hits nothing. Used to stop the VR lasers on
    /// whatever they're pointing at.
    pub fn ray_hit(&self, origin: Vec3, dir: Vec3) -> Option<f32> {
        let mut best: Option<f32> = None;
        for (i, object) in self.objects.iter().enumerate() {
            if object.render.is_none() || !self.is_active(i) {
                continue;
            }
            let world = self.world_transform(i);
            let inv = world.inverse();
            let local_origin = inv.transform_point3(origin);
            let local_dir = inv.transform_vector3(dir);
            for render in object.render_slots() {
                let (min, max) = self.mesh_bounds[render.mesh];
                if let Some(t_local) = ray_aabb(local_origin, local_dir, min, max) {
                    let hit_world = world.transform_point3(local_origin + local_dir * t_local);
                    let t = (hit_world - origin).length();
                    if best.is_none_or(|b| t < b) {
                        best = Some(t);
                    }
                }
            }
        }
        best
    }

    pub fn pick(&self, origin: Vec3, dir: Vec3) -> Option<usize> {
        self.pick_hit(origin, dir).map(|(i, _)| i)
    }

    /// The closest object the ray hits and its world distance (mesh-precise,
    /// like `pick`).
    pub fn pick_hit(&self, origin: Vec3, dir: Vec3) -> Option<(usize, f32)> {
        let mut best: Option<(usize, f32)> = None;
        for (i, object) in self.objects.iter().enumerate() {
            if object.render.is_none() {
                continue;
            }
            // A disabled object (or one under a disabled parent) is "not there":
            // it isn't rendered and must not be pickable in the viewport either.
            if !self.is_active(i) {
                continue;
            }
            let world = self.world_transform(i);
            let inv = world.inverse();
            let local_origin = inv.transform_point3(origin);
            let local_dir = inv.transform_vector3(dir);
            // Test every slot's mesh so a click on any sub-mesh selects the whole
            // object. AABB is the broad-phase reject; the PRECISE hit is ray-vs-
            // triangle against the mesh geometry. (AABB-only picking selected a big
            // object like a couch when a small object — an orb — sat INSIDE its loose
            // bounding box, because the couch's AABB front face is nearer than the
            // orb even though the couch's actual geometry is behind it.)
            for render in object.render_slots() {
                let (min, max) = self.mesh_bounds[render.mesh];
                if ray_aabb(local_origin, local_dir, min, max).is_none() {
                    continue; // misses the box entirely -> skip the triangle test
                }
                let t_local = match self.mesh_geometry.get(render.mesh) {
                    Some((pos, idx)) if !idx.is_empty() => {
                        ray_mesh(local_origin, local_dir, pos, idx)
                    }
                    // No CPU geometry (e.g. a primitive without it) -> AABB fallback.
                    _ => ray_aabb(local_origin, local_dir, min, max),
                };
                if let Some(t_local) = t_local {
                    let hit_world = world.transform_point3(local_origin + local_dir * t_local);
                    let t_world = (hit_world - origin).length();
                    if best.is_none_or(|(_, t)| t_world < t) {
                        best = Some((i, t_world));
                    }
                }
            }
        }
        best
    }

    /// Build the serialized `MaterialRef` for one material index (File if it
    /// has a backing `.material`, else an Inline snapshot).
    fn material_ref(
        &self,
        material_index: usize,
        project_root: &Path,
        shaders: &ShaderLibrary,
    ) -> MaterialRef {
        let entry = &self.materials[material_index];
        match &entry.file {
            Some(path) => MaterialRef::File(citrus_core::asset_ref::reference(project_root, &relative_to(path, project_root))),
            None if entry.model == entry.default => MaterialRef::Model,
            None => {
                let (params, features) = material_from_model(&entry.model);
                let custom = shaders
                    .get(&entry.model.shader)
                    .and_then(|e| e.source.as_ref())
                    .map(|s| s.unpack(&entry.model.custom_values))
                    .unwrap_or_default();
                MaterialRef::Inline {
                    params,
                    features,
                    shader: entry.model.shader.clone(),
                    custom,
                    render_queue: Some(entry.model.render_queue),
                    textures: tex_file_from_paths(&entry.model.textures),
                }
            }
        }
    }

    /// Serialize the current scene to a SceneFile.
    pub fn to_scene_file(&self, project_root: &Path, shaders: &ShaderLibrary) -> SceneFile {
        let default_material = || MaterialRef::Inline {
            params: MaterialParams::default(),
            features: MaterialFeatures::default(),
            shader: "standard".into(),
            custom: Default::default(),
            render_queue: None,
            textures: Default::default(),
        };
        let entries = self
            .objects
            .iter()
            .map(|object| {
                let material = match &object.render {
                    Some(render) => self.material_ref(render.material, project_root, shaders),
                    None => default_material(),
                };
                let extra_materials: Vec<MaterialRef> = object
                    .extra_render
                    .iter()
                    .map(|r| self.material_ref(r.material, project_root, shaders))
                    .collect();
                SceneEntry {
                    id: object.id.to_string(),
                    name: object.name.clone(),
                    source: object.source.clone(),
                    enabled: object.enabled,
                    static_geometry: object.static_geometry,
                    lightmap_scale: object.lightmap_scale,
                    cast_shadows: object.cast_shadows,
                    layer: object.layer,
                    material,
                    extra_materials,
                    parent: object.parent,
                    components: object
                        .components
                        .iter()
                        .map(|c| citrus_assets::ComponentData::of(c.as_ref()))
                        .collect(),
                    translation: object.translation.to_array(),
                    rotation: object.rotation.to_array(),
                    scale: object.scale.to_array(),
                    model: None,
                    model_part: None,
                }
            })
            .collect();
        SceneFile {
            entries,
            skybox: self.skybox.clone(),
            environment: self.environment.clone(),
            layers: self.layers.clone(),
            editor_camera: None,
            collapsed: Vec::new(),
        }
    }

    /// Extract object `root` and all its descendants as a self-contained
    /// [`Prefab`](crate::prefab::Prefab): entries are re-indexed so parent
    /// references are local to the prefab and the root becomes parentless. Drives
    /// the editor's "Create Prefab from Selection" action (CHECKLIST T0 #7).
    pub fn prefab_from_object(
        &self,
        root: usize,
        project_root: &Path,
        shaders: &ShaderLibrary,
    ) -> Option<crate::prefab::Prefab> {
        if root >= self.objects.len() {
            return None;
        }
        let file = self.to_scene_file(project_root, shaders);
        // Collect root + descendants (parents always precede children in `objects`,
        // but walk defensively in case they don't).
        let mut keep = vec![root];
        let mut i = 0;
        while i < keep.len() {
            let p = keep[i];
            for (idx, e) in file.entries.iter().enumerate() {
                if e.parent == Some(p) && !keep.contains(&idx) {
                    keep.push(idx);
                }
            }
            i += 1;
        }
        // Old index -> new local index.
        let remap: std::collections::HashMap<usize, usize> =
            keep.iter().enumerate().map(|(new, &old)| (old, new)).collect();
        let entries = keep
            .iter()
            .map(|&old| {
                let mut e = file.entries[old].clone();
                e.parent = if old == root {
                    None
                } else {
                    e.parent.and_then(|p| remap.get(&p).copied())
                };
                e
            })
            .collect();
        Some(crate::prefab::Prefab::new(entries))
    }

    /// Resolve one serialized `MaterialRef` to a material index. `File` loads
    /// (or fetches cached) the asset; `Inline` overrides the given template
    /// material's model in place and returns it.
    fn resolve_material_ref(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        project_root: &Path,
        mref: &MaterialRef,
        default_material: usize,
    ) -> usize {
        match mref {
            MaterialRef::Model => default_material,
            MaterialRef::File(path) => {
                let abs = citrus_core::asset_ref::resolve_path(project_root, path);
                self.material_from_file(renderer, shaders, &abs, project_root)
            }
            MaterialRef::Inline {
                params,
                features,
                shader,
                custom,
                render_queue,
                textures,
            } => {
                let entry_ref = &mut self.materials[default_material];
                let has_normal = entry_ref.model.has_normal_texture;
                let name = entry_ref.model.name.clone();
                let mut model = model_from_material(&name, params, features, has_normal);
                model.shader = shader.clone();
                if let Some(q) = render_queue {
                    model.render_queue = *q;
                }
                model.textures = tex_paths_from_file(textures);
                if shader != "standard" {
                    let shader_entry = shaders.resolve(renderer, project_root, shader);
                    if let Some(source) = &shader_entry.source {
                        model.custom_values = source.pack(custom).to_vec();
                    }
                }
                self.materials[default_material].model = model;
                default_material
            }
        }
    }

    /// Load any LOD-level models not uploaded yet (cheap when all are loaded).
    pub fn prepare_lods(&mut self, renderer: &mut Renderer, project_root: &Path) {
        // (template key, model to load, triangle share to keep when generated)
        let mut wanted: Vec<(String, String, Option<f32>)> = Vec::new();
        for o in &self.objects {
            let Some(lod) = o.components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::LodComponent>()) else { continue };
            let base = match &o.source {
                ObjectSource::Model { path, .. } => Some(path.as_str()),
                _ => None,
            };
            for level in &lod.levels {
                let Some(key) = lod_template_key(level, base) else { continue };
                if !self.model_templates.contains_key(&key) && !self.lod_failed.contains(&key) && !wanted.iter().any(|w| w.0 == key) {
                    let generated = level.model.is_empty();
                    let source = if generated { base.unwrap_or_default().to_owned() } else { level.model.clone() };
                    wanted.push((key, source, generated.then_some(level.keep)));
                }
            }
        }
        for (key, source, keep) in wanted {
            let result = citrus_assets::load_model_with_meta(project_root.join(&source)).and_then(|mut asset| {
                if let Some(keep) = keep {
                    for mesh in &mut asset.meshes {
                        *mesh = citrus_assets::simplify::simplify(mesh, keep);
                    }
                    // Blendshapes index the original vertices.
                    asset.morphs.clear();
                }
                self.install_model_template(renderer, &key, &asset, None)
            });
            if let Err(e) = result {
                tracing::warn!("LOD model {key}: {e:#}");
                self.lod_failed.insert(key);
            }
        }
    }

    /// LOD level for object `i` at its world position, with a 5 % distance
    /// band so objects don't flicker at a boundary. `Err(())` = culled.
    fn lod_level(&mut self, i: usize, pos: Vec3) -> Result<Option<(usize, usize)>, ()> {
        let (Some(cam), Some(lod)) = (
            self.lod_camera,
            self.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::LodComponent>()),
        ) else {
            return Ok(None);
        };
        let d = pos.distance(cam);
        let id = self.objects[i].id;
        let near = lod.select_lod(d * 0.95);
        let far = lod.select_lod(d * 1.05);
        let current = self.lod_state.get(&id).copied();
        let level = match current {
            Some(cur) if (cur == near || cur == far) => cur,
            _ => lod.select_lod(d),
        };
        self.lod_state.insert(id, level);
        let Some(level) = level else {
            return Err(());
        };
        let base = match &self.objects[i].source {
            ObjectSource::Model { path, .. } => Some(path.as_str()),
            _ => None,
        };
        let Some(key) = lod_template_key(&lod.levels[level], base) else { return Ok(None) };
        Ok(self.model_templates.get(&key).map(|(base, mats)| (*base, mats.len().max(1))))
    }

    /// Upload the builtin test-scene meshes once and remember their templates.
    fn ensure_builtin_template(&mut self, renderer: &mut Renderer) -> Result<()> {
        if self.builtin_template.is_some() {
            return Ok(());
        }
        let test = citrus_assets::test_scene();
        let mesh_base = self.mesh_handles.len();
        let object_start = self.objects.len();
        self.add_asset_scene(renderer, &test, None)?;
        let template_materials: Vec<usize> = self.objects[object_start..]
            .iter()
            .filter_map(|o| o.render.map(|r| r.material))
            .collect();
        self.objects.truncate(object_start);
        self.draws.clear();
        self.builtin_template = Some((mesh_base, template_materials));
        Ok(())
    }

    /// Upload an imported model (or install its pre-uploaded GPU data) and
    /// record its mesh base + per-mesh template materials. No objects are kept;
    /// scene entries create the placements.
    pub(crate) fn install_model_template(
        &mut self,
        renderer: &mut Renderer,
        path: &str,
        asset: &citrus_assets::Scene,
        prepared: Option<citrus_render::PreparedScene>,
    ) -> Result<()> {
        if self.model_templates.contains_key(path) {
            return Ok(());
        }
        self.model_parts.insert(path.to_owned(), citrus_assets::ModelPart::of(asset));
        let mesh_base = self.mesh_handles.len();
        let object_start = self.objects.len();
        match prepared {
            Some(prep) => {
                let (mh, th, sk) = renderer.install_prepared(prep);
                self.add_installed_asset(renderer, asset, &mh, &th, &sk, Some(Path::new(path)))?;
            }
            None => self.add_asset_scene(renderer, asset, Some(Path::new(path)))?,
        }
        let mut per_mesh_material = vec![0usize; asset.meshes.len()];
        for object in &self.objects[object_start..] {
            for render in object.render_slots() {
                per_mesh_material[render.mesh - mesh_base] = render.material;
            }
        }
        self.objects.truncate(object_start);
        self.draws.clear();
        self.model_templates
            .insert(path.to_owned(), (mesh_base, per_mesh_material));
        Ok(())
    }

    /// Create objects for `entries` (parent indices local to `entries`),
    /// appended after the existing objects. Model / builtin templates must
    /// already be installed. Returns the index of the first new object.
    fn push_entries(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        project_root: &Path,
        registry: &ComponentRegistry,
        entries: &[SceneEntry],
    ) -> Result<usize> {
        self.anim_assets.set_root(project_root);
        let base = self.objects.len();
        for entry in entries {
            // (mesh, template material) for every render slot of this source.
            let slots: Vec<(usize, usize)> = match &entry.source {
                ObjectSource::Model {
                    path,
                    mesh,
                    extra_meshes,
                } => {
                    let (mesh_base, materials) = self
                        .model_templates
                        .get(path)
                        .context("scene references a model that failed to load")?
                        .clone();
                    let resolve = |m: usize| {
                        let local = m.min(materials.len().saturating_sub(1));
                        (mesh_base + local, materials[local])
                    };
                    std::iter::once(resolve(*mesh))
                        .chain(extra_meshes.iter().map(|m| resolve(*m)))
                        .collect()
                }
                ObjectSource::Builtin { mesh } => {
                    let (mesh_base, materials) = self
                        .builtin_template
                        .as_ref()
                        .context("scene references builtin meshes but none loaded")?;
                    let local = (*mesh).min(2);
                    let material = materials.get(local).copied().unwrap_or(0);
                    vec![(mesh_base + local, material)]
                }
                ObjectSource::Primitive { shape } => {
                    let mesh = self.ensure_primitive_mesh(renderer, *shape)?;
                    let material = self.ensure_default_material(renderer)?;
                    vec![(mesh, material)]
                }
                ObjectSource::Empty | ObjectSource::Camera | ObjectSource::Light => Vec::new(),
            };

            let mut render = None;
            let mut extra_render = Vec::new();
            for (k, &(mesh, default_material)) in slots.iter().enumerate() {
                let mref = if k == 0 {
                    Some(&entry.material)
                } else {
                    entry.extra_materials.get(k - 1)
                };
                let material = match mref {
                    Some(m) => self.resolve_material_ref(renderer, shaders, project_root, m, default_material),
                    None => default_material,
                };
                let info = RenderInfo { mesh, material };
                if k == 0 {
                    render = Some(info);
                } else {
                    extra_render.push(info);
                }
            }

            self.objects.push(SceneObject {
                // Use the saved id; generate one for legacy scenes (empty).
                id: entry.id.parse().unwrap_or_else(|_| ObjectId::new()),
                name: entry.name.clone(),
                render,
                extra_render,
                source: entry.source.clone(),
                enabled: entry.enabled,
                static_geometry: entry.static_geometry,
                lightmap_scale: entry.lightmap_scale,
                cast_shadows: entry.cast_shadows,
                layer: entry.layer,
                parent: None,
                translation: Vec3::from(entry.translation),
                rotation: Quat::from_array(entry.rotation),
                scale: Vec3::from(entry.scale),
                components: entry
                    .components
                    .iter()
                    .filter_map(|c| c.load(registry))
                    .collect(),
            });
        }
        for (k, entry) in entries.iter().enumerate() {
            if let Some(parent) = entry.parent
                && parent < entries.len()
                && parent != k
            {
                self.objects[base + k].parent = Some(base + parent);
            }
        }
        Ok(base)
    }

    /// Add a worker-loaded scene to this one (additive streaming): installs its
    /// pre-uploaded models + textures, creates its objects (keeping their ids)
    /// and tags them with `origin` for `unload_origin`. Baked lighting of the
    /// added scene is not applied (it uses probes / realtime lighting).
    pub fn append_loaded(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        project_root: &Path,
        registry: &ComponentRegistry,
        data: crate::scene_loader::LoadedSceneData,
        origin: &str,
    ) -> Result<usize> {
        for (rel, compiled) in data.shaders {
            shaders.preload(renderer, project_root, &rel, compiled);
        }
        if let Some((refs, prep)) = data.material_textures {
            let (_mh, th, _sk) = renderer.install_prepared(prep);
            for ((abs, srgb), handle) in refs.into_iter().zip(th) {
                self.texture_file_cache.entry((abs, srgb)).or_insert(handle);
            }
        }
        let mut prepared = data.prepared.unwrap_or_default();
        for (path, asset) in &data.models {
            self.install_model_template(renderer, path, asset, prepared.remove(path))?;
        }
        let first = self.append_entries(renderer, shaders, project_root, registry, &data.file.entries)?;
        for o in &self.objects[first..] {
            self.object_origin.insert(o.id, origin.to_owned());
        }
        Ok(first)
    }

    /// Remove every object that `append_loaded` brought in from `origin`.
    pub fn unload_origin(&mut self, origin: &str) -> usize {
        let mut removed = 0;
        for i in (0..self.objects.len()).rev() {
            if i < self.objects.len()
                && self.object_origin.get(&self.objects[i].id).is_some_and(|o| o == origin)
            {
                self.remove_object(i);
                removed += 1;
            }
        }
        let alive: std::collections::HashSet<ObjectId> = self.objects.iter().map(|o| o.id).collect();
        self.object_origin.retain(|id, _| alive.contains(id));
        removed
    }

    /// Add `entries` to a live scene (runtime prefab spawn): imports any model
    /// not loaded yet (synchronously), creates the objects, and pushes the
    /// materials they use to the renderer. Returns the first new index.
    pub fn append_entries(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        project_root: &Path,
        registry: &ComponentRegistry,
        entries: &[SceneEntry],
    ) -> Result<usize> {
        for entry in entries {
            match &entry.source {
                ObjectSource::Model { path, .. } if !self.model_templates.contains_key(path) => {
                    let asset = citrus_assets::load_model_with_meta(project_root.join(path))
                        .with_context(|| format!("importing {path}"))?;
                    self.install_model_template(renderer, path, &asset, None)?;
                }
                ObjectSource::Builtin { .. } => self.ensure_builtin_template(renderer)?,
                _ => {}
            }
        }
        let materials_before = self.materials.len();
        let base = self.push_entries(renderer, shaders, project_root, registry, entries)?;
        let mut touched: Vec<usize> = (materials_before..self.materials.len()).collect();
        for (k, entry) in entries.iter().enumerate() {
            if matches!(entry.material, MaterialRef::Inline { .. })
                && let Some(r) = self.objects[base + k].render
            {
                touched.push(r.material);
            }
        }
        touched.sort_unstable();
        touched.dedup();
        for m in touched {
            self.apply_material(renderer, shaders, project_root, m);
        }
        Ok(base)
    }

    /// Rebuild the whole scene from a SceneFile. The renderer's scene
    /// resources must have been reset by the caller.
    /// Parse every model a scene file references (the slow, CPU-only part) so it
    /// can run on a worker thread; the result feeds `load_scene_file_with_models`
    /// on the main thread for the GPU upload. `Scene` is `Send`.
    pub fn parse_scene_models(
        file: &SceneFile,
        project_root: &Path,
    ) -> Result<HashMap<String, citrus_assets::Scene>> {
        let mut models = HashMap::new();
        for path in file.model_paths() {
            let abs = project_root.join(&path);
            let asset = citrus_assets::load_model_with_meta(&abs).with_context(|| format!("importing {path} for scene"))?;
            models.insert(path, asset);
        }
        Ok(models)
    }

    /// Synchronous load: parse models then upload. Kept for callers that don't
    /// background the parse (most scene loads).
    pub fn load_scene_file(
        renderer: &mut Renderer,
        file: &SceneFile,
        project_root: &Path,
        registry: &ComponentRegistry,
        shaders: &mut ShaderLibrary,
    ) -> Result<Self> {
        let models = Self::parse_scene_models(file, project_root)?;
        Self::load_scene_file_with_models(
            renderer,
            file,
            project_root,
            registry,
            shaders,
            &models,
            None,
            None,
            |_| {},
        )
    }

    /// GPU-side load using pre-parsed models (from `parse_scene_models`). When
    /// `prepared` is Some, the model meshes/textures were already uploaded on a
    /// loader thread and are just installed here (no main-thread upload);
    /// otherwise they're uploaded now. `progress` is called per model/material so
    /// the caller can pump the splash/modal during the (main-thread) build.
    #[allow(clippy::too_many_arguments)]
    pub fn load_scene_file_with_models(
        renderer: &mut Renderer,
        file: &SceneFile,
        project_root: &Path,
        registry: &ComponentRegistry,
        shaders: &mut ShaderLibrary,
        models: &HashMap<String, citrus_assets::Scene>,
        mut prepared: Option<HashMap<String, citrus_render::PreparedScene>>,
        material_textures: Option<(Vec<(PathBuf, bool)>, citrus_render::PreparedScene)>,
        mut progress: impl FnMut(&str),
    ) -> Result<Self> {
        let mut expanded = file.clone();
        expanded.expand_models(&|p| models.get(p).map(citrus_assets::ModelPart::of));
        let file = &expanded;
        let mut scene = Self::empty();
        scene.skybox = file.skybox.clone();
        scene.environment = file.environment.clone();
        scene.layers = file.layers.clone().normalized();

        // Material textures decoded + uploaded on the loader thread: install the
        // GPU handles and seed the file cache keyed by (abs path, srgb), so the
        // per-material bind below hits the cache instead of decoding 4K EXR/PNG
        // on the main thread. When None (single-queue GPU), the bind falls back
        // to a synchronous decode.
        if let Some((refs, prep)) = material_textures {
            let (_mh, th, _sk) = renderer.install_prepared(prep);
            for ((abs, srgb), handle) in refs.into_iter().zip(th) {
                scene.texture_file_cache.insert((abs, srgb), handle);
            }
        }

        // Import each referenced model (and the builtin set) once.
        let mut model_object_template: HashMap<String, Vec<usize>> = HashMap::new();
        let mut needs_builtin = false;
        for entry in &file.entries {
            match &entry.source {
                ObjectSource::Model { path, .. } => {
                    model_object_template.entry(path.clone()).or_default();
                }
                ObjectSource::Builtin { .. } => needs_builtin = true,
                ObjectSource::Primitive { .. }
                | ObjectSource::Empty
                | ObjectSource::Camera
                | ObjectSource::Light => {}
            }
        }

        if needs_builtin {
            scene.ensure_builtin_template(renderer)?;
        }
        for path in model_object_template.keys() {
            let asset = models
                .get(path)
                .with_context(|| format!("model {path} was not pre-parsed"))?;
            progress(&format!("Uploading {path}…"));
            let prep = prepared.as_mut().and_then(|m| m.remove(path));
            scene.install_model_template(renderer, path, asset, prep)?;
        }

        scene.push_entries(renderer, shaders, project_root, registry, &file.entries)?;

        // Push all material models (incl. inline overrides) to the renderer
        // (shader compiles happen here, so report per material).
        for i in 0..scene.materials.len() {
            progress(&format!("Compiling materials… ({}/{})", i + 1, scene.materials.len()));
            scene.apply_material(renderer, shaders, project_root, i);
        }
        Ok(scene)
    }
}

/// CPU linear-blend skinning of a vertex list by a joint palette.
fn skin_vertices(base: &[citrus_render::Vertex], palette: &[Mat4]) -> Vec<citrus_render::Vertex> {
    let mut verts = base.to_vec();
    for v in &mut verts {
        v.position = citrus_assets::skin_position(Vec3::from(v.position), v.joints, v.weights, palette).to_array();
        v.normal = citrus_assets::skin_direction(Vec3::from(v.normal), v.joints, v.weights, palette).to_array();
    }
    verts
}

/// The clip carrying most weight in a motion (clip or blend tree) at `time`,
/// with its local time and duration. Drives events and root motion.
fn dominant_clip(
    animator: &citrus_core::Animator,
    animations: &[citrus_assets::AnimationClip],
    find: &dyn Fn(&str) -> Option<usize>,
    name: &str,
    time: f32,
) -> Option<(usize, f32, f32)> {
    if let Some(tree) = animator.blend_trees.iter().find(|t| t.name == name) {
        let p = [animator.get_float(&tree.parameter), animator.get_float(&tree.parameter_y)];
        let w = tree.weights(p);
        let motions: Vec<(usize, f32)> = tree
            .motions
            .iter()
            .zip(&w)
            .filter_map(|(m, &w)| find(&m.clip).map(|c| (c, w)))
            .collect();
        let dur: f32 = motions.iter().map(|&(c, w)| w * animations[c].duration).sum();
        let (c, _) = motions.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1))?;
        let cd = animations[c].duration;
        let local = if dur > 1e-6 { time / dur * cd } else { 0.0 };
        return Some((c, local, cd));
    }
    let c = find(name)?;
    Some((c, time, animations[c].duration))
}

/// Pose of a clip or blend tree at `time` (blend-tree clips phase-synced).
fn sample_motion(
    animator: &citrus_core::Animator,
    skel: &citrus_assets::Skeleton,
    animations: &[citrus_assets::AnimationClip],
    find: &dyn Fn(&str) -> Option<usize>,
    name: &str,
    time: f32,
) -> Option<(Vec<Mat4>, f32)> {
    let looping = animator.looping;
    let sample = |clip: usize, t: f32| {
        let c = &animations[clip];
        let t = if looping { t } else { t.clamp(0.0, (c.duration - 1e-4).max(0.0)) };
        c.sample(skel, t)
    };
    if let Some(tree) = animator.blend_trees.iter().find(|t| t.name == name) {
        let p = [animator.get_float(&tree.parameter), animator.get_float(&tree.parameter_y)];
        let w = tree.weights(p);
        let motions: Vec<(usize, f32)> = tree
            .motions
            .iter()
            .zip(&w)
            .filter(|(_, w)| **w > 1e-4)
            .filter_map(|(m, &w)| find(&m.clip).map(|c| (c, w)))
            .collect();
        if motions.is_empty() {
            return None;
        }
        let dur: f32 = motions.iter().map(|&(c, w)| w * animations[c].duration).sum();
        let phase = if dur > 1e-6 { time / dur } else { 0.0 };
        let poses: Vec<(Vec<Mat4>, f32)> = motions
            .iter()
            .map(|&(c, w)| (sample(c, phase * animations[c].duration), w))
            .collect();
        return Some((blend_many(&poses), dur));
    }
    let c = find(name)?;
    Some((sample(c, time), animations[c].duration))
}

/// Advance an Animator by `dt`: returns the blended joint locals and the
/// root-motion delta (skeleton space, horizontal). Fires events into
/// `state.fired`. `clips` index `animations` for this skeleton.
fn advance_animator(
    animator: &mut citrus_core::Animator,
    skel: &citrus_assets::Skeleton,
    animations: &[citrus_assets::AnimationClip],
    clips: &[usize],
    dt: f32,
) -> (Vec<Mat4>, Vec3) {
    let find = |name: &str| clips.iter().copied().find(|&c| animations[c].name == name);
    animator.state.fired.clear();
    if animator.state.clips.len() != clips.len() {
        animator.state.clips = clips.iter().map(|&c| animations[c].name.clone()).collect();
    }
    if !animator.state.started {
        animator.state.started = true;
        if animator.play_on_start {
            let name = if animator.clip.is_empty() {
                clips.first().map(|&c| animations[c].name.clone())
            } else {
                Some(animator.clip.clone())
            };
            animator.state.current = name;
            animator.state.playing = animator.state.current.is_some();
        }
    }
    let speed = animator.speed;
    let prev_time = animator.state.time;
    if animator.state.playing {
        let s = &mut animator.state;
        s.time += dt * speed;
        s.fade_elapsed += dt;
        if let Some((_, t)) = s.previous.as_mut() {
            *t += dt * speed;
        }
    }
    let Some(current) = animator.state.current.clone() else {
        animator.state.duration = 0.0;
        return (skel.rest_locals(), Vec3::ZERO);
    };
    let Some((mut pose, duration)) = sample_motion(animator, skel, animations, &find, &current, animator.state.time) else {
        animator.state.duration = 0.0;
        return (skel.rest_locals(), Vec3::ZERO);
    };
    animator.state.duration = duration;
    if !animator.looping && duration > 0.0 {
        animator.state.time = animator.state.time.min(duration);
    }

    // Events + root motion follow the dominant clip.
    let mut root_delta = Vec3::ZERO;
    if let (Some((c, a, cd)), Some((_, b, _))) = (
        dominant_clip(animator, animations, &find, &current, prev_time),
        dominant_clip(animator, animations, &find, &current, animator.state.time),
    ) && cd > 1e-6
    {
        let (a, b) = if animator.looping { (a, b) } else { (a.clamp(0.0, cd), b.clamp(0.0, cd)) };
        let clip_name = &animations[c].name;
        if b > a {
            for e in animator.events.iter().filter(|e| &e.clip == clip_name) {
                let first = (a / cd).floor() as i64;
                let last = (b / cd).floor() as i64;
                for k in first..=last {
                    let t = k as f32 * cd + e.time;
                    if t > a && t <= b {
                        animator.state.fired.push(e.name.clone());
                    }
                }
            }
        }
        if animator.root_motion
            && let Some(root) = skel.joints.iter().position(|j| j.parent.is_none())
        {
            let clip = &animations[c];
            let root_at = |t: f32| clip.sample(skel, t)[root].w_axis.truncate();
            let wrap = |t: f32| t.rem_euclid(cd);
            let start = root_at(0.0);
            let mut d = if animator.looping && (b / cd).floor() > (a / cd).floor() {
                (root_at(cd - 1e-4) - root_at(wrap(a))) + (root_at(wrap(b)) - start)
            } else {
                root_at(if animator.looping { wrap(b) } else { b }) - root_at(if animator.looping { wrap(a) } else { a })
            };
            d.y = 0.0;
            root_delta = d;
            // Keep the skeleton in place horizontally; the object moves instead.
            let t = pose[root].w_axis;
            pose[root].w_axis = glam::Vec4::new(start.x, t.y, start.z, t.w);
        }
    }

    let s = &mut animator.state;
    let weight = if s.fade_duration > 0.0 { (s.fade_elapsed / s.fade_duration).min(1.0) } else { 1.0 };
    if weight >= 1.0 {
        animator.state.previous = None;
    }
    let previous = animator.state.previous.clone();
    let pose = match previous.and_then(|(name, t)| sample_motion(animator, skel, animations, &find, &name, t)) {
        Some((prev, _)) => blend_locals(&prev, &pose, weight),
        None => pose,
    };
    (pose, root_delta)
}

/// Weighted per-joint TRS average of several poses (weights sum to ~1).
fn blend_many(poses: &[(Vec<Mat4>, f32)]) -> Vec<Mat4> {
    let Some((first, _)) = poses.first() else {
        return Vec::new();
    };
    let total: f32 = poses.iter().map(|(_, w)| *w).sum::<f32>().max(1e-6);
    (0..first.len())
        .map(|j| {
            let (mut s, mut t) = (Vec3::ZERO, Vec3::ZERO);
            let mut q = glam::Vec4::ZERO;
            let reference = first[j].to_scale_rotation_translation().1;
            for (pose, w) in poses {
                let (ps, pr, pt) = pose[j].to_scale_rotation_translation();
                let w = w / total;
                s += ps * w;
                t += pt * w;
                // Same hemisphere so the nlerp doesn't take the long way round.
                let pr = if pr.dot(reference) < 0.0 { -pr } else { pr };
                q += glam::Vec4::from(pr) * w;
            }
            let r = Quat::from_vec4(q).normalize();
            Mat4::from_scale_rotation_translation(s, r, t)
        })
        .collect()
}

/// Per-joint TRS blend of two poses (`t` = 0 -> `a`, 1 -> `b`).
fn blend_locals(a: &[Mat4], b: &[Mat4], t: f32) -> Vec<Mat4> {
    a.iter()
        .zip(b)
        .map(|(ma, mb)| {
            let (sa, ra, ta) = ma.to_scale_rotation_translation();
            let (sb, rb, tb) = mb.to_scale_rotation_translation();
            Mat4::from_scale_rotation_translation(sa.lerp(sb, t), ra.slerp(rb, t), ta.lerp(tb, t))
        })
        .collect()
}

pub fn model_from_material(
    name: &str,
    p: &MaterialParams,
    f: &MaterialFeatures,
    has_normal_texture: bool,
) -> MaterialModel {
    MaterialModel {
        name: name.to_owned(),
        shader: "standard".into(),
        custom_values: Vec::new(),
        base_color: p.base_color,
        metallic: p.metallic,
        roughness: p.roughness,
        occlusion_strength: p.occlusion_strength,
        toon_enabled: f.toon,
        toon_steps: p.toon_steps,
        pbr_toon_blend: p.pbr_toon_blend,
        emission_enabled: f.emission,
        emission_color: p.emission_color,
        emission_intensity: p.emission_intensity,
        alpha_mode: match f.alpha_mode {
            AlphaMode::Opaque => AlphaModeModel::Opaque,
            AlphaMode::Cutout => AlphaModeModel::Cutout,
            AlphaMode::Blend => AlphaModeModel::Blend,
        },
        alpha_cutoff: p.alpha_cutoff,
        has_normal_texture,
        normal_map_enabled: f.normal_map,
        normal_strength: p.normal_strength,
        double_sided: f.double_sided,
        rim_color: p.rim_color,
        rim_power: p.rim_power,
        rim_strength: p.rim_strength,
        ramp_smoothness: p.ramp_smoothness,
        emission_scroll: p.emission_scroll,
        emission_pulse: p.emission_pulse,
        albedo_tiling: p.albedo_tiling,
        albedo_offset: p.albedo_offset,
        normal_tiling: p.normal_tiling,
        normal_offset: p.normal_offset,
        orm_tiling: p.orm_tiling,
        orm_offset: p.orm_offset,
        emission_tiling: p.emission_tiling,
        emission_offset: p.emission_offset,
        ao_invert: p.ao_invert,
        roughness_invert: p.roughness_invert,
        metallic_invert: p.metallic_invert,
        displacement_scale: p.displacement_scale,
        reflection_intensity: p.reflection_intensity,
        screen_reflections: p.screen_reflections,
        matcap_strength: p.matcap_strength,
        matcap_blend: [
            citrus_core::MatcapBlend::from_f32(p.matcap_blend[0]),
            citrus_core::MatcapBlend::from_f32(p.matcap_blend[1]),
            citrus_core::MatcapBlend::from_f32(p.matcap_blend[2]),
        ],
        render_queue: (match f.alpha_mode {
            AlphaMode::Opaque => AlphaModeModel::Opaque,
            AlphaMode::Cutout => AlphaModeModel::Cutout,
            AlphaMode::Blend => AlphaModeModel::Blend,
        })
        .default_render_queue(),
        textures: citrus_core::MaterialTexturePaths::default(),
    }
}

/// Convert an asset-file texture set into the editable model paths.
/// The (relative path, srgb) for the 16 material texture slots, in slot order.
/// Shared by the main-thread bind (`resolve_texture_slots`) and the worker-side
/// pre-decode (`collect_material_texture_refs`) so both agree on colour space.
/// Colour slots (albedo/emission/matcaps) are sRGB; data slots are linear.
fn texture_slot_order(
    paths: &citrus_core::MaterialTexturePaths,
) -> [(Option<PathBuf>, bool); 16] {
    [
        (paths.albedo.clone(), true),
        (paths.normal.clone(), false),
        (paths.orm.clone(), false),
        (paths.emission.clone(), true),
        (paths.opacity.clone(), false),
        (paths.emission_mask.clone(), false),
        (paths.matcap[0].clone(), true),
        (paths.matcap_mask[0].clone(), false),
        (paths.matcap[1].clone(), true),
        (paths.matcap_mask[1].clone(), false),
        (paths.matcap[2].clone(), true),
        (paths.matcap_mask[2].clone(), false),
        (paths.ao.clone(), false),
        (paths.roughness.clone(), false),
        (paths.metallic.clone(), false),
        (paths.displacement.clone(), false),
    ]
}

/// The camera object id in a `camera:<object id>` texture path.
fn camera_texture_id(path: &Path) -> Option<ObjectId> {
    path.to_str()?.strip_prefix("camera:")?.trim().parse().ok()
}

/// Convention: a custom shader with an `OUTLINE` toggle plus `outline_color`
/// and `outline_width` properties gets the engine's inverted-hull outline.
fn custom_outline(source: &citrus_assets::ShaderSource, values: &[f32]) -> Option<([f32; 4], f32)> {
    let on = source.prop("OUTLINE")?;
    let color = source.prop("outline_color")?;
    let width = source.prop("outline_width")?;
    let v = |at: usize| values.get(at).copied().unwrap_or(0.0);
    if v(on.offset) == 0.0 || v(width.offset) <= 0.0 {
        return None;
    }
    let c = color.offset;
    let alpha = if color.kind.size() == 4 { v(c + 3) } else { 1.0 };
    Some(([v(c), v(c + 1), v(c + 2), alpha], v(width.offset)))
}

pub fn tex_paths_from_file(
    t: &citrus_assets::MaterialTextures,
) -> citrus_core::MaterialTexturePaths {
    citrus_core::MaterialTexturePaths {
        albedo: t.albedo.clone(),
        normal: t.normal.clone(),
        orm: t.orm.clone(),
        emission: t.emission.clone(),
        opacity: t.opacity.clone(),
        emission_mask: t.emission_mask.clone(),
        matcap: t.matcap.clone(),
        matcap_mask: t.matcap_mask.clone(),
        ao: t.ao.clone(),
        roughness: t.roughness.clone(),
        metallic: t.metallic.clone(),
        displacement: t.displacement.clone(),
        shader: t.shader.iter().map(|(n, p)| (n.clone(), Some(p.clone()))).collect(),
    }
}

/// Convert the editable model paths back into an asset-file texture set.
pub fn tex_file_from_paths(
    t: &citrus_core::MaterialTexturePaths,
) -> citrus_assets::MaterialTextures {
    citrus_assets::MaterialTextures {
        albedo: t.albedo.clone(),
        normal: t.normal.clone(),
        orm: t.orm.clone(),
        emission: t.emission.clone(),
        opacity: t.opacity.clone(),
        emission_mask: t.emission_mask.clone(),
        matcap: t.matcap.clone(),
        matcap_mask: t.matcap_mask.clone(),
        ao: t.ao.clone(),
        roughness: t.roughness.clone(),
        metallic: t.metallic.clone(),
        displacement: t.displacement.clone(),
        shader: t
            .shader
            .iter()
            .filter_map(|(n, p)| p.clone().map(|p| (n.clone(), p)))
            .collect(),
    }
}

pub fn material_from_model(m: &MaterialModel) -> (MaterialParams, MaterialFeatures) {
    (
        MaterialParams {
            base_color: m.base_color,
            metallic: m.metallic,
            roughness: m.roughness,
            occlusion_strength: m.occlusion_strength,
            toon_steps: m.toon_steps,
            pbr_toon_blend: m.pbr_toon_blend,
            emission_color: m.emission_color,
            emission_intensity: m.emission_intensity,
            // Derived at apply_material from the actual map; 1 = no scaling here.
            emission_map_mean: [1.0; 3],
            alpha_cutoff: m.alpha_cutoff,
            normal_strength: m.normal_strength,
            rim_color: m.rim_color,
            rim_power: m.rim_power,
            rim_strength: m.rim_strength,
            ramp_smoothness: m.ramp_smoothness,
            base_scroll: [0.0, 0.0],
            emission_scroll: m.emission_scroll,
            emission_pulse: m.emission_pulse,
            albedo_tiling: m.albedo_tiling,
            albedo_offset: m.albedo_offset,
            normal_tiling: m.normal_tiling,
            normal_offset: m.normal_offset,
            orm_tiling: m.orm_tiling,
            orm_offset: m.orm_offset,
            emission_tiling: m.emission_tiling,
            emission_offset: m.emission_offset,
            // Invert only has meaning with the matching split map assigned;
            // gate it so an invert toggle left on with no map can't zero a
            // channel (1 - default-white = 0).
            ao_invert: m.ao_invert && m.textures.ao.is_some(),
            roughness_invert: m.roughness_invert && m.textures.roughness.is_some(),
            metallic_invert: m.metallic_invert && m.textures.metallic.is_some(),
            displacement_scale: if m.textures.displacement.is_some() {
                m.displacement_scale
            } else {
                0.0
            },
            reflection_intensity: m.reflection_intensity,
            screen_reflections: m.screen_reflections,
            matcap_strength: m.matcap_strength,
            matcap_blend: [
                m.matcap_blend[0].to_f32(),
                m.matcap_blend[1].to_f32(),
                m.matcap_blend[2].to_f32(),
            ],
        },
        features_from_model(m),
    )
}

pub fn features_from_model(m: &MaterialModel) -> MaterialFeatures {
    MaterialFeatures {
        toon: m.toon_enabled,
        normal_map: m.normal_map_enabled,
        emission: m.emission_enabled,
        alpha_mode: match m.alpha_mode {
            AlphaModeModel::Opaque => AlphaMode::Opaque,
            AlphaModeModel::Cutout => AlphaMode::Cutout,
            AlphaModeModel::Blend => AlphaMode::Blend,
        },
        double_sided: m.double_sided,
        water: false,
    }
}

pub fn relative_to(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Strip a trailing `(N)` (N = digits) from a duplicate name so re-duplicating
/// `file(2)` yields `file(3)`, not `file(2)(1)`. Returns the base, trimmed.
fn strip_dup_suffix(name: &str) -> &str {
    let mut name = name.trim_end();
    loop {
        if let Some(rest) = name.strip_suffix(" Copy") {
            name = rest.trim_end();
            continue;
        }
        if let Some(open) = name.rfind('(')
            && name.ends_with(')')
        {
            let inner = &name[open + 1..name.len() - 1];
            if !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_digit()) {
                name = name[..open].trim_end();
                continue;
            }
        }
        return name;
    }
}

/// Shader's per-fragment probe-volume cap (`MAX_PROBE_VOLUMES` in standard.frag).
/// The Per-object auto grid clusters down to this many tight volumes.
const MAX_AUTO_VOLUMES: usize = 4;

/// Greedily merge AABBs (by smallest merged volume) until at most `max` remain, so
/// the Per-object grid stays within the shader's volume cap. O(n³) but n = object
/// count, small.
fn cluster_aabbs(boxes: &mut Vec<(Vec3, Vec3)>, max: usize) {
    while boxes.len() > max {
        let (mut bi, mut bj, mut best) = (0usize, 1usize, f32::INFINITY);
        for i in 0..boxes.len() {
            for j in (i + 1)..boxes.len() {
                let lo = boxes[i].0.min(boxes[j].0);
                let hi = boxes[i].1.max(boxes[j].1);
                let d = (hi - lo).max(Vec3::ZERO);
                let vol = d.x * d.y * d.z;
                if vol < best {
                    best = vol;
                    bi = i;
                    bj = j;
                }
            }
        }
        let lo = boxes[bi].0.min(boxes[bj].0);
        let hi = boxes[bi].1.max(boxes[bj].1);
        boxes.swap_remove(bj); // bj > bi, so this doesn't disturb index bi
        boxes[bi] = (lo, hi);
    }
}

fn ray_aabb(origin: Vec3, dir: Vec3, min: Vec3, max: Vec3) -> Option<f32> {
    let inv_dir = dir.recip();
    let t1 = (min - origin) * inv_dir;
    let t2 = (max - origin) * inv_dir;
    let t_min = t1.min(t2).max_element();
    let t_max = t1.max(t2).min_element();
    (t_max >= t_min.max(0.0)).then_some(t_min.max(0.0))
}

/// Nearest ray-vs-triangle hit distance across an indexed mesh (positions are
/// object-local; the caller transforms the ray into the same space). Used to pick
/// the actual SURFACE, not the loose AABB.
fn ray_mesh(origin: Vec3, dir: Vec3, pos: &[Vec3], idx: &[u32]) -> Option<f32> {
    let mut best = f32::INFINITY;
    for tri in idx.as_chunks::<3>().0 {
        let (Some(&a), Some(&b), Some(&c)) = (
            pos.get(tri[0] as usize),
            pos.get(tri[1] as usize),
            pos.get(tri[2] as usize),
        ) else {
            continue;
        };
        if let Some(t) = ray_triangle(origin, dir, a, b, c)
            && t < best {
                best = t;
            }
    }
    best.is_finite().then_some(best)
}

/// Möller–Trumbore ray-triangle intersection (two-sided). Returns the forward
/// distance `t` along `dir`, or None.
fn ray_triangle(origin: Vec3, dir: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let e1 = b - a;
    let e2 = c - a;
    let p = dir.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-7 {
        return None; // ray parallel to the triangle
    }
    let inv_det = 1.0 / det;
    let tv = origin - a;
    let u = tv.dot(p) * inv_det;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = tv.cross(e1);
    let v = dir.dot(q) * inv_det;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) * inv_det;
    (t > 1e-5).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_texture_paths_name_a_camera_and_never_the_main_view() {
        let id = ObjectId::new();
        assert_eq!(camera_texture_id(Path::new(&format!("camera:{id}"))), Some(id));
        assert_eq!(camera_texture_id(Path::new("textures/tv.png")), None);
        let mut scene = LoadedScene::empty();
        for (cam_id, rtt) in [(1u32, true), (2, false)] {
            scene.objects.push(SceneObject {
                id: ObjectId::new(),
                name: String::new(),
                render: None,
                extra_render: Vec::new(),
                source: citrus_assets::ObjectSource::Camera,
                enabled: true,
                static_geometry: false,
                lightmap_scale: 1.0,
                cast_shadows: citrus_core::ShadowCasting::On,
                layer: 0,
                parent: None,
                translation: Vec3::ZERO,
                rotation: glam::Quat::IDENTITY,
                scale: Vec3::ONE,
                components: vec![Box::new(citrus_core::CameraComponent { id: cam_id, render_texture: rtt, ..Default::default() })],
            });
        }
        assert_eq!(scene.main_camera(), Some(1), "the render-texture camera (lower id) is skipped");
    }

    #[test]
    fn posed_bounds_follow_the_joints() {
        let vert = |p: [f32; 3], j: u32| citrus_render::Vertex {
            position: p,
            joints: [j, 0, 0, 0],
            weights: [1.0, 0.0, 0.0, 0.0],
            ..Default::default()
        };
        // An "arm": two vertices on joint 0 at the origin, two on joint 1 at x = 1.
        let sm = SkinnedMesh::new(
            0,
            vec![vert([0.0, 0.0, 0.0], 0), vert([0.0, 1.0, 0.0], 0), vert([1.0, 0.0, 0.0], 1), vert([1.0, 1.0, 0.0], 1)],
            0,
        );
        let rest = posed_bounds(&sm.joint_boxes, &[Mat4::IDENTITY, Mat4::IDENTITY]).unwrap();
        assert_eq!(rest, (Vec3::ZERO, Vec3::new(1.0, 1.0, 0.0)));
        // Joint 1 swings 5 m out: the bind-pose box would miss it entirely.
        let posed = posed_bounds(&sm.joint_boxes, &[Mat4::IDENTITY, Mat4::from_translation(Vec3::X * 5.0)]).unwrap();
        assert_eq!(posed, (Vec3::ZERO, Vec3::new(6.0, 1.0, 0.0)));
    }

    #[test]
    fn cluster_aabbs_merges_down_to_cap() {
        // Six well-separated unit boxes; merging to a cap of 4 must leave exactly 4
        // (and the two nearest get merged first, so the count drops by exactly 2).
        let mut boxes: Vec<(Vec3, Vec3)> = (0..6)
            .map(|i| {
                let c = Vec3::new(i as f32 * 10.0, 0.0, 0.0);
                (c - Vec3::splat(0.5), c + Vec3::splat(0.5))
            })
            .collect();
        cluster_aabbs(&mut boxes, 4);
        assert_eq!(boxes.len(), 4);
        // Every original box must still be covered by some cluster (merging only
        // grows AABBs, never drops geometry).
        for i in 0..6 {
            let c = Vec3::new(i as f32 * 10.0, 0.0, 0.0);
            assert!(
                boxes.iter().any(|(lo, hi)| c.cmpge(*lo).all() && c.cmple(*hi).all()),
                "object {i} center fell outside every cluster"
            );
        }
    }

    #[test]
    fn strip_dup_suffix_handles_numbered_and_plain() {
        assert_eq!(strip_dup_suffix("file"), "file");
        assert_eq!(strip_dup_suffix("file(1)"), "file");
        assert_eq!(strip_dup_suffix("file(42)"), "file");
        assert_eq!(strip_dup_suffix("l1(2)"), "l1");
        // Non-numeric parens are part of the name, not a dup suffix.
        assert_eq!(strip_dup_suffix("file(abc)"), "file(abc)");
        assert_eq!(strip_dup_suffix("file()"), "file()");
        // A space before the suffix is trimmed.
        assert_eq!(strip_dup_suffix("file (3)"), "file");
        // Legacy "Copy" chains collapse to the base name.
        assert_eq!(strip_dup_suffix("Spot Light Copy Copy Copy"), "Spot Light");
        assert_eq!(strip_dup_suffix("Spot Light Copy (2)"), "Spot Light");
    }

    #[test]
    fn cluster_aabbs_noop_under_cap() {
        let mut boxes = vec![
            (Vec3::ZERO, Vec3::ONE),
            (Vec3::splat(5.0), Vec3::splat(6.0)),
        ];
        cluster_aabbs(&mut boxes, 4);
        assert_eq!(boxes.len(), 2);
    }
}

#[cfg(test)]
mod animator_tests {
    use super::*;
    use citrus_assets::{AnimChannel, AnimationClip, ChannelPath, Joint, Skeleton};
    use citrus_core::Animator;

    fn skeleton() -> Skeleton {
        Skeleton {
            joints: vec![Joint {
                name: "root".into(),
                parent: None,
                inverse_bind: Mat4::IDENTITY,
                rest_translation: Vec3::ZERO,
                rest_rotation: Quat::IDENTITY,
                rest_scale: Vec3::ONE,
            }],
        }
    }

    /// Moves the root from 0 to `to` over one second.
    fn clip(name: &str, to: Vec3) -> AnimationClip {
        AnimationClip {
            name: name.into(),
            duration: 1.0,
            channels: vec![AnimChannel {
                joint: 0,
                path: ChannelPath::Translation,
                times: vec![0.0, 1.0],
                vec_values: vec![Vec3::ZERO, to],
                quat_values: vec![],
            }],
        }
    }

    fn root_pos(locals: &[Mat4]) -> Vec3 {
        locals[0].w_axis.truncate()
    }

    fn advance_animator_pose(
        a: &mut Animator,
        skel: &Skeleton,
        clips: &[AnimationClip],
        idx: &[usize],
        dt: f32,
    ) -> Vec<Mat4> {
        advance_animator(a, skel, clips, idx, dt).0
    }

    #[test]
    fn blend_tree_mixes_clips_by_parameter() {
        let skel = skeleton();
        let clips = vec![clip("Walk", Vec3::X), clip("Run", Vec3::Y)];
        let mut a = Animator {
            clip: "Move".into(),
            blend_trees: vec![citrus_core::BlendTree {
                name: "Move".into(),
                parameter: "Speed".into(),
                parameter_y: String::new(),
                motions: vec![
                    citrus_core::BlendMotion { clip: "Walk".into(), position: [1.0, 0.0] },
                    citrus_core::BlendMotion { clip: "Run".into(), position: [3.0, 0.0] },
                ],
            }],
            ..Default::default()
        };
        a.set_float("Speed", 2.0);
        let p = root_pos(&advance_animator_pose(&mut a, &skel, &clips, &[0, 1], 0.5));
        assert!((p - Vec3::new(0.25, 0.25, 0.0)).length() < 1e-3, "{p}");
        a.set_float("Speed", 3.0);
        let p = root_pos(&advance_animator_pose(&mut a, &skel, &clips, &[0, 1], 0.0));
        assert!((p - Vec3::new(0.0, 0.5, 0.0)).length() < 1e-3, "{p}");
    }

    #[test]
    fn events_fire_once_per_pass_including_loop_wrap() {
        let skel = skeleton();
        let clips = vec![clip("Walk", Vec3::X)];
        let mut a = Animator {
            events: vec![citrus_core::AnimEvent { clip: "Walk".into(), time: 0.25, name: "Step".into() }],
            ..Default::default()
        };
        advance_animator(&mut a, &skel, &clips, &[0], 0.2);
        assert!(a.state.fired.is_empty());
        advance_animator(&mut a, &skel, &clips, &[0], 0.1);
        assert_eq!(a.state.fired, vec!["Step".to_string()]);
        advance_animator(&mut a, &skel, &clips, &[0], 0.5);
        assert!(a.state.fired.is_empty());
        // 0.8 -> 1.3 wraps past 1.25.
        advance_animator(&mut a, &skel, &clips, &[0], 0.5);
        assert_eq!(a.state.fired, vec!["Step".to_string()]);
    }

    #[test]
    fn root_motion_moves_object_not_skeleton() {
        let skel = skeleton();
        let clips = vec![clip("Walk", Vec3::new(0.0, 0.0, 2.0))];
        let mut a = Animator {
            root_motion: true,
            ..Default::default()
        };
        let (pose, d) = advance_animator(&mut a, &skel, &clips, &[0], 0.25);
        assert!((d - Vec3::new(0.0, 0.0, 0.5)).length() < 1e-3, "{d}");
        assert!(root_pos(&pose).z.abs() < 1e-4, "root pinned in place");
        // Across the loop: 0.25 -> 1.25 is one full cycle = 2 m.
        let (_, d) = advance_animator(&mut a, &skel, &clips, &[0], 1.0);
        assert!((d.z - 2.0).abs() < 1e-2, "{d}");
    }

    #[test]
    fn plays_first_clip_and_cross_fades() {
        let skel = skeleton();
        let clips = vec![clip("Walk", Vec3::X), clip("Jump", Vec3::Y)];
        let idx = [0, 1];
        let mut a = Animator::default();
        let p = root_pos(&advance_animator_pose(&mut a, &skel, &clips, &idx, 0.5));
        assert_eq!(a.state.current.as_deref(), Some("Walk"));
        assert_eq!(a.state.clips, vec!["Walk".to_string(), "Jump".to_string()]);
        assert!((p - Vec3::new(0.5, 0.0, 0.0)).length() < 1e-4);

        a.play("Jump", 1.0);
        // Halfway through the fade: Walk at t=1.0 (wraps to 0) blends with Jump at t=0.5.
        let p = root_pos(&advance_animator_pose(&mut a, &skel, &clips, &idx, 0.5));
        assert!((p.y - 0.25).abs() < 1e-3, "{p}");
        let p = root_pos(&advance_animator_pose(&mut a, &skel, &clips, &idx, 0.6));
        assert!(a.state.previous.is_none(), "fade finished");
        assert!((p.x).abs() < 1e-4 && p.y > 0.0);
    }

    #[test]
    fn non_looping_clip_holds_last_frame_and_finishes() {
        let skel = skeleton();
        let clips = vec![clip("Die", Vec3::X)];
        let mut a = Animator {
            looping: false,
            ..Default::default()
        };
        advance_animator_pose(&mut a, &skel, &clips, &[0], 0.4);
        assert!(!a.finished());
        let p = root_pos(&advance_animator_pose(&mut a, &skel, &clips, &[0], 2.0));
        assert!(a.finished());
        assert!((p.x - 1.0).abs() < 1e-2, "{p}");
    }

    #[test]
    fn no_play_on_start_stays_in_rest_pose() {
        let skel = skeleton();
        let clips = vec![clip("Walk", Vec3::X)];
        let mut a = Animator {
            play_on_start: false,
            ..Default::default()
        };
        let p = root_pos(&advance_animator_pose(&mut a, &skel, &clips, &[0], 0.5));
        assert_eq!(p, Vec3::ZERO);
        a.play("Walk", 0.0);
        let p = root_pos(&advance_animator_pose(&mut a, &skel, &clips, &[0], 0.25));
        assert!((p.x - 0.25).abs() < 1e-4);
    }
}

#[cfg(test)]
mod lod_tests {
    use super::*;
    use citrus_core::{LodComponent, LodLevel};

    fn scene_with_lod() -> LoadedScene {
        let mut scene = LoadedScene::empty();
        scene.objects.push(SceneObject {
            id: ObjectId::new(),
            name: String::new(),
            render: None,
            extra_render: Vec::new(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: citrus_core::ShadowCasting::On,
            layer: 0,
            parent: None,
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components: vec![Box::new(LodComponent {
                levels: vec![
                    LodLevel { keep: 0.0, max_distance: 10.0, model: String::new() },
                    LodLevel { keep: 0.0, max_distance: 20.0, model: String::new() },
                ],
                cull_distance: 30.0,
            })],
        });
        scene
    }

    fn level_at(scene: &mut LoadedScene, d: f32) -> Option<Option<usize>> {
        scene.lod_camera = Some(Vec3::new(d, 0.0, 0.0));
        scene.lod_level(0, Vec3::ZERO).ok()?;
        Some(scene.lod_state[&scene.objects[0].id])
    }

    #[test]
    fn lod_switches_with_hysteresis_and_culls() {
        let mut s = scene_with_lod();
        assert_eq!(level_at(&mut s, 5.0), Some(Some(0)));
        // Just past the boundary stays on level 0 (inside the 5% band).
        assert_eq!(level_at(&mut s, 10.3), Some(Some(0)));
        assert_eq!(level_at(&mut s, 11.0), Some(Some(1)));
        // Coming back, level 1 holds until clearly inside level 0's range.
        assert_eq!(level_at(&mut s, 9.8), Some(Some(1)));
        assert_eq!(level_at(&mut s, 9.0), Some(Some(0)));
        // Past the last level keeps the coarsest; past cull_distance culls.
        assert_eq!(level_at(&mut s, 25.0), Some(Some(1)));
        assert_eq!(level_at(&mut s, 40.0), None);
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;

    fn obj(scene: &mut LoadedScene, name: &str, parent: Option<usize>, t: Vec3) -> usize {
        scene.objects.push(SceneObject {
            id: ObjectId::new(),
            name: name.into(),
            render: None,
            extra_render: Vec::new(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: citrus_core::ShadowCasting::On,
            layer: 0,
            parent,
            translation: t,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components: Vec::new(),
        });
        scene.objects.len() - 1
    }

    #[test]
    fn grouping_keeps_world_positions_and_the_shared_parent() {
        let mut s = LoadedScene::empty();
        let room = obj(&mut s, "Room", None, Vec3::new(10.0, 0.0, 0.0));
        let a = obj(&mut s, "A", Some(room), Vec3::new(1.0, 0.0, 0.0));
        let b = obj(&mut s, "B", Some(room), Vec3::new(3.0, 0.0, 0.0));
        let a_child = obj(&mut s, "A child", Some(a), Vec3::new(0.0, 1.0, 0.0));
        let ids: Vec<ObjectId> = [a, b, a_child].iter().map(|&i| s.objects[i].id).collect();
        let world_before: Vec<Vec3> = [a, b, a_child].iter().map(|&i| s.world_transform(i).w_axis.truncate()).collect();
        let g = s.group_into_empty(&[a, b, a_child]).unwrap();
        assert_eq!(s.objects[g].name, "Group");
        assert_eq!(s.objects[g].parent, s.index_of(s.objects[room].id), "under the shared parent");
        assert!((s.world_transform(g).w_axis.truncate() - Vec3::new(12.0, 0.0, 0.0)).length() < 1e-4, "at their centre");
        for (id, before) in ids.iter().zip(world_before) {
            let i = s.index_of(*id).unwrap();
            assert!((s.world_transform(i).w_axis.truncate() - before).length() < 1e-4, "world position kept");
        }
        let a_now = s.index_of(ids[0]).unwrap();
        assert_eq!(s.objects[a_now].parent, Some(g));
        assert_eq!(s.objects[s.index_of(ids[2]).unwrap()].parent, Some(a_now), "a member's own child stays with it");
        for (i, o) in s.objects.iter().enumerate() {
            assert!(o.parent.is_none_or(|p| p < i), "parents stay ahead of children");
        }
    }
    #[test]
    fn root_objects_and_mixed_parents_all_move_under_the_new_group() {
        let mut s = LoadedScene::empty();
        let lights: Vec<usize> = (0..5).map(|k| obj(&mut s, "light", None, Vec3::new(k as f32, 2.0, 0.0))).collect();
        let room = obj(&mut s, "Room", None, Vec3::new(0.0, 0.0, 5.0));
        let lamp = obj(&mut s, "Lamp", Some(room), Vec3::new(1.0, 0.0, 0.0));
        let mut members = lights.clone();
        members.push(lamp);
        let ids: Vec<ObjectId> = members.iter().map(|&i| s.objects[i].id).collect();
        let g = s.group_into_empty(&members).unwrap();
        assert_eq!(s.objects[g].parent, None, "different parents: the group goes at the root");
        for id in &ids {
            assert_eq!(s.objects[s.index_of(*id).unwrap()].parent, Some(g), "every selected object is under the group");
        }
        assert_eq!(s.objects.iter().filter(|o| o.parent == Some(g)).count(), 6);
        for (i, o) in s.objects.iter().enumerate() {
            assert!(o.parent.is_none_or(|p| p < i));
        }
    }

    #[test]
    fn cast_shadows_survives_duplicate_and_save() {
        use citrus_core::ShadowCasting;
        let mut s = LoadedScene::empty();
        let leaf = obj(&mut s, "Leaf", None, Vec3::ZERO);
        let caster = obj(&mut s, "Caster", Some(leaf), Vec3::ZERO);
        s.objects[leaf].cast_shadows = ShadowCasting::TwoSided;
        s.objects[caster].cast_shadows = ShadowCasting::ShadowsOnly;
        let copy = s.duplicate_object(leaf, &ComponentRegistry::with_builtins()).unwrap();
        assert_eq!(s.objects[copy].cast_shadows, ShadowCasting::TwoSided);
        assert_eq!(s.objects[copy + 1].cast_shadows, ShadowCasting::ShadowsOnly, "descendants keep theirs");
        let file = s.to_scene_file(Path::new("."), &ShaderLibrary::default());
        let saved: Vec<ShadowCasting> = file.entries.iter().map(|e| e.cast_shadows).collect();
        assert_eq!(saved, s.objects.iter().map(|o| o.cast_shadows).collect::<Vec<_>>());
    }

    #[test]
    fn cast_shadows_maps_to_the_renderer() {
        use citrus_core::ShadowCasting as C;
        use citrus_render::ShadowCasting as R;
        for (c, r) in [(C::On, R::On), (C::Off, R::Off), (C::TwoSided, R::TwoSided), (C::ShadowsOnly, R::ShadowsOnly)] {
            assert_eq!(render_shadow_casting(c), r);
            assert_eq!((c.casts(), c.visible()), (r.casts(), r.visible()));
        }
    }
}

