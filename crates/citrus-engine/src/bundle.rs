//! Project scaffolding (`New Project`) and game bundling (`Build Game`).
//!
//! A citrus project is a standalone cargo workspace: a game-binary package at
//! the root (`src/main.rs`) plus a `plugins/components` crate the editor
//! hot-loads and the game links statically. `scaffold_project` writes that
//! layout; `build_game` compiles it `--release` and copies the executable plus
//! the referenced assets into `build/`, ready to ship.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use citrus_assets::ProjectFile;

/// Asset subfolders every project gets.
const ASSET_DIRS: &[&str] = &["scenes", "materials", "shaders", "textures", "models", "audio", "prefabs"];

/// Top-level project folders that are code or build output, never game content.
const NON_ASSET_DIRS: &[&str] = &["build", "build-windows", "target", "src", "plugins"];

/// Which platform `build_game` produces an executable for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildTarget {
    /// The platform the editor runs on (output in `build/`).
    Host,
    /// 64-bit Windows (MSVC ABI). From Linux/macOS this cross-compiles with
    /// `cargo xwin`; output in `build-windows/`.
    Windows,
}

impl BuildTarget {
    pub const ALL: [BuildTarget; 2] = [BuildTarget::Host, BuildTarget::Windows];

    pub fn label(self) -> &'static str {
        match self {
            BuildTarget::Host if cfg!(windows) => "Windows (this machine)",
            BuildTarget::Host if cfg!(target_os = "macos") => "macOS (this machine)",
            BuildTarget::Host => "Linux (this machine)",
            BuildTarget::Windows => "Windows x64",
        }
    }

    /// Parse a CLI name (`host`, `windows`).
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "host" | "native" => Some(BuildTarget::Host),
            "windows" | "win" | "win64" | "x86_64-pc-windows-msvc" => Some(BuildTarget::Windows),
            _ => None,
        }
    }

    /// True when this target needs a cross toolchain from the current host.
    fn is_cross(self) -> bool {
        self == BuildTarget::Windows && !cfg!(windows)
    }

    fn is_windows(self) -> bool {
        match self {
            BuildTarget::Host => cfg!(windows),
            BuildTarget::Windows => true,
        }
    }

    fn output_dir(self) -> &'static str {
        if self.is_cross() { "build-windows" } else { "build" }
    }
}

const WINDOWS_TRIPLE: &str = "x86_64-pc-windows-msvc";

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

