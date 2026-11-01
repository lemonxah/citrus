# Feature UI Placement & Usage Guide

Where each engine feature's UI lives in the ZestEngine editor, how to use it, and its
status. Placement follows Unity/Godot/Unreal conventions (researched 2026-06-19):
**object concerns → Inspector components; project-wide config → Settings;
reusable data → Assets with their own editor; cross-cutting tools → dockable
Windows; pure code systems → runtime API (no editor UI).**

> The earlier "Tools → Systems" dump was wrong — it crammed components, settings,
> and runtime-only APIs into one window. It has been removed. Features now live in
> their correct homes (see Status below).

---

## Placement matrix

| Feature | Correct home | Status |
|---|---|---|
| **LOD** | **Component** (Inspector) | ✅ `LodComponent` ("Add Component → LOD Group") |
| **Particles / VFX** | **Component** (emitter) | ◑ runtime sim (`particles.rs`); component + GPU draw pending |
| **Navmesh (data)** | **Window** (bake) + **Settings** (agent/recast params) | ◑ A* runtime (`navmesh.rs`); bake window + nav settings pending |
| **Nav agent** | **Component** | ◑ pending (pairs with navmesh) |
| **AI behavior** | **Asset+Editor** (behavior tree) or scripts | ◑ scripts today; BT editor is a large follow-up |
| **Anim state machine** | **Component** (Animator) + **Window** (graph asset) | ◑ runtime graph (`anim_graph.rs`); component + editor pending |
| **Audio mixer** | **Window** (mixing console) | ✅ Tools → Audio Mixer |
| **Localization** | **Settings** (locales) + **Window** (string tables) | ◑ runtime (`localization.rs`); window pending |
| **Save/load** | **Runtime-only** (no editor UI) | ✅ `savegame.rs` API |
| **Events** | **Runtime-only** | ✅ `events.rs` API |
| **World streaming** | **Settings** + **Component** (streaming source) | ◑ runtime (`streaming.rs`); settings/component pending |
| **Asset handles** | **Runtime-only** | ✅ `asset_handle.rs` API |
| **Prefabs** | **Asset** + create-from-selection + drag-instantiate | ✅ Object → Create Prefab from Selection |
| **Runtime UI** | **Scene object tree** (Canvas + widget components) | ◑ layout solver (`ui_canvas.rs`); scene-tree + GPU draw pending |
| **Shader graph** | **Asset+Editor** (node window) → material | ◑ codegen (`shader_graph.rs`); node editor pending |
| **Physics queries** | **Runtime-only** (raycast/overlap API) | ✅ `physics.rs` |

---

## How to use each (current)

### LOD — Inspector component ✅
Select an object → **Add Component → "LOD Group"**. Add levels, each with a max
camera distance and (optionally) a model path for that level (blank = the base
mesh). Set a cull distance (0 = never). Levels auto-sort by distance.
`LodComponent::select_lod(distance)` returns the active level; the renderer reads it.
*Remaining:* the render-side mesh swap (load + swap per level) — the data + UI are done.

### Audio mixer — Tools → Audio Mixer ✅
Per-bus volume fader + mute; gain chains up to Master (`effective_gain(bus)`). Audio
sources will pick a target bus (source-component dropdown — pending).

### Prefabs — Object → Create Prefab from Selection ✅
Select an object subtree → creates a `.prefab` next to the scene. Load + instantiate
via `Prefab::load(path).instantiate(pos)` (drag-to-instantiate from the asset browser
is the next step).

### Runtime-only systems (no editor UI, by design) ✅
- **Save/load:** `SaveGame::new().set(k, v); save(path)` / `SaveGame::load(path)`.
- **Events:** `bus.emit("name", payload)` / `bus.read("name")` / `bus.swap()` per frame.
- **Asset handles:** `assets.request(path)` → `Handle<T>`; `assets.tick(budget, loader)`.
- **Physics queries:** `physics.raycast(origin, dir, max)` / `overlap_sphere(c, r)`.

### Runtime systems with UI pending
Particles, navmesh/agents, anim graph, localization, streaming, runtime UI, shader
graph all have tested runtime/logic modules but their authoring UI belongs in the
homes above (components / settings / asset-editor windows), NOT a Tools dump. Each is
a focused follow-up; the pattern is established by `LodComponent`:
1. define the data struct in `zestengine-core` (`impl TypedComponent`),
2. register in both `ComponentRegistry::with_builtins` (core) and the editor's
   `EditorComponents` + an `impl Inspect`,
3. consume it in the engine.

---

## Navigation & AI — the three-layer model (reference)

Keep these **separate** (Unreal's canonical chain):
`NavMeshBoundsVolume → RecastNavMesh → AIController → BehaviorTree → MoveTo`.

- **Layer A — Navmesh data:** baked from collision geometry. ZestEngine target: a
  **NavMeshSurface component** scoping a volume + a **Navigation Settings** panel
  (agent radius/height/slope/step, area types + costs) + a **Bake** button/window
  with an overlay toggle. The `navmesh.rs` A* + walkability grid is the runtime core.
- **Layer B — Agents:** a **NavAgent component** (radius/speed/avoidance) on movable
  objects; queries the navmesh for a path and steers. Does NOT decide *where* to go.
- **Layer C — AI behavior:** the "brain." Either user scripts/components, or
  (Unreal-style) a **Behavior Tree asset** edited in a graph window + a Blackboard,
  driven by an AIController component. This is independent of navigation.

Do not conflate bake (data/tool), agent (component), and behavior (asset/script).
