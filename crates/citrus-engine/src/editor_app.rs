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
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::camera::FlyCamera;
use citrus_core::{ComponentCommand, ComponentRegistry, ObjectId};
use citrus_editor::{
    CodeDiagnostic, CodeEditor, EditorComponents, FileBrowser, InspectorContent, InspectorPanel,
    MaterialModel, ObjectInfoModel, ScenePanel, ShaderUiInfo, TransformModel,
};
use citrus_render::{CameraData, FrameInput, LightData, Renderer};
use crate::gizmo::{GizmoState, GizmoTool};
use crate::scene::{LoadedScene, RenderInfo, material_from_model, model_from_material, relative_to};
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
        xr: None,
        xr_session: None,
        vr_targets: citrus_core::TrackerTargets::default(),
        vr_rig: citrus_core::VrRig::default(),
        xr_stereo_ready: false,
        vr_grab_anchor: None,
        vr_drag_hand: None,
        vr_panel_anchor: None,
        vr_prev_hand_dist: None,
        vr_prev_trigger: false,
        vr_move_dist: 0.0,
        vr_menu_open: false,
        vr_prev_menu: false,
        vr_ui_pointer: None,
        vr_ui_pressed: false,
        vr_ui_prev_pressed: false,
        vr_overlay_draw: None,
        scene: LoadedScene::empty(),
        egui_ctx: egui::Context::default(),
        egui_state: None,
        tasks: crate::tasks::TaskManager::default(),
        show_task_popup: false,
        bake_job: None,
        bake_pending_warmup: false,
        deferred_model_applies: Vec::new(),
        splash: None,
        pending_init: false,
        splash_painted: false,
        renderer_rx: None,
        pending_window: None,
        load_status: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
        profiler_window: None,
        profiler_egui_ctx: egui::Context::default(),
        profiler_egui_state: None,
        prof_history: ProfHistory::default(),
        last_render_stats: citrus_render::RenderStats::default(),
        dock_state: default_layout(),
        inspector: InspectorPanel::new(),
        inspector_lock_target: None,
        scene_panel: ScenePanel::new(),
        file_browser: FileBrowser::new(project_root.clone()),
        selection: Selection::None,
        multi_objects: Vec::new(),
        multi_files: Vec::new(),
        refl_bake_pending: None,
        file_material: None,
        file_meta: None,
        open_editors: vec![],
        lsp: None,
        lsp_failed: false,
        lsp_requests: HashMap::new(),
        file_diagnostics: HashMap::new(),
        gizmo: GizmoState::new(),
        widget_filter: WidgetFilter::default(),
        gizmo_drag: false,
        orbit_armed: false,
        camera_tab_visible: false,
        viewport_visible: true,
        audio: audio::AudioEngine::new(),
        actions: Vec::new(),
        undo_stack: UndoStack::default(),
        suppress_undo_record: false,
        components: ComponentRegistry::with_builtins(),
        editor_components: EditorComponents::with_builtins(),
        playing: false,
        play_paused: false,
        play_time: 0.0,
        play_scene_switched: false,
        play_origin_scene: None,
        play_snapshot: None,
        physics: None,
        shaders: ShaderLibrary::default(),
        shader_files: Vec::new(),
        last_shader_scan: None,
        engine_shader_mtime: None,
        last_asset_check: None,
        dirty_materials: HashSet::new(),
        last_material_edit: None,
        plugins: plugins::PluginHost::default(),
        plugin_build_error: None,
        status: None,
        reload_pending: false,
        pending_job: None,
        project: citrus_assets::ProjectFile::default(),
        camera: FlyCamera::default(),
        orbit_pivot: None,
        looking: false,
        look_just_ended: false,
        panning: false,
        look_delta: (0.0, 0.0),
        keys: HashSet::new(),
        input: crate::input_engine::InputManager::default(),
        net: None,
        voice: None,
        net_addr: "127.0.0.1:9000".to_string(),
        show_bindings: false,
        show_network: false,
        show_layers: false,
        show_mixer: false,
        mixer: crate::audio_mixer::AudioMixer::new(),
        rebinding: None,
        last_cursor: None,
        viewport_rect: egui::Rect::EVERYTHING,
        project_root,
        current_scene_path: None,
        pending_lm_uv_models: None,
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
        handle_drag: None,
        stats: Stats::default(),
        world: hecs::World::new(),
        start: Instant::now(),
        last_frame: Instant::now(),
        rt_gi: crate::realtime_gi::RealtimeGiState::default(),
        selected_material_slot: 0,
        frame_timings: FrameTimings::default(),
        gi_debug: 0,
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

/// One action on the VR left-hand menu. The desktop editor keeps its full egui
/// UI; this is the VR-only quick menu pointed at with the right controller.
#[derive(Clone, Copy)]
enum VrAction {
    CalibrateTpose,
    ClearCalibration,
    ToggleFootIk,
    ResetRig,
    DeleteSelected,
}

/// VR tool actions, surfaced as a "VR Tools" egui window so they're reachable
/// from the in-VR UI panel (and the desktop). The full editor UI is otherwise
/// shown as-is on the panel.
const VR_TOOLS: &[(&str, VrAction)] = &[
    ("Calibrate T-pose", VrAction::CalibrateTpose),
    ("Clear Calibration", VrAction::ClearCalibration),
    ("Toggle Foot IK", VrAction::ToggleFootIk),
    ("Reset View/Scale", VrAction::ResetRig),
    ("Delete Selected", VrAction::DeleteSelected),
];

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

/// A model file's `.meta` open in the Inspector for editing import options.
struct FileMeta {
    path: PathBuf,
    meta: citrus_assets::AssetMeta,
    dirty: bool,
}

/// A heavy, UI-blocking operation deferred one frame so a "busy" overlay paints
/// before it runs (otherwise the window looks frozen).
enum PendingJob {
    ReloadPlugins,
}

/// Which pass of the chunked bake is running.
#[derive(PartialEq)]
enum BakeRunPhase {
    /// Main lightmaps + probes.
    Main,
    /// FluxVoxel voxel volumes (appended to the probe set).
    FluxVoxel,
}

/// Drives a chunked lighting bake across frames (one unit per frame). Holds the
/// GPU `BakeJob` plus the metadata needed to assemble `BakedData` when it
/// completes, and carries the partial result from the main pass into the FluxVoxel
/// pass. See the bake phase of the background-task plan.
struct BakeRunner {
    job: citrus_render::BakeJob,
    task_id: crate::tasks::TaskId,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    progress: std::sync::Arc<std::sync::Mutex<crate::tasks::TaskProgress>>,
    phase: BakeRunPhase,
    settings: citrus_assets::BakeSettings,
    /// Main pass: object→lightmap-layer map and probe volumes for assembly.
    object_lightmap: std::collections::HashMap<usize, usize>,
    probe_volumes: Vec<scene::ProbeVolumeMeta>,
    /// Partial bake carried from the main pass into the FluxVoxel pass.
    baked: Option<scene::BakedData>,
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
    /// Re-render the active Reflection Probe's cubemap from the current scene
    /// (realtime probe refresh — surroundings changed since the last capture).
    RecaptureReflections,
    /// Recapture then read the cube back and persist it next to the scene as a
    /// `.reflprobe` sidecar (baked reflection probe — loaded on scene open).
    BakeReflections,
    ImportModel(PathBuf),
    CreateMaterial(PathBuf),
    CreateScene(PathBuf),
    CreateShader(PathBuf),
    CreatePostFx(PathBuf),
    CreateFolder(PathBuf),
    PickAt(egui::Pos2),
    AssignMaterialAt(egui::Pos2, PathBuf),
    /// (object index, material slot, `.material` path)
    AssignMaterialToObject(usize, usize, PathBuf),
    MaterialEdited(usize),
    ResetMaterial(usize),
    /// An image was dropped on a scene material's texture slot: (material index,
    /// slot 0..12, absolute image path). Converted to project-relative + applied.
    AssignTexture {
        material: usize,
        slot: usize,
        path: PathBuf,
    },
    /// An image was dropped on a texture slot of the open `.material` file
    /// editor: (slot 0..12, absolute image path).
    AssignFileTexture {
        slot: usize,
        path: PathBuf,
    },
    /// Extract an embedded (imported) material to a `.material` file so it can be
    /// edited + saved.
    ExtractMaterial(usize),
    SaveFileMaterial,
    /// Write the open model `.meta` import settings to disk.
    SaveFileMeta,
    /// Save the model's `.meta` and reload the scene so it reimports with the new
    /// settings.
    ReimportModel(PathBuf),
    /// Re-load a model file and write its embedded textures (PNG) + materials
    /// (`.material`) into `<project>/extracted/<model>/` as reusable assets.
    ExtractModelAssets(PathBuf),
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
    SpawnPostFxVolume,
    SpawnReflectionProbe,
    SpawnFluxVolume,
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
    /// Assign one face of the 6-image cubemap skybox (index 0..6 = +X,-X,+Y,-Y,+Z,-Z).
    SetSkyboxFace(usize, PathBuf),
    /// Clear the cubemap faces (fall back to the equirect / procedural sky).
    ClearSkyboxFaces,
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
    /// Re-run lightmap-UV generation for every model with a `.lmuv` marker (picks
    /// up the current unwrapper) by reloading the scene.
    RegenerateLightmapUvs,
    /// Generate lightmap UVs for ALL static models that lack a usable one (mark +
    /// reload), so the whole scene becomes bakeable in one click.
    GenerateAllLightmapUvs,
    /// Enable (true) or disable (false) auto-generated lightmap UVs for one model
    /// — writes/removes its `.lmuv` marker, then reloads. `true` generates UVs for
    /// the model's meshes that need them; `false` reverts to the model's own uv0.
    SetLightmapUvGen(String, bool),
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
    /// Lighting-bake settings + Bake / Clear ("FluxBaker").
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
    // Right (Inspector) node a touch wider (~30px at the 1600px default)
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

/// Active drag of a component gizmo handle (box face or range radius). One
/// unified state for all `GizmoSpec` kinds; the result is written back through
/// `EditorComponents::apply_gizmo_edit`, so built-ins and plugins resize the
/// same way and the drag suppresses the orbit camera for the whole gesture.
struct GizmoDrag {
    object: usize,
    /// Index into the object's `components`.
    component: usize,
    /// Index into that component's `gizmos()` list (the `GizmoEdit` index).
    gizmo: usize,
    start_cursor: egui::Pos2,
    kind: GizmoDragKind,
}

enum GizmoDragKind {
    BoxFace {
        axis: usize,
        sign: f32,
        /// Box centered on the object (resize moves the OBJECT) vs. its own
        /// local center (resize moves that center, e.g. a collider).
        object_anchored: bool,
        start_size: Vec3,
        start_center: Vec3,
        /// Object world translation at drag start (for object-anchored boxes).
        start_origin_world: Vec3,
        /// Unit outward direction of the dragged face, world space.
        world_axis: Vec3,
        /// World scale along `axis` (component size → world meters).
        scale_a: f32,
        /// Screen pixels per 1 world-meter along `world_axis`.
        screen_axis: egui::Vec2,
    },
    Range {
        start_radius: f32,
        /// Screen pixels per 1 world-meter along the radial (camera-right) dir.
        screen_axis: egui::Vec2,
    },
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
/// are filtered; the move/rotate/scale gizmos are never hidden. A filtered-
/// off billboard still draws when its object is the current selection.
#[derive(Clone)]
struct WidgetFilter {
    /// Master toggle (the eye button): when false, no gizmos/widgets/billboards
    /// are shown at all (transform handles included).
    enabled: bool,
    lights: WidgetSetting,
    cameras: WidgetSetting,
    probes: WidgetSetting,
    reflection_probes: WidgetSetting,
    audio: WidgetSetting,
    /// The selected object's grey orientation cross (own toggle, separate from the
    /// transform handles). Hidden when the master toggle is off too.
    cross: bool,
}

impl Default for WidgetFilter {
    fn default() -> Self {
        Self {
            enabled: true,
            lights: WidgetSetting::default(),
            cameras: WidgetSetting::default(),
            probes: WidgetSetting::default(),
            reflection_probes: WidgetSetting::default(),
            audio: WidgetSetting::default(),
            cross: true,
        }
    }
}

struct EngineApp {
    config: AppConfig,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    /// OpenXR context (VR). `None` when no runtime/headset; editor runs flat.
    xr: Option<citrus_xr::XrContext>,
    /// Live VR session (poses/lifecycle), once the device is created.
    xr_session: Option<citrus_xr::XrSession>,
    /// Latest VR tracker poses for the full-body IK solver (consumed by the
    /// avatar-IK application step — humanoid bone mapping).
    #[allow(dead_code)]
    vr_targets: citrus_core::TrackerTargets,
    /// VR play-space rig (fly / scale / drag locomotion); identity until driven.
    vr_rig: citrus_core::VrRig,
    /// True once the per-eye XR swapchains have been created (`setup_stereo`).
    xr_stereo_ready: bool,
    /// Grab-drag anchor (world point under the grabbing controller at grab start).
    vr_grab_anchor: Option<glam::Vec3>,
    /// Which hand currently owns the grab-drag (`Some(true)` = left, `Some(false)`
    /// = right). Re-anchors when the active hand changes so either grip can drag.
    vr_drag_hand: Option<bool>,
    /// Hand-menu anchor captured when the menu opens: (head world position at open,
    /// flat forward direction). The panel sits along `flat` at a distance and size
    /// that BOTH scale with the player's VR scale, so it keeps the same apparent
    /// size/distance whether you've scaled yourself up or down. `None` = closed.
    vr_panel_anchor: Option<(glam::Vec3, glam::Vec3)>,
    /// Inter-hand distance last frame (for two-handed scale).
    vr_prev_hand_dist: Option<f32>,
    /// Right trigger state last frame (edge detection for pointer select).
    vr_prev_trigger: bool,
    /// Distance along the pointer ray to hold a grabbed object.
    vr_move_dist: f32,
    /// Whether the VR hand menu is open (toggled by the menu button).
    vr_menu_open: bool,
    /// Menu button state last frame (edge detection).
    vr_prev_menu: bool,
    /// VR UI cursor position in egui points (from the right pointer on the panel).
    vr_ui_pointer: Option<egui::Pos2>,
    /// VR UI click state (right trigger over the panel) + last-frame edge.
    vr_ui_pressed: bool,
    vr_ui_prev_pressed: bool,
    /// What to draw for the VR UI overlay this frame (panel transform + cursor).
    vr_overlay_draw: Option<citrus_render::VrOverlayDraw>,
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
    /// Full multi-selection of scene objects (anchor = `selection`). When it
    /// holds >1 and contains the anchor, the inspector shows shared components/
    /// values and edits apply to all; otherwise selection is single (`selection`).
    multi_objects: Vec<usize>,
    /// Full multi-selection of files in the browser (ctrl/shift). The anchor is
    /// `Selection::File`. Used for batch operations and grid highlighting.
    multi_files: Vec<PathBuf>,
    /// Frames to wait after a reflection recapture before reading the cube back to
    /// disk (the capture lands in the next render frame). `Some(0)` = save now.
    refl_bake_pending: Option<u32>,
    file_material: Option<FileMaterial>,
    /// Model `.meta` currently shown in the Inspector (FBX/glTF import options).
    file_meta: Option<FileMeta>,
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
    /// Set each frame iff the Camera tab was the visible/active dock tab; gates
    /// the camera-preview render so it (and its Flux trace) is skipped when the
    /// tab is hidden.
    camera_tab_visible: bool,
    /// Set each frame iff the Viewport tab was visible; gates the main scene draws
    /// + viewport Flux trace so they're skipped when the viewport is hidden (e.g.
    /// only the Camera tab is shown).
    viewport_visible: bool,
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
    /// Play clock (seconds). Advances only while playing and not paused, so
    /// time-based components don't jump across a pause. Reset on Play start.
    play_time: f32,
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
    /// Last-seen mtime of the engine's standard.frag, for dev-time hot-reload
    /// when it is edited on disk outside the editor.
    engine_shader_mtime: Option<std::time::SystemTime>,
    /// Wall-clock of the last asset-change check (on window-focus regain), so
    /// externally-edited assets (e.g. a re-exported FBX) reimport automatically.
    last_asset_check: Option<std::time::SystemTime>,
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
    /// A heavy op to run after this frame paints its busy overlay (so the UI
    /// shows it's working instead of appearing frozen).
    pending_job: Option<PendingJob>,
    /// project.citrus: name, last scene, per-project engine settings.
    project: citrus_assets::ProjectFile,
    camera: FlyCamera,
    /// Orbit pivot, locked for the duration of one left-drag.
    orbit_pivot: Option<Vec3>,
    /// Right mouse held: mouse-look (cursor hidden + locked) + WASD fly.
    looking: bool,
    /// Set the frame mouse-look ends: egui's pointer was frozen during look
    /// (it never saw window events), so the next frame injects a PointerMoved
    /// to resync it, otherwise the first click is hit-tested at the stale
    /// position and the orbit-arm edge is missed (orbit dead until a 2nd click).
    look_just_ended: bool,
    /// Middle mouse held: pan.
    panning: bool,
    /// Raw mouse deltas accumulated while looking.
    look_delta: (f64, f64),
    keys: HashSet<KeyCode>,
    /// Input binding system (2C): drives Play-mode components from the active
    /// control scheme; editable in the Bindings window.
    input: crate::input_engine::InputManager,
    /// Active networking session (2G) when hosting/joined from the Network panel.
    net: Option<crate::net::NetSession>,
    /// Voice comms (task 8), created lazily once networking is active.
    voice: Option<crate::voice::VoiceChat>,
    /// Address entered in the Network panel for Join.
    net_addr: String,
    /// Bindings editor window open.
    show_bindings: bool,
    /// Network panel open.
    show_network: bool,
    /// Layers settings (names + collision matrix) window open.
    show_layers: bool,
    /// Audio mixer window open.
    show_mixer: bool,
    /// Audio bus mixer (volumes/mutes per bus). Drives playback gains.
    mixer: crate::audio_mixer::AudioMixer,
    /// Action currently being rebound in the Bindings window (scheme, action,
    /// slot). The next key/mouse press is captured into it.
    rebinding: Option<(usize, String)>,
    last_cursor: Option<(f64, f64)>,
    /// Viewport tab rect in egui points (updated每 frame by the tab).
    viewport_rect: egui::Rect,
    project_root: PathBuf,
    current_scene_path: Option<PathBuf>,
    /// Models (project-relative paths) that the in-progress bake found without a
    /// lightmap UV. `Some` shows a modal asking whether to generate one; cleared
    /// when the user picks an option.
    pending_lm_uv_models: Option<Vec<String>>,
    scene_name_input: String,
    show_stats: bool,
    /// Desired state of the separate profiler window (persisted). Reconciled
    /// against `profiler_window` each frame to open/close the real OS window.
    show_stats_overlay: bool,
    /// The separate profiler OS window (movable to another monitor), its own
    /// egui context + winit input state, and a rolling history for the graphs.
    /// CPU-drawn splash window shown during startup; closed after the first
    /// editor frame. `pending_init` defers the heavy `init` until after the
    /// splash has been shown (so it's actually visible during the blocking load).
    /// Background task system (worker imports, chunked bake) + status-bar UI.
    tasks: crate::tasks::TaskManager,
    /// Whether the status-bar background-tasks popup is expanded.
    show_task_popup: bool,
    /// Active chunked bake, stepped one unit per frame; None when idle.
    bake_job: Option<BakeRunner>,
    /// True for one frame after a bake starts, so the bake modal paints before
    /// the first UI-locking lightmap step.
    bake_pending_warmup: bool,
    /// Model imports that finished parsing while a bake is running; their GPU
    /// upload is deferred until the bake ends (it would realloc `meshes` and
    /// invalidate the bake's TLAS).
    deferred_model_applies: Vec<(PathBuf, citrus_assets::Scene)>,
    splash: Option<crate::splash::Splash>,
    pending_init: bool,
    /// Set once the splash has actually painted a frame, so the load only starts
    /// after it's visible (Wayland needs a configure roundtrip).
    splash_painted: bool,
    /// Flat-path renderer build in flight on a worker thread; `about_to_wait`
    /// polls this and calls `finish_init` when the renderer arrives.
    renderer_rx: Option<std::sync::mpsc::Receiver<Result<Renderer>>>,
    /// The main window, held while the renderer + scene load before the editor
    /// opens (so it never appears half-loaded).
    pending_window: Option<Arc<Window>>,
    /// Current load phase ("Building plugins…", "Loading 3D models…", …), shown
    /// on the splash at startup and the modal for in-editor scene loads. Shared
    /// so the scene-parse worker can update it too.
    load_status: std::sync::Arc<std::sync::Mutex<String>>,
    profiler_window: Option<Arc<Window>>,
    profiler_egui_ctx: egui::Context,
    profiler_egui_state: Option<egui_winit::State>,
    prof_history: ProfHistory,
    /// Last frame's render stats, cached so the profiler window UI can read them
    /// outside the main egui pass.
    last_render_stats: citrus_render::RenderStats,
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
    /// In-progress component-gizmo handle drag (box face / range radius).
    handle_drag: Option<GizmoDrag>,
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
    /// Which material slot of the selected object the inspector edits (for
    /// multi-material meshes). Clamped to the object's slot count.
    selected_material_slot: usize,
    /// EMA-smoothed per-frame CPU section timings (ms) for the stats overlay.
    frame_timings: FrameTimings,
    /// GI debug view: 0 = off, 1 = world normals, 2 = indirect/GI term.
    gi_debug: u32,
}

/// Per-frame main-thread CPU costs (ms), EMA-smoothed for a readable overlay.
#[derive(Default, Clone, Copy)]
struct FrameTimings {
    gi: f32,
    components: f32,
    physics: f32,
    audio: f32,
    draws: f32,
    render: f32,
    /// GPU-side timings (ms), read back from timestamp queries via RenderStats.
    gpu_frame: f32,
    gpu_gi: f32,
}

impl FrameTimings {
    /// Blend a fresh sample into the smoothed value.
    fn ema(slot: &mut f32, sample_ms: f32) {
        *slot += (sample_ms - *slot) * 0.2;
    }
}

/// A rolling series of time-bucketed samples (one value per ~50 ms bucket), so
/// the graph always spans the same wall-clock window regardless of frame rate.
struct Series {
    data: std::collections::VecDeque<f32>,
}

impl Default for Series {
    fn default() -> Self {
        Self {
            data: std::collections::VecDeque::with_capacity(Series::CAP),
        }
    }
}

impl Series {
    fn push(&mut self, v: f32) {
        if self.data.len() == Series::CAP {
            self.data.pop_front();
        }
        self.data.push_back(v);
    }

    fn last(&self) -> f32 {
        self.data.back().copied().unwrap_or(0.0)
    }

    fn max(&self) -> f32 {
        self.data.iter().copied().fold(0.0_f32, f32::max)
    }
}

/// One graphed metric: its rolling series, display style, and visibility.
struct Metric {
    label: &'static str,
    color: egui::Color32,
    unit: &'static str,
    visible: bool,
    series: Series,
    /// Max over the in-progress time bucket (flushed to `series` per bucket).
    pending: f32,
}

impl Metric {
    fn new(label: &'static str, color: egui::Color32, unit: &'static str, visible: bool) -> Self {
        Self {
            label,
            color,
            unit,
            visible,
            series: Series::default(),
            pending: 0.0,
        }
    }
}

/// Rolling history of every profiler metric, in a fixed order (see the index
/// constants), plus the current time-bucket accumulator.
struct ProfHistory {
    metrics: Vec<Metric>,
    bucket_t: f32,
}

impl Series {
    /// Graph window / resolution: 20 s of history at one sample per 50 ms.
    const WINDOW: f32 = 20.0;
    const BUCKET: f32 = 0.05;
    const CAP: usize = (Series::WINDOW / Series::BUCKET) as usize;
}

impl Default for ProfHistory {
    fn default() -> Self {
        use egui::Color32 as C;
        // Order MUST match `record_prof_sample`'s value array. Defaults: the
        // headline costs visible, the rest off to keep the graph readable.
        Self {
            metrics: vec![
                Metric::new("Frame time", C::from_rgb(120, 200, 255), "ms", true),
                Metric::new("FPS", C::from_rgb(140, 220, 140), "", false),
                Metric::new("GPU frame", C::from_rgb(255, 170, 90), "ms", true),
                Metric::new("GPU: Flux GI", C::from_rgb(255, 120, 200), "ms", true),
                Metric::new("GPU: shadows", C::from_rgb(160, 210, 255), "ms", true),
                Metric::new("GPU: scene", C::from_rgb(255, 230, 120), "ms", true),
                Metric::new("GPU: reflections", C::from_rgb(120, 230, 200), "ms", true),
                Metric::new("GPU: post", C::from_rgb(210, 160, 255), "ms", true),
                Metric::new("GPU: egui", C::from_rgb(255, 180, 160), "ms", false),
                Metric::new("GPU: camera preview", C::from_rgb(180, 255, 160), "ms", false),
                Metric::new("GPU utilization", C::from_rgb(230, 200, 90), "%", false),
                Metric::new("CPU realtime GI", C::from_rgb(150, 180, 230), "ms", false),
                Metric::new("CPU components", C::from_rgb(120, 220, 220), "ms", false),
                Metric::new("CPU physics", C::from_rgb(180, 160, 240), "ms", false),
                Metric::new("CPU audio", C::from_rgb(200, 220, 130), "ms", false),
                Metric::new("CPU build draws", C::from_rgb(160, 200, 160), "ms", false),
                Metric::new("CPU render submit", C::from_rgb(220, 130, 130), "ms", false),
                Metric::new("Draw calls", C::from_rgb(190, 190, 190), "", false),
            ],
            bucket_t: 0.0,
        }
    }
}

/// Round up to a "nice" axis maximum (1/2/5 × 10ⁿ) so the y grid lands on
/// readable values.
fn nice_ceil(v: f32) -> f32 {
    if v <= 0.0 {
        return 1.0;
    }
    let exp = v.log10().floor();
    let base = 10f32.powf(exp);
    let n = v / base;
    let nice = if n <= 1.0 {
        1.0
    } else if n <= 2.0 {
        2.0
    } else if n <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * base
}

/// Draw all visible metrics into one combined graph with a shared linear axis
/// (value on the left and right edges, time along the bottom). Hovering picks the
/// nearest line: it's drawn bright while the others dim, and a tooltip shows that
/// metric's name, value, and how long ago the sample is.
fn draw_combined_graph(ui: &mut egui::Ui, h: &ProfHistory) {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 260.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3.0, egui::Color32::from_gray(16));

