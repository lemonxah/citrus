//! One running game: the per-frame gameplay loop shared by the editor's Play
//! mode and the standalone runtime, so both behave identically.
//!
//! Frame order (Unity-style):
//! 1. `start` for every newly active object (Play start, spawned, re-enabled)
//! 2. fixed steps at [`FIXED_DT`]: `fixed_update`, then one physics step each
//! 3. collision / trigger callbacks
//! 4. `update`, then `late_update`
//! 5. audio spatialization
//!
//! Component commands that touch the world (spawn, destroy, physics, audio …)
//! are applied right after the pass that issued them. The rest (scene switch,
//! window, networking, quit) are returned to the host from [`PlaySession::tick`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use glam::{Mat4, Quat, Vec3};

use zestengine_core::{AnyBox, Component, ComponentCommand, ComponentCtx, ComponentRegistry, InputState, NetView, ObjectId};
use zestengine_render::Renderer;

use crate::audio::AudioEngine;
use crate::navmesh::NavSystem;
use crate::physics::{ContactEvent, FIXED_DT, PhysicsWorld};
use crate::prefab::Prefab;
use crate::scene::{LoadedScene, PassEnv};
use crate::shaders::ShaderLibrary;

#[path = "play_net.rs"]
mod play_net;

/// Upper bound on fixed steps per frame, so a long hitch can't snowball.
const MAX_STEPS: u32 = 8;

/// The mutable engine state a tick works on.
pub struct World<'a> {
    pub scene: &'a mut LoadedScene,
    /// Needed only to spawn prefabs (mesh / material upload). None in headless tests.
    pub renderer: Option<&'a mut Renderer>,
    pub registry: &'a ComponentRegistry,
    pub shaders: &'a mut ShaderLibrary,
}

pub struct TickInput<'a> {
    /// Real frame time in seconds.
    pub dt: f32,
    pub input: &'a InputState,
    pub net: &'a NetView,
    /// Listener pose when the scene has no `AudioListener` (the camera's
    /// world transform).
    pub listener_fallback: Mat4,
    /// Game view, pointer, camera and UI clicks for this frame.
    pub screen: &'a zestengine_core::ScreenInfo,
}

/// CPU cost of the last tick, for the profiler overlay.
#[derive(Clone, Copy, Default)]
#[cfg_attr(not(feature = "editor"), allow(dead_code))]
pub struct PlayTimings {
    pub components_ms: f32,
    pub physics_ms: f32,
    pub audio_ms: f32,
}

pub struct PlaySession {
    physics: PhysicsWorld,
    physics_settings: zestengine_core::PhysicsSettings,
    /// Water clock minus play time (non-zero when following a shared clock).
    water_offset: f32,
    nav: NavSystem,
    audio: Option<AudioEngine>,
    project_root: PathBuf,
    started: HashSet<ObjectId>,
    accumulator: f32,
    /// Scaled game time in seconds.
    pub time: f32,
    pub time_scale: f32,
    /// Readable this frame (emitted last frame).
    events_prev: Vec<AnyBox>,
    events_next: Vec<AnyBox>,
    prefabs: HashMap<String, Prefab>,
    listener: Mat4,
    pub timings: PlayTimings,
    save: zestengine_core::SaveGame,
    save_dir: PathBuf,
    strings: zestengine_core::Localization,
    mixer: zestengine_core::AudioMixer,
    /// Object-hierarchy rigs for Animators without a skinned mesh: the rig and
    /// the scene object id of each node (rest = transforms at first use).
    object_rigs: HashMap<ObjectId, (crate::anim_graph::Rig, Vec<ObjectId>)>,
    /// World position of each parameter-driver source last frame.
    /// Last seen position + smoothed velocity per driver source object.
    driver_prev: HashMap<ObjectId, (Vec3, Vec3)>,
    /// Water Containers' water and the streams poured from them.
    carry: crate::water_carry::CarrySim,
    /// Static objects and still colliders as walls in the simulated water.
    water_walls: crate::water_walls::WaterWalls,
    /// A UI text field had the keyboard last tick (prediction records no
    /// gameplay input then).
    typing: bool,
}

#[derive(Clone, Copy)]
enum Contact {
    CollisionEnter,
    CollisionExit,
    TriggerEnter,
    TriggerExit,
}

impl PlaySession {
    /// Begin playing `scene`. `audio` is the output device to play through
    /// (None = silent); hand it back with [`PlaySession::end`].
    pub fn new(scene: &LoadedScene, project_root: &Path, audio: Option<AudioEngine>) -> Self {
        Self {
            physics: PhysicsWorld::build_with(scene, Some(project_root), &zestengine_core::PhysicsSettings::default()),
            physics_settings: zestengine_core::PhysicsSettings::default(),
            water_offset: 0.0,
            nav: NavSystem::default(),
            audio,
            project_root: project_root.to_owned(),
            started: HashSet::new(),
            accumulator: 0.0,
            time: 0.0,
            time_scale: 1.0,
            events_prev: Vec::new(),
            events_next: Vec::new(),
            prefabs: HashMap::new(),
            listener: Mat4::IDENTITY,
            timings: PlayTimings::default(),
            save: zestengine_core::SaveGame::new(),
            save_dir: project_root.join(".zestengine").join("saves"),
            strings: load_strings(project_root),
            mixer: zestengine_core::AudioMixer::default(),
            object_rigs: HashMap::new(),
            driver_prev: HashMap::new(),
            carry: crate::water_carry::CarrySim::default(),
            water_walls: crate::water_walls::WaterWalls::default(),
            typing: false,
        }
    }

    /// The water clock: play time, or the networked game's shared clock.
    pub fn water_time(&self) -> f32 {
        self.time + self.water_offset
    }

    /// Follow a clock every peer agrees on for the water (the host's, from
    /// [`crate::net::NetSession::shared_clock`]); None = local play time.
    pub fn set_shared_clock(&mut self, clock: Option<f64>) {
        let target = clock.map_or(0.0, |c| (c - self.time as f64) as f32);
        if (target - self.water_offset).abs() > 0.5 {
            self.water_offset = target;
        } else {
            self.water_offset += (target - self.water_offset) * 0.1;
        }
    }

    /// Project physics (gravity, contact stiffness, default material). Rebuilds
    /// the simulation, so call it before the first tick.
    pub fn set_physics(&mut self, settings: &zestengine_core::PhysicsSettings, scene: &LoadedScene) {
        self.physics_settings = settings.clone();
        self.physics = PhysicsWorld::build_with(scene, Some(&self.project_root), settings);
    }

    /// Where `ctx.write_save` / `load_save` slots live (default: the
    /// project's `.zestengine/saves`; a built game uses the user's data folder).
    pub fn set_save_dir(&mut self, dir: PathBuf) {
        self.save_dir = dir;
    }

