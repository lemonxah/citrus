//! Unified Inspector panel: shows the selected object (transform, mesh,
//! material slots + material editor) or the selected file (.material editor,
//! .scene loader, generic file info).

use std::path::PathBuf;

use egui::{RichText, Ui};

use zestengine_core::{Component, ComponentRegistry, MaterialModel, ObjectId, ShaderUiInfo};

use crate::components::{EditorComponents, components_ui};
use crate::sections::material_editor_ui;
use egui_phosphor::regular as ph;

#[derive(Clone, PartialEq)]
pub struct TransformModel {
    pub translation: [f32; 3],
    /// Euler XYZ in degrees.
    pub rotation_deg: [f32; 3],
    pub scale: [f32; 3],
}

/// Which header / transform values differ across a multi-object selection.
#[derive(Clone, Copy, Debug, Default)]
pub struct MixedInfo {
    pub name: bool,
    pub enabled: bool,
    pub static_geometry: bool,
    pub cast_shadows: bool,
    pub translation: [bool; 3],
    pub rotation: [bool; 3],
    pub scale: [bool; 3],
}

pub struct ObjectInfoModel {
    pub mixed: MixedInfo,
    pub name: String,
    /// Whether the object renders / contributes light.
    pub enabled: bool,
    /// Non-moving: included in the lighting bake (lightmaps + occluder).
    pub static_geometry: bool,
    /// Per-object lightmap-resolution multiplier ("Scale In Lightmap").
    pub lightmap_scale: f32,
    /// "Cast Shadows" (Unity's MeshRenderer setting).
    pub cast_shadows: zestengine_core::ShadowCasting,
    /// "Mesh" / "Empty" / "Camera" / "Primitive".
    pub kind: &'static str,
    pub transform: TransformModel,
    /// (vertices, triangles) for mesh objects.
    pub mesh: Option<(u32, u32)>,
    /// Project-relative `.material` behind the active material slot (None =
    /// the model's own material).
    pub material_file: Option<String>,
}

pub enum InspectorContent<'a> {
    Empty,
    Object {
        info: &'a mut ObjectInfoModel,
        material: Option<&'a mut MaterialModel>,
        /// Reflected info for the material's custom shader, if it uses one.
        shader_info: Option<&'a ShaderUiInfo>,
        components: &'a mut Vec<Box<dyn Component>>,
        registry: &'a ComponentRegistry,
        /// Editor-side inspector/gizmo dispatch for the components.
        editor_components: &'a EditorComponents,
        /// (id, name) of every scene object, for `ObjectRef` picker dropdowns.
        objects: &'a [(ObjectId, String)],
        /// Layer display names, so layer pickers (camera culling mask) stay
        /// consistent with the Layers window.
        layer_names: &'a [String],
        /// Inline editor for assets assigned to component slots.
        asset_editor: Option<&'a crate::components::AssetEditor<'a>>,
    },
    MaterialFile {
        path: String,
        material: &'a mut MaterialModel,
        shader_info: Option<&'a ShaderUiInfo>,
        dirty: bool,
    },
    SceneFile {
        path: String,
    },
    File {
        path: String,
        size: Option<u64>,
    },
}

/// Result of the pinned object header row.
#[derive(Default)]
pub struct ObjectHeaderResponse {
    pub object_changed: bool,
    pub lock_changed: bool,
}

#[derive(Default)]
pub struct InspectorResponse {
    pub object_changed: bool,
    pub material_changed: bool,
    pub reset_material: bool,
    pub save_material: bool,
    pub load_scene: bool,
    /// A `.material` file was dropped on the material slot.
    pub material_dropped: Option<PathBuf>,
    /// An image file was dropped on a texture slot: (slot index 0..12, absolute
    /// path). The engine converts it to project-relative and assigns it.
    pub texture_dropped: Option<(usize, PathBuf)>,
    /// Component picked from the Add Component menu.
    pub add_component: Option<&'static str>,
    /// Component index whose remove button was clicked.
    pub remove_component: Option<usize>,
    /// The code editor's text changed / its save button was clicked.
    pub code_changed: bool,
    pub save_code: bool,
    /// Run cargo clippy over the plugin crates.
    pub run_check: bool,
}

/// One cargo clippy / rustc message, simplified for display.
#[derive(Clone, Debug)]
pub struct CodeDiagnostic {
    /// "error" or "warning".
    pub level: String,
    /// Project-relative file.
    pub file: String,
    pub line: u32,
    pub message: String,
}

pub struct InspectorPanel {
    search: String,
    /// When set, the inspector keeps showing the locked selection even as the
    /// scene selection changes. The engine reads this to freeze its content.
    pub locked: bool,
}

impl Default for InspectorPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl InspectorPanel {
    pub fn new() -> Self {
        Self {
            search: String::new(),
            locked: false,
        }
    }

