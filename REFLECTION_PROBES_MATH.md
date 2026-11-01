# Reflection-Probe Cubemapping: Mathematics & Implementation Reference

A self-contained reference for ZestEngine's reflection-probe + IBL pipeline (Rust + Vulkan).
Equations/code are quoted from primary sources where possible. Sources at the end.

> **Authorship correction:** The parallax-corrected cubemap work is by **Sébastien Lagarde
> & Antoine Zanuttini** (SIGGRAPH 2012), not "Zucconi."

> **ZestEngine status callouts** are flagged inline as `> ZESTENGINE:` — they map each piece of the
> math to where it lives in our code, and whether we match the reference.

---

## 1. Cubemap Face Conventions

### 1.1 Face selection table (sc, tc, ma)

Given a sampling direction `(rx, ry, rz)`, the face is chosen by the **largest-magnitude
coordinate** (the *major axis direction*). Identical across OpenGL `ARB/EXT_texture_cube_map`,
the GL core spec, and the Vulkan spec (**Cube Map Face and Coordinate Selection**; Vulkan
names `ma` as `rc`).

```
 major axis
 direction     target (face)               layer   sc     tc     ma
  +rx          CUBE_MAP_POSITIVE_X (+X)       0     -rz    -ry    rx
  -rx          CUBE_MAP_NEGATIVE_X (-X)       1     +rz    -ry    rx
  +ry          CUBE_MAP_POSITIVE_Y (+Y)       2     +rx    +rz    ry
  -ry          CUBE_MAP_NEGATIVE_Y (-Y)       3     +rx    -rz    ry
  +rz          CUBE_MAP_POSITIVE_Z (+Z)       4     +rx    -ry    rz
  -rz          CUBE_MAP_NEGATIVE_Z (-Z)       5     -rx    -ry    rz
```

### 1.2 Coordinate transform (u, v)

```
s = ( sc/|ma| + 1 ) / 2
t = ( tc/|ma| + 1 ) / 2
```
The divisor is `|ma|`, the **absolute** major-axis magnitude.

> ZESTENGINE: `zestengine-render/src/lib.rs::ReflProbeJob::face_dir(face, fx, fy)` is the inverse of
> this table and is used by `build_face_mip0` to build the skybox cube from the equirect map.
> Verified to match the spec exactly:
> `+X → [1,-fy,-fx]`, `-X → [-1,-fy,fx]`, `+Y → [fx,1,fy]`, `-Y → [fx,-1,-fy]`,
> `+Z → [fx,-fy,1]`, `-Z → [-fx,-fy,-1]`. Since the skybox renders correctly, this convention
> is self-consistent and is our ground-truth orientation.

### 1.3 Left-handed sampling convention (RenderMan inheritance)

`ARB_texture_cube_map` issues:
> "The coordinate system is left-handed if you think of yourself within the cube… This matches
> the convention of the RenderMan interface."

The sc/tc sign pattern implements a **left-handed** sampling basis with texture **v pointing
down** (upper-left origin per face). Capturing the 6 faces with a right-handed `lookAt`
(y-up, v-up framebuffer) is handedness/vertically mismatched against this — which is why the
canonical capture up-vectors are mostly `(0,-1,0)`.

### 1.4 Per-face capture view/up vectors (canonical glm::lookAt)

`eye = capturePos`, `center = eye + dir`:

```
 Face   Forward dir (center - eye)    Up vector
  +X    ( 1.0,  0.0,  0.0)            ( 0.0, -1.0,  0.0)
  -X    (-1.0,  0.0,  0.0)            ( 0.0, -1.0,  0.0)
  +Y    ( 0.0,  1.0,  0.0)            ( 0.0,  0.0,  1.0)
  -Y    ( 0.0, -1.0,  0.0)            ( 0.0,  0.0, -1.0)
  +Z    ( 0.0,  0.0,  1.0)            ( 0.0, -1.0,  0.0)
  -Z    ( 0.0,  0.0,-1.0)            ( 0.0, -1.0,  0.0)
```

