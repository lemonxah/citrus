//! New Project template catalogue: the id, name, description and picture of
//! every starter project. Metadata only, so the Project Hub can list templates
//! without linking the engine; the content each template writes lives in
//! `citrus_engine::templates`.

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

/// Picture shown on a template card (mapped to the editor icon set by the UI).
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
