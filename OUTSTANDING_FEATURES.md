# ZestEngine — Outstanding Features Backlog

Re-verified against the code on 2026-10-30. The detailed write-up, including the platform plan for Windows and Android, is in [ENGINE_REVIEW_2026-10-30.md](ENGINE_REVIEW_2026-10-30.md). Things ZestEngine already does are in [Features.md](Features.md).

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

Update 2026-11-01: the remaining partial (◑) follow-ups that don't need hardware were completed in a parallel fan-out — GPU shadow-cache signature, portals / nested screens, 3+ probe blending, skybox undo, RT-reflection shore calm, lag-comp history across host migration, the curve/tangent editor, and per-package GPU freeing. A second wave the same day landed a first increment of every missing (❌) feature below (now ◑ with the remainder noted), plus a live-GPU run of the editor against the RTX 4080 SUPER.

**Priority:** P1 = expected of a serious engine · P2 = advanced / polish.
**Effort:** S ≈ days · M ≈ 1–2 weeks · L ≈ several weeks · XL ≈ months.

## P1

| Feature | State | Effort | Note |
|---|---|---|---|
| GPU-driven culling / indirect draws | ✓ | M | Landed (compute frustum + optional two-phase Hi-Z culling, indirect draws for main, prepass, shadows and VR eyes; the late Hi-Z pass re-tests what last frame's depth hid against this frame's, so disocclusions draw the same frame); GPU-side shadow-cache signature done 2026-11-01 (the shadow layer cache now keys the full eligible caster list the GPU cull tests). Verified on an RTX 4080 SUPER (GPU and CPU culling render the same; 2026-10-31). Lightmapped statics batch: the layer rides in each instance (col0.w). |
| GPU instancing across materials | ◑ | M | Per-instance colour + one parameter vec4 landed 2026-11-01 (`PerInstance` component, 96-byte instance records in the CPU and GPU-cull paths, opt-in `MaterialFeatures::per_instance` specialization). Cross-material batch merging (descriptor-set dedup), multi-parameter and per-instance textures remain. |
| Render-to-texture: portals | ✓ | S | Cameras-to-texture with Hz throttling, off-screen skip and live resize landed; screens inside screens and portals landed 2026-11-01 (dependency-ordered rendering; throttled/off-screen cameras no longer poison the topo sort). |
| Reflection probes: >2 blending | ✓ | M | Per-probe cubes and two-probe blending landed everywhere (forward, SSR, RT, custom shaders); 3+ probe blending landed 2026-11-01 across forward, SSR and RT. |
| Android / Quest bring-up | ◑ | S | Builds and packages (Android + Meta Quest targets, OpenXR on Android, Vulkan 1.1 fallback, ASTC cook, XR-only multiview frames, mobile tier). ASTC HDR (vendored ARM astcenc) and compressed model-embedded textures landed 2026-10-31. Never run on a device: on-device validation, `XR_FB_foveation`. |
| Windows on real hardware | ◑ | S | Cross-build verified under Wine only. The C/C++ runtime is linked statically (no VC++ redistributable needed). |
| Undo coverage | ✓ | M | Transform, component, material and Cast Shadows edits are undoable, and so are structural edits (delete, duplicate, reorder, re-parent, group) as object-list snapshots (2026-10-31); Environment edits share the scene history; the Animation and Animator Controller tabs keep their own (Ctrl+Z while the tab has focus). Skybox image changes are now undoable (2026-11-01). |
| Water: shoreline | ✓ | M | Underwater fog, waterline, blur, caustics and the ripple field in the shader landed. Shore wave damping landed 2026-10-31: each lake / pool / ocean bakes the depth of water over the static ground (128x128 over its area, rebaked when it or the ground changes) and its waves, normals and crest foam calm over water shallower than `Shore calm depth` (default 2 m; 0 = off); floating bodies ride the calmed waves too. Applied to the RT-reflection displaced water (`water_displace.comp`) 2026-11-01, and the RT reflection shading normal calms the same way (2026-11-01). |
| Asset cook | ✓ | S | Builds ship only reachable files. Desktop builds pack them into `assets.pak`, unpacked once per build into a per-user cache. |
| Compressed bakes | ◑ | S | BC6H `.lightmap` v2 / `.reflprobe` v4 with a fallback for GPUs without BC (E5B9G9R9 / RGBA16F decoded at load): lightmaps 48 MB -> 3 MB, probe 96 MB -> 8 MB. Quest builds re-encode bakes as ASTC 4x4 HDR at cook time (2026-10-31); ASTC data also decodes on GPUs without it. The Baker tab shows the measured BC6H lightmap error against the raw bake. |
| NAT relay / internet play | ◑ | S | Authenticated relay (cookies, owner-keyed rooms, join proofs, operator token, per-IP / pending / backoff limits), hole punching with relay fallback, room handover to a migration successor. Claims are limited to the best-ranked live successor of a silent host (2026-10-31). Client-to-client relay paths are not planned: sessions are a star. |
| Client prediction / lag compensation | ✓ | S | Pawn, First / Third Person Controller, VR Player and any component implementing `Predictable` predicted, surviving host migration; adaptive interpolation; `ray_cast_at` / `ray_cast_rewound`. Listen-server hosts smooth remote predicted pawns. Lag-compensation history now restarts on the continued session clock at host migration (2026-11-01). |
| Net encryption / auth | ◑ | S | Argon2id passphrases, transcript-bound keys, authenticated REJECT / BYE / kick, Ed25519 player identities with allow / ban lists and host pinning, stateless cookies, half-open cap. A client tries other CHALLENGEs when its JOIN goes unanswered, so one raced forgery no longer stalls a join (2026-10-31). The relay join key is sealed to the relay (relay protocol 3). Account login (`ZESTENGINE_ACCOUNT` / `ZESTENGINE_PASSWORD`, host `ZESTENGINE_ACCOUNTS`) landed 2026-10-31. |

