//! New Project dialog body: template picker cards with a Desktop / VR filter,
//! then the location fields. Built on the editor theme tokens and widget kit.

use std::path::Path;

use citrus_editor::{icons, theme, widgets};
use egui::{Rect, Sense, Ui, Vec2};

use crate::templates::{ProjectTemplate, TEMPLATES, TemplateCategory};

pub(crate) struct NewProjectState<'a> {
    pub parent: &'a mut String,
    pub name: &'a mut String,
    pub template: &'a mut String,
    pub filter: &'a mut Option<TemplateCategory>,
}

const CARD_W: f32 = 200.0;
const CARD_H: f32 = 104.0;
const GAP: f32 = 8.0;
const COLUMNS: usize = 3;

/// The dialog contents. Returns true when "Create & Open" was clicked with
/// valid inputs; `cancel` is set when Cancel was clicked.
pub(crate) fn new_project_ui(ui: &mut Ui, s: NewProjectState, cancel: &mut bool) -> bool {
    let width = CARD_W * COLUMNS as f32 + GAP * (COLUMNS as f32 - 1.0);
    ui.set_width(width);

    ui.horizontal(|ui| {
        widgets::eyebrow(ui, "Template");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            widgets::segmented(
                ui,
                s.filter,
                &[(None, "All"), (Some(TemplateCategory::Desktop), "Desktop"), (Some(TemplateCategory::Vr), "VR")],
            );
        });
    });
    ui.add_space(6.0);

    let shown: Vec<&ProjectTemplate> =
        TEMPLATES.iter().filter(|t| s.filter.is_none_or(|f| t.category == f)).collect();
    for row in shown.chunks(COLUMNS) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for t in row {
                if template_card(ui, t, *s.template == t.id).clicked() {
                    *s.template = t.id.to_string();
                }
            }
        });
        ui.add_space(GAP);
    }

    if let Some(t) = crate::templates::find(s.template) {
        widgets::info_note(ui, &format!("{}: {}", t.name, t.features.join("  /  ")));
    }

    ui.add_space(theme::PAD);
    widgets::eyebrow(ui, "Location");
    let field_w = width - theme::LABEL_WIDTH - 8.0;
    widgets::property_row(ui, "Parent folder", |ui| {
        ui.add(egui::TextEdit::singleline(s.parent).desired_width(field_w))
    });
    widgets::property_row(ui, "Project name", |ui| ui.add(egui::TextEdit::singleline(s.name).desired_width(field_w)));
    let target = Path::new(s.parent.as_str()).join(s.name.as_str());
    ui.label(egui::RichText::new(target.display().to_string()).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_DIM));

    let valid = !s.parent.trim().is_empty()
        && !s.name.trim().is_empty()
        && Path::new(s.parent.as_str()).is_dir()
        && crate::templates::find(s.template).is_some();
    if !valid {
        ui.label(
            egui::RichText::new("Pick an existing parent folder, a name and a template.")
                .size(theme::SIZE_META)
                .color(theme::STATUS_WARN_FG),
        );
    } else if target.join("project.citrus").exists() {
        ui.label(
            egui::RichText::new("A project already exists there.").size(theme::SIZE_META).color(theme::STATUS_WARN_FG),
        );
    }

    ui.add_space(theme::PAD);
    let mut create = false;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.add_enabled_ui(valid, |ui| {
            if widgets::primary_button_sized(ui, Some(icons::Icon::Plus), "Create & Open", theme::H_PRIMARY).clicked() {
                create = true;
            }
        });
        if widgets::secondary_button_sized(ui, None, "Cancel", theme::H_PRIMARY).clicked() {
            *cancel = true;
        }
    });
    create && valid
}

fn template_card(ui: &mut Ui, t: &ProjectTemplate, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(CARD_W, CARD_H), Sense::click());
    let response = response.on_hover_text(t.description);
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let hovered = response.hovered();
    let (fill, stroke) = if selected {
        (theme::accent_faint(), egui::Stroke::new(1.0_f32, theme::accent()))
    } else if hovered {
        (theme::BG_BUTTON, egui::Stroke::new(1.0_f32, theme::LINE_BUTTON))
    } else {
        (theme::BG_FIELD, egui::Stroke::new(1.0_f32, theme::LINE_FIELD))
    };
    let p = ui.painter();
    p.rect(rect, theme::R_TOOLBAR, fill, stroke, egui::StrokeKind::Inside);

    let pad = 10.0;
    let tile = Rect::from_min_size(rect.min + Vec2::splat(pad), Vec2::splat(32.0));
    let icon_color = if selected { theme::accent() } else { theme::TEXT_SECONDARY };
    p.rect_filled(tile, theme::R_ROW, theme::with_alpha(icon_color, 0.14));
    icons::paint(p, Rect::from_center_size(tile.center(), Vec2::splat(18.0)), t.icon.editor_icon(), icon_color);

    let text_x = tile.right() + 10.0;
    let name = p.layout(t.name.to_string(), theme::semibold(theme::SIZE_TITLE), theme::TEXT_STRONG, rect.right() - text_x - pad);
    p.galley(egui::pos2(text_x, tile.top() - 1.0), name, theme::TEXT_STRONG);
    p.text(
        egui::pos2(text_x, tile.bottom() + 1.0),
        egui::Align2::LEFT_BOTTOM,
        t.category.label(),
        egui::FontId::proportional(theme::SIZE_META),
        if selected { theme::accent() } else { theme::TEXT_DIM },
    );

    let desc = p.layout(
        t.description.to_string(),
        egui::FontId::proportional(theme::SIZE_META),
        theme::TEXT_MUTED,
        CARD_W - pad * 2.0,
    );
    let desc_rect = Rect::from_min_max(egui::pos2(rect.left() + pad, tile.bottom() + 8.0), rect.max - Vec2::splat(pad));
    p.with_clip_rect(desc_rect).galley(desc_rect.min, desc, theme::TEXT_MUTED);
    response
}
