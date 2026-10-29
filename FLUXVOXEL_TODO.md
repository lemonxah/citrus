# FluxVoxel GI — Techniques, Cross-Reference & TODO

FluxVoxel is a **propagation-free, analytically-injected SH irradiance volume** for the
no-ray-tracing / VR / low-end backend. It deliberately mixes ideas from four published
techniques. This document (1) lists each technique's features, (2) cross-references what
FluxVoxel **has today**, and (3) lists what's **outstanding** so we can tackle them.

Lineage & sources: LPV (Kaplanyan & Dachsbacher, I3D 2010) · Irradiance Volumes (Greger
et al. 1998) · DDGI (Majercik et al., JCGT 2019 + Scaling 2021) · VXGI/VCT (Crassin et al.
2011; NVIDIA VXGI 2015).

Legend: ✅ have · ◑ partial · ❌ missing.

---

## 1. Light Propagation Volumes (LPV)

| Feature | Description | FluxVoxel |
|---|---|---|
| SH grid of radiance | 2-band SH (4 coef) per cell, RGB; 32³, camera-cascaded | ✅ SH-L1 probe grid (cascaded for the SDF backend; per-volume for FluxVoxel) |
| Light injection | RSM surfels → clamped cosine SH lobe, half-cell offset | ◑ analytic per-light injection (`inject_light`), directional (L0-trim/L1-boost). No RSM. |
| **Iterative propagation** | 6-neighbour SH flux spread, 4–8 iters → 1 diffuse bounce | ❌ **no propagation** — we inject direct only (this is the "propagation-free" choice) |
| Geometry Volume occlusion | SH "fuzzy blocking probability", half-cell offset | ❌ no occlusion in the FluxVoxel inject path |
| Directional cosine lobe | front-hemisphere only | ◑ approximated via L0-trim/L1-boost (`add_sh_direct`) |
| Glossy (cone-march volume) | rough-only specular | ❌ |
| Fully dynamic, zero precompute | rebuilt per frame | ✅ dynamic lights injected per frame |

**Outstanding from LPV:** propagation pass (→ a real diffuse bounce), geometry-volume
occlusion (→ shadows), glossy cone-march.

---

## 2. Irradiance Volumes / Light Probes

| Feature | Description | FluxVoxel |
|---|---|---|
| Directional irradiance field | grid of probes, SH-L1/L2 or ambient cube | ✅ SH-L1 probes, trilinear sampled by `standard.frag` |
| Bake / inject | offline cubemap/path-trace → SH | ◑ FluxVoxel has a build-time baked static base (`.lightdata`) + per-frame analytic inject |
| Interpolation + normal eval | trilinear/barycentric, evaluate SH in N | ✅ |
| No propagation | probes independent | ✅ (matches our design) |
| ∞-bounce (when baked) | converged offline solve | ◑ static base is multi-bounce; dynamic inject is single-bounce |
| Leak-prone (no visibility) | interpolation crosses walls | ◑ the SW/HW probes carry DDGI distance moments; the FluxVoxel analytic inject does NOT |
| Diffuse only | no specular | ❌ specular (see VXGI) |

**Outstanding from Irradiance Volumes:** an **auto scene-covering grid when there are no
FluxVolumes** (today FluxVoxel needs author-placed volumes — this is why the floor outside
the volume is dark); per-volume + global density controls.

---

## 3. DDGI (the one you want toggled)

| Feature | Description | FluxVoxel |
|---|---|---|
| Irradiance + **distance** maps per probe | octa irradiance + mean/mean² distance | ◑ probes store SH-L1 + a directional distance SH (`ProbeSh::dist`) for the SW/HW path |
| Ray-traced injection | ~192 rays/probe, prev-frame feedback (multi-bounce) | ◑ SW (SDF march) / HW (ray-query) do this; **FluxVoxel does analytic inject, no rays** |
| **Chebyshev / variance visibility** | distance moments down-weight occluded probes → leak-free | ◑ `standard.frag`'s `sample_volume` does a Chebyshev-lite test for the SW/HW probes; **the FluxVoxel analytic inject writes no distance data, so its lights don't shadow** |
| Temporal EMA (hysteresis ~0.97) | converge rays over frames | ✅ FluxVoxel temporal blend / motion-aware EMA |
| Probe relocation / classification | move probes out of geometry, prune dead | ❌ |
| Self-shadow bias | world-space bias vector | ◑ small biases exist |
| Diffuse-only core | specular separate | ❌ specular |

