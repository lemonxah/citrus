# ZestEngine Engine — Review & Outstanding Work (2026-10-30)

This pass covered three things:
- Flux / FluxRT / FluxVoxel correctness and performance.
- A full render-pipeline review: HDR end to end, CPU stalls, wasted passes.
- A large feature batch: physics and navigation gizmos, animation controllers, the shader system v2 with ZestEngine Toon, project templates, built-in networking, user-generated content, and Windows builds.

The second half of this document is the honest list of what is still missing, including the platform picture: Linux, Windows and Android.

Unless a line says otherwise, everything below was verified:
- **Release build:** `cargo build --release --workspace` is clean.
- **Tests:** `cargo test --release --workspace` passes all 231 tests (assets 47, core 54, engine 121, render 6, others 3).
- **GPU checks** ran on an RTX 4080 SUPER:
  - The sample game under XWayland.
  - The Windows build under Wine.

---

## 1. What landed

### 1.1 Flux GI family
- **FluxRT with a bake loaded (mixed lighting).** Flux and FluxRT keep running when baked lightmaps are present, and they add only what the bake can't contain:
  - moving emitters
  - realtime lights
  - dynamic objects

  The relevant fields are `FluxSettings::mixed_lighting`, `probe_info.y` and `debug.z = 2`. Static emitters are filtered out of the gather in this mode, so baked light is never counted twice. An emissive object flying past the couch now adds light to its lightmap.
- **FluxRT without a bake: the couch stripes and flicker.** The causes and fixes:
  - **Back-face hits:** they are now detected and treated as invalid.
  - **Emitter shadow rays:** they now start from the geometric normal.
  - **Probe placement:** probes use the closest depth plus a normal offset (`flux_probe_surface` / `flux_probe_offset`).
  - **Filtering:** the integrate pass has a normal and plane edge-stop, magnitude comes from the denoised scalar (SH used as a ratio), and SH history is bilinear-validated with a luma clamp.
  - **Result:** the arm is stable to 1/255 between frames.
- **Correctness fixes:**
  - Metallic is now read from `params.x` in the RT gather.
  - The instance custom index is now `gpu_instances.len()`.
  - Emitters were counted twice.
  - FluxVoxel light hashing ignored direction, spot angle and light kind.
  - An empty volume now has a sentinel.
  - The FluxVoxel camera clipmap now scrolls in quarter-extent steps, so the static base is rebuilt on a step rather than every probe-spacing move. (A toroidal scroll that never rebuilds is not done.)
  - WAR barriers on the GI targets now cover both fragment and compute stages.
- **Performance:**
  - Flux and FluxRT share the GDF and emitter data.
  - The RT probe re-bake runs only when it is actually needed (fallback or VR).
  - Screen-GI transients and the per-frame RT buffers are persistent.
  - The emitter shadow rays are merged.
  - The forward 5x5 filter is folded into integrate.
  - `rt_scene` is built once per frame.

### 1.2 HDR pipeline (end to end)
- **Verified:** lowering exposure by 3 EV darkens the scene about 5x (sRGB), while 8x emissives keep their colour instead of dimming with it. Values above 1 reach the tonemapper (Features.md has the numbers).
- **Linear RGBA16F everywhere radiance is sampled again later:**
  - the main pass
  - the viewport and the camera preview
  - the reflection capture cube (`ENV_CUBE_FORMAT`)
  - planar mirrors
  - VR eyes, which are followed by bloom and post
- **Pipeline formats:** every pass sets its colour format (`PipelineKey::color_format` via `set_target_format`), so pipelines always match the target they draw into.
- **Environment maps** are built in float. EXR is stored as f16, sRGB sources are decoded, and `.hdr` loads as float.
- **Bloom:** a mip-chain bloom (`bloom.rs`). Post-process volumes blend per effect and have a box gizmo with a blend-distance shell.

