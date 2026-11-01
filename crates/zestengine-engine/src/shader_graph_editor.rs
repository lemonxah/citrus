//! The Shader Graph tab: a node editor for `.shadergraph` assets. It edits
//! [`zestengine_assets::ShaderGraph`], previews the generated GLSL, and saves
//! the graph as RON so the shader library compiles and hot-reloads it.

use std::path::{Path, PathBuf};

use egui::{Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2};
use zestengine_assets::{
    GraphNode, GraphNodeKind, GraphTextureSlot, ShaderGraph, generate_glsl, SHADER_GRAPH_EXTENSION,
};
use zestengine_editor::egui_phosphor::regular as ph;
use zestengine_editor::icons::Icon;
use zestengine_editor::{theme, widgets};

use crate::anim_controller_editor::{rel_path, unique_path};

const NODE_WIDTH: f32 = 156.0;
const NODE_HEADER: f32 = 24.0;
const NODE_ROW: f32 = 18.0;
const PORT_RADIUS: f32 = 5.0;

/// The names the palette lists, in creation order.
const NODE_KINDS: &[&str] = &[
    "Value",
    "Color",
    "Texture Sample",
    "Add",
    "Subtract",
    "Multiply",
    "Divide",
    "Mix",
    "Sine",
    "Cosine",
    "Clamp",
    "One Minus",
    "Dot",
    "Normalize",
    "RGB",
    "Output",
];

pub struct ShaderGraphTab {
    pub open_request: bool,
    last_file: Option<PathBuf>,
    path: Option<String>,
    graph: ShaderGraph,
    dirty: bool,
    pan: Vec2,
    zoom: f32,
    selected: Option<usize>,
    dragging: Option<usize>,
    linking: Option<usize>,
    context: Option<(Option<usize>, Pos2)>,
    left_width: f32,
    right_width: f32,
    spawn_offset: f32,
}

impl Default for ShaderGraphTab {
    fn default() -> Self {
        Self {
            open_request: false,
            last_file: None,
            path: None,
            graph: ShaderGraph::default(),
            dirty: false,
            pan: Vec2::ZERO,
            zoom: 1.0,
            selected: None,
            dragging: None,
            linking: None,
            context: None,
            left_width: 240.0,
            right_width: 300.0,
            spawn_offset: 0.0,
        }
    }
}

fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// A fresh `.shadergraph` asset in `dir`, opened in the tab.
pub fn create_asset(dir: &Path, root: &Path, tab: &mut ShaderGraphTab) {
    let path = unique_path(dir, "New Shader Graph", SHADER_GRAPH_EXTENSION);
    match zestengine_assets::save_shader_graph(&path, &zestengine_assets::default_graph()) {
        Ok(()) => {
            tab.open(root, &rel_path(root, &path));
            tab.open_request = true;
        }
        Err(e) => tracing::error!("creating shader graph: {e:#}"),
    }
}

/// The editor should focus its tab this frame (callers read this, then the
/// dock tab is opened in `editor_app`).
pub fn take_open_request(ctx: &egui::Context, tab: &mut ShaderGraphTab, root: &Path) -> bool {
    let key = egui::Id::new("zestengine-open-shadergraph");
    if let Some(path) = ctx.data_mut(|d| d.remove_temp::<String>(key)) {
        tab.open(root, &path);
        return true;
    }
    std::mem::take(&mut tab.open_request)
}

/// Inspector body for a selected `.shadergraph` file.
pub fn file_inspector(ui: &mut egui::Ui, path: &Path, root: &Path, tab: &mut ShaderGraphTab) {
    let rel = rel_path(root, path);
    ui.label(egui::RichText::new(file_stem(&rel)).font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
    ui.label(egui::RichText::new(&rel).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_MUTED));
    ui.add_space(8.0);
    if tab.last_file.as_deref() != Some(path) {
        tab.last_file = Some(path.to_owned());
        tab.open(root, &rel);
        tab.open_request = true;
    }
    if widgets::secondary_button(ui, Some(Icon::FileDoc), "Open in Shader Graph").clicked() {
        tab.open(root, &rel);
        tab.open_request = true;
    }
    ui.add_space(6.0);
    widgets::info_note(ui, "Assign this .shadergraph to a material's Shader field to use it on objects.");
}

