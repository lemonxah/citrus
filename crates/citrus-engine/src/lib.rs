//! citrus-engine: application loop, scene management, editor shell.
//!
//! The editor is a dockable layout (egui_dock) around a transparent
//! Viewport tab: Scene list, unified Inspector, project Files browser,
//! menu bar, transform gizmos, picking, and drag & drop assets.

mod camera;
mod gizmo;
mod scene;
mod undo;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use egui_dock::{DockArea, DockState, NodeIndex};
use glam::Vec3;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use camera::FlyCamera;
use gizmo::{GizmoState, GizmoTool};
use scene::{LoadedScene, material_from_model, model_from_material, relative_to};
use undo::{ObjectState, UndoEntry, UndoStack};
use citrus_editor::{
    FileBrowser, InspectorContent, InspectorPanel, MaterialModel, ObjectInfoModel, ScenePanel,
    SHADER_REGISTRY, TransformModel,
};
use citrus_render::{CameraData, FrameInput, LightData, Renderer};

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
        scene_panel: ScenePanel::new(),
        file_browser: FileBrowser::new(project_root.clone()),
        selection: Selection::None,
        file_material: None,
        gizmo: GizmoState::new(),
        actions: Vec::new(),
        undo_stack: UndoStack::default(),
        suppress_undo_record: false,
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
    CreateFolder(PathBuf),
    PickAt(egui::Pos2),
    AssignMaterialAt(egui::Pos2, PathBuf),
    AssignMaterialToObject(usize, PathBuf),
    MaterialEdited(usize),
    ResetMaterial(usize),
    SaveFileMaterial,
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
    /// Re-parent an object (None = unparent).
    SetParent(usize, Option<usize>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Viewport,
    Scene,
    Inspector,
    Files,
}

fn default_layout() -> DockState<Tab> {
    let mut state = DockState::new(vec![Tab::Viewport]);
    let tree = state.main_surface_mut();
    let [viewport, _right] = tree.split_right(NodeIndex::root(), 0.78, vec![Tab::Inspector]);
    let [viewport, _left] = tree.split_left(viewport, 0.18, vec![Tab::Scene]);
    let [_viewport, _bottom] = tree.split_below(viewport, 0.74, vec![Tab::Files]);
    state
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
    scene_panel: ScenePanel,
    file_browser: FileBrowser,
    selection: Selection,
    file_material: Option<FileMaterial>,
    gizmo: GizmoState,
    actions: Vec<EditorAction>,
    undo_stack: UndoStack,
    /// Set while applying undo/redo so the frame diff doesn't re-record it.
    suppress_undo_record: bool,
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
    stats: Stats,
    #[allow(dead_code)] // entities arrive with the component-system milestone
    world: hecs::World,
    start: Instant,
    last_frame: Instant,
}

