//! Editor application: the dockable egui shell (menu bar, Inspector, Scene
//! tree, Files, gizmos, picking) wrapped around the runtime. Compiled only
//! with the `editor` feature; a shipped game never links it.

use crate::{audio, bundle, crash, gizmo, icon, log_capture, lsp, physics, plugins, scene};
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

use crate::camera::FlyCamera;
use citrus_core::{ComponentCommand, ComponentRegistry, ObjectId};
use citrus_editor::{
    CodeDiagnostic, CodeEditor, EditorComponents, FileBrowser, InspectorContent, InspectorPanel,
    MaterialModel, ObjectInfoModel, ScenePanel, ShaderUiInfo, TransformModel,
};
use citrus_render::{CameraData, FrameInput, LightData, Renderer};
use crate::gizmo::{GizmoState, GizmoTool};
use crate::scene::{LoadedScene, material_from_model, model_from_material, relative_to};
use crate::shaders::ShaderLibrary;
use crate::undo::{ObjectState, UndoEntry, UndoStack};

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

pub fn run(config: AppConfig) -> Result<()> {
    crash::install();
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
        file_diagnostics: HashMap::new(),
        gizmo: GizmoState::new(),
        widget_filter: WidgetFilter::default(),
        gizmo_drag: false,
        orbit_armed: false,
        audio: audio::AudioEngine::new(),
        actions: Vec::new(),
        undo_stack: UndoStack::default(),
        suppress_undo_record: false,
        components: ComponentRegistry::with_builtins(),
        editor_components: EditorComponents::with_builtins(),
        playing: false,
        play_paused: false,
        play_scene_switched: false,
        play_origin_scene: None,
        play_snapshot: None,
        physics: None,
        shaders: ShaderLibrary::default(),
        shader_files: Vec::new(),
        last_shader_scan: None,
        dirty_materials: HashSet::new(),
        last_material_edit: None,
        plugins: plugins::PluginHost::default(),
        plugin_build_error: None,
        status: None,
        reload_pending: false,
        project: citrus_assets::ProjectFile::default(),
        camera: FlyCamera::default(),
        orbit_pivot: None,
        looking: false,
        look_just_ended: false,
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
        show_project_settings: false,
        show_new_project: false,
        show_open_project: false,
        open_project_path: String::new(),
        lightmap_preview: false,
        scene_dirty: false,
        show_quit_dialog: false,
        quit_after_frame: false,
        new_project_parent: String::new(),
        new_project_name: "my-game".into(),
        build_status: None,
        log_filter: LogFilter::default(),
        probe_drag: None,
        audio_drag: None,
        collider_drag: None,
        stats: Stats::default(),
        world: hecs::World::new(),
        start: Instant::now(),
        last_frame: Instant::now(),
        rt_gi: crate::realtime_gi::RealtimeGiState::default(),
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
    /// Reference list to show in the `gr` picker popup.
    references: Option<Vec<citrus_editor::ReferenceItem>>,
}

