//! citrus-engine: application loop, scene management, editor shell.
//!
//! The editor is a dockable layout (egui_dock) around a transparent
//! Viewport tab: Scene list, unified Inspector, project Files browser,
//! menu bar, transform gizmos, picking, and drag & drop assets.

mod camera;
mod gizmo;
mod icon;
mod log_capture;
mod lsp;
mod plugins;
mod scene;
mod shaders;
mod undo;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use egui_dock::{DockArea, DockState, NodeIndex};
use glam::Vec3;
use winit::application::ApplicationHandler;
use winit::event::{
    DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use camera::FlyCamera;
use citrus_editor::{
    CodeDiagnostic, CodeEditor, ComponentRegistry, FileBrowser, InspectorContent, InspectorPanel,
    MaterialModel, ObjectInfoModel, ScenePanel, ShaderUiInfo, TransformModel,
};
use citrus_render::{CameraData, FrameInput, LightData, Renderer};
use gizmo::{GizmoState, GizmoTool};
use scene::{LoadedScene, material_from_model, model_from_material, relative_to};
use shaders::ShaderLibrary;
use undo::{ObjectState, UndoEntry, UndoStack};

pub struct AppConfig {
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Optional .scene / model file to open; falls back to the test scene.
    pub scene_path: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            title: "citrus editor".into(),
            width: 1600,
            height: 900,
            scene_path: None,
        }
    }
}

/// Install the tracing subscriber (stdout + the in-app Log tab capture).
/// Call once at startup before [`run`].
pub fn init_logging() {
    log_capture::init();
}

pub fn run(config: AppConfig) -> Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let project_root = std::env::current_dir()?;
    let mut app = EngineApp {
        config,
        window: None,
        renderer: None,
        scene: LoadedScene::empty(),
        egui_ctx: egui::Context::default(),
        egui_state: None,
        dock_state: default_layout(),
        inspector: InspectorPanel::new(),
        inspector_lock_target: None,
        scene_panel: ScenePanel::new(),
        file_browser: FileBrowser::new(project_root.clone()),
        selection: Selection::None,
        file_material: None,
        open_editors: vec![],
        lsp: None,
        lsp_failed: false,
        lsp_requests: HashMap::new(),
        gizmo: GizmoState::new(),
        actions: Vec::new(),
        undo_stack: UndoStack::default(),
        suppress_undo_record: false,
        components: ComponentRegistry::with_builtins(),
        playing: false,
        play_snapshot: None,
        shaders: ShaderLibrary::default(),
        shader_files: Vec::new(),
        last_shader_scan: None,
        dirty_materials: HashSet::new(),
        last_material_edit: None,
        plugins: plugins::PluginHost::default(),
        plugin_build_error: None,
        project: citrus_assets::ProjectFile::default(),
        camera: FlyCamera::default(),
        orbit_pivot: None,
        looking: false,
        panning: false,
        look_delta: (0.0, 0.0),
        keys: HashSet::new(),
        last_cursor: None,
        viewport_rect: egui::Rect::EVERYTHING,
        project_root,
        current_scene_path: None,
        scene_name_input: "scenes/world.scene".into(),
        show_stats: true,
        show_stats_overlay: false,
        show_help: false,
        log_filter: LogFilter::default(),
        probe_drag: None,
        stats: Stats::default(),
        world: hecs::World::new(),
        start: Instant::now(),
        last_frame: Instant::now(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[derive(Clone, PartialEq)]
enum Selection {
    None,
    Object(usize),
    File(PathBuf),
}

/// Pre-frame snapshot of the selection's editable state (undo diffing).
enum EditSnapshot {
    None,
    Object {
        index: usize,
        state: ObjectState,
        material: Option<usize>,
        model: Option<Box<MaterialModel>>,
    },
    File {
        path: PathBuf,
        model: Box<MaterialModel>,
    },
}

/// Plain-text file open in the rudimentary code editor.
struct OpenEditor {
    path: PathBuf,
    text: String,
    dirty: bool,
    language: String,
    diagnostics: Vec<CodeDiagnostic>,
    last_edit: Option<Instant>,
    /// Text changed since the last LSP `didChange` was sent.
    lsp_dirty: bool,
    /// Active completion popup + hover tooltip (LSP-fed).
    completion: Option<citrus_editor::CompletionState>,
    hover: Option<citrus_editor::HoverState>,
    /// Pending go-to-definition jump (0-based line, utf-8 column).
    goto: Option<(u32, u32)>,
}

/// An in-flight LSP request, keyed by request id, awaiting its response.
#[derive(Clone)]
enum LspRequestKind {
    Completion { path: PathBuf, anchor_char: usize },
    Hover { path: PathBuf },
    Definition,
}

/// Extensions the code editor opens (scene/material files have dedicated
/// inspectors).
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "frag", "vert", "glsl", "slang", "toml", "ron", "md", "txt", "json", "citrus",
];

/// Scratch state for a `.material` file opened in the Inspector.
struct FileMaterial {
    path: PathBuf,
    file: citrus_assets::MaterialFile,
    model: MaterialModel,
    dirty: bool,
}

#[derive(Default)]
struct Stats {
    frames: u64,
    fps: f32,
    frame_ms: f32,
    accum: f32,
    accum_frames: u32,
}

impl Stats {
    fn tick(&mut self, dt: f32) {
        self.frames += 1;
        self.accum += dt;
        self.accum_frames += 1;
        if self.accum >= 0.25 {
            self.fps = self.accum_frames as f32 / self.accum;
            self.frame_ms = self.accum * 1000.0 / self.accum_frames as f32;
            self.accum = 0.0;
            self.accum_frames = 0;
        }
    }
}

/// Deferred editor operations (UI runs without renderer access; these are
/// processed afterwards).
enum EditorAction {
    SelectFile(PathBuf),
    ImportModel(PathBuf),
    CreateMaterial(PathBuf),
    CreateScene(PathBuf),
    CreateShader(PathBuf),
    CreateFolder(PathBuf),
    PickAt(egui::Pos2),
    AssignMaterialAt(egui::Pos2, PathBuf),
    AssignMaterialToObject(usize, PathBuf),
    MaterialEdited(usize),
    ResetMaterial(usize),
    SaveFileMaterial,
    /// A `.material` file's model changed in the Inspector: propagate to
    /// every scene material loaded from that file.
    FileMaterialEdited(PathBuf),
    /// Replace the scene with an empty one.
    NewScene,
    /// Open a code/text file in a dockable editor tab (focus if already open).
    OpenCodeFile(PathBuf),
    /// Save the open editor for this path.
    SaveOpenEditor(PathBuf),
    /// Request LSP completion / hover / definition at a cursor char index.
    LspCompletion(PathBuf, usize),
    LspHover(PathBuf, usize),
    LspGoto(PathBuf, usize),
    /// New Rust component module in the plugin crate.
    CreateComponent,
    LoadScene(PathBuf),
    SaveScene(Option<PathBuf>),
    /// Alt + left drag: orbit the camera around the selected object.
    Orbit(f32, f32),
    /// Right mouse pressed over the viewport: enter mouse-look.
    StartLook,
    /// Scroll over the viewport: dolly the camera.
    Dolly(f32),
    /// Left drag ended: release the locked orbit pivot.
    OrbitEnd,
    /// F: frame the selected object.
    FocusSelected,
    Undo,
    Redo,
    /// Create an empty/camera/primitive in the scene.
    Spawn(citrus_assets::ObjectSource),
    SpawnLight(citrus_editor::LightKind),
    SpawnProbeVolume,
    /// (child, new parent, before-sibling) reorder/move.
    MoveObject(usize, Option<usize>, Option<usize>),
    DeleteObject(usize),
    DeleteFile(PathBuf),
    SetSkybox(PathBuf),
    ClearSkybox,
    /// Re-parent an object (None = unparent).
    SetParent(usize, Option<usize>),
    /// Enter / leave Play mode (components run while playing).
    TogglePlay,
    /// Create the starter plugin crate under plugins/.
    CreatePluginCrate,
    /// Rebuild + reload all component plugins.
    ReloadPlugins,
}

#[derive(Clone, PartialEq, Eq, Debug)]
#[allow(dead_code)]
enum Tab {
    Viewport,
    /// Live view from the scene's main camera (smallest camera id).
    Camera,
    Scene,
    Inspector,
    /// World lighting / skybox setup.
    Environment,
    Files,
    /// Filterable console mirroring all tracing logs.
    Log,
    /// Independent code/shader editor tab (can be opened many times, dragged, closed).
    Code(PathBuf),
}

fn default_layout() -> DockState<Tab> {
    // Camera shares the Viewport node (tab next to it); Environment shares the
    // Inspector node.
    let mut state = DockState::new(vec![Tab::Viewport, Tab::Camera]);
    let tree = state.main_surface_mut();
    let [viewport, _right] = tree.split_right(
        NodeIndex::root(),
        0.78,
        vec![Tab::Inspector, Tab::Environment],
    );
    let [viewport, _left] = tree.split_left(viewport, 0.18, vec![Tab::Scene]);
    let [_viewport, _bottom] = tree.split_below(viewport, 0.74, vec![Tab::Files, Tab::Log]);
    state
}

// Log level colours for the console.
const LOG_ERROR: egui::Color32 = egui::Color32::from_rgb(255, 110, 110);
const LOG_WARN: egui::Color32 = egui::Color32::from_rgb(240, 200, 90);
const LOG_INFO: egui::Color32 = egui::Color32::from_rgb(200, 205, 215);
const LOG_DEBUG: egui::Color32 = egui::Color32::from_rgb(120, 180, 235);
const LOG_TRACE: egui::Color32 = egui::Color32::from_rgb(140, 140, 150);

/// Active drag of one Light Probe Volume face handle (box-resize).
struct ProbeDrag {
    object: usize,
    /// Box axis being resized (0 = X, 1 = Y, 2 = Z).
    axis: usize,
    /// Component `size` at drag start.
    start_size: Vec3,
    /// Object origin in world space at drag start.
    start_origin_world: Vec3,
    /// Unit outward direction of the dragged face, in world space.
    world_axis: Vec3,
    /// World scale along `axis` (maps component size → world meters).
    scale_a: f32,
    /// Screen pixels per 1 world-meter along `world_axis`.
    screen_axis: egui::Vec2,
    /// Cursor position when the drag began.
    start_cursor: egui::Pos2,
}

/// Log tab view state: which levels to show, a substring filter, and whether
/// to keep the view pinned to the newest entry.
struct LogFilter {
    error: bool,
    warn: bool,
    info: bool,
    debug: bool,
    trace: bool,
    search: String,
    autoscroll: bool,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self {
            error: true,
            warn: true,
            info: true,
            // Debug/trace are off the global filter by default; toggles still
            // work if the user raises RUST_LOG.
            debug: true,
            trace: true,
            search: String::new(),
            autoscroll: true,
        }
    }
}

impl LogFilter {
    fn shows(&self, level: tracing::Level) -> bool {
        match level {
            tracing::Level::ERROR => self.error,
            tracing::Level::WARN => self.warn,
            tracing::Level::INFO => self.info,
            tracing::Level::DEBUG => self.debug,
            tracing::Level::TRACE => self.trace,
        }
    }
}

struct EngineApp {
    config: AppConfig,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    scene: LoadedScene,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    dock_state: DockState<Tab>,
    inspector: InspectorPanel,
    /// Selection the Inspector is pinned to while `inspector.locked` is on.
    inspector_lock_target: Option<Selection>,
    scene_panel: ScenePanel,
    file_browser: FileBrowser,
    selection: Selection,
    file_material: Option<FileMaterial>,
    open_editors: Vec<OpenEditor>,
    /// Language server (rust-analyzer) for `.rs` editing; spawned on demand.
    lsp: Option<lsp::LspClient>,
    /// Set if spawning the language server failed, so we don't retry.
    lsp_failed: bool,
    /// In-flight LSP completion/hover requests, keyed by request id.
    lsp_requests: HashMap<i64, LspRequestKind>,
    gizmo: GizmoState,
    actions: Vec<EditorAction>,
    undo_stack: UndoStack,
    /// Set while applying undo/redo so the frame diff doesn't re-record it.
    suppress_undo_record: bool,
    /// Registered component types (built-ins now; plugins extend this).
    components: ComponentRegistry,
    /// Play mode: components run every frame; edits aren't recorded to undo.
    playing: bool,
    /// Object state captured at Play start, restored at Stop.
    play_snapshot: Option<Vec<ObjectState>>,
    /// Custom shader compile cache + hot reload.
    shaders: ShaderLibrary,
    /// Project-relative `.frag` paths for the shader picker.
    shader_files: Vec<String>,
    /// Last shader-file scan / hot-reload poll.
    last_shader_scan: Option<Instant>,
    /// Scene materials edited since their last auto-save.
    dirty_materials: HashSet<usize>,
    /// Debounce for material auto-save (save when the gesture settles).
    last_material_edit: Option<Instant>,
    /// Rust component plugins (built + hot-loaded dylibs).
    plugins: plugins::PluginHost,
    /// Last failed plugin build's compiler output (shown in a window).
    plugin_build_error: Option<String>,
    /// project.citrus: name, last scene, per-project engine settings.
    project: citrus_assets::ProjectFile,
    camera: FlyCamera,
    /// Orbit pivot, locked for the duration of one left-drag.
    orbit_pivot: Option<Vec3>,
    /// Right mouse held: mouse-look (cursor hidden + locked) + WASD fly.
    looking: bool,
    /// Middle mouse held: pan.
    panning: bool,
    /// Raw mouse deltas accumulated while looking.
    look_delta: (f64, f64),
    keys: HashSet<KeyCode>,
    last_cursor: Option<(f64, f64)>,
    /// Viewport tab rect in egui points (updated每 frame by the tab).
    viewport_rect: egui::Rect,
    project_root: PathBuf,
    current_scene_path: Option<PathBuf>,
    scene_name_input: String,
    show_stats: bool,
    show_stats_overlay: bool,
    show_help: bool,
    /// Log tab filters (levels + search + autoscroll).
    log_filter: LogFilter,
    /// In-progress Light Probe Volume face-handle resize.
    probe_drag: Option<ProbeDrag>,
    stats: Stats,
    #[allow(dead_code)] // entities arrive with the component-system milestone
    world: hecs::World,
    start: Instant,
    last_frame: Instant,
}

