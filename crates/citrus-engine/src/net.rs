//! Built-in networking (2G): transport, connections and the replicated
//! session registry.
//!
//! Topology: host-authoritative star. One peer hosts (peer id 1, either a
//! player-host or a dedicated server); clients join it and every packet goes
//! through the host, which validates writes (only an object's owner may move
//! it, write its variables or despawn it) and relays them. One code path
//! serves client-server games and "P2P-feel" social worlds on a LAN or over a
//! forwarded port.
//!
//! Transport: UDP (or an in-memory hub for tests) with two channels per
//! connection. Unreliable packets carry snapshots, avatar poses, animator
//! parameters, voice and unreliable RPCs. A reliable ordered channel
//! (sequence numbers, batched acks, RTT-based resend, fragmentation of large
//! messages) carries everything that must arrive: joins/leaves, ownership,
//! spawns, variables, RPCs, chat, scene changes and late-join catch-up.
//! Connections handshake (HELLO / WELCOME / REJECT), ping for RTT and time out
//! after `NetConfig::timeout` of silence.
//!
//! Scene-level replication (which objects to send, interpolation, physics
//! hand-over) lives in `net_sync`; this module only moves state around.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use citrus_core::{
    HOST_PEER, InterpolationMode, NetEvent, NetRpc, NetSpawnSource, NetValue, NetView, NetWorldOp, NetworkManager,
    ObjectId, OwnershipMode, PeerInfo, RpcTarget, TRACKER_SLOTS,
};
use glam::{Quat, Vec3};

/// Bumped whenever the wire format changes; mismatched peers are rejected.
pub const PROTOCOL: u16 = 4;
/// UDP port LAN discovery queries go to.
pub const DISCOVERY_PORT: u16 = 47777;
/// Largest datagram we build (stays under common path MTUs).
pub const MTU: usize = 1200;
const FRAG: usize = MTU - 16;
const RECV_BUF: usize = 2048;
const PING_INTERVAL: f64 = 0.5;
const HELLO_INTERVAL: f64 = 0.25;
const MAX_QUEUE: usize = 4096;
/// Pending world writes (variables, parents) wait this long for their object.
const PENDING_TTL: f64 = 10.0;
/// Snapshots kept per object.
const SNAP_KEEP: usize = 32;

// Packet tags.
const P_HELLO: u8 = 1;
const P_WELCOME: u8 = 2;
const P_REJECT: u8 = 3;
const P_PING: u8 = 4;
const P_PONG: u8 = 5;
const P_BYE: u8 = 6;
const P_REL: u8 = 7;
const P_ACK: u8 = 8;
const P_SNAP: u8 = 9;
const P_AVATAR: u8 = 10;
const P_ANIM: u8 = 11;
const P_VOICE: u8 = 12;
const P_URPC: u8 = 13;
const P_DISCOVER: u8 = 14;
const P_INFO: u8 = 15;
const P_FACE: u8 = 16;

// Reliable message tags.
const M_PEERS: u8 = 1;
const M_JOINED: u8 = 2;
const M_LEFT: u8 = 3;
const M_OWNER: u8 = 4;
const M_CLAIM: u8 = 5;
const M_RELEASE: u8 = 6;
const M_TRANSFER: u8 = 7;
const M_OWNER_REQ: u8 = 8;
const M_DENY: u8 = 9;
const M_SPAWN: u8 = 10;
const M_DESPAWN: u8 = 11;
const M_VAR: u8 = 12;
const M_ACTIVE: u8 = 13;
const M_PARENT: u8 = 14;
const M_RPC: u8 = 15;
const M_TEXT: u8 = 16;
const M_SCENE: u8 = 17;
const M_XFORM: u8 = 18;
const M_ANIM: u8 = 19;

// ------------------------------------------------------------------ transport

/// A datagram transport. UDP in games; [`MemHub`] endpoints in tests.
pub trait Transport {
    fn send(&self, data: &[u8], to: SocketAddr);
    fn recv(&mut self, buf: &mut [u8]) -> Option<(usize, SocketAddr)>;
    fn local_addr(&self) -> Option<SocketAddr>;
}

pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    pub fn bind(port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket })
    }
}

impl Transport for UdpTransport {
    fn send(&self, data: &[u8], to: SocketAddr) {
        let _ = self.socket.send_to(data, to);
    }
    fn recv(&mut self, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
        loop {
            match self.socket.recv_from(buf) {
                Ok(v) => return Some(v),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
                // ICMP port-unreachable surfaces as a reset on some platforms.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(e) => {
                    tracing::debug!("net recv error: {e}");
                    return None;
                }
            }
        }
    }
    fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }
}

/// In-process packet hub: endpoints exchange datagrams through shared queues,
/// with optional random loss and severed links. Deterministic, so tests can
/// drive sessions with a manual clock.
#[derive(Clone, Default)]
pub struct MemHub {
    inner: Arc<Mutex<HubInner>>,
}

#[derive(Default)]
struct HubInner {
    queues: HashMap<SocketAddr, VecDeque<(Vec<u8>, SocketAddr)>>,
    next: u16,
    loss: f32,
    rng: u64,
    cut: HashSet<SocketAddr>,
}

impl MemHub {
    pub fn new() -> Self {
        Self::default()
    }
    /// A new endpoint with its own address.
    pub fn endpoint(&self) -> MemTransport {
        let mut h = self.inner.lock().unwrap();
        h.next += 1;
        let addr: SocketAddr = format!("10.0.0.1:{}", 20000 + h.next).parse().unwrap();
        h.queues.insert(addr, VecDeque::new());
        MemTransport { addr, hub: self.clone() }
    }
    /// Drop this fraction (0..1) of packets.
    pub fn set_loss(&self, loss: f32) {
        let mut h = self.inner.lock().unwrap();
        h.loss = loss;
        h.rng = 0x9E37_79B9_7F4A_7C15;
    }
    /// Silently drop everything to and from `addr` (a vanished peer).
    pub fn cut(&self, addr: SocketAddr) {
        self.inner.lock().unwrap().cut.insert(addr);
    }
}

pub struct MemTransport {
    addr: SocketAddr,
    hub: MemHub,
}

impl Transport for MemTransport {
    fn send(&self, data: &[u8], to: SocketAddr) {
        let mut h = self.hub.inner.lock().unwrap();
        if h.cut.contains(&self.addr) || h.cut.contains(&to) {
            return;
        }
        if h.loss > 0.0 {
            h.rng ^= h.rng << 13;
            h.rng ^= h.rng >> 7;
            h.rng ^= h.rng << 17;
            if (h.rng % 10_000) as f32 / 10_000.0 < h.loss {
                return;
            }
        }
        if let Some(q) = h.queues.get_mut(&to) {
            q.push_back((data.to_vec(), self.addr));
        }
    }
    fn recv(&mut self, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
        let mut h = self.hub.inner.lock().unwrap();
        let (data, from) = h.queues.get_mut(&self.addr)?.pop_front()?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Some((n, from))
    }
    fn local_addr(&self) -> Option<SocketAddr> {
        Some(self.addr)
    }
}

// ------------------------------------------------------------------ config / state

/// Session settings (from the scene's Network Manager when there is one).
#[derive(Clone, Debug)]
pub struct NetConfig {
    pub player_name: String,
    pub session_name: String,
    pub max_players: u32,
    pub timeout: f32,
    pub discovery: bool,
    pub discovery_port: u16,
    pub clients_can_spawn: bool,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            player_name: std::env::var("CITRUS_NAME").unwrap_or_else(|_| "Player".to_string()),
            session_name: "Citrus Session".to_string(),
            max_players: 16,
            timeout: 10.0,
            discovery: true,
            discovery_port: DISCOVERY_PORT,
            clients_can_spawn: true,
        }
    }
}

impl NetConfig {
    pub fn from_manager(m: &NetworkManager) -> Self {
        Self {
            session_name: m.session_name.clone(),
            max_players: m.max_players.max(1),
            timeout: m.timeout.max(1.0),
            discovery: m.discovery,
            clients_can_spawn: m.clients_can_spawn,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected(String),
}

/// Traffic counters for the Network window.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetStats {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub packets_in: u64,
    pub packets_out: u64,
    pub resends: u64,
    /// Bytes per second over the last full second.
    pub rate_in: f32,
    pub rate_out: f32,
    window_start: f64,
    window_in: u64,
    window_out: u64,
}

/// A replicated runtime spawn, kept by every peer for late-join catch-up.
#[derive(Clone, Debug)]
pub struct SpawnRecord {
    pub id: u128,
    pub source: NetSpawnSource,
    pub translation: Vec3,
    pub rotation: Quat,
    pub owner: u64,
}

/// Latest known world transform of an object.
#[derive(Clone, Copy, Debug)]
pub struct XformState {
    pub pos: Vec3,
    pub rot: Quat,
    pub scale: Vec3,
    pub vel: Vec3,
}

impl Default for XformState {
    fn default() -> Self {
        Self { pos: Vec3::ZERO, rot: Quat::IDENTITY, scale: Vec3::ONE, vel: Vec3::ZERO }
    }
}

/// The replicated registry: authoritative on the host, mirrored on clients.
#[derive(Default)]
pub struct NetWorld {
    /// netid -> (owner, seq). Owner 0 = unowned (host authority).
    pub owners: HashMap<u128, (u64, u64)>,
    pub spawns: Vec<SpawnRecord>,
    pub vars: HashMap<u128, Vec<(String, NetValue)>>,
    pub actives: HashMap<u128, bool>,
    pub parents: HashMap<u128, u128>,
    pub xforms: HashMap<u128, XformState>,
    pub anims: HashMap<u128, HashMap<String, f32>>,
    /// Scene the host switched everyone to with `net_load_scene`.
    pub scene: Option<String>,
    // Host-side settings gathered from the scene each frame.
    pub modes: HashMap<u128, OwnershipMode>,
    pub var_anyone: HashSet<u128>,
    pub relevancy: HashMap<u128, f32>,
    pub despawn_on_leave: HashSet<u128>,
    /// Each peer's player object (relevancy origin, despawned on leave).
    pub player_obj: HashMap<u64, u128>,
}

impl NetWorld {
    fn forget(&mut self, id: u128) {
        self.owners.remove(&id);
        self.spawns.retain(|s| s.id != id);
        self.vars.remove(&id);
        self.actives.remove(&id);
        self.parents.remove(&id);
        self.xforms.remove(&id);
        self.anims.remove(&id);
        self.despawn_on_leave.remove(&id);
        self.player_obj.retain(|_, o| *o != id);
    }
    fn set_var(&mut self, id: u128, key: &str, value: NetValue) {
        let vars = self.vars.entry(id).or_default();
        match vars.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value,
            None => vars.push((key.to_owned(), value)),
        }
    }
}

/// One transform sample (sender clock).
#[derive(Clone, Copy, Debug)]
pub struct Snap {
    pub time: f64,
    pub pos: Vec3,
    pub rot: Quat,
    pub scale: Vec3,
    pub vel: Vec3,
}

/// Received snapshots for one object, oldest first.
#[derive(Default)]
pub struct SnapBuffer {
    pub origin: u64,
    pub snaps: VecDeque<Snap>,
    /// Set by a late-join baseline: hold it until live snapshots arrive.
    pub baseline: bool,
}

/// One received avatar sample: world poses in wire order (head, hands, hips, feet).
#[derive(Clone, Copy, Debug)]
pub struct AvatarSnap {
    pub time: f64,
    pub poses: [Option<(Vec3, Quat)>; TRACKER_SLOTS],
}

#[derive(Default)]
pub struct AvatarBuffer {
    pub origin: u64,
    pub snaps: VecDeque<AvatarSnap>,
}

/// A transform entry on the wire. Omitted fields are unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub struct SnapEntry {
    pub id: u128,
    pub pos: Option<Vec3>,
    pub rot: Option<Quat>,
    pub scale: Option<Vec3>,
    pub vel: Option<Vec3>,
    pub quantized: bool,
}

/// Compat: latest received transform of an object.
#[derive(Clone, Copy)]
pub struct RemoteTransform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
    pub seq: u64,
}

/// A received voice frame (mono PCM) tagged by sender + sequence for the jitter
/// buffer to reorder.
pub struct VoicePacket {
    pub from: u64,
    pub seq: u32,
    /// Codec payload (first byte names the codec, see `voice_codec`).
    pub payload: Vec<u8>,
}

/// A synced-variable write waiting for its object to exist in the scene.
pub struct PendingVar {
    pub id: u128,
    pub key: String,
    pub value: NetValue,
    pub expires: f64,
}

struct Conn {
    addr: SocketAddr,
    peer: u64,
    name: String,
    last_recv: f64,
    last_ping: f64,
    rtt: f32,
    send_seq: u32,
    unacked: BTreeMap<u32, (Vec<u8>, f64)>,
    recv_next: u32,
    recv_buf: BTreeMap<u32, (bool, Vec<u8>)>,
    frag: Vec<u8>,
    acks: Vec<u32>,
}

impl Conn {
    fn new(addr: SocketAddr, peer: u64, now: f64) -> Self {
        Self {
            addr,
            peer,
            name: String::new(),
            last_recv: now,
            last_ping: now,
            rtt: 0.05,
            send_seq: 0,
            unacked: BTreeMap::new(),
            recv_next: 0,
            recv_buf: BTreeMap::new(),
            frag: Vec::new(),
            acks: Vec::new(),
        }
    }
    fn rto(&self) -> f64 {
        (self.rtt as f64 * 2.0 + 0.05).clamp(0.08, 1.0)
    }
}

