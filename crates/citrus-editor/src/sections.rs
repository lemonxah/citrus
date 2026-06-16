//! Reusable material editor: shader picker + collapsible feature sections.
//! Headers toggle on title click as well as the arrow; sections with a
//! master enable checkbox follow the Poiyomi pattern (off = feature compiled
//! out of the shader variant).

use std::path::{Path, PathBuf};

use egui::collapsing_header::{CollapsingState, paint_default_icon};
use egui::{ComboBox, DragValue, Frame, Label, RichText, Sense, Slider, Ui};

use citrus_core::{AlphaModeModel, MatcapBlend, MaterialModel, ShaderPropKindUi, ShaderUiInfo};

/// Section metadata for search: (title, keywords).
const SECTIONS: [(&str, &[&str]); 6] = [
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
    ("Matcaps", &["matcap", "matcaps", "sphere", "reflection"]),
];

const MATCAP_TITLES: [&str; 3] = ["Matcap 1", "Matcap 2", "Matcap 3"];

/// True for file extensions the engine can load as a texture.
fn is_image_path(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "png" | "jpg" | "jpeg" | "tga" | "bmp" | "dds" | "hdr" | "exr" | "ktx" | "ktx2"
                | "webp" | "gif"
        )
    )
}

/// One texture-slot row: assigned filename + a drag-drop target (drag an image
/// from the file browser) + a clear button. `idx` is the 0..12 slot index
/// reported on drop via `tex_dropped`. Returns true if cleared in place.
fn texture_row(
    ui: &mut Ui,
    label: &str,
    idx: usize,
    slot: &mut Option<PathBuf>,
    tex_dropped: &mut Option<(usize, PathBuf)>,
) -> bool {
    let mut cleared = false;
    let current = slot
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned());
    let resp = Frame::group(ui.style())
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if current.is_some() && ui.small_button("✖").on_hover_text("Clear").clicked() {
                        *slot = None;
                        cleared = true;
                    }
                    match &current {
                        Some(name) => ui.label(RichText::new(name).weak()),
                        None => ui.label(RichText::new("drop image").small().weak()),
                    };
                });
            });
        })
        .response;
    if let Some(hover) = resp.dnd_hover_payload::<PathBuf>()
        && is_image_path(&hover)
    {
        ui.painter().rect_stroke(
            resp.rect,
            4.0,
            egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
            egui::StrokeKind::Outside,
        );
    }
    if let Some(dropped) = resp.dnd_release_payload::<PathBuf>()
        && is_image_path(&dropped)
    {
        *tex_dropped = Some((idx, (*dropped).clone()));
    }
    cleared
}

/// Per-texture UV transform: tiling (scale) + offset, two compact rows. Returns
/// true if either changed.
fn uv_transform_row(ui: &mut Ui, tiling: &mut [f32; 2], offset: &mut [f32; 2]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Tiling").small().weak());
        changed |= ui
            .add(DragValue::new(&mut tiling[0]).speed(0.01).prefix("x "))
            .changed();
        changed |= ui
            .add(DragValue::new(&mut tiling[1]).speed(0.01).prefix("y "))
            .changed();
        ui.separator();
        ui.label(RichText::new("Offset").small().weak());
        changed |= ui
            .add(DragValue::new(&mut offset[0]).speed(0.005).prefix("x "))
            .changed();
        changed |= ui
            .add(DragValue::new(&mut offset[1]).speed(0.005).prefix("y "))
            .changed();
    });
    changed
}

fn section_matches(search: &str, index: usize) -> bool {
    if search.is_empty() {
        return true;
    }
    let (title, keywords) = SECTIONS[index];
    title.to_lowercase().contains(search) || keywords.iter().any(|k| k.contains(search))
}