    // Inset a plot area, leaving margins for the axis labels.
    let plot = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 46.0, rect.top() + 8.0),
        egui::pos2(rect.right() - 46.0, rect.bottom() - 18.0),
    );

    // Shared axis: scale to the largest visible value, and use the common unit
    // when every visible series shares one (e.g. all "ms").
    let mut gmax = 0.0f32;
    let mut unit: Option<&str> = None;
    let mut unit_mismatch = false;
    for m in h.metrics.iter().filter(|m| m.visible) {
        gmax = gmax.max(m.series.max());
        match unit {
            None => unit = Some(m.unit),
            Some(u) if u != m.unit => unit_mismatch = true,
            _ => {}
        }
    }
    let unit = if unit_mismatch { "" } else { unit.unwrap_or("") };
    let gmax = nice_ceil(gmax.max(1e-3));
    let grid = egui::Color32::from_gray(34);
    let lbl_col = egui::Color32::from_gray(130);

    // Horizontal grid + value labels on both sides.
    for k in 0..=4 {
        let f = k as f32 / 4.0;
        let y = plot.bottom() - f * plot.height();
        p.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            egui::Stroke::new(1.0, grid),
        );
        let val = gmax * f;
        let txt = if unit.is_empty() {
            format!("{val:.1}")
        } else {
            format!("{val:.1}{unit}")
        };
        p.text(
            egui::pos2(plot.left() - 4.0, y),
            egui::Align2::RIGHT_CENTER,
            &txt,
            egui::FontId::monospace(10.0),
            lbl_col,
        );
        p.text(
            egui::pos2(plot.right() + 4.0, y),
            egui::Align2::LEFT_CENTER,
            &txt,
            egui::FontId::monospace(10.0),
            lbl_col,
        );
    }
    // Vertical grid + time labels (every 5 s; "now" on the right).
    let tdiv = (Series::WINDOW / 5.0) as i32;
    for k in 0..=tdiv {
        let f = k as f32 / tdiv as f32;
        let x = plot.right() - f * plot.width();
        p.line_segment(
            [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
            egui::Stroke::new(1.0, grid),
        );
        let label = if k == 0 {
            "now".to_string()
        } else {
            format!("-{}s", (f * Series::WINDOW) as i32)
        };
        p.text(
            egui::pos2(x, plot.bottom() + 3.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::monospace(10.0),
            lbl_col,
        );
    }

    let n = h
        .metrics
        .iter()
        .filter(|m| m.visible)
        .map(|m| m.series.data.len())
        .max()
        .unwrap_or(0);
    if n < 2 {
        return;
    }
    let dx = plot.width() / (Series::CAP.saturating_sub(1)).max(1) as f32;
    let x0 = plot.right() - dx * (n - 1) as f32; // newest on the right
    let y_of = |v: f32| plot.bottom() - (v / gmax).clamp(0.0, 1.0) * plot.height();

    // Hover: nearest sample column, then the visible series whose point is
    // closest to the cursor.
    let hover = resp.hover_pos().filter(|pos| plot.contains(*pos));
    let mut hovered: Option<usize> = None;
    let mut hover_i = 0usize;
    if let Some(pos) = hover {
        hover_i = (((pos.x - x0) / dx).round() as i32).clamp(0, n as i32 - 1) as usize;
        let mut best = f32::INFINITY;
        for (mi, m) in h.metrics.iter().enumerate().filter(|(_, m)| m.visible) {
            if let Some(&v) = m.series.data.get(hover_i) {
                let d = (y_of(v) - pos.y).abs();
                if d < best {
                    best = d;
                    hovered = Some(mi);
                }
            }
        }
    }

    for (mi, m) in h.metrics.iter().enumerate().filter(|(_, m)| m.visible) {
        let dimmed = hovered.is_some() && hovered != Some(mi);
        let color = if dimmed {
            m.color.linear_multiply(0.22)
        } else {
            m.color
        };
        let width = if hovered == Some(mi) { 2.5 } else { 1.3 };
        let pts: Vec<egui::Pos2> = m
            .series
            .data
            .iter()
            .enumerate()
            .map(|(i, &v)| egui::pos2(x0 + dx * i as f32, y_of(v)))
            .collect();
        p.add(egui::Shape::line(pts, egui::Stroke::new(width, color)));
    }

    // Hover marker + tooltip for the picked series.
    if let (Some(pos), Some(mi)) = (hover, hovered) {
        let m = &h.metrics[mi];
        let v = m.series.data.get(hover_i).copied().unwrap_or(0.0);
        let x = x0 + dx * hover_i as f32;
        let y = y_of(v);
        p.line_segment(
            [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
            egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
        );
        p.circle_filled(egui::pos2(x, y), 3.5, m.color);
        let ago = (n - 1 - hover_i) as f32 * Series::BUCKET;
        let unit = if m.unit.is_empty() {
            String::new()
        } else {
            format!(" {}", m.unit)
        };
        let txt = format!("{}\n{v:.2}{unit}   {ago:.1}s ago", m.label);
        let galley = p.layout_no_wrap(txt, egui::FontId::monospace(12.0), m.color);
        let sz = galley.size();
        let tip = egui::pos2(
            (pos.x + 12.0).min(plot.right() - sz.x - 6.0),
            (pos.y + 12.0).min(plot.bottom() - sz.y - 6.0),
        );
        p.rect_filled(
            egui::Rect::from_min_size(tip - egui::vec2(5.0, 3.0), sz + egui::vec2(10.0, 6.0)),
            3.0,
            egui::Color32::from_black_alpha(230),
        );
        p.galley(tip, galley, m.color);
    }
}

/// A clickable legend row (full-width hit area): colour swatch + label + current
/// value and rolling max. Returns the response so the caller can toggle the
/// metric's visibility; hidden metrics are dimmed.
fn legend_row(ui: &mut egui::Ui, m: &Metric) -> egui::Response {
    let cur = m.series.last();
    let maxv = m.series.max();
    let text_col = if m.visible {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_gray(110)
    };
    let resp = ui
        .horizontal(|ui| {
            let (sw, _) =
                ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
            let sw_col = if m.visible { m.color } else { egui::Color32::from_gray(70) };
            ui.painter().rect_filled(sw, 2.0, sw_col);
            ui.label(egui::RichText::new(m.label).size(13.0).color(text_col));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let unit = if m.unit.is_empty() {
                    String::new()
                } else {
                    format!(" {}", m.unit)
                };
                ui.label(
                    egui::RichText::new(format!("{cur:.2}{unit}   (max {maxv:.1})"))
                        .monospace()
                        .size(12.0)
                        .color(text_col),
                );
            });
        })
        .response;
    // Make the whole row a click target (labels don't consume clicks).
    let resp = resp.interact(egui::Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(resp.rect, 2.0, egui::Color32::from_white_alpha(8));
    }
    resp
}

/// Shared egui style for the editor (and the profiler window, which uses its own
/// context): bump tiny text to a readable floor, and use a thin floating
/// scrollbar.
///
/// The bar is a thin OVERLAY (`floating`, no allocated gutter): reserving width
/// for the bar only when it's shown reflows the content, which toggles the
/// scrollbar and bounces the layout every frame (seen in the inspector). Keeping
/// it a pure overlay means the bar never changes layout. (Avoiding overlap with
/// content is a per-ScrollArea concern — e.g. `auto_shrink`/inner margin — not a
/// global one.)
fn apply_editor_style(ctx: &egui::Context) {
    ctx.style_mut(|style| {
        for font_id in style.text_styles.values_mut() {
            if font_id.size < 13.0 {
                font_id.size = 13.0;
            }
        }
        let s = &mut style.spacing.scroll;
        s.floating = true;
        s.bar_width = 8.0;
        s.floating_width = 8.0;
        s.floating_allocated_width = 0.0;
    });
}

/// A plain label + right-aligned integer count row (no graph).
fn count_row(ui: &mut egui::Ui, label: &str, value: u32) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(13.0).color(egui::Color32::WHITE));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(value.to_string())
                    .monospace()
                    .size(13.0)
                    .color(egui::Color32::WHITE),
            );
        });
    });
}

impl EngineApp {
    /// Update the splash window's status line (no-op once it's closed).
    /// Set the current load phase. Updates the shared string (read by the splash
    /// each tick and the in-editor modal) and presents the splash immediately so
    /// the phase shows even right before a blocking main-thread step.
    fn set_load_status(&mut self, status: &str) {
        *self.load_status.lock().unwrap() = status.to_string();
        if let Some(s) = self.splash.as_mut() {
            s.set_status(status);
        }
    }

