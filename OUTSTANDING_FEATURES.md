# Citrus — Outstanding Features Backlog

Re-verified against the code on 2026-10-30. The detailed write-up, including the platform plan for Windows and Android, is in [ENGINE_REVIEW_2026-10-30.md](ENGINE_REVIEW_2026-10-30.md). Things Citrus already does are in [Features.md](Features.md).

The earlier P0/P1 gaps from the June audit are now **done**:
- runtime UI (screen, world and VR, plus the UI Editor)
- prefabs, now with internal reference remapping
- physics queries, triggers and joints
- the audio mixer and buses
- animation state machines (Animator Controllers)
- particles and the navmesh
- the save API and localization
- the event bus (`ctx.emit`)
- seamless and additive scene loading
- occlusion culling
- planar reflections
- LOD switching (`LodComponent`)
- GPU instancing of repeated draws, posed skinned-mesh bounds, TLAS refit
- full-body tracking (`Humanoid IK`: 11 tracker roles, procedural stepping), remote humanoid IK and networked UGC avatars
- LOD generation (quadric-error decimation per LOD level), compact scene files (model instances stored as their changes, unedited model materials as references)
- cloth: Bone Cloth, Mesh Cloth (self-collision), Cloth Colliders with capsule auto-fit, Wind Zones, VRM spring-bone import
- face tracking (OpenXR + OSC), lip sync, SlimeVR, voice v2 (Opus, adaptive jitter buffer, proximity)
- water: ocean / lake / river bodies, Gerstner waves with wind seas, buoyancy, drag and flow, interactive ripples, surface rendering with refraction and foam
- multi-object editing with mixed values, Group Into Empty

**Priority:** P1 = expected of a serious engine · P2 = advanced / polish.
**Effort:** S ≈ days · M ≈ 1–2 weeks · L ≈ several weeks · XL ≈ months.

## P1

| Feature | State | Effort | Note |
|---|---|---|---|
| GPU-driven culling / indirect draws | ❌ | L | CPU instancing landed; GPU culling for very large counts. |
| GPU instancing across materials | ❌ | M | Instance draws that share a mesh but differ in material values (per-instance colours / parameters in an instance buffer, like Unity's per-instance properties), so material variants don't break batches. |
| Render-to-texture: portals | ◑ | S | Cameras-to-texture with Hz throttling, off-screen skip and live resize landed; screens inside screens and portals open. |
| Reflection probes: >2 blending | ◑ | M | Per-probe cubes and two-probe blending landed everywhere (forward, SSR, RT, custom shaders); three or more at one pixel is open. |
| Android / Quest bring-up | ◑ | L | Builds, packages into a signed APK (`tools/android/build_apk.sh`) and extracts assets on first run; Vulkan 1.1 fallback, ASTC and the Quest OpenXR loader still needed, and it has never run on a device (review §2). |
| Windows on real hardware | ◑ | S | Cross-build verified under Wine only; needs the VC++ runtime. |
| Undo coverage | ◑ | M | Component and tab edits aren't undoable; structural edits (delete, reorder, group) aren't either. |
| Water: shoreline | ◑ | M | Underwater fog, waterline, blur, caustics and the ripple field in the shader landed; a depth bake for shore wave damping is open. |
| Swimming in deep water | ❌ | M | Characters swim where the water is deeper than they stand: buoyant body, stroke movement on the surface and diving below it, breath, climbing out at the edge; underwater audio and view already follow the camera. |
| Asset cook | ◑ | S | Builds ship only reachable files; packing them into one archive is open. |
| Compressed bakes | ❌ | S | Lightmaps (3 x 2048^2 = 50 MB here) and reflection-probe cubemaps (100 MB) are stored uncompressed; BC6H would cut them ~8x on disk and in VRAM. |
| NAT relay / internet play | ❌ | L | Star topology over UDP; port forwarding needed. |
| Client prediction / lag compensation | ❌ | L | The owner simulates; others interpolate. |
| Net encryption / auth | ❌ | M | LAN or trusted servers only. |

## P2

| Feature | State | Effort | Note |
|---|---|---|---|
| Humanoid retargeting | ❌ | L | |
| Sub-state machines, curve editor | ❌ | M | Animator Controller / Animation tabs. |
| Projected decals | ❌ | M | |
| Terrain + foliage | ❌ | XL | |
| Volumetric clouds | ◑ | L | Volumetric fog exists. |
| Visual shader graph | ❌ | L | Shaders are code plus inspector properties. |
| Logic in UGC (WASM scripts) | ❌ | L | See docs/user-content.md. |
| Per-package GPU freeing | ◑ | S | Freed on world switch, not on unload. |
| Cinematics / sequencer | ❌ | XL | |
| AI behaviour trees + perception | ❌ | L | |
| Vehicle / destruction | ❌ | XL | |
| Virtual shadow maps, meshlets | ❌ | XL | |
| Console platforms | ❌ | XL | |

## Suggested sequence

1. GPU-driven culling for very large scenes.
2. Android / Quest 3 bring-up.
3. BC6H-compressed bakes (lightmaps, reflection probes: ~8x smaller), asset cook.
4. Internet play: a relay or Steam networking, then prediction.
5. Avatar scaling to the player's height, in-editor pin painting for Mesh Cloth.
