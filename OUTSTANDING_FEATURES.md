# Citrus — Outstanding Features Backlog

The **dedicated list of features still to do**, derived by taking the authoritative
engine-feature audit ([ENGINE_FEATURE_CHECKLIST.md](ENGINE_FEATURE_CHECKLIST.md) — Unreal /
Unity / Godot / Bevy) and diffing it against Citrus's **actual code state** (verified against
the source on 2026-06-18, not from memory). Only the **gaps** are listed here; things Citrus
already does well are in [Features.md](Features.md).

**Priority:** P0 = table-stakes (engine isn't "usable general-purpose" without it) · P1 =
expected of a serious engine · P2 = advanced / polish.
**Effort:** S ≈ days · M ≈ 1–2 weeks · L ≈ several weeks · XL ≈ months.
**State:** ❌ missing · ◑ partial (what exists / what's left spelled out).

> Audit corrections vs. earlier notes: **post-processing bloom + chromatic aberration ARE
> now rendered** (`post.frag`, UE-order exposure→bloom→tonemap w/ Karis average) — so post is
> effectively done, not a gap. Animation **clips + GPU skinning exist**; only the Animator
> graph is missing. The build path **does** produce a stripped standalone binary; only
> asset-cook + editor-only stripping remain.

---

## P0 — Table-stakes gaps (close these to be a "usable engine")

### 1. Runtime UI system ❌ — *the #1 blocker* · Effort: XL
- **State:** none. egui powers the *editor*, but there is no in-game UI.
- **Missing:** a retained `UICanvas` (screen + world space for VR), `UIRect`/anchors, a 2D
  batched UI renderer, a **text/font atlas** (new subsystem — the editor's egui fonts don't
  carry over), core widgets (Text, Image/Panel, Button, Checkbox, Slider), a hit-test +
  pointer/focus event system (mouse now, **VR controller ray** later), and editor authoring.
- **Why:** no game ships without menus/HUD; the VR path specifically needs world-space UI.
  This is the single largest hole in the entire audit.
- **Touch points:** new `citrus-ui` crate or `citrus-render` 2D pass; `citrus-engine` component
  + event wiring; editor authoring mode. Spec already drafted in Features.md §2F.

### 2. Prefab system ❌ · Effort: L
- **State:** only **duplicate** (Ctrl+D clones a subtree with fresh ids). No prefab *asset*.
- **Missing:** a saveable `.prefab` asset, instancing into scenes, and **per-instance
  overrides** (the Unity/Godot/Unreal model — edit the prefab, instances update; override
  per-instance). Nested prefabs later.
- **Why:** designer-scale content authoring is impossible without it; every other engine treats
  it as foundational.
- **Touch points:** `citrus-assets` (new asset + override diff), `scene.rs` (instancing +
  override resolution), inspector (override indicators), file browser.

### 3. Physics completion ◑ · Effort: M
- **State:** rapier world with rigid bodies (Dynamic/Kinematic/Fixed), box/sphere/mesh-AABB
  colliders, and the **layer-collision matrix** (landed 2026-06-18). Joint *sets* are allocated
  but empty; `is_trigger` is a collider flag with no event delivery.
- **Missing:** **physics queries** (raycast / shape-cast / overlap) exposed to gameplay;
  **trigger enter/stay/exit events** to components; **joint components** (authoring + serialize:
  hinge/fixed/spring/etc.); a **physics-driven character controller** (today `Pawn` moves by
  transform writes, not forces); CCD tuning; parented-body world↔local conversion.
- **Why:** queries + triggers are needed by almost all gameplay (interaction, detection, AI).
- **Touch points:** `physics.rs`, `ComponentCtx` (query API, 2D §2D), new joint components in
  `citrus-core`.

### 4. Undo/redo coverage ◑ · Effort: S–M
- **State:** covers transform edits (move/rotate/scale), material edits + slot assignment, and
  `.material` file edits. Delete is intentionally non-undoable.
- **Missing:** component add/remove, object creation/duplication, reparenting, layer changes,
  and most structural edits aren't on the undo stack.
- **Why:** an editor where half the actions can't be undone erodes trust fast.
- **Touch points:** `undo.rs` (extend `UndoEntry`), the action sites in `editor_app.rs`.

### 5. Audio mixer / buses ◑ · Effort: M
- **State:** `AudioSource` (clip, loop, volume, pitch, play-on-start) + spatial attenuation +
  `AudioListener`. No mixing graph.
- **Missing:** named **mixer buses/groups**, per-bus volume + effects (**reverb**, EQ, filter,
  compressor), reverb zones, and a runtime API to drive them. Optional: interactive music.
- **Why:** any polished game needs master/music/sfx bus control and reverb for spaces.
- **Touch points:** `audio.rs` (bus graph over rodio), inspector, `ComponentCtx` audio API.

### 6. Build: asset cook + editor-only stripping ◑ · Effort: M
- **State:** `bundle.rs` emits a standalone cargo workspace and a **stripped LTO binary**
  (cdylib+rlib static link). Scenes/sidecars bundle by living in `scenes/`.
- **Missing:** an **asset-cook step** (pack/optimize assets, not raw copies) and an
  **`EDITOR_ONLY` component marker** so editor-only behaviour (gizmo helpers, probe volumes)
  is excluded from the shipped runtime (Features.md §2E). No console targets.
- **Why:** shipping hygiene — smaller builds, no editor code in the game.
- **Touch points:** `TypedComponent` (add `EDITOR_ONLY`), `runtime.rs` update loop filter,
  `bundle.rs` cook pass.

---

## P1 — Expected of a serious engine

### 7. Animation graph (state machines / blend trees) ◑ · Effort: L
- **State:** `AnimationClip`s import + **GPU skinning** play (currently the default/first clip;
  rig + IK import + VR full-body IK exist). No control layer.
- **Missing:** an **Animator component** — clip selection, **state machine**, **blend trees /
  blend spaces**, transitions, layers/masks, **animation events**, root motion, retargeting at
  runtime.
- **Touch points:** `scene.rs` skinning path, new `Animator` component, `citrus-assets` clip
  metadata.

### 8. Particles / VFX ❌ · Effort: L
- **Missing:** a GPU particle system (emitters, lifetime, velocity/forces, color/size curves,
  collision, sub-emitters, trails), authored in the editor. Nearly every genre needs it.
- **Touch points:** new compute-driven particle pass in `citrus-render`, component + editor UI.

### 9. Navigation / navmesh + pathfinding ❌ · Effort: L
- **Missing:** navmesh bake from static geometry, `NavAgent` pathing, avoidance; unblocks
  AI/agents and click-to-move (Features.md §2B todo).
- **Touch points:** new navmesh baker (reuse bake-geometry gather), agent component, queries.

### 10. Save / load game state API ❌ · Effort: M
- **State:** scene serialization exists; no *gameplay* save.
- **Missing:** a `ctx.save(slot)` / `load(slot)` API capturing component state + scene + custom
  game data to a save file, restorable at runtime.
- **Touch points:** `ComponentCtx`, a save serializer reusing component `save()`/`load()`.

### 11. Multi-probe reflection blending ◑ · Effort: L (GPU)
- **State:** probe **priority selection** lands the best single probe (importance → smaller box,
  2026-06-18); box-projection math is unit-tested.
- **Missing:** per-fragment blending of **multiple overlapping probes** with smooth
  `DistanceAlpha` falloff + skylight fallback (the documented UE algorithm). Needs a **cubemap
  array** (one captured cube per probe) threaded through the standard shader, SSR resolve, and
  skybox path — a renderer refactor that needs GPU iteration.
- **Touch points:** `lib.rs` env-cube → cube array, `standard.frag`, `ssr_resolve.frag`.

### 12. Asset streaming + typed handles ◑ · Effort: L
- **State:** worker-thread decode + transfer-queue upload + BC import cache + chunked bake.
- **Missing:** a unified **`AssetHandle<T>`** registry (request-and-await, ref-counted
  keep-alive, completion on the main-thread tick), a **per-frame time-slice budget** (UE's
  `FTimeLimitScope` pattern), and **texture mip streaming** by projected screen size. Patterns
  fully researched in [UNREAL_RESEARCH_2026-06-18.md](UNREAL_RESEARCH_2026-06-18.md) §C.
- **Touch points:** new asset registry in `citrus-assets`, `tasks.rs`, texture upload path.

### 13. LOD + GPU instancing + occlusion culling ◑ · Effort: L
- **State:** **CPU frustum culling** (main pass + per-VR-eye) landed 2026-06-18.
- **Missing:** mesh **LODs** (distance-switched), **GPU instancing** for many identical props
  (foliage/crowds — pairs with frustum culling), and **occlusion culling**.
- **Touch points:** `scene.rs` draw assembly, `lib.rs` instanced draw path, mesh import LOD.

### 14. Visual / node shader graph ◑ · Effort: L
- **State:** custom GLSL shaders (runtime glslc, pragma-declared props, hot reload).
- **Missing:** a node-based **material/shader graph** editor (Godot VisualShader / UE Material
  Editor). *Lower urgency — text shaders cover authoring; this is UX.*
- **Touch points:** new editor graph UI → GLSL codegen.

### 15. Event / messaging bus ◑ · Effort: S–M
- **State:** networked messaging (`broadcast`/`send_to`) exists; signals fire over the wire.
- **Missing:** a **local component-to-component event bus** (signals / UnityEvent-style) for
  decoupled gameplay (Features.md §2D todo).
- **Touch points:** `ComponentCtx`, a typed event queue applied post-update.

### 16. Localization ❌ · Effort: M
- **Missing:** string tables + a `tr("key")` lookup + locale switching; pairs with Runtime UI.
- **Touch points:** `citrus-assets` (string tables), UI text widgets.

### 17. Level / world streaming ❌ · Effort: XL
- **Missing:** load/unload scene chunks by proximity (open-world). Builds on the asset-streaming
  handle work (#12). *Defer until a game needs it.*

---

## P2 — Advanced / polish (not disqualifying)

| Feature | State | Effort | Note |
|---|---|---|---|
| **Planar reflections** (mirrors/water) | ❌ | L | The one reflection model UE/Unity/Godot have that we lack. |
| **Decals** | ❌ | M | Deferred/forward decal projector. |
| **Volumetric clouds** | ◑ (fog only) | L | Have volumetric fog; clouds missing. |
| **Terrain tools** | ❌ | XL | Godot ships none either — genuinely optional. |
| **Cinematics / sequencer** | ❌ | XL | Timeline/Sequencer-style. |
| **AI behavior trees + perception** | ❌ | L | Pairs with navmesh (#9). |
| **Gameplay ability system** | ❌ | L | UE GAS-style; only some genres need it. |
| **Vehicle / cloth / destruction physics** | ❌ | XL | rapier extensions. |
| **Virtual shadow maps** | ❌ | L | UE-style; current CSM is fine for most. |
| **Virtualized geometry** (Nanite/meshlets) | ❌ | XL | Bevy's is still experimental too. |
| **Console platform support** | ❌ | XL | NDA/porting effort. |
| **Visual scripting** (Blueprint) | ❌ | XL | **Explicitly NOT table-stakes** — Godot removed it; Rust plugins suffice. |

---

## Suggested sequence (max genres-unblocked per unit of effort)

1. **Runtime UI (#1)** — unblocks literally every game; do first despite being XL.
2. **Prefabs (#2)** — designer-scale content.
3. **Physics completion (#3)** — queries + triggers unblock interaction/AI.
4. **Undo coverage (#4)** + **Audio mixer (#5)** + **event bus (#15)** — small/medium polish that
   makes the editor and gameplay feel finished.
5. **Animation graph (#7)** — runtime AnimGraph over existing clips/skinning.
6. **Particles (#8)** + **Navmesh (#9)** — VFX + AI.
7. **Save/load (#10)** + **Localization (#16)** + **build cook/strip (#6)** — shipping plumbing.
8. **Asset streaming handles (#12)** → **LOD/instancing (#13)** → **world streaming (#17)** — scale.
9. **Rendering polish:** multi-probe blending (#11), planar reflections, decals, clouds.

**Two true T0 holes remain — Runtime UI and Prefabs.** Everything else in P0 is a partial to
finish. Close those two and complete the P0 partials, and Citrus crosses cleanly from "engine
with notable gaps" into "usable general-purpose engine."
</content>
