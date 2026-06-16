//! Scene hierarchy: a tree under a root "Scene" node with click-select,
//! collapse arrows, drag-to-reparent (drop a row onto another row to parent
//! it; drop onto the root or empty space to unparent), and a right-click
//! Add Object menu.

use std::collections::HashSet;

use egui::{Align2, FontId, RichText, ScrollArea, Sense, Ui};

pub struct SceneObjectRow {
    pub name: String,
    pub parent: Option<usize>,
    /// Small glyph for the object kind (mesh/empty/camera).
    pub icon: &'static str,
    /// Disabled objects render dimmed (gray) in the tree.
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
    ReflectionProbe,
    FluxVolume,
    AudioSource,
    BoxCollider,
    SphereCollider,
    Cube,
    Sphere,
    Capsule,
    Plane,
}

impl SpawnKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "Empty",
            Self::Camera => "Camera",
            Self::Light(kind) => kind.label(),
            Self::LightProbeVolume => "Light Probe Volume",
            Self::PostFxVolume => "Post FX Volume",
            Self::ReflectionProbe => "Reflection Probe",
            Self::FluxVolume => "Flux Volume",
            Self::AudioSource => "Audio Source",
            Self::BoxCollider => "Box Collider",
            Self::SphereCollider => "Sphere Collider",
            Self::Cube => "Cube",
            Self::Sphere => "Sphere",
            Self::Capsule => "Capsule",
            Self::Plane => "Plane",
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
    ui.menu_button("Light", |ui| {
        for kind in citrus_core::LightKind::ALL {
            if ui.button(kind.label()).clicked() {
                response.spawn = Some(SpawnKind::Light(kind));
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Light Probe Volume").clicked() {
            response.spawn = Some(SpawnKind::LightProbeVolume);
            ui.close();
        }
        if ui.button("Reflection Probe").clicked() {
            response.spawn = Some(SpawnKind::ReflectionProbe);
            ui.close();
        }
        if ui.button("Flux Volume").clicked() {
            response.spawn = Some(SpawnKind::FluxVolume);
            ui.close();
        }
        if ui.button("Post FX Volume").clicked() {
            response.spawn = Some(SpawnKind::PostFxVolume);
            ui.close();
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
    if ui.button("Audio Source").clicked() {
        response.spawn = Some(SpawnKind::AudioSource);
        ui.close();
    }
    ui.menu_button("Collider", |ui| {
        if ui.button("Box Collider").clicked() {
            response.spawn = Some(SpawnKind::BoxCollider);
            ui.close();
        }
        if ui.button("Sphere Collider").clicked() {
            response.spawn = Some(SpawnKind::SphereCollider);
            ui.close();
        }
    });
}

#[derive(Default)]
pub struct ScenePanel {
    filter: String,
    collapsed: HashSet<usize>,
    root_collapsed: bool,
    /// Inline rename in progress: (object index, edit buffer, focus-on-first-frame).
    renaming: Option<(usize, String, bool)>,
    /// Full multi-selection (object indices). Synced from the engine's canonical
    /// selection each frame via [`set_multi`], mutated here on ctrl/shift clicks,
    /// and read back after `ui` when `selection_changed`. The `selected` anchor
    /// (last-clicked) is passed in/out separately and is always also in `multi`.
    multi: HashSet<usize>,
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
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.text_edit_singleline(&mut self.filter);
        });
        ui.separator();
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

        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Background click sensor over the whole tree, registered BEFORE
                // the rows so the rows (drawn after) take hit priority — a click
                // that lands here (empty space) clears the selection. More
                // reliable than a trailing rect, which can be mis-sized.
                let deselect_bg =
                    ui.interact(ui.max_rect(), ui.id().with("tree-deselect-bg"), Sense::click());
                // Root "Scene" node: everything lives under it; dropping a
                // row here unparents it, clicking toggles the whole tree.
                let root = self.row_widget(
                    ui,
                    "",
                    "Scene",
                    0,
                    false,
                    !roots.is_empty(),
                    self.root_collapsed,
                    true,
                    true,
                    &[],
                );
                if root.clicked() {
                    self.root_collapsed = !self.root_collapsed;
                    // Alt: cascade the whole scene open/closed.
                    if ui.input(|i| i.modifiers.alt) {
                        let collapse = self.root_collapsed;
                        for &r in &roots {
                            self.cascade(&children, r, collapse);
                        }
                    }
                }
                if let Some(payload) = root.dnd_release_payload::<usize>() {
                    response.reparent.push((*payload, None));
                }
                root.context_menu(|ui| spawn_menu(ui, &mut response));

                if filter.is_empty() {
                    if !self.root_collapsed {
                        let mut ancestry: Vec<bool> = Vec::new();
                        let last = roots.len().saturating_sub(1);
                        for (n, &index) in roots.iter().enumerate() {
                            self.tree_row(
                                ui,
                                rows,
                                &children,
                                index,
                                1,
                                n == last,
                                &mut ancestry,
                                selected,
                                &mut response,
                            );
                        }
                    }
                } else {
                    // Filtered: flat list (no tree lines).
                    for (i, row) in rows.iter().enumerate() {
                        if row.name.to_lowercase().contains(&filter) {
                            self.object_row(
                                ui, rows, &children, false, i, 1, true, &[], selected,
                                &mut response,
                            );
                        }
                    }
                }
                if rows.is_empty() {
                    ui.label(RichText::new("    Scene is empty").weak());
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
                        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fbx" | "gltf" | "glb" | "obj"))
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
        response
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
        index: usize,
        depth: usize,
        is_last: bool,
        ancestry: &mut Vec<bool>,
        selected: &mut Option<usize>,
        response: &mut ScenePanelResponse,
    ) {
        if depth > 64 {
            return;
        }
        let has_children = !children[index].is_empty();
        self.object_row(
            ui, rows, children, has_children, index, depth, is_last, ancestry, selected,
            response,
        );
        if has_children && !self.collapsed.contains(&index) {
            // This node's column continues for its children iff it has a
            // following sibling.
            ancestry.push(!is_last);
            let last = children[index].len() - 1;
            for (n, &child) in children[index].iter().enumerate() {
                self.tree_row(
                    ui,
                    rows,
                    children,
                    child,
                    depth + 1,
                    n == last,
                    ancestry,
                    selected,
                    response,
                );
            }
            ancestry.pop();
        }
    }

    /// One full-width row: tree connector lines + hover highlight + collapse
    /// arrow + icon + name, left-aligned at its depth. Returns the response.
    #[allow(clippy::too_many_arguments)]
    fn row_widget(
        &mut self,
        ui: &mut Ui,
        icon: &str,
        name: &str,
        depth: usize,
        is_selected: bool,
        has_children: bool,
        collapsed: bool,
        enabled: bool,
        is_last: bool,
        ancestry: &[bool],
    ) -> egui::Response {
        let (rect, row) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 20.0),
            Sense::click_and_drag(),
        );
        let indent = 4.0 + depth as f32 * 14.0;
        if is_selected {
            ui.painter()
                .rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
        } else if row.hovered() {
            ui.painter()
                .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        // Tree connector lines: faint verticals down each ancestor column that
        // still has rows below, plus an ├/└ connector into this row.
        if depth >= 1 {
            let stroke = egui::Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.5));
            let gx = |d: usize| rect.left() + 4.0 + d as f32 * 14.0 + 6.0;
            let cy = rect.center().y;
            for (level, &cont) in ancestry.iter().enumerate() {
                if cont {
                    ui.painter()
                        .vline(gx(level), rect.y_range(), stroke);
                }
            }
            let px = gx(depth - 1);
            ui.painter()
                .line_segment([egui::pos2(px, rect.top()), egui::pos2(px, cy)], stroke);
            ui.painter()
                .line_segment([egui::pos2(px, cy), egui::pos2(gx(depth) - 2.0, cy)], stroke);
            if !is_last {
                ui.painter()
                    .line_segment([egui::pos2(px, cy), egui::pos2(px, rect.bottom())], stroke);
            }
        }
        if has_children {
            ui.painter().text(
                egui::pos2(rect.left() + indent + 6.0, rect.center().y),
                Align2::CENTER_CENTER,
                if collapsed { "⏵" } else { "⏷" },
                FontId::proportional(10.0),
                ui.visuals().weak_text_color(),
            );
        }
        let color = if is_selected {
            ui.visuals().selection.stroke.color
        } else if enabled {
            // Enabled objects read as plain white; disabled ones dim to gray.
            egui::Color32::from_gray(235)
        } else {
            egui::Color32::from_gray(120)
        };
        let label = if icon.is_empty() {
            name.to_owned()
        } else {
            format!("{icon} {name}")
        };
        ui.painter().text(
            egui::pos2(rect.left() + indent + 14.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            color,
        );
        row
    }

    #[allow(clippy::too_many_arguments)]
    fn object_row(
        &mut self,
        ui: &mut Ui,
        rows: &[SceneObjectRow],
        children: &[Vec<usize>],
        has_children: bool,
        index: usize,
        depth: usize,
        is_last: bool,
        ancestry: &[bool],
        selected: &mut Option<usize>,
        response: &mut ScenePanelResponse,
    ) {
        let row_data = &rows[index];
        // Highlight the anchor AND every multi-selected row.
        let is_selected = *selected == Some(index) || self.multi.contains(&index);
        let collapsed = self.collapsed.contains(&index);

        // Inline rename editor in place of the normal row.
        if matches!(&self.renaming, Some((i, _, _)) if *i == index) {
            let indent = 4.0 + depth as f32 * 14.0;
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

        let row = self.row_widget(
            ui,
            row_data.icon,
            &row_data.name,
            depth,
            is_selected,
            has_children,
            collapsed,
            row_data.enabled,
            is_last,
            ancestry,
        );
        let indent = 4.0 + depth as f32 * 14.0;

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
                painter.rect_filled(
                    rect,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(60, 46, 110, 235),
                );
                painter.rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(150, 120, 220)),
                    egui::StrokeKind::Inside,
                );
                painter.galley(origin + pad, galley, egui::Color32::WHITE);
            }
        }
        if row.clicked() {
            let on_arrow = has_children
                && row
                    .interact_pointer_pos()
                    .is_some_and(|p| p.x < row.rect.left() + indent + 12.0);
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
            let color = ui.visuals().selection.stroke.color;
            let painter = ui.painter();
            match cursor_y.map_or(DropZone::Onto, |y| drop_zone(row.rect, y)) {
                DropZone::Onto => {
                    painter.rect_stroke(
                        row.rect,
                        3.0,
                        egui::Stroke::new(1.5, color),
                        egui::StrokeKind::Outside,
                    );
                }
                DropZone::Before => {
                    painter.hline(
                        row.rect.left()..=row.rect.right(),
                        row.rect.top(),
                        egui::Stroke::new(2.5, color),
                    );
                }
                DropZone::After => {
                    painter.hline(
                        row.rect.left()..=row.rect.right(),
                        row.rect.bottom(),
                        egui::Stroke::new(2.5, color),
                    );
                }
            }
        }
        row.context_menu(|ui| {
            if rows[index].parent.is_some() && ui.button("Unparent").clicked() {
                response.reparent.push((index, None));
                ui.close();
            }
            if ui
                .button("🗑 Delete")
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
