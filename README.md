# citrus

A Rust + Vulkan game engine with a dockable editor, built VR-first
(OpenXR). Powers [vrsh](../vrsh), a social-VR platform — but citrus itself is
general-purpose: scenes, materials, FBX/glTF import, VRM avatars + rigging/IK
(in progress), shader variants, undo, hierarchy, and a growing toolset.

## Stack

| Concern | Choice |
|---|---|
| Graphics | Vulkan 1.3 via [ash](https://crates.io/crates/ash) (dynamic rendering, sync2) |
| VR | OpenXR via the [openxr](https://crates.io/crates/openxr) crate (Monado / SteamVR runtimes) |
| ECS | [hecs](https://crates.io/crates/hecs) |
| Editor GUI | [egui](https://crates.io/crates/egui), rendered in-engine |
| Worlds/props | glTF 2.0 |
| Avatars | VRM (glTF extension): humanoid rig, expressions, spring bones |
| Default shader | citrus standard uber-shader, Poiyomi-inspired — see [docs/shaders.md](docs/shaders.md) |
| Windowing | winit |

## Workspace layout

```
crates/
  citrus      binary entry point (desktop now, +VR at M4)
  citrus-engine   app loop, ECS world, timing, input
  citrus-render   ash Vulkan renderer (context, swapchain, frames; RHI grows here)
  citrus-xr       OpenXR session/swapchains/input          (stub)
  citrus-assets   glTF + VRM loading                       (stub)
  citrus-editor   egui in-engine world/avatar editor       (stub)
```

## Roadmap

- [x] **M1 — Foundation**: window + Vulkan device + swapchain + animated clear,
      frames in flight, resize/out-of-date handling
- [x] **M2 — Meshes & materials**: depth buffer, orbit camera, glTF import,
      citrus standard shader phase 1 (PBR/toon hybrid, emission, cutout/blend,
      normal maps), specialization-constant variant cache, egui material
      inspector with live edits (`cargo run` → built-in test scene;
      `cargo run -- world.glb` → glTF)
- [ ] **M3 — Assets**: full glTF scenes, VRM avatar import (rig, blendshapes,
      spring bones), skinned animation
- [ ] **M4 — VR**: OpenXR session sharing the Vulkan device, stereo rendering
      (multiview), head/controller tracking, basic locomotion + grab
- [ ] **M5 — Editor**: egui in-engine editor — world building (hierarchy,
      gizmos, materials) and avatar setup (VRM mapping, expression testing)
- [ ] **M6 — Networking**: world/avatar sync, voice, IK replication
- [ ] **M7 — Content pipeline**: world/avatar publishing format, mobile-tier
      shader validation, content sandboxing (worlds are data, not code)

## Running

```sh
cargo run --bin citrus
```

Open a model or scene directly: `cargo run -- world.glb` / `model.fbx` /
`scenes/world.scene`.

### Editor

Dockable layout (drag tabs to rearrange): **Viewport**, **Scene** (object
list), **Inspector** (selected object: transform/mesh/material slots; or
selected file: .material editor, .scene loader), **Files** (project browser —
click to inspect, double-click models to import, drag `.material` onto meshes
or material slots, right-click to create assets). Menu bar: File (save/load
scenes), Tools (gizmo mode), View (stats, layout reset), Help (controls).

| Input | Action |
|---|---|
| Left click (no drag) | Select object (purple outline) |
| Left drag | Orbit selection / viewport center (Alt: force orbit over gizmo) |
| Escape | Deselect |
| F | Focus selected object |
| G / R / S | Gizmo: move / rotate / scale (also buttons in the viewport) |
| Right mouse (hold) | Mouse-look (cursor hidden) + WASD/Q/E fly, Shift fast |
| Middle mouse drag | Pan |
| Scroll | Dolly |
| Ctrl (while dragging gizmo) | Snap to grid |
| Ctrl+S | Save scene |

Viewport overlays: gizmo tool buttons (top-left); pivot mode (Pivot/Center),
Global/Local orientation, snap toggle and grid size (top-center).

`RUST_LOG=debug` for verbose logs. Install `vulkan-validation-layers` to get
validation automatically in dev. Working backlog: [TODO.md](TODO.md).
