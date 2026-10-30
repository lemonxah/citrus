# citrus — Features & Goals

Tracks engine capabilities: **what we already support** vs **what we still want to
build**. Working bug/backlog list lives in [TODO.md](TODO.md); high-level milestones
in [README.md](README.md). This file is the feature-level map. Update it when a
capability lands or a new goal is set.

Legend: `[done]` implemented · `[partial]` partial / needs validation · `[todo]` not started

---

## 1. Implemented features

### Rendering
- [done] Vulkan 1.3 renderer (ash 0.38, dynamic rendering, sync2)
- [done] PBR standard shader (metal/rough, base/normal/emission), multi-light frag loop:
  energy-conserving Cook-Torrance (Fresnel kS / diffuse kD = 1-F) + roughness-aware ambient
  **diffuse + specular** (indirect split so metals/smooth surfaces pick up environment colour
  instead of flat black; reflection probes are a follow-up); indirect term fed by baked lightmaps /
  probe SH (else flat ambient). Normals use the inverse-transpose model matrix (correct under
  non-uniform scale). **Two PBR variants share this pipeline (so both get GI + baked maps):**
  - **Standard** (Mochie-like): the full PBR path above.
  - **Toon** (Poiyomi-like, the `Toon Shading` material toggle / `FEAT_TOON` variant): a smooth
    quantized **cel ramp** (Ramp Steps + Toon Strength + Ramp Smoothness) over the same PBR base, a
    crisp toon specular, and a **Fresnel rim light** (Rim Strength / Rim Color / Rim Power). Still
    samples the same probe GI + baked lightmaps + shadows.
  - **12 texture slots** (set 1): albedo, normal, ORM, emission, **opacity**, **emission mask**,
    and **3 matcaps + 3 matcap masks**. Matcaps are view-space sphere-mapped, masked, and added
    with per-layer strength (FX UBO); opacity drives alpha; the emission mask gates the glow.
    `.material` files carry all slots (`MaterialTextures`, `#[serde(default)]` back-compatible).
    All 12 slots are assignable in the inspector, placed in the section they belong to. **Base**
    (albedo, normal map + strength, ORM, opacity), **Emission** (emission map, emission mask), and
    **Matcaps** (each of the 3 layers is its own sub-section: matcap texture, blend mode, strength,
    mask). Drag an image from the file browser onto a slot; per-slot clear. `apply_material` rebinds
    the material's descriptor set live (`Renderer::set_material_textures`, skipped when unchanged),
    resolving model paths (sRGB colour / linear data) with a fallback to import-embedded handles.
    Round-trips through `.material` files and inline scene materials (`MaterialRef::Inline { textures }`).
    Imported materials stay read-only until extracted.
  - **Extract textures & materials** (Model Import inspector, `⤓` button): re-loads the model file and
    writes its embedded textures as PNGs + materials as `.material` files into
    `<project>/extracted/<model>/{textures,materials}/`, with the `.material` texture slots pointing at
    the extracted PNGs (project-relative). Turns an imported FBX/glTF's inline assets into editable,
    reusable project files. (`EditorAction::ExtractModelAssets` → `extract_model_assets`.)
  - **Matcap blend modes**: each matcap layer combines with the shaded colour via Add / Multiply /
    Replace (`MatcapBlend`), carried in the FX UBO (`fx.matcap_blend`) and applied by `blend_matcap`
    in the shader. Per-layer strength + mask still apply.
  - **Per-texture UV transform**: Albedo / Normal / ORM / Emission each have independent Tiling
    (xy scale, default 1) + Offset (default 0), edited inline under each texture row in the
    inspector. Carried in the FX UBO (`fx.albedo_st`/`normal_st`/`orm_st`/`emission_st`, xy=tiling
    zw=offset); the shader builds a per-map UV `v_uv*tiling + offset (+ scroll)`. Opacity follows
    albedo's UV, emission mask follows emission's. Animated UV scroll stacks on top.
  - **EXR (OpenEXR) images** load as textures + file-browser thumbnails (`image` `exr` feature);
    recognised as images for thumbnails, the skybox action, and material slot drops. HDR values are
    currently clamped to LDR on load (full float pipeline is a follow-up). Compressions the Rust
    decoder lacks (DWAA/DWAB) fall back to transcoding via `oiiotool` (OpenImageIO) to a temp
    ZIP EXR; if `oiiotool` is absent it logs a hint to re-export with ZIP/PIZ.
    **EXR now loads natively as HDR float** — decoded to linear RGBA f16 and
    uploaded as `R16G16B16A16_SFLOAT` (`TextureData.hdr`), no LDR clamp and no PNG
    round-trip. DWAA/single-channel sources transcode to a temp lossless ZIP EXR
    (float) via `oiiotool`, never PNG.
  - **Displacement / parallax occlusion mapping**: a height map slot (15, binding 16) drives
    per-pixel **POM** in `standard.frag` — the tangent-space view ray marches the height field and
    every map samples the displaced UV, so flat geometry (e.g. a floor) gains apparent depth with
    no tessellation. `Displacement Scale` slider (gated to "map assigned"); branch-gated by
    `fx.parallax.x > 0` so it costs nothing when off. The shift is computed in albedo-tiled space
    and mapped back to mesh UV so per-map tiling stays coherent. Silhouettes stay flat (POM, not
    true geometry displacement — that would need tessellation).
  - **Split AO / Roughness / Metallic maps**: alongside the packed ORM, each PBR channel can take
    its own texture (slots 12/13/14, bindings 13-15), each with an **Invert** toggle (e.g. a
    smoothness map used as roughness). Split maps *multiply* their packed-ORM channel — the 1×1
    white default is a no-op, so a material can use packed ORM, separate maps, or a mix. They share
    the ORM tiling/offset. Invert is gated to "map assigned" (so it can't zero a channel via
    `1 - white`). Carried in `fx.orm_invert`; `MaterialTextures`/`MaterialTexturePaths` extended
    `#[serde(default)]` back-compatibly.
  - **Per-material FX uniform buffer** (set 1, binding 4): the 128-byte push block is full (and
    AMD-capped), so extended params live in a small per-material UBO instead: rim colour/power/
    strength, ramp smoothness, and **animated UV scroll + emission pulse** (Poiyomi-style). The
    UBO is host-visible and rewritten on edit (`Renderer::upload_material_fx`); materials are static
    at runtime so there's no in-flight hazard. `.material` files stay back-compatible (the new
    `MaterialParams` fields are `#[serde(default)]`). This enables richer built-in shader
    options without a custom shader.
- [done] **Custom-shader pipeline exposes the full scene**: the preamble now declares the complete
  frame UBO (lights array, shadows `u_shadow`, probe SH `probes`, baked `u_lightmap`, post settings)
  plus ready-to-call helpers (`citrus_gi(world_pos, n)` for probe-SH baked GI with ambient fallback,
  `citrus_direct_diffuse(world_pos, n)` for all scene lights with attenuation/spot cones, and
  `citrus_sh(...)`), so custom shaders integrate with lighting/GI the same way the built-ins do.
- [done] Lights: directional / point / spot (color, intensity, range, spot angle/blend,
  up to 16/frame, distance attenuation + spot cones)
- [partial] Shadow-casting lights (shadow-map array, PCF; needs acne/bias GPU validation).
  Per-light **Shadows** dropdown: No Shadows / Hard Shadows (single depth tap, sharp) /
  Soft Shadows (5x5 PCF penumbra). The filter mode rides the sign of the light's shadow
  view-count, so no extra GPU light field is needed.
- [done] **Baked soft shadows** via a per-light **Radius** (light source size): the bake
  aims each shadow ray at a random point on a disc of that radius, so shadows get a smooth
  penumbra that hides the texel stair-stepping at low texel density instead of a hard,
  jagged edge. 0 = hard. Realtime shadows are unaffected (they already PCF-soften).
- [done] Skybox: procedural gradient + equirect LDR image (per-scene)
- [done] Selection outline (inverted hull, depth-prepass, always-on-top)
- [done] Error shader (animated swirl for broken/missing/unknown shaders)
- [done] VSync toggle (FIFO / MAILBOX / IMMEDIATE)
- [done] **CPU frustum culling**: each `DrawCmd` carries an object-space bounding-sphere
  radius (`bound_radius`, from the mesh AABB); the renderer extracts the 6 frustum planes
  from `proj·view` (Gribb-Hartmann, 0..1 depth) and skips draws whose world sphere is fully
  outside. Scoped to the **main camera pass** and **per VR eye** (each eye culled against its
  own view/proj) — the shared draw order still feeds shadows + the reflection cube, which need
  off-screen geometry. Radius scales by the transform's largest axis (non-uniform scale); a
  safety net falls back to the full order if culling ever removed everything, so a bad matrix
  can't blank the viewport.
- [done] **Reflection-probe cube capture culling fix**: the cube capture renders with a
  positive-height (non-Y-flipped) viewport, which mirrors winding vs. the main passes, so the
  standard CCW/BACK pipelines were culling *front* faces (geometry captured inside-out — floors
  vanished, spheres showed their backs), placing reflections in the wrong spot. The capture now
  forces no-cull (`double_sided`) for all draws, matching the bake gbuffer pass.
- [done] **Layer system (Unity-style)**: 32 named layers, a per-object `layer`, a per-camera
  `culling_mask`, and a symmetric **layer collision matrix** (`LayerSettings` in citrus-core,
  serialized with the scene). **Physics:** colliders get rapier `InteractionGroups` (membership
  = layer bit, filter = matrix row) so "layer A ignores B" is honored by the solver.
  **Rendering:** `LoadedScene::visible_layers` masks draws — the editor viewport's per-layer
  toggle, or (in a shipped game) the active camera's culling mask. **UI:** Inspector layer
  dropdown, **Tools → Layers** window (rename layers, edit the collision matrix, toggle
  viewport visibility), and a Camera "Culling Mask" section. Unit-tested.
- [done] **Profiler window** (View → Profiler window): a *separate OS window* (own
  surface/swapchain/egui, sharing the GpuContext — `profiler_window.rs`) you can drag to
  another monitor (opens 2× wide), so stats never fill the viewport. Shows **one combined
  realtime graph** (20 s history, time-bucketed so it's stable at any frame rate; shared linear
  axis with value labels left+right and a time axis along the bottom; hover a line to highlight
  it, dim the rest, and read its value + age) with a **clickable legend** below to show/hide any
  series. Series: frame time / FPS; **GPU per-pass timestamp breakdown** — total GPU frame plus
  Flux GI, shadows, scene, reflections, post, egui, camera-preview (so you can see *which* pass
  dominates, not just total); GPU utilization %; per-phase CPU breakdown; and draw calls. Plus
  static draw/pipeline counts. **Zero per-frame cost when closed.** Note the CPU "realtime
  GI" timer only covers the trace's CPU prep; the actual march cost is GPU work (read via
  `VK_QUERY_TYPE_TIMESTAMP`, see `RenderStats`). **Zero per-frame cost when closed**: GPU
  timestamp queries + readback and all history bookkeeping are skipped unless the window is open.
- [done] Camera preview tab (renders scene from the main camera to an offscreen target). Renders linear HDR
  and runs the post pass (bloom, grading, tonemap, vignette) with the post profile resolved at the preview
  camera, so it matches the game.
- [done] Play mode: the Scene tab (renamed from Viewport) keeps the free editor camera while playing; the game
  shows in the Camera tab (gameplay pointer, screen rays, game UI and cursor lock follow that tab). The
  transport camera button makes the Scene view follow the game camera instead (off by default). Flying the
  Scene camera (right mouse) doesn't feed mouse/keys to the game.
- [done] **Startup splash** (`splash.rs`): a borderless CPU-drawn window (softbuffer) shown
  before the Vulkan renderer exists, with a scaled background + bottom status line (ab_glyph).
  Shows the embedded `assets/splash.png` **instantly** as a placeholder while the animated
  `splash.webp` (embedded `citrus-editor/assets/splash.webp`, or an override in the project
  root) is **decoded on a worker thread** (streamed frame-by-frame to bound memory) and
  swapped in, looped by elapsed time. The Vulkan renderer build also runs on a worker thread
  (it's `Send`; the scene with non-`Send` `dyn Component`s can't be, so it stays on main) so
  the splash animates during the build; the status line updates across later plugin/scene
  phases. Closed after the first editor frame. Plugin `cargo build` runs on its own worker in
  parallel with the renderer build (`PluginHost::build_all` / `load_built`); only the dylib
  load + registration stays on main.
- [done] **Background-task system** (`tasks.rs` + status bar): heavy CPU work runs on worker
  threads (parse off-thread, GPU-apply on main); the status bar gets a **Background Tasks**
  button with aggregate progress that expands to a per-task list with progress bars + **Cancel**,
  plus an auto-expiring **notifications** line. Wired tasks: **threaded model import**
  (`load_model_with_meta` on a worker → `add_asset_scene` on main); **chunked light bake**
  (`bake.rs` `BakeJob` begin/step/finish — one lightmap/probe-volume per frame on the main
  thread with live progress: lights, bounces, lightmaps, probe volumes, FluxVR; cancellable
  between units, partial result discarded); and **focus re-import** (FS scan on a worker behind
  a **blocking modal**, GPU reload on main). Imports that finish during a bake defer their GPU
  upload until it ends (the bake's TLAS embeds mesh device addresses).

### Assets & formats
- [done] glTF import (PBR factors + textures, lightmap UV via TEXCOORD_1)
- [done] FBX import via ufbx (per-material parts, PBR factors, base/normal/emission,
  embedded textures)
- [done] **Multi-material slots**: an imported mesh with N materials is one scene object
  with N material slots (not N objects). `SceneObject.extra_render` holds slots beyond the
  primary; `sync_draws` fans out one draw per slot, picking tests all slots, realtime GI
  gathers all slots' emission/albedo. The inspector shows a slot selector (when >1) that
  picks which slot to edit; drops/edits target the selected slot. Round-trips via
  `ObjectSource::Model::extra_meshes` + `SceneEntry::extra_materials` (both `#[serde(default)]`).
  Known gap: the baked-lightmap path still atlases slot 0 only (one layer per object); realtime
  GI covers every slot.
- [done] Procedural primitives (cube/sphere/capsule/plane)
- [done] `.material` files (RON): save/load/assign, per-path texture cache
- [done] **Off-thread material textures on scene load**: the loader thread decodes + uploads
  every material texture a scene references (via the transfer-queue `Uploader`), keyed by
  (abs path, srgb), so the main thread only installs handles — the heavy 4K EXR/PNG/JPG decode
  no longer freezes the splash. `LoadedScene::collect_material_texture_refs` gathers the refs
  (file + inline materials); single-queue GPUs fall back to a synchronous main-thread decode.
- [done] **BC texture import cache** (`citrus-assets/tex_cache.rs`, `intel_tex_2`): each source
  image is decoded, mip-chained, and block-compressed **once** (BC7 sRGB for colour, BC7 UNORM
  for data maps, BC6H for HDR/EXR), persisted to a sibling `.citrus_texcache/` keyed by
  (filename, srgb) and invalidated on source mtime/size change. Later loads skip decode +
  recompress entirely and upload the compressed mips straight from cache (`upload_compressed`),
  cutting load time and VRAM. Non-multiple-of-4 sources and GPUs without `textureCompressionBC`
  fall back to an uncompressed-but-cached path. The `textureCompressionBC` device feature is
  enabled when reported.
- [done] `.scene` files: full save/load (objects, transforms, parents, components, materials)
- [done] `project.citrus` (RON): project name, last scene, per-project settings
- [done] `.lightmap` / `.lightdata` sidecars (baked GI + probe SH)
- [done] Lightmap UVs (second UV set, generated atlas fallback)
- [done] Audio clips: `.wav` / `.flac` / `.mp3` (rodio)

### Lighting / baking
- [partial] GPU light bake (Vulkan ray query): BLAS/TLAS, lightmap path tracer (direct +
  shadow + multi-bounce indirect), light-probe SH-L1 bake. Built, needs GPU visual
  validation. **Runtime sampling**: 5a flat probe-average ambient (done); **5b
  per-fragment probe SH** (done): probes uploaded to a set-0 storage buffer (binding 2)
  + volume metadata in the frame UBO; `standard.frag` finds the containing volume,
  trilinearly blends 8 probes, evaluates SH-L1 in the surface normal, and uses it as the
  indirect term (flat ambient fallback outside any volume). Active in **both** the editor
  viewport and the **game runtime**: `run_game` loads the scene's `.lightmap`/`.lightdata`
  sidecars (shared `LoadedScene::load_bake_sidecars`) and uploads the probes. The sidecars
  bundle automatically (they live in `scenes/`, copied with the scene). **5c
  per-object lightmaps (done)**: baked lightmaps upload as a `R32G32B32A32_SFLOAT` 2D array
  (one layer per static object, resampled to a common size; set-0 binding 3), `uv1` is
  forwarded through the vertex pipeline, and `standard.frag` samples the layer (per-object
  index in the push constant) for static-object GI. Lightmap takes priority, else probes,
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
- [done] Primitive lightmap UVs: a second UV set (`uv1`). Plane/sphere/capsule reuse their
  single non-overlapping `uv0` chart; the **cube packs its 6 faces into a non-overlapping
  3×2 atlas** (with a gutter) so faces don't share lightmap texels. Imported meshes use
  their 2nd UV set, or `uv0` (glTF) / a planar unwrap (FBX) when absent.
- [done] **Bake denoise + seam fix**: after the path trace, each lightmap is run through
  an edge-aware **À-Trous denoiser** (CPU; reads back the position+normal gbuffer, weights
  a 5×5 wavelet blur by world-position + normal similarity so it smooths MC grain without
  crossing shadow/geometry edges or UV-chart seams), then **seam-stitched** (co-located
  cross-chart texels (the cube's per-face atlas, the sphere's lat-long meridian) are
  averaged when their normals agree, so the chart boundary stops showing as a line while
  genuine hard-edge discontinuities are kept), then **dilated** (valid texels spread into
  the gutter) so bilinear sampling at chart edges never reads the black background.
- [done] **ACES tonemapping**: the standard + skybox shaders roll HDR highlights off to
  [0,1] (Narkowicz ACES, in linear before the sRGB swapchain write), so a close point light
  or stacked ambient + baked bounce shows surface detail rather than clipping to flat white.
- [done] **Realtime GI**: an Environment-tab setting (serializes with the scene, so it runs
  in the editor *and* a shipped game) that, while the scene isn't baked, re-traces an auto
  probe grid from the realtime lights (reuses the ray-query path tracer with `probes_only`),
  temporally blends the SH, and uploads so surfaces show live indirect bounce. Settings:
  Enabled, Bounces, Quality (rays/probe), Intensity, Probe Spacing, Responsiveness (temporal
  blend), Update Interval. Driven by a shared `RealtimeGiState` with **dirty-detection**:
  it only re-traces when lights/objects/settings change, then settles and goes idle, so a
  static scene does no work. The **Software (SDF) march runs on a background thread**, so
  moving objects (Play mode) don't hitch the frame; the main thread blends + uploads when a
  trace finishes. Hardware (ray-query) mode is still synchronous + rebuilds accel structures
  each trace; GPU async / resident-accel is the follow-up.
- [done] Approximate **lumens/lux readout** under a light's Intensity (display only; our
  intensity stays a radiance multiplier; point = 4π·I, spot = cone solid angle ·I, dir = lux).
