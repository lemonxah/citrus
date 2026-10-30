//! The Animator Controller tab: a node-graph editor for `.animctrl` state
//! machines (states, Any State, Entry, transitions), with Layers and
//! Parameters on the left, an inspector for the selection on the right, and a
//! live view of the selected object's Animator while it runs.

use std::path::{Path, PathBuf};

use citrus_assets::{
    AnimCondition, AnimLayer, AnimParamKind, AnimParameter, AnimState, AnimTransition, AnimatorController, CondOp,
    LayerBlend, Motion,
};
use citrus_core::{Animator, BlendMotion, BlendTree};
use citrus_editor::egui_phosphor::regular as ph;
use citrus_editor::icons::Icon;
use citrus_editor::{theme, widgets};
use egui::{Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2};

use crate::anim_clip_editor::AnimClipTab;
use crate::scene::LoadedScene;

const NODE_SIZE: Vec2 = Vec2::new(160.0, 40.0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Node {
    State(usize),
    Any,
    Entry,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sel {
    None,
    Node(Node),
    Transition(usize),
}

pub struct AnimControllerTab {
    path: Option<String>,
    ctrl: AnimatorController,
    dirty: bool,
    layer: usize,
    sel: Sel,
    pan: Vec2,
    zoom: f32,
    dragging: Option<Node>,
    /// Drawing a new transition from this node (drag with Shift, or the
    /// context menu's Make Transition followed by a click).
    linking: Option<Node>,
    context: Option<(Option<Node>, Option<usize>, Pos2)>,
    /// Asked the editor to show this tab.
    pub open_request: bool,
    last_file: Option<PathBuf>,
    files: Vec<String>,
    clip_files: Vec<String>,
}

impl Default for AnimControllerTab {
    fn default() -> Self {
        Self {
            path: None,
            ctrl: AnimatorController::default(),
            dirty: false,
            layer: 0,
            sel: Sel::None,
            pan: Vec2::ZERO,
            zoom: 1.0,
            dragging: None,
            linking: None,
            context: None,
            open_request: false,
            last_file: None,
            files: Vec::new(),
            clip_files: Vec::new(),
        }
    }
}

// ------------------------------------------------------------ shared helpers

/// Project-relative paths (forward slashes) of files with extension `ext`.
pub(crate) fn scan_files(root: &Path, ext: &str) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, ext: &str, out: &mut Vec<String>, depth: usize) {
        if depth > 12 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "target" || name == "build" {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, ext, out, depth + 1);
            } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case(ext))
                && let Ok(rel) = p.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, ext, &mut out, 0);
    out.sort();
    out
}

/// A free `<dir>/<stem>.<ext>` (numbered when taken).
pub(crate) fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (1..).map(|n| dir.join(format!("{stem} {n}.{ext}"))).find(|p| !p.exists()).unwrap()
}

pub(crate) fn rel_path(root: &Path, abs: &Path) -> String {
    abs.strip_prefix(root).unwrap_or(abs).to_string_lossy().replace('\\', "/")
}

/// Single-line text field that edits a buffer and reports the new text when
/// the edit is committed (Enter or focus lost) and differs from `current`.
pub(crate) fn commit_text(ui: &mut egui::Ui, id: egui::Id, current: &str, width: f32) -> Option<String> {
    let mut buf: String = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_else(|| current.to_owned());
    let resp = ui.add(egui::TextEdit::singleline(&mut buf).id(id.with("edit")).desired_width(width));
    if resp.has_focus() || resp.changed() {
        ui.data_mut(|d| d.insert_temp(id, buf.clone()));
    }
    if resp.lost_focus() {
        ui.data_mut(|d| d.remove::<String>(id));
        if buf != current && !buf.trim().is_empty() {
            return Some(buf.trim().to_owned());
        }
    } else if !resp.has_focus() {
        ui.data_mut(|d| d.remove::<String>(id));
    }
    None
}

fn file_stem(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_owned())
}

/// Requests the editor should act on this frame: tabs to show (true =
/// controller, false = clip). Reads the Animator inspector's Open button too.
pub fn take_open_requests(
    ctx: &egui::Context,
    ctrl: &mut AnimControllerTab,
    clip: &mut AnimClipTab,
    root: &Path,
) -> Vec<bool> {
    let mut out = Vec::new();
    let key = egui::Id::new(citrus_editor::OPEN_ANIM_CONTROLLER_KEY);
    if let Some(path) = ctx.data_mut(|d| d.remove_temp::<String>(key)) {
        ctrl.open(root, &path);
        out.push(true);
    }
    if std::mem::take(&mut ctrl.open_request) {
        out.push(true);
    }
    if std::mem::take(&mut clip.open_request) {
        out.push(false);
    }
    out
}

/// Inspector body for a selected `.animctrl` / `.anim` file: opens it in its
/// tab (once per selection) and offers a button to reopen.
pub fn file_inspector(
    ui: &mut egui::Ui,
    path: &Path,
    root: &Path,
    ctrl: &mut AnimControllerTab,
    clip: &mut AnimClipTab,
) {
    let rel = rel_path(root, path);
    let is_ctrl = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("animctrl"));
    ui.label(egui::RichText::new(file_stem(&rel)).font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
    ui.label(egui::RichText::new(&rel).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_MUTED));
    ui.add_space(8.0);
    if ctrl.last_file.as_deref() != Some(path) {
        ctrl.last_file = Some(path.to_owned());
        if is_ctrl {
            ctrl.open(root, &rel);
            ctrl.open_request = true;
        } else {
            clip.open(root, &rel);
            clip.open_request = true;
        }
    }
    let label = if is_ctrl { "Open in Animator Controller" } else { "Open in Animation" };
    if widgets::secondary_button(ui, Some(Icon::FileDoc), label).clicked() {
        if is_ctrl {
            ctrl.open(root, &rel);
            ctrl.open_request = true;
        } else {
            clip.open(root, &rel);
            clip.open_request = true;
        }
    }
    ui.add_space(6.0);
    widgets::info_note(
        ui,
        if is_ctrl {
            "Assign it to an Animator's Controller field (drag the file onto it)."
        } else {
            "Use it as an Animator's clip, or as a state's motion in a controller."
        },
    );
}

/// Create a new controller or clip in `dir` and open it.
pub fn create_asset(dir: &Path, controller: bool, root: &Path, ctrl: &mut AnimControllerTab, clip: &mut AnimClipTab) {
    if controller {
        let path = unique_path(dir, "New Controller", citrus_assets::ANIM_CONTROLLER_EXTENSION);
        let mut c = AnimatorController::default();
        c.layers[0].machine.states.push(AnimState { name: "Idle".into(), ..Default::default() });
        c.layers[0].machine.default_state = "Idle".into();
        match citrus_assets::save_anim_controller(&path, &c) {
            Ok(()) => {
                ctrl.open(root, &rel_path(root, &path));
                ctrl.open_request = true;
            }
            Err(e) => tracing::error!("creating animator controller: {e:#}"),
        }
    } else {
        let path = unique_path(dir, "New Clip", citrus_assets::ANIM_CLIP_EXTENSION);
        match citrus_assets::save_anim_clip(&path, &citrus_assets::AnimClip::default()) {
            Ok(()) => {
                clip.open(root, &rel_path(root, &path));
                clip.open_request = true;
            }
            Err(e) => tracing::error!("creating animation clip: {e:#}"),
        }
    }
}

