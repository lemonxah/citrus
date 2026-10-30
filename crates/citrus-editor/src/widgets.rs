//! The editor widget kit (docs/design/editor-ui-guide.md): buttons, troughs
//! and segments, icon toggles, badges, the type tile, axis vector fields,
//! property rows, section headers, info notes and the dashed add button. All
//! sizes and colours come from [`theme`].

use egui::{
    Align, Color32, CornerRadius, Layout, Response, RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, WidgetText,
};

use crate::icons::{self, Icon};
use crate::theme::{self, ObjectKind};

fn radius(r: u8) -> CornerRadius {
    CornerRadius::same(r)
}

/// A button drawn by hand so its fill, text and icon follow the spec exactly.
#[allow(clippy::too_many_arguments)]
fn button_impl(
    ui: &mut Ui,
    icon: Option<Icon>,
    label: &str,
    height: f32,
    fill: Color32,
    hover_fill: Color32,
    stroke: Stroke,
    text: Color32,
    font: egui::FontId,
    r: u8,
) -> Response {
    let pad = 10.0;
    let icon_w = if icon.is_some() { 14.0 + if label.is_empty() { 0.0 } else { 6.0 } } else { 0.0 };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, text);
    let width = (pad * 2.0 + icon_w + galley.size().x).max(height);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if response.hovered() || response.has_focus() { hover_fill } else { fill };
        ui.painter().rect(rect, radius(r), fill, stroke, StrokeKind::Inside);
        let content_w = icon_w + galley.size().x;
        let mut x = rect.center().x - content_w * 0.5;
        if let Some(icon) = icon {
            let ir = egui::Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), Vec2::splat(14.0));
            icons::paint(ui.painter(), ir, icon, text);
            x += icon_w;
        }
        ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y * 0.5), galley, text);
        focus_ring(ui, &response, rect, r);
    }
    response
}

/// 2 px accent ring around a keyboard-focused control.
pub fn focus_ring(ui: &Ui, response: &Response, rect: egui::Rect, r: u8) {
    if response.has_focus() && ui.memory(|m| m.focused().is_some()) {
        ui.painter().rect_stroke(
            rect.expand(1.5),
            radius(r + 1),
            Stroke::new(2.0_f32, theme::accent()),
            StrokeKind::Outside,
        );
    }
}

/// The one main action of a panel: accent fill, dark semibold text, 32 px.
pub fn primary_button(ui: &mut Ui, icon: Option<Icon>, label: &str) -> Response {
    primary_button_sized(ui, icon, label, theme::H_PRIMARY)
}

pub fn primary_button_sized(ui: &mut Ui, icon: Option<Icon>, label: &str, height: f32) -> Response {
    let accent = theme::accent();
    let hover = accent.lerp_to_gamma(theme::ACCENT_HOVER, 0.35);
    button_impl(ui, icon, label, height, accent, hover, Stroke::NONE, theme::TEXT_ON_ACCENT, theme::semibold(theme::SIZE_BODY), theme::R_TOOLBAR)
}

/// Every other button: bg.button with a line.button border, 26 px.
pub fn secondary_button(ui: &mut Ui, icon: Option<Icon>, label: &str) -> Response {
    secondary_button_sized(ui, icon, label, theme::H_FIELD)
}

pub fn secondary_button_sized(ui: &mut Ui, icon: Option<Icon>, label: &str, height: f32) -> Response {
    button_impl(
        ui,
        icon,
        label,
        height,
        theme::BG_BUTTON,
        theme::BG_SEGMENT_ACTIVE,
        Stroke::new(1.0_f32, theme::LINE_BUTTON),
        theme::TEXT_SECONDARY,
        egui::FontId::proportional(theme::SIZE_BODY),
        theme::R_ROW,
    )
}

