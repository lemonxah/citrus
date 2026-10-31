# Networking

Citrus ships a built-in networking layer for multiplayer games: social VR
worlds where players grab and pass objects around, and shooters where the
server has the final say. This page covers the model, the components, the
`ComponentCtx` API and the editor tooling.

Code: `crates/citrus-core/src/net.rs` (API types, components, ctx helpers),
`crates/citrus-core/src/net_rewind.rs` (lag-compensation history),
`crates/citrus-engine/src/net.rs` (transport, connections, session registry),
`crates/citrus-engine/src/net_secure.rs` (key stretching, keys, encryption, replay window, loss),
`crates/citrus-engine/src/net_identity.rs` (player identities, fingerprints, host pinning),
`crates/citrus-engine/src/net_relay.rs` (relay client, hole punching, routes),
`crates/citrus-engine/src/net_predict.rs` (prediction, reconciliation, adaptive delay),
`crates/citrus-engine/src/net_sync.rs` (scene replication),
`crates/citrus-core/src/motion.rs` (replayable movement: `Predictable`, `MoveHost`, `MotionState`),
`crates/citrus-engine/src/play_net.rs` (play-session hooks, predicted-controller simulation),
`crates/citrus-engine/src/net_window.rs` (editor window),
`crates/citrus-editor/src/net_inspect.rs` (inspectors),
`crates/citrus-relay` (the relay server).

## Model

- **Host-authoritative star.** One peer hosts (peer id `1`, the server); every
  other peer joins it. All traffic goes through the host, which validates and
  relays. The host can be a player (listen server) or a dedicated server
  (Network Manager > Dedicated Host). When the host leaves, the next player in
  line takes over and the session goes on (see Host migration); peer ids never
  change, so after a migration the host is whichever peer `ctx.net_host_peer()`
  names.
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
| Unreliable | transform snapshots, avatar poses, face tracking, Animator parameters, voice, predicted-controller inputs and states, unreliable RPCs |
| Reliable, ordered | join / leave, player list, ownership, spawns, despawns, synced variables, active / parent changes, RPCs, chat, scene changes, late-join catch-up |

The reliable channel numbers every message, acks in batches, resends after
`2 x RTT + 50 ms` and fragments messages larger than one datagram (1200 bytes),
so large variable values arrive intact. Game packets are at most 1200 bytes;
sealing adds 25 and a relay hop 5, so datagrams stay under 1232 bytes (below
the 1280-byte IPv6 minimum MTU, no IP fragmentation). Connections ping every
0.5 s for RTT, jitter and loss and time out after `timeout` seconds of silence
(default 10). A clean disconnect sends BYE so the others notice immediately.

### Security

Every session is encrypted and authenticated (protocol 6):

1. **HELLO**: protocol, a random 16-byte client nonce, padded to 128 bytes.
2. **CHALLENGE**: the host's key-stretching parameters, its identity (Ed25519
   public key), epoch and peer id, and a stateless cookie (HMAC of the
   client's endpoint and nonce under a host secret, rotating every 30 s). The
   host keeps no state for a HELLO, and the CHALLENGE (89 bytes) is never
   larger than the HELLO, so the host can't be used to amplify floods.
3. The client stretches the join secret into the session key `K` (see Key
   stretching), hashes the transcript (client nonce + the whole CHALLENGE)
   and derives the connection key as HKDF-SHA256(K, transcript). A tampered
   CHALLENGE only yields keys that don't match.
4. **JOIN**: client nonce and cookie in the clear, then sealed with
   ChaCha20-Poly1305: protocol, name, the player id (public key) and its
   signature over the transcript, and, after a migration, the peer id it is
   rejoining as. The host checks the cookie first (no crypto for strangers),
   caps joins in progress and their rate, then opens the JOIN (proving the
   secret) and verifies the signature (proving the player id).
5. **WELCOME** (sealed, signed by the host over the transcript). The client's
   first sealed packet confirms the join; until then it counts as half-open
   (at most 32, dropped after 5 s).
6. From then on every packet is `[0x20][counter][ciphertext][16-byte tag]`.
   The nonce is the direction plus the counter, so it never repeats per key.
   Forged, corrupted and replayed packets (64-packet sliding window) are
   dropped and counted (Network window > Rejected).

**Rejections are authenticated.** A host that can't open a JOIN (wrong
secret) answers with a REJECT signed by its identity key, which the client
checks against the CHALLENGE it got. Refusals after the JOIN opened
(session full, banned, not on the allow list, bad identity signature) are
sealed with the connection key. BYE (leave, handoff, close, kick) is sealed
like everything else. A REJECT nobody signed (a version mismatch, or a
forgery) is only a hint: the client keeps trying and only gives up with that
message if no CHALLENGE turns up within 2 s. So an on-path attacker can't
abort a join or kick a player with forged packets.