impl EngineApp {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let (icon_w, icon_h, icon_rgba) = icon::rgba();
        let attrs = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_window_icon(winit::window::Icon::from_rgba(icon_rgba, icon_w, icon_h).ok())
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.width,
                self.config.height,
            ));
        // Wayland resolves icons via app_id → .desktop file; X11 via
        // WM_CLASS. Same name on both, plus a best-effort desktop-entry
        // install so the compositor can actually find the icon.
        #[cfg(target_os = "linux")]
        let attrs = {
            use winit::platform::{wayland, x11};
            let attrs = wayland::WindowAttributesExtWayland::with_name(attrs, "citrus", "citrus");
            x11::WindowAttributesExtX11::with_name(attrs, "citrus", "citrus")
        };
        #[cfg(target_os = "linux")]
        icon::install_desktop_entry();
        let window = Arc::new(event_loop.create_window(attrs)?);
        let mut renderer = Renderer::new(window.clone())?;

        // Plugins first: scene files may reference plugin components.
        if plugins::PluginHost::any_plugins(&self.project_root)
            && let Err(e) = self
                .plugins
                .build_and_load(&self.project_root, &mut self.components)
        {
            tracing::error!("building component plugins: {e:#}");
            self.plugin_build_error = Some(format!("{e:#}"));
        }

        // project.citrus: restores the last scene and per-project settings;
        // created with defaults on first run.
        match citrus_assets::load_project_file(&self.project_root) {
            Ok(project) => self.project = project,
            Err(_) => {
                self.project = citrus_assets::ProjectFile {
                    name: self
                        .project_root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "citrus project".into()),
                    ..Default::default()
                };
                if let Err(e) = citrus_assets::save_project_file(&self.project_root, &self.project)
                {
                    tracing::warn!("creating project.citrus: {e:#}");
                }
            }
        }
        window.set_title(&format!("{} — citrus", self.project.name));
        renderer.set_vsync(self.project.settings.vsync);
        self.show_stats = self.project.settings.show_stats;
        self.show_stats_overlay = self.project.settings.show_stats_overlay;
        self.gizmo.snap = self.project.settings.snap;
        self.gizmo.grid_size = self.project.settings.grid_size;

        match self.config.scene_path.clone() {
            Some(path) if path.ends_with(".scene") => {
                let file = citrus_assets::load_scene_file(&path)?;
                self.scene = LoadedScene::load_scene_file(
                    &mut renderer,
                    &file,
                    &self.project_root,
                    &self.components,
                    &mut self.shaders,
                )?;
                self.current_scene_path = Some(PathBuf::from(path));
            }
            Some(path) => {
                let asset = citrus_assets::load_model(&path)?;
                self.scene
                    .add_asset_scene(&mut renderer, &asset, Some(Path::new(&path)))?;
            }
            None => {
                // Last scene from project.citrus; broken/missing files fall
                // back to the built-in test scene instead of aborting.
                let restored = self.project.last_scene.clone().and_then(|rel| {
                    let abs = self.project_root.join(&rel);
                    let result = citrus_assets::load_scene_file(&abs).and_then(|file| {
                        LoadedScene::load_scene_file(
                            &mut renderer,
                            &file,
                            &self.project_root,
                            &self.components,
                            &mut self.shaders,
                        )
                    });
                    match result {
                        Ok(scene) => Some((scene, abs)),
                        Err(e) => {
                            tracing::warn!("restoring last scene {rel}: {e:#}");
                            None
                        }
                    }
                });
                match restored {
                    Some((scene, path)) => {
                        self.scene = scene;
                        self.current_scene_path = Some(path);
                    }
                    None => {
                        let asset = citrus_assets::test_scene();
                        self.scene.add_asset_scene(&mut renderer, &asset, None)?;
                        // Lighting comes from the world environment (Environment
                        // window), not an auto-spawned scene light.
                    }
                }
            }
        }

        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            None,
            None,
            None,
        ));
        self.renderer = Some(renderer);
        self.window = Some(window);
        self.apply_skybox();
        Ok(())
    }

    fn set_looking(&mut self, looking: bool) {
        if self.looking == looking {
            return;
        }
        self.looking = looking;
        if let Some(window) = &self.window {
            window.set_cursor_visible(!looking);
            let grab = if looking {
                window
                    .set_cursor_grab(CursorGrabMode::Locked)
                    .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
            } else {
                window.set_cursor_grab(CursorGrabMode::None)
            };
            if let Err(e) = grab {
                tracing::debug!("cursor grab: {e}");
            }
        }
        if !looking {
            self.look_delta = (0.0, 0.0);
        }
    }

    fn cursor_in_viewport(&self) -> bool {
        let Some((x, y)) = self.last_cursor else {
            return false;
        };
        let ppp = self.egui_ctx.pixels_per_point();
        self.viewport_rect
            .contains(egui::pos2(x as f32 / ppp, y as f32 / ppp))
    }

    fn update_camera(&mut self, dt: f32) {
        let (dx, dy) = std::mem::take(&mut self.look_delta);
        if self.looking {
            self.camera.look(dx as f32, dy as f32);
            let key = |code: KeyCode| self.keys.contains(&code) as i32 as f32;
            let local = Vec3::new(
                key(KeyCode::KeyD) - key(KeyCode::KeyA),
                key(KeyCode::KeyE) - key(KeyCode::KeyQ),
                key(KeyCode::KeyW) - key(KeyCode::KeyS),
            );
            if local != Vec3::ZERO {
                let fast = self.keys.contains(&KeyCode::ShiftLeft)
                    || self.keys.contains(&KeyCode::ShiftRight);
                self.camera.fly(local, dt, fast);
            }
        }
    }

    /// Cursor position (egui points) → world ray.
    fn cursor_ray(&self, pos: egui::Pos2) -> Option<(Vec3, Vec3)> {
        let window = self.window.as_ref()?;
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return None;
        }
        let ppp = self.egui_ctx.pixels_per_point();
        let px = pos.x * ppp;
        let py = pos.y * ppp;
        let ndc_x = 2.0 * (px / size.width as f32) - 1.0;
        let ndc_y = 1.0 - 2.0 * (py / size.height as f32);
        let aspect = size.width as f32 / size.height as f32;
        let inv = (self.camera.proj(aspect) * self.camera.view()).inverse();
        let near = inv.project_point3(Vec3::new(ndc_x, ndc_y, 0.0));
        let far = inv.project_point3(Vec3::new(ndc_x, ndc_y, 1.0));
        Some((
            self.camera.position,
            (far - near).normalize_or(self.camera.forward()),
        ))
    }

    fn select_object(&mut self, index: Option<usize>) {
        self.selection = match index {
            Some(i) => Selection::Object(i),
            None => Selection::None,
        };
    }

    fn process_actions(&mut self) {
        if self.renderer.is_none() {
            return;
        }
        let actions = std::mem::take(&mut self.actions);
        for action in actions {
            // Renderer exists (checked above); reborrow per action so other
            // &self helpers (cursor_ray, pick) stay usable.
            macro_rules! renderer {
                () => {
                    self.renderer.as_mut().unwrap()
                };
            }
            match action {
                EditorAction::SelectFile(path) => {
                    self.file_material = None;
                    if path.extension().is_some_and(|e| e == "material") {
                        match citrus_assets::load_material_file(&path) {
                            Ok(file) => {
                                let mut model = model_from_material(
                                    &file.name,
                                    &file.params,
                                    &file.features,
                                    file.textures.normal.is_some(),
                                );
                                model.shader = file.shader.clone();
                                if let Some(q) = file.render_queue {
                                    model.render_queue = q;
                                }
                                self.file_material = Some(FileMaterial {
                                    path: path.clone(),
                                    file,
                                    model,
                                    dirty: false,
                                });
                            }
                            Err(e) => tracing::error!("opening material: {e:#}"),
                        }
                    }
                    self.selection = Selection::File(path);
                }
                EditorAction::ImportModel(path) => match citrus_assets::load_model(&path) {
                    Ok(asset) => {
                        let rel = self
                            .project_root
                            .join(relative_to(&path, &self.project_root));
                        let source = relative_to(&rel, &self.project_root);
                        if let Err(e) = self.scene.add_asset_scene(
                            renderer!(),
                            &asset,
                            Some(Path::new(&source)),
                        ) {
                            tracing::error!("importing model: {e:#}");
                        }
                    }
                    Err(e) => tracing::error!("importing model: {e:#}"),
                },
                EditorAction::CreateMaterial(dir) => {
                    let path = unique_path(&dir, "new_material", "material");
                    let file = citrus_assets::MaterialFile {
                        name: path
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "new material".into()),
                        shader: "standard".into(),
                        params: Default::default(),
                        features: Default::default(),
                        render_queue: None,
                        textures: Default::default(),
                        custom: Default::default(),
                    };
                    match citrus_assets::save_material_file(&path, &file) {
                        Ok(()) => self.actions.push(EditorAction::SelectFile(path)),
                        Err(e) => tracing::error!("creating material: {e:#}"),
                    }
                }
                EditorAction::CreateScene(dir) => {
                    let path = unique_path(&dir, "new_scene", "scene");
                    if let Err(e) =
                        citrus_assets::save_scene_file(&path, &citrus_assets::SceneFile::default())
                    {
                        tracing::error!("creating scene: {e:#}");
                    }
                }
                EditorAction::CreateShader(dir) => {
                    let path = unique_path(&dir, "new_shader", "frag");
                    match std::fs::write(&path, citrus_assets::SHADER_TEMPLATE) {
                        Ok(()) => self.actions.push(EditorAction::SelectFile(path)),
                        Err(e) => tracing::error!("creating shader: {e:#}"),
                    }
                }
                EditorAction::CreateFolder(dir) => {
                    let path = unique_path(&dir, "new_folder", "");
                    if let Err(e) = std::fs::create_dir_all(&path) {
                        tracing::error!("creating folder: {e:#}");
                    }
                }
                EditorAction::PickAt(pos) => {
                    if let Some((origin, dir)) = self.cursor_ray(pos) {
                        let hit = self.scene.pick(origin, dir);
                        self.select_object(hit);
                    }
                }
                EditorAction::AssignMaterialAt(pos, path) => {
                    if let Some((origin, dir)) = self.cursor_ray(pos)
                        && let Some(hit) = self.scene.pick(origin, dir)
                    {
                        let Some(before) = self.scene.objects[hit].render.map(|r| r.material)
                        else {
                            continue;
                        };
                        self.scene.assign_material(
                            renderer!(),
                            &mut self.shaders,
                            hit,
                            &path,
                            &self.project_root,
                        );
                        let after = self.scene.objects[hit]
                            .render
                            .map(|r| r.material)
                            .unwrap_or(before);
                        if before != after {
                            self.undo_stack.record(UndoEntry::Assign {
                                object: hit,
                                before,
                                after,
                            });
                        }
                    }
                }
                EditorAction::AssignMaterialToObject(object, path) => {
                    let Some(before) = self.scene.objects[object].render.map(|r| r.material) else {
                        continue;
                    };
                    self.scene.assign_material(
                        renderer!(),
                        &mut self.shaders,
                        object,
                        &path,
                        &self.project_root,
                    );
                    let after = self.scene.objects[object]
                        .render
                        .map(|r| r.material)
                        .unwrap_or(before);
                    if before != after {
                        self.undo_stack.record(UndoEntry::Assign {
                            object,
                            before,
                            after,
                        });
                    }
                }
                EditorAction::MaterialEdited(index) => {
                    self.scene.apply_material(
                        renderer!(),
                        &mut self.shaders,
                        &self.project_root,
                        index,
                    );
                }
                EditorAction::ResetMaterial(index) => {
                    self.scene.materials[index].model = self.scene.materials[index].default.clone();
                    self.scene.apply_material(
                        renderer!(),
                        &mut self.shaders,
                        &self.project_root,
                        index,
                    );
                }
                EditorAction::FileMaterialEdited(path) => {
                    let Some(fm) = &self.file_material else {
                        continue;
                    };
                    let model = fm.model.clone();
                    for index in 0..self.scene.materials.len() {
                        if self.scene.materials[index].file.as_deref() != Some(&path) {
                            continue;
                        }
                        // Keep texture-derived state from the scene entry;
                        // the file editor doesn't change textures.
                        let has_normal = self.scene.materials[index].model.has_normal_texture;
                        let mut model = model.clone();
                        model.has_normal_texture = has_normal;
                        self.scene.materials[index].model = model;
                        self.scene.apply_material(
                            renderer!(),
                            &mut self.shaders,
                            &self.project_root,
                            index,
                        );
                    }
                }
                EditorAction::SaveFileMaterial => {
                    if let Some(fm) = &mut self.file_material {
                        let (params, features) = material_from_model(&fm.model);
                        fm.file.params = params;
                        fm.file.features = features;
                        fm.file.shader = fm.model.shader.clone();
                        fm.file.name = fm.model.name.clone();
                        fm.file.render_queue = Some(fm.model.render_queue);
                        if fm.model.shader != "standard" {
                            // Save custom values by property name (robust to
                            // property reordering in the shader source).
                            if let Some(source) = self
                                .shaders
                                .get(&fm.model.shader)
                                .and_then(|e| e.source.as_ref())
                            {
                                fm.file.custom = source.unpack(&fm.model.custom_values);
                            }
                        } else {
                            fm.file.custom.clear();
                        }
                        match citrus_assets::save_material_file(&fm.path, &fm.file) {
                            Ok(()) => fm.dirty = false,
                            Err(e) => tracing::error!("saving material: {e:#}"),
                        }
                    }
                }
                EditorAction::OpenCodeFile(path) => {
                    self.open_code_file(path);
                }
                EditorAction::SaveOpenEditor(path) => {
                    if let Some(editor) = self.open_editors.iter_mut().find(|e| e.path == path) {
                        match std::fs::write(&editor.path, &editor.text) {
                            Ok(()) => {
                                editor.dirty = false;
                                tracing::info!("saved {}", editor.path.display());
                            }
                            Err(e) => tracing::error!("saving file: {e:#}"),
                        }
                    }
                    if let Some(lsp) = self.lsp.as_ref() {
                        lsp.save(&path);
                    }
                }
                EditorAction::LspCompletion(path, cursor) => {
                    if let Some(editor) = self.open_editors.iter().find(|e| e.path == path) {
                        let (line, character) = char_to_line_col(&editor.text, cursor);
                        let anchor_char = word_start(&editor.text, cursor);
                        if let Some(lsp) = self.lsp.as_mut() {
                            let id = lsp.completion(&path, line, character);
                            self.lsp_requests
                                .insert(id, LspRequestKind::Completion { path, anchor_char });
                        }
                    }
                }
                EditorAction::LspHover(path, cursor) => {
                    if let Some(editor) = self.open_editors.iter().find(|e| e.path == path) {
                        let (line, character) = char_to_line_col(&editor.text, cursor);
                        if let Some(lsp) = self.lsp.as_mut() {
                            let id = lsp.hover(&path, line, character);
                            self.lsp_requests.insert(id, LspRequestKind::Hover { path });
                        }
                    }
                }
                EditorAction::LspGoto(path, cursor) => {
                    if let Some(editor) = self.open_editors.iter().find(|e| e.path == path) {
                        let (line, character) = char_to_line_col(&editor.text, cursor);
                        if let Some(lsp) = self.lsp.as_mut() {
                            let id = lsp.definition(&path, line, character);
                            self.lsp_requests.insert(id, LspRequestKind::Definition);
                        }
                    }
                }
                EditorAction::CreateComponent => {
                    match plugins::create_component(&self.project_root) {
                        Ok(path) => self.actions.push(EditorAction::SelectFile(path)),
                        Err(e) => tracing::error!("creating component: {e:#}"),
                    }
                }
                EditorAction::NewScene => {
                    if let Err(e) = renderer!().reset_scene() {
                        tracing::error!("resetting scene: {e:#}");
                    }
                    self.scene = LoadedScene::empty();
                    self.selection = Selection::None;
                    self.file_material = None;
                    self.current_scene_path = None;
                    self.undo_stack.clear();
                    self.apply_skybox();
                    self.save_project();
                }
                EditorAction::LoadScene(path) => {
                    match citrus_assets::load_scene_file(&path) {
                        Ok(file) => {
                            if let Err(e) = renderer!().reset_scene() {
                                tracing::error!("resetting scene: {e:#}");
                            }
                            match LoadedScene::load_scene_file(
                                renderer!(),
                                &file,
                                &self.project_root,
                                &self.components,
                                &mut self.shaders,
                            ) {
                                Ok(scene) => {
                                    self.scene = scene;
                                    self.selection = Selection::None;
                                    self.current_scene_path = Some(path);
                                    // Indices into the old scene are invalid.
                                    self.undo_stack.clear();
                                    self.apply_skybox();
                                    self.save_project();
                                }
                                Err(e) => {
                                    tracing::error!("loading scene: {e:#}");
                                    self.scene = LoadedScene::empty();
                                    self.selection = Selection::None;
                                }
                            }
                        }
                        Err(e) => tracing::error!("loading scene: {e:#}"),
                    }
                }
                EditorAction::Orbit(dx, dy) => {
                    // Lock the pivot on the first orbit frame of a drag;
                    // recomputing it mid-drag makes the rotation drift.
                    if self.orbit_pivot.is_none() {
                        let target = match self.selection {
                            Selection::Object(i) => self.scene.world_transform(i).w_axis.truncate(),
                            _ => {
                                // What's at the viewport center: the hit
                                // object, or a point ahead of the camera.
                                let center = self.viewport_rect.center();
                                self.cursor_ray(center)
                                    .and_then(|(origin, dir)| {
                                        self.scene
                                            .pick(origin, dir)
                                            .map(|hit| self.scene.object_bounds(hit).0)
                                    })
                                    .unwrap_or_else(|| {
                                        self.camera.position + self.camera.forward() * 5.0
                                    })
                            }
                        };
                        // Orbit around the point on the CURRENT view ray at
                        // the target's depth: the camera is already looking
                        // straight at it, so look-at orbit engages with zero
                        // snap.
                        let forward = self.camera.forward();
                        let depth = (target - self.camera.position).dot(forward).max(0.5);
                        self.orbit_pivot = Some(self.camera.position + forward * depth);
                    }
                    if let Some(pivot) = self.orbit_pivot {
                        self.camera.orbit(pivot, dx, dy);
                    }
                }
                EditorAction::OrbitEnd => self.orbit_pivot = None,
                EditorAction::Undo => self.apply_history(true),
                EditorAction::Redo => self.apply_history(false),
                EditorAction::Spawn(source) => {
                    let name = match &source {
                        citrus_assets::ObjectSource::Empty => "Empty".to_owned(),
                        citrus_assets::ObjectSource::Camera => "Camera".to_owned(),
                        citrus_assets::ObjectSource::Primitive { shape } => {
                            shape.label().to_owned()
                        }
                        _ => "Object".to_owned(),
                    };
                    // Place in front of the camera, on the ground-ish plane.
                    let mut position = self.camera.position + self.camera.forward() * 3.0;
                    position.y = position.y.max(0.0);
                    match self.scene.spawn(renderer!(), source, name, position) {
                        Ok(index) => self.selection = Selection::Object(index),
                        Err(e) => tracing::error!("spawning object: {e:#}"),
                    }
                }
                EditorAction::SpawnLight(kind) => {
                    use citrus_editor::{LightComponent, LightKind};
                    let name = format!("{} Light", kind.label());
                    // Directional lights default to aiming down-ish (a sun);
                    // point/spot spawn in front of the camera like other
                    // objects.
                    let (position, rotation) = match kind {
                        LightKind::Directional => (
                            self.camera.position + self.camera.forward() * 3.0 + Vec3::Y * 3.0,
                            glam::Quat::from_rotation_arc(
                                Vec3::NEG_Z,
                                Vec3::new(-0.4, -1.0, -0.3).normalize(),
                            ),
                        ),
                        _ => {
                            let mut p = self.camera.position + self.camera.forward() * 3.0;
                            p.y = p.y.max(0.5);
                            (p, glam::Quat::IDENTITY)
                        }
                    };
                    match self.scene.spawn(
                        renderer!(),
                        citrus_assets::ObjectSource::Light,
                        name,
                        position,
                    ) {
                        Ok(index) => {
                            let light = LightComponent {
                                kind,
                                ..LightComponent::default()
                            };
                            self.scene.objects[index].rotation = rotation;
                            self.scene.objects[index].components.push(Box::new(light));
                            self.selection = Selection::Object(index);
                        }
                        Err(e) => tracing::error!("spawning light: {e:#}"),
                    }
                }
                EditorAction::SpawnProbeVolume => {
                    let mut p = self.camera.position + self.camera.forward() * 4.0;
                    p.y = p.y.max(1.5);
                    match self.scene.spawn(
                        renderer!(),
                        citrus_assets::ObjectSource::Empty,
                        "Light Probe Volume".to_owned(),
                        p,
                    ) {
                        Ok(index) => {
                            self.scene.objects[index]
                                .components
                                .push(Box::new(citrus_editor::LightProbeVolume::default()));
                            self.selection = Selection::Object(index);
                        }
                        Err(e) => tracing::error!("spawning probe volume: {e:#}"),
                    }
                }
                EditorAction::SetParent(child, parent) => {
                    self.scene.set_parent(child, parent);
                }
                EditorAction::MoveObject(child, parent, before) => {
                    let map = self.scene.reorder_object(child, parent, before);
                    if !map.is_empty() {
                        // Object indices shifted; remap selection + lock target,
                        // and keep this structural change out of undo diffing.
                        let remap = |sel: &Selection| match sel {
                            Selection::Object(i) if *i < map.len() => Selection::Object(map[*i]),
                            other => other.clone(),
                        };
                        #[allow(clippy::redundant_closure_call)]
                        let new_selection = remap(&self.selection);
                        self.selection = new_selection;
                        self.inspector_lock_target = self.inspector_lock_target.as_ref().map(remap);
                        self.suppress_undo_record = true;
                    }
                }
                EditorAction::DeleteObject(index) => {
                    // By design, deletion is NOT undoable (see TODO.md).
                    self.scene.remove_object(index);
                    // Indices shifted; drop selection/lock to stay safe.
                    self.selection = Selection::None;
                    self.inspector_lock_target = None;
                    self.inspector.locked = false;
                }
                EditorAction::DeleteFile(path) => {
                    let result = if path.is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    match result {
                        Ok(()) => {
                            tracing::info!("deleted {}", path.display());
                            if self.selection == Selection::File(path.clone()) {
                                self.selection = Selection::None;
                            }
                            if self
                                .file_material
                                .as_ref()
                                .is_some_and(|fm| fm.path == path)
                            {
                                self.file_material = None;
                            }
                            self.open_editors.retain(|e| e.path != path);
                        }
                        Err(e) => tracing::error!("deleting {}: {e:#}", path.display()),
                    }
                }
                EditorAction::SetSkybox(path) => {
                    let rel = path
                        .strip_prefix(&self.project_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    self.scene.skybox = Some(rel);
                    self.apply_skybox();
                }
                EditorAction::ClearSkybox => {
                    self.scene.skybox = None;
                    self.apply_skybox();
                }
                EditorAction::TogglePlay => self.toggle_play(),
                EditorAction::CreatePluginCrate => {
                    match plugins::create_template(&self.project_root) {
                        Ok(dir) => tracing::info!("component plugin at {}", dir.display()),
                        Err(e) => tracing::error!("creating plugin crate: {e:#}"),
                    }
                }
                EditorAction::ReloadPlugins => {
                    match self
                        .plugins
                        .build_and_load(&self.project_root, &mut self.components)
                    {
                        Ok(names) => {
                            self.plugin_build_error = None;
                            // Old instances point at the previous build:
                            // re-create every component from serialized
                            // state through the fresh registry.
                            for object in &mut self.scene.objects {
                                let saved = object.save_components();
                                object.load_components(&saved, &self.components);
                            }
                            tracing::info!("reloaded plugins: {names:?}");
                        }
                        Err(e) => {
                            tracing::error!("reloading plugins: {e:#}");
                            self.plugin_build_error = Some(format!("{e:#}"));
                        }
                    }
                }
                EditorAction::StartLook => self.set_looking(true),
                EditorAction::Dolly(amount) => self.camera.dolly(amount),
                EditorAction::FocusSelected => {
                    if let Selection::Object(i) = self.selection {
                        let (center, radius) = self.scene.object_bounds(i);
                        self.camera.focus(center, radius);
                    }
                }
                EditorAction::SaveScene(path) => {
                    let path = path
                        .or_else(|| self.current_scene_path.clone())
                        .unwrap_or_else(|| self.project_root.join(&self.scene_name_input));
                    // Materialize material associations: every material the
                    // scene's objects reference gets a real `.material` file
                    // (created under materials/ if missing, refreshed
                    // otherwise), so the saved scene points at assets.
                    // Exception: imported materials with embedded textures
                    // stay inline — a .material file can't carry embedded
                    // textures yet, and converting them would drop the
                    // textures on reload.
                    let mut referenced: Vec<usize> = self
                        .scene
                        .objects
                        .iter()
                        .filter_map(|o| o.render.map(|r| r.material))
                        .collect();
                    referenced.sort_unstable();
                    referenced.dedup();
                    for index in referenced {
                        let entry = &self.scene.materials[index];
                        if entry.file.is_none() && entry.embedded_textures {
                            continue;
                        }
                        self.save_scene_material(index);
                    }
                    let file = self.scene.to_scene_file(&self.project_root, &self.shaders);
                    match citrus_assets::save_scene_file(&path, &file) {
                        Ok(()) => {
                            tracing::info!("scene saved to {}", path.display());
                            self.current_scene_path = Some(path);
                            self.save_project();
                        }
                        Err(e) => tracing::error!("saving scene: {e:#}"),
                    }
                }
            }
        }
    }

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("citrus-menubar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New Scene").clicked() {
                        self.actions.push(EditorAction::NewScene);
                        ui.close();
                    }
                    ui.menu_button("Open Scene", |ui| {
                        let scenes = scan_project_files(&self.project_root, "scene");
                        if scenes.is_empty() {
                            ui.label(
                                egui::RichText::new("no .scene files in the project")
                                    .small()
                                    .weak(),
                            );
                        }
                        for rel in scenes {
                            if ui.button(&rel).clicked() {
                                self.actions
                                    .push(EditorAction::LoadScene(self.project_root.join(&rel)));
                                ui.close();
                            }
                        }
                    });
                    ui.separator();
                    if ui.button("Save Scene        Ctrl+S").clicked() {
                        self.actions.push(EditorAction::SaveScene(None));
                        ui.close();
                    }
                    ui.menu_button("Save Scene As…", |ui| {
                        ui.text_edit_singleline(&mut self.scene_name_input);
                        if ui.button("Save").clicked() {
                            let path = self.project_root.join(&self.scene_name_input);
                            self.actions.push(EditorAction::SaveScene(Some(path)));
                            ui.close();
                        }
                    });
                    ui.separator();
                    ui.label(
                        egui::RichText::new("Import: double-click a model in Files")
                            .small()
                            .weak(),
                    );
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui
                        .add_enabled(
                            self.undo_stack.can_undo(),
                            egui::Button::new("Undo        Ctrl+Z"),
                        )
                        .clicked()
                    {
                        self.actions.push(EditorAction::Undo);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.undo_stack.can_redo(),
                            egui::Button::new("Redo        Ctrl+Shift+Z"),
                        )
                        .clicked()
                    {
                        self.actions.push(EditorAction::Redo);
                        ui.close();
                    }
                });
                ui.menu_button("Object", |ui| {
                    use citrus_assets::{ObjectSource, PrimitiveShape};
                    if ui.button("Create Empty").clicked() {
                        self.actions.push(EditorAction::Spawn(ObjectSource::Empty));
                        ui.close();
                    }
                    if ui.button("Create Camera").clicked() {
                        self.actions.push(EditorAction::Spawn(ObjectSource::Camera));
                        ui.close();
                    }
                    ui.menu_button("Create Light", |ui| {
                        for kind in citrus_editor::LightKind::ALL {
                            if ui.button(kind.label()).clicked() {
                                self.actions.push(EditorAction::SpawnLight(kind));
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Light Probe Volume").clicked() {
                            self.actions.push(EditorAction::SpawnProbeVolume);
                            ui.close();
                        }
                    });
                    ui.separator();
                    for shape in [
                        PrimitiveShape::Cube,
                        PrimitiveShape::Sphere,
                        PrimitiveShape::Capsule,
                        PrimitiveShape::Plane,
                    ] {
                        if ui.button(shape.label()).clicked() {
                            self.actions
                                .push(EditorAction::Spawn(ObjectSource::Primitive { shape }));
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Tools", |ui| {
                    for (tool, label) in [
                        (GizmoTool::Move, "Move        G"),
                        (GizmoTool::Rotate, "Rotate      R"),
                        (GizmoTool::Scale, "Scale       S"),
                    ] {
                        if ui.radio(self.gizmo.tool == tool, label).clicked() {
                            self.gizmo.tool = tool;
                        }
                    }
                    ui.separator();
                    let has_plugins = plugins::PluginHost::any_plugins(&self.project_root);
                    if !has_plugins && ui.button("Create Component Plugin").clicked() {
                        self.actions.push(EditorAction::CreatePluginCrate);
                        ui.close();
                    }
                    if has_plugins
                        && ui
                            .button("Build & Reload Components")
                            .on_hover_text(
                                "cargo-builds plugins/ and hot-reloads them (blocks the UI)",
                            )
                            .clicked()
                    {
                        self.actions.push(EditorAction::ReloadPlugins);
                        ui.close();
                    }
                });
                // CloseOnClickOutside lets you toggle several checkboxes without
                // the menu snapping shut after each click.
                egui::containers::menu::MenuButton::new("View")
                    .config(
                        egui::containers::menu::MenuConfig::new()
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| {
                        ui.checkbox(&mut self.show_stats, "FPS in menu bar");
                        ui.checkbox(&mut self.show_stats_overlay, "Render stats overlay");
                        if let Some(renderer) = self.renderer.as_mut() {
                            let mut vsync = renderer.vsync();
                            if ui
                                .checkbox(&mut vsync, "VSync")
                                .on_hover_text("Off = uncapped frame rate (real numbers)")
                                .changed()
                            {
                                renderer.set_vsync(vsync);
                            }
                        }
                        if ui
                            .add_enabled(
                                self.scene.skybox.is_some(),
                                egui::Button::new("Clear Skybox"),
                            )
                            .on_hover_text("Revert to the procedural gradient sky")
                            .clicked()
                        {
                            self.actions.push(EditorAction::ClearSkybox);
                            ui.close();
                        }
                        if ui.button("Reset Layout").clicked() {
                            self.dock_state = default_layout();
                            ui.close();
                        }
                    });
                ui.menu_button("Windows", |ui| {
                    ui.label(egui::RichText::new("Open / focus a panel").small().weak());
                    for (tab, label) in [
                        (Tab::Viewport, "Viewport"),
                        (Tab::Camera, "Camera"),
                        (Tab::Scene, "Scene"),
                        (Tab::Inspector, "Inspector"),
                        (Tab::Environment, "Environment"),
                        (Tab::Files, "Files"),
                        (Tab::Log, "Log"),
                    ] {
                        let open = self.dock_state.iter_all_tabs().any(|(_, t)| *t == tab);
                        if ui.selectable_label(open, label).clicked() {
                            if let Some(loc) = self.dock_state.find_tab(&tab) {
                                self.dock_state.set_active_tab(loc);
                            } else {
                                self.dock_state.push_to_focused_leaf(tab);
                            }
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Help", |ui| {
                    ui.checkbox(&mut self.show_help, "Controls");
                });

                ui.separator();
                let label = if self.playing { "⏹ Stop" } else { "▶ Play" };
                let button = egui::Button::new(label).fill(if self.playing {
                    ui.visuals().selection.bg_fill
                } else {
                    ui.visuals().widgets.inactive.bg_fill
                });
                if ui
                    .add(button)
                    .on_hover_text("Run components; Stop restores the scene")
                    .clicked()
                {
                    self.actions.push(EditorAction::TogglePlay);
                }

                if self.show_stats {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{:.0} fps · {:.2} ms",
                                self.stats.fps, self.stats.frame_ms
                            ))
                            .monospace()
                            .weak(),
                        );
                    });
                }
            });
        });

        if let Some(error) = &self.plugin_build_error {
            let mut open = true;
            egui::Window::new("Component build failed")
                .open(&mut open)
                .default_width(560.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::both().max_height(360.0).show(ui, |ui| {
                        ui.label(egui::RichText::new(error).small().monospace());
                    });
                });
            if !open {
                self.plugin_build_error = None;
            }
        }

        if self.show_help {
            egui::Window::new("Controls")
                .open(&mut self.show_help)
                .show(ctx, |ui| {
                    ui.label("Left click (no drag) — select object · Escape — deselect");
                    ui.label("Left drag — orbit selection or viewport center (Alt forces orbit over the gizmo)");
                    ui.label("F — focus the selected object");
                    ui.label("Right mouse (hold) — look · WASD fly · Q/E down/up · Shift fast");
                    ui.label("Middle drag — pan · Scroll — dolly");
                    ui.label("G / R / S — gizmo move / rotate / scale (buttons top-left)");
                    ui.label("Ctrl while dragging — snap to grid (controls top-center)");
                    ui.label("Ctrl+Z / Ctrl+Shift+Z — undo / redo");
                    ui.label("Drag a .material from Files onto a mesh or a material slot");
                    ui.label("Double-click a model file in Files to import it");
                });
        }
    }

    /// Open (or focus) a code/text file in a dockable editor tab.
    fn open_code_file(&mut self, path: PathBuf) {
        if !self.open_editors.iter().any(|e| e.path == path) {
            let language = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("txt")
                .to_owned();
            match std::fs::read_to_string(&path) {
                Ok(text) if text.len() <= 2 * 1024 * 1024 => {
                    // Rust files get language-server diagnostics.
                    if language == "rs"
                        && let Some(lsp) = self.ensure_lsp()
                    {
                        lsp.open(&path, "rust", &text);
                    }
                    self.open_editors.push(OpenEditor {
                        path: path.clone(),
                        text,
                        dirty: false,
                        language,
                        diagnostics: Vec::new(),
                        last_edit: None,
                        lsp_dirty: false,
                        completion: None,
                        hover: None,
                        goto: None,
                    });
                }
                Ok(_) => {
                    tracing::warn!("file too large for the code editor");
                    return;
                }
                Err(e) => {
                    tracing::error!("opening file: {e:#}");
                    return;
                }
            }
        }
        let tab = Tab::Code(path);
        if let Some(loc) = self.dock_state.find_tab(&tab) {
            self.dock_state.set_active_tab(loc);
        } else {
            // Open alongside the Viewport so code sits in the main work area.
            if let Some((surface, node, _)) = self.dock_state.find_tab(&Tab::Viewport) {
                self.dock_state
                    .set_focused_node_and_surface((surface, node));
            }
            self.dock_state.push_to_focused_leaf(tab);
        }
    }

    /// Close the focused code tab (Ctrl+W), dropping its editor buffer.
    fn close_focused_code_tab(&mut self) {
        let path = self
            .dock_state
            .find_active_focused()
            .and_then(|(_, t)| match t {
                Tab::Code(p) => Some(p.clone()),
                _ => None,
            });
        if let Some(path) = path {
            if let Some(loc) = self.dock_state.find_tab(&Tab::Code(path.clone())) {
                self.dock_state.remove_tab(loc);
            }
            self.open_editors.retain(|e| e.path != path);
        }
    }

    /// Lazily spawn the Rust language server (rooted at the project), reusing
    /// it across files. Returns None if it isn't available.
    fn ensure_lsp(&mut self) -> Option<&mut lsp::LspClient> {
        if self.lsp.is_none() && !self.lsp_failed {
            match lsp::LspClient::spawn("rust-analyzer", &self.project_root) {
                Ok(client) => {
                    tracing::info!("started rust-analyzer");
                    self.lsp = Some(client);
                }
                Err(e) => {
                    tracing::warn!("rust-analyzer unavailable ({e}); no Rust diagnostics");
                    self.lsp_failed = true;
                }
            }
        }
        self.lsp.as_mut()
    }

    /// Poll the language server and push any new edits as `didChange`, then
    /// route diagnostics to the matching editor. Called once per frame.
    fn pump_lsp(&mut self) {
        // Send buffered changes.
        let changed: Vec<(PathBuf, String)> = self
            .open_editors
            .iter_mut()
            .filter(|e| e.lsp_dirty && e.language == "rs")
            .map(|e| {
                e.lsp_dirty = false;
                (e.path.clone(), e.text.clone())
            })
            .collect();
        // Definition jumps are collected here and applied after the LSP borrow
        // ends, since opening a target file needs `&mut self`.
        let mut goto_targets: Vec<(PathBuf, u32, u32)> = Vec::new();
        if let Some(lsp) = self.lsp.as_mut() {
            for (path, text) in changed {
                lsp.change(&path, "rust", &text);
            }
            // Drain server events → editors.
            for event in lsp.poll() {
                match event {
                    lsp::LspEvent::Diagnostics { path, diags } => {
                        if let Some(editor) = self.open_editors.iter_mut().find(|e| e.path == path)
                        {
                            editor.diagnostics = diags
                                .into_iter()
                                .map(|d| CodeDiagnostic {
                                    level: if d.severity <= 1 { "error" } else { "warning" }
                                        .to_owned(),
                                    file: path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_default(),
                                    line: d.line + 1,
                                    message: d.message,
                                })
                                .collect();
                        }
                    }
                    lsp::LspEvent::Response { id, result } => {
                        if let Some(kind) = self.lsp_requests.remove(&id) {
                            if matches!(kind, LspRequestKind::Definition) {
                                if let Some(target) = parse_definition(&result) {
                                    goto_targets.push(target);
                                }
                            } else {
                                apply_lsp_response(&mut self.open_editors, kind, result);
                            }
                        }
                    }
                    lsp::LspEvent::Initialized => {}
                }
            }
        }
        // Open + jump to any definition targets (uses &mut self).
        for (path, line, col) in goto_targets {
            self.open_code_file(path.clone());
            if let Some(editor) = self.open_editors.iter_mut().find(|e| e.path == path) {
                editor.goto = Some((line, col));
            }
        }
    }

    fn toggle_play(&mut self) {
        if self.playing {
            if let Some(snapshot) = self.play_snapshot.take() {
                let registry = &self.components;
                for (object, state) in self.scene.objects.iter_mut().zip(snapshot) {
                    object.translation = state.translation;
                    object.rotation = state.rotation;
                    object.scale = state.scale;
                    object.load_components(&state.components, registry);
                }
            }
            self.playing = false;
        } else {
            self.play_snapshot = Some(self.scene.objects.iter().map(object_state).collect());
            self.playing = true;
            self.scene
                .start_components(self.start.elapsed().as_secs_f32());
        }
    }

    /// Push the scene's skybox (or the procedural sky) to the renderer.
    fn apply_skybox(&mut self) {
        let skybox = self.scene.skybox.clone();
        let project_root = self.project_root.clone();
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        match skybox {
            Some(rel) => {
                let abs = project_root.join(&rel);
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

    /// Persist project.citrus: current settings + the open scene.
    fn save_project(&mut self) {
        if let Some(renderer) = &self.renderer {
            self.project.settings.vsync = renderer.vsync();
        }
        self.project.settings.show_stats = self.show_stats;
        self.project.settings.show_stats_overlay = self.show_stats_overlay;
        self.project.settings.snap = self.gizmo.snap;
        self.project.settings.grid_size = self.gizmo.grid_size;
        self.project.last_scene = self
            .current_scene_path
            .as_ref()
            .map(|p| relative_to(p, &self.project_root));
        if let Err(e) = citrus_assets::save_project_file(&self.project_root, &self.project) {
            tracing::error!("saving project.citrus: {e:#}");
        }
    }

    /// Rescan project `.frag` files for the shader picker and hot-reload any
    /// loaded shaders whose files changed. Called at most every 2 seconds.
    fn refresh_shaders(&mut self) {
        if self
            .last_shader_scan
            .is_some_and(|t| t.elapsed().as_secs_f32() < 2.0)
        {
            return;
        }
        self.last_shader_scan = Some(Instant::now());
        self.shader_files = scan_project_files(&self.project_root, "frag");
        if let Some(renderer) = self.renderer.as_mut() {
            let changed = self.shaders.poll_reload(renderer, &self.project_root);
            if !changed.is_empty() {
                self.scene.reapply_materials_using(
                    renderer,
                    &mut self.shaders,
                    &self.project_root,
                    &changed,
                );
            }
        }
    }

    /// Reflected shader info for the selection's material, resolved before
    /// the UI runs (compiles the shader on first use). Also initializes the
    /// model's custom values from the shader defaults.
    fn selected_shader_info(&mut self) -> Option<ShaderUiInfo> {
        let shader = match &self.selection {
            Selection::Object(i) => {
                let object = self.scene.objects.get(*i)?;
                let material = object.render.map(|r| r.material)?;
                self.scene.materials[material].model.shader.clone()
            }
            Selection::File(_) => self.file_material.as_ref()?.model.shader.clone(),
            Selection::None => return None,
        };
        if shader == "standard" {
            return None;
        }
        let renderer = self.renderer.as_mut()?;
        let entry = self.shaders.resolve(renderer, &self.project_root, &shader);
        let defaults = entry.defaults();
        let ui = entry.ui.clone();
        let has_source = entry.source.is_some();
        if has_source {
            let model = match &self.selection {
                Selection::Object(i) => {
                    let material = self.scene.objects[*i].render.map(|r| r.material)?;
                    &mut self.scene.materials[material].model
                }
                Selection::File(_) => &mut self.file_material.as_mut()?.model,
                Selection::None => return None,
            };
            if model.custom_values.len() != defaults.len() {
                model.custom_values = defaults;
            }
        }
        Some(ui)
    }

    /// Snapshot the selection's editable state before the UI runs, so edits
    /// can be diffed into undo entries afterwards.
    fn capture_edit_snapshot(&self) -> EditSnapshot {
        match &self.selection {
            Selection::Object(i) if *i < self.scene.objects.len() => {
                let o = &self.scene.objects[*i];
                EditSnapshot::Object {
                    index: *i,
                    state: object_state(o),
                    material: o.render.map(|r| r.material),
                    model: o
                        .render
                        .map(|r| Box::new(self.scene.materials[r.material].model.clone())),
                }
            }
            Selection::File(path) => match &self.file_material {
                Some(fm) if fm.path == *path => EditSnapshot::File {
                    path: path.clone(),
                    model: Box::new(fm.model.clone()),
                },
                _ => EditSnapshot::None,
            },
            _ => EditSnapshot::None,
        }
    }

    /// Diff the post-frame state against the snapshot and record undo
    /// entries. Continuous gestures coalesce inside the stack.
    fn record_edits(&mut self, pre: EditSnapshot) {
        if std::mem::take(&mut self.suppress_undo_record) {
            return;
        }
        match pre {
            EditSnapshot::Object {
                index,
                state,
                material,
                model,
            } if index < self.scene.objects.len() => {
                let o = &self.scene.objects[index];
                let now = object_state(o);
                if now != state {
                    self.undo_stack.record(UndoEntry::Object {
                        index,
                        before: state,
                        after: now,
                    });
                }
                if let (Some(material), Some(model)) = (material, model)
                    && material < self.scene.materials.len()
                {
                    let current = &self.scene.materials[material].model;
                    if *current != *model {
                        self.dirty_materials.insert(material);
                        self.last_material_edit = Some(Instant::now());
                        self.undo_stack.record(UndoEntry::Material {
                            index: material,
                            before: model,
                            after: Box::new(current.clone()),
                        });
                    }
                }
            }
            EditSnapshot::File { path, model } => {
                if let Some(fm) = &self.file_material
                    && fm.path == path
                    && fm.model != *model
                {
                    self.last_material_edit = Some(Instant::now());
                    self.undo_stack.record(UndoEntry::FileMaterial {
                        path,
                        before: model,
                        after: Box::new(fm.model.clone()),
                    });
                }
            }
            _ => {}
        }
    }

    /// Auto-save edited materials once the edit gesture settles. Materials
    /// without a backing file get one created under `materials/`.
    fn autosave_materials(&mut self) {
        // Code editors: save each 1s after its last keystroke (saving .frag
        // files also triggers shader hot reload — live shader editing).
        let to_save: Vec<PathBuf> = self
            .open_editors
            .iter()
            .filter(|e| {
                e.dirty
                    && e.last_edit
                        .is_some_and(|t| t.elapsed().as_secs_f32() >= 1.0)
            })
            .map(|e| e.path.clone())
            .collect();
        for path in to_save {
            self.actions.push(EditorAction::SaveOpenEditor(path));
        }
        if self
            .last_material_edit
            .is_none_or(|t| t.elapsed().as_secs_f32() < 0.8)
        {
            return;
        }
        self.last_material_edit = None;
        // A `.material` file open in the Inspector saves through the same
        // path as the manual 💾 button.
        if self.file_material.as_ref().is_some_and(|fm| fm.dirty) {
            self.actions.push(EditorAction::SaveFileMaterial);
        }
        for index in std::mem::take(&mut self.dirty_materials) {
            if index < self.scene.materials.len() {
                self.save_scene_material(index);
            }
        }
    }

    /// Persist one scene material to its `.material` file, creating
    /// `materials/<name>.material` if it has none yet. Texture paths in an
    /// existing file are preserved (the editor doesn't model them yet).
    fn save_scene_material(&mut self, index: usize) {
        let model = self.scene.materials[index].model.clone();
        let path = match &self.scene.materials[index].file {
            Some(path) => path.clone(),
            None => {
                let dir = self.project_root.join("materials");
                let stem: String = model
                    .name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() {
                            c.to_ascii_lowercase()
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let stem = stem.trim_matches('_');
                let path = unique_path(
                    &dir,
                    if stem.is_empty() { "material" } else { stem },
                    "material",
                );
                self.scene.set_material_file(index, path.clone());
                path
            }
        };
        let mut file = citrus_assets::load_material_file(&path).unwrap_or_else(|_| {
            citrus_assets::MaterialFile {
                name: model.name.clone(),
                shader: "standard".into(),
                params: Default::default(),
                features: Default::default(),
                render_queue: None,
                textures: Default::default(),
                custom: Default::default(),
            }
        });
        let (params, features) = material_from_model(&model);
        file.name = model.name.clone();
        file.shader = model.shader.clone();
        file.params = params;
        file.features = features;
        file.render_queue = Some(model.render_queue);
        file.custom = if model.shader != "standard" {
            self.shaders
                .get(&model.shader)
                .and_then(|e| e.source.as_ref())
                .map(|s| s.unpack(&model.custom_values))
                .unwrap_or(file.custom)
        } else {
            Default::default()
        };
        match citrus_assets::save_material_file(&path, &file) {
            Ok(()) => tracing::info!("auto-saved material to {}", path.display()),
            Err(e) => tracing::error!("auto-saving material: {e:#}"),
        }
    }

    /// Apply one history entry (true = undo, false = redo).
    fn apply_history(&mut self, undo: bool) {
        let Some(entry) = (if undo {
            self.undo_stack.pop_undo()
        } else {
            self.undo_stack.pop_redo()
        }) else {
            return;
        };
        self.suppress_undo_record = true;
        match entry {
            UndoEntry::Object {
                index,
                before,
                after,
            } => {
                let state = if undo { before } else { after };
                if let Some(o) = self.scene.objects.get_mut(index) {
                    o.name = state.name;
                    o.translation = state.translation;
                    o.rotation = state.rotation;
                    o.scale = state.scale;
                    o.load_components(&state.components, &self.components);
                }
            }
            UndoEntry::Material {
                index,
                before,
                after,
            } => {
                let model = if undo { before } else { after };
                if index < self.scene.materials.len() {
                    self.scene.materials[index].model = *model;
                    self.dirty_materials.insert(index);
                    self.last_material_edit = Some(Instant::now());
                    if let Some(renderer) = self.renderer.as_mut() {
                        self.scene.apply_material(
                            renderer,
                            &mut self.shaders,
                            &self.project_root,
                            index,
                        );
                    }
                }
            }
            UndoEntry::Assign {
                object,
                before,
                after,
            } => {
                let material = if undo { before } else { after };
                if object < self.scene.objects.len()
                    && material < self.scene.materials.len()
                    && let Some(render) = &mut self.scene.objects[object].render
                {
                    render.material = material;
                }
            }
            UndoEntry::FileMaterial {
                path,
                before,
                after,
            } => {
                let model = if undo { before } else { after };
                if let Some(fm) = &mut self.file_material
                    && fm.path == path
                {
                    fm.model = *model;
                    fm.dirty = true;
                }
            }
        }
    }

    /// Render statistics overlay, bottom-left of the viewport.
    fn stats_overlay(&self, ctx: &egui::Context, stats: citrus_render::RenderStats) {
        if !self.viewport_rect.is_finite() {
            return; // first frame: the viewport tab hasn't reported its rect yet
        }
        let pos = egui::pos2(
            self.viewport_rect.left() + 8.0,
            self.viewport_rect.bottom() - 8.0,
        );
        egui::Area::new(egui::Id::new("citrus-stats-overlay"))
            .order(egui::Order::Middle)
            .pivot(egui::Align2::LEFT_BOTTOM)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let row = |ui: &mut egui::Ui, label: &str, value: String| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(label)
                                    .size(15.0)
                                    .color(egui::Color32::WHITE),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(value)
                                            .size(15.0)
                                            .monospace()
                                            .color(egui::Color32::WHITE),
                                    );
                                },
                            );
                        });
                    };
                    ui.set_min_width(220.0);
                    row(
                        ui,
                        "Frame",
                        format!("{:.1} ms ({:.0} fps)", self.stats.frame_ms, self.stats.fps),
                    );
                    row(ui, "Draw calls", stats.draw_calls.to_string());
                    row(ui, "  opaque", stats.opaque_draws.to_string());
                    row(ui, "  transparent", format!("+{}", stats.transparent_draws));
                    row(ui, "  outline", format!("+{}", stats.outline_draws));
                    if stats.error_draws > 0 {
                        row(ui, "  error", format!("+{}", stats.error_draws));
                    }
                    row(ui, "Materials drawn", stats.materials_drawn.to_string());
                    row(ui, "Pipeline binds", stats.pipeline_binds.to_string());
                    row(ui, "Shader variants", stats.pipeline_variants.to_string());
                    // Reflections / probes / shadows report here once those
                    // passes exist (see TODO.md).
                });
            });
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        self.stats.tick(dt);
        self.update_camera(dt);
        self.refresh_shaders();

        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(egui_state) = self.egui_state.as_mut() else {
            return;
        };

        let raw_input = egui_state.take_egui_input(&window);
        let size = window.inner_size();
        let aspect = size.width.max(1) as f32 / size.height.max(1) as f32;
        let view = self.camera.view();
        let proj = self.camera.proj(aspect);

        let render_stats = self
            .renderer
            .as_ref()
            .map(|r| r.stats())
            .unwrap_or_default();

        let pre_edit = self.capture_edit_snapshot();
        let shader_info = self.selected_shader_info();
        // Picker entries: standard + every project .frag.
        let mut shader_list: Vec<String> = Vec::with_capacity(self.shader_files.len() + 1);
        shader_list.push("standard".into());
        shader_list.extend(self.shader_files.iter().cloned());

        let egui_ctx = self.egui_ctx.clone();
        let output = egui_ctx.run(raw_input, |ctx| {
            self.menu_bar(ctx);
            if self.show_stats_overlay {
                self.stats_overlay(ctx, render_stats);
            }

            let camera_preview = self
                .renderer
                .as_ref()
                .and_then(|r| r.camera_preview_texture());
            let mut dock_state = std::mem::replace(&mut self.dock_state, DockState::new(vec![]));
            // The focused code tab drives which file's diagnostics the
            // Inspector shows (kept out of the editor to stop layout shift).
            let focused_code = dock_state
                .find_active_focused()
                .and_then(|(_, tab)| match tab {
                    Tab::Code(p) => Some(p.clone()),
                    _ => None,
                });
            let mut tabs = EditorTabs {
                scene: &mut self.scene,
                selection: &mut self.selection,
                inspector: &mut self.inspector,
                inspector_lock_target: &mut self.inspector_lock_target,
                scene_panel: &mut self.scene_panel,
                file_browser: &mut self.file_browser,
                file_material: &mut self.file_material,
                open_editors: &mut self.open_editors,
                focused_code,
                gizmo: &mut self.gizmo,
                actions: &mut self.actions,
                viewport_rect: &mut self.viewport_rect,
                registry: &self.components,
                shader_list: &shader_list,
                shader_info: shader_info.as_ref(),
                camera_preview,
                view,
                proj,
                looking: self.looking,
                log_filter: &mut self.log_filter,
                probe_drag: &mut self.probe_drag,
            };
            DockArea::new(&mut dock_state)
                .style(egui_dock::Style::from_egui(ctx.style().as_ref()))
                // Per-tab close X (only on closeable tabs = Code); no group
                // close-all button.
                .show_close_buttons(true)
                .show_leaf_close_all_buttons(false)
                .show(ctx, &mut tabs);
            self.dock_state = dock_state;
        });

        if let Some(egui_state) = self.egui_state.as_mut() {
            egui_state.handle_platform_output(&window, output.platform_output);
        }
        let primitives = egui_ctx.tessellate(output.shapes, output.pixels_per_point);

        self.process_actions();
        self.pump_lsp();
        let t = self.start.elapsed().as_secs_f32();
        if self.playing {
            // Component-driven motion must not land in undo history; play
            // edits are restored wholesale on Stop anyway.
            self.scene.update_components(dt, t);
        } else {
            self.record_edits(pre_edit);
        }
        self.autosave_materials();
        // Cameras always carry a Camera component (covers spawns, loaded
        // scenes, and scenes from before the component existed).
        self.scene.ensure_camera_components(&self.components);
        self.scene.ensure_light_components(&self.components);
        self.scene.ensure_camera_ids();
        let selected = match self.selection {
            Selection::Object(i) => Some(i),
            _ => None,
        };
        self.scene.sync_draws(selected, 1.0);
        // World sun (from the Environment window) leads the light list; scene
        // Light objects follow.
        let env = self.scene.environment.clone();
        let mut lights = Vec::new();
        if env.sun_enabled {
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
                shadow_bias: 0.003,
            });
        }
        lights.extend(self.scene.gather_lights());
        let world_light = LightData {
            direction: Vec3::from(env.sun_direction).normalize_or(Vec3::NEG_Y),
            color: env.sun_color,
            intensity: if env.sun_enabled {
                env.sun_intensity
            } else {
                0.0
            },
            ambient: [
                env.ambient[0] * env.ambient_intensity,
                env.ambient[1] * env.ambient_intensity,
                env.ambient[2] * env.ambient_intensity,
            ],
        };

        // Render the main-camera preview only while a Camera tab is open.
        let camera_tab_open = self
            .dock_state
            .iter_all_tabs()
            .any(|(_, tab)| matches!(tab, Tab::Camera));
        let camera_preview = if camera_tab_open {
            // Preview target is a fixed 16:9; match its aspect.
            self.scene
                .main_camera_view_proj(16.0 / 9.0)
                .map(|(view, proj, position)| CameraData {
                    view,
                    proj,
                    position,
                })
        } else {
            None
        };

        let shadow_res = env.shadow_resolution.clamp(256, 8192);
        let shadow_pcf_texel = env.shadow_softness.max(0.0) / shadow_res as f32;

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if let Err(e) = renderer.set_shadow_resolution(shadow_res) {
            tracing::error!("setting shadow resolution: {e:#}");
        }
        let frame = FrameInput {
            clear_color: [0.016, 0.016, 0.024, 1.0],
            camera: CameraData {
                view,
                proj,
                position: self.camera.position,
            },
            light: world_light,
            lights: &lights,
            camera_preview,
            draw_skybox: env.skybox_enabled,
            shadow_pcf_texel,
            shadow_distance: env.shadow_distance,
            time: t,
            draws: &self.scene.draws,
            egui: Some(citrus_render::EguiDraw {
                pixels_per_point: output.pixels_per_point,
                primitives,
                textures_delta: output.textures_delta,
            }),
        };
        if let Err(e) = renderer.render(&frame) {
            tracing::error!("render failed: {e:#}");
            event_loop.exit();
        }
        window.request_redraw();
    }
}

