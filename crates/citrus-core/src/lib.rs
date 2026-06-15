//! citrus-core: the engine-runtime API shared by the engine, editor, and
//! plugins. Egui-free by design so a shipped game links it (through the engine)
//! without pulling in any editor code.
//!
//! Holds the component/behaviour system: the [`Component`]/[`TypedComponent`]
//! traits (lifecycle hooks only, no UI), [`ComponentCtx`] (the in-game API
//! surface), [`ComponentRegistry`], [`Transform`], the deferred
//! [`ComponentCommand`]s, and the built-in component data. Editor-only concerns
//! (inspector UI, viewport gizmos) are separate traits in `citrus-editor`.

use std::collections::HashMap;

use glam::{Mat4, Quat, Vec3};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

mod ik;
mod input;
mod models;
pub use ik::{IkTargets, TrackerTargets, TwoBoneSolution, solve_fabrik, solve_two_bone};
pub use input::{
    ActionBinding, ActionKind, ActionValue, Bindings, ControlScheme, InputSource, InputState, Key,
    MouseAxis, MouseButton, PadAxis, PadButton, RawInput, resolve,
};
pub use models::{
    AlphaModeModel, MatcapBlend, MaterialModel, MaterialTexturePaths, ShaderPropKindUi,
    ShaderPropUi, ShaderUiInfo, TEXTURE_SLOT_LABELS,
};

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

    /// Raw 128-bit value for wire encoding (networking).
    pub fn raw(&self) -> u128 {
        self.0
    }

    /// Reconstruct from a raw 128-bit value (networking).
    pub fn from_raw(v: u128) -> Self {
        ObjectId(v)
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

/// A decomposed transform (translation / rotation / scale). The gameplay-side
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
    /// Make this camera object the active render camera (2A).
    SetActiveCamera(ObjectId),
    /// Write another object's LOCAL transform (each field optional). Lets a pawn
    /// drive a child camera (pitch / spring-arm) without owning its component.
    SetLocalTransform {
        id: ObjectId,
        translation: Option<Vec3>,
        rotation: Option<Quat>,
        scale: Option<Vec3>,
    },
    /// Local peer requests authority over a networked object (2G). Granted +
    /// broadcast by the engine's network system; the owner replicates its
    /// transform to everyone else.
    RequestOwnership(ObjectId),
    /// Local peer relinquishes authority over a networked object, so another peer
    /// can claim it.
    ReleaseOwnership(ObjectId),
    /// Change the window/render resolution at runtime (graphics settings).
    SetResolution(u32, u32),
    /// Toggle vsync at runtime.
    SetVsync(bool),
    /// Set shadow-map resolution (256..=8192) at runtime.
    SetShadowResolution(u32),
    /// Send a networked text message (None target = broadcast / public).
    NetMessage { to: Option<u64>, text: String },
}

/// Read-only networking view handed to components each frame. Filled by the
/// engine's network system; empty/disconnected in single-player.
#[derive(Default, Clone)]
pub struct NetView {
    pub connected: bool,
    pub is_server: bool,
    /// This machine's peer id (0 when offline).
    pub local_peer: u64,
    /// Per-object current authority (only objects that are owned appear).
    pub owners: HashMap<ObjectId, u64>,
    /// Text messages received this frame: `(from_peer, is_private, text)`.
    pub messages: Vec<(u64, bool, String)>,
}

impl NetView {
    /// Current owner of an object, if any peer holds authority.
    pub fn owner(&self, id: ObjectId) -> Option<u64> {
        self.owners.get(&id).copied()
    }
    /// Whether the local peer holds authority over an object.
    pub fn owns(&self, id: ObjectId) -> bool {
        self.connected && self.owner(id) == Some(self.local_peer)
    }
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
    /// reference objects (names are cosmetic and may collide or be empty).
    pub object_ids: &'a [ObjectId],
    /// Index of the object that owns the component being updated.
    pub self_index: usize,
    /// World matrix of the owning object's parent (None if it's a root). Used to
    /// convert a desired world position/transform back into the local TRS that
    /// the `translation`/`rotation`/`scale` fields hold.
    pub parent_world: Option<Mat4>,
    /// Resolved input action snapshot for this frame (2C). Read with
    /// `ctx.input.axis2("Move")`, `ctx.input.pressed("Jump")`, etc.
    pub input: &'a InputState,
    /// Networking view (2G): ownership + local peer. Empty when offline.
    pub net: &'a NetView,
    /// Spawn points this frame as `(object index, tag)`, from objects carrying a
    /// [`SpawnPoint`] component. Resolve a transform via `spawn_point(tag)`.
    pub spawn_points: &'a [(usize, String)],
}