impl EngineApp {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.width,
                self.config.height,
            ));
        let window = Arc::new(event_loop.create_window(attrs)?);
        let mut renderer = Renderer::new(window.clone())?;

        match self.config.scene_path.clone() {
            Some(path) if path.ends_with(".scene") => {
                let file = citrus_assets::load_scene_file(&path)?;
                self.scene =
                    LoadedScene::load_scene_file(&mut renderer, &file, &self.project_root)?;
                self.current_scene_path = Some(PathBuf::from(path));
            }
            Some(path) => {
                let asset = citrus_assets::load_model(&path)?;
                self.scene
                    .add_asset_scene(&mut renderer, &asset, Some(Path::new(&path)))?;
            }
            None => {
                let asset = citrus_assets::test_scene();
                self.scene.add_asset_scene(&mut renderer, &asset, None)?;
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
        Some((self.camera.position, (far - near).normalize_or(self.camera.forward())))
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
                    match citrus_assets::load_model(&path) {
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
                    }
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
                        textures: Default::default(),
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
                    if let Some((origin, dir)) = self.cursor_ray(pos) {
                        if let Some(hit) = self.scene.pick(origin, dir) {
                            let Some(before) =
                                self.scene.objects[hit].render.map(|r| r.material)
                            else {
                                continue;
                            };
                            self.scene
                                .assign_material(renderer!(), hit, &path, &self.project_root);
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
                }
                EditorAction::AssignMaterialToObject(object, path) => {
                    let Some(before) = self.scene.objects[object].render.map(|r| r.material)
                    else {
                        continue;
                    };
                    self.scene
                        .assign_material(renderer!(), object, &path, &self.project_root);
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
                    self.scene.apply_material(renderer!(), index);
                }
                EditorAction::ResetMaterial(index) => {
                    self.scene.materials[index].model =
                        self.scene.materials[index].default.clone();
                    self.scene.apply_material(renderer!(), index);
                }
                EditorAction::SaveFileMaterial => {
                    if let Some(fm) = &mut self.file_material {
                        let (params, features) = material_from_model(&fm.model);
                        fm.file.params = params;
                        fm.file.features = features;
                        fm.file.shader = fm.model.shader.clone();
                        fm.file.name = fm.model.name.clone();
                        match citrus_assets::save_material_file(&fm.path, &fm.file) {
                            Ok(()) => fm.dirty = false,
                            Err(e) => tracing::error!("saving material: {e:#}"),
                        }
                    }
                }
                EditorAction::LoadScene(path) => {
                    match citrus_assets::load_scene_file(&path) {
                        Ok(file) => {
                            if let Err(e) = renderer!().reset_scene() {
                                tracing::error!("resetting scene: {e:#}");
                            }
                            match LoadedScene::load_scene_file(renderer!(), &file, &self.project_root)
                            {
                                Ok(scene) => {
                                    self.scene = scene;
                                    self.selection = Selection::None;
                                    self.current_scene_path = Some(path);
                                    // Indices into the old scene are invalid.
                                    self.undo_stack.clear();
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
                            Selection::Object(i) => {
                                self.scene.world_transform(i).w_axis.truncate()
                            }
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
                EditorAction::SetParent(child, parent) => {
                    self.scene.set_parent(child, parent);
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
                    let file = self.scene.to_scene_file(&self.project_root);
                    match citrus_assets::save_scene_file(&path, &file) {
                        Ok(()) => {
                            tracing::info!("scene saved to {}", path.display());
                            self.current_scene_path = Some(path);
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
                    ui.separator();
                    for shape in [
                        PrimitiveShape::Cube,
                        PrimitiveShape::Sphere,
                        PrimitiveShape::Capsule,
                        PrimitiveShape::Plane,
                    ] {
                        if ui.button(shape.label()).clicked() {
                            self.actions.push(EditorAction::Spawn(
                                ObjectSource::Primitive { shape },
                            ));
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
                        if ui
                            .radio(self.gizmo.tool == tool, label)
                            .clicked()
                        {
                            self.gizmo.tool = tool;
                        }
                    }
                });
                ui.menu_button("View", |ui| {
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
                    if ui.button("Reset Layout").clicked() {
                        self.dock_state = default_layout();
                        ui.close();
                    }
                });
                ui.menu_button("Help", |ui| {
                    ui.checkbox(&mut self.show_help, "Controls");
                });

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

    /// Snapshot the selection's editable state before the UI runs, so edits
    /// can be diffed into undo entries afterwards.
    fn capture_edit_snapshot(&self) -> EditSnapshot {
        match &self.selection {
            Selection::Object(i) if *i < self.scene.objects.len() => {
                let o = &self.scene.objects[*i];
                EditSnapshot::Object {
                    index: *i,
                    state: ObjectState {
                        name: o.name.clone(),
                        translation: o.translation,
                        rotation: o.rotation,
                        scale: o.scale,
                    },
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
                let now = ObjectState {
                    name: o.name.clone(),
                    translation: o.translation,
                    rotation: o.rotation,
                    scale: o.scale,
                };
                if now != state {
                    self.undo_stack.record(UndoEntry::Object {
                        index,
                        before: state,
                        after: now,
                    });
                }
                if let (Some(material), Some(model)) = (material, model) {
                    if material < self.scene.materials.len() {
                        let current = &self.scene.materials[material].model;
                        if *current != *model {
                            self.undo_stack.record(UndoEntry::Material {
                                index: material,
                                before: model,
                                after: Box::new(current.clone()),
                            });
                        }
                    }
                }
            }
            EditSnapshot::File { path, model } => {
                if let Some(fm) = &self.file_material {
                    if fm.path == path && fm.model != *model {
                        self.undo_stack.record(UndoEntry::FileMaterial {
                            path,
                            before: model,
                            after: Box::new(fm.model.clone()),
                        });
                    }
                }
            }
            _ => {}
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
                    if let Some(renderer) = self.renderer.as_mut() {
                        self.scene.apply_material(renderer, index);
                    }
                }
            }
            UndoEntry::Assign {
                object,
                before,
                after,
            } => {
                let material = if undo { before } else { after };
                if object < self.scene.objects.len() && material < self.scene.materials.len() {
                    if let Some(render) = &mut self.scene.objects[object].render {
                        render.material = material;
                    }
                }
            }
            UndoEntry::FileMaterial {
                path,
                before,
                after,
            } => {
                let model = if undo { before } else { after };
                if let Some(fm) = &mut self.file_material {
                    if fm.path == path {
                        fm.model = *model;
                        fm.dirty = true;
                    }
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
                    row(ui, "Frame", format!(
                        "{:.1} ms ({:.0} fps)",
                        self.stats.frame_ms, self.stats.fps
                    ));
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

        let egui_ctx = self.egui_ctx.clone();
        let output = egui_ctx.run(raw_input, |ctx| {
            self.menu_bar(ctx);
            if self.show_stats_overlay {
                self.stats_overlay(ctx, render_stats);
            }

            let mut dock_state = std::mem::replace(&mut self.dock_state, DockState::new(vec![]));
            let mut tabs = EditorTabs {
                scene: &mut self.scene,
                selection: &mut self.selection,
                inspector: &mut self.inspector,
                scene_panel: &mut self.scene_panel,
                file_browser: &mut self.file_browser,
                file_material: &mut self.file_material,
                gizmo: &mut self.gizmo,
                actions: &mut self.actions,
                viewport_rect: &mut self.viewport_rect,
                view,
                proj,
                looking: self.looking,
            };
            DockArea::new(&mut dock_state)
                .style(egui_dock::Style::from_egui(ctx.style().as_ref()))
                .show_close_buttons(false)
                .show(ctx, &mut tabs);
            self.dock_state = dock_state;
        });

        if let Some(egui_state) = self.egui_state.as_mut() {
            egui_state.handle_platform_output(&window, output.platform_output);
        }
        let primitives = egui_ctx.tessellate(output.shapes, output.pixels_per_point);

        self.process_actions();
        self.record_edits(pre_edit);

        // Selection pulse + draw transform sync.
        let t = self.start.elapsed().as_secs_f32();
        let selected = match self.selection {
            Selection::Object(i) => Some(i),
            _ => None,
        };
        self.scene.sync_draws(selected, 1.0);

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let frame = FrameInput {
            clear_color: [0.016, 0.016, 0.024, 1.0],
            camera: CameraData {
                view,
                proj,
                position: self.camera.position,
            },
            light: LightData::default(),
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
    scene_panel: &'a mut ScenePanel,
    file_browser: &'a mut FileBrowser,
    file_material: &'a mut Option<FileMaterial>,
    gizmo: &'a mut GizmoState,
    actions: &'a mut Vec<EditorAction>,
    viewport_rect: &'a mut egui::Rect,
    view: glam::Mat4,
    proj: glam::Mat4,
    looking: bool,
}

impl egui_dock::TabViewer for EditorTabs<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match tab {
            Tab::Viewport => "Viewport".into(),
            Tab::Scene => "Scene".into(),
            Tab::Inspector => "Inspector".into(),
            Tab::Files => "Files".into(),
        }
    }

    fn clear_background(&self, tab: &Tab) -> bool {
        !matches!(tab, Tab::Viewport)
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match tab {
            Tab::Viewport => self.viewport_ui(ui),
            Tab::Scene => {
                let rows: Vec<citrus_editor::SceneObjectRow> = self
                    .scene
                    .objects
                    .iter()
                    .map(|o| citrus_editor::SceneObjectRow {
                        name: o.name.clone(),
                        parent: o.parent,
                        icon: match o.kind_label() {
                            "Empty" => "◇",
                            "Camera" => "🎥",
                            _ => "🧊",
                        },
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
            }
            Tab::Inspector => self.inspector_ui(ui),
            Tab::Files => {
                let selected = match self.selection {
                    Selection::File(path) => Some(path.clone()),
                    _ => None,
                };
                let response = self.file_browser.ui(ui, selected.as_deref());
                if let Some(path) = response.clicked {
                    self.actions.push(EditorAction::SelectFile(path));
                }
                if let Some(path) = response.import_model {
                    self.actions.push(EditorAction::ImportModel(path));
                }
                if let Some(dir) = response.create_material_in {
                    self.actions.push(EditorAction::CreateMaterial(dir));
                }
                if let Some(dir) = response.create_scene_in {
                    self.actions.push(EditorAction::CreateScene(dir));
                }
                if let Some(dir) = response.create_folder_in {
                    self.actions.push(EditorAction::CreateFolder(dir));
                }
            }
        }
    }
}

impl EditorTabs<'_> {
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
        if let Selection::Object(i) = *self.selection {
            {
                let pivot_local = match (self.gizmo.pivot, self.scene.objects[i].render) {
                    (gizmo::PivotMode::Center, Some(render)) => {
                        self.scene.mesh_center_local(render.mesh)
                    }
                    _ => Vec3::ZERO,
                };
                let drag_started =
                    response.drag_started_by(egui::PointerButton::Primary) && !alt;
                let dragging = response.dragged_by(egui::PointerButton::Primary) && !alt;
                // Gizmo operates in world space; parented objects convert
                // back to parent-local afterwards.
                let world = self.scene.world_transform(i);
                let (mut w_scale, mut w_rot, mut w_trans) =
                    world.to_scale_rotation_translation();
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

        // Plain left drag (not claimed by the gizmo): orbit the camera
        // around a pivot locked at drag start.
        if response.dragged_by(egui::PointerButton::Primary) && !self.gizmo.is_focused() {
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
        if response.clicked() && !gizmo_busy && !gizmo_changed && !self.looking {
            if let Some(pos) = response.interact_pointer_pos() {
                self.actions.push(EditorAction::PickAt(pos));
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
        if let Some(payload) = response.dnd_release_payload::<PathBuf>() {
            if payload.extension().is_some_and(|e| e == "material") {
                if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                    self.actions
                        .push(EditorAction::AssignMaterialAt(pos, (*payload).clone()));
                }
            }
        }
    }

    fn inspector_ui(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match self.selection {
                Selection::None => {
                    self.inspector
                        .ui(ui, InspectorContent::Empty, SHADER_REGISTRY);
                }
                Selection::Object(i) => {
                    let index = *i;
                    let object = &self.scene.objects[index];
                    let render = object.render;
                    let (rx, ry, rz) = object.rotation.to_euler(glam::EulerRot::XYZ);
                    let mut info = ObjectInfoModel {
                        name: object.name.clone(),
                        kind: object.kind_label(),
                        transform: TransformModel {
                            translation: object.translation.to_array(),
                            rotation_deg: [
                                rx.to_degrees(),
                                ry.to_degrees(),
                                rz.to_degrees(),
                            ],
                            scale: object.scale.to_array(),
                        },
                        mesh: render.map(|r| {
                            let mi = self.scene.mesh_info(r.mesh);
                            (mi.vertices, mi.triangles)
                        }),
                    };
                    let material_index = render.map(|r| r.material);
                    let material = material_index
                        .map(|m| &mut self.scene.materials[m].model);
                    let response = self.inspector.ui(
                        ui,
                        InspectorContent::Object {
                            info: &mut info,
                            material,
                        },
                        SHADER_REGISTRY,
                    );
                    if response.object_changed {
                        let object = &mut self.scene.objects[index];
                        object.name = info.name;
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
                                        dirty: fm.dirty,
                                    },
                                    SHADER_REGISTRY,
                                );
                                if response.material_changed {
                                    fm.dirty = true;
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
                                InspectorContent::SceneFile {
                                    path: path_display,
                                },
                                SHADER_REGISTRY,
                            );
                            if response.load_scene {
                                self.actions.push(EditorAction::LoadScene(path.clone()));
                            }
                        }
                        _ => {
                            let size = std::fs::metadata(&path).map(|m| m.len()).ok();
                            self.inspector.ui(
                                ui,
                                InspectorContent::File {
                                    path: path_display,
                                    size,
                                },
                                SHADER_REGISTRY,
                            );
                        }
                    }
                }
            });
    }
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
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.looking {
                self.look_delta.0 += delta.0;
                self.look_delta.1 += delta.1;
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let egui_wants = if let (Some(state), Some(window)) =
            (self.egui_state.as_mut(), self.window.as_ref())
        {
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
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } if !egui_wants => {
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
                    MouseButton::Right => {
                        // Look starts from the viewport tab widget (see
                        // viewport_ui); only the release is handled here so
                        // it can't get stuck while the cursor is locked.
                        if !pressed {
                            self.set_looking(false);
                        }
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
                if let Some((lx, ly)) = self.last_cursor {
                    if self.panning {
                        self.camera
                            .pan((position.x - lx) as f32, (position.y - ly) as f32);
                    }
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
