//! Project scaffolding (`New Project`) and game bundling (`Build Game`).
//!
//! A citrus project is a standalone cargo workspace: a game-binary package at
//! the root (`src/main.rs`) plus a `plugins/components` crate the editor
//! hot-loads and the game links statically. `scaffold_project` writes that
//! layout; `build_game` compiles it `--release` and copies the executable plus
//! the referenced assets into `build/`, ready to ship.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use citrus_assets::{MaterialRef, ObjectSource, PrimitiveShape, ProjectFile, SceneEntry, SceneFile};
use citrus_render::{MaterialFeatures, MaterialParams};

/// Asset subfolders every project gets.
const ASSET_DIRS: &[&str] = &["scenes", "materials", "shaders", "textures"];

/// Locate the citrus checkout so generated projects can path-depend on its
/// crates. Walks up from the running editor's executable looking for the
/// workspace that contains `crates/citrus-engine`; falls back to `$CITRUS_ROOT`.
pub fn citrus_root() -> Result<PathBuf> {
    if let Ok(root) = std::env::var("CITRUS_ROOT") {
        let root = PathBuf::from(root);
        if root.join("crates/citrus-engine/Cargo.toml").is_file() {
            return Ok(root);
        }
    }
    let exe = std::env::current_exe().context("locating editor executable")?;
    let mut dir = exe.parent();
    while let Some(d) = dir {
        if d.join("crates/citrus-engine/Cargo.toml").is_file() {
            return Ok(d.to_path_buf());
        }
        dir = d.parent();
    }
    bail!(
        "could not find the citrus checkout (looked above {}); set $CITRUS_ROOT",
        exe.display()
    )
}

/// A cargo-legal crate name derived from a project name.
fn crate_name(project_name: &str) -> String {
    let mut s: String = project_name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s = s.trim_matches('-').to_string();
    if s.is_empty() || s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s = format!("game-{s}");
    }
    s
}

fn inline_material() -> MaterialRef {
    MaterialRef::Inline {
        params: MaterialParams::default(),
        features: MaterialFeatures::default(),
        shader: "standard".into(),
        custom: Default::default(),
        render_queue: None,
    }
}

fn entry(name: &str, source: ObjectSource, translation: [f32; 3], scale: [f32; 3]) -> SceneEntry {
    SceneEntry {
        id: String::new(),
        name: name.into(),
        source,
        enabled: true,
        static_geometry: false,
        lightmap_scale: 1.0,
        material: inline_material(),
        parent: None,
        components: Vec::new(),
        translation,
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale,
    }
}

/// A minimal but non-empty starter scene: a camera looking at a lit cube on a
/// ground plane (the world sun is on by default, so it renders immediately).
fn starter_scene() -> SceneFile {
    SceneFile {
        entries: vec![
            entry("Main Camera", ObjectSource::Camera, [0.0, 1.5, 5.0], [1.0; 3]),
            entry(
                "Ground",
                ObjectSource::Primitive {
                    shape: PrimitiveShape::Plane,
                },
                [0.0, 0.0, 0.0],
                [10.0, 1.0, 10.0],
            ),
            entry(
                "Cube",
                ObjectSource::Primitive {
                    shape: PrimitiveShape::Cube,
                },
                [0.0, 0.5, 0.0],
                [1.0; 3],
            ),
        ],
        skybox: None,
        environment: Default::default(),
    }
}

