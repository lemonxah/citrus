# citrus — Features & Goals

Tracks engine capabilities: **what we already support** vs **what we still want to
build**. Working bug/backlog list lives in [TODO.md](TODO.md); high-level milestones
in [README.md](README.md). This file is the feature-level map — update it when a
capability lands or a new goal is set.

Legend: `[done]` implemented · `[partial]` partial / needs validation · `[todo]` not started

---

## 1. Implemented features

### Rendering
- [done] Vulkan 1.3 renderer (ash 0.38, dynamic rendering, sync2)
- [done] PBR standard shader (metal/rough, base/normal/emission), multi-light frag loop —
  energy-conserving Cook-Torrance (Fresnel kS / diffuse kD = 1-F) + roughness-aware ambient;
  indirect term fed by baked lightmaps / probe SH (else flat ambient)
- [done] Lights: directional / point / spot — color, intensity, range, spot angle/blend
  (up to 16/frame, distance attenuation + spot cones)
- [partial] Shadow-casting lights (shadow-map array, PCF; needs acne/bias GPU validation).
  Per-light **Shadows** dropdown: No Shadows / Hard Shadows (single depth tap, sharp) /
  Soft Shadows (5x5 PCF penumbra). The filter mode rides the sign of the light's shadow
  view-count, so no extra GPU light field is needed.
- [done] **Baked soft shadows** via a per-light **Radius** (light source size): the bake
  aims each shadow ray at a random point on a disc of that radius, so shadows get a smooth
  penumbra that hides the texel stair-stepping at low texel density (instead of a hard,
  jagged edge). 0 = hard. Realtime shadows are unaffected (they already PCF-soften).
- [done] Skybox: procedural gradient + equirect LDR image (per-scene)
- [done] Selection outline (inverted hull, depth-prepass, always-on-top)
- [done] Error shader (animated swirl for broken/missing/unknown shaders)
- [done] VSync toggle (FIFO / MAILBOX / IMMEDIATE)
- [done] Stats overlay (frame time, draw-call breakdown, pipeline binds, shader variants)
- [done] Camera preview tab (renders scene from the main camera to an offscreen target)

### Assets & formats
- [done] glTF import (PBR factors + textures, lightmap UV via TEXCOORD_1)
- [done] FBX import via ufbx (per-material parts, PBR factors, base/normal/emission,
  embedded textures)
- [done] Procedural primitives (cube/sphere/capsule/plane)
- [done] `.material` files (RON): save/load/assign, per-path texture cache
- [done] `.scene` files: full save/load (objects, transforms, parents, components, materials)
- [done] `project.citrus` (RON): project name, last scene, per-project settings
- [done] `.lightmap` / `.lightdata` sidecars (baked GI + probe SH)
- [done] Lightmap UVs (second UV set, generated atlas fallback)
- [done] Audio clips: `.wav` / `.flac` / `.mp3` (rodio)

### Lighting / baking
- [partial] GPU light bake (Vulkan ray query): BLAS/TLAS, lightmap path tracer (direct +
  shadow + multi-bounce indirect), light-probe SH-L1 bake. Built, needs GPU visual
  validation. **Runtime sampling**: 5a flat probe-average ambient (done); **5b
  per-fragment probe SH** (done) — probes uploaded to a set-0 storage buffer (binding 2)
  + volume metadata in the frame UBO; `standard.frag` finds the containing volume,
  trilinearly blends 8 probes, evaluates SH-L1 in the surface normal, and uses it as the
  indirect term (flat ambient fallback outside any volume). Active in **both** the editor
  viewport and the **game runtime** — `run_game` loads the scene's `.lightmap`/`.lightdata`
  sidecars (shared `LoadedScene::load_bake_sidecars`) and uploads the probes. The sidecars
  bundle automatically (they live in `scenes/`, copied with the scene). **5c
  per-object lightmaps (done)**: baked lightmaps upload as a `R32G32B32A32_SFLOAT` 2D array
  (one layer per static object, resampled to a common size; set-0 binding 3), `uv1` is
  forwarded through the vertex pipeline, and `standard.frag` samples the layer (per-object
  index in the push constant) for static-object GI — lightmap takes priority, else probes,
  else flat ambient. Active in editor + game. Bake output still needs visual validation.
- [done] **Bake light policy**: the bake captures **Baked + Mixed** lights *and* the
  environment sun/sky; **Realtime lights are never baked**. Once a bake exists, those
  baked lights (incl. the env sun) drop from the realtime pass to avoid double-counting;
  with no bake they all stay realtime, so an un-baked scene is never dark (baking is
  opt-in). Default light mode is Realtime. Only **Static** objects get lightmaps.
- [done] Per-object lightmap controls (Unity-style): a **Static** toggle in the
  inspector header (object is baked as a lightmapped surface + ray-trace occluder)
  and a **Scale In Lightmap** multiplier in the Mesh section that scales that object's
  texel density up/down from the scene default (sharper surface or fewer texels).
- [done] Primitive lightmap UVs: a second UV set (`uv1`) — plane/sphere/capsule reuse their
  single non-overlapping `uv0` chart; the **cube packs its 6 faces into a non-overlapping
  3×2 atlas** (with a gutter) so faces don't share lightmap texels. Imported meshes use
  their 2nd UV set, or `uv0` (glTF) / a planar unwrap (FBX) when absent.
- [done] **Bake denoise + seam fix**: after the path trace, each lightmap is run through
  an edge-aware **À-Trous denoiser** (CPU; reads back the position+normal gbuffer, weights
  a 5×5 wavelet blur by world-position + normal similarity so it smooths MC grain without
  crossing shadow/geometry edges or UV-chart seams), then **seam-stitched** (co-located
  cross-chart texels — the cube's per-face atlas, the sphere's lat-long meridian — are
  averaged when their normals agree, so the chart boundary stops showing as a line while
  genuine hard-edge discontinuities are kept), then **dilated** (valid texels spread into
  the gutter) so bilinear sampling at chart edges never reads the black background.
