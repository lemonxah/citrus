# Networking

Citrus ships a built-in networking layer for multiplayer games: social VR
worlds where players grab and pass objects around, and shooters where the
server has the final say. This page covers the model, the components, the
`ComponentCtx` API and the editor tooling.

Code: `crates/citrus-core/src/net.rs` (API types, components, ctx helpers),
`crates/citrus-engine/src/net.rs` (transport, connections, session registry),
`crates/citrus-engine/src/net_sync.rs` (scene replication),
`crates/citrus-engine/src/play_net.rs` (play-session hooks),
`crates/citrus-engine/src/net_window.rs` (editor window),
`crates/citrus-editor/src/net_inspect.rs` (inspectors).

## Model

- **Host-authoritative star.** One peer hosts (peer id `1`, the server); every
  other peer joins it. All traffic goes through the host, which validates and
  relays. The host can be a player (listen server) or a dedicated server
  (Network Manager > Dedicated Host).
- **Ownership.** Every networked object has an owner peer, or none. The
  *authority* over an object is its owner, or the host when it is unowned. The
  authority simulates the object and replicates it; everyone else shows what it
  sends. Offline, the local game has authority over everything, so the same
  gameplay code runs in single-player.
- **The host enforces it.** Only an object's owner may move it, write its
  variables (unless the component allows anyone), change its parent / active
  state, release or transfer it, or despawn it. Writes from anyone else are
  dropped.
- **Identity.** A networked object's network id is its `ObjectId`. Scene
  objects already share ids on every peer that loads the same `.scene`.
  Runtime spawns get their id from the spawner, and their children get
  deterministic ids derived from it, so the whole subtree matches everywhere
  (object references inside a spawned prefab are remapped to the new ids).

### Transport

UDP with two channels per connection:

| Channel | Carries |
|---|---|
| Unreliable | transform snapshots, avatar poses, face tracking, Animator parameters, voice, unreliable RPCs |
| Reliable, ordered | join / leave, player list, ownership, spawns, despawns, synced variables, active / parent changes, RPCs, chat, scene changes, late-join catch-up |

The reliable channel numbers every message, acks in batches, resends after
`2 x RTT + 50 ms` and fragments messages larger than one datagram (1200 bytes),
so large variable values arrive intact. Connections handshake (HELLO, then
WELCOME or REJECT for a protocol mismatch or a full session), ping every 0.5 s
for RTT and time out after `timeout` seconds of silence (default 10). A clean
disconnect sends BYE so the others notice immediately.

For tests and tools the transport is a trait: `MemHub` gives in-process
endpoints with optional packet loss and severed links, and sessions can run on
a manual clock (`use_manual_clock` / `advance`).

## Starting a session

- **Editor**: Tools > Network. Enter a name and an address, then Host (binds the
  port in the address) or Join. The LAN list finds hosts on the local network.
  While connected the window shows the player list with ping, traffic, and every
  networked object with who simulates it. Connections stay alive while you edit;
  replication runs in Play mode, and stopping Play clears replicated state.
- **Built games**: a Network Manager with Auto Start = Host / Join, or the
  environment variables `CITRUS_HOST=<port>` / `CITRUS_JOIN=<host:port>`.
  `CITRUS_NAME` sets the player name.
- **From gameplay** (menus): `ctx.net_host(port)` (0 = the Network Manager's
  port), `ctx.net_join("1.2.3.4:9000")`, `ctx.net_disconnect()`.

LAN discovery: a host answers broadcast queries on UDP port 47777
(`LanBrowser` in `citrus_engine::net`).

## Components

### Network Object

Marks an object as networked and says what replicates.

| Field | Meaning |
|---|---|
| Ownership | `Takeover` (anyone can grab it, last claim wins), `Request` (granted when unowned; otherwise the owner gets `OwnershipRequested` and decides), `Host only` |
| Transform / Scale | replicate the world transform (scale optional) |
| Rigid Body | the authority simulates the body; other peers make it kinematic and follow. When authority comes back it turns dynamic again with the last known velocity, so a thrown object keeps flying |
| Parent / Active | replicate re-parenting and enable / disable |
| Mode / Delay | `Snap`, `Interpolate` (render `Delay` seconds behind and interpolate buffered snapshots) or `Extrapolate` (also predict with the sent velocity, capped at 0.25 s, when snapshots run late) |
| Send Rate | transform updates per second |
| Move / Turn Threshold | skip sends while the object is still |
| Quantize | 1 mm positions (24 bits per axis, +-8 km) and 32-bit rotations |
| Relevancy | only send to players within this distance (0 = everyone) |
| Despawn On Leave | remove the object when its owner leaves (otherwise it becomes unowned) |

The engine writes runtime state into `state` (`connected`, `owner`,
`authority`); the inspector shows it during Play.

`Sync` still works: it maps onto Network Object defaults (`smoothing` 0 =
Snap, otherwise Interpolate) and keeps its grab-to-own toggle.

### Network Variables