## P2

| Feature | State | Effort | Note |
|---|---|---|---|
| Humanoid retargeting | ◑ | L | First increment 2026-11-01: precomputed role-to-role bone mapping with rest-pose normalization, locomotion scaling and `retarget_clip` (public API + tests). IK-aware reach/limb constraints and the retarget UI / automatic Animator binding remain. |
| Sub-state machines, curve editor | ✓ | M | Sub-state machines done (2026-10-31): created from the Animator Controller canvas menu, double-click to edit inside (breadcrumb path to go back up), transitions to / from them at the parent level; the runtime plays them flattened, inner states named `Outer/Inner`. Curve/tangent editor in the Animation tab landed 2026-11-01 (graphical curve with draggable tangent handles). |
| Projected decals | ◑ | M | Deferred world-space box-projector pass landed 2026-11-01 (depth + G-buffer normal, opaque only, tint/fade/tiling, editor gizmo + inspector). Normal-bent projection, emissive/alpha modes and transparent surfaces remain. |
| Terrain + foliage | ◑ | XL | Heightmap terrain landed 2026-11-01 (import, heightfield mesh + TerrainCollider, 4-layer splat material, painted weights, editor controls). Foliage scatter, per-texel splat control map and chunking/LOD remain. |
| Volumetric clouds | ◑ | L | Volumetric fog exists. |
| Visual shader graph | ◑ | L | Node editor + typed data model + GLSL codegen landed 2026-11-01 (`.shadergraph` files compile through the same glslc path; dock tab with drag-to-connect ports). More node types, sub-graphs/functions and a live material preview remain. |
| Logic in UGC (WASM scripts) | ◑ | L | Sandboxed `Script` component landed 2026-11-01 (wasmtime, no WASI, fuel + memory caps, start/update hooks, `spin.wat` example, sandbox tests). Package integration (`.wasm` in paks), a wider API surface and cross-module calls remain. |
| Per-package GPU freeing | ✓ | S | Freed on package unload (2026-11-01): meshes, textures, materials, skinning state and pending uploads freed immediately; shared and base-scene resources retained. |
| Cinematics / sequencer | ◑ | XL | Sequencer landed 2026-11-01: `.seq` RON model, timeline editor (tracks, keys, curves, FOV), edit/play playback driving transforms + camera. Cutting, event tracks, camera blending and audio tracks remain. |
| AI behaviour trees + perception | ◑ | L | Behaviour trees landed 2026-11-01 (`.behaviour` RON asset, sequence/selector/condition/action nodes, typed blackboard, perception cone + raycast, editor UI). Persistent blackboard authoring, more decorators and navmesh-aware actions remain. |
| Vehicle / destruction | ◑ | XL | Wheeled Vehicle landed 2026-11-01 (raycast suspension, steering, throttle/brake, grip, fixed-step integration, controller input). Destruction, more vehicle types and enter/exit seats remain. |
| Virtual shadow maps, meshlets | ◑ | XL | Meshlet generation landed 2026-11-01 (`build_lod_meshlets`: decimation + clustering sharing one index space, retained per LOD). No GPU consumer yet; the VSM page primitives remain unwired into the shadow pass. |
| Console platforms | ◑ | XL | Groundwork only: `CONSOLE_BRINGUP.md` (2026-11-01) audits platform seams and proposes cfg layering. No SDK in the repo; the Phase A FSR3/link gating refactor is documented, not shipped. |

## Suggested sequence

1. Android / Quest 3: on-device validation (and `XR_FB_foveation`).
2. Windows on real hardware: validate the native toolchain.
3. In-headset validation of body calibration and avatar scaling; GPU cloth.
