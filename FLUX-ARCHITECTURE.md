# Flux GI — Architecture & Roadmap

Learnings from reading the Unreal Engine 5.8 source (Lumen) and how they apply to
Flux, our realtime GI. This is the plan of record for evolving Flux from a
per-pixel brute-force tracer into a Lumen-style caching architecture.

Source studied: `Engine/Source/Runtime/Renderer/Private/Lumen/` and
`Engine/Shaders/Private/Lumen/` in UE 5.8.0-preview-1.

---

## The core insight

**Lumen is a caching architecture that happens to trace a few rays — not a ray
tracer with a good denoiser.** Everything it does is in service of *not*
computing lighting at the pixel that needs it. It reuses lighting:

- across **space** — sparse screen probes (1 per ~16×16 px), not per pixel;
- across **surfaces** — a persistent, world-space, *lit* Surface Cache;
- across **time** — temporal accumulation in the probe domain;
- across **distance** — a world-space Radiance Cache rays terminate into early.

Flux today does the opposite of all four: it computes indirect light **per
pixel, every frame, from scratch, by re-shading at each ray hit**. That single
choice *is* the noise. Denoiser tuning polishes a signal computed in the wrong
place and the wrong way.

---

## How Lumen works (four pillars)

1. **Persistent scene + Surface Cache.** Meshes are decomposed offline into
   *cards* (6-axis oriented planes). At runtime cards are captured into a 4096²
   atlas (albedo/normal/depth/emissive), streamed by GPU feedback (only pages
   the gather actually sampled, near-frustum, budgeted ~300 captures/frame). The
   cache is then *lit* into a persistent `FinalLightingAtlas`: a direct pass +
   **Radiosity** (card probes sample *last frame's* FinalLighting at their hit →
   multi-bounce accumulates over frames). Only a budgeted slice is re-lit/frame.

2. **Tracing returns cached lighting, not re-shaded surfaces.** Software
   (sphere-march mesh-SDF then a 4-clipmap Global SDF, `MaxSteps=256`) or
   hardware RT — both end as *visibility query + one atlas fetch* of
   `FinalLightingAtlas`. No material eval, no recursion at the hit. This is the
   low-variance keystone.

3. **World-space Radiance Cache.** A 4-clipmap volumetric grid of radiance
   probes around the camera (48³, 1024 rays/probe, ~100 updated/frame, persist 8
   frames). Screen-probe rays check coverage and **terminate early** into it, so
   far-field + multi-bounce is a cheap shared interpolation.

4. **Screen Probe Gather + real denoiser.** One probe per 16×16 tile (~256
   px/probe), jittered, plus adaptive probes at edges/disocclusion. **64
   rays/probe** in an octahedral atlas (~256× fewer traces than per-pixel),
   **importance-sampled** by `BRDF(SH) × prev-frame incoming lighting`. Denoise
   in the **probe domain** (temporal EMA ~12 frames + edge-gated spatial +
   firefly clamp), project each probe to a **3-band RGB SH (9 coeffs)**, then per
   pixel do a plane-weighted **bilateral upsample** of the 4 nearest probes and
   integrate against the pixel's own normal/BRDF.

Reflections reuse the same trace infra (VNDF GGX importance sampling, mirror
fast-path, rough rays shortened into the radiance cache) with a dedicated
denoiser: dual reprojection (surface + reflection-hit motion), **YCoCg
neighborhood variance clamp**, and a **variance-driven** spatial filter
(weights = depth × normal × luminance, widening on disocclusion).

UE files worth re-reading per pillar:
- Scene/cache: `LumenSceneRendering.cpp`, `LumenSurfaceCache.cpp`,
  `LumenSceneCardCapture.cpp`, `LumenRadiosity.cpp`, `LumenSceneLighting.cpp`.
- Final gather: `LumenScreenProbeGather.cpp` (read its top comment),
  `LumenScreenProbeTracing.cpp`, `LumenScreenProbeImportanceSampling.cpp`,
  `LumenScreenProbeFiltering.cpp`, shader `LumenScreenProbeGather.usf`.
- Radiance cache: `LumenRadianceCache.cpp/.h`.
- Tracing/denoise: `LumenSoftwareRayTracing.ush`, `GlobalDistanceFieldUtils.ush`,
  `LumenReflections.usf`, `LumenReflectionDenoiserTemporal.usf` / `...Spatial.usf`.

---

## Where Flux fails today (mapped to our code)

