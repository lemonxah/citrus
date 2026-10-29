# Unreal Engine 5.8 Research — Baked Lighting, Reflections, Async Loading

Source read: `/home/lemonxah/git/UnrealEngine-5.8.0-preview-1`. Findings drive the
Citrus changes in this pass. File paths + symbols cited so they're re-checkable.

---

## A. Baked lighting (Lightmass)

1. **What gets a lightmap.** `UPrimitiveComponent::HasStaticLighting()`
   (`PrimitiveComponent.cpp:500`): `Mobility == Static || LightmapType == ForceSurface`,
   AND `SupportsStaticLighting()`, AND the mesh has a lightmap UV channel. Default for a
   placed static mesh = baked.

2. **Emissive does NOT exclude a mesh from RECEIVING baked light.** `bUseEmissiveForStaticLighting`
   (`FLightmassPrimitiveSettings`, engine default **false**) only controls whether the
   emissive surface *contributes* light — Lightmass turns emissive texels into **mesh area
   lights** (`UnrealLightmass/.../LightingMesh.cpp` ~180-434). Receiving GI is a separate
   final-gather pass gated only by `HasStaticLighting()`. **→ Our helmet bug is NOT
   "emissive excludes from bake." The helmet is `metallic:1.0`; metals have no diffuse, so
   their lightmap is legitimately ~black. The fix is reflections + the emission-mean bug,
   not the bake selection.**

3. **Two-sided in the bake** (`LightingSystem.cpp:2598-2614`): samples the front face, then
   a second sample with the **negated tangent basis** (`-WorldTangent*`, `-WorldLightVector`),
   and stores `(front + back) * 0.5` into the **one shared lightmap texel**. There is no
   separate per-side UV. `bTwoSided` also disables backface light culling.

4. **Lightmap quality features** we don't have (gap list): photon mapping + final gather,
   **irradiance caching with rotational/translational gradients**, **sky occlusion /
   bent-normal** per texel, **AO bake** (`bUseAmbientOcclusion`), and **SDF shadows** for a
   stationary directional light (the `.doms` channel). We have À-Trous denoise + seam-stitch
   + dilate, which is a reasonable substitute for irradiance caching's smoothness but misses
   sky-occlusion/bent-normal and AO bake.

5. **Directional lightmaps.** UE stores **HQ directional lightmaps** (HL2 3-axis basis
   derived from per-texel SH): `Coefficients[0]` = average colour, others = dominant-direction
   colour, so **normal maps still respond to baked light**. `LM_NUM_HQ_LIGHTMAP_COEF=2`
   (`ImportExport.h`). **We store flat irradiance only — normal-mapped detail goes flat on
   baked surfaces. This is our biggest baked-lighting gap.**

---

## B. Reflections

1. **Reflection method is GLOBAL, not per-material.** `EReflectionMethod` (`EngineTypes.h:472`):
   `None | Lumen | ScreenSpace`. Stored on `FPostProcessSettings::ReflectionMethod`
   (post-process-volume / project global). Ray-traced is **Lumen with HW ray tracing**, not a
   separate enum. There is **no per-material opaque SSR/reflection checkbox** — the only
   per-material reflection control is `UMaterial::bScreenSpaceReflections` for *translucency*.
   **→ Validates removing our per-material `screen_reflections` checkbox and making reflection
   type a global Environment setting (Reflection Probes / SSR / Ray-traced).**

2. **Capture priority + blending (the key algorithm).** No explicit priority field — **smaller
   radius wins.** Captures sorted by radius ascending (`RendererScene.cpp:5105`,
   `FReflectionCaptureSortKey`). GPU blend is an **"Under" (back-to-front) alpha composite**
   (`ReflectionEnvironmentComposite.ush`): apply largest first, accumulate
   `rgb += sample.rgb; a *= 1 - sample.a`, early-out when `a < 0.001` → the **smallest capture
   covering a pixel dominates**, larger ones fill the remainder. Blend weight = **smooth
   `DistanceAlpha`** (smoothstep falloff): sphere ramps the inner 60%, box fades within a
   `BoxTransitionDistance` shell inside the surface. **→ This is exactly the multi-probe
   priority+blend model to adopt.**

3. **Box projection** (`GetLookupVectorForBoxCapture`): intersect reflection ray with the box
   (ray-slab), re-vector from the capture centre (+ optional `LocalCaptureOffset`). Matches what
   we already do.

4. **Fallback chain:** SSR/Lumen (screen) → reflection captures (smallest first) → **skylight
   cubemap** — each layer consumes the alpha the previous left. The skylight cubemap is the
   universal global fallback so off-screen/uncaptured reflections never go black. **We use the
   env cube as this fallback — good; keep it as the last layer.**

5. **Missing-but-worth-adding:** skylight/env cubemap as explicit last fallback (have it),
   **capture mixing via `AverageBrightness`** (normalizes specular vs baked diffuse so
   reflections aren't over/under-bright in mixed lighting), and **multiple-capture blend** (we
   only sampled one probe — fixing this pass).

---

## C. Async asset loading / UI responsiveness