/// Transparent text button (menu bar items).
pub fn ghost_button(ui: &mut Ui, label: &str, height: f32) -> Response {
    button_impl(
        ui,
        None,
        label,
        height,
        Color32::TRANSPARENT,
        theme::BG_BUTTON,
        Stroke::NONE,
        theme::TEXT_SECONDARY,
        egui::FontId::proportional(theme::SIZE_BODY),
        theme::R_FIELD,
    )
}

/// Icon-only toggle / button. `active` shows bg.segment.active and strong
/// icon colour. Always pass a tooltip (it doubles as the accessible name).
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str, active: bool) -> Response {
    icon_button_sized(ui, icon, tooltip, active, Vec2::splat(theme::H_TOGGLE))
}

pub fn icon_button_sized(ui: &mut Ui, icon: Icon, tooltip: &str, active: bool, size: Vec2) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if active {
            theme::BG_SEGMENT_ACTIVE
        } else if response.hovered() {
            theme::BG_BUTTON
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, radius(theme::R_FIELD), fill);
        let color = if active || response.hovered() { theme::TEXT_STRONG } else { theme::TEXT_MUTED };
        let s = (size.y * 0.6).clamp(12.0, 16.0);
        icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), Vec2::splat(s)), icon, color);
        focus_ring(ui, &response, rect, theme::R_FIELD);
    }
    response.on_hover_text(tooltip)
}

/// A tool button that turns accent when active (viewport tools, Play).
pub fn accent_icon_button(ui: &mut Ui, icon: Icon, tooltip: &str, active: bool, size: Vec2) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if active {
            theme::accent()
        } else if response.hovered() {
            theme::BG_BUTTON
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, radius(theme::R_ROW), fill);
        let color = if active {
            theme::TEXT_ON_ACCENT
        } else if response.hovered() {
            theme::TEXT_STRONG
        } else {
            theme::TEXT_SECONDARY
        };
        icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), Vec2::splat(15.0)), icon, color);
        focus_ring(ui, &response, rect, theme::R_ROW);
    }
    response.on_hover_text(tooltip)
}

/// A trough (bg.field, line.field border, 8 px radius, 2 px padding) holding
/// segments or tool buttons in a row.
pub fn trough<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(theme::BG_FIELD)
        .stroke(Stroke::new(1.0_f32, theme::LINE_FIELD))
        .corner_radius(radius(theme::R_TOOLBAR))
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(add).inner
        })
}

/// One segment inside a trough.
pub fn segment(ui: &mut Ui, selected: bool, label: &str) -> Response {
    let font = egui::FontId::proportional(theme::SIZE_BODY);
    let color = if selected { theme::TEXT_STRONG } else { theme::TEXT_MUTED };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, color);
    let size = Vec2::new(galley.size().x + 18.0, theme::H_BUTTON_SMALL);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if selected {
            theme::BG_SEGMENT_ACTIVE
        } else if response.hovered() {
            theme::BG_BUTTON
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, radius(theme::R_ROW), fill);
        let text = if response.hovered() && !selected { theme::TEXT_SECONDARY } else { color };
        ui.painter().galley_with_override_text_color(rect.center() - galley.size() * 0.5, galley, text);
        focus_ring(ui, &response, rect, theme::R_ROW);
    }
    response
}

/// Segmented control over `options`. The response is marked changed when the
/// value changes.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> Response {
    let mut changed = false;
    let mut inner = trough(ui, |ui| {
        for (v, label) in options {
            if segment(ui, *value == *v, label).clicked() && *value != *v {
                *value = *v;
                changed = true;
            }
        }
    })
    .response;
    if changed {
        inner.mark_changed();
    }
    inner
}

/// Mono count pill (`#24262B`), e.g. the hierarchy object count.
pub fn badge(ui: &mut Ui, text: &str) -> Response {
    pill(ui, text, theme::LINE_PANEL, theme::TEXT_MUTED, theme::mono(theme::SIZE_TINY))
}

/// Warning-coloured count badge (Log tab).
pub fn warn_badge(ui: &mut Ui, text: &str) -> Response {
    pill(ui, text, theme::STATUS_WARN_BG, theme::STATUS_WARN_FG, theme::mono(theme::SIZE_TINY))
}

