#![allow(dead_code)]
//! Rust component plugins: crates under `plugins/`, built by the editor
//! (`cargo build -p <name>`) and hot-loaded as dylibs.
//!
//! Plugins are workspace members, so their dependency versions are pinned by
//! the same Cargo.lock as the editor. That (plus the same rustc) is what
//! makes sharing `dyn Component` across the dylib boundary safe in practice.
//! Each plugin exports `citrus_register(&mut ComponentRegistry)`.
//!
//! Loaded libraries are NEVER unloaded: scene objects can hold component
//! instances whose code lives in an older build. Reloading registers the new
//! build's types over the old names and the engine re-instantiates every
//! object's components from serialized state.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use anyhow::{Context as _, Result, bail};
use citrus_core::ComponentRegistry;
#[cfg(feature = "editor")]
use citrus_editor::{CodeDiagnostic, EditorComponents};

#[cfg(feature = "editor")]
pub fn check_shader(path: &Path) -> Vec<CodeDiagnostic> {
    let output = Command::new("glslc").arg("-o").arg("-").arg(path).output();

    let mut diags = Vec::new();
    match output {
        Ok(o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            for (i, line) in stderr.lines().enumerate() {
                if line.contains("error:") || line.contains("ERROR:") {
                    diags.push(CodeDiagnostic {
                        level: "error".into(),
                        file: path.to_string_lossy().into_owned(),
                        line: (i + 1) as u32,
                        message: line.trim().to_owned(),
                    });
                } else if line.contains("warning:") || line.contains("WARNING:") {
                    diags.push(CodeDiagnostic {
                        level: "warning".into(),
                        file: path.to_string_lossy().into_owned(),
                        line: (i + 1) as u32,
                        message: line.trim().to_owned(),
                    });
                }
            }
            if diags.is_empty() {
                diags.push(CodeDiagnostic {
                    level: "error".into(),
                    file: path.to_string_lossy().into_owned(),
                    line: 1,
                    message: "glslc failed (see full output in terminal)".into(),
                });
            }
        }
        Err(e) => {
            diags.push(CodeDiagnostic {
                level: "error".into(),
                file: path.to_string_lossy().into_owned(),
                line: 1,
                message: format!("failed to run glslc: {e}"),
            });
        }
        _ => {}
    }
    diags
}

#[derive(Default)]
pub struct PluginHost {
    /// Intentionally leaked for the app's lifetime (see module docs).
    libs: Vec<libloading::Library>,
    /// Suffix for copied dylibs so every reload loads a fresh path.
    generation: u32,
}

/// (directory, package name) for each crate under `plugins/`.
pub fn discover(project_root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(project_root.join("plugins")) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let manifest = dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && let Some(name) = package_name(&text)
        {
            out.push((dir, name));
        }
    }
    out.sort();
    out
}

/// Minimal `name = "…"` extraction (avoids a toml dependency).
fn package_name(manifest: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("name")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim();
        Some(rest.trim_matches('"').to_owned())
    })
}

impl PluginHost {
    pub fn any_plugins(project_root: &Path) -> bool {
        !discover(project_root).is_empty()
    }

    /// Build every plugin crate and load + register it. Returns the loaded
    /// plugin names; the first failure aborts with the compiler output.
    #[cfg(feature = "editor")]
    pub fn build_and_load(
        &mut self,
        project_root: &Path,
        registry: &mut ComponentRegistry,
        editor: &mut EditorComponents,
    ) -> Result<Vec<String>> {
        let mut loaded = Vec::new();
        for (_dir, name) in discover(project_root) {
            tracing::info!("building component plugin {name}…");
            // `--features editor`: the editor needs each plugin's inspector +
            // gizmo (and its `citrus_register_editor` export). A shipped game
            // builds plugins without it, so citrus-editor/egui never link in.
            let output = Command::new("cargo")
                .current_dir(project_root)
                .args(["build", "-p", &name, "--features", "editor"])
                .output()
                .context("running cargo")?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !output.status.success() {
                bail!("building {name} failed:\n{}", stderr.trim());
            }
            // Surface rustc/clippy warnings from a successful build in the log.
            if stderr.contains("warning:") {
                tracing::warn!("plugin {name} built with warnings:\n{}", stderr.trim());
            }
            self.load(project_root, &name, registry, editor)
                .with_context(|| format!("loading plugin {name}"))?;
            loaded.push(name);
        }
        Ok(loaded)
    }