1. **Thread split:** Game thread runs Slate (UI) + gameplay; **Render thread runs one frame
   behind** consuming enqueued immutable closures (`ENQUEUE_RENDER_COMMAND`); RHI thread submits.
   A 200 ms render in frame N-1 doesn't stall the game thread building UI for frame N — you get
   latency, not a freeze. Controlled by `FFrameEndSync` with a **fence-depth allowance**
   (`r.OneFrameThreadLag`); hard flush (`FlushRenderingCommands`) only for readbacks.

2. **Time-slicing budget (the pattern to steal):** `FAsyncLoadingThreadState2` holds
   `(bUseTimeLimit, TimeLimit, StartTime)` installed via a RAII `FTimeLimitScope`; every work
   loop polls `IsTimeLimitExceeded()` **between granular work units** (one object/mip per
   iteration) and returns a tri-state `Complete | TimeOut | Pending`, resuming next frame.
   Default budget ~5 ms/frame. **→ A small per-thread budget + granular resumable work is how
   the editor never freezes during a load.**

3. **Handle = await token + keep-alive refcount:** `RequestAsyncLoad(paths) -> FStreamableHandle`
   (a `TSharedPtr`); completion delegate fires **on the game-thread tick** via a drained
   "completed" queue; assets stay resident while a handle is alive (RAII release on last drop).

4. **Texture mip streaming:** a worker computes wanted mip from **projected on-screen texel
   size**, outputs index lists; a **thread-tagged upload state machine** does async IO →
   transfer-queue staging → create a new `VkImage` of the new mip count → copy the shared
   smaller mips → swap the handle. Spread over N frames, capped per frame.

**Citrus status:** we already do worker-decode + main-thread GPU-apply + chunked bake + BC
import cache. The new ideas to adopt are the **RAII per-frame time budget**, the
**handle-as-refcount** async API, the **fence-depth frame lag** for egui smoothness, and
**projected-size mip streaming**. (Documented as follow-ups; not all built this pass.)

---

## D. Changes applied this session (2026-06-18)

1. **Reflection cube orientation fixed.** The capture rendered with a positive-height
   viewport (no Y-flip) — the only pass in the engine that didn't apply the standard
   glam→Vulkan Y-flip — so faces were upside down ("reflection on the back/wrong side"). Now
   uses the same negative-height viewport as every main pass; winding is safe because the
   capture forces no-cull. (`lib.rs step_refl_capture`.)

2. **Stale `.reflprobe` no longer masks fixes.** A **Realtime** probe now always re-captures
   live and ignores any on-disk sidecar; only a **Baked** probe loads it. Previously a stale
   sidecar hid every capture fix until a manual re-bake. (`scene.rs has_baked_reflection_probe`,
   `editor_app after_scene_loaded`.)

3. **Removed the per-material SSR/RT checkbox** (`screen_reflections`) from the shader gate and
   the material inspector — validated by UE (reflection method is global, not per-material).
   (`standard.frag`, `sections.rs`.)

4. **Reflections are now their own top-level Environment section** (not under GI), with a global
   **Type = Reflection Probes / Screen-space (SSR) / Ray-traced** (the existing `reflection_mode`,
   relabelled + relocated). (`editor_app.rs`.)

5. **Unreal-style probe priority selection.** `active_reflection_probe` now prefers a probe
   whose box **contains** the surface, then **higher Importance**, then **smaller box** — instead
   of just the nearest. (`scene.rs`.)

6. **Helmet "received no baked light" — root-caused and fixed.** The helmet is a glTF PBR
   material with an MR map, so `metallic = 1.0` is a *multiplier* (not "fully metal"). But the
   lightmap bake used the **scalar** `base_color`/`metallic` (the gbuffer is flat, no per-texel
   texture sampling), so the whole helmet baked as white fully-metallic → `diffuse = albedo *
   (1-metallic) = 0` → black lightmap. Fix: the bake now uses **texture-mean-corrected** albedo
   + metalness (`MaterialEntry::bake_albedo/bake_metallic`, computed in `apply_material` from the
   albedo + ORM map means × scalars). A textured object now bakes with its real average colour and
   metalness. *(Note: the metallic visor still correctly receives no diffuse — metals reflect; the
   fix lets the non-metallic painted regions receive baked light, and the reflection fixes above
   give the metallic regions their reflections.)*

7. **Box-projection math is now unit-tested** (`box_project_reflection` in citrus-core, the
   reference the shader must match): centre = identity, off-centre = parallax-corrected, hits the
   facing wall. Confirms the bug was orientation/stale-bake, not the projection math.

## E. Remaining reflection work (deliberately deferred — needs GPU iteration)

- **Per-fragment multi-probe BLENDING** (multiple overlapping probes blended with the smooth
  `DistanceAlpha` "Under" composite from §B.2). This needs a **cubemap ARRAY** (one captured cube
  per probe) plus an array binding through the standard shader + the SSR resolve + the skybox path.
  That's a broad renderer refactor I can't visually validate headless, so this pass ships
  **priority *selection*** (best single probe) + the correct blend/priority **algorithm documented**
  (§B.2) to implement next. The per-texel bake (sampling albedo/ORM in the bake gbuffer instead of
  the CPU mean) and **directional lightmaps** (§A.5) are the other two GPU-side follow-ups.
- **Planar reflections** — the one reflection model UE has that Citrus lacks (mirror/water). Worth
  adding after multi-probe; needs a per-planar-reflector scene re-render across a mirror plane.
</content>
