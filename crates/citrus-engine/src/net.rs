//! Built-in networking (2G): a UDP **star-relay** session. One peer *hosts*
//! (acts as a dedicated server or a player-host); others *join* it. All traffic
//! flows through the host, which relays to the other peers — one code path serves
//! both client-server and peer-to-peer use on a LAN.
//!
//! Replicated state: per-object **ownership** (whoever grabs an object becomes
//! its authority) and the owner's **transform**, broadcast to everyone else. The
//! host arbitrates ownership (last-claim-wins) so it stays consistent.
//!
//! Wire format is a compact hand-rolled binary (no serde/bincode dependency, so
//! the runtime stays lean). LAN-focused; NAT traversal / reliability / delta
//! compression are documented follow-ups.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};

use citrus_core::{NetView, ObjectId};
use glam::{Quat, Vec3};

const TAG_JOIN: u8 = 1;
const TAG_WELCOME: u8 = 2;
const TAG_TRANSFORM: u8 = 3;
const TAG_CLAIM: u8 = 4;
const TAG_RELEASE: u8 = 5;
const TAG_OWNER: u8 = 6;
const TAG_MSG: u8 = 7;
const TAG_VOICE: u8 = 8;

/// A latest-wins transform received for a non-owned object, plus the seq it
/// arrived with (older packets are dropped).
#[derive(Clone, Copy)]
pub struct RemoteTransform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
    pub seq: u64,
}

/// One live networking session (host or client).
pub struct NetSession {
    socket: UdpSocket,
    is_host: bool,
    local_peer: u64,
    /// Host: client addr per peer id. Client: just the host addr under peer 1.
    peers: HashMap<u64, SocketAddr>,
    host_addr: Option<SocketAddr>,
    /// object -> (owner peer, claim seq). owner 0 = unowned.
    owners: HashMap<ObjectId, (u64, u64)>,
    /// object -> latest remote transform (for objects we don't own).
    remote: HashMap<ObjectId, RemoteTransform>,
    /// Text messages received since the last `view()` drain.
    inbox: Vec<(u64, bool, String)>,
    /// Voice packets received since the last `take_voice()` drain.
    voice_in: Vec<VoicePacket>,
    next_peer: u64,
    seq: u64,
    claim_seq: u64,
}

/// A received voice frame (mono PCM) tagged by sender + sequence for the jitter
/// buffer to reorder.
pub struct VoicePacket {
    pub from: u64,
    pub seq: u32,
    pub samples: Vec<i16>,
}

impl NetSession {
    /// Start hosting on `port` (0 = OS-assigned). The host is peer id 1.
    pub fn host(port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        socket.set_nonblocking(true)?;
        tracing::info!("net: hosting on {}", socket.local_addr()?);
        Ok(Self::new(socket, true, 1))
    }

    /// Join a host at `addr` (e.g. "127.0.0.1:9000").
    pub fn join(addr: &str) -> std::io::Result<Self> {
        let host: SocketAddr = addr
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
        let socket = UdpSocket::bind(("0.0.0.0", 0))?;
        socket.set_nonblocking(true)?;
        let mut s = Self::new(socket, false, 0);
        s.host_addr = Some(host);
        // Peer id assigned by the host in WELCOME.
        let mut buf = Vec::new();
        buf.push(TAG_JOIN);
        let _ = s.socket.send_to(&buf, host);
        tracing::info!("net: joining {host}");
        Ok(s)
    }

    fn new(socket: UdpSocket, is_host: bool, local_peer: u64) -> Self {
        Self {
            socket,
            is_host,
            local_peer,
            peers: HashMap::new(),
            host_addr: None,
            owners: HashMap::new(),
            remote: HashMap::new(),
            inbox: Vec::new(),
            voice_in: Vec::new(),
            next_peer: 2,
            seq: 0,
            claim_seq: 0,
        }
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }
    pub fn is_host(&self) -> bool {
        self.is_host
    }
    pub fn local_peer(&self) -> u64 {
        self.local_peer
    }
    pub fn peer_count(&self) -> usize {
        // host counts itself + clients; client reports 1 (host) + itself once known
        if self.is_host {
            self.peers.len() + 1
        } else {
            self.peers.len() + 1
        }
    }