    /// Open the editor window (deferred until the renderer + scene finished
    /// loading, so it never appears half-loaded). The splash closes on the first
    /// editor frame.
    fn open_editor(&mut self) {
        *self.load_status.lock().unwrap() = String::new();
        self.window = self.pending_window.take();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Start the editor load: create the window, then build the Vulkan renderer
    /// on a worker thread so the splash keeps animating during the (heavy) build.
    /// VR is the exception — its Vulkan must be created against the OpenXR
    /// instance, so that path builds synchronously. `finish_init` runs once the
    /// renderer is ready (immediately for VR; from `about_to_wait` otherwise).
    fn start_load(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        // Phosphor icon font (file-browser type icons, etc.).
        citrus_editor::install_icon_font(&self.egui_ctx);
        apply_editor_style(&self.egui_ctx);

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

        // Best-effort center on the primary monitor. X11 honors this; Wayland
        // leaves window placement to the compositor (this becomes a no-op there).
        if let Some(mon) = window
            .primary_monitor()
            .or_else(|| window.available_monitors().next())
        {
            let ms = mon.size();
            let ws = window.outer_size();
            if ms.width > ws.width && ms.height > ws.height {
                let origin = mon.position();
                let x = origin.x + ((ms.width - ws.width) / 2) as i32;
                let y = origin.y + ((ms.height - ws.height) / 2) as i32;
                window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
        }

        // OpenXR (None when no headset). VR needs Vulkan created against the XR
        // instance, so build synchronously and finish immediately.
        let xr = match citrus_xr::XrContext::start("citrus editor") {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("OpenXR start failed: {e:#}");
                None
            }
        };
        if let Some(xr) = xr {
            tracing::info!("VR ready: {}", xr.system_name());
            let renderer =
                Renderer::new_with_xr(window.clone(), Some((&xr.instance, xr.system)))?;
            return self.finish_init(window, renderer, Some(xr));
        }

        // Flat path: build the renderer off-thread. `about_to_wait` polls the
        // channel and calls `finish_init` when it lands, ticking the splash
        // animation in the meantime.
        let (tx, rx) = std::sync::mpsc::channel();
        let win = window.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Renderer::new(win));
        });
        self.renderer_rx = Some(rx);
        self.pending_window = Some(window);
        Ok(())
    }

    /// Finish the load once the renderer exists: plugins, project, scene, and the
    /// rest of GPU setup. Stays on the main thread — the scene holds non-Send
    /// `dyn Component` instances, so it can't be built off-thread.
    fn finish_init(
        &mut self,
        window: Arc<Window>,
        mut renderer: Renderer,
        xr: Option<citrus_xr::XrContext>,
    ) -> Result<()> {
        // Plugins first: scene files may reference plugin components.
        self.set_load_status("Building plugins…");
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
        // Load the project's input bindings (2C) into the live input manager.
        self.input.bindings = self.project.bindings.clone();
        renderer.set_vsync(self.project.settings.vsync);
        self.show_stats = self.project.settings.show_stats;
        self.show_stats_overlay = self.project.settings.show_stats_overlay;
        self.gizmo.snap = self.project.settings.snap;
        self.gizmo.grid_size = self.project.settings.grid_size;

        self.set_load_status("Loading scene…");
        // Decide the scene to load. A `.scene` is parsed on a worker thread (the
        // editor opens only once it's fully loaded, never half-loaded). Cheap/rare
        // cases (a single CLI model, the builtin test scene) load synchronously
        // here using the local `renderer`.
        let mut bg_scene: Option<PathBuf> = None;
        match self.config.scene_path.clone() {
            Some(path) if path.ends_with(".scene") => bg_scene = Some(PathBuf::from(path)),
            Some(path) => {
                let asset = citrus_assets::load_model_with_meta(&path)?;
                self.scene
                    .add_asset_scene(&mut renderer, &asset, Some(Path::new(&path)))?;
            }
            None => {
                let last = self
                    .project
                    .last_scene
                    .clone()
                    .map(|rel| self.project_root.join(rel))
                    .filter(|abs| abs.exists());
                match last {
                    Some(abs) => bg_scene = Some(abs),
                    None => {
                        let asset = citrus_assets::test_scene();
                        self.scene.add_asset_scene(&mut renderer, &asset, None)?;
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
        // Hold the window closed (in `pending_window`) until the scene is loaded.
        self.pending_window = Some(window);
        // Now that the renderer's Vulkan device was created (via OpenXR when VR is
        // present), create the session that shares it.
        if let Some(xr) = xr {
            let (vi, pd, dev, qfi) = self.renderer.as_ref().unwrap().vulkan_raw_handles();
            match xr.create_session(vi, pd, dev, qfi) {
                Ok(session) => self.xr_session = Some(session),
                Err(e) => tracing::warn!("OpenXR session create failed: {e:#}"),
            }
            self.xr = Some(xr);
        }
        match bg_scene {
            // Parse on a worker; `apply_loaded_scene` uploads + opens the editor
            // when it lands (polled in `about_to_wait` while the splash shows).
            Some(path) => self.spawn_scene_load(path, false),
            // Synchronously-loaded model/test scene: finalize + open now.
            None => {
                self.after_scene_loaded();
                self.open_editor();
            }
        }
        Ok(())
    }

    /// Parse a `.scene` (file + its models) on a worker thread; the GPU upload
    /// happens in `apply_loaded_scene` on the main thread when it lands. `blocking`
    /// shows the in-editor modal (use it for runtime loads, not startup).
    fn spawn_scene_load(&mut self, path: PathBuf, blocking: bool) {
        let root = self.project_root.clone();
        let status = self.load_status.clone();
        // Loader thread uploads meshes/textures on the transfer queue (when
        // available), so the main thread only installs handles + builds — keeping
        // the UI/splash responsive. None → main-thread upload fallback.
        let uploader = self.renderer.as_ref().and_then(|r| r.uploader());
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "scene".into());
        self.tasks.spawn(
            format!("Loading {label}"),
            crate::tasks::TaskKind::LoadScene,
            blocking,
            move |cancel, _progress| {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(crate::tasks::TaskPayload::None);
                }
                *status.lock().unwrap() = "Loading 3D models…".to_string();
                let file = citrus_assets::load_scene_file(&path).map_err(|e| e.to_string())?;
                let models = LoadedScene::parse_scene_models(&file, &root).map_err(|e| e.to_string())?;
                // Upload each model's GPU resources on this worker thread, plus the
                // scene's material textures (the heavy 4K EXR/PNG decode), so the
                // main thread only installs handles. None → main-thread fallback.
                let mut prepared = None;
                let mut material_textures = None;
                if let Some(up) = &uploader {
                    *status.lock().unwrap() = "Uploading 3D models…".to_string();
                    let mut map = std::collections::HashMap::new();
                    for (p, scene) in &models {
                        let has_skel = !scene.skeletons.is_empty();
                        let prep = up
                            .prepare(&scene.meshes, &scene.textures, has_skel)
                            .map_err(|e| e.to_string())?;
                        map.insert(p.clone(), prep);
                    }
                    prepared = Some(map);

                    let refs = LoadedScene::collect_material_texture_refs(&file, &root);
                    if !refs.is_empty() {
                        *status.lock().unwrap() = "Loading textures…".to_string();
                        // Decode + BC-encode every texture in parallel across cores
                        // (each call is independent CPU work). Results are stored by
                        // index so order is preserved; uploads stay serial below
                        // (the transfer queue isn't shared across threads).
                        let slots: Vec<std::sync::Mutex<Option<Result<_, String>>>> =
                            (0..refs.len()).map(|_| std::sync::Mutex::new(None)).collect();
                        let next = std::sync::atomic::AtomicUsize::new(0);
                        // Reserve cores for the UI/OS so a big import doesn't pin the
                        // whole machine while the editor is interactive.
                        let threads = crate::sw_gi::bake_worker_count().min(refs.len());
                        std::thread::scope(|s| {
                            for _ in 0..threads {
                                s.spawn(|| {
                                    crate::sw_gi::lower_compute_priority();
                                    loop {
                                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if i >= refs.len() {
                                        break;
                                    }
                                    let (abs, srgb) = &refs[i];
                                    let t0 = Instant::now();
                                    let r = citrus_assets::load_texture_bc(abs, *srgb)
                                        .map_err(|e| e.to_string());
                                    if let Ok(ct) = &r {
                                        tracing::info!(
                                            "texture {} ready in {:?} ({}x{}, {} mips)",
                                            abs.display(),
                                            t0.elapsed(),
                                            ct.width,
                                            ct.height,
                                            ct.mips.len()
                                        );
                                    }
                                    *slots[i].lock().unwrap() = Some(r);
                                    }
                                });
                            }
                        });
                        let mut compressed = Vec::with_capacity(refs.len());
                        for slot in slots {
                            compressed.push(slot.into_inner().unwrap().unwrap()?);
                        }
                        *status.lock().unwrap() = "Uploading textures…".to_string();
                        let prep = up.prepare_compressed(&compressed).map_err(|e| e.to_string())?;
                        material_textures = Some((refs, prep));
                    }
                }
                Ok(crate::tasks::TaskPayload::Scene {
                    path,
                    file: Box::new(file),
                    models,
                    prepared,
                    material_textures,
                })
            },
        );
    }

    /// Main-thread GPU upload of a worker-parsed scene, then finalize. Opens the
    /// editor if this was the startup load (window still pending).
    fn apply_loaded_scene(
        &mut self,
        path: PathBuf,
        file: citrus_assets::SceneFile,
        models: std::collections::HashMap<String, citrus_assets::Scene>,
        prepared: Option<std::collections::HashMap<String, citrus_render::PreparedScene>>,
        material_textures: Option<(Vec<(PathBuf, bool)>, citrus_render::PreparedScene)>,
    ) {
        self.set_load_status("Installing scene…");
        // Pump the splash (throttled) as each model/material uploads so it keeps
        // animating through the otherwise-blocking GPU upload. The splash is
        // taken out so the closure owns it (no borrow clash with the scene/render
        // fields the loader needs).
        let mut splash = self.splash.take();
        let status = self.load_status.clone();
        let mut last = Instant::now();
        let uploaded = match self.renderer.as_mut() {
            Some(renderer) => LoadedScene::load_scene_file_with_models(
                renderer,
                &file,
                &self.project_root,
                &self.components,
                &mut self.shaders,
                &models,
                prepared,
                material_textures,
                |label: &str| {
                    *status.lock().unwrap() = label.to_string();
                    if let Some(s) = splash.as_mut()
                        && last.elapsed().as_millis() >= 16
                    {
                        let _ = s.set_status(label);
                        last = Instant::now();
                    }
                },
            ),
            None => {
                self.splash = splash;
                return;
            }
        };
        self.splash = splash;
        match uploaded {
            Ok(scene) => {
                self.scene = scene;
                self.current_scene_path = Some(path);
                if let Some(c) = file.editor_camera {
                    self.camera.position = Vec3::from(c.position);
                    self.camera.yaw = c.yaw;
                    self.camera.pitch = c.pitch;
                }
                self.restore_collapsed(&file.collapsed);
                self.set_load_status("Setting up lighting…");
                self.after_scene_loaded();
                self.tasks
                    .notify("Scene loaded", crate::tasks::NotifyLevel::Info);
            }
            Err(e) => {
                tracing::error!("scene load: {e:#}");
                self.tasks
                    .notify(format!("Scene load failed: {e}"), crate::tasks::NotifyLevel::Warn);
            }
        }
        // Startup load: now that the scene is ready, open the editor.
        if self.window.is_none() {
            self.open_editor();
        }
    }

    /// Post-load setup shared by sync and background scene loads: skybox, baked
    /// lighting upload, reflection env. Each `set_load_status` also redraws the
    /// splash, so the phase text reflects the step that is actually running (and
    /// is the truth, not a fixed label) rather than a single "Setting up lighting"
    /// covering everything.
    fn after_scene_loaded(&mut self) {
        self.set_load_status("Loading skybox…");
        let t0 = Instant::now();
        self.apply_skybox();
        tracing::info!("after_scene_loaded: skybox total {:?}", t0.elapsed());
        let t1 = Instant::now();

        // Baked lighting AFTER the renderer is stored, so `upload_baked_probes`
        // can push lightmaps/probes to the GPU (else a baked scene loads black).
        // Only announce the GPU upload when there is actually baked data on disk;
        // otherwise this is a no-op and a "uploading lighting" label would lie.
        let base = self.bake_base_path();
        self.scene.load_bake_sidecars(&base);
        if self
            .scene
            .baked
            .as_ref()
            .is_some_and(|b| !b.lightmaps.is_empty() || !b.probe_sh.is_empty())
        {
            self.set_load_status("Uploading baked lighting…");
        }
        self.upload_baked_probes();
        tracing::info!("after_scene_loaded: bake {:?}", t1.elapsed());
        let t2 = Instant::now();

        // Reflection env: a Baked-mode probe loads its `.reflprobe` sidecar; a
        // Realtime-mode probe (the default) always captures live, so the on-disk
        // sidecar can never go stale relative to the scene or to engine/capture
        // fixes. (Previously any existing sidecar was loaded regardless of mode,
        // so a stale capture masked every later fix until a manual re-bake.)
        let use_baked = self.scene.has_baked_reflection_probe();
        if use_baked && self.load_reflection_bake() {
            self.set_load_status("Loading reflection probe…");
        } else if let Some(center) = self
            .scene
            .active_reflection_probe(glam::Vec3::ZERO)
            .map(|p| (glam::Vec3::from(p.center), p.resolution))
        {
            if self.renderer.is_some() {
                self.set_load_status("Capturing reflections…");
                if let Some(r) = self.renderer.as_mut() {
                    r.request_reflection_capture(center.0, center.1);
                }
            }
        }
        tracing::info!("after_scene_loaded: reflection {:?}", t2.elapsed());
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

    /// Cursor position (egui points) → world ray. NDC is relative to the viewport
    /// tab rect, since the scene renders into a texture filling that rect (RTT).
    fn cursor_ray(&self, pos: egui::Pos2) -> Option<(Vec3, Vec3)> {
        let rect = self.viewport_rect;
        if !rect.is_finite() || rect.width() < 1.0 || rect.height() < 1.0 {
            return None;
        }
        let ndc_x = 2.0 * ((pos.x - rect.left()) / rect.width()) - 1.0;
        let ndc_y = 1.0 - 2.0 * ((pos.y - rect.top()) / rect.height());
        let aspect = rect.width() / rect.height();
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
        // Any single-source selection (viewport pick, spawn, hierarchy single
        // click) collapses the multi-selection to just this object.
        self.multi_objects = index.into_iter().collect();
    }

    /// The effective multi-selection of scene objects. Falls back to just the
    /// anchor when the stored set is stale (an external source changed the
    /// anchor without going through the scene panel).
    fn selected_object_indices(&self) -> Vec<usize> {
        match self.selection {
            Selection::Object(i) => {
                if self.multi_objects.len() > 1 && self.multi_objects.contains(&i) {
                    self.multi_objects.clone()
                } else {
                    vec![i]
                }
            }
            _ => Vec::new(),
        }
    }

    /// Build the VR menu's floating panels: the editor UI split into the screen
    /// regions AROUND the viewport (left/right/top/bottom strips, viewport omitted),
    /// each as a 3D quad in a world-anchored arc that scales with the player. Returns
    /// the panels (parallel to the screen regions they sample, for pointer mapping).
    fn vr_compute_panels(
        &self,
        head0: glam::Vec3,
        flat: glam::Vec3,
    ) -> ([Option<citrus_render::VrPanel>; 4], Vec<egui::Rect>) {
        use glam::{Mat4, Quat, Vec3};
        let screen = self.egui_ctx.content_rect();
        let (sw, sh) = (screen.width().max(1.0), screen.height().max(1.0));
        let vp = self.viewport_rect;
        let scale = self.vr_rig.scale.max(1e-3);
        let mut regions: Vec<egui::Rect> = Vec::new();
        if vp.min.x - screen.min.x > 8.0 {
            regions.push(egui::Rect::from_min_max(
                screen.min,
                egui::pos2(vp.min.x, screen.max.y),
            ));
        }
        if screen.max.x - vp.max.x > 8.0 {
            regions.push(egui::Rect::from_min_max(
                egui::pos2(vp.max.x, screen.min.y),
                screen.max,
            ));
        }
        if vp.min.y - screen.min.y > 8.0 {
            regions.push(egui::Rect::from_min_max(
                egui::pos2(vp.min.x, screen.min.y),
                egui::pos2(vp.max.x, vp.min.y),
            ));
        }
        if screen.max.y - vp.max.y > 8.0 {
            regions.push(egui::Rect::from_min_max(
                egui::pos2(vp.min.x, vp.max.y),
                egui::pos2(vp.max.x, screen.max.y),
            ));
        }
        if regions.is_empty() {
            regions.push(screen); // fallback: no viewport rect yet → one panel
        }
        regions.truncate(4);
        let n = regions.len();
        let base_yaw = flat.x.atan2(flat.z);
        let dist = 1.4 * scale;
        let ph = 0.9 * scale;
        let mut panels: [Option<citrus_render::VrPanel>; 4] = [None; 4];
        for (i, r) in regions.iter().enumerate() {
            let frac = if n > 1 {
                i as f32 / (n as f32 - 1.0) - 0.5
            } else {
                0.0
            };
            let yaw = base_yaw + frac * 1.3; // ~±37° spread across the arc
            let dir = Vec3::new(yaw.sin(), 0.0, yaw.cos());
            let pos = head0 + dir * dist;
            let to_user = (head0 - pos).normalize_or(Vec3::Z);
            let face_yaw = to_user.x.atan2(to_user.z);
            let rot = Quat::from_rotation_y(face_yaw);
            let aspect = (r.width() / r.height().max(1.0)).clamp(0.1, 6.0);
            let pw = ph * aspect;
            let model = Mat4::from_scale_rotation_translation(Vec3::new(pw, ph, 1.0), rot, pos);
            let uv_rect = [
                (r.min.x - screen.min.x) / sw,
                (r.min.y - screen.min.y) / sh,
                (r.max.x - screen.min.x) / sw,
                (r.max.y - screen.min.y) / sh,
            ];
            panels[i] = Some(citrus_render::VrPanel { model, uv_rect });
        }
        (panels, regions)
    }

    /// VR locomotion + pointer interaction, driven by controller input. Tracker/
    /// controller poses are stage-space (from `body_poses`); the `VrRig` maps them
    /// to world. Left stick = fly, right stick = turn + vertical, left grip =
    /// grab-drag the world, both grips = scale yourself, right trigger = pick/move,
    /// menu button = toggle the hand menu. All relative to the player rig.
    fn update_vr(&mut self, input: citrus_xr::VrInput, dt: f32) {
        use glam::Vec3;
        let head = self.vr_targets.head;
        let lhand = self.vr_targets.left_hand;
        let rhand = self.vr_targets.right_hand;
        let speed = 3.0 * self.vr_rig.scale; // bigger player covers more ground

        // Fly relative to the head's facing (projected onto the ground plane).
        if let Some((_, hrot)) = head {
            let wr = self.vr_rig.stage_rot_to_world(hrot);
            let fwd = (wr * Vec3::NEG_Z * Vec3::new(1.0, 0.0, 1.0)).normalize_or_zero();
            let right = (wr * Vec3::X * Vec3::new(1.0, 0.0, 1.0)).normalize_or_zero();
            let (lx, ly) = input.left_stick;
            let delta = (right * lx + fwd * ly) * speed * dt;
            if delta.length_squared() > 0.0 {
                self.vr_rig.fly(delta);
            }
            // Right stick turns only (no vertical fly — use a grip-drag to move
            // yourself up/down instead).
            let (rx, _ry) = input.right_stick;
            if rx.abs() > 0.05 {
                let pivot = head
                    .map(|(p, _)| self.vr_rig.stage_to_world(p))
                    .unwrap_or(self.vr_rig.origin);
                self.vr_rig.turn_about(pivot, -rx * 1.5 * dt);
            }
        }

        // Two-handed scale: change by the inter-hand distance ratio about the
        // midpoint (pull apart = world grows / you shrink).
        let both_grip = input.left_grip > 0.5 && input.right_grip > 0.5;
        if both_grip {
            if let (Some((lp, _)), Some((rp, _))) = (lhand, rhand) {
                let lw = self.vr_rig.stage_to_world(lp);
                let rw = self.vr_rig.stage_to_world(rp);
                let dist = (lw - rw).length();
                if let Some(prev) = self.vr_prev_hand_dist {
                    if prev > 1e-3 && dist > 1e-3 {
                        self.vr_rig.scale_about((lw + rw) * 0.5, prev / dist);
                    }
                }
                self.vr_prev_hand_dist = Some(dist);
            }
        } else {
            self.vr_prev_hand_dist = None;
        }

        // Grab-drag the world with EITHER grip (when not scaling). Up/down comes
        // from dragging vertically. The active hand re-anchors on change so you
        // can hand off the drag from one controller to the other.
        if both_grip {
            self.vr_grab_anchor = None;
            self.vr_drag_hand = None;
        } else {
            // Prefer whichever hand is gripping; if both single grips somehow,
            // left wins (both_grip already handled the two-grip case).
            let drag = if input.left_grip > 0.5 {
                lhand.map(|(p, _)| (p, true))
            } else if input.right_grip > 0.5 {
                rhand.map(|(p, _)| (p, false))
            } else {
                None
            };
            match drag {
                Some((hp, is_left)) => {
                    if self.vr_drag_hand != Some(is_left) {
                        // New grab (or switched hands): re-anchor under this hand.
                        self.vr_grab_anchor = Some(self.vr_rig.grab_anchor(hp));
                        self.vr_drag_hand = Some(is_left);
                    }
                    if let Some(anchor) = self.vr_grab_anchor {
                        self.vr_rig.drag_to(anchor, hp);
                    }
                }
                None => {
                    self.vr_grab_anchor = None;
                    self.vr_drag_hand = None;
                }
            }
        }

        // Right-hand pointer.
        let trigger_edge = input.right_trigger && !self.vr_prev_trigger;
        self.vr_overlay_draw = None;
        self.vr_ui_pointer = None;
        self.vr_ui_pressed = false;
        if let Some((rp, rr)) = rhand {
            let ray = {
                let (wp, wr) = self.vr_rig.stage_pose_to_world((rp, rr));
                citrus_core::pointer_ray(wp, wr)
            };
            if self.vr_menu_open {
                // VR-native multi-panel UI: split the editor UI into separate 3D
                // panels (the screen regions AROUND the viewport — left/right/top/
                // bottom strips), arranged in a world-anchored arc. The viewport
                // region is omitted (you're inside it). Point with the right
                // controller; trigger = click, mapped to the hit panel's region.
                if let Some((head0, flat)) = self.vr_panel_anchor {
                    let (panels, regions) = self.vr_compute_panels(head0, flat);
                    // Right-hand pointer: hit-test each panel, map to its screen
                    // region so egui sees a click at the right place.
                    for (vpanel, r) in panels.iter().flatten().zip(regions.iter()) {
                        let ppos = vpanel.model.w_axis.truncate();
                        let pn = vpanel.model.z_axis.truncate().normalize_or(glam::Vec3::Z);
                        if let Some(hit) = citrus_core::ray_plane(ray, ppos, pn) {
                            let local = vpanel.model.inverse().transform_point3(hit);
                            if local.x.abs() <= 0.5 && local.y.abs() <= 0.5 {
                                let u = local.x + 0.5;
                                let v = local.y + 0.5;
                                let px = r.min.x + u * r.width();
                                let py = r.min.y + (1.0 - v) * r.height();
                                self.vr_ui_pointer = Some(egui::pos2(px, py));
                                self.vr_ui_pressed = input.right_trigger;
                                break;
                            }
                        }
                    }
                    self.vr_overlay_draw = Some(citrus_render::VrOverlayDraw {
                        panels,
                        // Hands + laser endpoints are filled in at the render site
                        // (lasers show even when the hand menu is down).
                        left_hand: None,
                        right_hand: None,
                        left_laser_end: None,
                        right_laser_end: None,
                    });
                }
            } else {
                // Object mode: pick on press, then drag the selection along the ray.
                if trigger_edge {
                    if let Some(idx) = self.scene.pick(ray.origin, ray.dir) {
                        self.select_object(Some(idx));
                        let obj_pos = self.scene.world_transform(idx).w_axis.truncate();
                        self.vr_move_dist = (obj_pos - ray.origin).length();
                    }
                }
                if input.right_trigger {
                    if let Selection::Object(idx) = self.selection {
                        if idx < self.scene.objects.len() {
                            let target = ray.origin + ray.dir * self.vr_move_dist;
                            let id = self.scene.objects[idx].id;
                            self.scene.set_local_transform(id, Some(target), None, None);
                        }
                    }
                }
            }
        }
        self.vr_prev_trigger = input.right_trigger;

        // Toggle the UI panel on the menu button's rising edge.
        if input.menu != self.vr_prev_menu {
            tracing::info!(
                "VR menu button = {} (controllers tracking: left={}, right={})",
                input.menu,
                lhand.is_some(),
                rhand.is_some(),
            );
        }
        if input.menu && !self.vr_prev_menu {
            self.vr_menu_open = !self.vr_menu_open;
            tracing::info!("VR hand menu {}", if self.vr_menu_open { "opened" } else { "closed" });
            if self.vr_menu_open {
                // World-anchor the panel ~1.2 m in front of the head at eye height,
                // facing the user, and leave it there until the menu closes.
                if let Some(hs) = head {
                    let (hp, hr) = self.vr_rig.stage_pose_to_world(hs);
                    let fwd = (hr * Vec3::NEG_Z).normalize_or(Vec3::NEG_Z);
                    let flat = Vec3::new(fwd.x, 0.0, fwd.z).normalize_or(fwd);
                    // Store the head point + direction; the panel transform is
                    // rebuilt each frame so its distance/size track the player scale.
                    self.vr_panel_anchor = Some((hp, flat));
                }
            } else {
                self.vr_panel_anchor = None;
            }
        }
        self.vr_prev_menu = input.menu;
    }

    /// Run a VR tool action (the in-VR egui "VR Tools" window calls these; they
    /// mirror operations that have no desktop button yet).
    fn dispatch_vr_action(&mut self, action: VrAction) {
        match action {
            VrAction::CalibrateTpose => {
                if self.scene.calibrate_vr_tpose(&self.vr_targets) {
                    tracing::info!("VR: captured T-pose calibration");
                }
            }
            VrAction::ClearCalibration => self.scene.clear_vr_calibration(),
            VrAction::ToggleFootIk => {
                let on = self.scene.foot_ik_enabled();
                self.scene
                    .set_foot_ik((!on).then(crate::humanoid::FootIkParams::default));
            }
            VrAction::ResetRig => self.vr_rig = citrus_core::VrRig::default(),
            VrAction::DeleteSelected => {
                if let Selection::Object(idx) = self.selection {
                    if idx < self.scene.objects.len() {
                        self.scene.remove_object(idx);
                        self.selection = Selection::None;
                    }
                }
            }
        }
    }

    /// A small "VR Tools" egui window (only while a VR session is active) so the
    /// VR-specific actions are reachable from the in-headset UI panel as well as
    /// the desktop. The rest of the editor UI is shown on the panel unchanged.
    fn vr_tools_window(&mut self, ctx: &egui::Context) {
        if self.xr_session.is_none() {
            return;
        }
        let mut action = None;
        let mut gi_changed = false;
        egui::Window::new("VR Tools")
            .default_open(true)
            .show(ctx, |ui| {
                ui.label(format!("Calibrated: {}", self.scene.has_vr_calibration()));
                ui.label(format!("Foot IK: {}", self.scene.foot_ik_enabled()));
                ui.label(format!("Player scale: {:.2}x", self.vr_rig.scale));
                ui.separator();
                // Rendering / performance toggles. Realtime GI also lights the VR
                // view (via the world-probe volume); reflections + quality affect
                // cost. Any change re-triggers the GI trace.
                ui.label("Rendering");
                let gi = &mut self.scene.environment.realtime_gi;
                gi_changed |= ui.checkbox(&mut gi.enabled, "Realtime GI").changed();
                let refl = match gi.reflection_mode {
                    0 => "Reflections: Off",
                    1 => "Reflections: SSR",
                    _ => "Reflections: RT",
                };
                if ui.button(refl).clicked() {
                    gi.reflection_mode = (gi.reflection_mode + 1) % 3;
                    gi_changed = true;
                }
                if ui
                    .button(format!("GI Quality: {}", gi.quality.label()))
                    .clicked()
                {
                    use citrus_assets::FluxQuality::*;
                    gi.quality = match gi.quality {
                        Performance => Balanced,
                        Balanced => High,
                        High => Ultra,
                        Ultra => Performance,
                    };
                    gi_changed = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Bounces");
                    if ui.button("−").clicked() && gi.bounces > 1 {
                        gi.bounces -= 1;
                        gi_changed = true;
                    }
                    ui.label(format!("{}", gi.bounces));
                    if ui.button("+").clicked() && gi.bounces < 4 {
                        gi.bounces += 1;
                        gi_changed = true;
                    }
                });
                ui.separator();
                for (label, act) in VR_TOOLS {
                    if ui.button(*label).clicked() {
                        action = Some(*act);
                    }
                }
            });
        if gi_changed {
            self.rt_gi.invalidate();
        }
        if let Some(a) = action {
            self.dispatch_vr_action(a);
        }
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
                    | EditorAction::SpawnPostFxVolume
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
                    // Model file → load its .meta so import options show in the
                    // Inspector (creating the sidecar on first view).
                    self.file_meta = None;
                    let is_model = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fbx" | "gltf" | "glb" | "obj"))
                        .unwrap_or(false);
                    if is_model {
                        match citrus_assets::load_or_create_asset_meta(&path) {
                            Ok(meta) => {
                                self.file_meta = Some(FileMeta {
                                    path: path.clone(),
                                    meta,
                                    dirty: false,
                                })
                            }
                            Err(e) => tracing::error!("loading .meta: {e:#}"),
                        }
                    }
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
                                model.textures = crate::scene::tex_paths_from_file(&file.textures);
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
                EditorAction::ImportModel(path) => {
                    // Parse the model on a worker thread; the GPU upload happens
                    // on the main thread in `apply_imported_model` when it lands.
                    let rel = self.project_root.join(crate::scene::relative_to(&path, &self.project_root));
                    let source = PathBuf::from(crate::scene::relative_to(&rel, &self.project_root));
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "model".into());
                    self.tasks.spawn(
                        format!("Import {name}"),
                        crate::tasks::TaskKind::ImportModel,
                        false,
                        move |cancel, _progress| {
                            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                                return Ok(crate::tasks::TaskPayload::None);
                            }
                            let scene = citrus_assets::load_model_with_meta(&path)
                                .map_err(|e| e.to_string())?;
                            Ok(crate::tasks::TaskPayload::Model {
                                source,
                                scene: Box::new(scene),
                            })
                        },
                    );
                }
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
                EditorAction::RecaptureReflections => {
                    // Realtime probe refresh: re-render the active probe's cubemap
                    // from the current scene (same path as the on-load capture).
                    let probe = self
                        .scene
                        .active_reflection_probe(glam::Vec3::ZERO)
                        .map(|p| (glam::Vec3::from(p.center), p.resolution));
                    match (probe, self.renderer.as_mut()) {
                        (Some((center, res)), Some(r)) => {
                            r.request_reflection_capture(center, res);
                            self.set_status("Recapturing reflection probes…", false);
                        }
                        _ => self.set_status(
                            "No Reflection Probe in the scene — add one to an object first.",
                            false,
                        ),
                    }
                }
                EditorAction::BakeReflections => {
                    let probe = self
                        .scene
                        .active_reflection_probe(glam::Vec3::ZERO)
                        .map(|p| (glam::Vec3::from(p.center), p.resolution));
                    match (probe, self.renderer.as_mut()) {
                        (Some((center, res)), Some(r)) => {
                            r.request_reflection_capture(center, res);
                            // Capture lands next render frame; read it back after.
                            self.refl_bake_pending = Some(2);
                            self.set_status("Baking reflection probe…", true);
                        }
                        _ => self.set_status(
                            "No Reflection Probe in the scene — add one to an object first.",
                            false,
                        ),
                    }
                }
                EditorAction::PickAt(pos) => {
                    // Clicking the 3D viewport drops any lingering code-editor
                    // text focus, so keyboard shortcuts (Ctrl+Z/Y) route to the
                    // editor again instead of egui's TextEdit consuming them.
                    self.egui_ctx.memory_mut(|m| m.stop_text_input());
                    if let Some((origin, dir)) = self.cursor_ray(pos) {
                        let hit = self.scene.pick(origin, dir);
                        // Ctrl/Shift click adds/toggles into the multi-selection
                        // (like the file explorer); plain click is single-select.
                        let additive = self.egui_ctx.input(|i| {
                            i.modifiers.command || i.modifiers.shift
                        });
                        match (hit, additive) {
                            (Some(idx), true) => {
                                if let Some(p) =
                                    self.multi_objects.iter().position(|&x| x == idx)
                                {
                                    if self.multi_objects.len() > 1 {
                                        self.multi_objects.remove(p);
                                        if self.selection == Selection::Object(idx) {
                                            self.selection = Selection::Object(
                                                self.multi_objects[p.min(
                                                    self.multi_objects.len() - 1,
                                                )],
                                            );
                                        }
                                    }
                                } else {
                                    self.multi_objects.push(idx);
                                    self.selection = Selection::Object(idx);
                                }
                            }
                            (hit, _) => self.select_object(hit),
                        }
                    }
                }
                EditorAction::AssignMaterialAt(pos, path) => {
                    if let Some((origin, dir)) = self.cursor_ray(pos)
                        && let Some(hit) = self.scene.pick(origin, dir)
                    {
                        // Viewport drops target the primary slot (slot 0).
                        let Some(before) = self.scene.objects[hit].render.map(|r| r.material)
                        else {
                            continue;
                        };
                        self.scene.assign_material(
                            renderer!(),
                            &mut self.shaders,
                            hit,
                            0,
                            &path,
                            &self.project_root,
                        );
                        let after = self.scene.objects[hit]
                            .render
                            .map(|r| r.material)
                            .unwrap_or(before);
                        if before != after {
                            self.undo_stack.record(
                                UndoEntry::Assign {
                                    object: hit,
                                    slot: 0,
                                    before,
                                    after,
                                },
                                false,
                            );
                        }
                        // A reassigned material may change emission/albedo.
                        self.rt_gi.invalidate();
                    }
                }
                EditorAction::AssignMaterialToObject(object, slot, path) => {
                    let Some(before) = self.scene.slot_material(object, slot) else {
                        continue;
                    };
                    self.scene.assign_material(
                        renderer!(),
                        &mut self.shaders,
                        object,
                        slot,
                        &path,
                        &self.project_root,
                    );
                    let after = self.scene.slot_material(object, slot).unwrap_or(before);
                    if before != after {
                        self.undo_stack.record(
                            UndoEntry::Assign {
                                object,
                                slot,
                                before,
                                after,
                            },
                            false,
                        );
                    }
                    self.rt_gi.invalidate();
                }
                EditorAction::MaterialEdited(index) => {
                    self.scene.apply_material(
                        renderer!(),
                        &mut self.shaders,
                        &self.project_root,
                        index,
                    );
                    // Emission/albedo may have changed; re-trace bounce light.
                    self.rt_gi.invalidate();
                }
                EditorAction::AssignTexture {
                    material,
                    slot,
                    path,
                } => {
                    if material < self.scene.materials.len() {
                        let rel = PathBuf::from(relative_to(&path, &self.project_root));
                        if let Some(s) =
                            self.scene.materials[material].model.textures.slot_mut(slot)
                        {
                            *s = Some(rel);
                        }
                        self.scene.apply_material(
                            renderer!(),
                            &mut self.shaders,
                            &self.project_root,
                            material,
                        );
                        // A new texture (albedo/emission/etc.) can change bounce light.
                        self.rt_gi.invalidate();
                        // Persist to the backing `.material` file if there is one.
                        self.dirty_materials.insert(material);
                    }
                }
                EditorAction::AssignFileTexture { slot, path } => {
                    if let Some(fm) = self.file_material.as_mut() {
                        let rel = PathBuf::from(relative_to(&path, &self.project_root));
                        if let Some(s) = fm.model.textures.slot_mut(slot) {
                            *s = Some(rel);
                        }
                        fm.dirty = true;
                        self.actions
                            .push(EditorAction::FileMaterialEdited(fm.path.clone()));
                    }
                }
                EditorAction::ResetMaterial(index) => {
                    self.scene.materials[index].model = self.scene.materials[index].default.clone();
                    self.scene.apply_material(
                        renderer!(),
                        &mut self.shaders,
                        &self.project_root,
                        index,
                    );
                    self.rt_gi.invalidate();
                }
                EditorAction::ExtractMaterial(index) => {
                    self.extract_material(index);
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
                    self.rt_gi.invalidate();
                }
                EditorAction::SaveFileMaterial => {
                    if let Some(fm) = &mut self.file_material {
                        let (params, features) = material_from_model(&fm.model);
                        fm.file.params = params;
                        fm.file.features = features;
                        fm.file.shader = fm.model.shader.clone();
                        fm.file.name = fm.model.name.clone();
                        fm.file.render_queue = Some(fm.model.render_queue);
                        fm.file.textures = crate::scene::tex_file_from_paths(&fm.model.textures);
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
                EditorAction::SaveFileMeta => {
                    if let Some(fm) = &mut self.file_meta {
                        match citrus_assets::save_asset_meta(&fm.path, &fm.meta) {
                            Ok(()) => fm.dirty = false,
                            Err(e) => tracing::error!("saving .meta: {e:#}"),
                        }
                    }
                }
                EditorAction::ReimportModel(path) => {
                    if let Some(fm) = &mut self.file_meta
                        && fm.path == path
                    {
                        if let Err(e) = citrus_assets::save_asset_meta(&fm.path, &fm.meta) {
                            tracing::error!("saving .meta: {e:#}");
                        }
                        fm.dirty = false;
                    }
                    // Reload the scene so instances of this model reimport with the
                    // new settings (skips while playing / with unsaved scene edits).
                    if self.playing {
                        self.set_status("Stop play mode to reimport", false);
                    } else if self.scene_dirty {
                        self.set_status("Save the scene first to reimport", false);
                    } else if let Some(scene) = self.current_scene_path.clone() {
                        self.load_scene_runtime(&scene);
                        self.set_status("Reimported model", false);
                    }
                }
                EditorAction::ExtractModelAssets(path) => {
                    self.extract_model_assets(&path);
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
                        Ok(index) => self.select_object(Some(index)),
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
                            self.select_object(Some(index));
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
                            self.select_object(Some(index));
                        }
                        Err(e) => tracing::error!("spawning probe volume: {e:#}"),
                    }
                }
                EditorAction::SpawnReflectionProbe => {
                    let mut p = self.camera.position + self.camera.forward() * 4.0;
                    p.y = p.y.max(1.5);
                    match self.scene.spawn(
                        renderer!(),
                        citrus_assets::ObjectSource::Empty,
                        "Reflection Probe".to_owned(),
                        p,
                    ) {
                        Ok(index) => {
                            self.scene.objects[index]
                                .components
                                .push(Box::new(citrus_core::ReflectionProbe::default()));
                            self.select_object(Some(index));
                        }
                        Err(e) => tracing::error!("spawning reflection probe: {e:#}"),
                    }
                }
                EditorAction::SpawnFluxVolume => {
                    let mut p = self.camera.position + self.camera.forward() * 4.0;
                    p.y = p.y.max(1.5);
                    match self.scene.spawn(
                        renderer!(),
                        citrus_assets::ObjectSource::Empty,
                        "Flux Volume".to_owned(),
                        p,
                    ) {
                        Ok(index) => {
                            self.scene.objects[index]
                                .components
                                .push(Box::new(citrus_core::FluxVolume::default()));
                            self.select_object(Some(index));
                        }
                        Err(e) => tracing::error!("spawning flux volume: {e:#}"),
                    }
                }
                EditorAction::SpawnPostFxVolume => {
                    let p = self.camera.position + self.camera.forward() * 4.0;
                    match self.scene.spawn(
                        renderer!(),
                        citrus_assets::ObjectSource::Empty,
                        "Post FX Volume".to_owned(),
                        p,
                    ) {
                        Ok(index) => {
                            self.scene.objects[index]
                                .components
                                .push(Box::new(citrus_core::VolumeComponent::default()));
                            self.select_object(Some(index));
                        }
                        Err(e) => tracing::error!("spawning post fx volume: {e:#}"),
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
                            self.select_object(Some(index));
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
                            self.select_object(Some(index));
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
                            self.select_object(Some(index));
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
                EditorAction::SetSkyboxFace(i, path) => {
                    let rel = path
                        .strip_prefix(&self.project_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    let faces = self
                        .scene
                        .environment
                        .skybox_faces
                        .get_or_insert_with(|| std::array::from_fn(|_| String::new()));
                    if i < 6 {
                        faces[i] = rel;
                    }
                    self.apply_skybox();
                }
                EditorAction::ClearSkyboxFaces => {
                    self.scene.environment.skybox_faces = None;
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
                    // Defer the blocking cargo build one frame so the busy overlay
                    // paints "Compiling components…" first.
                    self.set_status("Compiling components…", true);
                    self.pending_job = Some(PendingJob::ReloadPlugins);
                }
                EditorAction::BakeLighting => self.start_bake(),
                EditorAction::ClearBake => {
                    self.scene.baked = None;
                    self.upload_baked_probes();
                    tracing::info!("cleared baked lighting");
                }
                EditorAction::RegenerateLightmapUvs => {
                    // Markers already exist; reloading re-imports each model and
                    // re-runs the (current) unwrapper.
                    if self.playing {
                        self.set_status("Stop play mode to regenerate UVs", false);
                    } else if self.scene_dirty {
                        self.set_status("Save the scene first to regenerate UVs", false);
                    } else if let Some(scene) = self.current_scene_path.clone() {
                        self.load_scene_runtime(&scene);
                        self.set_status("Regenerated lightmap UVs", false);
                    }
                }
                EditorAction::GenerateAllLightmapUvs => {
                    let models = self.scene.models_needing_lightmap_uv();
                    for m in &models {
                        if let Err(e) =
                            citrus_assets::set_lightmap_uv_marker(self.project_root.join(m), true)
                        {
                            tracing::error!("writing lightmap-UV marker for {m}: {e:#}");
                        }
                    }
                    if self.playing {
                        self.set_status("Stop play mode, then reload", false);
                    } else if self.scene_dirty {
                        self.set_status("Save the scene first, then reload", false);
                    } else if let Some(scene) = self.current_scene_path.clone() {
                        self.load_scene_runtime(&scene);
                        self.set_status(
                            format!("Generated lightmap UVs for {} model(s)", models.len()),
                            false,
                        );
                    }
                }
                EditorAction::SetLightmapUvGen(path, on) => {
                    if let Err(e) =
                        citrus_assets::set_lightmap_uv_marker(self.project_root.join(&path), on)
                    {
                        tracing::error!("setting lightmap-UV marker for {path}: {e:#}");
                    }
                    if self.playing {
                        self.set_status("Stop play mode, then reload", false);
                    } else if self.scene_dirty {
                        self.set_status("Save the scene first, then reload", false);
                    } else if let Some(scene) = self.current_scene_path.clone() {
                        self.load_scene_runtime(&scene);
                        self.set_status(
                            if on { "Generated lightmap UVs" } else { "Reverted to model UVs" },
                            false,
                        );
                    }
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
                    // Materials are NOT rewritten on scene save; only manual
                    // edits persist (tracked in `dirty_materials`, flushed by
                    // `autosave_materials`, or saved via the material editor).
                    // A material with an existing `.material` file serializes as
                    // a file reference; one without stays inline in the scene.
                    let mut file = self.scene.to_scene_file(&self.project_root, &self.shaders);
                    // Persist the current editor viewpoint so the scene reopens
                    // framed the same way.
                    file.editor_camera = Some(citrus_assets::EditorCamera {
                        position: self.camera.position.to_array(),
                        yaw: self.camera.yaw,
                        pitch: self.camera.pitch,
                    });
                    // Persist the scene-tree collapsed state (by stable object id)
                    // so the hierarchy reopens with the same rows expanded.
                    file.collapsed = self
                        .scene_panel
                        .collapsed_indices()
                        .filter_map(|i| self.scene.objects.get(i).map(|o| o.id.to_string()))
                        .collect();
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
                    // Create a reusable .prefab from the selected object subtree
                    // (CHECKLIST T0 #7). Saved next to the scene; instantiate later
                    // via `Prefab::load(..).instantiate(pos)`.
                    if let Selection::Object(i) = self.selection {
                        if ui.button("Create Prefab from Selection").clicked() {
                            if let Some(prefab) =
                                self.scene.prefab_from_object(i, &self.project_root, &self.shaders)
                            {
                                let name = self.scene.objects[i].name.replace(' ', "_");
                                let path = self.project_root.join(format!("{name}.prefab"));
                                match prefab.save(&path) {
                                    Ok(_) => self.set_load_status(&format!("Saved prefab {}", path.display())),
                                    Err(e) => self.set_load_status(&format!("Prefab save failed: {e}")),
                                }
                            }
                            ui.close();
                        }
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
                    if ui.button("Input Bindings…").clicked() {
                        self.show_bindings = true;
                        ui.close();
                    }
                    if ui.button("Network…").clicked() {
                        self.show_network = true;
                        ui.close();
                    }
                    if ui.button("Layers…").clicked() {
                        self.show_layers = true;
                        ui.close();
                    }
                    if ui.button("Audio Mixer…").clicked() {
                        self.show_mixer = true;
                        ui.close();
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
                        ui.checkbox(&mut self.show_stats_overlay, "Profiler window")
                            .on_hover_text(
                                "Open the profiler in a separate window you can move to \
                                 another monitor",
                            );
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
                        ui.separator();
                        ui.label(egui::RichText::new("GI Debug View").small().weak());
                        ui.radio_value(&mut self.gi_debug, 0, "Off (normal shading)");
                        ui.radio_value(&mut self.gi_debug, 1, "World normals")
                            .on_hover_text("Verify the surface normals feeding GI");
                        ui.radio_value(&mut self.gi_debug, 2, "Indirect GI only")
                            .on_hover_text("Isolate the Flux indirect bounce");
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
                        (Tab::Baker, "FluxBaker"),
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

    /// True when the active/focused dock tab is a code editor. Used to keep
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
                // FluxVoxel voxel volumes are owned by `realtime_gi::update_flux_voxel`
                // (it seeds its static base from the baked SH, then adds dynamic
                // lights live), so skip them here — only plain DDGI volumes are
                // uploaded as static baked probes.
                let vols: Vec<_> = b
                    .probe_volumes
                    .iter()
                    .filter(|v| !v.flux_voxel)
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
    /// Start a chunked lighting bake (one unit/frame; see `step_bake`). No-op if
    /// one is already running or the GPU can't ray trace.
    fn start_bake(&mut self) {
        // Before baking, check for static models that have no lightmap UV (uv1 is
        // a uv0 fallback → would bake garbage, so the bake skips them). If any,
        // ask the user whether to generate a non-overlapping unwrap first.
        let needs = self.scene.models_needing_lightmap_uv();
        if !needs.is_empty() {
            self.pending_lm_uv_models = Some(needs);
            return; // the modal (lightmap_uv_window) drives what happens next
        }
        self.do_bake();
    }

    fn do_bake(&mut self) {
        use crate::tasks::{BakePhase, NotifyLevel, TaskKind, TaskProgress};
        if self.bake_job.is_some() {
            self.tasks.notify("Bake already running", NotifyLevel::Info);
            return;
        }
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        if !renderer.supports_baking() {
            self.tasks
                .notify("This GPU can't ray trace; bake unavailable", NotifyLevel::Warn);
            return;
        }
        let gather = self.scene.gather_bake();
        if gather.instances.is_empty() && gather.probes.is_empty() {
            self.tasks.notify(
                "Nothing to bake — mark objects Static and/or add a Light Probe Volume",
                NotifyLevel::Warn,
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
            gpu_idle_frac: settings.gpu_throttle.clamp(0.0, 4.0),
        };
        let job = match renderer.bake_begin(&input) {
            Ok(j) => j,
            Err(e) => {
                self.tasks
                    .notify(format!("Bake failed: {e}"), NotifyLevel::Warn);
                return;
            }
        };
        // FluxVoxel volume count for the progress display (peek; recomputed when the
        // main pass completes).
        let flux_voxel_volumes = self
            .scene
            .gather_flux_voxel_bake()
            .map(|f| f.probe_volumes.len() as u32)
            .unwrap_or(0);
        let (task_id, cancel, progress) = self.tasks.register_stepped(
            "Bake lighting",
            TaskKind::Bake,
            TaskProgress::Bake {
                lights: gather.lights.len() as u32,
                bounces: settings.bounces,
                lightmap_done: 0,
                lightmap_total: gather.instances.len() as u32,
                probe_volumes: gather.probe_volumes.len() as u32,
                flux_voxel_volumes,
                phase: BakePhase::Lightmaps,
            },
        );
        let mut object_lightmap = std::collections::HashMap::new();
        for (layer, &obj) in gather.instance_objects.iter().enumerate() {
            object_lightmap.insert(obj, layer);
        }
        self.bake_job = Some(BakeRunner {
            job,
            task_id,
            cancel,
            progress,
            phase: BakeRunPhase::Main,
            settings,
            object_lightmap,
            probe_volumes: gather.probe_volumes,
            baked: None,
        });
        // Live preview: switch to baked mode with no lightmaps yet, so the
        // viewport darkens immediately and then lights up object-by-object as
        // each lightmap finishes (see `step_bake`).
        self.scene.baked = Some(scene::BakedData::default());
        self.upload_baked_probes();
        // Paint the bake modal once before the first lightmap step locks the UI.
        self.bake_pending_warmup = true;
    }

    /// Advance the active bake by one unit per frame; handles cancel, progress,
    /// phase transition (main → FluxVoxel), and finalization.
    fn step_bake(&mut self) {
        use crate::tasks::{BakePhase, NotifyLevel, TaskProgress};
        if self.bake_job.is_none() {
            return;
        }
        // Cancel: tear down the GPU job, discard the partial result.
        if self
            .bake_job
            .as_ref()
            .unwrap()
            .cancel
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let runner = self.bake_job.take().unwrap();
            if let Some(r) = self.renderer.as_ref() {
                let _ = r.bake_finish(runner.job);
            }
            self.tasks.complete_stepped(runner.task_id);
            self.tasks.notify("Bake cancelled", NotifyLevel::Info);
            return;
        }
        let step = {
            let runner = self.bake_job.as_mut().unwrap();
            match self.renderer.as_ref() {
                Some(r) => r.bake_step(&mut runner.job),
                None => return,
            }
        };
        match step {
            Err(e) => {
                tracing::error!("bake step: {e:#}");
                let runner = self.bake_job.take().unwrap();
                if let Some(r) = self.renderer.as_ref() {
                    let _ = r.bake_finish(runner.job);
                }
                self.tasks.complete_stepped(runner.task_id);
                self.tasks
                    .notify(format!("Bake failed: {e}"), NotifyLevel::Warn);
            }
            Ok(citrus_render::BakeStep::Lightmap { done, .. }) => {
                // Snapshot the partial result, then drop the bake_job borrow so we
                // can apply it to the scene (live preview).
                let preview = {
                    let runner = self.bake_job.as_mut().unwrap();
                    if let Ok(mut p) = runner.progress.lock() {
                        if let TaskProgress::Bake {
                            lightmap_done,
                            phase,
                            ..
                        } = &mut *p
                        {
                            *lightmap_done = done as u32;
                            *phase = BakePhase::Lightmaps;
                        }
                    }
                    // Only the main lightmap pass produces lightmaps to preview;
                    // the FluxVoxel pass is probes-only.
                    if runner.phase == BakeRunPhase::Main {
                        let lightmaps = runner.job.lightmaps_so_far().to_vec();
                        let n = lightmaps.len();
                        // Map only objects whose layer is already uploaded (others
                        // stay dark until their lightmap finishes).
                        let object_lightmap = runner
                            .object_lightmap
                            .iter()
                            .filter(|&(_, &layer)| layer < n)
                            .map(|(&o, &l)| (o, l))
                            .collect();
                        Some(scene::BakedData {
                            object_lightmap,
                            lightmaps,
                            probe_volumes: Vec::new(),
                            probe_sh: Vec::new(),
                        })
                    } else {
                        None
                    }
                };
                if let Some(baked) = preview {
                    self.scene.baked = Some(baked);
                    self.upload_baked_probes();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Ok(citrus_render::BakeStep::Probes) => {
                let runner = self.bake_job.as_mut().unwrap();
                if let Ok(mut p) = runner.progress.lock() {
                    if let TaskProgress::Bake { phase, .. } = &mut *p {
                        *phase = match runner.phase {
                            BakeRunPhase::FluxVoxel => BakePhase::FluxVoxel,
                            BakeRunPhase::Main => BakePhase::Probes,
                        };
                    }
                }
            }
            Ok(citrus_render::BakeStep::Complete) => self.bake_phase_complete(),
        }
    }

    /// A bake pass finished: collect its output, advance main → FluxVoxel, or
    /// finalize when the FluxVoxel pass is done.
    fn bake_phase_complete(&mut self) {
        use crate::tasks::{BakePhase, TaskProgress};
        let mut runner = match self.bake_job.take() {
            Some(r) => r,
            None => return,
        };
        let output = match self.renderer.as_ref() {
            Some(r) => r.bake_finish(runner.job),
            None => return,
        };
        match runner.phase {
            BakeRunPhase::Main => {
                let baked = scene::BakedData {
                    object_lightmap: std::mem::take(&mut runner.object_lightmap),
                    lightmaps: output.lightmaps,
                    probe_volumes: std::mem::take(&mut runner.probe_volumes),
                    probe_sh: output.probes,
                };
                // Kick the FluxVoxel voxel pass if any FluxVolumes exist; else done.
                if let Some(fg) = self.scene.gather_flux_voxel_bake() {
                    let fin = citrus_render::BakeInput {
                        instances: &fg.instances,
                        lights: &fg.lights,
                        probes: &fg.probes,
                        sky_color: fg.sky_color,
                        bounces: runner.settings.bounces,
                        samples: runner.settings.samples,
                        probes_only: true,
                        gpu_idle_frac: runner.settings.gpu_throttle.clamp(0.0, 4.0),
                    };
                    match self.renderer.as_ref().map(|r| r.bake_begin(&fin)) {
                        Some(Ok(job)) => {
                            if let Ok(mut p) = runner.progress.lock() {
                                if let TaskProgress::Bake { phase, .. } = &mut *p {
                                    *phase = BakePhase::FluxVoxel;
                                }
                            }
                            self.bake_job = Some(BakeRunner {
                                job,
                                task_id: runner.task_id,
                                cancel: runner.cancel,
                                progress: runner.progress,
                                phase: BakeRunPhase::FluxVoxel,
                                settings: runner.settings,
                                object_lightmap: std::collections::HashMap::new(),
                                probe_volumes: fg.probe_volumes,
                                baked: Some(baked),
                            });
                            return;
                        }
                        _ => {
                            tracing::warn!("FluxVoxel bake begin failed; finalizing main bake only");
                        }
                    }
                }
                self.finalize_bake(baked, runner.task_id);
            }
            BakeRunPhase::FluxVoxel => {
                let mut baked = runner.baked.take().unwrap_or_else(|| scene::BakedData {
                    object_lightmap: std::collections::HashMap::new(),
                    lightmaps: Vec::new(),
                    probe_volumes: Vec::new(),
                    probe_sh: Vec::new(),
                });
                let offset = baked.probe_sh.len();
                for mut v in std::mem::take(&mut runner.probe_volumes) {
                    v.sh_base += offset;
                    baked.probe_volumes.push(v);
                }
                baked.probe_sh.extend(output.probes);
                self.finalize_bake(baked, runner.task_id);
            }
        }
    }

    /// Install a finished bake: store on the scene, reseed FluxVoxel, upload, save,
    /// clear the task, and flush any imports deferred during the bake.
    fn finalize_bake(&mut self, baked: scene::BakedData, task_id: crate::tasks::TaskId) {
        let n = baked.lightmaps.len();
        self.scene.baked = Some(baked);
        self.rt_gi.invalidate_flux_voxel();
        self.upload_baked_probes();
        self.save_bake();
        self.tasks.complete_stepped(task_id);
        self.tasks
            .notify(format!("Bake complete: {n} lightmaps"), crate::tasks::NotifyLevel::Info);
        // Apply model imports that finished while the bake held the meshes.
        for (source, asset) in std::mem::take(&mut self.deferred_model_applies) {
            self.apply_imported_model(source, asset);
        }
    }

    /// Realtime-GI preview: while enabled (and the scene isn't baked), re-trace
    /// the auto probe grid from the realtime lights every ~0.2s, blend toward
    /// the previous result (temporal smoothing), and upload so un-baked surfaces
    /// show live indirect bounce. Reuses the bake path tracer (`probes_only`).
    fn update_realtime_gi(&mut self, dt: f32) {
        let vr_active = self.xr_session.is_some();
        if let Some(renderer) = self.renderer.as_mut() {
            self.rt_gi.update(renderer, &mut self.scene, dt, vr_active);
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

    /// Read the captured reflection cube back from the GPU and persist its 6 faces
    /// to `<scene>.reflprobe` (a small RGBA8 sidecar: magic, size, 6×size²×4 bytes).
    /// A baked reflection probe — loaded on scene open so the cube survives reloads
    /// without re-rendering it. Failures are logged, never fatal.
    /// Handle the inspector's "Bake This Probe" button: find a Reflection Probe
    /// whose transient `bake_now` flag is set, capture from its centre at its
    /// resolution, and queue the readback/save (same path as Bake Reflections).
    fn poll_probe_bake_requests(&mut self) {
        if self.refl_bake_pending.is_some() {
            return; // a bake is already in flight
        }
        let mut req: Option<(usize, u32)> = None;
        for (i, obj) in self.scene.objects.iter_mut().enumerate() {
            for c in obj.components.iter_mut() {
                if let Some(p) = c.as_any_mut().downcast_mut::<citrus_core::ReflectionProbe>()
                    && p.bake_now
                {
                    p.bake_now = false;
                    req = Some((i, p.resolution));
                    break;
                }
            }
            if req.is_some() {
                break;
            }
        }
        let Some((i, res)) = req else { return };
        let center = self.scene.world_transform(i).w_axis.truncate();
        if let Some(r) = self.renderer.as_mut() {
            r.request_reflection_capture(center, res);
            self.refl_bake_pending = Some(2);
            self.set_status("Baking reflection probe…", true);
        }
    }

    fn save_reflection_bake(&mut self) {
        let Some(faces) = self.renderer.as_ref().and_then(|r| r.read_reflection_faces()) else {
            self.set_status("Reflection bake failed (readback)", false);
            return;
        };
        if faces.len() != 6 {
            return;
        }
        let size = faces[0].width;
        let mut out = Vec::with_capacity(16 + faces.iter().map(|f| f.pixels.len()).sum::<usize>());
        out.extend_from_slice(b"CITRSRP1");
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(faces.len() as u32).to_le_bytes());
        for f in &faces {
            out.extend_from_slice(&f.pixels);
        }
        let path = self.bake_base_path().with_extension("reflprobe");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, &out) {
            Ok(()) => self.set_status("Baked reflection probe", false),
            Err(e) => {
                tracing::error!("saving .reflprobe: {e:#}");
                self.set_status("Reflection bake failed (write)", false);
            }
        }
    }

    /// Load a `<scene>.reflprobe` sidecar (if present) and upload it as the
    /// reflection env, skipping the load-time scene recapture. Missing = no-op.
    fn load_reflection_bake(&mut self) -> bool {
        let path = self.bake_base_path().with_extension("reflprobe");
        let Ok(bytes) = std::fs::read(&path) else {
            return false;
        };
        if bytes.len() < 16 || &bytes[..8] != b"CITRSRP1" {
            return false;
        }
        let size = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let count = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
        let face_len = (size * size * 4) as usize;
        if count != 6 || bytes.len() < 16 + face_len * 6 {
            return false;
        }
        let mut faces: Vec<citrus_render::TextureData> = Vec::with_capacity(6);
        for i in 0..6 {
            let start = 16 + i * face_len;
            faces.push(citrus_render::TextureData {
                width: size,
                height: size,
                pixels: bytes[start..start + face_len].to_vec(),
                srgb: false,
                hdr: false,
            });
        }
        let arr: [citrus_render::TextureData; 6] = match faces.try_into() {
            Ok(a) => a,
            Err(_) => return false,
        };
        match self.renderer.as_mut() {
            Some(r) => match r.set_reflection_faces(&arr) {
                Ok(()) => {
                    tracing::info!("loaded baked reflection probe ({size}px)");
                    true
                }
                Err(e) => {
                    tracing::error!("uploading baked reflection probe: {e:#}");
                    false
                }
            },
            None => false,
        }
    }

    /// Write the baked lightmaps (`.lightmap`) and probe data (`.lightdata`).
    fn save_bake(&self) {
        let Some(baked) = &self.scene.baked else {
            return;
        };
        let base = self.bake_base_path();

        // .lightmap: static GI, one entry per lit object.
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

        // .lightdata: probe volumes + SH for dynamic objects.
        let ld = citrus_assets::LightDataFile {
            volumes: baked
                .probe_volumes
                .iter()
                .map(|v| citrus_assets::ProbeVolumeData {
                    world_to_local: v.world_to_local.to_cols_array(),
                    size: v.size,
                    counts: [v.counts[0] as u32, v.counts[1] as u32, v.counts[2] as u32],
                    sh_base: v.sh_base as u32,
                    flux_voxel: v.flux_voxel,
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
            // Pawn camera possession is play-only; revert to the default camera.
            self.scene.active_camera_override = None;
            if let Some(audio) = self.audio.as_mut() {
                audio.stop_all();
            }
            if self.play_scene_switched {
                // A component switched scenes during play; the snapshot indices
                // no longer match the loaded scene. Return to the scene we
                // started from (reloaded from disk; unsaved pre-play edits are
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
            self.play_time = 0.0;
            self.physics = Some(physics::PhysicsWorld::build(&self.scene));
            let mut commands = Vec::new();
            let net_view = self.net.as_mut().map(|n| n.view()).unwrap_or_default();
            self.scene
                .start_components(self.play_time, self.input.state(), &net_view, &mut commands);
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
        let mut load = None;
        for c in commands {
            match c {
                ComponentCommand::LoadScene(rel) => load = Some(rel),
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
                    if let Some(r) = self.renderer.as_mut() {
                        r.resize();
                    }
                }
                ComponentCommand::SetVsync(on) => {
                    if let Some(r) = self.renderer.as_mut() {
                        r.set_vsync(on);
                    }
                }
                ComponentCommand::SetShadowResolution(res) => {
                    if let Some(r) = self.renderer.as_mut()
                        && let Err(e) = r.set_shadow_resolution(res.clamp(256, 8192))
                    {
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
            let net_view = self.net.as_mut().map(|n| n.view()).unwrap_or_default();
            self.scene
                .start_components(self.play_time, self.input.state(), &net_view, &mut commands);
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
    /// Input Bindings window (2C): pick the active control scheme, view/clear
    /// each action's bindings, and rebind by capturing the next key/mouse press.
    /// Layers settings (Unity-style): edit the 32 layer names, the symmetric
    /// collision matrix (which layers collide in physics), and toggle per-layer
    /// viewport visibility (the editor's render culling). Stored in the scene.
    fn layers_window(&mut self, ctx: &egui::Context) {
        use citrus_core::NUM_LAYERS;
        let mut open = self.show_layers;
        egui::Window::new("Layers")
            .open(&mut open)
            .resizable(true)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Layer names, the physics collision matrix, and viewport visibility. \
                         Assign an object's layer in the Inspector. Saved with the scene.",
                    )
                    .weak()
                    .small(),
                );
                ui.separator();
                // Highest layer index actually in use (+a couple spare rows), so
                // the matrix stays compact instead of always showing all 32.
                // Consistent with the object layer dropdown + camera culling mask.
                // The matrix can name beyond `shown` to reveal more layers (naming a
                // higher layer raises shown_count everywhere).
                let max_used = self
                    .scene
                    .objects
                    .iter()
                    .map(|o| o.layer as usize)
                    .max()
                    .unwrap_or(0);
                let shown = self.scene.layers.shown_count().max(max_used + 1);

                egui::ScrollArea::both().max_height(440.0).show(ui, |ui| {
                    ui.heading("Names & visibility");
                    egui::Grid::new("layer-names").striped(true).show(ui, |ui| {
                        ui.label("#");
                        ui.label("Name");
                        ui.label("Visible");
                        ui.end_row();
                        // The NAMES list shows ALL layers (scrollable) so you can name
                        // any of the 32; naming a higher one raises `shown_count`, which
                        // reveals it in the matrix + camera mask + object dropdown.
                        for l in 0..NUM_LAYERS {
                            ui.label(format!("{l}"));
                            let name = self
                                .scene
                                .layers
                                .names
                                .get_mut(l)
                                .map(|s| s as &mut String);
                            if let Some(name) = name {
                                // Layer 0 ("Default") keeps its name fixed.
                                ui.add_enabled(l != 0, egui::TextEdit::singleline(name));
                            } else {
                                ui.label("-");
                            }
                            let bit = 1u32 << (l as u32 & 31);
                            let mut vis = self.scene.visible_layers & bit != 0;
                            if ui.checkbox(&mut vis, "").changed() {
                                if vis {
                                    self.scene.visible_layers |= bit;
                                } else {
                                    self.scene.visible_layers &= !bit;
                                }
                            }
                            ui.end_row();
                        }
                    });

                    ui.separator();
                    ui.heading("Collision matrix");
                    ui.label(
                        egui::RichText::new("Checked = the two layers collide (physics).")
                            .weak()
                            .small(),
                    );
                    egui::Grid::new("layer-matrix").striped(true).show(ui, |ui| {
                        // Header row: blank corner + column labels.
                        ui.label("");
                        for c in 0..shown {
                            ui.label(egui::RichText::new(format!("{c}")).small());
                        }
                        ui.end_row();
                        for r in 0..shown {
                            ui.label(
                                egui::RichText::new(self.scene.layers.layer_name(r as u8)).small(),
                            );
                            for c in 0..shown {
                                // Lower triangle only (symmetric); blank the rest.
                                if c > r {
                                    ui.label("");
                                    continue;
                                }
                                let mut on = self.scene.layers.collide(r as u8, c as u8);
                                if ui.checkbox(&mut on, "").changed() {
                                    self.scene.layers.set_collide(r as u8, c as u8, on);
                                }
                            }
                            ui.end_row();
                        }
                    });
                    // Breathing room so the horizontal scrollbar (the 32-wide matrix
                    // needs one) sits below the grid instead of over its last row.
                    ui.add_space(18.0);
                });

                ui.separator();
                if ui.button("Reset to defaults").clicked() {
                    self.scene.layers = citrus_core::LayerSettings::default();
                    self.scene.visible_layers = citrus_core::all_layers_mask();
                }
            });
        self.show_layers = open;

        // Audio mixer window: a fader + mute per bus. `effective_gain(bus)` drives
        // playback volume (the audio backend multiplies each source by it).
        let mut mixer_open = self.show_mixer;
        egui::Window::new("Audio Mixer")
            .open(&mut mixer_open)
            .resizable(false)
            .show(ctx, |ui| {
                use citrus_editor::egui_phosphor::regular as ph;
                ui.label(
                    egui::RichText::new("Buses · linear gain, chained to Master")
                        .small()
                        .weak(),
                );
                // Grid so the mute icon / name / fader / gain columns line up.
                egui::Grid::new("audio-mixer-grid")
                    .num_columns(4)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        for name in self.mixer.bus_names() {
                            // Mute toggle as a phosphor speaker icon (no checkbox box).
                            let muted = self.mixer.is_muted(&name);
                            let icon = if muted { ph::SPEAKER_NONE } else { ph::SPEAKER_HIGH };
                            if ui
                                .add(egui::Button::new(icon).frame(false))
                                .on_hover_text(if muted { "Unmute" } else { "Mute" })
                                .clicked()
                            {
                                self.mixer.set_muted(&name, !muted);
                            }
                            ui.label(egui::RichText::new(&name).monospace());
                            let mut v = self.mixer.volume(&name);
                            ui.spacing_mut().slider_width = 130.0;
                            if ui
                                .add(egui::Slider::new(&mut v, 0.0..=1.0).fixed_decimals(2))
                                .changed()
                            {
                                self.mixer.set_volume(&name, v);
                            }
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} {:.2}",
                                    ph::ARROW_RIGHT,
                                    self.mixer.effective_gain(&name)
                                ))
                                .weak(),
                            );
                            ui.end_row();
                        }
                    });
            });
        self.show_mixer = mixer_open;
    }

    /// Modal shown when a bake found static models without a lightmap UV. Offers
    /// to generate a non-overlapping unwrap (persisted via a `.lmuv` marker, then
    /// reproduced on reload), bake without them, or cancel.
    fn lightmap_uv_window(&mut self, ctx: &egui::Context) {
        let Some(models) = self.pending_lm_uv_models.clone() else {
            return;
        };
        let mut choice: Option<u8> = None;
        egui::Window::new("Generate Lightmap UVs?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    "These models have no lightmap UV (second UV set). Baking their \
                     overlapping texture UVs would produce a garbage lightmap, so the \
                     bake skips them:",
                );
                for m in &models {
                    ui.label(format!("  • {m}"));
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Generate a non-overlapping lightmap UV for them? It's saved as a \
                         .lmuv marker next to the model and regenerated on load, so it \
                         persists. (Re-imports the scene.)",
                    )
                    .small()
                    .weak(),
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Generate UVs & Reload").clicked() {
                        choice = Some(0);
                    }
                    if ui.button("Bake Without Them").clicked() {
                        choice = Some(1);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(2);
                    }
                });
            });
        match choice {
            Some(0) => {
                self.pending_lm_uv_models = None;
                for m in &models {
                    if let Err(e) =
                        citrus_assets::set_lightmap_uv_marker(self.project_root.join(m), true)
                    {
                        tracing::error!("writing lightmap-UV marker for {m}: {e:#}");
                    }
                }
                if self.playing {
                    self.set_status("Stop play mode, then bake again", false);
                } else if self.scene_dirty {
                    self.set_status("Save the scene first, then bake again", false);
                } else if let Some(scene) = self.current_scene_path.clone() {
                    self.load_scene_runtime(&scene);
                    self.set_status("Generated lightmap UVs — click Bake again", false);
                } else {
                    self.set_status("Save the scene first, then bake again", false);
                }
            }
            Some(1) => {
                self.pending_lm_uv_models = None;
                self.do_bake();
            }
            Some(2) => self.pending_lm_uv_models = None,
            _ => {}
        }
    }

    /// Saves to `project.citrus` on every change.
    fn bindings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_bindings;
        let mut dirty = false;
        let mut cancel_rebind = false;
        egui::Window::new("Input Bindings")
            .open(&mut open)
            .resizable(true)
            .default_width(380.0)
            .show(ctx, |ui| {
                let b = &mut self.input.bindings;
                ui.horizontal(|ui| {
                    ui.label("Scheme");
                    let cur = b.active_scheme().map(|s| s.name.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("citrus-scheme")
                        .selected_text(cur)
                        .show_ui(ui, |ui| {
                            for i in 0..b.schemes.len() {
                                let name = b.schemes[i].name.clone();
                                if ui.selectable_label(b.active == i, name).clicked() {
                                    b.active = i;
                                    dirty = true;
                                }
                            }
                        });
                });
                ui.separator();
                if let Some(rb) = &self.rebinding {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!("Press a key/mouse button for \"{}\" (Esc cancels)", rb.1),
                    );
                }
                let active = b.active;
                if let Some(scheme) = b.schemes.get_mut(active) {
                    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                        for (ai, act) in scheme.actions.iter_mut().enumerate() {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.strong(&act.name);
                                    ui.label(
                                        egui::RichText::new(format!("{:?}", act.kind)).weak().small(),
                                    );
                                });
                                dirty |= binding_slot_row(ui, "Buttons", &mut act.buttons, ai, "button", &mut self.rebinding);
                                if matches!(act.kind, citrus_core::ActionKind::Axis1 | citrus_core::ActionKind::Axis2) {
                                    dirty |= binding_slot_row(ui, "+X", &mut act.pos_x, ai, "pos_x", &mut self.rebinding);
                                    dirty |= binding_slot_row(ui, "-X", &mut act.neg_x, ai, "neg_x", &mut self.rebinding);
                                }
                                if matches!(act.kind, citrus_core::ActionKind::Axis2) {
                                    dirty |= binding_slot_row(ui, "+Y", &mut act.pos_y, ai, "pos_y", &mut self.rebinding);
                                    dirty |= binding_slot_row(ui, "-Y", &mut act.neg_y, ai, "neg_y", &mut self.rebinding);
                                }
                                if let Some(a) = &act.analog_x {
                                    ui.label(egui::RichText::new(format!("analog X: {}", a.label())).weak().small());
                                }
                                if let Some(a) = &act.analog_y {
                                    ui.label(egui::RichText::new(format!("analog Y: {}", a.label())).weak().small());
                                }
                            });
                        }
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Reset to defaults").clicked() {
                        self.input.bindings = citrus_core::Bindings::default();
                        dirty = true;
                    }
                    if self.rebinding.is_some() && ui.button("Cancel rebind").clicked() {
                        cancel_rebind = true;
                    }
                });
            });
        if cancel_rebind {
            self.rebinding = None;
        }
        self.show_bindings = open;
        if dirty {
            self.project.bindings = self.input.bindings.clone();
            self.save_project();
        }
    }

    /// Network panel (2G): host or join a session, see status, disconnect.
    fn network_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_network;
        // Defer mutations that conflict with the &self.net borrow in the closure.
        enum NetAct {
            None,
            Host(u16),
            Join(String),
            Disconnect,
        }
        let mut act = NetAct::None;
        let addr = &mut self.net_addr;
        let net = self.net.as_ref();
        egui::Window::new("Network")
            .open(&mut open)
            .resizable(false)
            .default_width(300.0)
            .show(ctx, |ui| match net {
                Some(net) => {
                    ui.label(format!(
                        "{} — peer {}",
                        if net.is_host() { "Hosting" } else { "Client" },
                        net.local_peer()
                    ));
                    ui.label(format!("Peers: {}", net.peer_count()));
                    if let Some(a) = net.local_addr() {
                        ui.label(egui::RichText::new(format!("Local: {a}")).weak().small());
                    }
                    if ui.button("Disconnect").clicked() {
                        act = NetAct::Disconnect;
                    }
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.label("Address");
                        ui.text_edit_singleline(addr);
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Host").clicked() {
                            let port = addr
                                .rsplit(':')
                                .next()
                                .and_then(|p| p.parse::<u16>().ok())
                                .unwrap_or(9000);
                            act = NetAct::Host(port);
                        }
                        if ui.button("Join").clicked() {
                            act = NetAct::Join(addr.clone());
                        }
                    });
                    ui.label(
                        egui::RichText::new(
                            "Host binds the port; Join connects to a host. Objects with a \
                             Sync component replicate while playing.",
                        )
                        .weak()
                        .small(),
                    );
                }
            });
        self.show_network = open;
        match act {
            NetAct::None => {}
            NetAct::Disconnect => self.net = None,
            NetAct::Host(port) => match crate::net::NetSession::host(port) {
                Ok(s) => self.net = Some(s),
                Err(e) => self.set_status(format!("Host failed: {e}"), false),
            },
            NetAct::Join(a) => match crate::net::NetSession::join(&a) {
                Ok(s) => self.net = Some(s),
                Err(e) => self.set_status(format!("Join failed: {e}"), false),
            },
        }
    }

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
        if let Some(c) = file.editor_camera {
            self.camera.position = Vec3::from(c.position);
            self.camera.yaw = c.yaw;
            self.camera.pitch = c.pitch;
        }
        self.restore_collapsed(&file.collapsed);
        self.apply_skybox();
        self.load_bake();
    }

    /// Restore the scene-tree collapsed rows from saved object ids (map ids back
    /// to current indices; unknown ids are dropped).
    fn restore_collapsed(&mut self, collapsed: &[String]) {
        use std::collections::HashMap;
        let by_id: HashMap<String, usize> = self
            .scene
            .objects
            .iter()
            .enumerate()
            .map(|(i, o)| (o.id.to_string(), i))
            .collect();
        let set: Vec<usize> = collapsed
            .iter()
            .filter_map(|id| by_id.get(id).copied())
            .collect();
        self.scene_panel.set_collapsed(set);
    }

    /// Push the scene's skybox (or the procedural sky) to the renderer.
    fn apply_skybox(&mut self) {
        let skybox = self.scene.skybox.clone();
        let faces = self.scene.environment.skybox_faces.clone();
        let project_root = self.project_root.clone();
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        // Cubemap (6-face) skybox takes precedence over the equirect one — but only
        // once ALL six faces are assigned (a partial set falls back to equirect).
        if let Some(faces) = faces.filter(|f| f.iter().all(|s| !s.is_empty())) {
            let loaded: Result<Vec<_>, _> = faces
                .iter()
                .map(|rel| citrus_assets::load_texture_file(project_root.join(rel), true))
                .collect();
            match loaded {
                Ok(data) => {
                    let refs: [&citrus_render::TextureData; 6] = std::array::from_fn(|i| &data[i]);
                    if let Err(e) = renderer.set_skybox_cube(refs) {
                        tracing::error!("setting cubemap skybox: {e:#}");
                    }
                    return;
                }
                Err(e) => tracing::error!("loading cubemap skybox: {e:#}"),
            }
        }
        match skybox {
            Some(rel) => {
                let abs = project_root.join(&rel);
                let t0 = Instant::now();
                match citrus_assets::load_texture_file(&abs, true) {
                    Ok(data) => {
                        let decoded = t0.elapsed();
                        if let Err(e) = renderer.set_skybox(Some(&data)) {
                            tracing::error!("setting skybox: {e:#}");
                        }
                        tracing::info!(
                            "skybox {rel}: decode {:?} + set/env-cube {:?} ({}x{})",
                            decoded,
                            t0.elapsed() - decoded,
                            data.width,
                            data.height
                        );
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

    /// While a heavy job is queued, dim the screen and show a centered
    /// "working…" card. It paints this frame; the blocking job runs right after
    /// render, so the user sees it's busy instead of a frozen window.
    fn busy_overlay(&self, ctx: &egui::Context) {
        // A pending blocking job (plugin compile) or a blocking background task
        // (focus re-import) dims the UI and blocks interaction until it's done.
        let label = match &self.pending_job {
            Some(PendingJob::ReloadPlugins) => "Compiling components…".to_string(),
            None => match self.tasks.blocking() {
                Some(h) => h.label.clone(),
                None => return,
            },
        };
        let label = label.as_str();
        // Dim the whole UI.
        let screen = ctx.content_rect();
        egui::Area::new("citrus-busy-dim".into())
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(140));
            });
        // Centered card with a spinner + label.
        // Live phase ("Loading 3D models…", "Setting up lighting…") shared with
        // the startup splash; shown as the modal's detail line.
        let phase = self.load_status.lock().unwrap().clone();
        egui::Area::new("citrus-busy-card".into())
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(egui::RichText::new(label).strong());
                    });
                    let detail = if phase.is_empty() {
                        "The editor is busy — this can take a few seconds."
                    } else {
                        phase.as_str()
                    };
                    ui.label(egui::RichText::new(detail).small().weak());
                });
            });
        ctx.request_repaint();
    }

    /// While a bake runs, show a centered progress card (no full-screen dim, so
    /// the live lightmap build-up stays visible behind it). Each lightmap step is
    /// one long GPU submit that locks the frame; the card — painted before the
    /// first step and updated between steps — is the "something is happening".
    fn bake_modal(&self, ctx: &egui::Context) {
        let Some(runner) = self.bake_job.as_ref() else {
            return;
        };
        let (line, frac) = match runner.progress.lock().ok().as_deref() {
            Some(crate::tasks::TaskProgress::Bake {
                lightmap_done,
                lightmap_total,
                bounces,
                phase,
                ..
            }) => {
                let p = match phase {
                    crate::tasks::BakePhase::Lightmaps => "Tracing lightmaps",
                    crate::tasks::BakePhase::Probes => "Tracing probes",
                    crate::tasks::BakePhase::FluxVoxel => "Tracing FluxVoxel",
                };
                let total = (*lightmap_total).max(1);
                (
                    format!("{p} — {lightmap_done}/{total}  ·  {bounces} bounces"),
                    *lightmap_done as f32 / total as f32,
                )
            }
            _ => ("Building light data…".to_string(), 0.0),
        };
        egui::Area::new("citrus-bake-card".into())
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 24.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(egui::RichText::new("Building light data").strong());
                    });
                    ui.add(
                        egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                            .desired_width(260.0)
                            .text(line),
                    );
                    ui.label(
                        egui::RichText::new("The viewport may pause per lightmap.")
                            .small()
                            .weak(),
                    );
                });
            });
        ctx.request_repaint();
    }

    /// On window-focus regain: if any project asset changed in another app since
    /// the last check, reload the current scene (which reimports models/textures/
    /// materials/shaders fresh). Skips while playing or with unsaved scene edits
    /// (those would be clobbered), surfacing a hint instead.
    fn reload_changed_assets(&mut self) {
        use crate::tasks::{TaskKind, TaskPayload};
        let now = std::time::SystemTime::now();
        // First focus just establishes a baseline (don't reload on startup).
        let Some(since) = self.last_asset_check.replace(now) else {
            return;
        };
        if self.playing || self.bake_job.is_some() {
            return;
        }
        if self.tasks.blocking().is_some() {
            return; // a re-import is already running
        }
        // The filesystem walk runs on a worker (it can be slow on a big project);
        // a blocking modal covers it, and the apply (GPU reload) runs on the main
        // thread when it returns.
        let root = self.project_root.clone();
        let scene_path = self.current_scene_path.clone();
        self.tasks.spawn(
            "Checking for changes…",
            TaskKind::FocusReimport,
            true,
            move |cancel, _progress| {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(TaskPayload::None);
                }
                match first_changed_asset(&root, since) {
                    Some(changed) => {
                        let label = changed
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        Ok(TaskPayload::FocusChanges { scene_path, label })
                    }
                    None => Ok(TaskPayload::None),
                }
            },
        );
    }

    /// Apply a captured input to the slot/action currently being rebound (2C),
    /// then persist bindings. Called from the window-event handler.
    fn apply_rebind(&mut self, src: citrus_core::InputSource) {
        let Some((ai, slot)) = self.rebinding.take() else {
            return;
        };
        if let Some(scheme) = self.input.bindings.active_scheme_mut()
            && let Some(act) = scheme.actions.get_mut(ai)
        {
            let list = match slot.as_str() {
                "button" => &mut act.buttons,
                "pos_x" => &mut act.pos_x,
                "neg_x" => &mut act.neg_x,
                "pos_y" => &mut act.pos_y,
                "neg_y" => &mut act.neg_y,
                _ => return,
            };
            if !list.contains(&src) {
                list.push(src);
            }
        }
        self.project.bindings = self.input.bindings.clone();
        self.save_project();
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
    /// Apply a finished worker task on the main thread (GPU upload etc.).
    /// Import/texture/focus arms are filled in by their phases; cancelled and
    /// failed tasks just surface a notification.
    fn apply_task_result(&mut self, r: crate::tasks::TaskResult) {
        use crate::tasks::{NotifyLevel, TaskPayload};
        if r.cancelled {
            self.tasks.notify("Task cancelled", NotifyLevel::Info);
            return;
        }
        let payload = match r.outcome {
            Ok(p) => p,
            Err(e) => {
                self.tasks.notify(format!("Task failed: {e}"), NotifyLevel::Warn);
                return;
            }
        };
        match payload {
            TaskPayload::None => {}
            TaskPayload::Model { source, scene } => {
                self.apply_imported_model(source, *scene);
            }
            TaskPayload::Scene {
                path,
                file,
                models,
                prepared,
                material_textures,
            } => {
                self.apply_loaded_scene(path, *file, models, prepared, material_textures);
            }
            TaskPayload::FocusChanges { scene_path, label } => {
                use crate::tasks::NotifyLevel;
                if self.scene_dirty {
                    self.tasks.notify(
                        format!("'{label}' changed on disk — reload to apply"),
                        NotifyLevel::Warn,
                    );
                } else if let Some(path) = scene_path {
                    self.load_scene_runtime(&path);
                    self.tasks
                        .notify(format!("Reimported after '{label}' changed"), NotifyLevel::Info);
                }
            }
        }
    }

    /// Main-thread GPU apply for a worker-parsed model: upload meshes/textures
    /// into the scene. `source` is the project-relative path.
    fn apply_imported_model(&mut self, source: PathBuf, scene: citrus_assets::Scene) {
        use crate::tasks::NotifyLevel;
        // A bake embeds mesh device addresses in its TLAS, so uploading new meshes
        // mid-bake would invalidate it — defer the upload until the bake ends.
        if self.bake_job.is_some() {
            self.tasks
                .notify("Import queued (baking)", NotifyLevel::Info);
            self.deferred_model_applies.push((source, scene));
            return;
        }
        let result = match self.renderer.as_mut() {
            Some(renderer) => self.scene.add_asset_scene(renderer, &scene, Some(source.as_path())),
            None => return,
        };
        match result {
            Ok(()) => {
                self.scene_dirty = true;
                self.tasks
                    .notify(format!("Imported {}", source.display()), NotifyLevel::Info);
            }
            Err(e) => {
                tracing::error!("importing model: {e:#}");
                self.tasks
                    .notify(format!("Import failed: {e}"), NotifyLevel::Warn);
            }
        }
    }

    /// The status-bar background-tasks popup: per-task progress bar + cancel.
    fn task_popup(&mut self, ctx: &egui::Context) {
        if !self.show_task_popup {
            return;
        }
        let mut cancel: Option<crate::tasks::TaskId> = None;
        egui::Window::new("Background tasks")
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-8.0, -32.0))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                let mut any = false;
                for h in self.tasks.visible() {
                    any = true;
                    let (frac, detail) = {
                        let p = h.progress.lock().unwrap();
                        (p.fraction(), p.detail())
                    };
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&h.label).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Cancel").clicked() {
                                cancel = Some(h.id);
                            }
                        });
                    });
                    match frac {
                        Some(f) => {
                            ui.add(egui::ProgressBar::new(f).desired_height(8.0));
                        }
                        None => {
                            ui.add(egui::ProgressBar::new(0.0).desired_height(8.0).animate(true));
                        }
                    }
                    ui.label(egui::RichText::new(detail).size(12.0).weak());
                    ui.add_space(4.0);
                }
                if !any {
                    ui.label(egui::RichText::new("No background tasks").weak());
                }
            });
        if let Some(id) = cancel {
            self.tasks.cancel(id);
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let lsp_busy = !self.lsp_requests.is_empty();
        let recent = self
            .status
            .as_ref()
            .filter(|(_, t, _)| t.elapsed().as_secs_f32() < 6.0)
            .map(|(m, _, s)| (m.clone(), *s));
        let project = self.project.name.clone();
        let objects = self.scene.objects.len();
        // Full name + project-relative path of the selected file, so the user
        // can read the whole filename the grid tiles crop with an ellipsis.
        let selected_file = if let Selection::File(path) = &self.selection {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rel = path
                .strip_prefix(&self.project_root)
                .unwrap_or(path)
                .display()
                .to_string();
            Some((name, rel))
        } else {
            None
        };
        egui::TopBottomPanel::bottom("citrus-status-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{project}  ·  {objects} objects")).size(14.0));
                if let Some((name, rel)) = selected_file {
                    ui.separator();
                    ui.label(egui::RichText::new("File:").size(14.0).weak());
                    ui.label(egui::RichText::new(name).size(14.0))
                        .on_hover_text(rel);
                }
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
                    // Background-tasks button + aggregate progress (center gap).
                    if let Some((n, frac)) = self.tasks.aggregate() {
                        ui.separator();
                        if ui
                            .button(egui::RichText::new(format!("⚙ Tasks ({n})")).size(14.0))
                            .clicked()
                        {
                            self.show_task_popup = !self.show_task_popup;
                        }
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(90.0)
                                .desired_height(8.0),
                        );
                    } else {
                        self.show_task_popup = false;
                    }
                    // Most recent task notification (auto-expires).
                    if let Some(note) = self.tasks.notifications.last() {
                        let col = match note.level {
                            crate::tasks::NotifyLevel::Warn => egui::Color32::from_rgb(230, 170, 90),
                            crate::tasks::NotifyLevel::Info => egui::Color32::from_gray(180),
                        };
                        ui.separator();
                        ui.label(egui::RichText::new(&note.text).size(13.0).color(col));
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

        // Engine built-in shaders (e.g. standard.frag) are baked into the binary
        // at build time, so watch their source on disk and recompile + hot-swap
        // the pipeline when they change outside the editor.
        let frag = std::path::Path::new(citrus_render::ENGINE_SHADER_DIR).join("standard.frag");
        let mtime = std::fs::metadata(&frag).and_then(|m| m.modified()).ok();
        if mtime != self.engine_shader_mtime {
            let first = self.engine_shader_mtime.is_none();
            self.engine_shader_mtime = mtime;
            if !first {
                // Drop the renderer borrow before set_status (which needs &mut self).
                if let Some(result) = self.renderer.as_mut().map(|r| r.reload_standard_shaders()) {
                    match result {
                        Ok(()) => {
                            self.set_status("Reloaded engine shader (standard.frag)", false)
                        }
                        Err(e) => self.set_status(format!("standard.frag reload failed: {e}"), true),
                    }
                }
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
        // Honour the inspector lock: when locked, the inspector edits the LOCKED
        // object, not `self.selection`. The snapshot must track the same target or
        // edits to a locked object diff against the wrong one and never record.
        let effective = if self.inspector.locked {
            self.inspector_lock_target
                .clone()
                .unwrap_or_else(|| self.selection.clone())
        } else {
            self.selection.clone()
        };
        match &effective {
            Selection::Object(i) if *i < self.scene.objects.len() => {
                let o = &self.scene.objects[*i];
                // Capture the material the inspector is actually editing — the
                // SELECTED slot, not always the primary (`o.render`). For a
                // multi-material mesh, editing slot >0 changed a different material
                // than the snapshot tracked, so the diff missed it and the edit
                // never landed in the undo tree.
                let slots: Vec<_> = o.render_slots().collect();
                let slot = self.selected_material_slot.min(slots.len().saturating_sub(1));
                let material = slots.get(slot).map(|r| r.material);
                EditSnapshot::Object {
                    index: *i,
                    state: object_state(o),
                    material,
                    model: material.map(|m| Box::new(self.scene.materials[m].model.clone())),
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
    /// Apply the anchor object's edit to the other multi-selected objects.
    /// Transforms propagate as a DELTA (translate/rotate/scale together, matching
    /// the gizmo); shared components are set to the anchor's new value wholesale
    /// (correct when the value was shared, which the shared-inspector enforces).
    fn propagate_multi_edit(
        &mut self,
        anchor: usize,
        before: &ObjectState,
        after: &ObjectState,
    ) -> Vec<UndoEntry> {
        let dt = after.translation - before.translation;
        let dr = after.rotation * before.rotation.inverse();
        let inv = |v: f32| if v.abs() > 1e-6 { v } else { 1.0 };
        let ds = glam::Vec3::new(
            after.scale.x / inv(before.scale.x),
            after.scale.y / inv(before.scale.y),
            after.scale.z / inv(before.scale.z),
        );
        // Components whose RON changed on the anchor this edit.
        let changed: Vec<(String, String)> = after
            .components
            .iter()
            .filter(|(name, ron)| {
                before
                    .components
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, r)| r)
                    != Some(ron)
            })
            .cloned()
            .collect();
        let targets: Vec<usize> = self
            .multi_objects
            .iter()
            .copied()
            .filter(|&j| j != anchor && j < self.scene.objects.len())
            .collect();
        let xform_changed = dt != glam::Vec3::ZERO
            || ds != glam::Vec3::ONE
            || before.rotation != after.rotation;
        let mut entries = Vec::new();
        for j in targets {
            // Snapshot before mutating so the propagated edit is itself undoable
            // (otherwise undo would revert only the anchor, leaving the others).
            let t_before = object_state(&self.scene.objects[j]);
            if xform_changed {
                let o = &mut self.scene.objects[j];
                o.translation += dt;
                o.rotation = (dr * o.rotation).normalize();
                o.scale *= ds;
            }
            if !changed.is_empty() {
                let mut comps = self.scene.objects[j].save_components();
                let mut touched = false;
                for (name, ron) in &changed {
                    if let Some(slot) = comps.iter_mut().find(|(n, _)| n == name) {
                        slot.1 = ron.clone();
                        touched = true;
                    }
                }
                if touched {
                    self.scene.objects[j].load_components(&comps, &self.components);
                }
            }
            let t_after = object_state(&self.scene.objects[j]);
            if t_after != t_before {
                // Returned to the caller to bundle into ONE Group entry with the
                // anchor, so the whole multi-move is a single undo step (and one
                // coalescing unit across the drag, not N interleaved entries).
                entries.push(UndoEntry::Object {
                    index: j,
                    before: t_before,
                    after: t_after,
                });
            }
        }
        entries
    }

    fn record_edits(&mut self, pre: EditSnapshot, gesture_active: bool) {
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
                    let anchor = UndoEntry::Object {
                        index,
                        before: state.clone(),
                        after: now.clone(),
                    };
                    // Multi-select: mirror the anchor's edit to the other selected
                    // objects (transforms as a delta so they move/rotate/scale
                    // together; shared components set to the anchor's new value), and
                    // bundle anchor + others into ONE atomic Group so a multi-move is
                    // a single undo step that coalesces cleanly across the drag.
                    if self.multi_objects.len() > 1 && self.multi_objects.contains(&index) {
                        let mut group = vec![anchor];
                        group.extend(self.propagate_multi_edit(index, &state, &now));
                        let entry = if group.len() > 1 {
                            UndoEntry::Group(group)
                        } else {
                            group.pop().unwrap()
                        };
                        self.undo_stack.record(entry, gesture_active);
                    } else {
                        self.undo_stack.record(anchor, gesture_active);
                    }
                }
                if let (Some(material), Some(model)) = (material, model)
                    && material < self.scene.materials.len()
                {
                    let current = &self.scene.materials[material].model;
                    if *current != *model {
                        self.scene_dirty = true;
                        self.dirty_materials.insert(material);
                        self.last_material_edit = Some(Instant::now());
                        self.undo_stack.record(
                            UndoEntry::Material {
                                index: material,
                                before: model,
                                after: Box::new(current.clone()),
                            },
                            gesture_active,
                        );
                    }
                }
            }
            EditSnapshot::File { path, model } => {
                if let Some(fm) = &self.file_material
                    && fm.path == path
                    && fm.model != *model
                {
                    self.last_material_edit = Some(Instant::now());
                    self.undo_stack.record(
                        UndoEntry::FileMaterial {
                            path,
                            before: model,
                            after: Box::new(fm.model.clone()),
                        },
                        gesture_active,
                    );
                }
            }
            _ => {}
        }
    }

    /// Auto-save edited materials once the edit gesture settles. Materials
    /// without a backing file get one created under `materials/`.
    fn autosave_materials(&mut self) {
        // Code editors: save each 1s after its last keystroke (saving .frag
        // files also triggers shader hot reload, for live shader editing).
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

    /// Persist one scene material to its `.material` file, **only if it already
    /// has one**. Embedded (imported) materials have no backing file and are
    /// read-only until the user extracts them ([`extract_material`]); we never
    /// auto-create a phantom `.material` for them.
    fn save_scene_material(&mut self, index: usize) {
        let Some(path) = self.scene.materials[index].file.clone() else {
            return;
        };
        let model = self.scene.materials[index].model.clone();
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
        file.textures = crate::scene::tex_file_from_paths(&model.textures);
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

    /// Extract an embedded (imported) material to a real `.material` file under
    /// `materials/`, assign it back to the material slot, and persist it. After
    /// that it's a normal editable asset. No-op if it already has a file.
    fn extract_material(&mut self, index: usize) {
        if index >= self.scene.materials.len() || self.scene.materials[index].file.is_some() {
            return;
        }
        let name = self.scene.materials[index].model.name.clone();
        let stem: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
            .collect();
        let stem = stem.trim_matches('_');
        let dir = self.project_root.join("materials");
        let path = unique_path(&dir, if stem.is_empty() { "material" } else { stem }, "material");
        self.scene.set_material_file(index, path.clone());
        self.save_scene_material(index);
        self.set_status(
            format!("Extracted material '{}' → {}", name, relative_to(&path, &self.project_root)),
            false,
        );
    }

    /// Re-load a model file and write its embedded textures (PNG) + materials
    /// (`.material`) into `<project>/extracted/<model>/{textures,materials}/` so the
    /// imported assets become editable, reusable project files. Textures decoded on
    /// import are not retained, so this re-reads the source model.
    fn extract_model_assets(&mut self, path: &Path) {
        let scene = match citrus_assets::load_model(path) {
            Ok(s) => s,
            Err(e) => {
                self.set_status(format!("Extract failed: {e:#}"), true);
                return;
            }
        };
        let stem: String = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("model")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
            .collect();
        let stem = stem.trim_matches('_');
        let stem = if stem.is_empty() { "model" } else { stem };
        let base = self.project_root.join("extracted").join(stem);
        let tex_dir = base.join("textures");
        let mat_dir = base.join("materials");
        if let Err(e) = std::fs::create_dir_all(&tex_dir).and_then(|_| std::fs::create_dir_all(&mat_dir)) {
            self.set_status(format!("Extract failed: {e}"), true);
            return;
        }

        // Write each embedded texture as a PNG; remember its path per index.
        let mut tex_paths: Vec<Option<PathBuf>> = vec![None; scene.textures.len()];
        let mut tex_written = 0;
        for (i, t) in scene.textures.iter().enumerate() {
            let p = tex_dir.join(format!("tex_{i}.png"));
            match image::RgbaImage::from_raw(t.width, t.height, t.pixels.clone()) {
                Some(buf) => match buf.save(&p) {
                    Ok(()) => {
                        tex_paths[i] = Some(p);
                        tex_written += 1;
                    }
                    Err(e) => tracing::warn!("extract texture {i}: {e}"),
                },
                None => tracing::warn!("extract texture {i}: bad RGBA buffer size"),
            }
        }

        // Write each material as a `.material` referencing the extracted textures
        // by project-relative path.
        let rel = |p: &Option<PathBuf>| -> Option<PathBuf> {
            p.as_ref().map(|p| PathBuf::from(relative_to(p, &self.project_root)))
        };
        let mut mat_written = 0;
        for (mi, m) in scene.materials.iter().enumerate() {
            let mut textures = citrus_assets::MaterialTextures::default();
            textures.albedo = m.albedo.and_then(|i| rel(&tex_paths[i]));
            textures.normal = m.normal.and_then(|i| rel(&tex_paths[i]));
            textures.orm = m.orm.and_then(|i| rel(&tex_paths[i]));
            textures.emission = m.emission.and_then(|i| rel(&tex_paths[i]));
            let mf = citrus_assets::MaterialFile {
                name: m.name.clone(),
                shader: "standard".into(),
                params: m.params,
                features: m.features,
                render_queue: None,
                textures,
                custom: Default::default(),
            };
            let name_stem: String = m
                .name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
                .collect();
            let name_stem = name_stem.trim_matches('_');
            let fallback = format!("material_{mi}");
            let stem = if name_stem.is_empty() { fallback.as_str() } else { name_stem };
            let mp = unique_path(&mat_dir, stem, "material");
            match citrus_assets::save_material_file(&mp, &mf) {
                Ok(()) => mat_written += 1,
                Err(e) => tracing::warn!("extract material '{}': {e:#}", m.name),
            }
        }

        self.set_status(
            format!(
                "Extracted {tex_written} texture(s) + {mat_written} material(s) → {}",
                relative_to(&base, &self.project_root)
            ),
            false,
        );
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
        self.apply_entry(entry, undo);
    }

    /// Apply a single history entry (recursing into `Group`). `apply_history` owns
    /// the pop + `suppress_undo_record`; this just mutates the scene.
    fn apply_entry(&mut self, entry: UndoEntry, undo: bool) {
        match entry {
            UndoEntry::Group(entries) => {
                // Undo reverts in reverse application order; redo replays forward.
                if undo {
                    for e in entries.into_iter().rev() {
                        self.apply_entry(e, undo);
                    }
                } else {
                    for e in entries {
                        self.apply_entry(e, undo);
                    }
                }
            }
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
                    self.rt_gi.invalidate();
                }
            }
            UndoEntry::Assign {
                object,
                slot,
                before,
                after,
            } => {
                let material = if undo { before } else { after };
                if object < self.scene.objects.len() && material < self.scene.materials.len() {
                    self.scene.set_slot_material(object, slot, material);
                }
                self.rt_gi.invalidate();
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

    /// Open the profiler as a separate OS window (movable to another monitor).
    /// Needs the event loop to create the winit window, so it's driven from
    /// `redraw` via `update_profiler_window`.
    fn open_profiler_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.profiler_window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Citrus Profiler")
            .with_inner_size(winit::dpi::LogicalSize::new(1120.0, 840.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("creating profiler window: {e:#}");
                self.show_stats_overlay = false;
                return;
            }
        };
        let (Ok(disp), Ok(wh)) = (window.display_handle(), window.window_handle()) else {
            tracing::error!("profiler window exposed no raw handles");
            self.show_stats_overlay = false;
            return;
        };
        let (disp, wh) = (disp.as_raw(), wh.as_raw());
        let size = window.inner_size();
        match self.renderer.as_mut() {
            Some(r) => {
                if let Err(e) = r.open_profiler_window(disp, wh, size.width, size.height) {
                    tracing::error!("renderer profiler window: {e:#}");
                    self.show_stats_overlay = false;
                    return;
                }
            }
            None => return,
        }
        // Fresh egui context per open: the renderer's egui texture store was
        // destroyed on close, so a reused context (which thinks the font atlas
        // Managed(0) is already uploaded and won't resend it) would draw against
        // a now-empty store -> "Bad texture ID". A new context re-uploads all
        // textures on its first frame.
        self.profiler_egui_ctx = egui::Context::default();
        apply_editor_style(&self.profiler_egui_ctx);
        self.profiler_egui_state = Some(egui_winit::State::new(
            self.profiler_egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            None,
            None,
            None,
        ));
        window.request_redraw();
        self.profiler_window = Some(window);
    }

    /// Close the profiler window: the renderer tears down its surface/swapchain
    /// (full device idle) BEFORE the winit window drops, as required.
    fn close_profiler_window(&mut self) {
        if let Some(r) = self.renderer.as_mut() {
            r.close_profiler_window();
        }
        self.profiler_egui_state = None;
        self.profiler_window = None;
    }

    /// Fold this frame's metrics into the current time bucket; flush a sample to
    /// every series once `BUCKET` seconds have elapsed (keeps the 20 s window
    /// stable at any frame rate). Values array order MUST match `ProfHistory`.
    fn record_prof_sample(&mut self, dt: f32) {
        let s = self.last_render_stats;
        let t = &self.frame_timings;
        let util = if self.stats.frame_ms > 0.0 {
            (t.gpu_frame / self.stats.frame_ms * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        let vals = [
            self.stats.frame_ms,
            self.stats.fps,
            t.gpu_frame,
            t.gpu_gi,
            s.gpu_shadows_ms,
            s.gpu_scene_ms,
            s.gpu_reflect_ms,
            s.gpu_post_ms,
            s.gpu_egui_ms,
            s.gpu_cam_preview_ms,
            util,
            t.gi,
            t.components,
            t.physics,
            t.audio,
            t.draws,
            t.render,
            s.draw_calls as f32,
        ];
        let h = &mut self.prof_history;
        // Spikes matter, so each bucket keeps the max seen across its frames.
        for (m, &v) in h.metrics.iter_mut().zip(vals.iter()) {
            m.pending = m.pending.max(v);
        }
        h.bucket_t += dt;
        if h.bucket_t >= Series::BUCKET {
            h.bucket_t = 0.0;
            for (m, &v) in h.metrics.iter_mut().zip(vals.iter()) {
                m.series.push(m.pending);
                m.pending = v; // next bucket starts from the current frame
            }
        }
    }

    /// Reconcile the desired profiler state with the actual window, then run +
    /// render the profiler egui UI into it. Called once per frame from `redraw`.
    fn update_profiler_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.show_stats_overlay && self.profiler_window.is_none() {
            self.open_profiler_window(event_loop);
        } else if !self.show_stats_overlay && self.profiler_window.is_some() {
            self.close_profiler_window();
        }
        let Some(window) = self.profiler_window.clone() else {
            return;
        };
        let raw_input = match self.profiler_egui_state.as_mut() {
            Some(state) => state.take_egui_input(&window),
            None => return,
        };
        let ctx = self.profiler_egui_ctx.clone();
        let output = ctx.run(raw_input, |ctx| self.profiler_ui(ctx));
        if let Some(state) = self.profiler_egui_state.as_mut() {
            state.handle_platform_output(&window, output.platform_output);
        }
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let draw = citrus_render::EguiDraw {
            pixels_per_point: output.pixels_per_point,
            primitives,
            textures_delta: output.textures_delta,
        };
        if let Some(r) = self.renderer.as_mut() {
            if let Err(e) = r.render_profiler(&draw) {
                tracing::error!("profiler render: {e:#}");
            }
        }
        window.request_redraw();
    }

    /// The profiler window's egui contents: one combined realtime graph, a
    /// clickable legend (toggles each metric in the graph) with current values,
    /// and the non-time-series draw/pipeline counts.
    fn profiler_ui(&mut self, ctx: &egui::Context) {
        let s = self.last_render_stats;
        let frame_ms = self.stats.frame_ms;
        let fps = self.stats.fps;
        let mut toggle: Option<usize> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading(format!("{frame_ms:.1} ms   {fps:.0} fps"));
                ui.label(
                    egui::RichText::new("20 s history · shared axis · hover a line for details")
                        .size(11.0)
                        .color(egui::Color32::from_gray(130)),
                );
                ui.add_space(2.0);

                draw_combined_graph(ui, &self.prof_history);
                ui.add_space(4.0);

                ui.label(egui::RichText::new("Click a row to show/hide it").strong());
                for (i, m) in self.prof_history.metrics.iter().enumerate() {
                    if legend_row(ui, m).clicked() {
                        toggle = Some(i);
                    }
                }

                ui.add_space(6.0);
                ui.label(egui::RichText::new("Draw / pipeline").strong());
                count_row(ui, "Draw calls", s.draw_calls);
                count_row(ui, "  opaque", s.opaque_draws);
                count_row(ui, "  transparent", s.transparent_draws);
                count_row(ui, "  outline", s.outline_draws);
                if s.error_draws > 0 {
                    count_row(ui, "  error", s.error_draws);
                }
                count_row(ui, "Materials drawn", s.materials_drawn);
                count_row(ui, "Pipeline binds", s.pipeline_binds);
                count_row(ui, "Shader variants", s.pipeline_variants);
            });
        });
        if let Some(i) = toggle
            && let Some(m) = self.prof_history.metrics.get_mut(i)
        {
            m.visible = !m.visible;
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        self.stats.tick(dt);
        // Background tasks: collect finished worker results, apply their GPU side
        // on this (main) thread, and expire old notifications.
        let results = self.tasks.poll_workers();
        for r in results {
            self.apply_task_result(r);
        }
        // Advance the chunked bake one unit this frame (GPU, main thread). The
        // first frame after starting is a warm-up: paint the bake modal once
        // before the first (UI-locking) lightmap step, so there's an indicator.
        if self.bake_pending_warmup {
            self.bake_pending_warmup = false;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        } else {
            self.step_bake();
        }
        self.tasks.prune_notifications();
        self.update_camera(dt);
        self.refresh_shaders();
        let gi_t = Instant::now();
        self.update_realtime_gi(dt);
        let gi_ms = gi_t.elapsed().as_secs_f32() * 1000.0;
        FrameTimings::ema(&mut self.frame_timings.gi, gi_ms);

        // If mouse-look just ended, egui's pointer state is stale: window
        // events are withheld while looking, so egui (a) never saw the right
        // button RELEASE, so it still thinks Secondary is held, which keeps
        // `press_origin` pinned and breaks drag detection for the next gesture
        // (a click still registers, but orbit/gizmo/widget drags don't), and
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
        // VR pointer: feed the right-controller-on-panel position into egui as the
        // mouse, so the whole desktop UI is clickable from inside the headset.
        if let Some(pos) = self.vr_ui_pointer {
            raw_input.events.push(egui::Event::PointerMoved(pos));
            if self.vr_ui_pressed != self.vr_ui_prev_pressed {
                raw_input.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: self.vr_ui_pressed,
                    modifiers: egui::Modifiers::default(),
                });
            }
        } else if self.vr_ui_prev_pressed {
            // Pointer left the panel while pressed: release so egui doesn't stick.
            raw_input.events.push(egui::Event::PointerButton {
                pos: egui::Pos2::ZERO,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
        }
        self.vr_ui_prev_pressed = self.vr_ui_pressed;
        let size = window.inner_size();
        // The editor viewport renders to an offscreen texture sized to the dock
        // rect (RTT), so the camera aspect comes from that rect, not the window.
        let ppp = self.egui_ctx.pixels_per_point();
        let vrect = self.viewport_rect;
        let (vw, vh) = (
            (vrect.width() * ppp).round().max(1.0) as u32,
            (vrect.height() * ppp).round().max(1.0) as u32,
        );
        let aspect = if vrect.is_finite() && vrect.height() > 1.0 {
            vrect.width() / vrect.height()
        } else {
            size.width.max(1) as f32 / size.height.max(1) as f32
        };
        let view = self.camera.view();
        let proj = self.camera.proj(aspect);
        let viewport_extent = if self.viewport_visible && vrect.is_finite() {
            Some([vw, vh])
        } else {
            None
        };

        let render_stats = self
            .renderer
            .as_ref()
            .map(|r| r.stats())
            .unwrap_or_default();
        // Only fold profiling state when the profiler window is open, so a
        // closed profiler adds zero per-frame work (the renderer likewise skips
        // its GPU timestamp queries when no profiler window exists).
        if self.profiler_window.is_some() {
            FrameTimings::ema(&mut self.frame_timings.gpu_frame, render_stats.gpu_frame_ms);
            FrameTimings::ema(&mut self.frame_timings.gpu_gi, render_stats.gpu_gi_ms);
            self.last_render_stats = render_stats;
            self.record_prof_sample(dt);
        }

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
            self.vr_tools_window(ctx);
            self.project_windows(ctx);
            self.bindings_window(ctx);
            self.network_window(ctx);
            self.layers_window(ctx);
            self.lightmap_uv_window(ctx);
            self.busy_overlay(ctx);
            self.bake_modal(ctx);
            self.status_bar(ctx);
            self.task_popup(ctx);
            // The render stats now live in a separate OS window (the profiler),
            // reconciled + rendered after this egui pass — no in-viewport panel.

            let camera_preview = self
                .renderer
                .as_ref()
                .and_then(|r| r.camera_preview_texture());
            let viewport_texture = self
                .renderer
                .as_ref()
                .and_then(|r| r.viewport_texture());
            let camera_overlay = matches!(
                self.selection,
                Selection::Object(i) if self.scene.camera_view_proj_for(i, 1.0).is_some()
            );
            let mut dock_state = std::mem::replace(&mut self.dock_state, DockState::new(vec![]));
            // The focused code tab drives which file's diagnostics the
            // Inspector shows (kept out of the editor to stop layout shift).
            let focused_code = dock_state
                .find_active_focused()
                .and_then(|(_, tab)| match tab {
                    Tab::Code(p) => Some(p.clone()),
                    _ => None,
                });
            // egui_dock only calls TabViewer::ui for the visible (active) tab of
            // each leaf, so camera_ui sets this flag iff the Camera tab is on
            // screen this frame. Reset before the dock, read after to gate the
            // (now Flux-traced) preview render.
            self.camera_tab_visible = false;
            self.viewport_visible = false;
            // Models whose lightmap UVs were auto-generated (.lmuv marker), for the
            // FluxBaker "Generated Lightmap UVs" list. Computed before the mutable
            // scene borrow below.
            let generated_uv_models =
                self.scene.models_with_generated_lightmap_uv(&self.project_root);
            let mut tabs = EditorTabs {
                camera_visible: &mut self.camera_tab_visible,
                viewport_visible: &mut self.viewport_visible,
                scene: &mut self.scene,
                selection: &mut self.selection,
                multi_objects: &mut self.multi_objects,
                multi_files: &mut self.multi_files,
                inspector: &mut self.inspector,
                inspector_lock_target: &mut self.inspector_lock_target,
                scene_panel: &mut self.scene_panel,
                file_browser: &mut self.file_browser,
                file_diagnostics: &self.file_diagnostics,
                editor_components: &self.editor_components,
                file_material: &mut self.file_material,
                file_meta: &mut self.file_meta,
                open_editors: &mut self.open_editors,
                focused_code,
                gizmo: &mut self.gizmo,
                widget_filter: &mut self.widget_filter,
                gi_debug: &mut self.gi_debug,
                gizmo_drag: &mut self.gizmo_drag,
                orbit_armed: &mut self.orbit_armed,
                // An orbit is genuinely in progress (locked pivot) — used to ignore
                // all viewport widgets/gizmos while the camera is rotating.
                orbiting: self.orbit_pivot.is_some(),
                pointer_in_viewport,
                actions: &mut self.actions,
                viewport_rect: &mut self.viewport_rect,
                registry: &self.components,
                shader_list: &shader_list,
                shader_info: shader_info.as_ref(),
                camera_preview,
                viewport_texture,
                camera_overlay,
                view,
                proj,
                looking: self.looking,
                vim_mode: self.project.settings.vim_mode,
                log_filter: &mut self.log_filter,
                handle_drag: &mut self.handle_drag,
                can_bake,
                lightmap_preview: &mut self.lightmap_preview,
                generated_uv_models: &generated_uv_models,
                selected_material_slot: &mut self.selected_material_slot,
            };
            DockArea::new(&mut dock_state)
                .style(egui_dock::Style::from_egui(ctx.style().as_ref()))
                // Per-tab close X (only on closeable tabs = Code); no group
                // close-all button.
                .show_close_buttons(true)
                .show_leaf_close_all_buttons(false)
                // No per-leaf collapse button: it only hid the leaf's contents
                // without resizing the neighbouring docks, which was confusing.
                .show_leaf_collapse_buttons(false)
                .show(ctx, &mut tabs);
            self.dock_state = dock_state;

            // Clear the published "dragged object" once the pointer is up, after
            // ObjectRef drop boxes have had this frame to consume it (so a later
            // plain release over a box can't re-apply a stale drag).
            if !ctx.input(|i| i.pointer.any_down()) {
                ctx.data_mut(|d| d.remove::<usize>(egui::Id::new(citrus_editor::DRAG_OBJECT_KEY)));
                ctx.data_mut(|d| {
                    d.remove::<String>(egui::Id::new(citrus_editor::DRAG_FILE_KEY))
                });
            }
        });

        if let Some(egui_state) = self.egui_state.as_mut() {
            egui_state.handle_platform_output(&window, output.platform_output);
        }
        let primitives = egui_ctx.tessellate(output.shapes, output.pixels_per_point);

        self.process_actions();
        // Per-probe "Bake This Probe" requests from the inspector button.
        self.poll_probe_bake_requests();
        // Reflection-probe bake: a few frames after the recapture request the cube
        // holds the captured scene, so read it back to disk now.
        if let Some(n) = self.refl_bake_pending {
            if n == 0 {
                self.refl_bake_pending = None;
                self.save_reflection_bake();
            } else {
                self.refl_bake_pending = Some(n - 1);
            }
        }
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
            // Advance the play clock (pauses with the sim) so time-based
            // components don't jump across a pause.
            self.play_time += dt;
            // Resolve input + networking view for this frame, then drive
            // components. Component-driven motion must not land in undo history;
            // play edits are restored wholesale on Stop anyway.
            if let Some(net) = self.net.as_mut() {
                net.pump();
            }
            let net_view = self.net.as_mut().map(|n| n.view()).unwrap_or_default();
            self.input.resolve_frame();
            let mut commands = Vec::new();
            let comp_t = Instant::now();
            self.scene
                .update_components(dt, self.play_time, self.input.state(), &net_view, &mut commands);
            let comp_ms = comp_t.elapsed().as_secs_f32() * 1000.0;
            // Physics: step after component logic, then write the simulated
            // transforms back onto dynamic/kinematic objects.
            let phys_t = Instant::now();
            if let Some(phys) = self.physics.as_mut()
                && !phys.is_empty()
            {
                phys.step(dt);
                phys.sync_back(&mut self.scene);
            }
            let phys_ms = phys_t.elapsed().as_secs_f32() * 1000.0;
            // Spatialize audio against the listener (moves with components).
            let audio_t = Instant::now();
            if let Some(audio) = self.audio.as_mut() {
                let cues = self.scene.gather_audio();
                let listener = self.scene.audio_listener().unwrap_or(self.camera.position);
                audio.update(&cues, listener);
            }
            let audio_ms = audio_t.elapsed().as_secs_f32() * 1000.0;
            FrameTimings::ema(&mut self.frame_timings.components, comp_ms);
            FrameTimings::ema(&mut self.frame_timings.physics, phys_ms);
            FrameTimings::ema(&mut self.frame_timings.audio, audio_ms);
            // (Per-section CPU breakdown is shown live in the FrameTimings overlay;
            // no periodic log spam.)
            // Apply deferred requests (e.g. ctx.load_scene) after the update
            // pass so the scene isn't swapped mid-iteration.
            self.apply_component_commands(commands);
            // Replicate networked objects (2G): owner sends, others apply.
            if let Some(net) = self.net.as_mut() {
                self.scene.network_sync(net, dt);
            }
            // Voice comms (task 8): push-to-talk capture + spatial playback.
            if self.net.is_some() {
                if self.voice.is_none() {
                    self.voice = crate::voice::VoiceChat::new();
                }
                let transmit = self.input.state().down("Voice");
                let listener = self.scene.audio_listener().unwrap_or(self.camera.position);
                if let (Some(voice), Some(net)) = (self.voice.as_mut(), self.net.as_mut()) {
                    voice.capture_and_send(net, transmit);
                    let owners = net.owners_snapshot();
                    let packets = net.take_voice();
                    let positions = self.scene.peer_voice_positions(&owners);
                    voice.receive(packets, &positions);
                    voice.update(listener, 25.0);
                }
            }
        } else {
            // Coalesce undo entries only while a pointer drag is in progress (one
            // slider/handle gesture = one entry); discrete edits each get their own.
            let gesture_active = self.egui_ctx.input(|i| i.pointer.any_down());
            self.record_edits(pre_edit, gesture_active);
            // Not simulating → decay the play-only timings toward zero.
            FrameTimings::ema(&mut self.frame_timings.components, 0.0);
            FrameTimings::ema(&mut self.frame_timings.physics, 0.0);
            FrameTimings::ema(&mut self.frame_timings.audio, 0.0);
        }
        self.autosave_materials();
        // Cameras always carry a Camera component (covers spawns, loaded
        // scenes, and scenes from before the component existed).
        self.scene.ensure_camera_components(&self.components);
        self.scene.ensure_light_components(&self.components);
        self.scene.ensure_camera_ids();
        // Highlight every multi-selected object (anchor + extras) in the viewport.
        let selected = self.selected_object_indices();
        let draws_t = Instant::now();
        // Apply the MAIN CAMERA's culling mask on top of the viewport's layer toggle,
        // so deselecting a layer in a camera's Culling Mask hides it in the editor too
        // (previously the editor only used the Layers-window toggle, so the per-camera
        // mask did nothing until you ran the game). Restored right after the draw
        // build so the Layers window still edits the toggle, not the intersection.
        let view_toggle = self.scene.visible_layers;
        self.scene.visible_layers = view_toggle & self.scene.main_camera_culling_mask();
        self.scene.sync_draws_multi(&selected, 1.0);
        self.scene.visible_layers = view_toggle;
        FrameTimings::ema(
            &mut self.frame_timings.draws,
            draws_t.elapsed().as_secs_f32() * 1000.0,
        );
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
                baked: false,
            });
        }
        lights.extend(self.scene.gather_lights());
        // Full light set (incl. baked) for the reflection-probe cube capture, so
        // a baked scene's reflections aren't captured in the dark.
        let capture_lights = self.scene.gather_lights_all();
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
            // Skybox IBL (the env-cube specular reflection on metallic/smooth
            // surfaces) is gated by the Enable-skybox toggle: off → metals stop
            // mirroring the sky, so with no lights the scene is black.
            env_intensity: if env.skybox_enabled { 1.0 } else { 0.0 },
        };

        // Render the camera preview only when the Camera tab is actually VISIBLE
        // (the active tab in its dock leaf this frame), or when a camera object is
        // selected (shown as a viewport overlay so its framing can be tweaked
        // live). The tab merely being present in the layout no longer forces the
        // (now Flux-traced) preview to render every frame. Selected camera takes
        // precedence over the main one.
        let selected_camera = match self.selection {
            Selection::Object(i) if self.scene.camera_view_proj_for(i, 1.0).is_some() => Some(i),
            _ => None,
        };
        let preview_view = selected_camera
            .and_then(|i| self.scene.camera_view_proj_for(i, 16.0 / 9.0))
            .or_else(|| {
                self.camera_tab_visible
                    .then(|| self.scene.main_camera_view_proj(16.0 / 9.0))
                    .flatten()
            });
        let camera_preview = preview_view.map(|(view, proj, position)| CameraData {
            view,
            proj,
            position,
        });

        let shadow_res = env.shadow_resolution.clamp(256, 8192);
        let shadow_pcf_texel = env.shadow_softness.max(0.0) / shadow_res as f32;

        // VR: pump the OpenXR session + read tracker poses for the IK solver and
        // controller inputs for locomotion/interaction. Done before binding the
        // renderer so `update_vr` (which mutates the scene/selection) doesn't clash
        // with the renderer borrow held below.
        let mut vr_input = citrus_xr::VrInput::default();
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
            vr_input = session.input();
        }
        if self.xr_session.is_some() {
            self.update_vr(vr_input, dt);
        }

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if let Err(e) = renderer.set_shadow_resolution(shadow_res) {
            tracing::error!("setting shadow resolution: {e:#}");
        }
        // Feed VR tracker poses into the scene's humanoid IK targets (any source
        // can; IK isn't VR-specific). Remap through the captured T-pose calibration
        // when one exists so each tracker drives its bone naturally. Then advance +
        // CPU-skin (previews in-editor).
        let vr_targets = self.scene.vr_apply_calibration(&self.vr_targets);
        self.scene
            .set_ik_targets(self.xr_session.as_ref().map(|_| vr_targets));
        self.scene.update_skinning(renderer, dt);
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
            capture_lights: &capture_lights,
            camera_preview,
            draw_skybox: env.skybox_enabled,
            shadow_pcf_texel,
            shadow_distance: env.shadow_distance,
            time: t,
            draws: &self.scene.draws,
            lightmap_preview: self.lightmap_preview,
            gi_debug: self.gi_debug,
            // The editor renders the scene into the viewport texture (RTT), never
            // the swapchain — so the swapchain pass is clear + egui only.
            render_viewport: false,
            viewport_extent,
            postfx,
            reflection_probe: self.scene.active_reflection_probe(self.camera.position),
            fog: self.scene.fog_params(),
            voxel_specular: self.scene.environment.realtime_gi.mode
                == citrus_assets::GiMode::FluxVoxel
                && self.scene.environment.realtime_gi.voxel_specular,
            egui: Some(citrus_render::EguiDraw {
                pixels_per_point: output.pixels_per_point,
                primitives,
                textures_delta: output.textures_delta,
            }),
        };
        let render_t = Instant::now();
        if let Err(e) = renderer.render(&frame) {
            tracing::error!("render failed: {e:#}");
            event_loop.exit();
        }

        // VR stereo: also render the scene into the headset's per-eye images. Gated
        // on an active XR session, so the desktop editor is unaffected when flat.
        // The eye view = the XR stage-space view composed with the play-space rig
        // (fly/scale/drag), so locomotion moves the player through the world.
        if self.xr_session.is_some() {
            if !self.xr_stereo_ready {
                let fmt = renderer.color_format_raw();
                if let Some(session) = self.xr_session.as_mut() {
                    match session.setup_stereo(fmt) {
                        Ok((w, h)) => {
                            self.xr_stereo_ready = true;
                            tracing::info!("XR stereo swapchains ready ({w}x{h})");
                        }
                        Err(e) => tracing::warn!("XR setup_stereo failed: {e:#}"),
                    }
                }
            }
            if self.xr_stereo_ready {
                let fmt = renderer.color_format_raw();
                let stage_to_world = self.vr_rig.stage_to_world_mat();
                let world_to_stage = stage_to_world.inverse();
                // Controller markers always show (so you can see your tracked
                // hands), even when the hand menu is down. Map the stage-space
                // tracker poses into world space for the eye render.
                let hand_mat = |h: Option<(glam::Vec3, glam::Quat)>| {
                    h.map(|(p, q)| stage_to_world * glam::Mat4::from_rotation_translation(q, p))
                };
                let left_hand = hand_mat(self.vr_targets.left_hand);
                let right_hand = hand_mat(self.vr_targets.right_hand);
                // Per-hand laser endpoint: cast from the controller along its
                // forward and stop at the nearest of {any UI panel, a scene
                // object}, otherwise reach far (2000). Both hands stop on hits.
                let panels = self
                    .vr_overlay_draw
                    .map(|o| o.panels)
                    .unwrap_or([None; citrus_render::VR_MAX_PANELS]);
                let laser_end = |hand: Option<glam::Mat4>| -> Option<glam::Vec3> {
                    let m = hand?;
                    let origin = m.w_axis.truncate();
                    let dir = (-m.z_axis.truncate()).normalize_or(glam::Vec3::NEG_Z);
                    let mut t = 2000.0_f32;
                    for vpanel in panels.iter().flatten() {
                        let pm = vpanel.model;
                        let ppos = pm.w_axis.truncate();
                        let pn = pm.z_axis.truncate().normalize_or(glam::Vec3::Z);
                        if let Some(hit) =
                            citrus_core::ray_plane(citrus_core::Ray { origin, dir }, ppos, pn)
                        {
                            let local = pm.inverse().transform_point3(hit);
                            if local.x.abs() <= 0.5 && local.y.abs() <= 0.5 {
                                t = t.min((hit - origin).length());
                            }
                        }
                    }
                    if let Some(d) = self.scene.ray_hit(origin, dir) {
                        t = t.min(d);
                    }
                    Some(origin + dir * t)
                };
                let left_laser_end = laser_end(left_hand);
                let right_laser_end = laser_end(right_hand);
                let overlay = match self.vr_overlay_draw {
                    Some(mut ov) => {
                        ov.left_hand = left_hand;
                        ov.right_hand = right_hand;
                        ov.left_laser_end = left_laser_end;
                        ov.right_laser_end = right_laser_end;
                        Some(ov)
                    }
                    None => Some(citrus_render::VrOverlayDraw {
                        panels: [None; citrus_render::VR_MAX_PANELS],
                        left_hand,
                        right_hand,
                        left_laser_end,
                        right_laser_end,
                    }),
                };
                // Only re-render the UI texture when the menu (some panel) is up.
                if overlay.is_some_and(|o| o.panels.iter().any(|p| p.is_some())) {
                    if let Some(egui_draw) = frame.egui.as_ref() {
                        if let Err(e) = renderer.render_vr_ui(egui_draw) {
                            tracing::error!("VR UI render: {e:#}");
                        }
                    }
                }
                let begun = self
                    .xr_session
                    .as_mut()
                    .map(|s| s.begin_frame())
                    .unwrap_or(Ok(None));
                match begun {
                    Ok(Some(xr_frame)) => {
                        for eye in &xr_frame.eyes {
                            let world_view = eye.view * world_to_stage;
                            if let Err(e) = renderer.render_xr_eye(
                                eye.image, fmt, eye.width, eye.height, world_view, eye.proj,
                                &frame, overlay,
                            ) {
                                tracing::error!("XR eye render: {e:#}");
                            }
                        }
                        if let Some(session) = self.xr_session.as_mut() {
                            if let Err(e) = session.end_frame(xr_frame) {
                                tracing::error!("XR end_frame: {e:#}");
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::error!("XR begin_frame: {e:#}"),
                }
            }
        }
        FrameTimings::ema(
            &mut self.frame_timings.render,
            render_t.elapsed().as_secs_f32() * 1000.0,
        );

        // The first real editor frame has now been presented, so close the
        // startup splash window (dropping it closes the borderless window).
        if self.splash.is_some() {
            self.splash = None;
        }

        // Profiler window: reconcile open/close (it's a separate OS window) and
        // render its egui UI. Cheap no-op when closed.
        self.update_profiler_window(event_loop);

        // Deferred heavy jobs: this frame already painted the busy overlay, so
        // run the blocking work now (the next frame shows the result + clears it).
        if self.reload_pending {
            self.reload_pending = false;
            self.do_reload_plugins();
        }
        if let Some(job) = self.pending_job.take() {
            match job {
                PendingJob::ReloadPlugins => self.do_reload_plugins(),
            }
        }

        window.request_redraw();
    }
}

/// Dock tab renderer; collects actions for post-UI processing.
struct EditorTabs<'a> {
    /// Set true by `camera_ui` when the Camera tab is the visible/active tab.
    camera_visible: &'a mut bool,
    /// Set true by `viewport_ui` when the Viewport tab is the visible/active tab.
    viewport_visible: &'a mut bool,
    scene: &'a mut LoadedScene,
    selection: &'a mut Selection,
    multi_objects: &'a mut Vec<usize>,
    multi_files: &'a mut Vec<PathBuf>,
    inspector: &'a mut InspectorPanel,
    inspector_lock_target: &'a mut Option<Selection>,
    scene_panel: &'a mut ScenePanel,
    file_browser: &'a mut FileBrowser,
    file_diagnostics: &'a HashMap<PathBuf, (u32, u32)>,
    editor_components: &'a EditorComponents,
    vim_mode: bool,
    file_material: &'a mut Option<FileMaterial>,
    file_meta: &'a mut Option<FileMeta>,
    open_editors: &'a mut Vec<OpenEditor>,
    /// Path of the focused code tab (drives Inspector diagnostics).
    focused_code: Option<PathBuf>,
    gizmo: &'a mut GizmoState,
    widget_filter: &'a mut WidgetFilter,
    /// Viewport render mode (0 lit, 1 world normals, 2 indirect GI, 3 unlit).
    gi_debug: &'a mut u32,
    gizmo_drag: &'a mut bool,
    orbit_armed: &'a mut bool,
    /// True while the camera is actively orbiting (locked pivot); viewport
    /// widgets/gizmos are ignored so a rotate gesture can't grab a handle.
    orbiting: bool,
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
    /// The editor viewport's offscreen render (RTT), painted to fill the tab.
    viewport_texture: Option<(egui::TextureId, [f32; 2])>,
    /// A camera object is selected → draw its preview as a viewport overlay.
    camera_overlay: bool,
    view: glam::Mat4,
    proj: glam::Mat4,
    looking: bool,
    log_filter: &'a mut LogFilter,
    handle_drag: &'a mut Option<GizmoDrag>,
    /// GPU can ray-trace (Baker tab enables its Bake button).
    can_bake: bool,
    /// Baker tab: lightmap-UV checker preview toggle.
    lightmap_preview: &'a mut bool,
    /// Baker tab: model paths whose lightmap UVs were auto-generated (a `.lmuv`
    /// marker exists), so they can be regenerated/reverted. Computed per frame.
    generated_uv_models: &'a [String],
    /// Inspector: selected material slot of the current object (multi-material).
    selected_material_slot: &'a mut usize,
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
            Tab::Baker => "FluxBaker".into(),
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
                // Sync the panel's multi-selection from the engine's canonical set
                // so highlighting + ctrl/shift behaviour reflect the live state.
                self.scene_panel.set_multi(self.multi_objects.iter().copied());
                let response = self.scene_panel.ui(ui, &rows, &mut selected);
                if response.selection_changed {
                    *self.multi_objects = self.scene_panel.multi();
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
                if let Some(path) = response.import_model {
                    self.actions.push(EditorAction::ImportModel(path));
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
                        SpawnKind::PostFxVolume => {
                            self.actions.push(EditorAction::SpawnPostFxVolume);
                            return;
                        }
                        SpawnKind::ReflectionProbe => {
                            self.actions.push(EditorAction::SpawnReflectionProbe);
                            return;
                        }
                        SpawnKind::FluxVolume => {
                            self.actions.push(EditorAction::SpawnFluxVolume);
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
                self.file_browser.set_multi(self.multi_files.iter().cloned());
                let response = self
                    .file_browser
                    .ui(ui, selected.as_deref(), self.file_diagnostics);
                if let Some(path) = response.clicked {
                    // Single click just selects (Inspector shows file info). The
                    // browser tracked the ctrl/shift multi-selection internally.
                    *self.multi_files = self.file_browser.multi();
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
                if response.clear_selection {
                    // Clicked empty space in the grid: drop the file selection so
                    // the Inspector clears (matches the scene-tree behaviour).
                    self.multi_files.clear();
                    if matches!(self.selection, Selection::File(_)) {
                        *self.selection = Selection::None;
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

    /// Unified component-gizmo interaction: grab a box-face or range-radius
    /// handle (from any component's `gizmos()`), suppress orbit for the gesture,
    /// and write the resize back via `apply_gizmo_edit`. Replaces the per-type
    /// probe/collider/audio handlers; FluxVolume/ReflectionProbe and plugins get
    /// working resize handles for free.
    fn gizmo_interaction(
        &mut self,
        response: &egui::Response,
        cursor: Option<egui::Pos2>,
        alt: bool,
    ) {
        let full_rect = *self.viewport_rect;

        // Continue / end an in-progress drag.
        if let Some(drag) = self.handle_drag.as_ref() {
            if !response.dragged_by(egui::PointerButton::Primary) {
                *self.handle_drag = None;
                return;
            }
            let Some(cur) = cursor else { return };
            let delta = cur - drag.start_cursor;
            let (object, component, gizmo) = (drag.object, drag.component, drag.gizmo);
            match &drag.kind {
                GizmoDragKind::BoxFace {
                    axis,
                    sign,
                    object_anchored,
                    start_size,
                    start_center,
                    start_origin_world,
                    world_axis,
                    scale_a,
                    screen_axis,
                } => {
                    let (axis, sign, object_anchored) = (*axis, *sign, *object_anchored);
                    let (start_size, start_center, start_origin_world, world_axis, scale_a) =
                        (*start_size, *start_center, *start_origin_world, *world_axis, *scale_a);
                    let len2 = screen_axis.length_sq();
                    let meters_world = if len2 > 1.0e-6 {
                        delta.dot(*screen_axis) / len2
                    } else {
                        0.0
                    };
                    let local_delta = if scale_a.abs() > 1.0e-5 {
                        meters_world / scale_a
                    } else {
                        0.0
                    };
                    let new_size_a = (start_size[axis] + local_delta).max(0.1);
                    let applied = new_size_a - start_size[axis];
                    let mut new_size = start_size;
                    new_size[axis] = new_size_a;
                    let new_half = new_size * 0.5;
                    if object_anchored {
                        // Move the object so the opposite face stays put (parent-aware).
                        let new_origin = start_origin_world + world_axis * (applied * scale_a * 0.5);
                        let parent_world = self.scene.objects[object]
                            .parent
                            .map_or(glam::Mat4::IDENTITY, |p| self.scene.world_transform(p));
                        let new_translation = parent_world.inverse().transform_point3(new_origin);
                        if let Some(c) = self.scene.objects[object].components.get_mut(component) {
                            self.editor_components.apply_gizmo_edit(
                                c.as_mut(),
                                citrus_editor::GizmoEdit::Box {
                                    index: gizmo,
                                    half_extents: new_half,
                                    center: start_center,
                                },
                            );
                        }
                        self.scene.objects[object].translation = new_translation;
                    } else {
                        let mut axv = Vec3::ZERO;
                        axv[axis] = sign * (applied * 0.5);
                        let new_center = start_center + axv;
                        if let Some(c) = self.scene.objects[object].components.get_mut(component) {
                            self.editor_components.apply_gizmo_edit(
                                c.as_mut(),
                                citrus_editor::GizmoEdit::Box {
                                    index: gizmo,
                                    half_extents: new_half,
                                    center: new_center,
                                },
                            );
                        }
                    }
                }
                GizmoDragKind::Range {
                    start_radius,
                    screen_axis,
                } => {
                    let len2 = screen_axis.length_sq();
                    let meters_world = if len2 > 1.0e-6 {
                        delta.dot(*screen_axis) / len2
                    } else {
                        0.0
                    };
                    let new_radius = (start_radius + meters_world).max(0.0);
                    if let Some(c) = self.scene.objects[object].components.get_mut(component) {
                        self.editor_components.apply_gizmo_edit(
                            c.as_mut(),
                            citrus_editor::GizmoEdit::Range {
                                index: gizmo,
                                radius: new_radius,
                            },
                        );
                    }
                }
            }
            return;
        }

        // Maybe start a drag: a primary press on a handle (gizmo wins over orbit;
        // the transform gizmo still wins over us).
        if !response.drag_started_by(egui::PointerButton::Primary) || alt {
            return;
        }
        let Selection::Object(i) = *self.selection else {
            return;
        };
        if !self.scene.is_active(i) {
            return;
        }
        let Some(press) = cursor else { return };
        if self.gizmo.pick_preview(press) {
            return; // the move/rotate/scale gizmo owns this press
        }
        let full_rect2 = full_rect;
        let world = self.scene.world_transform(i);
        let (w_scale, w_rot, w_trans) = world.to_scale_rotation_translation();
        let axes = [Vec3::X, Vec3::Y, Vec3::Z];
        let mut best: Option<(f32, GizmoDrag)> = None;
        for (ci, comp) in self.scene.objects[i].components.iter().enumerate() {
            for (gi, spec) in self.editor_components.gizmos(comp.as_ref()).into_iter().enumerate() {
                match spec {
                    citrus_editor::GizmoSpec::Box {
                        center,
                        half_extents,
                        object_anchored,
                        ..
                    } => {
                        for axis in 0..3 {
                            for sign in [-1.0f32, 1.0] {
                                let face_local = center + axes[axis] * (sign * half_extents[axis]);
                                let face_world = world.transform_point3(face_local);
                                let Some(fs) = self.world_to_screen(face_world, full_rect2) else {
                                    continue;
                                };
                                let world_axis = (w_rot * axes[axis]) * sign;
                                let Some(plus) =
                                    self.world_to_screen(face_world + world_axis, full_rect2)
                                else {
                                    continue;
                                };
                                let dist = handle_pick_dist(press, fs, plus - fs, 26.0);
                                if dist > 12.0 {
                                    continue;
                                }
                                if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                                    best = Some((
                                        dist,
                                        GizmoDrag {
                                            object: i,
                                            component: ci,
                                            gizmo: gi,
                                            start_cursor: press,
                                            kind: GizmoDragKind::BoxFace {
                                                axis,
                                                sign,
                                                object_anchored,
                                                start_size: half_extents * 2.0,
                                                start_center: center,
                                                start_origin_world: w_trans,
                                                world_axis,
                                                scale_a: w_scale[axis],
                                                screen_axis: plus - fs,
                                            },
                                        },
                                    ));
                                }
                            }
                        }
                    }
                    citrus_editor::GizmoSpec::Range { center, radius, .. } => {
                        let origin = world.transform_point3(center);
                        let right = self.camera_right();
                        let handle = origin + right * radius;
                        let Some(hs) = self.world_to_screen(handle, full_rect2) else {
                            continue;
                        };
                        let outward = self
                            .world_to_screen(handle + right, full_rect2)
                            .map(|p| p - hs)
                            .unwrap_or(egui::vec2(1.0, 0.0));
                        let dist = handle_pick_dist(press, hs, outward, 26.0);
                        if dist > 12.0 {
                            continue;
                        }
                        if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                            best = Some((
                                dist,
                                GizmoDrag {
                                    object: i,
                                    component: ci,
                                    gizmo: gi,
                                    start_cursor: press,
                                    kind: GizmoDragKind::Range {
                                        start_radius: radius,
                                        screen_axis: outward,
                                    },
                                },
                            ));
                        }
                    }
                    citrus_editor::GizmoSpec::Points { .. } => {}
                }
            }
        }
        if let Some((_, drag)) = best {
            *self.handle_drag = Some(drag);
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
        // (color, timestamp, rest): split so wrapped lines can hang-indent
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
        // The Camera tab is on screen this frame, so the preview is worth
        // rendering (the gate read after the dock pass in `redraw`).
        *self.camera_visible = true;
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
    /// "FluxBaker": lighting-bake settings + Bake / Clear actions.
    fn baker_ui(&mut self, ui: &mut egui::Ui) {
        use egui::{DragValue, RichText, Slider};
        ui.heading("FluxBaker");
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
                            for s in [128u32, 256, 512, 1024, 2048, 4096] {
                                ui.selectable_value(&mut bake.max_lightmap, s, format!("{s}"));
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("GPU Throttle");
                    ui.add(Slider::new(&mut bake.gpu_throttle, 0.0..=2.0).step_by(0.1))
                        .on_hover_text(
                            "Idle the GPU for this fraction of each bake dispatch so the desktop \
                             stays responsive while baking. 0 = fastest bake (hogs the GPU, can \
                             freeze the machine); 1 ≈ 50% GPU duty (≈2× slower, system usable).",
                        );
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
                ui.label(RichText::new("Lightmap UVs").strong());
                let needing = self.scene.models_needing_lightmap_uv();
                if !needing.is_empty() {
                    ui.label(
                        RichText::new(format!(
                            "{} model(s) have no usable lightmap UV (the bake skips them):",
                            needing.len()
                        ))
                        .small()
                        .weak(),
                    );
                    for m in &needing {
                        ui.label(RichText::new(format!("  • {m}")).small());
                    }
                    if ui
                        .button("⚙ Generate lightmap UVs for these")
                        .on_hover_text(
                            "Auto-unwrap a non-overlapping lightmap UV for each model that needs \
                             one (saved as a .lmuv marker; reloads the scene).",
                        )
                        .clicked()
                    {
                        self.actions.push(EditorAction::GenerateAllLightmapUvs);
                    }
                }
                if !self.generated_uv_models.is_empty() {
                    ui.label(RichText::new("Generated (auto-unwrapped):").small().strong());
                    for m in self.generated_uv_models {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("• {m}")).small());
                            if ui
                                .small_button("Revert")
                                .on_hover_text("Remove the generated UVs; use the model's own uv0.")
                                .clicked()
                            {
                                self.actions
                                    .push(EditorAction::SetLightmapUvGen(m.clone(), false));
                            }
                        });
                    }
                    if ui
                        .button("↻ Regenerate all")
                        .on_hover_text(
                            "Re-run the unwrapper on every generated model (picks up unwrapper \
                             improvements). Reloads the scene.",
                        )
                        .clicked()
                    {
                        self.actions.push(EditorAction::RegenerateLightmapUvs);
                    }
                }
                if needing.is_empty() && self.generated_uv_models.is_empty() {
                    ui.label(
                        RichText::new("All models have usable lightmap UVs.").small().weak(),
                    );
                }

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
        // Reflection probe baking: capture the active probe's cubemap and persist
        // it as a `.reflprobe` sidecar (loaded on scene open). Disabled until the
        // scene has a Reflection Probe (add one from the hierarchy → Light menu).
        let has_probe = self
            .scene
            .active_reflection_probe(glam::Vec3::ZERO)
            .is_some();
        ui.horizontal(|ui| {
            let refl_btn = egui::Button::new(RichText::new("✨ Bake Reflections").strong());
            if ui
                .add_enabled(has_probe, refl_btn)
                .on_hover_text(
                    "Capture the active Reflection Probe's cubemap and save it as a \
                     .reflprobe sidecar (loaded on scene open)",
                )
                .clicked()
            {
                self.actions.push(EditorAction::BakeReflections);
            }
            if ui
                .add_enabled(has_probe, egui::Button::new("Recapture"))
                .on_hover_text(
                    "Re-render the active probe's cubemap from the current scene \
                     (this session only, not saved)",
                )
                .clicked()
            {
                self.actions.push(EditorAction::RecaptureReflections);
            }
        });
        if !has_probe {
            ui.label(
                RichText::new("Add a Reflection Probe (hierarchy → Light) to bake reflections.")
                    .small()
                    .weak(),
            );
        }
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
                    ui.label(RichText::new("Flux GI (realtime)").strong());
                    let gi = &mut env.realtime_gi;
                    // Settings stay EDITABLE even with a baked lightmap loaded: the
                    // FluxVoxel backend runs alongside a bake (the baked voxels are its
                    // static base), and for the other backends the values you set here
                    // take effect the moment the bake is cleared. (Previously the whole
                    // section was disabled when baked, so you couldn't tweak anything.)
                    ui.add_enabled_ui(true, |ui| {
                        ui.checkbox(&mut gi.enabled, "Enabled").on_hover_text(
                            "Flux: realtime global illumination. Screen-space probes \
                             march the scene distance field every frame for live indirect bounce \
                             — no bake, runs anywhere (no RT cores needed).",
                        );
                        ui.add_enabled_ui(gi.enabled, |ui| {
                            // Backend first: it decides which settings below apply.
                            // Each control is gated by `gi.mode.uses_*()` so the panel
                            // only shows what the chosen backend actually consumes
                            // (modular settings — FluxVoxel hides bounces/quality/etc).
                            ui.horizontal(|ui| {
                                ui.label("Backend");
                                egui::ComboBox::from_id_salt("citrus-flux-mode")
                                    .selected_text(gi.mode.label())
                                    .show_ui(ui, |ui| {
                                        for m in citrus_assets::GiMode::ALL {
                                            ui.selectable_value(&mut gi.mode, m, m.label());
                                        }
                                    });
                            })
                            .response
                            .on_hover_text(
                                "Trace backend (NOT auto): Flux marches the SDF (runs anywhere); \
                                 FluxRT ray-queries the scene BVH for exact geometry (needs a \
                                 ray-query GPU, falls back to Flux); FluxVoxel injects lights into \
                                 an analytic voxel volume — cheapest, no ray tracing, for VR / \
                                 low-end. Settings below adapt to the backend.",
                            );
                            let mode = gi.mode;
                            ui.horizontal(|ui| {
                                ui.label("Intensity");
                                ui.add(egui::Slider::new(&mut gi.intensity, 0.0..=4.0))
                                    .on_hover_text("Indirect bounce strength multiplier.");
                            });
                            if mode.uses_quality() {
                                ui.horizontal(|ui| {
                                    ui.label("Quality");
                                    egui::ComboBox::from_id_salt("citrus-flux-quality")
                                        .selected_text(gi.quality.label())
                                        .show_ui(ui, |ui| {
                                            for q in citrus_assets::FluxQuality::ALL {
                                                ui.selectable_value(&mut gi.quality, q, q.label());
                                            }
                                        });
                                })
                                .response
                                .on_hover_text(
                                    "Rays per screen probe each frame. Temporal accumulation \
                                     smooths the rest, so even Performance stays clean once settled.",
                                );
                            }
                            if mode.uses_bounces() {
                                ui.horizontal(|ui| {
                                    ui.label("Bounces");
                                    ui.add(egui::Slider::new(&mut gi.bounces, 1..=4)).on_hover_text(
                                        "Indirect bounces per ray (more = softer fill).",
                                    );
                                });
                            }
                            ui.horizontal(|ui| {
                                ui.label("Smoothing");
                                ui.add(egui::Slider::new(&mut gi.smoothing, 0.0..=1.0))
                                    .on_hover_text(
                                        "Temporal stability: higher = smoother but more motion lag; \
                                         lower = sharper/more responsive but noisier while moving.",
                                    );
                            });
                            // FluxVoxel-only controls: density of the auto scene grid +
                            // the expensive toggles (DDGI occlusion). All ON by default.
                            if mode.uses_voxel_density() {
                                ui.horizontal(|ui| {
                                    ui.label("Voxel density");
                                    ui.add(
                                        egui::Slider::new(&mut gi.voxel_density, 0.25..=4.0)
                                            .suffix(" /m"),
                                    )
                                    .on_hover_text(
                                        "Probes per world meter for the auto scene-covering grid \
                                         (used when there are no FluxVolumes). Higher = finer GI, \
                                         more cost. Per-FluxVolume density is set on each volume.",
                                    );
                                });
                                ui.checkbox(&mut gi.voxel_auto_grid, "Auto scene grid")
                                    .on_hover_text(
                                        "Build a grid covering the scene when no FluxVolumes are \
                                         placed, so everything gets voxel GI. Mutually exclusive \
                                         with placed FluxVolumes: placing any volume turns this off \
                                         and the author then controls coverage.",
                                    );
                                if gi.voxel_auto_grid {
                                    ui.horizontal(|ui| {
                                        ui.label("Grid mode");
                                        egui::ComboBox::from_id_salt("citrus-voxel-gridmode")
                                            .selected_text(gi.voxel_grid_mode.label())
                                            .show_ui(ui, |ui| {
                                                for m in citrus_assets::VoxelGridMode::ALL {
                                                    ui.selectable_value(
                                                        &mut gi.voxel_grid_mode,
                                                        m,
                                                        m.label(),
                                                    );
                                                }
                                            });
                                    })
                                    .response
                                    .on_hover_text(
                                        "Auto-grid layout:\n\
                                         • Whole scene: one box over everything (cost scales with level size).\n\
                                         • Camera clipmap: fixed cube following the camera — constant cost, best for big levels.\n\
                                         • Occupancy culled: tight bounds around geometry.\n\
                                         • Per object: tight grids hugging object clusters, gaps left uncovered.",
                                    );
                                    if gi.voxel_grid_mode
                                        == citrus_assets::VoxelGridMode::CameraClipmap
                                    {
                                        ui.horizontal(|ui| {
                                            ui.label("Clipmap extent");
                                            ui.add(
                                                egui::Slider::new(
                                                    &mut gi.voxel_clipmap_extent,
                                                    4.0..=64.0,
                                                )
                                                .suffix(" m"),
                                            )
                                            .on_hover_text(
                                                "Half-size of the camera-following cube; the grid is \
                                                 2× this on a side, centred on the camera.",
                                            );
                                        });
                                    }
                                }
                                ui.checkbox(&mut gi.voxel_ddgi_occlusion, "DDGI occlusion")
                                    .on_hover_text(
                                        "Block voxel lights with geometry (cheap DDGI-style shadows \
                                         from a coarse scene occupancy grid). Turn off on low-end / \
                                         VR to save the per-probe marches.",
                                    );
                                ui.checkbox(&mut gi.voxel_propagation, "Light propagation")
                                    .on_hover_text(
                                        "LPV-style diffuse bounce: spread injected light through \
                                         the voxel grid so it fills shadowed pockets (≥1 bounce). \
                                         Off = direct-only (cheapest).",
                                    );
                                ui.checkbox(&mut gi.voxel_specular, "Specular GI")
                                    .on_hover_text(
                                        "Metallic / rough surfaces sample the voxel volume in the \
                                         reflection direction (VXGI-style glossy) so they pick up \
                                         emissive + voxel-light bounce, not just the reflection cube.",
                                    );
                            }
                            egui::CollapsingHeader::new("Advanced")
                                .id_salt("citrus-flux-advanced")
                                .show(ui, |ui| {
                                    if mode.uses_gdf() {
                                        ui.horizontal(|ui| {
                                            ui.label("GDF resolution");
                                            egui::ComboBox::from_id_salt("citrus-flux-gdfres")
                                                .selected_text(format!("{}³", gi.gdf_resolution))
                                                .show_ui(ui, |ui| {
                                                    for r in [64u32, 128, 256] {
                                                        ui.selectable_value(
                                                            &mut gi.gdf_resolution,
                                                            r,
                                                            format!("{r}³"),
                                                        );
                                                    }
                                                });
                                        });
                                    }
                                    if mode.uses_march_distance() {
                                        ui.horizontal(|ui| {
                                            ui.label("March distance");
                                            ui.add(
                                                egui::Slider::new(&mut gi.march_distance, 0.0..=500.0)
                                                    .suffix(" m"),
                                            )
                                            .on_hover_text(
                                                "Max trace distance (0 = auto from scene size).",
                                            );
                                        });
                                    }
                                    if mode.uses_samples() {
                                        ui.horizontal(|ui| {
                                            ui.label("Firefly clamp");
                                            ui.add(egui::Slider::new(
                                                &mut gi.firefly_clamp,
                                                0.5..=16.0,
                                            ))
                                            .on_hover_text(
                                                "Caps bright bounce outliers (lower = calmer).",
                                            );
                                        });
                                    }
                                    ui.horizontal(|ui| {
                                        ui.label("Probe fallback");
                                        egui::ComboBox::from_id_salt("citrus-flux-fallback")
                                            .selected_text(gi.probe_fallback.label())
                                            .show_ui(ui, |ui| {
                                                for f in citrus_assets::ProbeFallback::ALL {
                                                    ui.selectable_value(
                                                        &mut gi.probe_fallback,
                                                        f,
                                                        f.label(),
                                                    );
                                                }
                                            });
                                    });
                                });
                        });
                    });
                    if baked {
                        let note = if gi.mode == citrus_assets::GiMode::FluxVoxel {
                            "FluxVoxel runs alongside the bake (baked voxels are its static base) \
                             — these settings are live."
                        } else {
                            "A bake is loaded, so Flux/FluxRT realtime GI is paused; these settings \
                             apply when you Clear the bake. (FluxVoxel runs with a bake.)"
                        };
                        ui.label(RichText::new(note).small().weak());
                    }

                    // Reflections: a dedicated Environment section (NOT under GI),
                    // matching Unreal where reflection method is a global scene
                    // setting, not a per-material toggle. Type = how specular
                    // reflections are produced for the whole scene.
                    ui.separator();
                    egui::CollapsingHeader::new("Reflections")
                        .id_salt("citrus-env-reflections")
                        .default_open(true)
                        .show(ui, |ui| {
                            let gi = &mut env.realtime_gi;
                            ui.horizontal(|ui| {
                                ui.label("Type");
                                let label = match gi.reflection_mode {
                                    1 => "Screen-space (SSR)",
                                    2 => "Ray-traced",
                                    _ => "Reflection Probes",
                                };
                                egui::ComboBox::from_id_salt("citrus-refl-type")
                                    .selected_text(label)
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut gi.reflection_mode, 0, "Reflection Probes")
                                            .on_hover_text(
                                                "Box-projected cubemap probes, with the env / \
                                                 skylight cube as the fallback. Cheapest, works \
                                                 everywhere.",
                                            );
                                        ui.selectable_value(&mut gi.reflection_mode, 1, "Screen-space (SSR)")
                                            .on_hover_text(
                                                "Screen-space reflections layered over the probe / \
                                                 env base; off-screen rays fall back to the probes.",
                                            );
                                        ui.selectable_value(&mut gi.reflection_mode, 2, "Ray-traced")
                                            .on_hover_text(
                                                "Traces real geometry (needs a ray-query GPU; \
                                                 falls back to SSR if unavailable).",
                                            );
                                    });
                            });
                            // Intensity / distance / roughness tune SSR + Ray-traced.
                            gi.ssr_enabled = gi.reflection_mode == 1;
                            ui.add_enabled_ui(gi.reflection_mode != 0, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Intensity");
                                    ui.add(egui::Slider::new(&mut gi.ssr_intensity, 0.0..=2.0))
                                        .on_hover_text("Reflection strength multiplier.");
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Max distance");
                                    ui.add(
                                        egui::Slider::new(&mut gi.ssr_max_distance, 1.0..=200.0)
                                            .suffix(" m"),
                                    )
                                    .on_hover_text("Max view-space ray distance.");
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Roughness cutoff");
                                    ui.add(egui::Slider::new(&mut gi.ssr_roughness_cutoff, 0.0..=1.0))
                                        .on_hover_text(
                                            "Surfaces rougher than this skip SSR (the probe / env \
                                             cube carries them instead).",
                                        );
                                });
                            });
                            ui.separator();
                            ui.label(RichText::new("Reflection Probes").small().strong());
                            ui.label(
                                RichText::new(
                                    "Add a Reflection Probe component to an object for a \
                                     box-projected cubemap. The highest-priority probe covering a \
                                     surface is used (higher Importance wins, then smaller box), \
                                     fading to the skybox at the box edge. Recapture after editing \
                                     the scene.",
                                )
                                .small()
                                .weak(),
                            );
                            ui.horizontal(|ui| {
                                if ui
                                    .button("Recapture")
                                    .on_hover_text(
                                        "Re-render the active probe's cubemap from the current \
                                         scene (this session only).",
                                    )
                                    .clicked()
                                {
                                    self.actions.push(EditorAction::RecaptureReflections);
                                }
                                if ui
                                    .button("Bake to disk")
                                    .on_hover_text(
                                        "Recapture, then persist the cube as a .reflprobe sidecar \
                                         (loaded on scene open as a baked reflection probe).",
                                    )
                                    .clicked()
                                {
                                    self.actions.push(EditorAction::BakeReflections);
                                }
                            });
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
                    egui::CollapsingHeader::new("Fog")
                        .id_salt("citrus-env-fog")
                        .show(ui, |ui| {
                            ui.checkbox(&mut env.fog_enabled, "Enabled")
                                .on_hover_text("Exponential distance + height fog (atmospheric depth).");
                            ui.add_enabled_ui(env.fog_enabled, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Color");
                                    ui.color_edit_button_rgb(&mut env.fog_color);
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Density");
                                    ui.add(egui::Slider::new(&mut env.fog_density, 0.0..=0.5).logarithmic(true));
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Start distance");
                                    ui.add(egui::Slider::new(&mut env.fog_start_distance, 0.0..=100.0).suffix(" m"));
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Height falloff");
                                    ui.add(egui::Slider::new(&mut env.fog_height_falloff, 0.0..=1.0))
                                        .on_hover_text("Fog thins above the height reference (0 = uniform).");
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Height ref");
                                    ui.add(egui::Slider::new(&mut env.fog_height_ref, -50.0..=50.0).suffix(" m"));
                                });
                            });
                        });

                    ui.separator();
                    egui::CollapsingHeader::new("Post-processing (global)")
                        .id_salt("citrus-env-postfx")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(
                                    "Always-applied scene post FX (the global volume). Local Volume \
                                     components blend on top by priority/weight.",
                                )
                                .small()
                                .weak(),
                            );
                            postfx_editor_ui(ui, &mut env.postfx);
                        });
                }

                // ---- Skybox section: enable toggle + the texture sources (a single
                // 360° equirect image, OR a 6-face cubemap that overrides it). The
                // toggle also gates whether the skybox lights the scene (IBL): off →
                // metals stop reflecting it, so with no lights the scene is black.
                ui.separator();
                ui.label(RichText::new("Skybox").strong());
                ui.checkbox(&mut self.scene.environment.skybox_enabled, "Enable skybox")
                    .on_hover_text(
                        "Draw the skybox AND let it light the scene (image-based lighting): \
                         metallic/smooth surfaces reflect it. Off = no skybox reflection, so \
                         with no lights the scene is black.",
                    );
                let is_image = |p: &std::path::Path| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase())
                        .is_some_and(|e| {
                            matches!(e.as_str(), "png" | "jpg" | "jpeg" | "bmp" | "tga" | "hdr" | "exr")
                        })
                };

                // 360° equirectangular image slot (used when no full cubemap is set).
                let slot = egui::Frame::group(ui.style())
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label("360° image (equirectangular)");
                        match &self.scene.skybox {
                            Some(path) => {
                                ui.label(RichText::new(path).small().weak());
                            }
                            None => {
                                ui.label(
                                    RichText::new("Procedural gradient — drop an image here")
                                        .small()
                                        .weak(),
                                );
                            }
                        }
                    })
                    .response;
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
                if self.scene.skybox.is_some() && ui.button("Clear 360° image").clicked() {
                    self.actions.push(EditorAction::ClearSkybox);
                }

                // 6-face cubemap. When all six faces are assigned they OVERRIDE the
                // 360° image; a partial set falls back to the equirect/procedural sky.
                egui::CollapsingHeader::new("Cubemap (6 faces)")
                    .id_salt("citrus-skybox-cube")
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(
                                "Drop an image on each face. All six together override the \
                                 360° image above.",
                            )
                            .small()
                            .weak(),
                        );
                        const FACE_LABELS: [&str; 6] =
                            ["+X right", "-X left", "+Y top", "-Y bottom", "+Z front", "-Z back"];
                        for i in 0..6 {
                            let assigned = self
                                .scene
                                .environment
                                .skybox_faces
                                .as_ref()
                                .map(|f| f[i].clone())
                                .filter(|s| !s.is_empty());
                            let face = egui::Frame::group(ui.style())
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(FACE_LABELS[i]);
                                        match &assigned {
                                            Some(p) => {
                                                ui.label(RichText::new(p).small().weak());
                                            }
                                            None => {
                                                ui.label(
                                                    RichText::new("drop image")
                                                        .small()
                                                        .weak(),
                                                );
                                            }
                                        }
                                    });
                                })
                                .response;
                            if face
                                .dnd_hover_payload::<std::path::PathBuf>()
                                .is_some_and(|p| is_image(&p))
                            {
                                ui.painter().rect_stroke(
                                    face.rect,
                                    4.0,
                                    egui::Stroke::new(
                                        2.0,
                                        ui.visuals().selection.stroke.color,
                                    ),
                                    egui::StrokeKind::Outside,
                                );
                            }
                            if let Some(p) = face.dnd_release_payload::<std::path::PathBuf>()
                                && is_image(&p)
                            {
                                self.actions.push(EditorAction::SetSkyboxFace(i, (*p).clone()));
                            }
                        }
                        if self.scene.environment.skybox_faces.is_some()
                            && ui.button("Clear faces").clicked()
                        {
                            self.actions.push(EditorAction::ClearSkyboxFaces);
                        }
                    });

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
        // The Viewport tab is on screen this frame, so the main scene render is
        // worth doing (gate read after the dock pass in `redraw`).
        *self.viewport_visible = true;
        let rect = ui.max_rect();
        *self.viewport_rect = rect;
        // Paint the editor viewport's offscreen render (RTT) filling the tab; the
        // gizmo + billboards draw on top. (One frame old, like any egui texture.)
        if let Some((tex, _)) = self.viewport_texture {
            ui.painter().image(
                tex,
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        let response = ui.interact(
            rect,
            ui.id().with("viewport-interact"),
            egui::Sense::click_and_drag(),
        );

        // Right mouse over the viewport starts mouse-look. Detected through
        // this widget (not raw winit) so egui's hit-testing decides; clicks
        // on panels, tab bars, or resize handles never reach us.
        if response.hovered()
            && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary))
        {
            self.actions.push(EditorAction::StartLook);
        }

        // Scroll dollies the camera only while the pointer is over the viewport
        // AND the viewport widget actually has the pointer (not occluded by a
        // floating window). The winit rect test alone let scroll bleed through
        // windows sitting over the viewport; `contains_pointer()` is
        // occlusion-aware (same reason clicks use `hovered()` above), so a window
        // over the cursor now swallows the wheel like it swallows clicks.
        if self.pointer_in_viewport && response.contains_pointer() {
            let scroll = ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                self.actions.push(EditorAction::Dolly(scroll / 50.0));
            }
        }

        // Left drag orbits (around the selection, or whatever sits at the
        // viewport center) unless the gizmo grabbed the drag. Alt forces
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

        // While the camera is actively orbiting, ignore every viewport widget so
        // a rotate gesture can't grab a probe/collider/audio handle or the gizmo
        // as the cursor sweeps across them.
        if !self.orbiting && self.widget_filter.enabled {
            // Light Probe Volume box-resize: dragging a face handle changes the
            // volume's size along that axis, keeping the opposite face fixed. Run
            // before the transform gizmo so a grabbed handle wins the drag; the
            // gizmo still moves/rotates the object when no handle is grabbed.
            self.gizmo_interaction(&response, cursor, alt);
        }
        let probe_active = self.handle_drag.is_some();

        if !self.orbiting && self.widget_filter.enabled && !probe_active && let Selection::Object(i) = *self.selection {
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
                // The scene now renders into the viewport texture that fills this
                // tab (RTT), so the gizmo's NDC mapping uses the tab rect.
                gizmo_changed = self.gizmo.interact(
                    ui,
                    rect,
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
        if self.widget_filter.enabled {
            let selected = match self.selection {
                Selection::Object(i) => Some(*i),
                _ => None,
            };
            let view_proj = self.proj * self.view;
            let full_rect = *self.viewport_rect;
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
                // vanishing), then against the screen rect; near-plane
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

        // Component gizmos for the selected object: resizable boxes (with face
        // handles + optional blend shell), interior point clouds, and range
        // spheres — each component declares its set via `gizmos()` and they are
        // ALL drawn here uniformly (built-ins and plugins alike).
        let gizmo_sel = match self.selection {
            Selection::Object(i) => Some(*i),
            _ => None,
        };
        if let Some(sel) = gizmo_sel
            && self.scene.is_active(sel)
        {
            let view_proj = self.proj * self.view;
            let full_rect = *self.viewport_rect;
            let painter = ui.painter();
            const W_EPS: f32 = 0.001;
            let to_screen = |clip: glam::Vec4| -> egui::Pos2 {
                let ndc = clip.truncate() / clip.w;
                egui::pos2(
                    full_rect.left() + (ndc.x * 0.5 + 0.5) * full_rect.width(),
                    full_rect.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * full_rect.height(),
                )
            };
            let project = |p: Vec3| -> Option<egui::Pos2> {
                let clip = view_proj * p.extend(1.0);
                (clip.w > W_EPS).then(|| to_screen(clip))
            };
            let world = self.scene.world_transform(sel);
            let cam_pos = self.view.inverse().w_axis.truncate();
            let line = |a: Vec3, b: Vec3, stroke: egui::Stroke| {
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
                if let Some(s) =
                    clip_segment_to_rect(to_screen(ca), to_screen(cb), full_rect.expand(40.0))
                {
                    painter.line_segment(s, stroke);
                }
            };
            // Depth-aware line: subdivide the edge and dim the parts whose
            // midpoint sits behind scene geometry (ray-AABB occlusion against the
            // other objects), so a box reads as "behind something" where hidden.
            let occ_line = |a: Vec3, b: Vec3, stroke: egui::Stroke| {
                let dim = egui::Stroke::new(
                    stroke.width,
                    egui::Color32::from_rgb(
                        stroke.color.r() / 3,
                        stroke.color.g() / 3,
                        stroke.color.b() / 3,
                    ),
                );
                const SUB: usize = 10;
                for k in 0..SUB {
                    let t0 = k as f32 / SUB as f32;
                    let t1 = (k + 1) as f32 / SUB as f32;
                    let mid = a.lerp(b, (t0 + t1) * 0.5);
                    let d = mid - cam_pos;
                    let dist = d.length();
                    let occluded = dist > 1.0e-3
                        && self
                            .scene
                            .ray_hit(cam_pos, d / dist)
                            .is_some_and(|h| h < dist - 0.05);
                    line(a.lerp(b, t0), a.lerp(b, t1), if occluded { dim } else { stroke });
                }
            };
            let box_edges = |center: Vec3, half: Vec3, stroke: egui::Stroke| {
                let corner = |sx: f32, sy: f32, sz: f32| {
                    world.transform_point3(
                        center + Vec3::new(half.x * sx, half.y * sy, half.z * sz),
                    )
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
                    occ_line(c[k], c[(k + 1) % 4], stroke);
                    occ_line(c[k + 4], c[(k + 1) % 4 + 4], stroke);
                    occ_line(c[k], c[k + 4], stroke);
                }
            };
            let axes = [Vec3::X, Vec3::Y, Vec3::Z];
            for comp in &self.scene.objects[sel].components {
                for spec in self.editor_components.gizmos(comp.as_ref()) {
                    match spec {
                        citrus_editor::GizmoSpec::Box {
                            center,
                            half_extents,
                            blend,
                            color,
                            ..
                        } => {
                            box_edges(center, half_extents, egui::Stroke::new(1.5, color));
                            if blend > 0.001 {
                                let inner = Vec3::new(
                                    (half_extents.x - blend).max(0.0),
                                    (half_extents.y - blend).max(0.0),
                                    (half_extents.z - blend).max(0.0),
                                );
                                let dim = egui::Color32::from_rgb(
                                    color.r() / 2,
                                    color.g() / 2,
                                    color.b() / 2,
                                );
                                box_edges(center, inner, egui::Stroke::new(1.0, dim));
                            }
                            let center_screen = project(world.transform_point3(center));
                            for axis in 0..3 {
                                for sign in [-1.0f32, 1.0] {
                                    let fc = world.transform_point3(
                                        center + axes[axis] * (sign * half_extents[axis]),
                                    );
                                    let Some(p) = project(fc) else { continue };
                                    let outward = center_screen
                                        .map(|cs| p - cs)
                                        .filter(|v| v.length() > 1.0)
                                        .or_else(|| {
                                            project(fc + world.transform_vector3(axes[axis]) * sign)
                                                .map(|a| a - p)
                                        })
                                        .unwrap_or(egui::vec2(0.0, -1.0));
                                    let hovered = cursor.is_some_and(|cu| {
                                        handle_pick_dist(cu, p, outward, 26.0) <= 12.0
                                    });
                                    let (scale, col) = handle_style(hovered, color);
                                    draw_arrow_handle(painter, p, outward, scale, col);
                                }
                            }
                        }
                        citrus_editor::GizmoSpec::Points { positions, color } => {
                            for local in positions {
                                if let Some(p) = project(world.transform_point3(local))
                                    && full_rect.contains(p)
                                {
                                    painter.circle_filled(p, 1.6, color);
                                }
                            }
                        }
                        citrus_editor::GizmoSpec::Range {
                            center,
                            radius,
                            color,
                        } => {
                            let origin = world.transform_point3(center);
                            let stroke = egui::Stroke::new(1.5, color);
                            let circle = |a: Vec3, b: Vec3| {
                                const SEG: usize = 32;
                                let mut prev = origin + a * radius;
                                for k in 1..=SEG {
                                    let ang = k as f32 / SEG as f32 * std::f32::consts::TAU;
                                    let q = origin + (a * ang.cos() + b * ang.sin()) * radius;
                                    line(prev, q, stroke);
                                    prev = q;
                                }
                            };
                            circle(Vec3::X, Vec3::Y);
                            circle(Vec3::Y, Vec3::Z);
                            circle(Vec3::X, Vec3::Z);
                            let right = self.camera_right();
                            let hp = origin + right * radius;
                            if let Some(p) = project(hp) {
                                let outward = project(hp + right)
                                    .map(|a| a - p)
                                    .unwrap_or(egui::vec2(1.0, 0.0));
                                let hovered = cursor.is_some_and(|cu| {
                                    handle_pick_dist(cu, p, outward, 26.0) <= 12.0
                                });
                                let (scale, col) = handle_style(hovered, color);
                                draw_arrow_handle(painter, p, outward, scale, col);
                            }
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
            let full_rect = *self.viewport_rect;
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
                let is_refl_probe = object.components.iter().any(|c| {
                    c.as_any()
                        .downcast_ref::<citrus_core::ReflectionProbe>()
                        .is_some()
                });
                let is_audio = object.components.iter().any(|c| {
                    c.as_any()
                        .downcast_ref::<citrus_core::AudioSource>()
                        .is_some()
                });
                let is_flux_volume = object.components.iter().any(|c| {
                    c.as_any()
                        .downcast_ref::<citrus_core::FluxVolume>()
                        .is_some()
                });
                if !is_light
                    && !is_camera
                    && !is_probe
                    && !is_refl_probe
                    && !is_audio
                    && !is_flux_volume
                {
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
                // Icon priority: light, probe volume (incl. FluxVolume), reflection
                // probe, audio, camera.
                let setting = if is_light {
                    self.widget_filter.lights
                } else if is_probe || is_flux_volume {
                    self.widget_filter.probes
                } else if is_refl_probe {
                    self.widget_filter.reflection_probes
                } else if is_audio {
                    self.widget_filter.audio
                } else {
                    self.widget_filter.cameras
                };
                // Master eye off hides everything; otherwise filtered-off
                // billboards still show for the selected object.
                if !self.widget_filter.enabled || (!setting.visible && !selected) {
                    continue;
                }
                if is_light {
                    draw_light_icon(painter, screen, selected, setting.size);
                } else if is_probe || is_flux_volume {
                    draw_probe_icon(painter, screen, selected, setting.size);
                } else if is_refl_probe {
                    draw_reflection_probe_icon(painter, screen, selected, setting.size);
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
            let full_rect = *self.viewport_rect;
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

        // (AudioSource ranges + collider box/sphere are now drawn by the generic
        // component-gizmo pass above via `gizmos()`.)

        // Mesh-collider AABB (yellow): not editable (follows the mesh), so it's
        // not a `gizmos()` resize box — kept as a dedicated read-only draw.
        if let Selection::Object(i) = *self.selection
            && self.scene.is_active(i)
        {
            const YELLOW: egui::Color32 = egui::Color32::from_rgb(240, 220, 70);
            let world = self.scene.world_transform(i);
            let view_proj = self.proj * self.view;
            let full_rect = *self.viewport_rect;
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
        // Hidden when gizmos are globally off (eye button) or its own toggle is off.
        if self.widget_filter.enabled
            && self.widget_filter.cross
            && let Selection::Object(i) = *self.selection
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
            let full_rect = *self.viewport_rect;
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

        // Selected-camera preview (bottom-right): live view through the camera
        // being edited, so its framing can be tweaked from the viewport.
        if self.camera_overlay
            && let Some((texture, size)) = self.camera_preview
        {
            let w = 260.0;
            let h = w / (size[0] / size[1].max(1.0));
            egui::Area::new(ui.id().with("vp-cam-overlay"))
                .order(egui::Order::Middle)
                .pivot(egui::Align2::RIGHT_BOTTOM)
                .fixed_pos(rect.right_bottom() + egui::vec2(-8.0, -8.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(egui::RichText::new("📷 Camera").small().weak());
                        ui.add(
                            egui::Image::new(egui::load::SizedTexture::new(
                                texture,
                                egui::vec2(w, h),
                            ))
                            .maintain_aspect_ratio(true),
                        );
                    });
                });
        }

        // Widget filter (top-right): per-billboard visibility + size. The
        // move/rotate/scale gizmos are deliberately absent; they can't be
        // hidden. Selected objects always show their billboard regardless.
        egui::Area::new(ui.id().with("vp-widgets"))
            .order(egui::Order::Middle)
            .pivot(egui::Align2::RIGHT_TOP)
            .fixed_pos(rect.right_top() + egui::vec2(-8.0, 8.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                    use citrus_editor::egui_phosphor::regular as ph;
                    // Eye = master toggle for ALL viewport gizmos/widgets.
                    let eye = if self.widget_filter.enabled { ph::EYE } else { ph::EYE_SLASH };
                    if ui
                        .selectable_label(self.widget_filter.enabled, eye)
                        .on_hover_text("Toggle all gizmos")
                        .clicked()
                    {
                        self.widget_filter.enabled = !self.widget_filter.enabled;
                    }
                    // Render-mode dropdown (icon only): how the viewport shades.
                    let mode_icon = match *self.gi_debug {
                        1 => ph::COMPASS,         // world normals
                        2 => ph::CUBE_TRANSPARENT, // indirect GI
                        3 => ph::PAINT_BRUSH,     // unlit
                        4 | 5 => ph::SPHERE,      // reflection debug
                        _ => ph::LIGHTBULB,       // lit
                    };
                    let mode = &mut *self.gi_debug;
                    egui::containers::menu::MenuButton::new(mode_icon)
                        .config(egui::containers::menu::MenuConfig::new())
                        .ui(ui, |ui| {
                            ui.label(egui::RichText::new("Render mode").small().weak());
                            ui.selectable_value(mode, 0, format!("{} Lit", ph::LIGHTBULB));
                            ui.selectable_value(mode, 3, format!("{} Unlit", ph::PAINT_BRUSH));
                            ui.selectable_value(mode, 2, format!("{} Indirect GI", ph::CUBE_TRANSPARENT));
                            ui.selectable_value(mode, 1, format!("{} World Normals", ph::COMPASS));
                            ui.separator();
                            ui.label(egui::RichText::new("Reflection debug").small().weak());
                            ui.selectable_value(mode, 4, format!("{} Reflection cube (mirror)", ph::SPHERE))
                                .on_hover_text(
                                    "Every surface becomes a perfect mirror of the reflection \
                                     cube (sharp, no roughness/BRDF) — reveals the captured \
                                     cube's orientation directly.",
                                );
                            ui.selectable_value(mode, 5, format!("{} Reflection vector", ph::ARROWS_OUT))
                                .on_hover_text("Reflection direction R as RGB (+X red, +Y green, +Z blue).");
                        });
                    // Gizmos dropdown (icon only): per-kind billboard filter. Stays
                    // open across clicks so several kinds toggle at once.
                    egui::containers::menu::MenuButton::new(ph::SQUARES_FOUR)
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
                                ("Reflection Probes", &mut filter.reflection_probes),
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
                            ui.checkbox(&mut filter.cross, "Orientation cross")
                                .on_hover_text(
                                    "The grey axis cross at the selected object's pivot \
                                     (Move/Scale tools). Independent of the transform handles.",
                                );
                            ui.separator();
                            ui.label(
                                egui::RichText::new("Move/Rotate/Scale always shown · selected objects ignore this")
                                    .small()
                                    .weak(),
                            );
                        });
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

        // Drop from the Files panel onto the viewport: a `.material` assigns to
        // the hit mesh; a model file (FBX/glTF) imports into the scene.
        if let Some(payload) = response.dnd_release_payload::<PathBuf>() {
            let ext = payload
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase());
            match ext.as_deref() {
                Some("material") => {
                    if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                        self.actions
                            .push(EditorAction::AssignMaterialAt(pos, (*payload).clone()));
                    }
                }
                Some("fbx" | "gltf" | "glb" | "obj") => {
                    self.actions.push(EditorAction::ImportModel((*payload).clone()));
                }
                _ => {}
            }
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

        // Effective selection (honours the lock).
        let effective = if self.inspector.locked {
            self.inspector_lock_target
                .clone()
                .unwrap_or_else(|| self.selection.clone())
        } else {
            self.selection.clone()
        };
        // Pinned header, stays put while the body scrolls. For an object it's the
        // single flex row [enabled][name (grows)][static][lock]; otherwise just
        // the lock toggle.
        match &effective {
            Selection::Object(i) if *i < self.scene.objects.len() => {
                let index = *i;
                let obj = &mut self.scene.objects[index];
                let mut name = obj.name.clone();
                let mut enabled = obj.enabled;
                let mut static_geometry = obj.static_geometry;
                let hr =
                    self.inspector
                        .object_header(ui, &mut name, &mut enabled, &mut static_geometry);
                if hr.object_changed {
                    obj.name = name;
                    obj.enabled = enabled;
                    obj.static_geometry = static_geometry;
                }
                if hr.lock_changed {
                    *self.inspector_lock_target = if self.inspector.locked {
                        Some(self.selection.clone())
                    } else {
                        None
                    };
                }
                // Layer dropdown (Unity-style): drives the camera culling mask
                // (rendering) and the layer-collision matrix (physics). Shows the
                // SAME layer set as the camera mask + Layers window (`shown_count`).
                // Edit the names + matrix in Tools → Layers.
                let shown = self.scene.layers.shown_count();
                let layer_names: Vec<String> = (0..shown as u8)
                    .map(|l| self.scene.layers.layer_name(l))
                    .collect();
                let selected_text = self.scene.layers.layer_name(self.scene.objects[index].layer);
                let obj = &mut self.scene.objects[index];
                let mut layer = obj.layer;
                ui.horizontal(|ui| {
                    ui.label("Layer");
                    egui::ComboBox::from_id_salt("object-layer")
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            for (l, name) in layer_names.iter().enumerate() {
                                ui.selectable_value(&mut layer, l as u8, name);
                            }
                        });
                });
                if layer != obj.layer {
                    obj.layer = layer;
                }
                // Lightmap UVs (model objects): auto-generate toggle (off by
                // default) + regenerate. Generated UVs persist as a `.lmuv` marker.
                let model_path = match &self.scene.objects[index].source {
                    citrus_assets::ObjectSource::Model { path, .. } => Some(path.clone()),
                    _ => None,
                };
                if let Some(mpath) = model_path {
                    let has_marker = self.generated_uv_models.iter().any(|m| m == &mpath);
                    ui.horizontal(|ui| {
                        let mut on = has_marker;
                        if ui
                            .checkbox(&mut on, "Auto-gen lightmap UVs")
                            .on_hover_text(
                                "When this model has no usable lightmap UV (no 2nd UV set AND an \
                                 overlapping uv0), auto-unwrap a non-overlapping one for baking. \
                                 Off (default) = use the model's own UVs. Reloads the scene.",
                            )
                            .changed()
                        {
                            self.actions.push(EditorAction::SetLightmapUvGen(mpath.clone(), on));
                        }
                        if has_marker
                            && ui
                                .small_button("Regenerate")
                                .on_hover_text("Re-run the unwrapper (picks up improvements).")
                                .clicked()
                        {
                            self.actions.push(EditorAction::RegenerateLightmapUvs);
                        }
                    });
                }
            }
            _ => {
                if self.inspector.lock_header(ui) {
                    *self.inspector_lock_target = if self.inspector.locked {
                        Some(self.selection.clone())
                    } else {
                        None
                    };
                }
            }
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Reserve a constant right gutter so the floating scrollbar
                // overlays empty padding instead of the widgets (slider pips on
                // the right edge). A fixed margin never reflows, so no bounce.
                egui::Frame::default()
                    .inner_margin(egui::Margin { right: 12, ..egui::Margin::ZERO })
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
                    // Multi-select banner: the inspector shows the anchor object,
                    // but edits to shared transform/component values apply to ALL
                    // selected objects (see `propagate_multi_edit`). Components not
                    // present on every selected object are listed so it's clear
                    // which ones won't propagate.
                    let multi = self.multi_objects.len();
                    if multi > 1 && self.multi_objects.contains(&index) {
                        let sel: Vec<usize> = self
                            .multi_objects
                            .iter()
                            .copied()
                            .filter(|&j| j < self.scene.objects.len())
                            .collect();
                        // Component names shared by ALL selected objects.
                        let shared: std::collections::BTreeSet<String> = {
                            let mut sets = sel.iter().map(|&j| {
                                self.scene.objects[j]
                                    .save_components()
                                    .into_iter()
                                    .map(|(n, _)| n)
                                    .collect::<std::collections::BTreeSet<String>>()
                            });
                            match sets.next() {
                                Some(first) => sets.fold(first, |a, b| {
                                    a.intersection(&b).cloned().collect()
                                }),
                                None => Default::default(),
                            }
                        };
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(48, 40, 70))
                            .inner_margin(egui::Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(format!("{multi} objects selected"))
                                        .strong()
                                        .color(egui::Color32::from_rgb(200, 180, 255)),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "Editing applies to all selected. Showing the active object.",
                                    )
                                    .weak()
                                    .small(),
                                );
                                if shared.is_empty() {
                                    ui.label(
                                        egui::RichText::new("No components shared by all.")
                                            .small()
                                            .color(egui::Color32::from_rgb(220, 160, 120)),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Shared: {}",
                                            shared.into_iter().collect::<Vec<_>>().join(", ")
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                }
                            });
                        ui.separator();
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
                    // Material slots for this object (slot 0 + extras). The
                    // inspector edits the selected one.
                    let slots: Vec<RenderInfo> = object.render_slots().collect();
                    let slot_count = slots.len();
                    if *self.selected_material_slot >= slot_count.max(1) {
                        *self.selected_material_slot = 0;
                    }
                    // A multi-material mesh: pick which slot to edit.
                    if slot_count > 1 {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new("Material Slots").strong());
                            for s in 0..slot_count {
                                let mi = slots[s].material;
                                let name = &self.scene.materials[mi].model.name;
                                let label = format!("{s}: {name}");
                                if ui
                                    .selectable_label(*self.selected_material_slot == s, label)
                                    .clicked()
                                {
                                    *self.selected_material_slot = s;
                                }
                            }
                        });
                    }
                    let slot = *self.selected_material_slot;
                    let material_index = slots.get(slot).map(|r| r.material);
                    // Object list (id + name) for ObjectRef pickers, built as an
                    // owned Vec before the mutable component borrow so it doesn't
                    // alias scene.objects.
                    let objects: Vec<(ObjectId, String)> = self
                        .scene
                        .objects
                        .iter()
                        .map(|o| (o.id, o.name.clone()))
                        .collect();
                    // Layer names for the camera culling-mask picker (cloned before
                    // the mutable scene borrow below).
                    let layer_names: Vec<String> = self.scene.layers.names.clone();
                    // Imported (embedded) materials have no backing file; they're
                    // read-only until extracted, so don't hand them to the editor.
                    let embedded = material_index
                        .map(|m| self.scene.materials[m].file.is_none())
                        .unwrap_or(false);
                    // Split borrows: components live on the object, the
                    // material model in the scene's material list.
                    let scene = &mut *self.scene;
                    let components = &mut scene.objects[index].components;
                    let material = if embedded {
                        None
                    } else {
                        material_index.map(|m| &mut scene.materials[m].model)
                    };
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
                            layer_names: &layer_names,
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
                        self.actions.push(EditorAction::AssignMaterialToObject(
                            index, slot, path,
                        ));
                    }
                    if let Some((slot, path)) = response.texture_dropped
                        && let Some(mi) = material_index
                    {
                        self.actions.push(EditorAction::AssignTexture {
                            material: mi,
                            slot,
                            path,
                        });
                    }
                    // Embedded material: offer extraction (then it's editable).
                    if embedded
                        && let Some(mi) = material_index
                    {
                        ui.separator();
                        ui.label(
                            egui::RichText::new(
                                "Imported material — read-only. Extract it to a .material file to edit + save.",
                            )
                            .small()
                            .weak(),
                        );
                        if ui.button("⤓ Extract Material").clicked() {
                            self.actions.push(EditorAction::ExtractMaterial(mi));
                        }
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
                                if let Some((slot, path)) = response.texture_dropped {
                                    self.actions
                                        .push(EditorAction::AssignFileTexture { slot, path });
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
                        Some("fbx" | "gltf" | "glb" | "obj") => {
                            ui.heading("Model Import");
                            ui.label(egui::RichText::new(&path_display).small().weak());
                            ui.separator();
                            let mut do_save = false;
                            let mut do_reimport = false;
                            let mut do_extract = false;
                            if let Some(fm) = self.file_meta.as_mut().filter(|f| f.path == *path) {
                                match fm.meta.importer.as_model_mut() {
                                    Some(model) => {
                                        if model_import_ui(ui, model) {
                                            fm.dirty = true;
                                        }
                                    }
                                    None => {
                                        ui.label("No import settings for this asset type.");
                                    }
                                }
                                ui.separator();
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(fm.dirty, egui::Button::new("💾 Save"))
                                        .clicked()
                                    {
                                        do_save = true;
                                    }
                                    if ui
                                        .button("↻ Reimport")
                                        .on_hover_text("Save settings + reload the scene so this model reimports")
                                        .clicked()
                                    {
                                        do_reimport = true;
                                    }
                                    if fm.dirty {
                                        ui.label(egui::RichText::new("unsaved").small().weak());
                                    }
                                });
                                ui.separator();
                                if ui
                                    .button("⤓ Extract textures & materials")
                                    .on_hover_text(
                                        "Write this model's embedded textures (PNG) and materials \
                                         (.material) into <project>/extracted/<model>/ so they can \
                                         be edited and reused as project assets.",
                                    )
                                    .clicked()
                                {
                                    do_extract = true;
                                }
                            } else {
                                ui.label("Loading import settings…");
                            }
                            if do_save {
                                self.actions.push(EditorAction::SaveFileMeta);
                            }
                            if do_reimport {
                                self.actions.push(EditorAction::ReimportModel(path.clone()));
                            }
                            if do_extract {
                                self.actions.push(EditorAction::ExtractModelAssets(path.clone()));
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
            });
    }
}