| Lumen | Flux (`crates/citrus-engine/src/sw_gi.rs`, `shaders/screen_gi.comp`, `shaders/standard.frag::screen_gi_upsample`) | Consequence |
|---|---|---|
| 1 probe / 256 px, 64 rays/probe | **1 trace per pixel**, 6–24 cosine rays (`screen_gi.comp`) | ~256× more trace work → too few rays → variance |
| Hit = read cached FinalLighting | Hit = re-shade (`direct_light` + `emitter_light` NEE) every ray | per-ray variance, no surface reuse |
| Persistent world-space lit cache + Radiosity | **none** — recomputed every frame | nothing converges; fresh noise each frame |
| Radiance-cache far-field early-out | every ray marches the full GDF | long marches, no multibounce reuse |
| Importance sample BRDF × prev lighting | uniform cosine hemisphere | low effective sample count |
| Probe-domain temporal + variance clamp + variance-driven spatial | per-pixel temporal in compute + single 5×5 bilateral (`screen_gi_upsample`) | bilateral can't fix raw per-pixel variance (worst on curvature) |
| Per-probe SH + plane-weighted upsample + BRDF integral | direct irradiance + plane/radial bilateral | no analytic integration → over-blur or noise |

**We already own most of the ingredients**, just wired as per-pixel brute force
instead of the Lumen topology:
- a Global Distance Field (the GDF the compute marches),
- baked **SH probe volumes** (`sample_volume` / `sh_eval` in `standard.frag`) —
  this is literally a world-space radiance cache,
- temporal reprojection (in `screen_gi.comp`),
- a plane-aware bilateral (`screen_gi_upsample`).

---

## Target architecture

```
                 (persists across frames, converges)
  world SH probe volumes  ◄── baking + Flux unified into one tracer
        ▲   ▲
        │   └────────────── far-field ray early-out (radiance cache)
        │
  screen probe grid  (1 / 8–16 px tile, octahedral atlas, N rays/probe)
        │  trace: short GDF march near-field → read cache at hit (no re-shade)
        ▼
  probe-domain denoise  (temporal EMA + variance clamp + edge-gated spatial)
        ▼
  per-probe SH (L1/L2)
        ▼
  per-pixel: plane-weighted bilateral upsample + BRDF integrate  ──► indirect
```

---

## Phased roadmap (highest leverage first)

### Phase 1 — Sparsify the gather  ✅ ALREADY DONE (verified 2026-06-16)
The gather **already runs at probe resolution**: `SCREEN_PROBE_DIV = 4`
(`lib.rs:936`), `ScreenGiTargets` allocates the radiance target at
`screen / SCREEN_PROBE_DIV` (`lib.rs:993`), and `record_flux_trace` dispatches
`screen_gi.comp` over `probe_extent` (one invocation per probe, `gpu_gi.rs:762`).
The forward `screen_gi_upsample` plane-weighted bilateral interpolates probes →
per-pixel. The sparse screen-probe topology already exists. Remaining levers:
raise rays/probe (we have headroom now), and tune `DIV` (4→2 sharper, 4→8 cheaper).

### Phase 2 — Rays read a cache, stop re-shading  ✅ DONE (2026-06-16)
The gather now reads the world SH radiance cache at ray hits. `screen_gi.comp`
binds the probe SSBO (binding 9) + volume metadata (in the params UBO) — the same
buffer `realtime_gi` maintains every frame via `set_baked_probes`. At a hit,
`cache_irradiance()` does the 8-corner trilinear SH-L1 eval (identical convolution
to `standard.frag`); a covered hit terminates the path with `albedo * E/π`
(Lumen's "trace returns cached lighting" — converged, multi-bounce, ~zero
variance, no recursion). No coverage → falls back to the old analytic re-shade +
bounce so empty/unbaked scenes don't regress. Plumbed via `GiVolume` /
`ScreenGiParams.volumes` (`gpu_gi.rs`) and `self.probe_buffer.handle` /
`self.probe_volumes` (`lib.rs`).

### Phase 2 (original notes) — Rays read a cache, stop re-shading
At the SDF hit, sample the **baked SH probe volumes** (or a cheap world-space
surfel/voxel radiance grid) instead of `direct_light + emitter_light`. Low
variance + multi-bounce by construction. This is the "trace returns cached
lighting" keystone.