    /// Audio bus volumes (from the project's Audio Mixer).
    /// Apply the project's Steam Audio settings to the audio device.
    pub fn set_spatial_audio(&mut self, settings: &zestengine_core::SpatialAudioSettings) {
        if let Some(audio) = self.audio.as_mut() {
            audio.configure(settings, &self.project_root);
        }
    }

    pub fn set_mixer(&mut self, mixer: zestengine_core::AudioMixer) {
        self.mixer = mixer;
    }

    fn slot_path(&self, slot: &str) -> PathBuf {
        let safe: String = slot
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        self.save_dir.join(format!("{safe}.save"))
    }

    /// A different scene was loaded mid-play: start it fresh.
    pub fn scene_changed(&mut self, scene: &LoadedScene) {
        self.physics = PhysicsWorld::build_with(scene, Some(&self.project_root), &self.physics_settings);
        self.nav = NavSystem::default();
        self.started.clear();
        self.accumulator = 0.0;
        self.events_prev.clear();
        self.events_next.clear();
        self.object_rigs.clear();
        self.driver_prev.clear();
        self.carry.reset();
        if let Some(audio) = self.audio.as_mut() {
            audio.stop_all();
        }
    }

    /// Objects were added / removed outside the session (additive streaming).
    pub fn structure_changed(&mut self) {
        self.physics.mark_dirty();
        self.nav.invalidate();
    }

    /// Deliver an engine-side event (e.g. content loading) to every component
    /// on the next tick via `ctx.events`.
    pub fn post_event<E: std::any::Any>(&mut self, event: E) {
        self.events_prev.push(AnyBox(Box::new(event)));
    }

    /// Stop playing; returns the audio device for reuse.
    #[cfg_attr(not(feature = "editor"), allow(dead_code))]
    pub fn end(mut self) -> Option<AudioEngine> {
        if let Some(audio) = self.audio.as_mut() {
            audio.stop_all();
        }
        self.audio
    }

    /// Advance one frame. Returns the commands only the host can apply.
    pub fn tick(&mut self, w: &mut World, inp: &TickInput) -> Vec<ComponentCommand> {
        let tick_start = Instant::now();
        let unscaled = inp.dt.clamp(0.0, 0.25);
        let dt = unscaled * self.time_scale;
        self.time += dt;
        self.listener = crate::audio::listener_pose(w.scene).unwrap_or(inp.listener_fallback);
        let events = std::mem::take(&mut self.events_prev);
        let mut host = Vec::new();
        self.typing = inp.screen.ui_text_editing;

        self.net_pre_tick(w, inp.net, &mut host);
        self.start_pending(w, inp, dt, unscaled, &events, &mut host);
        self.apply_animation_output(w, inp, dt, unscaled, &events, &mut host);

        let phys_start = Instant::now();
        let mut fixed_ms = 0.0;
        self.physics.sync_structure(w.scene);
        self.carry.track(w.scene, dt);
        self.accumulator += dt;
        let mut steps = 0;
        while self.accumulator >= FIXED_DT && steps < MAX_STEPS {
            let t = Instant::now();
            self.pass(w, inp, FIXED_DT, unscaled, &events, &mut host, |_| true, |_, c, ctx| {
                c.fixed_update(ctx)
            });
            fixed_ms += t.elapsed().as_secs_f32() * 1000.0;
            self.physics.sync_structure(w.scene);
            let ids = w.scene.id_index();
            let world = w.scene.world_transforms();
            self.physics.push_poses(w.scene, &ids, &world);
            let vortices = crate::water_fx::scene_vortices(w.scene, &world, self.water_time());
            let mut water = self.physics.apply_water(w.scene, &world, self.water_time(), &vortices);
            water.fx_pushes.append(&mut w.scene.water_fx_pushes);
            for &(i, frac) in &water.submerged {
                if let Some(b) = w.scene.objects[i]
                    .components
                    .iter_mut()
                    .find_map(|c| c.as_any_mut().downcast_mut::<zestengine_core::Buoyancy>())
                {
                    b.submerged = frac;
                }
            }
            let water_t = self.water_time();
            self.carry.step(w.scene, &world, Some(&mut self.physics), water_t, FIXED_DT, &mut water);
            crate::physics::step_ripples(w.scene, &world, self.listener.w_axis.truncate(), &water, Some(&self.physics), water_t, FIXED_DT);
            self.water_walls.update(w.scene, &world);
            w.scene.water_splashes.extend(water.splashes.iter().copied());
            // One wake per body per frame is plenty for the effects.
            for wake in &water.wakes {
                if !w.scene.water_wakes.iter().any(|x| x.at.distance_squared(wake.at) < 1e-4) {
                    w.scene.water_wakes.push(crate::water_fx::FxWake { at: wake.at, flow: wake.flow, radius: wake.radius });
                }
            }
            self.physics.step();
            self.accumulator -= FIXED_DT;
            steps += 1;
        }
        if steps == MAX_STEPS {
            self.accumulator = self.accumulator.min(FIXED_DT);
        }
        if steps > 0 {
            self.physics.reset_forces();
            // What the inspector shows as each body's real mass and density.
            for o in w.scene.objects.iter_mut() {
                let id = o.id;
                if let Some(rb) = o.components.iter_mut().find_map(|c| c.as_any_mut().downcast_mut::<zestengine_core::RigidBody>()) {
                    rb.measured = self.physics.mass_and_volume(id);
                }
            }
        }
        let ids = w.scene.id_index();
        let alpha = (self.accumulator / FIXED_DT).clamp(0.0, 1.0);
        self.physics.write_back(w.scene, &ids, alpha);
        w.scene.water_pours = self.carry.snapshot(alpha);
        // Navmeshes bake once the physics world has stepped (its query
        // structure is built by the step).
        if steps > 0 && self.nav.needs_bake() {
            self.nav.bake(w.scene, &self.physics);
        }
        let physics_ms = phys_start.elapsed().as_secs_f32() * 1000.0 - fixed_ms;

        let contacts = self.physics.take_events();
        if !contacts.is_empty() {
            self.deliver_contacts(w, inp, dt, unscaled, &events, &mut host, &contacts);
        }

        self.pass(w, inp, dt, unscaled, &events, &mut host, |_| true, |_, c, ctx| c.update(ctx));
        self.nav.move_agents(w.scene, &mut self.physics, dt);
        self.drive_animator_params(w.scene, dt);
        self.animate_objects(w.scene, dt);
        self.pass(w, inp, dt, unscaled, &events, &mut host, |_| true, |_, c, ctx| c.late_update(ctx));

        let audio_start = Instant::now();
        let water_t = self.water_time();
        if let Some(audio) = self.audio.as_mut() {
            audio.sync_geometry(w.scene);
            audio.update(&crate::audio::gather_cues(w.scene), self.listener, &self.mixer);
            let under = crate::underwater::listener(w.scene, self.listener.w_axis.truncate(), water_t);
            audio.set_underwater(under.as_ref(), unscaled, &self.project_root, &self.mixer);
        }
        let audio_ms = audio_start.elapsed().as_secs_f32() * 1000.0;

        self.events_prev = std::mem::take(&mut self.events_next);
        let total = tick_start.elapsed().as_secs_f32() * 1000.0;
        self.timings = PlayTimings {
            components_ms: (total - physics_ms - audio_ms).max(0.0),
            physics_ms,
            audio_ms,
        };
        host
    }

