//! Project file browser, Unity-style: a folder tree on the left and an
//! icon grid of the selected folder's contents on the right. Click to
//! inspect, double-click models to import / folders to enter, drag tiles
//! (e.g. `.material` onto a material slot, a viewport mesh, or a folder to
//! move), right-click for create/rename/cut/copy/paste.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use egui::{Align2, FontId, RichText, ScrollArea, Sense, Ui};

const TILE: egui::Vec2 = egui::vec2(78.0, 88.0);

struct Clipboard {
    path: PathBuf,
    /// True = cut (paste moves), false = copy.
    cut: bool,
}

struct Renaming {
    path: PathBuf,
    text: String,
    /// Focus the text field on its first frame.
    focus: bool,
}

pub struct FileBrowser {
    pub root: PathBuf,
    /// Folder whose contents the grid shows.
    current_dir: PathBuf,
    /// Expanded folders in the tree (the root is always open).
    open_dirs: HashSet<PathBuf>,
    clipboard: Option<Clipboard>,
    renaming: Option<Renaming>,
    /// Ferris (the Rust mascot) tile icon for `.rs` files, decoded + uploaded
    /// once on first use.
    ferris: Option<egui::TextureHandle>,
    /// Per-file LSP problem tally (errors, warnings), refreshed each frame from
    /// the engine. Used to badge files (and aggregate onto folders).
    diags: std::collections::HashMap<PathBuf, (u32, u32)>,
}

#[derive(Default)]
pub struct FileBrowserResponse {
    /// File single-clicked: select / show in Inspector.
    pub clicked: Option<PathBuf>,
    /// File double-clicked: open it (the engine picks the action by type —
    /// import model, open code editor, etc.).
    pub activated: Option<PathBuf>,
    /// Create a new asset inside this directory.
    pub create_material_in: Option<PathBuf>,
    pub create_postfx_in: Option<PathBuf>,
    pub create_scene_in: Option<PathBuf>,
    pub create_shader_in: Option<PathBuf>,
    pub create_folder_in: Option<PathBuf>,
    /// New Rust component in the project's plugin crate (location-independent).
    pub create_component: bool,
    /// (old, new) paths for renames/moves, so selection can follow.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// File or folder requested for deletion.
    pub delete: Option<PathBuf>,
    /// Image file requested to become the scene skybox.
    pub set_skybox: Option<PathBuf>,
}

impl FileBrowser {
    pub fn new(root: PathBuf) -> Self {
        Self {
            current_dir: root.clone(),
            root,
            open_dirs: HashSet::new(),
            clipboard: None,
            renaming: None,
            ferris: None,
            diags: std::collections::HashMap::new(),
        }
    }

    /// (errors, warns) for a file, or aggregated over a folder's descendants.
    fn diag_counts(&self, path: &Path, is_dir: bool) -> (u32, u32) {
        if is_dir {
            let (mut e, mut w) = (0, 0);
            for (p, (pe, pw)) in &self.diags {
                if p.starts_with(path) {
                    e += pe;
                    w += pw;
                }
            }
            (e, w)
        } else {
            self.diags.get(path).copied().unwrap_or((0, 0))
        }
    }

    /// Small problem dot (red = has errors, yellow = warnings only).
    fn diag_dot(painter: &egui::Painter, center: egui::Pos2, errors: u32, warns: u32) {
        if errors == 0 && warns == 0 {
            return;
        }
        let color = if errors > 0 {
            egui::Color32::from_rgb(235, 90, 80)
        } else {
            egui::Color32::from_rgb(220, 180, 70)
        };
        painter.circle_filled(center, 4.0, color);
    }

