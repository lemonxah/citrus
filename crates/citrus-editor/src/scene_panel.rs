//! Scene hierarchy: nested object tree with click-select and
//! drag-to-reparent (drop a row onto another row to parent it; drop onto
//! empty space below the tree to unparent).

use egui::{Label, RichText, ScrollArea, Sense, Ui};

pub struct SceneObjectRow {
    pub name: String,
    pub parent: Option<usize>,
    /// Small glyph for the object kind (mesh/empty/camera).
    pub icon: &'static str,
}

#[derive(Default)]
pub struct ScenePanelResponse {
    pub selection_changed: bool,
    /// (child, new parent) reparent requests.
    pub reparent: Vec<(usize, Option<usize>)>,
}

#[derive(Default)]
pub struct ScenePanel {
    filter: String,
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
                if filter.is_empty() {
                    for &root in &roots {
                        tree_row(ui, rows, &children, root, 0, selected, &mut response);
                    }
                } else {
                    // Filtered: flat list.
                    for (i, row) in rows.iter().enumerate() {
                        if row.name.to_lowercase().contains(&filter) {
                            object_row(ui, rows, i, 0, selected, &mut response);
                        }
                    }
                }
                if rows.is_empty() {
                    ui.label(RichText::new("Scene is empty").weak());
                }
                // Remaining empty space: drop target to unparent.
                let rest = ui.available_rect_before_wrap();
                if rest.height() > 8.0 {
                    let bg = ui.allocate_rect(rest, Sense::hover());
                    if let Some(payload) = bg.dnd_release_payload::<usize>() {
                        response.reparent.push((*payload, None));
                    }
                }
            });
        response
    }
}

fn tree_row(
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
    object_row(ui, rows, index, depth, selected, response);
    for &child in &children[index] {
        tree_row(ui, rows, children, child, depth + 1, selected, response);
    }
}

fn object_row(
    ui: &mut Ui,
    rows: &[SceneObjectRow],
    index: usize,
    depth: usize,
    selected: &mut Option<usize>,
    response: &mut ScenePanelResponse,
) {
    let row = &rows[index];
    let is_selected = *selected == Some(index);
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 14.0);
        let text = format!("{} {}", row.icon, row.name);
        let text = if is_selected {
            RichText::new(text).strong()
        } else {
            RichText::new(text)
        };
        let label = ui.add(Label::new(text).sense(Sense::click_and_drag()).selectable(false));
        label.dnd_set_drag_payload(index);
        if label.clicked() {
            *selected = if is_selected { None } else { Some(index) };
            response.selection_changed = true;
        }
        if let Some(payload) = label.dnd_release_payload::<usize>() {
            if *payload != index {
                response.reparent.push((*payload, Some(index)));
            }
        }
        if label.dnd_hover_payload::<usize>().is_some_and(|p| *p != index) {
            ui.painter().rect_stroke(
                label.rect.expand(2.0),
                2.0,
                egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
                egui::StrokeKind::Outside,
            );
        }
        label.context_menu(|ui| {
            if rows[index].parent.is_some() && ui.button("Unparent").clicked() {
                response.reparent.push((index, None));
                ui.close();
            }
        });
    });
}