- [done] **ACES tonemapping**: the standard + skybox shaders roll HDR highlights off to
  [0,1] (Narkowicz ACES, in linear before the sRGB swapchain write), so a close point light
  or stacked ambient + baked bounce shows surface detail instead of clipping to flat white.
- [done] **Realtime GI**: an Environment-tab setting (serializes with the scene, so it runs
  in the editor *and* a shipped game) that, while the scene isn't baked, re-traces an auto
  probe grid from the realtime lights (reuses the ray-query path tracer with `probes_only`),
  temporally blends the SH, and uploads so surfaces show live indirect bounce. Settings:
  Enabled, Bounces, Quality (rays/probe), Intensity, Probe Spacing, Responsiveness (temporal
  blend), Update Interval. Driven by a shared `RealtimeGiState` with **dirty-detection** —
  it only re-traces when lights/objects/settings change, then settles and goes idle, so a
  static scene does no work. The **Software (SDF) march runs on a background thread**, so
  moving objects (Play mode) don't hitch the frame; the main thread blends + uploads when a
  trace finishes. Hardware (ray-query) mode is still synchronous + rebuilds accel structures
  each trace — GPU async / resident-accel is the follow-up.
- [done] Approximate **lumens/lux readout** under a light's Intensity (display only — our
  intensity stays a radiance multiplier; point = 4π·I, spot = cone solid angle ·I, dir = lux).
- [wip] **Software GI (Lumen-style, no RT cores / no bake)**: a second realtime-GI **Mode**
  (Environment tab → Hardware (RT cores) | Software (SDF)) that marches per-mesh signed
  distance fields instead of the hardware BVH. Phase 1a (mode setting + UI) and 1b (per-mesh
  CPU SDF generation — `sdf::generate_sdf`, closest-point-on-triangle distance + nearest-tri
  normal sign, unit-tested) are done. Phase 1c is a CPU **multi-bounce** path march
  (`sw_gi.rs`, honors the Bounces setting, throughput ×albedo per hop) reusing the SDFs,
  **parallelized across cores** on a **background thread** (now the CPU fallback). The default
  path is a **GPU compute march** (`sw_gi.comp` + `gpu_gi.rs`): the per-mesh SDFs are merged
  CPU-side into one **Global Distance Field** (`sw_gi::build_gdf` → a 3D distance texture +
  nearest-instance index texture), and a compute shader marches that single field per probe (one
  texture sample per step instead of looping meshes), writing the packed probe layout directly —
  far cheaper than the CPU march, so it runs synchronously per re-trace. The GDF is **cached on the
  GPU** (`Renderer::gi_set_gdf`) and re-uploaded only when a geometry/materials/bounds hash
  (`hash_gdf_inputs`) changes, so a static scene keeps a high-res field for free while lights and
  emitters move; `Renderer::gi_march` then runs each trace against the cached field.
  `gi_gpu_available()` gates the whole path — when compute init failed it returns false and the
  driver builds nothing, marching on the CPU thread instead. Each fresh trace is **spatially denoised**
  first — a separable [1,2,1] blur over the
  probe SH grid (`sw_gi::blur_probe_grid`) that cancels the blotchy per-probe Monte-Carlo
  variance with **no temporal lag**, so Responsiveness can run high (snappy updates to moving
  objects) without trading back into noise. The denoised trace is then blended in with a
  **motion-aware EMA**: while a light/emitter is *moving* it snaps toward the latest trace (rate
  = Responsiveness) so the bounce tracks in realtime; when *static* it averages at a fixed gentle
  rate so residual per-trace variance converges smoothly — so raising Responsiveness never makes
  a still scene flicker. A short per-frame ease (faster while moving) glides between updates (cheap
  in-place SSBO rewrite `update_probe_sh`, no GPU stall). The probe grid is **cascaded**
  (SDFGI-style): the coarsest volume covers the whole padded scene AABB and each finer cascade
  halves the box (doubling density) around the same center, up to 3 cascades (16/axis each for
  software, single 32 grid for hardware). The shader picks the finest cascade containing a
  fragment and **cross-fades into the next coarser one near its boundary** (`sample_volume` +
  edge fade in `standard.frag`) so the resolution change isn't a visible seam — this is what
  removes the trilinear "squares" near the action while keeping edges/sky cheap. The 8-corner
  blend also **smoothsteps the trilinear factor** (Hermite, C1-continuous across cell boundaries),
  so the per-cell gradient kink that reads as faceting/banding on smooth falloff is gone without a
  finer grid. All cascades are
  concentric (scene-centered) so the cross-fade lines up and the GI doesn't shift with the camera.
  Each cascade is
  blurred by its own grid layout. **DDGI-style visibility (leak prevention)**: the march also
  records the SH-L1 of the directional first-hit distance per probe (`ProbeSh::dist`, packed into
  the probe SSBO's previously-unused `.w` lanes — no extra buffer). The shader replaces plain
  trilinear with a DDGI-style weighted 8-corner blend (`sample_volume` in `standard.frag`): each
  probe's weight is scaled by a soft Chebyshev-lite visibility test (probe→fragment distance vs.
  the probe's stored seen-distance in that direction) plus a front-facing term, so probes
  occluded from a fragment (behind a wall, under an object) are down-weighted → light no longer
  leaks. Bake/hardware paths leave `dist` zero, which disables the test (plain trilinear). **4
  bounces** cap so a CPU trace finishes inside the update interval; probe-spacing floor 0.25 m so
  a tiny value can't silently explode the probe count. **Emissive
  materials are area emitters** in both bake (static objects) + realtime GI, sampled by
  **next-event estimation (NEE)**: each emissive instance is reduced to a sphere area-light
  (`sw_gi::emitter_spheres`) the march samples *directly* — both the probe's direct view (added
  analytically to the SH) and at every bounce surface — instead of relying on random rays to hit a
  small bright surface. This removes the blotchy Monte-Carlo fill that otherwise rings an emitter
  (the dominant direct term is variance-free in a single trace; the dim indirect residual is cleaned
  by the temporal accumulation + grid blur). Implemented in both the CPU march and the GPU
  `sw_gi.comp` (emitter SSBO, binding 6). The headless `cargo run --example gi_preview` renders a
  minimal plane+emissive-sphere scene through the real march to a PNG for tuning without the editor.
  Next: surface cache
  / screen probes for contact-scale GI — probe GI is low-frequency, so tight contact fill
  (under-object darkening) is still limited.
