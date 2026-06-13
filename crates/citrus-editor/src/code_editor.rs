//! Dedicated reusable code editor for dock tabs (Rust, GLSL, etc).
//!
//! Syntax highlighting via egui_extras (syntect), plus an LSP-driven
//! completion popup and hover tooltip. The widget renders the popups and
//! applies completion edits locally; the engine feeds it items/hover text and
//! relays completion/hover *requests* to the language server.

use std::path::Path;

use egui::text::{CCursor, CCursorRange};
use egui::{Key, Modifiers, RichText, Ui};

use crate::inspector::CodeDiagnostic;

/// One completion candidate from the language server.
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub detail: String,
}

/// Active completion popup state (owned by the engine per editor).
pub struct CompletionState {
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    /// Char index where the current word started (filter + replace anchor).
    pub anchor_char: usize,
}

/// Hover info to display as a tooltip.
pub struct HoverState {
    pub text: String,
}

/// Response from the code editor UI.
#[derive(Default)]
pub struct CodeEditorResponse {
    pub text_changed: bool,
    pub save_requested: bool,
    pub run_check_requested: bool,
    /// Completion requested (Ctrl+Space) at this cursor char index.
    pub request_completion: Option<usize>,
    /// Hover requested at this char index (Ctrl + pointer over the text).
    pub request_hover: Option<usize>,
    /// Go-to-definition requested (Ctrl+Click) at this char index.
    pub request_definition: Option<usize>,
}

pub struct CodeEditor;

impl CodeEditor {
    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &self,
        ui: &mut Ui,
        path: &Path,
        text: &mut String,
        language: &str,
        dirty: bool,
        diagnostics: &[CodeDiagnostic],
        checking: bool,
        completion: &mut Option<CompletionState>,
        hover: &mut Option<HoverState>,
        // Pending go-to-definition target (0-based line, utf-8 col) to jump to.
        goto: &mut Option<(u32, u32)>,
    ) -> CodeEditorResponse {
        let mut response = CodeEditorResponse::default();

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_owned());

        ui.horizontal(|ui| {
            ui.heading(RichText::new(&name).strong());
            if ui
                .add_enabled(dirty, egui::Button::new("💾 Save"))
                .clicked()
            {
                response.save_requested = true;
            }
            if checking {
                ui.spinner();
            }
            if dirty {
                ui.label(RichText::new("unsaved").small().weak());
            }
            // Fixed-height problem counts (the full list lives in the
            // Inspector so the code area never shifts while typing).
            let errors = diagnostics.iter().filter(|d| d.level == "error").count();
            let warns = diagnostics.len() - errors;
            if errors > 0 {
                ui.label(
                    RichText::new(format!("⛔ {errors}"))
                        .small()
                        .color(ui.visuals().error_fg_color),
                );
            }
            if warns > 0 {
                ui.label(
                    RichText::new(format!("⚠ {warns}"))
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            }
            ui.label(
                RichText::new("Ctrl+Space: complete · Ctrl+hover: info")
                    .small()
                    .weak(),
            );
        });
        ui.label(RichText::new(path.display().to_string()).small().weak());
        ui.separator();

        // While the completion popup is open, steal navigation keys before the
        // TextEdit consumes them.
        let (mut nav_down, mut nav_up, mut accept, mut dismiss) = (false, false, false, false);
        if completion.is_some() {
            ui.input_mut(|i| {
                nav_down = i.consume_key(Modifiers::NONE, Key::ArrowDown);
                nav_up = i.consume_key(Modifiers::NONE, Key::ArrowUp);
                accept = i.consume_key(Modifiers::NONE, Key::Enter)
                    || i.consume_key(Modifiers::NONE, Key::Tab);
                dismiss = i.consume_key(Modifiers::NONE, Key::Escape);
            });
        }
        let trigger_completion = ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space));

        let theme = egui_extras::syntax_highlighting::CodeTheme::from_memory(ui.ctx(), ui.style());
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            let mut job = egui_extras::syntax_highlighting::highlight(
                ui.ctx(),
                ui.style(),
                &theme,
                buf.as_str(),
                language,
            );
            job.wrap.max_width = wrap_width;
            ui.fonts_mut(|f| f.layout_job(job))
        };

        // The editor scrolls so long files (and go-to-definition jumps) work.
        let goto_target = goto.take();
        let output = egui::ScrollArea::vertical()
            .id_salt("code-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let output = egui::TextEdit::multiline(text)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(25)
                    .layouter(&mut layouter)
                    .show(ui);
                // Jump the cursor to a go-to-definition target.
                if let Some((line, col)) = goto_target {
                    let target = line_col_to_char(text, line, col);
                    let cc = CCursor::new(target);
                    let mut st = output.state.clone();
                    st.cursor.set_char_range(Some(CCursorRange::one(cc)));
                    st.store(ui.ctx(), output.response.id);
                    output.response.request_focus();
                    let rect = output
                        .galley
                        .pos_from_cursor(cc)
                        .translate(output.galley_pos.to_vec2());
                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                }
                output
            })
            .inner;

        response.text_changed = output.response.changed();
        let cursor_char = output.cursor_range.map(|r| r.primary.index);

        // Inline diagnostics: squiggle under each problem line, with the
        // message on hover.
        draw_diagnostics(ui, text, &output, diagnostics);

        if trigger_completion && let Some(c) = cursor_char {
            response.request_completion = Some(c);
        }

        // Ctrl + pointer over the text: hover info; Ctrl+Click: go to def.
        let ctrl = ui.input(|i| i.modifiers.ctrl);
        if let Some(pos) = output.response.hover_pos() {
            if ctrl {
                let local = pos - output.galley_pos;
                let cc = output.galley.cursor_from_pos(local);
                response.request_hover = Some(cc.index);
                if output.response.clicked() {
                    response.request_definition = Some(cc.index);
                }
            } else {
                *hover = None;
            }
        } else {
            *hover = None;
        }
        if let Some(h) = hover.as_ref()
            && !h.text.is_empty()
            && let Some(pos) = output.response.hover_pos()
        {
            egui::Area::new(egui::Id::new("lsp-hover"))
                .order(egui::Order::Tooltip)
                .fixed_pos(pos + egui::vec2(12.0, 16.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(480.0);
                        ui.label(RichText::new(&h.text).small());
                    });
                });
        }

        // Completion popup. Take the state out to avoid borrow conflicts.
        if let Some(mut state) = completion.take() {
            let keep = render_completion(
                ui,
                text,
                &output,
                &mut state,
                cursor_char,
                nav_down,
                nav_up,
                accept,
                dismiss,
                &mut response,
            );
            if keep {
                *completion = Some(state);
            }
        }

        response
    }
}