enum Clock {
    Real(Instant),
    Manual(f64),
}

/// Per-object outgoing replication state (owned by `net_sync`).
#[derive(Default)]
pub struct SendState {
    pub last_send: f64,
    pub last_key: f64,
    pub sent: Option<XformState>,
    pub prev_pos: Option<(Vec3, f64)>,
    pub parent: Option<u128>,
    pub active: Option<bool>,
    pub anim: HashMap<String, f32>,
    pub last_anim: f64,
    pub last_anim_full: f64,
    pub last_avatar: f64,
    pub seen: bool,
    pub missing_since: Option<f64>,
}

// ------------------------------------------------------------------ session

/// One live networking session (host or client).
pub struct NetSession {
    transport: Box<dyn Transport>,
    discovery: Option<UdpSocket>,
    pub cfg: NetConfig,
    clock: Clock,
    is_host: bool,
    local_peer: u64,
    state: ConnState,
    host_addr: Option<SocketAddr>,
    started: f64,
    last_hello: f64,
    conns: HashMap<u64, Conn>,
    addr_peer: HashMap<SocketAddr, u64>,
    /// Everyone known (id -> name), the local peer included.
    peers: BTreeMap<u64, String>,
    next_peer: u64,
    claim_seq: u64,
    xform_seq: u64,
    pub world: NetWorld,
    pub stats: NetStats,
    // Outputs drained by `view()`.
    inbox: Vec<(u64, bool, String)>,
    events: Vec<NetEvent>,
    rpcs: Vec<NetRpc>,
    world_ops: Vec<NetWorldOp>,
    hold_ops_until: f64,
    voice_in: Vec<VoicePacket>,
    // Replication inputs/outputs used by `net_sync`.
    pub pending_vars: Vec<PendingVar>,
    pub pending_parents: Vec<(u128, u128, f64)>,
    pub buffers: HashMap<u128, SnapBuffer>,
    pub offsets: HashMap<u64, f64>,
    pub anim_in: HashMap<u128, HashMap<String, f32>>,
    pub avatar_in: HashMap<u128, AvatarBuffer>,
    /// Latest face frame per avatar: (sender time, frame).
    pub face_in: HashMap<u128, (f64, citrus_core::FaceFrame)>,
    pub send_state: HashMap<u128, SendState>,
    pub remote_bodies: HashMap<ObjectId, Vec3>,
    pub reclaimed_bodies: HashMap<ObjectId, Vec3>,
    /// Peers whose player object the host still has to spawn.
    pub players_to_spawn: Vec<u64>,
    pub host_player_done: bool,
    pub spawn_counter: u32,
}

impl NetSession {
    /// Start hosting on `port` (0 = OS-assigned). The host is peer id 1.
    pub fn host(port: u16) -> std::io::Result<Self> {
        Self::host_with(port, NetConfig::default())
    }

    pub fn host_with(port: u16, cfg: NetConfig) -> std::io::Result<Self> {
        let t = UdpTransport::bind(port)?;
        tracing::info!("net: hosting on {:?}", t.local_addr());
        let mut s = Self::host_on(Box::new(t), cfg);
        if s.cfg.discovery {
            match UdpSocket::bind(("0.0.0.0", s.cfg.discovery_port)).and_then(|d| d.set_nonblocking(true).map(|_| d)) {
                Ok(d) => s.discovery = Some(d),
                Err(e) => tracing::debug!("net: LAN discovery unavailable: {e}"),
            }
        }
        Ok(s)
    }

    /// Host over any transport (tests use [`MemHub`]).
    pub fn host_on(transport: Box<dyn Transport>, cfg: NetConfig) -> Self {
        let mut s = Self::new(transport, cfg, true);
        s.local_peer = HOST_PEER;
        s.state = ConnState::Connected;
        s.peers.insert(HOST_PEER, s.cfg.player_name.clone());
        s.events.push(NetEvent::Connected { peer: HOST_PEER });
        s
    }

    /// Join a host at `addr` (e.g. "127.0.0.1:9000").
    pub fn join(addr: &str) -> std::io::Result<Self> {
        Self::join_with(addr, NetConfig::default())
    }

    pub fn join_with(addr: &str, cfg: NetConfig) -> std::io::Result<Self> {
        let host = parse_addr(addr)?;
        let t = UdpTransport::bind(0)?;
        tracing::info!("net: joining {host}");
        Ok(Self::join_on(Box::new(t), host, cfg))
    }

    pub fn join_on(transport: Box<dyn Transport>, host: SocketAddr, cfg: NetConfig) -> Self {
        let mut s = Self::new(transport, cfg, false);
        let now = s.now();
        s.host_addr = Some(host);
        s.conns.insert(HOST_PEER, Conn::new(host, HOST_PEER, now));
        s.addr_peer.insert(host, HOST_PEER);
        s.send_hello();
        s
    }

    fn new(transport: Box<dyn Transport>, cfg: NetConfig, is_host: bool) -> Self {
        Self {
            transport,
            discovery: None,
            cfg,
            clock: Clock::Real(Instant::now()),
            is_host,
            local_peer: 0,
            state: ConnState::Connecting,
            host_addr: None,
            started: 0.0,
            last_hello: 0.0,
            conns: HashMap::new(),
            addr_peer: HashMap::new(),
            peers: BTreeMap::new(),
            next_peer: 2,
            claim_seq: 0,
            xform_seq: 0,
            world: NetWorld::default(),
            stats: NetStats::default(),
            inbox: Vec::new(),
            events: Vec::new(),
            rpcs: Vec::new(),
            world_ops: Vec::new(),
            hold_ops_until: 0.0,
            voice_in: Vec::new(),
            pending_vars: Vec::new(),
            pending_parents: Vec::new(),
            buffers: HashMap::new(),
            offsets: HashMap::new(),
            anim_in: HashMap::new(),
            avatar_in: HashMap::new(),
            face_in: HashMap::new(),
            send_state: HashMap::new(),
            remote_bodies: HashMap::new(),
            reclaimed_bodies: HashMap::new(),
            players_to_spawn: Vec::new(),
            host_player_done: false,
            spawn_counter: 0,
        }
    }

    /// Drive time by hand (tests): `now()` only moves with [`Self::advance`].
    pub fn use_manual_clock(&mut self) {
        let now = self.now();
        self.clock = Clock::Manual(now);
    }
    pub fn advance(&mut self, dt: f64) {
        if let Clock::Manual(t) = &mut self.clock {
            *t += dt;
        }
    }
    /// Seconds since the session started (this machine's clock).
    pub fn now(&self) -> f64 {
        match &self.clock {
            Clock::Real(start) => start.elapsed().as_secs_f64(),
            Clock::Manual(t) => *t,
        }
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.transport.local_addr()
    }
    pub fn is_host(&self) -> bool {
        self.is_host
    }
    pub fn local_peer(&self) -> u64 {
        self.local_peer
    }
    pub fn state(&self) -> &ConnState {
        &self.state
    }
    pub fn is_connected(&self) -> bool {
        self.state == ConnState::Connected
    }
    pub fn peer_count(&self) -> usize {
        self.peers.len().max(1)
    }
    /// Round-trip time to the host in ms (0 on the host).
    pub fn rtt_ms(&self) -> f32 {
        if self.is_host {
            0.0
        } else {
            self.conns.get(&HOST_PEER).map(|c| c.rtt * 1000.0).unwrap_or(0.0)
        }
    }

    /// Everyone in the session, local peer included.
    pub fn peer_infos(&self) -> Vec<PeerInfo> {
        self.peers
            .iter()
            .map(|(&id, name)| {
                let rtt = if id == self.local_peer {
                    0.0
                } else if self.is_host {
                    self.conns.get(&id).map(|c| c.rtt * 1000.0).unwrap_or(0.0)
                } else if id == HOST_PEER {
                    self.rtt_ms()
                } else {
                    0.0
                };
                PeerInfo { id, name: name.clone(), rtt_ms: rtt, is_host: id == HOST_PEER }
            })
            .collect()
    }

    /// Owner peer of a network id (0 = unowned).
    pub fn owner_of_raw(&self, id: u128) -> u64 {
        self.world.owners.get(&id).map(|(o, _)| *o).unwrap_or(0)
    }
    pub fn owner_of(&self, id: ObjectId) -> u64 {
        self.owner_of_raw(id.raw())
    }

    /// Whether the local peer holds ownership of an object.
    pub fn owns(&self, id: ObjectId) -> bool {
        self.local_peer != 0 && self.owner_of(id) == self.local_peer
    }

    /// Whether the local peer simulates an object (owner, or host when unowned).
    pub fn has_authority_raw(&self, id: u128) -> bool {
        if !self.is_connected() {
            return false;
        }
        match self.owner_of_raw(id) {
            0 => self.is_host,
            o => o == self.local_peer,
        }
    }

    /// Non-draining `(object, owner)` snapshot, used to position remote peers'
    /// voices at the object they own (spatial voice).
    pub fn owners_snapshot(&self) -> Vec<(ObjectId, u64)> {
        let mut out: Vec<(ObjectId, u64)> = self
            .world
            .player_obj
            .iter()
            .map(|(p, id)| (ObjectId::from_raw(*id), *p))
            .collect();
        out.extend(
            self.world
                .owners
                .iter()
                .filter(|(id, (o, _))| *o != 0 && !self.world.player_obj.values().any(|p| p == *id))
                .map(|(id, (o, _))| (ObjectId::from_raw(*id), *o)),
        );
        out
    }

    /// Build the per-frame [`NetView`] components see, draining received
    /// messages, events and RPCs into it (each is delivered once).
    pub fn view(&mut self) -> NetView {
        let owners = self
            .world
            .owners
            .iter()
            .filter(|(_, (o, _))| *o != 0)
            .map(|(id, (o, _))| (ObjectId::from_raw(*id), *o))
            .collect();
        // After a scene switch, hold later world ops until the new scene is in.
        let world_ops = if self.now() < self.hold_ops_until {
            match self.world_ops.iter().rposition(|op| matches!(op, NetWorldOp::LoadScene(_))) {
                Some(i) => self.world_ops.drain(..=i).collect(),
                None => Vec::new(),
            }
        } else {
            std::mem::take(&mut self.world_ops)
        };
        NetView {
            connected: self.is_connected(),
            is_server: self.is_host,
            local_peer: self.local_peer,
            owners,
            messages: std::mem::take(&mut self.inbox),
            peers: self.peer_infos(),
            events: std::mem::take(&mut self.events),
            rpcs: std::mem::take(&mut self.rpcs),
            world_ops,
            remote_bodies: self.remote_bodies.clone(),
            reclaimed_bodies: std::mem::take(&mut self.reclaimed_bodies),
            rtt_ms: self.rtt_ms(),
        }
    }

    /// Drain voice packets received since the last call (for the jitter buffer).
    pub fn take_voice(&mut self) -> Vec<VoicePacket> {
        std::mem::take(&mut self.voice_in)
    }

    /// Forget all replicated world state (Play stopped in the editor).
    pub fn reset_world(&mut self) {
        self.world = NetWorld::default();
        self.buffers.clear();
        self.anim_in.clear();
        self.avatar_in.clear();
        self.face_in.clear();
        self.send_state.clear();
        self.pending_vars.clear();
        self.pending_parents.clear();
        self.world_ops.clear();
        self.remote_bodies.clear();
        self.reclaimed_bodies.clear();
        self.host_player_done = false;
        self.players_to_spawn = if self.is_host { self.conns.keys().copied().collect() } else { Vec::new() };
    }

    fn push_event(&mut self, e: NetEvent) {
        if self.events.len() >= MAX_QUEUE {
            self.events.remove(0);
        }
        self.events.push(e);
    }

    // ------------------------------------------------------------ raw send

    fn send_raw(&mut self, data: &[u8], to: SocketAddr) {
        self.stats.bytes_out += data.len() as u64;
        self.stats.packets_out += 1;
        self.transport.send(data, to);
    }

    fn send_to_peer(&mut self, peer: u64, data: &[u8]) {
        if let Some(addr) = self.conns.get(&peer).map(|c| c.addr) {
            self.send_raw(data, addr);
        }
    }

    /// Unreliable send to everyone else (host: all clients; client: the host).
    fn fanout(&mut self, data: &[u8], except: Option<u64>) {
        let targets: Vec<SocketAddr> =
            self.conns.values().filter(|c| Some(c.peer) != except).map(|c| c.addr).collect();
        for a in targets {
            self.send_raw(data, a);
        }
    }

    fn send_hello(&mut self) {
        let Some(host) = self.host_addr else {
            return;
        };
        let mut b = vec![P_HELLO];
        put_u16(&mut b, PROTOCOL);
        put_str(&mut b, &self.cfg.player_name);
        self.last_hello = self.now();
        self.send_raw(&b, host);
    }

    // ------------------------------------------------------------ reliable channel

    fn send_reliable(&mut self, peer: u64, msg: &[u8]) {
        let now = self.now();
        let Some(c) = self.conns.get_mut(&peer) else {
            return;
        };
        let addr = c.addr;
        let mut out = Vec::new();
        let n = msg.len().div_ceil(FRAG).max(1);
        for k in 0..n {
            let chunk = &msg[(k * FRAG).min(msg.len())..((k + 1) * FRAG).min(msg.len())];
            let seq = c.send_seq;
            c.send_seq = c.send_seq.wrapping_add(1);
            let mut p = Vec::with_capacity(chunk.len() + 6);
            p.push(P_REL);
            put_u32(&mut p, seq);
            p.push((k + 1 == n) as u8);
            p.extend_from_slice(chunk);
            c.unacked.insert(seq, (p.clone(), now));
            out.push(p);
        }
        for p in out {
            self.send_raw(&p, addr);
        }
    }