/// Dock tab renderer; collects actions for post-UI processing.
struct EditorTabs<'a> {
    scene: &'a mut LoadedScene,
    selection: &'a mut Selection,
    inspector: &'a mut InspectorPanel,
    inspector_lock_target: &'a mut Option<Selection>,
    scene_panel: &'a mut ScenePanel,
    file_browser: &'a mut FileBrowser,
    file_material: &'a mut Option<FileMaterial>,
    open_editors: &'a mut Vec<OpenEditor>,
    /// Path of the focused code tab (drives Inspector diagnostics).
    focused_code: Option<PathBuf>,
    gizmo: &'a mut GizmoState,
    actions: &'a mut Vec<EditorAction>,
    viewport_rect: &'a mut egui::Rect,
    registry: &'a ComponentRegistry,
    shader_list: &'a [String],
    shader_info: Option<&'a ShaderUiInfo>,
    /// Main-camera preview (egui texture + pixel size), if the renderer has
    /// rendered one yet.
    camera_preview: Option<(egui::TextureId, [f32; 2])>,
    view: glam::Mat4,
    proj: glam::Mat4,
    looking: bool,
    log_filter: &'a mut LogFilter,
    probe_drag: &'a mut Option<ProbeDrag>,
}

impl egui_dock::TabViewer for EditorTabs<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match tab {
            Tab::Viewport => "Viewport".into(),
            Tab::Camera => "Camera".into(),
            Tab::Scene => "Scene".into(),
            Tab::Inspector => "Inspector".into(),
            Tab::Environment => "Environment".into(),
            Tab::Files => "Files".into(),
            Tab::Log => "Log".into(),
            Tab::Code(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".to_owned())
                .into(),
        }
    }

    fn clear_background(&self, tab: &Tab) -> bool {
        !matches!(tab, Tab::Viewport)
    }

    fn closeable(&mut self, tab: &mut Tab) -> bool {
        // Only code tabs get a per-tab close X; panels stay put (reopen via
        // the Windows menu).
        matches!(tab, Tab::Code(_))
    }

    fn on_close(&mut self, tab: &mut Tab) -> egui_dock::tab_viewer::OnCloseResponse {
        // Closing a code tab drops its buffer (auto-saved on edit settle).
        if let Tab::Code(path) = tab {
            self.open_editors.retain(|e| &e.path != path);
        }
        egui_dock::tab_viewer::OnCloseResponse::Close
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match tab {
            Tab::Viewport => self.viewport_ui(ui),
            Tab::Camera => self.camera_ui(ui),
            Tab::Environment => self.environment_ui(ui),
            Tab::Scene => {
                let rows: Vec<citrus_editor::SceneObjectRow> = self
                    .scene
                    .objects
                    .iter()
                    .map(|o| citrus_editor::SceneObjectRow {
                        name: o.name.clone(),
                        parent: o.parent,
                        // No icon: egui's bundled font lacks glyphs for the
                        // object kinds (they render as missing-glyph squares),
                        // so the row shows just the name.
                        icon: "",
                        enabled: o.enabled,
                    })
                    .collect();
                let mut selected = match self.selection {
                    Selection::Object(i) => Some(*i),
                    _ => None,
                };
                let response = self.scene_panel.ui(ui, &rows, &mut selected);
                if response.selection_changed {
                    *self.selection = match selected {
                        Some(i) => Selection::Object(i),
                        None => Selection::None,
                    };
                }
                for (child, parent) in response.reparent {
                    self.actions.push(EditorAction::SetParent(child, parent));
                }
                for (child, parent, before) in response.moves {
                    self.actions
                        .push(EditorAction::MoveObject(child, parent, before));
                }
                if let Some(index) = response.delete {
                    self.actions.push(EditorAction::DeleteObject(index));
                }
                if let Some(kind) = response.spawn {
                    use citrus_assets::{ObjectSource, PrimitiveShape};
                    use citrus_editor::SpawnKind;
                    let source = match kind {
                        SpawnKind::Empty => ObjectSource::Empty,
                        SpawnKind::Camera => ObjectSource::Camera,
                        SpawnKind::Light(light_kind) => {
                            self.actions.push(EditorAction::SpawnLight(light_kind));
                            return;
                        }
                        SpawnKind::LightProbeVolume => {
                            self.actions.push(EditorAction::SpawnProbeVolume);
                            return;
                        }
                        SpawnKind::Cube => ObjectSource::Primitive {
                            shape: PrimitiveShape::Cube,
                        },
                        SpawnKind::Sphere => ObjectSource::Primitive {
                            shape: PrimitiveShape::Sphere,
                        },
                        SpawnKind::Capsule => ObjectSource::Primitive {
                            shape: PrimitiveShape::Capsule,
                        },
                        SpawnKind::Plane => ObjectSource::Primitive {
                            shape: PrimitiveShape::Plane,
                        },
                    };
                    self.actions.push(EditorAction::Spawn(source));
                }
            }
            Tab::Code(path) => {
                if let Some(editor) = self.open_editors.iter_mut().find(|e| &e.path == path) {
                    let response = CodeEditor.ui(
                        ui,
                        path,
                        &mut editor.text,
                        &editor.language,
                        editor.dirty,
                        &editor.diagnostics,
                        false,
                        &mut editor.completion,
                        &mut editor.hover,
                        &mut editor.goto,
                    );
                    if response.text_changed {
                        editor.dirty = true;
                        editor.lsp_dirty = true;
                        editor.last_edit = Some(Instant::now());
                    }
                    if response.save_requested {
                        self.actions
                            .push(EditorAction::SaveOpenEditor(path.clone()));
                    }
                    if let Some(cursor) = response.request_completion {
                        self.actions
                            .push(EditorAction::LspCompletion(path.clone(), cursor));
                    }
                    if let Some(cursor) = response.request_hover {
                        self.actions
                            .push(EditorAction::LspHover(path.clone(), cursor));
                    }
                    if let Some(cursor) = response.request_definition {
                        self.actions
                            .push(EditorAction::LspGoto(path.clone(), cursor));
                    }
                } else {
                    ui.label(format!("No editor buffer for {}", path.display()));
                }
            }
            Tab::Inspector => self.inspector_ui(ui),
            Tab::Files => {
                let selected = match self.selection {
                    Selection::File(path) => Some(path.clone()),
                    _ => None,
                };
                let response = self.file_browser.ui(ui, selected.as_deref());
                if let Some(path) = response.clicked {
                    // Single click just selects (Inspector shows file info).
                    self.actions.push(EditorAction::SelectFile(path));
                }
                if let Some(path) = response.activated {
                    // Double click opens by type: models import, code/text open
                    // an editor tab, scenes can be loaded from the Inspector.
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(str::to_lowercase);
                    if matches!(ext.as_deref(), Some("gltf" | "glb" | "fbx")) {
                        self.actions.push(EditorAction::ImportModel(path));
                    } else if ext.as_deref().is_some_and(|e| CODE_EXTENSIONS.contains(&e)) {
                        self.actions.push(EditorAction::OpenCodeFile(path));
                    } else {
                        self.actions.push(EditorAction::SelectFile(path));
                    }
                }
                if let Some(dir) = response.create_material_in {
                    self.actions.push(EditorAction::CreateMaterial(dir));
                }
                if let Some(dir) = response.create_scene_in {
                    self.actions.push(EditorAction::CreateScene(dir));
                }
                if let Some(dir) = response.create_shader_in {
                    self.actions.push(EditorAction::CreateShader(dir));
                }
                if let Some(dir) = response.create_folder_in {
                    self.actions.push(EditorAction::CreateFolder(dir));
                }
                if response.create_component {
                    self.actions.push(EditorAction::CreateComponent);
                }
                if let Some(path) = response.delete {
                    self.actions.push(EditorAction::DeleteFile(path));
                }
                if let Some(path) = response.set_skybox {
                    self.actions.push(EditorAction::SetSkybox(path));
                }
                // Keep the selection and any open .material following
                // renamed/moved files.
                for (old, new) in response.moved {
                    if *self.selection == Selection::File(old.clone()) {
                        *self.selection = Selection::File(new.clone());
                    }
                    if let Some(fm) = self.file_material.as_mut()
                        && fm.path == old
                    {
                        fm.path = new.clone();
                    }
                    for editor in self.open_editors.iter_mut() {
                        if editor.path == old {
                            editor.path = new.clone();
                        }
                    }
                }
            }
            Tab::Log => self.log_ui(ui),
        }
    }
}