/// Render the completion popup and apply navigation/acceptance. Returns true
/// to keep the popup open.
#[allow(clippy::too_many_arguments)]
fn render_completion(
    ui: &mut Ui,
    text: &mut String,
    output: &egui::text_edit::TextEditOutput,
    state: &mut CompletionState,
    cursor_char: Option<usize>,
    nav_down: bool,
    nav_up: bool,
    accept: bool,
    dismiss: bool,
    response: &mut CodeEditorResponse,
) -> bool {
    if dismiss {
        return false;
    }
    let Some(cur) = cursor_char else {
        return false;
    };
    if cur < state.anchor_char {
        return false; // moved before the word
    }

    let prefix: String = text
        .chars()
        .skip(state.anchor_char)
        .take(cur - state.anchor_char)
        .collect();
    let lower = prefix.to_lowercase();
    let mut filtered: Vec<usize> = (0..state.items.len())
        .filter(|&i| state.items[i].label.to_lowercase().starts_with(&lower))
        .collect();
    if filtered.is_empty() && !lower.is_empty() {
        filtered = (0..state.items.len())
            .filter(|&i| state.items[i].label.to_lowercase().contains(&lower))
            .collect();
    }
    if filtered.is_empty() {
        return false;
    }

    let n = filtered.len();
    let mut sel = state.selected.min(n - 1);
    if nav_down {
        sel = (sel + 1) % n;
    }
    if nav_up {
        sel = (sel + n - 1) % n;
    }
    state.selected = sel;

    // Render the popup and capture a click.
    let cursor_rect = output.galley.pos_from_cursor(CCursor::new(cur));
    let screen = output.galley_pos + cursor_rect.left_bottom().to_vec2();
    let mut clicked: Option<usize> = None;
    egui::Area::new(egui::Id::new("lsp-completion"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(420.0);
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for (vis, &item_i) in filtered.iter().enumerate() {
                            let it = &state.items[item_i];
                            let label = if it.detail.is_empty() {
                                it.label.clone()
                            } else {
                                format!("{}   {}", it.label, it.detail)
                            };
                            let item_resp = ui.selectable_label(vis == sel, label);
                            if item_resp.clicked() {
                                clicked = Some(vis);
                            }
                            // Keep the keyboard-selected item visible.
                            if vis == sel && (nav_down || nav_up) {
                                item_resp.scroll_to_me(Some(egui::Align::Center));
                            }
                        }
                    });
            });
        });

    let accept_vis = if accept { Some(sel) } else { clicked };
    if let Some(vis) = accept_vis {
        let item = &state.items[filtered[vis]];
        let insert = item.insert_text.clone();
        let new_cursor = replace_range_chars(text, state.anchor_char, cur, &insert);
        let mut st = output.state.clone();
        st.cursor
            .set_char_range(Some(CCursorRange::one(CCursor::new(new_cursor))));
        st.store(ui.ctx(), output.response.id);
        response.text_changed = true;
        return false;
    }
    true
}

