//! Scene hierarchy: a tree under a root "Scene" node with click-select,
//! collapse arrows, drag-to-reparent (drop a row onto another row to parent
//! it; drop onto the root or empty space to unparent), and a right-click
//! Add Object menu.

use std::collections::HashSet;

use egui::{FontId, RichText, ScrollArea, Sense, Ui};
use egui_phosphor::regular as ph;

use crate::icons::{self, Icon};
use crate::theme::{self, ObjectKind};
use crate::widgets;

pub struct SceneObjectRow {
    pub name: String,
    pub parent: Option<usize>,
    /// Type family: icon + colour.
    pub kind: ObjectKind,
    /// The object's own active flag (the eye toggles it).
    pub enabled: bool,
}

/// What to add from the scene tree's context menu. The engine maps this to
/// its object sources (the editor crate stays renderer/asset-agnostic).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnKind {
    Empty,
    Camera,
    Light(citrus_core::LightKind),
    LightProbeVolume,
    PostFxVolume,
    GlobalPostFxVolume,
    ReflectionProbe,
    FluxVolume,
    AudioSource,
    BoxCollider,
    SphereCollider,
    CapsuleCollider,
    /// A Mesh Collider on the selected mesh objects.
    MeshCollider,
    /// A 20 x 20 m lake.
    Water,
    /// A cube with a collider and a rigid body half as dense as water.
    FloatingCube,
    /// An open-top container with a rigid body that holds water.
    Bucket,
    Waterfall,
    Whirlpool,
    /// Wind for water waves, cloth and steam.
    WindZone,
    Cube,
    Sphere,
    Capsule,
    Plane,
    Ui(UiPreset),
}

/// Ready-made UI objects for the "UI" create menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiPreset {
    Canvas,
    WorldCanvas,
    Panel,
    Text,
    Button,
    Image,
    Toggle,
    Slider,
    TextField,
    ProgressBar,
}

impl UiPreset {
    pub const ALL: [UiPreset; 10] = [
        Self::Canvas,
        Self::WorldCanvas,
        Self::Panel,
        Self::Text,
        Self::Button,
        Self::Image,
        Self::Toggle,
        Self::Slider,
        Self::TextField,
        Self::ProgressBar,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Canvas => "Canvas",
            Self::WorldCanvas => "World Canvas (VR)",
            Self::Panel => "Panel",
            Self::Text => "Text",
            Self::Button => "Button",
            Self::Image => "Image",
            Self::Toggle => "Toggle",
            Self::Slider => "Slider",
            Self::TextField => "Text Field",
            Self::ProgressBar => "Progress Bar",
        }
    }
}

impl SpawnKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "Empty",
            Self::Camera => "Camera",
            Self::Light(kind) => kind.label(),
            Self::LightProbeVolume => "Light Probe Volume",
            Self::PostFxVolume => "Post FX Volume (Box)",
            Self::GlobalPostFxVolume => "Post FX Volume (Global)",
            Self::ReflectionProbe => "Reflection Probe",
            Self::FluxVolume => "Flux Volume",
            Self::AudioSource => "Audio Source",
            Self::BoxCollider => "Box Collider",
            Self::SphereCollider => "Sphere Collider",
            Self::CapsuleCollider => "Capsule Collider",
            Self::MeshCollider => "Mesh Collider",
            Self::Water => "Water",
            Self::FloatingCube => "Floating Cube",
            Self::Bucket => "Bucket",
            Self::Waterfall => "Waterfall",
            Self::Whirlpool => "Whirlpool",
            Self::WindZone => "Wind Zone",
            Self::Cube => "Cube",
            Self::Sphere => "Sphere",
            Self::Capsule => "Capsule",
            Self::Plane => "Plane",
            Self::Ui(p) => p.label(),
        }
    }
}