fn animator(scene: &LoadedScene, i: usize) -> Option<&Animator> {
    scene.objects.get(i)?.components.iter().find_map(|c| c.as_any().downcast_ref::<Animator>())
}

fn animator_mut(scene: &mut LoadedScene, i: usize) -> Option<&mut Animator> {
    scene.objects.get_mut(i)?.components.iter_mut().find_map(|c| c.as_any_mut().downcast_mut::<Animator>())
}

// ------------------------------------------------------------------ the tab

impl AnimControllerTab {
    pub fn open(&mut self, root: &Path, rel: &str) {
        if self.path.as_deref() == Some(rel) {
            return;
        }
        if self.dirty {
            self.save(root);
        }
        match citrus_assets::load_anim_controller(root.join(rel)) {
            Ok(mut c) => {
                if c.layers.is_empty() {
                    c.layers.push(AnimLayer::default());
                }
                self.ctrl = c;
                self.path = Some(rel.to_owned());
                self.dirty = false;
                self.layer = 0;
                self.sel = Sel::None;
                self.linking = None;
                self.dragging = None;
            }
            Err(e) => tracing::error!("opening animator controller {rel}: {e:#}"),
        }
    }

    fn save(&mut self, root: &Path) {
        let Some(path) = &self.path else {
            return;
        };
        match citrus_assets::save_anim_controller(root.join(path), &self.ctrl) {
            Ok(()) => self.dirty = false,
            Err(e) => tracing::error!("saving animator controller: {e:#}"),
        }
    }

    fn changed(&mut self, scene: &mut LoadedScene) {
        self.dirty = true;
        if let Some(path) = &self.path {
            scene.anim_assets.set_controller(path, self.ctrl.clone());
        }
    }

    /// The object whose Animator runs this controller: the selection, else
    /// (while playing) the first object using it.
    fn live_object(&self, scene: &LoadedScene, selected: Option<usize>, playing: bool) -> Option<usize> {
        let path = self.path.as_deref()?;
        let uses = |i: usize| animator(scene, i).is_some_and(|a| scene.anim_assets.same_asset(&a.controller, path) && !a.state.layers.is_empty());
        if let Some(s) = selected.filter(|&s| uses(s)) {
            return Some(s);
        }
        if playing {
            return (0..scene.objects.len()).find(|&i| uses(i));
        }
        None
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, scene: &mut LoadedScene, selected: Option<usize>, root: &Path, playing: bool) {
        scene.anim_assets.set_root(root);
        let selected = selected.filter(|&i| i < scene.objects.len());
        let live = self.live_object(scene, selected, playing);
        if self.ctrl.layers.is_empty() {
            self.ctrl.layers.push(AnimLayer::default());
        }
        if self.layer >= self.ctrl.layers.len() {
            self.layer = 0;
            self.sel = Sel::None;
        }

        egui::TopBottomPanel::top("citrus-animctrl-toolbar")
            .frame(egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::symmetric(12, 6)))
            .show_inside(ui, |ui| self.toolbar(ui, scene, root, live));

        if self.path.is_none() {
            self.empty_state(ui, root, scene, selected);
            return;
        }