### Phase 3 — Far-field early-out into the probe volumes
Terminate screen-probe rays into the SH volumes past a short near-field march
(Lumen's radiance-cache trick). We already have the volumes.

### Phase 4 — Denoiser, ported from Lumen  ◑ IN PROGRESS (2026-06-16)
Ported `shaders/flux_common.glsl` — our own versions of UE's Lumen math
(verbatim constants): `sh_basis3` (SHBasisFunction3), `calc_diffuse_transfer_sh3`,
`dot_sh3`, `eval_sh_irradiance` (directional-occlusion SH→irradiance),
`mul/add_sh3`, octahedral encode/decode, `equiarea_to_unit_vector` (Lumen probe
ray mapping), `firefly_clamp` (MaxRayIntensity hue-preserving), `plane_depth_weight`
(PLANE_WEIGHTING upsample). Build-time `#include` resolves (glslc).
- ✅ **Firefly clamp** now hue-preserving (scale-by-max, Lumen's exact model),
  via the helper in `screen_gi.comp`.
- ✅ **Temporal model fixed** (the motion-noise fix): we no longer crank alpha to
  0.5 while the camera moves — Lumen handles camera motion with reprojection +
  per-probe disocclusion reject, accumulating ~10 frames in motion. Alpha is now
  a low fixed value (0.10 still / 0.18 moving, smoothing-tuned) ≈ Lumen's
  MaxFramesAccumulated≈10 (`Alpha=1/(1+N)`, N→10 → 0.09).
- ◑ **Remaining for exact 1:1:** (a) the per-probe frame counter `N` (needs a
  dedicated ping-pong target) for the true `Alpha=1/(1+N)` + fast-update +
  disocclusion-reset; (b) the **per-probe octahedral radiance atlas** so we can
  run Lumen's probe-domain **spatial filter** (`ScreenProbeFilterGatherTraces`:
  position×angle weights) and **SH projection** (`ScreenProbeConvertToIrradiance`)
  then integrate per-pixel via `4π·eval_sh_irradiance` — this is the big
  remaining restructure (our gather writes one irradiance/probe, not an octa atlas).

### Phase 5 — Importance sampling + per-probe SH
Sample the hemisphere by previous-frame probe radiance; project each probe to SH
and integrate against the per-pixel BRDF on upsample.

Phases 1–2 alone get most of the way and reuse existing infrastructure. This is
also where **baking + Flux unify into one tracer** — the baked SH volumes become
the realtime radiance cache.

### Phase 6 — Software + Hardware (RT-core) trace backends (Lumen parity)
Lumen has two interchangeable tracers that return the **same** result (read the
surface cache at the hit). Flux should match:
- **Software** = the GDF sphere-march in `screen_gi.comp` (done; Phase 2 reads
  the cache at hits).
- **Hardware** = a `VK_KHR_ray_query` variant of the gather that traces the real
  **TLAS** instead of the GDF, then reads the SAME world SH cache at the hit (no
  re-shade). Gate on device RT support (already detected via `supports_baking()`
  / ray-query) and the `reflection_mode`/a GI-backend setting. Reuse the
  ray-query infra already used for baking + RT reflections. Both backends share
  `ScreenGiParams`, `cache_irradiance`, and the denoise/upsample — only the
  visibility query differs (GDF march vs `rayQueryEXT`). HW gives exact geometry
  (no GDF over-estimate / leak) where RT cores exist; SW runs everywhere.

### HDR render pipeline — STATUS
The core HDR pipeline already works: the scene renders to a linear **HDR float**
target (`R16G16B16A16_SFLOAT`, `post::HDR_FORMAT`), surfaces output linear
radiance (`debug.w = 1`, both editor-viewport and game paths), and a fullscreen
post pass does exposure → grade → **ACES tonemap** → vignette into the swapchain.
- ✅ **Bloom fixed** (2026-06-16): was a ~2%-screen single ring with a hard
  threshold (imperceptible). Now a **soft-knee threshold** (Karis quadratic) +
  a **wide multi-octave Gaussian spread** (~25% of screen), operating on the
  linear-HDR scene **before** tonemap — UE's bloom math, single-pass approximation
  of the pyramid.
- ◑ Remaining for full UE parity: a real **downsample/upsample mip-chain** bloom
  (cheaper + wider than the single-pass approx), a true **HDR-float texture path**
  (EXRs currently clamp to LDR on load), and optional **HDR display output**
  (HDR10/scRGB swapchain).

---

## 1:1 Lumen ↔ Flux component map

| Lumen component | Flux equivalent | Status |
|---|---|---|
| Global SDF clipmaps | GDF (`gpu_gi.rs` dist/index 3D textures) | ✅ have (single grid, not clipmaps) |
| Mesh SDF near-field | — | ✗ (acceptable; GDF-only near+far) |
| Surface Cache (lit world atlas) | World SH probe volumes (`probes` SSBO, `sample_volume`/`sh_eval`) | ◑ exists but baked/fallback-only; needs realtime upkeep while Flux active |
| Radiosity (multi-bounce on cache) | screen-space `prev_indirect()` feedback in `screen_gi.comp` | ◑ screen-space only (fails off-screen); move to world SH cache |
| Radiance Cache (world far-field probes) | Same world SH probe volumes (clipmap/cascade) | ◑ have cascades in `realtime_gi.rs`; not read by gather |
| Screen Probe placement (1/16px + adaptive) | `SCREEN_PROBE_DIV=4` uniform grid | ✅ uniform; ✗ adaptive probes |
| Screen Probe trace (64 rays, octahedral, importance) | `screen_gi.comp` cosine hemisphere, `samples` rays | ◑ uniform cosine, no importance sampling |
| Trace returns **cached** lighting | `incoming()` **re-shades** (direct+NEE) per hit | ✗ — THE keystone gap (Phase 2) |
| Probe-domain temporal + variance clamp | per-pixel temporal EMA, **no clamp** | ◑ no variance clamp (Phase 3) |
| DDGI two-moment Chebyshev probe visibility | mean+variance distance SH → smooth occlusion | ✅ DONE (2026-06-16) — `dist²` SH (`ProbeSh.dist2`, 5th probe vec4); both `cache_irradiance` (gather) and `sample_volume` (forward) use `variance/(variance+Δ²)`; fixes the "0→1 at specific distances" flips as objects move |
| Probe → SH → bilateral upsample + BRDF | direct irradiance + plane bilateral (`screen_gi_upsample`) | ◑ no per-probe SH integration |
| Reflections: VNDF GGX + dual-reproj denoiser | `ssr_resolve.frag` screen march + env cube | ◑ no SDF reflection, simpler denoise |

## Math conventions to match (the spec)

These must match so results are physically consistent and tunable like Lumen.

**Spherical harmonics.** Lumen uses 3-band RGB SH (L2, 9 coeffs/channel):
`SHBasisFunction3`, irradiance via `EvaluateSHIrradiance` / `DotSH3(L, CalcDiffuseTransferSH3(N, AO))`. Flux currently stores **SH-L1** (4 coeffs: `c[0..3]`) with the
radiance→Lambertian-irradiance band factors `A0=π, A1=2π/3`, divided by the
diffuse BRDF `π` → constant `0.282095` (Y0), L1 `0.488603 * 2/3 = 0.325735`
(see `sh_eval` in `standard.frag`). Decision: **keep L1 for the realtime cache**
(cheap, VR-friendly), but document it as the intentional reduced-order form of
Lumen's L2. Use the *same* radiance→irradiance convolution (cosine-lobe band
factors) everywhere so baked and realtime probes integrate identically.