fn pill(ui: &mut Ui, text: &str, fill: Color32, color: Color32, font: egui::FontId) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let size = Vec2::new(galley.size().x + 10.0, 16.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, radius(theme::R_FIELD), fill);
    ui.painter().galley(rect.center() - galley.size() * 0.5, galley, color);
    response
}

/// Accent chip with a dot ("Playing — edits revert when you stop").
pub fn chip(ui: &mut Ui, text: &str) -> Response {
    let accent = theme::accent();
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(theme::SIZE_META), accent);
    let size = Vec2::new(galley.size().x + 26.0, theme::H_BUTTON_SMALL);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, radius(theme::R_ROW), theme::accent_faint());
    ui.painter().circle_filled(egui::pos2(rect.left() + 10.0, rect.center().y), 3.0, accent);
    ui.painter().galley(egui::pos2(rect.left() + 18.0, rect.center().y - galley.size().y * 0.5), galley, accent);
    response
}

/// Type icon on a 16% tile of the type colour (inspector header).
pub fn type_tile(ui: &mut Ui, kind: ObjectKind, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let c = kind.color();
    ui.painter().rect_filled(rect, radius(theme::R_TOOLBAR), theme::with_alpha(c, 0.16));
    icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), Vec2::splat(size * 0.5)), kind.icon(), c);
    response
}

/// One axis field: 26 px, bg.field, a 16 px axis tile (drag it to scrub),
/// then the mono value (type to edit).
pub fn axis_field(ui: &mut Ui, axis: usize, value: &mut f32, decimals: usize, speed: f64, width: f32) -> Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, theme::H_FIELD), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, radius(theme::R_FIELD), theme::BG_FIELD);
    let tile = egui::Rect::from_min_size(rect.min, Vec2::new(16.0, rect.height()));
    let c = theme::axis(axis);
    painter.rect_filled(tile, CornerRadius { nw: theme::R_FIELD, sw: theme::R_FIELD, ne: 0, se: 0 }, theme::with_alpha(c, 0.12));
    painter.text(tile.center(), egui::Align2::CENTER_CENTER, ["X", "Y", "Z"][axis.min(2)], theme::mono_medium(theme::SIZE_TINY), c);

    let id = ui.id().with(("citrus-axis-scrub", axis, rect.min.x as i32, rect.min.y as i32));
    let scrub = ui.interact(tile, id, Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    let mut changed = false;
    if scrub.dragged() {
        let d = scrub.drag_delta().x as f64 * speed;
        if d != 0.0 {
            *value = (*value as f64 + d) as f32;
            changed = true;
        }
    }
    let field = egui::Rect::from_min_max(egui::pos2(tile.right() + 2.0, rect.top()), rect.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field).layout(Layout::left_to_right(Align::Center)));
    {
        let w = &mut child.visuals_mut().widgets;
        for s in [&mut w.inactive, &mut w.hovered, &mut w.active] {
            s.weak_bg_fill = Color32::TRANSPARENT;
            s.bg_fill = Color32::TRANSPARENT;
            s.bg_stroke = Stroke::NONE;
        }
        child.style_mut().override_font_id = Some(theme::mono(theme::SIZE_META));
    }
    let mut resp = child.add_sized(
        field.size(),
        egui::DragValue::new(value).speed(speed).max_decimals(decimals).min_decimals(decimals),
    );
    if changed {
        resp.mark_changed();
    }
    resp
}

/// X / Y / Z fields in equal columns. True when any changed.
pub fn vec3_field(ui: &mut Ui, values: &mut [f32; 3], decimals: usize, speed: f64) -> bool {
    vec3_field_mixed(ui, values, decimals, speed, [false; 3]).iter().any(|c| *c)
}