        egui::SidePanel::left("citrus-animctrl-left")
            .resizable(true)
            .default_width(236.0)
            .frame(egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::same(12)))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("citrus-animctrl-left-scroll").show(ui, |ui| {
                    self.layers_ui(ui, scene);
                    widgets::section_gap(ui);
                    self.params_ui(ui, scene, live.filter(|_| playing));
                });
            });
        egui::SidePanel::right("citrus-animctrl-right")
            .resizable(true)
            .default_width(290.0)
            .frame(egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::same(12)))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("citrus-animctrl-right-scroll").show(ui, |ui| {
                    self.inspector_ui(ui, scene, selected.or(live), root);
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG_VIEWPORT))
            .show_inside(ui, |ui| self.canvas(ui, scene, live));

        // Save once an edit settles (no button held), like code tabs.
        if self.dirty && !ui.input(|i| i.pointer.any_down()) && !ui.ctx().wants_keyboard_input() {
            self.save(root);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, scene: &mut LoadedScene, root: &Path, live: Option<usize>) {
        ui.horizontal(|ui| {
            ui.set_min_height(theme::H_TOOLBAR);
            ui.label(egui::RichText::new(ph::FLOW_ARROW).size(15.0).color(theme::TEXT_MUTED));
            match &self.path {
                Some(p) => {
                    ui.label(egui::RichText::new(file_stem(p)).font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG))
                        .on_hover_text(p.as_str());
                    widgets::chip(ui, if self.dirty { "Unsaved" } else { "Saved" });
                }
                None => {
                    ui.label(egui::RichText::new("No controller").color(theme::TEXT_MUTED));
                }
            }
            ui.add_space(8.0);
            let new = widgets::secondary_button(ui, Some(Icon::Plus), "New");
            if new.clicked() {
                let dir = root.join("animations");
                create_asset(&dir, true, root, self, &mut AnimClipTab::default());
                self.open_request = false;
            }
            let open = widgets::secondary_button(ui, Some(Icon::Folder), "Open");
            if open.clicked() {
                self.files = scan_files(root, citrus_assets::ANIM_CONTROLLER_EXTENSION);
            }
            egui::Popup::menu(&open).show(|ui| {
                if self.files.is_empty() {
                    ui.label(egui::RichText::new("No .animctrl files in the project").color(theme::TEXT_MUTED));
                }
                for f in self.files.clone() {
                    if ui.selectable_label(self.path.as_deref() == Some(f.as_str()), &f).clicked() {
                        self.open(root, &f);
                        ui.close();
                    }
                }
            });
            if self.path.is_some() {
                if widgets::icon_button_sized(ui, Icon::Target, "Frame all states", false, Vec2::splat(theme::H_FIELD)).clicked() {
                    self.pan = Vec2::ZERO;
                    self.zoom = 1.0;
                    let states = &self.ctrl.layers[self.layer].machine.states;
                    if !states.is_empty() {
                        let c = states.iter().fold(Vec2::ZERO, |a, s| a + Vec2::from(s.position)) / states.len() as f32;
                        self.pan = -c;
                    }
                }
                if let Some(i) = live {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        widgets::chip(ui, &format!("Live: {}", scene.objects[i].name));
                    });
                }
            }
        });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, root: &Path, scene: &mut LoadedScene, selected: Option<usize>) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG_VIEWPORT)).show_inside(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.3);
                ui.label(egui::RichText::new("No Animator Controller open").font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
                ui.label(
                    egui::RichText::new("Select a .animctrl in Files, press Open on an Animator, or create one.")
                        .color(theme::TEXT_MUTED),
                );
                ui.add_space(10.0);
                if widgets::primary_button(ui, Some(Icon::Plus), "Create Controller").clicked() {
                    create_asset(&root.join("animations"), true, root, self, &mut AnimClipTab::default());
                    self.open_request = false;
                    // Hook it to the selected object's Animator when there is one.
                    if let (Some(i), Some(path)) = (selected, self.path.clone())
                        && let Some(a) = animator_mut(scene, i)
                        && a.controller.is_empty()
                    {
                        a.controller = path;
                    }
                }
            });
        });
    }

    // ------------------------------------------------------------- left panel

    fn layers_ui(&mut self, ui: &mut egui::Ui, scene: &mut LoadedScene) {
        ui.horizontal(|ui| {
            widgets::eyebrow(ui, "Layers");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, Icon::Plus, "Add layer", false).clicked() {
                    let n = self.ctrl.layers.len();
                    self.ctrl.layers.push(AnimLayer { name: format!("Layer {n}"), ..Default::default() });
                    self.layer = n;
                    self.sel = Sel::None;
                    self.changed(scene);
                }
            });
        });
        let mut pick = None;
        for (k, l) in self.ctrl.layers.iter().enumerate() {
            let text = if k == 0 {
                l.name.clone()
            } else {
                format!("{}  ({}, {:.0}%)", l.name, if l.blend == LayerBlend::Additive { "add" } else { "override" }, l.weight * 100.0)
            };
            if ui.add_sized([ui.available_width(), theme::H_ROW], egui::Button::selectable(self.layer == k, text)).clicked() {
                pick = Some(k);
            }
        }
        if let Some(k) = pick {
            self.layer = k;
            self.sel = Sel::None;
        }
    }

    fn params_ui(&mut self, ui: &mut egui::Ui, scene: &mut LoadedScene, live: Option<usize>) {
        let mut edited = false;
        ui.horizontal(|ui| {
            widgets::eyebrow(ui, if live.is_some() { "Parameters (live)" } else { "Parameters" });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let add = widgets::icon_button(ui, Icon::Plus, "Add parameter", false);
                egui::Popup::menu(&add).show(|ui| {
                    for kind in AnimParamKind::ALL {
                        if ui.button(kind.label()).clicked() {
                            let mut name = kind.label().to_owned();
                            let mut n = 1;
                            while self.ctrl.parameter(&name).is_some() {
                                name = format!("{} {n}", kind.label());
                                n += 1;
                            }
                            self.ctrl.parameters.push(AnimParameter { name, kind, default: 0.0 });
                            edited = true;
                            ui.close();
                        }
                    }
                });
            });
        });
        if self.ctrl.parameters.is_empty() {
            ui.label(egui::RichText::new("None yet. Parameters drive transitions and blend trees.").size(theme::SIZE_META).color(theme::TEXT_DIM));
        }
        let mut remove = None;
        let mut rename: Option<(String, String)> = None;
        let count = self.ctrl.parameters.len();
        for k in 0..count {
            let p = self.ctrl.parameters[k].clone();
            ui.horizontal(|ui| {
                ui.set_min_height(theme::H_FIELD);
                ui.label(egui::RichText::new(kind_glyph(p.kind)).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_DIM))
                    .on_hover_text(p.kind.label());
                if let Some(new) = commit_text(ui, ui.id().with(("citrus-param-name", k)), &p.name, 96.0)
                    && self.ctrl.parameter(&new).is_none()
                {
                    rename = Some((p.name.clone(), new));
                }
                let mut value = match live.and_then(|i| animator(scene, i)) {
                    Some(a) => a.get_float(&p.name),
                    None => p.default,
                };
                let before = value;
                match p.kind {
                    AnimParamKind::Float => {
                        ui.add(egui::DragValue::new(&mut value).speed(0.02).max_decimals(3));
                    }
                    AnimParamKind::Int => {
                        let mut v = value.round() as i32;
                        ui.add(egui::DragValue::new(&mut v));
                        value = v as f32;
                    }
                    AnimParamKind::Bool => {
                        let mut b = value > 0.5;
                        ui.checkbox(&mut b, "");
                        value = if b { 1.0 } else { 0.0 };
                    }
                    AnimParamKind::Trigger => {
                        if live.is_some() {
                            if ui.small_button("Set").on_hover_text("Fire the trigger").clicked() {
                                value = 1.0;
                            }
                        } else {
                            let mut b = value > 0.5;
                            ui.checkbox(&mut b, "");
                            value = if b { 1.0 } else { 0.0 };
                        }
                    }
                }
                if value != before {
                    match live.and_then(|i| animator_mut(scene, i)) {
                        Some(a) => a.set_float(&p.name, value),
                        None => {
                            self.ctrl.parameters[k].default = value;
                            edited = true;
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, Icon::Trash, &format!("Remove {}", p.name), false).clicked() {
                        remove = Some(k);
                    }
                });
            });
        }
        if let Some((old, new)) = rename {
            self.ctrl.rename_parameter(&old, &new);
            edited = true;
        }
        if let Some(k) = remove {
            self.ctrl.parameters.remove(k);
            edited = true;
        }
        if edited {
            self.changed(scene);
        }
    }

    // ------------------------------------------------------------ inspector

    fn inspector_ui(&mut self, ui: &mut egui::Ui, scene: &mut LoadedScene, object: Option<usize>, root: &Path) {
        let mut edited = false;
        match self.sel {
            Sel::Node(Node::State(s)) if s < self.ctrl.layers[self.layer].machine.states.len() => {
                edited |= self.state_inspector(ui, scene, s, object, root);
            }
            Sel::Transition(t) if t < self.ctrl.layers[self.layer].machine.transitions.len() => {
                edited |= self.transition_inspector(ui, t);
            }
            Sel::Node(Node::Any) => {
                ui.label(egui::RichText::new("Any State").font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
                widgets::info_note(ui, "Transitions from Any State are checked first, from whatever state is playing. Right-click it and choose Make Transition.");
                edited |= self.transition_list(ui, None);
            }
            Sel::Node(Node::Entry) => {
                ui.label(egui::RichText::new("Entry").font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
                widgets::info_note(ui, "The layer starts in its default state (orange). Right-click a state and choose Set as Default, or drag from Entry to a state.");
            }
            _ => {
                edited |= self.layer_inspector(ui, scene, object);
            }
        }
        if edited {
            self.changed(scene);
        }
    }

    fn layer_inspector(&mut self, ui: &mut egui::Ui, scene: &LoadedScene, object: Option<usize>) -> bool {
        let mut edited = false;
        let li = self.layer;
        ui.label(egui::RichText::new("Layer").font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
        ui.add_space(4.0);
        widgets::property_row(ui, "Name", |ui| {
            if let Some(new) = commit_text(ui, ui.id().with(("citrus-layer-name", li)), &self.ctrl.layers[li].name, ui.available_width()) {
                self.ctrl.layers[li].name = new;
                edited = true;
            }
        });
        if li == 0 {
            widgets::info_note(ui, "The base layer always plays at full weight.");
        } else {
            let layer = &mut self.ctrl.layers[li];
            widgets::property_row(ui, "Weight", |ui| {
                edited |= citrus_editor::widgets::slider(ui, egui::Slider::new(&mut layer.weight, 0.0..=1.0)).changed();
            });
            widgets::property_row(ui, "Blending", |ui| {
                edited |= widgets::segmented(ui, &mut layer.blend, &[(LayerBlend::Override, "Override"), (LayerBlend::Additive, "Additive")]).changed();
            });
        }
        ui.add_space(6.0);
        widgets::eyebrow(ui, "Mask");
        ui.label(
            egui::RichText::new("Bones or child paths this layer affects (with their children). Empty = everything.")
                .size(theme::SIZE_META)
                .color(theme::TEXT_DIM),
        );
        let mut remove = None;
        for (k, m) in self.ctrl.layers[li].mask.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(m).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_BODY));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, Icon::Close, &format!("Remove {m}"), false).clicked() {
                        remove = Some(k);
                    }
                });
            });
        }
        if let Some(k) = remove {
            self.ctrl.layers[li].mask.remove(k);
            edited = true;
        }
        let candidates: Vec<String> = match object {
            Some(i) => {
                let bones: Vec<String> = scene.skeleton_rest_pose(i).into_iter().map(|(n, _)| n).collect();
                if bones.is_empty() {
                    crate::anim_graph::object_rig(scene, i).0.names.into_iter().filter(|n| !n.is_empty()).collect()
                } else {
                    bones
                }
            }
            None => Vec::new(),
        };
        let add = widgets::secondary_button(ui, Some(Icon::Plus), "Add to mask");
        egui::Popup::menu(&add).show(|ui| {
            if candidates.is_empty() {
                ui.label(egui::RichText::new("Select an object with an Animator to list its bones").color(theme::TEXT_MUTED));
            }
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for c in &candidates {
                    if ui.button(c).clicked() {
                        if !self.ctrl.layers[li].mask.contains(c) {
                            self.ctrl.layers[li].mask.push(c.clone());
                            edited = true;
                        }
                        ui.close();
                    }
                }
            });
        });
        if li > 0 {
            ui.add_space(10.0);
            if widgets::secondary_button(ui, Some(Icon::Trash), "Remove layer").clicked() {
                self.ctrl.layers.remove(li);
                self.layer = li - 1;
                edited = true;
            }
        }
        ui.add_space(10.0);
        widgets::info_note(
            ui,
            "Right-click the canvas to create states. Drag with Shift from a state (or use Make Transition) to connect two states. Scroll to zoom, drag empty space to pan.",
        );
        edited
    }

    fn clip_choices(&self, scene: &LoadedScene, object: Option<usize>) -> Vec<String> {
        let mut out: Vec<String> = object.and_then(|i| animator(scene, i)).map(|a| a.state.clips.clone()).unwrap_or_default();
        out.extend(self.clip_files.iter().cloned());
        out
    }

    fn state_inspector(&mut self, ui: &mut egui::Ui, scene: &mut LoadedScene, s: usize, object: Option<usize>, root: &Path) -> bool {
        let mut edited = false;
        let li = self.layer;
        let params = self.ctrl.parameters.clone();
        if self.clip_files.is_empty() {
            self.clip_files = scan_files(root, citrus_assets::ANIM_CLIP_EXTENSION);
        }
        let choices = self.clip_choices(scene, object);
        let machine = &mut self.ctrl.layers[li].machine;
        let is_default = machine.default_index() == Some(s);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("State").font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
            if is_default {
                widgets::chip(ui, "Default");
            }
        });
        ui.add_space(4.0);
        let name = machine.states[s].name.clone();
        widgets::property_row(ui, "Name", |ui| {
            if let Some(new) = commit_text(ui, ui.id().with(("citrus-state-name", li, s)), &name, ui.available_width())
                && machine.state_index(&new).is_none()
            {
                machine.rename_state(&name, &new);
                edited = true;
            }
        });
        let st = &mut machine.states[s];
        let mut is_tree = matches!(st.motion, Motion::BlendTree(_));
        widgets::property_row(ui, "Motion", |ui| {
            if widgets::segmented(ui, &mut is_tree, &[(false, "Clip"), (true, "Blend Tree")]).changed() {
                st.motion = if is_tree {
                    Motion::BlendTree(BlendTree {
                        name: st.name.clone(),
                        parameter: params.iter().find(|p| p.kind == AnimParamKind::Float).map(|p| p.name.clone()).unwrap_or_default(),
                        ..Default::default()
                    })
                } else {
                    Motion::Clip(String::new())
                };
                edited = true;
            }
        });
        match &mut st.motion {
            Motion::Clip(clip) => {
                widgets::property_row(ui, "Clip", |ui| {
                    edited |= clip_combo(ui, egui::Id::new(("citrus-state-clip", li, s)), clip, &choices);
                });
            }
            Motion::BlendTree(tree) => {
                edited |= blend_tree_ui(ui, tree, &params, &choices, (li, s));
            }
        }
        widgets::property_row(ui, "Speed", |ui| {
            edited |= ui.add(egui::DragValue::new(&mut st.speed).speed(0.02).range(-10.0..=10.0)).changed();
        });
        widgets::property_row(ui, "Speed param", |ui| {
            edited |= param_combo(ui, egui::Id::new(("citrus-state-speedp", li, s)), &mut st.speed_parameter, &params, true, |k| {
                k == AnimParamKind::Float
            });
        });
        widgets::property_row(ui, "Loop", |ui| edited |= ui.checkbox(&mut st.looping, "").changed());
        widgets::property_row(ui, "On enter", |ui| {
            edited |= ui.add(egui::TextEdit::singleline(&mut st.on_enter).hint_text("event name").desired_width(ui.available_width())).changed();
        });
        widgets::property_row(ui, "On exit", |ui| {
            edited |= ui.add(egui::TextEdit::singleline(&mut st.on_exit).hint_text("event name").desired_width(ui.available_width())).changed();
        });
        ui.label(
            egui::RichText::new("State events reach on_animation_event and AnimationEvent, like clip events.")
                .size(theme::SIZE_META)
                .color(theme::TEXT_DIM),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if !is_default && widgets::secondary_button(ui, None, "Set as Default").clicked() {
                machine.default_state = name.clone();
                edited = true;
            }
            if widgets::secondary_button(ui, Some(Icon::Trash), "Delete").clicked() {
                machine.remove_state(s);
                self.sel = Sel::None;
                edited = true;
            }
        });
        if let Sel::Node(Node::State(_)) = self.sel {
            widgets::section_gap(ui);
            edited |= self.transition_list(ui, Some(name));
        }
        edited
    }

    /// Outgoing transitions of a state (None = Any State), click to select.
    fn transition_list(&mut self, ui: &mut egui::Ui, from: Option<String>) -> bool {
        widgets::eyebrow(ui, "Transitions");
        let machine = &self.ctrl.layers[self.layer].machine;
        let list: Vec<(usize, String)> = machine
            .transitions
            .iter()
            .enumerate()
            .filter(|(_, t)| t.from == from)
            .map(|(k, t)| (k, t.to.clone()))
            .collect();
        if list.is_empty() {
            ui.label(egui::RichText::new("None. Shift-drag to a state to add one.").size(theme::SIZE_META).color(theme::TEXT_DIM));
        }
        for (k, to) in list {
            if ui.add_sized([ui.available_width(), theme::H_ROW], egui::Button::selectable(false, format!("{}  {to}", ph::ARROW_RIGHT))).clicked() {
                self.sel = Sel::Transition(k);
            }
        }
        false
    }

    fn transition_inspector(&mut self, ui: &mut egui::Ui, t: usize) -> bool {
        let mut edited = false;
        let params = self.ctrl.parameters.clone();
        let machine = &mut self.ctrl.layers[self.layer].machine;
        let tr = &mut machine.transitions[t];
        let from = tr.from.clone().unwrap_or_else(|| "Any State".into());
        ui.label(egui::RichText::new("Transition").font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
        ui.label(egui::RichText::new(format!("{from}  {}  {}", ph::ARROW_RIGHT, tr.to)).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_BODY));
        ui.add_space(6.0);
        widgets::property_row(ui, "Has exit time", |ui| edited |= ui.checkbox(&mut tr.has_exit_time, "").changed());
        ui.add_enabled_ui(tr.has_exit_time, |ui| {
            widgets::property_row(ui, "Exit time", |ui| {
                edited |= ui.add(egui::DragValue::new(&mut tr.exit_time).speed(0.01).range(0.0..=100.0)).on_hover_text("Normalized time of the source state").changed();
            });
        });
        widgets::property_row(ui, "Duration (s)", |ui| {
            edited |= ui.add(egui::DragValue::new(&mut tr.duration).speed(0.01).range(0.0..=10.0)).changed();
        });
        widgets::property_row(ui, "Offset", |ui| {
            edited |= ui.add(egui::DragValue::new(&mut tr.offset).speed(0.01).range(0.0..=1.0)).on_hover_text("Normalized start time in the target state").changed();
        });
        widgets::property_row(ui, "Interruptible", |ui| edited |= ui.checkbox(&mut tr.interruptible, "").changed());
        if tr.from.is_none() {
            widgets::property_row(ui, "To self", |ui| edited |= ui.checkbox(&mut tr.can_transition_to_self, "").changed());
        }
        ui.add_space(6.0);
        widgets::eyebrow(ui, "Conditions");
        let mut remove = None;
        for (k, c) in tr.conditions.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                edited |= param_combo(ui, egui::Id::new(("citrus-cond-param", t, k)), &mut c.parameter, &params, false, |_| true);
                let kind = params.iter().find(|p| p.name == c.parameter).map(|p| p.kind).unwrap_or_default();
                let ops: &[CondOp] = match kind {
                    AnimParamKind::Float => &[CondOp::Greater, CondOp::Less],
                    AnimParamKind::Int => &[CondOp::Greater, CondOp::Less, CondOp::Equals, CondOp::NotEquals],
                    AnimParamKind::Bool => &[CondOp::If, CondOp::IfNot],
                    AnimParamKind::Trigger => &[CondOp::If],
                };
                if !ops.contains(&c.op) {
                    c.op = ops[0];
                    edited = true;
                }
                egui::ComboBox::from_id_salt(("citrus-cond-op", t, k)).width(78.0).selected_text(c.op.label()).show_ui(ui, |ui| {
                    for &op in ops {
                        edited |= ui.selectable_value(&mut c.op, op, op.label()).changed();
                    }
                });
                if c.op.uses_threshold() {
                    let speed = if kind == AnimParamKind::Int { 1.0 } else { 0.02 };
                    edited |= ui.add(egui::DragValue::new(&mut c.threshold).speed(speed)).changed();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Remove condition", false).clicked() {
                        remove = Some(k);
                    }
                });
            });
        }
        if let Some(k) = remove {
            tr.conditions.remove(k);
            edited = true;
        }
        if widgets::dashed_button(ui, Some(Icon::Plus), "Add condition").clicked() {
            let first = params.first();
            tr.conditions.push(AnimCondition {
                parameter: first.map(|p| p.name.clone()).unwrap_or_default(),
                op: match first.map(|p| p.kind) {
                    Some(AnimParamKind::Float) | Some(AnimParamKind::Int) => CondOp::Greater,
                    _ => CondOp::If,
                },
                threshold: 0.0,
            });
            edited = true;
        }
        if !tr.has_exit_time && tr.conditions.is_empty() {
            ui.add_space(4.0);
            widgets::info_note(ui, "This transition never fires: add a condition or enable exit time.");
        }
        ui.add_space(10.0);
        if widgets::secondary_button(ui, Some(Icon::Trash), "Delete transition").clicked() {
            machine.transitions.remove(t);
            self.sel = Sel::None;
            edited = true;
        }
        edited
    }

    // --------------------------------------------------------------- canvas

    fn to_screen(&self, rect: Rect, p: [f32; 2]) -> Pos2 {
        rect.center() + (self.pan + Vec2::from(p)) * self.zoom
    }

    fn node_pos(&self, node: Node) -> [f32; 2] {
        let m = &self.ctrl.layers[self.layer].machine;
        match node {
            Node::State(i) => m.states.get(i).map(|s| s.position).unwrap_or_default(),
            Node::Any => m.any_state_position,
            Node::Entry => m.entry_position,
        }
    }

    fn node_rect(&self, rect: Rect, node: Node) -> Rect {
        let size = match node {
            Node::State(_) => NODE_SIZE,
            _ => Vec2::new(120.0, 32.0),
        } * self.zoom;
        Rect::from_center_size(self.to_screen(rect, self.node_pos(node)), size)
    }

    fn nodes(&self) -> Vec<Node> {
        let n = self.ctrl.layers[self.layer].machine.states.len();
        let mut v: Vec<Node> = vec![Node::Entry, Node::Any];
        v.extend((0..n).map(Node::State));
        v
    }

    fn node_at(&self, rect: Rect, p: Pos2) -> Option<Node> {
        self.nodes().into_iter().rev().find(|&n| self.node_rect(rect, n).contains(p))
    }

    /// Screen segment of each transition (offset sideways when a reverse one exists).
    fn transition_segments(&self, rect: Rect) -> Vec<(usize, Pos2, Pos2)> {
        let m = &self.ctrl.layers[self.layer].machine;
        let mut out = Vec::new();
        for (k, t) in m.transitions.iter().enumerate() {
            let Some(to) = m.state_index(&t.to) else {
                continue;
            };
            let from = match &t.from {
                None => Node::Any,
                Some(f) => match m.state_index(f) {
                    Some(i) => Node::State(i),
                    None => continue,
                },
            };
            let a = self.node_rect(rect, from).center();
            let b = self.node_rect(rect, Node::State(to)).center();
            let reverse = m.transitions.iter().any(|o| o.to == *t.from.as_deref().unwrap_or("") && o.from.as_deref() == Some(t.to.as_str()));
            let dir = (b - a).normalized();
            let side = if reverse { Vec2::new(-dir.y, dir.x) * 7.0 } else { Vec2::ZERO };
            out.push((k, a + side, b + side));
        }
        out
    }

    fn canvas(&mut self, ui: &mut egui::Ui, scene: &mut LoadedScene, live: Option<usize>) {
        let rect = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let mut edited = false;

        // Zoom around the pointer.
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0
                && let Some(p) = pointer
            {
                let before = (p - rect.center()) / self.zoom - self.pan;
                self.zoom = (self.zoom * (1.0 + scroll * 0.0015)).clamp(0.3, 2.5);
                self.pan = (p - rect.center()) / self.zoom - before;
            }
        }

        // Grid.
        let step = 32.0 * self.zoom;
        if step > 6.0 {
            let origin = rect.center() + self.pan * self.zoom;
            let mut x = rect.left() + (origin.x - rect.left()).rem_euclid(step);
            while x < rect.right() {
                painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.0_f32, theme::with_alpha(theme::LINE_PANEL, 0.6)));
                x += step;
            }
            let mut y = rect.top() + (origin.y - rect.top()).rem_euclid(step);
            while y < rect.bottom() {
                painter.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], Stroke::new(1.0_f32, theme::with_alpha(theme::LINE_PANEL, 0.6)));
                y += step;
            }
        }

        let hovered_node = pointer.filter(|p| rect.contains(*p)).and_then(|p| self.node_at(rect, p));
        let segments = self.transition_segments(rect);
        let hovered_tr = if hovered_node.is_none() {
            pointer.and_then(|p| {
                segments
                    .iter()
                    .map(|&(k, a, b)| (k, dist_to_segment(p, a, b)))
                    .filter(|&(_, d)| d < 6.0)
                    .min_by(|x, y| x.1.total_cmp(&y.1))
                    .map(|(k, _)| k)
            })
        } else {
            None
        };

        // Live playback of this layer.
        let lp = live.and_then(|i| animator(scene, i)).and_then(|a| a.state.layers.get(self.layer).cloned());

        // Transitions.
        for &(k, a, b) in &segments {
            let selected = self.sel == Sel::Transition(k);
            let active = lp.as_ref().is_some_and(|l| l.transition == Some(k) && l.in_transition());
            let color = if selected || active {
                theme::accent()
            } else if hovered_tr == Some(k) {
                theme::TEXT_SECONDARY
            } else {
                theme::TEXT_DIM
            };
            painter.line_segment([a, b], Stroke::new(if selected { 2.0_f32 } else { 1.5 }, color));
            if active && let Some(l) = &lp {
                let p = a + (b - a) * l.transition_progress();
                painter.line_segment([a, p], Stroke::new(3.0_f32, theme::accent()));
            }
            arrow_head(&painter, a, b, color, self.zoom);
        }

        // Entry -> default state.
        if let Some(d) = self.ctrl.layers[self.layer].machine.default_index() {
            let a = self.node_rect(rect, Node::Entry).center();
            let b = self.node_rect(rect, Node::State(d)).center();
            painter.line_segment([a, b], Stroke::new(1.5_f32, theme::with_alpha(theme::TYPE_LIGHT, 0.7)));
            arrow_head(&painter, a, b, theme::with_alpha(theme::TYPE_LIGHT, 0.7), self.zoom);
        }

        // Nodes.
        let default = self.ctrl.layers[self.layer].machine.default_index();
        for node in self.nodes() {
            let r = self.node_rect(rect, node);
            if !rect.intersects(r) {
                continue;
            }
            let (label, tint) = match node {
                Node::Entry => ("Entry".to_owned(), theme::STATUS_OK),
                Node::Any => ("Any State".to_owned(), theme::TYPE_UI),
                Node::State(i) => (
                    self.ctrl.layers[self.layer].machine.states[i].name.clone(),
                    if default == Some(i) { theme::TYPE_LIGHT } else { theme::TEXT_MUTED },
                ),
            };
            let current = matches!(node, Node::State(i) if lp.as_ref().is_some_and(|l| l.state == i));
            let previous = matches!(node, Node::State(i) if lp.as_ref().is_some_and(|l| l.previous.is_some_and(|(p, _)| p == i)));
            let fill = if current {
                theme::accent_faint()
            } else if matches!(node, Node::State(_)) {
                theme::BG_BUTTON
            } else {
                theme::with_alpha(tint, 0.18)
            };
            let selected = self.sel == Sel::Node(node);
            let stroke = if selected || current {
                Stroke::new(2.0_f32, theme::accent())
            } else if hovered_node == Some(node) {
                Stroke::new(1.0_f32, theme::TEXT_SECONDARY)
            } else if previous {
                Stroke::new(1.5_f32, theme::accent_soft())
            } else {
                Stroke::new(1.0_f32, theme::LINE_BUTTON)
            };
            painter.rect(r, CornerRadius::same(theme::R_ROW), fill, stroke, StrokeKind::Inside);
            if matches!(node, Node::State(_)) {
                painter.rect_filled(
                    Rect::from_min_size(r.min, Vec2::new(4.0 * self.zoom, r.height())),
                    CornerRadius { nw: theme::R_ROW, sw: theme::R_ROW, ne: 0, se: 0 },
                    tint,
                );
            }
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                &label,
                theme::semibold((theme::SIZE_BODY * self.zoom).clamp(8.0, 20.0)),
                theme::TEXT_STRONG,
            );
            if current && let Some(l) = &lp {
                let f = if self.ctrl.layers[self.layer].machine.states.get(l.state).is_some_and(|s| s.looping) {
                    l.normalized_time.fract()
                } else {
                    l.normalized_time.clamp(0.0, 1.0)
                };
                let bar = Rect::from_min_size(Pos2::new(r.left() + 6.0, r.bottom() - 5.0), Vec2::new((r.width() - 12.0) * f, 2.0));
                painter.rect_filled(bar, CornerRadius::ZERO, theme::accent());
            }
        }

        // Interaction.
        let shift = ui.input(|i| i.modifiers.shift);
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(p) = response.interact_pointer_pos() {
                match self.node_at(rect, p) {
                    Some(n) if shift || n == Node::Entry => self.linking = Some(n),
                    Some(n) => {
                        self.dragging = Some(n);
                        self.sel = Sel::Node(n);
                    }
                    None => {}
                }
            }
        if response.dragged_by(egui::PointerButton::Primary) || response.dragged_by(egui::PointerButton::Middle) {
            let d = response.drag_delta() / self.zoom;
            match self.dragging {
                Some(n) if response.dragged_by(egui::PointerButton::Primary) => {
                    let m = &mut self.ctrl.layers[self.layer].machine;
                    let slot = match n {
                        Node::State(i) => m.states.get_mut(i).map(|s| &mut s.position),
                        Node::Any => Some(&mut m.any_state_position),
                        Node::Entry => Some(&mut m.entry_position),
                    };
                    if let Some(slot) = slot {
                        slot[0] += d.x;
                        slot[1] += d.y;
                        edited = true;
                    }
                }
                _ if self.linking.is_none() => self.pan += d,
                _ => {}
            }
        }
        if let Some(from) = self.linking
            && let Some(p) = pointer
        {
            let a = self.node_rect(rect, from).center();
            painter.line_segment([a, p], Stroke::new(2.0_f32, theme::accent()));
            arrow_head(&painter, a, p, theme::accent(), self.zoom);
        }
        let released = response.drag_stopped_by(egui::PointerButton::Primary);
        let clicked = response.clicked_by(egui::PointerButton::Primary);
        if (released || clicked) && let Some(from) = self.linking {
            let target = response.interact_pointer_pos().or(pointer).and_then(|p| self.node_at(rect, p));
            match target {
                Some(Node::State(to)) if Some(Node::State(to)) != Some(from) || from == Node::Any => {
                    let m = &mut self.ctrl.layers[self.layer].machine;
                    let to_name = m.states[to].name.clone();
                    match from {
                        Node::Entry => m.default_state = to_name,
                        Node::Any => {
                            m.transitions.push(AnimTransition { from: None, to: to_name, ..Default::default() });
                            self.sel = Sel::Transition(m.transitions.len() - 1);
                        }
                        Node::State(f) => {
                            let from_name = m.states[f].name.clone();
                            // Unity's default: wait for the end of the source state.
                            m.transitions.push(AnimTransition {
                                from: Some(from_name),
                                to: to_name,
                                has_exit_time: true,
                                exit_time: 1.0,
                                ..Default::default()
                            });
                            self.sel = Sel::Transition(m.transitions.len() - 1);
                        }
                    }
                    edited = true;
                    self.linking = None;
                }
                Some(_) if released => self.linking = None,
                None => self.linking = None,
                _ => {}
            }
        } else if clicked {
            self.sel = match (hovered_node, hovered_tr) {
                (Some(n), _) => Sel::Node(n),
                (None, Some(k)) => Sel::Transition(k),
                _ => Sel::None,
            };
        }
        if released {
            self.dragging = None;
        }
        if response.secondary_clicked() {
            self.linking = None;
            if let Some(p) = response.interact_pointer_pos() {
                let canvas = (p - rect.center()) / self.zoom - self.pan;
                self.context = Some((hovered_node, hovered_tr, Pos2::new(canvas.x, canvas.y)));
                if let Some(n) = hovered_node {
                    self.sel = Sel::Node(n);
                } else if let Some(k) = hovered_tr {
                    self.sel = Sel::Transition(k);
                }
            }
        }
        response.context_menu(|ui| {
            let Some((mut node, mut tr, at)) = self.context else {
                ui.close();
                return;
            };
            let m = &self.ctrl.layers[self.layer].machine;
            if matches!(node, Some(Node::State(i)) if i >= m.states.len()) {
                node = None;
            }
            if tr.is_some_and(|k| k >= m.transitions.len()) {
                tr = None;
            }
            match (node, tr) {
                (Some(n @ Node::State(i)), _) => {
                    if ui.button("Make Transition").clicked() {
                        self.linking = Some(n);
                        ui.close();
                    }
                    if ui.button("Set as Layer Default").clicked() {
                        let m = &mut self.ctrl.layers[self.layer].machine;
                        m.default_state = m.states[i].name.clone();
                        edited = true;
                        ui.close();
                    }
                    if ui.button("Duplicate").clicked() {
                        let m = &mut self.ctrl.layers[self.layer].machine;
                        let mut copy = m.states[i].clone();
                        copy.name = m.unique_name(&copy.name);
                        copy.position[0] += 24.0;
                        copy.position[1] += 56.0;
                        m.states.push(copy);
                        self.sel = Sel::Node(Node::State(m.states.len() - 1));
                        edited = true;
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        self.ctrl.layers[self.layer].machine.remove_state(i);
                        self.sel = Sel::None;
                        edited = true;
                        ui.close();
                    }
                }
                (Some(n @ Node::Any), _) | (Some(n @ Node::Entry), _) => {
                    let label = if n == Node::Any { "Make Transition" } else { "Set Default State" };
                    if ui.button(label).clicked() {
                        self.linking = Some(n);
                        ui.close();
                    }
                }
                (None, Some(k)) => {
                    if ui.button("Delete Transition").clicked() {
                        self.ctrl.layers[self.layer].machine.transitions.remove(k);
                        self.sel = Sel::None;
                        edited = true;
                        ui.close();
                    }
                }
                (None, None) => {
                    for (label, tree) in [("Create State", false), ("Create Blend Tree State", true)] {
                        if ui.button(label).clicked() {
                            let params = &self.ctrl.parameters;
                            let float = params.iter().find(|p| p.kind == AnimParamKind::Float).map(|p| p.name.clone()).unwrap_or_default();
                            let m = &mut self.ctrl.layers[self.layer].machine;
                            let name = m.unique_name(if tree { "Blend Tree" } else { "New State" });
                            let motion = if tree {
                                Motion::BlendTree(BlendTree { name: name.clone(), parameter: float, ..Default::default() })
                            } else {
                                Motion::Clip(String::new())
                            };
                            m.states.push(AnimState { name: name.clone(), motion, position: [at.x, at.y], ..Default::default() });
                            if m.states.len() == 1 {
                                m.default_state = name;
                            }
                            self.sel = Sel::Node(Node::State(m.states.len() - 1));
                            edited = true;
                            ui.close();
                        }
                    }
                }
            }
        });

        // Keyboard: Delete removes the selection, Escape cancels linking.
        if response.hovered() || response.has_focus() {
            let (del, esc) = ui.input(|i| (i.key_pressed(egui::Key::Delete), i.key_pressed(egui::Key::Escape)));
            if esc {
                self.linking = None;
            }
            if del && !ui.ctx().wants_keyboard_input() {
                let m = &mut self.ctrl.layers[self.layer].machine;
                match self.sel {
                    Sel::Node(Node::State(i)) if i < m.states.len() => {
                        m.remove_state(i);
                        edited = true;
                    }
                    Sel::Transition(k) if k < m.transitions.len() => {
                        m.transitions.remove(k);
                        edited = true;
                    }
                    _ => {}
                }
                self.sel = Sel::None;
            }
        }

        let hint = if self.linking.is_some() {
            "Click a state to connect · Esc cancels"
        } else {
            "Right-click: create · Shift-drag: transition · Scroll: zoom · Drag empty: pan"
        };
        painter.text(
            rect.left_bottom() + Vec2::new(10.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            hint,
            egui::FontId::proportional(theme::SIZE_META),
            theme::TEXT_DIM,
        );
        if self.linking.is_some() || self.dragging.is_some() {
            ui.ctx().request_repaint();
        }
        if lp.is_some() {
            ui.ctx().request_repaint();
        }
        if edited {
            self.changed(scene);
        }
    }
}

