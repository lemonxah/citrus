//! citrus-engine: application loop, scene management, editor shell.
//!
//! The editor is a dockable layout (egui_dock) around a transparent
//! Viewport tab: Scene list, unified Inspector, project Files browser,
//! menu bar, transform gizmos, picking, and drag & drop assets.

// Always compiled (runtime + bundling path; no egui / editor).
pub mod anim_graph;
mod audio;
mod cloth;
mod face_drive;
mod face_input;
mod slimevr;
#[cfg(feature = "editor")]
mod ron_fields;
mod viseme;
mod voice_codec;
mod bundle;
mod humanoid;
pub mod resource_tracker;
mod body;
pub mod audio_mixer;
pub mod navmesh;
pub mod particles;
pub mod water_render;
pub mod water_fx;
pub mod water_carry;
#[cfg(feature = "editor")]
mod editor_water;
mod water_walls;
mod underwater;
pub mod prefab;
mod log_capture;
pub mod physics;
pub mod authored_collision;
mod plugins;
mod game_ui;
mod play;
mod realtime_gi;
mod refl_slots;
mod runtime;
mod scene;
mod scene_loader;
// User-generated content packages (.citruspak) loaded at runtime.
pub mod content;
pub mod occlusion;
// Public so the headless `gi_preview` example can drive the real probe march.
pub mod sw_gi;
mod input_engine;
// Public so tools (dedicated servers, bots, test harnesses) can drive sessions directly.
pub mod net;
mod net_identity;
pub mod net_predict;
mod net_relay;
mod net_secure;
mod net_sync;
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
mod bake_store;
#[cfg(feature = "editor")]
mod undo;

pub use runtime::{GameConfig, XrMode, run_game};
#[cfg(target_os = "android")]
pub use runtime::{android_assets_root, run_game_android};
#[cfg(target_os = "android")]
pub use winit::platform::android::activity::AndroidApp;
pub use bundle::{BuildTarget, build_project_dir, scaffold_project};
pub mod templates;
pub use templates::{ProjectTemplate, TemplateCategory};


/// Install the tracing subscriber (stdout, plus the editor's Log tab capture).
pub fn init_logging() {
    log_capture::init();
}

#[cfg(feature = "editor")]
mod command_palette;
#[cfg(feature = "editor")]
mod editor_app;
#[cfg(feature = "editor")]
mod net_window;
#[cfg(feature = "editor")]
mod new_project_ui;
#[cfg(feature = "editor")]
mod river_edit;
#[cfg(feature = "editor")]
mod cloth_paint;
#[cfg(feature = "editor")]
mod terrain_paint;
#[cfg(feature = "editor")]
mod ui_edit;
#[cfg(feature = "editor")]
mod ui_editor;
#[cfg(feature = "editor")]
mod anim_controller_editor;
#[cfg(feature = "editor")]
mod anim_clip_editor;
mod ui_templates;
#[cfg(feature = "editor")]
pub use editor_app::{AppConfig, run};