**Outstanding from DDGI (the headline ask):** **occlusion for the FluxVoxel voxel lights**
— write per-probe directional distance moments during analytic injection and run the
Chebyshev test, so a FluxVoxel light is blocked by walls (cheap shadows). Make it a
**toggle** (DDGI visibility on/off). Plus probe relocation/classification (leak + dead-probe
cleanup).

---

## 4. VXGI / Voxel Cone Tracing

| Feature | Description | FluxVoxel |
|---|---|---|
| Voxel opacity + emittance, mipped | SVO or clip-map, anisotropic 6-dir | ❌ we use **probes, not a radiance voxel grid**; the SDF backend has a distance GDF but no radiance voxels |
| Hardware-rasterizer voxelization | conservative raster + atomic writes | ❌ |
| Diffuse cone gather | ~6 wide cones over hemisphere | ❌ (we sample probes instead) |
| **Glossy specular cone** | one narrow roughness-driven cone → reflections | ❌ **this is what would light the metallic floor from the volume** |
| Volumetric (anisotropic) occlusion | accumulated opacity along cone | ❌ |
| Fully dynamic, no RT | re-voxelize per frame | n/a (we don't voxelize radiance) |

**Outstanding from VXGI:** the big one is **specular/glossy from the volume** (a roughness
cone or a DDGI glossy approximation) so **metallic/reflective surfaces get GI from emissive
+ FluxVoxel lights**, not just diffuse surfaces. (Right now metals rely on the reflection
cube only — which is why the metallic floor doesn't pick up the emissive bounce.) Optionally
a true anisotropic radiance voxel grid if we ever want cone-traced GI.

---

## Consolidated outstanding work (the FluxVoxel TODO)

**A. Coverage & settings (fixes the dark floor + your settings asks) — DONE 2026-06-18**
- [x] **Auto scene-covering FluxVoxel grid when there are 0 FluxVolumes**, with a global
  **voxel density** control (`RealtimeGi.voxel_density`, probes/m; drives the auto-grid
  spacing in `scene.fluxvr_volumes`). Toggle `voxel_auto_grid` (default on) to opt out.
- [x] When ≥1 FluxVolume: each volume already carries its **own `density`** (edited on the
  FluxVolume component, Unity-style). The auto grid only kicks in when none are placed.
- [x] **Modular settings per backend**: `GiMode::uses_{bounces,quality,samples,gdf,
  march_distance,voxel_density,ddgi_occlusion}()` declare what each backend consumes; the
  Environment GUI renders only those controls. FluxVoxel now hides bounces/quality/GDF/
  march and shows voxel density + the toggles instead. Reflections live in their own section.

**B. Occlusion / shadows (DDGI) — toggleable — DONE 2026-06-18**
- [x] FluxVoxel voxel lights are now **occluded by a coarse scene occupancy grid**
  (`sw_gi::SceneOccupancy`, mesh-AABB bit-grid; `inject_light_occ` DDA-marches light→probe).
  Soft transmittance (never hard-blacks — the grid is coarse, so it's contact darkening, not
  stencil shadows). **Toggle `voxel_ddgi_occlusion`** (Environment → "DDGI occlusion",
  default on; off saves the per-probe marches on low-end/VR). Unit-tested.
  - NOTE: this is an occupancy-DDA approximation, NOT yet true per-probe distance moments +
    Chebyshev. Upgrading to real DDGI moments (write `ProbeSh::dist`/`dist2` during inject,
    Chebyshev in `sample_volume`) is the remaining quality follow-up. **Needs visual verification.**
- [x] Probe **relocation** + **classification** — DONE 2026-06-18: `sw_gi::relocate_probes`
  nudges probes out of solid cells (occupancy grid) into the nearest empty neighbour; a
  fully-trapped probe is classified inactive (`fluxvr_active` mask) and its SH zeroed each
  frame so it can't leak wrong values into the trilinear blend. Unit-tested.

**C. Specular / metallic (VXGI) — DONE 2026-06-18; gated 2026-06-19**
- [x] **Specular GI from the volume** (DDGI glossy approx): `standard.frag::volume_radiance`
  trilinearly samples the probe volume's RADIANCE (`sh_radiance`, raw L1) in the reflection
  direction R, so SMOOTH METALS pick up the local emissive/voxel bounce the coarse reflection
  cube misses. **Toggle `voxel_specular`** (Environment → "Specular GI", default on), threaded
  via `FrameInput.voxel_specular` → `fog_params.w`.
  - GATING FIX 2026-06-19: the term is VIEW-DEPENDENT (R = reflect(-V, N)); the first version
    `reflectance·vrad·roughness` lit ROUGH/dielectric floors at grazing Fresnel, so the emissive
    floor glow rode the camera ("GI only on the camera side"). Now gated `f0 · vrad ·
    metallic·(1-roughness)` → ~0 for dielectric OR rough surfaces, which instead get the emissive
    as **view-independent diffuse GI** (consistent from every angle). Only smooth metals get the
    view-dependent specular, where it's correct.

**D. Bounces (LPV / DDGI) — DONE 2026-06-18**
- [x] **LPV-style propagation** (`sw_gi::flux_propagate`): each iteration spreads a fraction
  (`6·gain<1` so it decays) of every cell's directional radiance to its 6 axis neighbours,
  re-projected as a travel-direction lobe, giving ≥1 diffuse bounce that fills shadowed
  pockets. Runs per-frame on the combined static+dynamic field, per volume slice (3 iters,
  gain 0.12). **Toggle `voxel_propagation`** (Environment → "Light propagation", default on;
  off = direct-only). Unit-tested (spreads + stays bounded).

**E. Emissive fidelity — DONE 2026-06-18**
- [x] **Multi-point emissive sampling** (`scene.fluxvr_lights`): a compact emitter stays one
  point, but an elongated one (strip/screen/visor) scatters K points (K = round(long/thin),
  capped 8) along its longest axis, energy split `flux/K` so total emitted flux is preserved —
  so it lights its surroundings over its whole extent, not just the centre. (Per-UV emission-map
  sampling at each point — vs the object-average colour — remains a finer follow-up.)

**F. Light range + gain — DONE 2026-06-18; retuned 2026-06-19**
- [x] **Modest emissive range** (`scene.fluxvr_lights`): `√(trueLum)·1.2`, clamp 0.5–5 m (the
  earlier `√(lum/0.03)` gave ~16 m — flooded the whole floor). Range is computed from the TRUE
  emission so the GI gain below doesn't inflate reach.
- [x] **Emissive GI gain** `EMISSIVE_GI_GAIN = 8.0`: a physical bounce off a dark floor is
  `albedo·irradiance/π` — far too dim to see, so the emissive floor "glow" was actually coming
  from the (view-DEPENDENT) volume-specular term. Now the emissive injects ~8× into the volume
  as a bright area source, giving a clearly-visible **view-INDEPENDENT diffuse pool** (same from
  every angle). Tunable constant; could be exposed as an Environment slider if per-scene control
  is wanted.

---

## DDGI done right (2026-06-18) — distance moments, occlusion free per frame

Replaced the per-light occluded DDA (the perf + flicker source) with **cached per-probe DDGI
distance moments**: `sw_gi::flux_distance_moments` projects directional distance-to-geometry
(from the occupancy grid) into SH-L1 `dist`/`dist2`, stored in `ProbeSh` (packed into the
probe buffer's `c[].w` + `c[4]`). The shader's existing **Chebyshev** (`sample_volume`) then
blocks a probe's light from reaching fragments behind geometry — leak-free diffuse + cheap
shadows (e.g. an emissive orb no longer lights the BACK of the helmet). Injection is now ALWAYS
the cheap analytic path (static + dynamic); occlusion is the cached moments, so **DDGI adds
~zero per-frame cost and never flickers**. Toggle `voxel_ddgi_occlusion` off → moments zeroed →
Chebyshev is a no-op. This is the "true DDGI moments" upgrade the §B note asked for.

## Perf regression fix (2026-06-18) — DDGI was 600→60 fps + flickery

The first cut did the expensive GI work PER FRAME, which tanked fps and flickered:
- **Occupancy grid** was rebuilt every frame → now cached, rebuilt only with the static base.
- **Propagation** ran over the whole probe grid every frame (with array clones) → now baked into
  the cached static base (static + emissive carry the bounce; dynamic movers stay direct-only).
- **Occluded inject** ran per-frame for dynamic lights (a DDA march per probe×light) → that was
  the 600→60 killer AND the flicker source (the integer blocked-cell count jumps as a light moves
  relative to the static grid). Occlusion now lives ONLY in the cached static base; the per-frame
  dynamic pass is the cheap analytic inject, so **DDGI on/off has ~zero per-frame cost**.
- **Steady-scene skip**: a dynamic-light hash skips the per-frame clone+inject+upload entirely
  when nothing moved.
- **Stable auto-grid**: bounds come from STATIC geometry only and are quantized to 1 m, so a
  moving object can't jitter the grid and force a per-frame re-bake (which also flickered).

Net: occlusion/propagation are precomputed once; the per-frame path is just the dynamic direct
inject (or nothing). Moving lights no longer cast voxel shadows (the tradeoff for the cheap
backend); static + emissive do.

## Performance toggles (low-end / VR — all default ON)

Expensive operations that can be turned off without breaking the scene:

| Setting | Where | Default | Off = |
|---|---|---|---|
| `RealtimeGi.enabled` | Environment → Flux GI | off* | no realtime GI |
| `voxel_ddgi_occlusion` | Environment → DDGI occlusion | **on** | voxel lights don't shadow (cheaper) |
| `voxel_propagation` | Environment → Light propagation | **on** | direct-only, no diffuse bounce (cheapest) |
| `voxel_specular` | Environment → Specular GI | **on** | metals use reflection cube only |
| `voxel_auto_grid` | Environment → Auto scene grid | **on** | only author FluxVolumes lit |
| `ssr_enabled` / `reflection_mode` | Environment → Reflections | SSR | cube probes only (cheapest) |
| `probe_fallback` | Advanced | Off | no world-probe march |
| quality / bounces / GDF res | Advanced (SDF/RT only) | Balanced/2/128³ | lower = cheaper |

\* GI defaults off so an un-set-up scene isn't paying for it; turning it on is one click.

---

## Where FluxVoxel sits vs the matrix

FluxVoxel = **Irradiance-Volume sampling + LPV-style analytic injection + LPV propagation
(1 bounce) + the SH directionality hack + coarse occupancy-DDA occlusion + probe relocation +
a VXGI-style glossy specular term**. All of A–F are now implemented and toggleable (default on).
The **one remaining quality upgrade** is true per-probe DDGI distance moments + Chebyshev (vs the
current occupancy-DDA occlusion) for leak-free diffuse; plus per-UV emission-map sampling for the
multi-point emissive. It stays the cheapest, no-RT backend; everything expensive is a toggle for
low-end / VR. **The new shader/GI paths (specular, propagation, occlusion) still want visual
verification on hardware** — the math + CPU pieces are unit-tested, but I can't GPU-render here.
</content>