fn replace_range_chars(text: &mut String, start_char: usize, end_char: usize, repl: &str) -> usize {
    let start_byte = byte_of_char(text, start_char);
    let end_byte = byte_of_char(text, end_char);
    text.replace_range(start_byte..end_byte, repl);
    start_char + repl.chars().count()
}

fn byte_of_char(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// (0-based line, utf-8 byte column) → char index in `text`.
fn line_col_to_char(text: &str, line: u32, col: u32) -> usize {
    let (start, _) = line_char_range(text, line as usize);
    let chars: Vec<char> = text.chars().collect();
    let mut bytes = 0u32;
    let mut idx = start;
    while idx < chars.len() && chars[idx] != '\n' && bytes < col {
        bytes += chars[idx].len_utf8() as u32;
        idx += 1;
    }
    idx
}

/// (start, end) char indices of the 0-based line (end excludes the newline).
fn line_char_range(text: &str, line0: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0usize;
    if line0 > 0 {
        let mut line = 0usize;
        let mut found = false;
        for (i, &c) in chars.iter().enumerate() {
            if c == '\n' {
                line += 1;
                if line == line0 {
                    start = i + 1;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return (chars.len(), chars.len());
        }
    }
    let mut end = start;
    while end < chars.len() && chars[end] != '\n' {
        end += 1;
    }
    (start, end)
}

/// Draw a wavy squiggle under each diagnostic line and show its message on
/// hover. Diagnostic lines are 1-based (engine convention).
fn draw_diagnostics(
    ui: &Ui,
    text: &str,
    output: &egui::text_edit::TextEditOutput,
    diagnostics: &[CodeDiagnostic],
) {
    if diagnostics.is_empty() {
        return;
    }
    let painter = ui.painter_at(output.response.rect);
    let hover_pos = output.response.hover_pos();
    let mut tip: Option<(egui::Pos2, String, bool)> = None;

    for d in diagnostics {
        let is_error = d.level == "error";
        let color = if is_error {
            egui::Color32::from_rgb(235, 90, 80)
        } else {
            egui::Color32::from_rgb(220, 180, 70)
        };
        let line0 = d.line.saturating_sub(1) as usize;
        let (s, e) = line_char_range(text, line0);
        let start_rect = output.galley.pos_from_cursor(CCursor::new(s));
        let end_rect = output.galley.pos_from_cursor(CCursor::new(e.max(s)));
        // Single visual row only (wrapped lines underline their first row).
        let same_row = (start_rect.top() - end_rect.top()).abs() < 1.0;
        let y = output.galley_pos.y + start_rect.bottom() - 1.0;
        let x0 = output.galley_pos.x + start_rect.left();
        let x1 = if same_row && end_rect.right() > start_rect.left() {
            output.galley_pos.x + end_rect.right()
        } else {
            x0 + 40.0
        };
        squiggle(&painter, x0, x1.max(x0 + 6.0), y, color);

        // Hover over the row → tooltip with the message.
        if let Some(p) = hover_pos {
            let row_top = output.galley_pos.y + start_rect.top();
            let row_bot = output.galley_pos.y + start_rect.bottom();
            if p.y >= row_top && p.y <= row_bot {
                tip = Some((p, d.message.clone(), is_error));
            }
        }
    }

    if let Some((p, msg, is_error)) = tip {
        egui::Area::new(egui::Id::new("diag-tip"))
            .order(egui::Order::Tooltip)
            .fixed_pos(p + egui::vec2(12.0, 16.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(480.0);
                    let color = if is_error {
                        ui.visuals().error_fg_color
                    } else {
                        ui.visuals().warn_fg_color
                    };
                    ui.label(RichText::new(msg).small().color(color));
                });
            });
    }
}

/// Wavy underline between x0 and x1 at baseline y.
fn squiggle(painter: &egui::Painter, x0: f32, x1: f32, y: f32, color: egui::Color32) {
    let step = 3.0;
    let amp = 1.5;
    let mut points = Vec::new();
    let mut x = x0;
    let mut up = true;
    while x <= x1 {
        points.push(egui::pos2(x, y + if up { 0.0 } else { amp }));
        x += step;
        up = !up;
    }
    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, egui::Stroke::new(1.0, color)));
    }
}
