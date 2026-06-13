# citrus (engine) TODO / working list

Near-term working list, changelog, and detailed design notes for upcoming work.

- Feature status and goals: [Features.md](Features.md) (source of truth for what's done).
- Bugs, verify-after-rebuild items, crash/stability: [BUGS.md](BUGS.md).
- High-level milestones: [README.md](README.md).

Update this whenever work starts, lands, or a design is fleshed out. Don't restate
feature status here — flip it in Features.md.

## Design notes — upcoming / in-progress work

These back the goals tracked in Features.md; this is where the implementation detail
lives.

- **Baked / static lighting** (IN PROGRESS) — GPU bake via **Vulkan ray query**
  (VK_KHR_ray_query on the existing device; RTX/RDNA2+ hardware RT). Full v1 target:
  lightmaps (direct + soft shadows + multi-bounce indirect GI) for static surfaces,
  plus **light-probe volumes** for dynamic objects. Phased build:
  - [x] **Phase 0 — authoring data**: `static_geometry` flag on objects (inspector
        "Static" checkbox, serialized); `LightProbeVolume` component (box `size` +
        `density` /m -> grid of probes; live count readout; box + probe-point gizmo
        when selected); `LightMode` already authored.
  - [x] **Phase 1 — RT device infra**: enable VK_KHR_acceleration_structure +
        ray_query + deferred_host_operations + bufferDeviceAddress when the device
        supports them (graceful fallback disables baking); allocator BDA follows.
        `GpuContext::ray_tracing()` / `accel` loader.
  - [x] **Phase 2 — accel structures**: BLAS per static mesh, TLAS over static
        instances; instance-descriptor SSBO (vertex/index device addresses + albedo/
        emission) for the trace. (`bake.rs`)
  - [x] **Phase 3 — lightmap bake**: rasters a pos+normal gbuffer into uv1 space per
        static object, then a ray-query compute path tracer per texel (direct + shadow
        + cosine-hemisphere multi-bounce indirect + sky on miss). **Needs GPU validation.**
  - [x] **Phase 4 — probe bake**: per `LightProbeVolume`, sphere-traces incoming
        radiance and projects to SH-L1. **Needs GPU validation.**
  - [ ] **Phase 5 — runtime sampling** (NEXT): descriptor set 2 — static objects
        sample their lightmap (uv1), dynamic objects trilinearly interpolate probe SH;
        Baked lights drop out of the realtime loop. Needs: lightmap atlas/array upload
        (resample to a uniform runtime size), probe SSBO, per-draw lightmap layer (free
        `emission.w` push slot), and standard.frag changes.
  - [x] **Phase 6 — editor UX**: dockable **"Baker's Man"** tab (texel density / max
        lightmap / bounces / samples), baked-state readout, **Bake** / **Clear** (Bake
        gated on ray-query support); writes `.lightmap` + `.lightdata` sidecars, loaded
        with the scene. Bake settings modelled on **Bakery** (texels-per-meter density +
        bounces + samples + max size, per-object density multiplier). Remaining: live
        progress feedback (currently blocks the UI). Bakery-style follow-ups (not v1):
        seam dilation, denoise (OIDN/OptiX), directional lightmaps.
  Ties into the skybox/HDR IBL work (environment is another bake input). Until the bake
  runs, Baked lights still render realtime so scenes aren't dark.

- **Global illumination (lightmap-based)** — the bake's lightmap pass traces
  multi-bounce indirect, so lightmaps carry bounced/indirect GI. The **standard shader
  reads the baked lightmap** (uv1, set 2) and adds that indirect irradiance to its
  lighting (replacing flat ambient for static surfaces); dynamic objects get GI from the
  probe SH. Realtime/SSGI-style dynamic GI is a much later follow-up; this item is the
  lightmap GI path (phases 3 + 5).

- **HDR skybox + IBL** — the normal skybox ships (procedural + equirect LDR, per-scene).
  Still to come: load **HDR** equirect (`.hdr`/`.exr`), convert to a cubemap, drive
  image-based lighting (irradiance + prefiltered specular + BRDF LUT). Scene-level
  environment settings (rotation, intensity, tint); the ambient term feeds from the
  skybox once IBL exists.

- **VR editing** — build worlds/avatars from inside the headset. Builds on M4 (VR
  rendering) + M5 (editor): editor panels as quad layers / in-world surfaces with
  laser-pointer interaction (synthetic pointer events from a controller ray), controller
  grab = move/rotate objects directly, thumbstick locomotion + teleport in edit mode,
  snap/grid honored. Desktop editor stays the full-fat authoring surface; VR starts with
  placement/inspection and grows toward full authoring.

- **Custom shaders (Slang frontend)** — phase 2 of the shader system. User-authored
  shaders in **Slang** (compiled to SPIR-V; current GLSL-with-pragmas path is v1).
  Inspector sections/properties **reflected from compiled SPIR-V** (bindings + a
  property-metadata block, Unity ShaderLab-style). Standard shader becomes just another
  registry entry. Also: custom vertex stage, texture-slot properties, spec-constant
  feature toggles, shader graph later. Hot reload currently leaks superseded modules +
  pipeline variants until app exit (small, by design; slangc not installed yet).

- **Plugin system — beyond components** — plugins register components today. Still to
  come: register systems, add menu entries/panels, a stable ABI / wasm boundary instead
  of the same-workspace-dylib assumption. Plugin build currently blocks the UI thread;
  move to a background task with a progress toast.

- **Components phase 2** — components on hecs entities (objects are still a Vec),
  component-driven lights/colliders, multi-component duplicates UX, copy/paste component
  values, Reset per component. Bob/Orbit restore-on-stop relies on the play snapshot;
  keep that invariant when adding new built-ins.

- **Pawns / controllers / binding system / in-game API** — see Features.md sections
  2A–2E for the full task breakdown and dependency graph.

- **Game UI system (runtime UI)** — retained scene-graph UI (Unity uGUI-style),
  screen-space + world-space (VR), visually authored. UICanvas + UIRect + widgets
  (Text/Image/Button/Checkbox/Radio/Slider) as scene objects; 2D batched renderer +
  font subsystem in citrus-render; pointer/key event system delivered to components via
  the in-game API. Full breakdown in Features.md section 2F.

### Smaller backlog
- [ ] Multi-select + multi-object gizmo
- [ ] Stencil/JFA-based outline (upgrade from inverted hull; perfect concave silhouettes)
- [ ] Material texture-slot assignment UI (thumbnails, drag textures from Files panel)
- [ ] Per-section material presets (save/load partial `.material`)
- [ ] Unsaved-changes dialog on exit (save / discard / cancel)
- [ ] Camera-facing axis handles on move/scale gizmo (requires vendoring transform-gizmo)

## Changelog

### Done (2026-06-12 second batch)
- Log console tab (`Tab::Log`): tracing ring (5000), level filters + substring search,
  follow + clear, line wrap with timestamp offset, concrete event timestamps. Plugin
  cargo + glslc errors route in via tracing.
- First-class code editor + LSP: code/text files in dockable `Tab::Code` (multiple,
  close/rearrange/split), syntect highlighting, per-tab dirty + debounced auto-save
  (`.frag` hot-reloads). rust-analyzer on demand: diagnostics, completion (Ctrl+Space),
  hover (Ctrl+hover). Follow-ups: go-to-def, gutter markers, signature help, GLSL server.
- Component system: `TypedComponent` trait + `ComponentRegistry`, Add/remove in
  inspector, serialize into `.scene`, undo (snapshot-diff). Built-ins: Spin, Bob.
- Play mode (Play/Stop): components run while playing; Stop restores transforms +
  component state; play-time motion never lands in undo or saves.
- Custom shaders v1 (GLSL): runtime glslc against an engine preamble, `//! prop` pragmas
  reflected into the Inspector, values in `.material`/`.scene`, ~2s hot reload, error
  swirl + compiler output on failure. Files -> Create -> New Shader.
- Rust component plugins: `plugins/*` workspace crates, cargo-built + dylib-loaded at
  startup and via Tools -> Build & Reload Components, export `citrus_register`. Old libs
  stay mapped; reload re-instantiates from serialized state.
- Selection outline always on top (depth cleared before the outline prepass); corner
  gaps fixed (radial inflate from mesh center, normal fallback in concave regions).
- Material auto-save (0.8s after the gesture settles); Files/Scene right-click on empty
  space + full-width rows; inline rename, cut/copy/paste, drag-to-move.
- Files panel rebuilt as a Unity-style Project view (resizable folder tree + icon grid).
- `project.citrus` (RON): name, last scene (restored on startup), per-project settings;
  saved on scene save/load/new + window close. File menu New Scene / Open Scene.
- Editing a `.material` updates all scene objects using it live. Scene tree rebuilt as a
  real tree (root "Scene" node, reparent drag & drop, context menus).
- App icon (procedural citrus slice; X11 window icon + best-effort desktop entry).
- Scene save materializes materials (each referenced material gets a real `.material`,
  scene references it by path; imported-with-embedded-textures stay inline).
- Camera component (FOV / near / far), auto-attached to every camera object; viewport
  draws each camera's frustum wireframe.

### Done (2026-06-12 editor batch)
- Clickable section headers; no raycast picking on UI/gizmo; move/rotate/scale gizmos.
- Project file browser; `.material` files (RON); unified Inspector; drag & drop
  `.material` onto a slot or mesh; FBX import via ufbx; cursor lock during look.
- Dockable windows (egui_dock); `.scene` save/load; menu bar; FPS/frame-time/redraw
  counter; shader picker; error swirl shader.
- Selection outline (inverted hull, depth-only prepass); scene hierarchy (empties,
  cameras, primitives) with reparent; viewport gizmo overlay (pivot/orientation/snap).
- F to frame; Escape deselects; Alt+drag orbit; relative orbit; stats overlay; VSync
  toggle; scroll-dolly fix; left-drag orbit / click select.

### Done (later editor work)
- Runtime scene loading: `ComponentCtx::load_scene(path)` queues a `ComponentCommand`
  (new, in citrus-editor) drained by the engine after the update pass — switches levels /
  menu -> game during Play, starts the new scene's components + audio, continues playing.
  Stop reloads the pre-play scene (tracked via `play_origin_scene`/`play_scene_switched`;
  unsaved pre-play edits are lost on switch — known v1 limitation). First slice of the
  in-game API (Features.md 2D).
- Lights (directional/point/spot) + shadow-casting lights (shadow-map array, PCF) +
  camera preview tab + skybox (procedural + equirect). **Shadows need bias GPU validation.**
- Undo/redo (move/rotate/scale/rename, material edits + assignments). **By design:
  object deletion is NOT undoable** (keep it that way when delete is implemented).
- Gizmo fat hit areas + orbit priority; scroll dolly only over the viewport; gizmo
  rotation snap (Ctrl, 15deg); orientation cross (local/global).
- Light Probe Volume billboard (3 bulbs at -42/0/42, yellow, reuses the light image).
- Audio components (AudioSource spatial/non-spatial + AudioListener; rodio
  .wav/.flac/.mp3). **Needs a clip + speakers to verify playback.**
- Viewport widget filter (per-billboard visibility + size; gizmos can't be hidden).
- Collision zones (Box/Sphere/Mesh-convex colliders, is_trigger + layer, standalone +
  component, yellow editable widgets). Authoring only; layer matrix lives in physics.
- Scroll dolly + orbit re-arm after mouse-look (winit-based viewport-hover test).
- Scene tree connector lines + Alt-click cascade; Inspector dock width widened + min-width.

## Renderer debt

- [ ] **Occlusion culling** — skip drawing objects hidden behind others. Likely GPU
      two-phase: Hi-Z depth pyramid from last frame + per-object bounds test in compute;
      frustum culling first as the cheap baseline. Stats overlay should report culled counts.
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