### 1.3 Render pipeline: milliseconds removed
- **No CPU waits on the frame path:**
  - TLAS slots are built per frame inside the frame command buffer.
  - The reflection capture is recorded in-frame, one face per frame, with GPU mip generation and a copy into the live environment.
  - Probe SH is staged and copied in the command buffer.
  - VR eyes use a slot ring with fences.
- **The depth prepass is reused.** `invariant gl_Position` makes the depths match, so the main pass uses an EQUAL depth test with depth writes off and loads the prepass depth instead of re-rasterising it.
- **Shadow layers are cached** by a signature of the view, the casters and the mesh versions. Shadow casters are culled per view.
- **Draw recording:**
  - A state sort key orders draws.
  - Redundant bind calls are skipped.
  - Frustum culling has no "nothing visible" fallback.
  - Planar and preview passes are culled too.
- **Scene walks on the CPU:** world transforms and the active mask are computed once per frame.
- **GPU instancing.** Scene draws take their model matrix from a per-instance vertex stream (`standard.vert` / `outline.vert`, locations 8-11). Each pass writes its matrices into a per-frame ring buffer (per VR eye slot for VR), and a run of draws with the same pipeline, material, mesh and per-object push data becomes one `vkCmdDrawIndexed` with `instance_count > 1`.
  - Custom-shader materials still draw one at a time, because their fragment code may read `pc.model`.
  - Measured: the strategy template draws 33 objects in 9 draw calls (4 instanced); the FPS template draws 35 in 7 (5 instanced).
- **TLAS refit.** When only transforms changed, the per-frame TLAS is updated in place (`ALLOW_UPDATE` / `UPDATE`) instead of rebuilt, with a full rebuild every 120 refits. FluxRT GI went from 0.46 to 0.43 ms in the benchmark below.
- **Skinned mesh bounds** follow the pose: every joint's bind-space vertex box is moved by that frame's skinning matrix, and culling uses the union.
- **The G-buffer** is packed as A2B10G10R10.
- **Occlusion culling:** a visibility bake with preview, saved per scene alongside lightmaps, probes and reflection probes.

### 1.3a Measured before / after
Method:
- **Before:** HEAD (`fb9c710`, the commit before this pass), extracted read-only with `git archive`.
- **After:** the working tree.
- **Both:** run as the sample game on HEAD's own `world.scene`, with no bake, the orbs animating, and vsync off.
- **Timings:** GPU time comes from the renderer's timestamp zones. Frame time is the mean wall-clock interval over 240 frames.
- **Reproduce:** `ZESTENGINE_FRAME_STATS=1` turns this logging on in any game build. It also records the GPU timestamp zones without the profiler window, and logs draw calls.

| GI mode | Frame (ms) before → after | GPU (ms) before → after | GI pass (ms) before → after | Shadows (ms) before → after |
| --- | --- | --- | --- | --- |
| FluxVoxel | 1.85 → 0.24 | 0.70 → 0.21 | n/a | 0.028 → 0.009 |
| Flux (software) | 3.22 → 1.78 | 2.25 → 1.76 | 1.90 → 1.52 | 0.028 → 0.010 |
| FluxRT | 1.85 → 0.68 | 0.27 → 0.66 | 0 → 0.43 | 0.028 → 0.010 |

What the table shows:
- **CPU:** the old frame was CPU-bound in every mode (1.85 ms with only 0.27-0.70 ms of GPU work) because of the per-frame waits listed in 1.3. Now the frame time is within 0.03 ms of GPU time.
- **FluxRT's GPU time went up** because in HEAD the FluxRT gather **never ran** in the game path (GI pass 0.000 ms). That was the "FluxRT isn't lighting anything" bug. 0.43 ms is the cost of it actually running, and the frame is still 2.7× faster.
- **The "after" column includes added work:** HDR bloom and the per-object depth prepass, which HEAD didn't have.
- **A still scene costs less:** once the GI converges and nothing moves, the trace is skipped (GI pass 0 ms). Your scene with its bake, frozen orbs and FluxRT runs at 0.26 ms per frame (0.24 ms GPU).

