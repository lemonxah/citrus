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

- [ ] **Plugin system (Rust)** — `citrus_plugin::Plugin` trait (init, update, editor_ui,
      events); registration first via static linking (a `plugins/` workspace crate the
      app links), later dynamic loading (`libloading` + stable ABI layer or an IPC/wasm
      boundary — dylib Rust ABI is unstable, needs care). Plugins should be able to:
      register components, register systems, add menu entries/panels.
- [ ] **Component system (Unity-style)** — objects are hecs entities; components are
      Rust structs registered in a component registry (name, default value, inspector
      UI via reflection-lite trait, serde for `.scene` files). Inspector gets an
      "Add Component" button; components can drive behaviour each frame via systems
      (e.g. Spin, light components, colliders later). Transform becomes the first
      built-in component; plugin system registers custom ones.
- [x] Undo/redo (Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y + Edit menu): object move/rotate/
      scale/rename, material property edits (scene + .material files), material
      assignments. Snapshot-diff based with gesture coalescing (one drag = one
      entry). **By design: object deletion will NOT be undoable** (user decision —
      keep it that way when delete is implemented).
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