    /// Reliable send to every client (host only), optionally skipping one.
    fn broadcast_reliable(&mut self, msg: &[u8], except: Option<u64>) {
        let peers: Vec<u64> = self.conns.keys().copied().filter(|p| Some(*p) != except).collect();
        for p in peers {
            self.send_reliable(p, msg);
        }
    }

    /// Host: to all clients; client: to the host.
    fn upstream_or_broadcast(&mut self, msg: &[u8]) {
        if self.is_host {
            self.broadcast_reliable(msg, None);
        } else {
            self.send_reliable(HOST_PEER, msg);
        }
    }

    fn on_rel(&mut self, peer: u64, data: &[u8]) {
        let mut c = Cursor::new(data, 1);
        let seq = c.u32();
        let last = c.u8() != 0;
        if c.bad {
            return;
        }
        let payload = c.rest().to_vec();
        let mut ready = Vec::new();
        {
            let Some(conn) = self.conns.get_mut(&peer) else {
                return;
            };
            conn.acks.push(seq);
            let ahead = seq.wrapping_sub(conn.recv_next);
            if ahead >= 1 << 20 || conn.recv_buf.contains_key(&seq) {
                return; // duplicate or ancient
            }
            conn.recv_buf.insert(seq, (last, payload));
            while let Some((last, p)) = conn.recv_buf.remove(&conn.recv_next) {
                conn.recv_next = conn.recv_next.wrapping_add(1);
                conn.frag.extend_from_slice(&p);
                if last {
                    ready.push(std::mem::take(&mut conn.frag));
                }
            }
        }
        for m in ready {
            self.on_msg(peer, &m);
        }
    }

    fn flush_acks(&mut self) {
        let mut out = Vec::new();
        for c in self.conns.values_mut() {
            if c.acks.is_empty() {
                continue;
            }
            for chunk in c.acks.chunks((MTU - 4) / 4) {
                let mut b = vec![P_ACK];
                put_u16(&mut b, chunk.len() as u16);
                for s in chunk {
                    put_u32(&mut b, *s);
                }
                out.push((b, c.addr));
            }
            c.acks.clear();
        }
        for (b, a) in out {
            self.send_raw(&b, a);
        }
    }

    // ------------------------------------------------------------ pump

    /// Receive + process all pending packets, resend, ping and time out. Call
    /// once per frame (also while not playing, to keep connections alive).
    pub fn pump(&mut self) {
        let mut buf = [0u8; RECV_BUF];
        while let Some((len, src)) = self.transport.recv(&mut buf) {
            let data = buf[..len].to_vec();
            self.stats.bytes_in += len as u64;
            self.stats.packets_in += 1;
            self.handle(&data, src);
        }
        self.serve_discovery();
        let now = self.now();
        if self.state == ConnState::Connecting && !self.is_host {
            if now - self.started > self.cfg.timeout as f64 {
                self.disconnect_local("could not reach the host");
            } else if now - self.last_hello > HELLO_INTERVAL {
                self.send_hello();
            }
        }
        // Resends, pings, timeouts.
        let mut resend = Vec::new();
        let mut ping = Vec::new();
        let mut dead = Vec::new();
        let connected = self.state == ConnState::Connected;
        for c in self.conns.values_mut() {
            let rto = c.rto();
            for (p, sent) in c.unacked.values_mut() {
                if now - *sent > rto {
                    *sent = now;
                    resend.push((p.clone(), c.addr));
                }
            }
            if connected && now - c.last_ping > PING_INTERVAL {
                c.last_ping = now;
                ping.push(c.addr);
            }
            if now - c.last_recv > self.cfg.timeout as f64 {
                dead.push(c.peer);
            }
        }
        self.stats.resends += resend.len() as u64;
        for (p, a) in resend {
            self.send_raw(&p, a);
        }
        for a in ping {
            let mut b = vec![P_PING];
            put_f64(&mut b, now);
            self.send_raw(&b, a);
        }
        for p in dead {
            if self.is_host {
                self.drop_peer(p, "timed out");
            } else if self.state != ConnState::Connecting {
                self.disconnect_local("connection to the host timed out");
            }
        }
        self.flush_acks();
        let s = &mut self.stats;
        if now - s.window_start >= 1.0 {
            let span = (now - s.window_start) as f32;
            s.rate_in = (s.bytes_in - s.window_in) as f32 / span;
            s.rate_out = (s.bytes_out - s.window_out) as f32 / span;
            s.window_in = s.bytes_in;
            s.window_out = s.bytes_out;
            s.window_start = now;
        }
        self.pending_vars.retain(|p| p.expires > now);
        self.pending_parents.retain(|p| p.2 > now);
    }

    fn serve_discovery(&mut self) {
        let Some(d) = self.discovery.as_ref() else {
            return;
        };
        let mut buf = [0u8; 64];
        let game_port = self.transport.local_addr().map(|a| a.port()).unwrap_or(0);
        while let Ok((n, src)) = d.recv_from(&mut buf) {
            if n < 1 || buf[0] != P_DISCOVER {
                continue;
            }
            let mut b = vec![P_INFO];
            put_u16(&mut b, PROTOCOL);
            put_u16(&mut b, game_port);
            put_u32(&mut b, self.peers.len() as u32);
            put_u32(&mut b, self.cfg.max_players);
            put_str(&mut b, &self.cfg.session_name);
            let _ = d.send_to(&b, src);
        }
    }

    fn disconnect_local(&mut self, reason: &str) {
        if matches!(self.state, ConnState::Disconnected(_)) {
            return;
        }
        tracing::info!("net: disconnected ({reason})");
        self.state = ConnState::Disconnected(reason.to_owned());
        self.push_event(NetEvent::Disconnected { reason: reason.to_owned() });
    }

    /// Leave the session (clients) or close it (host), telling the others.
    pub fn disconnect(&mut self) {
        let addrs: Vec<SocketAddr> = self.conns.values().map(|c| c.addr).collect();
        for a in addrs {
            for _ in 0..3 {
                self.send_raw(&[P_BYE], a);
            }
        }
        self.conns.clear();
        self.disconnect_local("disconnected");
    }

    // ------------------------------------------------------------ packet dispatch

    fn handle(&mut self, data: &[u8], src: SocketAddr) {
        let Some(&tag) = data.first() else {
            return;
        };
        if self.is_host && tag == P_HELLO {
            self.on_hello(data, src);
            return;
        }
        let Some(&peer) = self.addr_peer.get(&src) else {
            return;
        };
        let now = self.now();
        if let Some(c) = self.conns.get_mut(&peer) {
            c.last_recv = now;
        }
        let mut c = Cursor::new(data, 1);
        match tag {
            P_WELCOME if !self.is_host => {
                let id = c.u64();
                let max = c.u32();
                let name = c.string();
                if self.state == ConnState::Connecting && !c.bad {
                    self.local_peer = id;
                    self.cfg.max_players = max;
                    self.cfg.session_name = name;
                    self.state = ConnState::Connected;
                    self.peers.insert(id, self.cfg.player_name.clone());
                    self.push_event(NetEvent::Connected { peer: id });
                    tracing::info!("net: joined as peer {id}");
                }
            }
            P_REJECT if !self.is_host => {
                let reason = c.string();
                self.disconnect_local(&format!("rejected: {reason}"));
            }
            P_PING => {
                let t = c.f64();
                let mut b = vec![P_PONG];
                put_f64(&mut b, t);
                self.send_raw(&b, src);
            }
            P_PONG => {
                let t = c.f64();
                if let Some(conn) = self.conns.get_mut(&peer) {
                    let sample = (now - t).max(0.0) as f32;
                    conn.rtt = conn.rtt * 0.8 + sample * 0.2;
                }
            }
            P_BYE => {
                if self.is_host {
                    self.drop_peer(peer, "left");
                } else {
                    self.disconnect_local("the host closed the session");
                }
            }
            P_REL => self.on_rel(peer, data),
            P_ACK => {
                let n = c.u16() as usize;
                if let Some(conn) = self.conns.get_mut(&peer) {
                    for _ in 0..n {
                        let s = c.u32();
                        if c.bad {
                            break;
                        }
                        conn.unacked.remove(&s);
                    }
                }
            }
            P_SNAP => self.on_snap(peer, data),
            P_AVATAR => self.on_avatar(peer, data),
            P_FACE => self.on_face(peer, data),
            P_ANIM => self.on_anim(peer, data),
            P_VOICE => {
                let from = c.u64();
                let seq = c.u32();
                let n = c.u16() as usize;
                let at = 1 + 8 + 4 + 2;
                let Some(payload) = data.get(at..at + n) else {
                    return;
                };
                if c.bad {
                    return;
                }
                if from != self.local_peer {
                    self.voice_in.push(VoicePacket { from, seq, payload: payload.to_vec() });
                    if self.voice_in.len() > 512 {
                        self.voice_in.remove(0);
                    }
                }
                if self.is_host {
                    self.fanout(data, Some(peer));
                }
            }
            P_URPC => {
                let body = &data[1..];
                self.on_rpc(peer, body, false);
            }
            _ => {}
        }
    }

    fn on_hello(&mut self, data: &[u8], src: SocketAddr) {
        let now = self.now();
        if let Some(&peer) = self.addr_peer.get(&src) {
            // Duplicate HELLO: our WELCOME was lost.
            if let Some(c) = self.conns.get_mut(&peer) {
                c.last_recv = now;
            }
            self.send_welcome(peer, src);
            return;
        }
        let mut c = Cursor::new(data, 1);
        let proto = c.u16();
        let name = c.string();
        let reject = |s: &mut Self, why: &str| {
            let mut b = vec![P_REJECT];
            put_str(&mut b, why);
            s.send_raw(&b, src);
        };
        if c.bad || proto != PROTOCOL {
            reject(self, "protocol version mismatch");
            return;
        }
        if self.peers.len() as u32 >= self.cfg.max_players {
            reject(self, "session full");
            return;
        }
        let peer = self.next_peer;
        self.next_peer += 1;
        let mut conn = Conn::new(src, peer, now);
        conn.name = if name.is_empty() { format!("Player {peer}") } else { name };
        let name = conn.name.clone();
        self.conns.insert(peer, conn);
        self.addr_peer.insert(src, peer);
        self.send_welcome(peer, src);
        // Tell everyone else, then bring the newcomer up to date.
        let mut j = vec![M_JOINED];
        put_u64(&mut j, peer);
        put_str(&mut j, &name);
        self.broadcast_reliable(&j, Some(peer));
        self.peers.insert(peer, name.clone());
        self.send_catchup(peer);
        self.players_to_spawn.push(peer);
        self.push_event(NetEvent::PeerJoined { id: peer, name: name.clone() });
        tracing::info!("net: peer {peer} ({name}) joined from {src}");
    }

    fn send_welcome(&mut self, peer: u64, src: SocketAddr) {
        let mut b = vec![P_WELCOME];
        put_u64(&mut b, peer);
        put_u32(&mut b, self.cfg.max_players);
        put_str(&mut b, &self.cfg.session_name);
        self.send_raw(&b, src);
    }

    /// Everything a late joiner needs, in dependency order.
    fn send_catchup(&mut self, peer: u64) {
        let mut m = vec![M_PEERS];
        put_u32(&mut m, self.peers.len() as u32);
        for (id, name) in &self.peers {
            put_u64(&mut m, *id);
            put_str(&mut m, name);
        }
        self.send_reliable(peer, &m);
        if let Some(scene) = self.world.scene.clone() {
            let mut m = vec![M_SCENE];
            put_str(&mut m, &scene);
            self.send_reliable(peer, &m);
        }
        let mut msgs = Vec::new();
        for s in &self.world.spawns {
            msgs.push(enc_spawn(s));
        }
        for (id, (owner, seq)) in &self.world.owners {
            let mut m = vec![M_OWNER];
            put_u128(&mut m, *id);
            put_u64(&mut m, *owner);
            put_u64(&mut m, *seq);
            msgs.push(m);
        }
        for (id, parent) in &self.world.parents {
            let mut m = vec![M_PARENT];
            put_u128(&mut m, *id);
            put_u128(&mut m, *parent);
            msgs.push(m);
        }
        for (id, on) in &self.world.actives {
            let mut m = vec![M_ACTIVE];
            put_u128(&mut m, *id);
            m.push(*on as u8);
            msgs.push(m);
        }
        for (id, vars) in &self.world.vars {
            for (k, v) in vars {
                msgs.push(enc_var(*id, k, v, HOST_PEER));
            }
        }
        for (id, x) in &self.world.xforms {
            let mut m = vec![M_XFORM];
            enc_entry(
                &mut m,
                &SnapEntry { id: *id, pos: Some(x.pos), rot: Some(x.rot), scale: Some(x.scale), vel: None, quantized: false },
            );
            msgs.push(m);
        }
        for (id, params) in &self.world.anims {
            let mut m = vec![M_ANIM];
            put_u128(&mut m, *id);
            put_u16(&mut m, params.len() as u16);
            for (k, v) in params {
                put_str(&mut m, k);
                put_f32(&mut m, *v);
            }
            msgs.push(m);
        }
        for m in msgs {
            self.send_reliable(peer, &m);
        }
    }

