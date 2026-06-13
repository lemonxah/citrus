//! citrus-core: the engine-runtime API shared by the engine, editor, and
//! plugins. Egui-free by design so a shipped game links it (through the engine)
//! without pulling in any editor code.
//!
//! Holds the component/behaviour system: the [`Component`]/[`TypedComponent`]
//! traits (lifecycle hooks only — no UI), [`ComponentCtx`] (the in-game API
//! surface), [`ComponentRegistry`], [`Transform`], the deferred
//! [`ComponentCommand`]s, and the built-in component data. Editor-only concerns
//! (inspector UI, viewport gizmos) are separate traits in `citrus-editor`.

use glam::{Mat4, Quat, Vec3};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------ object id

/// A stable, globally-unique object identity (UUID v4). Assigned when an object
/// is created (editor spawn / import / scene load) and serialized in `.scene`,
/// so cross-references survive reloads, reordering, and networking. Names stay
/// purely cosmetic. Serialized as the canonical hyphenated string.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct ObjectId(u128);

impl ObjectId {
    /// The "unassigned" id (all-zero). Used as a placeholder until a real id is
    /// generated (e.g. loading a legacy scene without ids).
    pub const NIL: ObjectId = ObjectId(0);

    /// Generate a fresh random v4 UUID.
    pub fn new() -> Self {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("getrandom for object id");
        b[6] = (b[6] & 0x0f) | 0x40; // version 4
        b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
        ObjectId(u128::from_be_bytes(b))
    }

    pub fn is_nil(&self) -> bool {
        self.0 == 0
    }
}

impl Default for ObjectId {
    fn default() -> Self {
        ObjectId::NIL
    }
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0.to_be_bytes();
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
            b[14], b[15]
        )
    }
}

impl std::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

impl std::str::FromStr for ObjectId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 {
            return Err(format!("invalid object id {s:?}"));
        }
        u128::from_str_radix(&hex, 16)
            .map(ObjectId)
            .map_err(|e| format!("invalid object id {s:?}: {e}"))
    }
}

impl From<ObjectId> for String {
    fn from(id: ObjectId) -> String {
        id.to_string()
    }
}

impl TryFrom<String> for ObjectId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// A reference to another object, settable in the editor and resolved at
/// runtime through [`ComponentCtx`]. `None` = unset. Components store this
/// instead of an index or a name.
#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ObjectRef(pub Option<ObjectId>);

impl ObjectRef {
    pub const NONE: ObjectRef = ObjectRef(None);
    pub fn new(id: ObjectId) -> Self {
        ObjectRef(Some(id))
    }
    pub fn id(&self) -> Option<ObjectId> {
        self.0
    }
    pub fn is_set(&self) -> bool {
        self.0.is_some()
    }
}

// ------------------------------------------------------------------ transform

/// A decomposed transform (translation / rotation / scale) — the gameplay-side
/// view of any object. The in-game API resolves object references into this.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

impl Transform {
    pub fn from_matrix(m: Mat4) -> Self {
        let (scale, rotation, translation) = m.to_scale_rotation_translation();
        Self {
            translation,
            rotation,
            scale,
        }
    }
    pub fn matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
    /// Local -Z in world space (the "looking" direction).
    pub fn forward(&self) -> Vec3 {
        self.rotation * Vec3::NEG_Z
    }
    pub fn right(&self) -> Vec3 {
        self.rotation * Vec3::X
    }
    pub fn up(&self) -> Vec3 {
        self.rotation * Vec3::Y
    }
}

// -------------------------------------------------------------- in-game ctx

/// Deferred engine request a component can make from a lifecycle hook. The
/// engine applies these after the update pass finishes (so the scene isn't
/// mutated mid-iteration). The first slice of the in-game API; grow the enum as
/// more world control is exposed.
#[derive(Debug, Clone)]
pub enum ComponentCommand {
    /// Switch to another scene (level change, menu -> game). Path is
    /// project-relative, e.g. "scenes/level2.scene". Replaces the current scene.
    LoadScene(String),
}

