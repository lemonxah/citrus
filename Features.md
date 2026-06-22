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
- [done] Camera preview tab (renders scene from the main camera to an offscreen target)
- [done] **Startup splash** (`splash.rs`): a borderless CPU-drawn window (softbuffer) shown
  before the Vulkan renderer exists, with a scaled background + bottom status line (ab_glyph).
  Shows the embedded `assets/splash.png` **instantly** as a placeholder while the animated
  `splash.webp` (embedded `citrus-editor/assets/splash.webp`, or an override in the project
  root) is **decoded on a worker thread** (streamed frame-by-frame to bound memory) and
  swapped in, looped by elapsed time. The Vulkan renderer build also runs on a worker thread
  (it's `Send`; the scene with non-`Send` `dyn Component`s can't be, so it stays on main) so
  the splash animates during the build; the status line updates across later plugin/scene
  phases. Closed after the first editor frame.
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
  [todo] Chromatic aberration + bloom rendering (need an offscreen-HDR fullscreen pass; the
  settings are authorable now but only apply once that pass lands).

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
  BoxCollider, SphereCollider, MeshCollider, Spin, Bob
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
  distance, linear/log rolloff); AudioListener; driven in Play mode

### Physics / collision
- [done] Collider components (Box / Sphere / Mesh-convex) with is_trigger + layer, spawnable
  standalone or as components, yellow editable viewport widgets
- [partial] Physics simulation (rapier3d): a `RigidBody` component (Dynamic / Kinematic /
  Fixed, mass, restitution, friction, gravity scale). On Play (editor) and scene load
  (game) the engine builds a rapier world: colliders become cuboid/ball shapes (Mesh ->
  AABB cuboid), `RigidBody` objects get that body kind, collider-only objects become fixed
  (static) bodies, steps it under gravity each frame, and writes the simulated transforms
  back. **Layer-collision matrix** (done): colliders carry rapier `InteractionGroups` from the
  scene's `LayerSettings` (membership = the object's layer bit, filter = its collision-matrix
  row), so the Unity-style "which layers collide" matrix is enforced by the solver. Still todo:
  joints, queries (raycast/overlap), trigger events, CCD tuning, parented-body world↔local
  conversion.

---

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

- [todo] Expand `ComponentCtx` (or a new `World`/`Api` handle) with:
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
    `find_object(name)` kept as a convenience. tags, parent/children, spawn/despawn,
    set-active still todo
  - [partial] **Transforms of other objects**: any object resolves to a `Transform`
    (translation/rotation/scale, + forward/right/up/matrix) via `object_transform` /
    `object_transform_named` / `object_position` / `object_matrix` (world snapshot);
    *set* still todo
  - [todo] **Components**: get/add/remove a component on any object; typed access
  - [todo] **Input**: read the binding-system action snapshot (2C)
  - [todo] **Physics queries**: raycast, shape-cast, overlap; apply force/impulse; collision/
    trigger enter/stay/exit callbacks delivered to components
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
  - [todo] **Audio**: play/stop one-shots, change volume/pitch, spatial params
  - [todo] **Lights**: color/intensity/range/enabled at runtime
  - [partial] **Time / scene**: time scale, pause, app quit [todo]; **load/switch scene
    is done**: `ComponentCtx::load_scene(path)` (the first in-game-API slice) queues a
    `ComponentCommand` the engine applies after the update pass; switches levels / menu
    -> game during Play, continues playing in the new scene, and Stop returns to the
    pre-play scene.
  - [todo] **Events / messaging**: component-to-component messages or a simple event bus
- [todo] Stable, safe surface usable from plugin components (not just built-ins) without
  reaching into editor internals; likely a `citrus-api` crate both editor and plugins
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

### 2H. Render-to-texture cameras (camera output as a material input) [todo]
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
- [todo] **Shader precompilation**: shaders compile via `glslc` at runtime today; the
  bundler compiles every material/custom shader to **SPIR-V** ahead of time and ships the
  `.spv`, so the player's machine needs no `shaderc`/`glslc`.
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
- [todo] Occlusion culling, mipmaps, MSAA/TAA, multi-select gizmo, JFA outline

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
  transitions, cross-fade blend, 1D blend trees, `current_pose` clip weights. Feeding weights
  into skinning + a graph editor remain.
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