    #[cfg(feature = "editor")]
    fn load(
        &mut self,
        project_root: &Path,
        name: &str,
        registry: &mut ComponentRegistry,
        editor: &mut EditorComponents,
    ) -> Result<()> {
        let file = dylib_name(name);
        let built = project_root.join("target/debug").join(&file);
        // Copy to a unique path: the next `cargo build` overwrites the
        // original, and dlopen caches by path. The filename includes this
        // process's PID so two editor instances open on the same project don't
        // overwrite each other's mmapped copy (which SIGSEGVs the other editor).
        let dir = project_root.join("target/citrus-plugins");
        std::fs::create_dir_all(&dir)?;
        let copy = dir.join(format!(
            "{name}-{}-{}.{DYLIB_EXT}",
            std::process::id(),
            self.generation
        ));
        self.generation += 1;
        std::fs::copy(&built, &copy)
            .with_context(|| format!("copying {} for loading", built.display()))?;
        unsafe {
            // RTLD_NOW: resolve every symbol at load time. With the default
            // lazy binding a missing/incompatible symbol becomes a deferred
            // null-pointer call (a segfault from inside ld.so on first use,
            // hard to trace). RTLD_NOW turns that into a clean load error.
            // RTLD_LOCAL keeps the plugin's symbols out of the global scope.
            #[cfg(unix)]
            let lib: libloading::Library = {
                use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
                Library::open(Some(&copy), RTLD_NOW | RTLD_LOCAL)
                    .with_context(|| format!("loading {}", copy.display()))?
                    .into()
            };
            #[cfg(not(unix))]
            let lib = libloading::Library::new(&copy)
                .with_context(|| format!("loading {}", copy.display()))?;
            let register: libloading::Symbol<fn(&mut ComponentRegistry)> = lib
                .get(b"citrus_register")
                .context("plugin has no `citrus_register` export")?;
            register(registry);
            // Editor-only inspector/gizmo registration (optional; a runtime
            // game build of a plugin won't export it).
            if let Ok(register_editor) =
                lib.get::<fn(&mut EditorComponents)>(b"citrus_register_editor")
            {
                register_editor(editor);
            }
            self.libs.push(lib); // keep mapped forever
        }
        tracing::info!("loaded component plugin {name}");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
const DYLIB_EXT: &str = "so";
#[cfg(target_os = "macos")]
const DYLIB_EXT: &str = "dylib";
#[cfg(target_os = "windows")]
const DYLIB_EXT: &str = "dll";

fn dylib_name(package: &str) -> String {
    let stem = package.replace('-', "_");
    #[cfg(target_os = "windows")]
    return format!("{stem}.dll");
    #[cfg(not(target_os = "windows"))]
    format!("lib{stem}.{DYLIB_EXT}")
}

/// Starter plugin written by Tools → Create Component Plugin.
pub fn create_template(project_root: &Path) -> Result<PathBuf> {
    let dir = project_root.join("plugins/components");
    if dir.join("Cargo.toml").exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "citrus-project-components"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[features]
# The editor builds plugins with `--features editor` (inspector + gizmo + egui);
# a shipped game builds without it, so the editor crate and egui never link in.
default = []
editor = ["dep:citrus-editor", "dep:egui"]

[dependencies]
citrus-core.workspace = true
citrus-editor = { workspace = true, optional = true }
egui = { workspace = true, optional = true }
serde.workspace = true
glam.workspace = true
"#,
    )?;
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"//! Project components, hot-loaded by the citrus editor (Tools → Build &
//! Reload Components).
//!
//! A component's RUNTIME behaviour is `citrus_core::TypedComponent` (no UI) and
//! always compiles. Its EDITOR inspector (`citrus_editor::Inspect`) and gizmo
//! (`citrus_editor::Gizmo`) are gated behind the `editor` feature, so a shipped
//! game (built without that feature) links neither citrus-editor nor egui.

use citrus_core::{ComponentCtx, ComponentRegistry, TypedComponent};
#[cfg(feature = "editor")]
use citrus_editor::{EditorComponents, Gizmo, Inspect, InspectCtx};
use serde::{Deserialize, Serialize};

/// Register runtime behaviour (called by the editor and a shipped game).
#[unsafe(no_mangle)]
pub fn citrus_register(registry: &mut ComponentRegistry) {
    registry.register::<Orbit>();
    // citrus: new components register here
}

/// Register editor-only traits; compiled only with `--features editor`.
#[cfg(feature = "editor")]
#[unsafe(no_mangle)]
pub fn citrus_register_editor(editor: &mut EditorComponents) {
    editor.register::<Orbit>();
    // citrus: new components register their editor traits here
}

/// Example: circle around the object's authored position.
#[derive(Serialize, Deserialize)]
pub struct Orbit {
    pub degrees_per_second: f32,
    pub radius: f32,
    #[serde(skip)]
    applied: glam::Vec3,
}

impl Default for Orbit {
    fn default() -> Self {
        Self {
            degrees_per_second: 90.0,
            radius: 1.5,
            applied: glam::Vec3::ZERO,
        }
    }
}

impl TypedComponent for Orbit {
    const NAME: &'static str = "Orbit";

    fn update(&mut self, ctx: &mut ComponentCtx) {
        let angle = ctx.time * self.degrees_per_second.to_radians();
        let offset = glam::Vec3::new(angle.cos(), 0.0, angle.sin()) * self.radius;
        *ctx.translation += offset - self.applied;
        self.applied = offset;
    }
}

#[cfg(feature = "editor")]
impl Inspect for Orbit {
    fn inspector_ui(&mut self, ui: &mut egui::Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        changed |= ui
            .add(egui::Slider::new(&mut self.degrees_per_second, -360.0..=360.0).text("Speed (°/s)"))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut self.radius, 0.0..=10.0).text("Radius"))
            .changed();
        changed
    }
}