fn kind_glyph(kind: AnimParamKind) -> &'static str {
    match kind {
        AnimParamKind::Float => "F",
        AnimParamKind::Int => "I",
        AnimParamKind::Bool => "B",
        AnimParamKind::Trigger => "T",
    }
}

fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
    (a + ab * t - p).length()
}

/// Triangle at the segment midpoint pointing from `a` to `b`.
fn arrow_head(painter: &egui::Painter, a: Pos2, b: Pos2, color: Color32, zoom: f32) {
    let dir = (b - a).normalized();
    if !dir.x.is_finite() {
        return;
    }
    let mid = a + (b - a) * 0.5;
    let s = 7.0 * zoom.clamp(0.6, 1.6);
    let side = Vec2::new(-dir.y, dir.x);
    let tip = mid + dir * s;
    painter.add(Shape::convex_polygon(vec![tip, mid - dir * s * 0.6 + side * s, mid - dir * s * 0.6 - side * s], color, Stroke::NONE));
}

/// Clip picker: known clips in a menu plus a free text field (any imported
/// clip name or a `.anim` path).
fn clip_combo(ui: &mut egui::Ui, id: egui::Id, value: &mut String, choices: &[String]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 30.0).max(60.0);
        changed |= ui.add(egui::TextEdit::singleline(value).hint_text("clip or .anim").desired_width(w)).changed();
        let pick = widgets::icon_button_sized(ui, Icon::Caret, "Choose a clip", false, Vec2::splat(theme::H_FIELD));
        egui::Popup::menu(&pick).id(id).show(|ui| {
            if choices.is_empty() {
                ui.label(egui::RichText::new("No clips found").color(theme::TEXT_MUTED));
            }
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for c in choices {
                    if ui.selectable_label(value == c, c).clicked() {
                        *value = c.clone();
                        changed = true;
                        ui.close();
                    }
                }
            });
        });
        // Drop a .anim from the Files panel.
        let dragging: Option<String> = ui.data(|d| d.get_temp(egui::Id::new(citrus_editor::DRAG_FILE_KEY)));
        if let Some(p) = dragging
            && p.to_ascii_lowercase().ends_with(".anim")
            && ui.rect_contains_pointer(ui.min_rect())
            && ui.input(|i| i.pointer.any_released())
        {
            *value = p;
            changed = true;
        }
    });
    changed
}