- [wip] **Software GI (Lumen-style, no RT cores / no bake)**: a second realtime-GI **Mode**
  (Environment tab → Hardware (RT cores) | Software (SDF)) that marches per-mesh signed
  distance fields instead of the hardware BVH. Phase 1a (mode setting + UI) and 1b (per-mesh
  CPU SDF generation: `sdf::generate_sdf`, closest-point-on-triangle distance + nearest-tri
  normal sign, unit-tested) are done. Phase 1c is a CPU **multi-bounce** path march
  (`sw_gi.rs`, honors the Bounces setting, throughput ×albedo per hop) reusing the SDFs,
  **parallelized across cores** on a **background thread** (now the CPU fallback). The default
  path is a **GPU compute march** (`sw_gi.comp` + `gpu_gi.rs`): the per-mesh SDFs are merged
  CPU-side into one **Global Distance Field** (`sw_gi::build_gdf` → a 3D distance texture +
  nearest-instance index texture), and a compute shader marches that single field per probe (one
  texture sample per step instead of looping meshes), writing the packed probe layout directly.
  This is far cheaper than the CPU march, so it runs synchronously per re-trace. The GDF is **cached on the
  GPU** (`Renderer::gi_set_gdf`) and re-uploaded only when a geometry/materials/bounds hash
  (`hash_gdf_inputs`) changes, so a static scene keeps a high-res field for free while lights and
  emitters move; `Renderer::gi_march` then runs each trace against the cached field.
  `gi_gpu_available()` gates the whole path: when compute init failed it returns false and the
  driver builds nothing, marching on the CPU thread instead. Each fresh trace is **spatially denoised**
  first: a separable [1,2,1] blur over the
  probe SH grid (`sw_gi::blur_probe_grid`) that cancels the blotchy per-probe Monte-Carlo
  variance with **no temporal lag**, so Responsiveness can run high (snappy updates to moving
  objects) without trading back into noise. The denoised trace is then blended in with a
  **motion-aware EMA**: while a light/emitter is *moving* it snaps toward the latest trace (rate
  = Responsiveness) so the bounce tracks in realtime; when *static* it averages at a fixed gentle
  rate so residual per-trace variance converges smoothly, so raising Responsiveness never makes
  a still scene flicker. A short per-frame ease (faster while moving) glides between updates (cheap
  in-place SSBO rewrite `update_probe_sh`, no GPU stall). The probe grid is **cascaded**
  (SDFGI-style): the coarsest volume covers the whole padded scene AABB and each finer cascade
  halves the box (doubling density) around the same center, up to 3 cascades (16/axis each for
  software, single 32 grid for hardware). The shader picks the finest cascade containing a
  fragment and **cross-fades into the next coarser one near its boundary** (`sample_volume` +
  edge fade in `standard.frag`) so the resolution change isn't a visible seam. This is what
  removes the trilinear "squares" near the action while keeping edges/sky cheap. The 8-corner
  blend also **smoothsteps the trilinear factor** (Hermite, C1-continuous across cell boundaries),
  so the per-cell gradient kink that reads as faceting/banding on smooth falloff is gone without a
  finer grid. All cascades are
  concentric (scene-centered) so the cross-fade lines up and the GI doesn't shift with the camera.
  Each cascade is
  blurred by its own grid layout. **DDGI-style visibility (leak prevention)**: the march also
  records the SH-L1 of the directional first-hit distance per probe (`ProbeSh::dist`, packed into
  the probe SSBO's previously-unused `.w` lanes, no extra buffer). The shader replaces plain
  trilinear with a DDGI-style weighted 8-corner blend (`sample_volume` in `standard.frag`): each
  probe's weight is scaled by a soft Chebyshev-lite visibility test (probe→fragment distance vs.
  the probe's stored seen-distance in that direction) plus a front-facing term, so probes
  occluded from a fragment (behind a wall, under an object) are down-weighted, so light no longer
  leaks. Bake/hardware paths leave `dist` zero, which disables the test (plain trilinear). **4
  bounces** cap so a CPU trace finishes inside the update interval; probe-spacing floor 0.25 m so
  a tiny value can't silently explode the probe count. **Emissive
  materials are area emitters** in both bake (static objects) + realtime GI, sampled by
  **next-event estimation (NEE)**: each emissive instance is reduced to a sphere area-light
  (`sw_gi::emitter_spheres`) the march samples *directly* (both the probe's direct view, added
  analytically to the SH, and at every bounce surface) instead of relying on random rays to hit a
  small bright surface. This removes the blotchy Monte-Carlo fill that otherwise rings an emitter
  (the dominant direct term is variance-free in a single trace; the dim indirect residual is cleaned
  by the temporal accumulation + grid blur). Implemented in both the CPU march and the GPU
  `sw_gi.comp` (emitter SSBO, binding 6). The headless `cargo run --example gi_preview` renders a
  minimal plane+emissive-sphere scene through the real march to a PNG for tuning without the editor.
  Next: surface cache
  / screen probes for contact-scale GI. Probe GI is low-frequency, so tight contact fill
  (under-object darkening) is still limited.
- [wip] **Screen-space probe GI (Lumen-style realtime)**: a per-pixel-resolution dynamic GI
  path layered over the world probes for contact-scale, view-dependent indirect. A depth
  prepass (`screen_gi_pass`, shadow pipeline → sampleable `sgi.depth`) feeds a compute trace
  (`screen_gi.comp`): one screen probe per `SCREEN_PROBE_DIV²` (4×4) block reconstructs world
  pos + normal from depth, cosine-samples the hemisphere against the cached **GDF**, and adds
  the analytic emitter **NEE** pool. Results **temporally accumulate with reprojection**
  (ping-pong image pair, prev-frame view-proj + camera-distance disocclusion) and are
  **depth-aware bilaterally upsampled** in `standard.frag` (binding 4). Emissive objects are
  **excluded from the GDF entirely** (static or not): their light comes from the variance-free
  NEE spheres, while their coarse padded SDF box in the field only stamped a square halo +
  voxel banding on the bounce; keeping them out makes a static emitter look identical to a
  dynamic one. The trace RNG is **seeded per frame** (`sgi_frame` → `misc.x`) so each frame
  samples different directions and temporal accumulation actually converges the Monte-Carlo
  noise (a fixed `0` seed produced un-averageable fixed-pattern blotches). Trace cost tuned to
  10 samples / 2 bounces / 96-step march cap, leaning on temporal accumulation for smoothness.
  The depth prepass + compute trace are **folded into the main frame command buffer** (one
  submit, no per-frame CPU fence stalls): the trace's transient descriptor pool + host buffers
  are returned to the renderer and freed one frame later, after that frame's fence signals
  (`ScreenGiTransient`, `sgi_transients[frame_index]`). **Transparent** surfaces are kept out of
  the depth prepass (a glass pixel would otherwise trace GI on the glass → milky fill); emissive
  surfaces ARE in it, so they sample their own GI and receive bounce additively on top of their
  emission. Probe normals are reconstructed from the depth neighbour CLOSER in depth per axis,
  so the tangent never spans a silhouette (which left a dark no-GI outline around objects).
  **Emitter NEE falloff**: a soft cosine terminator (`dot(n,l)+0.3` clamped). Emission fades
  smoothly past the horizon but a surface facing clearly away gets nothing (no bleed onto a
  box's back face). NO occlusion test on the direct emitter NEE: GDF-based occlusion blotches
  against the coarse field, and a screen-space occlusion trace false-shadowed whenever the
  emitter was on-screen (the ray grazes the receiver floor); both looked worse than the leak.
  The hemisphere bounce still occludes via the GDF march. (A solid object can leak an emitter's
  *direct* glow; the proper fix is the surface/radiance cache below, where occlusion is intrinsic
  to the trace.)
  Next: surface/radiance cache for cheap multi-bounce + intrinsic occlusion; masked depth-prepass
  for alpha-test cutout holes; per-camera trace for the in-game camera.
- [wip] **Screen-space reflections (SSR)** — now **deferred, current-frame** (no 1-frame lag).
  The forward pass writes a thin G-buffer (a second MRT attachment, `RGBA16F`: specular
  reflectance.rgb = Fresnel·ao·spec, roughness.a) via opt-in `gbuf` pipeline variants
  (`PipelineKey::gbuf`), and keeps the env-cube reflection composited in `color` (so it stays
  correctly fogged and is the SSR-miss fallback). A fullscreen **resolve pass** (`ssr.rs` /
  `ssr_resolve.frag`, modelled on the post pass) then runs after the forward pass: it reconstructs
  view position + geometric normal from the Flux depth prepass, re-derives the env radiance the
  forward pass added (same cube + box-projection), marches the reflected ray against the **current**
  frame's HDR colour (McGuire screen-space march: fixed ~2px stride, perspective-correct 1/z,
  sign-change hit + 8-step binary refine + residual confidence fade), and outputs
  `scene + reflectance·conf·(ssr − env)` into a resolved HDR target the post pass tonemaps. So a
  confident hit cleanly swaps the env reflection for the live screen reflection and a miss leaves the
  forward result untouched — killing the old last-frame lag/swim and LDR source. Wired on both the
  game swapchain path (`self.ssr_targets`, per frame-in-flight) and the editor viewport
  (`self.viewport_ssr`). Tunable from **Environment → Flux GI → Reflections (SSR)** (enable,
  intensity, max distance, roughness cutoff), serialized in `RealtimeGi`. Limits: on-screen geometry
  only (off-screen rays fall back to the env cube), geometric (depth-reconstructed) normals so
  normal-mapped reflectors lose bump detail in the reflection, and it rides on the Flux depth prepass
  so it's active only while realtime GI is on. Off-screen GDF fallback, temporal accumulation and a
  roughness blur are the planned follow-ups.
- [done] **Reflection env-cube capture, chunked one face/frame**: the scene reflection cubemap
  (rendered from a probe centre into the env cube) is captured **one cube face per frame** plus a
  final mip-gen/swap step (`ReflCaptureJob`, `begin/step/finish_refl_capture`), instead of all 6
  faces + first-time pipeline compile in a single frame. The editor opens immediately and the
  reflection fills in over ~7 frames with no stall. The old env cube stays sampled until the swap,
  so no half-built cube is ever shown.
- [done] **Reflection Probe object + baking UI**: add a `ReflectionProbe` to the scene from the
  hierarchy context menu (**Light → Reflection Probe**); its world position is the capture centre,
  `size`×`scale` the box influence (box-projection optional). **FluxBaker** has a **✨ Bake
  Reflections** button (capture + write the `.reflprobe` sidecar, loaded on scene open) and a
  **Recapture** (session-only); both disabled until the scene has a probe. Same actions still live
  in the Environment panel.
- [done] **Reflection capture lighting fix (FluxVoxel)**: `gather_lights_impl` grew a
  `flux_voxel_skip` arg so the "drop Realtime/Mixed lights in FluxVoxel mode" filter applies
  ONLY to the forward pass, **not** to the reflection-probe capture (`gather_lights_all`). The
  cube is a separate forward render with no voxel volume, so it must direct-light the scene —
  previously in FluxVoxel mode it lost every light and reflections of geometry came out black
  (only the skybox + self-emissive survived). This was the actual cause of the "black scene in
  reflections" reports.
- [done] **Reflection capture descriptor-set fix (stale/dangling bindings)**: the dedicated
  `refl_capture_set` was written once at renderer init and never maintained, so every later
  rebind (lightmap array load after a bake, `swap_reflection_env`, probe-SH buffer replace,
  shadow-atlas resize) left the capture pass sampling the 1×1 **black** placeholder lightmap or a
  **destroyed** view. In a baked scene the captured cube showed only emissive surfaces (couch /
  walls / floor black) — reflections looked like "not a reflection". All rebind sites now go
  through `shared_binding_sets()` (frame sets + camera preview + capture set). Also fixed the
  **Bake Reflections readback race**: the readback waited a fixed 2 frames but the capture takes
  ~7 (one face/frame), so it persisted the PRE-capture cube to `.reflprobe`; it now waits for
  `reflection_capture_in_flight()` to clear before the countdown starts.
- [done] **Reflection-probe Box Offset (Unity parity)**: `ReflectionProbe.box_offset` decouples
  the parallax box from the capture point — box center = object position + offset, capture stays
  at the object position (Lagarde: "the box center doesn't need to match the cubemap center").
  Author the probe OBJECT at eye height (so interior objects — a helmet on a stand — occupy the
  cube directions eye-level reflection rays actually sample) and offset the box up so it still
  hugs floor/ceiling/walls. Plumbed as `refl_box` through the frame UBO, SSR-resolve, and RT
  uniforms + all three shaders' box projection; inspector "Box Offset" row; the box gizmo draws
  at the offset. Also: flush-with-box surfaces (wall mirrors, the floor) now pass the inside-box
  test (2% tolerance + backward-hit clamp), matching across standard/SSR/RT paths.
- [done] **Planar reflections (true mirrors)**: add a **Planar Reflection** component to any flat
  drawn surface (floor, wall-mirror quad); the scene re-renders each frame from the main camera
  mirrored about the object's world +Y plane (Householder reflect × view), with a Lengyel
  **oblique near-plane** projection ([0,1] depth) clipping everything behind the mirror, into a
  half-res RT array (set 0, **binding 8**, up to `MAX_PLANAR`=4 planes/frame, recorded in the
  frame cb right after shadows). Same negative-height viewport as the main pass, so the forward
  shader composites by **screen UV** (`gl_FragCoord / screen`) — exact per-pixel parallax. The
  per-draw layer + roughness-fade cutoff ride the free `emission.w` push lane (layer+1 integer,
  cutoff fraction); `frame.refl_box.w` = plane count gates sampling so the capture/preview/VR/
  planar passes themselves fall back to the probe (no recursion; VR eyes zero it explicitly).
  The plane's own draws are excluded from its pass; mirrored winding handled by force-no-cull.
  Smooth surfaces get the mirror, rougher ones fade back to the probe (`max_roughness` slider).
- [done] **Reflection-probe "Lock box in world"**: with the toggle on, `box_offset`
  auto-compensates for object movement each frame (`sync_draws_multi`), so dragging the probe
  repositions ONLY the capture point inside the room-aligned box; the box gizmo stays put.
  Align the box first, then lock it and place the capture point freely.
- [done] **Realtime probes refresh continuously + Baked probes capture static-only**: a
  Realtime-mode probe now re-requests its capture whenever the previous one lands (one face per
  frame → fresh cube every ~7 frames, editor and game runtime both), so dynamic lighting (an
  emissive orb's FluxVoxel glow, moved objects) tracks in reflections instead of freezing at the
  scene-load snapshot; paused while a manual bake awaits readback. **Bake Reflections** now uses
  `request_reflection_capture_static`: only `static_geometry` draws render into the cube and the
  capture shader drops the realtime probe-GI term (`debug.z = -1`), so dynamic objects, their
  emission, and their GI bounce never persist into the `.reflprobe` (Unity baked-probe
  semantics). Known gap: dynamic casters can still shadow the static capture via the shared
  shadow atlas.
- [done] **Realtime probe Update Rate, whole-cube captures, moving objects excluded**:
  - **Update Rate** (Hz, default 30, 0 = every frame) replaces the fixed 0.2 s pacing that made realtime reflections step about 4 times a second.
  - All six faces and the mip chain now render in the frame a capture starts, each face with its own UBO and descriptor set. **Time Slicing** brings back one face per frame.
  - **Moving Objects** (off by default): non-static objects stay out of realtime captures, with lighting still live. Box projection paints everything in a cube onto the box walls, so an orb inside the room showed up as a ghost on the far floor edge. SSR and ray-traced reflections show moving objects instead.
- [done] **Reflection-probe math reference + debug views**: `REFLECTION_PROBES_MATH.md` documents
  the full cubemap/parallax/split-sum math (Vulkan face table, `R=reflect(-V,N)`, Lagarde box
  projection, Karis split-sum, Vulkan capture-orientation pitfalls, a bug→symptom checklist) with
  primary sources, and cross-references each piece to our code. Verified the whole pipeline matches
  the reference (face_dir = Vulkan spec, capture position = box center = correct Lagarde,
  `box_projection` default true, env_brdf_approx = Karis). Added two viewport **render modes**:
  **4 = Reflection cube (mirror)** (every surface a perfect mirror of `u_env`, reveals cube
  orientation) and **5 = Reflection vector** (R as RGB), per the reference's recommended debug
  visualizations — use them to empirically confirm orientation instead of guessing.
- [done] **Reflection-probe fixes from the math doc** (2026-06-18): (1) **Orientation proven**
  by an executable round-trip test (`refl_capture_tests::cube_capture_matches_sampling`) that
  simulates the GPU capture (look_at_rh + perspective_rh + positive viewport) for all 6 faces ×
  7 sample points and asserts it equals the `face_dir` sampler convention — capture faces
  extracted to `CUBE_CAPTURE_FACES` as the single source of truth. (2) **Roughness mip fix**
  (§6.5): `ENV_MAX_LOD` was hardcoded to the 64px skybox cube's mip count, so 256px captured
  probe cubes under-blurred (rough reflections too sharp — a defined blob on a rough floor); now
  `textureQueryLevels(u_env)`-driven in `standard.frag` + `ssr_resolve.frag`, correct for any
  cube size. (3) **Box-projection coverage guard** (§6.4): parallax only applies to surfaces
  INSIDE the probe box; a surface outside it (floor extending past the probe) falls back to the
  infinite-cube direction instead of a mis-placed parallax hit.
- [done] **FluxVoxel modular settings + coverage** (FLUXVOXEL_TODO §A): `GiMode::uses_{bounces,
  quality,samples,gdf,march_distance,voxel_density,ddgi_occlusion}()` declare per-backend
  capabilities; the Environment → Flux GI panel renders only the controls the chosen backend
  uses (FluxVoxel hides bounces/quality/GDF/march, shows **Voxel density**, **Auto scene grid**,
  **DDGI occlusion** instead). `RealtimeGi.voxel_density` (probes/m) drives the auto scene-covering
  grid built when no FluxVolumes are placed; `voxel_auto_grid` gates it.
- [done] **FluxVoxel DDGI-style occlusion** (FLUXVOXEL_TODO §B, toggleable): voxel lights are
  softly blocked by geometry via a coarse scene **occupancy bit-grid** (`sw_gi::SceneOccupancy`,
  mesh-AABB rasterized; `inject_light_occ` DDA-marches light→probe, soft transmittance with a
  floor so the coarse grid never hard-blacks). Toggle **`voxel_ddgi_occlusion`** (Environment →
  "DDGI occlusion", default **on**; off saves the per-probe marches on low-end/VR). Unit-tested
  (`occupancy_blocks_through_wall_not_around`, `occluded_inject_darkens_behind_wall`). NOTE: this
  is the occupancy-DDA approximation, not yet true per-probe distance moments + Chebyshev — that's
  the documented quality follow-up. Needs visual verification.
- [done] **FluxVoxel propagation, specular, relocation, emissive fidelity** (FLUXVOXEL_TODO
  B-F): (1) **LPV propagation** (`sw_gi::flux_propagate`) spreads injected radiance through the
  grid for a >=1 diffuse bounce, per-frame per-volume, toggle `voxel_propagation`. (2) **Specular
  from the volume** (`standard.frag::volume_radiance` + `sh_radiance`) - metallic/rough surfaces
  sample the probe volume in the reflection dir for emissive/voxel-light bounce (weighted by
  roughness so mirrors keep the cube), toggle `voxel_specular` via `FrameInput`->`fog_params.w`.
  (3) **Probe relocation + classification** (`sw_gi::relocate_probes` + `fluxvr_active` mask) -
  nudge probes out of solid, zero fully-trapped ones. (4) **Multi-point emissive** + **principled
  range** in `scene.flux_voxel_lights` - elongated emitters scatter K points along their long axis
  (energy split), range from a luminance cutoff. All toggleable, default on; CPU pieces
  unit-tested (`propagation_spreads_and_stays_bounded`, `relocation_pushes_probe_out_of_solid`).
  Shader paths need visual verification.
- [done] **FluxVoxel per-frame stall fix** (perf): the per-frame probe upload was going through
  `Renderer::set_baked_probes`, which does a `device_wait_idle` + buffer realloc + descriptor
  rewrite on every call — a full GPU stall each frame the emitters moved (cost scaling with probe
  count, so only the grid density appeared to affect FPS). It now uses the in-place
  `update_probe_sh` (no stall/realloc) on the per-frame path, falling back to `set_baked_probes`
  only on a rebuild or probe-count change — the same cheap path Flux/FluxRT already used. Removes
  the FluxVoxel-was-slower-than-FluxRT paradox.
- [done] **FluxVoxel auto-grid modes + mutual exclusion** (`VoxelGridMode`): the auto grid is now
  **mutually exclusive** with author-placed FluxVolumes (placing any volume turns it off, so the
  global density slider stops perturbing — and dropping the bake of — the placed region). Four
  selectable layouts (Environment → Grid mode, shown when Auto scene grid is on): **Whole scene**
  (one padded box, current behaviour), **Camera clipmap** (a fixed `2·extent` cube that follows
  the camera, snapped to the probe spacing → constant probe count regardless of level size; the
  camera comes from `Renderer::last_view_pos`), **Occupancy culled** (tight bounds around
  geometry), **Per object** (tight grids clustered onto object AABBs, capped at `MAX_AUTO_VOLUMES`
  = the shader's 4-volume limit via `cluster_aabbs`, unit-tested). `voxel_clipmap_extent` controls
  the clipmap half-size. NOTE: Occupancy currently only tightens the bounds; dropping individual
  empty probes needs sparse shader indexing (follow-up).
- [done] **Full `fluxvr` → `flux_voxel` / `FluxVoxel` rename** (no backwards compat): all
  lowercase `fluxvr_*` identifiers → `flux_voxel_*`; the `FluxVrLight` component type →
  `FluxVoxelLight`; comments/labels `Flux VR`/`FluxVR` → `FluxVoxel`. Removed the legacy
  GiMode serde aliases (`Hardware`/`RayQuery`/`Software`/`FluxVR`) and the `ComponentRegistry::load`
  "Flux VR Light" → "FluxVoxel Light" migration map — old scene/bake files are no longer
  supported by design.
- [todo] Lower-distortion primitive lightmap unwrap (octahedral sphere instead of lat-long;
  even cube-face packing). The seam-stitch hides the seams but the lat-long sphere still
  wastes texels at the poles.
- [done] Baker's Man dock tab (texel density 1–1024 /m log, bounces, samples up to 65536,
  max size, Bake/Clear) + a **UV-checker preview** toggle: renders objects as a
  lightmap-UV checkerboard whose cell size tracks each object's would-be texel density
  (big squares = low resolution, stretched = UV distortion; grey = non-static), live from
  the current bake settings, no re-bake needed.
- [done] **Bake / startup CPU niceness** (responsive editor + live splash): the CPU path tracer
  (`sw_gi::march_probes`), GDF build (`build_gdf`), and the parallel texture import/decode (runs
  on the scene-load worker during startup) no longer fan out to *every* core at normal priority
  (which pinned the whole machine to a standstill and starved the splash animation).
  `sw_gi::bake_worker_count` caps workers at **80% of cores** (always ≥1 core free for the UI/OS),
  and `lower_compute_priority` drops each worker thread's Linux nice to +10 so interactive threads
  always win the CPU under contention. The nice path is cfg-gated to Linux + the `editor` feature
  (which links libc); a shipped game compiles it out and never bakes.
- [done] **GPU bake tiling** (no more whole-desktop freeze while baking): the lightmap path-trace
  was one `cmd_dispatch(groups, groups)` per object — a single multi-second GPU job that
  monopolized the GPU so the OS compositor couldn't run, freezing the entire desktop (a long
  dispatch can't be preempted mid-flight on consumer GPUs). It now dispatches in horizontal row
  bands (`LIGHTMAP_BAND_GROUPS` = 8 workgroups = 64 rows), each its own submission via
  `cmd_dispatch_base` (pipeline created with `DISPATCH_BASE`; `gl_GlobalInvocationID.y` carries the
  band origin, so no shader/push change). The probe bake is tiled the same way
  (`PROBE_CHUNK_GROUPS`) since a dense FluxVoxel auto-grid can be tens of thousands of probes.
  Output is identical (tiles cover the same grid). Because tiling alone still kept the GPU ~100%
  busy back-to-back (the machine still stalled), each heavy submission also goes through
  `throttled_submit`, which idles the GPU for a fraction of the time the submission took —
  genuinely freeing the GPU for the compositor. The fraction is the **GPU Throttle** bake setting
  (`BakeSettings.gpu_throttle`, default 1.0 ≈ 50% duty / ~2× slower; slider 0–2 in the Baker tab):
  0 = fastest bake (hogs the GPU), higher = more responsive desktop. Threaded through
  `BakeInput.gpu_idle_frac`; the realtime-GI preview passes **0** so it stays fast (it reuses the
  same GPU bake in `probes_only` mode — without this it would have been throttled too).

- [done] **Realtime/Mixed lights in FluxVoxel** (correct, no double-count): Realtime/Mixed
  `LightComponent` lights now direct-light the forward pass in FluxVoxel mode (they used to be
  dropped, assuming the voxel grid covered them — which broke once the auto-grid became mutually
  exclusive with placed volumes), and are **no longer injected into the voxel grid** (the grid
  carries only emissive + dedicated `FluxVoxelLight` volume lights). Previously a Mixed light was
  triple-counted — lightmap + forward + grid — which read as "too much light". Mixed semantics are
  unchanged and correct: with no bake it's a full realtime light; with a lightmap its static
  contribution is baked and it direct-lights only the non-static (non-lightmapped) objects (the
  `gather_lights` baked flag + the shader's `light_baked && has_lightmap` skip).
- [done] **Forward light cap 16 → 128** (`MAX_LIGHTS`): lights beyond 16 silently didn't render.
  Raised to 128 (frame-UBO bound; a `const _` assert keeps `FrameUbo` ≤ 16 KB). Truly unbounded
  would need a lights storage buffer (the screen-GI path already uses one). Only `standard.frag`
  uses this UBO array, so the change is self-contained.
- [done] **Duplicate naming `name(N)`**: duplicating an object now yields `file`, `file(1)`,
  `file(2)`… (smallest free N, stripping any existing `(N)` first) instead of the stacking
  `Copy Copy Copy` (`scene::strip_dup_suffix` / `next_duplicate_name`, unit-tested).

- [done] **Texture tiling fix** (albedo + normal/ORM/emission): `apply_material` (the live
  material-update path) copied every param from the model except the per-map UV tiling/offset, so
  editing or reapplying a material silently reset `albedo_tiling`/etc. to 1×/0 — tiling "didn't
  work". Now copied through to `MaterialParams` → `MaterialFx.albedo_st`. The sampler was already
  REPEAT and the shader already applied `uv * st.xy + st.zw`.
- [done] **Sharper lightmap defaults**: bake defaults raised so lightmaps aren't pixelated/blocky
  out of the box — texel density 16 → 40 /m, Max Lightmap 512 → 2048, samples 256 → 512; the Max
  Lightmap dropdown gains a 4096 option. (Existing scenes keep their saved settings — raise them in
  the Baker tab.) Caveat: the lightmap texture array still pads every layer to the largest size, so
  high res × many static objects is VRAM-heavy until the per-object atlas lands.

### Post-processing
- [partial] **Unity Volume-style post-processing**: a `.postfx` profile asset (RON: tonemap
  mode/exposure, bloom, color grading, vignette, chromatic aberration) created from the file
  browser's New Post FX Profile; a **Volume (Post FX)** component (global/local, priority,
  weight, blend distance, box extents) references one; `LoadedScene::effective_postfx` blends
  the volumes affecting the camera (priority-ordered, weight × local proximity) into one
  profile fed to the shaders via the frame UBO. **Applied now (per-pixel, in standard+skybox
  shaders): exposure, tonemap (None/Reinhard/ACES), color grading (exposure/contrast/
  saturation/temperature/tint), vignette.** ACES moved out of hardcode into the profile.
  An in-editor **`.postfx` profile editor** (select the asset → sliders for tonemap, color
  grading, vignette, bloom, chromatic aberration; saves + live-invalidates the cache).
  [done] Volumes override per effect (Unity-style): an effect a Volume profile leaves disabled no longer
  replaces the scene's global post settings (Environment > Post), and tonemap / exposure only override when
  the profile's Tonemap box is ticked (`Tonemap::enabled`, off for existing profiles). Previously any global
  volume at weight 1 replaced every global setting, including the enable toggles, so a profile with bloom off
  silently disabled the Environment's bloom, grading and exposure. The global settings are now the base
  (`blend_over`); an effect enabled only in a volume fades in from neutral by weight.
- [done] Hierarchy Add menu and Object menu have a Volume submenu (Post FX Volume (Box), Post FX Volume
  (Global), Light Probe Volume, Reflection Probe, Flux Volume). A new Post FX Volume gets its own
  `postfx/Post FX Volume.postfx` profile (all effects off) so it works as soon as an effect is enabled.
- [done] Post FX Volume editor support: its own hierarchy icon + type colour (indigo), a viewport billboard
  (own toggle + size in the billboard widgets menu), and for box volumes a resizable box gizmo like the
  reflection probe's, with an outer shell at the blend distance (where the effect starts fading in). The
  Environment > Post panel lists which volumes currently override which effects. Box weighting now uses the
  object's full transform (rotated / scaled box, world-unit fade), matching what the gizmo draws.
  [done] Bloom is a mip-chain (`bloom.comp`, `bloom.rs`): prefilter (13-tap + Karis average, exposure,
  soft-knee threshold) into half resolution, downsample to the bottom level, then tent-upsample back
  blending each level by the profile's radius (scatter). The post pass adds level 0 x intensity. Each
  post target (game swapchain, editor Scene view, camera preview) owns a chain. Replaces the old 48-tap
  ring average, which was dominated by dark taps and showed almost no glow from small emitters.
  The editor Scene view's skybox was inline-tonemapped into the HDR target (double tonemap vs the game);
  it now renders linear like the game path.

### Editor
- [done] Play / Pause / Stop. Pause freezes components/physics/audio on a play clock that
  doesn't advance while paused (so time-based motion doesn't jump on resume)
- [done] Selected-camera preview overlay (bottom-right of the viewport): live view through a
  selected camera object so its framing can be tweaked while editing its transform
- [done] Drag a file from the Files panel onto an inspector asset field to assign it
  (`InspectCtx::file_field`; the browser publishes the dragged file's project-relative path to
  egui memory; plugin-safe). E.g. drop a `.postfx` onto a Volume's Profile field.
- [done] Dockable panels (egui_dock): Scene / Inspector / Files / Log / Code / Baker
  around a transparent viewport
- [done] Transform gizmos: move (W) / rotate (R) / scale (E), pivot/center + global/local,
  snap (grid + 15deg rotation), fat hit areas with orbit priority, hover emphasis
- [done] Camera-relative axis flipping + orientation cross on move/scale gizmos
- [done] Scene tree keyboard navigation while the tree has focus (last click inside it): Up / Down move
  the selection through visible rows, Right expands or steps into the first child, Left collapses or steps
  to the parent; the row scrolls into view.
- [done] Add component popup keeps a fixed size (320 px list) so filtering can't shrink it and flip it
  below the button.
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
  navigate only, with no Inspector selection.
- [done] File browser image thumbnails: image tiles (png/jpg/jpeg/tga/bmp) show a real decoded
  preview in place of the generic image glyph. Decoded + downscaled (`THUMB_MAX`) on demand for
  visible tiles only, cached as textures, budgeted per frame (`THUMB_PER_FRAME`) so opening an
  image-heavy folder doesn't hitch; undecodable files fall back to the glyph. **EXR / `.hdr`
  thumbnails** are detected as HDR (float color type / extension) and **tonemapped (ACES) +
  sRGB-encoded** before downscale — previously a raw linear→u8 clamp read black/blown-out, so
  HDR previews now match the in-engine look.
- [done] **Material sphere previews**: `.material` tiles render the material **on a lit sphere**
  (Unity/Unreal content-browser style) instead of a generic glyph. A self-contained CPU shader
  parses the material's PBR params from the RON (`base_color`, `metallic`, `roughness`,
  emission) and shades a unit sphere with a key light + Blinn specular + faux-sky reflection
  (tonemapped + sRGB), so colour/metalness/glossiness/emission are all visible. Cached + frame-
  budgeted like image thumbnails, and **re-rendered when the material's mtime changes** so an
  edit refreshes the tile.
