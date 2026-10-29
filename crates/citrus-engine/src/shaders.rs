//! Runtime custom-shader library: loads `.frag` files from the project,
//! compiles them via glslc, registers the SPIR-V with the renderer, and
//! exposes reflected property metadata for the inspector. Polls file mtimes
//! for hot reload.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use citrus_assets::{ShaderPropKind, ShaderSource};
use citrus_core::{ShaderPropKindUi, ShaderPropUi, ShaderUiInfo};
use citrus_render::{Renderer, ShaderId};

/// A shader parsed + compiled on a worker thread (or the error).
pub type PrecompiledShader = Result<(ShaderSource, Vec<u8>), String>;

/// Compile every custom shader a scene's materials reference (worker side).
pub fn precompile_scene_shaders(file: &citrus_assets::SceneFile, project_root: &Path) -> Vec<(String, PrecompiledShader)> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        if name.ends_with(".frag") && !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    };
    for entry in &file.entries {
        for m in std::iter::once(&entry.material).chain(entry.extra_materials.iter()) {
            match m {
                citrus_assets::MaterialRef::Inline { shader, .. } => add(shader),
                citrus_assets::MaterialRef::File(path) => {
                    if let Ok(mf) = citrus_assets::load_material_file(project_root.join(path)) {
                        add(&mf.shader);
                    }
                }
            }
        }
    }
    names
        .into_iter()
        .map(|rel| {
            let compiled = citrus_assets::load_shader_file(project_root.join(&rel)).map_err(|e| format!("{e:#}"));
            (rel, compiled)
        })
        .collect()
}

pub struct ShaderEntry {
    /// Parsed source (None when the file couldn't be read/parsed/compiled).
    pub source: Option<ShaderSource>,
    /// Inspector view (display name, properties, error).
    pub ui: ShaderUiInfo,
    /// Renderer registration; None while broken.
    pub id: Option<ShaderId>,
    mtime: Option<SystemTime>,
}

impl ShaderEntry {
    /// Defaults for `MaterialModel::custom_values` (zeros while broken, so
    /// values survive a broken intermediate save during hot editing).
    pub fn defaults(&self) -> Vec<f32> {
        self.source
            .as_ref()
            .map(|s| s.defaults().to_vec())
            .unwrap_or_else(|| vec![0.0; citrus_assets::SHADER_PROP_FLOATS])
    }
}

/// Project-relative `.frag` path → compiled entry.
#[derive(Default)]
pub struct ShaderLibrary {
    entries: HashMap<String, ShaderEntry>,
}

impl ShaderLibrary {
    /// Get a shader, loading and compiling it on first use.
    pub fn resolve(
        &mut self,
        renderer: &mut Renderer,
        project_root: &Path,
        rel: &str,
    ) -> &ShaderEntry {
        if !self.entries.contains_key(rel) {
            let entry = load_entry(renderer, project_root, rel);
            self.entries.insert(rel.to_owned(), entry);
        }
        &self.entries[rel]
    }

    /// Install a shader compiled off the main thread (scene loader worker), so
    /// `resolve` finds it without running `glslc` on the main thread.
    pub fn preload(&mut self, renderer: &mut Renderer, project_root: &Path, rel: &str, compiled: PrecompiledShader) {
        if self.entries.contains_key(rel) {
            return;
        }
        let mtime = std::fs::metadata(project_root.join(rel)).and_then(|m| m.modified()).ok();
        let result = compiled.and_then(|(source, spirv)| {
            renderer
                .register_shader(&spirv)
                .map(|id| (source, id))
                .map_err(|e| format!("{e:#}"))
        });
        let entry = match result {
            Ok((source, id)) => ShaderEntry {
                ui: ui_from_source(&source),
                source: Some(source),
                id: Some(id),
                mtime,
            },
            Err(e) => ShaderEntry {
                source: None,
                ui: ShaderUiInfo {
                    display_name: rel.to_owned(),
                    props: Vec::new(),
                    error: Some(e),
                },
                id: None,
                mtime,
            },
        };
        self.entries.insert(rel.to_owned(), entry);
    }

    /// Already-loaded shader (no compile side effects); used on save paths.
    pub fn get(&self, rel: &str) -> Option<&ShaderEntry> {
        self.entries.get(rel)
    }

    /// Re-check mtimes; recompile changed files. Returns the shader paths
    /// that changed (their materials need re-applying).
    pub fn poll_reload(&mut self, renderer: &mut Renderer, project_root: &Path) -> Vec<String> {
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(rel, entry)| {
                let mtime = std::fs::metadata(project_root.join(rel))
                    .and_then(|m| m.modified())
                    .ok();
                mtime != entry.mtime
            })
            .map(|(rel, _)| rel.clone())
            .collect();
        for rel in &stale {
            tracing::info!("reloading shader {rel}");
            let entry = load_entry(renderer, project_root, rel);
            self.entries.insert(rel.clone(), entry);
        }
        stale
    }
}

fn load_entry(renderer: &mut Renderer, project_root: &Path, rel: &str) -> ShaderEntry {
    let abs = project_root.join(rel);
    let mtime = std::fs::metadata(&abs).and_then(|m| m.modified()).ok();
    match citrus_assets::load_shader_file(&abs)
        .and_then(|(source, spirv)| Ok((source, renderer.register_shader(&spirv)?)))
    {
        Ok((source, id)) => ShaderEntry {
            ui: ui_from_source(&source),
            source: Some(source),
            id: Some(id),
            mtime,
        },
        Err(e) => {
            tracing::error!("shader {rel}: {e:#}");
            ShaderEntry {
                source: None,
                ui: ShaderUiInfo {
                    display_name: rel.to_owned(),
                    props: Vec::new(),
                    error: Some(format!("{e:#}")),
                },
                id: None,
                mtime,
            }
        }
    }
}

fn ui_from_source(source: &ShaderSource) -> ShaderUiInfo {
    ShaderUiInfo {
        display_name: source.display_name.clone(),
        props: source
            .props
            .iter()
            .map(|p| ShaderPropUi {
                label: p.label.clone(),
                kind: match p.kind {
                    ShaderPropKind::Float { min, max } => ShaderPropKindUi::Float { min, max },
                    ShaderPropKind::Toggle => ShaderPropKindUi::Toggle,
                    ShaderPropKind::Color => ShaderPropKindUi::Color,
                    ShaderPropKind::Color3 => ShaderPropKindUi::Color3,
                },
                offset: p.offset,
            })
            .collect(),
        error: None,
    }
}
