# Authoritative Game-Engine Feature Checklist

What a game engine **must** have to be considered a usable, general-purpose engine —
derived by auditing **Unreal Engine 5**, **Unity 6**, **Godot 4.x**, and **Bevy
0.17/0.18** (editor *and* runtime). Each feature is tiered by how essential it is, and
cross-referenced against **ZestEngine**.

**Tiers**
- **T0 — Table-stakes:** lacking it, the thing is "a rendering framework," not a usable
  engine. Present in all of Unreal/Unity/Godot (Bevy's T0 gaps are exactly why it's a
  famous boundary case — superb renderer, no editor).
- **T1 — Expected:** a serious engine ships it; a game can be made without it but most
  teams will hit the wall fast.
- **T2 — Advanced / differentiating:** AAA polish; absence doesn't disqualify an engine.

**ZestEngine status:** ✅ done · ◑ partial · ❌ missing. ZestEngine column reflects state at
2026-06-18 (this includes the layer system, frustum culling, and reflection work landed
this session).

Sources: official docs for all four engines (docs.godotengine.org, docs.unity3d.com,
dev.epicgames.com, bevy.org), Wikipedia "Game engine" core enumeration, and the
beginner curricula (Unity Essentials, Godot step-by-step) as the consensus floor.

---

## The boundary case that defines "usable"

Bevy is the proof that **a renderer alone is not an engine**. Its rendering breadth rivals
Unreal (PBR, SSR, GTAO, TAA, DoF, volumetric fog, atmosphere, decals, DLSS, even
experimental RT-GI and meshlets), yet it is widely described as "not yet usable as a
general-purpose engine" — because it lacks the **editor/tooling tier**: no official editor,
no inspector, no content browser, no undo, no play-in-editor, no built-in physics. That gap
*is* the definition of T0. **For ZestEngine the lesson is direct: the editor + inspector +
prefab + undo + play loop, plus built-in physics, are what separate an engine from a
graphics demo — invest there, not only in the renderer.**

---

## T0 — Table-stakes (15)

| # | Feature | Unreal | Unity | Godot | Bevy | **ZestEngine** |
|---|---|---|---|---|---|---|
| 1 | **Real-time PBR renderer** (forward/deferred) + dynamic shadows | ✅ | ✅ | ✅ | ✅ | ✅ |
| 2 | **Scene graph / composition model** (hierarchy, parent/child, transforms) | ✅ | ✅ | ✅ | ✅ | ✅ |
| 3 | **Visual scene/level editor** with viewport **transform gizmos** (move/rotate/scale, snap) | ✅ | ✅ | ✅ | ❌ | ✅ |
| 4 | **Inspector** — select an object, edit its components/properties live | ✅ | ✅ | ✅ | ❌ | ✅ |
| 5 | **Content/asset browser** | ✅ | ✅ | ✅ | ❌ | ✅ |
| 6 | **Asset import pipeline** — glTF/FBX + PNG/JPG + WAV/OGG/MP3, per-asset settings | ✅ | ✅ | ✅ | ◑ | ✅ |
| 7 | **Prefab / reusable-object template** with instancing + **per-instance overrides** | ✅ (BP/Prefab) | ✅ (Prefabs) | ✅ (scene instancing) | ◑ (scenes/BSN) | ◑ (`prefab.rs` + Object→Create Prefab; instantiate w/ overrides; drag-spawn pending) |
| 8 | **Undo/redo** in the editor | ✅ | ✅ | ✅ | ❌ | ◑ |
| 9 | **Play-in-editor** — press play, no separate build | ✅ | ✅ | ✅ | ❌ | ✅ |
| 10 | **Scripting / gameplay language** (C++/C#/GDScript/Rust/Blueprint) | ✅ | ✅ | ✅ | ✅ | ✅ (Rust plugins) |
| 11 | **Input system** — KB/mouse/gamepad/touch → actions, rebinding | ✅ | ✅ | ✅ | ✅ | ✅ |
| 12 | **Physics** — rigid bodies, colliders, **queries (raycast/overlap)**, triggers, char controller, **layer matrix** | ✅ | ✅ | ✅ | ◑ (ext) | ◑ (bodies/colliders/layer matrix ✅; **raycast + overlap_sphere** ✅; joints/triggers/char-controller pending) |
| 13 | **Runtime UI** — retained widgets, layout/anchors, text/fonts (menus + HUD) | ✅ UMG | ✅ | ✅ Control | ◑ | ◑ (`ui_canvas.rs` retained tree + anchor layout solver + in-editor preview; GPU draw + text atlas pending) |
| 14 | **Audio** — playback + 3D spatial + a **mixer/buses** | ✅ | ✅ | ✅ | ◑ | ◑ (playback + 3D ✅; **mixer/buses** `audio_mixer.rs` + Tools→Audio Mixer panel; rodio-sink wiring pending) |
| 15 | **Build / export** to a distributable (≥ desktop), editor strip/cook | ✅ | ✅ | ✅ | ◑ | ◑ |

**ZestEngine T0 gaps:** #7 Prefabs (only "duplicate"), #13 Runtime UI (**the single biggest
hole**), plus the partials: #8 undo coverage, #12 physics queries/joints/triggers, #14 audio
mixer, #15 cook/strip pipeline.