/// [`vec3_field`] with per-axis mixed flags (shown as an em dash); returns
/// which axes changed.
pub fn vec3_field_mixed(ui: &mut Ui, values: &mut [f32; 3], decimals: usize, speed: f64, mixed: [bool; 3]) -> [bool; 3] {
    let w = ((ui.available_width() - 8.0) / 3.0).max(40.0);
    let mut changed = [false; 3];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (a, v) in values.iter_mut().enumerate() {
            let resp = axis_field(ui, a, v, decimals, speed, w);
            changed[a] = resp.changed();
            if mixed[a] && !resp.has_focus() {
                mixed_overlay(ui, resp.rect);
            }
        }
    });
    changed
}

/// Two-column row: muted label (104 px) then the control filling the rest.
pub fn property_row<R>(ui: &mut Ui, label: impl Into<WidgetText>, add: impl FnOnce(&mut Ui) -> R) -> R {
    property_row_w(ui, label, theme::LABEL_WIDTH, add)
}

pub fn property_row_w<R>(ui: &mut Ui, label: impl Into<WidgetText>, label_width: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.set_min_height(theme::H_FIELD);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(label_width, theme::H_FIELD), Sense::hover());
        let text: WidgetText = label.into();
        let mixed = is_mixed(ui, text.text());
        let galley = text.color(theme::TEXT_MUTED).into_galley(ui, Some(egui::TextWrapMode::Truncate), label_width, egui::TextStyle::Body);
        ui.painter().galley(egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5), galley, theme::TEXT_MUTED);
        let start = ui.cursor().min.x;
        let out = add(ui);
        if mixed {
            let row = ui.min_rect();
            mixed_overlay(ui, egui::Rect::from_min_max(egui::pos2(start, row.top()), row.max));
        }
        out
    })
    .inner
}

/// egui temp-data key: normalized names of the fields that differ across a
/// multi-object selection, for the component being drawn.
pub const MIXED_FIELDS_KEY: &str = "citrus-mixed-fields";

/// A label or field name in the form mixed-field lists use: lowercase
/// letters and digits only, any "(...)" note dropped.
pub fn mixed_key(name: &str) -> String {
    let base = name.split('(').next().unwrap_or(name);
    base.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Whether the row labelled `label` shows a value that differs across the
/// selection.
pub fn is_mixed(ui: &Ui, label: &str) -> bool {
    let key = mixed_key(label);
    !key.is_empty()
        && ui.data(|d| d.get_temp::<Vec<String>>(egui::Id::new(MIXED_FIELDS_KEY))).is_some_and(|v| v.contains(&key))
}

/// Cover a value that differs across the selection with an em dash (Unity
/// style). Hidden while hovered or being typed into, so the field stays
/// usable; editing it sets that value on every selected object.
pub fn mixed_overlay(ui: &Ui, rect: egui::Rect) {
    if ui.rect_contains_pointer(rect) {
        return;
    }
    let typing = ui.ctx().wants_keyboard_input()
        && ui.input(|i| i.pointer.press_origin()).is_some_and(|p| rect.contains(p));
    if typing {
        return;
    }
    let p = ui.painter();
    p.rect_filled(rect.shrink(1.0), radius(theme::R_FIELD), theme::BG_FIELD);
    p.text(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "\u{2014}",
        theme::mono(theme::SIZE_META),
        theme::TEXT_SECONDARY,
    );
}

/// Uppercase section eyebrow (Environment tab groups).
pub fn eyebrow(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(text.to_uppercase())
            .font(theme::semibold(theme::SIZE_META))
            .color(theme::TEXT_MUTED)
            .extra_letter_spacing(theme::SIZE_META * 0.06),
    );
    ui.add_space(2.0);
}

/// bg.app note with an info icon and muted 11 px text.
pub fn info_note(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::BG_APP)
        .corner_radius(radius(theme::R_ROW))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                icons::icon(ui, Icon::Info, 13.0, theme::TEXT_DIM);
                ui.add(egui::Label::new(RichText::new(text).size(theme::SIZE_META).color(theme::TEXT_MUTED)).wrap());
            });
        });
}