- [done] File browser tile-size slider (top bar): scales tiles + icons/previews live
  (`tile_px`), so previews can be enlarged for a closer look.
- [done] Status bar shows the selected file's full name (with project-relative path on hover),
  so names the grid tiles clip with `…` are still fully readable.
- [done] Unified Inspector (object transform/mesh/material slots, `.material` editor,
  component list with Add/Remove)
- [done] Orbit / pan / scroll-dolly editor camera, F to frame, Escape to deselect
- [done] Editor camera viewpoint persisted in the `.scene` file (`editor_camera`:
  position/yaw/pitch) — reopening a scene restores the last framing
- [done] Play mode (Play/Stop): components run, transforms + state restored on Stop
- [done] Undo/redo (move/rotate/scale/rename, material edits + assignments; delete is
  intentionally non-undoable)
- [done] Menu bar (File/Edit/Tools/View/Help), New/Open/Save scene
- [done] Viewport widget filter (per-billboard visibility + size)
- [done] Billboard widgets: lights, cameras (frustum), probe volumes (3 bulbs), audio (speaker)
- [partial] **Editor redesign** (docs/design/editor-ui-spec.md, how-to + conversion plan in
  docs/design/editor-ui-guide.md): `citrus-editor` `theme.rs` (spec tokens, runtime accent with faint/soft
  variants, egui + egui_dock styles), `icons.rs` (spec SVG icon set drawn from path data, logo), `widgets.rs`
  (buttons, troughs/segments, icon toggles, badges, chips, type tile, axis vector fields, property rows,
  section headers, info notes, dashed add button, search field); IBM Plex Sans/Mono embedded. Converted: menu
  bar (logo, centred transport with Play/Pause/Step + game camera, breadcrumb, Ctrl K command palette,
  settings gear), status bar, dock tabs (36 px, accent underline, tab icons), spec layout sizes, hierarchy
  (type icons/colours, eye toggle, count badges, header + Add, filter by name/type, `Name (2)` duplicates),
  inspector header (type tile, borderless name, type line, Active, Layer, Static), Transform (axis fields,
  scrub, reset), component section headers + dashed Add component, themed row helpers (all inspectors),
  viewport overlays (Select/Move/Rotate/Scale/Rect tools with Q-T keys, Pivot/Global/Snap trough, playing
  chip, orientation widget with click-to-snap, stats pill, tab-strip shading/gizmos/stats/view controls,
  play outline, UI canvas frame), Log (segmented filter with counts, coloured mono rows, warn tint, empty
  state), Files (folder tree, toolbar with search / Size / Create, tiles), Environment + FluxBaker (themed rows,
  eyebrows, In-this-scene card, primary Bake), UI Rect 3x3 anchor grid, accent presets in Project Settings >
  Appearance. Remaining phases are tracked in the guide.
- [done] **UI Editor tab** (`ui_editor.rs`, `ui_edit.rs`, `ui_templates.rs`): canvas preview at a
  chosen resolution (or the 3D panel's pixels), click-select, drag-move, 8 resize handles keeping
  the opposite edge fixed for any pivot/anchors, anchor markers, snap (off/1/5/10/20, Ctrl = 10),
  arrow nudge, Add menu, 2D/3D switch, hidden pages laid out while selected (`reveal`). Same
  handles in the viewport over the Game UI overlay (3D gizmo hidden for screen UI, click picks UI
  first). Templates: Main Menu, Pause Menu, Settings, HUD, Overlay, Dialog, Loading Screen.
- [done] Code-free UI behaviour: `UiButton.on_click` (`UiAction`: load scene, show / hide / toggle
  by name, quit), `UiPauseMenu` (Escape toggles a panel, time scale + cursor), `UiSlider.audio_bus`,
  new `UiProgressBar`. 2D and 3D canvases scale from the same reference size.
- [done] Editor icons: glyphs missing from egui's fonts (the empty boxes on remove-component and
  elsewhere) replaced with Phosphor icons; arrows in UI text are ASCII.
- [done] Resize artifacts (ghost copies of geometry that never cleared): the reflection / fog
  resolve read depth from the Flux screen-GI depth prepass, which is only drawn while screen GI
  traces (never with the FluxVoxel backend, not while the camera is still). After a resize that
  image was new and held leftover memory, so fog / SSR drew ghost shapes from it. Both the editor
  viewport and the game path now resolve against the scene pass's own stored depth. Also: the
  viewport target is recreated at frame start and tracks its own GI freshness, and screen GI /
  SSR reject non-finite history.
- [done] SSR hit test is a depth-span overlap: a hit needs the ray's depth range over the step to overlap the
  surface's front + an assumed thickness (3% of depth, 5-25 cm). Fixes long grazing steps smearing small
  objects along the ray (the old "behind by < max(step, ...)" test) and rays passing far behind an object
  whose front faces the camera. Jitter is now interleaved-gradient noise at half a step.
- [done] Ray-traced reflection quality (Environment > Reflections, Ray-traced): Rays per pixel (1/2/4/8/16;
  rough surfaces sample their GGX lobe on a per-pixel rotated Hammersley set, stable frame to frame) and
  Bounces (1-4; each hit continues along its mirror direction weighted by Fresnel, so mirrors / metals show
  real reflections instead of their env fallback). Max distance now caps the primary rays. Hits read their
  baked lightmap (uv1 + layer) when lightmaps are in use, direct light matches the forward falloff and spot
  cone and skips baked lights on lightmapped hits, and the Scene view's misses fall back to the sky (its env
  intensity lane was 0, so misses were black).
- [done] Flux review fixes (2026-10-30):
  - FluxRT now actually runs: it shares the GDF bounds + emitter feed with Flux (previously only the Flux branch
    fed them, so FluxRT used stale data or never traced), and the editor Scene view + camera preview pass the TLAS
    (they always fell back to the software march). The synchronous hardware probe re-bake only runs when the
    world-probe fallback / VR needs it, and the 10 Hz cadence floor applies only then.
  - Crease stripes / flicker: probes sit on the closest real surface texel of their tile (a bilinear depth at the
    tile corner sat off the surface in creases), normals come from +-1 full-res pixel, the ray offset scales with
    the pixel footprint (a fixed 4 cm pushed rays through thin gaps); back-face hits return 0 and a probe with >25%
    back-face hits is marked invalid; the integrate has a normal edge-stop, uses the pixel's own distance, and
    takes its magnitude from the denoised gather with the SH as a clamped direction ratio (the SH estimator's
    grazing clamp biased creases dark; the denoise pass was otherwise unused); SH history is bilinear over the 4
    previous probes with per-tap distance/normal validation and the relative firefly clamp (both gathers).
  - Synchronization: GI image overwrites wait for the previous frame's reads (WAR), probe SH updates are copied in
    the frame command buffer from per-frame staging (no CPU writes into a buffer in-flight frames read).
  - Correctness: RT GI reads metallic from the instance params (was always 0), TLAS custom index = shading record
    index (instances without a BLAS shifted every later one), the software gather zeroes sky probes' SH (stale SH
    ghosted), emitter SH scales with Intensity, hit shadow rays start off the geometric normal, RT hits drop the
    SDF near-field fade (darkened exact creases), probe volumes use rotation-aware centres and FluxVoxel uploads
    each relocated probe's offset so the Chebyshev test uses the real probe position, the FluxVoxel static hash
    covers light direction / spot angles / kind, and the SDF generator uses angle-weighted pseudo-normals (correct
    sign at edges and vertices) and a BVH + worker threads (~200x faster).
  - Performance: per-frame TLAS slots rebuilt on the GPU inside the frame command buffer (no device idle, no CPU
    wait), RT scene list + texture views built once per frame, GI trace descriptor pools / buffers reused per frame
    slot, emitter shadow rays shared between the SH and scalar terms, FluxVoxel no-volume fallback uploads once,
    clipmap scrolls in quarter-extent steps.
- [done] HDR pipeline: pipeline variants carry their pass's color format (`PipelineKey::color_format`, set per
  pass), so HDR / capture / planar / VR passes no longer render with swapchain-format pipelines; reflection
  captures, the skybox environment cube and planar mirrors are RGBA16F linear HDR (no inline tonemap, grading or
  vignette baked into reflections); the env cube is built in float from EXR (f16), Radiance .hdr (now loaded as
  float) and sRGB-decoded LDR skies; `.reflprobe` v2 stores RGBA16F (v1 still loads); VR eyes render HDR + bloom +
  post into the XR image with the UI overlay after tonemapping; egui pipelines declare no depth.
- [done] Render pipeline performance review fixes: realtime reflection captures record into the frame command
  buffer with persistent targets and per-face frustum culling, finish with a GPU mip chain + copy into the live
  env (no CPU waits, no per-capture allocation) and are paced to ~5 Hz; VR eyes submit without CPU waits (slot
  ring) and use the sorted draw order; shadow views cull casters per view and cache layers whose view + casters
  + mesh deformation are unchanged; draws sort by a precomputed state key with material set / vertex buffer
  binds deduplicated; looking at empty sky draws nothing (the old fallback drew everything); planar passes,
  the GI depth prepass and the camera preview are frustum culled; per-frame world transform / active caches feed
  lights, post volumes, probes and audio; capture lights are gathered only while a capture runs; the dead SSR
  colour-history images and the editor swapchain depth were removed; bloom level transitions are batched; the
  forward GI denoise computes its tap rays incrementally.
  Later in the same pass: the G-buffer is A2B10G10R10 (octahedral normal in rg, roughness / reflectivity
  split over b + a, decoded with texelFetch), and the main pass reuses the GI depth prepass (prepass on the
  standard vertex shader with `invariant gl_Position`, main pass EQUAL test, no depth writes).