fn param_combo(
    ui: &mut egui::Ui,
    id: egui::Id,
    value: &mut String,
    params: &[AnimParameter],
    allow_none: bool,
    filter: impl Fn(AnimParamKind) -> bool,
) -> bool {
    let mut changed = false;
    let shown = if value.is_empty() { "None".to_owned() } else { value.clone() };
    egui::ComboBox::from_id_salt(id).width(110.0).selected_text(shown).show_ui(ui, |ui| {
        if allow_none {
            changed |= ui.selectable_value(value, String::new(), "None").changed();
        }
        for p in params.iter().filter(|p| filter(p.kind)) {
            changed |= ui.selectable_value(value, p.name.clone(), &p.name).changed();
        }
    });
    changed
}

fn blend_tree_ui(ui: &mut egui::Ui, tree: &mut BlendTree, params: &[AnimParameter], choices: &[String], salt: (usize, usize)) -> bool {
    let mut edited = false;
    let floats = |k: AnimParamKind| matches!(k, AnimParamKind::Float | AnimParamKind::Int);
    widgets::property_row(ui, "Parameter", |ui| {
        edited |= param_combo(ui, egui::Id::new(("citrus-tree-p", salt)), &mut tree.parameter, params, false, floats);
    });
    widgets::property_row(ui, "Parameter Y", |ui| {
        edited |= param_combo(ui, egui::Id::new(("citrus-tree-py", salt)), &mut tree.parameter_y, params, true, floats);
    });
    ui.label(
        egui::RichText::new(if tree.is_2d() {
            "2D: each motion at an (X, Y) parameter point."
        } else {
            "1D: each motion at a parameter threshold."
        })
        .size(theme::SIZE_META)
        .color(theme::TEXT_DIM),
    );
    let two_d = tree.is_2d();
    let mut remove = None;
    for (k, m) in tree.motions.iter_mut().enumerate() {
        ui.push_id(("citrus-tree-motion", salt, k), |ui| {
            ui.horizontal(|ui| {
                ui.set_max_width(ui.available_width());
                let w = if two_d { 110.0 } else { 150.0 };
                ui.allocate_ui(Vec2::new(w, theme::H_FIELD), |ui| {
                    edited |= clip_combo(ui, egui::Id::new(("citrus-tree-clip", salt, k)), &mut m.clip, choices);
                });
                edited |= ui.add(egui::DragValue::new(&mut m.position[0]).speed(0.05).max_decimals(2)).changed();
                if two_d {
                    edited |= ui.add(egui::DragValue::new(&mut m.position[1]).speed(0.05).max_decimals(2)).changed();
                }
                if widgets::icon_button(ui, Icon::Close, "Remove motion", false).clicked() {
                    remove = Some(k);
                }
            });
        });
    }
    if let Some(k) = remove {
        tree.motions.remove(k);
        edited = true;
    }
    if widgets::dashed_button(ui, Some(Icon::Plus), "Add motion").clicked() {
        let next = tree.motions.last().map(|m| m.position[0] + 1.0).unwrap_or(0.0);
        tree.motions.push(BlendMotion { clip: String::new(), position: [next, 0.0] });
        edited = true;
    }
    edited
}