Later passes the same night, profiled with new GPU zones inside the Flux trace (gather / denoise / integrate, also in the profiler and the `ZESTENGINE_FRAME_STATS` log):
- **Flux (software) gather: 1.34 → 0.87 ms (-35%).** Hits no longer march a shadow ray for lights whose unshadowed contribution there is below 1e-3, and the hit normal uses a 4-tap tetrahedral gradient instead of 6 taps. Shadow rays also stop where they leave the GDF box, so a directional light's ray no longer runs all 96 steps. Image difference against the previous build: mean 0.47/255 (temporal noise).
  - The whole Flux GI pass is now 1.00 ms (was 1.90 in HEAD), and the frame is 1.27 ms (was 3.22).
- **FluxRT gather: 0.29 → 0.27 ms** from the same light threshold on its ray-query shadow rays.
- **FluxVoxel static rebuild: 68 → 5 ms, now off the main thread.** This rebuild runs at load, on every camera-clipmap step, and on every static-light edit. Nearly all of it was the DDGI distance moments: each ray marched the full scene diagonal and was then clamped to 8 m, so the march now stops at 8 m (identical output), and the probes are split across CPU threads. After the first build it runs on a worker thread while the previous base keeps rendering, so moving the camera through a clipmap no longer hitches.
- **CPU scene walks:** the CPU share of the frame now measures 0.02-0.03 ms in both the sample game and the strategy template, so the planned per-component index lists weren't worth adding.

### 1.3b FluxRT and the bake: measured
In a copy of your `world.scene` (FluxRT, Ultra, with bake), I froze the orbits and compared GI on against GI off:
- **With the bake:** 8384 pixels gain more than +8 in red and blue, and 5210 more than +8 in green. The added light sits on the couch front and arm next to the orbs, the same places as in the no-bake run. Dynamic emitters add light on top of the lightmap.
- **With the orbs moving** (screenshots at 6, 8, 10 and 12 s, GI on against GI off), a magenta pool follows the magenta orb across the lightmapped pillow and couch arm. At 12 s, 8789 pixels gain more than +8 in red and blue.
- **The floor stays dark under the orbs** because they orbit the helmet about 1.1 m above the floor. A 10 cm sphere at emission intensity 1 is a weak light at that distance. The green "pool" in earlier screenshots was bloom; it shows with GI off too.
- **New Emissive Boost setting** (Environment > Realtime GI > Advanced) scales how much light emissive objects throw into the GI without changing how bright they look, like Lumen's emissive boost.
  - It applies to Flux, FluxRT and FluxVoxel.
  - At 8, FluxRT puts a clearly visible, local green pool on the couch (average +12 green across the couch).
  - FluxVoxel keeps its built-in ×8 emissive gain. It tints the whole couch evenly because of its coarse probe grid, while FluxRT's light falls off with distance squared as it physically should.

### 1.4 Editor gizmos for built-in systems
| Component | Gizmo |
| --- | --- |
| Rigid Body | collider |
| Character Controller | capsule |
| Physics Joint | anchor handle, axis and limits, connection line |
| NavMesh Surface | resizable bake box |
| Nav Agent | radius and stopping ring; the live path in Play |
| Post FX volume | box plus blend shell |

The underlying primitive is `GizmoSpec::Lines`.

### 1.5 Animation
- **Animator Controllers** (`.animctrl`) are state machines. They support:
  - Any State and exit time
  - triggers and cross-fades
  - layers with masks and additive blending
  - parameter drivers, so code or other controllers can drive one
- **Authored `.anim` clips.**
- **Editor tabs:** Animator Controller and Animation.

### 1.6 Shader system v2 and ZestEngine Toon
- **External shaders:** GLSL or HLSL files declare their properties (`@property`, `@feature`, `@preset`), and those properties appear in the inspector.
- **Variants:** feature toggles compile to specialization-constant keyword variants, so a material's pipeline contains only the features it uses. The game bundle bakes these variants.
- **ZestEngine Toon** is a single Poiyomi-style uber shader with Simple, Medium and Heavy presets. It covers:
  - toon ramps, rim, matcaps, specular modes
  - emission, detail and decal layers
  - dissolve, glitter, UV discard, parallax
  - an inverted-hull outline
