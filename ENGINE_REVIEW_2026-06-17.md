# Citrus Engine — Review, Fixes & Roadmap (2026-06-17)

A pass over the whole codebase comparing Citrus against how production engines
(Unreal, Unity) solve the same problems. This documents **what was fixed**, **what
was improved for performance**, **what new capability landed**, and an honest
**gap analysis of what is still required to ship games**.

All code changes in this pass compile clean (`cargo check --workspace`).

---

## 1. Bugs fixed

### 1.1 Reflection probe captured geometry inside-out (the reported "offset / zoomed" reflection)
**File:** `crates/citrus-render/src/lib.rs` (`step_refl_capture`, `record_scene_draws`)

**Root cause.** The main scene passes render with a **negative-height Vulkan
viewport** (`y = H, height = -H`) — the standard trick to keep glam's Y-up
projection upright on screen. That viewport flip also flips triangle winding, and
the pipelines are tuned for it (`FrontFace::COUNTER_CLOCKWISE` + `CullMode::BACK`;
see the comment at `pipeline.rs:556`).

The reflection-probe cube capture, however, rendered each face with a **positive-height
viewport** (`lib.rs:4143`). Same pipelines, opposite winding → **back-face culling
inverted**: solid objects rendered their *back* faces (inside-out), single-sided
floors disappeared entirely. The captured cubemap was garbage, so reflections of
nearby objects (the pink ball) landed in the wrong place and at the wrong apparent
scale. This is exactly why the bake's G-buffer pass already uses `CullMode::NONE`
(`bake.rs:860`) — it hits the same positive-viewport winding issue.

**Fix.** Added a `capturing_refl` flag on the renderer. While the 6 cube faces
render, `record_scene_draws` forces every draw's pipeline key to `double_sided`
(= `CullMode::NONE`), so geometry is captured right-side-out — the same approach
the bake pass uses. The face orientation (positive viewport) is correct and matches
the CPU `face_dir` cube convention used for skybox reflections, so it was left alone.

> Note: box-projection parallax (`standard.frag`) is mathematically correct. If a
> reflection still looks "too big/small" after this fix, it is the probe **box
> size** vs. room size (authoring) — a `[10,6,10]` default box in a small room
> over-projects. Tighten the probe's Size to the room for 1:1 reflections.

### 1.2 EXR / HDR thumbnails rendered black or blown-out in the file browser
**File:** `crates/citrus-editor/src/file_browser.rs` (`decode_thumb`)

**Root cause.** EXR/`.hdr` images decode to **linear float** with values that
routinely exceed 1.0. The thumbnail path did a raw `to_rgba8()` — a clamp from
linear `[0,1]` to 8-bit with **no tonemap and no sRGB gamma**. A dim environment
read near-black; a bright sky clipped to flat white. Effectively "no preview."

**Fix.** HDR sources are now detected (float color type or `.exr`/`.hdr`
extension), tonemapped with **Narkowicz ACES** and **sRGB-encoded** — the same
rolloff the skybox/standard shaders use — so the thumbnail matches the in-engine
look. LDR images keep the fast path.

### 1.3 (hardening) Transparent LDR sources forced opaque only on the LDR path
Kept the existing "force alpha = 255" guard for LDR images that decode fully
transparent (some TGAs), now scoped correctly so it doesn't fight the HDR path.

---

## 2. New features

### 2.1 Material previews — material applied to a sphere (file browser)
**File:** `crates/citrus-editor/src/file_browser.rs`

`.material` tiles now show the material **rendered on a lit sphere** instead of a
generic glyph — the Unity/Unreal content-browser behavior. Implementation is a
self-contained **CPU shader**: it parses the material's PBR params straight from
the RON (`base_color`, `metallic`, `roughness`, `emission_color/intensity`) and
shades a unit sphere with a key light + Blinn specular + a faux-sky environment
reflection, so **base colour, metalness, glossiness and emission are all visible
at a glance**. Tonemapped + sRGB-encoded to match the engine. Cached and
frame-budgeted exactly like image thumbnails, and **re-rendered when the material
file's mtime changes**, so editing a material refreshes its tile.