- [done] GPU instancing: scene draws read their model matrix from an instance-rate vertex stream
  (`standard.vert` / `outline.vert` locations 8-11; the shadow pass keeps push constants). Each
  `record_scene_draws` call appends its matrices to an `InstanceRing` (one per frame slot, one per VR eye slot,
  reset after that submission's fence) and consecutive draws sharing pipeline, material, mesh and per-object
  push data are one instanced draw. Custom-shader materials draw singly (their fragment code may read
  `pc.model`). `RenderStats::instanced_draws`. Strategy template: 33 objects in 9 draw calls.
- [done] TLAS refit: the per-frame RT TLAS is built with `ALLOW_UPDATE` and refit in place when only
  transforms moved (same instances and BLASes), with a full rebuild every 120 refits.
- [done] Skinned mesh culling bounds follow the pose: each skinned mesh keeps per-joint bind-space vertex
  boxes; `update_skinning` moves them by the frame's palette and the union becomes the draw's bounds
  (Animator copies and the shared IK / clip path). Test: `posed_bounds_follow_the_joints`.
- [done] Emissive Boost (Environment > Realtime GI > Advanced, `RealtimeGi::emissive_boost`, default 1 =
  physical): scales the light emissive objects throw into Flux / FluxRT (NEE emitter spheres) and FluxVoxel
  (on top of its built-in x8 gain) without changing their on-screen brightness.
- [done] **Dock tabs always reachable**:
  - **Labels shrink to fit:** when a panel's tabs don't fit, the widest labels shrink to an equal share with an ellipsis, and hovering shows the full name. This replaces egui_dock's scroll, which slid tabs under the panel edge.
  - **Scene toolbar space reserved:** the Lit / gizmo / Perspective toolbar's width is kept clear in the Scene view's strip.
  - **Close buttons:** every tab keeps its close X; closed panels reopen from the Windows menu.
- [done] **Game-path GPU zones**: the Scene, Reflections and Post timestamp zones were only stamped in the editor viewport path, so game builds always reported them as 0 and about 0.2–0.3 ms of every frame went unaccounted. They are now stamped in `render()`'s game path too. FluxRT benchmark frame: GI 0.44 ms, scene 0.20, post 0.015, shadows 0.01.
- [done] **Forward light loop skips lights behind the surface** (N·L <= 0) before the BRDF and shadow-map lookups.
- [done] **Per-draw light culling (main camera pass)**:
  - **How it works:** each built-in draw gets a 128-bit mask of the lights whose range reaches its bounding sphere. Instanced runs get the union of their draws' masks. `standard.frag` loops only the set bits.
  - **Where the mask lives:** it rides in `pc.model`'s first column, which built-in shaders no longer read because the model matrix comes from the instance stream. Custom shaders keep the real model matrix.
  - **Other passes:** reflection captures, planar mirrors, camera preview and VR light every draw, as before.
  - **Result:** pixel-identical output. Neutral in the couch test scene, where one floor draw is reached by every light; it helps scenes with many local lights.
- [done] **FluxVoxel: floor GI, no probe grid**:
  - **Floor fade:** probe coverage used to fade to ambient over the outer 20% of the volume, so a floor resting on the bottom face got about 2% of its GI and showed only probe dots. The outermost volume now has full coverage inside its box and fades over one probe cell outside it, as in DDGI. Both `sample_probes` and `sample_irradiance_smooth` do this.
  - **Backface step:** probes behind the shaded surface (such as the layer just under a floor) are dropped by a step across the surface plane.
  - **Linear weights:** the smooth sampler uses trilinear weights; smootherstep made each probe a plateau, which read as a grid.
  - **Moving emitters per pixel:** moving emitters are no longer injected into the 0.5 m grid. They travel in the light array as kind 3 lights, and `standard.frag` (plus `citrus_gi` in custom shaders) evaluates the grid's own injection kernel per pixel as indirect light. Static emitters stay in the cached, propagated grid.
- [done] **HUD preview on the Camera tab**: while editing, the game UI previews over the Scene view when the Game UI filter is on (where it can be edited), and otherwise over the Camera tab, which shows what the game will.
- [done] **Per-pixel emitter light in Flux / FluxRT**:
  - **Problem:** the gathers used to add each emissive sphere's direct light at probe resolution, then the screen-space denoise blurred it. Its world-space footprint changed with camera angle and distance, so rotating the camera changed how the couch was lit.
  - **Fix:** the gathers now store only a per-probe emitter visibility bitmask (`ScreenProbe.c[4]`, 80-byte probes). `flux_integrate.comp` shades each emitter per pixel from the pixel's own position and normal, with visibility blended over the 3x3 nearest probes on the same surface.
  - **Result:** the light is identical from every view and has no temporal lag behind a moving emitter.
  - **Test view:** `CITRUS_VIEW_MODE=2` (indirect) / `6` (realtime Flux GI only) on the game runtime, for test captures.
- [done] Multiple reflection probes with two-probe blending:
  - **Selection:** `LoadedScene::active_reflection_probes` ranks every probe for the camera (contains the camera, importance, smaller box, nearer). The best two go to renderer slots 0 (binding 7) and 1 (binding 9, its own env cube). Their boxes are in the frame UBO (`refl2_*`, `refl_blend`).
  - **Blending in `standard.frag`:** per pixel, each probe's influence ramps across its blend-distance shell inside its box. Where both boxes overlap, the higher importance, then smaller, probe wins; outside both, slot 0 is used. With a single probe the shader path is unchanged.
  - **Slot management (`refl_slots.rs`, game runtime and editor):**
    - baked probes upload their own cube
    - realtime probes, and baked probes without a cube yet, capture live into their slot (`request_reflection_capture_slot`); refreshes alternate between the two slots
    - two probes trading places swap slots (`swap_reflection_slots`) instead of reloading
  - **`.reflprobe` v3** stores one cube per probe id. v1/v2 single-cube files still load and serve any baked probe without its own entry.
  - **Baking:** Bake Reflections bakes every probe in turn; Bake This Probe updates that probe's entry.
  - **Tests:** v3 round-trip and the legacy fallback. On the GPU, a second 4x-intensity probe over part of the room brightens reflections there with a ramp across its 1 m blend shell; a single-probe scene renders identically to before.
  - **SSR / RT:** the SSR resolve (binding 5) and RT reflections (binding 11) apply the same two-probe blend (`shaders/probe_blend.glsl`), so they replace exactly the environment the forward pass added; verified on the GPU in both modes.
  - **Custom shaders:** the GLSL / HLSL preamble's `citrus_env` blends both probes the same way.
- [done] HDR verified end to end (2026-10-30): with 8x emissive orbs and bloom off, lowering exposure by 3 EV darkens the couch region from 106 to 20.5 (green, sRGB), while the orbs keep their colour (core 140/205/147 to 39/91/37: blown-out cyan at 0 EV, true green at -3 EV). Radiance above 1 survives to the tonemapper. Note: a Volume profile overrides tonemap / exposure only with `tonemap.enabled: true`.
- [done] Flux trace GPU zones: gather / denoise / integrate timestamps (`RenderStats::gpu_gi_*_ms`, profiler,
  `CITRUS_FRAME_STATS` log). Flux software gather 1.34 -> 0.87 ms: lights below 1e-3 unshadowed contribution skip
  their shadow march, 4-tap tetrahedral hit normal, shadow rays clipped to the GDF box (directional lights).
  FluxRT gather skips negligible lights too (0.29 -> 0.27 ms).
- [done] FluxVoxel static rebuild 68 -> 5 ms (DDGI moment rays stop at their 8 m clamp instead of the scene
  diagonal; probes split across threads) and asynchronous after the first build (worker thread, the previous
  base renders until the new one lands), so camera-clipmap steps and static-light edits no longer hitch.
- [done] Frame timing in game builds: `CITRUS_FRAME_STATS=1` turns vsync off, records the GPU timestamp zones
  without the profiler window, and logs the mean frame interval, GPU zones and draw calls every 240 frames.
  Before / after numbers for this pass are in ENGINE_REVIEW_2026-10-30.md §1.3a.
- [done] Mixed lighting: Flux / FluxRT run with a bake loaded and ADD to it. The gather then carries only what
  the bake can't hold (`FluxSettings::mixed_lighting`, `probe_info.y` in the gathers): moving emitters (static
  emitters are dropped from the NEE list), realtime lights (baked lights filtered out), no sky, no world-SH cache,
  and static surfaces' emission skipped (RT instance `emission.w` = static). The forward pass adds it on top of
  the lightmap for lightmapped surfaces and on top of the baked probes for dynamic objects (`debug.z` = 2).
  Turning realtime GI off returns to the bake alone (its probes are re-uploaded instead of cleared). Previously
  any bake paused Flux entirely, so a moving emitter never lit baked surfaces.
- [done] Emitter GI fixes (FluxRT, Flux, CPU march): shadow rays toward an emitter sphere stopped exactly at its
  estimated radius and grazed the emitter's own (tessellated) surface on some frames, so its light flickered on
  nearby surfaces as it moved; they now stop clearly short of it. FluxRT gather rays that hit an emitter no
  longer add its emission on top of the analytic emitter term (double counting).
- [done] Editor gizmos for physics and navigation (selected object): Rigid Body diamond coloured by body kind;
  Character Controller capsule + step-height ring; Physics Joint anchor handle (drag to move the anchor), hinge
  axis + limit arc, slider axis + limit ticks, ball rings, spring / rope rest-length circle, and a dashed line to
  the connected body; NavMesh Surface resizable bake box; Nav Agent stopping ring and, in Play, its planned path
  with a destination cross (red when unreachable). New `GizmoSpec::Lines` (local or world space) for plugins.
- [done] Ray-traced reflection hits are textured: every renderer texture is in a bindless table (rt_reflect
  binding 9, 4096 entries, descriptor indexing: runtime arrays, non-uniform indexing, partially bound) and each
  TLAS instance carries its albedo / normal / ORM / emission texture indices, tiling-offset and metallic /
  roughness / normal strength (176-byte instance record). The hit samples them at the interpolated UV with a
  ray-cone mip (pixel footprint grown over the path and by roughness, against the hit triangle's texel
  density), applies the normal map through the vertex tangent, and shades diffuse x (direct + env fill) plus
  a metal/fresnel env specular. Textures past the table fall back to base x mean albedo. Also fixed: the RT
  and bake shaders read 72-byte vertices from the 104-byte engine vertex (every fetch past vertex 0 wrong).
- [done] SSR streak / ghost reduction (`ssr_resolve.frag`): back-face rejection at the hit (G-buffer normal
  facing along the ray = the ray passed behind it; this stretched small objects into streaks), thickness
  scaled to depth (3%, 5-35 cm, never below the step's depth travel) instead of a 1 m minimum, rays heading
  toward the camera faded out, and a decisive confidence curve with a narrower screen-edge fade so a pixel
  shows SSR or the probe rather than a half-transparent double image.
- [done] Screen-space reflections ran no matter what Environment > Reflections said: the reflection mode /
  SSR settings ride in `FluxSettings`, which `realtime_gi.rs` only pushed on the Flux/FluxRT GI-on path. With
  the FluxVoxel backend or GI off the renderer kept its default (mode 1 = SSR on), so emissive orbs left SSR
  streaks on mirrors and floors even with Reflection Probes selected. Now pushed every frame before any early
  return (and SSR only when the mode is SSR).
- [done] Crash selecting objects with plugin components in a release editor (free(): invalid pointer in
  egui ViewportState drop): plugins were always built in the dev profile, and egui's structs change layout
  with `debug_assertions` / `accesskit`. Plugins now build with the editor's profile (`--release` when the
  editor is release, loaded from the matching target dir) and, when the editor binary came from the same
  workspace, in the same cargo invocation as it so feature unification yields the identical egui build.