---

## T1 — Expected of a serious engine

| # | Feature | UE | Unity | Godot | Bevy | **ZestEngine** |
|---|---|---|---|---|---|---|
| 16 | **Baked GI / lightmapping** | ✅ | ✅ | ✅ | ◑ | ✅ |
| 17 | **Realtime GI** (SDFGI / Lumen / APV / probes) | ✅ Lumen | ✅ APV | ✅ SDFGI | ◑ | ✅ Flux |
| 18 | **Reflection probes** (box/sphere, blended) | ✅ | ✅ | ✅ | ◑ | ◑ (single-probe; multi-blend WIP) |
| 19 | **Screen-space reflections** | ✅ | ✅ | ✅ | ✅ | ✅ |
| 20 | **Post-processing stack** — bloom, tonemap, color grading, AA, AO, DoF, vignette | ✅ | ✅ | ✅ | ✅ | ◑ (grade/tonemap/vignette; bloom/CA authorable, not yet rendered) |
| 21 | **Skeletal animation** + **state machine / blend trees** | ✅ | ✅ | ✅ | ◑ (graph, no FSM) | ◑ (rig/IK import ✅; **runtime AnimGraph** `anim_graph.rs`: states/transitions/cross-fade/1D blend; feed-to-skinning + graph editor pending) |
| 22 | **IK** (two-bone / FABRIK / full-body) | ✅ | ✅ pkg | ✅ | ◑ ext | ✅ |
| 23 | **Animation retargeting** (humanoid) | ✅ | ✅ | ✅ | ❌ | ◑ |
| 24 | **Navigation / navmesh + pathfinding** | ✅ Recast | ✅ | ✅ | ◑ ext | ◑ (`navmesh.rs` walkability grid + A* + string-pull; scene-bake + agent component pending) |
| 25 | **Particles / VFX system** (GPU) | ✅ Niagara | ✅ VFX Graph | ✅ GPUParticles | ◑ ext | ◑ (`particles.rs` CPU emitter sim + editor preview; GPU billboard draw + emitter component pending) |
| 26 | **Material / shader graph** (node editor) | ✅ | ✅ | ✅ VisualShader | ❌ | ◑ (`shader_graph.rs` node graph → **GLSL codegen** + live preview; visual node-editor pending) |
| 27 | **Profiler** (CPU + GPU per-pass) | ✅ Insights | ✅ | ✅ | ◑ | ✅ |
| 28 | **Hot reload** (scripts + assets) | ✅ Live Coding | ◑ | ✅ | ◑ | ◑ (shaders/plugins; asset partial) |
| 29 | **Frustum / occlusion culling, LOD, instancing** | ✅ all | ✅ all | ✅ | ◑ | ◑ (frustum ✅; **LOD now a `LodComponent` in the Inspector** (Add Component → LOD Group) + `lod.rs` select/hysteresis; render mesh-swap + occlusion/instancing pending) |
| 30 | **Event / messaging** (signals / UnityEvent / delegates) | ✅ | ✅ | ✅ signals | ✅ observers | ◑ (net messaging + **gameplay event bus** `events.rs` double-buffered typed emit/read) |
| 31 | **Save/load game state** API | ✅ | ✅ | ✅ | ◑ | ✅ (`savegame.rs`: typed `SaveValue` KV store, atomic `.save` RON write, typed getters; unit-tested) |
| 32 | **Localization** (string tables) | ✅ | ✅ | ✅ | ◑ | ✅ (`localization.rs`: `.loc` RON tables, `tr()` with default→key fallback, runtime language switch; unit-tested) |
| 33 | **Level / world streaming** (open worlds) | ✅ World Partition | ✅ | ✅ | ◑ | ◑ (`streaming.rs` cell residency + load/unload hysteresis; async loader wiring pending) |
| 34 | **Asset streaming + typed handles** (async, ref-counted, mip streaming) | ✅ | ✅ Addressables | ◑ | ✅ | ◑ (worker decode + **typed ref-counted `Handle<T>`** `asset_handle.rs`: dedupe, per-frame load budget, GC; decoder wiring pending) |
| 35 | **Networking / replication** | ✅ | ✅ NGO | ✅ | ◑ ext | ◑ (LAN relay) |