#[derive(Default)]
pub struct ScenePanelResponse {
    pub selection_changed: bool,
    /// (child, new parent) reparent requests (keep Vec position).
    pub reparent: Vec<(usize, Option<usize>)>,
    /// (child, new parent, before-sibling) move/reorder requests. `before`
    /// None = append at the end of the new parent's children.
    pub moves: Vec<(usize, Option<usize>, Option<usize>)>,
    /// Object requested via the context menu.
    pub spawn: Option<SpawnKind>,
    /// Object index requested for deletion (context menu / Delete key).
    pub delete: Option<usize>,
    /// (index, new name) inline-rename commit (F2 in the tree).
    pub rename: Option<(usize, String)>,
    /// A model file dropped onto the tree (import it into the scene).
    pub import_model: Option<std::path::PathBuf>,
    /// Eye clicked: flip this object's active flag.
    pub toggle_enabled: Option<usize>,
    /// "Group Into Empty": these objects go under a new Empty.
    pub group: Option<Vec<usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DropZone {
    /// Top of the row: drop as a sibling before it.
    Before,
    /// Middle of the row: drop as a child (reparent).
    Onto,
    /// Bottom of the row: drop as a sibling after it.
    After,
}

fn drop_zone(rect: egui::Rect, y: f32) -> DropZone {
    let t = (y - rect.top()) / rect.height().max(1.0);
    if t < 0.30 {
        DropZone::Before
    } else if t > 0.70 {
        DropZone::After
    } else {
        DropZone::Onto
    }
}

/// Next sibling of `index` (same parent, next in display order), if any.
fn next_sibling(rows: &[SceneObjectRow], index: usize) -> Option<usize> {
    let parent = rows[index].parent;
    (index + 1..rows.len()).find(|&j| rows[j].parent == parent)
}

fn spawn_menu(ui: &mut Ui, response: &mut ScenePanelResponse) {
    ui.label(RichText::new("Add Object").small().weak());
    for kind in [SpawnKind::Empty, SpawnKind::Camera] {
        if ui.button(kind.label()).clicked() {
            response.spawn = Some(kind);
            ui.close();
        }
    }
    ui.menu_button("Water", |ui| {
        for kind in [SpawnKind::Water, SpawnKind::Waterfall, SpawnKind::Whirlpool, SpawnKind::FloatingCube, SpawnKind::Bucket] {
            if ui.button(kind.label()).clicked() {
                response.spawn = Some(kind);
                ui.close();
            }
        }
    });
    ui.menu_button("Light", |ui| {
        for kind in citrus_core::LightKind::ALL {
            if ui.button(kind.label()).clicked() {
                response.spawn = Some(SpawnKind::Light(kind));
                ui.close();
            }
        }
    });
    ui.menu_button("Volume", |ui| {
        for kind in [
            SpawnKind::PostFxVolume,
            SpawnKind::GlobalPostFxVolume,
            SpawnKind::LightProbeVolume,
            SpawnKind::ReflectionProbe,
            SpawnKind::FluxVolume,
        ] {
            if ui.button(kind.label()).clicked() {
                response.spawn = Some(kind);
                ui.close();
            }
        }
    });
    ui.menu_button("3D Object", |ui| {
        for kind in [
            SpawnKind::Cube,
            SpawnKind::Sphere,
            SpawnKind::Capsule,
            SpawnKind::Plane,
        ] {
            if ui.button(kind.label()).clicked() {
                response.spawn = Some(kind);
                ui.close();
            }
        }
    });
    ui.menu_button("UI", |ui| {
        for preset in UiPreset::ALL {
            if ui.button(preset.label()).clicked() {
                response.spawn = Some(SpawnKind::Ui(preset));
                ui.close();
            }
        }
    });
    if ui.button("Audio Source").clicked() {
        response.spawn = Some(SpawnKind::AudioSource);
        ui.close();
    }
    ui.menu_button("Physics", |ui| {
        if ui.button(SpawnKind::WindZone.label()).clicked() {
            response.spawn = Some(SpawnKind::WindZone);
            ui.close();
        }
    });
    ui.menu_button("Collider", |ui| {
        if ui.button("Box Collider").clicked() {
            response.spawn = Some(SpawnKind::BoxCollider);
            ui.close();
        }
        if ui.button("Sphere Collider").clicked() {
            response.spawn = Some(SpawnKind::SphereCollider);
            ui.close();
        }
        if ui.button("Capsule Collider").clicked() {
            response.spawn = Some(SpawnKind::CapsuleCollider);
            ui.close();
        }
        if ui.button("Mesh Collider").on_hover_text("Add a Mesh Collider to the selected mesh objects").clicked() {
            response.spawn = Some(SpawnKind::MeshCollider);
            ui.close();
        }
    });
}

#[derive(Default)]
pub struct ScenePanel {
    filter: String,
    collapsed: HashSet<usize>,
    /// Inline rename in progress: (object index, edit buffer, focus-on-first-frame).
    renaming: Option<(usize, String, bool)>,
    /// Full multi-selection (object indices). Synced from the engine's canonical
    /// selection each frame via [`set_multi`], mutated here on ctrl/shift clicks,
    /// and read back after `ui` when `selection_changed`. The `selected` anchor
    /// (last-clicked) is passed in/out separately and is always also in `multi`.
    multi: HashSet<usize>,
    /// The tree has keyboard focus (last click landed in it): arrow keys
    /// navigate.
    focused: bool,
    /// Row to scroll into view this frame (after keyboard navigation).
    scroll_to: Option<usize>,
    /// Selection as of last frame, to notice picks made elsewhere (viewport).
    last_selected: Option<usize>,
}

impl ScenePanel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sync the multi-selection from the engine's canonical set before drawing.
    pub fn set_multi(&mut self, set: impl IntoIterator<Item = usize>) {
        self.multi = set.into_iter().collect();
    }