    /// Pinned object header (spec §8.1): type tile, borderless name with the
    /// type line under it, Active + lock on the right; then Layer and Static.
    #[allow(clippy::too_many_arguments)]
    pub fn object_header(
        &mut self,
        ui: &mut Ui,
        kind: crate::theme::ObjectKind,
        type_line: &str,
        name: &mut String,
        enabled: &mut bool,
        static_geometry: &mut bool,
        layer: &mut u8,
        layer_names: &[String],
    ) -> ObjectHeaderResponse {
        use crate::{icons::Icon, theme, widgets};
        let mut r = ObjectHeaderResponse::default();
        ui.horizontal(|ui| {
            widgets::type_tile(ui, kind, 32.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let w = (ui.available_width() - 110.0).max(60.0);
                if ui
                    .add(
                        egui::TextEdit::singleline(name)
                            .frame(false)
                            .hint_text("\u{2014}")
                            .font(theme::semibold(theme::SIZE_TITLE))
                            .text_color(theme::TEXT_STRONG)
                            .desired_width(w),
                    )
                    .changed()
                {
                    r.object_changed = true;
                }
                ui.label(RichText::new(type_line).size(theme::SIZE_META).color(theme::TEXT_MUTED));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, if self.locked { Icon::Lock } else { Icon::LockOpen }, "Lock the Inspector to the current selection", self.locked)
                    .clicked()
                {
                    self.locked = !self.locked;
                    r.lock_changed = true;
                }
                if ui
                    .checkbox(enabled, RichText::new("Active").color(theme::TEXT_SECONDARY))
                    .on_hover_text("Enable / disable (skips updates, rendering and lighting)")
                    .changed()
                {
                    r.object_changed = true;
                }
            });
        });
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("Layer").size(theme::SIZE_META).color(theme::TEXT_MUTED));
            let current = layer_names.get(*layer as usize).cloned().unwrap_or_else(|| format!("Layer {layer}"));
            egui::ComboBox::from_id_salt("zestengine-object-layer")
                .width(cols[0].available_width())
                .selected_text(current)
                .show_ui(&mut cols[0], |ui| {
                    for (l, n) in layer_names.iter().enumerate() {
                        if ui.selectable_value(layer, l as u8, n).changed() {
                            r.object_changed = true;
                        }
                    }
                });
            cols[1].label(RichText::new("Static").size(theme::SIZE_META).color(theme::TEXT_MUTED));
            if cols[1]
                .checkbox(static_geometry, "")
                .on_hover_text("Non-moving: included in the lighting bake (lightmapped + occluder)")
                .changed()
            {
                r.object_changed = true;
            }
        });
        ui.add_space(4.0);
        r
    }

    /// Draw the lock toggle header (a 🔒 button). Returns true if the lock
    /// state changed this frame (the engine snapshots the selection then).
    pub fn lock_header(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            let icon = if self.locked { ph::LOCK } else { ph::LOCK_OPEN };
            if ui
                .selectable_label(self.locked, icon)
                .on_hover_text("Lock the Inspector to the current selection")
                .clicked()
            {
                self.locked = !self.locked;
                changed = true;
            }
            if self.locked {
                ui.label(RichText::new("Locked").small().weak());
            }
        });
        changed
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        content: InspectorContent<'_>,
        shaders: &[&str],
    ) -> InspectorResponse {
        let mut response = InspectorResponse::default();
        match content {
            InspectorContent::Empty => {
                ui.label(RichText::new("Nothing selected").weak());
                ui.label(
                    RichText::new("Select an object in the viewport or Scene panel,\nor a file in the Files panel.")
                        .small()
                        .weak(),
                );
            }
            InspectorContent::Object {
                info,
                material,
                shader_info,
                components,
                registry,
                editor_components,
                objects,
                layer_names,
                asset_editor,
            } => {
                self.object_ui(
                    ui,
                    info,
                    material,
                    shader_info,
                    components,
                    registry,
                    editor_components,
                    objects,
                    layer_names,
                    asset_editor,
                    shaders,
                    &mut response,
                );
            }
            InspectorContent::MaterialFile {
                path,
                material,
                shader_info,
                dirty,
            } => {
                ui.heading(RichText::new(&material.name).strong());
                ui.label(RichText::new(path).small().weak());
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(dirty, egui::Button::new(format!("{} Save", ph::FLOPPY_DISK)))
                        .clicked()
                    {
                        response.save_material = true;
                    }
                    if dirty {
                        ui.label(RichText::new("unsaved changes").small().weak());
                    }
                });
                ui.separator();
                response.material_changed |= material_editor_ui(
                    ui,
                    &mut self.search,
                    material,
                    shaders,
                    shader_info,
                    &mut response.texture_dropped,
                );
            }
            InspectorContent::SceneFile { path } => {
                ui.heading("Scene");
                ui.label(RichText::new(&path).small().weak());
                ui.separator();
                if ui.button("Load Scene").clicked() {
                    response.load_scene = true;
                }
            }
            InspectorContent::File { path, size } => {
                let name = std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                ui.heading(name);
                ui.label(RichText::new(&path).small().weak());
                if let Some(size) = size {
                    ui.label(format!("Size: {}", human_size(size)));
                }
            }
        }
        response
    }

    #[allow(clippy::too_many_arguments)]
    fn object_ui(
        &mut self,
        ui: &mut Ui,
        info: &mut ObjectInfoModel,
        material: Option<&mut MaterialModel>,
        shader_info: Option<&ShaderUiInfo>,
        components: &mut Vec<Box<dyn Component>>,
        registry: &ComponentRegistry,
        editor_components: &EditorComponents,
        objects: &[(ObjectId, String)],
        layer_names: &[String],
        asset_editor: Option<&crate::components::AssetEditor<'_>>,
        shaders: &[&str],
        response: &mut InspectorResponse,
    ) {
        // The object name / enabled / static row is drawn as the pinned header
        // (see `object_header`), so the body starts at Transform.
        crate::widgets::divider(ui);
        let mut reset = false;
        let (open, _) = crate::widgets::section_header(ui, ui.make_persistent_id("zestengine-transform"), None, "Transform", None, |ui| {
            reset = crate::widgets::icon_button(ui, crate::icons::Icon::Reset, "Reset transform", false).clicked();
        });
        let mixed = info.mixed;
        let t = &mut info.transform;
        if reset {
            t.translation = [0.0; 3];
            t.rotation_deg = [0.0; 3];
            t.scale = [1.0; 3];
            response.object_changed = true;
        }
        if open {
            egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 0, top: 0, bottom: 10 }).show(ui, |ui| {
                response.object_changed |= transform_row(ui, "Position", &mut t.translation, 0.02, 2, mixed.translation);
                response.object_changed |= transform_row(ui, "Rotation", &mut t.rotation_deg, 0.5, 1, mixed.rotation);
                response.object_changed |= scale_row(ui, &mut t.scale, mixed.scale);
            });
        }

        // Shown for meshes with an embedded (read-only) material too, so their
        // mesh settings stay reachable.
        if info.mesh.is_some() || material.is_some() {
            // Collapsed by default so the components below stay in reach.
            crate::widgets::divider(ui);
            let name = material.as_ref().map(|m| m.name.clone()).unwrap_or_default();
            let (open, _) = crate::widgets::section_header_with(
                ui,
                ui.make_persistent_id("zestengine-material-section"),
                None,
                "Mesh & Material",
                None,
                false,
                |ui| {
                    ui.label(RichText::new(name).size(crate::theme::SIZE_META).color(crate::theme::TEXT_DIM));
                },
            );
            if open {
                egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 0, top: 0, bottom: 10 }).show(ui, |ui| {
                    let has_mesh = info.mesh.is_some();
                    mesh_ui(ui, info, response);
                    if let Some(material) = material {
                        if has_mesh {
                            ui.separator();
                        }
                        self.material_ui(ui, info, material, shader_info, shaders, response);
                    }
                });
            }
        }

        // Components (Unity-style: list + Add Component at the bottom).
        let comp = components_ui(ui, components, registry, editor_components, objects, layer_names, asset_editor);
        response.object_changed |= comp.changed;
        response.add_component = comp.add;
        response.remove_component = comp.remove;
    }

    fn material_ui(
        &mut self,
        ui: &mut Ui,
        info: &mut ObjectInfoModel,
        material: &mut MaterialModel,
        shader_info: Option<&ShaderUiInfo>,
        shaders: &[&str],
        response: &mut InspectorResponse,
    ) {
        // The active material slot (the slot selector, when an object has more
        // than one, is drawn by the engine above the inspector body). Dropping a
        // `.material` here assigns it to the selected slot.
        // The active slot's material as an asset slot: drop, browse or make a
        // `.material`; click to show it in Files, double-click to open it.
        let mut value = info.material_file.clone().unwrap_or_default();
        let hint = format!("{} (from the model)", material.name);
        let changed = crate::widgets::property_row(ui, "Material", |ui| crate::components::asset_chip(ui, "active-material", &mut value, &hint, &["material"]));
        let root: Option<String> = ui.data(|d| d.get_temp(egui::Id::new(crate::PROJECT_ROOT_KEY)));
        if changed
            && let (Some(rel), Some(root)) = (crate::components::resolve_slot(ui, &value), root)
        {
            response.material_dropped = Some(PathBuf::from(root).join(rel));
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Material").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("Reset")
                    .on_hover_text("Reset this material to its loaded values")
                    .clicked()
                {
                    response.reset_material = true;
                }
            });
        });
        response.material_changed |= material_editor_ui(
            ui,
            &mut self.search,
            material,
            shaders,
            shader_info,
            &mut response.texture_dropped,
        );
    }
}