    /// The owner the local view should report (0 = unowned).
    fn owner_of(&self, id: ObjectId) -> u64 {
        self.owners.get(&id).map(|(o, _)| *o).unwrap_or(0)
    }

    /// Whether the local peer currently holds authority over an object.
    pub fn owns(&self, id: ObjectId) -> bool {
        self.local_peer != 0 && self.owner_of(id) == self.local_peer
    }

    /// Non-draining `(object, owner)` snapshot — used to position remote peers'
    /// voices at the object they own (spatial voice).
    pub fn owners_snapshot(&self) -> Vec<(ObjectId, u64)> {
        self.owners
            .iter()
            .filter(|(_, (o, _))| *o != 0)
            .map(|(id, (o, _))| (*id, *o))
            .collect()
    }

    /// Build the per-frame [`NetView`] components see, draining received messages
    /// into it (so each message is delivered once).
    pub fn view(&mut self) -> NetView {
        let owners = self
            .owners
            .iter()
            .filter(|(_, (o, _))| *o != 0)
            .map(|(id, (o, _))| (*id, *o))
            .collect();
        NetView {
            connected: true,
            is_server: self.is_host,
            local_peer: self.local_peer,
            owners,
            messages: std::mem::take(&mut self.inbox),
        }
    }

    /// Drain voice packets received since the last call (for the jitter buffer).
    pub fn take_voice(&mut self) -> Vec<VoicePacket> {
        std::mem::take(&mut self.voice_in)
    }

    /// Send a text message: `to = None` broadcasts (public), `Some(peer)` is
    /// private. Routed through the host.
    pub fn send_message(&mut self, to: Option<u64>, text: &str) {
        let target = to.unwrap_or(0);
        let mut b = vec![TAG_MSG];
        put_u64(&mut b, target);
        put_u64(&mut b, self.local_peer);
        put_str(&mut b, text);
        self.route_msg(&b, target, self.local_peer, None);
    }

    /// Send a mono PCM voice frame to everyone (relayed; spatialized on receipt).
    pub fn send_voice(&mut self, seq: u32, samples: &[i16]) {
        let mut b = vec![TAG_VOICE];
        put_u64(&mut b, self.local_peer);
        put_u32(&mut b, seq);
        put_u32(&mut b, samples.len() as u32);
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        self.fanout(&b, None);
    }

    /// Route a message packet by target peer (0 = broadcast). The host delivers
    /// locally + relays; a client forwards to the host.
    fn route_msg(&mut self, data: &[u8], target: u64, from: u64, except: Option<SocketAddr>) {
        let is_private = target != 0;
        if self.is_host {
            if target == 0 {
                // Broadcast: relay to all clients (except origin) + deliver to host.
                for (peer, addr) in &self.peers {
                    if Some(*addr) != except && *peer != from {
                        let _ = self.socket.send_to(data, addr);
                    }
                }
                if from != self.local_peer {
                    self.deliver_msg(data);
                }
            } else if target == self.local_peer {
                self.deliver_msg(data);
            } else if let Some(addr) = self.peers.get(&target) {
                let _ = self.socket.send_to(data, addr);
            }
        } else if let Some(h) = self.host_addr {
            let _ = self.socket.send_to(data, h);
        }
        let _ = is_private;
    }

    fn deliver_msg(&mut self, data: &[u8]) {
        let mut c = Cursor { d: data, p: 1 };
        let target = c.u64();
        let from = c.u64();
        let text = c.string();
        self.inbox.push((from, target != 0, text));
    }

    /// Latest remote transform for an object we don't own.
    pub fn remote_transform(&self, id: ObjectId) -> Option<RemoteTransform> {
        self.remote.get(&id).copied()
    }

    // --- ownership API (driven by ComponentCommands) ---

    /// Local peer claims authority over an object.
    pub fn request_ownership(&mut self, id: ObjectId) {
        self.claim_seq += 1;
        if self.is_host {
            self.set_owner(id, self.local_peer, self.claim_seq);
            self.broadcast_owner(id);
        } else if let Some(h) = self.host_addr {
            let mut b = vec![TAG_CLAIM];
            put_u128(&mut b, id.raw());
            let _ = self.socket.send_to(&b, h);
        }
    }