### 2.2 Frustum culling (performance / shipping essential)
**Files:** `crates/citrus-render/src/{types.rs,lib.rs}`, `crates/citrus-engine/src/scene.rs`

Citrus previously submitted **every** mesh every frame regardless of whether it was
on screen — the single biggest CPU/GPU waste for any non-trivial level. Added
**CPU frustum culling** for the main camera pass:

- `DrawCmd` carries a per-draw object-space `bound_radius` (AABB half-diagonal),
  filled in `sync_draws` from the mesh AABB.
- The renderer extracts the 6 frustum planes from `proj * view`
  (Gribb-Hartmann, 0..1 depth) and skips any draw whose world bounding sphere is
  fully outside — radius scaled by the transform's largest axis (handles non-uniform
  scale conservatively).
- **Scoped to the main pass only.** The shared draw `order` still feeds shadow
  casters and the reflection-cube capture (which need off-screen geometry), so those
  are untouched.
- **Safety net:** if culling ever removed *everything* (e.g. a bad matrix), it
  falls back to the full order — a convention mistake can never blank the viewport.

This is O(draws) per frame with no GPU round-trip and scales the engine from "demo
scene" to "real level" on the CPU-submission side.

### 2.3 Layer system — Unity-style layers + collision matrix (rendering **and** physics)
**Files:** `crates/citrus-core/src/lib.rs`, `crates/citrus-engine/src/{scene.rs,physics.rs,
runtime.rs,editor_app.rs}`, `crates/citrus-assets/src/scene_file.rs`,
`crates/citrus-editor/src/components.rs`

**Which model, and why.** Two designs were on the table:

- **Unity:** 32 named layers (a bitmask per object), a **camera culling mask** for
  rendering, and a symmetric **32×32 Layer Collision Matrix** for physics. Simple,
  designer-facing.
- **Unreal:** collision **channels / object types** with 3-state **Block / Overlap /
  Ignore** response matrices and per-component overrides — more expressive, heavier
  to author; rendering visibility isn't layer-based at all (it uses show flags).

The request ("set an object's layer; layers affect the renderer and collision
physics; a layer matrix") *is* the Unity model, and Citrus already cites Unity as its
UX reference — so **Unity-style layers** were implemented:

- **Data** (`citrus-core`): `LayerSettings { names[32], collision[32] }` (the
  collision rows are bitmasks, kept symmetric via `set_collide`), plus a per-object
  `layer: u8` and a per-camera `culling_mask: u32`. Unit-tested (symmetry, defaults,
  short-vec repair).
- **Serialization:** `layer` on each `SceneEntry`, `layers` on the `SceneFile` — both
  `#[serde(default)]`, so old scenes load unchanged (everything on layer 0 "Default",
  all-collide).
- **Physics** (`physics.rs`): each collider gets rapier `InteractionGroups`
  (membership = its layer bit, filter = its collision-matrix row), so "layer A ignores
  layer B" is honored by the solver — the canonical use (player vs. trigger-only
  volumes, ragdoll self-collision, etc.).
- **Rendering:** `LoadedScene::visible_layers` masks draws in `sync_draws`. In the
  **editor** it's the viewport's per-layer visibility toggle; in a **shipped game**
  the runtime sets it from the **active camera's `culling_mask`** each frame (e.g. a
  first-person view-model layer the main camera sees but the shadow/reflection cameras
  don't).
- **Editor UI (all of it):**
  - **Inspector** — a Layer dropdown on every object's header.
  - **Tools → Layers** window — rename the 32 layers, toggle the **collision matrix**
    (lower-triangle, symmetric), toggle **per-layer viewport visibility**, and reset
    to defaults. The matrix auto-sizes to the highest layer in use so it stays compact.
  - **Camera component** — a "Culling Mask" section (Everything / Nothing + per-layer
    checkboxes).

This is a genuine shipping essential (view-model/FX layers, physics interaction
filtering) that both Unity and Unreal consider table-stakes.