impl ComponentCtx<'_> {
    /// Queue a scene switch (level change, menu -> game). Project-relative
    /// path. Applied after the current update pass.
    pub fn load_scene(&mut self, path: impl Into<String>) {
        self.commands.push(ComponentCommand::LoadScene(path.into()));
    }

    /// Find an object by name; returns its index for the lookups below. Names
    /// aren't guaranteed unique, so the first match wins.
    pub fn find_object(&self, name: &str) -> Option<usize> {
        self.object_names.iter().position(|n| n == name)
    }

    /// World [`Transform`] (translation/rotation/scale) of an object: the
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

    /// World [`Transform`] of a referenced object. The preferred way to read
    /// another object you hold a reference to.
    pub fn transform_of(&self, target: ObjectRef) -> Option<Transform> {
        self.object_transform(self.resolve(target)?)
    }

    /// World position of a referenced object.
    pub fn position_of(&self, target: ObjectRef) -> Option<Vec3> {
        self.object_position(self.resolve(target)?)
    }

    // ---- camera possession (2A) ----

    /// Make a referenced camera object the active render camera (deferred).
    pub fn set_active_camera(&mut self, target: ObjectRef) {
        if let Some(id) = target.id() {
            self.commands.push(ComponentCommand::SetActiveCamera(id));
        }
    }

    /// Write another object's local transform (any subset of fields).
    pub fn set_local_transform(
        &mut self,
        target: ObjectRef,
        translation: Option<Vec3>,
        rotation: Option<Quat>,
        scale: Option<Vec3>,
    ) {
        if let Some(id) = target.id() {
            self.commands.push(ComponentCommand::SetLocalTransform {
                id,
                translation,
                rotation,
                scale,
            });
        }
    }

    // ---- networking (2G) ----

    /// Whether the local peer currently has authority over the owning object.
    pub fn owns_self(&self) -> bool {
        self.net.owns(self.self_id())
    }

    /// Whether the local peer has authority over a referenced object.
    pub fn owns(&self, target: ObjectRef) -> bool {
        target.id().map(|id| self.net.owns(id)).unwrap_or(false)
    }

    /// Request authority over the owning object (e.g. on grab). The engine grants
    /// + broadcasts it; once owned, the object's transform replicates to peers.
    pub fn request_ownership(&mut self) {
        self.commands
            .push(ComponentCommand::RequestOwnership(self.self_id()));
    }

    /// Release authority over the owning object so another peer can claim it.
    pub fn release_ownership(&mut self) {
        self.commands
            .push(ComponentCommand::ReleaseOwnership(self.self_id()));
    }

    /// Request authority over a referenced networked object.
    pub fn request_ownership_of(&mut self, target: ObjectRef) {
        if let Some(id) = target.id() {
            self.commands.push(ComponentCommand::RequestOwnership(id));
        }
    }

    // ---- graphics settings (runtime) ----

    /// Change the game resolution at runtime.
    pub fn set_resolution(&mut self, width: u32, height: u32) {
        self.commands
            .push(ComponentCommand::SetResolution(width, height));
    }
    /// Toggle vsync at runtime.
    pub fn set_vsync(&mut self, on: bool) {
        self.commands.push(ComponentCommand::SetVsync(on));
    }
    /// Set shadow-map resolution at runtime (256..=8192).
    pub fn set_shadow_resolution(&mut self, res: u32) {
        self.commands
            .push(ComponentCommand::SetShadowResolution(res));
    }

    // ---- networked messaging (2G) ----

    /// Broadcast a public text message to all peers.
    pub fn broadcast(&mut self, text: impl Into<String>) {
        self.commands.push(ComponentCommand::NetMessage {
            to: None,
            text: text.into(),
        });
    }
    /// Send a private text message to one peer.
    pub fn send_to(&mut self, peer: u64, text: impl Into<String>) {
        self.commands.push(ComponentCommand::NetMessage {
            to: Some(peer),
            text: text.into(),
        });
    }
    /// Text messages received this frame: `(from_peer, is_private, text)`.
    pub fn messages(&self) -> &[(u64, bool, String)] {
        &self.net.messages
    }

    /// World [`Transform`] of the first spawn point with this tag (2, spawn
    /// points). `None` if no matching [`SpawnPoint`] exists.
    pub fn spawn_point(&self, tag: &str) -> Option<Transform> {
        let idx = self
            .spawn_points
            .iter()
            .find(|(_, t)| t == tag)
            .map(|(i, _)| *i)?;
        self.object_transform(idx)
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
        registry.register::<ReflectionProbe>();
        registry.register::<AudioSource>();
        registry.register::<AudioListener>();
        registry.register::<VolumeComponent>();
        registry.register::<BoxCollider>();
        registry.register::<SphereCollider>();
        registry.register::<MeshCollider>();
        registry.register::<Spin>();
        registry.register::<Bob>();
        registry.register::<RigidBody>();
        registry.register::<Pawn>();
        registry.register::<SpawnPoint>();
        registry.register::<Sync>();
        registry
    }

    /// Register a component type. Re-registering a name replaces the entry;
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