impl EditorTabs<'_> {
    /// Screen position of a world point (full-window NDC mapping, matching the
    /// scene swapchain). `None` if behind the camera.
    fn world_to_screen(&self, world: Vec3, full_rect: egui::Rect) -> Option<egui::Pos2> {
        let clip = self.proj * self.view * world.extend(1.0);
        if clip.w <= 0.001 {
            return None;
        }
        let ndc = clip.truncate() / clip.w;
        Some(egui::pos2(
            full_rect.left() + (ndc.x * 0.5 + 0.5) * full_rect.width(),
            full_rect.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * full_rect.height(),
        ))
    }

    /// Drive the Light Probe Volume face-handle resize: start a drag when the
    /// press lands on a face handle, apply size changes while dragging, and end
    /// on release. Box resize keeps the opposite face fixed (the origin shifts
    /// by half the size delta).
    fn probe_resize_interaction(
        &mut self,
        response: &egui::Response,
        cursor: Option<egui::Pos2>,
        alt: bool,
    ) {
        let full_rect = response.ctx.viewport_rect();

        // Continue / end an in-progress drag.
        if let Some(drag) = self.probe_drag.as_ref() {
            if !response.dragged_by(egui::PointerButton::Primary) {
                *self.probe_drag = None;
                return;
            }
            let Some(cur) = cursor else { return };
            let delta = cur - drag.start_cursor;
            let len2 = drag.screen_axis.length_sq();
            let meters_world = if len2 > 1.0e-6 {
                delta.dot(drag.screen_axis) / len2
            } else {
                0.0
            };
            let local_delta = if drag.scale_a.abs() > 1.0e-5 {
                meters_world / drag.scale_a
            } else {
                0.0
            };
            let new_size = (drag.start_size[drag.axis] + local_delta).max(0.1);
            let applied_world = (new_size - drag.start_size[drag.axis]) * drag.scale_a;
            let new_origin = drag.start_origin_world + drag.world_axis * (applied_world * 0.5);
            let (object, axis) = (drag.object, drag.axis);

            if let Some(volume) = self.scene.objects[object]
                .components
                .iter_mut()
                .find_map(|c| {
                    c.as_any_mut()
                        .downcast_mut::<citrus_editor::LightProbeVolume>()
                })
            {
                volume.size[axis] = new_size;
            }
            // Shift the origin so the opposite face stays put (parent-aware).
            let parent_world = self.scene.objects[object]
                .parent
                .map_or(glam::Mat4::IDENTITY, |p| self.scene.world_transform(p));
            self.scene.objects[object].translation =
                parent_world.inverse().transform_point3(new_origin);
            return;
        }

        // Maybe start a drag: a primary press on a face handle, when the
        // transform gizmo isn't claiming the same spot.
        if !response.drag_started_by(egui::PointerButton::Primary) || alt {
            return;
        }
        let Selection::Object(i) = *self.selection else {
            return;
        };
        let Some(press) = cursor else { return };
        if self.gizmo.pick_preview(press) {
            return; // the move/rotate/scale gizmo owns this press
        }
        let Some(volume) = self.scene.objects[i]
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<citrus_editor::LightProbeVolume>())
        else {
            return;
        };
        let world = self.scene.world_transform(i);
        let (w_scale, w_rot, w_trans) = world.to_scale_rotation_translation();
        let half = Vec3::from(volume.size) * 0.5;
        let size = Vec3::from(volume.size);
        let axes = [Vec3::X, Vec3::Y, Vec3::Z];
        let mut best: Option<(f32, ProbeDrag)> = None;
        for axis in 0..3 {
            for sign in [-1.0f32, 1.0] {
                let face_world = world.transform_point3(axes[axis] * (sign * half[axis]));
                let Some(face_screen) = self.world_to_screen(face_world, full_rect) else {
                    continue;
                };
                let dist = face_screen.distance(press);
                if dist > 9.0 {
                    continue;
                }
                let world_axis = (w_rot * axes[axis]) * sign;
                let Some(plus) = self.world_to_screen(face_world + world_axis, full_rect) else {
                    continue;
                };
                let drag = ProbeDrag {
                    object: i,
                    axis,
                    start_size: size,
                    start_origin_world: w_trans,
                    world_axis,
                    scale_a: w_scale[axis],
                    screen_axis: plus - face_screen,
                    start_cursor: press,
                };
                if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                    best = Some((dist, drag));
                }
            }
        }
        if let Some((_, drag)) = best {
            *self.probe_drag = Some(drag);
        }
    }

    /// The Log tab: a filterable console mirroring every tracing event (engine
    /// logs + routed compiler/shader errors).
    fn log_ui(&mut self, ui: &mut egui::Ui) {
        let f = &mut *self.log_filter;
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Levels").small().weak());
            ui.toggle_value(&mut f.error, egui::RichText::new("Error").color(LOG_ERROR));
            ui.toggle_value(&mut f.warn, egui::RichText::new("Warn").color(LOG_WARN));
            ui.toggle_value(&mut f.info, egui::RichText::new("Info").color(LOG_INFO));
            ui.toggle_value(&mut f.debug, egui::RichText::new("Debug").color(LOG_DEBUG));
            ui.toggle_value(&mut f.trace, egui::RichText::new("Trace").color(LOG_TRACE));
            ui.separator();
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut f.search)
                    .hint_text("filter…")
                    .desired_width(140.0),
            );
            if ui.button("✖").on_hover_text("Clear filter").clicked() {
                f.search.clear();
            }
            ui.separator();
            ui.checkbox(&mut f.autoscroll, "Follow");
            if ui
                .button("🗑 Clear")
                .on_hover_text("Clear the log")
                .clicked()
            {
                log_capture::store().lock().unwrap().entries.clear();
            }
        });
        ui.separator();

        let needle = f.search.to_lowercase();
        // Snapshot the matching entries under the lock, then release it before
        // rendering. Multi-line messages (compiler output) are flattened to one
        // display row per physical line so the virtualized list keeps a uniform
        // row height; continuation lines are indented under their header.
        let mut rows: Vec<(egui::Color32, String)> = Vec::new();
        {
            let ring = log_capture::store().lock().unwrap();
            for e in ring.entries.iter().filter(|e| f.shows(e.level)) {
                if !needle.is_empty()
                    && !e.message.to_lowercase().contains(&needle)
                    && !e.target.to_lowercase().contains(&needle)
                {
                    continue;
                }
                let (color, tag) = match e.level {
                    tracing::Level::ERROR => (LOG_ERROR, "ERROR"),
                    tracing::Level::WARN => (LOG_WARN, "WARN "),
                    tracing::Level::INFO => (LOG_INFO, "INFO "),
                    tracing::Level::DEBUG => (LOG_DEBUG, "DEBUG"),
                    tracing::Level::TRACE => (LOG_TRACE, "TRACE"),
                };
                let short = e.target.rsplit("::").next().unwrap_or(&e.target);
                let mut lines = e.message.lines();
                let first = lines.next().unwrap_or("");
                rows.push((color, format!("{:8.2}  {tag}  {short}: {first}", e.seconds)));
                for cont in lines {
                    rows.push((color, format!("{:>20}{cont}", "")));
                }
            }
        }

        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(f.autoscroll)
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                ui.spacing_mut().item_spacing.y = 0.0;
                for (color, line) in &rows[range] {
                    ui.label(egui::RichText::new(line).monospace().color(*color));
                }
            });
    }

    /// The Camera tab: shows the main camera's offscreen preview, scaled to
    /// fit while preserving the target's aspect.
    fn camera_ui(&mut self, ui: &mut egui::Ui) {
        let has_camera = self.scene.main_camera().is_some();
        match self.camera_preview {
            Some((texture, size)) if has_camera => {
                let avail = ui.available_size();
                let aspect = size[0] / size[1].max(1.0);
                let mut w = avail.x;
                let mut h = w / aspect;
                if h > avail.y {
                    h = avail.y;
                    w = h * aspect;
                }
                ui.centered_and_justified(|ui| {
                    ui.add(
                        egui::Image::new(egui::load::SizedTexture::new(texture, egui::vec2(w, h)))
                            .maintain_aspect_ratio(true),
                    );
                });
            }
            _ => {
                ui.centered_and_justified(|ui| {
                    let msg = if has_camera {
                        "Rendering main camera…"
                    } else {
                        "No camera in the scene.\nAdd one from the Scene tree → Camera."
                    };
                    ui.label(egui::RichText::new(msg).weak());
                });
            }
        }
    }

    /// The Environment tab: world sun, ambient, and skybox setup.
    fn environment_ui(&mut self, ui: &mut egui::Ui) {
        use egui::{DragValue, RichText};
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.heading("Environment");
                ui.separator();

                {
                    let env = &mut self.scene.environment;
                    ui.label(RichText::new("World Light (Sun / Moon)").strong());
                    ui.checkbox(&mut env.sun_enabled, "Enabled");
                    ui.add_enabled_ui(env.sun_enabled, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Color");
                            ui.color_edit_button_rgb(&mut env.sun_color);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Intensity");
                            ui.add(
                                DragValue::new(&mut env.sun_intensity)
                                    .speed(0.05)
                                    .range(0.0..=50.0),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("Direction");
                            for (lbl, i) in [("X", 0usize), ("Y", 1), ("Z", 2)] {
                                ui.label(RichText::new(lbl).weak());
                                ui.add(DragValue::new(&mut env.sun_direction[i]).speed(0.02));
                            }
                        });
                    });

                    ui.separator();
                    ui.label(RichText::new("Ambient").strong());
                    ui.horizontal(|ui| {
                        ui.label("Color");
                        ui.color_edit_button_rgb(&mut env.ambient);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Intensity");
                        ui.add(
                            DragValue::new(&mut env.ambient_intensity)
                                .speed(0.02)
                                .range(0.0..=10.0),
                        );
                    });

                    ui.separator();
                    ui.label(RichText::new("Shadows").strong());
                    ui.horizontal(|ui| {
                        ui.label("Resolution");
                        egui::ComboBox::from_id_salt("citrus-shadow-res")
                            .selected_text(format!("{}", env.shadow_resolution))
                            .show_ui(ui, |ui| {
                                for res in [512u32, 1024, 2048, 4096] {
                                    ui.selectable_value(
                                        &mut env.shadow_resolution,
                                        res,
                                        format!("{res} × {res}"),
                                    );
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Distance");
                        ui.add(
                            egui::Slider::new(&mut env.shadow_distance, 5.0..=150.0).suffix(" m"),
                        )
                        .on_hover_text(
                            "Directional shadow coverage. Smaller = sharper, less reach.",
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Softness");
                        ui.add(egui::Slider::new(&mut env.shadow_softness, 0.0..=4.0));
                    });
                    ui.label(
                        RichText::new(format!(
                            "≈ {:.0} texels/m, PCF spacing ≈ {:.5} uv",
                            env.shadow_resolution as f32 / env.shadow_distance.max(1.0),
                            env.shadow_softness.max(0.0) / env.shadow_resolution as f32
                        ))
                        .small()
                        .weak(),
                    );

                    ui.separator();
                    ui.label(RichText::new("Skybox").strong());
                    ui.checkbox(&mut env.skybox_enabled, "Draw skybox");
                }

                // Skybox texture slot: drop an image from the Files panel.
                let slot = egui::Frame::group(ui.style())
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match &self.scene.skybox {
                            Some(path) => {
                                ui.label("Skybox texture");
                                ui.label(RichText::new(path).small().weak());
                            }
                            None => {
                                ui.label("Skybox texture");
                                ui.label(
                                    RichText::new("Procedural gradient — drop an image here")
                                        .small()
                                        .weak(),
                                );
                            }
                        }
                    })
                    .response;
                let is_image = |p: &std::path::Path| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase())
                        .is_some_and(|e| {
                            matches!(e.as_str(), "png" | "jpg" | "jpeg" | "bmp" | "tga")
                        })
                };
                if slot
                    .dnd_hover_payload::<std::path::PathBuf>()
                    .is_some_and(|p| is_image(&p))
                {
                    ui.painter().rect_stroke(
                        slot.rect,
                        4.0,
                        egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                        egui::StrokeKind::Outside,
                    );
                }
                if let Some(p) = slot.dnd_release_payload::<std::path::PathBuf>()
                    && is_image(&p)
                {
                    self.actions.push(EditorAction::SetSkybox((*p).clone()));
                }
                if self.scene.skybox.is_some() && ui.button("Clear Skybox").clicked() {
                    self.actions.push(EditorAction::ClearSkybox);
                }

                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Disable the sun, zero the ambient, and turn off the skybox \
                         for a fully black world.",
                    )
                    .small()
                    .weak(),
                );
            });
    }

    fn viewport_ui(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        *self.viewport_rect = rect;
        let response = ui.interact(
            rect,
            ui.id().with("viewport-interact"),
            egui::Sense::click_and_drag(),
        );

        // Right mouse over the viewport starts mouse-look. Detected through
        // this widget (not raw winit) so egui's hit-testing decides — clicks
        // on panels, tab bars, or resize handles never reach us.
        if response.hovered()
            && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary))
        {
            self.actions.push(EditorAction::StartLook);
        }

        // Scroll over the viewport dollies the camera. Read through egui for
        // the same reason: the dock consumes wheel events at the winit level.
        if response.hovered() {
            let scroll = ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                self.actions.push(EditorAction::Dolly(scroll / 50.0));
            }
        }

        // Left drag orbits (around the selection, or whatever sits at the
        // viewport center) — unless the gizmo grabbed the drag. Alt forces
        // the orbit even when starting on a gizmo handle.
        let alt = ui.input(|i| i.modifiers.alt);
        let cursor = response.hover_pos();
        let mut gizmo_changed = false;

        // Light Probe Volume box-resize: dragging a face handle changes the
        // volume's size along that axis, keeping the opposite face fixed. Run
        // before the transform gizmo so a grabbed handle wins the drag; the
        // gizmo still moves/rotates the object when no handle is grabbed.
        self.probe_resize_interaction(&response, cursor, alt);
        let probe_active = self.probe_drag.is_some();

        if !probe_active && let Selection::Object(i) = *self.selection {
            {
                let pivot_local = match (self.gizmo.pivot, self.scene.objects[i].render) {
                    (gizmo::PivotMode::Center, Some(render)) => {
                        self.scene.mesh_center_local(render.mesh)
                    }
                    _ => Vec3::ZERO,
                };
                let drag_started = response.drag_started_by(egui::PointerButton::Primary) && !alt;
                let dragging = response.dragged_by(egui::PointerButton::Primary) && !alt;
                // Gizmo operates in world space; parented objects convert
                // back to parent-local afterwards.
                let world = self.scene.world_transform(i);
                let (mut w_scale, mut w_rot, mut w_trans) = world.to_scale_rotation_translation();
                // The scene fills the whole window (panels just occlude it),
                // so the gizmo's NDC mapping must use the full screen rect —
                // the painter still clips to this tab.
                gizmo_changed = self.gizmo.interact(
                    ui,
                    // Full window: matches the swapchain the scene renders to.
                    ui.ctx().viewport_rect(),
                    self.view,
                    self.proj,
                    (&mut w_trans, &mut w_rot, &mut w_scale),
                    pivot_local,
                    cursor,
                    drag_started,
                    dragging,
                );
                if gizmo_changed {
                    let parent_world = self.scene.objects[i]
                        .parent
                        .map_or(glam::Mat4::IDENTITY, |p| self.scene.world_transform(p));
                    let local = parent_world.inverse()
                        * glam::Mat4::from_scale_rotation_translation(w_scale, w_rot, w_trans);
                    let (scale, rotation, translation) = local.to_scale_rotation_translation();
                    let object = &mut self.scene.objects[i];
                    object.translation = translation;
                    object.rotation = rotation;
                    object.scale = scale;
                }
            }
        }

        // Camera frustum widget: only the selected camera shows its
        // orientation, FOV and framing (near/far rects + up-marker).
        {
            let selected = match self.selection {
                Selection::Object(i) => Some(*i),
                _ => None,
            };
            let view_proj = self.proj * self.view;
            let full_rect = ui.ctx().viewport_rect();
            let painter = ui.painter();
            let to_screen = |clip: glam::Vec4| -> egui::Pos2 {
                let ndc = clip.truncate() / clip.w;
                egui::pos2(
                    full_rect.left() + (ndc.x * 0.5 + 0.5) * full_rect.width(),
                    full_rect.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * full_rect.height(),
                )
            };
            for (i, object) in self.scene.objects.iter().enumerate() {
                if !matches!(object.source, citrus_assets::ObjectSource::Camera) {
                    continue;
                }
                if selected != Some(i) || !self.scene.is_active(i) {
                    continue;
                }
                let camera = object
                    .components
                    .iter()
                    .find_map(|c| c.as_any().downcast_ref::<citrus_editor::CameraComponent>());
                let (fov, near, far) =
                    camera.map_or((60.0, 0.1, 100.0), |c| (c.fov_deg, c.near, c.far));
                let world = self.scene.world_transform(i);
                let aspect = (full_rect.width() / full_rect.height().max(1.0)).max(0.1);
                let near = near.max(0.01);
                // Full frustum gets unwieldy with big far planes; cap the
                // widget's reach while the FOV/aspect stay exact.
                let far = far.clamp(near + 0.01, 25.0);
                let corners = |d: f32| {
                    let h = (fov.to_radians() * 0.5).tan() * d;
                    let w = h * aspect;
                    // Cameras look down -Z (glTF convention).
                    [
                        world.transform_point3(Vec3::new(-w, h, -d)),
                        world.transform_point3(Vec3::new(w, h, -d)),
                        world.transform_point3(Vec3::new(w, -h, -d)),
                        world.transform_point3(Vec3::new(-w, -h, -d)),
                    ]
                };
                let near_c = corners(near);
                let far_c = corners(far);
                let origin = world.transform_point3(Vec3::ZERO);
                let stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(110, 255, 140));
                // Lines are clipped against the editor camera's near plane
                // in clip space (partially-behind segments draw instead of
                // vanishing), then against the screen rect — near-plane
                // clipping can yield million-pixel endpoints that the
                // tessellator won't rasterize reliably.
                let line = |a: Vec3, b: Vec3| {
                    const W_EPS: f32 = 0.001;
                    let mut ca = view_proj * a.extend(1.0);
                    let mut cb = view_proj * b.extend(1.0);
                    if ca.w <= W_EPS && cb.w <= W_EPS {
                        return; // fully behind the editor camera
                    }
                    if ca.w < W_EPS {
                        let t = (W_EPS - ca.w) / (cb.w - ca.w);
                        ca += (cb - ca) * t;
                    } else if cb.w < W_EPS {
                        let t = (W_EPS - cb.w) / (ca.w - cb.w);
                        cb += (ca - cb) * t;
                    }
                    if let Some(segment) =
                        clip_segment_to_rect(to_screen(ca), to_screen(cb), full_rect.expand(40.0))
                    {
                        painter.line_segment(segment, stroke);
                    }
                };
                for k in 0..4 {
                    line(near_c[k], near_c[(k + 1) % 4]);
                    line(far_c[k], far_c[(k + 1) % 4]);
                    line(near_c[k], far_c[k]);
                    line(origin, near_c[k]);
                }
                // Up marker: triangle on the near rect's top edge.
                let up_h = (fov.to_radians() * 0.5).tan() * near;
                let up_tip = world.transform_point3(Vec3::new(0.0, up_h * 1.8, -near));
                line(near_c[0], up_tip);
                line(near_c[1], up_tip);
            }
        }

        // Light Probe Volume widget: the selected object's box + its resolved
        // probe grid, so the density reads at a glance before baking.
        {
            let selected = match self.selection {
                Selection::Object(i) => Some(*i),
                _ => None,
            };
            let view_proj = self.proj * self.view;
            let full_rect = ui.ctx().viewport_rect();
            let painter = ui.painter();
            let to_screen = |clip: glam::Vec4| -> egui::Pos2 {
                let ndc = clip.truncate() / clip.w;
                egui::pos2(
                    full_rect.left() + (ndc.x * 0.5 + 0.5) * full_rect.width(),
                    full_rect.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * full_rect.height(),
                )
            };
            const W_EPS: f32 = 0.001;
            let stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 200, 255));
            let line = |a: Vec3, b: Vec3| {
                let mut ca = view_proj * a.extend(1.0);
                let mut cb = view_proj * b.extend(1.0);
                if ca.w <= W_EPS && cb.w <= W_EPS {
                    return;
                }
                if ca.w < W_EPS {
                    let t = (W_EPS - ca.w) / (cb.w - ca.w);
                    ca += (cb - ca) * t;
                } else if cb.w < W_EPS {
                    let t = (W_EPS - cb.w) / (ca.w - cb.w);
                    cb += (ca - cb) * t;
                }
                if let Some(segment) =
                    clip_segment_to_rect(to_screen(ca), to_screen(cb), full_rect.expand(40.0))
                {
                    painter.line_segment(segment, stroke);
                }
            };
            for (i, object) in self.scene.objects.iter().enumerate() {
                if selected != Some(i) {
                    continue;
                }
                let Some(volume) = object
                    .components
                    .iter()
                    .find_map(|c| c.as_any().downcast_ref::<citrus_editor::LightProbeVolume>())
                else {
                    continue;
                };
                let world = self.scene.world_transform(i);
                let half = Vec3::from(volume.size) * 0.5;
                // 8 box corners, then the 12 edges.
                let corner = |sx: f32, sy: f32, sz: f32| {
                    world.transform_point3(Vec3::new(half.x * sx, half.y * sy, half.z * sz))
                };
                let c = [
                    corner(-1.0, -1.0, -1.0),
                    corner(1.0, -1.0, -1.0),
                    corner(1.0, 1.0, -1.0),
                    corner(-1.0, 1.0, -1.0),
                    corner(-1.0, -1.0, 1.0),
                    corner(1.0, -1.0, 1.0),
                    corner(1.0, 1.0, 1.0),
                    corner(-1.0, 1.0, 1.0),
                ];
                for k in 0..4 {
                    line(c[k], c[(k + 1) % 4]); // back face
                    line(c[k + 4], c[(k + 1) % 4 + 4]); // front face
                    line(c[k], c[k + 4]); // connecting edges
                }
                // Resize handles at each face center: drag to change the box
                // size along that axis (the opposite face stays put).
                let axes = [Vec3::X, Vec3::Y, Vec3::Z];
                for axis in 0..3 {
                    for sign in [-1.0f32, 1.0] {
                        let fc = world.transform_point3(axes[axis] * (sign * half[axis]));
                        let clip = view_proj * fc.extend(1.0);
                        if clip.w <= W_EPS {
                            continue;
                        }
                        let p = to_screen(clip);
                        let active = self
                            .probe_drag
                            .as_ref()
                            .is_some_and(|d| d.object == i && d.axis == axis);
                        let hovered = self.probe_drag.is_none()
                            && cursor.is_some_and(|c| c.distance(p) <= 9.0);
                        if hovered {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                        }
                        let (s, col) = if active || hovered {
                            (4.5, egui::Color32::WHITE)
                        } else {
                            (3.0, egui::Color32::from_rgb(120, 200, 255))
                        };
                        painter.rect_filled(
                            egui::Rect::from_center_size(p, egui::vec2(s * 2.0, s * 2.0)),
                            2.0,
                            col,
                        );
                    }
                }
                // Probe points, capped so a dense volume doesn't flood the
                // painter (the inspector still reports the true count).
                if volume.probe_count() <= 4096 {
                    let dot = egui::Color32::from_rgb(180, 225, 255);
                    for local in volume.local_positions() {
                        let clip = view_proj * world.transform_point3(local).extend(1.0);
                        if clip.w <= W_EPS {
                            continue;
                        }
                        let p = to_screen(clip);
                        if full_rect.contains(p) {
                            painter.circle_filled(p, 1.6, dot);
                        }
                    }
                }
            }
        }

        // Billboards: a drawn bulb at every light and a drawn camera at every
        // camera so they read as real icons (egui's font has no 💡/🎥 glyphs).
        // `billboards` records the (screen pos, object index) click targets.
        let mut billboards: Vec<(egui::Pos2, usize)> = Vec::new();
        {
            let view_proj = self.proj * self.view;
            let full_rect = ui.ctx().viewport_rect();
            let painter = ui.painter();
            for (i, object) in self.scene.objects.iter().enumerate() {
                if !self.scene.is_active(i) {
                    continue; // no widgets for disabled objects
                }
                let is_light = object.components.iter().any(|c| {
                    c.as_any()
                        .downcast_ref::<citrus_editor::LightComponent>()
                        .is_some()
                });
                let is_camera = matches!(object.source, citrus_assets::ObjectSource::Camera);
                if !is_light && !is_camera {
                    continue;
                }
                let pos = self.scene.world_transform(i).w_axis.truncate();
                let clip = view_proj * pos.extend(1.0);
                if clip.w <= 0.001 {
                    continue; // behind the camera
                }
                let ndc = clip.truncate() / clip.w;
                let screen = egui::pos2(
                    full_rect.left() + (ndc.x * 0.5 + 0.5) * full_rect.width(),
                    full_rect.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * full_rect.height(),
                );
                if !full_rect.contains(screen) {
                    continue;
                }
                let selected = matches!(*self.selection, Selection::Object(s) if s == i);
                if is_light {
                    draw_light_icon(painter, screen, selected);
                } else {
                    draw_camera_icon(painter, screen, selected);
                }
                billboards.push((screen, i));
            }
        }

        // Light gizmo for the selected object whenever it carries a Light
        // component (not only dedicated Light objects). Directional/spot show
        // their aim; point/spot show their range; spot shows its cone.
        if let Selection::Object(i) = *self.selection
            && self.scene.is_active(i)
            && let Some(light) = self.scene.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<citrus_editor::LightComponent>())
        {
            use citrus_editor::LightKind;
            let view_proj = self.proj * self.view;
            let full_rect = ui.ctx().viewport_rect();
            let painter = ui.painter();
            let stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 226, 120));
            let to_screen = |clip: glam::Vec4| -> egui::Pos2 {
                let ndc = clip.truncate() / clip.w;
                egui::pos2(
                    full_rect.left() + (ndc.x * 0.5 + 0.5) * full_rect.width(),
                    full_rect.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * full_rect.height(),
                )
            };
            let line = |a: Vec3, b: Vec3| {
                const W_EPS: f32 = 0.001;
                let mut ca = view_proj * a.extend(1.0);
                let mut cb = view_proj * b.extend(1.0);
                if ca.w <= W_EPS && cb.w <= W_EPS {
                    return;
                }
                if ca.w < W_EPS {
                    let t = (W_EPS - ca.w) / (cb.w - ca.w);
                    ca += (cb - ca) * t;
                } else if cb.w < W_EPS {
                    let t = (W_EPS - cb.w) / (ca.w - cb.w);
                    cb += (ca - cb) * t;
                }
                if let Some(segment) =
                    clip_segment_to_rect(to_screen(ca), to_screen(cb), full_rect.expand(40.0))
                {
                    painter.line_segment(segment, stroke);
                }
            };

            let world = self.scene.world_transform(i);
            let origin = world.transform_point3(Vec3::ZERO);
            let (_, rotation, _) = world.to_scale_rotation_translation();
            let fwd = rotation * Vec3::NEG_Z;
            let right = rotation * Vec3::X;
            let up = rotation * Vec3::Y;
            // Wire circle of `radius` centered at `c`, in the plane
            // spanned by axes `a`/`b`.
            let circle = |c: Vec3, a: Vec3, b: Vec3, radius: f32| {
                const SEG: usize = 24;
                let mut prev = c + a * radius;
                for k in 1..=SEG {
                    let ang = k as f32 / SEG as f32 * std::f32::consts::TAU;
                    let p = c + (a * ang.cos() + b * ang.sin()) * radius;
                    line(prev, p);
                    prev = p;
                }
            };

            match light.kind {
                LightKind::Directional => {
                    // A bundle of parallel rays through a small disc.
                    let len = 2.0;
                    let r = 0.35;
                    circle(origin, right, up, r);
                    for k in 0..8 {
                        let ang = k as f32 / 8.0 * std::f32::consts::TAU;
                        let off = (right * ang.cos() + up * ang.sin()) * r;
                        line(origin + off, origin + off + fwd * len);
                    }
                    line(origin, origin + fwd * (len + 0.3));
                }
                LightKind::Point => {
                    let radius = light.range.min(25.0);
                    circle(origin, right, up, radius);
                    circle(origin, right, fwd, radius);
                    circle(origin, up, fwd, radius);
                }
                LightKind::Spot => {
                    let dist = light.range.min(25.0);
                    let half = (light.spot_angle.to_radians() * 0.5).tan();
                    let end = origin + fwd * dist;
                    let rad = half * dist;
                    circle(end, right, up, rad);
                    for k in 0..4 {
                        let ang = k as f32 / 4.0 * std::f32::consts::TAU;
                        let rim = end + (right * ang.cos() + up * ang.sin()) * rad;
                        line(origin, rim);
                    }
                }
            }
        }

        // Plain left drag (not claimed by the gizmo or a probe handle): orbit
        // the camera around a pivot locked at drag start.
        if response.dragged_by(egui::PointerButton::Primary)
            && !self.gizmo.is_focused()
            && !probe_active
        {
            let delta = response.drag_delta();
            self.actions.push(EditorAction::Orbit(delta.x, delta.y));
        }
        if response.drag_stopped_by(egui::PointerButton::Primary) {
            self.actions.push(EditorAction::OrbitEnd);
        }

        // Click-to-pick on release-without-drag, unless the gizmo owns the
        // cursor or we're flying.
        let gizmo_busy =
            self.gizmo.is_focused() || cursor.is_some_and(|p| self.gizmo.pick_preview(p));
        if response.clicked()
            && !gizmo_busy
            && !gizmo_changed
            && !self.looking
            && let Some(pos) = response.interact_pointer_pos()
        {
            // A click near a camera/light billboard selects it directly;
            // these objects have no mesh AABB to ray-pick against.
            let hit = billboards
                .iter()
                .map(|(p, i)| (p.distance(pos), *i))
                .filter(|(d, _)| *d <= 16.0)
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, i)| i);
            match hit {
                Some(i) => *self.selection = Selection::Object(i),
                None => self.actions.push(EditorAction::PickAt(pos)),
            }
        }

        // Viewport overlays: gizmo tool buttons (top-left), pivot /
        // orientation / snap controls (top-center).
        egui::Area::new(ui.id().with("vp-tools"))
            .order(egui::Order::Middle)
            .fixed_pos(rect.left_top() + egui::vec2(8.0, 8.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (tool, label, hint) in [
                            (GizmoTool::Move, "⬌", "Move (G)"),
                            (GizmoTool::Rotate, "🔄", "Rotate (R)"),
                            (GizmoTool::Scale, "⛶", "Scale (S)"),
                        ] {
                            if ui
                                .selectable_label(self.gizmo.tool == tool, label)
                                .on_hover_text(hint)
                                .clicked()
                            {
                                self.gizmo.tool = tool;
                            }
                        }
                    });
                });
            });
        egui::Area::new(ui.id().with("vp-pivot"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::pos2(rect.center().x - 170.0, rect.top() + 8.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("vp-pivot-mode")
                            .width(70.0)
                            .selected_text(match self.gizmo.pivot {
                                gizmo::PivotMode::Origin => "Pivot",
                                gizmo::PivotMode::Center => "Center",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.gizmo.pivot,
                                    gizmo::PivotMode::Origin,
                                    "Pivot",
                                )
                                .on_hover_text("Transform around the object's origin");
                                ui.selectable_value(
                                    &mut self.gizmo.pivot,
                                    gizmo::PivotMode::Center,
                                    "Center",
                                )
                                .on_hover_text("Transform around the mesh bounds center");
                            });
                        egui::ComboBox::from_id_salt("vp-orientation")
                            .width(70.0)
                            .selected_text(if self.gizmo.local_orientation {
                                "Local"
                            } else {
                                "Global"
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.gizmo.local_orientation,
                                    false,
                                    "Global",
                                );
                                ui.selectable_value(
                                    &mut self.gizmo.local_orientation,
                                    true,
                                    "Local",
                                );
                            });
                        ui.separator();
                        ui.checkbox(&mut self.gizmo.snap, "Snap")
                            .on_hover_text("Snap to grid (or hold Ctrl)");
                        ui.add(
                            egui::DragValue::new(&mut self.gizmo.grid_size)
                                .speed(0.05)
                                .range(0.01..=10.0)
                                .suffix(" m"),
                        )
                        .on_hover_text("Grid size");
                    });
                });
            });

        // Drop a .material anywhere on the viewport: assign to the hit mesh.
        if let Some(payload) = response.dnd_release_payload::<PathBuf>()
            && payload.extension().is_some_and(|e| e == "material")
            && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
        {
            self.actions
                .push(EditorAction::AssignMaterialAt(pos, (*payload).clone()));
        }
    }

    fn inspector_ui(&mut self, ui: &mut egui::Ui) {
        let shader_refs: Vec<&str> = self.shader_list.iter().map(String::as_str).collect();

        // When a code tab is focused, show its problems here (kept out of the
        // editor so the code area doesn't shift while typing).
        if let Some(path) = self.focused_code.clone()
            && let Some(editor) = self.open_editors.iter().find(|e| e.path == path)
        {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.horizontal(|ui| {
                ui.heading("Problems");
                ui.label(egui::RichText::new(name).small().weak());
            });
            if editor.diagnostics.is_empty() {
                ui.label(egui::RichText::new("✓ No problems").small().weak());
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for d in &editor.diagnostics {
                            let color = if d.level == "error" {
                                ui.visuals().error_fg_color
                            } else {
                                ui.visuals().warn_fg_color
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} {} — {}",
                                    if d.level == "error" { "⛔" } else { "⚠" },
                                    d.line,
                                    d.message
                                ))
                                .small()
                                .color(color),
                            );
                        }
                    });
            }
            return;
        }

        // Lock toggle: when locked, the inspector keeps showing the snapshot
        // selection regardless of what the user clicks next.
        if self.inspector.lock_header(ui) {
            *self.inspector_lock_target = if self.inspector.locked {
                Some(self.selection.clone())
            } else {
                None
            };
        }
        let effective = if self.inspector.locked {
            self.inspector_lock_target
                .clone()
                .unwrap_or_else(|| self.selection.clone())
        } else {
            self.selection.clone()
        };
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match &effective {
                Selection::None => {
                    self.inspector.ui(ui, InspectorContent::Empty, &shader_refs);
                }
                Selection::Object(i) => {
                    let index = *i;
                    if index >= self.scene.objects.len() {
                        // Locked onto an object that was since deleted.
                        self.inspector.ui(ui, InspectorContent::Empty, &shader_refs);
                        return;
                    }
                    let object = &self.scene.objects[index];
                    let render = object.render;
                    let (rx, ry, rz) = object.rotation.to_euler(glam::EulerRot::XYZ);
                    let mut info = ObjectInfoModel {
                        name: object.name.clone(),
                        enabled: object.enabled,
                        static_geometry: object.static_geometry,
                        kind: object.kind_label(),
                        transform: TransformModel {
                            translation: object.translation.to_array(),
                            rotation_deg: [rx.to_degrees(), ry.to_degrees(), rz.to_degrees()],
                            scale: object.scale.to_array(),
                        },
                        mesh: render.map(|r| {
                            let mi = self.scene.mesh_info(r.mesh);
                            (mi.vertices, mi.triangles)
                        }),
                    };
                    let material_index = render.map(|r| r.material);
                    // Split borrows: components live on the object, the
                    // material model in the scene's material list.
                    let scene = &mut *self.scene;
                    let components = &mut scene.objects[index].components;
                    let material = material_index.map(|m| &mut scene.materials[m].model);
                    let response = self.inspector.ui(
                        ui,
                        InspectorContent::Object {
                            info: &mut info,
                            material,
                            shader_info: self.shader_info,
                            components,
                            registry: self.registry,
                        },
                        &shader_refs,
                    );
                    if response.object_changed {
                        let object = &mut self.scene.objects[index];
                        object.name = info.name;
                        object.enabled = info.enabled;
                        object.static_geometry = info.static_geometry;
                        object.translation = Vec3::from(info.transform.translation);
                        object.rotation = glam::Quat::from_euler(
                            glam::EulerRot::XYZ,
                            info.transform.rotation_deg[0].to_radians(),
                            info.transform.rotation_deg[1].to_radians(),
                            info.transform.rotation_deg[2].to_radians(),
                        );
                        object.scale = Vec3::from(info.transform.scale);
                    }
                    if let Some(material_index) = material_index {
                        if response.material_changed {
                            self.actions
                                .push(EditorAction::MaterialEdited(material_index));
                        }
                        if response.reset_material {
                            self.actions
                                .push(EditorAction::ResetMaterial(material_index));
                        }
                    }
                    if let Some(path) = response.material_dropped {
                        self.actions
                            .push(EditorAction::AssignMaterialToObject(index, path));
                    }
                    // Component add/remove takes effect immediately so the
                    // frame's undo diff captures it as one Object entry.
                    if let Some(name) = response.add_component
                        && let Some(component) = self.registry.create(name)
                    {
                        self.scene.objects[index].components.push(component);
                    }
                    if let Some(remove) = response.remove_component
                        && remove < self.scene.objects[index].components.len()
                    {
                        self.scene.objects[index].components.remove(remove);
                    }
                }
                Selection::File(path) => {
                    let path_display = path.display().to_string();
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(str::to_lowercase);
                    match ext.as_deref() {
                        Some("material") => {
                            if let Some(fm) = self.file_material.as_mut() {
                                let response = self.inspector.ui(
                                    ui,
                                    InspectorContent::MaterialFile {
                                        path: path_display,
                                        material: &mut fm.model,
                                        shader_info: self.shader_info,
                                        dirty: fm.dirty,
                                    },
                                    &shader_refs,
                                );
                                if response.material_changed {
                                    fm.dirty = true;
                                    // Objects using this .material update
                                    // live, not only on save.
                                    self.actions
                                        .push(EditorAction::FileMaterialEdited(fm.path.clone()));
                                }
                                if response.save_material {
                                    self.actions.push(EditorAction::SaveFileMaterial);
                                }
                            } else {
                                ui.label("Failed to open material (see log).");
                            }
                        }
                        Some("scene") => {
                            let response = self.inspector.ui(
                                ui,
                                InspectorContent::SceneFile { path: path_display },
                                &shader_refs,
                            );
                            if response.load_scene {
                                self.actions.push(EditorAction::LoadScene(path.clone()));
                            }
                        }
                        Some(ext) if CODE_EXTENSIONS.contains(&ext) => {
                            // Code/text files open in their own dockable editor
                            // tab; the inspector just offers a button to open it.
                            ui.heading(
                                std::path::Path::new(&path_display)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| path_display.clone()),
                            );
                            ui.label(egui::RichText::new(&path_display).small().weak());
                            if ui.button("✏ Open in editor").clicked() {
                                self.actions.push(EditorAction::OpenCodeFile(path.clone()));
                            }
                        }
                        _ => {
                            let size = std::fs::metadata(path).map(|m| m.len()).ok();
                            self.inspector.ui(
                                ui,
                                InspectorContent::File {
                                    path: path_display,
                                    size,
                                },
                                &shader_refs,
                            );
                        }
                    }
                }
            });
    }
}

