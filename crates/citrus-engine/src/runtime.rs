//! Standalone game runtime: load a scene and run it in a window, no editor.
//!
//! This is what a bundled game's generated `main.rs` calls. It reuses the same
//! scene / renderer / component path as the editor's Play mode, minus every
//! egui panel. `FrameInput.egui` is `None`, so no editor code runs. Components
//! are linked in statically and registered through the `register` callback
//! (the runtime replacement for the editor's dylib hot-load).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use glam::{Mat4, Vec3};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorGrabMode, Window, WindowId};

use citrus_core::{ComponentCommand, ComponentRegistry};
use citrus_render::{CameraData, FrameInput, LightData, Renderer};

use crate::game_ui::{GameUi, UiFrame, UiInput, UiKeys, UiPointer, UiRay, UiScope, WorldUi};
use crate::input_engine::InputManager;
use crate::net::NetSession;
use crate::play::{PlaySession, TickInput, World};
use crate::scene::LoadedScene;
use crate::shaders::ShaderLibrary;

/// Entry configuration for a bundled game.
pub struct GameConfig {
    /// Folder holding the game's assets (`scenes/`, `materials/`, …). Scene-file
    /// asset paths resolve relative to this. In a bundle it's the exe-relative
    /// `assets/` directory.
    pub assets_root: PathBuf,
    /// Project-relative path of the first scene, e.g. `"scenes/world.scene"`.
    pub boot_scene: String,
    pub title: String,
    pub width: f64,
    pub height: f64,
    /// Input control schemes (2C). Loaded from `project.citrus` by
    /// [`GameConfig::from_project_dir`]; defaults to KB+Mouse / Gamepad.
    pub bindings: citrus_core::Bindings,
    /// Audio buses from `project.citrus`.
    pub audio_mixer: citrus_core::AudioMixer,
    pub spatial_audio: citrus_core::SpatialAudioSettings,
}

impl Default for GameConfig {
    fn default() -> Self {
        Self {
            assets_root: PathBuf::from("assets"),
            boot_scene: String::new(),
            title: "citrus game".into(),
            width: 1280.0,
            height: 720.0,
            bindings: citrus_core::Bindings::default(),
            audio_mixer: citrus_core::AudioMixer::default(),
            spatial_audio: citrus_core::SpatialAudioSettings::default(),
        }
    }
}

impl GameConfig {
    /// Build a config from a project directory's `project.citrus`: the boot
    /// scene and window title come from the project settings, assets resolve
    /// relative to `assets_root`. This keeps a generated `main.rs` tiny and lets
    /// the Project Settings boot-scene choice take effect without editing code.
    pub fn from_project_dir(assets_root: impl Into<PathBuf>) -> Result<Self> {
        let assets_root = assets_root.into();
        let project = citrus_assets::load_project_file(&assets_root)
            .context("reading project.citrus")?;
        let boot_scene = project
            .boot_scene
            .or(project.last_scene)
            .context("project has no boot_scene set (Project Settings -> Starting scene)")?;
        Ok(Self {
            assets_root,
            boot_scene,
            title: project.name,
            bindings: project.bindings,
            audio_mixer: project.audio_mixer,
            spatial_audio: project.spatial_audio,
            ..Default::default()
        })
    }
}

