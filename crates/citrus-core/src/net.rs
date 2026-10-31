//! Networking API (2G): the data components see and the built-in networking
//! components. The engine's `net` module owns the transport and session; this
//! side is egui-free and transport-free so plugins and game code only touch
//! [`NetView`], the [`ComponentCtx`] helpers below and these components.
//!
//! Model: one peer hosts (peer id 1, the server), others join it. Every
//! networked object has an owner peer; owner 0 means "unowned", and the host
//! is then its authority. The authority simulates the object and replicates
//! it; everyone else interpolates what it sends. When the host leaves, the
//! next member in its succession list takes over (host migration): peer ids
//! stay, `NetView::host_peer` names the new host and `HostChanged` fires.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use glam::{Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::{ComponentCommand, ComponentCtx, ObjectId, ObjectRef, TrackerTargets, TypedComponent};

/// Peer id of the session's first host (after a host migration the host is
/// whichever peer `NetView::host_peer` names).
pub const HOST_PEER: u64 = 1;

// ------------------------------------------------------------------ values

/// A small typed value carried by synced variables and RPC arguments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NetValue {
    Bool(bool),
    Int(i64),
    Float(f32),
    Vec3([f32; 3]),
    Quat([f32; 4]),
    Str(String),
    Bytes(Vec<u8>),
}

impl Default for NetValue {
    fn default() -> Self {
        NetValue::Int(0)
    }
}

impl NetValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            NetValue::Bool(b) => Some(*b),
            NetValue::Int(i) => Some(*i != 0),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            NetValue::Int(i) => Some(*i),
            NetValue::Float(f) => Some(*f as i64),
            NetValue::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }
    pub fn as_float(&self) -> Option<f32> {
        match self {
            NetValue::Float(f) => Some(*f),
            NetValue::Int(i) => Some(*i as f32),
            _ => None,
        }
    }
    pub fn as_vec3(&self) -> Option<Vec3> {
        match self {
            NetValue::Vec3(v) => Some(Vec3::from(*v)),
            _ => None,
        }
    }
    pub fn as_quat(&self) -> Option<Quat> {
        match self {
            NetValue::Quat(q) => Some(Quat::from_array(*q)),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            NetValue::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            NetValue::Bytes(b) => Some(b),
            _ => None,
        }
    }
    /// Short type name for inspectors.
    pub fn kind_label(&self) -> &'static str {
        match self {
            NetValue::Bool(_) => "Bool",
            NetValue::Int(_) => "Int",
            NetValue::Float(_) => "Float",
            NetValue::Vec3(_) => "Vec3",
            NetValue::Quat(_) => "Quat",
            NetValue::Str(_) => "String",
            NetValue::Bytes(_) => "Bytes",
        }
    }
}

impl From<bool> for NetValue {
    fn from(v: bool) -> Self {
        NetValue::Bool(v)
    }
}
impl From<i32> for NetValue {
    fn from(v: i32) -> Self {
        NetValue::Int(v as i64)
    }
}
impl From<i64> for NetValue {
    fn from(v: i64) -> Self {
        NetValue::Int(v)
    }
}
impl From<u32> for NetValue {
    fn from(v: u32) -> Self {
        NetValue::Int(v as i64)
    }
}
impl From<f32> for NetValue {
    fn from(v: f32) -> Self {
        NetValue::Float(v)
    }
}
impl From<Vec3> for NetValue {
    fn from(v: Vec3) -> Self {
        NetValue::Vec3(v.to_array())
    }
}
impl From<Quat> for NetValue {
    fn from(v: Quat) -> Self {
        NetValue::Quat(v.to_array())
    }
}
impl From<&str> for NetValue {
    fn from(v: &str) -> Self {
        NetValue::Str(v.to_owned())
    }
}
impl From<String> for NetValue {
    fn from(v: String) -> Self {
        NetValue::Str(v)
    }
}
impl From<Vec<u8>> for NetValue {
    fn from(v: Vec<u8>) -> Self {
        NetValue::Bytes(v)
    }
}

// ------------------------------------------------------------------ RPC / events

/// Who receives an RPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcTarget {
    /// Every peer, including the sender.
    All,
    /// Every peer except the sender.
    Others,
    /// The host only.
    Server,
    /// The owner of the RPC's object (the host when unowned).
    Owner,
    /// One specific peer.
    Peer(u64),
}

/// A received RPC / network event.
#[derive(Clone, Debug)]
pub struct NetRpc {
    pub from: u64,
    /// Object the RPC was sent on (None = global).
    pub object: Option<ObjectId>,
    pub name: String,
    pub args: Vec<NetValue>,
}

impl NetRpc {
    pub fn arg(&self, i: usize) -> Option<&NetValue> {
        self.args.get(i)
    }
}