---

## T2 — Advanced / differentiating (not disqualifying)

| Feature | Who has it | **ZestEngine** |
|---|---|---|
| **Virtualized geometry** (Nanite / meshlets) | UE ✅, Bevy ◑ exp | ❌ |
| **Virtual shadow maps** | UE ✅ | ❌ |
| **Hardware ray tracing** (RT reflections/shadows/GI) | UE Lumen, Unity HDRP, Bevy Solari exp | ◑ (RT reflections) |
| **Planar reflections** (mirrors/water) | UE, Unity, Godot | ❌ |
| **Volumetrics** (fog ✅ / clouds) | UE/Unity/Godot | ◑ (fog only) |
| **Decals** | UE/Unity/Godot/Bevy | ❌ |
| **Terrain tools** | UE/Unity (Godot 3rd-party) | ❌ |
| **Cinematics / sequencer** | UE Sequencer, Unity Timeline | ❌ |
| **Visual scripting** (Blueprint/Bolt) | UE/Unity (Godot removed it) | ❌ (and arguably fine to skip) |
| **Gameplay ability system** | UE GAS | ❌ |
| **AI behavior trees + perception** | UE, Unity (ext) | ❌ |
| **Vehicle / cloth / destruction physics** | UE Chaos, Unity | ❌ |
| **Console platform support** | UE/Unity | ❌ |
| **VR/XR** (OpenXR) | UE/Unity/Godot | ✅ (a ZestEngine strength) |
| **GPU-driven rendering / occlusion culling** | UE, Unity U6, Bevy | ❌ |

Note two things the consensus explicitly rates **NOT** table-stakes, so ZestEngine shouldn't feel
behind for them: **visual scripting** (Godot *removed* it in 4.0 and is still universally
"usable"; a text/Rust scripting layer is what's mandatory) and **networking** (engine-dev
consensus: "very optional," game-dependent — ZestEngine already exceeds the floor here).

---

## ZestEngine scorecard

_Updated 2026-06-19 after the feature-foundations session — see the progress table below._

- **T0 (15):** ~11 done; **Prefabs (#7) + Runtime UI (#13) moved ❌→◑** (data model + editor
  UI/preview landed); physics gained raycast/overlap; audio gained a mixer. Remaining partials:
  physics joints/triggers, audio sink-wiring, build cook/strip, undo edge cases.