**Vulkan caveat (§5):** NDC/framebuffer Y points *down*, so the up vectors (esp. ±Y) must be
retuned per engine, or top/bottom faces flip. The **dir** vectors stay the same.

> ZESTENGINE: `step_refl_capture` uses exactly these dir/up pairs with `Mat4::look_at_rh` + a
> **positive-height** viewport. Traced per-face: pixel `(fx,fy)` on +X → world dir `[1,-fy,-fx]`,
> on +Z → `[fx,-fy,1]`, on +Y → `[fx,1,fy]` — i.e. it reproduces `face_dir` (§1.2) exactly.
> So the captured probe cube and the skybox cube share one convention. (See §5.2 on why the
> positive-height viewport is correct here, not the main pass's negative-height flip.)

### 1.5 Seamless edge handling (Vulkan)

Cross-face filtering is **on by default** in Vulkan unless
`VK_SAMPLER_CREATE_NON_SEAMLESS_CUBE_MAP_BIT_EXT` is set (contrast OpenGL, which needs
`GL_TEXTURE_CUBE_MAP_SEAMLESS`). At face corners the texel is the average of the three
incident faces.

---

## 2. The Reflection Vector

### 2.1 GLSL `reflect()`

```
reflect(I, N) = I − 2 · dot(N, I) · N        // N must be unit-length; I is the incident vector
```
`reflect()` does **not** normalize `N`. Output magnitude equals `|I|`.

### 2.2 Why shaders use `R = reflect(-V, N)`

- `reflect(I,N)` expects `I` = **incident** = camera→surface (arriving ray).
- PBR shaders define `V` = **view** = surface→camera = `normalize(cameraPos − fragPos)`.

These are antiparallel, so `I = -V`:
```
R = reflect(-V, N) = 2·(N·V)·N − V        // "mirror the view vector about the normal"
```
Passing `+V` flips R and samples the opposite hemisphere — the classic "reflection on the
wrong side / slides the wrong way" bug.

> ZESTENGINE: `standard.frag:784` is `vec3 R = reflect(-V, N);` with
> `V = normalize(frame.camera_pos.xyz - v_world_pos)` (`:661`). **Correct** per the reference.

### 2.3 Mirror-sphere empirical validation (use this to judge correctness)

From `R = 2(N·V)N − V`:

- **Center of the sphere** (normal faces camera, `N=V`, `N·V=1`): `R = V` → shows the
  environment **directly BEHIND the camera**. *(A real chrome ball shows the photographer as
  the central dot.)* **If the center shows what's in FRONT, the incident sign is flipped.**
- **Silhouette / grazing edge** (`N·V≈0`): `R ≈ −V` → shows the environment **in FRONT** of
  the camera; the scene wraps around the rim.
- **Transition:** moving center→rim, R sweeps continuously from `V` (behind) to `−V` (front).

> ZESTENGINE takeaway: a chrome sphere catching the **sun near its silhouette** while the sun is in
> front of you is **CORRECT**, not a bug. The sun only belongs at the *center* if it's behind
> the camera. Judge orientation with the in-engine debug views (§6.9), which we added as render
> modes 4 (mirror) and 5 (reflection-vector-as-RGB).

---

## 3. Parallax-Corrected / Box-Projected Cubemaps (Lagarde & Zanuttini, SIGGRAPH 2012)

### 3.1 AABB ray-box intersection (exact HLSL from Lagarde)

```hlsl
float3 DirectionWS     = PositionWS - CameraWS;
float3 ReflDirectionWS = reflect(DirectionWS, NormalWS);

float3 FirstPlaneIntersect  = (BoxMax - PositionWS) / ReflDirectionWS;
float3 SecondPlaneIntersect = (BoxMin - PositionWS) / ReflDirectionWS;
float3 FurthestPlane = max(FirstPlaneIntersect, SecondPlaneIntersect);
float  Distance = min(min(FurthestPlane.x, FurthestPlane.y), FurthestPlane.z);

float3 IntersectPositionWS = PositionWS + ReflDirectionWS * Distance;
ReflDirectionWS = IntersectPositionWS - CubemapPositionWS;   // re-vector
return texCUBE(envMap, ReflDirectionWS);
```
Specialized slab test for a point **inside** the box: only the furthest of each plane pair
(`max`) and the nearest exit (`min`).

### 3.2 Re-vector step

```
R' = P_hit - C        // P_hit = intersect point, C = CUBEMAP CAPTURE POSITION
```

### 3.3 Box (proxy geometry) vs CAPTURE position — the crucial distinction

Lagarde (verbatim): **"It should be noted that the box center doesn't need to match the
cubemap center."**

Two **independent** quantities:
1. **Proxy box** `BoxMin/BoxMax` — approximates the room surfaces; used **only** to find
   where the ray lands (`P_hit`).
2. **Capture position** `C` (`CubemapPositionWS`) — the exact point the cube was rendered
   from. Faces encode the world *as seen from C*, so a world point `P_hit` is only correctly
   retrieved along `P_hit − C`.

Re-vectoring with the **box center** `B` instead of `C` asks the cube for "what's visible from
C along `P−B`" — the wrong texel whenever `B ≠ C`. Result: reflections **spatially shifted off
the surfaces they belong to**.

> ZESTENGINE — RESOLVED: verified that ZestEngine captures the cube AT the box center
> (`active_reflection_probe` returns the probe's world position as `center`, which feeds BOTH
> `request_reflection_capture(center)` and `ubo.refl_center`). So capture position == box
> center, and `R = hit - refl_center` IS the correct Lagarde re-vector — no separate capture
> position needed. ADDED (2026-06-18): an **inside-box guard** so the parallax only applies to
> surfaces actually within the probe box; a surface outside it (a floor extending past the
> probe) now falls back to the infinite-cube direction instead of a mis-placed parallax hit —
> that was a real cause of "floor reflecting incorrectly" when the floor exceeds the probe box.

### 3.4 OBB generalization

Transform ray (3×3, rotation/scale) + position (full affine) into the box's unit-cube space,
slab-test against `[-1,1]³`, recover `Distance`, apply it to the **world-space** reflect dir,
then re-vector against `C` exactly as AABB. Local space only makes the slab test trivial.

---

## 4. Prefiltered Environment Map / Split-Sum (Karis, UE4 2013)

### 4.1 Split-sum

```
∫ Li f cosθ dl ≈ ( (1/N) Σ Li(lk) ) · ( (1/N) Σ [ f(lk,v) cosθ / p(lk,v) ] )
```
First factor = **pre-filtered env map**; second = **environment BRDF** (LUT). Exact for a
constant environment.

### 4.2 Pre-filter + the `R = N = V` assumption

```hlsl
float3 PrefilterEnvMap(float Roughness, float3 R){
    float3 N=R, V=R; float3 col=0; float TotalWeight=0;
    for(uint i=0;i<1024;i++){
        float2 Xi=Hammersley(i,1024);
        float3 H=ImportanceSampleGGX(Xi,Roughness,N);
        float3 L=2*dot(V,H)*H-V;
        float NoL=saturate(dot(N,L));
        if(NoL>0){ col+=EnvMap.SampleLevel(s,L,0).rgb*NoL; TotalWeight+=NoL; }
    }
    return col/TotalWeight;
}
```
Karis: assuming `n=v=r` is the **largest** error source — it loses the elongated grazing-angle
stretch. **Mip↔roughness is an implementation choice** (no closed form in Karis):
LearnOpenGL uses `sample = textureLod(prefilter, R, roughness*MAX_LOD)` (`MAX_LOD=4`).

> ZESTENGINE — FIXED (2026-06-18): `ENV_MAX_LOD` was a hardcoded `6.0` (= `cube_mip_count(64)-1`,
> right for the 64px skybox cube) but a CAPTURED probe cube is 256px (9 mips), so `roughness=1`
> sampled mip 6 of 9 → reflections **too sharp** on rough surfaces (a defined blob on a rough
> floor instead of a smooth wash). Now `#define ENV_MAX_LOD (float(textureQueryLevels(u_env)-1))`
> in both `standard.frag` and `ssr_resolve.frag` — reads the bound cube's real mip count at
> runtime, correct for either cube.

### 4.3 Environment BRDF LUT (scale/bias to F0)

`prefiltered · (F0 · scale + bias)`. Karis stores the LUT as R16G16 (precision matters).

### 4.4 Karis EnvBRDFApprox (analytic, no LUT)

```glsl
half3 EnvBRDFApprox(half3 SpecularColor, half Roughness, half NoV){
    const half4 c0={-1,-0.0275,-0.572,0.022};
    const half4 c1={ 1, 0.0425, 1.04,-0.04};
    half4 r=Roughness*c0+c1;
    half a004=min(r.x*r.x,exp2(-9.28*NoV))*r.x+r.y;
    half2 AB=half2(-1.04,1.04)*a004+r.zw;
    return SpecularColor*AB.x+AB.y;
}
```
> ZESTENGINE: `standard.frag::env_brdf_approx(roughness, NdotV)` is this function — used at `:819`.
> Matches the reference. (Lazarov's `a004` dielectric fit is the same curve reparameterized.)

---

## 5. Capture Orientation Pitfalls in Vulkan

### 5.1 Vulkan NDC +Y is DOWN; framebuffer origin top-left
Viewport transform has **no** Y negation (`o_y = y + height/2`, `p_y = height`); fragments are
located from the **upper-left**. The optional **negative-height** viewport
(`VK_KHR_maintenance1`) negates clip-space Y to emulate OpenGL's bottom-left origin.

### 5.2 Why cube CAPTURE wants a POSITIVE-height viewport
The cube sampling convention (§1) is fixed: left-handed, **+Y up, upper-left origin**. Willems
(#679): the original cube system is LH +Y-up; Vulkan is LH **+Y-down**, so reconciling needs a
**180° rotation about X**. His `shadowmappingomni` cube-capture uses a **positive-height
viewport** and bakes per-face orientation into the **view matrices**. Rule: the negative-height
flip is for the **swapchain pass only**; offscreen cube faces stay logically un-flipped.

**Failure mode:** reusing the main-pass negative-height flip for cube faces **double-flips** →
faces vertically inverted vs the sampling convention.

> ZESTENGINE: we already do this — capture uses a positive-height viewport with no-cull
> (`capturing_refl`); the main pass uses negative-height. Confirmed consistent with `face_dir`.

### 5.3 Winding / culling
Front/back = signed polygon area in **framebuffer** coords (post-viewport). A negative-height
viewport flips area sign → reverses apparent winding. The Y-flip and frontFace/cull are
**independent levers to keep consistent per render target**. ZestEngine sidesteps this in capture
by forcing **no-cull** (`force_no_cull = capturing_refl`).

### 5.4 Vertical flip vs handedness/horizontal mirror — distinguishing symptoms
Determinant rule: **odd** axis negations (det −1) = reflection (mirror, reversed winding,
backwards text); **even** (det +1) = rotation (orientation preserved, readable text).

- Upside-down but **readable** text → det +1 → **viewport / UV Y-origin** bug.
- **Left-right mirror, backwards** text, up still up → det −1 → **handedness** bug (stray sign).
- Cubemap-specific: a Y-origin bug looks **different per face** (sides rotated, top/bottom maybe
  fine); a true handedness bug makes **every** face read backwards **uniformly**.

---

## 6. Common Bugs & Diagnosis Checklist (mechanism → symptom)

| # | Bug | Symptom |
|---|---|---|
| 6.1 | **Handedness mismatch** (sample RH world dir against LH cube) | Reflections **mirrored L↔R**; text backwards; every face reads backwards uniformly. Fix: negate Z (or flip X) of the sample dir. |
| 6.2 | **Y-flip at capture / swapped ±Y faces** | Reflections **vertically flipped**; sky/ground swapped. Differs per face. |
| 6.3 | **Incident-vector / reflect sign** (passed `+V`, or `cameraPos−Position`, or space mismatch) | Reflection **slides opposite** to camera; wrong side; or barely responds. |
| 6.4 | **Box center used instead of capture position** (parallax) | Reflections **offset / anchored to the wrong point**; reflected geometry **doesn't line up** with real geometry ("not between camera and object"). Wrong box *size* → reflections **stretch/slide** as camera moves. |
| 6.5 | **Wrong mip for roughness** (LOD constant ≠ mip count, or hardware `texture()`) | Rough surfaces too sharp/too blurry; roughness has little effect; fireflies; mip seams. |
| 6.6 | **sRGB vs linear at capture/prefilter** | Reflections too bright/washed or too dark/over-saturated; don't match direct lighting. |
| 6.7 | **No seamless filtering / face-by-face prefilter** | Hard **seams along cube-face edges**, worst on blurry mips. |
| 6.8 | **Skybox inside-out / wrong face order** | Skybox disappears/black, or environment scrambled/rotated. |

> ZESTENGINE — status (2026-06-18): the reflection pipeline is now fixed end-to-end:
> - **6.1/6.2/6.3 (orientation / handedness / reflect sign):** PROVEN CORRECT by an executable
>   round-trip test (`refl_capture_tests::cube_capture_matches_sampling`) that simulates the GPU
>   capture (look_at_rh + perspective_rh + positive viewport) for all 6 faces × 7 sample points
>   and asserts it equals `face_dir`. Not a guess — it's CI-checked. Plus
>   `mirror_center_reflects_behind_camera` guards the reflect-sign invariant.
> - **6.4 (box coverage):** capture == box center (correct); added the inside-box guard (§3.3).
> - **6.5 (roughness mip):** fixed via `textureQueryLevels` (§4.2).
> - **Dark scene in reflections:** was the FluxVoxel light filter dropping all lights from the
>   capture (`gather_lights_all`) — fixed earlier.
> Remaining if anything still looks off: it's authoring (probe box not sized to the room) or
> §6.6 (the capture is tonemapped LDR `R8G8B8A8_UNORM`; brightness, not position). Use render
> modes 4/5 to confirm.

### 6.9 On-screen DEBUG views (X→R, Y→G, Z→B, shown as `dir*0.5+0.5`)

- **A. Labeled cube-cross blit** — unfold 6 faces as a 4×3 cross; a correct cube shows a
  **continuous horizon** across −X→+Z→+X→−Z with no rotation jumps.
- **B. Reflection vector as RGB** — output `reflect(viewDir,N)*0.5+0.5`; on a sphere R should
  sweep smoothly across the color wheel. Flat/inverted/stuck channel → R is wrong (§6.3).
- **C. Sampled cube direction as RGB** — output `normalize(dir)*0.5+0.5`; +Z→blue, up→green.
  Tells whether a misorientation is in **direction math** (this) vs **face arrangement** (A).

> ZESTENGINE: render modes **4** (perfect mirror of the cube — variant of C/A) and **5**
> (reflection vector as RGB — view B) are wired in (`standard.frag` gi_dbg 4/5, editor render-
> mode dropdown → "Reflection debug"). Use mode 5 on a sphere: if the center is **not** the
> color of the behind-camera direction, it's a §6.3 sign bug; if every face mirrors, §6.1.

---

## Sources

**Cube conventions / sampling**
- Vulkan spec — Textures (face selection, sface/tface, edge handling): https://docs.vulkan.org/spec/latest/chapters/textures.html
- `VK_EXT_non_seamless_cube_map`: https://docs.vulkan.org/features/latest/features/proposals/VK_EXT_non_seamless_cube_map.html
- OpenGL `ARB_texture_cube_map`: https://registry.khronos.org/OpenGL/extensions/ARB/ARB_texture_cube_map.txt
- LearnOpenGL — Point Shadows (6-face lookAt): https://learnopengl.com/Advanced-Lighting/Shadows/Point-Shadows
- LearnOpenGL — Cubemaps: https://learnopengl.com/Advanced-OpenGL/Cubemaps

**Reflection vector**
- Khronos GLSL builtins (`reflect`): https://docs.vulkan.org/glsl/latest/chapters/builtinfunctions.html

**Parallax-corrected cubemap**
- Lagarde blog (canonical code): https://seblagarde.wordpress.com/2012/09/29/image-based-lighting-approaches-and-parallax-corrected-cubemap/
- SIGGRAPH 2012 slides: https://seblagarde.wordpress.com/wp-content/uploads/2012/08/parallax_corrected_cubemap-siggraph2012.pdf
- Interplay of Light (corroboration): https://interplayoflight.wordpress.com/2013/04/29/parallax-corrected-cubemapping-with-any-cubemap/

**Split-sum / prefilter / env BRDF**
- Karis, "Real Shading in UE4" (SIGGRAPH 2013): https://blog.selfshadow.com/publications/s2013-shading-course/karis/s2013_pbs_epic_slides.pdf
- Lazarov, "Getting More Physical in CoD: Black Ops II": https://blog.selfshadow.com/publications/s2013-shading-course/lazarov/s2013_pbs_black_ops_2_notes.pdf
- Karis EnvBRDFApprox (mirror): https://lygia.xyz/lighting/common/envBRDFApprox
- LearnOpenGL — Specular IBL: https://learnopengl.com/PBR/IBL/Specular-IBL
- bruop, IBL (mip solid-angle, seams): https://bruop.github.io/ibl/

**Vulkan orientation**
- Vulkan spec — Controlling the Viewport: https://registry.khronos.org/vulkan/specs/latest/html/vkspec.html#vertexpostproc-viewport
- VK_KHR_maintenance1 (negative height): https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_maintenance1.html
- Sascha Willems — Flipping the Vulkan viewport: https://www.saschawillems.de/blog/2019/03/29/flipping-the-vulkan-viewport/
- Sascha Willems — shadowmappingomni (positive-height cube capture): https://github.com/SaschaWillems/Vulkan/blob/master/examples/shadowmappingomni/shadowmappingomni.cpp
- Sascha Willems — issue #679 (cube LH +Y-up vs Vulkan +Y-down, 180° X): https://github.com/SaschaWillems/Vulkan/issues/679

**Bugs / debug**
- Wikipedia — Cube mapping: https://en.wikipedia.org/wiki/Cube_mapping
- Castaño — Seamless cube map filtering: http://www.ludicon.com/castano/blog/articles/seamless-cube-map-filtering/
- Unity — Advanced Reflection Probe (parallax box): https://docs.unity3d.com/Manual/AdvancedRefProbe.html

---

## Verification caveats
1. "Left-handed" is the standard *interpretation* of the sign table (pinned via DirectX7 /
   RenderMan reference), not a verbatim spec phrase.
2. Mip = f(roughness) is **not** in Karis — it's an implementation choice (LearnOpenGL linear).
3. LearnOpenGL's continuous split omits the BRDF-term `1/N`; the prefilter `/totalWeight`
   reconciles it with Karis.
4. The "negative-height flip mandates a frontFace swap" lore is imprecise — the Y-flip and
   frontFace/cull are independent levers to keep consistent per render target.