impl ShaderGraphTab {
    pub fn open(&mut self, root: &Path, rel: &str) {
        if self.path.as_deref() == Some(rel) {
            return;
        }
        if self.dirty {
            self.save(root);
        }
        match zestengine_assets::load_graph(root.join(rel)) {
            Ok(graph) => {
                self.graph = graph;
                self.path = Some(rel.to_owned());
                self.dirty = false;
                self.selected = None;
                self.linking = None;
                self.dragging = None;
                self.pan = Vec2::ZERO;
                self.zoom = 1.0;
            }
            Err(e) => tracing::error!("opening shader graph {rel}: {e:#}"),
        }
    }

    fn save(&mut self, root: &Path) {
        let Some(path) = &self.path else {
            return;
        };
        match zestengine_assets::save_shader_graph(root.join(path), &self.graph) {
            Ok(()) => self.dirty = false,
            Err(e) => tracing::error!("saving shader graph: {e:#}"),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, root: &Path) {
        egui::TopBottomPanel::top("zestengine-shadergraph-toolbar")
            .frame(egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::symmetric(12, 6)))
            .show_inside(ui, |ui| self.toolbar(ui, root));

        if self.path.is_none() {
            self.empty_state(ui, root);
            return;
        }

        egui::SidePanel::left("zestengine-shadergraph-left")
            .resizable(false)
            .exact_width(self.left_width)
            .frame(egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::same(12)))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("zestengine-shadergraph-left-scroll").show(ui, |ui| {
                    self.palette(ui);
                });
            });
        egui::SidePanel::right("zestengine-shadergraph-right")
            .resizable(false)
            .exact_width(self.right_width)
            .frame(egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::same(12)))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("zestengine-shadergraph-right-scroll").show(ui, |ui| {
                    self.inspector(ui);
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG_VIEWPORT))
            .show_inside(ui, |ui| self.canvas(ui));

        if self.dirty && !ui.input(|i| i.pointer.any_down()) && !ui.ctx().wants_keyboard_input() {
            self.save(root);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, root: &Path) {
        ui.horizontal(|ui| {
            ui.set_min_height(theme::H_TOOLBAR);
            ui.label(egui::RichText::new(ph::FLOW_ARROW).size(15.0).color(theme::TEXT_MUTED));
            match &self.path {
                Some(p) => {
                    ui.label(egui::RichText::new(file_stem(p)).font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG))
                        .on_hover_text(p.as_str());
                    widgets::badge(ui, if self.dirty { "Unsaved" } else { "Saved" });
                }
                None => {
                    ui.label(egui::RichText::new("No shader graph").color(theme::TEXT_MUTED));
                }
            }
            ui.add_space(8.0);
            if widgets::secondary_button(ui, Some(Icon::Plus), "New").clicked() {
                create_asset(&root.join("shaders"), root, self);
                self.open_request = false;
            }
            if self.path.is_some()
                && widgets::icon_button_sized(ui, Icon::Target, "Frame graph", false, Vec2::splat(theme::H_FIELD)).clicked()
            {
                self.frame_all();
            }
            if self.path.is_some()
                && widgets::secondary_button(ui, Some(Icon::FileDoc), "Save").clicked()
            {
                self.save(root);
            }
        });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, root: &Path) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG_VIEWPORT)).show_inside(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.3);
                ui.label(egui::RichText::new("No shader graph open").font(theme::semibold(theme::SIZE_TITLE)).color(theme::TEXT_STRONG));
                ui.label(egui::RichText::new("Select a .shadergraph in Files, or create one.").color(theme::TEXT_MUTED));
                ui.add_space(10.0);
                if widgets::primary_button(ui, Some(Icon::Plus), "Create Shader Graph").clicked() {
                    create_asset(&root.join("shaders"), root, self);
                    self.open_request = false;
                }
            });
        });
    }

    fn palette(&mut self, ui: &mut egui::Ui) {
        widgets::eyebrow(ui, "Add node");
        for name in NODE_KINDS {
            if ui.add_sized([self.left_width - 24.0, theme::H_FIELD], egui::Button::new(*name)).clicked() {
                let kind = kind_from_name(name, self.selected);
                self.spawn_node(kind);
            }
        }
        ui.add_space(8.0);
        widgets::eyebrow(ui, "Graph");
        let ok = zestengine_assets::validate(&self.graph);
        match ok {
            Ok(types) => {
                widgets::badge(ui, &format!("{} nodes", self.graph.nodes.len()));
                ui.label(egui::RichText::new(format!("Output type: {}", types.last().map(|t| t.label()).unwrap_or("?"))).color(theme::TEXT_MUTED));
            }
            Err(e) => {
                ui.label(egui::RichText::new("Graph invalid").color(theme::STATUS_ERROR).strong());
                ui.add(egui::Label::new(egui::RichText::new(e).color(theme::STATUS_ERROR).small()).wrap());
            }
        }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        widgets::eyebrow(ui, "Node");
        let Some(id) = self.selected.filter(|&i| i < self.graph.nodes.len()) else {
            ui.label(egui::RichText::new("Select a node to edit it.").color(theme::TEXT_MUTED));
            widgets::section_gap(ui);
            self.glsl_preview(ui);
            return;
        };
        let kind = self.graph.nodes[id].kind.clone();
        ui.label(egui::RichText::new(kind.title()).font(theme::semibold(theme::SIZE_BODY)).color(theme::TEXT_STRONG));
        ui.label(egui::RichText::new(format!("Node {id}")).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_MUTED));

        let mut changed = false;
        match &kind {
            GraphNodeKind::Value { value } => {
                let mut v = *value;
                if ui.add(egui::DragValue::new(&mut v).speed(0.01)).changed() {
                    changed = true;
                    if let GraphNodeKind::Value { value } = &mut self.graph.nodes[id].kind {
                        *value = v;
                    }
                }
            }
            GraphNodeKind::Color { color } => {
                let mut c = *color;
                if widgets::color_hex(ui, &mut c).changed() {
                    changed = true;
                    if let GraphNodeKind::Color { color } = &mut self.graph.nodes[id].kind {
                        *color = c;
                    }
                }
            }
            GraphNodeKind::TextureSample { slot, uv } => {
                let mut idx = *slot as usize;
                egui::ComboBox::from_id_salt(("shadergraph-tex-slot", id))
                    .selected_text(slot.label())
                    .show_ui(ui, |ui| {
                        for (i, s) in [GraphTextureSlot::Albedo, GraphTextureSlot::Normal, GraphTextureSlot::Orm, GraphTextureSlot::Emission].iter().enumerate() {
                            ui.selectable_value(&mut idx, i, s.label());
                        }
                    });
                if idx != *slot as usize {
                    changed = true;
                    if let GraphNodeKind::TextureSample { slot, .. } = &mut self.graph.nodes[id].kind {
                        *slot = [GraphTextureSlot::Albedo, GraphTextureSlot::Normal, GraphTextureSlot::Orm, GraphTextureSlot::Emission][idx];
                    }
                }
                let uv_text = uv.map(|u| format!("node {u}")).unwrap_or_else(|| "v_uv".into());
                widgets::property_row(ui, "UV", |ui| {
                    ui.label(egui::RichText::new(uv_text).color(theme::TEXT_MUTED));
                });
                if uv.is_some()
                    && ui.button("Disconnect UV").clicked()
                    && let Some(k) = self.graph.nodes[id].kind.disconnect(0)
                {
                    self.graph.nodes[id].kind = k;
                    changed = true;
                }
            }
            GraphNodeKind::Output { .. } => {
                ui.label(egui::RichText::new("Writes the final surface colour.").color(theme::TEXT_MUTED));
            }
            _ => {
                for (slot, label) in kind.input_slots().iter().enumerate() {
                    let text = kind
                        .input(slot)
                        .map(|src| format!("node {src}"))
                        .unwrap_or_else(|| "unconnected".into());
                    widgets::property_row(ui, *label, |ui| {
                        ui.label(egui::RichText::new(text).color(theme::TEXT_MUTED));
                    });
                }
            }
        }

        if ui.button("Delete Node").clicked() {
            self.delete_node(id);
            self.selected = None;
            return;
        }

        if changed {
            self.dirty = true;
        }

        widgets::section_gap(ui);
        self.glsl_preview(ui);
    }

    fn glsl_preview(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Generated GLSL")
            .default_open(false)
            .show(ui, |ui| match generate_glsl(&self.graph) {
                Ok(body) => {
                    ui.add(
                        egui::Label::new(egui::RichText::new(body).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_MUTED))
                            .wrap(),
                    );
                }
                Err(e) => {
                    ui.label(egui::RichText::new(e).color(theme::STATUS_ERROR).small());
                }
            });
    }

    // --------------------------------------------------------------- canvas

    fn to_screen(&self, rect: Rect, p: [f32; 2]) -> Pos2 {
        rect.center() + (self.pan + Vec2::from(p)) * self.zoom
    }

    fn node_size(&self, id: usize) -> Vec2 {
        let kind = &self.graph.nodes[id].kind;
        let slots = kind.input_slots().len().max(1);
        let extra = match kind {
            GraphNodeKind::Value { .. } | GraphNodeKind::Color { .. } => 26.0,
            GraphNodeKind::TextureSample { .. } => 18.0,
            _ => 0.0,
        };
        Vec2::new(NODE_WIDTH, NODE_HEADER + slots as f32 * NODE_ROW + extra)
    }

    fn node_rect(&self, rect: Rect, id: usize) -> Rect {
        Rect::from_center_size(self.to_screen(rect, self.graph.nodes[id].position), self.node_size(id) * self.zoom)
    }

    fn output_port(&self, rect: Rect, id: usize) -> Option<Pos2> {
        if matches!(self.graph.nodes[id].kind, GraphNodeKind::Output { .. }) {
            return None;
        }
        Some(Pos2::new(self.node_rect(rect, id).right(), self.node_rect(rect, id).center().y))
    }

    fn input_ports(&self, rect: Rect, id: usize) -> Vec<(usize, Pos2)> {
        let total = self.graph.nodes[id].kind.input_slots().len();
        if total == 0 {
            return Vec::new();
        }
        let r = self.node_rect(rect, id);
        let y0 = r.top() + NODE_HEADER * self.zoom;
        let span = (r.height() - NODE_HEADER * self.zoom - 6.0 * self.zoom).max(1.0);
        (0..total)
            .map(|slot| {
                let y = y0 + span * (slot as f32 + 0.5) / total as f32;
                (slot, Pos2::new(r.left(), y))
            })
            .collect()
    }

    fn node_at(&self, rect: Rect, p: Pos2) -> Option<usize> {
        (0..self.graph.nodes.len()).rev().find(|&id| self.node_rect(rect, id).contains(p))
    }

    fn output_port_at(&self, rect: Rect, p: Pos2) -> Option<usize> {
        (0..self.graph.nodes.len())
            .rev()
            .find(|&id| self.output_port(rect, id).is_some_and(|q| p.distance(q) <= PORT_RADIUS * self.zoom + 3.0))
    }

    fn input_port_at(&self, rect: Rect, p: Pos2) -> Option<(usize, usize)> {
        for id in (0..self.graph.nodes.len()).rev() {
            for (slot, q) in self.input_ports(rect, id) {
                if p.distance(q) <= PORT_RADIUS * self.zoom + 3.0 {
                    return Some((id, slot));
                }
            }
        }
        None
    }

    fn spawn_node(&mut self, kind: GraphNodeKind) -> usize {
        let x = 60.0 + self.spawn_offset;
        let y = 40.0 + self.spawn_offset;
        self.spawn_offset = (self.spawn_offset + 28.0) % 320.0;
        self.graph.nodes.push(GraphNode { position: [x, y], kind });
        let id = self.graph.nodes.len() - 1;
        self.selected = Some(id);
        self.dirty = true;
        id
    }

    fn delete_node(&mut self, id: usize) {
        self.graph.nodes.remove(id);
        let max = self.graph.nodes.len().saturating_sub(1);
        for node in &mut self.graph.nodes {
            let mut slot = 0;
            while slot < node.kind.input_slots().len() {
                let Some(src) = node.kind.input(slot) else {
                    slot += 1;
                    continue;
                };
                if src == id {
                    if let Some(k) = node.kind.disconnect(slot) {
                        node.kind = k;
                        slot += 1;
                        continue;
                    }
                    if let Some(k) = node.kind.with_input(slot, 0.min(max)) {
                        node.kind = k;
                    }
                } else if src > id
                    && let Some(k) = node.kind.with_input(slot, (src - 1).min(max))
                {
                    node.kind = k;
                }
                slot += 1;
            }
        }
        self.dirty = true;
    }

    fn frame_all(&mut self) {
        if self.graph.nodes.is_empty() {
            return;
        }
        let mut min = Vec2::splat(f32::INFINITY);
        let mut max = Vec2::splat(f32::NEG_INFINITY);
        for node in &self.graph.nodes {
            let p = Vec2::from(node.position);
            min = min.min(p);
            max = max.max(p);
        }
        self.pan = -((min + max) * 0.5);
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let mut edited = false;

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

        for (to, node) in self.graph.nodes.iter().enumerate() {
            let targets = self.input_ports(rect, to);
            if targets.is_empty() {
                continue;
            }
            for (slot, q) in targets {
                let Some(src) = node.kind.input(slot) else { continue };
                let Some(a) = self.output_port(rect, src) else { continue };
                let color = if self.selected == Some(to) { theme::accent() } else { theme::TEXT_DIM };
                painter.line_segment([a, q], Stroke::new(1.5_f32, color));
                arrow_head(&painter, a, q, color, self.zoom);
            }
        }

        if let Some(from) = self.linking
            && let Some(a) = self.output_port(rect, from)
            && let Some(p) = pointer
        {
            painter.line_segment([a, p], Stroke::new(2.0_f32, theme::accent()));
            arrow_head(&painter, a, p, theme::accent(), self.zoom);
        }

        for id in 0..self.graph.nodes.len() {
            let r = self.node_rect(rect, id);
            if !rect.intersects(r) {
                continue;
            }
            let kind = &self.graph.nodes[id].kind;
            let selected = self.selected == Some(id);
            let hovered = hovered_node == Some(id);
            let fill = if selected { theme::accent_faint() } else { theme::BG_BUTTON };
            let stroke = if selected {
                Stroke::new(2.0_f32, theme::accent())
            } else if hovered {
                Stroke::new(1.0_f32, theme::TEXT_SECONDARY)
            } else {
                Stroke::new(1.0_f32, theme::LINE_BUTTON)
            };
            painter.rect(r, CornerRadius::same(theme::R_ROW), fill, stroke, StrokeKind::Inside);
            painter.text(
                Pos2::new(r.left() + 8.0 * self.zoom, r.top() + 8.0 * self.zoom),
                egui::Align2::LEFT_TOP,
                kind.title(),
                theme::semibold((theme::SIZE_BODY * self.zoom).clamp(8.0, 18.0)),
                theme::TEXT_STRONG,
            );
            match kind {
                GraphNodeKind::Value { value } => {
                    painter.text(
                        Pos2::new(r.left() + 8.0 * self.zoom, r.top() + (NODE_HEADER + 6.0) * self.zoom),
                        egui::Align2::LEFT_TOP,
                        format!("{value:.3}"),
                        theme::mono((theme::SIZE_META * self.zoom).clamp(7.0, 16.0)),
                        theme::TEXT_MUTED,
                    );
                }
                GraphNodeKind::Color { color } => {
                    let swatch = Rect::from_min_size(
                        Pos2::new(r.left() + 8.0 * self.zoom, r.top() + (NODE_HEADER + 4.0) * self.zoom),
                        Vec2::splat(14.0 * self.zoom),
                    );
                    let rgba = Color32::from_rgba_unmultiplied(
                        (color[0] * 255.0) as u8,
                        (color[1] * 255.0) as u8,
                        (color[2] * 255.0) as u8,
                        (color[3] * 255.0) as u8,
                    );
                    painter.rect_filled(swatch, CornerRadius::same(3), rgba);
                }
                GraphNodeKind::TextureSample { slot, .. } => {
                    painter.text(
                        Pos2::new(r.left() + 8.0 * self.zoom, r.top() + (NODE_HEADER + 6.0) * self.zoom),
                        egui::Align2::LEFT_TOP,
                        slot.label(),
                        theme::mono((theme::SIZE_META * self.zoom).clamp(7.0, 16.0)),
                        theme::TEXT_MUTED,
                    );
                }
                _ => {}
            }
            for (_, q) in self.input_ports(rect, id) {
                painter.circle_filled(q, PORT_RADIUS * self.zoom, theme::TEXT_SECONDARY);
            }
            if let Some(q) = self.output_port(rect, id) {
                painter.circle_filled(q, PORT_RADIUS * self.zoom, theme::accent());
            }
        }

        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(p) = response.interact_pointer_pos()
        {
            if let Some(src) = self.output_port_at(rect, p) {
                self.linking = Some(src);
                self.dragging = None;
            } else if let Some(id) = self.node_at(rect, p) {
                self.dragging = Some(id);
                self.selected = Some(id);
                self.linking = None;
            } else {
                self.dragging = None;
            }
        }
        if response.dragged_by(egui::PointerButton::Primary) || response.dragged_by(egui::PointerButton::Middle) {
            let d = response.drag_delta() / self.zoom;
            if let Some(id) = self.dragging
                && response.dragged_by(egui::PointerButton::Primary)
            {
                self.graph.nodes[id].position[0] += d.x;
                self.graph.nodes[id].position[1] += d.y;
                edited = true;
            } else if self.linking.is_none() {
                self.pan += d;
            }
        }
        let released = response.drag_stopped_by(egui::PointerButton::Primary);
        if released
            && let Some(from) = self.linking
        {
            let target = response.interact_pointer_pos().or(pointer).and_then(|p| self.input_port_at(rect, p));
            if let Some((to, slot)) = target
                && to != from
                && let Some(k) = self.graph.nodes[to].kind.with_input(slot, from)
            {
                self.graph.nodes[to].kind = k;
                edited = true;
            }
            self.linking = None;
        }
        if released {
            self.dragging = None;
        }
        if response.clicked_by(egui::PointerButton::Primary) {
            self.selected = hovered_node;
        }
        if response.secondary_clicked() {
            self.linking = None;
            if let Some(p) = response.interact_pointer_pos() {
                let world = (p - rect.center()) / self.zoom - self.pan;
                self.context = Some((hovered_node, Pos2::new(world.x, world.y)));
                if hovered_node.is_some() {
                    self.selected = hovered_node;
                }
            }
        }
        response.context_menu(|ui| {
            let Some((node, at)) = self.context else {
                ui.close();
                return;
            };
            if let Some(id) = node.filter(|&i| i < self.graph.nodes.len()) {
                if ui.button("Delete Node").clicked() {
                    self.delete_node(id);
                    self.selected = None;
                    ui.close();
                }
                return;
            }
            ui.menu_button("Add Node", |ui| {
                for name in NODE_KINDS {
                    if ui.button(*name).clicked() {
                        let kind = kind_from_name(name, self.selected);
                        self.spawn_node_at(kind, [at.x, at.y]);
                        ui.close();
                    }
                }
            });
        });

        if edited {
            self.dirty = true;
        }
    }

    fn spawn_node_at(&mut self, kind: GraphNodeKind, at: [f32; 2]) -> usize {
        self.graph.nodes.push(GraphNode { position: at, kind });
        let id = self.graph.nodes.len() - 1;
        self.selected = Some(id);
        self.dirty = true;
        id
    }
}

