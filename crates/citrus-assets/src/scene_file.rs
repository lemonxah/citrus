//! `.scene` asset files (RON): a list of placed objects with sources and
//! material references, so multiple scenes can be saved and reloaded.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use citrus_render::{MaterialFeatures, MaterialParams};

pub const SCENE_EXTENSION: &str = "scene";

/// Where an object's mesh came from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ObjectSource {
    /// A model file in the project; `mesh` is the flattened primitive index
    /// produced by the importer (stable for an unchanged file).
    Model { path: String, mesh: usize },
    /// A built-in test-scene mesh (sphere/cube/plane by index).
    Builtin { mesh: usize },
    /// A generated primitive shape.
    Primitive { shape: PrimitiveShape },
    /// A transform-only grouping node.
    Empty,
    /// A camera placement.
    Camera,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrimitiveShape {
    Cube,
    Sphere,
    Capsule,
    Plane,
}

impl PrimitiveShape {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cube => "Cube",
            Self::Sphere => "Sphere",
            Self::Capsule => "Capsule",
            Self::Plane => "Plane",
        }
    }
}

/// Which material an object uses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MaterialRef {
    /// A `.material` asset file (project-relative path).
    File(String),
    /// Snapshot of parameters applied over whatever textures the object's
    /// imported material carried.
    Inline {
        params: MaterialParams,
        features: MaterialFeatures,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SceneEntry {
    pub name: String,
    pub source: ObjectSource,
    pub material: MaterialRef,
    /// Index of the parent entry in this file, if any. Transforms are local
    /// to the parent.
    #[serde(default)]
    pub parent: Option<usize>,
    pub translation: [f32; 3],
    /// Quaternion xyzw.
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SceneFile {
    pub entries: Vec<SceneEntry>,
}

pub fn load_scene_file(path: impl AsRef<Path>) -> Result<SceneFile> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("parsing scene file {}", path.display()))
}

pub fn save_scene_file(path: impl AsRef<Path>, scene: &SceneFile) -> Result<()> {
    let path = path.as_ref();
    let text = ron::ser::to_string_pretty(scene, ron::ser::PrettyConfig::default())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