- [todo] Lower-distortion primitive lightmap unwrap (octahedral sphere instead of lat-long;
  even cube-face packing) — the seam-stitch hides the seams but the lat-long sphere still
  wastes texels at the poles.
- [done] Baker's Man dock tab (texel density 1–1024 /m log, bounces, samples up to 65536,
  max size, Bake/Clear) + a **UV-checker preview** toggle: renders objects as a
  lightmap-UV checkerboard whose cell size tracks each object's would-be texel density
  (big squares = low resolution, stretched = UV distortion; grey = non-static) — live from
  the current bake settings, no re-bake needed.

### Post-processing
- [partial] **Unity Volume-style post-processing**: a `.postfx` profile asset (RON — tonemap
  mode/exposure, bloom, color grading, vignette, chromatic aberration) created from the file
  browser's New Post FX Profile; a **Volume (Post FX)** component (global/local, priority,
  weight, blend distance, box extents) references one; `LoadedScene::effective_postfx` blends
  the volumes affecting the camera (priority-ordered, weight × local proximity) into one
  profile fed to the shaders via the frame UBO. **Applied now (per-pixel, in standard+skybox
  shaders): exposure, tonemap (None/Reinhard/ACES), color grading (exposure/contrast/
  saturation/temperature/tint), vignette.** ACES moved out of hardcode into the profile.
  An in-editor **`.postfx` profile editor** (select the asset → sliders for tonemap, color
  grading, vignette, bloom, chromatic aberration; saves + live-invalidates the cache).
  [todo] Chromatic aberration + bloom rendering (need an offscreen-HDR fullscreen pass — the
  settings are authorable now but only apply once that pass lands).

### Editor
- [done] Play / Pause / Stop — Pause freezes components/physics/audio on a play clock that
  doesn't advance while paused (so time-based motion doesn't jump on resume)
- [done] Selected-camera preview overlay (bottom-right of the viewport): live view through a
  selected camera object so its framing can be tweaked while editing its transform
- [done] Drag a file from the Files panel onto an inspector asset field to assign it
  (`InspectCtx::file_field`; the browser publishes the dragged file's project-relative path to
  egui memory — plugin-safe). E.g. drop a `.postfx` onto a Volume's Profile field.
- [done] Dockable panels (egui_dock): Scene / Inspector / Files / Log / Code / Baker
  around a transparent viewport
- [done] Transform gizmos: move (W) / rotate (R) / scale (E), pivot/center + global/local,
  snap (grid + 15deg rotation), fat hit areas with orbit priority, hover emphasis
- [done] Camera-relative axis flipping + orientation cross on move/scale gizmos
- [done] Scene tree: nested hierarchy, drag-to-reparent, connector lines, Alt-click cascade,
  F2 inline rename of the selected object, Ctrl+D duplicates the selected object (whole
  subtree; shares meshes/materials, fresh ids, not undoable like delete)
- [done] Ctrl+D in the file browser duplicates the selected file/folder (`<stem>_copy`)
- [done] Project file browser (Unity-style folder tree + icon grid, rename/cut/copy/paste/
  drag-move, context menus, F2 inline rename of the selected file/folder). Per-type icons from
  the **Phosphor icon font** (`egui-phosphor`, registered via `install_icon_font`): globe
  (scene), sphere (material), cube (model), image, sparkle (shader), aperture (postfx), map
  (lightmap), database (lightdata), gear (config), file-text (markdown), orange-slice
  (.citrus), folder, file (unknown); `.rs` keeps a monochrome Ferris silhouette. Known asset
  extensions are hidden (the icon conveys the type) and names clip to one line. Folder clicks
  navigate only — no Inspector selection.
- [done] Unified Inspector (object transform/mesh/material slots, `.material` editor,
  component list with Add/Remove)
- [done] Orbit / pan / scroll-dolly editor camera, F to frame, Escape to deselect
- [done] Play mode (Play/Stop): components run, transforms + state restored on Stop
- [done] Undo/redo (move/rotate/scale/rename, material edits + assignments; delete is
  intentionally non-undoable)
- [done] Menu bar (File/Edit/Tools/View/Help), New/Open/Save scene
- [done] Viewport widget filter (per-billboard visibility + size)
- [done] Billboard widgets: lights, cameras (frustum), probe volumes (3 bulbs), audio (speaker)
- [done] Global bottom status bar: project + object count, live rust-analyzer activity
  spinner, and compile/result messages ("Compiling components…", shader reloads). Global
  minimum text size (folder-explorer 13px) so nothing renders unreadably small
- [done] Log console tab (tracing ring buffer, level filter + search, follow, wrap, timestamps)
- [done] Code editor tabs with a custom **Citrus Purple** syntect theme (solid-black
  background, purple-leaning palette, borderless text box); line-number gutter; fills the
  dock; debounced auto-save; bottom status line (mode / file / language / line:col /
  unsaved, folds in the vim `:` command line); caret stays solid while moving. Minimal
  header (problem counts + hint) — the tab name carries the filename
- [done] Vim mode (toggle in **Edit menu**, persisted in `project.citrus`; per-file
  mode): Normal / Insert /
  Visual / Visual-line / Command. Motions h j k l w b e 0 ^ $ gg {n}gg G {n}G (counts);
  i a I A o O; x D C dd cc yy dw cw yw d$/c$/y$ p P; visual d y c p; `u` undo / Ctrl+R
  redo (per-file snapshot stack, an insert session = one undo); `gd` go-to-definition,
  `gr` references (picker popup). Command line (`:`): `:w` write, `:q`/`:wq`/`:x` close,
  `:{n}` goto line, `[%]s/pat/rep/[g]` regex substitution (`$1`/`${name}` capture refs)
  with **live preview** — matches/replacements highlight as you type and revert on
  Escape, commit on Enter. Core subset — `f`/`t`, `/` search, `.` repeat can follow