/// `Label  [X ....] [Y ....] [Z ....]` with axis tiles (scrub on the tile).
/// The mesh block of "Mesh & Material": counts, Scale In Lightmap, Cast Shadows.
fn mesh_ui(ui: &mut Ui, info: &mut ObjectInfoModel, response: &mut InspectorResponse) {
    let Some((vertices, triangles)) = info.mesh else {
        return;
    };
    ui.label(RichText::new("Mesh").strong());
    ui.label(format!("{vertices} vertices · {triangles} triangles"));
    ui.add_enabled_ui(info.static_geometry, |ui| {
        ui.horizontal(|ui| {
            ui.label("Scale In Lightmap");
            if ui
                .add(
                    egui::DragValue::new(&mut info.lightmap_scale)
                        .speed(0.05)
                        .range(0.0..=16.0),
                )
                .on_hover_text(
                    "Per-object lightmap-resolution multiplier (needs Contribute GI)",
                )
                .changed()
            {
                response.object_changed = true;
            }
        });
    });
    if cast_shadows_row(ui, &mut info.cast_shadows, info.mixed.cast_shadows) {
        response.object_changed = true;
    }
}

/// "Cast Shadows" dropdown. `mixed` shows an em dash until a value is picked.
fn cast_shadows_row(ui: &mut Ui, value: &mut zestengine_core::ShadowCasting, mixed: bool) -> bool {
    let mut changed = false;
    crate::widgets::property_row(ui, "Cast Shadows", |ui| {
        let text = if mixed { "\u{2014}" } else { value.label() };
        egui::ComboBox::from_id_salt("zestengine-object-cast-shadows")
            .width(ui.available_width())
            .selected_text(text)
            .show_ui(ui, |ui| {
                for mode in zestengine_core::ShadowCasting::ALL {
                    // Picking the shown value over a mixed selection still applies it.
                    if ui.selectable_label(!mixed && *value == mode, mode.label()).clicked() {
                        changed |= mixed || *value != mode;
                        *value = mode;
                    }
                }
            })
            .response
            .on_hover_text(
                "On: casts shadows. Off: casts none. Two Sided: casts from both faces. \
                 Shadows Only: invisible, but still casts (and occludes in the bake).",
            );
    });
    changed
}