    /// Host: a client left or timed out.
    fn drop_peer(&mut self, peer: u64, reason: &str) {
        let Some(conn) = self.conns.remove(&peer) else {
            return;
        };
        self.addr_peer.remove(&conn.addr);
        self.peers.remove(&peer);
        self.players_to_spawn.retain(|p| *p != peer);
        let mut m = vec![M_LEFT];
        put_u64(&mut m, peer);
        put_str(&mut m, reason);
        self.broadcast_reliable(&m, None);
        // Their objects: despawn the ones flagged, release the rest.
        let owned: Vec<u128> =
            self.world.owners.iter().filter(|(_, (o, _))| *o == peer).map(|(id, _)| *id).collect();
        let player = self.world.player_obj.get(&peer).copied();
        for id in owned {
            if self.world.despawn_on_leave.contains(&id) || Some(id) == player {
                self.despawn_raw(id);
            } else {
                self.grant(id, 0);
            }
        }
        if let Some(p) = player
            && self.world.spawns.iter().any(|s| s.id == p) {
                self.despawn_raw(p);
            }
        self.world.player_obj.remove(&peer);
        self.offsets.remove(&peer);
        self.push_event(NetEvent::PeerLeft { id: peer, reason: reason.to_owned() });
        tracing::info!("net: peer {peer} {reason}");
    }

    // ------------------------------------------------------------ reliable messages

    fn on_msg(&mut self, from: u64, m: &[u8]) {
        let Some(&tag) = m.first() else {
            return;
        };
        let mut c = Cursor::new(m, 1);
        let host = self.is_host;
        match tag {
            M_PEERS if !host => {
                let n = c.u32();
                let mut list = BTreeMap::new();
                for _ in 0..n.min(4096) {
                    let id = c.u64();
                    let name = c.string();
                    list.insert(id, name);
                }
                if c.bad {
                    return;
                }
                list.insert(self.local_peer, self.cfg.player_name.clone());
                for (id, name) in &list {
                    if *id != self.local_peer && !self.peers.contains_key(id) {
                        self.push_event(NetEvent::PeerJoined { id: *id, name: name.clone() });
                    }
                }
                self.peers = list;
            }
            M_JOINED if !host => {
                let id = c.u64();
                let name = c.string();
                if !c.bad && id != self.local_peer {
                    self.peers.insert(id, name.clone());
                    self.push_event(NetEvent::PeerJoined { id, name });
                }
            }
            M_LEFT if !host => {
                let id = c.u64();
                let reason = c.string();
                self.peers.remove(&id);
                self.offsets.remove(&id);
                self.push_event(NetEvent::PeerLeft { id, reason });
            }
            M_OWNER if !host => {
                let id = c.u128();
                let owner = c.u64();
                let seq = c.u64();
                if !c.bad {
                    self.set_owner_local(id, owner, seq);
                }
            }
            M_CLAIM if host => {
                let id = c.u128();
                if !c.bad {
                    self.handle_claim(id, from);
                }
            }
            M_RELEASE if host => {
                let id = c.u128();
                if !c.bad && self.owner_of_raw(id) == from {
                    self.grant(id, 0);
                }
            }
            M_TRANSFER if host => {
                let id = c.u128();
                let to = c.u64();
                if !c.bad && self.owner_of_raw(id) == from {
                    self.grant(id, to);
                }
            }
            M_OWNER_REQ if !host => {
                let id = c.u128();
                let by = c.u64();
                self.push_event(NetEvent::OwnershipRequested { object: ObjectId::from_raw(id), by });
            }
            M_DENY if !host => {
                let id = c.u128();
                self.push_event(NetEvent::OwnershipDenied { object: ObjectId::from_raw(id) });
            }
            M_SPAWN => {
                let Some(mut rec) = dec_spawn(&mut c) else {
                    return;
                };
                if host {
                    if !self.cfg.clients_can_spawn {
                        let mut d = vec![M_DESPAWN];
                        put_u128(&mut d, rec.id);
                        self.send_reliable(from, &d);
                        return;
                    }
                    if rec.owner != 0 {
                        rec.owner = from;
                    }
                    self.forward_spawn(rec, Some(from));
                } else {
                    self.apply_spawn(rec);
                }
            }
            M_DESPAWN => {
                let id = c.u128();
                if c.bad {
                    return;
                }
                if host {
                    let owner = self.owner_of_raw(id);
                    if owner != from && !(owner == 0 && from == HOST_PEER) {
                        return;
                    }
                    let mut d = vec![M_DESPAWN];
                    put_u128(&mut d, id);
                    self.broadcast_reliable(&d, Some(from));
                }
                self.apply_despawn(id);
            }
            M_VAR => {
                let id = c.u128();
                let key = c.string();
                let value = c.value();
                let sender = c.u64();
                if c.bad {
                    return;
                }
                let sender = if host { from } else { sender };
                if host {
                    let owner = self.owner_of_raw(id);
                    if owner != from && !self.world.var_anyone.contains(&id) {
                        tracing::debug!("net: dropped var write {key:?} from non-owner {from}");
                        return;
                    }
                    self.broadcast_reliable(&enc_var(id, &key, &value, from), Some(from));
                }
                self.apply_var(id, key, value, sender);
            }
            M_ACTIVE => {
                let id = c.u128();
                let on = c.u8() != 0;
                if c.bad || (host && self.owner_of_raw(id) != from) {
                    return;
                }
                if host {
                    self.broadcast_reliable(m, Some(from));
                }
                self.world.actives.insert(id, on);
                self.world_ops.push(NetWorldOp::SetActive(ObjectId::from_raw(id), on));
            }
            M_PARENT => {
                let id = c.u128();
                let parent = c.u128();
                if c.bad || (host && self.owner_of_raw(id) != from) {
                    return;
                }
                if host {
                    self.broadcast_reliable(m, Some(from));
                }
                self.world.parents.insert(id, parent);
                let exp = self.now() + PENDING_TTL;
                self.pending_parents.push((id, parent, exp));
            }
            M_RPC => self.on_rpc(from, &m[1..], true),
            M_TEXT => {
                let target = c.u64();
                let sender = c.u64();
                let text = c.string();
                if c.bad {
                    return;
                }
                if host {
                    // The host knows who really sent it.
                    let sender = from;
                    self.route_text(target, sender, &text, Some(from));
                } else {
                    self.inbox.push((sender, target != 0, text));
                }
            }
            M_SCENE if !host => {
                let path = c.string();
                if c.bad {
                    return;
                }
                self.clear_world_for_scene(&path);
            }
            M_XFORM if !host => {
                let Some(e) = dec_entry(&mut c) else {
                    return;
                };
                let x = XformState {
                    pos: e.pos.unwrap_or(Vec3::ZERO),
                    rot: e.rot.unwrap_or(Quat::IDENTITY),
                    scale: e.scale.unwrap_or(Vec3::ONE),
                    vel: Vec3::ZERO,
                };
                self.world.xforms.insert(e.id, x);
                let b = self.buffers.entry(e.id).or_default();
                if b.snaps.is_empty() {
                    b.baseline = true;
                    b.snaps.push_back(Snap { time: 0.0, pos: x.pos, rot: x.rot, scale: x.scale, vel: Vec3::ZERO });
                }
            }
            M_ANIM if !host => {
                let id = c.u128();
                let n = c.u16();
                let mut params = HashMap::new();
                for _ in 0..n {
                    let k = c.string();
                    let v = c.f32();
                    params.insert(k, v);
                }
                if !c.bad {
                    self.world.anims.insert(id, params.clone());
                    self.anim_in.insert(id, params);
                }
            }
            _ => {}
        }
    }

    fn clear_world_for_scene(&mut self, path: &str) {
        self.world = NetWorld { scene: Some(path.to_owned()), ..Default::default() };
        self.buffers.clear();
        self.anim_in.clear();
        self.avatar_in.clear();
        self.face_in.clear();
        self.send_state.clear();
        self.pending_vars.clear();
        self.pending_parents.clear();
        self.world_ops.retain(|op| matches!(op, NetWorldOp::LoadScene(_)));
        self.world_ops.push(NetWorldOp::LoadScene(path.to_owned()));
        self.hold_ops_until = self.now() + 0.5;
        if self.is_host {
            self.host_player_done = false;
            self.players_to_spawn = self.conns.keys().copied().collect();
        }
    }

    // ------------------------------------------------------------ ownership

    fn set_owner_local(&mut self, id: u128, owner: u64, seq: u64) {
        let prev = self.owner_of_raw(id);
        if let Some((_, s)) = self.world.owners.get(&id)
            && *s > seq
        {
            return;
        }
        self.world.owners.insert(id, (owner, seq));
        if prev != owner {
            self.push_event(NetEvent::OwnershipChanged { object: ObjectId::from_raw(id), owner, previous: prev });
        }
    }

    /// Host: assign ownership and tell everyone.
    fn grant(&mut self, id: u128, owner: u64) {
        self.claim_seq += 1;
        let seq = self.claim_seq;
        self.set_owner_local(id, owner, seq);
        let mut m = vec![M_OWNER];
        put_u128(&mut m, id);
        put_u64(&mut m, owner);
        put_u64(&mut m, seq);
        self.broadcast_reliable(&m, None);
    }

    fn handle_claim(&mut self, id: u128, by: u64) {
        let cur = self.owner_of_raw(id);
        let mode = self.world.modes.get(&id).copied().unwrap_or(OwnershipMode::Takeover);
        let ok = cur == by
            || match mode {
                OwnershipMode::Takeover => true,
                OwnershipMode::Request => cur == 0,
                OwnershipMode::Fixed => by == HOST_PEER,
            };
        if ok {
            self.grant(id, by);
            return;
        }
        if mode == OwnershipMode::Request && cur != 0 {
            if cur == self.local_peer {
                self.push_event(NetEvent::OwnershipRequested { object: ObjectId::from_raw(id), by });
            } else {
                let mut m = vec![M_OWNER_REQ];
                put_u128(&mut m, id);
                put_u64(&mut m, by);
                self.send_reliable(cur, &m);
            }
        }
        if by == self.local_peer {
            self.push_event(NetEvent::OwnershipDenied { object: ObjectId::from_raw(id) });
        } else {
            let mut m = vec![M_DENY];
            put_u128(&mut m, id);
            self.send_reliable(by, &m);
        }
    }

    /// Local peer claims authority over an object.
    pub fn request_ownership(&mut self, id: ObjectId) {
        if !self.is_connected() {
            return;
        }
        if self.is_host {
            self.handle_claim(id.raw(), HOST_PEER);
        } else {
            let mut m = vec![M_CLAIM];
            put_u128(&mut m, id.raw());
            self.send_reliable(HOST_PEER, &m);
        }
    }

    /// Local peer releases authority over an object.
    pub fn release_ownership(&mut self, id: ObjectId) {
        if !self.is_connected() || self.owner_of(id) != self.local_peer {
            return;
        }
        if self.is_host {
            self.grant(id.raw(), 0);
        } else {
            let mut m = vec![M_RELEASE];
            put_u128(&mut m, id.raw());
            self.send_reliable(HOST_PEER, &m);
        }
    }

    /// Hand an object to `to` (current owner, or the host for anything).
    pub fn transfer_ownership(&mut self, id: ObjectId, to: u64) {
        if !self.is_connected() {
            return;
        }
        if self.is_host {
            self.grant(id.raw(), to);
        } else if self.owner_of(id) == self.local_peer {
            let mut m = vec![M_TRANSFER];
            put_u128(&mut m, id.raw());
            put_u64(&mut m, to);
            self.send_reliable(HOST_PEER, &m);
        }
    }

    // ------------------------------------------------------------ spawning

    /// Replicate a spawn the local play session already applied.
    pub fn spawn(&mut self, source: NetSpawnSource, root_id: ObjectId, translation: Vec3, rotation: Quat, owner: u64) {
        let rec = SpawnRecord { id: root_id.raw(), source, translation, rotation, owner };
        self.record_spawn(&rec);
        if self.is_host {
            self.claim_seq += 1;
            let seq = self.claim_seq;
            self.set_owner_local(rec.id, rec.owner, seq);
            self.broadcast_reliable(&enc_spawn(&rec), None);
            self.broadcast_owner(rec.id);
        } else if self.is_connected() {
            // Optimistic: the spawner simulates it until the host says otherwise.
            self.world.owners.insert(rec.id, (rec.owner, 0));
            self.send_reliable(HOST_PEER, &enc_spawn(&rec));
        }
        self.push_event(NetEvent::Spawned { object: root_id, owner: rec.owner });
    }

    /// Host: spawn something on every peer, the host included (player prefabs).
    pub fn spawn_everywhere(&mut self, source: NetSpawnSource, translation: Vec3, rotation: Quat, owner: u64) -> ObjectId {
        let id = ObjectId::new();
        let rec = SpawnRecord { id: id.raw(), source, translation, rotation, owner };
        self.forward_spawn(rec, None);
        id
    }

    fn broadcast_owner(&mut self, id: u128) {
        let (owner, seq) = self.world.owners.get(&id).copied().unwrap_or((0, 0));
        let mut m = vec![M_OWNER];
        put_u128(&mut m, id);
        put_u64(&mut m, owner);
        put_u64(&mut m, seq);
        self.broadcast_reliable(&m, None);
    }

