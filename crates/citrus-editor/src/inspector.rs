//! Unified Inspector panel: shows the selected object (transform, mesh,
//! material slots + material editor) or the selected file (.material editor,
//! .scene loader, generic file info).

use std::path::PathBuf;

use egui::{DragValue, Frame, RichText, Ui};

use crate::sections::material_editor_ui;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlphaModeModel {
    Opaque,
    Cutout,
    Blend,
}

impl AlphaModeModel {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Opaque => "Opaque",
            Self::Cutout => "Cutout",
            Self::Blend => "Transparent",
        }
    }
}

/// Editor-side view of one standard-shader material.
#[derive(Clone, PartialEq)]
pub struct MaterialModel {
    pub name: String,
    pub shader: String,
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub occlusion_strength: f32,
    pub toon_enabled: bool,
    pub toon_steps: f32,
    pub pbr_toon_blend: f32,
    pub emission_enabled: bool,
    pub emission_color: [f32; 3],
    pub emission_intensity: f32,
    pub alpha_mode: AlphaModeModel,
    pub alpha_cutoff: f32,
    pub has_normal_texture: bool,
    pub normal_map_enabled: bool,
    pub normal_strength: f32,
    pub double_sided: bool,
}

#[derive(Clone, PartialEq)]
pub struct TransformModel {
    pub translation: [f32; 3],
    /// Euler XYZ in degrees.
    pub rotation_deg: [f32; 3],
    pub scale: [f32; 3],
}

pub struct ObjectInfoModel {
    pub name: String,
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
    },
    MaterialFile {
        path: String,
        material: &'a mut MaterialModel,
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
}

pub struct InspectorPanel {
    search: String,
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
        }
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
            InspectorContent::Object { info, material } => {
                self.object_ui(ui, info, material, shaders, &mut response);
            }
            InspectorContent::MaterialFile {
                path,
                material,
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
                    material_editor_ui(ui, &mut self.search, material, shaders);
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

    fn object_ui(
        &mut self,
        ui: &mut Ui,
        info: &mut ObjectInfoModel,
        material: Option<&mut MaterialModel>,
        shaders: &[&str],
        response: &mut InspectorResponse,
    ) {
        ui.horizontal(|ui| {
            if ui.text_edit_singleline(&mut info.name).changed() {
                response.object_changed = true;
            }
            ui.label(RichText::new(info.kind).small().weak());
        });
        ui.separator();

        ui.label(RichText::new("Transform").strong());
        let t = &mut info.transform;
        egui::Grid::new("transform-grid")
            .num_columns(4)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                response.object_changed |=
                    transform_row(ui, "Location", &mut t.translation, 0.02);
                response.object_changed |=
                    transform_row(ui, "Rotation", &mut t.rotation_deg, 0.5);
                response.object_changed |= transform_row(ui, "Scale", &mut t.scale, 0.01);
            });

        let Some(material) = material else {
            return; // empties / cameras: transform only
        };
        ui.separator();
        if let Some((vertices, triangles)) = info.mesh {
            ui.label(RichText::new("Mesh").strong());
            ui.label(format!("{vertices} vertices · {triangles} triangles"));
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
                ui.label(
                    RichText::new("drop a .material file here")
                        .small()
                        .weak(),
                );
            })
            .response;
        if let Some(hover) = slot.dnd_hover_payload::<PathBuf>() {
            if hover.extension().is_some_and(|e| e == "material") {
                ui.painter().rect_stroke(
                    slot.rect,
                    4.0,
                    egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                    egui::StrokeKind::Outside,
                );
            }
        }
        if let Some(dropped) = slot.dnd_release_payload::<PathBuf>() {
            if dropped.extension().is_some_and(|e| e == "material") {
                response.material_dropped = Some((*dropped).clone());
            }
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
        response.material_changed |= material_editor_ui(ui, &mut self.search, material, shaders);
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