/// How a light renders its shadows. `None` skips the shadow map entirely; `Hard`
/// does a single depth comparison (sharp, aliased edge); `Soft` runs the PCF
/// kernel for a smooth penumbra.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ShadowType {
    None,
    Hard,
    Soft,
}

impl ShadowType {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No Shadows",
            Self::Hard => "Hard Shadows",
            Self::Soft => "Soft Shadows",
        }
    }
    pub const ALL: [ShadowType; 3] = [Self::None, Self::Hard, Self::Soft];
    /// Whether a shadow map should be allocated/rendered for this light.
    pub fn casts(self) -> bool {
        !matches!(self, Self::None)
    }
    /// Whether the PCF kernel runs (soft penumbra) vs a single hard tap.
    pub fn soft(self) -> bool {
        matches!(self, Self::Soft)
    }
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
    #[serde(default = "default_shadow_type", alias = "cast_shadows")]
    pub shadow_type: ShadowType,
    #[serde(default = "default_shadow_bias")]
    pub shadow_bias: f32,
    /// Light source radius (world units) for baked soft shadows: the bake
    /// samples the shadow ray over a disc of this radius, so a larger value
    /// gives a wider, softer penumbra. 0 = hard shadow.
    #[serde(default = "default_light_radius")]
    pub radius: f32,
}

fn default_light_mode() -> LightMode {
    LightMode::Realtime
}
fn default_shadow_type() -> ShadowType {
    ShadowType::Soft
}
fn default_shadow_bias() -> f32 {
    0.002
}
fn default_light_radius() -> f32 {
    0.1
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
            shadow_type: ShadowType::Soft,
            shadow_bias: 0.002,
            radius: default_light_radius(),
        }
    }
}