**Key stretching.** Passphrases go through Argon2id: the host picks the
parameters (default 64 MiB x 3 passes, 19 MiB x 2 on Android / Quest), runs
it once at session start on a worker thread, and sends memory, passes,
lanes and a random 16-byte salt in the CHALLENGE; a client runs it once per
join. A client refuses parameters weaker than 8 MiB x 1 pass for a
passphrase (or an unstretched offer), so a forged CHALLENGE can't downgrade
it into a cheap offline guess, and it refuses absurd ones (over 256 MiB or
10 passes). The Network window's Generate button makes a random secret
(4 groups of 4 characters, about 80 bits) that is recognised by its shape
and only goes through HKDF (offline guessing can't touch 80 bits). An empty
secret is an open session: still encrypted against passive sniffing and
injection by off-path attackers, but anyone who can reach it may join.
`NetConfig::kdf` overrides the host's parameters.

The join secret comes from the Network window, the Network Manager's Secret
or `CITRUS_SECRET`.

### Player identities

Each install has a persistent Ed25519 key pair in `identity.key` in the
user's config folder, next to the game's `input.ron` (`%APPDATA%\<title>`,
`~/Library/Application Support/<title>`, `$XDG_CONFIG_HOME/<title>` or
`~/.config/<title>`; `CITRUS_CONFIG_DIR` replaces the base; the editor uses
`Citrus Editor`). The public key is the player id; its fingerprint is 128
bits of its SHA-256 written as 8 groups of 4 hex digits (`1a2b-3c4d-...`;
the UI shows the first two groups, hover for the full one, click to copy).

- The host learns every player's verified id from the JOIN signature and
  passes them on, so every peer knows who is who: `PeerInfo::identity` /
  `fingerprint`, `ctx.peer_identity(peer)`, `ctx.peer_fingerprint(peer)`.
- **Allow list / ban list** (Network Manager, one fingerprint per line):
  only allow-listed players get in (empty = anyone with the secret), banned
  ones never do. The Network window's Kick and Ban buttons (host) act on the
  session (bans added there last for the session; copy the fingerprint to
  the Network Manager to keep someone out for good). Gameplay:
  `ctx.net_kick(peer, "reason", ban)`. A kicked client is told why and does
  not try to migrate.
- **Pin Host Identity** (Network Manager, `NetConfig::pin_host`): clients
  remember the host's id per address or room code the first time
  (`known_hosts` next to `identity.key`) and refuse a different one later.
  After a migration the new host must be the member the old host vouched
  for (its id from the player list), pinned or not.

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
  environment variables `CITRUS_HOST=<port>` / `CITRUS_JOIN=<host:port or
  room code>`, `CITRUS_RELAY=<relay:port>`, `CITRUS_SECRET`, `CITRUS_NAME`,
  `CITRUS_RELAY_TOKEN` (relays that require an operator token)
  (`CITRUS_NO_PUNCH` keeps relay peers on the relay).
- **From gameplay** (menus): `ctx.net_host(port)` (0 = the Network Manager's
  port; also opens a relay room when the manager names a relay),
  `ctx.net_join("1.2.3.4:9000")` or `ctx.net_join("ABC123")`,
  `ctx.net_disconnect()`.

LAN discovery: a host answers broadcast queries on UDP port 47777
(`LanBrowser` in `citrus_engine::net`).

## Internet play (relay + hole punching)

No port forwarding needed: run a relay somewhere both sides can reach and
set it as the Relay (Network window, Network Manager, or `CITRUS_RELAY`).

```text
cargo run --release -p citrus-relay -- --bind 0.0.0.0:47800
# options: --max-rooms 4096 --max-members 64 --idle 30
#          --token SECRET          (or CITRUS_RELAY_TOKEN) hosts must know it to open rooms
#          --rooms-per-ip 8        rooms one IP may hold
#          --max-pending 16        joiners per room that haven't exchanged data yet
#          --max-pending-total 1024
#          --orphan-grace 30       seconds a room waits for a successor after its host left
```

It's a small UDP server (`crates/citrus-relay`): open UDP 47800 on the
machine it runs on and that's it.

- **Hosting** with a relay set binds the host's port as usual (LAN clients
  still connect directly) and opens a room on the relay; the relay assigns a
  six-character room code (or keeps the Network Manager's Room Code), shown
  in the Network window with a copy button and via `NetSession::room_code()`.
- **Joining** takes `ABC123` (through the configured relay) or
  `ABC123@relay.example.com:47800`. `host:port` still joins directly.
- **Hole punching**: when a client joins, the relay introduces both peers
  (the public endpoint it sees, each peer's LAN endpoint and a random punch
  token per member). Both send PUNCH packets to every candidate every 0.15 s
  for 4 s; a valid one is answered with PUNCH_ACK and the first ACK makes
  that endpoint the direct route. No ACK in time (symmetric NATs, strict
  firewalls) keeps the peer relayed; a direct route silent for 4 s falls back
  to the relay, and punching retries after 20 s. Relayed packets are always
  accepted, so switching paths never drops the connection.
- **Relay protocol 2** (`citrus_relay::proto`): REGISTER (role, room code,
  nonce, LAN endpoints, cookie, proof; padded to 160 bytes) / COOKIE /
  REGISTERED (slot, own punch token, public endpoint, claim nonce) / PEER
  (introduction) / DATA (slot + payload: destination going in, source coming
  out) / PUNCH / PUNCH_ACK / CLAIM / KEEPALIVE / LEAVE / ERROR. Tags are
  0xF1..0xFB, so relay control, relayed data and direct game traffic share
  one socket. Star topology like the sessions: the host reaches every
  client, a client only the host. The relay only sees ciphertext.
- **Relay authentication.** A REGISTER without a valid cookie only gets a
  COOKIE back (stateless, bound to the sender's endpoint and the room code;
  the reply is smaller than the padded request, and so is every other reply
  to a sender that isn't a member yet). A host creates its room with a room
  owner key (Ed25519, generated per session), a join key (HKDF of the
  stretched session key: the relay never learns anything that opens the
  session) and the key-stretching parameters; with `--token` it also proves
  the operator token (HMAC over the cookie). A joiner gets the parameters in
  the COOKIE, stretches its secret and proves it with an HMAC over the cookie
  under the join key, so the relay drops strangers before they reach the
  host. Abuse limits: per-member byte rates (4 MB/s hosts, 1 MB/s clients),
  rooms per IP, room and server caps on pending joiners (admitted but no
  data exchanged yet, dropped after 15 s), exponential back-off for an IP
  after repeated failures (up to 60 s, during which it gets nothing), and
  idle members dropped after 30 s. The join key travels to the relay in the
  clear: someone sniffing the host's link to the relay could pass the
  relay's check, but still not the session's.
- **Rooms survive their host.** When the host leaves or goes idle the room
  stays open for `--orphan-grace` seconds; a member signs a CLAIM (over the
  room code, its slot, its nonce and a relay-issued claim nonce) with the
  room owner key, which the host shared inside the encrypted session, and
  takes the host slot. Every member is re-introduced to the new host and
  keeps reaching it at the same virtual address.
- Relay peers appear to the session as virtual `240.x.y.z` addresses that
  encode their relay slot; the transport resolves them to the punched
  endpoint or wraps the datagram for the relay.

The player list shows each peer's route (`LAN`, `direct` or `relayed`).

## Host migration

On by default (Network Manager > Host Migration, `NetConfig::migration`).

- **Succession plan.** Whenever someone joins or leaves (and when bans
  change) the host sends every member its plan: the members in join order
  (lowest peer id first; `NetConfig::can_host = false` peers are left out),
  each with the endpoints others can try directly (the address the host
  sees, or the punched endpoint of a relayed peer), the relay room and its
  owner key, the session limits, the next free peer id and the ban / allow
  lists. Every member walks the same list, so the election needs no vote.
  The Network window marks the next host.
- **Host lost.** A client notices when the host times out (Network Manager
  Timeout) or sends a sealed BYE. The old host's reliable messages to us
  that were never acknowledged are kept for the next host. Gameplay gets
  `HostMigrating { previous }`; objects the host simulated (unowned ones)
  pause, owned ones keep running on their owners.
- **Election.** Each member takes the first candidate that is still in the
  player list. If that's itself, it takes over at once; otherwise it tries
  the candidate directly for `migrate_attempt` seconds (default 3), then
  through the relay room for as long (attaching the relay if it joined
  directly), then moves on to the next candidate. A client only accepts a
  CHALLENGE from the peer it is looking for, and that peer's WELCOME must
  be signed by the player id the old host vouched for.
- **Taking over.** The new host keeps its own world (every peer holds the
  full replicated state: spawns, owners, variables, parents, activation,
  transforms, Animator parameters), bumps the epoch (ownership sequence
  numbers start above everything the old host issued), releases what the
  old host owned (its player object and despawn-on-leave objects go, the
  rest becomes unowned: now ours), acts on its own unacknowledged messages,
  claims the relay room, and waits for the others (timeout + two attempts
  per higher-ranked candidate + 2 s; members that don't come back are
  dropped like a leaving player). A member that comes back keeps its peer id
  (its signature must match the id on record); someone new gets a fresh one.
- **State continuity.** A returning member gets a full resync from the new
  host (whose copy wins): spawns it doesn't know are despawned, ownership
  is taken as sent. What the member owns it re-asserts (its variables, and
  spawns the new host missed), because its own copy is the newest. Its
  reliable messages the old host never acknowledged are resent; RPCs, chat
  and variable writes carry a per-sender message id, so copies that did get
  through are dropped everywhere, and RPCs / chat routed while members are
  still reconnecting are replayed to them when they arrive.
- **Prediction continues.** A predicted controller's owner keeps predicting
  through the migration. When it rejoins it sends its newest predicted state
  and input number; the new host continues the simulation from there (it
  ignores the inputs until then), so nothing walked during the migration is
  rolled back.
- **The clock continues.** The session clock (`shared_clock`, `ctx.water_time`,
  `ctx.net_time`) runs on by itself while there is no host; the new host
  carries the offset of its estimate over, and clients ease onto the new
  estimate by at most half a frame per frame, so water and other clock-driven
  effects never jump or run backwards.
- **Graceful quit.** A host that disconnects with members (Network window >
  Leave, `ctx.net_disconnect()`, or quitting) sends its successor a final
  resync, then a sealed BYE naming the successor and its endpoints: the
  session moves at once, without waiting for a timeout. End session (host)
  closes it for everyone instead.
- **The new host fails too.** Members that can't reach a candidate move on
  to the next one; a host that took over and then dies is replaced the same
  way (its own plan is sent to everyone who rejoined).
- **Disagreement.** If a partition makes two members take over (each
  thought the better-ranked one was gone), the lower-ranked one keeps
  probing the better-ranked one with a sealed probe for the reconnect
  window; once it answers as a host, the lower-ranked one hands its members
  to it (sealed BYE naming it) and rejoins as a client. Lower rank in the
  plan always wins.
- **Late joiners** knocking while the host is being replaced (at a
  successor's address, or at a relay room whose host slot is empty) get in
  once the successor has taken over.
- **Cut off.** A member that can't reach anybody gives up when the list runs
  out. A member that took over but heard from nobody before the reconnect
  window closed ends its session (it was cut off from everyone, not the last
  one standing). Alone with a host that hands off to it, a member keeps the
  session going by itself.

Gameplay: `ctx.i_am_host()`, `ctx.net_host_peer()`, `ctx.host_changed()`
(the new host's id, the frame it changed), `ctx.net_migrating()`, and the
`HostMigrating` / `HostChanged` events. The Network window shows the
migration in progress, the members a new host still waits for, and the
number of migrations.

## Connection quality

Pings carry the sender's measured incoming loss (from counter gaps) and its
current interpolation delay, so both directions are known.
`ctx.net_rtt_ms()` / `ctx.net_rtt()` (round trip to the host) and
`ctx.net_quality()` (`NetQuality { rtt_ms, jitter_ms, loss_in, loss_out,
relayed }`) expose it to gameplay; `PeerInfo` adds `jitter_ms`, `loss` (both
directions combined) and `view_delay`.

## Client prediction (Predicted controllers)

Network Object > Predicted makes an object's movement server-authoritative
without input lag. It works on any controller prediction can replay: the
built-in Pawn, the First Person Controller, the Third Person Controller and
the VR Player (smooth locomotion, snap / smooth turning and swimming;
teleports are carried as snaps), each with or without a Character
Controller.

1. The owner's controller moves at once from local input, as always. Each
   frame becomes an input command (sequence number, frame delta, the active
   input actions, plus what the controller read from elsewhere: the view
   yaw / pitch it ended at, or for the VR Player the stick, turn, play-space
   facing, headset and hand poses and a teleport target) stored with the
   state it produced. All unacknowledged commands go to the host every
   frame (up to 12 per packet; a repeated frame costs 5 bytes), so a lost
   packet costs nothing. While a UI text field has the keyboard the frame is
   recorded without input.
2. The host queues them per object (in order; a missing one is skipped after
   100 ms; a time budget stops sped-up clients from simulating faster than
   real time), simulates the controller with exactly the same step and
   replies to the owner with the resulting state (position, rotation,
   velocity, grounded, crouch and crouch capsule, swimming, breath, a climb
   out of the water in progress) and the last input sequence it processed.
   Everyone else gets normal snapshots from the host; snapshots the owner
   tries to send for it are dropped.
3. The owner compares that with its prediction for the same sequence. On a
   mismatch (over 1 mm, or a different grounded / crouch / swim state) it
   rewinds the controller, its Character Controller body and capsule to the
   host's state, replays the unacknowledged inputs and folds the difference
   into a visual offset that decays with a 0.1 s time constant, so
   corrections glide instead of snapping (over 2 m snaps: teleports). The
   view comes from the owner's input (the host adopts the recorded view
   angles, so even inputs it had to skip never turn it), so a correction
   never turns the camera.

A controller copy driven by another peer (a remote player's First / Third
Person Controller or VR Player, or the owner's predicted one on the host)
ignores local input and leaves the camera and cursor alone; it takes them
the first frame the local player drives it.

The live frame and its replays run one step. `citrus_core::Predictable`
(`motion` / `set_motion`, `frame_extra`, `replay`) is the controller side;
it moves through `citrus_core::MoveHost` (translation, rotation,
`move_character`, ground contact, capsule resize, water surface, ray casts),
which `ComponentCtx` implements for `update` and the play session
implements for the host simulation and the owner's rewind + replay.

Gameplay that moves the controller on the host (knockback, teleports) just
writes its transform on the host; the owner reconciles. Hooks: the host loop
calls `PlaySession::net_predict` after the tick and before `network_sync`;
the offset comes back off at the start of the next tick.

### Interpolation of everything else

Remote objects render behind by an adaptive delay (Network Object >
Adaptive, on by default): one send interval, plus more when packets are lost,
plus three times the measured arrival jitter of their sender, never below
Delay and at most 0.5 s. It grows at up to 25 % of real time and shrinks at
5 %, so playback never jumps. When the buffer still runs dry (late or lost
snapshots), Interpolate keeps going at the last velocity for up to 0.1 s
before holding; a stopping object sends two rest snapshots (zero velocity at
the final pose) so receivers don't drift past where it stopped.

## Lag compensation

The host records every networked object with a hit shape (Character
Controller capsule, Sphere / Capsule / Box Collider, triggers excluded) each
frame for the last second: the owner-reported pose for remote-simulated
objects, the simulated pose otherwise.

```rust
// Client: say when the shot was seen.
ctx.send_rpc(None, "Fire", vec![origin.into(), dir.into(), (ctx.net_view_time() as f32).into()], RpcTarget::Server, true);
// Host: test against where the targets were then (clamped to 0.5 s back).
for r in ctx.rpcs_named("Fire") {
    let hit = ctx.ray_cast_rewound(r.from, origin, dir, 100.0);           // rewind by the shooter's latency
    // or: ctx.ray_cast_at(t as f64, origin, dir, 100.0);                 // rewind to the reported time
    let wall = ctx.raycast(origin, dir, 100.0);                            // geometry is not rewound
    if let Some(h) = hit.filter(|h| wall.is_none_or(|w| w.distance > h.distance)) { /* damage h.object */ }
}
```

`ctx.net_time()` is the session clock (the host's, as this peer estimates
it). `ray_cast_rewound` excludes the calling object and everything the
shooter owns; `ray_cast_at_filtered` takes an explicit exclude list. Only the
recorded hitboxes are rewound, not world geometry or physics bodies.

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
discovery, relay, room code, secret, Host Migration, Pin Host Identity, the
Allow List and Ban List (player fingerprints, one per line), the player prefab
spawned (owned by the player) for every peer that joins, the Spawn Point tag
those spawn at (round-robin by index), Dedicated Host, Clients Can Spawn, and
Auto Start for built games. Player objects are despawned when their player
leaves (a new host after a migration knows them too).

A spawned pawn only reads input on the peer that owns it: on other peers its
`possessed` flag is cleared before it starts.

## ComponentCtx API

```rust
// Session
ctx.net_connected(); ctx.is_server(); ctx.is_client();
ctx.local_peer();                 // 0 offline; the first host is 1
ctx.net_host_peer(); ctx.i_am_host(); ctx.host_changed(); ctx.net_migrating();
ctx.peer_identity(id); ctx.peer_fingerprint(id); // verified player ids
ctx.net_kick(peer, "reason", ban); // host
ctx.peers();                      // &[PeerInfo { id, name, rtt_ms, is_host, jitter_ms, loss, view_delay, identity, fingerprint }]
ctx.net_rtt_ms(); ctx.net_rtt();  // round trip to the host (ms / s)
ctx.net_quality();                // NetQuality { rtt_ms, jitter_ms, loss_in, loss_out, relayed }
ctx.net_time(); ctx.net_view_time(); // session clock; when remote objects shown now were current
ctx.ray_cast_at(t, origin, dir, max); ctx.ray_cast_rewound(shooter, origin, dir, max); // host, lag compensated
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
`Despawned`, `VarChanged`, `HostMigrating { previous }`,
`HostChanged { host, previous }`.

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
  (Network Manager spawn, or any owned Pawn, First / Third Person
  Controller, VR Player or Network Avatar) is within the
  object's Relevancy distance.

## Limitations

- Star topology through the host. The relay forwards between the host and
  its clients only (no client-to-client paths).
- Host migration needs a way to reach the successor: its LAN / public
  address, a punched endpoint, or the relay room. Clients behind NAT that
  joined a directly-hosted session (no relay) can't be reached by the others,
  so set a relay for internet sessions. The migration takes the host timeout
  to notice a crash (a graceful quit is immediate), and objects the host
  simulated pause meanwhile. A partition can split a session for the
  reconnect window; the tie-break only merges hosts that can reach each
  other. State the old host had not yet relayed from a member that has left
  too is lost; the lag-compensation history and the host's input queues
  start over.
- Prediction covers the built-in Pawn, First / Third Person Controller and
  VR Player (their movement and Character Controller); other movement
  components aren't replayed unless they implement `Predictable`. The host
  takes the owner's view angles, VR poses and teleport targets as sent (a
  teleport is only checked against the arc's reach). The host shows
  remote predicted pawns at their simulated position without smoothing, so on
  a listen server they can step when inputs arrive in bursts.
- Lag compensation rewinds recorded hitboxes only (not world geometry or
  physics), sampled once per host frame.
- Snapshots are stamped with the sender's clock; the receiver estimates the
  offset (so delay = transit + `interp_delay`); there is no shared tick.
- Network ids are full 128-bit object ids (16 bytes per entry).
- The play session only spawns prefabs with a renderer (headless servers
  spawn via `net_clone` of scene templates).
- After `net_load_scene`, other world changes are held for 0.5 s so they land
  in the new scene; a very slow scene load can still miss them.
- Security: the first contact has no prior trust: an on-path attacker that
  races a forged CHALLENGE ahead of the host's can stall that join (not
  break into the session), and without Pin Host Identity a client can't tell
  a different host behind the same address. Player ids are per install, not
  accounts. The relay join key is sent to the relay in the clear (see
  Relay authentication). An older client gets a clear version error from a
  protocol 6 host (rate-limited, the only pre-auth reply that can be larger
  than its request).

## Voice chat

Add a `Voice Chat` component to a networked scene (next to the Network
Manager); without one voice stays off. It sets proximity or global range, the
falloff (natural inverse-distance or linear between "full volume within" and
"silent past"), the transmit mode (voice activity with a dBFS threshold, push to
talk on an input action, or always) and mic gain.

Frames are 20 ms of 16 kHz mono. With the system's libopus (loaded at run time,
so builds need no C toolchain) they are Opus at the configured bitrate with
in-band forward error correction; without it they go as PCM. The first payload
byte names the codec, so mixed peers interoperate. Introduced in protocol 4 (current: 6).

Each speaker plays through an adaptive jitter buffer: arrival jitter is measured
against the 20 ms cadence and the buffer holds just enough (40 to 300 ms). A
missing frame is rebuilt from the next packet's error-correction data, else
concealed by the codec; a long gap re-buffers; when the buffer runs long, quiet
frames are dropped to win the latency back. Voices are positioned (stereo) at
the speaker's Network Avatar head.

Everything that plays also feeds lip sync (see the `Lip Sync` component), so
mouths move with what you hear.