/// A connected player.
#[derive(Clone, Debug, Default)]
pub struct PeerInfo {
    pub id: u64,
    pub name: String,
    /// Round-trip time to this peer as seen from the local machine (ms; 0 for self).
    pub rtt_ms: f32,
    pub is_host: bool,
    /// Round-trip jitter (ms) and packet loss (0..1, both directions) to this peer.
    pub jitter_ms: f32,
    pub loss: f32,
    /// Seconds behind the newest state this peer renders remote objects
    /// (reported by clients; lag compensation rewinds by it).
    pub view_delay: f32,
    /// The player's verified id (its Ed25519 public key, checked by the host
    /// during the handshake), and its fingerprint (8 groups of 4 hex digits).
    pub identity: Option<[u8; 32]>,
    pub fingerprint: String,
}

/// Connection quality to the host (zero on the host itself and offline).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetQuality {
    pub rtt_ms: f32,
    /// Smoothed round-trip variation (ms).
    pub jitter_ms: f32,
    /// Fraction of packets lost on the way in / out (0..1).
    pub loss_in: f32,
    pub loss_out: f32,
    /// Traffic goes through a relay server rather than a direct path.
    pub relayed: bool,
}

/// Session and replication events, delivered once via [`NetView::events`].
#[derive(Clone, Debug)]
pub enum NetEvent {
    /// The local peer finished joining (or started hosting).
    Connected { peer: u64 },
    /// The session ended (host closed, timed out, rejected, or disconnected).
    Disconnected { reason: String },
    PeerJoined { id: u64, name: String },
    PeerLeft { id: u64, reason: String },
    OwnershipChanged { object: ObjectId, owner: u64, previous: u64 },
    /// Another peer asked for an object the local peer owns (`Request` mode);
    /// answer with `ctx.transfer_ownership`.
    OwnershipRequested { object: ObjectId, by: u64 },
    OwnershipDenied { object: ObjectId },
    Spawned { object: ObjectId, owner: u64 },
    Despawned { object: ObjectId },
    VarChanged { object: ObjectId, key: String, value: NetValue, from: u64 },
    /// The host is gone and a successor is being looked for (unowned objects
    /// pause until `HostChanged`).
    HostMigrating { previous: u64 },
    /// A new host took over (`host` may be the local peer: `ctx.i_am_host()`).
    HostChanged { host: u64, previous: u64 },
}

/// What a networked spawn instantiates.
#[derive(Clone, Debug, PartialEq)]
pub enum NetSpawnSource {
    /// A project-relative `.prefab` path.
    Prefab(String),
    /// Clone of an existing scene object (e.g. a disabled template).
    Clone(ObjectId),
}

/// World changes the network asks the play session to apply (engine-internal).
#[derive(Clone, Debug)]
pub enum NetWorldOp {
    Spawn {
        source: NetSpawnSource,
        root_id: ObjectId,
        translation: Vec3,
        rotation: Quat,
        owner: u64,
    },
    Despawn(ObjectId),
    SetActive(ObjectId, bool),
    LoadScene(String),
}

// ------------------------------------------------------------------ view

/// Read-only networking view handed to components each frame. Filled by the
/// engine's network system; empty/disconnected in single-player.
#[derive(Default, Clone)]
pub struct NetView {
    pub connected: bool,
    pub is_server: bool,
    /// This machine's peer id (0 when offline).
    pub local_peer: u64,
    /// The current host's peer id (changes on host migration).
    pub host_peer: u64,
    /// The host was lost and a successor is taking over.
    pub migrating: bool,
    /// Per-object current authority (only objects that are owned appear).
    pub owners: HashMap<ObjectId, u64>,
    /// Text messages received this frame: `(from_peer, is_private, text)`.
    pub messages: Vec<(u64, bool, String)>,
    /// Everyone in the session, the local peer included.
    pub peers: Vec<PeerInfo>,
    /// Session / replication events since last frame.
    pub events: Vec<NetEvent>,
    /// RPCs received since last frame.
    pub rpcs: Vec<NetRpc>,
    /// Engine-internal: world changes the play session applies.
    pub world_ops: Vec<NetWorldOp>,
    /// Engine-internal: networked dynamic bodies another peer simulates
    /// (kinematic here), with their last known velocity.
    pub remote_bodies: HashMap<ObjectId, Vec3>,
    /// Engine-internal: bodies whose authority just came back to this peer.
    pub reclaimed_bodies: HashMap<ObjectId, Vec3>,
    /// Round-trip time to the host (ms; 0 on the host).
    pub rtt_ms: f32,
    /// Jitter, loss and route to the host.
    pub quality: NetQuality,
    /// The session clock: the host's time as this peer estimates it.
    pub clock: f64,
    /// How far behind the newest snapshots remote objects render (s).
    pub view_delay: f32,
    /// Host: recent hitbox poses for lag-compensated hit tests.
    pub rewind: Option<Arc<Mutex<crate::HitHistory>>>,
}