- [done] Reflection probe capture feedback: surfaces inside the cube capture sampled the env cube, which
  in that pass is the probe's own previous capture, so every Realtime recapture (and a Bake that followed
  one) fed reflections back in (a glossy floor's reflection of an emissive ball became a second, streaked
  copy in mirrors). The capture UBO marks `refl_center.w = -1`; standard.frag then uses diffuse GI instead
  of the cube for specular (no reflections of reflections, like Unity's default bounce count).
- [done] Double reflections of emissives (sharp ball + smeared streak on a chrome cube): the FluxVoxel
  specular-from-volume term was ADDED on top of the probe / planar reflection. It is now a fallback, only
  where no probe zone or planar mirror covers the fragment.
- [done] Reflection Probe capture point: `capture_offset` (world-axis, like `box_offset`) moves where the
  cube is rendered from without moving the object or its box. Pink viewport handle (new
  `GizmoSpec::Handle` / `GizmoEdit::Handle`, dragged in the camera plane, dashed line to the box centre,
  clamped inside the box) + "Capture Offset" inspector row.
- [done] Inspector minimum width (352 px): `clamp_leaf_width` pushes its dock split back after each frame
  (egui_dock has no per-leaf minimum).
- [done] Probe reflections sparkled on curved mirrors (chrome sphere on a 2048 cube): mirror-smooth
  surfaces always sampled mip 0. The env lookup now uses max(roughness LOD, `textureQueryLod` of the raw
  reflection vector), measured before box projection so box edges don't add seams.
- [done] Reflection capture + planar mirrors shaded every surface with its normal reversed: both passes
  mirror the on-screen winding (cube faces use a positive-height viewport, planar uses a reflected camera)
  and compensated with no-cull, so `gl_FrontFacing` was false on outward faces and standard.frag flipped
  N. New `PipelineKey::mirrored` (clockwise front face, normal culling) for those passes; proven by
  `refl_capture_tests::mirrored_passes_reverse_winding` (main CCW-front, capture and planar reversed).
- [done] Viewport resize crash (SIGSEGV in egui_ash_renderer::cmd_draw): a replaced viewport target freed its
  egui descriptor set while the frame being drawn (built before the resize) still referenced its texture id.
  Replaced targets are now retired for FRAMES_IN_FLIGHT + 1 frames, then unregistered from egui and freed.
- [done] UI canvas billboard (window icon, own filter entry) and the "Game UI overlay" toggle
  moved from the menu bar into the billboard widgets menu, under Orientation cross.
- [done] Menu bar: hovering another top-level menu while one is open switches to it (egui's
  `MenuBar` only opens on click; `top_menu` / `switch_hovered_menu`).
- [done] Swapchain rebuilds whenever the window size differs from the size it was built for,
  not only on a Resized event / OUT_OF_DATE.
- [done] Global bottom status bar: project + object count, live rust-analyzer activity
  spinner, and compile/result messages ("Compiling components…", shader reloads). Global
  minimum text size (folder-explorer 13px) so nothing renders unreadably small
- [done] Log console tab (tracing ring buffer, level filter + search, follow, wrap, timestamps)
- [done] Code editor tabs with a custom **Citrus Purple** syntect theme (solid-black
  background, purple-leaning palette, borderless text box); line-number gutter; fills the
  dock; debounced auto-save; bottom status line (mode / file / language / line:col /
  unsaved, folds in the vim `:` command line); caret stays solid while moving. Minimal
  header (problem counts + hint); the tab name carries the filename
- [done] Vim mode (toggle in **Edit menu**, persisted in `project.citrus`; per-file
  mode): Normal / Insert /
  Visual / Visual-line / Command. Motions h j k l w b e 0 ^ $ gg {n}gg G {n}G (counts);
  i a I A o O; x D C dd cc yy dw cw yw d$/c$/y$ p P; visual d y c p; `u` undo / Ctrl+R
  redo (per-file snapshot stack, an insert session = one undo); `gd` go-to-definition,
  `gr` references (picker popup). Command line (`:`): `:w` write, `:q`/`:wq`/`:x` close,
  `:{n}` goto line, `[%]s/pat/rep/[g]` regex substitution (`$1`/`${name}` capture refs)
  with **live preview**: matches/replacements highlight as you type and revert on
  Escape, commit on Enter. Core subset; `f`/`t`, `/` search, `.` repeat can follow
- [partial] rust-analyzer LSP (diagnostics, completion, hover, go-to-definition,
  find-references on `.rs`); file browser badges files (and aggregates onto folders)
  with red/yellow problem dots, and live-updates as files change on disk
- [done] App icon (procedural citrus slice; X11 window icon + desktop entry install)
- [done] Crash handler (symbolized backtrace on SIGSEGV/SIGBUS/SIGILL/SIGABRT)

### Components
- [done] `TypedComponent` trait (serde + Default + inspector UI + start/update/late_update),
  `ComponentRegistry`, serialize into `.scene`, participate in undo
- [done] Built-ins: Camera, Light, LightProbeVolume, AudioSource, AudioListener,
  BoxCollider, SphereCollider, MeshCollider, RigidBody, CharacterController, Pawn,
  SpawnPoint, Spin, Bob
- [done] **Editor Play shows the game camera**: the viewport renders from the scene's active
  camera while playing (Pawn possession etc.), with a "Game camera" toolbar toggle to fly the
  editor camera instead. Previously Play always rendered from the editor fly camera.
- [done] **Screen / pointer API**: `ctx.screen_size`, `pointer_position`, `screen_ray(px)`,
  `pointer_ray()`, `world_to_screen(p)` (click-to-select / click-to-move with `raycast`).
- [done] **Shared play loop** (`play.rs` `PlaySession`): the editor's Play mode and a
  shipped game run the identical frame: `start` for newly active objects, fixed 60 Hz steps
  (`fixed_update` + physics), collision/trigger callbacks, `update`, `late_update`, audio.
  Components on inactive objects don't run. Stop-restore matches objects by id, removes
  objects spawned during play, and reloads the scene from disk if play destroyed objects.
  Headless tests in `play.rs` cover the loop end to end.
- [done] Rust component plugins (`plugins/*` workspace crates, cargo-built + dylib-loaded,
  `citrus_register`, hot reload)
- [done] Runtime scene switching: `ComponentCtx::load_scene(path)` lets gameplay
  components change levels / go menu -> game during Play (first slice of the in-game API,
  2D); editor Open/New/Save Scene already covers authoring
- [done] Custom GLSL shaders v1 (runtime glslc, pragma-declared properties reflected into
  Inspector, hot reload)
- [done] Engine built-in shader hot-reload: `standard.frag` (baked into the binary) is
  watched on disk; edits outside the editor recompile via glslc and hot-swap the pipeline
  (cached variants rebuilt lazily), with status-bar success/compile-error feedback

### Audio
- [done] AudioSource (clip, play-on-start, loop, volume, pitch) + Spatial toggle (min/max
  distance, linear/log rolloff); AudioListener; driven in editor Play mode **and in a
  shipped game** (the runtime previously never opened an audio device)
- [done] Gameplay audio: `ctx.play_sound` / `play_sound_at` one-shots,
  `ctx.play_audio` / `stop_audio` for Audio Sources; sources on spawned objects start when
  they activate and stop when their object is destroyed or disabled
- [done] **Steam Audio spatialization** (`steam_audio.rs`, `audionimbus` 0.16 / Steam Audio 4.8.1, Apache-2.0;
  cargo feature `steam-audio`, on for the editor): spatial Audio Sources and `play_sound_at` one-shots render
  through Steam Audio — HRTF binaural (or stereo panning), distance attenuation (the source's rolloff),
  air absorption, directivity (weighted dipole along the object's -Z). Each voice is a rodio Source rendering
  1024-sample frames at 48 kHz; the game thread pushes per-voice parameters through a shared slot. The listener
  is a full pose (Audio Listener transform, else the camera).
- [done] **Occlusion + transmission**: a worker thread ray-traces active static-geometry meshes (one merged
  Steam Audio static mesh, rebuilt only when the static set changes) against every occluding voice every
  ~33 ms; results feed each voice's direct effect.
- [done] libphonon is downloaded at build time (`auto-install`), copied next to the editor binary with an
  `$ORIGIN` rpath (`crates/citrus/build.rs`), and shipped next to built games (`bundle.rs`). Feature off /
  init failure: the old distance-volume path.
- [done] **Steam Audio, full interface** (`steam_audio.rs`, audionimbus FFI to Valve's libphonon 4.8.1):
  real-time reflections (Convolution / Parametric / Hybrid) per source or as listener-centric reverb,
  decoded binaurally; acoustic materials (`Acoustic Material`: 11 Steam presets + custom) per static
  triangle; moving occluders (instanced meshes, transforms updated per tick); probe baking (`Audio Probe
  Volume`, Tools > Bake Audio -> `<scene>.audiobake`, loaded with the scene; baked reverb inside probe
  volumes, real-time elsewhere); pathing from baked probes; ambisonic clips (order 1-3, listener
  rotation + binaural / speaker decode) and ambisonic encode for sources; SOFA HRTFs (fallback to
  default); output layouts Headphones / Stereo / Quad / 5.1 / 7.1 with virtual surround for beds; ray
  tracer Default / Embree / Radeon Rays with fallback. Project Settings > Spatial Audio
  (`SpatialAudioSettings` in project.citrus). Game builds ship STEAM_AUDIO_LICENSE.txt + third-party notices.
- [todo] TrueAudioNext (AMD GPU convolution); per-volume bake flags (one probe batch today); shared
  reverb bus (listener reverb is convolved per voice); reflections for one-shots. First build after
  `cargo clean` needs a second build to place libphonon next to the editor.

### Physics / collision
- [done] Collider components (Box / Sphere / Mesh-convex) with is_trigger + layer, spawnable
  standalone or as components, yellow editable viewport widgets
- [partial] Physics simulation (rapier3d): a `RigidBody` component (Dynamic / Kinematic /
  Fixed, mass, restitution, friction, gravity scale); collider-only objects are static.
  **Layer-collision matrix** (done): colliders carry rapier `InteractionGroups` from the
  scene's `LayerSettings`.
- [done] Physics rework (`physics.rs`): bodies keyed by `ObjectId`, so spawn / destroy /
  enable mid-play add and remove bodies; **fixed 60 Hz step** with interpolated write-back
  (was variable `dt`); every collider component on an object becomes part of its body;
  **Mesh Collider** uses exact triangles for static geometry and a convex hull for moving
  bodies (was an AABB box); kinematic bodies follow their transform (previously overwritten
  by the sim); gameplay writes to a dynamic body teleport it; parented bodies write back
  in local space; CCD on dynamic bodies. Static-only worlds now step, so raycasts work in
  them.
- [done] **Collision + trigger callbacks**: `on_collision_enter/exit`,
  `on_trigger_enter/exit(other)` delivered to every component on both objects.
- [done] **Queries + forces** for gameplay: `ctx.raycast` / `raycast_filtered` (layer mask,
  triggers, exclude), `ctx.overlap_sphere` (exact shape test), `ctx.velocity`,
  `add_force` / `add_impulse` / `add_torque` / `set_velocity`.
- [done] **Character Controller** component (rapier kinematic character controller):
  capsule, step height, max slope, snap to ground; `ctx.move_character(delta)` +
  `ctx.is_grounded()`. `Pawn` walks through it when present (was a flat floor).
- [todo] Joints, contact points/impulses in collision callbacks, character pushing dynamic
  bodies, shape casts, live collider edits during play.

---

### Game-readiness push, part 3 (2026-10-29)
- [done] **World-space UI**: `UI Canvas` render mode World renders into a texture (per-canvas
  egui context, painted inside the frame command buffer) shown on an unlit quad; mouse ray +
  right VR controller ray interaction; `Create -> UI -> World Canvas (VR)`.
- [done] **UI text fields** (`UI Text Field`, `ctx.ui_submitted`), **keyboard / gamepad
  navigation** (focus, submit, cancel, slider steps), `ctx.ui_cancel_pressed`.
- [done] **Animation**: blend trees (1D / 2D, phase-synced), animation events
  (`on_animation_event` + `AnimationEvent`), root motion (through the Character Controller),
  **GPU skinning** (compute pass writing the vertex buffer; CPU fallback per Animator).
- [done] **Animator Controllers + clip authoring** (2026-10-30): `.animctrl` assets
  (`citrus_assets::AnimatorController`: Float / Int / Bool / Trigger parameters; layers with
  weight, Override / Additive blending and bone / path masks; per-layer state machines with
  clip, `.anim` or blend-tree states, speed parameters, on-enter / on-exit events, Any State,
  default state, transitions with conditions, exit time, duration, offset, interruptible).
  Runtime in `citrus-engine/src/anim_graph.rs` (rig + masked TRS poses, trigger consumption,
  Any State priority, cross-fades, layer composition, root motion on the base layer) hooked
  into `update_skinning` for skinned models and `PlaySession::animate_objects` for plain object
  hierarchies. `Animator.controller` (empty = old clip path), `Animator.drivers` (parameters
  fed from movement / ground check / another Animator), `AnimatorStateChanged` events, code API
  (`set_animator_int/bool/trigger`, `play_animator_state`, `animator_state`, ...). `.anim`
  clips (`citrus_assets::AnimClip`: tracks by relative object path or bone name, Position /
  Rotation / Scale / Light Intensity, Step / Linear / Cubic keys, clip events). Editor:
  **Animator Controller** tab (graph canvas with pan / zoom, drag states, Shift-drag or
  Make Transition, right-click create / default / duplicate / delete, Layers + Parameters
  panel with live values while playing, state / transition / layer inspector, live state
  highlight + transition progress, autosave) and **Animation** tab (ruler + playhead scrub,
  track list with Add Property for children and bones, key diamonds with select / drag /
  delete / copy / paste / per-key curve, Record from gizmo / Inspector edits, edit-mode
  preview playback, length / loop / FPS, clip events). Files: icons, Create -> New Animator
  Controller / New Animation Clip, selecting the file opens its tab; Animator inspector has
  the Controller field + Open.
- [todo] Animation follow-ups: curve (tangent) editor, sub-state machines, transition
  interruption sources, humanoid retargeting, bone preview / record for skinned models in edit
  mode (bone tracks are keyed by value), material colour tracks.
- [done] **Physics joints** (`Joint`: fixed / hinge / ball / slider / spring / rope, limits,
  motors, `ctx.set_joint_motor`).
- [done] **Navigation**: height-aware navmesh baked from colliders (slope, step, clearance,
  radius erosion), A* + string pulling, `Nav Agent` steering, `ctx.set_destination` /
  `find_path` / `rebake_navmesh`, editor "Show navmesh" preview.
- [done] **Particles**: `Particle Emitter` (CPU sim, camera-facing unlit quads, color / size
  over life, bursts, gameplay control), previews in the editor.
- [done] **Seamless loading / multi-scene**: shared background loader (`scene_loader.rs`) for
  editor and runtime (models, textures, skybox, baked lighting, shaders all off the main
  thread; skybox / bake applied over following frames). `ctx.preload_scene`, instant
  `load_scene`, `load_scene_additive` / `unload_scene`, `keep_on_scene_change`, `Scene Portal`
  component. Editor Open Scene is now asynchronous (was a main-thread stall), as are all
  editor reloads (Stop-restore, reimport, lightmap-UV regen, focus reimport) and Play
  refuses to start while one is pending. Scene switches never block: `SceneFlow::request_switch`
  starts the load, the current scene keeps running, `poll_switch` installs it when ready
  (kept objects serialized at install time). The runtime boot scene loads the same way
  (no frames until it's in; a failed boot load still exits with the error).
- [done] **Material texture means off the main thread**: `apply_material`'s albedo / emission /
  mask / ORM averages (GI emitter scale, bake albedo/metalness) used to decode every texture
  synchronously per material. Now `citrus_assets::texture_mean` caches them on disk
  (`.citrus_texcache/<file>.<srgb>.mean`, invalidated by mtime/size) and the loader worker
  warms the cache in parallel (`collect_material_mean_refs`).
- [todo] Remaining main-thread work during loads: the scene install itself (pipelines,
  materials; one step), post-load skybox upload / baked-lighting upload / reflection capture
  (one frame each, with device waits), `set_material_textures`' `device_wait_idle`, the VR
  renderer build (must be created against the OpenXR instance), the runtime `Renderer::new`,
  and the in-editor plugin rebuild / Open Project cargo builds.
- [done] June modules resolved: savegame (`ctx.save_value` / `write_save` / `load_save`),
  localization (`ctx.tr`, `localization/*.loc`), audio mixer (saved in `project.citrus`, applied
  to Audio Source buses, `ctx.set_bus_volume`), LOD Group (model swap by distance, hysteresis,
  culling). Superseded modules (`events`, `anim_graph`, `asset_handle`, `streaming`,
  `shader_graph`, `ui_canvas`, engine copies of `savegame` / `localization` / `lod`) deleted.
- [todo] Visual shader graph (the old `shader_graph` codegen had no editor), additive-scene
  baked lightmaps, joint breaking, contact points in collision callbacks.

## 2. Goals — to be implemented

### 2A. Pawns & camera possession [partial]
A **Pawn** is a controllable entity that a controller can "possess". It owns movement
state and can drive which scene camera is active.

**Implemented** (`Pawn` component in `citrus-core`, FP/TP/TopDown/Strategy modes): a possessed
pawn reads the action snapshot, moves itself (transform-based on a flat floor; physics-driven
movement is the follow-up), yaws the body + pitches a child camera, and **activates its camera**
via `ComponentCommand::SetActiveCamera` (a `LoadedScene::active_camera_override` honored by
`main_camera`, cleared on Stop). The pawn drives a child camera through a new
`ComponentCommand::SetLocalTransform`. Params editable in the inspector (`Inspect for Pawn`).
Remaining: RigidBody-driven movement, spring-arm collision, camera-follow rigs as a separate type.

- [todo] `Pawn` component: identity + movement params (mass, gravity, jump power, move
  power per-direction, max speed, accel/decel, ground friction, air control) editable
  in the Inspector **and** via the in-game API
- [todo] Possession model: a controller possesses/unpossesses a Pawn; only the possessed
  Pawn receives input
- [todo] Active-camera registry: scene cameras enumerable by id/name; API to set the active
  render camera at runtime (`set_active_camera(cam)`), independent of which Pawn is possessed
- [todo] Camera-follow modes wired to the controller type (rig per controller below)
- [todo] Pawn / physics binding: movement applies forces/velocity through the RigidBody
  (physics engine), not direct transform writes, when a body is present
- [todo] Serialize Pawn params in `.scene`; restore on Stop like other play state

### 2B. Player controllers [partial]
Controllers translate **bindings** (abstract actions) into Pawn movement. Shared
`Controller` interface; concrete movement per type. All support the basic verbs the
controller needs (move fwd/back/left/right, jump, look, etc.).

**Implemented** as `Pawn` control **modes** (combined pawn+controller for simplicity):
**FirstPerson** (WASD + mouselook, jump, camera at eye height), **ThirdPerson** (WASD relative to
facing, orbit camera on an arm), **TopDown** (world-plane WASD, body faces movement, fixed high
camera), **Strategy** (camera-only pan). Movement reads the action snapshot (device-agnostic) and
camera placement is per-mode. A dedicated `Controller` trait + spring-arm collision + click-to-move
navmesh pathing remain follow-ups.

**Spawn points** (separate goal): a `SpawnPoint` component (tag + index) marks locations. A `Pawn`
with a matching `spawn_tag` teleports there on Play start, and game code queries them via
`ctx.spawn_point(tag)`. The engine surfaces them in `ComponentCtx.spawn_points`.

- [todo] `Controller` trait/interface: consumes an action snapshot from the binding system,
  produces movement intent for the possessed Pawn (decoupled from raw input devices)
- [todo] **First-person** controller: WASD + mouselook, jump, optional crouch/sprint;
  camera at eye position
- [todo] **Third-person** controller: WASD relative to camera, orbit camera rig with
  collision spring-arm, jump
- [todo] **Isometric / top-down** controller, two modes:
  - [todo] click-to-move (navmesh/raycast-to-ground pathing)
  - [todo] WASD direct movement in iso space
  - [todo] fixed isometric camera rig
- [todo] **Strategy** controller: edge/WASD camera pan, zoom, rotate; no possessed body
  (camera-only) or unit selection later
- [todo] Movement params read from the Pawn (jump power, move power, etc.), physics from the
  RigidBody, so controllers stay device- and physics-agnostic
- [todo] Inspector: pick controller type per Pawn; expose its tunables

### 2C. Input binding system [done]
Control schemes that are **independent of the controller** but share one interface, so
each game can mix keyboard+mouse and/or gamepad freely.

**Implemented** (`citrus-core::input` + `citrus-engine::input_engine`): named **actions**
(Button / Axis1 / Axis2), **bindings** (composite WASD→2D axis, analog sticks, deadzone/scale),
**control schemes** (default KB+Mouse and Gamepad), gamepad via **gilrs**, the per-frame
`InputState` snapshot read through `ComponentCtx` (`ctx.input.axis2("Move")`,
`ctx.input.pressed("Jump")`). Schemes **serialize to `project.citrus`** (`ProjectFile.bindings`)
and are editable in the editor **Tools → Input Bindings** window (rebind by capturing the next
key/mouse press) and at runtime via the same `Bindings` API. Tested in `input.rs`.

- [todo] **Action** abstraction: named actions (e.g. `MoveX`, `MoveY`, `Jump`, `Look`,
  `Fire`) typed as button / 1D axis / 2D axis
- [todo] **Binding**: map physical inputs (key, mouse button/axis, gamepad button/stick/
  trigger) to actions; composite bindings (WASD to 2D axis), modifiers (invert, deadzone,
  scale), chords
- [todo] **Control scheme**: a named set of bindings (e.g. "KB+Mouse", "Gamepad"); active
  scheme switchable, auto-switch on last-used device
- [todo] Device backends: keyboard + mouse (winit) now, **gamepad** via `gilrs`
- [todo] Per-frame action snapshot the controller reads (`ctx.input.action("Jump").pressed()`,
  `.axis2("Move")`), exposed through the in-game API
- [todo] Serialize schemes/bindings to a project asset (`.bindings` or in `project.citrus`);
  editor UI to author them (rebinding screen pattern)
- [todo] Runtime rebinding API (for in-game key-remap menus)

### 2D. In-game API (scripting surface for components) [todo]
Other engines expose the world to gameplay scripts; today `ComponentCtx` only hands a
component **its own local TRS + dt/time**. Goal: a real API surface that non-editor
components use to read/affect the world, identical in editor Play mode and in a built
game.

Status 2026-10-29: the core gameplay surface landed (see docs/components.md for the
reference): other-component access, events, spawn/destroy/enable, physics, audio, app
control. Items below are marked individually.

- [partial] Expand `ComponentCtx` (or a new `World`/`Api` handle) with:
  - [partial] **Self transform**: local read/write (done); `self_transform()` world read
    (done); `set_world_position(world)` world-space *write* (done; converts through the
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
    `find_object(name)` kept as a convenience. **Spawn / destroy / set-active done**:
    `ctx.spawn_prefab(path, pos, rot)` (loads `.prefab`, returns the new id immediately),
    `ctx.instantiate(template, pos, rot)`, `ctx.destroy` / `destroy_self`,
    `ctx.set_active` / `is_active`. Tags and reparenting still todo
  - [partial] **Transforms of other objects**: any object resolves to a `Transform`
    (translation/rotation/scale, + forward/right/up/matrix) via `object_transform` /
    `object_transform_named` / `object_position` / `object_matrix` (world snapshot);
    *set* still todo
  - [partial] **Components**: typed read `ctx.get::<T>(obj)` / `get_self::<T>()` (siblings
    included) and deferred write `ctx.modify::<T>(obj, |c| ...)` done; add/remove todo
  - [done] **Input**: `ctx.input` action snapshot (2C)
  - [done] **Physics queries**: raycast, overlap, velocity; force/impulse/torque/velocity;
    collision + trigger enter/exit callbacks; character controller. Shape-cast and
    "stay" callbacks todo
  - [todo] **Shaders / materials**: set material properties + shader params at runtime
    (the pragma-declared props), swap materials
  - [todo] **Colliders**: toggle, resize, change layer at runtime
  - [partial] **Camera control**: `ctx.set_active_camera(cam_ref)` (done, 2A); set
    FOV/post params still todo
  - [done] **Graphics settings (runtime)**: `ctx.set_resolution(w, h)`, `ctx.set_vsync(on)`,
    `ctx.set_shadow_resolution(res)`, applied immediately in editor Play + a shipped game, so an
    in-game settings menu can change resolution/quality live
  - [done] **Networked messaging**: `ctx.broadcast(text)` / `ctx.send_to(peer, text)` +
    `ctx.messages()` (2G)
  - [partial] **Audio**: one-shots (`play_sound`, `play_sound_at`) and Audio Source
    play/stop done; runtime volume/pitch/spatial changes todo
  - [todo] **Lights**: color/intensity/range/enabled at runtime
  - [done] **Time / scene / app**: `ctx.set_time_scale` (0 pauses; `ctx.unscaled_dt` for
    menus), `ctx.quit()`, `ctx.set_cursor_locked` (Escape releases in the editor, clicking
    the viewport re-locks); **load/switch scene is done**: `ComponentCtx::load_scene(path)` (the first in-game-API slice) queues a
    `ComponentCommand` the engine applies after the update pass; switches levels / menu
    -> game during Play, continues playing in the new scene, and Stop returns to the
    pre-play scene.
  - [done] **Events / messaging**: typed broadcast bus, `ctx.emit(event)` /
    `ctx.events::<T>()` (readable the following frame)
- [done] Stable surface usable from plugin components without reaching into editor
  internals: everything lives in `citrus-core` (`ComponentCtx`, `PhysicsQuery`,
  `ComponentLookup`), implemented by the engine

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

### 2F. Game UI system (runtime UI) [partial]

Status 2026-10-29: the retained screen-space system (A) is working in editor Play and in a
built game (`game_ui.rs`, components in `citrus-core/src/ui.rs`, reference in
docs/components.md "UI"):
- [done] Components: `UI Canvas` (constant-pixel / scale-with-screen + match width/height,
  sort order), `UI Rect` (anchors / pivot / position / size, y-down, anchor presets in the
  Inspector), `UI Panel` (color, corner radius, border), `UI Image` (texture, tint,
  preserve aspect), `UI Text` (size, color, X/Y alignment, wrap, custom .ttf/.otf font),
  `UI Button` (state colors, interactable), `UI Toggle`, `UI Slider`.
- [done] Layout: canvas roots, children laid out top-down in hierarchy order; an element
  without a `UI Rect` fills its parent.
- [done] Rendering: painted through the renderer's existing egui pass (no new Vulkan pass);
  egui provides the font atlas, text layout and texture upload. The runtime owns an egui
  context sized to the window; the editor paints into its own context over the viewport
  rect, so the result matches.
- [done] Interaction: press/release click semantics, hover/pressed states, slider drag,
  toggle flip; `ctx.ui_clicked(button)`, `ctx.pointer_over_ui()`. Clicks on UI no longer
  select objects behind them in editor Play.
- [done] Editor: Create -> UI -> Canvas / Panel / Text / Button / Image / Toggle / Slider
  (parents under the selected UI tree or the first canvas, making one if needed); live
  layout preview over the viewport in edit mode ("UI" toolbar toggle) with the selected
  element outlined.
- [todo] World-space canvases + VR controller-ray interaction, 9-slice images, masks /
  scroll views, text input fields, keyboard / gamepad navigation, visual rect handles in the
  viewport. `ui_canvas.rs` (the earlier unwired layout solver) is superseded and can be
  removed.
In-game menus / inventory / HUD. **The developer picks the approach per project**;
both are first-class and can coexist (e.g. egui debug overlay on top of a retained HUD):

- **A. Retained scene-graph UI** (the citrus-native system, below), Unity uGUI-style:
  widgets are scene objects under a `UICanvas`, **visually authored** in the editor and
  serialized into `.scene`, **screen-space + world-space** (world-space for VR
  controller-ray interaction). Best for polished, designer-authored, VR, and
  shipped-game UI. This is the default and the larger build.
- **B. Immediate-mode UI (egui)**, opt-in, code-driven. The same egui the editor uses,
  exposed to gameplay so a component builds its UI each frame in Rust. Best for debug
  HUDs, dev tools, prototypes, and devs who already know egui. Lighter to author (no
  scene wiring), but not visually authored and weaker for world-space/VR.

How the choice works in a build:
- [todo] egui (option B) is always **available**: `citrus-render` keeps the egui pass in
  every build (~1.5M; not worth gating out). To use it a game opts in at the API
  level: `run_game` hands each frame's egui `Context` to a game callback, and
  `FrameInput.egui` carries the tessellated output exactly as the editor's path does (the
  plumbing already exists; a default game leaves it `None`).
- [todo] The retained system (A) never requires egui; egui (B) never requires the
  retained system. A project can use both: retained HUD + an egui debug panel.
- [todo] Editor authoring (visual rect editing, inspector wiring) applies to the retained
  system only; egui UI is authored in code.

The rest of this section specifies the **retained scene-graph system (A)**.

Foundations
- [todo] `UICanvas` component: the UI root. Mode = **Screen-space** overlay (reference
  resolution + scale mode: constant-pixel / scale-with-screen / match-w-h) or
  **World-space** (a quad in the scene at the object's transform, sized in world units).
- [todo] `UIRect` (RectTransform-style) on every UI widget: anchors (min/max), pivot,
  offsets/size; parent rect resolved top-down each frame so children lay out relative to
  the parent. Replaces or augments the normal Transform for UI objects.
- [todo] **2D UI renderer** in citrus-render: batched quads (panels/images, solid color
  or texture, optional 9-slice), per-canvas clip/mask rects. Screen-space drawn as an
  overlay pass after the scene; world-space drawn as scene geometry (so it depth-sorts
  and is hittable by a 3D ray).
- [todo] **Text rendering**: font atlas / glyph cache (candidate: `fontdue` or `ab_glyph`
  for raster, `cosmic-text` if shaping/i18n needed), SDF optional for crisp scaling;
  alignment, wrapping, color. New subsystem; the editor's egui fonts don't carry over.

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
  the canvas quad via a 3D ray (mouse ray now, **VR controller ray** when VR lands;
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

### 2G. Networking & multiplayer [partial]
Built-in networking so games can be multiplayer, supporting **both** topologies:
**client-server** (an authoritative server, dedicated or player-hosted) and
**peer-to-peer**. One replication/API surface; the topology is a choice per game.

**Implemented** (`citrus-engine::net`): a UDP **star-relay** session (`NetSession::host`/`join`).
One peer hosts (dedicated server or player-host), others join, all traffic relayed through the
host, so one path serves client-server and P2P on a LAN. **Ownership-based replication**: the
`Sync` component marks an object networked; whoever **grabs** it (presses the grab action) claims
authority (host-arbitrated, last-claim-wins), broadcasts its transform to everyone else (who apply
it snapped/smoothed), and releases it for others, exactly the "move it, let go, someone else takes
it" model. Exposed through `ComponentCtx` (`net.owns`, `request_ownership`, `release_ownership`) +
`NetView`. Driven from the editor **Tools → Network** panel or a game's `CITRUS_HOST`/`CITRUS_JOIN`
env vars. Wire format is a compact hand-rolled binary (no extra deps).

**Messaging** (public/private): `ctx.broadcast(text)` and `ctx.send_to(peer, text)`; received
messages arrive via `ctx.messages()` (`(from_peer, is_private, text)` each frame), host-routed.

**Spatial voice comms** (`citrus-engine::voice`, push-to-talk on the `Voice` action): mic captured
via **cpal**, downmixed to mono 16 kHz, sent as PCM frames over the transport, and played back per
peer through a **jitter buffer** (≈120 ms pre-buffer + seq reordering) so it never sounds laggy:
late/lost packets become brief silence, not time-stretched "lag." Playback is **spatial**: each
peer's voice sink volume falls off with distance (positioned at the object that peer owns), mirroring
the `AudioEngine`'s distance model. Latency-agnostic by construction (network arrival is decoupled
from playback). LAN-grade raw PCM; Opus + packet-loss concealment are follow-ups.

Remaining: NAT traversal, reliability/delta-compression, client prediction/reconciliation, lockstep.

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
  sync, pairs with a deterministic physics step, see #26). Trust model documented.

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
  ownership queries, `spawn_networked`, send/receive RPCs, so components write
  network-aware logic without touching the transport.
- [todo] **Voice chat** (VR-first: spatial voice) + **IK pose replication** for avatars
  (folds in the existing M6 milestone scope).
- [todo] Editor/runtime tooling: a local "host + N clients" test harness, a net-stats
  overlay (ping, bandwidth, packet loss), and lag simulation for testing.

### 2H. Render-to-texture cameras (camera output as a material input) [partial]

Landed (2026-10-30):
- **Camera component:** Render Texture toggle plus Texture Size (`CameraComponent::render_texture`, `texture_size`). A render-texture camera is never the main camera.
- **Materials:** sample the camera through the texture path `camera:<object id>`. No file-format change; it works in albedo / emission and resolves via `Renderer::camera_target`.
- **Renderer:** one linear HDR target per camera (max 8), registered as a normal texture with its own depth and per-frame set-0 copies (binding 0 = its UBO; bindings 1-9 copied from the frame set). Recorded every frame before the main pass, frustum-culled to that camera. The main pass tonemaps the screen like any other emissive surface.
- **Recursion guard:** materials that sample any camera target are skipped inside the camera passes, so a screen never draws itself or another screen.
- **Editor:** the camera inspector's Create Screen button writes an emissive `<camera> screen.material` and spawns a plane facing the view, sized to the texture aspect.
- **Verified:** a camera at the side of the room shows its live, upright view on a floor screen while the main view is unchanged. Test: `camera_texture_paths_name_a_camera_and_never_the_main_view`.
- **Update Rate (Hz, 0 = every frame):** throttles a camera's refreshes, and a camera whose screen isn't in the main camera's view isn't rendered at all. Verified: moving the screen out of view drops the pass (GPU 0.359 to 0.326 ms for a 512x288 target).
- **Resizing:** changing Texture Size resizes the live target in place (same texture handle; materials using it are re-pointed).
- **Still open:**
  - screens inside screens
  - the mirror / portal variants
Let a `Camera` render the scene from its viewpoint into an offscreen texture that a
material can sample, so a plane + that material becomes an in-world screen: a CCTV
monitor, a TV showing a moveable camera, a mirror, a portal, a minimap, or
picture-in-picture. The camera is an ordinary scene object, so moving it (or
possessing it, 2A) updates the screen live.

Foundation already in place: the renderer's `CameraPreview` (`citrus-render`) is
already a render-to-texture pass, an offscreen color+depth target with per-frame
camera UBOs, rendered by the scene pass and exposed to egui as a user texture. This
feature generalizes that single editor-only target into a reusable, material-sampled
resource.

- [todo] **`RenderTexture` GPU resource**: an offscreen target = color image (+ its own
  depth), a sampler, and per-frame-in-flight descriptor wiring. Configurable extent and
  format: `rgba8 srgb` (LDR / UI), `rgba16f` linear (HDR, feeds bloom/tonemap), optional
  mip chain for minified screens. Managed in a pool keyed by a handle; created/resized/
  destroyed as cameras opt in or change size.
- [todo] **Camera output mode**: extend `CameraComponent` with an output target,
  `Display` (default: the main swapchain pass) vs `RenderTexture { handle, width, height,
  format, clear_color, update: EveryFrame | OnDemand | Hz(n) }`. A camera in
  `RenderTexture` mode is excluded from the main display pass.
- [todo] **Material texture source**: today `MaterialTextures` slots are project-relative
  file paths bound into set 1 (`t_albedo`/`t_normal`/`t_orm`/`t_emission`, combined image
  samplers). Make a slot a typed source (`FilePath(path)` | `RenderTarget(camera ref)`),
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
  feed is a cycle. Bound it: render each `RenderTexture` at most once per frame and let a
  cyclic sampler read *last frame's* result (one-frame latency), or drop the camera's own
  target from its view. No unbounded recursion.
- [todo] **Mirror / portal variant**: a mirror is an RTT camera whose view matrix is the
  main camera reflected across the screen plane, with an oblique near-plane clip at the
  mirror surface; a portal pairs two cameras. Same resource, different view derivation;
  list as a follow-on once the basic RTT path works.
- [todo] **Editor**: Camera inspector picks output (Display / Render Texture + size +
  format); a Render Texture shows in the file browser / material texture slots as a
  droppable source (drag onto a slot, like a file). Drop it on a plane's material and you
  have a working TV whose picture tracks the camera.
- [todo] **Performance & culling**: each active RTT camera is an extra scene pass per
  frame. Skip rendering a target no visible material samples (or whose screen is
  off-camera / too small in screen space), throttle via the `Hz`/`OnDemand` update mode,
  cap resolution, and share the shadow map with the main pass. Document the per-target
  cost.

Ties to camera control in the in-game API (2D: set active camera, FOV) and to camera
possession (2A: the moveable camera can be a possessed pawn). HDR targets feed the
existing tonemap/bloom path.

### 2I. Build & bundle (game export) [in progress]

- [done] **Build targets (2026-10-30)**: `BuildTarget::{Host, Windows}`. Project Settings has a
  Target dropdown, the File menu has **Build Game for Windows**, and the CLI takes
  `citrus --build <dir> --target windows`. From Linux / macOS it cross-compiles with
  `cargo xwin rustc --target x86_64-pc-windows-msvc`, after checking for cargo-xwin and the
  rustup target. Output goes to `build-windows/` (exe, `phonon.dll` plus Steam Audio licence
  files, assets). Verified: the FPS template builds and runs under Wine (RTX 4080, Steam Audio,
  HUD). Players need the VC++ 2015-2022 runtime. The GNU target is unsupported (audionimbus-sys
  whitelist).
- [partial] **Android build (2026-10-30)**: the engine and a game build and link for
  `aarch64-linux-android` (NDK r29; winit `android-native-activity` on Android; plugin `.so`
  on Android). `citrus_engine::run_game_android(app, config, register)` is the `android_main`
  entry. **APK packaging:** `tools/android/build_apk.sh <game-crate> <assets> <out.apk> [package]`
  builds the cdylib (`examples/android-game` shows `android_main`), bundles the assets with a
  `citrus_assets.txt` list, writes a NativeActivity manifest (Vulkan 1.3 required, arm64-v8a),
  then `aapt2` / `zipalign` / `apksigner` with a debug key kept next to the APK. On first run
  `citrus_engine::android_assets_root(&app)` extracts the assets to internal storage (again only
  when the list changes). Verified: the FPS template packs into a 6.7 MB APK that `aapt2 dump
  badging` and `apksigner verify` accept, exporting `ANativeActivity_onCreate` + `android_main`.
  **Not yet:** a Vulkan 1.1 fallback (Quest 2), ASTC textures. **Never run on a device:** the
  library links the system's minimal `libstdc++.so` (the texture compressor's build script), which
  only a device run can confirm.
- [done] **Background builds**: Build Game runs as a status-bar task instead of blocking the
  editor; one build at a time, and the result lands in the Project Settings status line.
Turn a project into a standalone, runnable game: compile the runtime with the project's
components linked in, collect every asset the game needs, and emit a `build/` folder with
an executable a player can double-click. No editor, no toolchain assumptions on the
target machine.

citrus is a **Rust-native** engine, so its build model matches Bevy, not Unity/Godot:
the components *are* Rust code compiled into the binary, so a build is fundamentally a
`cargo build --release` of a thin runtime binary + an assets folder resolved relative to
the executable. Unity (player exe + `_Data/` with managed DLLs) and Godot (export-template
binary + a `.pck` data pack) ship scripts *as data* because they're interpreted/managed;
citrus does not. An optional packed-archive step (Godot-style `.pck`) is a later add; v1
emits a folder.

**Boot-scene decision:** both a project setting *and* an editable entry file. The
generated `src/main.rs` is real, editable Rust that by default reads `boot_scene` from the
project config and calls `run_game`. Beginners never touch it (Godot "Main Scene"
convention), advanced users edit it for custom startup (splash, save-driven scene choice;
Bevy "main.rs is code"). The setting is the default; `main.rs` is the override.

Landed so far:
- [done] **Runtime game loop**: `citrus_engine::run_game(GameConfig, register)` (in
  `citrus-engine/src/runtime.rs`): opens a window, creates the renderer, loads the boot
  scene, fires `start`, then runs `update`/`late_update` + render each frame with
  `FrameInput.egui = None` and `camera_preview = None` (no editor code on the path). Drains
  `ComponentCommand::LoadScene` to switch scenes. Uses the scene's `Camera` component for
  view/proj (fixed fallback if none). `GameConfig::from_project_dir` reads `boot_scene` +
  title from `project.citrus`.
- [done] **Static component linking**: the project's component crate builds as both
  `cdylib` (editor hot-load) and `rlib` (`crate-type = ["cdylib", "rlib"]`); a game binary
  depends on it as a normal crate (editor feature off) and calls `citrus_register`
  directly, with no shipped dylib and no `libloading`.
- [done] **`New Project`** (File menu + `citrus --new-project <parent> <name>`):
  `bundle::scaffold_project` writes a standalone cargo workspace: root `Cargo.toml` (game
  bin + `[workspace.dependencies]` path-pointing at the citrus checkout, found via
  `bundle::citrus_root`), editable `src/main.rs`, `plugins/components` (cdylib+rlib),
  `scenes/ materials/ shaders/ textures/`, a starter scene (camera + lit cube on a plane),
  and `project.citrus` with `boot_scene`. The editor then switches to the new project
  (reloads project file, file browser, plugins, boot scene).
- [done] **Project Settings UI** (File -> Project Settings…): edits `project.citrus`:
  name + a **Starting scene** picker (boot scene), with a Build Game button. Saves on
  change.
- [done] **Build Game** (File menu + `citrus --build [dir]`): `bundle::build_game` runs
  `cargo build --release --bin <game>`, then assembles `build/<game>` + `build/assets/`
  (the asset dirs + `project.citrus`, copied so paths resolve exe-relative). Verified
  end-to-end: a scaffolded project builds and the bundled executable runs standalone
  (window + Vulkan + scene render confirmed).
- [done] **Editor-free runtime path proven**: `examples/sample-game` (detached package)
  links `citrus-engine` + a components crate and runs a scene.

- [done] **Editor stripped from a build**: `citrus-engine` now has a default-on `editor`
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
- [done] **Lean release profile**: the scaffold's generated `Cargo.toml` sets
  `[profile.release]` with `lto`, `codegen-units = 1`, `strip`, `panic = "abort"`. A
  sample game shrank 9.6M (default release) → 5.8M with no behaviour change. Editor is
  unaffected (the profile lives in the generated project).

Still to do:
- **egui stays in the render path** (decided): `citrus-render` always links
  `egui`/`egui-ash-renderer` for its overlay. That adds ~1.5M to a game binary, which is
  acceptable, so it is *not* gated out. Upside: **immediate-mode egui game UI** (2F option
  B) needs no feature flag; it's a matter of `run_game` handing the per-frame egui
  `Context` to a game callback (the egui render pass is already there). The default game
  links egui but never invokes it (`FrameInput.egui = None`).
- [done] **Shader precompilation**: Build Game compiles every custom `.frag` in the copied
  assets to a `.frag.spv` sidecar; `load_shader_file` uses the sidecar when it isn't older
  than the source, so the player's machine needs no `glslc`. Built-in shaders were already
  embedded at compile time.
- [done] **Prefabs in the editor**: drag a `.prefab` from Files onto the viewport to place
  it under the cursor (surface hit, else ground plane), or select it and press "Place in
  scene". Runtime spawning is `ctx.spawn_prefab`.
- [done] **Copy every content folder**: Build Game copied only `scenes/materials/shaders/
  textures`, so models, audio and prefabs were missing from a built game. It now copies
  every top-level project folder except `build/`, `target/`, `src/`, `plugins/` and hidden
  ones; New Project also scaffolds `models/`, `audio/`, `prefabs/`.
- [todo] **Asset collection**: walk the scenes reachable from the boot scene (and their
  materials → textures → meshes → audio → bake sidecars `.lightmap`/`.lightdata`) and copy
  only what's referenced into `build/assets/`. Path resolution switches from
  project-relative to **exe-relative** at runtime (`GameConfig.assets_root` already drives
  this). Dead-asset stripping; a "copy everything" fallback for the first cut. Skip / warn
  on missing assets rather than aborting.
- [todo] **`GameState` (global runtime state / blackboard)**: a project-defined,
  serializable type the runtime owns **outside any scene**, so it survives scene swaps
  (player health, inventory, score, progression, which level is next). Exposed through the
  in-game API (2D) so components read/write it; lives in the project's component crate.
  This is the persistent layer beneath the scene flow: scenes come and go via
  `ComponentCtx::load_scene` (already implemented), `GameState` does not. Distinguish two
  concerns that are *separate*: (a) this in-memory global state, and (b) **savegame
  persistence**, serializing `GameState` (+ optionally live scene object/component state) to
  a save file under an OS user-data dir and restoring it, so a player can quit and resume.
  Save/load is its own in-game API slice.
- [todo] **Remaining build polish**: window icon/size from project config into the
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
  jump, gravity) and for the API's physics queries. Land it first or stub a kinematic
  fallback.
- **Binding system (2C)** has no hard deps; buildable now; controllers consume it, and
  game UI (2F) uses it for keyboard/gamepad navigation.
- **In-game API (2D)** is the backbone; controllers/pawns are its first real clients,
  so grow the API and the Pawn together rather than big-bang.
- **Editor/gameplay split (2E)** is small and unblocks a correct game-build path. Do it
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

- [partial] 3D physics engine (rapier3d): rigid bodies + gravity step + transform
  writeback done (see Physics/collision); joints, layer matrix, queries, triggers todo
- [done] Phase 5 runtime sampling: 5a flat ambient, 5b per-fragment probe SH, 5c
  per-object lightmaps; all sampled in the standard shader (editor + game), sidecars
  bundled. Pending: visual validation of the GPU bake output.
- [partial] Global illumination in the standard shader: probe SH-L1 sampled per fragment
  (done); baked lightmap sampling for static objects still todo
- [todo] HDR skybox + IBL
- [wip] **Full-body IK + VR locomotion** — analytic two-bone + FABRIK solvers
  (`citrus_core::ik`); `humanoid::HumanoidRig` maps Mixamo/VRM/Unity bones and
  `pose_from_trackers` drives any humanoid from head/hands/hips/feet targets.
  Added: **terrain foot IK** (`apply_foot_ik` — hips drop + per-leg two-bone solve
  onto the ground + foot-to-normal roll, pluggable ground sampler;
  `LoadedScene::set_foot_ik`/`ground_height`), **T-pose tracker calibration**
  (`TrackerCalibration` + `humanoid::{calibrate_tpose, targets_from_calibrated}` +
  `LoadedScene::calibrate_vr_tpose`/`vr_apply_calibration` — capture each tracker's
  offset to its bone, then drive naturally), and **VR play-space locomotion**
  (`citrus_core::VrRig`: fly, turn, scale-self, grab-drag the world, pointer ray).
  All unit-tested.
- [wip] **Editor VR** (VR-only; desktop editor untouched) — stereo rendering into
  the HMD (`Renderer::render_xr_eye` per eye + an editor `setup_stereo`/`begin_
  frame`/`end_frame` loop, eye view composed with the `VrRig`), locomotion (fly /
  turn / scale-self / grab-drag via controller sticks+grips, `XrSession::input`
  bound to Oculus Touch), right-hand pointer select+move (`pointer_ray` +
  `LoadedScene::pick`), and **the whole editor UI on a left-hand panel**: a
  dedicated `vr_egui` renderer draws the egui UI into a texture (`render_vr_ui`),
  shown on a quad (`vr_overlay.rs` + `vr_quad` shaders) in each eye with a pointer
  cursor; the right-controller ray → panel UV → egui pointer events, so every
  desktop panel/button is usable in VR (a "VR Tools" window adds the VR-only
  actions). All gated behind an active XR session. Untested without a headset
  (tuning notes in MORNING-NOTES.md §4).
- [todo] VR rendering (OpenXR) + VR editing
- [todo] Slang custom-shader frontend (phase 2)
- [todo] Networking, content pipeline, VRM avatars (milestones M3-M7)
- [todo] Mipmaps, MSAA/TAA, multi-select gizmo, JFA outline
- [done] Baked occlusion culling (`occlusion.rs`, Occlusion tab): the static geometry's bounds split into view
  cells (cell size setting, capped at 32k cells); per cell, sample points (dropped when inside geometry) shoot
  Low/Medium/High (256/1024/4096) rays in all directions and then aim rays at surface points of every object
  still hidden, against a CPU BVH, on low-priority worker threads with progress + cancel. Per-cell bitsets are
  saved as `<scene>.occlusion` and loaded with the scene (editor, play-mode switches, shipped game). Static
  objects not visible from the culling camera's cell skip the main camera pass (`DrawCmd::occluded`; still in
  shadows, probes and captures); dynamic objects and objects added after the bake are never culled; outside
  the baked bounds nothing is culled. The editor Scene view is now frustum culled too. Visualization: cull the
  Scene view, preview from the game camera (fly around and see what that camera's cell draws), show the
  bounds + current cell, and per-cell stats (visible / total).
- [done] Per-scene baked data on every scene load / switch (editor open, play-mode switches, Stop restore,
  shipped game): `.lightmap` + `.lightdata` (lightmaps, probes), `.audiobake`, `.occlusion`, and the baked
  `.reflprobe` cube, read on the loader thread and applied in the post-load steps. The shipped game never
  loaded `.reflprobe` before (Baked-mode probes reflected only the sky); it now does, and captures live once
  when the scene has no baked cube.

## Gameplay / engine subsystems (ENGINE_FEATURE_CHECKLIST, 2026-06-19)

Foundations added to close the T0/T1 gap list. Each is a tested module under
`crates/citrus-engine/src/`; authoring/inspect UI is in **Tools → Systems** (and
**Tools → Audio Mixer**) unless noted. See `ENGINE_FEATURE_CHECKLIST.md` for the
per-feature "what's done / what's left" table.

- [done] **Prefabs** (`prefab.rs`, T0 #7) — `.prefab` RON subtree; `instantiate` clears ids
  (fresh on insert) + applies per-instance transform overrides. Editor: **Object → Create
  Prefab from Selection** (`scene.prefab_from_object` extracts the subtree, re-indexes parents).
- [done] **Physics queries** (`physics.rs`, T0 #12) — `raycast` (returns object index + point +
  normal + distance) and `overlap_sphere`; colliders carry the object index in `user_data`.
- [wip] **Runtime UI** (`ui_canvas.rs`, T0 #13) — retained `UiNode` tree + anchor layout solver
  (stretch / corner-pin / center, nested, button hit-test). Live **preview** in the Systems
  panel. GPU draw of solved rects + a text atlas is the remaining render layer.
- [done] **Audio mixer** (`audio_mixer.rs`, T0 #14) — bus graph (master/music/sfx/voice/ui),
  per-bus volume + mute, `effective_gain` chains up the tree, cycle guard. **Tools → Audio Mixer.**
- [wip] **Animation state machine** (`anim_graph.rs`, T1 #21) — states, param/trigger
  transitions, cross-fade blend, 1D blend trees, `current_pose` clip weights. Still unwired;
  the working path is the `Animator` component below.
- [done] **Animator component** (2026-10-29): plays a named clip per object with
  cross-fades (`ctx.play_animation(target, clip, fade)`, `stop_animation`, `Animator::play`,
  `finished()`), speed, loop, play-on-start; Inspector clip dropdown from the model's clips.
  Each object with an Animator gets its **own skinned vertex buffers**, so spawned copies of a
  model animate independently (previously every instance shared one pose). Clips are tied to
  the skeleton they came from. Objects without an Animator keep the old shared path (first
  clip of their own skeleton, VR humanoid IK). Still todo: blend trees / state graph on top,
  animation events, root motion, GPU skinning.
- [done] **Navmesh + pathfinding** (`navmesh.rs`, T1 #24) — walkability grid, A* (8-connected,
  no corner-cutting), line-of-sight string-pull to natural waypoints. Scene-bake + agent
  component remain.
- [wip] **Particles** (`particles.rs`, T1 #25) — CPU emitter sim (rate/lifetime/cone/gravity/
  drag, recycle cap). GPU instanced-billboard draw + an emitter component remain.
- [wip] **Shader graph** (`shader_graph.rs`, T1 #26) — node graph (const/uv/texture/mul/add/mix)
  → **GLSL codegen** with topo-sort + cycle/ref validation; live output in the Systems panel.
  A visual node editor is the remaining UI.
- [done] **LOD** (`lod.rs`, T1 #29) — distance-banded LOD select + hysteresis (no popping) + cull
  distance. Mesh-swap wiring + component inspector remain.
- [done] **Events / messaging** (`events.rs`, T1 #30) — double-buffered typed event bus
  (`emit`/`signal`/`read`/`swap`), Bevy-style one-frame visibility.
- [done] **Save/load game state** (`savegame.rs`, T1 #31) — typed `SaveValue` KV store, atomic
  `.save` RON write, typed getters.
- [done] **Localization** (`localization.rs`, T1 #32) — `.loc` string tables, `tr()` with
  default→key fallback, runtime language switch.
- [done] **World streaming** (`streaming.rs`, T1 #33) — cell residency with load/unload
  hysteresis band. Async loader wiring remains.
- [done] **Asset streaming handles** (`asset_handle.rs`, T1 #34) — typed ref-counted `Handle<T>`,
  dedupe-by-path, per-frame load budget, GC of unused slots. Real-decoder wiring remains.
- [done] **FPS verification** — `sw_gi.rs::fluxvoxel_per_frame_cost_is_sub_millisecond` measures
  the per-frame FluxVoxel CPU GI cost at ~156 µs for 27k probes + 2 moving lights (~6400 fps
  headroom), and zero when idle (dynamic-hash skip).

## Editor fixes + UI placement (2026-06-19)

- [done] **Mesh-precise object picking** (`scene.rs::pick`) — was ray-vs-AABB, which selected a
  big object (couch) when a small one (orb) sat inside its loose bounding box. Now AABB is the
  broad-phase reject and the actual hit is **ray-vs-triangle** (`ray_mesh` / `ray_triangle`
  Möller–Trumbore) against `mesh_geometry`, so you select the surface you click.
- [done] **Consistent layer UIs** — object layer dropdown, camera culling mask, and collision
  matrix all use `LayerSettings::shown_count()` (highest named + 2, clamped [8,32]) instead of
  32 / 8 / 4. The Layers-window names list shows all 32 (naming a higher layer reveals it
  everywhere). Camera mask now shows real layer names via `InspectCtx.layer_names`.
- [done] **LOD is now a component** — `LodComponent` (citrus-core) with an Inspector editor
  (Add Component → "LOD Group": per-level distance + model path, cull distance). Replaces the
  incorrect Tools-menu placement. See `FEATURE_UI_GUIDE.md` for where every feature's UI belongs.
- [done] **Removed the "Tools → Systems" dump** — it wrongly mixed components, settings, and
  runtime-only APIs. Features now live in their correct homes (components / settings / asset-
  editor windows / runtime-only).
- [done] **Emissive GI on all (non-metal) objects** — the floor glow had been a view-DEPENDENT
  specular term (now gated to smooth metals). The diffuse emissive bounce is injected ~8× into
  the voxel volume (`EMISSIVE_GI_GAIN`) with an 8 m reach, so floor/couch/props get a visible
  VIEW-INDEPENDENT colored pool (chrome/mirror specular deferred per request).

## User-generated content packages (.citruspak, 2026-10-30)

Worlds, avatars and props built in the editor load into a running game without
recompiling it (VRChat-style). Full design + safety model: [docs/user-content.md](docs/user-content.md).

- [done] **`.citruspak` format** (`citrus-assets/src/package.rs`) — zip with `manifest.ron`
  (kind World/Avatar/Prop, name/author/version, engine format version, SHA-256 content hash
  over the file table, required components, stats, thumbnail), `files/` (entry `.scene` + every
  referenced asset, baked sidecars), `texcache/` (BC import-cache blobs re-bound on extract).
- [done] **Builder** — Tools > Build Content Package (World = open scene, Avatar/Prop =
  selected hierarchy): transitive dependency scan of every RON string (incl. component data),
  glTF/FBX externals, model `.meta`/`.lmuv`, scene sidecars; built-in vs plugin vs unknown
  component validation; limits; Analyse / Build report.
- [done] **Avatar Descriptor** component — eye point (camera placement), humanoid bone-map
  overrides (`HumanoidRig::map_with_overrides`), body capsule for the Character Controller.
- [done] **ContentManager** (`citrus-engine/src/content.rs`) in the runtime + editor Play mode:
  path / content id / `http://` (pluggable `ContentFetcher` for HTTPS) on a worker thread,
  validate → extract into a hash-keyed cache (`ContentCache`, source index, LRU prune) →
  decode + transfer-queue upload; worlds switch through `SceneFlow::switch_to_loaded`, props /
  avatars append with fresh ids, avatars attach to the player; unload by origin.
- [done] **Gameplay API** — `ctx.load_content`, `spawn_content`, `spawn_avatar`, `enter_world`,
  `unload_content`, `ContentEvent` (Progress / Loaded / Spawned / WorldEntered / Failed /
  Unloaded) via `ctx.content_events()`.
- [done] **Safety** — data only (whitelisted file types, no `.spv`, shader `#include` rejected),
  per-file SHA-256 + content hash, path sanitizing (outside-package refs blanked, glTF URIs
  confined), per-kind resource limits checked on claims, entry sizes and measured data
  (triangles / texture memory before upload), BC blob validation, unknown components dropped.
- [done] **F8 developer overlay** in built games (`GameConfig::content_overlay` /
  `CITRUS_CONTENT_OVERLAY=1`): inbox folders → Spawn / Wear / Enter, loaded list → Unload.
- [todo] Free GPU meshes/textures per package on unload (today: at the next world switch);
  replicate avatar content ids over the network; sandboxed WASM scripting for content logic.

## Custom shader system v2 + Citrus Toon (2026-10-30)

- [done] **Property declarations v2** (`citrus-assets/src/shader_props.rs`): `// @property name
  "Label" Type = default` with Range, Float, Int, IntRange, Toggle, Enum, Color(HDR), Color3,
  Vector2/3/4, Texture2D (white/black/normal/gray, `@linear`); `@feature` / `@foldout` / `@header`
  groups, `@show_if(expr)` conditions, `@tooltip`, `@preset`. v1 `//! prop` pragmas still work.
- [done] **Per-material std140 block + texture properties**: generated layout and GLSL/HLSL
  declarations, uploaded to a new pipeline set 2 (binding 0 UBO, bindings 1..20 textures);
  values saved by name (`custom`, `textures.shader`), old files load unchanged.
- [done] **Inspector**: presets, foldouts with feature checkboxes, conditional visibility,
  sliders / drags / enum combos / HDR colour pickers / vectors / texture slots with drop +
  tiling/offset, property filter, variant badge, fixed material texture slots, compile errors.
- [done] **HLSL custom shaders** (`.hlsl`, `citrus_frag(CitrusInput)`, auto-combined samplers,
  `packoffset` cbuffer) and shaders from any project folder; hot reload remaps values by name.
- [done] **Keyword variants** (shader_feature model): `@keyword` properties are specialization
  constants; the pipeline key includes the material's keyword set (lazy, cached pipelines).
  Game builds bake the keyword sets their materials use into frozen-constant SPIR-V sidecars.
- [done] **Per-material inverted-hull outline** (`OUTLINE` + `outline_color` + `outline_width`).
- [done] **Citrus Toon** (`shaders/citrus_toon.frag`): Poiyomi-style lighting modes, shadow
  layers, rim, 2 matcaps, toon / anisotropic / GGX specular, reflections, 2 emission slots,
  dissolve, glitter, decal, detail maps, UV tile discard, vertex colour, alpha options,
  parallax; Simple / Medium / Heavy presets. Out of scope list in `docs/shaders.md`.
- [fixed] Custom-shader preamble matched an old frame layout (16 lights, 4-coefficient probes),
  so custom shaders read shadows/probe GI from wrong offsets; now mirrors `FrameUbo`.

## Built-in networking layer (2026-10-30)

Extends 2G. API and model: `docs/networking.md`.

- [done] **Transport**: UDP (or in-memory `MemHub` for tests) with an unreliable channel and a
  reliable ordered channel (seq numbers, batched acks, RTT-based resend, fragmentation past the
  1200-byte MTU). HELLO / WELCOME / REJECT handshake with protocol check and max players, 0.5 s
  pings (RTT), timeout, clean BYE. Fixes the old 512-byte receive buffer that truncated voice.
- [done] **Sessions**: host / join by address, LAN discovery (`LanBrowser`, UDP 47777), player
  list with names / ids / ping, `PeerJoined` / `PeerLeft` / `Connected` / `Disconnected` events,
  `ctx.net_host` / `net_join` / `net_disconnect`, `Network Manager` component (limits, player
  prefab per peer at Spawn Points, dedicated host, auto start), `CITRUS_NAME`.
- [done] **Editor Network window** rebuilt on the widget kit: name / address, Host / Join, LAN
  list, status chip, players with ping, traffic, networked objects with who simulates them;
  connections stay alive while editing; Stop clears replicated state.
- [done] **Object sync**: `Network Object` (ownership Takeover / Request / Host only; transform,
  scale, rigid body, parent and active sync; snap / interpolate / extrapolate with snapshot
  buffers and clock-offset estimation; per-object send rate, still thresholds, field deltas with
  1 s keyframes, quantized 24-bit positions and 32-bit rotations, relevancy distance, despawn on
  owner leave). Host-authoritative: unowned objects are simulated by the host; the host drops
  writes from non-owners. `Sync` maps onto it unchanged.
- [done] **Ownership**: request / release / transfer, `OwnershipChanged` / `OwnershipRequested` /
  `OwnershipDenied` events, owners released (or despawned) when they leave.
- [done] **Rigid body hand-over**: non-authority peers make networked dynamic bodies kinematic;
  regaining authority restores dynamics with the last velocity.
- [done] **Networked spawning**: `ctx.net_spawn` / `net_spawn_server` / `net_clone` /
  `net_despawn` with deterministic child ids and intra-prefab reference remapping; late joiners
  get spawns, owners, parents, active states, variables, last transforms and animator params.
- [done] **Synced variables**: `Network Variables` (typed key / values, owner-write or anyone),
  `ctx.set_net_var` / `net_var` / `net_var_changes`, `VarChanged` events.
- [done] **RPCs**: `ctx.rpc` / `rpc_unreliable` / `rpc_self` / `send_rpc` to All / Others / Server
  / Owner / Peer, received via `ctx.rpcs()` / `rpcs_named` / `rpcs_for_self`; offline loopback.
- [done] **Animator and avatar sync**: `Network Animator` (parameters), `Network Avatar` (head /
  hands / hips / feet poses, optional drive from the local VR rig, replicated `TrackerTargets`).
- [done] **Scene switch for everyone**: `ctx.net_load_scene` (late joiners follow).
- [done] Inspectors for all five components (relevancy sphere gizmo, live owner / authority
  readout during Play).
- [done] Tests: in-memory and UDP loopback sessions (join / leave / timeout / full, ownership
  modes, reliability under 30% loss + fragmentation, variables, RPC targets, spawn + late join,
  owner leave) and two- / three-peer scene tests (transform convergence, ownership moving
  authority, animator / avatar sync, pawn possession, spawn + vars + RPC + late join + despawn).
- [todo] NAT traversal / relay, host migration, client prediction + reconciliation, lag
  compensation, compact network ids, per-avatar IK for remote humanoids, encryption.

## Project templates + gameplay kit (2026-10-30)

New Project offers starter games; each is built from reusable built-in components and plays in
the editor's Play mode as soon as it is created. Code: `crates/citrus-engine/src/templates/`,
`crates/citrus-core/src/kit/`, inspectors in `crates/citrus-editor/src/kit_ui.rs`.

- [done] **Template registry** (`templates::TEMPLATES`: id, name, description, Desktop / VR
  category, card icon, feature list). `scaffold_project(parent, name, template)` writes the
  cargo workspace as before plus the template's scenes, material palette, prefabs, input
  bindings (project.citrus) and sample assets copied from the checkout (sky texture, the helmet
  model); a missing asset is skipped. CLI: `citrus --new-project <parent> <name> [template]`.
- [done] **Empty**: the previous starter scene (default).
- [done] **First Person Shooter**: walled arena with cover, ramp and platform, physics crates,
  target range; FPS controller (mouse look, WASD, jump, sprint, crouch), hitscan rifle with
  reload, muzzle flash and impact particles, health with regen, crosshair / health / ammo /
  score HUD, Escape pause menu (Resume / Settings / Restart / Quit). Target dummy prefab.
- [done] **Third Person Shooter**: same arena; capsule character, over-the-shoulder camera on a
  boom that pulls in at walls, aim (right mouse) zooms in and strafes, weapon, HUD, pause.
- [done] **Top-down click to move**: follow camera with wheel zoom, NavMesh Surface (characters
  on their own layer so they don't block the bake), click marker, a lever that opens a gate
  (rebakes the navmesh) and a lamp switch.
- [done] **Strategy (RTS)**: RTS camera (edge / WASD pan, wheel zoom, Q / E rotate, bounds),
  eight units with selection rings, box and click selection (Shift adds), right-click moves in
  grid formation with a move marker, Gold / Wood resource bar with income and a selected-count
  readout. Unit prefab.
- [done] **VR Shooter**: VR Player rig (head camera + hands), pistol and blaster that snap into
  the hand and fire on the trigger, throwable balls, teleport arc + smooth move + snap turn,
  targets, wrist HUD (score / ammo) as a world-space canvas, instructions board.
- [done] **VR Social**: room with lights, planar-reflection mirror, avatar body that follows the
  head (head shell, torso), world-space welcome board with working buttons, four `player` spawn
  points (picked by peer id), synced grabbables (`Sync` + `Grabbable`, ownership requested on
  grab), Voice push-to-talk binding.
- [done] **Gameplay kit components** (registered built-ins with inspectors and gizmos): Health,
  Hitscan Weapon, Target Dummy, First / Third Person Controller, Follow Camera, Click To Move,
  Interactable, RTS Camera, RTS Selection, Selectable, Resource Store, HUD Binding, VR Player,
  Grabbable, VR Avatar.
- [done] **VR for gameplay**: `ScreenInfo::vr` (`VrFrame`) carries the headset / controller state
  to components in the runtime and editor Play; in editor Play a VR Player drives the headset
  rig. Without a headset the VR Player simulates one (mouse look, WASD, look-and-press-Grab,
  left mouse = trigger, T = teleport).
- [done] **Editor Play input**: while the viewport shows the game camera, right mouse and the
  wheel go to the game (RTS orders, zoom) instead of the fly camera.
- [done] New Project dialog: template cards with Desktop / VR filter (see editor-ui-guide).
- [done] Tests: every template scaffolds, its scenes / project / prefabs load and every component
  deserializes with all object references resolving; headless weapon damage through physics,
  HUD binding, FPS movement; unit tests for formation / selection rect / camera pose / edge pan /
  boom / teleport arc / spread / health / reload math.
- [done] **Crouch collision**: First Person Controller `crouch_height` shrinks the character
  capsule while crouched (feet stay planted) and only stands up with headroom (five upward
  rays); physics reshapes the capsule when the Character Controller changes at runtime. Test:
  `crouch_shrinks_the_capsule_and_waits_for_headroom`.
- [done] **Prefab references survive instancing**: `Prefab::instantiate` / `instantiate_rooted`
  give fresh ids and rewrite references between prefab objects in component data (editor drop,
  `ctx.spawn_prefab`, network spawns). Template prefabs keep their ids so this works (spawned
  RTS units get their selection ring).
- [done] **VR Social networked avatars**: a Network Manager (session "VR Social", spawn tag
  `player`) spawns `prefabs/net_avatar.prefab` per player. Network Avatar gained **Follow
  Local Player** (the owner's copy sits on the scene's VR Player rig and copies its head / hand
  poses, VR or desktop fallback) and **Hidden For Owner** (its own visuals are disabled locally),
  so peers see head, hands and torso without the owner seeing a double. Test:
  `vr_social_avatar_prefab_mirrors_the_local_player_for_others`.
- [todo] VR left trigger is not read by citrus-xr yet.

## Bone cloth + cloth colliders (2026-10-30)
- [done] **Bone Cloth** (`citrus_core::cloth`): position-based secondary motion for joint chains
  (hair, tails, ears, bone-rigged skirts). Pick root joints; every joint below simulates, with an
  optional virtual tail so the last bone swings. Connection modes: independent chains, or
  neighbouring chains linked as a loop / open strip (a root with several children is sorted
  around its axis into a ring). Gravity, damping, stiffness toward the animated pose (root / tip),
  angle limit, link stiffness, blend with the animation. Collision with per-particle radius
  (root / tip), friction and edge collision (segments between particles, so colliders can't slip
  through). World inertia, max speed (faster locomotion carried rigidly), teleport distance /
  angle with Reset or Keep shape. Fixed-rate sub-stepping (Frequency, Iterations). Runs in the
  skinning pass for Animator models (CPU and GPU skinning) and shared / full-body-IK avatars,
  in edit mode and Play.
- [done] **Cloth Colliders**: spheres, tapered capsules (separate start / end radius) and planes,
  each following a joint (or the object). Bone Cloth uses the object's colliders (all, or a
  ticked subset) plus colliders on other listed objects.
- [done] **Auto-fit capsules**: presets Body (hips, legs, arms) / Legs / Arms / Selected bones.
  Each bone's capsule runs from the joint to the child its vertices lie toward (principal axis
  when there is none) with radii from a coverage quantile of the bind-pose vertices it dominates,
  extrapolated to the ends; Inset grows or shrinks every radius. Refitting replaces only
  auto-fitted colliders; editing one by hand keeps it.
- [done] **Gizmos**: collider wireframes (selected one highlighted, with start / end handles and
  radius handles), and the live simulated chains, chain links and particle radii.
- Tests: `citrus-core` cloth (hanging, stiffness, sphere / edge collision, teleport, inertia,
  ring links, capsule fit, collider frames), `citrus-engine`
  `auto_fit_wraps_the_thigh_and_cloth_slides_off_it`.
- [done] **Wind Zones** (`Wind Zone`: global / sphere / box, strength, turbulence, gust
  frequency, gizmo) push Bone Cloth and Mesh Cloth (`wind` scale on each) and raise wind seas on
  Water.
- [done] **Mesh Cloth** (`citrus_core::cloth_mesh`): a skinned submesh simulated from its own
  triangles: seam welding, optional particle spacing (reduction), stretch / bending /
  keep-shape / tether constraints, pins from a vertex colour channel, bones or the top of the
  mesh, collision with Cloth Colliders, self-collision (spatial hash), inertia and teleport like
  Bone Cloth, rebuilt normals; CPU skinning path. Inspector and particle gizmo.
- [done] **VRM spring bones**: `.vrm` imports as a model; `VRM.secondaryAnimation` (0.x) and
  `VRMC_springBone` (1.0) become Bone Cloth chains and Cloth Colliders on the skinned object
  when the model is added to a scene.
- [todo] In-editor pin painting for Mesh Cloth, distance culling of cloth, GPU cloth.

## Full-body tracking, remote IK, networked avatars (2026-10-30)
- [done] **Tracker roles**: `TrackerTargets` covers head, hands, hips, feet, chest, elbows and
  knees (`TRACKER_SLOTS` = 11, `slots` / `from_slots` / `transformed`); citrus-xr binds the Vive
  tracker chest / elbow / knee roles next to waist and feet.
- [done] **Full-body solver** (`humanoid::solve_body`, VR-avatar style): pelvis from the hip
  tracker or estimated under the head (turned by its yaw), spine bent toward the chest and head
  and twisted with the head, shoulders turning toward reaching hands, two-bone arms / legs with
  elbow / knee trackers or body-relative bend goals, tracker rotation on head / hands / feet /
  chest, and without foot trackers planted feet (world space) that step when the body moves or
  turns away. Works on top of the Animator pose with a weight. `HumanoidRig` gained chest, neck
  and shoulders (also bone-map roles).
- [done] **Calibration**: T-pose capture lines the avatar up under the headset; uncalibrated play
  uses a default calibration (ideal headset / controller poses) so head and hand rotations map
  sensibly. `ctx.calibrate_trackers()` / `ctx.clear_tracker_calibration()`; the runtime now
  applies calibration like the editor.
- [done] **`Humanoid IK` component**: source Auto / Local trackers / Network / Objects, tracking
  root, weight, spine twist, shoulder rotation, foot planting and step tuning; per-object skinned
  instance (no Animator needed); Avatar Descriptor bone maps apply. Inspector and a gizmo of the
  live targets.
- [done] **Remote humanoid IK**: the avatar packet carries all 11 roles (protocol 3); owners send
  their calibrated targets; remote copies pose their Humanoid IK from them.
- [done] **Networked UGC avatars**: wearing a package sets the player's Network Avatar `wearing`
  (content id + URL), replicated as the `citrus.avatar` synced variable with late-join catch-up;
  peers load and wear it. Worn avatars get Humanoid IK on their skinned parts automatically.
- Tests: `three_point_tracking_reaches_head_and_hands_and_crouches_on_planted_feet`,
  `foot_and_hip_trackers_drive_the_legs_and_knee_trackers_aim_them`,
  `planted_feet_hold_then_step_after_the_body`, `default_calibration_maps_an_ideal_headset_onto_the_head_bone`,
  `humanoid_ik_picks_network_targets_for_remote_avatars_and_local_ones_otherwise`,
  `worn_avatar_package_reaches_peers_and_late_joiners`, `remote_players_announced_avatar_is_requested_once`,
  plus chest / knee replication in the network convergence test.
- [done] Command palette: the list no longer stays at the height of an earlier search.
- [todo] Avatar scaling to the player's height, finger tracking, a calibration UI in the VR
  Player kit, legacy global IK path (humanoids without Humanoid IK) to retire.

## Face tracking, SlimeVR, voice v2, lip sync, render fixes (2026-10-30)
- [done] **Blendshapes**: glTF morph targets (names from `extras.targetNames`) and FBX blend
  shapes (ufbx channels remapped onto triangle corners) import as sparse offsets; skinned
  objects with Face Tracking / Blend Shapes / Lip Sync apply them before CPU skinning.
- [done] **Face tracking** (`citrus_core::face`): Unified Expressions frame (102 base shapes,
  blended shapes, ARKit and VRM aliases, gaze), `XR_FB_face_tracking2` mapping, OSC parameter
  decoding (v2 / FT/v2 / combined signed parameters). Headset read directly in citrus-xr
  (`face_weights`, `eye_gaze`); opt-in OSC receiver (`face_input.rs`). `Face Tracking`
  component (source Auto / Local / Network, smoothing, eye bones, mappings, live matched list),
  `Blend Shapes` (sliders). Faces replicate (P_FACE). Worn avatars get Face Tracking and Lip Sync.
- [done] **SlimeVR direct** (`slimevr.rs`): connects to the server's driver bridge socket
  (`SlimeVRDriver`, protocol 2, schema read from the installed server), announces the headset
  and controllers, streams their poses and fills body tracker slots by role. No SteamVR needed.
- [done] **Voice v2**: Opus via runtime-loaded libopus (PCM fallback), adaptive jitter buffer
  (RFC 3550 jitter, 40 to 300 ms target, FEC recovery, concealment, quiet-frame catch-up),
  stereo positioning at the speaker's head, `Voice Chat` component (proximity / global, rolloff,
  voice activity / push to talk / always, gain, bitrate). Protocol 4.
- [done] **Lip sync**: per-frame viseme analysis (loudness gate, formant peaks, fricative
  energy, onsets) of the local mic and of every voice as it plays; `Lip Sync` component (when
  untracked / always / off, gain, smoothing, face-shape fallback).
- [done] **Command palette** keeps its height after a search; **bloom** drops NaN / Inf samples
  and the standard shader never writes them; **sphere-light specular** (5 cm source radius,
  sun angular size) keeps mirror highlights finite.
- [done] **Flux emitter shadows** are a world-space distance-field cone trace per pixel instead
  of per-probe visibility bits (the shadow under an emitter no longer slides with the camera).
- Tests: face (names, OSC, FB mapping, wire form), face_drive (tracking + authored + lip sync),
  face_input (OSC parsing, receiver), slimevr (messages, framing, live socket), voice_codec
  (Opus round trip), viseme (vowels / silence / hiss), voice jitter buffer (buffering, jitter
  growth, reordering, concealment, late drops), face replication in the network test.
- [todo] Flux: probe placement jitter + plane-aware integrate weights (review notes); HTC
  facial tracking; OSCQuery advertising for VRCFaceTracking auto-discovery; GPU morph path;
  HRTF voice through Steam Audio.

## Smaller scenes, generated LODs, VRM models (2026-10-30)
- [done] **Compact scene files**: a material still exactly as its model / primitive imported it
  saves as `material: Model` (no inline copy; 80 % of a typical scene file was these). Model
  instances save as their root plus changes: untouched parts that nothing references are left
  out and rebuilt from the model on load with stable ids, deleted parts are listed
  (`model.removed`), edited parts stay as entries tagged `model_part`. Older scenes load as before
  and shrink on their next save. Geometry was never stored in `.scene` files; the large files
  next to a scene are the bakes (see BC6H in OUTSTANDING_FEATURES).
- [done] **Generated LODs**: `citrus_assets::simplify` (quadric-error edge collapse, borders and
  UV seams locked, flip-safe, attributes and skin weights kept). A LOD Group level with no model
  and a Keep percentage builds its meshes from the object's own model at load.
- Tests: `pristine_parts_drop_out_and_come_back`, `edited_materials_and_components_keep_a_part_stored`,
  simplify (sphere halves keeping its silhouette, flat grid collapses with its border locked),
  VRM spring parsing.

## Water system, multi-select editing, grouping, asset cook (2026-10-30)
- [done] **Water bodies** (`Water`): Ocean (endless), Lake (rectangle or drawn polygon), River
  (spline points with width, depth and flow speed; the surface follows the points' heights, waves
  calm toward the banks). `column()`, `height_at()`, `flow_at()`; inspector with point lists and
  draggable point gizmos.
- [done] **Waves**: Gerstner sets plus a wind sea from Wind Zones that builds over time, limited by
  fetch and depth; depth-aware dispersion (`omega = sqrt(g k tanh(k d))`).
- [done] **Water physics**: `Buoyancy` (per-point lift centred slabs, drag, angular drag,
  current and river flow), `ctx.water_surface(p)`.
- [done] **Interactive surface** (`RippleSim`): a height-field wave simulation in a window that
  follows the listener; floating bodies push it (falling in, wakes), it absorbs at its border and
  adds to `height_at` so bodies ride each other's wakes.
- [done] **Water rendering** (`FEAT_WATER`, `water_render.rs`): Gerstner displacement and normals
  in the standard shaders from the frame UBO (`water_bodies[4]`, play clock), flow-mapped ripples,
  Fresnel reflection over env / probes / planar, refraction and depth absorption from an opaque
  colour + depth copy taken mid-pass only on frames that draw water, shore and crest foam; meshes
  per body type (camera-following ocean grid, lake grid / polygon, river ribbon). VR eyes and
  preview cameras get waves without refraction.
- [todo] Water: sample the ripple field in the shader (GPU upload), shoreline depth bake (wave
  damping near shore), underwater fog / waterline / caustics, river bend mitering, freeing old
  water meshes, planar reflections on water.
- [done] **Multi-object editing**: fields that differ across the selection show an em dash (header
  name, per-axis transform, component fields matched by label); typed transform values set that
  axis on every selected object; component edits copy only the changed fields (they used to
  overwrite whole components); header active / static / layer / name apply to all. Gizmos, light
  cones and billboard highlights draw for every selected object.
- [done] **Group Into Empty** (Scene tree context menu): selected objects move under a new
  "Group" empty at their centre, under their shared parent, world transforms kept.
- [done] **Asset cook**: builds ship only what scenes, prefabs, `resources/` and paths named in
  `src/` reach (dependencies traced through every asset), with a size report.
- [done] **Capsule Collider** (X / Y / Z axis, height includes the caps) and Create / Add menu
  entries for Capsule and Mesh colliders, Water and a Floating Cube.
- [done] **Physics materials** (`.physmat`): friction and bounciness with combine modes (Average /
  Minimum / Multiply / Maximum), density (mass for bodies with Mass 0), buoyancy and water-drag
  multipliers, contact offset; presets (wood, metal, rubber, ice, stone, foam). Slots on Rigid
  Body and every collider (collider overrides body overrides project default). File browser
  "New Physics Material" and a file inspector.
- [done] **Project Settings > Physics**: gravity, default physics material, contact stiffness (Hz)
  and damping, solver iterations; used by editor Play and built games. Stiffness is solver-wide
  (rapier has no per-contact softness).
- [done] **Water reacts to every body**: dynamic Rigid Bodies float by Archimedes from their real
  density (mass over collider volume, or the physics material's density) scaled by the material's
  buoyancy and water drag; `Buoyancy` became an optional override. Any body crossing the surface
  pushes ripples; solid colliders through the surface (posts, walls, sunken boxes) block the
  ripple field so waves reflect off them, and water flowing past them sheds alternating eddies at
  the Strouhal frequency plus a bow push; fast entries raise splash events.
- [done] **Water effects** (`water_fx.rs`, one CPU particle pool, single draw): splashes (crown
  spray, foam, mist on hard hits), wake foam and spray from bodies moving through water, foam
  advected by current and vortices. Ripples are the height-field simulation only (no ring
  sprites): bodies and landing waterfall streaks push it, in Play and in the editor (so plunge
  pools churn while authoring).
- [done] **Waterfall** component and builder gizmo (lip handles, exit-speed handle, free-fall arcs):
  a sheet of velocity-stretched streaks poured off the lip lands on water or the ground below
  and bursts into spray and mist, foaming the surface; its plunge pool swirls (vortex fed to floating bodies).
  **Whirlpool** component (radius, strength, pull, direction). Create > Water menu entries.
  Runs in the editor, so waterfalls preview while authoring.
- [done] **River path builder** (Water > River > Edit Path): click to add points on the surface
  under the cursor (mesh raycast, plane fallback), Shift+Click inserts into the nearest segment,
  Ctrl+Click adds upstream, clicking the centre line splits a segment in place; preview point
  with connection lines and hint bar; drag points and width handles; Delete removes the selected
  point; Esc / deselect exits; inspector rows sync with the viewport selection; undoable.
- [done] **Asset slots**: every inspector file slot (physics materials, post profiles, animator
  controllers / clips, audio clips, prefabs, sounds) has a browse list of matching project files
  and, where a template exists, a [+] that creates a new asset and assigns it; Active Material and
  skybox slots got browse buttons too.
- [done] **Wave Rider** component: objects ride the water without a physics body (Float at a draft
  within reach of the surface, or Suspended at their own depth with orbital motion decaying as
  e^(-k depth)); per-wave footprint filtering by size, weight damps the horizontal swing, tilt with
  the surface, Stokes drift past a steepness threshold (owner moves the anchor); engine-filled
  `state` (on_surface, in_water, depth, surface height, offset). Offsets are draw-only
  (`render_world_transforms`), so the synced transform stays the anchor.
- [done] **Shared water clock** for multiplayer: `NetSession::shared_clock` (host clock, RTT
  corrected), `PlaySession::water_time` follows it smoothly; physics, rendering, effects, riders
  and `ctx.water_time` / `ctx.water_surface` all use it.
- [done] Inspector: Add Component focuses its search (cleared each opening); Mesh & Material is a
  collapsible section, closed by default.
- [done] Sliders use the bright accent for the filled track and knob (`widgets::slider`); the
  Project Settings gear is a proper cog.
- [done] Water in RT reflections (water BLAS rebuilt in-frame when its mesh changes, instance masks
  keep it out of shadows / GI, water hit shading with waves and ripple normals) and the ripple
  field rendered on the surface (RGBA16F height + slope texture per water, uploaded without
  stalls, vertex height + per-pixel normal).
- [done] **Smooth rivers**: a centripetal Catmull-Rom curve through the river points
  (`Water::river_path`, cached until the points change) drives the surface mesh, the physics
  column (height, depth, flow down the slope, bank distance) and the editor gizmo and builder.
- [done] **Underwater** (`underwater.frag`, `UnderwaterSettings` on Water): view fog toward the
  water colour by scene depth with red absorbed first, darkening with depth, soft caustics on
  up-facing surfaces, screen wobble, a per-pixel near-plane waterline; Scene view, Camera tab,
  built game and VR eyes. Listener under water muffles every voice (two-pole low-pass sliding
  18 kHz to 500 Hz plus a duck, 0.2 s fade) and loops the water's selectable underwater sound.
  Inspector "Underwater" section (sound, volume, muffle, fog density, distortion, caustics).
- [todo] Underwater: no Snell's window from below; caustics only while the camera is under; the
  post wobble's mask is still a flat line at the camera.
- [done] RT reflections trace the moving water: a compute pass writes the displaced surface (Gerstner
  sum, ripple field, bank calming) into per-frame-slot buffers and the water BLAS is refit each
  frame (full rebuild only when the mesh changes); runs only for water while RT reflections are on.
- [todo] Wind sea state is per client (builds from local history). The wave sum is duplicated in
  standard_common.glsl, rt_reflect.comp and water_displace.comp; keep them in sync.
- [done] **Mesh Collider shapes**: static = exact triangles; moving bodies = one hull simplified to
  64 corners (farthest-point sampled); "Follow shape" splits a concave mesh into up to 12 pieces of
  24 corners each (skipped when the mesh is already ~convex); Convex = a hull even when static.
  Shapes are cached on the scene; the green gizmo draws only fold and border edges (no face
  diagonals). The collider's material slot is labelled Physics Material.
- [done] Underwater view follows the waves: the pass gets the camera's water body (waves,
  transform, rest surface, clock) in a small uniform block and evaluates the Gerstner surface per
  pixel for the waterline, the fog's surface cut and depth darkening.
- [done] Adding a Rigid Body in the inspector adds a collider when the object has none (a Mesh
  Collider on mesh objects, else a Box Collider).
- [done] Box / Sphere / Capsule Colliders added in the inspector fit the object's mesh bounds
  (its own meshes and its children's, in its local space); flat meshes get a thin slab under the
  surface; capsules pick the longest axis.
- [done] Asset slots holding a physics material or a post FX profile show an "Edit <name>"
  toggle under the field that opens that asset's full editor inline (saved to the file; volumes
  update next frame), via `InspectCtx::asset_editor`.
- [done] Buoyancy stability: lift capped at 6x a body's weight plus extra heave damping for very
  light floats (a 1 kg couch-sized body bobs instead of launching); Rigid Body mass defaults to 0
  = Auto (collider volume x physics material density, 500 kg/m3 without one), shown as "Auto".
- [done] **Asset references by id** (`citrus_core::asset_ref`): slots store `@<id>` from the
  asset's `.meta` sidecar (created on first assignment), resolved through a per-project index of
  the sidecars (rescanned on a miss), so renames and moves keep every reference. The file browser
  hides `.meta` files and carries them along on rename / move / delete; copies get fresh ids.
  Plain paths in older files still resolve. Loaders resolve refs (physics materials, post
  profiles, audio, animator controllers / clips, prefabs, particle and UI textures, material
  textures, material files, skybox); the build cook follows refs and ships their sidecars
  (and now ships `.physmat`).
- [done] **Asset slot chips**: every asset slot (component slots, the object's material slot,
  material texture slots) is a read-only chip with the file's icon and name; click shows and
  highlights the file in the file browser, double-click opens it in the inspector; drop, browse
  and [+] new still assign.
- [todo] Skybox image slots in Environment still show paths (browse works; not chips yet).
- [done] **GPU shallow-water simulation** (`water_sim.comp`, `citrus-render/src/water_sim.rs`,
  CPU reference `citrus-core/src/water_pipe.rs`): virtual-pipe model per interactive Water (up to
  4), 256^2 default, fp32 state (column height, 4 outflow pipes, velocity, per-cell rest depth /
  base flow / water-shore-obstacle). Window covers small bodies whole (shores reflect, pools
  slosh) or follows the camera with an absorbing border (toroidal storage, only new cells reset).
  CFL substeps, wave speed from min(depth, Wave depth cap 3 m), mass kept. Bodies displace their
  submerged volume (bows, troughs, rings) and drag water by their relative speed; waterfall
  impacts and wakes add dents and mass; fixed colliders are obstacles; rivers carry disturbances
  and pile up against rocks. Writes the ripple texture (height, gradient, flow speed) directly,
  so raster, RT reflections and RT displacement show it; async readback (1-2 frames late, no
  waits) feeds `height_at` / `flow_at` (buoyancy, riders, foam, `ctx.water_surface`) and drags
  floating bodies with moving water. CPU wave-equation fallback without a renderer. Inspector
  "Simulation" section. Idle water stops stepping.
- [done] **Dispersive deep-water layer** (eWave, `water_fft.comp`, CPU reference `water_ewave.rs`):
  height + velocity potential on the pipe sim's window, Stockham FFT row/column passes, exact
  energy-preserving per-mode rotation with w^2 = (g k + s k^3) tanh(k d) (capillary term), sponge
  / shore masking, long-wave handover to the pipe sim in shallow water, strength by depth
  (`dispersive_share`), "Dispersion" inspector field. Impacts become wave trains (short waves
  trail long ones), moving bodies leave ~19.5 deg Kelvin wakes; orbital velocity reaches
  `flow_at`. Power-of-two resolutions only.
- [done] Caustics on every surface under water, seen from above too (forward pass, per-body
  transform + extent + strength in `GpuWater`); the underwater pass no longer adds its own.
- [done] Underwater blur (post pass disk blur below the waterline, "Blur" setting) and denser
  default fog with a light floor so murk reads as haze in dim scenes.
- [done] Splashes are displacement: a fast entry (> 2.5 m/s down after 0.2 s clear of the water)
  punches a cavity into the simulation, which throws the crown and rings; only hard hits
  (> 4 m/s) add a few droplets; splashes and wakes no longer spawn white foam blobs. Bodies held
  in place (static, kinematic, editor) displace against the rest level, so passing waves don't
  pump rings out of them.
- [todo] Water simulation: foam from flow speed / convergence; terrain heightfield bed (beaches,
  shallows, shoaling); half-resolution readback; release old ripple textures; GPU smoke test of
  barriers and descriptors; per-object Cast Shadows (bulb meshes); authored UCX_ collision.
- [done] Mesh Collider **Detail** (Low / Medium / High): hull corners 64 / 128 / 256, Follow-shape
  pieces 8 / 16 / 32 of 24 / 48 / 96 corners. Ball-shaped meshes (any tessellation) become an
  exact sphere. Adding a Rigid Body to a primitive or a ball-shaped mesh adds its exact collider
  (Sphere, Capsule, Box for cubes and planes) instead of a Mesh Collider.
- [done] Water simulation in the editor (not playing): Rigid Bodies and characters crossing a
  GPU-simulated water displace it from their collider bounds, moving at the speed they're dragged
  (`editor_water.rs`), so dragging a body through water pushes waves and sloshes pools.
- [done] Selecting an object in the viewport reveals it in the hierarchy: collapsed parents above
  it expand and its row scrolls into view.
- [done] Transform gizmo: Shift moves / rotates at half speed for fine placement; Ctrl (or Snap)
  snaps moves onto absolute grid lines (only the axes being moved; small steps accumulate until
  they cross a line), rotations / scales to their steps.
- [done] Saving the scene during Play (or paused) is refused with a status message; play-time
  changes are never written.
- [done] Rigid Body mass is an explicit Auto / Set choice (Set notes it overrides the material's
  density); in Play the inspector shows the simulated mass, collider volume, resulting density and
  whether it floats or sinks.
- [done] Transform Scale link toggle (lock icon): while linked, editing one axis scales the others
  by the same ratio; the toggle is remembered.
- [done] Water look fixes: still water idles flat (the following sim window now starts new cells level
  around bodies instead of collapsing a mound into rings; heavy resting bodies use the rest level;
  wakes ignore the sim's own sloshing); graded ocean grid (>= 192 quads, 0.25 m at the centre)
  shifted by whole cells; per-vertex spacing band-limits Gerstner waves and the ripple field
  (ripple texture now has GPU-built mips) so the mesh stays smooth and detail lives in per-pixel
  normals; RT displacement uses the same band limit; thickness-based opacity fade softens the
  waterline; normal-variance roughness removes specular sparkle.
- [done] **Physical caustics**: 1/|det(I + k Hessian)| from the Gerstner and simulated surfaces along
  the refracted sun direction (set-0 binding 10 holds each body's ripple texture); visible from
  above and below; Caustics 1 = physical (default).
- [done] **Physical refraction**: Snell's law through the per-pixel normal, followed down the water
  thickness with two depth refinements, screen-edge and total-internal-reflection fallbacks;
  Refraction 1 = physical (default). Older scenes keep their saved 0.03 (set it to 1).
- [done] GPU crash diagnostics: markers at every profiler zone begin/end plus skinning, texture
  uploads, water simulation, underwater, RT / SSR reflections and bloom. NVIDIA:
  VK_NV_device_diagnostic_checkpoints (last checkpoint per pipeline stage). AMD: VK_AMD_buffer_marker
  into a host-coherent buffer (last marker started vs finished). Plus VK_EXT_device_fault where
  supported (driver fault description, faulting addresses, vendor codes). All logged once on a lost
  device (render, skybox install); startup logs which are available.
- [done] Waterfalls render as a solid sheet: a double-sided lit, alpha-blended ribbon along the fall
  arc (widening with spread, breaking up at the sides and toward the plunge) with procedural streak
  colour/normal maps scrolling at the water's speed; the sheet particles are no longer drawn and
  only drive the landing splash and pool displacement. Water objects get their own hierarchy
  type and wave icon.
- [done] Water foam: two-layer contact foam (dense band against whatever the water touches, patchy
  lace out to Foam width, lapping in and out), whitewater where the simulated flow runs fast, crest
  foam; all drawn as a cellular bubble network (jittered-cell edges at two scales) carried by the
  flow with a flow-map cross-fade. Foam and fine ripples advect with the world-space flow the vertex
  shader decodes.
- [done] Box gizmo resize (water, colliders, volumes, probes): dragging a face moves only that face,
  the opposite one stays fixed in the world (the object shifts when its box is centred on it); hold
  Alt during the drag to resize both sides about the centre. Whitewater foam is depth-relative
  (Froude number) so shallow sloshing stays clear.
- [done] Foam amount per water (Look > Foam, 0 = off) scales every foam layer. Waterfall plunges
  and whirlpools foam on the surface itself (churned spots, thick in the middle, lacing out and
  carried outward) instead of floating foam sprites; mist is thinner.
- [done] Pool water body: shaped like a lake (rectangle or polygon), switching to it applies a
  still, clear, pool-blue look with no waves, wind sea or foam. Steam (0 = off) rises off warm
  water (hot tubs, hot springs) and drifts with the wind.
- [done] Caustics per light: every light refracted through a surface focuses on what lies under it
  (point lights by their own direction, not only the sun), and baked / Mixed lights on lightmapped
  floors add the caustics as a change to their baked light. Project caustics (on for pools,
  Projection strength, 1 = physical): lights under the surface throw the moving pattern out onto
  walls and ceilings, along the refracted path and with the Fresnel loss at grazing angles.
- [done] Waterfall sheets and spray draw after every water surface (fixed render queue), and
  transparent draws sort by their bounds' world centre: world-space meshes all sorted as the origin,
  so the sheet blinked in and out as the camera moved.
- [done] Bodies moving through water make a wake, not ripple rings: the water under a body is
  dragged only by the share of the column it fills (and slower), so it no longer shoves a bulge
  ahead (the bow is now about a real hull's v^2/2g); body footprints are softened over
  neighbouring cells and round ones rise gently at the rim, so sliding across the grid no longer
  fires a ring per cell; a moving body starts dispersive waves at 35% (drops and pushes keep full
  strength). Editor drags smooth the body's speed (0.1 s) and cap it at 4 m/s.
- [done] Bodies in the water block the dispersive ripples (by draft, full past 8 cm), so ripples
  reflect off a floating couch or boat instead of running underneath; long waves still pass under.
- [done] Wake vs ripples: a body's push on the dispersive layer is spread over Ripple smoothing
  (default 0.06 s; it moved once a frame and kicked grid-sized ripples all round it at frame
  rate), the blocking under it follows the same smoothed footprint (so what it pushes in ahead and
  lets go behind cancel instead of piling up energy), and the finest grid waves are damped by
  Fine ripple damping (default 6/s, was a fixed 1.5): about 7x less grid-scale roughness for the
  same wake. Water inspector: Wake ripples, Ripple smoothing, Fine ripple damping.
- [done] Fluid viscosity (Pa s) and templates: Water, Sea water, Hot spring (steams), Swamp, Oil,
  Syrup, Honey, Mud set density, viscosity and the look (colour, clarity, roughness, ripples, foam,
  refraction) as a starting point. Viscosity damps waves at 2 nu k^2 (thick fluids keep only long,
  slow waves), slows flow over the bed (3 nu / h^2), and drags bodies by the sphere drag curve
  (Stokes drag in thick fluids, form drag in thin), with Stokes rotational drag; what floats is
  set by density alone.
- [done] Water and colliders: every static object (its colliders, or its render mesh without any)
  and every still collider through the surface is a wall the simulated water reflects off, in the
  editor and in Play (`WaterWalls`); closed shapes fill their cells, triangle meshes (open pool
  walls) mark where they cross the surface. Moving bodies push the water; a collider without a
  Rigid Body pushes it while being moved (dragged in the editor, animated in Play) and is a wall
  again once it stops. Only objects whose layer collides with the water's (Layers matrix) count,
  for walls, pushes, buoyancy and drag.
- [done] Inside a body's footprint the water is drawn flattened (no free surface there, only the
  pressure under it), so a bobbing sphere no longer has water poking up through it; the readback
  that lifts the body keeps the real value.
- [done] Bodies fill at most the water column under them: a ball resting on a couch in shallow
  water (together thicker than the water is deep) raised a standing spike, since the empty column
  under them could never let the extra head drain.
- [done] Collider physics materials are an opt-in override on objects with a Rigid Body
  ("Override material"; the body's material otherwise); static colliders keep a plain material slot.
- [done] Lake / Pool outlines: Shape presets fitted to Size (Rectangle, Circle, Oval, Rounded),
  Smooth outline (closed curve through the points, for springs and kidney pools), Add point
  splits the longest edge; drag the points in the viewport to fit a pool mesh or spring.
- [done] **Water Container** component (carryable water): an inner box with the +Y face open holds a
  level of water that sloshes with the container's motion and spills over the lowest rim as a
  stream of drops; drops landing in another container's opening fill it, in a lake push its
  surface. Dipping the opening under a water surface scoops to the outside level; waterfall
  streaks into it fill it. The water's mass rides on the Rigid Body at its centre of mass, so a
  fuller floating bucket sits lower (containers count their hull plus hollow in buoyancy).
  Create > Water > Bucket spawns an open bucket mesh, wall colliders, Rigid Body, container and
  Grabbable. Gameplay: `ctx.container_water(id)`, `ctx.fill_container(id, litres)`,
  `ctx.pour_container(id, litres)`.
- [todo] Separate static / dynamic friction (rapier has one coefficient; needs a contact hook).
- Tests: water bodies, wind sea, ripple ring / follow, floating / sinking / current, packed GPU
  waves vs physics, water meshes, RON field merge / mixed detection, grouping, asset cook,
  capsule rest height, material bounce, density mass and buoyancy scale, `.physmat` round trip.