/// Run a game to completion: open a window, load the boot scene, and run the
/// component loop until the window closes. `register` adds the game's component
/// types (the built-ins are already present); it's the statically-linked stand-in
/// for the editor's plugin dylib load.
pub fn run_game(config: GameConfig, register: impl FnOnce(&mut ComponentRegistry)) -> Result<()> {
    let mut registry = ComponentRegistry::with_builtins();
    register(&mut registry);
    let config_bindings = config.bindings.clone();

    let event_loop = EventLoop::new().context("creating event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = GameApp {
        config,
        registry,
        shaders: ShaderLibrary::default(),
        scene: LoadedScene::empty(),
        window: None,
        renderer: None,
        play: None,
        cursor_locked: false,
        last_cam_world: Mat4::IDENTITY,
        ui_ctx: egui::Context::default(),
        game_ui: GameUi::default(),
        ui_frame: UiFrame::default(),
        world_ui: WorldUi::default(),
        flow: crate::scene_loader::SceneFlow::default(),
        post_load: None,
        particles: crate::particles::ParticleRenderer::default(),
        ui_keys: UiKeys::default(),
        vr_trigger_prev: false,
        world_ui_draws: Vec::new(),
        pending_world_frame: UiFrame::default(),
        pointer: None,
        mouse: UiPointer::default(),
        rt_gi: crate::realtime_gi::RealtimeGiState::default(),
        input: InputManager::new(config_bindings),
        // Optional networking via env vars: CITRUS_HOST=<port> or CITRUS_JOIN=<addr>.
        net: start_net_from_env(),
        voice: None,
        start: Instant::now(),
        last_frame: Instant::now(),
        init_error: None,
        xr: None,
        xr_session: None,
        vr_targets: citrus_core::TrackerTargets::default(),
    };
    event_loop.run_app(&mut app)?;
    if let Some(e) = app.init_error.take() {
        return Err(e);
    }
    Ok(())
}

struct GameApp {
    config: GameConfig,
    registry: ComponentRegistry,
    shaders: ShaderLibrary,
    scene: LoadedScene,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    /// The running game (components, physics, audio). Created with the first
    /// scene and reset on every scene switch.
    play: Option<PlaySession>,
    /// Gameplay asked for a locked cursor (re-applied when focus returns).
    cursor_locked: bool,
    /// Last frame's camera world transform: the audio listener fallback.
    last_cam_world: Mat4,
    /// egui context the game UI paints into (sized to the window, 1 point = 1 pixel).
    ui_ctx: egui::Context,
    game_ui: GameUi,
    /// UI clicks / hover since the last tick.
    ui_frame: UiFrame,
    world_ui: WorldUi,
    /// Background scene loads, carried objects, additive streaming.
    flow: crate::scene_loader::SceneFlow,
    post_load: Option<crate::scene_loader::PostLoad>,
    particles: crate::particles::ParticleRenderer,
    /// Text typed since the last UI frame.
    ui_keys: UiKeys,
    vr_trigger_prev: bool,
    world_ui_draws: Vec<citrus_render::WorldUiDraw>,
    /// World-canvas interaction, merged into `ui_frame` with the screen UI.
    pending_world_frame: UiFrame,
    /// Cursor position in window pixels.
    pointer: Option<glam::Vec2>,
    /// Left mouse button edges since the last UI frame.
    mouse: UiPointer,
    /// Realtime-GI driver. Runs the scene's realtime-GI setting in the game.
    rt_gi: crate::realtime_gi::RealtimeGiState,
    /// Input binding system (2C): keyboard/mouse/gamepad → action snapshot.
    input: InputManager,
    /// Active networking session (2G), if hosting/joined.
    net: Option<NetSession>,
    /// Voice comms (task 8), created lazily once networking is active.
    voice: Option<crate::voice::VoiceChat>,
    start: Instant,
    last_frame: Instant,
    /// Set if `init` failed; surfaced after the loop exits.
    init_error: Option<anyhow::Error>,
    /// OpenXR context (VR). `None` when no runtime/headset; the game runs flat.
    xr: Option<citrus_xr::XrContext>,
    /// Live VR session (poses/lifecycle), once the device is created.
    xr_session: Option<citrus_xr::XrSession>,
    /// Latest VR tracker poses (head/hands/hips/feet) for the full-body IK solver.
    /// Consumed by the avatar-IK application step (humanoid bone mapping) — wired.
    #[allow(dead_code)]
    vr_targets: citrus_core::TrackerTargets,
}