    /// Local peer releases authority over an object.
    pub fn release_ownership(&mut self, id: ObjectId) {
        if self.is_host {
            self.set_owner(id, 0, self.claim_seq + 1);
            self.claim_seq += 1;
            self.broadcast_owner(id);
        } else if let Some(h) = self.host_addr {
            let mut b = vec![TAG_RELEASE];
            put_u128(&mut b, id.raw());
            let _ = self.socket.send_to(&b, h);
        }
    }

    fn set_owner(&mut self, id: ObjectId, owner: u64, seq: u64) {
        match self.owners.get(&id) {
            Some((_, s)) if *s > seq => {}
            _ => {
                self.owners.insert(id, (owner, seq));
            }
        }
    }

    /// Send the transform of an object the local peer owns to all other peers.
    pub fn send_transform(&mut self, id: ObjectId, t: Vec3, r: Quat, s: Vec3) {
        self.seq += 1;
        let mut b = vec![TAG_TRANSFORM];
        put_u128(&mut b, id.raw());
        put_u64(&mut b, self.local_peer);
        put_u64(&mut b, self.seq);
        put_vec3(&mut b, t);
        put_quat(&mut b, r);
        put_vec3(&mut b, s);
        self.fanout(&b, None);
    }

    /// Receive + process all pending packets. Call once per frame.
    pub fn pump(&mut self) {
        let mut buf = [0u8; 512];
        loop {
            let (len, src) = match self.socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::debug!("net recv error: {e}");
                    break;
                }
            };
            self.handle(&buf[..len], src);
        }
    }

    fn handle(&mut self, data: &[u8], src: SocketAddr) {
        if data.is_empty() {
            return;
        }
        let tag = data[0];
        let mut c = Cursor { d: data, p: 1 };
        match tag {
            TAG_JOIN if self.is_host => {
                let peer = self.next_peer;
                self.next_peer += 1;
                self.peers.insert(peer, src);
                let mut b = vec![TAG_WELCOME];
                put_u64(&mut b, peer);
                let _ = self.socket.send_to(&b, src);
                // Bring the new peer up to date on current ownership.
                let owned: Vec<ObjectId> = self.owners.keys().copied().collect();
                for id in owned {
                    let mut o = vec![TAG_OWNER];
                    let (owner, seq) = self.owners[&id];
                    put_u128(&mut o, id.raw());
                    put_u64(&mut o, owner);
                    put_u64(&mut o, seq);
                    let _ = self.socket.send_to(&o, src);
                }
                tracing::info!("net: peer {peer} joined from {src}");
            }
            TAG_WELCOME if !self.is_host => {
                self.local_peer = c.u64();
                self.peers.insert(1, src); // host is peer 1
                self.host_addr = Some(src);
                tracing::info!("net: joined as peer {}", self.local_peer);
            }
            TAG_TRANSFORM => {
                let id = ObjectId::from_raw(c.u128());
                let _sender = c.u64();
                let seq = c.u64();
                let t = c.vec3();
                let r = c.quat();
                let s = c.vec3();
                let newer = self.remote.get(&id).map(|x| seq > x.seq).unwrap_or(true);
                if newer {
                    self.remote.insert(
                        id,
                        RemoteTransform {
                            translation: t,
                            rotation: r,
                            scale: s,
                            seq,
                        },
                    );
                }
                if self.is_host {
                    self.fanout(data, Some(src)); // relay to other clients
                }
            }
            TAG_CLAIM if self.is_host => {
                let id = ObjectId::from_raw(c.u128());
                let peer = self.peer_for(src);
                self.claim_seq += 1;
                self.set_owner(id, peer, self.claim_seq);
                self.broadcast_owner(id);
            }
            TAG_RELEASE if self.is_host => {
                let id = ObjectId::from_raw(c.u128());
                self.claim_seq += 1;
                self.set_owner(id, 0, self.claim_seq);
                self.broadcast_owner(id);
            }
            TAG_OWNER if !self.is_host => {
                let id = ObjectId::from_raw(c.u128());
                let owner = c.u64();
                let seq = c.u64();
                self.set_owner(id, owner, seq);
            }
            TAG_MSG => {
                let target = c.u64();
                let from = c.u64();
                if self.is_host {
                    // Host routes (relay/deliver); never echo to the sender.
                    self.route_msg(data, target, from, Some(src));
                } else {
                    self.deliver_msg(data);
                }
            }
            TAG_VOICE => {
                let from = c.u64();
                let seq = c.u32();
                let n = c.u32() as usize;
                let mut samples = Vec::with_capacity(n);
                for _ in 0..n {
                    samples.push(c.i16());
                }
                if from != self.local_peer {
                    self.voice_in.push(VoicePacket { from, seq, samples });
                }
                if self.is_host {
                    self.fanout(data, Some(src)); // relay to other peers
                }
            }
            _ => {}
        }
    }

    fn peer_for(&self, addr: SocketAddr) -> u64 {
        self.peers
            .iter()
            .find(|(_, a)| **a == addr)
            .map(|(p, _)| *p)
            .unwrap_or(0)
    }

    fn broadcast_owner(&self, id: ObjectId) {
        let (owner, seq) = self.owners.get(&id).copied().unwrap_or((0, 0));
        let mut b = vec![TAG_OWNER];
        put_u128(&mut b, id.raw());
        put_u64(&mut b, owner);
        put_u64(&mut b, seq);
        self.fanout(&b, None);
    }

    /// Send to all peers (host: all clients; client: the host), skipping `except`.
    fn fanout(&self, data: &[u8], except: Option<SocketAddr>) {
        if self.is_host {
            for addr in self.peers.values() {
                if Some(*addr) != except {
                    let _ = self.socket.send_to(data, addr);
                }
            }
        } else if let Some(h) = self.host_addr {
            let _ = self.socket.send_to(data, h);
        }
    }
}

