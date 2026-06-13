//! Reusable material editor: shader picker + collapsible feature sections.
//! Headers toggle on title click as well as the arrow; sections with a
//! master enable checkbox follow the Poiyomi pattern (off = feature compiled
//! out of the shader variant).

use egui::collapsing_header::{CollapsingState, paint_default_icon};
use egui::{ComboBox, DragValue, Label, RichText, Sense, Slider, Ui};

use crate::inspector::{AlphaModeModel, MaterialModel};

/// Reflected custom-shader property kinds (mirrors the pragma metadata
/// parsed by citrus-assets; the engine converts between the two).
#[derive(Clone, Copy, Debug)]
pub enum ShaderPropKindUi {
    Float { min: f32, max: f32 },
    Toggle,
    Color,
    Color3,
}

#[derive(Clone, Debug)]
pub struct ShaderPropUi {
    pub label: String,
    pub kind: ShaderPropKindUi,
    /// Flat float offset into `MaterialModel::custom_values`.
    pub offset: usize,
}

/// Everything the inspector needs to draw a custom shader's material UI.
#[derive(Clone, Debug, Default)]
pub struct ShaderUiInfo {
    pub display_name: String,
    pub props: Vec<ShaderPropUi>,
    /// Compile/parse error; shown instead of properties (error swirl in the
    /// viewport).
    pub error: Option<String>,
}

/// Section metadata for search: (title, keywords).
const SECTIONS: [(&str, &[&str]); 5] = [
    (
        "Base",
        &[
            "color",
            "albedo",
            "metallic",
            "roughness",
            "occlusion",
            "ao",
        ],
    ),
    (
        "Toon Shading",
        &["toon", "steps", "bands", "blend", "ramp", "cel"],
    ),
    ("Emission", &["emission", "glow", "emissive", "intensity"]),
    (
        "Transparency",
        &["alpha", "cutout", "blend", "opacity", "cutoff"],
    ),
    (
        "Geometry",
        &["normal", "double", "sided", "culling", "bump"],
    ),
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
    shader_info: Option<&ShaderUiInfo>,
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
                        // New shader = new property layout; defaults refill.
                        m.custom_values.clear();
                        changed = true;
                    }
                }
            });
        if !known {
            ui.label(RichText::new("unknown shader").color(ui.visuals().error_fg_color));
        }
    });

    if m.shader != "standard" {
        changed |= custom_shader_ui(ui, m, shader_info);
        // Pipeline state still comes from the standard feature set.
        section(ui, "Transparency", None, &mut changed, |ui, changed| {
            alpha_mode_ui(ui, m, changed);
        });
        section(ui, "Geometry", None, &mut changed, |ui, changed| {
            property_row(ui, "Double Sided", changed, |ui| {
                ui.checkbox(&mut m.double_sided, "")
            });
        });
        return changed;
    }

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
            alpha_mode_ui(ui, m, changed);
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

fn alpha_mode_ui(ui: &mut Ui, m: &mut MaterialModel, changed: &mut bool) {
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
        // Follow the new mode's default queue, unless the user had customized
        // it away from the old mode's default.
        if m.render_queue == before.default_render_queue() {
            m.render_queue = m.alpha_mode.default_render_queue();
        }
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
    render_queue_ui(ui, m, changed);
}

/// Render-queue (draw-order priority) control: a value plus Unity-style
/// preset buttons. Higher = drawn later; transparent (≥3000) sorts
/// back-to-front. The gaps let you fine-tune layered transparency ordering.
fn render_queue_ui(ui: &mut Ui, m: &mut MaterialModel, changed: &mut bool) {
    property_row(ui, "Render Queue", changed, |ui| {
        ui.add(
            DragValue::new(&mut m.render_queue)
                .range(0..=5000)
                .speed(1.0),
        )
    });
    ui.horizontal(|ui| {
        for (label, q) in [
            ("Geometry", 2000),
            ("AlphaTest", 2450),
            ("Transparent", 3000),
            ("Overlay", 4000),
        ] {
            if ui.selectable_label(m.render_queue == q, label).clicked() {
                m.render_queue = q;
                *changed = true;
            }
        }
    });
}

/// Reflected property editor for a custom shader. Returns true on change.
fn custom_shader_ui(ui: &mut Ui, m: &mut MaterialModel, info: Option<&ShaderUiInfo>) -> bool {
    let mut changed = false;
    ui.separator();
    let Some(info) = info else {
        ui.label(RichText::new("Loading shader…").weak());
        return false;
    };
    if let Some(error) = &info.error {
        ui.label(
            RichText::new("Shader failed to compile")
                .color(ui.visuals().error_fg_color)
                .strong(),
        );
        ui.label(RichText::new(error).small().monospace());
        return false;
    }
    if m.custom_values.len() < 16 {
        // Engine fills defaults on resolve; guard against a stale frame.
        ui.label(RichText::new("Initializing properties…").weak());
        return false;
    }
    section(ui, "Properties", None, &mut changed, |ui, changed| {
        if info.props.is_empty() {
            ui.label(
                RichText::new("This shader declares no properties")
                    .small()
                    .weak(),
            );
        }
        for prop in &info.props {
            let at = prop.offset;
            match prop.kind {
                ShaderPropKindUi::Float { min, max } => {
                    property_row(ui, &prop.label, changed, |ui| {
                        ui.add(Slider::new(&mut m.custom_values[at], min..=max))
                    });
                }
                ShaderPropKindUi::Toggle => {
                    let mut on = m.custom_values[at] != 0.0;
                    property_row(ui, &prop.label, changed, |ui| {
                        let response = ui.checkbox(&mut on, "");
                        if response.changed() {
                            m.custom_values[at] = on as u32 as f32;
                        }
                        response
                    });
                }
                ShaderPropKindUi::Color => {
                    property_row(ui, &prop.label, changed, |ui| {
                        let slice: &mut [f32; 4] =
                            (&mut m.custom_values[at..at + 4]).try_into().unwrap();
                        ui.color_edit_button_rgba_unmultiplied(slice)
                    });
                }
                ShaderPropKindUi::Color3 => {
                    property_row(ui, &prop.label, changed, |ui| {
                        let slice: &mut [f32; 3] =
                            (&mut m.custom_values[at..at + 3]).try_into().unwrap();
                        ui.color_edit_button_rgb(slice)
                    });
                }
            }
        }
    });
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
