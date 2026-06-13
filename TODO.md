# citrus (engine) TODO / backlog

Living document — update whenever a feature is added, finished, or discovered.
(Bigger-picture milestones live in [README.md](README.md); this is the working list.)

## Bugs

- [ ] **Verify**: gizmo floated away from the selected object's pivot — fixed by
      projecting the gizmo against the full window rect (the swapchain) instead of
      the viewport tab rect; confirm anchored correctly after the rebuild
- [ ] **Verify**: right click in viewport sometimes did nothing instead of starting
      camera look — was gated on `egui_ctx.is_using_pointer()` at press time, which
      could be true spuriously; now the look starts from the viewport tab's own
      widget (`button_pressed(Secondary) && hovered`), so egui's hit-testing decides

## Done (2026-06-12 second batch)

- [x] **Log console tab** — dockable `Tab::Log` (next to Files) mirroring every
      `tracing` event into a 5000-entry ring (custom subscriber layer). Filter by
      level (Error/Warn/Info/Debug/Trace toggles) + substring search; Follow
      (stick-to-bottom) + Clear. Plugin `cargo build` errors/warnings and `glslc`
      shader compile errors route in via `tracing` (multi-line output flattened to
      indented rows). Reachable from Windows menu.
- [x] **First-class code editor + LSP** — code/text files open in dockable
      `Tab::Code` tabs (multiple at once, close/rearrange/split), not the
      Inspector; full **syntect** syntax highlighting (Rust, GLSL, …); per-tab
      dirty + debounced auto-save (saving `.frag` still hot-reloads shaders).
      **rust-analyzer** spawned on demand (JSON-RPC over stdio on a reader
      thread) surfaces live diagnostics, **completion** (Ctrl+Space popup with
      insert), and **hover** (Ctrl+hover tooltip) on `.rs` files. Follow-ups:
      go-to-def, inline gutter markers, signature help, a shader (GLSL) server.
- [x] **Component system (Unity-style)** — `TypedComponent` trait (serde +
      Default + inspector UI + per-frame `update`) with a `ComponentRegistry`;
      Inspector shows components with ➕ Add Component / ✕ remove; components
      serialize into `.scene` files and participate in undo (snapshot-diff).
      Built-ins: Spin, Bob.
- [x] **Play mode** — ▶ Play / ⏹ Stop in the menu bar: components run while
      playing; Stop restores transforms + component state from the play
      snapshot; play-time motion never lands in undo or saves.
- [x] **Custom shaders v1 (GLSL)** — user `.frag` files compiled at runtime
      via `glslc` against an engine preamble (frame UBO, 4 texture slots,
      varyings, 16 push-constant floats). Properties declared as
      `//! prop name float|toggle|color|color3 range(…) default(…)` pragmas
      and reflected into the Inspector; values stored by name in `.material`
      and `.scene` files. Hot reload on file change (~2s poll); failed
      compiles render the error swirl + show compiler output in the
      Inspector. Files → Create → New Shader writes a starter. (Slang still
      a candidate frontend later; see below.)
- [x] **Rust component plugins** — crates under `plugins/` (workspace members
      so dependency versions match the editor exactly) are cargo-built and
      dylib-loaded by the editor: at startup and via Tools → Build & Reload
      Components. Plugins export `citrus_register(&mut ComponentRegistry)`.
      Old libraries stay mapped forever (instances may reference them);
      reload re-instantiates scene components from serialized state. Build
      errors show in a window. Starter crate: `plugins/components` (Orbit).
- [x] Selection outline always on top: depth buffer cleared before the
      outline prepass, so only the selected object can mask its own outline
- [x] Outline corner gaps fixed: hull inflates radially from the mesh center
      (pure function of position → duplicated hard-edge vertices stay
      welded), normal fallback in concave regions
- [x] Material auto-save: edits persist 0.8s after the gesture settles —
      file-backed materials to their `.material`, others to a new
      `materials/<name>.material` (texture paths in existing files preserved)
- [x] Files/Scene panels: right-click works on empty space and full-width
      rows; Scene tree context menu adds objects (Empty/Camera/primitives)
- [x] File explorer: inline Rename, Cut/Copy/Paste (recursive folder copy),
      drag a file onto a folder (or empty space = root) to move; selection
      follows renames
- [x] Files panel is now a Unity-style Project view: resizable folder tree
      on the left (full-width rows, collapse arrows, click = show contents),
      icon grid on the right (big type icons, names wrapped to 2 lines and
      cropped with …, vertical scroll only). Double-click folders to enter,
      ⬆ + breadcrumb at the top; all rename/clipboard/drag-move/context-menu
      ops work on tiles and tree rows.