    /// Host: accept a spawn (from a client or itself), apply it locally and
    /// relay it to everyone except `except` (who already has it).
    fn forward_spawn(&mut self, rec: SpawnRecord, except: Option<u64>) {
        self.broadcast_reliable(&enc_spawn(&rec), except);
        self.claim_seq += 1;
        let seq = self.claim_seq;
        let id = rec.id;
        self.apply_spawn_with_seq(rec, seq);
        self.broadcast_owner(id);
    }

    fn record_spawn(&mut self, rec: &SpawnRecord) {
        self.world.spawns.retain(|s| s.id != rec.id);
        self.world.spawns.push(rec.clone());
    }

    fn apply_spawn(&mut self, rec: SpawnRecord) {
        self.apply_spawn_with_seq(rec, 0);
    }

    fn apply_spawn_with_seq(&mut self, rec: SpawnRecord, seq: u64) {
        if self.world.spawns.iter().any(|s| s.id == rec.id) {
            return;
        }
        self.record_spawn(&rec);
        self.set_owner_local(rec.id, rec.owner, seq);
        self.world_ops.push(NetWorldOp::Spawn {
            source: rec.source.clone(),
            root_id: ObjectId::from_raw(rec.id),
            translation: rec.translation,
            rotation: rec.rotation,
            owner: rec.owner,
        });
        self.push_event(NetEvent::Spawned { object: ObjectId::from_raw(rec.id), owner: rec.owner });
    }

    /// Replicate a despawn the local play session already applied.
    pub fn despawn(&mut self, id: ObjectId) {
        let raw = id.raw();
        if !self.is_connected() {
            return;
        }
        let mut d = vec![M_DESPAWN];
        put_u128(&mut d, raw);
        self.upstream_or_broadcast(&d);
        self.world.forget(raw);
        self.buffers.remove(&raw);
        self.send_state.remove(&raw);
        self.push_event(NetEvent::Despawned { object: id });
    }

    /// Host: despawn on every peer, the host included.
    pub fn despawn_raw(&mut self, id: u128) {
        let mut d = vec![M_DESPAWN];
        put_u128(&mut d, id);
        self.broadcast_reliable(&d, None);
        self.apply_despawn(id);
    }

    fn apply_despawn(&mut self, id: u128) {
        self.world.forget(id);
        self.buffers.remove(&id);
        self.send_state.remove(&id);
        self.anim_in.remove(&id);
        self.avatar_in.remove(&id);
        self.face_in.remove(&id);
        self.world_ops.push(NetWorldOp::Despawn(ObjectId::from_raw(id)));
        self.push_event(NetEvent::Despawned { object: ObjectId::from_raw(id) });
    }

    // ------------------------------------------------------------ variables / state

    /// Replicate a synced-variable write the local play session already applied.
    pub fn set_var(&mut self, id: ObjectId, key: &str, value: NetValue) {
        let raw = id.raw();
        self.world.set_var(raw, key, value.clone());
        if self.is_connected() {
            let local = self.local_peer;
            self.upstream_or_broadcast(&enc_var(raw, key, &value, local));
        }
        self.push_event(NetEvent::VarChanged { object: id, key: key.to_owned(), value, from: self.local_peer });
    }

    fn apply_var(&mut self, id: u128, key: String, value: NetValue, from: u64) {
        self.world.set_var(id, &key, value.clone());
        let expires = self.now() + PENDING_TTL;
        self.pending_vars.retain(|p| !(p.id == id && p.key == key));
        self.pending_vars.push(PendingVar { id, key: key.clone(), value: value.clone(), expires });
        self.push_event(NetEvent::VarChanged { object: ObjectId::from_raw(id), key, value, from });
    }

    pub fn set_active(&mut self, id: u128, on: bool) {
        self.world.actives.insert(id, on);
        let mut m = vec![M_ACTIVE];
        put_u128(&mut m, id);
        m.push(on as u8);
        self.upstream_or_broadcast(&m);
    }

    /// `parent` 0 = root.
    pub fn set_parent(&mut self, id: u128, parent: u128) {
        self.world.parents.insert(id, parent);
        let mut m = vec![M_PARENT];
        put_u128(&mut m, id);
        put_u128(&mut m, parent);
        self.upstream_or_broadcast(&m);
    }

    /// Host: switch every peer (and late joiners) to a scene.
    pub fn load_scene(&mut self, path: &str) {
        if !self.is_host {
            return;
        }
        let mut m = vec![M_SCENE];
        put_str(&mut m, path);
        self.broadcast_reliable(&m, None);
        self.clear_world_for_scene(path);
        self.hold_ops_until = 0.0;
    }

    // ------------------------------------------------------------ RPC / text

    pub fn send_rpc(&mut self, object: Option<ObjectId>, name: &str, args: &[NetValue], target: RpcTarget, reliable: bool) {
        let me = self.local_peer;
        let obj = object.map(|o| o.raw()).unwrap_or(0);
        let body = enc_rpc(me, target, obj, name, args);
        let rpc = NetRpc { from: me, object, name: name.to_owned(), args: args.to_vec() };
        let owner = match self.owner_of_raw(obj) {
            0 => HOST_PEER,
            o => o,
        };
        let local = match target {
            RpcTarget::All => true,
            RpcTarget::Others => false,
            RpcTarget::Server => self.is_host,
            RpcTarget::Owner => owner == me,
            RpcTarget::Peer(p) => p == me,
        };
        if local {
            self.rpcs.push(rpc);
        }
        if !self.is_connected() {
            return;
        }
        if self.is_host {
            self.route_rpc(me, target, obj, &body, reliable, None);
        } else if !local || target == RpcTarget::All {
            self.send_rpc_packet(HOST_PEER, &body, reliable);
        }
    }

    fn send_rpc_packet(&mut self, peer: u64, body: &[u8], reliable: bool) {
        if reliable {
            let mut m = Vec::with_capacity(body.len() + 1);
            m.push(M_RPC);
            m.extend_from_slice(body);
            self.send_reliable(peer, &m);
        } else {
            let mut p = Vec::with_capacity(body.len() + 1);
            p.push(P_URPC);
            p.extend_from_slice(body);
            self.send_to_peer(peer, &p);
        }
    }

    /// Host: deliver an RPC to the clients its target names (the host itself
    /// was handled by the caller), skipping `origin`.
    fn route_rpc(&mut self, sender: u64, target: RpcTarget, obj: u128, body: &[u8], reliable: bool, origin: Option<u64>) {
        let clients: Vec<u64> = self.conns.keys().copied().collect();
        let to: Vec<u64> = match target {
            RpcTarget::All | RpcTarget::Others => clients.into_iter().filter(|p| *p != sender).collect(),
            RpcTarget::Server => Vec::new(),
            RpcTarget::Owner => match self.owner_of_raw(obj) {
                0 | HOST_PEER => Vec::new(),
                o if o != sender => vec![o],
                _ => Vec::new(),
            },
            RpcTarget::Peer(p) if p != HOST_PEER && p != sender => vec![p],
            RpcTarget::Peer(_) => Vec::new(),
        };
        for p in to {
            if Some(p) != origin {
                self.send_rpc_packet(p, body, reliable);
            }
        }
    }

    fn on_rpc(&mut self, from: u64, body: &[u8], reliable: bool) {
        let mut c = Cursor::new(body, 0);
        let Some((sender, target, obj, name, args)) = dec_rpc(&mut c) else {
            return;
        };
        let sender = if self.is_host { from } else { sender };
        let rpc = NetRpc {
            from: sender,
            object: (obj != 0).then(|| ObjectId::from_raw(obj)),
            name,
            args,
        };
        if self.is_host {
            let owner = match self.owner_of_raw(obj) {
                0 => HOST_PEER,
                o => o,
            };
            let to_host = match target {
                RpcTarget::All | RpcTarget::Others | RpcTarget::Server => true,
                RpcTarget::Owner => owner == HOST_PEER,
                RpcTarget::Peer(p) => p == HOST_PEER,
            };
            let fwd = enc_rpc(sender, target, obj, &rpc.name, &rpc.args);
            self.route_rpc(sender, target, obj, &fwd, reliable, Some(from));
            if to_host {
                self.rpcs.push(rpc);
            }
        } else {
            self.rpcs.push(rpc);
        }
        if self.rpcs.len() > MAX_QUEUE {
            self.rpcs.remove(0);
        }
    }

    /// Send a text message: `to = None` broadcasts (public), `Some(peer)` is
    /// private. Routed through the host; never echoed to the sender.
    pub fn send_message(&mut self, to: Option<u64>, text: &str) {
        if !self.is_connected() {
            return;
        }
        let target = to.unwrap_or(0);
        if self.is_host {
            let me = self.local_peer;
            self.route_text(target, me, text, None);
        } else {
            let mut m = vec![M_TEXT];
            put_u64(&mut m, target);
            put_u64(&mut m, self.local_peer);
            put_str(&mut m, text);
            self.send_reliable(HOST_PEER, &m);
        }
    }

    fn route_text(&mut self, target: u64, sender: u64, text: &str, origin: Option<u64>) {
        let mut m = vec![M_TEXT];
        put_u64(&mut m, target);
        put_u64(&mut m, sender);
        put_str(&mut m, text);
        if target == 0 {
            let to: Vec<u64> = self.conns.keys().copied().filter(|p| *p != sender && Some(*p) != origin).collect();
            for p in to {
                self.send_reliable(p, &m);
            }
            if sender != self.local_peer {
                self.inbox.push((sender, false, text.to_owned()));
            }
        } else if target == self.local_peer {
            self.inbox.push((sender, true, text.to_owned()));
        } else {
            self.send_reliable(target, &m);
        }
    }

    // ------------------------------------------------------------ unreliable state

    /// Send transform entries for objects the local peer has authority over.
    /// The host filters by relevancy per client.
    pub fn send_snapshot(&mut self, entries: &[SnapEntry]) {
        if entries.is_empty() || !self.is_connected() {
            return;
        }
        let now = self.now();
        let origin = self.local_peer;
        if self.is_host {
            let clients: Vec<u64> = self.conns.keys().copied().collect();
            for c in clients {
                let list: Vec<SnapEntry> = entries.iter().copied().filter(|e| self.relevant(e.id, c)).collect();
                for p in build_snap_packets(origin, now, &list) {
                    self.send_to_peer(c, &p);
                }
            }
        } else {
            for p in build_snap_packets(origin, now, entries) {
                self.send_to_peer(HOST_PEER, &p);
            }
        }
    }

    /// Host-side interest management: is `id` worth sending to `peer`?
    fn relevant(&self, id: u128, peer: u64) -> bool {
        let range = self.world.relevancy.get(&id).copied().unwrap_or(0.0);
        if range <= 0.0 || self.owner_of_raw(id) == peer {
            return true;
        }
        let Some(player) = self.world.player_obj.get(&peer) else {
            return true;
        };
        match (self.world.xforms.get(&id), self.world.xforms.get(player)) {
            (Some(a), Some(b)) => a.pos.distance(b.pos) <= range,
            _ => true,
        }
    }

    fn on_snap(&mut self, peer: u64, data: &[u8]) {
        let mut c = Cursor::new(data, 1);
        let origin = c.u64();
        let time = c.f64();
        let n = c.u8();
        let mut entries = Vec::with_capacity(n as usize);
        for _ in 0..n {
            match dec_entry(&mut c) {
                Some(e) => entries.push(e),
                None => return,
            }
        }
        let origin = if self.is_host { peer } else { origin };
        if self.is_host {
            // Only the owner may move an object.
            entries.retain(|e| self.owner_of_raw(e.id) == peer);
        }
        let now = self.now();
        let sample = now - time;
        let off = self.offsets.entry(origin).or_insert(sample);
        if sample < *off {
            *off = sample;
        } else {
            *off += (sample - *off) * 0.02;
        }
        for e in &entries {
            if self.has_authority_raw(e.id) {
                continue;
            }
            self.insert_snapshot(origin, time, e);
        }
        if self.is_host && !entries.is_empty() {
            let clients: Vec<u64> = self.conns.keys().copied().filter(|p| *p != peer).collect();
            for cl in clients {
                let list: Vec<SnapEntry> = entries.iter().copied().filter(|e| self.relevant(e.id, cl)).collect();
                for p in build_snap_packets(origin, time, &list) {
                    self.send_to_peer(cl, &p);
                }
            }
        }
    }

    fn insert_snapshot(&mut self, origin: u64, time: f64, e: &SnapEntry) {
        let base = self.world.xforms.get(&e.id).copied().unwrap_or_default();
        let buf = self.buffers.entry(e.id).or_default();
        if buf.origin != origin || buf.baseline {
            buf.snaps.clear();
            buf.origin = origin;
            buf.baseline = false;
        }
        if buf.snaps.iter().any(|s| s.time == time)
            || (buf.snaps.len() >= SNAP_KEEP && buf.snaps.front().is_some_and(|s| time < s.time))
        {
            return;
        }
        let prev = buf.snaps.back().copied();
        let from = prev.map(|p| XformState { pos: p.pos, rot: p.rot, scale: p.scale, vel: p.vel }).unwrap_or(base);
        let s = Snap {
            time,
            pos: e.pos.unwrap_or(from.pos),
            rot: e.rot.unwrap_or(from.rot),
            scale: e.scale.unwrap_or(from.scale),
            vel: e.vel.unwrap_or(Vec3::ZERO),
        };
        let at = buf.snaps.iter().position(|x| x.time > time).unwrap_or(buf.snaps.len());
        buf.snaps.insert(at, s);
        while buf.snaps.len() > SNAP_KEEP {
            buf.snaps.pop_front();
        }
        let newest = *buf.snaps.back().unwrap();
        self.world.xforms.insert(e.id, XformState { pos: newest.pos, rot: newest.rot, scale: newest.scale, vel: newest.vel });
        self.xform_seq += 1;
    }