/// Full-width dashed button (Add component, Add handler).
pub fn dashed_button(ui: &mut Ui, icon: Option<Icon>, label: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), theme::H_PRIMARY), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if response.hovered() {
            p.rect_filled(rect, radius(theme::R_TOOLBAR), theme::BG_APP);
        }
        let color = if response.hovered() { theme::TEXT_MUTED } else { theme::LINE_DASHED };
        let r = rect.shrink(0.5);
        let corners = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
        p.extend(Shape::dashed_line(&corners, Stroke::new(1.0_f32, color), 4.0, 3.0));
        let text_color = if response.hovered() { theme::TEXT_STRONG } else { theme::TEXT_SECONDARY };
        let galley = p.layout_no_wrap(label.to_owned(), egui::FontId::proportional(theme::SIZE_BODY), text_color);
        let w = galley.size().x + if icon.is_some() { 20.0 } else { 0.0 };
        let mut x = rect.center().x - w * 0.5;
        if let Some(icon) = icon {
            icons::paint(p, egui::Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), Vec2::splat(14.0)), icon, text_color);
            x += 20.0;
        }
        p.galley(egui::pos2(x, rect.center().y - galley.size().y * 0.5), galley, text_color);
        focus_ring(ui, &response, rect, theme::R_TOOLBAR);
    }
    response
}

/// Search field: magnifier icon, placeholder, 28 px.
pub fn search_field(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, theme::H_TOOLBAR), Sense::hover());
    ui.painter().rect(rect, radius(theme::R_ROW), theme::BG_FIELD, Stroke::new(1.0_f32, theme::LINE_FIELD), StrokeKind::Inside);
    icons::paint(
        ui.painter(),
        egui::Rect::from_center_size(egui::pos2(rect.left() + 14.0, rect.center().y), Vec2::splat(13.0)),
        Icon::Search,
        theme::TEXT_DIM,
    );
    let inner = egui::Rect::from_min_max(egui::pos2(rect.left() + 26.0, rect.top()), egui::pos2(rect.right() - 6.0, rect.bottom()));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
    child.add(
        egui::TextEdit::singleline(text)
            .frame(false)
            .hint_text(RichText::new(hint).color(theme::TEXT_DIM))
            .desired_width(inner.width())
            .vertical_align(Align::Center),
    )
}

/// A slider with the track filled and the knob in the accent colour. egui
/// paints the knob with the same style colour as the rail, so the knob is
/// recoloured after it's drawn.
pub fn slider(ui: &mut Ui, slider: egui::Slider<'_>) -> Response {
    let accent = theme::accent();
    let layer = ui.layer_id();
    let start = ui.ctx().graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0));
    let response = ui
        .scope(|ui| {
            ui.visuals_mut().selection.bg_fill = accent;
            ui.add(slider)
        })
        .inner;
    let knob = if response.hovered() || response.dragged() { theme::ACCENT_HOVER } else { accent };
    ui.ctx().graphics_mut(|g| {
        let list = g.entry(layer);
        let end = list.next_idx().0;
        let circles: Vec<usize> =
            list.all_entries().enumerate().skip(start).take(end - start).filter(|(_, c)| matches!(c.shape, egui::Shape::Circle(_))).map(|(i, _)| i).collect();
        for i in circles {
            list.mutate_shape(egui::layers::ShapeIdx(i), |c| {
                if let egui::Shape::Circle(circle) = &mut c.shape {
                    circle.fill = knob;
                    circle.stroke = Stroke::new(1.0_f32, theme::TEXT_ON_ACCENT);
                }
            });
        }
    });
    response
}

/// Collapsible component / section header (34 px): caret, optional
/// type-coloured icon, semibold title, optional enable checkbox, then any
/// trailing controls (kebab menu). Returns whether the body is open.
pub fn section_header(
    ui: &mut Ui,
    id: egui::Id,
    icon: Option<(Icon, Color32)>,
    title: &str,
    enabled: Option<&mut bool>,
    trailing: impl FnOnce(&mut Ui),
) -> (bool, bool) {
    section_header_with(ui, id, icon, title, enabled, true, trailing)
}