fn transform_row(ui: &mut Ui, label: &str, values: &mut [f32; 3], speed: f64, decimals: usize, mixed: [bool; 3]) -> bool {
    crate::widgets::property_row_w(ui, label, 64.0, |ui| crate::widgets::vec3_field_mixed(ui, values, decimals, speed, mixed))
        .iter()
        .any(|c| *c)
}

/// The Scale row with a link toggle: while linked, changing one axis scales
/// the other two by the same ratio (proportions kept).
fn scale_row(ui: &mut Ui, values: &mut [f32; 3], mixed: [bool; 3]) -> bool {
    let lock_id = egui::Id::new("zestengine-scale-linked");
    let mut linked = ui.data_mut(|d| *d.get_persisted_mut_or_default::<bool>(lock_id));
    let before = *values;
    // The lock sits in the label column, right after "Scale", so the axis
    // fields line up with Position and Rotation above.
    let changed = crate::widgets::property_row_w(ui, "Scale", 64.0, |ui| {
        let row = ui.min_rect();
        let text = egui::WidgetText::from("Scale").into_galley(ui, Some(egui::TextWrapMode::Truncate), 64.0, egui::TextStyle::Body);
        let size = egui::Vec2::splat(crate::theme::H_TOGGLE);
        let at = egui::pos2(row.left() + text.size().x + 4.0, row.center().y - size.y * 0.5);
        let icon = if linked { crate::icons::Icon::Lock } else { crate::icons::Icon::LockOpen };
        let tip = if linked { "Scale linked: axes change together" } else { "Link the scale axes" };
        let mut cell = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(at, size)));
        if crate::widgets::icon_button(&mut cell, icon, tip, linked).clicked() {
            linked = !linked;
        }
        crate::widgets::vec3_field_mixed(ui, values, 2, 0.01, mixed)
    });
    ui.data_mut(|d| d.insert_persisted(lock_id, linked));
    if linked && let Some(axis) = (0..3).find(|&k| changed[k]) {
        let (old, new) = (before[axis], values[axis]);
        for k in (0..3).filter(|&k| k != axis) {
            values[k] = if old.abs() > 1e-6 { before[k] * new / old } else { new };
        }
    }
    changed.iter().any(|c| *c)
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}