impl GameApp {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.width,
                self.config.height,
            ));
        let window = Arc::new(event_loop.create_window(attrs)?);
        let mut renderer = Renderer::new(window.clone())?;

        // The boot scene loads in the background; frames start once it's in.
        let boot = self.config.boot_scene.clone();
        self.load_scene(&renderer, &boot);

        self.renderer = Some(renderer);
        self.window = Some(window);
        // Start OpenXR (VR). Degrades gracefully to flat when unavailable.
        match citrus_xr::XrContext::start(&self.config.title) {
            Ok(Some(xr)) => {
                tracing::info!("VR ready: {}", xr.system_name());
                // Create the VR session sharing the renderer's Vulkan device.
                let (vi, pd, dev, qfi) =
                    self.renderer.as_ref().unwrap().vulkan_raw_handles();
                match xr.create_session(vi, pd, dev, qfi) {
                    Ok(session) => self.xr_session = Some(session),
                    Err(e) => tracing::warn!("OpenXR session create failed: {e:#}"),
                }
                self.xr = Some(xr);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("OpenXR start failed: {e:#}"),
        }
        self.start = Instant::now();
        self.last_frame = Instant::now();
        Ok(())
    }

    /// Start replacing the current scene with the one at `rel` (relative to the
    /// assets root). It loads in the background (instant when a preload
    /// finished) while the current scene keeps running; `finish_scene_switch`
    /// installs it.
    fn load_scene(&mut self, renderer: &Renderer, rel: &str) {
        self.flow
            .request_switch(rel, &self.config.assets_root, renderer, &self.scene, &self.shaders);
    }

    /// Install a finished scene switch: kept objects come along with their
    /// ids, and components start on the next tick. Returns false when the
    /// boot scene failed (the game can't run).
    fn finish_scene_switch(&mut self, renderer: &mut Renderer) -> bool {
        let Some(result) =
            self.flow
                .poll_switch(renderer, &self.registry, &mut self.shaders, &self.config.assets_root, &self.scene)
        else {
            return true;
        };
        let (_rel, scene, post) = match result {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("loading scene: {e:#}");
                if self.play.is_none() {
                    self.init_error = Some(e.context(format!("loading boot scene {:?}", self.config.boot_scene)));
                    return false;
                }
                return true;
            }
        };
        self.scene = scene;
        // The renderer was reset: drop caches holding its handles.
        self.world_ui = WorldUi::default();
        self.particles = crate::particles::ParticleRenderer::default();
        // Loaded scenes carry their own Camera/Light components, but older or
        // hand-written scenes might not, so mirror the editor's safety net.
        self.scene.ensure_camera_components(&self.registry);
        self.scene.ensure_light_components(&self.registry);
        self.scene.ensure_camera_ids();
        // Skybox + baked lighting apply over the next frames (`step_post_load`).
        self.post_load = Some(post);

        // A game is always playing; components start on the next tick.
        match self.play.as_mut() {
            Some(play) => play.scene_changed(&self.scene),
            None => {
                let mut session = PlaySession::new(&self.scene, &self.config.assets_root, crate::audio::AudioEngine::new());
                session.set_mixer(self.config.audio_mixer.clone());
                session.set_spatial_audio(&self.config.spatial_audio);
                session.set_save_dir(user_save_dir(&self.config.title));
                self.play = Some(session);
            }
        }
        true
    }

    /// Apply the component requests the play session leaves to the host
    /// (scene switch, window, renderer settings, networking, quit). The last
    /// `LoadScene` wins.
    fn apply_host_commands(
        &mut self,
        renderer: &mut Renderer,
        event_loop: &ActiveEventLoop,
        commands: Vec<ComponentCommand>,
    ) {
        let mut next_scene = None;
        for c in commands {
            match c {
                ComponentCommand::LoadScene(rel) => next_scene = Some(rel),
                ComponentCommand::PreloadScene(rel) => {
                    self.flow
                        .preload(&rel, &self.config.assets_root, renderer, &self.scene, &self.shaders)
                }
                ComponentCommand::LoadSceneAdditive(rel) => {
                    self.flow.load_additive(&rel, &self.config.assets_root, renderer)
                }
                ComponentCommand::UnloadScene(rel) => {
                    if self.flow.unload(&rel, &mut self.scene)
                        && let Some(play) = self.play.as_mut()
                    {
                        play.structure_changed();
                    }
                }
                ComponentCommand::KeepOnSceneChange(id, keep) => {
                    if keep {
                        self.flow.kept.insert(id);
                    } else {
                        self.flow.kept.remove(&id);
                    }
                }
                ComponentCommand::RequestOwnership(id) => {
                    if let Some(net) = self.net.as_mut() {
                        net.request_ownership(id);
                    }
                }
                ComponentCommand::ReleaseOwnership(id) => {
                    if let Some(net) = self.net.as_mut() {
                        net.release_ownership(id);
                    }
                }
                ComponentCommand::SetResolution(w, h) => {
                    if let Some(window) = self.window.as_ref() {
                        let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
                    }
                    renderer.resize();
                }
                ComponentCommand::SetVsync(on) => renderer.set_vsync(on),
                ComponentCommand::SetShadowResolution(res) => {
                    if let Err(e) = renderer.set_shadow_resolution(res.clamp(256, 8192)) {
                        tracing::warn!("set shadow resolution: {e:#}");
                    }
                }
                ComponentCommand::NetMessage { to, text } => {
                    if let Some(net) = self.net.as_mut() {
                        net.send_message(to, &text);
                    }
                }
                ComponentCommand::SetCursorLocked(locked) => {
                    self.cursor_locked = locked;
                    self.apply_cursor_lock();
                }
                ComponentCommand::Quit => event_loop.exit(),
                other => tracing::warn!("unhandled component command {other:?}"),
            }
        }
        if let Some(rel) = next_scene {
            self.load_scene(renderer, &rel);
        }
    }

    /// Lay out, interact with and paint the scene's UI for this frame.
    fn run_game_ui(&mut self, width: f32, height: f32) -> citrus_render::EguiDraw {
        let view = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, height));
        let raw = egui::RawInput {
            screen_rect: Some(view),
            ..Default::default()
        };
        let mut pointer = self.mouse;
        pointer.pos = if self.cursor_locked {
            None
        } else {
            self.pointer.map(|p| egui::pos2(p.x, p.y))
        };
        let input = UiInput {
            pointer,
            nav: self.input.ui_nav(),
            keys: std::mem::take(&mut self.ui_keys),
        };
        let (game_ui, scene, root) = (&mut self.game_ui, &mut self.scene, &self.config.assets_root);
        let mut frame = UiFrame::default();
        let out = self.ui_ctx.run(raw, |ctx| {
            frame = game_ui.run(ctx, view, scene, root, Some(&input), None, UiScope::Screen);
        });
        frame.merge(std::mem::take(&mut self.pending_world_frame));
        self.ui_frame.pointer_over_ui = frame.pointer_over_ui;
        self.ui_frame.text_editing = frame.text_editing;
        self.ui_frame.cancel |= frame.cancel;
        self.ui_frame.clicks.extend(frame.clicks);
        self.ui_frame.submits.extend(frame.submits);
        self.mouse.pressed = false;
        self.mouse.released = false;
        let primitives = self.ui_ctx.tessellate(out.shapes, out.pixels_per_point);
        citrus_render::EguiDraw {
            pixels_per_point: out.pixels_per_point,
            primitives,
            textures_delta: out.textures_delta,
        }
    }

    /// Lay out world-space canvases, pointed at by the mouse ray and the right
    /// VR controller (trigger = click).
    fn update_world_ui(&mut self, renderer: &mut Renderer, view: Mat4, proj: Mat4, width: f32, height: f32) {
        self.scene.extra_draws.clear();
        let mut rays = Vec::new();
        if let Some(session) = self.xr_session.as_ref()
            && let Some((pos, rot)) = session.body_poses().right_hand
        {
            let trigger = session.input().right_trigger;
            rays.push(UiRay {
                origin: pos,
                dir: rot * Vec3::NEG_Z,
                down: trigger,
                pressed: trigger && !self.vr_trigger_prev,
                released: !trigger && self.vr_trigger_prev,
            });
            self.vr_trigger_prev = trigger;
        }
        if !self.cursor_locked
            && let Some(p) = self.pointer
        {
            let ndc = glam::Vec2::new(2.0 * p.x / width - 1.0, 1.0 - 2.0 * p.y / height);
            let inv = (proj * view).inverse();
            let near = inv.project_point3(ndc.extend(0.0));
            let far = inv.project_point3(ndc.extend(1.0));
            if let Some(dir) = (far - near).try_normalize() {
                rays.push(UiRay {
                    origin: near,
                    dir,
                    down: self.mouse.down,
                    pressed: self.mouse.pressed,
                    released: self.mouse.released,
                });
            }
        }
        let (draws, frame) = self.world_ui.update(renderer, &mut self.scene, &self.config.assets_root, &rays, None);
        self.world_ui_draws = draws;
        self.pending_world_frame = frame;
    }

    /// One post-load step per frame after a scene switch: skybox, then baked lighting.
    fn step_post_load(&mut self, renderer: &mut Renderer) {
        let Some(post) = self.post_load.as_mut() else {
            return;
        };
        if let Some(sky) = post.skybox.take() {
            sky.install(renderer);
            return;
        }
        if let Some(baked) = post.baked.take() {
            self.scene.baked = baked;
            upload_probes(renderer, self.scene.baked.as_ref());
        }
        self.post_load = None;
    }

    fn apply_cursor_lock(&self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if self.cursor_locked {
            let _ = window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined));
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
        }
        window.set_cursor_visible(!self.cursor_locked);
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        let t = self.start.elapsed().as_secs_f32();

        let Some(mut renderer) = self.renderer.take() else {
            return;
        };
        if !self.finish_scene_switch(&mut renderer) {
            self.renderer = Some(renderer);
            event_loop.exit();
            return;
        }
        // Nothing to show until the boot scene is in.
        if self.play.is_none() {
            self.renderer = Some(renderer);
            return;
        }

        // Resolve input (keyboard/mouse accumulated + gamepad poll) and the
        // networking view for this frame, then drive components.
        if let Some(net) = self.net.as_mut() {
            net.pump();
        }
        let net_view = self.net.as_mut().map(|n| n.view()).unwrap_or_default();
        self.input.resolve_frame();
        let (win_w, win_h) = self
            .window
            .as_ref()
            .map(|w| {
                let s = w.inner_size();
                (s.width.max(1) as f32, s.height.max(1) as f32)
            })
            .unwrap_or((1.0, 1.0));
        let (screen_view, screen_proj) = self
            .scene
            .main_camera_view_proj(win_w / win_h)
            .map(|(v, p, _)| (v, p))
            .unwrap_or((Mat4::IDENTITY, Mat4::IDENTITY));
        let pointer = if self.cursor_locked { None } else { self.pointer };
        let screen = citrus_core::ScreenInfo {
            size: glam::Vec2::new(win_w, win_h),
            pointer,
            view: screen_view,
            proj: screen_proj,
            pointer_over_ui: self.ui_frame.pointer_over_ui,
            ui_clicks: std::mem::take(&mut self.ui_frame.clicks),
            ui_submits: std::mem::take(&mut self.ui_frame.submits),
            ui_text_editing: self.ui_frame.text_editing,
            ui_cancel: std::mem::take(&mut self.ui_frame.cancel),
        };
        let host = match self.play.as_mut() {
            Some(play) => play.tick(
                &mut World {
                    scene: &mut self.scene,
                    renderer: Some(&mut renderer),
                    registry: &self.registry,
                    shaders: &mut self.shaders,
                },
                &TickInput {
                    dt,
                    input: self.input.state(),
                    net: &net_view,
                    listener_fallback: self.last_cam_world,
                    screen: &screen,
                },
            ),
            None => Vec::new(),
        };
        self.apply_host_commands(&mut renderer, event_loop, host);
        self.step_post_load(&mut renderer);
        if self.flow.poll(&mut renderer, &self.registry, &mut self.shaders, &self.config.assets_root, &mut self.scene)
            && let Some(play) = self.play.as_mut()
        {
            play.structure_changed();
        }
        // Replicate networked objects (owner sends, others apply).
        if let Some(net) = self.net.as_mut() {
            self.scene.network_sync(net, dt);
        }

        // Realtime GI (if the scene enables it): live indirect bounce, same as
        // the editor preview. VR samples the world-probe volume (it can't use the
        // camera-space Flux gather), so flag the march to populate it.
        let vr_active = self.xr_session.is_some();
        self.rt_gi.update(&mut renderer, &mut self.scene, dt, vr_active);

        // VR: pump the OpenXR session lifecycle + read tracker poses into a
        // full-body target set for the IK solver. Avatar bone mapping (applying
        // the solve to a humanoid skeleton) is the remaining step.
        if let Some(session) = &mut self.xr_session {
            session.poll();
            let b = session.body_poses();
            self.vr_targets = citrus_core::TrackerTargets {
                head: b.head,
                left_hand: b.left_hand,
                right_hand: b.right_hand,
                hips: b.hips,
                left_foot: b.left_foot,
                right_foot: b.right_foot,
            };
        }

        // Feed VR tracker poses into the scene's humanoid IK targets (gameplay
        // could set these too — IK isn't VR-specific). Then advance + CPU-skin.
        self.scene
            .set_ik_targets(self.xr_session.as_ref().map(|_| self.vr_targets));
        self.scene.update_skinning(&mut renderer, dt);

        // Shipped game: the main camera's layer culling mask drives which layers
        // render (editor uses the viewport's own visibility toggle instead).
        self.scene.visible_layers = self.scene.main_camera_culling_mask();
        self.update_world_ui(&mut renderer, screen_view, screen_proj, win_w, win_h);
        let cam = screen_view.inverse();
        self.scene.prepare_lods(&mut renderer, &self.config.assets_root);
        self.scene.lod_camera = Some(cam.w_axis.truncate());
        self.particles
            .update(&mut renderer, &mut self.scene, &self.config.assets_root, cam, dt);
        self.scene.sync_draws(None, 0.0);

        let (width, height) = self
            .window
            .as_ref()
            .map(|w| {
                let s = w.inner_size();
                (s.width.max(1), s.height.max(1))
            })
            .unwrap_or((1, 1));
        let aspect = width as f32 / height as f32;

        let (view, proj, cam_pos) = self.scene.main_camera_view_proj(aspect).unwrap_or_else(|| {
            // No camera in the scene: a fixed look-at origin so something shows.
            let position = Vec3::new(0.0, 2.0, 6.0);
            let view = Mat4::look_at_rh(position, Vec3::ZERO, Vec3::Y);
            let proj = Mat4::perspective_rh(60f32.to_radians(), aspect.max(0.01), 0.05, 500.0);
            (view, proj, position)
        });
        self.last_cam_world = view.inverse();

        // Voice comms (task 8): capture mic on push-to-talk, play remote peers
        // back spatially (positioned at the object each peer owns).
        if self.net.is_some() {
            if self.voice.is_none() {
                self.voice = crate::voice::VoiceChat::new();
            }
            let transmit = self.input.state().down("Voice");
            if let (Some(voice), Some(net)) = (self.voice.as_mut(), self.net.as_mut()) {
                voice.capture_and_send(net, transmit);
                let owners = net.owners_snapshot();
                let packets = net.take_voice();
                let positions = self.scene.peer_voice_positions(&owners);
                voice.receive(packets, &positions);
                voice.update(cam_pos, 25.0);
            }
        }

        let env = self.scene.environment.clone();
        // For a baked scene the environment sun is in the bake, not realtime.
        let sun_realtime = env.sun_enabled && self.scene.baked.is_none();
        let mut lights = Vec::new();
        if sun_realtime {
            lights.push(citrus_render::LightInstance {
                kind: citrus_render::LightKind::Directional,
                position: Vec3::ZERO,
                direction: Vec3::from(env.sun_direction).normalize_or(Vec3::NEG_Y),
                color: env.sun_color,
                intensity: env.sun_intensity,
                range: 0.0,
                spot_inner_deg: 0.0,
                spot_outer_deg: 0.0,
                cast_shadows: true,
                soft_shadows: true,
                shadow_bias: 0.003,
                baked: false,
            });
        }
        lights.extend(self.scene.gather_lights());
        // Full light set (incl. baked) for the reflection cube capture.
        let capture_lights = self.scene.gather_lights_all();
        let world_light = LightData {
            direction: Vec3::from(env.sun_direction).normalize_or(Vec3::NEG_Y),
            color: env.sun_color,
            intensity: if sun_realtime { env.sun_intensity } else { 0.0 },
            ambient: self.scene.baked_ambient().unwrap_or([
                env.ambient[0] * env.ambient_intensity,
                env.ambient[1] * env.ambient_intensity,
                env.ambient[2] * env.ambient_intensity,
            ]),
            // Skybox IBL gated by the Enable-skybox toggle (see editor_app).
            env_intensity: if env.skybox_enabled { 1.0 } else { 0.0 },
        };

        let shadow_res = env.shadow_resolution.clamp(256, 8192);
        let shadow_pcf_texel = env.shadow_softness.max(0.0) / shadow_res as f32;
        if let Err(e) = renderer.set_shadow_resolution(shadow_res) {
            tracing::error!("setting shadow resolution: {e:#}");
        }
        // Realtime-mode probe: keep re-capturing continuously (one face/frame) so
        // in-game reflections track dynamic lighting. Same policy as the editor.
        if !renderer.reflection_capture_in_flight() {
            if let Some(p) = self.scene.active_reflection_probe(Vec3::ZERO) {
                if p.realtime {
                    renderer.request_reflection_capture(Vec3::from(p.capture), p.resolution);
                }
            }
        }
        let postfx = self.scene.effective_postfx(cam_pos, &self.config.assets_root);
        let planar = self.scene.planar_surfaces();

        let ui_draw = self.run_game_ui(win_w, win_h);
        let frame = FrameInput {
            clear_color: [0.016, 0.016, 0.024, 1.0],
            camera: CameraData {
                view,
                proj,
                position: cam_pos,
            },
            light: world_light,
            lights: &lights,
            capture_lights: &capture_lights,
            camera_preview: None,
            draw_skybox: env.skybox_enabled,
            shadow_pcf_texel,
            shadow_distance: env.shadow_distance,
            time: t,
            draws: &self.scene.draws,
            lightmap_preview: false,
            gi_debug: 0,
            render_viewport: true,
            viewport_extent: None,
            postfx,
            reflection_probe: self.scene.active_reflection_probe(cam_pos),
            planar: &planar,
            fog: self.scene.fog_params(),
            voxel_specular: self.scene.environment.realtime_gi.mode
                == citrus_assets::GiMode::FluxVoxel
                && self.scene.environment.realtime_gi.voxel_specular,
            egui: Some(ui_draw),
            world_ui: std::mem::take(&mut self.world_ui_draws),
        };
        if let Err(e) = renderer.render(&frame) {
            tracing::error!("render failed: {e:#}");
            event_loop.exit();
        }

        self.renderer = Some(renderer);
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

