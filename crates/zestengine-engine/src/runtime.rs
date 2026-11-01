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

use zestengine_core::{ComponentCommand, ComponentRegistry};
use zestengine_render::{CameraData, FrameInput, LightData, Renderer};

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
    /// Input control schemes (2C). Loaded from `project.zestengine` by
    /// [`GameConfig::from_project_dir`]; defaults to KB+Mouse / Gamepad.
    pub bindings: zestengine_core::Bindings,
    /// Audio buses from `project.zestengine`.
    pub audio_mixer: zestengine_core::AudioMixer,
    pub spatial_audio: zestengine_core::SpatialAudioSettings,
    /// External tracking (OSC) receiver for face tracking from other tools.
    pub tracking_input: zestengine_core::TrackingInputSettings,
    pub physics: zestengine_core::PhysicsSettings,
    /// User-content (`.zestenginepak`) cache folder; None = the per-user cache dir.
    pub content_cache: Option<PathBuf>,
    /// Enable the F8 user-content developer overlay (also `ZESTENGINE_CONTENT_OVERLAY=1`).
    pub content_overlay: bool,
    /// Whether to start OpenXR. A Meta Quest build uses [`XrMode::Required`].
    pub xr: XrMode,
    /// Display refresh rate to ask the headset for (`XR_FB_display_refresh_rate`,
    /// Quest: 72 / 90 / 120 Hz); None keeps the runtime's default. The
    /// `ZESTENGINE_XR_REFRESH_HZ` environment variable overrides it.
    pub xr_refresh_rate: Option<f32>,
}

/// How a game uses OpenXR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum XrMode {
    /// VR when a runtime and headset are present, flat otherwise. On Android
    /// only builds that ship the OpenXR loader (Meta Quest) find a runtime.
    #[default]
    Auto,
    /// Standalone headset: failing to start VR is a startup error rather
    /// than a flat window nobody can see.
    Required,
    /// Never start OpenXR.
    Off,
}

impl Default for GameConfig {
    fn default() -> Self {
        Self {
            assets_root: PathBuf::from("assets"),
            boot_scene: String::new(),
            title: "zestengine game".into(),
            width: 1280.0,
            height: 720.0,
            bindings: zestengine_core::Bindings::default(),
            audio_mixer: zestengine_core::AudioMixer::default(),
            spatial_audio: zestengine_core::SpatialAudioSettings::default(),
            tracking_input: zestengine_core::TrackingInputSettings::default(),
            physics: zestengine_core::PhysicsSettings::default(),
            content_cache: None,
            content_overlay: false,
            xr: XrMode::Auto,
            xr_refresh_rate: None,
        }
    }
}

impl GameConfig {
    /// Build a config from a project directory's `project.zestengine`: the boot
    /// scene and window title come from the project settings, assets resolve
    /// relative to `assets_root`. This keeps a generated `main.rs` tiny and lets
    /// the Project Settings boot-scene choice take effect without editing code.
    /// A shipped build's `assets.pak` beside the executable takes precedence
    /// over `assets_root`: it's extracted once and the game runs from there.
    pub fn from_project_dir(assets_root: impl Into<PathBuf>) -> Result<Self> {
        let assets_root = match shipped_pak_root()? {
            Some(root) => root,
            None => assets_root.into(),
        };
        let project = zestengine_assets::load_project_file(&assets_root)
            .context("reading project.zestengine")?;
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
            tracking_input: project.tracking_input,
            physics: project.physics,
            xr: if project.vr { XrMode::Auto } else { XrMode::Off },
            ..Default::default()
        })
    }
}

/// Extraction folder of the `assets.pak` next to the running executable, or
/// `None` without one (development runs, Android).
fn shipped_pak_root() -> Result<Option<PathBuf>> {
    if cfg!(target_os = "android") {
        return Ok(None);
    }
    let Some(exe) = std::env::current_exe().ok() else { return Ok(None) };
    let pak = exe.with_file_name(zestengine_assets::game_pak::GAME_PAK_NAME);
    if !pak.is_file() {
        return Ok(None);
    }
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let cache = crate::content::content_cache_dir(&stem).with_file_name("assets");
    let root = zestengine_assets::game_pak::extract_game_pak(&pak, &cache).context("unpacking assets.pak")?;
    Ok(Some(root))
}

/// Run a game to completion: open a window, load the boot scene, and run the
/// component loop until the window closes. `register` adds the game's component
/// types (the built-ins are already present); it's the statically-linked stand-in
/// for the editor's plugin dylib load.
pub fn run_game(config: GameConfig, register: impl FnOnce(&mut ComponentRegistry)) -> Result<()> {
    run_game_on(EventLoop::new().context("creating event loop")?, config, register)
}

/// Android entry: call from the game's `#[unsafe(no_mangle)] fn android_main(app: AndroidApp)`
/// (the crate builds as a `cdylib` with a NativeActivity manifest). `config.assets_root`
/// must be a real directory, e.g. assets extracted under `app.internal_data_path()`.
#[cfg(target_os = "android")]
pub fn run_game_android(
    app: winit::platform::android::activity::AndroidApp,
    config: GameConfig,
    register: impl FnOnce(&mut ComponentRegistry),
) -> Result<()> {
    use winit::platform::android::EventLoopBuilderExtAndroid;
    if let Some(dir) = app.internal_data_path() {
        crate::input_engine::set_user_config_dir(dir.join("config"));
    }
    let event_loop = EventLoop::builder().with_android_app(app).build().context("creating event loop")?;
    run_game_on(event_loop, config, register)
}