/// Liang–Barsky: clip a 2D segment to a rect. None = fully outside.
fn clip_segment_to_rect(a: egui::Pos2, b: egui::Pos2, rect: egui::Rect) -> Option<[egui::Pos2; 2]> {
    let d = b - a;
    let mut t0 = 0.0f32;
    let mut t1 = 1.0f32;
    for (p, q) in [
        (-d.x, a.x - rect.left()),
        (d.x, rect.right() - a.x),
        (-d.y, a.y - rect.top()),
        (d.y, rect.bottom() - a.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None; // parallel and outside
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
            if t0 > t1 {
                return None;
            }
        }
    }
    Some([a + d * t0, a + d * t1])
}

/// All project-relative paths with the given extension, sorted.
fn scan_project_files(root: &Path, ext: &str) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, ext: &str, depth: usize, out: &mut Vec<String>) {
        if depth > 8 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            if path.is_dir() {
                walk(&path, root, ext, depth + 1, out);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(relative_to(&path, root));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, ext, 0, &mut out);
    out.sort();
    out
}

fn object_state(o: &scene::SceneObject) -> ObjectState {
    ObjectState {
        name: o.name.clone(),
        translation: o.translation,
        rotation: o.rotation,
        scale: o.scale,
        components: o.save_components(),
    }
}

