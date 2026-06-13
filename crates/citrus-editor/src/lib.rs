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
mod file_browser;
mod inspector;
mod scene_panel;
mod sections;

pub use code_editor::{
    CodeEditor, CodeEditorResponse, CompletionItem, CompletionState, HoverState,
};
pub use components::{
    Bob, CameraComponent, Component, ComponentCtx, ComponentRegistry, ComponentsResponse,
    LightComponent, LightKind, LightMode, LightProbeVolume, Spin, TypedComponent, components_ui,
};
pub use file_browser::{FileBrowser, FileBrowserResponse};
pub use inspector::{
    AlphaModeModel, CodeDiagnostic, InspectorContent, InspectorPanel, InspectorResponse,
    MaterialModel, ObjectInfoModel, TransformModel,
};
pub use scene_panel::{SceneObjectRow, ScenePanel, ScenePanelResponse, SpawnKind};
pub use sections::{ShaderPropKindUi, ShaderPropUi, ShaderUiInfo};
