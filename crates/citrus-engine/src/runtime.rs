//! Standalone game runtime: load a scene and run it in a window, no editor.
//!
//! This is what a bundled game's generated `main.rs` calls. It reuses the same
//! scene / renderer / component path as the editor's Play mode, minus every
//! egui panel. `FrameInput.egui` is `None`, so no editor code runs. Components
//! are linked in statically and registered through the `register` callback
//! (the runtime replacement for the editor's dylib hot-load).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use glam::{Mat4, Vec3};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use citrus_core::{ComponentCommand, ComponentRegistry};
use citrus_render::{CameraData, FrameInput, LightData, Renderer};

use crate::input_engine::InputManager;
use crate::net::NetSession;
use crate::physics::PhysicsWorld;
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
        physics: None,
        rt_gi: crate::realtime_gi::RealtimeGiState::default(),
        input: InputManager::new(config_bindings),
        // Optional networking via env vars: CITRUS_HOST=<port> or CITRUS_JOIN=<addr>.
        net: start_net_from_env(),
        voice: None,
        start: Instant::now(),
        last_frame: Instant::now(),
        init_error: None,
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
    /// Physics simulation, rebuilt whenever a scene loads. A game always runs it.
    physics: Option<PhysicsWorld>,
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

        let boot = self.config.boot_scene.clone();
        self.load_scene(&mut renderer, &boot)
            .with_context(|| format!("loading boot scene {boot:?}"))?;

        self.renderer = Some(renderer);
        self.window = Some(window);
        self.start = Instant::now();
        self.last_frame = Instant::now();
        Ok(())
    }

    /// Replace the current scene with the one at `rel` (relative to the assets
    /// root), upload its GPU resources, set its skybox, and fire `start`.
    fn load_scene(&mut self, renderer: &mut Renderer, rel: &str) -> Result<()> {
        let abs = self.config.assets_root.join(rel);
        let file = citrus_assets::load_scene_file(&abs)
            .with_context(|| format!("reading scene {}", abs.display()))?;
        self.scene = LoadedScene::load_scene_file(
            renderer,
            &file,
            &self.config.assets_root,
            &self.registry,
            &mut self.shaders,
        )?;
        // Loaded scenes carry their own Camera/Light components, but older or
        // hand-written scenes might not, so mirror the editor's safety net.
        self.scene.ensure_camera_components(&self.registry);
        self.scene.ensure_light_components(&self.registry);
        self.scene.ensure_camera_ids();
        apply_skybox(renderer, &self.scene, &self.config.assets_root);

        // Baked GI: load the scene's `.lightmap`/`.lightdata` sidecars and push
        // the probe SH to the renderer so the standard shader samples it.
        self.scene.load_bake_sidecars(&abs.with_extension(""));
        upload_probes(renderer, self.scene.baked.as_ref());

        // A game is always "playing": build physics + start every component.
        self.physics = Some(PhysicsWorld::build(&self.scene));
        let mut commands = Vec::new();
        let net_view = self.net.as_mut().map(|n| n.view()).unwrap_or_default();
        self.scene.start_components(
            self.start.elapsed().as_secs_f32(),
            self.input.state(),
            &net_view,
            &mut commands,
        );
        self.apply_commands(renderer, commands);
        Ok(())
    }

    /// Apply deferred component requests after an update pass. Only the last
    /// `LoadScene` wins (mirrors the editor); a switch re-fires `start`. Camera /
    /// transform / ownership commands apply immediately.
    fn apply_commands(&mut self, renderer: &mut Renderer, commands: Vec<ComponentCommand>) {
        let mut next_scene = None;
        for c in commands {
            match c {
                ComponentCommand::LoadScene(rel) => next_scene = Some(rel),
                ComponentCommand::SetActiveCamera(id) => self.scene.set_active_camera(id),
                ComponentCommand::SetLocalTransform {
                    id,
                    translation,
                    rotation,
                    scale,
                } => self.scene.set_local_transform(id, translation, rotation, scale),
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
            }
        }
        if let Some(rel) = next_scene
            && let Err(e) = self.load_scene(renderer, &rel)
        {
            tracing::error!("switching to scene {rel:?}: {e:#}");
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        let t = self.start.elapsed().as_secs_f32();

        let Some(mut renderer) = self.renderer.take() else {
            return;
        };

        // Resolve input (keyboard/mouse accumulated + gamepad poll) and the
        // networking view for this frame, then drive components.
        if let Some(net) = self.net.as_mut() {
            net.pump();
        }
        let net_view = self.net.as_mut().map(|n| n.view()).unwrap_or_default();
        self.input.resolve_frame();
        let mut commands = Vec::new();
        self.scene
            .update_components(dt, t, self.input.state(), &net_view, &mut commands);
        // Step physics + write simulated transforms back.
        if let Some(phys) = self.physics.as_mut()
            && !phys.is_empty()
        {
            phys.step(dt);
            phys.sync_back(&mut self.scene);
        }
        self.apply_commands(&mut renderer, commands);
        // Replicate networked objects (owner sends, others apply).
        if let Some(net) = self.net.as_mut() {
            self.scene.network_sync(net, dt);
        }

        // Realtime GI (if the scene enables it): live indirect bounce, same as
        // the editor preview.
        self.rt_gi.update(&mut renderer, &mut self.scene, dt);

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
            });
        }
        lights.extend(self.scene.gather_lights());
        let world_light = LightData {
            direction: Vec3::from(env.sun_direction).normalize_or(Vec3::NEG_Y),
            color: env.sun_color,
            intensity: if sun_realtime { env.sun_intensity } else { 0.0 },
            ambient: self.scene.baked_ambient().unwrap_or([
                env.ambient[0] * env.ambient_intensity,
                env.ambient[1] * env.ambient_intensity,
                env.ambient[2] * env.ambient_intensity,
            ]),
        };

        let shadow_res = env.shadow_resolution.clamp(256, 8192);
        let shadow_pcf_texel = env.shadow_softness.max(0.0) / shadow_res as f32;
        if let Err(e) = renderer.set_shadow_resolution(shadow_res) {
            tracing::error!("setting shadow resolution: {e:#}");
        }
        let postfx = self.scene.effective_postfx(cam_pos, &self.config.assets_root);

        let frame = FrameInput {
            clear_color: [0.016, 0.016, 0.024, 1.0],
            camera: CameraData {
                view,
                proj,
                position: cam_pos,
            },
            light: world_light,
            lights: &lights,
            camera_preview: None,
            draw_skybox: env.skybox_enabled,
            shadow_pcf_texel,
            shadow_distance: env.shadow_distance,
            time: t,
            draws: &self.scene.draws,
            lightmap_preview: false,
            gi_debug: 0,
            postfx,
            egui: None,
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

/// Upload the scene's skybox (or clear it) on the renderer.
fn apply_skybox(renderer: &mut Renderer, scene: &LoadedScene, assets_root: &Path) {
    match scene.skybox.clone() {
        Some(rel) => {
            let abs = assets_root.join(&rel);
            match citrus_assets::load_texture_file(&abs, true) {
                Ok(data) => {
                    if let Err(e) = renderer.set_skybox(Some(&data)) {
                        tracing::error!("setting skybox: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::error!("loading skybox {rel}: {e:#}");
                    let _ = renderer.set_skybox(None);
                }
            }
        }
        None => {
            let _ = renderer.set_skybox(None);
        }
    }
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
                if let PhysicalKey::Code(code) = event.physical_key {
                    self.input.key(code, event.state.is_pressed());
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.input.mouse_button(button, state == ElementState::Pressed);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 50.0,
                };
                self.input.wheel(dy);
            }
            WindowEvent::Focused(false) => self.input.clear_held(),
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            self.input.mouse_motion(delta.0, delta.1);
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}