impl LightComponent {
    /// Approximate photometric readout for the inspector. Our `intensity` isn't
    /// a physical unit (it's a radiance multiplier with a non-inverse-square
    /// falloff), so this treats `intensity` as candela-equivalent and converts
    /// to luminous flux the textbook way: point = 4π·I over the full sphere,
    /// spot = the cone's solid angle 2π(1-cos θ)·I. Directional has no flux
    /// (it's an infinite source) so it reports illuminance (lux ≈ intensity).
    /// Returns (value, unit) for display only; nothing reads it back.
    pub fn approx_photometric(&self) -> (f32, &'static str) {
        match self.kind {
            LightKind::Directional => (self.intensity, "lx"),
            LightKind::Point => (self.intensity * 4.0 * std::f32::consts::PI, "lm"),
            LightKind::Spot => {
                let half = (self.spot_angle * 0.5).to_radians();
                let solid = 2.0 * std::f32::consts::PI * (1.0 - half.cos());
                (self.intensity * solid, "lm")
            }
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

/// A precomputed reflection capture: a box zone whose surroundings are captured
/// into a roughness-prefiltered cubemap (Unreal reflection-capture / Unity
/// reflection-probe style) and sampled at runtime for specular environment
/// reflection. The object's transform places it; `size` is the box influence
/// region (used for box-projected parallax + blend between probes).
#[derive(Serialize, Deserialize)]
pub struct ReflectionProbe {
    /// Full box influence size in local meters (centered on the object).
    pub size: [f32; 3],
    /// Captured cubemap face resolution (per side). 64 / 128 / 256.
    pub resolution: u32,
    /// Reflection strength multiplier applied when sampled.
    pub intensity: f32,
    /// Box-projected parallax correction (Unity-style): reproject the reflection
    /// ray onto the box so flat surfaces line up. Off = treat as distant (infinite).
    pub box_projection: bool,
}

impl Default for ReflectionProbe {
    fn default() -> Self {
        Self {
            size: [10.0, 6.0, 10.0],
            resolution: 128,
            intensity: 1.0,
            box_projection: true,
        }
    }
}

impl TypedComponent for ReflectionProbe {
    const NAME: &'static str = "Reflection Probe";
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

/// A post-processing Volume (Unity-style): references a `.postfx` profile and
/// applies it globally or within a local box, blended by priority/weight. The
/// camera blends every volume affecting it into one effective profile.
#[derive(Serialize, Deserialize)]
pub struct VolumeComponent {
    /// Project-relative path to the `.postfx` profile this volume applies.
    pub profile: String,
    /// Global affects the whole scene; local only within the box (object
    /// transform + `half_extents`), faded in over `blend_distance`.
    pub global: bool,
    /// Higher priority blends last (wins ties).
    pub priority: f32,
    /// Maximum contribution, 0..1.
    pub weight: f32,
    /// Local: world-units over which the effect fades in approaching the box.
    pub blend_distance: f32,
    /// Local box half-size (object-local units).
    pub half_extents: [f32; 3],
}

impl Default for VolumeComponent {
    fn default() -> Self {
        Self {
            profile: String::new(),
            global: true,
            priority: 0.0,
            weight: 1.0,
            blend_distance: 1.0,
            half_extents: [5.0, 5.0, 5.0],
        }
    }
}

impl TypedComponent for VolumeComponent {
    const NAME: &'static str = "Volume (Post FX)";
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

/// How the physics engine simulates a body. Dynamic bodies fall and react to
/// forces/collisions; Kinematic are moved by gameplay but push dynamics;
/// Fixed never move (static geometry / level colliders).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BodyKind {
    Dynamic,
    Kinematic,
    Fixed,
}

impl BodyKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dynamic => "Dynamic",
            Self::Kinematic => "Kinematic",
            Self::Fixed => "Fixed",
        }
    }
    pub const ALL: [BodyKind; 3] = [Self::Dynamic, Self::Kinematic, Self::Fixed];
}

/// Makes an object participate in the physics simulation (paired with a
/// collider for its shape). The engine builds a rapier rigid body from this on
/// Play and writes the simulated transform back each step.
#[derive(Serialize, Deserialize)]
pub struct RigidBody {
    pub kind: BodyKind,
    /// Mass in kg (Dynamic only; <= 0 falls back to the collider's density).
    pub mass: f32,
    /// Bounciness 0..1.
    pub restitution: f32,
    pub friction: f32,
    /// Per-body gravity multiplier (0 = floats).
    pub gravity_scale: f32,
}

impl Default for RigidBody {
    fn default() -> Self {
        Self {
            kind: BodyKind::Dynamic,
            mass: 1.0,
            restitution: 0.0,
            friction: 0.5,
            gravity_scale: 1.0,
        }
    }
}

impl TypedComponent for RigidBody {
    const NAME: &'static str = "Rigid Body";
}

// -------------------------------------------------- pawns & controllers (2A/2B)

/// How a [`Pawn`] reads input and places its camera.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ControlMode {
    /// WASD + mouselook; camera at eye height, body yaws, camera pitches.
    FirstPerson,
    /// WASD relative to facing; orbit camera on a spring arm behind the body.
    ThirdPerson,
    /// WASD in world plane; body faces movement; fixed high camera angle.
    TopDown,
    /// Camera-only: WASD pans, wheel zooms; no body movement.
    Strategy,
}

impl ControlMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::FirstPerson => "First Person",
            Self::ThirdPerson => "Third Person",
            Self::TopDown => "Top Down",
            Self::Strategy => "Strategy",
        }
    }
    pub const ALL: [ControlMode; 4] = [
        Self::FirstPerson,
        Self::ThirdPerson,
        Self::TopDown,
        Self::Strategy,
    ];
}