/// Per-frame update context handed to components in Play mode. Transform
/// fields are the owning object's local TRS.
pub struct ComponentCtx<'a> {
    pub dt: f32,
    /// Seconds since engine start.
    pub time: f32,
    pub translation: &'a mut Vec3,
    pub rotation: &'a mut Quat,
    pub scale: &'a mut Vec3,
    /// Deferred engine requests; drained and applied after the update pass.
    pub commands: &'a mut Vec<ComponentCommand>,
    /// Read-only world view: every object's world transform this frame
    /// (snapshot at the start of the update pass) for resolving object
    /// references. Parallel to `object_names`.
    pub world_transforms: &'a [Mat4],
    /// Object names, parallel to `world_transforms`.
    pub object_names: &'a [String],
    /// Stable object ids, parallel to `world_transforms`. The preferred way to
    /// reference objects (names are cosmetic and may collide / be empty).
    pub object_ids: &'a [ObjectId],
    /// Index of the object that owns the component being updated.
    pub self_index: usize,
    /// World matrix of the owning object's parent (None if it's a root). Used to
    /// convert a desired world position/transform back into the local TRS that
    /// the `translation`/`rotation`/`scale` fields hold.
    pub parent_world: Option<Mat4>,
}

impl ComponentCtx<'_> {
    /// Queue a scene switch (level change, menu -> game). Project-relative
    /// path. Applied after the current update pass.
    pub fn load_scene(&mut self, path: impl Into<String>) {
        self.commands.push(ComponentCommand::LoadScene(path.into()));
    }

    /// Find an object by name; returns its index for the lookups below. Names
    /// aren't guaranteed unique — the first match wins.
    pub fn find_object(&self, name: &str) -> Option<usize> {
        self.object_names.iter().position(|n| n == name)
    }

    /// World [`Transform`] (translation/rotation/scale) of an object — the
    /// gameplay view of any object reference (snapshot at frame start).
    pub fn object_transform(&self, index: usize) -> Option<Transform> {
        self.world_transforms
            .get(index)
            .map(|m| Transform::from_matrix(*m))
    }

    /// Raw world matrix of an object (when you need the matrix, not the
    /// decomposed transform).
    pub fn object_matrix(&self, index: usize) -> Option<Mat4> {
        self.world_transforms.get(index).copied()
    }

    /// World position of an object.
    pub fn object_position(&self, index: usize) -> Option<Vec3> {
        self.world_transforms.get(index).map(|m| m.w_axis.truncate())
    }

    /// Find an object by name and return its world [`Transform`] in one step.
    pub fn object_transform_named(&self, name: &str) -> Option<Transform> {
        self.find_object(name).and_then(|i| self.object_transform(i))
    }

    /// World [`Transform`] of the owning object.
    pub fn self_transform(&self) -> Transform {
        self.object_transform(self.self_index).unwrap_or_default()
    }

    /// Place the owning object at a world-space position, accounting for its
    /// parent. Writes the local translation (the field the engine reads), so
    /// this is correct whether the object is a root or nested under parents.
    /// Prefer this over assigning `*ctx.translation` directly when the value you
    /// have is a world coordinate (e.g. another object's position).
    pub fn set_world_position(&mut self, world: Vec3) {
        *self.translation = match self.parent_world {
            Some(p) => p.inverse().transform_point3(world),
            None => world,
        };
    }

    /// The owning object's stable id.
    pub fn self_id(&self) -> ObjectId {
        self.object_ids
            .get(self.self_index)
            .copied()
            .unwrap_or_default()
    }

    /// Resolve a stable id to its current index.
    pub fn index_of(&self, id: ObjectId) -> Option<usize> {
        self.object_ids.iter().position(|i| *i == id)
    }

    /// Resolve an [`ObjectRef`] to its index (None if unset or gone).
    pub fn resolve(&self, target: ObjectRef) -> Option<usize> {
        self.index_of(target.id()?)
    }

    /// World [`Transform`] of a referenced object — the preferred way to read
    /// another object you hold a reference to.
    pub fn transform_of(&self, target: ObjectRef) -> Option<Transform> {
        self.object_transform(self.resolve(target)?)
    }

    /// World position of a referenced object.
    pub fn position_of(&self, target: ObjectRef) -> Option<Vec3> {
        self.object_position(self.resolve(target)?)
    }
}

// ----------------------------------------------------------- component trait

