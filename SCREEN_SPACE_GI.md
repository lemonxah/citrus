# Screen-space GI (Lumen-style) — roadmap

Goal: replace the blocky world-probe-grid indirect lighting with **screen-space
radiance probes** (Lumen's final gather), so indirect light is reconstructed at
near-screen resolution instead of a coarse 3D grid. The world-probe grid stays as
the **fallback** for rays that leave the screen (off-screen / far light), exactly
as Lumen layers screen probes over its World Radiance Cache.

Implemented purely from the public Lumen technique (SIGGRAPH 2022 course notes,
Epic docs) — no UE source porting.

**Constraint:** the assistant cannot run Vulkan/GPU here. Every phase is a
buildable step the user tests on their GPU; expect iteration.

## Starting point (renderer map)
- Forward renderer, **single color attachment**, no G-buffer/MRT.
- Depth target is attachment-only (DONT_CARE store) — **not sampleable**.
- No screen normals / world-position buffer.
- No inverse matrices in the frame UBO (added in A.1).
- GPU GDF march (`gpu_gi.rs`) exists + is async — reuse it for probe tracing.
- GI integration point: `standard.frag` `sample_probes` → `indirect` (~line 529-549).
- Probe upload: `set_baked_probes` / `update_probe_sh`, set 0 binding 2 SSBO + UBO `probe_volumes[]`.

## Phases

### Phase A — Foundation
- [x] **A.1** Add `inv_view` / `inv_proj` to `FrameUbo` (+ populate). Appended last
  so existing shaders reading a FrameData prefix are unaffected.
- [x] **Debug view** (View menu → GI Debug View): world-normals + indirect-only
  modes (`FrameInput.gi_debug` → `frame.debug.y` → `standard.frag`). Lets the
  blockiness be inspected directly + makes each later phase verifiable on GPU.
  Interim: lower **Probe Spacing** to shrink the grid cells while watching the
  "Indirect GI only" view.
- [ ] **A.2** Depth+normal **prepass**: render geometry to a sampleable depth
  target + a world-normal color target (RG/RGBA16F) before the main pass. New
  prepass pipeline + minimal vert/frag. Barrier to SHADER_READ before the gather.

### Phase A.2 — Depth prepass (DONE)
- [x] `ScreenGiTargets` (sampleable depth + RGBA16F gather image), recreated on
  resize. Depth prepass recorded each frame via the **shadow pipeline** (color-less
  depth-only) into `sgi.depth`, then barriered to SHADER_READ.

### Phase B/C — Screen-space gather (DONE — implemented, needs GPU validation)
- [x] **`screen_gi.comp`**: one invocation per pixel — reconstruct world pos from
  depth + `inv_view_proj`, reconstruct normal from depth derivatives, trace a
  cosine hemisphere against the cached GDF (reusing the `sw_gi.comp` march), output
  screen-resolution irradiance to `sgi.gi`. Emission picked up on GDF hit.
- [x] `GpuGi::screen_resolve` — compute pipeline + dispatch (synchronous for now).
- [x] **Forward integration**: set-0 binding 4 = `u_screen_gi`; `standard.frag`
  samples it for the indirect term when `frame.debug.z > 0.5` (screen-GI active =
  GDF present = software realtime GI on). Falls back to world probes otherwise.

**Runs when:** Environment → Realtime GI **enabled + Software mode** (that's what
populates the GDF the gather traces).

**Still TODO (optimize / iterate on GPU):**
- [ ] Async the gather (currently synchronous = blocking submits per frame).
- [ ] Verify depth→world reconstruction with the Y-flipped viewport; fix if the
  gather looks vertically flipped / offset.
- [ ] Temporal + spatial filter (few rays/frame stability); half-res + upsample.
- [ ] NEE emitter spheres for the screen gather (currently emission-on-hit only).

### Phase D — Polish
- [ ] Adaptive probes in high-contrast/disoccluded tiles; importance-sampled rays.
- [ ] Tune octahedral res / ray budget / temporal window.

## Notes
- Y-flipped viewport (negative-height) — watch screen-UV↔NDC conventions in
  depth→world reconstruction.
- Keep the world-probe path intact as the fallback throughout.