impl NetView {
    /// Current owner of an object, if any peer holds authority.
    pub fn owner(&self, id: ObjectId) -> Option<u64> {
        self.owners.get(&id).copied()
    }
    /// Whether the local peer holds ownership of an object.
    pub fn owns(&self, id: ObjectId) -> bool {
        self.connected && self.owner(id) == Some(self.local_peer)
    }
    /// Whether the local peer simulates an object: its owner, the host for
    /// unowned objects, and always true offline.
    pub fn has_authority(&self, id: ObjectId) -> bool {
        if !self.connected {
            return true;
        }
        match self.owner(id) {
            Some(o) => o == self.local_peer,
            None => self.is_server,
        }
    }
    pub fn peer(&self, id: u64) -> Option<&PeerInfo> {
        self.peers.iter().find(|p| p.id == id)
    }
}

// ------------------------------------------------------------------ components

/// How a [`NetworkObject`]'s ownership may change hands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipMode {
    /// Any peer can take it at any time (grab-to-own; last claim wins).
    Takeover,
    /// Granted only when unowned; otherwise the owner gets an
    /// `OwnershipRequested` event and decides.
    Request,
    /// Only the host assigns ownership.
    Fixed,
}

impl OwnershipMode {
    pub const ALL: [OwnershipMode; 3] = [Self::Takeover, Self::Request, Self::Fixed];
    pub fn label(self) -> &'static str {
        match self {
            Self::Takeover => "Takeover",
            Self::Request => "Request",
            Self::Fixed => "Host only",
        }
    }
    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// How non-authority peers show a replicated transform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterpolationMode {
    /// Snap to the newest state.
    None,
    /// Render `interp_delay` behind and interpolate between buffered snapshots.
    Interpolate,
    /// Interpolate, and extrapolate with the sent velocity when the buffer runs dry.
    Extrapolate,
}

impl InterpolationMode {
    pub const ALL: [InterpolationMode; 3] = [Self::None, Self::Interpolate, Self::Extrapolate];
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Snap",
            Self::Interpolate => "Interpolate",
            Self::Extrapolate => "Extrapolate",
        }
    }
}

/// Runtime replication state the engine writes onto a [`NetworkObject`] (not saved).
#[derive(Clone, Copy, Debug, Default)]
pub struct NetObjectState {
    pub connected: bool,
    pub owner: u64,
    pub authority: bool,
    /// Predicted owner: the smoothing offset currently added to the
    /// rendered position (removed again before the next simulation step).
    pub visual_offset: Vec3,
}

/// Marks an object as networked: a stable network identity (the object id,
/// shared by every peer that loads the scene, or the id chosen by the spawner
/// for runtime spawns), an owner, and what replicates.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkObject {
    pub ownership: OwnershipMode,
    pub sync_transform: bool,
    pub sync_scale: bool,
    /// Owner simulates the Rigid Body; other peers make it kinematic and follow.
    pub sync_rigidbody: bool,
    pub sync_parent: bool,
    pub sync_active: bool,
    /// Transform updates per second.
    pub send_rate: f32,
    pub interpolation: InterpolationMode,
    /// Seconds remote peers render behind the newest snapshot.
    pub interp_delay: f32,
    /// Skip sends while the object moved less than this (meters).
    pub position_threshold: f32,
    /// Skip sends while the object turned less than this (degrees).
    pub rotation_threshold: f32,
    /// Only replicate to players within this distance (0 = everyone).
    pub relevancy: f32,
    /// Remove the object when its owner leaves (otherwise it becomes unowned).
    pub despawn_on_owner_leave: bool,
    /// Pack positions into 24 bits per axis (1 mm, +-8 km) and rotations into 32 bits.
    pub quantize: bool,
    /// Grow the interpolation delay above `interp_delay` when the measured
    /// jitter and loss need it (and shrink back when the link calms down).
    pub adaptive_delay: bool,
    /// Server-authoritative movement with client-side prediction, for a
    /// [`crate::Pawn`], First / Third Person Controller or VR Player (any
    /// [`crate::Predictable`]): the owner moves at once from its own input
    /// and sends the inputs; the host simulates the controller from them and
    /// replies with the authoritative state, which the owner reconciles with
    /// (rewind + replay, smoothed). Everyone else interpolates the host's
    /// state.
    pub predicted: bool,
    #[serde(skip)]
    pub state: NetObjectState,
}

impl Default for NetworkObject {
    fn default() -> Self {
        Self {
            ownership: OwnershipMode::Takeover,
            sync_transform: true,
            sync_scale: false,
            sync_rigidbody: true,
            sync_parent: true,
            sync_active: true,
            send_rate: 20.0,
            interpolation: InterpolationMode::Interpolate,
            interp_delay: 0.1,
            position_threshold: 0.001,
            rotation_threshold: 0.1,
            relevancy: 0.0,
            despawn_on_owner_leave: false,
            quantize: true,
            adaptive_delay: true,
            predicted: false,
            state: NetObjectState::default(),
        }
    }
}

