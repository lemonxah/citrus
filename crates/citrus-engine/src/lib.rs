//! citrus-engine: application loop, scene management, editor shell.
//!
//! The editor is a dockable layout (egui_dock) around a transparent
//! Viewport tab: Scene list, unified Inspector, project Files browser,
//! menu bar, transform gizmos, picking, and drag & drop assets.

// Always compiled (runtime + bundling path; no egui / editor).
mod audio;
mod bundle;
mod humanoid;
pub mod audio_mixer;
pub mod navmesh;
pub mod particles;
pub mod prefab;
mod log_capture;
pub mod physics;
mod plugins;
mod game_ui;
mod play;
mod realtime_gi;
mod runtime;
mod scene;
mod scene_loader;
// Public so the headless `gi_preview` example can drive the real probe march.
pub mod sw_gi;
mod input_engine;
mod net;
mod voice;
mod shaders;

// Editor-only modules (egui, gizmos, LSP, undo, free-fly camera, window icon).
#[cfg(feature = "editor")]
mod camera;
#[cfg(feature = "editor")]
mod crash;
#[cfg(feature = "editor")]
mod gizmo;
#[cfg(feature = "editor")]
mod icon;
#[cfg(feature = "editor")]
mod lsp;
#[cfg(feature = "editor")]
mod splash;
#[cfg(feature = "editor")]
mod tasks;
#[cfg(feature = "editor")]
mod undo;

pub use runtime::{GameConfig, run_game};
pub use bundle::{build_project_dir, scaffold_project};


/// Install the tracing subscriber (stdout + the in-app Log tab capture).
pub fn init_logging() {
    log_capture::init();
}

#[cfg(feature = "editor")]
mod command_palette;
#[cfg(feature = "editor")]
mod editor_app;
#[cfg(feature = "editor")]
mod ui_edit;
#[cfg(feature = "editor")]
mod ui_editor;
#[cfg(feature = "editor")]
mod ui_templates;
#[cfg(feature = "editor")]
pub use editor_app::{AppConfig, run};