    /// Lazily decode + upload the embedded Ferris image; cheap to clone after.
    fn ferris_texture(&mut self, ctx: &egui::Context) -> egui::TextureHandle {
        self.ferris
            .get_or_insert_with(|| {
                let bytes = include_bytes!("../assets/ferris.png");
                let image = image::load_from_memory(bytes)
                    .expect("embedded ferris.png decodes")
                    .to_rgba8();
                let size = [image.width() as usize, image.height() as usize];
                // Monochrome silhouette: white RGB + original alpha, so drawing
                // it with a tint colours the whole crab one flat colour (matches
                // the rest of the file icons).
                let mono: Vec<u8> = image.pixels().flat_map(|p| [255, 255, 255, p[3]]).collect();
                let color = egui::ColorImage::from_rgba_unmultiplied(size, &mono);
                ctx.load_texture("citrus-ferris", color, egui::TextureOptions::LINEAR)
            })
            .clone()
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        selected: Option<&Path>,
        diags: &std::collections::HashMap<PathBuf, (u32, u32)>,
    ) -> FileBrowserResponse {
        let mut response = FileBrowserResponse::default();
        // Refresh the per-frame diagnostics snapshot for the badge helpers.
        self.diags = diags.clone();

        // F2 starts an inline rename of the selected file/folder (matches the
        // right-click Rename). Gated on no text field already focused so it
        // doesn't fire mid-edit elsewhere.
        if self.renaming.is_none()
            && let Some(path) = selected
            && ui.input(|i| i.key_pressed(egui::Key::F2))
            && !ui.memory(|m| m.focused().is_some())
        {
            self.begin_rename(path);
        }
        if !self.current_dir.starts_with(&self.root) || !self.current_dir.is_dir() {
            self.current_dir = self.root.clone();
        }

        egui::SidePanel::left(ui.id().with("files-tree"))
            .resizable(true)
            .default_width(150.0)
            .width_range(90.0..=320.0)
            .show_inside(ui, |ui| {
                ScrollArea::vertical()
                    .id_salt("files-tree-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let root = self.root.clone();
                        self.tree_row(ui, &root, 0, &mut response);
                        // Space below the tree: drop = move to root,
                        // right-click = ops in the root.
                        let mut rest = ui.available_rect_before_wrap();
                        rest.set_height(rest.height().max(24.0));
                        let bg = ui.allocate_rect(rest, Sense::click());
                        if let Some(payload) = bg.dnd_release_payload::<PathBuf>() {
                            self.move_into(&payload, &root, &mut response);
                        }
                        bg.context_menu(|ui| {
                            self.paste_button(ui, &root, &mut response);
                            self.create_menu(ui, &root, &mut response);
                        });
                    });
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                let at_root = self.current_dir == self.root;
                if ui
                    .add_enabled(!at_root, egui::Button::new("⬆"))
                    .on_hover_text("Parent folder")
                    .clicked()
                    && let Some(parent) = self.current_dir.parent()
                {
                    self.current_dir = parent.to_owned();
                }
                let crumb = self
                    .current_dir
                    .strip_prefix(&self.root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let crumb = if crumb.is_empty() {
                    "(project)".to_owned()
                } else {
                    crumb
                };
                ui.label(RichText::new(crumb).size(14.0).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let dir = self.current_dir.clone();
                    ui.menu_button("➕ Create", |ui| {
                        self.create_menu(ui, &dir, &mut response);
                    });
                });
            });
            ui.separator();
            ScrollArea::vertical()
                .id_salt("files-grid-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // The whole grid container senses clicks (tiles keep
                    // priority), so right-click works in any empty spot —
                    // beside the last tile of a row included.
                    let dir = self.current_dir.clone();
                    let scope =
                        ui.scope_builder(egui::UiBuilder::new().sense(Sense::click()), |ui| {
                            self.grid_ui(ui, selected, &mut response);
                            // Stretch the container over the leftover panel
                            // space so it's interactive too.
                            let mut rest = ui.available_rect_before_wrap();
                            rest.set_height(rest.height().max(24.0));
                            ui.allocate_rect(rest, Sense::hover());
                        });
                    scope.response.context_menu(|ui| {
                        self.paste_button(ui, &dir, &mut response);
                        self.create_menu(ui, &dir, &mut response);
                    });
                });
        });
        response
    }

    // ------------------------------------------------------------- tree

    fn tree_row(
        &mut self,
        ui: &mut Ui,
        dir: &Path,
        depth: usize,
        response: &mut FileBrowserResponse,
    ) {
        if depth > 16 {
            return;
        }
        let is_root = dir == self.root;
        let subdirs = subdirs_of(dir);
        let open = is_root || self.open_dirs.contains(dir);

        if self.renaming.as_ref().is_some_and(|r| r.path == dir) {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), Sense::hover());
            self.rename_editor(ui, rect, response);
        } else {
            // Full-width row: clicks, right-clicks, hover and drops work
            // across the whole line, not just over the text.
            let (rect, row) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), Sense::click());
            let indent = 4.0 + depth as f32 * 12.0;
            let current = dir == self.current_dir;
            if current {
                ui.painter()
                    .rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
            } else if row.hovered() {
                ui.painter()
                    .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
            }
            let text_color = if current {
                ui.visuals().selection.stroke.color
            } else {
                ui.visuals().text_color()
            };
            if !subdirs.is_empty() {
                ui.painter().text(
                    egui::pos2(rect.left() + indent + 6.0, rect.center().y),
                    Align2::CENTER_CENTER,
                    if open { "⏷" } else { "⏵" },
                    FontId::proportional(10.0),
                    ui.visuals().weak_text_color(),
                );
            }
            let name = if is_root {
                "Project".to_owned()
            } else {
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            ui.painter().text(
                egui::pos2(rect.left() + indent + 14.0, rect.center().y),
                Align2::LEFT_CENTER,
                format!("🗀 {name}"),
                FontId::proportional(13.0),
                text_color,
            );
            // Aggregate problem badge for the folder's contents.
            let (errs, warns) = self.diag_counts(dir, true);
            Self::diag_dot(
                &ui.painter(),
                egui::pos2(rect.right() - 9.0, rect.center().y),
                errs,
                warns,
            );

            if row.clicked() {
                let on_arrow = row
                    .interact_pointer_pos()
                    .is_some_and(|p| p.x < rect.left() + indent + 12.0);
                if on_arrow && !subdirs.is_empty() {
                    self.toggle_open(dir);
                } else {
                    // Single click only navigates the grid — folders aren't
                    // inspectable assets, so it doesn't change the selection.
                    self.current_dir = dir.to_owned();
                }
            }
            if row.double_clicked() {
                self.toggle_open(dir);
            }
            if let Some(payload) = row.dnd_release_payload::<PathBuf>() {
                self.move_into(&payload, dir, response);
            }
            if row
                .dnd_hover_payload::<PathBuf>()
                .is_some_and(|p| *p != dir)
            {
                ui.painter().rect_stroke(
                    rect,
                    3.0,
                    egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
                    egui::StrokeKind::Outside,
                );
            }
            let dir = dir.to_owned();
            row.context_menu(|ui| {
                if !is_root {
                    self.item_ops_menu(ui, &dir, response);
                }
                self.paste_button(ui, &dir, response);
                ui.separator();
                self.create_menu(ui, &dir, response);
            });
        }

        if open {
            for sub in subdirs {
                self.tree_row(ui, &sub, depth + 1, response);
            }
        }
    }

    fn toggle_open(&mut self, dir: &Path) {
        if dir == self.root {
            return;
        }
        if !self.open_dirs.remove(dir) {
            self.open_dirs.insert(dir.to_owned());
        }
    }

    // ------------------------------------------------------------- grid

    fn grid_ui(
        &mut self,
        ui: &mut Ui,
        selected: Option<&Path>,
        response: &mut FileBrowserResponse,
    ) {
        let (dirs, files) = list_dir(&self.current_dir);
        if dirs.is_empty() && files.is_empty() {
            ui.label(RichText::new("Empty folder").weak());
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            for path in dirs {
                self.tile(ui, &path, true, selected, response);
            }
            for path in files {
                self.tile(ui, &path, false, selected, response);
            }
        });
    }

    fn tile(
        &mut self,
        ui: &mut Ui,
        path: &Path,
        is_dir: bool,
        selected: Option<&Path>,
        response: &mut FileBrowserResponse,
    ) {
        let (rect, tile) = ui.allocate_exact_size(TILE, Sense::click_and_drag());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let is_selected = selected == Some(path);
        if is_selected {
            ui.painter()
                .rect_filled(rect, 6.0, ui.visuals().selection.bg_fill);
        } else if tile.hovered() {
            ui.painter()
                .rect_filled(rect, 6.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }

        let is_rust = !is_dir
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("rs"));
        let icon_center = egui::pos2(rect.center().x, rect.top() + 26.0);
        let icon_color = if is_selected {
            ui.visuals().selection.stroke.color
        } else {
            ui.visuals().text_color()
        };
        if is_rust {
            // Monochrome Ferris silhouette for .rs files. ~30px tall, keep aspect.
            let tex = self.ferris_texture(ui.ctx());
            let [tw, th] = tex.size();
            let h = 30.0;
            let w = h * tw as f32 / th as f32;
            let img_rect = egui::Rect::from_center_size(icon_center, egui::vec2(w, h));
            ui.painter().image(
                tex.id(),
                img_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                icon_color,
            );
        } else {
            // Phosphor icon glyph for the file/folder type.
            let glyph = if is_dir {
                egui_phosphor::regular::FOLDER
            } else {
                phosphor_icon(icon_kind(path))
            };
            ui.painter().text(
                icon_center,
                Align2::CENTER_CENTER,
                glyph,
                FontId::proportional(30.0),
                icon_color,
            );
        }

        // LSP problem badge at the icon's top-right.
        let (errs, warns) = self.diag_counts(path, is_dir);
        Self::diag_dot(
            &ui.painter(),
            egui::pos2(icon_center.x + 15.0, icon_center.y - 13.0),
            errs,
            warns,
        );

        let renaming_this = self.renaming.as_ref().is_some_and(|r| r.path == path);
        if renaming_this {
            let name_rect = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 2.0, rect.bottom() - 26.0),
                egui::pos2(rect.right() - 2.0, rect.bottom() - 4.0),
            );
            self.rename_editor(ui, name_rect, response);
        } else {
            // Name under the icon, single line, cropped with `…`. Known asset
            // extensions are hidden (the icon conveys the type).
            let name = display_name(path, is_dir);
            let color = if is_selected {
                ui.visuals().selection.stroke.color
            } else {
                ui.visuals().text_color()
            };
            let mut job = egui::text::LayoutJob::default();
            job.append(
                &name,
                0.0,
                egui::TextFormat {
                    font_id: FontId::proportional(13.0),
                    color,
                    ..Default::default()
                },
            );
            job.wrap = egui::text::TextWrapping {
                max_width: TILE.x - 8.0,
                max_rows: 1,
                break_anywhere: true,
                overflow_character: Some('…'),
            };
            job.halign = egui::Align::Center;
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            ui.painter().galley(
                egui::pos2(rect.center().x, rect.bottom() - 32.0),
                galley,
                color,
            );
        }

        tile.dnd_set_drag_payload(path.to_owned());
        if tile.dragged() {
            // Also publish the dragged file's project-relative path into egui
            // memory (a String) so inspector drop-fields can read it across the
            // plugin egui boundary — same pattern as the scene tree's
            // dragged-object key.
            if !is_dir {
                let rel = path
                    .strip_prefix(&self.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                ui.data_mut(|d| d.insert_temp(egui::Id::new(crate::DRAG_FILE_KEY), rel));
            }
            // Floating "ghost" of the dragged tile (icon + name) following the
            // cursor, so the drag has visual feedback. Painted on a top layer,
            // mirroring the scene tree's drag ghost.
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("citrus-drag-ghost"),
                ));
                let glyph = if is_dir {
                    egui_phosphor::regular::FOLDER
                } else {
                    phosphor_icon(icon_kind(path))
                };
                let label = format!("{glyph} {}", display_name(path, is_dir));
                let font = egui::TextStyle::Body.resolve(ui.style());
                let galley = painter.layout_no_wrap(label, font, egui::Color32::WHITE);
                let pad = egui::vec2(8.0, 4.0);
                let origin = pos + egui::vec2(14.0, 6.0);
                let rect = egui::Rect::from_min_size(origin, galley.size() + pad * 2.0);
                painter.rect_filled(
                    rect,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(60, 46, 110, 235),
                );
                painter.rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(150, 120, 220)),
                    egui::StrokeKind::Inside,
                );
                painter.galley(origin + pad, galley, egui::Color32::WHITE);
            }
        }
        if tile.double_clicked() {
            // Double-click opens: folders enter; files are activated.
            if is_dir {
                self.current_dir = path.to_owned();
                self.open_dirs.insert(path.to_owned());
            } else {
                response.activated = Some(path.to_owned());
            }
        } else if tile.clicked() && !renaming_this && !is_dir {
            // Single-click selects a file (shown in the Inspector). Folders are
            // navigation only — they aren't inspectable assets, so a folder
            // click doesn't change the selection.
            response.clicked = Some(path.to_owned());
        }
        if is_dir {
            if let Some(payload) = tile.dnd_release_payload::<PathBuf>() {
                self.move_into(&payload, path, response);
            }
            if tile
                .dnd_hover_payload::<PathBuf>()
                .is_some_and(|p| *p != path)
            {
                ui.painter().rect_stroke(
                    rect,
                    6.0,
                    egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
                    egui::StrokeKind::Outside,
                );
            }
        }
        let path = path.to_owned();
        let parent = self.current_dir.clone();
        tile.context_menu(|ui| {
            self.item_ops_menu(ui, &path, response);
            self.paste_button(ui, &parent, response);
            ui.separator();
            self.create_menu(ui, &parent, response);
        });
    }

    // ----------------------------------------------------------- actions

    fn create_menu(&mut self, ui: &mut Ui, dir: &Path, response: &mut FileBrowserResponse) {
        if ui.button("New Material").clicked() {
            response.create_material_in = Some(dir.to_owned());
            ui.close();
        }
        if ui.button("New Scene").clicked() {
            response.create_scene_in = Some(dir.to_owned());
            ui.close();
        }
        if ui.button("New Shader").clicked() {
            response.create_shader_in = Some(dir.to_owned());
            ui.close();
        }
        if ui.button("New Post FX Profile").clicked() {
            response.create_postfx_in = Some(dir.to_owned());
            ui.close();
        }
        if ui.button("New Component (Rust)").clicked() {
            response.create_component = true;
            ui.close();
        }
        if ui.button("New Folder").clicked() {
            response.create_folder_in = Some(dir.to_owned());
            ui.close();
        }
    }

    /// Start an inline rename of `path` (seeded with its current file name).
    fn begin_rename(&mut self, path: &Path) {
        self.renaming = Some(Renaming {
            path: path.to_owned(),
            text: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            focus: true,
        });
    }

    /// Rename / Cut / Copy / Delete items for one file or folder.
    fn item_ops_menu(&mut self, ui: &mut Ui, path: &Path, response: &mut FileBrowserResponse) {
        if ui.button("Rename").clicked() {
            self.begin_rename(path);
            ui.close();
        }
        if ui.button("Cut").clicked() {
            self.clipboard = Some(Clipboard {
                path: path.to_owned(),
                cut: true,
            });
            ui.close();
        }
        if ui.button("Copy").clicked() {
            self.clipboard = Some(Clipboard {
                path: path.to_owned(),
                cut: false,
            });
            ui.close();
        }
        let is_image = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "bmp" | "tga"));
        if is_image
            && ui
                .button("Set as Skybox")
                .on_hover_text("Use this image as the scene's equirectangular skybox")
                .clicked()
        {
            response.set_skybox = Some(path.to_owned());
            ui.close();
        }
        ui.separator();
        if ui
            .button("🗑 Delete")
            .on_hover_text("Delete this file or folder")
            .clicked()
        {
            response.delete = Some(path.to_owned());
            ui.close();
        }
    }

    fn paste_button(&mut self, ui: &mut Ui, dir: &Path, response: &mut FileBrowserResponse) {
        let Some(clip) = &self.clipboard else { return };
        let label = format!(
            "Paste {:?}{}",
            clip.path.file_name().unwrap_or_default(),
            if clip.cut { " (move)" } else { "" }
        );
        if ui.button(label).clicked() {
            let (source, cut) = (clip.path.clone(), clip.cut);
            let dest = unique_in_dir(dir, &source);
            let result = if cut {
                std::fs::rename(&source, &dest)
            } else {
                copy_recursively(&source, &dest)
            };
            match result {
                Ok(()) => {
                    if cut {
                        response.moved.push((source, dest));
                        self.clipboard = None;
                    }
                }
                Err(e) => tracing::error!("pasting: {e}"),
            }
            ui.close();
        }
    }

    /// Drag-and-drop / cut-paste move into a folder.
    fn move_into(&mut self, source: &Path, dir: &Path, response: &mut FileBrowserResponse) {
        if source.parent() == Some(dir) || dir.starts_with(source) {
            return; // no-op or folder-into-itself
        }
        let dest = unique_in_dir(dir, source);
        match std::fs::rename(source, &dest) {
            Ok(()) => response.moved.push((source.to_owned(), dest)),
            Err(e) => tracing::error!("moving {}: {e}", source.display()),
        }
    }

    /// The inline rename editor, placed at `rect`.
    fn rename_editor(&mut self, ui: &mut Ui, rect: egui::Rect, response: &mut FileBrowserResponse) {
        let Some(renaming) = &mut self.renaming else {
            return;
        };
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        let output = egui::TextEdit::singleline(&mut renaming.text)
            .font(FontId::proportional(11.0))
            .show(&mut child);
        let edit = output.response;
        if renaming.focus {
            edit.request_focus();
            // Pre-select the stem only, so typing replaces the name but
            // keeps the extension.
            let stem_chars = renaming
                .text
                .rfind('.')
                .filter(|&i| i > 0)
                .map(|i| renaming.text[..i].chars().count())
                .unwrap_or_else(|| renaming.text.chars().count());
            let mut state = output.state;
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(stem_chars),
                )));
            state.store(ui.ctx(), edit.id);
            renaming.focus = false;
        }
        let commit = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let cancel =
            ui.input(|i| i.key_pressed(egui::Key::Escape)) || (edit.lost_focus() && !commit);
        if commit {
            let path = renaming.path.clone();
            let new_name = renaming.text.trim().to_owned();
            if !new_name.is_empty() && !new_name.contains('/') {
                let dest = path.with_file_name(&new_name);
                if dest != path {
                    if dest.exists() {
                        tracing::error!("rename: {} already exists", dest.display());
                    } else {
                        match std::fs::rename(&path, &dest) {
                            Ok(()) => {
                                if self.current_dir == path {
                                    self.current_dir = dest.clone();
                                }
                                response.moved.push((path, dest));
                            }
                            Err(e) => tracing::error!("renaming: {e}"),
                        }
                    }
                }
            }
            self.renaming = None;
        } else if cancel {
            self.renaming = None;
        }
    }
}

