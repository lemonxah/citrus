//! Project file browser: tree of every file in the project folder.
//! Click to inspect, double-click models to import, drag files (e.g.
//! `.material` onto a material slot or a viewport mesh), right-click
//! folders to create assets.

use std::path::{Path, PathBuf};

use egui::{CollapsingHeader, Label, RichText, ScrollArea, Sense, Ui};

pub struct FileBrowser {
    pub root: PathBuf,
}

#[derive(Default)]
pub struct FileBrowserResponse {
    /// File clicked: show in Inspector.
    pub clicked: Option<PathBuf>,
    /// Model file double-clicked: import into the scene.
    pub import_model: Option<PathBuf>,
    /// Create a new asset inside this directory.
    pub create_material_in: Option<PathBuf>,
    pub create_scene_in: Option<PathBuf>,
    pub create_folder_in: Option<PathBuf>,
}

impl FileBrowser {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn ui(&mut self, ui: &mut Ui, selected: Option<&Path>) -> FileBrowserResponse {
        let mut response = FileBrowserResponse::default();
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.root.display().to_string()).small().weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("➕ Create", |ui| {
                    create_menu(ui, &self.root, &mut response);
                });
            });
        });
        ui.separator();
        let root = self.root.clone();
        ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            dir_contents_ui(ui, &root, selected, &mut response);
        });
        response
    }
}

fn create_menu(ui: &mut Ui, dir: &Path, response: &mut FileBrowserResponse) {
    if ui.button("New Material").clicked() {
        response.create_material_in = Some(dir.to_owned());
        ui.close();
    }
    if ui.button("New Scene").clicked() {
        response.create_scene_in = Some(dir.to_owned());
        ui.close();
    }
    if ui.button("New Folder").clicked() {
        response.create_folder_in = Some(dir.to_owned());
        ui.close();
    }
}

fn dir_contents_ui(
    ui: &mut Ui,
    dir: &Path,
    selected: Option<&Path>,
    response: &mut FileBrowserResponse,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        ui.label(RichText::new("(unreadable)").weak());
        return;
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.is_dir() {
            dirs.push((name, path));
        } else {
            files.push((name, path));
        }
    }
    dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    for (name, path) in dirs {
        let header = CollapsingHeader::new(format!("🗀 {name}"))
            .id_salt(&path)
            .show(ui, |ui| {
                dir_contents_ui(ui, &path, selected, response);
            });
        header.header_response.context_menu(|ui| {
            create_menu(ui, &path, response);
        });
    }

    for (name, path) in files {
        let is_selected = selected == Some(path.as_path());
        let icon = file_icon(&path);
        let text = if is_selected {
            RichText::new(format!("{icon} {name}")).strong()
        } else {
            RichText::new(format!("{icon} {name}"))
        };
        let row = ui.add(
            Label::new(text)
                .sense(Sense::click_and_drag())
                .selectable(false),
        );
        row.dnd_set_drag_payload(path.clone());
        if row.double_clicked() && citrus_assets_is_model(&path) {
            response.import_model = Some(path.clone());
        } else if row.clicked() {
            response.clicked = Some(path.clone());
        }
        row.context_menu(|ui| {
            if let Some(parent) = path.parent() {
                create_menu(ui, parent, response);
            }
        });
    }
}

fn citrus_assets_is_model(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some("gltf" | "glb" | "fbx")
    )
}

fn file_icon(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("gltf" | "glb" | "fbx") => "🧊",
        Some("material") => "🎨",
        Some("scene") => "🌍",
        Some("png" | "jpg" | "jpeg" | "tga") => "🖼",
        Some("rs") => "🦀",
        Some("vert" | "frag" | "glsl" | "slang" | "spv") => "✨",
        Some("toml" | "ron" | "json") => "⚙",
        Some("md") => "📄",
        _ => "·",
    }
}