/// Create a fresh project under `parent/<name>` and return its root path.
/// Errors if the folder already exists and is non-empty.
pub fn scaffold_project(parent: &Path, name: &str) -> Result<PathBuf> {
    let root = parent.join(name);
    if root.join("project.citrus").exists() {
        bail!("a citrus project already exists at {}", root.display());
    }
    let citrus = citrus_root()?;
    let pkg = crate_name(name);

    for dir in ASSET_DIRS {
        std::fs::create_dir_all(root.join(dir))
            .with_context(|| format!("creating {dir}/"))?;
    }
    std::fs::create_dir_all(root.join("src"))?;

    // Root Cargo.toml: the game binary + a workspace whose dependency table the
    // plugins/components crate inherits (`citrus-*.workspace = true`).
    let citrus_disp = citrus.display();
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{pkg}"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "{pkg}"
path = "src/main.rs"

# Standalone workspace: the citrus crates are path-depended from your local
# checkout. Move/clone the engine elsewhere and update these paths.
[workspace]
members = ["plugins/components"]

[workspace.dependencies]
citrus-core = {{ path = "{citrus_disp}/crates/citrus-core" }}
citrus-editor = {{ path = "{citrus_disp}/crates/citrus-editor" }}
# Runtime only: default-features = false drops the editor (no citrus-editor,
# egui_dock, gizmos, LSP) from the shipped game. It must be set HERE — a member
# inheriting with `workspace = true` can't override default-features.
citrus-engine = {{ path = "{citrus_disp}/crates/citrus-engine", default-features = false }}
egui = "0.33"
serde = {{ version = "1", features = ["derive"] }}
glam = "0.30"

[dependencies]
citrus-engine.workspace = true
# The component crate, linked statically (no editor feature -> no editor/egui).
citrus-project-components = {{ path = "plugins/components" }}
anyhow = "1"

# Lean shipping binary: link-time optimization, one codegen unit, stripped
# symbols, and abort-on-panic (no unwinding tables). ~40% smaller than the
# default release profile.
[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
panic = "abort"
"#
        ),
    )
    .context("writing Cargo.toml")?;

    // Editable game entry point: reads the boot scene from project.citrus and
    // registers this project's components. Edit for custom startup.
    std::fs::write(
        root.join("src/main.rs"),
        format!(
            r#"//! Game entry point. The boot scene comes from project.citrus
//! (Project Settings -> Starting scene); edit this file for custom startup.

use anyhow::Result;

fn main() -> Result<()> {{
    citrus_engine::init_logging();

    // Assets ship next to the executable (build/assets); for `cargo run` during
    // development they live in the project folder.
    let exe_assets = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("assets")))
        .filter(|p| p.join("project.citrus").is_file());
    let assets_root = exe_assets
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    let config = citrus_engine::GameConfig::from_project_dir(assets_root)?;
    citrus_engine::run_game(config, |registry| {{
        {pkg_us}_components::citrus_register(registry);
    }})
}}
"#,
            pkg_us = "citrus_project"
        ),
    )
    .context("writing src/main.rs")?;

    // The component crate (cdylib for editor hot-load + rlib for the game).
    crate::plugins::create_template(&root).context("scaffolding plugins/components")?;
    // create_template defaults to cdylib only; a bundled game links it as rlib.
    add_rlib_crate_type(&root.join("plugins/components/Cargo.toml"))?;

    // Starter scene + project file.
    citrus_assets::save_scene_file(root.join("scenes/main.scene"), &starter_scene())
        .context("writing starter scene")?;
    let project = ProjectFile {
        name: name.to_string(),
        last_scene: Some("scenes/main.scene".into()),
        boot_scene: Some("scenes/main.scene".into()),
        ..Default::default()
    };
    citrus_assets::save_project_file(&root, &project).context("writing project.citrus")?;

    Ok(root)
}

/// Ensure the component crate also produces an `rlib` (for static game linking),
/// not just the editor's `cdylib`.
fn add_rlib_crate_type(manifest: &Path) -> Result<()> {
    let text = std::fs::read_to_string(manifest).context("reading components Cargo.toml")?;
    if text.contains("\"rlib\"") {
        return Ok(());
    }
    let patched = text.replace(
        r#"crate-type = ["cdylib"]"#,
        r#"crate-type = ["cdylib", "rlib"]"#,
    );
    std::fs::write(manifest, patched).context("patching components crate-type")?;
    Ok(())
}

/// Compile the project `--release` and assemble a self-contained `build/`
/// folder: the executable plus a `build/assets/` copy of the referenced asset
/// dirs and `project.citrus`. Returns the path of the produced executable.
/// `log` receives human-readable progress lines.
pub fn build_game(
    project_root: &Path,
    project: &ProjectFile,
    mut log: impl FnMut(String),
) -> Result<PathBuf> {
    if !project_root.join("src/main.rs").is_file() || !project_root.join("Cargo.toml").is_file() {
        bail!(
            "this project has no game entry ({}/src/main.rs). Use File -> New Project to scaffold one.",
            project_root.display()
        );
    }
    let pkg = crate_name(&project.name);

    log(format!("cargo build --release ({pkg})…"));
    let output = std::process::Command::new("cargo")
        .current_dir(project_root)
        .args(["build", "--release", "--bin", &pkg])
        .output()
        .context("running cargo build")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("cargo build failed:\n{}", stderr.trim());
    }

    let exe_name = if cfg!(windows) {
        format!("{pkg}.exe")
    } else {
        pkg.clone()
    };
    let built = project_root.join("target/release").join(&exe_name);
    if !built.is_file() {
        bail!("build succeeded but {} is missing", built.display());
    }

    let build_dir = project_root.join("build");
    let assets_dir = build_dir.join("assets");
    // Fresh output each time so deleted assets don't linger.
    let _ = std::fs::remove_dir_all(&build_dir);
    std::fs::create_dir_all(&assets_dir).context("creating build/assets")?;

    log("copying executable…".into());
    let out_exe = build_dir.join(&exe_name);
    std::fs::copy(&built, &out_exe)
        .with_context(|| format!("copying {}", built.display()))?;

    log("copying assets…".into());
    for dir in ASSET_DIRS {
        let src = project_root.join(dir);
        if src.is_dir() {
            copy_dir(&src, &assets_dir.join(dir))?;
        }
    }
    // The game reads boot_scene/title from project.citrus at startup.
    std::fs::copy(
        project_root.join("project.citrus"),
        assets_dir.join("project.citrus"),
    )
    .context("copying project.citrus")?;

    log(format!("done: {}", out_exe.display()));
    Ok(out_exe)
}

/// Build a project given only its root: loads `project.citrus` and logs to
/// stdout. Convenience for the CLI (`citrus --build <dir>`).
pub fn build_project_dir(project_root: &Path) -> Result<PathBuf> {
    let project =
        citrus_assets::load_project_file(project_root).context("reading project.citrus")?;
    build_game(project_root, &project, |m| println!("build: {m}"))
}

/// Recursive directory copy (files + subdirs).
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}