// --------------------------------------------------------------- wire helpers

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_str(b: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    put_u32(b, bytes.len() as u32);
    b.extend_from_slice(bytes);
}
fn put_u128(b: &mut Vec<u8>, v: u128) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_f32(b: &mut Vec<u8>, v: f32) {
    b.extend_from_slice(&v.to_le_bytes());
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

struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
}
impl Cursor<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        let s = self.d.get(self.p..self.p + n).unwrap_or(&[]);
        self.p += n;
        s
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap_or([0; 4]))
    }
    fn i16(&mut self) -> i16 {
        i16::from_le_bytes(self.take(2).try_into().unwrap_or([0; 2]))
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().unwrap_or([0; 8]))
    }
    fn string(&mut self) -> String {
        let n = self.u32() as usize;
        String::from_utf8_lossy(self.take(n)).into_owned()
    }
    fn u128(&mut self) -> u128 {
        u128::from_le_bytes(self.take(16).try_into().unwrap_or([0; 16]))
    }
    fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.take(4).try_into().unwrap_or([0; 4]))
    }
    fn vec3(&mut self) -> Vec3 {
        Vec3::new(self.f32(), self.f32(), self.f32())
    }
    fn quat(&mut self) -> Quat {
        Quat::from_xyzw(self.f32(), self.f32(), self.f32(), self.f32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn settle(a: &mut NetSession, b: &mut NetSession, times: u32) {
        for _ in 0..times {
            a.pump();
            b.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    // End-to-end over UDP loopback: join handshake, ownership claim + broadcast,
    // transform replication, and public messaging.
    #[test]
    fn ownership_messaging_and_transform_over_loopback() {
        let mut host = NetSession::host(0).expect("host");
        let port = host.local_addr().expect("addr").port();
        let mut client = NetSession::join(&format!("127.0.0.1:{port}")).expect("join");
        settle(&mut host, &mut client, 25);
        assert_eq!(client.local_peer(), 2, "host assigned the client a peer id");

        // Client claims an object; host arbitrates + broadcasts ownership back.
        let id = ObjectId::new();
        client.request_ownership(id);
        settle(&mut host, &mut client, 25);
        assert!(client.owns(id), "client owns after a granted claim");
        assert_eq!(host.owner_of(id), 2, "host records the client as owner");

        // Owner's transform replicates to the host.
        client.send_transform(id, Vec3::new(1.0, 2.0, 3.0), Quat::IDENTITY, Vec3::ONE);
        settle(&mut host, &mut client, 25);
        let rt = host.remote_transform(id).expect("host got transform");
        assert!((rt.translation - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-3);

        // Public message reaches the host.
        client.send_message(None, "hello");
        settle(&mut host, &mut client, 25);
        let msgs = host.view().messages;
        assert!(msgs.iter().any(|(_, _, t)| t == "hello"), "host received broadcast");
    }
}