fn kind_from_name(name: &str, hint: Option<usize>) -> GraphNodeKind {
    let i = hint.unwrap_or(0);
    match name {
        "Value" => GraphNodeKind::Value { value: 0.5 },
        "Color" => GraphNodeKind::Color { color: [0.85, 0.42, 0.2, 1.0] },
        "Texture Sample" => GraphNodeKind::TextureSample { slot: GraphTextureSlot::Albedo, uv: None },
        "Add" => GraphNodeKind::Add { a: i, b: i },
        "Subtract" => GraphNodeKind::Sub { a: i, b: i },
        "Multiply" => GraphNodeKind::Mul { a: i, b: i },
        "Divide" => GraphNodeKind::Div { a: i, b: i },
        "Mix" => GraphNodeKind::Mix { a: i, b: i, t: i },
        "Sine" => GraphNodeKind::Sin { x: i },
        "Cosine" => GraphNodeKind::Cos { x: i },
        "Clamp" => GraphNodeKind::Clamp { x: i, lo: i, hi: i },
        "One Minus" => GraphNodeKind::OneMinus { x: i },
        "Dot" => GraphNodeKind::Dot { a: i, b: i },
        "Normalize" => GraphNodeKind::Normalize { x: i },
        "RGB" => GraphNodeKind::SwizzleRgb { x: i },
        "Output" => GraphNodeKind::Output { color: i },
        _ => GraphNodeKind::Value { value: 0.0 },
    }
}

fn arrow_head(painter: &egui::Painter, a: Pos2, b: Pos2, color: Color32, zoom: f32) {
    let dir = (b - a).normalized();
    let s = 7.0 * zoom.clamp(0.6, 1.6);
    let n = Vec2::new(-dir.y, dir.x) * (s * 0.35);
    let tip = b - dir * (PORT_RADIUS * zoom + 2.0);
    painter.add(Shape::convex_polygon(
        vec![tip + dir * s, tip - dir * s + n, tip - dir * s - n],
        color,
        Stroke::NONE,
    ));
}