### 2.4 VR per-eye frustum culling (VR performance)
**File:** `crates/citrus-render/src/lib.rs` (`render_xr_eye`)

The stereo path renders the **whole scene once per eye** (`record_scene_draws` per
eye), and previously submitted **every** draw to **both** eyes. The frustum culling
from §2.2 is now applied **per eye** against that eye's own view/proj, so an object
off the side of the left eye's frustum is skipped for the left eye. Direct CPU+GPU
saving on the most submission-heavy path in the engine. (More VR wins in §8.)

---

## 3. How production engines solve asset loading (and where Citrus already stands)

The brief asked specifically about Unreal's approach to **not locking the UI** during
asset loads. Unreal's model is:

1. **Async streaming requests** (`StreamableManager` / `LoadPackageAsync`): the game
   thread *requests* an asset and gets a handle; the actual file read + deserialize
   happens on worker threads.
2. **GPU upload decoupled from decode**: texture mips decode on workers and upload
   via a transfer/copy queue, never blocking the render thread.
3. **Placeholder-then-swap**: a default/!low-mip asset shows immediately; the real
   asset swaps in when ready.

**Citrus already implements this pattern** (see `tasks.rs` + the background-task
system, and `Features.md` §"Background-task system"):

- Model import parses on a **worker thread**, GPU-applies on main.
- Scene-load material textures decode + upload off-thread via a **transfer-queue
  uploader**, keyed by `(path, srgb)`; the main thread only installs handles.
- The startup **splash** decodes its animation on a worker while the Vulkan renderer
  builds on another, so nothing blocks.
- **BC texture import cache** (`tex_cache.rs`): each source is block-compressed once
  and persisted, so later loads skip decode entirely.
- The new **material/EXR thumbnails are frame-budgeted** (`THUMB_PER_FRAME`), so even
  opening a 4K-texture-heavy folder streams in over frames instead of hitching.

So the architecture is sound and matches Unreal's intent. The remaining gaps are
**(a)** a unified `AssetHandle<T>` + ref-counted asset registry so any subsystem can
request-and-await (today it's per-feature), and **(b)** mip **streaming** (load low
mips first, stream high mips on camera proximity) — see §5.

---

## 4. Light baking — current state

The baker is already strong and close to production technique-for-technique:

- GPU ray-query path tracer (BLAS/TLAS), direct + shadow + multi-bounce indirect,
  SH-L1 light probes, per-object lightmaps (`R32G32B32A32_SFLOAT` array).
- **Baked soft shadows** via per-light radius (disc sampling).
- **Edge-aware À-Trous denoise** (Dammertz), **seam stitching** across UV charts,
  and **gutter dilation** — the three things that make a baked lightmap not look
  like Monte-Carlo noise with black seams.
- Chunked, cancellable bake on the task system (one lightmap/probe-volume per frame).

Left as-is this pass (it needs **GPU visual validation**, not code changes, and the
technique set is already correct). The documented next steps — octahedral sphere
unwrap (less pole distortion than lat-long) and even cube-face packing — remain the
right follow-ups.

---

## 5. Gap analysis — what is *absolutely* needed to ship games

Ranked by how blocking each is for a real title. Items marked ✅ exist; the rest are
the honest "not shippable without this" list.