    /// Latest remote transform for an object we don't own.
    pub fn remote_transform(&self, id: ObjectId) -> Option<RemoteTransform> {
        let s = self.buffers.get(&id.raw())?.snaps.back()?;
        Some(RemoteTransform { translation: s.pos, rotation: s.rot, scale: s.scale, seq: self.xform_seq })
    }

    /// Send the transform of an object the local peer owns to all other peers.
    pub fn send_transform(&mut self, id: ObjectId, t: Vec3, r: Quat, s: Vec3) {
        self.world.xforms.insert(id.raw(), XformState { pos: t, rot: r, scale: s, vel: Vec3::ZERO });
        self.send_snapshot(&[SnapEntry { id: id.raw(), pos: Some(t), rot: Some(r), scale: Some(s), vel: None, quantized: false }]);
    }

    /// Animator parameters of an object the local peer has authority over.
    pub fn send_anim(&mut self, id: u128, params: &[(String, f32)]) {
        if params.is_empty() || !self.is_connected() {
            return;
        }
        let anims = self.world.anims.entry(id).or_default();
        for (k, v) in params {
            anims.insert(k.clone(), *v);
        }
        let mut b = vec![P_ANIM];
        put_u64(&mut b, self.local_peer);
        put_u128(&mut b, id);
        let n = params.len().min(64);
        b.push(n as u8);
        for (k, v) in &params[..n] {
            put_str(&mut b, k);
            put_f32(&mut b, *v);
        }
        if b.len() <= MTU {
            self.fanout(&b, None);
        }
    }

    fn on_anim(&mut self, peer: u64, data: &[u8]) {
        let mut c = Cursor::new(data, 1);
        let _origin = c.u64();
        let id = c.u128();
        let n = c.u8();
        let mut params = Vec::new();
        for _ in 0..n {
            let k = c.string();
            let v = c.f32();
            params.push((k, v));
        }
        if c.bad || (self.is_host && self.owner_of_raw(id) != peer) || self.has_authority_raw(id) {
            return;
        }
        let stored = self.world.anims.entry(id).or_default();
        let live = self.anim_in.entry(id).or_default();
        for (k, v) in params {
            stored.insert(k.clone(), v);
            live.insert(k, v);
        }
        if self.is_host {
            self.fanout(data, Some(peer));
        }
    }

    /// Avatar part poses (world space, wire order) of an object we have authority over.
    pub fn send_avatar(&mut self, id: u128, poses: &[Option<(Vec3, Quat)>; TRACKER_SLOTS]) {
        if !self.is_connected() {
            return;
        }
        let mut b = vec![P_AVATAR];
        put_u64(&mut b, self.local_peer);
        put_f64(&mut b, self.now());
        put_u128(&mut b, id);
        let mask = poses.iter().enumerate().fold(0u16, |m, (i, p)| if p.is_some() { m | (1 << i) } else { m });
        put_u16(&mut b, mask);
        for (p, r) in poses.iter().flatten() {
            put_vec3(&mut b, *p);
            put_rot32(&mut b, *r);
        }
        self.fanout(&b, None);
    }

    fn on_avatar(&mut self, peer: u64, data: &[u8]) {
        let mut c = Cursor::new(data, 1);
        let origin = c.u64();
        let time = c.f64();
        let id = c.u128();
        let mask = c.u16();
        let mut poses = [None; TRACKER_SLOTS];
        for (i, slot) in poses.iter_mut().enumerate() {
            if mask & (1 << i) != 0 {
                let p = c.vec3();
                let r = c.rot32();
                *slot = Some((p, r));
            }
        }
        if c.bad || (self.is_host && self.owner_of_raw(id) != peer) || self.has_authority_raw(id) {
            return;
        }
        let origin = if self.is_host { peer } else { origin };
        let now = self.now();
        let sample = now - time;
        let off = self.offsets.entry(origin).or_insert(sample);
        if sample < *off {
            *off = sample;
        }
        let buf = self.avatar_in.entry(id).or_default();
        if buf.origin != origin {
            buf.snaps.clear();
            buf.origin = origin;
        }
        let at = buf.snaps.iter().position(|x| x.time > time).unwrap_or(buf.snaps.len());
        buf.snaps.insert(at, AvatarSnap { time, poses });
        while buf.snaps.len() > SNAP_KEEP {
            buf.snaps.pop_front();
        }
        if self.is_host {
            self.fanout(data, Some(peer));
        }
    }

    /// Face tracking of an avatar we have authority over.
    pub fn send_face(&mut self, id: u128, face: &citrus_core::FaceFrame) {
        if !self.is_connected() {
            return;
        }
        let mut b = vec![P_FACE];
        put_u64(&mut b, self.local_peer);
        put_f64(&mut b, self.now());
        put_u128(&mut b, id);
        face.encode(&mut b);
        if b.len() <= MTU {
            self.fanout(&b, None);
        }
    }

    fn on_face(&mut self, peer: u64, data: &[u8]) {
        let mut c = Cursor::new(data, 1);
        let _origin = c.u64();
        let time = c.f64();
        let id = c.u128();
        if c.bad || (self.is_host && self.owner_of_raw(id) != peer) || self.has_authority_raw(id) {
            return;
        }
        let Some((frame, _)) = data.get(1 + 8 + 8 + 16..).and_then(citrus_core::FaceFrame::decode) else {
            return;
        };
        if self.face_in.get(&id).is_some_and(|(t, _)| *t > time) {
            return;
        }
        self.face_in.insert(id, (time, frame));
        if self.is_host {
            self.fanout(data, Some(peer));
        }
    }

    /// Send one encoded voice frame to everyone (relayed by the host).
    pub fn send_voice(&mut self, seq: u32, payload: &[u8]) {
        if !self.is_connected() || payload.len() > MTU - 20 {
            return;
        }
        let mut b = vec![P_VOICE];
        put_u64(&mut b, self.local_peer);
        put_u32(&mut b, seq);
        put_u16(&mut b, payload.len() as u16);
        b.extend_from_slice(payload);
        self.fanout(&b, None);
    }

    /// Render time (sender clock) for a buffered object from `origin`.
    /// The host's clock as this peer estimates it (one-way delay corrected
    /// with half the round trip): a time base every peer agrees on, for
    /// things each client simulates itself, like the water. None until the
    /// host has been heard from.
    pub fn shared_clock(&self) -> Option<f64> {
        if self.is_host {
            return Some(self.now());
        }
        let off = self.offsets.get(&HOST_PEER)?;
        let rtt = self.conns.get(&HOST_PEER).map_or(0.0, |c| c.rtt as f64);
        Some(self.now() - off + rtt * 0.5)
    }

    pub fn render_time(&self, origin: u64, delay: f32) -> f64 {
        let off = self.offsets.get(&origin).copied().unwrap_or(0.0);
        self.now() - off - delay as f64
    }
}

impl Drop for NetSession {
    fn drop(&mut self) {
        if self.is_connected() {
            self.disconnect();
        }
    }
}

// ------------------------------------------------------------------ sampling

/// Transform at `render_time` from a snapshot buffer.
pub fn sample_snapshots(buf: &SnapBuffer, render_time: f64, mode: InterpolationMode) -> Option<Snap> {
    let newest = *buf.snaps.back()?;
    if buf.baseline || mode == InterpolationMode::None || buf.snaps.len() == 1 {
        return Some(newest);
    }
    if render_time >= newest.time {
        if mode == InterpolationMode::Extrapolate {
            let ahead = (render_time - newest.time).min(0.25) as f32;
            return Some(Snap { pos: newest.pos + newest.vel * ahead, ..newest });
        }
        return Some(newest);
    }
    let oldest = buf.snaps[0];
    if render_time <= oldest.time {
        return Some(oldest);
    }
    let i = buf.snaps.iter().position(|s| s.time > render_time)?;
    let (a, b) = (buf.snaps[i - 1], buf.snaps[i]);
    let t = ((render_time - a.time) / (b.time - a.time).max(1e-6)) as f32;
    Some(Snap {
        time: render_time,
        pos: a.pos.lerp(b.pos, t),
        rot: a.rot.slerp(b.rot, t),
        scale: a.scale.lerp(b.scale, t),
        vel: a.vel.lerp(b.vel, t),
    })
}

/// Avatar poses at `render_time`.
pub fn sample_avatar(buf: &AvatarBuffer, render_time: f64) -> Option<[Option<(Vec3, Quat)>; TRACKER_SLOTS]> {
    let newest = buf.snaps.back()?;
    if render_time >= newest.time || buf.snaps.len() == 1 {
        return Some(newest.poses);
    }
    let Some(i) = buf.snaps.iter().position(|s| s.time > render_time) else {
        return Some(newest.poses);
    };
    if i == 0 {
        return Some(buf.snaps[0].poses);
    }
    let (a, b) = (&buf.snaps[i - 1], &buf.snaps[i]);
    let t = ((render_time - a.time) / (b.time - a.time).max(1e-6)) as f32;
    let mut out = [None; TRACKER_SLOTS];
    for ((slot, pa), pb) in out.iter_mut().zip(a.poses).zip(b.poses) {
        *slot = match (pa, pb) {
            (Some((pa, ra)), Some((pb, rb))) => Some((pa.lerp(pb, t), ra.slerp(rb, t))),
            (_, b) => b,
        };
    }
    Some(out)
}

// ------------------------------------------------------------------ LAN discovery

/// A session found on the LAN.
#[derive(Clone, Debug)]
pub struct LanSession {
    pub addr: SocketAddr,
    pub name: String,
    pub players: u32,
    pub max_players: u32,
    last_seen: Instant,
}

/// Finds hosts on the local network by UDP broadcast.
pub struct LanBrowser {
    socket: UdpSocket,
    port: u16,
    sessions: Vec<LanSession>,
}

impl LanBrowser {
    pub fn new() -> std::io::Result<Self> {
        Self::with_port(DISCOVERY_PORT)
    }

    pub fn with_port(port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0))?;
        socket.set_broadcast(true)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket, port, sessions: Vec::new() })
    }

    /// Broadcast a discovery query (call every couple of seconds while browsing).
    pub fn refresh(&mut self) {
        let mut b = vec![P_DISCOVER];
        put_u16(&mut b, PROTOCOL);
        let _ = self.socket.send_to(&b, ("255.255.255.255", self.port));
        let _ = self.socket.send_to(&b, ("127.0.0.1", self.port));
    }

    /// Collect replies; returns every session seen in the last few seconds.
    pub fn poll(&mut self) -> &[LanSession] {
        let mut buf = [0u8; 512];
        while let Ok((n, src)) = self.socket.recv_from(&mut buf) {
            let mut c = Cursor::new(&buf[..n], 0);
            if c.u8() != P_INFO || c.u16() != PROTOCOL {
                continue;
            }
            let port = c.u16();
            let players = c.u32();
            let max_players = c.u32();
            let name = c.string();
            if c.bad {
                continue;
            }
            let addr = SocketAddr::new(src.ip(), port);
            self.sessions.retain(|s| s.addr != addr);
            self.sessions.push(LanSession { addr, name, players, max_players, last_seen: Instant::now() });
        }
        self.sessions.retain(|s| s.last_seen.elapsed().as_secs_f32() < 6.0);
        &self.sessions
    }
}

pub fn parse_addr(addr: &str) -> std::io::Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    if let Ok(a) = addr.parse::<SocketAddr>() {
        return Ok(a);
    }
    addr.to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad address {addr:?}")))
}

// ------------------------------------------------------------------ message codecs

fn enc_spawn(s: &SpawnRecord) -> Vec<u8> {
    let mut m = vec![M_SPAWN];
    put_u128(&mut m, s.id);
    match &s.source {
        NetSpawnSource::Prefab(p) => {
            m.push(0);
            put_str(&mut m, p);
        }
        NetSpawnSource::Clone(id) => {
            m.push(1);
            put_u128(&mut m, id.raw());
        }
    }
    put_vec3(&mut m, s.translation);
    put_quat(&mut m, s.rotation);
    put_u64(&mut m, s.owner);
    m
}

fn dec_spawn(c: &mut Cursor) -> Option<SpawnRecord> {
    let id = c.u128();
    let source = match c.u8() {
        0 => NetSpawnSource::Prefab(c.string()),
        _ => NetSpawnSource::Clone(ObjectId::from_raw(c.u128())),
    };
    let translation = c.vec3();
    let rotation = c.quat();
    let owner = c.u64();
    (!c.bad).then_some(SpawnRecord { id, source, translation, rotation, owner })
}

fn enc_var(id: u128, key: &str, v: &NetValue, from: u64) -> Vec<u8> {
    let mut m = vec![M_VAR];
    put_u128(&mut m, id);
    put_str(&mut m, key);
    put_value(&mut m, v);
    put_u64(&mut m, from);
    m
}

fn enc_rpc(from: u64, target: RpcTarget, obj: u128, name: &str, args: &[NetValue]) -> Vec<u8> {
    let mut b = Vec::new();
    put_u64(&mut b, from);
    match target {
        RpcTarget::All => b.push(0),
        RpcTarget::Others => b.push(1),
        RpcTarget::Server => b.push(2),
        RpcTarget::Owner => b.push(3),
        RpcTarget::Peer(p) => {
            b.push(4);
            put_u64(&mut b, p);
        }
    }
    put_u128(&mut b, obj);
    put_str(&mut b, name);
    b.push(args.len().min(255) as u8);
    for a in args.iter().take(255) {
        put_value(&mut b, a);
    }
    b
}

