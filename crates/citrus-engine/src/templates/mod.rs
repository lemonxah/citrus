//! Project templates for New Project: a registry of starter games (empty,
//! FPS, TPS, top-down click-to-move, strategy, VR shooter, VR social) and the
//! content each writes into a fresh project: scenes, materials, prefabs,
//! input bindings and copied sample assets. Every template is built from the
//! built-in gameplay kit (`citrus_core::kit`) and plays in the editor as is.

mod builder;
mod fps;
mod strategy;
mod topdown;
mod tps;
mod vr_shooter;
mod vr_social;

use std::path::Path;

use anyhow::{Context as _, Result};
use citrus_assets::{MaterialFile, SceneFile};
use citrus_core::Bindings;

use crate::prefab::Prefab;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TemplateCategory {
    Desktop,
    Vr,
}

impl TemplateCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "Desktop",
            Self::Vr => "VR",
        }
    }
}

/// Picture shown on a template card (mapped to the editor icon set).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TemplateIcon {
    Empty,
    Crosshair,
    Camera,
    Pointer,
    Grid,
    Headset,
    Social,
}

#[cfg(feature = "editor")]
impl TemplateIcon {
    pub fn editor_icon(self) -> citrus_editor::icons::Icon {
        use citrus_editor::icons::Icon;
        match self {
            Self::Empty => Icon::Plus,
            Self::Crosshair => Icon::Target,
            Self::Camera => Icon::Camera,
            Self::Pointer => Icon::ToolSelect,
            Self::Grid => Icon::Grid,
            Self::Headset => Icon::Gizmo,
            Self::Social => Icon::Audio,
        }
    }
}

/// One entry in the New Project template picker.
#[derive(Clone, Copy, Debug)]
pub struct ProjectTemplate {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub category: TemplateCategory,
    pub icon: TemplateIcon,
    /// Short feature list for the picker's detail line.
    pub features: &'static [&'static str],
}

pub const DEFAULT_TEMPLATE: &str = "empty";

pub const TEMPLATES: &[ProjectTemplate] = &[
    ProjectTemplate {
        id: "empty",
        name: "Empty",
        description: "A camera, a cube and a ground plane. Start from scratch.",
        category: TemplateCategory::Desktop,
        icon: TemplateIcon::Empty,
        features: &["Starter scene", "Default input bindings"],
    },
    ProjectTemplate {
        id: "fps",
        name: "First Person Shooter",
        description: "Mouse-look FPS controller, hitscan rifle, health, target dummies, HUD and pause menu.",
        category: TemplateCategory::Desktop,
        icon: TemplateIcon::Crosshair,
        features: &["WASD + mouse look, jump, sprint, crouch", "Hitscan weapon with reload", "Target range", "Crosshair, ammo and health HUD"],
    },
    ProjectTemplate {
        id: "tps",
        name: "Third Person Shooter",
        description: "Over-the-shoulder character with a collision-aware camera boom, aim mode, weapon and HUD.",
        category: TemplateCategory::Desktop,
        icon: TemplateIcon::Camera,
        features: &["Orbit camera that avoids walls", "Aim (right mouse) zooms over the shoulder", "Hitscan weapon, health, targets"],
    },
    ProjectTemplate {
        id: "topdown",
        name: "Top-Down Click to Move",
        description: "Isometric follow camera; click the ground to walk over a baked navmesh, click switches to use them.",
        category: TemplateCategory::Desktop,
        icon: TemplateIcon::Pointer,
        features: &["NavMesh Surface + Nav Agent", "Click marker", "Interactable switch and lamp", "Wheel zoom"],
    },
    ProjectTemplate {
        id: "strategy",
        name: "Strategy (RTS)",
        description: "RTS camera with edge pan, zoom and rotate; box-select units and send them in formation.",
        category: TemplateCategory::Desktop,
        icon: TemplateIcon::Grid,
        features: &["Edge / WASD pan, wheel zoom, Q/E rotate", "Box and click selection", "Right-click formation moves", "Resource HUD"],
    },
    ProjectTemplate {
        id: "vr_shooter",
        name: "VR Shooter",
        description: "VR rig with hands, a grabbable pistol (trigger fires), teleport and smooth locomotion, targets, wrist HUD.",
        category: TemplateCategory::Vr,
        icon: TemplateIcon::Headset,
        features: &["Head + hand tracking", "Teleport arc and snap turn", "Grab + trigger weapons", "Desktop fallback in the editor"],
    },
    ProjectTemplate {
        id: "vr_social",
        name: "VR Social",
        description: "Hangout room: avatar body, mirror, world-space welcome panel, player spawn points and synced grabbables.",
        category: TemplateCategory::Vr,
        icon: TemplateIcon::Social,
        features: &["Avatar that follows your head", "Planar reflection mirror", "Spawn points + Sync objects for multiplayer", "Push-to-talk voice binding"],
    },
];

pub fn find(id: &str) -> Option<&'static ProjectTemplate> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// Everything a template writes into a project.
pub(crate) struct TemplateContent {
    /// Project-relative path -> scene.
    pub scenes: Vec<(String, SceneFile)>,
    pub materials: Vec<(String, MaterialFile)>,
    pub prefabs: Vec<(String, Prefab)>,
    pub bindings: Bindings,
    /// Repo-relative sample assets copied into the same relative path.
    pub assets: Vec<&'static str>,
    pub boot_scene: String,
}

pub(crate) fn build(id: &str) -> Result<TemplateContent> {
    let content = match id {
        "empty" => builder::empty(),
        "fps" => fps::build(),
        "tps" => tps::build(),
        "topdown" => topdown::build(),
        "strategy" => strategy::build(),
        "vr_shooter" => vr_shooter::build(),
        "vr_social" => vr_social::build(),
        other => anyhow::bail!(
            "unknown project template {other:?} (expected one of: {})",
            TEMPLATES.iter().map(|t| t.id).collect::<Vec<_>>().join(", ")
        ),
    };
    Ok(content)
}

/// Write a template's content into `root` (which already has its asset
/// folders). `citrus` is the engine checkout to copy sample assets from; a
/// missing asset is skipped and any scene skybox using it is cleared.
/// Returns the boot scene and bindings for project.citrus.
pub(crate) fn write_content(root: &Path, citrus: Option<&Path>, id: &str) -> Result<(String, Bindings)> {
    let mut content = build(id)?;
    let mut missing = Vec::new();
    for rel in &content.assets {
        let copied = citrus.map(|c| c.join(rel)).filter(|src| src.is_file()).map(|src| {
            let dst = root.join(rel);
            if let Some(dir) = dst.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))
        });
        match copied {
            Some(r) => {
                r?;
            }
            None => {
                tracing::warn!("template asset {rel} not found; skipped");
                missing.push(rel.to_string());
            }
        }
    }
    for (rel, scene) in &mut content.scenes {
        if scene.skybox.as_ref().is_some_and(|s| missing.contains(s)) {
            scene.skybox = None;
        }
        citrus_assets::save_scene_file(root.join(&*rel), scene).with_context(|| format!("writing {rel}"))?;
    }
    for (rel, material) in &content.materials {
        citrus_assets::save_material_file(root.join(rel), material).with_context(|| format!("writing {rel}"))?;
    }
    for (rel, prefab) in &content.prefabs {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        prefab.save(&path).with_context(|| format!("writing {rel}"))?;
    }
    Ok((content.boot_scene, content.bindings))
}

#[cfg(test)]
mod tests;