/// Visible (non-hidden, non-target) sorted subdirectories.
fn subdirs_of(dir: &Path) -> Vec<PathBuf> {
    let (dirs, _) = list_dir(dir);
    dirs
}

/// (folders, files) of a directory, hidden/target filtered, sorted by name.
fn list_dir(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (dirs, files);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        } else {
            files.push(path);
        }
    }
    let by_name = |a: &PathBuf, b: &PathBuf| {
        a.file_name()
            .map(|n| n.to_ascii_lowercase())
            .cmp(&b.file_name().map(|n| n.to_ascii_lowercase()))
    };
    dirs.sort_by(by_name);
    files.sort_by(by_name);
    (dirs, files)
}

/// Destination for `source` inside `dir`, de-duplicated (`name_1.ext`, …).
fn unique_in_dir(dir: &Path, source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let ext = source.extension().map(|e| e.to_string_lossy().into_owned());
    let make = |n: u32| {
        let name = if n == 0 {
            stem.clone()
        } else {
            format!("{stem}_{n}")
        };
        match &ext {
            Some(ext) => dir.join(format!("{name}.{ext}")),
            None => dir.join(name),
        }
    };
    (0..1000)
        .map(make)
        .find(|p| !p.exists())
        .unwrap_or_else(|| make(0))
}