type RpcParts = (u64, RpcTarget, u128, String, Vec<NetValue>);

fn dec_rpc(c: &mut Cursor) -> Option<RpcParts> {
    let from = c.u64();
    let target = match c.u8() {
        0 => RpcTarget::All,
        1 => RpcTarget::Others,
        2 => RpcTarget::Server,
        3 => RpcTarget::Owner,
        _ => RpcTarget::Peer(c.u64()),
    };
    let obj = c.u128();
    let name = c.string();
    let n = c.u8();
    let mut args = Vec::with_capacity(n as usize);
    for _ in 0..n {
        args.push(c.value());
    }
    (!c.bad).then_some((from, target, obj, name, args))
}

const E_POS: u8 = 1;
const E_ROT: u8 = 2;
const E_SCALE: u8 = 4;
const E_VEL: u8 = 8;
const E_QUANT: u8 = 16;

pub fn enc_entry(b: &mut Vec<u8>, e: &SnapEntry) {
    put_u128(b, e.id);
    let mut flags = 0;
    if e.pos.is_some() {
        flags |= E_POS;
    }
    if e.rot.is_some() {
        flags |= E_ROT;
    }
    if e.scale.is_some() {
        flags |= E_SCALE;
    }
    if e.vel.is_some() {
        flags |= E_VEL;
    }
    if e.quantized {
        flags |= E_QUANT;
    }
    b.push(flags);
    if let Some(p) = e.pos {
        if e.quantized {
            for v in p.to_array() {
                put_i24(b, (v * 1000.0).round().clamp(-8_388_607.0, 8_388_607.0) as i32);
            }
        } else {
            put_vec3(b, p);
        }
    }
    if let Some(r) = e.rot {
        if e.quantized {
            put_rot32(b, r);
        } else {
            put_rot56(b, r);
        }
    }
    if let Some(s) = e.scale {
        put_vec3(b, s);
    }
    if let Some(v) = e.vel {
        put_vec3(b, v);
    }
}

pub fn dec_entry(c: &mut Cursor) -> Option<SnapEntry> {
    let id = c.u128();
    let flags = c.u8();
    let q = flags & E_QUANT != 0;
    let pos = (flags & E_POS != 0).then(|| {
        if q {
            Vec3::new(c.i24() as f32 / 1000.0, c.i24() as f32 / 1000.0, c.i24() as f32 / 1000.0)
        } else {
            c.vec3()
        }
    });
    let rot = (flags & E_ROT != 0).then(|| if q { c.rot32() } else { c.rot56() });
    let scale = (flags & E_SCALE != 0).then(|| c.vec3());
    let vel = (flags & E_VEL != 0).then(|| c.vec3());
    (!c.bad).then_some(SnapEntry { id, pos, rot, scale, vel, quantized: q })
}

fn entry_size(e: &SnapEntry) -> usize {
    17 + e.pos.map_or(0, |_| if e.quantized { 9 } else { 12 })
        + e.rot.map_or(0, |_| if e.quantized { 4 } else { 7 })
        + e.scale.map_or(0, |_| 12)
        + e.vel.map_or(0, |_| 12)
}

/// Batch entries into as few packets as fit the MTU.
pub fn build_snap_packets(origin: u64, time: f64, entries: &[SnapEntry]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let mut b = vec![P_SNAP];
        put_u64(&mut b, origin);
        put_f64(&mut b, time);
        let count_at = b.len();
        b.push(0);
        let mut n = 0u8;
        while i < entries.len() && n < 255 && b.len() + entry_size(&entries[i]) <= MTU {
            enc_entry(&mut b, &entries[i]);
            n += 1;
            i += 1;
        }
        if n == 0 {
            i += 1; // cannot happen with sane entries; avoid looping forever
            continue;
        }
        b[count_at] = n;
        out.push(b);
    }
    out
}

// ------------------------------------------------------------------ wire helpers