    /// Run `start` on every active object that hasn't started yet (repeats so
    /// objects spawned by a `start` also start this frame).
    fn start_pending(
        &mut self,
        w: &mut World,
        inp: &TickInput,
        dt: f32,
        unscaled: f32,
        events: &[AnyBox],
        host: &mut Vec<ComponentCommand>,
    ) {
        for _ in 0..8 {
            let active = w.scene.active_flags();
            let pending: HashSet<usize> = (0..w.scene.objects.len())
                .filter(|&i| active[i] && !self.started.contains(&w.scene.objects[i].id))
                .collect();
            if pending.is_empty() {
                break;
            }
            let ids: HashSet<ObjectId> = pending.iter().map(|&i| w.scene.objects[i].id).collect();
            self.started.extend(ids.iter().copied());
            if let Some(audio) = self.audio.as_mut() {
                for cue in crate::audio::gather_cues(w.scene) {
                    if cue.play_on_start && ids.contains(&cue.object) {
                        audio.start_cue(&cue, self.listener, &self.project_root, &self.mixer);
                    }
                }
            }
            self.pass(w, inp, dt, unscaled, events, host, |i| pending.contains(&i), |_, c, ctx| {
                c.start(ctx)
            });
        }
    }

    /// Feed Animator parameter drivers (movement speed, grounded, another
    /// Animator's parameter).
    fn drive_animator_params(&mut self, scene: &mut LoadedScene, dt: f32) {
        use zestengine_core::{DriverSource, PhysicsQuery as _};
        let ids = scene.id_index();
        let mut writes: Vec<(usize, String, f32)> = Vec::new();
        let mut seen: Vec<(ObjectId, Vec3, Vec3)> = Vec::new();
        for (i, o) in scene.objects.iter().enumerate() {
            let Some(a) = o.components.iter().find_map(|c| c.as_any().downcast_ref::<zestengine_core::Animator>()) else {
                continue;
            };
            for d in &a.drivers {
                if d.parameter.is_empty() {
                    continue;
                }
                let src_id = d.object.id().unwrap_or(o.id);
                let Some(&src) = ids.get(&src_id) else {
                    continue;
                };
                let value = match d.source {
                    DriverSource::AnimatorParameter => scene.objects[src]
                        .components
                        .iter()
                        .find_map(|c| c.as_any().downcast_ref::<zestengine_core::Animator>())
                        .map(|other| other.get_float(&d.source_parameter)),
                    DriverSource::Grounded => Some(if self.physics.grounded(src_id) { 1.0 } else { 0.0 }),
                    _ => {
                        let world = scene.world_transform(src);
                        let pos = world.w_axis.truncate();
                        let (last, prev_vel) = self.driver_prev.get(&src_id).copied().unwrap_or((pos, Vec3::ZERO));
                        // Raw finite difference of the scene position. Kinematic
                        // bodies (the character) update their position once per
                        // fixed step, so under vsync the estimate beats between
                        // 0x and 2x the true speed; smooth it so the Speed /
                        // ForwardSpeed / Strafe drivers stay steady.
                        let vel = if dt > 1e-6 {
                            let raw = (pos - last) / dt;
                            let a = 1.0 - (-dt / 0.08).exp();
                            prev_vel.lerp(raw, a)
                        } else {
                            prev_vel
                        };
                        seen.push((src_id, pos, vel));
                        let fwd = world.transform_vector3(Vec3::NEG_Z).normalize_or_zero();
                        let right = world.transform_vector3(Vec3::X).normalize_or_zero();
                        Some(match d.source {
                            DriverSource::Speed => Vec3::new(vel.x, 0.0, vel.z).length(),
                            DriverSource::VerticalSpeed => vel.y,
                            DriverSource::ForwardSpeed => vel.dot(fwd),
                            _ => vel.dot(right),
                        })
                    }
                };
                if let Some(v) = value {
                    writes.push((i, d.parameter.clone(), v * d.scale));
                }
            }
        }
        if dt > 1e-6 {
            for (id, pos, vel) in seen {
                self.driver_prev.insert(id, (pos, vel));
            }
        }
        for (i, name, v) in writes {
            if let Some(a) = scene.objects[i]
                .components
                .iter_mut()
                .find_map(|c| c.as_any_mut().downcast_mut::<zestengine_core::Animator>())
            {
                a.set_float(&name, v);
            }
        }
    }

