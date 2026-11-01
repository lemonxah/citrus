//! `project.zestengine` — per-project state (RON): project name, the scene to
//! reopen on startup, and engine settings specific to this project.
//! Created on first run; extended with `#[serde(default)]` fields as the
//! engine grows so old files keep loading.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

pub const PROJECT_FILE_NAME: &str = "project.zestengine";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectFile {
    pub name: String,
    /// Project-relative path of the last opened scene; reloaded on startup
    /// so the editor reopens the scene you were last in.
    pub last_scene: Option<String>,
    /// Project-relative path of the scene a built game loads first. Distinct
    /// from `last_scene`: the editor restores `last_scene`, while a shipped game
    /// boots `boot_scene`. Set in Project Settings.
    #[serde(default)]
    pub boot_scene: Option<String>,
    pub settings: ProjectSettings,
    /// Input control schemes (2C): rebindable in the editor and at runtime,
    /// shared by the editor Play mode and a shipped game.
    #[serde(default)]
    pub bindings: zestengine_core::Bindings,
    /// Audio buses (volumes / mutes), applied in the editor and a built game.
    #[serde(default)]
    pub audio_mixer: zestengine_core::AudioMixer,
    /// Steam Audio quality, reflections, HRTF and output layout.
    #[serde(default)]
    pub spatial_audio: zestengine_core::SpatialAudioSettings,
    /// OSC face-tracking receiver (off unless enabled).
    #[serde(default)]
    pub tracking_input: zestengine_core::TrackingInputSettings,
    /// Gravity, contact stiffness and the default physics material.
    #[serde(default)]
    pub physics: zestengine_core::PhysicsSettings,
    /// A built game starts in VR when a headset and OpenXR runtime are
    /// present. Off: it always opens a flat window (a Meta Quest build
    /// starts in VR regardless).
    #[serde(default)]
    pub vr: bool,
}

impl Default for ProjectFile {
    fn default() -> Self {
        Self {
            name: "zestengine project".into(),
            last_scene: None,
            boot_scene: None,
            settings: ProjectSettings::default(),
            bindings: zestengine_core::Bindings::default(),
            audio_mixer: zestengine_core::AudioMixer::default(),
            spatial_audio: zestengine_core::SpatialAudioSettings::default(),
            tracking_input: zestengine_core::TrackingInputSettings::default(),
            physics: zestengine_core::PhysicsSettings::default(),
            vr: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectSettings {
    pub vsync: bool,
    pub show_stats: bool,
    pub show_stats_overlay: bool,
    /// Gizmo grid snapping.
    pub snap: bool,
    pub grid_size: f32,
    /// Vim modal editing in the code editor.
    pub vim_mode: bool,
    /// Editor accent colour (sRGB), the only yellow in the UI; None = default.
    pub accent: Option<[u8; 3]>,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            vsync: true,
            show_stats: true,
            show_stats_overlay: false,
            snap: false,
            grid_size: 0.5,
            vim_mode: false,
            accent: None,
        }
    }
}

pub fn load_project_file(root: impl AsRef<Path>) -> Result<ProjectFile> {
    let path = root.as_ref().join(PROJECT_FILE_NAME);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_project_file(root: impl AsRef<Path>, project: &ProjectFile) -> Result<()> {
    let path = root.as_ref().join(PROJECT_FILE_NAME);
    let text = ron::ser::to_string_pretty(project, ron::ser::PrettyConfig::default())?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