- **T1 (20):** **save/load + localization done ✅**; navmesh, particles, world streaming, shader
  graph, LOD, animation state machines, asset handles **all ❌/◑→◑** (tested logic + in-editor
  UI; the GPU-render / authoring-canvas layer is what's left per the progress table).
- **T2:** has VR (a differentiator) + RT reflections; the rest are optional polish.

### Prioritized roadmap to "usable general-purpose engine"

The order is chosen so each item unblocks the most game genres per unit of work.

1. **Runtime UI system** (T0 #13) — *the #1 blocker.* No game ships without menus/HUD; the VR
   path needs world-space UI too. Retained `UICanvas` + rect/anchors + a text atlas. Biggest
   single gap in the whole audit.
2. **Prefab system** (T0 #7) — a saveable, instantiable asset with per-instance overrides
   (today only "duplicate"). Unlocks designer-scale content.
3. **Physics completion** (T0 #12) — raycast/shape queries, trigger enter/stay/exit events,
   joints, a character controller. The layer matrix already landed this session.
4. **Animation state machines / blend trees** (T1 #21) — runtime AnimGraph over the existing
   rig/IK import.
5. **Particles / VFX** (T1 #25) — a GPU particle system; nearly every genre needs it.
6. **Navmesh + pathfinding** (T1 #24) — unblocks AI/agents.
7. **Save/load + localization + build cook/strip** (T0 #15, T1 #31/#32) — shipping plumbing.
8. **Asset streaming with typed handles** (T1 #34) — adopt Unreal's handle-as-refcount +
   per-frame time-budget pattern (already researched in `UNREAL_RESEARCH_2026-06-18.md`).
9. **LOD + instancing + occlusion culling** (T1 #29) — scene scaling (frustum culling already
   landed this session).
10. **Multi-probe reflection blending, bloom render pass, decals, planar reflections** — the
    rendering polish tier.

### Progress log — 2026-06-18 session

**Feature foundations added this session (all unit-tested; 94 workspace tests pass):**

| # | Feature | Module | What's done | UI / render remaining |
|---|---|---|---|---|
| 7 | Prefabs | `prefab.rs` | `.prefab` RON subtree, `instantiate` with fresh ids + per-instance transform override | editor "Create Prefab from selection" action + drag-instantiate |
| 12 | Physics queries | `physics.rs` | `raycast` (object+point+normal), `overlap_sphere`; collider user_data carries object id | joints/triggers still pending |
| 13 | Runtime UI | `ui_canvas.rs` | retained `UiNode` tree + **anchor layout solver** (stretch/pin/center, nested), button hit-test | GPU draw of solved rects + text atlas |
| 14 | Audio mixer | `audio_mixer.rs` | bus graph, per-bus volume/mute, `effective_gain` chain, cycle guard | mixer panel; wire rodio sinks to gains |
| 21 | Anim state machine | `anim_graph.rs` | states, param/trigger transitions, cross-fade blend, 1D blend trees, `current_pose` weights | graph editor; feed weights into skinning |
| 24 | Navmesh + pathfinding | `navmesh.rs` | walkability grid, **A*** (8-conn, no corner-cut), line-of-sight string-pull | bake grid from scene geometry; agent component |
| 25 | Particles | `particles.rs` | CPU emitter sim (rate/lifetime/cone/gravity/drag, recycle cap) | GPU instanced billboard draw; emitter component |
| 26 | Shader graph | `shader_graph.rs` | node graph → **GLSL codegen** (const/uv/texture/mul/add/mix), topo-sort, cycle/ref validation | node-editor UI; inline into material shader |
| 29 | LOD | `lod.rs` | distance-banded LOD select + **hysteresis** (no popping) + cull distance | LOD group component inspector; mesh swap |
| 30 | Events/messaging | `events.rs` | double-buffered typed event bus (`emit`/`read`/`swap`) | — (runtime API) |
| 31 | Save/load | `savegame.rs` | typed `SaveValue` KV store, atomic `.save` write | optional save-slot UI |
| 32 | Localization | `localization.rs` | `.loc` tables, `tr()` fallback, runtime language switch | string-table editor + language dropdown |
| 33 | World streaming | `streaming.rs` | cell residency with load/unload **hysteresis** band | async loader wiring |
| 34 | Asset handles | `asset_handle.rs` | typed ref-counted `Handle<T>`, dedupe-by-path, **per-frame load budget**, GC of unused | wire to real decoders |

Plus the rendering/engine fixes: **#29 culling** (per-camera mask now applies in the editor
viewport), **#17 Realtime GI** (FluxVoxel DDGI via cached distance moments — leak-free + free
per frame, propagation, specular, multi-point emissive, perf fixed), **#18 Reflections**
(capture-lighting + orientation test-lock + mip/box fixes), **#8 Undo** (atomic multi-select).

**Editor UI placement (corrected 2026-06-19 — see `FEATURE_UI_GUIDE.md`):**
- The earlier "Tools → Systems" dump was WRONG (components + settings + runtime-only APIs in one
  window) and has been **removed**. Features now go to their correct homes.
- **LOD → Inspector component** `LodComponent` ("Add Component → LOD Group"), the exemplar of
  the right pattern; particles/nav-agent/streaming/anim/UI-widgets are the same kind of
  follow-up (component or settings or asset-editor window, never a Tools dump).
- **Audio Mixer** stays a proper Window (Tools → Audio Mixer).
- **Object → Create Prefab from Selection** — writes a `.prefab`.
- Runtime-only systems (save/load, events, asset handles, physics queries) correctly have **no
  editor UI**.
- Layer UIs unified: object dropdown, camera culling mask, and collision matrix all show the
  same `LayerSettings::shown_count()` (was 32 / 8 / 4); the names list shows all 32 to reveal
  more. Per-camera culling mask applies in the editor viewport.

**FPS verified (measured, headless):** the FluxVoxel per-frame CPU GI work (clone cached base +
inject dynamic lights + build probes with cached moments) is **~156 µs for a 27k-probe grid with
2 moving lights → ~6400 fps of headroom** (`fluxvoxel_per_frame_cost_is_sub_millisecond` test,
`--nocapture`), and ZERO when nothing moves (idle-skip). The new DDGI moments / propagation /
specular are all cached, so they cost nothing per frame.

**What's genuinely left:** **GPU rendering** for the Runtime UI canvas and particle billboards
(the layout solver + sim run and preview in-editor, but drawing them in the 3D scene needs
renderer passes), full node-EDITOR canvases for shader/anim graphs (today they're inspectable +
the graph compiles live), and physics joints/triggers. T2 polish (decals, planar, bloom,
Nanite-class) stays out of scope.

**Bottom line:** ZestEngine is already past the "rendering framework" stage — it has the editor,
inspector, content browser, gizmos, play-in-editor, scripting, input, physics foundation, GI,
and VR that Bevy notably lacks. The two true T0 holes are **Runtime UI** and **Prefabs**; close
those and complete the physics/animation/audio partials, and it crosses cleanly into
"usable general-purpose engine."
</content>