/// Object-safe component, as stored on scene objects. Implement
/// [`TypedComponent`] instead of this directly. Runtime-only: editor inspector
/// UI and viewport gizmos are separate traits in `citrus-editor`.
pub trait Component: 'static {
    fn type_name(&self) -> &'static str;
    /// Called once when Play mode starts.
    fn start(&mut self, ctx: &mut ComponentCtx);
    /// Per-frame behaviour (Play mode only).
    fn update(&mut self, ctx: &mut ComponentCtx);
    /// Runs after every component finished `update` this frame.
    fn late_update(&mut self, ctx: &mut ComponentCtx);
    /// Serialize to RON.
    fn save(&self) -> String;
    /// Downcasting for engine + editor systems that read typed component data.
    fn as_any(&self) -> &dyn std::any::Any;
    /// Mutable downcasting (assigning ids, editor inspector/gizmo dispatch).
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Typed component interface; blanket-implements [`Component`]. All lifecycle
/// hooks default to no-ops — override the ones you need. Inspector UI lives in
/// `citrus-editor`'s `Inspect` trait, not here.
pub trait TypedComponent: Serialize + DeserializeOwned + Default + 'static {
    const NAME: &'static str;
    /// Called once when Play mode starts.
    fn start(&mut self, _ctx: &mut ComponentCtx) {}
    /// Called every frame while playing.
    fn update(&mut self, _ctx: &mut ComponentCtx) {}
    /// Called every frame after all components ran `update`.
    fn late_update(&mut self, _ctx: &mut ComponentCtx) {}
}

impl<T: TypedComponent> Component for T {
    fn type_name(&self) -> &'static str {
        T::NAME
    }
    fn start(&mut self, ctx: &mut ComponentCtx) {
        TypedComponent::start(self, ctx)
    }
    fn update(&mut self, ctx: &mut ComponentCtx) {
        TypedComponent::update(self, ctx)
    }
    fn late_update(&mut self, ctx: &mut ComponentCtx) {
        TypedComponent::late_update(self, ctx)
    }
    fn save(&self) -> String {
        ron::to_string(self).unwrap_or_default()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

struct ComponentInfo {
    name: &'static str,
    create: fn() -> Box<dyn Component>,
    load: fn(&str) -> Result<Box<dyn Component>, ron::error::SpannedError>,
}

/// Name → component factory/loader (runtime). One registry per app; plugins
/// extend it via their `citrus_register` export.
#[derive(Default)]
pub struct ComponentRegistry {
    entries: Vec<ComponentInfo>,
}

impl ComponentRegistry {
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register::<CameraComponent>();
        registry.register::<LightComponent>();
        registry.register::<LightProbeVolume>();
        registry.register::<AudioSource>();
        registry.register::<AudioListener>();
        registry.register::<BoxCollider>();
        registry.register::<SphereCollider>();
        registry.register::<MeshCollider>();
        registry.register::<Spin>();
        registry.register::<Bob>();
        registry
    }

    /// Register a component type. Re-registering a name replaces the entry —
    /// that's how hot-reloaded plugin components supersede their old build.
    pub fn register<T: TypedComponent>(&mut self) {
        let info = ComponentInfo {
            name: T::NAME,
            create: || Box::new(T::default()),
            load: |data| Ok(Box::new(ron::from_str::<T>(data)?) as Box<dyn Component>),
        };
        match self.entries.iter_mut().find(|e| e.name == T::NAME) {
            Some(entry) => {
                tracing::debug!("component {:?} re-registered (reload)", T::NAME);
                *entry = info;
            }
            None => self.entries.push(info),
        }
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.iter().map(|e| e.name)
    }

    pub fn create(&self, name: &str) -> Option<Box<dyn Component>> {
        let entry = self.entries.iter().find(|e| e.name == name)?;
        Some((entry.create)())
    }

    /// Deserialize a saved component. Unknown names and broken data log a
    /// warning and return None so scene loads survive missing components.
    pub fn load(&self, name: &str, data: &str) -> Option<Box<dyn Component>> {
        let Some(entry) = self.entries.iter().find(|e| e.name == name) else {
            tracing::warn!("unknown component {name:?}; dropping it");
            return None;
        };
        match (entry.load)(data) {
            Ok(component) => Some(component),
            Err(e) => {
                tracing::warn!("loading component {name:?}: {e}");
                None
            }
        }
    }
}

// ----------------------------------------------------------- builtin data

/// Camera properties (Unity-style). Attached automatically to camera objects;
/// the viewport draws a frustum widget from these values.
#[derive(Serialize, Deserialize)]
pub struct CameraComponent {
    /// Stable per-camera identity, saved in the `.scene`. The scene's "main"
    /// camera is the one with the smallest id. 0 = not yet assigned.
    #[serde(default)]
    pub id: u32,
    /// Vertical field of view, degrees.
    pub fov_deg: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for CameraComponent {
    fn default() -> Self {
        Self {
            id: 0,
            fov_deg: 60.0,
            near: 0.1,
            far: 100.0,
        }
    }
}

impl TypedComponent for CameraComponent {
    const NAME: &'static str = "Camera";
}

/// What shape of light a [`LightComponent`] emits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LightKind {
    Directional,
    Point,
    Spot,
}

