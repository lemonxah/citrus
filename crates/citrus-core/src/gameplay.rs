//! The gameplay half of the in-game API: object lifecycle (spawn / destroy /
//! enable), access to other components, events, physics, audio and app control.
//! Everything that mutates the world is deferred through [`ComponentCommand`]
//! and applied by the engine between passes; queries answer immediately.

use std::any::{Any, TypeId};

use glam::{Mat4, Quat, Vec2, Vec3};

use crate::{Component, ComponentCommand, ComponentCtx, ObjectId, ObjectRef};

/// A physics ray-cast hit.
#[derive(Clone, Copy, Debug)]
pub struct RayHit {
    pub object: ObjectId,
    pub point: Vec3,
    pub normal: Vec3,
    pub distance: f32,
}

/// What a ray cast / overlap test may hit.
#[derive(Clone, Copy, Debug)]
pub struct RayFilter {
    /// Layers to test (bit per layer).
    pub layer_mask: u32,
    pub include_triggers: bool,
    /// Ignore this object's colliders (typically the caster itself).
    pub exclude: Option<ObjectId>,
}

impl Default for RayFilter {
    fn default() -> Self {
        Self {
            layer_mask: u32::MAX,
            include_triggers: false,
            exclude: None,
        }
    }
}

/// The game view this frame: size, pointer and camera, plus UI interaction.
#[derive(Clone, Default)]
pub struct ScreenInfo {
    /// Game viewport size in pixels.
    pub size: Vec2,
    /// Pointer position in pixels from the viewport's top-left (None when outside).
    pub pointer: Option<Vec2>,
    pub view: Mat4,
    pub proj: Mat4,
    /// The pointer is over an interactive UI element (skip world clicks).
    pub pointer_over_ui: bool,
    /// UI buttons clicked since the last frame.
    pub ui_clicks: Vec<ObjectId>,
    /// Text fields submitted (Enter) since the last frame.
    pub ui_submits: Vec<ObjectId>,
    /// A UI text field is being edited (keyboard goes to it, not gameplay).
    pub ui_text_editing: bool,
    /// UI cancel / back (Escape, gamepad East) was pressed.
    pub ui_cancel: bool,
    /// Headset and controllers (inactive without VR).
    pub vr: crate::VrFrame,
}

/// Immediate physics queries, implemented by the engine's physics world.
pub trait PhysicsQuery {
    fn raycast(&self, origin: Vec3, dir: Vec3, max_dist: f32, filter: &RayFilter) -> Option<RayHit>;
    fn overlap_sphere(&self, center: Vec3, radius: f32, filter: &RayFilter) -> Vec<ObjectId>;
    /// Linear velocity of a rigid body.
    fn velocity(&self, id: ObjectId) -> Option<Vec3>;
    /// Whether a character controller touched the ground on its last move.
    fn grounded(&self, id: ObjectId) -> bool;
}

/// Navmesh queries, implemented by the engine's navigation system.
pub trait NavQuery {
    /// Walkable path (waypoints, start and end included) or None.
    fn find_path(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>>;
    /// Closest walkable point (within a few meters).
    fn nearest_walkable(&self, p: Vec3) -> Option<Vec3>;
}

/// Read access to components on other objects, implemented by the engine.
pub trait ComponentLookup {
    fn component(&self, object: usize, type_id: TypeId) -> Option<&dyn Component>;
}

/// A type-erased value carried by a command (events).
pub struct AnyBox(pub Box<dyn Any>);

impl std::fmt::Debug for AnyBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AnyBox")
    }
}

type DeferredFn = Box<dyn FnOnce(&mut dyn Any)>;

/// A deferred edit of another component (see [`ComponentCtx::modify`]).
pub struct Deferred(pub DeferredFn);

impl std::fmt::Debug for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Deferred")
    }
}