/// [`section_header`] starting closed unless `default_open`.
pub fn section_header_with(
    ui: &mut Ui,
    id: egui::Id,
    icon: Option<(Icon, Color32)>,
    title: &str,
    enabled: Option<&mut bool>,
    default_open: bool,
    trailing: impl FnOnce(&mut Ui),
) -> (bool, bool) {
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
    let mut toggled_enabled = false;
    let open = state.is_open();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(4.0, 0.0))).layout(Layout::left_to_right(Align::Center)));
    let caret = child.allocate_exact_size(Vec2::new(16.0, 16.0), Sense::click());
    let caret_rect = caret.0;
    let painter = child.painter().clone();
    let c = egui::Rect::from_center_size(caret_rect.center(), Vec2::splat(12.0));
    paint_caret(&painter, c, open, theme::TEXT_DIM);
    let mut click = caret.1.clicked();
    if let Some((icon, color)) = icon {
        icons::icon(&mut child, icon, 14.0, color);
    }
    let title_resp = child.add(egui::Label::new(RichText::new(title).font(theme::semibold(theme::SIZE_BODY)).color(theme::TEXT_STRONG)).sense(Sense::click()));
    click |= title_resp.clicked();
    child.with_layout(Layout::right_to_left(Align::Center), |ui| {
        trailing(ui);
        if let Some(e) = enabled {
            toggled_enabled = ui.checkbox(e, "").changed();
        }
    });
    if click {
        state.toggle(ui);
    }
    state.store(ui.ctx());
    (state.is_open(), toggled_enabled)
}

/// Caret pointing right, rotated down when open.
pub fn paint_caret(painter: &egui::Painter, rect: egui::Rect, open: bool, color: Color32) {
    let c = rect.center();
    let s = rect.width() * 0.22;
    let pts = if open {
        [c + Vec2::new(-s * 1.3, -s * 0.6), c + Vec2::new(0.0, s * 0.8), c + Vec2::new(s * 1.3, -s * 0.6)]
    } else {
        [c + Vec2::new(-s * 0.6, -s * 1.3), c + Vec2::new(s * 0.8, 0.0), c + Vec2::new(-s * 0.6, s * 1.3)]
    };
    painter.add(Shape::line(pts.to_vec(), Stroke::new(1.4_f32, color)));
}

/// 1 px line.panel divider across the available width.
pub fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, theme::LINE_PANEL);
}

/// Colour swatch + hex text, editing through egui's picker on click.
pub fn color_hex(ui: &mut Ui, rgba: &mut [f32; 4]) -> Response {
    ui.horizontal(|ui| {
        let r = ui.color_edit_button_rgba_unmultiplied(rgba);
        let c = Color32::from_rgba_unmultiplied(
            (rgba[0].clamp(0.0, 1.0) * 255.0) as u8,
            (rgba[1].clamp(0.0, 1.0) * 255.0) as u8,
            (rgba[2].clamp(0.0, 1.0) * 255.0) as u8,
            255,
        );
        ui.label(RichText::new(format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b())).font(theme::mono(theme::SIZE_META)).color(theme::TEXT_BODY));
        r
    })
    .inner
}

/// Fixed-width muted label at the start of a hand-built row (same column as
/// [`property_row`]). Returns the label response for hover text.
pub fn row_label(ui: &mut Ui, text: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(theme::LABEL_WIDTH, theme::H_FIELD), Sense::hover());
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), egui::FontId::proportional(theme::SIZE_BODY), theme::TEXT_MUTED);
    job.wrap = egui::text::TextWrapping::truncate_at_width(theme::LABEL_WIDTH - 4.0);
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5), galley, theme::TEXT_MUTED);
    response
}

/// Space, a 1 px divider, space: between groups in a panel.
pub fn section_gap(ui: &mut Ui) {
    ui.add_space(8.0);
    divider(ui);
    ui.add_space(6.0);
}