- **GPU check:** toon shading, rim and outline all render. A red outline test is in the sample game.

### 1.7 Templates, networking, user content
- **Templates:** FPS, TPS, top-down click-to-move, strategy, VR shooter and VR social, plus a gameplay kit. Each template plays immediately; there is a New Project dialog and `zestengine --new-project`.
- **Networking:**
  - `NetworkObject` with ownership, interpolation and active/parent sync
  - `NetworkVariables`, RPCs, `NetworkAnimator`, `NetworkAvatar`
  - a Network Manager with a player prefab, spawn points and auto-start
  - LAN discovery and a Network window
- **User content:** `.zestenginepak` packages (avatars, worlds, props) load into a running game without recompiling. They are hashed, sanitised, size-limited and cached.

### 1.8 Follow-ups finished in this pass
- **Windows game builds from Linux.**
  - Build targets: `BuildTarget::{Host, Windows}`.
  - Project Settings has a Target dropdown, and the File menu has **Build Game for Windows**. From the CLI: `zestengine --build <dir> --target windows`.
  - The build cross-compiles with `cargo xwin` and the MSVC toolchain, then ships `phonon.dll` (Steam Audio) and its licence files in `build-windows/`.
  - Verified: the FPS template built in 92 s using the shared target dir. Under Wine the exe found the RTX 4080, started Steam Audio and rendered the HUD, sky and shadows.
- **Game builds run in the background.** They used to block the editor UI; they now run as a background task with progress shown in the status bar.
- **Prefab references survive instancing.** `Prefab::instantiate` gives fresh ids and rewrites references between prefab objects inside component data. This affects the editor drop, `ctx.spawn_prefab` and network spawns. Template prefabs keep their ids for exactly this reason.
- **VR Social networked avatars.**
  - The Network Manager spawns `prefabs/net_avatar.prefab` for each player.
  - Network Avatar gained **Follow Local Player** (the owner's copy mirrors the scene's VR Player rig, in VR or on desktop) and **Hidden For Owner**.
  - Other players see the head, hands and torso, and the owner doesn't see a duplicate of themselves. Covered by a test.
- **The game runtime ignores out-of-window cursor positions.** A stale position reported in desktop coordinates at window creation was making the strategy template's RTS camera edge-pan to the map corner on start.
- **A flaky UGC test is fixed.** The HTTP fetcher test's one-shot server now reads the whole request before replying, so the socket closes cleanly instead of resetting.
- **Crouch collision.** The First Person Controller shrinks the capsule while crouched (`crouch_height`, feet stay planted) and only stands up when there is headroom. Physics reshapes the capsule at runtime. Covered by a test.

---

### Follow-up pass (2026-10-30, evening)

- **Profiler gap fixed:** game builds never stamped the Scene, Reflections and Post GPU zones, so roughly 0.2–0.3 ms per frame was invisible. With the zones stamped, a FluxRT benchmark frame breaks down as GI 0.44 ms (gather 0.28, integrate 0.13), scene 0.20, post 0.015 and shadows 0.01.
- **Flux / FluxRT emitter light now per pixel:** it is identical from every view, at a cost of about 0.02–0.03 ms in the integrate. Gathering the visibility taps inside the main 5x5 probe loop was tried and was slower (0.17 ms, likely register spills), so it was reverted.
- **Forward pass:** lights behind the surface now skip the BRDF and shadow lookups.
- **Realtime probes:** Update Rate plus whole-cube captures. A 2048 probe at 4 Hz and at 30 Hz measured the same frame time.
- **Per-draw light culling** is implemented and pixel-identical, but neutral in the couch scene: one floor draw is reached by all 23 lights. So the 0.20 ms scene pass is not the light loop.
- **Scene-pass profile:** each feature was removed in turn on FluxVoxel, 1280x720. The light loop is about 0.15 ms of the 0.21 (0.062 without it), shadow lookups about 0.04, and environment/probe reflections under 0.01. Because the floor draw is reached by all 20 edge spots, per-tile light culling is the fix for big draws.
- **Next targets:** the scene pass, at 0.20 ms for 6 draws, is dominated by per-pixel features: the 25-tap screen-GI upsample, two-probe reflection blending, and point-light shadow PCF. Feature toggles are needed to split them, and finer per-pixel (tiled) culling would help large draws such as the floor. Measure them on an idle machine, because desktop GPU contention here swung single passes by 0.05–0.07 ms.

