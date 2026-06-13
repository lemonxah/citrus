# citrus

A Rust + Vulkan game engine with a dockable in-engine editor, built VR-first
(OpenXR). Powers [vrsh](../vrsh), a social-VR platform — but citrus itself is
general-purpose: scenes, materials, FBX/glTF import, components and Rust
plugins, custom shaders, lighting, audio, colliders, and a growing editor.

## Project docs

- **[Features.md](Features.md)** — the feature map: everything implemented vs
  planned, with status markers. Start here.
- **[TODO.md](TODO.md)** — working list, changelog, and design notes for
  upcoming work.
- **[BUGS.md](BUGS.md)** — open bugs, verify-after-rebuild items, crash/stability.
- **[docs/shaders.md](docs/shaders.md)** — the citrus standard shader.

## Stack

| Concern | Choice |
|---|---|
| Graphics | Vulkan 1.3 via [ash](https://crates.io/crates/ash) (dynamic rendering, sync2) |
| Lighting bake | Vulkan ray query (VK_KHR_ray_query / acceleration_structure) |
| VR | OpenXR via the [openxr](https://crates.io/crates/openxr) crate (Monado / SteamVR runtimes) |
| Editor GUI | [egui](https://crates.io/crates/egui) + egui_dock, rendered in-engine |
| Worlds/props | glTF 2.0, FBX (ufbx) |
| Avatars | VRM (glTF extension): humanoid rig, expressions, spring bones (planned) |
| Audio | [rodio](https://crates.io/crates/rodio) (.wav/.flac/.mp3) |
| Default shader | citrus standard uber-shader, Poiyomi-inspired — see [docs/shaders.md](docs/shaders.md) |
| Windowing | winit |

## Workspace layout

```
crates/
  citrus          binary entry point (desktop now, +VR at M4)
  citrus-engine   app loop, scene, timing, input, editor host
  citrus-render   ash Vulkan renderer (context, swapchain, frames, bake)
  citrus-xr       OpenXR session/swapchains/input          (stub)
  citrus-assets   glTF/FBX, .material/.scene/project files, bake sidecars
  citrus-editor   egui editor panels (inspector, files, code editor, components)
plugins/
  components      example Rust component plugin (Orbit)
```

## Features

For the full itemized status see **[Features.md](Features.md)**. Highlights of
what's already in:

**Rendering** — PBR standard shader (metal/rough, normal/emission); directional
/ point / spot lights with shadow maps; procedural + equirect skybox; always-on-top
selection outline; camera preview tab; stats overlay; vsync toggle.

**Assets & formats** — glTF and FBX import; built-in primitives; `.material`,
`.scene`, and `project.citrus` files; `.wav`/`.flac`/`.mp3` audio.

**Editor** — dockable panels (egui_dock); transform gizmos (move **W** / rotate
**R** / scale **E**) with snapping, pivot/orientation modes, camera-relative
handles; scene tree with reparenting; Unity-style project file browser (with LSP
problem badges, live FS updates); unified inspector; play mode; undo/redo;
viewport widget filter and billboards; log console.

**Code editor** — dockable tabs with syntect highlighting, line numbers,
debounced auto-save; rust-analyzer LSP (diagnostics, completion, hover,
go-to-definition, references); a **vim mode** (motions, operators, visual,
`:` command line with live regex `:s`/`:%s` preview, undo/redo, `gd`/`gr`).

**Components & scripting** — `TypedComponent` system with built-ins
(Light, Camera, Audio, Colliders, Spin, Bob, Light Probe Volume); cargo-built,
hot-reloadable **Rust plugins**; runtime custom **GLSL shaders**; runtime scene
switching from gameplay code (first slice of the in-game API).

**Audio** — spatial / non-spatial `AudioSource` + `AudioListener` with distance
attenuation, driven in play mode.

**Physics / collision** — Box / Sphere / Mesh colliders with editable viewport
widgets (authoring only; the physics engine is planned).

**Lighting bake — IN TESTING, NOT YET VERIFIED.** A GPU lightmap + light-probe
bake (Vulkan ray query: BLAS/TLAS, path-traced direct + soft shadows +
multi-bounce indirect, SH-L1 probes) with a "Baker's Man" editor tab and
`.lightmap`/`.lightdata` sidecars. It compiles and runs on RT-capable hardware,
but the GPU output has not been visually validated and runtime sampling
(Phase 5) is not done, so it does not light the scene yet.

## Planned

Tracked in **[Features.md](Features.md)** (with task breakdowns); the major
goals:

- **Pawns & camera possession** — controllable entities that drive the active camera.
- **Player controllers** — first-person, third-person, isometric/top-down, strategy.
- **Input binding system** — device-independent actions + control schemes (KB/mouse, gamepad).
- **In-game API** — a scripting surface so components read/affect the world (objects, transforms, physics, materials, audio, camera, scene).
- **Editor-only vs gameplay components** — components that never ship in a built game.
- **Game UI system** — retained scene-graph UI (screen + world-space) for menus/HUD/inventory.
- **3D physics engine** — rigid bodies, materials, joints, layer matrix, queries (Rapier3d).
- **Lighting** — bake runtime sampling + lightmap GI in the standard shader; HDR skybox + IBL.
- **VR** — OpenXR stereo rendering, controller input, and in-headset world/avatar editing.
- **Custom shaders phase 2** — Slang frontend with SPIR-V reflection.
- **Milestones M3–M7** — VRM avatars, VR, full editor, networking, content pipeline.

## Roadmap (milestones)

- [x] **M1 — Foundation**: window + Vulkan device + swapchain, frames in flight,
      resize/out-of-date handling
- [x] **M2 — Meshes & materials**: depth, orbit camera, glTF/FBX import, citrus
      standard shader phase 1, variant cache, egui material inspector with live edits
- [ ] **M3 — Assets**: full glTF scenes, VRM avatar import (rig, blendshapes,
      spring bones), skinned animation
- [ ] **M4 — VR**: OpenXR session sharing the Vulkan device, stereo rendering
      (multiview), head/controller tracking, locomotion + grab
- [~] **M5 — Editor** (in progress): dockable editor, gizmos, hierarchy, materials,
      components/plugins, code editor — much landed; avatar setup tooling still to come
- [ ] **M6 — Networking**: world/avatar sync, voice, IK replication
- [ ] **M7 — Content pipeline**: publishing format, mobile-tier shader validation,
      content sandboxing (worlds are data, not code)

## Running

```sh
cargo run --bin citrus
```

Open a model or scene directly: `cargo run -- world.glb` / `model.fbx` /
`scenes/world.scene`.

### Editor

Dockable layout (drag tabs to rearrange): **Viewport**, **Scene**, **Inspector**,
**Files**, **Log**, **Baker**, and **Code** tabs. Menu bar: File (new/open/save
scenes), Edit (undo/redo), Tools, View (stats, layout, camera preview), Help.

| Input | Action |
|---|---|
| Left click (no drag) | Select object (purple outline) |
| Left drag | Orbit selection / viewport center (Alt: force orbit over gizmo) |
| Escape | Deselect |
| F | Focus selected object |
| W / E / R | Gizmo: move / scale / rotate (also buttons in the viewport) |
| Right mouse (hold) | Mouse-look (cursor hidden) + WASD/Q/E fly, Shift fast |
| Middle mouse drag | Pan |
| Scroll | Dolly (over the viewport) |
| Ctrl (while dragging gizmo) | Snap to grid; while rotating, snap to 15° |
| Play / Stop | Run components (menu bar); Stop restores the pre-play state |
| Ctrl+S | Save scene |

Viewport overlays: gizmo tool buttons (top-left); pivot mode, orientation, snap +
grid size (top-center); widget filter (top-right).

`RUST_LOG=debug` for verbose logs. Install `vulkan-validation-layers` to get
validation automatically in dev.
