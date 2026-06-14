//! Unified Inspector panel: shows the selected object (transform, mesh,
//! material slots + material editor) or the selected file (.material editor,
//! .scene loader, generic file info).

use std::path::PathBuf;

use egui::{DragValue, Frame, RichText, Ui};

use citrus_core::{Component, ComponentRegistry, MaterialModel, ObjectId, ShaderUiInfo};

use crate::components::{EditorComponents, components_ui};
use crate::sections::material_editor_ui;

#[derive(Clone, PartialEq)]
pub struct TransformModel {
    pub translation: [f32; 3],
    /// Euler XYZ in degrees.
    pub rotation_deg: [f32; 3],
    pub scale: [f32; 3],
}

pub struct ObjectInfoModel {
    pub name: String,
    /// Whether the object renders / contributes light.
    pub enabled: bool,
    /// Non-moving: included in the lighting bake (lightmaps + occluder).
    pub static_geometry: bool,
    /// Per-object lightmap-resolution multiplier ("Scale In Lightmap").
    pub lightmap_scale: f32,
    /// "Mesh" / "Empty" / "Camera" / "Primitive".
    pub kind: &'static str,
    pub transform: TransformModel,
    /// (vertices, triangles) for mesh objects.
    pub mesh: Option<(u32, u32)>,
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

#[derive(Default)]
pub struct InspectorResponse {
    pub object_changed: bool,
    pub material_changed: bool,
    pub reset_material: bool,
    pub save_material: bool,
    pub load_scene: bool,
    /// A `.material` file was dropped on the material slot.
    pub material_dropped: Option<PathBuf>,
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

    /// Draw the lock toggle header (a 🔒 button). Returns true if the lock
    /// state changed this frame (the engine snapshots the selection then).
    pub fn lock_header(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            let icon = if self.locked { "🔒" } else { "🔓" };
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
                        .add_enabled(dirty, egui::Button::new("💾 Save"))
                        .clicked()
                    {
                        response.save_material = true;
                    }
                    if dirty {
                        ui.label(RichText::new("unsaved changes").small().weak());
                    }
                });
                ui.separator();
                response.material_changed |=
                    material_editor_ui(ui, &mut self.search, material, shaders, shader_info);
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
        shaders: &[&str],
        response: &mut InspectorResponse,
    ) {
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut info.enabled, "")
                .on_hover_text("Enable / disable (skips rendering and lighting)")
                .changed()
            {
                response.object_changed = true;
            }
            if ui.text_edit_singleline(&mut info.name).changed() {
                response.object_changed = true;
            }
            ui.label(RichText::new(info.kind).small().weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .checkbox(&mut info.static_geometry, "Contribute GI")
                    .on_hover_text(
                        "Non-moving: included in the lighting bake (lightmapped + occluder)",
                    )
                    .changed()
                {
                    response.object_changed = true;
                }
            });
        });
        ui.separator();

        ui.label(RichText::new("Transform").strong());
        let t = &mut info.transform;
        egui::Grid::new("transform-grid")
            .num_columns(4)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                response.object_changed |= transform_row(ui, "Location", &mut t.translation, 0.02);
                response.object_changed |= transform_row(ui, "Rotation", &mut t.rotation_deg, 0.5);
                response.object_changed |= transform_row(ui, "Scale", &mut t.scale, 0.01);
            });

        if let Some(material) = material {
            self.material_ui(ui, info, material, shader_info, shaders, response);
        }

        // Components (Unity-style: list + Add Component at the bottom).
        ui.separator();
        ui.label(RichText::new("Components").strong());
        let comp = components_ui(ui, components, registry, editor_components, objects);
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
        ui.separator();
        if let Some((vertices, triangles)) = info.mesh {
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
            ui.separator();
        }
        ui.label(RichText::new("Material Slots").strong());
        // One slot per object for now (imports split multi-material meshes
        // into one object per part).
        let slot = Frame::group(ui.style())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Slot 0");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(&material.name).strong());
                    });
                });
                ui.label(RichText::new("drop a .material file here").small().weak());
            })
            .response;
        if let Some(hover) = slot.dnd_hover_payload::<PathBuf>()
            && hover.extension().is_some_and(|e| e == "material")
        {
            ui.painter().rect_stroke(
                slot.rect,
                4.0,
                egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                egui::StrokeKind::Outside,
            );
        }
        if let Some(dropped) = slot.dnd_release_payload::<PathBuf>()
            && dropped.extension().is_some_and(|e| e == "material")
        {
            response.material_dropped = Some((*dropped).clone());
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
        response.material_changed |=
            material_editor_ui(ui, &mut self.search, material, shaders, shader_info);
    }
}

/// One grid row: `Label   X [....]  Y [....]  Z [....]` — all rows align
/// because the surrounding Grid sizes columns uniformly.
fn transform_row(ui: &mut Ui, label: &str, values: &mut [f32; 3], speed: f64) -> bool {
    let mut changed = false;
    ui.label(label);
    for (axis, value) in ["X", "Y", "Z"].iter().zip(values.iter_mut()) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(*axis).weak());
            changed |= ui
                .add_sized(
                    [60.0, 18.0],
                    DragValue::new(value).speed(speed).max_decimals(3),
                )
                .changed();
        });
    }
    ui.end_row();
    changed
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