impl NetworkObject {
    /// The settings a legacy [`crate::Sync`] maps to.
    pub fn from_sync(smoothing: f32) -> Self {
        Self {
            interpolation: if smoothing > 0.0 { InterpolationMode::Interpolate } else { InterpolationMode::None },
            ..Default::default()
        }
    }
}

impl TypedComponent for NetworkObject {
    const NAME: &'static str = "Network Object";
}

/// One named synced variable (authored default; live value at runtime).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct NetVar {
    pub name: String,
    pub value: NetValue,
}

/// Typed key/value state replicated with owner-write semantics: the object's
/// authority writes (`ctx.set_net_var`), the host validates and relays, every
/// peer sees the value and a `VarChanged` event. Late joiners get the current values.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct NetworkVariables {
    pub vars: Vec<NetVar>,
    /// Let any peer write (host still relays), not just the authority.
    pub anyone_can_write: bool,
}


impl NetworkVariables {
    pub fn get(&self, key: &str) -> Option<&NetValue> {
        self.vars.iter().find(|v| v.name == key).map(|v| &v.value)
    }
    pub fn set(&mut self, key: &str, value: NetValue) {
        match self.vars.iter_mut().find(|v| v.name == key) {
            Some(v) => v.value = value,
            None => self.vars.push(NetVar { name: key.to_owned(), value }),
        }
    }
}

impl TypedComponent for NetworkVariables {
    const NAME: &'static str = "Network Variables";
}

/// Replicates the object's Animator parameters from its authority.
/// Triggers replicate as their float value; prefer a bool or an RPC when a
/// one-frame trigger must never be missed.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkAnimator {
    /// Parameters to sync (empty = every parameter).
    pub parameters: Vec<String>,
    pub send_rate: f32,
}

impl Default for NetworkAnimator {
    fn default() -> Self {
        Self { parameters: Vec::new(), send_rate: 15.0 }
    }
}

impl TypedComponent for NetworkAnimator {
    const NAME: &'static str = "Network Animator";
}

/// VR / humanoid pose replication. The authority sends the world poses of the
/// tracked part objects (head, hands, optional hips/feet); other peers write
/// them back and expose them as [`TrackerTargets`] for IK. With
/// `drive_from_vr`, the owner's parts follow the local headset and controllers
/// (put this on the play-space root; parts are its direct children).
///
/// For a Network Manager player prefab, use `follow_local_player` instead:
/// the owner's copy tracks the scene's [`crate::VrPlayer`] rig (head and
/// hands, VR or desktop fallback) and hides `owner_hidden` locally, so only
/// the other peers see it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkAvatar {
    pub head: ObjectRef,
    pub left_hand: ObjectRef,
    pub right_hand: ObjectRef,
    pub hips: ObjectRef,
    pub left_foot: ObjectRef,
    pub right_foot: ObjectRef,
    pub send_rate: f32,
    pub drive_from_vr: bool,
    /// Owner: mirror the local VR Player rig (root, head, hands).
    pub follow_local_player: bool,
    /// Objects disabled on the owner's peer (its own avatar visuals).
    pub owner_hidden: Vec<ObjectRef>,
    /// Latest tracker targets (sent or received), world space. Covers every
    /// tracker role, not only the part objects.
    #[serde(skip)]
    pub targets: TrackerTargets,
    /// Cached VR Player rig (follow mode).
    #[serde(skip)]
    pub player: Option<ObjectId>,
    #[serde(skip)]
    pub hidden_applied: bool,
    /// Another peer owns this avatar (set by the network sync).
    #[serde(skip)]
    pub remote: bool,
    /// The content package this player wears (set when an avatar package is
    /// worn locally, or received for a remote player).
    #[serde(skip)]
    pub wearing: Option<WornAvatar>,
    /// What the owner last announced.
    #[serde(skip)]
    pub wearing_sent: Option<WornAvatar>,
    /// Face tracking: sent by the owner, received on remote copies.
    #[serde(skip)]
    pub face: Option<crate::FaceFrame>,
    /// The player's avatar scale over its authored size (sent by the owner,
    /// applied to remote copies of the avatar).
    #[serde(skip)]
    pub avatar_scale: f32,
    /// The player's standing eye height (m), as the owner measured it.
    #[serde(skip)]
    pub player_eye_height: Option<f32>,
    /// What the owner last announced (scale, eye height).
    #[serde(skip)]
    pub body_sent: Option<(f32, f32)>,
}

/// A worn avatar package as replicated: its content id and, when it came from
/// one, the URL other players can download it from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WornAvatar {
    pub id: String,
    pub url: String,
}

