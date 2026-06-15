//! citrus-editor: in-engine editor panels (egui-only, renderer-agnostic).
//!
//! Panels render into plain `Ui`s so the engine can host them as dock tabs:
//! - Inspector: selected object (transform/mesh/material slots) or file
//! - Scene: object list
//! - Files: project browser with drag & drop and asset creation
//!
//! The full world/avatar editor (gizmo toolbox, VRM tooling) keeps growing
//! here through M5.

mod code_editor;
mod components;
mod vim;
mod file_browser;
mod inspector;
mod scene_panel;
mod sections;

pub use code_editor::{
    CodeEditor, CodeEditorResponse, CompletionItem, CompletionState, HoverState, ReferenceItem,
};
// Editor-only component UI. The runtime component types (Component,
// ComponentRegistry, the structs, ComponentCtx, Transform, …) come from
// citrus-core — import those directly, not through the editor.
pub use components::{
    ComponentsResponse, DRAG_FILE_KEY, DRAG_OBJECT_KEY, EditorComponents, Gizmo, GizmoCtx, Inspect,
    InspectCtx, components_ui,
};
pub use file_browser::{FileBrowser, FileBrowserResponse};

/// Register the Phosphor icon font with egui so the file browser (and any UI)
/// can render its icon glyphs. Call once at startup.
pub fn install_icon_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}
// The egui-free material/shader data models live in citrus-core now; re-export
// them so existing `citrus_editor::MaterialModel`-style paths keep working.
pub use citrus_core::{
    AlphaModeModel, MaterialModel, ShaderPropKindUi, ShaderPropUi, ShaderUiInfo,
};
pub use inspector::{
    CodeDiagnostic, InspectorContent, InspectorPanel, InspectorResponse, ObjectHeaderResponse,
    ObjectInfoModel, TransformModel,
};
pub use scene_panel::{SceneObjectRow, ScenePanel, ScenePanelResponse, SpawnKind};
