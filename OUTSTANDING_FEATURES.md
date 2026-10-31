# Citrus — Outstanding Features Backlog

Re-verified against the code on 2026-10-30. The detailed write-up, including the platform plan for Windows and Android, is in [ENGINE_REVIEW_2026-10-30.md](ENGINE_REVIEW_2026-10-30.md). Things Citrus already does are in [Features.md](Features.md).

The earlier P0/P1 gaps from the June audit are now **done**:
- runtime UI (screen, world and VR, plus the UI Editor)
- prefabs, now with internal reference remapping
- physics queries, triggers and joints
- the audio mixer and buses
- animation state machines (Animator Controllers)
- particles and the navmesh
- the save API and localization
- the event bus (`ctx.emit`)
- seamless and additive scene loading
- occlusion culling
- planar reflections
- LOD switching (`LodComponent`)
- GPU instancing of repeated draws, posed skinned-mesh bounds, TLAS refit
- full-body tracking (`Humanoid IK`: 11 tracker roles, procedural stepping), remote humanoid IK and networked UGC avatars
- LOD generation (quadric-error decimation per LOD level), compact scene files (model instances stored as their changes, unedited model materials as references)
- cloth: Bone Cloth, Mesh Cloth (self-collision), Cloth Colliders with capsule auto-fit, Wind Zones, VRM spring-bone import
- face tracking (OpenXR + OSC), lip sync, SlimeVR, voice v2 (Opus, adaptive jitter buffer, proximity)
- water: ocean / lake / river bodies, Gerstner waves with wind seas, buoyancy, drag and flow, interactive ripples, surface rendering with refraction and foam
- water simulation (shallow-water + dispersive layers, ground bed and beach run-up, viscosity and fluid templates), underwater view and audio, waterfalls, carryable water, swimming (desktop controllers and VR)
- physics materials with static and dynamic friction, authored UCX_ collision, per-object Cast Shadows
- multi-object editing with mixed values, Group Into Empty

**Priority:** P1 = expected of a serious engine · P2 = advanced / polish.
**Effort:** S ≈ days · M ≈ 1–2 weeks · L ≈ several weeks · XL ≈ months.

## P1