impl LightKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Directional => "Directional",
            Self::Point => "Point",
            Self::Spot => "Spot",
        }
    }
    pub const ALL: [LightKind; 3] = [Self::Directional, Self::Point, Self::Spot];
}

/// Whether a light is recomputed every frame or baked into the scene's lighting.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LightMode {
    Realtime,
    Baked,
    Mixed,
}

impl LightMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Realtime => "Realtime",
            Self::Baked => "Baked (static)",
            Self::Mixed => "Mixed",
        }
    }
    pub const ALL: [LightMode; 3] = [Self::Realtime, Self::Baked, Self::Mixed];
}

/// A scene light. The object's transform places/orients it; the engine gathers
/// every `LightComponent` each frame and feeds the renderer's multi-light path.
#[derive(Serialize, Deserialize)]
pub struct LightComponent {
    pub kind: LightKind,
    #[serde(default = "default_light_mode")]
    pub mode: LightMode,
    /// Linear RGB tint, 0..1.
    pub color: [f32; 3],
    pub intensity: f32,
    pub range: f32,
    /// Spot cone full angle at the outer edge, degrees.
    pub spot_angle: f32,
    /// 0 = hard cone edge, 1 = soft falloff.
    pub spot_blend: f32,
    #[serde(default = "default_true_light")]
    pub cast_shadows: bool,
    #[serde(default = "default_shadow_bias")]
    pub shadow_bias: f32,
}

fn default_light_mode() -> LightMode {
    LightMode::Realtime
}
fn default_true_light() -> bool {
    true
}
fn default_shadow_bias() -> f32 {
    0.002
}

impl Default for LightComponent {
    fn default() -> Self {
        Self {
            kind: LightKind::Directional,
            mode: LightMode::Realtime,
            color: [1.0, 0.98, 0.92],
            intensity: 3.0,
            range: 10.0,
            spot_angle: 45.0,
            spot_blend: 0.15,
            cast_shadows: true,
            shadow_bias: 0.002,
        }
    }
}

impl TypedComponent for LightComponent {
    const NAME: &'static str = "Light";
}

/// Rotate around a local axis at constant speed.
#[derive(Serialize, Deserialize)]
pub struct Spin {
    pub axis: [f32; 3],
    pub degrees_per_second: f32,
}

impl Default for Spin {
    fn default() -> Self {
        Self {
            axis: [0.0, 1.0, 0.0],
            degrees_per_second: 45.0,
        }
    }
}

impl TypedComponent for Spin {
    const NAME: &'static str = "Spin";
    fn update(&mut self, ctx: &mut ComponentCtx) {
        let axis = Vec3::from(self.axis).normalize_or(Vec3::Y);
        let step = Quat::from_axis_angle(axis, self.degrees_per_second.to_radians() * ctx.dt);
        *ctx.rotation = (step * *ctx.rotation).normalize();
    }
}

/// Sine-wave hover along an axis.
#[derive(Serialize, Deserialize)]
pub struct Bob {
    pub axis: [f32; 3],
    pub amplitude: f32,
    /// Cycles per second.
    pub frequency: f32,
    /// Offset currently applied, so each frame applies only the delta.
    #[serde(skip)]
    applied: Vec3,
}

impl Default for Bob {
    fn default() -> Self {
        Self {
            axis: [0.0, 1.0, 0.0],
            amplitude: 0.5,
            frequency: 0.5,
            applied: Vec3::ZERO,
        }
    }
}

impl TypedComponent for Bob {
    const NAME: &'static str = "Bob";
    fn update(&mut self, ctx: &mut ComponentCtx) {
        let phase = ctx.time * self.frequency * std::f32::consts::TAU;
        let offset = Vec3::from(self.axis) * (self.amplitude * phase.sin());
        *ctx.translation += offset - self.applied;
        self.applied = offset;
    }
}

/// A box volume seeded with a regular grid of irradiance light probes.
#[derive(Serialize, Deserialize)]
pub struct LightProbeVolume {
    /// Full box size in local meters (grid spans -size/2 .. +size/2).
    pub size: [f32; 3],
    /// Probes per meter along each axis.
    pub density: f32,
}

impl Default for LightProbeVolume {
    fn default() -> Self {
        Self {
            size: [4.0, 3.0, 4.0],
            density: 1.0,
        }
    }
}

