# Components & the in-game API

How gameplay behaviour is written in citrus: the component model, the in-game
API a component talks to (`ComponentCtx`), object references, the editor-only
inspector/gizmo traits, and how to ship a hot-reloadable Rust plugin.

Audience: anyone (human or agent) writing a component. For the engine internals
see the crate sources; for the feature map see [Features.md](../Features.md).

## The runtime / editor split

A component has two halves that live in two crates:

| Concern | Crate | Trait | Ships in a built game? |
|---|---|---|---|
| Behaviour (start/update) | `citrus-core` | `TypedComponent` | yes |
| Inspector UI | `citrus-editor` | `Inspect` | no |
| Viewport gizmo | `citrus-editor` | `Gizmo` | no |

`citrus-core` is egui-free on purpose: a shipped game links the engine + core
and never pulls in the editor or egui. The editor halves are gated behind an
`editor` cargo feature (see [Writing a plugin](#writing-a-plugin)), so they
compile only when the editor builds the plugin.

Rule of thumb: **behaviour goes in core, anything that draws goes in the
editor.** A component never holds an egui type in a field, and nothing in core
imports the editor.

## Anatomy of a component

Implement `TypedComponent`. The blanket impl turns it into the object-safe
`Component` the engine stores (serialization, downcasting, lifecycle dispatch
are all derived for you). It must be `Serialize + Deserialize + Default`.

```rust
use citrus_core::{ComponentCtx, TypedComponent};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Spin {
    pub axis: [f32; 3],
    pub degrees_per_second: f32,
}

impl Default for Spin {
    fn default() -> Self {
        Self { axis: [0.0, 1.0, 0.0], degrees_per_second: 45.0 }
    }
}

impl TypedComponent for Spin {
    const NAME: &'static str = "Spin";

    fn update(&mut self, ctx: &mut ComponentCtx) {
        let axis = glam::Vec3::from(self.axis).normalize_or(glam::Vec3::Y);
        let step = glam::Quat::from_axis_angle(axis, self.degrees_per_second.to_radians() * ctx.dt);
        *ctx.rotation = (step * *ctx.rotation).normalize();
    }
}
```

`NAME` is the stable identity used in the inspector, the `.scene` file, and the
registry — renaming it orphans saved data, so treat it as permanent.

### Lifecycle

All hooks default to no-ops; override what you need. They run **only in Play
mode** (the editor does not tick components while authoring).

| Hook | When |
|---|---|
| `start(&mut self, ctx)` | once, when Play mode starts |
| `update(&mut self, ctx)` | every frame |
| `late_update(&mut self, ctx)` | every frame, after *all* components ran `update` |

There is no `pre_update` / `fixed_update`. A frame runs every component's
`update`, then every component's `late_update`. Use `late_update` for anything
that must read other objects' post-`update` state (e.g. a camera following a
target that moved this frame).

Fields marked `#[serde(skip)]` are scratch state (not saved); initialize them in
`Default`. Components that accumulate (`Bob`, `Orbit`) keep an `applied` offset
this way so each frame applies only the delta.

## The in-game API: `ComponentCtx`

The single handle a component gets each frame. It exposes the owning object's
transform (read **and** write) and a read-only snapshot of the rest of the
scene for resolving references.

### Self transform (local)

```rust
*ctx.translation += glam::Vec3::Y * ctx.dt;   // &mut Vec3, local space
*ctx.rotation = ...;                           // &mut Quat
*ctx.scale = ...;                              // &mut Vec3
ctx.dt      // seconds since last frame
ctx.time    // seconds since engine start
```

`translation`/`rotation`/`scale` are the object's **local** TRS (relative to its
parent) — the exact fields the engine reads back. Writing them directly is fine
for self-relative motion (spin, bob).

### World-space writes