/// Draw a little light-bulb icon (glass + screw base) centered at `center`.
/// egui's bundled font has no 💡 glyph, so the billboard is drawn by hand.
fn draw_light_icon(painter: &egui::Painter, center: egui::Pos2, selected: bool) {
    let glass = if selected {
        egui::Color32::from_rgb(255, 242, 150)
    } else {
        egui::Color32::from_rgb(235, 215, 120)
    };
    let dark = egui::Color32::from_rgb(70, 62, 35);
    let r = 7.0;
    let bulb = center - egui::vec2(0.0, 2.0);
    if selected {
        // Emitted-light rays.
        for k in 0..8 {
            let a = k as f32 / 8.0 * std::f32::consts::TAU;
            let d = egui::vec2(a.cos(), a.sin());
            painter.line_segment(
                [bulb + d * (r + 2.0), bulb + d * (r + 5.0)],
                egui::Stroke::new(1.5, glass),
            );
        }
    }
    painter.circle_filled(bulb, r, glass);
    painter.circle_stroke(bulb, r, egui::Stroke::new(1.0, dark));
    // Screw base: a small block with a thread line.
    let base =
        egui::Rect::from_center_size(center + egui::vec2(0.0, r + 1.5), egui::vec2(r * 1.1, 5.0));
    painter.rect_filled(base, 1.0, dark);
    painter.line_segment(
        [
            egui::pos2(base.left() + 1.0, base.center().y),
            egui::pos2(base.right() - 1.0, base.center().y),
        ],
        egui::Stroke::new(1.0, glass),
    );
}