/// Android: the game's assets ship inside the APK (`assets/`), listed one
/// relative path per line in `assets/zestengine_assets.txt` by the packager.
/// Extract them into the app's internal storage on first run (or when that
/// list changes), install the cooked texture caches, and return the directory
/// to use as `GameConfig::assets_root`.
#[cfg(target_os = "android")]
pub fn android_assets_root(app: &winit::platform::android::activity::AndroidApp) -> Result<PathBuf> {
    use std::io::Read;
    let read = |path: &str| -> Result<Vec<u8>> {
        let name = std::ffi::CString::new(path)?;
        let mut asset = app.asset_manager().open(&name).with_context(|| format!("APK asset {path} missing"))?;
        let mut bytes = Vec::new();
        asset.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let root = app.internal_data_path().context("no internal data path")?.join("assets");
    let manifest = read("zestengine_assets.txt")?;
    let stamp = root.join(".zestengine_assets.txt");
    if std::fs::read(&stamp).ok().as_deref() == Some(manifest.as_slice()) {
        return Ok(root);
    }
    for rel in String::from_utf8_lossy(&manifest).lines().map(str::trim).filter(|l| !l.is_empty()) {
        let dest = root.join(rel);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&dest, read(rel)?).with_context(|| format!("extracting {rel}"))?;
    }
    // The cook ships ASTC texture caches under a non-hidden folder (the APK
    // packager drops dot-prefixed paths): bind them to the extracted files.
    let (seeded, warnings) = zestengine_assets::package::seed_cooked_texture_caches(&root);
    for w in &warnings {
        tracing::warn!("{w}");
    }
    tracing::info!("extracted assets ({seeded} cooked texture cache(s))");
    std::fs::write(&stamp, &manifest)?;
    Ok(root)
}

fn run_game_on(event_loop: EventLoop<()>, config: GameConfig, register: impl FnOnce(&mut ComponentRegistry)) -> Result<()> {
    let mut registry = ComponentRegistry::with_builtins();
    register(&mut registry);
    let mut input = InputManager::new(config.bindings.clone());
    let bindings_path = crate::input_engine::player_bindings_path(&config.title);
    if let Some(dir) = bindings_path.parent() {
        crate::net::set_identity_dir(dir.to_path_buf());
    }
    input.set_overrides_path(Some(bindings_path));
    let slimevr = config.tracking_input.slimevr.then(crate::slimevr::SlimeVr::start);
    let mut face_input = crate::face_input::FaceInput::default();
    face_input.set_osc(config.tracking_input.osc_enabled.then_some(config.tracking_input.osc_port));
    let content_cache = config
        .content_cache
        .clone()
        .unwrap_or_else(|| crate::content::content_cache_dir(&config.title));
    let overlay_enabled = config.content_overlay || std::env::var_os("ZESTENGINE_CONTENT_OVERLAY").is_some();
    let mut inbox = vec![content_cache.join("inbox"), config.assets_root.join("content")];
    if let Some(dirs) = std::env::var_os("ZESTENGINE_CONTENT_DIR") {
        inbox.extend(std::env::split_paths(&dirs));
    }

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
        water: crate::water_render::WaterRenderer::default(),
        water_fx: crate::water_fx::WaterFx::default(),
        carry_fx: crate::water_carry::CarryFx::default(),
        ui_keys: UiKeys::default(),
        vr_trigger_prev: false,
        world_ui_draws: Vec::new(),
        pending_world_frame: UiFrame::default(),
        pointer: None,
        mouse: UiPointer::default(),
        rt_gi: crate::realtime_gi::RealtimeGiState::default(),
        refl_slots: crate::refl_slots::ReflectionSlots::default(),
        input,
        // Optional networking via env vars: ZESTENGINE_HOST=<port> or ZESTENGINE_JOIN=<addr>.
        net: start_net_from_env(),
        voice: None,
        start: Instant::now(),
        last_frame: Instant::now(),
        init_error: None,
        xr: None,
        xr_session: None,
        xr_stereo: None,
        xr_cull_camera: None,
        vr_targets: zestengine_core::TrackerTargets::default(),
        face_input,
        slimevr,
        body: crate::body::BodyTracking::load(),
        content: crate::content::ContentManager::with_startup_sources(content_cache, std::env::var("ZESTENGINE_CONTENT").ok()),
        content_overlay: crate::content::ContentOverlay::new(inbox),
        content_overlay_enabled: overlay_enabled,
    };
    event_loop.run_app(&mut app)?;
    if let Some(e) = app.init_error.take() {
        return Err(e);
    }
    Ok(())
}

fn frame_stats_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ZESTENGINE_FRAME_STATS").is_some())
}

/// Debug view mode for test captures (`ZESTENGINE_VIEW_MODE=2` = indirect/GI only),
/// matching the editor's viewport render modes.
fn view_mode() -> u32 {
    static MODE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        std::env::var("ZESTENGINE_VIEW_MODE").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
    })
}