/// An in-flight LSP request, keyed by request id, awaiting its response.
#[derive(Clone)]
enum LspRequestKind {
    Completion { path: PathBuf, anchor_char: usize },
    Hover { path: PathBuf },
    Definition,
    References { path: PathBuf },
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
    CreatePostFx(PathBuf),
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
    /// Close a code tab by path (vim `:q`/`:wq`).
    CloseCodeTab(PathBuf),
    /// Request LSP completion / hover / definition at a cursor char index.
    LspCompletion(PathBuf, usize),
    LspHover(PathBuf, usize),
    LspGoto(PathBuf, usize),
    LspReferences(PathBuf, usize),
    /// Open a file (if needed) and jump to a 0-based line/col (reference pick).
    OpenAndGoto(PathBuf, u32, u32),
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
    SpawnLight(citrus_core::LightKind),
    SpawnProbeVolume,
    SpawnAudioSource,
    SpawnBoxCollider,
    SpawnSphereCollider,
    /// (child, new parent, before-sibling) reorder/move.
    MoveObject(usize, Option<usize>, Option<usize>),
    DeleteObject(usize),
    DuplicateObject(usize),
    DuplicateFile(PathBuf),
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
    /// Run the GPU lighting bake (lightmaps + probes).
    BakeLighting,
    /// Discard the baked lighting result.
    ClearBake,
    /// Scaffold a new project under `parent/<name>` and switch to it.
    NewProject { parent: PathBuf, name: String },
    /// Open (switch to) an existing project folder.
    OpenProject(PathBuf),
    /// Compile the current project into a standalone `build/` executable.
    BuildGame,
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
    /// Lighting-bake settings + Bake / Clear ("Baker's Man").
    Baker,
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
    // Right (Inspector) node a touch wider — ~30px at the 1600px default —
    // so the inspector's rows fit without clipping.
    let [viewport, _right] = tree.split_right(
        NodeIndex::root(),
        0.76,
        vec![Tab::Inspector, Tab::Environment, Tab::Baker],
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

/// Which collider handle is being dragged.
#[derive(Clone, Copy)]
enum ColliderHandle {
    /// Box face: axis (0/1/2) and sign (-1/+1).
    BoxFace(usize, f32),
    SphereRadius,
}

/// Active drag of a Box/Sphere collider handle.
struct ColliderDrag {
    object: usize,
    handle: ColliderHandle,
    start_center: [f32; 3],
    start_size: [f32; 3],
    start_radius: f32,
    /// World scale along the dragged axis (size→meters); 1.0 for sphere.
    scale_a: f32,
    screen_axis: egui::Vec2,
    start_cursor: egui::Pos2,
}

/// Active drag of an AudioSource min/max-distance sphere radius.
struct AudioDrag {
    object: usize,
    /// True = max_distance, false = min_distance.
    max: bool,
    start_radius: f32,
    /// Screen pixels per 1 world-meter along the radial (camera-right) dir.
    screen_axis: egui::Vec2,
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
    /// Wrap long lines instead of extending (and horizontal-scrolling).
    wrap: bool,
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
            wrap: true,
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

/// Per-kind visibility + size for a billboard widget.
#[derive(Clone, Copy)]
struct WidgetSetting {
    visible: bool,
    size: f32,
}

impl Default for WidgetSetting {
    fn default() -> Self {
        Self {
            visible: true,
            // Billboards read better a bit larger by default; the widget
            // filter still lets each kind be resized.
            size: 1.6,
        }
    }
}

/// Viewport billboard-widget filter (top-right overlay). Only billboard icons
/// are filtered — the move/rotate/scale gizmos are never hidden. A filtered-
/// off billboard still draws when its object is the current selection.
#[derive(Clone, Default)]
struct WidgetFilter {
    lights: WidgetSetting,
    cameras: WidgetSetting,
    probes: WidgetSetting,
    audio: WidgetSetting,
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
    /// Per-file LSP problem tally (errors, warnings) for file-browser badges;
    /// keyed by absolute path, updated on every publishDiagnostics.
    file_diagnostics: HashMap<PathBuf, (u32, u32)>,
    gizmo: GizmoState,
    widget_filter: WidgetFilter,
    /// A left-drag that began on a gizmo handle: claims the whole gesture so
    /// orbit can't steal it even if the gizmo grabs a frame late.
    gizmo_drag: bool,
    /// The current left-drag began with the pointer over the viewport, so it
    /// may orbit. Drags that start on a panel never orbit.
    orbit_armed: bool,
    /// Audio playback (None if no output device). Drives AudioSource sounds
    /// in Play mode.
    audio: Option<audio::AudioEngine>,
    actions: Vec<EditorAction>,
    undo_stack: UndoStack,
    /// Set while applying undo/redo so the frame diff doesn't re-record it.
    suppress_undo_record: bool,
    /// Registered component types (built-ins now; plugins extend this).
    components: ComponentRegistry,
    /// Editor-side inspector/gizmo dispatch (built-ins + plugin editor traits).
    editor_components: EditorComponents,
    /// Play mode: components run every frame; edits aren't recorded to undo.
    playing: bool,
    /// Paused while playing: components/physics/audio freeze but the played
    /// state stays so you can inspect it. Cleared on Stop.
    play_paused: bool,
    /// Object state captured at Play start, restored at Stop.
    play_snapshot: Option<Vec<ObjectState>>,
    /// Active physics simulation (built on Play, cleared on Stop).
    physics: Option<physics::PhysicsWorld>,
    /// Set if a component switched scenes during play (via `ctx.load_scene`).
    /// Stop then reloads `play_origin_scene` instead of the snapshot restore.
    play_scene_switched: bool,
    /// Scene path active when Play started; reloaded on Stop after a runtime
    /// scene switch. None if the pre-play scene was never saved.
    play_origin_scene: Option<PathBuf>,
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
    /// Global status-bar message: (text, when set, show a spinner).
    status: Option<(String, Instant, bool)>,
    /// A plugin reload was requested; deferred one frame so the status bar can
    /// show "Compiling components…" before the (blocking) cargo build.
    reload_pending: bool,
    /// project.citrus: name, last scene, per-project engine settings.
    project: citrus_assets::ProjectFile,
    camera: FlyCamera,
    /// Orbit pivot, locked for the duration of one left-drag.
    orbit_pivot: Option<Vec3>,
    /// Right mouse held: mouse-look (cursor hidden + locked) + WASD fly.
    looking: bool,
    /// Set the frame mouse-look ends: egui's pointer was frozen during look
    /// (it never saw window events), so the next frame injects a PointerMoved
    /// to resync it — otherwise the first click is hit-tested at the stale
    /// position and the orbit-arm edge is missed (orbit dead until a 2nd click).
    look_just_ended: bool,
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
    /// Project Settings window open.
    show_project_settings: bool,
    /// New Project dialog open + its inputs (parent dir, project name).
    show_new_project: bool,
    new_project_parent: String,
    new_project_name: String,
    /// Open Project dialog open + the folder path being entered.
    show_open_project: bool,
    open_project_path: String,
    /// Baker's Man: render objects as a lightmap-UV checker (texel-density preview).
    lightmap_preview: bool,
    /// Last Build Game result message (shown in the settings/log).
    build_status: Option<String>,
    /// Scene has unsaved edits since the last save/load (drives the exit prompt).
    scene_dirty: bool,
    /// Close requested with unsaved edits: the Save/Discard/Cancel dialog is up.
    show_quit_dialog: bool,
    /// Set by the quit dialog (or a clean close) to exit after this frame; the
    /// redraw loop performs the actual `event_loop.exit()`.
    quit_after_frame: bool,
    /// Log tab filters (levels + search + autoscroll).
    log_filter: LogFilter,
    /// In-progress Light Probe Volume face-handle resize.
    probe_drag: Option<ProbeDrag>,
    audio_drag: Option<AudioDrag>,
    collider_drag: Option<ColliderDrag>,
    stats: Stats,
    #[allow(dead_code)] // entities arrive with the component-system milestone
    world: hecs::World,
    start: Instant,
    last_frame: Instant,
    /// Realtime-GI preview: temporally-accumulated probe SH from the last few
    /// updates (blended toward each new probe-only trace), the grid dimensions
    /// it was built for (a layout change forces a reset), whether RT-GI probes
    /// are currently uploaded (so we can clear them when it turns off), and a
    /// throttle timer.
    /// Realtime-GI driver (continuous probe re-trace from the realtime lights).
    rt_gi: crate::realtime_gi::RealtimeGiState,
}

impl EngineApp {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        // Global minimum text size = the folder-explorer text (13px): bump any
        // smaller text style up so nothing renders unreadably small; larger
        // styles (headings) are left alone.
        self.egui_ctx.style_mut(|style| {
            for font_id in style.text_styles.values_mut() {
                if font_id.size < 13.0 {
                    font_id.size = 13.0;
                }
            }
        });

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
            && let Err(e) = self.plugins.build_and_load(
                &self.project_root,
                &mut self.components,
                &mut self.editor_components,
            )
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
        // Load + upload the scene's baked lighting AFTER the renderer is stored,
        // so `upload_baked_probes` can actually push the lightmaps/probes to the
        // GPU (otherwise a scene with a bake loads black until re-baked).
        self.load_bake();
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
            // egui's pointer froze during look; resync it next frame.
            self.look_just_ended = true;
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
            // Mark the scene unsaved for content mutations (transform/material
            // edits are caught separately in record_edits). Saving/loading
            // clears the flag.
            if matches!(
                &action,
                EditorAction::Spawn(_)
                    | EditorAction::SpawnLight(_)
                    | EditorAction::SpawnProbeVolume
                    | EditorAction::SpawnAudioSource
                    | EditorAction::SpawnBoxCollider
                    | EditorAction::SpawnSphereCollider
                    | EditorAction::DeleteObject(_)
                    | EditorAction::DuplicateObject(_)
                    | EditorAction::ImportModel(_)
                    | EditorAction::NewScene
                    | EditorAction::ClearSkybox
            ) {
                self.scene_dirty = true;
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
                EditorAction::CreatePostFx(dir) => {
                    let path = unique_path(&dir, "new_postfx", "postfx");
                    match citrus_assets::save_postfx(
                        &path,
                        &citrus_assets::PostFxProfile::default(),
                    ) {
                        Ok(()) => self.actions.push(EditorAction::SelectFile(path)),
                        Err(e) => tracing::error!("creating postfx: {e:#}"),
                    }
                }
                EditorAction::CreateFolder(dir) => {
                    let path = unique_path(&dir, "new_folder", "");
                    if let Err(e) = std::fs::create_dir_all(&path) {
                        tracing::error!("creating folder: {e:#}");
                    }
                }
                EditorAction::PickAt(pos) => {
                    // Clicking the 3D viewport drops any lingering code-editor
                    // text focus, so keyboard shortcuts (Ctrl+Z/Y) route to the
                    // editor again instead of egui's TextEdit consuming them.
                    self.egui_ctx.memory_mut(|m| m.stop_text_input());
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
                EditorAction::CloseCodeTab(path) => {
                    if let Some(loc) = self.dock_state.find_tab(&Tab::Code(path.clone())) {
                        self.dock_state.remove_tab(loc);
                    }
                    self.open_editors.retain(|e| e.path != path);
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
                EditorAction::LspReferences(path, cursor) => {
                    if let Some(editor) = self.open_editors.iter().find(|e| e.path == path) {
                        let (line, character) = char_to_line_col(&editor.text, cursor);
                        if let Some(lsp) = self.lsp.as_mut() {
                            let id = lsp.references(&path, line, character);
                            self.lsp_requests
                                .insert(id, LspRequestKind::References { path });
                        }
                    }
                }
                EditorAction::OpenAndGoto(path, line, col) => {
                    self.open_code_file(path.clone());
                    if let Some(editor) = self.open_editors.iter_mut().find(|e| e.path == path) {
                        editor.goto = Some((line, col));
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
                                    self.scene_dirty = false;
                                    // Indices into the old scene are invalid.
                                    self.undo_stack.clear();
                                    self.apply_skybox();
                                    self.load_bake();
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
                    use citrus_core::{LightComponent, LightKind};
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
                                .push(Box::new(citrus_core::LightProbeVolume::default()));
                            self.selection = Selection::Object(index);
                        }
                        Err(e) => tracing::error!("spawning probe volume: {e:#}"),
                    }
                }
                EditorAction::SpawnAudioSource => {
                    let p = self.camera.position + self.camera.forward() * 3.0;
                    match self.scene.spawn(
                        renderer!(),
                        citrus_assets::ObjectSource::Empty,
                        "Audio Source".to_owned(),
                        p,
                    ) {
                        Ok(index) => {
                            self.scene.objects[index]
                                .components
                                .push(Box::new(citrus_core::AudioSource::default()));
                            self.selection = Selection::Object(index);
                        }
                        Err(e) => tracing::error!("spawning audio source: {e:#}"),
                    }
                }
                EditorAction::SpawnBoxCollider => {
                    let p = self.camera.position + self.camera.forward() * 3.0;
                    match self.scene.spawn(
                        renderer!(),
                        citrus_assets::ObjectSource::Empty,
                        "Box Collider".to_owned(),
                        p,
                    ) {
                        Ok(index) => {
                            self.scene.objects[index]
                                .components
                                .push(Box::new(citrus_core::BoxCollider::default()));
                            self.selection = Selection::Object(index);
                        }
                        Err(e) => tracing::error!("spawning box collider: {e:#}"),
                    }
                }
                EditorAction::SpawnSphereCollider => {
                    let p = self.camera.position + self.camera.forward() * 3.0;
                    match self.scene.spawn(
                        renderer!(),
                        citrus_assets::ObjectSource::Empty,
                        "Sphere Collider".to_owned(),
                        p,
                    ) {
                        Ok(index) => {
                            self.scene.objects[index]
                                .components
                                .push(Box::new(citrus_core::SphereCollider::default()));
                            self.selection = Selection::Object(index);
                        }
                        Err(e) => tracing::error!("spawning sphere collider: {e:#}"),
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
                EditorAction::DuplicateObject(index) => {
                    // Not undoable (like delete). Select the new copy.
                    if let Some(new_root) =
                        self.scene.duplicate_object(index, &self.components)
                    {
                        self.selection = Selection::Object(new_root);
                    }
                }
                EditorAction::DuplicateFile(path) => {
                    if let Some(dest) = duplicate_file_path(&path) {
                        match std::fs::copy(&path, &dest) {
                            Ok(_) => {
                                tracing::info!(
                                    "duplicated {} -> {}",
                                    path.display(),
                                    dest.display()
                                );
                                self.actions.push(EditorAction::SelectFile(dest));
                            }
                            Err(e) => tracing::error!("duplicating file: {e:#}"),
                        }
                    }
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
                    // Defer the blocking cargo build one frame so the status
                    // bar paints "Compiling components…" first.
                    self.set_status("Compiling components…", true);
                    self.reload_pending = true;
                }
                EditorAction::BakeLighting => self.run_bake(),
                EditorAction::ClearBake => {
                    self.scene.baked = None;
                    self.upload_baked_probes();
                    tracing::info!("cleared baked lighting");
                }
                EditorAction::NewProject { parent, name } => {
                    self.set_status(format!("creating project {name}…"), true);
                    match bundle::scaffold_project(&parent, &name) {
                        Ok(root) => {
                            tracing::info!("scaffolded project at {}", root.display());
                            self.switch_project(root);
                        }
                        Err(e) => {
                            tracing::error!("creating project: {e:#}");
                            self.set_status(format!("new project failed: {e}"), false);
                        }
                    }
                }
                EditorAction::OpenProject(root) => {
                    if root.join("project.citrus").is_file() {
                        tracing::info!("opening project {}", root.display());
                        self.switch_project(root);
                    } else {
                        self.set_status(
                            format!("no project.citrus in {}", root.display()),
                            false,
                        );
                    }
                }
                EditorAction::BuildGame => {
                    // Persist settings first so the build picks up boot_scene.
                    self.save_project();
                    self.set_status("building game…", true);
                    let project_root = self.project_root.clone();
                    let project = self.project.clone();
                    let mut lines = Vec::new();
                    let result = bundle::build_game(&project_root, &project, |msg| {
                        tracing::info!("build: {msg}");
                        lines.push(msg);
                    });
                    match result {
                        Ok(exe) => {
                            let msg = format!("built {}", exe.display());
                            tracing::info!("{msg}");
                            self.build_status = Some(msg.clone());
                            self.set_status(msg, false);
                        }
                        Err(e) => {
                            tracing::error!("build game failed: {e:#}");
                            self.build_status = Some(format!("build failed: {e}"));
                            self.set_status("build failed (see Log)", false);
                        }
                    }
                }
                EditorAction::StartLook => {
                    self.egui_ctx.memory_mut(|m| m.stop_text_input());
                    self.set_looking(true);
                }
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
                    // Materials are NOT rewritten on scene save — only manual
                    // edits persist (tracked in `dirty_materials`, flushed by
                    // `autosave_materials`, or saved via the material editor).
                    // A material with an existing `.material` file serializes as
                    // a file reference; one without stays inline in the scene.
                    let file = self.scene.to_scene_file(&self.project_root, &self.shaders);
                    match citrus_assets::save_scene_file(&path, &file) {
                        Ok(()) => {
                            tracing::info!("scene saved to {}", path.display());
                            self.current_scene_path = Some(path);
                            self.scene_dirty = false;
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
                    if ui.button("New Project…").clicked() {
                        if self.new_project_parent.is_empty() {
                            // Default new projects beside the current project.
                            self.new_project_parent = self
                                .project_root
                                .parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                        }
                        self.show_new_project = true;
                        ui.close();
                    }
                    if ui.button("Open Project…").clicked() {
                        if self.open_project_path.is_empty() {
                            self.open_project_path = self.project_root.display().to_string();
                        }
                        self.show_open_project = true;
                        ui.close();
                    }
                    if ui.button("Project Settings…").clicked() {
                        self.show_project_settings = true;
                        ui.close();
                    }
                    if ui
                        .button("Build Game")
                        .on_hover_text("cargo build --release + bundle assets into build/ (blocks the UI)")
                        .clicked()
                    {
                        self.actions.push(EditorAction::BuildGame);
                        ui.close();
                    }
                    ui.separator();
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
                        if self.scene_dirty {
                            self.show_quit_dialog = true;
                        } else {
                            self.quit_after_frame = true;
                        }
                        ui.close();
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
                    ui.separator();
                    if ui
                        .checkbox(&mut self.project.settings.vim_mode, "Vim mode")
                        .changed()
                    {
                        self.save_project();
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
                        for kind in citrus_core::LightKind::ALL {
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
                    if ui.button("Create Audio Source").clicked() {
                        self.actions.push(EditorAction::SpawnAudioSource);
                        ui.close();
                    }
                    ui.menu_button("Create Collider", |ui| {
                        if ui.button("Box Collider").clicked() {
                            self.actions.push(EditorAction::SpawnBoxCollider);
                            ui.close();
                        }
                        if ui.button("Sphere Collider").clicked() {
                            self.actions.push(EditorAction::SpawnSphereCollider);
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
                        (GizmoTool::Move, "Move        W"),
                        (GizmoTool::Rotate, "Rotate      R"),
                        (GizmoTool::Scale, "Scale       E"),
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
                    ui.separator();
                    let can_bake = self
                        .renderer
                        .as_ref()
                        .is_some_and(|r| r.supports_baking());
                    let baked = self.scene.baked.is_some();
                    if ui
                        .add_enabled(can_bake, egui::Button::new("Bake Lighting"))
                        .on_hover_text(if can_bake {
                            "Ray-trace lightmaps + probes for Static objects and probe volumes (blocks the UI)"
                        } else {
                            "This GPU has no ray-query support; baking is unavailable"
                        })
                        .clicked()
                    {
                        self.actions.push(EditorAction::BakeLighting);
                        ui.close();
                    }
                    if ui
                        .add_enabled(baked, egui::Button::new("Clear Baked Lighting"))
                        .clicked()
                    {
                        self.actions.push(EditorAction::ClearBake);
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
                        (Tab::Baker, "Baker's Man"),
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
                // Pause/Resume: freeze components/physics/audio while staying in
                // Play so the played state can be inspected.
                if self.playing {
                    let plabel = if self.play_paused { "▶ Resume" } else { "⏸ Pause" };
                    if ui
                        .add(egui::Button::new(plabel).fill(if self.play_paused {
                            ui.visuals().selection.bg_fill
                        } else {
                            ui.visuals().widgets.inactive.bg_fill
                        }))
                        .on_hover_text("Freeze play to inspect; Resume continues")
                        .clicked()
                    {
                        self.play_paused = !self.play_paused;
                    }
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
                    ui.label("W / E / R — gizmo move / scale / rotate (buttons top-left)");
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
                        references: None,
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

    /// True when the active/focused dock tab is a code editor — used to keep
    /// Escape from doing the global deselect while editing (Escape is the vim
    /// Insert -> Normal key there).
    fn code_tab_focused(&mut self) -> bool {
        self.dock_state
            .find_active_focused()
            .map(|(_, tab)| matches!(tab, Tab::Code(_)))
            .unwrap_or(false)
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
                        // Project-wide tally so the file browser can badge files
                        // (and folders) that have problems, even unopened ones.
                        let errors = diags.iter().filter(|d| d.severity <= 1).count() as u32;
                        let warns = diags.len() as u32 - errors;
                        if errors == 0 && warns == 0 {
                            self.file_diagnostics.remove(&path);
                        } else {
                            self.file_diagnostics.insert(path.clone(), (errors, warns));
                        }
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

    /// Push the scene's baked probe SH + volume metadata to the renderer (set-0
    /// binding 2) so the standard shader samples it per fragment. Empty when no
    /// bake, reverting fragments to flat ambient.
    fn upload_baked_probes(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        match &self.scene.baked {
            Some(b) => {
                let vols: Vec<_> = b
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
                renderer.set_baked_probes(&b.probe_sh, &vols);
                renderer.set_baked_lightmaps(&b.lightmaps);
            }
            None => {
                renderer.set_baked_probes(&[], &[]);
                renderer.set_baked_lightmaps(&[]);
            }
        }
    }

    /// Gather the scene's static geometry, baked lights, and probe volumes,
    /// run the GPU lighting bake, and store the result for runtime sampling.
    /// Blocks the UI while the GPU traces (off the hot path; explicit action).
    fn run_bake(&mut self) {
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        if !renderer.supports_baking() {
            tracing::warn!("this GPU can't ray trace; lighting bake unavailable");
            return;
        }
        let gather = self.scene.gather_bake();
        if gather.instances.is_empty() && gather.probes.is_empty() {
            tracing::warn!(
                "nothing to bake — mark objects Static and/or add a Light Probe Volume"
            );
            return;
        }
        let settings = self.scene.environment.bake;
        let input = citrus_render::BakeInput {
            instances: &gather.instances,
            lights: &gather.lights,
            probes: &gather.probes,
            sky_color: gather.sky_color,
            bounces: settings.bounces,
            samples: settings.samples,
            probes_only: false,
        };
        tracing::info!(
            "baking: {} static instance(s), {} baked light(s), {} probe(s)…",
            gather.instances.len(),
            gather.lights.len(),
            gather.probes.len()
        );
        let started = Instant::now();
        let output = match renderer.bake_lighting(&input) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("lighting bake failed: {e:#}");
                return;
            }
        };
        tracing::info!(
            "bake complete: {} lightmap(s), {} probe(s) in {:.2}s",
            output.lightmaps.len(),
            output.probes.len(),
            started.elapsed().as_secs_f32()
        );
        let mut object_lightmap = std::collections::HashMap::new();
        for (layer, &obj) in gather.instance_objects.iter().enumerate() {
            object_lightmap.insert(obj, layer);
        }
        let baked = scene::BakedData {
            object_lightmap,
            lightmaps: output.lightmaps,
            probe_volumes: gather.probe_volumes,
            probe_sh: output.probes,
        };
        // Runtime sampling (Phase 5) uploads `baked` to the renderer; until
        // then the bake result lives on the scene (and serializes with it).
        self.scene.baked = Some(baked);
        self.upload_baked_probes();
        self.save_bake();
    }

    /// Realtime-GI preview: while enabled (and the scene isn't baked), re-trace
    /// the auto probe grid from the realtime lights every ~0.2s, blend toward
    /// the previous result (temporal smoothing), and upload so un-baked surfaces
    /// show live indirect bounce. Reuses the bake path tracer (`probes_only`).
    fn update_realtime_gi(&mut self, dt: f32) {
        if let Some(renderer) = self.renderer.as_mut() {
            self.rt_gi.update(renderer, &mut self.scene, dt);
        }
    }

    /// Base path (no extension) for this scene's `.lightmap` / `.lightdata`
    /// sidecars: next to the scene file, or `baked/untitled` if unsaved.
    fn bake_base_path(&self) -> PathBuf {
        match &self.current_scene_path {
            Some(p) => p.with_extension(""),
            None => self.project_root.join("baked/untitled"),
        }
    }

    /// Write the baked lightmaps (`.lightmap`) and probe data (`.lightdata`).
    fn save_bake(&self) {
        let Some(baked) = &self.scene.baked else {
            return;
        };
        let base = self.bake_base_path();

        // .lightmap — static GI, one entry per lit object.
        let mut lm = citrus_assets::LightmapFile::default();
        for (&object, &layer) in &baked.object_lightmap {
            if let Some(map) = baked.lightmaps.get(layer) {
                lm.entries.push(citrus_assets::LightmapEntry {
                    object: object as u32,
                    size: map.size,
                    pixels: map.pixels.clone(),
                });
            }
        }
        if let Err(e) = citrus_assets::save_lightmaps(base.with_extension("lightmap"), &lm) {
            tracing::error!("saving .lightmap: {e:#}");
        }

        // .lightdata — probe volumes + SH for dynamic objects.
        let ld = citrus_assets::LightDataFile {
            volumes: baked
                .probe_volumes
                .iter()
                .map(|v| citrus_assets::ProbeVolumeData {
                    world_to_local: v.world_to_local.to_cols_array(),
                    size: v.size,
                    counts: [v.counts[0] as u32, v.counts[1] as u32, v.counts[2] as u32],
                    sh_base: v.sh_base as u32,
                })
                .collect(),
            probes: baked
                .probe_sh
                .iter()
                .map(|sh| {
                    let c = &sh.coeffs;
                    [
                        c[0][0], c[0][1], c[0][2], c[1][0], c[1][1], c[1][2], c[2][0], c[2][1],
                        c[2][2], c[3][0], c[3][1], c[3][2],
                    ]
                })
                .collect(),
        };
        if let Err(e) = citrus_assets::save_lightdata(base.with_extension("lightdata"), &ld) {
            tracing::error!("saving .lightdata: {e:#}");
        }
        tracing::info!(
            "wrote {} / {}",
            base.with_extension("lightmap").display(),
            base.with_extension("lightdata").display()
        );
    }

    /// Load `.lightmap` / `.lightdata` sidecars for the current scene, if they
    /// exist, into `scene.baked`. Missing files are not an error.
    fn load_bake(&mut self) {
        let base = self.bake_base_path();
        self.scene.load_bake_sidecars(&base);
        self.upload_baked_probes();
    }

    fn toggle_play(&mut self) {
        self.play_paused = false;
        if self.playing {
            self.playing = false;
            self.physics = None;
            if let Some(audio) = self.audio.as_mut() {
                audio.stop_all();
            }
            if self.play_scene_switched {
                // A component switched scenes during play; the snapshot indices
                // no longer match the loaded scene. Return to the scene we
                // started from (reloaded from disk — unsaved pre-play edits are
                // lost, a known v1 limitation).
                self.play_scene_switched = false;
                self.play_snapshot = None;
                match self.play_origin_scene.take() {
                    Some(origin) => self.load_scene_runtime(&origin),
                    None => tracing::warn!(
                        "scene switched during play from an unsaved scene; staying put"
                    ),
                }
            } else if let Some(snapshot) = self.play_snapshot.take() {
                let registry = &self.components;
                for (object, state) in self.scene.objects.iter_mut().zip(snapshot) {
                    object.translation = state.translation;
                    object.rotation = state.rotation;
                    object.scale = state.scale;
                    object.load_components(&state.components, registry);
                }
            }
        } else {
            self.play_snapshot = Some(self.scene.objects.iter().map(object_state).collect());
            self.play_scene_switched = false;
            self.play_origin_scene = None;
            self.playing = true;
            self.physics = Some(physics::PhysicsWorld::build(&self.scene));
            let mut commands = Vec::new();
            self.scene
                .start_components(self.start.elapsed().as_secs_f32(), &mut commands);
            // Start play-on-start audio sources.
            if let Some(audio) = self.audio.as_mut() {
                let cues = self.scene.gather_audio();
                let listener = self
                    .scene
                    .audio_listener()
                    .unwrap_or(self.camera.position);
                audio.start(&cues, listener, &self.project_root);
            }
            // A start hook may have requested a scene switch.
            self.apply_component_commands(commands);
        }
    }

    /// Apply deferred component requests gathered during a lifecycle pass. The
    /// last `LoadScene` wins (loading once); a switch during play continues
    /// playing in the new scene.
    fn apply_component_commands(&mut self, commands: Vec<ComponentCommand>) {
        let load = commands
            .into_iter()
            .rev()
            .find_map(|c| match c {
                ComponentCommand::LoadScene(rel) => Some(rel),
            });
        let Some(rel) = load else {
            return;
        };
        if self.playing && !self.play_scene_switched {
            self.play_origin_scene = self.current_scene_path.clone();
            self.play_scene_switched = true;
        }
        let path = self.project_root.join(&rel);
        self.load_scene_runtime(&path);
        if self.playing {
            // The new scene needs a fresh physics world (old body indices are stale).
            self.physics = Some(physics::PhysicsWorld::build(&self.scene));
            // Run the new scene's start hooks + audio so play continues there.
            let mut commands = Vec::new();
            self.scene
                .start_components(self.start.elapsed().as_secs_f32(), &mut commands);
            if let Some(audio) = self.audio.as_mut() {
                let cues = self.scene.gather_audio();
                let listener = self.scene.audio_listener().unwrap_or(self.camera.position);
                audio.start(&cues, listener, &self.project_root);
            }
            // Chained switches from those start hooks.
            self.apply_component_commands(commands);
        }
    }

    /// Load a scene by absolute path, replacing the current one. Used by
    /// runtime (play-mode) scene switches and Stop-restore. Does not run start
    /// hooks (the caller decides) and does not persist to project.citrus.
    /// New Project + Project Settings modal windows.
    fn project_windows(&mut self, ctx: &egui::Context) {
        // ---- New Project ----
        let mut open = self.show_new_project;
        let mut do_create = false;
        egui::Window::new("New Project")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Scaffold a new citrus project (scenes / materials / shaders / src + a starter scene).");
                ui.add_space(6.0);
                egui::Grid::new("new-project-grid").num_columns(2).show(ui, |ui| {
                    ui.label("Parent folder");
                    ui.text_edit_singleline(&mut self.new_project_parent);
                    ui.end_row();
                    ui.label("Project name");
                    ui.text_edit_singleline(&mut self.new_project_name);
                    ui.end_row();
                });
                ui.add_space(4.0);
                let target = Path::new(&self.new_project_parent).join(&self.new_project_name);
                ui.label(egui::RichText::new(format!("→ {}", target.display())).small().weak());
                ui.add_space(6.0);
                let valid = !self.new_project_parent.trim().is_empty()
                    && !self.new_project_name.trim().is_empty()
                    && Path::new(&self.new_project_parent).is_dir();
                if !valid {
                    ui.label(
                        egui::RichText::new("Pick an existing parent folder and a name.")
                            .small()
                            .color(egui::Color32::from_rgb(220, 160, 90)),
                    );
                }
                if ui.add_enabled(valid, egui::Button::new("Create & Open")).clicked() {
                    do_create = true;
                }
            });
        self.show_new_project = open && !do_create;
        if do_create {
            self.actions.push(EditorAction::NewProject {
                parent: PathBuf::from(self.new_project_parent.clone()),
                name: self.new_project_name.trim().to_string(),
            });
        }

        // ---- Open Project ----
        let mut open = self.show_open_project;
        let mut do_open = false;
        egui::Window::new("Open Project")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Open an existing citrus project folder (one containing project.citrus).");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Folder");
                    ui.text_edit_singleline(&mut self.open_project_path);
                });
                let dir = Path::new(&self.open_project_path);
                let valid = dir.join("project.citrus").is_file();
                ui.add_space(4.0);
                if !valid && !self.open_project_path.trim().is_empty() {
                    ui.label(
                        egui::RichText::new("No project.citrus in that folder.")
                            .small()
                            .color(egui::Color32::from_rgb(220, 160, 90)),
                    );
                }
                ui.add_space(6.0);
                if ui.add_enabled(valid, egui::Button::new("Open")).clicked() {
                    do_open = true;
                }
            });
        self.show_open_project = open && !do_open;
        if do_open {
            self.actions
                .push(EditorAction::OpenProject(PathBuf::from(self.open_project_path.clone())));
        }

        // ---- Project Settings ----
        let mut open = self.show_project_settings;
        let mut dirty = false;
        egui::Window::new("Project Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| {
                egui::Grid::new("project-settings-grid").num_columns(2).show(ui, |ui| {
                    ui.label("Name");
                    dirty |= ui.text_edit_singleline(&mut self.project.name).changed();
                    ui.end_row();

                    ui.label("Starting scene")
                        .on_hover_text("The scene a built game loads first (boot scene).");
                    let scenes = scan_project_files(&self.project_root, "scene");
                    let current = self
                        .project
                        .boot_scene
                        .clone()
                        .unwrap_or_else(|| "(none)".into());
                    egui::ComboBox::from_id_salt("boot-scene")
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for rel in &scenes {
                                if ui
                                    .selectable_label(
                                        self.project.boot_scene.as_deref() == Some(rel.as_str()),
                                        rel,
                                    )
                                    .clicked()
                                {
                                    self.project.boot_scene = Some(rel.clone());
                                    dirty = true;
                                }
                            }
                            if scenes.is_empty() {
                                ui.label(egui::RichText::new("no .scene files").small().weak());
                            }
                        });
                    ui.end_row();
                });
                ui.add_space(8.0);
                ui.separator();
                if let Some(status) = &self.build_status {
                    ui.label(egui::RichText::new(status).small());
                }
                if ui
                    .button("Build Game")
                    .on_hover_text("cargo build --release + bundle into build/")
                    .clicked()
                {
                    self.actions.push(EditorAction::BuildGame);
                }
            });
        self.show_project_settings = open;
        if dirty {
            self.save_project();
        }

        // ---- Unsaved-changes (quit) dialog ----
        if self.show_quit_dialog {
            let mut open = true;
            egui::Window::new("Unsaved changes")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label("This scene has unsaved changes. Save before quitting?");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save & Quit").clicked() {
                            self.actions.push(EditorAction::SaveScene(None));
                            self.quit_after_frame = true;
                            self.show_quit_dialog = false;
                        }
                        if ui.button("Discard & Quit").clicked() {
                            self.quit_after_frame = true;
                            self.show_quit_dialog = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_quit_dialog = false;
                        }
                    });
                });
            // Window close (✕) acts as Cancel.
            if !open {
                self.show_quit_dialog = false;
            }
        }
    }