/// Push baked probe SH + volume metadata to the renderer (set-0 binding 2), or
/// clear it when the scene has no bake.
fn upload_probes(renderer: &mut Renderer, baked: Option<&crate::scene::BakedData>) {
    match baked {
        Some(b) => {
            let volumes: Vec<(Mat4, [f32; 3], [u32; 3], u32)> = b
                .probe_volumes
                .iter()
                .map(|v| {
                    (
                        v.world_to_local,
                        v.size,
                        [v.counts[0] as u32, v.counts[1] as u32, v.counts[2] as u32],
                        v.sh_base as u32,
                    )
                })
                .collect();
            renderer.set_baked_probes(&b.probe_sh, &volumes);
            renderer.set_baked_lightmaps(&b.lightmaps);
        }
        None => {
            renderer.set_baked_probes(&[], &[]);
            renderer.set_baked_lightmaps(&[]);
        }
    }
}

/// Per-user save folder for a game: `$XDG_DATA_HOME` / `~/.local/share`,
/// `%APPDATA%`, or `~/Library/Application Support`, plus `<title>/saves`.
fn user_save_dir(title: &str) -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    let name: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    base.unwrap_or_else(|| PathBuf::from(".")).join(name).join("saves")
}

/// Optionally start networking from env vars so a built game can host/join
/// without code: `CITRUS_HOST=9000` hosts on a port; `CITRUS_JOIN=1.2.3.4:9000`
/// joins a host.
fn start_net_from_env() -> Option<NetSession> {
    if let Ok(port) = std::env::var("CITRUS_HOST") {
        match port.parse::<u16>().map_err(|e| e.to_string()).and_then(|p| {
            NetSession::host(p).map_err(|e| e.to_string())
        }) {
            Ok(s) => return Some(s),
            Err(e) => tracing::error!("CITRUS_HOST failed: {e}"),
        }
    }
    if let Ok(addr) = std::env::var("CITRUS_JOIN") {
        match NetSession::join(&addr) {
            Ok(s) => return Some(s),
            Err(e) => tracing::error!("CITRUS_JOIN failed: {e}"),
        }
    }
    None
}