/// Full material editor. Returns true if anything changed. Texture-slot drops
/// are reported via `tex_dropped` ((slot 0..12, absolute image path)); the
/// engine converts the path to project-relative and assigns it.
pub fn material_editor_ui(
    ui: &mut Ui,
    search: &mut String,
    m: &mut MaterialModel,
    shaders: &[&str],
    shader_info: Option<&ShaderUiInfo>,
    tex_dropped: &mut Option<(usize, PathBuf)>,
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
        // Render queue is the last entry for every shader (draw-order priority).
        ui.separator();
        render_queue_ui(ui, m, &mut changed);
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
            *changed |= texture_row(ui, "Albedo", 0, m.textures.slot_mut(0).unwrap(), tex_dropped);
            *changed |= uv_transform_row(ui, &mut m.albedo_tiling, &mut m.albedo_offset);
            property_row(ui, "Metallic", changed, |ui| {
                ui.add(Slider::new(&mut m.metallic, 0.0..=1.0))
            });
            property_row(ui, "Roughness", changed, |ui| {
                ui.add(Slider::new(&mut m.roughness, 0.0..=1.0))
            });
            property_row(ui, "Occlusion", changed, |ui| {
                ui.add(Slider::new(&mut m.occlusion_strength, 0.0..=1.0))
            });
            property_row(ui, "Reflection", changed, |ui| {
                ui.add(Slider::new(&mut m.reflection_intensity, 0.0..=2.0))
                    .on_hover_text(
                        "Per-material reflection strength. Scales the environment / \
                         reflection-probe cube AND screen-space/RT reflections for this \
                         material (mix & match per material). 1 = default, 0 = matte.",
                    )
            });
            property_row(ui, "Screen/RT Reflections", changed, |ui| {
                ui.checkbox(&mut m.screen_reflections, "")
                    .on_hover_text(
                        "Reflection technique mix: ON = screen-space / ray-traced \
                         reflections on top of the environment cube; OFF = environment / \
                         reflection-probe cube only (cheaper, no SSR/RT for this material).",
                    )
            });
            *changed |= texture_row(
                ui,
                "ORM (Occl/Rough/Metal)",
                2,
                m.textures.slot_mut(2).unwrap(),
                tex_dropped,
            );
            *changed |= uv_transform_row(ui, &mut m.orm_tiling, &mut m.orm_offset);
            ui.label(
                RichText::new(
                    "Or split maps below — each multiplies its packed-ORM channel \
                     (leave ORM empty to use them alone). They share the ORM tiling.",
                )
                .small()
                .weak(),
            );
            *changed |= texture_row(ui, "Ambient Occlusion", 12, m.textures.slot_mut(12).unwrap(), tex_dropped);
            let has_ao = m.textures.ao.is_some();
            ui.add_enabled_ui(has_ao, |ui| {
                *changed |= ui.checkbox(&mut m.ao_invert, "Invert").changed();
            });
            *changed |= texture_row(ui, "Roughness", 13, m.textures.slot_mut(13).unwrap(), tex_dropped);
            let has_rough = m.textures.roughness.is_some();
            ui.add_enabled_ui(has_rough, |ui| {
                *changed |= ui
                    .checkbox(&mut m.roughness_invert, "Invert (smoothness → roughness)")
                    .changed();
            });
            *changed |= texture_row(ui, "Metallic", 14, m.textures.slot_mut(14).unwrap(), tex_dropped);
            let has_metal = m.textures.metallic.is_some();
            ui.add_enabled_ui(has_metal, |ui| {
                *changed |= ui.checkbox(&mut m.metallic_invert, "Invert").changed();
            });
            ui.separator();
            *changed |=
                texture_row(ui, "Normal Map", 1, m.textures.slot_mut(1).unwrap(), tex_dropped);
            *changed |= uv_transform_row(ui, &mut m.normal_tiling, &mut m.normal_offset);
            ui.add_enabled_ui(m.has_normal_texture, |ui| {
                property_row(ui, "Use Normal Map", changed, |ui| {
                    ui.checkbox(&mut m.normal_map_enabled, "")
                });
                property_row(ui, "Normal Strength", changed, |ui| {
                    ui.add(Slider::new(&mut m.normal_strength, 0.0..=2.0))
                });
            });
            ui.separator();
            *changed |=
                texture_row(ui, "Opacity", 4, m.textures.slot_mut(4).unwrap(), tex_dropped);
            ui.separator();
            *changed |= texture_row(
                ui,
                "Displacement (height)",
                15,
                m.textures.slot_mut(15).unwrap(),
                tex_dropped,
            );
            let has_disp = m.textures.displacement.is_some();
            ui.add_enabled_ui(has_disp, |ui| {
                property_row(ui, "Displacement Scale", changed, |ui| {
                    ui.add(Slider::new(&mut m.displacement_scale, 0.0..=0.1))
                        .on_hover_text(
                            "Parallax occlusion depth (0 = off). Height samples in the \
                             albedo UV tiling; all maps shift together.",
                        )
                });
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
                ui.label(
                    egui::RichText::new(
                        "Poiyomi-style cel shading (PBR base + ramp + rim). Shares GI \
                         + baked lightmaps with the Standard shader.",
                    )
                    .small()
                    .weak(),
                );
                property_row(ui, "Ramp Steps", changed, |ui| {
                    ui.add(Slider::new(&mut m.toon_steps, 2.0..=8.0).step_by(1.0))
                });
                property_row(ui, "Toon Strength", changed, |ui| {
                    ui.add(Slider::new(&mut m.pbr_toon_blend, 0.0..=1.0))
                        .on_hover_text("Blend between smooth PBR (0) and full cel ramp (1)")
                });
                property_row(ui, "Ramp Smoothness", changed, |ui| {
                    ui.add(Slider::new(&mut m.ramp_smoothness, 0.01..=0.49))
                        .on_hover_text("Softness of each cel-band terminator")
                });
                ui.separator();
                property_row(ui, "Rim Strength", changed, |ui| {
                    ui.add(Slider::new(&mut m.rim_strength, 0.0..=3.0))
                        .on_hover_text("Fresnel edge glow (0 = off)")
                });
                property_row(ui, "Rim Color", changed, |ui| {
                    ui.color_edit_button_rgb(&mut m.rim_color)
                });
                property_row(ui, "Rim Power", changed, |ui| {
                    ui.add(Slider::new(&mut m.rim_power, 0.5..=8.0))
                        .on_hover_text("Higher = tighter edge")
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
                *changed |= texture_row(
                    ui,
                    "Emission Map",
                    3,
                    m.textures.slot_mut(3).unwrap(),
                    tex_dropped,
                );
                *changed |= uv_transform_row(ui, &mut m.emission_tiling, &mut m.emission_offset);
                property_row(ui, "Intensity", changed, |ui| {
                    ui.add(
                        DragValue::new(&mut m.emission_intensity)
                            .speed(0.05)
                            .range(0.0..=100.0),
                    )
                });
                *changed |= texture_row(
                    ui,
                    "Emission Mask",
                    5,
                    m.textures.slot_mut(5).unwrap(),
                    tex_dropped,
                );
                property_row(ui, "Pulse Speed", changed, |ui| {
                    ui.add(Slider::new(&mut m.emission_pulse, 0.0..=10.0))
                        .on_hover_text("Brightness oscillation rate (0 = steady)")
                });
                ui.horizontal(|ui| {
                    ui.label("UV Scroll");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        *changed |= ui
                            .add(DragValue::new(&mut m.emission_scroll[1]).speed(0.01).prefix("y "))
                            .changed();
                        *changed |= ui
                            .add(DragValue::new(&mut m.emission_scroll[0]).speed(0.01).prefix("x "))
                            .changed();
                    });
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
            ui.label(
                RichText::new("Normal map + strength live in the Base section.")
                    .small()
                    .weak(),
            );
        });
    }

    if section_matches(&search_lower, 5) {
        section(ui, "Matcaps", None, &mut changed, |ui, changed| {
            ui.label(
                RichText::new(
                    "Sphere-mapped reflection layers. Drop a matcap texture, pick how it \
                     blends with the colour, and (optionally) a mask. Strength 0 = off.",
                )
                .small()
                .weak(),
            );
            // Per-layer slot indices into the 12-slot texture set:
            // matcap tex = 6/8/10, matcap mask = 7/9/11.
            for i in 0..3 {
                let tex_slot = 6 + i * 2;
                let mask_slot = 7 + i * 2;
                section(ui, MATCAP_TITLES[i], None, changed, |ui, changed| {
                    *changed |= texture_row(
                        ui,
                        "Matcap",
                        tex_slot,
                        m.textures.slot_mut(tex_slot).unwrap(),
                        tex_dropped,
                    );
                    let before = m.matcap_blend[i];
                    ui.horizontal(|ui| {
                        ui.label("Blend");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ComboBox::from_id_salt(("matcap-blend", i))
                                .selected_text(m.matcap_blend[i].label())
                                .show_ui(ui, |ui| {
                                    for mode in MatcapBlend::ALL {
                                        ui.selectable_value(
                                            &mut m.matcap_blend[i],
                                            mode,
                                            mode.label(),
                                        );
                                    }
                                });
                        });
                    });
                    if m.matcap_blend[i] != before {
                        *changed = true;
                    }
                    property_row(ui, "Strength", changed, |ui| {
                        ui.add(Slider::new(&mut m.matcap_strength[i], 0.0..=2.0))
                    });
                    *changed |= texture_row(
                        ui,
                        "Mask",
                        mask_slot,
                        m.textures.slot_mut(mask_slot).unwrap(),
                        tex_dropped,
                    );
                });
            }
        });
    }

    // Render queue is the last entry for every shader (draw-order priority).
    ui.separator();
    render_queue_ui(ui, m, &mut changed);

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