/// Create a fresh project under `parent/<name>` from a project template
/// (`crate::templates::TEMPLATES` id, e.g. "empty", "fps") and return its root
/// path. Errors if a project already exists there.
pub fn scaffold_project(parent: &Path, name: &str, template: &str) -> Result<PathBuf> {
    let root = parent.join(name);
    if root.join("project.citrus").exists() {
        bail!("a citrus project already exists at {}", root.display());
    }
    if crate::templates::find(template).is_none() {
        bail!(
            "unknown project template {template:?} (expected one of: {})",
            crate::templates::TEMPLATES.iter().map(|t| t.id).collect::<Vec<_>>().join(", ")
        );
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
citrus-engine = {{ path = "{citrus_disp}/crates/citrus-engine", default-features = false, features = ["steam-audio"] }}
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

    // Template content (scenes, materials, prefabs, sample assets) + project file.
    let (boot, bindings) =
        crate::templates::write_content(&root, Some(&citrus), template).context("writing template content")?;
    let project = ProjectFile {
        name: name.to_string(),
        last_scene: Some(boot.clone()),
        boot_scene: Some(boot),
        bindings,
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
    target: BuildTarget,
    mut log: impl FnMut(String),
) -> Result<PathBuf> {
    if !project_root.join("src/main.rs").is_file() || !project_root.join("Cargo.toml").is_file() {
        bail!(
            "this project has no game entry ({}/src/main.rs). Use File -> New Project to scaffold one.",
            project_root.display()
        );
    }
    let pkg = crate_name(&project.name);

    // `cargo rustc` passes the rpath to the game binary only (no dependency
    // rebuild), so it finds the Steam Audio library shipped next to it.
    // Windows loads DLLs from the executable's folder without one.
    let mut args: Vec<&str> = Vec::new();
    if target.is_cross() {
        check_xwin()?;
        args.extend(["xwin", "rustc", "--release", "--target", WINDOWS_TRIPLE]);
    } else {
        args.extend(["rustc", "--release"]);
    }
    args.extend(["--bin", &pkg]);
    if !target.is_windows() {
        if cfg!(target_os = "linux") {
            args.extend(["--", "-C", "link-arg=-Wl,-rpath,$ORIGIN"]);
        } else if cfg!(target_os = "macos") {
            args.extend(["--", "-C", "link-arg=-Wl,-rpath,@executable_path"]);
        }
    }
    log(format!("cargo build --release ({pkg}, {})…", target.label()));
    let output = std::process::Command::new("cargo")
        .current_dir(project_root)
        .args(&args)
        .output()
        .context("running cargo build")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("cargo build failed:\n{}", stderr.trim());
    }

    let exe_name = if target.is_windows() {
        format!("{pkg}.exe")
    } else {
        pkg.clone()
    };
    let profile_dir = if target.is_cross() {
        project_root.join("target").join(WINDOWS_TRIPLE).join("release")
    } else {
        project_root.join("target/release")
    };
    let built = profile_dir.join(&exe_name);
    if !built.is_file() {
        bail!("build succeeded but {} is missing", built.display());
    }

    let build_dir = project_root.join(target.output_dir());
    let assets_dir = build_dir.join("assets");
    // Fresh output each time so deleted assets don't linger.
    let _ = std::fs::remove_dir_all(&build_dir);
    std::fs::create_dir_all(&assets_dir).context("creating build/assets")?;

    log("copying executable…".into());
    let out_exe = build_dir.join(&exe_name);
    std::fs::copy(&built, &out_exe)
        .with_context(|| format!("copying {}", built.display()))?;
    if let Some(lib) = steam_audio_library(&profile_dir, target) {
        log("copying Steam Audio library…".into());
        let name = lib.file_name().expect("library file name");
        std::fs::copy(&lib, build_dir.join(name))
            .with_context(|| format!("copying {}", lib.display()))?;
        // Apache-2.0 requires the licence (and third-party notices) to ship with it.
        std::fs::write(build_dir.join("STEAM_AUDIO_LICENSE.txt"), STEAM_AUDIO_LICENSE)
            .context("writing the Steam Audio licence")?;
        if let Some(notices) = lib
            .parent()
            .and_then(|out_lib| out_lib.parent())
            .map(|out| out.join("steam_audio_cache/steamaudio_core/steamaudio/THIRDPARTY.md"))
            .filter(|p| p.is_file())
        {
            std::fs::copy(&notices, build_dir.join("STEAM_AUDIO_THIRDPARTY.md"))
                .with_context(|| format!("copying {}", notices.display()))?;
        }
    }

    log("cooking assets…".into());
    let cooked = cook_assets(project_root)?;
    for rel in &cooked.files {
        let (from, to) = (project_root.join(rel), assets_dir.join(rel));
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
    }
    for w in &cooked.warnings {
        log(format!("warning: {w}"));
    }
    log(format!(
        "cooked {} file(s), {:.1} MB (left out {} unreferenced file(s), {:.1} MB)",
        cooked.files.len(),
        cooked.bytes as f64 / 1e6,
        cooked.skipped,
        cooked.skipped_bytes as f64 / 1e6
    ));
    log("precompiling shaders…".into());
    precompile_shaders(&assets_dir, &mut log);
    let baked = crate::shaders::bake_project_variants(&assets_dir, &mut log);
    log(format!("baked {baked} shader keyword variant(s)"));
    // The game reads boot_scene/title from project.citrus at startup.
    std::fs::copy(
        project_root.join("project.citrus"),
        assets_dir.join("project.citrus"),
    )
    .context("copying project.citrus")?;

    log(format!("done: {}", out_exe.display()));
    Ok(out_exe)
}

/// Steam Audio's licence text, shipped next to `libphonon` in game builds.
const STEAM_AUDIO_LICENSE: &str = include_str!("../assets/licenses/steam-audio-LICENSE.txt");

/// The Steam Audio shared library built for this profile (newest
/// `build/audionimbus-sys-*/out/lib` copy), if the game uses it.
fn steam_audio_library(profile_dir: &Path, target: BuildTarget) -> Option<PathBuf> {
    let name = if target.is_windows() {
        "phonon.dll"
    } else if cfg!(target_os = "macos") {
        "libphonon.dylib"
    } else {
        "libphonon.so"
    };
    std::fs::read_dir(profile_dir.join("build"))
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("audionimbus-sys-"))
        .map(|e| e.path().join("out/lib").join(name))
        .filter(|p| p.is_file())
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}

/// Cross-compiling to Windows needs `cargo-xwin` (downloads the MSVC CRT and
/// Windows SDK on first use) and the rustup target.
fn check_xwin() -> Result<()> {
    let xwin = std::process::Command::new("cargo")
        .args(["xwin", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !xwin {
        bail!(
            "Windows builds from this platform need cargo-xwin: run `cargo install --locked cargo-xwin` \
             and `rustup target add {WINDOWS_TRIPLE}`"
        );
    }
    let targets = std::process::Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    if let Ok(out) = targets
        && out.status.success()
        && !String::from_utf8_lossy(&out.stdout).lines().any(|l| l.trim() == WINDOWS_TRIPLE)
    {
        bail!("the Rust {WINDOWS_TRIPLE} target is missing: run `rustup target add {WINDOWS_TRIPLE}`");
    }
    Ok(())
}

/// Compile every custom `.frag` under `dir` to a `.frag.spv` sidecar so the
/// shipped game never needs `glslc`. Failures are logged, not fatal (the game
/// then falls back to runtime compilation for that shader).
fn precompile_shaders(dir: &Path, log: &mut impl FnMut(String)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            precompile_shaders(&path, log);
        } else if citrus_assets::is_shader_path(&path)
            && let Err(e) = citrus_assets::precompile_shader_file(&path) {
                log(format!("warning: shader {} not precompiled: {e:#}", path.display()));
            }
    }
}