    /// Evaluate Animators on objects without a skinned mesh: controllers and
    /// `.anim` clips animate the object and its descendants' transforms.
    fn animate_objects(&mut self, scene: &mut LoadedScene, dt: f32) {
        let targets: Vec<usize> = (0..scene.objects.len())
            .filter(|&i| {
                scene.objects[i].components.iter().any(|c| c.as_any().is::<zestengine_core::Animator>())
                    && scene.is_active(i)
                    && !scene.has_skinned_mesh(i)
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        scene.anim_assets.set_root(&self.project_root);
        let ids = scene.id_index();
        for i in targets {
            let id = scene.objects[i].id;
            let valid = self
                .object_rigs
                .get(&id)
                .is_some_and(|(_, nodes)| nodes.iter().all(|n| ids.contains_key(n)));
            if !valid {
                let (rig, nodes) = crate::anim_graph::object_rig(scene, i);
                let nodes = nodes.iter().map(|&n| scene.objects[n].id).collect();
                self.object_rigs.insert(id, (rig, nodes));
            }
            let (rig, node_ids) = &self.object_rigs[&id];
            let nodes: Vec<usize> = node_ids.iter().filter_map(|n| ids.get(n).copied()).collect();
            let LoadedScene { objects, anim_assets, .. } = &mut *scene;
            let Some(animator) = objects[i]
                .components
                .iter_mut()
                .find_map(|c| c.as_any_mut().downcast_mut::<zestengine_core::Animator>())
            else {
                continue;
            };
            if animator.controller.is_empty() && animator.clip.is_empty() && animator.state.current.is_none() {
                continue;
            }
            let ctrl = anim_assets.prepare(animator);
            let set = crate::anim_graph::ClipSet { imported: &[], indices: &[], assets: &anim_assets.clips };
            let out = crate::anim_graph::advance(animator, ctrl.as_deref(), rig, &set, dt);
            crate::anim_graph::apply_pose_to_objects(scene, &nodes, &out.pose);
        }
    }

    /// Root motion and animation events produced by the last skinning update.
    fn apply_animation_output(
        &mut self,
        w: &mut World,
        inp: &TickInput,
        dt: f32,
        unscaled: f32,
        events: &[AnyBox],
        host: &mut Vec<ComponentCommand>,
    ) {
        let mut fired: HashMap<usize, Vec<String>> = HashMap::new();
        let mut motion: Vec<(usize, Vec3)> = Vec::new();
        for (i, o) in w.scene.objects.iter_mut().enumerate() {
            let Some(a) = o
                .components
                .iter_mut()
                .find_map(|c| c.as_any_mut().downcast_mut::<zestengine_core::Animator>())
            else {
                continue;
            };
            if !a.state.fired.is_empty() {
                fired.insert(i, std::mem::take(&mut a.state.fired));
            }
            if a.state.root_delta != Vec3::ZERO {
                motion.push((i, std::mem::take(&mut a.state.root_delta)));
            }
            for (layer, from, to) in std::mem::take(&mut a.state.state_changes) {
                self.events_next.push(AnyBox(Box::new(zestengine_core::AnimatorStateChanged { object: o.id, layer, from, to })));
            }
        }
        for (i, delta) in motion {
            let has_cc = w.scene.objects[i]
                .components
                .iter()
                .any(|c| c.as_any().is::<zestengine_core::CharacterController>());
            if has_cc {
                self.physics.sync_structure(w.scene);
                self.physics.move_character(w.scene, i, delta);
            } else {
                let world = w.scene.world_transform(i).w_axis.truncate() + delta;
                let local = match w.scene.objects[i].parent {
                    Some(p) => w.scene.world_transform(p).inverse().transform_point3(world),
                    None => world,
                };
                w.scene.objects[i].translation = local;
            }
        }
        if fired.is_empty() {
            return;
        }
        for (&i, names) in &fired {
            let object = w.scene.objects[i].id;
            for name in names {
                self.events_next.push(AnyBox(Box::new(zestengine_core::AnimationEvent {
                    object,
                    name: name.clone(),
                })));
            }
        }
        self.pass(w, inp, dt, unscaled, events, host, |i| fired.contains_key(&i), |i, c, ctx| {
            for name in &fired[&i] {
                c.on_animation_event(ctx, name);
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver_contacts(
        &mut self,
        w: &mut World,
        inp: &TickInput,
        dt: f32,
        unscaled: f32,
        events: &[AnyBox],
        host: &mut Vec<ComponentCommand>,
        contacts: &[ContactEvent],
    ) {
        let ids = w.scene.id_index();
        let mut per_object: HashMap<usize, Vec<(Contact, ObjectId)>> = HashMap::new();
        for e in contacts {
            let kind = match (e.trigger, e.started) {
                (false, true) => Contact::CollisionEnter,
                (false, false) => Contact::CollisionExit,
                (true, true) => Contact::TriggerEnter,
                (true, false) => Contact::TriggerExit,
            };
            for (me, other) in [(e.a, e.b), (e.b, e.a)] {
                if let Some(&i) = ids.get(&me) {
                    per_object.entry(i).or_default().push((kind, other));
                }
            }
        }
        self.pass(
            w,
            inp,
            dt,
            unscaled,
            events,
            host,
            |i| per_object.contains_key(&i),
            |i, c, ctx| {
                for &(kind, other) in &per_object[&i] {
                    match kind {
                        Contact::CollisionEnter => c.on_collision_enter(ctx, other),
                        Contact::CollisionExit => c.on_collision_exit(ctx, other),
                        Contact::TriggerEnter => c.on_trigger_enter(ctx, other),
                        Contact::TriggerExit => c.on_trigger_exit(ctx, other),
                    }
                }
            },
        );
    }

    /// One component pass followed by applying the commands it issued.
    #[allow(clippy::too_many_arguments)]
    fn pass(
        &mut self,
        w: &mut World,
        inp: &TickInput,
        dt: f32,
        unscaled: f32,
        events: &[AnyBox],
        host: &mut Vec<ComponentCommand>,
        filter: impl Fn(usize) -> bool,
        call: impl FnMut(usize, &mut Box<dyn Component>, &mut ComponentCtx),
    ) {
        let mut commands = Vec::new();
        let env = PassEnv {
            dt,
            unscaled_dt: unscaled,
            time: self.time,
            water_time: self.water_time(),
            time_scale: self.time_scale,
            input: inp.input,
            net: inp.net,
            physics: Some(&self.physics),
            nav: (!self.nav.needs_bake()).then_some(&self.nav as &dyn zestengine_core::NavQuery),
            events,
            screen: inp.screen,
            save: &self.save,
            strings: &self.strings,
        };
        w.scene.run_pass(&env, &mut commands, filter, call);
        self.apply(w, commands, host);
    }

    fn apply(&mut self, w: &mut World, commands: Vec<ComponentCommand>, host: &mut Vec<ComponentCommand>) {
        for c in commands {
            match c {
                ComponentCommand::SetActiveCamera(id) => w.scene.set_active_camera(id),
                ComponentCommand::SetLocalTransform {
                    id,
                    translation,
                    rotation,
                    scale,
                } => w.scene.set_local_transform(id, translation, rotation, scale),
                ComponentCommand::SpawnPrefab {
                    path,
                    root_id,
                    translation,
                    rotation,
                } => self.spawn_prefab(w, &path, root_id, translation, rotation),
                ComponentCommand::Instantiate {
                    original,
                    new_id,
                    translation,
                    rotation,
                } => {
                    if let Some(i) = w.scene.index_of(original)
                        && let Some(n) = w.scene.duplicate_object(i, w.registry)
                    {
                        let o = &mut w.scene.objects[n];
                        o.id = new_id;
                        o.parent = None;
                        o.translation = translation;
                        o.rotation = rotation;
                        self.physics.mark_dirty();
                    }
                }
                ComponentCommand::Destroy(id) => {
                    if let Some(i) = w.scene.index_of(id) {
                        w.scene.remove_object(i);
                        self.physics.mark_dirty();
                    }
                }
                ComponentCommand::SetActive(id, on) => {
                    if let Some(i) = w.scene.index_of(id)
                        && w.scene.objects[i].enabled != on
                    {
                        w.scene.objects[i].enabled = on;
                        self.physics.mark_dirty();
                    }
                }
                ComponentCommand::Modify { id, type_id, f } => {
                    if let Some(i) = w.scene.index_of(id)
                        && let Some(c) = w.scene.objects[i]
                            .components
                            .iter_mut()
                            .find(|c| c.as_any().type_id() == type_id)
                    {
                        (f.0)(c.as_any_mut());
                    }
                }
                ComponentCommand::Emit(e) => self.events_next.push(e),
                ComponentCommand::AddForce { id, force } => self.physics.add_force(id, force),
                ComponentCommand::AddImpulse { id, impulse } => self.physics.add_impulse(id, impulse),
                ComponentCommand::AddTorque { id, torque } => self.physics.add_torque(id, torque),
                ComponentCommand::SetVelocity { id, linear } => self.physics.set_velocity(id, linear),
                ComponentCommand::RebakeNavMesh => self.nav.invalidate(),
                ComponentCommand::SetJointMotor { id, speed, max_force } => {
                    self.physics.set_joint_motor(id, speed, max_force)
                }
                ComponentCommand::MoveCharacter { id, delta } => {
                    self.physics.sync_structure(w.scene);
                    if let Some(i) = w.scene.index_of(id) {
                        self.physics.move_character(w.scene, i, delta);
                    }
                }
                ComponentCommand::PlaySound { clip, position, volume } => {
                    if let Some(audio) = self.audio.as_mut() {
                        audio.play_oneshot(&self.project_root, &clip, position, volume, self.listener, &self.mixer);
                    }
                }
                ComponentCommand::PlayAudio(id) => {
                    if let Some(audio) = self.audio.as_mut()
                        && let Some(cue) = crate::audio::gather_cues(w.scene).into_iter().find(|c| c.object == id)
                    {
                        audio.start_cue(&cue, self.listener, &self.project_root, &self.mixer);
                    }
                }
                ComponentCommand::StopAudio(id) => {
                    if let Some(audio) = self.audio.as_mut() {
                        audio.stop_object(id);
                    }
                }
                ComponentCommand::SetTimeScale(s) => self.time_scale = s.max(0.0),
                ComponentCommand::SaveSet { key, value } => self.save.set(key, value),
                ComponentCommand::SaveRemove(key) => {
                    self.save.remove(&key);
                }
                ComponentCommand::SaveWrite(slot) => {
                    let path = self.slot_path(&slot);
                    let result = std::fs::create_dir_all(&self.save_dir)
                        .map_err(|e| e.to_string())
                        .and_then(|_| self.save.save(&path).map_err(|e| e.to_string()));
                    if let Err(e) = result {
                        tracing::error!("write_save {}: {e}", path.display());
                    }
                }
                ComponentCommand::SaveLoad(slot) => {
                    let path = self.slot_path(&slot);
                    match zestengine_core::SaveGame::load(&path) {
                        Ok(save) => self.save = save,
                        Err(e) => tracing::warn!("load_save {}: {e}", path.display()),
                    }
                }
                ComponentCommand::SetLanguage(lang) => {
                    if !self.strings.set_language(&lang) {
                        tracing::warn!("set_language: no {lang:?} column in localization tables");
                    }
                }
                ComponentCommand::SetBusVolume { bus, volume } => self.mixer.set_volume(&bus, volume),
                ComponentCommand::SetBusMuted { bus, muted } => self.mixer.set_muted(&bus, muted),
                c @ (ComponentCommand::NetSpawn { .. } | ComponentCommand::NetDespawn(_) | ComponentCommand::NetSetVar { .. }) => {
                    self.apply_net_command(w, c, host)
                }
                other => host.push(other),
            }
        }
    }

    fn spawn_prefab(&mut self, w: &mut World, path: &str, root_id: ObjectId, translation: Vec3, rotation: Quat) {
        if !self.prefabs.contains_key(path) {
            match Prefab::load(zestengine_core::asset_ref::resolve_path(&self.project_root, path)) {
                Ok(p) => {
                    self.prefabs.insert(path.to_owned(), p);
                }
                Err(e) => {
                    tracing::error!("spawn_prefab {path:?}: {e:#}");
                    return;
                }
            }
        }
        let prefab = &self.prefabs[path];
        let mut entries = prefab.instantiate_rooted(translation.to_array(), Some(root_id));
        for e in entries.iter_mut().filter(|e| e.parent.is_none()) {
            e.rotation = rotation.to_array();
        }
        let Some(renderer) = w.renderer.as_deref_mut() else {
            tracing::error!("spawn_prefab {path:?}: no renderer");
            return;
        };
        match w
            .scene
            .append_entries(renderer, w.shaders, &self.project_root, w.registry, &entries)
        {
            Ok(_) => self.physics.mark_dirty(),
            Err(e) => tracing::error!("spawn_prefab {path:?}: {e:#}"),
        }
    }
}

/// Merge every `localization/*.loc` table (same language columns) into one.
fn load_strings(project_root: &Path) -> zestengine_core::Localization {
    let mut out = zestengine_core::Localization::default();
    let Ok(dir) = std::fs::read_dir(project_root.join("localization")) else {
        return out;
    };
    let mut files: Vec<PathBuf> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "loc"))
        .collect();
    files.sort();
    for path in files {
        match zestengine_core::Localization::load(&path) {
            Ok(table) if out.languages.is_empty() => out = table,
            Ok(table) if table.languages == out.languages => out.entries.extend(table.entries),
            Ok(_) => tracing::warn!("{}: language columns differ from the first table; skipped", path.display()),
            Err(e) => tracing::warn!("{}: {e}", path.display()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::SceneObject;
    use zestengine_assets::ObjectSource;
    use zestengine_core::{BoxCollider, ObjectRef, RigidBody, SphereCollider, TypedComponent};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct Health {
        hp: f32,
    }
    impl Default for Health {
        fn default() -> Self {
            Self { hp: 100.0 }
        }
    }
    impl TypedComponent for Health {
        const NAME: &'static str = "Test Health";
    }

    struct Ping;

    #[derive(Serialize, Deserialize, Default)]
    struct Shooter {
        target: ObjectRef,
        seen_hp: Vec<f32>,
        starts: u32,
        events_seen: Vec<usize>,
    }
    impl TypedComponent for Shooter {
        const NAME: &'static str = "Test Shooter";
        fn start(&mut self, _ctx: &mut ComponentCtx) {
            self.starts += 1;
        }
        fn update(&mut self, ctx: &mut ComponentCtx) {
            if let Some(h) = ctx.get::<Health>(self.target) {
                self.seen_hp.push(h.hp);
            }
            ctx.modify::<Health>(self.target, |h| h.hp -= 10.0);
            self.events_seen.push(ctx.events::<Ping>().count());
            ctx.emit(Ping);
        }
    }

    #[derive(Serialize, Deserialize, Default)]
    struct Counter {
        starts: u32,
        updates: u32,
        collisions: u32,
        triggers: u32,
    }
    impl TypedComponent for Counter {
        const NAME: &'static str = "Test Counter";
        fn start(&mut self, _ctx: &mut ComponentCtx) {
            self.starts += 1;
        }
        fn update(&mut self, _ctx: &mut ComponentCtx) {
            self.updates += 1;
        }
        fn on_collision_enter(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {
            self.collisions += 1;
        }
        fn on_trigger_enter(&mut self, _ctx: &mut ComponentCtx, _other: ObjectId) {
            self.triggers += 1;
        }
    }

    #[derive(Serialize, Deserialize, Default)]
    struct Spawner {
        template: ObjectRef,
        victim: ObjectRef,
        spawned: Option<ObjectId>,
    }
    impl TypedComponent for Spawner {
        const NAME: &'static str = "Test Spawner";
        fn update(&mut self, ctx: &mut ComponentCtx) {
            if self.spawned.is_none() {
                let id = ctx.instantiate(self.template, Vec3::new(4.0, 0.0, 0.0), Quat::IDENTITY);
                if let Some(id) = id {
                    ctx.set_active(id, true);
                }
                self.spawned = id;
                ctx.destroy(self.victim);
            }
        }
    }

    fn registry() -> ComponentRegistry {
        let mut r = ComponentRegistry::with_builtins();
        r.register::<Health>();
        r.register::<Shooter>();
        r.register::<Counter>();
        r.register::<Spawner>();
        r
    }

    fn obj(scene: &mut LoadedScene, pos: Vec3, enabled: bool, components: Vec<Box<dyn Component>>) -> ObjectId {
        let id = ObjectId::new();
        scene.objects.push(SceneObject {
            id,
            name: String::new(),
            render: None,
            extra_render: Vec::new(),
            source: ObjectSource::Empty,
            enabled,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: zestengine_core::ShadowCasting::On,
            layer: 0,
            parent: None,
            translation: pos,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components,
        });
        id
    }

    fn get<T: Component>(scene: &LoadedScene, id: ObjectId) -> &T {
        let i = scene.index_of(id).expect("object exists");
        scene.objects[i]
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<T>())
            .expect("component exists")
    }

    fn tick(session: &mut PlaySession, scene: &mut LoadedScene, registry: &ComponentRegistry, frames: usize) {
        let mut shaders = ShaderLibrary::default();
        let input = InputState::default();
        let net = NetView::default();
        let screen = zestengine_core::ScreenInfo::default();
        for _ in 0..frames {
            let host = session.tick(
                &mut World {
                    scene,
                    renderer: None,
                    registry,
                    shaders: &mut shaders,
                },
                &TickInput {
                    dt: 1.0 / 60.0,
                    input: &input,
                    net: &net,
                    listener_fallback: Mat4::IDENTITY,
                    screen: &screen,
                },
            );
            assert!(host.is_empty(), "unexpected host commands {host:?}");
        }
    }

    /// One frame with the given UI input; returns the host commands.
    fn tick_ui(
        session: &mut PlaySession,
        scene: &mut LoadedScene,
        registry: &ComponentRegistry,
        clicks: Vec<ObjectId>,
        cancel: bool,
    ) -> Vec<ComponentCommand> {
        let mut shaders = ShaderLibrary::default();
        let input = InputState::default();
        let net = NetView::default();
        let screen = zestengine_core::ScreenInfo {
            ui_clicks: clicks,
            ui_cancel: cancel,
            ..Default::default()
        };
        session.tick(
            &mut World { scene, renderer: None, registry, shaders: &mut shaders },
            &TickInput { dt: 1.0 / 60.0, input: &input, net: &net, listener_fallback: Mat4::IDENTITY, screen: &screen },
        )
    }

    #[test]
    fn ui_button_actions_and_pause_menu() {
        use zestengine_core::{UiAction, UiButton, UiPauseMenu};
        let registry = registry();
        let mut scene = LoadedScene::empty();
        let settings = obj(&mut scene, Vec3::ZERO, false, vec![]);
        let panel = obj(&mut scene, Vec3::ZERO, false, vec![]);
        obj(&mut scene, Vec3::ZERO, true, vec![Box::new(UiPauseMenu::default())]);
        let button = obj(
            &mut scene,
            Vec3::ZERO,
            true,
            vec![Box::new(UiButton {
                on_click: vec![UiAction::Show("Settings".into())],
                ..Default::default()
            })],
        );
        scene.objects[0].name = "Settings".into();
        scene.objects[1].name = "Pause Panel".into();
        let enabled = |scene: &LoadedScene, id: ObjectId| scene.objects[scene.index_of(id).unwrap()].enabled;
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        tick_ui(&mut session, &mut scene, &registry, vec![], false);

        tick_ui(&mut session, &mut scene, &registry, vec![button], false);
        assert!(enabled(&scene, settings), "Show action ran on click");

        tick_ui(&mut session, &mut scene, &registry, vec![], true);
        assert!(enabled(&scene, panel), "cancel opened the pause panel");
        let host = tick_ui(&mut session, &mut scene, &registry, vec![], false);
        assert_eq!(session.time_scale, 0.0, "open pause menu stops time");
        assert!(host.iter().any(|c| matches!(c, ComponentCommand::SetCursorLocked(false))));

        tick_ui(&mut session, &mut scene, &registry, vec![], true);
        tick_ui(&mut session, &mut scene, &registry, vec![], false);
        assert!(!enabled(&scene, panel));
        assert_eq!(session.time_scale, 1.0, "closing resumes");
    }

    /// One frame with live bindings: the runtime sees `raw`, the session
    /// ticks with its view, and binding requests go back to the runtime.
    fn tick_controls(
        session: &mut PlaySession,
        scene: &mut LoadedScene,
        registry: &ComponentRegistry,
        rt: &mut zestengine_core::ControlsRuntime,
        raw: &zestengine_core::RawInput,
        clicks: Vec<ObjectId>,
    ) {
        rt.frame(raw, false);
        let mut shaders = ShaderLibrary::default();
        let input = InputState::default();
        let net = NetView::default();
        let screen = zestengine_core::ScreenInfo { ui_clicks: clicks, controls: rt.info(), ..Default::default() };
        let host = session.tick(
            &mut World { scene, renderer: None, registry, shaders: &mut shaders },
            &TickInput { dt: 1.0 / 60.0, input: &input, net: &net, listener_fallback: Mat4::IDENTITY, screen: &screen },
        );
        for c in host {
            if let ComponentCommand::Input(c) = c {
                rt.apply(c).unwrap();
            }
        }
    }

    #[test]
    fn controls_menu_rebinds_through_capture() {
        use zestengine_core::{InputSource, Key, UiText};
        let registry = registry();
        let mut scene = LoadedScene::empty();
        for node in crate::ui_templates::UiTemplate::Controls.build() {
            obj(&mut scene, Vec3::ZERO, node.active, node.components);
            let o = scene.objects.last_mut().unwrap();
            o.name = node.name;
            o.parent = node.parent;
        }
        let id = |scene: &LoadedScene, n: &str| scene.objects.iter().find(|o| o.name == n).unwrap().id;
        let text = |scene: &LoadedScene, n: &str| {
            let o = scene.objects.iter().find(|o| o.name == n).unwrap();
            o.components.iter().find_map(|c| c.as_any().downcast_ref::<UiText>()).unwrap().text.clone()
        };
        let mut rt = zestengine_core::ControlsRuntime::new(zestengine_core::Bindings::default());
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        let none = zestengine_core::RawInput::default();
        tick_controls(&mut session, &mut scene, &registry, &mut rt, &none, vec![]);
        assert_eq!(text(&scene, "Controls Scheme"), "KB+Mouse (in use)");
        assert_eq!(text(&scene, "Controls Row 1 Label"), "Move Up");
        assert_eq!(text(&scene, "Controls Row 6 Label"), "Jump");
        assert_eq!(text(&scene, "Controls Row 6 Bind 1"), "Space");

        let jump = id(&scene, "Controls Row 6 Bind 1");
        tick_controls(&mut session, &mut scene, &registry, &mut rt, &none, vec![jump]);
        assert_eq!(text(&scene, "Controls Row 6 Bind 1"), "...");
        tick_controls(&mut session, &mut scene, &registry, &mut rt, &none, vec![]);
        let mut raw = zestengine_core::RawInput::default();
        raw.keys_down.insert(Key::J);
        tick_controls(&mut session, &mut scene, &registry, &mut rt, &raw, vec![]);
        tick_controls(&mut session, &mut scene, &registry, &mut rt, &none, vec![]);
        let kb = &rt.bindings().schemes[0];
        assert_eq!(kb.action("Jump").unwrap().buttons, vec![InputSource::Key(Key::J)], "rebound by pressing J");
        assert_eq!(text(&scene, "Controls Row 6 Bind 1"), "J");
        assert_eq!(rt.overrides().actions.len(), 1, "saved as a player override");

        // Clicks wait out the capture's mouse release first.
        for _ in 0..20 {
            tick_controls(&mut session, &mut scene, &registry, &mut rt, &none, vec![]);
        }
        let reset = id(&scene, "Controls Reset");
        tick_controls(&mut session, &mut scene, &registry, &mut rt, &none, vec![reset]);
        tick_controls(&mut session, &mut scene, &registry, &mut rt, &none, vec![]);
        assert_eq!(text(&scene, "Controls Row 6 Bind 1"), "Space", "reset to the game's defaults");
    }

    #[test]
    fn get_modify_and_events_between_components() {
        let registry = registry();
        let mut scene = LoadedScene::empty();
        let target = obj(&mut scene, Vec3::ZERO, true, vec![Box::new(Health::default())]);
        let shooter = obj(
            &mut scene,
            Vec3::ZERO,
            true,
            vec![Box::new(Shooter {
                target: target.into(),
                ..Default::default()
            })],
        );
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        tick(&mut session, &mut scene, &registry, 3);
        let s = get::<Shooter>(&scene, shooter);
        assert_eq!(s.starts, 1);
        assert_eq!(s.seen_hp, vec![100.0, 90.0, 80.0]);
        assert_eq!(s.events_seen, vec![0, 1, 1]);
        assert_eq!(get::<Health>(&scene, target).hp, 70.0);
    }

    #[test]
    fn instantiate_destroy_and_activation() {
        let registry = registry();
        let mut scene = LoadedScene::empty();
        let template = obj(&mut scene, Vec3::ZERO, false, vec![Box::new(Counter::default())]);
        let victim = obj(&mut scene, Vec3::ZERO, true, vec![Box::new(Counter::default())]);
        let spawner = obj(
            &mut scene,
            Vec3::ZERO,
            true,
            vec![Box::new(Spawner {
                template: template.into(),
                victim: victim.into(),
                spawned: None,
            })],
        );
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        tick(&mut session, &mut scene, &registry, 3);
        assert!(scene.index_of(victim).is_none(), "victim destroyed");
        let clone = get::<Spawner>(&scene, spawner).spawned.expect("spawned");
        let c = get::<Counter>(&scene, clone);
        assert_eq!(c.starts, 1, "clone started once when activated");
        assert_eq!(c.updates, 2, "clone updates from the frame after it was spawned");
        let t = get::<Counter>(&scene, template);
        assert_eq!((t.starts, t.updates), (0, 0), "disabled template never runs");
        let i = scene.index_of(clone).unwrap();
        assert_eq!(scene.objects[i].translation, Vec3::new(4.0, 0.0, 0.0));
    }

    #[test]
    fn collision_and_trigger_callbacks() {
        let registry = registry();
        let mut scene = LoadedScene::empty();
        obj(
            &mut scene,
            Vec3::ZERO,
            true,
            vec![Box::new(BoxCollider {
                size: [20.0, 1.0, 20.0],
                ..Default::default()
            })],
        );
        let zone = obj(
            &mut scene,
            Vec3::new(0.0, 2.0, 0.0),
            true,
            vec![
                Box::new(BoxCollider {
                    size: [2.0, 1.0, 2.0],
                    is_trigger: true,
                    ..Default::default()
                }),
                Box::new(Counter::default()),
            ],
        );
        let ball = obj(
            &mut scene,
            Vec3::new(0.0, 4.0, 0.0),
            true,
            vec![
                Box::new(SphereCollider::default()),
                Box::new(RigidBody::default()),
                Box::new(Counter::default()),
            ],
        );
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        tick(&mut session, &mut scene, &registry, 180);
        let b = get::<Counter>(&scene, ball);
        assert_eq!(b.collisions, 1, "ball hit the floor once");
        assert_eq!(b.triggers, 1, "ball passed through the zone once");
        assert_eq!(get::<Counter>(&scene, zone).triggers, 1);
    }

    #[derive(Serialize, Deserialize, Default)]
    struct EventLog {
        names: Vec<String>,
    }
    impl TypedComponent for EventLog {
        const NAME: &'static str = "Test Event Log";
        fn on_animation_event(&mut self, _ctx: &mut ComponentCtx, name: &str) {
            self.names.push(name.to_owned());
        }
    }

    #[test]
    fn animation_events_and_root_motion_are_applied() {
        let mut registry = registry();
        registry.register::<EventLog>();
        let mut scene = LoadedScene::empty();
        let mut animator = zestengine_core::Animator::default();
        animator.state.fired = vec!["Footstep".into()];
        animator.state.root_delta = Vec3::new(0.0, 0.0, 1.5);
        let id = obj(&mut scene, Vec3::ZERO, true, vec![Box::new(animator), Box::new(EventLog::default())]);
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        tick(&mut session, &mut scene, &registry, 1);
        assert_eq!(get::<EventLog>(&scene, id).names, vec!["Footstep".to_string()]);
        assert_eq!(scene.objects[0].translation, Vec3::new(0.0, 0.0, 1.5));
        assert!(get::<zestengine_core::Animator>(&scene, id).state.fired.is_empty());
    }

    #[test]
    fn anim_clips_drive_child_transforms_and_params_follow_drivers() {
        use zestengine_assets::{AnimClip, AnimTrack, KeyInterp, Keyframe, TrackProperty};
        let registry = registry();
        let mut scene = LoadedScene::empty();
        let leader = obj(&mut scene, Vec3::ZERO, true, vec![Box::new(zestengine_core::Animator::default())]);
        let follower = zestengine_core::Animator {
            clip: "wave.anim".into(),
            drivers: vec![zestengine_core::ParamDriver {
                parameter: "Mood".into(),
                source: zestengine_core::DriverSource::AnimatorParameter,
                object: ObjectRef::new(leader),
                source_parameter: "Mood".into(),
                scale: 2.0,
            }],
            ..Default::default()
        };
        let root = obj(&mut scene, Vec3::ZERO, true, vec![Box::new(follower)]);
        obj(&mut scene, Vec3::ZERO, true, vec![]);
        scene.objects[2].name = "Arm".into();
        scene.objects[2].parent = Some(1);
        scene.anim_assets.set_clip(
            "wave.anim",
            AnimClip {
                length: 1.0,
                tracks: vec![AnimTrack {
                    target: "Arm".into(),
                    property: TrackProperty::Translation,
                    keys: vec![
                        Keyframe { time: 0.0, value: [0.0; 4], interp: KeyInterp::Linear, ..Default::default() },
                        Keyframe { time: 1.0, value: [0.0, 6.0, 0.0, 0.0], interp: KeyInterp::Linear, ..Default::default() },
                    ],
                }],
                ..Default::default()
            },
        );
        let i = scene.index_of(leader).unwrap();
        scene.objects[i]
            .components
            .iter_mut()
            .find_map(|c| c.as_any_mut().downcast_mut::<zestengine_core::Animator>())
            .unwrap()
            .set_float("Mood", 1.5);
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        tick(&mut session, &mut scene, &registry, 30);
        let y = scene.objects[2].translation.y;
        assert!((y - 3.0).abs() < 0.05, "arm moved by the clip: {y}");
        assert_eq!(scene.objects[1].translation, Vec3::ZERO, "root not animated by the clip");
        assert_eq!(get::<zestengine_core::Animator>(&scene, root).get_float("Mood"), 3.0);
    }

    #[derive(Serialize, Deserialize, Default)]
    struct Saver {
        frame: u32,
        loaded_score: Option<i64>,
        greeting: String,
    }
    impl TypedComponent for Saver {
        const NAME: &'static str = "Test Saver";
        fn update(&mut self, ctx: &mut ComponentCtx) {
            match self.frame {
                0 => {
                    ctx.save_value("score", 42i64);
                    ctx.write_save("slot1");
                }
                1 => {
                    ctx.save_value("score", 7i64);
                    ctx.load_save("slot1");
                }
                _ => {
                    self.loaded_score = ctx.save_data().int("score");
                    self.greeting = ctx.tr("hello").to_owned();
                }
            }
            self.frame += 1;
        }
    }

    #[test]
    fn save_slots_and_localization() {
        let mut registry = registry();
        registry.register::<Saver>();
        let dir = std::env::temp_dir().join(format!("zestengine-save-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("localization")).unwrap();
        std::fs::write(
            dir.join("localization/strings.loc"),
            r#"Localization(languages: ["en", "fr"], entries: {"hello": ["Hello", "Bonjour"]})"#,
        )
        .unwrap();
        let mut scene = LoadedScene::empty();
        let id = obj(&mut scene, Vec3::ZERO, true, vec![Box::new(Saver::default())]);
        let mut session = PlaySession::new(&scene, &dir, None);
        session.strings.set_language("fr");
        tick(&mut session, &mut scene, &registry, 3);
        let s = get::<Saver>(&scene, id);
        assert_eq!(s.loaded_score, Some(42), "slot restored the written value");
        assert_eq!(s.greeting, "Bonjour");
        assert!(dir.join(".zestengine/saves/slot1.save").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scene_portal_preloads_then_switches_with_kept_activator() {
        let registry = registry();
        let mut scene = LoadedScene::empty();
        obj(
            &mut scene,
            Vec3::new(0.0, 0.0, 0.0),
            true,
            vec![
                Box::new(BoxCollider { size: [2.0, 2.0, 2.0], is_trigger: true, ..Default::default() }),
                Box::new({
                    let mut p = zestengine_core::ScenePortal::default();
                    p.scene = "scenes/level2.scene".into();
                    p.preload_distance = 10.0;
                    p
                }),
            ],
        );
        let player = obj(
            &mut scene,
            Vec3::new(0.0, 0.0, 20.0),
            true,
            vec![
                Box::new(SphereCollider::default()),
                Box::new(RigidBody { kind: zestengine_core::BodyKind::Kinematic, ..Default::default() }),
                Box::new({
                    let mut p = zestengine_core::Pawn::default();
                    p.possessed = false;
                    p
                }),
            ],
        );
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        let mut shaders = ShaderLibrary::default();
        let input = InputState::default();
        let net = NetView::default();
        let screen = zestengine_core::ScreenInfo::default();
        let mut run = |scene: &mut LoadedScene, session: &mut PlaySession| {
            session.tick(
                &mut World { scene, renderer: None, registry: &registry, shaders: &mut shaders },
                &TickInput { dt: 1.0 / 60.0, input: &input, net: &net, listener_fallback: Mat4::IDENTITY, screen: &screen },
            )
        };
        let host = run(&mut scene, &mut session);
        assert!(host.is_empty(), "too far to preload yet: {host:?}");
        scene.objects[1].translation = Vec3::new(0.0, 0.0, 8.0);
        let host = run(&mut scene, &mut session);
        assert!(matches!(host.as_slice(), [ComponentCommand::PreloadScene(p)] if p == "scenes/level2.scene"), "{host:?}");
        scene.objects[1].translation = Vec3::ZERO;
        let mut all = Vec::new();
        for _ in 0..3 {
            all.extend(run(&mut scene, &mut session));
        }
        assert!(all.iter().any(|c| matches!(c, ComponentCommand::KeepOnSceneChange(id, true) if *id == player)), "{all:?}");
        assert!(all.iter().any(|c| matches!(c, ComponentCommand::LoadScene(p) if p == "scenes/level2.scene")));
    }

    #[test]
    fn time_scale_zero_freezes_physics() {
        let registry = registry();
        let mut scene = LoadedScene::empty();
        obj(
            &mut scene,
            Vec3::new(0.0, 4.0, 0.0),
            true,
            vec![Box::new(SphereCollider::default()), Box::new(RigidBody::default())],
        );
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        session.time_scale = 0.0;
        tick(&mut session, &mut scene, &registry, 30);
        assert_eq!(scene.objects[0].translation.y, 4.0);
        session.time_scale = 1.0;
        tick(&mut session, &mut scene, &registry, 30);
        assert!(scene.objects[0].translation.y < 4.0);
    }
}