#[cfg(feature = "editor")]
impl Gizmo for Orbit {}
"#,
    )?;
    Ok(dir)
}

/// Run `cargo clippy` over every plugin crate on a background thread;
/// rustc errors and clippy lints arrive on the returned channel as
/// simplified diagnostics. (rust-analyzer LSP integration is the eventual
/// upgrade; clippy already includes all compiler errors.)
#[cfg(feature = "editor")]
pub fn run_check(project_root: &Path) -> mpsc::Receiver<Vec<CodeDiagnostic>> {
    let (tx, rx) = mpsc::channel();
    let root = project_root.to_owned();
    std::thread::spawn(move || {
        let mut diagnostics = Vec::new();
        for (_dir, name) in discover(&root) {
            let output = Command::new("cargo")
                .current_dir(&root)
                .args(["clippy", "-p", &name, "--message-format=json"])
                .output();
            match output {
                Ok(output) => parse_cargo_messages(&output.stdout, &mut diagnostics),
                Err(e) => diagnostics.push(CodeDiagnostic {
                    level: "error".into(),
                    file: String::new(),
                    line: 0,
                    message: format!("running cargo clippy: {e}"),
                }),
            }
        }
        let _ = tx.send(diagnostics);
    });
    rx
}

#[cfg(feature = "editor")]
fn parse_cargo_messages(stdout: &[u8], out: &mut Vec<CodeDiagnostic>) {
    for line in stdout.split(|&b| b == b'\n') {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if value["reason"] != "compiler-message" {
            continue;
        }
        let message = &value["message"];
        let level = message["level"].as_str().unwrap_or_default();
        if level != "error" && level != "warning" {
            continue;
        }
        let text = message["message"].as_str().unwrap_or_default();
        // Primary span, if any (some messages are crate-level).
        let (file, line_no) = message["spans"]
            .as_array()
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|s| s["is_primary"] == true)
                    .or_else(|| spans.first())
            })
            .map(|s| {
                (
                    s["file_name"].as_str().unwrap_or_default().to_owned(),
                    s["line_start"].as_u64().unwrap_or(0) as u32,
                )
            })
            .unwrap_or_default();
        out.push(CodeDiagnostic {
            level: level.to_owned(),
            file,
            line: line_no,
            message: text.to_owned(),
        });
    }
}

const REGISTER_MARKER: &str = "// citrus: new components register here";
const EDITOR_REGISTER_MARKER: &str =
    "// citrus: new components register their editor traits here";

