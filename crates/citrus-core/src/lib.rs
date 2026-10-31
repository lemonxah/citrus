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

mod audio_mixer;
mod cloth;
mod cloth_mesh;
pub use cloth_mesh::{MeshCloth, MeshClothRuntime, MeshClothSim, MeshParticle, PinSource};
mod cloth_pins;
pub use cloth_pins::{
    BrushFalloff, BrushMode, ClothPaint, Dab, PackedBytes, PaintChannel, PaintFit, WeldTopology, mirror_map, mirror_x,
    ray_triangles, select_by_height, topology_hash,
};
mod face;
mod voice;
mod water;
mod water_sim;
mod water_pipe;
mod water_ewave;
mod physics_material;
pub mod asset_ref;
mod water_fx;
mod water_container;
pub use water_container::{
    LITRES_PER_M3, WaterContainer, WaterContainerRuntime, held_volume, inside_box, level_for_volume, rim_corners, rim_level, spill_point, surface_polygon,
    volume_below, wet_lip,
};
mod wave_rider;
pub use wave_rider::{RideMode, RideState, WaveRider};
pub use water_fx::{GRAVITY, Vortex, WaterSplash, Waterfall, Whirlpool, fall_time, vortex_flow};
pub use physics_material::{CombineMode, PHYSMAT_EXTENSION, PhysicsMaterial, PhysicsSettings, load_physmat, save_physmat};
pub use water_sim::{RippleSim, WaterPush};
pub use water_ewave::{EwaveLayer, EwaveParams, SURFACE_TENSION, dispersive_share, ewave_omega, ewave_supported};
pub use water_pipe::{
    CELL_OBSTACLE, CELL_SHORE, CELL_WATER, MAX_SIM_SUBSTEPS, RUNUP_HEIGHT, PipeParams, PipeSim, SimField, SimWindow, WaterDisplacer, WaterSimFeed, choose_window,
    max_sim_dt, pipe_substeps, sim_resolution,
};
pub use water::{Buoyancy, FLUID_PRESETS, FluidPreset, GerstnerWave, OutlinePreset, ResolvedWave, RiverPoint, RiverSection, UnderwaterSettings, Water, WaterBody, WaterColumn, WaterLook, WaterRuntime};
pub use voice::{
    LipSync, LipSyncMode, LipSyncRuntime, VISEMES, VisemeFrame, VoiceChat, VoiceRange, VoiceRolloff, VoiceTransmit,
    viseme_index,
};
pub use face::{
    BlendShapes, BlendShapesRuntime, EXPRESSIONS, normalize, FaceFrame, FaceSource, TrackingInputSettings, FaceTracking, FaceTrackingRuntime, ShapeSource,
    expression_index,
};
pub use cloth::{
    BoneCloth, BoneClothRuntime, BoneClothSim, ClothWind, ClothWindParams, WindSample, WindShape, ClothCollider, ClothColliderRuntime, ClothColliders, ClothConnection,
    ClothFit, ClothFitPreset, ClothParticle, ClothShape, ClothShapeWorld, ClothTeleport, fit_capsule, max_scale,
    principal_axis, rigid_frame,
};
mod spatial_audio;
mod gameplay;
mod ik;
mod input;
mod input_remap;
mod localization;
mod models;
mod savegame;
mod ui;
mod vr;
mod body;
pub use body::{
    AvatarScaling, BodyCommand, BodyFit, BodyInfo, BodyProfile, DEFAULT_EYE_HEIGHT, EYE_TO_CROWN, FIRST_BODY_SLOT,
    HEIGHT_CAPTURE_SECONDS, HeightSampler, StoredPose, TrackerRole, arm_reach, arm_span, assign_roles, fit_avatar,
    robust_mean, role_name,
};
pub use ik::{
    HumanoidIk, HumanoidIkRuntime, IkSource, IkTargets, TrackingLocomotion, TRACKER_ROLES, TRACKER_SLOTS, TrackerCalibration, TrackerTargets, TwoBoneSolution, solve_fabrik,
    solve_two_bone,
};
pub use audio_mixer::AudioMixer;
pub use spatial_audio::{
    AcousticMaterial, AcousticPreset, AudioProbeVolume, OutputLayout, RayTracerKind, ReflectionMode, SpatialAudioSettings,
};
pub use localization::Localization;
pub use savegame::{SaveGame, SaveValue};
pub use gameplay::{AnyBox, ComponentLookup, Deferred, NavQuery, PhysicsQuery, RayFilter, RayHit, ScreenInfo};
pub use ui::{
    UiAction, UiAlign, UiButton, UiButtonState, UiCanvas, UiFillDirection, UiImage, UiPanel, UiPauseMenu,
    UiProgressBar, UiRect, UiRenderMode, UiScaleMode, UiSlider, UiText, UiTextField, UiToggle,
};
pub use vr::{Ray, VrFrame, VrRig, pointer_ray, ray_plane};
mod net;
pub use net::{
    HOST_PEER, InterpolationMode, NetAutoStart, NetEvent, NetObjectState, NetQuality, NetRpc, NetSpawnSource, NetValue,
    NetVar, NetView, NetWorldOp, NetworkAnimator, NetworkAvatar, NetworkManager, NetworkObject, NetworkVariables,
    OwnershipMode, PeerInfo, RpcTarget, WORN_AVATAR_VAR, WornAvatar, AVATAR_SCALE_VAR, PLAYER_EYE_HEIGHT_VAR,
};
mod net_rewind;
pub use net_rewind::{HitHistory, HitPose, HitShape, MAX_REWIND, RewindHit};
mod kit;
pub use kit::*;
mod motion;
pub use motion::{MotionState, MoveHost, Predictable, as_predictable, driven_remotely, is_predictable};
mod content;
pub use content::{
    AvatarDescriptor, ContentCommand, ContentEvent, ContentInfo, ContentKind, ContentPlacement, ContentRequest,
};
pub use input::{
    ActionBinding, ActionKind, ActionValue, Bindings, ControlScheme, InputSource, InputState, Key,
    MouseAxis, MouseButton, PadAxis, PadButton, RawInput, VrAxis, VrButton, resolve,
};
pub use input_remap::{
    ActionOverride, ActionTuning, BindingConflict, BindingOverrides, BindingSlot, CaptureKind, CaptureRequest,
    CaptureResult, ConflictPolicy, ControlsInfo, ControlsRuntime, InputCapture, InputCommand, InputDevice, RebindError,
    RemapEntry, active_sources, all_sources, device_activity, stick_name,
};
pub use models::{
    AlphaModeModel, MatcapBlend, MaterialModel, MaterialTexturePaths, ShaderCmpUi, ShaderCondUi,
    ShaderGroupUi, ShaderPresetUi, ShaderPropKindUi, ShaderPropUi, ShaderUiInfo,
    TEXTURE_SLOT_LABELS,
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

impl From<ObjectId> for ObjectRef {
    fn from(id: ObjectId) -> Self {
        ObjectRef(Some(id))
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
#[derive(Debug)]
pub enum ComponentCommand {
    /// Switch to another scene (level change, menu -> game). Path is
    /// project-relative, e.g. "scenes/level2.scene". Replaces the current scene.
    LoadScene(String),
    /// Start loading a scene in the background; a later `LoadScene` /
    /// `LoadSceneAdditive` of it switches instantly once ready.
    PreloadScene(String),
    /// Add another scene's objects to the current one (streaming rooms).
    LoadSceneAdditive(String),
    /// Remove the objects an additive load brought in.
    UnloadScene(String),
    /// Carry an object (and its children) into the next scene on `LoadScene`.
    KeepOnSceneChange(ObjectId, bool),
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
    /// Hand a networked object to another peer (owner or host only).
    TransferOwnership(ObjectId, u64),
    /// Spawn on every peer: applied locally right away, then replicated.
    NetSpawn {
        source: NetSpawnSource,
        root_id: ObjectId,
        translation: Vec3,
        rotation: Quat,
        /// Owner peer (0 = host authority).
        owner: u64,
    },
    /// Despawn a networked object on every peer.
    NetDespawn(ObjectId),
    /// Write a synced variable (applied locally, then replicated).
    NetSetVar { id: ObjectId, key: String, value: NetValue },
    /// Send an RPC / network event.
    NetRpc {
        object: Option<ObjectId>,
        name: String,
        args: Vec<NetValue>,
        target: RpcTarget,
        reliable: bool,
    },
    /// Start hosting / join / leave a session.
    NetHost { port: u16 },
    NetJoin { address: String },
    NetDisconnect,
    /// Host: remove a player from the session (optionally banning its id).
    NetKick { peer: u64, reason: String, ban: bool },
    /// Host-driven scene switch for every peer.
    NetLoadScene(String),
    /// Instantiate a `.prefab` (project-relative path). The first root gets
    /// `root_id` so the caller can reference it immediately.
    SpawnPrefab {
        path: String,
        root_id: ObjectId,
        translation: Vec3,
        rotation: Quat,
    },
    /// Clone an existing object (and its children) as a new root object.
    Instantiate {
        original: ObjectId,
        new_id: ObjectId,
        translation: Vec3,
        rotation: Quat,
    },
    /// Remove an object and its children from the scene.
    Destroy(ObjectId),
    /// Enable / disable an object (disabled objects don't update, render or collide).
    SetActive(ObjectId, bool),
    /// Run `f` against the first component of type `type_id` on object `id`.
    Modify {
        id: ObjectId,
        type_id: std::any::TypeId,
        f: Deferred,
    },
    /// Broadcast a gameplay event, readable by every component next frame.
    Emit(AnyBox),
    AddForce { id: ObjectId, force: Vec3 },
    AddImpulse { id: ObjectId, impulse: Vec3 },
    AddTorque { id: ObjectId, torque: Vec3 },
    SetVelocity { id: ObjectId, linear: Vec3 },
    /// Re-bake every NavMesh Surface (after the level geometry changed).
    RebakeNavMesh,
    /// Change a joint's motor at runtime (deg/s or m/s; max force).
    SetJointMotor { id: ObjectId, speed: f32, max_force: f32 },
    /// Move a [`CharacterController`] by `delta`, sliding along obstacles.
    MoveCharacter { id: ObjectId, delta: Vec3 },
    /// Fire-and-forget sound. `position: None` plays it non-spatially.
    PlaySound {
        clip: String,
        position: Option<Vec3>,
        volume: f32,
    },
    /// (Re)start the [`AudioSource`] on an object.
    PlayAudio(ObjectId),
    StopAudio(ObjectId),
    /// Set a persistent save value (see [`SaveGame`]).
    SaveSet { key: String, value: SaveValue },
    SaveRemove(String),
    /// Write the save data to a slot file.
    SaveWrite(String),
    /// Replace the save data with a slot file's contents (next pass).
    SaveLoad(String),
    /// Switch the localization language.
    SetLanguage(String),
    SetBusVolume { bus: String, volume: f32 },
    SetBusMuted { bus: String, muted: bool },
    /// Lock + hide the OS cursor (mouse-look) or release it.
    SetCursorLocked(bool),
    /// Scale game time (0 = paused, 1 = normal). Affects `dt` and physics.
    SetTimeScale(f32),
    /// Exit the game (stops Play mode in the editor).
    Quit,
    /// Capture full-body tracker calibration now (the player holds a T-pose).
    CalibrateTrackers,
    /// Drop the tracker calibration (back to the default offsets).
    ClearTrackerCalibration,
    /// Player height measurement, body calibration and the VR body menu.
    Body(BodyCommand),
    /// Start (`Some(port)`) or stop (`None`) the OSC face-tracking receiver.
    SetOscFaceTracking(Option<u16>),
    /// User-generated content request (load / spawn / enter world / unload).
    Content(ContentCommand),
    /// Rebind / reset / capture request for the live input bindings.
    Input(InputCommand),
}

/// Per-frame update context handed to components in Play mode. Transform
/// fields are the owning object's local TRS.
pub struct ComponentCtx<'a> {
    /// Frame delta in seconds, scaled by the time scale (fixed step in `fixed_update`).
    pub dt: f32,
    /// Frame delta ignoring the time scale (UI, pause menus).
    pub unscaled_dt: f32,
    /// Seconds since engine start.
    pub time: f32,
    /// The water clock: shared by every peer in a networked game, so waves
    /// line up on all clients.
    pub water_time: f32,
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
    /// Id -> index map for `object_ids` (fast reference resolution).
    pub id_index: &'a HashMap<ObjectId, usize>,
    /// Whether each object is active (enabled with all parents enabled).
    pub active: &'a [bool],
    /// Other components in the scene, for [`ComponentCtx::get`].
    pub lookup: &'a dyn ComponentLookup,
    /// Physics queries (raycasts, velocities). `None` outside Play mode.
    pub physics: Option<&'a dyn PhysicsQuery>,
    /// Navmesh path queries. `None` outside Play mode or before the bake.
    pub nav: Option<&'a dyn NavQuery>,
    /// Events emitted last frame via [`ComponentCtx::emit`].
    pub events: &'a [AnyBox],
    /// Current time scale.
    pub time_scale: f32,
    /// Game view size, pointer, camera and UI clicks.
    pub screen: &'a ScreenInfo,
    /// Persistent save data (key-value), see `ctx.save_value`.
    pub save: &'a SaveGame,
    /// Localized string tables (`localization/*.loc`).
    pub strings: &'a Localization,
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
        self.id_index.get(&id).copied()
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
    /// Called once, the first frame the object is active in Play mode.
    fn start(&mut self, ctx: &mut ComponentCtx);
    /// Per-frame behaviour (Play mode only).
    fn update(&mut self, ctx: &mut ComponentCtx);
    /// Runs after every component finished `update` this frame.
    fn late_update(&mut self, ctx: &mut ComponentCtx);
    /// Runs once per fixed physics step, before the step.
    fn fixed_update(&mut self, ctx: &mut ComponentCtx);
    fn on_collision_enter(&mut self, ctx: &mut ComponentCtx, other: ObjectId);
    fn on_collision_exit(&mut self, ctx: &mut ComponentCtx, other: ObjectId);
    fn on_trigger_enter(&mut self, ctx: &mut ComponentCtx, other: ObjectId);
    fn on_trigger_exit(&mut self, ctx: &mut ComponentCtx, other: ObjectId);
    fn on_animation_event(&mut self, ctx: &mut ComponentCtx, name: &str);
    /// Serialize to RON.
    fn save(&self) -> String;
    /// Downcasting for engine + editor systems that read typed component data.
    fn as_any(&self) -> &dyn std::any::Any;
    /// Mutable downcasting (assigning ids, editor inspector/gizmo dispatch).
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// A component switched off with the inspector's checkbox (edit mode): it
/// keeps its place in the list, its settings and its saved data, but none of
/// its callbacks run and systems that look components up by type don't see
/// it. Turning it back on unwraps it.
pub struct Disabled(pub Box<dyn Component>);

impl Component for Disabled {
    fn type_name(&self) -> &'static str {
        self.0.type_name()
    }
    fn start(&mut self, _ctx: &mut ComponentCtx) {}
    fn update(&mut self, _ctx: &mut ComponentCtx) {}
    fn late_update(&mut self, _ctx: &mut ComponentCtx) {}
    fn fixed_update(&mut self, _ctx: &mut ComponentCtx) {}
    fn on_collision_enter(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {}
    fn on_collision_exit(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {}
    fn on_trigger_enter(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {}
    fn on_trigger_exit(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {}
    fn on_animation_event(&mut self, _ctx: &mut ComponentCtx, _name: &str) {}
    fn save(&self) -> String {
        self.0.save()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Whether `c` is switched on (not wrapped in [`Disabled`]).
pub fn component_enabled(c: &dyn Component) -> bool {
    !c.as_any().is::<Disabled>()
}

/// The component itself, whether or not it is switched off.
pub fn component_inner(c: &dyn Component) -> &dyn Component {
    match c.as_any().downcast_ref::<Disabled>() {
        Some(d) => d.0.as_ref(),
        None => c,
    }
}

/// The component itself, mutably, whether or not it is switched off.
pub fn component_inner_mut(c: &mut Box<dyn Component>) -> &mut dyn Component {
    if c.as_any().is::<Disabled>() {
        let d = c.as_any_mut().downcast_mut::<Disabled>().expect("checked above");
        return d.0.as_mut();
    }
    c.as_mut()
}

/// Switch the component in `slot` on or off in place; true when it changed.
pub fn set_component_enabled(slot: &mut Box<dyn Component>, on: bool) -> bool {
    if component_enabled(slot.as_ref()) == on {
        return false;
    }
    if on {
        let d = slot.as_any_mut().downcast_mut::<Disabled>().expect("checked above");
        let inner = std::mem::replace(&mut d.0, Box::new(Placeholder));
        *slot = inner;
    } else {
        let taken = std::mem::replace(slot, Box::new(Placeholder));
        *slot = Box::new(Disabled(taken));
    }
    true
}

/// Stands in for a slot's component for the instant it is being swapped.
#[derive(Serialize, Deserialize, Default)]
struct Placeholder;

impl TypedComponent for Placeholder {
    const NAME: &'static str = "Placeholder";
}

/// Typed component interface; blanket-implements [`Component`]. All lifecycle
/// hooks default to no-ops — override the ones you need. Inspector UI lives in
/// `citrus-editor`'s `Inspect` trait, not here.
pub trait TypedComponent: Serialize + DeserializeOwned + Default + 'static {
    const NAME: &'static str;
    /// Called once, the first frame the object is active in Play mode
    /// (Play start, or later for spawned / re-enabled objects).
    fn start(&mut self, _ctx: &mut ComponentCtx) {}
    /// Called every frame while playing.
    fn update(&mut self, _ctx: &mut ComponentCtx) {}
    /// Called every frame after all components ran `update`.
    fn late_update(&mut self, _ctx: &mut ComponentCtx) {}
    /// Called once per fixed physics step (60 Hz), before the step. Apply
    /// forces here; `ctx.dt` is the fixed step.
    fn fixed_update(&mut self, _ctx: &mut ComponentCtx) {}
    /// A solid collider on this object started touching `other`.
    fn on_collision_enter(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {}
    /// A solid collider on this object stopped touching `other`.
    fn on_collision_exit(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {}
    /// `other` entered a trigger on this object (or this object entered `other`'s trigger).
    fn on_trigger_enter(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {}
    fn on_trigger_exit(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {}
    /// An `Animator` on this object passed an animation event marker.
    fn on_animation_event(&mut self, _ctx: &mut ComponentCtx, _name: &str) {}
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
    fn fixed_update(&mut self, ctx: &mut ComponentCtx) {
        TypedComponent::fixed_update(self, ctx)
    }
    fn on_collision_enter(&mut self, ctx: &mut ComponentCtx, other: ObjectId) {
        TypedComponent::on_collision_enter(self, ctx, other)
    }
    fn on_collision_exit(&mut self, ctx: &mut ComponentCtx, other: ObjectId) {
        TypedComponent::on_collision_exit(self, ctx, other)
    }
    fn on_trigger_enter(&mut self, ctx: &mut ComponentCtx, other: ObjectId) {
        TypedComponent::on_trigger_enter(self, ctx, other)
    }
    fn on_trigger_exit(&mut self, ctx: &mut ComponentCtx, other: ObjectId) {
        TypedComponent::on_trigger_exit(self, ctx, other)
    }
    fn on_animation_event(&mut self, ctx: &mut ComponentCtx, name: &str) {
        TypedComponent::on_animation_event(self, ctx, name)
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
        registry.register::<FluxVoxelLight>();
        registry.register::<FluxVolume>();
        registry.register::<ReflectionProbe>();
        registry.register::<PlanarReflection>();
        registry.register::<AudioSource>();
        registry.register::<AudioListener>();
        registry.register::<AcousticMaterial>();
        registry.register::<AudioProbeVolume>();
        registry.register::<VolumeComponent>();
        registry.register::<BoxCollider>();
        registry.register::<SphereCollider>();
        registry.register::<CapsuleCollider>();
        registry.register::<MeshCollider>();
        registry.register::<Spin>();
        registry.register::<Bob>();
        registry.register::<LodComponent>();
        registry.register::<RigidBody>();
        registry.register::<CharacterController>();
        registry.register::<PhysicsJoint>();
        registry.register::<NavMeshSurface>();
        registry.register::<NavAgent>();
        registry.register::<ParticleEmitter>();
        registry.register::<ScenePortal>();
        registry.register::<Pawn>();
        registry.register::<SpawnPoint>();
        registry.register::<Sync>();
        registry.register::<NetworkObject>();
        registry.register::<NetworkVariables>();
        registry.register::<NetworkAnimator>();
        registry.register::<NetworkAvatar>();
        registry.register::<NetworkManager>();
        registry.register::<Animator>();
        registry.register::<HumanoidIk>();
        registry.register::<FaceTracking>();
        registry.register::<BlendShapes>();
        registry.register::<LipSync>();
        registry.register::<VoiceChat>();
        registry.register::<Water>();
        registry.register::<Buoyancy>();
        registry.register::<Waterfall>();
        registry.register::<Whirlpool>();
        registry.register::<WaterContainer>();
        registry.register::<WaveRider>();
        registry.register::<BoneCloth>();
        registry.register::<ClothColliders>();
        registry.register::<ClothWind>();
        registry.register::<MeshCloth>();
        registry.register::<UiCanvas>();
        registry.register::<UiRect>();
        registry.register::<UiPanel>();
        registry.register::<UiImage>();
        registry.register::<UiText>();
        registry.register::<UiButton>();
        registry.register::<UiToggle>();
        registry.register::<UiSlider>();
        registry.register::<UiTextField>();
        registry.register::<UiProgressBar>();
        registry.register::<UiPauseMenu>();
        registry.register::<AvatarDescriptor>();
        kit::register_kit(&mut registry);
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
    /// Layer culling mask (Unity-style): a bit per layer; this camera only
    /// renders objects whose layer bit is set. Default = all layers.
    #[serde(default = "all_layers_mask")]
    pub culling_mask: u32,
    /// Render into a texture instead of the screen (CCTV / TV / minimap). A
    /// material samples it through the texture path `camera:<object id>`. Never
    /// the main camera.
    #[serde(default)]
    pub render_texture: bool,
    /// Render-texture size in pixels.
    #[serde(default = "default_render_texture_size")]
    pub texture_size: [u32; 2],
    /// Render-texture refreshes per second; 0 = every frame. Screens nobody
    /// can see are skipped regardless.
    #[serde(default)]
    pub update_hz: f32,
    /// Transient: the inspector's "Create Screen" button (spawns a plane showing
    /// this camera's feed). Consumed by the engine.
    #[serde(skip)]
    pub create_screen: bool,
}

fn default_render_texture_size() -> [u32; 2] {
    [512, 512]
}

impl Default for CameraComponent {
    fn default() -> Self {
        Self {
            id: 0,
            fov_deg: 60.0,
            near: 0.1,
            far: 100.0,
            culling_mask: all_layers_mask(),
            render_texture: false,
            texture_size: default_render_texture_size(),
            update_hz: 0.0,
            create_screen: false,
        }
    }
}

/// Default layer mask: every layer visible / collidable.
pub fn all_layers_mask() -> u32 {
    u32::MAX
}

/// Box-projected (parallax-corrected) reflection direction — the **reference
/// implementation** the `standard.frag` reflection-probe block must match. Given a
/// surface at `world_pos` reflecting along `refl_dir`, and a probe whose box is
/// centred at `center` with half-extents `half` (and whose cubemap was captured
/// from `center`), it intersects the reflection ray with the box and re-vectors
/// from the capture centre, so a flat surface lines the reflection up with the
/// captured environment instead of treating the cube as infinitely distant.
///
/// Kept here (egui-free, glam-only) so the math is **unit-tested** independently of
/// the GPU — `cargo test -p citrus-core box_projection` verifies the algorithm, and
/// any divergence between this and the shader is a bug in one of them.
pub fn box_project_reflection(world_pos: Vec3, refl_dir: Vec3, center: Vec3, half: Vec3) -> Vec3 {
    let bmin = center - half;
    let bmax = center + half;
    let inv_r = refl_dir.recip();
    let t1 = (bmax - world_pos) * inv_r;
    let t2 = (bmin - world_pos) * inv_r;
    let tmax = t1.max(t2);
    let dist = tmax.x.min(tmax.y).min(tmax.z);
    let hit = world_pos + refl_dir * dist;
    hit - center
}

/// Number of object layers (Unity-style: a 32-bit mask, one bit per layer).
pub const NUM_LAYERS: usize = 32;

/// Project-wide layer configuration: layer names + a symmetric layer-collision
/// matrix (Unity's "Layer Collision Matrix"). Drives both physics (which layers
/// collide) and rendering (per-camera culling masks). Serialized with the scene.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayerSettings {
    /// Display name per layer index. Empty string = unnamed (shown as "Layer N").
    pub names: Vec<String>,
    /// `collision[i]` is the bitmask of layers that layer `i` collides with.
    /// Kept symmetric (`collide(a,b) == collide(b,a)`).
    pub collision: Vec<u32>,
}

impl Default for LayerSettings {
    fn default() -> Self {
        let mut names = vec![String::new(); NUM_LAYERS];
        names[0] = "Default".to_string();
        Self {
            names,
            collision: vec![u32::MAX; NUM_LAYERS],
        }
    }
}

impl LayerSettings {
    /// Whether two layers collide (defaults to true for out-of-range indices).
    pub fn collide(&self, a: u8, b: u8) -> bool {
        self.collision
            .get(a as usize)
            .is_none_or(|m| m & (1u32 << (b as u32 & 31)) != 0)
    }

    /// Set (symmetrically) whether layers `a` and `b` collide.
    pub fn set_collide(&mut self, a: u8, b: u8, on: bool) {
        for (x, y) in [(a, b), (b, a)] {
            if let Some(m) = self.collision.get_mut(x as usize) {
                let bit = 1u32 << (y as u32 & 31);
                if on {
                    *m |= bit;
                } else {
                    *m &= !bit;
                }
            }
        }
    }

    /// How many layers the editor UIs show. All 32 (`NUM_LAYERS`), CONSISTENTLY
    /// across the object layer dropdown, the camera culling mask, and the collision
    /// matrix — Unity-style (every layer is always addressable). A single source of
    /// truth so the three can never disagree (they previously showed 32 / 8 / 4).
    pub fn shown_count(&self) -> usize {
        NUM_LAYERS
    }

    /// Display name for a layer index ("Layer N" when unnamed).
    pub fn layer_name(&self, i: u8) -> String {
        self.names
            .get(i as usize)
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Layer {i}"))
    }

    /// The collision filter mask for objects on `layer` (which layers it hits).
    pub fn collision_mask(&self, layer: u8) -> u32 {
        self.collision.get(layer as usize).copied().unwrap_or(u32::MAX)
    }

    /// Repair a deserialized value that's missing entries (short vecs from an
    /// older/edited file) so indexing 0..NUM_LAYERS is always safe.
    pub fn normalized(mut self) -> Self {
        self.names.resize(NUM_LAYERS, String::new());
        self.collision.resize(NUM_LAYERS, u32::MAX);
        if self.names[0].is_empty() {
            self.names[0] = "Default".to_string();
        }
        self
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

/// How a rendered object takes part in shadows (Unity's MeshRenderer "Cast
/// Shadows"). `Off` keeps it out of every shadow map and the bake's shadow
/// rays; `ShadowsOnly` casts but is never drawn by a camera.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ShadowCasting {
    #[default]
    On,
    Off,
    TwoSided,
    ShadowsOnly,
}

impl ShadowCasting {
    pub fn label(self) -> &'static str {
        match self {
            Self::On => "On",
            Self::Off => "Off",
            Self::TwoSided => "Two Sided",
            Self::ShadowsOnly => "Shadows Only",
        }
    }
    pub const ALL: [ShadowCasting; 4] = [Self::On, Self::Off, Self::TwoSided, Self::ShadowsOnly];
    /// The default; scene files skip writing it.
    pub fn is_on(&self) -> bool {
        *self == Self::On
    }
    /// Drawn into shadow maps.
    pub fn casts(self) -> bool {
        self != Self::Off
    }
    /// Drawn by cameras (main, prepass, captures, mirrors).
    pub fn visible(self) -> bool {
        self != Self::ShadowsOnly
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

/// Level-of-detail group (Unity `LODGroup`-style): swap the object's mesh for a
/// cheaper one as it gets farther from the camera, and optionally cull past the
/// last level. Authored PER-OBJECT in the Inspector (this is a component, not a
/// tool). The renderer reads the selected level via `select_lod`.
#[derive(Clone, Serialize, Deserialize)]
pub struct LodComponent {
    /// LOD levels, ascending by `max_distance`. Level 0 = highest detail (nearest).
    pub levels: Vec<LodLevel>,
    /// Beyond this camera distance the object is culled entirely (0 = never).
    pub cull_distance: f32,
}

/// One LOD level: used while camera distance <= `max_distance`.
#[derive(Clone, Serialize, Deserialize)]
pub struct LodLevel {
    pub max_distance: f32,
    /// Project-relative model path for this level (empty = keep the base mesh).
    pub model: String,
    /// With no model: generate this level from the object's own model,
    /// keeping this share of its triangles (0 or 1 = the base mesh).
    #[serde(default)]
    pub keep: f32,
}

impl Default for LodComponent {
    fn default() -> Self {
        Self {
            levels: vec![
                LodLevel { max_distance: 15.0, model: String::new(), keep: 0.0 },
                LodLevel { max_distance: 50.0, model: String::new(), keep: 0.5 },
            ],
            cull_distance: 0.0,
        }
    }
}

impl LodComponent {
    /// Pick the LOD index for a camera distance, or `None` if culled.
    pub fn select_lod(&self, distance: f32) -> Option<usize> {
        if self.cull_distance > 0.0 && distance > self.cull_distance {
            return None;
        }
        // Past the last level: keep the coarsest one (culling is cull_distance's job).
        self.levels
            .iter()
            .position(|l| distance <= l.max_distance)
            .or(self.levels.len().checked_sub(1))
    }
}

impl TypedComponent for LodComponent {
    const NAME: &'static str = "LOD Group";
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

/// Marks an object as a FluxVoxel light source. With the FluxVoxel GI backend active,
/// the object casts `color × intensity` into the FluxVoxel voxel light volume with
/// smooth distance falloff out to `range`, from the object's (possibly moving)
/// world position. Put it on a light or an emissive prop, static OR dynamic — a
/// dynamic FluxVoxel light mixes into the same voxel grid live, so it lights up
/// static objects sharing the volume. Only objects bearing this component feed
/// FluxVoxel, so you choose exactly which lights/props participate.
#[derive(Serialize, Deserialize)]
pub struct FluxVoxelLight {
    pub color: [f32; 3],
    pub intensity: f32,
    /// World-space distance at which the contribution reaches zero.
    pub range: f32,
}

impl Default for FluxVoxelLight {
    fn default() -> Self {
        Self {
            color: [1.0, 1.0, 1.0],
            intensity: 1.0,
            range: 8.0,
        }
    }
}

impl TypedComponent for FluxVoxelLight {
    const NAME: &'static str = "FluxVoxel Light";
}

/// A FluxVoxel voxel light volume: a box (centred on the object, `size` local
/// meters) filled with a regular grid of SH-L1 light probes at `density` probes
/// per meter. FluxVoxel injects every FluxVoxel Light into these voxels (static ones
/// baked once, dynamic ones mixed live) and the standard shader reads them to
/// light static + dynamic meshes inside the box. Place one (or several) over the
/// play space; configurable size + density trade quality for memory/cost.
#[derive(Serialize, Deserialize)]
pub struct FluxVolume {
    /// Full box size in local meters (grid spans -size/2 .. +size/2).
    pub size: [f32; 3],
    /// Probes per meter along each axis.
    pub density: f32,
}

impl Default for FluxVolume {
    fn default() -> Self {
        Self {
            size: [8.0, 4.0, 8.0],
            density: 0.75,
        }
    }
}

impl FluxVolume {
    /// Probe count along each axis (>= 2 so a cell always has 8 corners),
    /// clamped so a huge/dense box can't explode the probe buffer.
    pub fn counts(&self) -> [u32; 3] {
        let mut out = [2u32; 3];
        for (i, c) in out.iter_mut().enumerate() {
            let n = (self.size[i].max(0.0) * self.density).round() as i64 + 1;
            *c = n.clamp(2, 64) as u32;
        }
        out
    }

    pub fn probe_count(&self) -> usize {
        let [x, y, z] = self.counts();
        (x * y * z) as usize
    }

    /// Voxel-grid positions in local space (x-fastest then y then z) — the
    /// points FluxVoxel injects light into.
    pub fn local_positions(&self) -> Vec<Vec3> {
        let [nx, ny, nz] = self.counts();
        let half = Vec3::from(self.size) * 0.5;
        let step = |n: u32, axis: usize| {
            if n > 1 {
                self.size[axis] / (n - 1) as f32
            } else {
                0.0
            }
        };
        let (sx, sy, sz) = (step(nx, 0), step(ny, 1), step(nz, 2));
        let mut out = Vec::with_capacity((nx * ny * nz) as usize);
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

impl TypedComponent for FluxVolume {
    const NAME: &'static str = "Flux Volume";
}

/// A precomputed reflection capture: a box zone whose surroundings are captured
/// into a roughness-prefiltered cubemap (Unreal reflection-capture / Unity
/// reflection-probe style) and sampled at runtime for specular environment
/// reflection. The object's transform places it; `size` is the box influence
/// region (used for box-projected parallax + blend between probes).
/// How a reflection probe's cubemap is produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ReflectionProbeMode {
    /// Captured live (recaptured on demand / scene changes); not saved to disk.
    #[default]
    Realtime,
    /// Baked to a `.reflprobe` sidecar and loaded from disk (static, cheapest).
    Baked,
}

#[derive(Serialize, Deserialize)]
pub struct ReflectionProbe {
    /// Full box influence size in local meters (centered on the object).
    pub size: [f32; 3],
    /// Box-center offset from the object position (world-axis meters, Unity's
    /// box offset). Decouples WHERE the cube is captured from (the object
    /// position — put it at eye height so interior objects reflect right) from
    /// WHERE the box sits (aligned to the room's walls/floor/ceiling).
    #[serde(default)]
    pub box_offset: [f32; 3],
    /// Capture-point offset from the object position (world-axis meters).
    /// Moves where the cube is rendered from without moving the object or its
    /// box (drag the capture handle in the viewport).
    #[serde(default)]
    pub capture_offset: [f32; 3],
    /// Keep the box FIXED IN WORLD SPACE while the object moves: `box_offset`
    /// auto-compensates each frame, so dragging the probe repositions only the
    /// capture point inside the room-aligned box. Off = box follows the object.
    #[serde(default)]
    pub lock_box: bool,
    /// Transient: last frame's world position, for `lock_box` compensation.
    #[serde(skip)]
    pub last_world_pos: Option<[f32; 3]>,
    /// Captured cubemap face resolution (per side): 32 … 8192.
    pub resolution: u32,
    /// Realtime (live capture) vs Baked (loaded from a `.reflprobe` sidecar).
    #[serde(default)]
    pub mode: ReflectionProbeMode,
    /// Transient: set by the inspector's "Bake This Probe" button, consumed by
    /// the engine to capture + save this probe. Never serialized.
    #[serde(skip)]
    pub bake_now: bool,
    /// Reflection strength multiplier applied when sampled.
    pub intensity: f32,
    /// Box-projected parallax correction (Unity-style): reproject the reflection
    /// ray onto the box so flat surfaces line up. Off = treat as distant (infinite).
    pub box_projection: bool,
    /// Blend-zone thickness (local meters) just inside the box edge. The probe's
    /// weight ramps 0→1 across this shell, so overlapping probes cross-fade and
    /// the edge fades to the skybox instead of popping. 0 = hard cutoff at the box.
    #[serde(default = "default_blend_distance")]
    pub blend_distance: f32,
    /// Overlap priority: higher-importance probes consume blend weight first, so a
    /// small specific probe wins over a large surrounding one where they overlap.
    #[serde(default)]
    pub importance: i32,
    /// Realtime mode: fresh cubes per second (0 = every frame).
    #[serde(default = "default_probe_update_hz")]
    pub update_hz: f32,
    /// Realtime mode: spread a capture over 7 frames (one face each, then the
    /// mip chain) instead of rendering all six faces in one frame. Cheaper per
    /// frame, but moving objects step in the reflection.
    #[serde(default)]
    pub time_slicing: bool,
    /// Realtime mode: also draw non-static objects into the cube. Off by
    /// default: box projection maps everything in the cube onto the box walls,
    /// so a moving object inside the room shows up as a ghost on far surfaces
    /// (SSR / ray-traced reflections handle moving objects instead).
    #[serde(default)]
    pub include_dynamic: bool,
}

fn default_blend_distance() -> f32 {
    1.0
}

fn default_probe_update_hz() -> f32 {
    30.0
}

impl Default for ReflectionProbe {
    fn default() -> Self {
        Self {
            size: [10.0, 6.0, 10.0],
            box_offset: [0.0; 3],
            capture_offset: [0.0; 3],
            lock_box: false,
            last_world_pos: None,
            resolution: 256,
            intensity: 1.0,
            box_projection: true,
            blend_distance: default_blend_distance(),
            importance: 0,
            update_hz: default_probe_update_hz(),
            time_slicing: false,
            include_dynamic: false,
            mode: ReflectionProbeMode::Realtime,
            bake_now: false,
        }
    }
}

impl TypedComponent for ReflectionProbe {
    const NAME: &'static str = "Reflection Probe";
}

/// A true mirror for a flat surface (floor, wall mirror): the scene is
/// re-rendered every frame from the camera reflected about the object's world
/// +Y plane and composited with per-pixel parallax — exact reflections that a
/// reflection probe (single capture point) cannot produce for mid-room
/// geometry. Each active surface costs a mirrored scene pass; at most
/// `MAX_PLANAR` (4) render per frame.
#[derive(Serialize, Deserialize)]
pub struct PlanarReflection {
    /// Roughness above which the planar reflection has fully faded back to the
    /// reflection probe/env cube (the mirror render is sharp-only).
    pub max_roughness: f32,
}

impl Default for PlanarReflection {
    fn default() -> Self {
        Self { max_roughness: 0.35 }
    }
}

impl TypedComponent for PlanarReflection {
    const NAME: &'static str = "Planar Reflection";
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
    /// Mixer bus (project Audio Mixer) this source plays through.
    #[serde(default = "default_bus")]
    pub bus: String,
    /// Spatial: render through the listener's HRTF (binaural) rather than
    /// plain stereo panning. Needs the `steam-audio` engine feature.
    #[serde(default = "default_true")]
    pub hrtf: bool,
    /// Spatial: high frequencies fade with distance.
    #[serde(default = "default_true")]
    pub air_absorption: bool,
    /// Spatial: scene geometry between source and listener muffles it.
    #[serde(default = "default_true")]
    pub occlusion: bool,
    /// 0 = omnidirectional, 0.5 = cardioid, 1 = dipole along the object's
    /// forward (-Z) axis.
    #[serde(default)]
    pub directivity: f32,
    /// How narrow the directivity lobe is (higher = tighter beam).
    #[serde(default = "default_one")]
    pub directivity_sharpness: f32,
    /// Spatial: simulate this source's own reflections (reverb that depends
    /// on where it is). Off = the listener-centric reverb, if enabled.
    #[serde(default)]
    pub reflections: bool,
    /// Spatial: sound bends around corners using baked pathing probes.
    #[serde(default)]
    pub pathing: bool,
    /// The clip is an ambisonic recording of this order (1..=3, ACN / N3D);
    /// 0 = a regular mono / stereo clip. Ambisonic clips rotate with the
    /// listener instead of being placed at the object.
    #[serde(default)]
    pub ambisonic_order: u8,
    /// Spatial: encode to ambisonics and decode, instead of direct HRTF.
    #[serde(default)]
    pub spatialize_via_ambisonics: bool,
}

fn default_bus() -> String {
    "sfx".into()
}

fn default_true() -> bool {
    true
}

fn default_one() -> f32 {
    1.0
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
            bus: default_bus(),
            hrtf: true,
            air_absorption: true,
            occlusion: true,
            directivity: 0.0,
            directivity_sharpness: 1.0,
            reflections: false,
            pathing: false,
            ambisonic_order: 0,
            spatialize_via_ambisonics: false,
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
    /// Physics material (`.physmat`, project-relative; "" = the body's or project default).
    #[serde(default)]
    pub material: String,
}

impl Default for BoxCollider {
    fn default() -> Self {
        Self {
            center: [0.0; 3],
            size: [1.0; 3],
            is_trigger: false,
            layer: 0,
            material: String::new(),
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
    /// Physics material (`.physmat`, project-relative; "" = the body's or project default).
    #[serde(default)]
    pub material: String,
}

impl Default for SphereCollider {
    fn default() -> Self {
        Self {
            center: [0.0; 3],
            radius: 0.5,
            is_trigger: false,
            layer: 0,
            material: String::new(),
        }
    }
}

impl TypedComponent for SphereCollider {
    const NAME: &'static str = "Sphere Collider";
}

/// Axis a capsule runs along (local).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ColliderAxis {
    X,
    #[default]
    Y,
    Z,
}

impl ColliderAxis {
    pub const ALL: [ColliderAxis; 3] = [ColliderAxis::X, ColliderAxis::Y, ColliderAxis::Z];
    pub fn label(self) -> &'static str {
        match self {
            ColliderAxis::X => "X",
            ColliderAxis::Y => "Y",
            ColliderAxis::Z => "Z",
        }
    }
    pub fn vector(self) -> Vec3 {
        match self {
            ColliderAxis::X => Vec3::X,
            ColliderAxis::Y => Vec3::Y,
            ColliderAxis::Z => Vec3::Z,
        }
    }
}

/// A capsule collision shape: `height` end to end (caps included) along
/// `direction`, `radius` thick.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CapsuleCollider {
    pub center: [f32; 3],
    pub radius: f32,
    pub height: f32,
    pub direction: ColliderAxis,
    pub is_trigger: bool,
    pub layer: u32,
    /// Physics material (`.physmat`, project-relative; "" = the body's or project default).
    pub material: String,
}

impl Default for CapsuleCollider {
    fn default() -> Self {
        Self { center: [0.0; 3], radius: 0.5, height: 2.0, direction: ColliderAxis::Y, is_trigger: false, layer: 0, material: String::new() }
    }
}

impl TypedComponent for CapsuleCollider {
    const NAME: &'static str = "Capsule Collider";
}

/// Collision from the object's own mesh (convex hull or exact triangles).
#[derive(Serialize, Deserialize)]
pub struct MeshCollider {
    /// One convex hull even when static (static meshes otherwise collide
    /// with their exact triangles).
    pub convex: bool,
    /// Moving bodies: split into convex pieces that follow a concave shape
    /// (a couch's seat and arms) instead of one hull around it all.
    #[serde(default)]
    pub follow_shape: bool,
    /// How closely hulls and pieces hug the mesh (more corners and pieces
    /// cost more to build and simulate).
    #[serde(default)]
    pub detail: ColliderDetail,
    /// Use collision pieces modelled with the mesh (`UCX_`, `UBX_`, `USP_`,
    /// `UCP_` objects) when it has any, instead of building from the mesh.
    #[serde(default = "default_true")]
    pub use_authored: bool,
    pub is_trigger: bool,
    pub layer: u32,
    /// Physics material (`.physmat`, project-relative; "" = the body's or project default).
    #[serde(default)]
    pub material: String,
}

/// Mesh Collider fidelity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColliderDetail {
    Low,
    #[default]
    Medium,
    High,
}

impl ColliderDetail {
    pub const ALL: [ColliderDetail; 3] = [Self::Low, Self::Medium, Self::High];
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }
}

impl Default for MeshCollider {
    fn default() -> Self {
        Self {
            convex: false,
            follow_shape: true,
            detail: ColliderDetail::Medium,
            use_authored: true,
            is_trigger: false,
            layer: 0,
            material: String::new(),
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
    /// Mass in kg (Dynamic only; 0 = from the colliders' volume and the
    /// physics material's density, else 500 kg/m^3).
    pub mass: f32,
    /// Bounciness 0..1.
    pub restitution: f32,
    pub friction: f32,
    /// Per-body gravity multiplier (0 = floats).
    pub gravity_scale: f32,
    /// Physics material (`.physmat`, project-relative). When set it replaces
    /// restitution and friction above, and its colliders inherit it.
    #[serde(default)]
    pub material: String,
    /// Engine-filled in Play: the simulated mass (kg) and collider volume
    /// (m^3), so the inspector can show the body's real density.
    #[serde(skip)]
    pub measured: Option<(f32, f32)>,
}

impl Default for RigidBody {
    fn default() -> Self {
        Self {
            kind: BodyKind::Dynamic,
            mass: 0.0,
            restitution: 0.0,
            friction: 0.5,
            gravity_scale: 1.0,
            material: String::new(),
            measured: None,
        }
    }
}

impl TypedComponent for RigidBody {
    const NAME: &'static str = "Rigid Body";
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum JointKind {
    /// Welds the two bodies together.
    Fixed,
    /// Rotation about one axis (doors, wheels, levers).
    Hinge,
    /// Free rotation about the anchor (ragdoll shoulders, chains).
    Ball,
    /// Translation along one axis (pistons, drawers).
    Slider,
    /// Pulls the anchors toward `rest_length` (soft connection).
    Spring,
    /// Keeps the anchors within `rest_length` (cables, tethers).
    Rope,
}

impl JointKind {
    pub const ALL: [JointKind; 6] = [Self::Fixed, Self::Hinge, Self::Ball, Self::Slider, Self::Spring, Self::Rope];
    pub fn label(self) -> &'static str {
        match self {
            Self::Fixed => "Fixed",
            Self::Hinge => "Hinge",
            Self::Ball => "Ball",
            Self::Slider => "Slider",
            Self::Spring => "Spring",
            Self::Rope => "Rope",
        }
    }
}

/// Connects this object's rigid body to another body (or to its start point
/// in the world when `connected` is unset).
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct PhysicsJoint {
    pub kind: JointKind,
    pub connected: ObjectRef,
    /// Anchor in this object's local space.
    pub anchor: [f32; 3],
    /// Hinge / slider axis in this object's local space.
    pub axis: [f32; 3],
    /// Hinge: degrees; slider: meters.
    pub limits_enabled: bool,
    pub limits: [f32; 2],
    /// Hinge / slider motor (deg/s or m/s).
    pub motor_enabled: bool,
    pub motor_speed: f32,
    pub motor_max_force: f32,
    /// Spring / rope length (0 = the distance at start).
    pub rest_length: f32,
    pub stiffness: f32,
    pub damping: f32,
    /// Let the two connected bodies collide with each other.
    pub collide_connected: bool,
}

impl Default for PhysicsJoint {
    fn default() -> Self {
        Self {
            kind: JointKind::Hinge,
            connected: ObjectRef::NONE,
            anchor: [0.0; 3],
            axis: [0.0, 1.0, 0.0],
            limits_enabled: false,
            limits: [-90.0, 90.0],
            motor_enabled: false,
            motor_speed: 90.0,
            motor_max_force: 100.0,
            rest_length: 0.0,
            stiffness: 50.0,
            damping: 2.0,
            collide_connected: false,
        }
    }
}

impl TypedComponent for PhysicsJoint {
    const NAME: &'static str = "Joint";
}

/// Emits camera-facing particles (smoke, sparks, magic, dust).
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct ParticleEmitter {
    /// Particles per second while playing.
    pub rate: f32,
    /// Particles emitted at once when the emitter starts.
    pub burst: u32,
    /// Seconds each particle lives [min, max].
    pub lifetime: [f32; 2],
    /// Initial speed [min, max] along the emit direction.
    pub speed: [f32; 2],
    /// Emit direction in local space and cone half-angle (degrees).
    pub direction: [f32; 3],
    pub cone_angle: f32,
    /// Spawn inside a sphere of this radius.
    pub shape_radius: f32,
    /// Multiplier on world gravity (negative floats upward).
    pub gravity: f32,
    pub drag: f32,
    pub size: [f32; 2],
    pub color_start: [f32; 4],
    pub color_end: [f32; 4],
    /// Project-relative texture (empty = soft round dot).
    pub texture: String,
    pub max_particles: u32,
    pub looping: bool,
    /// Seconds a non-looping emitter emits for.
    pub duration: f32,
    pub play_on_start: bool,
    /// Particles stay where they were emitted (else they follow the object).
    pub world_space: bool,
    #[serde(skip)]
    pub control: ParticleControl,
}

/// Gameplay requests to a running emitter (consumed by the engine).
#[derive(Clone, Copy, Debug, Default)]
pub struct ParticleControl {
    pub play: bool,
    pub stop: bool,
    pub clear: bool,
    pub emit: u32,
    /// Live particle count (filled in by the engine).
    pub alive: u32,
    pub playing: bool,
}

impl Default for ParticleEmitter {
    fn default() -> Self {
        Self {
            rate: 20.0,
            burst: 0,
            lifetime: [1.0, 2.0],
            speed: [1.0, 2.0],
            direction: [0.0, 1.0, 0.0],
            cone_angle: 20.0,
            shape_radius: 0.05,
            gravity: 0.0,
            drag: 0.0,
            size: [0.2, 0.05],
            color_start: [1.0, 0.8, 0.4, 1.0],
            color_end: [1.0, 0.2, 0.05, 0.0],
            texture: String::new(),
            max_particles: 500,
            looping: true,
            duration: 2.0,
            play_on_start: true,
            world_space: true,
            control: ParticleControl::default(),
        }
    }
}

impl TypedComponent for ParticleEmitter {
    const NAME: &'static str = "Particle Emitter";
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PortalMode {
    /// Replace the current scene (level change).
    Switch,
    /// Stream the scene in alongside the current one.
    Additive,
    /// Stream a previously added scene back out.
    Unload,
}

impl PortalMode {
    pub const ALL: [PortalMode; 3] = [Self::Switch, Self::Additive, Self::Unload];
    pub fn label(self) -> &'static str {
        match self {
            Self::Switch => "Switch scene",
            Self::Additive => "Load additive",
            Self::Unload => "Unload",
        }
    }
}

/// Seamless scene transition: preloads `scene` in the background when the
/// activator comes within `preload_distance`, then switches / streams it when
/// the activator enters this object's trigger collider (add a Box/Sphere
/// Collider with "Is trigger"). Build the transition (a fall, a corridor, a
/// crack in the wall) so the activator spends the preload time inside it.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct ScenePortal {
    pub scene: String,
    pub mode: PortalMode,
    /// Object that activates the portal (unset = any object with a Pawn).
    pub activator: ObjectRef,
    /// Start preloading within this distance (0 = as soon as play starts).
    pub preload_distance: f32,
    /// Carry the activator into the new scene (Switch mode). It respawns at a
    /// matching SpawnPoint if its Pawn has a spawn tag.
    pub keep_activator: bool,
    /// Fire once, then disable.
    pub one_shot: bool,
    #[serde(skip)]
    preloaded: bool,
    #[serde(skip)]
    fired: bool,
}

impl Default for ScenePortal {
    fn default() -> Self {
        Self {
            scene: String::new(),
            mode: PortalMode::Switch,
            activator: ObjectRef::NONE,
            preload_distance: 30.0,
            keep_activator: true,
            one_shot: true,
            preloaded: false,
            fired: false,
        }
    }
}

impl ScenePortal {
    fn activator_index(&self, ctx: &ComponentCtx) -> Option<usize> {
        match self.activator.id() {
            Some(_) => ctx.resolve(self.activator),
            None => (0..ctx.object_ids.len()).find(|&i| {
                ctx.active.get(i).copied().unwrap_or(false)
                    && ctx.lookup.component(i, std::any::TypeId::of::<Pawn>()).is_some()
            }),
        }
    }
}

impl TypedComponent for ScenePortal {
    const NAME: &'static str = "Scene Portal";

    fn update(&mut self, ctx: &mut ComponentCtx) {
        if self.preloaded || self.scene.is_empty() || self.mode == PortalMode::Unload {
            return;
        }
        let near = self.preload_distance <= 0.0
            || self.activator_index(ctx).and_then(|i| ctx.object_position(i)).is_some_and(|p| {
                p.distance(ctx.self_transform().translation) <= self.preload_distance
            });
        if near {
            ctx.preload_scene(self.scene.clone());
            self.preloaded = true;
        }
    }

    fn on_trigger_enter(&mut self, ctx: &mut ComponentCtx, other: ObjectId) {
        if self.scene.is_empty() || (self.one_shot && self.fired) {
            return;
        }
        let Some(act) = self.activator_index(ctx) else {
            return;
        };
        if ctx.object_ids.get(act) != Some(&other) {
            return;
        }
        self.fired = true;
        match self.mode {
            PortalMode::Switch => {
                if self.keep_activator {
                    ctx.keep_on_scene_change(other, true);
                }
                ctx.load_scene(self.scene.clone());
            }
            PortalMode::Additive => ctx.load_scene_additive(self.scene.clone()),
            PortalMode::Unload => ctx.unload_scene(self.scene.clone()),
        }
    }
}

/// Bakes a walkable navigation grid from the physics colliders inside its box
/// (centered on the object) when play starts. Agents path over it.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct NavMeshSurface {
    /// Box size in meters (world axes, centered on the object).
    pub size: [f32; 3],
    pub cell_size: f32,
    pub agent_radius: f32,
    pub agent_height: f32,
    /// Steepest walkable slope in degrees.
    pub max_slope: f32,
    /// Tallest step an agent can climb between cells.
    pub step_height: f32,
    /// Layers whose colliders count as ground / obstacles.
    pub layer_mask: u32,
    /// Editor: overlay the baked walkable cells in the viewport.
    #[serde(skip)]
    pub show_preview: bool,
    /// Editor: re-bake the preview.
    #[serde(skip)]
    pub rebake_preview: bool,
}

impl Default for NavMeshSurface {
    fn default() -> Self {
        Self {
            size: [40.0, 10.0, 40.0],
            cell_size: 0.25,
            agent_radius: 0.35,
            agent_height: 1.8,
            max_slope: 45.0,
            step_height: 0.4,
            layer_mask: u32::MAX,
            show_preview: false,
            rebake_preview: false,
        }
    }
}

impl TypedComponent for NavMeshSurface {
    const NAME: &'static str = "NavMesh Surface";
}

/// Walks the object along navmesh paths. Set a destination with
/// `ctx.set_destination(agent, point)`; moves through a CharacterController
/// when the object has one.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct NavAgent {
    pub speed: f32,
    /// Turn rate in degrees per second (0 = don't rotate).
    pub turn_speed: f32,
    /// Stop when this close to the destination.
    pub stopping_distance: f32,
    #[serde(skip)]
    pub state: NavAgentState,
}

#[derive(Clone, Debug, Default)]
pub struct NavAgentState {
    pub destination: Option<Vec3>,
    /// Destination changed; the engine re-plans next update.
    pub replan: bool,
    pub path: Vec<Vec3>,
    pub next: usize,
    /// No path exists to the destination.
    pub unreachable: bool,
    pub velocity: Vec3,
}

impl Default for NavAgent {
    fn default() -> Self {
        Self {
            speed: 3.5,
            turn_speed: 540.0,
            stopping_distance: 0.1,
            state: NavAgentState::default(),
        }
    }
}

impl NavAgent {
    pub fn set_destination(&mut self, target: Vec3) {
        self.state.destination = Some(target);
        self.state.replan = true;
        self.state.unreachable = false;
    }

    pub fn stop(&mut self) {
        self.state.destination = None;
        self.state.path.clear();
        self.state.velocity = Vec3::ZERO;
    }

    /// Has a destination and reached it (or can't reach it).
    pub fn arrived(&self) -> bool {
        self.state.destination.is_none() || self.state.unreachable
    }

    /// Distance left along the current path.
    pub fn remaining_distance(&self, from: Vec3) -> f32 {
        let s = &self.state;
        if s.next >= s.path.len() {
            return 0.0;
        }
        let mut d = from.distance(s.path[s.next]);
        for w in s.path[s.next..].windows(2) {
            d += w[0].distance(w[1]);
        }
        d
    }
}

impl TypedComponent for NavAgent {
    const NAME: &'static str = "Nav Agent";
}

/// Kinematic capsule that walks: moved with `ctx.move_character(delta)`, it
/// slides along walls, climbs steps and slopes, and reports `ctx.is_grounded()`.
/// Gravity is up to the caller (a [`Pawn`] on the same object applies it).
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct CharacterController {
    pub radius: f32,
    /// Total capsule height, including both caps.
    pub height: f32,
    /// Capsule center relative to the object origin (default puts the feet at the origin).
    pub center: [f32; 3],
    /// Tallest step climbed automatically (0 disables autostep).
    pub step_height: f32,
    /// Steepest walkable slope in degrees.
    pub max_slope: f32,
    /// Keep the character glued to the ground when walking down slopes/steps (meters).
    pub snap_to_ground: f32,
}

impl Default for CharacterController {
    fn default() -> Self {
        Self {
            radius: 0.35,
            height: 1.8,
            center: [0.0, 0.9, 0.0],
            step_height: 0.3,
            max_slope: 45.0,
            snap_to_ground: 0.2,
        }
    }
}

impl TypedComponent for CharacterController {
    const NAME: &'static str = "Character Controller";
}

/// Plays skeletal animation clips on this object's skinned meshes, with
/// cross-fades. Each animated object gets its own pose, so spawned copies of a
/// model animate independently. Drive it from gameplay with
/// `ctx.play_animation(target, "Run", 0.2)`, or give it an Animator Controller
/// (`.animctrl` state machine) and drive that with parameters
/// (`ctx.set_animator_trigger(target, "Jump")`). Clips may be imported model
/// clips or authored `.anim` assets, which can also animate object hierarchies.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Animator {
    /// Project-relative `.animctrl` path. Empty = play `clip` / blend trees directly.
    pub controller: String,
    /// Clip, `.anim` path or blend tree to play on start (empty = the model's first clip).
    pub clip: String,
    pub speed: f32,
    pub looping: bool,
    pub play_on_start: bool,
    /// Named blend trees, played like clips (`play("Locomotion", 0.2)`), mixing
    /// clips by float parameters (`set_float("Speed", v)`).
    pub blend_trees: Vec<BlendTree>,
    /// Named markers on clips; reaching one calls `on_animation_event` on the
    /// object's components and emits an [`AnimationEvent`].
    pub events: Vec<AnimEvent>,
    /// Move the object by the root bone's horizontal motion (through its
    /// CharacterController when present) instead of letting the clip slide.
    pub root_motion: bool,
    /// Skin on the GPU (compute). Off = CPU skinning.
    pub gpu_skinning: bool,
    /// Parameters fed every frame from movement, physics or another Animator.
    pub drivers: Vec<ParamDriver>,
    #[serde(skip)]
    pub state: AnimatorState,
}

/// Where a [`ParamDriver`] reads its value.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DriverSource {
    /// Horizontal movement speed (m/s) of the source object.
    #[default]
    Speed,
    VerticalSpeed,
    /// Signed speed along the source object's forward (-Z) axis.
    ForwardSpeed,
    /// Signed speed along the source object's right (+X) axis.
    RightSpeed,
    /// 1 while the source's Character Controller touches the ground.
    Grounded,
    /// A parameter of the source object's Animator.
    AnimatorParameter,
}

impl DriverSource {
    pub const ALL: [DriverSource; 6] = [
        Self::Speed,
        Self::VerticalSpeed,
        Self::ForwardSpeed,
        Self::RightSpeed,
        Self::Grounded,
        Self::AnimatorParameter,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Speed => "Speed",
            Self::VerticalSpeed => "Vertical speed",
            Self::ForwardSpeed => "Forward speed",
            Self::RightSpeed => "Right speed",
            Self::Grounded => "Grounded",
            Self::AnimatorParameter => "Animator parameter",
        }
    }
}

/// Links a controller parameter to a live value, so state machines react to
/// movement or to other state machines without code.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct ParamDriver {
    pub parameter: String,
    pub source: DriverSource,
    /// Source object (unset = this object).
    pub object: ObjectRef,
    /// For `AnimatorParameter`: the source Animator's parameter name.
    pub source_parameter: String,
    pub scale: f32,
}

impl Default for ParamDriver {
    fn default() -> Self {
        Self {
            parameter: String::new(),
            source: DriverSource::Speed,
            object: ObjectRef::NONE,
            source_parameter: String::new(),
            scale: 1.0,
        }
    }
}

/// Mixes clips by one (1D) or two (2D) float parameters.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct BlendTree {
    pub name: String,
    pub parameter: String,
    /// Second parameter; empty = 1D tree.
    pub parameter_y: String,
    pub motions: Vec<BlendMotion>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct BlendMotion {
    pub clip: String,
    /// Parameter value(s) where this clip has full weight (y unused in 1D).
    pub position: [f32; 2],
}

impl BlendTree {
    pub fn is_2d(&self) -> bool {
        !self.parameter_y.is_empty()
    }

    /// Per-motion weights (sum 1) for the parameter value `p`.
    pub fn weights(&self, p: [f32; 2]) -> Vec<f32> {
        let n = self.motions.len();
        let mut w = vec![0.0; n];
        if n == 0 {
            return w;
        }
        if n == 1 {
            w[0] = 1.0;
            return w;
        }
        if !self.is_2d() {
            let mut order: Vec<usize> = (0..n).collect();
            order.sort_by(|&a, &b| self.motions[a].position[0].total_cmp(&self.motions[b].position[0]));
            let x = p[0];
            let first = order[0];
            let last = order[n - 1];
            if x <= self.motions[first].position[0] {
                w[first] = 1.0;
            } else if x >= self.motions[last].position[0] {
                w[last] = 1.0;
            } else {
                for pair in order.windows(2) {
                    let (a, b) = (pair[0], pair[1]);
                    let (xa, xb) = (self.motions[a].position[0], self.motions[b].position[0]);
                    if x >= xa && x <= xb {
                        let t = if xb - xa > 1e-6 { (x - xa) / (xb - xa) } else { 0.0 };
                        w[a] = 1.0 - t;
                        w[b] = t;
                        break;
                    }
                }
            }
            return w;
        }
        // 2D: inverse-distance weighting (exact hit = that motion only).
        for (i, m) in self.motions.iter().enumerate() {
            let d = ((m.position[0] - p[0]).powi(2) + (m.position[1] - p[1]).powi(2)).sqrt();
            if d < 1e-4 {
                w.iter_mut().for_each(|v| *v = 0.0);
                w[i] = 1.0;
                return w;
            }
            w[i] = 1.0 / (d * d);
        }
        let sum: f32 = w.iter().sum();
        w.iter_mut().for_each(|v| *v /= sum);
        w
    }
}

/// A named marker at `time` seconds into `clip`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct AnimEvent {
    pub clip: String,
    pub time: f32,
    pub name: String,
}

/// Broadcast when an animation event fires (readable via `ctx.events`).
#[derive(Clone, Debug)]
pub struct AnimationEvent {
    pub object: ObjectId,
    pub name: String,
}

/// Runtime playback state of an [`Animator`] (not saved).
#[derive(Clone, Default, Debug)]
pub struct AnimatorState {
    pub current: Option<String>,
    /// Seconds into the current clip.
    pub time: f32,
    /// Clip being faded out and its time.
    pub previous: Option<(String, f32)>,
    pub fade_elapsed: f32,
    pub fade_duration: f32,
    pub playing: bool,
    pub started: bool,
    /// Length of the current clip (filled in by the engine).
    pub duration: f32,
    /// Clip names available on this object's model (filled in by the engine).
    pub clips: Vec<String>,
    /// Blend-tree parameters.
    pub params: HashMap<String, f32>,
    /// Events fired since the engine last delivered them.
    pub fired: Vec<String>,
    /// World-space root motion since it was last applied.
    pub root_delta: Vec3,
    /// Root position (skeleton space) last frame, for root-motion deltas.
    pub last_root: Option<Vec3>,
    /// Controller playback per layer (filled in by the engine).
    pub layers: Vec<LayerPlayback>,
    /// Controller path `layers` was initialised for.
    pub controller_ready: Option<String>,
    /// `play_state` requests waiting for the next evaluation.
    pub requests: Vec<StateRequest>,
    /// Controller state changes `(layer, from, to)` since the engine last delivered them.
    pub state_changes: Vec<(usize, String, String)>,
}

/// Playback of one controller layer.
#[derive(Clone, Default, Debug)]
pub struct LayerPlayback {
    /// Current state index and name.
    pub state: usize,
    pub state_name: String,
    /// Seconds into the current state (speed applied).
    pub time: f32,
    /// `time` over the state's length (keeps growing past 1 while looping).
    pub normalized_time: f32,
    /// State being faded out and its time.
    pub previous: Option<(usize, f32)>,
    pub fade_elapsed: f32,
    pub fade_duration: f32,
    /// Transition currently blending (index into the layer's transitions).
    pub transition: Option<usize>,
}

impl LayerPlayback {
    pub fn in_transition(&self) -> bool {
        self.previous.is_some() && self.fade_duration > 0.0
    }
    /// 0..1 progress of the active cross-fade (1 when none).
    pub fn transition_progress(&self) -> f32 {
        if self.in_transition() { (self.fade_elapsed / self.fade_duration).clamp(0.0, 1.0) } else { 1.0 }
    }
}

/// A forced state switch (`play_state`). `layer` None = the first layer with
/// a state of that name.
#[derive(Clone, Debug)]
pub struct StateRequest {
    pub layer: Option<usize>,
    pub state: String,
    pub fade: f32,
}

/// Broadcast when an Animator Controller layer enters a new state
/// (readable via `ctx.events::<AnimatorStateChanged>()`).
#[derive(Clone, Debug)]
pub struct AnimatorStateChanged {
    pub object: ObjectId,
    pub layer: usize,
    pub from: String,
    pub to: String,
}

impl Default for Animator {
    fn default() -> Self {
        Self {
            controller: String::new(),
            drivers: Vec::new(),
            clip: String::new(),
            speed: 1.0,
            looping: true,
            play_on_start: true,
            blend_trees: Vec::new(),
            events: Vec::new(),
            root_motion: false,
            gpu_skinning: true,
            state: AnimatorState::default(),
        }
    }
}

impl Animator {
    /// Switch to `clip`, cross-fading over `fade` seconds. A no-op when that
    /// clip is already playing. With a controller this forces the state of
    /// that name instead (see [`Animator::play_state`]).
    pub fn play(&mut self, clip: &str, fade: f32) {
        if !self.controller.is_empty() {
            self.play_state(clip, fade);
            return;
        }
        let s = &mut self.state;
        s.started = true;
        if s.playing && s.current.as_deref() == Some(clip) {
            return;
        }
        s.previous = if fade > 0.0 {
            s.current.take().map(|c| (c, s.time))
        } else {
            None
        };
        s.fade_elapsed = 0.0;
        s.fade_duration = fade.max(0.0);
        s.current = Some(clip.to_owned());
        s.time = 0.0;
        s.playing = true;
    }

    /// Set a blend-tree parameter.
    pub fn set_float(&mut self, name: &str, value: f32) {
        self.state.params.insert(name.to_owned(), value);
    }

    pub fn get_float(&self, name: &str) -> f32 {
        self.state.params.get(name).copied().unwrap_or(0.0)
    }

    pub fn set_int(&mut self, name: &str, value: i32) {
        self.set_float(name, value as f32);
    }

    pub fn get_int(&self, name: &str) -> i32 {
        self.get_float(name).round() as i32
    }

    pub fn set_bool(&mut self, name: &str, value: bool) {
        self.set_float(name, if value { 1.0 } else { 0.0 });
    }

    pub fn get_bool(&self, name: &str) -> bool {
        self.get_float(name) > 0.5
    }

    /// Set a trigger: stays set until a transition consumes it.
    pub fn set_trigger(&mut self, name: &str) {
        self.set_float(name, 1.0);
    }

    pub fn reset_trigger(&mut self, name: &str) {
        self.set_float(name, 0.0);
    }

    /// Force a controller state by name (on the first layer that has it),
    /// cross-fading over `fade` seconds.
    pub fn play_state(&mut self, state: &str, fade: f32) {
        self.state.requests.push(StateRequest { layer: None, state: state.to_owned(), fade });
    }

    pub fn play_state_on_layer(&mut self, layer: usize, state: &str, fade: f32) {
        self.state.requests.push(StateRequest { layer: Some(layer), state: state.to_owned(), fade });
    }

    /// Current controller state of a layer.
    pub fn current_state(&self, layer: usize) -> Option<&str> {
        self.state.layers.get(layer).map(|l| l.state_name.as_str())
    }

    pub fn is_in_state(&self, layer: usize, state: &str) -> bool {
        self.current_state(layer) == Some(state)
    }

    pub fn is_in_transition(&self, layer: usize) -> bool {
        self.state.layers.get(layer).is_some_and(|l| l.in_transition())
    }

    /// Normalized time of a layer's current state (1.0 = one full pass).
    pub fn state_normalized_time(&self, layer: usize) -> f32 {
        self.state.layers.get(layer).map(|l| l.normalized_time).unwrap_or(0.0)
    }

    /// Freeze on the current frame.
    pub fn stop(&mut self) {
        self.state.playing = false;
    }

    pub fn is_playing(&self, clip: &str) -> bool {
        self.state.playing && self.state.current.as_deref() == Some(clip)
    }

    /// A non-looping clip reached its end.
    pub fn finished(&self) -> bool {
        !self.looping && self.state.duration > 0.0 && self.state.time >= self.state.duration
    }
}

impl TypedComponent for Animator {
    const NAME: &'static str = "Animator";
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
/// (FP/TP) writes the child camera's pitch / spring-arm. With a
/// [`CharacterController`] on the same object it walks through the physics world
/// (walls, steps, slopes); without one it moves on a flat floor at its start height.
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
    /// The body rotation this pawn writes (None in Strategy mode, which leaves it alone).
    pub fn body_rotation(&self) -> Option<Quat> {
        (!matches!(self.mode, ControlMode::Strategy)).then(|| Quat::from_rotation_y(self.yaw.to_radians()))
    }

    /// One movement step from an input frame (`dt` is the frame's scaled
    /// delta). `view` replaces the look input with a recorded (yaw, pitch).
    /// `physical` is `Some(grounded)` when a Character Controller moves the
    /// pawn: the returned world delta goes to `move_character`. Otherwise the
    /// pawn walks a flat floor and `pos` (local translation) moves in place.
    pub fn step(&mut self, input: &InputState, dt: f32, pos: &mut Vec3, physical: Option<bool>, view: Option<(f32, f32)>) -> Option<Vec3> {
        let dt = dt.min(0.1);
        let mv = input.axis2("Move");

        match view {
            Some((yaw, pitch)) => {
                self.yaw = yaw;
                self.pitch = pitch;
            }
            None => {
                // Look: mouse delta is per-frame (no dt); analog sticks pre-scaled.
                let look = input.axis2("Look");
                self.yaw -= look.x * self.look_sensitivity;
                if matches!(self.mode, ControlMode::FirstPerson | ControlMode::ThirdPerson) {
                    self.pitch = (self.pitch - look.y * self.look_sensitivity)
                        .clamp(self.min_pitch, self.max_pitch);
                }
            }
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

        let walking = matches!(self.mode, ControlMode::FirstPerson | ControlMode::ThirdPerson);
        if let Some(grounded) = physical {
            self.grounded = grounded;
            if self.grounded && self.vel.y < 0.0 {
                self.vel.y = 0.0;
            }
        }
        if walking {
            if self.grounded && input.pressed("Jump") {
                self.vel.y = self.jump_power;
                self.grounded = false;
            }
            self.vel.y -= self.gravity * dt;
        } else {
            self.vel.y = 0.0;
        }

        if physical.is_some() {
            return Some(self.vel * dt);
        }
        *pos += self.vel * dt;
        if walking && pos.y <= self.floor_y {
            pos.y = self.floor_y;
            self.vel.y = 0.0;
            self.grounded = true;
        }
        None
    }

    /// One frame of movement through `host`: `update` runs exactly this, and
    /// so do networked prediction's replays.
    fn advance(&mut self, host: &mut dyn MoveHost, input: &InputState, dt: f32, view: Option<(f32, f32)>) {
        let physical = host.has_character().then(|| host.grounded());
        let mut pos = host.translation();
        match self.step(input, dt, &mut pos, physical, view) {
            Some(delta) => host.move_character(delta),
            None => host.set_translation(pos),
        }
        if let Some(r) = self.body_rotation() {
            host.set_rotation(r);
        }
    }

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
            if matches!(self.mode, ControlMode::FirstPerson | ControlMode::ThirdPerson) {
                ctx.set_cursor_locked(true);
            }
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
        let (input, dt) = (ctx.input, ctx.dt);
        self.advance(ctx, input, dt, None);
        // Strategy zoom on the wheel via the Look.y / wheel channel is omitted;
        // arm_length stays as authored.
        self.place_camera(ctx);
    }
}

impl Predictable for Pawn {
    fn motion(&self) -> MotionState {
        MotionState { vel: self.vel, yaw: self.yaw, pitch: self.pitch, grounded: self.grounded, floor_y: self.floor_y, ..Default::default() }
    }

    fn set_motion(&mut self, m: &MotionState, _host: &mut dyn MoveHost) {
        self.vel = m.vel;
        self.yaw = m.yaw;
        self.pitch = m.pitch;
        self.grounded = m.grounded;
        self.floor_y = m.floor_y;
    }

    fn frame_extra(&self) -> Vec<f32> {
        vec![self.yaw, self.pitch]
    }

    fn replay(&mut self, host: &mut dyn MoveHost, input: &InputState, extra: &[f32], dt: f32) {
        self.advance(host, input, dt, motion::view_of(extra));
    }

    fn locally_driven(&self) -> bool {
        self.possessed
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

#[cfg(test)]
mod component_toggle_tests {
    use super::*;

    #[test]
    fn a_component_switched_off_keeps_its_place_and_settings() {
        let mut slot: Box<dyn Component> = Box::new(CharacterController { radius: 0.5, ..Default::default() });
        assert!(component_enabled(slot.as_ref()));
        assert!(set_component_enabled(&mut slot, false));
        assert!(!component_enabled(slot.as_ref()) && slot.type_name() == "Character Controller");
        assert!(slot.as_any().downcast_ref::<CharacterController>().is_none(), "systems looking it up by type don't see it");
        assert!(component_inner(slot.as_ref()).as_any().downcast_ref::<CharacterController>().is_some_and(|c| c.radius == 0.5));
        assert!(!set_component_enabled(&mut slot, false), "already off");
        assert!(set_component_enabled(&mut slot, true));
        assert!(slot.as_any().downcast_ref::<CharacterController>().is_some_and(|c| c.radius == 0.5), "back as it was");
    }
}

#[cfg(test)]
mod layer_tests {
    use super::*;

    #[test]
    fn default_layers_all_collide_and_named() {
        let s = LayerSettings::default();
        assert_eq!(s.layer_name(0), "Default");
        assert_eq!(s.layer_name(5), "Layer 5"); // unnamed fallback
        assert!(s.collide(0, 0));
        assert!(s.collide(3, 7));
        assert_eq!(s.collision_mask(0), u32::MAX);
    }

    #[test]
    fn set_collide_is_symmetric() {
        let mut s = LayerSettings::default();
        s.set_collide(1, 2, false);
        assert!(!s.collide(1, 2));
        assert!(!s.collide(2, 1)); // symmetric
        assert!(s.collide(1, 3)); // others unaffected
        s.set_collide(1, 2, true);
        assert!(s.collide(1, 2));
        assert!(s.collide(2, 1));
    }

    #[test]
    fn box_projection_at_center_is_identity() {
        // A fragment AT the capture centre: the box-projected vector points the
        // same way as the reflection ray (just scaled to the box face).
        let c = Vec3::ZERO;
        let h = Vec3::splat(5.0);
        for r in [Vec3::X, Vec3::Y, Vec3::Z, Vec3::new(1.0, 2.0, -3.0).normalize()] {
            let out = box_project_reflection(Vec3::ZERO, r, c, h).normalize();
            assert!(
                out.dot(r) > 0.999,
                "center reflection should be unchanged: {r:?} -> {out:?}"
            );
        }
    }

    #[test]
    fn box_projection_corrects_off_center_parallax() {
        // A fragment near the +X wall reflecting straight UP should hit the box
        // ceiling AHEAD of it (still +X-leaning), not straight up — that offset is
        // exactly the parallax correction a plain infinite cube would miss.
        let c = Vec3::ZERO;
        let h = Vec3::splat(5.0);
        let p = Vec3::new(4.0, 0.0, 0.0);
        let out = box_project_reflection(p, Vec3::Y, c, h);
        // Ray exits the top at (4, 5, 0); re-vectored from centre that's (4, 5, 0).
        assert!((out - Vec3::new(4.0, 5.0, 0.0)).length() < 1e-3, "got {out:?}");
        assert!(out.x > 0.0, "parallax must keep the +X lean: {out:?}");
    }

    #[test]
    fn box_projection_hits_facing_wall() {
        // Reflecting toward +X from the centre lands on the +X wall.
        let out = box_project_reflection(Vec3::ZERO, Vec3::X, Vec3::ZERO, Vec3::splat(5.0));
        assert!((out - Vec3::new(5.0, 0.0, 0.0)).length() < 1e-3, "got {out:?}");
    }

    #[test]
    fn normalized_repairs_short_vecs() {
        let s = LayerSettings { names: vec![], collision: vec![] }.normalized();
        assert_eq!(s.names.len(), NUM_LAYERS);
        assert_eq!(s.collision.len(), NUM_LAYERS);
        assert_eq!(s.names[0], "Default");
        assert!(s.collide(10, 20)); // filled with full-collision default
    }
}

#[cfg(test)]
mod blend_tree_tests {
    use super::*;

    fn tree(ys: &str, motions: &[(&str, [f32; 2])]) -> BlendTree {
        BlendTree {
            name: "t".into(),
            parameter: "x".into(),
            parameter_y: ys.into(),
            motions: motions
                .iter()
                .map(|(c, p)| BlendMotion { clip: (*c).into(), position: *p })
                .collect(),
        }
    }

    #[test]
    fn one_d_blends_neighbours_and_clamps() {
        let t = tree("", &[("Run", [6.0, 0.0]), ("Idle", [0.0, 0.0]), ("Walk", [2.0, 0.0])]);
        assert_eq!(t.weights([1.0, 0.0]), vec![0.0, 0.5, 0.5]);
        assert_eq!(t.weights([4.0, 0.0]), vec![0.5, 0.0, 0.5]);
        assert_eq!(t.weights([-3.0, 0.0]), vec![0.0, 1.0, 0.0]);
        assert_eq!(t.weights([9.0, 0.0]), vec![1.0, 0.0, 0.0]);
    }

    #[test]
    fn two_d_weights_normalize_and_snap() {
        let t = tree("y", &[("F", [0.0, 1.0]), ("B", [0.0, -1.0]), ("L", [-1.0, 0.0]), ("R", [1.0, 0.0])]);
        let w = t.weights([0.0, 1.0]);
        assert_eq!(w, vec![1.0, 0.0, 0.0, 0.0]);
        let w = t.weights([0.5, 0.5]);
        assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert!(w[0] > w[1] && w[3] > w[2]);
    }
}