impl ComponentCtx<'_> {
    // ---- screen / pointer ----

    pub fn screen_size(&self) -> Vec2 {
        self.screen.size
    }

    /// Pointer position in pixels from the game view's top-left.
    pub fn pointer_position(&self) -> Option<Vec2> {
        self.screen.pointer
    }

    /// World-space ray `(origin, direction)` through a pixel of the game view
    /// (click-to-select, click-to-move). Pair with `raycast`.
    pub fn screen_ray(&self, pixel: Vec2) -> Option<(Vec3, Vec3)> {
        let size = self.screen.size;
        if size.x < 1.0 || size.y < 1.0 {
            return None;
        }
        let ndc = Vec2::new(2.0 * pixel.x / size.x - 1.0, 1.0 - 2.0 * pixel.y / size.y);
        let inv = (self.screen.proj * self.screen.view).inverse();
        let near = inv.project_point3(ndc.extend(0.0));
        let far = inv.project_point3(ndc.extend(1.0));
        let dir = (far - near).try_normalize()?;
        Some((near, dir))
    }

    /// Ray under the pointer.
    pub fn pointer_ray(&self) -> Option<(Vec3, Vec3)> {
        self.screen_ray(self.screen.pointer?)
    }

    /// Pixel position of a world point (None when behind the camera).
    pub fn world_to_screen(&self, world: Vec3) -> Option<Vec2> {
        let clip = self.screen.proj * self.screen.view * world.extend(1.0);
        if clip.w <= 1e-6 {
            return None;
        }
        let ndc = clip.truncate() / clip.w;
        Some(Vec2::new(
            (ndc.x + 1.0) * 0.5 * self.screen.size.x,
            (1.0 - ndc.y) * 0.5 * self.screen.size.y,
        ))
    }

    /// Whether the pointer is over interactive UI (ignore world clicks then).
    pub fn pointer_over_ui(&self) -> bool {
        self.screen.pointer_over_ui
    }

    /// Whether a UI button was clicked since the last frame.
    pub fn ui_clicked(&self, button: impl Into<ObjectRef>) -> bool {
        button
            .into()
            .id()
            .is_some_and(|id| self.screen.ui_clicks.contains(&id))
    }

    /// Whether a text field was submitted (Enter) since the last frame.
    pub fn ui_submitted(&self, field: impl Into<ObjectRef>) -> bool {
        field
            .into()
            .id()
            .is_some_and(|id| self.screen.ui_submits.contains(&id))
    }

    /// A UI text field has keyboard focus; ignore keyboard gameplay input.
    pub fn ui_text_editing(&self) -> bool {
        self.screen.ui_text_editing
    }

    /// UI back / cancel pressed (Escape or gamepad East), e.g. to close a menu.
    pub fn ui_cancel_pressed(&self) -> bool {
        self.screen.ui_cancel
    }

    // ---- scene streaming ----

    /// Begin loading a scene in the background (during a transition). A later
    /// `load_scene` / `load_scene_additive` of the same path is then instant.
    pub fn preload_scene(&mut self, path: impl Into<String>) {
        self.commands.push(ComponentCommand::PreloadScene(path.into()));
    }

    /// Stream a scene in next to the current one (rooms, level chunks).
    pub fn load_scene_additive(&mut self, path: impl Into<String>) {
        self.commands.push(ComponentCommand::LoadSceneAdditive(path.into()));
    }

    /// Stream an additively loaded scene back out.
    pub fn unload_scene(&mut self, path: impl Into<String>) {
        self.commands.push(ComponentCommand::UnloadScene(path.into()));
    }

    /// Carry an object (and children) into the next scene on `load_scene`.
    pub fn keep_on_scene_change(&mut self, target: impl Into<ObjectRef>, keep: bool) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::KeepOnSceneChange(id, keep));
        }
    }

    // ---- object lifecycle ----

    /// Instantiate a `.prefab` (project-relative path, e.g. `"prefabs/enemy.prefab"`)
    /// at a world position/rotation. Returns the new root's id right away; the
    /// object exists from the next pass and runs `start` on its first frame.
    pub fn spawn_prefab(&mut self, path: impl Into<String>, translation: Vec3, rotation: Quat) -> ObjectId {
        let root_id = ObjectId::new();
        self.commands.push(ComponentCommand::SpawnPrefab {
            path: path.into(),
            root_id,
            translation,
            rotation,
        });
        root_id
    }

    /// Clone a scene object (with its children and components) as a new root at
    /// a world position/rotation. The clone copies the original's enabled state,
    /// so a disabled template needs `set_active(id, true)` afterwards.
    pub fn instantiate(&mut self, original: impl Into<ObjectRef>, translation: Vec3, rotation: Quat) -> Option<ObjectId> {
        let original = original.into().id()?;
        let new_id = ObjectId::new();
        self.commands.push(ComponentCommand::Instantiate {
            original,
            new_id,
            translation,
            rotation,
        });
        Some(new_id)
    }

    /// Remove an object and its children (end of the current pass).
    pub fn destroy(&mut self, target: impl Into<ObjectRef>) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::Destroy(id));
        }
    }

    pub fn destroy_self(&mut self) {
        let id = self.self_id();
        self.commands.push(ComponentCommand::Destroy(id));
    }

    /// Enable or disable an object. Disabled objects (and their children) stop
    /// updating, rendering and colliding.
    pub fn set_active(&mut self, target: impl Into<ObjectRef>, active: bool) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::SetActive(id, active));
        }
    }

    pub fn is_active(&self, target: impl Into<ObjectRef>) -> bool {
        self.resolve(target.into())
            .and_then(|i| self.active.get(i).copied())
            .unwrap_or(false)
    }

    // ---- other components ----

    /// Read a component of type `T` on another object (or this one).
    pub fn get<T: Component>(&self, target: impl Into<ObjectRef>) -> Option<&T> {
        let index = self.resolve(target.into())?;
        self.lookup
            .component(index, TypeId::of::<T>())?
            .as_any()
            .downcast_ref::<T>()
    }

    /// Read a sibling component on the owning object.
    pub fn get_self<T: Component>(&self) -> Option<&T> {
        self.lookup
            .component(self.self_index, TypeId::of::<T>())?
            .as_any()
            .downcast_ref::<T>()
    }

    /// Edit a component of type `T` on another object, e.g.
    /// `ctx.modify::<Health>(hit.object, |h| h.hp -= 10.0)`. Applied after the
    /// current pass; a no-op if the object or component is gone by then.
    pub fn modify<T: Component>(&mut self, target: impl Into<ObjectRef>, f: impl FnOnce(&mut T) + 'static) {
        let Some(id) = target.into().id() else {
            return;
        };
        self.commands.push(ComponentCommand::Modify {
            id,
            type_id: TypeId::of::<T>(),
            f: Deferred(Box::new(move |any: &mut dyn Any| {
                if let Some(t) = any.downcast_mut::<T>() {
                    f(t);
                }
            })),
        });
    }

    // ---- particles ----

    /// (Re)start a Particle Emitter.
    pub fn play_particles(&mut self, target: impl Into<ObjectRef>) {
        self.modify::<crate::ParticleEmitter>(target, |p| p.control.play = true);
    }

    /// Stop emitting (live particles finish their lifetime).
    pub fn stop_particles(&mut self, target: impl Into<ObjectRef>) {
        self.modify::<crate::ParticleEmitter>(target, |p| p.control.stop = true);
    }

    /// Emit `count` particles immediately (explosions, hits).
    pub fn emit_particles(&mut self, target: impl Into<ObjectRef>, count: u32) {
        self.modify::<crate::ParticleEmitter>(target, move |p| p.control.emit += count);
    }

    // ---- navigation ----

    /// Send a `Nav Agent` to a world point.
    pub fn set_destination(&mut self, agent: impl Into<ObjectRef>, point: Vec3) {
        self.modify::<crate::NavAgent>(agent, move |a| a.set_destination(point));
    }

    pub fn stop_agent(&mut self, agent: impl Into<ObjectRef>) {
        self.modify::<crate::NavAgent>(agent, |a| a.stop());
    }

    /// Re-bake navmeshes (doors opened, obstacles spawned). Takes effect after
    /// the next physics step.
    pub fn rebake_navmesh(&mut self) {
        self.commands.push(ComponentCommand::RebakeNavMesh);
    }

    /// Path over the navmesh (None when unreachable or nothing is baked).
    pub fn find_path(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.nav?.find_path(from, to)
    }

    pub fn nearest_walkable(&self, p: Vec3) -> Option<Vec3> {
        self.nav?.nearest_walkable(p)
    }

    // ---- animation ----

    /// Cross-fade an object's [`crate::Animator`] to `clip` over `fade` seconds.
    pub fn play_animation(&mut self, target: impl Into<ObjectRef>, clip: &str, fade: f32) {
        let clip = clip.to_owned();
        self.modify::<crate::Animator>(target, move |a| a.play(&clip, fade));
    }

    /// Set a blend-tree parameter on an object's Animator.
    pub fn set_animator_float(&mut self, target: impl Into<ObjectRef>, name: &str, value: f32) {
        let name = name.to_owned();
        self.modify::<crate::Animator>(target, move |a| a.set_float(&name, value));
    }

    pub fn stop_animation(&mut self, target: impl Into<ObjectRef>) {
        self.modify::<crate::Animator>(target, |a| a.stop());
    }

    pub fn set_animator_int(&mut self, target: impl Into<ObjectRef>, name: &str, value: i32) {
        let name = name.to_owned();
        self.modify::<crate::Animator>(target, move |a| a.set_int(&name, value));
    }

    pub fn set_animator_bool(&mut self, target: impl Into<ObjectRef>, name: &str, value: bool) {
        let name = name.to_owned();
        self.modify::<crate::Animator>(target, move |a| a.set_bool(&name, value));
    }

    /// Fire an Animator Controller trigger (consumed by the transition it starts).
    pub fn set_animator_trigger(&mut self, target: impl Into<ObjectRef>, name: &str) {
        let name = name.to_owned();
        self.modify::<crate::Animator>(target, move |a| a.set_trigger(&name));
    }

    pub fn reset_animator_trigger(&mut self, target: impl Into<ObjectRef>, name: &str) {
        let name = name.to_owned();
        self.modify::<crate::Animator>(target, move |a| a.reset_trigger(&name));
    }

    /// Force an Animator Controller state by name, cross-fading over `fade` seconds.
    pub fn play_animator_state(&mut self, target: impl Into<ObjectRef>, state: &str, fade: f32) {
        let state = state.to_owned();
        self.modify::<crate::Animator>(target, move |a| a.play_state(&state, fade));
    }

    /// A parameter of an object's Animator (0 when it has none).
    pub fn animator_float(&self, target: impl Into<ObjectRef>, name: &str) -> f32 {
        self.get::<crate::Animator>(target).map(|a| a.get_float(name)).unwrap_or(0.0)
    }

    pub fn animator_bool(&self, target: impl Into<ObjectRef>, name: &str) -> bool {
        self.get::<crate::Animator>(target).is_some_and(|a| a.get_bool(name))
    }

    /// Current controller state of an object's Animator on `layer`.
    pub fn animator_state(&self, target: impl Into<ObjectRef>, layer: usize) -> Option<String> {
        self.get::<crate::Animator>(target).and_then(|a| a.current_state(layer).map(str::to_owned))
    }

    // ---- events ----

    /// Broadcast an event of any type; every component can read it next frame
    /// with [`ComponentCtx::events`].
    pub fn emit<E: Any>(&mut self, event: E) {
        self.commands.push(ComponentCommand::Emit(AnyBox(Box::new(event))));
    }

    /// Events of type `E` emitted last frame.
    pub fn events<E: Any>(&self) -> impl Iterator<Item = &E> {
        self.events.iter().filter_map(|e| e.0.downcast_ref::<E>())
    }

    // ---- physics ----

    /// Nearest collider along a ray, ignoring triggers and the owning object.
    pub fn raycast(&self, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RayHit> {
        let filter = RayFilter {
            exclude: Some(self.self_id()),
            ..Default::default()
        };
        self.raycast_filtered(origin, dir, max_dist, &filter)
    }

    pub fn raycast_filtered(&self, origin: Vec3, dir: Vec3, max_dist: f32, filter: &RayFilter) -> Option<RayHit> {
        self.physics?.raycast(origin, dir, max_dist, filter)
    }

    /// Objects whose colliders intersect a sphere (triggers excluded).
    pub fn overlap_sphere(&self, center: Vec3, radius: f32) -> Vec<ObjectId> {
        self.overlap_sphere_filtered(center, radius, &RayFilter::default())
    }

    pub fn overlap_sphere_filtered(&self, center: Vec3, radius: f32, filter: &RayFilter) -> Vec<ObjectId> {
        self.physics
            .map(|p| p.overlap_sphere(center, radius, filter))
            .unwrap_or_default()
    }

    /// Linear velocity of a rigid body (zero if it has none).
    pub fn velocity(&self, target: impl Into<ObjectRef>) -> Vec3 {
        target
            .into()
            .id()
            .and_then(|id| self.physics?.velocity(id))
            .unwrap_or(Vec3::ZERO)
    }

    pub fn set_velocity(&mut self, target: impl Into<ObjectRef>, linear: Vec3) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::SetVelocity { id, linear });
        }
    }

    /// Continuous force (Newtons), applied over the next physics step.
    pub fn add_force(&mut self, target: impl Into<ObjectRef>, force: Vec3) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::AddForce { id, force });
        }
    }

    /// Instant velocity change scaled by mass (jumps, hits, explosions).
    pub fn add_impulse(&mut self, target: impl Into<ObjectRef>, impulse: Vec3) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::AddImpulse { id, impulse });
        }
    }

    pub fn add_torque(&mut self, target: impl Into<ObjectRef>, torque: Vec3) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::AddTorque { id, torque });
        }
    }

    /// Drive the motor of the `Joint` on an object (deg/s for hinges, m/s for
    /// sliders). Speed 0 with a max force acts as a brake.
    pub fn set_joint_motor(&mut self, target: impl Into<ObjectRef>, speed: f32, max_force: f32) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::SetJointMotor { id, speed, max_force });
        }
    }

    /// Move the owning object's [`crate::CharacterController`] by a world-space
    /// delta (include gravity yourself). Collisions are resolved immediately
    /// after the current pass.
    pub fn move_character(&mut self, delta: Vec3) {
        let id = self.self_id();
        self.commands.push(ComponentCommand::MoveCharacter { id, delta });
    }

    /// Whether the owning character controller was on the ground after its last move.
    pub fn is_grounded(&self) -> bool {
        self.physics.is_some_and(|p| p.grounded(self.self_id()))
    }

    // ---- audio ----

    /// Play a clip once, non-spatially (UI, music stingers).
    pub fn play_sound(&mut self, clip: impl Into<String>, volume: f32) {
        self.commands.push(ComponentCommand::PlaySound {
            clip: clip.into(),
            position: None,
            volume,
        });
    }

    /// Play a clip once at a world position (gunshots, impacts).
    pub fn play_sound_at(&mut self, clip: impl Into<String>, position: Vec3, volume: f32) {
        self.commands.push(ComponentCommand::PlaySound {
            clip: clip.into(),
            position: Some(position),
            volume,
        });
    }

    /// (Re)start the `AudioSource` on an object.
    pub fn play_audio(&mut self, target: impl Into<ObjectRef>) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::PlayAudio(id));
        }
    }

    pub fn stop_audio(&mut self, target: impl Into<ObjectRef>) {
        if let Some(id) = target.into().id() {
            self.commands.push(ComponentCommand::StopAudio(id));
        }
    }

    // ---- save data ----

    /// The persistent save data (read). Typed getters: `int`, `float`, `bool`,
    /// `str`, `vec3`.
    pub fn save_data(&self) -> &crate::SaveGame {
        self.save
    }

    /// Set a save value (applied after this pass; written to disk by `write_save`).
    pub fn save_value(&mut self, key: impl Into<String>, value: impl Into<crate::SaveValue>) {
        self.commands.push(ComponentCommand::SaveSet { key: key.into(), value: value.into() });
    }

    pub fn remove_save_value(&mut self, key: impl Into<String>) {
        self.commands.push(ComponentCommand::SaveRemove(key.into()));
    }

    /// Write the save data to slot `slot` in the user's save folder.
    pub fn write_save(&mut self, slot: impl Into<String>) {
        self.commands.push(ComponentCommand::SaveWrite(slot.into()));
    }

    /// Load slot `slot` (replaces the current save data; readable next pass).
    pub fn load_save(&mut self, slot: impl Into<String>) {
        self.commands.push(ComponentCommand::SaveLoad(slot.into()));
    }

    // ---- localization ----

    /// Translated string for `key` in the current language (the key itself
    /// when missing).
    pub fn tr<'k>(&'k self, key: &'k str) -> &'k str {
        self.strings.tr(key)
    }

    pub fn language(&self) -> &str {
        self.strings.current_language()
    }

    pub fn set_language(&mut self, lang: impl Into<String>) {
        self.commands.push(ComponentCommand::SetLanguage(lang.into()));
    }

    // ---- audio mixer ----

    /// Set a mixer bus volume (0..1), e.g. from a settings menu slider.
    pub fn set_bus_volume(&mut self, bus: impl Into<String>, volume: f32) {
        self.commands.push(ComponentCommand::SetBusVolume { bus: bus.into(), volume });
    }

    pub fn set_bus_muted(&mut self, bus: impl Into<String>, muted: bool) {
        self.commands.push(ComponentCommand::SetBusMuted { bus: bus.into(), muted });
    }

    // ---- app ----

    /// Lock and hide the cursor for mouse-look, or release it.
    pub fn set_cursor_locked(&mut self, locked: bool) {
        self.commands.push(ComponentCommand::SetCursorLocked(locked));
    }

    /// 0 pauses gameplay (`dt` becomes 0, physics stops), 1 is normal speed.
    pub fn set_time_scale(&mut self, scale: f32) {
        self.commands.push(ComponentCommand::SetTimeScale(scale.max(0.0)));
    }

    pub fn quit(&mut self) {
        self.commands.push(ComponentCommand::Quit);
    }

    // ---- water ----

    /// The water surface above or below `p` (world): its height and normal,
    /// or None when `p` isn't over any Water volume. Matches what floats and
    /// what's rendered at the current play time.
    pub fn water_surface(&self, p: glam::Vec3) -> Option<(f32, glam::Vec3)> {
        let ty = std::any::TypeId::of::<crate::Water>();
        (0..self.object_ids.len()).find_map(|i| {
            let water = self.lookup.component(i, ty)?.as_any().downcast_ref::<crate::Water>()?;
            water.height_at(self.object_matrix(i)?, p, self.water_time)
        })
    }

    /// Water in a [`crate::WaterContainer`]: litres and share of its
    /// capacity (as of the last physics step).
    pub fn container_water(&self, target: impl Into<ObjectRef>) -> Option<(f32, f32)> {
        let c = self.get::<crate::WaterContainer>(target)?;
        let cap = c.runtime.capacity.max(1e-6);
        Some((c.runtime.litres, (c.runtime.litres / cap).clamp(0.0, 1.0)))
    }

    /// Add `litres` to a container (what doesn't fit pours over the rim).
    pub fn fill_container(&mut self, target: impl Into<ObjectRef>, litres: f32) {
        let litres = litres.max(0.0);
        self.modify::<crate::WaterContainer>(target, move |c| c.runtime.add += litres);
    }

    /// Pour `litres` out of a container over its lowest rim point, as a
    /// stream (whatever it holds, at most).
    pub fn pour_container(&mut self, target: impl Into<ObjectRef>, litres: f32) {
        let litres = litres.max(0.0);
        self.modify::<crate::WaterContainer>(target, move |c| c.runtime.pour += litres);
    }

    // ---- full-body tracking ----

    /// Calibrate full-body tracking: call while the player stands in a T-pose
    /// matching their avatar. Lines the avatar up under the headset and stores
    /// each tracker's offset to its bone.
    pub fn calibrate_trackers(&mut self) {
        self.commands.push(ComponentCommand::CalibrateTrackers);
    }

    pub fn clear_tracker_calibration(&mut self) {
        self.commands.push(ComponentCommand::ClearTrackerCalibration);
    }

    /// Start or stop the OSC receiver that takes face tracking from external
    /// tools (VRCFaceTracking-style `/avatar/parameters/FT/v2/...` floats) on
    /// `port` (9000 is the usual). Headset face tracking needs no receiver.
    pub fn set_osc_face_tracking(&mut self, enabled: bool, port: u16) {
        self.commands.push(ComponentCommand::SetOscFaceTracking(enabled.then_some(port)));
    }
}
