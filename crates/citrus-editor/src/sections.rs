//! Reusable material editor: shader picker + collapsible feature sections.
//! Headers toggle on title click as well as the arrow; sections with a
//! master enable checkbox follow the Poiyomi pattern (off = feature compiled
//! out of the shader variant).

use egui::collapsing_header::{CollapsingState, paint_default_icon};
use egui::{ComboBox, DragValue, Label, RichText, Sense, Slider, Ui};

use crate::inspector::{AlphaModeModel, MaterialModel};

/// Section metadata for search: (title, keywords).
const SECTIONS: [(&str, &[&str]); 5] = [
    ("Base", &["color", "albedo", "metallic", "roughness", "occlusion", "ao"]),
    ("Toon Shading", &["toon", "steps", "bands", "blend", "ramp", "cel"]),
    ("Emission", &["emission", "glow", "emissive", "intensity"]),
    ("Transparency", &["alpha", "cutout", "blend", "opacity", "cutoff"]),
    ("Geometry", &["normal", "double", "sided", "culling", "bump"]),
];

fn section_matches(search: &str, index: usize) -> bool {
    if search.is_empty() {
        return true;
    }
    let (title, keywords) = SECTIONS[index];
    title.to_lowercase().contains(search) || keywords.iter().any(|k| k.contains(search))
}

/// Full material editor. Returns true if anything changed.
pub fn material_editor_ui(
    ui: &mut Ui,
    search: &mut String,
    m: &mut MaterialModel,
    shaders: &[&str],
) -> bool {
    let mut changed = false;

    // Shader picker. Only registered shaders are selectable; an unknown
    // shader (e.g. from a hand-edited .material) shows as an error entry.
    ui.horizontal(|ui| {
        ui.label("Shader");
        let known = shaders.contains(&m.shader.as_str());
        let display = if known {
            m.shader.clone()
        } else {
            format!("⚠ {}", m.shader)
        };
        ComboBox::from_id_salt("shader-select")
            .selected_text(display)
            .show_ui(ui, |ui| {
                for &shader in shaders {
                    if ui
                        .selectable_value(&mut m.shader, shader.to_owned(), shader)
                        .changed()
                    {
                        changed = true;
                    }
                }
            });
        if !known {
            ui.label(RichText::new("unknown shader").color(ui.visuals().error_fg_color));
        }
    });

    ui.horizontal(|ui| {
        ui.label("🔍");
        ui.text_edit_singleline(search);
    });
    ui.separator();

    let search_lower = search.to_lowercase();

    if section_matches(&search_lower, 0) {
        section(ui, "Base", None, &mut changed, |ui, changed| {
            property_row(ui, "Base Color", changed, |ui| {
                ui.color_edit_button_rgba_unmultiplied(&mut m.base_color)
            });
            property_row(ui, "Metallic", changed, |ui| {
                ui.add(Slider::new(&mut m.metallic, 0.0..=1.0))
            });
            property_row(ui, "Roughness", changed, |ui| {
                ui.add(Slider::new(&mut m.roughness, 0.0..=1.0))
            });
            property_row(ui, "Occlusion", changed, |ui| {
                ui.add(Slider::new(&mut m.occlusion_strength, 0.0..=1.0))
            });
        });
    }

    if section_matches(&search_lower, 1) {
        section(
            ui,
            "Toon Shading",
            Some(&mut m.toon_enabled),
            &mut changed,
            |ui, changed| {
                property_row(ui, "Steps", changed, |ui| {
                    ui.add(Slider::new(&mut m.toon_steps, 2.0..=8.0).step_by(1.0))
                });
                property_row(ui, "PBR → Toon", changed, |ui| {
                    ui.add(Slider::new(&mut m.pbr_toon_blend, 0.0..=1.0))
                });
            },
        );
    }

    if section_matches(&search_lower, 2) {
        section(
            ui,
            "Emission",
            Some(&mut m.emission_enabled),
            &mut changed,
            |ui, changed| {
                property_row(ui, "Color", changed, |ui| {
                    ui.color_edit_button_rgb(&mut m.emission_color)
                });
                property_row(ui, "Intensity", changed, |ui| {
                    ui.add(
                        DragValue::new(&mut m.emission_intensity)
                            .speed(0.05)
                            .range(0.0..=100.0),
                    )
                });
            },
        );
    }

    if section_matches(&search_lower, 3) {
        section(ui, "Transparency", None, &mut changed, |ui, changed| {
            let before = m.alpha_mode;
            ui.horizontal(|ui| {
                ui.label("Mode");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ComboBox::from_id_salt("alpha-mode")
                        .selected_text(m.alpha_mode.label())
                        .show_ui(ui, |ui| {
                            for mode in [
                                AlphaModeModel::Opaque,
                                AlphaModeModel::Cutout,
                                AlphaModeModel::Blend,
                            ] {
                                ui.selectable_value(&mut m.alpha_mode, mode, mode.label());
                            }
                        });
                });
            });
            if m.alpha_mode != before {
                *changed = true;
            }
            if m.alpha_mode == AlphaModeModel::Cutout {
                property_row(ui, "Cutoff", changed, |ui| {
                    ui.add(Slider::new(&mut m.alpha_cutoff, 0.0..=1.0))
                });
            }
            if m.alpha_mode == AlphaModeModel::Blend {
                property_row(ui, "Opacity", changed, |ui| {
                    ui.add(Slider::new(&mut m.base_color[3], 0.0..=1.0))
                });
            }
        });
    }

    if section_matches(&search_lower, 4) {
        section(ui, "Geometry", None, &mut changed, |ui, changed| {
            property_row(ui, "Double Sided", changed, |ui| {
                ui.checkbox(&mut m.double_sided, "")
            });
            ui.add_enabled_ui(m.has_normal_texture, |ui| {
                property_row(ui, "Normal Map", changed, |ui| {
                    ui.checkbox(&mut m.normal_map_enabled, "")
                });
                property_row(ui, "Normal Strength", changed, |ui| {
                    ui.add(Slider::new(&mut m.normal_strength, 0.0..=2.0))
                });
            });
            if !m.has_normal_texture {
                ui.label(RichText::new("No normal texture assigned").small().weak());
            }
        });
    }

    changed
}

/// Collapsible section. The whole header (arrow *and* title) toggles
/// open/closed; `enabled` adds a master feature checkbox to the header.
fn section(
    ui: &mut Ui,
    title: &str,
    enabled: Option<&mut bool>,
    changed: &mut bool,
    body: impl FnOnce(&mut Ui, &mut bool),
) {
    let id = ui.make_persistent_id(("citrus-section", title));
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, true);
    let mut active = true;
    let header = ui.horizontal(|ui| {
        state.show_toggle_button(ui, paint_default_icon);
        if let Some(enabled) = enabled {
            if ui.checkbox(enabled, "").changed() {
                *changed = true;
            }
            active = *enabled;
        }
        let label = ui.add(Label::new(RichText::new(title).strong()).sense(Sense::click()));
        if label.clicked() {
            state.toggle(ui);
        }
        if label.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    });
    state.show_body_indented(&header.response, ui, |ui| {
        ui.add_enabled_ui(active, |ui| body(ui, changed));
    });
    state.store(ui.ctx());
}

fn property_row(
    ui: &mut Ui,
    label: &str,
    changed: &mut bool,
    widget: impl FnOnce(&mut Ui) -> egui::Response,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widget(ui).changed() {
                *changed = true;
            }
        });
    });
}