impl LightProbeVolume {
    /// Probe count along each axis (>= 2 so a cell always has 8 corners).
    pub fn counts(&self) -> [usize; 3] {
        let mut out = [2usize; 3];
        for (i, c) in out.iter_mut().enumerate() {
            let n = (self.size[i].max(0.0) * self.density).round() as i64 + 1;
            *c = n.clamp(2, 256) as usize;
        }
        out
    }

    pub fn probe_count(&self) -> usize {
        let [x, y, z] = self.counts();
        x * y * z
    }

    /// Probe positions in local space, x-fastest then y then z.
    pub fn local_positions(&self) -> Vec<Vec3> {
        let [nx, ny, nz] = self.counts();
        let half = Vec3::from(self.size) * 0.5;
        let step = |n: usize, axis: usize| {
            if n > 1 {
                self.size[axis] / (n - 1) as f32
            } else {
                0.0
            }
        };
        let (sx, sy, sz) = (step(nx, 0), step(ny, 1), step(nz, 2));
        let mut out = Vec::with_capacity(nx * ny * nz);
        for iz in 0..nz {
            for iy in 0..ny {
                for ix in 0..nx {
                    out.push(Vec3::new(
                        -half.x + sx * ix as f32,
                        -half.y + sy * iy as f32,
                        -half.z + sz * iz as f32,
                    ));
                }
            }
        }
        out
    }
}

impl TypedComponent for LightProbeVolume {
    const NAME: &'static str = "Light Probe Volume";
}

/// How a spatial audio source quietens with distance.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AudioRolloff {
    Linear,
    Logarithmic,
}

impl AudioRolloff {
    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Logarithmic => "Logarithmic",
        }
    }
    pub const ALL: [AudioRolloff; 2] = [Self::Linear, Self::Logarithmic];
}

/// A sound emitter. Spatial sources attenuate with distance to the
/// AudioListener; non-spatial play at a constant volume.
#[derive(Serialize, Deserialize)]
pub struct AudioSource {
    pub clip: String,
    pub play_on_start: bool,
    pub looping: bool,
    pub volume: f32,
    pub pitch: f32,
    pub spatial: bool,
    pub min_distance: f32,
    pub max_distance: f32,
    pub rolloff: AudioRolloff,
}

impl Default for AudioSource {
    fn default() -> Self {
        Self {
            clip: String::new(),
            play_on_start: true,
            looping: false,
            volume: 1.0,
            pitch: 1.0,
            spatial: true,
            min_distance: 1.0,
            max_distance: 20.0,
            rolloff: AudioRolloff::Logarithmic,
        }
    }
}

impl TypedComponent for AudioSource {
    const NAME: &'static str = "Audio Source";
}

/// Marks the object whose transform is the "ears" for spatial audio.
#[derive(Serialize, Deserialize, Default)]
pub struct AudioListener {}

impl TypedComponent for AudioListener {
    const NAME: &'static str = "Audio Listener";
}

/// Number of collision layers (mirrors the layer-collision matrix the physics
/// engine will consult).
pub const COLLISION_LAYERS: u32 = 32;

/// Axis-aligned (object space) box collision zone. Authoring data for the
/// physics engine.
#[derive(Serialize, Deserialize)]
pub struct BoxCollider {
    pub center: [f32; 3],
    pub size: [f32; 3],
    pub is_trigger: bool,
    pub layer: u32,
}

impl Default for BoxCollider {
    fn default() -> Self {
        Self {
            center: [0.0; 3],
            size: [1.0; 3],
            is_trigger: false,
            layer: 0,
        }
    }
}

impl TypedComponent for BoxCollider {
    const NAME: &'static str = "Box Collider";
}

/// Sphere collision zone (center offset + radius).
#[derive(Serialize, Deserialize)]
pub struct SphereCollider {
    pub center: [f32; 3],
    pub radius: f32,
    pub is_trigger: bool,
    pub layer: u32,
}

impl Default for SphereCollider {
    fn default() -> Self {
        Self {
            center: [0.0; 3],
            radius: 0.5,
            is_trigger: false,
            layer: 0,
        }
    }
}

impl TypedComponent for SphereCollider {
    const NAME: &'static str = "Sphere Collider";
}

/// Collision from the object's own mesh (convex hull or exact triangles).
#[derive(Serialize, Deserialize)]
pub struct MeshCollider {
    pub convex: bool,
    pub is_trigger: bool,
    pub layer: u32,
}

impl Default for MeshCollider {
    fn default() -> Self {
        Self {
            convex: false,
            is_trigger: false,
            layer: 0,
        }
    }
}

impl TypedComponent for MeshCollider {
    const NAME: &'static str = "Mesh Collider";
}