fn copy_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)?.flatten() {
            copy_recursively(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Extensions whose type is conveyed by the icon, so the name hides them.
const KNOWN_EXTS: &[&str] = &[
    "rs", "gltf", "glb", "fbx", "material", "scene", "citrus", "lightmap", "lightdata", "png",
    "jpg", "jpeg", "tga", "bmp", "vert", "frag", "glsl", "slang", "spv", "postfx", "toml", "ron",
    "json", "md",
];

fn lower_ext(path: &Path) -> Option<String> {
    path.extension().and_then(|e| e.to_str()).map(str::to_lowercase)
}

/// The kind of icon drawn for a file, by extension.
#[derive(Clone, Copy)]
enum IconKind {
    Scene,
    Material,
    Model,
    Image,
    Shader,
    PostFx,
    Lightmap,
    Lightdata,
    Config,
    Markdown,
    Citrus,
    Doc,
}

fn icon_kind(path: &Path) -> IconKind {
    match lower_ext(path).as_deref() {
        Some("scene") => IconKind::Scene,
        Some("material") => IconKind::Material,
        Some("gltf" | "glb" | "fbx") => IconKind::Model,
        Some("png" | "jpg" | "jpeg" | "tga" | "bmp") => IconKind::Image,
        Some("vert" | "frag" | "glsl" | "slang" | "spv") => IconKind::Shader,
        Some("postfx") => IconKind::PostFx,
        Some("lightmap") => IconKind::Lightmap,
        Some("lightdata") => IconKind::Lightdata,
        Some("toml" | "ron" | "json") => IconKind::Config,
        Some("md") => IconKind::Markdown,
        Some("citrus") => IconKind::Citrus,
        _ => IconKind::Doc,
    }
}

/// Display label: directory name, or the file stem with a known extension
/// hidden (the icon conveys the type), else the full name.
fn display_name(path: &Path, is_dir: bool) -> String {
    let hide = !is_dir
        && lower_ext(path).is_some_and(|e| KNOWN_EXTS.contains(&e.as_str()));
    let part = if hide { path.file_stem() } else { path.file_name() };
    part.map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Phosphor icon glyph for a file kind.
fn phosphor_icon(kind: IconKind) -> &'static str {
    use egui_phosphor::regular as ph;
    match kind {
        IconKind::Scene => ph::GLOBE_HEMISPHERE_WEST,
        IconKind::Material => ph::SPHERE,
        IconKind::Model => ph::CUBE,
        IconKind::Image => ph::IMAGE,
        IconKind::Shader => ph::SPARKLE,
        IconKind::PostFx => ph::APERTURE,
        IconKind::Lightmap => ph::MAP_TRIFOLD,
        IconKind::Lightdata => ph::DATABASE,
        IconKind::Config => ph::GEAR_SIX,
        IconKind::Markdown => ph::FILE_TEXT,
        IconKind::Citrus => ph::ORANGE_SLICE,
        IconKind::Doc => ph::FILE,
    }
}

