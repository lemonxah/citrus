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
- [done] PBR standard shader (metal/rough, base/normal/emission), multi-light frag loop
- [done] Lights: directional / point / spot — color, intensity, range, spot angle/blend
  (up to 16/frame, distance attenuation + spot cones)
- [partial] Shadow-casting lights (shadow-map array, PCF; needs acne/bias GPU validation)
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
  shadow + multi-bounce indirect), light-probe SH-L1 bake. Built, needs GPU
  validation; runtime sampling (Phase 5) not done — bake isn't visible yet.
- [done] Baker's Man dock tab (texel density / bounces / samples / max size, Bake/Clear)

### Editor
- [done] Dockable panels (egui_dock): Scene / Inspector / Files / Log / Code / Baker
  around a transparent viewport
- [done] Transform gizmos: move (W) / rotate (R) / scale (E), pivot/center + global/local,
  snap (grid + 15deg rotation), fat hit areas with orbit priority, hover emphasis
- [done] Camera-relative axis flipping + orientation cross on move/scale gizmos
- [done] Scene tree: nested hierarchy, drag-to-reparent, connector lines, Alt-click cascade
- [done] Project file browser (Unity-style folder tree + icon grid, rename/cut/copy/paste/
  drag-move, context menus)
- [done] Unified Inspector (object transform/mesh/material slots, `.material` editor,
  component list with Add/Remove)
- [done] Orbit / pan / scroll-dolly editor camera, F to frame, Escape to deselect
- [done] Play mode (Play/Stop): components run, transforms + state restored on Stop
- [done] Undo/redo (move/rotate/scale/rename, material edits + assignments; delete is
  intentionally non-undoable)
- [done] Menu bar (File/Edit/Tools/View/Help), New/Open/Save scene
- [done] Viewport widget filter (per-billboard visibility + size)
- [done] Billboard widgets: lights, cameras (frustum), probe volumes (3 bulbs), audio (speaker)
- [done] Log console tab (tracing ring buffer, level filter + search, follow, wrap, timestamps)
- [done] Code editor tabs + syntect highlighting + debounced auto-save
- [partial] rust-analyzer LSP (diagnostics, completion, hover on `.rs`)
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
  standalone or as components, yellow editable viewport widgets — authoring only

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
  - [todo] **Self transform**: world + local read/write (have local; add world-space)
  - [todo] **Object graph**: find by name/id/tag, parent/children, spawn/despawn, set active
  - [todo] **Transforms of other objects**: get/set translation/rotation/scale
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
A retained, scene-graph UI for in-game menus / inventory / HUD — Unity uGUI-style.
Widgets are scene objects carrying UI components; the tree lives under a UICanvas and
serializes into `.scene` like any other objects. Distinct from the editor's egui UI.
Decisions: **retained scene-graph** (not immediate-mode), **screen-space + world-space**
(world-space for VR controller-ray interaction), **visually authored** in the editor.

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

---

## 4. Other tracked goals (see TODO.md for detail)

- [todo] 3D physics engine (Rapier3d: rigid bodies, materials, joints, layer matrix, queries)
- [todo] Phase 5 runtime sampling — make the bake actually light the scene
- [todo] Global illumination in the standard shader (sample baked lightmap / probe SH)
- [todo] HDR skybox + IBL
- [todo] VR rendering (OpenXR) + VR editing
- [todo] Slang custom-shader frontend (phase 2)
- [todo] Networking, content pipeline, VRM avatars (milestones M3-M7)
- [todo] Occlusion culling, mipmaps, MSAA/TAA, multi-select gizmo, JFA outline