## 2. Platform status

### Linux: works
This is the primary development platform. The editor, game builds, VR (OpenXR) and Steam Audio all work.

### Windows: works (cross-built), native toolchain untested
- **Editor:** `cargo xwin build --release --target x86_64-pc-windows-msvc` (use `CARGO_TARGET_DIR=target-win` to keep it apart from the Linux build). Under Wine it runs at about 91 fps, and scenes and bakes load.
- **Game builds:** see 1.8.
- **What a Windows player needs:**
  - **The VC++ 2015-2022 runtime** (`vcruntime140.dll`). Most machines already have it. Static CRT was not enabled because the C/C++ dependencies (ISPC texture compression, Steam Audio) would need to match.
  - **A Vulkan 1.3 driver.**
  - **The OpenXR runtime**, but only for VR.
- **The GNU target (`x86_64-pc-windows-gnu`) is not supported:** `audionimbus-sys` only whitelists MSVC.
- **Untested:** a native Windows toolchain (MSVC on Windows itself) and real Windows hardware. Only Wine and the cross-build have been run.

### Android (Quest / phones): compiles and links; never run on a device
- **Done (2026-10-30):**
  - The engine and the sample game build for `aarch64-linux-android` with the installed NDK r29 (`/opt/android-sdk`), producing an ARM64 Android ELF.
  - winit uses `android-native-activity` on Android.
  - The plugin loader knows `.so` on Android.
  - `zestengine_engine::run_game_android(app, config, register)` (with `AndroidApp` re-exported) is the entry point for a game's `android_main`.
  - Build command: `CC/CXX/AR/linker = $NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android30-*`, `cargo build --target aarch64-linux-android`, with `CARGO_TARGET_DIR=target-android`.
- **APK packaging works:** `tools/android/build_apk.sh` produced a signed, verified 6.7 MB APK of the FPS template (NativeActivity, arm64-v8a, Vulkan 1.3). Assets are extracted on first run by `android_assets_root`.
- **Not done: running it.** No device is attached, and the SDK has no emulator system image (installing one means a multi-GB download into the root-owned `/opt` plus KVM / Vulkan passthrough). The blockers below are what a first run will hit, roughly in order:
1. **Runtime shader compilation.**
   - Engine shaders are compiled at build time, and game bundles precompile custom shaders to `.spv` sidecars, so a shipped game doesn't need `glslc`.
   - The gaps: UGC packages with custom shaders still compile at runtime, and the editor-side tooling assumes `glslc`. For Android, either ship SPIR-V inside packages (precompiled per variant at package time) or embed `shaderc` / `naga`.
2. **Texture compression.**
   - BC formats are not supported on mobile GPUs. The engine already falls back to uncompressed textures when BC is missing, so this works today, but memory use is heavy.
   - Fix: add an ASTC (or ETC2) path to the import cache (`tex_cache.rs`).
3. **Vulkan version.**
   - The context requests Vulkan 1.3 core with dynamic rendering, synchronization2 and buffer device address. Quest 3 drivers expose 1.3, but Quest 2 and many phones are 1.1.
   - Fix: an extension-based fallback, or declare Quest 3 and newer as the minimum.
   - Ray query must stay optional (it already is). Flux software GI and FluxVoxel are the mobile GI tiers.