When the value you want to assign is a **world** coordinate (e.g. another
object's position), do not write `*ctx.translation` directly — that's a local
field, so for a parented object the engine would re-apply the parent transform
and the object lands in the wrong place. Use:

```rust
ctx.set_world_position(world_point);   // converts through the parent chain
```

For a root object this is identical to writing the local translation; for a
nested object it pushes `world_point` back through the parent's inverse world
matrix so the object ends up exactly there. (World-space rotation/scale writes
are not exposed yet.)

**Smoothed / lerped moves (planned).** Today `set_world_position` snaps to the
target. A planned addition lets a transform change *ease* instead, so motion
looks nicer without per-component hand-rolled smoothing:

```rust
// constant speed toward a target, framerate-independent
ctx.move_towards(target, max_delta_per_second * ctx.dt);
// exponential smoothing: 1 - exp(-smoothing * dt), stable at any framerate
ctx.lerp_position(target, smoothing);
ctx.slerp_rotation(target_rot, smoothing);
```

A higher-level tween (duration + easing curve: linear / ease-in-out / spring)
will run a transform change over time. All operate in world space and convert
through `parent_world` like `set_world_position`. Tracked in
[Features.md](../Features.md) §2D.

### Reading other objects

Objects are referenced by **stable id**, not name or index (see
[Object references](#object-references)). The read side returns the object's
**world** transform — every parent in its chain is already folded in, so you get
the object's true world position regardless of how deeply it's nested.

```rust
// Preferred: resolve an ObjectRef field.
let t: Option<Transform> = ctx.transform_of(self.target);   // world Transform
let p: Option<Vec3>      = ctx.position_of(self.target);    // world position

// Lower level, by id / index.
let idx: Option<usize>   = ctx.resolve(self.target);        // ObjectRef -> index
let idx: Option<usize>   = ctx.index_of(some_object_id);    // id -> index
let t                    = ctx.object_transform(idx);       // world Transform
let m                    = ctx.object_matrix(idx);          // world Mat4
let p                    = ctx.object_position(idx);        // world position

// By name (cosmetic, may collide / be empty — prefer ids).
let idx = ctx.find_object("Player");
let t   = ctx.object_transform_named("Player");

// Self.
let me  = ctx.self_transform();   // world Transform of the owner
let id  = ctx.self_id();          // owner's ObjectId
```

`Transform` carries `translation` / `rotation` / `scale` plus
`forward()` / `right()` / `up()` / `matrix()`. The snapshot is taken once at the
start of the update pass, so reads are stable-by-one-frame for objects already
ticked this frame — fine for references.

### Deferred commands

Some actions can't run mid-iteration (they mutate the scene). Queue them; the
engine applies them after the update pass:

```rust
ctx.load_scene("scenes/level2.scene");   // level change / menu -> game
```

`load_scene` is the first slice of this surface (`ComponentCommand`). Spawn /
despawn / add-component will land here as the API grows.

## Object references

Every object has an `ObjectId` — a UUID assigned at creation (spawn / import /
scene load) and serialized in the `.scene`. Ids survive reload, reordering, and
will survive networking; names stay purely cosmetic.

A component holds a reference as an `ObjectRef` field (`= ObjectRef::NONE` when
unset):

```rust
pub struct Orbit {
    pub target: ObjectRef,
    // ...
}
```

In the editor this field renders as a **drop target**: drag an object from the
Scene tree onto the box to set it; the ✕ clears it. At runtime, resolve it each
frame with `ctx.transform_of(self.target)` (returns `None` if unset or the
target was deleted — handle that case).

Worked example — orbit a target, correct at any depth in the hierarchy:

```rust
fn update(&mut self, ctx: &mut ComponentCtx) {
    let angle = ctx.time * self.degrees_per_second.to_radians();
    let offset = glam::Vec3::new(angle.cos(), 0.0, angle.sin()) * self.radius;
    match ctx.transform_of(self.target).map(|t| t.translation) {
        // c is the target's WORLD position; set_world_position converts c+offset
        // through the orbiter's parent, so a nested orbiter still circles it.
        Some(c) => ctx.set_world_position(c + offset),
        None => { /* no target: orbit own start point */ }
    }
}
```

## Built-in components

Registered automatically (`ComponentRegistry::with_builtins`). Their data lives
in `citrus-core`; their inspectors/gizmos in `citrus-editor`.

| Name | Purpose |
|---|---|
| `Camera` | FOV / near / far; viewport draws a frustum widget |
| `Light` | Directional / Point / Spot; realtime / baked / mixed; shadows |
| `Light Probe Volume` | Box grid of irradiance probes for the bake |
| `Audio Source` | Spatial / non-spatial clip with distance rolloff |
| `Audio Listener` | Marks the "ears" for spatial audio |
| `Box Collider` / `Sphere Collider` / `Mesh Collider` | Collision authoring (layers, trigger) |
| `Spin` | Constant rotation about a local axis |
| `Bob` | Sine hover along an axis |

## Editor traits

These live in `citrus-editor` and never link into a game. Implement them behind
`#[cfg(feature = "editor")]` in a plugin.

```rust
// Inspector: draw widgets, return true if anything changed (drives dirty/undo).
impl Inspect for Orbit {
    fn inspector_ui(&mut self, ui: &mut egui::Ui, ctx: &InspectCtx) -> bool {
        let mut changed = false;
        changed |= ui.add(egui::Slider::new(&mut self.radius, 0.0..=10.0).text("Radius")).changed();
        changed |= ctx.object_ref(ui, "Target", &mut self.target);   // drag-drop picker
        changed
    }
}

// Gizmo: draw into the viewport when selected. Default draws nothing.
impl Gizmo for Orbit {
    fn draw_gizmo(&self, ctx: &GizmoCtx) {
        // ctx.painter, ctx.world_to_screen(world_pt), ctx.world, ctx.selected
    }
}
```

`InspectCtx::object_ref(ui, label, &mut field)` draws the standard `ObjectRef`
drop box. `GizmoCtx` gives you an egui `Painter`, a `world_to_screen` projector,
the owning object's world matrix, and whether it's selected.

## Writing a plugin

Project components live in `plugins/<name>` as a `cdylib`. The editor compiles
and hot-reloads them (Tools -> Build & Reload Components); a shipped game links
the same crate without the `editor` feature.

`Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[features]
default = []
# The editor builds with --features editor; a game builds without it, so
# citrus-editor and egui never link into the shipped binary.
editor = ["dep:citrus-editor", "dep:egui"]

[dependencies]
citrus-core.workspace = true
citrus-editor = { workspace = true, optional = true }
egui = { workspace = true, optional = true }
serde.workspace = true
glam.workspace = true
```

`lib.rs` exports two registration functions. The runtime one always exists; the
editor one is feature-gated:

```rust
use citrus_core::{ComponentRegistry, TypedComponent};
#[cfg(feature = "editor")]
use citrus_editor::{EditorComponents, Gizmo, Inspect, InspectCtx};

// Runtime behaviour — called by the editor and by a shipped game.
#[unsafe(no_mangle)]
pub fn citrus_register(registry: &mut ComponentRegistry) {
    registry.register::<Orbit>();
}

// Editor-only traits — compiled only with the editor feature; absent in a game.
#[cfg(feature = "editor")]
#[unsafe(no_mangle)]
pub fn citrus_register_editor(editor: &mut EditorComponents) {
    editor.register::<Orbit>();
}
```

Registering a `NAME` that already exists **replaces** the old entry — that's how
hot-reload supersedes a previous build.

### The cdylib egui boundary

A plugin statically links its own copy of egui, so egui's `TypeId`s differ from
the host's. Any egui API backed by a **context plugin keyed by TypeId** — drag
and drop (`dnd_*`), selectable labels — panics (SIGABRT, uncatchable) when called
from plugin code. Avoid those in `Inspect`/`Gizmo`:

- Don't call egui dnd; use `InspectCtx::object_ref` (it detects drops via raw
  pointer state + a shared `usize` slot in egui memory, no dnd context).
- Don't draw selectable labels; the component inspector disables them globally.

Plain widgets (sliders, buttons, labels, painters) are fine.

## See also

- [Features.md](../Features.md) — in-game API roadmap (object graph, components,
  physics queries, input, spawn/despawn) and the editor/gameplay component split.
- `crates/citrus-core/src/lib.rs` — the authoritative API.
- `plugins/components/src/lib.rs` — the `Orbit` example, end to end.