- [partial] rust-analyzer LSP (diagnostics, completion, hover, go-to-definition,
  find-references on `.rs`); file browser badges files (and aggregates onto folders)
  with red/yellow problem dots, and live-updates as files change on disk
- [done] App icon (procedural citrus slice; X11 window icon + desktop entry install)
- [done] Crash handler (symbolized backtrace on SIGSEGV/SIGBUS/SIGILL/SIGABRT)

### Components
- [done] `TypedComponent` trait (serde + Default + inspector UI + start/update/late_update),
  `ComponentRegistry`, serialize into `.scene`, participate in undo
- [done] Built-ins: Camera, Light, LightProbeVolume, AudioSource, AudioListener,
  BoxCollider, SphereCollider, MeshCollider, Spin, Bob
- [done] Rust component plugins (`plugins/*` workspace crates, cargo-built + dylib-loaded,
  `citrus_register`, hot reload)
- [done] Runtime scene switching: `ComponentCtx::load_scene(path)` lets gameplay
  components change levels / go menu -> game during Play (first slice of the in-game API,
  2D); editor Open/New/Save Scene already covered authoring
- [done] Custom GLSL shaders v1 (runtime glslc, pragma-declared properties reflected into
  Inspector, hot reload)

### Audio
- [done] AudioSource (clip, play-on-start, loop, volume, pitch) + Spatial toggle (min/max
  distance, linear/log rolloff); AudioListener; driven in Play mode

### Physics / collision
- [done] Collider components (Box / Sphere / Mesh-convex) with is_trigger + layer, spawnable
  standalone or as components, yellow editable viewport widgets
- [partial] Physics simulation (rapier3d): a `RigidBody` component (Dynamic / Kinematic /
  Fixed, mass, restitution, friction, gravity scale). On Play (editor) and scene load
  (game) the engine builds a rapier world — colliders become cuboid/ball shapes (Mesh ->
  AABB cuboid), `RigidBody` objects get that body kind, collider-only objects become fixed
  (static) bodies — steps it under gravity each frame, and writes the simulated transforms
  back. Foundational slice; still todo: joints, layer-collision matrix, queries
  (raycast/overlap), trigger events, CCD tuning, parented-body world↔local conversion.

---

## 2. Goals — to be implemented

### 2A. Pawns & camera possession [todo]
A **Pawn** is a controllable entity that a controller can "possess"; it owns movement
state and can drive which scene camera is active.

- [todo] `Pawn` component: identity + movement params (mass, gravity, jump power, move
  power per-direction, max speed, accel/decel, ground friction, air control) editable
  in the Inspector **and** via the in-game API
- [todo] Possession model: a controller possesses/unpossesses a Pawn; only the possessed
  Pawn receives input
- [todo] Active-camera registry: scene cameras enumerable by id/name; API to set the active
  render camera at runtime (`set_active_camera(cam)`), independent of which Pawn is
  possessed
- [todo] Camera-follow modes wired to the controller type (rig per controller below)
- [todo] Pawn / physics binding: movement applies forces/velocity through the RigidBody
  (physics engine), not direct transform writes, when a body is present
- [todo] Serialize Pawn params in `.scene`; restore on Stop like other play state

### 2B. Player controllers [todo]
Controllers translate **bindings** (abstract actions) into Pawn movement. Shared
`Controller` interface; concrete movement per type. All support the basic verbs the
controller needs (move fwd/back/left/right, jump, look, etc.).

- [todo] `Controller` trait/interface: consumes an action snapshot from the binding system,
  produces movement intent for the possessed Pawn (decoupled from raw input devices)
- [todo] **First-person** controller: WASD + mouselook, jump, optional crouch/sprint;
  camera at eye position
- [todo] **Third-person** controller: WASD relative to camera, orbit camera rig with
  collision spring-arm, jump
- [todo] **Isometric / top-down** controller — two modes:
  - [todo] click-to-move (navmesh/raycast-to-ground pathing)
  - [todo] WASD direct movement in iso space
  - [todo] fixed isometric camera rig
- [todo] **Strategy** controller: edge/WASD camera pan, zoom, rotate; no possessed body
  (camera-only) or unit selection later
- [todo] Movement params read from the Pawn (jump power, move power, etc.), physics from the
  RigidBody — controllers stay device- and physics-agnostic
- [todo] Inspector: pick controller type per Pawn; expose its tunables

### 2C. Input binding system [todo]
Control schemes that are **independent of the controller** but share one interface, so
each game can mix keyboard+mouse and/or gamepad freely.

- [todo] **Action** abstraction: named actions (e.g. `MoveX`, `MoveY`, `Jump`, `Look`,
  `Fire`) typed as button / 1D axis / 2D axis
- [todo] **Binding**: map physical inputs (key, mouse button/axis, gamepad button/stick/
  trigger) to actions; composite bindings (WASD to 2D axis), modifiers (invert, deadzone,
  scale), chords
- [todo] **Control scheme**: a named set of bindings (e.g. "KB+Mouse", "Gamepad"); active
  scheme switchable, auto-switch on last-used device
- [todo] Device backends: keyboard + mouse (winit) now, **gamepad** via `gilrs`
- [todo] Per-frame action snapshot the controller reads (`ctx.input.action("Jump").pressed()`,
  `.axis2("Move")`) — exposed through the in-game API
- [todo] Serialize schemes/bindings to a project asset (`.bindings` or in `project.citrus`);
  editor UI to author them (rebinding screen pattern)
- [todo] Runtime rebinding API (for in-game key-remap menus)