Typed key / value state (`Bool`, `Int`, `Float`, `Vec3`, `Quat`, `String`,
`Bytes`) with owner-write semantics. Authored entries are the defaults; at
runtime the authority writes with `ctx.set_net_var`, the host validates and
relays, every peer's component updates and a `VarChanged` event fires. Late
joiners receive the current values. Anyone Can Write lifts the owner check.

### Network Animator

Replicates the object's Animator parameters (all, or a listed subset) from the
authority at its send rate, with a full refresh every second. Triggers
replicate as their float value; use a bool or an RPC for one-shots that must
never be missed.

### Network Avatar

VR / humanoid pose sync. Put it on the player rig root and point it at the
head, hands and optional hips / feet objects (its children). The authority
sends their world poses (30 Hz default, 32-bit rotations); other peers write
them back, interpolated, and expose them as `targets` (`TrackerTargets`) for
IK. With Drive From VR the owner's head and hands follow the local headset and
controllers (`ctx.screen.vr`).

The avatar packet carries all eleven tracker roles (head, hands, hips, feet,
chest, elbows, knees; a 16-bit presence mask, protocol 3). With Drive From VR
or Follow Local Player the owner sends its calibrated full-body targets for
every tracker it has, falling back to the part objects' poses; parts only need
to exist for roles you want visible objects for. On other peers `remote` is
set, and a `Humanoid IK` (source Auto) on the avatar model poses it from the
replicated `targets`.

Face tracking rides along: the owner's face frame (non-zero Unified
Expressions weights as bytes, plus eye gaze) goes out with the avatar poses and
remote copies expose it as `face` for their Face Tracking.

The worn avatar package travels too: when a player wears a `.citruspak`
avatar, its Network Avatar's `wearing` (content id + download URL) replicates
as the synced variable `citrus.avatar` (reliable, late joiners catch up), and
each peer's content manager loads and wears the same package on that player.
Peers download from the URL; a package worn from a local file only reaches
peers that already have it cached.

### Network Manager

Scene-level session settings: session name, port, max players, timeout, LAN
discovery, the player prefab spawned (owned by the player) for every peer that
joins, the Spawn Point tag those spawn at (round-robin by index), Dedicated
Host, Clients Can Spawn, and Auto Start for built games. Player objects are
despawned when their player leaves.

A spawned pawn only reads input on the peer that owns it: on other peers its
`possessed` flag is cleared before it starts.

## ComponentCtx API

```rust
// Session
ctx.net_connected(); ctx.is_server(); ctx.is_client();
ctx.local_peer();                 // 0 offline, 1 = host
ctx.peers();                      // &[PeerInfo { id, name, rtt_ms, is_host }]
ctx.peer_name(id);
ctx.net_events();                 // &[NetEvent] since last frame
ctx.net_host(port); ctx.net_join(addr); ctx.net_disconnect();
ctx.net_load_scene("scenes/arena.scene"); // host: everyone (and late joiners) switch

// Authority and ownership
ctx.has_authority();              // this object: owner, or host when unowned, or offline
ctx.has_authority_of(target);
ctx.owns_self(); ctx.owns(target); ctx.owner_of(target);
ctx.request_ownership(); ctx.request_ownership_of(target);
ctx.release_ownership(); ctx.release_ownership_of(target);
ctx.transfer_ownership(target, peer);

// Spawning (returns the network id immediately)
let id = ctx.net_spawn("prefabs/ball.prefab", pos, rot);      // owned by me
let id = ctx.net_spawn_server("prefabs/crate.prefab", pos, rot); // host authority
let id = ctx.net_clone(template_ref, pos, rot);               // clone + enable a scene object
ctx.net_despawn(target);          // authority only; a plain destroy of an owned spawn also despawns

// Synced variables
ctx.set_net_var_self("hp", 90);   // false if not allowed
ctx.net_var(target, "hp").and_then(NetValue::as_int);
for (object, key, value, from) in ctx.net_var_changes() { /* ... */ }

// RPCs / network events
ctx.rpc("RoundStart", vec![3.into()], RpcTarget::All);
ctx.rpc_unreliable("Footstep", vec![pos.into()], RpcTarget::Others);
ctx.rpc_self("Hit", vec![damage.into()], RpcTarget::Owner);
ctx.send_rpc(Some(object), "Open", vec![], RpcTarget::Server, true);
for r in ctx.rpcs_named("Hit") { let dmg = r.arg(0).and_then(NetValue::as_float); }
for r in ctx.rpcs_for_self() { /* RPCs sent on this object */ }

// Chat (unchanged)
ctx.broadcast("hi"); ctx.send_to(peer, "psst"); ctx.messages();
```