    /// Point the editor at a different project root: reload its project file,
    /// file browser, plugins, and boot/last scene. Used after New Project.
    fn switch_project(&mut self, root: PathBuf) {
        self.project_root = root.clone();
        self.file_browser = FileBrowser::new(root.clone());
        self.current_scene_path = None;
        self.scene_dirty = false;
        self.selection = Selection::None;
        self.file_material = None;
        self.undo_stack.clear();
        self.shaders = ShaderLibrary::default();
        self.scene = LoadedScene::empty();

        // Fresh registry + plugins for the new project.
        self.components = citrus_core::ComponentRegistry::with_builtins();
        self.editor_components = citrus_editor::EditorComponents::with_builtins();
        self.plugins = plugins::PluginHost::default();
        self.plugin_build_error = None;
        if plugins::PluginHost::any_plugins(&root) {
            if let Err(e) = self.plugins.build_and_load(
                &root,
                &mut self.components,
                &mut self.editor_components,
            ) {
                tracing::error!("building plugins for new project: {e:#}");
                self.plugin_build_error = Some(format!("{e:#}"));
            }
        }

        self.project = citrus_assets::load_project_file(&root).unwrap_or_default();
        let boot = self
            .project
            .boot_scene
            .clone()
            .or_else(|| self.project.last_scene.clone());
        if let Some(rel) = boot {
            let abs = root.join(&rel);
            self.load_scene_runtime(&abs);
        }
        if let Some(window) = self.window.as_ref() {
            window.set_title(&format!("{} — citrus", self.project.name));
        }
        self.set_status(format!("opened project {}", self.project.name), false);
    }

