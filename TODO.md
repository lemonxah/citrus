# citrus (engine) TODO / working list

Near-term working list, changelog, and detailed design notes for upcoming work.

- Feature status and goals: [Features.md](Features.md) (source of truth for what's done).
- Bugs, verify-after-rebuild items, crash/stability: [BUGS.md](BUGS.md).
- High-level milestones: [README.md](README.md).

Update this whenever work starts, lands, or a design is fleshed out. Don't restate
feature status here — flip it in Features.md.

## Design notes — upcoming / in-progress work

- **Crate-layering refactor** (IN PROGRESS) — the built game must link only
  `citrus-engine` + deps, never the editor. Today `citrus-engine` IS the editor app and
  depends on `citrus-editor`, and the runtime component API lived in `citrus-editor`.
  Target + progress:
  - [x] **`citrus-core`** (new, egui-free) created; `Transform` + `ComponentCtx` +
        `ComponentCommand` moved there. Engine, plugins, and the plugin template import
        them straight from `citrus-core`; editor no longer re-exports them (its
        `pub use` list is trimmed). Build green.
  - [x] Moved `Component`/`TypedComponent` (lifecycle only, no `inspector_ui`) +
        `ComponentRegistry` + all built-in component structs (data + start/update/late)
        into `citrus-core`.
  - [x] Editor `Inspect` trait (`inspector_ui`) + `Gizmo` trait (`draw_gizmo` + `GizmoCtx`)
        + `EditorComponents` name→fn dispatch registry; built-in `Inspect` impls moved
        there; `components_ui` dispatches through it. Plugins export `citrus_register`
        (runtime) + `citrus_register_editor` (editor traits); the loader calls both.
  - [x] Engine/editor/plugins import runtime types from `citrus-core`; `citrus-editor`
        re-exports nothing from core. Build green. (Built-in interactive viewport gizmos —
        collider/probe/audio handles — still drawn in EngineApp; migrate to `Gizmo` impls
        during Stage B.)
  - [ ] **Stage B**: move `EngineApp` out of `citrus-engine` into `citrus-editor` so the
        engine is a pure runtime lib; `citrus` bin launches the editor.

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
  - [ ] **Phase 5 — runtime sampling** (IN PROGRESS):
        - [x] **5a — coarse baked ambient**: `Scene::baked_ambient` averages the probe
              SH L0 into a flat ambient that replaces the env ambient when a bake with
              probes exists; Baked-mode lights drop out of `gather_lights` in that case
              (no double-count). Makes the bake visibly affect the scene; needs GPU eyes.
        - [ ] **5b — per-fragment probe SH**: descriptor set 2 SSBO (probe coeffs +
              volume metadata), trilinear interpolation in standard.frag → spatially
              varying GI for all objects.
        - [ ] **5c — lightmaps**: uv1 vertex plumbing, lightmap texture array (resample
              to a uniform size), per-draw layer (free `emission.w`), sample in
              standard.frag for crisp static GI.
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
  2A–2E for the full task breakdown and dependency graph. Landed so far: deferred
  `ComponentCommand` (load-scene), and an **object-reference world read** —
  `ComponentCtx` carries a per-frame world snapshot (`world_transforms` + `object_names`)
  resolving any object reference to a `Transform` (translation/rotation/scale +
  forward/right/up/matrix). **Stable object identity**: every `SceneObject` has an
  `ObjectId` (v4 UUID via `getrandom`, assigned at create, serialized as a string in
  `.scene`, legacy scenes get one on load); components hold an `ObjectRef` (resolved by
  id each frame via `ctx.resolve`/`transform_of`/`position_of`). The editor renders
  `ObjectRef` as a **drag-drop target** (`InspectCtx::object_ref` via `dnd_drop_zone`):
  drag an object from the Scene tree onto the box, ✕ clears. The tree drags a `usize`
  index (a std type, so the payload crosses the plugin/egui boundary), mapped to id via
  the object list. Scene-tree + viewport selection are release-based (`clicked()`), so
  starting a drag never changes the inspector. `find_object(name)` stays a convenience;
  the Orbit plugin's target is an `ObjectRef` set by drag. **World-space writes**:
  `ctx.set_world_position(world)` converts a world point to local through the owner's
  parent (`ComponentCtx::parent_world`), so a parented object lands at the right world
  spot (fixed Orbit circling a displaced center). Follow-ups: tags, world-space
  rotation/scale writes, and **smoothed/lerped transform moves** (`move_towards`,
  `lerp_position`/`slerp_rotation` with framerate-independent `1 - exp(-k*dt)`
  smoothing, plus a duration+easing tween) so setting a transform can ease instead of
  snap — see Features.md 2D.

- **Networking & multiplayer** — built-in, supporting both client-server (authoritative,
  prediction + reconciliation) and peer-to-peer (shared-authority / deterministic
  lockstep). Transport abstraction (renet/QUIC; webrtc for P2P/browser), NAT traversal,
  networked-object ownership + state replication (delta/snapshots), RPCs, in-game API
  surface (`is_server`/`local_player`/`spawn_networked`), voice + IK replication. Subsumes
  milestone M6. Full breakdown in Features.md section 2G.

- **Render-to-texture cameras** — a `Camera` renders the scene into an offscreen
  `RenderTexture` a material can sample, so a plane becomes an in-world screen (CCTV/TV,
  mirror, portal, minimap). Generalizes the existing editor `CameraPreview` offscreen
  target; adds a camera output mode, a typed material texture source
  (`FilePath` | `RenderTarget(camera id)`), offscreen-first frame-graph ordering with the
  color→shader-read barrier, and a one-frame-latency feedback guard. Full breakdown in
  Features.md section 2H.

- **Build & bundle (game export)** — turn a project into a standalone runnable game.
  **Landed (end-to-end working):** `citrus_engine::run_game(GameConfig, register)` in
  `runtime.rs` — a no-editor game loop (window + renderer + scene load + `start`/`update`/
  `late_update` + render with `egui: None`), draining `LoadScene`, using the scene `Camera`;
  `GameConfig::from_project_dir` reads `boot_scene`/title from `project.citrus`. Component
  crate builds `cdylib`+`rlib` (static link, no shipped dylib). **New Project** (File menu +
  `citrus --new-project <parent> <name>`) — `bundle::scaffold_project` writes a standalone
  cargo workspace (root game `Cargo.toml` with `[workspace.dependencies]` path-pointing at
  the citrus checkout via `bundle::citrus_root`, editable `src/main.rs`, `plugins/components`
  cdylib+rlib, `scenes/materials/shaders/textures`, starter scene, `project.citrus` with
  `boot_scene`), then switches the editor to it. **Project Settings UI** (File -> Project
  Settings…) edits name + boot-scene picker. **Build Game** (File menu + `citrus --build`) —
  `bundle::build_game` runs `cargo build --release` and assembles `build/<game>` +
  `build/assets/`; verified: a scaffolded project builds and the bundled exe runs standalone
  (window/Vulkan/scene render). **Boot scene decision:** `boot_scene` setting drives the
  default, generated `src/main.rs` is editable for override. **Next:** feature-gate
  `citrus-editor` out of `citrus-engine` (so `default-features = false` doesn't link the
  editor — biggest remaining task), shader SPIR-V precompile (no runtime `glslc`), dead-asset
  stripping, `GameState` blackboard + savegame persistence. Rust-native (Bevy single-exe +
  assets) model, not Unity/Godot data-pack. Full breakdown in Features.md 2I.

- **Game UI system (runtime UI)** — **developer's choice per project, both supported:**
  (A) the citrus-native **retained scene-graph UI** (Unity uGUI-style, screen + world-space
  VR, visually authored: UICanvas + UIRect + widgets as scene objects, 2D batched renderer
  + font subsystem in citrus-render, pointer/key events via the in-game API), and (B)
  **immediate-mode egui** (same egui the editor uses, driven from a per-frame game
  callback) for debug HUDs / tools / prototypes. egui stays in the render path in every
  build (~1.5M, acceptable — not gated), so option B needs no feature flag, just a
  `run_game` egui hook. Either or both. Full breakdown in Features.md section 2F.

### Smaller backlog
- [ ] Multi-select + multi-object gizmo
- [ ] Stencil/JFA-based outline (upgrade from inverted hull; perfect concave silhouettes)
- [ ] Material texture-slot assignment UI (thumbnails, drag textures from Files panel)
- [ ] Per-section material presets (save/load partial `.material`)
- [x] Unsaved-changes dialog on exit (save / discard / cancel) — `scene_dirty` set on
  edits/spawns/deletes, cleared on save/load; close is intercepted and a Save & Quit /
  Discard & Quit / Cancel dialog runs (Save&Quit saves the scene, then exits next frame).
- [x] Camera-facing axis handles on move/scale gizmo — no vendoring needed:
  `transform-gizmo` already has `TranslateView` (screen-plane move) + `RotateView`
  (screen-aligned rotate); added both to the gizmo mode sets.

## Changelog

### Done (2026-06-12 second batch)
- Log console tab (`Tab::Log`): tracing ring (5000), level filters + substring search,
  follow + clear, line wrap with timestamp offset, concrete event timestamps. Plugin
  cargo + glslc errors route in via tracing.
- Code editor + LSP: code/text files in dockable `Tab::Code` (multiple,
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
- Code editor: line-number gutter (wrapping-aware, drawn from galley row positions);
  vim mode (toggle + per-file modal state in egui memory, `vim.rs` — core motions/
  operators, see Features.md); fills the dock vertically; muted selection color.
  Vim command line (`:w`/`:q`/`:wq`/`:{n}`/`[%]s/pat/rep/[g]`); `{n}gg`/`{n}G` goto;
  visual `p` paste-over. `:q` routes through new `EditorAction::CloseCodeTab`.
  Substitution uses the `regex` crate (`pat` = regex, `rep` = `$1`/`${name}` refs).
  `u`/Ctrl+R undo/redo: per-file snapshot stack in egui memory, insert session = one
  unit. `gd` go-to-def (reuses LspGoto); `gr` references → `textDocument/references`
  (new `lsp.references`, `LspRequestKind::References`, `parse_references`) shown in a
  picker popup; picking routes through `EditorAction::OpenAndGoto`. Live `:s`/`:%s`
  preview: `vim::preview_substitute` recomputed each frame against a stashed base
  (`VimState::preview_base`), highlighted via `draw_ranges`, reverted on Escape.
  File browser is already live (reads fs each frame; app redraws continuously).
  Code editor: bottom status line (`line_col` helper) showing mode / file / language /
  Ln:Col / unsaved, folding in the vim `:` command line (removed from the header); caret
  forced solid for ~0.6s after vim keystrokes so it stays visible while moving.
  Custom **Citrus Purple** syntect theme (own `syntect` dep + `assets/citrus-purple.tmTheme`,
  `highlight_code` with a thread-local cache): black bg, purple palette, borderless box.
  Vim toggle moved to the **Edit menu** and persisted in `ProjectSettings::vim_mode`
  (passed through `EditorTabs::vim_mode`); header stripped of filename / Save / full path
  (the tab + status line cover it; code auto-saves). Escape keeps editor focus (re-grab
  after vim activity) so it only does Insert->Normal in-editor. Removed the
  Ctrl+Space/Ctrl+hover hint; per-editor status line made visible by capping the scroll
  viewport (`max_height`).
- Global editor status bar (`EngineApp::status_bar`): project + object count, live
  rust-analyzer spinner (`lsp_requests` non-empty), and transient compile/result messages
  (`set_status`); shader reloads + component builds report there. Plugin reload is deferred
  one frame (`reload_pending` + `do_reload_plugins`) so "Compiling components…" paints
  before the blocking cargo build. Global minimum text size (13px floor applied to egui
  text styles in `init`) so small UI text is readable.
- File browser: LSP problem badges — red/yellow dots on files with errors/warnings,
  aggregated onto folders. Engine keeps a project-wide `path -> (errors, warns)` map fed
  by every publishDiagnostics (not just open files).
- File browser: `.rs` files show the Ferris (rustcrab) image instead of the 🦀 glyph —
  official CC0 art (`crates/citrus-editor/assets/ferris.png`, embedded via `include_bytes!`),
  decoded + uploaded as an egui texture once on first use.
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

### Lighting gaps (specular / reflections)
The runtime lighting is **forward rasterization**: per-fragment Cook-Torrance over ≤16 lights +
shadow maps, sampling baked lightmaps / probe SH for indirect (GI is computed off-pass via ray-query
bake or the realtime SDF march). Missing specular/reflection paths:
- [~] **Reflection probes** — prefiltered environment cubemap done: `ReflectionEnv` builds a 6-face
      roughness-mip cube from the skybox (CPU equirect→cube, box-blurred mips), rebuilt on
      `set_skybox`, bound as `samplerCube` (set 0 binding 7) across all pass sites, sampled in
      `standard.frag` spec-env (roughness→mip; SSR refines on hit). Smooth/metallic surfaces reflect
      the environment. Placeable `ReflectionProbe` component (box zone) is in core.
      - [x] Placed ZONE: `ReflectionProbe` component box drives box-projected parallax + per-probe
            intensity in the shader (`FrameInput.reflection_probe` ← `scene.active_reflection_probe`
            picks the covering/nearest probe → `FrameUbo.refl_center/refl_extents` → standard.frag
            reprojects the reflection ray onto the box). Addable via Add Component.
      - [x] Per-zone scene CAPTURE: `Renderer::request_reflection_capture(center)` →
            `do_reflection_capture` renders the scene 6 faces (90° FOV) from the probe center into a
            cube, box-blurs a roughness mip chain (blit), and swaps it in as the reflection env. The
            engine requests it on scene load at the first `ReflectionProbe` zone. Reuses the frame's
            lights/shadows + capture set-0. (Face orientation + prefilter want on-device tuning.)
      - [ ] Blend multiple probes; editor box gizmo + size/intensity inspector UI + re-bake button.
- [x] **Screen-space reflections (SSR)** — cheap forward-integrated march in `standard.frag`:
      reflects last frame's colour against the full-res Flux depth prepass, view-space ray with
      binary refine, gated to smooth/metallic pixels (roughness cutoff), screen-edge fade. Tunable
      from Environment → Flux GI → Reflections (SSR). Rides on the Flux depth prepass, so it is
      active only while realtime GI is on. Set-0 bindings 5 (scene depth) + 6 (last-frame colour);
      colour history blitted per frame (swapchain + editor viewport paths).
- [x] **HDR rendering + bloom** — the game swapchain path renders the scene into a linear
      `R16G16B16A16_SFLOAT` target (`post::PostPass`, per-frame-in-flight), then a fullscreen post
      pass (`fullscreen.vert`+`post.frag`) applies bloom + chromatic aberration + exposure + grading
      + tonemap + vignette → swapchain. Surface shaders skip inline tonemap when HDR (`debug.w` /
      skybox `params0.w`). SSR history is now HDR. All `PostFx` fields (incl. bloom/CA) are live.
      Editor viewport keeps inline tonemap for now (no bloom there yet — follow-up).
- [ ] **Ray-traced reflections** — reuse the existing Vulkan ray-query path (already used for the
      GI bake) for runtime reflections on capable GPUs.
- [ ] (related) Specular occlusion + a proper split-sum BRDF LUT for the IBL term.

- [~] **Skybox** — equirect 360 (existing) + 6-texture cubemap done: `WorldEnvironment.skybox_faces`
      (6 paths, +X,-X,+Y,-Y,+Z,-Z) → `Renderer::set_skybox_cube` builds the env cube (sharp mip 0) →
      `skybox.frag` samples it via `samplerCube` (binding 7) when `params0.z` set; reflections reuse
      the same cube's blurred mips. Remaining: single pre-packed cubemap image (cross/strip) parse +
      an editor UI to assign the 6 faces (scene field works; set via `.scene` for now).
- [~] **Fog** — exponential distance + height fog done: `WorldEnvironment.fog_*` → `FrameInput.fog`
      → `FrameUbo.fog_color/fog_params` → `standard.frag` blends toward fog colour by
      `1 - exp(-density * (dist - start) * exp(-falloff * height))` pre-tonemap. Editor UI under
      Environment → Fog. (True volumetric light-shaft scattering is a later upgrade.)
- [ ] **Occlusion culling** — skip drawing objects hidden behind others. Likely GPU
      two-phase: Hi-Z depth pyramid from last frame + per-object bounds test in compute;
      frustum culling first as the cheap baseline. Stats overlay should report culled counts.
- [ ] Mipmap generation (textures currently render mip 0 only)
- [ ] MSAA / TAA decision for the editor viewport
- [ ] 16-bit / float glTF image formats (currently unsupported)
- [ ] Removing meshes/textures leaks GPU memory until scene reset (slot reuse / GC)
- [x] Inverse-transpose normal matrix for non-uniform scale — `standard.vert` now uses
      `transpose(inverse(mat3(model)))` for normals (tangents keep the plain basis), so
      non-uniformly-scaled objects light correctly.
- [ ] Async pipeline-variant compilation (avoid first-use hitch; spec'd in docs/shaders.md)

## Milestones (see README)

- [~] M3 — VRM avatars: skinning, blendshapes, spring bones; mipmaps
      - [x] Rigging IMPORT: `Vertex` joints+weights (glTF read_joints/weights, FBX ufbx skin
            deformers); glTF full armature (`Skeleton`: joint hierarchy + inverse-bind + rest pose)
            + animation clips (`AnimationClip` per-joint TRS channels). Runtime sampler
            `AnimationClip::sample` → `Skeleton::palette` (linear-blend skinning matrices).
      - [x] Skinning PLAYBACK (CPU): skinned meshes upload to a host-visible buffer
            (`upload_mesh_skinned`); `LoadedScene::update_skinning` samples the clip → joint palette
            (`Skeleton::palette`) → CPU-skins (`skin_position`/`skin_direction`) → `update_mesh_vertices`
            every frame in both the game runtime and the editor. Rigged glTF characters animate
            on-screen (first clip, looped).
      - [x] FBX skeleton hierarchy from skin clusters (`build_skeleton`): joints in cluster order
            (matches vertex indices), `geometry_to_bone` = inverse-bind, parents from the bone nodes'
            FBX parent chain, rest TRS from `local_transform`. FBX rigs now import an armature
            (posable by VR IK).
      - [x] FBX anim-stack → `AnimationClip` (`build_rig`): every anim stack sampled at 30 fps over its
            time range via `Node::evaluate_transform` into dense per-joint TRS channels. FBX rigs now
            animate like glTF.
      - [ ] Move to GPU skinning (joint-palette SSBO + SKINNED vertex variant) for perf; per-object
            Animator with clip selection + blending; double-buffer the skinned vertex buffer.
- [~] M4 — VR: OpenXR session, stereo multiview rendering, controller input
      - [x] OpenXR START (`citrus-xr`): `XrContext::start` loads the loader at runtime, creates an
            instance (XR_KHR_vulkan_enable2), selects the HMD system + tracking caps. Wired into both
            the game runtime (`GameApp.xr`) and the editor (`EngineApp.xr`); degrades to flat with no
            runtime/headset.
      - [x] Full-body IK solver (`citrus_core::ik`): analytic two-bone (limbs) + FABRIK (spine),
            `TrackerTargets` for head/hands/hips/feet. Unit-tested. Maps tracker poses → skeleton.
      - [x] OpenXR SESSION created sharing citrus-render's VkDevice (`XrContext::create_session` via
            `Renderer::vulkan_raw_handles`), lifecycle pumped each frame (`XrSession::poll`), head pose
            read (`head_pose` → stage/view reference spaces) → `TrackerTargets` (game + editor).
      - [ ] Per-eye swapchains + stereo render into the XR swapchains (session exists; rendering not
            yet — needs the renderer to render into XR-provided images).
      - [x] Controller + SteamVR/Vive tracker poses via OpenXR action sets: a "fullbody" action set
            with pose actions for hands (simple-controller `aim/pose`) + waist/feet (Vive tracker
            `role/{waist,left_foot,right_foot}` paths, htcx extension enabled when present), synced +
            located each frame → `XrSession::body_poses` → fills `TrackerTargets` (head/hands/hips/feet)
            in game + editor.
      - [x] Apply the IK solve to a humanoid avatar: `humanoid::HumanoidRig::map` matches bones by
            name (Mixamo/VRM/Unity), `pose_from_trackers` drives hips/head + two-bone arms/legs from
            `TrackerTargets` → joint local transforms → palette. Wired into `update_skinning`, game +
            editor. Retarget math is a first cut; per-rig roll/offset tuning needs a real avatar.
      - [x] IK generalized beyond VR: `citrus_core::IkTargets` alias + `LoadedScene::set_ik_targets`
            so any source (gameplay/procedural/network, not just VR) poses any humanoid avatar;
            `update_skinning` applies IK whenever any limb target is set + the rig is humanoid.
      - [ ] Per-eye swapchains + stereo render into XR images so it shows IN the headset (headset-
            gated; the avatar is tracker-driven + rendered on the desktop view now).
- [ ] M5 — Full editor: world building tools, avatar setup tooling
- [ ] M6 — Networking: world/avatar sync, voice, IK replication
- [ ] M7 — Content pipeline: publishing format, mobile-tier validation, sandboxing
- [ ] Standard shader phase 2: ramps, rim, matcaps, outlines, UV panning, detail maps
- [ ] Standard shader phase 3: audio-reactive, dissolve, glitter, iridescence, flipbooks
- [ ] Mobile shader tier (`TIER_MOBILE`) + budget meter in the inspector
