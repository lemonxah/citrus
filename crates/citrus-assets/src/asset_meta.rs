//! Per-asset `.meta` sidecar files (Unity/Godot-style). Each imported asset
//! `foo.fbx` gets a companion `foo.fbx.meta` (RON) holding a stable asset
//! GUID (so references survive renames/moves) plus that asset's importer
//! settings. Living next to the asset means the data travels with it in
//! version control and scales to thousands of assets without a central hot file.
//!
//! `#[serde(default)]` everywhere so old `.meta` files keep loading as the
//! importer settings grow.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use citrus_core::ObjectId;
use serde::{Deserialize, Serialize};

/// Extension appended to the full asset filename: `mesh.fbx` -> `mesh.fbx.meta`.
pub const META_EXT: &str = "meta";

/// One asset's sidecar metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AssetMeta {
    /// Stable, path-independent asset identity. Scenes/materials can reference an
    /// asset by this id so moving or renaming the file doesn't break links.
    pub id: ObjectId,
    /// Per-importer settings for this asset's type.
    pub importer: ImporterSettings,
}

impl Default for AssetMeta {
    fn default() -> Self {
        Self {
            id: ObjectId::new(),
            importer: ImporterSettings::Generic,
        }
    }
}

impl AssetMeta {
    /// A fresh meta whose importer settings default to the right type for the
    /// asset's extension (models get model-import defaults, etc.).
    pub fn new_for(asset: &Path) -> Self {
        Self {
            id: ObjectId::new(),
            importer: ImporterSettings::for_extension(asset),
        }
    }
}

/// Settings keyed by asset kind. Extend with new variants as importers gain
/// options (textures: sRGB/mips; audio: streaming; etc.).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ImporterSettings {
    Generic,
    Model(ModelImport),
}

impl Default for ImporterSettings {
    fn default() -> Self {
        Self::Generic
    }
}

impl ImporterSettings {
    /// Default settings variant for an asset path's extension.
    pub fn for_extension(asset: &Path) -> Self {
        match asset
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("fbx" | "glb" | "gltf" | "obj") => Self::Model(ModelImport::default()),
            _ => Self::Generic,
        }
    }

    /// The model-import settings if this is a model asset.
    pub fn as_model(&self) -> Option<&ModelImport> {
        match self {
            Self::Model(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_model_mut(&mut self) -> Option<&mut ModelImport> {
        match self {
            Self::Model(m) => Some(m),
            _ => None,
        }
    }
}

/// Import options shared by mesh formats (FBX / glTF / OBJ).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelImport {
    /// Uniform scale applied to imported geometry (e.g. 0.01 for cm→m sources).
    pub scale: f32,
    /// Import the file's materials (off = assign the default material).
    pub import_materials: bool,
    /// Flip the V texture coordinate (DirectX vs OpenGL convention mismatch).
    pub flip_uv: bool,
    /// Recompute smooth normals instead of using the file's.
    pub recalculate_normals: bool,
    /// Crease angle (degrees) for recomputed normals.
    pub smoothing_angle: f32,
}

impl Default for ModelImport {
    fn default() -> Self {
        Self {
            scale: 1.0,
            import_materials: true,
            flip_uv: false,
            recalculate_normals: false,
            smoothing_angle: 60.0,
        }
    }
}

/// The `.meta` path for an asset (`mesh.fbx` -> `mesh.fbx.meta`).
pub fn meta_path(asset: impl AsRef<Path>) -> PathBuf {
    let mut s = asset.as_ref().as_os_str().to_os_string();
    s.push(".");
    s.push(META_EXT);
    PathBuf::from(s)
}

/// Load an asset's `.meta`, or `None` if it doesn't exist yet.
pub fn load_asset_meta(asset: impl AsRef<Path>) -> Result<Option<AssetMeta>> {
    let path = meta_path(&asset);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let meta = ron::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(meta))
}

/// Load an asset's `.meta`, creating + writing a default (with a fresh GUID and
/// type-appropriate importer settings) if it's missing. Mirrors how Unity
/// auto-generates a `.meta` the first time it sees an asset.
pub fn load_or_create_asset_meta(asset: impl AsRef<Path>) -> Result<AssetMeta> {
    if let Some(meta) = load_asset_meta(&asset)? {
        return Ok(meta);
    }
    let meta = AssetMeta::new_for(asset.as_ref());
    save_asset_meta(&asset, &meta)?;
    Ok(meta)
}

/// Write an asset's `.meta` sidecar.
pub fn save_asset_meta(asset: impl AsRef<Path>, meta: &AssetMeta) -> Result<()> {
    let path = meta_path(&asset);
    let text = ron::ser::to_string_pretty(meta, ron::ser::PrettyConfig::default())?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fbx_meta_defaults_to_model_import() {
        let meta = AssetMeta::new_for(Path::new("/x/mesh.FBX"));
        assert!(meta.importer.as_model().is_some(), "fbx → model importer");
        assert!(!meta.id.is_nil(), "fresh meta gets a real GUID");
    }

    #[test]
    fn png_meta_is_generic() {
        let meta = AssetMeta::new_for(Path::new("/x/tex.png"));
        assert!(matches!(meta.importer, ImporterSettings::Generic));
    }

    #[test]
    fn meta_path_appends_extension() {
        assert_eq!(meta_path("a/b/mesh.fbx"), PathBuf::from("a/b/mesh.fbx.meta"));
    }

    #[test]
    fn round_trips_ron() {
        let mut meta = AssetMeta::new_for(Path::new("m.fbx"));
        if let Some(m) = meta.importer.as_model_mut() {
            m.scale = 0.01;
            m.flip_uv = true;
        }
        let s = ron::ser::to_string_pretty(&meta, Default::default()).unwrap();
        let back: AssetMeta = ron::from_str(&s).unwrap();
        assert_eq!(back.id, meta.id);
        assert_eq!(back.importer.as_model().unwrap().scale, 0.01);
    }
}