    fn load_scene_runtime(&mut self, path: &Path) {
        let file = match citrus_assets::load_scene_file(path) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("load scene {path:?}: {e:#}");
                return;
            }
        };
        if self.renderer.is_none() {
            return;
        }
        if let Err(e) = self.renderer.as_mut().unwrap().reset_scene() {
            tracing::error!("resetting scene: {e:#}");
        }
        let scene = match LoadedScene::load_scene_file(
            self.renderer.as_mut().unwrap(),
            &file,
            &self.project_root,
            &self.components,
            &mut self.shaders,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("load scene {path:?}: {e:#}");
                return;
            }
        };
        self.scene = scene;
        self.selection = Selection::None;
        self.file_material = None;
        self.current_scene_path = Some(path.to_path_buf());
        self.scene_dirty = false;
        self.undo_stack.clear();
        self.apply_skybox();
        self.load_bake();
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
    /// Set the global status-bar message (`spinner` shows a busy indicator).
    fn set_status(&mut self, msg: impl Into<String>, spinner: bool) {
        self.status = Some((msg.into(), Instant::now(), spinner));
    }

    /// Build + reload the component plugins (blocking cargo build), then
    /// re-instantiate scene components from their serialized state.
    fn do_reload_plugins(&mut self) {
        match self.plugins.build_and_load(
            &self.project_root,
            &mut self.components,
            &mut self.editor_components,
        ) {
            Ok(names) => {
                self.plugin_build_error = None;
                for object in &mut self.scene.objects {
                    let saved = object.save_components();
                    object.load_components(&saved, &self.components);
                }
                tracing::info!("reloaded plugins: {names:?}");
                self.set_status(format!("Components reloaded ({})", names.len()), false);
            }
            Err(e) => {
                tracing::error!("reloading plugins: {e:#}");
                self.plugin_build_error = Some(format!("{e:#}"));
                self.set_status("Component build failed", false);
            }
        }
    }

    /// Bottom status bar for the whole editor: project + object count on the
    /// left, current activity (rust-analyzer analyzing, compiling, last result)
    /// on the right.
    fn status_bar(&mut self, ctx: &egui::Context) {
        let lsp_busy = !self.lsp_requests.is_empty();
        let recent = self
            .status
            .as_ref()
            .filter(|(_, t, _)| t.elapsed().as_secs_f32() < 6.0)
            .map(|(m, _, s)| (m.clone(), *s));
        let project = self.project.name.clone();
        let objects = self.scene.objects.len();
        egui::TopBottomPanel::bottom("citrus-status-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{project}  ·  {objects} objects")).size(14.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if lsp_busy {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(egui::RichText::new("rust-analyzer").size(14.0));
                    } else if let Some((msg, spinner)) = recent {
                        if spinner {
                            ui.add(egui::Spinner::new().size(14.0));
                        }
                        ui.label(egui::RichText::new(msg).size(14.0));
                    } else {
                        ui.label(egui::RichText::new("Ready").size(14.0).weak());
                    }
                });
            });
        });
    }

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
                self.set_status(format!("Reloaded {} shader(s)", changed.len()), false);
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
                    self.scene_dirty = true;
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
                        self.scene_dirty = true;
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
        self.update_realtime_gi(dt);

        // If mouse-look just ended, egui's pointer state is stale: window
        // events are withheld while looking, so egui (a) never saw the right
        // button RELEASE — it still thinks Secondary is held, which keeps
        // `press_origin` pinned and breaks drag detection for the next gesture
        // (a click still registers, but orbit/gizmo/widget drags don't) — and
        // (b) has a frozen pointer position. Inject both a Secondary release
        // and a move at the true cursor so the next press starts a clean drag.
        let resync_pos = if std::mem::take(&mut self.look_just_ended) {
            let ppp = self.egui_ctx.pixels_per_point();
            self.last_cursor
                .map(|(x, y)| egui::pos2(x as f32 / ppp, y as f32 / ppp))
        } else {
            None
        };

        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(egui_state) = self.egui_state.as_mut() else {
            return;
        };

        let mut raw_input = egui_state.take_egui_input(&window);
        if let Some(pos) = resync_pos {
            raw_input.events.push(egui::Event::PointerMoved(pos));
            raw_input.events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
        }
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
        let can_bake = self.renderer.as_ref().is_some_and(|r| r.supports_baking());
        // Winit-based viewport-hover test: stays correct right after a
        // cursor-grab (mouse-look) ends, when egui's own pointer state is
        // briefly stale until the next move. Used to re-arm orbit/dolly.
        let pointer_in_viewport = self.cursor_in_viewport();
        // Picker entries: standard + every project .frag.
        let mut shader_list: Vec<String> = Vec::with_capacity(self.shader_files.len() + 1);
        shader_list.push("standard".into());
        shader_list.extend(self.shader_files.iter().cloned());

        let egui_ctx = self.egui_ctx.clone();
        let output = egui_ctx.run(raw_input, |ctx| {
            self.menu_bar(ctx);
            self.project_windows(ctx);
            self.status_bar(ctx);
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
                file_diagnostics: &self.file_diagnostics,
                editor_components: &self.editor_components,
                file_material: &mut self.file_material,
                open_editors: &mut self.open_editors,
                focused_code,
                gizmo: &mut self.gizmo,
                widget_filter: &mut self.widget_filter,
                gizmo_drag: &mut self.gizmo_drag,
                orbit_armed: &mut self.orbit_armed,
                pointer_in_viewport,
                actions: &mut self.actions,
                viewport_rect: &mut self.viewport_rect,
                registry: &self.components,
                shader_list: &shader_list,
                shader_info: shader_info.as_ref(),
                camera_preview,
                view,
                proj,
                looking: self.looking,
                vim_mode: self.project.settings.vim_mode,
                log_filter: &mut self.log_filter,
                probe_drag: &mut self.probe_drag,
                audio_drag: &mut self.audio_drag,
                collider_drag: &mut self.collider_drag,
                can_bake,
                lightmap_preview: &mut self.lightmap_preview,
            };
            DockArea::new(&mut dock_state)
                .style(egui_dock::Style::from_egui(ctx.style().as_ref()))
                // Per-tab close X (only on closeable tabs = Code); no group
                // close-all button.
                .show_close_buttons(true)
                .show_leaf_close_all_buttons(false)
                .show(ctx, &mut tabs);
            self.dock_state = dock_state;

            // Clear the published "dragged object" once the pointer is up, after
            // ObjectRef drop boxes have had this frame to consume it (so a later
            // plain release over a box can't re-apply a stale drag).
            if !ctx.input(|i| i.pointer.any_down()) {
                ctx.data_mut(|d| d.remove::<usize>(egui::Id::new(citrus_editor::DRAG_OBJECT_KEY)));
            }
        });

        if let Some(egui_state) = self.egui_state.as_mut() {
            egui_state.handle_platform_output(&window, output.platform_output);
        }
        let primitives = egui_ctx.tessellate(output.shapes, output.pixels_per_point);

        self.process_actions();
        // Exit requested by a clean close or the unsaved-changes dialog. Runs
        // after process_actions so a "Save & Quit" SaveScene lands first.
        if self.quit_after_frame {
            self.save_project();
            event_loop.exit();
            return;
        }
        self.pump_lsp();
        let t = self.start.elapsed().as_secs_f32();
        if self.playing && !self.play_paused {
            // Component-driven motion must not land in undo history; play
            // edits are restored wholesale on Stop anyway.
            let mut commands = Vec::new();
            self.scene.update_components(dt, t, &mut commands);
            // Physics: step after component logic, then write the simulated
            // transforms back onto dynamic/kinematic objects.
            if let Some(phys) = self.physics.as_mut()
                && !phys.is_empty()
            {
                phys.step(dt);
                phys.sync_back(&mut self.scene);
            }
            // Spatialize audio against the listener (moves with components).
            if let Some(audio) = self.audio.as_mut() {
                let cues = self.scene.gather_audio();
                let listener = self.scene.audio_listener().unwrap_or(self.camera.position);
                audio.update(&cues, listener);
            }
            // Apply deferred requests (e.g. ctx.load_scene) after the update
            // pass so the scene isn't swapped mid-iteration.
            self.apply_component_commands(commands);
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
        // When a bake exists the environment sun is baked, so it leaves the
        // realtime path (like Baked/Mixed lights).
        let baked = self.scene.baked.is_some();
        let sun_realtime = env.sun_enabled && !baked;
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
            // Baked probe ambient (which already folds in sky + baked lights)
            // replaces the flat env ambient once a bake with probes exists.
            ambient: self.scene.baked_ambient().unwrap_or([
                env.ambient[0] * env.ambient_intensity,
                env.ambient[1] * env.ambient_intensity,
                env.ambient[2] * env.ambient_intensity,
            ]),
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
        let postfx = self
            .scene
            .effective_postfx(self.camera.position, &self.project_root);
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
            lightmap_preview: self.lightmap_preview,
            postfx,
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

        // Deferred plugin reload: this frame already painted "Compiling
        // components…", so run the blocking cargo build now (the next frame
        // shows the result).
        if self.reload_pending {
            self.reload_pending = false;
            self.do_reload_plugins();
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
    file_diagnostics: &'a HashMap<PathBuf, (u32, u32)>,
    editor_components: &'a EditorComponents,
    vim_mode: bool,
    file_material: &'a mut Option<FileMaterial>,
    open_editors: &'a mut Vec<OpenEditor>,
    /// Path of the focused code tab (drives Inspector diagnostics).
    focused_code: Option<PathBuf>,
    gizmo: &'a mut GizmoState,
    widget_filter: &'a mut WidgetFilter,
    gizmo_drag: &'a mut bool,
    orbit_armed: &'a mut bool,
    /// Winit-based "pointer is over the viewport" (robust after cursor-grab).
    pointer_in_viewport: bool,
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
    audio_drag: &'a mut Option<AudioDrag>,
    collider_drag: &'a mut Option<ColliderDrag>,
    /// GPU can ray-trace (Baker tab enables its Bake button).
    can_bake: bool,
    /// Baker tab: lightmap-UV checker preview toggle.
    lightmap_preview: &'a mut bool,
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
            Tab::Baker => "Baker's Man".into(),
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
            Tab::Baker => self.baker_ui(ui),
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
                if let Some((index, name)) = response.rename
                    && index < self.scene.objects.len()
                {
                    // Caught by record_edits (dirty flag + undo) since F2 only
                    // renames the selected object, which is the edit snapshot.
                    self.scene.objects[index].name = name;
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
                        SpawnKind::AudioSource => {
                            self.actions.push(EditorAction::SpawnAudioSource);
                            return;
                        }
                        SpawnKind::BoxCollider => {
                            self.actions.push(EditorAction::SpawnBoxCollider);
                            return;
                        }
                        SpawnKind::SphereCollider => {
                            self.actions.push(EditorAction::SpawnSphereCollider);
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
                        &mut editor.references,
                        self.vim_mode,
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
                    if response.close_requested {
                        self.actions
                            .push(EditorAction::CloseCodeTab(path.clone()));
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
                    if let Some(cursor) = response.request_references {
                        self.actions
                            .push(EditorAction::LspReferences(path.clone(), cursor));
                    }
                    if let Some((target, line, col)) = response.goto_location {
                        self.actions.push(EditorAction::OpenAndGoto(target, line, col));
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
                let response = self
                    .file_browser
                    .ui(ui, selected.as_deref(), self.file_diagnostics);
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
                if let Some(dir) = response.create_postfx_in {
                    self.actions.push(EditorAction::CreatePostFx(dir));
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

    /// Radial direction (camera-right, world space) used to place audio range
    /// handles so they always face the viewer.
    fn camera_right(&self) -> Vec3 {
        self.view.inverse().x_axis.truncate().normalize_or(Vec3::X)
    }

    /// Drive AudioSource min/max-distance sphere resizing: a press on a radius
    /// handle starts a drag that changes that distance; takes priority over
    /// orbit (like the probe handles).
    fn audio_range_interaction(
        &mut self,
        response: &egui::Response,
        cursor: Option<egui::Pos2>,
        alt: bool,
    ) {
        // Continue / end an in-progress drag.
        if let Some(drag) = self.audio_drag.as_ref() {
            if !response.dragged_by(egui::PointerButton::Primary) {
                *self.audio_drag = None;
                return;
            }
            let Some(cur) = cursor else { return };
            let delta = cur - drag.start_cursor;
            let len2 = drag.screen_axis.length_sq();
            let meters = if len2 > 1e-6 {
                delta.dot(drag.screen_axis) / len2
            } else {
                0.0
            };
            let new_radius = (drag.start_radius + meters).max(0.01);
            let (object, max) = (drag.object, drag.max);
            if let Some(src) = self.scene.objects[object]
                .components
                .iter_mut()
                .find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::AudioSource>())
            {
                if max {
                    src.max_distance = new_radius.max(src.min_distance + 0.01);
                } else {
                    src.min_distance = new_radius.min(src.max_distance - 0.01).max(0.0);
                }
            }
            return;
        }

        // Maybe start a drag on a min/max handle.
        if !response.drag_started_by(egui::PointerButton::Primary) || alt {
            return;
        }
        let Selection::Object(i) = *self.selection else {
            return;
        };
        let Some(press) = cursor else { return };
        if self.gizmo.pick_preview(press) {
            return;
        }
        let Some(src) = self.scene.objects[i]
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<citrus_core::AudioSource>())
        else {
            return;
        };
        if !src.spatial {
            return;
        }
        let full_rect = response.ctx.viewport_rect();
        let origin = self.scene.world_transform(i).w_axis.truncate();
        let right = self.camera_right();
        let mut best: Option<(f32, AudioDrag)> = None;
        for (max, radius) in [(false, src.min_distance), (true, src.max_distance)] {
            let handle = origin + right * radius;
            let Some(hs) = self.world_to_screen(handle, full_rect) else {
                continue;
            };
            let dist = hs.distance(press);
            if dist > 14.0 {
                continue;
            }
            let Some(plus) = self.world_to_screen(handle + right, full_rect) else {
                continue;
            };
            let drag = AudioDrag {
                object: i,
                max,
                start_radius: radius,
                screen_axis: plus - hs,
                start_cursor: press,
            };
            if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                best = Some((dist, drag));
            }
        }
        if let Some((_, drag)) = best {
            *self.audio_drag = Some(drag);
        }
    }

    /// Drive Box/Sphere collider handle resizing: box faces grow the size and
    /// shift the collider's center to keep the opposite face fixed; the sphere
    /// handle changes the radius. Takes priority over orbit.
    fn collider_interaction(
        &mut self,
        response: &egui::Response,
        cursor: Option<egui::Pos2>,
        alt: bool,
    ) {
        // Continue / end a drag.
        if let Some(drag) = self.collider_drag.as_ref() {
            if !response.dragged_by(egui::PointerButton::Primary) {
                *self.collider_drag = None;
                return;
            }
            let Some(cur) = cursor else { return };
            let delta = cur - drag.start_cursor;
            let len2 = drag.screen_axis.length_sq();
            let world_m = if len2 > 1e-6 {
                delta.dot(drag.screen_axis) / len2
            } else {
                0.0
            };
            let object = drag.object;
            match drag.handle {
                ColliderHandle::BoxFace(axis, sign) => {
                    let local_delta = if drag.scale_a.abs() > 1e-5 {
                        world_m / drag.scale_a
                    } else {
                        0.0
                    };
                    let new_size = (drag.start_size[axis] + local_delta).max(0.01);
                    let applied = new_size - drag.start_size[axis];
                    if let Some(b) = self.scene.objects[object]
                        .components
                        .iter_mut()
                        .find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::BoxCollider>())
                    {
                        b.size[axis] = new_size;
                        // Shift center by half the growth so the opposite face
                        // stays put.
                        b.center[axis] = drag.start_center[axis] + sign * applied * 0.5;
                    }
                }
                ColliderHandle::SphereRadius => {
                    let new_radius = (drag.start_radius + world_m).max(0.01);
                    if let Some(s) = self.scene.objects[object]
                        .components
                        .iter_mut()
                        .find_map(|c| c.as_any_mut().downcast_mut::<citrus_core::SphereCollider>())
                    {
                        s.radius = new_radius;
                    }
                }
            }
            return;
        }

        // Maybe start a drag.
        if !response.drag_started_by(egui::PointerButton::Primary) || alt {
            return;
        }
        let Selection::Object(i) = *self.selection else {
            return;
        };
        let Some(press) = cursor else { return };
        if self.gizmo.pick_preview(press) {
            return;
        }
        let full_rect = response.ctx.viewport_rect();
        let world = self.scene.world_transform(i);
        let (w_scale, _, _) = world.to_scale_rotation_translation();

        // Box faces.
        if let Some((center, size)) = self.scene.objects[i]
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<citrus_core::BoxCollider>())
            .map(|b| (b.center, b.size))
        {
            let half = Vec3::from(size) * 0.5;
            let c = Vec3::from(center);
            let axes = [Vec3::X, Vec3::Y, Vec3::Z];
            let mut best: Option<(f32, ColliderDrag)> = None;
            for axis in 0..3 {
                for sign in [-1.0f32, 1.0] {
                    let face_local = c + axes[axis] * (sign * half[axis]);
                    let face_world = world.transform_point3(face_local);
                    let Some(fs) = self.world_to_screen(face_world, full_rect) else {
                        continue;
                    };
                    let dist = fs.distance(press);
                    if dist > 14.0 {
                        continue;
                    }
                    let outward = world.transform_vector3(axes[axis]) * sign;
                    let Some(plus) = self.world_to_screen(face_world + outward, full_rect) else {
                        continue;
                    };
                    let drag = ColliderDrag {
                        object: i,
                        handle: ColliderHandle::BoxFace(axis, sign),
                        start_center: center,
                        start_size: size,
                        start_radius: 0.0,
                        scale_a: w_scale[axis],
                        screen_axis: plus - fs,
                        start_cursor: press,
                    };
                    if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                        best = Some((dist, drag));
                    }
                }
            }
            if let Some((_, drag)) = best {
                *self.collider_drag = Some(drag);
                return;
            }
        }

        // Sphere radius handle (camera-right side).
        if let Some((center, radius)) = self.scene.objects[i]
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<citrus_core::SphereCollider>())
            .map(|s| (s.center, s.radius))
        {
            let origin = world.transform_point3(Vec3::from(center));
            let right = self.camera_right();
            let handle = origin + right * radius;
            if let Some(hs) = self.world_to_screen(handle, full_rect) {
                if hs.distance(press) <= 14.0 {
                    if let Some(plus) = self.world_to_screen(handle + right, full_rect) {
                        *self.collider_drag = Some(ColliderDrag {
                            object: i,
                            handle: ColliderHandle::SphereRadius,
                            start_center: center,
                            start_size: [0.0; 3],
                            start_radius: radius,
                            scale_a: 1.0,
                            screen_axis: plus - hs,
                            start_cursor: press,
                        });
                    }
                }
            }
        }
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
                        .downcast_mut::<citrus_core::LightProbeVolume>()
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
            .find_map(|c| c.as_any().downcast_ref::<citrus_core::LightProbeVolume>())
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
                if dist > 14.0 {
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
            ui.checkbox(&mut f.wrap, "Wrap")
                .on_hover_text("Wrap long lines instead of scrolling horizontally");
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
        let wrap = f.wrap;
        // Snapshot the matching entries under the lock, then release it before
        // rendering. Each entry is one (color, full-message) pair; when wrap is
        // off the messages are flattened to one row per physical line so the
        // virtualized list keeps a uniform row height.
        // (color, timestamp, rest) — split so wrapped lines can hang-indent
        // past the timestamp column.
        let mut entries: Vec<(egui::Color32, String, String)> = Vec::new();
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
                entries.push((color, e.time.clone(), format!("{tag}  {short}: {}", e.message)));
            }
        }

        let scroll = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(f.autoscroll);
        let weak = ui.visuals().weak_text_color();
        if wrap {
            // Wrapping breaks uniform row height, so render plainly (not
            // virtualized). The timestamp sits in its own column and the rest
            // wraps beside it, so continuation lines hang-indent past the time.
            // Labels are selectable so the log can be copied.
            scroll.show(ui, |ui| {
                ui.style_mut().interaction.selectable_labels = true;
                ui.spacing_mut().item_spacing.y = 2.0;
                for (color, time, rest) in &entries {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.add(
                            egui::Label::new(egui::RichText::new(time).monospace().color(weak))
                                .selectable(true),
                        );
                        ui.add(
                            egui::Label::new(egui::RichText::new(rest).monospace().color(*color))
                                .wrap()
                                .selectable(true),
                        );
                    });
                }
            });
        } else {
            // Flatten to physical lines for uniform-height virtualization;
            // continuation lines indent under their header. Selectable for copy.
            let mut rows: Vec<(egui::Color32, String)> = Vec::new();
            for (color, time, rest) in &entries {
                let mut lines = rest.lines();
                rows.push((*color, format!("{time}  {}", lines.next().unwrap_or(""))));
                for cont in lines {
                    rows.push((*color, format!("{:>16}{cont}", "")));
                }
            }
            let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
            scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                ui.style_mut().interaction.selectable_labels = true;
                ui.spacing_mut().item_spacing.y = 0.0;
                for (color, line) in &rows[range] {
                    ui.add(
                        egui::Label::new(egui::RichText::new(line).monospace().color(*color))
                            .selectable(true),
                    );
                }
            });
        }
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
    /// "Baker's Man" — lighting-bake settings + Bake / Clear actions.
    fn baker_ui(&mut self, ui: &mut egui::Ui) {
        use egui::{DragValue, RichText, Slider};
        ui.heading("Baker's Man");
        ui.label(
            RichText::new("Ray-traced lightmaps + light probes for static geometry")
                .small()
                .weak(),
        );
        ui.separator();

        // Scrollable settings; Bake/Clear pinned at the bottom.
        let baked = self.scene.baked.as_ref();
        let summary = baked.map(|b| {
            (
                b.lightmaps.len(),
                b.probe_sh.len(),
                b.lightmaps.iter().map(|l| l.size).max().unwrap_or(0),
            )
        });
        let footer = 84.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() - footer)
            .show(ui, |ui| {
                let bake = &mut self.scene.environment.bake;
                ui.label(RichText::new("Resolution").strong());
                ui.horizontal(|ui| {
                    ui.label("Texel Density")
                        .on_hover_text("Lightmap texels per world meter (Bakery-style)");
                    ui.add(
                        Slider::new(&mut bake.texel_density, 1.0..=1024.0)
                            .logarithmic(true)
                            .suffix(" /m"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Max Lightmap");
                    egui::ComboBox::from_id_salt("citrus-bake-max")
                        .selected_text(format!("{}", bake.max_lightmap))
                        .show_ui(ui, |ui| {
                            for s in [128u32, 256, 512, 1024, 2048] {
                                ui.selectable_value(&mut bake.max_lightmap, s, format!("{s}"));
                            }
                        });
                });

                ui.separator();
                ui.label(RichText::new("Quality").strong());
                ui.horizontal(|ui| {
                    ui.label("Bounces")
                        .on_hover_text("Indirect light bounces (0 = direct + sky only)");
                    ui.add(Slider::new(&mut bake.bounces, 0..=8));
                });
                ui.horizontal(|ui| {
                    ui.label("Samples")
                        .on_hover_text("Paths traced per texel / probe — higher = less noise");
                    ui.add(DragValue::new(&mut bake.samples).speed(8.0).range(1..=65536));
                });

                ui.separator();
                ui.label(RichText::new("Status").strong());
                match summary {
                    Some((lm, probes, max)) => {
                        ui.label(format!("Baked: {lm} lightmap(s) (max {max}px), {probes} probe(s)"));
                        ui.label(
                            RichText::new(
                                ".lightmap = static GI · .lightdata = probe data for dynamic objects",
                            )
                            .small()
                            .weak(),
                        );
                    }
                    None => {
                        ui.label(RichText::new("No bake yet").weak());
                    }
                }
                if !self.can_bake {
                    ui.label(
                        RichText::new("⚠ This GPU has no ray-query support; baking is unavailable")
                            .small()
                            .color(ui.visuals().warn_fg_color),
                    );
                }
                ui.label(
                    RichText::new("Tip: mark objects Static (Inspector) and add Light Probe Volumes for dynamic objects.")
                        .small()
                        .weak(),
                );
            });

        ui.separator();
        ui.horizontal(|ui| {
            let bake_btn = egui::Button::new(RichText::new("🔥 Bake").strong());
            if ui
                .add_enabled(self.can_bake, bake_btn)
                .on_hover_text("Trace lightmaps + probes now (blocks the UI)")
                .clicked()
            {
                self.actions.push(EditorAction::BakeLighting);
            }
            if ui
                .add_enabled(summary.is_some(), egui::Button::new("Clear"))
                .on_hover_text("Discard all baked lighting data")
                .clicked()
            {
                self.actions.push(EditorAction::ClearBake);
            }
        });
        ui.separator();
        ui.checkbox(self.lightmap_preview, "UV checker preview")
            .on_hover_text(
                "Show objects as a lightmap-UV checkerboard — the cell size tracks each \
                 object's texel density (big squares = low resolution, stretched squares = \
                 UV distortion). Grey = not lightmapped (non-static).",
            );
    }

    fn environment_ui(&mut self, ui: &mut egui::Ui) {
        use egui::{DragValue, RichText};
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.heading("Environment");
                ui.separator();

                {
                    let baked = self.scene.baked.is_some();
                    let can_bake = self.can_bake;
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
                    ui.label(RichText::new("Realtime GI").strong());
                    let gi = &mut env.realtime_gi;
                    ui.add_enabled_ui(can_bake && !baked, |ui| {
                        ui.checkbox(&mut gi.enabled, "Enabled").on_hover_text(
                            "Continuously trace light probes from the realtime lights so \
                             surfaces show live indirect bounce (no bake needed). Applies in \
                             the editor and a shipped game.",
                        );
                        ui.add_enabled_ui(gi.enabled, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Mode");
                                egui::ComboBox::from_id_salt("citrus-gi-mode")
                                    .selected_text(gi.mode.label())
                                    .show_ui(ui, |ui| {
                                        for m in citrus_assets::GiMode::ALL {
                                            ui.selectable_value(&mut gi.mode, m, m.label());
                                        }
                                    });
                            });
                            if gi.mode == citrus_assets::GiMode::Software {
                                ui.label(
                                    RichText::new("Software SDF GI (no RT cores): CPU-marched, \
                                        coarse preview — soft + low-frequency.")
                                        .small()
                                        .weak(),
                                );
                            }
                            ui.horizontal(|ui| {
                                ui.label("Bounces");
                                ui.add(egui::Slider::new(&mut gi.bounces, 1..=16))
                                    .on_hover_text("Indirect bounces per path");
                            });
                            ui.horizontal(|ui| {
                                ui.label("Quality");
                                ui.add(
                                    egui::Slider::new(&mut gi.samples, 16..=256)
                                        .suffix(" spp")
                                        .logarithmic(true),
                                )
                                .on_hover_text("Rays per probe — higher = less noise, more cost");
                            });
                            ui.horizontal(|ui| {
                                ui.label("Intensity");
                                ui.add(
                                    DragValue::new(&mut gi.intensity).speed(0.1).range(0.0..=64.0),
                                )
                                .on_hover_text("GI strength multiplier");
                            });
                            ui.horizontal(|ui| {
                                ui.label("Probe Spacing");
                                ui.add(
                                    egui::Slider::new(&mut gi.probe_spacing, 0.5..=8.0)
                                        .suffix(" m"),
                                )
                                .on_hover_text(
                                    "World units between auto-grid probes — smaller = finer GI, \
                                     more cost",
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.label("Responsiveness");
                                ui.add(egui::Slider::new(&mut gi.temporal_blend, 0.05..=1.0))
                                    .on_hover_text(
                                        "Per-update blend toward the new trace — lower = \
                                         smoother/laggier, higher = snappier/noisier",
                                    );
                            });
                            ui.horizontal(|ui| {
                                ui.label("Update Interval");
                                ui.add(
                                    egui::Slider::new(&mut gi.update_interval, 0.05..=1.0)
                                        .suffix(" s"),
                                )
                                .on_hover_text("Seconds between re-traces while the scene changes");
                            });
                        });
                    });
                    if baked {
                        ui.label(
                            RichText::new("Off while a bake is loaded (Clear it to use Realtime GI).")
                                .small()
                                .weak(),
                        );
                    } else if !can_bake {
                        ui.label(
                            RichText::new("This GPU has no ray-query support.")
                                .small()
                                .weak(),
                        );
                    }

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

        // Scroll dollies the camera only while the pointer is over the
        // viewport. Uses the winit-based test (not egui `contains_pointer`)
        // so it re-arms immediately after mouse-look ends, when egui's
        // pointer state is briefly stale.
        if self.pointer_in_viewport {
            let scroll = ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                self.actions.push(EditorAction::Dolly(scroll / 50.0));
            }
        }

        // Left drag orbits (around the selection, or whatever sits at the
        // viewport center) — unless the gizmo grabbed the drag. Alt forces
        // the orbit even when starting on a gizmo handle.
        let alt = ui.input(|i| i.modifiers.alt);
        // `hover_pos()` is None while a drag is in progress, which would make
        // the probe/gizmo handle hit-tests bail the instant a drag starts (so
        // orbit stole the gesture). Fall back to the interaction position,
        // which stays valid through the whole press+drag.
        let cursor = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());
        let mut gizmo_changed = false;

        // Claim a drag for the gizmo the instant it starts on a handle (using
        // the gizmo's own enlarged pick band), so orbit is suppressed for the
        // whole gesture even before/if the gizmo's grab registers. Alt forces
        // orbit. Cleared when the drag ends.
        if response.drag_started_by(egui::PointerButton::Primary) {
            let press = response.interact_pointer_pos();
            // Orbit may only begin when the press is over the viewport (egui
            // can otherwise route a panel drag to this big interact); it then
            // continues through the gesture even if the pointer leaves. The
            // winit-based check keeps it working right after mouse-look ends.
            *self.orbit_armed =
                self.pointer_in_viewport && press.is_some_and(|p| rect.contains(p));
            *self.gizmo_drag = !alt
                && matches!(self.selection, Selection::Object(_))
                && press.is_some_and(|p| self.gizmo.pick_preview(p));
        }
        if response.drag_stopped_by(egui::PointerButton::Primary) {
            *self.gizmo_drag = false;
            *self.orbit_armed = false;
        }

        // Light Probe Volume box-resize: dragging a face handle changes the
        // volume's size along that axis, keeping the opposite face fixed. Run
        // before the transform gizmo so a grabbed handle wins the drag; the
        // gizmo still moves/rotates the object when no handle is grabbed.
        self.probe_resize_interaction(&response, cursor, alt);
        // AudioSource min/max range spheres resize the same way.
        self.audio_range_interaction(&response, cursor, alt);
        // Box/Sphere collider handles too.
        self.collider_interaction(&response, cursor, alt);
        let probe_active = self.probe_drag.is_some()
            || self.audio_drag.is_some()
            || self.collider_drag.is_some();

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
                    .find_map(|c| c.as_any().downcast_ref::<citrus_core::CameraComponent>());
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
                    .find_map(|c| c.as_any().downcast_ref::<citrus_core::LightProbeVolume>())
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
                // Resize handles at each face center: an arrow pointing away
                // from the box. Drag to change the box size along that axis
                // (the opposite face stays put).
                let center_screen = {
                    let clip = view_proj * world.transform_point3(Vec3::ZERO).extend(1.0);
                    (clip.w > W_EPS).then(|| to_screen(clip))
                };
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
                            && cursor.is_some_and(|c| c.distance(p) <= 14.0);
                        // Keep the normal pointer; emphasize hover by growing
                        // the handle slightly and brightening its color.
                        let (arrow_scale, col) = handle_style(
                            active || hovered,
                            egui::Color32::from_rgb(120, 200, 255),
                        );
                        // Point away from the box center (fall back to the
                        // projected face axis if the center is degenerate).
                        let outward = center_screen
                            .map(|c| p - c)
                            .filter(|v| v.length() > 1.0)
                            .or_else(|| {
                                let a = view_proj
                                    * (fc + world.transform_vector3(axes[axis]) * sign).extend(1.0);
                                (a.w > W_EPS).then(|| to_screen(a) - p)
                            })
                            .unwrap_or(egui::vec2(0.0, -1.0));
                        draw_arrow_handle(painter, p, outward, arrow_scale, col);
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
                        .downcast_ref::<citrus_core::LightComponent>()
                        .is_some()
                });
                let is_camera = matches!(object.source, citrus_assets::ObjectSource::Camera);
                let is_probe = object.components.iter().any(|c| {
                    c.as_any()
                        .downcast_ref::<citrus_core::LightProbeVolume>()
                        .is_some()
                });
                let is_audio = object.components.iter().any(|c| {
                    c.as_any()
                        .downcast_ref::<citrus_core::AudioSource>()
                        .is_some()
                });
                if !is_light && !is_camera && !is_probe && !is_audio {
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
                // Icon priority: light, probe volume, audio, then camera.
                let setting = if is_light {
                    self.widget_filter.lights
                } else if is_probe {
                    self.widget_filter.probes
                } else if is_audio {
                    self.widget_filter.audio
                } else {
                    self.widget_filter.cameras
                };
                // Filtered-off billboards still show for the selected object.
                if !setting.visible && !selected {
                    continue;
                }
                if is_light {
                    draw_light_icon(painter, screen, selected, setting.size);
                } else if is_probe {
                    draw_probe_icon(painter, screen, selected, setting.size);
                } else if is_audio {
                    draw_audio_icon(painter, screen, selected, setting.size);
                } else {
                    draw_camera_icon(painter, screen, selected, setting.size);
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
                .find_map(|c| c.as_any().downcast_ref::<citrus_core::LightComponent>())
        {
            use citrus_core::LightKind;
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

        // AudioSource range: min/max distance wire spheres around a selected
        // spatial source, each with a draggable radius handle (camera-right).
        if let Selection::Object(i) = *self.selection
            && self.scene.is_active(i)
            && let Some((min_d, max_d)) = self.scene.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<citrus_core::AudioSource>())
                .filter(|s| s.spatial)
                .map(|s| (s.min_distance, s.max_distance))
        {
            let view_proj = self.proj * self.view;
            let full_rect = ui.ctx().viewport_rect();
            let origin = self.scene.world_transform(i).w_axis.truncate();
            let right = self.camera_right();
            let painter = ui.painter();
            let to_screen = |clip: glam::Vec4| -> egui::Pos2 {
                let ndc = clip.truncate() / clip.w;
                egui::pos2(
                    full_rect.left() + (ndc.x * 0.5 + 0.5) * full_rect.width(),
                    full_rect.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * full_rect.height(),
                )
            };
            let segline = |a: Vec3, b: Vec3, stroke: egui::Stroke| {
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
                if let Some(seg) = clip_segment_to_rect(to_screen(ca), to_screen(cb), full_rect) {
                    painter.line_segment(seg, stroke);
                }
            };
            let circle = |radius: f32, a: Vec3, b: Vec3, stroke: egui::Stroke| {
                const SEG: usize = 32;
                let mut prev = origin + a * radius;
                for k in 1..=SEG {
                    let ang = k as f32 / SEG as f32 * std::f32::consts::TAU;
                    let p = origin + (a * ang.cos() + b * ang.sin()) * radius;
                    segline(prev, p, stroke);
                    prev = p;
                }
            };
            for (is_max, radius, base) in [
                (false, min_d, egui::Color32::from_rgb(150, 220, 255)),
                (true, max_d, egui::Color32::from_rgb(95, 150, 200)),
            ] {
                if radius <= 0.0 {
                    continue;
                }
                let stroke = egui::Stroke::new(1.4, base);
                circle(radius, Vec3::X, Vec3::Y, stroke);
                circle(radius, Vec3::X, Vec3::Z, stroke);
                circle(radius, Vec3::Y, Vec3::Z, stroke);
                // Radius handle on the viewer-facing side.
                let handle = origin + right * radius;
                if let Some(hs) = self.world_to_screen(handle, full_rect) {
                    let active = self
                        .audio_drag
                        .as_ref()
                        .is_some_and(|d| d.object == i && d.max == is_max);
                    let hovered = self.audio_drag.is_none()
                        && cursor.is_some_and(|c| c.distance(hs) <= 14.0);
                    let (sc, col) = handle_style(active || hovered, base);
                    let outward = self
                        .world_to_screen(origin + right * (radius + 1.0), full_rect)
                        .map(|p| p - hs)
                        .unwrap_or(egui::vec2(1.0, 0.0));
                    draw_arrow_handle(painter, hs, outward, sc, col);
                }
            }
        }

        // Colliders (yellow): box wireframe + face resize arrows, sphere wire
        // circles + radius arrow, mesh-collider AABB. Drawn for the selection.
        if let Selection::Object(i) = *self.selection
            && self.scene.is_active(i)
        {
            const YELLOW: egui::Color32 = egui::Color32::from_rgb(240, 220, 70);
            let world = self.scene.world_transform(i);
            let view_proj = self.proj * self.view;
            let full_rect = ui.ctx().viewport_rect();
            let right = self.camera_right();
            let painter = ui.painter();
            let to_screen = |clip: glam::Vec4| -> egui::Pos2 {
                let ndc = clip.truncate() / clip.w;
                egui::pos2(
                    full_rect.left() + (ndc.x * 0.5 + 0.5) * full_rect.width(),
                    full_rect.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * full_rect.height(),
                )
            };
            let line = |a: Vec3, b: Vec3, stroke: egui::Stroke| {
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
                if let Some(seg) = clip_segment_to_rect(to_screen(ca), to_screen(cb), full_rect) {
                    painter.line_segment(seg, stroke);
                }
            };
            let stroke = egui::Stroke::new(1.4, YELLOW);
            let obj = &self.scene.objects[i];

            // Box collider: wireframe + face arrow handles.
            if let Some((center, size)) = obj
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<citrus_core::BoxCollider>())
                .map(|b| (Vec3::from(b.center), Vec3::from(b.size)))
            {
                let half = size * 0.5;
                let corner = |sx: f32, sy: f32, sz: f32| {
                    world.transform_point3(center + Vec3::new(half.x * sx, half.y * sy, half.z * sz))
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
                    line(c[k], c[(k + 1) % 4], stroke);
                    line(c[k + 4], c[(k + 1) % 4 + 4], stroke);
                    line(c[k], c[k + 4], stroke);
                }
                let center_screen = self.world_to_screen(world.transform_point3(center), full_rect);
                let axes = [Vec3::X, Vec3::Y, Vec3::Z];
                for axis in 0..3 {
                    for sign in [-1.0f32, 1.0] {
                        let fw = world.transform_point3(center + axes[axis] * (sign * half[axis]));
                        let Some(p) = self.world_to_screen(fw, full_rect) else {
                            continue;
                        };
                        let active = matches!(
                            self.collider_drag.as_ref(),
                            Some(d) if d.object == i
                                && matches!(d.handle, ColliderHandle::BoxFace(a, s) if a == axis && s == sign)
                        );
                        let hovered = self.collider_drag.is_none()
                            && cursor.is_some_and(|cc| cc.distance(p) <= 14.0);
                        let (sc, col) = handle_style(active || hovered, YELLOW);
                        let outward = center_screen
                            .map(|cs| p - cs)
                            .filter(|v| v.length() > 1.0)
                            .unwrap_or(egui::vec2(0.0, -1.0));
                        draw_arrow_handle(painter, p, outward, sc, col);
                    }
                }
            }

            // Sphere collider: 3 wire circles + radius arrow handle.
            if let Some((center, radius)) = obj
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<citrus_core::SphereCollider>())
                .map(|s| (Vec3::from(s.center), s.radius))
            {
                let o = world.transform_point3(center);
                let circle = |a: Vec3, b: Vec3| {
                    const SEG: usize = 32;
                    let mut prev = o + a * radius;
                    for k in 1..=SEG {
                        let ang = k as f32 / SEG as f32 * std::f32::consts::TAU;
                        let p = o + (a * ang.cos() + b * ang.sin()) * radius;
                        line(prev, p, stroke);
                        prev = p;
                    }
                };
                circle(Vec3::X, Vec3::Y);
                circle(Vec3::X, Vec3::Z);
                circle(Vec3::Y, Vec3::Z);
                let handle = o + right * radius;
                if let Some(hs) = self.world_to_screen(handle, full_rect) {
                    let active = matches!(
                        self.collider_drag.as_ref(),
                        Some(d) if d.object == i && matches!(d.handle, ColliderHandle::SphereRadius)
                    );
                    let hovered = self.collider_drag.is_none()
                        && cursor.is_some_and(|cc| cc.distance(hs) <= 14.0);
                    let (sc, col) = handle_style(active || hovered, YELLOW);
                    let outward = self
                        .world_to_screen(handle + right, full_rect)
                        .map(|p| p - hs)
                        .unwrap_or(egui::vec2(1.0, 0.0));
                    draw_arrow_handle(painter, hs, outward, sc, col);
                }
            }

            // Mesh collider: the mesh's AABB (follows the mesh, not editable).
            if obj
                .components
                .iter()
                .any(|c| c.as_any().downcast_ref::<citrus_core::MeshCollider>().is_some())
                && let Some((min, max)) = self.scene.render_mesh_bounds(i)
            {
                let corner = |sx: f32, sy: f32, sz: f32| {
                    world.transform_point3(Vec3::new(
                        if sx < 0.0 { min.x } else { max.x },
                        if sy < 0.0 { min.y } else { max.y },
                        if sz < 0.0 { min.z } else { max.z },
                    ))
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
                    line(c[k], c[(k + 1) % 4], stroke);
                    line(c[k + 4], c[(k + 1) % 4 + 4], stroke);
                    line(c[k], c[k + 4], stroke);
                }
            }
        }

        // Orientation cross: three short gray lines along the selected
        // object's LOCAL axes, crossing at the pivot, so its orientation is
        // readable under the move/scale gizmo. Editor-camera overlay only.
        if let Selection::Object(i) = *self.selection
            && matches!(self.gizmo.tool, GizmoTool::Move | GizmoTool::Scale)
            && self.scene.is_active(i)
        {
            let world = self.scene.world_transform(i);
            let origin = world.w_axis.truncate();
            // Follow the gizmo's orientation mode: object rotation in Local,
            // world axes in Global.
            let rot = if self.gizmo.local_orientation {
                world.to_scale_rotation_translation().1
            } else {
                glam::Quat::IDENTITY
            };
            let cam = self.view.inverse().w_axis.truncate();
            // Screen-constant length (~world units that project to a small,
            // steady on-screen size).
            let len = ((origin - cam).length() * 0.12).clamp(0.05, 50.0);
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
            let stroke = egui::Stroke::new(1.0, egui::Color32::from_gray(150));
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
                if let Some(seg) = clip_segment_to_rect(to_screen(ca), to_screen(cb), full_rect) {
                    painter.line_segment(seg, stroke);
                }
            };
            for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
                let d = rot * (axis * len);
                line(origin - d, origin + d);
            }
        }

        // Plain left drag (not claimed by the gizmo or a probe handle): orbit
        // the camera around a pivot locked at drag start.
        if response.dragged_by(egui::PointerButton::Primary)
            && *self.orbit_armed
            && !self.gizmo.is_focused()
            && !*self.gizmo_drag
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
                            (GizmoTool::Move, "⬌", "Move (W)"),
                            (GizmoTool::Rotate, "🔄", "Rotate (R)"),
                            (GizmoTool::Scale, "⛶", "Scale (E)"),
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

        // Widget filter (top-right): per-billboard visibility + size. The
        // move/rotate/scale gizmos are deliberately absent — they can't be
        // hidden. Selected objects always show their billboard regardless.
        egui::Area::new(ui.id().with("vp-widgets"))
            .order(egui::Order::Middle)
            .pivot(egui::Align2::RIGHT_TOP)
            .fixed_pos(rect.right_top() + egui::vec2(-8.0, 8.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    // Stay open across checkbox/slider clicks so several kinds
                    // can be toggled at once (like the View menu).
                    egui::containers::menu::MenuButton::new("👁 Gizmos")
                        .config(
                            egui::containers::menu::MenuConfig::new()
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                        )
                        .ui(ui, |ui| {
                            ui.label(egui::RichText::new("Billboard widgets").small().weak());
                            let filter = &mut *self.widget_filter;
                            for (label, setting) in [
                                ("Lights", &mut filter.lights),
                                ("Cameras", &mut filter.cameras),
                                ("Probe Volumes", &mut filter.probes),
                                ("Audio Sources", &mut filter.audio),
                            ] {
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut setting.visible, label);
                                    ui.add_enabled(
                                        setting.visible,
                                        egui::Slider::new(&mut setting.size, 0.3..=4.0)
                                            .show_value(false),
                                    )
                                    .on_hover_text("Widget size");
                                });
                            }
                            ui.separator();
                            ui.label(
                                egui::RichText::new("Move/Rotate/Scale always shown · selected objects ignore this")
                                    .small()
                                    .weak(),
                            );
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
        // Keep the inspector laid out at a usable width so its rows don't
        // collapse if the dock is dragged narrow (egui_dock has no per-leaf
        // minimum, so we enforce it from the content side).
        ui.set_min_width(330.0);
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
                ui.label(egui::RichText::new("✓ No problems").size(14.0).weak());
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
                                .size(14.0)
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
                        lightmap_scale: object.lightmap_scale,
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
                    // Object list (id + name) for ObjectRef pickers — built as an
                    // owned Vec before the mutable component borrow so it doesn't
                    // alias scene.objects.
                    let objects: Vec<(ObjectId, String)> = self
                        .scene
                        .objects
                        .iter()
                        .map(|o| (o.id, o.name.clone()))
                        .collect();
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
                            editor_components: self.editor_components,
                            objects: &objects,
                        },
                        &shader_refs,
                    );
                    if response.object_changed {
                        let object = &mut self.scene.objects[index];
                        object.name = info.name;
                        object.enabled = info.enabled;
                        object.static_geometry = info.static_geometry;
                        object.lightmap_scale = info.lightmap_scale;
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
                        Some("postfx") => {
                            ui.heading("Post FX Profile");
                            ui.label(egui::RichText::new(&path_display).small().weak());
                            ui.separator();
                            let mut profile = citrus_assets::load_postfx(path).unwrap_or_default();
                            if postfx_editor_ui(ui, &mut profile) {
                                if let Err(e) = citrus_assets::save_postfx(path, &profile) {
                                    tracing::error!("saving postfx: {e:#}");
                                }
                                // Volumes pick up the edit live next frame.
                                self.scene.invalidate_postfx_cache();
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

/// Sliders for a `.postfx` profile (the post-processing asset editor). Returns
/// true when any value changed (caller saves + invalidates the cache).
fn postfx_editor_ui(ui: &mut egui::Ui, p: &mut citrus_assets::PostFxProfile) -> bool {
    use citrus_assets::TonemapMode;
    use egui::{DragValue, RichText, Slider};
    let mut changed = false;

    ui.label(RichText::new("Tonemap").strong());
    ui.horizontal(|ui| {
        ui.label("Mode");
        egui::ComboBox::from_id_salt("postfx-tonemap")
            .selected_text(p.tonemap.mode.label())
            .show_ui(ui, |ui| {
                for m in TonemapMode::ALL {
                    changed |= ui.selectable_value(&mut p.tonemap.mode, m, m.label()).changed();
                }
            });
    });
    ui.horizontal(|ui| {
        ui.label("Exposure (EV)");
        changed |= ui.add(Slider::new(&mut p.tonemap.exposure, -8.0..=8.0)).changed();
    });

    ui.separator();
    changed |= ui.checkbox(&mut p.color_grading.enabled, "Color Grading").changed();
    ui.add_enabled_ui(p.color_grading.enabled, |ui| {
        let g = &mut p.color_grading;
        ui.horizontal(|ui| {
            ui.label("Post Exposure");
            changed |= ui.add(Slider::new(&mut g.exposure, -4.0..=4.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Contrast");
            changed |= ui.add(Slider::new(&mut g.contrast, 0.0..=2.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Saturation");
            changed |= ui.add(Slider::new(&mut g.saturation, 0.0..=2.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Temperature");
            changed |= ui.add(Slider::new(&mut g.temperature, -1.0..=1.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Tint");
            changed |= ui.add(Slider::new(&mut g.tint, -1.0..=1.0)).changed();
        });
    });

    ui.separator();
    changed |= ui.checkbox(&mut p.vignette.enabled, "Vignette").changed();
    ui.add_enabled_ui(p.vignette.enabled, |ui| {
        let v = &mut p.vignette;
        ui.horizontal(|ui| {
            ui.label("Intensity");
            changed |= ui.add(Slider::new(&mut v.intensity, 0.0..=1.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Smoothness");
            changed |= ui.add(Slider::new(&mut v.smoothness, 0.0..=1.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Color");
            changed |= ui.color_edit_button_rgb(&mut v.color).changed();
        });
    });

    ui.separator();
    changed |= ui.checkbox(&mut p.bloom.enabled, "Bloom (needs HDR pass)").changed();
    ui.add_enabled_ui(p.bloom.enabled, |ui| {
        let b = &mut p.bloom;
        ui.horizontal(|ui| {
            ui.label("Threshold");
            changed |= ui.add(DragValue::new(&mut b.threshold).speed(0.02).range(0.0..=10.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Intensity");
            changed |= ui.add(Slider::new(&mut b.intensity, 0.0..=3.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Radius");
            changed |= ui.add(Slider::new(&mut b.radius, 0.0..=1.0)).changed();
        });
    });

    ui.separator();
    changed |= ui
        .checkbox(&mut p.chromatic_aberration.enabled, "Chromatic Aberration (needs HDR pass)")
        .changed();
    ui.add_enabled_ui(p.chromatic_aberration.enabled, |ui| {
        ui.horizontal(|ui| {
            ui.label("Intensity");
            changed |= ui
                .add(Slider::new(&mut p.chromatic_aberration.intensity, 0.0..=2.0))
                .changed();
        });
    });

    changed
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

/// Draw one hand-drawn light bulb: glass disc at `glass`, screw base on the
/// far side of `up` (the direction the bulb points). egui's font has no 💡
/// glyph, so lights and probe volumes share this primitive.
fn draw_bulb(painter: &egui::Painter, glass: egui::Pos2, up: egui::Vec2, scale: f32, selected: bool) {
    let glass_col = if selected {
        egui::Color32::from_rgb(255, 242, 150)
    } else {
        egui::Color32::from_rgb(235, 215, 120)
    };
    let dark = egui::Color32::from_rgb(70, 62, 35);
    let up = up.normalized();
    let right = egui::vec2(-up.y, up.x);
    let r = 7.0 * scale;
    if selected {
        // Emitted-light rays.
        for k in 0..8 {
            let a = k as f32 / 8.0 * std::f32::consts::TAU;
            let d = egui::vec2(a.cos(), a.sin());
            painter.line_segment(
                [glass + d * (r + 2.0 * scale), glass + d * (r + 5.0 * scale)],
                egui::Stroke::new(1.5, glass_col),
            );
        }
    }
    painter.circle_filled(glass, r, glass_col);
    painter.circle_stroke(glass, r, egui::Stroke::new(1.0, dark));
    // Screw base: a small block on the far side of `up`, with a thread line.
    let bc = glass - up * (r + 1.5 * scale);
    let hw = r * 0.55;
    let hh = 2.5 * scale;
    let base = vec![
        bc + right * hw + up * hh,
        bc - right * hw + up * hh,
        bc - right * hw - up * hh,
        bc + right * hw - up * hh,
    ];
    painter.add(egui::Shape::convex_polygon(base, dark, egui::Stroke::NONE));
    painter.line_segment(
        [bc - right * (hw - 1.0), bc + right * (hw - 1.0)],
        egui::Stroke::new(1.0, glass_col),
    );
}

/// Hover/active emphasis for a draggable handle (probe volumes, and the
/// box/sphere colliders that reuse this): slightly bigger + a bit brighter
/// when emphasized, the base color otherwise. The pointer stays normal.
fn handle_style(emphasized: bool, base: egui::Color32) -> (f32, egui::Color32) {
    if emphasized {
        // Lighten toward white without going fully white.
        let lift = |c: u8| c.saturating_add((255 - c) / 2);
        (
            1.25,
            egui::Color32::from_rgb(lift(base.r()), lift(base.g()), lift(base.b())),
        )
    } else {
        (1.0, base)
    }
}

/// Draw a resize-handle arrow at `at` pointing along screen-space `dir`
/// (away from the box). A short shaft + filled triangular head.
fn draw_arrow_handle(
    painter: &egui::Painter,
    at: egui::Pos2,
    dir: egui::Vec2,
    scale: f32,
    color: egui::Color32,
) {
    let dir = if dir.length() > 1e-3 {
        dir.normalized()
    } else {
        egui::vec2(0.0, -1.0)
    };
    let perp = egui::vec2(-dir.y, dir.x);
    let head = 9.0 * scale;
    let half_w = 5.5 * scale;
    let shaft = 6.0 * scale;
    // Shaft from just outside the face toward the tip.
    let base = at + dir * shaft;
    painter.line_segment([at, base], egui::Stroke::new(2.5 * scale, color));
    // Arrowhead triangle.
    let tip = base + dir * head;
    let l = base + perp * half_w;
    let r = base - perp * half_w;
    painter.add(egui::Shape::convex_polygon(
        vec![tip, l, r],
        color,
        egui::Stroke::NONE,
    ));
}

/// Draw a light-bulb billboard (glass + screw base) centered at `center`.
fn draw_light_icon(painter: &egui::Painter, center: egui::Pos2, selected: bool, scale: f32) {
    draw_bulb(painter, center - egui::vec2(0.0, 2.0 * scale), egui::vec2(0.0, -1.0), scale, selected);
}

/// Draw the light-probe-volume emblem: three bulbs (the same image as the
/// light widget) fanned from a shared base at -42° / 0° / +42°, like Unity's
/// Light Probe Group icon.
fn draw_probe_icon(painter: &egui::Painter, center: egui::Pos2, selected: bool, scale: f32) {
    let base = center + egui::vec2(0.0, 8.0 * scale);
    let bulb_scale = scale * 0.62;
    let stem = 11.0 * scale;
    for deg in [-42.0_f32, 0.0, 42.0] {
        let a = deg.to_radians();
        // 0° points straight up; +deg tilts toward +X (screen-right).
        let dir = egui::vec2(a.sin(), -a.cos());
        draw_bulb(painter, base + dir * stem, dir, bulb_scale, selected);
    }
}

/// Draw a speaker icon (cabinet + cone + sound waves) centered at `center`.
fn draw_audio_icon(painter: &egui::Painter, center: egui::Pos2, selected: bool, scale: f32) {
    let body = if selected {
        egui::Color32::from_rgb(235, 225, 255)
    } else {
        egui::Color32::from_rgb(200, 190, 220)
    };
    let dark = egui::Color32::from_rgb(55, 50, 70);
    // Square cabinet on the left, trapezoid cone opening to the right.
    let cab = egui::Rect::from_center_size(
        center + egui::vec2(-5.0 * scale, 0.0),
        egui::vec2(5.0 * scale, 7.0 * scale),
    );
    painter.rect_filled(cab, 1.0 * scale, body);
    let cone = vec![
        egui::pos2(cab.right(), center.y - 3.0 * scale),
        egui::pos2(center.x + 3.0 * scale, center.y - 7.0 * scale),
        egui::pos2(center.x + 3.0 * scale, center.y + 7.0 * scale),
        egui::pos2(cab.right(), center.y + 3.0 * scale),
    ];
    painter.add(egui::Shape::convex_polygon(
        cone,
        body,
        egui::Stroke::new(1.0, dark),
    ));
    painter.rect_stroke(cab, 1.0 * scale, egui::Stroke::new(1.0, dark), egui::StrokeKind::Inside);
    // Sound waves: two arcs to the right (brighter when selected).
    let wave = if selected {
        egui::Color32::from_rgb(150, 220, 255)
    } else {
        egui::Color32::from_rgb(120, 170, 200)
    };
    for (k, r) in [(0u8, 4.0_f32), (1, 7.0)] {
        let cx = center.x + 5.0 * scale;
        let steps = 7;
        let mut prev: Option<egui::Pos2> = None;
        for s in 0..=steps {
            // -50°..50° arc opening right.
            let a = (-0.9 + 1.8 * s as f32 / steps as f32) * std::f32::consts::FRAC_PI_2;
            let p = egui::pos2(cx + a.cos() * r * scale, center.y + a.sin() * r * scale);
            if let Some(pp) = prev {
                painter.line_segment([pp, p], egui::Stroke::new(1.0 + k as f32 * 0.0, wave));
            }
            prev = Some(p);
        }
    }
}

/// Draw a little video-camera icon (body + lens) centered at `center`.
fn draw_camera_icon(painter: &egui::Painter, center: egui::Pos2, selected: bool, scale: f32) {
    let body_col = if selected {
        egui::Color32::from_rgb(225, 238, 255)
    } else {
        egui::Color32::from_rgb(190, 210, 235)
    };
    let dark = egui::Color32::from_rgb(40, 55, 75);
    let body = egui::Rect::from_min_size(
        center + egui::vec2(-9.0 * scale, -5.0 * scale),
        egui::vec2(12.0 * scale, 10.0 * scale),
    );
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
        egui::pos2(lx, cy - 2.5 * scale),
        egui::pos2(lx + 6.0 * scale, cy - 4.5 * scale),
        egui::pos2(lx + 6.0 * scale, cy + 4.5 * scale),
        egui::pos2(lx, cy + 2.5 * scale),
    ];
    painter.add(egui::Shape::convex_polygon(
        lens,
        body_col,
        egui::Stroke::new(1.0, dark),
    ));
    // Lens dot on the body front.
    painter.circle_filled(egui::pos2(body.left() + 4.0 * scale, cy), 1.6 * scale, dark);
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
        LspRequestKind::References { path } => {
            let refs = parse_references(&result);
            if let Some(editor) = editors.iter_mut().find(|e| e.path == path) {
                editor.references = if refs.is_empty() { None } else { Some(refs) };
            }
        }
    }
}

/// Parse an LSP `textDocument/references` result (array of `Location`) into a
/// pickable list.
fn parse_references(result: &serde_json::Value) -> Vec<citrus_editor::ReferenceItem> {
    let Some(arr) = result.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|loc| {
            let uri = loc.get("uri").and_then(|u| u.as_str())?;
            let path = uri.strip_prefix("file://").map(PathBuf::from)?;
            let range = loc.get("range")?;
            let line = range.pointer("/start/line").and_then(|l| l.as_u64())? as u32;
            let col = range
                .pointer("/start/character")
                .and_then(|c| c.as_u64())
                .unwrap_or(0) as u32;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            Some(citrus_editor::ReferenceItem {
                label: format!("{name}:{}", line + 1),
                path,
                line,
                col,
            })
        })
        .collect()
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

/// A unique sibling path for duplicating `src`: `<stem>_copy.<ext>` (numbered
/// if taken). Works for files and directories.
fn duplicate_file_path(src: &Path) -> Option<PathBuf> {
    let dir = src.parent()?;
    let stem = src.file_stem()?.to_string_lossy();
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(unique_path(dir, &format!("{stem}_copy"), &ext))
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
                if self.scene_dirty {
                    // Hold the close; the Save/Discard/Cancel dialog decides.
                    self.show_quit_dialog = true;
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                } else {
                    self.save_project();
                    event_loop.exit();
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } if !egui_wants
                && !self.egui_ctx.wants_keyboard_input()
                && !self.code_tab_focused() =>
            {
                // ...but not while a code editor / text field has focus (there
                // Escape is the vim Insert -> Normal key, not a deselect).
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
                            // Unity-style: W move, E scale, R rotate (these
                            // only fire when not mouse-looking, so they don't
                            // clash with WASDQE fly).
                            KeyCode::KeyW if !ctrl => self.gizmo.tool = GizmoTool::Move,
                            KeyCode::KeyE if !ctrl => self.gizmo.tool = GizmoTool::Scale,
                            KeyCode::KeyR if !ctrl => self.gizmo.tool = GizmoTool::Rotate,
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
                            KeyCode::KeyD if ctrl => {
                                // Duplicate the selected object (scene) or file.
                                match &self.selection {
                                    Selection::Object(i) => self
                                        .actions
                                        .push(EditorAction::DuplicateObject(*i)),
                                    Selection::File(p) => self
                                        .actions
                                        .push(EditorAction::DuplicateFile(p.clone())),
                                    Selection::None => {}
                                }
                            }
                            KeyCode::Delete => {
                                // Acts on the current selection: an object in the
                                // scene, or a file/folder in the browser (same as
                                // their right-click Delete).
                                match &self.selection {
                                    Selection::Object(i) => self
                                        .actions
                                        .push(EditorAction::DeleteObject(*i)),
                                    Selection::File(p) => self
                                        .actions
                                        .push(EditorAction::DeleteFile(p.clone())),
                                    Selection::None => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ElementState::Pressed => {
                    // egui consumed the press (e.g. a focused text field), but
                    // still track modifier keys so `self.keys` doesn't desync —
                    // otherwise a Ctrl press swallowed here leaves `ctrl` false
                    // for a later viewport shortcut (Ctrl+Z undo, etc.).
                    if matches!(
                        code,
                        KeyCode::ControlLeft
                            | KeyCode::ControlRight
                            | KeyCode::ShiftLeft
                            | KeyCode::ShiftRight
                            | KeyCode::AltLeft
                            | KeyCode::AltRight
                    ) {
                        self.keys.insert(code);
                    }
                }
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