/// Every top-level content folder of a project: all subdirectories except
/// code / build output and hidden ones (`.git`, `.citrus` caches).
fn asset_dirs(project_root: &Path) -> Result<Vec<String>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(project_root)
        .with_context(|| format!("reading {}", project_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || NON_ASSET_DIRS.contains(&name.as_str()) {
            continue;
        }
        dirs.push(name);
    }
    dirs.sort();
    Ok(dirs)
}

/// Build a project given only its root: loads `project.citrus` and logs to
/// stdout. Convenience for the CLI (`citrus --build <dir>`).
pub fn build_project_dir(project_root: &Path, target: BuildTarget) -> Result<PathBuf> {
    let project =
        citrus_assets::load_project_file(project_root).context("reading project.citrus")?;
    build_game(project_root, &project, target, |m| println!("build: {m}"))
}

/// What a build ships: every file reachable from the project's scenes,
/// prefabs, its `resources/` folder and paths named in its code.
pub struct CookedAssets {
    pub files: Vec<String>,
    pub bytes: u64,
    pub skipped: usize,
    pub skipped_bytes: u64,
    pub warnings: Vec<String>,
}

/// Work out which content files a build needs. Roots: every `.scene` and
/// `.prefab`, everything under `resources/` (loaded by path at run time),
/// and any project file whose path appears as a string literal in `src/`.
/// Dependencies are followed through every RON asset, model sidecar and
/// external model file (the content-package scanner).
pub fn cook_assets(project_root: &Path) -> Result<CookedAssets> {
    let mut all = Vec::new();
    for dir in asset_dirs(project_root)? {
        list_files(project_root, &project_root.join(&dir), &mut all);
    }
    let mut roots: Vec<String> = all
        .iter()
        .filter(|r| r.ends_with(".scene") || r.ends_with(".prefab") || r.starts_with("resources/"))
        .cloned()
        .collect();
    let mut code = String::new();
    let mut src_files = Vec::new();
    list_files(project_root, &project_root.join("src"), &mut src_files);
    for f in src_files.iter().filter(|f| f.ends_with(".rs")) {
        code.push_str(&std::fs::read_to_string(project_root.join(f)).unwrap_or_default());
    }
    roots.extend(all.iter().filter(|r| code.contains(&format!("\"{r}\""))).cloned());
    let mut keep = std::collections::BTreeSet::new();
    let mut warnings = Vec::new();
    for root in roots {
        let text = std::fs::read_to_string(project_root.join(&root)).unwrap_or_default();
        let (deps, w) = citrus_assets::package::collect_dependencies(project_root, &root, &text);
        keep.extend(deps);
        keep.insert(root);
        warnings.extend(w.into_iter().filter(|w| !w.contains("not allowed in content packages")));
    }
    let size = |r: &String| std::fs::metadata(project_root.join(r)).map(|m| m.len()).unwrap_or(0);
    let files: Vec<String> = keep.into_iter().filter(|r| project_root.join(r).is_file()).collect();
    let skipped: Vec<&String> = all.iter().filter(|r| !files.contains(r)).collect();
    Ok(CookedAssets {
        bytes: files.iter().map(size).sum(),
        skipped: skipped.len(),
        skipped_bytes: skipped.iter().map(|r| size(r)).sum(),
        files,
        warnings,
    })
}

/// Project-relative paths of every file under `dir` (hidden entries skipped).
fn list_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            list_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod cook_tests {
    use super::*;

    #[test]
    fn cook_keeps_what_scenes_and_code_reach() {
        let root = std::env::temp_dir().join(format!("citrus-cook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        write("scenes/level.scene", r#"(entries: [(name: "Box", material: File("materials/wood.material"))])"#);
        write("materials/wood.material", r#"(textures: (albedo: Some("textures/wood.png")))"#);
        write("textures/wood.png", "png");
        write("textures/unused.png", "png-not-referenced");
        write("prefabs/enemy.prefab", r#"(entries: [])"#);
        write("audio/boom.wav", "wav");
        write("audio/old.wav", "old");
        write("resources/config.ron", "()");
        write("src/lib.rs", r#"fn f() { play("audio/boom.wav"); }"#);
        let cooked = cook_assets(&root).unwrap();
        for f in ["scenes/level.scene", "materials/wood.material", "textures/wood.png", "prefabs/enemy.prefab", "audio/boom.wav", "resources/config.ron"] {
            assert!(cooked.files.iter().any(|x| x == f), "missing {f}: {:?}", cooked.files);
        }
        assert!(!cooked.files.iter().any(|x| x == "textures/unused.png" || x == "audio/old.wav"));
        assert_eq!(cooked.skipped, 2);
        let _ = std::fs::remove_dir_all(&root);
    }
}