/// Draw a little video-camera icon (body + lens) centered at `center`.
fn draw_camera_icon(painter: &egui::Painter, center: egui::Pos2, selected: bool) {
    let body_col = if selected {
        egui::Color32::from_rgb(225, 238, 255)
    } else {
        egui::Color32::from_rgb(190, 210, 235)
    };
    let dark = egui::Color32::from_rgb(40, 55, 75);
    let body = egui::Rect::from_min_size(center + egui::vec2(-9.0, -5.0), egui::vec2(12.0, 10.0));
    painter.rect_filled(body, 2.0, body_col);
    painter.rect_stroke(
        body,
        2.0,
        egui::Stroke::new(1.0, dark),
        egui::StrokeKind::Inside,
    );
    // Lens: a trapezoid jutting to the right (classic video-camera silhouette).
    let lx = body.right();
    let cy = center.y;
    let lens = vec![
        egui::pos2(lx, cy - 2.5),
        egui::pos2(lx + 6.0, cy - 4.5),
        egui::pos2(lx + 6.0, cy + 4.5),
        egui::pos2(lx, cy + 2.5),
    ];
    painter.add(egui::Shape::convex_polygon(
        lens,
        body_col,
        egui::Stroke::new(1.0, dark),
    ));
    // Lens dot on the body front.
    painter.circle_filled(egui::pos2(body.left() + 4.0, cy), 1.6, dark);
}