impl ApplicationHandler for GameApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.init(event_loop) {
            tracing::error!("game init failed: {e:#}");
            self.init_error = Some(e);
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize();
                }
            }
            // Feed the binding system (2C).
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state.is_pressed();
                if pressed && self.ui_frame.text_editing {
                    // Keys go to the focused text field, not gameplay.
                    match event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Backspace) => self.ui_keys.backspaces += 1,
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter) => self.ui_keys.enter = true,
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) => self.ui_keys.escape = true,
                        _ => {
                            if let Some(text) = &event.text {
                                self.ui_keys.text.push_str(text);
                            }
                        }
                    }
                } else if let PhysicalKey::Code(code) = event.physical_key {
                    self.input.key(code, pressed);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                self.input.mouse_button(button, pressed);
                if button == winit::event::MouseButton::Left {
                    self.mouse.down = pressed;
                    if pressed {
                        self.mouse.pressed = true;
                    } else {
                        self.mouse.released = true;
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = Some(glam::Vec2::new(position.x as f32, position.y as f32));
            }
            WindowEvent::CursorLeft { .. } => self.pointer = None,
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 50.0,
                };
                self.input.wheel(dy);
            }
            WindowEvent::Focused(false) => self.input.clear_held(),
            WindowEvent::Focused(true) => self.apply_cursor_lock(),
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            self.input.mouse_motion(delta.0, delta.1);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Boot: install the first scene even if no redraw arrives before it.
        if self.play.is_none()
            && let Some(mut renderer) = self.renderer.take()
        {
            let ok = self.finish_scene_switch(&mut renderer);
            self.renderer = Some(renderer);
            if !ok {
                event_loop.exit();
                return;
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}
