//! App icon (the zestengine slice from `zestengine_editor::icons`) plus desktop integration.
//!
//! X11/Windows take the icon directly from winit. Wayland compositors look
//! it up via the window's `app_id`, which points to a `.desktop` file, so we
//! install `zestengine.desktop` and the icon into the user's XDG data dirs on
//! first run, refreshing older icons when the logo changes.

use std::path::PathBuf;

/// The app icon as (width, height, RGBA8); shared with the Project Hub.
pub fn rgba() -> (u32, u32, Vec<u8>) {
    zestengine_editor::icons::app_icon_rgba()
}

/// Install `zestengine.desktop` into XDG data dirs if missing and refresh the icon, so
/// Wayland compositors (which resolve icons by `app_id`) show ours.
/// Best-effort: failures only log.
pub fn install_desktop_entry() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));

    if let Err(e) = zestengine_editor::icons::install_app_icon() {
        tracing::debug!("installing app icon: {e}");
    }

    let desktop_path = data.join("applications/zestengine.desktop");
    if !desktop_path.exists() {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "zestengine".into());
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=ZestEngine Editor\n\
             Comment=zestengine engine editor\n\
             Exec={exe}\n\
             Icon=zestengine\n\
             Terminal=false\n\
             Categories=Development;Graphics;3DGraphics;\n\
             StartupWMClass=zestengine\n"
        );
        let write = || -> anyhow::Result<()> {
            if let Some(parent) = desktop_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&desktop_path, entry)?;
            Ok(())
        };
        match write() {
            Ok(()) => tracing::info!("installed {}", desktop_path.display()),
            Err(e) => tracing::debug!("installing desktop entry: {e:#}"),
        }
    }
}