/// A controllable entity (pawn + player-controller combined). When `possessed`,
/// it reads the action snapshot, moves itself, drives + activates its camera, and
/// (FP/TP) writes the child camera's pitch / spring-arm. Movement is transform-
/// based on a flat floor for now; RigidBody-driven movement is a follow-up.
#[derive(Serialize, Deserialize)]
pub struct Pawn {
    pub mode: ControlMode,
    /// The local player controls this pawn (it receives input + owns the view).
    pub possessed: bool,
    /// Camera object this pawn drives/activates. For FP/TP make it a CHILD of the
    /// pawn so body yaw rotates it; the pawn writes its local pitch / arm offset.
    pub camera: ObjectRef,
    /// If set, the pawn teleports to a matching [`SpawnPoint`] on Play start.
    #[serde(default)]
    pub spawn_tag: String,
    pub move_speed: f32,
    pub accel: f32,
    pub decel: f32,
    pub jump_power: f32,
    pub gravity: f32,
    pub look_sensitivity: f32,
    pub eye_height: f32,
    /// Third-person / top-down camera distance.
    pub arm_length: f32,
    pub min_pitch: f32,
    pub max_pitch: f32,
    #[serde(skip)]
    vel: Vec3,
    #[serde(skip)]
    yaw: f32,
    #[serde(skip)]
    pitch: f32,
    #[serde(skip)]
    floor_y: f32,
    #[serde(skip)]
    grounded: bool,
    #[serde(skip)]
    started: bool,
}

impl Default for Pawn {
    fn default() -> Self {
        Self {
            mode: ControlMode::FirstPerson,
            possessed: true,
            camera: ObjectRef::NONE,
            spawn_tag: String::new(),
            move_speed: 6.0,
            accel: 40.0,
            decel: 30.0,
            jump_power: 6.0,
            gravity: 18.0,
            look_sensitivity: 0.15,
            eye_height: 1.6,
            arm_length: 5.0,
            min_pitch: -85.0,
            max_pitch: 85.0,
            vel: Vec3::ZERO,
            yaw: 0.0,
            pitch: -15.0,
            floor_y: 0.0,
            grounded: true,
            started: false,
        }
    }
}

impl Pawn {
    fn place_camera(&self, ctx: &mut ComponentCtx) {
        let (trans, rot) = match self.mode {
            ControlMode::FirstPerson => (
                Vec3::new(0.0, self.eye_height, 0.0),
                Quat::from_rotation_x(self.pitch.to_radians()),
            ),
            ControlMode::ThirdPerson => (
                Vec3::new(0.0, self.eye_height, self.arm_length),
                Quat::from_rotation_x(self.pitch.to_radians()),
            ),
            ControlMode::TopDown | ControlMode::Strategy => (
                Vec3::new(0.0, self.arm_length, self.arm_length * 0.6),
                Quat::from_rotation_x((-60.0_f32).to_radians()),
            ),
        };
        ctx.set_local_transform(self.camera, Some(trans), Some(rot), None);
    }
}

impl TypedComponent for Pawn {
    const NAME: &'static str = "Pawn";

    fn start(&mut self, ctx: &mut ComponentCtx) {
        self.vel = Vec3::ZERO;
        self.floor_y = ctx.self_transform().translation.y;
        // Spawn-point placement (2 — spawn points).
        if !self.spawn_tag.is_empty()
            && let Some(t) = ctx.spawn_point(&self.spawn_tag)
        {
            ctx.set_world_position(t.translation);
            self.floor_y = t.translation.y;
            self.yaw = t.rotation.to_euler(glam::EulerRot::YXZ).0.to_degrees();
        }
        if self.possessed {
            ctx.set_active_camera(self.camera);
        }
        *ctx.rotation = Quat::from_rotation_y(self.yaw.to_radians());
        self.place_camera(ctx);
        self.started = true;
    }