/// Synced-variable key a Network Avatar's worn package travels under.
pub const WORN_AVATAR_VAR: &str = "citrus.avatar";
/// Synced-variable keys of the player's avatar scale and eye height.
pub const AVATAR_SCALE_VAR: &str = "citrus.avatar_scale";
pub const PLAYER_EYE_HEIGHT_VAR: &str = "citrus.eye_height";

impl WornAvatar {
    pub fn encode(&self) -> String {
        format!("{}\n{}", self.id, self.url)
    }

    pub fn decode(s: &str) -> Option<Self> {
        let (id, url) = s.split_once('\n').unwrap_or((s, ""));
        (!id.is_empty()).then(|| Self { id: id.to_owned(), url: url.to_owned() })
    }

    /// What a peer passes to the content loader: the URL, else the id (works
    /// when the package is already in that peer's cache).
    pub fn source(&self) -> &str {
        if self.url.is_empty() { &self.id } else { &self.url }
    }
}

impl Default for NetworkAvatar {
    fn default() -> Self {
        Self {
            head: ObjectRef::NONE,
            left_hand: ObjectRef::NONE,
            right_hand: ObjectRef::NONE,
            hips: ObjectRef::NONE,
            left_foot: ObjectRef::NONE,
            right_foot: ObjectRef::NONE,
            send_rate: 30.0,
            drive_from_vr: true,
            follow_local_player: false,
            owner_hidden: Vec::new(),
            targets: TrackerTargets::default(),
            player: None,
            hidden_applied: false,
            remote: false,
            wearing: None,
            wearing_sent: None,
            face: None,
            avatar_scale: 1.0,
            player_eye_height: None,
            body_sent: None,
        }
    }
}

impl NetworkAvatar {
    /// Part references in wire order.
    pub fn parts(&self) -> [ObjectRef; 6] {
        [self.head, self.left_hand, self.right_hand, self.hips, self.left_foot, self.right_foot]
    }

    /// The scene's VR Player rig (cached by id, rescanned when it goes away).
    fn local_player(&mut self, ctx: &ComponentCtx) -> Option<usize> {
        if let Some(i) = self.player.and_then(|id| ctx.index_of(id)) {
            return Some(i);
        }
        let ty = std::any::TypeId::of::<crate::VrPlayer>();
        let i = (0..ctx.object_ids.len()).find(|&i| i != ctx.self_index && ctx.lookup.component(i, ty).is_some())?;
        self.player = ctx.object_ids.get(i).copied();
        Some(i)
    }

    /// Owner side of `follow_local_player`: sit on the rig root and copy the
    /// rig's head / hand poses (relative to its root) onto our parts.
    fn follow_player(&mut self, ctx: &mut ComponentCtx) {
        if !self.hidden_applied {
            self.hidden_applied = true;
            for r in self.owner_hidden.clone() {
                ctx.set_active(r, false);
            }
        }
        let Some(pi) = self.local_player(ctx) else {
            return;
        };
        let Some(root) = ctx.object_matrix(pi) else {
            return;
        };
        let rig = ctx.lookup.component(pi, std::any::TypeId::of::<crate::VrPlayer>())
            .and_then(|c| c.as_any().downcast_ref::<crate::VrPlayer>())
            .map(|p| [p.head, p.left_hand, p.right_hand]);
        let Some(rig) = rig else {
            return;
        };
        let (scale, rot, pos) = root.to_scale_rotation_translation();
        ctx.set_world_position(pos);
        *ctx.rotation = match ctx.parent_world {
            Some(p) => (p.to_scale_rotation_translation().1.inverse() * rot).normalize(),
            None => rot,
        };
        *ctx.scale = scale;
        let inv = root.inverse();
        for (part, src) in [self.head, self.left_hand, self.right_hand].into_iter().zip(rig) {
            let Some(world) = ctx.resolve(src).and_then(|i| ctx.object_matrix(i)) else {
                continue;
            };
            if part.is_set() {
                let (_, r, t) = (inv * world).to_scale_rotation_translation();
                ctx.set_local_transform(part, Some(t), Some(r), None);
            }
        }
    }
}

impl TypedComponent for NetworkAvatar {
    const NAME: &'static str = "Network Avatar";

    fn update(&mut self, ctx: &mut ComponentCtx) {
        if !ctx.has_authority() {
            return;
        }
        if self.follow_local_player {
            self.follow_player(ctx);
            return;
        }
        if !self.drive_from_vr || !ctx.screen.vr.active {
            return;
        }
        // VR poses are stage-space, i.e. local to the rig root this sits on.
        let vr = ctx.screen.vr;
        for (part, pose) in [(self.head, vr.head), (self.left_hand, vr.left_hand), (self.right_hand, vr.right_hand)] {
            if let (true, Some((p, r))) = (part.is_set(), pose) {
                ctx.set_local_transform(part, Some(p), Some(r), None);
            }
        }
    }
}