| Feature | State | Effort | Note |
|---|---|---|---|
| GPU-driven culling / indirect draws | ◑ | M | Landed (compute frustum + optional two-phase Hi-Z culling, indirect draws for main, prepass, shadows and VR eyes; the late Hi-Z pass re-tests what last frame's depth hid against this frame's, so disocclusions draw the same frame); follow-up: GPU-side shadow-cache signature. Verified on an RTX 4080 SUPER (GPU and CPU culling render the same; 2026-10-31). Lightmapped statics batch: the layer rides in each instance (col0.w). |
| GPU instancing across materials | ❌ | M | Instance draws that share a mesh but differ in material values (per-instance colours / parameters in an instance buffer, like Unity's per-instance properties), so material variants don't break batches. |
| Render-to-texture: portals | ◑ | S | Cameras-to-texture with Hz throttling, off-screen skip and live resize landed; screens inside screens and portals open. |
| Reflection probes: >2 blending | ◑ | M | Per-probe cubes and two-probe blending landed everywhere (forward, SSR, RT, custom shaders); three or more at one pixel is open. |
| Android / Quest bring-up | ◑ | S | Builds and packages (Android + Meta Quest targets, OpenXR on Android, Vulkan 1.1 fallback, ASTC cook, XR-only multiview frames, mobile tier). ASTC HDR (vendored ARM astcenc) and compressed model-embedded textures landed 2026-10-31. Never run on a device: on-device validation, `XR_FB_foveation`. |
| Windows on real hardware | ◑ | S | Cross-build verified under Wine only. The C/C++ runtime is linked statically (no VC++ redistributable needed). |
| Undo coverage | ◑ | M | Transform, component, material and Cast Shadows edits are undoable, and so are structural edits (delete, duplicate, reorder, re-parent, group) as object-list snapshots (2026-10-31); Environment edits share the scene history; the Animation and Animator Controller tabs keep their own (Ctrl+Z while the tab has focus). Skybox image changes aren't undoable. |
| Water: shoreline | ✓ | M | Underwater fog, waterline, blur, caustics and the ripple field in the shader landed. Shore wave damping landed 2026-10-31: each lake / pool / ocean bakes the depth of water over the static ground (128x128 over its area, rebaked when it or the ground changes) and its waves, normals and crest foam calm over water shallower than `Shore calm depth` (default 2 m; 0 = off); floating bodies ride the calmed waves too. Not applied to the RT-reflection displaced water (`water_displace.comp`). |
| Asset cook | ✓ | S | Builds ship only reachable files. Desktop builds pack them into `assets.pak`, unpacked once per build into a per-user cache. |
| Compressed bakes | ◑ | S | BC6H `.lightmap` v2 / `.reflprobe` v4 with a fallback for GPUs without BC (E5B9G9R9 / RGBA16F decoded at load): lightmaps 48 MB -> 3 MB, probe 96 MB -> 8 MB. Quest builds re-encode bakes as ASTC 4x4 HDR at cook time (2026-10-31); ASTC data also decodes on GPUs without it. The Baker tab shows the measured BC6H lightmap error against the raw bake. |
| NAT relay / internet play | ◑ | S | Authenticated relay (cookies, owner-keyed rooms, join proofs, operator token, per-IP / pending / backoff limits), hole punching with relay fallback, room handover to a migration successor. Claims are limited to the best-ranked live successor of a silent host (2026-10-31). Client-to-client relay paths are not planned: sessions are a star. |
| Client prediction / lag compensation | ◑ | S | Pawn, First / Third Person Controller, VR Player and any component implementing `Predictable` predicted, surviving host migration; adaptive interpolation; `ray_cast_at` / `ray_cast_rewound`. Listen-server hosts smooth remote predicted pawns. Left: lag-compensation history restarts on a new host. |
| Net encryption / auth | ◑ | S | Argon2id passphrases, transcript-bound keys, authenticated REJECT / BYE / kick, Ed25519 player identities with allow / ban lists and host pinning, stateless cookies, half-open cap. A client tries other CHALLENGEs when its JOIN goes unanswered, so one raced forgery no longer stalls a join (2026-10-31). The relay join key is sealed to the relay (relay protocol 3). Account login (`CITRUS_ACCOUNT` / `CITRUS_PASSWORD`, host `CITRUS_ACCOUNTS`) landed 2026-10-31. |

## P2

| Feature | State | Effort | Note |
|---|---|---|---|
| Humanoid retargeting | ❌ | L | |
| Sub-state machines, curve editor | ◑ | M | Sub-state machines done (2026-10-31): created from the Animator Controller canvas menu, double-click to edit inside (breadcrumb path to go back up), transitions to / from them at the parent level; the runtime plays them flattened, inner states named `Outer/Inner`. Still missing: the curve (tangent) editor in the Animation tab. |
| Projected decals | ❌ | M | |
| Terrain + foliage | ❌ | XL | |
| Volumetric clouds | ◑ | L | Volumetric fog exists. |
| Visual shader graph | ❌ | L | Shaders are code plus inspector properties. |
| Logic in UGC (WASM scripts) | ❌ | L | See docs/user-content.md. |
| Per-package GPU freeing | ◑ | S | Freed on world switch, not on unload. |
| Cinematics / sequencer | ❌ | XL | |
| AI behaviour trees + perception | ❌ | L | |
| Vehicle / destruction | ❌ | XL | |
| Virtual shadow maps, meshlets | ❌ | XL | |
| Console platforms | ❌ | XL | |

## Suggested sequence

1. Android / Quest 3: on-device validation.
2. Asset cook (pack shipped files into one archive).
3. In-headset validation of body calibration and avatar scaling; GPU cloth.
