//! citrus-editor: in-engine editor panels (egui-only, renderer-agnostic).
//!
//! Panels render into plain `Ui`s so the engine can host them as dock tabs:
//! - Inspector: selected object (transform/mesh/material slots) or file
//! - Scene: object list
//! - Files: project browser with drag & drop and asset creation
//!
//! The full world/avatar editor (gizmo toolbox, VRM tooling) keeps growing
//! here through M5.

mod audio_ui;
mod cloth_ui;
pub use cloth_ui::{CLOTH_PAINT_HINT, ClothPaintTool};
mod ik_ui;
mod face_ui;
mod water_ui;
pub use water_ui::{RIVER_PATH_HINT, RiverHandle, RiverPathEdit, river_handle};
mod physics_ui;
mod input_ui;
pub use input_ui::{BindingsEditor, RebindTarget, bindings_editor_ui};
mod waterfall_ui;
mod water_container_ui;
pub use physics_ui::{physics_settings_ui, physmat_editor_ui};
mod code_editor;
mod components;
mod kit_ui;
pub mod icons;
pub mod theme;
pub mod widgets;
mod vim;
mod file_browser;
mod inspector;
mod scene_panel;
mod sections;
mod shader_ui;

pub use code_editor::{
    CodeEditor, CodeEditorResponse, CompletionItem, CompletionState, HoverState, ReferenceItem,
};
// Editor-only component UI. The runtime component types (Component,
// ComponentRegistry, the structs, ComponentCtx, Transform, …) come from
// citrus-core. Import those directly, not through the editor.
pub use components::{
    ASSET_TEMPLATES_KEY, AUTHORED_COLLISION_KEY, AssetEditor, AssetTemplate, ComponentsResponse, DRAG_FILE_KEY, DRAG_OBJECT_KEY, EditorComponents,
    Gizmo, GizmoEdit, GizmoSpec, Inspect, InspectCtx, MIXED_COMPONENTS_KEY, OPEN_ANIM_CONTROLLER_KEY,
    OPEN_FILE_KEY, PING_FILE_KEY, PLAY_MODE_KEY, PROJECT_ROOT_KEY, asset_browse_button, asset_chip, asset_new_button, components_ui,
    resolve_slot,
};
pub use audio_ui::{AudioSettingsResponse, audio_settings_ui};
pub use file_browser::{FileBrowser, FileBrowserResponse};
/// Re-export the icon font so the engine's editor UI can use the same glyphs
/// (no emojis) the file browser does.
pub use egui_phosphor;

/// Install the editor fonts: IBM Plex Sans / Mono first, egui's defaults
/// behind them, and the Phosphor icon font. Call once at startup.
pub fn install_icon_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    theme::install_fonts(&mut fonts);
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}
// The egui-free material/shader data models live in citrus-core now; re-export
// them so existing `citrus_editor::MaterialModel`-style paths keep working.
pub use citrus_core::{
    AlphaModeModel, MaterialModel, ShaderCondUi, ShaderGroupUi, ShaderPresetUi, ShaderPropKindUi,
    ShaderPropUi, ShaderUiInfo,
};
pub use inspector::{
    CodeDiagnostic, InspectorContent, InspectorPanel, InspectorResponse, MixedInfo, ObjectHeaderResponse,
    ObjectInfoModel, TransformModel,
};
pub use scene_panel::{SceneObjectRow, ScenePanel, ScenePanelResponse, SpawnKind, UiPreset};