/// When a built game starts networking on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetAutoStart {
    Off,
    Host,
    Join,
}

impl NetAutoStart {
    pub const ALL: [NetAutoStart; 3] = [Self::Off, Self::Host, Self::Join];
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Host => "Host",
            Self::Join => "Join",
        }
    }
}

/// Session configuration for a scene: limits, the player prefab spawned for
/// each peer, and optional auto-start for built games. The editor Network
/// window and `ctx.net_host` read it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkManager {
    pub session_name: String,
    pub port: u16,
    pub max_players: u32,
    /// Seconds without traffic before a peer counts as gone.
    pub timeout: f32,
    /// Answer LAN discovery queries while hosting.
    pub discovery: bool,
    /// Spawned (owned by the peer) for every player that joins. Empty = none.
    pub player_prefab: String,
    /// SpawnPoint tag player prefabs spawn at (round-robin).
    pub spawn_tag: String,
    /// The host doesn't get a player of its own (dedicated server).
    pub dedicated: bool,
    pub clients_can_spawn: bool,
    pub auto_start: NetAutoStart,
    /// Address `Join` auto-start connects to: `host:port`, or a room code
    /// (`ABC123`, via `relay`, or `ABC123@relay.example.com:47800`).
    pub address: String,
    /// Relay server (`host:port`) for internet play without port forwarding.
    /// Hosting registers a room there too; empty = LAN / direct only.
    pub relay: String,
    /// Room code a host asks the relay for (empty = the relay picks one).
    pub room_code: String,
    /// Join secret: peers must know it to join, and it keys the session's
    /// encryption. Empty = anyone who reaches the session can join.
    pub secret: String,
    /// When the host leaves, the next player in line takes over and the
    /// session goes on (off = the session ends with its host).
    pub host_migration: bool,
    /// Clients remember the host's identity for this address / room code
    /// and refuse a different one later (trust on first use).
    pub pin_host: bool,
    /// Only these player fingerprints may join (empty = anyone with the secret).
    pub allow_list: Vec<String>,
    /// Player fingerprints refused at the door.
    pub ban_list: Vec<String>,
    /// Optional account logins (`name:password`, one per line). Empty keeps
    /// identity+secret-only auth.
    pub account_list: Vec<String>,
}

impl Default for NetworkManager {
    fn default() -> Self {
        Self {
            session_name: "Citrus Session".to_string(),
            port: 9000,
            max_players: 16,
            timeout: 10.0,
            discovery: true,
            player_prefab: String::new(),
            spawn_tag: "player".to_string(),
            dedicated: false,
            clients_can_spawn: true,
            auto_start: NetAutoStart::Off,
            address: "127.0.0.1:9000".to_string(),
            relay: String::new(),
            room_code: String::new(),
            secret: String::new(),
            host_migration: true,
            pin_host: false,
            allow_list: Vec::new(),
            ban_list: Vec::new(),
            account_list: Vec::new(),
        }
    }
}

impl TypedComponent for NetworkManager {
    const NAME: &'static str = "Network Manager";
}

// ------------------------------------------------------------------ ctx API