| Capability | Status | Notes |
|---|---|---|
| PBR rendering, lights, shadows | ✅ | Solid |
| GI (baked + realtime Flux) | ✅ | Strong; needs visual validation |
| Frustum culling | ✅ **(added this pass)** | |
| glTF/FBX import, materials, audio | ✅ | |
| Input bindings, gamepad | ✅ | |
| Physics (rapier), colliders, rigidbody | ✅ partial | No queries/triggers/joints yet |
| Networking + voice | ✅ partial | LAN star-relay; no NAT/prediction |
| **Runtime UI system (HUD/menus)** | ❌ **#1 blocker** | No game can ship without menus/HUD. Spec exists (Features §2F); nothing built. Retained `UICanvas` + text rendering is the big one. |
| **Occlusion culling / LOD / instancing** | ❌ | Frustum culling (done) is step 1; large scenes also need occlusion + mesh LODs + GPU instancing for many identical props. |
| **Asset streaming / level streaming** | ◐ | Async loads exist; no unified `AssetHandle` registry or mip/level streaming for open worlds. |
| **Save/load game state (serialization API)** | ❌ | Scene serialization exists; no gameplay save-game API. |
| **Animation system (skeletal blend trees)** | ◐ | Rigging/IK import exists; no state machine / blend trees / retargeting at runtime. |
| **Particles / VFX** | ❌ | No GPU particle system. Nearly every game needs it. |
| **Build/cook pipeline** (strip editor, package) | ◐ | Game bundle path exists (Features §2I); needs an editor-strip + asset-cook step. |
| Bloom / DOF / motion blur (HDR post) | ◐ | Post profile authorable; bloom/CA need the offscreen HDR pass. |
| Audio mixer / buses / reverb zones | ❌ | Only per-source playback today. |

**Recommended next three, in order:**
1. **Runtime UI (retained `UICanvas` + text atlas).** Single biggest blocker — no
   shippable game lacks menus/HUD, and the VR path needs world-space UI too.
2. **GPU instancing + mesh LOD.** Pairs with the frustum culling added here to make
   real-sized levels perform.
3. **Unified asset registry + level streaming.** Turns the existing async loaders
   into an open-world-capable streaming system.

---

## 6. Comparison to Godot & Bevy — what we're missing

Both are open-source and a fair yardstick for "what a modern small/indie engine is
expected to have."

### vs. Godot (designer-facing, batteries-included)
Godot's strengths are breadth of editor tooling. Gaps where Citrus trails:

- **Runtime UI** — Godot's Control-node UI (anchors, containers, themes, signals) is a
  flagship feature. Citrus has **none** (see §5 #1). Biggest single gap.
- **Animation system** — Godot's `AnimationPlayer`/`AnimationTree` (keyframe any
  property, state machines, blend spaces). Citrus has rig/IK import but no runtime
  animation graph.
- **Tilemaps / 2D pipeline** — Godot is first-class 2D. Citrus is 3D-only. (Likely
  out of scope, but worth a deliberate "no" rather than an accidental gap.)
- **Signals / event bus** — Godot's signal system is how nodes communicate. Citrus has
  no component-to-component event mechanism (listed `todo` in Features §2D).
- **`NavigationServer` / navmesh pathfinding** — Citrus has no navmesh; AI/agents
  can't path. Godot ships baking + agents + avoidance.
- **Resource hot-reload + `.import` pipeline** — Godot reimports assets on change with
  per-asset import settings. Citrus has a texture cache + LSP-driven `.rs` reload but
  no general per-asset import-settings pipeline.
- **Scene instancing / prefabs with overrides** — Godot's `PackedScene` instancing
  with per-instance property overrides. Citrus has duplicate but no prefab/nested-scene
  asset with override semantics.

### vs. Bevy (code-first, ECS, performance)
Bevy's strengths are architecture and rendering scalability. Gaps:

- **ECS / data-oriented core** — Bevy is archetypal ECS; Citrus uses `Vec<dyn Component>`
  per object (per the architecture note, an intentional choice). Fine for the current
  scale, but **batching, parallel system scheduling, and cache locality** are weaker.
  If scenes grow large, an ECS or at least archetype-batched draw submission is the
  scaling path.
- **Render graph** — Bevy has a declarative render graph with automatic resource
  barriers. Citrus hand-codes passes + barriers in `lib.rs` (works, but every new pass
  is manual Vulkan sync — error-prone and a velocity tax).
- **GPU-driven rendering / batching / instancing** — Bevy batches and (increasingly)
  GPU-culls. Citrus issues one draw per slot with no instancing — fine post-frustum-cull
  for moderate scenes, but many identical props (foliage, crowds) need **instancing**.
- **Asset server with typed handles** — Bevy's `AssetServer` + `Handle<T>` +
  hot-reloading is exactly the "unified asset registry" gap from §3.
