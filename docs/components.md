# Components & the in-game API

How gameplay behaviour is written in zestengine: the component model, the in-game
API a component talks to (`ComponentCtx`), object references, the editor-only
inspector/gizmo traits, and how to ship a hot-reloadable Rust plugin.

Audience: anyone (human or agent) writing a component. For the engine internals
see the crate sources; for the feature map see [Features.md](../Features.md).

## The runtime / editor split

A component has two halves that live in two crates:

| Concern | Crate | Trait | Ships in a built game? |
|---|---|---|---|
| Behaviour (start/update) | `zestengine-core` | `TypedComponent` | yes |
| Inspector UI | `zestengine-editor` | `Inspect` | no |
| Viewport gizmo | `zestengine-editor` | `Gizmo` | no |

`zestengine-core` is egui-free on purpose: a shipped game links the engine + core
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
use zestengine_core::{ComponentCtx, TypedComponent};
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
registry. Renaming it orphans saved data, so treat it as permanent.

### Lifecycle

All hooks default to no-ops; override what you need. They run **only in Play
mode** (the editor does not tick components while authoring).

| Hook | When |
|---|---|
| `start(&mut self, ctx)` | once, the first frame the object is active (Play start, or later when spawned / enabled) |
| `fixed_update(&mut self, ctx)` | once per fixed physics step (60 Hz), before the step; `ctx.dt` is the step |
| `on_collision_enter/exit(&mut self, ctx, other)` | a solid collider on this object started / stopped touching `other` |
| `on_trigger_enter/exit(&mut self, ctx, other)` | a trigger collider and `other` started / stopped overlapping (fires on both objects) |
| `update(&mut self, ctx)` | every frame |
| `late_update(&mut self, ctx)` | every frame, after *all* components ran `update` |

Frame order: `start` for newly active objects, then zero or more fixed steps
(`fixed_update` + physics), then collision / trigger callbacks, then every
`update`, then every `late_update`. Use `late_update` for anything that must
read other objects' post-`update` state (e.g. a camera following a target that
moved this frame). Components on disabled objects (or under a disabled parent)
don't run at all.

The editor's Play mode and a shipped game run the exact same loop
(`zestengine-engine/src/play.rs`).

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
ctx.dt           // seconds since last frame, scaled by the time scale
ctx.unscaled_dt  // ignores the time scale (pause menus)
ctx.time         // scaled seconds since Play started
```

`translation`/`rotation`/`scale` are the object's **local** TRS (relative to its
parent), the exact fields the engine reads back. Writing them directly is fine
for self-relative motion (spin, bob).

### World-space writes

When the value you want to assign is a **world** coordinate (e.g. another
object's position), do not write `*ctx.translation` directly. That's a local
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
target. A planned addition lets a transform change ease instead, so motion
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
ticked this frame, which is fine for references.

### Deferred commands

Anything that mutates the world is queued as a `ComponentCommand` and applied
right after the pass that issued it (so the scene never changes mid-iteration).
Queries (`get`, `raycast`, `velocity`, ...) answer immediately.

```rust
ctx.load_scene("scenes/level2.scene");   // level change / menu -> game
```

### Other components

```rust
// Read (immediate). Works for siblings on the same object too.
if let Some(h) = ctx.get::<Health>(self.target) { /* h.hp */ }
let mine = ctx.get_self::<Inventory>();

// Write (deferred, applied after this pass). A no-op if the object or
// component is gone by then.
ctx.modify::<Health>(hit.object, |h| h.hp -= 10.0);
```

`get` and `modify` take anything that converts into an `ObjectRef`
(an `ObjectRef` field or an `ObjectId`, e.g. `hit.object`).

### Events

A broadcast bus for decoupled gameplay. Any `'static` type is an event; events
emitted during frame N are readable by every component throughout frame N+1.

```rust
struct EnemyKilled { score: u32 }

ctx.emit(EnemyKilled { score: 100 });                 // the enemy
for e in ctx.events::<EnemyKilled>() { self.score += e.score; }  // the HUD
```

### Spawning, destroying, enabling