/// Files → Create → New Component: a fresh component module in the plugin
/// crate (created on demand), prefilled with every engine-called hook, and
/// registered in `citrus_register`. Returns the new `.rs` path.
pub fn create_component(project_root: &Path) -> Result<PathBuf> {
    let dir = create_template(project_root)?;
    let src = dir.join("src");

    let (mod_name, file) = (0..1000)
        .map(|n| {
            let name = if n == 0 {
                "new_component".to_owned()
            } else {
                format!("new_component_{n}")
            };
            let file = src.join(format!("{name}.rs"));
            (name, file)
        })
        .find(|(_, file)| !file.exists())
        .context("no free component file name")?;
    let struct_name: String = mod_name
        .split('_')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect();

    std::fs::write(&file, component_template(&struct_name))?;

    // Wire it into lib.rs: module declaration + registration.
    let lib = src.join("lib.rs");
    let mut text = std::fs::read_to_string(&lib).context("reading plugin lib.rs")?;
    text.push_str(&format!("\nmod {mod_name};\n"));
    let registration = format!("    registry.register::<{mod_name}::{struct_name}>();\n");
    if let Some(marker) = text.find(REGISTER_MARKER) {
        let line_start = text[..marker].rfind('\n').map_or(0, |i| i + 1);
        text.insert_str(line_start, &registration);
    } else if let Some(at) = text.find("fn citrus_register") {
        if let Some(brace) = text[at..].find('{') {
            text.insert_str(at + brace + 1, &format!("\n{registration}"));
        }
    } else {
        bail!("plugin lib.rs has no citrus_register function to extend");
    }
    // Editor-trait registration (best-effort; gated behind the `editor` feature
    // in the template).
    let editor_reg = format!("    editor.register::<{mod_name}::{struct_name}>();\n");
    if let Some(marker) = text.find(EDITOR_REGISTER_MARKER) {
        let line_start = text[..marker].rfind('\n').map_or(0, |i| i + 1);
        text.insert_str(line_start, &editor_reg);
    }
    std::fs::write(&lib, text)?;
    Ok(file)
}

/// Starter component: runtime behaviour always compiles; the editor inspector
/// + gizmo are gated behind the `editor` feature.
fn component_template(name: &str) -> String {
    format!(
        r#"//! {name} — describe what it does.
//!
//! Rename the struct + NAME, then Tools → Build & Reload Components.

use citrus_core::{{ComponentCtx, TypedComponent}};
#[cfg(feature = "editor")]
use citrus_editor::{{Gizmo, Inspect, InspectCtx}};
use serde::{{Deserialize, Serialize}};

#[derive(Serialize, Deserialize)]
pub struct {name} {{
    pub speed: f32,
}}

impl Default for {name} {{
    fn default() -> Self {{
        Self {{ speed: 1.0 }}
    }}
}}

impl TypedComponent for {name} {{
    /// Shown in the Add Component menu and saved into .scene files.
    const NAME: &'static str = "{name}";

    /// Called once when ▶ Play starts.
    fn start(&mut self, _ctx: &mut ComponentCtx) {{}}

    /// Called every frame while playing. `ctx` carries dt, time, the object's
    /// local TRS, and the in-game API (object references, scene load, …).
    fn update(&mut self, ctx: &mut ComponentCtx) {{
        // Example: spin around Y at `speed` rad/s.
        let step = glam::Quat::from_rotation_y(self.speed * ctx.dt);
        *ctx.rotation = (step * *ctx.rotation).normalize();
    }}

    /// Called every frame after all components ran `update`.
    fn late_update(&mut self, _ctx: &mut ComponentCtx) {{}}
}}

/// Editor inspector (editor build only).
#[cfg(feature = "editor")]
impl Inspect for {name} {{
    fn inspector_ui(&mut self, ui: &mut egui::Ui, _ctx: &InspectCtx) -> bool {{
        let mut changed = false;
        changed |= ui
            .add(egui::Slider::new(&mut self.speed, 0.0..=10.0).text("Speed"))
            .changed();
        changed
    }}
}}

/// Editor viewport gizmo (editor build only). Override `draw_gizmo` to draw.
#[cfg(feature = "editor")]
impl Gizmo for {name} {{}}
"#
    )
}