**Cosine-hemisphere estimator.** Sampling pdf `= cosθ/π` importance-samples the
Lambertian cosine, so diffuse irradiance estimate = simple mean of per-ray
incoming radiance (already correct in `screen_gi.comp`: `irr / samples`). Keep.

**Octahedral mapping.** Encode/decode already present (`oct_encode`/`oct_decode`
in `standard.frag` / `ssr_resolve.frag`); when we move to per-probe octahedral
radiance atlases, reuse these (matches Lumen's `UnitVectorToOctahedron`).

**Temporal accumulation.** Match Lumen's EMA + neighborhood clamp:
`alpha = 1 / (framesAccumulated + bias)`, capped at `MaxFramesAccumulated` (~12);
clamp reprojected history to the current trace's neighborhood:
`Extent = ClampScale * stddev(neighbours)`, `history = clamp(history, mean±Extent)`
in **YCoCg** (or luminance for the cheap version). This is what lets history be
trusted while the camera moves without ghosting — the main fix for the motion
noise we see now (current code drops to `alpha≈0.5` while moving = barely any
accumulation).

**Trace returns cached lighting.** At a ray hit, sample the world SH cache
irradiance (`sh_eval` at the hit, oriented by the hit normal) instead of
`direct_light + emitter_light`. The cache already holds converged multi-bounce,
so the per-ray result is low-variance — Lumen's `EvaluateRayHitFromSurfaceCache`.

**Far-field early-out.** When the world cache covers a region, clamp the GDF
march distance and interpolate the cache beyond it (Lumen
`MinTraceDistanceBeforeInterpolation`).

**Importance sampling (later).** Build a per-probe PDF = `BRDF_SH × prev-frame
incoming radiance`, cull low-PDF directions and reallocate their ray budget
(Lumen `ScreenProbeGenerateRaysCS`).

**Reflections (later).** VNDF GGX (`ImportanceSampleVisibleGGX`) ray generation,
mirror fast-path under a roughness epsilon, rough rays shortened into the
radiance cache; denoise with dual reprojection (surface + hit motion) and a
YCoCg neighborhood variance clamp.

## Constraints / non-goals
- No tessellation pipeline → no card-capture rasterization like Lumen; our
  "surface cache" equivalent is the world SH probe volumes (and later, optional
  per-object low-res irradiance atlases).
- Targets VR — keep the per-pixel cost low; sparsification (Phase 1) is the main
  lever for that too.
- Keep the soft, atmospheric aesthetic: bias toward smooth/converged over sharp.

## Verification against UE5.8 source (2026-06-16)

Checked `flux_common.glsl` line-by-line against UE5.8
(`/home/lemonxah/git/UnrealEngine-5.8.0-preview-1/Engine/Shaders/Private`):

| Flux helper | UE source | Result |
|---|---|---|
| `sh_basis3` | `SHBasisFunction3` (SHCommon.ush) | exact |
| `calc_diffuse_transfer_sh3` | `CalcDiffuseTransferSH3` (SHCommon.ush) | exact (L0=2π/(1+e), L1=2π/(2+e), L2=e·2π/(3+4e+e²)) |
| `eval_sh_irradiance` | `EvaluateSHIrradiance` (SHCommon.ush) | exact (Z0/Z1/Z2 zonal AO expansion) |
| `equiarea_to_unit_vector` | `EquiAreaSphericalMapping` (MonteCarlo.ush) | exact |
| `unit_vector_to_octahedron` / `octahedron_to_unit_vector` | OctahedralCommon.ush | exact |
| `hammersley` | `Hammersley` (MonteCarlo.ush) | core exact; the `Random` rotation is applied separately (Cranley-Patterson jitter) |

Reference integrate (`LumenScreenProbeFiltering.usf` `ScreenProbeConvertToIrradiance`):
`RadianceSH = (1/N)·Σ MulSH3(SHBasisFunction3(dir), Radiance)` then
`Irradiance = 4π·DotSH3(RadianceSH, CalcDiffuseTransferSH3(n,1))`.

### Pipeline status (software)
- Gather (`screen_gi.comp`) → ConvertToSH (`sh_buf`, normalized 1/N·1/pdf·sky.w, emitters projected in) → done.
- ScreenProbeTemporal: temporal variance/firefly clamp on the gather (adaptive) → done.
- ScreenProbeSpatialFilter: variance-driven à-trous (`flux_denoise.comp`, plane + curvature edge-stops, spatial firefly clamp) → done, on the SCALAR.
- ScreenProbeTemporal on the SH (`screen_gi.comp`): the per-probe SH is itself
  temporally accumulated via a ping-pong `sh_buf` (reprojected previous-frame probe,
  blended at the same alpha as the scalar, disocclusion-gated) → done.
- ScreenProbeSpatialFilter on the SH (`flux_integrate.comp`): a 5×5 probe-neighbourhood
  depth-weighted gather of the SH before integration → done.
- ScreenProbeIntegrate (`flux_integrate.comp`): PURE `EvaluateSHIrradiance` = DotSH3
  with the cosine-lobe transfer (folded /PI), carrying BOTH magnitude and per-pixel
  direction from the now temporal+spatially-denoised SH → done (strict-1:1 path).

The radiance SH is normalized `(1/N)·(π/cos clamped)·sky.w` so the integrate is
self-consistent with the scalar at the probe normal; analytic emitters are projected
into the SH (L1 lobe) so DotSH3 captures the smooth emitter pools.

### HW ray-query backend (Flux-RT)
- `screen_gi_rt.comp` written + compiles (build.rs, vulkan1.3): the RT-core equivalent
  of UE's `LumenScreenProbeHardwareRayTracing`. Same screen-probe model + SH output as
  the SDF gather, but each gather ray is a `rayQueryEXT` closest-hit against the scene
  TLAS (reusing the `rt_reflect` TLAS/instance-geometry layout: vertex/index buffer
  references + per-instance albedo/emission), with ray-query shadow rays for direct
  light. Single bounce. Writes the temporally-accumulated SH + scalar, identical layout
  to the SW path so `flux_denoise`/`flux_integrate` are backend-agnostic.
- REMAINING: runtime selection wiring (pick the RT pipeline when the device exposes
  ray-query; build the GI TLAS + instance/light/emitter SSBOs; dispatch) and HARDWARE
  verification — BLOCKED on this dev GPU (no ray-query), like `rt_reflect.comp` which
  also ships unverified-on-dev. The shader is ready for an RT GPU.

### Other remaining
- Pure-DotSH3 brightness parity vs the old hybrid is approximate (clamped 1/cos bias at
  grazing + L1 emitter lobe vs analytic NEE); tune the gather constants if it reads off.
