//! citrus-editor: in-engine editor panels (egui-only, renderer-agnostic).
//!
//! Panels render into plain `Ui`s so the engine can host them as dock tabs:
//! - Inspector: selected object (transform/mesh/material slots) or file
//! - Scene: object list
//! - Files: project browser with drag & drop and asset creation
//!
//! The full world/avatar editor (gizmo toolbox, VRM tooling) keeps growing
//! here through M5.

mod file_browser;
mod inspector;
mod scene_panel;
mod sections;

pub use file_browser::{FileBrowser, FileBrowserResponse};
pub use inspector::{
    AlphaModeModel, InspectorContent, InspectorPanel, InspectorResponse, MaterialModel,
    ObjectInfoModel, TransformModel,
};
pub use scene_panel::{SceneObjectRow, ScenePanel, ScenePanelResponse};

/// Registered shaders selectable on materials. Custom shaders (TODO.md)
/// will extend this at runtime.
pub const SHADER_REGISTRY: &[&str] = &["standard"];