    fn update(&mut self, ctx: &mut ComponentCtx) {
        if !self.possessed {
            return;
        }
        if !self.started {
            TypedComponent::start(self, ctx);
        }
        let dt = ctx.dt.min(0.1);
        let look = ctx.input.axis2("Look");
        let mv = ctx.input.axis2("Move");

        // Look: mouse delta is per-frame (no dt); analog sticks pre-scaled.
        self.yaw -= look.x * self.look_sensitivity;
        if matches!(self.mode, ControlMode::FirstPerson | ControlMode::ThirdPerson) {
            self.pitch = (self.pitch - look.y * self.look_sensitivity)
                .clamp(self.min_pitch, self.max_pitch);
        }

        // Movement direction per mode.
        let yaw_rot = Quat::from_rotation_y(self.yaw.to_radians());
        let (dir, face_move) = match self.mode {
            ControlMode::FirstPerson | ControlMode::ThirdPerson => {
                let fwd = yaw_rot * Vec3::NEG_Z;
                let right = yaw_rot * Vec3::X;
                (right * mv.x + fwd * mv.y, false)
            }
            // World-plane movement; body faces the move direction.
            ControlMode::TopDown | ControlMode::Strategy => {
                (Vec3::new(mv.x, 0.0, -mv.y), true)
            }
        };
        let dir = dir.normalize_or_zero();

        let mut pos = *ctx.translation;
        let horizontal = if dir.length_squared() > 1e-4 {
            let target = dir * self.move_speed;
            let cur = Vec3::new(self.vel.x, 0.0, self.vel.z);
            let next = cur.lerp(target, (self.accel * dt).min(1.0));
            if face_move && self.mode == ControlMode::TopDown {
                self.yaw = (-dir.x).atan2(-dir.z).to_degrees();
            }
            next
        } else {
            let cur = Vec3::new(self.vel.x, 0.0, self.vel.z);
            cur.lerp(Vec3::ZERO, (self.decel * dt).min(1.0))
        };
        self.vel.x = horizontal.x;
        self.vel.z = horizontal.z;

        if matches!(self.mode, ControlMode::FirstPerson | ControlMode::ThirdPerson) {
            // Gravity + jump on a flat floor.
            if self.grounded && ctx.input.pressed("Jump") {
                self.vel.y = self.jump_power;
                self.grounded = false;
            }
            self.vel.y -= self.gravity * dt;
        } else {
            self.vel.y = 0.0;
        }

        pos += self.vel * dt;
        if matches!(self.mode, ControlMode::FirstPerson | ControlMode::ThirdPerson)
            && pos.y <= self.floor_y
        {
            pos.y = self.floor_y;
            self.vel.y = 0.0;
            self.grounded = true;
        }
        *ctx.translation = pos;

        if !matches!(self.mode, ControlMode::Strategy) {
            *ctx.rotation = Quat::from_rotation_y(self.yaw.to_radians());
        }
        // Strategy zoom on the wheel via the Look.y / wheel channel is omitted;
        // arm_length stays as authored.
        self.place_camera(ctx);
    }
}

/// Marks an object as a spawn location. Pawns with a matching `spawn_tag` teleport
/// here on Play start; game code finds them via `ctx.spawn_point(tag)`.
#[derive(Serialize, Deserialize)]
pub struct SpawnPoint {
    /// Group this point belongs to: "player", "npc", a team name, etc.
    pub tag: String,
    /// Ordering hint within the tag (round-robin / slot index).
    #[serde(default)]
    pub index: u32,
}

impl Default for SpawnPoint {
    fn default() -> Self {
        Self {
            tag: "player".to_string(),
            index: 0,
        }
    }
}

impl TypedComponent for SpawnPoint {
    const NAME: &'static str = "Spawn Point";
}

// ------------------------------------------------------------- networking (2G)

/// Replicates the owning object's transform over the network. Whoever has
/// authority (see [`NetView`]) broadcasts its transform; everyone else receives
/// and applies it (optionally smoothed). `grabbable` lets the local player take
/// ownership by pressing `grab_action`, move it, and release it for others.
#[derive(Serialize, Deserialize)]
pub struct Sync {
    /// Any peer can grab authority by pressing `grab_action`.
    pub grabbable: bool,
    /// Action that toggles local ownership when `grabbable`.
    pub grab_action: String,
    /// Remote-update smoothing rate (0 = snap; higher = snappier lerp).
    pub smoothing: f32,
    #[serde(skip)]
    started: bool,
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            grabbable: true,
            grab_action: "Grab".to_string(),
            smoothing: 12.0,
            started: false,
        }
    }
}

impl TypedComponent for Sync {
    const NAME: &'static str = "Sync";

    fn update(&mut self, ctx: &mut ComponentCtx) {
        self.started = true;
        if !ctx.net.connected || !self.grabbable || self.grab_action.is_empty() {
            return;
        }
        // Toggle authority on grab: take it if we don't own it, else release.
        if ctx.input.pressed(&self.grab_action) {
            if ctx.owns_self() {
                ctx.release_ownership();
            } else {
                ctx.request_ownership();
            }
        }
    }
}