impl ComponentCtx<'_> {
    /// Hosting or joined (false in single-player).
    pub fn net_connected(&self) -> bool {
        self.net.connected
    }
    pub fn is_server(&self) -> bool {
        self.net.connected && self.net.is_server
    }
    pub fn is_client(&self) -> bool {
        self.net.connected && !self.net.is_server
    }
    /// This machine's peer id (0 offline).
    pub fn local_peer(&self) -> u64 {
        self.net.local_peer
    }
    /// The current host's peer id (0 offline; changes on host migration).
    pub fn net_host_peer(&self) -> u64 {
        if self.net.connected { self.net.host_peer } else { 0 }
    }
    /// This peer hosts the session (also right after taking over as host).
    pub fn i_am_host(&self) -> bool {
        self.is_server()
    }
    /// The new host's peer id when the host changed since last frame.
    pub fn host_changed(&self) -> Option<u64> {
        self.net.events.iter().rev().find_map(|e| match e {
            NetEvent::HostChanged { host, .. } => Some(*host),
            _ => None,
        })
    }
    /// The host was lost and a successor is taking over.
    pub fn net_migrating(&self) -> bool {
        self.net.connected && self.net.migrating
    }
    /// A peer's verified player id (Ed25519 public key).
    pub fn peer_identity(&self, id: u64) -> Option<[u8; 32]> {
        self.net.peer(id).and_then(|p| p.identity)
    }
    /// A peer's player-id fingerprint (what allow / ban lists hold).
    pub fn peer_fingerprint(&self, id: u64) -> Option<&str> {
        self.net.peer(id).map(|p| p.fingerprint.as_str()).filter(|f| !f.is_empty())
    }
    /// Host: remove a player (`ban` also refuses its player id from now on).
    pub fn net_kick(&mut self, peer: u64, reason: impl Into<String>, ban: bool) {
        self.commands.push(ComponentCommand::NetKick { peer, reason: reason.into(), ban });
    }
    /// Everyone in the session, the local peer included.
    pub fn peers(&self) -> &[PeerInfo] {
        &self.net.peers
    }
    pub fn peer_name(&self, id: u64) -> Option<&str> {
        self.net.peer(id).map(|p| p.name.as_str())
    }
    /// Session / replication events since last frame.
    pub fn net_events(&self) -> &[NetEvent] {
        &self.net.events
    }
    /// Round-trip time to the host in milliseconds (0 on the host and offline).
    pub fn net_rtt_ms(&self) -> f32 {
        self.net.rtt_ms
    }
    /// Round-trip time to the host in seconds.
    pub fn net_rtt(&self) -> f32 {
        self.net.rtt_ms / 1000.0
    }
    /// Jitter, packet loss and route to the host.
    pub fn net_quality(&self) -> NetQuality {
        self.net.quality
    }

    /// Whether the local peer simulates the owning object (its owner, the
    /// host for unowned objects, always true offline). Gate gameplay that
    /// moves a networked object on this.
    pub fn has_authority(&self) -> bool {
        self.net.has_authority(self.self_id())
    }
    pub fn has_authority_of(&self, target: impl Into<ObjectRef>) -> bool {
        target.into().id().is_some_and(|id| self.net.has_authority(id))
    }
    /// Owner peer of an object (None = unowned).
    pub fn owner_of(&self, target: impl Into<ObjectRef>) -> Option<u64> {
        self.net.owner(target.into().id()?)
    }
    pub fn release_ownership_of(&mut self, target: impl Into<ObjectRef>) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::ReleaseOwnership(id));
        }
    }
    /// Hand an object to another peer (owner or host only).
    pub fn transfer_ownership(&mut self, target: impl Into<ObjectRef>, peer: u64) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::TransferOwnership(id, peer));
        }
    }

    /// Spawn a prefab on every peer, owned by the local peer. Returns its
    /// network id (= object id) right away.
    pub fn net_spawn(&mut self, prefab: impl Into<String>, translation: Vec3, rotation: Quat) -> ObjectId {
        let owner = self.net.local_peer;
        self.net_spawn_with(NetSpawnSource::Prefab(prefab.into()), translation, rotation, owner)
    }
    /// Spawn a prefab on every peer with the host as authority (unowned).
    pub fn net_spawn_server(&mut self, prefab: impl Into<String>, translation: Vec3, rotation: Quat) -> ObjectId {
        self.net_spawn_with(NetSpawnSource::Prefab(prefab.into()), translation, rotation, 0)
    }
    /// Clone a scene object on every peer (a disabled template works well).
    pub fn net_clone(&mut self, original: impl Into<ObjectRef>, translation: Vec3, rotation: Quat) -> Option<ObjectId> {
        let id = original.into().id()?;
        let owner = self.net.local_peer;
        Some(self.net_spawn_with(NetSpawnSource::Clone(id), translation, rotation, owner))
    }
    /// Networked spawn with an explicit source and owner (0 = host authority).
    pub fn net_spawn_with(&mut self, source: NetSpawnSource, translation: Vec3, rotation: Quat, owner: u64) -> ObjectId {
        let root_id = ObjectId::new();
        self.commands.push(ComponentCommand::NetSpawn { source, root_id, translation, rotation, owner });
        root_id
    }
    /// Remove a networked object on every peer (authority only).
    pub fn net_despawn(&mut self, target: impl Into<ObjectRef>) {
        if let Some(id) = target.into().id()
            && self.net.has_authority(id)
        {
            self.commands.push(ComponentCommand::NetDespawn(id));
        }
    }

    /// A synced variable on an object's [`NetworkVariables`].
    pub fn net_var(&self, target: impl Into<ObjectRef>, key: &str) -> Option<&NetValue> {
        self.get::<NetworkVariables>(target)?.get(key)
    }
    pub fn net_var_self(&self, key: &str) -> Option<&NetValue> {
        self.get_self::<NetworkVariables>()?.get(key)
    }
    /// Write a synced variable. Applied locally and replicated when the local
    /// peer is the object's authority (or the component allows anyone);
    /// ignored otherwise. Returns whether the write was accepted locally.
    pub fn set_net_var(&mut self, target: impl Into<ObjectRef>, key: &str, value: impl Into<NetValue>) -> bool {
        let Some(id) = target.into().id() else {
            return false;
        };
        let anyone = self.get::<NetworkVariables>(id).is_some_and(|v| v.anyone_can_write);
        if !anyone && !self.net.has_authority(id) {
            return false;
        }
        self.commands.push(ComponentCommand::NetSetVar { id, key: key.to_owned(), value: value.into() });
        true
    }
    pub fn set_net_var_self(&mut self, key: &str, value: impl Into<NetValue>) -> bool {
        let id = self.self_id();
        self.set_net_var(id, key, value)
    }
    /// `VarChanged` events since last frame (any object).
    pub fn net_var_changes(&self) -> impl Iterator<Item = (ObjectId, &str, &NetValue, u64)> {
        self.net.events.iter().filter_map(|e| match e {
            NetEvent::VarChanged { object, key, value, from } => Some((*object, key.as_str(), value, *from)),
            _ => None,
        })
    }

    /// Send a reliable RPC (global, no object).
    pub fn rpc(&mut self, name: &str, args: Vec<NetValue>, target: RpcTarget) {
        self.send_rpc(None, name, args, target, true);
    }
    /// Send an unreliable RPC (may be dropped; for frequent cosmetic events).
    pub fn rpc_unreliable(&mut self, name: &str, args: Vec<NetValue>, target: RpcTarget) {
        self.send_rpc(None, name, args, target, false);
    }
    /// Send a reliable RPC on the owning object (receivers filter with `rpcs_for_self`).
    pub fn rpc_self(&mut self, name: &str, args: Vec<NetValue>, target: RpcTarget) {
        let id = self.self_id();
        self.send_rpc(Some(id), name, args, target, true);
    }
    pub fn send_rpc(&mut self, object: Option<ObjectId>, name: &str, args: Vec<NetValue>, target: RpcTarget, reliable: bool) {
        if !self.net.connected {
            // Offline: targets that include the local player loop back as a local event.
            let local = match target {
                RpcTarget::All | RpcTarget::Server | RpcTarget::Owner => true,
                RpcTarget::Others => false,
                RpcTarget::Peer(p) => p == 0 || p == self.net.local_peer,
            };
            if local {
                let rpc = NetRpc { from: 0, object, name: name.to_owned(), args };
                self.commands.push(ComponentCommand::Emit(crate::AnyBox(Box::new(rpc))));
            }
            return;
        }
        self.commands.push(ComponentCommand::NetRpc { object, name: name.to_owned(), args, target, reliable });
    }
    /// RPCs received since last frame (offline loopback included).
    pub fn rpcs(&self) -> impl Iterator<Item = &NetRpc> {
        self.net.rpcs.iter().chain(self.events::<NetRpc>())
    }
    pub fn rpcs_named<'s>(&'s self, name: &'s str) -> impl Iterator<Item = &'s NetRpc> + 's {
        self.rpcs().filter(move |r| r.name == name)
    }
    /// RPCs sent on the owning object.
    pub fn rpcs_for_self(&self) -> impl Iterator<Item = &NetRpc> {
        let me = self.self_id();
        self.rpcs().filter(move |r| r.object == Some(me))
    }

    /// Start hosting (uses the scene's Network Manager settings; port 0 = its port).
    pub fn net_host(&mut self, port: u16) {
        self.commands.push(ComponentCommand::NetHost { port });
    }
    pub fn net_join(&mut self, address: impl Into<String>) {
        self.commands.push(ComponentCommand::NetJoin { address: address.into() });
    }
    pub fn net_disconnect(&mut self) {
        self.commands.push(ComponentCommand::NetDisconnect);
    }
    /// Host: switch every peer (and late joiners) to a scene. Offline: a plain load.
    pub fn net_load_scene(&mut self, path: impl Into<String>) {
        self.commands.push(ComponentCommand::NetLoadScene(path.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_rules() {
        let a = ObjectId::new();
        let b = ObjectId::new();
        let mut v = NetView::default();
        assert!(v.has_authority(a), "offline peers own everything");
        v.connected = true;
        v.local_peer = 2;
        v.owners.insert(a, 2);
        assert!(v.has_authority(a));
        assert!(!v.has_authority(b), "unowned objects belong to the host");
        v.is_server = true;
        v.local_peer = 1;
        assert!(v.has_authority(b));
        assert!(!v.has_authority(a));
    }

    #[test]
    fn variables_and_values() {
        let mut vars = NetworkVariables::default();
        vars.set("hp", 100.into());
        vars.set("hp", 90.into());
        assert_eq!(vars.vars.len(), 1);
        assert_eq!(vars.get("hp").and_then(NetValue::as_int), Some(90));
        let s = ron::to_string(&vars).unwrap();
        let back: NetworkVariables = ron::from_str(&s).unwrap();
        assert_eq!(back.get("hp"), Some(&NetValue::Int(90)));
        let no: NetworkObject = ron::from_str("(send_rate: 5.0)").unwrap();
        assert_eq!(no.send_rate, 5.0);
        assert!(no.sync_transform, "missing fields default");
    }
}
