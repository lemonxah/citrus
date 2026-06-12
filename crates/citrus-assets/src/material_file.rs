//! `.material` asset files (RON).
//!
//! A material file stores standard-shader parameters, feature toggles, and
//! project-relative texture paths. The `shader` field selects which shader
//! the material targets — only "standard" exists today; custom shaders with
//! reflected inspector sections are on the roadmap (TODO.md).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use citrus_render::{MaterialFeatures, MaterialParams, TextureData};

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
    /// Shader this material targets. Currently always "standard".
    #[serde(default = "default_shader")]
    pub shader: String,
    pub params: MaterialParams,
    pub features: MaterialFeatures,
    #[serde(default)]
    pub textures: MaterialTextures,
}

pub fn load_material_file(path: impl AsRef<Path>) -> Result<MaterialFile> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let material: MaterialFile = ron::from_str(&text)
        .with_context(|| format!("parsing material file {}", path.display()))?;
    if material.shader != "standard" {
        bail!(
            "material {} uses shader {:?}; only \"standard\" is supported (custom shaders are on the roadmap)",
            path.display(),
            material.shader
        );
    }
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