/// Model (.fbx/.gltf) import options editor, backed by the asset's `.meta`.
/// Returns true when a value changed.
fn model_import_ui(ui: &mut egui::Ui, m: &mut citrus_assets::ModelImport) -> bool {
    use egui::{DragValue, Slider};
    let mut changed = false;
    egui::Grid::new("model-import-grid").num_columns(2).show(ui, |ui| {
        ui.label("Scale");
        changed |= ui
            .add(DragValue::new(&mut m.scale).speed(0.001).range(0.0001..=1000.0))
            .on_hover_text("Uniform import scale (e.g. 0.01 for cm→m sources)")
            .changed();
        ui.end_row();
        ui.label("Import Materials");
        changed |= ui.checkbox(&mut m.import_materials, "").changed();
        ui.end_row();
        ui.label("Flip UV (V)");
        changed |= ui.checkbox(&mut m.flip_uv, "").changed();
        ui.end_row();
        ui.label("Recalc Normals");
        changed |= ui.checkbox(&mut m.recalculate_normals, "").changed();
        ui.end_row();
        if m.recalculate_normals {
            ui.label("Smoothing Angle");
            changed |= ui
                .add(Slider::new(&mut m.smoothing_angle, 0.0..=180.0).suffix("°"))
                .changed();
            ui.end_row();
        }
    });
    changed
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
/// Distance from `press` to the drawn arrow handle: the segment from `at` (face
/// center) outward along `dir` for `len` pixels (covering shaft + arrowhead).
/// Lets the whole arrow be a click target, not just the base.
fn handle_pick_dist(press: egui::Pos2, at: egui::Pos2, dir: egui::Vec2, len: f32) -> f32 {
    let d = if dir.length() > 1e-3 {
        dir.normalized()
    } else {
        return press.distance(at);
    };
    let t = (press - at).dot(d).clamp(0.0, len);
    press.distance(at + d * t)
}

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

/// Draw a reflection-probe icon: a chrome sphere with a specular highlight (so
/// it reads as a mirror ball, distinct from the light-probe bulbs).
fn draw_reflection_probe_icon(
    painter: &egui::Painter,
    center: egui::Pos2,
    selected: bool,
    scale: f32,
) {
    let r = 8.0 * scale;
    let rim = if selected {
        egui::Color32::from_rgb(150, 220, 255)
    } else {
        egui::Color32::from_rgb(110, 170, 210)
    };
    // Chrome body: a vertical light→dark gradient faked with two stacked circles.
    painter.circle_filled(center, r, egui::Color32::from_rgb(70, 90, 110));
    painter.circle_filled(
        center - egui::vec2(0.0, r * 0.28),
        r * 0.72,
        egui::Color32::from_rgb(120, 150, 175),
    );
    // Specular highlight dot, upper-left.
    painter.circle_filled(
        center + egui::vec2(-r * 0.34, -r * 0.40),
        r * 0.24,
        egui::Color32::from_rgb(235, 245, 255),
    );
    painter.circle_stroke(center, r, egui::Stroke::new(1.5, rim));
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
/// First project asset file (recursively) modified after `since`, or None.
/// Skips `target/` and dot-directories. Used by the focus-regain reimport.
fn first_changed_asset(root: &Path, since: std::time::SystemTime) -> Option<PathBuf> {
    const EXTS: &[&str] = &[
        "fbx", "gltf", "glb", "obj", "png", "jpg", "jpeg", "tga", "hdr", "material", "frag",
        "scene", "postfx", "wav", "flac", "mp3",
    ];
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if let Some(found) = first_changed_asset(&path, since) {
                return Some(found);
            }
        } else if let Some(ext) = path.extension().and_then(|x| x.to_str())
            && EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext))
            && let Ok(modified) = entry.metadata().and_then(|m| m.modified())
            && modified > since
        {
            return Some(path);
        }
    }
    None
}

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
        if self.window.is_some() || self.pending_init {
            return;
        }
        // Show the splash window FIRST, then defer the heavy `init` to
        // `about_to_wait`. The event loop configures the splash window in
        // between, so it's actually on screen while the (blocking) init runs.
        // Drop an animated `splash.webp` / `splash.gif` in the project root to
        // override the embedded static image.
        match crate::splash::Splash::new(event_loop, &self.project_root) {
            Ok(mut s) => {
                s.set_status("Starting…");
                self.splash = Some(s);
            }
            Err(e) => tracing::warn!("splash window: {e:#}"),
        }
        self.pending_init = true;
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta } = event {
            // Feed the binding system (2C) so Play-mode mouselook works; the
            // editor fly-cam still uses look_delta gated on `looking`.
            if self.playing {
                self.input.mouse_motion(delta.0, delta.1);
            }
            if self.looking {
                self.look_delta.0 += delta.0;
                self.look_delta.1 += delta.1;
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // Splash is its own borderless window: just repaint it on request and
        // swallow its events. It's closed after the first editor frame.
        if let Some(s) = self.splash.as_mut() {
            if window_id == s.window_id() {
                if matches!(event, WindowEvent::RedrawRequested) {
                    s.present();
                }
                return;
            }
        }

        // Profiler is a separate OS window: route its events here and return so
        // the main-window editor logic below never sees them.
        if let Some(pw) = self.profiler_window.clone() {
            if window_id == pw.id() {
                if let Some(state) = self.profiler_egui_state.as_mut() {
                    let r = state.on_window_event(&pw, &event);
                    if r.repaint {
                        pw.request_redraw();
                    }
                }
                match event {
                    WindowEvent::CloseRequested => {
                        // Closing the window just turns the profiler off.
                        self.show_stats_overlay = false;
                        self.close_profiler_window();
                    }
                    WindowEvent::Resized(size) => {
                        if let Some(r) = self.renderer.as_mut() {
                            r.resize_profiler_window(size.width, size.height);
                        }
                    }
                    WindowEvent::RedrawRequested => {
                        // Rendered from the main redraw loop; nothing to do here.
                    }
                    _ => {}
                }
                return;
            }
        }

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

        // Rebind capture (2C): while a binding slot is armed, the next key /
        // mouse-button press is captured into it (Esc cancels). Highest priority
        // so it pre-empts editor shortcuts and egui focus.
        if self.rebinding.is_some() {
            match &event {
                WindowEvent::KeyboardInput {
                    event:
                        KeyEvent {
                            physical_key: PhysicalKey::Code(code),
                            state: ElementState::Pressed,
                            ..
                        },
                    ..
                } => {
                    if *code == KeyCode::Escape {
                        self.rebinding = None;
                    } else {
                        self.apply_rebind(citrus_core::InputSource::Key(
                            crate::input_engine::map_key(*code),
                        ));
                    }
                    return;
                }
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button,
                    ..
                } => {
                    if let Some(b) = crate::input_engine::map_mouse(*button) {
                        self.apply_rebind(citrus_core::InputSource::Mouse(b));
                    }
                    return;
                }
                _ => {}
            }
        }

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
                    self.input.key(code, false);
                }
                ElementState::Pressed if !egui_wants => {
                    self.keys.insert(code);
                    self.input.key(code, true);
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
                    // still track modifier keys so `self.keys` doesn't desync,
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
            // Regained focus → reimport assets edited in other apps (e.g. an FBX
            // re-exported from Blender) without a manual reimport.
            WindowEvent::Focused(true) => self.reload_changed_assets(),
            WindowEvent::MouseInput { button, state, .. } => {
                let pressed = state == ElementState::Pressed;
                if self.playing {
                    self.input.mouse_button(button, pressed);
                }
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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Startup: drive the splash animation and the deferred/threaded load.
        // Runs until the main window exists (i.e. finish_init has completed).
        if self.window.is_none() {
            // Splash: show the current load phase + animate every tick.
            if let Some(s) = self.splash.as_mut() {
                let status = self.load_status.lock().unwrap().clone();
                let painted = if status.is_empty() {
                    s.present()
                } else {
                    s.set_status(&status)
                };
                self.splash_painted = painted || self.splash_painted;
            }
            // Kick off the load once the splash has shown a frame (Wayland needs
            // the configure roundtrip first); proceed immediately if no splash.
            if self.pending_init && (self.splash_painted || self.splash.is_none()) {
                self.pending_init = false;
                if let Err(e) = self.start_load(event_loop) {
                    tracing::error!("starting load failed: {e:#}");
                    event_loop.exit();
                }
            }
            // Renderer ready → finish_init (builds plugins/project, then spawns
            // the scene parse; the editor opens only once the scene is applied).
            if let Some(rx) = self.renderer_rx.as_ref() {
                match rx.try_recv() {
                    Ok(Ok(renderer)) => {
                        self.renderer_rx = None;
                        let window = self.pending_window.take().expect("pending window");
                        if let Err(e) = self.finish_init(window, renderer, None) {
                            tracing::error!("initialization failed: {e:#}");
                            event_loop.exit();
                        } else {
                            tracing::info!(elapsed = ?self.start.elapsed(), "renderer ready");
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::error!("renderer build failed: {e:#}");
                        event_loop.exit();
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        tracing::error!("renderer build thread died");
                        event_loop.exit();
                    }
                }
            }
            // Scene parse finished → upload on the main thread + open the editor.
            let results = self.tasks.poll_workers();
            for r in results {
                self.apply_task_result(r);
            }
            return;
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

/// One binding-slot row in the Input Bindings window: shows the slot's bound
/// inputs as removable chips plus a capture (＋) button that arms a rebind.
/// Returns true if a binding was removed. The actual key/mouse capture happens
/// in the window-event handler when `rebinding` is set.
fn binding_slot_row(
    ui: &mut egui::Ui,
    label: &str,
    list: &mut Vec<citrus_core::InputSource>,
    action_index: usize,
    slot: &str,
    rebinding: &mut Option<(usize, String)>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_sized([42.0, 16.0], egui::Label::new(egui::RichText::new(label).small()));
        let mut remove = None;
        for (i, src) in list.iter().enumerate() {
            if ui.small_button(format!("{} ✕", src.label())).clicked() {
                remove = Some(i);
            }
        }
        if let Some(i) = remove {
            list.remove(i);
            changed = true;
        }
        let waiting = rebinding
            .as_ref()
            .map(|(a, s)| *a == action_index && s == slot)
            .unwrap_or(false);
        if ui.small_button(if waiting { "…" } else { "＋" }).clicked() {
            *rebinding = Some((action_index, slot.to_string()));
        }
    });
    changed
}