/// Char index → (line, utf-8 byte column) for an LSP position (we negotiate
/// utf-8 position encoding).
fn char_to_line_col(text: &str, char_index: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut col = 0u32;
    for (i, ch) in text.chars().enumerate() {
        if i == char_index {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf8() as u32;
        }
    }
    (line, col)
}

/// Char index of the start of the identifier word ending at `cursor`.
fn word_start(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut start = cursor.min(chars.len());
    while start > 0 {
        let c = chars[start - 1];
        if c.is_alphanumeric() || c == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// Route a completion/hover response into the matching open editor.
fn apply_lsp_response(editors: &mut [OpenEditor], kind: LspRequestKind, result: serde_json::Value) {
    match kind {
        LspRequestKind::Completion { path, anchor_char } => {
            let arr = result
                .get("items")
                .and_then(|i| i.as_array())
                .or_else(|| result.as_array());
            let items: Vec<citrus_editor::CompletionItem> = arr
                .map(|arr| {
                    arr.iter()
                        .map(|it| {
                            let label = it
                                .get("label")
                                .and_then(|l| l.as_str())
                                .unwrap_or("")
                                .to_owned();
                            let insert_text = it
                                .pointer("/textEdit/newText")
                                .and_then(|t| t.as_str())
                                .or_else(|| it.get("insertText").and_then(|t| t.as_str()))
                                .unwrap_or(&label)
                                .to_owned();
                            let detail = it
                                .get("detail")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_owned();
                            citrus_editor::CompletionItem {
                                label,
                                insert_text,
                                detail,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !items.is_empty()
                && let Some(editor) = editors.iter_mut().find(|e| e.path == path)
            {
                editor.completion = Some(citrus_editor::CompletionState {
                    items,
                    selected: 0,
                    anchor_char,
                });
            }
        }
        LspRequestKind::Hover { path } => {
            let text = hover_text(&result);
            if let Some(editor) = editors.iter_mut().find(|e| e.path == path) {
                editor.hover = if text.is_empty() {
                    None
                } else {
                    Some(citrus_editor::HoverState { text })
                };
            }
        }
        // Definition jumps are handled in `pump_lsp` (they need to open files).
        LspRequestKind::Definition => {}
    }
}

/// Parse an LSP `textDocument/definition` result into `(path, line, col)`.
///
/// Accepts a single `Location`, an array of `Location`, or an array of
/// `LocationLink` (the `target*` shape), taking the first entry.
fn parse_definition(result: &serde_json::Value) -> Option<(PathBuf, u32, u32)> {
    let loc = if result.is_array() {
        result.get(0)?
    } else {
        result
    };
    // LocationLink uses `targetUri`/`targetSelectionRange`; Location uses
    // `uri`/`range`.
    let uri = loc
        .get("uri")
        .or_else(|| loc.get("targetUri"))
        .and_then(|u| u.as_str())?;
    let path = uri.strip_prefix("file://").map(PathBuf::from)?;
    let range = loc
        .get("range")
        .or_else(|| loc.get("targetSelectionRange"))
        .or_else(|| loc.get("targetRange"))?;
    let line = range.pointer("/start/line").and_then(|l| l.as_u64())? as u32;
    let col = range
        .pointer("/start/character")
        .and_then(|c| c.as_u64())
        .unwrap_or(0) as u32;
    Some((path, line, col))
}

/// Extract plain text from an LSP `hover.contents` value.
fn hover_text(result: &serde_json::Value) -> String {
    let Some(contents) = result.get("contents") else {
        return String::new();
    };
    if let Some(s) = contents.as_str() {
        return s.to_owned();
    }
    if let Some(v) = contents.get("value").and_then(|v| v.as_str()) {
        return v.to_owned();
    }
    if let Some(arr) = contents.as_array() {
        return arr
            .iter()
            .map(|c| {
                c.as_str()
                    .or_else(|| c.get("value").and_then(|v| v.as_str()))
                    .unwrap_or("")
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let make = |n: u32| {
        let name = if n == 0 {
            stem.to_owned()
        } else {
            format!("{stem}_{n}")
        };
        if ext.is_empty() {
            dir.join(name)
        } else {
            dir.join(format!("{name}.{ext}"))
        }
    };
    (0..1000)
        .map(make)
        .find(|p| !p.exists())
        .unwrap_or_else(|| make(0))
}

impl ApplicationHandler for EngineApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.init(event_loop) {
            tracing::error!("initialization failed: {e:#}");
            event_loop.exit();
        } else {
            tracing::info!(elapsed = ?self.start.elapsed(), "engine initialized");
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.looking
        {
            self.look_delta.0 += delta.0;
            self.look_delta.1 += delta.1;
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let egui_wants =
            if let (Some(state), Some(window)) = (self.egui_state.as_mut(), self.window.as_ref()) {
                // While looking, the cursor is locked: keep egui out of it.
                if self.looking {
                    false
                } else {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        window.request_redraw();
                    }
                    response.consumed
                }
            } else {
                false
            };

        match event {
            WindowEvent::CloseRequested => {
                self.save_project();
                event_loop.exit();
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } if !egui_wants && !self.egui_ctx.wants_keyboard_input() => {
                // ...but not while a code editor / text field has focus.
                self.selection = Selection::None;
                self.file_material = None;
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        ..
                    },
                ..
            } => match state {
                ElementState::Released => {
                    self.keys.remove(&code);
                }
                ElementState::Pressed if !egui_wants => {
                    self.keys.insert(code);
                    let ctrl = self.keys.contains(&KeyCode::ControlLeft)
                        || self.keys.contains(&KeyCode::ControlRight);
                    if !self.looking {
                        match code {
                            KeyCode::KeyG => self.gizmo.tool = GizmoTool::Move,
                            KeyCode::KeyR => self.gizmo.tool = GizmoTool::Rotate,
                            KeyCode::KeyS if !ctrl => self.gizmo.tool = GizmoTool::Scale,
                            KeyCode::KeyS if ctrl => {
                                self.actions.push(EditorAction::SaveScene(None));
                            }
                            KeyCode::KeyF => {
                                self.actions.push(EditorAction::FocusSelected);
                            }
                            KeyCode::KeyZ if ctrl => {
                                let shift = self.keys.contains(&KeyCode::ShiftLeft)
                                    || self.keys.contains(&KeyCode::ShiftRight);
                                self.actions.push(if shift {
                                    EditorAction::Redo
                                } else {
                                    EditorAction::Undo
                                });
                            }
                            KeyCode::KeyY if ctrl => {
                                self.actions.push(EditorAction::Redo);
                            }
                            KeyCode::KeyW if ctrl => {
                                self.close_focused_code_tab();
                            }
                            KeyCode::Delete => {
                                if let Selection::Object(i) = self.selection {
                                    self.actions.push(EditorAction::DeleteObject(i));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ElementState::Pressed => {}
            },
            WindowEvent::Resized(_) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize();
                }
            }
            WindowEvent::MouseInput { button, state, .. } => {
                let pressed = state == ElementState::Pressed;
                match button {
                    MouseButton::Right
                        // Look starts from the viewport tab widget (see
                        // viewport_ui); only the release is handled here so
                        // it can't get stuck while the cursor is locked.
                        if !pressed => {
                            self.set_looking(false);
                        }
                    MouseButton::Middle => {
                        self.panning = pressed
                            && self.cursor_in_viewport()
                            && !self.egui_ctx.is_using_pointer();
                    }
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((lx, ly)) = self.last_cursor
                    && self.panning
                {
                    self.camera
                        .pan((position.x - lx) as f32, (position.y - ly) as f32);
                }
                self.last_cursor = Some((position.x, position.y));
            }
            // While looking the cursor is locked and egui is bypassed, so
            // handle scroll here; otherwise the viewport tab handles it.
            WindowEvent::MouseWheel { delta, .. } if self.looking => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 60.0,
                };
                self.camera.dolly(scroll);
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}