4. **Asset I/O: done** with extraction on first run (`android_assets_root`).
5. **Packaging: done** for phones (`tools/android/build_apk.sh`). Quest additionally needs the OpenXR loader in the APK and the Meta manifest entries.
6. **Steam Audio** ships an `aarch64-linux-android` build. `audionimbus-sys` would need to fetch it (it currently fetches desktop builds only).
7. **Performance tiers.**
   - Mobile tile GPUs pay for every full-screen pass. A "mobile" preset would:
     - drop the depth prepass
     - turn off SSR and SSGI
     - use baked lighting plus FluxVoxel
     - halve the bloom chain
   - Suggested order: 3, 5 and 4 first (it boots), then 1 and 2 (content), then 7.

---

## 3. Outstanding work (verified against the code on 2026-10-30)

These are things that are actually missing or partial. Everything else that older lists flagged is done:
- runtime UI, prefabs, particles, navmesh
- the save API, localization, the audio mixer
- multi-scene and seamless loading, occlusion culling
- the animation state machine

### Rendering
- **GPU-driven culling / indirect draws.** Instancing (1.3) batches consecutive identical draws, but culling and batching are still done on the CPU each pass. For very large object counts, the next step is GPU frustum/occlusion culling writing indirect draws.
- **Automatic LOD generation.** `LodComponent` switches between authored models by distance (with hysteresis and distance culling), but there is no mesh decimation at import to generate those levels.
- **Render-to-texture cameras: the basic path landed** (Features.md §2H: `camera:<id>` texture paths, Create Screen). Update Rate throttling and off-screen skipping landed too. Resizing a live target landed. Still open: screens inside screens, portals.
- **Reflection probes beyond two at once.** Multiple probes with per-pixel two-probe blending landed (Features.md). What remains:
  - more than two probes blending at the same pixel
  - (SSR, RT and custom shaders now blend both probes as well)
- **Projected decals.** The ZestEngine Toon decal is a texture layer, not a world-space decal projector.
- **Terrain.** There is no heightmap terrain, splat painting, or foliage scatter.
- **Remote humanoid IK.** `NetworkAvatar::targets` replicates the poses, but the skinning IK path only uses the global (local-player) targets. Remote players need per-avatar IK targets.

### Animation
- Runtime humanoid retargeting.
- Sub-state machines.
- A curve / tangent editor.
- Bone preview and recording for skinned models in edit mode.

### Networking
- **No NAT traversal, relay or host migration.** It is a star topology over UDP; players behind NAT need port forwarding.
- **No client-side prediction / reconciliation or lag compensation.** The owner simulates locally and remote peers interpolate.
- **No shared tick.** Sender-stamped clocks with offset estimation stand in for it.
- **No encryption or authentication.** LAN or trusted servers only.
- **UGC avatars are not networked yet.** The next step is replicating each player's avatar content id (plus its source URL) so peers load the same package. The VR Social template's networked avatar is the built-in fallback.

### User content
- **HTTPS downloads** need a game-provided fetcher: there is no TLS client in the dependency tree.
- **No logic in content.** The recommended path is a sandboxed WASM `Script` component (see `docs/user-content.md`); native plugins are never an option for untrusted content.
- **Freeing GPU resources** happens per scene, not per package. An unloaded prop keeps its meshes and textures until the next world switch.

### Editor / workflow
- **Undo coverage.** Transforms, renames and material edits are undoable; component edits and many tab-specific edits are not.
- **No visual shader graph.** Shaders are code plus inspector properties.
- **No asset cook step.** The build copies every content folder as is and precompiles shaders, but it doesn't strip unreferenced assets or pack into an archive.

### Platform
- See section 2: Android work and real Windows hardware.

### Suggested next steps (most value per effort)
1. GPU-driven culling with indirect draws.
2. Render-texture throttling and portals.
4. Networked UGC avatars plus remote humanoid IK. This completes the VRChat-style loop.
5. Android bring-up for Quest 3: Vulkan 1.3, APK asset I/O, ASTC, SPIR-V in packages.
6. Mesh decimation to generate LOD levels at import.
7. A NAT relay (or Steam networking) for internet play.
