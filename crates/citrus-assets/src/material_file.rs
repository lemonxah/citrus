//! `.material` asset files (RON).
//!
//! A material file stores shader parameters, feature toggles, and
//! project-relative texture paths. The `shader` field selects which shader
//! the material targets: "standard", or the project-relative path of a
//! custom `.frag` shader (see shader_file.rs); custom property values are
//! stored by name in `custom`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use citrus_render::{MaterialFeatures, MaterialParams, TextureData};
use serde::{Deserialize, Serialize};

pub const MATERIAL_EXTENSION: &str = "material";

fn default_shader() -> String {
    "standard".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterialTextures {
    pub albedo: Option<PathBuf>,
    pub normal: Option<PathBuf>,
    pub orm: Option<PathBuf>,
    pub emission: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaterialFile {
    pub name: String,
    /// "standard" or a project-relative custom `.frag` shader path.
    #[serde(default = "default_shader")]
    pub shader: String,
    pub params: MaterialParams,
    pub features: MaterialFeatures,
    /// Draw-order priority (Unity render queue). `None` = derive from the
    /// alpha mode on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_queue: Option<i32>,
    #[serde(default)]
    pub textures: MaterialTextures,
    /// Custom-shader property values by property name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom: BTreeMap<String, Vec<f32>>,
}

pub fn load_material_file(path: impl AsRef<Path>) -> Result<MaterialFile> {
    let path = path.as_ref();
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let material: MaterialFile = ron::from_str(&text)
        .with_context(|| format!("parsing material file {}", path.display()))?;
    Ok(material)
}

pub fn save_material_file(path: impl AsRef<Path>, material: &MaterialFile) -> Result<()> {
    let path = path.as_ref();
    let text = ron::ser::to_string_pretty(material, ron::ser::PrettyConfig::default())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Load an image file from disk as RGBA8 texture data.
pub fn load_texture_file(path: impl AsRef<Path>, srgb: bool) -> Result<TextureData> {
    let path = path.as_ref();
    let img = image::open(path)
        .with_context(|| format!("loading texture {}", path.display()))?
        .into_rgba8();
    Ok(TextureData {
        width: img.width(),
        height: img.height(),
        pixels: img.into_raw(),
        srgb,
    })
}