### 2D. In-game API (scripting surface for components) [todo]
Other engines expose the world to gameplay scripts; today `ComponentCtx` only hands a
component **its own local TRS + dt/time**. Goal: a real API surface that non-editor
components use to read/affect the world, identical in editor Play mode and in a built
game.

- [todo] Expand `ComponentCtx` (or a new `World`/`Api` handle) with:
  - [partial] **Self transform**: local read/write (done); `self_transform()` world read
    (done); `set_world_position(world)` world-space *write* (done — converts through the
    parent chain via `parent_world`, so a nested object lands at the right world spot);
    world-space rotation/scale write still todo
  - [todo] **Smoothed / lerped transform moves**: instead of snapping when a component
    sets a transform, ease toward the target so motion looks nicer. Surface:
    `move_towards(target, max_delta)` (constant speed, frame-rate independent via dt),
    `lerp_position(target, smoothing)` / `slerp_rotation(target, smoothing)` (exponential
    smoothing, `1 - exp(-smoothing * dt)` so it's stable at any framerate), and a
    higher-level tween (duration + easing curve: linear / ease-in-out / spring) that runs
    a transform change over time. Works on self now, on referenced objects once world
    *set* lands. Each operates in world space and converts through `parent_world` like
    `set_world_position`.
  - [partial] **Object graph**: every object has a stable UUID (`ObjectId`, assigned at
    create, serialized in `.scene`); `ObjectRef` field type + inspector **drag-drop
    target** (drag an object from the Scene tree onto the reference box; ✕ clears);
    resolve via `ctx.resolve`/`transform_of`/`position_of`/`index_of`/`self_id`;
    `find_object(name)` kept as a convenience. tags, parent/children, spawn/despawn,
    set-active still todo
  - [partial] **Transforms of other objects**: any object resolves to a `Transform`
    (translation/rotation/scale, + forward/right/up/matrix) via `object_transform` /
    `object_transform_named` / `object_position` / `object_matrix` (world snapshot);
    *set* still todo
  - [todo] **Components**: get/add/remove a component on any object; typed access
  - [todo] **Input**: read the binding-system action snapshot (2C)
  - [todo] **Physics queries**: raycast, shape-cast, overlap; apply force/impulse; collision/
    trigger enter/stay/exit callbacks delivered to components
  - [todo] **Shaders / materials**: set material properties + shader params at runtime
    (the pragma-declared props), swap materials
  - [todo] **Colliders**: toggle, resize, change layer at runtime
  - [todo] **Camera control**: set active camera, set FOV/post params (ties to 2A)
  - [todo] **Audio**: play/stop one-shots, change volume/pitch, spatial params
  - [todo] **Lights**: color/intensity/range/enabled at runtime
  - [partial] **Time / scene**: time scale, pause, app quit [todo]; **load/switch scene
    is done** — `ComponentCtx::load_scene(path)` (the first in-game-API slice) queues a
    `ComponentCommand` the engine applies after the update pass; switches levels / menu
    -> game during Play, continues playing in the new scene, and Stop returns to the
    pre-play scene.
  - [todo] **Events / messaging**: component-to-component messages or a simple event bus
- [todo] Stable, safe surface usable from plugin components (not just built-ins) without
  reaching into editor internals — likely a `citrus-api` crate both editor and plugins
  depend on

### 2E. Editor-only vs gameplay components [todo]
Two component kinds sharing one trait; the only difference is whether they run in a
shipped game.

- [todo] Add an **`EDITOR_ONLY`** marker to `TypedComponent` (const or trait method),
  default false
- [todo] Editor-only (e.g. LightProbeVolume, gizmo helpers): run/draw in the editor, **never
  in the built game**; excluded from the game runtime's update loop
- [todo] Gameplay components (Spin, Bob, Orbit, all unmarked custom ones): run identically in
  editor Play mode and the built game, through the 2D API
- [todo] Registry/inspector aware of the distinction (badge editor-only components); ensure
  serialization + the future game-build path strip editor-only behavior cleanly

### 2F. Game UI system (runtime UI) [todo]
In-game menus / inventory / HUD. **The developer picks the approach per project** —
both are first-class and can coexist (e.g. egui debug overlay on top of a retained HUD):

- **A. Retained scene-graph UI** (the citrus-native system, below) — Unity uGUI-style:
  widgets are scene objects under a `UICanvas`, **visually authored** in the editor and
  serialized into `.scene`, **screen-space + world-space** (world-space for VR
  controller-ray interaction). Best for polished, designer-authored, VR, and
  shipped-game UI. This is the default and the larger build.
- **B. Immediate-mode UI (egui)** — opt-in, code-driven. The same egui the editor uses,
  exposed to gameplay so a component builds its UI each frame in Rust. Best for debug
  HUDs, dev tools, prototypes, and devs who already know egui. Lighter to author (no
  scene wiring), but not visually authored and weaker for world-space/VR.

How the choice works in a build:
- [todo] egui (option B) is always **available** — `citrus-render` keeps the egui pass in
  every build (~1.5M; not worth gating out). To use it a game just opts in at the API
  level: `run_game` hands each frame's egui `Context` to a game callback, and
  `FrameInput.egui` carries the tessellated output exactly as the editor's path does (the
  plumbing already exists; a default game leaves it `None`).
- [todo] The retained system (A) never requires egui; egui (B) never requires the
  retained system. A project can use both — retained HUD + an egui debug panel.
- [todo] Editor authoring (visual rect editing, inspector wiring) applies to the retained
  system only; egui UI is authored in code.

The rest of this section specifies the **retained scene-graph system (A)**.

