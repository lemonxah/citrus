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
    Light(crate::LightKind),
    LightProbeVolume,
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
        for kind in crate::LightKind::ALL {
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
    });
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
}

#[derive(Default)]
pub struct ScenePanel {
    filter: String,
    collapsed: HashSet<usize>,
    root_collapsed: bool,
}

impl ScenePanel {
    pub fn new() -> Self {
        Self::default()
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
                );
                if root.clicked() {
                    self.root_collapsed = !self.root_collapsed;
                }
                if let Some(payload) = root.dnd_release_payload::<usize>() {
                    response.reparent.push((*payload, None));
                }
                root.context_menu(|ui| spawn_menu(ui, &mut response));

                if filter.is_empty() {
                    if !self.root_collapsed {
                        for &index in &roots {
                            self.tree_row(ui, rows, &children, index, 1, selected, &mut response);
                        }
                    }
                } else {
                    // Filtered: flat list.
                    for (i, row) in rows.iter().enumerate() {
                        if row.name.to_lowercase().contains(&filter) {
                            self.object_row(ui, rows, false, i, 1, selected, &mut response);
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
                bg.context_menu(|ui| spawn_menu(ui, &mut response));
            });
        response
    }

    #[allow(clippy::too_many_arguments)]
    fn tree_row(
        &mut self,
        ui: &mut Ui,
        rows: &[SceneObjectRow],
        children: &[Vec<usize>],
        index: usize,
        depth: usize,
        selected: &mut Option<usize>,
        response: &mut ScenePanelResponse,
    ) {
        if depth > 64 {
            return;
        }
        let has_children = !children[index].is_empty();
        self.object_row(ui, rows, has_children, index, depth, selected, response);
        if has_children && !self.collapsed.contains(&index) {
            for &child in &children[index] {
                self.tree_row(ui, rows, children, child, depth + 1, selected, response);
            }
        }
    }

    /// One full-width row: hover highlight + collapse arrow + icon + name,
    /// left-aligned at its depth. Returns the row's response.
    #[allow(clippy::too_many_arguments)]
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
        has_children: bool,
        index: usize,
        depth: usize,
        selected: &mut Option<usize>,
        response: &mut ScenePanelResponse,
    ) {
        let row_data = &rows[index];
        let is_selected = *selected == Some(index);
        let collapsed = self.collapsed.contains(&index);
        let row = self.row_widget(
            ui,
            row_data.icon,
            &row_data.name,
            depth,
            is_selected,
            has_children,
            collapsed,
            row_data.enabled,
        );
        let indent = 4.0 + depth as f32 * 14.0;

        row.dnd_set_drag_payload(index);
        if row.clicked() {
            let on_arrow = has_children
                && row
                    .interact_pointer_pos()
                    .is_some_and(|p| p.x < row.rect.left() + indent + 12.0);
            if on_arrow {
                if !self.collapsed.remove(&index) {
                    self.collapsed.insert(index);
                }
            } else {
                *selected = if is_selected { None } else { Some(index) };
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