- [x] `project.citrus` (RON, created on first run): project name (window
      title), last-opened scene (restored on startup; broken files fall back
      to the test scene), per-project engine settings (vsync, stats toggles,
      snap + grid size). Saved on scene save/load/new and on window close.
- [x] File menu: New Scene (empty scene) and Open Scene (submenu listing all
      project .scene files) alongside Save / Save As
- [x] Editing a `.material` file updates all scene objects using it live
      (not just on save); rename pre-selects the filename stem so the
      extension survives typing
- [x] Scene panel rebuilt as a real tree: root "Scene" node (drop = unparent,
      right-click = Add Object), left-aligned full-width rows with collapse
      arrows per parent, hover highlight; reparent drag & drop and context
      menus preserved.
- [x] App icon: procedural citrus-slice icon; winit window icon (X11) +
      `app_id` "citrus" and a best-effort `~/.local/share` desktop-entry +
      icon install for Wayland compositors
- [x] Scene save materializes materials: every material referenced by the
      scene gets a real `.material` file (created in materials/ if missing,
      refreshed otherwise) and the scene references it by path. Exception:
      imported materials with embedded textures stay inline until a texture
      export pipeline exists (.material can't carry embedded textures).
- [x] Camera component (FOV / near / far), auto-attached to every camera
      object; viewport always draws each camera's frustum wireframe
      (near/far rects, edges, up-marker; purple when selected) so the
      orientation and framing are visible. Post-process settings join the
      component once a post pipeline exists.

## Done (2026-06-12 editor batch)

- [x] Clickable section headers (collapse/expand by clicking the title, not just the arrow)
- [x] No raycast picking on UI resize handles / gizmo (picking goes through the
      viewport tab's own egui widget now; UI always wins)
- [x] Transform gizmos: move/rotate/scale on selected object (G/R/S + Tools menu)
- [x] Project file browser panel (all project files; click = inspect,
      double-click model = import, drag = assign)
- [x] `.material` files (RON): save/load, assign to objects, per-path texture cache
- [x] Unified Inspector: object (name/transform/mesh/material slot + editor),
      `.material` editor with Save, `.scene` loader, generic file info
- [x] Drag & drop `.material` onto a material slot or onto a mesh in the viewport
- [x] FBX import via ufbx (per-material mesh parts, PBR factors + base/normal/emission
      textures, embedded textures supported)
- [x] Cursor hidden + locked during right-mouse look (raw mouse deltas)
- [x] Dockable windows (egui_dock): Scene / Inspector / Files around transparent Viewport
- [x] `.scene` files: save (Ctrl+S / File menu) and load (click in Files → Load Scene)
- [x] Menu bar: File / Edit / Tools / View / Help (keep extending!)
- [x] FPS + frame-time + redraw counter (menu bar corner, View toggle)
- [x] Shader picker on materials (registry; unknown shader = error swirl)
- [x] Files panel ➕ Create / right-click → New Material / Scene / Folder
- [x] Error shader: animated pink/purple swirl (broken/missing/unknown-shader materials)
- [x] Selection = bright purple inverted-hull outline (surface stays fully visible);
      static width (no pulse), depth-only prepass so transparent objects show a
      true outline instead of filling solid purple
- [x] Scene hierarchy: empty objects (grouping), cameras, primitives
      (cube/sphere/capsule/plane) via the Object menu; nested Scene tree with
      drag-to-reparent (drop on row = parent, drop on empty space = unparent),
      world-transform-preserving reparent, gizmo works in world space for children,
      .scene files store parent links
- [x] Viewport overlay: gizmo tool buttons (top-left), pivot Pivot/Center +
      Global/Local orientation dropdowns, snap toggle + grid size (top-center;
      Ctrl also snaps while dragging)
- [x] F — focus/frame the selected object
- [x] Escape deselects (quit moved to File menu)
- [x] Alt+left drag with no selection orbits around the viewport-center object/point
- [x] Orbit stays relative: camera rotates with the orbit instead of snapping the
      pivot to the screen center
- [x] Stats overlay (viewport bottom-left): frame time, draw calls split into
      opaque / +transparent / +outline / +error, materials drawn, pipeline binds,
      cached shader variants. Reflections/probes/shadows report here when those
      passes exist. Separate View toggles for menu-bar FPS vs the overlay;
      large white text for readability.
- [x] VSync toggle (View menu): FIFO ↔ MAILBOX/IMMEDIATE for uncapped frame rates
- [x] Fix: scroll dolly dead on fresh start — the dock consumed wheel events at the
      winit level; scroll now reads through the viewport tab (egui input), with the
      winit path kept only during mouse-look (cursor locked, egui bypassed)
- [x] Left drag = orbit (no modifier needed); click-without-drag = select;
      Alt forces orbit over gizmo handles

## Next up (designed, not started)

- [x] **Lightmap UVs (second UV set)** — `Vertex` carries `uv1` (lightmap coords). FBX uses
      its 2nd UV set when present, else generates a per-triangle grid atlas (non-overlapping,
      seam-heavy — a real chart unwrap via **xatlas** is the quality follow-up). glTF uses
      `TEXCOORD_1` or falls back to the primary set. Built-in primitives reuse their clean
      primary UVs. Stored on the mesh ready for the bake; not yet sampled by any shader.

- [ ] **Baked / static lighting** (IN PROGRESS) — GPU bake via **Vulkan ray query**
      (VK_KHR_ray_query on the existing device; RTX/RDNA2+ hardware RT). Full v1 target:
      lightmaps (direct + soft shadows + multi-bounce indirect GI) for static surfaces,
      plus **light-probe volumes** for dynamic objects. Phased build:
      - [x] **Phase 0 — authoring data**: `static_geometry` flag on objects (inspector
            "Static" checkbox, serialized); new `LightProbeVolume` component (box `size`
            + `density` /m → grid of probes; live count readout; box + probe-point gizmo
            when selected); `LightMode` already authored.
      - [x] **Phase 1 — RT device infra**: enable VK_KHR_acceleration_structure +
            ray_query + deferred_host_operations + bufferDeviceAddress when the device
            supports them (graceful fallback disables baking); allocator BDA follows.
            `GpuContext::ray_tracing()` / `accel` loader.
      - [ ] **Phase 2 — accel structures**: BLAS per static mesh, TLAS over static
            instances; geometry/material SSBOs for the trace (mesh buffers need AS-build
            + device-address usage flags).
      - [ ] **Phase 3 — lightmap bake**: raster a pos+normal gbuffer into uv1 space per
            static object, then a ray-query compute pass per texel (direct + soft shadow
            from Baked/Mixed lights + hemisphere-sampled indirect); HDR lightmap → disk.
      - [ ] **Phase 4 — probe bake**: per `LightProbeVolume`, trace the sphere at each
            grid probe → SH-L1 irradiance, stored per scene.
      - [ ] **Phase 5 — runtime sampling**: set 2 — static objects sample the lightmap
            (uv1), dynamic objects trilinearly interpolate probe SH; Baked lights drop
            out of the realtime loop (Mixed keeps direct).
      - [ ] **Phase 6 — editor UX**: "Bake Lighting" action + progress.
      Ties into the skybox/HDR IBL work (environment is another bake input). Until the
      bake runs, Baked lights still render realtime so scenes aren't dark.

- [x] **Lights (directional / point / spot)** — `LightComponent` (Type, Mode, Color,
      Intensity, Range, Spot Angle/Blend) attached to `ObjectSource::Light` objects;
      Scene tree → Light submenu spawns each kind; gizmo-movable like any object. The
      renderer evaluates up to `MAX_LIGHTS` (16) per frame with distance attenuation and
      spot cones (multi-light frag loop). Light/camera billboards + selectable in the
      viewport; widgets hidden for disabled objects.
- [x] **Shadow-casting lights** — `LightComponent` Cast Shadows + bias; shadow-map array
      (8×1024 D32, `sampler2DArrayShadow`), depth pass from each caster's POV (directional
      = camera-following ortho, spot = perspective cone, point = 6 cube faces), light
      view-projs in the frame UBO, PCF sampling in standard.frag. Shared by the main view
      and the Camera preview. **Needs GPU validation** (acne/bias tuning, cascades, and a
      shadow-caster budget UI are follow-ups).
- [x] **Camera preview tab** — Game-view-style tab rendering the scene from the "main"
      camera (smallest stable camera id, saved in `.scene`) to an offscreen target shown
      as an egui user texture. View menu → Camera preview.
- [x] **Skybox (procedural + equirect)** — fullscreen skybox pass behind the scene:
      procedural gradient by default, or an equirectangular image (Files → right-click
      → Set as Skybox), saved per-scene. HDR + IBL still TODO (see below).

- [ ] **HDR skybox + IBL** — the normal skybox ships (procedural + equirect LDR image,
      per-scene). Still to come: load **HDR** equirect (`.hdr`/`.exr`), convert to a
      cubemap, and drive image-based lighting (irradiance + prefiltered specular + BRDF
      LUT) so materials pick up ambient/specular from the environment. Scene-level
      environment settings (rotation, intensity, tint); the ambient term feeds from the
      skybox once IBL exists. Pairs with the light components for full environment
      lighting.

- [ ] **VR editing** — build worlds and avatars from inside the headset, not just at
      the desk. Builds on M4 (VR rendering) + M5 (editor): editor panels rendered as
      quad layers / in-world surfaces with laser-pointer interaction (egui can be fed
      synthetic pointer events from a controller ray), controller grab = move/rotate
      objects directly (gizmo-free manipulation), thumbstick locomotion + teleport in
      edit mode, snap/grid honored when placing. Desktop editor stays the full-fat
      authoring surface; VR mode starts with placement/inspection and grows toward
      full authoring.

- [ ] **Custom shaders** — user-authored shaders in a well-documented language
      (leading candidate: **Slang**, compiled to SPIR-V; GLSL-with-includes as the
      fallback option). Each `.material` picks its shader (`shader:` field already in
      the format, default "standard"). Inspector sections/properties get **reflected
      from the compiled shader** (SPIR-V reflection for bindings + a property-metadata
      block for sections, ranges, defaults — Unity ShaderLab-style). The standard
      shader becomes just another entry in the shader registry. Failed compiles render
      with the error swirl shader.

- [ ] **Plugin system — beyond components** — plugins can register components today
      (see Done: Rust component plugins). Still to come: register systems, add menu
      entries/panels, and a stable ABI / wasm boundary instead of the
      same-workspace-dylib assumption. Plugin build currently blocks the UI thread;
      move to a background task with a progress toast.
- [ ] **Components phase 2** — components on hecs entities (objects are still a Vec),
      component-driven lights/colliders, multi-component duplicates UX, copy/paste
      component values, Reset per component. Bob/Orbit restore-on-stop relies on the
      play snapshot; keep that invariant when adding new built-ins.
- [ ] **Custom shaders phase 2** — Slang frontend (compiled to SPIR-V), custom vertex
      stage, texture-slot properties, spec-constant feature toggles in user shaders,
      shader graph later. Hot reload currently leaks superseded shader modules +
      pipeline variants until app exit (small, by design).
- [x] Undo/redo (Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y + Edit menu): object move/rotate/
      scale/rename, material property edits (scene + .material files), material
      assignments. Snapshot-diff based with gesture coalescing (one drag = one
      entry). **By design: object deletion will NOT be undoable** (user decision —
      keep it that way when delete is implemented).
- [ ] Gizmo rotation snap: holding Ctrl locks rotation to 15° steps (easy
      90° turns and anything between); translation already grid-snaps
- [ ] **Audio components** — `AudioSource` (clip, play-on-start, loop, volume,
      pitch) with a **Spatial** toggle: spatial = 3D settings (min/max
      distance, rolloff linear/log/custom, doppler, spread/cone); non-spatial
      = constant-volume 2D ignoring listener position. Needs an `AudioListener`
      (default on the main camera) and an audio backend (kira/rodio); spatial
      pan + attenuation from listener↔source transforms each frame.
- [ ] Multi-select + multi-object gizmo
- [ ] Stencil/JFA-based outline (upgrade from inverted hull; perfect concave silhouettes)
- [ ] Material texture-slot assignment UI (thumbnails, drag textures from Files panel)
- [ ] Per-section material presets (save/load partial `.material`)

## Renderer debt

- [ ] **Occlusion culling** — skip drawing objects hidden behind others. Likely GPU
      two-phase: depth pyramid (Hi-Z) from last frame's depth + per-object bounds
      test in a compute pass; frustum culling first as the cheap baseline win.
      Stats overlay should report culled-object counts.

- [ ] Mipmap generation (textures currently render mip 0 only)
- [ ] MSAA / TAA decision for the editor viewport
- [ ] 16-bit / float glTF image formats (currently unsupported)
- [ ] Removing meshes/textures leaks GPU memory until scene reset (slot reuse / GC)
- [ ] Inverse-transpose normal matrix for non-uniform scale (shader TODO)
- [ ] Async pipeline-variant compilation (avoid first-use hitch; spec'd in docs/shaders.md)

## Milestones (see README)

- [ ] M3 — VRM avatars: skinning, blendshapes, spring bones; mipmaps
- [ ] M4 — VR: OpenXR session, stereo multiview rendering, controller input
- [ ] M5 — Full editor: world building tools, avatar setup tooling
- [ ] M6 — Networking: world/avatar sync, voice, IK replication
- [ ] M7 — Content pipeline: publishing format, mobile-tier validation, sandboxing
- [ ] Standard shader phase 2: ramps, rim, matcaps, outlines, UV panning, detail maps
- [ ] Standard shader phase 3: audio-reactive, dissolve, glitter, iridescence, flipbooks
- [ ] Mobile shader tier (`TIER_MOBILE`) + budget meter in the inspector