```rust
// From a .prefab asset (Object -> Create Prefab from Selection).
let id = ctx.spawn_prefab("prefabs/enemy.prefab", pos, rot);
// Clone a scene object (often a disabled "template"); copies its enabled state.
let id = ctx.instantiate(self.bullet_template, pos, rot).unwrap();
ctx.set_active(id, true);

ctx.destroy(self.target);       // object + children
ctx.destroy_self();
ctx.set_active(self.door, false);
ctx.is_active(self.door);
```

Spawn calls return the new id immediately; the object exists from the next pass
and runs `start` on its first active frame.

### Physics

Needs a collider (`Box` / `Sphere` / `Capsule` / `Mesh Collider`). Add a
`Rigid Body` to make it move (Dynamic = simulated, Kinematic = moved by gameplay
but pushes dynamics, Fixed / none = static). Adding a Rigid Body in the inspector also adds a collider if the object
has none (Mesh Collider for mesh objects, Box Collider otherwise). The simulation runs at a fixed
60 Hz and dynamic bodies are interpolated for smooth rendering.

Surface behaviour comes from physics materials (`.physmat`, file browser > New
Physics Material): friction, bounciness, how each combines with the other
surface, density (sets the mass when the Rigid Body's Mass is 0), buoyancy and
water drag multipliers, and a contact offset. Drop one on a Rigid Body's or a
collider's Material slot; a collider's material beats its body's, which beats
the project default (Project Settings > Physics, alongside gravity, contact
stiffness and damping, and solver iterations).

```rust
// Queries (immediate). raycast ignores triggers and the caller's own colliders.
if let Some(hit) = ctx.raycast(origin, dir, 100.0) {
    // hit.object, hit.point, hit.normal, hit.distance
}
let filter = RayFilter { layer_mask: 1 << 3, include_triggers: true, exclude: None };
ctx.raycast_filtered(origin, dir, 100.0, &filter);
let near: Vec<ObjectId> = ctx.overlap_sphere(center, 5.0);
let v = ctx.velocity(self.ball);

// Forces (deferred). Continuous forces belong in fixed_update.
ctx.add_force(self.ball, Vec3::Y * 20.0);
ctx.add_impulse(hit.object, dir * 5.0);
ctx.add_torque(self.ball, Vec3::Y);
ctx.set_velocity(self.ball, Vec3::ZERO);
```

Writing the transform of a dynamic body (`*ctx.translation = ...`,
`set_world_position`) teleports it. Kinematic bodies follow their transform
every step.

**Character controller.** Add `Character Controller` (capsule) to an object and
move it with:

```rust
ctx.move_character(velocity * ctx.dt);   // slides on walls, climbs steps/slopes
if ctx.is_grounded() { /* can jump */ }
```

Gravity is yours to apply. The built-in `Pawn` uses the controller
automatically when one is on the same object. Moves chain: several in one
frame (or between physics steps) each start where the last ended. The
capsule keeps a 2 cm skin off what it touches, and one that starts inside
something (spawned with its feet exactly on the floor, standing up from a
crouch, a networked rewind) is pushed out on its next move instead of
sinking.

### Audio

```rust
ctx.play_sound("audio/click.ogg", 1.0);                 // 2D one-shot
ctx.play_sound_at("audio/boom.ogg", hit.point, 1.0);    // positional one-shot
ctx.play_audio(self.radio);                             // (re)start an Audio Source
ctx.stop_audio(self.radio);
```

Clip paths are project-relative (.wav / .ogg / .flac / .mp3).

### Spatial audio (Steam Audio)

Spatial Audio Sources render through Valve's Steam Audio: sounds come from their direction (HRTF
binaural, or plain stereo panning with HRTF off), fade with the source's Min/Max Distance and Rolloff,
lose high frequencies with distance (Air absorption) and can be directional (Directivity: 0 omni,
0.5 cardioid, 1 dipole along the object's forward -Z; Sharpness narrows the beam).

With Occlusion on, static geometry (objects marked Static with a mesh) between the source and the
listener muffles the sound, letting some through the material. `ctx.play_sound_at` one-shots get the
same treatment. Put an Audio Listener on the camera (or the player's head in VR); its orientation
matters now, not just its position.

Built games ship `libphonon` next to the executable (Build Game copies it). Without the `steam-audio`
engine feature, spatial sources only change volume with distance.

Audio Source also has Reflections (simulate its own reverb; off = the listener reverb), Pathing (sound
around corners from a baked Audio Probe Volume), Via ambisonics, and Ambisonic clip (1st-3rd order
recordings rotate with the listener).

| Component | Purpose |
|---|---|
| `Acoustic Material` | How a mesh sounds: preset (Generic, Brick, Concrete, Ceramic, Gravel, Carpet, Glass, Plaster, Wood, Metal, Rock) or custom absorption / scattering / transmission; Moving occluder makes a non-static mesh occlude |
| `Audio Probe Volume` | Box of floor probes (spacing, height) for baked reverb and pathing; bake with Tools > Bake Audio |

Project Settings > Spatial Audio sets the reflection type (Convolution, Parametric, Hybrid), quality,
ray tracer, a custom .sofa HRTF and the output layout (Headphones, Stereo, Quad, 5.1, 7.1). Changes
apply the next time Play starts.

### Screen and pointer

```rust
let size = ctx.screen_size();                 // game view, pixels
if let Some(px) = ctx.pointer_position() { }  // from the top-left
// Click-to-select / click-to-move: ray under the pointer + raycast.
if ctx.input.pressed("Fire") && !ctx.pointer_over_ui()
    && let Some((origin, dir)) = ctx.pointer_ray()
    && let Some(hit) = ctx.raycast(origin, dir, 500.0)
{
    self.destination = hit.point;
}
let label_pos = ctx.world_to_screen(enemy_pos);   // e.g. a health bar over a head
```

In the editor these are relative to the viewport, using the game camera.

### Animation

Add an `Animator` to an object whose model has skinned meshes and clips. The
Inspector lists the model's clips. Each animated object has its own pose, so
spawned copies animate independently.

```rust
ctx.play_animation(self.body, "Run", 0.2);   // cross-fade over 0.2 s
ctx.stop_animation(self.body);

// Reading state (e.g. wait for a one-shot to end):
if let Some(a) = ctx.get::<Animator>(self.body) {
    if a.finished() { /* non-looping clip reached its end */ }
    if a.is_playing("Attack") { }
}
```

`play` is a no-op when that clip is already playing. Set `looping` / `speed`
on the component (or via `ctx.modify::<Animator>`).

**Blend trees** (Inspector -> Blend trees) mix clips by float parameters and
are played like clips. 1D trees blend the two neighbouring clips; 2D trees
(set "Parameter Y") weight clips by distance. Clips are phase-synced.

```rust
ctx.play_animation(self.body, "Locomotion", 0.2);
ctx.set_animator_float(self.body, "Speed", ctx.velocity(self.body).length());
```

**Events**: markers on a clip (clip, time, name). Passing one calls
`on_animation_event(&mut self, ctx, name)` on the object's components and
emits `AnimationEvent { object, name }` on the event bus (footsteps, hit
frames).

**Root motion**: with "Root motion" on, the root bone's horizontal movement
moves the object (through its Character Controller when present) instead of
sliding the mesh.

**GPU skinning** (default on): Animator meshes are skinned by a compute pass,
so many animated characters cost little CPU.

**Animator Controllers** (`.animctrl`, Unity-style state machines). Set the
Animator's Controller field (drag the file on, or Create -> New Animator
Controller in Files) and press Open to edit it in the **Animator Controller**
tab. A controller has typed parameters (Float, Int, Bool, Trigger) and layers;
each layer is a graph of states (a clip, a `.anim` path or a blend tree, with
speed, an optional speed parameter, loop, and on-enter / on-exit event names)
joined by transitions (conditions, exit time, cross-fade duration, offset,
interruptible). Any State transitions are checked first. Extra layers blend
over the base layer (Override with a weight, or Additive over the rest pose)
and can be masked to bones or child paths (each with its children).

**Sub-state machines** group states: right-click the canvas -> Create Sub-State
Machine, then double-click it (or Open in the inspector) to edit its own states,
Entry default, Any State and transitions; the path above the graph (or Esc)
goes back up. At the parent level it is connected like any state: a transition
into it enters its default state (recursively), and a transition from it leaves
from whichever inner state is playing (exit time is that state's). A
sub-machine's Any State only applies to states inside it. Parameters are
shared by every level. Transitions inside a sub-machine name only its own
states, so renames and deletes stay consistent per level. At runtime inner
states are named by path, `Outer/Inner` (`Locomotion/Walk`, or
`Combat/Attack/Heavy` two levels down): use that name with `play_animator_state`,
`is_in_state`, and in `animator_state` / `AnimatorStateChanged`. Checked in
order for a playing inner state: the top-level Any State, then outer levels'
transitions before inner ones, then the sub-machine's Any State before the
state's own. State names cannot contain `/`.

```rust
ctx.set_animator_float(self.body, "Speed", speed);
ctx.set_animator_bool(self.body, "Grounded", ctx.is_grounded());
ctx.set_animator_trigger(self.body, "Jump");        // consumed by the transition it fires
ctx.play_animator_state(self.body, "Die", 0.1);     // force a state
let state = ctx.animator_state(self.body, 0);        // current state on layer 0
// On the component: set_int / get_bool / reset_trigger / current_state /
// is_in_state / is_in_transition / state_normalized_time / play_state_on_layer.
for e in ctx.events::<AnimatorStateChanged>() { /* e.object, e.layer, e.from, e.to */ }
```

State on-enter / on-exit names arrive like clip events (`on_animation_event`,
`AnimationEvent`), and every state change emits `AnimatorStateChanged`, so other
components and other state machines can react. **Parameter drivers** (Inspector
-> Parameter drivers) feed a parameter every frame from an object's movement
(speed, vertical, forward, right), its ground check, or another Animator's
parameter, so controllers can follow each other without code.

**Animation clips** (`.anim`) are made in the **Animation** tab: tracks animate
the target object (the selection's nearest Animator) and its children by
relative path, or bones of a skinned model, with Position / Rotation / Scale /
Light Intensity keys (Constant, Linear or Auto cubic). Scrub the ruler, press
Play to preview in the editor, or turn on Record and move / rotate / scale the
hierarchy with the gizmo or Inspector to key it at the playhead. An Animator on
a plain object hierarchy plays `.anim` clips (as its Clip, or from a
controller) by moving the objects themselves.

### UI

UI is built from components on scene objects. The **UI Editor** tab (next to
Viewport; Windows -> UI Editor) previews a canvas at a chosen resolution: click
to select, drag to move, drag the handles to resize, arrows nudge, Ctrl snaps.
Its **Templates** menu builds Main Menu, Pause Menu, Settings, HUD, Overlay,
Dialog and Loading Screen layouts that already work in Play. Screen UI can also
be selected and dragged in the Viewport while the Game UI overlay is on (the
billboard widgets menu). Create -> UI in the Scene panel adds single elements.

A `UI Canvas` object is the root; UI components on its children are laid out
inside the game view:

| Component | Purpose |
|---|---|
| `UI Canvas` | Root. 2D (screen overlay) or 3D (world panel); scale with the reference resolution or constant pixel size |
| `UI Rect` | Anchors / pivot / position / size relative to the parent (y down, (0,0) = top-left). Without one an element fills its parent |
| `UI Panel` | Filled rect: color, corner radius, border |
| `UI Image` | Texture with tint, optional aspect fit |
| `UI Text` | Text: size, color, alignment, wrap, optional .ttf/.otf font |
| `UI Button` | Clickable, tints the panel by state |
| `UI Toggle` | Checkbox (`on`); `Inverts Y of` an action (e.g. `Look`) makes it an Invert Look setting saved to the player's bindings |
| `UI Slider` | Draggable `value` in `min..=max`; optional Audio bus it drives (e.g. `music`) |
| `UI Progress Bar` | Health / loading bar: `value` 0..1, fill direction, colors |
| `UI Pause Menu` | On the canvas: Escape / gamepad back toggles the named panel, pausing time and freeing the cursor |

Buttons have an **On click** list that needs no code: Load scene, Show / Hide
/ Toggle an object by name, Quit. A Resume button is `Hide Pause Panel`; a
Settings button hides the menu page and shows the settings page.

Gameplay reads and writes them like any component:

```rust
// Menu
if ctx.ui_clicked(self.play_button) { ctx.load_scene("scenes/level1.scene"); }
if ctx.ui_clicked(self.quit_button) { ctx.quit(); }

// HUD
let score = self.score;
ctx.modify::<UiText>(self.score_label, move |t| t.text = format!("Score {score}"));

// Settings
if let Some(s) = ctx.get::<UiSlider>(self.volume_slider) { self.volume = s.value; }
```

Clicks are reported the frame after they happen. Use `ctx.pointer_over_ui()`
to ignore world clicks that landed on UI.

### UI: world space, text fields, navigation

Set a canvas's Render mode to **3D (world panel)** (or 3D in the UI Editor,
which places it in front of the camera) for a panel in the scene (VR
menus, in-world screens): it renders to a texture shown on a quad in the
object's XY plane (facing +Z), sized by "Size (m)". It scales from the same
reference size as 2D, so a layout keeps its proportions in either mode. The mouse (through the
camera) and the right VR controller (trigger = click) point at it.

`UI Text Field` is single-line text entry: click (or navigate + submit) to
type, Enter submits, Escape cancels. While typing, keyboard input goes to the
field, not gameplay.

```rust
if ctx.ui_submitted(self.name_field) {
    let name = ctx.get::<UiTextField>(self.name_field).map(|f| f.text.clone());
}
if ctx.ui_cancel_pressed() { /* close the menu */ }
```

Keyboard / gamepad navigation works on every menu: arrows, D-pad or left
stick move focus (highlighted), Enter / gamepad South presses, Escape / East
is cancel; left / right step a focused slider.

### Joints

`Joint` connects this object's rigid body to `Connected body` (or to its start
point in the world): Fixed, Hinge, Ball, Slider, Spring, Rope, with limits and
motors.

```rust
ctx.set_joint_motor(self.door, 90.0, 500.0);  // deg/s (hinge) or m/s (slider), max force
```

### Navigation

Add a `NavMesh Surface` (its box covers the walkable area; "Show navmesh"
previews the bake in the editor). It bakes from colliders when play starts.
Give characters a `Nav Agent` (plus a Character Controller for collisions):

```rust
ctx.set_destination(self.enemy, player_pos);
ctx.stop_agent(self.enemy);
let path = ctx.find_path(from, to);          // Option<Vec<Vec3>>
if ctx.get::<NavAgent>(self.enemy).is_some_and(|a| a.arrived()) { }
ctx.rebake_navmesh();                         // after the level changed
```

### Particles

`Particle Emitter`: rate, bursts, lifetime, speed, cone, gravity, drag, size
and color over lifetime, optional texture; previews in the editor.

```rust
ctx.emit_particles(self.sparks, 40);
ctx.play_particles(self.smoke);
ctx.stop_particles(self.smoke);
```

### Full-body tracking

Add `Humanoid IK` to a humanoid skinned model. Source Auto drives it from this
machine's headset, controllers and trackers (head, hands, hips, feet, chest,
elbows, knees; tracking space sits on the VR Player), or from the replicated
trackers when it belongs to a remote player's Network Avatar. Objects drives
each role from a scene object. Without a hip tracker the pelvis follows the
head; without foot trackers the feet stay planted and step. Calibrate while
the player holds a T-pose:

```rust
ctx.calibrate_trackers();
ctx.clear_tracker_calibration();
```

Players also calibrate from the VR body menu (the VR Player's Menu button):
height measurement, a guided T-pose that assigns tracker roles, and a floor
check. The VR Player's Avatar scaling fits the avatar to the player's height
(Scale avatar, Scale player, Off). See [vr-avatars.md](vr-avatars.md).

### Face tracking, lip sync and voice

`Face Tracking` drives a model's blendshapes from Unified Expressions data. The
headset's face and eye tracking is read directly through OpenXR (WiVRn, Virtual
Desktop, SteamVR where it exposes `XR_FB_face_tracking2` and
`XR_EXT_eye_gaze_interaction`); tools that send OSC (VRCFaceTracking, Babble,
Steam Link) work when Project Settings > External tracking enables the receiver,
or from code with `ctx.set_osc_face_tracking(true, 9000)`. Blendshapes match by
name automatically (Unified Expressions, ARKit, VRM blinks); `mappings` cover the
rest. Remote players' faces replicate through their Network Avatar.

`Blend Shapes` holds fixed weights you set in the inspector or from code.
`Lip Sync` drives viseme blendshapes (`vrc.v_aa`, `v_aa`, VRM A/I/U/E/O, VRoid
`Fcl_MTH_A`) from the speaker's voice, when face tracking is off or always; on
avatars without visemes it moves the jaw and lips instead. `Voice Chat` (one per
scene) turns voice on; see [networking.md](networking.md#voice-chat).

SlimeVR: Project Settings > External tracking > SlimeVR connects to a running
SlimeVR server directly (as its SteamVR driver would), sends the headset and
controllers and reads the waist, feet, knees, elbows and chest trackers.

### Bone cloth

Add `Bone Cloth` to a skinned model and pick root joints (hair roots, a skirt
ring, a tail); every joint below them swings. Add `Cloth Colliders` to the same
model for spheres, tapered capsules and planes that follow joints, or press
**Fit capsules** to wrap the hips, legs and arms from the skinned vertices.
Tune motion in the inspector (stiffness toward the animated pose, gravity,
friction, edge collision, world inertia and the teleport thresholds). The cloth
runs in the skinning pass, so it works with Animators, GPU skinning and
full-body IK avatars. Cloth can also collide with the Cloth Colliders of other
objects listed under **Collide with another object**.

`Mesh Cloth` simulates a whole skinned submesh (skirts, dresses, capes):
pin it with a vertex colour channel, bones or the top band, then tune stretch,
bending, keep-shape and self-collision. `Wind Zone` blows on both kinds of
cloth and raises waves on Water. VRM avatars (`.vrm`) bring their spring bones
in as Bone Cloth and Cloth Colliders when added to a scene.

Read the simulated particles from code with
`ctx.get::<BoneCloth>(id).and_then(|c| c.runtime.sim.as_ref())`.

### Multi-scene games and seamless loading

Scenes load on a background thread. Preload the next scene while a transition
plays (a fall, a corridor, an elevator), then switch instantly:

```rust
ctx.preload_scene("scenes/level2.scene");          // starts loading now
ctx.keep_on_scene_change(self.player, true);       // carry the player over
ctx.load_scene("scenes/level2.scene");             // instant once preloaded
ctx.load_scene_additive("scenes/room_b.scene");    // stream a room in
ctx.unload_scene("scenes/room_a.scene");           // and out
```

The `Scene Portal` component does this without code: put it on an object with
a trigger collider, set the scene and mode (Switch / Load additive / Unload).
It preloads when the activator (default: the Pawn) comes within "Preload
distance" and fires when the activator enters the trigger. Carried objects
keep their ids and respawn at a matching Spawn Point if their Pawn has a
spawn tag. Additive scenes use probe / realtime lighting (their baked
lightmaps aren't applied).

### User-generated content

Load `.zestenginepak` worlds, avatars and props at runtime (path, URL or content id);
results arrive as `ContentEvent`s. Details: [user-content.md](user-content.md).

```rust
let req = ctx.load_content("https://example.com/forest.zestenginepak");
let (req, root) = ctx.spawn_content("chair.zestenginepak", pos, rot);
ctx.spawn_avatar(self.player, "avatars/robot.zestenginepak");
ctx.enter_world(world_id);
for e in ctx.content_events() { /* Progress / Loaded / Spawned / WorldEntered / Failed */ }
```

### Save data, localization, audio buses

```rust
ctx.save_value("level", 3i64);
ctx.write_save("slot1");              // user data folder in a built game
ctx.load_save("slot1");
let level = ctx.save_data().int("level");

ctx.tr("menu.play");                  // localization/*.loc tables
ctx.set_language("fr");

ctx.set_bus_volume("music", 0.5);     // Audio Mixer buses (Audio Source "Mixer bus")
```

### App control

```rust
ctx.set_cursor_locked(true);   // mouse-look; Escape releases it in the editor
ctx.set_time_scale(0.0);       // pause gameplay (dt = 0, physics stops)
ctx.quit();                    // exits the game; ends Play mode in the editor
```

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

Worked example, orbit a target, correct at any depth in the hierarchy:

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
in `zestengine-core`; their inspectors/gizmos in `zestengine-editor`.

| Name | Purpose |
|---|---|
| `Camera` | FOV / near / far; viewport draws a frustum widget |
| `Light` | Directional / Point / Spot; realtime / baked / mixed; shadows |
| `Light Probe Volume` | Box grid of irradiance probes for the bake |
| `Audio Source` | Spatial / non-spatial clip with distance rolloff |
| `Audio Listener` | Marks the "ears" for spatial audio |
| `Box Collider` / `Sphere Collider` / `Capsule Collider` / `Mesh Collider` | Collision shapes; `is_trigger` makes a trigger. Mesh = exact triangles when static, convex hull when moving. Optional physics material |
| `Rigid Body` | Dynamic / Kinematic / Fixed body, mass, gravity scale, physics material (or plain bounciness / friction) |
| `Character Controller` | Kinematic capsule for walking characters (`move_character`, `is_grounded`) |
| `Pawn` | Ready-made player controller (first / third person, top-down, strategy) |
| `Face Tracking`, `Blend Shapes`, `Lip Sync` | Unified Expressions face tracking onto blendshapes and eye bones, fixed blendshape weights, viseme lip sync from voice |
| `Voice Chat` | Scene voice settings: proximity / global, falloff, voice activity / push to talk, Opus bitrate |
| `Humanoid IK` | Full-body tracking IK for a humanoid model: local trackers, a remote player's replicated trackers or scene objects; planted feet with procedural steps |
| `Mesh Cloth`, `Wind Zone` | Simulates a skinned garment mesh; wind for cloth and water |
| `Water`, `Buoyancy` | A body of water (Gerstner waves, wind seas, depth, current, look). Waves calm over shallows near a shore: water shallower than `Shore calm depth` (default 2 m, 0 = off) over the static ground under it, baked automatically and rebaked when the ground or the water changes. Every Rigid Body floats by its density; `Buoyancy` optionally overrides the lift and sample points (`ctx.water_surface(p)`) |
| `Wave Rider` | Rides the waves without a physics body: Float (sits on the surface at its draft) or Suspended (keeps its depth, orbital motion fading with depth); size, weight and tilt shape the response, steep waves push light things along. Deterministic from the anchor and the shared water clock, so networked clients agree; read `state.on_surface`, `state.depth`, `state.surface` |
| `Waterfall`, `Whirlpool` | A sheet of water poured off a lip (drag the lip and speed handles) that lands in spray, mist, foam and a swirling plunge pool; a standalone swirl that carries floating bodies and foam |
| `Bone Cloth`, `Cloth Colliders` | Secondary motion for joint chains (hair, skirts, tails) and the joint-following spheres / tapered capsules / planes it collides with, with capsule auto-fit |
| `Animator` | Plays / cross-fades clips (imported or `.anim`) on skinned meshes or object hierarchies, optionally driven by an Animator Controller state machine |
| `UI Canvas`, `UI Rect`, `UI Panel`, `UI Image`, `UI Text`, `UI Button`, `UI Toggle`, `UI Slider`, `UI Text Field`, `UI Progress Bar`, `UI Pause Menu` | In-game UI (see [UI](#ui)) |
| `Joint` | Fixed / hinge / ball / slider / spring / rope between bodies |
| `NavMesh Surface`, `Nav Agent` | Navigation mesh bake + path-following characters |
| `Particle Emitter` | Camera-facing particle effects |
| `Scene Portal` | Trigger-driven seamless scene switch / streaming |
| `LOD Group` | Swap to lower-detail models by camera distance |
| `Spawn Point` | Tagged spawn location (`ctx.spawn_point(tag)`) |
| `Network Object`, `Network Variables`, `Network Animator`, `Network Avatar`, `Network Manager` | Multiplayer: replicated identity / transform / physics, synced variables, animator and VR pose sync, session settings (see [networking.md](networking.md)) |
| `Avatar Descriptor` | Root of an avatar content package: eye point, humanoid bone-map overrides, body capsule (see [user-content.md](user-content.md)) |
| `Spin` | Constant rotation about a local axis |
| `Bob` | Sine hover along an axis |
| `Health`, `Target Dummy` | Hit points with regen and a death action (`DamageEvent` / `DeathEvent`); a shooting-range target that tips over, scores (`ScoreEvent`) and stands back up |
| `Hitscan Weapon` | Raycast gun: fire on an action or a held VR trigger, damage, impulse, spread, magazine / reserve / reload, muzzle and impact particles |
| `First Person Controller`, `Third Person Controller` | FPS movement (look, WASD, jump, sprint, crouch); TPS with a collision-aware over-the-shoulder boom and aim mode |
| `Follow Camera`, `RTS Camera` | Smoothed top-down follow with wheel zoom; strategy camera with edge / WASD pan, zoom, rotate and bounds |
| `Click To Move`, `Interactable` | Click the ground to path there on the navmesh, click an interactable to walk up and use it (toggle an object, rebake the navmesh) |
| `RTS Selection`, `Selectable`, `Resource Store` | Box / click selection, right-click moves in grid formation, selection rings, named resources with income |
| `HUD Binding` | Writes Health / Ammo / Score / a resource / selected count into a UI Text (`{value}`, `{max}`) or UI Progress Bar |
| `VR Player`, `Grabbable`, `VR Avatar` | VR rig (head + hands from `ctx.screen.vr`, teleport arc, smooth move, snap turn, grab + trigger, desktop fallback), pick-up objects, a body that follows the head |

The gameplay kit components (Health through VR Avatar above) live in
`zestengine-core/src/kit/` and are what the New Project templates are built from;
their pure helpers (`formation_offsets`, `screen_rect`, `rts_camera_pose`,
`boom_position`, `teleport_arc`, `spread_direction`) are public for games that
want the math without the component. Headset state reaches every component as
`ctx.screen.vr` (`VrFrame`: stage-space head / hand poses, sticks, grips,
triggers; `active` is false without a running headset).

## Editor traits

These live in `zestengine-editor` and never link into a game. Implement them behind
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
# zestengine-editor and egui never link into the shipped binary.
editor = ["dep:zestengine-editor", "dep:egui"]

[dependencies]
zestengine-core.workspace = true
zestengine-editor = { workspace = true, optional = true }
egui = { workspace = true, optional = true }
serde.workspace = true
glam.workspace = true
```

`lib.rs` exports two registration functions. The runtime one always exists; the
editor one is feature-gated:

```rust
use zestengine_core::{ComponentRegistry, TypedComponent};
#[cfg(feature = "editor")]
use zestengine_editor::{EditorComponents, Gizmo, Inspect, InspectCtx};

// Runtime behaviour — called by the editor and by a shipped game.
#[unsafe(no_mangle)]
pub fn zestengine_register(registry: &mut ComponentRegistry) {
    registry.register::<Orbit>();
}

// Editor-only traits — compiled only with the editor feature; absent in a game.
#[cfg(feature = "editor")]
#[unsafe(no_mangle)]
pub fn zestengine_register_editor(editor: &mut EditorComponents) {
    editor.register::<Orbit>();
}
```

Registering a `NAME` that already exists **replaces** the old entry; that's how
hot-reload supersedes a previous build.

### The cdylib egui boundary

A plugin statically links its own copy of egui, so egui's `TypeId`s differ from
the host's. Any egui API backed by a **context plugin keyed by TypeId** (drag
and drop (`dnd_*`), selectable labels) panics (SIGABRT, uncatchable) when called
from plugin code. Avoid those in `Inspect`/`Gizmo`:

- Don't call egui dnd; use `InspectCtx::object_ref` (it detects drops via raw
  pointer state + a shared `usize` slot in egui memory, no dnd context).
- Don't draw selectable labels; the component inspector disables them globally.

Plain widgets (sliders, buttons, labels, painters) are fine.

## See also

- [Features.md](../Features.md) — in-game API roadmap (object graph, components,
  physics queries, input, spawn/despawn) and the editor/gameplay component split.
- [networking.md](networking.md) — multiplayer components and the `ctx` networking API.
- `crates/zestengine-core/src/lib.rs` — the authoritative API.
- `plugins/components/src/lib.rs` — the `Orbit` example, end to end.