    /// The current multi-selection (read after `ui` when `selection_changed`).
    pub fn multi(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.multi.iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// Indices of currently-collapsed rows (for persisting tree state).
    pub fn collapsed_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.collapsed.iter().copied()
    }

    /// Restore the collapsed-row set (from the saved scene tree state).
    pub fn set_collapsed(&mut self, set: impl IntoIterator<Item = usize>) {
        self.collapsed = set.into_iter().collect();
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        rows: &[SceneObjectRow],
        selected: &mut Option<usize>,
    ) -> ScenePanelResponse {
        let mut response = ScenePanelResponse::default();
        // Selected elsewhere (the viewport): open every collapsed parent above
        // it and scroll its row into view.
        if *selected != self.last_selected
            && let Some(sel) = *selected
            && sel < rows.len()
        {
            let mut p = rows[sel].parent;
            let mut guard = 0;
            while let Some(i) = p.filter(|&i| i < rows.len()) {
                self.collapsed.remove(&i);
                p = rows[i].parent;
                guard += 1;
                if guard > rows.len() {
                    break;
                }
            }
            self.scroll_to = Some(sel);
        }
        let panel_rect = ui.max_rect();
        if let Some(p) = ui.input(|i| i.pointer.any_pressed().then(|| i.pointer.interact_pos()).flatten()) {
            self.focused = panel_rect.contains(p);
        }
        // Header strip: title + count, Add menu, kebab.
        egui::Frame::new().inner_margin(egui::Margin::symmetric(theme::PAD as i8, 0)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_height(theme::H_TAB_STRIP);
                ui.label(RichText::new("Scene").font(theme::semibold(theme::SIZE_BODY)).color(theme::TEXT_STRONG));
                widgets::badge(ui, &rows.len().to_string());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let kebab = widgets::icon_button(ui, Icon::Kebab, "More", false);
                    egui::Popup::menu(&kebab).show(|ui| {
                        if ui.button("Expand all").clicked() {
                            self.collapsed.clear();
                            ui.close();
                        }
                        if ui.button("Collapse all").clicked() {
                            self.collapsed = (0..rows.len()).collect();
                            ui.close();
                        }
                    });
                    let add = widgets::secondary_button_sized(ui, Some(Icon::Plus), "Add", theme::H_BUTTON_SMALL);
                    egui::Popup::menu(&add).show(|ui| spawn_menu(ui, &mut response));
                });
            });
        });
        widgets::divider(ui);
        egui::Frame::new().inner_margin(egui::Margin::same(8)).show(ui, |ui| {
            let w = ui.available_width();
            widgets::search_field(ui, &mut self.filter, "Filter by name or type", w);
        });
        let filter = self.filter.to_lowercase();

        // F2 starts an inline rename of the selected object, unless a text
        // field is already focused (e.g. the filter box).
        if self.renaming.is_none()
            && let Some(i) = *selected
            && i < rows.len()
            && ui.input(|inp| inp.key_pressed(egui::Key::F2))
            && !ui.memory(|m| m.focused().is_some())
        {
            self.renaming = Some((i, rows[i].name.clone(), true));
        }

        // children[i] = indices whose parent == i; roots have no parent.
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
        let mut roots: Vec<usize> = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            match row.parent {
                Some(p) if p < rows.len() && p != i => children[p].push(i),
                _ => roots.push(i),
            }
        }

        if self.focused && self.renaming.is_none() && !ui.memory(|m| m.focused().is_some()) {
            self.keyboard_nav(ui, rows, &children, &roots, &filter, selected, &mut response);
        }

        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Background click sensor over the whole tree, registered BEFORE
                // the rows so the rows (drawn after) take hit priority — a click
                // that lands here (empty space) clears the selection. More
                // reliable than a trailing rect, which can be mis-sized.
                let deselect_bg =
                    ui.interact(ui.max_rect(), ui.id().with("tree-deselect-bg"), Sense::click());
                ui.spacing_mut().item_spacing.y = 0.0;
                let hidden = hidden_flags(rows);
                if filter.is_empty() {
                    for &index in &roots {
                        self.tree_row(ui, rows, &children, &hidden, index, 0, selected, &mut response);
                    }
                } else {
                    // Filtered: flat list, matching name or type.
                    for (i, row) in rows.iter().enumerate() {
                        if row.name.to_lowercase().contains(&filter) || row.kind.label().to_lowercase().contains(&filter) {
                            self.object_row(ui, rows, &children, &hidden, false, i, 0, selected, &mut response);
                        }
                    }
                }
                if rows.is_empty() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(theme::PAD);
                        ui.label(RichText::new("Scene is empty").color(theme::TEXT_DIM));
                    });
                }
                // Remaining empty space: drop target to unparent and
                // right-click target for the Add Object menu.
                let mut rest = ui.available_rect_before_wrap();
                rest.set_height(rest.height().max(24.0));
                let bg = ui.allocate_rect(rest, Sense::click());
                if let Some(payload) = bg.dnd_release_payload::<usize>() {
                    response.reparent.push((*payload, None));
                }
                // A model file dragged from the Files panel onto the tree imports it.
                if let Some(payload) = bg.dnd_release_payload::<std::path::PathBuf>()
                    && payload
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fbx" | "gltf" | "glb" | "vrm" | "obj"))
                        .unwrap_or(false)
                {
                    response.import_model = Some((*payload).clone());
                }
                // A left-click on empty tree space (not on a row) clears the
                // selection. `deselect_bg` covers the area around rows; `bg` is
                // the trailing rect that fills the (often large) area BELOW the
                // last row and sits on top there — so check both.
                if deselect_bg.clicked() || bg.clicked() {
                    *selected = None;
                    self.multi.clear();
                    response.selection_changed = true;
                }
                bg.context_menu(|ui| spawn_menu(ui, &mut response));
            });
        self.last_selected = *selected;
        response
    }

    /// Arrow keys: Up / Down step through the visible rows, Right expands (or
    /// steps into the first child), Left collapses (or steps out to the parent).
    #[allow(clippy::too_many_arguments)]
    fn keyboard_nav(
        &mut self,
        ui: &Ui,
        rows: &[SceneObjectRow],
        children: &[Vec<usize>],
        roots: &[usize],
        filter: &str,
        selected: &mut Option<usize>,
        response: &mut ScenePanelResponse,
    ) {
        let (up, down, left, right) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
            )
        });
        if !(up || down || left || right) || rows.is_empty() {
            return;
        }
        let visible: Vec<usize> = if filter.is_empty() {
            let mut out = Vec::new();
            let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
            while let Some(n) = stack.pop() {
                out.push(n);
                if !self.collapsed.contains(&n) {
                    stack.extend(children[n].iter().rev().copied());
                }
            }
            out
        } else {
            (0..rows.len())
                .filter(|&i| rows[i].name.to_lowercase().contains(filter) || rows[i].kind.label().to_lowercase().contains(filter))
                .collect()
        };
        if visible.is_empty() {
            return;
        }
        let cur = selected.filter(|i| *i < rows.len());
        let pos = cur.and_then(|c| visible.iter().position(|&v| v == c));
        let mut target = None;
        if up || down {
            target = Some(match pos {
                None => if down { visible[0] } else { visible[visible.len() - 1] },
                Some(p) if up => visible[p.saturating_sub(1)],
                Some(p) => visible[(p + 1).min(visible.len() - 1)],
            });
        } else if let Some(c) = cur {
            let has_children = !children[c].is_empty() && filter.is_empty();
            if right && has_children {
                if self.collapsed.remove(&c) {
                    self.scroll_to = Some(c);
                } else {
                    target = Some(children[c][0]);
                }
            } else if left {
                if has_children && !self.collapsed.contains(&c) {
                    self.collapsed.insert(c);
                    self.scroll_to = Some(c);
                } else if let Some(p) = rows[c].parent.filter(|p| *p < rows.len()) {
                    target = Some(p);
                }
            }
        }
        if let Some(t) = target.filter(|t| Some(*t) != cur) {
            *selected = Some(t);
            self.multi.clear();
            self.multi.insert(t);
            self.scroll_to = Some(t);
            response.selection_changed = true;
        }
    }

    /// Collapse (`collapse=true`) or expand the whole subtree rooted at
    /// `index`, inclusive. Used by Alt+click.
    fn cascade(&mut self, children: &[Vec<usize>], index: usize, collapse: bool) {
        let mut stack = vec![index];
        while let Some(n) = stack.pop() {
            if collapse {
                self.collapsed.insert(n);
            } else {
                self.collapsed.remove(&n);
            }
            stack.extend(children[n].iter().copied());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn tree_row(
        &mut self,
        ui: &mut Ui,
        rows: &[SceneObjectRow],
        children: &[Vec<usize>],
        hidden: &[bool],
        index: usize,
        depth: usize,
        selected: &mut Option<usize>,
        response: &mut ScenePanelResponse,
    ) {
        if depth > 64 {
            return;
        }
        let has_children = !children[index].is_empty();
        self.object_row(ui, rows, children, hidden, has_children, index, depth, selected, response);
        if has_children && !self.collapsed.contains(&index) {
            for &child in &children[index] {
                self.tree_row(ui, rows, children, hidden, child, depth + 1, selected, response);
            }
        }
    }

    /// One 26 px row: caret (space kept on leaves), type icon, name, child
    /// count when collapsed, and the eye toggle on the right. Returns the row
    /// response and whether the eye was clicked.
    #[allow(clippy::too_many_arguments)]
    fn row_widget(
        &mut self,
        ui: &mut Ui,
        row_data: &SceneObjectRow,
        depth: usize,
        is_selected: bool,
        child_count: usize,
        collapsed: bool,
        hidden: bool,
    ) -> (egui::Response, bool) {
        let (rect, row) = ui.allocate_exact_size(egui::vec2(ui.available_width(), theme::H_ROW), Sense::click_and_drag());
        let inner = rect.shrink2(egui::vec2(6.0, 1.0));
        if is_selected {
            ui.painter().rect_filled(inner, theme::R_ROW, theme::accent_faint());
        } else if row.hovered() {
            ui.painter().rect_filled(inner, theme::R_ROW, theme::with_alpha(theme::BG_BUTTON, 0.6));
        }
        let x0 = inner.left() + 4.0 + depth as f32 * theme::INDENT;
        let cy = rect.center().y;
        if child_count > 0 {
            crate::widgets::paint_caret(
                ui.painter(),
                egui::Rect::from_center_size(egui::pos2(x0 + 8.0, cy), egui::vec2(12.0, 12.0)),
                !collapsed,
                theme::TEXT_DIM,
            );
        }
        let icon_color = if hidden { theme::with_alpha(row_data.kind.color(), 0.45) } else { row_data.kind.color() };
        icons::paint(
            ui.painter(),
            egui::Rect::from_center_size(egui::pos2(x0 + 16.0 + 9.0, cy), egui::vec2(14.0, 14.0)),
            row_data.kind.icon(),
            icon_color,
        );
        // Eye toggle (22 px) on the right.
        let eye_rect = egui::Rect::from_center_size(egui::pos2(inner.right() - 13.0, cy), egui::vec2(22.0, 22.0));
        let eye = ui.interact(eye_rect, row.id.with("eye"), Sense::click());
        let eye_color = if hidden {
            theme::TEXT_SECONDARY
        } else if eye.hovered() {
            theme::TEXT_STRONG
        } else if is_selected {
            theme::TEXT_MUTED
        } else {
            theme::EYE_QUIET
        };
        icons::paint(
            ui.painter(),
            egui::Rect::from_center_size(eye_rect.center(), egui::vec2(14.0, 14.0)),
            if row_data.enabled && !hidden { Icon::Eye } else { Icon::EyeOff },
            eye_color,
        );
        let eye_clicked = eye.on_hover_text(if row_data.enabled { format!("Hide {}", row_data.name) } else { format!("Show {}", row_data.name) }).clicked();

        let name_color = if hidden {
            theme::TEXT_HIDDEN
        } else if is_selected {
            theme::TEXT_SELECTED
        } else {
            theme::TEXT_SECONDARY
        };
        let name_left = x0 + 16.0 + 22.0;
        let mut name_right = eye_rect.left() - 6.0;
        if collapsed && child_count > 0 {
            let text = child_count.to_string();
            let galley = ui.painter().layout_no_wrap(text, theme::mono(theme::SIZE_TINY), theme::TEXT_MUTED);
            let badge = egui::Rect::from_center_size(
                egui::pos2(name_right - galley.size().x * 0.5 - 5.0, cy),
                egui::vec2(galley.size().x + 10.0, 16.0),
            );
            ui.painter().rect_filled(badge, theme::R_FIELD, theme::LINE_PANEL);
            ui.painter().galley(badge.center() - galley.size() * 0.5, galley, theme::TEXT_MUTED);
            name_right = badge.left() - 6.0;
        }
        let font = FontId::proportional(theme::SIZE_BODY);
        let mut job = egui::text::LayoutJob::simple_singleline(row_data.name.clone(), font, name_color);
        job.wrap = egui::text::TextWrapping::truncate_at_width((name_right - name_left).max(0.0));
        let galley = ui.painter().layout_job(job);
        ui.painter().galley(egui::pos2(name_left, cy - galley.size().y * 0.5), galley, name_color);
        (row, eye_clicked)
    }

    #[allow(clippy::too_many_arguments)]
    fn object_row(
        &mut self,
        ui: &mut Ui,
        rows: &[SceneObjectRow],
        children: &[Vec<usize>],
        hidden: &[bool],
        has_children: bool,
        index: usize,
        depth: usize,
        selected: &mut Option<usize>,
        response: &mut ScenePanelResponse,
    ) {
        let row_data = &rows[index];
        // Highlight the anchor AND every multi-selected row.
        let is_selected = *selected == Some(index) || self.multi.contains(&index);
        let collapsed = self.collapsed.contains(&index);

        // Inline rename editor in place of the normal row.
        if matches!(&self.renaming, Some((i, _, _)) if *i == index) {
            let indent = 10.0 + depth as f32 * theme::INDENT + 38.0;
            let mut commit = false;
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.add_space(indent);
                if let Some((_, text, focus)) = &mut self.renaming {
                    let edit = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                    if *focus {
                        edit.request_focus();
                        *focus = false;
                    }
                    if edit.lost_focus() {
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            cancel = true;
                        } else {
                            commit = true;
                        }
                    }
                }
            });
            if commit {
                if let Some((i, text, _)) = self.renaming.take() {
                    let name = text.trim().to_owned();
                    if !name.is_empty() && name != rows[i].name {
                        response.rename = Some((i, name));
                    }
                }
            } else if cancel {
                self.renaming = None;
            }
            return;
        }

        let (row, eye_clicked) = self.row_widget(
            ui,
            row_data,
            depth,
            is_selected,
            if has_children { children[index].len() } else { 0 },
            collapsed,
            hidden[index],
        );
        if self.scroll_to == Some(index) {
            row.scroll_to_me(None);
            self.scroll_to = None;
        }
        if eye_clicked {
            response.toggle_enabled = Some(index);
            return;
        }
        let indent = 10.0 + depth as f32 * theme::INDENT;

        row.dnd_set_drag_payload(index);
        // Also publish the dragged index into egui memory as a plain usize, so
        // ObjectRef drop boxes (incl. plugin inspectors with their own egui)
        // can read it without egui's DragAndDrop plugin. Cleared each frame the
        // pointer is up (engine, end of frame).
        if row.dragged() {
            ui.data_mut(|d| d.insert_temp(egui::Id::new(crate::DRAG_OBJECT_KEY), index));
            // Floating "ghost" of the dragged row, following the cursor, so the
            // drag has visual feedback (and the same chip lands in an ObjectRef
            // drop box). Painted on a top layer without any egui dnd context.
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("citrus-drag-ghost"),
                ));
                let font = egui::TextStyle::Body.resolve(ui.style());
                let galley =
                    painter.layout_no_wrap(row_data.name.clone(), font, egui::Color32::WHITE);
                let pad = egui::vec2(8.0, 4.0);
                let origin = pos + egui::vec2(14.0, 6.0);
                let rect = egui::Rect::from_min_size(origin, galley.size() + pad * 2.0);
                painter.rect_filled(rect, theme::R_ROW, theme::BG_SEGMENT_ACTIVE);
                painter.rect_stroke(rect, theme::R_ROW, egui::Stroke::new(1.0_f32, theme::accent()), egui::StrokeKind::Inside);
                painter.galley(origin + pad, galley, theme::TEXT_STRONG);
            }
        }
        if row.clicked() {
            let on_arrow = has_children
                && row
                    .interact_pointer_pos()
                    .is_some_and(|p| p.x < row.rect.left() + indent + 16.0);
            if on_arrow {
                let collapse = !self.collapsed.contains(&index);
                // Alt: cascade the whole subtree; plain click toggles one node.
                if ui.input(|i| i.modifiers.alt) {
                    self.cascade(children, index, collapse);
                } else if collapse {
                    self.collapsed.insert(index);
                } else {
                    self.collapsed.remove(&index);
                }
            } else {
                let mods = ui.input(|i| i.modifiers);
                if mods.command {
                    // Ctrl/Cmd: toggle this row in/out of the multi-selection.
                    if self.multi.contains(&index) && self.multi.len() > 1 {
                        self.multi.remove(&index);
                        // Keep the anchor valid (any remaining member).
                        if *selected == Some(index) {
                            *selected = self.multi.iter().next().copied();
                        }
                    } else {
                        self.multi.insert(index);
                        *selected = Some(index);
                    }
                } else if mods.shift {
                    // Shift: select the index range between the anchor and here
                    // (scene-order approximation of "everything in between").
                    if let Some(a) = *selected {
                        let (lo, hi) = (a.min(index), a.max(index));
                        for i in lo..=hi {
                            self.multi.insert(i);
                        }
                    }
                    self.multi.insert(index);
                    *selected = Some(index);
                } else {
                    // Plain click: single-select (clear the multi-selection). A
                    // click on the lone selection toggles it off.
                    let only = self.multi.len() <= 1 && *selected == Some(index);
                    self.multi.clear();
                    if only {
                        *selected = None;
                    } else {
                        self.multi.insert(index);
                        *selected = Some(index);
                    }
                }
                response.selection_changed = true;
            }
        }
        // Drag-drop: top third inserts before (reorder), middle reparents,
        // bottom third inserts after (reorder).
        let cursor_y = ui.ctx().pointer_latest_pos().map(|p| p.y);
        if let Some(payload) = row.dnd_release_payload::<usize>() {
            let child = *payload;
            if child != index {
                let zone = cursor_y.map_or(DropZone::Onto, |y| drop_zone(row.rect, y));
                let parent = rows[index].parent;
                match zone {
                    DropZone::Onto => response.moves.push((child, Some(index), None)),
                    DropZone::Before => response.moves.push((child, parent, Some(index))),
                    DropZone::After => {
                        response
                            .moves
                            .push((child, parent, next_sibling(rows, index)))
                    }
                }
            }
        } else if row
            .dnd_hover_payload::<usize>()
            .is_some_and(|p| *p != index)
        {
            let color = theme::accent();
            let painter = ui.painter();
            match cursor_y.map_or(DropZone::Onto, |y| drop_zone(row.rect, y)) {
                DropZone::Onto => {
                    painter.rect_stroke(
                        row.rect,
                        3.0,
                        egui::Stroke::new(1.5_f32, color),
                        egui::StrokeKind::Outside,
                    );
                }
                DropZone::Before => {
                    painter.hline(
                        row.rect.left()..=row.rect.right(),
                        row.rect.top(),
                        egui::Stroke::new(2.5_f32, color),
                    );
                }
                DropZone::After => {
                    painter.hline(
                        row.rect.left()..=row.rect.right(),
                        row.rect.bottom(),
                        egui::Stroke::new(2.5_f32, color),
                    );
                }
            }
        }
        row.context_menu(|ui| {
            let selected: Vec<usize> = if self.multi.contains(&index) { self.multi.iter().copied().collect() } else { vec![index] };
            if ui
                .button(format!("{} Group Into Empty", ph::FOLDER_SIMPLE_PLUS))
                .on_hover_text("Put the selected objects under a new empty at their centre")
                .clicked()
            {
                response.group = Some(selected);
                ui.close();
            }
            if rows[index].parent.is_some() && ui.button("Unparent").clicked() {
                response.reparent.push((index, None));
                ui.close();
            }
            if ui
                .button(format!("{} Delete", ph::TRASH))
                .on_hover_text("Delete this object and its children")
                .clicked()
            {
                response.delete = Some(index);
                ui.close();
            }
            ui.separator();
            spawn_menu(ui, response);
        });
    }
}

/// Rows hidden by their own flag or any hidden ancestor.
fn hidden_flags(rows: &[SceneObjectRow]) -> Vec<bool> {
    (0..rows.len())
        .map(|i| {
            let mut k = Some(i);
            let mut depth = 0;
            while let Some(j) = k.filter(|&j| j < rows.len()) {
                if !rows[j].enabled {
                    return true;
                }
                depth += 1;
                if depth > 64 {
                    break;
                }
                k = rows[j].parent;
            }
            false
        })
        .collect()
}
