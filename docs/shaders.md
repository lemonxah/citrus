# citrus standard shader

The single default shader used by every world and avatar, on PC and mobile.
Inspired by Poiyomi: one heavily configurable **uber-shader** instead of many
small shaders, so creators toggle features instead of writing GLSL.

## Why an uber-shader

Poiyomi's model works because creators get a huge feature surface behind
checkboxes, and the shader compiler strips everything unused. We replicate
that with **SPIR-V specialization constants + compile-time variant flags**:

- Authoring time: every feature is a material property in the editor.
- Build/publish time: the material's enabled-feature set selects (or bakes) a
  shader variant; disabled features cost zero GPU time.
- Runtime: variants are cached in a pipeline cache keyed by
  `(feature_bits, vertex_layout, render_state, tier)`.

## Feature set (target, phased)

**Phase 1 — core PBR/toon hybrid**
- Base: albedo × vertex color, normal map, metallic/roughness/AO (ORM packed)
- Lighting modes: `PBR`, `Toon` (ramp or step shading), blend between them
- Emission with masks
- Alpha modes: opaque / cutout / transparent (premultiplied)
- Double-sided rendering with proper normal flip

**Phase 2 — the "Poiyomi feel"**
- Shading ramps / gradient lighting, shadow tinting
- Rim lighting (color, power, masked)
- Matcaps (add/multiply/replace, masked, 2 slots)
- Outlines (inverted-hull, color/width properties, distance fade)
- UV manipulation: tiling/offset, panning, rotation per texture slot
- Detail maps (albedo/normal second set)

**Phase 3 — flair**
- Audio-link-style reactive params (driven by engine-side audio analysis bus)
- Dissolve (alpha + edge glow), glitter/sparkle, iridescence
- Flipbook/spritesheet animation
- Vertex animation hooks (wind, jiggle handled by spring bones instead)

## Tiers

| | PC (`tier_pc`) | Mobile (`tier_mobile`) |
|---|---|---|
| Lighting | Full PBR IBL + analytic lights | Baked/SH ambient + 1 directional |
| Texture slots | All | Albedo, normal (optional), emission, ORM |
| Matcap/rim/outline | Yes | Rim yes; matcap 1 slot; outlines optional |
| Transparency | Full | Cutout strongly preferred |
| Target | Desktop Vulkan 1.3 | Quest-class (Vulkan 1.1, tiled GPU) |

Same material asset, two compilation targets. The mobile tier compiles the
same source with `TIER_MOBILE` set, which forces cheap fallbacks for features
out of budget (matcap→off, PBR→simplified BRDF, etc.). Creators see one
material; the publisher emits both variants and validates mobile budget.

## Material inspector GUI

The shader is only as good as its inspector. Creators interact with the GUI,
not the SPIR-V. The egui material inspector (citrus-editor) is a first-class
part of the shader spec:

- **Collapsible feature sections**, each with a master enable toggle in the
  header (toggle off = section collapses, variant bit clears, GPU cost gone).
  Disabled sections render dimmed, never hidden, for discoverability.
- **Search/filter bar** across all property names ("rim" jumps you to rim
  lighting), like Poiyomi's ctrl+F workflow.
- **Live preview**: edits apply to the running scene immediately, with no
  apply button. Variant recompiles happen async with the old pipeline kept
  until the new one is ready (no hitching).
- **Texture slots** as thumbnail widgets: click to assign, right-click to
  clear; per-slot UV controls (tiling/offset/pan speed/rotation) fold out
  under the thumbnail.
- **Proper widgets per type**: color = picker with HDR intensity for
  emission; ranges = sliders with drag-to-type; ramps = gradient editor
  widget; masks = channel-select dropdown (R/G/B/A).
- **Presets**: save/load named property bundles (full material or
  per-section), shippable with the platform (e.g. "Toon Skin", "Glossy
  Metal") and shareable as small JSON/RON files.
- **Tier preview toggle** in the inspector header: PC ⇄ Mobile, showing
  exactly what the mobile tier degrades, plus a live mobile-budget meter
  (texture memory, variant cost) so creators see Quest problems *while*
  authoring rather than at publish time.
- **Reset-to-default** per property (right-click) and per section.

Build order: a minimal version of this inspector ships **with M2** (phase 1
features need it for testing) and grows with each shader phase. The
inspector and the shader are one deliverable, not two.

## Implementation notes

- Source language: **Slang** or GLSL with includes, decided at milestone M2.
  Compiled to SPIR-V at build time (`glslc`/`slangc`), variants via
  specialization constants where possible, preprocessor defines where not
  (e.g. texture binding presence).
- Bindless-ish material model on PC (descriptor indexing) to keep one
  pipeline across materials with the same feature bits; classic descriptor
  sets on mobile tier.
- Material parameters serialize into the glTF material `extras` /
  a `VRSH_materials_standard` extension so worlds and VRM avatars carry vrsh
  materials portably while staying valid glTF for other tools.

## Custom shaders (v1, implemented)

User-authored GLSL fragment shaders: any `.frag` file in the project appears
in the material Shader picker. The engine prepends a fixed preamble and
compiles via `glslc` at runtime, with hot reload (~2s file polling) and the
error swirl plus compiler output in the Inspector on failure.
Files → Create → New Shader writes a commented starter.

The shader body is the fragment stage only (standard vertex stage). Do not
write a `#version` line. Provided by the preamble:

| | |
|---|---|
| Textures | `t_albedo`, `t_normal`, `t_orm`, `t_emission` (material slots) |
| Varyings | `v_world_pos`, `v_normal`, `v_uv`, `v_color`, `v_tangent` |
| Uniforms | `u_time`, `u_camera_pos`, `u_light_dir`, `u_light_color`, `u_ambient` |
| Output | `o_color` |

Properties are pragma comments, reflected into Inspector widgets and packed
into 16 push-constant floats (vec-valued properties auto-align to vec4):

```glsl
//! shader "Wobble"
//! prop tint color default(1, 0.5, 0.1, 1)
//! prop speed float range(0, 10) default(2)
//! prop flat_shaded toggle
//! prop glow_color color3 default(1, 1, 0.5)
```

Kinds: `float` (Slider, optional `range(min,max)`), `toggle` (checkbox,
0/1), `color` (RGBA), `color3` (RGB). Values are saved by property name in
`.material` files and inline scene materials, so reordering declarations is
safe. Alpha mode and double-sided still come from the material's
Transparency/Geometry sections (they drive pipeline state).

Phase 2 (designed): Slang frontend, custom vertex stage, texture-slot
properties, specialization-constant feature toggles in user shaders.