Foundations
- [todo] `UICanvas` component: the UI root. Mode = **Screen-space** overlay (reference
  resolution + scale mode: constant-pixel / scale-with-screen / match-w-h) or
  **World-space** (a quad in the scene at the object's transform, sized in world units).
- [todo] `UIRect` (RectTransform-style) on every UI widget: anchors (min/max), pivot,
  offsets/size; parent rect resolved top-down each frame so children lay out relative to
  the parent. Replaces/augments the normal Transform for UI objects.
- [todo] **2D UI renderer** in citrus-render: batched quads (panels/images, solid color
  or texture, optional 9-slice), per-canvas clip/mask rects. Screen-space drawn as an
  overlay pass after the scene; world-space drawn as scene geometry (so it depth-sorts
  and is hittable by a 3D ray).
- [todo] **Text rendering**: font atlas / glyph cache (candidate: `fontdue` or `ab_glyph`
  for raster, `cosmic-text` if shaping/i18n needed), SDF optional for crisp scaling;
  alignment, wrapping, color. New subsystem — the editor's egui fonts don't carry over.

Widgets (UI components)
- [todo] `UIText` — string, font, size, color, alignment, wrap
- [todo] `UIImage` / `UIPanel` — sprite/texture or solid fill, tint, 9-slice
- [todo] `UIButton` — visual states (normal / hover / pressed / disabled) + transition;
  `onClick`
- [todo] `UICheckbox` — checked bool, toggle, `onValueChanged`
- [todo] `UIRadioGroup` + `UIRadio` — exclusive selection within a group, `onValueChanged`
- [todo] `UISlider` — min / max / value, fill + draggable handle, `onValueChanged`
- [todo] (later) text input field, dropdown, scroll view, progress bar, layout groups
  (horizontal/vertical/grid) + content-size fitters

Event system
- [todo] Per-frame UI hit-test: screen-space against the 2D cursor; world-space against
  the canvas quad via a 3D ray (mouse ray now, **VR controller ray** when VR lands —
  same widget/event path, only the ray source differs).
- [todo] Pointer events: **PointerEnter/Exit (hover)**, **PointerDown**, **PointerUp**,
  **Click**, drag (for sliders). Focus model + **KeyDown/KeyUp** to the focused widget.
- [todo] Keyboard/gamepad navigation (Tab / d-pad move focus, Submit / Cancel) via the
  binding system (2C).
- [todo] Event delivery to gameplay components through the in-game API (2D): `onClick`
  etc. target a component method or fire an event the component subscribes to.

Editor authoring
- [todo] UI edit mode in the viewport: 2D rect editing with anchor/pivot handles, drag to
  place/resize, add widgets via a menu, snap to canvas/guides.
- [todo] Inspector for each widget's properties; wire events (`onClick` -> target object +
  component + method/event).
- [todo] Screen-space canvas previewed at reference resolution; world-space canvas edited
  in 3D like any object.

### 2G. Networking & multiplayer [todo]
Built-in networking so games can be multiplayer, supporting **both** topologies:
**client-server** (an authoritative server, dedicated or player-hosted) and
**peer-to-peer**. One replication/API surface; the topology is a choice per game.

Transport & connection
- [todo] **Transport layer** abstraction over reliable + unreliable channels (candidate:
  `renet`/`renetcode` on UDP, or `quinn` (QUIC); `webrtc`/`matchbox` for browser + P2P).
  Pluggable so server-based and P2P share the same send/recv API.
- [todo] **Connection management**: host / join, lobby, player slots, disconnect + timeout
  handling, and (P2P) host migration.
- [todo] **NAT traversal for P2P**: STUN/TURN/ICE (or a relay fallback) so peers connect
  without port-forwarding; matchmaking/relay service is a later add.

Topologies
- [todo] **Client-server (authoritative)**: server owns the simulation; clients send input,
  receive state. Includes client-side **prediction + reconciliation** and **interpolation**
  so movement is smooth under latency, and server authority to resist cheating.