/// Opt-in frame timing log (`ZESTENGINE_FRAME_STATS=1`, vsync off): every 240
/// frames, the mean frame interval and the renderer's GPU timestamp zones.
fn frame_stats(renderer: &Renderer) {
    /// Frames, last frame time, frame ms sum, GPU zone ms sums, late Hi-Z
    /// recoveries summed over the window.
    type Acc = (u32, Option<Instant>, f64, [f64; 9], u64);
    static ACC: std::sync::Mutex<Acc> = std::sync::Mutex::new((0, None, 0.0, [0.0; 9], 0));
    if !frame_stats_on() {
        return;
    }
    let s = renderer.stats();
    let Ok(mut a) = ACC.lock() else {
        return;
    };
    let now = Instant::now();
    if let Some(prev) = a.1 {
        a.0 += 1;
        a.2 += now.duration_since(prev).as_secs_f64() * 1000.0;
        let z = [
            s.gpu_frame_ms,
            s.gpu_gi_ms,
            s.gpu_shadows_ms,
            s.gpu_scene_ms,
            s.gpu_reflect_ms,
            s.gpu_post_ms,
            s.gpu_gi_gather_ms,
            s.gpu_gi_denoise_ms,
            s.gpu_gi_integrate_ms,
        ];
        for (acc, v) in a.3.iter_mut().zip(z) {
            *acc += v as f64;
        }
        a.4 += s.gpu_cull_recovered as u64;
    }
    a.1 = Some(now);
    if a.0 >= 240 {
        let n = a.0 as f64;
        let z = a.3.map(|v| v / n);
        tracing::info!(
            target: "frame_stats",
            "frame {:.3} ms | gpu {:.3} ms (gi {:.3} [gather {:.3}, denoise {:.3}, integrate {:.3}], shadows {:.3}, scene {:.3}, reflect {:.3}, post {:.3}) | {} draw calls ({} instanced), {} objects",
            a.2 / n, z[0], z[1], z[6], z[7], z[8], z[2], z[3], z[4], z[5],
            s.draw_calls, s.instanced_draws, s.opaque_draws + s.transparent_draws
        );
        if s.gpu_culling {
            tracing::info!(
                target: "frame_stats",
                "gpu cull {:.3} ms (hi-z {:.3}) | main view: {} tested, {} visible ({} recovered by the late pass), {} frustum/size culled, {} occluded | late-pass recoveries over {} frames: {} | {} indirect draws, {} shadow, {} eye instances",
                s.gpu_cull_ms, s.gpu_hiz_ms, s.gpu_cull_tested, s.gpu_cull_visible, s.gpu_cull_recovered,
                s.gpu_cull_frustum, s.gpu_cull_occluded, a.0, a.4,
                s.indirect_draws, s.gpu_cull_shadow_visible, s.gpu_cull_eye_visible
            );
        }
        *a = (0, Some(now), 0.0, [0.0; 9], 0);
    }
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
    water: crate::water_render::WaterRenderer,
    water_fx: crate::water_fx::WaterFx,
    carry_fx: crate::water_carry::CarryFx,
    /// Text typed since the last UI frame.
    ui_keys: UiKeys,
    vr_trigger_prev: bool,
    world_ui_draws: Vec<zestengine_render::WorldUiDraw>,
    /// World-canvas interaction, merged into `ui_frame` with the screen UI.
    pending_world_frame: UiFrame,
    /// Cursor position in window pixels.
    pointer: Option<glam::Vec2>,
    /// Left mouse button edges since the last UI frame.
    mouse: UiPointer,
    /// Realtime-GI driver. Runs the scene's realtime-GI setting in the game.
    rt_gi: crate::realtime_gi::RealtimeGiState,
    /// The two reflection probes' cubes (renderer slots 0 / 1).
    refl_slots: crate::refl_slots::ReflectionSlots,
    /// Input binding system (2C): keyboard/mouse/gamepad → action snapshot.
    input: InputManager,
    /// Active networking session (2G), if hosting/joined.
    net: Option<NetSession>,
    /// Voice comms (task 8), created lazily once networking is active.
    voice: Option<crate::voice::VoiceSystem>,
    start: Instant,
    last_frame: Instant,
    /// Set if `init` failed; surfaced after the loop exits.
    init_error: Option<anyhow::Error>,
    /// OpenXR context (VR). `None` when no runtime/headset; the game runs flat.
    xr: Option<zestengine_xr::XrContext>,
    /// Live VR session (poses/lifecycle), once the device is created.
    xr_session: Option<zestengine_xr::XrSession>,
    /// The eye swapchains (`setup_stereo`), once created.
    xr_stereo: Option<zestengine_xr::StereoSetup>,
    /// Last view enclosing both eyes (XR-only frames whose XR frame is skipped
    /// still run the shared per-frame work from it).
    xr_cull_camera: Option<CameraData>,
    /// Latest VR tracker poses (head/hands/hips/feet) for the full-body IK solver.
    /// Consumed by the avatar-IK application step (humanoid bone mapping) — wired.
    #[allow(dead_code)]
    vr_targets: zestengine_core::TrackerTargets,
    face_input: crate::face_input::FaceInput,
    /// SlimeVR server connection (full-body trackers), when enabled.
    slimevr: Option<crate::slimevr::SlimeVr>,
    /// The player's measured body, tracker roles and calibration.
    body: crate::body::BodyTracking,
    /// User-generated content packages (ctx.load_content / spawn_avatar / …).
    content: crate::content::ContentManager,
    content_overlay: crate::content::ContentOverlay,
    content_overlay_enabled: bool,
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
        // OpenXR first: a VR session needs the Vulkan instance and device
        // created through the runtime (it adds the extensions it requires).
        let xr = self.start_xr()?;
        let mut renderer = match &xr {
            Some(xr) => Renderer::new_with_xr(window.clone(), Some((&xr.instance, xr.system)))?,
            None => Renderer::new(window.clone())?,
        };
        if frame_stats_on() {
            // Measure throughput, not the display's refresh rate.
            renderer.set_vsync(false);
        }

        // The boot scene loads in the background; frames start once it's in.
        let boot = self.config.boot_scene.clone();
        self.load_scene(&renderer, &boot);

        if let Some(xr) = xr {
            // The VR session shares the renderer's Vulkan device.
            let (vi, pd, dev, qfi) = renderer.vulkan_raw_handles();
            match xr.create_session(vi, pd, dev, qfi) {
                Ok(session) => self.xr_session = Some(session),
                Err(e) if self.config.xr == XrMode::Required => {
                    return Err(e.context("creating the OpenXR session (this build requires VR)"));
                }
                Err(e) => tracing::warn!("OpenXR session create failed: {e:#}"),
            }
            self.xr = Some(xr);
            #[cfg(target_os = "android")]
            if self.xr_session.is_some() {
                request_android_permissions(&["com.oculus.permission.EYE_TRACKING", "com.oculus.permission.FACE_TRACKING"]);
            }
        }
        self.renderer = Some(renderer);
        self.window = Some(window);
        self.start = Instant::now();
        self.last_frame = Instant::now();
        Ok(())
    }

    /// Start OpenXR per [`GameConfig::xr`]. `None` runs flat; with
    /// [`XrMode::Required`] a missing runtime or headset is an error.
    fn start_xr(&self) -> Result<Option<zestengine_xr::XrContext>> {
        if self.config.xr == XrMode::Off {
            return Ok(None);
        }
        let started = zestengine_xr::XrContext::start(&self.config.title);
        if self.config.xr == XrMode::Required {
            return match started {
                Ok(Some(xr)) => {
                    tracing::info!("VR ready: {}", xr.system_name());
                    Ok(Some(xr))
                }
                Ok(None) => anyhow::bail!(
                    "this build requires VR but no OpenXR runtime or headset was found \
                     (is libopenxr_loader.so in the APK, and is the headset awake?)"
                ),
                Err(e) => Err(e.context("starting OpenXR (this build requires VR)")),
            };
        }
        match started {
            Ok(Some(xr)) => {
                tracing::info!("VR ready: {}", xr.system_name());
                Ok(Some(xr))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!("OpenXR start failed: {e:#}");
                Ok(None)
            }
        }
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
        let (rel, scene, post) = match result {
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
        self.content.scene_replaced(&rel);
        // The renderer was reset: drop caches holding its handles.
        self.world_ui = WorldUi::default();
        self.body.reset_render();
        self.body.scene_changed();
        self.particles = crate::particles::ParticleRenderer::default();
        self.water = crate::water_render::WaterRenderer::default();
        self.water_fx = crate::water_fx::WaterFx::default();
        self.carry_fx = crate::water_carry::CarryFx::default();
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
                session.set_physics(&self.config.physics, &self.scene);
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
            let Some(c) = crate::net_sync::route_command(&mut self.net, &self.scene, c) else {
                continue;
            };
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
                ComponentCommand::SetCursorLocked(locked) => {
                    self.cursor_locked = locked;
                    self.apply_cursor_lock();
                }
                ComponentCommand::Quit => event_loop.exit(),
                ComponentCommand::CalibrateTrackers => self.body.calibrate_now(),
                ComponentCommand::ClearTrackerCalibration => self.body.clear_calibration(),
                ComponentCommand::Body(c) => self.body.command(c),
                ComponentCommand::SetOscFaceTracking(port) => self.face_input.set_osc(port),
                ComponentCommand::Content(c) => self.content.queue(c),
                ComponentCommand::Input(c) => self.input.apply(c),
                other => tracing::warn!("unhandled component command {other:?}"),
            }
        }
        if let Some(rel) = next_scene {
            self.load_scene(renderer, &rel);
        }
    }

    /// Lay out, interact with and paint the scene's UI for this frame.
    /// Pointer, spawn point (2 m ahead of the camera) and possessed pawn for
    /// the user-content overlay.
    fn content_overlay_input(&self) -> crate::content::OverlayInput {
        let cam = self.last_cam_world;
        let forward = cam.transform_vector3(Vec3::NEG_Z).normalize_or_zero();
        let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z);
        let player = self.scene.objects.iter().find_map(|o| {
            o.components
                .iter()
                .any(|c| c.as_any().downcast_ref::<zestengine_core::Pawn>().is_some_and(|p| p.possessed))
                .then_some(o.id)
        });
        crate::content::OverlayInput {
            pointer: self.pointer.map(|p| egui::pos2(p.x, p.y)),
            clicked: self.mouse.pressed,
            spawn_at: (
                cam.transform_point3(Vec3::ZERO) + forward * 2.0,
                glam::Quat::from_rotation_arc(Vec3::NEG_Z, flat),
            ),
            player,
        }
    }

    fn run_game_ui(&mut self, width: f32, height: f32) -> zestengine_render::EguiDraw {
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
        let overlay_input = self.content_overlay.open.then(|| self.content_overlay_input());
        let (overlay, content) = (&mut self.content_overlay, &mut self.content);
        let mut over_overlay = false;
        let (game_ui, scene, root) = (&mut self.game_ui, &mut self.scene, &self.config.assets_root);
        let mut frame = UiFrame::default();
        let out = self.ui_ctx.run(raw, |ctx| {
            frame = game_ui.run(ctx, view, scene, root, Some(&input), None, UiScope::Screen);
            if let Some(oi) = &overlay_input {
                over_overlay = overlay.show(ctx, oi, content);
            }
        });
        frame.pointer_over_ui |= over_overlay;
        frame.merge(std::mem::take(&mut self.pending_world_frame));
        self.ui_frame.pointer_over_ui = frame.pointer_over_ui;
        self.ui_frame.text_editing = frame.text_editing;
        self.ui_frame.cancel |= frame.cancel;
        self.ui_frame.clicks.extend(frame.clicks);
        self.ui_frame.submits.extend(frame.submits);
        self.mouse.pressed = false;
        self.mouse.released = false;
        let primitives = self.ui_ctx.tessellate(out.shapes, out.pixels_per_point);
        zestengine_render::EguiDraw {
            pixels_per_point: out.pixels_per_point,
            primitives,
            textures_delta: out.textures_delta,
        }
    }

    /// Lay out world-space canvases, pointed at by the mouse ray and the right
    /// VR controller (trigger = click).
    fn update_world_ui(&mut self, renderer: &mut Renderer, view: Mat4, proj: Mat4, width: f32, height: f32, dt: f32) {
        self.scene.extra_draws.clear();
        let mut rays = Vec::new();
        let rig = self.scene.vr_player_rig().unwrap_or_else(|| camera_rig(self.last_cam_world, self.vr_targets.head));
        if let Some(session) = self.xr_session.as_ref()
            && let Some(hand) = self.vr_targets.right_hand
        {
            let (pos, rot) = rig.stage_pose_to_world(hand);
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
        let (mut draws, frame) = self.world_ui.update(renderer, &mut self.scene, &self.config.assets_root, &rays, None);
        draws.extend(self.body.panel_frame(renderer, &mut self.scene, &rig, dt));
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
            return;
        }
        // Reflections: baked probes upload their cubes, realtime ones capture
        // live; `refl_slots.update` does both per frame as the probes change.
        self.refl_slots.set_baked(std::mem::take(&mut post.reflection));
        self.post_load = None;
    }

    fn apply_cursor_lock(&self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        // The content overlay needs a free cursor while open.
        let locked = self.cursor_locked && !self.content_overlay.open;
        if locked {
            let _ = window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined));
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
        }
        window.set_cursor_visible(!locked);
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        let t = self.start.elapsed().as_secs_f32();

        let Some(mut renderer) = self.renderer.take() else {
            return;
        };
        let xr_running = self.xr_session.as_ref().is_some_and(|s| s.running);
        if loop_mode(renderer.has_surface(), xr_running) == LoopMode::Idle {
            self.renderer = Some(renderer);
            return;
        }
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
        crate::net_sync::auto_start(&mut self.net, &self.scene);
        if let Some(net) = self.net.as_mut() {
            net.pump();
        }
        let net_view = self.net.as_mut().map(|n| n.view()).unwrap_or_default();
        let vr = vr_frame(self.xr_session.as_ref(), &self.vr_targets);
        self.input.feed_vr(&vr);
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
        // A position outside the window (a stale or out-of-window report) is
        // no pointer: it must not edge-pan an RTS camera or hover UI.
        let pointer = (if self.cursor_locked { None } else { self.pointer })
            .filter(|p| p.x >= 0.0 && p.y >= 0.0 && p.x < win_w && p.y < win_h);
        let screen = zestengine_core::ScreenInfo {
            size: glam::Vec2::new(win_w, win_h),
            pointer,
            view: screen_view,
            proj: screen_proj,
            pointer_over_ui: self.ui_frame.pointer_over_ui,
            ui_clicks: std::mem::take(&mut self.ui_frame.clicks),
            ui_submits: std::mem::take(&mut self.ui_frame.submits),
            ui_text_editing: self.ui_frame.text_editing,
            ui_cancel: std::mem::take(&mut self.ui_frame.cancel),
            vr,
            controls: self.input.controls_info(),
            body: self.body.info(),
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
        let content = self.content.update(&mut crate::content::ContentHost {
            renderer: &mut renderer,
            registry: &self.registry,
            shaders: &mut self.shaders,
            root: &self.config.assets_root,
            scene: &mut self.scene,
            flow: &mut self.flow,
        });
        self.content_overlay.note_events(&content.events);
        if let Some(play) = self.play.as_mut() {
            if content.structure_changed {
                play.structure_changed();
            }
            for e in content.events {
                play.post_event(e);
            }
        }
        self.step_post_load(&mut renderer);
        if self.flow.poll(&mut renderer, &self.registry, &mut self.shaders, &self.config.assets_root, &mut self.scene)
            && let Some(play) = self.play.as_mut()
        {
            play.structure_changed();
        }
        // Replicate networked objects (owner sends, others apply), after
        // predicting / simulating predicted pawns.
        if let Some(net) = self.net.as_mut() {
            if let Some(play) = self.play.as_mut() {
                play.net_predict(&mut self.scene, net, self.input.state(), dt);
            }
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
            if session.exit_requested() {
                self.renderer = Some(renderer);
                event_loop.exit();
                return;
            }
            let b = session.body_poses();
            let mut devices = crate::body::xr_devices(&b);
            if let Some(slime) = &self.slimevr {
                slime.send_devices(b.head, b.left_hand, b.right_hand);
                devices.extend(slime.devices());
            }
            self.vr_targets = self.body.assemble(b.head, b.left_hand, b.right_hand, devices);
        }

        // Feed VR tracker poses into the scene's humanoid IK targets (gameplay
        // could set these too — IK isn't VR-specific), through the body
        // calibration and avatar fit. Then advance + CPU-skin.
        let face = self.face_input.poll(self.xr_session.as_ref());
        self.scene.set_local_face(face);
        let tracking = self.xr_session.as_ref().is_some_and(|s| s.running);
        let triggers = self.xr_session.as_ref().filter(|_| tracking).map(|s| s.input()).map_or([false; 2], |i| [i.left_trigger, i.right_trigger]);
        let body_frame = crate::body::BodyFrame { dt, tracking, apply: true, triggers };
        let vr_targets = self.body.update(&mut self.scene, &self.vr_targets, body_frame);
        self.scene
            .set_ik_targets(self.xr_session.as_ref().map(|_| vr_targets));
        self.scene.update_water_wind(dt);
        self.scene.update_skinning(&mut renderer, dt);

        // Shipped game: the main camera's layer culling mask drives which layers
        // render (editor uses the viewport's own visibility toggle instead).
        self.scene.visible_layers = self.scene.main_camera_culling_mask();
        self.update_world_ui(&mut renderer, screen_view, screen_proj, win_w, win_h, dt);
        let cam = screen_view.inverse();
        self.scene.sync_terrain_meshes(&mut renderer, &mut self.shaders, &self.config.assets_root);
        self.scene.prepare_lods(&mut renderer, &self.config.assets_root);
        self.scene.lod_camera = Some(cam.w_axis.truncate());
        self.scene.occlusion_camera = Some(cam.w_axis.truncate());
        self.particles
            .update(&mut renderer, &mut self.scene, &self.config.assets_root, cam, dt);
        if let Some(p) = self.play.as_mut() {
            p.set_shared_clock(self.net.as_ref().and_then(|n| n.shared_clock()));
        }
        let play_time = self.play.as_ref().map(|p| p.water_time());
        self.water.update(&mut renderer, &mut self.scene, cam.w_axis.truncate(), play_time, dt);
        let net = self.net.as_ref();
        let owns = |id: zestengine_core::ObjectId| net.is_none_or(|n| !n.is_connected() || n.has_authority_raw(id.raw()));
        self.scene.update_wave_riders(self.water.time(), dt, true, &owns);
        self.water_fx.update(&mut renderer, &mut self.scene, cam, play_time, dt);
        self.carry_fx.update(&mut renderer, &mut self.scene, cam, play_time.is_some(), dt, &mut self.water_fx.sim.particles);
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

        // Voice chat + lip sync (needs a Voice Chat component in the scene).
        let input = self.input.state();
        let right = self.last_cam_world.x_axis.truncate();
        crate::voice::run_frame(&mut self.voice, self.net.as_mut(), &mut self.scene, |a| input.down(a), cam_pos, right);

        let env = self.scene.environment.clone();
        // For a baked scene the environment sun is in the bake, not realtime.
        let sun_realtime = env.sun_enabled && !self.scene.lightmaps_in_use();
        let mut lights = Vec::new();
        if sun_realtime {
            lights.push(zestengine_render::LightInstance {
                kind: zestengine_render::LightKind::Directional,
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
        // The two reflection probes around the camera (blended per pixel);
        // realtime ones keep re-capturing so reflections track dynamic lighting.
        let refl = self.refl_slots.update(&mut renderer, self.scene.active_reflection_probes(cam_pos), true);
        let camera_views = self.scene.camera_target_views();
        for v in &camera_views {
            if let Err(e) = renderer.resize_camera_target(v.id, v.size[0], v.size[1]) {
                tracing::error!("resizing render-texture camera: {e:#}");
            }
        }
        let postfx = zestengine_render::PostFx {
            underwater: crate::underwater::view_fx(&self.scene, cam_pos, self.water.time()),
            ..self.scene.effective_postfx(cam_pos, &self.config.assets_root, self.water.time())
        };
        let planar = self.scene.planar_surfaces();
        let decals = self.scene.gather_decals(&mut renderer, &self.config.assets_root);
        // Full light set (incl. baked) for the reflection cube capture, only
        // while one is queued or running (empty = use `lights`).
        let voxel_emitters = {
            let gi = &self.scene.environment.realtime_gi;
            if gi.enabled && gi.mode == zestengine_assets::GiMode::FluxVoxel {
                self.scene.flux_voxel_dynamic_emitters()
            } else {
                Vec::new()
            }
        };
        let capture_lights = if renderer.reflection_capture_in_flight() {
            self.scene.gather_lights_all()
        } else {
            Vec::new()
        };

        let ui_draw = self.run_game_ui(win_w, win_h);
        let mut frame = FrameInput {
            clear_color: [0.016, 0.016, 0.024, 1.0],
            camera: CameraData {
                view,
                proj,
                position: cam_pos,
            },
            light: world_light,
            lights: &lights,
            capture_lights: &capture_lights,
            voxel_emitters: &voxel_emitters,
            camera_preview: None,
            preview_postfx: postfx,
            draw_skybox: env.skybox_enabled,
            shadow_pcf_texel,
            shadow_distance: env.shadow_distance,
            time: t,
            water_time: self.water.time(),
            water: self.water.bodies(),
            draws: &self.scene.draws,
            decals: &decals,
            lightmap_preview: false,
            gi_debug: view_mode(),
            render_viewport: true,
            viewport_extent: None,
            postfx,
            reflection_probe: refl[0],
            reflection_probes: refl,
            reflection_probe_b: refl[1],
            reflection_primary: 0,
            camera_targets: &camera_views,
            planar: &planar,
            fog: self.scene.fog_params(),
            voxel_specular: self.scene.environment.realtime_gi.mode
                == zestengine_assets::GiMode::FluxVoxel
                && self.scene.environment.realtime_gi.voxel_specular,
            egui: Some(ui_draw),
            world_ui: std::mem::take(&mut self.world_ui_draws),
        };
        // A standalone headset (Android with a running session) shows only the
        // eyes: the window is never presented, so its pass is skipped and the
        // eyes get their own frame update (render_xr_frame) from the head pose.
        let xr_only = cfg!(target_os = "android") && self.xr_session.as_ref().is_some_and(|s| s.running);
        if !xr_only
            && renderer.has_surface()
            && let Err(e) = renderer.render(&frame)
        {
            tracing::error!("render failed: {e:#}");
            renderer.report_device_lost(&e);
            event_loop.exit();
        }
        // VR: the same frame into the headset's eyes, from the VR Player rig
        // (or the main camera when the scene has none).
        let mut xr_rendered = false;
        if let Some(session) = self.xr_session.as_mut() {
            let rig = self.scene.vr_player_rig().unwrap_or_else(|| camera_rig(view.inverse(), self.vr_targets.head));
            let world_to_stage = rig.stage_to_world_mat().inverse();
            if self.xr_stereo.is_none() {
                // Desktop keeps the window's format when the runtime offers it;
                // a standalone headset takes its own sRGB / half-float pick.
                let preferred: Vec<u32> = if xr_only { Vec::new() } else { vec![renderer.color_format_raw()] };
                match session.setup_stereo(&preferred, xr_only && renderer.xr_multiview()) {
                    Ok(setup) => {
                        tracing::info!(
                            "XR stereo swapchains ready ({}x{}, VkFormat {}, multiview {})",
                            setup.width, setup.height, setup.format, setup.multiview
                        );
                        self.xr_stereo = Some(setup);
                        let hz = std::env::var("ZESTENGINE_XR_REFRESH_HZ").ok().and_then(|v| v.parse().ok()).or(self.config.xr_refresh_rate);
                        if let Some(hz) = hz
                            && let Some(got) = session.request_refresh_rate(hz)
                        {
                            tracing::info!("XR display refresh rate {got} Hz");
                        }
                    }
                    Err(e) => tracing::warn!("XR setup_stereo failed: {e:#}"),
                }
            }
            if let Some(stereo) = self.xr_stereo {
                match session.begin_frame() {
                    Ok(Some(xr_frame)) => {
                        let eyes: Vec<(Mat4, Mat4)> = xr_frame.eyes.iter().map(|e| (e.view * world_to_stage, e.proj)).collect();
                        // One view around both eyes: shadow cascades and multiview culling.
                        let cull = xr_frame
                            .combined_view(0.05, 2000.0)
                            .map(|(v, p)| (v * world_to_stage, p))
                            .or_else(|| eyes.first().copied());
                        if xr_only && let Some((cv, cp)) = cull {
                            let cam = CameraData { view: cv, proj: cp, position: cv.inverse().w_axis.truncate() };
                            self.xr_cull_camera = Some(cam);
                            frame.camera = cam;
                            xr_rendered = true;
                            if let Err(e) = renderer.render_xr_frame(&frame) {
                                tracing::error!("XR frame failed: {e:#}");
                                renderer.report_device_lost(&e);
                                event_loop.exit();
                            }
                        }
                        let base_fx = frame.postfx;
                        let eye_fx = |view: Mat4| zestengine_render::PostFx {
                            underwater: crate::underwater::view_fx(&self.scene, view.inverse().w_axis.truncate(), self.water.time()),
                            ..base_fx
                        };
                        if stereo.multiview && eyes.len() == 2 && let Some(cull) = cull {
                            let cams = [0, 1].map(|i| zestengine_render::XrEyeCamera { view: eyes[i].0, proj: eyes[i].1, postfx: eye_fx(eyes[i].0) });
                            if let Err(e) = renderer.render_xr_stereo(
                                xr_frame.eyes[0].image, stereo.format, stereo.width, stereo.height, &cams, cull, &frame,
                            ) {
                                tracing::error!("XR stereo render: {e:#}");
                            }
                        } else {
                            for (eye, &(world_view, proj)) in xr_frame.eyes.iter().zip(&eyes) {
                                frame.postfx = eye_fx(world_view);
                                if let Err(e) = renderer.render_xr_eye(
                                    eye.image, stereo.format, eye.width, eye.height, world_view, proj, &frame, None,
                                ) {
                                    tracing::error!("XR eye render: {e:#}");
                                }
                            }
                        }
                        if let Err(e) = session.end_frame(xr_frame) {
                            tracing::error!("XR end_frame: {e:#}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::error!("XR begin_frame: {e:#}"),
                }
            }
        }
        // XR-only frame the runtime asked not to render: still apply this
        // frame's uploads (textures, world UI, skinning) so nothing is lost.
        if xr_only && !xr_rendered {
            if let Some(cam) = self.xr_cull_camera {
                frame.camera = cam;
            }
            if let Err(e) = renderer.render_xr_frame(&frame) {
                tracing::error!("XR frame failed: {e:#}");
                renderer.report_device_lost(&e);
                event_loop.exit();
            }
        }
        frame_stats(&renderer);

        self.renderer = Some(renderer);
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

/// Push baked probe SH + volume metadata to the renderer (set-0 binding 2), or
/// clear it when the scene has no bake.
pub(crate) fn upload_probes(renderer: &mut Renderer, baked: Option<&crate::scene::BakedData>) {
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
            b.upload_lightmaps(renderer);
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
/// without code: `ZESTENGINE_HOST=9000` hosts on a port; `ZESTENGINE_JOIN=1.2.3.4:9000`
/// joins a host.
fn start_net_from_env() -> Option<NetSession> {
    if let Ok(port) = std::env::var("ZESTENGINE_HOST") {
        match port.parse::<u16>().map_err(|e| e.to_string()).and_then(|p| {
            NetSession::host(p).map_err(|e| e.to_string())
        }) {
            Ok(s) => return Some(s),
            Err(e) => tracing::error!("ZESTENGINE_HOST failed: {e}"),
        }
    }
    if let Ok(addr) = std::env::var("ZESTENGINE_JOIN") {
        match NetSession::join(&addr) {
            Ok(s) => return Some(s),
            Err(e) => tracing::error!("ZESTENGINE_JOIN failed: {e}"),
        }
    }
    None
}

impl Drop for GameApp {
    fn drop(&mut self) {
        // The XR swapchains live on the renderer's device: end the session
        // before the device goes.
        self.xr_session = None;
    }
}

impl ApplicationHandler for GameApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Poll);
        if self.window.is_some() {
            // Android: a new native window after a pause.
            if let Some(renderer) = self.renderer.as_mut()
                && let Err(e) = renderer.surface_recreated()
            {
                tracing::error!("recreating the window surface: {e:#}");
                self.init_error = Some(e);
                event_loop.exit();
            }
            return;
        }
        if let Err(e) = self.init(event_loop) {
            tracing::error!("game init failed: {e:#}");
            self.init_error = Some(e);
            event_loop.exit();
        }
    }

    /// Android pauses destroy the native window: drop the surface and idle
    /// until `resumed` brings a new one. The XR session keeps being polled so
    /// it can stop and restart with the app.
    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.surface_lost();
        }
        self.input.clear_held();
        event_loop.set_control_flow(ControlFlow::wait_duration(std::time::Duration::from_millis(100)));
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
                if pressed
                    && self.content_overlay_enabled
                    && !event.repeat
                    && event.physical_key == PhysicalKey::Code(winit::keyboard::KeyCode::F8)
                {
                    self.content_overlay.open = !self.content_overlay.open;
                    self.apply_cursor_lock();
                    return;
                }
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
        let xr_running = self.xr_session.as_ref().is_some_and(|s| s.running);
        let has_surface = self.renderer.as_ref().is_some_and(Renderer::has_surface);
        if self.renderer.is_some() && loop_mode(has_surface, xr_running) == LoopMode::Idle {
            if let Some(session) = self.xr_session.as_mut() {
                session.poll();
                if session.exit_requested() {
                    event_loop.exit();
                }
            }
            return;
        }
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

/// Headset + controller state for gameplay (`ctx.screen.vr`), from the live
/// session's latest stage-space poses. Inactive without a running session.
pub(crate) fn vr_frame(session: Option<&zestengine_xr::XrSession>, poses: &zestengine_core::TrackerTargets) -> zestengine_core::VrFrame {
    let Some(session) = session.filter(|s| s.running) else {
        return zestengine_core::VrFrame::default();
    };
    // No input focus (system menu, guardian): poses keep tracking, buttons
    // and sticks read released.
    let input = if session.focused() { session.input() } else { zestengine_xr::VrInput::default() };
    vr_frame_from(input, poses)
}

fn vr_frame_from(input: zestengine_xr::VrInput, poses: &zestengine_core::TrackerTargets) -> zestengine_core::VrFrame {
    zestengine_core::VrFrame {
        active: true,
        head: poses.head,
        left_hand: poses.left_hand,
        right_hand: poses.right_hand,
        left_stick: glam::Vec2::from(input.left_stick),
        right_stick: glam::Vec2::from(input.right_stick),
        left_grip: input.left_grip,
        right_grip: input.right_grip,
        left_trigger: input.left_trigger,
        right_trigger: input.right_trigger,
        menu: input.menu,
        left_trigger_value: input.left_trigger_value,
        right_trigger_value: input.right_trigger_value,
        a: input.a,
        b: input.b,
        x: input.x,
        y: input.y,
        left_stick_click: input.left_stick_click,
        right_stick_click: input.right_stick_click,
    }
}

/// What a game-loop iteration does, from the window surface and VR state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopMode {
    /// Tick and render (the window frame when there is a surface, the eyes
    /// when VR runs).
    Run,
    /// Paused with nothing to draw into: skip the frame.
    Idle,
}

fn loop_mode(has_surface: bool, xr_running: bool) -> LoopMode {
    if has_surface || xr_running { LoopMode::Run } else { LoopMode::Idle }
}

/// Play space for a scene without a VR Player: the tracked head lands on the
/// main camera, facing its yaw.
fn camera_rig(cam_world: Mat4, head_stage: Option<(Vec3, glam::Quat)>) -> zestengine_core::VrRig {
    let (_, rot, pos) = cam_world.to_scale_rotation_translation();
    let f = rot * Vec3::NEG_Z;
    let yaw = (-f.x).atan2(-f.z);
    let head = head_stage.map_or(Vec3::ZERO, |(p, _)| p);
    zestengine_core::VrRig { origin: pos - glam::Quat::from_rotation_y(yaw) * head, yaw, scale: 1.0 }
}

/// tracing writer for logcat: one `__android_log_write` per formatted line,
/// at the event's priority, under the tag `zestengine`.
#[cfg(target_os = "android")]
pub(crate) struct Logcat;

#[cfg(target_os = "android")]
pub(crate) struct LogcatLine {
    priority: i32,
    buf: Vec<u8>,
}

#[cfg(target_os = "android")]
#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(priority: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32;
}

#[cfg(target_os = "android")]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logcat {
    type Writer = LogcatLine;

    fn make_writer(&'a self) -> LogcatLine {
        LogcatLine { priority: 4, buf: Vec::new() }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> LogcatLine {
        // android/log.h: VERBOSE 2, DEBUG 3, INFO 4, WARN 5, ERROR 6.
        let priority = match *meta.level() {
            tracing::Level::TRACE => 2,
            tracing::Level::DEBUG => 3,
            tracing::Level::INFO => 4,
            tracing::Level::WARN => 5,
            tracing::Level::ERROR => 6,
        };
        LogcatLine { priority, buf: Vec::new() }
    }
}

#[cfg(target_os = "android")]
impl std::io::Write for LogcatLine {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "android")]
impl Drop for LogcatLine {
    fn drop(&mut self) {
        let text: Vec<u8> = self.buf.iter().copied().filter(|&b| b != 0).collect();
        let text = String::from_utf8_lossy(&text);
        if let Ok(line) = std::ffi::CString::new(text.trim_end()) {
            unsafe { __android_log_write(self.priority, c"zestengine".as_ptr(), line.as_ptr()) };
        }
    }
}

/// Ask for runtime permissions the manifest declares (Quest Pro eye and face
/// tracking) that are not granted yet. The system prompt is asynchronous, so
/// a grant takes effect on the next launch.
#[cfg(target_os = "android")]
fn request_android_permissions(permissions: &[&str]) {
    use jni::objects::{JObject, JValue};
    let ctx = ndk_context::android_context();
    let run = || -> jni::errors::Result<()> {
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast())? };
        let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
        let mut env = vm.attach_current_thread()?;
        let mut missing = Vec::new();
        for p in permissions {
            let name = env.new_string(p)?;
            let state = env
                .call_method(&activity, "checkSelfPermission", "(Ljava/lang/String;)I", &[JValue::Object(&name)])?
                .i()?;
            if state != 0 {
                missing.push(name);
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let array = env.new_object_array(missing.len() as i32, "java/lang/String", JObject::null())?;
        for (i, name) in missing.iter().enumerate() {
            env.set_object_array_element(&array, i as i32, name)?;
        }
        env.call_method(&activity, "requestPermissions", "([Ljava/lang/String;I)V", &[JValue::Object(&array), JValue::Int(1)])?;
        Ok(())
    };
    if let Err(e) = run() {
        tracing::warn!("requesting Android permissions {permissions:?}: {e}");
        if let Ok(vm) = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
            && let Ok(env) = vm.attach_current_thread()
        {
            let _ = env.exception_clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idles_only_without_surface_or_vr() {
        assert_eq!(loop_mode(true, false), LoopMode::Run);
        assert_eq!(loop_mode(true, true), LoopMode::Run);
        assert_eq!(loop_mode(false, true), LoopMode::Run);
        assert_eq!(loop_mode(false, false), LoopMode::Idle);
    }

    #[test]
    fn camera_rig_puts_the_head_on_the_camera() {
        let cam = Mat4::from_rotation_translation(glam::Quat::from_rotation_y(0.7), Vec3::new(3.0, 1.8, -2.0));
        let head = (Vec3::new(0.3, 1.65, -0.2), glam::Quat::IDENTITY);
        let rig = camera_rig(cam, Some(head));
        assert!((rig.stage_to_world(head.0) - Vec3::new(3.0, 1.8, -2.0)).length() < 1e-4);
        assert!((rig.yaw - 0.7).abs() < 1e-4);
        let untracked = camera_rig(cam, None);
        assert!((untracked.origin - Vec3::new(3.0, 1.8, -2.0)).length() < 1e-4);
    }

    #[test]
    fn controller_input_maps_to_the_vr_frame() {
        let input = zestengine_xr::VrInput {
            left_stick: (0.5, -1.0),
            right_stick: (0.0, 0.25),
            left_grip: 0.8,
            right_grip: 0.1,
            left_trigger: true,
            right_trigger: false,
            menu: true,
            right_trigger_value: 0.4,
            a: true,
            right_stick_click: true,
            ..Default::default()
        };
        let poses = zestengine_core::TrackerTargets { head: Some((Vec3::Y, glam::Quat::IDENTITY)), ..Default::default() };
        let f = vr_frame_from(input, &poses);
        assert!(f.active && f.left_trigger && !f.right_trigger && f.menu);
        assert_eq!(f.left_stick, glam::Vec2::new(0.5, -1.0));
        assert_eq!(f.right_stick, glam::Vec2::new(0.0, 0.25));
        assert_eq!((f.left_grip, f.right_grip), (0.8, 0.1));
        assert_eq!(f.right_trigger_value, 0.4);
        assert!(f.a && !f.b && !f.x && !f.y && f.right_stick_click && !f.left_stick_click);
        assert_eq!(f.head, poses.head);
    }
}