- **Plugin ecosystem / schedule** — Bevy's plugin + system-ordering model. Citrus has
  dylib plugins but no scheduled-system model.

**Net:** the **critical, both-engines-have-it, we-don't** items are: **(1) runtime UI**,
**(2) an animation/state-machine system**, **(3) a typed asset registry with
hot-reload**, **(4) GPU instancing for repeated meshes**, and **(5) a
signal/event bus**. UI and animation are the two that block shipping most genres.

---

## 7. VR rendering — performance optimizations

What the stereo path does today (`render_xr_eye`): one full scene render per eye, a
separate command buffer per eye, desktop post/SSR/screen-GI disabled for VR (good —
those are expensive and camera-space). Concrete wins, in rough ROI order:

1. **Per-eye frustum culling — ✅ done this pass (§2.4).** Immediate, zero-risk.
2. **Single-pass stereo / multiview (`VK_KHR_multiview`).** The biggest structural win:
   render both eyes in **one** pass with a 2-layer framebuffer and `gl_ViewIndex`,
   roughly **halving CPU draw submission** and improving GPU vertex reuse. The renderer
   already declares Vulkan 1.3 (multiview is core), and shaders would select the eye's
   view/proj by `gl_ViewIndex`. This is the standard modern VR pipeline; today's
   two-separate-passes approach leaves the most on the table.
3. **Fixed-foveated rendering (`VK_KHR_fragment_shading_rate`).** Shade the periphery
   at a coarser rate than the center. Big fragment-cost saving (lens periphery is
   compressed anyway), modest effort with FSR attachments. Pairs well with multiview.
4. **Render at the runtime's recommended resolution and skip the desktop mirror cost.**
   Ensure the eye extent uses `recommendedImageRectWidth/Height` (not max), and make the
   desktop "spectator" mirror optional / lower-rate — drawing the full scene a *third*
   time for the monitor is pure overhead during play.
5. **Cull shadows/reflection captures to the combined eye frustum**, and lower their
   cadence in VR (reflection cube is already chunked; shadows could update less often).
6. **MSAA over post-AA in VR.** VR needs MSAA (TAA/SMAA smear with head motion); make
   sure the eye targets use MSAA resolve rather than a post AA pass.
7. **Late-latching / reprojection** is the compositor's job (OpenXR runtime handles
   it), but ensure poses are sampled as late as possible before `record_scene_draws`
   to minimize perceived latency.

**Recommended next VR step:** multiview (#2). It's the single change that most improves
VR frame time and is the foundation foveation builds on.

---

## 8. Summary

- **Bugs fixed:** 3 (reflection cube inside-out culling; EXR/HDR thumbnails;
  transparent-LDR hardening).
- **Performance:** frustum culling on the main pass **and per VR eye** — the
  foundational scene-scaling win, with a no-blank-screen safety net.
- **New features:**
  1. **Material sphere previews** in the file browser (CPU-shaded from PBR params).
  2. **Correct HDR/EXR thumbnail previewing** (ACES + sRGB).
  3. **Unity-style layer system** — per-object layers, a physics **collision matrix**,
     per-camera **render culling masks**, full editor UI (inspector dropdown,
     Tools → Layers window, camera culling-mask widget). Unit-tested.
  4. **Frustum culling** subsystem (main pass + per-eye VR).
- **Asset loading:** confirmed the engine already matches Unreal's worker-thread /
  decouple-GPU-upload / placeholder-swap model; identified the two remaining gaps
  (unified handles, mip streaming).
- **Engine comparisons:** Unreal (§1.1, §3), **Godot & Bevy** gap analysis (§6) — the
  critical missing items both have are **runtime UI, animation graph, typed asset
  registry, GPU instancing, signal/event bus**.
- **VR:** per-eye culling shipped; **multiview (single-pass stereo)** identified as the
  highest-ROI next VR change, then fixed-foveated rendering (§7).
- **Shipping gap analysis:** the #1 blocker is a **runtime UI system**; full table in §5.

All changes verified with `cargo check --workspace` and `cargo test --workspace`
(15 test suites green, including new layer-matrix tests).
</content>
</invoke>