- [todo] **Peer-to-peer**: shared/host authority or deterministic **lockstep** (input-only
  sync — pairs with a deterministic physics step, see #26). Trust model documented.

Replication
- [todo] **Networked objects + ownership**: mark which objects/components replicate and who
  has authority over each (`Networked` marker / per-object owner). Spawn/despawn replicate.
- [todo] **State sync**: transform + component-field replication with **delta compression**
  and snapshots; tick rate + bandwidth budget. Relevancy/interest management (don't send
  everything to everyone) as a follow-up.
- [todo] **RPCs / networked events**: reliable messages between peers/clients/server,
  delivered to components (ties to the in-game API event bus, 2D).

Integration
- [todo] **In-game API surface** (2D): `is_server` / `is_client` / `local_player`, object
  ownership queries, `spawn_networked`, send/receive RPCs — so components write
  network-aware logic without touching the transport.
- [todo] **Voice chat** (VR-first: spatial voice) + **IK pose replication** for avatars
  (folds in the existing M6 milestone scope).
- [todo] Editor/runtime tooling: a local "host + N clients" test harness, a net-stats
  overlay (ping, bandwidth, packet loss), and lag simulation for testing.

### 2H. Render-to-texture cameras (camera output as a material input) [todo]
Let a `Camera` render the scene from its viewpoint into an offscreen texture that a
material can sample, so a plane + that material becomes an in-world screen — a CCTV
monitor, a TV showing a moveable camera, a mirror, a portal, a minimap, or
picture-in-picture. The camera is an ordinary scene object, so moving it (or
possessing it, 2A) updates the screen live.

Foundation already in place: the renderer's `CameraPreview` (`citrus-render`) is
exactly a render-to-texture pass — an offscreen color+depth target with per-frame
camera UBOs, rendered by the scene pass and exposed to egui as a user texture. This
feature generalizes that single editor-only target into a reusable, material-sampled
resource.

- [todo] **`RenderTexture` GPU resource**: an offscreen target = color image (+ its own
  depth), a sampler, and per-frame-in-flight descriptor wiring. Configurable extent and
  format — `rgba8 srgb` (LDR / UI), `rgba16f` linear (HDR, feeds bloom/tonemap), optional
  mip chain for minified screens. Managed in a pool keyed by a handle; created/resized/
  destroyed as cameras opt in or change size.
- [todo] **Camera output mode**: extend `CameraComponent` with an output target —
  `Display` (default: the main swapchain pass) vs `RenderTexture { handle, width, height,
  format, clear_color, update: EveryFrame | OnDemand | Hz(n) }`. A camera in
  `RenderTexture` mode is excluded from the main display pass.
- [todo] **Material texture source**: today `MaterialTextures` slots are project-relative
  file paths bound into set 1 (`t_albedo`/`t_normal`/`t_orm`/`t_emission`, combined image
  samplers). Make a slot a typed source — `FilePath(path)` | `RenderTarget(camera ref)` —
  serialized in `.material`. At bind time a `RenderTarget` slot points the descriptor at
  the live `RenderTexture` image view instead of a disk-loaded image; an emissive TV uses
  `t_emission`/`t_albedo`. References the source camera by `ObjectId` (survives reload /
  reorder).
- [todo] **Frame graph ordering**: record all active RTT camera passes *before* the main
  pass each frame (offscreen-first), then the main pass samples their results. Insert the
  `COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL` image barrier between an RTT pass
  and any pass that samples it. Build a per-frame dependency order (camera A's target is
  sampled by a material camera B renders → A before B).
- [todo] **Feedback / recursion guard**: a camera filming a screen that shows its own
  feed is a cycle. Bound it — render each `RenderTexture` at most once per frame and let a
  cyclic sampler read *last frame's* result (one-frame latency), or drop the camera's own
  target from its view. No unbounded recursion.
- [todo] **Mirror / portal variant**: a mirror is an RTT camera whose view matrix is the
  main camera reflected across the screen plane, with an oblique near-plane clip at the
  mirror surface; a portal pairs two cameras. Same resource, different view derivation —
  list as a follow-on once the basic RTT path works.
- [todo] **Editor**: Camera inspector picks output (Display / Render Texture + size +
  format); a Render Texture shows in the file browser / material texture slots as a
  droppable source (drag onto a slot, like a file). Drop it on a plane's material and you
  have a working TV whose picture tracks the camera.
- [todo] **Performance & culling**: each active RTT camera is an extra scene pass per
  frame — skip rendering a target no visible material samples (or whose screen is
  off-camera / too small in screen space), throttle via the `Hz`/`OnDemand` update mode,
  cap resolution, and share the shadow map with the main pass. Document the per-target
  cost.

Ties to camera control in the in-game API (2D — set active camera, FOV) and to camera
possession (2A — the moveable camera can be a possessed pawn). HDR targets feed the
existing tonemap/bloom path.

### 2I. Build & bundle (game export) [in progress]
Turn a project into a standalone, runnable game: compile the runtime with the project's
components linked in, collect every asset the game needs, and emit a `build/` folder with
an executable a player can double-click. No editor, no toolchain assumptions on the
target machine.

citrus is a **Rust-native** engine, so its build model matches Bevy, not Unity/Godot:
the components *are* Rust code compiled into the binary, so a build is fundamentally a
`cargo build --release` of a thin runtime binary + an assets folder resolved relative to
the executable. Unity (player exe + `_Data/` with managed DLLs) and Godot (export-template
binary + a `.pck` data pack) ship scripts *as data* because they're interpreted/managed;
citrus does not. An optional packed-archive step (Godot-style `.pck`) is a later add — v1
emits a folder.

**Boot-scene decision:** both a project setting *and* an editable entry file. The
generated `src/main.rs` is real, editable Rust that by default reads `boot_scene` from the
project config and calls `run_game` — beginners never touch it (Godot "Main Scene"
convention), advanced users edit it for custom startup (splash, save-driven scene choice;
Bevy "main.rs is code"). The setting is the default; `main.rs` is the override.

Landed so far:
- [done] **Runtime game loop** — `citrus_engine::run_game(GameConfig, register)` (in
  `citrus-engine/src/runtime.rs`): opens a window, creates the renderer, loads the boot
  scene, fires `start`, then runs `update`/`late_update` + render each frame with
  `FrameInput.egui = None` and `camera_preview = None` (no editor code on the path). Drains
  `ComponentCommand::LoadScene` to switch scenes. Uses the scene's `Camera` component for
  view/proj (fixed fallback if none). `GameConfig::from_project_dir` reads `boot_scene` +
  title from `project.citrus`.
- [done] **Static component linking** — the project's component crate builds as both
  `cdylib` (editor hot-load) and `rlib` (`crate-type = ["cdylib", "rlib"]`); a game binary
  depends on it as a normal crate (editor feature off) and calls `citrus_register`
  directly — no shipped dylib, no `libloading`.
- [done] **`New Project`** (File menu + `citrus --new-project <parent> <name>`) —
  `bundle::scaffold_project` writes a standalone cargo workspace: root `Cargo.toml` (game
  bin + `[workspace.dependencies]` path-pointing at the citrus checkout, found via
  `bundle::citrus_root`), editable `src/main.rs`, `plugins/components` (cdylib+rlib),
  `scenes/ materials/ shaders/ textures/`, a starter scene (camera + lit cube on a plane),
  and `project.citrus` with `boot_scene`. The editor then switches to the new project
  (reloads project file, file browser, plugins, boot scene).
- [done] **Project Settings UI** (File -> Project Settings…) — edits `project.citrus`:
  name + a **Starting scene** picker (boot scene), with a Build Game button. Saves on
  change.
- [done] **Build Game** (File menu + `citrus --build [dir]`) — `bundle::build_game` runs
  `cargo build --release --bin <game>`, then assembles `build/<game>` + `build/assets/`
  (the asset dirs + `project.citrus`, copied so paths resolve exe-relative). Verified
  end-to-end: a scaffolded project builds and the bundled executable runs standalone
  (window + Vulkan + scene render confirmed).
- [done] **Editor-free runtime path proven** — `examples/sample-game` (detached package)
  links `citrus-engine` + a components crate and runs a scene.

- [done] **Editor stripped from a build** — `citrus-engine` now has a default-on `editor`
  cargo feature. The `EngineApp` moved into a gated `editor_app` module; `citrus-editor`,
  `egui`, `egui_dock`, `egui-winit`, `transform-gizmo-egui`, `hecs`, `image`, `serde_json`,
  `libc` and the editor-only modules (gizmo/lsp/undo/camera/icon/crash) are optional, and
  the `plugins.rs` editor/clippy paths are gated. The egui-free **data models**
  (`MaterialModel`, `AlphaModeModel`, `ShaderUiInfo`, `ShaderPropUi`, `ShaderPropKindUi`)
  moved to `citrus-core`, so the always-on scene/shader path no longer touches the editor.
  A game depends on `citrus-engine` with `default-features = false`: verified that
  `citrus-editor`, `egui_dock`, `transform-gizmo`, and `syntect` are **absent** from a
  built game's dependency tree (egui itself stays, shared via the renderer). Editor and
  game both build; the game still runs.

Also landed:
- [done] **Lean release profile** — the scaffold's generated `Cargo.toml` sets
  `[profile.release]` with `lto`, `codegen-units = 1`, `strip`, `panic = "abort"`. A
  sample game shrank 9.6M (default release) → 5.8M with no behaviour change. Editor is
  unaffected (the profile lives in the generated project).

Still to do:
- **egui stays in the render path** (decided) — `citrus-render` always links
  `egui`/`egui-ash-renderer` for its overlay. That adds ~1.5M to a game binary, which is
  acceptable, so it is *not* gated out. Upside: **immediate-mode egui game UI** (2F option
  B) needs no feature flag — it's just a matter of `run_game` handing the per-frame egui
  `Context` to a game callback (the egui render pass is already there). The default game
  links egui but never invokes it (`FrameInput.egui = None`).
- [todo] **Shader precompilation** — shaders compile via `glslc` at runtime today; the
  bundler compiles every material/custom shader to **SPIR-V** ahead of time and ships the
  `.spv`, so the player's machine needs no `shaderc`/`glslc`.
- [todo] **Asset collection** — walk the scenes reachable from the boot scene (and their
  materials → textures → meshes → audio → bake sidecars `.lightmap`/`.lightdata`) and copy
  only what's referenced into `build/assets/`. Path resolution switches from
  project-relative to **exe-relative** at runtime (`GameConfig.assets_root` already drives
  this). Dead-asset stripping; a "copy everything" fallback for the first cut. Skip / warn
  on missing assets rather than aborting.
- [todo] **`GameState` (global runtime state / blackboard)** — a project-defined,
  serializable type the runtime owns **outside any scene**, so it survives scene swaps
  (player health, inventory, score, progression, which level is next). Exposed through the
  in-game API (2D) so components read/write it; lives in the project's component crate.
  This is the persistent layer beneath the scene flow — scenes come and go via
  `ComponentCtx::load_scene` (already implemented), `GameState` does not. Distinguish two
  concerns that are *separate*: (a) this in-memory global state, and (b) **savegame
  persistence** — serialize `GameState` (+ optionally live scene object/component state) to
  a save file under an OS user-data dir and restore it, so a player can quit and resume.
  Save/load is its own in-game API slice.
- [todo] **Remaining build polish** — window icon/size from project config into the
  generated entry; cross-compilation to other targets; an optional packed-archive
  (`.pck`-style) instead of a loose `build/assets/` folder. (Core Build Game action +
  `build/<game>` + `build/assets/` output already land above.)

Depends on the in-game API (2D) for `GameState` access + save/load, and on the
editor/gameplay component split (2E) so editor-only components are excluded from the
build. The scene-flow primitive it relies on (`load_scene`) is already in place.

---

## 3. Dependencies between goals

```
binding system (2C) ---+
                       +--> controllers (2B) --> pawns (2A) --> camera possession
physics engine (#26) --+                              |
                                                      v
                          in-game API (2D) <-- editor/gameplay split (2E)
                              ^        ^
              game UI (2F) ---+        +--- binding system (2C, UI navigation)
```

- **Physics engine (TODO #26)** is a prerequisite for proper Pawn movement (forces,
  jump, gravity) and for the API's physics queries — land it first or stub a kinematic
  fallback.
- **Binding system (2C)** has no hard deps — buildable now; controllers consume it, and
  game UI (2F) uses it for keyboard/gamepad navigation.
- **In-game API (2D)** is the backbone; controllers/pawns are its first real clients,
  so grow the API and the Pawn together rather than big-bang.
- **Editor/gameplay split (2E)** is small and unblocks a correct game-build path; do it
  early so new components declare their kind from the start.
- **Game UI (2F)** depends on the in-game API (2D) for event delivery and the binding
  system (2C) for navigation; its 2D renderer + text subsystem are independent and can
  start now. World-space UI is built/tested with a mouse ray; the VR controller ray
  plugs into the same event path once VR (M4) lands.
- **Networking (2G)** builds on the in-game API (2D) for the replication/RPC surface and
  on the physics engine (#26) for a deterministic step (P2P lockstep). The transport +
  connection layer is independent and can start now; client-server prediction and P2P
  lockstep are largely separate tracks sharing one replication model. Subsumes the M6
  milestone (world/avatar sync, voice, IK).

---

## 4. Other tracked goals (see TODO.md for detail)

- [partial] 3D physics engine (rapier3d) — rigid bodies + gravity step + transform
  writeback done (see Physics/collision); joints, layer matrix, queries, triggers todo
- [done] Phase 5 runtime sampling — 5a flat ambient, 5b per-fragment probe SH, 5c
  per-object lightmaps; all sampled in the standard shader (editor + game), sidecars
  bundled. Pending: visual validation of the GPU bake output.
- [partial] Global illumination in the standard shader — probe SH-L1 sampled per fragment
  (done); baked lightmap sampling for static objects still todo
- [todo] HDR skybox + IBL
- [todo] VR rendering (OpenXR) + VR editing
- [todo] Slang custom-shader frontend (phase 2)
- [todo] Networking, content pipeline, VRM avatars (milestones M3-M7)
- [todo] Occlusion culling, mipmaps, MSAA/TAA, multi-select gizmo, JFA outline