RPC targets: `All` (including the sender), `Others`, `Server`, `Owner` (of the
RPC's object; the host when unowned) and `Peer(id)`. RPCs arrive next frame
through `ctx.rpcs()`. Offline, targets that include the local player loop back,
so single-player code paths keep working.

`NetEvent`: `Connected`, `Disconnected { reason }`, `PeerJoined`, `PeerLeft`,
`OwnershipChanged`, `OwnershipRequested`, `OwnershipDenied`, `Spawned`,
`Despawned`, `VarChanged`.

### Patterns

Grab to own (VR):

```rust
fn update(&mut self, ctx: &mut ComponentCtx) {
    if self.grabbed_this_frame && !ctx.has_authority() {
        ctx.request_ownership();   // Takeover objects switch on the next round trip
    }
    if ctx.has_authority() {
        // move it; the engine replicates the transform / body
    }
}
```

Server-authoritative damage (shooter):

```rust
// Client: ask the server.
ctx.send_rpc(Some(target), "Fire", vec![origin.into(), dir.into()], RpcTarget::Server, true);
// Server: validate, then write the variable everyone sees.
for r in ctx.rpcs_named("Fire") { /* raycast, then */ ctx.set_net_var(target, "hp", hp - 10); }
```

Gate gameplay that moves a networked object on `ctx.has_authority()`: on the
other peers, replication overwrites the transform after every frame anyway.

### Water that matches on every client

Waves, floating and the water effects run on a shared water clock: the host's
clock as each peer estimates it (`NetSession::shared_clock`, one-way delay
corrected by half the round trip), exposed to gameplay as `ctx.water_time`
and used by `ctx.water_surface`. Put a **Wave Rider** on things that should
bob on the water (balls, buoys, fish near the surface) and sync them with a
Network Object as usual: the synced transform is the calm anchor, and every
client adds the same wave motion locally, so nothing wave-related crosses the
network. Steep waves and current move the anchor on the owner only, which
then replicates like any other movement.

Not shared: the wind sea builds up from Wind Zones over time on each client,
the interactive ripple field reacts to local physics, and dynamic Rigid Bodies
floating by buoyancy are simulated by their owner and replicated (use a Wave
Rider when every client should compute the bobbing itself).

## Frame order

1. Host loop pumps the session (receive, resend, ping, timeouts) and builds the
   frame's `NetView`.
2. Play session: applies network world changes (spawns, despawns, activation,
   scene switch), clears `possessed` on remote pawns, hands rigid bodies to or
   from kinematic, then runs the usual component passes.
3. Networking commands from components are applied locally (spawn, despawn,
   variable writes) and handed to the session.
4. `LoadedScene::network_sync`: received variables and parents land, the
   authority sends due snapshots / parameters / poses, everything else is
   sampled from its buffers, owned spawns that vanished are despawned, and the
   host spawns player prefabs for new peers.

## Bandwidth

- Per-object send rates, still-object thresholds, field-level deltas (omitted
  position / rotation / scale reuse the last value) with a full keyframe every
  second to heal losses.
- Quantized transforms: 9-byte positions, 4-byte rotations (smallest-three).
- Entries from one peer are batched into as few MTU-sized packets as fit.
- Relevancy: the host only sends an object to players whose player object
  (Network Manager spawn, or any owned Pawn / Network Avatar) is within the
  object's Relevancy distance.

## Limitations

- Star topology through the host; no NAT traversal / relay service and no host
  migration. Players behind NAT need the host's port forwarded (or a VPN).
- No client-side prediction or reconciliation for owned characters beyond
  "the owner simulates locally"; no lag compensation for hit scans. Server
  authority for shooters is expressed with RPCs to the server plus host-owned
  state.
- Snapshots are stamped with the sender's clock; the receiver estimates the
  offset (so delay = transit + `interp_delay`); there is no shared tick.
- Network ids are full 128-bit object ids (16 bytes per entry).
- The play session only spawns prefabs with a renderer (headless servers
  spawn via `net_clone` of scene templates).
- After `net_load_scene`, other world changes are held for 0.5 s so they land
  in the new scene; a very slow scene load can still miss them.
- No encryption or authentication; LAN / trusted servers only.

## Voice chat

Add a `Voice Chat` component to a networked scene (next to the Network
Manager); without one voice stays off. It sets proximity or global range, the
falloff (natural inverse-distance or linear between "full volume within" and
"silent past"), the transmit mode (voice activity with a dBFS threshold, push to
talk on an input action, or always) and mic gain.

Frames are 20 ms of 16 kHz mono. With the system's libopus (loaded at run time,
so builds need no C toolchain) they are Opus at the configured bitrate with
in-band forward error correction; without it they go as PCM. The first payload
byte names the codec, so mixed peers interoperate. Protocol 4.

Each speaker plays through an adaptive jitter buffer: arrival jitter is measured
against the 20 ms cadence and the buffer holds just enough (40 to 300 ms). A
missing frame is rebuilt from the next packet's error-correction data, else
concealed by the codec; a long gap re-buffers; when the buffer runs long, quiet
frames are dropped to win the latency back. Voices are positioned (stereo) at
the speaker's Network Avatar head.

Everything that plays also feeds lip sync (see the `Lip Sync` component), so
mouths move with what you hear.