fn put_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u128(b: &mut Vec<u8>, v: u128) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_f32(b: &mut Vec<u8>, v: f32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_f64(b: &mut Vec<u8>, v: f64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_i24(b: &mut Vec<u8>, v: i32) {
    b.extend_from_slice(&v.to_le_bytes()[..3]);
}
fn put_str(b: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(u16::MAX as usize);
    put_u16(b, n as u16);
    b.extend_from_slice(&bytes[..n]);
}
fn put_vec3(b: &mut Vec<u8>, v: Vec3) {
    put_f32(b, v.x);
    put_f32(b, v.y);
    put_f32(b, v.z);
}
fn put_quat(b: &mut Vec<u8>, v: Quat) {
    put_f32(b, v.x);
    put_f32(b, v.y);
    put_f32(b, v.z);
    put_f32(b, v.w);
}

/// Smallest-three: drop the largest component (recomputed on decode).
fn smallest_three(r: Quat) -> (usize, [f32; 3]) {
    let mut q = r.normalize().to_array();
    let (big, _) = q.iter().enumerate().fold((0, 0.0f32), |(bi, bv), (i, v)| if v.abs() > bv { (i, v.abs()) } else { (bi, bv) });
    if q[big] < 0.0 {
        q = q.map(|v| -v);
    }
    let mut rest = [0.0; 3];
    let mut k = 0;
    for (i, v) in q.iter().enumerate() {
        if i != big {
            rest[k] = *v;
            k += 1;
        }
    }
    (big, rest)
}

fn from_smallest_three(big: usize, rest: [f32; 3]) -> Quat {
    let w = (1.0 - rest.iter().map(|v| v * v).sum::<f32>()).max(0.0).sqrt();
    let mut q = [0.0; 4];
    let mut k = 0;
    for (i, slot) in q.iter_mut().enumerate() {
        if i == big {
            *slot = w;
        } else {
            *slot = rest[k];
            k += 1;
        }
    }
    Quat::from_array(q).normalize()
}

const RMAX: f32 = std::f32::consts::FRAC_1_SQRT_2;

fn put_rot32(b: &mut Vec<u8>, r: Quat) {
    let (big, rest) = smallest_three(r);
    let mut v = big as u32;
    for c in rest {
        let q = (((c / RMAX).clamp(-1.0, 1.0) * 0.5 + 0.5) * 1023.0).round() as u32;
        v = (v << 10) | q;
    }
    put_u32(b, v);
}

fn put_rot56(b: &mut Vec<u8>, r: Quat) {
    let (big, rest) = smallest_three(r);
    b.push(big as u8);
    for c in rest {
        let q = ((c / RMAX).clamp(-1.0, 1.0) * 32767.0).round() as i16;
        b.extend_from_slice(&q.to_le_bytes());
    }
}

fn put_value(b: &mut Vec<u8>, v: &NetValue) {
    match v {
        NetValue::Bool(x) => {
            b.push(0);
            b.push(*x as u8);
        }
        NetValue::Int(x) => {
            b.push(1);
            b.extend_from_slice(&x.to_le_bytes());
        }
        NetValue::Float(x) => {
            b.push(2);
            put_f32(b, *x);
        }
        NetValue::Vec3(x) => {
            b.push(3);
            for c in x {
                put_f32(b, *c);
            }
        }
        NetValue::Quat(x) => {
            b.push(4);
            for c in x {
                put_f32(b, *c);
            }
        }
        NetValue::Str(s) => {
            b.push(5);
            put_str(b, s);
        }
        NetValue::Bytes(d) => {
            b.push(6);
            put_u32(b, d.len() as u32);
            b.extend_from_slice(d);
        }
    }
}

/// Bounds-checked reader: short reads yield zeros and set `bad`.
pub struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
    pub bad: bool,
}

impl<'a> Cursor<'a> {
    pub fn new(d: &'a [u8], p: usize) -> Self {
        Self { d, p, bad: false }
    }
    fn take(&mut self, n: usize) -> &'a [u8] {
        match self.d.get(self.p..self.p + n) {
            Some(s) => {
                self.p += n;
                s
            }
            None => {
                self.bad = true;
                self.p = self.d.len();
                &[]
            }
        }
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = self.d.get(self.p..).unwrap_or(&[]);
        self.p = self.d.len();
        s
    }
    fn arr<const N: usize>(&mut self) -> [u8; N] {
        self.take(N).try_into().unwrap_or([0; N])
    }
    fn u8(&mut self) -> u8 {
        self.arr::<1>()[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.arr())
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.arr())
    }
    fn i16(&mut self) -> i16 {
        i16::from_le_bytes(self.arr())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.arr())
    }
    fn u128(&mut self) -> u128 {
        u128::from_le_bytes(self.arr())
    }
    fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.arr())
    }
    fn f64(&mut self) -> f64 {
        f64::from_le_bytes(self.arr())
    }
    fn i24(&mut self) -> i32 {
        let b: [u8; 3] = self.arr();
        (i32::from_le_bytes([b[0], b[1], b[2], 0]) << 8) >> 8
    }
    fn string(&mut self) -> String {
        let n = self.u16() as usize;
        String::from_utf8_lossy(self.take(n)).into_owned()
    }
    fn vec3(&mut self) -> Vec3 {
        Vec3::new(self.f32(), self.f32(), self.f32())
    }
    fn quat(&mut self) -> Quat {
        Quat::from_xyzw(self.f32(), self.f32(), self.f32(), self.f32())
    }
    fn rot32(&mut self) -> Quat {
        let v = self.u32();
        let big = (v >> 30) as usize & 3;
        let mut rest = [0.0; 3];
        for (k, slot) in rest.iter_mut().enumerate() {
            let q = (v >> (10 * (2 - k))) & 1023;
            *slot = ((q as f32 / 1023.0) * 2.0 - 1.0) * RMAX;
        }
        from_smallest_three(big, rest)
    }
    fn rot56(&mut self) -> Quat {
        let big = (self.u8() & 3) as usize;
        let mut rest = [0.0; 3];
        for slot in rest.iter_mut() {
            *slot = self.i16() as f32 / 32767.0 * RMAX;
        }
        from_smallest_three(big, rest)
    }
    fn value(&mut self) -> NetValue {
        match self.u8() {
            0 => NetValue::Bool(self.u8() != 0),
            1 => NetValue::Int(i64::from_le_bytes(self.arr())),
            2 => NetValue::Float(self.f32()),
            3 => NetValue::Vec3([self.f32(), self.f32(), self.f32()]),
            4 => NetValue::Quat([self.f32(), self.f32(), self.f32(), self.f32()]),
            5 => NetValue::Str(self.string()),
            6 => {
                let n = self.u32() as usize;
                NetValue::Bytes(self.take(n).to_vec())
            }
            _ => {
                self.bad = true;
                NetValue::Int(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 60.0;

    fn cfg(name: &str) -> NetConfig {
        NetConfig { player_name: name.to_string(), discovery: false, ..Default::default() }
    }

    fn host(hub: &MemHub) -> NetSession {
        let mut h = NetSession::host_on(Box::new(hub.endpoint()), cfg("Host"));
        h.use_manual_clock();
        h
    }

    fn client(hub: &MemHub, host: &NetSession, name: &str) -> NetSession {
        let mut c = NetSession::join_on(Box::new(hub.endpoint()), host.local_addr().unwrap(), cfg(name));
        c.use_manual_clock();
        c
    }

    fn step(all: &mut [&mut NetSession], frames: usize) {
        for _ in 0..frames {
            for s in all.iter_mut() {
                s.advance(DT);
                s.pump();
            }
        }
    }

    fn events(s: &mut NetSession) -> Vec<NetEvent> {
        s.view().events
    }

    #[test]
    fn join_leave_timeout_and_player_list() {
        let hub = MemHub::new();
        let mut h = host(&hub);
        let mut a = client(&hub, &h, "Ann");
        let mut b = client(&hub, &h, "Bob");
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert_eq!((a.local_peer(), b.local_peer()), (2, 3));
        for s in [&h, &a, &b] {
            let names: Vec<String> = s.peer_infos().into_iter().map(|p| p.name).collect();
            assert_eq!(names, vec!["Host", "Ann", "Bob"], "everyone sees the same player list");
        }
        assert!(events(&mut h).iter().any(|e| matches!(e, NetEvent::PeerJoined { id: 3, name } if name == "Bob")));
        assert!(events(&mut a).iter().any(|e| matches!(e, NetEvent::PeerJoined { id: 3, .. })));

        // Clean leave.
        b.disconnect();
        step(&mut [&mut h, &mut a], 10);
        assert!(events(&mut h).iter().any(|e| matches!(e, NetEvent::PeerLeft { id: 3, .. })));
        assert!(events(&mut a).iter().any(|e| matches!(e, NetEvent::PeerLeft { id: 3, .. })));
        assert_eq!(a.peer_count(), 2);

        // A vanished client times out on the host, and notices itself.
        hub.cut(a.local_addr().unwrap());
        step(&mut [&mut h, &mut a], (11.0 / DT) as usize);
        assert!(events(&mut h).iter().any(|e| matches!(e, NetEvent::PeerLeft { id: 2, reason } if reason == "timed out")));
        assert!(matches!(a.state(), ConnState::Disconnected(_)));
        assert_eq!(h.peer_count(), 1);
    }

    #[test]
    fn full_session_rejects() {
        let hub = MemHub::new();
        let mut h = NetSession::host_on(Box::new(hub.endpoint()), NetConfig { max_players: 2, ..cfg("Host") });
        h.use_manual_clock();
        let mut a = client(&hub, &h, "A");
        let mut b = client(&hub, &h, "B");
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert!(a.is_connected());
        assert!(matches!(b.state(), ConnState::Disconnected(r) if r.contains("full")));
    }

    #[test]
    fn ownership_takeover_request_and_fixed() {
        let hub = MemHub::new();
        let mut h = host(&hub);
        let mut a = client(&hub, &h, "A");
        let mut b = client(&hub, &h, "B");
        step(&mut [&mut h, &mut a, &mut b], 10);
        let id = ObjectId::new();

        // Takeover (default): last claim wins.
        a.request_ownership(id);
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert!(a.owns(id) && b.owner_of(id) == 2 && h.owner_of(id) == 2);
        b.request_ownership(id);
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert!(b.owns(id) && !a.owns(id));
        assert!(events(&mut a).iter().any(|e| matches!(e, NetEvent::OwnershipChanged { owner: 3, previous: 2, .. })));

        // Request: the owner is asked and decides.
        h.world.modes.insert(id.raw(), OwnershipMode::Request);
        a.request_ownership(id);
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert!(b.owns(id), "request mode does not steal");
        assert!(events(&mut b).iter().any(|e| matches!(e, NetEvent::OwnershipRequested { by: 2, .. })));
        assert!(events(&mut a).iter().any(|e| matches!(e, NetEvent::OwnershipDenied { .. })));
        b.transfer_ownership(id, 2);
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert!(a.owns(id), "owner handed it over");
        a.release_ownership(id);
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert_eq!(h.owner_of(id), 0);
        assert!(h.has_authority_raw(id.raw()), "unowned objects fall back to the host");

        // Fixed: clients can't claim.
        h.world.modes.insert(id.raw(), OwnershipMode::Fixed);
        a.request_ownership(id);
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert_eq!(h.owner_of(id), 0);
    }

    #[test]
    fn reliable_channel_survives_loss_and_fragments() {
        let hub = MemHub::new();
        let mut h = host(&hub);
        let mut a = client(&hub, &h, "A");
        step(&mut [&mut h, &mut a], 10);
        hub.set_loss(0.3);
        for i in 0..40 {
            a.send_message(None, &format!("m{i}"));
        }
        let big = NetValue::Bytes((0..5000u32).map(|i| (i % 251) as u8).collect());
        let id = ObjectId::new();
        a.request_ownership(id);
        step(&mut [&mut h, &mut a], 60);
        a.set_var(id, "blob", big.clone());
        step(&mut [&mut h, &mut a], 240);
        let v = h.view();
        let got: Vec<&str> = v.messages.iter().map(|(_, _, t)| t.as_str()).collect();
        let want: Vec<String> = (0..40).map(|i| format!("m{i}")).collect();
        assert_eq!(got, want.iter().map(|s| s.as_str()).collect::<Vec<_>>(), "all delivered, in order");
        assert!(v.events.iter().any(|e| matches!(e, NetEvent::VarChanged { value, .. } if *value == big)), "5 kB value reassembled");
        assert!(h.stats.resends + a.stats.resends > 0, "loss forced resends");
    }

    #[test]
    fn synced_variables_owner_write() {
        let hub = MemHub::new();
        let mut h = host(&hub);
        let mut a = client(&hub, &h, "A");
        let mut b = client(&hub, &h, "B");
        step(&mut [&mut h, &mut a, &mut b], 10);
        let id = ObjectId::new();
        a.request_ownership(id);
        step(&mut [&mut h, &mut a, &mut b], 10);
        a.set_var(id, "hp", NetValue::Int(75));
        step(&mut [&mut h, &mut a, &mut b], 10);
        for s in [&mut h, &mut b] {
            assert!(events(s).iter().any(|e| matches!(e, NetEvent::VarChanged { key, value: NetValue::Int(75), from: 2, .. } if key == "hp")));
            assert!(s.pending_vars.iter().any(|p| p.key == "hp"), "queued for the scene");
        }
        // A non-owner write is dropped by the host.
        b.set_var(id, "hp", NetValue::Int(0));
        step(&mut [&mut h, &mut a, &mut b], 10);
        assert!(!events(&mut a).iter().any(|e| matches!(e, NetEvent::VarChanged { value: NetValue::Int(0), .. })));
        assert_eq!(h.world.vars[&id.raw()][0].1, NetValue::Int(75));
    }

    #[test]
    fn rpc_targets() {
        let hub = MemHub::new();
        let mut h = host(&hub);
        let mut a = client(&hub, &h, "A");
        let mut b = client(&hub, &h, "B");
        step(&mut [&mut h, &mut a, &mut b], 10);
        let obj = ObjectId::new();
        b.request_ownership(obj);
        step(&mut [&mut h, &mut a, &mut b], 10);
        let names = |s: &mut NetSession| -> Vec<String> { s.view().rpcs.into_iter().map(|r| r.name).collect() };
        for s in [&mut h, &mut a, &mut b] {
            names(s);
        }
        a.send_rpc(None, "all", &[NetValue::Int(1)], RpcTarget::All, true);
        a.send_rpc(None, "others", &[], RpcTarget::Others, true);
        a.send_rpc(None, "server", &[], RpcTarget::Server, true);
        a.send_rpc(Some(obj), "owner", &[], RpcTarget::Owner, true);
        a.send_rpc(None, "peer3", &[], RpcTarget::Peer(3), false);
        h.send_rpc(None, "fromhost", &[NetValue::from("hi")], RpcTarget::Others, true);
        step(&mut [&mut h, &mut a, &mut b], 10);
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        let (hn, an, bn) = (sorted(names(&mut h)), sorted(names(&mut a)), sorted(names(&mut b)));
        assert_eq!(hn, vec!["all", "others", "server"]);
        assert_eq!(an, vec!["all", "fromhost"]);
        assert_eq!(bn, vec!["all", "fromhost", "others", "owner", "peer3"]);
    }

    #[test]
    fn spawn_and_late_join_catch_up() {
        let hub = MemHub::new();
        let mut h = host(&hub);
        let mut a = client(&hub, &h, "A");
        step(&mut [&mut h, &mut a], 10);
        let template = ObjectId::new();
        let hs = ObjectId::new();
        h.spawn(NetSpawnSource::Clone(template), hs, Vec3::X, Quat::IDENTITY, 0);
        let as_ = ObjectId::new();
        a.spawn(NetSpawnSource::Prefab("prefabs/ball.prefab".into()), as_, Vec3::Y, Quat::IDENTITY, 2);
        h.set_var(hs, "score", NetValue::Int(7));
        h.send_transform(hs, Vec3::new(1.0, 2.0, 3.0), Quat::IDENTITY, Vec3::ONE);
        step(&mut [&mut h, &mut a], 10);
        let hv = h.view();
        assert!(hv.world_ops.iter().any(|op| matches!(op, NetWorldOp::Spawn { root_id, .. } if *root_id == as_)), "host spawns the client's object");
        let av = a.view();
        assert!(av.world_ops.iter().any(|op| matches!(op, NetWorldOp::Spawn { root_id, .. } if *root_id == hs)));
        assert_eq!(av.owners.get(&as_), Some(&2));

        // A late joiner gets both spawns, their owners, the variable and the last transform.
        let mut c = client(&hub, &h, "C");
        step(&mut [&mut h, &mut a, &mut c], 20);
        let cv = c.view();
        let spawned: Vec<ObjectId> = cv
            .world_ops
            .iter()
            .filter_map(|op| match op {
                NetWorldOp::Spawn { root_id, .. } => Some(*root_id),
                _ => None,
            })
            .collect();
        assert!(spawned.contains(&hs) && spawned.contains(&as_));
        assert_eq!(cv.owners.get(&as_), Some(&2));
        assert!(c.pending_vars.iter().any(|p| p.id == hs.raw() && p.value == NetValue::Int(7)));
        let x = c.remote_transform(hs).expect("baseline transform");
        assert!((x.translation - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-4);

        // Despawn by the owner reaches everyone.
        a.despawn(as_);
        step(&mut [&mut h, &mut a, &mut c], 10);
        assert!(c.view().world_ops.iter().any(|op| matches!(op, NetWorldOp::Despawn(id) if *id == as_)));
        assert!(h.world.spawns.iter().all(|s| s.id != as_.raw()));
    }

    #[test]
    fn owner_leaving_releases_or_despawns() {
        let hub = MemHub::new();
        let mut h = host(&hub);
        let mut a = client(&hub, &h, "A");
        step(&mut [&mut h, &mut a], 10);
        let kept = ObjectId::new();
        let temp = ObjectId::new();
        a.request_ownership(kept);
        a.spawn(NetSpawnSource::Clone(ObjectId::new()), temp, Vec3::ZERO, Quat::IDENTITY, 2);
        step(&mut [&mut h, &mut a], 10);
        h.world.despawn_on_leave.insert(temp.raw());
        a.disconnect();
        step(&mut [&mut h], 5);
        assert_eq!(h.owner_of(kept), 0, "released to the host");
        assert!(h.view().world_ops.iter().any(|op| matches!(op, NetWorldOp::Despawn(id) if *id == temp)));
    }

    #[test]
    fn transform_codec_and_interpolation() {
        let r = Quat::from_euler(glam::EulerRot::YXZ, 1.1, -0.4, 0.25);
        let e = SnapEntry { id: 9, pos: Some(Vec3::new(123.4567, -0.001, 77.0)), rot: Some(r), scale: None, vel: Some(Vec3::X), quantized: true };
        let mut b = Vec::new();
        enc_entry(&mut b, &e);
        assert_eq!(b.len(), entry_size(&e));
        let d = dec_entry(&mut Cursor::new(&b, 0)).unwrap();
        assert!((d.pos.unwrap() - e.pos.unwrap()).abs().max_element() <= 0.0005);
        assert!(d.rot.unwrap().angle_between(r).to_degrees() < 0.2);
        let e2 = SnapEntry { quantized: false, ..e };
        let mut b2 = Vec::new();
        enc_entry(&mut b2, &e2);
        let d2 = dec_entry(&mut Cursor::new(&b2, 0)).unwrap();
        assert!(d2.rot.unwrap().angle_between(r).to_degrees() < 0.01);
        assert!(b.len() < b2.len(), "quantized is smaller");

        let mut buf = SnapBuffer::default();
        for k in 0..5 {
            let t = k as f64 * 0.1;
            buf.snaps.push_back(Snap { time: t, pos: Vec3::X * k as f32, rot: Quat::IDENTITY, scale: Vec3::ONE, vel: Vec3::X * 10.0 });
        }
        let mid = sample_snapshots(&buf, 0.25, InterpolationMode::Interpolate).unwrap();
        assert!((mid.pos.x - 2.5).abs() < 1e-4);
        let ahead = sample_snapshots(&buf, 0.45, InterpolationMode::Extrapolate).unwrap();
        assert!((ahead.pos.x - 4.5).abs() < 1e-3);
        let held = sample_snapshots(&buf, 0.45, InterpolationMode::Interpolate).unwrap();
        assert_eq!(held.pos.x, 4.0);

        let many: Vec<SnapEntry> = (0..200).map(|i| SnapEntry { id: i, ..e }).collect();
        let packets = build_snap_packets(1, 0.0, &many);
        assert!(packets.len() > 1 && packets.iter().all(|p| p.len() <= MTU), "batched under the MTU");
    }

    // End-to-end over real UDP loopback: handshake, ownership, transform, chat.
    #[test]
    fn udp_loopback_session() {
        let mut host = NetSession::host_with(0, cfg("Host")).expect("host");
        let port = host.local_addr().expect("addr").port();
        let mut client = NetSession::join_with(&format!("127.0.0.1:{port}"), cfg("Client")).expect("join");
        let settle = |a: &mut NetSession, b: &mut NetSession| {
            for _ in 0..40 {
                a.pump();
                b.pump();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        settle(&mut host, &mut client);
        assert_eq!(client.local_peer(), 2, "host assigned the client a peer id");
        let id = ObjectId::new();
        client.request_ownership(id);
        settle(&mut host, &mut client);
        assert!(client.owns(id));
        assert_eq!(host.owner_of(id), 2);
        client.send_transform(id, Vec3::new(1.0, 2.0, 3.0), Quat::IDENTITY, Vec3::ONE);
        settle(&mut host, &mut client);
        let rt = host.remote_transform(id).expect("host got transform");
        assert!((rt.translation - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-3);
        client.send_message(None, "hello");
        settle(&mut host, &mut client);
        assert!(host.view().messages.iter().any(|(_, _, t)| t == "hello"));
    }

    #[test]
    fn lan_discovery_finds_host() {
        let port = 47000 + (std::process::id() % 900) as u16;
        let mut host = NetSession::host_with(0, NetConfig { session_name: "Lan Test".into(), discovery_port: port, ..cfg("Host") })
            .expect("host");
        let Ok(mut browser) = LanBrowser::with_port(port) else {
            return;
        };
        let mut found = false;
        for _ in 0..50 {
            browser.refresh();
            host.pump();
            std::thread::sleep(std::time::Duration::from_millis(5));
            if browser.poll().iter().any(|s| s.name == "Lan Test") {
                found = true;
                break;
            }
        }
        assert!(found || host.discovery.is_none(), "discovery answered");
    }
}
